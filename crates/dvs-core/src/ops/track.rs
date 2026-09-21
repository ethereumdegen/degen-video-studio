//! Track ops: add, remove, rename, order, and the per-track flags and mixer trim.
//!
//! Two decisions are worth stating because every other file in `ops/` inherits them.
//!
//! **What a lock protects.** `locked` guards a track's *contents*: nothing may add, move,
//! trim or delete the clips on it. It deliberately does not guard the mixer strip or the lock
//! flag itself — a lock you cannot clear is a trap, and muting a locked track changes no
//! edit. So `track.remove` refuses on a locked track while `track.mute` does not.
//!
//! **Names are addresses.** `V1` is how an agent and a human both refer to a track, so names
//! stay unique within a sequence and `track.add` defaults to the next free one for the kind
//! rather than leaving a nameless track that no selector can reach.

use crate::error::{Error, Result};
use crate::ids::ClipId;
use crate::op::{args, Op, OpCx, OpEffect, Registry};
use crate::ops::util::assert_unlocked;
use crate::project::{Project, Track, TrackKind};
use crate::selector;

pub fn register(registry: &mut Registry) {
    registry
        .register(TrackAdd)
        .register(TrackRemove)
        .register(TrackRename)
        .register(TrackFlag(Flag::Mute))
        .register(TrackFlag(Flag::Solo))
        .register(TrackFlag(Flag::Lock))
        .register(TrackFlag(Flag::Hide))
        .register(TrackReorder)
        .register(TrackGain)
        .register(TrackPan);
}

/// A position among siblings.
///
/// An index past the end clamps to the end and the clamp is reported: "put it last" is a
/// normal thing to mean and `--index 99` is how an agent says it, but a fractional or
/// negative index is a mistake worth naming. Shared with `fx.reorder`, which has the same
/// contract for the same reason.
pub(crate) fn opt_index(args: &serde_json::Value, key: &str) -> Result<Option<usize>> {
    let Some(value) = args::opt_f64(args, key)? else {
        return Ok(None);
    };
    if !value.is_finite() || value.fract() != 0.0 || value < 0.0 {
        return Err(Error::bad_args(format!(
            "field '{key}' must be a whole index of 0 or more, got {value}"
        )));
    }
    Ok(Some(value as usize))
}

fn parse_kind(text: &str) -> Result<TrackKind> {
    match text.trim().to_ascii_lowercase().as_str() {
        "video" | "v" => Ok(TrackKind::Video),
        "audio" | "a" => Ok(TrackKind::Audio),
        "caption" | "captions" | "cc" | "subtitle" => Ok(TrackKind::Caption),
        other => Err(Error::bad_args(format!(
            "unknown track kind '{other}'; expected video, audio or caption"
        ))),
    }
}

/// A name that is free in this sequence, compared the way [`Track::name`] is resolved:
/// case-insensitively, because `v1` and `V1` addressing different tracks would be a trap.
fn assert_name_free(sequence: &crate::project::Sequence, name: &str) -> Result<()> {
    if sequence
        .tracks
        .iter()
        .any(|track| track.name.eq_ignore_ascii_case(name))
    {
        return Err(Error::bad_args(format!(
            "track name '{name}' is already used in sequence '{}'; names address tracks",
            sequence.name
        )));
    }
    Ok(())
}

pub struct TrackAdd;

