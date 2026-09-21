//! CMX3600 EDL: the oldest, dumbest, most universally readable description of a cut.
//!
//! An EDL carries almost nothing — no effects, no transforms, no nesting, one video
//! channel and a handful of audio ones — and that is exactly why it is worth writing. A
//! colorist, an online suite, a broadcast QC desk and a forty-year-old linear controller
//! all read it, and what it does carry is the list every one of them actually needs: for
//! each event, which reel, which source timecode, and where it lands on the record
//! timeline.
//!
//! Timecodes come straight from [`Time::timecode`], the same function the digest and the
//! CLI print, so an EDL and a `dvs inspect` report can never disagree about where a cut
//! is. Record out-points are exclusive, which matches half-open [`dvs_core::time::Span`]
//! and means event *n*'s record out is event *n+1*'s record in.

use crate::timeline::{self, Entry};
use crate::Export;
use dvs_core::error::Result;
use dvs_core::ids::SequenceId;
use dvs_core::op::Warning;
use dvs_core::project::{Project, Source, TrackKind};
use dvs_core::time::{Fps, Time};
use std::fmt::Write as _;

/// Write the sequence as a CMX3600 EDL.
pub fn export(project: &Project, sequence: &SequenceId) -> Result<Export> {
    let sequence = project.sequence(sequence)?;
    let fps = sequence.fps;
    let flat = timeline::flatten(sequence);
    let mut warnings: Vec<Warning> = flat.iter().flat_map(|t| t.warnings.clone()).collect();

    let mut out = String::new();
    let _ = writeln!(out, "TITLE: {}", title_of(&sequence.name));
    // Non-drop is the only timecode this engine produces; saying so is part of the
    // format, and an EDL read as drop-frame when it is not lands 108 frames out per hour.
    let _ = writeln!(out, "FCM: NON-DROP FRAME");

    let mut event = 0usize;
    let mut video_tracks = 0usize;
    let mut audio_tracks = 0usize;
    for track in &flat {
        let channel = match track.track.kind {
            TrackKind::Audio => {
                audio_tracks += 1;
                if audio_tracks == 1 {
                    "A".to_string()
                } else {
                    format!("A{audio_tracks}")
                }
            }
            _ => {
                video_tracks += 1;
                if video_tracks > 1 {
                    warnings.push(Warning {
                        code: "track-not-exported",
                        target: track.track.name.clone(),
                        detail: format!(
                            "an EDL has one video channel; the events from '{}' are written too, \
                             but a reader that honours record timecode will see them collide \
                             with V1",
                            track.track.name
                        ),
                    });
                }
                "V".to_string()
            }
        };

        for entry in &track.entries {
            let Entry::Clip(placed) = entry else {
                continue;
            };
            warnings.extend(timeline::feature_warnings(placed.clip, "EDL"));
            event += 1;
            let clip = placed.clip;
            let name = source_name(project, clip).unwrap_or_else(|| placed.label());
            let source_in = Time::from_frames(placed.source_in, fps);
            let source_out = source_in + Time::from_frames(placed.frames, fps);
            let record_in = Time::from_frames(placed.start, fps);
            let record_out = Time::from_frames(placed.start + placed.frames, fps);
            let transition = transition_field(clip, fps);
            let _ = writeln!(
                out,
                "{event:03}  {:<8} {:<5} {:<8} {} {} {} {}",
                reel_of(&name),
                channel,
                transition,
                source_in.timecode(fps),
                source_out.timecode(fps),
                record_in.timecode(fps),
                record_out.timecode(fps),
            );
            let _ = writeln!(out, "* FROM CLIP NAME: {name}");
            if let Some(kind) = dissolve_name(clip) {
                let _ = writeln!(out, "* EFFECT NAME: {kind}");
            }
            if !clip.speed.is_one() || clip.reverse {
                let speed = clip.speed.as_f64() * if clip.reverse { -1.0 } else { 1.0 };
                // M2 is how a CMX list states a retime: reel, play rate, source start.
                let _ = writeln!(
                    out,
                    "M2   {:<8} {:>11.1} {}",
                    reel_of(&name),
                    speed * fps.as_f64(),
                    source_in.timecode(fps)
                );
            }
        }
    }

    Ok(Export {
        text: out,
        warnings,
    })
}

/// EDL titles are upper case ASCII by convention and by the tolerance of the readers.
fn title_of(name: &str) -> String {
    name.to_uppercase()
        .chars()
        .map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '_' })
        .collect()
}

/// Eight characters of reel name, the field width the format allows. `AX` — "auxiliary"
/// — is the conventional placeholder for a file-based source with no tape reel.
fn reel_of(name: &str) -> String {
    let cleaned: String = name
        .to_uppercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    if cleaned.is_empty() {
        "AX".to_string()
    } else {
        cleaned
    }
}

