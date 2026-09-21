//! Encoding: frames in over a pipe, a container out.
//!
//! The compositor produces premultiplied linear f32; encoders want 8-bit limited-range
//! YUV. That conversion happens exactly once, here, and every color flag is stated rather
//! than defaulted, so a file we write is tagged with the same primaries it was encoded
//! against. Untagged output is the reason a video looks right in one player and washed out
//! in another.
//!
//! Hardware encoders are opt-in per platform (`videotoolbox` on macOS, `nvenc`/`vaapi` on
//! Linux) because their rate control differs from x264's and because their availability is
//! a property of the machine, not the project. `Encoder::Auto` resolves to the best
//! *software* encoder present, which is reproducible everywhere.

use crate::frame::Frame;
use crate::toolchain::Toolchain;
use dvs_core::color::Rgba;
use dvs_core::error::{Error, Result};
use dvs_core::time::Fps;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Encoder {
    /// The default: `libx264`, which exists on every install and whose output is
    /// comparable across machines.
    Auto,
    X264,
    X265,
    Svtav1,
    Nvenc,
    Vaapi,
    VideoToolbox,
    ProRes,
    /// Anything else the local ffmpeg has, by encoder name.
    Named(String),
}

impl Encoder {
    pub fn parse(text: &str) -> Encoder {
        match text.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Encoder::Auto,
            "x264" | "h264" | "libx264" => Encoder::X264,
            "x265" | "hevc" | "libx265" => Encoder::X265,
            "av1" | "svtav1" | "libsvtav1" => Encoder::Svtav1,
            "nvenc" | "h264_nvenc" => Encoder::Nvenc,
            "vaapi" | "h264_vaapi" => Encoder::Vaapi,
            "videotoolbox" | "vt" | "h264_videotoolbox" => Encoder::VideoToolbox,
            "prores" | "prores_ks" => Encoder::ProRes,
            other => Encoder::Named(other.to_string()),
        }
    }

    /// The ffmpeg encoder name, checked against what this build actually has. A request
    /// for an encoder the machine lacks is an error naming the alternatives, not a silent
    /// fallback — a silent fallback changes file size and quality without telling anyone.
    pub fn resolve(&self, tool: &Toolchain) -> Result<String> {
        let wanted = match self {
            Encoder::Auto | Encoder::X264 => "libx264",
            Encoder::X265 => "libx265",
            Encoder::Svtav1 => "libsvtav1",
            Encoder::Nvenc => "h264_nvenc",
            Encoder::Vaapi => "h264_vaapi",
            Encoder::VideoToolbox => "h264_videotoolbox",
            Encoder::ProRes => "prores_ks",
            Encoder::Named(name) => name.as_str(),
        };
        if tool.has_encoder(wanted) {
            return Ok(wanted.to_string());
        }
        Err(Error::tool(
            "ffmpeg",
            format!(
                "encoder '{wanted}' is not in this ffmpeg build; available: {}",
                available(tool).join(", ")
            ),
        ))
    }

    /// Quality flags differ per encoder family: CRF for x264/x265, `-cq` for NVENC, `-q:v`
    /// for VideoToolbox and ProRes profiles for ProRes.
    fn quality_args(&self, quality: u32) -> Vec<String> {
        match self {
            Encoder::Auto | Encoder::X264 | Encoder::X265 | Encoder::Svtav1 => {
                vec!["-crf".into(), quality.to_string()]
            }
            Encoder::Nvenc => vec![
                "-rc".into(),
                "vbr".into(),
                "-cq".into(),
                quality.to_string(),
            ],
            Encoder::Vaapi => vec!["-qp".into(), quality.to_string()],
            Encoder::VideoToolbox => vec!["-q:v".into(), (quality.min(100)).to_string()],
            Encoder::ProRes => vec!["-profile:v".into(), "3".into()],
            Encoder::Named(_) => vec!["-crf".into(), quality.to_string()],
        }
    }

    fn preset_args(&self, preset: Option<&str>) -> Vec<String> {
        match self {
            Encoder::Auto | Encoder::X264 | Encoder::X265 => {
                vec!["-preset".into(), preset.unwrap_or("medium").to_string()]
            }
            Encoder::Svtav1 => vec!["-preset".into(), preset.unwrap_or("8").to_string()],
            Encoder::Nvenc => vec!["-preset".into(), preset.unwrap_or("p5").to_string()],
            _ => Vec::new(),
        }
    }
}

