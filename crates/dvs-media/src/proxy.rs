//! Proxies, thumbnails and waveforms: the derived media that makes interaction bearable.
//!
//! All three are regenerable and live under `cache/`, so they are never part of the
//! document's identity and `dvs gc` can drop them freely.
//!
//! The proxy exists for two distinct reasons. The obvious one is speed: scrubbing 4K H.265
//! on a laptop is hopeless, and an agent rendering a contact sheet does not need full
//! resolution. The less obvious and more important one is correctness: a variable-frame-rate
//! source has no answer to "what is frame 1274", so import normalizes it to constant rate
//! and everything frame-exact happens against that.

use crate::frame::Frame;
use crate::toolchain::Toolchain;
use dvs_core::error::{Error, Result};
use dvs_core::project::Probe;
use dvs_core::time::{Fps, Span, Time};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Proxy target height. 540p is enough to judge framing and read titles while being ~8×
/// cheaper to decode than 1080p.
pub const PROXY_HEIGHT: u32 = 540;

#[derive(Debug, Clone)]
pub struct ProxySpec {
    pub height: u32,
    /// Constant output rate. For a VFR source this is what makes frame indices meaningful.
    pub fps: Fps,
    /// Quality. Proxies are for looking at, not for delivery.
    pub quality: u32,
}

impl ProxySpec {
    pub fn for_probe(probe: &Probe, fallback: Fps) -> ProxySpec {
        let fps = probe
            .video
            .as_ref()
            .map(|stream| stream.fps)
            .unwrap_or(fallback);
        ProxySpec {
            height: PROXY_HEIGHT,
            fps,
            quality: 26,
        }
    }
}

/// True when a source cannot be edited frame-exactly as it stands, or is expensive enough
/// that interaction needs a stand-in.
pub fn wants_proxy(probe: &Probe) -> bool {
    if probe.vfr {
        return true;
    }
    probe
        .video
        .as_ref()
        .is_some_and(|stream| stream.display_size()[1] > 1080)
}

/// Encode a constant-frame-rate, low-resolution stand-in. Audio is kept so the proxy can
/// drive scrubbing and silence detection on its own.
pub fn make_proxy(tool: &Toolchain, source: &Path, output: &Path, spec: &ProxySpec) -> Result<()> {
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let result = tool
        .ffmpeg_command()
        .arg("-i")
        .arg(source)
        // `-2` keeps the width even and preserves aspect, which h264's 4:2:0 requires.
        .args([
            "-vf",
            &format!(
                "fps={},scale=-2:{}:flags=bilinear",
                spec.fps.ffmpeg_arg(),
                spec.height
            ),
        ])
        .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", &spec.quality.to_string()])
        .args(["-pix_fmt", "yuv420p", "-g", "30"])
        .args(["-c:a", "aac", "-b:a", "128k", "-ac", "2"])
        .args(["-movflags", "+faststart"])
        .arg(output)
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| Error::tool("ffmpeg", e.to_string()))?;
    if !result.status.success() {
        return Err(Error::tool(
            "ffmpeg",
            format!(
                "building a proxy for '{}' failed: {}",
                source.display(),
                String::from_utf8_lossy(&result.stderr).trim()
            ),
        ));
    }
    Ok(())
}

/// Extract evenly spaced JPEG thumbnails. Returns the written paths in time order.
pub fn thumbnails(
    tool: &Toolchain,
    source: &Path,
    duration: Time,
    every: Time,
    width: u32,
    output_dir: &Path,
    stem: &str,
) -> Result<Vec<PathBuf>> {
    if !every.is_positive() {
        return Err(Error::bad_args("thumbnail interval must be positive"));
    }
    std::fs::create_dir_all(output_dir).map_err(|e| Error::io(output_dir, e))?;
    let count = (duration.as_secs_f64() / every.as_secs_f64()).floor().max(1.0) as usize;
    let mut written = Vec::with_capacity(count);
    for index in 0..count {
        let at = every * dvs_core::time::Rat::new(index as i64, 1)?;
        let path = output_dir.join(format!("{stem}-{index:04}.jpg"));
        let result = tool
            .ffmpeg_command()
            .args(["-ss", &format!("{:.4}", at.as_secs_f64())])
            .arg("-i")
            .arg(source)
            .args(["-frames:v", "1", "-vf", &format!("scale={width}:-2")])
            .args(["-q:v", "4"])
            .arg(&path)
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| Error::tool("ffmpeg", e.to_string()))?;
        if !result.status.success() {
            return Err(Error::tool(
                "ffmpeg",
                format!(
                    "thumbnail at {} failed: {}",
                    at.clock(),
                    String::from_utf8_lossy(&result.stderr).trim()
                ),
            ));
        }
        written.push(path);
    }
    Ok(written)
}

