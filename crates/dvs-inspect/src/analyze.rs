//! Pixel detectors: black, frozen, flash, scene cut.
//!
//! All four are answers to "is the video dead here?", and all four are computed from two
//! scalars per sampled frame — the mean linear luminance, and the mean absolute per-pixel
//! difference against the previous sample. That is deliberately cheap. The alternative,
//! per-frame full-resolution analysis, costs exactly as much as a render: a ten-minute
//! 1080p timeline is 18 000 frames, and decoding all of them to find a two-second freeze is
//! not a diagnostic, it is a second export. Sampling every second finds everything a viewer
//! would notice, at 1/30th of the work, and the interval is a parameter so a caller who
//! genuinely needs frame resolution (single-frame flash hunting, a golden test) can ask for
//! it.
//!
//! Every threshold is a named constant with a stated reason rather than a number inline,
//! because these are the values that decide whether a lint is trusted or ignored:
//!
//! - [`BLACK_LUMA`] works in *linear* light, not on sRGB bytes. sRGB 12/255 is 0.0026
//!   linear, so a threshold of 0.003 accepts a deliberately black frame and a very dark
//!   night shot's letterbox, while rejecting an actually-visible dark grade.
//! - [`FREEZE_CHANGE`] has to sit above codec noise. Two decodes of the same still are not
//!   bit-identical after a lossy round trip; measured on x264 `-crf 18` output the residual
//!   is under 5e-4, so 1.5e-3 separates "stuck decoder" from "quiet shot".
//! - [`SCENE_CHANGE`] is a mean absolute difference, so a cut between two unrelated shots
//!   lands around 0.2–0.6 and a camera move inside one shot stays under 0.1.
//!
//! Scene cuts get one extra step: the coarse sample only says "a cut happened in this
//! second", which is useless for `marker.from-scenes` and actively wrong for
//! `seq.auto-cut-scenes`. [`refine_cut`] bisects the interval down to the exact frame, at
//! a cost of about 2·log2(fps·every) extra composites per cut instead of fps·every.

use dvs_comp::{CompOptions, Compositor};
use crate::Subject;
use dvs_core::error::{Error, Result};
use dvs_core::ids::SequenceId;
use dvs_core::time::{Fps, Span, Time, R};
use dvs_media::{Frame, Toolchain};
use rayon::prelude::*;
use serde::Serialize;

/// Mean linear luminance at or below which a frame reads as black. See the module doc for
/// why this is not `16/255`.
pub const BLACK_LUMA: f32 = 0.003;

/// Mean absolute per-pixel difference below which two frames are the same picture.
pub const FREEZE_CHANGE: f32 = 0.0015;

/// Shortest still run worth reporting. A held frame across a cut or a one-second beauty
/// shot is normal; two seconds of no movement in an edit that is supposed to be moving is
/// the signature of a stuck decoder or a mis-trimmed clip.
pub const MIN_FREEZE: Time = Time::from_ratio(R::new_raw(2, 1));

/// Luminance step, relative to *both* neighbouring samples and in the same direction, that
/// makes a sample a flash. 0.20 in linear light is roughly a doubling of apparent
/// brightness — visible, unpleasant, and never intentional in an unattended render.
pub const FLASH_JUMP: f32 = 0.20;

/// Mean absolute difference above which consecutive samples are different shots.
pub const SCENE_CHANGE: f64 = 0.30;

/// How the sampled analysis is taken. Thresholds are parameters so a caller can tighten
/// them for a specific source; the defaults are the constants above.
#[derive(Debug, Clone)]
pub struct AnalyzeOptions {
    /// Interval between analysed frames.
    pub every: Time,
    /// Render scale. Detection is a whole-frame statistic, so half size costs a quarter of
    /// the work and changes no answer.
    pub scale: f64,
    /// Decode from proxies when an asset has one.
    pub use_proxy: bool,
    /// Portion of the timeline to analyse; `None` is all of it.
    pub range: Option<Span>,
    pub black_luma: f32,
    pub freeze_change: f32,
    pub min_freeze: Time,
    pub flash_jump: f32,
    pub scene_change: f64,
    /// Bisect each detected cut down to the frame. Off is faster and quantizes cuts to
    /// `every`, which is fine for a digest and not fine for an op that splits clips.
    pub refine_cuts: bool,
}

