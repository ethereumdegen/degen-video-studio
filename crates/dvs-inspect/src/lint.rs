//! Lint: the specific mistakes an editor who cannot see the result makes.
//!
//! `PLAN.md` §5 names the rules; this module is them. Three properties are what make the
//! output usable rather than decorative.
//!
//! **Every finding carries a selector.** `Finding::target` is a term the selector grammar
//! accepts — a clip id, a track id, `track[kind=audio]@12/1-15/1` — so the next step is a
//! runnable op rather than a search. A diagnostic that says "there is a gap on the second
//! video track" costs a round trip; one that says `trk_01J8@18/1-18.5/1` does not.
//!
//! **Document rules need no ffmpeg.** With [`LintOptions::render`] off, everything that can
//! be decided from `project.json` and the font database still runs: gaps, overlaps, source
//! bounds, retiming, resolution, captions, store hygiene, and title geometry. That is the
//! pass an agent runs after every edit, and it costs milliseconds. Only the rules that
//! genuinely need pixels or samples — black, frozen, flash, contrast, loudness — are gated
//! behind `render`, and each is listed in [`RENDER_RULES`].
//!
//! **Severity is about the output, not about tidiness.** [`Severity::Error`] means the
//! render will be wrong or be rejected: clips overlapping, a clip past the end of its
//! source, a missing blob, text clipped by the frame edge, samples past full scale.
//! [`Severity::Warning`] means it will probably look or sound wrong. [`Severity::Info`] is
//! a fact worth knowing. The CLI maps any `Error` to exit code 4.
//!
//! The thresholds are constants with reasons, because a lint nobody believes is worse than
//! no lint: [`MIN_CONTRAST`] is WCAG AA, [`MIN_TYPE_FRACTION`] is the smallest type that
//! survives a phone screen and a 500 kbit/s re-encode, and the loudness targets come from
//! the platforms' published specs rather than from taste.

use crate::analyze::{self, AnalyzeOptions};
use crate::digest::{self, DigestOptions, TitleMeasurement, TITLE_SAFE_AREA};
use crate::Subject;
use dvs_core::error::Result;
use dvs_core::ids::{AssetId, SequenceId, TrackId};
use dvs_core::project::{
    CaptionStyle, Clip, Fit, Project, Sequence, Source, Track, TrackKind,
};
use dvs_core::time::{Span, Time, R};
use dvs_media::Toolchain;
use serde::{Deserialize, Serialize};

/// WCAG AA contrast for large text, which every title is. Below this, white-on-white and
/// its relatives are invisible to a viewer and undetectable to the agent that made them.
pub const MIN_CONTRAST: f64 = 4.5;

/// Smallest legible type, as a fraction of frame height. 2.2% is 24 px at 1080p — the floor
/// below which a caption stops surviving a phone screen and a low-bitrate re-encode.
pub const MIN_TYPE_FRACTION: f32 = 0.022;

/// Upscale ratio treated as native. A one-percent margin absorbs the rounding in
/// `1920/1080 * 1080` without letting a real 2× blow-up through.
pub const UPSCALE_TOLERANCE: f64 = 1.02;

/// Aspect mismatch, as a fraction, above which a contained clip is letterboxed.
pub const LETTERBOX_TOLERANCE: f64 = 0.01;

/// Longest silence that passes without comment.
pub const MAX_SILENCE: Time = Time::from_ratio(R::new_raw(2, 1));

/// How close two simultaneous audio tracks may be in loudness before the quieter one is
/// being buried. 6 LU is about the point where a music bed stops being a bed.
pub const DUCK_MARGIN_LU: f64 = 6.0;

/// Tolerance around a loudness target. Platforms normalise anyway; ±1 LU is the band inside
/// which correcting is pointless churn.
pub const LOUDNESS_TOLERANCE_LU: f64 = 1.0;

/// Integrated loudness below which a mix is treated as silent, so the loudness rules stay
/// quiet about a sequence that has no audible audio (the `silence-gap` rule owns that case).
const AUDIBLE_FLOOR_LUFS: f64 = -60.0;

/// Shortest overlap between two audio tracks worth measuring. Below a second the gated
/// loudness of the overlap is not defined.
const MIN_DUCK_OVERLAP: Time = Time::from_ratio(R::new_raw(1, 1));

/// Every rule this crate can emit, in reporting order. Exhaustive by contract: the test
/// suite asserts that each of these fires on one fixture and stays quiet on another.
pub const RULES: &[&str] = &[
    "gap",
    "overlap",
    "orphan-transition",
    "past-source-end",
    "speed-frame-drop",
    "fps-mismatch",
    "vfr-source",
    "upscaled",
    "letterboxed",
    "av-drift",
    "caption-too-fast",
    "caption-overlap",
    "caption-too-long",
    "unreferenced-asset",
    "missing-asset",
    "stale-proxy",
    "title-overflow",
    "unsafe-area",
    "font-fallback",
    "tiny-type",
    "low-contrast",
    "black-frames",
    "frozen-frames",
    "flash",
    "loudness-out-of-spec",
    "true-peak",
    "clipping",
    "silence-gap",
    "unducked-music",
];

/// Rules that need composited pixels or mixed samples, and therefore ffmpeg. Skipped when
/// [`LintOptions::render`] is off.
pub const RENDER_RULES: &[&str] = &[
    "low-contrast",
    "black-frames",
    "frozen-frames",
    "flash",
    "loudness-out-of-spec",
    "true-peak",
    "clipping",
    "silence-gap",
    "unducked-music",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// A fact worth knowing.
    Info,
    /// The output will probably be wrong.
    Warning,
    /// The output will be wrong or will be rejected. Ordered last so that
    /// `findings.iter().map(|f| f.severity).max()` is the exit decision.
    Error,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub rule: &'static str,
    pub severity: Severity,
    /// A selector naming exactly what to fix.
    pub target: String,
    pub detail: String,
    /// What was measured, when the rule measures something.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// What it should have been, in the same unit as `value`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<f64>,
}

impl Finding {
    fn new(
        rule: &'static str,
        severity: Severity,
        target: impl std::fmt::Display,
        detail: impl Into<String>,
    ) -> Finding {
        Finding {
            rule,
            severity,
            target: target.to_string(),
            detail: detail.into(),
            value: None,
            required: None,
        }
    }

    fn measured(mut self, value: f64, required: f64) -> Finding {
        self.value = Some(value);
        self.required = Some(required);
        self
    }
}

/// Delivery loudness targets. Each platform publishes one; missing it by more than a
/// tolerance means the platform will change the level itself, which changes the mix.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoudnessProfile {
    /// −14 LUFS, −1 dBTP. YouTube, Spotify video, most social platforms.
    #[default]
    YouTube,
    /// −16 LUFS, −1 dBTP. Apple Podcasts and the podcast world's de-facto standard.
    Podcast,
    /// −23 LUFS, −1 dBTP. EBU R128 broadcast delivery.
    Broadcast,
}

impl LoudnessProfile {
    /// Accepts the platform names and their common aliases.
    pub fn parse(text: &str) -> Result<LoudnessProfile> {
        match text.trim().to_ascii_lowercase().as_str() {
            "youtube" | "yt" | "social" => Ok(LoudnessProfile::YouTube),
            "podcast" | "spoken" => Ok(LoudnessProfile::Podcast),
            "broadcast" | "ebu" | "r128" => Ok(LoudnessProfile::Broadcast),
            other => Err(dvs_core::error::Error::bad_args(format!(
                "unknown loudness profile '{other}'; expected youtube, podcast or broadcast"
            ))),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            LoudnessProfile::YouTube => "youtube",
            LoudnessProfile::Podcast => "podcast",
            LoudnessProfile::Broadcast => "broadcast",
        }
    }

    pub fn target_lufs(self) -> f64 {
        match self {
            LoudnessProfile::YouTube => -14.0,
            LoudnessProfile::Podcast => -16.0,
            LoudnessProfile::Broadcast => -23.0,
        }
    }

    /// True-peak ceiling in dBTP. −1 across the board: every one of these specs leaves a
    /// dB of headroom for the lossy encoder that follows.
    pub fn true_peak_ceiling_db(self) -> f64 {
        -1.0
    }
}

