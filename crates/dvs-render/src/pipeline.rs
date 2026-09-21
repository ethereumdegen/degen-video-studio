//! Rendering a sequence to a file: schedule, encode the misses, join, mux.
//!
//! The shape of a render is fixed by the cache. Frames cannot be streamed straight into one
//! encoder, because then nothing is reusable; they go into per-segment closed-GOP chunks
//! that `concat -c copy` can join without re-compressing. That costs one process per
//! segment and buys the only property that makes an agent loop bearable: the twentieth edit
//! re-encodes the seconds it touched, not the minutes it did not.
//!
//! Audio is the exception and is bounced once for the whole range. A mix is not a sequence
//! of independent windows — a fade, a duck or a reverb tail crosses a video cut without
//! caring — so splitting the bounce at video boundaries would put an audible seam at every
//! segment edge to save work that is cheap anyway.
//!
//! Two details that look like paranoia and are not:
//!
//! - A chunk is encoded to `<key>.tmpNNN.mp4` and renamed into place only after ffmpeg
//!   exits successfully. A truncated file sitting at `<key>.mp4` would be a cache *hit*
//!   forever after, which is the one failure mode this crate must not have.
//! - The mixed PCM is padded by one frame of silence before muxing, because
//!   [`dvs_media::mux_audio`] passes `-shortest`: the output is as long as the shortest
//!   stream, so the video must be that stream. AAC happens to round its last frame *up*
//!   and would survive without the pad, but that is a property of one codec's framing, not
//!   of the pipeline, and a render one frame short of the timeline is exactly the kind of
//!   defect nobody notices until the cut lands in someone else's edit.

use crate::cache;
use crate::plan;
use dvs_audio::{analyze_loudness, mix_span, Loudness, MixSpec};
use dvs_comp::{CompOptions, Compositor};
use dvs_core::color::Rgba;
use dvs_core::engine::Workspace;
use dvs_core::error::{Error, Result};
use dvs_core::ids::SequenceId;
use dvs_core::paths::ProjectPaths;
use dvs_core::project::{Sequence, Tools};
use dvs_core::time::{Fps, Span, Time};
use dvs_core::ENGINE_VERSION;
use dvs_media::{
    concat, mux_audio, write_pcm, write_png, AudioSpec, EncodeSpec, Encoder, EncoderSession, Frame,
    Toolchain,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// What to render and how.
#[derive(Debug, Clone)]
pub struct RenderSpec {
    /// Where the finished file goes. The container is chosen by its extension.
    pub output: PathBuf,
    /// Which sequence to render.
    pub sequence: SequenceId,
    /// `None` renders the whole sequence.
    pub range: Option<Span>,
    /// Output scale against the sequence size. `0.5` is a half-size preview.
    pub scale: f64,
    /// Video encoder. [`Encoder::Auto`] resolves to `libx264`, which exists everywhere.
    pub encoder: Encoder,
    /// CRF-like quality; lower is better. 18 is the project default.
    pub quality: u32,
    /// Encoder speed preset, `None` for the encoder's own default.
    pub preset: Option<String>,
    /// Decode from proxies. Fast and lower quality: right for a preview, wrong for delivery.
    pub use_proxy: bool,
    /// Ignore cached chunks and re-encode every segment. What goldens and `--no-cache`
    /// renders use to prove the cache is not lying.
    pub no_cache: bool,
    /// Audio codec for the muxed mix, e.g. `aac`, `libopus`.
    pub audio_codec: String,
    /// Audio bitrate for that codec, e.g. `192k`.
    pub audio_bitrate: String,
    /// Return the per-frame compositor reports and the mix loudness in the report.
    pub with_digest: bool,
}

impl RenderSpec {
    /// Delivery defaults: full size, `libx264` at CRF 18, AAC 192k, cache on.
    pub fn new(output: impl Into<PathBuf>, sequence: SequenceId) -> RenderSpec {
        RenderSpec {
            output: output.into(),
            sequence,
            range: None,
            scale: 1.0,
            encoder: Encoder::Auto,
            quality: 18,
            preset: None,
            use_proxy: false,
            no_cache: false,
            audio_codec: "aac".to_string(),
            audio_bitrate: "192k".to_string(),
            with_digest: false,
        }
    }
}

/// What a render did. `segmentsReused` is the number an agent watches: it is the difference
/// between an edit loop and a batch job.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderReport {
    /// The file that was written.
    pub output: PathBuf,
    /// Frames in the rendered range, encoded or reused.
    pub frames: i64,
    /// Length of the rendered range.
    pub duration: Time,
    /// Wall time of the whole render, including the cache hits it skipped.
    pub render_ms: u128,
    /// Segments in the plan.
    pub segments_total: usize,
    /// How many of them came from the cache instead of an encoder.
    pub segments_reused: usize,
    /// Decoder seeks performed. A number close to the frame count means the timeline is
    /// thrashing the decoders — usually a reversed or heavily retimed clip.
    pub seeks: u32,
    /// Per-frame compositor reports plus the mix loudness, when `with_digest` asked for
    /// them. `dvs-inspect` turns this into the digest an agent reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<Value>,
    /// Non-fatal problems: a reused segment whose report sidecar went missing, a mix that
    /// could not be measured. The file was still written.
    pub warnings: Vec<String>,
    /// What produced this file: engine version, ffmpeg version, encoders used. Encoded
    /// bytes are not reproducible across ffmpeg builds, so a golden has to be able to say
    /// which build made it. Reported, not persisted — `dvs doctor` is the single writer of
    /// `project.tools`, because a render that rewrote the document on every run would make
    /// `project.json` churn for a command that is supposed to only read it.
    pub tools: Tools,
}

