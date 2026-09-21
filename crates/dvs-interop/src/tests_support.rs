//! Shared fixtures for this crate's tests.
//!
//! The writers resolve every clip through the asset store, so a fixture needs real files
//! on disk, and the `melt` acceptance test needs those files to be real media. ffmpeg
//! synthesizes them once per test process and each fixture imports a copy into its own
//! store, which keeps the tests independent without paying for an encode per test.

use dvs_core::asset::AssetStore;
use dvs_core::ids::{AssetId, StyleId, TitleId};
use dvs_core::ids::SequenceId;
use dvs_core::paths::ProjectPaths;
use dvs_core::project::{
    Asset, AssetKind, AudioStream, CaptionCue, ColorMatrix, ColorRange, Clip, Probe, Project,
    Sequence, Source, Title, Track, TrackKind, Transition, TransitionKind, VideoStream,
};
use dvs_core::time::{Fps, Rat, Span, Time};
use dvs_core::vfs::FsVfs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Four seconds of 320×240 30 fps picture with a 440 Hz tone, synthesized once.
static MEDIA: LazyLock<PathBuf> = LazyLock::new(|| {
    let dir = std::env::temp_dir().join(format!("dvs-interop-media-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    for (name, source) in [("a.mp4", "testsrc2"), ("b.mp4", "smptebars")] {
        let path = dir.join(name);
        if path.exists() {
            continue;
        }
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args([
                "-f",
                "lavfi",
                "-i",
                &format!("{source}=size=320x240:rate=30:duration=4"),
            ])
            .args([
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=4",
            ])
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"])
            .args(["-c:a", "aac", "-shortest"])
            .arg(&path)
            .status()
            .expect("ffmpeg is installed");
        assert!(status.success(), "ffmpeg could not synthesize {name}");
    }
    dir
});

pub struct Fixture {
    /// Kept alive so the project directory outlives the test.
    _dir: tempfile::TempDir,
    pub paths: ProjectPaths,
    pub assets: AssetStore,
    pub project: Project,
    pub sequence: SequenceId,
    pub asset: AssetId,
    pub second_asset: AssetId,
}

/// A project at `fps` with a 320×240 frame, one video track, one audio track, and two
/// four-second assets in a real content-addressed store.
pub fn fixture(fps: Fps) -> Fixture {
    let dir = tempfile::tempdir().expect("temp project");
    let paths = ProjectPaths::new(dir.path());
    let assets = AssetStore::new(paths.assets_dir(), FsVfs::shared());

    let mut project = Project::new("promo", fps, [320, 240], 48000);
    let sequence = project.active_sequence.clone();
    {
        let sequence = project.sequence_mut(&sequence).expect("main");
        sequence.tracks.push(Track::new("V1", TrackKind::Video));
        sequence.tracks.push(Track::new("A1", TrackKind::Audio));
    }

    let asset = import(&mut project, &assets, "a.mp4", "ast_a");
    let second_asset = import(&mut project, &assets, "b.mp4", "ast_b");

    Fixture {
        _dir: dir,
        paths,
        assets,
        project,
        sequence,
        asset,
        second_asset,
    }
}

fn import(project: &mut Project, assets: &AssetStore, file: &str, id: &str) -> AssetId {
    let path = MEDIA.join(file);
    let hash = assets.import_path(&path).expect("import into the store");
    let id = AssetId::from_raw(id);
    project.assets.insert(
        id.clone(),
        Asset {
            id: id.clone(),
            name: file.to_string(),
            hash,
            kind: AssetKind::Video,
            probe: Probe {
                duration: Time::from_secs(4),
                video: Some(VideoStream {
                    stream_index: 0,
                    size: [320, 240],
                    fps: Fps::new(30, 1).expect("30"),
                    codec: "h264".to_string(),
                    pix_fmt: "yuv420p".to_string(),
                    color_range: ColorRange::Tv,
                    color_matrix: ColorMatrix::Bt709,
                    color_primaries: None,
                    transfer: None,
                    rotation: 0,
                    sar: Rat::ONE,
                    frames: Some(120),
                    bit_rate: None,
                }),
                audio: Some(AudioStream {
                    stream_index: 1,
                    rate: 48000,
                    channels: 2,
                    codec: "aac".to_string(),
                    bit_rate: None,
                }),
                vfr: false,
                container: "mov,mp4,m4a".to_string(),
            },
            proxy: None,
            source_path: Some(path.display().to_string()),
            imported: chrono::Utc::now(),
            provenance: None,
        },
    );
    id
}

impl Fixture {
    pub fn sequence_ref(&self) -> &Sequence {
        self.project.sequence(&self.sequence).expect("sequence")
    }

    pub fn sequence_mut(&mut self) -> &mut Sequence {
        let id = self.sequence.clone();
        self.project.sequence_mut(&id).expect("sequence")
    }

    fn track_index(&self, name: &str) -> usize {
        self.sequence_ref()
            .tracks
            .iter()
            .position(|track| track.name == name)
            .unwrap_or_else(|| panic!("no track named {name}"))
    }

    /// Append a clip cut from the first asset.
    pub fn push_clip(&mut self, track: &str, start: &str, duration: &str, source_in: &str) {
        let asset = self.asset.clone();
        self.push_source(
            track,
            Source::Asset {
                asset,
                stream: None,
            },
            start,
            duration,
            source_in,
        );
    }

    /// Append a clip cut from the second asset, so exports have two distinct producers.
    pub fn push_second(&mut self, track: &str, start: &str, duration: &str, source_in: &str) {
        let asset = self.second_asset.clone();
        self.push_source(
            track,
            Source::Asset {
                asset,
                stream: None,
            },
            start,
            duration,
            source_in,
        );
    }

    /// Append a title clip, which no interchange format can hold.
    pub fn push_title(&mut self, track: &str, start: &str, duration: &str) {
        let id = TitleId::from_raw("ttl_lower");
        self.project.titles.insert(
            id.clone(),
            Title {
                id: id.clone(),
                name: "lower third".to_string(),
                size: [320, 240],
                svg: "<svg/>".to_string(),
                fields: Default::default(),
            },
        );
        self.push_source(track, Source::Title { title: id }, start, duration, "0");
    }

    fn push_source(
        &mut self,
        track: &str,
        source: Source,
        start: &str,
        duration: &str,
        source_in: &str,
    ) {
        let index = self.track_index(track);
        let mut clip = Clip::new(
            source,
            Time::parse(start).expect("start"),
            Time::parse(duration).expect("duration"),
        );
        clip.source_in = Time::parse(source_in).expect("source in");
        self.sequence_mut().tracks[index].clips.push(clip);
    }

    pub fn set_transition(
        &mut self,
        track: &str,
        clip: usize,
        kind: TransitionKind,
        duration: &str,
    ) {
        let index = self.track_index(track);
        let duration = Time::parse(duration).expect("duration");
        self.sequence_mut().tracks[index].clips[clip].transition_in = Some(Transition {
            kind,
            duration,
            easing: Default::default(),
            direction: Default::default(),
            color: None,
        });
    }

    pub fn set_speed(&mut self, track: &str, clip: usize, num: i64, den: i64) {
        let index = self.track_index(track);
        self.sequence_mut().tracks[index].clips[clip].speed = Rat::new(num, den).expect("speed");
    }

    pub fn set_fades(&mut self, track: &str, clip: usize, fade_in: &str, fade_out: &str) {
        let index = self.track_index(track);
        let clip = &mut self.sequence_mut().tracks[index].clips[clip];
        clip.fade_in = Time::parse(fade_in).expect("fade in");
        clip.fade_out = Time::parse(fade_out).expect("fade out");
    }

    pub fn name_clip(&mut self, track: &str, clip: usize, name: &str) {
        let index = self.track_index(track);
        self.sequence_mut().tracks[index].clips[clip].name = Some(name.to_string());
    }

    pub fn rename_asset(&mut self, name: &str) {
        let id = self.asset.clone();
        if let Some(asset) = self.project.assets.get_mut(&id) {
            asset.name = name.to_string();
        }
    }

    /// Add a caption track carrying one cue.
    pub fn push_cue(&mut self, start: &str, end: &str, text: &str) {
        let span = Span::new(
            Time::parse(start).expect("start"),
            Time::parse(end).expect("end"),
        );
        let sequence = self.sequence_mut();
        if !sequence
            .tracks
            .iter()
            .any(|track| track.kind == TrackKind::Caption)
        {
            let mut track = Track::new("CC1", TrackKind::Caption);
            track.style = Some(StyleId::from_raw("sty_default"));
            sequence.tracks.push(track);
        }
        let track = sequence
            .tracks
            .iter_mut()
            .find(|track| track.kind == TrackKind::Caption)
            .expect("caption track");
        track.cues.push(CaptionCue {
            id: dvs_core::ids::CueId::new(),
            span,
            text: text.to_string(),
            style: None,
        });
    }
}

/// Frames actually present in a rendered file, counted rather than derived from the
/// container duration, which rounds.
pub fn frame_count(path: &Path) -> i64 {
    let output = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("ffprobe is installed");
    assert!(
        output.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .trim_end_matches(',')
        .parse()
        .unwrap_or_else(|_| panic!("ffprobe reported no frame count for {}", path.display()))
}

/// Whether a rendered file actually carries sound. An export that silently drops the
/// audio tracks still renders the right number of frames.
pub fn has_audio(path: &Path) -> bool {
    let output = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "a"])
        .args(["-show_entries", "stream=codec_type", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("ffprobe is installed");
    String::from_utf8_lossy(&output.stdout).contains("audio")
}
