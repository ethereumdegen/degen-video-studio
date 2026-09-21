//! Time is exact.
//!
//! Every position and duration in a project is a rational number of seconds, and every frame
//! rate is a rational number of frames per second, so `30000/1001` is not an approximation of
//! anything — it is the value. Floats never enter the canonical document: `0.1 + 0.2` drifting
//! by one ULP is invisible until frame 26970 lands on the wrong side of a cut.
//!
//! Agents speak human time (`00:01:12.5`, `1m12s`, `90s`, `1800f`); [`Time::parse`] and
//! [`Time::parse_with_fps`] convert, and ops snap the result to the sequence frame grid and
//! report the snap. The serialized form is always `num/den`.

use crate::error::{Error, Result};
use num_integer::Integer;
use num_rational::Ratio;
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt;
use std::ops::{Add, AddAssign, Div, Mul, Neg, Sub};

/// The rational type behind every time value. `i64` numerators leave room for
/// `seconds * 1001 * 48000` without overflow for any realistic timeline length.
pub type R = Ratio<i64>;

fn parse_ratio(text: &str) -> Option<R> {
    let (num, den) = text.split_once('/')?;
    let num: i64 = num.trim().parse().ok()?;
    let den: i64 = den.trim().parse().ok()?;
    if den == 0 {
        return None;
    }
    Some(R::new(num, den))
}

/// Exact decimal → rational. `"12.5"` becomes `25/2`, not `12.5f64`.
fn parse_decimal(text: &str) -> Option<R> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None => (1i64, text.strip_prefix('+').unwrap_or(text)),
    };
    let (int_part, frac_part) = match digits.split_once('.') {
        Some((i, f)) => (i, f),
        None => (digits, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit()) || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let int: i64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().ok()?
    };
    if frac_part.is_empty() {
        return Some(R::from_integer(sign * int));
    }
    let den = 10i64.checked_pow(u32::try_from(frac_part.len()).ok()?)?;
    let frac: i64 = frac_part.parse().ok()?;
    Some(R::new(sign * (int * den + frac), den))
}

/// A duration or position in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Time(R);

impl Time {
    pub const ZERO: Time = Time(R::new_raw(0, 1));

    pub const fn from_ratio(value: R) -> Self {
        Time(value)
    }

    pub fn from_secs(secs: i64) -> Self {
        Time(R::from_integer(secs))
    }

    pub fn new(num: i64, den: i64) -> Result<Self> {
        if den == 0 {
            return Err(Error::bad_args("time denominator is zero"));
        }
        Ok(Time(R::new(num, den)))
    }

    /// Only for reporting and for talking to APIs that are themselves float-based
    /// (ffmpeg arguments, audio sample math). Never for canonical storage.
    pub fn as_secs_f64(self) -> f64 {
        *self.0.numer() as f64 / *self.0.denom() as f64
    }

    pub fn ratio(self) -> R {
        self.0
    }

    pub fn from_frames(frames: i64, fps: Fps) -> Self {
        Time(R::from_integer(frames) / fps.ratio())
    }

    /// Frame index containing this instant. Floor, because frame *n* covers `[n/fps,
    /// (n+1)/fps)` — the frame you see at `t` is the one that started at or before `t`.
    pub fn frame_floor(self, fps: Fps) -> i64 {
        (self.0 * fps.ratio()).floor().to_integer()
    }

    /// Nearest frame boundary. Used when snapping an agent's request to the grid.
    pub fn frame_round(self, fps: Fps) -> i64 {
        (self.0 * fps.ratio()).round().to_integer()
    }

    /// Frame count of a duration: ceil, so 1.5 frames of content occupies 2 frames.
    pub fn frame_ceil(self, fps: Fps) -> i64 {
        (self.0 * fps.ratio()).ceil().to_integer()
    }

    pub fn snap(self, fps: Fps) -> Self {
        Time::from_frames(self.frame_round(fps), fps)
    }

    pub fn is_frame_aligned(self, fps: Fps) -> bool {
        (self.0 * fps.ratio()).is_integer()
    }

    pub fn from_samples(samples: i64, rate: u32) -> Self {
        Time(R::new(samples, i64::from(rate)))
    }

