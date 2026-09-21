//! Synthetic sources: bars, countdown, frame numbers, tone.
//!
//! These exist because an agent building a slate, a leader, or a test pattern should not
//! have to find media first. `frame-numbers` in particular is a verification tool: burning
//! the frame index into the picture is how you prove a cut landed on the frame you asked
//! for, and it is what the frame-exactness tests in this workspace render against.

use crate::title::Rasterizer;
use dvs_core::color::Rgba;
use dvs_core::error::Result;
use dvs_core::project::Generator;
use dvs_core::time::{Fps, Time};
use dvs_media::Frame;

/// SMPTE-style 75% bars, top three quarters, with a black/white/blue PLUGE strip below.
const BARS: [Rgba; 7] = [
    Rgba::opaque(192, 192, 192),
    Rgba::opaque(192, 192, 0),
    Rgba::opaque(0, 192, 192),
    Rgba::opaque(0, 192, 0),
    Rgba::opaque(192, 0, 192),
    Rgba::opaque(192, 0, 0),
    Rgba::opaque(0, 0, 192),
];

/// Render a generator frame.
///
/// `local` is the clip-local time, so a countdown restarts with its clip rather than
/// following the timeline; `duration` is the clip's length, which a countdown needs to know
/// what to count down from.
pub fn render(
    generator: Generator,
    params: &serde_json::Map<String, serde_json::Value>,
    size: [u32; 2],
    local: Time,
    duration: Time,
    fps: Fps,
    rasterizer: &Rasterizer,
) -> Result<Frame> {
    match generator {
        Generator::Bars => Ok(bars(size)),
        // A tone is audio only; its picture is deliberately empty so a tone on a video
        // track does not paint black over the clip beneath it.
        Generator::Tone => Ok(Frame::transparent(size[0], size[1])),
        Generator::Countdown => countdown(size, local, duration, params, rasterizer),
        Generator::FrameNumbers => frame_numbers(size, local, fps, params, rasterizer),
    }
}

/// Whether a generator produces audio rather than picture.
pub fn is_audio(generator: Generator) -> bool {
    matches!(generator, Generator::Tone)
}

/// Sample value of the 1 kHz reference tone at a sample index. Used by the mixer.
pub fn tone_sample(index: i64, rate: u32, frequency: f64, amplitude: f64) -> f32 {
    let phase = (index as f64 / rate as f64) * frequency * std::f64::consts::TAU;
    (phase.sin() * amplitude) as f32
}

fn bars(size: [u32; 2]) -> Frame {
    let (width, height) = (size[0].max(1), size[1].max(1));
    let mut frame = Frame::transparent(width, height);
    let bar_bottom = (height as f32 * 0.75) as u32;
    let bar_width = width as f32 / BARS.len() as f32;
    for y in 0..height {
        for x in 0..width {
            let color = if y < bar_bottom {
                BARS[((x as f32 / bar_width) as usize).min(BARS.len() - 1)]
            } else {
                // PLUGE: black, 4% gray, white blocks for checking black level and clipping.
                match (x as f32 / (width as f32 / 3.0)) as u32 {
                    0 => Rgba::BLACK,
                    1 => Rgba::opaque(10, 10, 10),
                    _ => Rgba::WHITE,
                }
            };
            frame.set_pixel(x, y, color.to_linear_premul());
        }
    }
    frame
}

fn countdown(
    size: [u32; 2],
    local: Time,
    duration: Time,
    params: &serde_json::Map<String, serde_json::Value>,
    rasterizer: &Rasterizer,
) -> Result<Frame> {
    let remaining = (duration - local).max(Time::ZERO).as_secs_f64().ceil() as i64;
    let background = params
        .get("background")
        .and_then(|v| v.as_str())
        .map(Rgba::parse)
        .transpose()?
        .unwrap_or(Rgba::opaque(8, 10, 14));
    let color = params
        .get("color")
        .and_then(|v| v.as_str())
        .map(Rgba::parse)
        .transpose()?
        .unwrap_or(Rgba::WHITE);
    let svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}">
  <rect width="{w}" height="{h}" fill="{bg}"/>
  <circle cx="{cx}" cy="{cy}" r="{r}" fill="none" stroke="{fg}" stroke-opacity="0.35" stroke-width="{sw}"/>
  <text x="{cx}" y="{ty}" font-family="sans-serif" font-size="{fs}" fill="{fg}"
        text-anchor="middle">{value}</text>
</svg>"#,
        w = size[0].max(1),
        h = size[1].max(1),
        bg = background,
        fg = color,
        cx = size[0] as f32 / 2.0,
        cy = size[1] as f32 / 2.0,
        r = size[1] as f32 * 0.3,
        sw = (size[1] as f32 * 0.01).max(1.0),
        ty = size[1] as f32 / 2.0 + size[1] as f32 * 0.16,
        fs = size[1] as f32 * 0.45,
        value = remaining.max(0)
    );
    rasterizer.rasterize(&svg, size)
}

