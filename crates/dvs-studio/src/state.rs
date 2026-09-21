//! What the window shows, as data both halves agree on.
//!
//! Everything here is `Serialize` and camelCase, because the view layer is a webview and
//! these structs cross that boundary verbatim as the payload of a Tauri command. Keeping
//! the derived view *here* rather than sending the raw document has two reasons: the
//! document is owned by a worker while an agent may be rewriting it on disk, and the
//! frontend should never have to re-implement selector resolution or timeline arithmetic
//! to draw a row.
//!
//! Accessibility note that shapes the types: every visible element carries the text a
//! screen reader will announce (`ClipBox::label`, `ClipBox::announce`, `TrackRow::name`),
//! computed here in Rust rather than assembled from fragments in JavaScript. A clip is not
//! a coloured rectangle with a tooltip; it is a labelled control that happens to be drawn
//! as a rectangle.

use dvs_core::ids::{ClipId, SequenceId, TrackId};
use dvs_core::project::TrackKind;
use dvs_core::time::{Fps, Span, Time};
use serde::Serialize;
use std::path::PathBuf;

/// How the window was started.
#[derive(Debug, Clone)]
pub struct StudioOptions {
    /// Project directory. Discovered upward from the working directory when absent.
    pub root: PathBuf,
    /// Sequence to open; the project's active one when absent.
    pub sequence: Option<String>,
    /// Render scale for the viewport. Below 1.0 the picture is a preview, which is what
    /// makes scrubbing a 4K timeline feel like scrubbing.
    pub scale: f64,
    /// Decode from proxies when an asset has one.
    pub use_proxy: bool,
    /// Print the window's state as text and exit, instead of opening a window.
    ///
    /// This is the same information the window shows, in the form a terminal and a screen
    /// reader can already consume — the accessible path to "what does the studio see"
    /// without opening a GUI at all.
    pub describe: bool,
}

impl Default for StudioOptions {
    fn default() -> Self {
        StudioOptions {
            root: PathBuf::from("."),
            sequence: None,
            scale: 0.5,
            use_proxy: true,
            describe: false,
        }
    }
}

/// One clip as the timeline draws it, and as a screen reader announces it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipBox {
    pub id: ClipId,
    pub track: TrackId,
    /// Row index as drawn, top to bottom — the reverse of document order, because an
    /// editor puts the topmost layer at the top.
    pub row: usize,
    pub start: Time,
    pub end: Time,
    /// Short label drawn inside the rectangle.
    pub label: String,
    /// The full sentence assistive technology reads, e.g.
    /// "intro, video clip on V1, 0 to 4.004 seconds, dissolve in over 0.5 seconds".
    pub announce: String,
    /// What it plays, for the detail line.
    pub source: String,
    pub kind: ClipKind,
    pub enabled: bool,
    /// Transition into this clip: kind and duration.
    pub transition: Option<TransitionBadge>,
    /// Set for clips the newest journal entry touched, so the view can highlight what just
    /// changed — and so the live region can say it.
    pub touched: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransitionBadge {
    pub kind: String,
    pub duration: Time,
}

/// Clip family. Drives colour *and* the word in the label, because colour alone is not a
/// distinction for a third of readers and none at all for a screen reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClipKind {
    Video,
    Audio,
    Title,
    Image,
    Color,
    Generator,
    Nested,
    Caption,
}

impl ClipKind {
    /// The word a reader hears and a label shows.
    pub fn word(self) -> &'static str {
        match self {
            ClipKind::Video => "video",
            ClipKind::Audio => "audio",
            ClipKind::Title => "title",
            ClipKind::Image => "image",
            ClipKind::Color => "color",
            ClipKind::Generator => "generator",
            ClipKind::Nested => "sequence",
            ClipKind::Caption => "caption",
        }
    }
}

/// One track row.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackRow {
    pub id: TrackId,
    pub name: String,
    pub kind: TrackKind,
    pub muted: bool,
    pub locked: bool,
    pub hidden: bool,
    pub clip_count: usize,
    /// "V1, video track, 3 clips, muted" — the row's own announcement.
    pub announce: String,
}

/// A hole nothing covers: black picture or silence, and what lint reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GapBox {
    pub row: usize,
    pub track: TrackId,
    pub start: Time,
    pub end: Time,
    pub announce: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkerPin {
    pub at: Time,
    pub name: String,
}

/// The flattened timeline the view draws.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineModel {
    pub rows: Vec<TrackRow>,
    pub clips: Vec<ClipBox>,
    pub gaps: Vec<GapBox>,
    pub markers: Vec<MarkerPin>,
    pub duration: Time,
    pub fps: Fps,
}