impl Default for AnalyzeOptions {
    fn default() -> Self {
        AnalyzeOptions {
            every: Time::from_secs(1),
            scale: 0.5,
            use_proxy: true,
            range: None,
            black_luma: BLACK_LUMA,
            freeze_change: FREEZE_CHANGE,
            min_freeze: MIN_FREEZE,
            flash_jump: FLASH_JUMP,
            scene_change: SCENE_CHANGE,
            refine_cuts: true,
        }
    }
}

/// One analysed frame.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameStat {
    pub at: Time,
    pub frame: i64,
    /// Mean linear luminance, composited over black.
    pub luma: f32,
    /// Mean absolute per-pixel difference against the previous sample; `0.0` for the first.
    pub change: f32,
}

/// What the sampled pass found.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoAnalysis {
    pub samples: Vec<FrameStat>,
    /// Spans that are black. A run of black samples covers `[first, last + every)`, so a
    /// reported range is the window the blackness was observed in, never wider.
    pub black_ranges: Vec<Span>,
    /// Spans with no movement, excluding black ones — a black hole is reported as black,
    /// which is the more actionable of the two diagnoses.
    pub frozen_ranges: Vec<Span>,
    /// Single-sample luminance spikes.
    pub flashes: Vec<Time>,
    /// Detected shot changes, refined to the frame when
    /// [`AnalyzeOptions::refine_cuts`] is set.
    pub scene_cuts: Vec<Time>,
    /// The interval the samples were taken at, so a consumer can tell "no flashes" from
    /// "sampled too coarsely to see one".
    pub every: Time,
}

/// A compositor over a subject's sequence, at an analysis quality.
///
/// Shared by every module in this crate so that the pixels a digest measures, the pixels a
/// lint judges and the pixels a contact sheet shows are produced by one configuration.
pub(crate) fn compositor<'a>(
    subject: Subject<'a>,
    tool: &'a Toolchain,
    sequence: &SequenceId,
    scale: f64,
    use_proxy: bool,
) -> Result<Compositor<'a>> {
    Compositor::new(
        tool,
        subject.project,
        subject.paths,
        subject.assets,
        sequence,
        CompOptions {
            scale,
            use_proxy,
            // Analysis reduces every frame to a handful of scalars; a sharper scaler would
            // change the fourth decimal place and cost real time.
            scaler: "bilinear",
        },
    )
}

/// Frame-grid instants covering `range`, spaced by `every`.
///
/// Snapped to the grid and de-duplicated, so asking for a 10 ms interval on a 30 fps
/// timeline analyses each frame once rather than three times.
pub(crate) fn instants(range: Span, every: Time, fps: Fps) -> Vec<Time> {
    let step = if every.is_positive() {
        every
    } else {
        fps.frame_duration()
    };
    let mut out: Vec<Time> = Vec::new();
    let mut at = range.start;
    while at < range.end {
        let snapped = Time::from_frames(at.frame_floor(fps), fps);
        if out.last() != Some(&snapped) {
            out.push(snapped);
        }
        at = at + step;
    }
    out
}