impl std::str::FromStr for LoudnessProfile {
    type Err = dvs_core::error::Error;

    fn from_str(text: &str) -> Result<LoudnessProfile> {
        LoudnessProfile::parse(text)
    }
}

#[derive(Debug, Clone)]
pub struct LintOptions {
    pub profile: LoudnessProfile,
    /// Run the rules that need pixels and samples. Off is a document-only pass with no
    /// ffmpeg process at all.
    pub render: bool,
    /// Frame sampling interval for the pixel rules.
    pub sample_every: Time,
    pub scale: f64,
    pub use_proxy: bool,
}

impl Default for LintOptions {
    fn default() -> Self {
        LintOptions {
            profile: LoudnessProfile::default(),
            render: true,
            sample_every: Time::from_secs(1),
            scale: 0.5,
            use_proxy: true,
        }
    }
}

impl LintOptions {
    fn digest(&self) -> DigestOptions {
        DigestOptions {
            sample_every: self.sample_every,
            scale: self.scale,
            use_proxy: self.use_proxy,
            with_audio: true,
        }
    }

    fn analyze(&self) -> AnalyzeOptions {
        AnalyzeOptions {
            every: self.sample_every,
            scale: self.scale,
            use_proxy: self.use_proxy,
            ..AnalyzeOptions::default()
        }
    }
}

/// A time window as the selector grammar writes it.
///
/// Only clips, cues, effects and markers answer to a window — a track is not a thing that
/// exists "between 2 s and 4 s" — so this is used on clip-shaped heads only. A target that
/// does not resolve is worse than a vague one: it costs the agent a turn and teaches it to
/// stop trusting the field.
fn window(head: impl std::fmt::Display, span: Span) -> String {
    format!("{head}@{}-{}", span.start, span.end)
}

/// Check a sequence against every rule the options allow.
///
/// Takes `&Workspace` or a [`Subject`], so `inspect.lint` can run inside an op with only a
/// `Project` and an `OpCx` in hand.
pub fn lint<'a>(
    subject: impl Into<Subject<'a>>,
    tool: &'a Toolchain,
    sequence: &SequenceId,
    options: &LintOptions,
) -> Result<Vec<Finding>> {
    analyze::require_interval(options.sample_every, "sample-every")?;
    let subject = subject.into();
    let project = subject.project;
    let seq = project.sequence(sequence)?;
    let mut out = Vec::new();

    timeline_rules(seq, &mut out);
    source_rules(project, seq, &mut out);
    caption_rules(project, seq, &mut out);
    store_rules(subject, &mut out);

    // Title geometry is measured either way; the backdrop pass is what adds contrast, so
    // `low-contrast` appears exactly when pixels were available.
    let titles = if options.render {
        digest::title_measurements(subject, tool, sequence, &options.digest())?
    } else {
        digest::title_texts(project, sequence)?
    };
    title_rules(&titles, &mut out);

    if options.render {
        pixel_rules(subject, tool, sequence, options, &mut out)?;
        audio_rules(subject, tool, sequence, options, &mut out)?;
    }
    Ok(out)
}

// ------------------------------------------------------------------ timeline

fn timeline_rules(seq: &Sequence, out: &mut Vec<Finding>) {
    for track in &seq.tracks {
        if track.kind == TrackKind::Caption || track.clips.is_empty() {
            continue;
        }
        for hole in seq.uncovered_gaps(track) {
            let what = if track.kind == TrackKind::Video {
                "black"
            } else {
                "silence"
            };
            out.push(
                Finding::new(
                    "gap",
                    Severity::Warning,
                    &track.id,
                    format!(
                        "{:.3}s of {what} on {} between {} and {}",
                        hole.duration().as_secs_f64(),
                        track.name,
                        hole.start,
                        hole.end
                    ),
                )
                .measured(hole.duration().as_secs_f64(), 0.0),
            );
        }
        overlap_rules(track, out);
        transition_rules(track, out);
    }
}

fn overlap_rules(track: &Track, out: &mut Vec<Finding>) {
    for pair in track.clips.windows(2) {
        let (first, second) = (&pair[0], &pair[1]);
        if second.start < first.end() {
            let by = first.end() - second.start;
            out.push(
                Finding::new(
                    "overlap",
                    Severity::Error,
                    &second.id,
                    format!(
                        "'{}' starts {:.3}s before '{}' ends on {}; an overlap must be expressed as a transition",
                        second.label(),
                        by.as_secs_f64(),
                        first.label(),
                        track.name
                    ),
                )
                .measured(by.as_secs_f64(), 0.0),
            );
        }
    }
}

/// A transition needs handles on both sides: it cannot be longer than either neighbour, and
/// it needs a neighbour at all. Both states render as an abrupt cut with a stutter, which is
/// very hard to attribute by eye.
fn transition_rules(track: &Track, out: &mut Vec<Finding>) {
    for (index, clip) in track.clips.iter().enumerate() {
        let Some(transition) = &clip.transition_in else {
            continue;
        };
        let previous = index
            .checked_sub(1)
            .map(|before| &track.clips[before])
            .filter(|previous| previous.end() == clip.start);
        let Some(previous) = previous else {
            out.push(Finding::new(
                "orphan-transition",
                Severity::Warning,
                &clip.id,
                format!(
                    "'{}' has a {:?} transition in but nothing touching it before",
                    clip.label(),
                    transition.kind
                ),
            ));
            continue;
        };
        let shortest = clip.duration.min(previous.duration);
        if transition.duration > shortest {
            out.push(
                Finding::new(
                    "orphan-transition",
                    Severity::Warning,
                    &clip.id,
                    format!(
                        "a {:.3}s transition into '{}' is longer than its {:.3}s neighbour",
                        transition.duration.as_secs_f64(),
                        clip.label(),
                        shortest.as_secs_f64()
                    ),
                )
                .measured(
                    transition.duration.as_secs_f64(),
                    shortest.as_secs_f64(),
                ),
            );
        }
    }
}

// -------------------------------------------------------------------- source

fn source_rules(project: &Project, seq: &Sequence, out: &mut Vec<Finding>) {
    for track in &seq.tracks {
        for clip in &track.clips {
            past_source_end(project, clip, out);
            speed_rules(project, seq, clip, out);
            asset_rules(project, seq, clip, out);
            link_rules(seq, clip, out);
        }
    }
}

fn past_source_end(project: &Project, clip: &Clip, out: &mut Vec<Finding>) {
    let Some(available) = dvs_core::ops::util::source_available(project, &clip.source) else {
        return;
    };
    let needed = clip.source_span().end;
    if needed > available {
        let over = needed - available;
        out.push(
            Finding::new(
                "past-source-end",
                Severity::Error,
                &clip.id,
                format!(
                    "'{}' reads {:.3}s past the end of its source; those frames render as a freeze",
                    clip.label(),
                    over.as_secs_f64()
                ),
            )
            .measured(needed.as_secs_f64(), available.as_secs_f64()),
        );
    }
}

/// Retiming below one source frame per output frame repeats frames, which reads as judder.
/// Measured against the *source* rate, because slowing a 60 fps source to half speed on a
/// 30 fps timeline still has a fresh frame for every output frame.
fn speed_rules(project: &Project, seq: &Sequence, clip: &Clip, out: &mut Vec<Finding>) {
    let Some(asset) = clip.source.asset_id() else {
        return;
    };
    let Ok(asset) = project.asset(asset) else {
        return;
    };
    let Some(stream) = asset.probe.video.as_ref() else {
        return;
    };
    let per_output = clip.speed.as_f64() * stream.fps.as_f64() / seq.fps.as_f64();
    if per_output < 1.0 {
        out.push(
            Finding::new(
                "speed-frame-drop",
                Severity::Warning,
                &clip.id,
                format!(
                    "'{}' at speed {} draws {per_output:.2} source frames per output frame, so frames repeat",
                    clip.label(),
                    clip.speed
                ),
            )
            .measured(per_output, 1.0),
        );
    }
}