/// Min/max pairs per bucket, which is what a waveform display needs and what silence
/// detection can run on without decoding the whole file again.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Waveform {
    pub buckets_per_second: u32,
    pub duration: Time,
    /// `[min, max]` per bucket, mono-summed, in −1..1.
    pub peaks: Vec<[f32; 2]>,
    /// RMS per bucket, which is what loudness-like decisions should use.
    pub rms: Vec<f32>,
}

pub fn waveform(
    tool: &Toolchain,
    source: &Path,
    duration: Time,
    buckets_per_second: u32,
) -> Result<Waveform> {
    if buckets_per_second == 0 {
        return Err(Error::bad_args("buckets per second must be positive"));
    }
    // 8 kHz mono is plenty for a peak display and 6× cheaper to decode than 48 kHz stereo.
    const RATE: u32 = 8_000;
    let samples = crate::decode::decode_audio(
        tool,
        source,
        Span::new(Time::ZERO, duration),
        RATE,
        1,
    )?;
    let per_bucket = (RATE / buckets_per_second).max(1) as usize;
    let mut peaks = Vec::with_capacity(samples.len() / per_bucket + 1);
    let mut rms = Vec::with_capacity(peaks.capacity());
    for chunk in samples.chunks(per_bucket) {
        let mut min = 0.0f32;
        let mut max = 0.0f32;
        let mut square = 0.0f64;
        for sample in chunk {
            min = min.min(*sample);
            max = max.max(*sample);
            square += (*sample as f64) * (*sample as f64);
        }
        peaks.push([min, max]);
        rms.push((square / chunk.len().max(1) as f64).sqrt() as f32);
    }
    Ok(Waveform {
        buckets_per_second,
        duration,
        peaks,
        rms,
    })
}

/// Write a frame as a PNG through ffmpeg, so there is one image encoder in the stack.
pub fn write_png(tool: &Toolchain, frame: &Frame, output: &Path) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let mut child = tool
        .ffmpeg_command()
        .args(["-f", "rawvideo", "-pix_fmt", "rgba"])
        .args(["-s", &format!("{}x{}", frame.width(), frame.height())])
        .args(["-i", "-", "-frames:v", "1"])
        .arg(output)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::tool("ffmpeg", e.to_string()))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| Error::tool("ffmpeg", "png writer has no stdin"))?;
        stdin
            .write_all(&frame.to_rgba8())
            .map_err(|e| Error::tool("ffmpeg", format!("writing png bytes: {e}")))?;
    }
    let output_status = child
        .wait_with_output()
        .map_err(|e| Error::tool("ffmpeg", e.to_string()))?;
    if !output_status.status.success() {
        return Err(Error::tool(
            "ffmpeg",
            format!(
                "writing '{}' failed: {}",
                output.display(),
                String::from_utf8_lossy(&output_status.stderr).trim()
            ),
        ));
    }
    Ok(())
}

