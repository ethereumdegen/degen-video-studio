//! Clip ops: every edit that moves, sizes or restyles a clip on the timeline.
//!
//! Three decisions shape this module and are worth stating once instead of re-deriving
//! them at nineteen call sites.
//!
//! **Edges are timeline instants, not source offsets.** `clip.trim --out 00:10` means "the
//! clip's last frame is the one before 00:10 on the timeline", and the source in-point is
//! recomputed so the content under the surviving frames does not move. That is what a trim
//! is in an editor, it is what makes `--mode roll` expressible at all (a roll moves one
//! shared edge, so the argument has to name a place on the timeline), and it keeps the
//! three arguments independent: `--in`/`--out` move an edge, `--start` moves the whole
//! clip, and `clip.slip` moves the content under a clip that stays put.
//!
//! **Nothing here can produce an overlap or an empty clip.** [`crate::project::Sequence::validate`]
//! would reject both, but "clips 'a' and 'b' overlap" arrives after the fact and does not
//! say what to do about it. Every op checks the range it is about to occupy and fails
//! naming the clip in the way — or clamps and reports a `past-source-end` warning where
//! clamping is the honest answer, because an agent asking for more frames than the media
//! has wants all of them, not an error.
//!
//! **Retiming is stored as `duration` + `speed`, never as a source out-point.** A trim and
//! a speed change therefore cannot disagree: the source span is always
//! `[source_in, source_in + duration * speed)`, computed and never stored. Reverse
//! playback consumes that same span backwards, which is why every edge calculation has a
//! `reverse` branch — for a reversed clip the timeline head is the far end of the source.

use crate::error::{Error, Result};
use crate::ids::{ClipId, SequenceId, TrackId};
use crate::op::{args, Op, OpCx, OpEffect, Registry};
use crate::ops::util;
use crate::project::{
    eval_keyframes, Blend, Clip, Crop, Fit, Keyframe, Project, Sequence, Track, TrackKind,
};
use crate::selector::{resolve_clips, resolve_one_clip, resolve_track};
use crate::time::{Fps, Rat, Span, Time};
use std::collections::BTreeMap;

pub fn register(registry: &mut Registry) {
    registry
        .register(Insert)
        .register(Overwrite)
        .register(Append)
        .register(Remove)
        .register(Split)
        .register(Trim)
        .register(Slip)
        .register(Slide)
        .register(MoveClip)
        .register(Speed)
        .register(Link)
        .register(Unlink)
        .register(Rename)
        .register(SetTransform)
        .register(SetOpacity)
        .register(SetBlend)
        .register(SetCrop)
        .register(SetFit)
        .register(SetEnabled);
}

// ---------------------------------------------------------------- argument plumbing

const PLACE_ARGS: &[&str] = &[
    "track",
    "source",
    "at",
    "duration",
    "source-in",
    "source-out",
    "name",
];

const APPEND_ARGS: &[&str] = &[
    "track",
    "source",
    "duration",
    "source-in",
    "source-out",
    "name",
];

/// Snap a requested time to the sequence frame grid and record the snap under the argument
/// name the caller used. Every time an agent supplies comes through here: `42.5` on a
/// `30000/1001` timeline is frame 1274, and that difference belongs in the report rather
/// than in a surprise three edits later.
fn snap_arg(
    args: &serde_json::Value,
    key: &'static str,
    fps: Fps,
    effect: &mut OpEffect,
) -> Result<Option<Time>> {
    let Some(requested) = args::opt_time(args, key, fps)? else {
        return Ok(None);
    };
    let applied = requested.snap(fps);
    let carried = std::mem::take(effect);
    *effect = carried.snap(key, requested, applied, fps);
    Ok(Some(applied))
}

fn snap_required(
    args: &serde_json::Value,
    key: &'static str,
    fps: Fps,
    effect: &mut OpEffect,
) -> Result<Time> {
    snap_arg(args, key, fps, effect)?
        .ok_or_else(|| Error::bad_args(format!("missing time field '{key}'")))
}

/// Absolute positions cannot be negative; only deltas (`--by`) can.
fn assert_on_timeline(value: Time, key: &str) -> Result<()> {
    if value.is_negative() {
        return Err(Error::bad_args(format!(
            "field '{key}': {value} is before the start of the timeline"
        )));
    }
    Ok(())
}

fn floor_frame(value: Time, fps: Fps) -> Time {
    Time::from_frames(value.frame_floor(fps), fps)
}

fn ceil_frame(value: Time, fps: Fps) -> Time {
    Time::from_frames(value.frame_ceil(fps), fps)
}

/// `"x,y"`, `[x, y]`, or one number meaning both components — `--scale 2` is how an agent
/// says "twice as big", and refusing it would be pedantry.
fn opt_pair(args: &serde_json::Value, key: &str) -> Result<Option<[f32; 2]>> {
    let bad = || Error::bad_args(format!("field '{key}' must be 'x,y', [x, y] or one number"));
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(number)) => {
            let value = finite(number.as_f64().ok_or_else(bad)?, key)?;
            Ok(Some([value, value]))
        }
        Some(serde_json::Value::Array(items)) => {
            if items.len() != 2 {
                return Err(bad());
            }
            let mut out = [0.0f32; 2];
            for (slot, item) in out.iter_mut().zip(items) {
                *slot = finite(item.as_f64().ok_or_else(bad)?, key)?;
            }
            Ok(Some(out))
        }
        Some(serde_json::Value::String(text)) => {
            let values: Vec<f32> = text
                .split(',')
                .map(|part| {
                    part.trim()
                        .parse::<f64>()
                        .map_err(|_| bad())
                        .and_then(|value| finite(value, key))
                })
                .collect::<Result<_>>()?;
            match values.as_slice() {
                [both] => Ok(Some([*both, *both])),
                [x, y] => Ok(Some([*x, *y])),
                _ => Err(bad()),
            }
        }
        Some(_) => Err(bad()),
    }
}

fn finite(value: f64, key: &str) -> Result<f32> {
    if !value.is_finite() {
        return Err(Error::bad_args(format!("field '{key}' must be finite")));
    }
    Ok(value as f32)
}

/// A closed vocabulary parsed through serde, so the spellings an op accepts are exactly the
/// document's and cannot drift from it.
fn parse_enum<T: serde::de::DeserializeOwned>(text: &str, key: &str, allowed: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(text.to_string())).map_err(|_| {
        Error::bad_args(format!("field '{key}': '{text}' is not valid; use {allowed}"))
    })
}

fn string_schema(description: &str) -> serde_json::Value {
    serde_json::json!({ "type": "string", "description": description })
}

fn time_schema(description: &str) -> serde_json::Value {
    serde_json::json!({
        "type": ["string", "number"],
        "description": format!("{description} — seconds, 'num/den', 'mm:ss.mmm', '1m12s' or '1800f'")
    })
}

fn bool_schema(description: &str) -> serde_json::Value {
    serde_json::json!({ "type": "boolean", "description": description })
}

fn number_schema(description: &str) -> serde_json::Value {
    serde_json::json!({ "type": "number", "description": description })
}

fn clip_schema(many: bool) -> serde_json::Value {
    serde_json::json!({
        "type": "string",
        "description": if many {
            "selector naming one or more clips: an id, '#name', or 'clip[track=V1]'"
        } else {
            "selector naming exactly one clip: an id or '#name'"
        }
    })
}

