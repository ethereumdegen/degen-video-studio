//! The one place a sequence is turned into frames.
//!
//! MLT, FCPXML, OTIO and EDL disagree about almost everything, but they all want the same
//! preprocessing: rational positions collapsed onto the frame grid, holes between clips
//! made explicit, and each clip's source resolved to a file on disk. Doing that once here
//! is not just deduplication — it is the only way the four writers can agree with each
//! other, and with the native renderer, about *which frame a cut is on*.
//!
//! Positions are rounded through the clip's end rather than through its duration. Rounding
//! a duration lets a half-frame rounding error accumulate down the track, so the tenth cut
//! in an export lands a frame away from the same cut in the native render; taking
//! `end.frame_round() - start.frame_round()` makes adjacent clips exactly adjacent by
//! construction, which is the property every one of these formats depends on.

use dvs_core::asset::AssetStore;
use dvs_core::color::Rgba;
use dvs_core::error::Result;
use dvs_core::op::Warning;
use dvs_core::project::{Asset, Blend, Clip, Project, Sequence, Source, Track, TrackKind, Transform};
use dvs_core::time::{Fps, Time};
use std::path::PathBuf;

/// A clip placed on the frame grid.
#[derive(Debug, Clone, Copy)]
pub struct Placed<'a> {
    pub clip: &'a Clip,
    /// First timeline frame the clip occupies.
    pub start: i64,
    /// Number of frames occupied. Always at least one: a clip shorter than a frame still
    /// has to show up somewhere, and a zero-length entry is invalid in every format here.
    pub frames: i64,
    /// First source frame, at the sequence frame rate and before any retiming.
    pub source_in: i64,
}

impl Placed<'_> {
    /// One past the last frame, i.e. where the next entry starts.
    pub fn end(&self) -> i64 {
        self.start + self.frames
    }

    /// What a human recognizes this clip by in an exported file.
    pub fn label(&self) -> String {
        self.clip
            .name
            .clone()
            .unwrap_or_else(|| self.clip.source.describe())
    }
}

/// A track entry: media, or the hole before it.
#[derive(Debug, Clone, Copy)]
pub enum Entry<'a> {
    /// A hole of `frames` frames. Black on video, silence on audio.
    Blank { frames: i64 },
    Clip(Placed<'a>),
}

/// A track reduced to a gap-free sequence of entries.
#[derive(Debug, Clone)]
pub struct FlatTrack<'a> {
    pub track: &'a Track,
    pub entries: Vec<Entry<'a>>,
    /// Total length in frames, blanks included.
    pub frames: i64,
    /// Clips that were dropped on the way in, with the reason.
    pub warnings: Vec<Warning>,
}

impl<'a> FlatTrack<'a> {
    /// The placed clips, without the blanks.
    pub fn clips(&self) -> impl Iterator<Item = &Placed<'a>> {
        self.entries.iter().filter_map(|entry| match entry {
            Entry::Clip(placed) => Some(placed),
            Entry::Blank { .. } => None,
        })
    }
}

/// Frame position of an instant, rounded to the nearest frame.
pub fn frame_of(at: Time, fps: Fps) -> i64 {
    at.frame_round(fps)
}

/// Video and audio tracks flattened onto the frame grid, in document order — which is
/// bottom-to-top, the same direction MLT numbers its tracks in.
///
/// Caption tracks carry cues rather than clips and are not part of this; they leave the
/// project through [`crate::subtitle`].
pub fn flatten(sequence: &Sequence) -> Vec<FlatTrack<'_>> {
    let fps = sequence.fps;
    let mut flat = Vec::with_capacity(sequence.tracks.len());
    for track in &sequence.tracks {
        if track.kind == TrackKind::Caption {
            continue;
        }
        let mut entries: Vec<Entry> = Vec::with_capacity(track.clips.len() * 2);
        let mut warnings = Vec::new();
        let mut cursor = 0i64;
        for clip in &track.clips {
            let wanted = frame_of(clip.start, fps);
            let measured = frame_of(clip.end(), fps) - wanted;
            let frames = measured.max(1);
            if measured < 1 {
                warnings.push(Warning {
                    code: "sub-frame-clip",
                    target: clip.label().to_string(),
                    detail: format!(
                        "'{}' is shorter than one frame ({}); it was widened to a single frame, \
                         which moves everything after it on this track",
                        clip.label(),
                        clip.duration
                    ),
                });
            }
            // Never place a clip before the end of the previous one: a format built on
            // consecutive entries has no way to express two clips on the same frame.
            let start = wanted.max(cursor);
            if start > cursor {
                entries.push(Entry::Blank {
                    frames: start - cursor,
                });
            }
            if !clip.enabled {
                // A disabled clip is not media the target should play, but the hole it
                // leaves has to stay the same length or every later cut moves.
                warnings.push(Warning {
                    code: "clip-disabled",
                    target: clip.label().to_string(),
                    detail: format!(
                        "'{}' is disabled and was exported as {frames} frames of blank",
                        clip.label()
                    ),
                });
                entries.push(Entry::Blank { frames });
                cursor = start + frames;
                continue;
            }
            entries.push(Entry::Clip(Placed {
                clip,
                start,
                frames,
                source_in: frame_of(clip.source_in, fps),
            }));
            cursor = start + frames;
        }
        flat.push(FlatTrack {
            track,
            entries,
            frames: cursor,
            warnings,
        });
    }
    flat
}

