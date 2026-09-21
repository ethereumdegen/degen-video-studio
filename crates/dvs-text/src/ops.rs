//! The ops that make a transcript an editing surface.
//!
//! `transcript.find`, `transcript.cut-words` and `transcript.keep-phrases` are the reason
//! this crate exists: they let an agent edit by meaning ("drop every 'um'", "keep the
//! sentence about pricing") and get back a report of exactly which ranges went, in a
//! document where positions are frame-exact. Each one resolves words through the clip that
//! shows them, snaps the resulting ranges to the sequence frame grid, ripples the track
//! closed so no black hole is left behind, and applies the identical cut to the linked
//! audio or video clip — an a/v pair that drifted by two frames is the classic silent
//! failure of this kind of edit.
//!
//! The `caption.*` ops share the same words. Generation reports the reading rate it
//! achieved (`maxCps`) rather than asserting the captions are fine, because the operator
//! cannot look at them.

use crate::captions;
use crate::transcript::{self, Transcript, Word, DEFAULT_FILLERS};
use crate::whisper::{self, RunSpec};
use dvs_core::error::{Error, Result};
use dvs_core::ids::{AssetId, ClipId, StyleId, TrackId};
use dvs_core::op::{args, Op, OpCx, OpEffect, Registry};
use dvs_core::ops::util::{assert_unlocked, ripple_after, sequence_fps};
use dvs_core::project::{
    CaptionCue, CaptionPosition, CaptionStyle, Clip, Project, Track, TrackKind,
};
use dvs_core::selector;
use dvs_core::time::{Fps, Span, Time};
use dvs_core::Rgba;
use dvs_core::SequenceId;
use dvs_interop::{from_srt, from_vtt, to_srt, to_vtt};
use dvs_media::toolchain::Toolchain;
use std::collections::BTreeMap;
use std::path::Path;

pub fn register(registry: &mut Registry) {
    registry.register(TranscriptRun);
    registry.register(TranscriptImport);
    registry.register(TranscriptFind);
    registry.register(TranscriptCutWords);
    registry.register(TranscriptKeepPhrases);
    registry.register(CaptionGenerate);
    registry.register(CaptionImport);
    registry.register(CaptionExport);
    registry.register(CaptionSetStyle);
    registry.register(CaptionRemove);
}

/// Default merge distance for filler cuts: fillers closer together than a quarter second
/// are one stumble, not two.
fn default_min_gap() -> Time {
    Time::new(1, 4).expect("1/4 is valid")
}

/// A span reported to an agent: exact for arithmetic, timecode and clock for reading.
fn span_json(span: Span, fps: Fps) -> serde_json::Value {
    serde_json::json!({
        "start": span.start.to_string(),
        "end": span.end.to_string(),
        "duration": span.duration().to_string(),
        "timecode": span.start.timecode(fps),
        "clock": span.start.clock(),
    })
}

/// The asset behind a clip, or an error naming why the clip has no words.
fn clip_asset(clip: &Clip) -> Result<AssetId> {
    clip.source.asset_id().cloned().ok_or_else(|| {
        Error::op(format!(
            "clip '{}' plays {} and has no transcript; transcripts belong to media assets",
            clip.label(),
            clip.source.describe()
        ))
    })
}

/// Clips in a sequence whose source is this asset, in track then timeline order.
fn clips_using(project: &Project, seq: &SequenceId, asset: &AssetId) -> Result<Vec<(TrackId, ClipId)>> {
    let sequence = project.sequence(seq)?;
    let mut out = Vec::new();
    for track in &sequence.tracks {
        for clip in &track.clips {
            if clip.source.asset_id() == Some(asset) {
                out.push((track.id.clone(), clip.id.clone()));
            }
        }
    }
    Ok(out)
}

/// `--target` if given, else every clip using `--asset`. One of the two is required: an op
/// that silently captioned the whole timeline because no target was named would be a very
/// expensive surprise.
fn target_clips(
    project: &Project,
    seq: &SequenceId,
    args: &serde_json::Value,
    op: &str,
) -> Result<Vec<(TrackId, ClipId)>> {
    if let Some(text) = args::opt_str(args, "target") {
        return selector::resolve_clips(project, seq, text);
    }
    let Some(query) = args::opt_str(args, "asset") else {
        return Err(Error::bad_args(format!(
            "{op} needs '--target <selector>' or '--asset <id|name>'"
        )));
    };
    let asset = project.resolve_asset(query)?;
    let clips = clips_using(project, seq, &asset)?;
    if clips.is_empty() {
        let sequence = project.sequence(seq)?;
        return Err(Error::no_match(
            "clip",
            format!("clip[source={query}]"),
            sequence.clip_ids(),
        ));
    }
    Ok(clips)
}

/// Load each transcript a set of clips needs, once per asset.
fn transcripts_for(
    project: &Project,
    seq: &SequenceId,
    clips: &[(TrackId, ClipId)],
    paths: &dvs_core::paths::ProjectPaths,
) -> Result<BTreeMap<AssetId, Transcript>> {
    let sequence = project.sequence(seq)?;
    let mut out = BTreeMap::new();
    for (_, clip_id) in clips {
        let (_, clip) = sequence
            .find_clip(clip_id)
            .ok_or_else(|| Error::no_match("clip", clip_id.as_str(), sequence.clip_ids()))?;
        let asset = clip_asset(clip)?;
        if !out.contains_key(&asset) {
            out.insert(asset.clone(), Transcript::load(paths, &asset)?);
        }
    }
    Ok(out)
}

/// A clip's words, in timeline time, as a transcript so the phrase and filler search can be
/// reused unchanged on either time base.
fn timeline_view(source: &Transcript, clip: &Clip) -> Transcript {
    Transcript::new(
        source.asset.clone(),
        source.language.clone(),
        source.model.clone(),
        source.words_for_clip(clip),
    )
}

/// Reduce a clip to a sub-range of its current span, keeping the same source material under
/// the frames that survive.
///
/// Mirrors `seq.nest`'s trim, including the reverse rule: for a reversed clip it is the
/// *tail* trim that advances `source_in`, because the span is read backwards. The two have
/// to agree — a transcript-driven cut and a structural edit that disagreed about which
/// frames a trimmed clip shows would put the words out of sync with the picture.
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

/// What a ripple delete did to one track.
#[derive(Debug, Default)]
struct Deleted {
    removed: Vec<ClipId>,
    created: Vec<ClipId>,
    changed: Vec<ClipId>,
}

/// Remove a timeline range from a track and close the hole behind it.
///
/// A range in the middle of a clip splits it, which is why this returns created ids: the
/// tail is a new clip, and an agent that asked to cut three "um"s needs to be able to
/// address the four pieces it now has. Everything at or after the range moves left by its
/// duration, so no gap is produced — a gap on a video track renders as black.
fn ripple_delete(track: &mut Track, span: Span) -> Deleted {
    let mut out = Deleted::default();
    if span.is_empty() {
        return out;
    }
    let mut keep: Vec<Clip> = Vec::with_capacity(track.clips.len() + 1);
    for clip in std::mem::take(&mut track.clips) {
        let Some(overlap) = clip.span().intersect(&span) else {
            keep.push(clip);
            continue;
        };
        let has_head = clip.start < overlap.start;
        let has_tail = clip.end() > overlap.end;
        if !has_head && !has_tail {
            out.removed.push(clip.id.clone());
            continue;
        }
        if has_head {
            let mut head = clip.clone();
            trim_to(&mut head, Span::new(clip.start, overlap.start));
            out.changed.push(head.id.clone());
            keep.push(head);
        }
        if has_tail {
            let mut tail = clip.clone();
            if has_head {
                tail.id = ClipId::new();
                tail.link = None;
                out.created.push(tail.id.clone());
            } else {
                out.changed.push(tail.id.clone());
            }
            trim_to(&mut tail, Span::new(overlap.end, clip.end()));
            keep.push(tail);
        }
    }
    track.clips = keep;
    ripple_after(track, span.end, -span.duration());
    out
}

/// The tracks a clip-level ripple must touch: the clip's own, plus its a/v counterpart's.
/// Cutting one and not the other is how linked audio and video drift.
fn linked_tracks(project: &Project, seq: &SequenceId, track: &TrackId, clip: &ClipId) -> Result<Vec<TrackId>> {
    let sequence = project.sequence(seq)?;
    let (_, found) = sequence
        .find_clip(clip)
        .ok_or_else(|| Error::no_match("clip", clip.as_str(), sequence.clip_ids()))?;
    let mut tracks = vec![track.clone()];
    if let Some(link) = &found.link {
        if let Some((other, _)) = sequence.find_clip(link) {
            if !tracks.contains(&other.id) {
                tracks.push(other.id.clone());
            }
        }
    }
    for id in &tracks {
        assert_unlocked(sequence.track(id)?)?;
    }
    Ok(tracks)
}

/// Apply ripple deletes back to front, so a span's coordinates are never invalidated by an
/// earlier edit.
fn apply_cuts(
    project: &mut Project,
    seq: &SequenceId,
    tracks: &[TrackId],
    spans: &[Span],
) -> Result<Deleted> {
    let mut total = Deleted::default();
    let sequence = project.sequence_mut(seq)?;
    for span in spans.iter().rev() {
        let mut fresh: Vec<Vec<ClipId>> = Vec::with_capacity(tracks.len());
        for id in tracks {
            let track = sequence.track_mut(id)?;
            let done = ripple_delete(track, *span);
            total.removed.extend(done.removed);
            total.changed.extend(done.changed);
            fresh.push(done.created.clone());
            total.created.extend(done.created);
        }
        // A cut through the middle of a linked pair leaves a new tail on each track,
        // covering the identical timeline range. Pairing them keeps the link intact, so a
        // later move or trim still carries both halves; leaving them unlinked is how the
        // `av-drift` lint starts firing three edits later.
        if let [first, second] = fresh.as_slice() {
            if let ([left], [right]) = (first.as_slice(), second.as_slice()) {
                let (left, right) = (left.clone(), right.clone());
                if let Some((track, index)) = sequence.find_clip_mut(&left) {
                    track.clips[index].link = Some(right.clone());
                }
                if let Some((track, index)) = sequence.find_clip_mut(&right) {
                    track.clips[index].link = Some(left);
                }
            }
        }
    }
    // A clip trimmed twice reports as changed twice; the ids an agent reads back should be
    // a set.
    total.changed.sort();
    total.changed.dedup();
    total.changed.retain(|id| !total.removed.contains(id));
    Ok(total)
}

/// Caption cues the sequence still holds after a ripple would be out of sync with the
/// picture, and `ripple_after` deliberately leaves them alone. Say so rather than letting
/// an agent discover it in a render.
fn caption_drift(project: &Project, seq: &SequenceId, from: Time) -> Option<(String, usize)> {
    let sequence = project.sequence(seq).ok()?;
    let stale: usize = sequence
        .tracks
        .iter()
        .filter(|track| track.kind == TrackKind::Caption)
        .map(|track| track.cues.iter().filter(|cue| cue.span.end > from).count())
        .sum();
    (stale > 0).then(|| (sequence.id.to_string(), stale))
}

/// Pad, clamp to the clip, snap to the frame grid, and merge what the padding joined.
fn prepare_cuts(spans: Vec<Span>, pad: Time, within: Span, fps: Fps) -> Vec<Span> {
    let padded: Vec<Span> = spans
        .into_iter()
        .filter_map(|span| {
            let grown = Span::new(span.start - pad, span.end + pad);
            let clamped = grown.intersect(&within)?;
            let snapped = Span::new(clamped.start.snap(fps), clamped.end.snap(fps));
            (!snapped.is_empty()).then_some(snapped)
        })
        .collect();
    transcript::merge_spans(padded, Time::ZERO)
}