    pub fn sample_round(self, rate: u32) -> i64 {
        (self.0 * R::from_integer(i64::from(rate))).round().to_integer()
    }

    pub fn sample_floor(self, rate: u32) -> i64 {
        (self.0 * R::from_integer(i64::from(rate))).floor().to_integer()
    }

    pub fn is_zero(self) -> bool {
        self.0 == R::from_integer(0)
    }

    pub fn is_negative(self) -> bool {
        self.0 < R::from_integer(0)
    }

    pub fn is_positive(self) -> bool {
        self.0 > R::from_integer(0)
    }

    pub fn min(self, other: Self) -> Self {
        if self.0 <= other.0 {
            self
        } else {
            other
        }
    }

    pub fn max(self, other: Self) -> Self {
        if self.0 >= other.0 {
            self
        } else {
            other
        }
    }

    pub fn abs(self) -> Self {
        Time(if self.0 < R::from_integer(0) {
            -self.0
        } else {
            self.0
        })
    }

    /// `HH:MM:SS:FF` non-drop timecode. Frames are truncated toward the start of the
    /// second, matching how every NLE displays a position.
    pub fn timecode(self, fps: Fps) -> String {
        let negative = self.is_negative();
        let total_frames = self.abs().frame_round(fps);
        let fps_int = fps.nominal_int().max(1);
        let frames = total_frames % i64::from(fps_int);
        let total_secs = total_frames / i64::from(fps_int);
        let secs = total_secs % 60;
        let mins = (total_secs / 60) % 60;
        let hours = total_secs / 3600;
        format!(
            "{}{hours:02}:{mins:02}:{secs:02}:{frames:02}",
            if negative { "-" } else { "" }
        )
    }

    /// `HH:MM:SS.mmm`, for SRT/VTT and for human-readable digests.
    pub fn clock(self) -> String {
        let negative = self.is_negative();
        let abs = self.abs();
        let millis_total = (abs.0 * R::from_integer(1000)).round().to_integer();
        let millis = millis_total % 1000;
        let total_secs = millis_total / 1000;
        format!(
            "{}{:02}:{:02}:{:02}.{:03}",
            if negative { "-" } else { "" },
            total_secs / 3600,
            (total_secs / 60) % 60,
            total_secs % 60,
            millis
        )
    }

    /// Accepts, in this order: `num/den`, `HH:MM:SS(.mmm)` / `MM:SS(.mmm)`, a unit-suffixed
    /// duration (`90s`, `1m12s`, `500ms`, `1h2m3s`), or plain decimal seconds.
    pub fn parse(text: &str) -> Result<Self> {
        Self::parse_inner(text, None)
    }

    /// As [`Time::parse`], plus frame counts (`1800f`) and four-part timecode
    /// (`HH:MM:SS:FF`), both of which need a frame rate to mean anything.
    pub fn parse_with_fps(text: &str, fps: Fps) -> Result<Self> {
        Self::parse_inner(text, Some(fps))
    }