fn object_schema(properties: serde_json::Value, required: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn placement_properties(with_at: bool) -> serde_json::Value {
    let mut properties = serde_json::json!({
        "track": string_schema("track to place the clip on, by name ('V1') or id"),
        "source": string_schema(
            "what the clip shows: an asset id/name, 'image:<asset>', 'color:#rrggbb', \
             'title:<id|name>', 'seq:<id|name>', or a generator (bars, tone, countdown, frame-numbers)"
        ),
        "duration": time_schema("how long the clip runs on the timeline"),
        "source-in": time_schema("first instant used from the source"),
        "source-out": time_schema("end of the range used from the source; an alternative to 'duration'"),
        "name": string_schema("clip name, unique within the sequence")
    });
    if with_at {
        properties["at"] = time_schema("where on the timeline the clip starts");
    }
    properties
}

// ---------------------------------------------------------------- shared timeline logic

/// Source length available to every clip in a sequence, gathered before anything is
/// mutated: the clamp needs the whole project while the edit needs the track exclusively.
type Limits = BTreeMap<ClipId, Option<Time>>;

fn source_limits(project: &Project, seq: &SequenceId) -> Result<Limits> {
    let sequence = project.sequence(seq)?;
    let mut limits = Limits::new();
    for track in &sequence.tracks {
        for clip in &track.clips {
            limits.insert(
                clip.id.clone(),
                util::source_available(project, &clip.source),
            );
        }
    }
    Ok(limits)
}

fn limit_of(limits: &Limits, clip: &ClipId) -> Option<Time> {
    limits.get(clip).copied().flatten()
}

/// Targets for an op that edits clips in place: the clips a selector named, plus each
/// one's a/v counterpart when `links` is set, grouped per track and ordered late-to-early
/// so an edit that changes one clip's length cannot invalidate a target not reached yet.
/// Locked and caption tracks are refused here, before anything is touched.
fn targets(
    project: &Project,
    seq: &SequenceId,
    text: &str,
    links: bool,
) -> Result<Vec<(TrackId, Vec<ClipId>)>> {
    let mut pairs = resolve_clips(project, seq, text)?;
    let sequence = project.sequence(seq)?;
    if links {
        let mut counterparts = Vec::new();
        for (_, clip_id) in &pairs {
            let Some((_, clip)) = sequence.find_clip(clip_id) else {
                continue;
            };
            let Some(link) = clip.link.clone() else {
                continue;
            };
            if let Some((track, linked)) = sequence.find_clip(&link) {
                counterparts.push((track.id.clone(), linked.id.clone()));
            }
        }
        pairs.extend(counterparts);
    }

    let mut groups: Vec<(TrackId, Vec<ClipId>)> = Vec::new();
    for (track_id, clip_id) in pairs {
        match groups.iter_mut().find(|(id, _)| *id == track_id) {
            Some((_, clips)) => {
                if !clips.contains(&clip_id) {
                    clips.push(clip_id);
                }
            }
            None => groups.push((track_id, vec![clip_id])),
        }
    }
    for (track_id, clips) in &mut groups {
        let track = sequence.track(track_id)?;
        util::assert_unlocked(track)?;
        util::assert_takes_clips(track)?;
        let mut indexed: Vec<(usize, ClipId)> = clips
            .iter()
            .map(|id| util::clip_index(track, id).map(|index| (index, id.clone())))
            .collect::<Result<_>>()?;
        indexed.sort_by(|a, b| b.0.cmp(&a.0));
        *clips = indexed.into_iter().map(|(_, id)| id).collect();
    }
    Ok(groups)
}

/// The property ops all do the same three things — resolve, refuse a locked track, report
/// every id they touched — and differ only in the field they set.
fn for_each_clip(
    project: &mut Project,
    seq: &SequenceId,
    text: &str,
    mut edit: impl FnMut(&mut Clip) -> Result<()>,
) -> Result<OpEffect> {
    let groups = targets(project, seq, text, false)?;
    let mut effect = OpEffect::new();
    let sequence = project.sequence_mut(seq)?;
    for (track_id, clips) in &groups {
        let track = sequence.track_mut(track_id)?;
        for clip_id in clips {
            let index = util::clip_index(track, clip_id)?;
            edit(&mut track.clips[index])?;
            effect = effect.changed(clip_id);
        }
    }
    Ok(effect)
}

/// Names address clips (`#intro`), so a duplicate would make a selector ambiguous the next
/// time it is used rather than now, when the caller can still choose another name.
fn assert_name_free(sequence: &Sequence, name: Option<&str>, ignore: Option<&ClipId>) -> Result<()> {
    let Some(name) = name else { return Ok(()) };
    let taken = sequence
        .tracks
        .iter()
        .flat_map(|track| &track.clips)
        .any(|clip| Some(&clip.id) != ignore && clip.name.as_deref() == Some(name));
    if taken {
        return Err(Error::bad_args(format!(
            "another clip in '{}' is already named '{name}'",
            sequence.name
        )));
    }
    Ok(())
}

/// Build the clip an insert, overwrite or append places: resolve `--source`, and work out
/// how long it should run.
fn planned_clip(
    project: &Project,
    args: &serde_json::Value,
    start: Time,
    fps: Fps,
) -> Result<(Clip, OpEffect)> {
    let text = args::str_field(args, "source")?;
    let source = util::parse_source(project, text)?;
    let available = util::source_available(project, &source);
    let mut effect = OpEffect::new();

    let source_in = snap_arg(args, "source-in", fps, &mut effect)?.unwrap_or(Time::ZERO);
    if source_in.is_negative() {
        return Err(Error::bad_args("field 'source-in' cannot be negative"));
    }
    let source_out = snap_arg(args, "source-out", fps, &mut effect)?;
    let requested = snap_arg(args, "duration", fps, &mut effect)?;
    if requested.is_some() && source_out.is_some() {
        return Err(Error::bad_args(
            "pass either 'duration' or 'source-out', not both",
        ));
    }
    if let Some(total) = available {
        if total.is_positive() && source_in >= total {
            return Err(Error::op(format!(
                "'source-in' {source_in} is at or past the end of {text} ({total})"
            )));
        }
    }

    let mut duration = match (requested, source_out) {
        (Some(duration), _) => duration,
        (None, Some(out)) => {
            if out <= source_in {
                return Err(Error::bad_args(format!(
                    "'source-out' {out} is not after 'source-in' {source_in}"
                )));
            }
            out - source_in
        }
        // "Use this media" means all of it that is left, which is the only default that
        // does not make the agent probe the asset before it can place a clip.
        (None, None) => match available {
            Some(total) => total - source_in,
            None => {
                return Err(Error::bad_args(format!(
                    "source '{text}' has no length of its own; pass 'duration'"
                )))
            }
        },
    };

    let mut clamped = None;
    if let Some(total) = available {
        let room = total - source_in;
        if duration > room {
            clamped = Some(room);
            duration = room;
        }
    }
    // A clip that is not a whole number of frames long cannot be rendered without a
    // partial frame, so media that ends mid-frame gives up that frame.
    duration = floor_frame(duration, fps);
    if !duration.is_positive() {
        return Err(Error::op(format!(
            "{text} leaves no whole frame to place at {fps} fps"
        )));
    }

    let mut clip = Clip::new(source, start, duration);
    clip.source_in = source_in;
    if let Some(name) = args::opt_str(args, "name") {
        if name.trim().is_empty() {
            return Err(Error::bad_args("field 'name' cannot be empty"));
        }
        clip.name = Some(name.to_string());
    }
    if let Some(room) = clamped {
        effect = effect.warn(
            "past-source-end",
            &clip.id,
            format!("{text} has only {room} left after {source_in}; the clip runs {duration}"),
        );
    }
    Ok((clip, effect))
}

/// Split a clip in two at a timeline instant, returning the part that starts later.
///
/// The later part's source in-point is advanced by exactly what the earlier part consumes,
/// so the halves play as the one clip did. Everything that belongs to an *edge* rather than
/// to the content — the incoming transition, the fades, the a/v link — stays on the side it
/// belongs to instead of being duplicated onto both.
///
/// Public because every op that cuts a clip — `clip.split`, `clip.insert` landing inside
/// one, `clip.overwrite`, `seq.nest`, scene detection — has to cut it the same way, and a
/// second implementation of the source-in arithmetic would be a second set of off-by-one
/// bugs.
///
/// The caller owns two things this function cannot check: `at` must be strictly inside
/// `clip.start .. clip.end()` (on a boundary it would hand back a zero-length half, which
/// [`Sequence::validate`] rejects), and the returned clip must be inserted into the track
/// immediately after the one that was split, to keep the list sorted by start.
pub fn split_at(clip: &mut Clip, at: Time) -> Clip {
    let head_len = at - clip.start;
    let tail_len = clip.end() - at;

    let mut tail = clip.clone();
    tail.id = ClipId::new();
    tail.start = at;
    tail.duration = tail_len;
    tail.transition_in = None;
    // The a/v pairing is one-to-one: leaving both halves pointing at the counterpart would
    // make "the linked clip" ambiguous, which is worse than losing the pairing.
    tail.link = None;
    tail.fade_in = Time::ZERO;
    tail.fade_out = clip.fade_out.min(tail_len);
    if clip.reverse {
        // Reversed playback walks the source span downwards, so the earlier half of the
        // timeline is the upper half of the source.
        tail.source_in = clip.source_in;
        clip.source_in = clip.source_in + tail_len * clip.speed;
    } else {
        tail.source_in = clip.source_in + head_len * clip.speed;
    }

    let mut head_keys = BTreeMap::new();
    let mut tail_keys = BTreeMap::new();
    for (path, keys) in &clip.keyframes {
        let (head, split_tail) = split_keys(keys, head_len);
        head_keys.insert(path.clone(), head);
        tail_keys.insert(path.clone(), split_tail);
    }
    tail.keyframes = tail_keys;
    clip.keyframes = head_keys;

    clip.duration = head_len;
    clip.fade_in = clip.fade_in.min(head_len);
    clip.fade_out = Time::ZERO;
    tail
}

/// Keyframe times are clip-local, so a split rebases the later half and gives both halves a
/// key on the boundary: without one the animation would jump back to its first value in the
/// middle of what used to be a single move.
fn split_keys(keys: &[Keyframe], at: Time) -> (Vec<Keyframe>, Vec<Keyframe>) {
    if keys.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let value = eval_keyframes(keys, at);
    let easing = keys
        .iter()
        .rev()
        .find(|key| key.at <= at)
        .map(|key| key.easing)
        .unwrap_or_default();
    let mut head: Vec<Keyframe> = keys.iter().copied().filter(|key| key.at < at).collect();
    head.push(Keyframe { at, value, easing });
    let mut tail = vec![Keyframe {
        at: Time::ZERO,
        value,
        easing,
    }];
    tail.extend(keys.iter().filter(|key| key.at > at).map(|key| Keyframe {
        at: key.at - at,
        value: key.value,
        easing: key.easing,
    }));
    (head, tail)
}

/// Move a clip's head to `to`, keeping the content under the surviving frames where it is.
fn trim_in(clip: &mut Clip, to: Time) {
    let delta = to - clip.start;
    if !clip.reverse {
        clip.source_in = clip.source_in + delta * clip.speed;
    }
    // A reversed clip draws from the top of its source span, so giving up output at the
    // head gives up source at the end of the span and leaves `source_in` alone.
    clip.start = to;
    clip.duration = clip.duration - delta;
    clip.fade_in = clip.fade_in.min(clip.duration);
}

/// Move a clip's tail to `to`, keeping its first frame where it is.
fn trim_out(clip: &mut Clip, to: Time) {
    let delta = to - clip.end();
    if clip.reverse {
        clip.source_in = clip.source_in - delta * clip.speed;
    }
    clip.duration = clip.duration + delta;
    clip.fade_out = clip.fade_out.min(clip.duration);
}

/// Furthest a clip's out edge can go before it points past the end of its source. `None`
/// when the source is synthesised and has no end.
fn out_limit(clip: &Clip, available: Option<Time>) -> Option<Time> {
    let total = available?;
    Some(if clip.reverse {
        clip.end() + clip.source_in / clip.speed
    } else {
        clip.start + (total - clip.source_in) / clip.speed
    })
}

/// Earliest a clip's in edge can go before it points before the start of its source.
fn in_limit(clip: &Clip, available: Option<Time>) -> Option<Time> {
    let total = available?;
    Some(if clip.reverse {
        clip.end() - (total - clip.source_in) / clip.speed
    } else {
        clip.start - clip.source_in / clip.speed
    })
}

/// Clamp an edge into what its source can supply, and say so.
///
/// Refusing would be wrong: an agent that asks to trim past the end of the media wants as
/// much as there is. Accepting silently would be worse — the clip would point at frames
/// that do not exist and the render would show black. So it clamps and warns.
fn clamp_edge(
    to: Time,
    lo: Option<Time>,
    hi: Option<Time>,
    fps: Fps,
    target: &ClipId,
    what: &'static str,
) -> (Time, OpEffect) {
    let effect = OpEffect::new();
    if let Some(hi) = hi.map(|hi| floor_frame(hi, fps)) {
        if to > hi {
            return (
                hi,
                effect.warn(
                    "past-source-end",
                    target,
                    format!("{what} clamped from {to} to {hi}: the source ends there"),
                ),
            );
        }
    }
    if let Some(lo) = lo.map(|lo| ceil_frame(lo, fps)) {
        if to < lo {
            return (
                lo,
                effect.warn(
                    "past-source-start",
                    target,
                    format!("{what} clamped from {to} to {lo}: the source starts there"),
                ),
            );
        }
    }
    (to, effect)
}

/// What clearing a range did to the clips that were in it.
#[derive(Default)]
struct Cleared {
    removed: Vec<ClipId>,
    changed: Vec<ClipId>,
    created: Vec<ClipId>,
}

/// Make room on a track: clips overlapping `span` are trimmed back to its edges, split in
/// two when the span falls inside one, and removed when nothing of them survives. This is
/// the overwrite rule, and the only path by which a clip is destroyed without being named.
fn clear_span(track: &mut Track, span: Span) -> Cleared {
    let mut cleared = Cleared::default();
    let mut index = 0;
    while index < track.clips.len() {
        let clip_span = track.clips[index].span();
        if !clip_span.overlaps(&span) {
            index += 1;
            continue;
        }
        match (clip_span.start >= span.start, clip_span.end <= span.end) {
            (true, true) => {
                // Removed, so the next clip slides into this index: do not advance.
                cleared.removed.push(track.clips.remove(index).id);
            }
            (false, false) => {
                let tail = split_at(&mut track.clips[index], span.end);
                trim_out(&mut track.clips[index], span.start);
                cleared.changed.push(track.clips[index].id.clone());
                cleared.created.push(tail.id.clone());
                track.clips.insert(index + 1, tail);
                index += 2;
            }
            (false, true) => {
                trim_out(&mut track.clips[index], span.start);
                cleared.changed.push(track.clips[index].id.clone());
                index += 1;
            }
            (true, false) => {
                trim_in(&mut track.clips[index], span.end);
                cleared.changed.push(track.clips[index].id.clone());
                index += 1;
            }
        }
    }
    cleared
}

/// Clear links pointing at clips that no longer exist. A dangling link is what the
/// `av-drift` lint reports, and not creating one is this module's job.
fn drop_links_to(sequence: &mut Sequence, gone: &[ClipId]) -> Vec<ClipId> {
    if gone.is_empty() {
        return Vec::new();
    }
    let mut changed = Vec::new();
    for track in &mut sequence.tracks {
        for clip in &mut track.clips {
            if clip.link.as_ref().is_some_and(|link| gone.contains(link)) {
                clip.link = None;
                changed.push(clip.id.clone());
            }
        }
    }
    changed
}

/// A video clip on an audio track would have nothing to render and nothing to mix.
fn assert_same_kind(from: TrackKind, to: TrackKind, name: &str) -> Result<()> {
    if from != to {
        return Err(Error::op(format!(
            "track '{name}' is a {to:?} track; a {from:?} clip cannot live there"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------- placement

pub struct Insert;

impl Op for Insert {
    fn id(&self) -> &'static str {
        "clip.insert"
    }
    fn about(&self) -> &'static str {
        "Insert a clip, rippling everything at or after that point later by its duration; a clip the insert lands inside is split"
    }
    fn schema(&self) -> serde_json::Value {
        object_schema(placement_properties(true), &["track", "source", "at"])
    }
    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, PLACE_ARGS)?;
        let seq = cx.sequence(project)?;
        let fps = util::sequence_fps(project, &seq)?;
        let mut effect = OpEffect::new();
        let track_id = resolve_track(project, &seq, args::str_field(&args, "track")?)?;
        let at = snap_required(&args, "at", fps, &mut effect)?;
        assert_on_timeline(at, "at")?;
        {
            let track = project.sequence(&seq)?.track(&track_id)?;
            util::assert_unlocked(track)?;
            util::assert_takes_clips(track)?;
        }
        let (clip, planned) = planned_clip(project, &args, at, fps)?;
        effect.merge(planned);
        assert_name_free(project.sequence(&seq)?, clip.name.as_deref(), None)?;

        let duration = clip.duration;
        let created = clip.id.clone();
        let track = project.sequence_mut(&seq)?.track_mut(&track_id)?;
        // An insert landing inside a clip splits it, the way an insert edit does in every
        // editor: the tail travels with the rest of the timeline instead of being erased.
        if let Some(index) = track
            .clips
            .iter()
            .position(|clip| clip.start < at && clip.end() > at)
        {
            let tail = split_at(&mut track.clips[index], at);
            effect = effect
                .changed(track.clips[index].id.clone())
                .created(tail.id.clone());
            track.clips.insert(index + 1, tail);
        }
        util::ripple_after(track, at, duration);
        track.place(clip);
        Ok(effect.created(created))
    }
}

pub struct Overwrite;

impl Op for Overwrite {
    fn id(&self) -> &'static str {
        "clip.overwrite"
    }
    fn about(&self) -> &'static str {
        "Place a clip over whatever is there: neighbours are trimmed or split around it, covered clips are removed, nothing ripples"
    }
    fn schema(&self) -> serde_json::Value {
        object_schema(placement_properties(true), &["track", "source", "at"])
    }
    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, PLACE_ARGS)?;
        let seq = cx.sequence(project)?;
        let fps = util::sequence_fps(project, &seq)?;
        let mut effect = OpEffect::new();
        let track_id = resolve_track(project, &seq, args::str_field(&args, "track")?)?;
        let at = snap_required(&args, "at", fps, &mut effect)?;
        assert_on_timeline(at, "at")?;
        {
            let track = project.sequence(&seq)?.track(&track_id)?;
            util::assert_unlocked(track)?;
            util::assert_takes_clips(track)?;
        }
        let (clip, planned) = planned_clip(project, &args, at, fps)?;
        effect.merge(planned);
        assert_name_free(project.sequence(&seq)?, clip.name.as_deref(), None)?;

        let span = clip.span();
        let created = clip.id.clone();
        let sequence = project.sequence_mut(&seq)?;
        let cleared = {
            let track = sequence.track_mut(&track_id)?;
            let cleared = clear_span(track, span);
            track.place(clip);
            cleared
        };
        for id in &cleared.removed {
            effect = effect.removed(id);
        }
        for id in &cleared.changed {
            effect = effect.changed(id);
        }
        for id in &cleared.created {
            effect = effect.created(id);
        }
        for id in drop_links_to(sequence, &cleared.removed) {
            effect = effect.changed(id);
        }
        Ok(effect.created(created))
    }
}

