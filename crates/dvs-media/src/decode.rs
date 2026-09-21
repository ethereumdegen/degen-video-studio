//! Decoding: one ffmpeg process per open source, read sequentially.
//!
//! The access pattern of a renderer is "give me every frame in order", and the access
//! pattern of a preview is "give me a frame near here". Both are served by a single
//! streaming process plus a rule for when to restart it:
//!
//! - forward, close: read and discard the frames in between (cheaper than a seek, which
//!   would decode from the previous keyframe anyway);
//! - backward, or far forward: respawn with `-ss`, which seeks to a keyframe and discards
//!   up to the requested timestamp.
//!
//! The filter chain is built from the probe, never left to defaults: `fps` makes the output
//! constant-rate at the sequence rate, `scale` states the input range and matrix
//! explicitly, and the output is full-range RGBA. A source whose color tags we guess wrong
//! is the difference between correct and washed-out, and it fails silently, so the values
//! are always passed.

use crate::frame::Frame;
use crate::toolchain::Toolchain;
use dvs_core::error::{Error, Result};
use dvs_core::project::{ColorMatrix, ColorRange, VideoStream};
use dvs_core::time::{Fps, Span, Time};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

/// Reading forward this many frames is cheaper than paying for a keyframe seek plus the
/// decode-and-discard that follows it.
const FORWARD_SCAN_LIMIT: i64 = 48;

#[derive(Debug, Clone)]
pub struct DecodeSpec {
    /// Output raster size. The renderer asks for the size it will composite at, so scaling
    /// happens once, inside ffmpeg's SIMD scaler, rather than twice.
    pub size: [u32; 2],
    /// Output frame rate. Constant by construction.
    pub fps: Fps,
    pub range: ColorRange,
    pub matrix: ColorMatrix,
    /// Scaler: `bicubic` for stills and downscales, `bilinear` for speed in previews.
    pub scaler: &'static str,
}

impl DecodeSpec {
    /// Decode a stream at its display size and a given output rate.
    pub fn for_stream(stream: &VideoStream, fps: Fps) -> DecodeSpec {
        DecodeSpec {
            size: stream.display_size(),
            fps,
            range: stream.color_range,
            matrix: stream.color_matrix,
            scaler: "bicubic",
        }
    }

    pub fn at_size(mut self, size: [u32; 2]) -> DecodeSpec {
        self.size = size;
        self
    }

    fn filter_chain(&self) -> String {
        // An unflagged source is BT.709 limited if it is HD, BT.601 limited otherwise —
        // the same assumption every player makes. It is recorded as a guess in the probe
        // (`Unknown`) and applied here so the decode is at least deterministic.
        let matrix = match self.matrix {
            ColorMatrix::Bt709 => "bt709",
            ColorMatrix::Bt601 => "bt601",
            ColorMatrix::Bt2020Ncl => "bt2020ncl",
            ColorMatrix::Smpte240m => "smpte240m",
            ColorMatrix::Unknown => {
                if self.size[1] >= 720 {
                    "bt709"
                } else {
                    "bt601"
                }
            }
        };
        let range = match self.range {
            ColorRange::Pc => "full",
            // Limited is the overwhelming default for camera and screen-capture output.
            ColorRange::Tv | ColorRange::Unknown => "limited",
        };
        format!(
            "fps={fps},scale={w}:{h}:flags={flags}:in_range={range}:in_color_matrix={matrix}:out_range=full,format=rgba",
            fps = self.fps.ffmpeg_arg(),
            w = self.size[0],
            h = self.size[1],
            flags = self.scaler,
            range = range,
            matrix = matrix,
        )
    }
}

/// A sequential RGBA reader over one video source.
pub struct VideoDecoder<'a> {
    tool: &'a Toolchain,
    path: PathBuf,
    spec: DecodeSpec,
    process: Option<Running>,
    /// Source time of the first frame the current process emits.
    origin: Time,
    /// Frames read from the current process.
    emitted: i64,
    /// Last frame emitted by the *current* process.
    last: Option<Frame>,
    /// Last frame emitted by any process for this source, kept across respawns.
    held: Option<Frame>,
    exhausted: bool,
    /// Counts respawns, which is the metric that tells us whether an access pattern is
    /// pathological. The render loop logs it.
    seeks: u32,
}

struct Running {
    child: Child,
    stdout: ChildStdout,
}

