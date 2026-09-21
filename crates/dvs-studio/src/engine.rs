//! The worker half of the studio: reading the document, compositing pixels, applying ops.
//!
//! Everything in this module runs on one background thread. That is not a performance
//! detail, it is the correctness rule the window depends on:
//!
//! * **Nothing blocks the UI.** A composited frame costs an ffmpeg decode — tens of
//!   milliseconds on a good day, seconds on a cold seek. Doing that inside a command
//!   handler freezes the webview, so the UI asks and is told later.
//! * **One writer at a time.** A render and an op application cannot interleave on the
//!   same document, because they run on the same thread in arrival order. There is no lock
//!   to forget to take.
//!
//! The [`Handle`] is what the window holds: cheap to clone, `Send + Sync`, and owning no
//! part of the document. Every method either posts a request and awaits a
//! [`tokio::sync::oneshot`] reply, or answers from data the handle already has.
//!
//! Three things deserve their own paragraph.
//!
//! **The frame cache.** Dragging the playhead over a cut asks for the same handful of
//! frames again and again as the mouse jitters. Without a cache each pixel of mouse travel
//! respawns a decoder. The cache is eight entries keyed by `(index, revision)`: the
//! revision in the key is what makes an edit invalidate the picture without anybody having
//! to remember to flush it.
//!
//! **Provenance.** An op typed into the window's console goes through the same
//! [`Engine`] and the same registry as `dvs op` and the MCP server — journalled as
//! [`Actor::Human`] rather than written straight into the document. One journal, one undo
//! stack, and an agent reading `history.jsonl` can see what the person at the keyboard did.
//!
//! **Sentences, not fragments.** Every row, clip and gap leaves here with the sentence a
//! screen reader will read and `--describe` will print. Composing that text in Rust, from
//! the document, is the only way it can stay true to the timeline; a frontend concatenating
//! fragments would drift from the model the first time a field is added.

use crate::state::{
    Activity, Applied, ClipBox, ClipKind, Finding, GapBox, MarkerPin, Snapshot, StudioOptions,
    TimelineModel, TrackRow, TransitionBadge,
};
use crossbeam_channel::{Receiver, Sender};
use dvs_comp::{CompOptions, Compositor};
use dvs_core::engine::{Engine, Workspace};
use dvs_core::error::{Error, Result};
use dvs_core::ids::{ClipId, SequenceId};
use dvs_core::journal::{Actor, Entry, Journal};
use dvs_core::op::{Registry, Snap};
use dvs_core::paths::ProjectPaths;
use dvs_core::project::{Generator, Sequence, Source, Track, TrackKind};
use dvs_core::time::{Fps, Time};
use dvs_core::vfs::FsVfs;
use dvs_inspect::{LintOptions, Severity};
use dvs_media::Toolchain;
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use tokio::sync::oneshot;
use tokio_stream::Stream;

/// The op catalog, built once per process. Construction walks every capability crate's
/// registration; the console, the CLI and the MCP server must see an identical list, so
/// there is exactly one of these.
static REGISTRY: LazyLock<Registry> = LazyLock::new(dvs_mcp::full_registry);

/// How many composited frames to keep. Eight covers the jitter of a hand on a mouse and a
/// few frames of step-back; beyond that a preview frame is cheaper to recompute than to
/// hold, since each one is megabytes of RGBA.
const CACHE_FRAMES: usize = 8;

/// How much history the activity feed carries. A long session's journal is thousands of
/// entries and the panel shows a dozen.
const ACTIVITY_LIMIT: usize = 200;

/// Arguments worth putting in an activity line, most telling first. An op's whole argument
/// object is unreadable at a glance and mostly redundant with its id.
const INTERESTING: &[&str] = &[
    "target", "track", "at", "in", "out", "duration", "text", "source", "name", "to", "by",
    "kind", "value", "op", "seq",
];

/// Argument names that carry a timeline instant, and therefore snap to the frame grid.
const TIME_ARGS: &[&str] = &["at", "in", "out", "duration", "start", "end"];

/// A composited frame: width, height, and `width * height * 4` bytes of straight-alpha
/// RGBA8 over the sequence background.
pub type Rendered = (u32, u32, Vec<u8>);

/// Worker handle. Cheap to clone, `Send + Sync`, holds no document.
#[derive(Debug, Clone)]
pub struct Handle {
    inner: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    requests: Sender<Request>,
    paths: ProjectPaths,
    sequence: SequenceId,
    /// Frames actually composited, as opposed to served from the cache. Instrumentation:
    /// a number that climbs while the playhead sits still means the cache is not working.
    renders: Arc<AtomicU64>,
}