fn asset_rules(project: &Project, seq: &Sequence, clip: &Clip, out: &mut Vec<Finding>) {
    let Some(id) = clip.source.asset_id() else {
        return;
    };
    let Ok(asset) = project.asset(id) else {
        return;
    };
    if let Some(stream) = asset.probe.video.as_ref() {
        if stream.fps != seq.fps {
            out.push(
                Finding::new(
                    "fps-mismatch",
                    Severity::Info,
                    &clip.id,
                    format!(
                        "'{}' is {} fps on a {} fps sequence; frames are resampled on decode",
                        asset.name, stream.fps, seq.fps
                    ),
                )
                .measured(stream.fps.as_f64(), seq.fps.as_f64()),
            );
        }
    }
    if asset.probe.vfr && asset.proxy.is_none() {
        out.push(Finding::new(
            "vfr-source",
            Severity::Warning,
            &clip.id,
            format!(
                "'{}' has a variable frame rate and no constant-rate proxy, so frame-exact edits on it are guesses",
                asset.name
            ),
        ));
    }
    resolution_rules(project, seq, clip, &asset.name, out);
}

fn resolution_rules(
    project: &Project,
    seq: &Sequence,
    clip: &Clip,
    name: &str,
    out: &mut Vec<Finding>,
) {
    // Vector titles, flat colors and generators are synthesised at whatever size they are
    // asked for, so neither rule means anything for them.
    if !matches!(
        clip.source,
        Source::Asset { .. } | Source::Image { .. } | Source::Sequence { .. }
    ) {
        return;
    }
    let Ok(native) = digest::source_size(project, clip, seq.size) else {
        return;
    };
    let Ok(place) = digest::placement(project, clip, seq.size) else {
        return;
    };
    let ratio = if native[0] > 0 {
        f64::from(place.dest.w) / f64::from(native[0])
    } else {
        1.0
    };
    if ratio > UPSCALE_TOLERANCE {
        out.push(
            Finding::new(
                "upscaled",
                Severity::Warning,
                &clip.id,
                format!(
                    "'{name}' is {}x{} shown at {:.0}x{:.0}, a {ratio:.2}x blow-up",
                    native[0], native[1], place.dest.w, place.dest.h
                ),
            )
            .measured(ratio, 1.0),
        );
    }

    if !matches!(clip.fit, Fit::Contain) {
        return;
    }
    let source_aspect = f64::from(native[0]) / f64::from(native[1].max(1));
    let frame_aspect = f64::from(seq.size[0]) / f64::from(seq.size[1].max(1));
    let mismatch = (source_aspect - frame_aspect).abs() / frame_aspect;
    if mismatch <= LETTERBOX_TOLERANCE {
        return;
    }
    // Only a clip that is trying to fill the frame gets letterboxed; an inset overlay is
    // supposed to leave the frame showing around it.
    let fills = place.dest.w >= seq.size[0] as f32 * 0.99 || place.dest.h >= seq.size[1] as f32 * 0.99;
    if !fills {
        return;
    }
    let covered = f64::from(place.dest.w * place.dest.h)
        / f64::from((seq.size[0] * seq.size[1]).max(1));
    out.push(
        Finding::new(
            "letterboxed",
            Severity::Info,
            &clip.id,
            format!(
                "'{name}' is {source_aspect:.3}:1 in a {frame_aspect:.3}:1 frame, so {:.0}% of the frame is bars",
                (1.0 - covered) * 100.0
            ),
        )
        .measured(1.0 - covered, 0.0),
    );
}

/// Linked a/v clips are one edit in two places. Once their starts or durations differ, every
/// later trim moves only one of them and the drift compounds silently.
fn link_rules(seq: &Sequence, clip: &Clip, out: &mut Vec<Finding>) {
    let Some(link) = &clip.link else {
        return;
    };
    let Some((_, other)) = seq.find_clip(link) else {
        out.push(Finding::new(
            "av-drift",
            Severity::Warning,
            &clip.id,
            format!(
                "'{}' is linked to {link}, which is not in this sequence",
                clip.label()
            ),
        ));
        return;
    };
    // Reported once per pair: the clip that sorts first by id is the one that speaks.
    if clip.id.as_str() > other.id.as_str() {
        return;
    }
    let start_drift = (clip.start - other.start).abs();
    let length_drift = (clip.duration - other.duration).abs();
    let drift = start_drift.max(length_drift);
    if drift.is_positive() {
        out.push(
            Finding::new(
                "av-drift",
                Severity::Warning,
                &clip.id,
                format!(
                    "'{}' and its linked '{}' differ by {:.3}s in position or length",
                    clip.label(),
                    other.label(),
                    drift.as_secs_f64()
                ),
            )
            .measured(drift.as_secs_f64(), 0.0),
        );
    }
}

// ------------------------------------------------------------------ captions

fn caption_rules(project: &Project, seq: &Sequence, out: &mut Vec<Finding>) {
    let fallback = CaptionStyle::named("default");
    for track in &seq.tracks {
        if track.kind != TrackKind::Caption {
            continue;
        }
        let mut previous: Option<&dvs_core::project::CaptionCue> = None;
        for cue in &track.cues {
            let style = cue
                .style
                .as_ref()
                .or(track.style.as_ref())
                .and_then(|id| project.styles.get(id))
                .unwrap_or(&fallback);
            let cps = cue.chars_per_second();
            if cps > style.max_cps {
                out.push(
                    Finding::new(
                        "caption-too-fast",
                        Severity::Warning,
                        &cue.id,
                        format!(
                            "'{}' runs at {cps:.1} characters per second",
                            cue.text.replace('\n', " ")
                        ),
                    )
                    .measured(cps, style.max_cps),
                );
            }
            if cue.lines() > style.max_lines {
                out.push(
                    Finding::new(
                        "caption-too-long",
                        Severity::Warning,
                        &cue.id,
                        format!("{} lines of caption at once", cue.lines()),
                    )
                    .measured(cue.lines() as f64, style.max_lines as f64),
                );
            }
            if let Some(previous) = previous {
                if cue.span.start < previous.span.end {
                    let by = previous.span.end - cue.span.start;
                    out.push(
                        Finding::new(
                            "caption-overlap",
                            Severity::Error,
                            &cue.id,
                            format!(
                                "overlaps the previous cue by {:.3}s, so both are on screen",
                                by.as_secs_f64()
                            ),
                        )
                        .measured(by.as_secs_f64(), 0.0),
                    );
                }
            }
            previous = Some(cue);
        }
    }
}

// --------------------------------------------------------------------- store

fn store_rules(subject: Subject<'_>, out: &mut Vec<Finding>) {
    let project = subject.project;
    let referenced = referenced_assets(project);
    for (id, asset) in &project.assets {
        if !subject.assets.exists(&asset.hash) {
            out.push(Finding::new(
                "missing-asset",
                Severity::Error,
                id,
                format!(
                    "'{}' ({}) is not in the asset store; relink or re-import it",
                    asset.name, asset.hash
                ),
            ));
        }
        if !referenced.contains(id) {
            out.push(Finding::new(
                "unreferenced-asset",
                Severity::Info,
                id,
                format!("'{}' is not used by any clip or effect", asset.name),
            ));
        }
        if let Some(proxy) = &asset.proxy {
            let path = subject.paths.resolve(proxy);
            let usable = std::fs::metadata(&path).is_ok_and(|meta| meta.len() > 0);
            if !usable {
                out.push(Finding::new(
                    "stale-proxy",
                    Severity::Warning,
                    id,
                    format!(
                        "'{}' points at proxy '{proxy}', which is missing or empty; regenerate it with asset.proxy",
                        asset.name
                    ),
                ));
            }
        }
    }
}

