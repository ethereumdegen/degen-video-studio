//! The ops that move a timeline in and out of other editors.
//!
//! Exports are query ops: they write a file and leave the document alone, so they never
//! enter the journal and `undo` never has to explain what undoing an export would mean.
//! Each one reports the path it wrote plus every feature it could not carry, because the
//! whole point of handing a project to Kdenlive is that the human on the other end knows
//! what is missing before they start working.
//!
//! `import.mlt` is the return path. Kdenlive normalizes a document on every save, so it
//! is deliberately narrow: cuts, positions and source in-points for media this project
//! already owns. A producer pointing at a file the project has never imported is
//! reported as dropped rather than guessed at, because inventing an asset from a path
//! found in someone else's XML is how a project ends up referring to media that moves.

use crate::{edl, fcpxml, mlt, otio, subtitle};
use dvs_core::error::{Error, Result};
use dvs_core::ids::AssetId;
use dvs_core::op::{args, Op, OpCx, OpEffect, Registry, Warning};
use dvs_core::project::{CaptionCue, Clip, Project, Source, Track, TrackKind};
use dvs_core::time::Time;
use std::path::{Path, PathBuf};

pub fn register(registry: &mut Registry) {
    registry.register(ExportKdenlive);
    registry.register(ExportMlt);
    registry.register(ExportFcpxml);
    registry.register(ExportOtio);
    registry.register(ExportEdl);
    registry.register(ExportSrt);
    registry.register(ImportMlt);
}

/// Schema shared by every export op. One required argument keeps the CLI and the MCP
/// tool list uniform: `dvs export <format> <out>`.
fn out_schema(what: &str, extension: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["out"],
        "properties": {
            "out": {
                "type": "string",
                "description": format!(
                    "file to write {what} to; a relative path is resolved against the project \
                     root (conventionally *.{extension})"
                )
            }
        }
    })
}

/// Absolute for an absolute argument, project-relative otherwise. An export is a file a
/// human goes looking for, so it lands where they asked and not in a cache directory.
fn destination(cx: &OpCx, out: &str) -> PathBuf {
    let path = Path::new(out);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cx.paths.root().join(path)
    }
}

/// Write an export and describe it. `--dry-run` reports the path and the warnings
/// without touching the filesystem, which is what makes "what would this lose?" a
/// question an agent can ask cheaply.
fn deliver(cx: &OpCx, out: &str, text: &str, warnings: Vec<Warning>) -> Result<OpEffect> {
    let path = destination(cx, out);
    if !cx.dry_run {
        cx.assets.vfs().write(&path, text.as_bytes())?;
    }
    let mut effect = OpEffect::new().data(serde_json::json!({
        "path": cx.paths.relativize(&path),
        "bytes": text.len(),
        "dryRun": cx.dry_run,
    }));
    effect.warnings = warnings;
    Ok(effect)
}

struct ExportKdenlive;

impl Op for ExportKdenlive {
    fn id(&self) -> &'static str {
        "export.kdenlive"
    }

    fn about(&self) -> &'static str {
        "write the sequence as a Kdenlive 26.08 project (MLT XML with kdenlive:* properties)"
    }

    fn schema(&self) -> serde_json::Value {
        out_schema("a Kdenlive project", "kdenlive")
    }

    fn is_query(&self) -> bool {
        true
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["out"])?;
        let out = args::str_field(&args, "out")?;
        let sequence = cx.sequence(project)?;
        let export = mlt::export(project, &sequence, cx.paths, cx.assets, true)?;
        deliver(cx, out, &export.text, export.warnings)
    }
}

struct ExportMlt;

impl Op for ExportMlt {
    fn id(&self) -> &'static str {
        "export.mlt"
    }

    fn about(&self) -> &'static str {
        "write the sequence as plain MLT XML, for melt and other MLT hosts"
    }

    fn schema(&self) -> serde_json::Value {
        out_schema("an MLT document", "mlt")
    }

    fn is_query(&self) -> bool {
        true
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["out"])?;
        let out = args::str_field(&args, "out")?;
        let sequence = cx.sequence(project)?;
        let export = mlt::export(project, &sequence, cx.paths, cx.assets, false)?;
        deliver(cx, out, &export.text, export.warnings)
    }
}

