//! Effects, keyframes and transitions: everything that modulates a clip rather than moving it.
//!
//! Three things here are deliberate.
//!
//! **The effect catalog is closed.** [`FX_KINDS`] is the v1 list, and `fx.add` refuses
//! anything else while naming the whole catalog. A renderer that silently ignores an unknown
//! `kind` is the worst possible failure for a blind operator: the document says the shot is
//! graded, the pixels say it is not.
//!
//! **Keyframe paths are validated against the clip, not against a string grammar.** An
//! `fx.<id>.<param>` path whose effect id is not on this clip is rejected with the clip's real
//! effect ids, because that mistake — animating an effect that lives on a different clip — is
//! otherwise invisible until the render looks unchanged.
//!
//! **A transition is owned by the later clip** (`transitionIn`) and may never be longer than
//! either neighbour. Clips still do not overlap; the transition describes how the cut between
//! two adjacent clips is blended, so it needs a preceding clip to blend from.

use crate::error::{Error, Result};
use crate::ids::{ClipId, EffectId, SequenceId, TrackId};
use crate::op::{args, Op, OpCx, OpEffect, Registry};
use crate::ops::util::{assert_unlocked, clip_index, sequence_fps};
use crate::project::{Clip, Direction, Easing, Effect, Keyframe, Project, Transition, TransitionKind};
use crate::selector::{self, Match, Selector};
use crate::time::Time;

pub fn register(registry: &mut Registry) {
    registry
        .register(FxAdd)
        .register(FxRemove)
        .register(FxSet)
        .register(FxReorder)
        .register(FxEnable)
        .register(KfSet)
        .register(KfRemove)
        .register(KfClear)
        .register(KfEase)
        .register(TransitionSet)
        .register(TransitionRemove);
}

/// The v1 effect catalog. Every kind here is implemented by the compositor; an `fx.add` of
/// anything else is an error that lists this list, so discovery costs one failed call rather
/// than a render that looks untouched.
pub const FX_KINDS: &[&str] = &[
    "color.lut",
    "color.grade",
    "blur",
    "sharpen",
    "crop",
    "mask.shape",
    "chroma-key",
    "stabilize",
];

/// Clip parameters the compositor evaluates per frame. `fx.<effectId>.<param>` is the other
/// legal shape and is checked against the clip's own effects.
pub const KEYFRAME_PATHS: &[&str] = &[
    "opacity",
    "transform.pos.x",
    "transform.pos.y",
    "transform.scale",
    "transform.scale.x",
    "transform.scale.y",
    "transform.rotation",
];

/// Parse a value the document already knows how to deserialize, replacing serde's message
/// with one that lists the accepted spellings — the agent needs the list, not the type name.
/// Strings are lowercased first: the document spelling is kebab-lowercase, and refusing
/// `"Dissolve"` teaches nothing.
fn opt_enum<T: serde::de::DeserializeOwned>(
    args: &serde_json::Value,
    key: &str,
    accepted: &str,
) -> Result<Option<T>> {
    let Some(value) = args.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let normalized = match value {
        serde_json::Value::String(text) => {
            serde_json::Value::String(text.trim().to_ascii_lowercase())
        }
        other => other.clone(),
    };
    serde_json::from_value::<T>(normalized)
        .map(Some)
        .map_err(|_| {
            Error::bad_args(format!(
                "field '{key}' must be one of {accepted}; got {value}"
            ))
        })
}

/// `--params '{"amount": 4}'` arrives as a JSON string from a shell and as an object from
/// MCP. Both are accepted so a caller does not have to know which surface it is on.
fn opt_params(
    args: &serde_json::Value,
    key: &str,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(serde_json::Map::new()),
        Some(serde_json::Value::Object(map)) => Ok(map.clone()),
        Some(serde_json::Value::String(text)) => {
            match serde_json::from_str::<serde_json::Value>(text) {
                Ok(serde_json::Value::Object(map)) => Ok(map),
                _ => Err(Error::bad_args(format!(
                    "field '{key}' must be a JSON object, got '{text}'"
                ))),
            }
        }
        Some(other) => Err(Error::bad_args(format!(
            "field '{key}' must be a JSON object, got {other}"
        ))),
    }
}