/// One unit of work for the worker, with the channel its answer goes back on.
enum Request {
    Frame {
        index: i64,
        reply: oneshot::Sender<Result<Rendered>>,
    },
    Reload {
        reply: oneshot::Sender<Result<Snapshot>>,
    },
    Apply {
        op: String,
        args: Value,
        reply: oneshot::Sender<Result<Applied>>,
    },
    Undo {
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    Redo {
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    Lint {
        reply: oneshot::Sender<Result<Vec<Finding>>>,
    },
}

impl Handle {
    /// Open the project and start the worker thread.
    ///
    /// The project directory is discovered upward from `options.root` the way `git` finds a
    /// repository, so starting the studio inside `myproject/assets` works.
    pub fn open(options: &StudioOptions) -> Result<(Handle, Snapshot)> {
        let workspace = Workspace::open_native(&options.root)?;
        let paths = workspace.paths.clone();
        let sequence = workspace
            .project
            .resolve_sequence(options.sequence.as_deref())?;
        // Entries already on disk are history; anything after this happened while the human
        // was watching, and is worth announcing.
        let baseline = workspace.journal.len() as u64;
        let render = CompOptions {
            scale: options.scale,
            use_proxy: options.use_proxy,
            // Scrubbing wants the cheap scaler. Nothing downstream inherits this: the
            // export path never comes through here.
            scaler: "bilinear",
        };
        let snapshot = snapshot(&workspace, &sequence, baseline, &render)?;

        let renders = Arc::new(AtomicU64::new(0));
        let worker = Worker {
            engine: Engine::new(REGISTRY.clone(), workspace).as_human(),
            sequence: sequence.clone(),
            render,
            baseline,
            renders: Arc::clone(&renders),
        };

        let (requests, incoming) = crossbeam_channel::unbounded();
        std::thread::Builder::new()
            .name("dvs-studio-engine".to_string())
            .spawn(move || serve(worker, incoming))
            .map_err(|error| Error::io(paths.root(), error))?;

        Ok((
            Handle {
                inner: Arc::new(Shared {
                    requests,
                    paths,
                    sequence,
                    renders,
                }),
            },
            snapshot,
        ))
    }

    /// The sequence this window shows. Fixed for the life of the handle: a window shows one
    /// sequence, and opening another is opening another window.
    pub fn sequence(&self) -> &SequenceId {
        &self.inner.sequence
    }

    pub fn paths(&self) -> &ProjectPaths {
        &self.inner.paths
    }

    /// Frames composited since the window opened. Cache hits do not count.
    pub fn renders(&self) -> u64 {
        self.inner.renders.load(Ordering::Relaxed)
    }

    /// Re-read `project.json` and `history.jsonl`.
    pub async fn reload(&self) -> Result<Snapshot> {
        self.ask(|reply| Request::Reload { reply }).await
    }

    /// Composite one frame at the studio's scale. Returns RGBA8 straight alpha over the
    /// sequence background.
    pub async fn frame(&self, index: i64) -> Result<Rendered> {
        self.ask(|reply| Request::Frame { index, reply }).await
    }

    /// Apply an op through the same registry the CLI and MCP use, journalled as `human`.
    pub async fn apply(&self, op: &str, args: Value) -> Result<Applied> {
        let op = op.to_string();
        self.ask(|reply| Request::Apply { op, args, reply }).await
    }

    pub async fn undo(&self) -> Result<Option<String>> {
        self.ask(|reply| Request::Undo { reply }).await
    }

    pub async fn redo(&self) -> Result<Option<String>> {
        self.ask(|reply| Request::Redo { reply }).await
    }

    /// Document-only lint. The pixel rules are deliberately off: they are a partial render,
    /// and a window that stalls for ten seconds when a panel opens is a broken window.
    pub async fn lint(&self) -> Result<Vec<Finding>> {
        self.ask(|reply| Request::Lint { reply }).await
    }

    /// Fires whenever `project.json` or `history.jsonl` changes on disk.
    pub fn watch(&self) -> impl Stream<Item = ()> + Send + 'static {
        crate::watch::watch(&self.inner.paths)
    }

    /// Parse a console line — `clip.split --target '#intro' --at 42.5` — into an op id and
    /// an argument object.
    ///
    /// The grammar and the string/number decision are `dvs op`'s, deliberately: a console
    /// that coerced arguments differently from the CLI would make the two surfaces disagree
    /// about the same typed line, which is the one thing a shared document cannot survive.
    /// See `crates/dvs-cli/src/commands/op.rs`.
    pub fn parse_command(&self, line: &str) -> Result<(String, Value)> {
        let tokens = tokenize(line)?;
        let Some((id, rest)) = tokens.split_first() else {
            return Err(Error::bad_args(
                "type an op and its arguments, e.g. clip.split --target #intro --at 42.5",
            ));
        };
        // An unknown id fails here, with the registry's own candidate list.
        let schema = REGISTRY.get(id)?.schema();
        let mut object = Map::new();
        for (key, values) in scan(rest)? {
            object.insert(key.clone(), coerce(&schema, &key, &values, id)?);
        }
        Ok((id.clone(), Value::Object(object)))
    }

    async fn ask<T>(&self, make: impl FnOnce(oneshot::Sender<Result<T>>) -> Request) -> Result<T> {
        let (reply, answer) = oneshot::channel();
        self.inner
            .requests
            .send(make(reply))
            .map_err(|_| stopped())?;
        answer.await.map_err(|_| stopped())?
    }
}

fn stopped() -> Error {
    Error::op("the studio engine thread stopped; reopen the project")
}

// ------------------------------------------------------------------------ the worker

/// Everything the worker owns. The [`Engine`] owns the [`Workspace`], so reads and writes
/// go through one object and an op applied here cannot miss the document a frame was
/// rendered from.
struct Worker {
    engine: Engine,
    sequence: SequenceId,
    render: CompOptions,
    baseline: u64,
    renders: Arc<AtomicU64>,
}

impl Worker {
    fn revision(&self) -> u64 {
        self.engine.workspace.journal.len() as u64
    }

    /// Re-open the project from disk. Re-reading rather than trusting the in-memory
    /// document is the point: the reason to reload is that somebody else wrote the file.
    fn reload(&mut self) -> Result<Snapshot> {
        let paths = self.engine.workspace.paths.clone();
        self.engine.workspace = Workspace::open(paths, FsVfs::shared())?;
        snapshot(
            &self.engine.workspace,
            &self.sequence,
            self.baseline,
            &self.render,
        )
    }

    fn apply(&mut self, op: &str, args: Value) -> Result<Applied> {
        let applied = self
            .engine
            .apply(op, args, Some(self.sequence.as_str().to_string()), false)?;
        // The engine wrote the document; re-reading ties this worker's revision — and
        // therefore its frame cache — to the bytes on disk rather than to its own memory,
        // which is what a concurrent agent edit would otherwise desynchronise.
        self.reload()?;
        Ok(report(applied))
    }

    fn undo(&mut self) -> Result<Option<String>> {
        let undone = self.engine.undo()?;
        self.reload()?;
        Ok(undone)
    }

    fn redo(&mut self) -> Result<Option<String>> {
        let redone = self.engine.redo()?;
        self.reload()?;
        Ok(redone)
    }

    fn lint(&self) -> Result<Vec<Finding>> {
        let workspace = &self.engine.workspace;
        let findings = dvs_inspect::lint(
            workspace,
            Toolchain::shared()?,
            &self.sequence,
            &LintOptions {
                render: false,
                scale: self.render.scale,
                use_proxy: self.render.use_proxy,
                ..LintOptions::default()
            },
        )?;
        Ok(findings
            .into_iter()
            .map(|finding| Finding {
                rule: finding.rule.to_string(),
                severity: severity_name(finding.severity).to_string(),
                // Resolving the selector here is what lets a click on a finding select the
                // clip: the selector grammar lives in dvs-core and the frontend must not
                // grow a second, wrong implementation of it.
                clip: named_clip(&workspace.project, &self.sequence, &finding.target),
                target: finding.target,
                detail: finding.detail,
            })
            .collect())
    }
}

/// The clip a finding's target names, when it names exactly one.
fn named_clip(
    project: &dvs_core::project::Project,
    sequence: &SequenceId,
    target: &str,
) -> Option<ClipId> {
    match dvs_core::selector::resolve_clips(project, sequence, target) {
        Ok(clips) if clips.len() == 1 => Some(clips[0].1.clone()),
        // A target naming a track, an asset or a time window is not a clip, and a target
        // naming several is not a thing a single click can select.
        _ => None,
    }
}

/// The worker loop.
///
/// The shape is unusual for a reason. A [`Compositor`] borrows the document and caches
/// decoders bound to the clips that were in it, so it cannot outlive an edit — and it must
/// not be rebuilt per frame either, or every frame respawns ffmpeg. So reads run in an
/// inner loop with a live compositor, and the first request that mutates the document ends
/// that loop, drops the compositor, and is handled with the document mutable again.
fn serve(mut worker: Worker, requests: Receiver<Request>) {
    let mut cache = FrameCache::default();
    let mut revision = worker.revision();
    loop {
        let mut comp: Option<Compositor<'_>> = None;
        let mutation = loop {
            let Ok(request) = requests.recv() else {
                return;
            };
            match request {
                Request::Frame { index, reply } => {
                    let answer = render(&worker, &mut comp, &mut cache, revision, index);
                    let _ = reply.send(answer);
                }
                Request::Lint { reply } => {
                    let _ = reply.send(worker.lint());
                }
                mutation => break mutation,
            }
        };
        // Everything below needs the document mutable, which the compositor's borrow
        // forbids; dropping it here is also what discards decoders bound to the old edit.
        drop(comp);
        match mutation {
            Request::Reload { reply } => {
                let _ = reply.send(worker.reload());
            }
            Request::Apply { op, args, reply } => {
                let _ = reply.send(worker.apply(&op, args));
            }
            Request::Undo { reply } => {
                let _ = reply.send(worker.undo());
            }
            Request::Redo { reply } => {
                let _ = reply.send(worker.redo());
            }
            Request::Frame { .. } | Request::Lint { .. } => {
                unreachable!("reads are answered in the inner loop")
            }
        }
        revision = worker.revision();
    }
}

/// Composite one frame, or hand back the one we already have.
fn render<'w>(
    worker: &'w Worker,
    comp: &mut Option<Compositor<'w>>,
    cache: &mut FrameCache,
    revision: u64,
    index: i64,
) -> Result<Rendered> {
    if let Some(hit) = cache.get(index, revision) {
        return Ok(hit);
    }
    if comp.is_none() {
        *comp = Some(Compositor::new(
            Toolchain::shared()?,
            &worker.engine.workspace.project,
            &worker.engine.workspace.paths,
            &worker.engine.workspace.assets,
            &worker.sequence,
            worker.render.clone(),
        )?);
    }
    let comp = comp.as_mut().expect("just built");
    worker.renders.fetch_add(1, Ordering::Relaxed);
    let frame = comp.frame(index)?;
    // Flatten onto the sequence background: the viewport is an opaque picture, and drawing
    // a transparency checkerboard is the window's job, not the compositor's.
    let rgb = frame.to_rgb8_over(comp.background()?);
    let rendered = (frame.width(), frame.height(), opaque_rgba(&rgb));
    cache.put(index, revision, rendered.clone());
    Ok(rendered)
}

