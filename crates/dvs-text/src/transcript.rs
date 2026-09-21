//! Word-timestamped transcripts, stored per *asset*.
//!
//! An agent cannot watch the video, but it can read what was said and when, which is why
//! this layer carries most of the editing weight for talking-head, screencast and social
//! work: "cut every 'um'", "keep the sentence that mentions pricing", "caption it".
//!
//! The storage decision is the load-bearing one. A transcript belongs to the asset
//! (`transcript/<assetId>.json`), never to a clip, so one transcription survives every
//! trim, split, slip, retime and reorder the timeline goes through afterwards. The clip is
//! a *view* onto those words: [`Transcript::words_for_clip`] maps source time into timeline
//! time through `source_in`, `speed` and `reverse` and drops what the clip does not show.
//! Copying words onto clips instead would mean every trim has to re-cut the text, and the
//! first op that forgot to would leave an agent reading words that are no longer on screen.
//!
//! Matching is normalized — lowercase, punctuation stripped, whitespace collapsed — because
//! a model writes `"Pricing,"` and an agent writes `pricing`, and a literal comparison
//! would silently answer "no occurrences" to a question that has three.

use dvs_core::error::{Error, Result};
use dvs_core::ids::AssetId;
use dvs_core::paths::ProjectPaths;
use dvs_core::project::Clip;
use dvs_core::time::{Span, Time};
use serde::{Deserialize, Serialize};

/// Confidence for a word that arrived without one. An imported transcript that states no
/// score is treated as certain rather than as worthless, because a zero would make every
/// confidence filter downstream discard the whole file.
fn certain() -> f32 {
    1.0
}

/// One recognised word, in the source media's own time base.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Word {
    /// The word as the model wrote it, punctuation and capitalisation included. Display
    /// text and match text differ on purpose; [`normalize`] produces the latter.
    pub text: String,
    pub start: Time,
    pub end: Time,
    /// Model confidence in `0.0..=1.0`.
    #[serde(default = "certain")]
    pub confidence: f32,
}

impl Word {
    pub fn new(text: impl Into<String>, start: Time, end: Time, confidence: f32) -> Self {
        Word {
            text: text.into(),
            start,
            end,
            confidence,
        }
    }

    pub fn span(&self) -> Span {
        Span::new(self.start, self.end)
    }

    /// Characters that occupy space on screen. Whitespace is excluded to match
    /// [`dvs_core::project::CaptionCue::chars_per_second`], so a reading-rate budget
    /// computed here and checked there agree exactly.
    pub fn dense_len(&self) -> usize {
        self.text.chars().filter(|c| !c.is_whitespace()).count()
    }

    /// The comparison form: lowercase, punctuation removed.
    pub fn key(&self) -> String {
        normalize(&self.text)
    }
}

/// The match form of a token: ASCII-folded lowercase with every non-alphanumeric character
/// dropped, so `"Pricing,"`, `pricing` and `PRICING` are one word and `don't` matches
/// `dont`. Returns an empty string for a token that is pure punctuation, which callers
/// treat as "not a word" rather than as a word that matches nothing.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
        }
    }
    out
}

/// Words joined back into readable text, for a cue body or a digest excerpt.
pub fn text_of(words: &[Word]) -> String {
    let mut out = String::new();
    for word in words {
        let piece = word.text.trim();
        if piece.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(piece);
    }
    out
}

/// Merge spans that overlap, touch, or are separated by less than `min_gap`.
///
/// This is what turns twelve consecutive "um"s into one cut instead of twelve, and it is
/// shared rather than reimplemented because a ripple applied twelve times is twelve chances
/// to be one frame wrong.
pub fn merge_spans(mut spans: Vec<Span>, min_gap: Time) -> Vec<Span> {
    spans.sort_by(|a, b| a.start.cmp(&b.start));
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    for span in spans {
        match out.last_mut() {
            Some(last) if span.start <= last.end || span.start - last.end < min_gap => {
                last.end = last.end.max(span.end);
            }
            _ => out.push(span),
        }
    }
    out
}

/// A phrase as a sequence of match keys. An empty result means the caller asked for
/// nothing matchable (`","`), which is never treated as "matches everywhere".
fn keys_of(phrase: &str) -> Vec<String> {
    phrase
        .split_whitespace()
        .map(normalize)
        .filter(|key| !key.is_empty())
        .collect()
}