    fn parse_inner(text: &str, fps: Option<Fps>) -> Result<Self> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(Error::bad_args("empty time value"));
        }
        if let Some(ratio) = parse_ratio(trimmed) {
            return Ok(Time(ratio));
        }
        if let Some(frames) = trimmed.strip_suffix('f').or_else(|| trimmed.strip_suffix('F')) {
            let fps = fps.ok_or_else(|| {
                Error::bad_args(format!("'{trimmed}' is a frame count but no frame rate is known"))
            })?;
            let frames: i64 = frames
                .trim()
                .parse()
                .map_err(|_| Error::bad_args(format!("bad frame count '{trimmed}'")))?;
            return Ok(Time::from_frames(frames, fps));
        }
        if trimmed.contains(':') {
            return Self::parse_colon(trimmed, fps);
        }
        if let Some(units) = Self::parse_units(trimmed)? {
            return Ok(units);
        }
        parse_decimal(trimmed)
            .map(Time)
            .ok_or_else(|| Error::bad_args(format!("cannot parse time '{text}'")))
    }

    fn parse_colon(text: &str, fps: Option<Fps>) -> Result<Self> {
        let (sign, body) = match text.strip_prefix('-') {
            Some(rest) => (-1, rest),
            None => (1, text),
        };
        let parts: Vec<&str> = body.split(':').collect();
        let bad = || Error::bad_args(format!("cannot parse timecode '{text}'"));
        let mut total = match parts.len() {
            2 => {
                let mins: i64 = parts[0].parse().map_err(|_| bad())?;
                let secs = parse_decimal(parts[1]).ok_or_else(bad)?;
                R::from_integer(mins * 60) + secs
            }
            3 => {
                let hours: i64 = parts[0].parse().map_err(|_| bad())?;
                let mins: i64 = parts[1].parse().map_err(|_| bad())?;
                let secs = parse_decimal(parts[2]).ok_or_else(bad)?;
                R::from_integer(hours * 3600 + mins * 60) + secs
            }
            4 => {
                // HH:MM:SS:FF — non-drop timecode. The fields are a *frame count* in
                // disguise, not wall-clock seconds: at 30000/1001 the label 00:01:00:00 is
                // frame 1800, which arrives 60.06 real seconds in. Inverting the label the
                // naive way (seconds + frames/fps) drifts by 0.1% — 99 frames over an hour.
                let fps = fps.ok_or_else(|| {
                    Error::bad_args(format!(
                        "'{text}' is frame timecode but no frame rate is known"
                    ))
                })?;
                let hours: i64 = parts[0].parse().map_err(|_| bad())?;
                let mins: i64 = parts[1].parse().map_err(|_| bad())?;
                let secs: i64 = parts[2].parse().map_err(|_| bad())?;
                let frames: i64 = parts[3].parse().map_err(|_| bad())?;
                let nominal = i64::from(fps.nominal_int().max(1));
                let total_frames = (hours * 3600 + mins * 60 + secs) * nominal + frames;
                R::from_integer(total_frames) / fps.ratio()
            }
            _ => return Err(bad()),
        };
        if sign < 0 {
            total = -total;
        }
        Ok(Time(total))
    }

    /// `1h2m3.5s`, `500ms`, `90s`. Returns `None` when the text carries no unit at all,
    /// so the caller can fall back to plain seconds.
    fn parse_units(text: &str) -> Result<Option<Self>> {
        if !text
            .bytes()
            .any(|b| matches!(b, b'h' | b'm' | b's' | b'H' | b'M' | b'S'))
        {
            return Ok(None);
        }
        let lower = text.to_ascii_lowercase();
        let bytes = lower.as_bytes();
        let mut total = R::from_integer(0);
        let mut idx = 0usize;
        let mut saw_unit = false;
        let negative = bytes.first() == Some(&b'-');
        if negative {
            idx = 1;
        }
        while idx < bytes.len() {
            let start = idx;
            while idx < bytes.len() && (bytes[idx].is_ascii_digit() || bytes[idx] == b'.') {
                idx += 1;
            }
            if start == idx {
                return Err(Error::bad_args(format!("cannot parse duration '{text}'")));
            }
            let value = parse_decimal(&lower[start..idx])
                .ok_or_else(|| Error::bad_args(format!("cannot parse duration '{text}'")))?;
            let unit_start = idx;
            while idx < bytes.len() && bytes[idx].is_ascii_alphabetic() {
                idx += 1;
            }
            let unit = &lower[unit_start..idx];
            let scale = match unit {
                "h" => R::from_integer(3600),
                "m" => R::from_integer(60),
                "s" => R::from_integer(1),
                "ms" => R::new(1, 1000),
                "us" => R::new(1, 1_000_000),
                "" => return Err(Error::bad_args(format!("missing unit in '{text}'"))),
                other => {
                    return Err(Error::bad_args(format!(
                        "unknown time unit '{other}' in '{text}'"
                    )))
                }
            };
            total += value * scale;
            saw_unit = true;
        }
        if !saw_unit {
            return Ok(None);
        }
        Ok(Some(Time(if negative { -total } else { total })))
    }
}