/// Render progress, for surfaces that show it.
///
/// Taken as `&dyn Progress` rather than a generic so `render_with` is one function: the CLI
/// draws a bar, the MCP server passes [`Silent`], and neither costs the other a monomorphic
/// copy of the whole pipeline.
pub trait Progress {
    /// A segment is about to be produced (`reused` = it came from the cache).
    fn segment(&self, index: usize, total: usize, reused: bool);
    /// `done` of `total` frames of the whole range are accounted for.
    fn frame(&self, done: i64, total: i64);
}

/// The no-op reporter.
#[derive(Debug, Clone, Copy, Default)]
pub struct Silent;

impl Progress for Silent {
    fn segment(&self, _index: usize, _total: usize, _reused: bool) {}
    fn frame(&self, _done: i64, _total: i64) {}
}

/// Render a sequence to `spec.output`, reusing every cached segment that is still valid.
pub fn render(workspace: &Workspace, tool: &Toolchain, spec: &RenderSpec) -> Result<RenderReport> {
    render_with(workspace, tool, spec, &Silent)
}

/// [`render`] with progress callbacks.
pub fn render_with(
    workspace: &Workspace,
    tool: &Toolchain,
    spec: &RenderSpec,
    progress: &dyn Progress,
) -> Result<RenderReport> {
    let started = Instant::now();
    let project = &workspace.project;
    let paths = &workspace.paths;
    let seq = project.sequence(&spec.sequence)?;
    let fps = seq.fps;
    let range = resolve_range(seq, spec.range)?;
    let total_frames = frames_in(range, fps);
    let segments = plan::segment_plan(project, &spec.sequence, range)?;

    let segment_dir = paths.segment_dir();
    std::fs::create_dir_all(&segment_dir).map_err(|e| Error::io(&segment_dir, e))?;

    let options = CompOptions {
        scale: spec.scale,
        use_proxy: spec.use_proxy,
        scaler: "bicubic",
    };
    // Built on the first miss and kept for the rest of the render: the decoder sessions and
    // the font database inside it are the expensive part, and an all-hit render must not
    // pay for them at all.
    let mut compositor: Option<Compositor> = None;

    let mut warnings: Vec<String> = Vec::new();
    let mut chunks: Vec<PathBuf> = Vec::with_capacity(segments.len());
    let mut frame_reports: Vec<Value> = Vec::new();
    let mut segment_reports: Vec<Value> = Vec::with_capacity(segments.len());
    let mut reused = 0usize;
    let mut done = 0i64;

    for (index, span) in segments.iter().enumerate() {
        let key = cache::segment_key(project, &spec.sequence, *span, spec, tool)?;
        let chunk = cache::segment_path(paths, &key);
        let hit = !spec.no_cache && chunk.is_file();
        progress.segment(index, segments.len(), hit);
        let segment_frames = frames_in(*span, fps);

        if hit {
            reused += 1;
            // Only the digest consumes these, and a ten-minute render is eighteen thousand
            // of them: reading the sidecars back when nobody asked would be pure garbage.
            if spec.with_digest {
                match read_reports(paths, &key) {
                    Ok(reports) => frame_reports.extend(reports),
                    Err(error) => warnings.push(format!(
                        "segment {key} was reused but its frame report is unreadable \
                         ({error}); the digest is missing {segment_frames} frame(s)"
                    )),
                }
            }
            done += segment_frames;
            progress.frame(done, total_frames);
        } else {
            if compositor.is_none() {
                compositor = Some(Compositor::new(
                    tool,
                    project,
                    paths,
                    &workspace.assets,
                    &spec.sequence,
                    options.clone(),
                )?);
            }
            let comp = compositor.as_mut().expect("compositor was just built");
            let background = comp.background()?;
            let reports = encode_segment(
                tool,
                comp,
                spec,
                background,
                fps,
                *span,
                &chunk,
                progress,
                &mut done,
                total_frames,
            )?;
            write_reports(paths, &key, &reports)?;
            if spec.with_digest {
                frame_reports.extend(reports);
            }
        }

        chunks.push(chunk);
        segment_reports.push(json!({
            "key": key,
            "range": { "start": span.start, "end": span.end },
            "frames": segment_frames,
            "reused": hit,
        }));
    }

    let bounce = if dvs_audio::has_audio(&workspace.project, &spec.sequence, range)? {
        Some(bounce_audio(workspace, tool, spec, seq, range)?)
    } else {
        None
    };

    let joined = WorkFile::new(&paths.cache_dir(), "join.mp4")?;
    concat(tool, &chunks, joined.path())?;
    if let Some(parent) = spec.output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
    }
    match &bounce {
        Some(bounce) => mux_audio(tool, joined.path(), &bounce.spec, &spec.output)?,
        None => move_file(joined.path(), &spec.output)?,
    }

    let mut digest = None;
    if spec.with_digest {
        let loudness = match &bounce {
            Some(bounce) => {
                let measured =
                    analyze_loudness(&bounce.samples, bounce.spec.rate, bounce.spec.channels);
                match measured {
                    Ok(loudness) => Some(loudness),
                    Err(error) => {
                        warnings.push(format!("the mix could not be measured: {error}"));
                        None
                    }
                }
            }
            None => None,
        };
        digest = Some(digest_value(
            seq,
            range,
            total_frames,
            &segment_reports,
            frame_reports,
            loudness,
        ));
    }

    let mut encoders = vec![spec.encoder.resolve(tool)?];
    if bounce.is_some() {
        encoders.push(spec.audio_codec.clone());
    }
    Ok(RenderReport {
        output: spec.output.clone(),
        frames: total_frames,
        duration: range.duration(),
        render_ms: started.elapsed().as_millis(),
        segments_total: segments.len(),
        segments_reused: reused,
        seeks: compositor.as_ref().map_or(0, |comp| comp.seek_count()),
        digest,
        warnings,
        tools: Tools {
            engine: Some(ENGINE_VERSION.to_string()),
            ffmpeg: Some(tool.version().to_string()),
            encoders,
        },
    })
}