impl Op for TrackAdd {
    fn id(&self) -> &'static str {
        "track.add"
    }

    fn about(&self) -> &'static str {
        "Add a video, audio or caption track"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["video", "audio", "caption"],
                    "description": "What the track carries"
                },
                "name": {
                    "type": "string",
                    "description": "Track name; defaults to the next free one for the kind (V1, V2, A1, CC1)"
                },
                "index": {
                    "type": "integer",
                    "description": "Position among the sequence's tracks; defaults to last"
                }
            },
            "required": ["kind"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["kind", "name", "index"])?;
        let seq = cx.sequence(project)?;
        let kind = parse_kind(args::str_field(&args, "kind")?)?;
        let requested = opt_index(&args, "index")?;
        let explicit = args::opt_str(&args, "name").map(str::trim);

        let sequence = project.sequence_mut(&seq)?;
        let name = match explicit {
            Some(name) if !name.is_empty() => {
                assert_name_free(sequence, name)?;
                name.to_string()
            }
            Some(_) => return Err(Error::bad_args("track name is empty")),
            None => sequence.next_track_name(kind),
        };

        let track = Track::new(name, kind);
        let id = track.id.clone();
        let mut effect = OpEffect::new().created(&id);
        let index = match requested {
            Some(requested) if requested > sequence.tracks.len() => {
                effect = effect.warn(
                    "index-clamped",
                    &id,
                    format!(
                        "index {requested} is past the end; added at {}",
                        sequence.tracks.len()
                    ),
                );
                sequence.tracks.len()
            }
            Some(requested) => requested,
            None => sequence.tracks.len(),
        };
        sequence.tracks.insert(index, track);
        Ok(effect)
    }
}

pub struct TrackRemove;

impl Op for TrackRemove {
    fn id(&self) -> &'static str {
        "track.remove"
    }

    fn about(&self) -> &'static str {
        "Remove a track; refuses a track that still holds clips unless forced"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "track": { "type": "string", "description": "Track selector", "examples": ["V1", "trk_01J", "track[kind=audio]"] },
                "force": {
                    "type": "boolean",
                    "description": "Remove the track and everything on it"
                }
            },
            "required": ["track"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["track", "force"])?;
        let seq = cx.sequence(project)?;
        let target = selector::resolve_track(project, &seq, args::str_field(&args, "track")?)?;
        let force = args::opt_bool(&args, "force")?.unwrap_or(false);

        let sequence = project.sequence_mut(&seq)?;
        let index = sequence
            .tracks
            .iter()
            .position(|track| track.id == target)
            .ok_or_else(|| {
                Error::no_match("track", target.as_str(), sequence.track_names())
            })?;
        let track = &sequence.tracks[index];
        assert_unlocked(track)?;
        let held = track.clips.len() + track.cues.len();
        if held > 0 && !force {
            return Err(Error::op(format!(
                "track '{}' still holds {held} clip(s)/cue(s); pass force to remove it with its contents",
                track.name
            )));
        }

        let removed = sequence.tracks.remove(index);
        let mut effect = OpEffect::new().removed(&removed.id);
        for clip in &removed.clips {
            effect = effect.removed(&clip.id);
        }
        for cue in &removed.cues {
            effect = effect.removed(&cue.id);
        }

        // Ducking and a/v links point at ids that just stopped existing. A dangling id is
        // invisible until the mixer or a trim follows it, so it is cleared here and reported.
        let gone_clips: Vec<ClipId> = removed.clips.iter().map(|clip| clip.id.clone()).collect();
        for track in &mut sequence.tracks {
            for clip in &mut track.clips {
                if clip
                    .ducking
                    .as_ref()
                    .is_some_and(|ducking| ducking.against == removed.id)
                {
                    clip.ducking = None;
                    effect = effect.warn(
                        "reference-cleared",
                        &clip.id,
                        format!("ducking against removed track '{}'", removed.name),
                    );
                    effect = effect.changed(&clip.id);
                }
                if clip.link.as_ref().is_some_and(|link| gone_clips.contains(link)) {
                    clip.link = None;
                    effect = effect.warn(
                        "reference-cleared",
                        &clip.id,
                        "a/v link to a clip on the removed track",
                    );
                    effect = effect.changed(&clip.id);
                }
            }
        }
        Ok(effect)
    }
}

pub struct TrackRename;