fn available(tool: &Toolchain) -> Vec<String> {
    crate::toolchain::SW_ENCODERS
        .iter()
        .chain(crate::toolchain::HW_ENCODERS)
        .filter(|name| tool.has_encoder(name))
        .map(|name| name.to_string())
        .collect()
}

#[derive(Debug, Clone)]
pub struct AudioSpec {
    /// Raw interleaved f32 PCM on disk, written by the mixer.
    pub pcm: PathBuf,
    pub rate: u32,
    pub channels: u16,
    /// `aac`, `libopus`, `pcm_s16le`.
    pub codec: String,
    pub bitrate: String,
}

#[derive(Debug, Clone)]
pub struct EncodeSpec {
    pub output: PathBuf,
    pub size: [u32; 2],
    pub fps: Fps,
    pub encoder: Encoder,
    /// CRF-like quality; 18 is the project default.
    pub quality: u32,
    pub preset: Option<String>,
    pub audio: Option<AudioSpec>,
    /// What transparent pixels resolve to. Sequence background, black by default.
    pub background: Rgba,
    /// Force every frame to be a keyframe boundary candidate at segment starts, so
    /// concatenating segments with `-c copy` is valid.
    pub closed_gop: bool,
    pub extra: Vec<String>,
}

impl EncodeSpec {
    pub fn new(output: impl Into<PathBuf>, size: [u32; 2], fps: Fps) -> EncodeSpec {
        EncodeSpec {
            output: output.into(),
            size,
            fps,
            encoder: Encoder::Auto,
            quality: 18,
            preset: None,
            audio: None,
            background: Rgba::BLACK,
            closed_gop: false,
            extra: Vec::new(),
        }
    }
}

/// A running encoder. Frames are written in order; `finish` is the only way to get a
/// complete file, and it reports ffmpeg's stderr on failure instead of leaving a truncated
/// container behind.
pub struct EncoderSession {
    child: Child,
    stdin: Option<ChildStdin>,
    output: PathBuf,
    background: Rgba,
    size: [u32; 2],
    written: u64,
}

