//! Turning words into readable captions.
//!
//! Three constraints decide where a cue begins and ends, and none of them is "every N
//! words". A caption that changes faster than a person reads is worse than no caption, so
//! the reading rate (`max_cps`) sets a *minimum display duration* and cues are held past
//! the last word they contain, up to the moment the next cue needs the screen. The line
//! budget comes from the type size against the frame rather than from a magic constant, so
//! 96 px captions on a vertical video break earlier than 48 px ones on 1080p. And a cue
//! never splits a sentence when its end is a word or two away, because "we raised our" /
//! "prices." reads as two wrong statements.
//!
//! [`cue_svg`] exists so burning captions reuses the title render path instead of growing a
//! second text renderer with its own font resolution, its own outline handling and its own
//! set of bugs. A caption is a title that happens to be generated from a transcript.

use crate::transcript::Word;
use dvs_core::ids::CueId;
use dvs_core::project::{escape_xml, CaptionCue, CaptionPosition, CaptionStyle};
use dvs_core::time::{Span, Time};

/// Frame width [`cues_from_words`] assumes when the caller has no frame to measure
/// against — 1080p, the size the `size_px` defaults were chosen for. Callers that know the
/// sequence size use [`cues_from_words_for_width`] and get an exact budget.
pub const REFERENCE_WIDTH: u32 = 1920;

/// Mean glyph advance of a sans-serif face as a fraction of the em. Real advance needs the
/// font, which this crate deliberately does not load; 0.5 em is the long-run average for
/// mixed-case Latin text and errs toward breaking early.
const GLYPH_ADVANCE: f32 = 0.5;

/// Below this a line stops being a line. Reached only by absurd type sizes, but a zero
/// budget would loop forever looking for a break.
const MIN_CHARS_PER_LINE: usize = 12;

/// A silence at least this long ends a cue: the speaker stopped, so the caption should too.
const MAX_PAUSE: Time = Time::from_ratio(dvs_core::time::R::new_raw(3, 5));

/// No cue is displayed for less than this, however short the word. A one-frame flash of
/// text is unreadable and reads as a glitch.
const MIN_DISPLAY: Time = Time::from_ratio(dvs_core::time::R::new_raw(4, 5));

/// How far past a full line a sentence boundary may sit and still be pulled into the cue.
const SENTENCE_LOOKAHEAD: usize = 2;

/// Line height as a multiple of the type size.
const LINE_HEIGHT: f32 = 1.2;

/// Cap height plus a little: where the first baseline sits below the top of the text block.
const ASCENT: f32 = 0.8;

/// Characters that fit on one line of captions at this style's type size.
///
/// Derived from the safe-area width rather than fixed, because the whole point of
/// `size_px` is that a caption sized for a phone screen wraps sooner than one sized for a
/// television.
pub fn chars_per_line(style: &CaptionStyle, frame_width: u32) -> usize {
    let usable = frame_width as f32 * style.safe_area.clamp(0.1, 1.0);
    let advance = style.size_px.max(1.0) * GLYPH_ADVANCE;
    ((usable / advance).floor().max(0.0) as usize).max(MIN_CHARS_PER_LINE)
}

/// Whether a word closes a sentence. Trailing quotes and brackets are looked through, so
/// `said."` still ends one.
fn ends_sentence(text: &str) -> bool {
    text.trim_end()
        .trim_end_matches(|c: char| matches!(c, '"' | '\'' | ')' | ']' | '»' | '”' | '’'))
        .ends_with(['.', '!', '?', '…'])
}

/// Non-whitespace characters, the measure [`CaptionCue::chars_per_second`] uses. Computing
/// the reading budget in the same unit the lint checks is what makes the guarantee real.
fn dense_len(words: &[Word]) -> usize {
    words.iter().map(Word::dense_len).sum()
}

/// Why a group of words stopped growing. Only a budget stop may reach forward for a
/// sentence boundary: reaching across a pause would pull in the *next* sentence, and
/// reaching past a boundary is pointless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Budget,
    Sentence,
    Pause,
    End,
}

