//! Helpers the timeline ops share.
//!
//! Three kinds of arithmetic show up in every clip op and are exactly the kind that is
//! wrong by one frame in silence: shifting the clips after an edit, finding a clip's index
//! after the caller named it by id, and snapping a span to the frame grid. They live here
//! once. [`ripple_after`] especially: it is the only place in the crate where one clip
//! moves because of another clip's length, so ripple is either right everywhere or wrong
//! in one visible function.
//!
//! The source helpers ([`parse_source`], [`source_available`]) are here rather than in
//! `clip.rs` because titles and nested sequences are placed by the `title.*` and `seq.*`
//! ops too, and two spellings of "what does `color:#ff0000` mean" would be two bugs.

use crate::color::Rgba;
use crate::error::{Error, Result};
use crate::ids::{ClipId, SequenceId, TitleId};
use crate::project::{AssetKind, Generator, Project, Source, Title, Track, TrackKind};
use crate::time::{Fps, Span, Time};

/// The frame grid every time argument aimed at this sequence must land on.
pub fn sequence_fps(project: &Project, seq: &SequenceId) -> Result<Fps> {
    Ok(project.sequence(seq)?.fps)
}

/// Shift every clip starting at or after `from` by `by`.
///
/// `>=` and not `>`: a ripple insert at the exact start of a clip must push that clip,
/// otherwise the inserted clip lands on top of it. Because the clip list is sorted by
/// start, the affected clips are a suffix and shifting them all by the same amount keeps
/// the order — callers that shift by a negative amount are closing a hole they just made,
/// so the suffix cannot pass the clip in front of it either.
///
/// Markers and caption cues are deliberately left alone: a ripple that silently moved a
/// marker would make "the pricing marker is at 42 s" untrue without saying so.
pub fn ripple_after(track: &mut Track, from: Time, by: Time) {
    if by.is_zero() {
        return;
    }
    for clip in track.clips.iter_mut().filter(|clip| clip.start >= from) {
        clip.start += by;
    }
}

/// Position of a clip on the track, or a `no-match` error listing what the track holds.
/// Indices are never accepted from a caller; they are derived here and used immediately.
pub fn clip_index(track: &Track, clip: &ClipId) -> Result<usize> {
    track
        .clips
        .iter()
        .position(|candidate| &candidate.id == clip)
        .ok_or_else(|| {
            Error::no_match(
                "clip",
                clip.as_str(),
                track.clips.iter().map(|c| c.label().to_string()).collect(),
            )
        })
}

/// A locked track refuses every mutation. The lock exists so a human can protect a track
/// from an agent working in the same document, which only works if it is checked before
/// anything is touched.
pub fn assert_unlocked(track: &Track) -> Result<()> {
    if track.locked {
        return Err(Error::op(format!(
            "track '{}' is locked; unlock it (track.lock) before editing it",
            track.name
        )));
    }
    Ok(())
}

/// Caption tracks carry cues, so a clip op pointed at one is a mistake worth naming.
pub fn assert_takes_clips(track: &Track) -> Result<()> {
    if track.kind == TrackKind::Caption {
        return Err(Error::op(format!(
            "track '{}' is a caption track; it carries cues, not clips — use the caption.* ops",
            track.name
        )));
    }
    Ok(())
}

pub fn snap_span(span: Span, fps: Fps) -> Span {
    Span::new(span.start.snap(fps), span.end.snap(fps))
}

/// Restore the sorted-by-start invariant after an op changed starts out of order (a move,
/// a slide). Stable, so clips that land on the same start keep the order the caller saw,
/// which makes the overlap error name the pair the caller expects.
pub fn resort(track: &mut Track) {
    track.clips.sort_by(|a, b| a.start.cmp(&b.start));
}

/// Clips that would collide with `span`, ignoring one clip — the one being moved, which
/// must not collide with where it already is.
pub fn occupants(track: &Track, span: Span, ignore: Option<&ClipId>) -> Vec<ClipId> {
    track
        .clips
        .iter()
        .filter(|clip| Some(&clip.id) != ignore && clip.span().overlaps(&span))
        .map(|clip| clip.id.clone())
        .collect()
}

/// Refuse a placement that would overlap, naming the clip in the way. The engine's
/// validation would catch the overlap afterwards, but "clips 'a' and 'b' overlap" is a
/// worse answer than "'b' already occupies 8/1–12/1".
pub fn assert_free(track: &Track, span: Span, ignore: Option<&ClipId>) -> Result<()> {
    let blocking = occupants(track, span, ignore);
    if blocking.is_empty() {
        return Ok(());
    }
    let detail: Vec<String> = blocking
        .iter()
        .filter_map(|id| track.clips.iter().find(|clip| &clip.id == id))
        .map(|clip| format!("'{}' {}", clip.label(), clip.span()))
        .collect();
    Err(Error::op(format!(
        "{} on track '{}' already occupies {}; pass --overwrite or pick a free range",
        detail.join(", "),
        track.name,
        span
    )))
}

