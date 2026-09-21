//! The audio ops: what an agent can do to a mix without hearing it.
//!
//! Two rules shape this module. First, an op never invents a document concept to express an
//! edit: `audio.mute-range` splits the clip and disables the middle piece, because a
//! "muted region" field would be a second, parallel way to say "this does not play" that
//! every renderer, exporter and lint would then have to learn. Second, an op that changes
//! something measurable reports the measurement. `audio.normalize` puts the loudness it
//! measured and the loudness it targeted into `OpEffect.data` — the operator cannot listen,
//! so the number *is* the result.
//!
//! Attack and release times are deliberately **not** snapped to the frame grid. Every other
//! time argument here is, because a cut that is not on a frame boundary cannot be rendered;
//! but a ducking envelope is a sidechain time constant measured in samples, and rounding
//! 250 ms up to 267 ms on a 30 fps timeline would be a frame grid leaking into a place it
//! has no meaning.

use crate::loudness::{analyze_loudness, normalize_gain_db};
use crate::mix::{mix_tracks, MixSpec};
use crate::silence::{detect_silence, detect_speech};
use dvs_core::error::{Error, Result};
use dvs_core::ids::{ClipId, SequenceId, TrackId};
use dvs_core::op::{args, Op, OpCx, OpEffect, Registry};
use dvs_core::ops::util;
use dvs_core::project::{Clip, Ducking, Project, Sequence, Source, Track, TrackKind};
use dvs_core::selector;
use dvs_core::time::{Fps, Span, Time};
use dvs_media::toolchain::Toolchain;

/// Default delivery target, in LUFS. −14 is what YouTube, Spotify and Apple Music all
/// normalize to, so it is the least surprising thing to hand an agent that did not say.
const DEFAULT_LUFS: f64 = -14.0;

/// Default silence threshold for `seq.trim-silence`, in dBFS. Room tone in a decent
/// recording sits near −50; dialogue peaks near −12.
const DEFAULT_SILENCE_DB: f64 = -40.0;

/// Register every op this crate owns. `seq.trim-silence` lives here rather than in
/// `dvs-core` because it needs a mix to decide where the silence is.
pub fn register(registry: &mut Registry) {
    registry.register(Gain);
    registry.register(Pan);
    registry.register(Fade);
    registry.register(Duck);
    registry.register(Normalize);
    registry.register(MuteRange);
    registry.register(Detach);
    registry.register(TrimSilence);
}

/// Clips named by `--target`, or every clip of `--track`.
fn target_clips(
    project: &Project,
    sequence: &SequenceId,
    arguments: &serde_json::Value,
) -> Result<Vec<(TrackId, ClipId)>> {
    if let Some(text) = args::opt_str(arguments, "target") {
        return selector::resolve_clips(project, sequence, text);
    }
    let Some(text) = args::opt_str(arguments, "track") else {
        return Err(Error::bad_args("name the clips with 'target' or a whole 'track'"));
    };
    let track_id = selector::resolve_track(project, sequence, text)?;
    let track = project.sequence(sequence)?.track(&track_id)?;
    Ok(track
        .clips
        .iter()
        .map(|clip| (track_id.clone(), clip.id.clone()))
        .collect())
}

/// Mutable access to one clip, with the track's lock honoured first.
fn clip_mut<'s>(
    sequence: &'s mut Sequence,
    track: &TrackId,
    clip: &ClipId,
) -> Result<&'s mut Clip> {
    let track = sequence.track_mut(track)?;
    util::assert_unlocked(track)?;
    let index = util::clip_index(track, clip)?;
    Ok(&mut track.clips[index])
}

/// Split the clip at `index` at a timeline instant, returning the index of the tail piece.
///
/// `dvs-core` exposes splitting only as the `clip.split` op, and an op cannot call another
/// op, so the arithmetic lives here too. It is the arithmetic that matters: a reversed clip
/// reads its source backwards, so the *tail* of the timeline keeps the original in-point
/// and the *head* is the part that moves — getting that backwards silently swaps the two
/// halves of every reversed split.
fn split_at(track: &mut Track, index: usize, at: Time) -> Option<usize> {
    let clip = &track.clips[index];
    if at <= clip.start || at >= clip.end() {
        return None;
    }
    let local = at - clip.start;
    let rest = clip.duration - local;
    let mut tail = clip.clone();
    tail.id = ClipId::new();
    tail.start = at;
    tail.duration = rest;
    if !clip.reverse {
        tail.source_in = clip.source_in + local * clip.speed;
    }
    // A fade belongs to the edge it is on: the head keeps the fade in, the tail the fade
    // out. A transition into the clip likewise stays with the head.
    tail.fade_in = Time::ZERO;
    tail.transition_in = None;
    // The a/v link is one-to-one; a split leaves two clips where one partner expects one,
    // so the new piece starts unlinked rather than claiming a partner it does not have.
    tail.link = None;
    for keys in tail.keyframes.values_mut() {
        for key in keys.iter_mut() {
            key.at = key.at - local;
        }
    }

    let head = &mut track.clips[index];
    head.duration = local;
    head.fade_out = Time::ZERO;
    if head.reverse {
        // Reversed: the timeline tail plays the earliest source, so the head's material
        // begins where the tail's ends.
        head.source_in = head.source_in + rest * head.speed;
    }
    head.fade_in = head.fade_in.min(head.duration);
    tail.fade_out = tail.fade_out.min(tail.duration);

    track.clips.insert(index + 1, tail);
    Some(index + 1)
}


