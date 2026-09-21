//! Finding the gaps: where nobody is talking.
//!
//! Silence detection is the lever behind `seq.trim-silence`, the `silence-gap` lint and the
//! `silences` field of every render digest, so it has to answer with *spans a human would
//! point at*, not with a list of quiet samples. Those are different things. Speech crosses
//! zero thousands of times a second; a per-sample threshold shreds one pause into hundreds
//! of one-sample fragments, and an op built on that output would cut a sentence into
//! confetti. So the measurement is a short-window RMS, and only runs of quiet windows that
//! last at least `min_duration` are reported.
//!
//! [`detect_speech`] is the exact complement, and that is the point: `seq.trim-silence`
//! removes the silences and keeps the speech, so the two must partition the buffer with no
//! sample belonging to both or to neither. A pause shorter than `min_duration` is therefore
//! part of the speech around it — the breath between two words stays in the cut.

use crate::SILENCE_FLOOR_DB;
use dvs_core::time::{Span, Time};
use rayon::prelude::*;

/// RMS window, in milliseconds. Long enough to span the period of a 50 Hz fundamental so a
/// low voice does not read as silence between cycles, short enough that a 100 ms pause is
/// still resolvable.
const WINDOW_MS: i64 = 20;

/// Spans quieter than `threshold_db` for at least `min_duration`.
///
/// Times are relative to the start of `samples`, so a caller that mixed a span starting at
/// 30 s adds 30 s to every result.
pub fn detect_silence(
    samples: &[f32],
    rate: u32,
    channels: u16,
    threshold_db: f64,
    min_duration: Time,
) -> Vec<Span> {
    let windows = window_levels(samples, rate, channels);
    if windows.is_empty() {
        return Vec::new();
    }
    let window_frames = window_frames(rate);
    let total_frames = samples.len() / usize::from(channels.max(1));
    let min_frames = min_duration.sample_round(rate).max(0);

    let mut spans = Vec::new();
    let mut run_start: Option<usize> = None;
    for (index, level) in windows.iter().enumerate() {
        let quiet = *level < threshold_db;
        match (quiet, run_start) {
            (true, None) => run_start = Some(index),
            (false, Some(start)) => {
                push_run(&mut spans, start, index, window_frames, total_frames, rate, min_frames);
                run_start = None;
            }
            _ => {}
        }
    }
    if let Some(start) = run_start {
        push_run(
            &mut spans,
            start,
            windows.len(),
            window_frames,
            total_frames,
            rate,
            min_frames,
        );
    }
    spans
}

/// Everything [`detect_silence`] did not call silence.
///
/// Used by `seq.trim-silence` to know what survives the cut. Because it is the complement
/// of the same run of windows, concatenating the speech spans of a buffer reproduces the
/// buffer minus exactly the spans that were removed — no sample is dropped twice and none
/// is kept twice.
pub fn detect_speech(
    samples: &[f32],
    rate: u32,
    channels: u16,
    threshold_db: f64,
    min_duration: Time,
) -> Vec<Span> {
    let total_frames = (samples.len() / usize::from(channels.max(1))) as i64;
    let end = Time::from_samples(total_frames, rate);
    let silences = detect_silence(samples, rate, channels, threshold_db, min_duration);
    let mut spans = Vec::with_capacity(silences.len() + 1);
    let mut cursor = Time::ZERO;
    for silence in silences {
        if silence.start > cursor {
            spans.push(Span::new(cursor, silence.start));
        }
        cursor = silence.end;
    }
    if cursor < end {
        spans.push(Span::new(cursor, end));
    }
    spans
}

fn window_frames(rate: u32) -> usize {
    ((i64::from(rate) * WINDOW_MS) / 1000).max(1) as usize
}

/// RMS level of each non-overlapping window, in dBFS.
///
/// Non-overlapping windows quantize a boundary to 20 ms, which is far finer than any cut a
/// person would make by hand and keeps the scan linear in the buffer.
fn window_levels(samples: &[f32], rate: u32, channels: u16) -> Vec<f64> {
    let channels = usize::from(channels.max(1));
    if samples.len() < channels {
        return Vec::new();
    }
    let chunk = window_frames(rate) * channels;
    samples
        .par_chunks(chunk)
        .map(|window| {
            let mean = window.iter().map(|s| f64::from(*s) * f64::from(*s)).sum::<f64>()
                / window.len() as f64;
            if mean <= 0.0 {
                SILENCE_FLOOR_DB
            } else {
                (10.0 * mean.log10()).max(SILENCE_FLOOR_DB)
            }
        })
        .collect()
}