/// The words that carry a match key, paired with their index. Punctuation-only tokens are
/// skipped so a stray `--` between two words cannot break a phrase match.
fn keyed(words: &[Word]) -> Vec<(usize, String)> {
    words
        .iter()
        .enumerate()
        .map(|(index, word)| (index, word.key()))
        .filter(|(_, key)| !key.is_empty())
        .collect()
}

/// Whether `needle` sits at `at` in a keyed word list.
fn matches_at(keyed: &[(usize, String)], at: usize, needle: &[String]) -> bool {
    at + needle.len() <= keyed.len()
        && needle
            .iter()
            .enumerate()
            .all(|(offset, key)| keyed[at + offset].1 == *key)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transcript {
    /// The asset these words were recognised from. The file name is derived from it, so a
    /// transcript cannot be filed against the wrong media.
    pub asset: AssetId,
    /// BCP-47-ish language tag as the recogniser reported it, or `und` when unknown.
    pub language: String,
    /// What produced it: a whisper model name, or `import` for words that came from
    /// outside this engine.
    pub model: String,
    /// Sorted by `start`. [`Transcript::in_span`] binary-searches this, and
    /// [`Transcript::load`] enforces the order on read.
    pub words: Vec<Word>,
}

impl Transcript {
    pub fn new(
        asset: AssetId,
        language: impl Into<String>,
        model: impl Into<String>,
        words: Vec<Word>,
    ) -> Self {
        let mut transcript = Transcript {
            asset,
            language: language.into(),
            model: model.into(),
            words,
        };
        transcript.sort_words();
        transcript
    }

    /// Restore the sorted-by-start invariant. Stable, so two words sharing a start keep
    /// the order the recogniser emitted them in.
    pub fn sort_words(&mut self) {
        self.words.sort_by(|a, b| a.start.cmp(&b.start));
    }

    pub fn path(paths: &ProjectPaths, asset: &AssetId) -> std::path::PathBuf {
        paths.transcript(asset.as_str())
    }

    pub fn exists(paths: &ProjectPaths, asset: &AssetId) -> bool {
        Self::path(paths, asset).is_file()
    }

    /// Read `transcript/<assetId>.json`. A missing file is an op error naming the two ways
    /// to produce one, because "no such file" is not the actionable part of that failure.
    pub fn load(paths: &ProjectPaths, asset: &AssetId) -> Result<Transcript> {
        let path = Self::path(paths, asset);
        let bytes = std::fs::read(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Error::op(format!(
                    "asset {asset} has no transcript at {}; produce one with 'transcript.run' \
                     (needs a whisper model) or 'transcript.import --path words.json'",
                    path.display()
                ))
            } else {
                Error::io(&path, error)
            }
        })?;
        let mut transcript: Transcript =
            serde_json::from_slice(&bytes).map_err(|error| Error::json(&path, error))?;
        if transcript.asset != *asset {
            return Err(Error::op(format!(
                "{} is filed under {asset} but names asset {}",
                path.display(),
                transcript.asset
            )));
        }
        transcript.sort_words();
        Ok(transcript)
    }

    pub fn save(&self, paths: &ProjectPaths) -> Result<()> {
        let dir = paths.transcripts_dir();
        std::fs::create_dir_all(&dir).map_err(|error| Error::io(&dir, error))?;
        let path = Self::path(paths, &self.asset);
        let mut bytes = serde_json::to_vec_pretty(self).expect("a transcript serializes");
        bytes.push(b'\n');
        std::fs::write(&path, bytes).map_err(|error| Error::io(&path, error))
    }

    /// Source time covered by the recognised words.
    pub fn span(&self) -> Span {
        match (self.words.first(), self.words.last()) {
            (Some(first), Some(last)) => Span::new(first.start, last.end),
            _ => Span::new(Time::ZERO, Time::ZERO),
        }
    }

    /// The words overlapping a span, as a slice of the stored order.
    ///
    /// Half-open, like every other span in the engine: a word starting exactly at
    /// `span.end` is outside, and a word ending exactly at `span.start` is outside.
    pub fn in_span(&self, span: Span) -> &[Word] {
        if span.is_empty() || self.words.is_empty() {
            return &[];
        }
        let hi = self.words.partition_point(|word| word.start < span.end);
        let lo = self.words[..hi].partition_point(|word| word.end <= span.start);
        &self.words[lo.min(hi)..hi]
    }

    /// Every occurrence of a word sequence, one [`Span`] per occurrence covering the
    /// matched words.
    ///
    /// Matching is normalized (case-insensitive, punctuation-ignoring, whitespace-collapsing)
    /// and occurrences do not overlap: after a hit, scanning resumes past it, so `"very very
    /// very"` searched for `"very very"` reports one range, not two.
    pub fn find_phrase(&self, phrase: &str) -> Vec<Span> {
        let needle = keys_of(phrase);
        if needle.is_empty() {
            return Vec::new();
        }
        let keyed = keyed(&self.words);
        let mut out = Vec::new();
        let mut at = 0usize;
        while at + needle.len() <= keyed.len() {
            if matches_at(&keyed, at, &needle) {
                let first = keyed[at].0;
                let last = keyed[at + needle.len() - 1].0;
                out.push(Span::new(self.words[first].start, self.words[last].end));
                at += needle.len();
            } else {
                at += 1;
            }
        }
        out
    }

    /// The asset's words as they appear on the timeline through one clip.
    ///
    /// This is why transcripts are per asset: the mapping runs through `source_in`, `speed`
    /// and `reverse`, so a clip that was trimmed, retimed to 2× and reversed still reports
    /// exactly the words it shows, at the times it shows them. Words the clip does not
    /// cover are dropped, and a word straddling an edge is clamped to the clip so no caption
    /// or cut derived from it can address a frame the clip never displays.
    pub fn words_for_clip(&self, clip: &Clip) -> Vec<Word> {
        // A held frame (speed 0) consumes no source time, so no source word maps onto it.
        if *clip.speed.ratio().numer() == 0 {
            return Vec::new();
        }
        let source = clip.source_span();
        let consumed = clip.duration * clip.speed;
        let to_timeline = |source_at: Time| -> Time {
            let local = if clip.reverse {
                (clip.source_in + consumed - source_at) / clip.speed
            } else {
                (source_at - clip.source_in) / clip.speed
            };
            clip.start + local
        };

        let mut out = Vec::new();
        for word in &self.words {
            if word.span().intersect(&source).is_none() {
                continue;
            }
            let (a, b) = (to_timeline(word.start), to_timeline(word.end));
            // Reverse plays the span backwards, so the word's source start is its timeline
            // end. Ordering by value rather than by field keeps one code path.
            let mapped = Span::new(a.min(b), a.max(b));
            let Some(visible) = mapped.intersect(&clip.span()) else {
                continue;
            };
            out.push(Word {
                text: word.text.clone(),
                start: visible.start,
                end: visible.end,
                confidence: word.confidence,
            });
        }
        out.sort_by(|a, b| a.start.cmp(&b.start));
        out
    }

    /// Ranges occupied by filler words, merged when separated by less than `min_gap`.
    ///
    /// Merging is the point: twelve "um"s in a row become one cut, so the ripple runs once
    /// and the reported edit is one range an agent can read back. Multi-word fillers
    /// (`you know`) are matched greedily longest-first, so `you know` is never reported as
    /// the single word `you`.
    pub fn filler_spans(&self, fillers: &[String], min_gap: Time) -> Vec<Span> {
        let mut needles: Vec<Vec<String>> = fillers
            .iter()
            .map(|filler| keys_of(filler))
            .filter(|keys| !keys.is_empty())
            .collect();
        needles.sort_by(|a, b| b.len().cmp(&a.len()));
        if needles.is_empty() {
            return Vec::new();
        }
        let keyed = keyed(&self.words);
        let mut hits = Vec::new();
        let mut at = 0usize;
        while at < keyed.len() {
            let matched = needles
                .iter()
                .find(|needle| matches_at(&keyed, at, needle))
                .map(|needle| needle.len());
            match matched {
                Some(len) => {
                    let first = keyed[at].0;
                    let last = keyed[at + len - 1].0;
                    hits.push(Span::new(self.words[first].start, self.words[last].end));
                    at += len;
                }
                None => at += 1,
            }
        }
        merge_spans(hits, min_gap)
    }
}

