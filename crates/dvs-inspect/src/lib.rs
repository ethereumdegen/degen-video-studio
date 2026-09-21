//! The feedback channel: what an agent gets instead of watching the video.
//!
//! Every other crate in this workspace answers "make this change". This one answers "what
//! did that actually do?", which is the harder half when the operator cannot see the
//! result. A human editor scrubs the timeline and knows in two seconds that V1 has a hole
//! in it, that the lower third is off the bottom of the frame, or that the music is
//! drowning the voice. An agent has none of that, and without a measurement it will keep
//! editing confidently in the wrong direction.
//!
//! So this crate turns the things a person would *notice* into numbers and ids:
//!
//! - [`digest`] — one JSON document describing a sequence: clips, gaps, titles, captions,
//!   loudness, black/frozen/scene-cut ranges. Emitted with every render and on demand.
//! - [`lint`] — the specific mistakes a blind editor makes, each finding carrying a
//!   selector-shaped `target` so the fix is a directly runnable op.
//! - [`analyze`] — the pixel detectors underneath both: black, frozen, flash, scene cuts.
//! - [`sheet`] — a contact sheet with burned-in timecodes, and a single annotated frame
//!   with every layer's bounding box numbered. A vision model gets stable handles rather
//!   than "the text near the top".
//! - [`diff`] — perceptual comparison of two rendered files: SSIM per sample plus the time
//!   ranges that changed. Used for intent checking and for golden tests.
//!
//! Two properties hold throughout. Everything is measured from the same compositor the
//! renderer uses, so a digest cannot disagree with the output. And everything is sampled at
//! a stated interval rather than per frame, because a per-frame analysis of a ten-minute
//! 1080p timeline is a full render — see [`digest::DigestOptions::sample_every`].

use dvs_core::engine::Workspace;

pub mod analyze;
pub mod diff;
pub mod digest;
pub mod lint;
pub mod ops;
pub mod sheet;

pub use analyze::{analyze, scenes, AnalyzeOptions, FrameStat, VideoAnalysis};
pub use diff::{diff, ssim, DiffReport, SampleDiff};
pub use digest::{
    digest, AudioDigest, CaptionDigest, ClipDigest, Digest, DigestOptions, GapDigest, TitleDigest,
    TrackDigest, VideoDigest,
};
pub use lint::{lint, Finding, LintOptions, LoudnessProfile, Severity};
pub use sheet::{annotate_frame, contact_sheet, Annotation, SheetCell, SheetReport};

/// The three things an inspection needs from an open project: the document, where it lives,
/// and the media store behind it.
///
/// It exists because the two callers of this crate hold different things. A CLI or the
/// Tauri shell has a [`Workspace`]. An op does not — `Op::apply` is handed
/// `&mut Project` plus an [`OpCx`], deliberately, so that an op can run against a document
/// the engine is about to validate and discard. Rather than have two spellings of every
/// entry point, every public function here takes `impl Into<Subject>`: pass `&Workspace`
/// from the outside, or build one from the op context with [`Subject::new`].
///
/// [`OpCx`]: dvs_core::op::OpCx
#[derive(Debug, Clone, Copy)]
pub struct Subject<'a> {
    pub project: &'a dvs_core::project::Project,
    pub paths: &'a dvs_core::paths::ProjectPaths,
    pub assets: &'a dvs_core::asset::AssetStore,
}

impl<'a> Subject<'a> {
    pub fn new(
        project: &'a dvs_core::project::Project,
        paths: &'a dvs_core::paths::ProjectPaths,
        assets: &'a dvs_core::asset::AssetStore,
    ) -> Subject<'a> {
        Subject {
            project,
            paths,
            assets,
        }
    }
}

impl<'a> From<&'a Workspace> for Subject<'a> {
    fn from(workspace: &'a Workspace) -> Subject<'a> {
        Subject::new(&workspace.project, &workspace.paths, &workspace.assets)
    }
}

/// Add this crate's ops to a registry: the `inspect.*` queries plus the two mutations that
/// act on what they found (`marker.from-scenes`, `seq.auto-cut-scenes`).
pub fn register(registry: &mut dvs_core::op::Registry) {
    ops::register(registry);
}

/// Fixtures shared by every module's tests.
///
/// A private module at the crate root is visible to its descendants, so `lint::tests` and
/// `sheet::tests` both build their documents here. One builder means a rule's "triggers"
/// and "does not trigger" fixtures differ only in the thing under test, which is the whole
/// point of the pair.
#[cfg(test)]
pub(crate) mod tests {
    use dvs_core::asset::AssetStore;
    use dvs_core::color::Rgba;
    use dvs_core::engine::Workspace;
    use dvs_core::ids::{AssetId, SequenceId, TitleId, TrackId};
    use dvs_core::paths::ProjectPaths;
    use dvs_core::project::{
        Asset, AssetKind, CaptionCue, Clip, ColorMatrix, ColorRange, Probe, Project, Sequence,
        Source, Title, Track, TrackKind, VideoStream,
    };
    use dvs_core::time::{Fps, Rat, Span, Time};
    use dvs_core::vfs::FsVfs;
    use dvs_media::Toolchain;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    pub(crate) const RATE: u32 = 48_000;

