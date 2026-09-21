//! The per-clip effect chain.
//!
//! A frame arrives from the decoder, the title rasteriser or a generator; every enabled
//! [`Effect`] on the clip runs over it in document order; the compositor then places the
//! result. Effects never resize a frame — placement, fit and the geometric crop belong to
//! `geometry.rs` — so the chain stays a pure pixel operation, which is what lets the render
//! cache key a segment on the document alone.
//!
//! Four failure modes this module exists to prevent:
//!
//! - **A grade that silently did nothing.** An unknown effect kind, and an unknown parameter
//!   inside a known one, are both errors that name the accepted set. For an operator who
//!   cannot see the picture, a `{"radiuss": 4}` that is ignored is worse than a failed call:
//!   the document then claims the shot is blurred and the pixels disagree.
//! - **Grading the wrong numbers.** [`Frame`] is premultiplied **linear light**. Exposure is
//!   therefore a multiply and contrast pivots on 0.18 middle grey rather than 0.5; every
//!   colour operation unpremultiplies first, because adding a lift to premultiplied values
//!   lifts the transparent pixels too and paints a halo around every soft edge.
//! - **"The LUT looks crushed."** A `.cube` is authored against *display-referred* values,
//!   so the lookup converts linear → sRGB before the table and back after. Skipping that
//!   conversion, or applying it twice, is exactly the crushed-shadows bug; [`Lut::sample`]
//!   therefore takes and returns display-referred triples and nothing else here does.
//! - **A stabilize that quietly passed the frame through.** Motion is measured once by
//!   ffmpeg's `vidstabdetect` into `cache/stabilize/<clipId>.trf` and read back per frame.
//!   With that file missing the effect fails and names `fx.analyze`, because an
//!   un-stabilized render is indistinguishable from a stabilized one to an agent reading
//!   the document.

use dvs_core::asset::AssetStore;
use dvs_core::color::{linear_to_srgb, Rgba};
use dvs_core::error::{Error, Result};
use dvs_core::ids::ClipId;
use dvs_core::paths::ProjectPaths;
use dvs_core::project::{Clip, Effect, Project, VideoStream};
use dvs_core::time::Time;
use dvs_media::frame::Frame;
use dvs_media::toolchain::Toolchain;
use rayon::prelude::*;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

/// The effect kinds the compositor implements. The list is closed and is quoted verbatim in
/// every rejection, so discovering the catalog costs one failed call rather than a render
/// that looks untouched.
pub const CATALOG: &[&str] = &[
    "color.lut",
    "color.grade",
    "blur",
    "sharpen",
    "crop",
    "mask.shape",
    "chroma-key",
    "stabilize",
];

/// Rec.709 luminance weights. Used for saturation, white balance and the chroma-key split,
/// applied to linear values — which is what makes "preserve luminance" mean preserve light.
const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

/// Scene-referred middle grey. In linear light an 18% grey card is 0.18, not 0.5, and a
/// contrast control that pivots on 0.5 darkens everything a colourist would call correct.
const MIDDLE_GRAY: f32 = 0.18;

/// Rec.709 chroma denominators, so `Cb`/`Cr` land in ±0.5 for in-gamut colour.
const CB_SCALE: f32 = 1.8556;
const CR_SCALE: f32 = 1.5748;

/// Everything an effect may read besides the pixels: the document (to resolve a LUT asset),
/// ffmpeg (for the stabilize analysis), the project directory, the asset store, the clip
/// that owns the chain, the instant being rendered (keyframes are evaluated at it) and the
/// sequence size.
///
/// `sequence_size` is not the frame size. The compositor decodes at whatever resolution the
/// placement needs, so a `radius` an agent expressed in sequence pixels has to be rescaled
/// into frame pixels or the same document would blur differently at preview and at
/// delivery.
pub struct EffectCx<'a> {
    pub project: &'a Project,
    pub tool: &'a Toolchain,
    pub paths: &'a ProjectPaths,
    pub assets: &'a AssetStore,
    pub clip: &'a Clip,
    pub at: Time,
    pub sequence_size: [u32; 2],
}

impl EffectCx<'_> {
    /// Sequence pixels → `frame` pixels, per axis.
    fn pixel_scale(&self, frame: &Frame) -> [f32; 2] {
        [
            ratio(frame.width(), self.sequence_size[0]),
            ratio(frame.height(), self.sequence_size[1]),
        ]
    }

    /// The clip's video stream: `stabilize` needs the frame rate its `.trf` is indexed by
    /// and the resolution its vectors were measured in.
    fn video_stream(&self) -> Result<&VideoStream> {
        let asset = self.clip.source.asset_id().ok_or_else(|| {
            Error::op(format!(
                "clip '{}' has a stabilize effect but its source is {}; motion analysis needs media",
                self.clip.label(),
                self.clip.source.describe()
            ))
        })?;
        let record = self.project.asset(asset)?;
        record.probe.video.as_ref().ok_or_else(|| {
            Error::op(format!(
                "clip '{}' uses '{}', which has no video stream to stabilize",
                self.clip.label(),
                record.name
            ))
        })
    }
}

/// `a / b` with a zero denominator treated as 1:1, so a degenerate size cannot turn a
/// radius into a NaN that silently blanks the frame.
fn ratio(a: u32, b: u32) -> f32 {
    if b == 0 {
        1.0
    } else {
        a as f32 / b as f32
    }
}

/// Run one effect over a frame. `Effect::enabled` is *not* consulted here — [`apply_all`]
/// is where the chain honours it — so a caller that wants to see what a disabled effect
/// would do can still reach it.
pub fn apply(frame: &mut Frame, effect: &Effect, cx: &EffectCx) -> Result<()> {
    match effect.kind.as_str() {
        "color.grade" => grade(frame, effect, cx),
        "color.lut" => color_lut(frame, effect, cx),
        "blur" => blur(frame, effect, cx),
        "sharpen" => sharpen(frame, effect, cx),
        "crop" => crop(frame, effect, cx),
        "mask.shape" => mask_shape(frame, effect, cx),
        "chroma-key" => chroma_key(frame, effect, cx),
        "stabilize" => stabilize(frame, effect, cx),
        other => Err(Error::op(format!(
            "clip '{}' has effect '{}' of unknown kind '{other}'; the catalog is: {}",
            cx.clip.label(),
            effect.id,
            CATALOG.join(", ")
        ))),
    }
}

/// Every enabled effect on `cx.clip`, in document order. Order is the document's, because
/// a grade before a LUT and a grade after one are different pictures.
pub fn apply_all(frame: &mut Frame, cx: &EffectCx) -> Result<()> {
    for effect in cx.clip.effects.iter().filter(|effect| effect.enabled) {
        apply(frame, effect, cx)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------- parameters

/// Reject a parameter this effect does not implement, naming the accepted set.
fn accept(effect: &Effect, accepted: &[&str]) -> Result<()> {
    let unknown: Vec<&str> = effect
        .params
        .keys()
        .map(String::as_str)
        .filter(|key| !accepted.contains(key))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(Error::bad_args(format!(
        "effect '{}' of kind '{}' has unknown parameter(s): {}; accepted: {}",
        effect.id,
        effect.kind,
        unknown.join(", "),
        accepted.join(", ")
    )))
}

/// A numeric parameter. A JSON string is accepted because a shell-driven agent produces
/// `"4"` where MCP produces `4`, but anything that is not a number fails rather than
/// falling back to the default: a `"radius": "four"` that blurs by 0 is the same invisible
/// wrongness as a typo'd key.
fn number(effect: &Effect, key: &str, default: f64) -> Result<f64> {
    match effect.params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(value)) => value.as_f64().ok_or_else(|| {
            Error::bad_args(format!(
                "effect '{}' parameter '{key}' is not a finite number",
                effect.kind
            ))
        }),
        Some(Value::String(text)) => text.trim().parse::<f64>().map_err(|_| {
            Error::bad_args(format!(
                "effect '{}' parameter '{key}' must be a number, got '{text}'",
                effect.kind
            ))
        }),
        Some(other) => Err(Error::bad_args(format!(
            "effect '{}' parameter '{key}' must be a number, got {other}",
            effect.kind
        ))),
    }
}

/// The keyframed value of `fx.<effectId>.<key>` at `cx.at`, or `None` when the clip does
/// not animate it. The dotted path is only built when the clip has keyframes at all,
/// because this runs once per parameter per effect per frame.
fn keyframed(cx: &EffectCx, effect: &Effect, key: &str) -> Option<f64> {
    if cx.clip.keyframes.is_empty() {
        return None;
    }
    let path = format!("fx.{}.{key}", effect.id);
    if !cx.clip.keyframes.contains_key(&path) {
        return None;
    }
    Some(cx.clip.param_at(&path, 0.0, cx.at))
}

/// A parameter that may be animated: the keyframe list wins, the static value is the
/// fallback.
fn scalar(cx: &EffectCx, effect: &Effect, key: &str, default: f64) -> Result<f64> {
    match keyframed(cx, effect, key) {
        Some(value) => Ok(value),
        None => number(effect, key, default),
    }
}

/// A fraction of the frame, rejected outside `0..=1` — a `crop.left` of 1.5 is a typo, and
/// silently clamping it hides the typo behind a black frame.
fn fraction(cx: &EffectCx, effect: &Effect, key: &str) -> Result<f32> {
    let value = scalar(cx, effect, key, 0.0)?;
    if !(0.0..=1.0).contains(&value) {
        return Err(Error::bad_args(format!(
            "effect '{}' parameter '{key}' is a fraction of the frame and must be 0..=1, got {value}",
            effect.kind
        )));
    }
    Ok(value as f32)
}

/// `lift`/`gamma`/`gain` accept one number or three per-channel numbers. A keyframe on the
/// parameter drives all three channels together, because the document defines one path per
/// parameter rather than one per channel.
fn triple(cx: &EffectCx, effect: &Effect, key: &str, default: f32) -> Result<[f32; 3]> {
    if let Some(animated) = keyframed(cx, effect, key) {
        return Ok([animated as f32; 3]);
    }
    match effect.params.get(key) {
        None | Some(Value::Null) => Ok([default; 3]),
        Some(Value::Array(items)) => {
            if items.len() != 3 {
                return Err(Error::bad_args(format!(
                    "effect '{}' parameter '{key}' takes a number or three per-channel numbers, got {} entries",
                    effect.kind,
                    items.len()
                )));
            }
            let mut out = [default; 3];
            for (slot, item) in out.iter_mut().zip(items) {
                *slot = item.as_f64().ok_or_else(|| {
                    Error::bad_args(format!(
                        "effect '{}' parameter '{key}' must be numbers, got {item}",
                        effect.kind
                    ))
                })? as f32;
            }
            Ok(out)
        }
        Some(_) => Ok([number(effect, key, default as f64)? as f32; 3]),
    }
}

/// A boolean parameter, accepting the string spellings a CLI produces.
fn flag(effect: &Effect, key: &str) -> Result<bool> {
    match effect.params.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(Value::String(text)) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Ok(true),
            "false" | "no" | "0" => Ok(false),
            other => Err(Error::bad_args(format!(
                "effect '{}' parameter '{key}' must be a boolean, got '{other}'",
                effect.kind
            ))),
        },
        Some(other) => Err(Error::bad_args(format!(
            "effect '{}' parameter '{key}' must be a boolean, got {other}",
            effect.kind
        ))),
    }
}

// -------------------------------------------------------------------------- pixel passes

/// Rec.709 relative luminance of a linear triple.
fn luma(rgb: [f32; 3]) -> f32 {
    LUMA[0] * rgb[0] + LUMA[1] * rgb[1] + LUMA[2] * rgb[2]
}