/// Remove a timeline range from a track and close the hole.
///
/// Clips inside the range go; clips that straddle it are trimmed or split; everything after
/// slides back by the range's length. Caption cues follow the same rule, because a cue left
/// where it was after a ripple is a subtitle that now belongs to a different sentence.
fn delete_range_ripple(track: &mut Track, range: Span) -> Vec<String> {
    let mut removed = Vec::new();
    let mut index = 0;
    while index < track.clips.len() {
        let span = track.clips[index].span();
        if span.intersect(&range).is_none() {
            index += 1;
            continue;
        }
        if span.start >= range.start && span.end <= range.end {
            removed.push(track.clips[index].id.to_string());
            track.clips.remove(index);
            continue;
        }
        if span.start < range.start && span.end > range.end {
            split_at(track, index, range.start);
            // The middle piece is now at index + 1 and runs to the original end.
            split_at(track, index + 1, range.end);
            removed.push(track.clips[index + 1].id.to_string());
            track.clips.remove(index + 1);
            index += 2;
            continue;
        }
        if span.start < range.start {
            track.clips[index].duration = range.start - span.start;
            let clip = &mut track.clips[index];
            clip.fade_out = clip.fade_out.min(clip.duration);
        } else {
            let cut = range.end - span.start;
            let clip = &mut track.clips[index];
            if !clip.reverse {
                clip.source_in = clip.source_in + cut * clip.speed;
            }
            clip.start = clip.start + cut;
            clip.duration = clip.duration - cut;
            clip.fade_in = clip.fade_in.min(clip.duration);
        }
        index += 1;
    }

    let shift = range.duration();
    util::ripple_after(track, range.end, -shift);

    track.cues.retain(|cue| {
        !(cue.span.start >= range.start && cue.span.end <= range.end)
    });
    for cue in &mut track.cues {
        cue.span.start = shift_time(cue.span.start, range, shift);
        cue.span.end = shift_time(cue.span.end, range, shift);
    }
    track.cues.retain(|cue| !cue.span.is_empty());
    util::resort(track);
    removed
}

/// Where an instant lands once `range` is cut out of the timeline.
fn shift_time(at: Time, range: Span, shift: Time) -> Time {
    if at <= range.start {
        at
    } else if at >= range.end {
        at - shift
    } else {
        range.start
    }
}

/// Snap a detected range inward to the frame grid.
///
/// Inward and not nearest: a cut point rounded outward would eat the first frame of the
/// word after the pause, and a clipped consonant is far more noticeable than 16 ms of extra
/// silence.
fn snap_inward(span: Span, fps: Fps) -> Span {
    Span::new(
        Time::from_frames(span.start.frame_ceil(fps), fps),
        Time::from_frames(span.end.frame_floor(fps), fps),
    )
}

/// Tracks that can carry samples.
fn audio_tracks(sequence: &Sequence) -> Vec<TrackId> {
    sequence
        .tracks
        .iter()
        .filter(|track| track.kind != TrackKind::Caption && !track.clips.is_empty())
        .map(|track| track.id.clone())
        .collect()
}

struct Gain;

impl Op for Gain {
    fn id(&self) -> &'static str {
        "audio.gain"
    }

    fn about(&self) -> &'static str {
        "set the level of clips or of a whole track, in decibels"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["gain-db"],
            "properties": {
                "target": { "type": "string", "description": "clip selector, e.g. '#music' or 'clip[track=A1]'" },
                "track": { "type": "string", "description": "track selector; sets the track trim instead of clip gains" },
                "gain-db": { "type": "number", "description": "level in dB; -6 halves the amplitude" },
                "relative": { "type": "boolean", "description": "add to the existing gain instead of replacing it" }
            }
        })
    }

    fn apply(&self, project: &mut Project, arguments: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&arguments, &["target", "track", "gain-db", "relative"])?;
        let sequence = cx.sequence(project)?;
        let gain = args::f64_field(&arguments, "gain-db")? as f32;
        let relative = args::opt_bool(&arguments, "relative")?.unwrap_or(false);
        let mut effect = OpEffect::new();

        if args::opt_str(&arguments, "target").is_none() {
            let text = args::str_field(&arguments, "track")?;
            let track_id = selector::resolve_track(project, &sequence, text)?;
            let seq = project.sequence_mut(&sequence)?;
            let track = seq.track_mut(&track_id)?;
            util::assert_unlocked(track)?;
            if !cx.dry_run {
                track.gain_db = if relative { track.gain_db + gain } else { gain };
            }
            return Ok(effect.changed(track_id));
        }

        let targets = target_clips(project, &sequence, &arguments)?;
        let seq = project.sequence_mut(&sequence)?;
        for (track, clip) in targets {
            let clip = clip_mut(seq, &track, &clip)?;
            if !cx.dry_run {
                clip.gain_db = if relative { clip.gain_db + gain } else { gain };
            }
            effect = effect.changed(clip.id.clone());
        }
        Ok(effect)
    }
}

struct Pan;

impl Op for Pan {
    fn id(&self) -> &'static str {
        "audio.pan"
    }

    fn about(&self) -> &'static str {
        "place clips or a whole track in the stereo field, -1 left to +1 right"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["pan"],
            "properties": {
                "target": { "type": "string", "description": "clip selector" },
                "track": { "type": "string", "description": "track selector; sets the track pan instead of clip pans" },
                "pan": { "type": "number", "minimum": -1, "maximum": 1, "description": "-1 hard left, 0 center, +1 hard right" }
            }
        })
    }

    fn apply(&self, project: &mut Project, arguments: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&arguments, &["target", "track", "pan"])?;
        let sequence = cx.sequence(project)?;
        let pan = args::f64_field(&arguments, "pan")?;
        if !(-1.0..=1.0).contains(&pan) {
            return Err(Error::bad_args(format!(
                "pan {pan} is outside -1..1; -1 is hard left and +1 is hard right"
            )));
        }
        let pan = pan as f32;
        let mut effect = OpEffect::new();

        if args::opt_str(&arguments, "target").is_none() {
            let text = args::str_field(&arguments, "track")?;
            let track_id = selector::resolve_track(project, &sequence, text)?;
            let seq = project.sequence_mut(&sequence)?;
            let track = seq.track_mut(&track_id)?;
            util::assert_unlocked(track)?;
            if !cx.dry_run {
                track.pan = pan;
            }
            return Ok(effect.changed(track_id));
        }

        let targets = target_clips(project, &sequence, &arguments)?;
        let seq = project.sequence_mut(&sequence)?;
        for (track, clip) in targets {
            let clip = clip_mut(seq, &track, &clip)?;
            if !cx.dry_run {
                clip.pan = pan;
            }
            effect = effect.changed(clip.id.clone());
        }
        Ok(effect)
    }
}