pub struct Append;

impl Op for Append {
    fn id(&self) -> &'static str {
        "clip.append"
    }
    fn about(&self) -> &'static str {
        "Place a clip at the end of a track"
    }
    fn schema(&self) -> serde_json::Value {
        object_schema(placement_properties(false), &["track", "source"])
    }
    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, APPEND_ARGS)?;
        let seq = cx.sequence(project)?;
        let fps = util::sequence_fps(project, &seq)?;
        let track_id = resolve_track(project, &seq, args::str_field(&args, "track")?)?;
        let at = {
            let track = project.sequence(&seq)?.track(&track_id)?;
            util::assert_unlocked(track)?;
            util::assert_takes_clips(track)?;
            util::track_end(track)
        };
        let (clip, effect) = planned_clip(project, &args, at, fps)?;
        assert_name_free(project.sequence(&seq)?, clip.name.as_deref(), None)?;
        let created = clip.id.clone();
        project.sequence_mut(&seq)?.track_mut(&track_id)?.place(clip);
        Ok(effect.created(created))
    }
}

pub struct Remove;

impl Op for Remove {
    fn id(&self) -> &'static str {
        "clip.remove"
    }
    fn about(&self) -> &'static str {
        "Remove clips, leaving a gap; with --ripple the gap is closed"
    }
    fn schema(&self) -> serde_json::Value {
        object_schema(
            serde_json::json!({
                "target": clip_schema(true),
                "ripple": bool_schema("close the gap by pulling everything after the clip earlier")
            }),
            &["target"],
        )
    }
    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "ripple"])?;
        let seq = cx.sequence(project)?;
        let ripple = args::opt_bool(&args, "ripple")?.unwrap_or(false);
        let groups = targets(project, &seq, args::str_field(&args, "target")?, false)?;
        let mut effect = OpEffect::new();
        let mut removed = Vec::new();
        let sequence = project.sequence_mut(&seq)?;
        for (track_id, clips) in &groups {
            let track = sequence.track_mut(track_id)?;
            // Late-to-early, so closing one gap cannot move a clip still to be removed.
            for clip_id in clips {
                let index = util::clip_index(track, clip_id)?;
                let span = track.clips[index].span();
                track.clips.remove(index);
                if ripple {
                    util::ripple_after(track, span.end, -span.duration());
                }
                removed.push(clip_id.clone());
                effect = effect.removed(clip_id);
            }
        }
        for id in drop_links_to(sequence, &removed) {
            effect = effect.changed(id);
        }
        Ok(effect)
    }
}

pub struct Split;

impl Op for Split {
    fn id(&self) -> &'static str {
        "clip.split"
    }
    fn about(&self) -> &'static str {
        "Split the clip containing a time into two clips sharing its source"
    }
    fn schema(&self) -> serde_json::Value {
        object_schema(
            serde_json::json!({
                "target": clip_schema(false),
                "at": time_schema("where to cut")
            }),
            &["target", "at"],
        )
    }
    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "at"])?;
        let seq = cx.sequence(project)?;
        let fps = util::sequence_fps(project, &seq)?;
        let mut effect = OpEffect::new();
        let (track_id, clip_id) = resolve_one_clip(project, &seq, args::str_field(&args, "target")?)?;
        let at = snap_required(&args, "at", fps, &mut effect)?;
        {
            let track = project.sequence(&seq)?.track(&track_id)?;
            util::assert_unlocked(track)?;
            let clip = &track.clips[util::clip_index(track, &clip_id)?];
            // A cut on an edge is not a cut: it would either produce an empty clip or do
            // nothing at all, and both are worse answers than naming the edge that was hit.
            if at <= clip.start {
                return Err(Error::op(format!(
                    "'{}' starts at {}, so a split at {at} is on that boundary, not inside the clip",
                    clip.label(),
                    clip.start
                )));
            }
            if at >= clip.end() {
                return Err(Error::op(format!(
                    "'{}' ends at {}, so a split at {at} is on that boundary, not inside the clip",
                    clip.label(),
                    clip.end()
                )));
            }
        }
        let track = project.sequence_mut(&seq)?.track_mut(&track_id)?;
        let index = util::clip_index(track, &clip_id)?;
        let linked = track.clips[index].link.is_some();
        let tail = split_at(&mut track.clips[index], at);
        let tail_id = tail.id.clone();
        track.clips.insert(index + 1, tail);
        effect = effect.changed(&clip_id).created(&tail_id);
        if linked {
            effect = effect.warn(
                "link-dropped",
                &tail_id,
                "the a/v pairing stayed with the first half; link the new clip if it needs one",
            );
        }
        Ok(effect)
    }
}

// ---------------------------------------------------------------- edges

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrimMode {
    /// Only this clip changes; a gap may open where it used to be.
    None,
    /// The rest of the track follows, so the timeline closes or opens by the same amount.
    Ripple,
    /// The touching neighbour's opposite edge moves with this one; the timeline keeps its
    /// length and the cut point moves.
    Roll,
}

fn trim_mode(args: &serde_json::Value) -> Result<TrimMode> {
    match args::opt_str(args, "mode").unwrap_or("none") {
        "none" => Ok(TrimMode::None),
        "ripple" => Ok(TrimMode::Ripple),
        "roll" => Ok(TrimMode::Roll),
        other => Err(Error::bad_args(format!(
            "field 'mode': '{other}' is not a trim mode; use none, ripple or roll"
        ))),
    }
}

pub struct Trim;

impl Op for Trim {
    fn id(&self) -> &'static str {
        "clip.trim"
    }
    fn about(&self) -> &'static str {
        "Move a clip's in or out edge, or its whole start, in none, ripple or roll mode"
    }
    fn schema(&self) -> serde_json::Value {
        object_schema(
            serde_json::json!({
                "target": clip_schema(true),
                "in": time_schema("new timeline position of the clip's first frame"),
                "out": time_schema("new timeline position of the clip's end edge (exclusive)"),
                "start": time_schema("new timeline start, keeping the clip's duration and content"),
                "mode": {
                    "type": "string",
                    "enum": ["none", "ripple", "roll"],
                    "description": "none: only this clip moves. ripple: the rest of the track follows. roll: the touching neighbour's edge moves with it."
                }
            }),
            &["target"],
        )
    }
    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "in", "out", "start", "mode"])?;
        let seq = cx.sequence(project)?;
        let fps = util::sequence_fps(project, &seq)?;
        let mode = trim_mode(&args)?;
        let mut effect = OpEffect::new();
        let in_point = snap_arg(&args, "in", fps, &mut effect)?;
        let out_point = snap_arg(&args, "out", fps, &mut effect)?;
        let start = snap_arg(&args, "start", fps, &mut effect)?;
        for (value, key) in [(in_point, "in"), (out_point, "out"), (start, "start")] {
            if let Some(value) = value {
                assert_on_timeline(value, key)?;
            }
        }
        if in_point.is_none() && out_point.is_none() && start.is_none() {
            return Err(Error::bad_args(
                "clip.trim needs one of 'in', 'out' or 'start'",
            ));
        }
        if start.is_some() && (in_point.is_some() || out_point.is_some()) {
            return Err(Error::bad_args(
                "'start' moves the whole clip, so it cannot be combined with 'in' or 'out'",
            ));
        }
        if start.is_some() && mode == TrimMode::Roll {
            return Err(Error::bad_args(
                "mode 'roll' moves a shared edge, so it needs 'in' or 'out'; to push a clip into its neighbours use clip.slide",
            ));
        }
        if let (Some(in_point), Some(out_point)) = (in_point, out_point) {
            if out_point <= in_point {
                return Err(Error::bad_args(format!(
                    "'out' {out_point} is not after 'in' {in_point}"
                )));
            }
        }

        let limits = source_limits(project, &seq)?;
        let groups = targets(project, &seq, args::str_field(&args, "target")?, true)?;
        let sequence = project.sequence_mut(&seq)?;
        for (track_id, clips) in &groups {
            let track = sequence.track_mut(track_id)?;
            for clip_id in clips {
                if let Some(to) = start {
                    effect.merge(move_start(track, clip_id, to, mode)?);
                }
                // Out before in: trimming the out edge first can only shorten the range
                // both edges have to fit inside, never invalidate the in edge check.
                if let Some(to) = out_point {
                    effect.merge(trim_out_edge(track, clip_id, to, mode, &limits, fps)?);
                }
                if let Some(to) = in_point {
                    effect.merge(trim_in_edge(track, clip_id, to, mode, &limits, fps)?);
                }
                effect = effect.changed(clip_id);
            }
        }
        Ok(effect)
    }
}

