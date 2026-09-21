//! MLT XML, which is also what a `.kdenlive` file is.
//!
//! This is the handover format. Kdenlive 26.08 exposes no scripting API — its CLI opens,
//! renders and exits — so the only way an agent's edit reaches a human editor is as a
//! document Kdenlive can open, and the only way to prove the document means what the
//! project means is to render it with `melt` and compare. That round trip
//! (`project.json` → XML → `melt` → mp4) is the cheapest end-to-end test this codebase
//! has, which is why the writer came first.
//!
//! Two details in here are worth more than the rest of the file put together:
//!
//! * **MLT `in`/`out` are inclusive frame indices.** A thirty-frame clip is `in="0"
//!   out="29"`. Writing `out="30"` does not produce an error — it produces a timeline one
//!   frame longer at every cut, and by the tenth cut the export and the native render
//!   disagree by a third of a second. [`tests::in_and_out_are_inclusive`] pins it.
//! * **Frame rates stay rational.** `frame_rate_num="30000" frame_rate_den="1001"`, never
//!   `29.97`. A document that says 29.97 drifts by a frame every 33 seconds against the
//!   source it was cut from.
//!
//! The reader deliberately implements a subset — profile, producers, playlists, entries,
//! blanks — and ignores everything else it meets. Kdenlive rewrites and normalizes a
//! document every time it saves, inventing properties and services this crate has never
//! heard of; a reader that validated would reject the very files it exists to read.

use crate::timeline::{self, Entry, Placed, Resource};
use crate::Export;
use dvs_core::asset::AssetStore;
use dvs_core::error::{Error, Result};
use dvs_core::ids::SequenceId;
use dvs_core::op::Warning;
use dvs_core::paths::ProjectPaths;
use dvs_core::project::{Clip, Project, TrackKind, Transition, TransitionKind};
use dvs_core::time::{Fps, Time};
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// The MLT release the written documents declare. Kdenlive 26.08 ships against MLT 7.40;
/// the attribute is informational, but a document that claims a version it does not match
/// is a lie a future reader will act on.
pub const MLT_VERSION: &str = "7.40.0";

/// Kdenlive's own document format version, distinct from the MLT version. 26.08 reads
/// 1.1 documents without running its upgrade path.
pub const KDENLIVE_DOC_VERSION: &str = "1.1";

/// The Kdenlive release these documents are written for.
pub const KDENLIVE_VERSION: &str = "26.08.0";

/// Alpha compositing transition, chosen once per process.
///
/// `frei0r.cairoblend` is the compositor Kdenlive uses and the only one that works on a
/// headless machine; `qtblend` needs the MLT Qt module, which refuses to initialize
/// without X11 or Wayland and would turn a CI render into a black frame. So the frei0r
/// plugin is looked for on disk and `qtblend` is the fallback for the machines that do
/// not have it.
static COMPOSITOR: LazyLock<&'static str> = LazyLock::new(|| {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("FREI0R_PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs.extend(
        [
            "/usr/lib/frei0r-1",
            "/usr/lib64/frei0r-1",
            "/usr/lib/x86_64-linux-gnu/frei0r-1",
            "/usr/local/lib/frei0r-1",
            "/opt/homebrew/lib/frei0r-1",
        ]
        .map(PathBuf::from),
    );
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(Path::new(&home).join(".frei0r-1/lib"));
    }
    let found = dirs.iter().any(|dir| {
        dir.join("cairoblend.so").exists() || dir.join("cairoblend.dylib").exists()
    });
    if found {
        "frei0r.cairoblend"
    } else {
        "qtblend"
    }
});

/// Write the sequence as MLT XML; `kdenlive` adds the `kdenlive:*` document properties
/// that turn the same XML into a project file Kdenlive 26.08 opens.
pub fn export(
    project: &Project,
    sequence: &SequenceId,
    paths: &ProjectPaths,
    assets: &AssetStore,
    kdenlive: bool,
) -> Result<Export> {
    let document = Document::build(project, sequence, assets, kdenlive)?;
    let text = document.write(project, paths, kdenlive)?;
    Ok(Export {
        text,
        warnings: document.warnings,
    })
}

// ---------------------------------------------------------------------------
// The intermediate document
// ---------------------------------------------------------------------------

/// A producer: one distinct piece of media, at one speed, used one way.
///
/// Producers are interned rather than emitted per clip because a bin clip used forty
/// times is one entry in Kdenlive's project bin, and because MLT opens the file once per
/// producer.
struct Producer {
    id: String,
    /// What this producer is a copy of: the content hash, or the color. One piece of
    /// media used at two speeds and once without picture is three producers but a
    /// single clip in the bin, and Kdenlive links them by their shared `kdenlive:id`.
    identity: String,
    /// Kdenlive's own small integer id, which its bin uses to link timeline instances
    /// back to bin clips.
    bin_id: usize,
    /// Whether this producer is the one the bin lists for its identity.
    bin: bool,
    service: String,
    resource: String,
    name: String,
    /// Length in frames at the sequence rate. MLT clamps entries to this, so it has to
    /// cover the largest `out` any entry asks for.
    length: i64,
    audio_only: bool,
    has_audio: bool,
    hash: Option<String>,
}

struct Filter {
    service: &'static str,
    props: Vec<(String, String)>,
    range: Option<(i64, i64)>,
}

struct EntryOut {
    producer: usize,
    in_frame: i64,
    out_frame: i64,
    filters: Vec<Filter>,
}

enum Item {
    Blank(i64),
    Entry(EntryOut),
}

struct PlaylistOut {
    id: String,
    items: Vec<Item>,
    filters: Vec<Filter>,
}

struct TransitionOut {
    id: String,
    service: String,
    a_track: usize,
    b_track: usize,
    props: Vec<(String, String)>,
    range: Option<(i64, i64)>,
}

struct TrackOut {
    /// The id the main tractor points at: a playlist, or a tractor wrapping two.
    reference: String,
    name: String,
    audio: bool,
    locked: bool,
    hide: Option<&'static str>,
    playlists: Vec<PlaylistOut>,
    /// Present when the track needed two playlists to express a transition, and always
    /// in Kdenlive mode, whose timeline model has no other shape.
    tractor: Option<String>,
    transitions: Vec<TransitionOut>,
    filters: Vec<Filter>,
}

struct Document {
    fps: Fps,
    size: [u32; 2],
    frames: i64,
    sequence_name: String,
    producers: Vec<Producer>,
    tracks: Vec<TrackOut>,
    warnings: Vec<Warning>,
    guides: String,
}

/// Running id allocation. MLT resolves references by id as it parses, so every id has to
/// be unique across the whole document, not just within its element type.
#[derive(Default)]
struct Ids {
    producer: usize,
    playlist: usize,
    tractor: usize,
    transition: usize,
}

impl Ids {
    fn producer(&mut self) -> String {
        let id = format!("producer{}", self.producer);
        self.producer += 1;
        id
    }
    fn playlist(&mut self) -> String {
        let id = format!("playlist{}", self.playlist);
        self.playlist += 1;
        id
    }
    fn tractor(&mut self) -> String {
        let id = format!("tractor{}", self.tractor);
        self.tractor += 1;
        id
    }
    fn transition(&mut self) -> String {
        let id = format!("transition{}", self.transition);
        self.transition += 1;
        id
    }
}