/// Split words into cue-sized groups as index ranges.
fn groups(words: &[Word], budget: usize) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < words.len() {
        // At least one word per cue, however long it is: the alternative is an empty cue.
        let mut end = at + 1;
        let mut chars = words[at].text.chars().count();
        let mut stop = if ends_sentence(&words[at].text) {
            Stop::Sentence
        } else {
            Stop::End
        };
        while end < words.len() && stop == Stop::End {
            if words[end].start - words[end - 1].end >= MAX_PAUSE {
                stop = Stop::Pause;
                break;
            }
            let with = chars + 1 + words[end].text.chars().count();
            if with > budget {
                stop = Stop::Budget;
                break;
            }
            chars = with;
            end += 1;
            if ends_sentence(&words[end - 1].text) {
                stop = Stop::Sentence;
            }
        }
        if stop == Stop::Budget {
            let limit = (end + SENTENCE_LOOKAHEAD).min(words.len());
            if let Some(boundary) = (end..limit).find(|index| ends_sentence(&words[*index].text)) {
                // Only if no pause intervenes — a boundary on the far side of a silence
                // belongs to the next cue.
                let unbroken = (end..=boundary)
                    .all(|index| words[index].start - words[index - 1].end < MAX_PAUSE);
                if unbroken {
                    end = boundary + 1;
                }
            }
        }
        out.push(at..end);
        at = end;
    }
    out
}

/// Display time a run of text needs to be readable at `max_cps`. A non-positive ceiling
/// means the caller has opted out of the constraint, not that every cue is infinite.
fn reading_time(dense: usize, max_cps: f64) -> Time {
    let denominator = (max_cps * 1_000_000.0).floor();
    if !(denominator >= 1.0) {
        return Time::ZERO;
    }
    // Floor on the denominator, so the resulting rate is at or below the ceiling rather
    // than a rounding error above it.
    Time::new(dense as i64 * 1_000_000, denominator as i64).unwrap_or(Time::ZERO)
}

/// Segment words into cues for a caller that knows the frame width.
///
/// Cues never overlap: a cue held for reading time yields the screen the instant the next
/// one starts. When speech is genuinely faster than `max_cps` there is no honest fix — the
/// words were said — so the cue keeps its text and `caption.generate` reports it as a
/// `caption-too-fast` finding instead of silently dropping words.
pub fn cues_from_words_for_width(
    words: &[Word],
    style: &CaptionStyle,
    frame_width: u32,
) -> Vec<CaptionCue> {
    let per_line = chars_per_line(style, frame_width);
    let budget = per_line.saturating_mul(style.max_lines.max(1));
    let groups = groups(words, budget);
    let mut cues = Vec::with_capacity(groups.len());
    for (index, range) in groups.iter().enumerate() {
        let group = &words[range.clone()];
        let text = crate::transcript::text_of(group);
        if text.is_empty() {
            continue;
        }
        let start = group[0].start;
        let spoken_end = group[group.len() - 1].end;
        let next_start = groups
            .get(index + 1)
            .map(|next| words[next.start].start);

        let desired = (start + reading_time(dense_len(group), style.max_cps))
            .max(start + MIN_DISPLAY)
            .max(spoken_end);
        let mut end = match next_start {
            Some(next) => desired.min(next.max(spoken_end)),
            None => desired,
        };
        // A cue that would run into the next one is cut at it; overlapping cues cannot be
        // displayed, and two captions on screen at once is the one state renderers cannot
        // resolve.
        if let Some(next) = next_start {
            if next > start {
                end = end.min(next);
            }
        }
        cues.push(CaptionCue {
            id: CueId::new(),
            span: Span::new(start, end),
            text: wrap_lines(&text, per_line, style.max_lines).join("\n"),
            // The track owns the style, so restyling every caption is one edit rather than
            // one per cue.
            style: None,
        });
    }
    cues
}

/// Segment words into cues against a 1080p frame. See [`cues_from_words_for_width`].
pub fn cues_from_words(words: &[Word], style: &CaptionStyle) -> Vec<CaptionCue> {
    cues_from_words_for_width(words, style, REFERENCE_WIDTH)
}

/// Greedy fill at a fixed width.
fn greedy(words: &[&str], width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in words {
        match lines.last_mut() {
            Some(line) if line.chars().count() + 1 + word.chars().count() <= width => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push((*word).to_string()),
        }
    }
    lines
}