/// Effects addressed by `--target`, optionally narrowed by `--effect`.
///
/// A selector may name effects outright (`fx[kind=blur]`), in which case `effect` only
/// filters; when it names clips, `effect` says which effect on each — by id or by kind, since
/// "the blur on that clip" is how an agent thinks and the ids are ULIDs it never typed.
fn resolve_effects(
    project: &Project,
    seq: &SequenceId,
    target: &str,
    wanted: Option<&str>,
) -> Result<Vec<(TrackId, ClipId, EffectId)>> {
    let parsed = Selector::parse(target)?;
    let mut direct: Vec<(TrackId, ClipId, EffectId)> = Vec::new();
    let mut clips: Vec<(TrackId, ClipId)> = Vec::new();
    for hit in selector::resolve(project, seq, &parsed)? {
        match hit {
            Match::Effect {
                track,
                clip,
                effect,
            } => direct.push((track, clip, effect)),
            Match::Clip { track, clip } => clips.push((track, clip)),
            _ => {}
        }
    }

    let sequence = project.sequence(seq)?;
    if !direct.is_empty() {
        let Some(wanted) = wanted else {
            return Ok(direct);
        };
        let candidates: Vec<String> = direct
            .iter()
            .map(|(_, _, effect)| effect.to_string())
            .collect();
        let filtered: Vec<(TrackId, ClipId, EffectId)> = direct
            .into_iter()
            .filter(|(_, clip, effect)| {
                effect.as_str() == wanted
                    || sequence
                        .find_clip(clip)
                        .and_then(|(_, clip)| clip.effect(effect))
                        .is_some_and(|fx| fx.kind == wanted)
            })
            .collect();
        if filtered.is_empty() {
            return Err(Error::no_match("effect", wanted, candidates));
        }
        return Ok(filtered);
    }

    let wanted = wanted.ok_or_else(|| {
        Error::bad_args(format!(
            "selector '{target}' names clips, not effects; pass effect with an effect id or kind"
        ))
    })?;
    let mut out: Vec<(TrackId, ClipId, EffectId)> = Vec::new();
    for (track_id, clip_id) in clips {
        let (_, clip) = sequence
            .find_clip(&clip_id)
            .ok_or_else(|| Error::no_match("clip", clip_id.as_str(), sequence.clip_ids()))?;
        let hits: Vec<EffectId> = clip
            .effects
            .iter()
            .filter(|fx| fx.id.as_str() == wanted || fx.kind == wanted)
            .map(|fx| fx.id.clone())
            .collect();
        if hits.is_empty() {
            return Err(Error::no_match("effect", wanted, effect_labels(clip)));
        }
        out.extend(
            hits.into_iter()
                .map(|effect| (track_id.clone(), clip_id.clone(), effect)),
        );
    }
    if out.is_empty() {
        return Err(Error::no_match("clip", target, sequence.clip_ids()));
    }
    Ok(out)
}

/// How an error names the effects a clip really has: id plus kind, because the agent knows
/// the kind it asked for and has never seen the id.
fn effect_labels(clip: &Clip) -> Vec<String> {
    clip.effects
        .iter()
        .map(|fx| format!("{} ({})", fx.id, fx.kind))
        .collect()
}

/// Locate one clip for mutation, refusing a locked track. Every keyframe op needs exactly
/// this; doing the lock check here makes it impossible to forget in one of them.
fn clip_for_edit<'a>(
    project: &'a mut Project,
    seq: &SequenceId,
    track: &TrackId,
    clip: &ClipId,
) -> Result<&'a mut Clip> {
    let track = project.sequence_mut(seq)?.track_mut(track)?;
    assert_unlocked(track)?;
    let index = clip_index(track, clip)?;
    Ok(&mut track.clips[index])
}

/// Is this path something the compositor will actually animate on this clip?
fn validate_path(clip: &Clip, path: &str) -> Result<()> {
    if KEYFRAME_PATHS.contains(&path) {
        return Ok(());
    }
    if let Some(rest) = path.strip_prefix("fx.") {
        let (id, param) = rest.split_once('.').ok_or_else(|| {
            Error::bad_args(format!(
                "keyframe path '{path}' needs the form fx.<effectId>.<param>"
            ))
        })?;
        if param.is_empty() {
            return Err(Error::bad_args(format!(
                "keyframe path '{path}' names no parameter"
            )));
        }
        if clip.effects.iter().any(|fx| fx.id.as_str() == id) {
            return Ok(());
        }
        return Err(Error::no_match("effect", id, effect_labels(clip)));
    }
    Err(Error::bad_args(format!(
        "keyframe path '{path}' is not animatable; expected one of {} or fx.<effectId>.<param>",
        KEYFRAME_PATHS.join(", ")
    )))
}

/// Keyframe times are clip-local, which is the one place an agent reliably passes a timeline
/// time by mistake. Refusing a key outside the clip turns that into an error instead of a
/// curve that holds one value forever.
fn local_time(clip: &Clip, at: Time) -> Result<()> {
    if at.is_negative() || at > clip.duration {
        return Err(Error::bad_args(format!(
            "keyframe at {at} is outside clip '{}'; clip-local time runs 0 to {}",
            clip.label(),
            clip.duration
        )));
    }
    Ok(())
}

pub struct FxAdd;

impl Op for FxAdd {
    fn id(&self) -> &'static str {
        "fx.add"
    }

    fn about(&self) -> &'static str {
        "Add an effect to every clip the target selects"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "Clip selector", "examples": ["#intro", "clip[track=V1]"] },
                "kind": { "type": "string", "enum": FX_KINDS, "description": "Effect kind from the v1 catalog" },
                "params": { "description": "Effect parameters as a JSON object" }
            },
            "required": ["target", "kind"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "kind", "params"])?;
        let kind = args::str_field(&args, "kind")?.trim().to_string();
        if !FX_KINDS.contains(&kind.as_str()) {
            return Err(Error::bad_args(format!(
                "unknown effect kind '{kind}'; the v1 catalog is {}",
                FX_KINDS.join(", ")
            )));
        }
        let seq = cx.sequence(project)?;
        let params = opt_params(&args, "params")?;
        let targets = selector::resolve_clips(project, &seq, args::str_field(&args, "target")?)?;

        // Validate every target before touching any of them, so a locked track halfway
        // through a multi-clip selector cannot leave half the effects applied.
        let sequence = project.sequence(&seq)?;
        for (track_id, _) in &targets {
            assert_unlocked(sequence.track(track_id)?)?;
        }

        let mut effect = OpEffect::new();
        for (track_id, clip_id) in &targets {
            let clip = clip_for_edit(project, &seq, track_id, clip_id)?;
            let mut added = Effect::new(kind.clone());
            added.params = params.clone();
            effect = effect.created(&added.id).changed(clip_id);
            clip.effects.push(added);
        }
        Ok(effect)
    }
}

