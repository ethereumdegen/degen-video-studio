//! Perceptual comparison of two rendered files.
//!
//! Two jobs, both of which an agent cannot do by eye.
//!
//! **Intent checking.** "I retimed one clip — did that change only 00:40–00:45?" The answer
//! is [`DiffReport::changed_ranges`]: sample both files at the same instants, and report the
//! windows where the picture actually differs. A render that touched the whole timeline
//! when it was supposed to touch five seconds is the most expensive kind of silent mistake,
//! because everything downstream of it looks fine.
//!
//! **Golden tests.** Encoded bytes are not reproducible across ffmpeg builds even when our
//! frames are, so a golden test cannot be a checksum. It is `min_ssim >= 0.99` over sampled
//! frames, which is stable across encoders and still fails on a real regression.
//!
//! # SSIM, and the constant that silently breaks it
//!
//! SSIM is computed here rather than taken from a crate: it is a local mean/variance
//! comparison in about forty lines, and the one dependency that does it well
//! (`dssim-core`) is AGPL, which would relicense this whole workspace.
//!
//! Per window, on a single luma plane in linear light:
//!
//! ```text
//! ssim = ((2·μx·μy + C1)(2·σxy + C2)) / ((μx² + μy² + C1)(σx² + σy² + C2))
//! C1 = (0.01·L)²   C2 = (0.03·L)²
//! ```
//!
//! **`L` is the dynamic range of the input, and it is the trap.** Every reference
//! implementation and every copied snippet assumes 8-bit data, so `L = 255`, giving
//! `C1 = 6.5025` and `C2 = 58.52`. Our pixels are floats in `0..1`, where those constants
//! are hundreds of times larger than any variance in the image — they would swamp both the
//! numerator and the denominator and every comparison, including a frame against its own
//! inverse, would score close to 1.0. The golden-test story would be worthless and would
//! look like it was working. So `L = 1.0` here, `C1 = 1e-4`, `C2 = 9e-4`, and the two tests
//! that pin this are `ssim(x, x) == 1.0` exactly and `ssim(x, inverse of x) < 0.5`.
//!
//! The window is 8×8 stepped by 4 pixels rather than the 11×11 Gaussian of the original
//! paper. Reason: a box window over a half-window step keeps every pixel inside at least
//! two windows, costs a sixteenth of a stride-1 pass, and the Gaussian's advantage is
//! sub-percent on the mean — which is all this function returns.

use crate::analyze::instants;
use dvs_core::error::{Error, Result};
use dvs_core::time::{Fps, Span, Time};
use dvs_media::decode::{DecodeSpec, VideoDecoder};
use dvs_media::{probe, Frame, Toolchain};
use rayon::prelude::*;
use serde::Serialize;
use std::path::Path;

/// Window side, in pixels.
const WINDOW: usize = 8;

/// Stabilizing constants for `L = 1.0`. See the module doc.
const C1: f64 = 0.01 * 0.01;
const C2: f64 = 0.03 * 0.03;

/// Structural similarity below which a sample counts as changed.
///
/// A re-encode of identical frames scores above 0.999; a different shot scores well under
/// 0.9. 0.99 sits in the empty space between the two.
pub const CHANGED_SSIM: f64 = 0.99;

