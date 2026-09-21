//! The ops that bring media into a project.
//!
//! Import is the only place in the system that touches a file the user picked, and it is
//! where three decisions get made once instead of being re-litigated at render time: the
//! bytes are hashed and copied into the content-addressed store (so a moved or edited
//! original cannot silently change a finished edit), the file is probed (so color and rate
//! are known facts rather than render-time guesses), and a constant-rate proxy is built for
//! anything that needs one.

use crate::probe::probe;
use crate::proxy::{self, ProxySpec};
use crate::toolchain::Toolchain;
use dvs_core::error::{Error, Result};
use dvs_core::ids::AssetId;
use dvs_core::op::{args, Op, OpCx, OpEffect, Registry};
use dvs_core::project::{Asset, Project, Source};
use dvs_core::time::Time;
use std::path::{Path, PathBuf};

pub fn register(registry: &mut Registry) {
    registry.register(Import);
    registry.register(Remove);
    registry.register(Relink);
    registry.register(Proxy);
    registry.register(Thumbnails);
    registry.register(Waveform);
}

/// Clips that reference an asset, as labels for an error message.
fn users(project: &Project, asset: &AssetId) -> Vec<String> {
    let mut used = Vec::new();
    for sequence in project.sequences.values() {
        for track in &sequence.tracks {
            for clip in &track.clips {
                if clip.source.asset_id() == Some(asset) {
                    used.push(format!("{}/{}/{}", sequence.name, track.name, clip.label()));
                }
            }
        }
    }
    used
}

fn asset_path(cx: &OpCx, asset: &Asset) -> Result<PathBuf> {
    cx.assets.find(&asset.hash)
}

struct Import;

impl Op for Import {
    fn id(&self) -> &'static str {
        "asset.import"
    }

    fn about(&self) -> &'static str {
        "probe, hash and copy media into the project, building a proxy when one is needed"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {
                    "description": "file to import, or a list of files",
                    "oneOf": [
                        { "type": "string" },
                        { "type": "array", "items": { "type": "string" } }
                    ]
                },
                "name": { "type": "string", "description": "display name; defaults to the file name" },
                "proxy": {
                    "description": "'auto' (default) builds one for VFR or >1080p sources",
                    "oneOf": [ { "type": "boolean" }, { "const": "auto" } ]
                }
            }
        })
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        &["ffmpeg", "ffprobe"]
    }

    fn apply(&self, project: &mut Project, args: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&args, &["path", "name", "proxy"])?;
        let paths = args::string_list(&args, "path")?;
        let paths = if paths.is_empty() {
            vec![args::str_field(&args, "path")?.to_string()]
        } else {
            paths
        };
        let name_override = args::opt_str(&args, "name").map(str::to_string);
        if name_override.is_some() && paths.len() > 1 {
            return Err(Error::bad_args(
                "'name' applies to a single import; drop it when importing several files",
            ));
        }
        let proxy_mode = match args.get("proxy") {
            None | Some(serde_json::Value::Null) => ProxyMode::Auto,
            Some(serde_json::Value::String(text)) if text == "auto" => ProxyMode::Auto,
            _ => match args::opt_bool(&args, "proxy")? {
                Some(true) => ProxyMode::Always,
                Some(false) => ProxyMode::Never,
                None => ProxyMode::Auto,
            },
        };

        let tool = Toolchain::shared()?;
        let mut effect = OpEffect::new();
        for raw in paths {
            let path = Path::new(&raw);
            if !path.is_file() {
                return Err(Error::bad_args(format!("'{raw}' is not a file")));
            }
            let probed = probe(tool, path)?;
            if probed.probe.duration.is_zero() && probed.probe.video.is_none() {
                return Err(Error::op(format!(
                    "'{raw}' has no audio or video streams ffmpeg can read"
                )));
            }
            if cx.dry_run {
                effect = effect.created(format!("(dry-run) {raw}"));
                continue;
            }
            let hash = cx.assets.import_path(path)?;
            let id = AssetId::new();
            let name = name_override.clone().unwrap_or_else(|| {
                path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| raw.clone())
            });
            let mut asset = Asset {
                id: id.clone(),
                name,
                hash: hash.clone(),
                kind: probed.kind,
                probe: probed.probe.clone(),
                proxy: None,
                source_path: Some(path.display().to_string()),
                imported: chrono::Utc::now(),
                provenance: None,
            };

            let wants = match proxy_mode {
                ProxyMode::Always => true,
                ProxyMode::Never => false,
                ProxyMode::Auto => proxy::wants_proxy(&probed.probe),
            };
            if wants && probed.probe.video.is_some() {
                let sequence = project.sequence(&project.active_sequence)?;
                let spec = ProxySpec::for_probe(&probed.probe, sequence.fps);
                let output = cx.paths.proxy_dir().join(format!("{id}.mp4"));
                proxy::make_proxy(tool, &cx.assets.find(&hash)?, &output, &spec)?;
                asset.proxy = Some(cx.paths.relativize(&output));
                if probed.probe.vfr {
                    effect = effect.warn(
                        "vfr-source",
                        &id,
                        format!(
                            "'{}' has variable frame timing; a constant-rate proxy at {} was built and \
                             frame-exact edits use it",
                            asset.name, spec.fps
                        ),
                    );
                }
            } else if probed.probe.vfr {
                effect = effect.warn(
                    "vfr-source",
                    &id,
                    "variable frame timing and no proxy: frame positions in this source are approximate",
                );
            }

            effect = effect.created(&id);
            project.assets.insert(id, asset);
        }
        Ok(effect)
    }
}