struct Fade;

impl Op for Fade {
    fn id(&self) -> &'static str {
        "audio.fade"
    }

    fn about(&self) -> &'static str {
        "set the fade in and fade out of clips"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "clip selector" },
                "track": { "type": "string", "description": "track selector; fades every clip on it" },
                "in": { "type": "string", "description": "fade-in length, e.g. '0.5', '250ms', '12f'" },
                "out": { "type": "string", "description": "fade-out length" }
            }
        })
    }

    fn apply(&self, project: &mut Project, arguments: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&arguments, &["target", "track", "in", "out"])?;
        let sequence = cx.sequence(project)?;
        let fps = util::sequence_fps(project, &sequence)?;
        let fade_in = args::opt_time(&arguments, "in", fps)?;
        let fade_out = args::opt_time(&arguments, "out", fps)?;
        if fade_in.is_none() && fade_out.is_none() {
            return Err(Error::bad_args("audio.fade needs 'in', 'out', or both"));
        }
        for (name, value) in [("in", fade_in), ("out", fade_out)] {
            if value.is_some_and(Time::is_negative) {
                return Err(Error::bad_args(format!("fade '{name}' cannot be negative")));
            }
        }
        let targets = target_clips(project, &sequence, &arguments)?;
        let mut effect = OpEffect::new();
        if let Some(requested) = fade_in {
            effect = effect.snap("in", requested, requested.snap(fps), fps);
        }
        if let Some(requested) = fade_out {
            effect = effect.snap("out", requested, requested.snap(fps), fps);
        }

        let seq = project.sequence_mut(&sequence)?;
        for (track, clip) in targets {
            let clip = clip_mut(seq, &track, &clip)?;
            let duration = clip.duration;
            let label = clip.id.to_string();
            let mut too_long = false;
            if let Some(value) = fade_in {
                let snapped = value.snap(fps).min(duration);
                too_long |= snapped < value.snap(fps);
                if !cx.dry_run {
                    clip.fade_in = snapped;
                }
            }
            if let Some(value) = fade_out {
                let snapped = value.snap(fps).min(duration);
                too_long |= snapped < value.snap(fps);
                if !cx.dry_run {
                    clip.fade_out = snapped;
                }
            }
            effect = effect.changed(label.clone());
            if too_long {
                effect = effect.warn(
                    "fade-clamped",
                    label,
                    format!("fade was longer than the clip and was clamped to {duration}"),
                );
            }
        }
        Ok(effect)
    }
}

struct Duck;

impl Op for Duck {
    fn id(&self) -> &'static str {
        "audio.duck"
    }

    fn about(&self) -> &'static str {
        "duck a track under another one, e.g. music under dialogue"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["track", "against"],
            "properties": {
                "track": { "type": "string", "description": "track to duck, e.g. the music bed" },
                "against": { "type": "string", "description": "trigger track, e.g. the dialogue" },
                "by": { "type": "number", "description": "gain reduction in dB while the trigger sounds; default -12" },
                "attack": { "type": "string", "description": "time to reach full reduction; the sidechain looks ahead by this much so the duck is down before the first word" },
                "release": { "type": "string", "description": "time to return to unity after the trigger stops" },
                "threshold": { "type": "number", "description": "trigger level in dBFS; default -30" }
            }
        })
    }

    fn apply(&self, project: &mut Project, arguments: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(
            &arguments,
            &["track", "against", "by", "attack", "release", "threshold"],
        )?;
        let sequence = cx.sequence(project)?;
        let fps = util::sequence_fps(project, &sequence)?;
        let track_id =
            selector::resolve_track(project, &sequence, args::str_field(&arguments, "track")?)?;
        let against =
            selector::resolve_track(project, &sequence, args::str_field(&arguments, "against")?)?;
        if track_id == against {
            let name = project.sequence(&sequence)?.track(&track_id)?.name.clone();
            return Err(Error::bad_args(format!(
                "track '{name}' cannot duck against itself; name the trigger track with 'against'"
            )));
        }
        let by = args::opt_f64(&arguments, "by")?.unwrap_or(-12.0) as f32;
        if by > 0.0 {
            return Err(Error::bad_args(format!(
                "'by' is a gain reduction and must be zero or negative, got {by}"
            )));
        }
        // Envelope constants stay off the frame grid; see the module doc.
        let attack = args::opt_time(&arguments, "attack", fps)?
            .unwrap_or_else(|| Time::new(1, 5).expect("1/5 is valid"));
        let release = args::opt_time(&arguments, "release", fps)?
            .unwrap_or_else(|| Time::new(1, 2).expect("1/2 is valid"));
        if attack.is_negative() || release.is_negative() {
            return Err(Error::bad_args("attack and release cannot be negative"));
        }
        let threshold = args::opt_f64(&arguments, "threshold")?.unwrap_or(-30.0) as f32;

        let ducking = Ducking {
            against: against.clone(),
            by,
            attack,
            release,
            threshold,
        };
        let seq = project.sequence_mut(&sequence)?;
        let track = seq.track_mut(&track_id)?;
        util::assert_unlocked(track)?;
        if track.clips.is_empty() {
            return Err(Error::op(format!(
                "track '{}' has no clips to duck",
                track.name
            )));
        }
        let mut effect = OpEffect::new();
        for clip in &mut track.clips {
            if !cx.dry_run {
                clip.ducking = Some(ducking.clone());
            }
            effect = effect.changed(clip.id.clone());
        }
        Ok(effect.data(serde_json::json!({
            "track": track_id.to_string(),
            "against": against.to_string(),
            "byDb": by,
            "attack": attack.to_string(),
            "release": release.to_string(),
            "thresholdDb": threshold,
        })))
    }
}

struct Normalize;