struct ExportFcpxml;

impl Op for ExportFcpxml {
    fn id(&self) -> &'static str {
        "export.fcpxml"
    }

    fn about(&self) -> &'static str {
        "write the sequence as FCPXML 1.11, for Final Cut Pro and Resolve"
    }

    fn schema(&self) -> serde_json::Value {
        out_schema("an FCPXML document", "fcpxml")
    }

    fn is_query(&self) -> bool {
        true
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["out"])?;
        let out = args::str_field(&args, "out")?;
        let sequence = cx.sequence(project)?;
        let export = fcpxml::export(project, &sequence, cx.paths, cx.assets)?;
        deliver(cx, out, &export.text, export.warnings)
    }
}

struct ExportOtio;

impl Op for ExportOtio {
    fn id(&self) -> &'static str {
        "export.otio"
    }

    fn about(&self) -> &'static str {
        "write the sequence as OpenTimelineIO JSON"
    }

    fn schema(&self) -> serde_json::Value {
        out_schema("an OTIO timeline", "otio")
    }

    fn is_query(&self) -> bool {
        true
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["out"])?;
        let out = args::str_field(&args, "out")?;
        let sequence = cx.sequence(project)?;
        let export = otio::export(project, &sequence, cx.paths, cx.assets)?;
        deliver(cx, out, &export.text, export.warnings)
    }
}

struct ExportEdl;

impl Op for ExportEdl {
    fn id(&self) -> &'static str {
        "export.edl"
    }

    fn about(&self) -> &'static str {
        "write the sequence as a CMX3600 EDL"
    }

    fn schema(&self) -> serde_json::Value {
        out_schema("an EDL", "edl")
    }

    fn is_query(&self) -> bool {
        true
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["out"])?;
        let out = args::str_field(&args, "out")?;
        let sequence = cx.sequence(project)?;
        let export = edl::export(project, &sequence)?;
        deliver(cx, out, &export.text, export.warnings)
    }
}

struct ExportSrt;

impl Op for ExportSrt {
    fn id(&self) -> &'static str {
        "export.srt"
    }

    fn about(&self) -> &'static str {
        "write the sequence's caption cues as SubRip or WebVTT"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["out"],
            "properties": {
                "out": {
                    "type": "string",
                    "description": "subtitle file to write; relative paths resolve against the project root"
                },
                "format": {
                    "type": "string",
                    "enum": ["srt", "vtt"],
                    "description": "defaults to the extension of 'out'"
                },
                "track": {
                    "type": "string",
                    "description": "caption track to export; default is every caption track, merged"
                }
            }
        })
    }

    fn is_query(&self) -> bool {
        true
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["out", "format", "track"])?;
        let out = args::str_field(&args, "out")?;
        let wanted = args::opt_str(&args, "track");
        let sequence = cx.sequence(project)?;
        let sequence = project.sequence(&sequence)?;

        let caption_tracks: Vec<&Track> = sequence
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Caption)
            .filter(|track| match wanted {
                Some(name) => track.name.eq_ignore_ascii_case(name) || track.id.as_str() == name,
                None => true,
            })
            .collect();
        if caption_tracks.is_empty() {
            return Err(Error::no_match(
                "caption track",
                wanted.unwrap_or("any"),
                sequence
                    .tracks
                    .iter()
                    .filter(|track| track.kind == TrackKind::Caption)
                    .map(|track| track.name.clone())
                    .collect(),
            ));
        }
        let cues: Vec<CaptionCue> = caption_tracks
            .iter()
            .flat_map(|track| track.cues.iter().cloned())
            .collect();
        if cues.is_empty() {
            return Err(Error::op(
                "the caption track has no cues; generate them with caption.generate first",
            ));
        }

        let format = match args::opt_str(&args, "format") {
            Some(format) => format.to_ascii_lowercase(),
            None => match Path::new(out).extension().and_then(|e| e.to_str()) {
                Some(extension) if extension.eq_ignore_ascii_case("vtt") => "vtt".to_string(),
                _ => "srt".to_string(),
            },
        };
        let text = match format.as_str() {
            "srt" => subtitle::to_srt(&cues),
            "vtt" => subtitle::to_vtt(&cues),
            other => {
                return Err(Error::bad_args(format!(
                    "unknown subtitle format '{other}'; use 'srt' or 'vtt'"
                )))
            }
        };

        let mut warnings = Vec::new();
        let overlapping = cues
            .windows(2)
            .filter(|pair| pair[0].span.end > pair[1].span.start)
            .count();
        if overlapping > 0 {
            warnings.push(Warning {
                code: "caption-overlap",
                target: sequence.name.clone(),
                detail: format!(
                    "{overlapping} cue(s) overlap the one before them; players show whichever \
                     they reach first"
                ),
            });
        }
        deliver(cx, out, &text, warnings)
    }
}

