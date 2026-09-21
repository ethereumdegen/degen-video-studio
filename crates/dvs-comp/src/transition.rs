//! Transitions: two frames and a progress value in, one frame out.
//!
//! The document keeps clips strictly non-overlapping and puts the overlap in a
//! `transitionIn` on the later clip. That choice pushes one requirement here: during a
//! transition the *outgoing* clip has already ended on the timeline, so the compositor has
//! to read past its out-point — the handles a real NLE also needs. This module does not
//! care where the frames came from; it only knows how to mix them.
//!
//! Progress is eased by the transition's own easing curve before it arrives, so a dissolve
//! with `ease-in-out` is a property of the document rather than a hardcoded curve.

use crate::blend::lerp_pixel;
use dvs_core::color::Rgba;
use dvs_core::project::{Direction, Transition, TransitionKind};
use dvs_media::Frame;

/// Mix `outgoing` and `incoming` at `progress` in 0..1.
///
/// `progress` is the already-eased fraction of the transition that has elapsed: 0 is
/// entirely the outgoing clip, 1 entirely the incoming one.
pub fn mix(outgoing: &Frame, incoming: &Frame, transition: &Transition, progress: f32) -> Frame {
    let progress = progress.clamp(0.0, 1.0);
    let size = incoming.size().max_with(outgoing.size());
    match transition.kind {
        TransitionKind::Cut => {
            if progress >= 1.0 {
                incoming.clone()
            } else {
                outgoing.clone()
            }
        }
        TransitionKind::Dissolve => dissolve(outgoing, incoming, progress, size),
        TransitionKind::Dip => dip(
            outgoing,
            incoming,
            progress,
            transition.color.unwrap_or(Rgba::BLACK),
            size,
        ),
        TransitionKind::Wipe => wipe(outgoing, incoming, progress, transition.direction, size),
        TransitionKind::Slide => {
            slide(outgoing, incoming, progress, transition.direction, size, false)
        }
        TransitionKind::Push => {
            slide(outgoing, incoming, progress, transition.direction, size, true)
        }
    }
}

/// Helper so a transition between differently sized frames uses the larger canvas rather
/// than silently cropping one of them.
trait SizeExt {
    fn max_with(self, other: [u32; 2]) -> [u32; 2];
}

impl SizeExt for [u32; 2] {
    fn max_with(self, other: [u32; 2]) -> [u32; 2] {
        [self[0].max(other[0]), self[1].max(other[1])]
    }
}

fn dissolve(outgoing: &Frame, incoming: &Frame, progress: f32, size: [u32; 2]) -> Frame {
    let mut out = Frame::transparent(size[0], size[1]);
    for y in 0..size[1] {
        for x in 0..size[0] {
            out.set_pixel(
                x,
                y,
                lerp_pixel(outgoing.pixel(x, y), incoming.pixel(x, y), progress),
            );
        }
    }
    out
}

/// Dip: outgoing → color over the first half, color → incoming over the second. The
/// midpoint is fully the dip color, which is what makes a dip-to-black read as a beat
/// rather than as a fast dissolve.
fn dip(
    outgoing: &Frame,
    incoming: &Frame,
    progress: f32,
    color: Rgba,
    size: [u32; 2],
) -> Frame {
    let dip_color = color.to_linear_premul();
    let mut out = Frame::transparent(size[0], size[1]);
    for y in 0..size[1] {
        for x in 0..size[0] {
            let pixel = if progress < 0.5 {
                lerp_pixel(outgoing.pixel(x, y), dip_color, progress * 2.0)
            } else {
                lerp_pixel(dip_color, incoming.pixel(x, y), (progress - 0.5) * 2.0)
            };
            out.set_pixel(x, y, pixel);
        }
    }
    out
}