impl Op for Normalize {
    fn id(&self) -> &'static str {
        "audio.normalize"
    }

    fn about(&self) -> &'static str {
        "measure loudness and write the gain that puts it on a LUFS target"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "track": { "type": "string", "description": "track to normalize; omit to normalize the whole sequence" },
                "lufs": { "type": "number", "description": "target integrated loudness; default -14 (YouTube/Spotify)" }
            }
        })
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        &["ffmpeg"]
    }

    fn apply(&self, project: &mut Project, arguments: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&arguments, &["track", "lufs"])?;
        let sequence_id = cx.sequence(project)?;
        let target = args::opt_f64(&arguments, "lufs")?.unwrap_or(DEFAULT_LUFS);
        let scope = match args::opt_str(&arguments, "track") {
            Some(text) => Some(selector::resolve_track(project, &sequence_id, text)?),
            None => None,
        };

        let sequence = project.sequence(&sequence_id)?;
        let span = Span::new(Time::ZERO, sequence.duration());
        if span.is_empty() {
            return Err(Error::op(
                "the sequence is empty; there is nothing to measure",
            ));
        }
        let spec = MixSpec::of(sequence);
        let tracks: Vec<TrackId> = match &scope {
            Some(track) => vec![track.clone()],
            None => audio_tracks(sequence),
        };
        if tracks.is_empty() {
            return Err(Error::op("no track in this sequence carries audio"));
        }

        let tool = Toolchain::shared()?;
        let mixed = mix_tracks(
            project,
            &sequence_id,
            Some(&tracks),
            span,
            spec,
            tool,
            cx.assets,
            cx.paths,
        )?;
        let measured = analyze_loudness(&mixed, spec.rate, spec.channels)?;
        let delta = normalize_gain_db(measured.integrated_lufs, target);

        if measured.integrated_lufs <= crate::SILENCE_FLOOR_DB {
            return Err(Error::op(
                "the selected audio is silent; a gain that normalizes silence does not exist",
            ));
        }

        let mut effect = OpEffect::new();
        let seq = project.sequence_mut(&sequence_id)?;
        for id in &tracks {
            let track = seq.track_mut(id)?;
            util::assert_unlocked(track)?;
            if !cx.dry_run {
                track.gain_db += delta;
            }
            effect = effect.changed(id.clone());
        }

        let projected_peak = measured.true_peak_db + f64::from(delta);
        if projected_peak > -1.0 {
            effect = effect.warn(
                "true-peak",
                scope
                    .as_ref()
                    .map(TrackId::to_string)
                    .unwrap_or_else(|| sequence_id.to_string()),
                format!(
                    "after {delta:+.2} dB the true peak would be {projected_peak:.2} dBTP, above the -1 dBTP delivery ceiling"
                ),
            );
        }
        Ok(effect.data(serde_json::json!({
            "measuredLufs": round2(measured.integrated_lufs),
            "targetLufs": target,
            "gainDb": round2(f64::from(delta)),
            "resultingLufs": round2(measured.integrated_lufs + f64::from(delta)),
            "truePeakDb": round2(measured.true_peak_db),
            "projectedTruePeakDb": round2(projected_peak),
            "lra": round2(measured.lra),
            "shortTermMinLufs": round2(measured.short_term_min_lufs),
            "clippedSamples": measured.clipped_samples,
            "tracks": tracks.iter().map(TrackId::to_string).collect::<Vec<_>>(),
        })))
    }
}

/// Two decimals is the resolution a loudness meter is trusted to; more digits invite an
/// agent to chase noise between runs.
fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

struct MuteRange;

impl Op for MuteRange {
    fn id(&self) -> &'static str {
        "audio.mute-range"
    }

    fn about(&self) -> &'static str {
        "silence a time range on a track by splitting the clip and disabling the middle"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["track", "start", "end"],
            "properties": {
                "track": { "type": "string", "description": "track selector" },
                "start": { "type": "string", "description": "range start on the timeline" },
                "end": { "type": "string", "description": "range end on the timeline" }
            }
        })
    }

    fn apply(&self, project: &mut Project, arguments: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&arguments, &["track", "start", "end"])?;
        let sequence = cx.sequence(project)?;
        let fps = util::sequence_fps(project, &sequence)?;
        let track_id =
            selector::resolve_track(project, &sequence, args::str_field(&arguments, "track")?)?;
        let start = args::time_field(&arguments, "start", fps)?;
        let end = args::time_field(&arguments, "end", fps)?;
        let range = util::snap_span(Span::new(start, end), fps);
        if range.is_empty() {
            return Err(Error::bad_args(format!(
                "range {start}..{end} is empty after snapping to the {fps} fps grid"
            )));
        }
        let mut effect = OpEffect::new()
            .snap("start", start, range.start, fps)
            .snap("end", end, range.end, fps);

        let seq = project.sequence_mut(&sequence)?;
        let track = seq.track_mut(&track_id)?;
        util::assert_unlocked(track)?;
        let hits: Vec<ClipId> = track
            .clips
            .iter()
            .filter(|clip| clip.span().overlaps(&range))
            .map(|clip| clip.id.clone())
            .collect();
        if hits.is_empty() {
            return Err(Error::op(format!(
                "no clip on '{}' covers {range}",
                track.name
            )));
        }
        if cx.dry_run {
            return Ok(effect.data(serde_json::json!({
                "wouldSplit": hits.iter().map(ClipId::to_string).collect::<Vec<_>>(),
            })));
        }
        for id in hits {
            let mut index = util::clip_index(track, &id)?;
            if track.clips[index].start < range.start {
                index = split_at(track, index, range.start).expect("start is inside the clip");
                effect = effect.created(track.clips[index].id.clone());
            }
            if track.clips[index].end() > range.end {
                let tail = split_at(track, index, range.end).expect("end is inside the clip");
                effect = effect.created(track.clips[tail].id.clone());
            }
            track.clips[index].enabled = false;
            effect = effect.changed(track.clips[index].id.clone());
        }
        Ok(effect)
    }
}

struct Detach;