enum ProxyMode {
    Auto,
    Always,
    Never,
}

struct Remove;

impl Op for Remove {
    fn id(&self) -> &'static str {
        "asset.remove"
    }

    fn about(&self) -> &'static str {
        "drop an asset from the project; refuses while clips still use it"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["asset"],
            "properties": {
                "asset": { "type": "string", "description": "asset id, name or file stem" },
                "force": { "type": "boolean", "description": "also remove every clip using it" }
            }
        })
    }

    fn apply(&self, project: &mut Project, args: serde_json::Value, _cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&args, &["asset", "force"])?;
        let id = project.resolve_asset(args::str_field(&args, "asset")?)?;
        let force = args::opt_bool(&args, "force")?.unwrap_or(false);
        let used = users(project, &id);
        if !used.is_empty() && !force {
            return Err(Error::op(format!(
                "asset '{id}' is used by {} clip(s): {}; pass force to remove them too",
                used.len(),
                used.join(", ")
            )));
        }
        let mut effect = OpEffect::new();
        for sequence in project.sequences.values_mut() {
            for track in &mut sequence.tracks {
                let before = track.clips.len();
                track.clips.retain(|clip| clip.source.asset_id() != Some(&id));
                if track.clips.len() != before {
                    effect = effect.changed(&track.id);
                }
            }
        }
        project.assets.shift_remove(&id);
        Ok(effect.removed(&id))
    }
}

struct Relink;

impl Op for Relink {
    fn id(&self) -> &'static str {
        "asset.relink"
    }

    fn about(&self) -> &'static str {
        "point an asset at a new file, re-probing and re-hashing it"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["asset", "path"],
            "properties": {
                "asset": { "type": "string" },
                "path": { "type": "string" }
            }
        })
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        &["ffprobe"]
    }

    fn apply(&self, project: &mut Project, args: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&args, &["asset", "path"])?;
        let id = project.resolve_asset(args::str_field(&args, "asset")?)?;
        let raw = args::str_field(&args, "path")?;
        let path = Path::new(raw);
        if !path.is_file() {
            return Err(Error::bad_args(format!("'{raw}' is not a file")));
        }
        let tool = Toolchain::shared()?;
        let probed = probe(tool, path)?;
        let hash = cx.assets.import_path(path)?;
        let asset = project
            .assets
            .get_mut(&id)
            .ok_or_else(|| Error::no_match("asset", id.as_str(), Vec::new()))?;
        let mut effect = OpEffect::new().changed(&id);
        // A relink that changes the duration silently leaves clips pointing past the end
        // of the new file. Report it; `dvs lint` will name the offending clips.
        if probed.probe.duration < asset.probe.duration {
            effect = effect.warn(
                "past-source-end",
                &id,
                format!(
                    "new file is shorter ({} vs {}); clips may now run past its end",
                    probed.probe.duration.clock(),
                    asset.probe.duration.clock()
                ),
            );
        }
        asset.hash = hash;
        asset.probe = probed.probe;
        asset.kind = probed.kind;
        asset.proxy = None;
        asset.source_path = Some(path.display().to_string());
        Ok(effect)
    }
}

struct Proxy;