impl EncoderSession {
    pub fn start(tool: &Toolchain, spec: &EncodeSpec) -> Result<EncoderSession> {
        let encoder = spec.encoder.resolve(tool)?;
        let mut command = tool.ffmpeg_command();
        command
            .args(["-f", "rawvideo", "-pix_fmt", "rgb24"])
            .args(["-s", &format!("{}x{}", spec.size[0], spec.size[1])])
            .args(["-r", &spec.fps.ffmpeg_arg()])
            .args(["-i", "-"]);
        if let Some(audio) = &spec.audio {
            command
                .args(["-f", "f32le", "-ar", &audio.rate.to_string()])
                .args(["-ac", &audio.channels.to_string()])
                .arg("-i")
                .arg(&audio.pcm);
        }
        command.args(["-c:v", encoder.as_str()]);
        command.args(spec.encoder.preset_args(spec.preset.as_deref()));
        command.args(spec.encoder.quality_args(spec.quality));
        // Tag and convert in one place. `out_range=tv` plus matching metadata is what makes
        // the file display identically in ffplay, a browser and QuickTime.
        command
            .args([
                "-vf",
                "scale=out_range=tv:out_color_matrix=bt709:flags=bicubic",
            ])
            .args(["-pix_fmt", pixel_format(&encoder)])
            .args(["-colorspace", "bt709", "-color_primaries", "bt709"])
            .args(["-color_trc", "bt709", "-color_range", "tv"]);
        if spec.closed_gop {
            // Segment concatenation with `-c copy` requires each segment to start with a
            // keyframe and not reference across its boundary.
            command.args(["-g", "60", "-flags", "+cgop", "-sc_threshold", "0"]);
        }
        if let Some(audio) = &spec.audio {
            command
                .args(["-c:a", &audio.codec])
                .args(["-b:a", &audio.bitrate])
                .args(["-shortest"]);
        }
        command.args(&spec.extra);
        if spec
            .output
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4") || ext.eq_ignore_ascii_case("mov"))
        {
            // Without this a consumer must download the whole file before it can start.
            command.args(["-movflags", "+faststart"]);
        }
        command
            .arg(&spec.output)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| Error::tool("ffmpeg", format!("cannot start encoder: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::tool("ffmpeg", "encoder has no stdin"))?;
        Ok(EncoderSession {
            child,
            stdin: Some(stdin),
            output: spec.output.clone(),
            background: spec.background,
            size: spec.size,
            written: 0,
        })
    }

    pub fn write(&mut self, frame: &Frame) -> Result<()> {
        if frame.size() != self.size {
            return Err(Error::op(format!(
                "encoder expects {}x{} frames, got {}x{}",
                self.size[0],
                self.size[1],
                frame.width(),
                frame.height()
            )));
        }
        let bytes = frame.to_rgb8_over(self.background);
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| Error::op("encoder already finished"))?;
        match stdin.write_all(&bytes) {
            Ok(()) => {
                self.written += 1;
                Ok(())
            }
            Err(error) => {
                // A broken pipe means ffmpeg died; its stderr is the real diagnosis.
                Err(self.fail(format!("writing frame {}: {error}", self.written)))
            }
        }
    }

    pub fn frames_written(&self) -> u64 {
        self.written
    }

    pub fn finish(mut self) -> Result<PathBuf> {
        drop(self.stdin.take());
        let status = self
            .child
            .wait()
            .map_err(|e| Error::tool("ffmpeg", e.to_string()))?;
        if !status.success() {
            let mut stderr = String::new();
            if let Some(mut pipe) = self.child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            return Err(Error::tool(
                "ffmpeg",
                format!(
                    "encoding '{}' failed after {} frames: {}",
                    self.output.display(),
                    self.written,
                    stderr.trim()
                ),
            ));
        }
        Ok(self.output)
    }

    fn fail(&mut self, context: String) -> Error {
        let mut stderr = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        Error::tool("ffmpeg", format!("{context}; ffmpeg said: {}", stderr.trim()))
    }
}

/// ProRes needs a 10-bit 422 format; everything else here is 8-bit 420 for compatibility.
fn pixel_format(encoder: &str) -> &'static str {
    match encoder {
        "prores_ks" | "prores_videotoolbox" => "yuv422p10le",
        _ => "yuv420p",
    }
}

