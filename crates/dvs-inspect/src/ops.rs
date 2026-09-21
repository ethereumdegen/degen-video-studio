//! The ops: inspection as part of the same registry every other verb lives in.
//!
//! Six queries and two mutations. The queries exist so that the digest, the lint and the
//! detectors are reachable the same way `clip.split` is — one `dvs op inspect.lint --json`,
//! one MCP tool, one journal-free call — rather than only from a bespoke CLI subcommand.
//! They are all `is_query`, so nothing they do is journaled and nothing they do can change
//! `project.json`.
//!
//! `inspect.lint` deliberately **never fails on findings**. An op that returned an error for
//! a lint hit would make a batch abort halfway and roll back edits that were perfectly
//! fine; worse, an agent would learn to stop calling it. So the findings come back in
//! `OpEffect.data`, each one is also raised as a warning, and the decision to treat them as
//! fatal belongs to the surface — the CLI maps any `Severity::Error` to exit code 4.
//!
//! The two mutations are the point of detecting anything: `marker.from-scenes` writes the
//! cuts into the document as addresses, and `seq.auto-cut-scenes` splits at them. Both
//! reuse `dvs_core::ops::clip::split_at` and the marker ordering invariant rather than
//! reimplementing them, because a second splitter would be a second set of off-by-one bugs.

use crate::analyze::{self, AnalyzeOptions, SCENE_CHANGE};
use crate::digest::{self, DigestOptions, MIN_SILENCE, SILENCE_THRESHOLD_DB};
use crate::lint::{self, LintOptions, LoudnessProfile, Severity};
use crate::Subject;
use dvs_core::error::{Error, Result};
use dvs_core::ids::{ClipId, MarkerId, TrackId};
use dvs_core::op::{args, Op, OpCx, OpEffect, Registry};
use dvs_core::ops::clip::split_at;
use dvs_core::project::{Marker, Project, TrackKind};
use dvs_core::time::{Span, Time};
use dvs_media::{probe, Toolchain};
use serde_json::json;

pub fn register(registry: &mut Registry) {
    registry
        .register(InspectDigest)
        .register(InspectLint)
        .register(InspectScenes)
        .register(InspectSilence)
        .register(InspectLoudness)
        .register(InspectProbe)
        .register(MarkerFromScenes)
        .register(SeqAutoCutScenes);
}

/// Schema fragment for the sampling knobs, shared by every op that looks at pixels so the
/// argument names cannot drift apart between them.
fn sampling_schema() -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    map.insert(
        "sample-every".to_string(),
        json!({
            "type": "string",
            "description": "Interval between analysed frames; smaller costs proportionally more",
            "default": "1/1",
            "examples": ["1/1", "5s", "1f"]
        }),
    );
    map.insert(
        "scale".to_string(),
        json!({
            "type": "number",
            "description": "Render scale for the analysed frames",
            "default": 0.5,
            "exclusiveMinimum": 0.0,
            "maximum": 1.0
        }),
    );
    map.insert(
        "proxy".to_string(),
        json!({
            "type": "boolean",
            "description": "Decode from proxies where they exist",
            "default": true
        }),
    );
    map
}

fn object_schema(
    properties: serde_json::Map<String, serde_json::Value>,
    required: &[&str],
) -> serde_json::Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

/// Read the shared sampling arguments.
fn sampling(args: &serde_json::Value, fps: dvs_core::time::Fps) -> Result<(Time, f64, bool)> {
    let every = args::opt_time(args, "sample-every", fps)?.unwrap_or(Time::from_secs(1));
    let scale = args::opt_f64(args, "scale")?.unwrap_or(0.5);
    if !(0.0..=1.0).contains(&scale) || scale == 0.0 {
        return Err(Error::bad_args(format!(
            "scale must be greater than 0 and at most 1, got {scale}"
        )));
    }
    let proxy = args::opt_bool(args, "proxy")?.unwrap_or(true);
    Ok((every, scale, proxy))
}

fn profile(args: &serde_json::Value) -> Result<LoudnessProfile> {
    match args::opt_str(args, "profile") {
        Some(text) => LoudnessProfile::parse(text),
        None => Ok(LoudnessProfile::default()),
    }
}

fn span_json(span: Span) -> serde_json::Value {
    json!({
        "start": span.start,
        "end": span.end,
        "duration": span.duration().as_secs_f64()
    })
}

// -------------------------------------------------------------- inspect.digest

struct InspectDigest;