impl Op for Proxy {
    fn id(&self) -> &'static str {
        "asset.proxy"
    }

    fn about(&self) -> &'static str {
        "build or rebuild a constant-rate, low-resolution stand-in for an asset"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["asset"],
            "properties": {
                "asset": { "type": "string" },
                "height": { "type": "integer", "minimum": 64, "default": 540 },
                "fps": { "type": "string", "description": "output rate; defaults to the source rate" }
            }
        })
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        &["ffmpeg"]
    }

    fn apply(&self, project: &mut Project, args: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&args, &["asset", "height", "fps"])?;
        let id = project.resolve_asset(args::str_field(&args, "asset")?)?;
        let sequence_fps = project.sequence(&project.active_sequence)?.fps;
        let asset = project.asset(&id)?.clone();
        if asset.probe.video.is_none() {
            return Err(Error::op(format!(
                "asset '{}' has no video stream to proxy",
                asset.name
            )));
        }
        let mut spec = ProxySpec::for_probe(&asset.probe, sequence_fps);
        if let Some(height) = args::opt_f64(&args, "height")? {
            spec.height = height.max(64.0) as u32;
        }
        if let Some(text) = args::opt_str(&args, "fps") {
            spec.fps = dvs_core::time::Fps::parse(text)?;
        }
        if cx.dry_run {
            return Ok(OpEffect::new().changed(&id));
        }
        let tool = Toolchain::shared()?;
        let output = cx.paths.proxy_dir().join(format!("{id}.mp4"));
        proxy::make_proxy(tool, &asset_path(cx, &asset)?, &output, &spec)?;
        let stored = cx.paths.relativize(&output);
        project
            .assets
            .get_mut(&id)
            .expect("asset resolved above")
            .proxy = Some(stored.clone());
        Ok(OpEffect::new()
            .changed(&id)
            .data(serde_json::json!({ "proxy": stored, "height": spec.height, "fps": spec.fps.to_string() })))
    }
}

struct Thumbnails;

impl Op for Thumbnails {
    fn id(&self) -> &'static str {
        "asset.thumbnails"
    }

    fn about(&self) -> &'static str {
        "extract evenly spaced thumbnails for an asset into the cache"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["asset"],
            "properties": {
                "asset": { "type": "string" },
                "every": { "type": "string", "default": "5s" },
                "width": { "type": "integer", "minimum": 16, "default": 320 }
            }
        })
    }

    fn is_query(&self) -> bool {
        // Thumbnails are cache artifacts; the document does not change, so journaling the
        // call would add an undo step that undoes nothing.
        true
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        &["ffmpeg"]
    }

    fn apply(&self, project: &mut Project, args: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&args, &["asset", "every", "width"])?;
        let id = project.resolve_asset(args::str_field(&args, "asset")?)?;
        let asset = project.asset(&id)?.clone();
        let fps = project.sequence(&project.active_sequence)?.fps;
        let every = args::opt_time(&args, "every", fps)?.unwrap_or(Time::from_secs(5));
        let width = args::opt_f64(&args, "width")?.unwrap_or(320.0).max(16.0) as u32;
        let tool = Toolchain::shared()?;
        let written = proxy::thumbnails(
            tool,
            &asset_path(cx, &asset)?,
            asset.probe.duration,
            every,
            width,
            &cx.paths.thumb_dir(),
            id.as_str(),
        )?;
        let files: Vec<String> = written.iter().map(|p| cx.paths.relativize(p)).collect();
        Ok(OpEffect::new().data(serde_json::json!({ "thumbnails": files })))
    }
}

struct Waveform;

impl Op for Waveform {
    fn id(&self) -> &'static str {
        "asset.waveform"
    }

    fn about(&self) -> &'static str {
        "compute min/max/RMS peaks for an asset's audio and cache them"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["asset"],
            "properties": {
                "asset": { "type": "string" },
                "buckets-per-second": { "type": "integer", "minimum": 1, "default": 20 }
            }
        })
    }

    fn is_query(&self) -> bool {
        true
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        &["ffmpeg"]
    }

    fn apply(&self, project: &mut Project, args: serde_json::Value, cx: &mut OpCx) -> Result<OpEffect> {
        args::reject_unknown(&args, &["asset", "buckets-per-second"])?;
        let id = project.resolve_asset(args::str_field(&args, "asset")?)?;
        let asset = project.asset(&id)?.clone();
        if asset.probe.audio.is_none() {
            return Err(Error::op(format!(
                "asset '{}' has no audio stream",
                asset.name
            )));
        }
        let buckets = args::opt_f64(&args, "buckets-per-second")?.unwrap_or(20.0).max(1.0) as u32;
        let tool = Toolchain::shared()?;
        let wave = proxy::waveform(tool, &asset_path(cx, &asset)?, asset.probe.duration, buckets)?;
        let path = cx.paths.waveform_dir().join(format!("{id}.json"));
        std::fs::create_dir_all(cx.paths.waveform_dir())
            .map_err(|e| Error::io(cx.paths.waveform_dir(), e))?;
        std::fs::write(&path, serde_json::to_vec(&wave).expect("waveform serializes"))
            .map_err(|e| Error::io(&path, e))?;
        Ok(OpEffect::new().data(serde_json::json!({
            "waveform": cx.paths.relativize(&path),
            "buckets": wave.peaks.len(),
            "bucketsPerSecond": buckets
        })))
    }
}