/// Concatenate encoded segments without re-encoding. This is what makes the segment cache
/// worth having: an unchanged segment is copied, not re-compressed.
pub fn concat(tool: &Toolchain, segments: &[PathBuf], output: &Path) -> Result<()> {
    if segments.is_empty() {
        return Err(Error::op("nothing to concatenate"));
    }
    let list = segments
        .iter()
        .map(|path| {
            let absolute = path
                .canonicalize()
                .unwrap_or_else(|_| path.clone())
                .display()
                .to_string();
            // The concat demuxer takes a quoted path with single quotes escaped.
            format!("file '{}'", absolute.replace('\'', r"'\''"))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut list_file = tempfile::Builder::new()
        .prefix("dvs-concat-")
        .suffix(".txt")
        .tempfile()
        .map_err(|e| Error::io("concat list", e))?;
    list_file
        .write_all(list.as_bytes())
        .map_err(|e| Error::io("concat list", e))?;
    let output_result = tool
        .ffmpeg_command()
        .args(["-f", "concat", "-safe", "0"])
        .arg("-i")
        .arg(list_file.path())
        .args(["-c", "copy"])
        .arg(output)
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| Error::tool("ffmpeg", e.to_string()))?;
    if !output_result.status.success() {
        return Err(Error::tool(
            "ffmpeg",
            format!(
                "concatenating {} segment(s) failed: {}",
                segments.len(),
                String::from_utf8_lossy(&output_result.stderr).trim()
            ),
        ));
    }
    Ok(())
}

/// Mux a video file and a raw PCM track into the final container.
pub fn mux_audio(
    tool: &Toolchain,
    video: &Path,
    audio: &AudioSpec,
    output: &Path,
) -> Result<()> {
    let result = tool
        .ffmpeg_command()
        .arg("-i")
        .arg(video)
        .args(["-f", "f32le", "-ar", &audio.rate.to_string()])
        .args(["-ac", &audio.channels.to_string()])
        .arg("-i")
        .arg(&audio.pcm)
        .args(["-c:v", "copy", "-c:a", &audio.codec, "-b:a", &audio.bitrate])
        .args(["-map", "0:v:0", "-map", "1:a:0", "-shortest"])
        .arg(output)
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| Error::tool("ffmpeg", e.to_string()))?;
    if !result.status.success() {
        return Err(Error::tool(
            "ffmpeg",
            format!(
                "muxing audio into '{}' failed: {}",
                output.display(),
                String::from_utf8_lossy(&result.stderr).trim()
            ),
        ));
    }
    Ok(())
}

