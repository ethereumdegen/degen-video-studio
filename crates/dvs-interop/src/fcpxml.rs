//! FCPXML 1.11, for Final Cut Pro and DaVinci Resolve.
//!
//! FCPXML's one unforgiving rule is its clock. Every `offset`, `start` and `duration` is
//! a rational `Ns/Ds`, and Final Cut rejects — not rounds, rejects — a document whose
//! values are not exact multiples of the sequence's `frameDuration`. So every time value
//! in this writer is produced the same way: convert to a frame index on the sequence
//! grid, then multiply by the frame duration. Nothing here divides.
//!
//! The second decision worth stating is the shape of the timeline. FCPXML's spine is a
//! single primary storyline; anything else is a *connected* clip hanging off a spine item
//! at a lane. A multi-track timeline therefore has no literal translation, and the
//! translation that survives a round trip through Final Cut is one spine-long gap with
//! every clip connected to it: lanes above zero for video in track order, below zero for
//! audio. Splitting the bottom video track into the spine instead would look tidier in
//! the inspector and would break the moment a connected clip had to span two spine items.

use crate::timeline::{self, Entry, Resource};
use crate::Export;
use dvs_core::asset::AssetStore;
use dvs_core::error::{Error, Result};
use dvs_core::ids::SequenceId;
use dvs_core::op::Warning;
use dvs_core::paths::ProjectPaths;
use dvs_core::project::{Asset, Project, TrackKind};
use dvs_core::time::Fps;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;
use std::collections::BTreeMap;

/// The version the document declares. 1.11 is what Final Cut Pro 11 and Resolve 19 read.
pub const FCPXML_VERSION: &str = "1.11";

/// A frame duration as FCPXML states it: the reciprocal of the frame rate, in seconds.
/// `30000/1001` fps is `1001/30000s`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Timebase {
    num: i64,
    den: i64,
}

impl Timebase {
    fn of(fps: Fps) -> Timebase {
        let ratio = fps.ratio();
        let (num, den) = dvs_core::time::reduce(*ratio.denom(), *ratio.numer());
        Timebase { num, den }
    }

    fn frame_duration(&self) -> String {
        format!("{}/{}s", self.num, self.den)
    }

    /// A frame count as an FCPXML instant. Deliberately unreduced: `2002/30000s` states
    /// its timebase, and `1001/15000s` — the same number — makes a reader work out
    /// whether it is on the frame grid.
    fn at(&self, frames: i64) -> String {
        if frames == 0 {
            return "0s".to_string();
        }
        format!("{}/{}s", frames * self.num, self.den)
    }
}