/// Run `map` over every pixel's *straight* linear RGB, rows in parallel.
///
/// The unpremultiply/repremultiply round trip is the whole point: premultiplied values are
/// colour × coverage, and a lift, a gamma or a LUT applied to that product is a function of
/// the coverage as well as the colour. Fully transparent pixels carry no colour and are
/// skipped, which also keeps them exactly zero.
fn map_colors(frame: &mut Frame, map: impl Fn([f32; 3]) -> [f32; 3] + Send + Sync) {
    let stride = frame.width() as usize * 4;
    if stride == 0 {
        return;
    }
    frame.pixels_mut().par_chunks_mut(stride).for_each(|row| {
        for pixel in row.chunks_exact_mut(4) {
            let alpha = pixel[3];
            if alpha <= f32::EPSILON {
                continue;
            }
            let inverse = 1.0 / alpha;
            let out = map([
                pixel[0] * inverse,
                pixel[1] * inverse,
                pixel[2] * inverse,
            ]);
            pixel[0] = out[0] * alpha;
            pixel[1] = out[1] * alpha;
            pixel[2] = out[2] * alpha;
        }
    });
}

/// Multiply a per-pixel coverage into the frame. `coverage` is called with the pixel
/// *centre* in frame pixels, which is what makes a mask edge land exactly on the requested
/// fraction instead of half a pixel off it. All four channels scale together because the
/// buffer is premultiplied.
fn map_coverage(frame: &mut Frame, coverage: impl Fn(f32, f32) -> f32 + Send + Sync) {
    let stride = frame.width() as usize * 4;
    if stride == 0 {
        return;
    }
    frame
        .pixels_mut()
        .par_chunks_mut(stride)
        .enumerate()
        .for_each(|(y, row)| {
            let center_y = y as f32 + 0.5;
            for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
                let factor = coverage(x as f32 + 0.5, center_y).clamp(0.0, 1.0);
                if factor >= 1.0 {
                    continue;
                }
                for channel in pixel.iter_mut() {
                    *channel *= factor;
                }
            }
        });
}

// -------------------------------------------------------------------------- color.grade

const GRADE_PARAMS: &[&str] = &[
    "lift",
    "gamma",
    "gain",
    "saturation",
    "temperature",
    "contrast",
    "exposure",
];

/// A resolved grade. Exposure is folded into `gain` because both are multiplies that apply
/// before the lift, which keeps `exposure: 1` an exact doubling rather than two roundings.
struct Grade {
    gain: [f32; 3],
    lift: [f32; 3],
    gamma: [f32; 3],
    temperature: f32,
    saturation: f32,
    contrast: f32,
}

impl Grade {
    fn read(cx: &EffectCx, effect: &Effect) -> Result<Grade> {
        let exposure = scalar(cx, effect, "exposure", 0.0)? as f32;
        let mut gain = triple(cx, effect, "gain", 1.0)?;
        let stops = exposure.exp2();
        for channel in gain.iter_mut() {
            *channel *= stops;
        }
        let gamma = triple(cx, effect, "gamma", 1.0)?;
        if gamma.iter().any(|value| *value <= 0.0) {
            return Err(Error::bad_args(format!(
                "effect '{}' parameter 'gamma' must be positive, got {gamma:?}",
                effect.kind
            )));
        }
        let saturation = scalar(cx, effect, "saturation", 1.0)? as f32;
        if saturation < 0.0 {
            return Err(Error::bad_args(format!(
                "effect '{}' parameter 'saturation' must not be negative, got {saturation}",
                effect.kind
            )));
        }
        let contrast = scalar(cx, effect, "contrast", 1.0)? as f32;
        if contrast <= 0.0 {
            return Err(Error::bad_args(format!(
                "effect '{}' parameter 'contrast' is a slope about middle grey and must be positive, got {contrast}",
                effect.kind
            )));
        }
        Ok(Grade {
            gain,
            lift: triple(cx, effect, "lift", 0.0)?,
            gamma,
            temperature: scalar(cx, effect, "temperature", 0.0)? as f32,
            saturation,
            contrast,
        })
    }

    /// Nothing to do: worth checking once per frame rather than paying a full pass to
    /// multiply by one.
    fn is_identity(&self) -> bool {
        self.gain == [1.0; 3]
            && self.lift == [0.0; 3]
            && self.gamma == [1.0; 3]
            && self.temperature == 0.0
            && self.saturation == 1.0
            && self.contrast == 1.0
    }

    /// Straight linear RGB in, straight linear RGB out.
    ///
    /// Order: the ASC-CDL slope/offset/power (with exposure inside the slope) first, since
    /// those are scene-referred corrections; then white balance, which is a property of the
    /// light rather than of the grade; then saturation; then contrast last, so the S-curve
    /// sees the tonality everything else produced.
    fn pixel(&self, rgb: [f32; 3]) -> [f32; 3] {
        let mut out = rgb;
        for channel in 0..3 {
            let mut value = out[channel] * self.gain[channel] + self.lift[channel];
            if self.gamma[channel] != 1.0 {
                value = value.max(0.0).powf(1.0 / self.gamma[channel]);
            }
            out[channel] = value;
        }
        if self.temperature != 0.0 {
            out = white_balance(out, self.temperature);
        }
        if self.saturation != 1.0 {
            let grey = luma(out);
            for channel in out.iter_mut() {
                *channel = grey + (*channel - grey) * self.saturation;
            }
        }
        if self.contrast != 1.0 {
            for channel in out.iter_mut() {
                *channel = if *channel > 0.0 {
                    MIDDLE_GRAY * (*channel / MIDDLE_GRAY).powf(self.contrast)
                } else {
                    0.0
                };
            }
        }
        out
    }
}

/// A warm/cool shift measured in hundreds of mireds: `+1.0` moves a 6500 K white point to
/// roughly 4600 K, `-1.0` the other way. Mireds rather than kelvin because a reciprocal
/// scale is perceptually even — 1000 K is a different amount of "warmer" at 3000 K than at
/// 8000 K.
///
/// The red/blue gains are renormalised to hold Rec.709 luma constant: a white balance
/// change must not read as an exposure change, or every temperature tweak turns into a
/// second exposure tweak to undo it.
fn white_balance(rgb: [f32; 3], temperature: f32) -> [f32; 3] {
    /// Gain per 100 mireds. Chosen so ±1.0 is a visible but recoverable shift.
    const STRENGTH: f32 = 0.2;
    let warm = (1.0 + STRENGTH * temperature).max(0.0);
    let cool = (1.0 - STRENGTH * temperature).max(0.0);
    let shifted = [rgb[0] * warm, rgb[1], rgb[2] * cool];
    let before = luma(rgb);
    let after = luma(shifted);
    if before <= 0.0 || after <= 0.0 {
        return shifted;
    }
    let normalize = before / after;
    [
        shifted[0] * normalize,
        shifted[1] * normalize,
        shifted[2] * normalize,
    ]
}

fn grade(frame: &mut Frame, effect: &Effect, cx: &EffectCx) -> Result<()> {
    accept(effect, GRADE_PARAMS)?;
    let grade = Grade::read(cx, effect)?;
    if grade.is_identity() {
        return Ok(());
    }
    map_colors(frame, |rgb| grade.pixel(rgb));
    Ok(())
}

// ---------------------------------------------------------------------------- color.lut

const LUT_PARAMS: &[&str] = &["asset", "path"];

/// The float counterpart of [`dvs_core::color::srgb_to_linear`], which takes a byte because
/// decoding has a 256-entry table. A LUT lookup needs the inverse at full precision.
fn srgb_to_linear(value: f32) -> f32 {
    let v = value.clamp(0.0, 1.0);
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// A parsed Adobe `.cube` lookup table.
///
/// Both shapes are supported. A 3D table is `size³` entries with red varying fastest, read
/// with trilinear interpolation. A 1D table is `size` entries acting as three independent
/// channel curves, which is what a pure contrast or log-to-display transfer LUT is.
///
/// Entries are display-referred — the domain a colourist authored against — so
/// [`Lut::sample`] takes and returns sRGB-encoded values, never the linear ones in a
/// [`Frame`].
pub struct Lut {
    size: usize,
    three_d: bool,
    domain_min: [f32; 3],
    domain_max: [f32; 3],
    table: Vec<[f32; 3]>,
}

/// Summarised rather than derived: a 33³ table is 36k floats, and a panic message or a
/// log line that dumps them is unreadable.
impl std::fmt::Debug for Lut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Lut({}D, size {}, domain {:?}..{:?}, {} entries)",
            if self.three_d { 3 } else { 1 },
            self.size,
            self.domain_min,
            self.domain_max,
            self.table.len()
        )
    }
}

fn cube_error(line: usize, message: impl AsRef<str>) -> Error {
    Error::bad_args(format!("line {line}: {}", message.as_ref()))
}

/// Three floats out of the rest of a header line.
fn three_floats<'a>(parts: impl Iterator<Item = &'a str>) -> Option<[f32; 3]> {
    let mut out = [0.0f32; 3];
    let mut count = 0;
    for (slot, text) in out.iter_mut().zip(parts) {
        *slot = text.parse().ok()?;
        count += 1;
    }
    (count == 3).then_some(out)
}