/// `--start`: the clip and its content travel together. In ripple mode the rest of the
/// track travels with it; otherwise the destination has to be empty.
fn move_start(track: &mut Track, clip_id: &ClipId, to: Time, mode: TrimMode) -> Result<OpEffect> {
    let index = util::clip_index(track, clip_id)?;
    let original = track.clips[index].span();
    let delta = to - original.start;
    if delta.is_zero() {
        return Ok(OpEffect::new());
    }
    if mode == TrimMode::Ripple {
        if let Some(previous) = index.checked_sub(1) {
            if track.clips[previous].end() > to {
                return Err(Error::op(format!(
                    "moving '{}' to {to} would run into '{}', which ends at {}",
                    track.clips[index].label(),
                    track.clips[previous].label(),
                    track.clips[previous].end()
                )));
            }
        }
        util::ripple_after(track, original.start, delta);
        return Ok(OpEffect::new());
    }
    util::assert_free(track, original.shifted(delta), Some(clip_id))?;
    track.clips[index].start = to;
    util::resort(track);
    Ok(OpEffect::new())
}

fn trim_in_edge(
    track: &mut Track,
    clip_id: &ClipId,
    to: Time,
    mode: TrimMode,
    limits: &Limits,
    fps: Fps,
) -> Result<OpEffect> {
    let index = util::clip_index(track, clip_id)?;
    let original = track.clips[index].span();
    let source_floor = in_limit(&track.clips[index], limit_of(limits, clip_id));
    // In a roll the neighbour's out edge moves too, so its source bounds this edge as well.
    let neighbour = match mode {
        TrimMode::Roll => Some(adjacent_before(track, index)?),
        _ => None,
    };
    let source_ceiling = neighbour.and_then(|previous| {
        out_limit(
            &track.clips[previous],
            limit_of(limits, &track.clips[previous].id),
        )
    });
    let (to, mut effect) = clamp_edge(to, source_floor, source_ceiling, fps, clip_id, "in point");

    if original.end - to < fps.frame_duration() {
        return Err(Error::op(format!(
            "trimming the in point of '{}' to {to} leaves no frames; it ends at {}",
            track.clips[index].label(),
            original.end
        )));
    }
    if let Some(previous) = neighbour {
        if to - track.clips[previous].start < fps.frame_duration() {
            return Err(Error::op(format!(
                "rolling the edge to {to} leaves nothing of '{}', which starts at {}",
                track.clips[previous].label(),
                track.clips[previous].start
            )));
        }
        trim_out(&mut track.clips[previous], to);
        effect = effect.changed(track.clips[previous].id.clone());
    }

    trim_in(&mut track.clips[index], to);
    match mode {
        TrimMode::Ripple => {
            // A ripple trim leaves no hole in front of the clip: the clip stays where it
            // was and everything behind it moves up by what the head gave away.
            let delta = to - original.start;
            track.clips[index].start = original.start;
            util::ripple_after(track, original.end, -delta);
        }
        TrimMode::None => util::assert_free(track, track.clips[index].span(), Some(clip_id))?,
        TrimMode::Roll => {}
    }
    Ok(effect)
}

fn trim_out_edge(
    track: &mut Track,
    clip_id: &ClipId,
    to: Time,
    mode: TrimMode,
    limits: &Limits,
    fps: Fps,
) -> Result<OpEffect> {
    let index = util::clip_index(track, clip_id)?;
    let original = track.clips[index].span();
    let source_ceiling = out_limit(&track.clips[index], limit_of(limits, clip_id));
    let neighbour = match mode {
        TrimMode::Roll => Some(adjacent_after(track, index)?),
        _ => None,
    };
    let source_floor = neighbour
        .and_then(|next| in_limit(&track.clips[next], limit_of(limits, &track.clips[next].id)));
    let (to, mut effect) = clamp_edge(to, source_floor, source_ceiling, fps, clip_id, "out point");

    if to - original.start < fps.frame_duration() {
        return Err(Error::op(format!(
            "trimming the out point of '{}' to {to} leaves no frames; it starts at {}",
            track.clips[index].label(),
            original.start
        )));
    }
    if let Some(next) = neighbour {
        if track.clips[next].end() - to < fps.frame_duration() {
            return Err(Error::op(format!(
                "rolling the edge to {to} leaves nothing of '{}', which ends at {}",
                track.clips[next].label(),
                track.clips[next].end()
            )));
        }
        trim_in(&mut track.clips[next], to);
        effect = effect.changed(track.clips[next].id.clone());
    }

    trim_out(&mut track.clips[index], to);
    match mode {
        TrimMode::Ripple => util::ripple_after(track, original.end, to - original.end),
        TrimMode::None => util::assert_free(track, track.clips[index].span(), Some(clip_id))?,
        TrimMode::Roll => {}
    }
    Ok(effect)
}

/// The clip whose out edge touches this clip's in edge. A roll with no shared edge is a
/// plain trim, so asking for one is a mistake worth naming rather than quietly honouring.
fn adjacent_before(track: &Track, index: usize) -> Result<usize> {
    let start = track.clips[index].start;
    index
        .checked_sub(1)
        .filter(|previous| track.clips[*previous].end() == start)
        .ok_or_else(|| {
            Error::op(format!(
                "nothing touches the in point of '{}' at {start}; a roll needs a shared edge",
                track.clips[index].label()
            ))
        })
}

fn adjacent_after(track: &Track, index: usize) -> Result<usize> {
    let end = track.clips[index].end();
    Some(index + 1)
        .filter(|next| track.clips.get(*next).is_some_and(|clip| clip.start == end))
        .ok_or_else(|| {
            Error::op(format!(
                "nothing touches the out point of '{}' at {end}; a roll needs a shared edge",
                track.clips[index].label()
            ))
        })
}

pub struct Slip;

impl Op for Slip {
    fn id(&self) -> &'static str {
        "clip.slip"
    }
    fn about(&self) -> &'static str {
        "Move the source in-point under a clip without moving the clip on the timeline"
    }
    fn schema(&self) -> serde_json::Value {
        object_schema(
            serde_json::json!({
                "target": clip_schema(true),
                "by": time_schema("how much later in the source to start, negative for earlier"),
                "to": time_schema("absolute source in-point")
            }),
            &["target"],
        )
    }
    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "by", "to"])?;
        let seq = cx.sequence(project)?;
        let fps = util::sequence_fps(project, &seq)?;
        let mut effect = OpEffect::new();
        let by = snap_arg(&args, "by", fps, &mut effect)?;
        let to = snap_arg(&args, "to", fps, &mut effect)?;
        match (by, to) {
            (Some(_), Some(_)) => {
                return Err(Error::bad_args("pass either 'by' or 'to', not both"))
            }
            (None, None) => return Err(Error::bad_args("clip.slip needs 'by' or 'to'")),
            _ => {}
        }
        if let Some(to) = to {
            assert_on_timeline(to, "to")?;
        }

        let limits = source_limits(project, &seq)?;
        let groups = targets(project, &seq, args::str_field(&args, "target")?, false)?;
        let sequence = project.sequence_mut(&seq)?;
        for (track_id, clips) in &groups {
            let track = sequence.track_mut(track_id)?;
            for clip_id in clips {
                let index = util::clip_index(track, clip_id)?;
                let limit = limit_of(&limits, clip_id);
                let clip = &mut track.clips[index];
                let consumed = clip.duration * clip.speed;
                let wanted = match (by, to) {
                    (Some(by), _) => clip.source_in + by,
                    (_, Some(to)) => to,
                    _ => unreachable!("one of 'by' and 'to' is present"),
                };
                let mut applied = wanted.max(Time::ZERO);
                if applied != wanted {
                    effect = effect.warn(
                        "past-source-start",
                        clip_id,
                        format!("slip clamped from {wanted} to {applied}: the source starts there"),
                    );
                }
                if let Some(total) = limit {
                    let last = floor_frame((total - consumed).max(Time::ZERO), fps);
                    if applied > last {
                        effect = effect.warn(
                            "past-source-end",
                            clip_id,
                            format!("slip clamped from {applied} to {last}: the clip needs {consumed} of source"),
                        );
                        applied = last;
                    }
                }
                clip.source_in = applied;
                effect = effect.changed(clip_id);
            }
        }
        Ok(effect)
    }
}

pub struct Slide;

impl Op for Slide {
    fn id(&self) -> &'static str {
        "clip.slide"
    }
    fn about(&self) -> &'static str {
        "Move a clip along the timeline, absorbing the move into the clips on either side"
    }
    fn schema(&self) -> serde_json::Value {
        object_schema(
            serde_json::json!({
                "target": clip_schema(true),
                "by": time_schema("how far to slide, negative to slide earlier"),
                "to": time_schema("absolute timeline start to slide to")
            }),
            &["target"],
        )
    }
    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "by", "to"])?;
        let seq = cx.sequence(project)?;
        let fps = util::sequence_fps(project, &seq)?;
        let mut effect = OpEffect::new();
        let by = snap_arg(&args, "by", fps, &mut effect)?;
        let to = snap_arg(&args, "to", fps, &mut effect)?;
        match (by, to) {
            (Some(_), Some(_)) => {
                return Err(Error::bad_args("pass either 'by' or 'to', not both"))
            }
            (None, None) => return Err(Error::bad_args("clip.slide needs 'by' or 'to'")),
            _ => {}
        }
        if let Some(to) = to {
            assert_on_timeline(to, "to")?;
        }

        let limits = source_limits(project, &seq)?;
        let groups = targets(project, &seq, args::str_field(&args, "target")?, true)?;
        let sequence = project.sequence_mut(&seq)?;
        for (track_id, clips) in &groups {
            let track = sequence.track_mut(track_id)?;
            for clip_id in clips {
                let index = util::clip_index(track, clip_id)?;
                let delta = match (by, to) {
                    (Some(by), _) => by,
                    (_, Some(to)) => to - track.clips[index].start,
                    _ => unreachable!("one of 'by' and 'to' is present"),
                };
                effect.merge(slide_clip(track, clip_id, delta, &limits, fps)?);
            }
        }
        Ok(effect)
    }
}