/// Mean absolute per-pixel difference over RGB, in linear light.
///
/// Premultiplied values are compared as they are: a layer that vanished leaves black, which
/// is exactly what the viewer sees over the sequence background, so no unpremultiply is
/// needed and none is done.
pub fn mean_abs_diff(a: &Frame, b: &Frame) -> f32 {
    if a.size() != b.size() {
        return 1.0;
    }
    let count = (a.width() as usize) * (a.height() as usize);
    if count == 0 {
        return 0.0;
    }
    // Chunked so the reduction is parallel over big frames and the f64 accumulator keeps a
    // 4K sum honest.
    let sum: f64 = a
        .pixels()
        .par_chunks(4096)
        .zip(b.pixels().par_chunks(4096))
        .map(|(left, right)| {
            left.chunks_exact(4)
                .zip(right.chunks_exact(4))
                .map(|(x, y)| {
                    ((x[0] - y[0]).abs() + (x[1] - y[1]).abs() + (x[2] - y[2]).abs()) as f64
                })
                .sum::<f64>()
        })
        .sum();
    (sum / (count as f64 * 3.0)) as f32
}

/// Sample a sequence and run every detector over the result.
///
/// Takes anything that can name a document: `&Workspace` for a caller that has one open,
/// or a [`Subject`] for an op, which is handed a `Project` and an [`OpCx`] rather than a
/// workspace.
///
/// [`OpCx`]: dvs_core::op::OpCx
pub fn analyze<'a>(
    subject: impl Into<Subject<'a>>,
    tool: &'a Toolchain,
    sequence: &SequenceId,
    options: &AnalyzeOptions,
) -> Result<VideoAnalysis> {
    let subject = subject.into();
    let seq = subject.project.sequence(sequence)?;
    let fps = seq.fps;
    let duration = seq.duration();
    let range = match options.range {
        Some(range) => Span::new(range.start.max(Time::ZERO), range.end.min(duration)),
        None => Span::new(Time::ZERO, duration),
    };
    let mut analysis = VideoAnalysis {
        samples: Vec::new(),
        black_ranges: Vec::new(),
        frozen_ranges: Vec::new(),
        flashes: Vec::new(),
        scene_cuts: Vec::new(),
        every: options.every,
    };
    if range.is_empty() {
        return Ok(analysis);
    }

    let mut comp = compositor(subject, tool, sequence, options.scale, options.use_proxy)?;
    let mut previous: Option<Frame> = None;
    for at in instants(range, options.every, fps) {
        let index = at.frame_floor(fps);
        let frame = comp.frame(index)?;
        let change = match &previous {
            Some(prior) => mean_abs_diff(prior, &frame),
            None => 0.0,
        };
        analysis.samples.push(FrameStat {
            at,
            frame: index,
            luma: frame.mean_luma(),
            change,
        });
        previous = Some(frame);
    }

    analysis.black_ranges = black_runs(&analysis.samples, options, range.end);
    analysis.frozen_ranges = frozen_runs(&analysis.samples, options);
    analysis.flashes = flashes(&analysis.samples, options.flash_jump);
    analysis.scene_cuts = cuts(&mut comp, &analysis.samples, options, fps)?;
    Ok(analysis)
}

/// Detected shot changes in a sequence.
///
/// `threshold` is the mean-absolute-difference gate; pass a non-positive value to take
/// [`SCENE_CHANGE`]. Cuts are refined to the frame, because the callers are
/// `marker.from-scenes` and `seq.auto-cut-scenes` and a marker one second away from the cut
/// it names is worse than no marker.
pub fn scenes<'a>(
    subject: impl Into<Subject<'a>>,
    tool: &'a Toolchain,
    sequence: &SequenceId,
    threshold: f64,
) -> Result<Vec<Time>> {
    let options = AnalyzeOptions {
        scene_change: if threshold > 0.0 {
            threshold
        } else {
            SCENE_CHANGE
        },
        refine_cuts: true,
        ..AnalyzeOptions::default()
    };
    Ok(analyze(subject, tool, sequence, &options)?.scene_cuts)
}