/// Read a PNG (or any still ffmpeg can decode) back into a frame. Used by the perceptual
/// diff and by golden tests.
pub fn read_png(tool: &Toolchain, path: &Path) -> Result<Frame> {
    crate::decode::decode_image(tool, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::color::Rgba;
    use dvs_core::project::{Probe, VideoStream};
    use dvs_core::time::Rat;

    fn stream(size: [u32; 2], fps: Fps) -> VideoStream {
        VideoStream {
            stream_index: 0,
            size,
            fps,
            codec: "h264".into(),
            pix_fmt: "yuv420p".into(),
            color_range: Default::default(),
            color_matrix: Default::default(),
            color_primaries: None,
            transfer: None,
            rotation: 0,
            sar: Rat::ONE,
            frames: None,
            bit_rate: None,
        }
    }

    #[test]
    fn proxy_is_wanted_for_vfr_and_for_oversized_sources() {
        let fps = Fps::new(30, 1).unwrap();
        let hd = Probe {
            duration: Time::from_secs(10),
            video: Some(stream([1920, 1080], fps)),
            audio: None,
            vfr: false,
            container: String::new(),
        };
        assert!(!wants_proxy(&hd));
        assert!(wants_proxy(&Probe { vfr: true, ..hd.clone() }));
        assert!(wants_proxy(&Probe {
            video: Some(stream([3840, 2160], fps)),
            ..hd
        }));
    }

    #[test]
    fn a_vfr_source_becomes_a_constant_rate_proxy() {
        let tool = Toolchain::discover().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // A genuinely variable-rate file: one second at 30 fps followed by one at 5 fps,
        // concatenated with timestamps passed through. `setpts` tricks produce a file whose
        // nominal and average rates differ by ~1%, which is indistinguishable from rounding;
        // this one differs by 12%, which is what a real screen recording looks like.
        for (name, rate) in [("fast.mp4", 30), ("slow.mp4", 5)] {
            let made = tool
                .ffmpeg_command()
                .args([
                    "-f",
                    "lavfi",
                    "-i",
                    &format!("testsrc2=size=640x360:rate={rate}:duration=1"),
                ])
                .args(["-c:v", "libx264", "-preset", "ultrafast"])
                .arg(dir.path().join(name))
                .output()
                .unwrap();
            assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
        }
        let list = dir.path().join("parts.txt");
        std::fs::write(&list, "file 'fast.mp4'\nfile 'slow.mp4'\n").unwrap();
        let source = dir.path().join("vfr.mp4");
        let made = tool
            .ffmpeg_command()
            .args(["-f", "concat", "-safe", "0"])
            .arg("-i")
            .arg(&list)
            .args(["-fps_mode", "passthrough", "-c:v", "libx264", "-preset", "ultrafast"])
            .arg(&source)
            .output()
            .unwrap();
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));

        let probe = crate::probe::probe(&tool, &source).unwrap().probe;
        assert!(probe.vfr, "fixture must actually be variable rate");
        let output = dir.path().join("proxy/vfr.mp4");
        make_proxy(
            &tool,
            &source,
            &output,
            &ProxySpec {
                height: 180,
                fps: Fps::new(30, 1).unwrap(),
                quality: 30,
            },
        )
        .unwrap();
        let proxied = crate::probe::probe(&tool, &output).unwrap().probe;
        let proxy_stream = proxied.video.expect("proxy has video");
        assert_eq!(proxy_stream.display_size()[1], 180);
        assert_eq!(proxy_stream.fps, Fps::new(30, 1).unwrap());
        assert!(!proxied.vfr, "proxy must be constant rate");
        // The source's own timing was irregular, which is the reason the proxy exists.
        assert!(probe.duration.is_positive());
    }

    #[test]
    fn waveform_buckets_track_signal_and_silence() {
        let tool = Toolchain::discover().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("half.wav");
        // One second of tone, one second of silence.
        // Explicit amplitude: ffmpeg's `sine` source generates at 1/8 scale, so asserting a
        // level against it would measure ffmpeg's default gain rather than our bucketing.
        let made = tool
            .ffmpeg_command()
            .args([
                "-f",
                "lavfi",
                "-i",
                "aevalsrc=0.8*sin(2*PI*440*t):s=48000:d=1",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=48000:cl=mono:d=1",
                "-filter_complex",
                "[0:a][1:a]concat=n=2:v=0:a=1",
            ])
            .arg(&source)
            .output()
            .unwrap();
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));

        let wave = waveform(&tool, &source, Time::from_secs(2), 10).unwrap();
        assert_eq!(wave.peaks.len(), 20, "10 buckets per second over 2 s");
        let loud = wave.rms[..8].iter().copied().fold(0.0f32, f32::max);
        let quiet = wave.rms[12..].iter().copied().fold(0.0f32, f32::max);
        assert!(loud > 0.2, "tone half measured {loud}");
        assert!(quiet < 0.01, "silent half measured {quiet}");
        assert!(wave.peaks[..8].iter().any(|[min, max]| *min < -0.2 && *max > 0.2));
    }

    #[test]
    fn frames_round_trip_through_png() {
        let tool = Toolchain::discover().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame.png");
        let mut frame = Frame::filled(32, 16, Rgba::opaque(10, 200, 90));
        frame.set_pixel(0, 0, Rgba::opaque(255, 255, 255).to_linear_premul());
        write_png(&tool, &frame, &path).unwrap();
        let back = read_png(&tool, &path).unwrap();
        assert_eq!(back.size(), [32, 16]);
        assert_eq!(back.to_rgba8(), frame.to_rgba8(), "png round trip lost pixels");
    }

    #[test]
    fn thumbnails_land_on_the_requested_interval() {
        let tool = Toolchain::discover().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src.mp4");
        crate::decode::synthesize(
            &tool,
            &source,
            "testsrc2",
            Time::from_secs(4),
            Fps::new(30, 1).unwrap(),
            [320, 180],
        )
        .unwrap();
        let written = thumbnails(
            &tool,
            &source,
            Time::from_secs(4),
            Time::from_secs(1),
            160,
            &dir.path().join("thumb"),
            "ast_x",
        )
        .unwrap();
        assert_eq!(written.len(), 4);
        assert!(written.iter().all(|path| path.is_file()));
        // Different instants of an animated source must differ, or the seek was ignored.
        let first = read_png(&tool, &written[0]).unwrap();
        let last = read_png(&tool, &written[3]).unwrap();
        assert_ne!(first.pixels(), last.pixels());
    }
}