/// The ranges of `whole` that `keep` does not cover.
fn complement(whole: Span, keep: &[Span]) -> Vec<Span> {
    let mut out = Vec::new();
    let mut cursor = whole.start;
    for span in keep {
        if span.start > cursor {
            out.push(Span::new(cursor, span.start));
        }
        cursor = cursor.max(span.end);
    }
    if cursor < whole.end {
        out.push(Span::new(cursor, whole.end));
    }
    out
}

/// A caption track: the one named, else the first there is, else a new one.
fn caption_track(
    project: &mut Project,
    seq: &SequenceId,
    requested: Option<&str>,
) -> Result<(TrackId, bool)> {
    if let Some(text) = requested {
        let id = selector::resolve_track(project, seq, text)?;
        let track = project.sequence(seq)?.track(&id)?;
        if track.kind != TrackKind::Caption {
            return Err(Error::bad_args(format!(
                "track '{}' holds {} clips; captions live on a caption track (add one with \
                 track.add --kind caption)",
                track.name,
                format!("{:?}", track.kind).to_lowercase()
            )));
        }
        assert_unlocked(track)?;
        return Ok((id, false));
    }
    if let Some(track) = project
        .sequence(seq)?
        .tracks
        .iter()
        .find(|track| track.kind == TrackKind::Caption)
    {
        assert_unlocked(track)?;
        return Ok((track.id.clone(), false));
    }
    let sequence = project.sequence_mut(seq)?;
    let name = sequence.next_track_name(TrackKind::Caption);
    let track = Track::new(name, TrackKind::Caption);
    let id = track.id.clone();
    sequence.tracks.push(track);
    Ok((id, true))
}

/// A named style, or an error listing the styles that exist.
fn resolve_style(project: &Project, query: &str) -> Result<StyleId> {
    let direct = StyleId::from_raw(query);
    if project.styles.contains_key(&direct) {
        return Ok(direct);
    }
    let matches: Vec<&CaptionStyle> = project
        .styles
        .values()
        .filter(|style| style.name.eq_ignore_ascii_case(query))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => Err(Error::no_match(
            "style",
            query,
            project.styles.values().map(|s| s.name.clone()).collect(),
        )),
        many => Err(Error::bad_args(format!(
            "style name '{query}' is ambiguous ({} matches); use an id",
            many.len()
        ))),
    }
}

/// The style captions use when nobody chose one, creating it on first use.
fn default_style(project: &mut Project) -> (StyleId, bool) {
    if let Some(style) = project
        .styles
        .values()
        .find(|style| style.name == "default")
    {
        return (style.id.clone(), false);
    }
    let style = CaptionStyle::named("default");
    let id = style.id.clone();
    project.styles.insert(id.clone(), style);
    (id, true)
}

/// Place cues on a track, dropping the ones the new range replaces.
///
/// Replacing rather than appending makes regeneration idempotent: running
/// `caption.generate` twice produces one set of captions, not two stacked on top of each
/// other, which is unrenderable and invisible until someone watches the output.
fn place_cues(track: &mut Track, cues: Vec<CaptionCue>) -> Vec<String> {
    let mut removed = Vec::new();
    if let (Some(first), Some(last)) = (cues.first(), cues.last()) {
        let range = Span::new(first.span.start, last.span.end);
        track.cues.retain(|cue| {
            if cue.span.overlaps(&range) {
                removed.push(cue.id.to_string());
                false
            } else {
                true
            }
        });
    }
    track.cues.extend(cues);
    track.cues.sort_by(|a, b| a.span.start.cmp(&b.span.start));
    removed
}

/// Snap a cue's span to the frame grid, so a caption turns on when a frame does. A span
/// that would collapse to nothing keeps its exact times: dropping a cue to satisfy the
/// grid would lose text.
fn snap_cue(cue: &mut CaptionCue, fps: Fps) -> bool {
    let snapped = Span::new(cue.span.start.snap(fps), cue.span.end.snap(fps));
    if snapped.is_empty() || snapped == cue.span {
        return false;
    }
    cue.span = snapped;
    true
}

/// The highest reading rate among a set of cues, which is the number the `caption-too-fast`
/// lint and the digest both report.
fn max_cps(cues: &[CaptionCue]) -> f64 {
    cues.iter()
        .map(CaptionCue::chars_per_second)
        .filter(|rate| rate.is_finite())
        .fold(0.0, f64::max)
}

struct TranscriptRun;

impl Op for TranscriptRun {
    fn id(&self) -> &'static str {
        "transcript.run"
    }

    fn about(&self) -> &'static str {
        "Transcribe an asset's audio into word timestamps with whisper"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["asset"],
            "properties": {
                "asset": { "type": "string", "description": "Asset id, name or file stem" },
                "model": {
                    "type": "string",
                    "description": "Path to GGML weights, or a whisper.cpp model name; \
                                    defaults to $DVS_WHISPER_MODEL then base.en in ~/.cache/dvs/models",
                    "examples": ["base.en", "large-v3", "/opt/models/ggml-small.en.bin"]
                },
                "language": {
                    "type": "string",
                    "description": "Spoken language code; omit to let the model detect it",
                    "examples": ["en", "de"]
                }
            },
            "additionalProperties": false
        })
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        &["ffmpeg", "whisper"]
    }

    fn apply(&self, project: &mut Project, args: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&args, &["asset", "model", "language"])?;
        let asset_id = project.resolve_asset(args::str_field(&args, "asset")?)?;
        let asset = project.asset(&asset_id)?;
        if asset.probe.audio.is_none() {
            return Err(Error::op(format!(
                "asset '{}' has no audio stream to transcribe",
                asset.name
            )));
        }
        let spec = RunSpec {
            model: args::opt_str(&args, "model"),
            language: args::opt_str(&args, "language"),
            duration: asset.probe.duration,
        };
        let media = cx.assets.find(&asset.hash)?;
        let tool = Toolchain::shared()?;

        if cx.dry_run {
            // A dry run must not claim a transcription that this build cannot perform, so
            // the model is resolved (and the missing-feature error raised) even here.
            if !whisper::AVAILABLE {
                return whisper::run(tool, &media, &asset_id, &spec).map(|_| OpEffect::new());
            }
            let model = whisper::resolve_model(spec.model)?;
            return Ok(OpEffect::new().data(serde_json::json!({
                "asset": asset_id.to_string(),
                "model": model.name,
                "modelPath": model.path.display().to_string(),
                "duration": spec.duration.to_string(),
            })));
        }

        let transcript = whisper::run(tool, &media, &asset_id, &spec)?;
        transcript.save(cx.paths)?;
        let span = transcript.span();
        Ok(OpEffect::new()
            .changed(&asset_id)
            .data(serde_json::json!({
                "asset": asset_id.to_string(),
                "words": transcript.words.len(),
                "language": transcript.language,
                "model": transcript.model,
                "covered": span.duration().to_string(),
                "path": cx.paths.relativize(&Transcript::path(cx.paths, &asset_id)),
            })))
    }
}

/// Seconds from whatever a tool wrote: a JSON number, a decimal string, or an `HH:MM:SS,mmm`
/// timestamp. Parsed exactly — `0.5` becomes `1/2`, never a float — so two files describing
/// the same speech produce the same document.
fn json_time(value: &serde_json::Value, field: &str) -> Result<Time> {
    match value {
        serde_json::Value::Number(number) => Time::parse(&number.to_string()),
        serde_json::Value::String(text) => Time::parse(&text.replace(',', ".")),
        other => Err(Error::bad_args(format!(
            "'{field}' must be a number or a time string, got {other}"
        ))),
    }
    .map_err(|error| Error::bad_args(format!("'{field}': {error}")))
}

/// A time from the first key that is present.
fn time_of(item: &serde_json::Value, keys: &[&str]) -> Result<Time> {
    for key in keys {
        if let Some(value) = item.get(*key) {
            if !value.is_null() {
                return json_time(value, key);
            }
        }
    }
    Err(Error::bad_args(format!(
        "a word needs one of {} to place it in time",
        keys.join("/")
    )))
}

/// Confidence under any of the names the ecosystem uses, defaulting to certain.
fn confidence_of(item: &serde_json::Value) -> f32 {
    item.get("confidence")
        .or_else(|| item.get("score"))
        .or_else(|| item.get("probability"))
        .and_then(serde_json::Value::as_f64)
        .map(|value| value.clamp(0.0, 1.0) as f32)
        .unwrap_or(1.0)
}

/// A `{text|word, start|from, end|to}` object.
fn plain_word(item: &serde_json::Value) -> Result<Option<Word>> {
    let text = item
        .get("text")
        .or_else(|| item.get("word"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Error::bad_args("a word needs a 'text' or 'word' string"))?
        .trim()
        .to_string();
    if text.is_empty() {
        return Ok(None);
    }
    let start = time_of(item, &["start", "from"])?;
    let end = time_of(item, &["end", "to"])?;
    if end < start {
        return Err(Error::bad_args(format!(
            "word '{text}' ends at {end} before it starts at {start}"
        )));
    }
    Ok(Some(Word::new(text, start, end, confidence_of(item))))
}

fn plain_words(items: &[serde_json::Value]) -> Result<Vec<Word>> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        if let Some(word) = plain_word(item)? {
            out.push(word);
        }
    }
    Ok(out)
}

/// whisper.cpp `--output-json --output-json-full`: `offsets` are milliseconds, which is the
/// exact form, so they win over the `HH:MM:SS,mmm` strings beside them.
fn whisper_cpp_words(items: &[serde_json::Value]) -> Result<Vec<Word>> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let text = item
            .get("text")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::bad_args("a whisper.cpp entry needs 'text'"))?
            .trim()
            .to_string();
        if text.is_empty() {
            continue;
        }
        let (start, end) = match item.get("offsets") {
            Some(offsets) if offsets.is_object() => {
                let millis = |key: &str| -> Result<Time> {
                    let value = offsets
                        .get(key)
                        .and_then(serde_json::Value::as_i64)
                        .ok_or_else(|| {
                            Error::bad_args(format!("'offsets.{key}' must be milliseconds"))
                        })?;
                    Time::new(value, 1000)
                };
                (millis("from")?, millis("to")?)
            }
            _ => {
                let stamps = item.get("timestamps").ok_or_else(|| {
                    Error::bad_args("a whisper.cpp entry needs 'offsets' or 'timestamps'")
                })?;
                (
                    time_of(stamps, &["from"])?,
                    time_of(stamps, &["to"])?,
                )
            }
        };
        if end < start {
            return Err(Error::bad_args(format!(
                "word '{text}' ends at {end} before it starts at {start}"
            )));
        }
        out.push(Word::new(text, start, end, confidence_of(item)));
    }
    Ok(out)
}

/// WhisperX / faster-whisper: segments, each usually carrying its own word list. A segment
/// with no words is kept as one entry rather than dropped — coarse timing is still better
/// than no text.
fn whisperx_words(items: &[serde_json::Value]) -> Result<Vec<Word>> {
    let mut out = Vec::new();
    for segment in items {
        match segment.get("words").and_then(serde_json::Value::as_array) {
            Some(words) if !words.is_empty() => out.extend(plain_words(words)?),
            _ => {
                if let Some(word) = plain_word(segment)? {
                    out.push(word);
                }
            }
        }
    }
    Ok(out)
}

