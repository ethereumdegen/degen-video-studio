//! Markers: named instants on a sequence.
//!
//! Markers are how an agent leaves itself an address it can find again — "the pricing bit is
//! here" — and how a human's GUI playhead notes survive a round trip. They carry no media and
//! affect no render, so the only invariants that matter are the two that make them addressable:
//! they sit on the frame grid like everything else, and the list stays sorted by time so
//! `digest` and playhead navigation agree on what "the next marker" means.

use crate::error::{Error, Result};
use crate::ids::{MarkerId, SequenceId};
use crate::op::{args, Op, OpCx, OpEffect, Registry};
use crate::ops::util::sequence_fps;
use crate::project::{Marker, Project};
use crate::selector::{self, Match, Selector};

pub fn register(registry: &mut Registry) {
    registry
        .register(MarkerAdd)
        .register(MarkerRemove)
        .register(MarkerRename)
        .register(MarkerClear);
}

/// Markers named by a selector.
///
/// A selector that resolves to something that is not a marker, or to nothing, is reported
/// against the marker list rather than against whatever the grammar happened to search: the
/// caller asked `marker.remove` for a marker, so the candidates worth printing are markers.
fn resolve_markers(
    project: &Project,
    seq: &SequenceId,
    text: &str,
) -> Result<Vec<MarkerId>> {
    let names: Vec<String> = project
        .sequence(seq)?
        .markers
        .iter()
        .map(|marker| marker.name.clone())
        .collect();
    let matches = match Selector::parse(text).and_then(|parsed| selector::resolve(project, seq, &parsed)) {
        Ok(matches) => matches,
        Err(Error::NoMatch { .. }) => return Err(Error::no_match("marker", text, names)),
        Err(other) => return Err(other),
    };
    let ids: Vec<MarkerId> = matches
        .into_iter()
        .filter_map(|hit| match hit {
            Match::Marker(id) => Some(id),
            _ => None,
        })
        .collect();
    if ids.is_empty() {
        return Err(Error::no_match("marker", text, names));
    }
    Ok(ids)
}

pub struct MarkerAdd;

impl Op for MarkerAdd {
    fn id(&self) -> &'static str {
        "marker.add"
    }

    fn about(&self) -> &'static str {
        "Mark an instant on the sequence"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "at": {
                    "type": "string",
                    "description": "Position, snapped to the sequence frame grid",
                    "examples": ["42/1", "00:01:12.500", "1800f"]
                },
                "name": {
                    "type": "string",
                    "description": "Marker name; defaults to the timecode it landed on"
                },
                "color": { "type": "string", "description": "Hex color for the GUI", "examples": ["#fb8500"] },
                "note": { "type": "string", "description": "Longer text an agent can read back later" }
            },
            "required": ["at"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["at", "name", "color", "note"])?;
        let seq = cx.sequence(project)?;
        let fps = sequence_fps(project, &seq)?;
        let requested = args::time_field(&args, "at", fps)?;
        let at = requested.snap(fps);
        if at.is_negative() {
            return Err(Error::bad_args(format!(
                "marker position {requested} is before the start of the sequence"
            )));
        }
        let color = match args::opt_str(&args, "color") {
            Some(text) => Some(crate::color::Rgba::parse(text)?),
            None => None,
        };
        let name = match args::opt_str(&args, "name").map(str::trim) {
            Some(name) if !name.is_empty() => name.to_string(),
            // An unnamed marker would still be addressable by id, but a timecode is what a
            // human reading `history.jsonl` can recognize.
            _ => at.timecode(fps),
        };
        let note = args::opt_str(&args, "note").map(str::to_string);

        let marker = Marker {
            id: MarkerId::new(),
            at,
            name,
            color,
            note,
        };
        let id = marker.id.clone();
        let sequence = project.sequence_mut(&seq)?;
        let index = sequence.markers.partition_point(|existing| existing.at <= at);
        sequence.markers.insert(index, marker);
        Ok(OpEffect::new()
            .created(&id)
            .snap("at", requested, at, fps))
    }
}

pub struct MarkerRemove;

impl Op for MarkerRemove {
    fn id(&self) -> &'static str {
        "marker.remove"
    }

    fn about(&self) -> &'static str {
        "Remove the markers a selector names"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Marker selector",
                    "examples": ["pricing", "mk_01J", "marker[name=pricing]"]
                }
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
        let doomed = resolve_markers(project, &seq, args::str_field(&args, "target")?)?;
        let sequence = project.sequence_mut(&seq)?;
        sequence.markers.retain(|marker| !doomed.contains(&marker.id));
        let mut effect = OpEffect::new();
        for id in &doomed {
            effect = effect.removed(id);
        }
        Ok(effect)
    }
}

pub struct MarkerRename;

impl Op for MarkerRename {
    fn id(&self) -> &'static str {
        "marker.rename"
    }

    fn about(&self) -> &'static str {
        "Rename a marker; its id is stable and does not change"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "Marker selector; exactly one marker" },
                "name": { "type": "string", "description": "New name" }
            },
            "required": ["target", "name"],
            "additionalProperties": false
        })
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
            return Err(Error::bad_args("marker name is empty"));
        }
        let target = args::str_field(&args, "target")?;
        let matched = resolve_markers(project, &seq, target)?;
        let [id] = matched.as_slice() else {
            return Err(Error::bad_args(format!(
                "selector '{target}' names {} markers; rename takes exactly one",
                matched.len()
            )));
        };
        let id = id.clone();

        let sequence = project.sequence_mut(&seq)?;
        let marker = sequence
            .markers
            .iter_mut()
            .find(|marker| marker.id == id)
            .ok_or_else(|| Error::no_match("marker", id.as_str(), Vec::new()))?;
        marker.name = name;
        Ok(OpEffect::new().changed(&id))
    }
}