/// Write the sequence as FCPXML.
pub fn export(
    project: &Project,
    sequence: &SequenceId,
    paths: &ProjectPaths,
    assets: &AssetStore,
) -> Result<Export> {
    let sequence = project.sequence(sequence)?;
    let fps = sequence.fps;
    let base = Timebase::of(fps);
    let flat = timeline::flatten(sequence);
    let mut warnings: Vec<Warning> = flat.iter().flat_map(|t| t.warnings.clone()).collect();
    let total = flat.iter().map(|t| t.frames).max().unwrap_or(0).max(1);

    // Resources first: formats, then assets, because an asset references a format.
    let mut formats: Vec<Format> = vec![Format {
        id: "r1".to_string(),
        size: sequence.size,
        fps,
    }];
    let mut media: Vec<Media> = Vec::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut next_id = 2usize;

    for track in &flat {
        for placed in track.clips() {
            warnings.extend(timeline::feature_warnings(placed.clip, "FCPXML"));
            warnings.extend(timeline::retime_warning(placed.clip, "FCPXML"));
            let (resource, warning) = timeline::resolve_source(project, assets, placed.clip)?;
            warnings.extend(warning);
            let Resource::File { asset, path } = resource else {
                continue;
            };
            if seen.contains_key(&asset.hash) {
                continue;
            }
            let format = format_for(&mut formats, &mut next_id, asset, fps);
            let id = format!("r{next_id}");
            next_id += 1;
            seen.insert(asset.hash.clone(), media.len());
            media.push(Media {
                id,
                format,
                asset,
                url: timeline::file_url(&path),
            });
        }
    }

    let mut xml = Writer::new_with_indent(Vec::new(), b' ', 2);
    let mut write = |event: Event<'_>| -> Result<()> {
        xml.write_event(event)
            .map_err(|e| Error::op(format!("writing FCPXML: {e}")))
    };
    write(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
    write(Event::DocType(BytesText::from_escaped("fcpxml")))?;
    write(Event::Start(element("fcpxml", &[("version", FCPXML_VERSION)])))?;

    write(Event::Start(element("resources", &[])))?;
    for format in &formats {
        write(Event::Empty(element(
            "format",
            &[
                ("id", format.id.as_str()),
                ("name", &format.name()),
                ("frameDuration", &Timebase::of(format.fps).frame_duration()),
                ("width", &format.size[0].to_string()),
                ("height", &format.size[1].to_string()),
                ("colorSpace", "1-1-1 (Rec. 709)"),
            ],
        )))?;
    }
    for item in &media {
        let probe = &item.asset.probe;
        let frames = probe.duration.frame_ceil(fps).max(1);
        let audio = probe.audio.as_ref();
        let mut attrs: Vec<(&str, String)> = vec![
            ("id", item.id.clone()),
            ("name", item.asset.name.clone()),
            ("uid", uid_of(&item.asset.hash)),
            ("start", "0s".to_string()),
            ("duration", base.at(frames)),
            ("hasVideo", u8::from(probe.video.is_some()).to_string()),
            ("hasAudio", u8::from(audio.is_some()).to_string()),
            ("format", item.format.clone()),
        ];
        if probe.video.is_some() {
            attrs.push(("videoSources", "1".to_string()));
        }
        if let Some(audio) = audio {
            attrs.push(("audioSources", "1".to_string()));
            attrs.push(("audioChannels", audio.channels.to_string()));
            attrs.push(("audioRate", audio.rate.to_string()));
        }
        let borrowed: Vec<(&str, &str)> = attrs
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect();
        write(Event::Start(element("asset", &borrowed)))?;
        write(Event::Empty(element(
            "media-rep",
            &[("kind", "original-media"), ("src", item.url.as_str())],
        )))?;
        write(Event::End(BytesEnd::new("asset")))?;
    }
    write(Event::End(BytesEnd::new("resources")))?;

    write(Event::Start(element(
        "library",
        &[("location", timeline::file_url(paths.root()).as_str())],
    )))?;
    write(Event::Start(element("event", &[("name", &project.name)])))?;
    write(Event::Start(element(
        "project",
        &[("name", &sequence.name)],
    )))?;
    write(Event::Start(element(
        "sequence",
        &[
            ("format", "r1"),
            ("duration", &base.at(total)),
            ("tcStart", "0s"),
            ("tcFormat", "NDF"),
            ("audioLayout", if sequence.channels > 1 { "stereo" } else { "mono" }),
            ("audioRate", &format!("{}k", sequence.sample_rate / 1000)),
        ],
    )))?;
    write(Event::Start(element("spine", &[])))?;
    write(Event::Start(element(
        "gap",
        &[
            ("name", &format!("{} timeline", sequence.name)),
            ("offset", "0s"),
            ("start", "0s"),
            ("duration", &base.at(total)),
        ],
    )))?;

    let mut video_lane = 0i32;
    let mut audio_lane = 0i32;
    for track in &flat {
        let lane = match track.track.kind {
            TrackKind::Audio => {
                audio_lane -= 1;
                audio_lane
            }
            _ => {
                video_lane += 1;
                video_lane
            }
        };
        for entry in &track.entries {
            let Entry::Clip(placed) = entry else {
                continue;
            };
            let (resource, _) = timeline::resolve_source(project, assets, placed.clip)?;
            let offset = base.at(placed.start);
            let duration = base.at(placed.frames);
            let name = placed.label();
            match resource {
                Resource::File { asset, .. } => {
                    let reference = seen
                        .get(&asset.hash)
                        .map(|index| media[*index].id.clone())
                        .ok_or_else(|| {
                            Error::op(format!("asset '{}' lost its resource id", asset.name))
                        })?;
                    // A retime is a `<conform-rate>`/`<timeMap>` this writer does not
                    // emit; the in-point is what stays honest, and the speed itself is
                    // reported as a warning above.
                    let mut attrs: Vec<(&str, String)> = vec![
                        ("ref", reference),
                        ("lane", lane.to_string()),
                        ("offset", offset),
                        ("name", name),
                        ("start", base.at(placed.source_in)),
                        ("duration", duration),
                        ("format", media_format(&media, &asset.hash)),
                        ("tcFormat", "NDF".to_string()),
                    ];
                    if placed.clip.enabled {
                        attrs.push(("enabled", "1".to_string()));
                    }
                    let borrowed: Vec<(&str, &str)> = attrs
                        .iter()
                        .map(|(key, value)| (*key, value.as_str()))
                        .collect();
                    write(Event::Empty(element("asset-clip", &borrowed)))?;
                }
                Resource::Color(_) => {
                    write(Event::Empty(element(
                        "gap",
                        &[
                            ("name", name.as_str()),
                            ("lane", &lane.to_string()),
                            ("offset", &offset),
                            ("start", "0s"),
                            ("duration", &duration),
                        ],
                    )))?;
                }
            }
        }
    }

    write(Event::End(BytesEnd::new("gap")))?;
    write(Event::End(BytesEnd::new("spine")))?;
    write(Event::End(BytesEnd::new("sequence")))?;
    write(Event::End(BytesEnd::new("project")))?;
    write(Event::End(BytesEnd::new("event")))?;
    write(Event::End(BytesEnd::new("library")))?;
    write(Event::End(BytesEnd::new("fcpxml")))?;

    let mut text = String::from_utf8(xml.into_inner())
        .map_err(|e| Error::op(format!("FCPXML is not valid UTF-8: {e}")))?;
    text.push('\n');
    Ok(Export { text, warnings })
}