impl Drop for Running {
    fn drop(&mut self) {
        // The child is blocked writing into a pipe nobody reads; killing it is the only
        // way to avoid leaking an ffmpeg per clip for the life of the process.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl<'a> VideoDecoder<'a> {
    pub fn open(tool: &'a Toolchain, path: impl Into<PathBuf>, spec: DecodeSpec) -> VideoDecoder<'a> {
        VideoDecoder {
            tool,
            path: path.into(),
            spec,
            process: None,
            origin: Time::ZERO,
            emitted: 0,
            last: None,
            held: None,
            exhausted: false,
            seeks: 0,
        }
    }

    pub fn size(&self) -> [u32; 2] {
        self.spec.size
    }

    pub fn seek_count(&self) -> u32 {
        self.seeks
    }

    fn frame_bytes(&self) -> usize {
        (self.spec.size[0] as usize) * (self.spec.size[1] as usize) * 4
    }

    /// Index of the frame the current process is about to emit, in "frames since origin".
    fn wanted_index(&self, source_time: Time) -> i64 {
        (source_time - self.origin).frame_floor(self.spec.fps)
    }

    /// The frame covering `source_time`.
    ///
    /// Past the end of the source the last decoded frame is held rather than returning
    /// black: a clip trimmed one frame long is a rounding artifact, and a black flash is
    /// both worse and harder to diagnose than a repeated frame. The renderer reports the
    /// condition through the `past-source-end` warning instead.
    pub fn frame_at(&mut self, source_time: Time) -> Result<Frame> {
        let source_time = source_time.max(Time::ZERO);
        let mut wanted = self.wanted_index(source_time);
        if self.process.is_none() || wanted < self.emitted - 1 || wanted > self.emitted + FORWARD_SCAN_LIMIT
        {
            self.spawn(source_time)?;
            wanted = 0;
        }
        if self.emitted > 0 && wanted == self.emitted - 1 {
            if let Some(frame) = &self.last {
                return Ok(frame.clone());
            }
        }
        while self.emitted <= wanted {
            match self.read_one()? {
                Some(frame) => {
                    self.held = Some(frame.clone());
                    self.last = Some(frame);
                    self.emitted += 1;
                }
                None => {
                    self.exhausted = true;
                    break;
                }
            }
        }
        // `last` belongs to the current process and is cleared by a respawn; `held` survives
        // it. A seek past the end of the source therefore still returns the final picture
        // rather than a black frame, which is what a clip trimmed one frame long needs.
        match self.last.as_ref().or(self.held.as_ref()) {
            Some(frame) => Ok(frame.clone()),
            None => Ok(Frame::transparent(self.spec.size[0], self.spec.size[1])),
        }
    }

    /// Whether the source ran out during the last read.
    pub fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    fn spawn(&mut self, at: Time) -> Result<()> {
        drop(self.process.take());
        // Align the seek to the output frame grid so index arithmetic after the seek is
        // exact rather than drifting by a sub-frame offset.
        let aligned = Time::from_frames(at.frame_floor(self.spec.fps), self.spec.fps);
        let mut command = self.tool.ffmpeg_command();
        if aligned.is_positive() {
            command.args(["-ss", &format!("{:.6}", aligned.as_secs_f64())]);
        }
        command
            .arg("-i")
            .arg(&self.path)
            .args(["-an", "-sn", "-dn"])
            .args(["-vf", &self.spec.filter_chain()])
            .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|e| Error::tool("ffmpeg", format!("cannot start decoder: {e}")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::tool("ffmpeg", "decoder produced no stdout"))?;
        self.process = Some(Running { child, stdout });
        self.origin = aligned;
        self.emitted = 0;
        self.last = None;
        self.exhausted = false;
        self.seeks += 1;
        Ok(())
    }

    fn read_one(&mut self) -> Result<Option<Frame>> {
        let bytes = self.frame_bytes();
        let Some(process) = self.process.as_mut() else {
            return Ok(None);
        };
        let mut buffer = vec![0u8; bytes];
        let mut filled = 0usize;
        while filled < bytes {
            match process.stdout.read(&mut buffer[filled..]) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) => {
                    return Err(Error::tool(
                        "ffmpeg",
                        format!("decoder read failed: {error}"),
                    ))
                }
            }
        }
        if filled == 0 {
            // Clean end of stream: collect the child so a decode error surfaces here
            // rather than as a silent black frame.
            let status = process
                .child
                .wait()
                .map_err(|e| Error::tool("ffmpeg", e.to_string()))?;
            if !status.success() {
                let mut stderr = String::new();
                if let Some(mut pipe) = process.child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                return Err(Error::tool(
                    "ffmpeg",
                    format!(
                        "decoding '{}' failed: {}",
                        self.path.display(),
                        stderr.trim()
                    ),
                ));
            }
            return Ok(None);
        }
        if filled < bytes {
            return Err(Error::tool(
                "ffmpeg",
                format!(
                    "decoder returned a partial frame ({filled} of {bytes} bytes) for '{}'",
                    self.path.display()
                ),
            ));
        }
        Ok(Some(Frame::from_rgba8(
            self.spec.size[0],
            self.spec.size[1],
            &buffer,
        )))
    }
}