impl Op for Detach {
    fn id(&self) -> &'static str {
        "audio.detach"
    }

    fn about(&self) -> &'static str {
        "move an a/v clip's audio onto an audio track, linked to the video"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["target"],
            "properties": {
                "target": { "type": "string", "description": "clip selector naming one a/v clip" },
                "to": { "type": "string", "description": "destination audio track; a free one is used or created when omitted" }
            }
        })
    }

    fn apply(&self, project: &mut Project, arguments: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&arguments, &["target", "to"])?;
        let sequence_id = cx.sequence(project)?;
        let (video_track, video_clip) = selector::resolve_one_clip(
            project,
            &sequence_id,
            args::str_field(&arguments, "target")?,
        )?;

        let sequence = project.sequence(&sequence_id)?;
        let track = sequence.track(&video_track)?;
        let index = util::clip_index(track, &video_clip)?;
        let clip = &track.clips[index];
        let Source::Asset { asset, .. } = &clip.source else {
            return Err(Error::op(format!(
                "clip '{}' is a {} and has no audio stream to detach",
                clip.label(),
                clip.source.describe()
            )));
        };
        if project.asset(asset)?.probe.audio.is_none() {
            return Err(Error::op(format!(
                "asset '{}' has no audio stream",
                project.asset(asset)?.name
            )));
        }
        if clip.link.is_some() {
            return Err(Error::op(format!(
                "clip '{}' is already linked to another clip; unlink it before detaching",
                clip.label()
            )));
        }
        let span = clip.span();
        let mut audio = clip.clone();
        audio.id = ClipId::new();
        audio.name = clip.name.as_ref().map(|name| format!("{name} audio"));
        audio.link = Some(video_clip.clone());
        audio.transition_in = None;
        audio.effects.clear();
        audio.keyframes.clear();

        // Destination: the named track, else the first audio track with room, else a new
        // one. Placing detached audio on top of an existing clip would destroy it.
        let destination = match args::opt_str(&arguments, "to") {
            Some(text) => {
                let id = selector::resolve_track(project, &sequence_id, text)?;
                let candidate = sequence.track(&id)?;
                if candidate.kind != TrackKind::Audio {
                    return Err(Error::bad_args(format!(
                        "'{}' is a {:?} track; detached audio needs an audio track",
                        candidate.name, candidate.kind
                    )));
                }
                util::assert_free(candidate, span, None)?;
                Some(id)
            }
            None => sequence
                .tracks
                .iter()
                .find(|candidate| {
                    candidate.kind == TrackKind::Audio
                        && !candidate.locked
                        && util::occupants(candidate, span, None).is_empty()
                })
                .map(|candidate| candidate.id.clone()),
        };

        if cx.dry_run {
            return Ok(OpEffect::new().changed(video_clip));
        }

        let mut effect = OpEffect::new();
        let name = project.sequence(&sequence_id)?.next_track_name(TrackKind::Audio);
        let sequence = project.sequence_mut(&sequence_id)?;
        let destination = match destination {
            Some(id) => id,
            None => {
                let track = Track::new(name, TrackKind::Audio);
                let id = track.id.clone();
                sequence.tracks.push(track);
                effect = effect.created(id.clone());
                id
            }
        };
        let audio_id = audio.id.clone();
        let target = sequence.track_mut(&destination)?;
        util::assert_unlocked(target)?;
        target.place(audio);

        let source = sequence.track_mut(&video_track)?;
        util::assert_unlocked(source)?;
        let index = util::clip_index(source, &video_clip)?;
        source.clips[index].link = Some(audio_id.clone());

        Ok(effect
            .created(audio_id)
            .changed(video_clip)
            .changed(destination))
    }
}

struct TrimSilence;