pub struct FxRemove;

impl Op for FxRemove {
    fn id(&self) -> &'static str {
        "fx.remove"
    }

    fn about(&self) -> &'static str {
        "Remove an effect from the clips the target selects"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "Clip or effect selector" },
                "effect": { "type": "string", "description": "Effect id or kind; required when the target names clips" }
            },
            "required": ["target"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "effect"])?;
        let seq = cx.sequence(project)?;
        let hits = resolve_effects(
            project,
            &seq,
            args::str_field(&args, "target")?,
            args::opt_str(&args, "effect"),
        )?;

        let mut effect = OpEffect::new();
        for (track_id, clip_id, effect_id) in &hits {
            let clip = clip_for_edit(project, &seq, track_id, clip_id)?;
            let index = clip
                .effects
                .iter()
                .position(|fx| fx.id == *effect_id)
                .ok_or_else(|| {
                    Error::no_match("effect", effect_id.as_str(), effect_labels(clip))
                })?;
            clip.effects.remove(index);
            // A keyframe curve for an effect that no longer exists would animate nothing,
            // and lint would report it forever; it goes with the effect.
            let orphaned: Vec<String> = clip
                .keyframes
                .keys()
                .filter(|path| path.starts_with(&format!("fx.{effect_id}.")))
                .cloned()
                .collect();
            for path in orphaned {
                clip.keyframes.remove(&path);
            }
            effect = effect.removed(effect_id).changed(clip_id);
        }
        Ok(effect)
    }
}

pub struct FxSet;

impl Op for FxSet {
    fn id(&self) -> &'static str {
        "fx.set"
    }

    fn about(&self) -> &'static str {
        "Merge parameters into an existing effect; a null value clears a parameter"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "Clip or effect selector" },
                "effect": { "type": "string", "description": "Effect id or kind; required when the target names clips" },
                "params": { "type": "object", "description": "Parameters to merge; a null value removes that parameter" }
            },
            "required": ["target", "params"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "effect", "params"])?;
        let seq = cx.sequence(project)?;
        let params = opt_params(&args, "params")?;
        if params.is_empty() {
            return Err(Error::bad_args(
                "fx.set needs params; use fx.remove to drop an effect entirely",
            ));
        }
        let hits = resolve_effects(
            project,
            &seq,
            args::str_field(&args, "target")?,
            args::opt_str(&args, "effect"),
        )?;

        let mut effect = OpEffect::new();
        for (track_id, clip_id, effect_id) in &hits {
            let clip = clip_for_edit(project, &seq, track_id, clip_id)?;
            let labels = effect_labels(clip);
            let fx = clip
                .effects
                .iter_mut()
                .find(|fx| fx.id == *effect_id)
                .ok_or_else(|| Error::no_match("effect", effect_id.as_str(), labels))?;
            for (key, value) in params.clone() {
                if value.is_null() {
                    fx.params.remove(&key);
                } else {
                    fx.params.insert(key, value);
                }
            }
            effect = effect.changed(effect_id).changed(clip_id);
        }
        Ok(effect)
    }
}

pub struct FxReorder;

impl Op for FxReorder {
    fn id(&self) -> &'static str {
        "fx.reorder"
    }

    fn about(&self) -> &'static str {
        "Move an effect in a clip's chain; effects apply in order"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "Clip or effect selector" },
                "effect": { "type": "string", "description": "Effect id or kind; required when the target names clips" },
                "index": { "type": "integer", "description": "Destination position in the chain; past the end means last" }
            },
            "required": ["target", "index"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "effect", "index"])?;
        let seq = cx.sequence(project)?;
        let requested = super::track::opt_index(&args, "index")?
            .ok_or_else(|| Error::bad_args("missing index field 'index'"))?;
        let hits = resolve_effects(
            project,
            &seq,
            args::str_field(&args, "target")?,
            args::opt_str(&args, "effect"),
        )?;
        if hits.len() != 1 {
            return Err(Error::bad_args(format!(
                "fx.reorder moves one effect; the target selected {}",
                hits.len()
            )));
        }
        let (track_id, clip_id, effect_id) = &hits[0];

        let clip = clip_for_edit(project, &seq, track_id, clip_id)?;
        let from = clip
            .effects
            .iter()
            .position(|fx| fx.id == *effect_id)
            .ok_or_else(|| Error::no_match("effect", effect_id.as_str(), Vec::new()))?;
        let last = clip.effects.len() - 1;
        let mut effect = OpEffect::new().changed(effect_id).changed(clip_id);
        let to = if requested > last {
            effect = effect.warn(
                "index-clamped",
                effect_id,
                format!("index {requested} is past the end of the chain; moved to {last}"),
            );
            last
        } else {
            requested
        };
        if to != from {
            let fx = clip.effects.remove(from);
            clip.effects.insert(to, fx);
        }
        Ok(effect)
    }
}