/// `C` for a cut, `D nnn` for a dissolve of nnn frames.
fn transition_field(clip: &dvs_core::project::Clip, fps: Fps) -> String {
    match clip.transition_in.as_ref() {
        Some(transition)
            if transition.kind != dvs_core::project::TransitionKind::Cut
                && transition.duration.is_positive() =>
        {
            format!("D {:03}", transition.duration.frame_round(fps).max(1))
        }
        _ => "C".to_string(),
    }
}

fn dissolve_name(clip: &dvs_core::project::Clip) -> Option<&'static str> {
    let transition = clip.transition_in.as_ref()?;
    if transition.kind == dvs_core::project::TransitionKind::Cut
        || !transition.duration.is_positive()
    {
        return None;
    }
    Some(match transition.kind {
        dvs_core::project::TransitionKind::Dissolve => "CROSS DISSOLVE",
        dvs_core::project::TransitionKind::Dip => "DIP TO COLOR DISSOLVE",
        _ => "WIPE",
    })
}

/// The name a human recognizes the source by: the clip's own name if it has one, else
/// the media file it came from.
fn source_name(project: &Project, clip: &dvs_core::project::Clip) -> Option<String> {
    if let Some(name) = &clip.name {
        return Some(name.clone());
    }
    match &clip.source {
        Source::Asset { asset, .. } | Source::Image { asset } => {
            Some(project.asset(asset).ok()?.name.clone())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::fixture;
    use dvs_core::project::TransitionKind;

    #[test]
    fn timecodes_are_the_same_instants_the_engine_reports() {
        let fps = Fps::new(30000, 1001).unwrap();
        let mut fixture = fixture(fps);
        fixture.push_clip("V1", "0", "2", "1.5");
        let text = export(&fixture.project, &fixture.sequence)
            .expect("export")
            .text;
        let line = text
            .lines()
            .find(|line| line.starts_with("001"))
            .expect("an event");
        let fields: Vec<&str> = line.split_whitespace().collect();
        let source_in = Time::from_frames(Time::parse("1.5").unwrap().frame_round(fps), fps);
        let expected = [
            source_in.timecode(fps),
            (source_in + Time::from_frames(60, fps)).timecode(fps),
            Time::ZERO.timecode(fps),
            Time::from_frames(60, fps).timecode(fps),
        ];
        assert_eq!(&fields[fields.len() - 4..], &expected[..], "{line}");
        assert_eq!(expected[0], "00:00:01:15");
        assert_eq!(expected[3], "00:00:02:00");
    }

    #[test]
    fn events_are_numbered_and_record_times_are_contiguous() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        fixture.push_second("V1", "1", "2", "0");
        fixture.push_clip("A1", "0", "3", "0");
        let text = export(&fixture.project, &fixture.sequence)
            .expect("export")
            .text;
        assert!(text.starts_with("TITLE: MAIN\nFCM: NON-DROP FRAME\n"), "{text}");
        let events: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("00"))
            .collect();
        assert_eq!(events.len(), 3, "{text}");
        assert!(events[0].starts_with("001  A"), "{}", events[0]);
        assert!(events[1].contains(" V "), "{}", events[1]);
        assert!(events[2].contains(" A "), "audio channel: {}", events[2]);
        let first_out = events[0].split_whitespace().last().expect("record out");
        let second_in = events[1]
            .split_whitespace()
            .nth_back(1)
            .expect("record in");
        assert_eq!(
            first_out, second_in,
            "the record out of one event is the record in of the next"
        );
    }

    #[test]
    fn a_dissolve_is_a_d_event_with_its_length_in_frames() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        fixture.push_second("V1", "1", "1", "0");
        fixture.set_transition("V1", 1, TransitionKind::Dissolve, "0.5");
        let text = export(&fixture.project, &fixture.sequence)
            .expect("export")
            .text;
        assert!(text.contains("D 015"), "{text}");
        assert!(text.contains("* EFFECT NAME: CROSS DISSOLVE"), "{text}");
    }

    #[test]
    fn clip_names_become_reels_and_comments() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        fixture.name_clip("V1", 0, "interview take 2");
        let text = export(&fixture.project, &fixture.sequence)
            .expect("export")
            .text;
        assert!(text.contains("* FROM CLIP NAME: interview take 2"), "{text}");
        assert!(text.contains("INTERVIE"), "reel is eight characters: {text}");
    }

    #[test]
    fn a_retimed_clip_gets_an_m2_speed_record() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        fixture.set_speed("V1", 0, 2, 1);
        let text = export(&fixture.project, &fixture.sequence)
            .expect("export")
            .text;
        assert!(text.contains("M2   "), "{text}");
        assert!(text.contains("60.0"), "two times 30 fps: {text}");
    }
}