/// Assets a document still points at, through clips *and* through effect parameters — a LUT
/// is referenced only by the effect that uses it, and reporting it unreferenced would
/// invite an agent to garbage-collect a file the render needs.
fn referenced_assets(project: &Project) -> std::collections::BTreeSet<AssetId> {
    let mut used = std::collections::BTreeSet::new();
    for sequence in project.sequences.values() {
        for track in &sequence.tracks {
            for clip in &track.clips {
                if let Some(id) = clip.source.asset_id() {
                    used.insert(id.clone());
                }
                for effect in &clip.effects {
                    for value in effect.params.values() {
                        if let Some(text) = value.as_str() {
                            let candidate = AssetId::from_raw(text);
                            if project.assets.contains_key(&candidate) {
                                used.insert(candidate);
                            }
                        }
                    }
                }
            }
        }
    }
    used
}

// -------------------------------------------------------------------- titles

fn title_rules(titles: &[TitleMeasurement], out: &mut Vec<Finding>) {
    for measurement in titles {
        let frame = measurement.frame_size;
        for text in &measurement.reports {
            if text.overflows(frame) {
                out.push(Finding::new(
                    "title-overflow",
                    Severity::Error,
                    &measurement.clip,
                    format!(
                        "'{}' extends past the frame: bbox {:?} in a {}x{} frame",
                        text.text, text.bbox, frame[0], frame[1]
                    ),
                ));
            } else if !text.in_safe_area(frame, TITLE_SAFE_AREA) {
                // No `value`: the violation is a rectangle escaping a rectangle, and there
                // is no single number that says which edge or by how much. The bbox is in
                // the digest for a caller that needs the geometry.
                out.push(Finding::new(
                    "unsafe-area",
                    Severity::Warning,
                    &measurement.clip,
                    format!(
                        "'{}' at {:?} sits outside the {:.0}% safe area of the {}x{} frame, where a player's chrome can cover it",
                        text.text,
                        text.bbox,
                        TITLE_SAFE_AREA * 100.0,
                        frame[0],
                        frame[1]
                    ),
                ));
            }
            if let Some(fallback) = &text.fallback {
                out.push(Finding::new(
                    "font-fallback",
                    Severity::Warning,
                    &measurement.clip,
                    format!(
                        "'{}' asked for a font that is not installed and was rendered in '{fallback}'; the render will differ on another machine",
                        text.text
                    ),
                ));
            }
            let fraction = text.font_size / frame[1].max(1) as f32;
            if fraction < MIN_TYPE_FRACTION {
                out.push(
                    Finding::new(
                        "tiny-type",
                        Severity::Warning,
                        &measurement.clip,
                        format!(
                            "'{}' is {:.0}px in a {}px-tall frame, below the legible floor",
                            text.text, text.font_size, frame[1]
                        ),
                    )
                    .measured(f64::from(fraction), f64::from(MIN_TYPE_FRACTION)),
                );
            }
            if let Some(contrast) = text.contrast {
                if contrast < MIN_CONTRAST {
                    out.push(
                        Finding::new(
                            "low-contrast",
                            Severity::Warning,
                            &measurement.clip,
                            format!(
                                "'{}' is at {contrast:.1}:1 against the picture behind it at {}",
                                text.text, measurement.at
                            ),
                        )
                        .measured(contrast, MIN_CONTRAST),
                    );
                }
            }
        }
    }
}

// -------------------------------------------------------------------- pixels

/// The detectors from [`crate::analyze`], expressed as findings.
///
/// Targets are `clip@start-end`, which names whatever is on screen during the dead window —
/// the clip an agent would trim, replace or re-grade. When the window is a *hole* rather
/// than a clip the selector matches nothing, and that is deliberate: a hole has no
/// addressable object, and the `gap` rule already reports it against the track it is on.
fn pixel_rules(
    subject: Subject<'_>,
    tool: &Toolchain,
    sequence: &SequenceId,
    options: &LintOptions,
    out: &mut Vec<Finding>,
) -> Result<()> {
    let analysis = analyze::analyze(subject, tool, sequence, &options.analyze())?;
    for span in &analysis.black_ranges {
        out.push(
            Finding::new(
                "black-frames",
                Severity::Warning,
                window("clip", *span),
                format!(
                    "{:.3}s of black picture from {}",
                    span.duration().as_secs_f64(),
                    span.start
                ),
            )
            .measured(span.duration().as_secs_f64(), 0.0),
        );
    }
    for span in &analysis.frozen_ranges {
        out.push(
            Finding::new(
                "frozen-frames",
                Severity::Warning,
                window("clip", *span),
                format!(
                    "the picture does not change for {:.3}s from {}",
                    span.duration().as_secs_f64(),
                    span.start
                ),
            )
            .measured(span.duration().as_secs_f64(), 0.0),
        );
    }
    let frame = subject.project.sequence(sequence)?.fps.frame_duration();
    for at in &analysis.flashes {
        out.push(Finding::new(
            "flash",
            Severity::Warning,
            window("clip", Span::from_duration(*at, frame)),
            format!("luminance spikes and returns at {at}"),
        ));
    }
    Ok(())
}

// --------------------------------------------------------------------- audio

fn audio_rules(
    subject: Subject<'_>,
    tool: &Toolchain,
    sequence: &SequenceId,
    options: &LintOptions,
    out: &mut Vec<Finding>,
) -> Result<()> {
    let seq = subject.project.sequence(sequence)?;
    let audio_tracks: Vec<&Track> = seq
        .tracks
        .iter()
        .filter(|track| track.kind == TrackKind::Audio && !track.clips.is_empty())
        .collect();
    if audio_tracks.is_empty() {
        return Ok(());
    }

    let (samples, spec) = analyze::mix_all(subject, tool, sequence)?;
    let loudness = dvs_audio::analyze_loudness(&samples, spec.rate, spec.channels)?;
    let profile = options.profile;

    if loudness.integrated_lufs > AUDIBLE_FLOOR_LUFS {
        let delta = loudness.integrated_lufs - profile.target_lufs();
        if delta.abs() > LOUDNESS_TOLERANCE_LU {
            out.push(
                Finding::new(
                    "loudness-out-of-spec",
                    Severity::Warning,
                    "track[kind=audio]",
                    format!(
                        "mix is {:.1} LUFS against the {} target of {:.1}; apply audio.normalize --lufs {:.0} ({:+.1} dB)",
                        loudness.integrated_lufs,
                        profile.name(),
                        profile.target_lufs(),
                        profile.target_lufs(),
                        dvs_audio::normalize_gain_db(loudness.integrated_lufs, profile.target_lufs())
                    ),
                )
                .measured(loudness.integrated_lufs, profile.target_lufs()),
            );
        }
    }
    if loudness.true_peak_db > profile.true_peak_ceiling_db() {
        out.push(
            Finding::new(
                "true-peak",
                Severity::Error,
                "track[kind=audio]",
                format!(
                    "true peak is {:.1} dBTP, above the {:.1} dBTP ceiling; a lossy encoder will clip it",
                    loudness.true_peak_db,
                    profile.true_peak_ceiling_db()
                ),
            )
            .measured(loudness.true_peak_db, profile.true_peak_ceiling_db()),
        );
    }
    if loudness.clipped_samples > 0 {
        out.push(
            Finding::new(
                "clipping",
                Severity::Error,
                "track[kind=audio]",
                format!(
                    "{} samples reached full scale; the mixer has no limiter, so lower a clip or track gain",
                    loudness.clipped_samples
                ),
            )
            .measured(loudness.clipped_samples as f64, 0.0),
        );
    }

    for span in dvs_audio::detect_silence(
        &samples,
        spec.rate,
        spec.channels,
        digest::SILENCE_THRESHOLD_DB,
        MAX_SILENCE,
    ) {
        out.push(
            Finding::new(
                "silence-gap",
                Severity::Warning,
                // The mix is the sum of every audio track, and a hole in it belongs to no
                // single clip, so the address is the tracks and the window goes in the text.
                "track[kind=audio]",
                format!(
                    "{:.3}s with no audible audio, from {} to {}",
                    span.duration().as_secs_f64(),
                    span.start,
                    span.end
                ),
            )
            .measured(span.duration().as_secs_f64(), MAX_SILENCE.as_secs_f64()),
        );
    }

    ducking_rules(subject, tool, sequence, &audio_tracks, out)?;
    Ok(())
}