/// The filler words `transcript.cut-words` removes when the caller names none. Chosen
/// because they are the ones that survive into a finished talking-head cut.
pub const DEFAULT_FILLERS: &[&str] = &["um", "uh", "er", "ah", "like", "you know"];

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::project::Source;
    use dvs_core::time::Rat;

    fn word(text: &str, start: (i64, i64), end: (i64, i64)) -> Word {
        Word::new(
            text,
            Time::new(start.0, start.1).unwrap(),
            Time::new(end.0, end.1).unwrap(),
            1.0,
        )
    }

    fn transcript(words: Vec<Word>) -> Transcript {
        Transcript::new(AssetId::new(), "en", "test", words)
    }

    /// `[10, 20)` of a source, played at 2× so ten source seconds occupy five timeline
    /// seconds, starting at timeline 100.
    fn retimed_clip() -> Clip {
        let mut clip = Clip::new(
            Source::Color {
                color: dvs_core::Rgba::BLACK,
            },
            Time::from_secs(100),
            Time::from_secs(5),
        );
        clip.source_in = Time::from_secs(10);
        clip.speed = Rat::new(2, 1).unwrap();
        clip
    }

    #[test]
    fn words_for_clip_maps_through_source_in_and_speed() {
        let transcript = transcript(vec![
            // Before the in-point: dropped.
            word("before", (9, 1), (19, 2)),
            // Source 10..11 → timeline 100..100.5 at 2×.
            word("first", (10, 1), (11, 1)),
            // Source 14..15 → timeline 102..102.5.
            word("middle", (14, 1), (15, 1)),
            // Starts exactly at the out-point (source 20): outside the half-open span.
            word("after", (20, 1), (21, 1)),
        ]);
        let mapped = transcript.words_for_clip(&retimed_clip());

        let names: Vec<&str> = mapped.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(
            names,
            ["first", "middle"],
            "only words inside [source_in, source_in + duration * speed) survive"
        );
        assert_eq!(mapped[0].start, Time::from_secs(100));
        assert_eq!(mapped[0].end, Time::new(201, 2).unwrap());
        assert_eq!(mapped[1].start, Time::from_secs(102));
        assert_eq!(mapped[1].end, Time::new(205, 2).unwrap());
    }

    #[test]
    fn words_for_clip_reverses_word_order_and_times() {
        let mut clip = retimed_clip();
        clip.reverse = true;
        let transcript = transcript(vec![
            word("early", (10, 1), (11, 1)),
            word("late", (19, 1), (20, 1)),
        ]);
        let mapped = transcript.words_for_clip(&clip);

        let names: Vec<&str> = mapped.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(
            names,
            ["late", "early"],
            "a reversed clip shows the last source word first"
        );
        // Source 19..20 is the first half-second of a reversed 10..20 span at 2×.
        assert_eq!(mapped[0].start, Time::from_secs(100));
        assert_eq!(mapped[0].end, Time::new(201, 2).unwrap());
        assert_eq!(mapped[1].end, Time::from_secs(105));
    }

    #[test]
    fn words_for_clip_clamps_a_word_straddling_the_out_point() {
        // Source 19.5..20.5 half-overlaps the clip's 10..20 source span.
        let transcript = transcript(vec![word("straddle", (39, 2), (41, 2))]);
        let mapped = transcript.words_for_clip(&retimed_clip());
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].start, Time::new(419, 4).unwrap());
        assert_eq!(
            mapped[0].end,
            Time::from_secs(105),
            "the visible part stops at the clip end, never past it"
        );
    }

    #[test]
    fn find_phrase_ignores_case_and_punctuation_and_reports_one_span_per_occurrence() {
        let transcript = transcript(vec![
            word("Our", (0, 1), (1, 2)),
            word("Pricing,", (1, 2), (1, 1)),
            word("page", (1, 1), (3, 2)),
            word("is", (3, 2), (2, 1)),
            word("live.", (2, 1), (5, 2)),
            word("PRICING!", (3, 1), (7, 2)),
        ]);

        let hits = transcript.find_phrase("pricing");
        assert_eq!(hits.len(), 2, "one span per occurrence");
        assert_eq!(hits[0], Span::new(Time::new(1, 2).unwrap(), Time::from_secs(1)));
        assert_eq!(
            hits[1],
            Span::new(Time::from_secs(3), Time::new(7, 2).unwrap())
        );

        let multi = transcript.find_phrase("  Pricing   PAGE ");
        assert_eq!(multi.len(), 1, "whitespace collapses and the pair matches once");
        assert_eq!(
            multi[0],
            Span::new(Time::new(1, 2).unwrap(), Time::new(3, 2).unwrap()),
            "the span covers both matched words, not one per word"
        );
        assert!(transcript.find_phrase(",,,").is_empty());
    }

    #[test]
    fn find_phrase_does_not_report_overlapping_occurrences() {
        let transcript = transcript(vec![
            word("very", (0, 1), (1, 1)),
            word("very", (1, 1), (2, 1)),
            word("very", (2, 1), (3, 1)),
        ]);
        let hits = transcript.find_phrase("very very");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0], Span::new(Time::ZERO, Time::from_secs(2)));
    }

    #[test]
    fn filler_spans_merge_within_the_gap_and_stay_apart_beyond_it() {
        let transcript = transcript(vec![
            word("um", (0, 1), (1, 5)),
            // 0.1 s after the previous filler: merges.
            word("uh,", (3, 10), (1, 2)),
            word("so", (1, 1), (3, 2)),
            // 2 s later: a separate cut.
            word("Um...", (7, 2), (37, 10)),
            word("yes", (4, 1), (9, 2)),
        ]);
        let fillers: Vec<String> = DEFAULT_FILLERS.iter().map(|f| f.to_string()).collect();
        let spans = transcript.filler_spans(&fillers, Time::new(1, 4).unwrap());

        assert_eq!(spans.len(), 2, "adjacent fillers merge, distant ones do not");
        assert_eq!(
            spans[0],
            Span::new(Time::ZERO, Time::new(1, 2).unwrap()),
            "the merged span covers both fillers and the gap between them"
        );
        assert_eq!(
            spans[1],
            Span::new(Time::new(7, 2).unwrap(), Time::new(37, 10).unwrap())
        );
    }

    #[test]
    fn filler_spans_prefer_the_longest_filler() {
        let transcript = transcript(vec![
            word("and", (0, 1), (1, 2)),
            word("you", (1, 2), (1, 1)),
            word("know", (1, 1), (3, 2)),
            word("that", (3, 2), (2, 1)),
        ]);
        let fillers = vec!["you know".to_string(), "you".to_string()];
        let spans = transcript.filler_spans(&fillers, Time::ZERO);
        assert_eq!(spans.len(), 1);
        assert_eq!(
            spans[0],
            Span::new(Time::new(1, 2).unwrap(), Time::new(3, 2).unwrap()),
            "'you know' is cut as one phrase, not as the word 'you'"
        );
    }

    #[test]
    fn in_span_is_half_open_at_both_ends() {
        let transcript = transcript(vec![
            word("a", (0, 1), (1, 1)),
            word("b", (1, 1), (2, 1)),
            word("c", (2, 1), (3, 1)),
        ]);
        let words = transcript.in_span(Span::new(Time::from_secs(1), Time::from_secs(2)));
        let names: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(
            names,
            ["b"],
            "'a' ends at the span start and 'c' starts at the span end; both are outside"
        );
        assert!(transcript
            .in_span(Span::new(Time::from_secs(3), Time::from_secs(4)))
            .is_empty());
    }

    #[test]
    fn save_then_load_round_trips_and_rejects_a_misfiled_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::new(dir.path());
        let original = transcript(vec![
            word("second", (2, 1), (3, 1)),
            word("first", (0, 1), (1, 1)),
        ]);
        original.save(&paths).unwrap();

        let loaded = Transcript::load(&paths, &original.asset).unwrap();
        assert_eq!(loaded.words[0].text, "first", "stored words are sorted");
        assert_eq!(loaded, original);

        let other = AssetId::new();
        std::fs::copy(
            Transcript::path(&paths, &original.asset),
            Transcript::path(&paths, &other),
        )
        .unwrap();
        let err = Transcript::load(&paths, &other).unwrap_err();
        assert!(
            err.to_string().contains("names asset"),
            "a transcript filed under the wrong asset must not be served: {err}"
        );

        let missing = Transcript::load(&paths, &AssetId::new()).unwrap_err();
        assert!(
            missing.to_string().contains("transcript.import"),
            "a missing transcript names how to produce one: {missing}"
        );
    }

    #[test]
    fn merge_spans_joins_touching_ranges_even_with_no_gap_allowance() {
        let merged = merge_spans(
            vec![
                Span::new(Time::from_secs(2), Time::from_secs(3)),
                Span::new(Time::ZERO, Time::from_secs(1)),
                Span::new(Time::from_secs(1), Time::from_secs(2)),
            ],
            Time::ZERO,
        );
        assert_eq!(
            merged,
            vec![Span::new(Time::ZERO, Time::from_secs(3))],
            "sorted, and touching ranges become one"
        );
    }
}