pub struct FxEnable;

impl Op for FxEnable {
    fn id(&self) -> &'static str {
        "fx.enable"
    }

    fn about(&self) -> &'static str {
        "Enable or bypass an effect without removing it"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "Clip or effect selector" },
                "effect": { "type": "string", "description": "Effect id or kind; required when the target names clips" },
                "on": { "type": "boolean", "description": "Enable the effect; pass false to bypass it. Defaults to true" }
            },
            "required": ["target"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "effect", "on"])?;
        let seq = cx.sequence(project)?;
        let on = args::opt_bool(&args, "on")?.unwrap_or(true);
        let hits = resolve_effects(
            project,
            &seq,
            args::str_field(&args, "target")?,
            args::opt_str(&args, "effect"),
        )?;

        let mut effect = OpEffect::new();
        for (track_id, clip_id, effect_id) in &hits {
            let clip = clip_for_edit(project, &seq, track_id, clip_id)?;
            let labels = effect_labels(clip);
            let fx = clip
                .effects
                .iter_mut()
                .find(|fx| fx.id == *effect_id)
                .ok_or_else(|| Error::no_match("effect", effect_id.as_str(), labels))?;
            fx.enabled = on;
            effect = effect.changed(effect_id).changed(clip_id);
        }
        Ok(effect)
    }
}

pub struct KfSet;

impl Op for KfSet {
    fn id(&self) -> &'static str {
        "kf.set"
    }

    fn about(&self) -> &'static str {
        "Set a keyframe on a clip parameter at a clip-local time"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "Clip selector; exactly one clip" },
                "path": {
                    "type": "string",
                    "description": "Animated parameter: one of the clip paths or fx.<effectId>.<param>",
                    "examples": KEYFRAME_PATHS
                },
                "at": { "type": "string", "description": "Clip-local time, snapped to the sequence grid", "examples": ["0/1", "2.5", "60f"] },
                "value": { "type": "number", "description": "Parameter value at that time" },
                "easing": {
                    "type": "string",
                    "enum": ["hold", "linear", "ease-in", "ease-out", "ease-in-out"],
                    "description": "Shape of the curve from this key to the next; defaults to linear"
                }
            },
            "required": ["target", "path", "at", "value"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "path", "at", "value", "easing"])?;
        let seq = cx.sequence(project)?;
        let fps = sequence_fps(project, &seq)?;
        let (track_id, clip_id) =
            selector::resolve_one_clip(project, &seq, args::str_field(&args, "target")?)?;
        let path = args::str_field(&args, "path")?.trim().to_string();
        let requested = args::time_field(&args, "at", fps)?;
        let value = args::f64_field(&args, "value")?;
        if !value.is_finite() {
            // A non-finite value cannot even be serialized back out as JSON.
            return Err(Error::bad_args(format!("keyframe value {value} is not finite")));
        }
        let easing: Easing = opt_enum(
            &args,
            "easing",
            "hold, linear, ease-in, ease-out, ease-in-out",
        )?
        .unwrap_or_default();

        let clip = clip_for_edit(project, &seq, &track_id, &clip_id)?;
        validate_path(clip, &path)?;
        let at = requested.snap(fps);
        local_time(clip, at)?;

        let keys = clip.keyframes.entry(path).or_default();
        let key = Keyframe { at, value, easing };
        match keys.binary_search_by(|existing| existing.at.cmp(&at)) {
            // One key per time: a second key at the same instant would make the curve
            // depend on insertion order.
            Ok(index) => keys[index] = key,
            Err(index) => keys.insert(index, key),
        }
        Ok(OpEffect::new()
            .changed(&clip_id)
            .snap("at", requested, at, fps))
    }
}

pub struct KfRemove;

impl Op for KfRemove {
    fn id(&self) -> &'static str {
        "kf.remove"
    }

    fn about(&self) -> &'static str {
        "Remove the keyframe at a time"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "Clip selector; exactly one clip" },
                "path": { "type": "string", "description": "Animated parameter" },
                "at": { "type": "string", "description": "Clip-local time of the key" }
            },
            "required": ["target", "path", "at"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "path", "at"])?;
        let seq = cx.sequence(project)?;
        let fps = sequence_fps(project, &seq)?;
        let (track_id, clip_id) =
            selector::resolve_one_clip(project, &seq, args::str_field(&args, "target")?)?;
        let path = args::str_field(&args, "path")?.trim().to_string();
        let requested = args::time_field(&args, "at", fps)?;

        let clip = clip_for_edit(project, &seq, &track_id, &clip_id)?;
        let at = requested.snap(fps);
        let paths: Vec<String> = clip.keyframes.keys().cloned().collect();
        let keys = clip
            .keyframes
            .get_mut(&path)
            .ok_or_else(|| Error::no_match("keyframe path", path.clone(), paths))?;
        let index = keys
            .binary_search_by(|existing| existing.at.cmp(&at))
            .map_err(|_| {
                Error::no_match(
                    "keyframe",
                    at.to_string(),
                    keys.iter().map(|key| key.at.to_string()).collect(),
                )
            })?;
        keys.remove(index);
        let emptied = keys.is_empty();
        if emptied {
            // An empty curve is not the same as no curve: `param_at` falls back to the
            // static value only when the path is absent.
            clip.keyframes.remove(&path);
        }
        Ok(OpEffect::new()
            .changed(&clip_id)
            .snap("at", requested, at, fps))
    }
}

