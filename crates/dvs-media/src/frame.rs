//! The pixel buffer everything in the render path speaks.
//!
//! Premultiplied, linear-light, f32 RGBA. Three deliberate choices:
//!
//! - **Linear light**, because a dissolve between black and white must pass through 50%
//!   *light*, not 50% *encoded value* — the latter is the classic too-dark cross-fade.
//! - **Premultiplied**, because compositing straight alpha requires a divide per pixel per
//!   layer and produces fringes wherever alpha is 0.
//! - **f32**, because a 30-layer stack in 8-bit accumulates visible banding, and because
//!   effects (grade, blur) want headroom above 1.0 rather than clipping mid-chain.
//!
//! The conversion to and from 8-bit sRGB happens exactly twice per frame: once when ffmpeg
//! hands us decoded bytes, once when we hand bytes back to the encoder.

use dvs_core::color::{linear_to_srgb, Rgba};
use std::sync::LazyLock;

/// sRGB byte → linear float. 256 entries, so decode never calls `powf`.
static SRGB_TO_LINEAR: LazyLock<[f32; 256]> = LazyLock::new(|| {
    let mut table = [0.0f32; 256];
    for (value, slot) in table.iter_mut().enumerate() {
        *slot = dvs_core::color::srgb_to_linear(value as u8);
    }
    table
});

/// Linear float → sRGB byte, quantized through a 4096-entry table. Encoding a 4K frame is
/// 8.3 M `powf` calls otherwise, which dominates the render loop.
static LINEAR_TO_SRGB: LazyLock<[u8; 4096]> = LazyLock::new(|| {
    let mut table = [0u8; 4096];
    for (index, slot) in table.iter_mut().enumerate() {
        let linear = index as f32 / (4096.0 - 1.0);
        *slot = (linear_to_srgb(linear) * 255.0 + 0.5) as u8;
    }
    table
});

fn encode_linear(value: f32) -> u8 {
    let clamped = value.clamp(0.0, 1.0);
    LINEAR_TO_SRGB[(clamped * 4095.0 + 0.5) as usize]
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    width: u32,
    height: u32,
    /// `width * height * 4`, premultiplied linear RGBA.
    pixels: Vec<f32>,
}

impl Frame {
    pub fn transparent(width: u32, height: u32) -> Frame {
        Frame {
            width,
            height,
            pixels: vec![0.0; (width as usize) * (height as usize) * 4],
        }
    }

    pub fn filled(width: u32, height: u32, color: Rgba) -> Frame {
        let premul = color.to_linear_premul();
        let mut pixels = Vec::with_capacity((width as usize) * (height as usize) * 4);
        for _ in 0..(width as usize) * (height as usize) {
            pixels.extend_from_slice(&premul);
        }
        Frame {
            width,
            height,
            pixels,
        }
    }

    pub fn from_pixels(width: u32, height: u32, pixels: Vec<f32>) -> Frame {
        debug_assert_eq!(pixels.len(), (width as usize) * (height as usize) * 4);
        Frame {
            width,
            height,
            pixels,
        }
    }

    /// Decoded ffmpeg output: full-range sRGB-encoded RGBA bytes, straight alpha.
    pub fn from_rgba8(width: u32, height: u32, bytes: &[u8]) -> Frame {
        let table = &*SRGB_TO_LINEAR;
        let count = (width as usize) * (height as usize);
        let mut pixels = vec![0.0f32; count * 4];
        for (index, chunk) in bytes.chunks_exact(4).take(count).enumerate() {
            let alpha = chunk[3] as f32 / 255.0;
            let out = &mut pixels[index * 4..index * 4 + 4];
            // Premultiply on the way in; alpha itself is linear.
            out[0] = table[chunk[0] as usize] * alpha;
            out[1] = table[chunk[1] as usize] * alpha;
            out[2] = table[chunk[2] as usize] * alpha;
            out[3] = alpha;
        }
        Frame {
            width,
            height,
            pixels,
        }
    }

