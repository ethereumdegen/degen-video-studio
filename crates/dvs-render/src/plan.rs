//! Where to cut the timeline into independently encodable pieces.
//!
//! A segment is the unit of cache reuse, so the cut points decide how much work one edit
//! costs. Two failure modes bracket the choice:
//!
//! - **Cut too rarely** and a one-second title change re-encodes ten minutes, which is the
//!   thing this crate exists to prevent.
//! - **Cut too often** and the render pays an ffmpeg process spawn, a container header and
//!   an extra keyframe for three frames of video. A closed-GOP segment shorter than its own
//!   GOP is also all-intra, which is both slow to encode and large on disk.
//!
//! So cuts land exactly where the composition changes discontinuously — a clip appearing or
//! disappearing on any visible track, a transition starting or ending, a caption cue edge, a
//! keyframe — and anything shorter than [`min_segment`] is merged into its neighbour.
//!
//! Every cut is snapped to the frame grid before it is used. An off-grid boundary would put
//! one segment's last frame and the next segment's first frame at the same instant, and
//! `concat` would duplicate it.
//!
//! Audio clip edges are deliberately *not* cut points: a segment carries video only and the
//! mix is bounced once over the whole range (see [`crate::pipeline`]), so an audio edge is
//! not a discontinuity in any encoded frame. Splitting there would spawn an encoder to
//! produce pixels identical to the ones on the other side of the cut.

use dvs_core::error::{Error, Result};
use dvs_core::ids::SequenceId;
use dvs_core::project::{Project, Sequence, TrackKind};
use dvs_core::time::{Fps, Span, Time, R};
use std::ops::Range;

/// Frames per GOP in a segment chunk.
///
/// This mirrors the `-g 60` that [`dvs_media::EncodeSpec::closed_gop`] passes to ffmpeg. A
/// segment shorter than one GOP cannot amortise its keyframe, which is why it is also the
/// floor for [`min_segment`]; if the encoder's GOP length changes, this constant moves with
/// it or the plan starts producing all-intra chunks.
pub const GOP_FRAMES: i64 = 60;

/// Default shortest segment: one second.
///
/// Below roughly this length the fixed cost of a segment — spawning ffmpeg, writing a
/// container, forcing an IDR — exceeds what reusing it can ever save.
pub const DEFAULT_MIN_SEGMENT: Time = Time::from_ratio(R::new_raw(1, 1));

/// The effective floor on a segment's length: the requested minimum, but never below one
/// GOP and never below one frame.
pub fn min_segment(fps: Fps, requested: Time) -> Time {
    let gop = Time::from_frames(GOP_FRAMES, fps);
    let frame = fps.frame_duration();
    requested.max(gop).max(frame)
}

/// The half-open frame index range a span covers at `fps`.
///
/// `floor` on the start and `ceil` on the end, so a span that is not frame-aligned still
/// covers every frame it touches rather than dropping a partial one at each edge.
pub fn frame_range(span: Span, fps: Fps) -> Range<i64> {
    span.start.frame_floor(fps)..span.end.frame_ceil(fps)
}

/// Split `range` into segments at every visual discontinuity, with [`DEFAULT_MIN_SEGMENT`]
/// as the floor.
///
/// The returned spans are frame-aligned, contiguous, non-overlapping and their union is
/// exactly the frame-aligned `range`.
pub fn segment_plan(project: &Project, sequence: &SequenceId, range: Span) -> Result<Vec<Span>> {
    segment_plan_with(project, sequence, range, DEFAULT_MIN_SEGMENT)
}