impl PartialOrd for Time {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Time {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

impl Add for Time {
    type Output = Time;
    fn add(self, rhs: Time) -> Time {
        Time(self.0 + rhs.0)
    }
}

impl AddAssign for Time {
    fn add_assign(&mut self, rhs: Time) {
        self.0 += rhs.0;
    }
}

impl Sub for Time {
    type Output = Time;
    fn sub(self, rhs: Time) -> Time {
        Time(self.0 - rhs.0)
    }
}

impl Neg for Time {
    type Output = Time;
    fn neg(self) -> Time {
        Time(-self.0)
    }
}

impl Mul<Rat> for Time {
    type Output = Time;
    fn mul(self, rhs: Rat) -> Time {
        Time(self.0 * rhs.0)
    }
}

impl Div<Rat> for Time {
    type Output = Time;
    fn div(self, rhs: Rat) -> Time {
        Time(self.0 / rhs.0)
    }
}

impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.0.numer(), self.0.denom())
    }
}

impl Serialize for Time {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Time {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        match Flexible::deserialize(d)? {
            Flexible::Text(text) => Time::parse(&text).map_err(de::Error::custom),
            Flexible::Number(value) => parse_decimal(&value.to_string())
                .map(Time)
                .ok_or_else(|| de::Error::custom(format!("bad time {value}"))),
        }
    }
}

impl schemars::JsonSchema for Time {
    fn schema_name() -> Cow<'static, str> {
        "Time".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "seconds as an exact rational 'num/den'; also accepts '12.5', '1m12s', '00:01:12.500'",
            "examples": ["0/1", "1274/30", "00:01:12.500"]
        })
    }
}

/// A non-time rational: clip speed, aspect correction. Same string form as [`Time`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rat(R);

impl Rat {
    pub const ONE: Rat = Rat(R::new_raw(1, 1));

    pub const fn from_ratio(value: R) -> Self {
        Rat(value)
    }

    pub fn new(num: i64, den: i64) -> Result<Self> {
        if den == 0 {
            return Err(Error::bad_args("rational denominator is zero"));
        }
        Ok(Rat(R::new(num, den)))
    }

    pub fn ratio(self) -> R {
        self.0
    }

    pub fn as_f64(self) -> f64 {
        *self.0.numer() as f64 / *self.0.denom() as f64
    }

    pub fn is_one(self) -> bool {
        self.0 == R::from_integer(1)
    }

    pub fn recip(self) -> Self {
        Rat(self.0.recip())
    }

    pub fn parse(text: &str) -> Result<Self> {
        let trimmed = text.trim();
        if let Some(ratio) = parse_ratio(trimmed) {
            return Ok(Rat(ratio));
        }
        parse_decimal(trimmed)
            .map(Rat)
            .ok_or_else(|| Error::bad_args(format!("cannot parse rational '{text}'")))
    }
}

impl Default for Rat {
    fn default() -> Self {
        Rat::ONE
    }
}

impl fmt::Display for Rat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.0.numer(), self.0.denom())
    }
}

impl Serialize for Rat {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Rat {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        match Flexible::deserialize(d)? {
            Flexible::Text(text) => Rat::parse(&text).map_err(de::Error::custom),
            Flexible::Number(value) => parse_decimal(&value.to_string())
                .map(Rat)
                .ok_or_else(|| de::Error::custom(format!("bad rational {value}"))),
        }
    }
}

impl schemars::JsonSchema for Rat {
    fn schema_name() -> Cow<'static, str> {
        "Rational".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "exact rational 'num/den'; also accepts a decimal",
            "examples": ["1/1", "1/2", "2.5"]
        })
    }
}

/// Frames per second, exact. `30000/1001`, never `29.97`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fps(R);

impl Fps {
    pub fn new(num: i64, den: i64) -> Result<Self> {
        if den <= 0 || num <= 0 {
            return Err(Error::bad_args(format!("bad frame rate {num}/{den}")));
        }
        Ok(Fps(R::new(num, den)))
    }

    pub const fn from_ratio_unchecked(value: R) -> Self {
        Fps(value)
    }

    pub fn ratio(self) -> R {
        self.0
    }

    pub fn as_f64(self) -> f64 {
        *self.0.numer() as f64 / *self.0.denom() as f64
    }

    pub fn frame_duration(self) -> Time {
        Time(self.0.recip())
    }