impl TimelineModel {
    /// Clip covering an instant on a row.
    pub fn clip_at(&self, row: usize, at: Time) -> Option<&ClipBox> {
        self.clips
            .iter()
            .find(|clip| clip.row == row && Span::new(clip.start, clip.end).contains(at))
    }
}

/// A journal entry as the activity feed shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    pub seq: u64,
    /// `agent`, `human` or `ai`.
    pub actor: String,
    pub op: String,
    /// The two or three arguments that matter, already formatted.
    pub summary: String,
    /// Local wall-clock time, `HH:MM:SS`.
    pub at: String,
    /// The whole line, for the live region: "agent ran clip.split on #intro at frame 1274".
    pub announce: String,
    /// True for entries that arrived while the window was open.
    pub fresh: bool,
}

/// A lint finding, flattened for display.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub rule: String,
    pub severity: String,
    pub target: String,
    pub detail: String,
    /// The clip this names, when it names one, so a click can select it.
    pub clip: Option<ClipId>,
}

/// Everything the view needs to draw itself once.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub project_name: String,
    pub root: String,
    pub sequence: SequenceId,
    pub sequence_name: String,
    pub size: [u32; 2],
    pub fps: Fps,
    pub duration: Time,
    pub frame_count: i64,
    pub timeline: TimelineModel,
    pub activity: Vec<Activity>,
    /// Journal length: the view compares it to decide whether anything actually changed.
    pub revision: u64,
    /// Viewport scale and proxy state, so the window can say what it is showing.
    pub scale: f64,
    pub use_proxy: bool,
}

/// What an applied op reports back: the summary line, plus anything the op wanted to say.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Applied {
    pub op: String,
    pub summary: String,
    pub changed: Vec<String>,
    pub created: Vec<String>,
    pub removed: Vec<String>,
    pub warnings: Vec<String>,
    /// "42.5 s snapped to frame 1274" — the thing a human most needs told.
    pub snapped: Vec<String>,
}