pub struct KfClear;

impl Op for KfClear {
    fn id(&self) -> &'static str {
        "kf.clear"
    }

    fn about(&self) -> &'static str {
        "Remove every keyframe on a parameter, restoring its static value"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "Clip selector; exactly one clip" },
                "path": { "type": "string", "description": "Animated parameter" }
            },
            "required": ["target", "path"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "path"])?;
        let seq = cx.sequence(project)?;
        let (track_id, clip_id) =
            selector::resolve_one_clip(project, &seq, args::str_field(&args, "target")?)?;
        let path = args::str_field(&args, "path")?.trim().to_string();

        let clip = clip_for_edit(project, &seq, &track_id, &clip_id)?;
        let paths: Vec<String> = clip.keyframes.keys().cloned().collect();
        let removed = clip.keyframes.remove(&path);
        if removed.is_none() {
            return Err(Error::no_match("keyframe path", path, paths));
        }
        Ok(OpEffect::new().changed(&clip_id))
    }
}

pub struct KfEase;

impl Op for KfEase {
    fn id(&self) -> &'static str {
        "kf.ease"
    }

    fn about(&self) -> &'static str {
        "Change the easing of the keyframe at a time"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "Clip selector; exactly one clip" },
                "path": { "type": "string", "description": "Animated parameter" },
                "at": { "type": "string", "description": "Clip-local time of the key" },
                "easing": {
                    "type": "string",
                    "enum": ["hold", "linear", "ease-in", "ease-out", "ease-in-out"],
                    "description": "Shape of the curve from this key to the next"
                }
            },
            "required": ["target", "path", "at", "easing"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "path", "at", "easing"])?;
        let seq = cx.sequence(project)?;
        let fps = sequence_fps(project, &seq)?;
        let (track_id, clip_id) =
            selector::resolve_one_clip(project, &seq, args::str_field(&args, "target")?)?;
        let path = args::str_field(&args, "path")?.trim().to_string();
        let requested = args::time_field(&args, "at", fps)?;
        let easing: Easing = opt_enum(
            &args,
            "easing",
            "hold, linear, ease-in, ease-out, ease-in-out",
        )?
        .ok_or_else(|| Error::bad_args("missing field 'easing'"))?;

        let clip = clip_for_edit(project, &seq, &track_id, &clip_id)?;
        let at = requested.snap(fps);
        let paths: Vec<String> = clip.keyframes.keys().cloned().collect();
        let keys = clip
            .keyframes
            .get_mut(&path)
            .ok_or_else(|| Error::no_match("keyframe path", path.clone(), paths))?;
        let index = keys
            .binary_search_by(|existing| existing.at.cmp(&at))
            .map_err(|_| {
                Error::no_match(
                    "keyframe",
                    at.to_string(),
                    keys.iter().map(|key| key.at.to_string()).collect(),
                )
            })?;
        keys[index].easing = easing;
        Ok(OpEffect::new()
            .changed(&clip_id)
            .snap("at", requested, at, fps))
    }
}

pub struct TransitionSet;

impl Op for TransitionSet {
    fn id(&self) -> &'static str {
        "transition.set"
    }

    fn about(&self) -> &'static str {
        "Blend the cut into a clip from the clip before it"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "The later clip: the one being transitioned into" },
                "kind": {
                    "type": "string",
                    "enum": ["cut", "dissolve", "dip", "wipe", "slide", "push"],
                    "description": "Blend to use; defaults to dissolve"
                },
                "duration": { "type": "string", "description": "Blend length; may not exceed either neighbour's duration", "examples": ["1/2", "0.5", "12f"] },
                "direction": {
                    "type": "string",
                    "enum": ["left", "right", "up", "down"],
                    "description": "Direction for wipe, slide and push"
                },
                "color": { "type": "string", "description": "Color for dip; defaults to the sequence background" }
            },
            "required": ["target", "duration"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "kind", "duration", "direction", "color"])?;
        let seq = cx.sequence(project)?;
        let fps = sequence_fps(project, &seq)?;
        let (track_id, clip_id) =
            selector::resolve_one_clip(project, &seq, args::str_field(&args, "target")?)?;
        let kind: TransitionKind =
            opt_enum(&args, "kind", "cut, dissolve, dip, wipe, slide, push")?
                // A cut is the absence of a transition, so it is never what `set` means by
                // default.
                .unwrap_or(TransitionKind::Dissolve);
        let direction: Direction =
            opt_enum(&args, "direction", "left, right, up, down")?.unwrap_or_default();
        let color = match args::opt_str(&args, "color") {
            Some(text) => Some(crate::color::Rgba::parse(text)?),
            None => None,
        };
        let requested = args::time_field(&args, "duration", fps)?;

        let track = project.sequence_mut(&seq)?.track_mut(&track_id)?;
        assert_unlocked(track)?;
        let index = clip_index(track, &clip_id)?;
        if index == 0 {
            return Err(Error::op(format!(
                "clip '{}' is first on track '{}'; a transition blends into a clip from the one before it",
                track.clips[index].label(),
                track.name
            )));
        }
        let (previous_label, previous_duration, previous_end) = {
            let previous = &track.clips[index - 1];
            (previous.label().to_string(), previous.duration, previous.end())
        };
        let clip = &track.clips[index];
        if previous_end != clip.start {
            return Err(Error::op(format!(
                "clips '{previous_label}' and '{}' are {} apart on track '{}'; close the gap before blending the cut",
                clip.label(),
                clip.start - previous_end,
                track.name
            )));
        }
        let duration = requested.snap(fps);
        if !duration.is_positive() {
            return Err(Error::bad_args(format!(
                "transition duration {requested} snaps to {duration} at {fps} fps, which is no frames at all"
            )));
        }
        if duration > previous_duration || duration > clip.duration {
            return Err(Error::op(format!(
                "transition of {duration} is longer than clip '{previous_label}' ({previous_duration}) or clip '{}' ({}); it may not exceed either neighbour",
                clip.label(),
                clip.duration
            )));
        }

        track.clips[index].transition_in = Some(Transition {
            kind,
            duration,
            easing: Easing::default(),
            direction,
            color,
        });
        Ok(OpEffect::new()
            .changed(&clip_id)
            .snap("duration", requested, duration, fps))
    }
}

