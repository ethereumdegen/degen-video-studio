//! The digest: one JSON object that stands in for watching the video.
//!
//! `PLAN.md` §5 fixes the shape and this module fills it. The rule behind every field is
//! the same: report what an editor would *check* after making a cut, as a number or an id,
//! never as prose. Where V1 has holes. Whether the lower third overflowed. Whether the mix
//! is at −14 LUFS. Where the shot changes are. What the captions' reading rate came out at.
//!
//! Three choices shape the implementation.
//!
//! **Sampling, not rendering.** Frames are composited every
//! [`DigestOptions::sample_every`] (1 s by default) at half scale. A per-frame digest of a
//! ten-minute 1080p timeline is 18 000 composites — the same work as the export it is
//! supposed to describe, which would make "render, then check" cost double. The interval is
//! in the output (`sampleEvery`) so a reader can tell "no flashes" from "not looked for
//! closely enough".
//!
//! **Titles are measured against the real backdrop.** Contrast is the reason. A white
//! lower third is perfect over a dark shot and invisible over a bright one, and nothing in
//! the document says which it is — only the composited pixels underneath do. So for each
//! title the frame is rendered a second time with that clip disabled, and the ink is
//! compared against what is actually behind it at that instant.
//!
//! **The document half is exact, the pixel half is sampled.** Clip ranges, gaps, source
//! ranges and upscale ratios come from the document and are true for every frame. Black,
//! frozen, scene-cut and title measurements are observations at sampled instants. Keeping
//! that distinction visible is what stops an agent trusting a number more than it should.

use crate::analyze::{self, AnalyzeOptions, VideoAnalysis};
use dvs_comp::geometry::{self, Placement, Rect};
use dvs_comp::title::{Rasterizer, TextReport};
use crate::Subject;
use dvs_core::error::{Error, Result};
use dvs_core::ids::{ClipId, MarkerId, SequenceId, TrackId};
use dvs_core::op::Warning;
use dvs_core::project::{CaptionStyle, Clip, Fit, Project, Source, Track, TrackKind};
use dvs_core::time::{Fps, Span, Time, R};
use dvs_media::{Frame, Toolchain};
use serde::Serialize;

/// Fraction of the frame a title is expected to stay inside. Matches
/// `CaptionStyle::safe_area`'s default; titles carry no style of their own, and 90% is the
/// broadcast action-safe convention every delivery spec still uses.
pub const TITLE_SAFE_AREA: f32 = 0.9;

/// Level below which mixed audio counts as silence, in dBFS. Room tone and a noise floor
/// sit above this; a muted or missing clip sits far below it.
pub const SILENCE_THRESHOLD_DB: f64 = -45.0;

/// Shortest silence worth naming. Below half a second it is a breath, not a hole.
pub const MIN_SILENCE: Time = Time::from_ratio(R::new_raw(1, 2));

/// How thoroughly to inspect.
#[derive(Debug, Clone)]
pub struct DigestOptions {
    /// Interval between analysed frames. One second finds everything a viewer notices;
    /// pass a frame duration when hunting single-frame artifacts, and expect it to cost as
    /// much as a render.
    pub sample_every: Time,
    /// Render scale for the sampled frames.
    pub scale: f64,
    /// Decode from proxies where they exist.
    pub use_proxy: bool,
    /// Mix and measure the audio. Off skips the most expensive part of a digest for a
    /// picture-only question.
    pub with_audio: bool,
}

impl Default for DigestOptions {
    fn default() -> Self {
        DigestOptions {
            sample_every: Time::from_secs(1),
            scale: 0.5,
            use_proxy: true,
            with_audio: true,
        }
    }
}