    /// Bytes for the encoder: sRGB-encoded RGBA, straight alpha.
    pub fn to_rgba8(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.pixels.len()];
        for (index, chunk) in self.pixels.chunks_exact(4).enumerate() {
            let alpha = chunk[3];
            let slot = &mut out[index * 4..index * 4 + 4];
            if alpha <= f32::EPSILON {
                slot.copy_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let inv = 1.0 / alpha;
            slot[0] = encode_linear(chunk[0] * inv);
            slot[1] = encode_linear(chunk[1] * inv);
            slot[2] = encode_linear(chunk[2] * inv);
            slot[3] = (alpha.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
        out
    }

    /// Bytes for a video encoder, which has no alpha: composite over `background` first.
    /// Skipping this step is how a title with soft edges ends up with black fringing.
    pub fn to_rgb8_over(&self, background: Rgba) -> Vec<u8> {
        let back = background.to_linear_premul();
        let count = (self.width as usize) * (self.height as usize);
        let mut out = vec![0u8; count * 3];
        for (index, chunk) in self.pixels.chunks_exact(4).enumerate() {
            let inv = 1.0 - chunk[3];
            let slot = &mut out[index * 3..index * 3 + 3];
            slot[0] = encode_linear(chunk[0] + back[0] * inv);
            slot[1] = encode_linear(chunk[1] + back[1] * inv);
            slot[2] = encode_linear(chunk[2] + back[2] * inv);
        }
        out
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn size(&self) -> [u32; 2] {
        [self.width, self.height]
    }

    pub fn pixels(&self) -> &[f32] {
        &self.pixels
    }

    pub fn pixels_mut(&mut self) -> &mut [f32] {
        &mut self.pixels
    }

    pub fn into_pixels(self) -> Vec<f32> {
        self.pixels
    }

    /// Premultiplied linear RGBA at a pixel. Out of bounds reads are transparent, which is
    /// what every sampling path wants at an edge.
    pub fn pixel(&self, x: u32, y: u32) -> [f32; 4] {
        if x >= self.width || y >= self.height {
            return [0.0; 4];
        }
        let index = ((y as usize) * (self.width as usize) + x as usize) * 4;
        [
            self.pixels[index],
            self.pixels[index + 1],
            self.pixels[index + 2],
            self.pixels[index + 3],
        ]
    }

    pub fn set_pixel(&mut self, x: u32, y: u32, value: [f32; 4]) {
        if x >= self.width || y >= self.height {
            return;
        }
        let index = ((y as usize) * (self.width as usize) + x as usize) * 4;
        self.pixels[index..index + 4].copy_from_slice(&value);
    }

    /// Bilinear sample in pixel coordinates, premultiplied so the interpolation is correct
    /// across an alpha edge without a divide.
    pub fn sample_bilinear(&self, x: f32, y: f32) -> [f32; 4] {
        let x0 = x.floor();
        let y0 = y.floor();
        let fx = x - x0;
        let fy = y - y0;
        let (x0, y0) = (x0 as i64, y0 as i64);
        let mut out = [0.0f32; 4];
        for (dx, dy, weight) in [
            (0i64, 0i64, (1.0 - fx) * (1.0 - fy)),
            (1, 0, fx * (1.0 - fy)),
            (0, 1, (1.0 - fx) * fy),
            (1, 1, fx * fy),
        ] {
            if weight <= 0.0 {
                continue;
            }
            let (sx, sy) = (x0 + dx, y0 + dy);
            if sx < 0 || sy < 0 {
                continue;
            }
            let sample = self.pixel(sx as u32, sy as u32);
            for channel in 0..4 {
                out[channel] += sample[channel] * weight;
            }
        }
        out
    }

    /// Mean linear RGB of the *visible* image, over black. Used by the digest and by the
    /// black-frame detector.
    pub fn mean_linear_rgb(&self) -> [f32; 3] {
        let count = (self.width as usize) * (self.height as usize);
        if count == 0 {
            return [0.0; 3];
        }
        let mut sum = [0.0f64; 3];
        for chunk in self.pixels.chunks_exact(4) {
            sum[0] += chunk[0] as f64;
            sum[1] += chunk[1] as f64;
            sum[2] += chunk[2] as f64;
        }
        [
            (sum[0] / count as f64) as f32,
            (sum[1] / count as f64) as f32,
            (sum[2] / count as f64) as f32,
        ]
    }

    /// Fraction of pixels with any coverage. `0.0` means the frame is empty, which is what
    /// the `invisible-layer` and `gap` diagnostics key on.
    pub fn alpha_coverage(&self) -> f32 {
        let count = (self.width as usize) * (self.height as usize);
        if count == 0 {
            return 0.0;
        }
        let covered = self
            .pixels
            .chunks_exact(4)
            .filter(|chunk| chunk[3] > 1.0 / 255.0)
            .count();
        covered as f32 / count as f32
    }

    /// Mean relative luminance in linear light, for black/flash detection.
    pub fn mean_luma(&self) -> f32 {
        let [r, g, b] = self.mean_linear_rgb();
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_round_trip_is_lossless_for_opaque_pixels() {
        let bytes: Vec<u8> = (0..=255u8)
            .flat_map(|v| [v, 255 - v, v / 2, 255])
            .collect();
        let frame = Frame::from_rgba8(256, 1, &bytes);
        assert_eq!(frame.to_rgba8(), bytes);
    }

    #[test]
    fn decode_linearizes_rather_than_copying_bytes() {
        // Mid-gray must land at 21.6% light, not 50%. If this ever reads 0.5, every
        // dissolve and every blur in the engine is wrong.
        let frame = Frame::from_rgba8(1, 1, &[128, 128, 128, 255]);
        let pixel = frame.pixel(0, 0);
        assert!((pixel[0] - 0.2159).abs() < 0.001, "got {pixel:?}");
    }

    #[test]
    fn a_half_transparent_white_composites_to_mid_light_over_black() {
        // The reason the buffer is premultiplied linear: 50% white over black is 50% light,
        // which encodes to 188, not 128.
        let frame = Frame::from_rgba8(1, 1, &[255, 255, 255, 128]);
        let rgb = frame.to_rgb8_over(Rgba::BLACK);
        assert_eq!(rgb.len(), 3);
        assert!(
            (186..=190).contains(&rgb[0]),
            "50% white over black encoded to {}",
            rgb[0]
        );
    }

    #[test]
    fn filled_frames_report_full_coverage_and_their_color() {
        let frame = Frame::filled(4, 4, Rgba::opaque(255, 0, 0));
        assert_eq!(frame.alpha_coverage(), 1.0);
        let mean = frame.mean_linear_rgb();
        assert!((mean[0] - 1.0).abs() < 1e-6 && mean[1] < 1e-6);
        assert_eq!(Frame::transparent(4, 4).alpha_coverage(), 0.0);
    }

    #[test]
    fn bilinear_sampling_interpolates_across_an_alpha_edge_without_fringing() {
        let mut frame = Frame::transparent(2, 1);
        frame.set_pixel(0, 0, Rgba::opaque(255, 255, 255).to_linear_premul());
        let middle = frame.sample_bilinear(0.5, 0.0);
        // Premultiplied interpolation: color and alpha fall off together, so the
        // unpremultiplied color stays white instead of drifting toward black.
        assert!((middle[3] - 0.5).abs() < 1e-6, "alpha {}", middle[3]);
        assert!((middle[0] / middle[3] - 1.0).abs() < 1e-5, "color {middle:?}");
    }

    #[test]
    fn out_of_bounds_reads_are_transparent() {
        let frame = Frame::filled(2, 2, Rgba::WHITE);
        assert_eq!(frame.pixel(5, 5), [0.0; 4]);
        assert_eq!(frame.pixel(0, 0)[3], 1.0);
    }
}
