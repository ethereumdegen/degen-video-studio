//! The segment cache, and the key that makes reuse safe.
//!
//! **The correctness requirement here is one-directional.** A false cache *miss* costs
//! time: a segment is re-encoded that did not have to be. A false cache *hit* ships the
//! wrong video — the agent is told the render succeeded, the digest describes frames that
//! were never encoded, and nobody notices until a human watches it. Those costs are not
//! comparable, so [`segment_key`] hashes everything that can possibly affect a segment's
//! pixels and deliberately over-includes wherever the alternative is a judgement call:
//!
//! - the slice of the document the segment covers — every clip overlapping it, with its
//!   effects, keyframes and transition, plus the *previous* clip whenever a transition
//!   reads back into it past its out-point;
//! - the referenced asset hashes (content identity, so re-importing the same bytes is a
//!   hit and different bytes never are), their probes and their proxy paths;
//! - referenced titles, caption styles and nested sequences, whole;
//! - the sequence's fps, size and background;
//! - [`dvs_core::ENGINE_VERSION`] and the ffmpeg version string, because our frames and
//!   their encoded bytes both change across builds;
//! - the resolved encoder name and every encoder setting: quality, preset, scale, proxy.
//!
//! What is *not* in the key is as deliberate: audio. A segment holds video only, the mix is
//! bounced once over the whole range, so an edit to a music bed must not invalidate a single
//! video chunk.
//!
//! Segments live at `cache/segments/<key>.mp4` with their per-frame compositor reports
//! beside them at `cache/segments/<key>.json`, so a cache hit can still answer "what is in
//! those frames" without decoding anything.