    /// The integer the industry names the rate by: 30 for `30000/1001`, 24 for `24000/1001`.
    /// Used for timecode frame fields only.
    pub fn nominal_int(self) -> u32 {
        self.0.ceil().to_integer().try_into().unwrap_or(1)
    }

    /// Accepts `30`, `30000/1001`, and the decimal shorthands the broadcast world uses —
    /// `29.97` means exactly `30000/1001`, not `2997/100`. Getting this wrong puts a
    /// ten-minute timeline 18 frames out of sync, so the mapping is explicit.
    pub fn parse(text: &str) -> Result<Self> {
        let trimmed = text.trim();
        if let Some(ratio) = parse_ratio(trimmed) {
            return Fps::new(*ratio.numer(), *ratio.denom());
        }
        let ndf = match trimmed {
            "23.976" | "23.98" => Some((24000, 1001)),
            "29.97" => Some((30000, 1001)),
            "47.952" | "47.95" => Some((48000, 1001)),
            "59.94" => Some((60000, 1001)),
            "119.88" => Some((120000, 1001)),
            _ => None,
        };
        if let Some((num, den)) = ndf {
            return Fps::new(num, den);
        }
        let decimal = parse_decimal(trimmed)
            .ok_or_else(|| Error::bad_args(format!("cannot parse frame rate '{text}'")))?;
        Fps::new(*decimal.numer(), *decimal.denom())
    }

    /// `-r` value for ffmpeg: exact, so no resampling happens behind our back.
    pub fn ffmpeg_arg(self) -> String {
        format!("{}/{}", self.0.numer(), self.0.denom())
    }

    pub fn is_ndf(self) -> bool {
        *self.0.denom() == 1001
    }
}

impl Default for Fps {
    fn default() -> Self {
        Fps(R::new_raw(30, 1))
    }
}

impl fmt::Display for Fps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if *self.0.denom() == 1 {
            write!(f, "{}", self.0.numer())
        } else {
            write!(f, "{}/{}", self.0.numer(), self.0.denom())
        }
    }
}

impl Serialize for Fps {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Fps {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        match Flexible::deserialize(d)? {
            Flexible::Text(text) => Fps::parse(&text).map_err(de::Error::custom),
            Flexible::Number(value) => Fps::parse(&value.to_string()).map_err(de::Error::custom),
        }
    }
}

impl schemars::JsonSchema for Fps {
    fn schema_name() -> Cow<'static, str> {
        "Fps".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "exact frame rate; '30', '30000/1001', or the shorthand '29.97'",
            "examples": ["30", "30000/1001", "24"]
        })
    }
}

/// A half-open span `[start, end)` on the timeline. Half-open is what makes adjacent clips
/// adjacent instead of overlapping by one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Span {
    pub start: Time,
    pub end: Time,
}

impl Span {
    pub fn new(start: Time, end: Time) -> Self {
        Span { start, end }
    }

    pub fn from_duration(start: Time, duration: Time) -> Self {
        Span {
            start,
            end: start + duration,
        }
    }

    pub fn duration(&self) -> Time {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    pub fn contains(&self, at: Time) -> bool {
        at >= self.start && at < self.end
    }

    pub fn overlaps(&self, other: &Span) -> bool {
        self.start < other.end && other.start < self.end
    }

    pub fn intersect(&self, other: &Span) -> Option<Span> {
        let span = Span::new(self.start.max(other.start), self.end.min(other.end));
        (!span.is_empty()).then_some(span)
    }

    pub fn shifted(&self, by: Time) -> Span {
        Span::new(self.start + by, self.end + by)
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}, {})", self.start, self.end)
    }
}

/// JSON accepts either a string or a number for every time-like field; agents write both.
#[derive(Deserialize)]
#[serde(untagged)]
enum Flexible {
    Text(String),
    Number(serde_json::Number),
}