/// Words out of whatever JSON a transcription tool produced, plus the language it claims.
///
/// The shape is detected rather than demanded: an agent that just ran whisper.cpp, WhisperX
/// or its own aligner should not have to reshape the output first, and a wrong guess here
/// is cheap to report while a refused import is a dead end.
fn parse_words(value: &serde_json::Value) -> Result<(Vec<Word>, Option<String>)> {
    if let Some(items) = value.as_array() {
        return Ok((plain_words(items)?, None));
    }
    let object = value.as_object().ok_or_else(|| {
        Error::bad_args("transcript JSON must be an array of words or an object holding them")
    })?;
    let language = |key: &str| -> Option<String> {
        object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    if let Some(items) = object.get("transcription").and_then(|v| v.as_array()) {
        let detected = object
            .get("result")
            .and_then(|result| result.get("language"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        return Ok((whisper_cpp_words(items)?, detected));
    }
    if let Some(items) = object.get("segments").and_then(|v| v.as_array()) {
        return Ok((whisperx_words(items)?, language("language")));
    }
    if let Some(items) = object.get("words").and_then(|v| v.as_array()) {
        return Ok((plain_words(items)?, language("language")));
    }
    Err(Error::bad_args(
        "unrecognised transcript JSON: expected a top-level array of {text,start,end}, a \
         whisper.cpp 'transcription' array, a WhisperX 'segments' array, or a 'words' array",
    ))
}

struct TranscriptImport;

impl Op for TranscriptImport {
    fn id(&self) -> &'static str {
        "transcript.import"
    }

    fn about(&self) -> &'static str {
        "Attach word timestamps produced elsewhere to an asset"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["asset"],
            "properties": {
                "asset": { "type": "string", "description": "Asset id, name or file stem" },
                "path": {
                    "type": "string",
                    "description": "Word JSON file: whisper.cpp, WhisperX, or [{text,start,end}]"
                },
                "words": {
                    "description": "The same JSON inline, for a caller with no file to write",
                    "oneOf": [ { "type": "array" }, { "type": "object" }, { "type": "string" } ]
                },
                "language": {
                    "type": "string",
                    "description": "Language tag; defaults to what the file declares, else 'und'"
                },
                "model": {
                    "type": "string",
                    "description": "Provenance recorded with the words; defaults to 'import'"
                }
            },
            "additionalProperties": false
        })
    }

    fn apply(&self, project: &mut Project, args: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&args, &["asset", "path", "words", "language", "model"])?;
        let asset_id = project.resolve_asset(args::str_field(&args, "asset")?)?;

        let value = match (args::opt_str(&args, "path"), args.get("words")) {
            (Some(path), _) => {
                let path = Path::new(path);
                let bytes = std::fs::read(path).map_err(|error| Error::io(path, error))?;
                serde_json::from_slice(&bytes).map_err(|error| Error::json(path, error))?
            }
            (None, Some(serde_json::Value::String(text))) => serde_json::from_str(text)
                .map_err(|error| Error::bad_args(format!("'words' is not JSON: {error}")))?,
            (None, Some(inline)) => inline.clone(),
            (None, None) => {
                return Err(Error::bad_args(
                    "transcript.import needs '--path <file.json>' or '--words <json>'",
                ))
            }
        };

        let (words, detected) = parse_words(&value)?;
        if words.is_empty() {
            return Err(Error::op(
                "the transcript holds no words; check that the file is word-level output",
            ));
        }
        let language = args::opt_str(&args, "language")
            .map(str::to_string)
            .or(detected)
            .unwrap_or_else(|| "und".to_string());
        // An external file's own model claim is not provenance this engine can vouch for,
        // so imported words are labelled as imported unless the caller states otherwise.
        let model = args::opt_str(&args, "model").unwrap_or("import");
        let transcript = Transcript::new(asset_id.clone(), language, model, words);
        let span = transcript.span();
        let effect = OpEffect::new()
            .changed(&asset_id)
            .data(serde_json::json!({
                "asset": asset_id.to_string(),
                "words": transcript.words.len(),
                "language": transcript.language,
                "model": transcript.model,
                "covered": span.duration().to_string(),
            }));
        if cx.dry_run {
            return Ok(effect);
        }
        transcript.save(cx.paths)?;
        Ok(effect)
    }
}

struct TranscriptFind;

impl Op for TranscriptFind {
    fn id(&self) -> &'static str {
        "transcript.find"
    }

    fn about(&self) -> &'static str {
        "Locate a phrase in a transcript, in source time and in timeline time"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["phrase"],
            "properties": {
                "phrase": {
                    "type": "string",
                    "description": "Word or word sequence; matched ignoring case and punctuation"
                },
                "asset": { "type": "string", "description": "Asset to search" },
                "target": {
                    "type": "string",
                    "description": "Clip selector to search instead of a whole asset",
                    "examples": ["#talk", "clip[track=V1]"]
                }
            },
            "additionalProperties": false
        })
    }

    fn is_query(&self) -> bool {
        true
    }

    fn apply(&self, project: &mut Project, args: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&args, &["phrase", "asset", "target"])?;
        let phrase = args::str_field(&args, "phrase")?;
        let seq = cx.sequence(project)?;
        let fps = sequence_fps(project, &seq)?;
        // Unlike the editing ops, a query must answer for an asset that has not been placed
        // yet: "where is pricing mentioned" is the question an agent asks *before* cutting.
        let (clips, transcripts) = match args::opt_str(&args, "target") {
            Some(text) => {
                let clips = selector::resolve_clips(project, &seq, text)?;
                let transcripts = transcripts_for(project, &seq, &clips, cx.paths)?;
                (clips, transcripts)
            }
            None => {
                let query = args::opt_str(&args, "asset").ok_or_else(|| {
                    Error::bad_args("transcript.find needs '--phrase' with '--target' or '--asset'")
                })?;
                let asset = project.resolve_asset(query)?;
                let transcript = Transcript::load(cx.paths, &asset)?;
                let clips = clips_using(project, &seq, &asset)?;
                (clips, BTreeMap::from([(asset, transcript)]))
            }
        };

        let mut source = Vec::new();
        for (asset, transcript) in &transcripts {
            for span in transcript.find_phrase(phrase) {
                let mut entry = span_json(span, fps);
                entry["asset"] = serde_json::Value::String(asset.to_string());
                source.push(entry);
            }
        }

        let sequence = project.sequence(&seq)?;
        let mut timeline = Vec::new();
        for (track_id, clip_id) in &clips {
            let Some((_, clip)) = sequence.find_clip(clip_id) else {
                continue;
            };
            let asset = clip_asset(clip)?;
            let Some(transcript) = transcripts.get(&asset) else {
                continue;
            };
            for span in timeline_view(transcript, clip).find_phrase(phrase) {
                let mut entry = span_json(span, fps);
                entry["track"] = serde_json::Value::String(track_id.to_string());
                entry["clip"] = serde_json::Value::String(clip_id.to_string());
                timeline.push(entry);
            }
        }

        Ok(OpEffect::new().data(serde_json::json!({
            "phrase": phrase,
            "occurrences": { "source": source.len(), "timeline": timeline.len() },
            "source": source,
            "timeline": timeline,
        })))
    }
}

struct TranscriptCutWords;

impl Op for TranscriptCutWords {
    fn id(&self) -> &'static str {
        "transcript.cut-words"
    }

    fn about(&self) -> &'static str {
        "Ripple out filler words from a clip, keeping its linked counterpart in sync"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["target"],
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Clip selector naming exactly one clip",
                    "examples": ["#talk", "clip[track=V1]:first"]
                },
                "words": {
                    "description": "Filler words or phrases to remove",
                    "default": "um,uh,er,ah,like,you know",
                    "oneOf": [
                        { "type": "string" },
                        { "type": "array", "items": { "type": "string" } }
                    ]
                },
                "min-gap": {
                    "type": "string",
                    "description": "Fillers closer together than this become one cut",
                    "default": "1/4"
                },
                "pad": {
                    "type": "string",
                    "description": "Extra time removed on each side of every filler",
                    "default": "0/1"
                }
            },
            "additionalProperties": false
        })
    }

    fn apply(&self, project: &mut Project, args: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "words", "min-gap", "pad"])?;
        let seq = cx.sequence(project)?;
        let fps = sequence_fps(project, &seq)?;
        let target = args::str_field(&args, "target")?;
        let (track_id, clip_id) = selector::resolve_one_clip(project, &seq, target)?;

        let requested_gap = args::opt_time(&args, "min-gap", fps)?.unwrap_or_else(default_min_gap);
        let min_gap = requested_gap.snap(fps);
        let requested_pad = args::opt_time(&args, "pad", fps)?.unwrap_or(Time::ZERO);
        let pad = requested_pad.snap(fps);
        if pad.is_negative() {
            return Err(Error::bad_args("'pad' cannot be negative"));
        }
        let mut fillers = args::string_list(&args, "words")?;
        if fillers.is_empty() {
            fillers = DEFAULT_FILLERS.iter().map(|word| word.to_string()).collect();
        }

        let sequence = project.sequence(&seq)?;
        let (_, clip) = sequence
            .find_clip(&clip_id)
            .ok_or_else(|| Error::no_match("clip", clip_id.as_str(), sequence.clip_ids()))?;
        let clip_span = clip.span();
        let asset = clip_asset(clip)?;
        let transcript = Transcript::load(cx.paths, &asset)?;
        let view = timeline_view(&transcript, clip);
        let raw = view.filler_spans(&fillers, min_gap);
        let cuts = prepare_cuts(raw, pad, clip_span, fps);

        let removed_total = cuts
            .iter()
            .fold(Time::ZERO, |sum, span| sum + span.duration());
        let mut effect = OpEffect::new()
            .snap("min-gap", requested_gap, min_gap, fps)
            .snap("pad", requested_pad, pad, fps)
            .data(serde_json::json!({
                "clip": clip_id.to_string(),
                "track": track_id.to_string(),
                "cuts": cuts
                    .iter()
                    .map(|span| {
                        let mut entry = span_json(*span, fps);
                        entry["words"] = serde_json::Value::String(transcript::text_of(
                            view.in_span(*span),
                        ));
                        entry
                    })
                    .collect::<Vec<_>>(),
                "cutCount": cuts.len(),
                "removed": removed_total.to_string(),
                "removedSeconds": removed_total.as_secs_f64(),
                "fillers": fillers,
            }));
        if cuts.is_empty() {
            return Ok(effect.warn(
                "no-fillers",
                &clip_id,
                format!(
                    "none of [{}] appear in the {} words this clip shows",
                    fillers.join(", "),
                    view.words.len()
                ),
            ));
        }

        let tracks = linked_tracks(project, &seq, &track_id, &clip_id)?;
        if let Some((sequence_id, stale)) = caption_drift(project, &seq, cuts[0].start) {
            effect = effect.warn(
                "caption-desync",
                sequence_id,
                format!(
                    "{stale} caption cue(s) start at or after the first cut and are not moved by \
                     a ripple; regenerate them with caption.generate"
                ),
            );
        }
        if cx.dry_run {
            return Ok(effect);
        }

        let done = apply_cuts(project, &seq, &tracks, &cuts)?;
        for id in &tracks {
            effect = effect.changed(id);
        }
        for id in done.changed {
            effect = effect.changed(id);
        }
        for id in done.created {
            effect = effect.created(id);
        }
        for id in done.removed {
            effect = effect.removed(id);
        }
        Ok(effect)
    }
}