impl DigestOptions {
    /// The analysis settings implied by these options.
    pub(crate) fn analyze(&self) -> AnalyzeOptions {
        AnalyzeOptions {
            every: self.sample_every,
            scale: self.scale,
            use_proxy: self.use_proxy,
            ..AnalyzeOptions::default()
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Digest {
    pub sequence: SequenceId,
    pub name: String,
    pub duration: Time,
    pub fps: Fps,
    pub size: [u32; 2],
    /// Interval the pixel and audio observations were taken at.
    pub sample_every: Time,
    pub tracks: Vec<TrackDigest>,
    /// Holes no other track of the same kind covers — what a viewer would see as black or
    /// hear as silence. A lower third's absence between titles is not one of these.
    pub gaps: Vec<GapDigest>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub markers: Vec<MarkerDigest>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub titles: Vec<TitleDigest>,
    pub captions: CaptionDigest,
    /// `None` when audio analysis was switched off or the sequence has no audio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioDigest>,
    pub video: VideoDigest,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackDigest {
    pub id: TrackId,
    pub name: String,
    pub kind: TrackKind,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub muted: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    pub clips: Vec<ClipDigest>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipDigest {
    pub id: ClipId,
    pub name: String,
    /// Timeline `[start, end)`.
    pub range: [Time; 2],
    /// What it shows, as the id or name the selector grammar accepts.
    pub source: String,
    /// Source `[in, out)`; absent for sources with no timeline of their own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_range: Option<[Time; 2]>,
    pub fitted: Fit,
    /// Destination pixels per source pixel. Above 1.0 the clip is being blown up past its
    /// native resolution, which is the `upscaled` lint.
    pub upscale: f64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GapDigest {
    pub track: TrackId,
    pub name: String,
    pub range: [Time; 2],
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkerDigest {
    pub id: MarkerId,
    pub at: Time,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TitleDigest {
    /// The clip, not the title document: an agent fixes the clip.
    pub id: ClipId,
    pub title: String,
    /// The instant the measurement was taken at.
    pub at: Time,
    pub text: String,
    /// `[x, y, width, height]` of the text, in output pixels.
    pub bbox: [f32; 4],
    pub font_size: f32,
    pub overflow: bool,
    pub in_safe_area: bool,
    /// WCAG contrast ratio of the ink against the composited pixels behind it, 1.0–21.0.
    /// `None` when the run rendered no ink to measure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contrast_vs_backdrop: Option<f64>,
    /// The family actually used, when the requested one was not installed.
    pub font_fallback: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptionDigest {
    pub cues: usize,
    /// Worst reading rate in characters per second.
    pub max_cps: f64,
    pub overlaps: usize,
    /// Cues exceeding their style's line budget.
    pub too_long: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDigest {
    pub integrated_lufs: f64,
    pub true_peak_db: f64,
    pub lra: f64,
    pub short_term_min_lufs: f64,
    pub silences: Vec<[Time; 2]>,
    pub clipped_samples: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoDigest {
    pub black_ranges: Vec<[Time; 2]>,
    pub frozen_ranges: Vec<[Time; 2]>,
    pub scene_cuts: Vec<Time>,
    pub flashes: Vec<Time>,
    /// Frames actually composited for this report.
    pub samples: usize,
}

fn range(span: Span) -> [Time; 2] {
    [span.start, span.end]
}

/// Describe a sequence: document structure, sampled pixels, measured audio.
///
/// Takes `&Workspace` or a [`Subject`]; the latter is what an op has, since `Op::apply` is
/// handed a `Project` and an `OpCx` rather than an open workspace.
pub fn digest<'a>(
    subject: impl Into<Subject<'a>>,
    tool: &'a Toolchain,
    sequence: &SequenceId,
    options: &DigestOptions,
) -> Result<Digest> {
    analyze::require_interval(options.sample_every, "sample-every")?;
    let subject = subject.into();
    let project = subject.project;
    let seq = project.sequence(sequence)?;
    let duration = seq.duration();
    let mut warnings: Vec<Warning> = Vec::new();

    let tracks = seq
        .tracks
        .iter()
        .map(|track| track_digest(project, track, seq.size))
        .collect::<Result<Vec<_>>>()?;

    let mut gaps = Vec::new();
    for track in &seq.tracks {
        // A caption track has no clips and a hole in it is not a hole in anything.
        if track.kind == TrackKind::Caption || track.clips.is_empty() {
            continue;
        }
        for hole in seq.uncovered_gaps(track) {
            warnings.push(Warning {
                code: "gap",
                // Bare id: `trk_x@18/1` reads well and resolves to nothing, because a
                // time window matches clips, and a gap is precisely where no clip is. The
                // range lives in the detail and in `gaps[]`.
                target: track.id.to_string(),
                detail: format!(
                    "{:.3}s of {} on {} between {} and {}",
                    hole.duration().as_secs_f64(),
                    if track.kind == TrackKind::Video {
                        "black"
                    } else {
                        "silence"
                    },
                    track.name,
                    hole.start,
                    hole.end
                ),
            });
            gaps.push(GapDigest {
                track: track.id.clone(),
                name: track.name.clone(),
                range: range(hole),
            });
        }
    }

    let captions = caption_digest(project, seq);
    if captions.overlaps > 0 {
        warnings.push(Warning {
            code: "caption-overlap",
            target: "track[kind=caption]".to_string(),
            detail: format!("{} overlapping caption cue(s)", captions.overlaps),
        });
    }

    let analysis = analyze::analyze(subject, tool, sequence, &options.analyze())?;
    let video = video_digest(&analysis);
    for span in &analysis.black_ranges {
        warnings.push(Warning {
            code: "black-frames",
            target: picture_target(seq, *span),
            detail: format!(
                "{:.3}s of black picture between {} and {}",
                span.duration().as_secs_f64(),
                span.start,
                span.end
            ),
        });
    }
    for span in &analysis.frozen_ranges {
        warnings.push(Warning {
            code: "frozen-frames",
            target: picture_target(seq, *span),
            detail: format!(
                "picture does not change for {:.3}s between {} and {}",
                span.duration().as_secs_f64(),
                span.start,
                span.end
            ),
        });
    }
    for at in &analysis.flashes {
        warnings.push(Warning {
            code: "flash",
            target: picture_target(seq, Span::new(*at, *at + seq.fps.frame_duration())),
            detail: format!("luminance spikes and returns within one sample at {at}"),
        });
    }

    let titles = title_digests(subject, tool, sequence, options)?;
    for title in &titles {
        if title.overflow {
            warnings.push(Warning {
                code: "title-overflow",
                target: title.id.to_string(),
                detail: format!("'{}' extends past the frame", title.text),
            });
        }
        if let Some(contrast) = title.contrast_vs_backdrop {
            if contrast < crate::lint::MIN_CONTRAST {
                warnings.push(Warning {
                    code: "low-contrast",
                    target: title.id.to_string(),
                    detail: format!(
                        "'{}' is at {contrast:.1}:1 against what is behind it",
                        title.text
                    ),
                });
            }
        }
    }

    let audio = if options.with_audio && has_audio(project, sequence, seq) {
        let block = audio_digest(subject, tool, sequence)?;
        if block.clipped_samples > 0 {
            warnings.push(Warning {
                code: "clipping",
                target: sequence.to_string(),
                detail: format!("{} samples at or past full scale", block.clipped_samples),
            });
        }
        Some(block)
    } else {
        None
    };

    Ok(Digest {
        sequence: sequence.clone(),
        name: seq.name.clone(),
        duration,
        fps: seq.fps,
        size: seq.size,
        sample_every: options.sample_every,
        tracks,
        gaps,
        markers: seq
            .markers
            .iter()
            .map(|marker| MarkerDigest {
                id: marker.id.clone(),
                at: marker.at,
                name: marker.name.clone(),
            })
            .collect(),
        titles,
        captions,
        audio,
        video,
        warnings,
    })
}

/// Whether the mix has anything in it.
///
/// Delegates to the mixer's own predicate rather than looking for audio *tracks*: a clip on
/// a video track plays its asset's sound, so the narrow question reports "no audio" for the
/// commonest timeline there is — one talking head on V1.
fn has_audio(project: &Project, sequence: &SequenceId, seq: &dvs_core::project::Sequence) -> bool {
    dvs_audio::has_audio(project, sequence, Span::new(Time::ZERO, seq.duration())).unwrap_or(false)
}

fn track_digest(project: &Project, track: &Track, seq_size: [u32; 2]) -> Result<TrackDigest> {
    let clips = track
        .clips
        .iter()
        .map(|clip| clip_digest(project, clip, seq_size))
        .collect::<Result<Vec<_>>>()?;
    Ok(TrackDigest {
        id: track.id.clone(),
        name: track.name.clone(),
        kind: track.kind,
        muted: track.muted,
        hidden: track.hidden,
        clips,
    })
}

fn clip_digest(project: &Project, clip: &Clip, seq_size: [u32; 2]) -> Result<ClipDigest> {
    // A clip whose source cannot be sized (a missing asset) still belongs in the digest —
    // that is precisely the case a reader needs to see — so the ratio degrades to 1.0
    // rather than failing the whole report.
    let upscale = upscale_ratio(project, clip, seq_size).unwrap_or(1.0);
    Ok(ClipDigest {
        id: clip.id.clone(),
        name: clip.label().to_string(),
        range: range(clip.span()),
        source: clip.source.describe(),
        source_range: source_timeline(clip).map(range),
        fitted: clip.fit,
        upscale,
        effects: clip.effects.iter().map(|fx| fx.kind.clone()).collect(),
        transition: clip
            .transition_in
            .as_ref()
            .map(|t| format!("{:?}", t.kind).to_lowercase()),
        speed: (!clip.speed.is_one()).then(|| clip.speed.to_string()),
        disabled: !clip.enabled,
    })
}

/// The source span a clip consumes, for sources that have one. A color, a title and a
/// generator are synthesised per frame and have no in/out point to report.
fn source_timeline(clip: &Clip) -> Option<Span> {
    match &clip.source {
        Source::Asset { .. } | Source::Sequence { .. } => Some(clip.source_span()),
        Source::Image { .. }
        | Source::Title { .. }
        | Source::Color { .. }
        | Source::Generator { .. } => None,
    }
}

/// Source size in its own pixels.
///
/// This mirrors `Compositor::source_size`, which is private to `dvs-comp`. The duplication
/// is deliberate rather than an omission: the compositor's copy runs per frame inside the
/// render loop, this one runs once per clip over a document with no toolchain and no
/// decoders, and the two answers are checked against each other by the `upscaled` lint
/// agreeing with `LayerReport::scale`.
pub(crate) fn source_size(
    project: &Project,
    clip: &Clip,
    seq_size: [u32; 2],
) -> Result<[u32; 2]> {
    match &clip.source {
        Source::Asset { asset, .. } | Source::Image { asset } => {
            let asset = project.asset(asset)?;
            let stream = asset.probe.video.as_ref().ok_or_else(|| {
                Error::op(format!(
                    "clip '{}' uses '{}', which has no video stream",
                    clip.label(),
                    asset.name
                ))
            })?;
            Ok(stream.display_size())
        }
        Source::Title { title } => Ok(project
            .titles
            .get(title)
            .ok_or_else(|| Error::no_match("title", title.as_str(), Vec::new()))?
            .size),
        Source::Sequence { sequence } => Ok(project.sequence(sequence)?.size),
        Source::Color { .. } | Source::Generator { .. } => Ok(seq_size),
    }
}

/// Where a clip's pixels land, using its static transform.
///
/// Keyframed transforms are evaluated per frame by the compositor; a document-level report
/// describes the clip as authored, and the sampled frame reports show what animation did to
/// it.
pub(crate) fn placement(
    project: &Project,
    clip: &Clip,
    seq_size: [u32; 2],
) -> Result<Placement> {
    let source = source_size(project, clip, seq_size)?;
    let (cropped, _) = geometry::cropped_size(source, clip.crop.as_ref());
    Ok(geometry::place(
        cropped,
        seq_size,
        clip.fit,
        &clip.transform,
        clip.transform.scale,
    ))
}

/// Destination pixels per native source pixel: `> 1.0` is an upscale.
pub(crate) fn upscale_ratio(project: &Project, clip: &Clip, seq_size: [u32; 2]) -> Result<f64> {
    let native = source_size(project, clip, seq_size)?;
    let dest = placement(project, clip, seq_size)?.dest;
    if native[0] == 0 {
        return Ok(1.0);
    }
    Ok(f64::from(dest.w) / f64::from(native[0]))
}

fn caption_digest(project: &Project, seq: &dvs_core::project::Sequence) -> CaptionDigest {
    let mut digest = CaptionDigest::default();
    let fallback = CaptionStyle::named("default");
    for track in &seq.tracks {
        if track.kind != TrackKind::Caption {
            continue;
        }
        let mut previous_end: Option<Time> = None;
        for cue in &track.cues {
            digest.cues += 1;
            let style = cue
                .style
                .as_ref()
                .or(track.style.as_ref())
                .and_then(|id| project.styles.get(id))
                .unwrap_or(&fallback);
            let cps = cue.chars_per_second();
            if cps.is_finite() {
                digest.max_cps = digest.max_cps.max(cps);
            }
            if cue.lines() > style.max_lines {
                digest.too_long += 1;
            }
            if previous_end.is_some_and(|end| cue.span.start < end) {
                digest.overlaps += 1;
            }
            previous_end = Some(match previous_end {
                Some(end) => end.max(cue.span.end),
                None => cue.span.end,
            });
        }
    }
    digest
}

/// A resolvable target for a finding about the picture over a time range.
///
/// Clips in that window when there are any — those are what an agent would edit. When the
/// window is empty of clips, which is exactly the case for black frames caused by a hole,
/// the video tracks are the thing that exists; `clip@…` there would parse and match
/// nothing, sending a loop chasing a target it can never act on.
fn picture_target(seq: &dvs_core::project::Sequence, span: Span) -> String {
    let covered = seq
        .tracks
        .iter()
        .filter(|track| track.kind == TrackKind::Video)
        .flat_map(|track| track.clips.iter())
        .any(|clip| clip.enabled && clip.span().overlaps(&span));
    if covered {
        format!("clip@{}-{}", span.start, span.end)
    } else {
        "track[kind=video]".to_string()
    }
}

fn video_digest(analysis: &VideoAnalysis) -> VideoDigest {
    VideoDigest {
        black_ranges: analysis.black_ranges.iter().copied().map(range).collect(),
        frozen_ranges: analysis.frozen_ranges.iter().copied().map(range).collect(),
        scene_cuts: analysis.scene_cuts.clone(),
        flashes: analysis.flashes.clone(),
        samples: analysis.samples.len(),
    }
}

fn audio_digest(
    subject: Subject<'_>,
    tool: &Toolchain,
    sequence: &SequenceId,
) -> Result<AudioDigest> {
    let (samples, spec) = analyze::mix_all(subject, tool, sequence)?;
    let loudness = dvs_audio::analyze_loudness(&samples, spec.rate, spec.channels)?;
    let silences = dvs_audio::detect_silence(
        &samples,
        spec.rate,
        spec.channels,
        SILENCE_THRESHOLD_DB,
        MIN_SILENCE,
    );
    Ok(AudioDigest {
        integrated_lufs: loudness.integrated_lufs,
        true_peak_db: loudness.true_peak_db,
        lra: loudness.lra,
        short_term_min_lufs: loudness.short_term_min_lufs,
        silences: silences.into_iter().map(range).collect(),
        clipped_samples: loudness.clipped_samples,
    })
}

/// One title clip's text, as rendered, with the backdrop it sits on.
pub(crate) struct TitleMeasurement {
    pub clip: ClipId,
    pub title: String,
    pub at: Time,
    /// Frame size the measurement was taken in — the analysis raster, not the sequence.
    pub frame_size: [u32; 2],
    /// Multiplier from measurement pixels back to sequence pixels.
    pub to_sequence: f32,
    pub reports: Vec<PlacedText>,
}

/// A text run in output-frame coordinates, with the contrast it achieves.
pub(crate) struct PlacedText {
    pub text: String,
    pub bbox: [f32; 4],
    pub font_size: f32,
    pub fallback: Option<String>,
    pub contrast: Option<f64>,
}

impl PlacedText {
    pub fn overflows(&self, frame: [u32; 2]) -> bool {
        let [x, y, w, h] = self.bbox;
        x < -0.5 || y < -0.5 || x + w > frame[0] as f32 + 0.5 || y + h > frame[1] as f32 + 0.5
    }

    pub fn in_safe_area(&self, frame: [u32; 2], safe: f32) -> bool {
        let safe = safe.clamp(0.1, 1.0);
        let (fw, fh) = (frame[0] as f32, frame[1] as f32);
        let (margin_x, margin_y) = (fw * (1.0 - safe) / 2.0, fh * (1.0 - safe) / 2.0);
        let [x, y, w, h] = self.bbox;
        x >= margin_x - 0.5
            && y >= margin_y - 0.5
            && x + w <= fw - margin_x + 0.5
            && y + h <= fh - margin_y + 0.5
    }
}

/// Title clips of a sequence and the instant each is measured at.
///
/// The midpoint, not the start: the start is usually inside a `transitionIn`, where the
/// title is half faded and the backdrop is a mix of two shots, so a contrast measured there
/// describes a frame nobody looks at.
fn title_clips(seq: &dvs_core::project::Sequence) -> Vec<(ClipId, Time)> {
    seq.tracks
        .iter()
        .filter(|track| track.kind != TrackKind::Audio)
        .flat_map(|track| track.clips.iter())
        .filter(|clip| clip.enabled)
        .filter_map(|clip| match &clip.source {
            Source::Title { .. } => Some((clip.id.clone(), midpoint(clip, seq.fps))),
            _ => None,
        })
        .collect()
}

/// Measure one title clip's text runs in frame coordinates.
///
/// `backdrop` is what is composited beneath the clip at that instant; without it the text
/// geometry is still exact and only the contrast is unknown, which is what lets
/// `lint --no-render` catch an overflowing title with no ffmpeg in the process.
fn measure_title(
    project: &Project,
    rasterizer: &Rasterizer,
    clip: &Clip,
    frame_size: [u32; 2],
    backdrop: Option<&Frame>,
    background: dvs_core::color::Rgba,
) -> Result<(String, Vec<PlacedText>)> {
    let Source::Title { title: title_id } = &clip.source else {
        return Ok((String::new(), Vec::new()));
    };
    let title = project
        .titles
        .get(title_id)
        .ok_or_else(|| Error::no_match("title", title_id.as_str(), Vec::new()))?;
    let svg = title.resolved_svg();
    let dest = placement(project, clip, frame_size)?.dest;
    let target = [
        (dest.w.round().max(2.0)) as u32,
        (dest.h.round().max(2.0)) as u32,
    ];
    let reports = rasterizer.text_reports(&svg, target)?;
    if reports.is_empty() {
        return Ok((title.name.clone(), Vec::new()));
    }
    let doc = rasterizer.document_size(&svg)?;
    let asked = requested_families(&svg);
    // The ink raster exists only to measure the text's own colour, which is only used for
    // contrast — so it is rasterized exactly when there is a backdrop to compare against.
    let raster = match backdrop {
        Some(_) => Some(rasterizer.rasterize(&svg, target)?),
        None => None,
    };
    let placed = reports
        .into_iter()
        .map(|report| {
            let mut placed = place_text(
                &report,
                doc,
                target,
                dest,
                raster.as_ref().zip(backdrop),
                background,
            );
            placed.fallback = placed
                .fallback
                .or_else(|| substituted(&asked, &report.requested));
            placed
        })
        .collect();
    Ok((title.name.clone(), placed))
}

/// CSS generic families. A document asking for one of these has, by definition, not asked
/// for a specific face, so a substitution is not a fallback worth reporting.
const GENERIC_FAMILIES: &[&str] = &[
    "sans-serif",
    "serif",
    "monospace",
    "cursive",
    "fantasy",
    "system-ui",
    "ui-sans-serif",
    "ui-serif",
    "ui-monospace",
    "math",
    "emoji",
];

/// Font families the SVG explicitly names, lowercased, generics dropped.
///
/// Scraped from the document rather than read off the rasterizer's report, and that is the
/// whole point: `usvg` resolves `font-family` at parse time and rewrites the span to the
/// family it actually found, so a run that asked for a font nobody has installed comes back
/// claiming it asked for the substitute. Comparing the report against what the document
/// says is the only way to see the substitution — and a silent substitution is exactly the
/// bug that makes a render come out different on another machine.
fn requested_families(svg: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let mut rest = svg;
    while let Some(index) = rest.find("font-family") {
        rest = &rest[index + "font-family".len()..];
        let value = rest
            .trim_start()
            .trim_start_matches(['=', ':'])
            .trim_start()
            .trim_start_matches(['"', '\'']);
        let end = value
            .find(['"', '\'', ';', '}', '<'])
            .unwrap_or(value.len());
        for family in value[..end].split(',') {
            let family = family.trim().trim_matches(['"', '\'']).trim();
            if family.is_empty() {
                continue;
            }
            let lowered = family.to_ascii_lowercase();
            if !GENERIC_FAMILIES.contains(&lowered.as_str()) {
                out.insert(lowered);
            }
        }
    }
    out
}

/// The family a run ended up in, when the document asked for something else.
fn substituted(
    asked: &std::collections::BTreeSet<String>,
    resolved: &[String],
) -> Option<String> {
    if asked.is_empty() {
        return None;
    }
    let used = resolved.first()?;
    (!asked.contains(&used.to_ascii_lowercase())).then(|| used.clone())
}

/// Title text geometry, measured in sequence pixels with no ffmpeg involvement.
///
/// Contrast is left unmeasured: it is the one title property that cannot be known from the
/// document, because it depends on the pixels underneath.
pub(crate) fn title_texts(
    project: &Project,
    sequence: &SequenceId,
) -> Result<Vec<TitleMeasurement>> {
    let seq = project.sequence(sequence)?;
    let clips = title_clips(seq);
    if clips.is_empty() {
        return Ok(Vec::new());
    }
    let rasterizer = Rasterizer::new();
    let mut out = Vec::with_capacity(clips.len());
    for (clip_id, at) in clips {
        let (_, clip) = seq
            .find_clip(&clip_id)
            .ok_or_else(|| Error::no_match("clip", clip_id.as_str(), seq.clip_ids()))?;
        let (title, reports) =
            measure_title(project, &rasterizer, clip, seq.size, None, seq.background)?;
        out.push(TitleMeasurement {
            clip: clip_id,
            title,
            at,
            frame_size: seq.size,
            to_sequence: 1.0,
            reports,
        });
    }
    Ok(out)
}

/// As [`title_texts`], plus each run's contrast against the composited backdrop.
pub(crate) fn title_measurements(
    subject: Subject<'_>,
    tool: &Toolchain,
    sequence: &SequenceId,
    options: &DigestOptions,
) -> Result<Vec<TitleMeasurement>> {
    let project = subject.project;
    let seq = project.sequence(sequence)?;
    let clips = title_clips(seq);
    if clips.is_empty() {
        return Ok(Vec::new());
    }

    let rasterizer = Rasterizer::new();
    // Built only to learn the raster size the render scale implies, so the text measurement
    // and the backdrop composite share one coordinate system.
    let frame_size =
        analyze::compositor(subject, tool, sequence, options.scale, options.use_proxy)?.size();
    // `Compositor::new` clamps its raster to at least 2 px, so this divides by a small
    // number at worst, never by zero.
    let to_sequence = seq.size[0] as f32 / frame_size[0].max(1) as f32;
    let mut out = Vec::with_capacity(clips.len());
    for (clip_id, at) in clips {
        let (_, clip) = seq
            .find_clip(&clip_id)
            .ok_or_else(|| Error::no_match("clip", clip_id.as_str(), seq.clip_ids()))?;
        let backdrop = backdrop_frame(subject, tool, sequence, &clip_id, at, options)?;
        let (title, reports) = measure_title(
            project,
            &rasterizer,
            clip,
            frame_size,
            Some(&backdrop),
            seq.background,
        )?;
        out.push(TitleMeasurement {
            clip: clip_id,
            title,
            at,
            frame_size,
            to_sequence,
            reports,
        });
    }
    Ok(out)
}

fn midpoint(clip: &Clip, fps: Fps) -> Time {
    let middle = clip.start + Time::from_ratio(clip.duration.ratio() / R::from_integer(2));
    Time::from_frames(middle.frame_floor(fps), fps)
}

/// Lift a text run from raster coordinates into frame coordinates and measure its contrast.
///
/// The rasterizer fits the document into the requested raster preserving aspect and centers
/// it, and `text_reports` reports bounding boxes in document space scaled by that same fit
/// factor — so recovering frame coordinates means adding the centering offset and the
/// clip's destination origin. Getting this wrong is silent: the numbers stay plausible and
/// every safe-area answer is wrong by a margin.
fn place_text(
    report: &TextReport,
    doc: [f32; 2],
    target: [u32; 2],
    dest: Rect,
    pixels: Option<(&Frame, &Frame)>,
    background: dvs_core::color::Rgba,
) -> PlacedText {
    let fit = if doc[0] > 0.0 && doc[1] > 0.0 {
        (target[0] as f32 / doc[0]).min(target[1] as f32 / doc[1])
    } else {
        1.0
    };
    let offset = [
        (target[0] as f32 - doc[0] * fit) / 2.0,
        (target[1] as f32 - doc[1] * fit) / 2.0,
    ];
    let raster_box = Rect {
        x: report.bbox[0] + offset[0],
        y: report.bbox[1] + offset[1],
        w: report.bbox[2],
        h: report.bbox[3],
    };
    let frame_box = [
        dest.x + raster_box.x,
        dest.y + raster_box.y,
        raster_box.w,
        raster_box.h,
    ];
    let contrast = pixels.and_then(|(raster, backdrop)| {
        contrast_of(raster, raster_box, backdrop, frame_box, background)
    });
    PlacedText {
        text: report.text.clone(),
        bbox: frame_box,
        font_size: report.font_size,
        fallback: report.fallback.clone(),
        contrast,
    }
}

/// WCAG contrast between a text run's ink and the pixels behind it.
///
/// Both luminances are relative luminance in linear light, which is what the WCAG ratio is
/// defined on — computing it on sRGB bytes, as most implementations accidentally do,
/// overstates the contrast of dark combinations by a factor of three.
fn contrast_of(
    raster: &Frame,
    ink_box: Rect,
    backdrop: &Frame,
    frame_box: [f32; 4],
    background: dvs_core::color::Rgba,
) -> Option<f64> {
    let ink = ink_luminance(raster, ink_box)?;
    let behind = backdrop_luminance(
        backdrop,
        Rect {
            x: frame_box[0],
            y: frame_box[1],
            w: frame_box[2],
            h: frame_box[3],
        },
        background,
    );
    let (high, low) = if ink >= behind {
        (ink, behind)
    } else {
        (behind, ink)
    };
    Some((high + 0.05) / (low + 0.05))
}

/// Mean luminance of the covered pixels in a region, un-premultiplied.
///
/// Only pixels with real coverage count: averaging in the transparent pixels around a
/// glyph would report the luminance of the text *box*, which for any normal title is mostly
/// the backdrop and would make every combination look like 1:1.
fn ink_luminance(raster: &Frame, region: Rect) -> Option<f64> {
    let (x0, y0, x1, y1) = region.bounds_in(raster.size());
    let mut sum = 0.0f64;
    let mut count = 0u64;
    for y in y0..y1 {
        for x in x0..x1 {
            let pixel = raster.pixel(x, y);
            if pixel[3] < 0.5 {
                continue;
            }
            let inv = 1.0 / pixel[3];
            sum += f64::from(
                0.2126 * pixel[0] * inv + 0.7152 * pixel[1] * inv + 0.0722 * pixel[2] * inv,
            );
            count += 1;
        }
    }
    (count > 0).then(|| sum / count as f64)
}

/// Mean luminance of a region of the composited backdrop, over the sequence background.
fn backdrop_luminance(backdrop: &Frame, region: Rect, background: dvs_core::color::Rgba) -> f64 {
    let back = background.to_linear_premul();
    let (x0, y0, x1, y1) = region.bounds_in(backdrop.size());
    let mut sum = 0.0f64;
    let mut count = 0u64;
    for y in y0..y1 {
        for x in x0..x1 {
            let pixel = backdrop.pixel(x, y);
            let open = 1.0 - pixel[3];
            let r = pixel[0] + back[0] * open;
            let g = pixel[1] + back[1] * open;
            let b = pixel[2] + back[2] * open;
            sum += f64::from(0.2126 * r + 0.7152 * g + 0.0722 * b);
            count += 1;
        }
    }
    if count == 0 {
        return f64::from(background.luminance());
    }
    sum / count as f64
}

/// Composite the frame that sits *under* a clip.
///
/// Rendered from a copy of the document with that one clip disabled, which is the only way
/// to answer "what is behind the title" without a second compositing path: the document is
/// small and cloning it is far cheaper than teaching the renderer to skip a layer.
fn backdrop_frame(
    subject: Subject<'_>,
    tool: &Toolchain,
    sequence: &SequenceId,
    clip: &ClipId,
    at: Time,
    options: &DigestOptions,
) -> Result<Frame> {
    let mut project = subject.project.clone();
    if let Some((track, index)) = project.sequence_mut(sequence)?.find_clip_mut(clip) {
        track.clips[index].enabled = false;
    }
    let seq = project.sequence(sequence)?;
    let mut comp = dvs_comp::Compositor::new(
        tool,
        &project,
        subject.paths,
        subject.assets,
        sequence,
        dvs_comp::CompOptions {
            scale: options.scale,
            use_proxy: options.use_proxy,
            scaler: "bilinear",
        },
    )?;
    let fps = seq.fps;
    comp.frame(at.frame_floor(fps))
}

/// Turn measurements into the digest's `titles` block.
pub(crate) fn title_digests(
    subject: Subject<'_>,
    tool: &Toolchain,
    sequence: &SequenceId,
    options: &DigestOptions,
) -> Result<Vec<TitleDigest>> {
    let mut out = Vec::new();
    for measurement in title_measurements(subject, tool, sequence, options)? {
        // Measurements are taken in the analysis raster, which is `scale` of the sequence.
        // The digest reports sequence pixels, because that is the coordinate system the
        // document — and therefore any fix an agent applies — is written in.
        let factor = measurement.to_sequence;
        for text in &measurement.reports {
            out.push(TitleDigest {
                id: measurement.clip.clone(),
                title: measurement.title.clone(),
                at: measurement.at,
                text: text.text.clone(),
                bbox: [
                    text.bbox[0] * factor,
                    text.bbox[1] * factor,
                    text.bbox[2] * factor,
                    text.bbox[3] * factor,
                ],
                font_size: text.font_size * factor,
                overflow: text.overflows(measurement.frame_size),
                in_safe_area: text.in_safe_area(measurement.frame_size, TITLE_SAFE_AREA),
                contrast_vs_backdrop: text.contrast,
                font_fallback: text.fallback.clone(),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::*;
    use dvs_core::color::Rgba;
    use dvs_core::project::{Clip, Source, TrackKind};
    use dvs_core::time::Rat;

    const RED: Rgba = Rgba::opaque(200, 40, 40);
    const BLUE: Rgba = Rgba::opaque(30, 60, 200);

    fn options() -> DigestOptions {
        DigestOptions {
            scale: 1.0,
            use_proxy: false,
            with_audio: false,
            ..DigestOptions::default()
        }
    }

    fn describe(fixture: &Fixture, options: &DigestOptions) -> Digest {
        let sequence = fixture.seq();
        digest(&fixture.ws, tool(), &sequence, options).expect("digest")
    }

    /// Every `target` an agent reads back has to be a selector it can feed to an op. A
    /// target that parses but matches nothing — `trk_x@18/1`, because a time window selects
    /// clips and a gap is exactly where no clip is — sends a loop chasing its own tail.
    fn assert_warning_targets_resolve(fixture: &Fixture, digest: &Digest) {
        let sequence = fixture.seq();
        assert!(!digest.warnings.is_empty(), "the fixture produced no warnings");
        for warning in &digest.warnings {
            let parsed = dvs_core::selector::Selector::parse(&warning.target).unwrap_or_else(
                |error| panic!("'{}' target '{}' is not a selector: {error}", warning.code, warning.target),
            );
            let matched = dvs_core::selector::resolve(&fixture.ws.project, &sequence, &parsed)
                .unwrap_or_else(|error| {
                    panic!(
                        "'{}' target '{}' resolves to nothing: {error}",
                        warning.code, warning.target
                    )
                });
            assert!(
                !matched.is_empty(),
                "'{}' target '{}' named nothing",
                warning.code,
                warning.target
            );
        }
    }

    #[test]
    fn a_hole_on_a_track_is_reported_and_filling_it_removes_the_report() {
        let mut fixture = Fixture::new([320, 180]);
        let track = fixture.video();
        fixture.push(&track, color_clip(RED, Time::ZERO, secs(2, 1)));
        let late = fixture.push(&track, color_clip(BLUE, secs(4, 1), secs(2, 1)));

        let holed = describe(&fixture, &options());
        assert_eq!(holed.gaps.len(), 1, "one hole, got {:?}", holed.gaps);
        assert_eq!(holed.gaps[0].range, [secs(2, 1), secs(4, 1)]);
        assert!(
            holed.warnings.iter().any(|warning| warning.code == "gap"
                && warning.target == track.to_string()),
            "the gap warning names the track it is on: {:?}",
            holed.warnings
        );
        assert_warning_targets_resolve(&fixture, &holed);

        // Slide the second clip back against the first: same document, no hole.
        let seq = fixture.sequence_mut();
        seq.track_mut(&track).expect("track").clips[1].start = secs(2, 1);
        let _ = late;
        let filled = describe(&fixture, &options());
        assert!(
            filled.gaps.is_empty(),
            "a filled track has no gaps, got {:?}",
            filled.gaps
        );
        assert!(
            !filled.warnings.iter().any(|warning| warning.code == "gap"),
            "and no gap warning"
        );
    }

    #[test]
    fn a_title_that_runs_off_the_frame_is_reported_as_overflowing() {
        let mut fixture = Fixture::new([320, 180]);
        let svg = text_title([320, 180], 250.0, 100.0, 40.0, "sans-serif", "#ffffff", "OVERFLOWING");
        let title = fixture.title("wide", [320, 180], svg);
        let track = fixture.video();
        fixture.push(
            &track,
            Clip::new(Source::Title { title }, Time::ZERO, secs(2, 1)),
        );
        let report = describe(&fixture, &options());
        let text = report.titles.first().expect("one measured text run");
        assert!(text.overflow, "bbox {:?} extends past 320x180", text.bbox);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.code == "title-overflow"),
            "and the digest warns about it"
        );
    }

    #[test]
    fn a_title_inside_the_safe_area_is_reported_clean() {
        let mut fixture = Fixture::new([320, 180]);
        let svg = text_title([320, 180], 100.0, 100.0, 40.0, "sans-serif", "#ffffff", "ok");
        let title = fixture.title("narrow", [320, 180], svg);
        let track = fixture.video();
        fixture.push(
            &track,
            Clip::new(Source::Title { title }, Time::ZERO, secs(2, 1)),
        );
        let report = describe(&fixture, &options());
        let text = report.titles.first().expect("one measured text run");
        assert!(!text.overflow, "bbox {:?} fits", text.bbox);
        assert!(text.in_safe_area, "bbox {:?} is inside the safe area", text.bbox);
        assert_eq!(text.font_fallback, None, "sans-serif resolves everywhere");
    }

    #[test]
    fn a_retimed_clip_reports_the_source_it_consumes_not_its_timeline_length() {
        let mut fixture = Fixture::new([320, 180]);
        let media = fixture.synth("talk.mp4", "testsrc2", 6, [320, 180]);
        let asset = fixture.import(&media, "talk.mp4");
        let track = fixture.video();
        let mut clip = Clip::new(Source::Asset { asset, stream: None }, Time::ZERO, secs(2, 1));
        clip.source_in = secs(1, 1);
        clip.speed = Rat::new(2, 1).expect("2x");
        fixture.push(&track, clip);
        let report = describe(&fixture, &options());
        let entry = &report.tracks[0].clips[0];
        assert_eq!(entry.range, [Time::ZERO, secs(2, 1)]);
        // Two seconds of timeline at double speed reads four seconds of source.
        assert_eq!(entry.source_range, Some([secs(1, 1), secs(5, 1)]));
        assert_eq!(entry.speed.as_deref(), Some("2/1"));
    }

    #[test]
    fn a_video_clips_own_sound_is_measured() {
        // No audio track at all: the sound lives on the V1 clip, which is what a camera
        // file is. Looking for audio *tracks* answered "no audio" for this timeline and
        // left the digest without the block an agent uses to check its mix.
        let mut fixture = Fixture::new([320, 180]);
        let media = fixture.av("talk.mp4", 2);
        let asset = fixture.import(&media, "talk.mp4");
        let track = fixture.video();
        fixture.push(
            &track,
            Clip::new(Source::Asset { asset, stream: None }, Time::ZERO, secs(2, 1)),
        );
        let report = describe(
            &fixture,
            &DigestOptions {
                with_audio: true,
                ..options()
            },
        );
        let audio = report
            .audio
            .expect("a video clip's own sound must be measured");
        assert!(
            audio.integrated_lufs.is_finite() && audio.integrated_lufs < 0.0,
            "expected a real programme loudness, got {}",
            audio.integrated_lufs
        );
    }

    #[test]
    fn the_serialized_shape_uses_the_field_names_the_plan_publishes() {
        let mut fixture = Fixture::new([320, 180]);
        let track = fixture.video();
        fixture.push(&track, color_clip(RED, Time::ZERO, secs(2, 1)));
        fixture.tone_track("bed.wav", Span::new(Time::ZERO, secs(2, 1)), 0.4);
        let captions = fixture.track(TrackKind::Caption);
        fixture.cue(&captions, Span::new(Time::ZERO, secs(2, 1)), "hello there");

        let report = describe(
            &fixture,
            &DigestOptions {
                with_audio: true,
                ..options()
            },
        );
        let json = serde_json::to_value(&report).expect("serialize");
        // These names are the contract an agent reads; renaming one silently breaks every
        // consumer, so they are pinned here rather than in prose.
        for path in [
            "sequence",
            "duration",
            "fps",
            "size",
            "sampleEvery",
            "tracks",
            "gaps",
            "captions",
            "audio",
            "video",
            "warnings",
        ] {
            assert!(json.get(path).is_some(), "digest is missing '{path}'");
        }
        for path in ["integratedLufs", "truePeakDb", "lra", "silences", "clippedSamples"] {
            assert!(
                json["audio"].get(path).is_some(),
                "audio block is missing '{path}'"
            );
        }
        for path in ["blackRanges", "frozenRanges", "sceneCuts"] {
            assert!(
                json["video"].get(path).is_some(),
                "video block is missing '{path}'"
            );
        }
        for path in ["cues", "maxCps", "overlaps"] {
            assert!(
                json["captions"].get(path).is_some(),
                "captions block is missing '{path}'"
            );
        }
        let clip = &json["tracks"][0]["clips"][0];
        for path in ["id", "range", "source", "fitted", "upscale"] {
            assert!(clip.get(path).is_some(), "clip entry is missing '{path}'");
        }
    }

    #[test]
    fn a_document_asking_for_a_font_nobody_has_reports_what_was_used_instead() {
        let mut fixture = Fixture::new([320, 180]);
        let svg = text_title(
            [320, 180],
            100.0,
            100.0,
            20.0,
            "No Such Face 9000",
            "#ffffff",
            "hi",
        );
        let title = fixture.title("borrowed", [320, 180], svg);
        let track = fixture.video();
        fixture.push(
            &track,
            Clip::new(Source::Title { title }, Time::ZERO, secs(2, 1)),
        );
        let report = describe(&fixture, &options());
        let text = report.titles.first().expect("one measured text run");
        let used = text
            .font_fallback
            .as_deref()
            .expect("a missing family must be reported as a substitution");
        assert_ne!(
            used.to_ascii_lowercase(),
            "no such face 9000",
            "the reported family is the one actually rendered"
        );
    }
}