/// The cache keys a render of `spec` would ask for, in timeline order.
///
/// Feeds [`cache::status`] ("how much of this render is already done?") and
/// [`cache::prune`] ("what is safe to delete?") without rendering anything.
pub fn plan_keys(
    workspace: &Workspace,
    tool: &Toolchain,
    spec: &RenderSpec,
) -> Result<Vec<String>> {
    let project = &workspace.project;
    let seq = project.sequence(&spec.sequence)?;
    let range = resolve_range(seq, spec.range)?;
    plan::segment_plan(project, &spec.sequence, range)?
        .into_iter()
        .map(|span| cache::segment_key(project, &spec.sequence, span, spec, tool))
        .collect()
}

/// Composite one instant and write it as a PNG.
///
/// The frame is the same one a render would encode at that instant — same compositor, same
/// keyframe evaluation — so `dvs frame` is a proof about the render, not a second guess.
pub fn render_frame(
    workspace: &Workspace,
    tool: &Toolchain,
    sequence: &SequenceId,
    at: Time,
    scale: f64,
    use_proxy: bool,
    output: &Path,
) -> Result<()> {
    let mut comp = Compositor::new(
        tool,
        &workspace.project,
        &workspace.paths,
        &workspace.assets,
        sequence,
        CompOptions {
            scale,
            use_proxy,
            scaler: "bicubic",
        },
    )?;
    let frame = comp.frame_at(at)?;
    write_png(tool, &frame, output)
}

/// Sample frames every `every` across `range`, for the contact sheet and the viewport.
///
/// Proxies are used when an asset has one: a sheet exists to be looked at quickly, and
/// decoding full-resolution frames to shrink them into a grid is the slow way to get the
/// same picture.
pub fn preview_frames(
    workspace: &Workspace,
    tool: &Toolchain,
    sequence: &SequenceId,
    range: Span,
    every: Time,
    scale: f64,
) -> Result<Vec<(Time, Frame)>> {
    if !every.is_positive() {
        return Err(Error::bad_args(format!(
            "sampling interval {every} must be positive"
        )));
    }
    let seq = workspace.project.sequence(sequence)?;
    let fps = seq.fps;
    let range = resolve_range(seq, Some(range))?;
    let mut comp = Compositor::new(
        tool,
        &workspace.project,
        &workspace.paths,
        &workspace.assets,
        sequence,
        CompOptions {
            scale,
            use_proxy: true,
            scaler: "bilinear",
        },
    )?;
    let step = every.frame_ceil(fps).max(1);
    let mut out = Vec::new();
    let mut index = range.start.frame_floor(fps);
    let end = range.end.frame_ceil(fps);
    while index < end {
        let at = Time::from_frames(index, fps);
        out.push((at, comp.frame(index)?));
        index += step;
    }
    Ok(out)
}