impl Lut {
    /// Parse the Adobe Cube LUT format: `TITLE`, `LUT_3D_SIZE`/`LUT_1D_SIZE`,
    /// `DOMAIN_MIN`/`DOMAIN_MAX`, the Resolve `*_INPUT_RANGE` spelling of the same thing,
    /// `#` comments, blank lines, then the table rows.
    ///
    /// An unrecognised keyword is an error rather than a skipped line: a `.cube` variant we
    /// do not understand would otherwise be graded with a partially-read table.
    pub fn parse_cube(text: &str) -> Result<Lut> {
        let mut size: Option<usize> = None;
        let mut three_d = true;
        let mut domain_min = [0.0f32; 3];
        let mut domain_max = [1.0f32; 3];
        let mut table: Vec<[f32; 3]> = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or_default().trim();
            if line.is_empty() {
                continue;
            }
            let number = index + 1;
            let mut parts = line.split_whitespace();
            let head = parts.next().unwrap_or_default();
            match head.to_ascii_uppercase().as_str() {
                "TITLE" => continue,
                keyword @ ("LUT_3D_SIZE" | "LUT_1D_SIZE") => {
                    if size.is_some() {
                        return Err(cube_error(number, "a .cube declares exactly one LUT size"));
                    }
                    three_d = keyword == "LUT_3D_SIZE";
                    let declared: usize = parts
                        .next()
                        .and_then(|value| value.parse().ok())
                        .ok_or_else(|| cube_error(number, format!("expected '{keyword} <n>'")))?;
                    if !(2..=256).contains(&declared) {
                        return Err(cube_error(
                            number,
                            format!("LUT size {declared} is outside the supported 2..=256"),
                        ));
                    }
                    size = Some(declared);
                }
                "DOMAIN_MIN" => {
                    domain_min = three_floats(parts)
                        .ok_or_else(|| cube_error(number, "expected 'DOMAIN_MIN <r> <g> <b>'"))?;
                }
                "DOMAIN_MAX" => {
                    domain_max = three_floats(parts)
                        .ok_or_else(|| cube_error(number, "expected 'DOMAIN_MAX <r> <g> <b>'"))?;
                }
                "LUT_1D_INPUT_RANGE" | "LUT_3D_INPUT_RANGE" => {
                    let low: f32 = parts
                        .next()
                        .and_then(|value| value.parse().ok())
                        .ok_or_else(|| cube_error(number, "expected '<min> <max>'"))?;
                    let high: f32 = parts
                        .next()
                        .and_then(|value| value.parse().ok())
                        .ok_or_else(|| cube_error(number, "expected '<min> <max>'"))?;
                    domain_min = [low; 3];
                    domain_max = [high; 3];
                }
                _ => {
                    let row = three_floats(std::iter::once(head).chain(parts)).ok_or_else(|| {
                        cube_error(
                            number,
                            format!(
                                "'{head}' is neither three numbers nor a known keyword \
                                 (TITLE, LUT_3D_SIZE, LUT_1D_SIZE, DOMAIN_MIN, DOMAIN_MAX, \
                                 LUT_3D_INPUT_RANGE, LUT_1D_INPUT_RANGE)"
                            ),
                        )
                    })?;
                    table.push(row);
                }
            }
        }
        let size = size.ok_or_else(|| {
            Error::bad_args("no LUT_3D_SIZE or LUT_1D_SIZE; this is not an Adobe .cube file")
        })?;
        let wanted = if three_d { size * size * size } else { size };
        if table.len() != wanted {
            return Err(Error::bad_args(format!(
                "{} LUT of size {size} needs {wanted} table rows, found {}",
                if three_d { "3D" } else { "1D" },
                table.len()
            )));
        }
        if domain_min
            .iter()
            .zip(&domain_max)
            .any(|(low, high)| high <= low)
        {
            return Err(Error::bad_args(format!(
                "LUT domain is empty: DOMAIN_MIN {domain_min:?} is not below DOMAIN_MAX {domain_max:?}"
            )));
        }
        Ok(Lut {
            size,
            three_d,
            domain_min,
            domain_max,
            table,
        })
    }

    /// Edge length of the table.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Whether this is a 3D cube rather than three 1D curves.
    pub fn is_3d(&self) -> bool {
        self.three_d
    }

    /// Look up a **display-referred** triple and return the display-referred result.
    ///
    /// 3D tables interpolate trilinearly, which reproduces an identity LUT exactly and
    /// keeps a 33³ creative LUT smooth. Values outside the declared domain clamp to it,
    /// because extrapolating a creative LUT invents colour.
    pub fn sample(&self, rgb: [f32; 3]) -> [f32; 3] {
        let last = (self.size - 1) as f32;
        let mut position = [0.0f32; 3];
        for channel in 0..3 {
            let span = self.domain_max[channel] - self.domain_min[channel];
            let normalized = (rgb[channel] - self.domain_min[channel]) / span;
            position[channel] = normalized.clamp(0.0, 1.0) * last;
        }
        if !self.three_d {
            let mut out = [0.0f32; 3];
            for channel in 0..3 {
                let low = position[channel].floor();
                let index = low as usize;
                let next = (index + 1).min(self.size - 1);
                let blend = position[channel] - low;
                out[channel] = self.table[index][channel] * (1.0 - blend)
                    + self.table[next][channel] * blend;
            }
            return out;
        }
        let mut low = [0usize; 3];
        let mut high = [0usize; 3];
        let mut blend = [0.0f32; 3];
        for channel in 0..3 {
            let floor = position[channel].floor();
            low[channel] = floor as usize;
            high[channel] = (low[channel] + 1).min(self.size - 1);
            blend[channel] = position[channel] - floor;
        }
        let mut out = [0.0f32; 3];
        for corner in 0..8 {
            let mut weight = 1.0f32;
            let mut index = 0usize;
            let mut stride = 1usize;
            for channel in 0..3 {
                let upper = corner & (1 << channel) != 0;
                weight *= if upper {
                    blend[channel]
                } else {
                    1.0 - blend[channel]
                };
                index += stride * if upper { high[channel] } else { low[channel] };
                stride *= self.size;
            }
            if weight == 0.0 {
                continue;
            }
            let entry = self.table[index];
            for channel in 0..3 {
                out[channel] += entry[channel] * weight;
            }
        }
        out
    }
}

/// Size and modification time behind a path, so a cache entry cannot outlive the file it
/// was parsed from. `(0, 0)` when the metadata is unreadable, which simply misses the cache.
fn stamp(path: &Path) -> (u64, u128) {
    let Ok(meta) = std::fs::metadata(path) else {
        return (0, 0);
    };
    let modified = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_nanos())
        .unwrap_or_default();
    (meta.len(), modified)
}

/// Parsed tables, keyed by path plus the stamp behind it.
///
/// Parsing a 33³ cube is 36k floats; doing that per frame would dominate a grade. The stamp
/// is in the key because a LUT handed over by `path` is an ordinary file an agent may
/// re-export mid-session — blobs in the asset store are content-addressed and cannot change
/// under us, but a working file can.
static LUT_CACHE: LazyLock<Mutex<HashMap<(PathBuf, u64, u128), Arc<Lut>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn load_lut(cx: &EffectCx, path: &Path) -> Result<Arc<Lut>> {
    let (len, modified) = stamp(path);
    let key = (path.to_path_buf(), len, modified);
    if let Some(hit) = LUT_CACHE
        .lock()
        .expect("LUT cache mutex is never held across a panic")
        .get(&key)
    {
        return Ok(Arc::clone(hit));
    }
    let bytes = cx.assets.vfs().read(path)?;
    let text = String::from_utf8_lossy(&bytes);
    let parsed = Arc::new(Lut::parse_cube(&text).map_err(|error| {
        Error::bad_args(format!("LUT '{}': {error}", path.display()))
    })?);
    LUT_CACHE
        .lock()
        .expect("LUT cache mutex is never held across a panic")
        .insert(key, Arc::clone(&parsed));
    Ok(parsed)
}

/// The `.cube` file this effect names: an imported asset, or a path relative to the project.
fn lut_path(effect: &Effect, cx: &EffectCx) -> Result<PathBuf> {
    match (effect.string("asset"), effect.string("path")) {
        (Some(_), Some(_)) => Err(Error::bad_args(
            "effect 'color.lut' takes 'asset' or 'path', not both",
        )),
        (Some(query), None) => {
            let id = cx.project.resolve_asset(query)?;
            cx.assets.find(&cx.project.asset(&id)?.hash)
        }
        (None, Some(path)) => Ok(cx.paths.resolve(path)),
        (None, None) => Err(Error::bad_args(
            "effect 'color.lut' needs 'asset' (an imported .cube) or 'path'",
        )),
    }
}

fn color_lut(frame: &mut Frame, effect: &Effect, cx: &EffectCx) -> Result<()> {
    accept(effect, LUT_PARAMS)?;
    let path = lut_path(effect, cx)?;
    let table = load_lut(cx, &path)?;
    map_colors(frame, |rgb| {
        let display = [
            linear_to_srgb(rgb[0]),
            linear_to_srgb(rgb[1]),
            linear_to_srgb(rgb[2]),
        ];
        let graded = table.sample(display);
        [
            srgb_to_linear(graded[0]),
            srgb_to_linear(graded[1]),
            srgb_to_linear(graded[2]),
        ]
    });
    Ok(())
}

// ---------------------------------------------------------------------------- blur

const BLUR_PARAMS: &[&str] = &["radius"];
const SHARPEN_PARAMS: &[&str] = &["amount", "radius"];

/// Three box passes approximate a Gaussian to within about 3% of its kernel, and each pass
/// is O(1) per pixel because the window is a running sum. That is the whole reason a
/// 100-pixel blur costs the same as a 2-pixel one.
const BLUR_PASSES: usize = 3;

/// One horizontal box pass, rows in parallel. Out-of-range taps clamp to the edge pixel,
/// which for an interior feature preserves its total energy exactly; the accumulator is
/// `f64` so a 4K row does not drift as values slide in and out of it.
fn box_pass_rows(src: &[f32], dst: &mut [f32], width: usize, radius: usize) {
    let inverse = 1.0 / (radius * 2 + 1) as f64;
    dst.par_chunks_mut(width * 4)
        .zip(src.par_chunks(width * 4))
        .for_each(|(out_row, in_row)| {
            let tap = |index: isize| -> &[f32] {
                let clamped = index.clamp(0, width as isize - 1) as usize * 4;
                &in_row[clamped..clamped + 4]
            };
            let mut sum = [0.0f64; 4];
            for channel in 0..4 {
                sum[channel] = f64::from(tap(0)[channel]) * (radius + 1) as f64;
            }
            for x in 1..=radius as isize {
                let pixel = tap(x);
                for channel in 0..4 {
                    sum[channel] += f64::from(pixel[channel]);
                }
            }
            for x in 0..width as isize {
                let out = &mut out_row[x as usize * 4..x as usize * 4 + 4];
                for channel in 0..4 {
                    out[channel] = (sum[channel] * inverse) as f32;
                }
                let leaving = tap(x - radius as isize);
                let entering = tap(x + radius as isize + 1);
                for channel in 0..4 {
                    sum[channel] += f64::from(entering[channel]) - f64::from(leaving[channel]);
                }
            }
        });
}

/// Transpose a 4-channel plane. The vertical blur is the horizontal one on the transpose,
/// which keeps every pass sequential in memory; one strided read stream is much cheaper
/// than a strided write stream.
fn transpose(src: &[f32], dst: &mut [f32], width: usize, height: usize) {
    dst.par_chunks_mut(height * 4)
        .enumerate()
        .for_each(|(x, column)| {
            for y in 0..height {
                let from = (y * width + x) * 4;
                column[y * 4..y * 4 + 4].copy_from_slice(&src[from..from + 4]);
            }
        });
}

/// Blur in place with per-axis radii in frame pixels. Radii round to whole pixels and a
/// radius of zero is an exact no-op.
///
/// The buffer is premultiplied, so all four channels are blurred directly: colour × coverage
/// and coverage average consistently, and no unpremultiply step can divide by a blurred
/// alpha that is now different from the one the colour was multiplied by.
fn box_blur(frame: &mut Frame, radius_x: f32, radius_y: f32) {
    let horizontal = radius_x.round().max(0.0) as usize;
    let vertical = radius_y.round().max(0.0) as usize;
    let (width, height) = (frame.width(), frame.height());
    if (horizontal == 0 && vertical == 0) || width == 0 || height == 0 {
        return;
    }
    let mut front = std::mem::replace(frame, Frame::transparent(0, 0)).into_pixels();
    let mut back = vec![0.0f32; front.len()];
    let (columns, rows) = (width as usize, height as usize);
    if horizontal > 0 {
        for _ in 0..BLUR_PASSES {
            box_pass_rows(&front, &mut back, columns, horizontal);
            std::mem::swap(&mut front, &mut back);
        }
    }
    if vertical > 0 {
        transpose(&front, &mut back, columns, rows);
        std::mem::swap(&mut front, &mut back);
        for _ in 0..BLUR_PASSES {
            box_pass_rows(&front, &mut back, rows, vertical);
            std::mem::swap(&mut front, &mut back);
        }
        transpose(&front, &mut back, rows, columns);
        std::mem::swap(&mut front, &mut back);
    }
    *frame = Frame::from_pixels(width, height, front);
}

/// `radius` in sequence pixels, rescaled into this frame's pixels.
fn blur_radii(frame: &Frame, effect: &Effect, cx: &EffectCx) -> Result<[f32; 2]> {
    let radius = scalar(cx, effect, "radius", 0.0)?;
    if radius < 0.0 {
        return Err(Error::bad_args(format!(
            "effect '{}' parameter 'radius' must not be negative, got {radius}",
            effect.kind
        )));
    }
    let scale = cx.pixel_scale(frame);
    Ok([radius as f32 * scale[0], radius as f32 * scale[1]])
}

fn blur(frame: &mut Frame, effect: &Effect, cx: &EffectCx) -> Result<()> {
    accept(effect, BLUR_PARAMS)?;
    let [x, y] = blur_radii(frame, effect, cx)?;
    box_blur(frame, x, y);
    Ok(())
}