struct ImportMlt;

impl Op for ImportMlt {
    fn id(&self) -> &'static str {
        "import.mlt"
    }

    fn about(&self) -> &'static str {
        "append the tracks of an MLT or .kdenlive document to the active sequence"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "MLT or .kdenlive file to read; relative paths resolve against the project root"
                }
            }
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["path"])?;
        let path = destination(cx, args::str_field(&args, "path")?);
        let bytes = cx.assets.vfs().read(&path)?;
        let xml = String::from_utf8(bytes)
            .map_err(|e| Error::op(format!("{} is not UTF-8 text: {e}", path.display())))?;
        let imported = mlt::from_mlt(&xml)?;

        let sequence_id = cx.sequence(project)?;
        let fps = project.sequence(&sequence_id)?.fps;
        let known = catalog(project, cx);

        let mut effect = OpEffect::new();
        let mut dropped: Vec<String> = Vec::new();
        let mut snapped = 0usize;
        let mut added_clips = 0usize;
        let mut tracks: Vec<Track> = Vec::new();

        for imported_track in &imported.tracks {
            let kind = if imported_track.audio {
                TrackKind::Audio
            } else {
                TrackKind::Video
            };
            let mut track = Track::new(String::new(), kind);
            let mut cursor = Time::ZERO;
            for clip in &imported_track.clips {
                let Some(asset) = lookup(&known, &clip.resource) else {
                    dropped.push(clip.resource.clone());
                    continue;
                };
                let start = clip.start.snap(fps);
                let duration = clip.duration.snap(fps).max(fps.frame_duration());
                let source_in = clip.source_in.snap(fps);
                if start != clip.start || duration != clip.duration {
                    snapped += 1;
                }
                if start < cursor {
                    // Two MLT playlists on one Kdenlive track overlap during a
                    // transition; this document model has no overlap, so the second copy
                    // of those frames is reported rather than silently colliding.
                    dropped.push(format!(
                        "{} at {} (overlaps the clip before it)",
                        clip.resource, clip.start
                    ));
                    continue;
                }
                let mut new_clip = Clip::new(
                    Source::Asset {
                        asset,
                        stream: None,
                    },
                    start,
                    duration,
                );
                new_clip.source_in = source_in;
                new_clip.name = clip.name.clone();
                if clip.speed != 1.0 && clip.speed.is_finite() && clip.speed != 0.0 {
                    new_clip.reverse = clip.speed < 0.0;
                    new_clip.speed = dvs_core::time::Rat::parse(&format!("{}", clip.speed.abs()))?;
                }
                cursor = new_clip.end();
                track.clips.push(new_clip);
                added_clips += 1;
            }
            if track.clips.is_empty() {
                continue;
            }
            tracks.push(track);
        }

        if imported.fps != fps {
            effect = effect.warn(
                "fps-mismatch",
                &sequence_id,
                format!(
                    "the document is {} and this sequence is {fps}; every imported position was \
                     snapped to this sequence's frame grid",
                    imported.fps
                ),
            );
        }
        if snapped > 0 {
            effect = effect.warn(
                "frame-snap",
                &sequence_id,
                format!("{snapped} imported clip(s) moved to the nearest frame of this sequence"),
            );
        }
        for resource in &dropped {
            effect = effect.warn(
                "clip-dropped",
                resource.clone(),
                format!(
                    "'{resource}' is not media this project has imported; import it and run \
                     import.mlt again to pick it up"
                ),
            );
        }

        let added_tracks = tracks.len();
        if !cx.dry_run {
            let sequence = project.sequence_mut(&sequence_id)?;
            for mut track in tracks {
                track.name = sequence.next_track_name(track.kind);
                effect = effect.created(&track.id);
                sequence.tracks.push(track);
            }
        }

        Ok(effect.data(serde_json::json!({
            "tracks": added_tracks,
            "clips": added_clips,
            "dropped": dropped,
            "fps": imported.fps.to_string(),
            "size": imported.size,
        })))
    }
}

