//! OpenTimelineIO JSON, written by hand.
//!
//! The `opentimelineio` crate on crates.io is a placeholder with no schema in it, and
//! linking the C++ library to emit a few hundred bytes of JSON would be an absurd trade.
//! The schema this writer targets is small and stable: a `Timeline.1` holding a
//! `Stack.1` of `Track.1`s, each a list of `Clip.1` and `Gap.1` children with
//! `TimeRange.1` source ranges built from `RationalTime.1`.
//!
//! OTIO's clock is a float rate with a float frame value, which is the one place this
//! project cannot keep its rational time — so the *frame index* is what gets written
//! (`value` is always a whole number) and the rate carries the fraction. A reader that
//! multiplies frames by `1/rate` gets the same instants back; one that adds up float
//! seconds does not, and that is OTIO's problem to have, not ours to make worse by
//! writing fractional frame positions.
//!
//! Every track is padded to the full timeline length with a trailing gap, because a
//! stack of tracks that disagree about where the timeline ends is how a conform ends up
//! one frame short.

use crate::timeline::{self, Entry, Resource};
use crate::Export;
use dvs_core::asset::AssetStore;
use dvs_core::error::{Error, Result};
use dvs_core::ids::SequenceId;
use dvs_core::op::Warning;
use dvs_core::paths::ProjectPaths;
use dvs_core::project::{Project, TrackKind};
use serde_json::{json, Map, Value};

/// Write the sequence as an OpenTimelineIO document.
pub fn export(
    project: &Project,
    sequence: &SequenceId,
    paths: &ProjectPaths,
    assets: &AssetStore,
) -> Result<Export> {
    let sequence = project.sequence(sequence)?;
    let fps = sequence.fps;
    let rate = fps.as_f64();
    let flat = timeline::flatten(sequence);
    let mut warnings: Vec<Warning> = flat.iter().flat_map(|t| t.warnings.clone()).collect();
    let total = flat.iter().map(|t| t.frames).max().unwrap_or(0).max(1);

    let mut tracks = Vec::with_capacity(flat.len());
    for track in &flat {
        let mut children: Vec<Value> = Vec::with_capacity(track.entries.len());
        for entry in &track.entries {
            match entry {
                Entry::Blank { frames } => children.push(gap("gap", *frames, rate)),
                Entry::Clip(placed) => {
                    warnings.extend(timeline::feature_warnings(placed.clip, "OTIO"));
                    warnings.extend(timeline::retime_warning(placed.clip, "OTIO"));
                    let (resource, warning) =
                        timeline::resolve_source(project, assets, placed.clip)?;
                    warnings.extend(warning);
                    match resource {
                        Resource::File { asset, path } => {
                            let available = asset.probe.duration.frame_ceil(fps).max(1);
                            children.push(json!({
                                "OTIO_SCHEMA": "Clip.1",
                                "metadata": {
                                    "dvs": {
                                        "clip": placed.clip.id.to_string(),
                                        "asset": asset.id.to_string(),
                                        "speed": placed.clip.speed.to_string(),
                                    }
                                },
                                "name": placed.label(),
                                "source_range": range(placed.source_in, placed.frames, rate),
                                "effects": [],
                                "markers": [],
                                "enabled": true,
                                "media_reference": {
                                    "OTIO_SCHEMA": "ExternalReference.1",
                                    "metadata": {},
                                    "name": asset.name.clone(),
                                    "available_range": range(0, available, rate),
                                    "available_image_bounds": Value::Null,
                                    "target_url": timeline::file_url(&path),
                                },
                            }));
                        }
                        // A color, a title or a generator has no external media; a gap of
                        // the same length keeps every later clip on its frame.
                        Resource::Color(_) => {
                            children.push(gap(&placed.label(), placed.frames, rate))
                        }
                    }
                }
            }
        }
        if track.frames < total {
            children.push(gap("gap", total - track.frames, rate));
        }
        tracks.push(json!({
            "OTIO_SCHEMA": "Track.1",
            "metadata": {
                "dvs": { "track": track.track.id.to_string() }
            },
            "name": track.track.name.clone(),
            "source_range": Value::Null,
            "effects": [],
            "markers": [],
            "enabled": !track.track.muted && !track.track.hidden,
            "children": children,
            "kind": match track.track.kind {
                TrackKind::Audio => "Audio",
                _ => "Video",
            },
        }));
    }

    let markers: Vec<Value> = sequence
        .markers
        .iter()
        .map(|marker| {
            json!({
                "OTIO_SCHEMA": "Marker.2",
                "metadata": {},
                "name": marker.name.clone(),
                "color": "RED",
                "comment": marker.note.clone().unwrap_or_default(),
                "marked_range": range(marker.at.frame_round(fps), 0, rate),
            })
        })
        .collect();

    let mut metadata = Map::new();
    metadata.insert(
        "dvs".to_string(),
        json!({
            "project": project.name.clone(),
            "projectRoot": paths.root().display().to_string(),
            "sequence": sequence.id.to_string(),
            "fps": fps.to_string(),
            "size": sequence.size,
            "sampleRate": sequence.sample_rate,
        }),
    );

    let document = json!({
        "OTIO_SCHEMA": "Timeline.1",
        "metadata": metadata,
        "name": sequence.name.clone(),
        "global_start_time": time(0, rate),
        "tracks": {
            "OTIO_SCHEMA": "Stack.1",
            "metadata": {},
            "name": "tracks",
            "source_range": Value::Null,
            "effects": [],
            "markers": markers,
            "enabled": true,
            "children": tracks,
        },
    });

    let mut text = serde_json::to_string_pretty(&document)
        .map_err(|e| Error::op(format!("writing OTIO: {e}")))?;
    text.push('\n');
    Ok(Export { text, warnings })
}