/// Unsharp mask: the frame plus `amount` times its difference from a blurred copy. Negative
/// amounts soften, which is occasionally what a denoise pass wants.
fn sharpen(frame: &mut Frame, effect: &Effect, cx: &EffectCx) -> Result<()> {
    accept(effect, SHARPEN_PARAMS)?;
    let amount = scalar(cx, effect, "amount", 1.0)? as f32;
    let radius = scalar(cx, effect, "radius", 1.0)?;
    if radius < 0.0 {
        return Err(Error::bad_args(format!(
            "effect '{}' parameter 'radius' must not be negative, got {radius}",
            effect.kind
        )));
    }
    if amount == 0.0 {
        return Ok(());
    }
    let scale = cx.pixel_scale(frame);
    let mut soft = frame.clone();
    box_blur(
        &mut soft,
        radius as f32 * scale[0],
        radius as f32 * scale[1],
    );
    let stride = frame.width() as usize * 4;
    if stride == 0 {
        return Ok(());
    }
    frame
        .pixels_mut()
        .par_chunks_mut(stride)
        .zip(soft.pixels().par_chunks(stride))
        .for_each(|(row, blurred)| {
            for (pixel, low) in row.chunks_exact_mut(4).zip(blurred.chunks_exact(4)) {
                for channel in 0..4 {
                    pixel[channel] += amount * (pixel[channel] - low[channel]);
                }
                // Coverage stays a coverage; colour keeps its f32 headroom above 1.0 so a
                // sharpened highlight is not clipped mid-chain, but cannot go negative.
                pixel[3] = pixel[3].clamp(0.0, 1.0);
                for channel in 0..3 {
                    pixel[channel] = pixel[channel].max(0.0);
                }
            }
        });
    Ok(())
}

// ---------------------------------------------------------------------------- crop / mask

const CROP_PARAMS: &[&str] = &["left", "top", "right", "bottom"];

/// Zero the cropped border's coverage.
///
/// The *geometric* crop — the one that changes which source rectangle is placed in the
/// frame — lives in the compositor. This one is an effect in the chain so it can sit after
/// a grade, which is what you want when the grade was matched on the full frame and the
/// crop is a reframe.
fn crop(frame: &mut Frame, effect: &Effect, cx: &EffectCx) -> Result<()> {
    accept(effect, CROP_PARAMS)?;
    let left = fraction(cx, effect, "left")?;
    let top = fraction(cx, effect, "top")?;
    let right = fraction(cx, effect, "right")?;
    let bottom = fraction(cx, effect, "bottom")?;
    if left + right == 0.0 && top + bottom == 0.0 {
        return Ok(());
    }
    let width = frame.width() as f32;
    let height = frame.height() as f32;
    let (min_x, max_x) = (left * width, width - right * width);
    let (min_y, max_y) = (top * height, height - bottom * height);
    map_coverage(frame, |x, y| {
        if x >= min_x && x < max_x && y >= min_y && y < max_y {
            1.0
        } else {
            0.0
        }
    });
    Ok(())
}

const MASK_PARAMS: &[&str] = &[
    "shape", "x", "y", "width", "height", "feather", "invert",
];

/// Signed distance from a pixel centre to a rectangle, negative inside. Exact along each
/// axis and an approximation diagonally outside a corner, which a feather ramp of a few
/// pixels cannot resolve.
fn rect_distance(x: f32, y: f32, rect: [f32; 4]) -> f32 {
    let [left, top, width, height] = rect;
    let dx = (x - (left + width * 0.5)).abs() - width * 0.5;
    let dy = (y - (top + height * 0.5)).abs() - height * 0.5;
    dx.max(dy)
}

/// Signed distance to an ellipse inscribed in `rect`, negative inside, scaled back into
/// pixels by the smaller radius so a feather in pixels means the same thing on both shapes.
fn ellipse_distance(x: f32, y: f32, rect: [f32; 4]) -> f32 {
    let [left, top, width, height] = rect;
    let radius_x = (width * 0.5).max(f32::EPSILON);
    let radius_y = (height * 0.5).max(f32::EPSILON);
    let nx = (x - (left + radius_x)) / radius_x;
    let ny = (y - (top + radius_y)) / radius_y;
    ((nx * nx + ny * ny).sqrt() - 1.0) * radius_x.min(radius_y)
}