/// Where a clip's pixels or samples come from, in terms an interchange file can state.
#[derive(Debug, Clone)]
pub enum Resource<'a> {
    /// A blob in the asset store, with its absolute path. Interchange files are read by
    /// other programs with their own working directories, so the path is never relative.
    File { asset: &'a Asset, path: PathBuf },
    /// A flat color. Titles, generators and nested sequences degrade to this.
    Color(Rgba),
}

impl Resource<'_> {
    /// `#AARRGGBB`, the byte order MLT's `color` producer parses.
    pub fn mlt_color(color: Rgba) -> String {
        format!(
            "#{:02x}{:02x}{:02x}{:02x}",
            color.a, color.r, color.g, color.b
        )
    }
}

/// Resolve a clip's source, degrading anything an interchange file cannot hold to a
/// transparent color and saying so.
///
/// Silently dropping a title would leave the human who opens the export in Kdenlive
/// looking at a timeline that is subtly *shorter* than the agent's — so the placeholder
/// keeps the duration and the warning explains the hole.
pub fn resolve_source<'a>(
    project: &'a Project,
    assets: &AssetStore,
    clip: &Clip,
) -> Result<(Resource<'a>, Option<Warning>)> {
    let label = clip.label().to_string();
    match &clip.source {
        Source::Asset { asset, .. } | Source::Image { asset } => {
            let asset = project.asset(asset)?;
            let path = assets.find(&asset.hash)?;
            Ok((Resource::File { asset, path }, None))
        }
        Source::Color { color } => Ok((Resource::Color(*color), None)),
        Source::Title { title } => {
            let name = project
                .titles
                .get(title)
                .map(|t| t.name.clone())
                .unwrap_or_else(|| title.to_string());
            Ok((
                Resource::Color(Rgba::TRANSPARENT),
                Some(Warning {
                    code: "title-not-representable",
                    target: label,
                    detail: format!(
                        "title '{name}' is a vector document only this engine renders; it was \
                         exported as a transparent placeholder of the same length"
                    ),
                }),
            ))
        }
        Source::Sequence { sequence } => {
            let name = project
                .sequences
                .get(sequence)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| sequence.to_string());
            Ok((
                Resource::Color(Rgba::TRANSPARENT),
                Some(Warning {
                    code: "nested-sequence-not-representable",
                    target: label,
                    detail: format!(
                        "nested sequence '{name}' was exported as a transparent placeholder; \
                         render it and import the result to hand it over"
                    ),
                }),
            ))
        }
        Source::Generator { generator, .. } => Ok((
            Resource::Color(Rgba::TRANSPARENT),
            Some(Warning {
                code: "generator-not-representable",
                target: label,
                detail: format!(
                    "the {} generator has no equivalent in this format and was exported as a \
                     transparent placeholder",
                    format!("{generator:?}").to_lowercase()
                ),
            }),
        )),
    }
}

/// Warnings for clip features the interchange formats cannot carry. Every writer reports
/// these, because "the export opened but my color grade is gone" is the failure this
/// crate exists to make impossible to hit silently.
pub fn feature_warnings(clip: &Clip, format: &'static str) -> Vec<Warning> {
    let mut warnings = Vec::new();
    let label = clip.label().to_string();
    for effect in clip.effects.iter().filter(|fx| fx.enabled) {
        warnings.push(Warning {
            code: "effect-not-exported",
            target: label.clone(),
            detail: format!(
                "effect '{}' has no {format} equivalent and was dropped",
                effect.kind
            ),
        });
    }
    if !clip.keyframes.is_empty() {
        warnings.push(Warning {
            code: "keyframes-not-exported",
            target: label.clone(),
            detail: format!(
                "{} animated parameter(s) were flattened to their static values",
                clip.keyframes.len()
            ),
        });
    }
    if clip.transform != Transform::default() || clip.crop.is_some() {
        warnings.push(Warning {
            code: "transform-not-exported",
            target: label.clone(),
            detail: format!(
                "the position, scale or crop of '{}' is not part of this {format} export; the \
                 clip arrives untransformed",
                clip.label()
            ),
        });
    }
    if clip.blend != Blend::Normal {
        warnings.push(Warning {
            code: "blend-not-exported",
            target: label,
            detail: format!("blend mode {:?} was exported as normal", clip.blend),
        });
    }
    warnings
}