/// Similarity at or above which two samples are called the same picture, and the per-pixel
/// difference that has to accompany it. Decoding the same file twice is bit-identical, so
/// "identical" really does mean identical rather than "close".
pub const IDENTICAL_SSIM: f64 = 0.9999;
const IDENTICAL_MAX_DIFF: f64 = 1e-4;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SampleDiff {
    pub at: Time,
    pub ssim: f64,
    /// Mean absolute per-pixel difference, linear light, `0..1`.
    pub mean_diff: f64,
    /// Worst single pixel, same scale.
    pub max_diff: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffReport {
    pub samples: Vec<SampleDiff>,
    /// Worst sampled frame. This is the number a golden test asserts on.
    pub min_ssim: f64,
    pub mean_ssim: f64,
    /// Windows where the picture differs, `[at, at + every)` per changed sample, coalesced.
    pub changed_ranges: Vec<Span>,
    pub identical: bool,
}

/// Mean SSIM of two frames of the same size.
///
/// Returns `0.0` for mismatched sizes: differently shaped pictures are maximally
/// dissimilar, and an error here would make a diff over a resolution change unusable.
pub fn ssim(a: &Frame, b: &Frame) -> f64 {
    if a.size() != b.size() {
        return 0.0;
    }
    let (width, height) = (a.width() as usize, a.height() as usize);
    if width == 0 || height == 0 {
        return 1.0;
    }
    let left = luma_plane(a);
    let right = luma_plane(b);
    // A picture smaller than the window is compared as one window. Refusing would make
    // thumbnails and icons uncomparable for no benefit.
    let window = WINDOW.min(width).min(height);
    let step = (window / 2).max(1);
    let last_row = height - window;
    let (sum, count) = (0..=last_row)
        .step_by(step)
        .collect::<Vec<usize>>()
        .par_iter()
        .map(|&top| {
            let mut sum = 0.0f64;
            let mut count = 0u64;
            let mut x = 0usize;
            while x + window <= width {
                sum += window_ssim(&left, &right, width, x, top, window);
                count += 1;
                x += step;
            }
            (sum, count)
        })
        .reduce(|| (0.0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    if count == 0 {
        return 1.0;
    }
    sum / count as f64
}

/// Relative luminance in linear light, composited over black.
///
/// Frames decoded from a video file are fully opaque, so the flatten is a no-op there; it
/// matters when a composited frame with transparency is compared, where treating the
/// premultiplied value as the visible one is exactly what the encoder will do.
fn luma_plane(frame: &Frame) -> Vec<f32> {
    frame
        .pixels()
        .chunks_exact(4)
        .map(|pixel| 0.2126 * pixel[0] + 0.7152 * pixel[1] + 0.0722 * pixel[2])
        .collect()
}

fn window_ssim(
    left: &[f32],
    right: &[f32],
    width: usize,
    x: usize,
    y: usize,
    window: usize,
) -> f64 {
    let mut sum_x = 0.0f64;
    let mut sum_y = 0.0f64;
    let mut sum_xx = 0.0f64;
    let mut sum_yy = 0.0f64;
    let mut sum_xy = 0.0f64;
    for row in y..y + window {
        let base = row * width + x;
        for index in base..base + window {
            let a = f64::from(left[index]);
            let b = f64::from(right[index]);
            sum_x += a;
            sum_y += b;
            sum_xx += a * a;
            sum_yy += b * b;
            sum_xy += a * b;
        }
    }
    let n = (window * window) as f64;
    let mean_x = sum_x / n;
    let mean_y = sum_y / n;
    // Population (biased) variance: the window is the whole population here, and the
    // unbiased correction would make an identical pair score slightly off 1.0.
    let var_x = sum_xx / n - mean_x * mean_x;
    let var_y = sum_yy / n - mean_y * mean_y;
    let cov = sum_xy / n - mean_x * mean_y;
    let numerator = (2.0 * mean_x * mean_y + C1) * (2.0 * cov + C2);
    let denominator = (mean_x * mean_x + mean_y * mean_y + C1) * (var_x + var_y + C2);
    if denominator == 0.0 {
        return 1.0;
    }
    numerator / denominator
}

/// Mean and worst absolute per-pixel difference over RGB, in linear light.
fn pixel_diff(a: &Frame, b: &Frame) -> (f64, f64) {
    if a.size() != b.size() {
        return (1.0, 1.0);
    }
    let count = (a.width() as usize) * (a.height() as usize);
    if count == 0 {
        return (0.0, 0.0);
    }
    let (sum, worst) = a
        .pixels()
        .par_chunks(4096)
        .zip(b.pixels().par_chunks(4096))
        .map(|(left, right)| {
            let mut sum = 0.0f64;
            let mut worst = 0.0f64;
            for (x, y) in left.chunks_exact(4).zip(right.chunks_exact(4)) {
                for channel in 0..3 {
                    let delta = f64::from((x[channel] - y[channel]).abs());
                    sum += delta;
                    if delta > worst {
                        worst = delta;
                    }
                }
            }
            (sum, worst)
        })
        .reduce(|| (0.0, 0.0), |a, b| (a.0 + b.0, a.1.max(b.1)));
    (sum / (count as f64 * 3.0), worst)
}

/// Compare two rendered files at shared instants.
///
/// Both files are sampled over the *longer* of the two durations, and each decoder holds
/// its last frame past its own end, so a length difference shows up as a changed tail
/// rather than as an error. `b` is decoded at `a`'s display size, because SSIM between
/// differently shaped pictures is not defined and silently resizing one of them is the
/// honest choice — it is what a viewer comparing them on one screen would see.
pub fn diff(tool: &Toolchain, a: &Path, b: &Path, every: Time) -> Result<DiffReport> {
    let probed_a = probe(tool, a)?;
    let probed_b = probe(tool, b)?;
    let stream_a = probed_a.probe.video.as_ref().ok_or_else(|| {
        Error::bad_args(format!("'{}' has no video stream to compare", a.display()))
    })?;
    let stream_b = probed_b.probe.video.as_ref().ok_or_else(|| {
        Error::bad_args(format!("'{}' has no video stream to compare", b.display()))
    })?;

    let size = stream_a.display_size();
    let size = [size[0].max(2), size[1].max(2)];
    let fps = stream_a.fps;
    let duration = probed_a.probe.duration.max(probed_b.probe.duration);
    let step = if every.is_positive() {
        every
    } else {
        fps.frame_duration()
    };

    let mut decoder_a = VideoDecoder::open(
        tool,
        a,
        DecodeSpec {
            size,
            fps,
            range: stream_a.color_range,
            matrix: stream_a.color_matrix,
            scaler: "bicubic",
        },
    );
    let mut decoder_b = VideoDecoder::open(
        tool,
        b,
        DecodeSpec {
            size,
            fps,
            range: stream_b.color_range,
            matrix: stream_b.color_matrix,
            scaler: "bicubic",
        },
    );

    let mut samples = Vec::new();
    for at in instants(Span::new(Time::ZERO, duration), step, fps) {
        let frame_a = decoder_a.frame_at(at)?;
        let frame_b = decoder_b.frame_at(at)?;
        let (mean_diff, max_diff) = pixel_diff(&frame_a, &frame_b);
        samples.push(SampleDiff {
            at,
            ssim: ssim(&frame_a, &frame_b),
            mean_diff,
            max_diff,
        });
    }
    Ok(report(samples, step, duration, fps))
}

fn report(samples: Vec<SampleDiff>, every: Time, duration: Time, fps: Fps) -> DiffReport {
    let min_ssim = samples
        .iter()
        .map(|sample| sample.ssim)
        .fold(f64::INFINITY, f64::min);
    let min_ssim = if min_ssim.is_finite() { min_ssim } else { 1.0 };
    let mean_ssim = if samples.is_empty() {
        1.0
    } else {
        samples.iter().map(|sample| sample.ssim).sum::<f64>() / samples.len() as f64
    };
    let identical = samples
        .iter()
        .all(|sample| sample.ssim >= IDENTICAL_SSIM && sample.max_diff <= IDENTICAL_MAX_DIFF);

    // A changed sample stands for the window it represents, not for one instant: sampling
    // every second and reporting a zero-length "change at 1 s" would tell a caller nothing
    // about what to re-check.
    let window = if every.is_positive() {
        every
    } else {
        fps.frame_duration()
    };
    let mut changed_ranges: Vec<Span> = Vec::new();
    for sample in samples.iter().filter(|s| s.ssim < CHANGED_SSIM) {
        let span = Span::new(sample.at, (sample.at + window).min(duration.max(sample.at)));
        match changed_ranges.last_mut() {
            Some(last) if last.end >= span.start => last.end = last.end.max(span.end),
            _ => changed_ranges.push(span),
        }
    }
    DiffReport {
        samples,
        min_ssim,
        mean_ssim,
        changed_ranges,
        identical,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{fps, secs, tool};
    use dvs_core::color::Rgba;
    use std::process::Stdio;
    use tempfile::TempDir;

    /// A gradient, so the frame has structure. A flat frame has zero variance and its
    /// inverse scores well — which would make the inverse test pass for the wrong reason.
    fn gradient(width: u32, height: u32) -> Frame {
        let mut pixels = Vec::with_capacity((width as usize) * (height as usize) * 4);
        for y in 0..height {
            for x in 0..width {
                let value = (x as f32 / width as f32) * 0.5 + (y as f32 / height as f32) * 0.5;
                pixels.extend_from_slice(&[value, value * 0.7, 1.0 - value, 1.0]);
            }
        }
        Frame::from_pixels(width, height, pixels)
    }

    fn inverted(frame: &Frame) -> Frame {
        let pixels = frame
            .pixels()
            .chunks_exact(4)
            .flat_map(|pixel| [1.0 - pixel[0], 1.0 - pixel[1], 1.0 - pixel[2], pixel[3]])
            .collect();
        Frame::from_pixels(frame.width(), frame.height(), pixels)
    }

    /// `testsrc2`, optionally with a red box burned in over `[1, 2)`, encoded losslessly and
    /// all-intra so that every unchanged frame decodes bit-identically in both files.
    fn clip(dir: &TempDir, name: &str, boxed: bool) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut command = tool().ffmpeg_command();
        command.args([
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x90:rate=30:duration=4",
        ]);
        if boxed {
            command.args([
                "-vf",
                "drawbox=x=0:y=0:w=iw:h=ih:color=red@1:t=fill:enable='gte(t\\,1)*lt(t\\,2)'",
            ]);
        }
        let output = command
            .args([
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-qp",
                "0",
                "-g",
                "1",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&path)
            .stderr(Stdio::piped())
            .output()
            .expect("run ffmpeg");
        assert!(
            output.status.success(),
            "fixture encode failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        path
    }

    #[test]
    fn a_frame_against_itself_scores_exactly_one() {
        let frame = gradient(64, 48);
        // Exactly, not approximately: with identical inputs every term of the ratio is the
        // same expression, so anything but 1.0 means the windows are misaligned.
        assert_eq!(ssim(&frame, &frame), 1.0);
    }

    #[test]
    fn a_frame_against_its_inverse_scores_far_below_a_half() {
        let frame = gradient(64, 48);
        let score = ssim(&frame, &inverted(&frame));
        assert!(
            score < 0.5,
            "an inverted frame must not look similar; got {score}. If this is near 1.0 the \
             stabilizing constants are scaled for 8-bit data and swamp the statistics."
        );
    }

    #[test]
    fn frames_of_a_different_size_are_maximally_dissimilar() {
        assert_eq!(ssim(&gradient(64, 48), &gradient(32, 24)), 0.0);
    }

    #[test]
    fn a_flat_frame_against_a_different_flat_frame_is_not_called_identical() {
        let black = Frame::filled(32, 32, Rgba::BLACK);
        let white = Frame::filled(32, 32, Rgba::WHITE);
        let score = ssim(&black, &white);
        assert!(
            score < CHANGED_SSIM,
            "black and white are not the same picture; got {score}"
        );
    }

    #[test]
    fn a_file_against_itself_is_identical() {
        let dir = TempDir::new().expect("tempdir");
        let file = clip(&dir, "a.mp4", false);
        let report = diff(tool(), &file, &file, Time::from_secs(1)).expect("diff");
        assert!(report.identical, "a file is identical to itself");
        assert_eq!(report.min_ssim, 1.0);
        assert_eq!(report.mean_ssim, 1.0);
        assert!(report.changed_ranges.is_empty());
        assert_eq!(report.samples.len(), 4, "four one-second samples over 4 s");
    }

    #[test]
    fn only_the_window_that_changed_is_reported() {
        let dir = TempDir::new().expect("tempdir");
        let plain = clip(&dir, "plain.mp4", false);
        let boxed = clip(&dir, "boxed.mp4", true);
        let report = diff(tool(), &plain, &boxed, Time::from_secs(1)).expect("diff");
        assert!(!report.identical);
        assert_eq!(
            report.changed_ranges,
            vec![Span::new(Time::from_secs(1), Time::from_secs(2))],
            "only the second the box covers changed; samples were {:?}",
            report
                .samples
                .iter()
                .map(|sample| (sample.at.to_string(), sample.ssim))
                .collect::<Vec<_>>()
        );
        let untouched: Vec<f64> = report
            .samples
            .iter()
            .filter(|sample| sample.at != Time::from_secs(1))
            .map(|sample| sample.ssim)
            .collect();
        assert!(
            untouched.iter().all(|score| *score >= IDENTICAL_SSIM),
            "the frames outside the edit are untouched; got {untouched:?}"
        );
    }

    #[test]
    fn sampling_finer_than_a_frame_does_not_sample_a_frame_twice() {
        let dir = TempDir::new().expect("tempdir");
        let file = clip(&dir, "a.mp4", false);
        let report = diff(tool(), &file, &file, secs(1, 1000)).expect("diff");
        assert_eq!(
            report.samples.len(),
            4 * fps().nominal_int() as usize,
            "one sample per frame at most"
        );
    }
}
