//! The one op this crate owns: `fx.analyze`.
//!
//! Stabilization is the only effect in the catalog that cannot be computed from the
//! document and the current frame alone — camera motion has to be measured across the
//! whole shot first. That measurement is ffmpeg's `vidstabdetect`, it belongs to whichever
//! crate already speaks ffmpeg and owns the warp, and `dvs-core` must not depend on
//! `dvs-comp`, so the `fx.*` namespace is shared: `dvs-core` registers
//! `add`/`remove`/`set`/`reorder`/`enable` and this module registers `analyze`. The
//! registry panics on a duplicate id, so the split cannot drift into an ambiguity.
//!
//! It is a query op: it writes `cache/stabilize/<clipId>.trf`, which is regenerable, and
//! leaves the document untouched — so it is not journaled and undo has nothing to undo.
//! A matched clip without a stabilize effect is a warning rather than an error, which is
//! what makes `fx.analyze --target '*'` usable as a pre-render pass over a whole sequence.

use crate::effects::{analyze_stabilization, parse_trf, stabilize_cache_path, EffectCx};
use dvs_core::error::{Error, Result};
use dvs_core::op::{args, Op, OpCx, OpEffect, Registry};
use dvs_core::project::Project;
use dvs_core::selector;
use dvs_media::toolchain::Toolchain;

/// Add this crate's ops to a registry.
pub fn register(registry: &mut Registry) {
    registry.register(Analyze);
}

/// libvidstab's default search aggressiveness. 5 handles handheld footage; 10 is for a
/// shot that moves so much the search window has to be opened, at several times the cost.
const DEFAULT_SHAKINESS: u32 = 5;

struct Analyze;