/// Every asset this project owns, by the two things an MLT `resource` can match: the
/// absolute path of the blob in the store, and the original file name.
fn catalog(project: &Project, cx: &OpCx) -> Vec<(Option<PathBuf>, String, Option<String>, AssetId)> {
    project
        .assets
        .iter()
        .map(|(id, asset)| {
            (
                cx.assets.find(&asset.hash).ok(),
                asset.name.clone(),
                asset.source_path.clone(),
                id.clone(),
            )
        })
        .collect()
}

fn lookup(
    known: &[(Option<PathBuf>, String, Option<String>, AssetId)],
    resource: &str,
) -> Option<AssetId> {
    let target = Path::new(resource);
    let file_name = target.file_name();
    known
        .iter()
        .find(|(path, name, source, _)| {
            path.as_deref() == Some(target)
                || source.as_deref() == Some(resource)
                || file_name.is_some_and(|file| file == name.as_str())
        })
        .map(|(_, _, _, id)| id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{fixture, Fixture};
    use dvs_core::time::Fps;

    fn run(fixture: &mut Fixture, op: &dyn Op, args: serde_json::Value) -> Result<OpEffect> {
        let paths = fixture.paths.clone();
        let assets = fixture.assets.clone();
        let mut cx = OpCx::new(&paths, &assets);
        op.apply(&mut fixture.project, args, &mut cx)
    }

    #[test]
    fn every_op_id_is_in_this_crates_namespace() {
        let mut registry = Registry::new();
        register(&mut registry);
        let mut ids = registry.ids();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![
                "export.edl",
                "export.fcpxml",
                "export.kdenlive",
                "export.mlt",
                "export.otio",
                "export.srt",
                "import.mlt",
            ]
        );
        for op in registry.iter() {
            assert_eq!(
                op.is_query(),
                op.id().starts_with("export."),
                "{} is on the wrong side of the query line",
                op.id()
            );
        }
    }

    #[test]
    fn exports_write_the_file_and_report_where() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        let effect = run(
            &mut fixture,
            &ExportKdenlive,
            serde_json::json!({ "out": "promo.kdenlive" }),
        )
        .expect("export");
        let data = effect.data.expect("data");
        assert_eq!(data["path"], "promo.kdenlive");
        let written = std::fs::read_to_string(fixture.paths.root().join("promo.kdenlive"))
            .expect("the file is on disk");
        assert!(written.contains("kdenlive:docproperties.version"));
        assert_eq!(data["bytes"].as_u64(), Some(written.len() as u64));
    }

    #[test]
    fn a_dry_run_reports_the_losses_without_writing() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_title("V1", "0", "1");
        let paths = fixture.paths.clone();
        let assets = fixture.assets.clone();
        let mut cx = OpCx::new(&paths, &assets).dry_run(true);
        let effect = ExportMlt
            .apply(
                &mut fixture.project,
                serde_json::json!({ "out": "out.mlt" }),
                &mut cx,
            )
            .expect("export");
        assert!(!paths.root().join("out.mlt").exists(), "dry run wrote a file");
        assert!(effect
            .warnings
            .iter()
            .any(|warning| warning.code == "title-not-representable"));
    }

    #[test]
    fn unknown_arguments_are_refused() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        let error = run(
            &mut fixture,
            &ExportEdl,
            serde_json::json!({ "out": "a.edl", "seq": "main" }),
        )
        .expect_err("'seq' is not an argument, it is context");
        assert!(error.to_string().contains("seq"), "{error}");
    }

    #[test]
    fn srt_export_picks_its_format_from_the_extension() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_cue("0", "1", "hello");
        run(
            &mut fixture,
            &ExportSrt,
            serde_json::json!({ "out": "captions.vtt" }),
        )
        .expect("export");
        let text = std::fs::read_to_string(fixture.paths.root().join("captions.vtt"))
            .expect("the file is on disk");
        assert!(text.starts_with("WEBVTT"), "{text}");
        assert!(text.contains("00:00:00.000 --> 00:00:01.000"), "{text}");
    }

    #[test]
    fn srt_export_without_captions_says_so_instead_of_writing_nothing() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        let error = run(
            &mut fixture,
            &ExportSrt,
            serde_json::json!({ "out": "captions.srt" }),
        )
        .expect_err("there are no captions");
        assert!(error.to_string().contains("caption"), "{error}");
        assert!(!fixture.paths.root().join("captions.srt").exists());
    }

    #[test]
    fn import_round_trips_an_exported_timeline_back_into_the_project() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        fixture.push_second("V1", "1.5", "1", "0");
        run(
            &mut fixture,
            &ExportMlt,
            serde_json::json!({ "out": "round.mlt" }),
        )
        .expect("export");

        let before = fixture.sequence_ref().tracks.len();
        let effect = run(
            &mut fixture,
            &ImportMlt,
            serde_json::json!({ "path": "round.mlt" }),
        )
        .expect("import");
        let data = effect.data.clone().expect("data");
        assert_eq!(data["tracks"], 1, "only the video track carries clips");
        assert_eq!(data["clips"], 2);
        assert!(effect.warnings.is_empty(), "{:?}", effect.warnings);

        let sequence = fixture.sequence_ref();
        assert_eq!(sequence.tracks.len(), before + 1);
        let imported = sequence.tracks.last().expect("the new track");
        assert_eq!(imported.name, "V2");
        assert_eq!(imported.clips.len(), 2);
        assert_eq!(imported.clips[0].duration, Time::from_secs(1));
        assert_eq!(imported.clips[1].start, Time::parse("1.5").unwrap());
        assert_eq!(imported.clips[1].source.asset_id(), Some(&fixture.second_asset));
    }

    #[test]
    fn import_reports_media_the_project_does_not_have() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        let xml = r#"<mlt producer="tractor0">
  <profile frame_rate_num="30" frame_rate_den="1" width="320" height="240"/>
  <producer id="p0">
    <property name="resource">/nowhere/stranger.mp4</property>
    <property name="mlt_service">avformat-novalidate</property>
  </producer>
  <playlist id="pl0"><entry producer="p0" in="0" out="29"/></playlist>
  <tractor id="tractor0"><track producer="pl0"/></tractor>