/// RGB8 to RGBA8 with a fully opaque alpha channel, in one allocation and one pass.
fn opaque_rgba(rgb: &[u8]) -> Vec<u8> {
    let mut rgba = vec![255u8; rgb.len() / 3 * 4];
    for (out, pixel) in rgba.chunks_exact_mut(4).zip(rgb.chunks_exact(3)) {
        out[..3].copy_from_slice(pixel);
    }
    rgba
}

/// The last few composited frames, least recently used first.
///
/// Keyed by revision as well as index so an edit cannot serve a stale picture: the key of a
/// frame rendered before the edit can never be asked for again, and the entry ages out of
/// the eight slots on its own.
#[derive(Default)]
struct FrameCache {
    entries: Vec<(i64, u64, Rendered)>,
}

impl FrameCache {
    fn get(&mut self, index: i64, revision: u64) -> Option<Rendered> {
        let at = self
            .entries
            .iter()
            .position(|(i, r, _)| *i == index && *r == revision)?;
        // Touch it, so scrubbing back and forth keeps both ends of the travel resident.
        let entry = self.entries.remove(at);
        let rendered = entry.2.clone();
        self.entries.push(entry);
        Some(rendered)
    }

    fn put(&mut self, index: i64, revision: u64, rendered: Rendered) {
        if self.entries.len() == CACHE_FRAMES {
            self.entries.remove(0);
        }
        self.entries.push((index, revision, rendered));
    }
}

// ------------------------------------------------------------------ snapshot building

fn snapshot(
    workspace: &Workspace,
    sequence: &SequenceId,
    baseline: u64,
    render: &CompOptions,
) -> Result<Snapshot> {
    let seq = workspace.project.sequence(sequence)?;
    let journal = &workspace.journal;
    let touched = touched_clips(journal.entries().last(), sequence, seq);
    Ok(Snapshot {
        project_name: workspace.project.name.clone(),
        root: workspace.paths.root().display().to_string(),
        sequence: sequence.clone(),
        sequence_name: seq.name.clone(),
        size: seq.size,
        fps: seq.fps,
        duration: seq.duration(),
        frame_count: seq.frame_count(),
        timeline: timeline(seq, &touched),
        activity: activity(journal, seq.fps, baseline),
        revision: journal.len() as u64,
        scale: render.scale,
        use_proxy: render.use_proxy,
    })
}

/// Flatten a sequence into what the timeline draws and what a reader hears.
///
/// Rows run top-down in reverse document order: the compositor stacks `tracks` bottom
/// first, so the last track is the topmost layer, and an editor draws the topmost layer at
/// the top. Painting them in document order would put V2 under V1 on screen and over it in
/// the picture, which is the kind of disagreement that costs an afternoon.
fn timeline(seq: &Sequence, touched: &HashSet<ClipId>) -> TimelineModel {
    let mut rows = Vec::with_capacity(seq.tracks.len());
    let mut clips = Vec::new();
    let mut gaps = Vec::new();
    for (row, track) in seq.tracks.iter().rev().enumerate() {
        let count = track.clips.len() + track.cues.len();
        rows.push(TrackRow {
            id: track.id.clone(),
            name: track.name.clone(),
            kind: track.kind,
            muted: track.muted,
            locked: track.locked,
            hidden: track.hidden,
            clip_count: count,
            announce: announce_track(track, count),
        });
        for clip in &track.clips {
            let kind = clip_kind(&clip.source, track.kind);
            let transition = clip.transition_in.as_ref().map(|t| TransitionBadge {
                kind: format!("{:?}", t.kind).to_lowercase(),
                duration: t.duration,
            });
            clips.push(ClipBox {
                id: clip.id.clone(),
                track: track.id.clone(),
                row,
                start: clip.start,
                end: clip.end(),
                label: clip.label().to_string(),
                announce: announce_clip(
                    clip.label(),
                    kind,
                    &track.name,
                    clip.start,
                    clip.end(),
                    transition.as_ref(),
                    clip.enabled,
                ),
                source: clip.source.describe(),
                kind,
                enabled: clip.enabled,
                transition,
                touched: touched.contains(&clip.id),
            });
        }
        // Caption tracks carry cues rather than clips, and a caption track drawn as an
        // empty row is a caption track nobody notices is wrong. The box carries the cue's
        // own id — it identifies the thing under the pointer, not a clip that could be
        // trimmed.
        for cue in &track.cues {
            let label = cue.text.lines().next().unwrap_or_default().to_string();
            clips.push(ClipBox {
                id: ClipId::from_raw(cue.id.as_str()),
                track: track.id.clone(),
                row,
                start: cue.span.start,
                end: cue.span.end,
                announce: announce_clip(
                    &label,
                    ClipKind::Caption,
                    &track.name,
                    cue.span.start,
                    cue.span.end,
                    None,
                    true,
                ),
                label,
                source: cue.text.clone(),
                kind: ClipKind::Caption,
                enabled: true,
                transition: None,
                touched: false,
            });
        }
        for hole in seq.uncovered_gaps(track) {
            gaps.push(GapBox {
                row,
                track: track.id.clone(),
                start: hole.start,
                end: hole.end,
                announce: format!(
                    "gap on {}, {} to {} seconds, {}",
                    track.name,
                    secs(hole.start),
                    secs(hole.end),
                    match track.kind {
                        TrackKind::Video => "black picture",
                        TrackKind::Audio => "silence",
                        TrackKind::Caption => "no captions",
                    }
                ),
            });
        }
    }
    TimelineModel {
        rows,
        clips,
        gaps,
        markers: seq
            .markers
            .iter()
            .map(|marker| MarkerPin {
                at: marker.at,
                name: marker.name.clone(),
            })
            .collect(),
        duration: seq.duration(),
        fps: seq.fps,
    }
}