fn frame_numbers(
    size: [u32; 2],
    local: Time,
    fps: Fps,
    params: &serde_json::Map<String, serde_json::Value>,
    rasterizer: &Rasterizer,
) -> Result<Frame> {
    let index = local.frame_floor(fps);
    let background = params
        .get("background")
        .and_then(|v| v.as_str())
        .map(Rgba::parse)
        .transpose()?
        .unwrap_or(Rgba::opaque(16, 16, 16));
    // Alternating tint per frame: two consecutive frames are then distinguishable even in a
    // thumbnail where the digits are too small to read.
    let tint = if index % 2 == 0 {
        Rgba::opaque(16, 16, 16)
    } else {
        Rgba::opaque(32, 32, 32)
    };
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}">
  <rect width="{w}" height="{h}" fill="{bg}"/>
  <rect y="0" width="{w}" height="{strip}" fill="{tint}"/>
  <text x="{cx}" y="{ty}" font-family="monospace" font-size="{fs}" fill="#ffffff"
        text-anchor="middle">{index}</text>
  <text x="{cx}" y="{tcy}" font-family="monospace" font-size="{tcfs}" fill="#9fb3c8"
        text-anchor="middle">{timecode}</text>
</svg>"##,
        w = size[0].max(1),
        h = size[1].max(1),
        bg = background,
        tint = tint,
        strip = (size[1] as f32 * 0.08).max(2.0),
        cx = size[0] as f32 / 2.0,
        ty = size[1] as f32 * 0.55,
        fs = size[1] as f32 * 0.3,
        tcy = size[1] as f32 * 0.75,
        tcfs = size[1] as f32 * 0.09,
        index = index,
        timecode = local.timecode(fps)
    );
    rasterizer.rasterize(&svg, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fps() -> Fps {
        Fps::new(30, 1).unwrap()
    }

    fn empty() -> serde_json::Map<String, serde_json::Value> {
        serde_json::Map::new()
    }

    #[test]
    fn bars_paint_seven_distinct_columns_and_a_pluge_strip() {
        let frame = bars([700, 400]);
        let sample_y = 100;
        let mut seen = Vec::new();
        for index in 0..7u32 {
            let x = index * 100 + 50;
            let pixel = frame.pixel(x, sample_y);
            assert!(pixel[3] > 0.99, "bars must be opaque");
            seen.push([pixel[0], pixel[1], pixel[2]]);
        }
        for (i, a) in seen.iter().enumerate() {
            for b in seen.iter().skip(i + 1) {
                assert!(
                    (a[0] - b[0]).abs() + (a[1] - b[1]).abs() + (a[2] - b[2]).abs() > 0.01,
                    "two bars are the same color: {a:?} {b:?}"
                );
            }
        }
        // Bottom quarter is the PLUGE: black on the left, white on the right.
        assert!(frame.pixel(50, 380)[0] < 0.01);
        assert!(frame.pixel(650, 380)[0] > 0.9);
    }

    #[test]
    fn a_tone_generator_paints_nothing() {
        let frame = render(
            Generator::Tone,
            &empty(),
            [32, 32],
            Time::ZERO,
            Time::from_secs(1),
            fps(),
            &Rasterizer::new(),
        )
        .unwrap();
        assert_eq!(frame.alpha_coverage(), 0.0);
        assert!(is_audio(Generator::Tone));
        assert!(!is_audio(Generator::Bars));
    }

    #[test]
    fn a_countdown_changes_every_second_and_not_every_frame() {
        let rasterizer = Rasterizer::new();
        let at = |secs: f64| {
            render(
                Generator::Countdown,
                &empty(),
                [160, 120],
                Time::parse(&secs.to_string()).unwrap(),
                Time::from_secs(3),
                fps(),
                &rasterizer,
            )
            .unwrap()
        };
        let first = at(0.0);
        let same_second = at(0.5);
        let next_second = at(1.2);
        assert_eq!(first.pixels(), same_second.pixels(), "digits must hold for a second");
        assert_ne!(first.pixels(), next_second.pixels(), "the digit must change");
    }

    #[test]
    fn frame_numbers_differ_on_consecutive_frames() {
        let rasterizer = Rasterizer::new();
        let frame_of = |index: i64| {
            render(
                Generator::FrameNumbers,
                &empty(),
                [200, 120],
                Time::from_frames(index, fps()),
                Time::from_secs(2),
                fps(),
                &rasterizer,
            )
            .unwrap()
        };
        let a = frame_of(10);
        let b = frame_of(11);
        assert_ne!(a.pixels(), b.pixels(), "frame 10 and 11 must be distinguishable");
        assert_eq!(a.pixels(), frame_of(10).pixels(), "and deterministic");
    }

    #[test]
    fn the_reference_tone_is_a_full_cycle_sine() {
        // A 1 kHz tone at 48 kHz has 48 samples per cycle: sample 12 is the positive peak.
        let peak = tone_sample(12, 48_000, 1000.0, 1.0);
        assert!((peak - 1.0).abs() < 0.01, "got {peak}");
        assert!(tone_sample(0, 48_000, 1000.0, 1.0).abs() < 0.01);
        assert!((tone_sample(36, 48_000, 1000.0, 0.5) + 0.5).abs() < 0.01);
    }

    #[test]
    fn a_bad_color_parameter_is_an_error_not_a_default() {
        let err = render(
            Generator::Countdown,
            &serde_json::Map::from_iter([("color".to_string(), serde_json::json!("nope"))]),
            [64, 64],
            Time::ZERO,
            Time::from_secs(1),
            fps(),
            &Rasterizer::new(),
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), dvs_core::error::exit::BAD_ARGS);
    }
}