/// Decode one image file (PNG, JPEG, WebP) to a frame at its native size.
pub fn decode_image(tool: &Toolchain, path: &Path) -> Result<Frame> {
    let output = tool
        .ffmpeg_command()
        .arg("-i")
        .arg(path)
        .args(["-vframes", "1", "-vf", "format=rgba", "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| Error::tool("ffmpeg", e.to_string()))?;
    if !output.status.success() {
        return Err(Error::tool(
            "ffmpeg",
            format!(
                "cannot decode image '{}': {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    // The raster size is not in the raw stream, so it comes from the probe.
    let probed = crate::probe::probe(tool, path)?;
    let stream = probed
        .probe
        .video
        .ok_or_else(|| Error::tool("ffmpeg", format!("'{}' has no image stream", path.display())))?;
    let [width, height] = stream.display_size();
    let expected = (width as usize) * (height as usize) * 4;
    if output.stdout.len() < expected {
        return Err(Error::tool(
            "ffmpeg",
            format!(
                "image '{}' decoded to {} bytes, expected {expected}",
                path.display(),
                output.stdout.len()
            ),
        ));
    }
    Ok(Frame::from_rgba8(width, height, &output.stdout))
}

/// Interleaved f32 PCM for a span of source time, resampled to the sequence rate.
///
/// Audio is decoded per span rather than streamed because the mixer needs random access
/// (a clip can appear anywhere on the timeline) and because f32 at 48 kHz stereo is 384 kB
/// per second — a whole ten-minute source is 230 MB, so spans stay bounded.
pub fn decode_audio(
    tool: &Toolchain,
    path: &Path,
    span: Span,
    rate: u32,
    channels: u16,
) -> Result<Vec<f32>> {
    if span.is_empty() {
        return Ok(Vec::new());
    }
    let output = tool
        .ffmpeg_command()
        .args(["-ss", &format!("{:.6}", span.start.as_secs_f64())])
        .arg("-i")
        .arg(path)
        .args(["-t", &format!("{:.6}", span.duration().as_secs_f64())])
        .args(["-vn", "-sn", "-dn"])
        .args(["-ar", &rate.to_string(), "-ac", &channels.to_string()])
        .args(["-f", "f32le", "-acodec", "pcm_f32le", "-"])
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| Error::tool("ffmpeg", e.to_string()))?;
    if !output.status.success() {
        return Err(Error::tool(
            "ffmpeg",
            format!(
                "cannot decode audio from '{}': {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    let mut samples = Vec::with_capacity(output.stdout.len() / 4);
    for chunk in output.stdout.chunks_exact(4) {
        samples.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    // ffmpeg's `-t` is honoured to the sample, but a source that ends early returns short;
    // the mixer pads, because a short buffer would otherwise shift everything after it.
    let wanted = span.duration().sample_round(rate) as usize * channels as usize;
    if samples.len() < wanted {
        samples.resize(wanted, 0.0);
    } else {
        samples.truncate(wanted);
    }
    Ok(samples)
}

/// Build a synthetic test source. Used by the test suites in this workspace and by
/// `dvs asset import --generate`, which is how a user makes a slate without finding media.
pub fn synthesize(
    tool: &Toolchain,
    path: &Path,
    source: &str,
    duration: Time,
    fps: Fps,
    size: [u32; 2],
) -> Result<()> {
    let mut command = tool.ffmpeg_command();
    command.args([
        "-f",
        "lavfi",
        "-i",
        &format!(
            "{source}=size={}x{}:rate={}:duration={:.4}",
            size[0],
            size[1],
            fps.ffmpeg_arg(),
            duration.as_secs_f64()
        ),
    ]);
    let status = command
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-crf", "18", "-pix_fmt", "yuv420p"])
        .arg(path)
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| Error::tool("ffmpeg", e.to_string()))?;
    if !status.status.success() {
        return Err(Error::tool(
            "ffmpeg",
            format!(
                "cannot synthesize '{source}': {}",
                String::from_utf8_lossy(&status.stderr).trim()
            ),
        ));
    }
    Ok(())
}

/// Extra ffmpeg invocation helper for callers that need a raw command with the standard
/// flags already applied.
pub fn command(tool: &Toolchain) -> Command {
    tool.ffmpeg_command()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::project::{ColorMatrix, ColorRange};

    fn spec(size: [u32; 2], matrix: ColorMatrix, range: ColorRange) -> DecodeSpec {
        DecodeSpec {
            size,
            fps: Fps::new(30, 1).unwrap(),
            range,
            matrix,
            scaler: "bicubic",
        }
    }

    #[test]
    fn filter_chain_states_range_and_matrix_explicitly() {
        let chain = spec([1920, 1080], ColorMatrix::Bt709, ColorRange::Tv).filter_chain();
        assert!(chain.contains("in_range=limited"), "{chain}");
        assert!(chain.contains("in_color_matrix=bt709"), "{chain}");
        assert!(chain.contains("out_range=full"), "{chain}");
        assert!(chain.starts_with("fps=30/1,"), "{chain}");
    }

    #[test]
    fn unflagged_sources_fall_back_by_resolution_not_to_nothing() {
        let hd = spec([1920, 1080], ColorMatrix::Unknown, ColorRange::Unknown).filter_chain();
        let sd = spec([640, 480], ColorMatrix::Unknown, ColorRange::Unknown).filter_chain();
        assert!(hd.contains("in_color_matrix=bt709"), "{hd}");
        assert!(sd.contains("in_color_matrix=bt601"), "{sd}");
    }

    #[test]
    fn decoding_a_synthetic_source_yields_frame_exact_content() {
        let tool = Toolchain::discover().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bars.mp4");
        let fps = Fps::new(30, 1).unwrap();
        synthesize(&tool, &path, "testsrc2", Time::from_secs(2), fps, [320, 240]).unwrap();

        let mut decoder = VideoDecoder::open(
            &tool,
            &path,
            DecodeSpec {
                size: [320, 240],
                fps,
                range: ColorRange::Tv,
                matrix: ColorMatrix::Bt601,
                scaler: "bilinear",
            },
        );
        let first = decoder.frame_at(Time::ZERO).unwrap();
        assert_eq!(first.size(), [320, 240]);
        assert!(first.alpha_coverage() > 0.99, "decoded frame is transparent");

        // testsrc2 animates, so two different instants must differ. A decoder that ignored
        // the requested time would return identical frames here.
        let later = decoder.frame_at(Time::new(3, 2).unwrap()).unwrap();
        assert_ne!(first.pixels(), later.pixels());

        // Going backwards must respawn and still land on the first frame's content.
        let seeks_before = decoder.seek_count();
        let again = decoder.frame_at(Time::ZERO).unwrap();
        assert!(decoder.seek_count() > seeks_before, "backward seek did not respawn");
        assert_eq!(again.pixels(), first.pixels());
    }

    #[test]
    fn reading_past_the_end_holds_the_last_frame() {
        let tool = Toolchain::discover().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.mp4");
        let fps = Fps::new(30, 1).unwrap();
        synthesize(&tool, &path, "testsrc2", Time::from_secs(1), fps, [160, 120]).unwrap();

        let mut decoder = VideoDecoder::open(
            &tool,
            &path,
            DecodeSpec {
                size: [160, 120],
                fps,
                range: ColorRange::Tv,
                matrix: ColorMatrix::Bt601,
                scaler: "bilinear",
            },
        );
        let last = decoder.frame_at(Time::new(29, 30).unwrap()).unwrap();
        let past = decoder.frame_at(Time::from_secs(5)).unwrap();
        assert!(decoder.is_exhausted());
        assert_eq!(past.pixels(), last.pixels());
    }

    #[test]
    fn audio_decode_returns_exactly_the_requested_sample_count() {
        let tool = Toolchain::discover().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        // `aevalsrc` with an explicit amplitude rather than `sine`: ffmpeg's sine source
        // generates at 1/8 scale (peak 4095 in s16), so a level assertion against it would
        // be testing ffmpeg's default gain instead of our decode path.
        let status = tool
            .ffmpeg_command()
            .args([
                "-f",
                "lavfi",
                "-i",
                "aevalsrc=0.9*sin(2*PI*1000*t):s=48000:d=2",
            ])
            .arg(&path)
            .output()
            .unwrap();
        assert!(status.status.success());

        let span = Span::new(Time::new(1, 2).unwrap(), Time::new(3, 2).unwrap());
        let samples = decode_audio(&tool, &path, span, 48_000, 2).unwrap();
        assert_eq!(samples.len(), 48_000 * 2, "one second of stereo at 48 kHz");
        let peak = samples.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
        // ffmpeg upmixes mono to stereo at constant power, so each channel carries
        // amplitude/√2 — 0.636 here, not 0.9. Asserting the naive value would have us
        // "fix" a correct downstream mix later.
        let expected = 0.9 / 2f32.sqrt();
        assert!(
            (peak - expected).abs() < 0.02,
            "expected ~{expected} after the constant-power upmix, got {peak}"
        );

        // Past the end pads with silence rather than returning short, which would shift
        // everything mixed after it.
        let tail = Span::new(Time::new(19, 10).unwrap(), Time::new(29, 10).unwrap());
        let padded = decode_audio(&tool, &path, tail, 48_000, 2).unwrap();
        assert_eq!(padded.len(), 48_000 * 2);
        assert_eq!(padded[padded.len() - 2..], [0.0, 0.0]);
    }
}