/// Sources that still resolve. Used by `dvs doctor` and the `missing-asset` lint.
pub fn missing_assets(project: &Project, cx: &OpCx) -> Vec<(AssetId, String)> {
    project
        .assets
        .iter()
        .filter(|(_, asset)| !cx.assets.exists(&asset.hash))
        .map(|(id, asset)| (id.clone(), asset.name.clone()))
        .collect()
}

/// Whether a source needs media at all, for callers deciding if ffmpeg is required.
pub fn needs_media(source: &Source) -> bool {
    source.asset_id().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::asset::AssetStore;
    use dvs_core::paths::ProjectPaths;
    use dvs_core::time::Fps;
    use dvs_core::vfs::FsVfs;
    use std::sync::Arc;

    struct Harness {
        _dir: tempfile::TempDir,
        paths: ProjectPaths,
        assets: AssetStore,
        project: Project,
    }

    fn harness() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::new(dir.path());
        let assets = AssetStore::new(paths.assets_dir(), Arc::new(FsVfs));
        let project = Project::new("t", Fps::new(30, 1).unwrap(), [1920, 1080], 48_000);
        Harness {
            _dir: dir,
            paths,
            assets,
            project,
        }
    }

    fn source_file(dir: &Path, name: &str, duration: i64) -> PathBuf {
        let tool = Toolchain::discover().unwrap();
        let path = dir.join(name);
        crate::decode::synthesize(
            &tool,
            &path,
            "testsrc2",
            Time::from_secs(duration),
            Fps::new(30, 1).unwrap(),
            [320, 180],
        )
        .unwrap();
        path
    }

    #[test]
    fn import_probes_hashes_and_deduplicates() {
        let mut h = harness();
        let media = source_file(h.paths.root(), "a.mp4", 1);
        let copy = h.paths.root().join("a-copy.mp4");
        std::fs::copy(&media, &copy).unwrap();

        let op = Import;
        let mut cx = OpCx::new(&h.paths, &h.assets);
        let effect = op
            .apply(
                &mut h.project,
                serde_json::json!({ "path": [media.display().to_string(), copy.display().to_string()] }),
                &mut cx,
            )
            .unwrap();
        assert_eq!(effect.created.len(), 2);
        assert_eq!(h.project.assets.len(), 2);
        let hashes: Vec<&String> = h.project.assets.values().map(|a| &a.hash).collect();
        assert_eq!(hashes[0], hashes[1], "identical bytes must share one blob");
        let asset = h.project.assets.values().next().unwrap();
        assert_eq!(asset.probe.video.as_ref().unwrap().size, [320, 180]);
        assert!(asset.probe.duration.is_positive());
    }

    #[test]
    fn importing_a_directory_or_missing_file_is_a_clear_argument_error() {
        let mut h = harness();
        let op = Import;
        let mut cx = OpCx::new(&h.paths, &h.assets);
        let err = op
            .apply(
                &mut h.project,
                serde_json::json!({ "path": "/nope/missing.mp4" }),
                &mut cx,
            )
            .unwrap_err();
        assert_eq!(err.exit_code(), dvs_core::error::exit::BAD_ARGS);
    }

    #[test]
    fn removing_a_used_asset_names_the_clips_until_forced() {
        let mut h = harness();
        let media = source_file(h.paths.root(), "b.mp4", 1);
        let mut cx = OpCx::new(&h.paths, &h.assets);
        Import
            .apply(
                &mut h.project,
                serde_json::json!({ "path": media.display().to_string() }),
                &mut cx,
            )
            .unwrap();
        let asset_id = h.project.assets.keys().next().unwrap().clone();

        // Place a clip that uses it.
        let seq_id = h.project.active_sequence.clone();
        let sequence = h.project.sequence_mut(&seq_id).unwrap();
        let mut track = dvs_core::project::Track::new("V1", dvs_core::project::TrackKind::Video);
        let mut clip = dvs_core::project::Clip::new(
            Source::Asset {
                asset: asset_id.clone(),
                stream: None,
            },
            Time::ZERO,
            Time::from_secs(1),
        );
        clip.name = Some("intro".into());
        track.place(clip);
        sequence.tracks.push(track);

        let err = Remove
            .apply(
                &mut h.project,
                serde_json::json!({ "asset": asset_id.to_string() }),
                &mut cx,
            )
            .unwrap_err();
        assert!(err.to_string().contains("intro"), "{err}");
        assert_eq!(h.project.assets.len(), 1, "nothing removed on failure");

        Remove
            .apply(
                &mut h.project,
                serde_json::json!({ "asset": asset_id.to_string(), "force": true }),
                &mut cx,
            )
            .unwrap();
        assert!(h.project.assets.is_empty());
        let sequence = h.project.sequence(&seq_id).unwrap();
        assert!(sequence.tracks[0].clips.is_empty(), "forced removal drops the clips");
    }

    #[test]
    fn relinking_to_a_shorter_file_warns_instead_of_silently_truncating() {
        let mut h = harness();
        let long = source_file(h.paths.root(), "long.mp4", 3);
        let short = source_file(h.paths.root(), "short.mp4", 1);
        let mut cx = OpCx::new(&h.paths, &h.assets);
        Import
            .apply(
                &mut h.project,
                serde_json::json!({ "path": long.display().to_string() }),
                &mut cx,
            )
            .unwrap();
        let id = h.project.assets.keys().next().unwrap().clone();
        let effect = Relink
            .apply(
                &mut h.project,
                serde_json::json!({ "asset": id.to_string(), "path": short.display().to_string() }),
                &mut cx,
            )
            .unwrap();
        assert!(
            effect.warnings.iter().any(|w| w.code == "past-source-end"),
            "{:?}",
            effect.warnings
        );
        assert!(h.project.asset(&id).unwrap().probe.duration < Time::from_secs(2));
    }

    #[test]
    fn proxy_op_records_a_project_relative_path() {
        let mut h = harness();
        let media = source_file(h.paths.root(), "c.mp4", 1);
        let mut cx = OpCx::new(&h.paths, &h.assets);
        Import
            .apply(
                &mut h.project,
                serde_json::json!({ "path": media.display().to_string(), "proxy": false }),
                &mut cx,
            )
            .unwrap();
        let id = h.project.assets.keys().next().unwrap().clone();
        assert!(h.project.asset(&id).unwrap().proxy.is_none());
        Proxy
            .apply(
                &mut h.project,
                serde_json::json!({ "asset": id.to_string(), "height": 90 }),
                &mut cx,
            )
            .unwrap();
        let stored = h.project.asset(&id).unwrap().proxy.clone().unwrap();
        assert!(stored.starts_with("cache/proxy/"), "{stored}");
        assert!(h.paths.resolve(&stored).is_file());
    }

    #[test]
    fn waveform_op_refuses_an_asset_with_no_audio() {
        let mut h = harness();
        let media = source_file(h.paths.root(), "silent.mp4", 1);
        let mut cx = OpCx::new(&h.paths, &h.assets);
        Import
            .apply(
                &mut h.project,
                serde_json::json!({ "path": media.display().to_string() }),
                &mut cx,
            )
            .unwrap();
        let id = h.project.assets.keys().next().unwrap().clone();
        let err = Waveform
            .apply(&mut h.project, serde_json::json!({ "asset": id.to_string() }), &mut cx)
            .unwrap_err();
        assert!(err.to_string().contains("no audio"), "{err}");
    }

    #[test]
    fn unknown_arguments_are_rejected_rather_than_ignored() {
        let mut h = harness();
        let mut cx = OpCx::new(&h.paths, &h.assets);
        let err = Import
            .apply(
                &mut h.project,
                serde_json::json!({ "path": "x.mp4", "proxies": true }),
                &mut cx,
            )
            .unwrap_err();
        assert!(err.to_string().contains("proxies"), "{err}");
    }
}