/// Wipe: a hard edge sweeping across the frame, one pixel of feather so the boundary does
/// not jitter frame to frame.
///
/// `direction` is the direction the *motion* travels, which is the same convention slide
/// and push use: `Left` means the incoming clip arrives from the right edge and the
/// boundary travels leftward. Having one of the three read the other way round would make
/// `--direction left` mean two different things depending on `--kind`.
fn wipe(
    outgoing: &Frame,
    incoming: &Frame,
    progress: f32,
    direction: Direction,
    size: [u32; 2],
) -> Frame {
    let mut out = Frame::transparent(size[0], size[1]);
    let width = size[0] as f32;
    let height = size[1] as f32;
    for y in 0..size[1] {
        for x in 0..size[0] {
            let position = match direction {
                Direction::Left => 1.0 - (x as f32 + 0.5) / width,
                Direction::Right => (x as f32 + 0.5) / width,
                Direction::Up => 1.0 - (y as f32 + 0.5) / height,
                Direction::Down => (y as f32 + 0.5) / height,
            };
            // One-pixel ramp at the edge: `progress` is continuous but pixels are not, and
            // a binary test makes the edge visibly jitter frame to frame.
            let feather = 1.0 / width.max(height);
            let mix = ((progress - position) / feather + 0.5).clamp(0.0, 1.0);
            out.set_pixel(
                x,
                y,
                lerp_pixel(outgoing.pixel(x, y), incoming.pixel(x, y), mix),
            );
        }
    }
    out
}