/// Encode one segment into `chunk`, returning the compositor's per-frame reports.
#[allow(clippy::too_many_arguments)]
fn encode_segment(
    tool: &Toolchain,
    comp: &mut Compositor<'_>,
    spec: &RenderSpec,
    background: Rgba,
    fps: Fps,
    span: Span,
    chunk: &Path,
    progress: &dyn Progress,
    done: &mut i64,
    total: i64,
) -> Result<Vec<Value>> {
    // Encode beside the final name, rename on success: an ffmpeg that dies halfway must not
    // leave a short file under a key that every later render would happily reuse. The
    // serial keeps two renders in one process off each other's staging file.
    let partial = chunk.with_extension(format!(
        "tmp{}-{}.mp4",
        std::process::id(),
        WORK_SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    let mut encode = EncodeSpec::new(&partial, comp.size(), fps);
    encode.encoder = spec.encoder.clone();
    encode.quality = spec.quality;
    encode.preset = spec.preset.clone();
    encode.background = background;
    // Every chunk must stand alone for `concat -c copy` to be a legal operation.
    encode.closed_gop = true;

    let mut session = EncoderSession::start(tool, &encode)?;
    let mut reports = Vec::new();
    for index in plan::frame_range(span, fps) {
        let (frame, report) = comp.frame_with_report(index)?;
        session.write(&frame)?;
        reports.push(
            serde_json::to_value(&report)
                .map_err(|e| Error::op(format!("frame report {index} is not serializable: {e}")))?,
        );
        *done += 1;
        progress.frame(*done, total);
    }
    session.finish()?;
    std::fs::rename(&partial, chunk).map_err(|e| Error::io(chunk, e))?;
    Ok(reports)
}

fn write_reports(paths: &ProjectPaths, key: &str, reports: &[Value]) -> Result<()> {
    let path = cache::report_path(paths, key);
    let bytes = serde_json::to_vec(reports)
        .map_err(|e| Error::op(format!("frame reports are not serializable: {e}")))?;
    std::fs::write(&path, bytes).map_err(|e| Error::io(&path, e))
}

fn read_reports(paths: &ProjectPaths, key: &str) -> Result<Vec<Value>> {
    let path = cache::report_path(paths, key);
    let bytes = std::fs::read(&path).map_err(|e| Error::io(&path, e))?;
    serde_json::from_slice(&bytes).map_err(|e| Error::json(&path, e))
}

/// The frame-aligned range a render covers.
fn resolve_range(seq: &Sequence, requested: Option<Span>) -> Result<Span> {
    let range = requested.unwrap_or_else(|| Span::new(Time::ZERO, seq.duration()));
    let fps = seq.fps;
    let aligned = Span::new(
        Time::from_frames(range.start.frame_floor(fps), fps),
        Time::from_frames(range.end.frame_ceil(fps), fps),
    );
    if aligned.is_empty() {
        return Err(Error::bad_args(format!(
            "nothing to render in {range} of sequence '{}'; it is {} long",
            seq.name,
            seq.duration()
        )));
    }
    Ok(aligned)
}

fn frames_in(span: Span, fps: Fps) -> i64 {
    let range = plan::frame_range(span, fps);
    range.end - range.start
}



/// The mixed range on disk as f32 PCM, plus the samples themselves for measurement.
struct Bounce {
    /// Deletes the PCM when the render ends; `spec` points at it.
    _pcm: WorkFile,
    spec: AudioSpec,
    samples: Vec<f32>,
}

fn bounce_audio(
    workspace: &Workspace,
    tool: &Toolchain,
    spec: &RenderSpec,
    seq: &Sequence,
    range: Span,
) -> Result<Bounce> {
    let mix = MixSpec::of(seq);
    let mut samples = mix_span(
        &workspace.project,
        &spec.sequence,
        range,
        mix,
        tool,
        &workspace.assets,
        &workspace.paths,
    )?;
    // `-shortest` makes the output as long as the shortest stream; keep that the video.
    let pad = seq.fps.frame_duration().sample_round(mix.rate).max(0) as usize
        * usize::from(mix.channels);
    samples.resize(samples.len() + pad, 0.0);

    let pcm = WorkFile::new(&workspace.paths.cache_dir(), "pcm")?;
    write_pcm(pcm.path(), &samples)?;
    Ok(Bounce {
        spec: AudioSpec {
            pcm: pcm.path().to_path_buf(),
            rate: mix.rate,
            channels: mix.channels,
            codec: spec.audio_codec.clone(),
            bitrate: spec.audio_bitrate.clone(),
        },
        _pcm: pcm,
        samples,
    })
}

/// The raw material for `dvs-inspect`'s digest: what the compositor reported for every
/// frame that went into this file, what the mix measured, and which segments were reused.
fn digest_value(
    seq: &Sequence,
    range: Span,
    frames: i64,
    segments: &[Value],
    frame_reports: Vec<Value>,
    loudness: Option<Loudness>,
) -> Value {
    json!({
        "sequence": seq.id,
        "range": { "start": range.start, "end": range.end },
        "fps": seq.fps,
        "size": seq.size,
        "frameCount": frames,
        "segments": segments,
        "frames": frame_reports,
        "audio": loudness,
    })
}

/// Rename, falling back to copy when the output is on another filesystem.
fn move_file(from: &Path, to: &Path) -> Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(from, to).map_err(|e| Error::io(to, e))?;
            std::fs::remove_file(from).map_err(|e| Error::io(from, e))
        }
    }
}

