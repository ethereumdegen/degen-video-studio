//! Sequence ops: create, retarget, duplicate, nest, remove.
//!
//! A sequence owns the frame grid and the raster size, which makes `seq.set` the interesting
//! op in this file: changing `fps` invalidates the one thing every other op assumes, that
//! positions are exact multiples of a frame. Leaving material between frames would turn
//! every later trim into a surprise, so the op moves the whole sequence onto the new grid,
//! preserves the non-overlap invariant while doing it, and reports every position it moved.
//!
//! `seq.nest` is the other structural op: it replaces a time range with a single clip backed
//! by a new sequence holding what was there. That is how an agent gets a reusable unit out of
//! a stretch of timeline — and why the clips it moves are trimmed rather than dropped.

use crate::error::{Error, Result};
use crate::ids::{ClipId, CueId, EffectId, MarkerId, SequenceId, TrackId};
use crate::op::{args, Op, OpCx, OpEffect, Registry};
use crate::ops::util::{assert_free, assert_takes_clips, assert_unlocked, sequence_fps, snap_span};
use crate::project::{Clip, Keyframe, Project, Sequence, Source, Track, TrackKind};
use crate::selector;
use crate::time::{Fps, Span, Time};
use std::collections::{BTreeMap, HashMap};

pub fn register(registry: &mut Registry) {
    registry
        .register(SeqNew)
        .register(SeqSet)
        .register(SeqDuplicate)
        .register(SeqActivate)
        .register(SeqNest)
        .register(SeqRemove);
}

/// The sequence an op acts on: an explicit `sequence` argument, else `--seq`, else the
/// project's active sequence. Only the `seq.*` ops need to name a sequence that is not the
/// current target, so the argument lives here rather than in the shared context.
fn target_sequence(
    project: &Project,
    args: &serde_json::Value,
    cx: &OpCx,
) -> Result<SequenceId> {
    match args::opt_str(args, "sequence") {
        Some(query) => project.resolve_sequence(Some(query)),
        None => cx.sequence(project),
    }
}

fn name_taken(project: &Project, name: &str) -> bool {
    project.sequences.values().any(|sequence| sequence.name == name)
}

/// A free name derived from `base`. Used where the caller did not supply one; an explicit
/// name that collides is an error instead, because silently editing a name an agent chose
/// makes every later `--seq <name>` miss.
fn unique_name(project: &Project, base: &str) -> String {
    if !name_taken(project, base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base} {n}"))
        .find(|candidate| !name_taken(project, candidate))
        .expect("an unbounded search terminates")
}

/// `"1920x1080"` or `[1920, 1080]`. Agents write both spellings, and a malformed size is
/// worth an error rather than a silent fallback to a default resolution.
fn opt_size(args: &serde_json::Value, key: &str) -> Result<Option<[u32; 2]>> {
    let checked = |size: [u32; 2]| -> Result<Option<[u32; 2]>> {
        if size[0] == 0 || size[1] == 0 {
            return Err(Error::bad_args(format!(
                "field '{key}' has a zero dimension: {}x{}",
                size[0], size[1]
            )));
        }
        Ok(Some(size))
    };
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(text)) => {
            let (width, height) = text
                .split_once(|c: char| c == 'x' || c == 'X' || c == '*')
                .ok_or_else(|| {
                    Error::bad_args(format!(
                        "field '{key}' must look like '1920x1080', got '{text}'"
                    ))
                })?;
            let dimension = |part: &str| -> Result<u32> {
                part.trim().parse::<u32>().map_err(|_| {
                    Error::bad_args(format!("field '{key}' has a non-numeric side in '{text}'"))
                })
            };
            checked([dimension(width)?, dimension(height)?])
        }
        Some(serde_json::Value::Array(items)) if items.len() == 2 => {
            let mut size = [0u32; 2];
            for (slot, item) in size.iter_mut().zip(items) {
                let value = item.as_u64().ok_or_else(|| {
                    Error::bad_args(format!("field '{key}' must be two positive integers"))
                })?;
                *slot = u32::try_from(value).map_err(|_| {
                    Error::bad_args(format!("field '{key}' side {value} is out of range"))
                })?;
            }
            checked(size)
        }
        Some(other) => Err(Error::bad_args(format!(
            "field '{key}' must be '1920x1080' or [1920, 1080], got {other}"
        ))),
    }
}

fn opt_fps(args: &serde_json::Value, key: &str) -> Result<Option<Fps>> {
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(text)) => Fps::parse(text)
            .map(Some)
            .map_err(|e| Error::bad_args(format!("field '{key}': {e}"))),
        Some(serde_json::Value::Number(number)) => Fps::parse(&number.to_string())
            .map(Some)
            .map_err(|e| Error::bad_args(format!("field '{key}': {e}"))),
        Some(other) => Err(Error::bad_args(format!(
            "field '{key}' must be a frame rate, got {other}"
        ))),
    }
}