impl Op for InspectDigest {
    fn id(&self) -> &'static str {
        "inspect.digest"
    }

    fn about(&self) -> &'static str {
        "Describe a sequence: clips, gaps, titles, captions, loudness, black/frozen/scene ranges"
    }

    fn schema(&self) -> serde_json::Value {
        let mut properties = sampling_schema();
        properties.insert(
            "audio".to_string(),
            json!({
                "type": "boolean",
                "description": "Mix and measure the audio; off skips the most expensive step",
                "default": true
            }),
        );
        object_schema(properties, &[])
    }

    fn is_query(&self) -> bool {
        true
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        &["ffmpeg", "ffprobe"]
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["sample-every", "scale", "proxy", "audio"])?;
        let sequence = cx.sequence(project)?;
        let fps = project.sequence(&sequence)?.fps;
        let (sample_every, scale, use_proxy) = sampling(&args, fps)?;
        let options = DigestOptions {
            sample_every,
            scale,
            use_proxy,
            with_audio: args::opt_bool(&args, "audio")?.unwrap_or(true),
        };
        let tool = Toolchain::shared()?;
        let subject = Subject::new(project, cx.paths, cx.assets);
        let report = digest::digest(subject, tool, &sequence, &options)?;
        let mut effect = OpEffect::new();
        for warning in &report.warnings {
            effect = effect.warn(warning.code, &warning.target, warning.detail.clone());
        }
        Ok(effect.data(serde_json::to_value(&report).map_err(|error| {
            Error::op(format!("cannot serialize the digest: {error}"))
        })?))
    }
}

// ---------------------------------------------------------------- inspect.lint

struct InspectLint;

impl Op for InspectLint {
    fn id(&self) -> &'static str {
        "inspect.lint"
    }

    fn about(&self) -> &'static str {
        "Check a sequence against every lint rule; findings are data, not an error"
    }

    fn schema(&self) -> serde_json::Value {
        let mut properties = sampling_schema();
        properties.insert(
            "profile".to_string(),
            json!({
                "type": "string",
                "description": "Loudness target to judge the mix against",
                "enum": ["youtube", "podcast", "broadcast"],
                "default": "youtube"
            }),
        );
        properties.insert(
            "render".to_string(),
            json!({
                "type": "boolean",
                "description": "Run the rules that need pixels and samples; off is a document-only pass with no ffmpeg",
                "default": true
            }),
        );
        object_schema(properties, &[])
    }

    fn is_query(&self) -> bool {
        true
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        // Only when `render` is set, which is the default; naming the tool here is what
        // makes `doctor` explain a missing ffmpeg before the op half-runs.
        &["ffmpeg", "ffprobe"]
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(
            &args,
            &["sample-every", "scale", "proxy", "profile", "render"],
        )?;
        let sequence = cx.sequence(project)?;
        let fps = project.sequence(&sequence)?.fps;
        let (sample_every, scale, use_proxy) = sampling(&args, fps)?;
        let options = LintOptions {
            profile: profile(&args)?,
            render: args::opt_bool(&args, "render")?.unwrap_or(true),
            sample_every,
            scale,
            use_proxy,
        };
        let tool = Toolchain::shared()?;
        let subject = Subject::new(project, cx.paths, cx.assets);
        let findings = lint::lint(subject, tool, &sequence, &options)?;

        let mut errors = 0usize;
        let mut warnings = 0usize;
        let mut infos = 0usize;
        let mut effect = OpEffect::new();
        for finding in &findings {
            match finding.severity {
                Severity::Error => errors += 1,
                Severity::Warning => warnings += 1,
                Severity::Info => infos += 1,
            }
            effect = effect.warn(finding.rule, &finding.target, finding.detail.clone());
        }
        Ok(effect.data(json!({
            "profile": options.profile,
            "rendered": options.render,
            "findings": findings,
            "counts": { "error": errors, "warning": warnings, "info": infos },
            "rules": lint::RULES,
            "skipped": if options.render { Vec::new() } else { lint::RENDER_RULES.to_vec() }
        })))
    }
}

// -------------------------------------------------------------- inspect.scenes

/// Shared by `inspect.scenes`, `marker.from-scenes` and `seq.auto-cut-scenes` so the three
/// cannot disagree about where the cuts are.
fn scene_threshold(args: &serde_json::Value) -> Result<f64> {
    let threshold = args::opt_f64(args, "threshold")?.unwrap_or(SCENE_CHANGE);
    if threshold <= 0.0 || threshold > 3.0 {
        return Err(Error::bad_args(format!(
            "scene threshold must be between 0 and 3 (mean absolute pixel difference), got {threshold}"
        )));
    }
    Ok(threshold)
}