fn time(frames: i64, rate: f64) -> Value {
    json!({
        "OTIO_SCHEMA": "RationalTime.1",
        "rate": rate,
        "value": frames as f64,
    })
}

fn range(start: i64, frames: i64, rate: f64) -> Value {
    json!({
        "OTIO_SCHEMA": "TimeRange.1",
        "duration": time(frames, rate),
        "start_time": time(start, rate),
    })
}

fn gap(name: &str, frames: i64, rate: f64) -> Value {
    json!({
        "OTIO_SCHEMA": "Gap.1",
        "metadata": {},
        "name": name,
        "source_range": range(0, frames, rate),
        "effects": [],
        "markers": [],
        "enabled": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::fixture;
    use dvs_core::time::Fps;

    fn document() -> Value {
        let mut fixture = fixture(Fps::new(30000, 1001).unwrap());
        fixture.push_clip("V1", "0", "1", "2");
        fixture.push_clip("V1", "2", "1", "0");
        fixture.push_second("A1", "0", "1.5", "0");
        let export = export(
            &fixture.project,
            &fixture.sequence,
            &fixture.paths,
            &fixture.assets,
        )
        .expect("export");
        serde_json::from_str(&export.text).expect("the document is JSON")
    }

    fn walk(node: &Value, visit: &mut impl FnMut(&Map<String, Value>)) {
        match node {
            Value::Object(map) => {
                if map.contains_key("OTIO_SCHEMA") {
                    visit(map);
                }
                for value in map.values() {
                    walk(value, visit);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, visit)),
            _ => {}
        }
    }

    #[test]
    fn every_node_declares_its_schema() {
        let document = document();
        assert_eq!(document["OTIO_SCHEMA"], "Timeline.1");
        assert_eq!(document["tracks"]["OTIO_SCHEMA"], "Stack.1");
        let mut schemas: Vec<String> = Vec::new();
        walk(&document, &mut |map| {
            schemas.push(
                map["OTIO_SCHEMA"]
                    .as_str()
                    .expect("a schema is a string")
                    .to_string(),
            );
        });
        for expected in [
            "Timeline.1",
            "Stack.1",
            "Track.1",
            "Clip.1",
            "Gap.1",
            "ExternalReference.1",
            "TimeRange.1",
            "RationalTime.1",
        ] {
            assert!(
                schemas.iter().any(|schema| schema == expected),
                "no {expected} node in the document; saw {schemas:?}"
            );
        }
    }

    #[test]
    fn each_tracks_children_sum_to_the_timeline_length() {
        let document = document();
        let tracks = document["tracks"]["children"]
            .as_array()
            .expect("tracks")
            .clone();
        assert_eq!(tracks.len(), 2);
        // Three seconds at 30000/1001 is ninety frames: two one-second clips with a
        // one-second hole between them.
        for track in &tracks {
            let total: f64 = track["children"]
                .as_array()
                .expect("children")
                .iter()
                .map(|child| {
                    child["source_range"]["duration"]["value"]
                        .as_f64()
                        .expect("a duration")
                })
                .sum();
            assert_eq!(
                total, 90.0,
                "track '{}' covers {total} frames, the timeline is 90",
                track["name"]
            );
        }
    }

    #[test]
    fn frame_positions_are_whole_and_the_rate_carries_the_fraction() {
        let document = document();
        let mut values = Vec::new();
        walk(&document, &mut |map| {
            if map["OTIO_SCHEMA"] == "RationalTime.1" {
                values.push((
                    map["value"].as_f64().expect("value"),
                    map["rate"].as_f64().expect("rate"),
                ));
            }
        });
        assert!(!values.is_empty());
        for (value, rate) in values {
            assert_eq!(value.fract(), 0.0, "{value} is not a whole frame");
            assert!(
                (rate - 30000.0 / 1001.0).abs() < 1e-9,
                "rate {rate} is not 30000/1001"
            );
        }
    }

    #[test]
    fn clips_carry_a_file_url_and_their_source_in_point() {
        let document = document();
        let first = &document["tracks"]["children"][0]["children"][0];
        assert_eq!(first["OTIO_SCHEMA"], "Clip.1");
        assert_eq!(
            first["source_range"]["start_time"]["value"], 60.0,
            "a two-second in point at 29.97 is frame 60"
        );
        let url = first["media_reference"]["target_url"]
            .as_str()
            .expect("a target url");
        assert!(url.starts_with("file:///"), "{url}");
        assert_eq!(first["media_reference"]["OTIO_SCHEMA"], "ExternalReference.1");
    }
}