struct TranscriptKeepPhrases;

impl Op for TranscriptKeepPhrases {
    fn id(&self) -> &'static str {
        "transcript.keep-phrases"
    }

    fn about(&self) -> &'static str {
        "Keep only the ranges of a clip containing the named phrases, rippling out the rest"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["target", "phrases"],
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Clip selector naming exactly one clip"
                },
                "phrases": {
                    "description": "Phrases to keep; every one must match something",
                    "oneOf": [
                        { "type": "string" },
                        { "type": "array", "items": { "type": "string" } }
                    ]
                },
                "pad": {
                    "type": "string",
                    "description": "Extra time kept on each side of every phrase",
                    "default": "0/1"
                }
            },
            "additionalProperties": false
        })
    }

    fn apply(&self, project: &mut Project, args: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "phrases", "pad"])?;
        let seq = cx.sequence(project)?;
        let fps = sequence_fps(project, &seq)?;
        let target = args::str_field(&args, "target")?;
        let (track_id, clip_id) = selector::resolve_one_clip(project, &seq, target)?;
        let phrases = args::string_list(&args, "phrases")?;
        if phrases.is_empty() {
            return Err(Error::bad_args(
                "transcript.keep-phrases needs at least one phrase",
            ));
        }
        let requested_pad = args::opt_time(&args, "pad", fps)?.unwrap_or(Time::ZERO);
        let pad = requested_pad.snap(fps);
        if pad.is_negative() {
            return Err(Error::bad_args("'pad' cannot be negative"));
        }

        let sequence = project.sequence(&seq)?;
        let (_, clip) = sequence
            .find_clip(&clip_id)
            .ok_or_else(|| Error::no_match("clip", clip_id.as_str(), sequence.clip_ids()))?;
        let clip_span = clip.span();
        let asset = clip_asset(clip)?;
        let transcript = Transcript::load(cx.paths, &asset)?;
        let view = timeline_view(&transcript, clip);

        let mut hits = Vec::new();
        for phrase in &phrases {
            let found = view.find_phrase(phrase);
            if found.is_empty() {
                // A phrase that matches nothing is an error, not a smaller edit: the
                // alternative is an agent keeping two sentences when it meant three and
                // never learning which one it lost.
                return Err(Error::no_match(
                    "phrase",
                    phrase,
                    vec![transcript::text_of(&view.words)
                        .chars()
                        .take(160)
                        .collect::<String>()],
                ));
            }
            hits.extend(found);
        }
        let keeps = prepare_cuts(hits, pad, clip_span, fps);
        let drops = complement(clip_span, &keeps);
        let kept_total = keeps
            .iter()
            .fold(Time::ZERO, |sum, span| sum + span.duration());
        let dropped_total = drops
            .iter()
            .fold(Time::ZERO, |sum, span| sum + span.duration());

        let mut effect = OpEffect::new()
            .snap("pad", requested_pad, pad, fps)
            .data(serde_json::json!({
                "clip": clip_id.to_string(),
                "track": track_id.to_string(),
                "kept": keeps.iter().map(|span| span_json(*span, fps)).collect::<Vec<_>>(),
                "keptTotal": kept_total.to_string(),
                "removed": drops.iter().map(|span| span_json(*span, fps)).collect::<Vec<_>>(),
                "removedTotal": dropped_total.to_string(),
                "phrases": phrases,
            }));
        if drops.is_empty() {
            return Ok(effect.warn(
                "nothing-removed",
                &clip_id,
                "the phrases cover the whole clip",
            ));
        }

        let tracks = linked_tracks(project, &seq, &track_id, &clip_id)?;
        if let Some((sequence_id, stale)) = caption_drift(project, &seq, drops[0].start) {
            effect = effect.warn(
                "caption-desync",
                sequence_id,
                format!(
                    "{stale} caption cue(s) start at or after the first removal and are not moved \
                     by a ripple; regenerate them with caption.generate"
                ),
            );
        }
        if cx.dry_run {
            return Ok(effect);
        }

        let done = apply_cuts(project, &seq, &tracks, &drops)?;
        for id in &tracks {
            effect = effect.changed(id);
        }
        for id in done.changed {
            effect = effect.changed(id);
        }
        for id in done.created {
            effect = effect.created(id);
        }
        for id in done.removed {
            effect = effect.removed(id);
        }
        Ok(effect)
    }
}

struct CaptionGenerate;

impl Op for CaptionGenerate {
    fn id(&self) -> &'static str {
        "caption.generate"
    }

    fn about(&self) -> &'static str {
        "Build caption cues from a transcript, honouring reading rate and line length"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "asset": { "type": "string", "description": "Caption every clip using this asset" },
                "target": { "type": "string", "description": "Clip selector to caption instead" },
                "track": {
                    "type": "string",
                    "description": "Caption track to write to; the first one is used, or one is created"
                },
                "style": {
                    "type": "string",
                    "description": "Caption style id or name; the track's style, else 'default'"
                },
                "max-cps": {
                    "type": "number",
                    "description": "Reading-rate ceiling in characters per second for this run",
                    "examples": [17, 20]
                }
            },
            "additionalProperties": false
        })
    }

    fn apply(&self, project: &mut Project, args: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&args, &["asset", "target", "track", "style", "max-cps"])?;
        let seq = cx.sequence(project)?;
        let fps = sequence_fps(project, &seq)?;
        let size = project.sequence(&seq)?.size;
        let clips = target_clips(project, &seq, &args, "caption.generate")?;
        let transcripts = transcripts_for(project, &seq, &clips, cx.paths)?;

        let sequence = project.sequence(&seq)?;
        let mut words: Vec<Word> = Vec::new();
        for (_, clip_id) in &clips {
            let Some((_, clip)) = sequence.find_clip(clip_id) else {
                continue;
            };
            let asset = clip_asset(clip)?;
            if let Some(transcript) = transcripts.get(&asset) {
                words.extend(transcript.words_for_clip(clip));
            }
        }
        words.sort_by(|a, b| a.start.cmp(&b.start));
        if words.is_empty() {
            return Err(Error::op(
                "the transcripts hold no words inside the selected clips; check that the clips \
                 cover the transcribed range",
            ));
        }

        let requested_style = args::opt_str(&args, "style");
        let track_style = match args::opt_str(&args, "track") {
            Some(text) => {
                let id = selector::resolve_track(project, &seq, text)?;
                project.sequence(&seq)?.track(&id)?.style.clone()
            }
            None => project
                .sequence(&seq)?
                .tracks
                .iter()
                .find(|track| track.kind == TrackKind::Caption)
                .and_then(|track| track.style.clone()),
        };
        let (style_id, style_created) = match (requested_style, track_style) {
            (Some(query), _) => (resolve_style(project, query)?, false),
            (None, Some(existing)) if project.styles.contains_key(&existing) => (existing, false),
            _ => default_style(project),
        };

        let mut style = project
            .styles
            .get(&style_id)
            .expect("the style was just resolved or created")
            .clone();
        if let Some(ceiling) = args::opt_f64(&args, "max-cps")? {
            if ceiling <= 0.0 {
                return Err(Error::bad_args("'max-cps' must be positive"));
            }
            // A per-run ceiling does not rewrite the style: the next generation should use
            // the style the document states, not a value someone passed once.
            style.max_cps = ceiling;
        }

        let mut cues = captions::cues_from_words_for_width(&words, &style, size[0]);
        let mut snapped = 0usize;
        for cue in cues.iter_mut() {
            if snap_cue(cue, fps) {
                snapped += 1;
            }
        }
        let achieved = max_cps(&cues);
        let over: Vec<&CaptionCue> = cues
            .iter()
            .filter(|cue| cue.chars_per_second() > style.max_cps)
            .collect();

        let mut effect = OpEffect::new().data(serde_json::json!({
            "cues": cues.len(),
            "maxCps": achieved,
            "ceiling": style.max_cps,
            "style": style_id.to_string(),
            "words": words.len(),
            "snappedCues": snapped,
            "charsPerLine": captions::chars_per_line(&style, size[0]),
            "tooFast": over.len(),
        }));
        for cue in &over {
            effect = effect.warn(
                "caption-too-fast",
                &cue.id,
                format!(
                    "{:.1} cps over a {:.1} ceiling at {}: the words were said faster than the \
                     ceiling allows, so the cue keeps its text",
                    cue.chars_per_second(),
                    style.max_cps,
                    cue.span.start.timecode(fps)
                ),
            );
        }
        if style_created {
            effect = effect.created(&style_id);
        }
        if cx.dry_run {
            return Ok(effect);
        }

        let (track_id, track_created) = caption_track(project, &seq, args::opt_str(&args, "track"))?;
        let track = project.sequence_mut(&seq)?.track_mut(&track_id)?;
        track.style = Some(style_id.clone());
        let replaced = place_cues(track, cues);
        effect = effect.changed(&track_id);
        if track_created {
            effect = effect.created(&track_id);
        }
        for id in replaced {
            effect = effect.removed(id);
        }
        Ok(effect)
    }
}

/// Subtitle interchange formats. Parsing and writing live in `dvs-interop`; this is only
/// the choice of which one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Srt,
    Vtt,
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Format::Srt => "srt",
            Format::Vtt => "vtt",
        }
    }

    fn parse(text: &str) -> Result<Format> {
        match text.trim().trim_start_matches('.').to_ascii_lowercase().as_str() {
            "srt" | "subrip" => Ok(Format::Srt),
            "vtt" | "webvtt" => Ok(Format::Vtt),
            other => Err(Error::bad_args(format!(
                "unknown caption format '{other}'; use srt or vtt"
            ))),
        }
    }

    /// `--format` wins, then the file extension, then the content: a WebVTT file always
    /// starts with its magic line, so a mislabelled file still imports correctly.
    fn detect(explicit: Option<&str>, path: &Path, content: Option<&str>) -> Result<Format> {
        if let Some(text) = explicit {
            return Format::parse(text);
        }
        if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
            if let Ok(format) = Format::parse(ext) {
                return Ok(format);
            }
        }
        match content {
            Some(text) if text.trim_start().starts_with("WEBVTT") => Ok(Format::Vtt),
            Some(_) => Ok(Format::Srt),
            None => Err(Error::bad_args(format!(
                "cannot tell the caption format of '{}'; pass --format srt|vtt",
                path.display()
            ))),
        }
    }
}

struct CaptionImport;

