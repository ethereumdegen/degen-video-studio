//! EBU R128 loudness, true peak and clipping — the mix report an agent can act on.
//!
//! "Is it too loud" has one answer that platforms and broadcasters agree on, and it is not
//! peak level: it is integrated loudness in LUFS, gated per EBU R128, with true peak
//! measured on an oversampled signal because inter-sample peaks clip a codec that the
//! sample values alone say is fine. This module exists so that the loudness printed in a
//! digest, the loudness the `audio.normalize` op corrects, and the loudness the lint
//! compares against a target profile are literally the same computation. Two measurements
//! that disagree by 0.3 LU would send an agent into a loop of corrections that never
//! converge.
//!
//! Levels are reported as finite numbers. `-inf` is the mathematically correct loudness of
//! silence and a menace in JSON, where it becomes `null` and makes every comparison in a
//! consumer silently false; [`crate::SILENCE_FLOOR_DB`] is reported instead.

use dvs_core::error::{Error, Result};
use ebur128::{EbuR128, Mode};
use rayon::prelude::*;
use serde::Serialize;

/// The short-term window EBU R128 defines, in milliseconds. Queried on the 100 ms grid the
/// standard's gating blocks use, so the reported minimum is one of the values a compliant
/// meter would have displayed.
const SHORT_TERM_MS: u32 = 3000;

/// How often the short-term value is sampled, in milliseconds.
const SHORT_TERM_STEP_MS: u32 = 100;

/// What a mix measures.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Loudness {
    /// Gated programme loudness over the whole buffer, in LUFS. This is the number a
    /// delivery spec names: −14 for YouTube, −16 for podcasts, −23 for broadcast.
    pub integrated_lufs: f64,
    /// Highest inter-sample peak, in dBTP. A mix that measures −0.2 dBTP will clip once an
    /// encoder resamples it, which is why this is not the sample peak.
    pub true_peak_db: f64,
    /// Loudness range in LU: how far the quiet parts sit below the loud parts. A
    /// talking-head edit under 3 LU has been over-compressed; over 15 LU has an
    /// unintelligible quiet section.
    pub lra: f64,
    /// Quietest short-term (3 s) window. The integrated value hides a section that is 10 LU
    /// too quiet; this does not.
    pub short_term_min_lufs: f64,
    /// Samples that reached or passed full scale. The mixer has no limiter, so this is the
    /// only report that summing two loud clips went past 0 dBFS.
    pub clipped_samples: u64,
}

/// Measure an interleaved f32 buffer.
///
/// The buffer must be a whole number of frames: a truncated frame would shift every
/// channel after it and quietly measure the wrong signal.
pub fn analyze_loudness(samples: &[f32], rate: u32, channels: u16) -> Result<Loudness> {
    if channels == 0 {
        return Err(Error::bad_args("loudness needs at least one channel"));
    }
    let channels = usize::from(channels);
    if !(16..=2_822_400).contains(&rate) {
        return Err(Error::bad_args(format!(
            "sample rate {rate} is outside what a loudness meter accepts"
        )));
    }
    if samples.len() % channels != 0 {
        return Err(Error::bad_args(format!(
            "{} samples is not a whole number of {channels}-channel frames",
            samples.len()
        )));
    }

    let clipped = count_clipped(samples);
    let mut meter = EbuR128::new(channels as u32, rate, Mode::I | Mode::LRA | Mode::TRUE_PEAK)
        .map_err(|error| Error::op(format!("cannot build a loudness meter: {error}")))?;

    // Fed in short-term steps rather than one block, because the minimum short-term value
    // only exists if the meter is interrogated as the signal goes by.
    let step_frames = (rate as usize * SHORT_TERM_STEP_MS as usize / 1000).max(1);
    let warmup_frames = rate as usize * SHORT_TERM_MS as usize / 1000;
    let mut fed = 0usize;
    let mut short_term_min = f64::INFINITY;
    for chunk in samples.chunks(step_frames * channels) {
        meter
            .add_frames_f32(chunk)
            .map_err(|error| Error::op(format!("loudness analysis failed: {error}")))?;
        fed += chunk.len() / channels;
        if fed >= warmup_frames {
            let short = meter
                .loudness_shortterm()
                .map_err(|error| Error::op(format!("short-term loudness failed: {error}")))?;
            if short.is_finite() && short < short_term_min {
                short_term_min = short;
            }
        }
    }

    let integrated = meter
        .loudness_global()
        .map_err(|error| Error::op(format!("integrated loudness failed: {error}")))?;
    let lra = meter
        .loudness_range()
        .map_err(|error| Error::op(format!("loudness range failed: {error}")))?;
    let mut peak = 0.0f64;
    for channel in 0..channels as u32 {
        let value = meter
            .true_peak(channel)
            .map_err(|error| Error::op(format!("true peak failed: {error}")))?;
        peak = peak.max(value);
    }

    Ok(Loudness {
        integrated_lufs: floored(integrated),
        true_peak_db: floored(if peak > 0.0 {
            20.0 * peak.log10()
        } else {
            f64::NEG_INFINITY
        }),
        lra: if lra.is_finite() { lra } else { 0.0 },
        // A buffer shorter than the 3 s short-term window never produced a value; the
        // integrated figure is the only thing known about it.
        short_term_min_lufs: floored(if short_term_min.is_finite() {
            short_term_min
        } else {
            integrated
        }),
        clipped_samples: clipped,
    })
}