/// Sample rates are whole numbers in a range real hardware accepts; a fractional or absurd
/// rate is a typo that would otherwise only surface as a broken mix.
fn opt_sample_rate(args: &serde_json::Value, key: &str) -> Result<Option<u32>> {
    let Some(value) = args::opt_f64(args, key)? else {
        return Ok(None);
    };
    if !value.is_finite() || value.fract() != 0.0 || !(8000.0..=384_000.0).contains(&value) {
        return Err(Error::bad_args(format!(
            "field '{key}' must be a whole sample rate between 8000 and 384000, got {value}"
        )));
    }
    Ok(Some(value as u32))
}

pub struct SeqNew;

impl Op for SeqNew {
    fn id(&self) -> &'static str {
        "seq.new"
    }

    fn about(&self) -> &'static str {
        "Add a sequence, inheriting format from the active one unless overridden"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Sequence name, unique in the project because names address sequences"
                },
                "fps": {
                    "type": "string",
                    "description": "Frame rate; defaults to the active sequence's",
                    "examples": ["30", "30000/1001", "29.97"]
                },
                "size": {
                    "description": "Frame size; defaults to the active sequence's",
                    "examples": ["1920x1080", [1080, 1920]]
                },
                "sample-rate": {
                    "type": "integer",
                    "description": "Audio sample rate in Hz; defaults to the active sequence's"
                },
                "activate": {
                    "type": "boolean",
                    "description": "Make this the sequence later ops target by default"
                }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        _cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["name", "fps", "size", "sample-rate", "activate"])?;
        let name = args::str_field(&args, "name")?.trim().to_string();
        if name.is_empty() {
            return Err(Error::bad_args("sequence name is empty"));
        }
        if name_taken(project, &name) {
            return Err(Error::bad_args(format!(
                "sequence name '{name}' is already used; names address sequences, so they stay unique"
            )));
        }
        // A second sequence in a project is nearly always the same format as the first, and
        // a mismatched grid only shows up as a resample at render time. Inheriting beats a
        // hardcoded 1080p30 that happens to be wrong.
        let template = project.sequences.get(&project.active_sequence);
        let fps = opt_fps(&args, "fps")?
            .or_else(|| template.map(|sequence| sequence.fps))
            .unwrap_or_default();
        let size = opt_size(&args, "size")?
            .or_else(|| template.map(|sequence| sequence.size))
            .unwrap_or([1920, 1080]);
        let sample_rate = opt_sample_rate(&args, "sample-rate")?
            .or_else(|| template.map(|sequence| sequence.sample_rate))
            .unwrap_or(48_000);
        let channels = template.map(|sequence| sequence.channels).unwrap_or(2);
        let activate = args::opt_bool(&args, "activate")?.unwrap_or(false);

        let mut sequence = Sequence::new(name, fps, size, sample_rate);
        sequence.channels = channels;
        let id = sequence.id.clone();
        project.sequences.insert(id.clone(), sequence);
        if activate {
            project.active_sequence = id.clone();
        }
        Ok(OpEffect::new().created(&id))
    }
}

pub struct SeqSet;

impl Op for SeqSet {
    fn id(&self) -> &'static str {
        "seq.set"
    }

    fn about(&self) -> &'static str {
        "Change a sequence's frame rate, size or sample rate, re-snapping its contents"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "sequence": { "type": "string", "description": "Sequence id or name; defaults to the active one" },
                "fps": {
                    "type": "string",
                    "description": "New frame rate; every clip start, duration, marker and cue is moved onto the new grid and the moves are reported",
                    "examples": ["24", "30000/1001"]
                },
                "size": { "description": "New frame size", "examples": ["1920x1080"] },
                "sample-rate": { "type": "integer", "description": "New audio sample rate in Hz" }
            },
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["sequence", "fps", "size", "sample-rate"])?;
        let id = target_sequence(project, &args, cx)?;
        let fps = opt_fps(&args, "fps")?;
        let size = opt_size(&args, "size")?;
        let sample_rate = opt_sample_rate(&args, "sample-rate")?;
        if fps.is_none() && size.is_none() && sample_rate.is_none() {
            return Err(Error::bad_args(
                "seq.set needs at least one of fps, size, sample-rate",
            ));
        }

        let sequence = project.sequence_mut(&id)?;
        let mut effect = OpEffect::new().changed(&id);
        if let Some(size) = size {
            sequence.size = size;
        }
        if let Some(sample_rate) = sample_rate {
            sequence.sample_rate = sample_rate;
        }
        if let Some(fps) = fps {
            if fps != sequence.fps {
                sequence.fps = fps;
                effect = regrid(sequence, fps, effect);
            }
        }
        Ok(effect)
    }
}