/// Two audio tracks playing at once at similar loudness is the sound of an un-ducked music
/// bed over dialogue, which is the single most common audio mistake in an unattended edit.
///
/// Measured rather than guessed from the document: a music track that happens to be mixed
/// 20 dB down is fine without ducking, and the document cannot say that. The track holding
/// the longest single clip is named as the bed, because a music bed is one long clip and
/// dialogue is many short ones.
fn ducking_rules(
    subject: Subject<'_>,
    tool: &Toolchain,
    sequence: &SequenceId,
    tracks: &[&Track],
    out: &mut Vec<Finding>,
) -> Result<()> {
    let project = subject.project;
    let seq = project.sequence(sequence)?;
    let spec = dvs_audio::MixSpec::of(seq);
    for (index, first) in tracks.iter().enumerate() {
        for second in tracks.iter().skip(index + 1) {
            if first.muted || second.muted {
                continue;
            }
            let Some(overlap) = longest_overlap(first, second) else {
                continue;
            };
            if overlap.duration() < MIN_DUCK_OVERLAP {
                continue;
            }
            if ducks_against(first, &second.id) || ducks_against(second, &first.id) {
                continue;
            }
            let left = track_loudness(subject, tool, sequence, &first.id, overlap, spec)?;
            let right = track_loudness(subject, tool, sequence, &second.id, overlap, spec)?;
            if left <= AUDIBLE_FLOOR_LUFS || right <= AUDIBLE_FLOOR_LUFS {
                continue;
            }
            let separation = (left - right).abs();
            if separation >= DUCK_MARGIN_LU {
                continue;
            }
            let (bed, over) = if longest_clip(first) >= longest_clip(second) {
                (first, second)
            } else {
                (second, first)
            };
            out.push(
                Finding::new(
                    "unducked-music",
                    Severity::Warning,
                    &bed.id,
                    format!(
                        "{} and {} both play at within {separation:.1} LU for {:.1}s with no ducking; try audio.duck --track {} --against {} --by -12",
                        bed.name,
                        over.name,
                        overlap.duration().as_secs_f64(),
                        bed.name,
                        over.name
                    ),
                )
                .measured(separation, DUCK_MARGIN_LU),
            );
        }
    }
    Ok(())
}

fn ducks_against(track: &Track, against: &TrackId) -> bool {
    track
        .clips
        .iter()
        .any(|clip| clip.ducking.as_ref().is_some_and(|duck| &duck.against == against))
}

fn longest_clip(track: &Track) -> Time {
    track
        .clips
        .iter()
        .map(|clip| clip.duration)
        .max()
        .unwrap_or(Time::ZERO)
}

/// The longest window in which both tracks have audio.
fn longest_overlap(first: &Track, second: &Track) -> Option<Span> {
    let mut best: Option<Span> = None;
    for left in &first.clips {
        for right in &second.clips {
            if let Some(span) = left.span().intersect(&right.span()) {
                if best.is_none_or(|current| span.duration() > current.duration()) {
                    best = Some(span);
                }
            }
        }
    }
    best
}

