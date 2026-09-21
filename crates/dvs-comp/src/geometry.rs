//! Where a clip's pixels land in the frame, and how they get there.
//!
//! Two steps, kept separate on purpose. [`place`] is pure arithmetic: source size, fit
//! mode, scale, rotation and anchor in, a destination rectangle out — testable without a
//! single pixel. [`blit`] is the only code that writes into the output frame.
//!
//! The fast path matters: an unrotated clip at 1:1 scale is the overwhelmingly common case
//! (a cut between two full-frame shots), and it reduces to a row-wise composite with no
//! per-pixel matrix work. Everything else goes through the inverse-mapped sampler.

use crate::blend::blend_pixel;
use dvs_core::project::{Blend, Crop, Fit, Transform};
use dvs_media::Frame;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn center(&self) -> [f32; 2] {
        [self.x + self.w / 2.0, self.y + self.h / 2.0]
    }

    /// Integer bounds that cover the rect, clamped to a frame size.
    pub fn bounds_in(&self, size: [u32; 2]) -> (u32, u32, u32, u32) {
        let x0 = self.x.floor().max(0.0) as u32;
        let y0 = self.y.floor().max(0.0) as u32;
        let x1 = (self.x + self.w).ceil().clamp(0.0, size[0] as f32) as u32;
        let y1 = (self.y + self.h).ceil().clamp(0.0, size[1] as f32) as u32;
        (x0, y0, x1.max(x0), y1.max(y0))
    }
}

/// A placement: destination rect plus the rotation applied about its center.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub dest: Rect,
    /// Clockwise degrees about the destination center.
    pub rotation: f32,
}

impl Placement {
    /// Whether the placement is an axis-aligned, unscaled, integer-offset copy — the case
    /// worth a fast path.
    pub fn is_pixel_aligned(&self, source: [u32; 2]) -> bool {
        self.rotation.abs() < f32::EPSILON
            && (self.dest.w - source[0] as f32).abs() < 0.01
            && (self.dest.h - source[1] as f32).abs() < 0.01
            && (self.dest.x - self.dest.x.round()).abs() < 0.01
            && (self.dest.y - self.dest.y.round()).abs() < 0.01
    }
}

/// Source size after a crop, and the crop's pixel offset within the source.
pub fn cropped_size(source: [u32; 2], crop: Option<&Crop>) -> ([u32; 2], [f32; 2]) {
    let Some(crop) = crop else {
        return (source, [0.0, 0.0]);
    };
    let left = (crop.left.clamp(0.0, 0.99) * source[0] as f32).round();
    let top = (crop.top.clamp(0.0, 0.99) * source[1] as f32).round();
    let right = (crop.right.clamp(0.0, 0.99) * source[0] as f32).round();
    let bottom = (crop.bottom.clamp(0.0, 0.99) * source[1] as f32).round();
    let width = (source[0] as f32 - left - right).max(1.0) as u32;
    let height = (source[1] as f32 - top - bottom).max(1.0) as u32;
    ([width, height], [left, top])
}

/// Compute where a source of `source` pixels lands inside a `frame`-sized output.
///
/// `scale` is the already-keyframe-evaluated transform scale, so animation is resolved
/// before geometry — the compositor owns time, this function does not know about it.
pub fn place(
    source: [u32; 2],
    frame: [u32; 2],
    fit: Fit,
    transform: &Transform,
    scale: [f32; 2],
) -> Placement {
    let (sw, sh) = (source[0].max(1) as f32, source[1].max(1) as f32);
    let (fw, fh) = (frame[0].max(1) as f32, frame[1].max(1) as f32);
    let (fit_x, fit_y) = match fit {
        Fit::Contain => {
            let factor = (fw / sw).min(fh / sh);
            (factor, factor)
        }
        Fit::Cover => {
            let factor = (fw / sw).max(fh / sh);
            (factor, factor)
        }
        Fit::Stretch => (fw / sw, fh / sh),
        Fit::None => (1.0, 1.0),
    };
    let w = sw * fit_x * scale[0];
    let h = sh * fit_y * scale[1];
    // `pos` is an offset from frame center, in output pixels, and `anchor` says which point
    // of the clip sits there. Anchoring at a corner is what makes "pin the logo to the
    // bottom right" expressible without knowing the clip's size.
    let anchor_x = transform.anchor[0];
    let anchor_y = transform.anchor[1];
    let target_x = fw / 2.0 + transform.pos[0];
    let target_y = fh / 2.0 + transform.pos[1];
    Placement {
        dest: Rect {
            x: target_x - w * anchor_x,
            y: target_y - h * anchor_y,
            w,
            h,
        },
        rotation: transform.rotation,
    }
}