/// Render a snapshot as plain text.
///
/// `dvs-studio --describe` prints this. It exists because a window is not an interface
/// everyone can use: the same timeline, the same activity, the same findings, as lines a
/// terminal and a screen reader already handle.
pub fn describe(snapshot: &Snapshot, findings: &[Finding]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} · sequence {} · {}×{} · {} fps · {} ({} frames)\n",
        snapshot.project_name,
        snapshot.sequence_name,
        snapshot.size[0],
        snapshot.size[1],
        snapshot.fps,
        snapshot.duration.clock(),
        snapshot.frame_count
    ));
    out.push_str(&format!("{}\n\n", snapshot.root));

    out.push_str("timeline\n");
    for (index, row) in snapshot.timeline.rows.iter().enumerate() {
        out.push_str(&format!("  {}\n", row.announce));
        for clip in snapshot.timeline.clips.iter().filter(|c| c.row == index) {
            out.push_str(&format!("    {}\n", clip.announce));
        }
        for gap in snapshot.timeline.gaps.iter().filter(|g| g.row == index) {
            out.push_str(&format!("    {}\n", gap.announce));
        }
    }
    if !snapshot.timeline.markers.is_empty() {
        out.push_str("markers\n");
        for marker in &snapshot.timeline.markers {
            out.push_str(&format!("  {} {}\n", marker.at.clock(), marker.name));
        }
    }

    out.push_str("\nactivity\n");
    if snapshot.activity.is_empty() {
        out.push_str("  (nothing yet)\n");
    }
    for entry in snapshot.activity.iter().take(20) {
        out.push_str(&format!("  {} {}\n", entry.at, entry.announce));
    }

    out.push_str("\nlint\n");
    if findings.is_empty() {
        out.push_str("  no findings\n");
    }
    for finding in findings {
        out.push_str(&format!(
            "  {} [{}] {} — {}\n",
            finding.severity, finding.rule, finding.target, finding.detail
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        let fps = Fps::new(30, 1).expect("fps");
        Snapshot {
            project_name: "promo".into(),
            root: "/tmp/promo".into(),
            sequence: SequenceId::from_raw("seq_main"),
            sequence_name: "main".into(),
            size: [1920, 1080],
            fps,
            duration: Time::from_secs(6),
            frame_count: 180,
            timeline: TimelineModel {
                rows: vec![TrackRow {
                    id: TrackId::from_raw("trk_v1"),
                    name: "V1".into(),
                    kind: TrackKind::Video,
                    muted: false,
                    locked: false,
                    hidden: false,
                    clip_count: 1,
                    announce: "V1, video track, 1 clip".into(),
                }],
                clips: vec![ClipBox {
                    id: ClipId::from_raw("clp_intro"),
                    track: TrackId::from_raw("trk_v1"),
                    row: 0,
                    start: Time::ZERO,
                    end: Time::from_secs(4),
                    label: "intro".into(),
                    announce: "intro, video clip on V1, 0 to 4 seconds".into(),
                    source: "talk.mp4".into(),
                    kind: ClipKind::Video,
                    enabled: true,
                    transition: None,
                    touched: false,
                }],
                gaps: vec![GapBox {
                    row: 0,
                    track: TrackId::from_raw("trk_v1"),
                    start: Time::from_secs(4),
                    end: Time::from_secs(6),
                    announce: "2 seconds of black on V1 from 4 to 6 seconds".into(),
                }],
                markers: vec![MarkerPin {
                    at: Time::from_secs(2),
                    name: "beat".into(),
                }],
                duration: Time::from_secs(6),
                fps,
            },
            activity: vec![Activity {
                seq: 7,
                actor: "agent".into(),
                op: "clip.split".into(),
                summary: "--target #intro --at 42.5".into(),
                at: "11:42:03".into(),
                announce: "agent ran clip.split on #intro, snapped to frame 1274".into(),
                fresh: true,
            }],
            revision: 7,
            scale: 0.5,
            use_proxy: true,
        }
    }

    #[test]
    fn the_text_view_carries_what_the_window_shows() {
        let findings = vec![Finding {
            rule: "gap".into(),
            severity: "warning".into(),
            target: "trk_v1".into(),
            detail: "2.000s of black on V1".into(),
            clip: None,
        }];
        let text = describe(&snapshot(), &findings);
        // Everything a sighted user reads off the window has to be in here, or the text
        // path is a decoration rather than an alternative.
        for expected in [
            "promo",
            "sequence main",
            "1920×1080",
            "V1, video track, 1 clip",
            "intro, video clip on V1",
            "2 seconds of black on V1",
            "beat",
            "agent ran clip.split",
            "warning [gap]",
        ] {
            assert!(text.contains(expected), "describe() is missing {expected:?}:\n{text}");
        }
    }

    #[test]
    fn an_empty_project_still_describes_itself() {
        let mut empty = snapshot();
        empty.timeline = TimelineModel::default();
        empty.activity.clear();
        let text = describe(&empty, &[]);
        assert!(text.contains("(nothing yet)"), "{text}");
        assert!(text.contains("no findings"), "{text}");
    }

    #[test]
    fn the_snapshot_serializes_with_the_names_the_frontend_reads() {
        let json = serde_json::to_value(snapshot()).expect("serialize");
        for path in ["projectName", "sequenceName", "frameCount", "timeline", "activity", "useProxy"] {
            assert!(json.get(path).is_some(), "snapshot is missing '{path}'");
        }
        let clip = &json["timeline"]["clips"][0];
        for path in ["announce", "kind", "start", "end", "row"] {
            assert!(clip.get(path).is_some(), "clip is missing '{path}'");
        }
        assert_eq!(clip["kind"], "video");
        // Times cross the boundary as the exact rationals they are, not as floats.
        assert_eq!(clip["end"], "4/1");
    }

    #[test]
    fn clip_kinds_have_a_word_a_reader_can_hear() {
        for kind in [
            ClipKind::Video,
            ClipKind::Audio,
            ClipKind::Title,
            ClipKind::Image,
            ClipKind::Color,
            ClipKind::Generator,
            ClipKind::Nested,
            ClipKind::Caption,
        ] {
            assert!(!kind.word().is_empty());
        }
        // Distinct words: colour is not the only thing distinguishing two kinds.
        let words: std::collections::BTreeSet<&str> = [
            ClipKind::Video,
            ClipKind::Audio,
            ClipKind::Title,
            ClipKind::Image,
            ClipKind::Color,
            ClipKind::Generator,
            ClipKind::Nested,
            ClipKind::Caption,
        ]
        .into_iter()
        .map(ClipKind::word)
        .collect();
        assert_eq!(words.len(), 8);
    }

    #[test]
    fn clip_lookup_is_half_open_like_the_timeline() {
        let snap = snapshot();
        assert!(snap.timeline.clip_at(0, Time::from_secs(0)).is_some());
        assert!(snap.timeline.clip_at(0, Time::from_secs(4)).is_none(), "the out-point belongs to the next clip");
        assert!(snap.timeline.clip_at(1, Time::from_secs(1)).is_none(), "no such row");
    }
}
