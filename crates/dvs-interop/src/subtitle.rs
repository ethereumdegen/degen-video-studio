//! SubRip (`.srt`) and WebVTT (`.vtt`).
//!
//! These two files are how captions leave the project for YouTube, a player, or a human
//! with a text editor, and how somebody else's captions come back in. The writers are
//! trivial; the readers are the point of this module.
//!
//! Caption files in the wild are malformed in a small, well-known set of ways: a UTF-8
//! BOM in front of the first index, CRLF line endings, a missing index line, two blank
//! lines between cues or none at all, WebVTT `NOTE`/`STYLE` blocks, cue settings trailing
//! the timestamp line, and cues that are simply out of chronological order because they
//! were appended by hand. A strict parser rejects the file; the caption text is perfectly
//! recoverable in every one of those cases, and refusing it would send an agent off to
//! debug a text file instead of editing video. So the reader absorbs all of them and
//! returns cues sorted by start time, and fails only when a timestamp genuinely cannot be
//! read — which is a real data loss and must not be silently skipped.

use dvs_core::error::{Error, Result};
use dvs_core::ids::CueId;
use dvs_core::project::CaptionCue;
use dvs_core::time::{Span, Time};

/// Serialize cues as SubRip: 1-based indices and `HH:MM:SS,mmm` timestamps.
///
/// Cues are emitted in chronological order regardless of the order they arrive in, and
/// blank lines inside a cue's text are dropped because a blank line is the format's cue
/// separator and cannot be escaped.
pub fn to_srt(cues: &[CaptionCue]) -> String {
    write_cues(cues, ',', None)
}

/// Serialize cues as WebVTT: a `WEBVTT` header and `HH:MM:SS.mmm` timestamps.
pub fn to_vtt(cues: &[CaptionCue]) -> String {
    write_cues(cues, '.', Some("WEBVTT"))
}

/// Parse SubRip. Tolerates a BOM, CRLF, missing indices, blank-line noise and unordered
/// cues; the result is sorted by start time.
pub fn from_srt(text: &str) -> Result<Vec<CaptionCue>> {
    parse_cues(text, "SRT")
}

/// Parse WebVTT. As [`from_srt`], plus the `WEBVTT` header, `NOTE`/`STYLE`/`REGION`
/// blocks, cue identifiers and cue settings after the timestamp.
pub fn from_vtt(text: &str) -> Result<Vec<CaptionCue>> {
    parse_cues(text, "VTT")
}

/// `HH:MM:SS,mmm` / `HH:MM:SS.mmm`. Both formats agree on everything but the separator,
/// so one formatter serves both and a difference between them cannot creep in.
fn stamp(at: Time, decimal: char) -> String {
    let clock = at.max(Time::ZERO).clock();
    match decimal {
        '.' => clock,
        other => clock.replace('.', &other.to_string()),
    }
}