struct Format {
    id: String,
    size: [u32; 2],
    fps: Fps,
}

impl Format {
    fn name(&self) -> String {
        format!("FFVideoFormat{}x{}p{}", self.size[0], self.size[1], self.fps)
    }
}

struct Media<'a> {
    id: String,
    format: String,
    asset: &'a Asset,
    url: String,
}

/// The format id for an asset, adding one when its picture differs from the sequence's.
/// Final Cut refuses an asset whose format claims a size the media does not have.
fn format_for(formats: &mut Vec<Format>, next_id: &mut usize, asset: &Asset, fps: Fps) -> String {
    let Some(video) = &asset.probe.video else {
        return formats[0].id.clone();
    };
    let size = video.display_size();
    let asset_fps = video.fps;
    if let Some(found) = formats
        .iter()
        .find(|format| format.size == size && format.fps == asset_fps)
    {
        return found.id.clone();
    }
    let id = format!("r{next_id}");
    *next_id += 1;
    formats.push(Format {
        id: id.clone(),
        size,
        fps: if asset_fps.as_f64() > 0.0 { asset_fps } else { fps },
    });
    id
}

fn media_format(media: &[Media<'_>], hash: &str) -> String {
    media
        .iter()
        .find(|item| item.asset.hash == hash)
        .map(|item| item.format.clone())
        .unwrap_or_else(|| "r1".to_string())
}

/// Final Cut wants a stable 32-hex-digit media uid. The content hash already is one.
fn uid_of(hash: &str) -> String {
    hash.trim_start_matches("blake3:")
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(32)
        .collect::<String>()
        .to_uppercase()
}

fn element<'a>(name: &'a str, attrs: &[(&str, &str)]) -> BytesStart<'a> {
    let mut start = BytesStart::new(name);
    for (key, value) in attrs {
        start.push_attribute((*key, *value));
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::fixture;

    fn document(fps: Fps) -> String {
        let mut fixture = fixture(fps);
        fixture.push_clip("V1", "0", "1", "2");
        fixture.push_clip("V1", "1.5", "1", "0");
        fixture.push_second("A1", "0", "2.5", "0");
        export(
            &fixture.project,
            &fixture.sequence,
            &fixture.paths,
            &fixture.assets,
        )
        .expect("export")
        .text
    }

    /// FCPXML carries a `<!DOCTYPE fcpxml>`, which roxmltree refuses unless asked.
    fn parse(text: &str) -> roxmltree::Document<'_> {
        roxmltree::Document::parse_with_options(
            text,
            roxmltree::ParsingOptions {
                allow_dtd: true,
                ..Default::default()
            },
        )
        .expect("valid XML")
    }

    /// `Ns/Ds` → (N, D). Anything else in a time attribute is a bug.
    fn rational(text: &str) -> (i64, i64) {
        let body = text.strip_suffix('s').unwrap_or_else(|| panic!("not a time: {text}"));
        match body.split_once('/') {
            Some((num, den)) => (num.parse().expect("numerator"), den.parse().expect("denominator")),
            None => (body.parse().expect("whole seconds"), 1),
        }
    }

    #[test]
    fn every_instant_is_a_whole_number_of_frames() {
        let text = document(Fps::new(30000, 1001).unwrap());
        let doc = parse(&text);
        let base = doc
            .descendants()
            .find(|n| n.has_tag_name("format"))
            .and_then(|n| n.attribute("frameDuration"))
            .expect("a frameDuration");
        assert_eq!(base, "1001/30000s", "29.97 must stay exact");
        let (fd_num, fd_den) = rational(base);

        let mut checked = 0;
        for node in doc.descendants() {
            for key in ["offset", "duration", "start", "tcStart"] {
                let Some(value) = node.attribute(key) else {
                    continue;
                };
                let (num, den) = rational(value);
                if num == 0 {
                    continue;
                }
                assert_eq!(
                    den, fd_den,
                    "{key}='{value}' on <{}> is not in the sequence timebase",
                    node.tag_name().name()
                );
                assert_eq!(
                    num % fd_num,
                    0,
                    "{key}='{value}' on <{}> is not a whole number of frames",
                    node.tag_name().name()
                );
                checked += 1;
            }
        }
        assert!(checked >= 6, "only {checked} time attributes were checked");
    }

    #[test]
    fn the_document_nests_the_way_final_cut_expects() {
        let text = document(Fps::new(30, 1).unwrap());
        let doc = parse(&text);
        let root = doc.root_element();
        assert_eq!(root.tag_name().name(), "fcpxml");
        assert_eq!(root.attribute("version"), Some(FCPXML_VERSION));

        let path = ["library", "event", "project", "sequence", "spine", "gap"];
        let mut node = root
            .children()
            .find(|n| n.has_tag_name("library"))
            .expect("a library");
        for step in &path[1..] {
            node = node
                .children()
                .find(|child| child.has_tag_name(*step))
                .unwrap_or_else(|| panic!("no <{step}> under <{}>", node.tag_name().name()));
        }
        let clips: Vec<_> = node
            .children()
            .filter(|n| n.has_tag_name("asset-clip"))
            .collect();
        assert_eq!(clips.len(), 3, "every clip should be connected to the gap");
        let lanes: Vec<&str> = clips.iter().filter_map(|c| c.attribute("lane")).collect();
        assert_eq!(lanes, vec!["1", "1", "-1"], "audio belongs below the spine");

        let assets: Vec<_> = doc
            .descendants()
            .filter(|n| n.has_tag_name("asset"))
            .collect();
        assert_eq!(assets.len(), 2, "one resource per distinct asset");
        assert!(assets
            .iter()
            .all(|asset| asset
                .children()
                .any(|child| child.has_tag_name("media-rep")
                    && child
                        .attribute("src")
                        .is_some_and(|src| src.starts_with("file:///")))));
    }

    #[test]
    fn a_title_becomes_a_gap_and_a_warning() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_title("V1", "0", "2");
        let export = export(
            &fixture.project,
            &fixture.sequence,
            &fixture.paths,
            &fixture.assets,
        )
        .expect("export");
        assert!(export
            .warnings
            .iter()
            .any(|warning| warning.code == "title-not-representable"));
        let doc = parse(&export.text);
        let gaps: Vec<_> = doc
            .descendants()
            .filter(|n| n.has_tag_name("gap") && n.attribute("lane").is_some())
            .collect();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].attribute("duration"), Some("60/30s"));
    }
}