impl Op for TrackRename {
    fn id(&self) -> &'static str {
        "track.rename"
    }

    fn about(&self) -> &'static str {
        "Rename a track; its id is stable and does not change"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "track": { "type": "string", "description": "Track selector" },
                "name": { "type": "string", "description": "New name, unique in the sequence" }
            },
            "required": ["track", "name"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["track", "name"])?;
        let seq = cx.sequence(project)?;
        let target = selector::resolve_track(project, &seq, args::str_field(&args, "track")?)?;
        let name = args::str_field(&args, "name")?.trim().to_string();
        if name.is_empty() {
            return Err(Error::bad_args("track name is empty"));
        }

        let sequence = project.sequence_mut(&seq)?;
        if sequence
            .tracks
            .iter()
            .any(|track| track.id != target && track.name.eq_ignore_ascii_case(&name))
        {
            return Err(Error::bad_args(format!(
                "track name '{name}' is already used in sequence '{}'; names address tracks",
                sequence.name
            )));
        }
        sequence.track_mut(&target)?.name = name;
        Ok(OpEffect::new().changed(&target))
    }
}

/// The boolean track flags. They differ only in the field they write, so they share an
/// implementation; they stay four ops because `track.mute` is the verb an agent looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    Mute,
    Solo,
    Lock,
    Hide,
}

pub struct TrackFlag(pub Flag);

impl Op for TrackFlag {
    fn id(&self) -> &'static str {
        match self.0 {
            Flag::Mute => "track.mute",
            Flag::Solo => "track.solo",
            Flag::Lock => "track.lock",
            Flag::Hide => "track.hide",
        }
    }

    fn about(&self) -> &'static str {
        match self.0 {
            Flag::Mute => "Silence a track in the mix",
            Flag::Solo => "Play only the soloed tracks",
            Flag::Lock => "Protect a track's clips from every edit",
            Flag::Hide => "Leave a track out of the composite",
        }
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "track": { "type": "string", "description": "Track selector" },
                "on": { "type": "boolean", "description": "Set the flag; pass false to clear it. Defaults to true" }
            },
            "required": ["track"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["track", "on"])?;
        let seq = cx.sequence(project)?;
        let target = selector::resolve_track(project, &seq, args::str_field(&args, "track")?)?;
        let on = args::opt_bool(&args, "on")?.unwrap_or(true);
        let track = project.sequence_mut(&seq)?.track_mut(&target)?;
        match self.0 {
            Flag::Mute => track.muted = on,
            Flag::Solo => track.solo = on,
            Flag::Lock => track.locked = on,
            Flag::Hide => track.hidden = on,
        }
        Ok(OpEffect::new().changed(&target))
    }
}

pub struct TrackReorder;

impl Op for TrackReorder {
    fn id(&self) -> &'static str {
        "track.reorder"
    }

    fn about(&self) -> &'static str {
        "Move a track to another position in the stack"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "track": { "type": "string", "description": "Track selector" },
                "index": { "type": "integer", "description": "Destination position; past the end means last" }
            },
            "required": ["track", "index"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["track", "index"])?;
        let seq = cx.sequence(project)?;
        let target = selector::resolve_track(project, &seq, args::str_field(&args, "track")?)?;
        let requested = opt_index(&args, "index")?
            .ok_or_else(|| Error::bad_args("missing index field 'index'"))?;

        let sequence = project.sequence_mut(&seq)?;
        let from = sequence
            .tracks
            .iter()
            .position(|track| track.id == target)
            .ok_or_else(|| Error::no_match("track", target.as_str(), sequence.track_names()))?;
        let last = sequence.tracks.len() - 1;
        let mut effect = OpEffect::new().changed(&target);
        let to = if requested > last {
            effect = effect.warn(
                "index-clamped",
                &target,
                format!("index {requested} is past the end; moved to {last}"),
            );
            last
        } else {
            requested
        };
        if to != from {
            let track = sequence.tracks.remove(from);
            sequence.tracks.insert(to, track);
        }
        Ok(effect)
    }
}