pub struct MarkerClear;

impl Op for MarkerClear {
    fn id(&self) -> &'static str {
        "marker.clear"
    }

    fn about(&self) -> &'static str {
        "Remove every marker from the sequence"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &[])?;
        let seq = cx.sequence(project)?;
        let sequence = project.sequence_mut(&seq)?;
        let mut effect = OpEffect::new();
        for marker in sequence.markers.drain(..) {
            effect = effect.removed(&marker.id);
        }
        Ok(effect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::AssetStore;
    use crate::paths::ProjectPaths;
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
            Fps::new(30000, 1001).expect("29.97"),
            [1920, 1080],
            48_000,
        )
    }

    fn markers(project: &Project) -> Vec<(String, String)> {
        let seq = project.active_sequence.clone();
        project
            .sequence(&seq)
            .expect("main")
            .markers
            .iter()
            .map(|marker| (marker.name.clone(), marker.at.to_string()))
            .collect()
    }

    #[test]
    fn a_marker_snaps_to_the_grid_and_reports_where_it_landed() {
        let mut project = project();
        let effect = apply(
            &MarkerAdd,
            &mut project,
            serde_json::json!({ "at": "42.5", "name": "pricing", "color": "#fb8500", "note": "the good bit" }),
        )
        .expect("add marker");

        let snap = effect.snapped.first().expect("42.5 s is not a frame at 29.97");
        assert_eq!(snap.field, "at");
        assert_eq!(snap.frame, 1274);
        let seq = project.active_sequence.clone();
        let marker = &project.sequence(&seq).expect("main").markers[0];
        assert_eq!(marker.at, Time::from_frames(1274, Fps::new(30000, 1001).unwrap()));
        assert_eq!(marker.note.as_deref(), Some("the good bit"));
        assert_eq!(effect.created, vec![marker.id.to_string()]);
    }

    #[test]
    fn markers_stay_sorted_by_time() {
        let mut project = project();
        for (at, name) in [("10", "third"), ("2", "first"), ("5", "second")] {
            apply(
                &MarkerAdd,
                &mut project,
                serde_json::json!({ "at": at, "name": name }),
            )
            .expect("add");
        }
        let names: Vec<String> = markers(&project)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, vec!["first", "second", "third"]);
    }

    #[test]
    fn an_unnamed_marker_is_labelled_with_its_timecode() {
        // On an exact grid the label is the position; 29.97 would drift by design.
        let mut project = Project::new(
            "promo",
            Fps::new(30, 1).expect("30 fps"),
            [1920, 1080],
            48_000,
        );
        apply(&MarkerAdd, &mut project, serde_json::json!({ "at": "1m2s" })).expect("add");
        assert_eq!(markers(&project)[0].0, "00:01:02:00");
    }

    #[test]
    fn removing_by_name_takes_only_that_marker_and_a_miss_lists_the_names() {
        let mut project = project();
        for name in ["pricing", "outro"] {
            apply(
                &MarkerAdd,
                &mut project,
                serde_json::json!({ "at": "1", "name": name }),
            )
            .expect("add");
        }

        let err = apply(
            &MarkerRemove,
            &mut project,
            serde_json::json!({ "target": "prcing" }),
        )
        .expect_err("a typo'd name is not a silent no-op");
        assert_eq!(err.exit_code(), crate::error::exit::NO_MATCH);
        assert!(err.to_string().contains("pricing"), "{err}");

        let effect = apply(
            &MarkerRemove,
            &mut project,
            serde_json::json!({ "target": "pricing" }),
        )
        .expect("remove");
        assert_eq!(effect.removed.len(), 1);
        let names: Vec<String> = markers(&project).into_iter().map(|(name, _)| name).collect();
        assert_eq!(names, vec!["outro"]);
    }

    #[test]
    fn renaming_keeps_the_id_and_the_position() {
        let mut project = project();
        apply(
            &MarkerAdd,
            &mut project,
            serde_json::json!({ "at": "3", "name": "pricing" }),
        )
        .expect("add");
        let seq = project.active_sequence.clone();
        let before = project.sequence(&seq).expect("main").markers[0].clone();

        apply(
            &MarkerRename,
            &mut project,
            serde_json::json!({ "target": "pricing", "name": "the price" }),
        )
        .expect("rename");

        let after = &project.sequence(&seq).expect("main").markers[0];
        assert_eq!(after.id, before.id);
        assert_eq!(after.at, before.at);
        assert_eq!(after.name, "the price");
    }

    #[test]
    fn clear_removes_everything_and_reports_the_ids() {
        let mut project = project();
        for name in ["a", "b", "c"] {
            apply(
                &MarkerAdd,
                &mut project,
                serde_json::json!({ "at": "1", "name": name }),
            )
            .expect("add");
        }
        let effect = apply(&MarkerClear, &mut project, serde_json::json!({})).expect("clear");
        assert_eq!(effect.removed.len(), 3);
        assert!(markers(&project).is_empty());
    }

    #[test]
    fn a_marker_before_zero_is_refused() {
        let mut project = project();
        let err = apply(&MarkerAdd, &mut project, serde_json::json!({ "at": "-1" }))
            .expect_err("there is no timeline before zero");
        assert_eq!(err.exit_code(), crate::error::exit::BAD_ARGS);
        assert!(markers(&project).is_empty());
    }
}