/// Greatest common divisor helper used by the interop writers, which need integer
/// timebases rather than rationals.
pub fn reduce(num: i64, den: i64) -> (i64, i64) {
    if den == 0 {
        return (num, den);
    }
    let g = num.gcd(&den).max(1);
    (num / g, den / g)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fps(num: i64, den: i64) -> Fps {
        Fps::new(num, den).unwrap()
    }

    #[test]
    fn ndf_rate_is_exact_over_a_long_timeline() {
        let rate = fps(30000, 1001);
        // One hour of 29.97 is 107892 frames, and the boundary is exact, not 3599.64 seconds.
        let hour = Time::from_secs(3600);
        assert_eq!(hour.frame_round(rate), 107_892);
        let frame = Time::from_frames(107_892, rate);
        assert_eq!(frame.to_string(), "8999991/2500");
        assert!(frame.is_frame_aligned(rate));
    }

    #[test]
    fn snapping_reports_an_exact_grid_value() {
        let rate = fps(30000, 1001);
        let requested = Time::parse("42.5").unwrap();
        let snapped = requested.snap(rate);
        assert!(snapped.is_frame_aligned(rate));
        assert_eq!(snapped.frame_round(rate), 1274);
        assert_ne!(requested, snapped);
    }

    #[test]
    fn parses_every_agent_spelling_of_the_same_instant() {
        let rate = fps(30, 1);
        let expected = Time::new(145, 2).unwrap(); // 72.5s
        for text in ["145/2", "72.5", "1m12.5s", "01:12.5", "00:01:12.500"] {
            assert_eq!(Time::parse(text).unwrap(), expected, "parsing {text}");
        }
        assert_eq!(
            Time::parse_with_fps("00:01:12:15", rate).unwrap(),
            expected,
            "frame timecode"
        );
        assert_eq!(
            Time::parse_with_fps("2175f", rate).unwrap(),
            expected,
            "frame count"
        );
    }

    #[test]
    fn frame_forms_need_a_frame_rate() {
        assert!(Time::parse("1800f").is_err());
        assert!(Time::parse("00:00:01:12").is_err());
    }

    #[test]
    fn decimal_frame_rates_map_to_broadcast_rationals() {
        assert_eq!(Fps::parse("29.97").unwrap(), fps(30000, 1001));
        assert_eq!(Fps::parse("23.976").unwrap(), fps(24000, 1001));
        assert_eq!(Fps::parse("30").unwrap(), fps(30, 1));
        assert_eq!(Fps::parse("30000/1001").unwrap(), fps(30000, 1001));
        assert!(Fps::parse("0").is_err());
    }

    #[test]
    fn timecode_round_trips_through_parsing() {
        let rate = fps(30000, 1001);
        let time = Time::from_frames(98_765, rate);
        let text = time.timecode(rate);
        assert_eq!(text, "00:54:52:05");
        let back = Time::parse_with_fps(&text, rate).unwrap();
        // Non-drop timecode is a label, not an instant: it drifts from wall clock by design,
        // so the round trip lands on the same frame index, which is what editing needs.
        assert_eq!(back.frame_round(rate), 98_765);
    }

    #[test]
    fn spans_are_half_open() {
        let a = Span::new(Time::from_secs(0), Time::from_secs(2));
        let b = Span::new(Time::from_secs(2), Time::from_secs(4));
        assert!(!a.overlaps(&b));
        assert!(a.contains(Time::parse("1.999").unwrap()));
        assert!(!a.contains(Time::from_secs(2)));
        assert_eq!(a.intersect(&b), None);
        assert_eq!(
            a.intersect(&Span::new(Time::parse("1").unwrap(), Time::from_secs(3))),
            Some(Span::new(Time::from_secs(1), Time::from_secs(2)))
        );
    }

    #[test]
    fn serialized_form_is_the_exact_rational() {
        let time = Time::parse("1/3").unwrap();
        let json = serde_json::to_string(&time).unwrap();
        assert_eq!(json, "\"1/3\"");
        assert_eq!(serde_json::from_str::<Time>(&json).unwrap(), time);
        assert_eq!(serde_json::from_str::<Time>("12.5").unwrap(), Time::new(25, 2).unwrap());
    }

    #[test]
    fn audio_positions_are_sample_exact() {
        let time = Time::from_frames(1, fps(30000, 1001));
        assert_eq!(time.sample_round(48_000), 1602);
        assert_eq!(Time::from_samples(1602, 48_000).sample_round(48_000), 1602);
    }
}