/// Slide one clip, giving the time it vacates to the neighbour behind it and taking the
/// time it needs from the neighbour in front. Where a neighbour is not adjacent the gap
/// absorbs the move instead, which is why each side is conditional.
fn slide_clip(
    track: &mut Track,
    clip_id: &ClipId,
    delta: Time,
    limits: &Limits,
    fps: Fps,
) -> Result<OpEffect> {
    let index = util::clip_index(track, clip_id)?;
    let original = track.clips[index].span();
    let mut effect = OpEffect::new().changed(clip_id);
    if delta.is_zero() {
        return Ok(effect);
    }
    let moved = original.shifted(delta);
    if moved.start.is_negative() {
        return Err(Error::op(format!(
            "sliding '{}' by {delta} would start it at {}, before the timeline",
            track.clips[index].label(),
            moved.start
        )));
    }

    if let Some(previous) = index.checked_sub(1) {
        let neighbour = &track.clips[previous];
        if neighbour.end() == original.start || neighbour.end() > moved.start {
            if moved.start - neighbour.start < fps.frame_duration() {
                return Err(Error::op(format!(
                    "sliding by {delta} would consume all of '{}'",
                    neighbour.label()
                )));
            }
            if let Some(reach) = out_limit(neighbour, limit_of(limits, &neighbour.id)) {
                if moved.start > reach {
                    return Err(Error::op(format!(
                        "sliding by {delta} needs '{}' to reach {}, but its source ends at {reach}",
                        neighbour.label(),
                        moved.start
                    )));
                }
            }
            trim_out(&mut track.clips[previous], moved.start);
            effect = effect.changed(track.clips[previous].id.clone());
        }
    }

    if index + 1 < track.clips.len() {
        let next = index + 1;
        let neighbour = &track.clips[next];
        if neighbour.start == original.end || neighbour.start < moved.end {
            if neighbour.end() - moved.end < fps.frame_duration() {
                return Err(Error::op(format!(
                    "sliding by {delta} would consume all of '{}'",
                    neighbour.label()
                )));
            }
            if let Some(reach) = in_limit(neighbour, limit_of(limits, &neighbour.id)) {
                if moved.end < reach {
                    return Err(Error::op(format!(
                        "sliding by {delta} needs '{}' to start at {}, but its source begins at {reach}",
                        neighbour.label(),
                        moved.end
                    )));
                }
            }
            trim_in(&mut track.clips[next], moved.end);
            effect = effect.changed(track.clips[next].id.clone());
        }
    }

    track.clips[index].start = moved.start;
    Ok(effect)
}

pub struct MoveClip;

impl Op for MoveClip {
    fn id(&self) -> &'static str {
        "clip.move"
    }
    fn about(&self) -> &'static str {
        "Move a clip to a time, and optionally to another track; refuses an occupied destination unless --overwrite"
    }
    fn schema(&self) -> serde_json::Value {
        object_schema(
            serde_json::json!({
                "target": clip_schema(false),
                "at": time_schema("new timeline start"),
                "track": string_schema("track to move onto; defaults to the clip's own track"),
                "overwrite": bool_schema("trim, split or remove whatever is in the way instead of refusing")
            }),
            &["target", "at"],
        )
    }
    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "at", "track", "overwrite"])?;
        let seq = cx.sequence(project)?;
        let fps = util::sequence_fps(project, &seq)?;
        let overwrite = args::opt_bool(&args, "overwrite")?.unwrap_or(false);
        let mut effect = OpEffect::new();
        let (from_track, clip_id) =
            resolve_one_clip(project, &seq, args::str_field(&args, "target")?)?;
        let at = snap_required(&args, "at", fps, &mut effect)?;
        assert_on_timeline(at, "at")?;
        let to_track = match args::opt_str(&args, "track") {
            Some(text) => resolve_track(project, &seq, text)?,
            None => from_track.clone(),
        };

        // Every clip that moves — the named one and its a/v counterpart — and the delta
        // they share, worked out before the first mutation so a refusal changes nothing.
        let delta;
        let mut moves: Vec<(TrackId, TrackId, ClipId)> = Vec::new();
        {
            let sequence = project.sequence(&seq)?;
            let source = sequence.track(&from_track)?;
            util::assert_unlocked(source)?;
            let destination = sequence.track(&to_track)?;
            util::assert_unlocked(destination)?;
            util::assert_takes_clips(destination)?;
            assert_same_kind(source.kind, destination.kind, &destination.name)?;
            let clip = &source.clips[util::clip_index(source, &clip_id)?];
            delta = at - clip.start;
            moves.push((from_track.clone(), to_track.clone(), clip_id.clone()));
            if let Some(link) = clip.link.clone() {
                if let Some((track, linked)) = sequence.find_clip(&link) {
                    util::assert_unlocked(track)?;
                    moves.push((track.id.clone(), track.id.clone(), linked.id.clone()));
                }
            }
        }
        if delta.is_zero() && from_track == to_track {
            return Ok(effect.changed(&clip_id));
        }

        let sequence = project.sequence_mut(&seq)?;
        let mut removed = Vec::new();
        for (from, to, id) in &moves {
            let index = util::clip_index(sequence.track(from)?, id)?;
            let mut clip = sequence.track_mut(from)?.clips.remove(index);
            clip.start = clip.start + delta;
            // A moved clip's incoming transition belonged to the edge it left behind.
            clip.transition_in = None;
            let span = clip.span();
            let track = sequence.track_mut(to)?;
            if overwrite {
                let cleared = clear_span(track, span);
                for gone in &cleared.removed {
                    effect = effect.removed(gone);
                }
                for touched in &cleared.changed {
                    effect = effect.changed(touched);
                }
                for made in &cleared.created {
                    effect = effect.created(made);
                }
                removed.extend(cleared.removed);
            } else if let Err(error) = util::assert_free(track, span, None) {
                // Put it back: a refused move leaves the document exactly as it was.
                clip.start = clip.start - delta;
                sequence.track_mut(from)?.place(clip);
                return Err(error);
            }
            sequence.track_mut(to)?.place(clip);
            effect = effect.changed(id);
        }
        for id in drop_links_to(sequence, &removed) {
            effect = effect.changed(id);
        }
        Ok(effect)
    }
}

pub struct Speed;

impl Op for Speed {
    fn id(&self) -> &'static str {
        "clip.speed"
    }
    fn about(&self) -> &'static str {
        "Retime a clip: 2/1 plays twice as fast; --preserve-source keeps the source range and recomputes the duration"
    }
    fn schema(&self) -> serde_json::Value {
        object_schema(
            serde_json::json!({
                "target": clip_schema(true),
                "speed": {
                    "type": ["string", "number"],
                    "description": "playback rate as an exact rational: '2/1' twice as fast, '1/2' half speed"
                },
                "reverse": bool_schema("play the source backwards"),
                "preserve-source": bool_schema(
                    "keep the source range and recompute the duration (default); false keeps the duration and consumes more or less source"
                )
            }),
            &["target"],
        )
    }
    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "speed", "reverse", "preserve-source"])?;
        let seq = cx.sequence(project)?;
        let fps = util::sequence_fps(project, &seq)?;
        let speed = args::opt_rat(&args, "speed")?;
        let reverse = args::opt_bool(&args, "reverse")?;
        let preserve = args::opt_bool(&args, "preserve-source")?.unwrap_or(true);
        if speed.is_none() && reverse.is_none() {
            return Err(Error::bad_args("clip.speed needs 'speed' or 'reverse'"));
        }
        if let Some(speed) = speed {
            if speed.as_f64() <= 0.0 {
                return Err(Error::bad_args(format!(
                    "field 'speed': {speed} is not positive; use 'reverse' to play backwards"
                )));
            }
        }

        let limits = source_limits(project, &seq)?;
        let groups = targets(project, &seq, args::str_field(&args, "target")?, true)?;
        let mut effect = OpEffect::new();
        let sequence = project.sequence_mut(&seq)?;
        for (track_id, clips) in &groups {
            let track = sequence.track_mut(track_id)?;
            for clip_id in clips {
                let index = util::clip_index(track, clip_id)?;
                let limit = limit_of(&limits, clip_id);
                let (duration, local) =
                    retimed_duration(&track.clips[index], speed, preserve, limit, fps, clip_id)?;
                // Checked before anything moves: a slowed-down clip grows, and growing into
                // its neighbour has to be refused rather than repaired afterwards.
                let span = Span::from_duration(track.clips[index].start, duration);
                util::assert_free(track, span, Some(clip_id))?;
                let clip = &mut track.clips[index];
                clip.duration = duration;
                clip.speed = speed.unwrap_or(clip.speed);
                if let Some(reverse) = reverse {
                    clip.reverse = reverse;
                }
                effect.merge(local);
                effect = effect.changed(clip_id);
            }
        }
        Ok(effect)
    }
}

/// The duration a retimed clip should have, and what to report about it.
///
/// With `preserve` the source range is the fixed quantity and the duration is arithmetic;
/// without it the duration is fixed and the clip consumes more or less source, which is the
/// case where it can run out and be clamped.
fn retimed_duration(
    clip: &Clip,
    speed: Option<Rat>,
    preserve: bool,
    available: Option<Time>,
    fps: Fps,
    clip_id: &ClipId,
) -> Result<(Time, OpEffect)> {
    let new_speed = speed.unwrap_or(clip.speed);
    let effect = OpEffect::new();
    if preserve {
        let wanted = (clip.duration * clip.speed) / new_speed;
        let duration = floor_frame(wanted, fps);
        if !duration.is_positive() {
            return Err(Error::op(format!(
                "at {new_speed} '{}' would be shorter than one frame",
                clip.label()
            )));
        }
        return Ok((duration, effect.snap("duration", wanted, duration, fps)));
    }
    let Some(total) = available else {
        return Ok((clip.duration, effect));
    };
    let room = total - clip.source_in;
    let wanted = clip.duration * new_speed;
    if wanted <= room {
        return Ok((clip.duration, effect));
    }
    let duration = floor_frame(room / new_speed, fps);
    if !duration.is_positive() {
        return Err(Error::op(format!(
            "at {new_speed} '{}' has no whole frame of source left",
            clip.label()
        )));
    }
    Ok((
        duration,
        effect.warn(
            "past-source-end",
            clip_id,
            format!(
                "at {new_speed} '{}' would need {wanted} of source but {room} is left; it now runs {duration}",
                clip.label()
            ),
        ),
    ))
}

// ---------------------------------------------------------------- pairing and properties

pub struct Link;

impl Op for Link {
    fn id(&self) -> &'static str {
        "clip.link"
    }
    fn about(&self) -> &'static str {
        "Pair a video clip with its audio clip, so trims, moves and retimes apply to both"
    }
    fn schema(&self) -> serde_json::Value {
        object_schema(
            serde_json::json!({
                "target": clip_schema(false),
                "with": clip_schema(false)
            }),
            &["target", "with"],
        )
    }
    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "with"])?;
        let seq = cx.sequence(project)?;
        let (left_track, left) = resolve_one_clip(project, &seq, args::str_field(&args, "target")?)?;
        let (right_track, right) = resolve_one_clip(project, &seq, args::str_field(&args, "with")?)?;
        if left == right {
            return Err(Error::bad_args("a clip cannot be linked to itself"));
        }
        {
            let sequence = project.sequence(&seq)?;
            util::assert_unlocked(sequence.track(&left_track)?)?;
            util::assert_unlocked(sequence.track(&right_track)?)?;
            if left_track == right_track {
                return Err(Error::op(
                    "an a/v pair lives on two tracks; two clips on one track play one after the other, not together",
                ));
            }
            for (track, clip, other) in [(&left_track, &left, &right), (&right_track, &right, &left)]
            {
                let track = sequence.track(track)?;
                let clip = &track.clips[util::clip_index(track, clip)?];
                if let Some(existing) = &clip.link {
                    if existing != other {
                        return Err(Error::op(format!(
                            "'{}' is already linked to {existing}; unlink it first",
                            clip.label()
                        )));
                    }
                }
            }
        }
        let sequence = project.sequence_mut(&seq)?;
        for (track, clip, other) in [(&left_track, &left, &right), (&right_track, &right, &left)] {
            let track = sequence.track_mut(track)?;
            let index = util::clip_index(track, clip)?;
            track.clips[index].link = Some(other.clone());
        }
        Ok(OpEffect::new().changed(&left).changed(&right))
    }
}

pub struct Unlink;

