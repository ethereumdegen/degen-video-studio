//! Blend modes over premultiplied linear pixels.
//!
//! The separable modes are defined in the Porter-Duff / PDF sense on *unpremultiplied*
//! colors, so each one unpremultiplies, blends, and recombines in a single expression:
//!
//! ```text
//! co = (1 - ab)·cs·as + (1 - as)·cb·ab + as·ab·B(cs, cb)
//! ao = as + ab·(1 - as)
//! ```
//!
//! Writing it out this way matters because the shortcut people reach for — blending the
//! premultiplied values directly — is only correct for `Normal`, and silently produces
//! dark halos for `Multiply` and washed-out edges for `Screen` wherever alpha is partial.

use dvs_core::project::Blend;

/// Blend `src` over `dst`, both premultiplied linear RGBA, with `alpha` scaling the source.
#[inline]
pub fn blend_pixel(dst: [f32; 4], src: [f32; 4], mode: Blend, alpha: f32) -> [f32; 4] {
    let src = [
        src[0] * alpha,
        src[1] * alpha,
        src[2] * alpha,
        src[3] * alpha,
    ];
    if src[3] <= 0.0 {
        return dst;
    }
    if matches!(mode, Blend::Normal) {
        // The common path: straight source-over, no unpremultiply needed.
        let inv = 1.0 - src[3];
        return [
            src[0] + dst[0] * inv,
            src[1] + dst[1] * inv,
            src[2] + dst[2] * inv,
            src[3] + dst[3] * inv,
        ];
    }
    let (sa, ba) = (src[3], dst[3]);
    let unpremul = |value: f32, alpha: f32| if alpha > 0.0 { value / alpha } else { 0.0 };
    let mut out = [0.0f32; 4];
    out[3] = sa + ba * (1.0 - sa);
    for channel in 0..3 {
        let cs = unpremul(src[channel], sa);
        let cb = unpremul(dst[channel], ba);
        let blended = separable(mode, cs, cb);
        out[channel] = (1.0 - ba) * cs * sa + (1.0 - sa) * cb * ba + sa * ba * blended;
    }
    out
}

#[inline]
fn separable(mode: Blend, cs: f32, cb: f32) -> f32 {
    match mode {
        Blend::Normal => cs,
        Blend::Add => (cs + cb).min(1.0),
        Blend::Multiply => cs * cb,
        Blend::Screen => cs + cb - cs * cb,
        Blend::Overlay => hard_light(cb, cs),
        Blend::SoftLight => {
            // W3C compositing soft-light: the piecewise form, not the cheap approximation,
            // because the approximation has a visible kink at 0.5 that shows up as a band
            // in a gradient.
            if cs <= 0.5 {
                cb - (1.0 - 2.0 * cs) * cb * (1.0 - cb)
            } else {
                let d = if cb <= 0.25 {
                    ((16.0 * cb - 12.0) * cb + 4.0) * cb
                } else {
                    cb.sqrt()
                };
                cb + (2.0 * cs - 1.0) * (d - cb)
            }
        }
    }
}

#[inline]
fn hard_light(cs: f32, cb: f32) -> f32 {
    if cs <= 0.5 {
        2.0 * cs * cb
    } else {
        1.0 - 2.0 * (1.0 - cs) * (1.0 - cb)
    }
}

/// Linear interpolation between two premultiplied pixels. This is what a dissolve is, and
/// doing it on premultiplied values is correct without an alpha special case.
#[inline]
pub fn lerp_pixel(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPAQUE_WHITE: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
    const OPAQUE_BLACK: [f32; 4] = [0.0, 0.0, 0.0, 1.0];
    const CLEAR: [f32; 4] = [0.0, 0.0, 0.0, 0.0];

    #[test]
    fn normal_over_transparent_keeps_the_source() {
        let out = blend_pixel(CLEAR, OPAQUE_WHITE, Blend::Normal, 1.0);
        assert_eq!(out, OPAQUE_WHITE);
    }

    #[test]
    fn alpha_scaling_is_applied_before_compositing() {
        let out = blend_pixel(OPAQUE_BLACK, OPAQUE_WHITE, Blend::Normal, 0.5);
        assert!((out[0] - 0.5).abs() < 1e-6, "{out:?}");
        assert!((out[3] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn multiply_with_white_is_identity_and_with_black_is_black() {
        let gray = [0.4, 0.4, 0.4, 1.0];
        let over_white = blend_pixel(OPAQUE_WHITE, gray, Blend::Multiply, 1.0);
        assert!((over_white[0] - 0.4).abs() < 1e-5, "{over_white:?}");
        let over_black = blend_pixel(OPAQUE_BLACK, gray, Blend::Multiply, 1.0);
        assert!(over_black[0] < 1e-5, "{over_black:?}");
    }

    #[test]
    fn screen_with_black_is_identity() {
        let gray = [0.3, 0.3, 0.3, 1.0];
        let out = blend_pixel(OPAQUE_BLACK, gray, Blend::Screen, 1.0);
        assert!((out[0] - 0.3).abs() < 1e-5, "{out:?}");
    }

    #[test]
    fn a_half_transparent_multiply_does_not_darken_toward_black() {
        // The bug this guards: blending premultiplied values directly for Multiply makes a
        // 50%-alpha white source darken the backdrop instead of leaving it alone.
        let backdrop = [0.5, 0.5, 0.5, 1.0];
        let half_white = [1.0, 1.0, 1.0, 1.0];
        let out = blend_pixel(backdrop, half_white, Blend::Multiply, 0.5);
        assert!(
            (out[0] - 0.5).abs() < 1e-5,
            "multiplying by white should not change the backdrop, got {out:?}"
        );
    }

    #[test]
    fn soft_light_is_continuous_across_the_midpoint() {
        let backdrop = [0.4, 0.4, 0.4, 1.0];
        let below = blend_pixel(backdrop, [0.499, 0.499, 0.499, 1.0], Blend::SoftLight, 1.0);
        let above = blend_pixel(backdrop, [0.501, 0.501, 0.501, 1.0], Blend::SoftLight, 1.0);
        assert!(
            (below[0] - above[0]).abs() < 0.01,
            "soft-light jumped at 0.5: {} vs {}",
            below[0],
            above[0]
        );
    }

    #[test]
    fn add_saturates_rather_than_wrapping() {
        let out = blend_pixel(OPAQUE_WHITE, OPAQUE_WHITE, Blend::Add, 1.0);
        assert!(out[0] <= 1.0 + 1e-6, "{out:?}");
        assert!(out[0] > 0.99);
    }

    #[test]
    fn lerp_at_the_endpoints_returns_the_endpoints() {
        assert_eq!(lerp_pixel(OPAQUE_BLACK, OPAQUE_WHITE, 0.0), OPAQUE_BLACK);
        assert_eq!(lerp_pixel(OPAQUE_BLACK, OPAQUE_WHITE, 1.0), OPAQUE_WHITE);
        let mid = lerp_pixel(CLEAR, OPAQUE_WHITE, 0.5);
        assert_eq!(mid, [0.5, 0.5, 0.5, 0.5]);
    }
}