/// Where a mixer value stops being an edit and starts being a mistake. +24 dB is more boost
/// than any sane mix needs and −96 dB is silence; outside that an agent has passed a linear
/// gain or a typo'd exponent.
const GAIN_FLOOR_DB: f64 = -96.0;
const GAIN_CEILING_DB: f64 = 24.0;

pub struct TrackGain;

impl Op for TrackGain {
    fn id(&self) -> &'static str {
        "track.gain"
    }

    fn about(&self) -> &'static str {
        "Set a track's level trim in decibels"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "track": { "type": "string", "description": "Track selector" },
                "gain-db": {
                    "type": "number",
                    "minimum": -96.0,
                    "maximum": 24.0,
                    "description": "Track trim in dB, applied after clip gain; 0 is unity"
                }
            },
            "required": ["track", "gain-db"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["track", "gain-db"])?;
        let seq = cx.sequence(project)?;
        let target = selector::resolve_track(project, &seq, args::str_field(&args, "track")?)?;
        let gain = args::f64_field(&args, "gain-db")?;
        if !gain.is_finite() || gain < GAIN_FLOOR_DB || gain > GAIN_CEILING_DB {
            return Err(Error::bad_args(format!(
                "gain-db {gain} is outside the usable range {GAIN_FLOOR_DB} to {GAIN_CEILING_DB}"
            )));
        }
        let track = project.sequence_mut(&seq)?.track_mut(&target)?;
        assert_has_signal(track, "gain")?;
        track.gain_db = gain as f32;
        Ok(OpEffect::new().changed(&target))
    }
}

pub struct TrackPan;

impl Op for TrackPan {
    fn id(&self) -> &'static str {
        "track.pan"
    }

    fn about(&self) -> &'static str {
        "Place a track in the stereo field"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "track": { "type": "string", "description": "Track selector" },
                "pan": {
                    "type": "number",
                    "minimum": -1.0,
                    "maximum": 1.0,
                    "description": "-1 hard left, 0 center, +1 hard right"
                }
            },
            "required": ["track", "pan"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["track", "pan"])?;
        let seq = cx.sequence(project)?;
        let target = selector::resolve_track(project, &seq, args::str_field(&args, "track")?)?;
        let pan = args::f64_field(&args, "pan")?;
        if !pan.is_finite() || !(-1.0..=1.0).contains(&pan) {
            return Err(Error::bad_args(format!(
                "pan {pan} is outside -1 (left) to +1 (right)"
            )));
        }
        let track = project.sequence_mut(&seq)?.track_mut(&target)?;
        assert_has_signal(track, "pan")?;
        track.pan = pan as f32;
        Ok(OpEffect::new().changed(&target))
    }
}