/// Move a whole sequence onto `fps`.
///
/// Snapping alone can produce an overlap — two starts one frame apart on the old grid can
/// round onto the same frame on the new one — and an overlap is not a representable state.
/// So clips are walked in order behind a cursor: a start that would land inside its
/// predecessor is pushed to the predecessor's end instead. `applied` in the reported snap is
/// where the clip actually went, push included.
fn regrid(sequence: &mut Sequence, fps: Fps, mut effect: OpEffect) -> OpEffect {
    let frame = fps.frame_duration();
    for track in &mut sequence.tracks {
        let mut cursor = Time::ZERO;
        let mut previous_duration: Option<Time> = None;
        for clip in &mut track.clips {
            let (was_start, was_duration) = (clip.start, clip.duration);
            let start = was_start.snap(fps).max(cursor);
            // A clip shorter than one frame of the new grid has nothing to show, and a
            // zero duration fails validation, so one frame is the floor.
            let duration = was_duration.snap(fps).max(frame);
            clip.start = start;
            clip.duration = duration;
            cursor = start + duration;

            let limit = previous_duration.map_or(duration, |previous| previous.min(duration));
            let mut clamped = None;
            if let Some(transition) = clip.transition_in.as_mut() {
                let fitted = transition.duration.snap(fps).max(frame).min(limit);
                if fitted != transition.duration {
                    clamped = Some(fitted);
                }
                transition.duration = fitted;
            }
            if let Some(fitted) = clamped {
                effect = effect.warn(
                    "transition-clamped",
                    &clip.id,
                    format!("transition shortened to {fitted} to fit its neighbours on the new grid"),
                );
            }
            effect = effect
                .snap("clip.start", was_start, start, fps)
                .snap("clip.duration", was_duration, duration, fps);
            if start != was_start || duration != was_duration || clamped.is_some() {
                effect = effect.changed(&clip.id);
            }
            previous_duration = Some(duration);
        }
        // Cues are drawn on frames like everything else; a cue boundary between frames would
        // render one frame early or late depending on the rounding of whoever asks.
        for cue in &mut track.cues {
            let was = cue.span;
            let mut snapped = snap_span(was, fps);
            if snapped.is_empty() {
                snapped.end = snapped.start + frame;
            }
            cue.span = snapped;
            effect = effect
                .snap("cue.start", was.start, snapped.start, fps)
                .snap("cue.end", was.end, snapped.end, fps);
            if snapped != was {
                effect = effect.changed(&cue.id);
            }
        }
    }
    for marker in &mut sequence.markers {
        let was = marker.at;
        marker.at = was.snap(fps);
        effect = effect.snap("marker.at", was, marker.at, fps);
        if marker.at != was {
            effect = effect.changed(&marker.id);
        }
    }
    effect
}

pub struct SeqDuplicate;

impl Op for SeqDuplicate {
    fn id(&self) -> &'static str {
        "seq.duplicate"
    }

    fn about(&self) -> &'static str {
        "Copy a sequence, giving everything inside it fresh ids"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "sequence": { "type": "string", "description": "Sequence to copy; defaults to the active one" },
                "name": { "type": "string", "description": "Name for the copy; defaults to '<name> copy'" },
                "activate": { "type": "boolean", "description": "Make the copy the sequence later ops target" }
            },
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["sequence", "name", "activate"])?;
        let source = target_sequence(project, &args, cx)?;
        let activate = args::opt_bool(&args, "activate")?.unwrap_or(false);
        let name = match args::opt_str(&args, "name").map(str::trim) {
            Some(explicit) if !explicit.is_empty() => {
                if name_taken(project, explicit) {
                    return Err(Error::bad_args(format!(
                        "sequence name '{explicit}' is already used"
                    )));
                }
                explicit.to_string()
            }
            _ => {
                let base = format!("{} copy", project.sequence(&source)?.name);
                unique_name(project, &base)
            }
        };

        let copy = deep_copy(project.sequence(&source)?, name);
        let id = copy.id.clone();
        project.sequences.insert(id.clone(), copy);
        if activate {
            project.active_sequence = id.clone();
        }
        Ok(OpEffect::new().created(&id))
    }
}

/// Copy a sequence and re-id everything in it.
///
/// Ids that appear inside *references* are remapped through the same tables: a copy whose
/// ducking still pointed at the original's track, or whose a/v link still pointed at the
/// original's clip, would drift the moment either copy was edited. Keyframe map keys embed
/// effect ids (`fx.<id>.<param>`), so the rename has to reach into the keys as well.
fn deep_copy(source: &Sequence, name: String) -> Sequence {
    let mut copy = source.clone();
    copy.id = SequenceId::new();
    copy.name = name;

    let mut tracks: HashMap<TrackId, TrackId> = HashMap::new();
    let mut clips: HashMap<ClipId, ClipId> = HashMap::new();
    for track in &mut copy.tracks {
        let fresh = TrackId::new();
        tracks.insert(track.id.clone(), fresh.clone());
        track.id = fresh;
        for clip in &mut track.clips {
            let fresh = ClipId::new();
            clips.insert(clip.id.clone(), fresh.clone());
            clip.id = fresh;
        }
        for cue in &mut track.cues {
            cue.id = CueId::new();
        }
    }
    for marker in &mut copy.markers {
        marker.id = MarkerId::new();
    }

    for track in &mut copy.tracks {
        for clip in &mut track.clips {
            if let Some(link) = clip.link.as_mut() {
                if let Some(fresh) = clips.get(link) {
                    *link = fresh.clone();
                }
            }
            if let Some(ducking) = clip.ducking.as_mut() {
                if let Some(fresh) = tracks.get(&ducking.against) {
                    ducking.against = fresh.clone();
                }
            }
            let mut effects: HashMap<String, String> = HashMap::new();
            for effect in &mut clip.effects {
                let fresh = EffectId::new();
                effects.insert(effect.id.to_string(), fresh.to_string());
                effect.id = fresh;
            }
            if !effects.is_empty() && !clip.keyframes.is_empty() {
                let remapped: BTreeMap<String, Vec<Keyframe>> = clip
                    .keyframes
                    .iter()
                    .map(|(path, keys)| {
                        let renamed = path
                            .strip_prefix("fx.")
                            .and_then(|rest| {
                                let (id, param) = rest.split_once('.')?;
                                Some(format!("fx.{}.{param}", effects.get(id)?))
                            })
                            .unwrap_or_else(|| path.clone());
                        (renamed, keys.clone())
                    })
                    .collect();
                clip.keyframes = remapped;
            }
        }
    }
    copy
}