impl Op for CaptionImport {
    fn id(&self) -> &'static str {
        "caption.import"
    }

    fn about(&self) -> &'static str {
        "Load an SRT or VTT subtitle file onto a caption track"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string", "description": "SRT or VTT file to read" },
                "track": {
                    "type": "string",
                    "description": "Caption track to write to; the first one is used, or one is created"
                },
                "format": {
                    "type": "string",
                    "enum": ["srt", "vtt"],
                    "description": "Override the format detected from the extension or content"
                }
            },
            "additionalProperties": false
        })
    }

    fn apply(&self, project: &mut Project, args: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&args, &["path", "track", "format"])?;
        let seq = cx.sequence(project)?;
        let fps = sequence_fps(project, &seq)?;
        let path = Path::new(args::str_field(&args, "path")?);
        let text = std::fs::read_to_string(path).map_err(|error| Error::io(path, error))?;
        let format = Format::detect(args::opt_str(&args, "format"), path, Some(&text))?;
        let mut cues = match format {
            Format::Srt => from_srt(&text)?,
            Format::Vtt => from_vtt(&text)?,
        };
        if cues.is_empty() {
            return Err(Error::op(format!(
                "'{}' holds no cues; is it really a {} file?",
                path.display(),
                format.name()
            )));
        }
        cues.sort_by(|a, b| a.span.start.cmp(&b.span.start));
        let mut snapped = 0usize;
        for cue in cues.iter_mut() {
            if snap_cue(cue, fps) {
                snapped += 1;
            }
        }

        let mut effect = OpEffect::new().data(serde_json::json!({
            "cues": cues.len(),
            "format": format.name(),
            "path": path.display().to_string(),
            "snappedCues": snapped,
            "maxCps": max_cps(&cues),
        }));
        if cx.dry_run {
            return Ok(effect);
        }
        let (track_id, created) = caption_track(project, &seq, args::opt_str(&args, "track"))?;
        let track = project.sequence_mut(&seq)?.track_mut(&track_id)?;
        let replaced = place_cues(track, cues);
        effect = effect.changed(&track_id);
        if created {
            effect = effect.created(&track_id);
        }
        for id in replaced {
            effect = effect.removed(id);
        }
        Ok(effect)
    }
}

struct CaptionExport;

impl Op for CaptionExport {
    fn id(&self) -> &'static str {
        "caption.export"
    }

    fn about(&self) -> &'static str {
        "Write a caption track out as SRT or VTT"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["out"],
            "properties": {
                "out": { "type": "string", "description": "File to write" },
                "format": {
                    "type": "string",
                    "enum": ["srt", "vtt"],
                    "description": "Defaults to the output extension"
                },
                "track": {
                    "type": "string",
                    "description": "Caption track to export; required when there is more than one"
                }
            },
            "additionalProperties": false
        })
    }

    fn is_query(&self) -> bool {
        true
    }

    fn apply(&self, project: &mut Project, args: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&args, &["out", "format", "track"])?;
        let seq = cx.sequence(project)?;
        let out = Path::new(args::str_field(&args, "out")?);
        let format = Format::detect(args::opt_str(&args, "format"), out, None)?;

        let sequence = project.sequence(&seq)?;
        let track = match args::opt_str(&args, "track") {
            Some(text) => {
                let id = selector::resolve_track(project, &seq, text)?;
                sequence.track(&id)?
            }
            None => {
                let caption_tracks: Vec<&Track> = sequence
                    .tracks
                    .iter()
                    .filter(|track| track.kind == TrackKind::Caption)
                    .collect();
                match caption_tracks.as_slice() {
                    [one] => *one,
                    [] => {
                        return Err(Error::op(format!(
                            "sequence '{}' has no caption track to export",
                            sequence.name
                        )))
                    }
                    many => {
                        return Err(Error::bad_args(format!(
                            "sequence '{}' has {} caption tracks; name one with --track",
                            sequence.name,
                            many.len()
                        )))
                    }
                }
            }
        };
        if track.cues.is_empty() {
            return Err(Error::op(format!(
                "caption track '{}' has no cues",
                track.name
            )));
        }
        let body = match format {
            Format::Srt => to_srt(&track.cues),
            Format::Vtt => to_vtt(&track.cues),
        };

        let effect = OpEffect::new().data(serde_json::json!({
            "path": out.display().to_string(),
            "format": format.name(),
            "cues": track.cues.len(),
            "track": track.id.to_string(),
            "bytes": body.len(),
        }));
        if cx.dry_run {
            return Ok(effect);
        }
        if let Some(parent) = out.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
        }
        std::fs::write(out, body).map_err(|error| Error::io(out, error))?;
        Ok(effect)
    }
}

struct CaptionSetStyle;

impl Op for CaptionSetStyle {
    fn id(&self) -> &'static str {
        "caption.style"
    }

    fn about(&self) -> &'static str {
        "Set caption style fields, creating the default style if there is none"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "style": { "type": "string", "description": "Style id or name; defaults to the track's, else 'default'" },
                "track": { "type": "string", "description": "Also attach this style to a caption track" },
                "name": { "type": "string", "description": "Rename the style" },
                "font": { "type": "string", "description": "Font family", "examples": ["Inter", "sans-serif"] },
                "size-px": { "type": "number", "description": "Type size in sequence pixels" },
                "color": { "type": "string", "description": "Fill color", "examples": ["#ffffff"] },
                "outline": { "type": "string", "description": "Outline color, or 'none'" },
                "background": { "type": "string", "description": "Plate color, or 'none'" },
                "position": { "type": "string", "enum": ["top", "middle", "bottom"] },
                "safe-area": { "type": "number", "description": "Fraction of the frame text may occupy" },
                "max-cps": { "type": "number", "description": "Reading-rate ceiling" },
                "max-lines": { "type": "integer", "description": "Hard line ceiling per cue" }
            },
            "additionalProperties": false
        })
    }

    fn apply(&self, project: &mut Project, args: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        let known = [
            "style",
            "track",
            "name",
            "font",
            "size-px",
            "color",
            "outline",
            "background",
            "position",
            "safe-area",
            "max-cps",
            "max-lines",
        ];
        args::reject_unknown(&args, &known)?;
        // `--track` alone is still an action: it attaches the style to that track.
        if known[2..].iter().all(|key| args.get(*key).is_none()) && args.get("track").is_none() {
            return Err(Error::bad_args(format!(
                "caption.style needs '--track <track>' or at least one field to set: {}",
                known[2..].join(", ")
            )));
        }
        let seq = cx.sequence(project)?;
        let track_arg = args::opt_str(&args, "track");
        let track_id = match track_arg {
            Some(text) => Some(selector::resolve_track(project, &seq, text)?),
            None => None,
        };
        let track_style = match &track_id {
            Some(id) => project.sequence(&seq)?.track(id)?.style.clone(),
            None => project
                .sequence(&seq)?
                .tracks
                .iter()
                .find(|track| track.kind == TrackKind::Caption)
                .and_then(|track| track.style.clone()),
        };
        let (style_id, created) = match (args::opt_str(&args, "style"), track_style) {
            (Some(query), _) => (resolve_style(project, query)?, false),
            (None, Some(existing)) if project.styles.contains_key(&existing) => (existing, false),
            _ => default_style(project),
        };

        let color = |key: &str| -> Result<Option<Option<Rgba>>> {
            match args::opt_str(&args, key) {
                None => Ok(None),
                Some(text) if text.eq_ignore_ascii_case("none") => Ok(Some(None)),
                Some(text) => Ok(Some(Some(Rgba::parse(text)?))),
            }
        };
        let outline = color("outline")?;
        let background = color("background")?;
        let fill = match args::opt_str(&args, "color") {
            None => None,
            Some(text) => Some(Rgba::parse(text)?),
        };
        let position = match args::opt_str(&args, "position") {
            None => None,
            Some(text) => Some(match text.to_ascii_lowercase().as_str() {
                "top" => CaptionPosition::Top,
                "middle" | "center" | "centre" => CaptionPosition::Middle,
                "bottom" => CaptionPosition::Bottom,
                other => {
                    return Err(Error::bad_args(format!(
                        "unknown caption position '{other}'; use top, middle or bottom"
                    )))
                }
            }),
        };
        let size_px = args::opt_f64(&args, "size-px")?;
        if let Some(size) = size_px.filter(|size| *size <= 0.0) {
            return Err(Error::bad_args(format!("'size-px' must be positive, got {size}")));
        }
        let safe_area = args::opt_f64(&args, "safe-area")?;
        if let Some(area) = safe_area.filter(|area| *area <= 0.0 || *area > 1.0) {
            return Err(Error::bad_args(format!(
                "'safe-area' is a fraction of the frame in (0, 1], got {area}"
            )));
        }
        let ceiling = args::opt_f64(&args, "max-cps")?;
        if let Some(value) = ceiling.filter(|value| *value <= 0.0) {
            return Err(Error::bad_args(format!("'max-cps' must be positive, got {value}")));
        }
        let max_lines = args::opt_f64(&args, "max-lines")?;
        if let Some(lines) = max_lines.filter(|lines| *lines < 1.0) {
            return Err(Error::bad_args(format!(
                "'max-lines' must be at least 1, got {lines}"
            )));
        }

        let mut effect = OpEffect::new().changed(&style_id);
        if created {
            effect = effect.created(&style_id);
        }
        if cx.dry_run {
            return Ok(effect);
        }

        let style = project
            .styles
            .get_mut(&style_id)
            .expect("the style was just resolved or created");
        if let Some(name) = args::opt_str(&args, "name") {
            style.name = name.to_string();
        }
        if let Some(font) = args::opt_str(&args, "font") {
            style.font = font.to_string();
        }
        if let Some(size) = size_px {
            style.size_px = size as f32;
        }
        if let Some(value) = fill {
            style.color = value;
        }
        if let Some(value) = outline {
            style.outline = value;
        }
        if let Some(value) = background {
            style.background = value;
        }
        if let Some(value) = position {
            style.position = value;
        }
        if let Some(area) = safe_area {
            style.safe_area = area as f32;
        }
        if let Some(value) = ceiling {
            style.max_cps = value;
        }
        if let Some(lines) = max_lines {
            style.max_lines = lines as usize;
        }
        let summary = serde_json::to_value(&*style).expect("a style serializes");
        effect = effect.data(summary);

        if let Some(id) = track_id {
            let track = project.sequence_mut(&seq)?.track_mut(&id)?;
            assert_unlocked(track)?;
            track.style = Some(style_id);
            effect = effect.changed(&id);
        }
        Ok(effect)
    }
}

struct CaptionRemove;