/// [`segment_plan`] with an explicit minimum segment length.
///
/// `min` is a request, not a promise: it is raised to one GOP by [`min_segment`], and a
/// range shorter than the result yields a single segment rather than an empty plan.
pub fn segment_plan_with(
    project: &Project,
    sequence: &SequenceId,
    range: Span,
    min: Time,
) -> Result<Vec<Span>> {
    let seq = project.sequence(sequence)?;
    let fps = seq.fps;
    let range = align(range, fps);
    if range.is_empty() {
        return Err(Error::bad_args(format!(
            "render range {range} is empty at {fps} fps; nothing to segment"
        )));
    }

    let mut cuts: Vec<Time> = cut_points(seq)
        .into_iter()
        .map(|at| at.snap(fps))
        .filter(|at| *at > range.start && *at < range.end)
        .collect();
    cuts.sort();
    cuts.dedup();

    let min = min_segment(fps, min);
    let mut plan: Vec<Span> = Vec::with_capacity(cuts.len() + 1);
    let mut cursor = range.start;
    for cut in cuts {
        push_or_merge(&mut plan, Span::new(cursor, cut), min);
        cursor = cut;
    }
    push_or_merge(&mut plan, Span::new(cursor, range.end), min);

    // The loop can only merge a short segment *forwards*, so a short tail survives it. Fold
    // it back into its predecessor; if there is no predecessor the whole range is shorter
    // than `min` and one undersized segment is the only honest answer.
    if plan.len() > 1 {
        let tail = plan[plan.len() - 1];
        if tail.duration() < min {
            plan.pop();
            if let Some(last) = plan.last_mut() {
                last.end = tail.end;
            }
        }
    }
    Ok(plan)
}

/// Extend the previous segment when it is still shorter than `min`, otherwise start a new
/// one. Extending rather than skipping the cut is what keeps the plan gap-free.
fn push_or_merge(plan: &mut Vec<Span>, span: Span, min: Time) {
    match plan.last_mut() {
        Some(last) if last.duration() < min => last.end = span.end,
        _ => plan.push(span),
    }
}

/// Snap a range outward to the frame grid: a render covers whole frames, and rounding the
/// end inward would silently drop the last frame an agent asked for.
fn align(range: Span, fps: Fps) -> Span {
    Span::new(
        Time::from_frames(range.start.frame_floor(fps), fps),
        Time::from_frames(range.end.frame_ceil(fps), fps),
    )
}