</mlt>"#;
        std::fs::write(fixture.paths.root().join("foreign.mlt"), xml).expect("write");
        let effect = run(
            &mut fixture,
            &ImportMlt,
            serde_json::json!({ "path": "foreign.mlt" }),
        )
        .expect("import");
        assert_eq!(effect.data.clone().expect("data")["clips"], 0);
        assert!(effect
            .warnings
            .iter()
            .any(|warning| warning.code == "clip-dropped"));
        assert_eq!(
            fixture.sequence_ref().tracks.len(),
            2,
            "a track with no usable clips must not be added"
        );
    }

    #[test]
    fn importing_a_kdenlive_transition_never_produces_overlapping_clips() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        fixture.push_second("V1", "1", "1", "0");
        fixture.set_transition(
            "V1",
            1,
            dvs_core::project::TransitionKind::Dissolve,
            "0.5",
        );
        run(
            &mut fixture,
            &ExportKdenlive,
            serde_json::json!({ "out": "with-transition.kdenlive" }),
        )
        .expect("export");

        let effect = run(
            &mut fixture,
            &ImportMlt,
            serde_json::json!({ "path": "with-transition.kdenlive" }),
        )
        .expect("import");

        // The dissolve is an overlap in MLT and cannot be one here, so the duplicated
        // frames come back as a dropped clip rather than an invalid document.
        assert!(
            effect
                .warnings
                .iter()
                .any(|warning| warning.detail.contains("overlaps")),
            "{:?}",
            effect.warnings
        );
        fixture
            .sequence_ref()
            .validate()
            .expect("the imported timeline must satisfy the document invariants");
        let imported = fixture
            .sequence_ref()
            .tracks
            .last()
            .expect("the imported track");
        assert_eq!(imported.clips.len(), 2);
        assert_eq!(imported.clips[0].duration, Time::parse("1.5").unwrap());
    }
}