pub struct SeqActivate;

impl Op for SeqActivate {
    fn id(&self) -> &'static str {
        "seq.activate"
    }

    fn about(&self) -> &'static str {
        "Choose the sequence subsequent ops target by default"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "sequence": { "type": "string", "description": "Sequence id or name" }
            },
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["sequence"])?;
        let id = target_sequence(project, &args, cx)?;
        let name = project.sequence(&id)?.name.clone();
        project.active_sequence = id.clone();
        Ok(OpEffect::new()
            .changed(&id)
            .data(serde_json::json!({ "activeSequence": id.to_string(), "name": name })))
    }
}

pub struct SeqNest;

impl Op for SeqNest {
    fn id(&self) -> &'static str {
        "seq.nest"
    }

    fn about(&self) -> &'static str {
        "Replace a time range with one clip backed by a new sequence holding what was there"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "sequence": { "type": "string", "description": "Sequence to nest inside; defaults to the active one" },
                "start": { "type": "string", "description": "Range start", "examples": ["00:00:10.000", "300f"] },
                "end": { "type": "string", "description": "Range end; alternative to duration" },
                "duration": { "type": "string", "description": "Range length; alternative to end" },
                "tracks": {
                    "type": "string",
                    "description": "Track selector; defaults to every non-caption track with material in the range",
                    "examples": ["V1", "track[kind=video]"]
                },
                "track": {
                    "type": "string",
                    "description": "Track the nest clip lands on; defaults to the first affected video track"
                },
                "name": { "type": "string", "description": "Name for the new sequence" }
            },
            "required": ["start"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(
            &args,
            &[
                "sequence", "start", "end", "duration", "tracks", "track", "name",
            ],
        )?;
        let seq_id = target_sequence(project, &args, cx)?;
        let fps = sequence_fps(project, &seq_id)?;
        let requested = args::opt_span(&args, fps)?.ok_or_else(|| {
            Error::bad_args("seq.nest needs 'start' with either 'end' or 'duration'")
        })?;
        let span = snap_span(requested, fps);
        if span.is_empty() {
            return Err(Error::bad_args(format!(
                "nest range {span} is empty at {fps} fps"
            )));
        }

        let selected: Vec<TrackId> = match args::opt_str(&args, "tracks") {
            Some(text) => selector::resolve_tracks(project, &seq_id, text)?,
            None => project
                .sequence(&seq_id)?
                .tracks
                .iter()
                .filter(|track| track.kind != TrackKind::Caption)
                .filter(|track| track.clips.iter().any(|clip| clip.span().overlaps(&span)))
                .map(|track| track.id.clone())
                .collect(),
        };
        if selected.is_empty() {
            return Err(Error::op(format!(
                "no track has material in {span} to nest"
            )));
        }

        let sequence = project.sequence(&seq_id)?;
        for id in &selected {
            let track = sequence.track(id)?;
            assert_unlocked(track)?;
            assert_takes_clips(track)?;
        }
        // Nest in the sequence's own track order, whatever order the selector reported, so
        // the copy composites the way the original did.
        let order: Vec<TrackId> = sequence
            .tracks
            .iter()
            .filter(|track| selected.contains(&track.id))
            .map(|track| track.id.clone())
            .collect();

        let host = match args::opt_str(&args, "track") {
            Some(text) => selector::resolve_track(project, &seq_id, text)?,
            None => order
                .iter()
                .find(|id| {
                    sequence
                        .track(id)
                        .is_ok_and(|track| track.kind == TrackKind::Video)
                })
                .unwrap_or(&order[0])
                .clone(),
        };
        let host_track = sequence.track(&host)?;
        assert_unlocked(host_track)?;
        assert_takes_clips(host_track)?;
        // A host that is not one of the nested tracks keeps its material, so the range has
        // to be free there already.
        if !order.contains(&host) {
            assert_free(host_track, span, None)?;
        }

        let name = match args::opt_str(&args, "name").map(str::trim) {
            Some(explicit) if !explicit.is_empty() => {
                if name_taken(project, explicit) {
                    return Err(Error::bad_args(format!(
                        "sequence name '{explicit}' is already used"
                    )));
                }
                explicit.to_string()
            }
            _ => unique_name(project, &format!("{} nest", sequence.name)),
        };
        let mut nested = Sequence::new(name, sequence.fps, sequence.size, sequence.sample_rate);
        nested.channels = sequence.channels;
        nested.background = sequence.background;
        let nested_id = nested.id.clone();
        let nested_name = nested.name.clone();

        let mut effect = OpEffect::new()
            .snap("start", requested.start, span.start, fps)
            .snap("end", requested.end, span.end, fps);

        let sequence = project.sequence_mut(&seq_id)?;
        for track_id in &order {
            let track = sequence.track_mut(track_id)?;
            let mut keep: Vec<Clip> = Vec::new();
            let mut inner: Vec<Clip> = Vec::new();
            for clip in std::mem::take(&mut track.clips) {
                let Some(overlap) = clip.span().intersect(&span) else {
                    keep.push(clip);
                    continue;
                };
                let original = clip.id.clone();
                let head_stays = clip.start < overlap.start;
                if head_stays {
                    let mut head = clip.clone();
                    trim_to(&mut head, Span::new(clip.start, overlap.start));
                    keep.push(head);
                }
                if clip.end() > overlap.end {
                    let mut tail = clip.clone();
                    tail.id = ClipId::new();
                    trim_to(&mut tail, Span::new(overlap.end, clip.end()));
                    effect = effect.created(&tail.id);
                    keep.push(tail);
                }

                // The moved part is a new clip in a new sequence; the original id survives
                // only if a head stayed behind on this timeline.
                let mut moved = clip;
                trim_to(&mut moved, overlap);
                moved.id = ClipId::new();
                effect = if head_stays {
                    effect.changed(&original)
                } else {
                    effect.removed(&original)
                };
                moved.start = moved.start - span.start;
                if moved.start.is_zero() && moved.transition_in.is_some() {
                    moved.transition_in = None;
                    effect = effect.warn(
                        "transition-dropped",
                        &original,
                        "the clip it blended from stayed outside the nest",
                    );
                }
                inner.push(moved);
            }
            track.clips = keep;
            if inner.is_empty() {
                continue;
            }
            // Mix state travels with the material: a muted or attenuated track whose clips
            // moved into an unmuted nest would suddenly be audible.
            let mut copy = Track::new(track.name.clone(), track.kind);
            copy.muted = track.muted;
            copy.hidden = track.hidden;
            copy.gain_db = track.gain_db;
            copy.pan = track.pan;
            copy.style = track.style.clone();
            copy.clips = inner;
            nested.tracks.push(copy);
        }

        let host_track = sequence.track_mut(&host)?;
        assert_free(host_track, span, None)?;
        let mut clip = Clip::new(
            Source::Sequence {
                sequence: nested_id.clone(),
            },
            span.start,
            span.duration(),
        );
        clip.name = Some(nested_name);
        let clip_id = clip.id.clone();
        host_track.place(clip);
        effect = effect.created(&clip_id).created(&nested_id).changed(&host);

        project.sequences.insert(nested_id, nested);
        Ok(effect)
    }
}