impl Op for Unlink {
    fn id(&self) -> &'static str {
        "clip.unlink"
    }
    fn about(&self) -> &'static str {
        "Break an a/v pairing so the two clips can be edited apart"
    }
    fn schema(&self) -> serde_json::Value {
        object_schema(serde_json::json!({ "target": clip_schema(true) }), &["target"])
    }
    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target"])?;
        let seq = cx.sequence(project)?;
        let groups = targets(project, &seq, args::str_field(&args, "target")?, false)?;
        let mut effect = OpEffect::new();
        let sequence = project.sequence_mut(&seq)?;
        let mut cleared = Vec::new();
        for (track_id, clips) in &groups {
            let track = sequence.track_mut(track_id)?;
            for clip_id in clips {
                let index = util::clip_index(track, clip_id)?;
                if let Some(other) = track.clips[index].link.take() {
                    cleared.push(other);
                    effect = effect.changed(clip_id);
                }
            }
        }
        // The far half of every pair, which lives on a track the selector never named. It
        // is cleared even when that track is locked: a one-sided link is invalid state,
        // and the lock protects a track's timing, not a pointer that now dangles.
        for other in &cleared {
            if let Some((track, index)) = sequence.find_clip_mut(other) {
                if track.clips[index].link.take().is_some() {
                    effect = effect.changed(other);
                }
            }
        }
        Ok(effect)
    }
}

pub struct Rename;

impl Op for Rename {
    fn id(&self) -> &'static str {
        "clip.rename"
    }
    fn about(&self) -> &'static str {
        "Name a clip, so selectors can address it as '#name'"
    }
    fn schema(&self) -> serde_json::Value {
        object_schema(
            serde_json::json!({
                "target": clip_schema(false),
                "name": string_schema("new name, unique within the sequence")
            }),
            &["target", "name"],
        )
    }
    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "name"])?;
        let seq = cx.sequence(project)?;
        let name = args::str_field(&args, "name")?.trim().to_string();
        if name.is_empty() {
            return Err(Error::bad_args("field 'name' cannot be empty"));
        }
        let (track_id, clip_id) = resolve_one_clip(project, &seq, args::str_field(&args, "target")?)?;
        {
            let sequence = project.sequence(&seq)?;
            util::assert_unlocked(sequence.track(&track_id)?)?;
            assert_name_free(sequence, Some(&name), Some(&clip_id))?;
        }
        let track = project.sequence_mut(&seq)?.track_mut(&track_id)?;
        let index = util::clip_index(track, &clip_id)?;
        track.clips[index].name = Some(name);
        Ok(OpEffect::new().changed(&clip_id))
    }
}

pub struct SetTransform;

impl Op for SetTransform {
    fn id(&self) -> &'static str {
        "clip.transform"
    }
    fn about(&self) -> &'static str {
        "Set a clip's position, scale, rotation or anchor"
    }
    fn schema(&self) -> serde_json::Value {
        object_schema(
            serde_json::json!({
                "target": clip_schema(true),
                "pos": { "type": ["string", "array", "number"], "description": "offset from the anchor in sequence pixels, 'x,y'" },
                "scale": { "type": ["string", "array", "number"], "description": "scale factor; one number scales both axes" },
                "rotation": number_schema("clockwise degrees"),
                "anchor": { "type": ["string", "array", "number"], "description": "normalized anchor in the source rect; '0.5,0.5' is its center" }
            }),
            &["target"],
        )
    }
    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "pos", "scale", "rotation", "anchor"])?;
        let seq = cx.sequence(project)?;
        let pos = opt_pair(&args, "pos")?;
        let scale = opt_pair(&args, "scale")?;
        let anchor = opt_pair(&args, "anchor")?;
        let rotation = match args::opt_f64(&args, "rotation")? {
            Some(value) => Some(finite(value, "rotation")?),
            None => None,
        };
        if pos.is_none() && scale.is_none() && anchor.is_none() && rotation.is_none() {
            return Err(Error::bad_args(
                "clip.transform needs one of 'pos', 'scale', 'rotation' or 'anchor'",
            ));
        }
        if let Some(scale) = scale {
            if scale.iter().any(|value| *value <= 0.0) {
                return Err(Error::bad_args(
                    "field 'scale' must be positive; to hide a clip use clip.enable or clip.opacity",
                ));
            }
        }
        let text = args::str_field(&args, "target")?;
        for_each_clip(project, &seq, text, |clip| {
            if let Some(pos) = pos {
                clip.transform.pos = pos;
            }
            if let Some(scale) = scale {
                clip.transform.scale = scale;
            }
            if let Some(anchor) = anchor {
                clip.transform.anchor = anchor;
            }
            if let Some(rotation) = rotation {
                clip.transform.rotation = rotation;
            }
            Ok(())
        })
    }
}

pub struct SetOpacity;

impl Op for SetOpacity {
    fn id(&self) -> &'static str {
        "clip.opacity"
    }
    fn about(&self) -> &'static str {
        "Set a clip's opacity, 0 to 1"
    }
    fn schema(&self) -> serde_json::Value {
        object_schema(
            serde_json::json!({
                "target": clip_schema(true),
                "opacity": number_schema("0 transparent, 1 opaque")
            }),
            &["target", "opacity"],
        )
    }
    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "opacity"])?;
        let seq = cx.sequence(project)?;
        let opacity = args::f64_field(&args, "opacity")?;
        if !(0.0..=1.0).contains(&opacity) {
            return Err(Error::bad_args(format!(
                "field 'opacity': {opacity} is outside 0..1"
            )));
        }
        let opacity = opacity as f32;
        let text = args::str_field(&args, "target")?;
        for_each_clip(project, &seq, text, |clip| {
            clip.opacity = opacity;
            Ok(())
        })
    }
}

pub struct SetBlend;

impl Op for SetBlend {
    fn id(&self) -> &'static str {
        "clip.blend"
    }
    fn about(&self) -> &'static str {
        "Set how a clip composites over the tracks below it"
    }
    fn schema(&self) -> serde_json::Value {
        object_schema(
            serde_json::json!({
                "target": clip_schema(true),
                "blend": {
                    "type": "string",
                    "enum": ["normal", "add", "multiply", "screen", "overlay", "soft-light"],
                    "description": "blend mode"
                }
            }),
            &["target", "blend"],
        )
    }
    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "blend"])?;
        let seq = cx.sequence(project)?;
        let blend: Blend = parse_enum(
            args::str_field(&args, "blend")?,
            "blend",
            "normal, add, multiply, screen, overlay or soft-light",
        )?;
        let text = args::str_field(&args, "target")?;
        for_each_clip(project, &seq, text, |clip| {
            clip.blend = blend;
            Ok(())
        })
    }
}

pub struct SetCrop;

impl Op for SetCrop {
    fn id(&self) -> &'static str {
        "clip.crop"
    }
    fn about(&self) -> &'static str {
        "Crop fractions off a clip's edges"
    }
    fn schema(&self) -> serde_json::Value {
        object_schema(
            serde_json::json!({
                "target": clip_schema(true),
                "left": number_schema("fraction removed from the left edge, 0 to 1"),
                "top": number_schema("fraction removed from the top edge"),
                "right": number_schema("fraction removed from the right edge"),
                "bottom": number_schema("fraction removed from the bottom edge"),
                "clear": bool_schema("remove the crop entirely")
            }),
            &["target"],
        )
    }
    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "left", "top", "right", "bottom", "clear"])?;
        let seq = cx.sequence(project)?;
        let clear = args::opt_bool(&args, "clear")?.unwrap_or(false);
        let mut edges = [None; 4];
        for (slot, key) in edges.iter_mut().zip(["left", "top", "right", "bottom"]) {
            *slot = match args::opt_f64(&args, key)? {
                Some(value) => {
                    if !(0.0..1.0).contains(&value) {
                        return Err(Error::bad_args(format!(
                            "field '{key}': {value} is outside 0..1; a crop is a fraction of the edge"
                        )));
                    }
                    Some(value as f32)
                }
                None => None,
            };
        }
        if clear && edges.iter().any(Option::is_some) {
            return Err(Error::bad_args(
                "'clear' removes the crop, so it cannot be combined with an edge",
            ));
        }
        if !clear && edges.iter().all(Option::is_none) {
            return Err(Error::bad_args(
                "clip.crop needs an edge ('left', 'top', 'right', 'bottom') or 'clear'",
            ));
        }
        let text = args::str_field(&args, "target")?;
        for_each_clip(project, &seq, text, |clip| {
            if clear {
                clip.crop = None;
                return Ok(());
            }
            let mut crop = clip.crop.unwrap_or_default();
            if let Some(value) = edges[0] {
                crop.left = value;
            }
            if let Some(value) = edges[1] {
                crop.top = value;
            }
            if let Some(value) = edges[2] {
                crop.right = value;
            }
            if let Some(value) = edges[3] {
                crop.bottom = value;
            }
            assert_crop_leaves_picture(&crop, clip.label())?;
            clip.crop = Some(crop);
            Ok(())
        })
    }
}

/// Opposite edges that meet leave no pixels at all, which renders as nothing — a mistake
/// this op can see and the compositor can only shrug at.
fn assert_crop_leaves_picture(crop: &Crop, label: &str) -> Result<()> {
    if crop.left + crop.right >= 1.0 {
        return Err(Error::bad_args(format!(
            "cropping '{label}' by {} left and {} right leaves no width",
            crop.left, crop.right
        )));
    }
    if crop.top + crop.bottom >= 1.0 {
        return Err(Error::bad_args(format!(
            "cropping '{label}' by {} top and {} bottom leaves no height",
            crop.top, crop.bottom
        )));
    }
    Ok(())
}

pub struct SetFit;

impl Op for SetFit {
    fn id(&self) -> &'static str {
        "clip.fit"
    }
    fn about(&self) -> &'static str {
        "Set how source pixels map into the sequence frame"
    }
    fn schema(&self) -> serde_json::Value {
        object_schema(
            serde_json::json!({
                "target": clip_schema(true),
                "fit": {
                    "type": "string",
                    "enum": ["contain", "cover", "stretch", "none"],
                    "description": "contain letterboxes, cover crops, stretch ignores aspect, none is pixel for pixel"
                }
            }),
            &["target", "fit"],
        )
    }
    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "fit"])?;
        let seq = cx.sequence(project)?;
        let fit: Fit = parse_enum(
            args::str_field(&args, "fit")?,
            "fit",
            "contain, cover, stretch or none",
        )?;
        let text = args::str_field(&args, "target")?;
        for_each_clip(project, &seq, text, |clip| {
            clip.fit = fit;
            Ok(())
        })
    }
}

pub struct SetEnabled;