impl Op for Analyze {
    fn id(&self) -> &'static str {
        "fx.analyze"
    }

    fn about(&self) -> &'static str {
        "measure camera motion for clips with a stabilize effect, into the render cache"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["target"],
            "properties": {
                "target": {
                    "type": "string",
                    "description": "selector for the clips to analyse, e.g. '#interview' or 'clip[track=V1]'"
                },
                "shakiness": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 10,
                    "default": DEFAULT_SHAKINESS,
                    "description": "libvidstab search aggressiveness; raise it when the shot moves a lot"
                }
            }
        })
    }

    fn is_query(&self) -> bool {
        true
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        &["ffmpeg"]
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["target", "shakiness"])?;
        let target = args::str_field(&args, "target")?;
        let shakiness = match args::opt_f64(&args, "shakiness")? {
            None => DEFAULT_SHAKINESS,
            Some(value) if value.fract() == 0.0 && (1.0..=10.0).contains(&value) => value as u32,
            Some(value) => {
                return Err(Error::bad_args(format!(
                    "'shakiness' is libvidstab's 1..=10 search aggressiveness, got {value}"
                )))
            }
        };

        // Nothing here mutates the document; reborrowing immutably lets the clips, the
        // sequence and the effect context coexist.
        let document: &Project = project;
        let sequence_id = cx.sequence(document)?;
        let clips = selector::resolve_clips(document, &sequence_id, target)?;
        let sequence = document.sequence(&sequence_id)?;
        let tool = Toolchain::shared()?;

        let mut effect = OpEffect::new();
        let mut analysed = Vec::new();
        for (_, clip_id) in clips {
            let Some((_, clip)) = sequence.find_clip(&clip_id) else {
                continue;
            };
            if !clip
                .effects
                .iter()
                .any(|fx| fx.kind == "stabilize" && fx.enabled)
            {
                effect = effect.warn(
                    "no-stabilize-effect",
                    &clip_id,
                    format!(
                        "clip '{}' has no enabled stabilize effect; nothing to analyse",
                        clip.label()
                    ),
                );
                continue;
            }
            let out = stabilize_cache_path(cx.paths, &clip.id);
            let stored = cx.paths.relativize(&out);
            if cx.dry_run {
                analysed.push(serde_json::json!({
                    "clip": clip.id,
                    "trf": stored,
                    "dryRun": true
                }));
                continue;
            }
            let asset_id = clip.source.asset_id().ok_or_else(|| {
                Error::op(format!(
                    "clip '{}' is {}, which has no media to measure; stabilize applies to \
                     footage",
                    clip.label(),
                    clip.source.describe()
                ))
            })?;
            let record = document.asset(asset_id)?;
            // The master blob, not the proxy: the transforms are in its pixel coordinates
            // and a render reads them back scaled by whatever it decoded at.
            let media = cx.assets.find(&record.hash)?;
            let effect_cx = EffectCx {
                project: document,
                tool,
                paths: cx.paths,
                assets: cx.assets,
                clip,
                at: clip.start,
                sequence_size: sequence.size,
            };
            analyze_stabilization(&effect_cx, &media, &out, shakiness)?;
            let frames = parse_trf(
                &std::fs::read_to_string(&out).map_err(|error| Error::io(&out, error))?,
            )?
            .len();
            analysed.push(serde_json::json!({
                "clip": clip.id,
                "source": record.name,
                "trf": stored,
                "frames": frames
            }));
        }
        if analysed.is_empty() && effect.warnings.is_empty() {
            return Err(Error::op(format!(
                "selector '{target}' matched no clip that could be analysed"
            )));
        }
        Ok(effect.data(serde_json::json!({ "analysed": analysed })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::asset::AssetStore;
    use dvs_core::ids::AssetId;
    use dvs_core::paths::ProjectPaths;
    use dvs_core::project::{Asset, Clip, Effect, Source, Track, TrackKind};
    use dvs_core::time::{Fps, Time};
    use dvs_core::vfs::FsVfs;
    use std::sync::Arc;

    /// A project on disk whose V1 clip uses a real, deliberately shaky one-second file.
    struct Fixture {
        project: Project,
        paths: ProjectPaths,
        assets: AssetStore,
        /// Held only so the project directory outlives the test.
        _dir: tempfile::TempDir,
    }

    fn shaky_project(with_effect: bool) -> Fixture {
        let tool = Toolchain::shared().expect("ffmpeg is installed for the test suite");
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = ProjectPaths::new(dir.path());
        let assets = AssetStore::new(paths.assets_dir(), Arc::new(FsVfs));

        let media = dir.path().join("shaky.mp4");
        let status = tool
            .ffmpeg_command()
            .args(["-f", "lavfi", "-i", "testsrc=size=360x280:rate=10:duration=1"])
            .args(["-vf", "crop=300:240:x=30+6*sin(2*PI*n/5):y=20"])
            .args(["-c:v", "libx264", "-qp", "0", "-pix_fmt", "yuv420p"])
            .arg(&media)
            .status()
            .expect("ffmpeg runs");
        assert!(status.success(), "could not synthesize the shaky clip");
        let hash = assets.import_path(&media).expect("import media");

        let fps = Fps::new(10, 1).expect("10 fps");
        let mut project = Project::new("stabilize", fps, [300, 240], 48000);
        let asset: Asset = serde_json::from_value(serde_json::json!({
            "id": "ast_shaky",
            "name": "shaky.mp4",
            "hash": hash,
            "kind": "video",
            "probe": {
                "duration": "1/1",
                "video": {
                    "streamIndex": 0,
                    "size": [300, 240],
                    "fps": "10",
                    "codec": "h264",
                    "pixFmt": "yuv420p"
                }
            },
            "imported": "2026-01-01T00:00:00Z"
        }))
        .expect("asset fixture deserializes");
        project.assets.insert(asset.id.clone(), asset);

        let mut clip = Clip::new(
            Source::Asset {
                asset: AssetId::from_raw("ast_shaky"),
                stream: None,
            },
            Time::ZERO,
            Time::from_secs(1),
        );
        clip.name = Some("shaky".to_string());
        if with_effect {
            clip.effects.push(Effect::new("stabilize"));
        }
        let mut track = Track::new("V1", TrackKind::Video);
        track.clips.push(clip);
        let sequence_id = project.active_sequence.clone();
        project
            .sequence_mut(&sequence_id)
            .expect("active sequence")
            .tracks
            .push(track);

        Fixture {
            project,
            paths,
            assets,
            _dir: dir,
        }
    }

    /// The one clip in the fixture sequence.
    fn only_clip(fixture: &Fixture) -> dvs_core::ids::ClipId {
        let sequence = fixture
            .project
            .sequence(&fixture.project.active_sequence)
            .expect("active sequence");
        sequence.tracks[0].clips[0].id.clone()
    }

    #[test]
    fn analyze_writes_transforms_for_a_stabilized_clip() {
        let mut fixture = shaky_project(true);
        let mut cx = OpCx::new(&fixture.paths, &fixture.assets);
        let effect = Analyze
            .apply(
                &mut fixture.project,
                serde_json::json!({ "target": "#shaky", "shakiness": 8 }),
                &mut cx,
            )
            .expect("analysis runs");

        let data = effect.data.expect("the op reports what it analysed");
        let analysed = data["analysed"].as_array().expect("an array of clips");
        assert_eq!(analysed.len(), 1);
        let frames = analysed[0]["frames"].as_u64().expect("a frame count");
        assert!(frames >= 9, "only {frames} frames measured of a 10-frame clip");

        let stored = analysed[0]["trf"].as_str().expect("a cache path");
        assert!(
            stored.starts_with("cache/stabilize/"),
            "transforms went to {stored}"
        );
        let text = std::fs::read_to_string(fixture.paths.resolve(stored)).expect("read trf");
        let parsed = parse_trf(&text).expect("the file the op wrote parses");
        assert!(
            parsed.iter().any(|frame| !frame.motions.is_empty()),
            "a deliberately shaky clip produced no motion vectors"
        );
    }

    #[test]
    fn a_clip_without_the_effect_is_a_warning_not_a_failure() {
        let mut fixture = shaky_project(false);
        let mut cx = OpCx::new(&fixture.paths, &fixture.assets);
        let effect = Analyze
            .apply(
                &mut fixture.project,
                serde_json::json!({ "target": "#shaky" }),
                &mut cx,
            )
            .expect("a batch pre-pass must not fail on unstabilized clips");
        assert_eq!(effect.warnings.len(), 1);
        assert_eq!(effect.warnings[0].code, "no-stabilize-effect");
        assert!(
            !stabilize_cache_path(&fixture.paths, &only_clip(&fixture)).exists(),
            "nothing to stabilize, so nothing should have been measured"
        );
    }

    #[test]
    fn a_dry_run_reports_the_path_without_spawning_ffmpeg() {
        let mut fixture = shaky_project(true);
        let mut cx = OpCx::new(&fixture.paths, &fixture.assets).dry_run(true);
        let effect = Analyze
            .apply(
                &mut fixture.project,
                serde_json::json!({ "target": "#shaky" }),
                &mut cx,
            )
            .expect("dry run succeeds");
        let data = effect.data.expect("dry runs still report");
        let stored = data["analysed"][0]["trf"].as_str().expect("a cache path");
        assert!(
            !fixture.paths.resolve(stored).exists(),
            "a dry run must not write {stored}"
        );
    }

    #[test]
    fn shakiness_outside_the_supported_range_is_rejected() {
        let mut fixture = shaky_project(true);
        let mut cx = OpCx::new(&fixture.paths, &fixture.assets);
        let error = Analyze
            .apply(
                &mut fixture.project,
                serde_json::json!({ "target": "#shaky", "shakiness": 42 }),
                &mut cx,
            )
            .expect_err("42 is not a libvidstab shakiness");
        assert_eq!(error.exit_code(), dvs_core::error::exit::BAD_ARGS);
        assert!(error.to_string().contains("1..=10"), "{error}");
    }
}
