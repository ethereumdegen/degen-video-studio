//! Selectors: how an agent says *which* thing.
//!
//! Indices are not addresses — "clip 3" is wrong the moment a ripple insert happens — so
//! every op takes a selector instead. The grammar is small enough to write from memory and
//! strict enough that a miss is an error carrying the real candidate list, which turns a
//! typo into one wasted turn instead of two.
//!
//! ```text
//! #talk                        by name or id, any addressable kind
//! clp_01J8XYZ…                 a bare token is the same lookup: id or name
//! V1                           a track by name, case-insensitive
//! *                            every clip in the sequence
//! clip[track=V1]               a kind head plus attribute filters, all of which must match
//! clip[source=talk.mp4]        assets match by id or by the file name they were imported as
//! clip[name^=title]            ^= prefix, *= contains, = equal, != not equal
//! clip[name*=ow]
//! clip[kind=title]             `kind` on a clip is its `Source` variant
//! clip[enabled=false, speed!=1/1]
//! track[kind=audio]            track[muted=true], track[locked=false], …
//! marker[name=pricing]         cue[text*=pricing], asset[kind=video], fx[kind=blur]
//! clip[track=V1]@00:10-00:20   only what overlaps that window
//! clip[track=V1]:last          :first, :last, :nth(N) narrow after filtering
//! #intro #music                space-separated terms are a union
//! ```
//!
//! Four decisions worth knowing, because they are what make the results predictable:
//!
//! - **Order is the timeline's, never the selector's.** Matches come back in track order,
//!   then timeline order, then id order, whatever order the terms were written in, and a
//!   thing matched by two terms appears once.
//! - **Every term must match something.** `#intro #musik` is an error rather than a
//!   silently smaller edit — the alternative is an agent deleting one clip when it meant
//!   two and never learning.
//! - **Time is resolved against the sequence.** `@00:10-00:20` is parsed with
//!   [`Time::parse_with_fps`] at the sequence frame rate, so `@0-90f` and `@00:00:00:00-…`
//!   mean something, and windows are half-open: a clip ending exactly at the window start
//!   is outside it, exactly as adjacent clips do not overlap.
//! - **Comparisons are ASCII case-insensitive.** Names are labels a human typed; case is
//!   noise. Ids are ULID-based and never collide on case alone.
//!
//! `clip`, `track`, `marker`, `cue`, `asset` and `fx` are reserved heads: a clip literally
//! named `clip` is addressed as `#clip`.

use crate::error::{Error, Result};
use crate::ids::{AssetId, ClipId, CueId, EffectId, MarkerId, SequenceId, TrackId};
use crate::project::{
    Asset, AssetKind, CaptionCue, Clip, Effect, Generator, Marker, Project, Sequence, Source,
    Track, TrackKind,
};
use crate::time::{Span, Time};
use serde::Serialize;
use std::borrow::Cow;

/// How many real labels an error carries. Enough to spot a typo, few enough that the
/// message stays readable; the overflow count tells the agent there is more.
const MAX_CANDIDATES: usize = 12;

/// What a selector term addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Clip,
    Track,
    Marker,
    Cue,
    Asset,
    Effect,
}

impl Kind {
    /// The word that heads a term of this kind. Effects are `fx`, because that is what the
    /// op namespace is called.
    pub fn keyword(self) -> &'static str {
        match self {
            Kind::Clip => "clip",
            Kind::Track => "track",
            Kind::Marker => "marker",
            Kind::Cue => "cue",
            Kind::Asset => "asset",
            Kind::Effect => "fx",
        }
    }

    /// What an error message calls it.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Effect => "effect",
            other => other.keyword(),
        }
    }

    fn parse(word: &str) -> Option<Kind> {
        [
            Kind::Clip,
            Kind::Track,
            Kind::Marker,
            Kind::Cue,
            Kind::Asset,
            Kind::Effect,
        ]
        .into_iter()
        .find(|kind| word.eq_ignore_ascii_case(kind.keyword()))
    }

    /// Filterable attributes. Published so a bad key fails with the alternatives listed
    /// instead of quietly matching nothing.
    pub fn attrs(self) -> &'static [&'static str] {
        match self {
            Kind::Clip => &["id", "name", "track", "source", "kind", "enabled", "speed"],
            Kind::Track => &["id", "name", "kind", "muted", "solo", "locked", "hidden"],
            Kind::Marker => &["id", "name", "note"],
            Kind::Cue => &["id", "text", "track", "style"],
            Kind::Asset => &["id", "name", "kind", "hash"],
            Kind::Effect => &["id", "kind", "enabled", "clip", "track"],
        }
    }
}

/// One resolved thing. An enum rather than a flattened record because the caller almost
/// always needs the track a clip sits on, and rediscovering it is a search.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Match {
    Clip { track: TrackId, clip: ClipId },
    Track(TrackId),
    Asset(AssetId),
    Marker(MarkerId),
    Cue { track: TrackId, cue: CueId },
    Effect {
        track: TrackId,
        clip: ClipId,
        effect: EffectId,
    },
}

impl Match {
    pub fn kind(&self) -> Kind {
        match self {
            Match::Clip { .. } => Kind::Clip,
            Match::Track(_) => Kind::Track,
            Match::Asset(_) => Kind::Asset,
            Match::Marker(_) => Kind::Marker,
            Match::Cue { .. } => Kind::Cue,
            Match::Effect { .. } => Kind::Effect,
        }
    }

    /// The id of the thing itself, for reporting in an [`crate::op::OpEffect`].
    pub fn id(&self) -> &str {
        match self {
            Match::Clip { clip, .. } => clip.as_str(),
            Match::Track(track) => track.as_str(),
            Match::Asset(asset) => asset.as_str(),
            Match::Marker(marker) => marker.as_str(),
            Match::Cue { cue, .. } => cue.as_str(),
            Match::Effect { effect, .. } => effect.as_str(),
        }
    }