impl Op for SetEnabled {
    fn id(&self) -> &'static str {
        "clip.enable"
    }
    fn about(&self) -> &'static str {
        "Enable or disable a clip without removing it"
    }
    fn schema(&self) -> serde_json::Value {
        object_schema(
            serde_json::json!({
                "target": clip_schema(true),
                "enabled": bool_schema("false leaves the clip in place but out of the render")
            }),
            &["target", "enabled"],
        )
    }
    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "enabled"])?;
        let seq = cx.sequence(project)?;
        let enabled = args::opt_bool(&args, "enabled")?
            .ok_or_else(|| Error::bad_args("missing boolean field 'enabled'"))?;
        let text = args::str_field(&args, "target")?;
        for_each_clip(project, &seq, text, |clip| {
            clip.enabled = enabled;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::AssetStore;
    use crate::ids::AssetId;
    use crate::paths::ProjectPaths;
    use crate::project::{Asset, AssetKind, Probe, Source};
    use crate::vfs::{MemVfs, Vfs};
    use std::sync::Arc;

    struct Fixture {
        project: Project,
        paths: ProjectPaths,
        assets: AssetStore,
        registry: Registry,
        seq: SequenceId,
        fps: Fps,
    }

    fn fixture(fps: Fps) -> Fixture {
        let project = Project::new("test", fps, [1920, 1080], 48_000);
        let seq = project.active_sequence.clone();
        let paths = ProjectPaths::new("/p");
        let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
        let assets = AssetStore::new(paths.assets_dir(), vfs);
        let mut registry = Registry::new();
        register(&mut registry);
        Fixture {
            project,
            paths,
            assets,
            registry,
            seq,
            fps,
        }
    }

    impl Fixture {
        fn run(&mut self, id: &str, args: serde_json::Value) -> Result<OpEffect> {
            let op = self.registry.get(id)?.clone();
            let mut cx = OpCx::new(&self.paths, &self.assets);
            op.apply(&mut self.project, args, &mut cx)
        }

        fn add_track(&mut self, name: &str, kind: TrackKind) -> TrackId {
            let mut track = Track::new(name, kind);
            track.id = TrackId::from_raw(format!("trk_{}", name.to_lowercase()));
            let id = track.id.clone();
            self.project
                .sequence_mut(&self.seq)
                .unwrap()
                .tracks
                .push(track);
            id
        }

        /// Media of `secs` seconds, so trims have a real end to run into.
        fn add_asset(&mut self, name: &str, secs: i64, kind: AssetKind) -> AssetId {
            let id = AssetId::from_raw(format!("ast_{name}"));
            self.project.assets.insert(
                id.clone(),
                Asset {
                    id: id.clone(),
                    name: format!("{name}.mp4"),
                    hash: format!("blake3:{name}"),
                    kind,
                    probe: Probe {
                        duration: Time::from_secs(secs),
                        ..Probe::default()
                    },
                    proxy: None,
                    source_path: None,
                    imported: chrono::Utc::now(),
                    provenance: None,
                },
            );
            id
        }

        fn add_clip(
            &mut self,
            track: &TrackId,
            id: &str,
            start: i64,
            duration: i64,
            asset: &AssetId,
        ) -> ClipId {
            let mut clip = Clip::new(
                Source::Asset {
                    asset: asset.clone(),
                    stream: None,
                },
                Time::from_secs(start),
                Time::from_secs(duration),
            );
            clip.id = ClipId::from_raw(id);
            let clip_id = clip.id.clone();
            self.project
                .sequence_mut(&self.seq)
                .unwrap()
                .track_mut(track)
                .unwrap()
                .place(clip);
            clip_id
        }

        fn track_of(&self, track: &TrackId) -> &Track {
            self.project
                .sequence(&self.seq)
                .unwrap()
                .track(track)
                .unwrap()
        }

        fn clip(&self, id: &str) -> &Clip {
            self.project
                .sequence(&self.seq)
                .unwrap()
                .find_clip(&ClipId::from_raw(id))
                .unwrap_or_else(|| panic!("clip '{id}' is gone"))
                .1
        }

        fn duration(&self) -> Time {
            self.project.sequence(&self.seq).unwrap().duration()
        }

        /// `[start, end)` of every clip on a track, in seconds, for comparing layouts.
        fn layout(&self, track: &TrackId) -> Vec<(f64, f64)> {
            self.track_of(track)
                .clips
                .iter()
                .map(|clip| (clip.start.as_secs_f64(), clip.end().as_secs_f64()))
                .collect()
        }

        /// The invariants the engine would reject the transaction over.
        fn assert_tidy(&self, track: &TrackId) {
            let clips = &self.track_of(track).clips;
            for clip in clips {
                assert!(
                    clip.duration.is_positive(),
                    "clip '{}' has duration {}",
                    clip.label(),
                    clip.duration
                );
                // No op may leave an edge between two frames: the renderer would have to
                // invent a partial frame to honour it.
                assert!(
                    clip.start.is_frame_aligned(self.fps) && clip.end().is_frame_aligned(self.fps),
                    "clip '{}' spans {}, which is off the {} grid",
                    clip.label(),
                    clip.span(),
                    self.fps
                );
            }
            for pair in clips.windows(2) {
                assert!(
                    pair[0].end() <= pair[1].start,
                    "'{}' ends at {} but '{}' starts at {}",
                    pair[0].label(),
                    pair[0].end(),
                    pair[1].label(),
                    pair[1].start
                );
            }
        }
    }

    /// Three four-second clips back to back on V1, drawn from a sixty-second take.
    fn three_in_a_row() -> (Fixture, TrackId, AssetId) {
        let mut f = fixture(Fps::new(30, 1).unwrap());
        let track = f.add_track("V1", TrackKind::Video);
        let asset = f.add_asset("talk", 60, AssetKind::Video);
        for (index, start) in [0, 4, 8].iter().enumerate() {
            f.add_clip(&track, &format!("clp_{index}"), *start, 4, &asset);
        }
        (f, track, asset)
    }

    #[test]
    fn insert_ripples_the_later_clips_by_the_inserted_duration() {
        let (mut f, track, _) = three_in_a_row();
        let effect = f
            .run(
                "clip.insert",
                serde_json::json!({
                    "track": "V1",
                    "source": "color:#ff0000",
                    "at": 4,
                    "duration": 2
                }),
            )
            .unwrap();
        assert_eq!(effect.created.len(), 1, "one clip placed, nothing split");
        assert_eq!(
            f.layout(&track),
            vec![(0.0, 4.0), (4.0, 6.0), (6.0, 10.0), (10.0, 14.0)]
        );
        f.assert_tidy(&track);
    }

    #[test]
    fn an_insert_inside_a_clip_splits_it_and_the_halves_stay_contiguous() {
        let (mut f, track, _) = three_in_a_row();
        f.run(
            "clip.insert",
            serde_json::json!({
                "track": "V1",
                "source": "color:#00ff00",
                "at": 6,
                "duration": 1
            }),
        )
        .unwrap();
        assert_eq!(
            f.layout(&track),
            vec![(0.0, 4.0), (4.0, 6.0), (6.0, 7.0), (7.0, 9.0), (9.0, 13.0)]
        );
        // The two halves must still play as one take: the tail resumes where the head left.
        let head = f.track_of(&track).clips[1].clone();
        let tail = f.track_of(&track).clips[3].clone();
        assert_eq!(head.source_span().end, tail.source_in);
        f.assert_tidy(&track);
    }

    #[test]
    fn overwrite_splits_what_it_lands_inside_and_removes_what_it_covers() {
        let (mut f, track, _) = three_in_a_row();
        let effect = f
            .run(
                "clip.overwrite",
                serde_json::json!({
                    "track": "V1",
                    "source": "color:#0000ff",
                    "at": 1,
                    "duration": 2
                }),
            )
            .unwrap();
        assert_eq!(
            f.layout(&track),
            vec![(0.0, 1.0), (1.0, 3.0), (3.0, 4.0), (4.0, 8.0), (8.0, 12.0)],
            "an overwrite never moves anything"
        );
        assert_eq!(f.track_of(&track).clips[0].source_in, Time::ZERO);
        assert_eq!(
            f.track_of(&track).clips[2].source_in,
            Time::from_secs(3),
            "the tail resumes three seconds into the take, not at zero"
        );
        assert_eq!(effect.created.len(), 2, "the new clip and the split-off tail");

        let effect = f
            .run(
                "clip.overwrite",
                serde_json::json!({
                    "track": "V1",
                    "source": "color:#0000ff",
                    "at": 4,
                    "duration": 4
                }),
            )
            .unwrap();
        assert_eq!(
            effect.removed,
            vec!["clp_1".to_string()],
            "a fully covered clip is removed, not trimmed to nothing"
        );
        f.assert_tidy(&track);
    }

    #[test]
    fn remove_leaves_a_gap_unless_it_ripples() {
        let (mut f, track, _) = three_in_a_row();
        f.run("clip.remove", serde_json::json!({ "target": "clp_1" }))
            .unwrap();
        assert_eq!(f.layout(&track), vec![(0.0, 4.0), (8.0, 12.0)]);

        let (mut f, track, _) = three_in_a_row();
        f.run(
            "clip.remove",
            serde_json::json!({ "target": "clp_1", "ripple": true }),
        )
        .unwrap();
        assert_eq!(f.layout(&track), vec![(0.0, 4.0), (4.0, 8.0)]);
        f.assert_tidy(&track);
    }

    #[test]
    fn split_halves_sum_to_the_original_and_share_the_source() {
        let (mut f, track, _) = three_in_a_row();
        let before = f.clip("clp_1").clone();
        let effect = f
            .run(
                "clip.split",
                serde_json::json!({ "target": "clp_1", "at": "00:05.5" }),
            )
            .unwrap();
        let tail_id = effect.created.first().expect("the split creates a clip").clone();
        let head = f.clip("clp_1").clone();
        let tail = f.clip(&tail_id).clone();
        assert_eq!(head.duration + tail.duration, before.duration);
        assert_eq!(head.start, before.start);
        assert_eq!(tail.end(), before.end());
        assert_eq!(head.source_in, before.source_in);
        assert_eq!(
            head.source_span().end,
            tail.source_in,
            "the halves have to be contiguous in the source"
        );
        assert_eq!(tail.source_span().end, before.source_span().end);
        f.assert_tidy(&track);
    }

    #[test]
    fn splitting_on_a_boundary_is_refused_and_names_the_edge() {
        let (mut f, track, _) = three_in_a_row();
        let error = f
            .run("clip.split", serde_json::json!({ "target": "clp_1", "at": 4 }))
            .unwrap_err();
        assert!(
            error.to_string().contains("boundary"),
            "the error should name the boundary, got: {error}"
        );
        assert_eq!(
            f.track_of(&track).clips.len(),
            3,
            "a refused split creates nothing"
        );
    }

    #[test]
    fn trimming_out_past_the_end_of_the_source_is_clamped_and_warned() {
        let mut f = fixture(Fps::new(30, 1).unwrap());
        let track = f.add_track("V1", TrackKind::Video);
        let asset = f.add_asset("short", 6, AssetKind::Video);
        f.add_clip(&track, "clp_0", 0, 4, &asset);
        let effect = f
            .run("clip.trim", serde_json::json!({ "target": "clp_0", "out": 20 }))
            .unwrap();
        let codes: Vec<&str> = effect.warnings.iter().map(|w| w.code).collect();
        assert_eq!(codes, vec!["past-source-end"]);
        assert_eq!(
            f.clip("clp_0").end(),
            Time::from_secs(6),
            "the clip stops where the media does"
        );
        assert_eq!(f.clip("clp_0").source_span().end, Time::from_secs(6));
    }

    #[test]
    fn a_roll_moves_the_shared_edge_and_keeps_the_timeline_length() {
        let (mut f, track, _) = three_in_a_row();
        let before = f.duration();
        f.run(
            "clip.trim",
            serde_json::json!({ "target": "clp_0", "out": 6, "mode": "roll" }),
        )
        .unwrap();
        assert_eq!(f.layout(&track), vec![(0.0, 6.0), (6.0, 8.0), (8.0, 12.0)]);
        assert_eq!(
            f.clip("clp_1").source_in,
            Time::from_secs(2),
            "the neighbour gave up its first two seconds of content"
        );
        assert_eq!(f.duration(), before, "a roll changes no overall length");
        f.assert_tidy(&track);
    }

    #[test]
    fn a_ripple_trim_closes_the_timeline_behind_it() {
        let (mut f, track, _) = three_in_a_row();
        f.run(
            "clip.trim",
            serde_json::json!({ "target": "clp_0", "out": 2, "mode": "ripple" }),
        )
        .unwrap();
        assert_eq!(
            f.layout(&track),
            vec![(0.0, 2.0), (2.0, 6.0), (6.0, 10.0)],
            "shortening a clip in ripple mode pulls the rest of the track up"
        );
        f.assert_tidy(&track);
    }

    #[test]
    fn speed_with_preserve_source_halves_the_duration_and_keeps_the_source_range() {
        let (mut f, track, _) = three_in_a_row();
        let before = f.clip("clp_0").source_span();
        f.run(
            "clip.speed",
            serde_json::json!({ "target": "clp_0", "speed": "2/1" }),
        )
        .unwrap();
        let after = f.clip("clp_0").clone();
        assert_eq!(after.duration, Time::from_secs(2));
        assert_eq!(
            after.source_span(),
            before,
            "at twice the rate the same source plays in half the time"
        );
        f.assert_tidy(&track);
    }

    #[test]
    fn slowing_a_clip_into_its_neighbour_is_refused() {
        let (mut f, track, _) = three_in_a_row();
        let error = f
            .run(
                "clip.speed",
                serde_json::json!({ "target": "clp_0", "speed": "1/2" }),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("clp_1"),
            "the refusal should name the clip in the way, got: {error}"
        );
        assert_eq!(f.layout(&track), vec![(0.0, 4.0), (4.0, 8.0), (8.0, 12.0)]);
    }

    #[test]
    fn speed_without_preserve_source_keeps_the_duration_and_clamps_at_the_end() {
        let mut f = fixture(Fps::new(30, 1).unwrap());
        let track = f.add_track("V1", TrackKind::Video);
        let asset = f.add_asset("short", 6, AssetKind::Video);
        f.add_clip(&track, "clp_0", 0, 4, &asset);
        let effect = f
            .run(
                "clip.speed",
                serde_json::json!({ "target": "clp_0", "speed": "2/1", "preserve-source": false }),
            )
            .unwrap();
        let clip = f.clip("clp_0").clone();
        assert_eq!(
            clip.duration,
            Time::from_secs(3),
            "4 s at double rate would need 8 s of a 6 s take, so it runs 3 s"
        );
        assert_eq!(clip.source_span().end, Time::from_secs(6));
        assert_eq!(
            effect.warnings.iter().map(|w| w.code).collect::<Vec<_>>(),
            vec!["past-source-end"]
        );
    }

    #[test]
    fn a_trim_on_a_linked_pair_changes_both_halves() {
        let mut f = fixture(Fps::new(30, 1).unwrap());
        let video = f.add_track("V1", TrackKind::Video);
        let audio = f.add_track("A1", TrackKind::Audio);
        let shot = f.add_asset("talk", 60, AssetKind::Video);
        let sound = f.add_asset("talk-audio", 60, AssetKind::Audio);
        f.add_clip(&video, "clp_v", 0, 4, &shot);
        f.add_clip(&audio, "clp_a", 0, 4, &sound);
        f.run(
            "clip.link",
            serde_json::json!({ "target": "clp_v", "with": "clp_a" }),
        )
        .unwrap();

        let effect = f
            .run("clip.trim", serde_json::json!({ "target": "clp_v", "out": 3 }))
            .unwrap();
        assert_eq!(f.clip("clp_v").duration, Time::from_secs(3));
        assert_eq!(
            f.clip("clp_a").duration,
            Time::from_secs(3),
            "the audio has to follow the picture or they drift"
        );
        assert!(
            effect.changed.contains(&"clp_a".to_string()),
            "both ids belong in the report, got {:?}",
            effect.changed
        );
        f.assert_tidy(&video);
        f.assert_tidy(&audio);
    }

    #[test]
    fn unlinking_clears_both_halves_of_the_pair() {
        let mut f = fixture(Fps::new(30, 1).unwrap());
        let video = f.add_track("V1", TrackKind::Video);
        let audio = f.add_track("A1", TrackKind::Audio);
        let shot = f.add_asset("talk", 60, AssetKind::Video);
        f.add_clip(&video, "clp_v", 0, 4, &shot);
        f.add_clip(&audio, "clp_a", 0, 4, &shot);
        f.run(
            "clip.link",
            serde_json::json!({ "target": "clp_v", "with": "clp_a" }),
        )
        .unwrap();
        f.run("clip.unlink", serde_json::json!({ "target": "clp_v" }))
            .unwrap();
        assert!(f.clip("clp_v").link.is_none());
        assert!(
            f.clip("clp_a").link.is_none(),
            "a one-sided link is what the av-drift lint reports"
        );
    }

    #[test]
    fn an_edit_to_a_locked_track_is_refused() {
        let (mut f, track, _) = three_in_a_row();
        f.project
            .sequence_mut(&f.seq)
            .unwrap()
            .track_mut(&track)
            .unwrap()
            .locked = true;
        let error = f
            .run("clip.trim", serde_json::json!({ "target": "clp_0", "out": 2 }))
            .unwrap_err();
        assert!(
            error.to_string().contains("locked"),
            "got: {error}"
        );
        assert_eq!(f.clip("clp_0").duration, Time::from_secs(4));

        let error = f
            .run(
                "clip.insert",
                serde_json::json!({ "track": "V1", "source": "bars", "at": 0, "duration": 1 }),
            )
            .unwrap_err();
        assert!(error.to_string().contains("locked"), "got: {error}");
        assert_eq!(f.track_of(&track).clips.len(), 3);
    }

    #[test]
    fn a_time_argument_lands_on_the_frame_grid_and_the_snap_is_reported() {
        let fps = Fps::new(30_000, 1001).unwrap();
        let mut f = fixture(fps);
        f.add_track("V1", TrackKind::Video);
        let effect = f
            .run(
                "clip.insert",
                serde_json::json!({
                    "track": "V1",
                    "source": "color:#ffffff",
                    "at": 42.5,
                    "duration": 2
                }),
            )
            .unwrap();
        let snap = effect
            .snapped
            .iter()
            .find(|snap| snap.field == "at")
            .expect("a request off the grid must be reported");
        assert_eq!(snap.frame, 1274);
        assert_eq!(snap.requested, "85/2");
        assert_eq!(snap.applied, Time::from_frames(1274, fps).to_string());

        let placed = f.clip(effect.created.first().unwrap());
        assert!(
            placed.start.is_frame_aligned(fps),
            "the clip landed at {}, which is not on a frame",
            placed.start
        );
        assert_eq!(placed.start.frame_round(fps), 1274);
    }

    #[test]
    fn a_typo_in_an_argument_name_is_an_error_not_a_silent_no_op() {
        let (mut f, _, _) = three_in_a_row();
        let error = f
            .run(
                "clip.trim",
                serde_json::json!({ "target": "clp_0", "outt": 2 }),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("unknown argument"),
            "got: {error}"
        );
        assert_eq!(f.clip("clp_0").duration, Time::from_secs(4));
    }

    #[test]
    fn move_refuses_an_occupied_destination_unless_told_to_overwrite() {
        let (mut f, track, _) = three_in_a_row();
        let error = f
            .run("clip.move", serde_json::json!({ "target": "clp_0", "at": 4 }))
            .unwrap_err();
        assert!(error.to_string().contains("occupies"), "got: {error}");
        assert_eq!(
            f.layout(&track),
            vec![(0.0, 4.0), (4.0, 8.0), (8.0, 12.0)],
            "a refused move puts the clip back where it was"
        );

        let effect = f
            .run(
                "clip.move",
                serde_json::json!({ "target": "clp_0", "at": 4, "overwrite": true }),
            )
            .unwrap();
        assert_eq!(effect.removed, vec!["clp_1".to_string()]);
        assert_eq!(f.layout(&track), vec![(4.0, 8.0), (8.0, 12.0)]);
        f.assert_tidy(&track);
    }

    #[test]
    fn placing_an_asset_without_a_duration_uses_what_is_left_after_the_source_in() {
        let mut f = fixture(Fps::new(30, 1).unwrap());
        let track = f.add_track("V1", TrackKind::Video);
        f.add_asset("short", 10, AssetKind::Video);
        f.run(
            "clip.append",
            serde_json::json!({ "track": "V1", "source": "short", "source-in": 8, "name": "tail" }),
        )
        .unwrap();
        let clip = f.track_of(&track).clips[0].clone();
        assert_eq!(clip.duration, Time::from_secs(2));
        assert_eq!(clip.source_span().end, Time::from_secs(10));

        // A second append lands after the first rather than on top of it.
        f.run(
            "clip.append",
            serde_json::json!({ "track": "V1", "source": "short", "duration": 1 }),
        )
        .unwrap();
        assert_eq!(f.layout(&track), vec![(0.0, 2.0), (2.0, 3.0)]);
    }

    #[test]
    fn slide_absorbs_the_move_into_both_neighbours() {
        let (mut f, track, _) = three_in_a_row();
        let before = f.duration();
        f.run(
            "clip.slide",
            serde_json::json!({ "target": "clp_1", "by": 1 }),
        )
        .unwrap();
        assert_eq!(f.layout(&track), vec![(0.0, 5.0), (5.0, 9.0), (9.0, 12.0)]);
        assert_eq!(f.clip("clp_1").source_in, Time::ZERO, "a slide keeps its own content");
        assert_eq!(
            f.clip("clp_2").source_in,
            Time::from_secs(1),
            "the clip in front gave up its first second"
        );
        assert_eq!(f.duration(), before);
        f.assert_tidy(&track);
    }

    #[test]
    fn a_reversed_clip_trimmed_at_the_tail_keeps_its_first_frame() {
        let mut f = fixture(Fps::new(30, 1).unwrap());
        let track = f.add_track("V1", TrackKind::Video);
        let asset = f.add_asset("talk", 60, AssetKind::Video);
        f.add_clip(&track, "clp_0", 0, 4, &asset);
        f.run("clip.slip", serde_json::json!({ "target": "clp_0", "by": 10 }))
            .unwrap();
        f.run(
            "clip.speed",
            serde_json::json!({ "target": "clp_0", "reverse": true }),
        )
        .unwrap();
        let first_frame = {
            let clip = f.clip("clp_0");
            clip.source_time(clip.start)
        };
        assert_eq!(first_frame, Time::from_secs(14));

        f.run("clip.trim", serde_json::json!({ "target": "clp_0", "out": 6 }))
            .unwrap();
        let clip = f.clip("clp_0").clone();
        assert_eq!(
            clip.source_time(clip.start),
            first_frame,
            "extending the tail of a reversed clip reaches further back, not forward"
        );
        assert_eq!(clip.source_in, Time::from_secs(8));
        assert_eq!(clip.source_span().end, Time::from_secs(14));
    }

    #[test]
    fn properties_apply_to_every_clip_a_selector_names() {
        let (mut f, track, _) = three_in_a_row();
        let effect = f
            .run(
                "clip.opacity",
                serde_json::json!({ "target": "clip[track=V1]", "opacity": 0.5 }),
            )
            .unwrap();
        assert_eq!(effect.changed.len(), 3, "every match belongs in the report");
        assert!(f
            .track_of(&track)
            .clips
            .iter()
            .all(|clip| clip.opacity == 0.5));

        let error = f
            .run(
                "clip.opacity",
                serde_json::json!({ "target": "clp_0", "opacity": 1.5 }),
            )
            .unwrap_err();
        assert!(error.to_string().contains("0..1"), "got: {error}");
        assert_eq!(f.clip("clp_0").opacity, 0.5);
    }

    #[test]
    fn a_clip_op_pointed_at_a_caption_track_says_what_to_use_instead() {
        let mut f = fixture(Fps::new(30, 1).unwrap());
        f.add_track("CC1", TrackKind::Caption);
        let error = f
            .run(
                "clip.insert",
                serde_json::json!({ "track": "CC1", "source": "bars", "at": 0, "duration": 1 }),
            )
            .unwrap_err();
        assert!(error.to_string().contains("caption"), "got: {error}");
    }
}
