//! The document model. `project.json` is this, verbatim.
//!
//! Three invariants hold everywhere and are what the rest of the engine may assume:
//!
//! 1. **Clips on a track never overlap and are sorted by `start`.** An overlap is not a
//!    state, it is a transition, expressed as `transitionIn` on the later clip. Ops cannot
//!    produce an overlap and [`Track::validate`] rejects one that arrives by hand-editing.
//! 2. **Every position and duration is exact** ([`Time`]), never a float.
//! 3. **Nothing here holds media.** Pixels and samples live in the content-addressed asset
//!    store, so `project.json` stays small, diffable, and cheap to snapshot for undo.

use crate::color::Rgba;
use crate::error::{Error, Result};
use crate::ids::*;
use crate::time::{Fps, Rat, Span, Time};
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Bumped when a change is not backward-compatible. Readers refuse a higher number rather
/// than silently misinterpreting fields.
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    #[serde(rename = "degenVideo")]
    pub format: u32,
    pub id: ProjectId,
    pub name: String,
    pub created: DateTime<Utc>,
    pub modified: DateTime<Utc>,
    pub active_sequence: SequenceId,
    #[serde(default)]
    pub assets: IndexMap<AssetId, Asset>,
    #[serde(default)]
    pub sequences: IndexMap<SequenceId, Sequence>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub titles: IndexMap<TitleId, Title>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub styles: IndexMap<StyleId, CaptionStyle>,
    /// What produced the renders in this project. Encoded bytes depend on the ffmpeg build,
    /// so provenance is recorded rather than assumed.
    #[serde(default)]
    pub tools: Tools,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Tools {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ffmpeg: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub encoders: Vec<String>,
}

impl Project {
    /// A new project with one empty sequence, which becomes the active one.
    pub fn new(name: impl Into<String>, fps: Fps, size: [u32; 2], sample_rate: u32) -> Self {
        let now = Utc::now();
        let sequence = Sequence::new("main", fps, size, sample_rate);
        let seq_id = sequence.id.clone();
        let mut sequences = IndexMap::new();
        sequences.insert(seq_id.clone(), sequence);
        Project {
            format: FORMAT_VERSION,
            id: ProjectId::new(),
            name: name.into(),
            created: now,
            modified: now,
            active_sequence: seq_id,
            assets: IndexMap::new(),
            sequences,
            titles: IndexMap::new(),
            styles: IndexMap::new(),
            tools: Tools::default(),
        }
    }

    pub fn check_format(&self) -> Result<()> {
        if self.format > FORMAT_VERSION {
            return Err(Error::op(format!(
                "project format {} is newer than this build supports ({FORMAT_VERSION}); upgrade dvs",
                self.format
            )));
        }
        Ok(())
    }

    /// Resolve a sequence by id or name; `None` means the active sequence.
    pub fn resolve_sequence(&self, query: Option<&str>) -> Result<SequenceId> {
        let Some(query) = query else {
            return Ok(self.active_sequence.clone());
        };
        if self.sequences.contains_key(&SequenceId::from_raw(query)) {
            return Ok(SequenceId::from_raw(query));
        }
        let matches: Vec<&Sequence> = self
            .sequences
            .values()
            .filter(|seq| seq.name == query)
            .collect();
        match matches.as_slice() {
            [one] => Ok(one.id.clone()),
            [] => Err(Error::no_match(
                "sequence",
                query,
                self.sequences.values().map(|s| s.name.clone()).collect(),
            )),
            many => Err(Error::bad_args(format!(
                "sequence name '{query}' is ambiguous ({} matches); use an id",
                many.len()
            ))),
        }
    }

    pub fn sequence(&self, id: &SequenceId) -> Result<&Sequence> {
        self.sequences.get(id).ok_or_else(|| {
            Error::no_match(
                "sequence",
                id.as_str(),
                self.sequences.keys().map(|k| k.to_string()).collect(),
            )
        })
    }

    pub fn sequence_mut(&mut self, id: &SequenceId) -> Result<&mut Sequence> {
        let candidates: Vec<String> = self.sequences.keys().map(|k| k.to_string()).collect();
        self.sequences
            .get_mut(id)
            .ok_or_else(|| Error::no_match("sequence", id.as_str(), candidates))
    }

    /// Resolve an asset by id, name, or file stem.
    pub fn resolve_asset(&self, query: &str) -> Result<AssetId> {
        let direct = AssetId::from_raw(query);
        if self.assets.contains_key(&direct) {
            return Ok(direct);
        }
        let matches: Vec<&Asset> = self
            .assets
            .values()
            .filter(|asset| {
                asset.name == query
                    || std::path::Path::new(&asset.name)
                        .file_stem()
                        .is_some_and(|stem| stem == query)
            })
            .collect();
        match matches.as_slice() {
            [one] => Ok(one.id.clone()),
            [] => Err(Error::no_match(
                "asset",
                query,
                self.assets.values().map(|a| a.name.clone()).collect(),
            )),
            many => Err(Error::bad_args(format!(
                "asset name '{query}' is ambiguous ({} matches); use an id",
                many.len()
            ))),
        }
    }