fn black_runs(samples: &[FrameStat], options: &AnalyzeOptions, end: Time) -> Vec<Span> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for (index, sample) in samples.iter().enumerate() {
        let black = sample.luma <= options.black_luma;
        match (black, start) {
            (true, None) => start = Some(index),
            (false, Some(first)) => {
                runs.push(black_span(samples, first, index - 1, options.every, end));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(first) = start {
        runs.push(black_span(
            samples,
            first,
            samples.len() - 1,
            options.every,
            end,
        ));
    }
    runs
}

fn black_span(samples: &[FrameStat], first: usize, last: usize, every: Time, end: Time) -> Span {
    Span::new(samples[first].at, (samples[last].at + every).min(end))
}

/// Runs of samples that did not change. The reported span starts at the *previous* sample,
/// because "nothing changed between 3 s and 4 s" means the picture has been still since 3 s.
fn frozen_runs(samples: &[FrameStat], options: &AnalyzeOptions) -> Vec<Span> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for index in 1..samples.len() {
        let sample = &samples[index];
        let still = sample.change < options.freeze_change && sample.luma > options.black_luma;
        match (still, start) {
            (true, None) => start = Some(index),
            (false, Some(first)) => {
                push_freeze(&mut runs, samples, first, index - 1, options.min_freeze);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(first) = start {
        push_freeze(
            &mut runs,
            samples,
            first,
            samples.len() - 1,
            options.min_freeze,
        );
    }
    runs
}

fn push_freeze(
    runs: &mut Vec<Span>,
    samples: &[FrameStat],
    first: usize,
    last: usize,
    min_freeze: Time,
) {
    let span = Span::new(samples[first - 1].at, samples[last].at);
    if span.duration() >= min_freeze {
        runs.push(span);
    }
}

fn flashes(samples: &[FrameStat], jump: f32) -> Vec<Time> {
    let mut out = Vec::new();
    for index in 1..samples.len().saturating_sub(1) {
        let before = samples[index - 1].luma;
        let here = samples[index].luma;
        let after = samples[index + 1].luma;
        let (down, up) = (here - before, here - after);
        // Same sign on both sides: the level went somewhere and came back, rather than
        // stepping to a new shot and staying there.
        if down.signum() == up.signum() && down.abs() > jump && up.abs() > jump {
            out.push(samples[index].at);
        }
    }
    out
}

fn cuts(
    comp: &mut Compositor<'_>,
    samples: &[FrameStat],
    options: &AnalyzeOptions,
    fps: Fps,
) -> Result<Vec<Time>> {
    let mut out = Vec::new();
    for index in 1..samples.len() {
        if f64::from(samples[index].change) <= options.scene_change {
            continue;
        }
        let at = if options.refine_cuts {
            refine_cut(
                comp,
                samples[index - 1].frame,
                samples[index].frame,
                options.scene_change,
                fps,
            )?
        } else {
            samples[index].at
        };
        if out.last() != Some(&at) {
            out.push(at);
        }
    }
    Ok(out)
}

/// Narrow a cut known to be in `(before, after]` down to the frame it happens on.
///
/// Bisection compares the midpoint against the *interval start* rather than against its own
/// predecessor: "has the picture changed yet by here?" is monotone across a single cut,
/// while "did it change at this exact frame?" is not, so only the former can be searched.
pub fn refine_cut(
    comp: &mut Compositor<'_>,
    before: i64,
    after: i64,
    threshold: f64,
    fps: Fps,
) -> Result<Time> {
    let mut low = before;
    let mut high = after;
    if high <= low + 1 {
        return Ok(Time::from_frames(high, fps));
    }
    let mut anchor = comp.frame(low)?;
    while high - low > 1 {
        let mid = low + (high - low) / 2;
        let frame = comp.frame(mid)?;
        if f64::from(mean_abs_diff(&anchor, &frame)) > threshold {
            high = mid;
        } else {
            low = mid;
            anchor = frame;
        }
    }
    Ok(Time::from_frames(high, fps))
}

/// Silence spans of a sequence's mix, in timeline time.
///
/// Thin wrapper over [`dvs_audio::detect_silence`] that owns the mixing, so the digest, the
/// `silence-gap` lint and the `inspect.silence` op cannot disagree about what was measured.
pub fn silences<'a>(
    subject: impl Into<Subject<'a>>,
    tool: &'a Toolchain,
    sequence: &SequenceId,
    threshold_db: f64,
    min_duration: Time,
) -> Result<Vec<Span>> {
    let subject = subject.into();
    let seq = subject.project.sequence(sequence)?;
    let spec = dvs_audio::MixSpec::of(seq);
    let span = Span::new(Time::ZERO, seq.duration());
    if span.is_empty() {
        return Ok(Vec::new());
    }
    let samples = dvs_audio::mix_span(
        subject.project,
        sequence,
        span,
        spec,
        tool,
        subject.assets,
        subject.paths,
    )?;
    Ok(dvs_audio::detect_silence(
        &samples,
        spec.rate,
        spec.channels,
        threshold_db,
        min_duration,
    ))
}

/// The mixed samples of a whole sequence plus the format they are in.
///
/// Both the digest's audio block and the loudness lints need this, and mixing twice for one
/// report would double the most expensive part of an inspection.
pub(crate) fn mix_all(
    subject: Subject<'_>,
    tool: &Toolchain,
    sequence: &SequenceId,
) -> Result<(Vec<f32>, dvs_audio::MixSpec)> {
    let seq = subject.project.sequence(sequence)?;
    let spec = dvs_audio::MixSpec::of(seq);
    let span = Span::new(Time::ZERO, seq.duration());
    if span.is_empty() {
        return Ok((Vec::new(), spec));
    }
    let samples = dvs_audio::mix_span(
        subject.project,
        sequence,
        span,
        spec,
        tool,
        subject.assets,
        subject.paths,
    )?;
    Ok((samples, spec))
}

/// Reject a negative sampling interval.
///
/// Zero is legal and means "every frame" — expensive, but sometimes exactly what is wanted
/// (single-frame flash hunting, a golden test), so [`instants`] honours it rather than
/// clamping. A negative interval is never a request, only a sign error, and `instants`
/// would silently substitute one frame; saying so is more useful than quietly doing
/// something else.
pub(crate) fn require_interval(every: Time, what: &str) -> Result<()> {
    if every.is_negative() {
        return Err(Error::bad_args(format!(
            "{what} cannot be negative, got {every}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::*;
    use dvs_core::color::Rgba;

    const RED: Rgba = Rgba::opaque(200, 40, 40);
    const BLUE: Rgba = Rgba::opaque(30, 60, 200);

    fn options() -> AnalyzeOptions {
        AnalyzeOptions {
            scale: 1.0,
            use_proxy: false,
            ..AnalyzeOptions::default()
        }
    }

    #[test]
    fn a_difference_is_normalized_per_channel_so_the_thresholds_mean_something() {
        let black = Frame::filled(8, 8, Rgba::BLACK);
        let white = Frame::filled(8, 8, Rgba::WHITE);
        assert_eq!(mean_abs_diff(&black, &black), 0.0);
        // Black against white is the largest difference two frames can have; anything other
        // than 1.0 means the per-channel normalization is off by a factor and every
        // threshold in this module is scaled wrong.
        assert!(
            (mean_abs_diff(&black, &white) - 1.0).abs() < 1e-6,
            "expected 1.0, got {}",
            mean_abs_diff(&black, &white)
        );
    }

    #[test]
    fn sampling_lands_on_the_frame_grid_and_never_past_the_end() {
        let range = Span::new(Time::ZERO, Time::from_secs(1));
        let every = secs(1, 3);
        let taken = instants(range, every, fps());
        assert_eq!(taken.len(), 3, "three thirds of a second");
        for at in &taken {
            assert!(at.is_frame_aligned(fps()), "{at} is not on the frame grid");
            assert!(*at < range.end, "{at} is past the end of the range");
        }
        // An interval finer than a frame cannot produce more samples than there are frames.
        let dense = instants(range, secs(1, 1000), fps());
        assert_eq!(dense.len(), 30);
    }

    #[test]
    fn a_cut_is_found_at_the_frame_it_happens_on_not_at_the_sample_that_saw_it() {
        let mut fixture = Fixture::new([320, 180]);
        let track = fixture.video();
        // The cut sits at 2.5 s, between the samples at 2 s and 3 s: a detector that
        // reported the sample instant would answer 3 s, which is 15 frames out.
        fixture.push(&track, color_clip(RED, Time::ZERO, secs(5, 2)));
        fixture.push(&track, color_clip(BLUE, secs(5, 2), secs(3, 2)));
        let sequence = fixture.seq();
        let cuts = analyze(&fixture.ws, tool(), &sequence, &options())
            .expect("analyze")
            .scene_cuts;
        assert_eq!(cuts, vec![secs(5, 2)], "one cut, at 2.5 s exactly");
    }

    #[test]
    fn a_continuous_shot_has_no_cuts() {
        let mut fixture = Fixture::new([320, 180]);
        let track = fixture.video();
        fixture.push(&track, color_clip(RED, Time::ZERO, secs(4, 1)));
        let sequence = fixture.seq();
        // Through the published entry point, which is what `inspect.scenes` and the CLI use.
        let cuts = scenes(&fixture.ws, tool(), &sequence, 0.0).expect("scenes");
        assert!(
            cuts.is_empty(),
            "a single flat clip has no shot changes, got {cuts:?}"
        );
    }

    #[test]
    fn a_black_stretch_is_bounded_by_where_the_blackness_actually_is() {
        let mut fixture = Fixture::new([320, 180]);
        let track = fixture.video();
        fixture.push(&track, color_clip(RED, Time::ZERO, secs(2, 1)));
        fixture.push(&track, color_clip(Rgba::BLACK, secs(2, 1), secs(2, 1)));
        fixture.push(&track, color_clip(RED, secs(4, 1), secs(2, 1)));
        let sequence = fixture.seq();
        let analysis = analyze(&fixture.ws, tool(), &sequence, &options()).expect("analyze");
        assert_eq!(analysis.black_ranges.len(), 1, "one black stretch");
        let span = analysis.black_ranges[0];
        assert!(
            span.start >= secs(2, 1) && span.end <= secs(4, 1),
            "black range {span} must not spill into the coloured clips"
        );
        assert!(
            analysis
                .frozen_ranges
                .iter()
                .all(|frozen| frozen.start < secs(2, 1) || frozen.end > secs(4, 1)),
            "a black hole is reported as black, not as a freeze: {:?}",
            analysis.frozen_ranges
        );
    }

    #[test]
    fn silence_is_measured_over_the_whole_timeline() {
        let mut fixture = Fixture::new([320, 180]);
        let track = fixture.tone_track("head.wav", Span::new(Time::ZERO, secs(1, 1)), 0.5);
        let path = fixture.tone("tail.wav", 1.0, 0.5);
        let asset = fixture.import(&path, "tail.wav");
        fixture.push(
            &track,
            dvs_core::project::Clip::new(
                dvs_core::project::Source::Asset { asset, stream: None },
                secs(3, 1),
                secs(1, 1),
            ),
        );
        let sequence = fixture.seq();
        let found = silences(&fixture.ws, tool(), &sequence, -45.0, secs(1, 4)).expect("silence");
        assert_eq!(found.len(), 1, "one hole, got {found:?}");
        let hole = found[0];
        assert!(
            (hole.start.as_secs_f64() - 1.0).abs() < 0.05
                && (hole.end.as_secs_f64() - 3.0).abs() < 0.05,
            "the hole is 1 s to 3 s, got {hole}"
        );
    }
}
