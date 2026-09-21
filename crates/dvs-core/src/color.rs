//! One color type, hex in the document, linear f32 in the compositor.
//!
//! Compositing in sRGB-encoded bytes is the classic wrong-dissolve bug: a 50% mix of black
//! and white comes out at 0.5 encoded (≈ 21% light) instead of 0.5 light. Everything in the
//! render path works in linear light; this type is the boundary.

use crate::error::{Error, Result};
use serde::de::{self, Deserialize, Deserializer};
use serde::{Serialize, Serializer};
use std::borrow::Cow;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const BLACK: Rgba = Rgba::opaque(0, 0, 0);
    pub const WHITE: Rgba = Rgba::opaque(255, 255, 255);
    pub const TRANSPARENT: Rgba = Rgba {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    pub const fn opaque(r: u8, g: u8, b: u8) -> Self {
        Rgba { r, g, b, a: 255 }
    }

    pub fn parse(text: &str) -> Result<Self> {
        let hex = text.trim().trim_start_matches('#');
        let bad = || Error::bad_args(format!("cannot parse color '{text}'"));
        let nibble = |c: u8| -> Result<u8> {
            (c as char).to_digit(16).map(|d| d as u8).ok_or_else(bad)
        };
        let bytes = hex.as_bytes();
        match bytes.len() {
            3 | 4 => {
                let mut out = [255u8; 4];
                for (i, b) in bytes.iter().enumerate() {
                    let v = nibble(*b)?;
                    out[i] = v * 17;
                }
                Ok(Rgba {
                    r: out[0],
                    g: out[1],
                    b: out[2],
                    a: out[3],
                })
            }
            6 | 8 => {
                let mut out = [255u8; 4];
                for (i, chunk) in bytes.chunks(2).enumerate() {
                    out[i] = nibble(chunk[0])? * 16 + nibble(chunk[1])?;
                }
                Ok(Rgba {
                    r: out[0],
                    g: out[1],
                    b: out[2],
                    a: out[3],
                })
            }
            _ => Err(bad()),
        }
    }

    /// Straight (non-premultiplied) sRGB-encoded floats, as `resvg` and `tiny-skia` want.
    pub fn to_srgb_f32(self) -> [f32; 4] {
        [
            self.r as f32 / 255.0,
            self.g as f32 / 255.0,
            self.b as f32 / 255.0,
            self.a as f32 / 255.0,
        ]
    }

    /// Premultiplied linear-light RGBA, which is what the compositor blends.
    pub fn to_linear_premul(self) -> [f32; 4] {
        let a = self.a as f32 / 255.0;
        [
            srgb_to_linear(self.r) * a,
            srgb_to_linear(self.g) * a,
            srgb_to_linear(self.b) * a,
            a,
        ]
    }

    /// Relative luminance (WCAG), used by the contrast lint.
    pub fn luminance(self) -> f32 {
        0.2126 * srgb_to_linear(self.r)
            + 0.7152 * srgb_to_linear(self.g)
            + 0.0722 * srgb_to_linear(self.b)
    }

    /// WCAG contrast ratio, 1.0 to 21.0.
    pub fn contrast(self, other: Rgba) -> f32 {
        let (a, b) = (self.luminance(), other.luminance());
        let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }
}

pub fn srgb_to_linear(value: u8) -> f32 {
    let v = value as f32 / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

pub fn linear_to_srgb(value: f32) -> f32 {
    let v = value.clamp(0.0, 1.0);
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

impl fmt::Display for Rgba {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.a == 255 {
            write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            write!(
                f,
                "#{:02x}{:02x}{:02x}{:02x}",
                self.r, self.g, self.b, self.a
            )
        }
    }
}

impl Default for Rgba {
    fn default() -> Self {
        Rgba::BLACK
    }
}

impl Serialize for Rgba {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Rgba {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        Rgba::parse(&text).map_err(de::Error::custom)
    }
}

impl schemars::JsonSchema for Rgba {
    fn schema_name() -> Cow<'static, str> {
        "Color".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "hex color: #rgb, #rgba, #rrggbb or #rrggbbaa",
            "pattern": r"^#?([0-9a-fA-F]{3,4}|[0-9a-fA-F]{6}|[0-9a-fA-F]{8})$"
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_four_hex_lengths() {
        assert_eq!(Rgba::parse("#fff").unwrap(), Rgba::WHITE);
        assert_eq!(Rgba::parse("ffffff").unwrap(), Rgba::WHITE);
        assert_eq!(
            Rgba::parse("#00000080").unwrap(),
            Rgba { r: 0, g: 0, b: 0, a: 128 }
        );
        assert_eq!(Rgba::parse("#f00f").unwrap(), Rgba::opaque(255, 0, 0));
        assert!(Rgba::parse("#ff").is_err());
    }

    #[test]
    fn linear_conversion_is_not_the_encoded_value() {
        // The whole reason this type exists: mid-gray is 21.6% light, not 50%.
        let mid = srgb_to_linear(128);
        assert!((mid - 0.2159).abs() < 0.001, "got {mid}");
        assert!((linear_to_srgb(mid) - 128.0 / 255.0).abs() < 0.001);
    }

    #[test]
    fn contrast_matches_wcag_reference_values() {
        assert!((Rgba::BLACK.contrast(Rgba::WHITE) - 21.0).abs() < 0.01);
        assert!((Rgba::WHITE.contrast(Rgba::WHITE) - 1.0).abs() < 0.001);
        // #767676 on white is the canonical 4.54:1 AA boundary case.
        let boundary = Rgba::parse("#767676").unwrap().contrast(Rgba::WHITE);
        assert!((boundary - 4.54).abs() < 0.05, "got {boundary}");
    }
}