/// Wrap text into at most `max_lines` lines of about `max_chars`, balanced.
///
/// Greedy wrapping produces "a very long first line" followed by one orphaned word, which
/// looks broken on screen. So the line *count* is decided greedily at `max_chars` (capped
/// at `max_lines`) and then the narrowest width that still fits that many lines is used,
/// which distributes the words evenly. When the text cannot fit `max_lines` lines at
/// `max_chars` the lines grow instead of multiplying: `max_lines` is a hard ceiling — a
/// third line would cover the picture — and an over-long line is what the
/// `caption-too-long` lint reports.
pub fn wrap_lines(text: &str, max_chars: usize, max_lines: usize) -> Vec<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return Vec::new();
    }
    let max_lines = max_lines.max(1);
    let widest = words
        .iter()
        .map(|word| word.chars().count())
        .max()
        .unwrap_or(1)
        .max(1);
    let total: usize = words.iter().map(|word| word.chars().count()).sum::<usize>() + words.len()
        - 1;
    let wanted = greedy(&words, max_chars.max(widest)).len().min(max_lines);

    // Line count is monotonically non-increasing in width, so the narrowest width holding
    // `wanted` lines is a binary search.
    let mut lo = widest;
    let mut hi = total.max(widest);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if greedy(&words, mid).len() <= wanted {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    greedy(&words, lo)
}