/// Every instant at which the composition changes discontinuously, unfiltered and
/// unsorted. Hidden tracks contribute nothing because they contribute no pixels.
fn cut_points(seq: &Sequence) -> Vec<Time> {
    let mut cuts = Vec::new();
    for track in &seq.tracks {
        if track.hidden {
            continue;
        }
        match track.kind {
            TrackKind::Video => {
                for clip in &track.clips {
                    cuts.push(clip.start);
                    cuts.push(clip.end());
                    if let Some(transition) = &clip.transition_in {
                        if transition.duration.is_positive() {
                            // The incoming clip's start is already a cut; the end of the
                            // mix is where the outgoing clip stops being decoded at all.
                            cuts.push(clip.start + transition.duration);
                        }
                    }
                    for keys in clip.keyframes.values() {
                        for key in keys {
                            // Keyframe times are clip-local; a key outside the clip only
                            // shapes the held value and is not a boundary.
                            let at = clip.start + key.at;
                            if clip.span().contains(at) {
                                cuts.push(at);
                            }
                        }
                    }
                }
            }
            TrackKind::Caption => {
                for cue in &track.cues {
                    cuts.push(cue.span.start);
                    cuts.push(cue.span.end);
                }
            }
            // Audio does not reach a segment: see the module doc.
            TrackKind::Audio => {}
        }
    }
    cuts
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::color::Rgba;
    use dvs_core::ids::CueId;
    use dvs_core::project::{
        CaptionCue, Clip, Easing, Keyframe, Source, Track, Transition, TransitionKind,
    };

    fn fps() -> Fps {
        Fps::new(30, 1).unwrap()
    }

    fn project(clips: Vec<Clip>) -> (Project, SequenceId) {
        let mut project = Project::new("t", fps(), [320, 180], 48_000);
        let id = project.active_sequence.clone();
        let seq = project.sequence_mut(&id).unwrap();
        let mut track = Track::new("V1", TrackKind::Video);
        for clip in clips {
            track.place(clip);
        }
        seq.tracks.push(track);
        (project, id)
    }

    fn color_clip(start: i64, duration: i64) -> Clip {
        Clip::new(
            Source::Color { color: Rgba::WHITE },
            Time::from_secs(start),
            Time::from_secs(duration),
        )
    }

    /// Contiguity, alignment and exact coverage, checked as one property so every test can
    /// assert the plan is *usable* and then assert what is interesting about it.
    fn assert_covers(plan: &[Span], range: Span) {
        assert!(!plan.is_empty(), "a non-empty range must yield a segment");
        assert_eq!(plan[0].start, range.start, "plan must start at the range");
        assert_eq!(
            plan[plan.len() - 1].end,
            range.end,
            "plan must end at the range"
        );
        for pair in plan.windows(2) {
            assert_eq!(
                pair[0].end, pair[1].start,
                "segments must be contiguous and non-overlapping: {} then {}",
                pair[0], pair[1]
            );
        }
        for span in plan {
            assert!(span.duration().is_positive(), "{span} is empty");
            assert!(
                span.start.is_frame_aligned(fps()) && span.end.is_frame_aligned(fps()),
                "{span} is off the frame grid"
            );
        }
    }

    #[test]
    fn three_clips_and_a_transition_cut_at_every_edge() {
        let mut second = color_clip(4, 4);
        second.transition_in = Some(Transition {
            kind: TransitionKind::Dissolve,
            duration: Time::from_secs(2),
            easing: Easing::Linear,
            direction: Default::default(),
            color: None,
        });
        let (project, id) = project(vec![color_clip(0, 4), second, color_clip(8, 4)]);
        let range = Span::new(Time::ZERO, Time::from_secs(12));

        let plan = segment_plan(&project, &id, range).unwrap();
        assert_covers(&plan, range);

        let starts: Vec<Time> = plan.iter().map(|span| span.start).collect();
        for edge in [4, 6, 8] {
            assert!(
                starts.contains(&Time::from_secs(edge)),
                "expected a segment boundary at {edge}s (clip/transition edge), got {starts:?}"
            );
        }
        assert_eq!(
            starts,
            vec![
                Time::ZERO,
                Time::from_secs(4),
                Time::from_secs(6),
                Time::from_secs(8)
            ],
            "only the real discontinuities may become boundaries"
        );
    }

    #[test]
    fn keyframe_and_caption_edges_are_boundaries() {
        let mut clip = color_clip(0, 12);
        clip.keyframes.insert(
            "opacity".to_string(),
            vec![
                Keyframe {
                    at: Time::from_secs(3),
                    value: 0.0,
                    easing: Easing::Linear,
                },
                Keyframe {
                    at: Time::from_secs(6),
                    value: 1.0,
                    easing: Easing::Linear,
                },
            ],
        );
        let (mut project, id) = project(vec![clip]);
        let seq = project.sequence_mut(&id).unwrap();
        let mut captions = Track::new("CC1", TrackKind::Caption);
        captions.cues.push(CaptionCue {
            id: CueId::new(),
            span: Span::new(Time::from_secs(9), Time::from_secs(11)),
            text: "hello".into(),
            style: None,
        });
        seq.tracks.push(captions);
        let range = Span::new(Time::ZERO, Time::from_secs(12));

        let starts: Vec<Time> = segment_plan(&project, &id, range)
            .unwrap()
            .iter()
            .map(|span| span.start)
            .collect();
        assert_eq!(
            starts,
            vec![
                Time::ZERO,
                Time::from_secs(3),
                Time::from_secs(6),
                Time::from_secs(9)
            ],
            "keyframe times and caption cue edges are discontinuities too; the 1 s tail \
             after the last cue is too short to stand alone and folds back"
        );
    }

    #[test]
    fn short_segments_merge_into_their_neighbour() {
        // Half-second clips: every edge is a cut point, none of them can pay for a segment.
        let clips: Vec<Clip> = (0..12)
            .map(|index| {
                Clip::new(
                    Source::Color { color: Rgba::WHITE },
                    Time::new(index, 2).unwrap(),
                    Time::new(1, 2).unwrap(),
                )
            })
            .collect();
        let (project, id) = project(clips);
        let range = Span::new(Time::ZERO, Time::from_secs(6));

        let plan = segment_plan(&project, &id, range).unwrap();
        assert_covers(&plan, range);
        let min = min_segment(fps(), DEFAULT_MIN_SEGMENT);
        assert!(
            plan.len() < 12,
            "twelve half-second clips must not become twelve segments: {plan:?}"
        );
        for span in &plan {
            assert!(
                span.duration() >= min,
                "{span} is shorter than the {min}s minimum"
            );
        }
    }

    #[test]
    fn a_single_clip_timeline_is_one_segment() {
        let (project, id) = project(vec![color_clip(0, 10)]);
        let range = Span::new(Time::ZERO, Time::from_secs(10));
        let plan = segment_plan(&project, &id, range).unwrap();
        assert_eq!(plan, vec![range], "nothing changes inside a single clip");
    }

    #[test]
    fn a_range_shorter_than_the_minimum_still_renders_as_one_segment() {
        let (project, id) = project(vec![color_clip(0, 1), color_clip(1, 1)]);
        let range = Span::new(Time::ZERO, Time::from_secs(2));
        assert_eq!(
            segment_plan(&project, &id, range).unwrap(),
            vec![range],
            "a two-second range cannot be split into GOP-sized pieces"
        );
    }

    #[test]
    fn a_sub_range_is_planned_on_its_own_edges() {
        let (project, id) = project(vec![color_clip(0, 4), color_clip(4, 4), color_clip(8, 4)]);
        let range = Span::new(Time::from_secs(2), Time::from_secs(10));
        let plan = segment_plan(&project, &id, range).unwrap();
        assert_covers(&plan, range);
        assert_eq!(
            plan,
            vec![
                Span::new(Time::from_secs(2), Time::from_secs(4)),
                Span::new(Time::from_secs(4), Time::from_secs(8)),
                Span::new(Time::from_secs(8), Time::from_secs(10)),
            ],
            "boundaries outside the requested range are not cut points"
        );
    }

    #[test]
    fn boundaries_snap_to_the_frame_grid_on_a_fractional_rate() {
        let ndf = Fps::new(30_000, 1001).unwrap();
        let mut project = Project::new("t", ndf, [320, 180], 48_000);
        let id = project.active_sequence.clone();
        let seq = project.sequence_mut(&id).unwrap();
        let mut track = Track::new("V1", TrackKind::Video);
        // 4.0 s is not on the 30000/1001 grid; the cut must land on a frame anyway.
        track.place(color_clip(0, 4));
        track.place(color_clip(4, 4));
        seq.tracks.push(track);

        let range = Span::new(Time::ZERO, Time::from_secs(8));
        let plan = segment_plan(&project, &id, range).unwrap();
        for span in &plan {
            assert!(
                span.start.is_frame_aligned(ndf) && span.end.is_frame_aligned(ndf),
                "{span} is not on the 30000/1001 grid"
            );
        }
        assert_eq!(plan[0].start, Time::ZERO);
        assert_eq!(plan.last().unwrap().end, Time::from_frames(240, ndf));
        assert_eq!(
            plan[1].start,
            Time::from_frames(120, ndf),
            "the 4 s clip edge snaps to frame 120, not to 4/1 s"
        );
    }

    #[test]
    fn an_empty_range_is_an_error_not_an_empty_plan() {
        let (project, id) = project(vec![color_clip(0, 4)]);
        let range = Span::new(Time::from_secs(2), Time::from_secs(2));
        assert!(
            segment_plan(&project, &id, range).is_err(),
            "an empty range must not silently produce an empty render"
        );
    }
}