pub struct TransitionRemove;

impl Op for TransitionRemove {
    fn id(&self) -> &'static str {
        "transition.remove"
    }

    fn about(&self) -> &'static str {
        "Turn a blended cut back into a hard cut"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "The later clip, the one carrying the transition" }
            },
            "required": ["target"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target"])?;
        let seq = cx.sequence(project)?;
        let (track_id, clip_id) =
            selector::resolve_one_clip(project, &seq, args::str_field(&args, "target")?)?;
        let clip = clip_for_edit(project, &seq, &track_id, &clip_id)?;
        if clip.transition_in.take().is_none() {
            return Err(Error::op(format!(
                "clip '{}' has no transition to remove",
                clip.label()
            )));
        }
        Ok(OpEffect::new().changed(&clip_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::AssetStore;
    use crate::color::Rgba;
    use crate::ids::TrackId;
    use crate::paths::ProjectPaths;
    use crate::project::{Source, Track, TrackKind};
    use crate::time::Fps;
    use crate::vfs::MemVfs;
    use std::sync::Arc;

    fn apply(op: &dyn Op, project: &mut Project, args: serde_json::Value) -> Result<OpEffect> {
        let paths = ProjectPaths::new("/p");
        let assets = AssetStore::new("/p/assets".into(), Arc::new(MemVfs::default()));
        let mut cx = OpCx::new(&paths, &assets);
        op.apply(project, args, &mut cx)
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

    /// V1 with two adjacent clips: `a` covering 0–2 s and `b` covering 2–3 s.
    fn project() -> Project {
        let mut project = Project::new(
            "promo",
            Fps::new(30, 1).expect("30 fps"),
            [1920, 1080],
            48_000,
        );
        let mut track = Track::new("V1", TrackKind::Video);
        track.id = TrackId::from_raw("trk_v1");
        track.clips = vec![clip("clp_a", 0, 2), clip("clp_b", 2, 1)];
        let seq = project.active_sequence.clone();
        project.sequence_mut(&seq).expect("main").tracks.push(track);
        project
    }

    fn clip_of(project: &Project, id: &str) -> Clip {
        let seq = project.active_sequence.clone();
        project
            .sequence(&seq)
            .expect("main")
            .find_clip(&ClipId::from_raw(id))
            .expect("clip exists")
            .1
            .clone()
    }

    fn with_blur(project: &mut Project) {
        let seq = project.active_sequence.clone();
        let mut fx = Effect::new("blur");
        fx.id = EffectId::from_raw("fx_blur");
        project.sequence_mut(&seq).expect("main").tracks[0].clips[0]
            .effects
            .push(fx);
    }

    #[test]
    fn an_unknown_effect_kind_lists_the_catalog() {
        let mut project = project();
        let err = apply(
            &FxAdd,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "kind": "bogus" }),
        )
        .expect_err("the catalog is closed");
        assert_eq!(err.exit_code(), crate::error::exit::BAD_ARGS);
        for kind in FX_KINDS {
            assert!(
                err.to_string().contains(kind),
                "catalog entry {kind} missing from: {err}"
            );
        }
        assert!(clip_of(&project, "clp_a").effects.is_empty());
    }

    #[test]
    fn adding_an_effect_reports_its_id_and_keeps_the_params() {
        let mut project = project();
        let effect = apply(
            &FxAdd,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "kind": "blur", "params": { "amount": 4 } }),
        )
        .expect("add blur");

        let clip = clip_of(&project, "clp_a");
        assert_eq!(clip.effects.len(), 1);
        assert_eq!(clip.effects[0].kind, "blur");
        assert_eq!(clip.effects[0].number("amount", 0.0), 4.0);
        assert_eq!(effect.created, vec![clip.effects[0].id.to_string()]);
    }

    #[test]
    fn setting_params_merges_and_a_null_clears() {
        let mut project = project();
        with_blur(&mut project);
        apply(
            &FxSet,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "effect": "blur", "params": { "amount": 8, "sigma": 2 } }),
        )
        .expect("merge");
        apply(
            &FxSet,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "effect": "fx_blur", "params": { "sigma": null } }),
        )
        .expect("clear one");

        let clip = clip_of(&project, "clp_a");
        assert_eq!(clip.effects[0].number("amount", 0.0), 8.0);
        assert!(
            !clip.effects[0].params.contains_key("sigma"),
            "a null value should clear the parameter: {:?}",
            clip.effects[0].params
        );
    }

    #[test]
    fn removing_an_effect_takes_its_keyframes_with_it() {
        let mut project = project();
        with_blur(&mut project);
        apply(
            &KfSet,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "path": "fx.fx_blur.amount", "at": "0", "value": 1 }),
        )
        .expect("animate the blur");
        apply(
            &FxRemove,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "effect": "blur" }),
        )
        .expect("remove");

        let clip = clip_of(&project, "clp_a");
        assert!(clip.effects.is_empty());
        assert!(
            clip.keyframes.is_empty(),
            "curves for a removed effect animate nothing: {:?}",
            clip.keyframes.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_unknown_effect_id_names_the_ones_the_clip_has() {
        let mut project = project();
        with_blur(&mut project);
        let err = apply(
            &KfSet,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "path": "fx.fx_nope.amount", "at": "0", "value": 1 }),
        )
        .expect_err("an effect id from another clip is not animatable here");
        assert_eq!(err.exit_code(), crate::error::exit::NO_MATCH);
        assert!(err.to_string().contains("fx_blur"), "{err}");
        assert!(err.to_string().contains("blur"), "{err}");
        assert!(clip_of(&project, "clp_a").keyframes.is_empty());
    }

    #[test]
    fn a_key_at_an_existing_time_replaces_it_and_the_list_stays_sorted() {
        let mut project = project();
        for (at, value) in [("1", 1.0), ("0", 0.0), ("0.5", 0.5)] {
            apply(
                &KfSet,
                &mut project,
                serde_json::json!({ "target": "#clp_a", "path": "opacity", "at": at, "value": value }),
            )
            .expect("set key");
        }
        apply(
            &KfSet,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "path": "opacity", "at": "0.5", "value": 0.25, "easing": "ease-in" }),
        )
        .expect("replace the middle key");

        let clip = clip_of(&project, "clp_a");
        let keys = &clip.keyframes["opacity"];
        assert_eq!(keys.len(), 3, "the key was duplicated: {keys:?}");
        let times: Vec<String> = keys.iter().map(|key| key.at.to_string()).collect();
        assert_eq!(times, vec!["0/1", "1/2", "1/1"]);
        assert_eq!(keys[1].value, 0.25);
        assert_eq!(keys[1].easing, Easing::EaseIn);
    }

    #[test]
    fn a_keyframe_path_the_compositor_does_not_know_is_refused() {
        let mut project = project();
        let err = apply(
            &KfSet,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "path": "transform.wobble", "at": "0", "value": 1 }),
        )
        .expect_err("unknown path");
        assert_eq!(err.exit_code(), crate::error::exit::BAD_ARGS);
        assert!(err.to_string().contains("opacity"), "{err}");
    }

    #[test]
    fn a_keyframe_outside_the_clip_is_refused_because_the_time_is_clip_local() {
        let mut project = project();
        // 2.5 s is inside clip `b` on the timeline but past the end of clip `a`.
        let err = apply(
            &KfSet,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "path": "opacity", "at": "2.5", "value": 1 }),
        )
        .expect_err("clip-local time");
        assert!(err.to_string().contains("clip-local"), "{err}");
    }

    #[test]
    fn removing_a_key_needs_one_to_be_there_and_drops_the_path_when_empty() {
        let mut project = project();
        apply(
            &KfSet,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "path": "opacity", "at": "1", "value": 0.5 }),
        )
        .expect("set");
        let err = apply(
            &KfRemove,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "path": "opacity", "at": "0" }),
        )
        .expect_err("no key at 0");
        assert!(err.to_string().contains("1/1"), "the times present should be listed: {err}");

        apply(
            &KfRemove,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "path": "opacity", "at": "1" }),
        )
        .expect("remove");
        assert!(
            clip_of(&project, "clp_a").keyframes.is_empty(),
            "an emptied curve should leave no path behind"
        );
    }

    #[test]
    fn keyframe_times_snap_to_the_sequence_grid_and_report_it() {
        let mut project = project();
        let effect = apply(
            &KfSet,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "path": "opacity", "at": "0.51", "value": 1 }),
        )
        .expect("set");
        assert_eq!(
            effect.snapped.first().map(|snap| snap.applied.clone()),
            Some("1/2".to_string()),
            "0.51 s is frame 15 at 30 fps: {:?}",
            effect.snapped
        );
        assert_eq!(
            clip_of(&project, "clp_a").keyframes["opacity"][0].at,
            Time::new(1, 2).unwrap()
        );
    }

    #[test]
    fn a_transition_longer_than_a_neighbour_names_both_clips() {
        let mut project = project();
        let err = apply(
            &TransitionSet,
            &mut project,
            serde_json::json!({ "target": "#clp_b", "duration": "3" }),
        )
        .expect_err("3 s does not fit a 1 s clip");
        let message = err.to_string();
        assert!(
            message.contains("'a'") && message.contains("'b'"),
            "both neighbours should be named: {message}"
        );
        assert!(message.contains("2/1") && message.contains("1/1"), "{message}");
        assert!(clip_of(&project, "clp_b").transition_in.is_none());
    }

    #[test]
    fn a_transition_lands_on_the_later_clip() {
        let mut project = project();
        apply(
            &TransitionSet,
            &mut project,
            serde_json::json!({ "target": "#clp_b", "duration": "0.5", "kind": "wipe", "direction": "up" }),
        )
        .expect("set the transition");

        assert!(
            clip_of(&project, "clp_a").transition_in.is_none(),
            "the transition belongs to the clip being entered"
        );
        let transition = clip_of(&project, "clp_b")
            .transition_in
            .expect("clip b carries it");
        assert_eq!(transition.duration, Time::new(1, 2).unwrap());
        assert_eq!(transition.kind, TransitionKind::Wipe);
        assert_eq!(transition.direction, Direction::Up);

        apply(
            &TransitionRemove,
            &mut project,
            serde_json::json!({ "target": "#clp_b" }),
        )
        .expect("back to a hard cut");
        assert!(clip_of(&project, "clp_b").transition_in.is_none());
        let err = apply(
            &TransitionRemove,
            &mut project,
            serde_json::json!({ "target": "#clp_b" }),
        )
        .expect_err("nothing left to remove");
        assert!(err.to_string().contains("no transition"), "{err}");
    }

    #[test]
    fn the_first_clip_on_a_track_has_nothing_to_transition_from() {
        let mut project = project();
        let err = apply(
            &TransitionSet,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "duration": "0.5" }),
        )
        .expect_err("no preceding clip");
        assert!(err.to_string().contains("first"), "{err}");
        assert!(clip_of(&project, "clp_a").transition_in.is_none());
    }

    #[test]
    fn a_transition_needs_the_neighbour_to_be_adjacent() {
        let mut project = project();
        let seq = project.active_sequence.clone();
        project.sequence_mut(&seq).expect("main").tracks[0].clips[1].start = Time::from_secs(4);
        let err = apply(
            &TransitionSet,
            &mut project,
            serde_json::json!({ "target": "#clp_b", "duration": "0.5" }),
        )
        .expect_err("a gap is not a cut");
        assert!(err.to_string().contains("apart"), "{err}");
    }

    #[test]
    fn effects_apply_in_chain_order_and_reorder_clamps() {
        let mut project = project();
        for kind in ["blur", "sharpen", "crop"] {
            apply(
                &FxAdd,
                &mut project,
                serde_json::json!({ "target": "#clp_a", "kind": kind }),
            )
            .expect("add");
        }
        apply(
            &FxReorder,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "effect": "crop", "index": 0 }),
        )
        .expect("crop first");
        let kinds: Vec<String> = clip_of(&project, "clp_a")
            .effects
            .iter()
            .map(|fx| fx.kind.clone())
            .collect();
        assert_eq!(kinds, vec!["crop", "blur", "sharpen"]);

        let effect = apply(
            &FxReorder,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "effect": "crop", "index": 9 }),
        )
        .expect("crop last");
        let kinds: Vec<String> = clip_of(&project, "clp_a")
            .effects
            .iter()
            .map(|fx| fx.kind.clone())
            .collect();
        assert_eq!(kinds, vec!["blur", "sharpen", "crop"]);
        assert_eq!(
            effect.warnings.first().map(|warning| warning.code),
            Some("index-clamped")
        );
    }

    #[test]
    fn bypassing_an_effect_keeps_it_in_the_document() {
        let mut project = project();
        with_blur(&mut project);
        apply(
            &FxEnable,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "effect": "blur", "on": false }),
        )
        .expect("bypass");
        let clip = clip_of(&project, "clp_a");
        assert_eq!(clip.effects.len(), 1);
        assert!(!clip.effects[0].enabled);
    }

    #[test]
    fn a_locked_track_refuses_an_effect() {
        let mut project = project();
        let seq = project.active_sequence.clone();
        project.sequence_mut(&seq).expect("main").tracks[0].locked = true;
        apply(
            &FxAdd,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "kind": "blur" }),
        )
        .expect_err("locked tracks refuse mutation");
        assert!(clip_of(&project, "clp_a").effects.is_empty());
    }

    #[test]
    fn an_argument_typo_is_an_error_rather_than_a_no_op() {
        let mut project = project();
        let err = apply(
            &FxAdd,
            &mut project,
            serde_json::json!({ "target": "#clp_a", "kind": "blur", "parms": { "amount": 4 } }),
        )
        .expect_err("a typo'd argument name must not be ignored");
        assert_eq!(err.exit_code(), crate::error::exit::BAD_ARGS);
        assert!(err.to_string().contains("parms"), "{err}");
    }
}