/// `2` rather than `2.000000`, `0.5` rather than `0.500000`: MLT parses the speed out of
/// the `timewarp` resource string, and a trailing-zero soup is unreadable in a file a
/// human is expected to open.
fn number(value: f64) -> String {
    let text = format!("{value:.6}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-" {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// The transition that actually needs expressing: an explicit cut is the absence of one.
fn transition_of(clip: &Clip) -> Option<&Transition> {
    clip.transition_in
        .as_ref()
        .filter(|t| t.kind != TransitionKind::Cut && t.duration.is_positive())
}

impl Document {
    fn build(
        project: &Project,
        sequence: &SequenceId,
        assets: &AssetStore,
        kdenlive: bool,
    ) -> Result<Document> {
        let sequence = project.sequence(sequence)?;
        let fps = sequence.fps;
        let flat = timeline::flatten(sequence);
        let mut warnings: Vec<Warning> = Vec::new();
        for track in &flat {
            warnings.extend(track.warnings.iter().cloned());
        }
        for track in sequence.tracks.iter().filter(|t| t.kind == TrackKind::Caption) {
            if !track.cues.is_empty() {
                warnings.push(Warning {
                    code: "captions-not-exported",
                    target: track.name.clone(),
                    detail: format!(
                        "{} caption cue(s) on '{}' are not part of an MLT timeline; export them \
                         with export.srt and attach the file in the editor",
                        track.cues.len(),
                        track.name
                    ),
                });
            }
        }
        let frames = flat.iter().map(|t| t.frames).max().unwrap_or(0).max(1);

        let mut ids = Ids::default();
        let mut producers: Vec<Producer> = Vec::new();
        let mut interned: BTreeMap<String, usize> = BTreeMap::new();
        let mut tracks: Vec<TrackOut> = Vec::new();

        for flat_track in &flat {
            let audio = flat_track.track.kind == TrackKind::Audio;
            let paired = kdenlive
                || flat_track
                    .clips()
                    .any(|placed| transition_of(placed.clip).is_some());
            let mut main: Vec<Item> = Vec::new();
            let mut side: Vec<Item> = Vec::new();
            let mut side_cursor = 0i64;
            let mut transitions: Vec<TransitionOut> = Vec::new();

            for entry in &flat_track.entries {
                let placed = match entry {
                    Entry::Blank { frames } => {
                        main.push(Item::Blank(*frames));
                        continue;
                    }
                    Entry::Clip(placed) => placed,
                };
                warnings.extend(timeline::feature_warnings(placed.clip, "MLT"));
                let (resource, warning) = timeline::resolve_source(project, assets, placed.clip)?;
                warnings.extend(warning);
                let speed = clip_speed(placed.clip);
                if placed.clip.reverse {
                    warnings.push(Warning {
                        code: "reverse-approximated",
                        target: placed.clip.label().to_string(),
                        detail: "a reversed clip is exported as a negative-speed timewarp \
                                 producer; MLT counts its frames from the other end, so check \
                                 the in-point in the editor"
                            .to_string(),
                    });
                }
                let producer = intern(
                    &mut producers,
                    &mut interned,
                    &mut ids,
                    resource,
                    audio,
                    speed,
                    fps,
                    frames,
                );

                // With a retimed producer the frames are the retimed ones, so the source
                // in-point moves with the speed: 2× speed halves the frame index.
                let in_frame = match speed {
                    Some(speed) if speed.abs() > f64::EPSILON => {
                        (placed.source_in as f64 / speed).round() as i64
                    }
                    _ => placed.source_in,
                };
                let in_frame = in_frame.max(0);

                let mut head = 0i64;
                if paired {
                    if let Some(transition) = transition_of(placed.clip) {
                        head = open_transition(
                            &mut main,
                            &mut side,
                            &mut side_cursor,
                            &mut transitions,
                            &producers,
                            &mut ids,
                            placed,
                            transition,
                            audio,
                            fps,
                            &mut warnings,
                        );
                    }
                }

                if head > 0 {
                    side.push(Item::Entry(EntryOut {
                        producer,
                        in_frame,
                        out_frame: in_frame + head - 1,
                        filters: clip_filters(placed.clip, audio, in_frame, head, fps, Part::Head),
                    }));
                    side_cursor += head;
                    if placed.frames > head {
                        main.push(Item::Entry(EntryOut {
                            producer,
                            in_frame: in_frame + head,
                            out_frame: in_frame + placed.frames - 1,
                            filters: clip_filters(
                                placed.clip,
                                audio,
                                in_frame + head,
                                placed.frames - head,
                                fps,
                                Part::Tail,
                            ),
                        }));
                    }
                } else {
                    main.push(Item::Entry(EntryOut {
                        producer,
                        in_frame,
                        out_frame: in_frame + placed.frames - 1,
                        filters: clip_filters(
                            placed.clip,
                            audio,
                            in_frame,
                            placed.frames,
                            fps,
                            Part::Whole,
                        ),
                    }));
                }
            }

            let mut playlists = vec![PlaylistOut {
                id: ids.playlist(),
                items: main,
                filters: Vec::new(),
            }];
            if paired {
                playlists.push(PlaylistOut {
                    id: ids.playlist(),
                    items: side,
                    filters: Vec::new(),
                });
            }

            let track = flat_track.track;
            let mut track_filters = Vec::new();
            if track.gain_db.abs() > f32::EPSILON {
                track_filters.push(Filter {
                    service: "volume",
                    props: vec![("level".into(), number(f64::from(track.gain_db)))],
                    range: None,
                });
            }
            if track.pan.abs() > f32::EPSILON {
                track_filters.push(panner(track.pan));
            }

            let tractor = paired.then(|| ids.tractor());
            if paired {
                // The two sub-playlists have to be summed, or a clip on the second one
                // would silence the first for its whole length.
                transitions.push(TransitionOut {
                    id: ids.transition(),
                    service: "mix".to_string(),
                    a_track: 0,
                    b_track: 1,
                    props: vec![
                        ("always_active".into(), "1".into()),
                        ("sum".into(), "1".into()),
                        ("accepts_blanks".into(), "1".into()),
                        ("internal_added".into(), "237".into()),
                    ],
                    range: None,
                });
            }
            let reference = tractor.clone().unwrap_or_else(|| playlists[0].id.clone());
            // An audio track must never contribute picture: `hide="video"` is also how
            // a reader that has no `kdenlive:audio_track` property tells the two apart.
            let hide = match (track.hidden || audio, track.muted) {
                (true, true) => Some("both"),
                (true, false) => Some("video"),
                (false, true) => Some("audio"),
                (false, false) => None,
            };
            tracks.push(TrackOut {
                reference,
                name: track.name.clone(),
                audio,
                locked: track.locked,
                hide,
                playlists,
                tractor,
                transitions,
                filters: track_filters,
            });
        }
        assign_bin_ids(&mut producers);

        let guides = serde_json::to_string(
            &sequence
                .markers
                .iter()
                .map(|marker| {
                    serde_json::json!({
                        "comment": marker.name,
                        "pos": marker.at.frame_round(fps),
                        "type": 0,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|_| "[]".to_string());

        Ok(Document {
            fps,
            size: sequence.size,
            frames,
            sequence_name: sequence.name.clone(),
            producers,
            tracks,
            warnings,
            guides,
        })
    }
}

/// Which part of a split clip a filter set belongs to. A fade-in applies to the head of
/// a clip and a fade-out to its tail; when a transition splits a clip across two
/// playlists, attaching both to both parts would fade the clip twice.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Whole,
    Head,
    Tail,
}

fn clip_speed(clip: &Clip) -> Option<f64> {
    let speed = clip.speed.as_f64() * if clip.reverse { -1.0 } else { 1.0 };
    ((speed - 1.0).abs() > 1e-9).then_some(speed)
}

fn panner(pan: f32) -> Filter {
    // MLT's panner runs 0 (hard left) to 1 (hard right); the document runs −1 to +1.
    let position = (f64::from(pan).clamp(-1.0, 1.0) + 1.0) / 2.0;
    Filter {
        service: "panner",
        props: vec![
            ("start".into(), number(position)),
            ("end".into(), number(position)),
        ],
        range: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn intern(
    producers: &mut Vec<Producer>,
    interned: &mut BTreeMap<String, usize>,
    ids: &mut Ids,
    resource: Resource<'_>,
    audio_only: bool,
    speed: Option<f64>,
    fps: Fps,
    timeline_frames: i64,
) -> usize {
    let speed_key = speed.map(number).unwrap_or_default();
    let (identity, producer) = match resource {
        Resource::File { asset, path } => {
            let path = path.display().to_string();
            let length = asset.probe.duration.frame_ceil(fps).max(1);
            let (service, resource, length) = match speed {
                Some(speed) => (
                    "timewarp".to_string(),
                    format!("{}:{path}", number(speed)),
                    ((length as f64 / speed.abs()).ceil() as i64).max(1),
                ),
                None => ("avformat-novalidate".to_string(), path.clone(), length),
            };
            (
                format!("file|{}", asset.hash),
                Producer {
                    id: String::new(),
                    identity: String::new(),
                    bin_id: 0,
                    bin: false,
                    service,
                    resource,
                    name: asset.name.clone(),
                    length,
                    audio_only,
                    has_audio: asset.probe.audio.is_some(),
                    hash: Some(asset.hash.clone()),
                },
            )
        }
        Resource::Color(color) => {
            let resource = Resource::mlt_color(color);
            (
                format!("color|{resource}"),
                Producer {
                    id: String::new(),
                    identity: String::new(),
                    bin_id: 0,
                    bin: false,
                    service: "color".to_string(),
                    resource: resource.clone(),
                    name: format!("color {resource}"),
                    length: timeline_frames,
                    audio_only,
                    has_audio: false,
                    hash: None,
                },
            )
        }
    };
    let key = format!("{identity}|{audio_only}|{speed_key}");
    if let Some(index) = interned.get(&key) {
        return *index;
    }
    let index = producers.len();
    let mut producer = producer;
    producer.id = ids.producer();
    producer.identity = identity;
    producers.push(producer);
    interned.insert(key, index);
    index
}

/// Decide which producers the project bin lists and what Kdenlive id they share.
///
/// Kdenlive's bin holds one clip per piece of media; the timeline may hold several
/// producers for it (an audio-only copy, a retimed copy) and links them back by
/// `kdenlive:id`. Listing all of them would show the same file three times in the bin.
/// The bin entry is the plainest producer of each identity, because a bin clip that is
/// silent or playing at double speed is not the clip the human imported.
fn assign_bin_ids(producers: &mut [Producer]) {
    let plain = |producer: &Producer| !producer.audio_only && producer.service != "timewarp";
    let mut ids: BTreeMap<String, usize> = BTreeMap::new();
    let mut chosen: BTreeMap<String, usize> = BTreeMap::new();
    for (index, producer) in producers.iter().enumerate() {
        // Kdenlive's bin ids start at 2: 0 and 1 are its own bookkeeping.
        let next = ids.len() + 2;
        ids.entry(producer.identity.clone()).or_insert(next);
        match chosen.get(&producer.identity) {
            Some(current) if plain(&producers[*current]) => {}
            _ => {
                chosen.insert(producer.identity.clone(), index);
            }
        }
    }
    for (index, producer) in producers.iter_mut().enumerate() {
        producer.bin_id = ids.get(&producer.identity).copied().unwrap_or(2);
        producer.bin = chosen.get(&producer.identity) == Some(&index);
    }
}

/// Express a transition by overlapping the two clips across a track's two playlists.
///
/// The document forbids overlapping clips — an overlap *is* the transition — but MLT has
/// no such concept: it dissolves between two tracks that both have picture at the same
/// frame. So the outgoing clip is extended over the transition (it has to have that much
/// source left, or the dissolve would be against black) and the incoming clip's head is
/// moved to the second playlist. Returns how many frames of the incoming clip ended up
/// there, which is zero when the overlap could not be created.
#[allow(clippy::too_many_arguments)]
fn open_transition(
    main: &mut [Item],
    side: &mut Vec<Item>,
    side_cursor: &mut i64,
    transitions: &mut Vec<TransitionOut>,
    producers: &[Producer],
    ids: &mut Ids,
    placed: &Placed<'_>,
    transition: &Transition,
    audio: bool,
    fps: Fps,
    warnings: &mut Vec<Warning>,
) -> i64 {
    let label = placed.clip.label().to_string();
    let wanted = transition.duration.frame_round(fps).max(1);
    let Some(Item::Entry(previous)) = main.last_mut() else {
        warnings.push(Warning {
            code: "transition-dropped",
            target: label,
            detail: "a transition needs a clip to come from; there is none before this one, so \
                     the export has a cut here"
                .to_string(),
        });
        return 0;
    };
    let available = producers[previous.producer].length - (previous.out_frame + 1);
    let head = wanted.min(placed.frames).min(available.max(0));
    if head <= 0 {
        warnings.push(Warning {
            code: "transition-dropped",
            target: label,
            detail: format!(
                "the outgoing clip has no source left past its out point, so the {} could not \
                 be built and the export has a cut here",
                kind_name(transition.kind)
            ),
        });
        return 0;
    }
    if head < wanted {
        warnings.push(Warning {
            code: "transition-shortened",
            target: label.clone(),
            detail: format!(
                "the {} was shortened from {wanted} to {head} frames: that is all the source the \
                 outgoing clip has left",
                kind_name(transition.kind)
            ),
        });
    }
    if transition.kind != TransitionKind::Dissolve {
        warnings.push(Warning {
            code: "transition-approximated",
            target: label,
            detail: format!(
                "MLT has no direct equivalent of a {}; it was exported as a dissolve of the same \
                 length",
                kind_name(transition.kind)
            ),
        });
    }
    previous.out_frame += head;
    if placed.start > *side_cursor {
        side.push(Item::Blank(placed.start - *side_cursor));
        *side_cursor = placed.start;
    }
    transitions.push(TransitionOut {
        id: ids.transition(),
        service: if audio { "mix" } else { "luma" }.to_string(),
        a_track: 0,
        b_track: 1,
        props: vec![("start".into(), "0".into()), ("end".into(), "1".into())],
        range: Some((placed.start, placed.start + head - 1)),
    });
    head
}

fn kind_name(kind: TransitionKind) -> &'static str {
    match kind {
        TransitionKind::Cut => "cut",
        TransitionKind::Dissolve => "dissolve",
        TransitionKind::Dip => "dip",
        TransitionKind::Wipe => "wipe",
        TransitionKind::Slide => "slide",
        TransitionKind::Push => "push",
    }
}

fn clip_filters(
    clip: &Clip,
    audio: bool,
    in_frame: i64,
    frames: i64,
    fps: Fps,
    part: Part,
) -> Vec<Filter> {
    let mut filters = Vec::new();
    if audio {
        if clip.gain_db.abs() > f32::EPSILON {
            filters.push(Filter {
                service: "volume",
                props: vec![("level".into(), number(f64::from(clip.gain_db)))],
                range: None,
            });
        }
        if clip.pan.abs() > f32::EPSILON {
            filters.push(panner(clip.pan));
        }
    } else if clip.opacity < 1.0 {
        filters.push(Filter {
            service: "brightness",
            props: vec![
                ("alpha".into(), number(f64::from(clip.opacity.max(0.0)))),
                ("opacity".into(), "1".into()),
            ],
            range: None,
        });
    }

    let fade_in = clip.fade_in.frame_round(fps).min(frames);
    let fade_out = clip.fade_out.frame_round(fps).min(frames);
    if fade_in > 0 && matches!(part, Part::Whole | Part::Head) {
        let range = Some((in_frame, in_frame + fade_in - 1));
        filters.push(if audio {
            Filter {
                service: "volume",
                props: vec![("gain".into(), "0".into()), ("end".into(), "1".into())],
                range,
            }
        } else {
            Filter {
                service: "brightness",
                props: vec![
                    ("alpha".into(), format!("0=0;{}=1", fade_in - 1)),
                    ("opacity".into(), "1".into()),
                ],
                range,
            }
        });
    }
    if fade_out > 0 && matches!(part, Part::Whole | Part::Tail) {
        let last = in_frame + frames - 1;
        let range = Some((last - fade_out + 1, last));
        filters.push(if audio {
            Filter {
                service: "volume",
                props: vec![("gain".into(), "1".into()), ("end".into(), "0".into())],
                range,
            }
        } else {
            Filter {
                service: "brightness",
                props: vec![
                    ("alpha".into(), format!("0=1;{}=0", fade_out - 1)),
                    ("opacity".into(), "1".into()),
                ],
                range,
            }
        });
    }
    filters
}

// ---------------------------------------------------------------------------
// Emission
// ---------------------------------------------------------------------------

/// Thin wrapper over `quick_xml::Writer` so the emitter reads like the document it
/// produces. Escaping goes through quick-xml: a clip named `Q&A` must not be able to
/// produce a file Kdenlive refuses to parse.
struct Xml {
    writer: Writer<Vec<u8>>,
}

impl Xml {
    fn new() -> Self {
        Xml {
            writer: Writer::new_with_indent(Vec::new(), b' ', 2),
        }
    }

    fn event(&mut self, event: Event<'_>) -> Result<()> {
        self.writer
            .write_event(event)
            .map_err(|e| Error::op(format!("writing MLT XML: {e}")))
    }

    fn open(&mut self, name: &str, attrs: &[(&str, &str)]) -> Result<()> {
        let mut start = BytesStart::new(name);
        for (key, value) in attrs {
            start.push_attribute((*key, *value));
        }
        self.event(Event::Start(start))
    }

    fn empty(&mut self, name: &str, attrs: &[(&str, &str)]) -> Result<()> {
        let mut start = BytesStart::new(name);
        for (key, value) in attrs {
            start.push_attribute((*key, *value));
        }
        self.event(Event::Empty(start))
    }

    fn close(&mut self, name: &str) -> Result<()> {
        self.event(Event::End(BytesEnd::new(name)))
    }

    fn property(&mut self, name: &str, value: &str) -> Result<()> {
        self.open("property", &[("name", name)])?;
        self.event(Event::Text(BytesText::new(value)))?;
        self.close("property")
    }

    fn finish(self) -> Result<String> {
        String::from_utf8(self.writer.into_inner())
            .map_err(|e| Error::op(format!("MLT XML is not valid UTF-8: {e}")))
    }
}

impl Document {
    fn write(&self, project: &Project, paths: &ProjectPaths, kdenlive: bool) -> Result<String> {
        let mut xml = Xml::new();
        xml.event(Event::Decl(BytesDecl::new("1.0", Some("utf-8"), None)))?;

        let main_tractor = format!("tractor{}", self.tracks.len());
        let root = paths.root().display().to_string();
        let frames = self.frames.to_string();
        let last = (self.frames - 1).max(0).to_string();
        let document_uuid = uuid_from(&format!("{}:{}", project.id, self.sequence_name));

        xml.open(
            "mlt",
            &[
                ("LC_NUMERIC", "C"),
                ("version", MLT_VERSION),
                ("title", &project.name),
                ("root", &root),
                ("producer", &main_tractor),
            ],
        )?;

        let (num, den) = (self.fps.ratio().numer().abs(), self.fps.ratio().denom().abs());
        let (dar_num, dar_den) = display_aspect(self.size);
        xml.empty(
            "profile",
            &[
                ("description", &format!("dvs {}x{}", self.size[0], self.size[1])),
                ("width", &self.size[0].to_string()),
                ("height", &self.size[1].to_string()),
                ("progressive", "1"),
                ("sample_aspect_num", "1"),
                ("sample_aspect_den", "1"),
                ("display_aspect_num", &dar_num.to_string()),
                ("display_aspect_den", &dar_den.to_string()),
                ("frame_rate_num", &num.to_string()),
                ("frame_rate_den", &den.to_string()),
                ("colorspace", "709"),
            ],
        )?;

        for producer in &self.producers {
            let out = (producer.length - 1).max(0).to_string();
            xml.open(
                "producer",
                &[
                    ("id", &producer.id),
                    ("in", "0"),
                    ("out", &out),
                ],
            )?;
            xml.property("length", &producer.length.to_string())?;
            xml.property("eof", "pause")?;
            xml.property("resource", &producer.resource)?;
            xml.property("mlt_service", &producer.service)?;
            if producer.service == "color" {
                xml.property("mlt_image_format", "rgba")?;
                xml.property("aspect_ratio", "1")?;
            } else {
                xml.property("seekable", "1")?;
                xml.property("mute_on_pause", "0")?;
            }
            if producer.audio_only {
                // An audio track must not contribute picture, or a music clip would
                // black out the video track under it.
                xml.property("set.test_image", "1")?;
            } else if !producer.has_audio {
                xml.property("set.test_audio", "1")?;
            }
            if kdenlive {
                xml.property("kdenlive:id", &producer.bin_id.to_string())?;
                xml.property("kdenlive:clipname", &producer.name)?;
                xml.property(
                    "kdenlive:duration",
                    &Time::from_frames(producer.length, self.fps).clock(),
                )?;
                xml.property("kdenlive:control_uuid", &uuid_from(&producer.id))?;
                if let Some(hash) = &producer.hash {
                    xml.property("kdenlive:file_hash", hash.trim_start_matches("blake3:"))?;
                }
            }
            xml.close("producer")?;
        }

        if kdenlive {
            xml.open("playlist", &[("id", "main_bin")])?;
            xml.property("kdenlive:docproperties.decimalPoint", ".")?;
            xml.property("kdenlive:docproperties.version", KDENLIVE_DOC_VERSION)?;
            xml.property("kdenlive:docproperties.kdenliveversion", KDENLIVE_VERSION)?;
            xml.property("kdenlive:docproperties.documentid", &project.id.to_string())?;
            xml.property("kdenlive:docproperties.uuid", &document_uuid)?;
            xml.property(
                "kdenlive:docproperties.profile",
                &format!("{}x{} {}", self.size[0], self.size[1], self.fps),
            )?;
            xml.property("kdenlive:docproperties.seekOffset", "30000")?;
            // One bin folder so the human who opens this does not get forty clips loose
            // at the root of their project bin.
            xml.property("kdenlive:folder.-1.1", &project.name)?;
            xml.property("xml_retain", "1")?;
            for producer in self.producers.iter().filter(|producer| producer.bin) {
                xml.empty(
                    "entry",
                    &[
                        ("producer", &producer.id),
                        ("in", "0"),
                        ("out", &(producer.length - 1).max(0).to_string()),
                    ],
                )?;
            }
            xml.close("playlist")?;
        }

        // The background track. Kdenlive requires it under the name `black_track`; MLT
        // needs it so every compositing transition has a lower track to blend onto.
        xml.open(
            "producer",
            &[("id", "black_track"), ("in", "0"), ("out", &last)],
        )?;
        xml.property("length", &frames)?;
        xml.property("eof", "continue")?;
        xml.property("resource", "black")?;
        xml.property("aspect_ratio", "1")?;
        xml.property("mlt_service", "color")?;
        xml.property("mlt_image_format", "rgba")?;
        xml.property("set.test_audio", "0")?;
        xml.close("producer")?;

        for track in &self.tracks {
            for playlist in &track.playlists {
                xml.open("playlist", &[("id", &playlist.id)])?;
                if kdenlive && track.audio {
                    xml.property("kdenlive:audio_track", "1")?;
                }
                for item in &playlist.items {
                    match item {
                        Item::Blank(frames) => {
                            xml.empty("blank", &[("length", &frames.to_string())])?
                        }
                        Item::Entry(entry) => {
                            let producer = &self.producers[entry.producer];
                            let attrs = [
                                ("producer", producer.id.as_str()),
                                ("in", &entry.in_frame.to_string()),
                                ("out", &entry.out_frame.to_string()),
                            ];
                            if entry.filters.is_empty() {
                                xml.empty("entry", &attrs)?;
                            } else {
                                xml.open("entry", &attrs)?;
                                for filter in &entry.filters {
                                    write_filter(&mut xml, filter)?;
                                }
                                xml.close("entry")?;
                            }
                        }
                    }
                }
                for filter in &playlist.filters {
                    write_filter(&mut xml, filter)?;
                }
                xml.close("playlist")?;
            }

            if let Some(tractor) = &track.tractor {
                xml.open(
                    "tractor",
                    &[("id", tractor), ("in", "0"), ("out", &last)],
                )?;
                if kdenlive {
                    xml.property("kdenlive:track_name", &track.name)?;
                    xml.property("kdenlive:trackheight", "70")?;
                    xml.property("kdenlive:timeline_active", "1")?;
                    xml.property("kdenlive:collapsed", "0")?;
                    if track.audio {
                        xml.property("kdenlive:audio_track", "1")?;
                    }
                    if track.locked {
                        xml.property("kdenlive:locked_track", "1")?;
                    }
                }
                for playlist in &track.playlists {
                    xml.empty("track", &[("producer", &playlist.id)])?;
                }
                for transition in &track.transitions {
                    write_transition(&mut xml, transition)?;
                }
                for filter in &track.filters {
                    write_filter(&mut xml, filter)?;
                }
                xml.close("tractor")?;
            }
        }

        xml.open(
            "tractor",
            &[
                ("id", &main_tractor),
                ("title", &self.sequence_name),
                ("global_feed", "1"),
                ("in", "0"),
                ("out", &last),
            ],
        )?;
        if kdenlive {
            xml.property("kdenlive:uuid", &document_uuid)?;
            xml.property("kdenlive:clipname", &self.sequence_name)?;
            xml.property("kdenlive:duration", &Time::from_frames(self.frames, self.fps).clock())?;
            xml.property(
                "kdenlive:maxduration",
                &self.frames.to_string(),
            )?;
            xml.property("kdenlive:sequenceproperties.documentuuid", &document_uuid)?;
            xml.property(
                "kdenlive:sequenceproperties.tracksCount",
                &self.tracks.len().to_string(),
            )?;
            xml.property(
                "kdenlive:sequenceproperties.activeTrack",
                &self.tracks.len().min(1).to_string(),
            )?;
            xml.property("kdenlive:sequenceproperties.guides", &self.guides)?;
            xml.property("kdenlive:sequenceproperties.groups", "[]")?;
            xml.property("kdenlive:sequenceproperties.position", "0")?;
            xml.property("kdenlive:sequenceproperties.zonein", "0")?;
            xml.property("kdenlive:sequenceproperties.zoneout", &frames)?;
        }
        xml.empty("track", &[("producer", "black_track")])?;
        for track in &self.tracks {
            match track.hide {
                Some(hide) => xml.empty(
                    "track",
                    &[("producer", &track.reference), ("hide", hide)],
                )?,
                None => xml.empty("track", &[("producer", &track.reference)])?,
            }
        }
        for (index, track) in self.tracks.iter().enumerate() {
            let slot = index + 1;
            let mut props = vec![
                ("always_active".to_string(), "1".to_string()),
                ("internal_added".to_string(), "237".to_string()),
            ];
            let service = if track.audio {
                props.push(("sum".to_string(), "1".to_string()));
                "mix".to_string()
            } else {
                props.push(("disable".to_string(), "0".to_string()));
                (*COMPOSITOR).to_string()
            };
            write_transition(
                &mut xml,
                &TransitionOut {
                    id: format!("transition_track{slot}"),
                    service,
                    a_track: 0,
                    b_track: slot,
                    props,
                    range: None,
                },
            )?;
        }
        xml.close("tractor")?;
        xml.close("mlt")?;
        let mut text = xml.finish()?;
        text.push('\n');
        Ok(text)
    }
}

fn write_filter(xml: &mut Xml, filter: &Filter) -> Result<()> {
    xml.open("filter", &[])?;
    xml.property("mlt_service", filter.service)?;
    for (key, value) in &filter.props {
        xml.property(key, value)?;
    }
    if let Some((start, end)) = filter.range {
        xml.property("in", &start.to_string())?;
        xml.property("out", &end.to_string())?;
    }
    xml.close("filter")
}

fn write_transition(xml: &mut Xml, transition: &TransitionOut) -> Result<()> {
    xml.open("transition", &[("id", &transition.id)])?;
    xml.property("a_track", &transition.a_track.to_string())?;
    xml.property("b_track", &transition.b_track.to_string())?;
    xml.property("mlt_service", &transition.service)?;
    for (key, value) in &transition.props {
        xml.property(key, value)?;
    }
    if let Some((start, end)) = transition.range {
        xml.property("in", &start.to_string())?;
        xml.property("out", &end.to_string())?;
    }
    xml.close("transition")
}

/// Reduced display aspect, which MLT wants as two integers.
fn display_aspect(size: [u32; 2]) -> (i64, i64) {
    let (width, height) = (i64::from(size[0]).max(1), i64::from(size[1]).max(1));
    dvs_core::time::reduce(width, height)
}

/// A brace-wrapped UUID derived from a project id, so re-exporting the same project twice
/// produces the same document rather than a diff of nothing but identifiers.
fn uuid_from(seed: &str) -> String {
    let tail = seed.rsplit('_').next().unwrap_or(seed);
    let bits = ulid::Ulid::from_string(tail)
        .map(u128::from)
        .unwrap_or_else(|_| fnv128(seed));
    let bytes = bits.to_be_bytes();
    let group = |range: std::ops::Range<usize>| -> String {
        bytes[range].iter().map(|b| format!("{b:02x}")).collect()
    };
    // RFC 4122 version 4 / variant 1 bits, so a reader that checks does not balk.
    let mut time_high = u16::from_be_bytes([bytes[6], bytes[7]]);
    time_high = (time_high & 0x0fff) | 0x4000;
    let mut clock = u16::from_be_bytes([bytes[8], bytes[9]]);
    clock = (clock & 0x3fff) | 0x8000;
    format!(
        "{{{}-{}-{time_high:04x}-{clock:04x}-{}}}",
        group(0..4),
        group(4..6),
        group(10..16)
    )
}

/// FNV-1a over two offsets, for ids that are not ULIDs (hand-written documents, tests).
fn fnv128(text: &str) -> u128 {
    let mix = |mut hash: u64, salt: u8| -> u64 {
        hash ^= u64::from(salt);
        for byte in text.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    };
    let high = mix(0xcbf2_9ce4_8422_2325, 0);
    let low = mix(0xcbf2_9ce4_8422_2325, 0xa5);
    (u128::from(high) << 64) | u128::from(low)
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// A timeline read back out of an MLT document.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedTimeline {
    pub fps: Fps,
    pub size: [u32; 2],
    pub tracks: Vec<ImportedTrack>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedTrack {
    pub name: String,
    pub audio: bool,
    pub clips: Vec<ImportedClip>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedClip {
    /// The producer's `resource`, with any `timewarp` speed prefix removed.
    pub resource: String,
    /// `kdenlive:clipname` when the document carries one.
    pub name: Option<String>,
    /// The producer's `mlt_service`, verbatim, known or not.
    pub service: String,
    pub speed: f64,
    pub start: Time,
    pub duration: Time,
    pub source_in: Time,
    pub start_frames: i64,
    pub frames: i64,
    pub source_in_frames: i64,
}

/// MLT's default profile, used when a document has no `<profile>`. `dv_pal` is what MLT
/// itself falls back to, so a document written without a profile means this whether it
/// says so or not.
const DEFAULT_PROFILE: ([i64; 2], [u32; 2]) = ([25, 1], [720, 576]);

/// Read the documented subset of an MLT document: profile, producers, playlists, entries
/// and blanks.
pub fn from_mlt(xml: &str) -> Result<ImportedTimeline> {
    let document = roxmltree::Document::parse(xml)
        .map_err(|e| Error::op(format!("not valid XML: {e}")))?;
    let root = document.root_element();
    if root.tag_name().name() != "mlt" {
        return Err(Error::op(format!(
            "expected an <mlt> document, found <{}>",
            root.tag_name().name()
        )));
    }

    let (fps, size) = match root.descendants().find(|n| n.has_tag_name("profile")) {
        Some(profile) => {
            let num = attr_i64(profile, "frame_rate_num").unwrap_or(DEFAULT_PROFILE.0[0]);
            let den = attr_i64(profile, "frame_rate_den").unwrap_or(DEFAULT_PROFILE.0[1]);
            let fps = Fps::new(num, den.max(1))?;
            let width = attr_i64(profile, "width").unwrap_or(i64::from(DEFAULT_PROFILE.1[0]));
            let height = attr_i64(profile, "height").unwrap_or(i64::from(DEFAULT_PROFILE.1[1]));
            (fps, [width.max(0) as u32, height.max(0) as u32])
        }
        None => (
            Fps::new(DEFAULT_PROFILE.0[0], DEFAULT_PROFILE.0[1])?,
            DEFAULT_PROFILE.1,
        ),
    };

    let mut by_id: BTreeMap<&str, roxmltree::Node> = BTreeMap::new();
    for node in root.descendants() {
        if let Some(id) = node.attribute("id") {
            by_id.entry(id).or_insert(node);
        }
    }

    let tractor = main_tractor(root, &by_id).ok_or_else(|| {
        Error::op("no <tractor> in the document: there is no timeline to import")
    })?;

    let mut tracks = Vec::new();
    for track in tractor.children().filter(|n| n.has_tag_name("track")) {
        let Some(reference) = track.attribute("producer") else {
            continue;
        };
        let Some(node) = by_id.get(reference).copied() else {
            return Err(Error::op(format!(
                "a <track> references '{reference}', which the document does not define"
            )));
        };
        // A bare producer as a track is the background; it carries no clips.
        let playlists: Vec<roxmltree::Node> = if node.has_tag_name("playlist") {
            vec![node]
        } else if node.has_tag_name("tractor") {
            node.children()
                .filter(|n| n.has_tag_name("track"))
                .filter_map(|n| n.attribute("producer"))
                .filter_map(|id| by_id.get(id).copied())
                .filter(|n| n.has_tag_name("playlist"))
                .collect()
        } else {
            continue;
        };
        if playlists.is_empty() {
            continue;
        }

        let hidden = track.attribute("hide").unwrap_or("");
        let audio = property(node, "kdenlive:audio_track").is_some_and(|v| v == "1")
            || playlists
                .iter()
                .any(|p| property(*p, "kdenlive:audio_track").is_some_and(|v| v == "1"))
            || hidden == "video";
        let name = property(node, "kdenlive:track_name")
            .map(str::to_string)
            .unwrap_or_else(|| {
                let prefix = if audio { "A" } else { "V" };
                format!(
                    "{prefix}{}",
                    tracks
                        .iter()
                        .filter(|t: &&ImportedTrack| t.audio == audio)
                        .count()
                        + 1
                )
            });

        let mut clips = Vec::new();
        for playlist in playlists {
            read_playlist(playlist, &by_id, fps, &mut clips)?;
        }
        clips.sort_by_key(|clip: &ImportedClip| clip.start_frames);
        tracks.push(ImportedTrack { name, audio, clips });
    }

    Ok(ImportedTimeline { fps, size, tracks })
}

/// The tractor the document says is the timeline: the one the root element points at,
/// else the last one, which is where every writer puts it.
fn main_tractor<'a>(
    root: roxmltree::Node<'a, 'a>,
    by_id: &BTreeMap<&str, roxmltree::Node<'a, 'a>>,
) -> Option<roxmltree::Node<'a, 'a>> {
    if let Some(named) = root
        .attribute("producer")
        .and_then(|id| by_id.get(id).copied())
    {
        if named.has_tag_name("tractor") {
            return Some(named);
        }
    }
    root.children()
        .filter(|n| n.has_tag_name("tractor"))
        .next_back()
}

fn read_playlist(
    playlist: roxmltree::Node,
    by_id: &BTreeMap<&str, roxmltree::Node>,
    fps: Fps,
    clips: &mut Vec<ImportedClip>,
) -> Result<()> {
    let mut cursor = 0i64;
    for child in playlist.children().filter(roxmltree::Node::is_element) {
        match child.tag_name().name() {
            "blank" => {
                cursor += child
                    .attribute("length")
                    .and_then(|text| frames_of(text, fps))
                    .unwrap_or(0);
            }
            "entry" => {
                let Some(reference) = child.attribute("producer") else {
                    continue;
                };
                let producer = by_id.get(reference).copied().ok_or_else(|| {
                    Error::op(format!(
                        "playlist entry references producer '{reference}', which the document \
                         does not define"
                    ))
                })?;
                let in_frame = child
                    .attribute("in")
                    .and_then(|text| frames_of(text, fps))
                    .unwrap_or(0);
                let out_frame = child
                    .attribute("out")
                    .and_then(|text| frames_of(text, fps))
                    .unwrap_or_else(|| {
                        property(producer, "length")
                            .and_then(|text| frames_of(text, fps))
                            .map(|length| length - 1)
                            .unwrap_or(in_frame)
                    });
                // Inclusive out: a one-frame entry is in == out.
                let frames = (out_frame - in_frame + 1).max(1);
                let service = property(producer, "mlt_service")
                    .unwrap_or("unknown")
                    .to_string();
                let raw = property(producer, "resource").unwrap_or("").to_string();
                let (speed, resource) = split_warp(&service, &raw);
                clips.push(ImportedClip {
                    resource,
                    name: property(producer, "kdenlive:clipname").map(str::to_string),
                    service,
                    speed,
                    start: Time::from_frames(cursor, fps),
                    duration: Time::from_frames(frames, fps),
                    source_in: Time::from_frames(in_frame, fps),
                    start_frames: cursor,
                    frames,
                    source_in_frames: in_frame,
                });
                cursor += frames;
            }
            _ => {}
        }
    }
    Ok(())
}

/// `timewarp` hides the speed in front of the path (`2:/clips/a.mp4`). Everything else
/// uses the resource as written.
fn split_warp(service: &str, resource: &str) -> (f64, String) {
    if service != "timewarp" {
        return (1.0, resource.to_string());
    }
    match resource.split_once(':') {
        Some((speed, path)) => match speed.parse::<f64>() {
            Ok(speed) => (speed, path.to_string()),
            Err(_) => (1.0, resource.to_string()),
        },
        None => (1.0, resource.to_string()),
    }
}

fn property<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    node.children()
        .filter(|n| n.has_tag_name("property"))
        .find(|n| n.attribute("name") == Some(name))
        .and_then(|n| n.text())
}

fn attr_i64(node: roxmltree::Node, name: &str) -> Option<i64> {
    node.attribute(name)?.trim().parse().ok()
}

/// MLT writes positions as either a frame count or a `HH:MM:SS.mmm` timecode, sometimes
/// both in one document — Kdenlive uses timecodes on producers and frames on entries.
fn frames_of(text: &str, fps: Fps) -> Option<i64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.contains(':') || text.contains('.') {
        return Time::parse(text).ok().map(|time| time.frame_round(fps));
    }
    text.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{fixture, Fixture};
    use dvs_core::project::Marker;

    fn export_text(fixture: &Fixture, kdenlive: bool) -> String {
        export(
            &fixture.project,
            &fixture.sequence,
            &fixture.paths,
            &fixture.assets,
            kdenlive,
        )
        .expect("export")
        .text
    }

    #[test]
    fn in_and_out_are_inclusive() {
        // One second at 30 fps from the head of the source: frames 0 through 29.
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        let xml = export_text(&fixture, false);
        assert!(
            xml.contains(r#"in="0" out="29""#),
            "a 30-frame clip must be in=0 out=29:\n{xml}"
        );
    }

    #[test]
    fn a_second_clip_starts_where_the_first_ends() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        fixture.push_clip("V1", "1", "1", "2");
        let xml = export_text(&fixture, false);
        let imported = from_mlt(&xml).expect("read back");
        let track = imported
            .tracks
            .iter()
            .find(|t| !t.audio)
            .expect("a video track");
        assert_eq!(track.clips.len(), 2);
        assert_eq!(track.clips[0].start_frames, 0);
        assert_eq!(track.clips[0].frames, 30);
        assert_eq!(
            track.clips[1].start_frames, 30,
            "the second entry must abut the first, with no blank between"
        );
        assert!(
            !xml.contains("<blank"),
            "adjacent clips must not produce a blank:\n{xml}"
        );
        assert_eq!(track.clips[1].source_in_frames, 60);
    }

    #[test]
    fn a_gap_becomes_a_blank_of_the_right_length() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        fixture.push_clip("V1", "1.5", "1", "0");
        let xml = export_text(&fixture, false);
        assert!(
            xml.contains(r#"<blank length="15"/>"#),
            "half a second at 30 fps is a 15-frame blank:\n{xml}"
        );
    }

    #[test]
    fn ndf_frame_rates_survive_as_rationals() {
        let mut fixture = fixture(Fps::new(30000, 1001).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        let xml = export_text(&fixture, true);
        assert!(
            xml.contains(r#"frame_rate_num="30000""#) && xml.contains(r#"frame_rate_den="1001""#),
            "29.97 must stay exact:\n{xml}"
        );
        assert!(!xml.contains("29.97"), "a decimal rate leaked into the profile");
    }

    #[test]
    fn round_trip_keeps_track_count_clip_count_and_lengths() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        fixture.push_clip("V1", "2", "0.5", "1");
        fixture.push_clip("A1", "0", "2.5", "0");
        let xml = export_text(&fixture, false);
        let imported = from_mlt(&xml).expect("read back");
        assert_eq!(imported.fps, Fps::new(30, 1).unwrap());
        assert_eq!(imported.size, [320, 240]);
        assert_eq!(imported.tracks.len(), 2);
        let video = &imported.tracks[0];
        assert_eq!(
            video.clips.iter().map(|c| c.frames).collect::<Vec<_>>(),
            vec![30, 15]
        );
        assert_eq!(video.clips[1].start_frames, 60);
        let audio = &imported.tracks[1];
        assert!(audio.audio, "the audio track must come back marked as audio");
        assert_eq!(audio.clips.len(), 1);
        assert_eq!(audio.clips[0].frames, 75);
    }

    #[test]
    fn reader_tolerates_unknown_properties_and_services() {
        let xml = r#"<?xml version="1.0"?>
<mlt LC_NUMERIC="C" version="7.40.0" producer="tractor0">
  <profile width="1920" height="1080" frame_rate_num="24" frame_rate_den="1"/>
  <producer id="p0">
    <property name="resource">/clips/weird.gif</property>
    <property name="mlt_service">qimage</property>
    <property name="pixbuf.rotate">90</property>
    <property name="invented.by.kdenlive">whatever</property>
  </producer>
  <playlist id="pl0">
    <property name="shotcut:name">something</property>
    <blank length="12"/>
    <entry producer="p0" in="0" out="9"/>
  </playlist>
  <tractor id="tractor0">
    <track producer="pl0"/>
    <transition id="t0"><property name="mlt_service">unknown_service</property></transition>
  </tractor>
</mlt>"#;
        let imported = from_mlt(xml).expect("an unknown service is not a parse failure");
        assert_eq!(imported.fps, Fps::new(24, 1).unwrap());
        assert_eq!(imported.tracks.len(), 1);
        let clip = &imported.tracks[0].clips[0];
        assert_eq!(clip.service, "qimage");
        assert_eq!(clip.resource, "/clips/weird.gif");
        assert_eq!(clip.start_frames, 12);
        assert_eq!(clip.frames, 10);
    }

    #[test]
    fn a_dangling_producer_reference_is_an_error() {
        let xml = r#"<mlt producer="tractor0">
  <profile frame_rate_num="25" frame_rate_den="1" width="720" height="576"/>
  <playlist id="pl0"><entry producer="missing" in="0" out="9"/></playlist>
  <tractor id="tractor0"><track producer="pl0"/></tractor>
</mlt>"#;
        let error = from_mlt(xml).expect_err("a reference to nothing cannot be imported");
        assert!(error.to_string().contains("missing"), "{error}");
    }

    #[test]
    fn kdenlive_mode_carries_the_document_keys_kdenlive_reads() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        fixture.push_clip("A1", "0", "1", "0");
        fixture.sequence_mut().markers.push(Marker {
            id: dvs_core::ids::MarkerId::from_raw("mk_1"),
            at: Time::from_secs(1),
            name: "pricing".to_string(),
            color: None,
            note: None,
        });
        let xml = export_text(&fixture, true);
        for key in [
            "kdenlive:docproperties.decimalPoint",
            "kdenlive:docproperties.version",
            "kdenlive:sequenceproperties.tracksCount",
            "kdenlive:sequenceproperties.activeTrack",
            "kdenlive:sequenceproperties.documentuuid",
            "kdenlive:sequenceproperties.guides",
            "kdenlive:sequenceproperties.groups",
            "kdenlive:clipname",
            "kdenlive:id",
            "kdenlive:duration",
            "kdenlive:control_uuid",
            "kdenlive:folder.-1.1",
        ] {
            assert!(xml.contains(key), "kdenlive mode is missing {key}:\n{xml}");
        }
        assert!(
            xml.contains(r#"<producer id="black_track""#),
            "kdenlive needs the black background track"
        );
        assert!(
            xml.contains(r#"<playlist id="main_bin">"#),
            "the project bin must be populated"
        );
        let document = roxmltree::Document::parse(&xml).expect("valid XML");
        let guides = document
            .descendants()
            .filter(|node| node.has_tag_name("property"))
            .find(|node| node.attribute("name") == Some("kdenlive:sequenceproperties.guides"))
            .and_then(|node| node.text())
            .expect("guides property");
        assert_eq!(
            guides, r#"[{"comment":"pricing","pos":30,"type":0}]"#,
            "markers should arrive as kdenlive guides"
        );
        // Kdenlive's timeline model is a tractor of two playlists per track.
        assert_eq!(xml.matches("<playlist id=\"playlist").count(), 4);
    }

    #[test]
    fn plain_mode_writes_one_playlist_per_track() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        fixture.push_clip("A1", "0", "1", "0");
        let xml = export_text(&fixture, false);
        assert_eq!(xml.matches("<playlist id=\"playlist").count(), 2);
        assert!(!xml.contains("kdenlive:"), "plain MLT must stay plain:\n{xml}");
    }

    #[test]
    fn a_dissolve_overlaps_the_two_clips_on_two_playlists() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        fixture.push_clip("V1", "1", "1", "1");
        fixture.set_transition("V1", 1, TransitionKind::Dissolve, "0.5");
        let xml = export_text(&fixture, false);
        assert!(
            xml.contains(r#"<property name="mlt_service">luma</property>"#),
            "a dissolve is a luma transition:\n{xml}"
        );
        // The outgoing clip is extended by fifteen frames so there is something to
        // dissolve from: its entry grows from in=0 out=29 to in=0 out=44. The incoming
        // clip starts at source frame 30, so its head is 30..44 on the second playlist
        // and its tail 45..59 on the first.
        assert!(xml.contains(r#"in="0" out="44""#), "extended outgoing clip:\n{xml}");
        assert!(xml.contains(r#"in="30" out="44""#), "head on playlist B:\n{xml}");
        assert!(xml.contains(r#"in="45" out="59""#), "tail on playlist A:\n{xml}");
        assert!(
            xml.contains(r#"<property name="in">30</property>"#),
            "the transition must cover the overlap:\n{xml}"
        );
    }

    #[test]
    fn a_transition_with_no_source_left_is_reported_not_faked() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        // The first clip consumes the whole four-second source, so there is nothing to
        // extend it with.
        fixture.push_clip("V1", "0", "4", "0");
        fixture.push_clip("V1", "4", "1", "0");
        fixture.set_transition("V1", 1, TransitionKind::Dissolve, "0.5");
        let export = export(
            &fixture.project,
            &fixture.sequence,
            &fixture.paths,
            &fixture.assets,
            false,
        )
        .expect("export");
        assert!(
            export
                .warnings
                .iter()
                .any(|w| w.code == "transition-dropped"),
            "an impossible transition must be reported: {:?}",
            export.warnings
        );
    }

    #[test]
    fn one_bin_clip_per_piece_of_media_however_many_producers_it_needs() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        // The same file on an audio track needs a second, picture-less producer.
        fixture.push_clip("A1", "0", "1", "0");
        let xml = export_text(&fixture, true);
        let document = roxmltree::Document::parse(&xml).expect("valid XML");
        let bin = document
            .descendants()
            .find(|node| node.attribute("id") == Some("main_bin"))
            .expect("a project bin");
        let entries: Vec<_> = bin
            .children()
            .filter(|node| node.has_tag_name("entry"))
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "one file must not appear in the bin twice:\n{xml}"
        );

        let producers: Vec<_> = document
            .descendants()
            .filter(|node| node.has_tag_name("producer"))
            .filter(|node| property(*node, "kdenlive:id").is_some())
            .collect();
        assert_eq!(producers.len(), 2, "two producers for the two uses");
        assert!(
            producers
                .iter()
                .all(|node| property(*node, "kdenlive:id") == Some("2")),
            "both producers must link back to the same bin clip"
        );
        let listed = entries[0].attribute("producer").expect("bin producer");
        let listed = producers
            .iter()
            .find(|node| node.attribute("id") == Some(listed))
            .expect("the listed producer");
        assert!(
            property(*listed, "set.test_image").is_none(),
            "the bin should show the clip with its picture, not the silent copy"
        );
    }

    #[test]
    fn fades_become_alpha_and_gain_ramps() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "2", "0");
        fixture.push_clip("A1", "0", "2", "0");
        fixture.set_fades("V1", 0, "0.5", "0");
        fixture.set_fades("A1", 0, "0", "0.5");
        let xml = export_text(&fixture, false);
        assert!(
            xml.contains(r#"<property name="alpha">0=0;14=1</property>"#),
            "a half-second video fade in is fifteen frames of alpha ramp:\n{xml}"
        );
        assert!(
            xml.contains(r#"<property name="mlt_service">brightness</property>"#),
            "{xml}"
        );
        assert!(
            xml.contains(r#"<property name="gain">1</property>"#)
                && xml.contains(r#"<property name="end">0</property>"#),
            "an audio fade out ramps the volume filter's gain:\n{xml}"
        );
        // The fade-out filter covers the last fifteen frames of the entry, not the clip
        // from its start.
        assert!(xml.contains(r#"<property name="in">45</property>"#), "{xml}");
        assert!(xml.contains(r#"<property name="out">59</property>"#), "{xml}");
    }

    #[test]
    fn unrepresentable_sources_keep_their_length_and_are_reported() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_title("V1", "0", "2");
        let export = export(
            &fixture.project,
            &fixture.sequence,
            &fixture.paths,
            &fixture.assets,
            true,
        )
        .expect("export");
        assert!(
            export
                .warnings
                .iter()
                .any(|w| w.code == "title-not-representable"),
            "{:?}",
            export.warnings
        );
        assert!(
            export.text.contains("#00000000"),
            "the placeholder must be transparent:\n{}",
            export.text
        );
        let imported = from_mlt(&export.text).expect("read back");
        assert_eq!(imported.tracks[0].clips[0].frames, 60);
    }

    #[test]
    fn audio_clips_do_not_black_out_the_video_under_them() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("A1", "0", "1", "0");
        let xml = export_text(&fixture, false);
        assert!(
            xml.contains(r#"<property name="set.test_image">1</property>"#),
            "an audio-track producer must not contribute picture:\n{xml}"
        );
    }

    #[test]
    fn speed_becomes_a_timewarp_producer() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        fixture.set_speed("V1", 0, 2, 1);
        let xml = export_text(&fixture, false);
        assert!(
            xml.contains(r#"<property name="mlt_service">timewarp</property>"#),
            "{xml}"
        );
        assert!(xml.contains("<property name=\"resource\">2:"), "{xml}");
    }

    #[test]
    fn xml_escaping_survives_a_clip_named_with_an_ampersand() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        fixture.name_clip("V1", 0, "Q&A <takes>");
        fixture.rename_asset("Q&A <takes>.mp4");
        let xml = export_text(&fixture, true);
        assert!(xml.contains("Q&amp;A &lt;takes&gt;"), "{xml}");
        roxmltree::Document::parse(&xml).expect("an escaped document still parses");
    }

    #[test]
    fn uuids_are_stable_across_exports() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        assert_eq!(export_text(&fixture, true), export_text(&fixture, true));
        assert!(uuid_from("prj_01J8ZQ5X7K9V2WD3N4T5M6P7Q8").starts_with('{'));
        assert_ne!(uuid_from("a"), uuid_from("b"));
    }

    /// Render an MLT file with `melt` and count the frames that came out.
    fn melt(file: &std::path::Path, rendered: &std::path::Path) -> i64 {
        let output = std::process::Command::new("melt")
            .arg("-quiet")
            .arg(file)
            .arg("-consumer")
            .arg(format!("avformat:{}", rendered.display()))
            .args(["real_time=0", "vcodec=libx264", "acodec=aac"])
            .output()
            .expect("melt is installed");
        assert!(
            output.status.success(),
            "melt failed on {}: {}",
            file.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        crate::tests_support::frame_count(rendered)
    }

    /// The P0 acceptance criterion: agent → JSON → XML → `melt` → mp4.
    #[test]
    fn melt_renders_the_exported_project_at_the_timeline_length() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        fixture.push_second("V1", "1.5", "1", "1");
        fixture.push_clip("A1", "0", "2.5", "0");
        let expected = fixture.sequence_ref().frame_count();
        assert_eq!(expected, 75, "two clips, a half-second hole, 30 fps");

        for (kdenlive, name) in [(true, "export.kdenlive"), (false, "export.mlt")] {
            let export = export(
                &fixture.project,
                &fixture.sequence,
                &fixture.paths,
                &fixture.assets,
                kdenlive,
            )
            .expect("export");
            let file = fixture.paths.root().join(name);
            std::fs::write(&file, export.text.as_bytes()).expect("write the project");
            let rendered = fixture.paths.root().join(format!("{name}.mp4"));
            let frames = melt(&file, &rendered);
            assert!(
                (frames - expected).abs() <= 1,
                "{name}: melt rendered {frames} frames, the timeline is {expected}"
            );
            assert!(
                crate::tests_support::has_audio(&rendered),
                "{name}: the audio track did not survive the export"
            );
        }
    }

    /// The dissolve path builds an overlap across two playlists and a `luma` transition,
    /// which is the construct most likely to produce XML that parses and then fails to
    /// render.
    #[test]
    fn melt_renders_a_dissolve() {
        let mut fixture = fixture(Fps::new(30, 1).unwrap());
        fixture.push_clip("V1", "0", "1", "0");
        fixture.push_second("V1", "1", "1", "0");
        fixture.set_transition("V1", 1, TransitionKind::Dissolve, "0.5");
        let export = export(
            &fixture.project,
            &fixture.sequence,
            &fixture.paths,
            &fixture.assets,
            true,
        )
        .expect("export");
        assert!(
            export.warnings.is_empty(),
            "a plain dissolve is representable: {:?}",
            export.warnings
        );
        let file = fixture.paths.root().join("dissolve.kdenlive");
        std::fs::write(&file, export.text.as_bytes()).expect("write the project");
        let frames = melt(&file, &fixture.paths.root().join("dissolve.mp4"));
        assert!(
            (frames - 60).abs() <= 1,
            "a dissolve must not change the timeline length; got {frames} frames"
        );
    }
}