/// Turn a run of quiet windows into a span, if it is long enough to be worth reporting.
#[allow(clippy::too_many_arguments)]
fn push_run(
    spans: &mut Vec<Span>,
    first_window: usize,
    end_window: usize,
    window_frames: usize,
    total_frames: usize,
    rate: u32,
    min_frames: i64,
) {
    let start = (first_window * window_frames).min(total_frames) as i64;
    let end = (end_window * window_frames).min(total_frames) as i64;
    if end - start < min_frames || end <= start {
        return;
    }
    spans.push(Span::new(
        Time::from_samples(start, rate),
        Time::from_samples(end, rate),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db_to_gain;

    const RATE: u32 = 48_000;

    /// Interleaved stereo tone for `seconds`, then silence, then tone again.
    fn tone_gap_tone(first: f64, gap: f64, last: f64) -> Vec<f32> {
        let mut out = Vec::new();
        let mut push = |seconds: f64, level: f32| {
            let frames = (seconds * f64::from(RATE)) as usize;
            for index in 0..frames {
                let phase = index as f64 / f64::from(RATE) * 440.0 * std::f64::consts::TAU;
                let value = phase.sin() as f32 * level;
                out.push(value);
                out.push(value);
            }
        };
        push(first, 0.5);
        push(gap, 0.0);
        push(last, 0.5);
        out
    }

    #[test]
    fn one_pause_is_one_span_not_fifty() {
        let samples = tone_gap_tone(1.0, 1.5, 1.0);
        let spans = detect_silence(&samples, RATE, 2, -40.0, Time::new(1, 10).expect("0.1s"));
        assert_eq!(
            spans.len(),
            1,
            "a single pause must be one span, got {spans:?}"
        );
        let span = spans[0];
        assert!(
            (span.start.as_secs_f64() - 1.0).abs() < 0.05,
            "pause starts at 1.0 s, reported {}",
            span.start
        );
        assert!(
            (span.end.as_secs_f64() - 2.5).abs() < 0.05,
            "pause ends at 2.5 s, reported {}",
            span.end
        );
    }

    #[test]
    fn a_pause_shorter_than_min_duration_is_not_reported() {
        let samples = tone_gap_tone(1.0, 0.2, 1.0);
        let short = detect_silence(&samples, RATE, 2, -40.0, Time::new(1, 2).expect("0.5s"));
        assert!(
            short.is_empty(),
            "a 0.2 s pause is below a 0.5 s minimum, got {short:?}"
        );
        let long = detect_silence(&samples, RATE, 2, -40.0, Time::new(1, 10).expect("0.1s"));
        assert_eq!(long.len(), 1, "the same pause passes a 0.1 s minimum");
    }

    #[test]
    fn a_quiet_room_tone_counts_as_silence_against_a_high_threshold() {
        let mut samples = tone_gap_tone(1.0, 1.0, 1.0);
        // Fill the "gap" with room tone 50 dB down. It is not digital silence.
        let gap_start = RATE as usize * 2;
        let gap_end = RATE as usize * 4;
        for index in gap_start..gap_end {
            let phase = index as f64 / f64::from(RATE) * 200.0 * std::f64::consts::TAU;
            samples[index] = phase.sin() as f32 * db_to_gain(-50.0);
        }
        let strict = detect_silence(&samples, RATE, 2, -60.0, Time::new(1, 10).expect("0.1s"));
        assert!(
            strict.is_empty(),
            "room tone at −50 dB is above a −60 dB threshold, got {strict:?}"
        );
        let loose = detect_silence(&samples, RATE, 2, -40.0, Time::new(1, 10).expect("0.1s"));
        assert_eq!(loose.len(), 1, "the same room tone is below −40 dB");
    }

    #[test]
    fn speech_and_silence_partition_the_buffer() {
        let samples = tone_gap_tone(1.0, 1.0, 1.0);
        let min = Time::new(1, 10).expect("0.1s");
        let silence = detect_silence(&samples, RATE, 2, -40.0, min);
        let speech = detect_speech(&samples, RATE, 2, -40.0, min);
        let total: f64 = silence
            .iter()
            .chain(&speech)
            .map(|span| span.duration().as_secs_f64())
            .sum();
        assert!(
            (total - 3.0).abs() < 0.05,
            "speech plus silence must cover the whole 3 s buffer, got {total}"
        );
        for quiet in &silence {
            for loud in &speech {
                assert!(
                    !quiet.overlaps(loud),
                    "speech {loud} overlaps silence {quiet}"
                );
            }
        }
        assert_eq!(speech.len(), 2, "two phrases around one pause");
    }

    #[test]
    fn leading_and_trailing_silence_are_found() {
        // 1 s of silence, 1 s of tone, 1 s of silence.
        let mut samples = tone_gap_tone(0.0, 1.0, 1.0);
        samples.extend(tone_gap_tone(0.0, 1.0, 0.0));
        let spans = detect_silence(&samples, RATE, 2, -40.0, Time::new(1, 2).expect("0.5s"));
        assert_eq!(spans.len(), 2, "head and tail silence, got {spans:?}");
        assert_eq!(spans[0].start, Time::ZERO);
        assert!(
            (spans[0].end.as_secs_f64() - 1.0).abs() < 0.05,
            "the leading silence is one second, got {}",
            spans[0]
        );
        assert!(
            (spans[1].start.as_secs_f64() - 2.0).abs() < 0.05
                && (spans[1].end.as_secs_f64() - 3.0).abs() < 0.05,
            "the trailing silence runs from 2 s to the end of the buffer, got {}",
            spans[1]
        );
    }
}