/// Composite `src` into `dst` at `placement`, scaling and rotating as needed.
///
/// `crop_offset` and `crop_size` describe the region of `src` that is actually used, so a
/// cropped clip does not need a second buffer.
pub fn blit(
    dst: &mut Frame,
    src: &Frame,
    placement: &Placement,
    crop_offset: [f32; 2],
    crop_size: [u32; 2],
    alpha: f32,
    blend: Blend,
) {
    if alpha <= 0.0 || placement.dest.w <= 0.0 || placement.dest.h <= 0.0 {
        return;
    }
    let size = dst.size();
    let uses_full_source = crop_offset == [0.0, 0.0] && crop_size == src.size();
    if uses_full_source && placement.is_pixel_aligned(src.size()) {
        blit_aligned(dst, src, placement, alpha, blend);
        return;
    }

    // Inverse map: output pixel → source pixel. Built once per blit rather than per pixel.
    let center = placement.dest.center();
    let radians = -placement.rotation.to_radians();
    let (sin, cos) = radians.sin_cos();
    let sx = crop_size[0] as f32 / placement.dest.w;
    let sy = crop_size[1] as f32 / placement.dest.h;

    // A rotated rect's axis-aligned bounds, so rotation does not clip its own corners.
    let half_w = placement.dest.w / 2.0;
    let half_h = placement.dest.h / 2.0;
    let (rot_sin, rot_cos) = placement.rotation.to_radians().sin_cos();
    let extent_x = (half_w * rot_cos).abs() + (half_h * rot_sin).abs();
    let extent_y = (half_w * rot_sin).abs() + (half_h * rot_cos).abs();
    let bounds = Rect {
        x: center[0] - extent_x,
        y: center[1] - extent_y,
        w: extent_x * 2.0,
        h: extent_y * 2.0,
    };
    let (x0, y0, x1, y1) = bounds.bounds_in(size);

    for y in y0..y1 {
        for x in x0..x1 {
            // Sample at pixel centers; sampling at corners shifts the image half a pixel,
            // which shows up as a soft edge on an otherwise pixel-exact placement.
            let ox = x as f32 + 0.5 - center[0];
            let oy = y as f32 + 0.5 - center[1];
            let rx = ox * cos - oy * sin;
            let ry = ox * sin + oy * cos;
            let local_x = rx + half_w;
            let local_y = ry + half_h;
            if local_x < 0.0 || local_y < 0.0 || local_x >= placement.dest.w || local_y >= placement.dest.h
            {
                continue;
            }
            let src_x = crop_offset[0] + local_x * sx - 0.5;
            let src_y = crop_offset[1] + local_y * sy - 0.5;
            let sample = src.sample_bilinear(src_x, src_y);
            if sample[3] <= 0.0 {
                continue;
            }
            let existing = dst.pixel(x, y);
            dst.set_pixel(x, y, blend_pixel(existing, sample, blend, alpha));
        }
    }
}

fn blit_aligned(dst: &mut Frame, src: &Frame, placement: &Placement, alpha: f32, blend: Blend) {
    let offset_x = placement.dest.x.round() as i64;
    let offset_y = placement.dest.y.round() as i64;
    let size = dst.size();
    for sy in 0..src.height() {
        let dy = offset_y + sy as i64;
        if dy < 0 || dy >= size[1] as i64 {
            continue;
        }
        for sx in 0..src.width() {
            let dx = offset_x + sx as i64;
            if dx < 0 || dx >= size[0] as i64 {
                continue;
            }
            let sample = src.pixel(sx, sy);
            if sample[3] <= 0.0 {
                continue;
            }
            let existing = dst.pixel(dx as u32, dy as u32);
            dst.set_pixel(
                dx as u32,
                dy as u32,
                blend_pixel(existing, sample, blend, alpha),
            );
        }
    }
}