    pub(crate) fn tool() -> &'static Toolchain {
        Toolchain::shared().expect("these tests measure real media and need ffmpeg on PATH")
    }

    pub(crate) fn fps() -> Fps {
        Fps::new(30, 1).expect("30/1 is a valid rate")
    }

    pub(crate) fn secs(num: i64, den: i64) -> Time {
        Time::new(num, den).expect("non-zero denominator")
    }

    /// A project on disk, with helpers that build documents directly rather than through
    /// ops — a lint has to be able to see states that the ops refuse to produce, such as
    /// two clips overlapping.
    pub(crate) struct Fixture {
        pub(crate) dir: TempDir,
        pub(crate) ws: Workspace,
    }

    impl Fixture {
        pub(crate) fn new(size: [u32; 2]) -> Fixture {
            let dir = TempDir::new().expect("tempdir");
            let paths = ProjectPaths::new(dir.path());
            let project = Project::new("probe", fps(), size, RATE);
            let ws = Workspace::create(paths, project, FsVfs::shared()).expect("create project");
            Fixture { dir, ws }
        }

        pub(crate) fn seq(&self) -> SequenceId {
            self.ws.project.active_sequence.clone()
        }

        pub(crate) fn sequence(&self) -> &Sequence {
            self.ws
                .project
                .sequence(&self.ws.project.active_sequence)
                .expect("active sequence")
        }

        pub(crate) fn sequence_mut(&mut self) -> &mut Sequence {
            let id = self.seq();
            self.ws.project.sequence_mut(&id).expect("active sequence")
        }

        pub(crate) fn track(&mut self, kind: TrackKind) -> TrackId {
            let seq = self.sequence_mut();
            let name = seq.next_track_name(kind);
            let track = Track::new(name, kind);
            let id = track.id.clone();
            seq.tracks.push(track);
            id
        }

        pub(crate) fn video(&mut self) -> TrackId {
            self.track(TrackKind::Video)
        }

        /// Append a clip without the sorted/no-overlap enforcement `Track::place` applies.
        pub(crate) fn push(&mut self, track: &TrackId, clip: Clip) -> dvs_core::ids::ClipId {
            let id = clip.id.clone();
            self.sequence_mut()
                .track_mut(track)
                .expect("track")
                .clips
                .push(clip);
            id
        }

        pub(crate) fn cue(&mut self, track: &TrackId, span: Span, text: &str) {
            let cue = CaptionCue {
                id: dvs_core::ids::CueId::new(),
                span,
                text: text.to_string(),
                style: None,
            };
            self.sequence_mut()
                .track_mut(track)
                .expect("track")
                .cues
                .push(cue);
        }

        /// An asset entry with a hand-built probe.
        ///
        /// `stored` decides whether bytes exist behind the hash. The bytes are not media:
        /// the probe in the document is what every document-only rule reads, and the store
        /// only ever asks "is this hash present".
        pub(crate) fn asset(&mut self, name: &str, probe: Probe, stored: bool) -> AssetId {
            let hash = if stored {
                self.ws
                    .assets
                    .import_bytes(name.as_bytes(), "mp4")
                    .expect("store bytes")
            } else {
                AssetStore::hash_bytes(name.as_bytes())
            };
            let asset = Asset {
                id: AssetId::new(),
                name: name.to_string(),
                hash,
                kind: AssetKind::Video,
                probe,
                proxy: None,
                source_path: None,
                imported: chrono::Utc::now(),
                provenance: None,
            };
            let id = asset.id.clone();
            self.ws.project.assets.insert(id.clone(), asset);
            id
        }

        /// Import a real file: hash the bytes into the store and probe them.
        pub(crate) fn import(&mut self, path: &Path, name: &str) -> AssetId {
            let hash = self.ws.assets.import_path(path).expect("import bytes");
            let probed = dvs_media::probe(tool(), path).expect("probe");
            let asset = Asset {
                id: AssetId::new(),
                name: name.to_string(),
                hash,
                kind: probed.kind,
                probe: probed.probe,
                proxy: None,
                source_path: Some(path.display().to_string()),
                imported: chrono::Utc::now(),
                provenance: None,
            };
            let id = asset.id.clone();
            self.ws.project.assets.insert(id.clone(), asset);
            id
        }

        pub(crate) fn title(&mut self, name: &str, size: [u32; 2], svg: String) -> TitleId {
            let title = Title {
                id: TitleId::new(),
                name: name.to_string(),
                size,
                svg,
                fields: Default::default(),
            };
            let id = title.id.clone();
            self.ws.project.titles.insert(id.clone(), title);
            id
        }

        /// A second sequence with `duration` of picture in it, for nesting.
        pub(crate) fn nested(&mut self, duration: Time) -> SequenceId {
            let mut seq = Sequence::new("inner", fps(), self.sequence().size, RATE);
            let mut track = Track::new("V1", TrackKind::Video);
            track.clips.push(color_clip(Rgba::opaque(0, 80, 160), Time::ZERO, duration));
            seq.tracks.push(track);
            let id = seq.id.clone();
            self.ws.project.sequences.insert(id.clone(), seq);
            id
        }

        /// A 1 kHz tone as a real WAV file. `amplitude` is linear, so 1.0 is full scale.
        ///
        /// Written with `aevalsrc` rather than lavfi's `sine`, which on this ffmpeg emits
        /// −21 dBFS and has no amplitude option — a "full scale" fixture that is actually
        /// 21 dB down would make the true-peak and clipping tests pass for the wrong reason.
        pub(crate) fn tone(&self, name: &str, seconds: f64, amplitude: f64) -> PathBuf {
            let path = self.dir.path().join(name);
            let status = tool()
                .ffmpeg_command()
                .args([
                    "-f",
                    "lavfi",
                    "-i",
                    &format!(
                        "aevalsrc=exprs={amplitude}*sin(2*PI*1000*t):duration={seconds}:sample_rate={RATE}:channel_layout=stereo"
                    ),
                    "-c:a",
                    "pcm_s16le",
                ])
                .arg(&path)
                .output()
                .expect("run ffmpeg");
            assert!(
                status.status.success(),
                "tone fixture failed: {}",
                String::from_utf8_lossy(&status.stderr)
            );
            path
        }

        /// Real decodable video, for the measurements that actually open the file.
        pub(crate) fn synth(
            &self,
            name: &str,
            source: &str,
            seconds: i64,
            size: [u32; 2],
        ) -> PathBuf {
            let path = self.dir.path().join(name);
            dvs_media::decode::synthesize(
                tool(),
                &path,
                source,
                Time::from_secs(seconds),
                fps(),
                size,
            )
            .expect("synthesize a test source");
            path
        }

        /// Real video that also carries sound, which is what a camera or screen recorder
        /// produces and what a "video has audio" assertion needs.
        pub(crate) fn av(&self, name: &str, seconds: i64) -> PathBuf {
            let path = self.dir.path().join(name);
            let made = tool()
                .ffmpeg_command()
                .args([
                    "-f",
                    "lavfi",
                    "-i",
                    &format!("testsrc2=size=320x180:rate={}:duration={seconds}", fps().ffmpeg_arg()),
                    "-f",
                    "lavfi",
                    "-i",
                    &format!("aevalsrc=0.4*sin(2*PI*440*t):s={RATE}:d={seconds}"),
                ])
                .args(["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-shortest"])
                .arg(&path)
                .output()
                .expect("run ffmpeg");
            assert!(
                made.status.success(),
                "a/v fixture failed: {}",
                String::from_utf8_lossy(&made.stderr)
            );
            path
        }

        /// A tone on its own audio track, as a real imported asset.
        pub(crate) fn tone_track(&mut self, name: &str, span: Span, amplitude: f64) -> TrackId {
            let path = self.tone(name, span.duration().as_secs_f64(), amplitude);
            let asset = self.import(&path, name);
            let track = self.track(TrackKind::Audio);
            let clip = Clip::new(
                Source::Asset {
                    asset,
                    stream: None,
                },
                span.start,
                span.duration(),
            );
            self.push(&track, clip);
            track
        }
    }

    pub(crate) fn color_clip(color: Rgba, start: Time, duration: Time) -> Clip {
        Clip::new(Source::Color { color }, start, duration)
    }

    /// A probe describing a video file that need not exist.
    pub(crate) fn video_probe(size: [u32; 2], rate: Fps, duration: Time) -> Probe {
        Probe {
            duration,
            video: Some(VideoStream {
                stream_index: 0,
                size,
                fps: rate,
                codec: "h264".to_string(),
                pix_fmt: "yuv420p".to_string(),
                color_range: ColorRange::Tv,
                color_matrix: ColorMatrix::Bt709,
                color_primaries: None,
                transfer: None,
                rotation: 0,
                sar: Rat::ONE,
                frames: None,
                bit_rate: None,
            }),
            audio: None,
            vfr: false,
            container: "mp4".to_string(),
        }
    }

    /// A title document whose only content is one text run, positioned in document pixels.
    pub(crate) fn text_title(
        size: [u32; 2],
        x: f32,
        y: f32,
        font_size: f32,
        family: &str,
        fill: &str,
        text: &str,
    ) -> String {
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\">\
             <text x=\"{x}\" y=\"{y}\" font-family=\"{family}\" font-size=\"{font_size}\" fill=\"{fill}\">{text}</text>\
             </svg>",
            size[0], size[1], size[0], size[1]
        )
    }
}