/// Slide (incoming moves over a static outgoing) and push (both move together).
fn slide(
    outgoing: &Frame,
    incoming: &Frame,
    progress: f32,
    direction: Direction,
    size: [u32; 2],
    push: bool,
) -> Frame {
    let (width, height) = (size[0] as i64, size[1] as i64);
    let offset = match direction {
        Direction::Left => [(width as f32 * (1.0 - progress)) as i64, 0],
        Direction::Right => [-(width as f32 * (1.0 - progress)) as i64, 0],
        Direction::Up => [0, (height as f32 * (1.0 - progress)) as i64],
        Direction::Down => [0, -(height as f32 * (1.0 - progress)) as i64],
    };
    let mut out = Frame::transparent(size[0], size[1]);
    for y in 0..size[1] {
        for x in 0..size[0] {
            let (ox, oy) = if push {
                // The outgoing clip travels the opposite way by the remaining distance.
                (
                    x as i64 + (offset[0] - offset[0].signum() * width),
                    y as i64 + (offset[1] - offset[1].signum() * height),
                )
            } else {
                (x as i64, y as i64)
            };
            let base = if (0..width).contains(&ox) && (0..height).contains(&oy) {
                outgoing.pixel(ox as u32, oy as u32)
            } else {
                [0.0; 4]
            };
            let ix = x as i64 - offset[0];
            let iy = y as i64 - offset[1];
            let over = if (0..width).contains(&ix) && (0..height).contains(&iy) {
                incoming.pixel(ix as u32, iy as u32)
            } else {
                [0.0; 4]
            };
            // The incoming frame is opaque where it exists, so source-over is the mix.
            let inv = 1.0 - over[3];
            out.set_pixel(
                x,
                y,
                [
                    over[0] + base[0] * inv,
                    over[1] + base[1] * inv,
                    over[2] + base[2] * inv,
                    over[3] + base[3] * inv,
                ],
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::project::Easing;
    use dvs_core::time::Time;

    fn transition(kind: TransitionKind, direction: Direction) -> Transition {
        Transition {
            kind,
            duration: Time::from_secs(1),
            easing: Easing::Linear,
            direction,
            color: None,
        }
    }

    fn frames() -> (Frame, Frame) {
        (
            Frame::filled(8, 8, Rgba::opaque(255, 0, 0)),
            Frame::filled(8, 8, Rgba::opaque(0, 0, 255)),
        )
    }

    #[test]
    fn every_transition_returns_the_endpoints_exactly() {
        let (out, incoming) = frames();
        for kind in [
            TransitionKind::Cut,
            TransitionKind::Dissolve,
            TransitionKind::Dip,
            TransitionKind::Wipe,
            TransitionKind::Slide,
            TransitionKind::Push,
        ] {
            let t = transition(kind, Direction::Left);
            let at_start = mix(&out, &incoming, &t, 0.0);
            let at_end = mix(&out, &incoming, &t, 1.0);
            assert_eq!(
                at_start.pixel(4, 4),
                out.pixel(4, 4),
                "{kind:?} at progress 0 must be the outgoing clip"
            );
            assert_eq!(
                at_end.pixel(4, 4),
                incoming.pixel(4, 4),
                "{kind:?} at progress 1 must be the incoming clip"
            );
        }
    }

    #[test]
    fn a_dissolve_midpoint_is_half_of_each_in_linear_light() {
        let (out, incoming) = frames();
        let mid = mix(&out, &incoming, &transition(TransitionKind::Dissolve, Direction::Left), 0.5);
        let pixel = mid.pixel(4, 4);
        assert!((pixel[0] - out.pixel(4, 4)[0] / 2.0).abs() < 1e-6, "{pixel:?}");
        assert!((pixel[2] - incoming.pixel(4, 4)[2] / 2.0).abs() < 1e-6, "{pixel:?}");
        assert!((pixel[3] - 1.0).abs() < 1e-6, "alpha must stay opaque");
    }

    #[test]
    fn a_dip_is_fully_the_dip_color_at_its_midpoint() {
        let (out, incoming) = frames();
        let mut t = transition(TransitionKind::Dip, Direction::Left);
        t.color = Some(Rgba::BLACK);
        let mid = mix(&out, &incoming, &t, 0.5);
        let pixel = mid.pixel(4, 4);
        assert!(pixel[0] < 1e-6 && pixel[2] < 1e-6, "midpoint should be black: {pixel:?}");
        // A dip is not a dissolve: at the quarter point it is darker than a dissolve would be.
        let quarter_dip = mix(&out, &incoming, &t, 0.25).pixel(4, 4);
        let quarter_dissolve = mix(
            &out,
            &incoming,
            &transition(TransitionKind::Dissolve, Direction::Left),
            0.25,
        )
        .pixel(4, 4);
        let luma = |p: [f32; 4]| p[0] + p[1] + p[2];
        assert!(luma(quarter_dip) < luma(quarter_dissolve), "{quarter_dip:?}");
    }

    #[test]
    fn a_wipe_edge_sits_where_progress_says() {
        let (out, incoming) = frames();
        let t = transition(TransitionKind::Wipe, Direction::Left);
        let half = mix(&out, &incoming, &t, 0.5);
        // Motion travels left, so the incoming clip has taken the right half and the
        // boundary is still to come on the left.
        assert!(half.pixel(6, 4)[2] > 0.5, "right edge should be incoming");
        assert!(half.pixel(1, 4)[0] > 0.5, "left edge should still be outgoing");
    }

    #[test]
    fn wipe_direction_reverses_which_side_changes_first() {
        let (out, incoming) = frames();
        let left = mix(&out, &incoming, &transition(TransitionKind::Wipe, Direction::Left), 0.25);
        let right = mix(&out, &incoming, &transition(TransitionKind::Wipe, Direction::Right), 0.25);
        assert!(left.pixel(7, 4)[2] > 0.5 && left.pixel(0, 4)[0] > 0.5);
        assert!(right.pixel(0, 4)[2] > 0.5 && right.pixel(7, 4)[0] > 0.5);
    }

    #[test]
    fn a_slide_leaves_the_outgoing_clip_in_place_while_push_moves_it() {
        let (out, incoming) = frames();
        let slid = mix(&out, &incoming, &transition(TransitionKind::Slide, Direction::Left), 0.5);
        let pushed = mix(&out, &incoming, &transition(TransitionKind::Push, Direction::Left), 0.5);
        // Motion travels left: halfway through, the incoming clip covers the right half and
        // the left half still shows the untouched outgoing frame. A push instead carries the
        // outgoing frame along, so the same pixel differs.
        assert!(slid.pixel(1, 4)[0] > 0.5, "slide keeps the outgoing frame: {:?}", slid.pixel(1, 4));
        assert!(slid.pixel(6, 4)[2] > 0.5, "incoming frame has arrived on the right");
        assert!(
            pushed.pixel(1, 4) != slid.pixel(1, 4),
            "push must displace the outgoing frame"
        );
    }

    #[test]
    fn a_cut_never_produces_an_intermediate_frame() {
        let (out, incoming) = frames();
        let t = transition(TransitionKind::Cut, Direction::Left);
        for progress in [0.1, 0.5, 0.9] {
            assert_eq!(mix(&out, &incoming, &t, progress).pixel(4, 4), out.pixel(4, 4));
        }
    }
}