fn threshold_schema() -> serde_json::Value {
    json!({
        "type": "number",
        "description": "Mean absolute pixel difference that counts as a shot change",
        "default": SCENE_CHANGE,
        "exclusiveMinimum": 0.0,
        "maximum": 3.0
    })
}

struct InspectScenes;

impl Op for InspectScenes {
    fn id(&self) -> &'static str {
        "inspect.scenes"
    }

    fn about(&self) -> &'static str {
        "Find shot changes, refined to the frame"
    }

    fn schema(&self) -> serde_json::Value {
        let mut properties = sampling_schema();
        properties.insert("threshold".to_string(), threshold_schema());
        object_schema(properties, &[])
    }

    fn is_query(&self) -> bool {
        true
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        &["ffmpeg", "ffprobe"]
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["sample-every", "scale", "proxy", "threshold"])?;
        let sequence = cx.sequence(project)?;
        let seq = project.sequence(&sequence)?;
        let fps = seq.fps;
        let (every, scale, use_proxy) = sampling(&args, fps)?;
        let threshold = scene_threshold(&args)?;
        let options = AnalyzeOptions {
            every,
            scale,
            use_proxy,
            scene_change: threshold,
            ..AnalyzeOptions::default()
        };
        let tool = Toolchain::shared()?;
        let subject = Subject::new(project, cx.paths, cx.assets);
        let analysis = analyze::analyze(subject, tool, &sequence, &options)?;
        Ok(OpEffect::new().data(json!({
            "threshold": threshold,
            "sampleEvery": every,
            "count": analysis.scene_cuts.len(),
            "cuts": analysis.scene_cuts.iter().map(|at| json!({
                "at": at,
                "timecode": at.timecode(fps),
                "frame": at.frame_round(fps)
            })).collect::<Vec<_>>()
        })))
    }
}

// ------------------------------------------------------------- inspect.silence

struct InspectSilence;

impl Op for InspectSilence {
    fn id(&self) -> &'static str {
        "inspect.silence"
    }

    fn about(&self) -> &'static str {
        "Find spans of the mix with no audible audio"
    }

    fn schema(&self) -> serde_json::Value {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "threshold-db".to_string(),
            json!({
                "type": "number",
                "description": "Level below which a window counts as silence, in dBFS",
                "default": SILENCE_THRESHOLD_DB,
                "maximum": 0.0
            }),
        );
        properties.insert(
            "min-duration".to_string(),
            json!({
                "type": "string",
                "description": "Shortest silence to report",
                "default": "1/2",
                "examples": ["1/4", "0.5", "2s"]
            }),
        );
        object_schema(properties, &[])
    }

    fn is_query(&self) -> bool {
        true
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        &["ffmpeg", "ffprobe"]
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["threshold-db", "min-duration"])?;
        let sequence = cx.sequence(project)?;
        let fps = project.sequence(&sequence)?.fps;
        let threshold = args::opt_f64(&args, "threshold-db")?.unwrap_or(SILENCE_THRESHOLD_DB);
        if threshold > 0.0 {
            return Err(Error::bad_args(format!(
                "threshold-db is a level below full scale and must not be positive, got {threshold}"
            )));
        }
        let min_duration = args::opt_time(&args, "min-duration", fps)?.unwrap_or(MIN_SILENCE);
        let tool = Toolchain::shared()?;
        let subject = Subject::new(project, cx.paths, cx.assets);
        let spans = analyze::silences(subject, tool, &sequence, threshold, min_duration)?;
        Ok(OpEffect::new().data(json!({
            "thresholdDb": threshold,
            "minDuration": min_duration,
            "count": spans.len(),
            "silences": spans.iter().copied().map(span_json).collect::<Vec<_>>()
        })))
    }
}

// ------------------------------------------------------------ inspect.loudness

struct InspectLoudness;