/// A caption track carries text, not signal, so a mixer value on it would silently do
/// nothing at render time.
fn assert_has_signal(track: &Track, what: &str) -> Result<()> {
    if track.kind == TrackKind::Caption {
        return Err(Error::op(format!(
            "track '{}' carries captions and has no audio to {what}",
            track.name
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::AssetStore;
    use crate::color::Rgba;
    use crate::ids::TrackId;
    use crate::paths::ProjectPaths;
    use crate::project::{Clip, Ducking, Source};
    use crate::time::{Fps, Time};
    use crate::vfs::MemVfs;
    use std::sync::Arc;

    fn apply(op: &dyn Op, project: &mut Project, args: serde_json::Value) -> Result<OpEffect> {
        let paths = ProjectPaths::new("/p");
        let assets = AssetStore::new("/p/assets".into(), Arc::new(MemVfs::default()));
        let mut cx = OpCx::new(&paths, &assets);
        op.apply(project, args, &mut cx)
    }

    fn project() -> Project {
        Project::new(
            "promo",
            Fps::new(30, 1).expect("30 fps"),
            [1920, 1080],
            48_000,
        )
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
        clip
    }

    fn track_named(project: &Project, name: &str) -> TrackId {
        let seq = project.active_sequence.clone();
        project
            .sequence(&seq)
            .expect("main")
            .resolve_track(name)
            .expect("track exists")
    }

    #[test]
    fn added_tracks_are_named_per_kind() {
        let mut project = project();
        for kind in ["video", "video", "audio", "caption"] {
            apply(&TrackAdd, &mut project, serde_json::json!({ "kind": kind }))
                .expect("add track");
        }
        let seq = project.active_sequence.clone();
        assert_eq!(
            project.sequence(&seq).expect("main").track_names(),
            vec!["V1", "V2", "A1", "CC1"]
        );
    }

    #[test]
    fn an_explicit_index_places_the_track_and_a_past_end_index_is_reported() {
        let mut project = project();
        apply(&TrackAdd, &mut project, serde_json::json!({ "kind": "video" })).expect("V1");
        apply(
            &TrackAdd,
            &mut project,
            serde_json::json!({ "kind": "audio", "index": 0 }),
        )
        .expect("A1 first");
        let effect = apply(
            &TrackAdd,
            &mut project,
            serde_json::json!({ "kind": "video", "index": 99 }),
        )
        .expect("V2 last");

        let seq = project.active_sequence.clone();
        assert_eq!(
            project.sequence(&seq).expect("main").track_names(),
            vec!["A1", "V1", "V2"]
        );
        assert_eq!(
            effect.warnings.first().map(|warning| warning.code),
            Some("index-clamped")
        );
    }

    #[test]
    fn a_duplicate_track_name_is_refused() {
        let mut project = project();
        apply(&TrackAdd, &mut project, serde_json::json!({ "kind": "video" })).expect("V1");
        let err = apply(
            &TrackAdd,
            &mut project,
            serde_json::json!({ "kind": "audio", "name": "v1" }),
        )
        .expect_err("names address tracks and stay unique");
        assert_eq!(err.exit_code(), crate::error::exit::BAD_ARGS);

        apply(&TrackAdd, &mut project, serde_json::json!({ "kind": "audio" })).expect("A1");
        let err = apply(
            &TrackRename,
            &mut project,
            serde_json::json!({ "track": "A1", "name": "V1" }),
        )
        .expect_err("rename onto a taken name");
        assert!(err.to_string().contains("already used"), "{err}");
    }

    #[test]
    fn a_non_empty_track_needs_force_to_go() {
        let mut project = project();
        apply(&TrackAdd, &mut project, serde_json::json!({ "kind": "video" })).expect("V1");
        let seq = project.active_sequence.clone();
        project.sequence_mut(&seq).expect("main").tracks[0]
            .clips
            .push(clip("clp_a", 0, 2));

        let err = apply(
            &TrackRemove,
            &mut project,
            serde_json::json!({ "track": "V1" }),
        )
        .expect_err("a track with clips must not vanish silently");
        assert!(err.to_string().contains("force"), "{err}");
        assert_eq!(project.sequence(&seq).expect("main").tracks.len(), 1);

        let effect = apply(
            &TrackRemove,
            &mut project,
            serde_json::json!({ "track": "V1", "force": true }),
        )
        .expect("forced removal");
        assert!(project.sequence(&seq).expect("main").tracks.is_empty());
        assert!(
            effect.removed.iter().any(|id| id == "clp_a"),
            "the clips that went with it should be reported: {:?}",
            effect.removed
        );
    }

    #[test]
    fn a_locked_track_refuses_removal_but_can_be_unlocked() {
        let mut project = project();
        apply(&TrackAdd, &mut project, serde_json::json!({ "kind": "video" })).expect("V1");
        apply(
            &TrackFlag(Flag::Lock),
            &mut project,
            serde_json::json!({ "track": "V1" }),
        )
        .expect("lock");

        let err = apply(
            &TrackRemove,
            &mut project,
            serde_json::json!({ "track": "V1" }),
        )
        .expect_err("locked tracks refuse mutation");
        assert_eq!(
            project.sequence(&project.active_sequence.clone()).unwrap().tracks.len(),
            1,
            "the locked track was removed anyway: {err}"
        );

        // A lock that cannot be cleared would be a trap, so the flag ops ignore it.
        apply(
            &TrackFlag(Flag::Lock),
            &mut project,
            serde_json::json!({ "track": "V1", "on": false }),
        )
        .expect("unlock");
        apply(
            &TrackRemove,
            &mut project,
            serde_json::json!({ "track": "V1" }),
        )
        .expect("removal after unlocking");
    }

    #[test]
    fn removing_a_track_clears_what_pointed_at_it() {
        let mut project = project();
        apply(&TrackAdd, &mut project, serde_json::json!({ "kind": "audio" })).expect("A1");
        apply(&TrackAdd, &mut project, serde_json::json!({ "kind": "audio" })).expect("A2");
        let a1 = track_named(&project, "A1");
        let seq = project.active_sequence.clone();
        let sequence = project.sequence_mut(&seq).expect("main");
        let mut music = clip("clp_music", 0, 8);
        music.ducking = Some(Ducking {
            against: a1.clone(),
            by: -12.0,
            attack: Time::new(1, 5).unwrap(),
            release: Time::new(1, 2).unwrap(),
            threshold: -30.0,
        });
        sequence.tracks[1].clips.push(music);

        let effect = apply(
            &TrackRemove,
            &mut project,
            serde_json::json!({ "track": "A1" }),
        )
        .expect("remove the empty dialogue track");

        let sequence = project.sequence(&seq).expect("main");
        assert!(
            sequence.tracks[0].clips[0].ducking.is_none(),
            "ducking still points at a removed track"
        );
        assert_eq!(
            effect.warnings.first().map(|warning| warning.code),
            Some("reference-cleared")
        );
    }

    #[test]
    fn reorder_moves_the_track_and_clamps_past_the_end() {
        let mut project = project();
        for _ in 0..3 {
            apply(&TrackAdd, &mut project, serde_json::json!({ "kind": "video" }))
                .expect("add");
        }
        apply(
            &TrackReorder,
            &mut project,
            serde_json::json!({ "track": "V3", "index": 0 }),
        )
        .expect("to the bottom");
        let seq = project.active_sequence.clone();
        assert_eq!(
            project.sequence(&seq).expect("main").track_names(),
            vec!["V3", "V1", "V2"]
        );

        let effect = apply(
            &TrackReorder,
            &mut project,
            serde_json::json!({ "track": "V3", "index": 12 }),
        )
        .expect("to the top");
        assert_eq!(
            project.sequence(&seq).expect("main").track_names(),
            vec!["V1", "V2", "V3"]
        );
        assert_eq!(
            effect.warnings.first().map(|warning| warning.code),
            Some("index-clamped")
        );
    }

    #[test]
    fn mixer_values_are_range_checked_and_refused_on_caption_tracks() {
        let mut project = project();
        apply(&TrackAdd, &mut project, serde_json::json!({ "kind": "audio" })).expect("A1");
        apply(
            &TrackAdd,
            &mut project,
            serde_json::json!({ "kind": "caption" }),
        )
        .expect("CC1");

        apply(
            &TrackGain,
            &mut project,
            serde_json::json!({ "track": "A1", "gain-db": -6.5 }),
        )
        .expect("gain");
        let a1 = track_named(&project, "A1");
        let seq = project.active_sequence.clone();
        assert_eq!(
            project.sequence(&seq).expect("main").track(&a1).unwrap().gain_db,
            -6.5
        );

        let err = apply(
            &TrackGain,
            &mut project,
            serde_json::json!({ "track": "A1", "gain-db": 400 }),
        )
        .expect_err("400 dB is a typo, not a mix");
        assert_eq!(err.exit_code(), crate::error::exit::BAD_ARGS);

        let err = apply(
            &TrackPan,
            &mut project,
            serde_json::json!({ "track": "CC1", "pan": 0.5 }),
        )
        .expect_err("captions have no signal to pan");
        assert!(err.to_string().contains("captions"), "{err}");
    }
}