/// Resample a frame to a new size with a box-average downscale / bilinear upscale. Used for
/// stills and nested sequences, where the source is already in memory and ffmpeg is not in
/// the loop.
pub fn resample(src: &Frame, size: [u32; 2]) -> Frame {
    if src.size() == size {
        return src.clone();
    }
    let (tw, th) = (size[0].max(1), size[1].max(1));
    let mut out = Frame::transparent(tw, th);
    let sx = src.width() as f32 / tw as f32;
    let sy = src.height() as f32 / th as f32;
    let downscale = sx > 1.0 || sy > 1.0;
    for y in 0..th {
        for x in 0..tw {
            let value = if downscale {
                // Averaging the covered source region is what keeps a 4K→540p proxy from
                // aliasing into noise; bilinear alone samples 1 of every 4 pixels.
                let x0 = (x as f32 * sx).floor() as u32;
                let y0 = (y as f32 * sy).floor() as u32;
                let x1 = ((x + 1) as f32 * sx).ceil().min(src.width() as f32) as u32;
                let y1 = ((y + 1) as f32 * sy).ceil().min(src.height() as f32) as u32;
                let mut sum = [0.0f32; 4];
                let mut count = 0.0f32;
                for yy in y0..y1.max(y0 + 1) {
                    for xx in x0..x1.max(x0 + 1) {
                        let sample = src.pixel(xx, yy);
                        for channel in 0..4 {
                            sum[channel] += sample[channel];
                        }
                        count += 1.0;
                    }
                }
                [
                    sum[0] / count,
                    sum[1] / count,
                    sum[2] / count,
                    sum[3] / count,
                ]
            } else {
                src.sample_bilinear((x as f32 + 0.5) * sx - 0.5, (y as f32 + 0.5) * sy - 0.5)
            };
            out.set_pixel(x, y, value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::color::Rgba;

    fn transform() -> Transform {
        Transform::default()
    }

    #[test]
    fn contain_letterboxes_and_cover_fills() {
        let source = [1920, 1080];
        let frame = [1080, 1920];
        let contain = place(source, frame, Fit::Contain, &transform(), [1.0, 1.0]);
        assert!((contain.dest.w - 1080.0).abs() < 0.01, "{:?}", contain.dest);
        assert!((contain.dest.h - 607.5).abs() < 0.1, "{:?}", contain.dest);
        assert!(contain.dest.y > 0.0, "contain should letterbox vertically");

        let cover = place(source, frame, Fit::Cover, &transform(), [1.0, 1.0]);
        assert!(cover.dest.h >= 1920.0, "{:?}", cover.dest);
        assert!(cover.dest.x < 0.0, "cover should overflow horizontally");
    }

    #[test]
    fn stretch_ignores_aspect_and_none_keeps_pixels() {
        let stretch = place([640, 480], [1920, 1080], Fit::Stretch, &transform(), [1.0, 1.0]);
        assert_eq!((stretch.dest.w, stretch.dest.h), (1920.0, 1080.0));
        let none = place([640, 480], [1920, 1080], Fit::None, &transform(), [1.0, 1.0]);
        assert_eq!((none.dest.w, none.dest.h), (640.0, 480.0));
        assert_eq!(none.dest.center(), [960.0, 540.0]);
    }

    #[test]
    fn anchor_moves_the_reference_point_not_the_size() {
        let mut t = transform();
        t.anchor = [1.0, 1.0];
        t.pos = [0.0, 0.0];
        let corner = place([100, 100], [200, 200], Fit::None, &t, [1.0, 1.0]);
        // Bottom-right anchor at frame center puts the clip's bottom-right at the center.
        assert_eq!(corner.dest.x, 0.0);
        assert_eq!(corner.dest.y, 0.0);
        assert_eq!((corner.dest.w, corner.dest.h), (100.0, 100.0));
    }

    #[test]
    fn scale_multiplies_the_fitted_size() {
        let doubled = place([100, 100], [400, 400], Fit::None, &transform(), [2.0, 0.5]);
        assert_eq!((doubled.dest.w, doubled.dest.h), (200.0, 50.0));
        assert_eq!(doubled.dest.center(), [200.0, 200.0]);
    }

    #[test]
    fn crop_reduces_the_source_and_reports_the_offset() {
        let crop = Crop {
            left: 0.25,
            top: 0.0,
            right: 0.25,
            bottom: 0.5,
        };
        let (size, offset) = cropped_size([100, 100], Some(&crop));
        assert_eq!(size, [50, 50]);
        assert_eq!(offset, [25.0, 0.0]);
        assert_eq!(cropped_size([100, 100], None), ([100, 100], [0.0, 0.0]));
    }

    #[test]
    fn an_aligned_blit_copies_pixels_exactly() {
        let mut dst = Frame::transparent(8, 8);
        let src = Frame::filled(4, 4, Rgba::opaque(255, 0, 0));
        let placement = Placement {
            dest: Rect { x: 2.0, y: 2.0, w: 4.0, h: 4.0 },
            rotation: 0.0,
        };
        assert!(placement.is_pixel_aligned(src.size()));
        blit(&mut dst, &src, &placement, [0.0, 0.0], [4, 4], 1.0, Blend::Normal);
        assert_eq!(dst.pixel(2, 2), src.pixel(0, 0));
        assert_eq!(dst.pixel(5, 5), src.pixel(0, 0));
        assert_eq!(dst.pixel(1, 1), [0.0; 4], "nothing outside the rect is touched");
        assert_eq!(dst.pixel(6, 6), [0.0; 4]);
    }

    #[test]
    fn a_rotated_blit_keeps_its_corners_inside_the_frame() {
        let mut dst = Frame::transparent(64, 64);
        let src = Frame::filled(20, 20, Rgba::WHITE);
        let placement = Placement {
            dest: Rect { x: 22.0, y: 22.0, w: 20.0, h: 20.0 },
            rotation: 45.0,
        };
        blit(&mut dst, &src, &placement, [0.0, 0.0], [20, 20], 1.0, Blend::Normal);
        // Rotating a square by 45° makes it a diamond: the center is covered, the
        // unrotated corners are not, and the diamond's tips extend past the original rect.
        assert!(dst.pixel(32, 32)[3] > 0.9, "center should be covered");
        assert!(dst.pixel(23, 23)[3] < 0.5, "the old corner should now be empty");
        assert!(dst.pixel(32, 19)[3] > 0.5, "the diamond tip should extend upward");
    }

    #[test]
    fn a_half_scale_blit_downsamples_rather_than_dropping_the_clip() {
        let mut dst = Frame::transparent(10, 10);
        let src = Frame::filled(8, 8, Rgba::WHITE);
        let placement = Placement {
            dest: Rect { x: 1.0, y: 1.0, w: 4.0, h: 4.0 },
            rotation: 0.0,
        };
        blit(&mut dst, &src, &placement, [0.0, 0.0], [8, 8], 1.0, Blend::Normal);
        assert!(dst.pixel(2, 2)[3] > 0.9);
        assert!(dst.pixel(6, 6)[3] < 0.1, "outside the scaled rect stays empty");
    }

    #[test]
    fn a_blit_outside_the_frame_is_a_no_op_rather_than_a_panic() {
        let mut dst = Frame::transparent(8, 8);
        let src = Frame::filled(4, 4, Rgba::WHITE);
        let placement = Placement {
            dest: Rect { x: -50.0, y: -50.0, w: 4.0, h: 4.0 },
            rotation: 0.0,
        };
        blit(&mut dst, &src, &placement, [0.0, 0.0], [4, 4], 1.0, Blend::Normal);
        assert_eq!(dst.alpha_coverage(), 0.0);
    }

    #[test]
    fn downscaling_averages_instead_of_point_sampling() {
        // A checkerboard downscaled by 2 must go gray. Point sampling would keep it a
        // checkerboard or turn it uniformly one color, both of which alias.
        let mut src = Frame::transparent(4, 4);
        for y in 0..4u32 {
            for x in 0..4u32 {
                let value = if (x + y) % 2 == 0 {
                    Rgba::WHITE
                } else {
                    Rgba::BLACK
                };
                src.set_pixel(x, y, value.to_linear_premul());
            }
        }
        let small = resample(&src, [2, 2]);
        for y in 0..2 {
            for x in 0..2 {
                let pixel = small.pixel(x, y);
                assert!(
                    (pixel[0] - 0.5).abs() < 0.1,
                    "checkerboard should average to mid gray, got {pixel:?}"
                );
            }
        }
    }

    #[test]
    fn resampling_to_the_same_size_is_identity() {
        let src = Frame::filled(6, 6, Rgba::opaque(3, 200, 100));
        assert_eq!(resample(&src, [6, 6]).pixels(), src.pixels());
    }
}