/// "intro, video clip on V1, 0 to 4.004 seconds, dissolve in over 0.5 seconds".
fn announce_clip(
    label: &str,
    kind: ClipKind,
    track: &str,
    start: Time,
    end: Time,
    transition: Option<&TransitionBadge>,
    enabled: bool,
) -> String {
    let mut text = format!(
        "{label}, {} clip on {track}, {} to {} seconds",
        kind.word(),
        secs(start),
        secs(end)
    );
    if let Some(transition) = transition {
        text.push_str(&format!(
            ", {} in over {} seconds",
            transition.kind,
            secs(transition.duration)
        ));
    }
    if !enabled {
        text.push_str(", disabled");
    }
    text
}

/// "V1, video track, 3 clips, muted".
fn announce_track(track: &Track, count: usize) -> String {
    let mut text = format!(
        "{}, {} track, {count} {}",
        track.name,
        track_word(track.kind),
        if count == 1 { "clip" } else { "clips" }
    );
    for (flag, word) in [
        (track.muted, "muted"),
        (track.locked, "locked"),
        (track.hidden, "hidden"),
    ] {
        if flag {
            text.push_str(", ");
            text.push_str(word);
        }
    }
    text
}

fn track_word(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Video => "video",
        TrackKind::Audio => "audio",
        TrackKind::Caption => "caption",
    }
}

/// Seconds as a person says them: `4.004`, `1.5`, `0`. Exact rational time is the
/// document's business; a sentence read aloud is not the place for `120120/29997`.
fn secs(time: Time) -> String {
    let text = format!("{:.3}", time.as_secs_f64());
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-" {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Colour family for a clip. The track decides for the two sources that can be either: a
/// file on an audio track is sound, and a 1 kHz tone has no picture wherever it sits.
fn clip_kind(source: &Source, track: TrackKind) -> ClipKind {
    match source {
        Source::Asset { .. } if track == TrackKind::Audio => ClipKind::Audio,
        Source::Asset { .. } => ClipKind::Video,
        Source::Title { .. } => ClipKind::Title,
        Source::Sequence { .. } => ClipKind::Nested,
        Source::Color { .. } => ClipKind::Color,
        Source::Image { .. } => ClipKind::Image,
        Source::Generator {
            generator: Generator::Tone,
            ..
        } => ClipKind::Audio,
        Source::Generator { .. } => ClipKind::Generator,
    }
}

/// Clips the newest journal entry wrote to, so the window can highlight what just changed.
///
/// The journal records RFC-6902 patches, not op effects, and that is the right source
/// anyway: an entry written by an agent in another process arrives as a line of JSON and
/// nothing else. Every patch path into this sequence names a track and a clip index, and
/// those indices are the ones in the document now on disk.
fn touched_clips(entry: Option<&Entry>, sequence: &SequenceId, seq: &Sequence) -> HashSet<ClipId> {
    let mut out = HashSet::new();
    let Some(entry) = entry else {
        return out;
    };
    let Ok(Value::Array(operations)) = serde_json::to_value(&entry.patch) else {
        return out;
    };
    let prefix = format!("/sequences/{}/tracks/", sequence.as_str());
    for operation in &operations {
        let Some(path) = operation.get("path").and_then(Value::as_str) else {
            continue;
        };
        if let Some(id) = clip_at_path(path, &prefix, seq) {
            out.insert(id);
        }
    }
    out
}

/// `/sequences/<seq>/tracks/2/clips/3/duration` → the id of clip 3 on track 2.
fn clip_at_path(path: &str, prefix: &str, seq: &Sequence) -> Option<ClipId> {
    let mut parts = path.strip_prefix(prefix)?.split('/');
    let track = seq.tracks.get(parts.next()?.parse::<usize>().ok()?)?;
    if parts.next()? != "clips" {
        return None;
    }
    let index = match parts.next()? {
        "-" => track.clips.len().checked_sub(1)?,
        number => number.parse().ok()?,
    };
    track.clips.get(index).map(|clip| clip.id.clone())
}

fn activity(journal: &Journal, fps: Fps, baseline: u64) -> Vec<Activity> {
    journal
        .tail(ACTIVITY_LIMIT)
        .iter()
        .map(|entry| {
            let actor = actor_name(entry.actor);
            Activity {
                seq: entry.seq,
                actor: actor.to_string(),
                op: entry.op.clone(),
                summary: summarize(&entry.args, fps),
                at: clock(entry.ts),
                announce: announce_entry(actor, &entry.op, &entry.args, fps),
                fresh: entry.seq > baseline,
            }
        })
        .collect()
}

fn actor_name(actor: Actor) -> &'static str {
    match actor {
        Actor::Agent => "agent",
        Actor::Human => "human",
        Actor::Ai => "ai",
    }
}

fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Info => "info",
        Severity::Warning => "warning",
        Severity::Error => "error",
    }
}

/// The journal's UTC instant on the human's wall clock.
///
/// A feed whose clock disagrees with the terminal beside it by an hour cannot answer the
/// only question it exists to answer — "did that just happen?" — so the local offset is
/// applied here rather than showing the stored UTC.
fn clock(ts: chrono::DateTime<chrono::Utc>) -> String {
    ts.with_timezone(&chrono::Local)
        .format("%H:%M:%S")
        .to_string()
}

/// "agent ran clip.split on #intro at frame 1274" — the sentence for the live region.
fn announce_entry(actor: &str, op: &str, args: &Value, fps: Fps) -> String {
    let mut text = format!("{actor} ran {op}");
    if let Some(undone) = args.get("op").and_then(Value::as_str) {
        // `project.undo` and `project.redo` are about another entry; naming it is the whole
        // content of the line.
        text.push_str(&format!(" of {undone}"));
    }
    if let Some(target) = args
        .get("target")
        .or_else(|| args.get("track"))
        .and_then(Value::as_str)
    {
        text.push_str(&format!(" on {target}"));
    }
    for key in TIME_ARGS {
        if let Ok(Some(at)) = dvs_core::op::args::opt_time(args, key, fps) {
            text.push_str(&format!(" at frame {}", at.frame_round(fps)));
            break;
        }
    }
    text
}

/// One line of the arguments that matter, in the spelling they were typed in.
///
/// A time argument also names the frame it landed on when the grid moved it: an agent asks
/// for 42.5 s on a 30000/1001 timeline and gets frame 1274, and a feed that hides that is
/// how a timeline drifts without anyone noticing.
fn summarize(args: &Value, fps: Fps) -> String {
    let Some(object) = args.as_object() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for key in INTERESTING {
        let Some(value) = object.get(*key) else {
            continue;
        };
        parts.push(argument(key, value, args, fps));
        if parts.len() == 3 {
            break;
        }
    }
    if parts.is_empty() {
        parts = object
            .iter()
            .take(2)
            .map(|(key, value)| argument(key, value, args, fps))
            .collect();
    }
    parts.join(" ")
}

fn argument(key: &str, value: &Value, args: &Value, fps: Fps) -> String {
    if value == &Value::Bool(true) {
        return format!("--{key}");
    }
    let text = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    if TIME_ARGS.contains(&key) {
        if let Ok(Some(at)) = dvs_core::op::args::opt_time(args, key, fps) {
            let frame = at.frame_round(fps);
            if Time::from_frames(frame, fps) != at {
                return format!("--{key} {text} (frame {frame})");
            }
        }
    }
    format!("--{key} {text}")
}