    pub fn asset(&self, id: &AssetId) -> Result<&Asset> {
        self.assets.get(id).ok_or_else(|| {
            Error::no_match(
                "asset",
                id.as_str(),
                self.assets.values().map(|a| a.name.clone()).collect(),
            )
        })
    }

    /// Every asset hash the document still points at. The complement is what `gc` prunes.
    pub fn referenced_hashes(&self) -> Vec<String> {
        self.assets.values().map(|a| a.hash.clone()).collect()
    }

    pub fn touch(&mut self) {
        self.modified = Utc::now();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AssetKind {
    Video,
    Audio,
    Image,
    Font,
    Lut,
    Subtitle,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Asset {
    pub id: AssetId,
    /// Original file name, kept for human recognition and for `relink`.
    pub name: String,
    /// `blake3:<hex>` of the imported bytes. The store path is derived from this.
    pub hash: String,
    pub kind: AssetKind,
    pub probe: Probe,
    /// Relative path of a CFR, low-resolution proxy under `cache/`, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Where it was imported from, for `relink` after a move.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    pub imported: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
}

/// How a generated asset came to exist. Present only for AI output, so a later reader can
/// tell a shot from a generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Provenance {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    pub duration: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoStream>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioStream>,
    /// Variable frame rate. Phone and screen recordings are routinely VFR, which breaks
    /// frame-exact editing; import generates a CFR proxy and lint reports it.
    #[serde(default)]
    pub vfr: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub container: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoStream {
    pub stream_index: u32,
    /// Storage size, before rotation metadata is applied.
    pub size: [u32; 2],
    pub fps: Fps,
    pub codec: String,
    pub pix_fmt: String,
    #[serde(default)]
    pub color_range: ColorRange,
    #[serde(default)]
    pub color_matrix: ColorMatrix,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_primaries: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer: Option<String>,
    /// Display rotation from container metadata, in degrees counter-clockwise.
    #[serde(default)]
    pub rotation: i32,
    /// Sample aspect ratio. Anamorphic sources are rare but silently wrong when ignored.
    #[serde(default)]
    pub sar: Rat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_rate: Option<u64>,
}

impl VideoStream {
    /// Size as displayed: rotation and sample aspect applied.
    pub fn display_size(&self) -> [u32; 2] {
        let [w, h] = self.size;
        let w = (w as f64 * self.sar.as_f64()).round().max(1.0) as u32;
        if self.rotation.rem_euclid(180) == 90 {
            [h, w]
        } else {
            [w, h]
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AudioStream {
    pub stream_index: u32,
    pub rate: u32,
    pub channels: u16,
    pub codec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_rate: Option<u64>,
}

/// `tv` (limited, 16–235) vs `pc` (full, 0–255). Treating one as the other is the most
/// common silent wrong-colors bug in a homemade pipeline, so it is explicit and probed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ColorRange {
    #[default]
    Unknown,
    Tv,
    Pc,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ColorMatrix {
    #[default]
    Unknown,
    Bt709,
    Bt601,
    Bt2020Ncl,
    Smpte240m,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Sequence {
    pub id: SequenceId,
    pub name: String,
    pub fps: Fps,
    pub size: [u32; 2],
    pub sample_rate: u32,
    #[serde(default = "default_channels")]
    pub channels: u16,
    #[serde(default)]
    pub background: Rgba,
    #[serde(default)]
    pub tracks: Vec<Track>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub markers: Vec<Marker>,
}

fn default_channels() -> u16 {
    2
}

impl Sequence {
    pub fn new(name: impl Into<String>, fps: Fps, size: [u32; 2], sample_rate: u32) -> Self {
        Sequence {
            id: SequenceId::new(),
            name: name.into(),
            fps,
            size,
            sample_rate,
            channels: 2,
            background: Rgba::BLACK,
            tracks: Vec::new(),
            markers: Vec::new(),
        }
    }

    /// End of the last clip on any track. The render length.
    pub fn duration(&self) -> Time {
        self.tracks
            .iter()
            .filter_map(|track| track.clips.last().map(|clip| clip.end()))
            .chain(self.tracks.iter().flat_map(|track| {
                track.cues.iter().map(|cue| cue.span.end)
            }))
            .max()
            .unwrap_or(Time::ZERO)
    }

    pub fn frame_count(&self) -> i64 {
        self.duration().frame_ceil(self.fps)
    }

    pub fn track(&self, id: &TrackId) -> Result<&Track> {
        self.tracks
            .iter()
            .find(|track| &track.id == id)
            .ok_or_else(|| Error::no_match("track", id.as_str(), self.track_names()))
    }

    pub fn track_mut(&mut self, id: &TrackId) -> Result<&mut Track> {
        let names = self.track_names();
        self.tracks
            .iter_mut()
            .find(|track| &track.id == id)
            .ok_or_else(|| Error::no_match("track", id.as_str(), names))
    }

    /// Holes on `track` that no other track of the same kind fills.
    ///
    /// A hole is only a problem if the viewer sees or hears it. An overlay track — a lower
    /// third on V2, a music bed on A2 — is empty most of the time by construction, so
    /// reporting every one of its holes is how a diagnostic trains its reader to ignore it.
    pub fn uncovered_gaps(&self, track: &Track) -> Vec<Span> {
        track
            .gaps(self.duration())
            .into_iter()
            .filter(|hole| !self.covered_elsewhere(track, *hole))
            .collect()
    }

    /// Whether every instant of `span` carries enabled content on another track of the same
    /// kind. Partial coverage is not coverage: the uncovered part is what shows.
    pub fn covered_elsewhere(&self, track: &Track, span: Span) -> bool {
        let mut cursor = span.start;
        while cursor < span.end {
            let reach = self
                .tracks
                .iter()
                .filter(|other| other.id != track.id && other.kind == track.kind && !other.hidden)
                .flat_map(|other| other.clips.iter())
                .filter(|clip| clip.enabled && clip.span().contains(cursor))
                .map(|clip| clip.end())
                .max();
            match reach {
                Some(end) => cursor = end,
                None => return false,
            }
        }
        true
    }

    pub fn track_names(&self) -> Vec<String> {
        self.tracks.iter().map(|t| t.name.clone()).collect()
    }

    /// Resolve a track by id or name (`V1`, `A2`).
    pub fn resolve_track(&self, query: &str) -> Result<TrackId> {
        if let Some(track) = self
            .tracks
            .iter()
            .find(|t| t.id.as_str() == query || t.name.eq_ignore_ascii_case(query))
        {
            return Ok(track.id.clone());
        }
        Err(Error::no_match("track", query, self.track_names()))
    }

    /// Default name for the next track of a kind: `V1`, `V2`, `A1`, `CC1`.
    pub fn next_track_name(&self, kind: TrackKind) -> String {
        let prefix = kind.name_prefix();
        let used = self
            .tracks
            .iter()
            .filter(|t| t.kind == kind)
            .filter_map(|t| t.name.strip_prefix(prefix)?.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        format!("{prefix}{}", used + 1)
    }

    pub fn find_clip(&self, id: &ClipId) -> Option<(&Track, &Clip)> {
        self.tracks
            .iter()
            .find_map(|track| track.clips.iter().find(|c| &c.id == id).map(|c| (track, c)))
    }

    pub fn find_clip_mut(&mut self, id: &ClipId) -> Option<(&mut Track, usize)> {
        for track in &mut self.tracks {
            if let Some(index) = track.clips.iter().position(|c| &c.id == id) {
                return Some((track, index));
            }
        }
        None
    }

    pub fn clip_ids(&self) -> Vec<String> {
        self.tracks
            .iter()
            .flat_map(|track| {
                track
                    .clips
                    .iter()
                    .map(|clip| clip.label().to_string())
            })
            .collect()
    }

    pub fn validate(&self) -> Result<()> {
        if self.size[0] == 0 || self.size[1] == 0 {
            return Err(Error::op(format!(
                "sequence '{}' has a zero dimension",
                self.name
            )));
        }
        for track in &self.tracks {
            track.validate(&self.name)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TrackKind {
    Video,
    Audio,
    Caption,
}

impl TrackKind {
    pub fn name_prefix(self) -> &'static str {
        match self {
            TrackKind::Video => "V",
            TrackKind::Audio => "A",
            TrackKind::Caption => "CC",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub id: TrackId,
    pub name: String,
    pub kind: TrackKind,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub solo: bool,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub hidden: bool,
    /// Track-level trim in decibels, applied after clip gain.
    #[serde(default)]
    pub gain_db: f32,
    /// −1 hard left, 0 center, +1 hard right.
    #[serde(default)]
    pub pan: f32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clips: Vec<Clip>,
    /// Caption tracks carry cues instead of clips.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cues: Vec<CaptionCue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<StyleId>,
}

impl Track {
    pub fn new(name: impl Into<String>, kind: TrackKind) -> Self {
        Track {
            id: TrackId::new(),
            name: name.into(),
            kind,
            muted: false,
            solo: false,
            locked: false,
            hidden: false,
            gain_db: 0.0,
            pan: 0.0,
            clips: Vec::new(),
            cues: Vec::new(),
            style: None,
        }
    }

    /// Insert keeping the sorted invariant. Callers that need ripple behaviour shift the
    /// following clips first; this only places.
    pub fn place(&mut self, clip: Clip) -> usize {
        let index = self
            .clips
            .partition_point(|existing| existing.start <= clip.start);
        self.clips.insert(index, clip);
        index
    }

    pub fn clip_at(&self, at: Time) -> Option<&Clip> {
        self.clips.iter().find(|clip| clip.span().contains(at))
    }

    /// Clips overlapping a span, in timeline order.
    pub fn clips_in(&self, span: Span) -> impl Iterator<Item = &Clip> {
        self.clips
            .iter()
            .filter(move |clip| clip.span().overlaps(&span))
    }

    /// Holes between clips inside `[0, until)`. A hole on a video track renders as black,
    /// which is almost never intended — the `gap` lint reports these.
    pub fn gaps(&self, until: Time) -> Vec<Span> {
        let mut gaps = Vec::new();
        let mut cursor = Time::ZERO;
        for clip in &self.clips {
            if clip.start > cursor {
                gaps.push(Span::new(cursor, clip.start));
            }
            cursor = cursor.max(clip.end());
        }
        if cursor < until {
            gaps.push(Span::new(cursor, until));
        }
        gaps
    }

    pub fn validate(&self, sequence: &str) -> Result<()> {
        let mut previous: Option<&Clip> = None;
        for clip in &self.clips {
            if !clip.duration.is_positive() {
                return Err(Error::op(format!(
                    "clip '{}' on {sequence}/{} has non-positive duration {}",
                    clip.label(),
                    self.name,
                    clip.duration
                )));
            }
            if let Some(prev) = previous {
                if clip.start < prev.end() {
                    return Err(Error::op(format!(
                        "clips '{}' and '{}' overlap on {sequence}/{}; an overlap must be expressed as a transition",
                        prev.label(),
                        clip.label(),
                        self.name
                    )));
                }
            }
            previous = Some(clip);
        }
        Ok(())
    }
}

/// What a clip shows or plays.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Source {
    /// Media from the asset store.
    Asset {
        asset: AssetId,
        /// Stream to use when a file has several; defaults to the probed primary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream: Option<u32>,
    },
    /// A title document, rendered per frame.
    Title { title: TitleId },
    /// Another sequence, composited as a clip. This is nesting.
    Sequence { sequence: SequenceId },
    /// A flat color.
    Color { color: Rgba },
    /// A still image from the asset store.
    Image { asset: AssetId },
    /// A synthetic generator: bars, tone, countdown.
    Generator {
        generator: Generator,
        #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
        params: serde_json::Map<String, serde_json::Value>,
    },
}

impl Source {
    pub fn asset_id(&self) -> Option<&AssetId> {
        match self {
            Source::Asset { asset, .. } | Source::Image { asset } => Some(asset),
            _ => None,
        }
    }

    /// Whether the source has content to composite visually.
    pub fn is_visual(&self) -> bool {
        !matches!(self, Source::Generator { generator, .. } if *generator == Generator::Tone)
    }

    pub fn describe(&self) -> String {
        match self {
            Source::Asset { asset, .. } => asset.to_string(),
            Source::Title { title } => title.to_string(),
            Source::Sequence { sequence } => sequence.to_string(),
            Source::Color { color } => color.to_string(),
            Source::Image { asset } => asset.to_string(),
            Source::Generator { generator, .. } => format!("{generator:?}").to_lowercase(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Generator {
    /// SMPTE-style color bars.
    Bars,
    /// 1 kHz sine, for sync and level checks.
    Tone,
    /// Numeric countdown, for slates.
    Countdown,
    /// Frame index burned into the frame, for verifying frame-exactness.
    FrameNumbers,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Clip {
    pub id: ClipId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub source: Source,
    /// Position on the timeline.
    pub start: Time,
    /// Timeline duration. Canonical; the source out-point is derived from
    /// `source_in + duration * speed`, so retiming cannot desynchronise the two.
    pub duration: Time,
    #[serde(default)]
    pub source_in: Time,
    #[serde(default)]
    pub speed: Rat,
    #[serde(default)]
    pub reverse: bool,
    #[serde(default)]
    pub transform: Transform,
    #[serde(default = "unit")]
    pub opacity: f32,
    #[serde(default)]
    pub blend: Blend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<Crop>,
    #[serde(default)]
    pub fit: Fit,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<Effect>,
    /// Animated parameters, keyed by dotted path: `opacity`, `transform.scale`,
    /// `fx.<effectId>.<param>`. Times are clip-local.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keyframes: BTreeMap<String, Vec<Keyframe>>,
    /// Transition from the previous clip into this one. Its duration is taken out of both
    /// neighbours' visible time; the clips themselves stay non-overlapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition_in: Option<Transition>,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub pan: f32,
    #[serde(default)]
    pub fade_in: Time,
    #[serde(default)]
    pub fade_out: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ducking: Option<Ducking>,
    /// The a/v counterpart of this clip; trims and moves apply to both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<ClipId>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn unit() -> f32 {
    1.0
}

fn yes() -> bool {
    true
}

impl Clip {
    pub fn new(source: Source, start: Time, duration: Time) -> Self {
        Clip {
            id: ClipId::new(),
            name: None,
            source,
            start,
            duration,
            source_in: Time::ZERO,
            speed: Rat::ONE,
            reverse: false,
            transform: Transform::default(),
            opacity: 1.0,
            blend: Blend::Normal,
            crop: None,
            fit: Fit::Contain,
            effects: Vec::new(),
            keyframes: BTreeMap::new(),
            transition_in: None,
            gain_db: 0.0,
            pan: 0.0,
            fade_in: Time::ZERO,
            fade_out: Time::ZERO,
            ducking: None,
            link: None,
            enabled: true,
        }
    }

    pub fn end(&self) -> Time {
        self.start + self.duration
    }

    pub fn span(&self) -> Span {
        Span::from_duration(self.start, self.duration)
    }

    /// What an error message calls this clip: its name if it has one, else its id.
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or_else(|| self.id.as_str())
    }

    /// Source media consumed, in source time. Retimed clips consume `duration * speed`.
    pub fn source_span(&self) -> Span {
        Span::from_duration(self.source_in, self.duration * self.speed)
    }

    /// Map a timeline instant to the source instant to decode. Reverse plays the same
    /// source span backwards, so the last output frame is the first source frame.
    pub fn source_time(&self, at: Time) -> Time {
        let local = at - self.start;
        let consumed = self.duration * self.speed;
        if self.reverse {
            self.source_in + consumed - local * self.speed
        } else {
            self.source_in + local * self.speed
        }
    }

    /// A keyframed parameter at a timeline instant, falling back to the static value.
    pub fn param_at(&self, path: &str, static_value: f64, at: Time) -> f64 {
        match self.keyframes.get(path) {
            Some(keys) if !keys.is_empty() => eval_keyframes(keys, at - self.start),
            _ => static_value,
        }
    }

    pub fn effect(&self, id: &EffectId) -> Option<&Effect> {
        self.effects.iter().find(|fx| &fx.id == id)
    }

    /// Multiplier applied by the clip's own fades at a timeline instant. Audio and video
    /// both use it; for video it multiplies opacity.
    pub fn fade_gain(&self, at: Time) -> f32 {
        let local = at - self.start;
        let mut gain = 1.0f32;
        if self.fade_in.is_positive() && local < self.fade_in {
            gain *= (local.as_secs_f64() / self.fade_in.as_secs_f64()).clamp(0.0, 1.0) as f32;
        }
        let from_end = self.duration - local;
        if self.fade_out.is_positive() && from_end < self.fade_out {
            gain *= (from_end.as_secs_f64() / self.fade_out.as_secs_f64()).clamp(0.0, 1.0) as f32;
        }
        gain
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Transform {
    /// Offset from the anchor position, in sequence pixels.
    #[serde(default)]
    pub pos: [f32; 2],
    #[serde(default = "unit_scale")]
    pub scale: [f32; 2],
    /// Clockwise degrees.
    #[serde(default)]
    pub rotation: f32,
    /// Normalized anchor within the source rect; `[0.5, 0.5]` is its center.
    #[serde(default = "center")]
    pub anchor: [f32; 2],
}

fn unit_scale() -> [f32; 2] {
    [1.0, 1.0]
}

fn center() -> [f32; 2] {
    [0.5, 0.5]
}

impl Default for Transform {
    fn default() -> Self {
        Transform {
            pos: [0.0, 0.0],
            scale: [1.0, 1.0],
            rotation: 0.0,
            anchor: [0.5, 0.5],
        }
    }
}

/// How source pixels are mapped into the sequence frame before `transform` applies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Fit {
    /// Fit inside, preserving aspect; letterbox or pillarbox.
    #[default]
    Contain,
    /// Fill the frame, preserving aspect; crop the overflow.
    Cover,
    /// Ignore aspect.
    Stretch,
    /// Pixel for pixel, centered.
    None,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Blend {
    #[default]
    Normal,
    Add,
    Multiply,
    Screen,
    Overlay,
    SoftLight,
}

/// Fractions of the source rect removed from each edge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Crop {
    #[serde(default)]
    pub left: f32,
    #[serde(default)]
    pub top: f32,
    #[serde(default)]
    pub right: f32,
    #[serde(default)]
    pub bottom: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Effect {
    pub id: EffectId,
    /// Registered effect kind, e.g. `color.grade`, `blur`, `chroma-key`.
    pub kind: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub params: serde_json::Map<String, serde_json::Value>,
}

impl Effect {
    pub fn new(kind: impl Into<String>) -> Self {
        Effect {
            id: EffectId::new(),
            kind: kind.into(),
            enabled: true,
            params: serde_json::Map::new(),
        }
    }

    pub fn number(&self, key: &str, fallback: f64) -> f64 {
        self.params
            .get(key)
            .and_then(|value| value.as_f64())
            .unwrap_or(fallback)
    }

    pub fn string(&self, key: &str) -> Option<&str> {
        self.params.get(key).and_then(|value| value.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Keyframe {
    /// Clip-local time.
    pub at: Time,
    pub value: f64,
    /// Easing from this key to the next.
    #[serde(default)]
    pub easing: Easing,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Easing {
    /// Hold the value until the next key, then jump. For discrete parameters.
    Hold,
    #[default]
    Linear,
    EaseIn,
    EaseOut,
    EaseInOut,
}

impl Easing {
    /// Shape a normalized 0..1 progress.
    pub fn apply(self, t: f64) -> f64 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Easing::Hold => 0.0,
            Easing::Linear => t,
            Easing::EaseIn => t * t,
            Easing::EaseOut => t * (2.0 - t),
            Easing::EaseInOut => {
                if t < 0.5 {
                    2.0 * t * t
                } else {
                    -1.0 + (4.0 - 2.0 * t) * t
                }
            }
        }
    }
}

/// Evaluate a keyframe list at a clip-local time. Before the first key and after the last,
/// the value is held — extrapolating an animation past its keys is never what was meant.
pub fn eval_keyframes(keys: &[Keyframe], local: Time) -> f64 {
    match keys {
        [] => 0.0,
        [only] => only.value,
        _ => {
            if local <= keys[0].at {
                return keys[0].value;
            }
            if let Some(last) = keys.last() {
                if local >= last.at {
                    return last.value;
                }
            }
            let index = keys.partition_point(|key| key.at <= local).max(1) - 1;
            let a = &keys[index];
            let b = &keys[index + 1];
            let span = (b.at - a.at).as_secs_f64();
            if span <= 0.0 {
                return b.value;
            }
            let progress = a.easing.apply((local - a.at).as_secs_f64() / span);
            a.value + (b.value - a.value) * progress
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Transition {
    pub kind: TransitionKind,
    pub duration: Time,
    #[serde(default)]
    pub easing: Easing,
    /// Direction for wipes, slides and pushes.
    #[serde(default)]
    pub direction: Direction,
    /// Color for `dip`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Rgba>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TransitionKind {
    /// No blend. Present so an explicit cut can override an inherited default.
    #[default]
    Cut,
    /// Cross dissolve.
    Dissolve,
    /// Dip to a color and back.
    Dip,
    /// Hard edge sweeping across.
    Wipe,
    /// Incoming clip slides over the outgoing one.
    Slide,
    /// Incoming clip pushes the outgoing one off frame.
    Push,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    #[default]
    Left,
    Right,
    Up,
    Down,
}

/// Sidechain ducking: drop this clip's level while another track has signal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Ducking {
    /// Track whose signal triggers the duck, usually dialogue.
    pub against: TrackId,
    /// Gain reduction in dB while the trigger is active, e.g. `-12`.
    pub by: f32,
    #[serde(default = "default_attack")]
    pub attack: Time,
    #[serde(default = "default_release")]
    pub release: Time,
    /// Trigger threshold in dBFS.
    #[serde(default = "default_threshold")]
    pub threshold: f32,
}

fn default_attack() -> Time {
    Time::new(1, 5).expect("1/5 is valid")
}

fn default_release() -> Time {
    Time::new(1, 2).expect("1/2 is valid")
}

fn default_threshold() -> f32 {
    -30.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Marker {
    pub id: MarkerId,
    pub at: Time,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Rgba>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CaptionCue {
    pub id: CueId,
    pub span: Span,
    /// Display text; `\n` separates lines.
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<StyleId>,
}

impl CaptionCue {
    /// Characters per second, the readability metric broadcast specs are written in.
    pub fn chars_per_second(&self) -> f64 {
        let seconds = self.span.duration().as_secs_f64();
        if seconds <= 0.0 {
            return f64::INFINITY;
        }
        self.text.chars().filter(|c| !c.is_whitespace()).count() as f64 / seconds
    }

    pub fn lines(&self) -> usize {
        self.text.lines().count().max(1)
    }
}

/// A title: an SVG document with optional `{{field}}` substitutions. SVG is the interchange
/// format degen-paint already writes, so a vector document authored there drops in here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Title {
    pub id: TitleId,
    pub name: String,
    pub size: [u32; 2],
    /// SVG source. `{{field}}` placeholders are replaced from `fields` at render time.
    pub svg: String,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub fields: IndexMap<String, String>,
}

impl Title {
    /// SVG with `{{field}}` substituted and XML-escaped.
    pub fn resolved_svg(&self) -> String {
        let mut out = self.svg.clone();
        for (key, value) in &self.fields {
            out = out.replace(&format!("{{{{{key}}}}}"), &escape_xml(value));
        }
        out
    }

    /// Text runs in the resolved document, for the digest and the overflow lint.
    pub fn texts(&self) -> Vec<String> {
        self.fields.values().cloned().collect()
    }
}

pub fn escape_xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CaptionStyle {
    pub id: StyleId,
    pub name: String,
    #[serde(default = "default_font")]
    pub font: String,
    #[serde(default = "default_caption_size")]
    pub size_px: f32,
    #[serde(default = "white")]
    pub color: Rgba,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outline: Option<Rgba>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Rgba>,
    #[serde(default)]
    pub position: CaptionPosition,
    /// Fraction of the frame considered safe for text.
    #[serde(default = "default_safe_area")]
    pub safe_area: f32,
    /// Readability ceiling for the `caption-too-fast` lint.
    #[serde(default = "default_max_cps")]
    pub max_cps: f64,
    #[serde(default = "default_max_lines")]
    pub max_lines: usize,
}

fn default_font() -> String {
    "sans-serif".to_string()
}

fn default_caption_size() -> f32 {
    48.0
}

fn white() -> Rgba {
    Rgba::WHITE
}

fn default_safe_area() -> f32 {
    0.9
}

fn default_max_cps() -> f64 {
    20.0
}

fn default_max_lines() -> usize {
    2
}

impl CaptionStyle {
    pub fn named(name: impl Into<String>) -> Self {
        CaptionStyle {
            id: StyleId::new(),
            name: name.into(),
            font: default_font(),
            size_px: default_caption_size(),
            color: Rgba::WHITE,
            outline: Some(Rgba::BLACK),
            background: None,
            position: CaptionPosition::Bottom,
            safe_area: default_safe_area(),
            max_cps: default_max_cps(),
            max_lines: default_max_lines(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CaptionPosition {
    Top,
    Middle,
    #[default]
    Bottom,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fps() -> Fps {
        Fps::new(30, 1).unwrap()
    }

    fn clip(start: i64, duration: i64) -> Clip {
        Clip::new(
            Source::Color {
                color: Rgba::WHITE,
            },
            Time::from_secs(start),
            Time::from_secs(duration),
        )
    }

    #[test]
    fn placing_keeps_clips_sorted() {
        let mut track = Track::new("V1", TrackKind::Video);
        track.place(clip(4, 2));
        track.place(clip(0, 2));
        track.place(clip(2, 2));
        let starts: Vec<f64> = track.clips.iter().map(|c| c.start.as_secs_f64()).collect();
        assert_eq!(starts, vec![0.0, 2.0, 4.0]);
        track.validate("main").unwrap();
    }

    #[test]
    fn overlapping_clips_are_rejected_with_both_labels() {
        let mut track = Track::new("V1", TrackKind::Video);
        let mut first = clip(0, 5);
        first.name = Some("intro".into());
        let mut second = clip(2, 5);
        second.name = Some("body".into());
        track.clips = vec![first, second];
        let err = track.validate("main").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("intro") && message.contains("body"), "{message}");
    }

    #[test]
    fn gaps_report_holes_and_the_tail() {
        let mut track = Track::new("V1", TrackKind::Video);
        track.place(clip(0, 2));
        track.place(clip(3, 2));
        let gaps = track.gaps(Time::from_secs(7));
        assert_eq!(gaps.len(), 2);
        assert_eq!(gaps[0], Span::new(Time::from_secs(2), Time::from_secs(3)));
        assert_eq!(gaps[1], Span::new(Time::from_secs(5), Time::from_secs(7)));
    }

    #[test]
    fn retimed_clip_maps_timeline_to_source() {
        let mut c = clip(10, 4);
        c.source_in = Time::from_secs(2);
        c.speed = Rat::new(2, 1).unwrap();
        // 4s of timeline at 2x consumes 8s of source.
        assert_eq!(c.source_span().duration(), Time::from_secs(8));
        assert_eq!(c.source_time(Time::from_secs(10)), Time::from_secs(2));
        assert_eq!(c.source_time(Time::from_secs(12)), Time::from_secs(6));
        c.reverse = true;
        assert_eq!(c.source_time(Time::from_secs(10)), Time::from_secs(10));
        assert_eq!(c.source_time(Time::from_secs(14)), Time::from_secs(2));
    }

    #[test]
    fn keyframes_hold_outside_their_range_and_ease_inside() {
        let keys = vec![
            Keyframe {
                at: Time::from_secs(0),
                value: 0.0,
                easing: Easing::Linear,
            },
            Keyframe {
                at: Time::from_secs(2),
                value: 10.0,
                easing: Easing::Linear,
            },
        ];
        assert_eq!(eval_keyframes(&keys, Time::from_secs(-1)), 0.0);
        assert_eq!(eval_keyframes(&keys, Time::from_secs(1)), 5.0);
        assert_eq!(eval_keyframes(&keys, Time::from_secs(5)), 10.0);

        let held = vec![
            Keyframe {
                at: Time::from_secs(0),
                value: 0.0,
                easing: Easing::Hold,
            },
            Keyframe {
                at: Time::from_secs(2),
                value: 10.0,
                easing: Easing::Linear,
            },
        ];
        assert_eq!(eval_keyframes(&held, Time::from_secs(1)), 0.0);
    }

    #[test]
    fn fades_ramp_at_both_ends() {
        let mut c = clip(0, 10);
        c.fade_in = Time::from_secs(2);
        c.fade_out = Time::from_secs(2);
        assert_eq!(c.fade_gain(Time::ZERO), 0.0);
        assert!((c.fade_gain(Time::from_secs(1)) - 0.5).abs() < 1e-6);
        assert_eq!(c.fade_gain(Time::from_secs(5)), 1.0);
        assert!((c.fade_gain(Time::from_secs(9)) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn sequence_duration_is_the_last_clip_end() {
        let mut seq = Sequence::new("main", fps(), [1920, 1080], 48_000);
        let mut video = Track::new("V1", TrackKind::Video);
        video.place(clip(0, 5));
        let mut audio = Track::new("A1", TrackKind::Audio);
        audio.place(clip(0, 12));
        seq.tracks = vec![video, audio];
        assert_eq!(seq.duration(), Time::from_secs(12));
        assert_eq!(seq.frame_count(), 360);
    }

    #[test]
    fn track_names_increment_per_kind() {
        let mut seq = Sequence::new("main", fps(), [1920, 1080], 48_000);
        assert_eq!(seq.next_track_name(TrackKind::Video), "V1");
        seq.tracks.push(Track::new("V1", TrackKind::Video));
        seq.tracks.push(Track::new("A1", TrackKind::Audio));
        assert_eq!(seq.next_track_name(TrackKind::Video), "V2");
        assert_eq!(seq.next_track_name(TrackKind::Audio), "A2");
        assert_eq!(seq.next_track_name(TrackKind::Caption), "CC1");
    }

    #[test]
    fn project_round_trips_through_json() {
        let mut project = Project::new("promo", fps(), [1920, 1080], 48_000);
        let seq_id = project.active_sequence.clone();
        let sequence = project.sequence_mut(&seq_id).unwrap();
        let mut track = Track::new("V1", TrackKind::Video);
        track.place(clip(0, 3));
        sequence.tracks.push(track);
        let json = serde_json::to_string_pretty(&project).unwrap();
        let back: Project = serde_json::from_str(&json).unwrap();
        assert_eq!(back, project);
        assert!(json.contains("\"degenVideo\": 1"));
    }

    #[test]
    fn newer_format_is_refused_rather_than_guessed() {
        let mut project = Project::new("p", fps(), [640, 480], 48_000);
        project.format = FORMAT_VERSION + 1;
        assert!(project.check_format().is_err());
    }

    #[test]
    fn titles_substitute_fields_and_escape_xml() {
        let mut title = Title {
            id: TitleId::new(),
            name: "lower-third".into(),
            size: [1920, 1080],
            svg: "<svg><text>{{name}}</text></svg>".into(),
            fields: IndexMap::new(),
        };
        title.fields.insert("name".into(), "Mazzola & Co <3".into());
        assert_eq!(
            title.resolved_svg(),
            "<svg><text>Mazzola &amp; Co &lt;3</text></svg>"
        );
    }

    #[test]
    fn caption_reading_rate_ignores_whitespace() {
        let cue = CaptionCue {
            id: CueId::new(),
            span: Span::new(Time::ZERO, Time::from_secs(2)),
            text: "hello there friend".into(),
            style: None,
        };
        assert!((cue.chars_per_second() - 8.0).abs() < 1e-9);
        assert_eq!(cue.lines(), 1);
    }

    #[test]
    fn display_size_applies_rotation_and_sample_aspect() {
        let stream = VideoStream {
            stream_index: 0,
            size: [1920, 1080],
            fps: fps(),
            codec: "h264".into(),
            pix_fmt: "yuv420p".into(),
            color_range: ColorRange::Tv,
            color_matrix: ColorMatrix::Bt709,
            color_primaries: None,
            transfer: None,
            rotation: 90,
            sar: Rat::ONE,
            frames: None,
            bit_rate: None,
        };
        assert_eq!(stream.display_size(), [1080, 1920]);
        let anamorphic = VideoStream {
            rotation: 0,
            sar: Rat::new(4, 3).unwrap(),
            ..stream
        };
        assert_eq!(anamorphic.display_size(), [2560, 1080]);
    }
}