/// The SVG a cue renders as, at a given frame size.
///
/// Same output shape as a title document, so the compositor's existing SVG path draws
/// captions: `font-family` for the face, `text-anchor` for centring, and `stroke` plus
/// `paint-order="stroke"` for the outline, which is the only way to get an outline *behind*
/// the glyph fill rather than eating into it. The block is positioned inside
/// `style.safe_area` so a caption cannot land in a phone's rounded corner or under a
/// broadcaster's bug.
pub fn cue_svg(cue: &CaptionCue, style: &CaptionStyle, size: [u32; 2]) -> String {
    let [width, height] = [size[0].max(1) as f32, size[1].max(1) as f32];
    let lines: Vec<&str> = cue.text.lines().filter(|line| !line.is_empty()).collect();
    let size_px = style.size_px.max(1.0);
    let line_height = size_px * LINE_HEIGHT;
    let block = line_height * lines.len().max(1) as f32;
    let safe = style.safe_area.clamp(0.1, 1.0);
    let margin_y = height * (1.0 - safe) / 2.0;
    let center_x = width / 2.0;
    let first_baseline = match style.position {
        CaptionPosition::Top => margin_y + size_px * ASCENT,
        CaptionPosition::Middle => (height - block) / 2.0 + size_px * ASCENT,
        CaptionPosition::Bottom => height - margin_y - block + size_px * ASCENT,
    };

    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width:.0}\" height=\"{height:.0}\" \
         viewBox=\"0 0 {width:.0} {height:.0}\">\n"
    );
    if let Some(background) = style.background {
        // Width is estimated from the glyph advance for the same reason the line budget
        // is: no font is loaded here. Erring wide keeps text off the plate's edge.
        let widest = lines
            .iter()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0) as f32;
        let plate = (widest * size_px * GLYPH_ADVANCE + size_px).min(width);
        svg.push_str(&format!(
            "  <rect x=\"{:.2}\" y=\"{:.2}\" width=\"{plate:.2}\" height=\"{:.2}\" \
             fill=\"{background}\" fill-opacity=\"{:.3}\" rx=\"{:.2}\"/>\n",
            center_x - plate / 2.0,
            first_baseline - size_px * ASCENT - size_px * 0.15,
            block + size_px * 0.3,
            f32::from(background.a) / 255.0,
            size_px * 0.1,
        ));
    }
    svg.push_str(&format!(
        "  <text font-family=\"{}\" font-size=\"{size_px:.2}\" fill=\"{}\" \
         text-anchor=\"middle\" xml:space=\"preserve\"",
        escape_xml(&style.font),
        style.color
    ));
    if let Some(outline) = style.outline {
        svg.push_str(&format!(
            " stroke=\"{outline}\" stroke-width=\"{:.2}\" stroke-linejoin=\"round\" \
             paint-order=\"stroke\"",
            size_px / 8.0
        ));
    }
    svg.push_str(">\n");
    for (index, line) in lines.iter().enumerate() {
        svg.push_str(&format!(
            "    <tspan x=\"{center_x:.2}\" y=\"{:.2}\">{}</tspan>\n",
            first_baseline + line_height * index as f32,
            escape_xml(line)
        ));
    }
    svg.push_str("  </text>\n</svg>\n");
    svg
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::project::CaptionStyle;

    fn style() -> CaptionStyle {
        CaptionStyle::named("test")
    }

    /// Words at a steady rate, `per` seconds each, back to back from `from`.
    fn run(from: f64, per: f64, texts: &[&str]) -> Vec<Word> {
        let scale = 1000.0;
        let mut at = (from * scale).round() as i64;
        let step = (per * scale).round() as i64;
        texts
            .iter()
            .map(|text| {
                let start = Time::new(at, scale as i64).unwrap();
                at += step;
                Word::new(
                    *text,
                    start,
                    Time::new(at, scale as i64).unwrap(),
                    1.0,
                )
            })
            .collect()
    }

    #[test]
    fn a_cue_is_held_long_enough_to_read() {
        let mut style = style();
        style.max_cps = 12.0;
        // "extraordinarily" is 15 characters spoken in 0.4 s: 37 cps if shown only while
        // it is said. A 3 s silence follows, so the cue can be held.
        let mut words = run(0.0, 0.4, &["extraordinarily"]);
        words.extend(run(4.0, 0.4, &["later"]));
        let cues = cues_from_words(&words, &style);

        assert_eq!(cues.len(), 2, "a 3.6 s pause separates the cues");
        assert!(
            cues[0].span.end > words[0].end,
            "the cue outlives the word it shows: {:?}",
            cues[0].span
        );
        assert!(
            cues[0].chars_per_second() <= style.max_cps,
            "{} cps exceeds the {} ceiling",
            cues[0].chars_per_second(),
            style.max_cps
        );
    }

    #[test]
    fn no_cue_exceeds_the_reading_rate_or_the_line_count() {
        let mut style = style();
        style.max_cps = 16.0;
        style.max_lines = 2;
        // 0.32 s per word at ~5 characters each is ~15 cps of speech, inside the ceiling
        // once cues are held over the gaps between sentences.
        let words = run(
            0.0,
            0.32,
            &[
                "today", "we", "shipped", "the", "pricing", "page", "and", "it", "works.",
                "next", "week", "we", "turn", "on", "billing", "for", "every", "account.",
                "after", "that", "the", "trial", "flow", "lands", "and", "we", "are",
                "done.",
            ],
        );
        let cues = cues_from_words(&words, &style);
        assert!(!cues.is_empty());
        for cue in &cues {
            assert!(
                cue.chars_per_second() <= style.max_cps,
                "cue '{}' runs at {:.2} cps, over the {} ceiling",
                cue.text,
                cue.chars_per_second(),
                style.max_cps
            );
            assert!(
                cue.lines() <= style.max_lines,
                "cue '{}' has {} lines, over the {} ceiling",
                cue.text,
                cue.lines(),
                style.max_lines
            );
            assert!(!cue.span.is_empty());
        }
        for pair in cues.windows(2) {
            assert!(
                pair[0].span.end <= pair[1].span.start,
                "cues must not overlap: {} then {}",
                pair[0].span,
                pair[1].span
            );
        }
    }

    #[test]
    fn a_sentence_end_one_word_away_is_pulled_into_the_cue() {
        let mut style = style();
        // Twelve characters per line over two lines: a 24-character cue budget.
        style.size_px = 1920.0 * 0.9 / (12.0 * GLYPH_ADVANCE);
        style.max_lines = 2;
        assert_eq!(chars_per_line(&style, REFERENCE_WIDTH), 12);

        // The budget is used up after "list" (23 characters). "prices." is one word past
        // it and ends the sentence, so the cue takes it rather than orphaning it.
        let near = run(0.0, 0.3, &["we", "have", "raised", "our", "list", "prices."]);
        let cues = cues_from_words(&near, &style);
        assert_eq!(
            cues.len(),
            1,
            "the sentence must not be split one word from its end: {:?}",
            cues.iter().map(|cue| cue.text.clone()).collect::<Vec<_>>()
        );
        assert!(cues[0].text.replace('\n', " ").ends_with("prices."));
        assert!(cues[0].lines() <= style.max_lines);

        // Three words past the budget is too far: holding a whole extra clause would push
        // the line count past the ceiling, so the cue breaks where the budget said.
        let far = run(
            0.0,
            0.3,
            &["we", "have", "raised", "our", "list", "prices", "again", "today."],
        );
        let split = cues_from_words(&far, &style);
        assert_eq!(
            split.len(),
            2,
            "the lookahead is bounded: {:?}",
            split.iter().map(|cue| cue.text.clone()).collect::<Vec<_>>()
        );
        assert_eq!(split[0].text.replace('\n', " "), "we have raised our list");
    }

    #[test]
    fn a_long_pause_starts_a_new_cue_even_mid_sentence() {
        let style = style();
        let mut words = run(0.0, 0.3, &["and", "then"]);
        words.extend(run(5.0, 0.3, &["nothing", "happened."]));
        let cues = cues_from_words(&words, &style);
        assert_eq!(cues.len(), 2, "a 4.4 s silence is not captioned over");
        assert_eq!(cues[0].text, "and then");
    }

    #[test]
    fn wrap_lines_balances_instead_of_orphaning_the_last_word() {
        assert_eq!(
            wrap_lines("aaa bbb ccc ddd", 12, 2),
            vec!["aaa bbb".to_string(), "ccc ddd".to_string()],
            "greedy wrapping would leave 'ddd' alone on line two"
        );
    }

    #[test]
    fn wrap_lines_never_exceeds_max_lines() {
        let text = "one two three four five six seven eight nine ten eleven twelve";
        let lines = wrap_lines(text, 10, 2);
        assert_eq!(lines.len(), 2, "lines widen rather than multiply: {lines:?}");
        assert_eq!(
            lines.join(" "),
            text,
            "no word may be dropped to make the text fit"
        );
    }

    #[test]
    fn wrap_lines_keeps_a_word_longer_than_the_budget() {
        let lines = wrap_lines("a pneumonoultramicroscopicsilicovolcanoconiosis", 10, 2);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1], "pneumonoultramicroscopicsilicovolcanoconiosis");
    }

    #[test]
    fn cue_svg_positions_the_block_inside_the_safe_area() {
        let mut style = style();
        style.size_px = 40.0;
        style.safe_area = 0.9;
        style.outline = Some(dvs_core::Rgba::BLACK);
        let cue = CaptionCue {
            id: CueId::new(),
            span: Span::new(Time::ZERO, Time::from_secs(2)),
            text: "first line\nsecond & last".to_string(),
            style: None,
        };
        let svg = cue_svg(&cue, &style, [1920, 1080]);

        assert!(svg.contains("paint-order=\"stroke\""), "{svg}");
        assert!(svg.contains("text-anchor=\"middle\""), "{svg}");
        assert!(svg.contains("font-family=\"sans-serif\""), "{svg}");
        assert!(
            svg.contains("second &amp; last"),
            "cue text must be XML-escaped: {svg}"
        );

        let baselines: Vec<f32> = svg
            .lines()
            .filter_map(|line| line.split("y=\"").nth(1))
            .filter_map(|rest| rest.split('"').next())
            .filter_map(|value| value.parse::<f32>().ok())
            .collect();
        assert_eq!(baselines.len(), 2, "one baseline per line: {svg}");
        // Bottom position: the descender of the last line stays above the safe-area edge.
        let safe_bottom = 1080.0 - 1080.0 * 0.05;
        assert!(
            baselines[1] + style.size_px * (LINE_HEIGHT - ASCENT) <= safe_bottom + 0.01,
            "last baseline {} overflows the safe area at {safe_bottom}",
            baselines[1]
        );
        assert!(baselines[0] < baselines[1], "lines run downward");
    }

    #[test]
    fn caption_position_moves_the_block() {
        let cue = CaptionCue {
            id: CueId::new(),
            span: Span::new(Time::ZERO, Time::from_secs(1)),
            text: "line".to_string(),
            style: None,
        };
        let baseline = |position: CaptionPosition| -> f32 {
            let mut style = style();
            style.position = position;
            let svg = cue_svg(&cue, &style, [1920, 1080]);
            svg.split("<tspan")
                .nth(1)
                .and_then(|rest| rest.split("y=\"").nth(1))
                .and_then(|rest| rest.split('"').next())
                .and_then(|value| value.parse::<f32>().ok())
                .expect("a tspan carries a baseline")
        };
        let top = baseline(CaptionPosition::Top);
        let middle = baseline(CaptionPosition::Middle);
        let bottom = baseline(CaptionPosition::Bottom);
        assert!(top < middle && middle < bottom, "{top} {middle} {bottom}");
        assert!(top >= 1080.0 * 0.05, "top must clear the safe-area margin");
    }

    #[test]
    fn chars_per_line_follows_the_type_size() {
        let mut small = style();
        small.size_px = 32.0;
        let mut large = style();
        large.size_px = 96.0;
        assert!(
            chars_per_line(&small, REFERENCE_WIDTH) > chars_per_line(&large, REFERENCE_WIDTH),
            "bigger type must wrap sooner"
        );
        assert_eq!(
            chars_per_line(&large, 1080),
            20,
            "972 safe pixels at a 48 px advance"
        );
    }
}