fn write_cues(cues: &[CaptionCue], decimal: char, header: Option<&str>) -> String {
    let mut order: Vec<&CaptionCue> = cues.iter().collect();
    order.sort_by(|a, b| {
        a.span
            .start
            .cmp(&b.span.start)
            .then(a.span.end.cmp(&b.span.end))
    });

    let mut out = String::with_capacity(cues.len() * 96);
    if let Some(header) = header {
        out.push_str(header);
        out.push_str("\n\n");
    }
    for (index, cue) in order.iter().enumerate() {
        out.push_str(&(index + 1).to_string());
        out.push('\n');
        out.push_str(&stamp(cue.span.start, decimal));
        out.push_str(" --> ");
        out.push_str(&stamp(cue.span.end, decimal));
        out.push('\n');
        for line in cue.text.lines().filter(|line| !line.trim().is_empty()) {
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// A line is a cue's timing line when it carries the arrow. Nothing else in either format
/// contains `-->`, which is what makes recovery from a missing index line possible.
fn is_timing(line: &str) -> bool {
    line.contains("-->")
}

/// `[HH:]MM:SS[.,]fff`, with any number of fraction digits. Exact: `01:00:00.001` is
/// `3600001/1000`, never a float that rounds to the previous millisecond.
fn parse_stamp(text: &str) -> Option<Time> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (whole, fraction) = match text.split_once(['.', ',']) {
        Some((whole, fraction)) => (whole, fraction),
        None => (text, ""),
    };
    let mut seconds: i64 = 0;
    let parts: Vec<&str> = whole.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    for part in &parts {
        let value: i64 = part.trim().parse().ok()?;
        if value < 0 {
            return None;
        }
        seconds = seconds.checked_mul(60)?.checked_add(value)?;
    }
    if fraction.is_empty() {
        return Time::new(seconds, 1).ok();
    }
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Keep the written precision instead of truncating to milliseconds: some generators
    // emit microseconds, and rounding them here would move a cue off the frame it was
    // aligned to.
    let digits = fraction.len().min(9);
    let scale = 10i64.pow(digits as u32);
    let fraction: i64 = fraction[..digits].parse().ok()?;
    Time::new(seconds.checked_mul(scale)? + fraction, scale).ok()
}

/// Split a timing line into its two instants, ignoring WebVTT cue settings
/// (`align:start position:50%`) that follow the end timestamp.
fn parse_timing(line: &str, format: &str, number: usize) -> Result<Span> {
    let (left, right) = line
        .split_once("-->")
        .ok_or_else(|| Error::op(format!("{format} line {number}: no '-->' in timing line")))?;
    let right = right.split_whitespace().next().unwrap_or("");
    let bad = |field: &str, text: &str| {
        Error::op(format!(
            "{format} line {number}: cannot read {field} timestamp '{text}'"
        ))
    };
    let start = parse_stamp(left).ok_or_else(|| bad("start", left.trim()))?;
    let end = parse_stamp(right).ok_or_else(|| bad("end", right))?;
    Ok(Span::new(start, end))
}

/// True for a bare cue index (`12`) that is immediately followed by a timing line. The
/// lookahead is what stops the next cue's index from being swallowed as the previous
/// cue's last line when the blank separator is missing.
fn is_index_before_timing(lines: &[&str], at: usize) -> bool {
    let line = lines[at].trim();
    !line.is_empty()
        && line.bytes().all(|b| b.is_ascii_digit())
        && lines.get(at + 1).is_some_and(|next| is_timing(next))
}

fn parse_cues(text: &str, format: &str) -> Result<Vec<CaptionCue>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    // `str::lines` already drops a trailing `\r`, so CRLF input needs no other handling.
    let lines: Vec<&str> = text.lines().collect();
    let mut cues: Vec<CaptionCue> = Vec::new();
    let mut index = 0usize;

    while index < lines.len() {
        let line = lines[index].trim();
        if line.is_empty() {
            index += 1;
            continue;
        }
        // WebVTT block headers introduce metadata, not a cue; skip to the blank line that
        // closes the block so their bodies cannot be read as caption text.
        let keyword = line.split_whitespace().next().unwrap_or("");
        if matches!(keyword, "WEBVTT" | "NOTE" | "STYLE" | "REGION") && !is_timing(line) {
            index += 1;
            while index < lines.len() && !lines[index].trim().is_empty() {
                index += 1;
            }
            continue;
        }
        if !is_timing(line) {
            // An index line, a cue identifier, or noise: the timing line decides.
            index += 1;
            continue;
        }

        let span = parse_timing(line, format, index + 1)?;
        index += 1;
        let mut body: Vec<&str> = Vec::new();
        while index < lines.len() {
            if lines[index].trim().is_empty()
                || is_timing(lines[index])
                || is_index_before_timing(&lines, index)
            {
                break;
            }
            body.push(lines[index].trim_end());
            index += 1;
        }
        cues.push(CaptionCue {
            id: CueId::new(),
            span,
            text: body.join("\n"),
            style: None,
        });
    }

    cues.sort_by(|a, b| {
        a.span
            .start
            .cmp(&b.span.start)
            .then(a.span.end.cmp(&b.span.end))
    });
    Ok(cues)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(start: &str, end: &str, text: &str) -> CaptionCue {
        CaptionCue {
            id: CueId::new(),
            span: Span::new(Time::parse(start).unwrap(), Time::parse(end).unwrap()),
            text: text.to_string(),
            style: None,
        }
    }

    #[test]
    fn srt_timestamps_are_the_same_instants_time_reports() {
        let at = Time::parse("1h2m3.5s").unwrap();
        assert_eq!(at.clock(), "01:02:03.500");
        assert_eq!(stamp(at, ','), "01:02:03,500");
    }

    #[test]
    fn srt_round_trips_commas_and_multiple_lines() {
        let cues = vec![
            cue("0", "2.5", "Hello, world"),
            cue("2.5", "5", "first line\nsecond line"),
        ];
        let text = to_srt(&cues);
        assert!(text.starts_with("1\n00:00:00,000 --> 00:00:02,500\nHello, world\n"));
        let back = from_srt(&text).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].text, "Hello, world");
        assert_eq!(back[1].text, "first line\nsecond line");
        assert_eq!(back[1].span, cues[1].span);
    }

    #[test]
    fn reader_absorbs_bom_crlf_and_missing_blank_lines() {
        let text = "\u{feff}1\r\n00:00:01,000 --> 00:00:02,000\r\nfirst\r\n2\r\n00:00:03,000 --> 00:00:04,000\r\nsecond\r\n";
        let cues = from_srt(text).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "first");
        assert_eq!(cues[1].text, "second");
        assert_eq!(cues[1].span.start, Time::from_secs(3));
    }

    #[test]
    fn out_of_order_input_comes_back_sorted() {
        let text = "\
2
00:00:10,000 --> 00:00:12,000
later

1
00:00:01,000 --> 00:00:02,000
earlier
";
        let cues = from_srt(text).unwrap();
        assert_eq!(
            cues.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            ["earlier", "later"]
        );
    }

    #[test]
    fn vtt_header_and_decimal_point_round_trip() {
        let cues = vec![cue("1/3", "2/3", "third of a second")];
        let text = to_vtt(&cues);
        assert!(text.starts_with("WEBVTT\n\n"), "missing header: {text}");
        assert!(text.contains("00:00:00.333 --> 00:00:00.667"), "{text}");
        let back = from_vtt(&text).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].text, "third of a second");
    }

    #[test]
    fn vtt_notes_settings_and_identifiers_are_not_caption_text() {
        let text = "\
WEBVTT
Kind: captions

NOTE this file was generated
by a tool

intro-cue
00:00:01.000 --> 00:00:02.000 align:start position:10%
visible text
";
        let cues = from_vtt(text).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "visible text");
        assert_eq!(cues[0].span.end, Time::from_secs(2));
    }

    #[test]
    fn fractions_keep_their_written_precision() {
        let cues = from_srt("00:00:01,0005 --> 00:00:02,000\nx\n").unwrap();
        assert_eq!(cues[0].span.start, Time::new(10005, 10000).unwrap());
    }

    #[test]
    fn an_unreadable_timestamp_is_an_error_not_a_dropped_cue() {
        let error = from_srt("1\n00:00:0x,000 --> 00:00:02,000\nlost\n").unwrap_err();
        assert!(
            error.to_string().contains("start timestamp"),
            "unhelpful error: {error}"
        );
    }

    #[test]
    fn empty_input_is_empty_output() {
        assert!(from_srt("\u{feff}\r\n\r\n").unwrap().is_empty());
        assert_eq!(to_srt(&[]), "");
    }
}