/// End of the last clip on a track — where an append lands.
pub fn track_end(track: &Track) -> Time {
    track
        .clips
        .last()
        .map(|clip| clip.end())
        .unwrap_or(Time::ZERO)
}

/// How much source time this kind of source can supply, or `None` when the answer is "as
/// much as asked for". A color, a title, a generator and a still image are synthesised per
/// frame and have no end; only timed media and a nested sequence can be trimmed past
/// theirs, so only those are clamped.
pub fn source_available(project: &Project, source: &Source) -> Option<Time> {
    match source {
        Source::Asset { asset, .. } => {
            let asset = project.assets.get(asset)?;
            match asset.kind {
                AssetKind::Video | AssetKind::Audio => Some(asset.probe.duration),
                _ => None,
            }
        }
        Source::Sequence { sequence } => project.sequences.get(sequence).map(|seq| seq.duration()),
        Source::Image { .. }
        | Source::Title { .. }
        | Source::Color { .. }
        | Source::Generator { .. } => None,
    }
}

/// A generator by its contract name. The names are the serialized `Generator` variants, so
/// this cannot drift from the document format.
pub fn generator_named(name: &str) -> Result<Generator> {
    serde_json::from_value::<Generator>(serde_json::Value::String(name.trim().to_string()))
        .map_err(|_| {
            Error::bad_args(format!(
                "unknown generator '{name}'; known: bars, tone, countdown, frame-numbers"
            ))
        })
}

/// `--source` as an agent writes it.
///
/// Precedence: the prefixed forms (`color:`, `title:`, `seq:`, `image:`, `gen:`) are
/// unambiguous; a bare token is resolved as an asset id, name or file stem first, and only
/// then as a generator keyword. Assets win because they are the user's data: importing
/// `bars.mp4` and writing `--source bars` must not silently place a synthetic test pattern
/// instead of the footage. `gen:bars` always means the generator.
pub fn parse_source(project: &Project, text: &str) -> Result<Source> {
    let text = text.trim();
    if text.is_empty() {
        return Err(Error::bad_args("empty source"));
    }
    if let Some(hex) = text.strip_prefix("color:") {
        return Ok(Source::Color {
            color: Rgba::parse(hex)?,
        });
    }
    if let Some(query) = text.strip_prefix("title:") {
        return Ok(Source::Title {
            title: resolve_title(project, query)?,
        });
    }
    if let Some(query) = text.strip_prefix("seq:") {
        return Ok(Source::Sequence {
            sequence: project.resolve_sequence(Some(query))?,
        });
    }
    if let Some(query) = text.strip_prefix("image:") {
        return Ok(Source::Image {
            asset: project.resolve_asset(query)?,
        });
    }
    if let Some(name) = text.strip_prefix("gen:") {
        return Ok(Source::Generator {
            generator: generator_named(name)?,
            params: serde_json::Map::new(),
        });
    }
    if project.resolve_asset(text).is_err() {
        if let Ok(generator) = generator_named(text) {
            return Ok(Source::Generator {
                generator,
                params: serde_json::Map::new(),
            });
        }
    }
    let asset = project.resolve_asset(text)?;
    // A still has no time axis: the image source kind tells the compositor to hold one
    // decoded frame instead of opening a decoder session that would seek forever.
    Ok(if project.asset(&asset)?.kind == AssetKind::Image {
        Source::Image { asset }
    } else {
        Source::Asset {
            asset,
            stream: None,
        }
    })
}