/// Turn the engine's record of an op into what the window shows: a status line plus the two
/// things easiest to miss — what the op warned about, and where a time actually landed.
fn report(applied: dvs_core::engine::Applied) -> Applied {
    let effect = applied.effect;
    let mut summary = match applied.seq {
        Some(seq) => format!("{} #{seq}", applied.op),
        None => applied.op.clone(),
    };
    for (label, ids) in [
        ("created", &effect.created),
        ("changed", &effect.changed),
        ("removed", &effect.removed),
    ] {
        if !ids.is_empty() {
            summary.push_str(&format!(" {label} {}", ids.join(", ")));
        }
    }
    Applied {
        op: applied.op,
        summary,
        changed: effect.changed,
        created: effect.created,
        removed: effect.removed,
        warnings: effect
            .warnings
            .iter()
            .map(|warning| format!("{}: {}", warning.target, warning.detail))
            .collect(),
        snapped: effect.snapped.iter().map(snap_line).collect(),
    }
}

/// "at 42.5 s snapped to frame 1274". The stored value is exact rational time; a person
/// reading a status bar wants seconds.
fn snap_line(snap: &Snap) -> String {
    let requested = Time::parse(&snap.requested)
        .map(secs)
        .unwrap_or_else(|_| snap.requested.clone());
    format!("{} {requested} s snapped to frame {}", snap.field, snap.frame)
}

// ------------------------------------------------------------------ the console grammar

/// Split a console line into tokens, honouring single and double quotes so that
/// `--name 'my clip'` is one argument. There is no shell here to do it first, and no
/// escape character: a backslash in a clip name is part of the name, not syntax.
fn tokenize(line: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    for character in line.chars() {
        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => current.push(character),
            None if character == '\'' || character == '"' => {
                quote = Some(character);
                started = true;
            }
            None if character.is_whitespace() => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None => {
                current.push(character);
                started = true;
            }
        }
    }
    if let Some(open) = quote {
        return Err(Error::bad_args(format!("unbalanced {open} in the command")));
    }
    if started {
        tokens.push(current);
    }
    Ok(tokens)
}

/// Group the tokens after the op id into `(key, values)` pairs. Values stay strings: what
/// they mean is the schema's business.
fn scan(rest: &[String]) -> Result<Vec<(String, Vec<String>)>> {
    let mut flags: Vec<(String, Vec<String>)> = Vec::new();
    let mut index = 0;
    while index < rest.len() {
        let token = &rest[index];
        index += 1;
        let Some(body) = token.strip_prefix("--") else {
            return Err(Error::bad_args(format!(
                "unexpected value '{token}'; op arguments are given as --key value"
            )));
        };
        let (key, inline) = match body.split_once('=') {
            Some((key, value)) => (key.to_string(), Some(value.to_string())),
            None => (body.to_string(), None),
        };
        if key.is_empty() {
            return Err(Error::bad_args("'--' is not an argument name"));
        }
        let value = match inline {
            Some(value) => Some(value),
            // `--gain -6` is a value; `--gain --track A1` is a flag with no value, which
            // means `true`.
            None => match rest.get(index) {
                Some(next) if !next.starts_with("--") => {
                    index += 1;
                    Some(next.clone())
                }
                _ => None,
            },
        };
        let value = value.unwrap_or_else(|| "true".to_string());
        match flags.iter_mut().find(|(name, _)| *name == key) {
            Some((_, values)) => values.push(value),
            None => flags.push((key, vec![value])),
        }
    }
    Ok(flags)
}

/// What an argument's schema says it is. A union of types (`["string", "number"]`, which
/// every time field uses) stays a string: ops parse `"42.5"` and `42.5` identically, and
/// keeping the text avoids turning `00:01:12` into nonsense.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bool,
    Number,
    Array,
    Text,
}

fn coerce(schema: &Value, key: &str, values: &[String], op: &str) -> Result<Value> {
    let property = schema
        .get("properties")
        .and_then(|properties| properties.get(key))
        .ok_or_else(|| unknown_flag(schema, key, op))?;
    let kind = kind_of(property);
    if values.len() > 1 && kind != Kind::Array {
        return Err(Error::bad_args(format!(
            "'--{key}' was given {} times but takes one value",
            values.len()
        )));
    }
    let raw = &values[0];
    match kind {
        // One occurrence stays a string so the op's own comma splitting applies; repeating
        // the flag is the explicit list form.
        Kind::Array if values.len() == 1 => Ok(Value::String(raw.clone())),
        Kind::Array => Ok(Value::Array(
            values.iter().cloned().map(Value::String).collect(),
        )),
        Kind::Bool => match raw.as_str() {
            "true" | "yes" | "1" => Ok(Value::Bool(true)),
            "false" | "no" | "0" => Ok(Value::Bool(false)),
            other => Err(Error::bad_args(format!(
                "'--{key}' is a flag; write '--{key}' or '--{key}=false', not '{other}'"
            ))),
        },
        Kind::Number => {
            let number: f64 = raw
                .trim()
                .parse()
                .map_err(|_| Error::bad_args(format!("'--{key}' must be a number, got '{raw}'")))?;
            // Integral values stay integers: an op that reads a count should not have to
            // defend against `6.0`, and the journal reads better without it.
            if number.fract() == 0.0 && number.abs() < 9.0e15 {
                Ok(Value::from(number as i64))
            } else {
                Ok(Value::from(number))
            }
        }
        Kind::Text => Ok(Value::String(raw.clone())),
    }
}

fn kind_of(property: &Value) -> Kind {
    let mut types: Vec<&str> = Vec::new();
    collect_types(property, &mut types);
    if types.iter().any(|kind| *kind == "array") || property.get("items").is_some() {
        return Kind::Array;
    }
    if types.iter().any(|kind| *kind == "string") {
        return Kind::Text;
    }
    if types
        .iter()
        .any(|kind| *kind == "number" || *kind == "integer")
    {
        return Kind::Number;
    }
    if types.iter().any(|kind| *kind == "boolean") {
        return Kind::Bool;
    }
    Kind::Text
}

/// Gather the type names a property allows, following the `oneOf`/`anyOf` unions the
/// catalog uses for "a path or a list of paths".
fn collect_types<'a>(property: &'a Value, into: &mut Vec<&'a str>) {
    match property.get("type") {
        Some(Value::String(name)) => into.push(name),
        Some(Value::Array(names)) => into.extend(names.iter().filter_map(Value::as_str)),
        _ => {}
    }
    for branch in ["oneOf", "anyOf", "allOf"] {
        if let Some(Value::Array(items)) = property.get(branch) {
            for item in items {
                collect_types(item, into);
            }
        }
    }
}