/// Gain, in dB, that moves a measured loudness onto a target.
///
/// One line, and a function anyway: `audio.normalize` writes it into the document and the
/// `loudness-out-of-spec` lint reports it as the suggested fix. Two spellings of this
/// subtraction would be a lint that recommends a correction the op does not apply.
pub fn normalize_gain_db(current_lufs: f64, target_lufs: f64) -> f32 {
    (target_lufs - current_lufs) as f32
}

/// Samples at or beyond full scale, counted in parallel because this walks the whole mix
/// and is pure arithmetic over a contiguous buffer.
fn count_clipped(samples: &[f32]) -> u64 {
    samples
        .par_iter()
        .filter(|sample| sample.abs() >= 1.0)
        .count() as u64
}

/// Replace a non-finite level with the reporting floor.
fn floored(value: f64) -> f64 {
    if value.is_finite() {
        value.max(crate::SILENCE_FLOOR_DB)
    } else {
        crate::SILENCE_FLOOR_DB
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db_to_gain;
    use crate::mix::fixture::{tool, wav, RATE};
    use std::process::Stdio;

    /// Read a WAV back as interleaved f32 at the sequence rate.
    fn decode(path: &std::path::Path, channels: u16, seconds: i64) -> Vec<f32> {
        dvs_media::decode_audio(
            tool(),
            path,
            dvs_core::time::Span::new(
                dvs_core::time::Time::ZERO,
                dvs_core::time::Time::from_secs(seconds),
            ),
            RATE,
            channels,
        )
        .expect("decode")
    }

    /// What ffmpeg's own `ebur128` filter says, so the Rust meter is checked against the
    /// reference implementation rather than against itself.
    fn ffmpeg_integrated(path: &std::path::Path) -> f64 {
        let output = tool()
            .ffmpeg_command()
            .args(["-loglevel", "info"])
            .arg("-i")
            .arg(path)
            .args(["-af", "ebur128=peak=true", "-f", "null", "-"])
            .stderr(Stdio::piped())
            .output()
            .expect("ffmpeg runs");
        let text = String::from_utf8_lossy(&output.stderr);
        let summary = text
            .split("Integrated loudness:")
            .nth(1)
            .expect("ffmpeg printed an integrated loudness summary");
        let value = summary
            .lines()
            .find_map(|line| line.trim().strip_prefix("I:"))
            .expect("the I: line");
        value
            .trim()
            .trim_end_matches(" LUFS")
            .trim()
            .parse::<f64>()
            .expect("a number")
    }

    #[test]
    fn integrated_loudness_agrees_with_the_reference_meter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tone.wav");
        // A 1 kHz sine, loudness-normalized by ffmpeg itself to −23 LUFS.
        wav(
            &path,
            "sine=frequency=1000:duration=10:sample_rate=48000,loudnorm=I=-23:LRA=7:tp=-2",
        );
        let reference = ffmpeg_integrated(&path);
        let samples = decode(&path, 2, 10);
        let measured = analyze_loudness(&samples, RATE, 2).expect("analyze");

        assert!(
            (measured.integrated_lufs - reference).abs() < 0.5,
            "measured {:.2} LUFS, ffmpeg says {reference:.2} LUFS",
            measured.integrated_lufs
        );
        assert!(
            (measured.integrated_lufs + 23.0).abs() < 1.0,
            "a −23 LUFS reference tone should measure near −23, got {:.2}",
            measured.integrated_lufs
        );
        assert_eq!(measured.clipped_samples, 0);
        assert!(
            measured.true_peak_db < 0.0,
            "a normalized tone must sit below full scale, got {:.2} dBTP",
            measured.true_peak_db
        );
    }

    #[test]
    fn clipped_samples_counts_a_deliberately_clipped_buffer() {
        let mut samples = vec![0.25f32; RATE as usize * 8];
        for sample in samples.iter_mut().step_by(1000) {
            *sample = 1.5;
        }
        let expected = samples.len().div_ceil(1000) as u64;
        let measured = analyze_loudness(&samples, RATE, 2).expect("analyze");
        assert_eq!(measured.clipped_samples, expected);

        let clean = vec![0.25f32; RATE as usize * 8];
        assert_eq!(
            analyze_loudness(&clean, RATE, 2).expect("analyze").clipped_samples,
            0
        );
    }

    #[test]
    fn normalize_gain_then_remeasure_lands_on_the_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("speechish.wav");
        // Amplitude-modulated noise: a flat sine is a degenerate case for a gated meter.
        wav(
            &path,
            "anoisesrc=d=12:c=pink:a=0.3:r=48000,tremolo=f=2:d=0.7",
        );
        let samples = decode(&path, 2, 12);
        let before = analyze_loudness(&samples, RATE, 2).expect("analyze");

        let target = -14.0;
        let gain = normalize_gain_db(before.integrated_lufs, target);
        let scaled: Vec<f32> = samples.iter().map(|s| s * db_to_gain(gain)).collect();
        let after = analyze_loudness(&scaled, RATE, 2).expect("analyze");

        assert!(
            (after.integrated_lufs - target).abs() < 0.5,
            "normalizing from {:.2} by {gain:.2} dB landed at {:.2}, not {target}",
            before.integrated_lufs,
            after.integrated_lufs
        );
    }

    #[test]
    fn short_term_minimum_finds_a_quiet_passage_the_integrated_value_hides() {
        // Eight seconds loud, eight seconds 20 dB down: integrated sits near the loud part,
        // the short-term minimum has to see the quiet one.
        let loud = db_to_gain(-10.0);
        let quiet = db_to_gain(-30.0);
        let frames = RATE as usize * 8;
        let mut samples = Vec::with_capacity(frames * 4);
        for index in 0..frames * 2 {
            let level = if index < frames { loud } else { quiet };
            let value = ((index as f64 / f64::from(RATE)) * 1000.0 * std::f64::consts::TAU).sin();
            samples.push(value as f32 * level);
            samples.push(value as f32 * level);
        }
        let measured = analyze_loudness(&samples, RATE, 2).expect("analyze");
        assert!(
            measured.short_term_min_lufs < measured.integrated_lufs - 10.0,
            "short-term minimum {:.2} should be far below integrated {:.2}",
            measured.short_term_min_lufs,
            measured.integrated_lufs
        );
        assert!(
            measured.lra > 10.0,
            "a 20 dB level drop is a wide loudness range, got {:.2} LU",
            measured.lra
        );
    }

    #[test]
    fn silence_reports_a_finite_floor() {
        let samples = vec![0.0f32; RATE as usize * 8];
        let measured = analyze_loudness(&samples, RATE, 2).expect("analyze");
        assert!(measured.integrated_lufs.is_finite());
        assert_eq!(measured.integrated_lufs, crate::SILENCE_FLOOR_DB);
        assert_eq!(measured.true_peak_db, crate::SILENCE_FLOOR_DB);
        let json = serde_json::to_value(measured).expect("serializable");
        assert!(
            json["integratedLufs"].is_number(),
            "a reported level must survive JSON as a number"
        );
    }

    #[test]
    fn a_ragged_buffer_is_rejected_rather_than_measured_wrong() {
        let samples = vec![0.1f32; 4801];
        let error = analyze_loudness(&samples, RATE, 2)
            .err()
            .expect("an odd sample count is not two channels");
        assert!(error.to_string().contains("whole number"));
    }
}