fn mask_shape(frame: &mut Frame, effect: &Effect, cx: &EffectCx) -> Result<()> {
    accept(effect, MASK_PARAMS)?;
    let ellipse = match effect.string("shape").unwrap_or("rect").trim() {
        "rect" => false,
        "ellipse" => true,
        other => {
            return Err(Error::bad_args(format!(
                "effect 'mask.shape' parameter 'shape' must be 'rect' or 'ellipse', got '{other}'"
            )))
        }
    };
    let x = scalar(cx, effect, "x", 0.0)? as f32;
    let y = scalar(cx, effect, "y", 0.0)? as f32;
    let width = scalar(cx, effect, "width", 1.0)? as f32;
    let height = scalar(cx, effect, "height", 1.0)? as f32;
    if width < 0.0 || height < 0.0 {
        return Err(Error::bad_args(format!(
            "effect 'mask.shape' needs a non-negative size, got width {width} height {height}"
        )));
    }
    let feather = fraction(cx, effect, "feather")?;
    let invert = flag(effect, "invert")?;
    let frame_width = frame.width() as f32;
    let frame_height = frame.height() as f32;
    let rect = [
        x * frame_width,
        y * frame_height,
        width * frame_width,
        height * frame_height,
    ];
    // A feather is a fraction of the shorter axis: the same number then means the same
    // visual softness on a 16:9 frame as on a square one.
    let feather_px = feather * frame_width.min(frame_height);
    map_coverage(frame, move |px, py| {
        let distance = if ellipse {
            ellipse_distance(px, py, rect)
        } else {
            rect_distance(px, py, rect)
        };
        let coverage = if feather_px <= 0.0 {
            if distance < 0.0 {
                1.0
            } else {
                0.0
            }
        } else {
            // Ramp centred on the boundary, so feathering does not move the edge.
            (0.5 - distance / feather_px).clamp(0.0, 1.0)
        };
        if invert {
            1.0 - coverage
        } else {
            coverage
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------- chroma-key

const KEY_PARAMS: &[&str] = &["color", "similarity", "blend", "spill"];

/// Linear RGB → luma plus Rec.709-shaped chroma differences.
fn to_ycc(rgb: [f32; 3]) -> [f32; 3] {
    let y = luma(rgb);
    [y, (rgb[2] - y) / CB_SCALE, (rgb[0] - y) / CR_SCALE]
}

/// The inverse of [`to_ycc`]: green is recovered from luma, which is what lets spill
/// suppression edit chroma without touching brightness.
fn from_ycc(ycc: [f32; 3]) -> [f32; 3] {
    let [y, cb, cr] = ycc;
    let r = y + CR_SCALE * cr;
    let b = y + CB_SCALE * cb;
    let g = (y - LUMA[0] * r - LUMA[2] * b) / LUMA[1];
    [r, g, b]
}

/// Key on chroma distance, not RGB distance.
///
/// RGB distance mixes brightness into the decision, and that is what produces a hard,
/// fringed edge: a shadowed fold of the same green screen is far from the key colour in RGB
/// while a mid-grey of similar brightness is near it, so the tolerance has to be opened
/// until the grey goes too — and the semi-transparent edge pixels, which are physically a
/// *mixture* of screen and subject, flip between fully keyed and fully kept with nothing in
/// between. Splitting luma from chroma puts the mixture proportionally between the two
/// chromas, so the ramp from `similarity` to `similarity + blend` hands it a proportional
/// alpha. `spill` then removes the screen colour that the mixture still carries, holding
/// luma constant so the edge does not darken.
fn chroma_key(frame: &mut Frame, effect: &Effect, cx: &EffectCx) -> Result<()> {
    accept(effect, KEY_PARAMS)?;
    let color = match effect.string("color") {
        Some(text) => Rgba::parse(text).map_err(|error| {
            Error::bad_args(format!("effect 'chroma-key' parameter 'color': {error}"))
        })?,
        None => Rgba::opaque(0, 255, 0),
    };
    let premultiplied = color.to_linear_premul();
    let key_alpha = premultiplied[3].max(f32::EPSILON);
    let key = to_ycc([
        premultiplied[0] / key_alpha,
        premultiplied[1] / key_alpha,
        premultiplied[2] / key_alpha,
    ]);
    let key_chroma = key[1] * key[1] + key[2] * key[2];
    if key_chroma <= f32::EPSILON {
        return Err(Error::bad_args(format!(
            "effect 'chroma-key' cannot key on '{color}': a neutral colour has no chroma to \
             separate, so every grey in the shot would match it"
        )));
    }
    let similarity = scalar(cx, effect, "similarity", 0.25)? as f32;
    let blend = scalar(cx, effect, "blend", 0.1)? as f32;
    let spill = scalar(cx, effect, "spill", 0.5)? as f32;
    if similarity < 0.0 || blend < 0.0 {
        return Err(Error::bad_args(format!(
            "effect 'chroma-key' needs non-negative 'similarity' and 'blend', got {similarity} and {blend}"
        )));
    }
    if !(0.0..=1.0).contains(&spill) {
        return Err(Error::bad_args(format!(
            "effect 'chroma-key' parameter 'spill' is a fraction and must be 0..=1, got {spill}"
        )));
    }
    let stride = frame.width() as usize * 4;
    if stride == 0 {
        return Ok(());
    }
    frame.pixels_mut().par_chunks_mut(stride).for_each(|row| {
        for pixel in row.chunks_exact_mut(4) {
            let alpha = pixel[3];
            if alpha <= f32::EPSILON {
                continue;
            }
            let inverse = 1.0 / alpha;
            let ycc = to_ycc([
                pixel[0] * inverse,
                pixel[1] * inverse,
                pixel[2] * inverse,
            ]);
            let distance =
                ((ycc[1] - key[1]).powi(2) + (ycc[2] - key[2]).powi(2)).sqrt();
            let coverage = if distance <= similarity {
                0.0
            } else if blend <= 0.0 || distance >= similarity + blend {
                1.0
            } else {
                (distance - similarity) / blend
            };
            if coverage <= 0.0 {
                pixel.fill(0.0);
                continue;
            }
            let mut kept = ycc;
            if spill > 0.0 {
                // How much of this pixel's chroma points at the screen colour. Only the
                // positive projection is spill; the opposite hue is the subject's own.
                let projection = (ycc[1] * key[1] + ycc[2] * key[2]) / key_chroma;
                if projection > 0.0 {
                    let take = spill * projection.min(1.0);
                    kept[1] -= take * key[1];
                    kept[2] -= take * key[2];
                }
            }
            let rgb = from_ycc(kept);
            let out_alpha = alpha * coverage;
            pixel[0] = rgb[0].max(0.0) * out_alpha;
            pixel[1] = rgb[1].max(0.0) * out_alpha;
            pixel[2] = rgb[2].max(0.0) * out_alpha;
            pixel[3] = out_alpha;
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------- stabilize

const STABILIZE_PARAMS: &[&str] = &["smoothing", "zoom"];

/// Below this radius from the measurement centroid, a one-pixel motion error is a large
/// angle, so those vectors are left out of the rotation estimate.
const ROTATION_MIN_RADIUS: f32 = 16.0;

/// One local motion vector from `vidstabdetect`: where the measured field was, and the
/// offset that maps it back onto the previous frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalMotion {
    /// Field centre in source pixels.
    pub at: [i32; 2],
    /// Offset onto the previous frame, in source pixels.
    pub offset: [i32; 2],
    /// Field edge length.
    pub size: i32,
    /// Local contrast, which is how much the measurement is worth.
    pub contrast: f64,
    /// Match error the search settled on.
    pub match_error: f64,
}

/// The vectors measured for one source frame. `frame` is 1-based as the file writes it;
/// frame 1 carries none, because there is no previous frame to match against.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameMotion {
    pub frame: i64,
    pub motions: Vec<LocalMotion>,
}

/// The counter-motion applied to one frame: a translation in *source media* pixels and a
/// rotation in radians about the frame centre.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Correction {
    pub dx: f32,
    pub dy: f32,
    pub rotation: f32,
}

/// Where the analysis for a clip lives. Under `cache/` because it is regenerable from the
/// media and must never be mistaken for part of the document.
pub fn stabilize_cache_path(paths: &ProjectPaths, clip: &ClipId) -> PathBuf {
    paths
        .cache_dir()
        .join("stabilize")
        .join(format!("{clip}.trf"))
}

/// Parse the ASCII `vid.stab` transform file `vidstabdetect` writes with
/// `fileformat=ascii`.
///
/// The header is `VID.STAB 1` followed by `#` comment lines; every later line is
/// `Frame <n> (List <k> [(LM <v-x> <v-y> <f-x> <f-y> <size> <contrast> <match>),…])`.
/// The binary format is refused loudly rather than parsed as text, because ffmpeg writes
/// binary *by default* and the resulting garbage transforms would look like extreme shake.
pub fn parse_trf(text: &str) -> Result<Vec<FrameMotion>> {
    let mut frames = Vec::new();
    let mut header = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !header {
            if !line.starts_with("VID.STAB") {
                let head: String = line.chars().take(16).filter(|c| !c.is_control()).collect();
                return Err(Error::op(format!(
                    "not an ASCII vid.stab transform file (it begins '{head}'); ffmpeg's \
                     vidstabdetect writes the binary format unless it is given \
                     fileformat=ascii — re-run `fx.analyze`"
                )));
            }
            header = true;
            continue;
        }
        let Some(rest) = line.strip_prefix("Frame ") else {
            continue;
        };
        let (index, tail) = rest.split_once(' ').unwrap_or((rest, ""));
        let frame: i64 = index.trim().parse().map_err(|_| {
            Error::op(format!("cannot read a frame number from '{line}'"))
        })?;
        let mut motions = Vec::new();
        for chunk in tail.split("(LM ").skip(1) {
            let body = chunk.split(')').next().unwrap_or_default();
            motions.push(parse_local_motion(body).ok_or_else(|| {
                Error::op(format!("frame {frame}: cannot read local motion '(LM {body})'"))
            })?);
        }
        frames.push(FrameMotion { frame, motions });
    }
    if !header {
        return Err(Error::op(
            "the transform file is empty; re-run `fx.analyze`",
        ));
    }
    Ok(frames)
}

fn parse_local_motion(body: &str) -> Option<LocalMotion> {
    let mut parts = body.split_whitespace();
    let mut integers = [0i32; 5];
    for slot in integers.iter_mut() {
        *slot = parts.next()?.parse().ok()?;
    }
    let contrast: f64 = parts.next()?.parse().ok()?;
    let match_error: f64 = parts.next()?.parse().ok()?;
    Some(LocalMotion {
        offset: [integers[0], integers[1]],
        at: [integers[2], integers[3]],
        size: integers[4],
        contrast,
        match_error,
    })
}

/// Median of the samples. A mean follows a moving subject: a person walking across a locked
/// shot contributes a whole cluster of vectors that disagree with the camera, and averaging
/// them makes the "camera" chase the person.
fn median(values: &mut [f32]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let middle = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) * 0.5
    } else {
        values[middle]
    }
}

/// How far the content moved between the previous frame and this one: `[dx, dy, radians]`.
///
/// `vidstabdetect` reports the offset that maps the current frame *back* onto the previous
/// one, so the content displacement is its negation. Verified against a synthetic pan:
/// content moving +8 px to the right per frame is written as `v-x = -8`. Getting this
/// backwards does not merely fail to stabilize — it doubles the shake.
fn frame_step(motions: &[LocalMotion]) -> [f32; 3] {
    if motions.is_empty() {
        return [0.0; 3];
    }
    let count = motions.len() as f32;
    let mut center = [0.0f32; 2];
    for motion in motions {
        center[0] += motion.at[0] as f32;
        center[1] += motion.at[1] as f32;
    }
    center[0] /= count;
    center[1] /= count;
    let mut xs: Vec<f32> = motions.iter().map(|m| -(m.offset[0] as f32)).collect();
    let mut ys: Vec<f32> = motions.iter().map(|m| -(m.offset[1] as f32)).collect();
    let mut rotations: Vec<f32> = motions
        .iter()
        .filter_map(|motion| {
            let rx = motion.at[0] as f32 - center[0];
            let ry = motion.at[1] as f32 - center[1];
            let radius = rx * rx + ry * ry;
            if radius < ROTATION_MIN_RADIUS * ROTATION_MIN_RADIUS {
                return None;
            }
            let vx = -(motion.offset[0] as f32);
            let vy = -(motion.offset[1] as f32);
            // Tangential component: the small-angle rotation this vector implies.
            Some((rx * vy - ry * vx) / radius)
        })
        .collect();
    [
        median(&mut xs),
        median(&mut ys),
        median(&mut rotations),
    ]
}

/// Turn per-frame measurements into the per-frame counter-motion.
///
/// The measured steps integrate into the camera's actual path; a boxcar mean over
/// ±`smoothing` frames is the path we want to keep — a deliberate pan survives it, because
/// a straight ramp is its own moving average, while a one-frame jolt does not. The
/// correction is the difference, so a locked shot gets nothing done to it and a shaky one
/// is pulled back onto the smooth path.
pub fn solve_corrections(frames: &[FrameMotion], smoothing: usize) -> Vec<Correction> {
    let mut path: Vec<[f32; 3]> = Vec::with_capacity(frames.len());
    let mut absolute = [0.0f32; 3];
    for frame in frames {
        let step = frame_step(&frame.motions);
        for axis in 0..3 {
            absolute[axis] += step[axis];
        }
        path.push(absolute);
    }
    let mut out = Vec::with_capacity(path.len());
    for index in 0..path.len() {
        let low = index.saturating_sub(smoothing);
        let high = (index + smoothing + 1).min(path.len());
        let window = &path[low..high];
        let count = window.len() as f32;
        let mut mean = [0.0f32; 3];
        for sample in window {
            for axis in 0..3 {
                mean[axis] += sample[axis];
            }
        }
        out.push(Correction {
            dx: mean[0] / count - path[index][0],
            dy: mean[1] / count - path[index][1],
            rotation: mean[2] / count - path[index][2],
        });
    }
    out
}

/// Solved corrections, keyed by the file, its stamp and the smoothing window. Re-reading
/// and re-solving a ten-minute `.trf` per frame would cost more than the warp itself.
#[allow(clippy::type_complexity)]
static STABILIZE_CACHE: LazyLock<
    Mutex<HashMap<(PathBuf, u64, u128, usize), Arc<Vec<Correction>>>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Read from the real filesystem rather than through the project's
/// [`dvs_core::vfs::Vfs`]: unlike a LUT, which is a document asset, the `.trf` is written
/// into `cache/` by ffmpeg itself and only exists there.
fn load_corrections(path: &Path, smoothing: usize) -> Result<Arc<Vec<Correction>>> {
    let (len, modified) = stamp(path);
    let key = (path.to_path_buf(), len, modified, smoothing);
    if let Some(hit) = STABILIZE_CACHE
        .lock()
        .expect("stabilize cache mutex is never held across a panic")
        .get(&key)
    {
        return Ok(Arc::clone(hit));
    }
    let text = std::fs::read_to_string(path).map_err(|error| Error::io(path, error))?;
    let frames = parse_trf(&text)?;
    let solved = Arc::new(solve_corrections(&frames, smoothing));
    STABILIZE_CACHE
        .lock()
        .expect("stabilize cache mutex is never held across a panic")
        .insert(key, Arc::clone(&solved));
    Ok(solved)
}

/// Counter-translate, counter-rotate and zoom in, sampling the source at the inverse-mapped
/// position so the output has no holes. `zoom` crops in from every edge first, which is how
/// the empty border the shake pushed into frame stays out of the picture.
fn warp(frame: &mut Frame, dx: f32, dy: f32, rotation: f32, zoom: f32) {
    let (width, height) = (frame.width(), frame.height());
    if width == 0 || height == 0 || (dx == 0.0 && dy == 0.0 && rotation == 0.0 && zoom == 0.0) {
        return;
    }
    let center_x = width as f32 * 0.5;
    let center_y = height as f32 * 0.5;
    let (sin, cos) = rotation.sin_cos();
    let keep = 1.0 - 2.0 * zoom;
    let pixels = {
        let source: &Frame = frame;
        let mut out = vec![0.0f32; (width as usize) * (height as usize) * 4];
        out.par_chunks_mut(width as usize * 4)
            .enumerate()
            .for_each(|(y, row)| {
                let oy = (y as f32 + 0.5 - center_y) * keep;
                for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
                    let ox = (x as f32 + 0.5 - center_x) * keep;
                    // Inverse rotation: where in the source this destination pixel looks.
                    let sx = ox * cos + oy * sin + center_x - dx;
                    let sy = -ox * sin + oy * cos + center_y - dy;
                    pixel.copy_from_slice(&source.sample_bilinear(sx - 0.5, sy - 0.5));
                }
            });
        out
    };
    *frame = Frame::from_pixels(width, height, pixels);
}

fn stabilize(frame: &mut Frame, effect: &Effect, cx: &EffectCx) -> Result<()> {
    accept(effect, STABILIZE_PARAMS)?;
    let smoothing = scalar(cx, effect, "smoothing", 10.0)?;
    if smoothing < 0.0 {
        return Err(Error::bad_args(format!(
            "effect 'stabilize' parameter 'smoothing' is a frame count and must not be negative, got {smoothing}"
        )));
    }
    let zoom = scalar(cx, effect, "zoom", 0.0)?;
    if !(0.0..0.5).contains(&zoom) {
        return Err(Error::bad_args(format!(
            "effect 'stabilize' parameter 'zoom' is the fraction cropped in from each edge and must be 0..0.5, got {zoom}"
        )));
    }
    let stream = cx.video_stream()?;
    let path = stabilize_cache_path(cx.paths, &cx.clip.id);
    if !path.is_file() {
        return Err(Error::op(format!(
            "clip '{}' has a stabilize effect but no motion analysis at {}; run \
             `fx.analyze --target '#{}'` first",
            cx.clip.label(),
            cx.paths.relativize(&path),
            cx.clip.label()
        )));
    }
    let corrections = load_corrections(&path, smoothing.round() as usize)?;
    if corrections.is_empty() {
        return Err(Error::op(format!(
            "motion analysis at {} covers no frames; re-run `fx.analyze --target '#{}'`",
            cx.paths.relativize(&path),
            cx.clip.label()
        )));
    }
    // The `.trf` is indexed by source frame at the media's own rate, which is what the
    // analysis decoded; the clip's speed and direction are already in `source_time`.
    let index = cx.clip.source_time(cx.at).frame_floor(stream.fps).max(0) as usize;
    let Some(correction) = corrections.get(index) else {
        return Err(Error::op(format!(
            "motion analysis at {} covers {} frames but clip '{}' needs source frame {index}; \
             the media changed since the analysis — re-run `fx.analyze --target '#{}'`",
            cx.paths.relativize(&path),
            corrections.len(),
            cx.clip.label(),
            cx.clip.label()
        )));
    };
    // Vectors were measured on the master media; the chain may be running on a decode-time
    // resize, so a translation in source pixels has to be scaled into frame pixels.
    let scale = [
        ratio(frame.width(), stream.size[0]),
        ratio(frame.height(), stream.size[1]),
    ];
    warp(
        frame,
        correction.dx * scale[0],
        correction.dy * scale[1],
        correction.rotation,
        zoom as f32,
    );
    Ok(())
}

/// Escape a path for a libavfilter option value. `:` separates options and `,` separates
/// filters, so an unescaped project directory with a colon in it would silently become a
/// different filter graph.
fn escape_filter_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut out = String::with_capacity(text.len() + 8);
    for character in text.chars() {
        if matches!(character, '\\' | ':' | ',' | '\'' | '[' | ']' | ';' | '=') {
            out.push('\\');
        }
        out.push(character);
    }
    out
}