/// Write interleaved f32 PCM for the encoder to pick up.
pub fn write_pcm(path: &Path, samples: &[f32]) -> Result<()> {
    let mut bytes = Vec::with_capacity(samples.len() * 4);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    std::fs::write(path, bytes).map_err(|e| Error::io(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::time::Time;

    #[test]
    fn a_missing_encoder_errors_and_lists_what_exists() {
        let tool = Toolchain::discover().unwrap();
        let err = Encoder::Named("libnope".into()).resolve(&tool).unwrap_err();
        assert_eq!(err.exit_code(), dvs_core::error::exit::TOOL_MISSING);
        assert!(err.to_string().contains("libx264"), "{err}");
        assert_eq!(Encoder::Auto.resolve(&tool).unwrap(), "libx264");
    }

    #[test]
    fn encoder_aliases_map_to_ffmpeg_names() {
        assert_eq!(Encoder::parse("h264"), Encoder::X264);
        assert_eq!(Encoder::parse(""), Encoder::Auto);
        assert_eq!(Encoder::parse("videotoolbox"), Encoder::VideoToolbox);
        assert_eq!(Encoder::parse("libvpx-vp9"), Encoder::Named("libvpx-vp9".into()));
    }

    #[test]
    fn written_frames_come_back_out_of_the_file_unchanged() {
        let tool = Toolchain::discover().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("solid.mp4");
        let fps = Fps::new(30, 1).unwrap();
        let mut spec = EncodeSpec::new(&output, [64, 48], fps);
        spec.quality = 0;
        let mut session = EncoderSession::start(&tool, &spec).unwrap();
        let red = Frame::filled(64, 48, Rgba::opaque(220, 40, 40));
        for _ in 0..15 {
            session.write(&red).unwrap();
        }
        assert_eq!(session.frames_written(), 15);
        session.finish().unwrap();

        let probed = crate::probe::probe(&tool, &output).unwrap();
        let stream = probed.probe.video.expect("encoded a video stream");
        assert_eq!(stream.size, [64, 48]);
        assert_eq!(stream.fps, fps);
        assert_eq!(probed.probe.duration, Time::new(1, 2).unwrap());

        // Round-trip the pixels: a red frame must decode back as red. This catches a
        // range/matrix mistake in the encode path, which is otherwise invisible until a
        // viewer complains.
        let mut decoder = crate::decode::VideoDecoder::open(
            &tool,
            &output,
            crate::decode::DecodeSpec::for_stream(&stream, fps),
        );
        let frame = decoder.frame_at(Time::ZERO).unwrap();
        let pixel = frame.pixel(32, 24);
        let srgb = [
            dvs_core::color::linear_to_srgb(pixel[0]) * 255.0,
            dvs_core::color::linear_to_srgb(pixel[1]) * 255.0,
            dvs_core::color::linear_to_srgb(pixel[2]) * 255.0,
        ];
        assert!(
            (srgb[0] - 220.0).abs() < 8.0 && (srgb[1] - 40.0).abs() < 8.0 && (srgb[2] - 40.0).abs() < 8.0,
            "round-tripped color drifted: {srgb:?}"
        );
    }

    #[test]
    fn a_wrong_sized_frame_is_refused_rather_than_corrupting_the_stream() {
        let tool = Toolchain::discover().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let spec = EncodeSpec::new(dir.path().join("x.mp4"), [32, 32], Fps::new(30, 1).unwrap());
        let mut session = EncoderSession::start(&tool, &spec).unwrap();
        let err = session.write(&Frame::filled(16, 16, Rgba::WHITE)).unwrap_err();
        assert!(err.to_string().contains("32x32"), "{err}");
    }

    #[test]
    fn segments_concatenate_into_their_total_duration() {
        let tool = Toolchain::discover().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let fps = Fps::new(30, 1).unwrap();
        let mut parts = Vec::new();
        for (index, color) in [Rgba::opaque(10, 10, 200), Rgba::opaque(200, 200, 10)]
            .into_iter()
            .enumerate()
        {
            let path = dir.path().join(format!("part{index}.mp4"));
            let mut spec = EncodeSpec::new(&path, [64, 48], fps);
            spec.closed_gop = true;
            let mut session = EncoderSession::start(&tool, &spec).unwrap();
            let frame = Frame::filled(64, 48, color);
            for _ in 0..30 {
                session.write(&frame).unwrap();
            }
            session.finish().unwrap();
            parts.push(path);
        }
        let joined = dir.path().join("joined.mp4");
        concat(&tool, &parts, &joined).unwrap();
        let probed = crate::probe::probe(&tool, &joined).unwrap();
        assert!(
            (probed.probe.duration.as_secs_f64() - 2.0).abs() < 0.05,
            "concatenated duration was {}",
            probed.probe.duration.as_secs_f64()
        );
    }

    #[test]
    fn muxing_pcm_produces_a_file_with_both_streams() {
        let tool = Toolchain::discover().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let fps = Fps::new(30, 1).unwrap();
        let video = dir.path().join("v.mp4");
        let mut session = EncoderSession::start(&tool, &EncodeSpec::new(&video, [64, 48], fps)).unwrap();
        let frame = Frame::filled(64, 48, Rgba::WHITE);
        for _ in 0..30 {
            session.write(&frame).unwrap();
        }
        session.finish().unwrap();

        let pcm = dir.path().join("a.pcm");
        let samples: Vec<f32> = (0..48_000 * 2)
            .map(|i| (i as f32 / 48.0).sin() * 0.25)
            .collect();
        write_pcm(&pcm, &samples).unwrap();
        let muxed = dir.path().join("av.mp4");
        mux_audio(
            &tool,
            &video,
            &AudioSpec {
                pcm: pcm.clone(),
                rate: 48_000,
                channels: 2,
                codec: "aac".into(),
                bitrate: "192k".into(),
            },
            &muxed,
        )
        .unwrap();
        let probed = crate::probe::probe(&tool, &muxed).unwrap();
        assert!(probed.probe.video.is_some());
        let audio = probed.probe.audio.expect("muxed audio stream");
        assert_eq!(audio.rate, 48_000);
        assert_eq!(audio.channels, 2);
    }
}