impl Op for InspectLoudness {
    fn id(&self) -> &'static str {
        "inspect.loudness"
    }

    fn about(&self) -> &'static str {
        "Measure the mix: integrated LUFS, true peak, loudness range, clipping"
    }

    fn schema(&self) -> serde_json::Value {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "profile".to_string(),
            json!({
                "type": "string",
                "description": "Target to compare against, and the gain that would reach it",
                "enum": ["youtube", "podcast", "broadcast"],
                "default": "youtube"
            }),
        );
        object_schema(properties, &[])
    }

    fn is_query(&self) -> bool {
        true
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        &["ffmpeg", "ffprobe"]
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["profile"])?;
        let sequence = cx.sequence(project)?;
        let target = profile(&args)?;
        let tool = Toolchain::shared()?;
        let subject = Subject::new(project, cx.paths, cx.assets);
        let (samples, spec) = analyze::mix_all(subject, tool, &sequence)?;
        if samples.is_empty() {
            return Err(Error::op(format!(
                "sequence '{}' has nothing to measure",
                project.sequence(&sequence)?.name
            )));
        }
        let loudness = dvs_audio::analyze_loudness(&samples, spec.rate, spec.channels)?;
        let gain = dvs_audio::normalize_gain_db(loudness.integrated_lufs, target.target_lufs());
        Ok(OpEffect::new().data(json!({
            "profile": target,
            "targetLufs": target.target_lufs(),
            "truePeakCeilingDb": target.true_peak_ceiling_db(),
            "gainDb": gain,
            "rate": spec.rate,
            "channels": spec.channels,
            "integratedLufs": loudness.integrated_lufs,
            "truePeakDb": loudness.true_peak_db,
            "lra": loudness.lra,
            "shortTermMinLufs": loudness.short_term_min_lufs,
            "clippedSamples": loudness.clipped_samples
        })))
    }
}

// --------------------------------------------------------------- inspect.probe

struct InspectProbe;

impl Op for InspectProbe {
    fn id(&self) -> &'static str {
        "inspect.probe"
    }

    fn about(&self) -> &'static str {
        "Re-probe an asset's stored bytes and compare with what the document believes"
    }

    fn schema(&self) -> serde_json::Value {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "asset".to_string(),
            json!({
                "type": "string",
                "description": "Asset id, name or file stem",
                "examples": ["talk.mp4", "ast_01J8XYZ"]
            }),
        );
        object_schema(properties, &["asset"])
    }

    fn is_query(&self) -> bool {
        true
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        &["ffprobe"]
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["asset"])?;
        let id = project.resolve_asset(args::str_field(&args, "asset")?)?;
        let asset = project.asset(&id)?;
        let path = cx.assets.find(&asset.hash)?;
        let tool = Toolchain::shared()?;
        let actual = probe(tool, &path)?;

        // The stored probe is what every edit decision was made against, so a difference
        // means the document and the bytes disagree — which happens when a project is
        // hand-edited or an asset entry is copied between projects.
        let differences = probe_differences(&asset.probe, &actual.probe);
        let proxy = asset.proxy.as_ref().map(|relative| {
            let resolved = cx.paths.resolve(relative);
            json!({
                "path": relative,
                "exists": std::fs::metadata(&resolved).is_ok_and(|meta| meta.len() > 0)
            })
        });
        let mut effect = OpEffect::new();
        for difference in &differences {
            effect = effect.warn("probe-mismatch", &id, difference.clone());
        }
        Ok(effect.data(json!({
            "asset": id,
            "name": asset.name,
            "hash": asset.hash,
            "path": path.display().to_string(),
            "kind": actual.kind,
            "stored": asset.probe,
            "actual": actual.probe,
            "matches": differences.is_empty(),
            "differences": differences,
            "proxy": proxy
        })))
    }
}

/// Fields where a stored probe and a fresh one disagree, named one per line so the message
/// says what to fix rather than dumping two JSON blobs.
fn probe_differences(
    stored: &dvs_core::project::Probe,
    actual: &dvs_core::project::Probe,
) -> Vec<String> {
    let mut out = Vec::new();
    if stored.duration != actual.duration {
        out.push(format!(
            "duration: document says {}, the file is {}",
            stored.duration, actual.duration
        ));
    }
    match (&stored.video, &actual.video) {
        (Some(stored), Some(actual)) => {
            if stored.size != actual.size {
                out.push(format!(
                    "video size: document says {}x{}, the file is {}x{}",
                    stored.size[0], stored.size[1], actual.size[0], actual.size[1]
                ));
            }
            if stored.fps != actual.fps {
                out.push(format!(
                    "video rate: document says {}, the file is {}",
                    stored.fps, actual.fps
                ));
            }
            if stored.codec != actual.codec {
                out.push(format!(
                    "video codec: document says {}, the file is {}",
                    stored.codec, actual.codec
                ));
            }
        }
        (Some(_), None) => out.push("video: the document expects a video stream, the file has none".to_string()),
        (None, Some(_)) => out.push("video: the file has a video stream the document does not know about".to_string()),
        (None, None) => {}
    }
    match (&stored.audio, &actual.audio) {
        (Some(stored), Some(actual)) => {
            if stored.rate != actual.rate || stored.channels != actual.channels {
                out.push(format!(
                    "audio: document says {} Hz / {} ch, the file is {} Hz / {} ch",
                    stored.rate, stored.channels, actual.rate, actual.channels
                ));
            }
        }
        (Some(_), None) => out.push("audio: the document expects an audio stream, the file has none".to_string()),
        (None, Some(_)) => out.push("audio: the file has an audio stream the document does not know about".to_string()),
        (None, None) => {}
    }
    out
}