    /// The track it lives on, when it lives on one.
    pub fn track(&self) -> Option<&TrackId> {
        match self {
            Match::Clip { track, .. } | Match::Cue { track, .. } | Match::Effect { track, .. } => {
                Some(track)
            }
            Match::Track(track) => Some(track),
            Match::Asset(_) | Match::Marker(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum AttrOp {
    Eq,
    Ne,
    Prefix,
    Contains,
}

impl AttrOp {
    pub fn token(self) -> &'static str {
        match self {
            AttrOp::Eq => "=",
            AttrOp::Ne => "!=",
            AttrOp::Prefix => "^=",
            AttrOp::Contains => "*=",
        }
    }

    fn test(self, actual: &str, wanted: &str) -> bool {
        match self {
            AttrOp::Eq => actual.eq_ignore_ascii_case(wanted),
            AttrOp::Ne => !actual.eq_ignore_ascii_case(wanted),
            // `get` rather than slicing: a multi-byte boundary must be a miss, not a panic.
            AttrOp::Prefix => actual
                .get(..wanted.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(wanted)),
            AttrOp::Contains => contains_ignore_case(actual, wanted),
        }
    }
}

fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let (haystack, needle) = (haystack.as_bytes(), needle.as_bytes());
    if needle.len() > haystack.len() {
        return false;
    }
    (0..=haystack.len() - needle.len())
        .any(|at| haystack[at..at + needle.len()].eq_ignore_ascii_case(needle))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub key: String,
    pub op: AttrOp,
    pub value: String,
}

/// What a term starts with.
#[derive(Debug, Clone, PartialEq)]
pub enum Head {
    /// `#talk`, or a bare token: id or name, any kind.
    Named(String),
    /// `clip`, `track[…]`, `*`: every candidate of that kind.
    Kind(Kind),
    /// Nothing but filters — `@00:10-00:20` on its own addresses everything in the window.
    Any,
}

/// Positional narrowing, applied after filtering so `clip[track=V1]:last` is the last clip
/// *on V1*, not the last clip that happens to also be on V1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    First,
    Last,
    /// Zero-based, so `:nth(0)` and `:first` are the same thing.
    Nth(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Term {
    pub head: Head,
    pub filters: Vec<Filter>,
    /// `@start-end`, kept as written: the frame rate needed to parse `1800f` or
    /// `00:01:00:12` belongs to the sequence, which parsing does not have.
    pub range: Option<(String, String)>,
    pub pick: Option<Pick>,
    /// The fragment as written, so an error can point at it.
    pub text: String,
}

impl Term {
    fn kind(&self) -> Option<Kind> {
        match &self.head {
            Head::Kind(kind) => Some(*kind),
            _ => None,
        }
    }

    fn parse(text: &str) -> Result<Term> {
        let head_end = text.find(['[', '@', ':']).unwrap_or(text.len());
        let (head_text, mut rest) = text.split_at(head_end);

        if head_text.contains([']', ')']) {
            return Err(Error::bad_args(format!(
                "unbalanced brackets in selector term '{text}'"
            )));
        }
        let head = match head_text {
            "" => Head::Any,
            "*" => Head::Kind(Kind::Clip),
            named if named.starts_with('#') => {
                let name = &named[1..];
                if name.is_empty() {
                    return Err(Error::bad_args(format!("'{text}' names nothing after '#'")));
                }
                Head::Named(name.to_string())
            }
            word => match Kind::parse(word) {
                Some(kind) => Head::Kind(kind),
                None => Head::Named(word.to_string()),
            },
        };

        let mut term = Term {
            head,
            filters: Vec::new(),
            range: None,
            pick: None,
            text: text.to_string(),
        };

        while !rest.is_empty() {
            if let Some(after) = rest.strip_prefix('[') {
                let end = after.find(']').ok_or_else(|| {
                    Error::bad_args(format!("unclosed '[' in selector term '{text}'"))
                })?;
                for part in split_outside_brackets(&after[..end], ',') {
                    term.filters.push(Filter::parse(part, text, term.kind())?);
                }
                rest = &after[end + 1..];
            } else if let Some(after) = rest.strip_prefix('@') {
                if term.range.is_some() {
                    return Err(Error::bad_args(format!(
                        "selector term '{text}' has two time ranges"
                    )));
                }
                // A time contains colons, so the range runs to the end of the term unless a
                // positional keyword follows: `@00:10-00:20:last` splits at `:last`.
                let end = pick_start(after);
                let (start, stop) = after[..end].split_once('-').ok_or_else(|| {
                    Error::bad_args(format!(
                        "time range '@{}' in '{text}' must be written '@start-end'",
                        &after[..end]
                    ))
                })?;
                let (start, stop) = (start.trim(), stop.trim());
                if start.is_empty() || stop.is_empty() {
                    return Err(Error::bad_args(format!(
                        "time range '@{}' in '{text}' is missing an end",
                        &after[..end]
                    )));
                }
                term.range = Some((start.to_string(), stop.to_string()));
                rest = &after[end..];
            } else if let Some(after) = rest.strip_prefix(':') {
                if term.pick.is_some() {
                    return Err(Error::bad_args(format!(
                        "selector term '{text}' narrows twice"
                    )));
                }
                let end = after.find(['[', '@']).unwrap_or(after.len());
                term.pick = Some(parse_pick(&after[..end], text)?);
                rest = &after[end..];
            } else {
                return Err(Error::bad_args(format!(
                    "unexpected '{rest}' in selector term '{text}'"
                )));
            }
        }
        Ok(term)
    }
}

fn parse_pick(word: &str, term: &str) -> Result<Pick> {
    if word.eq_ignore_ascii_case("first") {
        return Ok(Pick::First);
    }
    if word.eq_ignore_ascii_case("last") {
        return Ok(Pick::Last);
    }
    if let Some(index) = word
        .strip_prefix("nth(")
        .or_else(|| word.strip_prefix("NTH("))
        .and_then(|rest| rest.strip_suffix(')'))
    {
        return index.trim().parse::<usize>().map(Pick::Nth).map_err(|_| {
            Error::bad_args(format!(
                "':nth({index})' in '{term}' needs a zero-based number"
            ))
        });
    }
    Err(Error::bad_args(format!(
        "unknown ':{word}' in '{term}'; the positions are :first, :last and :nth(N)"
    )))
}

/// Index of the `:` that begins a positional keyword, or the end of the text. Only a colon
/// followed by a known keyword counts, so timecode colons survive.
fn pick_start(text: &str) -> usize {
    text.char_indices()
        .filter(|(_, ch)| *ch == ':')
        .find(|(at, _)| {
            let tail = &text[at + 1..];
            ["first", "last", "nth"]
                .iter()
                .any(|word| tail.get(..word.len()).is_some_and(|head| head.eq_ignore_ascii_case(word)))
        })
        .map_or(text.len(), |(at, _)| at)
}

impl Filter {
    fn parse(body: &str, term: &str, kind: Option<Kind>) -> Result<Filter> {
        // Compound operators first: `name^=x` must not split on the bare `=`.
        let (key, op, value) = [AttrOp::Ne, AttrOp::Prefix, AttrOp::Contains, AttrOp::Eq]
            .into_iter()
            .find_map(|op| {
                body.split_once(op.token())
                    .map(|(key, value)| (key.trim(), op, value.trim()))
            })
            .ok_or_else(|| {
                Error::bad_args(format!(
                    "filter '[{}]' in '{term}' needs one of =, !=, ^=, *=",
                    body.trim()
                ))
            })?;
        if key.is_empty() {
            return Err(Error::bad_args(format!(
                "filter '[{}]' in '{term}' has no attribute name",
                body.trim()
            )));
        }
        let value = value.trim_matches(['"', '\'']);
        if value.is_empty() {
            return Err(Error::bad_args(format!(
                "filter '[{}]' in '{term}' has no value",
                body.trim()
            )));
        }
        // An unknown key would match nothing, which reads as "there is no such clip"
        // instead of "you misspelled the attribute". Only checkable with a kind head.
        if let Some(kind) = kind {
            if !kind.attrs().iter().any(|known| key.eq_ignore_ascii_case(known)) {
                return Err(Error::bad_args(format!(
                    "'{key}' is not an attribute of {}; try one of: {}",
                    kind.label(),
                    kind.attrs().join(", ")
                )));
            }
        }
        Ok(Filter {
            key: key.to_string(),
            op,
            value: value.to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Selector {
    pub terms: Vec<Term>,
    text: String,
}

impl Selector {
    pub fn parse(text: &str) -> Result<Selector> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(Error::bad_args("empty selector"));
        }
        let terms = split_outside_brackets(trimmed, ' ')
            .into_iter()
            .map(Term::parse)
            .collect::<Result<Vec<Term>>>()?;
        if terms.is_empty() {
            return Err(Error::bad_args(format!("no terms in selector '{text}'")));
        }
        Ok(Selector {
            terms,
            text: trimmed.to_string(),
        })
    }

    /// The kind every term names, when they agree. `None` for `#name` terms and for a
    /// union of different kinds; ops use it to reject `clip.split --target track[kind=audio]`
    /// before touching the document.
    pub fn kind(&self) -> Option<Kind> {
        let first = self.terms.first()?.kind()?;
        self.terms
            .iter()
            .all(|term| term.kind() == Some(first))
            .then_some(first)
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Split on a separator that is not inside `[…]` or `(…)`, so `marker[name=act two]` and
/// `:nth(2)` survive being split on spaces.
fn split_outside_brackets(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (at, ch) in text.char_indices() {
        match ch {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = depth.saturating_sub(1),
            _ if ch == separator && depth == 0 => {
                let part = text[start..at].trim();
                if !part.is_empty() {
                    parts.push(part);
                }
                start = at + ch.len_utf8();
            }
            _ => {}
        }
    }
    let last = text[start..].trim();
    if !last.is_empty() {
        parts.push(last);
    }
    parts
}

/// One addressable thing, borrowed. `Copy`, so matching never clones a document node.
#[derive(Debug, Clone, Copy)]
enum Item<'a> {
    Clip {
        track: &'a Track,
        clip: &'a Clip,
    },
    Track(&'a Track),
    Cue {
        track: &'a Track,
        cue: &'a CaptionCue,
    },
    Effect {
        track: &'a Track,
        clip: &'a Clip,
        effect: &'a Effect,
    },
    Marker(&'a Marker),
    Asset(&'a Asset),
}

/// The strings an attribute answers to. Two at most, because `clip[track=V1]` and
/// `clip[track=trk_v1]` both have to work.
struct Attr<'a> {
    value: Cow<'a, str>,
    alias: Option<Cow<'a, str>>,
}

impl<'a> Attr<'a> {
    fn new(value: impl Into<Cow<'a, str>>) -> Attr<'a> {
        Attr {
            value: value.into(),
            alias: None,
        }
    }

    fn aliased(value: impl Into<Cow<'a, str>>, alias: impl Into<Cow<'a, str>>) -> Attr<'a> {
        Attr {
            value: value.into(),
            alias: Some(alias.into()),
        }
    }

    fn test(&self, op: AttrOp, wanted: &str) -> bool {
        let primary = op.test(&self.value, wanted);
        match (&self.alias, op) {
            (None, _) => primary,
            // `!=` must fail on every spelling: `clip[track!=V1]` cannot keep a clip just
            // because the track's *id* is not the string "V1".
            (Some(alias), AttrOp::Ne) => primary && op.test(alias, wanted),
            (Some(alias), _) => primary || op.test(alias, wanted),
        }
    }
}

fn boolean(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

fn track_kind(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Video => "video",
        TrackKind::Audio => "audio",
        TrackKind::Caption => "caption",
    }
}

fn asset_kind(kind: AssetKind) -> &'static str {
    match kind {
        AssetKind::Video => "video",
        AssetKind::Audio => "audio",
        AssetKind::Image => "image",
        AssetKind::Font => "font",
        AssetKind::Lut => "lut",
        AssetKind::Subtitle => "subtitle",
        AssetKind::Other => "other",
    }
}

fn source_kind(source: &Source) -> &'static str {
    match source {
        Source::Asset { .. } => "asset",
        Source::Title { .. } => "title",
        Source::Sequence { .. } => "sequence",
        Source::Color { .. } => "color",
        Source::Image { .. } => "image",
        Source::Generator { .. } => "generator",
    }
}

fn generator_name(generator: Generator) -> &'static str {
    match generator {
        Generator::Bars => "bars",
        Generator::Tone => "tone",
        Generator::Countdown => "countdown",
        Generator::FrameNumbers => "frame-numbers",
    }
}

/// A clip's source, addressable by the id it stores and by the human name behind it —
/// an agent writes `clip[source=talk.mp4]`, having never seen the ulid.
fn source_attr<'a>(source: &'a Source, project: &'a Project) -> Attr<'a> {
    match source {
        Source::Asset { asset, .. } | Source::Image { asset } => match project.assets.get(asset) {
            Some(found) => Attr::aliased(asset.as_str(), found.name.as_str()),
            None => Attr::new(asset.as_str()),
        },
        Source::Title { title } => match project.titles.get(title) {
            Some(found) => Attr::aliased(title.as_str(), found.name.as_str()),
            None => Attr::new(title.as_str()),
        },
        Source::Sequence { sequence } => match project.sequences.get(sequence) {
            Some(found) => Attr::aliased(sequence.as_str(), found.name.as_str()),
            None => Attr::new(sequence.as_str()),
        },
        Source::Color { color } => Attr::new(color.to_string()),
        Source::Generator { generator, .. } => Attr::new(generator_name(*generator)),
    }
}

impl<'a> Item<'a> {
    fn kind(self) -> Kind {
        match self {
            Item::Clip { .. } => Kind::Clip,
            Item::Track(_) => Kind::Track,
            Item::Cue { .. } => Kind::Cue,
            Item::Effect { .. } => Kind::Effect,
            Item::Marker(_) => Kind::Marker,
            Item::Asset(_) => Kind::Asset,
        }
    }

    fn id(self) -> &'a str {
        match self {
            Item::Clip { clip, .. } => clip.id.as_str(),
            Item::Track(track) => track.id.as_str(),
            Item::Cue { cue, .. } => cue.id.as_str(),
            Item::Effect { effect, .. } => effect.id.as_str(),
            Item::Marker(marker) => marker.id.as_str(),
            Item::Asset(asset) => asset.id.as_str(),
        }
    }

    fn name(self) -> Option<&'a str> {
        match self {
            Item::Clip { clip, .. } => clip.name.as_deref(),
            Item::Track(track) => Some(track.name.as_str()),
            Item::Marker(marker) => Some(marker.name.as_str()),
            Item::Asset(asset) => Some(asset.name.as_str()),
            Item::Cue { .. } | Item::Effect { .. } => None,
        }
    }

    fn answers_to(self, query: &str) -> bool {
        self.id().eq_ignore_ascii_case(query)
            || self
                .name()
                .is_some_and(|name| name.eq_ignore_ascii_case(query))
    }

    fn in_window(self, window: Span) -> bool {
        match self {
            Item::Clip { clip, .. } | Item::Effect { clip, .. } => clip.span().overlaps(&window),
            Item::Cue { cue, .. } => cue.span.overlaps(&window),
            // A marker is an instant, and a zero-length span overlaps nothing; containment
            // in the half-open window is what "the markers in 00:10-00:20" means.
            Item::Marker(marker) => window.contains(marker.at),
            Item::Track(_) | Item::Asset(_) => false,
        }
    }

    fn attr(self, key: &str, project: &'a Project) -> Option<Attr<'a>> {
        let key = key.to_ascii_lowercase();
        match self {
            Item::Clip { track, clip } => match key.as_str() {
                "id" => Some(Attr::new(clip.id.as_str())),
                "name" => clip.name.as_deref().map(Attr::new),
                "track" => Some(Attr::aliased(track.name.as_str(), track.id.as_str())),
                "source" => Some(source_attr(&clip.source, project)),
                "kind" => Some(Attr::new(source_kind(&clip.source))),
                "enabled" => Some(Attr::new(boolean(clip.enabled))),
                "speed" => Some(Attr::new(clip.speed.to_string())),
                _ => None,
            },
            Item::Track(track) => match key.as_str() {
                "id" => Some(Attr::new(track.id.as_str())),
                "name" => Some(Attr::new(track.name.as_str())),
                "kind" => Some(Attr::new(track_kind(track.kind))),
                "muted" => Some(Attr::new(boolean(track.muted))),
                "solo" => Some(Attr::new(boolean(track.solo))),
                "locked" => Some(Attr::new(boolean(track.locked))),
                "hidden" => Some(Attr::new(boolean(track.hidden))),
                _ => None,
            },
            Item::Cue { track, cue } => match key.as_str() {
                "id" => Some(Attr::new(cue.id.as_str())),
                "text" => Some(Attr::new(cue.text.as_str())),
                "track" => Some(Attr::aliased(track.name.as_str(), track.id.as_str())),
                "style" => cue.style.as_ref().map(|style| Attr::new(style.as_str())),
                _ => None,
            },
            Item::Effect {
                track,
                clip,
                effect,
            } => match key.as_str() {
                "id" => Some(Attr::new(effect.id.as_str())),
                "kind" => Some(Attr::new(effect.kind.as_str())),
                "enabled" => Some(Attr::new(boolean(effect.enabled))),
                "clip" => Some(Attr::aliased(clip.label(), clip.id.as_str())),
                "track" => Some(Attr::aliased(track.name.as_str(), track.id.as_str())),
                _ => None,
            },
            Item::Marker(marker) => match key.as_str() {
                "id" => Some(Attr::new(marker.id.as_str())),
                "name" => Some(Attr::new(marker.name.as_str())),
                "note" => marker.note.as_deref().map(Attr::new),
                _ => None,
            },
            Item::Asset(asset) => match key.as_str() {
                "id" => Some(Attr::new(asset.id.as_str())),
                "name" => Some(Attr::new(asset.name.as_str())),
                "kind" => Some(Attr::new(asset_kind(asset.kind))),
                "hash" => Some(Attr::new(asset.hash.as_str())),
                _ => None,
            },
        }
    }

    /// What a candidate list calls it: the text an agent can paste back as a selector.
    fn label(self) -> String {
        match self {
            Item::Track(track) => track.name.clone(),
            // A cue has no name, and twelve bare ulids help nobody: show what it says.
            Item::Cue { cue, .. } => {
                let text: String = cue.text.chars().take(24).collect();
                format!("#{} \"{}\"", cue.id, text)
            }
            other => format!("#{}", other.name().unwrap_or_else(|| other.id())),
        }
    }

    fn to_match(self) -> Match {
        match self {
            Item::Clip { track, clip } => Match::Clip {
                track: track.id.clone(),
                clip: clip.id.clone(),
            },
            Item::Track(track) => Match::Track(track.id.clone()),
            Item::Cue { track, cue } => Match::Cue {
                track: track.id.clone(),
                cue: cue.id.clone(),
            },
            Item::Effect {
                track,
                clip,
                effect,
            } => Match::Effect {
                track: track.id.clone(),
                clip: clip.id.clone(),
                effect: effect.id.clone(),
            },
            Item::Marker(marker) => Match::Marker(marker.id.clone()),
            Item::Asset(asset) => Match::Asset(asset.id.clone()),
        }
    }
}

/// Everything addressable, in the one order results are ever reported in: track order, then
/// timeline order, then id order. Building it once means a union cannot produce duplicates
/// or reorder anything — the result is always a subset of this list.
fn candidates<'a>(project: &'a Project, sequence: &'a Sequence) -> Vec<Item<'a>> {
    let mut items = Vec::new();
    for track in &sequence.tracks {
        items.push(Item::Track(track));
        // Clips are sorted by start and never overlap; that is a document invariant
        // (`Track::validate`), so their stored order is timeline order.
        for clip in &track.clips {
            items.push(Item::Clip { track, clip });
            for effect in &clip.effects {
                items.push(Item::Effect {
                    track,
                    clip,
                    effect,
                });
            }
        }
        let mut cues: Vec<&CaptionCue> = track.cues.iter().collect();
        cues.sort_by(|a, b| {
            a.span
                .start
                .cmp(&b.span.start)
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        items.extend(cues.into_iter().map(|cue| Item::Cue { track, cue }));
    }
    // Markers and cues carry no ordering invariant of their own — a hand-edited document
    // may list them in any order, and results must not inherit that.
    let mut markers: Vec<&Marker> = sequence.markers.iter().collect();
    markers.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.as_str().cmp(b.id.as_str())));
    items.extend(markers.into_iter().map(Item::Marker));
    items.extend(project.assets.values().map(Item::Asset));
    items
}

fn matches(item: Item<'_>, term: &Term, project: &Project, window: Option<Span>) -> bool {
    match &term.head {
        Head::Any => {}
        Head::Kind(kind) => {
            if item.kind() != *kind {
                return false;
            }
        }
        Head::Named(name) => {
            if !item.answers_to(name) {
                return false;
            }
        }
    }
    if let Some(window) = window {
        if !item.in_window(window) {
            return false;
        }
    }
    term.filters.iter().all(|filter| {
        item.attr(&filter.key, project)
            .is_some_and(|attr| attr.test(filter.op, &filter.value))
    })
}

fn truncated(mut labels: Vec<String>) -> Vec<String> {
    if labels.len() > MAX_CANDIDATES {
        let extra = labels.len() - MAX_CANDIDATES;
        labels.truncate(MAX_CANDIDATES);
        labels.push(format!("+{extra} more"));
    }
    labels
}

/// The error a miss produces. The candidate list is the whole point: it is what lets an
/// agent correct a typo without a round trip.
fn no_match(term: &Term, items: &[Item<'_>]) -> Error {
    let relevant = |item: &Item<'_>| match &term.head {
        Head::Kind(kind) => item.kind() == *kind,
        // A `#name` miss is a clip, track, marker or asset typo; cue and effect ulids
        // would just crowd out the line that helps.
        _ => matches!(
            item.kind(),
            Kind::Clip | Kind::Track | Kind::Marker | Kind::Asset
        ),
    };
    let labels = items
        .iter()
        .filter(|item| relevant(item))
        .map(|item| item.label())
        .collect();
    Error::no_match(
        term.kind().map_or("selector", Kind::label),
        term.text.clone(),
        truncated(labels),
    )
}

/// Resolve a parsed selector against one sequence.
pub fn resolve(
    project: &Project,
    sequence: &SequenceId,
    selector: &Selector,
) -> Result<Vec<Match>> {
    let seq = project.sequence(sequence)?;
    let items = candidates(project, seq);
    let mut picked = vec![false; items.len()];

    for term in &selector.terms {
        let window = match &term.range {
            Some((start, end)) => {
                let bad = |value: &str, error: Error| {
                    Error::bad_args(format!(
                        "bad time '{value}' in selector term '{}': {error}",
                        term.text
                    ))
                };
                let from = Time::parse_with_fps(start, seq.fps).map_err(|e| bad(start, e))?;
                let to = Time::parse_with_fps(end, seq.fps).map_err(|e| bad(end, e))?;
                if to <= from {
                    return Err(Error::bad_args(format!(
                        "time range '@{start}-{end}' in '{}' ends at or before it starts",
                        term.text
                    )));
                }
                Some(Span::new(from, to))
            }
            None => None,
        };

        let mut hits: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| matches(**item, term, project, window))
            .map(|(index, _)| index)
            .collect();
        match term.pick {
            Some(Pick::First) => hits.truncate(1),
            Some(Pick::Last) => {
                let last = hits.pop();
                hits.clear();
                hits.extend(last);
            }
            Some(Pick::Nth(n)) => {
                let at = hits.get(n).copied();
                hits.clear();
                hits.extend(at);
            }
            None => {}
        }
        // Each term must land. A union where one member is a typo would otherwise apply a
        // smaller edit than the agent asked for, silently.
        if hits.is_empty() {
            return Err(no_match(term, &items));
        }
        for index in hits {
            picked[index] = true;
        }
    }

    Ok(items
        .iter()
        .zip(picked)
        .filter(|(_, hit)| *hit)
        .map(|(item, _)| item.to_match())
        .collect())
}

/// Clips named by a selector, in timeline order.
pub fn resolve_clips(
    project: &Project,
    sequence: &SequenceId,
    text: &str,
) -> Result<Vec<(TrackId, ClipId)>> {
    let selector = Selector::parse(text)?;
    let found = resolve(project, sequence, &selector)?;
    let clips: Vec<(TrackId, ClipId)> = found
        .iter()
        .filter_map(|m| match m {
            Match::Clip { track, clip } => Some((track.clone(), clip.clone())),
            _ => None,
        })
        .collect();
    if clips.is_empty() {
        // Expanding a track into its clips would turn `clip.remove --target V1` into "delete
        // everything on V1" on a guess. Say so instead, with the selector that means it.
        if found.iter().all(|m| m.kind() == Kind::Track) {
            return Err(Error::bad_args(format!(
                "selector '{text}' names a track, not clips; write 'clip[track={text}]' for the clips on it"
            )));
        }
        return Err(Error::bad_args(format!(
            "selector '{text}' matched {} {}(s), not clips",
            found.len(),
            found[0].kind().label()
        )));
    }
    Ok(clips)
}

/// A selector that must name exactly one clip. Taking the first of several is how an agent
/// ends up trimming the wrong shot and not finding out.
pub fn resolve_one_clip(
    project: &Project,
    sequence: &SequenceId,
    text: &str,
) -> Result<(TrackId, ClipId)> {
    let mut clips = resolve_clips(project, sequence, text)?;
    if clips.len() > 1 {
        let seq = project.sequence(sequence)?;
        let labels = truncated(
            clips
                .iter()
                .map(|(track, clip)| match seq.find_clip(clip) {
                    Some((track, clip)) => format!("{}/#{}", track.name, clip.label()),
                    None => format!("{track}/#{clip}"),
                })
                .collect(),
        );
        return Err(Error::bad_args(format!(
            "selector '{text}' matched {} clips, expected one: {}",
            clips.len(),
            labels.join(", ")
        )));
    }
    Ok(clips.remove(0))
}

/// Tracks named by a selector, in track order.
pub fn resolve_tracks(
    project: &Project,
    sequence: &SequenceId,
    text: &str,
) -> Result<Vec<TrackId>> {
    let selector = Selector::parse(text)?;
    // A bare token like `V1` is searched across every addressable kind, so a miss comes
    // back listing clips and assets — useless when the caller asked for a track. Re-frame
    // it here, where the expected kind is known.
    let matches = match resolve(project, sequence, &selector) {
        Ok(matches) => matches,
        Err(Error::NoMatch { .. }) => Vec::new(),
        Err(other) => return Err(other),
    };
    let tracks: Vec<TrackId> = matches
        .into_iter()
        .filter_map(|m| match m {
            Match::Track(track) => Some(track),
            _ => None,
        })
        .collect();
    if tracks.is_empty() {
        let seq = project.sequence(sequence)?;
        if seq.tracks.is_empty() {
            return Err(Error::no_match(
                "track",
                text,
                vec![format!(
                    "sequence '{}' has no tracks yet — run `dvs op track.add --kind video`",
                    seq.name
                )],
            ));
        }
        return Err(Error::no_match("track", text, truncated(seq.track_names())));
    }
    Ok(tracks)
}

/// A selector that must name exactly one track.
pub fn resolve_track(project: &Project, sequence: &SequenceId, text: &str) -> Result<TrackId> {
    let mut tracks = resolve_tracks(project, sequence, text)?;
    if tracks.len() > 1 {
        let seq = project.sequence(sequence)?;
        let names: Vec<String> = tracks
            .iter()
            .map(|id| match seq.track(id) {
                Ok(track) => track.name.clone(),
                Err(_) => id.to_string(),
            })
            .collect();
        return Err(Error::bad_args(format!(
            "selector '{text}' matched {} tracks, expected one: {}",
            tracks.len(),
            names.join(", ")
        )));
    }
    Ok(tracks.remove(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{StyleId, TitleId};
    use crate::project::Probe;
    use crate::time::Fps;
    use chrono::Utc;

    fn asset(id: &str, name: &str, kind: AssetKind) -> Asset {
        Asset {
            id: AssetId::from_raw(id),
            name: name.to_string(),
            hash: format!("blake3:{id}"),
            kind,
            probe: Probe::default(),
            proxy: None,
            source_path: None,
            imported: Utc::now(),
            provenance: None,
        }
    }

    fn talk() -> Source {
        Source::Asset {
            asset: AssetId::from_raw("ast_talk"),
            stream: None,
        }
    }

    fn clip(id: &str, name: &str, source: Source, start: i64, end: i64) -> Clip {
        let mut clip = Clip::new(
            source,
            Time::from_secs(start),
            Time::from_secs(end - start),
        );
        clip.id = ClipId::from_raw(id);
        clip.name = Some(name.to_string());
        clip
    }

    fn track(id: &str, name: &str, kind: TrackKind) -> Track {
        let mut track = Track::new(name, kind);
        track.id = TrackId::from_raw(id);
        track
    }

    /// V1: intro [0,10) talk [10,18) outro [18,26, disabled)
    /// V2: title-lower [12,16, a title) slow-subtitle [20,24)
    /// A1: music [0,30), muted track
    /// CC1: one cue [10,13)
    fn fixture() -> (Project, SequenceId) {
        let fps = Fps::new(30, 1).expect("30 fps");
        let mut project = Project::new("promo", fps, [1920, 1080], 48_000);
        let seq_id = project.active_sequence.clone();

        for a in [
            asset("ast_talk", "talk.mp4", AssetKind::Video),
            asset("ast_music", "music.mp3", AssetKind::Audio),
        ] {
            project.assets.insert(a.id.clone(), a);
        }

        let mut v1 = track("trk_v1", "V1", TrackKind::Video);
        let mut talk_clip = clip("clp_talk", "talk", talk(), 10, 18);
        let mut blur = Effect::new("blur");
        blur.id = EffectId::from_raw("fx_blur");
        talk_clip.effects.push(blur);
        let mut outro = clip("clp_outro", "outro", talk(), 18, 26);
        outro.enabled = false;
        v1.clips = vec![
            clip("clp_intro", "intro", talk(), 0, 10),
            talk_clip,
            outro,
        ];

        let mut v2 = track("trk_v2", "V2", TrackKind::Video);
        v2.clips = vec![
            clip(
                "clp_title",
                "title-lower",
                Source::Title {
                    title: TitleId::from_raw("ttl_lower"),
                },
                12,
                16,
            ),
            clip("clp_slow", "slow-subtitle", talk(), 20, 24),
        ];

        let mut a1 = track("trk_a1", "A1", TrackKind::Audio);
        a1.muted = true;
        a1.clips = vec![clip(
            "clp_music",
            "music",
            Source::Asset {
                asset: AssetId::from_raw("ast_music"),
                stream: None,
            },
            0,
            30,
        )];

        let mut cc1 = track("trk_cc1", "CC1", TrackKind::Caption);
        cc1.cues = vec![CaptionCue {
            id: CueId::from_raw("cue_1"),
            span: Span::new(Time::from_secs(10), Time::from_secs(13)),
            text: "so today we ship".to_string(),
            style: Some(StyleId::from_raw("sty_default")),
        }];

        let sequence = project.sequence_mut(&seq_id).expect("active sequence");
        sequence.tracks = vec![v1, v2, a1, cc1];
        sequence.markers = vec![Marker {
            id: MarkerId::from_raw("mk_pricing"),
            at: Time::from_secs(42),
            name: "pricing".to_string(),
            color: None,
            note: None,
        }];
        sequence.validate().expect("fixture is a legal document");

        (project, seq_id)
    }

    /// Readable labels for whatever a selector matched, in the order it returned them.
    fn names(project: &Project, sequence: &SequenceId, text: &str) -> Vec<String> {
        let selector = Selector::parse(text).expect("selector parses");
        let seq = project.sequence(sequence).expect("sequence");
        resolve(project, sequence, &selector)
            .expect("selector resolves")
            .iter()
            .map(|m| match m {
                Match::Clip { clip, .. } => seq
                    .find_clip(clip)
                    .expect("clip is in the sequence")
                    .1
                    .label()
                    .to_string(),
                Match::Track(track) => seq.track(track).expect("track").name.clone(),
                Match::Marker(id) => seq
                    .markers
                    .iter()
                    .find(|marker| &marker.id == id)
                    .expect("marker")
                    .name
                    .clone(),
                Match::Cue { cue, .. } => cue.to_string(),
                Match::Asset(id) => project.asset(id).expect("asset").name.clone(),
                Match::Effect { effect, .. } => effect.to_string(),
            })
            .collect()
    }

    fn failure(project: &Project, sequence: &SequenceId, text: &str) -> Error {
        match Selector::parse(text) {
            Err(error) => error,
            Ok(selector) => {
                resolve(project, sequence, &selector).expect_err("selector should not resolve")
            }
        }
    }

    #[test]
    fn name_track_and_star_address_different_sets() {
        let (project, seq) = fixture();

        assert_eq!(names(&project, &seq, "#talk"), ["talk"]);
        assert_eq!(names(&project, &seq, "clp_intro"), ["intro"]);
        // A bare track name is the track itself, case-insensitively — not its clips.
        assert_eq!(names(&project, &seq, "V1"), ["V1"]);
        assert_eq!(names(&project, &seq, "v1"), ["V1"]);
        assert_eq!(names(&project, &seq, "trk_a1"), ["A1"]);
        // Track order, then timeline order; the disabled clip and the audio clip are clips.
        assert_eq!(
            names(&project, &seq, "*"),
            ["intro", "talk", "outro", "title-lower", "slow-subtitle", "music"]
        );
    }

    #[test]
    fn time_range_is_half_open_and_track_scoped() {
        let (project, seq) = fixture();

        // intro ends exactly at 10, the window start: adjacent, not overlapping.
        // outro starts inside the window; V2's title-lower is inside it but on another track.
        assert_eq!(
            names(&project, &seq, "clip[track=V1]@00:10-00:20"),
            ["talk", "outro"]
        );
        // A clip that starts before the window and ends inside it is in.
        assert_eq!(
            names(&project, &seq, "clip[track=V1]@00:04-00:12"),
            ["intro", "talk"]
        );
        // Without the track filter the window reaches every track — including the music
        // bed, which spans the whole sequence.
        assert_eq!(
            names(&project, &seq, "clip@00:10-00:20"),
            ["talk", "outro", "title-lower", "music"]
        );
        // A bare window narrows nothing but time: the caption cue comes with it, and so
        // does the effect on `talk`, which is inside the window because its clip is.
        assert_eq!(
            names(&project, &seq, "@00:10-00:20"),
            ["talk", "fx_blur", "outro", "title-lower", "music", "cue_1"]
        );
        // A marker is an instant: it is in the window that contains it, and no other.
        assert_eq!(names(&project, &seq, "@00:40-00:50"), ["pricing"]);
        // The window is parsed at the sequence frame rate, so frames are a legal spelling.
        assert_eq!(
            names(&project, &seq, "clip[track=V1]@300f-600f"),
            ["talk", "outro"]
        );
    }

    #[test]
    fn prefix_and_contains_are_different_filters() {
        let (project, seq) = fixture();

        // `slow-subtitle` contains "title" but does not begin with it, so `^=` being
        // anchored is the whole difference between these two answers.
        assert_eq!(names(&project, &seq, "clip[name^=title]"), ["title-lower"]);
        assert_eq!(
            names(&project, &seq, "clip[name*=title]"),
            ["title-lower", "slow-subtitle"]
        );
        assert_eq!(
            names(&project, &seq, "clip[name*=ow]"),
            ["title-lower", "slow-subtitle"]
        );
        assert_eq!(names(&project, &seq, "clip[name=intro]"), ["intro"]);
    }

    #[test]
    fn clip_kind_filters_by_source_variant() {
        let (project, seq) = fixture();

        assert_eq!(names(&project, &seq, "clip[kind=title]"), ["title-lower"]);
        assert_eq!(
            names(&project, &seq, "clip[kind=asset]"),
            ["intro", "talk", "outro", "slow-subtitle", "music"]
        );
    }

    #[test]
    fn attribute_filters_read_the_document() {
        let (project, seq) = fixture();

        assert_eq!(names(&project, &seq, "clip[enabled=false]"), ["outro"]);
        // A track matches under its name or its id, and `!=` has to reject both spellings.
        assert_eq!(names(&project, &seq, "clip[track=trk_v2]"), ["title-lower", "slow-subtitle"]);
        assert_eq!(
            names(&project, &seq, "clip[track!=V1]"),
            ["title-lower", "slow-subtitle", "music"]
        );
        // Assets are addressable by the file name they were imported as.
        assert_eq!(names(&project, &seq, "clip[source=music.mp3]"), ["music"]);
        assert_eq!(names(&project, &seq, "track[kind=audio]"), ["A1"]);
        assert_eq!(names(&project, &seq, "track[muted=true]"), ["A1"]);
        assert_eq!(names(&project, &seq, "marker[name=pricing]"), ["pricing"]);
        assert_eq!(names(&project, &seq, "cue[text*=ship]"), ["cue_1"]);
        assert_eq!(names(&project, &seq, "fx[kind=blur]"), ["fx_blur"]);
    }

    #[test]
    fn positions_narrow_after_filtering() {
        let (project, seq) = fixture();

        assert_eq!(names(&project, &seq, "clip[track=V1]:first"), ["intro"]);
        assert_eq!(names(&project, &seq, "clip[track=V1]:last"), ["outro"]);
        assert_eq!(names(&project, &seq, "clip[track=V1]:nth(1)"), ["talk"]);
        // Zero-based, so :nth(0) and :first are the same position.
        assert_eq!(names(&project, &seq, "clip[track=V1]:nth(0)"), ["intro"]);
        // `:last` is the last of the filtered set, not of the sequence.
        assert_eq!(names(&project, &seq, "clip[track=V2]:last"), ["slow-subtitle"]);
        // A position can follow a window.
        assert_eq!(
            names(&project, &seq, "clip[track=V1]@00:10-00:20:last"),
            ["outro"]
        );

        let error = failure(&project, &seq, "clip[track=V1]:nth(7)");
        assert!(matches!(error, Error::NoMatch { .. }), "got {error:?}");
    }

    #[test]
    fn a_union_returns_both_sets_once() {
        let (project, seq) = fixture();

        assert_eq!(names(&project, &seq, "#intro #music"), ["intro", "music"]);
        // Order is the timeline's, not the selector's.
        assert_eq!(names(&project, &seq, "#music #intro"), ["intro", "music"]);
        // Overlapping terms do not duplicate the clip they share.
        assert_eq!(
            names(&project, &seq, "clip[track=V1] #intro"),
            ["intro", "talk", "outro"]
        );
        // A typo in one member is an error, not a quietly smaller edit.
        let error = failure(&project, &seq, "#intro #musik");
        assert!(
            error.to_string().contains("musik"),
            "the failing term should be named: {error}"
        );
    }

    #[test]
    fn a_miss_lists_the_real_labels() {
        let (project, seq) = fixture();

        let error = failure(&project, &seq, "#intr");
        let Error::NoMatch { candidates, .. } = &error else {
            panic!("expected a no-match, got {error:?}");
        };
        assert!(
            candidates.iter().any(|c| c == "#intro"),
            "candidates should offer the real clip: {candidates:?}"
        );
        assert!(
            candidates.iter().any(|c| c == "V1"),
            "candidates should offer the real tracks: {candidates:?}"
        );
        // A kind head narrows the list to that kind.
        let error = failure(&project, &seq, "track[name=V9]");
        let Error::NoMatch { candidates, kind, .. } = &error else {
            panic!("expected a no-match, got {error:?}");
        };
        assert_eq!(*kind, "track");
        assert_eq!(candidates, &["V1", "V2", "A1", "CC1"]);
    }

    #[test]
    fn candidate_lists_are_truncated() {
        let (mut project, seq) = fixture();
        let sequence = project.sequence_mut(&seq).expect("sequence");
        let extra = track("trk_v3", "V3", TrackKind::Video);
        sequence.tracks.push(extra);
        for index in 0..20i64 {
            let id = format!("clp_fill{index}");
            let filler = clip(
                &id,
                &format!("fill{index}"),
                talk(),
                index * 2,
                index * 2 + 1,
            );
            sequence.tracks.last_mut().expect("V3").clips.push(filler);
        }

        let error = failure(&project, &seq, "clip[name=nope]");
        let Error::NoMatch { candidates, .. } = &error else {
            panic!("expected a no-match, got {error:?}");
        };
        assert_eq!(candidates.len(), MAX_CANDIDATES + 1);
        assert_eq!(candidates[MAX_CANDIDATES], "+14 more");
    }

    #[test]
    fn one_clip_refuses_to_guess() {
        let (project, seq) = fixture();

        assert_eq!(
            resolve_one_clip(&project, &seq, "#talk").expect("one clip"),
            (TrackId::from_raw("trk_v1"), ClipId::from_raw("clp_talk"))
        );

        let error =
            resolve_one_clip(&project, &seq, "clip[track=V1]").expect_err("three clips match");
        let message = error.to_string();
        assert!(message.contains("3 clips"), "count should be named: {message}");
        assert!(message.contains("V1/#intro"), "matches should be listed: {message}");
        assert_eq!(error.exit_code(), crate::error::exit::BAD_ARGS);

        // A track selector is not silently expanded into the clips on it.
        let error = resolve_clips(&project, &seq, "V1").expect_err("a track is not a clip");
        assert!(
            error.to_string().contains("clip[track=V1]"),
            "the error should hand over the selector that works: {error}"
        );
    }

    #[test]
    fn tracks_resolve_by_name_id_and_filter() {
        let (project, seq) = fixture();

        assert_eq!(
            resolve_track(&project, &seq, "A1").expect("by name"),
            TrackId::from_raw("trk_a1")
        );
        assert_eq!(
            resolve_track(&project, &seq, "trk_v2").expect("by id"),
            TrackId::from_raw("trk_v2")
        );
        assert_eq!(
            resolve_track(&project, &seq, "track[kind=audio]").expect("by filter"),
            TrackId::from_raw("trk_a1")
        );
        assert_eq!(
            resolve_tracks(&project, &seq, "track[kind=video]").expect("two video tracks"),
            vec![TrackId::from_raw("trk_v1"), TrackId::from_raw("trk_v2")]
        );

        let error =
            resolve_track(&project, &seq, "track[kind=video]").expect_err("two tracks match");
        assert!(
            error.to_string().contains("2 tracks"),
            "count should be named: {error}"
        );
    }

    #[test]
    fn a_missing_track_is_reported_as_a_track_not_as_an_asset() {
        let (project, seq) = fixture();
        // `V9` is a bare token, so the generic resolver would answer with whatever else it
        // knows about — clips and assets — which tells the caller nothing about tracks.
        let error = resolve_track(&project, &seq, "V9").expect_err("no such track");
        assert_eq!(error.exit_code(), crate::error::exit::NO_MATCH);
        let message = error.to_string();
        assert!(message.starts_with("track 'V9'"), "{message}");
        assert!(message.contains("V1"), "real track names should be listed: {message}");
        assert!(
            !message.contains(".mp4"),
            "asset names are not track candidates: {message}"
        );
    }

    #[test]
    fn an_empty_sequence_points_at_the_op_that_fixes_it() {
        let mut project = Project::new("p", Fps::new(30, 1).expect("fps"), [640, 480], 48_000);
        let seq = project.active_sequence.clone();
        project.sequence_mut(&seq).expect("sequence").tracks.clear();
        let error = resolve_track(&project, &seq, "V1").expect_err("no tracks at all");
        assert!(
            error.to_string().contains("track.add"),
            "a first-run miss should name the fix: {error}"
        );
    }

    #[test]
    fn malformed_selectors_are_bad_args() {
        let (project, seq) = fixture();

        for text in [
            "clip[track]",
            "@bogus",
            ":nth(x)",
            "clip[track=V1",
            "clip[=V1]",
            "clip[track=]",
            "#",
            "",
            "clip:nowhere",
            "clip[trak=V1]",
            "clip[track=V1]@00:20-00:10",
            "clip[track=V1]@99h-nonsense",
        ] {
            let error = failure(&project, &seq, text);
            assert_eq!(
                error.exit_code(),
                crate::error::exit::BAD_ARGS,
                "'{text}' should be bad args, got {error:?}"
            );
        }

        // The message points at the fragment that is wrong.
        assert!(failure(&project, &seq, "clip[track]")
            .to_string()
            .contains("[track]"));
        assert!(failure(&project, &seq, "clip[trak=V1]")
            .to_string()
            .contains("not an attribute of clip"));
        assert!(failure(&project, &seq, ":nth(x)").to_string().contains("nth"));
    }

    #[test]
    fn parsed_shape_is_inspectable() {
        let selector = Selector::parse("clip[track=V1]@00:10-00:20:last").expect("parses");
        assert_eq!(selector.kind(), Some(Kind::Clip));
        let term = &selector.terms[0];
        assert_eq!(term.head, Head::Kind(Kind::Clip));
        assert_eq!(
            term.range,
            Some(("00:10".to_string(), "00:20".to_string()))
        );
        assert_eq!(term.pick, Some(Pick::Last));
        assert_eq!(
            term.filters,
            vec![Filter {
                key: "track".to_string(),
                op: AttrOp::Eq,
                value: "V1".to_string()
            }]
        );

        // A union of one kind still has a kind; a mixed one does not.
        assert_eq!(
            Selector::parse("clip[track=V1] clip[track=V2]")
                .expect("parses")
                .kind(),
            Some(Kind::Clip)
        );
        assert_eq!(
            Selector::parse("clip[track=V1] track[kind=audio]")
                .expect("parses")
                .kind(),
            None
        );
        assert_eq!(Selector::parse("#talk").expect("parses").kind(), None);

        // Bracket contents survive the union split, so a name may contain a space.
        let spaced = Selector::parse("marker[name=act two]").expect("parses");
        assert_eq!(spaced.terms.len(), 1);
        assert_eq!(spaced.terms[0].filters[0].value, "act two");
    }
}