/// A title by id or name, with the same ambiguity rules as assets and sequences.
pub fn resolve_title(project: &Project, query: &str) -> Result<TitleId> {
    let direct = TitleId::from_raw(query);
    if project.titles.contains_key(&direct) {
        return Ok(direct);
    }
    let matches: Vec<&Title> = project
        .titles
        .values()
        .filter(|title| title.name == query)
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => Err(Error::no_match(
            "title",
            query,
            project.titles.values().map(|t| t.name.clone()).collect(),
        )),
        many => Err(Error::bad_args(format!(
            "title name '{query}' is ambiguous ({} matches); use an id",
            many.len()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{Asset, Clip, Probe};
    use crate::time::Time;
    use chrono::Utc;

    fn track_with(starts: &[(i64, i64)]) -> Track {
        let mut track = Track::new("V1", TrackKind::Video);
        for (index, (start, duration)) in starts.iter().enumerate() {
            let mut clip = Clip::new(
                Source::Color {
                    color: Rgba::BLACK,
                },
                Time::from_secs(*start),
                Time::from_secs(*duration),
            );
            clip.id = ClipId::from_raw(format!("clp_{index}"));
            track.clips.push(clip);
        }
        track
    }

    #[test]
    fn ripple_moves_the_clip_starting_exactly_at_the_boundary() {
        let mut track = track_with(&[(0, 4), (4, 4), (8, 4)]);
        ripple_after(&mut track, Time::from_secs(4), Time::from_secs(2));
        let starts: Vec<i64> = track
            .clips
            .iter()
            .map(|c| c.start.as_secs_f64() as i64)
            .collect();
        assert_eq!(starts, vec![0, 6, 10], "the clip at the boundary must move");
    }

    #[test]
    fn a_negative_ripple_closes_a_hole_without_reordering() {
        let mut track = track_with(&[(0, 4), (8, 4), (12, 4)]);
        ripple_after(&mut track, Time::from_secs(8), -Time::from_secs(4));
        assert_eq!(track.clips[1].start, Time::from_secs(4));
        assert_eq!(track.clips[2].start, Time::from_secs(8));
        assert!(track.clips.windows(2).all(|w| w[0].end() <= w[1].start));
    }

    #[test]
    fn occupancy_ignores_the_clip_being_moved() {
        let track = track_with(&[(0, 4), (4, 4)]);
        let span = Span::new(Time::from_secs(0), Time::from_secs(4));
        assert!(assert_free(&track, span, None).is_err());
        assert!(assert_free(&track, span, Some(&ClipId::from_raw("clp_0"))).is_ok());
        let straddling = Span::new(Time::from_secs(3), Time::from_secs(5));
        assert_eq!(occupants(&track, straddling, None).len(), 2);
    }

    #[test]
    fn synthetic_sources_have_no_end_but_media_does() {
        let mut project = Project::new("t", Fps::default(), [64, 64], 48000);
        project.assets.insert(
            crate::ids::AssetId::from_raw("ast_clip"),
            Asset {
                id: crate::ids::AssetId::from_raw("ast_clip"),
                name: "talk.mp4".into(),
                hash: "blake3:0".into(),
                kind: AssetKind::Video,
                probe: Probe {
                    duration: Time::from_secs(12),
                    ..Probe::default()
                },
                proxy: None,
                source_path: None,
                imported: Utc::now(),
                provenance: None,
            },
        );
        let media = parse_source(&project, "talk").unwrap();
        assert_eq!(source_available(&project, &media), Some(Time::from_secs(12)));
        let bars = parse_source(&project, "bars").unwrap();
        assert_eq!(source_available(&project, &bars), None);
        assert_eq!(
            parse_source(&project, "image:ast_clip").unwrap(),
            Source::Image {
                asset: crate::ids::AssetId::from_raw("ast_clip")
            }
        );
        assert!(matches!(
            parse_source(&project, "color:#ff8800").unwrap(),
            Source::Color { color } if color == Rgba::opaque(255, 136, 0)
        ));
        assert!(parse_source(&project, "nope").is_err());
    }

    #[test]
    fn an_imported_asset_wins_over_a_generator_keyword() {
        // The bug this guards: importing `bars.mp4` and writing `--source bars` used to
        // place a synthetic test pattern, leaving the footage unreferenced and the agent
        // with no idea why the render looked wrong.
        let mut project = Project::new("t", Fps::default(), [64, 64], 48000);
        project.assets.insert(
            crate::ids::AssetId::from_raw("ast_bars"),
            Asset {
                id: crate::ids::AssetId::from_raw("ast_bars"),
                name: "bars.mp4".into(),
                hash: "blake3:1".into(),
                kind: AssetKind::Video,
                probe: Probe {
                    duration: Time::from_secs(4),
                    ..Probe::default()
                },
                proxy: None,
                source_path: None,
                imported: Utc::now(),
                provenance: None,
            },
        );
        assert_eq!(
            parse_source(&project, "bars").unwrap(),
            Source::Asset {
                asset: crate::ids::AssetId::from_raw("ast_bars"),
                stream: None
            }
        );
        assert!(matches!(
            parse_source(&project, "gen:bars").unwrap(),
            Source::Generator { generator, .. } if generator == Generator::Bars
        ));
        // With no such asset the keyword still resolves, so a fresh project can slate.
        let empty = Project::new("t", Fps::default(), [64, 64], 48000);
        assert!(matches!(
            parse_source(&empty, "bars").unwrap(),
            Source::Generator { .. }
        ));
        assert!(parse_source(&empty, "gen:nope").is_err());
    }
}