// ----------------------------------------------------------- marker.from-scenes

struct MarkerFromScenes;

impl Op for MarkerFromScenes {
    fn id(&self) -> &'static str {
        "marker.from-scenes"
    }

    fn about(&self) -> &'static str {
        "Add a marker at every detected shot change"
    }

    fn schema(&self) -> serde_json::Value {
        let mut properties = sampling_schema();
        properties.insert("threshold".to_string(), threshold_schema());
        properties.insert(
            "prefix".to_string(),
            json!({
                "type": "string",
                "description": "Marker name prefix; markers are named '<prefix> N'",
                "default": "scene"
            }),
        );
        properties.insert(
            "replace".to_string(),
            json!({
                "type": "boolean",
                "description": "Remove existing markers with this prefix first, so re-running is idempotent",
                "default": true
            }),
        );
        object_schema(properties, &[])
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        &["ffmpeg", "ffprobe"]
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(
            &args,
            &["sample-every", "scale", "proxy", "threshold", "prefix", "replace"],
        )?;
        let sequence = cx.sequence(project)?;
        let fps = project.sequence(&sequence)?.fps;
        let (every, scale, use_proxy) = sampling(&args, fps)?;
        let threshold = scene_threshold(&args)?;
        let prefix = args::opt_str(&args, "prefix").unwrap_or("scene").to_string();
        let replace = args::opt_bool(&args, "replace")?.unwrap_or(true);

        let cuts = {
            let options = AnalyzeOptions {
                every,
                scale,
                use_proxy,
                scene_change: threshold,
                ..AnalyzeOptions::default()
            };
            let tool = Toolchain::shared()?;
            let subject = Subject::new(project, cx.paths, cx.assets);
            analyze::analyze(subject, tool, &sequence, &options)?.scene_cuts
        };

        let mut effect = OpEffect::new();
        let seq = project.sequence_mut(&sequence)?;
        if replace {
            let stale: Vec<MarkerId> = seq
                .markers
                .iter()
                .filter(|marker| marker.name.starts_with(&prefix))
                .map(|marker| marker.id.clone())
                .collect();
            seq.markers
                .retain(|marker| !marker.name.starts_with(&prefix));
            for id in stale {
                effect = effect.removed(id);
            }
        }
        for (index, at) in cuts.iter().enumerate() {
            let marker = Marker {
                id: MarkerId::new(),
                at: *at,
                name: format!("{prefix} {}", index + 1),
                color: None,
                note: Some(format!(
                    "shot change detected at {} (threshold {threshold})",
                    at.timecode(fps)
                )),
            };
            effect = effect.created(&marker.id);
            // The marker list is sorted by time: `digest` and playhead navigation both read
            // it in order, so an append would break "the next marker".
            let index = seq.markers.partition_point(|existing| existing.at <= *at);
            seq.markers.insert(index, marker);
        }
        Ok(effect.data(json!({ "threshold": threshold, "cuts": cuts.len() })))
    }
}

// -------------------------------------------------------- seq.auto-cut-scenes

struct SeqAutoCutScenes;