/// A retime warning for the formats that cannot state one. MLT can (a `timewarp`
/// producer) and an EDL can (an `M2` record); FCPXML and OTIO would need a time map this
/// writer does not build, so the clip arrives at natural speed and the caller is told.
pub fn retime_warning(clip: &Clip, format: &'static str) -> Option<Warning> {
    if clip.speed.is_one() && !clip.reverse {
        return None;
    }
    Some(Warning {
        code: "speed-not-exported",
        target: clip.label().to_string(),
        detail: format!(
            "'{}' plays at {}{} here but arrives at natural speed in {format}; retime it again \
             in the editor",
            clip.label(),
            clip.speed,
            if clip.reverse { " reversed" } else { "" }
        ),
    })
}

/// A `file://` URL for an absolute path, percent-encoding the characters a URL may not
/// carry literally. FCPXML and OTIO both reference media this way and both reject a raw
/// space.
pub fn file_url(path: &std::path::Path) -> String {
    let mut url = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                url.push(byte as char)
            }
            other => url.push_str(&format!("%{other:02X}")),
        }
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::ids::ClipId;
    use dvs_core::project::{Sequence, Track, TrackKind};

    fn fps() -> Fps {
        Fps::new(30, 1).unwrap()
    }

    fn clip(id: &str, start: &str, duration: &str) -> Clip {
        let mut clip = Clip::new(
            Source::Color {
                color: Rgba::BLACK,
            },
            Time::parse(start).unwrap(),
            Time::parse(duration).unwrap(),
        );
        clip.id = ClipId::from_raw(id);
        clip
    }

    fn sequence(clips: Vec<Clip>) -> Sequence {
        let mut sequence = Sequence::new("main", fps(), [1920, 1080], 48000);
        let mut track = Track::new("V1", TrackKind::Video);
        track.clips = clips;
        sequence.tracks.push(track);
        sequence
    }

    #[test]
    fn rounding_ends_instead_of_durations_keeps_cuts_from_drifting() {
        // 0.05 s is a frame and a half at 30 fps. Rounding each duration would make every
        // clip two frames and push the third cut from frame 3 to frame 4.
        let sequence = sequence(vec![
            clip("clp_a", "0", "0.05"),
            clip("clp_b", "0.05", "0.05"),
            clip("clp_c", "0.1", "0.05"),
        ]);
        let flat = flatten(&sequence);
        let starts: Vec<i64> = flat[0].clips().map(|placed| placed.start).collect();
        let frames: Vec<i64> = flat[0].clips().map(|placed| placed.frames).collect();
        assert_eq!(starts, vec![0, 2, 3]);
        assert_eq!(frames, vec![2, 1, 2]);
        assert_eq!(flat[0].frames, 5);
        assert!(
            !flat[0]
                .entries
                .iter()
                .any(|entry| matches!(entry, Entry::Blank { .. })),
            "clips that touch must not be separated by a blank"
        );
    }

    #[test]
    fn a_clip_shorter_than_a_frame_is_widened_and_reported() {
        let sequence = sequence(vec![clip("clp_a", "0", "1/300"), clip("clp_b", "1/300", "1")]);
        let flat = flatten(&sequence);
        assert_eq!(
            flat[0].clips().map(|p| p.frames).collect::<Vec<_>>(),
            vec![1, 30]
        );
        assert_eq!(flat[0].clips().map(|p| p.start).collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(
            flat[0].warnings.first().map(|w| w.code),
            Some("sub-frame-clip")
        );
    }

    #[test]
    fn a_hole_becomes_a_blank_of_exactly_its_frame_count() {
        let sequence = sequence(vec![clip("clp_a", "0", "1"), clip("clp_b", "1.5", "1")]);
        let flat = flatten(&sequence);
        let blanks: Vec<i64> = flat[0]
            .entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Blank { frames } => Some(*frames),
                _ => None,
            })
            .collect();
        assert_eq!(blanks, vec![15]);
        assert_eq!(flat[0].frames, 75);
    }

    #[test]
    fn a_disabled_clip_keeps_its_length_as_blank() {
        let mut clips = vec![clip("clp_a", "0", "1"), clip("clp_b", "1", "2")];
        clips[0].enabled = false;
        let sequence = sequence(clips);
        let flat = flatten(&sequence);
        assert_eq!(flat[0].frames, 90);
        assert_eq!(flat[0].clips().count(), 1);
        assert_eq!(
            flat[0].warnings.first().map(|w| w.code),
            Some("clip-disabled")
        );
    }

    #[test]
    fn caption_tracks_are_not_playlists() {
        let mut sequence = sequence(vec![clip("clp_a", "0", "1")]);
        sequence
            .tracks
            .push(Track::new("CC1", TrackKind::Caption));
        assert_eq!(flatten(&sequence).len(), 1);
    }

    #[test]
    fn file_urls_escape_what_a_url_cannot_hold() {
        let url = file_url(std::path::Path::new("/tmp/my clips/a&b.mp4"));
        assert_eq!(url, "file:///tmp/my%20clips/a%26b.mp4");
    }
}