use crate::pipeline::RenderSpec;
use dvs_core::error::{Error, Result};
use dvs_core::ids::{AssetId, SequenceId, StyleId, TitleId};
use dvs_core::paths::ProjectPaths;
use dvs_core::project::{Project, Source, TrackKind};
use dvs_core::time::Span;
use dvs_core::ENGINE_VERSION;
use dvs_media::Toolchain;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Hash of everything that can change the frames in `span`.
///
/// Stable across processes and machines for an unchanged document and toolchain; different
/// whenever anything above is different. See the module doc for why the inclusion list errs
/// on the side of too much.
pub fn segment_key(
    project: &Project,
    sequence: &SequenceId,
    span: Span,
    spec: &RenderSpec,
    tool: &Toolchain,
) -> Result<String> {
    let mut refs = Refs::default();
    let slice = sequence_slice(project, sequence, Some(span), &mut refs)?;

    // A nested sequence contributes its whole document, and may itself nest: walk to a
    // fixed point rather than recursing, so a cycle terminates instead of overflowing.
    let mut visited: BTreeSet<SequenceId> = BTreeSet::new();
    visited.insert(sequence.clone());
    let mut nested: Vec<Value> = Vec::new();
    loop {
        let pending: Vec<SequenceId> = refs
            .sequences
            .iter()
            .filter(|id| !visited.contains(*id))
            .cloned()
            .collect();
        if pending.is_empty() {
            break;
        }
        for id in pending {
            visited.insert(id.clone());
            nested.push(sequence_slice(project, &id, None, &mut refs)?);
        }
    }

    let mut assets = Vec::with_capacity(refs.assets.len());
    for id in &refs.assets {
        let asset = project.asset(id)?;
        assets.push(json!({
            "id": asset.id,
            "hash": asset.hash,
            "probe": asset.probe,
            // Proxy pixels are different pixels; which file we decode is part of the key.
            "proxy": spec.use_proxy.then(|| asset.proxy.clone()).flatten(),
        }));
    }
    let mut titles = Vec::with_capacity(refs.titles.len());
    for id in &refs.titles {
        let title = project
            .titles
            .get(id)
            .ok_or_else(|| Error::no_match("title", id.as_str(), Vec::new()))?;
        titles.push(json!({ "id": id, "title": title }));
    }
    let mut styles = Vec::with_capacity(refs.styles.len());
    for id in &refs.styles {
        let style = project
            .styles
            .get(id)
            .ok_or_else(|| Error::no_match("style", id.as_str(), Vec::new()))?;
        styles.push(json!({ "id": id, "style": style }));
    }

    let input = json!({
        "engine": ENGINE_VERSION,
        "ffmpeg": tool.version(),
        "encoder": spec.encoder.resolve(tool)?,
        "quality": spec.quality,
        "preset": spec.preset,
        "scale": spec.scale,
        "proxy": spec.use_proxy,
        "span": { "start": span.start, "end": span.end },
        "sequence": slice,
        "nested": nested,
        "assets": assets,
        "titles": titles,
        "styles": styles,
    });
    let bytes = serde_json::to_vec(&input)
        .map_err(|e| Error::op(format!("segment key input is not serializable: {e}")))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// `cache/segments/<key>.mp4`.
pub fn segment_path(paths: &ProjectPaths, key: &str) -> PathBuf {
    paths.segment_dir().join(format!("{key}.mp4"))
}

/// `cache/segments/<key>.json`: the compositor's per-frame reports for that chunk.
///
/// Written with every encode so that a later `--digest` run over an all-hit cache reports
/// what is actually in the frames instead of an empty list.
pub fn report_path(paths: &ProjectPaths, key: &str) -> PathBuf {
    paths.segment_dir().join(format!("{key}.json"))
}

/// Delete every cached segment whose key is not in `keep`, returning how many segments went.
///
/// Files that are not `<key>.mp4`/`<key>.json` pairs — the `.tmpNNNN.mp4` left behind by an
/// interrupted render, for instance — have no key in `keep` and are collected too, which is
/// the point: a truncated chunk that survived under a real key would be a false hit.
pub fn prune(paths: &ProjectPaths, keep: &[String]) -> Result<usize> {
    let dir = paths.segment_dir();
    if !dir.exists() {
        return Ok(0);
    }
    let keep: BTreeSet<&str> = keep.iter().map(String::as_str).collect();
    let mut removed = 0usize;
    for entry in std::fs::read_dir(&dir).map_err(|e| Error::io(&dir, e))? {
        let path = entry.map_err(|e| Error::io(&dir, e))?.path();
        if !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if keep.contains(stem) {
            continue;
        }
        let is_chunk = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"));
        std::fs::remove_file(&path).map_err(|e| Error::io(&path, e))?;
        // The sidecar rides along with its chunk; counting both would report twice the
        // work that was actually thrown away.
        removed += usize::from(is_chunk);
    }
    Ok(removed)
}

/// What is in the cache, and how much of `wanted` it already covers.
///
/// `wanted` is the key list a render would ask for — [`crate::plan_keys`] computes it — so
/// an agent can ask "how much of this render is already done" before starting one, and
/// `render.segments` can show why a render was slow.
pub fn status(paths: &ProjectPaths, wanted: &[String]) -> Result<CacheStatus> {
    let dir = paths.segment_dir();
    let wanted_set: BTreeSet<&str> = wanted.iter().map(String::as_str).collect();
    let mut entries = Vec::new();
    let mut bytes = 0u64;
    if dir.exists() {
        for entry in std::fs::read_dir(&dir).map_err(|e| Error::io(&dir, e))? {
            let entry = entry.map_err(|e| Error::io(&dir, e))?;
            let path = entry.path();
            if !path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"))
            {
                continue;
            }
            let Some(key) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let size = entry.metadata().map_err(|e| Error::io(&path, e))?.len();
            bytes += size;
            entries.push(CacheEntry {
                key: key.to_string(),
                bytes: size,
                wanted: wanted_set.contains(key),
            });
        }
    }
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    let present: BTreeSet<&str> = entries
        .iter()
        .filter(|entry| entry.wanted)
        .map(|entry| entry.key.as_str())
        .collect();
    let missing: Vec<String> = wanted
        .iter()
        .filter(|key| !present.contains(key.as_str()))
        .cloned()
        .collect();
    let stale: Vec<String> = entries
        .iter()
        .filter(|entry| !entry.wanted)
        .map(|entry| entry.key.clone())
        .collect();
    Ok(CacheStatus {
        dir: dir.display().to_string(),
        segments: entries.len(),
        bytes,
        wanted: wanted.len(),
        present: present.len(),
        missing,
        stale,
        entries,
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheStatus {
    pub dir: String,
    /// Chunks on disk, wanted or not.
    pub segments: usize,
    pub bytes: u64,
    /// Segments the current plan needs.
    pub wanted: usize,
    /// How many of those are already encoded.
    pub present: usize,
    /// Keys the next render will have to encode.
    pub missing: Vec<String>,
    /// Cached chunks no longer referenced: what [`prune`] would collect.
    pub stale: Vec<String>,
    pub entries: Vec<CacheEntry>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheEntry {
    /// The segment key: the file is `<key>.mp4`.
    pub key: String,
    /// Size of the chunk on disk.
    pub bytes: u64,
    /// Whether the plan that was passed to [`status`] still asks for this chunk.
    pub wanted: bool,
}

/// Everything a slice points at but does not contain.
#[derive(Default)]
struct Refs {
    assets: BTreeSet<AssetId>,
    titles: BTreeSet<TitleId>,
    styles: BTreeSet<StyleId>,
    sequences: BTreeSet<SequenceId>,
}

/// The part of a sequence that can affect frames in `span`, or all of it when `span` is
/// `None` (a nested sequence is keyed whole: mapping a nest's internal timing back through
/// speed and reverse is exactly the kind of cleverness a false hit hides in).
fn sequence_slice(
    project: &Project,
    sequence: &SequenceId,
    span: Option<Span>,
    refs: &mut Refs,
) -> Result<Value> {
    let seq = project.sequence(sequence)?;
    let mut tracks = Vec::new();
    for track in &seq.tracks {
        // Audio never reaches a chunk; see the module doc.
        if matches!(track.kind, TrackKind::Audio) {
            continue;
        }
        // A hidden track still contributes its flag: un-hiding it must miss the cache.
        let mut entry = json!({
            "id": track.id,
            "kind": track.kind,
            "hidden": track.hidden,
            "style": track.style,
        });
        if let Some(style) = &track.style {
            refs.styles.insert(style.clone());
        }
        if track.hidden {
            tracks.push(entry);
            continue;
        }

        let mut include: BTreeSet<usize> = BTreeSet::new();
        for (index, clip) in track.clips.iter().enumerate() {
            let covered = span.is_none_or(|span| clip.span().overlaps(&span));
            if !covered {
                continue;
            }
            include.insert(index);
            // A transition renders the previous clip past its out-point, so that clip's
            // pixels are in this segment even though its span is not.
            if clip.transition_in.is_some() {
                if let Some(previous) = index.checked_sub(1) {
                    include.insert(previous);
                }
            }
        }
        let mut clips = Vec::with_capacity(include.len());
        for index in include {
            let clip = &track.clips[index];
            match &clip.source {
                Source::Asset { asset, .. } | Source::Image { asset } => {
                    refs.assets.insert(asset.clone());
                }
                Source::Title { title } => {
                    refs.titles.insert(title.clone());
                }
                Source::Sequence { sequence } => {
                    refs.sequences.insert(sequence.clone());
                }
                Source::Color { .. } | Source::Generator { .. } => {}
            }
            let value = serde_json::to_value(clip)
                .map_err(|e| Error::op(format!("clip '{}' is not serializable: {e}", clip.label())))?;
            // Effect parameters can name an asset (a LUT, a matte) by id. Sweeping the
            // clip's own JSON for known ids costs nothing and closes the hole without a
            // per-effect list that would go stale the moment the catalog grows.
            collect_asset_ids(project, &value, refs);
            clips.push(value);
        }

        let cues: Vec<Value> = track
            .cues
            .iter()
            .filter(|cue| span.is_none_or(|span| cue.span.overlaps(&span)))
            .inspect(|cue| {
                if let Some(style) = &cue.style {
                    refs.styles.insert(style.clone());
                }
            })
            .map(|cue| serde_json::to_value(cue).unwrap_or(Value::Null))
            .collect();

        entry["clips"] = Value::Array(clips);
        entry["cues"] = Value::Array(cues);
        tracks.push(entry);
    }

    Ok(json!({
        "id": seq.id,
        "fps": seq.fps,
        "size": seq.size,
        "background": seq.background,
        "tracks": tracks,
    }))
}

/// Add every string in `value` that names an asset in this project.
fn collect_asset_ids(project: &Project, value: &Value, refs: &mut Refs) {
    match value {
        Value::String(text) => {
            let id = AssetId::from_raw(text.as_str());
            if project.assets.contains_key(&id) {
                refs.assets.insert(id);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_asset_ids(project, item, refs);
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                collect_asset_ids(project, item, refs);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::color::Rgba;
    use dvs_core::project::{Asset, AssetKind, Clip, Effect, Probe, Track};
    use dvs_core::time::{Fps, Time};
    use dvs_media::Encoder;

    fn fps() -> Fps {
        Fps::new(30, 1).unwrap()
    }

    fn fixture() -> (Project, SequenceId, RenderSpec) {
        let mut project = Project::new("t", fps(), [320, 180], 48_000);
        let id = project.active_sequence.clone();
        let asset = Asset {
            id: AssetId::from_raw("ast_key"),
            name: "clip.mp4".into(),
            hash: "blake3:aa".into(),
            kind: AssetKind::Video,
            probe: Probe {
                duration: Time::from_secs(30),
                ..Probe::default()
            },
            proxy: None,
            source_path: None,
            // `Asset::imported` is a chrono stamp and this crate does not depend on
            // chrono; the value cannot affect a key, so it is borrowed from a scratch
            // project rather than dragging in a dependency for a test fixture.
            imported: Project::new("stamp", fps(), [2, 2], 48_000).created,
            provenance: None,
        };
        project.assets.insert(asset.id.clone(), asset);
        let seq = project.sequence_mut(&id).unwrap();
        let mut track = Track::new("V1", TrackKind::Video);
        track.place(Clip::new(
            Source::Asset {
                asset: AssetId::from_raw("ast_key"),
                stream: None,
            },
            Time::ZERO,
            Time::from_secs(4),
        ));
        track.place(Clip::new(
            Source::Color { color: Rgba::WHITE },
            Time::from_secs(4),
            Time::from_secs(4),
        ));
        seq.tracks.push(track);
        let spec = RenderSpec::new("out.mp4", id.clone());
        (project, id, spec)
    }

    fn head(project: &Project, id: &SequenceId, spec: &RenderSpec) -> String {
        let tool = Toolchain::shared().unwrap();
        segment_key(
            project,
            id,
            Span::new(Time::ZERO, Time::from_secs(4)),
            spec,
            tool,
        )
        .unwrap()
    }

    #[test]
    fn an_untouched_document_keys_identically_twice() {
        let (project, id, spec) = fixture();
        assert_eq!(
            head(&project, &id, &spec),
            head(&project, &id, &spec),
            "the same document and settings must produce a byte-identical key"
        );
    }

    #[test]
    fn moving_a_clip_changes_its_segments_key() {
        let (project, id, spec) = fixture();
        let before = head(&project, &id, &spec);
        let mut moved = project.clone();
        let seq = moved.sequence_mut(&id).unwrap();
        seq.tracks[0].clips[0].start = Time::new(1, 2).unwrap();
        assert_ne!(
            before,
            head(&moved, &id, &spec),
            "a clip that moved renders different frames"
        );
    }

    #[test]
    fn an_effect_parameter_changes_the_key() {
        let (project, id, spec) = fixture();
        let mut with_effect = project.clone();
        let seq = with_effect.sequence_mut(&id).unwrap();
        let mut effect = Effect::new("color.grade");
        effect.params.insert("saturation".into(), json!(1.0));
        seq.tracks[0].clips[0].effects.push(effect);
        let one = head(&with_effect, &id, &spec);

        let seq = with_effect.sequence_mut(&id).unwrap();
        seq.tracks[0].clips[0].effects[0]
            .params
            .insert("saturation".into(), json!(1.4));
        assert_ne!(
            one,
            head(&with_effect, &id, &spec),
            "an effect parameter is a pixel-affecting change"
        );
    }

    #[test]
    fn encoder_quality_and_scale_are_part_of_the_key() {
        let (project, id, spec) = fixture();
        let base = head(&project, &id, &spec);

        let x265 = RenderSpec {
            encoder: Encoder::X265,
            ..spec.clone()
        };
        assert_ne!(base, head(&project, &id, &x265), "a different codec");

        let sharper = RenderSpec {
            quality: 12,
            ..spec.clone()
        };
        assert_ne!(base, head(&project, &id, &sharper), "a different crf");

        let half = RenderSpec {
            scale: 0.5,
            ..spec.clone()
        };
        assert_ne!(base, head(&project, &id, &half), "a different output size");

        let same_codec_other_spelling = RenderSpec {
            encoder: Encoder::X264,
            ..spec.clone()
        };
        assert_eq!(
            base,
            head(&project, &id, &same_codec_other_spelling),
            "'auto' and 'x264' resolve to the same encoder and must hit the same chunk"
        );
    }

    #[test]
    fn a_changed_asset_hash_changes_the_key() {
        let (project, id, spec) = fixture();
        let before = head(&project, &id, &spec);
        let mut relinked = project.clone();
        relinked
            .assets
            .get_mut(&AssetId::from_raw("ast_key"))
            .unwrap()
            .hash = "blake3:bb".into();
        assert_ne!(
            before,
            head(&relinked, &id, &spec),
            "different source bytes are different frames"
        );
    }

    #[test]
    fn an_edit_outside_the_segment_does_not_change_it() {
        let (project, id, spec) = fixture();
        let before = head(&project, &id, &spec);
        let mut edited = project.clone();
        let seq = edited.sequence_mut(&id).unwrap();
        seq.tracks[0].clips[1].opacity = 0.25;
        assert_eq!(
            before,
            head(&edited, &id, &spec),
            "editing the clip in the second segment must leave the first reusable"
        );
    }

    #[test]
    fn hiding_a_track_changes_the_key() {
        let (project, id, spec) = fixture();
        let before = head(&project, &id, &spec);
        let mut hidden = project.clone();
        hidden.sequence_mut(&id).unwrap().tracks[0].hidden = true;
        assert_ne!(
            before,
            head(&hidden, &id, &spec),
            "a hidden track renders nothing, which is a different frame"
        );
    }

    #[test]
    fn prune_keeps_what_is_asked_for_and_collects_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::new(dir.path());
        std::fs::create_dir_all(paths.segment_dir()).unwrap();
        for key in ["aaa", "bbb"] {
            std::fs::write(segment_path(&paths, key), b"chunk").unwrap();
            std::fs::write(report_path(&paths, key), b"[]").unwrap();
        }
        std::fs::write(paths.segment_dir().join("aaa.tmp99.mp4"), b"partial").unwrap();

        let keep = vec!["aaa".to_string()];
        assert_eq!(
            prune(&paths, &keep).unwrap(),
            2,
            "the unwanted chunk and the interrupted temp chunk both go"
        );
        assert!(segment_path(&paths, "aaa").exists());
        assert!(report_path(&paths, "aaa").exists());
        assert!(!segment_path(&paths, "bbb").exists());
        assert!(!report_path(&paths, "bbb").exists());
        assert!(!paths.segment_dir().join("aaa.tmp99.mp4").exists());
    }

    #[test]
    fn status_reports_what_the_next_render_still_has_to_encode() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::new(dir.path());
        std::fs::create_dir_all(paths.segment_dir()).unwrap();
        std::fs::write(segment_path(&paths, "have"), b"12345").unwrap();
        std::fs::write(segment_path(&paths, "old"), b"1").unwrap();

        let status = status(&paths, &["have".to_string(), "need".to_string()]).unwrap();
        assert_eq!(status.segments, 2);
        assert_eq!(status.bytes, 6);
        assert_eq!(status.present, 1);
        assert_eq!(status.missing, vec!["need".to_string()]);
        assert_eq!(status.stale, vec!["old".to_string()]);
    }
}