impl Op for TrimSilence {
    fn id(&self) -> &'static str {
        "seq.trim-silence"
    }

    fn about(&self) -> &'static str {
        "ripple-delete the silent ranges of a reference track from every track"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["track"],
            "properties": {
                "track": { "type": "string", "description": "reference track whose silence defines the cuts" },
                "threshold-db": { "type": "number", "description": "level below which the reference counts as silent; default -40" },
                "min-duration": { "type": "string", "description": "shortest silence worth cutting; default 0.5s" },
                "pad": { "type": "string", "description": "silence left at each edge so a cut does not clip a breath; default 0.1s" }
            }
        })
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        &["ffmpeg"]
    }

    fn apply(&self, project: &mut Project, arguments: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(
            &arguments,
            &["track", "threshold-db", "min-duration", "pad"],
        )?;
        let sequence_id = cx.sequence(project)?;
        let fps = util::sequence_fps(project, &sequence_id)?;
        let reference =
            selector::resolve_track(project, &sequence_id, args::str_field(&arguments, "track")?)?;
        let threshold = args::opt_f64(&arguments, "threshold-db")?.unwrap_or(DEFAULT_SILENCE_DB);
        let requested_min = args::opt_time(&arguments, "min-duration", fps)?
            .unwrap_or_else(|| Time::new(1, 2).expect("1/2 is valid"));
        let requested_pad = args::opt_time(&arguments, "pad", fps)?
            .unwrap_or_else(|| Time::new(1, 10).expect("1/10 is valid"));
        if requested_min.is_negative() || requested_pad.is_negative() {
            return Err(Error::bad_args("'min-duration' and 'pad' cannot be negative"));
        }
        let min_duration = requested_min.snap(fps);
        let pad = requested_pad.snap(fps);

        let sequence = project.sequence(&sequence_id)?;
        for track in &sequence.tracks {
            // Every track is rippled, so one locked track means the edit cannot keep a/v in
            // sync; refusing is better than desynchronising the timeline.
            util::assert_unlocked(track)?;
        }
        let span = Span::new(Time::ZERO, sequence.duration());
        if span.is_empty() {
            return Err(Error::op("the sequence is empty; there is no silence to trim"));
        }
        let spec = MixSpec::of(sequence);
        let tool = Toolchain::shared()?;
        let mixed = mix_tracks(
            project,
            &sequence_id,
            Some(std::slice::from_ref(&reference)),
            span,
            spec,
            tool,
            cx.assets,
            cx.paths,
        )?;
        // What survives the cut. An all-silent reference would otherwise have its every
        // range removed, which deletes the timeline — a result no caller wants and one
        // that is far cheaper to refuse than to undo.
        if detect_speech(&mixed, spec.rate, spec.channels, threshold, min_duration).is_empty() {
            let name = project.sequence(&sequence_id)?.track(&reference)?.name.clone();
            return Err(Error::op(format!(
                "reference track '{name}' is silent end to end at {threshold} dBFS; trimming it would delete the whole timeline"
            )));
        }
        let detected = detect_silence(&mixed, spec.rate, spec.channels, threshold, min_duration);

        let ranges: Vec<Span> = detected
            .into_iter()
            .filter_map(|silence| {
                let padded = Span::new(silence.start + pad, silence.end - pad);
                if padded.is_empty() {
                    return None;
                }
                let snapped = snap_inward(padded, fps);
                (!snapped.is_empty()).then_some(snapped)
            })
            .collect();

        let mut effect = OpEffect::new()
            .snap("min-duration", requested_min, min_duration, fps)
            .snap("pad", requested_pad, pad, fps);
        let saved = ranges
            .iter()
            .fold(Time::ZERO, |total, range| total + range.duration());
        let report = serde_json::json!({
            "removed": ranges.iter().map(|range| serde_json::json!({
                "start": range.start.to_string(),
                "end": range.end.to_string(),
                "clock": format!("{} - {}", range.start.clock(), range.end.clock()),
                "duration": range.duration().to_string(),
            })).collect::<Vec<_>>(),
            "count": ranges.len(),
            "savedSeconds": round2(saved.as_secs_f64()),
            "thresholdDb": threshold,
            "reference": reference.to_string(),
        });
        if ranges.is_empty() || cx.dry_run {
            return Ok(effect.data(report));
        }

        let sequence = project.sequence_mut(&sequence_id)?;
        // Back to front: cutting a later range first leaves the earlier ranges' times
        // still valid, so no offset bookkeeping is needed.
        for range in ranges.iter().rev() {
            for track in &mut sequence.tracks {
                for id in delete_range_ripple(track, *range) {
                    effect = effect.removed(id);
                }
            }
            // Markers move with the content here, unlike an ordinary ripple edit. A
            // marker names a moment in the material ("pricing"), and trim-silence removes
            // material underneath it; leaving it where it was would point it at a
            // different sentence.
            for marker in &mut sequence.markers {
                marker.at = shift_time(marker.at, *range, range.duration());
            }
        }
        for track in &sequence.tracks {
            effect = effect.changed(track.id.clone());
        }
        sequence.validate()?;
        Ok(effect.data(report))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mix::fixture::{secs, Fixture, RATE};

    fn registry() -> Registry {
        let mut registry = Registry::new();
        register(&mut registry);
        registry
    }

    fn run(fixture: &mut Fixture, op: &str, arguments: serde_json::Value) -> Result<OpEffect> {
        run_mode(fixture, op, arguments, false)
    }

    fn run_mode(
        fixture: &mut Fixture,
        op: &str,
        arguments: serde_json::Value,
        dry_run: bool,
    ) -> Result<OpEffect> {
        let registry = registry();
        let op = registry.get(op)?.clone();
        let sequence = fixture.sequence.to_string();
        let paths = fixture.paths.clone();
        let assets = fixture.assets.clone();
        let mut cx = OpCx::new(&paths, &assets)
            .with_sequence(Some(sequence))
            .dry_run(dry_run);
        op.apply(&mut fixture.project, arguments, &mut cx)
    }

    #[test]
    fn every_op_id_is_registered_once_and_namespaced() {
        let registry = registry();
        let ids = registry.ids();
        assert_eq!(
            ids,
            vec![
                "audio.detach",
                "audio.duck",
                "audio.fade",
                "audio.gain",
                "audio.mute-range",
                "audio.normalize",
                "audio.pan",
                "seq.trim-silence",
            ]
        );
    }

    #[test]
    fn gain_sets_a_track_trim_and_clip_gains_separately() {
        let mut fixture = Fixture::new(2);
        let asset = fixture.audio_asset(
            "dc",
            "aevalsrc=0.5:d=1:s=48000:c=stereo",
            Time::from_secs(1),
            2,
        );
        let track = fixture.audio_track("A1", &asset, Time::ZERO, Time::from_secs(1));

        run(&mut fixture, "audio.gain", serde_json::json!({"track": "A1", "gain-db": -6.0}))
            .expect("track gain");
        assert_eq!(
            fixture.project.sequence(&fixture.sequence).unwrap().track(&track).unwrap().gain_db,
            -6.0
        );

        run(
            &mut fixture,
            "audio.gain",
            serde_json::json!({"target": "clip[track=A1]", "gain-db": -3.0}),
        )
        .expect("clip gain");
        let clip = &fixture.project.sequence(&fixture.sequence).unwrap().track(&track).unwrap().clips[0];
        assert_eq!(clip.gain_db, -3.0);
        // The track trim must not have been overwritten by the clip edit.
        assert_eq!(
            fixture.project.sequence(&fixture.sequence).unwrap().track(&track).unwrap().gain_db,
            -6.0
        );
    }

    #[test]
    fn an_unknown_argument_is_refused_rather_than_ignored() {
        let mut fixture = Fixture::new(2);
        let asset = fixture.audio_asset(
            "dc",
            "aevalsrc=0.5:d=1:s=48000:c=stereo",
            Time::from_secs(1),
            2,
        );
        fixture.audio_track("A1", &asset, Time::ZERO, Time::from_secs(1));
        let error = run(
            &mut fixture,
            "audio.gain",
            serde_json::json!({"track": "A1", "gain_db": -6.0}),
        )
        .err()
        .expect("a snake_case typo must not be silently ignored");
        assert!(error.to_string().contains("unknown argument"));
    }

    #[test]
    fn duck_refuses_a_track_that_ducks_against_itself() {
        let mut fixture = Fixture::new(2);
        let asset = fixture.audio_asset(
            "dc",
            "aevalsrc=0.5:d=1:s=48000:c=stereo",
            Time::from_secs(1),
            2,
        );
        fixture.audio_track("A1", &asset, Time::ZERO, Time::from_secs(1));
        let error = run(
            &mut fixture,
            "audio.duck",
            serde_json::json!({"track": "A1", "against": "A1", "by": -10.0}),
        )
        .err()
        .expect("self-ducking has no defined level");
        assert!(error.to_string().contains("itself"));
    }

    #[test]
    fn duck_writes_the_envelope_onto_every_clip_of_the_track() {
        let mut fixture = Fixture::new(2);
        let asset = fixture.audio_asset(
            "dc",
            "aevalsrc=0.5:d=4:s=48000:c=stereo",
            Time::from_secs(4),
            2,
        );
        let music = fixture.audio_track("A1", &asset, Time::ZERO, Time::from_secs(1));
        let voice = fixture.audio_track("A2", &asset, Time::ZERO, Time::from_secs(1));
        // A second clip on the ducked track, to prove the op does not stop at the first.
        let extra = {
            let mut clip = fixture
                .project
                .sequence(&fixture.sequence)
                .unwrap()
                .track(&music)
                .unwrap()
                .clips[0]
                .clone();
            clip.id = ClipId::from_raw("clp_second");
            clip.start = Time::from_secs(2);
            clip
        };
        fixture.seq().track_mut(&music).unwrap().clips.push(extra);

        run(
            &mut fixture,
            "audio.duck",
            serde_json::json!({"track": "A1", "against": "A2", "by": -14.0, "attack": "0.3", "release": "0.8"}),
        )
        .expect("duck");

        let track = fixture.project.sequence(&fixture.sequence).unwrap().track(&music).unwrap();
        assert_eq!(track.clips.len(), 2);
        for clip in &track.clips {
            let duck = clip.ducking.as_ref().expect("every clip is ducked");
            assert_eq!(duck.against, voice);
            assert_eq!(duck.by, -14.0);
            assert_eq!(duck.attack, secs(3, 10));
            assert_eq!(duck.release, secs(4, 5));
        }
    }

    #[test]
    fn mute_range_splits_and_disables_the_middle_only() {
        let mut fixture = Fixture::new(2);
        let asset = fixture.audio_asset(
            "dc",
            "aevalsrc=0.5:d=4:s=48000:c=stereo",
            Time::from_secs(4),
            2,
        );
        let track = fixture.audio_track("A1", &asset, Time::ZERO, Time::from_secs(4));
        run(
            &mut fixture,
            "audio.mute-range",
            serde_json::json!({"track": "A1", "start": "1", "end": "2"}),
        )
        .expect("mute range");

        let clips = &fixture.project.sequence(&fixture.sequence).unwrap().track(&track).unwrap().clips;
        assert_eq!(clips.len(), 3, "one clip becomes head, middle and tail");
        assert!(clips[0].enabled && !clips[1].enabled && clips[2].enabled);
        assert_eq!(clips[1].span(), Span::new(Time::from_secs(1), Time::from_secs(2)));
        // The tail must still point at the right part of the source, or the audio jumps.
        assert_eq!(clips[2].source_in, Time::from_secs(2));

        let mixed = fixture.mix(Span::new(Time::ZERO, Time::from_secs(3)));
        let at = |seconds: usize| mixed[seconds * RATE as usize * 2 + 1000];
        assert!((at(0) - 0.5).abs() < 0.01, "head still plays");
        assert!(at(1).abs() < 1e-6, "the muted range is silent");
        assert!((at(2) - 0.5).abs() < 0.01, "tail still plays");
    }

    #[test]
    fn detach_moves_audio_to_its_own_track_without_doubling_it() {
        let mut fixture = Fixture::new(2);
        let asset = fixture.audio_asset(
            "av",
            "aevalsrc=0.5:d=2:s=48000:c=stereo",
            Time::from_secs(2),
            2,
        );
        let mut video = Track::new("V1", TrackKind::Video);
        video.id = TrackId::from_raw("trk_V1");
        let mut clip = Clip::new(
            Source::Asset {
                asset: asset.clone(),
                stream: None,
            },
            Time::ZERO,
            Time::from_secs(2),
        );
        clip.id = ClipId::from_raw("clp_shot");
        clip.name = Some("shot".to_string());
        video.clips.push(clip);
        fixture.seq().tracks.push(video);

        let before = fixture.mix(Span::new(Time::ZERO, secs(1, 2)));
        assert!((before[1000] - 0.5).abs() < 0.01, "the a/v clip is audible");

        run(&mut fixture, "audio.detach", serde_json::json!({"target": "#shot"}))
            .expect("detach");

        let sequence = fixture.project.sequence(&fixture.sequence).unwrap();
        let audio_track = sequence
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .expect("an audio track was created");
        assert_eq!(audio_track.clips.len(), 1);
        let audio_clip = &audio_track.clips[0];
        assert_eq!(audio_clip.link.as_ref(), Some(&ClipId::from_raw("clp_shot")));
        let video_clip = &sequence.track(&TrackId::from_raw("trk_V1")).unwrap().clips[0];
        assert_eq!(video_clip.link.as_ref(), Some(&audio_clip.id));

        let after = fixture.mix(Span::new(Time::ZERO, secs(1, 2)));
        assert!(
            (after[1000] - 0.5).abs() < 0.01,
            "detached audio must be heard exactly once, got {}",
            after[1000]
        );
    }

    #[test]
    fn normalize_reports_the_measurement_and_writes_the_gain() {
        let mut fixture = Fixture::new(2);
        let asset = fixture.audio_asset(
            "speech",
            "anoisesrc=d=12:c=pink:a=0.3:r=48000,tremolo=f=2:d=0.7",
            Time::from_secs(12),
            2,
        );
        let track = fixture.audio_track("A1", &asset, Time::ZERO, Time::from_secs(12));

        let effect = run(
            &mut fixture,
            "audio.normalize",
            serde_json::json!({"track": "A1", "lufs": -16.0}),
        )
        .expect("normalize");
        let data = effect.data.expect("normalize reports its measurement");
        let measured = data["measuredLufs"].as_f64().expect("measuredLufs");
        let gain = data["gainDb"].as_f64().expect("gainDb");
        assert_eq!(data["targetLufs"].as_f64(), Some(-16.0));
        assert!(
            (measured + gain + 16.0).abs() < 0.1,
            "reported gain must move the reported measurement onto the target"
        );

        let written = fixture.project.sequence(&fixture.sequence).unwrap().track(&track).unwrap().gain_db;
        assert!((f64::from(written) - gain).abs() < 0.01);

        // Re-mix with the gain applied and confirm the result really is on target.
        let span = Span::new(Time::ZERO, Time::from_secs(12));
        let mixed = fixture.mix(span);
        let after = analyze_loudness(&mixed, RATE, 2).expect("analyze");
        assert!(
            (after.integrated_lufs + 16.0).abs() < 0.5,
            "after normalizing the mix should measure -16 LUFS, got {:.2}",
            after.integrated_lufs
        );
    }

    #[test]
    fn trim_silence_removes_the_pauses_and_keeps_every_track_the_same_length() {
        let mut fixture = Fixture::new(2);
        // Speech on A1: sound, 2 s pause, sound. A music bed and a video track run the
        // whole way and must end up shortened by exactly the same amount.
        let speech = fixture.audio_asset(
            "speech",
            "aevalsrc='0.4*sin(2*PI*300*t)*(lt(t,2)+gt(t,4))':d=6:s=48000:c=stereo",
            Time::from_secs(6),
            2,
        );
        let bed = fixture.audio_asset(
            "bed",
            "aevalsrc='0.2*sin(2*PI*80*t)':d=6:s=48000:c=stereo",
            Time::from_secs(6),
            2,
        );
        fixture.audio_track("A1", &speech, Time::ZERO, Time::from_secs(6));
        fixture.audio_track("A2", &bed, Time::ZERO, Time::from_secs(6));
        let mut video = Track::new("V1", TrackKind::Video);
        video.id = TrackId::from_raw("trk_V1");
        let mut clip = Clip::new(
            Source::Asset {
                asset: speech.clone(),
                stream: None,
            },
            Time::ZERO,
            Time::from_secs(6),
        );
        clip.id = ClipId::from_raw("clp_video");
        video.clips.push(clip);
        fixture.seq().tracks.push(video);

        let effect = run(
            &mut fixture,
            "seq.trim-silence",
            serde_json::json!({"track": "A1", "threshold-db": -40.0, "min-duration": "0.5", "pad": "0.1"}),
        )
        .expect("trim silence");
        let data = effect.data.expect("trim-silence reports what it removed");
        assert_eq!(data["count"].as_u64(), Some(1), "one pause: {data}");
        let saved = data["savedSeconds"].as_f64().expect("savedSeconds");
        assert!(
            (saved - 1.8).abs() < 0.1,
            "a 2 s pause padded by 0.1 s each side removes ~1.8 s, got {saved}"
        );

        let sequence = fixture.project.sequence(&fixture.sequence).unwrap();
        let lengths: Vec<Time> = sequence
            .tracks
            .iter()
            .map(|track| track.clips.iter().map(|clip| clip.end()).max().unwrap_or(Time::ZERO))
            .collect();
        assert!(
            lengths.windows(2).all(|pair| pair[0] == pair[1]),
            "every track must stay the same length or a/v drifts: {lengths:?}"
        );
        assert!(
            (lengths[0].as_secs_f64() - (6.0 - saved)).abs() < 0.05,
            "tracks should be {} s long, got {}",
            6.0 - saved,
            lengths[0]
        );
        sequence.validate().expect("no overlaps were produced");
    }

    #[test]
    fn trim_silence_refuses_to_desynchronise_a_locked_track() {
        let mut fixture = Fixture::new(2);
        let asset = fixture.audio_asset(
            "speech",
            "aevalsrc='0.4*sin(2*PI*300*t)*(lt(t,2)+gt(t,4))':d=6:s=48000:c=stereo",
            Time::from_secs(6),
            2,
        );
        fixture.audio_track("A1", &asset, Time::ZERO, Time::from_secs(6));
        let locked = fixture.audio_track("A2", &asset, Time::ZERO, Time::from_secs(6));
        fixture.seq().track_mut(&locked).unwrap().locked = true;

        let error = run(
            &mut fixture,
            "seq.trim-silence",
            serde_json::json!({"track": "A1"}),
        )
        .err()
        .expect("a locked track cannot be rippled");
        assert!(error.to_string().contains("locked"));
    }

    #[test]
    fn trim_silence_refuses_an_all_silent_reference_instead_of_deleting_everything() {
        let mut fixture = Fixture::new(2);
        let quiet = fixture.audio_asset(
            "quiet",
            "aevalsrc=0:d=4:s=48000:c=stereo",
            Time::from_secs(4),
            2,
        );
        let bed = fixture.audio_asset(
            "bed",
            "aevalsrc='0.3*sin(2*PI*220*t)':d=4:s=48000:c=stereo",
            Time::from_secs(4),
            2,
        );
        fixture.audio_track("A1", &quiet, Time::ZERO, Time::from_secs(4));
        fixture.audio_track("A2", &bed, Time::ZERO, Time::from_secs(4));

        let error = run(
            &mut fixture,
            "seq.trim-silence",
            serde_json::json!({"track": "A1"}),
        )
        .err()
        .expect("an all-silent reference would delete the timeline");
        assert!(error.to_string().contains("silent end to end"), "{error}");
        let sequence = fixture.project.sequence(&fixture.sequence).unwrap();
        assert_eq!(
            sequence.duration(),
            Time::from_secs(4),
            "the timeline must be untouched"
        );
    }

    #[test]
    fn a_dry_run_measures_without_writing() {
        let mut fixture = Fixture::new(2);
        let asset = fixture.audio_asset(
            "speech",
            "anoisesrc=d=8:c=pink:a=0.3:r=48000",
            Time::from_secs(8),
            2,
        );
        let track = fixture.audio_track("A1", &asset, Time::ZERO, Time::from_secs(8));
        let effect = run_mode(
            &mut fixture,
            "audio.normalize",
            serde_json::json!({"track": "A1"}),
            true,
        )
        .expect("dry run");
        assert!(
            effect.data.is_some(),
            "a dry run still reports the measurement"
        );
        assert_eq!(
            fixture.project.sequence(&fixture.sequence).unwrap().track(&track).unwrap().gain_db,
            0.0,
            "a dry run must not write"
        );
    }
}