/// Reduce a clip to a sub-range of its current span, keeping the same source material under
/// the frames that survive.
///
/// Which end moves decides whether `source_in` moves: trimming the head of a forward clip
/// advances into the source, while for a reversed clip — which reads its span backwards — it
/// is the *tail* trim that advances the in-point. Keyframe times are clip-local, so a head
/// trim slides the whole curve; keys that fall outside the retained range are kept, because
/// they still shape the interpolation that crosses into it.
fn trim_to(clip: &mut Clip, span: Span) {
    let head = span.start - clip.start;
    let tail = clip.end() - span.end;
    clip.source_in = if clip.reverse {
        clip.source_in + tail * clip.speed
    } else {
        clip.source_in + head * clip.speed
    };
    clip.start = span.start;
    clip.duration = span.duration();
    clip.fade_in = clip.fade_in.min(clip.duration);
    clip.fade_out = clip.fade_out.min(clip.duration);
    if head.is_positive() {
        for keys in clip.keyframes.values_mut() {
            for key in keys.iter_mut() {
                key.at = key.at - head;
            }
        }
        // A transition belongs to the cut it was authored on, and that cut just moved.
        clip.transition_in = None;
    }
}

pub struct SeqRemove;

impl Op for SeqRemove {
    fn id(&self) -> &'static str {
        "seq.remove"
    }

    fn about(&self) -> &'static str {
        "Remove a sequence; refuses while a nested clip still plays it"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "sequence": { "type": "string", "description": "Sequence id or name; defaults to the active one" }
            },
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["sequence"])?;
        let id = target_sequence(project, &args, cx)?;
        if project.sequences.len() == 1 {
            return Err(Error::op(
                "a project must keep at least one sequence; create another before removing this one",
            ));
        }
        let name = project.sequence(&id)?.name.clone();

        // A nested clip is the one reference to a sequence that cannot be recreated from the
        // rest of the document, so removal names the referrers instead of orphaning them.
        let mut referrers: Vec<String> = Vec::new();
        for (other_id, other) in &project.sequences {
            if *other_id == id {
                continue;
            }
            for track in &other.tracks {
                for clip in &track.clips {
                    if matches!(&clip.source, Source::Sequence { sequence } if *sequence == id) {
                        referrers.push(format!("{}/{}/{}", other.name, track.name, clip.label()));
                    }
                }
            }
        }
        if !referrers.is_empty() {
            return Err(Error::op(format!(
                "sequence '{name}' is still played by nested clip(s) {}; remove those clips first",
                referrers.join(", ")
            )));
        }

        project
            .sequences
            .shift_remove(&id)
            .expect("the sequence resolved above");
        if project.active_sequence == id {
            project.active_sequence = project
                .sequences
                .keys()
                .next()
                .cloned()
                .expect("at least one sequence remains");
        }
        Ok(OpEffect::new().removed(&id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::AssetStore;
    use crate::color::Rgba;
    use crate::paths::ProjectPaths;
    use crate::project::Marker;
    use crate::vfs::MemVfs;
    use std::sync::Arc;

    /// Ops need only a project and a context, so a test needs no workspace on disk.
    fn apply(
        op: &dyn Op,
        project: &mut Project,
        args: serde_json::Value,
    ) -> Result<OpEffect> {
        let paths = ProjectPaths::new("/p");
        let assets = AssetStore::new("/p/assets".into(), Arc::new(MemVfs::default()));
        let mut cx = OpCx::new(&paths, &assets);
        op.apply(project, args, &mut cx)
    }

    fn fps30() -> Fps {
        Fps::new(30, 1).expect("30 fps")
    }

    fn clip(id: &str, start: i64, duration: i64) -> Clip {
        let mut clip = Clip::new(
            Source::Color {
                color: Rgba::BLACK,
            },
            Time::from_secs(start),
            Time::from_secs(duration),
        );
        clip.id = ClipId::from_raw(id);
        clip.name = Some(id.trim_start_matches("clp_").to_string());
        clip
    }

    fn project_with_clips() -> Project {
        let mut project = Project::new("promo", fps30(), [1920, 1080], 48_000);
        let mut track = Track::new("V1", TrackKind::Video);
        track.id = TrackId::from_raw("trk_v1");
        track.clips = vec![clip("clp_a", 0, 1), clip("clp_b", 1, 2), clip("clp_c", 3, 1)];
        let seq = project.active_sequence.clone();
        let sequence = project.sequence_mut(&seq).expect("main");
        sequence.tracks.push(track);
        project
    }

    #[test]
    fn changing_the_frame_rate_moves_everything_onto_the_new_grid() {
        let mut project = project_with_clips();
        let seq = project.active_sequence.clone();
        project
            .sequence_mut(&seq)
            .expect("main")
            .markers
            .push(Marker {
                id: MarkerId::from_raw("mk_1"),
                at: Time::from_secs(2),
                name: "pricing".into(),
                color: None,
                note: None,
            });

        let effect = apply(&SeqSet, &mut project, serde_json::json!({ "fps": "30000/1001" }))
            .expect("re-rate");

        let ndf = Fps::new(30000, 1001).expect("29.97");
        let sequence = project.sequence(&seq).expect("main");
        assert_eq!(sequence.fps, ndf);
        for track in &sequence.tracks {
            for clip in &track.clips {
                assert!(
                    clip.start.is_frame_aligned(ndf) && clip.duration.is_frame_aligned(ndf),
                    "clip {} is off the new grid: {} + {}",
                    clip.label(),
                    clip.start,
                    clip.duration
                );
            }
        }
        // 1 s is frame 30 on the new grid, which is 1001/1000 s — not 1 s.
        assert_eq!(sequence.tracks[0].clips[1].start, Time::new(1001, 1000).unwrap());
        assert_eq!(sequence.tracks[0].clips[2].start, Time::new(3003, 1000).unwrap());
        assert!(sequence.validate().is_ok(), "re-rating left an overlap");
        assert!(sequence.markers[0].at.is_frame_aligned(ndf));

        let fields: Vec<&str> = effect.snapped.iter().map(|snap| snap.field).collect();
        assert!(
            fields.contains(&"clip.start") && fields.contains(&"clip.duration"),
            "re-rate reported no clip snaps: {fields:?}"
        );
        assert!(
            effect.changed.iter().any(|id| id == "clp_b"),
            "moved clips should be reported as changed: {:?}",
            effect.changed
        );
    }

    #[test]
    fn re_rating_keeps_clips_apart_when_two_starts_round_together() {
        // Two adjacent one-frame clips at 30 fps cannot both survive a move to 5 fps as
        // distinct frames; the second is pushed rather than allowed to overlap.
        let mut project = Project::new("promo", fps30(), [1920, 1080], 48_000);
        let seq = project.active_sequence.clone();
        let mut track = Track::new("V1", TrackKind::Video);
        let mut first = clip("clp_a", 0, 1);
        first.duration = Time::from_frames(1, fps30());
        let mut second = clip("clp_b", 0, 1);
        second.start = Time::from_frames(1, fps30());
        second.duration = Time::from_frames(1, fps30());
        track.clips = vec![first, second];
        project.sequence_mut(&seq).expect("main").tracks.push(track);

        apply(&SeqSet, &mut project, serde_json::json!({ "fps": "5" })).expect("re-rate");

        let sequence = project.sequence(&seq).expect("main");
        let clips = &sequence.tracks[0].clips;
        assert_eq!(clips[0].start, Time::ZERO);
        assert_eq!(clips[1].start, clips[0].end());
        assert!(sequence.validate().is_ok(), "{:?}", sequence.validate());
    }

    #[test]
    fn removing_a_sequence_a_nested_clip_plays_names_the_clip() {
        let mut project = project_with_clips();
        let nested = Sequence::new("insert", fps30(), [1920, 1080], 48_000);
        let nested_id = nested.id.clone();
        project.sequences.insert(nested_id.clone(), nested);
        let mut host = clip("clp_nest", 10, 2);
        host.name = Some("the-nest".into());
        host.source = Source::Sequence {
            sequence: nested_id.clone(),
        };
        let seq = project.active_sequence.clone();
        project.sequence_mut(&seq).expect("main").tracks[0]
            .clips
            .push(host);

        let err = apply(
            &SeqRemove,
            &mut project,
            serde_json::json!({ "sequence": nested_id.to_string() }),
        )
        .expect_err("a referenced sequence must not be removable");
        assert!(err.to_string().contains("the-nest"), "{err}");
        assert!(project.sequences.contains_key(&nested_id));

        // Removing the referring clip clears the way.
        project.sequence_mut(&seq).expect("main").tracks[0].clips.pop();
        apply(
            &SeqRemove,
            &mut project,
            serde_json::json!({ "sequence": nested_id.to_string() }),
        )
        .expect("unreferenced sequence removes");
        assert!(!project.sequences.contains_key(&nested_id));
    }

    #[test]
    fn the_last_sequence_cannot_be_removed() {
        let mut project = project_with_clips();
        let err = apply(&SeqRemove, &mut project, serde_json::json!({}))
            .expect_err("the only sequence must stay");
        assert!(err.to_string().contains("at least one sequence"), "{err}");
    }

    #[test]
    fn nesting_a_range_moves_the_material_and_leaves_one_clip() {
        let mut project = project_with_clips();
        let seq = project.active_sequence.clone();

        let effect = apply(
            &SeqNest,
            &mut project,
            serde_json::json!({ "start": "1", "end": "3", "name": "middle" }),
        )
        .expect("nest");

        let sequence = project.sequence(&seq).expect("main");
        let starts: Vec<String> = sequence.tracks[0]
            .clips
            .iter()
            .map(|clip| clip.start.to_string())
            .collect();
        assert_eq!(starts, vec!["0/1", "1/1", "3/1"], "timeline shape changed");
        let nested_clip = &sequence.tracks[0].clips[1];
        let nested_id = match &nested_clip.source {
            Source::Sequence { sequence } => sequence.clone(),
            other => panic!("expected a nested sequence source, got {other:?}"),
        };
        assert_eq!(nested_clip.duration, Time::from_secs(2));

        let nested = project.sequence(&nested_id).expect("nested sequence");
        assert_eq!(nested.name, "middle");
        assert_eq!(nested.tracks.len(), 1);
        assert_eq!(nested.tracks[0].clips.len(), 1);
        assert_eq!(nested.tracks[0].clips[0].start, Time::ZERO);
        assert_eq!(nested.tracks[0].clips[0].duration, Time::from_secs(2));
        assert!(sequence.validate().is_ok() && nested.validate().is_ok());
        assert!(
            effect.removed.iter().any(|id| id == "clp_b"),
            "the consumed clip should be reported: {:?}",
            effect.removed
        );
    }

    #[test]
    fn nesting_splits_a_clip_that_straddles_the_range() {
        let mut project = Project::new("promo", fps30(), [1920, 1080], 48_000);
        let seq = project.active_sequence.clone();
        let mut track = Track::new("V1", TrackKind::Video);
        let mut long = clip("clp_long", 0, 10);
        long.source_in = Time::from_secs(5);
        track.clips = vec![long];
        project.sequence_mut(&seq).expect("main").tracks.push(track);

        apply(
            &SeqNest,
            &mut project,
            serde_json::json!({ "start": "2", "duration": "3" }),
        )
        .expect("nest");

        let sequence = project.sequence(&seq).expect("main");
        let clips = &sequence.tracks[0].clips;
        assert_eq!(clips.len(), 3, "head, nest and tail should remain");
        assert_eq!(clips[0].id.as_str(), "clp_long", "the head keeps the id");
        assert_eq!(clips[0].duration, Time::from_secs(2));
        assert_eq!(clips[1].start, Time::from_secs(2));
        assert_eq!(clips[2].start, Time::from_secs(5));
        // The tail reads source from 5 s in plus the 5 s of timeline that preceded it.
        assert_eq!(clips[2].source_in, Time::from_secs(10));
        assert!(sequence.validate().is_ok());

        let nested_id = match &clips[1].source {
            Source::Sequence { sequence } => sequence.clone(),
            other => panic!("expected a nested sequence source, got {other:?}"),
        };
        let inner = &project.sequence(&nested_id).expect("nested").tracks[0].clips[0];
        assert_eq!(inner.source_in, Time::from_secs(7));
        assert_eq!(inner.duration, Time::from_secs(3));
    }

    #[test]
    fn duplicating_a_sequence_shares_no_ids_with_the_original() {
        let mut project = project_with_clips();
        let seq = project.active_sequence.clone();
        let sequence = project.sequence_mut(&seq).expect("main");
        let mut effect = crate::project::Effect::new("blur");
        effect.id = EffectId::from_raw("fx_blur");
        sequence.tracks[0].clips[0].effects.push(effect);
        sequence.tracks[0].clips[0].keyframes.insert(
            "fx.fx_blur.amount".into(),
            vec![Keyframe {
                at: Time::ZERO,
                value: 4.0,
                easing: Default::default(),
            }],
        );
        let link = sequence.tracks[0].clips[1].id.clone();
        sequence.tracks[0].clips[0].link = Some(link);

        apply(&SeqDuplicate, &mut project, serde_json::json!({})).expect("duplicate");

        let copy = project
            .sequences
            .values()
            .find(|candidate| candidate.name == "main copy")
            .expect("the copy exists");
        let source = project.sequence(&seq).expect("main");
        assert_ne!(copy.id, source.id);
        assert_ne!(copy.tracks[0].id, source.tracks[0].id);
        assert_ne!(copy.tracks[0].clips[0].id, source.tracks[0].clips[0].id);

        let copied_fx = copy.tracks[0].clips[0].effects[0].id.clone();
        assert_ne!(copied_fx.as_str(), "fx_blur");
        assert!(
            copy.tracks[0]
                .clips[0]
                .keyframes
                .contains_key(&format!("fx.{copied_fx}.amount")),
            "keyframe paths still point at the original effect id: {:?}",
            copy.tracks[0].clips[0].keyframes.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            copy.tracks[0].clips[0].link.as_ref(),
            Some(&copy.tracks[0].clips[1].id),
            "the a/v link should point inside the copy"
        );
    }

    #[test]
    fn a_new_sequence_inherits_the_active_format_and_refuses_a_taken_name() {
        let mut project = Project::new("promo", Fps::new(24000, 1001).unwrap(), [3840, 2160], 44_100);
        apply(
            &SeqNew,
            &mut project,
            serde_json::json!({ "name": "titles", "activate": true }),
        )
        .expect("new sequence");

        let created = project.sequence(&project.active_sequence.clone()).expect("active");
        assert_eq!(created.name, "titles");
        assert_eq!(created.fps, Fps::new(24000, 1001).unwrap());
        assert_eq!(created.size, [3840, 2160]);
        assert_eq!(created.sample_rate, 44_100);

        let err = apply(&SeqNew, &mut project, serde_json::json!({ "name": "titles" }))
            .expect_err("names address sequences and stay unique");
        assert_eq!(err.exit_code(), crate::error::exit::BAD_ARGS);
    }

    #[test]
    fn an_explicit_format_overrides_the_inherited_one() {
        let mut project = Project::new("promo", fps30(), [1920, 1080], 48_000);
        apply(
            &SeqNew,
            &mut project,
            serde_json::json!({ "name": "vertical", "size": [1080, 1920], "fps": 60, "sample-rate": 96_000 }),
        )
        .expect("new sequence");
        let created = project
            .sequences
            .values()
            .find(|sequence| sequence.name == "vertical")
            .expect("created");
        assert_eq!(created.size, [1080, 1920]);
        assert_eq!(created.fps, Fps::new(60, 1).unwrap());
        assert_eq!(created.sample_rate, 96_000);
    }

    #[test]
    fn a_malformed_size_is_an_error_not_a_default() {
        let mut project = Project::new("promo", fps30(), [1920, 1080], 48_000);
        for bad in ["1920", "1920x0", "wide"] {
            let err = apply(
                &SeqNew,
                &mut project,
                serde_json::json!({ "name": format!("s-{bad}"), "size": bad }),
            )
            .expect_err("bad size must fail");
            assert_eq!(err.exit_code(), crate::error::exit::BAD_ARGS, "{bad}: {err}");
        }
    }
}