fn track_loudness(
    subject: Subject<'_>,
    tool: &Toolchain,
    sequence: &SequenceId,
    track: &TrackId,
    span: Span,
    spec: dvs_audio::MixSpec,
) -> Result<f64> {
    let samples = dvs_audio::mix_tracks(
        subject.project,
        sequence,
        Some(std::slice::from_ref(track)),
        span,
        spec,
        tool,
        subject.assets,
        subject.paths,
    )?;
    Ok(dvs_audio::analyze_loudness(&samples, spec.rate, spec.channels)?.integrated_lufs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::*;
    use dvs_core::color::Rgba;
    use dvs_core::project::{Clip, Ducking, Source, Transition, TransitionKind};
    use dvs_core::time::{Fps, Rat};
    use std::collections::BTreeSet;

    const RED: Rgba = Rgba::opaque(200, 40, 40);
    const BLUE: Rgba = Rgba::opaque(30, 60, 200);
    const NEAR_BLACK: Rgba = Rgba::opaque(6, 0, 0);

    /// One rule, one document that must trip it, one that must not.
    ///
    /// The pair is the whole test: a rule that fires on everything is as useless as one that
    /// never fires, and only the second fixture can tell the two apart.
    struct Case {
        rule: &'static str,
        size: [u32; 2],
        render: bool,
        /// Sample every frame instead of every second. Only the flash detector needs it —
        /// a one-frame spike is invisible to one-second sampling by construction.
        per_frame: bool,
        fires: fn(&mut Fixture),
        clean: fn(&mut Fixture),
    }

    fn findings(case: &Case, build: fn(&mut Fixture)) -> Vec<Finding> {
        let mut fixture = Fixture::new(case.size);
        build(&mut fixture);
        let options = LintOptions {
            render: case.render,
            sample_every: if case.per_frame {
                fps().frame_duration()
            } else {
                Time::from_secs(1)
            },
            // Full scale: the fixtures are tiny, and a half-scale raster would put the
            // title measurements in a different coordinate system than the assertions.
            scale: 1.0,
            ..LintOptions::default()
        };
        let sequence = fixture.seq();
        lint(&fixture.ws, tool(), &sequence, &options).expect("lint runs")
    }

    fn rules_seen(findings: &[Finding]) -> Vec<&'static str> {
        let mut seen: Vec<&'static str> = findings.iter().map(|f| f.rule).collect();
        seen.sort_unstable();
        seen.dedup();
        seen
    }

    // ---------------------------------------------------------------- builders

    fn two_clips(f: &mut Fixture, second_start: Time) {
        let track = f.video();
        f.push(&track, color_clip(RED, Time::ZERO, secs(2, 1)));
        f.push(&track, color_clip(BLUE, second_start, secs(2, 1)));
    }

    fn adjacent(f: &mut Fixture) {
        two_clips(f, secs(2, 1));
    }

    fn with_hole(f: &mut Fixture) {
        two_clips(f, secs(4, 1));
    }

    fn colliding(f: &mut Fixture) {
        two_clips(f, secs(1, 1));
    }

    fn transition(kind: TransitionKind, duration: Time) -> Transition {
        Transition {
            kind,
            duration,
            easing: Default::default(),
            direction: Default::default(),
            color: None,
        }
    }

    fn dangling_transition(f: &mut Fixture) {
        let track = f.video();
        let mut clip = color_clip(RED, Time::ZERO, secs(2, 1));
        clip.transition_in = Some(transition(TransitionKind::Dissolve, secs(1, 2)));
        f.push(&track, clip);
    }

    fn supported_transition(f: &mut Fixture) {
        let track = f.video();
        f.push(&track, color_clip(RED, Time::ZERO, secs(2, 1)));
        let mut clip = color_clip(BLUE, secs(2, 1), secs(2, 1));
        clip.transition_in = Some(transition(TransitionKind::Dissolve, secs(1, 2)));
        f.push(&track, clip);
    }

    fn nested_clip(f: &mut Fixture, duration: Time) {
        let inner = f.nested(secs(2, 1));
        let track = f.video();
        f.push(
            &track,
            Clip::new(Source::Sequence { sequence: inner }, Time::ZERO, duration),
        );
    }

    fn reads_past_source(f: &mut Fixture) {
        nested_clip(f, secs(5, 1));
    }

    fn inside_source(f: &mut Fixture) {
        nested_clip(f, secs(2, 1));
    }

    /// One clip over one asset, with the asset's probe and the clip's speed as given.
    fn asset_clip(f: &mut Fixture, probe: dvs_core::project::Probe, speed: Rat) -> AssetId {
        let asset = f.asset("talk.mp4", probe, true);
        let track = f.video();
        let mut clip = Clip::new(
            Source::Asset {
                asset: asset.clone(),
                stream: None,
            },
            Time::ZERO,
            secs(2, 1),
        );
        clip.speed = speed;
        f.push(&track, clip);
        asset
    }

    fn native_probe() -> dvs_core::project::Probe {
        video_probe([320, 180], fps(), secs(10, 1))
    }

    fn retimed_below_one_frame(f: &mut Fixture) {
        asset_clip(f, native_probe(), Rat::new(1, 2).expect("1/2"));
    }

    fn played_at_speed_one(f: &mut Fixture) {
        asset_clip(f, native_probe(), Rat::ONE);
    }

    fn off_rate_source(f: &mut Fixture) {
        let rate = Fps::new(25, 1).expect("25/1");
        asset_clip(f, video_probe([320, 180], rate, secs(10, 1)), Rat::ONE);
    }

    fn variable_rate_source(f: &mut Fixture) {
        let mut probe = native_probe();
        probe.vfr = true;
        asset_clip(f, probe, Rat::ONE);
    }

    fn small_source(f: &mut Fixture) {
        asset_clip(f, video_probe([320, 180], fps(), secs(10, 1)), Rat::ONE);
    }

    fn matching_source(f: &mut Fixture) {
        asset_clip(f, video_probe([1920, 1080], fps(), secs(10, 1)), Rat::ONE);
    }

    fn four_by_three_source(f: &mut Fixture) {
        asset_clip(f, video_probe([640, 480], fps(), secs(10, 1)), Rat::ONE);
    }

    fn linked_pair(f: &mut Fixture, audio_start: Time) {
        let video = f.video();
        let audio = f.track(TrackKind::Audio);
        let picture = f.push(&video, color_clip(RED, Time::ZERO, secs(2, 1)));
        let sound = f.push(
            &audio,
            Clip::new(
                Source::Generator {
                    generator: dvs_core::project::Generator::Tone,
                    params: Default::default(),
                },
                audio_start,
                secs(2, 1),
            ),
        );
        let seq = f.sequence_mut();
        seq.track_mut(&video).expect("video").clips[0].link = Some(sound.clone());
        seq.track_mut(&audio).expect("audio").clips[0].link = Some(picture);
    }

    fn drifted_link(f: &mut Fixture) {
        linked_pair(f, secs(1, 2));
    }

    fn aligned_link(f: &mut Fixture) {
        linked_pair(f, Time::ZERO);
    }

    fn captions(f: &mut Fixture, cues: &[(Span, &str)]) {
        let track = f.track(TrackKind::Caption);
        for (span, text) in cues {
            f.cue(&track, *span, text);
        }
    }

    fn unreadably_fast_caption(f: &mut Fixture) {
        captions(
            f,
            &[(
                Span::new(Time::ZERO, secs(1, 1)),
                "far too many characters to read inside a single second of screen time",
            )],
        );
    }

    fn comfortable_caption(f: &mut Fixture) {
        captions(f, &[(Span::new(Time::ZERO, secs(3, 1)), "short line")]);
    }

    fn overlapping_captions(f: &mut Fixture) {
        captions(
            f,
            &[
                (Span::new(Time::ZERO, secs(2, 1)), "first"),
                (Span::new(secs(1, 1), secs(3, 1)), "second"),
            ],
        );
    }

    fn sequential_captions(f: &mut Fixture) {
        captions(
            f,
            &[
                (Span::new(Time::ZERO, secs(2, 1)), "first"),
                (Span::new(secs(2, 1), secs(4, 1)), "second"),
            ],
        );
    }

    fn three_line_caption(f: &mut Fixture) {
        captions(f, &[(Span::new(Time::ZERO, secs(4, 1)), "one\ntwo\nthree")]);
    }

    fn two_line_caption(f: &mut Fixture) {
        captions(f, &[(Span::new(Time::ZERO, secs(4, 1)), "one\ntwo")]);
    }

    fn orphan_asset(f: &mut Fixture) {
        f.asset("unused.mp4", native_probe(), true);
        let track = f.video();
        f.push(&track, color_clip(RED, Time::ZERO, secs(2, 1)));
    }

    fn used_asset(f: &mut Fixture) {
        asset_clip(f, native_probe(), Rat::ONE);
    }

    fn asset_without_bytes(f: &mut Fixture) {
        let asset = f.asset("gone.mp4", native_probe(), false);
        let track = f.video();
        f.push(
            &track,
            Clip::new(Source::Asset { asset, stream: None }, Time::ZERO, secs(2, 1)),
        );
    }

    fn proxy(f: &mut Fixture, relative: &str, write: bool) {
        let asset = asset_clip(f, native_probe(), Rat::ONE);
        if write {
            let path = f.ws.paths.resolve(relative);
            std::fs::create_dir_all(path.parent().expect("proxy parent")).expect("mkdir");
            std::fs::write(&path, b"stand-in for a proxy file").expect("write proxy");
        }
        f.ws.project
            .assets
            .get_mut(&asset)
            .expect("asset")
            .proxy = Some(relative.to_string());
    }

    fn missing_proxy(f: &mut Fixture) {
        proxy(f, "cache/proxy/not-generated.mp4", false);
    }

    fn present_proxy(f: &mut Fixture) {
        proxy(f, "cache/proxy/generated.mp4", true);
    }

    /// A title clip filling the frame, over `backdrop` if one is given.
    fn titled(f: &mut Fixture, svg: String, backdrop: Option<Rgba>) {
        let size = f.sequence().size;
        if let Some(color) = backdrop {
            let under = f.video();
            f.push(&under, color_clip(color, Time::ZERO, secs(2, 1)));
        }
        let title = f.title("lower-third", size, svg);
        let over = f.video();
        f.push(
            &over,
            Clip::new(Source::Title { title }, Time::ZERO, secs(2, 1)),
        );
    }

    fn overflowing_title(f: &mut Fixture) {
        let size = f.sequence().size;
        titled(
            f,
            text_title(size, 250.0, 100.0, 40.0, "sans-serif", "#ffffff", "OVERFLOWING"),
            None,
        );
    }

    fn contained_title(f: &mut Fixture) {
        let size = f.sequence().size;
        titled(
            f,
            text_title(size, 100.0, 100.0, 40.0, "sans-serif", "#ffffff", "ok"),
            None,
        );
    }

    fn title_against_the_edge(f: &mut Fixture) {
        let size = f.sequence().size;
        titled(
            f,
            text_title(size, 2.0, 100.0, 20.0, "sans-serif", "#ffffff", "edge"),
            None,
        );
    }

    fn title_in_an_absent_font(f: &mut Fixture) {
        let size = f.sequence().size;
        titled(
            f,
            text_title(size, 100.0, 100.0, 20.0, "No Such Face 9000", "#ffffff", "hi"),
            None,
        );
    }

    fn title_in_unreadable_type(f: &mut Fixture) {
        let size = f.sequence().size;
        titled(
            f,
            text_title(size, 100.0, 100.0, 3.0, "sans-serif", "#ffffff", "fine print"),
            None,
        );
    }

    fn white_on_white(f: &mut Fixture) {
        let size = f.sequence().size;
        titled(
            f,
            text_title(size, 40.0, 110.0, 48.0, "sans-serif", "#ffffff", "HELLO"),
            Some(Rgba::WHITE),
        );
    }

    fn white_on_dark(f: &mut Fixture) {
        let size = f.sequence().size;
        titled(
            f,
            text_title(size, 40.0, 110.0, 48.0, "sans-serif", "#ffffff", "HELLO"),
            Some(Rgba::opaque(8, 8, 12)),
        );
    }

    fn dead_black_stretch(f: &mut Fixture) {
        let track = f.video();
        f.push(&track, color_clip(Rgba::BLACK, Time::ZERO, secs(3, 1)));
    }

    fn coloured_stretch(f: &mut Fixture) {
        let track = f.video();
        f.push(&track, color_clip(RED, Time::ZERO, secs(3, 1)));
    }

    fn still_for_seconds(f: &mut Fixture) {
        let track = f.video();
        f.push(&track, color_clip(RED, Time::ZERO, secs(4, 1)));
    }

    fn cut_every_second_and_a_half(f: &mut Fixture) {
        let track = f.video();
        f.push(&track, color_clip(RED, Time::ZERO, secs(3, 2)));
        f.push(&track, color_clip(BLUE, secs(3, 2), secs(3, 2)));
        f.push(&track, color_clip(RED, secs(3, 1), secs(3, 2)));
    }

    fn single_white_frame(f: &mut Fixture) {
        let track = f.video();
        let frame = fps().frame_duration();
        f.push(&track, color_clip(NEAR_BLACK, Time::ZERO, secs(1, 2)));
        f.push(&track, color_clip(Rgba::WHITE, secs(1, 2), frame));
        f.push(&track, color_clip(NEAR_BLACK, secs(1, 2) + frame, secs(1, 2)));
    }

    fn no_white_frame(f: &mut Fixture) {
        let track = f.video();
        f.push(&track, color_clip(NEAR_BLACK, Time::ZERO, secs(1, 1)));
    }

    fn loud_mix(f: &mut Fixture) {
        f.tone_track("loud.wav", Span::new(Time::ZERO, secs(3, 1)), 0.95);
    }

    /// The same mix with the correction the `loudness-out-of-spec` finding recommends
    /// applied, which is what makes the pair prove the advice rather than just the rule.
    fn normalized_mix(f: &mut Fixture) {
        loud_mix(f);
        let sequence = f.seq();
        let (samples, spec) = crate::analyze::mix_all(Subject::from(&f.ws), tool(), &sequence).expect("mix");
        let measured =
            dvs_audio::analyze_loudness(&samples, spec.rate, spec.channels).expect("loudness");
        let gain = dvs_audio::normalize_gain_db(
            measured.integrated_lufs,
            LoudnessProfile::YouTube.target_lufs(),
        );
        for track in &mut f.sequence_mut().tracks {
            for clip in &mut track.clips {
                clip.gain_db = gain;
            }
        }
    }

    fn summed_past_full_scale(f: &mut Fixture) {
        let span = Span::new(Time::ZERO, secs(2, 1));
        f.tone_track("one.wav", span, 1.0);
        f.tone_track("two.wav", span, 0.999);
    }

    fn quiet_enough_not_to_clip(f: &mut Fixture) {
        f.tone_track("quiet.wav", Span::new(Time::ZERO, secs(2, 1)), 0.3);
    }

    fn hole_in_the_audio(f: &mut Fixture) {
        let track = f.tone_track("head.wav", Span::new(Time::ZERO, secs(1, 1)), 0.5);
        let path = f.tone("tail.wav", 1.0, 0.5);
        let asset = f.import(&path, "tail.wav");
        f.push(
            &track,
            Clip::new(
                Source::Asset { asset, stream: None },
                secs(4, 1),
                secs(1, 1),
            ),
        );
    }

    fn continuous_audio(f: &mut Fixture) {
        f.tone_track("whole.wav", Span::new(Time::ZERO, secs(5, 1)), 0.5);
    }

    fn two_beds(f: &mut Fixture) -> (TrackId, TrackId) {
        let span = Span::new(Time::ZERO, secs(3, 1));
        let music = f.tone_track("music.wav", span, 0.3);
        let voice = f.tone_track("voice.wav", span, 0.28);
        (music, voice)
    }

    fn music_over_voice(f: &mut Fixture) {
        two_beds(f);
    }

    fn ducked_music_over_voice(f: &mut Fixture) {
        let (music, voice) = two_beds(f);
        let seq = f.sequence_mut();
        seq.track_mut(&music).expect("music").clips[0].ducking = Some(Ducking {
            against: voice,
            by: -12.0,
            attack: secs(1, 5),
            release: secs(1, 2),
            threshold: -30.0,
        });
    }

    // ------------------------------------------------------------------- table

    fn document(rule: &'static str, fires: fn(&mut Fixture), clean: fn(&mut Fixture)) -> Case {
        Case {
            rule,
            size: [320, 180],
            render: false,
            per_frame: false,
            fires,
            clean,
        }
    }

    fn rendered(rule: &'static str, fires: fn(&mut Fixture), clean: fn(&mut Fixture)) -> Case {
        Case {
            rule,
            size: [320, 180],
            render: true,
            per_frame: false,
            fires,
            clean,
        }
    }

    fn cases() -> Vec<Case> {
        let hd = |mut case: Case| {
            case.size = [1920, 1080];
            case
        };
        vec![
            document("gap", with_hole, adjacent),
            document("overlap", colliding, adjacent),
            document("orphan-transition", dangling_transition, supported_transition),
            document("past-source-end", reads_past_source, inside_source),
            document(
                "speed-frame-drop",
                retimed_below_one_frame,
                played_at_speed_one,
            ),
            document("fps-mismatch", off_rate_source, played_at_speed_one),
            document("vfr-source", variable_rate_source, played_at_speed_one),
            hd(document("upscaled", small_source, matching_source)),
            hd(document("letterboxed", four_by_three_source, matching_source)),
            document("av-drift", drifted_link, aligned_link),
            document(
                "caption-too-fast",
                unreadably_fast_caption,
                comfortable_caption,
            ),
            document("caption-overlap", overlapping_captions, sequential_captions),
            document("caption-too-long", three_line_caption, two_line_caption),
            document("unreferenced-asset", orphan_asset, used_asset),
            document("missing-asset", asset_without_bytes, used_asset),
            document("stale-proxy", missing_proxy, present_proxy),
            document("title-overflow", overflowing_title, contained_title),
            document("unsafe-area", title_against_the_edge, contained_title),
            document("font-fallback", title_in_an_absent_font, contained_title),
            document("tiny-type", title_in_unreadable_type, contained_title),
            rendered("low-contrast", white_on_white, white_on_dark),
            rendered("black-frames", dead_black_stretch, coloured_stretch),
            rendered(
                "frozen-frames",
                still_for_seconds,
                cut_every_second_and_a_half,
            ),
            Case {
                rule: "flash",
                size: [320, 180],
                render: true,
                per_frame: true,
                fires: single_white_frame,
                clean: no_white_frame,
            },
            rendered("loudness-out-of-spec", loud_mix, normalized_mix),
            rendered("true-peak", loud_mix, normalized_mix),
            rendered("clipping", summed_past_full_scale, quiet_enough_not_to_clip),
            rendered("silence-gap", hole_in_the_audio, continuous_audio),
            rendered("unducked-music", music_over_voice, ducked_music_over_voice),
        ]
    }

    #[test]
    fn the_table_covers_every_rule_the_crate_can_emit() {
        let covered: BTreeSet<&str> = cases().iter().map(|case| case.rule).collect();
        let declared: BTreeSet<&str> = RULES.iter().copied().collect();
        assert_eq!(
            covered, declared,
            "every rule needs a fixture that trips it and one that does not"
        );
    }

    #[test]
    fn every_rule_fires_on_its_fixture_and_stays_quiet_on_the_clean_one() {
        for case in cases() {
            let fired = findings(&case, case.fires);
            assert!(
                fired.iter().any(|finding| finding.rule == case.rule),
                "'{}' did not fire on its triggering fixture; saw {:?}",
                case.rule,
                rules_seen(&fired)
            );
            let clean = findings(&case, case.clean);
            assert!(
                !clean.iter().any(|finding| finding.rule == case.rule),
                "'{}' fired on its clean fixture; saw {:?}",
                case.rule,
                rules_seen(&clean)
            );
        }
    }

    #[test]
    fn a_document_only_pass_skips_every_rule_that_needs_pixels() {
        let mut fixture = Fixture::new([320, 180]);
        dead_black_stretch(&mut fixture);
        let sequence = fixture.seq();
        let findings = lint(
            &fixture.ws,
            tool(),
            &sequence,
            &LintOptions {
                render: false,
                ..LintOptions::default()
            },
        )
        .expect("lint");
        for finding in &findings {
            assert!(
                !RENDER_RULES.contains(&finding.rule),
                "'{}' needs pixels and must not run without them",
                finding.rule
            );
        }
    }

    #[test]
    fn upscaling_is_reported_with_the_ratio_it_measured() {
        let mut fixture = Fixture::new([1920, 1080]);
        small_source(&mut fixture);
        let sequence = fixture.seq();
        let findings = lint(
            &fixture.ws,
            tool(),
            &sequence,
            &LintOptions {
                render: false,
                ..LintOptions::default()
            },
        )
        .expect("lint");
        let upscaled = findings
            .iter()
            .find(|finding| finding.rule == "upscaled")
            .expect("a 320x180 source on a 1080p sequence is upscaled");
        // 1920 / 320: the ratio has to be the real number, because an agent decides whether
        // to re-import at a higher resolution by comparing it against 1.
        assert!(
            (upscaled.value.expect("measured ratio") - 6.0).abs() < 0.01,
            "expected a 6x blow-up, got {:?}",
            upscaled.value
        );
        assert_eq!(upscaled.required, Some(1.0));
    }

    #[test]
    fn contrast_is_measured_against_the_rendered_backdrop_not_the_document() {
        // Identical title, identical document except for the colour of the clip beneath —
        // so anything that reads only the document cannot tell these two apart.
        let over_white = findings(
            &rendered("low-contrast", white_on_white, white_on_dark),
            white_on_white,
        );
        let dark = findings(
            &rendered("low-contrast", white_on_white, white_on_dark),
            white_on_dark,
        );
        let failing = over_white
            .iter()
            .find(|finding| finding.rule == "low-contrast")
            .expect("white on white must fail");
        assert!(
            failing.value.expect("ratio") < 1.5,
            "white on white should be near 1:1, got {:?}",
            failing.value
        );
        assert!(
            !dark.iter().any(|finding| finding.rule == "low-contrast"),
            "white on a dark clip is legible; saw {:?}",
            rules_seen(&dark)
        );
    }

    #[test]
    fn a_caption_at_the_style_ceiling_passes_and_one_above_it_does_not() {
        let style = dvs_core::project::CaptionStyle::named("default");
        let mut fixture = Fixture::new([320, 180]);
        let track = fixture.track(TrackKind::Caption);
        // Exactly at the ceiling: 20 non-space characters in one second.
        fixture.cue(
            &track,
            Span::new(Time::ZERO, secs(1, 1)),
            "abcdefghijklmnopqrst",
        );
        let sequence = fixture.seq();
        let findings = lint(
            &fixture.ws,
            tool(),
            &sequence,
            &LintOptions {
                render: false,
                ..LintOptions::default()
            },
        )
        .expect("lint");
        assert_eq!(style.max_cps, 20.0);
        assert!(
            !findings
                .iter()
                .any(|finding| finding.rule == "caption-too-fast"),
            "a cue exactly at the ceiling is inside spec"
        );
    }

    #[test]
    fn severity_orders_so_the_worst_finding_decides_the_exit_code() {
        assert!(Severity::Error > Severity::Warning);
        assert!(Severity::Warning > Severity::Info);
        let mut fixture = Fixture::new([320, 180]);
        reads_past_source(&mut fixture);
        let sequence = fixture.seq();
        let findings = lint(
            &fixture.ws,
            tool(),
            &sequence,
            &LintOptions {
                render: false,
                ..LintOptions::default()
            },
        )
        .expect("lint");
        assert_eq!(
            findings.iter().map(|f| f.severity).max(),
            Some(Severity::Error)
        );
    }

    #[test]
    fn profiles_carry_the_published_targets() {
        assert_eq!(LoudnessProfile::parse("yt").expect("alias"), LoudnessProfile::YouTube);
        assert_eq!(
            LoudnessProfile::parse("r128").expect("alias"),
            LoudnessProfile::Broadcast
        );
        assert_eq!(LoudnessProfile::YouTube.target_lufs(), -14.0);
        assert_eq!(LoudnessProfile::Podcast.target_lufs(), -16.0);
        assert_eq!(LoudnessProfile::Broadcast.target_lufs(), -23.0);
        assert!(LoudnessProfile::parse("spotify-ish").is_err());
    }

    /// Every finding's `target` has to be a selector that actually names something.
    ///
    /// This is the property that makes a finding actionable rather than informative, and it
    /// is easy to break by accident — a track with a time window (`trk_x@2/1-4/1`) parses
    /// and then matches nothing, because a track is not a thing that exists between two
    /// instants.
    fn assert_targets_resolve(fixture: &Fixture, findings: &[Finding]) {
        let sequence = fixture.seq();
        assert!(!findings.is_empty(), "the fixture produced no findings");
        for finding in findings {
            let parsed = dvs_core::selector::Selector::parse(&finding.target)
                .unwrap_or_else(|error| {
                    panic!("'{}' target '{}' is not a selector: {error}", finding.rule, finding.target)
                });
            let matched =
                dvs_core::selector::resolve(&fixture.ws.project, &sequence, &parsed)
                    .unwrap_or_else(|error| {
                        panic!(
                            "'{}' target '{}' resolves to nothing: {error}",
                            finding.rule, finding.target
                        )
                    });
            assert!(
                !matched.is_empty(),
                "'{}' target '{}' named nothing",
                finding.rule,
                finding.target
            );
        }
    }

    #[test]
    fn every_document_finding_targets_something_that_exists() {
        let mut fixture = Fixture::new([320, 180]);
        // A document that trips as many document rules at once as one can: a hole, a
        // retimed clip over an off-rate source, a fast caption and an orphan asset.
        let track = fixture.video();
        let asset = fixture.asset(
            "talk.mp4",
            video_probe([320, 180], Fps::new(25, 1).expect("25/1"), secs(10, 1)),
            true,
        );
        let mut clip = Clip::new(
            Source::Asset {
                asset,
                stream: None,
            },
            Time::ZERO,
            secs(2, 1),
        );
        clip.speed = Rat::new(1, 2).expect("1/2");
        fixture.push(&track, clip);
        fixture.push(&track, color_clip(BLUE, secs(4, 1), secs(2, 1)));
        fixture.asset("unused.mp4", native_probe(), true);
        unreadably_fast_caption(&mut fixture);
        overflowing_title(&mut fixture);

        let sequence = fixture.seq();
        let findings = lint(
            &fixture.ws,
            tool(),
            &sequence,
            &LintOptions {
                render: false,
                ..LintOptions::default()
            },
        )
        .expect("lint");
        let seen = rules_seen(&findings);
        for expected in ["gap", "speed-frame-drop", "fps-mismatch", "unreferenced-asset"] {
            assert!(seen.contains(&expected), "expected {expected} in {seen:?}");
        }
        assert_targets_resolve(&fixture, &findings);
    }

    #[test]
    fn an_overlay_track_is_not_reported_as_a_gap() {
        // A lower third on V2 is absent for most of the timeline by construction. If that
        // counts as a gap, every real project emits noise and the rule stops being read.
        let case = document("gap", with_hole, adjacent);
        let covered = findings(&case, |f| {
            let base = f.video();
            f.push(&base, color_clip(RED, Time::ZERO, secs(6, 1)));
            let overlay = f.video();
            f.push(&overlay, color_clip(BLUE, secs(2, 1), secs(1, 1)));
        });
        assert!(
            !rules_seen(&covered).contains(&"gap"),
            "a covered overlay reported a gap: {covered:#?}"
        );

        // A hole nothing covers is still a hole, even with a second track present.
        let uncovered = findings(&case, |f| {
            let base = f.video();
            f.push(&base, color_clip(RED, Time::ZERO, secs(2, 1)));
            f.push(&base, color_clip(RED, secs(4, 1), secs(2, 1)));
            let overlay = f.video();
            f.push(&overlay, color_clip(BLUE, Time::ZERO, secs(1, 1)));
        });
        assert!(
            rules_seen(&uncovered).contains(&"gap"),
            "an uncovered hole must still be reported: {uncovered:#?}"
        );
    }

    #[test]
    fn every_audio_finding_targets_something_that_exists() {
        let mut fixture = Fixture::new([320, 180]);
        hole_in_the_audio(&mut fixture);
        let sequence = fixture.seq();
        let findings = lint(&fixture.ws, tool(), &sequence, &LintOptions::default()).expect("lint");
        let audio: Vec<Finding> = findings
            .into_iter()
            .filter(|finding| {
                matches!(
                    finding.rule,
                    "silence-gap" | "loudness-out-of-spec" | "true-peak" | "clipping"
                )
            })
            .collect();
        assert!(
            audio.iter().any(|finding| finding.rule == "silence-gap"),
            "the fixture has a hole in its audio"
        );
        assert_targets_resolve(&fixture, &audio);
    }
}