/// Serial number for scratch files, so two renders in one process cannot collide.
static WORK_SERIAL: AtomicU64 = AtomicU64::new(0);

/// A scratch file under `cache/`, removed when it goes out of scope.
///
/// Renders write a joined video and a PCM bounce that exist only until the mux; leaving
/// them behind would grow the cache directory by the size of the output on every render.
struct WorkFile {
    path: PathBuf,
}

impl WorkFile {
    fn new(dir: &Path, suffix: &str) -> Result<WorkFile> {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        let serial = WORK_SERIAL.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("render-{}-{serial}.{suffix}", std::process::id()));
        Ok(WorkFile { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkFile {
    fn drop(&mut self) {
        // Already moved into place, or never created: either way there is nothing to report.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::ids::AssetId;
    use dvs_core::project::{Asset, AssetKind, Clip, Generator, Project, Source, Track, TrackKind};
    use dvs_core::vfs::FsVfs;
    use dvs_media::decode::synthesize;
    use dvs_media::probe::probe;
    use dvs_media::read_png;
    use std::sync::atomic::AtomicUsize;

    /// A project directory with a real ffmpeg behind it. Every test here renders actual
    /// video: a segment cache that is only exercised against a fake encoder proves nothing
    /// about whether `concat -c copy` accepted the chunks.
    struct Harness {
        _dir: tempfile::TempDir,
        workspace: Workspace,
        tool: &'static Toolchain,
        sequence: SequenceId,
    }

    fn fps30() -> Fps {
        Fps::new(30, 1).unwrap()
    }

    fn harness(fps: Fps) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::new(dir.path().join("project"));
        let project = Project::new("t", fps, [320, 180], 48_000);
        let sequence = project.active_sequence.clone();
        let workspace = Workspace::create(paths, project, FsVfs::shared()).unwrap();
        Harness {
            _dir: dir,
            workspace,
            tool: Toolchain::shared().expect("ffmpeg is required for the render tests"),
            sequence,
        }
    }

    /// Synthesize a clip with ffmpeg, import it, and register it in the document.
    fn import_media(harness: &mut Harness, seconds: i64, fps: Fps) -> AssetId {
        static SERIAL: AtomicUsize = AtomicUsize::new(0);
        let name = format!("src{}.mp4", SERIAL.fetch_add(1, Ordering::Relaxed));
        let source = harness._dir.path().join(&name);
        synthesize(
            harness.tool,
            &source,
            "testsrc2",
            Time::from_secs(seconds),
            fps,
            [320, 180],
        )
        .unwrap();
        let probed = probe(harness.tool, &source).unwrap();
        let hash = harness.workspace.assets.import_path(&source).unwrap();
        let id = AssetId::new();
        let imported = harness.workspace.project.created;
        harness.workspace.project.assets.insert(
            id.clone(),
            Asset {
                id: id.clone(),
                name,
                hash,
                kind: AssetKind::Video,
                probe: probed.probe,
                proxy: None,
                source_path: None,
                imported,
                provenance: None,
            },
        );
        id
    }

    fn push_track(harness: &mut Harness, kind: TrackKind, clips: Vec<Clip>) {
        let sequence = harness.sequence.clone();
        let seq = harness.workspace.project.sequence_mut(&sequence).unwrap();
        let name = seq.next_track_name(kind);
        let mut track = Track::new(name, kind);
        for clip in clips {
            track.place(clip);
        }
        seq.tracks.push(track);
    }


    /// Import a clip that has both picture and sound, which is what a camera or a screen
    /// recorder produces.
    fn import_media_with_sound(harness: &mut Harness, seconds: i64, fps: Fps) -> AssetId {
        static SERIAL: AtomicUsize = AtomicUsize::new(0);
        let name = format!("av{}.mp4", SERIAL.fetch_add(1, Ordering::Relaxed));
        let source = harness._dir.path().join(&name);
        let made = harness
            .tool
            .ffmpeg_command()
            .args([
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc2=size=320x180:rate={}:duration={seconds}", fps.ffmpeg_arg()),
                "-f",
                "lavfi",
                "-i",
                &format!("aevalsrc=0.4*sin(2*PI*220*t):s=48000:d={seconds}"),
            ])
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-shortest"])
            .arg(&source)
            .output()
            .unwrap();
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
        let probed = probe(harness.tool, &source).unwrap();
        assert!(probed.probe.audio.is_some(), "fixture must carry sound");
        let hash = harness.workspace.assets.import_path(&source).unwrap();
        let id = AssetId::new();
        let imported = harness.workspace.project.created;
        harness.workspace.project.assets.insert(
            id.clone(),
            Asset {
                id: id.clone(),
                name,
                hash,
                kind: AssetKind::Video,
                probe: probed.probe,
                proxy: None,
                source_path: None,
                imported,
                provenance: None,
            },
        );
        id
    }

    /// `n` consecutive `seconds`-long cuts of the same asset, which is the shape of a
    /// multi-segment timeline.
    fn cuts(asset: &AssetId, count: i64, seconds: i64) -> Vec<Clip> {
        (0..count)
            .map(|index| {
                Clip::new(
                    Source::Asset {
                        asset: asset.clone(),
                        stream: None,
                    },
                    Time::from_secs(index * seconds),
                    Time::from_secs(seconds),
                )
            })
            .collect()
    }

    /// x264 at its fastest: these tests are about scheduling and cache behaviour, and
    /// `medium` would spend the whole suite's time on rate control.
    fn spec(harness: &Harness, name: &str) -> RenderSpec {
        RenderSpec {
            preset: Some("ultrafast".to_string()),
            ..RenderSpec::new(
                harness.workspace.paths.root().join(name),
                harness.sequence.clone(),
            )
        }
    }

    fn probe_output(harness: &Harness, spec: &RenderSpec) -> dvs_core::project::Probe {
        probe(harness.tool, &spec.output).unwrap().probe
    }

    #[test]
    fn one_changed_clip_re_encodes_one_segment_and_reuses_the_rest() {
        let mut harness = harness(fps30());
        let asset = import_media(&mut harness, 2, fps30());
        push_track(&mut harness, TrackKind::Video, cuts(&asset, 3, 2));
        let spec = spec(&harness, "out.mp4");

        let first = render(&harness.workspace, harness.tool, &spec).unwrap();
        assert_eq!(first.segments_total, 3, "three cuts are three segments");
        assert_eq!(first.segments_reused, 0, "an empty cache reuses nothing");

        let sequence = harness.sequence.clone();
        harness
            .workspace
            .project
            .sequence_mut(&sequence)
            .unwrap()
            .tracks[0]
            .clips[1]
            .opacity = 0.5;

        let second = render(&harness.workspace, harness.tool, &spec).unwrap();
        assert_eq!(second.segments_total, 3);
        assert_eq!(
            second.segments_reused, 2,
            "changing the middle clip must re-encode exactly one segment"
        );

        let probed = probe_output(&harness, &spec);
        let drift = (probed.duration - Time::from_secs(6)).abs();
        assert!(
            drift <= fps30().frame_duration(),
            "the re-rendered file is {} long, not 6 s",
            probed.duration
        );
        assert_eq!(
            probed.video.and_then(|stream| stream.frames),
            Some(180),
            "a mixed reused/re-encoded render must still concatenate to every frame"
        );
    }

    #[test]
    fn no_cache_re_encodes_everything_even_when_the_keys_match() {
        let mut harness = harness(fps30());
        let asset = import_media(&mut harness, 2, fps30());
        push_track(&mut harness, TrackKind::Video, cuts(&asset, 2, 2));
        let warm = spec(&harness, "warm.mp4");
        let first = render(&harness.workspace, harness.tool, &warm).unwrap();
        assert_eq!(first.segments_reused, 0);

        let cached = render(&harness.workspace, harness.tool, &warm).unwrap();
        assert_eq!(
            cached.segments_reused, cached.segments_total,
            "an unchanged document must be a total cache hit"
        );

        let forced = RenderSpec {
            no_cache: true,
            ..spec(&harness, "forced.mp4")
        };
        let report = render(&harness.workspace, harness.tool, &forced).unwrap();
        assert_eq!(
            report.segments_reused, 0,
            "--no-cache must re-encode every segment"
        );
        assert!(forced.output.is_file());
    }

    #[test]
    fn the_rendered_file_matches_the_timeline_clock() {
        for fps in [fps30(), Fps::new(30_000, 1001).unwrap()] {
            let mut harness = harness(fps);
            let asset = import_media(&mut harness, 2, fps);
            push_track(&mut harness, TrackKind::Video, cuts(&asset, 1, 2));
            let spec = spec(&harness, "clock.mp4");
            let report = render(&harness.workspace, harness.tool, &spec).unwrap();

            let seq = harness.workspace.project.sequence(&harness.sequence).unwrap();
            let expected = seq.frame_count();
            assert_eq!(report.frames, expected, "at {fps}");

            let probed = probe_output(&harness, &spec);
            let stream = probed.video.expect("the output has a video stream");
            assert_eq!(stream.fps, fps, "the container must carry the exact rate");
            assert_eq!(
                stream.frames,
                Some(expected),
                "the file must hold every frame of the timeline at {fps}"
            );
            let drift = (probed.duration - Time::from_frames(expected, fps)).abs();
            assert!(
                drift <= fps.frame_duration(),
                "duration {} drifted more than a frame from the timeline at {fps}",
                probed.duration
            );
        }
    }

    #[test]
    fn rendering_a_range_renders_only_that_range() {
        let mut harness = harness(fps30());
        let asset = import_media(&mut harness, 2, fps30());
        push_track(&mut harness, TrackKind::Video, cuts(&asset, 3, 2));
        let spec = RenderSpec {
            range: Some(Span::new(Time::from_secs(2), Time::from_secs(4))),
            ..spec(&harness, "middle.mp4")
        };

        let report = render(&harness.workspace, harness.tool, &spec).unwrap();
        assert_eq!(report.frames, 60, "two seconds at 30 fps");
        assert_eq!(report.duration, Time::from_secs(2));

        let probed = probe_output(&harness, &spec);
        assert_eq!(
            probed.video.and_then(|stream| stream.frames),
            Some(60),
            "a ranged render must not contain the rest of the timeline"
        );
    }

    #[test]
    fn a_video_clip_with_sound_is_not_rendered_silent() {
        // The commonest timeline there is: one talking head on V1, no audio track at all.
        // Gating the mux on "does the sequence have an audio track" shipped a silent file
        // for exactly this case.
        let mut harness = harness(fps30());
        let asset = import_media_with_sound(&mut harness, 2, fps30());
        push_track(
            &mut harness,
            TrackKind::Video,
            vec![Clip::new(
                Source::Asset {
                    asset,
                    stream: None,
                },
                Time::ZERO,
                Time::from_secs(2),
            )],
        );
        let spec = spec(&harness, "talking-head.mp4");
        render(&harness.workspace, harness.tool, &spec).unwrap();
        let probed = probe_output(&harness, &spec);
        let audio = probed
            .audio
            .expect("a video clip's own sound must reach the output");
        assert_eq!(audio.rate, 48_000);
        assert_eq!(audio.channels, 2);
    }

    #[test]
    fn audio_reaches_the_output_without_costing_a_video_frame() {
        let mut harness = harness(fps30());
        let asset = import_media(&mut harness, 2, fps30());
        push_track(&mut harness, TrackKind::Video, cuts(&asset, 1, 2));
        push_track(
            &mut harness,
            TrackKind::Audio,
            vec![Clip::new(
                Source::Generator {
                    generator: Generator::Tone,
                    params: serde_json::Map::new(),
                },
                Time::ZERO,
                Time::from_secs(2),
            )],
        );
        let spec = RenderSpec {
            with_digest: true,
            ..spec(&harness, "sound.mp4")
        };
        let report = render(&harness.workspace, harness.tool, &spec).unwrap();

        let probed = probe_output(&harness, &spec);
        let audio = probed.audio.expect("the muxed file must carry the mix");
        assert_eq!(audio.rate, 48_000, "the sequence's sample rate");
        assert_eq!(audio.channels, 2, "the sequence's channel count");
        assert_eq!(
            probed.video.and_then(|stream| stream.frames),
            Some(60),
            "muxing the mix in must not cost the last video frame"
        );

        let loudness = report.digest.expect("--digest was requested")["audio"].clone();
        let lufs = loudness["integratedLufs"]
            .as_f64()
            .unwrap_or_else(|| panic!("the mix must be measured, got {loudness}"));
        assert!(
            lufs.is_finite() && lufs < 0.0,
            "a 1 kHz tone must measure a real programme loudness, got {lufs} LUFS"
        );
    }

    #[test]
    fn the_report_carries_provenance_and_a_digest_on_request() {
        let mut harness = harness(fps30());
        let asset = import_media(&mut harness, 2, fps30());
        push_track(&mut harness, TrackKind::Video, cuts(&asset, 1, 2));
        let spec = RenderSpec {
            with_digest: true,
            ..spec(&harness, "digest.mp4")
        };
        let report = render(&harness.workspace, harness.tool, &spec).unwrap();

        assert_eq!(report.tools.engine.as_deref(), Some(ENGINE_VERSION));
        assert_eq!(report.tools.ffmpeg.as_deref(), Some(harness.tool.version()));
        assert!(
            report.tools.encoders.contains(&"libx264".to_string()),
            "provenance must name the encoder that produced the bytes: {:?}",
            report.tools.encoders
        );

        let digest = report.digest.expect("--digest was requested");
        let frames = digest["frames"].as_array().expect("per-frame reports");
        assert_eq!(
            frames.len() as i64,
            report.frames,
            "the digest must describe every frame that was encoded"
        );
        assert_eq!(frames[0]["layers"][0]["source"], json!(asset.to_string()));
    }

    #[test]
    fn a_reused_segment_still_reports_its_frames() {
        let mut harness = harness(fps30());
        let asset = import_media(&mut harness, 2, fps30());
        push_track(&mut harness, TrackKind::Video, cuts(&asset, 2, 2));
        let spec = RenderSpec {
            with_digest: true,
            ..spec(&harness, "again.mp4")
        };
        let first = render(&harness.workspace, harness.tool, &spec).unwrap();
        let second = render(&harness.workspace, harness.tool, &spec).unwrap();

        assert_eq!(second.segments_reused, second.segments_total);
        assert!(second.warnings.is_empty(), "{:?}", second.warnings);
        let frames = |report: &RenderReport| {
            report.digest.as_ref().unwrap()["frames"]
                .as_array()
                .unwrap()
                .len()
        };
        assert_eq!(
            frames(&second),
            frames(&first),
            "a cache hit must not blank out the digest for those frames"
        );
    }

    #[test]
    fn render_frame_writes_the_frame_the_compositor_composed() {
        let mut harness = harness(fps30());
        let asset = import_media(&mut harness, 2, fps30());
        push_track(&mut harness, TrackKind::Video, cuts(&asset, 1, 2));
        let output = harness.workspace.paths.root().join("frame.png");
        let at = Time::new(1, 1).unwrap();
        render_frame(
            &harness.workspace,
            harness.tool,
            &harness.sequence,
            at,
            1.0,
            false,
            &output,
        )
        .unwrap();

        let mut comp = Compositor::new(
            harness.tool,
            &harness.workspace.project,
            &harness.workspace.paths,
            &harness.workspace.assets,
            &harness.sequence,
            CompOptions::default(),
        )
        .unwrap();
        let expected = comp.frame(at.frame_floor(fps30())).unwrap().to_rgba8();
        let written = read_png(harness.tool, &output).unwrap().to_rgba8();

        assert_eq!(written.len(), expected.len(), "same raster size");
        let worst = written
            .iter()
            .zip(&expected)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0);
        assert!(
            worst <= 1,
            "the PNG differs from the composited frame by {worst}/255"
        );
    }

    #[test]
    fn preview_frames_samples_on_the_requested_interval() {
        let mut harness = harness(fps30());
        let asset = import_media(&mut harness, 2, fps30());
        push_track(&mut harness, TrackKind::Video, cuts(&asset, 2, 2));

        let frames = preview_frames(
            &harness.workspace,
            harness.tool,
            &harness.sequence,
            Span::new(Time::ZERO, Time::from_secs(4)),
            Time::from_secs(1),
            0.5,
        )
        .unwrap();

        let times: Vec<Time> = frames.iter().map(|(at, _)| *at).collect();
        assert_eq!(
            times,
            (0..4).map(Time::from_secs).collect::<Vec<_>>(),
            "one sample per second across the range"
        );
        assert_eq!(
            frames[0].1.size(),
            [160, 90],
            "the sheet renders at the requested scale"
        );
    }

    #[test]
    fn plan_keys_matches_what_the_cache_holds_after_a_render() {
        let mut harness = harness(fps30());
        let asset = import_media(&mut harness, 2, fps30());
        push_track(&mut harness, TrackKind::Video, cuts(&asset, 2, 2));
        let spec = spec(&harness, "keys.mp4");

        let keys = plan_keys(&harness.workspace, harness.tool, &spec).unwrap();
        let before = cache::status(&harness.workspace.paths, &keys).unwrap();
        assert_eq!(before.present, 0, "nothing is cached yet");
        assert_eq!(before.missing.len(), keys.len());

        render(&harness.workspace, harness.tool, &spec).unwrap();
        let after = cache::status(&harness.workspace.paths, &keys).unwrap();
        assert_eq!(
            after.present,
            keys.len(),
            "every planned segment must be on disk after a render"
        );
        assert!(after.stale.is_empty(), "{:?}", after.stale);
        assert_eq!(
            cache::prune(&harness.workspace.paths, &keys).unwrap(),
            0,
            "pruning to the live key set must not delete a live chunk"
        );
    }
}