/// Measure camera motion for `source` into `out`, the first pass of stabilization.
///
/// `shakiness` is libvidstab's 1..=10 search aggressiveness. `fileformat=ascii` is not
/// optional here: the default is a binary blob whose layout is a libvidstab implementation
/// detail, and this engine parses the transforms itself so that the warp is ours and
/// deterministic rather than ffmpeg's.
pub fn analyze_stabilization(
    cx: &EffectCx,
    source: &Path,
    out: &Path,
    shakiness: u32,
) -> Result<()> {
    if !(1..=10).contains(&shakiness) {
        return Err(Error::bad_args(format!(
            "shakiness is libvidstab's 1..=10 search aggressiveness, got {shakiness}"
        )));
    }
    if !cx.tool.has_filter("vidstabdetect") {
        return Err(Error::tool(
            "ffmpeg",
            "this build has no 'vidstabdetect' filter (it was compiled without libvidstab), \
             so stabilization cannot be analysed; install an ffmpeg with --enable-libvidstab",
        ));
    }
    if let Some(parent) = out.parent() {
        // ffmpeg writes this file itself, so the directory has to exist on the real
        // filesystem rather than in whatever `Vfs` the project is using.
        std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
    }
    let filter = format!(
        "vidstabdetect=result={}:shakiness={shakiness}:accuracy=15:fileformat=ascii",
        escape_filter_path(out)
    );
    let finished = cx
        .tool
        .ffmpeg_command()
        .arg("-i")
        .arg(source)
        .args(["-an", "-vf", &filter, "-f", "null", "-"])
        .output()
        .map_err(|error| Error::tool("ffmpeg", error.to_string()))?;
    if !finished.status.success() {
        return Err(Error::tool(
            "ffmpeg",
            format!(
                "vidstabdetect on '{}' failed: {}",
                source.display(),
                String::from_utf8_lossy(&finished.stderr).trim()
            ),
        ));
    }
    if !out.is_file() {
        return Err(Error::tool(
            "ffmpeg",
            format!(
                "vidstabdetect reported success but wrote no transforms to '{}'",
                out.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::ids::AssetId;
    use dvs_core::project::{Asset, Source};
    use dvs_core::time::Fps;
    use dvs_core::vfs::FsVfs;
    use std::io::Write;

    /// A project with one 100x100 30 fps video asset and one two-second clip using it.
    struct Fixture {
        project: Project,
        paths: ProjectPaths,
        assets: AssetStore,
        clip: Clip,
        sequence_size: [u32; 2],
        dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = ProjectPaths::new(dir.path());
        let assets = AssetStore::new(paths.assets_dir(), Arc::new(FsVfs));
        let fps = Fps::new(30, 1).expect("30 fps");
        let mut project = Project::new("effects", fps, [100, 100], 48000);
        // Built from JSON so the test does not need a chrono dependency for `imported`.
        let asset: Asset = serde_json::from_value(serde_json::json!({
            "id": "ast_shot",
            "name": "shot.mp4",
            "hash": "blake3:0123",
            "kind": "video",
            "probe": {
                "duration": "10/1",
                "video": {
                    "streamIndex": 0,
                    "size": [100, 100],
                    "fps": "30",
                    "codec": "h264",
                    "pixFmt": "yuv420p"
                }
            },
            "imported": "2026-01-01T00:00:00Z"
        }))
        .expect("asset fixture deserializes");
        project.assets.insert(asset.id.clone(), asset);
        let clip = Clip::new(
            Source::Asset {
                asset: AssetId::from_raw("ast_shot"),
                stream: None,
            },
            Time::ZERO,
            Time::from_secs(2),
        );
        Fixture {
            project,
            paths,
            assets,
            clip,
            sequence_size: [100, 100],
            dir,
        }
    }

    impl Fixture {
        fn cx(&self) -> EffectCx<'_> {
            self.at(Time::ZERO)
        }

        fn at(&self, at: Time) -> EffectCx<'_> {
            EffectCx {
                project: &self.project,
                tool: Toolchain::shared().expect("ffmpeg is installed for the test suite"),
                paths: &self.paths,
                assets: &self.assets,
                clip: &self.clip,
                at,
                sequence_size: self.sequence_size,
            }
        }
    }

    fn effect(kind: &str, params: serde_json::Value) -> Effect {
        let mut effect = Effect::new(kind);
        effect.params = params
            .as_object()
            .expect("test params are an object")
            .clone();
        effect
    }

    /// An opaque frame of one colour, in linear light.
    fn solid(width: u32, height: u32, rgb: [f32; 3]) -> Frame {
        let mut pixels = Vec::with_capacity((width as usize) * (height as usize) * 4);
        for _ in 0..(width as usize) * (height as usize) {
            pixels.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 1.0]);
        }
        Frame::from_pixels(width, height, pixels)
    }

    fn total_alpha(frame: &Frame) -> f64 {
        frame
            .pixels()
            .chunks_exact(4)
            .map(|pixel| f64::from(pixel[3]))
            .sum()
    }

    #[test]
    fn exposure_doubles_light_not_encoded_values() {
        let fixture = fixture();
        let mut frame = solid(2, 2, [0.1, 0.2, 0.3]);
        let before = frame.to_rgba8();
        apply(
            &mut frame,
            &effect("color.grade", serde_json::json!({ "exposure": 1 })),
            &fixture.cx(),
        )
        .expect("grade applies");
        let pixel = frame.pixel(1, 1);
        assert_eq!([pixel[0], pixel[1], pixel[2]], [0.2, 0.4, 0.6]);
        let after = frame.to_rgba8();
        // sRGB is not linear: twice the light is far from twice the code value, and a grade
        // that doubled the encoded bytes would be a very different picture.
        assert!(
            after[0] < u32::from(before[0]) as u8 * 2 - 20,
            "encoded {} vs {}",
            after[0],
            before[0]
        );
    }

    #[test]
    fn zero_saturation_greys_at_constant_luminance() {
        let fixture = fixture();
        let red = [1.0f32, 0.0, 0.0];
        let mut frame = solid(2, 2, red);
        apply(
            &mut frame,
            &effect("color.grade", serde_json::json!({ "saturation": 0 })),
            &fixture.cx(),
        )
        .expect("grade applies");
        let pixel = frame.pixel(0, 0);
        assert!(
            (pixel[0] - pixel[1]).abs() < 1e-6 && (pixel[1] - pixel[2]).abs() < 1e-6,
            "not grey: {pixel:?}"
        );
        assert!(
            (luma([pixel[0], pixel[1], pixel[2]]) - luma(red)).abs() < 1e-5,
            "luminance moved: {} -> {}",
            luma(red),
            luma([pixel[0], pixel[1], pixel[2]])
        );
        // The channel mean would be 1/3; luminance-weighted grey for pure red is 0.2126.
        assert!(
            (pixel[0] - 0.2126).abs() < 1e-4,
            "desaturated to the channel mean instead of the luminance: {pixel:?}"
        );
    }

    #[test]
    fn contrast_pivots_on_middle_grey() {
        let fixture = fixture();
        let mut frame = Frame::from_pixels(
            3,
            1,
            vec![
                0.05, 0.05, 0.05, 1.0, //
                MIDDLE_GRAY, MIDDLE_GRAY, MIDDLE_GRAY, 1.0, //
                0.5, 0.5, 0.5, 1.0,
            ],
        );
        apply(
            &mut frame,
            &effect("color.grade", serde_json::json!({ "contrast": 1.5 })),
            &fixture.cx(),
        )
        .expect("grade applies");
        let dark = frame.pixel(0, 0)[0];
        let grey = frame.pixel(1, 0)[0];
        let bright = frame.pixel(2, 0)[0];
        assert!(
            (grey - MIDDLE_GRAY).abs() < 1e-5,
            "middle grey moved to {grey}"
        );
        assert!(dark < 0.05, "dark did not darken: {dark}");
        assert!(bright > 0.5, "bright did not brighten: {bright}");
        assert!(
            bright - dark > 0.45,
            "contrast did not open the range: {dark}..{bright}"
        );
    }

    #[test]
    fn a_temperature_shift_trades_red_for_blue_at_constant_luminance() {
        let fixture = fixture();
        let neutral = [0.3f32, 0.3, 0.3];
        let mut warmed = solid(2, 2, neutral);
        apply(
            &mut warmed,
            &effect("color.grade", serde_json::json!({ "temperature": 1.0 })),
            &fixture.cx(),
        )
        .expect("grade applies");
        let pixel = warmed.pixel(0, 0);
        assert!(pixel[0] > neutral[0], "warming did not add red: {pixel:?}");
        assert!(pixel[2] < neutral[2], "warming did not remove blue: {pixel:?}");
        assert!(
            (luma([pixel[0], pixel[1], pixel[2]]) - luma(neutral)).abs() < 1e-5,
            "white balance changed the exposure: {} -> {}",
            luma(neutral),
            luma([pixel[0], pixel[1], pixel[2]])
        );

        let mut cooled = solid(2, 2, neutral);
        apply(
            &mut cooled,
            &effect("color.grade", serde_json::json!({ "temperature": -1.0 })),
            &fixture.cx(),
        )
        .expect("grade applies");
        assert!(
            cooled.pixel(0, 0)[2] > pixel[2],
            "cooling went the same way as warming"
        );
    }

    /// `size³` rows, red varying fastest, produced by `entry`.
    fn cube_text(size: usize, entry: impl Fn([f32; 3]) -> [f32; 3]) -> String {
        let mut text = String::from("TITLE \"test\"\n# a comment\n\n");
        text.push_str(&format!("LUT_3D_SIZE {size}\n"));
        text.push_str("DOMAIN_MIN 0.0 0.0 0.0\nDOMAIN_MAX 1.0 1.0 1.0\n\n");
        let last = (size - 1) as f32;
        for b in 0..size {
            for g in 0..size {
                for r in 0..size {
                    let value = entry([r as f32 / last, g as f32 / last, b as f32 / last]);
                    text.push_str(&format!("{} {} {}\n", value[0], value[1], value[2]));
                }
            }
        }
        text
    }

    fn write_cube(fixture: &Fixture, name: &str, text: &str) -> PathBuf {
        let path = fixture.dir.path().join(name);
        let mut file = std::fs::File::create(&path).expect("create cube");
        file.write_all(text.as_bytes()).expect("write cube");
        path
    }

    #[test]
    fn identity_cube_is_a_round_trip_and_inversion_happens_in_display_space() {
        let fixture = fixture();
        let identity = write_cube(&fixture, "identity.cube", &cube_text(2, |rgb| rgb));
        let inverted = write_cube(
            &fixture,
            "invert.cube",
            &cube_text(2, |rgb| [1.0 - rgb[0], 1.0 - rgb[1], 1.0 - rgb[2]]),
        );

        let original = solid(2, 2, [0.05, 0.25, 0.7]);
        let mut frame = original.clone();
        apply(
            &mut frame,
            &effect(
                "color.lut",
                serde_json::json!({ "path": identity.to_string_lossy() }),
            ),
            &fixture.cx(),
        )
        .expect("identity lut applies");
        for (after, before) in frame.to_rgba8().iter().zip(original.to_rgba8()) {
            assert!(
                after.abs_diff(before) <= 1,
                "identity LUT moved a pixel: {before} -> {after}"
            );
        }
        assert_eq!(frame.size(), [2, 2]);

        let mut frame = original.clone();
        apply(
            &mut frame,
            &effect(
                "color.lut",
                serde_json::json!({ "path": inverted.to_string_lossy() }),
            ),
            &fixture.cx(),
        )
        .expect("inverting lut applies");
        // A `.cube` is display-referred: inverting it must invert the *encoded* value. If
        // the linear→sRGB conversion were skipped (or applied twice) the bytes would come
        // out at 255 − encode(1 − linear) instead, which is tens of code values away.
        for (after, before) in frame.to_rgba8().chunks_exact(4).zip(original.to_rgba8().chunks_exact(4)) {
            for channel in 0..3 {
                let expected = 255 - before[channel];
                assert!(
                    after[channel].abs_diff(expected) <= 1,
                    "channel {channel}: expected ~{expected}, got {}",
                    after[channel]
                );
            }
        }
    }

    #[test]
    fn truncated_cube_is_rejected_with_the_expected_row_count() {
        let error = Lut::parse_cube("LUT_3D_SIZE 2\n0 0 0\n1 0 0\n").expect_err("short table");
        let message = error.to_string();
        assert!(message.contains('8') && message.contains('2'), "{message}");
    }

    #[test]
    fn one_dimensional_cube_is_three_channel_curves() {
        let lut = Lut::parse_cube("LUT_1D_SIZE 3\n0 0 0\n0.25 0.5 0.75\n1 1 1\n")
            .expect("1D cube parses");
        assert!(!lut.is_3d() && lut.size() == 3);
        let sampled = lut.sample([0.5, 0.5, 0.5]);
        assert!((sampled[0] - 0.25).abs() < 1e-6, "{sampled:?}");
        assert!((sampled[1] - 0.5).abs() < 1e-6, "{sampled:?}");
        assert!((sampled[2] - 0.75).abs() < 1e-6, "{sampled:?}");
    }

    #[test]
    fn blur_of_zero_is_a_no_op_and_a_large_blur_conserves_energy() {
        let mut fixture = fixture();
        fixture.sequence_size = [64, 64];
        let mut frame = Frame::transparent(64, 64);
        frame.set_pixel(32, 32, [1.0, 1.0, 1.0, 1.0]);
        let original = frame.clone();

        apply(
            &mut frame,
            &effect("blur", serde_json::json!({ "radius": 0 })),
            &fixture.cx(),
        )
        .expect("blur applies");
        assert_eq!(frame, original, "radius 0 must not touch the frame");

        apply(
            &mut frame,
            &effect("blur", serde_json::json!({ "radius": 8 })),
            &fixture.cx(),
        )
        .expect("blur applies");
        assert_eq!(frame.size(), [64, 64]);
        let spread = total_alpha(&frame);
        assert!(
            (spread - 1.0).abs() < 0.01,
            "blur lost or invented energy: {spread}"
        );
        assert!(
            frame.pixel(32, 32)[3] < 0.05,
            "the pixel did not actually spread: {:?}",
            frame.pixel(32, 32)
        );
        assert!(
            frame.pixel(40, 32)[3] > 0.0,
            "no energy eight pixels away from a radius-8 blur"
        );
    }

    #[test]
    fn blur_radius_is_in_sequence_pixels() {
        // A source decoded at half the sequence size must blur by half as many of its own
        // pixels, or the same document would look different at preview and at delivery.
        let mut fixture = fixture();
        fixture.sequence_size = [128, 128];
        let mut frame = Frame::transparent(64, 64);
        frame.set_pixel(32, 32, [1.0, 1.0, 1.0, 1.0]);
        apply(
            &mut frame,
            &effect("blur", serde_json::json!({ "radius": 4 })),
            &fixture.cx(),
        )
        .expect("blur applies");
        // Radius 4 sequence pixels is 2 frame pixels: three passes reach 6 px, not 12.
        assert!(frame.pixel(37, 32)[3] > 0.0, "expected spread within 6 px");
        assert_eq!(
            frame.pixel(41, 32)[3],
            0.0,
            "spread past the rescaled radius"
        );
    }

    #[test]
    fn sharpen_raises_local_contrast_across_an_edge() {
        let mut fixture = fixture();
        fixture.sequence_size = [32, 32];
        let mut frame = Frame::transparent(32, 32);
        for y in 0..32 {
            for x in 0..32 {
                let value = if x < 16 { 0.2 } else { 0.8 };
                frame.set_pixel(x, y, [value, value, value, 1.0]);
            }
        }
        apply(
            &mut frame,
            &effect("sharpen", serde_json::json!({ "amount": 1.5, "radius": 2 })),
            &fixture.cx(),
        )
        .expect("sharpen applies");
        assert_eq!(frame.size(), [32, 32]);
        let dark = frame.pixel(14, 16)[0];
        let light = frame.pixel(17, 16)[0];
        assert!(dark < 0.2, "no undershoot at the edge: {dark}");
        assert!(light > 0.8, "no overshoot at the edge: {light}");
        // Far from the edge nothing should move: an unsharp mask is a local operator.
        assert!((frame.pixel(1, 16)[0] - 0.2).abs() < 0.02);
    }

    #[test]
    fn crop_clears_coverage_at_the_requested_fractions() {
        let fixture = fixture();
        let mut frame = solid(100, 100, [0.5, 0.5, 0.5]);
        apply(
            &mut frame,
            &effect(
                "crop",
                serde_json::json!({ "left": 0.1, "right": 0.2, "top": 0.0, "bottom": 0.5 }),
            ),
            &fixture.cx(),
        )
        .expect("crop applies");
        assert_eq!(frame.size(), [100, 100]);
        assert_eq!(frame.pixel(9, 10)[3], 0.0, "column 9 should be cropped");
        assert_eq!(frame.pixel(10, 10)[3], 1.0, "column 10 should survive");
        assert_eq!(frame.pixel(79, 10)[3], 1.0, "column 79 should survive");
        assert_eq!(frame.pixel(80, 10)[3], 0.0, "column 80 should be cropped");
        assert_eq!(frame.pixel(50, 49)[3], 1.0, "row 49 should survive");
        assert_eq!(frame.pixel(50, 50)[3], 0.0, "row 50 should be cropped");
        // Premultiplied colour has to go with the coverage, or `to_rgba8` divides by zero.
        assert_eq!(frame.pixel(9, 10), [0.0; 4]);
    }

    #[test]
    fn hard_mask_edge_lands_on_the_requested_fraction() {
        let fixture = fixture();
        let params = serde_json::json!({
            "shape": "rect", "x": 0.25, "y": 0.0, "width": 0.5, "height": 1.0, "feather": 0
        });
        let mut frame = solid(100, 100, [0.5, 0.5, 0.5]);
        apply(&mut frame, &effect("mask.shape", params.clone()), &fixture.cx())
            .expect("mask applies");
        assert_eq!(frame.size(), [100, 100]);
        assert_eq!(frame.pixel(24, 50)[3], 0.0);
        assert_eq!(frame.pixel(25, 50)[3], 1.0);
        assert_eq!(frame.pixel(74, 50)[3], 1.0);
        assert_eq!(frame.pixel(75, 50)[3], 0.0);

        let mut inverted = solid(100, 100, [0.5, 0.5, 0.5]);
        let mut invert_params = params;
        invert_params["invert"] = serde_json::Value::Bool(true);
        apply(
            &mut inverted,
            &effect("mask.shape", invert_params),
            &fixture.cx(),
        )
        .expect("mask applies");
        assert_eq!(inverted.pixel(24, 50)[3], 1.0);
        assert_eq!(inverted.pixel(25, 50)[3], 0.0);
        assert_eq!(inverted.pixel(75, 50)[3], 1.0);
    }

    #[test]
    fn ellipse_mask_clears_the_corners() {
        let fixture = fixture();
        let mut frame = solid(100, 100, [0.5, 0.5, 0.5]);
        apply(
            &mut frame,
            &effect(
                "mask.shape",
                serde_json::json!({ "shape": "ellipse", "width": 1.0, "height": 1.0 }),
            ),
            &fixture.cx(),
        )
        .expect("mask applies");
        assert_eq!(frame.pixel(50, 50)[3], 1.0, "the centre must survive");
        for (x, y) in [(0, 0), (99, 0), (0, 99), (99, 99)] {
            assert_eq!(frame.pixel(x, y)[3], 0.0, "corner {x},{y} is outside");
        }
        assert_eq!(frame.pixel(50, 1)[3], 1.0, "the top of the ellipse is inside");
    }

    #[test]
    fn feathered_mask_ramps_instead_of_stepping() {
        let fixture = fixture();
        let mut frame = solid(100, 100, [0.5, 0.5, 0.5]);
        apply(
            &mut frame,
            &effect(
                "mask.shape",
                serde_json::json!({ "x": 0.25, "width": 0.5, "feather": 0.1 }),
            ),
            &fixture.cx(),
        )
        .expect("mask applies");
        let alpha = frame.pixel(25, 50)[3];
        assert!(
            (alpha - 0.5).abs() < 0.1,
            "the ramp should be half way at the boundary: {alpha}"
        );
        assert!(frame.pixel(20, 50)[3] < alpha, "no ramp outside the edge");
        assert!(frame.pixel(30, 50)[3] > alpha, "no ramp inside the edge");
        assert_eq!(frame.pixel(50, 50)[3], 1.0, "the interior must stay solid");
    }

    #[test]
    fn chroma_key_removes_the_screen_and_keeps_the_subject() {
        let fixture = fixture();
        let green = Rgba::parse("#00ff00").expect("hex").to_linear_premul();
        let skin = Rgba::parse("#ff8040").expect("hex").to_linear_premul();
        let mut pixels = Vec::with_capacity(100 * 100 * 4);
        for _ in 0..100 {
            for x in 0..100 {
                pixels.extend_from_slice(if x < 50 { &green } else { &skin });
            }
        }
        let mut frame = Frame::from_pixels(100, 100, pixels);
        apply(
            &mut frame,
            &effect(
                "chroma-key",
                serde_json::json!({ "color": "#00ff00", "similarity": 0.2, "blend": 0.05 }),
            ),
            &fixture.cx(),
        )
        .expect("key applies");
        assert_eq!(frame.size(), [100, 100]);
        assert_eq!(frame.pixel(10, 50), [0.0; 4], "the screen must be gone");
        let kept = frame.pixel(90, 50);
        assert_eq!(kept[3], 1.0, "the subject must stay opaque: {kept:?}");
        assert!(
            kept[0] > 0.5,
            "the subject's colour was destroyed by spill suppression: {kept:?}"
        );
    }

    #[test]
    fn chroma_key_refuses_a_neutral_key_colour() {
        let fixture = fixture();
        let mut frame = solid(4, 4, [0.5, 0.5, 0.5]);
        let error = apply(
            &mut frame,
            &effect("chroma-key", serde_json::json!({ "color": "#808080" })),
            &fixture.cx(),
        )
        .expect_err("a grey key is meaningless");
        assert_eq!(error.kind(), "bad-args");
        assert!(error.to_string().contains("chroma"), "{error}");
    }

    #[test]
    fn stabilize_without_analysis_names_the_analysis_op() {
        let fixture = fixture();
        let mut frame = solid(100, 100, [0.4, 0.4, 0.4]);
        let untouched = frame.clone();
        let error = apply(
            &mut frame,
            &effect("stabilize", serde_json::json!({ "smoothing": 10 })),
            &fixture.cx(),
        )
        .expect_err("no .trf means no stabilization");
        let message = error.to_string();
        assert!(message.contains("fx.analyze"), "{message}");
        assert!(message.contains("stabilize"), "{message}");
        assert_eq!(
            frame, untouched,
            "a missing analysis must not pass the frame through silently"
        );
    }

    /// A `.trf` for a locked shot with one frame of `jump` pixels of jolt that comes back
    /// on the next frame — the shake a stabilizer must remove, as opposed to the steady
    /// move it must keep.
    fn jolt_trf(frames: usize, at: usize, jump: i32) -> String {
        let mut text = String::from("VID.STAB 1\n#      accuracy = 15\n");
        for index in 0..frames {
            // Reported vectors map a frame back onto the previous one, so a content jump
            // of +jump is written as -jump, and the return as +jump.
            let step = if index == at {
                -jump
            } else if index == at + 1 {
                jump
            } else {
                0
            };
            if step == 0 {
                text.push_str(&format!("Frame {} (List 0 [])\n", index + 1));
                continue;
            }
            text.push_str(&format!(
                "Frame {} (List 2 [(LM {step} 0 20 20 32 0.5 0.1),(LM {step} 0 80 80 32 0.5 0.1)])\n",
                index + 1
            ));
        }
        text
    }

    #[test]
    fn trf_parsing_reads_vectors_and_refuses_the_binary_format() {
        let frames = parse_trf(
            "VID.STAB 1\n#      accuracy = 15\n\nFrame 1 (List 0 [])\n\
             Frame 2 (List 2 [(LM 4 -12 230 100 32 0.276942 0.691406),(LM 6 -8 50 100 16 0.28 0.0)])\n",
        )
        .expect("ascii trf parses");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].frame, 1);
        assert!(frames[0].motions.is_empty());
        assert_eq!(frames[1].motions.len(), 2);
        assert_eq!(frames[1].motions[0].offset, [4, -12]);
        assert_eq!(frames[1].motions[0].at, [230, 100]);
        assert_eq!(frames[1].motions[1].size, 16);

        let error = parse_trf("TRF1\u{1}\u{0}binary garbage").expect_err("binary is refused");
        assert!(error.to_string().contains("ASCII"), "{error}");
        assert!(error.to_string().contains("fileformat=ascii"), "{error}");
    }

    #[test]
    fn a_steady_pan_is_kept_and_a_jolt_is_removed() {
        let steady: Vec<FrameMotion> = (0..9)
            .map(|index| FrameMotion {
                frame: index + 1,
                motions: vec![LocalMotion {
                    at: [20, 20],
                    offset: [-6, 0],
                    size: 32,
                    contrast: 0.5,
                    match_error: 0.0,
                }],
            })
            .collect();
        let corrections = solve_corrections(&steady, 2);
        assert!(
            corrections[4].dx.abs() < 0.001,
            "a deliberate pan must survive smoothing: {:?}",
            corrections[4]
        );

        let jolted = parse_trf(&jolt_trf(11, 5, 10)).expect("trf parses");
        let corrections = solve_corrections(&jolted, 2);
        assert!(
            corrections[5].dx < -6.0,
            "a +10 px jolt should be pulled back, got {:?}",
            corrections[5]
        );
    }

    #[test]
    fn stabilize_shifts_the_frame_against_the_measured_jolt() {
        let fixture = fixture();
        let path = stabilize_cache_path(&fixture.paths, &fixture.clip.id);
        std::fs::create_dir_all(path.parent().expect("cache parent")).expect("cache dir");
        std::fs::write(&path, jolt_trf(11, 5, 10)).expect("write trf");

        let mut frame = Frame::transparent(100, 100);
        frame.set_pixel(60, 50, [1.0, 1.0, 1.0, 1.0]);
        // Timeline 5/30 s is source frame 5, which is the file's `Frame 6`.
        let at = Time::from_frames(5, Fps::new(30, 1).expect("30 fps"));
        apply(
            &mut frame,
            &effect(
                "stabilize",
                serde_json::json!({ "smoothing": 2, "zoom": 0.0 }),
            ),
            &fixture.at(at),
        )
        .expect("stabilize applies");
        assert_eq!(frame.size(), [100, 100]);

        let mut weight = 0.0f64;
        let mut moment = 0.0f64;
        for y in 0..100 {
            for x in 0..100 {
                let alpha = f64::from(frame.pixel(x, y)[3]);
                weight += alpha;
                moment += alpha * f64::from(x);
            }
        }
        assert!(weight > 0.9, "the content vanished: {weight}");
        let centroid = moment / weight;
        // The content jumped +10 px right at this frame; the correction pulls it ~8 px back
        // (the remainder is the smoothed path). A flipped sign would push it to ~68.
        assert!(
            (50.0..55.0).contains(&centroid),
            "stabilized content sits at {centroid}, expected about 52"
        );
    }

    #[test]
    fn stabilize_analysis_measures_a_known_pan() {
        let fixture = fixture();
        let tool = Toolchain::shared().expect("ffmpeg is installed for the test suite");
        let media = fixture.dir.path().join("pan.mp4");
        let status = tool
            .ffmpeg_command()
            .args(["-f", "lavfi", "-i", "testsrc=size=420x340:rate=10:duration=1"])
            // The crop window walks left, so the content walks right by 8 px per frame.
            .args(["-vf", "crop=300:240:x=60-8*n:y=60"])
            .args(["-c:v", "libx264", "-qp", "0", "-pix_fmt", "yuv420p"])
            .arg(&media)
            .status()
            .expect("ffmpeg runs");
        assert!(status.success(), "could not synthesize the pan");

        let out = stabilize_cache_path(&fixture.paths, &fixture.clip.id);
        analyze_stabilization(&fixture.cx(), &media, &out, 10).expect("analysis runs");
        let text = std::fs::read_to_string(&out).expect("read trf");
        let frames = parse_trf(&text).expect("the analysis wrote ascii transforms");
        assert!(frames.len() > 5, "only {} frames measured", frames.len());

        let mut measured: Vec<f32> = frames
            .iter()
            .skip(1)
            .map(|frame| frame_step(&frame.motions)[0])
            .collect();
        let typical = median(&mut measured);
        assert!(
            typical > 4.0,
            "a +8 px/frame pan measured as {typical}; the sign convention is inverted"
        );
    }

    #[test]
    fn an_unknown_parameter_names_the_accepted_set() {
        let fixture = fixture();
        let mut frame = solid(4, 4, [0.5, 0.5, 0.5]);
        let error = apply(
            &mut frame,
            &effect("blur", serde_json::json!({ "radiuss": 4 })),
            &fixture.cx(),
        )
        .expect_err("a typo must not be ignored");
        assert_eq!(error.kind(), "bad-args");
        let message = error.to_string();
        assert!(message.contains("radiuss"), "{message}");
        assert!(message.contains("accepted: radius"), "{message}");
    }

    #[test]
    fn a_non_numeric_parameter_is_rejected_rather_than_defaulted() {
        let fixture = fixture();
        let mut frame = solid(4, 4, [0.5, 0.5, 0.5]);
        let error = apply(
            &mut frame,
            &effect("blur", serde_json::json!({ "radius": "four" })),
            &fixture.cx(),
        )
        .expect_err("'four' is not a radius");
        assert_eq!(error.kind(), "bad-args");
        assert!(error.to_string().contains("four"), "{error}");
    }

    #[test]
    fn an_unknown_kind_lists_the_catalog() {
        let fixture = fixture();
        let mut frame = solid(4, 4, [0.5, 0.5, 0.5]);
        let error = apply(
            &mut frame,
            &effect("color.glow", serde_json::json!({})),
            &fixture.cx(),
        )
        .expect_err("the catalog is closed");
        assert_eq!(error.kind(), "op");
        let message = error.to_string();
        for kind in CATALOG {
            assert!(message.contains(kind), "{kind} missing from: {message}");
        }
    }

    #[test]
    fn every_file_free_kind_preserves_the_frame_size() {
        let mut fixture = fixture();
        fixture.sequence_size = [37, 23];
        for (kind, params) in [
            ("color.grade", serde_json::json!({ "exposure": 0.5, "contrast": 1.2, "temperature": 0.5, "saturation": 1.3, "lift": 0.01, "gamma": [1.1, 1.0, 0.9], "gain": 1.05 })),
            ("blur", serde_json::json!({ "radius": 3 })),
            ("sharpen", serde_json::json!({ "amount": 0.8, "radius": 2 })),
            ("crop", serde_json::json!({ "left": 0.1, "bottom": 0.1 })),
            ("mask.shape", serde_json::json!({ "shape": "ellipse", "feather": 0.2 })),
            ("chroma-key", serde_json::json!({ "color": "#00ff00" })),
        ] {
            let mut frame = solid(37, 23, [0.3, 0.45, 0.2]);
            apply(&mut frame, &effect(kind, params), &fixture.cx())
                .unwrap_or_else(|error| panic!("{kind} failed: {error}"));
            assert_eq!(frame.size(), [37, 23], "{kind} resized the frame");
            assert_eq!(
                frame.pixels().len(),
                37 * 23 * 4,
                "{kind} left the buffer inconsistent with its size"
            );
        }
    }

    #[test]
    fn apply_all_runs_enabled_effects_in_order() {
        let mut fixture = fixture();
        let mut doubling = effect("color.grade", serde_json::json!({ "exposure": 1 }));
        doubling.enabled = true;
        let mut disabled = effect("color.grade", serde_json::json!({ "exposure": 1 }));
        disabled.enabled = false;
        fixture.clip.effects = vec![doubling, disabled];
        let mut frame = solid(2, 2, [0.1, 0.1, 0.1]);
        apply_all(&mut frame, &fixture.cx()).expect("chain applies");
        assert_eq!(
            frame.pixel(0, 0)[0], 0.2,
            "the disabled effect must not run"
        );
    }

    #[test]
    fn a_keyframed_radius_is_read_at_the_render_instant() {
        use dvs_core::project::{Easing, Keyframe};
        let mut fixture = fixture();
        fixture.sequence_size = [64, 64];
        let mut blur = effect("blur", serde_json::json!({ "radius": 0 }));
        blur.id = dvs_core::ids::EffectId::from_raw("fx_blur");
        fixture.clip.keyframes.insert(
            "fx.fx_blur.radius".to_string(),
            vec![
                Keyframe {
                    at: Time::ZERO,
                    value: 0.0,
                    easing: Easing::Linear,
                },
                Keyframe {
                    at: Time::from_secs(1),
                    value: 8.0,
                    easing: Easing::Linear,
                },
            ],
        );

        let mut sharp = Frame::transparent(64, 64);
        sharp.set_pixel(32, 32, [1.0, 1.0, 1.0, 1.0]);
        let mut soft = sharp.clone();
        apply(&mut sharp, &blur, &fixture.at(Time::ZERO)).expect("blur applies");
        apply(&mut soft, &blur, &fixture.at(Time::from_secs(1))).expect("blur applies");
        assert_eq!(
            sharp.pixel(32, 32)[3], 1.0,
            "the keyframe at t=0 is a radius of 0"
        );
        assert!(
            soft.pixel(32, 32)[3] < 0.05,
            "the keyframe at t=1 s is a radius of 8: {:?}",
            soft.pixel(32, 32)
        );
    }
}