impl Op for SeqAutoCutScenes {
    fn id(&self) -> &'static str {
        "seq.auto-cut-scenes"
    }

    fn about(&self) -> &'static str {
        "Split clips at every detected shot change"
    }

    fn schema(&self) -> serde_json::Value {
        let mut properties = sampling_schema();
        properties.insert("threshold".to_string(), threshold_schema());
        properties.insert(
            "track".to_string(),
            json!({
                "type": "string",
                "description": "Track selector; defaults to every unlocked video track",
                "examples": ["V1", "track[kind=video]"]
            }),
        );
        object_schema(properties, &[])
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        &["ffmpeg", "ffprobe"]
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(
            &args,
            &["sample-every", "scale", "proxy", "threshold", "track"],
        )?;
        let sequence = cx.sequence(project)?;
        let fps = project.sequence(&sequence)?.fps;
        let (every, scale, use_proxy) = sampling(&args, fps)?;
        let threshold = scene_threshold(&args)?;

        let targets: Vec<TrackId> = match args::opt_str(&args, "track") {
            Some(selector) => dvs_core::selector::resolve_tracks(project, &sequence, selector)?,
            None => project
                .sequence(&sequence)?
                .tracks
                .iter()
                .filter(|track| track.kind == TrackKind::Video && !track.locked)
                .map(|track| track.id.clone())
                .collect(),
        };
        if targets.is_empty() {
            return Err(Error::op(format!(
                "sequence '{}' has no unlocked video track to cut",
                project.sequence(&sequence)?.name
            )));
        }

        let cuts = {
            let options = AnalyzeOptions {
                every,
                scale,
                use_proxy,
                scene_change: threshold,
                ..AnalyzeOptions::default()
            };
            let tool = Toolchain::shared()?;
            let subject = Subject::new(project, cx.paths, cx.assets);
            analyze::analyze(subject, tool, &sequence, &options)?.scene_cuts
        };

        let mut effect = OpEffect::new().data(json!({
            "threshold": threshold,
            "cuts": cuts.len()
        }));
        for track_id in targets {
            let seq = project.sequence_mut(&sequence)?;
            let track = seq.track_mut(&track_id)?;
            dvs_core::ops::util::assert_unlocked(track)?;
            // Walk the cuts from the end: a split inserts a clip, so any index or position
            // taken before it would be stale for the cuts after it.
            let mut ordered: Vec<Time> = cuts.clone();
            ordered.sort();
            for at in ordered.into_iter().rev() {
                let Some(index) = track
                    .clips
                    .iter()
                    // Strictly inside: a cut that lands on a clip boundary is already a cut,
                    // and splitting there would produce a zero-length clip the engine
                    // rejects — taking the whole transaction with it.
                    .position(|clip| clip.start < at && at < clip.end())
                else {
                    continue;
                };
                let linked = track.clips[index].link.is_some();
                let source = track.clips[index].id.clone();
                let tail = split_at(&mut track.clips[index], at);
                let tail_id: ClipId = tail.id.clone();
                track.clips.insert(index + 1, tail);
                effect = effect.changed(&source).created(&tail_id);
                if linked {
                    effect = effect.warn(
                        "link-dropped",
                        &tail_id,
                        "the a/v pairing stayed with the first half; link the new clip if it needs one",
                    );
                }
            }
        }
        Ok(effect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::*;
    use dvs_core::color::Rgba;
    use dvs_core::engine::Engine;
    use dvs_core::project::Source;
    use serde_json::json;

    const RED: Rgba = Rgba::opaque(200, 40, 40);
    const BLUE: Rgba = Rgba::opaque(30, 60, 200);

    fn registry() -> Registry {
        let mut registry = Registry::new();
        register(&mut registry);
        registry
    }

    /// Two visually different shots with the cut at 2.5 s, which is deliberately between
    /// the one-second samples: the ops have to report the frame, not the sample.
    fn two_shots(fixture: &mut Fixture) {
        let track = fixture.video();
        fixture.push(&track, color_clip(RED, Time::ZERO, secs(5, 2)));
        fixture.push(&track, color_clip(BLUE, secs(5, 2), secs(5, 2)));
    }

    /// The tempdir comes back with the engine on purpose: dropping it deletes the project
    /// directory, and an op that decodes media would then fail with a missing blob rather
    /// than with whatever the test is about.
    fn engine(fixture: Fixture) -> (Engine, tempfile::TempDir) {
        let Fixture { dir, ws } = fixture;
        (Engine::new(registry(), ws), dir)
    }

    #[test]
    fn the_registered_ids_are_the_published_ones() {
        let registry = registry();
        let mut ids = registry.ids();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![
                "inspect.digest",
                "inspect.lint",
                "inspect.loudness",
                "inspect.probe",
                "inspect.scenes",
                "inspect.silence",
                "marker.from-scenes",
                "seq.auto-cut-scenes",
            ]
        );
        for op in registry.iter() {
            let query = op.id().starts_with("inspect.");
            assert_eq!(
                op.is_query(),
                query,
                "'{}' is on the wrong side of the query line",
                op.id()
            );
        }
    }

    #[test]
    fn lint_reports_findings_as_data_instead_of_failing() {
        let mut fixture = Fixture::new([320, 180]);
        // A hole and a clip reading past its source: one warning, one error.
        let inner = fixture.nested(secs(2, 1));
        let track = fixture.video();
        fixture.push(
            &track,
            dvs_core::project::Clip::new(
                Source::Sequence { sequence: inner },
                Time::ZERO,
                secs(5, 1),
            ),
        );
        fixture.push(&track, color_clip(RED, secs(8, 1), secs(1, 1)));

        let (mut engine, _dir) = engine(fixture);
        let applied = engine
            .apply("inspect.lint", json!({ "render": false }), None, false)
            .expect("lint must not fail on findings");
        let data = applied.effect.data.expect("findings in data");
        assert!(
            data["counts"]["error"].as_u64().expect("error count") >= 1,
            "past-source-end is an error: {data}"
        );
        assert!(
            data["counts"]["warning"].as_u64().expect("warning count") >= 1,
            "the gap is a warning: {data}"
        );
        assert_eq!(
            data["rules"].as_array().expect("rule list").len(),
            crate::lint::RULES.len()
        );
        assert_eq!(
            data["skipped"].as_array().expect("skipped list").len(),
            crate::lint::RENDER_RULES.len(),
            "a document-only pass says which rules it did not run"
        );
        assert!(
            !applied.effect.warnings.is_empty(),
            "findings also surface as warnings so a human sees them"
        );
    }

    #[test]
    fn a_query_op_leaves_no_history_behind() {
        let mut fixture = Fixture::new([320, 180]);
        two_shots(&mut fixture);
        let (mut engine, _dir) = engine(fixture);
        let applied = engine
            .apply("inspect.scenes", json!({}), None, false)
            .expect("scenes");
        assert_eq!(applied.seq, None, "a query is never journaled");
        let cuts = applied.effect.data.expect("data")["cuts"]
            .as_array()
            .expect("cuts")
            .len();
        assert_eq!(cuts, 1, "one shot change");
    }

    #[test]
    fn scene_markers_land_on_the_cut_and_re_running_does_not_duplicate_them() {
        let mut fixture = Fixture::new([320, 180]);
        two_shots(&mut fixture);
        let (mut engine, _dir) = engine(fixture);

        engine
            .apply("marker.from-scenes", json!({}), None, false)
            .expect("markers");
        let sequence = engine.workspace.project.active_sequence.clone();
        let markers = engine
            .workspace
            .project
            .sequence(&sequence)
            .expect("sequence")
            .markers
            .clone();
        assert_eq!(markers.len(), 1, "one cut, one marker");
        assert_eq!(markers[0].at, secs(5, 2), "at the cut, not at the sample");
        assert_eq!(markers[0].name, "scene 1");

        engine
            .apply("marker.from-scenes", json!({}), None, false)
            .expect("markers again");
        assert_eq!(
            engine
                .workspace
                .project
                .sequence(&sequence)
                .expect("sequence")
                .markers
                .len(),
            1,
            "re-running replaces its own markers rather than piling up"
        );
    }

    #[test]
    fn auto_cut_splits_at_the_detected_cut_and_keeps_the_timeline_intact() {
        let mut fixture = Fixture::new([320, 180]);
        // One clip covering both shots: a colour change inside a single clip, which is what
        // footage with a hard cut in it looks like to the document.
        let track = fixture.video();
        let media = fixture.synth("shots.mp4", "testsrc2", 4, [320, 180]);
        let asset = fixture.import(&media, "shots.mp4");
        fixture.push(
            &track,
            dvs_core::project::Clip::new(
                Source::Asset { asset, stream: None },
                Time::ZERO,
                secs(4, 1),
            ),
        );
        let before = {
            let seq = fixture.sequence();
            (seq.tracks[0].clips.len(), seq.duration())
        };
        let (mut engine, _dir) = engine(fixture);
        let applied = engine
            // testsrc2 changes continuously, so a low threshold is what finds its steps.
            .apply(
                "seq.auto-cut-scenes",
                json!({ "threshold": 0.05 }),
                None,
                false,
            )
            .expect("auto-cut");
        assert!(
            !applied.effect.created.is_empty(),
            "at least one split happened"
        );

        let sequence = engine.workspace.project.active_sequence.clone();
        let seq = engine
            .workspace
            .project
            .sequence(&sequence)
            .expect("sequence");
        assert!(
            seq.tracks[0].clips.len() > before.0,
            "the track gained clips"
        );
        assert_eq!(
            seq.duration(),
            before.1,
            "splitting moves nothing, so the sequence is the same length"
        );
        assert!(
            seq.tracks[0].gaps(seq.duration()).is_empty(),
            "and leaves no hole between the halves"
        );
        // The engine validated the document on commit, so the halves cannot overlap; this
        // checks the other half of the invariant, that the source is continuous across the
        // cut.
        for pair in seq.tracks[0].clips.windows(2) {
            assert_eq!(
                pair[0].source_span().end,
                pair[1].source_in,
                "the halves play as the one clip did"
            );
        }
    }

    #[test]
    fn auto_cut_refuses_a_sequence_with_no_video_track() {
        let mut fixture = Fixture::new([320, 180]);
        fixture.tone_track("bed.wav", Span::new(Time::ZERO, secs(2, 1)), 0.4);
        let (mut engine, _dir) = engine(fixture);
        let error = engine
            .apply("seq.auto-cut-scenes", json!({}), None, false)
            .expect_err("nothing to cut");
        assert!(
            error.to_string().contains("video track"),
            "the error says what is missing: {error}"
        );
    }

    #[test]
    fn probing_an_asset_notices_the_document_disagreeing_with_the_bytes() {
        let mut fixture = Fixture::new([320, 180]);
        let media = fixture.synth("talk.mp4", "testsrc2", 2, [320, 180]);
        let asset = fixture.import(&media, "talk.mp4");
        // Claim a resolution the file does not have, the way a hand-edited document or a
        // copied asset entry would.
        if let Some(stream) = fixture
            .ws
            .project
            .assets
            .get_mut(&asset)
            .expect("asset")
            .probe
            .video
            .as_mut()
        {
            stream.size = [1920, 1080];
        }
        let (mut engine, _dir) = engine(fixture);
        let applied = engine
            .apply("inspect.probe", json!({ "asset": "talk.mp4" }), None, false)
            .expect("probe");
        let data = applied.effect.data.expect("data");
        assert_eq!(data["matches"], json!(false));
        assert!(
            data["differences"]
                .as_array()
                .expect("differences")
                .iter()
                .any(|line| line.as_str().is_some_and(|text| text.contains("video size"))),
            "the mismatch is named: {data}"
        );
        assert!(applied
            .effect
            .warnings
            .iter()
            .any(|warning| warning.code == "probe-mismatch"));
    }

    #[test]
    fn loudness_reports_the_gain_that_would_reach_the_profile() {
        let mut fixture = Fixture::new([320, 180]);
        fixture.tone_track("bed.wav", Span::new(Time::ZERO, secs(2, 1)), 0.5);
        let (mut engine, _dir) = engine(fixture);
        let applied = engine
            .apply(
                "inspect.loudness",
                json!({ "profile": "podcast" }),
                None,
                false,
            )
            .expect("loudness");
        let data = applied.effect.data.expect("data");
        assert_eq!(data["targetLufs"], json!(-16.0));
        let measured = data["integratedLufs"].as_f64().expect("measured");
        let gain = data["gainDb"].as_f64().expect("gain");
        assert!(
            (measured + gain - -16.0).abs() < 1e-3,
            "measured {measured} plus gain {gain} must land on the target"
        );
    }

    #[test]
    fn a_typo_in_an_argument_is_an_error_rather_than_a_silent_default() {
        let mut fixture = Fixture::new([320, 180]);
        two_shots(&mut fixture);
        let (mut engine, _dir) = engine(fixture);
        let error = engine
            .apply("inspect.scenes", json!({ "threshhold": 0.4 }), None, false)
            .expect_err("unknown argument");
        assert!(
            error.to_string().contains("threshhold"),
            "the error names the typo: {error}"
        );
    }

    #[test]
    fn a_dry_run_of_a_mutating_inspection_changes_nothing() {
        let mut fixture = Fixture::new([320, 180]);
        two_shots(&mut fixture);
        let (mut engine, _dir) = engine(fixture);
        engine
            .apply("marker.from-scenes", json!({}), None, true)
            .expect("dry run");
        let sequence = engine.workspace.project.active_sequence.clone();
        assert!(
            engine
                .workspace
                .project
                .sequence(&sequence)
                .expect("sequence")
                .markers
                .is_empty(),
            "a dry run reports without writing"
        );
    }
}