impl Op for CaptionRemove {
    fn id(&self) -> &'static str {
        "caption.remove"
    }

    fn about(&self) -> &'static str {
        "Remove caption cues by selector, or every cue on a track"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Cue selector",
                    "examples": ["cue[text*=pricing]", "cue[track=CC1]@00:10-00:20"]
                },
                "track": { "type": "string", "description": "Remove every cue on this track" }
            },
            "additionalProperties": false
        })
    }

    fn apply(&self, project: &mut Project, args: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "track"])?;
        let seq = cx.sequence(project)?;
        let mut doomed: Vec<(TrackId, String)> = Vec::new();
        match (args::opt_str(&args, "target"), args::opt_str(&args, "track")) {
            (Some(text), _) => {
                let selector = selector::Selector::parse(text)?;
                let matches = selector::resolve(project, &seq, &selector)?;
                for found in &matches {
                    match found {
                        selector::Match::Cue { track, cue } => {
                            doomed.push((track.clone(), cue.to_string()))
                        }
                        other => {
                            return Err(Error::bad_args(format!(
                                "selector '{text}' names a {:?} ({}); caption.remove takes cues or \
                                 a --track",
                                other.kind(),
                                other.id()
                            )))
                        }
                    }
                }
            }
            (None, Some(text)) => {
                let id = selector::resolve_track(project, &seq, text)?;
                let track = project.sequence(&seq)?.track(&id)?;
                if track.kind != TrackKind::Caption {
                    return Err(Error::bad_args(format!(
                        "track '{}' is not a caption track",
                        track.name
                    )));
                }
                for cue in &track.cues {
                    doomed.push((id.clone(), cue.id.to_string()));
                }
            }
            (None, None) => {
                return Err(Error::bad_args(
                    "caption.remove needs '--target <cue selector>' or '--track <track>'",
                ))
            }
        }
        if doomed.is_empty() {
            return Err(Error::op("no caption cues matched"));
        }

        let mut effect = OpEffect::new().data(serde_json::json!({ "cues": doomed.len() }));
        for (_, cue) in &doomed {
            effect = effect.removed(cue);
        }
        let mut touched: Vec<TrackId> = doomed.iter().map(|(track, _)| track.clone()).collect();
        touched.sort();
        touched.dedup();
        for id in &touched {
            assert_unlocked(project.sequence(&seq)?.track(id)?)?;
            effect = effect.changed(id);
        }
        if cx.dry_run {
            return Ok(effect);
        }
        let condemned: std::collections::BTreeSet<&str> =
            doomed.iter().map(|(_, cue)| cue.as_str()).collect();
        let sequence = project.sequence_mut(&seq)?;
        for id in &touched {
            let track = sequence.track_mut(id)?;
            track.cues.retain(|cue| !condemned.contains(cue.id.as_str()));
        }
        Ok(effect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::asset::AssetStore;
    use dvs_core::ids::CueId;
    use dvs_core::paths::ProjectPaths;
    use dvs_core::project::{Asset, AssetKind, AudioStream, Probe, Source};
    use dvs_core::vfs::FsVfs;
    use std::sync::Arc;

    struct Harness {
        _dir: tempfile::TempDir,
        paths: ProjectPaths,
        assets: AssetStore,
        project: Project,
        seq: SequenceId,
    }

    fn harness() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::new(dir.path());
        let assets = AssetStore::new(paths.assets_dir(), Arc::new(FsVfs));
        let project = Project::new("t", Fps::new(30, 1).unwrap(), [1920, 1080], 48_000);
        let seq = project.active_sequence.clone();
        Harness {
            _dir: dir,
            paths,
            assets,
            project,
            seq,
        }
    }

    impl Harness {
        /// A timed audio+video asset. The bytes behind it are a placeholder — every op under
        /// test reads the transcript, not the media — but they are really in the store, so
        /// the asset resolves the way a real one does.
        fn asset(&mut self, name: &str, seconds: i64) -> AssetId {
            let id = AssetId::new();
            let hash = self
                .assets
                .import_bytes(name.as_bytes(), "mp4")
                .expect("the store accepts bytes");
            self.project.assets.insert(
                id.clone(),
                Asset {
                    id: id.clone(),
                    name: name.to_string(),
                    hash,
                    kind: AssetKind::Video,
                    probe: Probe {
                        duration: Time::from_secs(seconds),
                        audio: Some(AudioStream {
                            rate: 48_000,
                            channels: 2,
                            codec: "aac".into(),
                            stream_index: 1,
                            bit_rate: None,
                        }),
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

        fn track(&mut self, name: &str, kind: TrackKind) -> TrackId {
            let sequence = self.project.sequence_mut(&self.seq).unwrap();
            let track = Track::new(name, kind);
            let id = track.id.clone();
            sequence.tracks.push(track);
            id
        }

        fn place(&mut self, track: &TrackId, asset: &AssetId, start: i64, duration: i64) -> ClipId {
            let clip = Clip::new(
                Source::Asset {
                    asset: asset.clone(),
                    stream: None,
                },
                Time::from_secs(start),
                Time::from_secs(duration),
            );
            let id = clip.id.clone();
            self.project
                .sequence_mut(&self.seq)
                .unwrap()
                .track_mut(track)
                .unwrap()
                .place(clip);
            id
        }

        fn link(&mut self, a: &ClipId, b: &ClipId) {
            let sequence = self.project.sequence_mut(&self.seq).unwrap();
            let (track, index) = sequence.find_clip_mut(a).unwrap();
            track.clips[index].link = Some(b.clone());
            let (track, index) = sequence.find_clip_mut(b).unwrap();
            track.clips[index].link = Some(a.clone());
        }

        fn clip(&self, id: &ClipId) -> &Clip {
            self.project
                .sequence(&self.seq)
                .unwrap()
                .find_clip(id)
                .expect("the clip is still there")
                .1
        }

        fn track_of(&self, id: &TrackId) -> &Track {
            self.project.sequence(&self.seq).unwrap().track(id).unwrap()
        }
    }

    fn word(text: &str, start: (i64, i64), end: (i64, i64)) -> Word {
        Word::new(
            text,
            Time::new(start.0, start.1).unwrap(),
            Time::new(end.0, end.1).unwrap(),
            1.0,
        )
    }

    /// Ten seconds of speech at 30 fps boundaries with three fillers: "um" at 1–1.5,
    /// "uh" at 1.7–2.0 (merging with the first), and "like" at 6.0–6.5.
    fn fixture_words() -> Vec<Word> {
        vec![
            word("We", (0, 1), (1, 2)),
            word("um", (1, 1), (3, 2)),
            word("uh", (17, 10), (2, 1)),
            word("raised", (2, 1), (5, 2)),
            word("our", (5, 2), (3, 1)),
            word("pricing", (3, 1), (7, 2)),
            word("today.", (7, 2), (4, 1)),
            word("Billing", (5, 1), (11, 2)),
            word("is", (11, 2), (6, 1)),
            word("like", (6, 1), (13, 2)),
            word("live", (7, 1), (15, 2)),
            word("now.", (15, 2), (8, 1)),
        ]
    }

    fn save_transcript(h: &Harness, asset: &AssetId, words: Vec<Word>) {
        Transcript::new(asset.clone(), "en", "test", words)
            .save(&h.paths)
            .unwrap();
    }

    #[test]
    fn cut_words_removes_the_fillers_closes_the_gaps_and_keeps_the_link_in_sync() {
        let mut h = harness();
        let asset = h.asset("talk.mp4", 10);
        save_transcript(&h, &asset, fixture_words());
        let v1 = h.track("V1", TrackKind::Video);
        let a1 = h.track("A1", TrackKind::Audio);
        let video = h.place(&v1, &asset, 0, 10);
        let audio = h.place(&a1, &asset, 0, 10);
        h.link(&video, &audio);
        // A second clip after the target proves the ripple reaches past the edit.
        let tail = h.place(&v1, &asset, 10, 2);

        let mut cx = OpCx::new(&h.paths, &h.assets);
        let effect = TranscriptCutWords
            .apply(
                &mut h.project,
                serde_json::json!({ "target": video.as_str(), "min-gap": "0.5" }),
                &mut cx,
            )
            .unwrap();

        let data = effect.data.expect("cut-words reports what it cut");
        assert_eq!(data["cutCount"], 2, "the um/uh pair merges into one cut: {data}");
        assert_eq!(
            data["removed"], "3/2",
            "1.0 s of um+uh plus 0.5 s of like: {data}"
        );
        assert!(
            data["cuts"][0]["words"]
                .as_str()
                .unwrap()
                .contains("um"),
            "each cut names the words it removed: {data}"
        );

        let track = h.track_of(&v1);
        let material_end = track.clips.last().expect("clips remain").end();
        assert_eq!(
            track.gaps(material_end),
            Vec::new(),
            "a ripple must leave no hole anywhere in the material"
        );
        let video_total: Time = track
            .clips
            .iter()
            .filter(|clip| clip.id != tail)
            .fold(Time::ZERO, |sum, clip| sum + clip.duration);
        assert_eq!(
            video_total,
            Time::from_secs(10) - Time::new(3, 2).unwrap(),
            "the cut duration must come out of the clip"
        );
        assert_eq!(
            h.clip(&tail).start,
            Time::from_secs(10) - Time::new(3, 2).unwrap(),
            "clips after the edit move left by exactly what was removed"
        );

        let audio_track = h.track_of(&a1);
        let audio_total = audio_track
            .clips
            .iter()
            .fold(Time::ZERO, |sum, clip| sum + clip.duration);
        assert_eq!(
            audio_total, video_total,
            "the linked audio must lose the same time as the video"
        );
        let video_bounds: Vec<Span> = track
            .clips
            .iter()
            .filter(|clip| clip.id != tail)
            .map(Clip::span)
            .collect();
        let audio_bounds: Vec<Span> = audio_track.clips.iter().map(Clip::span).collect();
        assert_eq!(
            video_bounds, audio_bounds,
            "linked a/v pieces must land on identical timeline ranges"
        );
        for piece in track.clips.iter().filter(|clip| clip.id != tail) {
            let link = piece
                .link
                .as_ref()
                .unwrap_or_else(|| panic!("piece at {} lost its a/v link", piece.start));
            let (_, counterpart) = h
                .project
                .sequence(&h.seq)
                .unwrap()
                .find_clip(link)
                .expect("the link resolves");
            assert_eq!(
                counterpart.span(),
                piece.span(),
                "a link must point at the piece covering the same range"
            );
        }
    }

    #[test]
    fn cut_words_reports_when_no_filler_is_present() {
        let mut h = harness();
        let asset = h.asset("clean.mp4", 5);
        save_transcript(
            &h,
            &asset,
            vec![word("all", (0, 1), (1, 1)), word("clear", (1, 1), (2, 1))],
        );
        let v1 = h.track("V1", TrackKind::Video);
        let clip = h.place(&v1, &asset, 0, 5);
        let before = h.clip(&clip).duration;

        let mut cx = OpCx::new(&h.paths, &h.assets);
        let effect = TranscriptCutWords
            .apply(
                &mut h.project,
                serde_json::json!({ "target": clip.as_str() }),
                &mut cx,
            )
            .unwrap();
        assert_eq!(effect.warnings.len(), 1);
        assert_eq!(effect.warnings[0].code, "no-fillers");
        assert_eq!(h.clip(&clip).duration, before, "nothing may be cut");
    }

    #[test]
    fn pad_widens_a_cut_and_is_clamped_to_the_clip() {
        let mut h = harness();
        let asset = h.asset("talk.mp4", 5);
        // A filler right at the head of the clip: a full second of padding in front of it
        // has nowhere to go.
        save_transcript(
            &h,
            &asset,
            vec![
                word("um", (0, 1), (3, 10)),
                word("hello", (1, 1), (3, 2)),
            ],
        );
        let v1 = h.track("V1", TrackKind::Video);
        let clip = h.place(&v1, &asset, 0, 5);

        let mut cx = OpCx::new(&h.paths, &h.assets);
        let effect = TranscriptCutWords
            .apply(
                &mut h.project,
                serde_json::json!({ "target": clip.as_str(), "pad": "1.0" }),
                &mut cx,
            )
            .unwrap();
        let data = effect.data.unwrap();
        assert_eq!(
            data["removed"], "13/10",
            "0.3 s of 'um' plus 1 s of trailing pad; the leading pad is clipped off: {data}"
        );

        let track = h.track_of(&v1);
        assert_eq!(track.clips.len(), 1);
        assert_eq!(
            track.clips[0].start,
            Time::ZERO,
            "padding past the head of the clip must not push material before zero"
        );
        assert_eq!(track.clips[0].duration, Time::new(37, 10).unwrap());
        assert_eq!(
            track.clips[0].source_in,
            Time::new(13, 10).unwrap(),
            "the surviving frames are the ones after the padded cut"
        );
    }

    #[test]
    fn keep_phrases_keeps_only_the_matching_ranges() {
        let mut h = harness();
        let asset = h.asset("talk.mp4", 10);
        save_transcript(&h, &asset, fixture_words());
        let v1 = h.track("V1", TrackKind::Video);
        let clip = h.place(&v1, &asset, 0, 10);

        let mut cx = OpCx::new(&h.paths, &h.assets);
        let effect = TranscriptKeepPhrases
            .apply(
                &mut h.project,
                serde_json::json!({ "target": clip.as_str(), "phrases": "our pricing,billing" }),
                &mut cx,
            )
            .unwrap();
        let data = effect.data.expect("keep-phrases reports the kept ranges");

        // "our pricing" spans 2.5–3.5 and "Billing" spans 5.0–5.5: 1.5 s in total.
        let kept = Time::parse(data["keptTotal"].as_str().unwrap()).unwrap();
        assert_eq!(kept, Time::new(3, 2).unwrap(), "{data}");
        assert_eq!(
            h.project.sequence(&h.seq).unwrap().duration(),
            kept,
            "the timeline is exactly the sum of the kept spans"
        );
        let track = h.track_of(&v1);
        assert_eq!(track.clips.len(), 2, "one piece per kept range");
        assert_eq!(
            track.gaps(kept),
            Vec::new(),
            "the pieces are butted together, with no black between them"
        );
        // The second piece still shows the material it showed before: 5.0 s into the source.
        assert_eq!(track.clips[1].source_in, Time::from_secs(5));
    }

    #[test]
    fn keep_phrases_refuses_a_phrase_that_matches_nothing() {
        let mut h = harness();
        let asset = h.asset("talk.mp4", 10);
        save_transcript(&h, &asset, fixture_words());
        let v1 = h.track("V1", TrackKind::Video);
        let clip = h.place(&v1, &asset, 0, 10);
        let before = h.clip(&clip).duration;

        let mut cx = OpCx::new(&h.paths, &h.assets);
        let error = TranscriptKeepPhrases
            .apply(
                &mut h.project,
                serde_json::json!({ "target": clip.as_str(), "phrases": "pricing,refunds" }),
                &mut cx,
            )
            .unwrap_err();
        assert_eq!(error.exit_code(), dvs_core::error::exit::NO_MATCH);
        assert_eq!(
            h.clip(&clip).duration,
            before,
            "a failed op must not have edited anything"
        );
    }

    #[test]
    fn import_accepts_whisper_cpp_whisperx_and_plain_word_json() {
        let mut h = harness();
        let asset = h.asset("talk.mp4", 5);

        let plain = serde_json::json!([
            { "text": "hello", "start": 0.0, "end": 0.5 },
            { "text": "world", "start": 0.5, "end": 1.25 }
        ]);
        let whisper_cpp = serde_json::json!({
            "model": { "type": "base" },
            "result": { "language": "en" },
            "transcription": [
                { "timestamps": { "from": "00:00:00,000", "to": "00:00:00,500" },
                  "offsets": { "from": 0, "to": 500 }, "text": " hello" },
                { "timestamps": { "from": "00:00:00,500", "to": "00:00:01,250" },
                  "offsets": { "from": 500, "to": 1250 }, "text": " world" }
            ]
        });
        let whisperx = serde_json::json!({
            "language": "en",
            "segments": [
                { "start": 0.0, "end": 1.25, "text": "hello world", "words": [
                    { "word": "hello", "start": 0.0, "end": 0.5 },
                    { "word": "world", "start": 0.5, "end": 1.25 }
                ]}
            ]
        });

        let mut produced = Vec::new();
        for shape in [&plain, &whisper_cpp, &whisperx] {
            let mut cx = OpCx::new(&h.paths, &h.assets);
            TranscriptImport
                .apply(
                    &mut h.project,
                    serde_json::json!({
                        "asset": asset.as_str(),
                        "words": shape,
                        "language": "en"
                    }),
                    &mut cx,
                )
                .unwrap();
            produced.push(Transcript::load(&h.paths, &asset).unwrap());
        }

        assert_eq!(
            produced[0], produced[1],
            "whisper.cpp millisecond offsets must land on the same exact times as decimal seconds"
        );
        assert_eq!(produced[1], produced[2], "WhisperX words must agree too");
        assert_eq!(produced[0].words.len(), 2);
        assert_eq!(produced[0].words[0].text, "hello");
        assert_eq!(produced[0].words[1].end, Time::new(5, 4).unwrap());
    }

    #[test]
    fn import_from_a_file_reports_the_span_and_rejects_unknown_json() {
        let mut h = harness();
        let asset = h.asset("talk.mp4", 5);
        let path = h.paths.root().join("words.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!([
                { "text": "one", "start": "1/2", "end": "3/2" }
            ]))
            .unwrap(),
        )
        .unwrap();

        let mut cx = OpCx::new(&h.paths, &h.assets);
        let effect = TranscriptImport
            .apply(
                &mut h.project,
                serde_json::json!({ "asset": asset.as_str(), "path": path.display().to_string() }),
                &mut cx,
            )
            .unwrap();
        let data = effect.data.unwrap();
        assert_eq!(data["words"], 1);
        assert_eq!(data["covered"], "1/1");
        assert_eq!(data["language"], "und", "no file, no claim: {data}");

        let mut cx = OpCx::new(&h.paths, &h.assets);
        let error = TranscriptImport
            .apply(
                &mut h.project,
                serde_json::json!({ "asset": asset.as_str(), "words": { "nonsense": true } }),
                &mut cx,
            )
            .unwrap_err();
        assert_eq!(error.exit_code(), dvs_core::error::exit::BAD_ARGS);
    }

    #[test]
    fn find_reports_source_and_timeline_occurrences() {
        let mut h = harness();
        let asset = h.asset("talk.mp4", 10);
        save_transcript(&h, &asset, fixture_words());
        let v1 = h.track("V1", TrackKind::Video);
        // A clip starting 2 s into the source and placed at timeline 100.
        let clip = h.place(&v1, &asset, 100, 4);
        {
            let sequence = h.project.sequence_mut(&h.seq).unwrap();
            let (track, index) = sequence.find_clip_mut(&clip).unwrap();
            track.clips[index].source_in = Time::from_secs(2);
        }

        let mut cx = OpCx::new(&h.paths, &h.assets);
        let effect = TranscriptFind
            .apply(
                &mut h.project,
                serde_json::json!({ "asset": asset.as_str(), "phrase": "pricing" }),
                &mut cx,
            )
            .unwrap();
        let data = effect.data.unwrap();
        assert_eq!(data["occurrences"]["source"], 1);
        assert_eq!(data["occurrences"]["timeline"], 1);
        assert_eq!(data["source"][0]["start"], "3/1");
        assert_eq!(
            data["timeline"][0]["start"], "101/1",
            "source 3 s is timeline 101 s for a clip trimmed to start at source 2 s: {data}"
        );
        assert_eq!(data["timeline"][0]["clip"], clip.to_string());

        // An asset nobody has placed yet still answers: this is the question an agent asks
        // before it cuts anything.
        let spare = h.asset("spare.mp4", 10);
        save_transcript(&h, &spare, fixture_words());
        let mut cx = OpCx::new(&h.paths, &h.assets);
        let effect = TranscriptFind
            .apply(
                &mut h.project,
                serde_json::json!({ "asset": spare.as_str(), "phrase": "pricing" }),
                &mut cx,
            )
            .unwrap();
        let data = effect.data.unwrap();
        assert_eq!(data["occurrences"]["source"], 1);
        assert_eq!(data["occurrences"]["timeline"], 0, "{data}");
    }

    #[test]
    fn generate_builds_cues_under_the_ceiling_and_reports_the_real_maximum() {
        let mut h = harness();
        let asset = h.asset("talk.mp4", 30);
        // Sixty words at a realistic 0.34 s each.
        let words: Vec<Word> = (0..60)
            .map(|index| {
                let start = Time::new(index * 34, 100).unwrap();
                let end = Time::new(index * 34 + 34, 100).unwrap();
                let text = if index % 10 == 9 {
                    format!("word{index}.")
                } else {
                    format!("word{index}")
                };
                Word::new(text, start, end, 1.0)
            })
            .collect();
        save_transcript(&h, &asset, words);
        let v1 = h.track("V1", TrackKind::Video);
        h.place(&v1, &asset, 0, 21);

        let mut cx = OpCx::new(&h.paths, &h.assets);
        let effect = CaptionGenerate
            .apply(
                &mut h.project,
                serde_json::json!({ "asset": asset.as_str(), "max-cps": 20 }),
                &mut cx,
            )
            .unwrap();
        let data = effect.data.expect("generate reports what it produced");
        assert!(data["cues"].as_u64().unwrap() >= 2, "{data}");
        let reported = data["maxCps"].as_f64().unwrap();

        let caption = h
            .project
            .sequence(&h.seq)
            .unwrap()
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Caption)
            .expect("a caption track is created when there is none");
        assert_eq!(caption.cues.len(), data["cues"].as_u64().unwrap() as usize);
        assert!(caption.style.is_some(), "the track carries the style used");

        let observed = max_cps(&caption.cues);
        assert!(
            (observed - reported).abs() < 1e-9,
            "the reported maximum must be the real one: {reported} vs {observed}"
        );
        for cue in &caption.cues {
            assert!(
                cue.chars_per_second() <= 20.0,
                "cue '{}' runs at {:.2} cps",
                cue.text,
                cue.chars_per_second()
            );
            assert!(cue.lines() <= 2);
        }
        for pair in caption.cues.windows(2) {
            assert!(pair[0].span.end <= pair[1].span.start);
        }
        assert!(
            effect.warnings.is_empty(),
            "nothing is too fast here: {:?}",
            effect.warnings
        );

        // A ceiling the speech cannot meet is reported, not quietly ignored: the words were
        // said at ~17 cps, so a 6 cps ceiling is unreachable and the agent has to hear it.
        let mut cx = OpCx::new(&h.paths, &h.assets);
        let strict = CaptionGenerate
            .apply(
                &mut h.project,
                serde_json::json!({ "asset": asset.as_str(), "max-cps": 6 }),
                &mut cx,
            )
            .unwrap();
        let data = strict.data.expect("generate reports its ceiling");
        assert_eq!(data["ceiling"], 6.0, "--max-cps must override the style: {data}");
        assert!(data["tooFast"].as_u64().unwrap() > 0, "{data}");
        assert!(
            strict
                .warnings
                .iter()
                .any(|warning| warning.code == "caption-too-fast"),
            "{:?}",
            strict.warnings
        );
        assert_eq!(
            h.project.styles.values().next().unwrap().max_cps,
            20.0,
            "a per-run ceiling must not rewrite the stored style"
        );
    }

    #[test]
    fn generating_twice_replaces_rather_than_stacks() {
        let mut h = harness();
        let asset = h.asset("talk.mp4", 10);
        save_transcript(&h, &asset, fixture_words());
        let v1 = h.track("V1", TrackKind::Video);
        h.place(&v1, &asset, 0, 10);

        let mut first = 0usize;
        for _ in 0..2 {
            let mut cx = OpCx::new(&h.paths, &h.assets);
            let effect = CaptionGenerate
                .apply(
                    &mut h.project,
                    serde_json::json!({ "asset": asset.as_str() }),
                    &mut cx,
                )
                .unwrap();
            let count = effect.data.unwrap()["cues"].as_u64().unwrap() as usize;
            if first == 0 {
                first = count;
            } else {
                assert_eq!(count, first);
            }
        }
        let caption = h
            .project
            .sequence(&h.seq)
            .unwrap()
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Caption)
            .unwrap();
        assert_eq!(
            caption.cues.len(),
            first,
            "the second run replaced the first set instead of stacking on it"
        );
        assert_eq!(
            h.project
                .sequence(&h.seq)
                .unwrap()
                .tracks
                .iter()
                .filter(|track| track.kind == TrackKind::Caption)
                .count(),
            1,
            "and it reused the track it made"
        );
    }

    #[test]
    fn generate_without_a_transcript_says_so() {
        let mut h = harness();
        let asset = h.asset("talk.mp4", 10);
        let v1 = h.track("V1", TrackKind::Video);
        h.place(&v1, &asset, 0, 10);
        let mut cx = OpCx::new(&h.paths, &h.assets);
        let error = CaptionGenerate
            .apply(
                &mut h.project,
                serde_json::json!({ "asset": asset.as_str() }),
                &mut cx,
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("transcript.import"),
            "the error must name a way forward: {error}"
        );
    }

    #[test]
    fn style_creates_a_default_and_sets_fields() {
        let mut h = harness();
        let cc = h.track("CC1", TrackKind::Caption);
        let mut cx = OpCx::new(&h.paths, &h.assets);
        let effect = CaptionSetStyle
            .apply(
                &mut h.project,
                serde_json::json!({
                    "track": cc.as_str(),
                    "font": "Inter",
                    "size-px": 64,
                    "position": "top",
                    "outline": "none",
                    "max-lines": 1
                }),
                &mut cx,
            )
            .unwrap();
        assert_eq!(effect.created.len(), 1, "the default style is created on demand");

        let style_id = h.track_of(&cc).style.clone().expect("the track is styled");
        let style = &h.project.styles[&style_id];
        assert_eq!(style.font, "Inter");
        assert_eq!(style.size_px, 64.0);
        assert_eq!(style.position, CaptionPosition::Top);
        assert_eq!(style.outline, None, "'none' clears a color");
        assert_eq!(style.max_lines, 1);

        let mut cx = OpCx::new(&h.paths, &h.assets);
        let error = CaptionSetStyle
            .apply(
                &mut h.project,
                serde_json::json!({ "safe-area": 1.5 }),
                &mut cx,
            )
            .unwrap_err();
        assert_eq!(error.exit_code(), dvs_core::error::exit::BAD_ARGS);
    }

    #[test]
    fn style_with_nothing_to_set_is_an_argument_error() {
        let mut h = harness();
        let mut cx = OpCx::new(&h.paths, &h.assets);
        let error = CaptionSetStyle
            .apply(&mut h.project, serde_json::json!({}), &mut cx)
            .unwrap_err();
        assert_eq!(error.exit_code(), dvs_core::error::exit::BAD_ARGS);
        assert!(h.project.styles.is_empty(), "and it created nothing");
    }

    #[test]
    fn captions_round_trip_through_srt_and_are_removable() {
        let mut h = harness();
        let cc = h.track("CC1", TrackKind::Caption);
        {
            let track = h.project.sequence_mut(&h.seq).unwrap().track_mut(&cc).unwrap();
            track.cues = vec![
                CaptionCue {
                    id: CueId::new(),
                    span: Span::new(Time::ZERO, Time::from_secs(2)),
                    text: "first cue".into(),
                    style: None,
                },
                CaptionCue {
                    id: CueId::new(),
                    span: Span::new(Time::from_secs(3), Time::from_secs(5)),
                    text: "second\ncue".into(),
                    style: None,
                },
            ];
        }
        let out = h.paths.root().join("subs.srt");

        let mut cx = OpCx::new(&h.paths, &h.assets);
        CaptionExport
            .apply(
                &mut h.project,
                serde_json::json!({ "out": out.display().to_string() }),
                &mut cx,
            )
            .unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("first cue"), "{text}");

        let mut cx = OpCx::new(&h.paths, &h.assets);
        CaptionRemove
            .apply(
                &mut h.project,
                serde_json::json!({ "track": cc.as_str() }),
                &mut cx,
            )
            .unwrap();
        assert!(h.track_of(&cc).cues.is_empty());

        let mut cx = OpCx::new(&h.paths, &h.assets);
        let effect = CaptionImport
            .apply(
                &mut h.project,
                serde_json::json!({ "path": out.display().to_string() }),
                &mut cx,
            )
            .unwrap();
        let data = effect.data.unwrap();
        assert_eq!(data["cues"], 2, "{data}");
        assert_eq!(data["format"], "srt");
        let cues = &h.track_of(&cc).cues;
        assert_eq!(cues[0].text, "first cue");
        assert_eq!(cues[1].text, "second\ncue", "line breaks survive the round trip");
        assert_eq!(cues[1].span, Span::new(Time::from_secs(3), Time::from_secs(5)));
    }

    #[test]
    fn a_sub_frame_cue_survives_the_snap_to_the_frame_grid() {
        let mut h = harness();
        let cc = h.track("CC1", TrackKind::Caption);
        let path = h.paths.root().join("tiny.srt");
        // Ten milliseconds: both ends round to frame 0 at 30 fps.
        std::fs::write(
            &path,
            "1\n00:00:00,000 --> 00:00:00,010\nblink\n\n",
        )
        .unwrap();

        let mut cx = OpCx::new(&h.paths, &h.assets);
        CaptionImport
            .apply(
                &mut h.project,
                serde_json::json!({ "path": path.display().to_string(), "track": cc.as_str() }),
                &mut cx,
            )
            .unwrap();
        let cues = &h.track_of(&cc).cues;
        assert_eq!(cues.len(), 1, "a short cue must not be snapped out of existence");
        assert_eq!(
            cues[0].span,
            Span::new(Time::ZERO, Time::new(1, 100).unwrap()),
            "its exact times are kept rather than collapsed onto one frame"
        );
    }

    #[test]
    fn caption_remove_by_selector_takes_only_the_matching_cues() {
        let mut h = harness();
        let cc = h.track("CC1", TrackKind::Caption);
        {
            let track = h.project.sequence_mut(&h.seq).unwrap().track_mut(&cc).unwrap();
            track.cues = vec![
                CaptionCue {
                    id: CueId::new(),
                    span: Span::new(Time::ZERO, Time::from_secs(2)),
                    text: "about pricing".into(),
                    style: None,
                },
                CaptionCue {
                    id: CueId::new(),
                    span: Span::new(Time::from_secs(2), Time::from_secs(4)),
                    text: "about nothing".into(),
                    style: None,
                },
            ];
        }
        let mut cx = OpCx::new(&h.paths, &h.assets);
        CaptionRemove
            .apply(
                &mut h.project,
                serde_json::json!({ "target": "cue[text*=pricing]" }),
                &mut cx,
            )
            .unwrap();
        let cues = &h.track_of(&cc).cues;
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "about nothing");
    }

    #[test]
    fn caption_ops_refuse_a_non_caption_track() {
        let mut h = harness();
        let v1 = h.track("V1", TrackKind::Video);
        let asset = h.asset("talk.mp4", 5);
        save_transcript(&h, &asset, fixture_words());
        h.place(&v1, &asset, 0, 5);
        let mut cx = OpCx::new(&h.paths, &h.assets);
        let error = CaptionGenerate
            .apply(
                &mut h.project,
                serde_json::json!({ "asset": asset.as_str(), "track": "V1" }),
                &mut cx,
            )
            .unwrap_err();
        assert_eq!(error.exit_code(), dvs_core::error::exit::BAD_ARGS);
        assert!(error.to_string().contains("caption track"), "{error}");
    }

    #[test]
    fn a_locked_track_refuses_a_transcript_cut() {
        let mut h = harness();
        let asset = h.asset("talk.mp4", 10);
        save_transcript(&h, &asset, fixture_words());
        let v1 = h.track("V1", TrackKind::Video);
        let clip = h.place(&v1, &asset, 0, 10);
        h.project
            .sequence_mut(&h.seq)
            .unwrap()
            .track_mut(&v1)
            .unwrap()
            .locked = true;

        let mut cx = OpCx::new(&h.paths, &h.assets);
        let error = TranscriptCutWords
            .apply(
                &mut h.project,
                serde_json::json!({ "target": clip.as_str() }),
                &mut cx,
            )
            .unwrap_err();
        assert!(error.to_string().contains("lock"), "{error}");
        assert_eq!(h.clip(&clip).duration, Time::from_secs(10));
    }

    #[test]
    fn a_dry_run_reports_the_cuts_without_making_them() {
        let mut h = harness();
        let asset = h.asset("talk.mp4", 10);
        save_transcript(&h, &asset, fixture_words());
        let v1 = h.track("V1", TrackKind::Video);
        let clip = h.place(&v1, &asset, 0, 10);

        let mut cx = OpCx::new(&h.paths, &h.assets).dry_run(true);
        let effect = TranscriptCutWords
            .apply(
                &mut h.project,
                serde_json::json!({ "target": clip.as_str(), "min-gap": "0.5" }),
                &mut cx,
            )
            .unwrap();
        assert_eq!(effect.data.unwrap()["cutCount"], 2);
        assert_eq!(
            h.clip(&clip).duration,
            Time::from_secs(10),
            "a dry run must not touch the document"
        );
    }

    #[test]
    fn transcript_run_without_the_whisper_feature_points_at_import() {
        let mut h = harness();
        let asset = h.asset("talk.mp4", 3);
        if Toolchain::shared().is_err() {
            return;
        }
        let mut cx = OpCx::new(&h.paths, &h.assets);
        let error = TranscriptRun
            .apply(
                &mut h.project,
                // An explicit model keeps this test independent of the environment.
                serde_json::json!({ "asset": asset.as_str(), "model": "base.en" }),
                &mut cx,
            )
            .unwrap_err();
        assert_eq!(
            error.exit_code(),
            dvs_core::error::exit::TOOL_MISSING,
            "a build without a transcriber is a tool problem: {error}"
        );
        let message = error.to_string();
        if whisper::AVAILABLE {
            assert!(
                message.contains("model") || message.contains("ffmpeg"),
                "with the feature on, the failure must be about the model or the media: {message}"
            );
        } else {
            assert!(message.contains("--features whisper"), "{message}");
            assert!(message.contains("transcript.import"), "{message}");
        }
    }

    #[test]
    fn a_cut_warns_when_caption_cues_would_drift() {
        let mut h = harness();
        let asset = h.asset("talk.mp4", 10);
        save_transcript(&h, &asset, fixture_words());
        let v1 = h.track("V1", TrackKind::Video);
        let clip = h.place(&v1, &asset, 0, 10);
        let cc = h.track("CC1", TrackKind::Caption);
        {
            let track = h.project.sequence_mut(&h.seq).unwrap().track_mut(&cc).unwrap();
            track.cues.push(CaptionCue {
                id: CueId::new(),
                span: Span::new(Time::from_secs(4), Time::from_secs(6)),
                text: "later".into(),
                style: None,
            });
        }
        let mut cx = OpCx::new(&h.paths, &h.assets);
        let effect = TranscriptCutWords
            .apply(
                &mut h.project,
                serde_json::json!({ "target": clip.as_str() }),
                &mut cx,
            )
            .unwrap();
        assert!(
            effect
                .warnings
                .iter()
                .any(|warning| warning.code == "caption-desync"),
            "cues after the cut do not ripple, so the op has to say so: {:?}",
            effect.warnings
        );
    }

    #[test]
    fn complement_covers_exactly_what_keep_does_not() {
        let whole = Span::new(Time::ZERO, Time::from_secs(10));
        let keep = vec![
            Span::new(Time::from_secs(2), Time::from_secs(3)),
            Span::new(Time::from_secs(6), Time::from_secs(10)),
        ];
        assert_eq!(
            complement(whole, &keep),
            vec![
                Span::new(Time::ZERO, Time::from_secs(2)),
                Span::new(Time::from_secs(3), Time::from_secs(6)),
            ]
        );
        assert!(complement(whole, &[whole]).is_empty());
    }
}