/// The error nobody should have to ask a follow-up question about: it names the arguments
/// this op really takes.
fn unknown_flag(schema: &Value, key: &str, op: &str) -> Error {
    let mut names: Vec<&str> = schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().map(String::as_str).collect())
        .unwrap_or_default();
    names.sort_unstable();
    Error::bad_args(format!(
        "'{op}' has no argument '{key}'; it accepts: {}",
        if names.is_empty() {
            "(none)".to_string()
        } else {
            names.join(", ")
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::color::Rgba;
    use dvs_core::ids::{AssetId, TitleId};
    use dvs_core::project::{Clip, Project};
    use std::path::PathBuf;

    fn fps() -> Fps {
        Fps::new(30, 1).expect("30 fps")
    }

    struct Fixture {
        dir: tempfile::TempDir,
        paths: ProjectPaths,
    }

    impl Fixture {
        /// A real project directory, empty apart from the default sequence.
        fn new() -> Fixture {
            let dir = tempfile::tempdir().expect("temp dir");
            let paths = ProjectPaths::new(dir.path());
            Workspace::create(
                paths.clone(),
                Project::new("studio", fps(), [160, 120], 48_000),
                FsVfs::shared(),
            )
            .expect("create the project");
            Fixture { dir, paths }
        }

        /// Run an op the way an agent in a terminal would: a fresh engine, `Actor::Agent`.
        fn run(&self, op: &str, args: Value) -> dvs_core::engine::Applied {
            let workspace = Workspace::open(self.paths.clone(), FsVfs::shared()).expect("open");
            let mut engine = Engine::new(REGISTRY.clone(), workspace);
            engine.apply(op, args, None, false).expect("apply an op")
        }

        /// Animated real media, so one instant differs from another.
        fn media(&self, name: &str, seconds: i64) -> PathBuf {
            let path = self.dir.path().join(name);
            dvs_media::decode::synthesize(
                Toolchain::shared().expect("ffmpeg"),
                &path,
                "testsrc2",
                Time::from_secs(seconds),
                fps(),
                [160, 120],
            )
            .expect("synthesize media");
            path
        }

        /// One video track carrying `seconds` of animated media, named `intro`.
        fn with_clip(&self, seconds: i64) -> &Fixture {
            let media = self.media("talk.mp4", seconds);
            self.run("track.add", serde_json::json!({ "kind": "video" }));
            self.run(
                "asset.import",
                serde_json::json!({ "path": media.to_string_lossy() }),
            );
            self.run(
                "clip.append",
                serde_json::json!({
                    "track": "V1",
                    "source": "talk",
                    "duration": seconds,
                    "name": "intro"
                }),
            );
            self
        }

        fn options(&self) -> StudioOptions {
            StudioOptions {
                root: self.paths.root().to_path_buf(),
                scale: 0.5,
                ..StudioOptions::default()
            }
        }

        fn open(&self) -> (Handle, Snapshot) {
            Handle::open(&self.options()).expect("open the studio")
        }
    }

    /// Build a project document by hand, for the shapes no op can produce in one line.
    /// The fixture owns the temporary directory, so it lives exactly as long as the test.
    fn documented(name: &str, build: impl FnOnce(&mut Project)) -> Fixture {
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = ProjectPaths::new(dir.path());
        let mut project = Project::new(name, fps(), [64, 36], 48_000);
        build(&mut project);
        Workspace::create(paths.clone(), project, FsVfs::shared()).expect("create");
        Fixture { dir, paths }
    }

    #[test]
    fn opening_a_directory_without_a_project_says_how_to_make_one() {
        let empty = tempfile::tempdir().unwrap();
        let error = Handle::open(&StudioOptions {
            root: empty.path().to_path_buf(),
            ..StudioOptions::default()
        })
        .expect_err("there is no project here");
        assert!(
            error.to_string().contains("dvs new"),
            "the error must say how to make a project: {error}"
        );
    }

    #[test]
    fn opening_below_the_project_discovers_the_root() {
        let fixture = Fixture::new();
        let (handle, snapshot) = Handle::open(&StudioOptions {
            root: fixture.paths.assets_dir(),
            ..StudioOptions::default()
        })
        .expect("discover the project above");
        assert_eq!(snapshot.project_name, "studio");
        assert_eq!(handle.paths().root(), fixture.paths.root());
    }

    #[tokio::test]
    async fn a_frame_is_rgba_at_the_configured_scale_and_moves_with_time() {
        let fixture = Fixture::new();
        fixture.with_clip(2);
        let (handle, snapshot) = fixture.open();
        assert_eq!(snapshot.size, [160, 120]);
        assert_eq!(snapshot.scale, 0.5);

        let (width, height, pixels) = handle.frame(0).await.expect("composite frame 0");
        // Half of a 160x120 sequence.
        assert_eq!([width, height], [80, 60]);
        assert_eq!(pixels.len(), width as usize * height as usize * 4);
        assert!(
            pixels.chunks_exact(4).all(|pixel| pixel[3] == 255),
            "the viewport is opaque"
        );

        let (_, _, later) = handle.frame(45).await.expect("composite a later frame");
        assert_ne!(
            pixels, later,
            "testsrc2 animates: two instants cannot be identical"
        );
    }

    #[tokio::test]
    async fn a_repeated_frame_is_served_from_the_cache_until_the_document_changes() {
        let fixture = Fixture::new();
        fixture.with_clip(2);
        let (handle, _) = fixture.open();

        handle.frame(12).await.expect("first render");
        assert_eq!(handle.renders(), 1);

        // Scrub away and back: the round trip must not re-enter ffmpeg for frame 12.
        handle.frame(24).await.expect("second frame");
        let again = handle.frame(12).await.expect("cached frame");
        assert_eq!(handle.renders(), 2, "frame 12 was composited twice");
        assert_eq!(again.0, 80);

        // An edit invalidates the key, because those pixels may no longer be the truth.
        handle
            .apply("marker.add", serde_json::json!({ "at": 1, "name": "beat" }))
            .await
            .expect("apply an op");
        handle.frame(12).await.expect("render after the edit");
        assert_eq!(
            handle.renders(),
            3,
            "a revision bump must invalidate the cached frame"
        );
    }

    #[tokio::test]
    async fn reload_picks_up_a_split_and_flags_the_clips_the_entry_touched() {
        let fixture = Fixture::new();
        fixture.with_clip(2);
        let (handle, first) = fixture.open();
        assert_eq!(first.timeline.clips.len(), 1);

        // The agent's edit, through the same path the CLI takes.
        fixture.run(
            "clip.split",
            serde_json::json!({ "target": "#intro", "at": 1 }),
        );
        let after = handle.reload().await.expect("reload");

        assert_eq!(after.timeline.clips.len(), 2, "the cut produced a clip");
        assert!(after.revision > first.revision);
        let touched: HashSet<&ClipId> = after
            .timeline
            .clips
            .iter()
            .filter(|clip| clip.touched)
            .map(|clip| &clip.id)
            .collect();
        let all: HashSet<&ClipId> = after.timeline.clips.iter().map(|clip| &clip.id).collect();
        assert_eq!(touched, all, "a split touches both halves");
    }

    #[test]
    fn a_clip_no_op_went_near_is_not_flagged() {
        let fixture = Fixture::new();
        fixture.with_clip(2);
        // A second track whose clip no later op touches.
        fixture.run("track.add", serde_json::json!({ "kind": "video" }));
        fixture.run(
            "clip.append",
            serde_json::json!({ "track": "V2", "source": "color:#204080", "duration": 1, "name": "card" }),
        );
        fixture.run(
            "clip.split",
            serde_json::json!({ "target": "#intro", "at": 1 }),
        );

        let (_, snapshot) = fixture.open();
        let card = snapshot
            .timeline
            .clips
            .iter()
            .find(|clip| clip.label == "card")
            .expect("the untouched clip is in the model");
        assert!(!card.touched, "the split did not touch the other track");
    }

    #[test]
    fn rows_run_top_down_and_every_source_kind_is_named() {
        let title = TitleId::new();
        let asset = AssetId::new();
        let nested = Sequence::new("inner", fps(), [64, 36], 48_000);
        let nested_id = nested.id.clone();
        let (title_for_build, asset_for_build) = (title.clone(), asset.clone());

        let fixture = documented("kinds", move |project| {
            project.sequences.insert(nested_id.clone(), nested);
            project.titles.insert(
                title_for_build.clone(),
                dvs_core::project::Title {
                    id: title_for_build.clone(),
                    name: "card".into(),
                    svg: "<svg xmlns='http://www.w3.org/2000/svg'/>".into(),
                    fields: Default::default(),
                    size: [64, 36],
                },
            );
            let sources = [
                Source::Asset {
                    asset: asset_for_build.clone(),
                    stream: None,
                },
                Source::Image {
                    asset: asset_for_build.clone(),
                },
                Source::Color { color: Rgba::WHITE },
                Source::Title {
                    title: title_for_build,
                },
                Source::Sequence {
                    sequence: nested_id,
                },
                Source::Generator {
                    generator: Generator::Bars,
                    params: Default::default(),
                },
            ];
            let mut video = Track::new("V1", TrackKind::Video);
            for (index, source) in sources.into_iter().enumerate() {
                video.clips.push(Clip::new(
                    source,
                    Time::from_secs(index as i64),
                    Time::from_secs(1),
                ));
            }
            let mut audio = Track::new("A1", TrackKind::Audio);
            audio.muted = true;
            audio.clips.push(Clip::new(
                Source::Asset {
                    asset: asset_for_build,
                    stream: None,
                },
                Time::ZERO,
                Time::from_secs(1),
            ));
            audio.clips.push(Clip::new(
                Source::Generator {
                    generator: Generator::Tone,
                    params: Default::default(),
                },
                Time::from_secs(1),
                Time::from_secs(1),
            ));
            let active = project.active_sequence.clone();
            let seq = project.sequence_mut(&active).expect("the default sequence");
            seq.tracks.push(video);
            seq.tracks.push(audio);
        });

        let (_, snapshot) = fixture.open();

        let names: Vec<&str> = snapshot
            .timeline
            .rows
            .iter()
            .map(|row| row.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["A1", "V1"],
            "rows are drawn top-down in reverse document order"
        );
        assert_eq!(
            snapshot.timeline.rows[0].announce,
            "A1, audio track, 2 clips, muted"
        );

        let kinds: Vec<ClipKind> = snapshot
            .timeline
            .clips
            .iter()
            .map(|clip| clip.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                ClipKind::Audio,
                ClipKind::Audio,
                ClipKind::Video,
                ClipKind::Image,
                ClipKind::Color,
                ClipKind::Title,
                ClipKind::Nested,
                ClipKind::Generator,
            ],
            "every Source variant maps to a family, and the track decides the ambiguous two"
        );
        assert!(snapshot
            .timeline
            .clips
            .iter()
            .all(|clip| clip.row < snapshot.timeline.rows.len()));
    }

    #[test]
    fn a_clip_announces_itself_as_a_sentence() {
        let fixture = documented("announce", |project| {
            let mut track = Track::new("V1", TrackKind::Video);
            let mut first = Clip::new(
                Source::Color { color: Rgba::WHITE },
                Time::ZERO,
                Time::from_secs(2),
            );
            first.name = Some("intro".into());
            let mut second = Clip::new(
                Source::Color { color: Rgba::BLACK },
                Time::from_secs(2),
                Time::from_secs(2),
            );
            second.name = Some("body".into());
            second.enabled = false;
            second.transition_in = Some(dvs_core::project::Transition {
                kind: dvs_core::project::TransitionKind::Dissolve,
                duration: Time::new(1, 2).unwrap(),
                easing: Default::default(),
                direction: Default::default(),
                color: None,
            });
            track.clips.push(first);
            track.clips.push(second);
            let active = project.active_sequence.clone();
            project
                .sequence_mut(&active)
                .expect("sequence")
                .tracks
                .push(track);
        });

        let (_, snapshot) = fixture.open();
        let announces: Vec<&str> = snapshot
            .timeline
            .clips
            .iter()
            .map(|clip| clip.announce.as_str())
            .collect();
        assert_eq!(
            announces,
            [
                "intro, color clip on V1, 0 to 2 seconds",
                "body, color clip on V1, 2 to 4 seconds, dissolve in over 0.5 seconds, disabled"
            ]
        );
    }

    #[test]
    fn a_hole_is_reported_against_the_row_it_is_drawn_on() {
        let fixture = documented("gaps", |project| {
            let mut track = Track::new("V1", TrackKind::Video);
            track.clips.push(Clip::new(
                Source::Color { color: Rgba::WHITE },
                Time::ZERO,
                Time::from_secs(1),
            ));
            track.clips.push(Clip::new(
                Source::Color { color: Rgba::BLACK },
                Time::from_secs(2),
                Time::from_secs(1),
            ));
            let active = project.active_sequence.clone();
            project
                .sequence_mut(&active)
                .expect("sequence")
                .tracks
                .push(track);
        });

        let (_, snapshot) = fixture.open();
        assert_eq!(snapshot.timeline.gaps.len(), 1);
        let gap = &snapshot.timeline.gaps[0];
        assert_eq!(gap.row, 0);
        assert_eq!(gap.start, Time::from_secs(1));
        assert_eq!(gap.end, Time::from_secs(2));
        assert_eq!(gap.announce, "gap on V1, 1 to 2 seconds, black picture");
    }

    #[test]
    fn a_console_line_parses_into_the_arguments_the_cli_would_build() {
        let fixture = Fixture::new();
        let (handle, _) = fixture.open();

        let (op, args) = handle
            .parse_command("clip.split --target '#intro' --at 42.5")
            .expect("a valid console line");
        assert_eq!(op, "clip.split");
        // Time fields are a string/number union, so the text survives verbatim — exactly
        // what `dvs op clip.split --target '#intro' --at 42.5` sends.
        assert_eq!(args, serde_json::json!({ "target": "#intro", "at": "42.5" }));
    }

    #[test]
    fn console_flags_follow_the_schema() {
        let fixture = Fixture::new();
        let (handle, _) = fixture.open();

        // An integer field becomes a number, not a string.
        let (_, args) = handle
            .parse_command("track.add --kind audio --index 2")
            .expect("valid");
        assert_eq!(args, serde_json::json!({ "kind": "audio", "index": 2 }));

        // A bare flag is true.
        let (_, args) = handle
            .parse_command("clip.enable --target #intro --enabled")
            .expect("valid");
        assert_eq!(args["enabled"], Value::Bool(true));

        // A repeated flag on a list-valued argument becomes an array.
        let (_, args) = handle
            .parse_command("asset.import --path one.mp4 --path two.mp4")
            .expect("valid");
        assert_eq!(args["path"], serde_json::json!(["one.mp4", "two.mp4"]));

        // Quoted values keep their spaces.
        let (_, args) = handle
            .parse_command("clip.rename --target #intro --name \"cold open\"")
            .expect("valid");
        assert_eq!(args["name"], Value::String("cold open".into()));
    }

    #[test]
    fn an_unknown_flag_names_the_ops_real_arguments() {
        let fixture = Fixture::new();
        let (handle, _) = fixture.open();
        let error = handle
            .parse_command("clip.split --target #intro --when 42.5")
            .expect_err("'when' is not an argument of clip.split");
        let message = error.to_string();
        assert!(message.contains("no argument 'when'"), "{message}");
        assert!(message.contains("target"), "{message}");
        assert!(message.contains("at"), "{message}");
    }

    #[test]
    fn an_unknown_op_comes_back_with_candidates() {
        let fixture = Fixture::new();
        let (handle, _) = fixture.open();
        let error = handle
            .parse_command("clip.splitt --target #intro --at 1")
            .expect_err("no such op");
        let report = error.to_report();
        assert_eq!(report.kind, "no-match");
        // The registry suggests the family the typo belongs to, which is what makes the
        // failure recoverable without a docs round trip.
        assert!(
            !report.candidates.is_empty()
                && report
                    .candidates
                    .iter()
                    .all(|name| name.starts_with("clip.")),
            "an unknown op must come back with real candidates: {:?}",
            report.candidates
        );
    }

    #[test]
    fn a_line_with_no_op_is_rejected_rather_than_guessed() {
        let fixture = Fixture::new();
        let (handle, _) = fixture.open();
        assert!(handle.parse_command("   ").is_err());
        assert!(
            handle.parse_command("clip.split --target '#intro").is_err(),
            "an unbalanced quote is a typo, not a clip named '#intro"
        );
    }

    #[tokio::test]
    async fn an_op_from_the_window_is_journalled_as_human_and_shows_in_the_feed() {
        let fixture = Fixture::new();
        fixture.with_clip(2);
        let (handle, opened) = fixture.open();

        let applied = handle
            .apply(
                "clip.split",
                serde_json::json!({ "target": "#intro", "at": 1 }),
            )
            .await
            .expect("split from the window");
        assert_eq!(applied.op, "clip.split");
        assert_eq!(applied.changed.len(), 1);
        assert_eq!(applied.created.len(), 1);
        assert!(applied.summary.starts_with("clip.split #"), "{applied:?}");

        let after = handle.reload().await.expect("reload");
        let newest = after.activity.last().expect("the feed has the new entry");
        assert_eq!(newest.op, "clip.split");
        assert_eq!(
            newest.actor, "human",
            "a click is a human edit even though it runs the agent's op"
        );
        assert!(newest.fresh, "it happened while the window was open");
        assert!(newest.seq > opened.revision);
        assert_eq!(newest.summary, "--target #intro --at 1");
        assert_eq!(newest.announce, "human ran clip.split on #intro at frame 30");
        assert_eq!(newest.at.len(), "12:34:56".len());
    }

    #[tokio::test]
    async fn an_off_grid_time_reports_the_frame_it_landed_on() {
        let fixture = Fixture::new();
        fixture.with_clip(2);
        let (handle, _) = fixture.open();
        // 1.01 s is between frames at 30 fps; the op snaps it and has to say so.
        let applied = handle
            .apply(
                "clip.split",
                serde_json::json!({ "target": "#intro", "at": 1.01 }),
            )
            .await
            .expect("split");
        assert_eq!(applied.snapped, ["at 1.01 s snapped to frame 30"]);
    }

    #[tokio::test]
    async fn undo_from_the_window_reverses_an_agents_edit() {
        let fixture = Fixture::new();
        fixture.with_clip(2);
        // The agent cuts...
        fixture.run(
            "clip.split",
            serde_json::json!({ "target": "#intro", "at": 1 }),
        );
        let (handle, _) = fixture.open();
        assert_eq!(handle.reload().await.unwrap().timeline.clips.len(), 2);

        // ...and the human at the keyboard takes it back.
        assert_eq!(handle.undo().await.unwrap().as_deref(), Some("clip.split"));
        let after = handle.reload().await.unwrap();
        assert_eq!(after.timeline.clips.len(), 1);
        let newest = after.activity.last().expect("undo is journalled too");
        assert_eq!(newest.actor, "human");
        assert_eq!(newest.announce, "human ran project.undo of clip.split");

        assert_eq!(handle.redo().await.unwrap().as_deref(), Some("clip.split"));
        assert_eq!(handle.reload().await.unwrap().timeline.clips.len(), 2);
    }

    #[tokio::test]
    async fn lint_reports_document_rules_without_rendering_and_resolves_the_clip() {
        let fixture = Fixture::new();
        fixture.with_clip(1);
        // Hand-edit the clip to read past the end of its one second of media, the way a
        // botched retime would. `past-source-end` is a document rule and names the clip.
        let mut workspace = Workspace::open(fixture.paths.clone(), FsVfs::shared()).unwrap();
        let sequence = workspace.project.active_sequence.clone();
        let clip_id = {
            let seq = workspace.project.sequence_mut(&sequence).unwrap();
            let clip = &mut seq.tracks[0].clips[0];
            clip.duration = Time::from_secs(3);
            clip.id.clone()
        };
        workspace.save().unwrap();

        let (handle, _) = fixture.open();
        let before = handle.renders();
        let findings = handle.lint().await.expect("lint");

        let finding = findings
            .iter()
            .find(|finding| finding.rule == "past-source-end")
            .unwrap_or_else(|| panic!("expected past-source-end, got {findings:?}"));
        assert_eq!(finding.severity, "error");
        assert_eq!(
            finding.clip.as_ref(),
            Some(&clip_id),
            "a finding that names a clip must resolve to it, so a click can select it"
        );
        assert_eq!(
            handle.renders(),
            before,
            "a document-only lint must not composite anything"
        );
    }

    #[test]
    fn a_snapped_time_names_the_frame_in_the_feed() {
        // 42.5 s on a 30000/1001 timeline is not on the grid; the feed has to say so.
        let fps = Fps::new(30_000, 1001).unwrap();
        let summary = summarize(&serde_json::json!({ "target": "#intro", "at": "42.5" }), fps);
        assert_eq!(summary, "--target #intro --at 42.5 (frame 1274)");

        // On the grid, the frame number is noise.
        assert_eq!(
            summarize(&serde_json::json!({ "at": "1" }), Fps::new(30, 1).unwrap()),
            "--at 1"
        );
    }

    #[test]
    fn opaque_rgba_widens_every_pixel() {
        assert_eq!(
            opaque_rgba(&[1, 2, 3, 4, 5, 6]),
            vec![1, 2, 3, 255, 4, 5, 6, 255]
        );
    }

    #[test]
    fn the_handle_can_live_in_tauri_state() {
        // Tauri hands a `&State<Handle>` to command handlers on several threads, and the
        // watcher subscription clones one into a task. A field that is not `Sync` — an
        // `Rc`, a `RefCell` — would fail here rather than at the far end of the wiring.
        fn managed<T: Send + Sync + Clone + 'static>() {}
        managed::<Handle>();
    }
}
