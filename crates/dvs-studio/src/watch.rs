//! Noticing that somebody else edited the project.
//!
//! The window is not the only writer. An agent in a terminal runs `dvs op clip.split …`,
//! `dvs-core` writes `project.json` and appends to `history.jsonl`, and this window has to
//! find out within a couple of hundred milliseconds — otherwise the human is looking at a
//! timeline that no longer exists, which is worse than looking at nothing.
//!
//! Two decisions here are load-bearing.
//!
//! **The directory is watched, never the two files.** `FsVfs::write` writes a `.tmp`
//! sibling and renames it over the target, so every save replaces the *inode*. An inotify
//! watch registered on `project.json` follows the old inode into the void and goes deaf
//! after the first edit — the window would then update exactly once and look correct while
//! being permanently stale. A watch on the containing directory sees the rename.
//!
//! **Events are debounced.** One op writes `project.json` and then appends to
//! `history.jsonl`, and the write itself arrives as several inotify events (create, write,
//! rename). Forwarding each one reloads the document several times per edit: wasted ffmpeg
//! work, a timeline that flickers, and an activity feed that animates the same entry twice.
//! One edit must produce exactly one wake-up.

use dvs_core::paths::ProjectPaths;
use notify::{RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::Stream;

/// Quiet period that closes a burst of filesystem events. Long enough to cover a document
/// write plus a journal append (microseconds apart in practice), short enough that the edit
/// is on screen before a human looks back at the window.
pub const DEBOUNCE: Duration = Duration::from_millis(120);

/// A stream that yields `()` every time the project's document or journal changes on disk.
///
/// The watcher lives on its own thread and forwards into an unbounded channel, so nothing
/// here needs an async runtime or a timer: the debounce is a blocking
/// [`crossbeam_channel::Receiver::recv_timeout`], which is exactly what a debounce is.
/// The stream ends when the watcher cannot be installed or the project directory goes away;
/// the window keeps working, it just stops learning about other people's edits.
pub fn watch(paths: &ProjectPaths) -> impl Stream<Item = ()> + Send + 'static {
    let (sender, receiver) = mpsc::unbounded_channel();
    let root = paths.root().to_path_buf();
    let document = paths.project_json();
    let journal = paths.history();

    // A named thread, because this one outlives every request and shows up in a backtrace.
    let spawned = std::thread::Builder::new()
        .name("dvs-studio-watch".to_string())
        .spawn(move || pump(&root, &document, &journal, &sender));
    if spawned.is_err() {
        // Nothing to clean up: the sender moved into a closure that never ran, so the
        // receiver below is already closed and the stream terminates immediately.
    }
    UnboundedReceiverStream::new(receiver)
}

/// Install the watcher and forward coalesced bursts until the receiver is dropped.
fn pump(root: &Path, document: &Path, journal: &Path, sender: &mpsc::UnboundedSender<()>) {
    let (events, incoming) = crossbeam_channel::unbounded();
    let Ok(mut watcher) = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        // A watcher error (queue overflow) means "something changed and I lost track of
        // what": treat it as a change, since a spurious reload is cheap and a missed one
        // leaves a stale window.
        let _ = events.send(event.map(|event| event.paths).unwrap_or_default());
    }) else {
        return;
    };
    // Non-recursive: `cache/` under the same root churns with proxies and segments, and
    // none of that changes the document.
    if watcher.watch(root, RecursiveMode::NonRecursive).is_err() {
        return;
    }

    loop {
        // Block until something interesting happens; ignore the rest of the directory.
        match incoming.recv() {
            Ok(paths) if !touches(&paths, document, journal) => continue,
            Ok(_) => {}
            Err(_) => break,
        }
        // Coalesce: keep swallowing events until the directory has been quiet for a beat.
        while incoming.recv_timeout(DEBOUNCE).is_ok() {}
        if sender.send(()).is_err() {
            break;
        }
    }
    // Explicit, so a future edit cannot drop the watcher early and silently stop the loop.
    drop(watcher);
}

/// Whether an event names the document or the journal. A rename over `project.json`
/// reports the destination, so the `.tmp` sibling never has to be recognised.
fn touches(paths: &[PathBuf], document: &Path, journal: &Path) -> bool {
    paths
        .iter()
        .any(|path| path == document || path == journal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::task::{Context, Poll};

    /// Drain everything the stream has ready, waiting `window` for stragglers.
    fn collect(stream: &mut (impl Stream<Item = ()> + Unpin), window: Duration) -> usize {
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        let deadline = std::time::Instant::now() + window;
        let mut seen = 0;
        while std::time::Instant::now() < deadline {
            match std::pin::Pin::new(&mut *stream).poll_next(&mut context) {
                Poll::Ready(Some(())) => seen += 1,
                Poll::Ready(None) => break,
                Poll::Pending => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        seen
    }

    fn project(root: &Path) -> ProjectPaths {
        let paths = ProjectPaths::new(root);
        let project = dvs_core::project::Project::new(
            "watch",
            dvs_core::time::Fps::new(30, 1).unwrap(),
            [64, 36],
            48_000,
        );
        dvs_core::engine::Workspace::create(
            paths.clone(),
            project,
            dvs_core::vfs::FsVfs::shared(),
        )
        .expect("create a project");
        paths
    }

    #[test]
    fn one_op_writing_both_files_wakes_the_window_once() {
        let dir = tempfile::tempdir().unwrap();
        let paths = project(dir.path());
        let mut stream = Box::pin(watch(&paths));
        // Give the watcher thread a moment to install itself before writing.
        std::thread::sleep(Duration::from_millis(150));

        let workspace =
            dvs_core::engine::Workspace::open(paths.clone(), dvs_core::vfs::FsVfs::shared())
                .unwrap();
        let mut engine = dvs_core::engine::Engine::new(dvs_mcp::full_registry(), workspace);
        engine
            .apply(
                "track.add",
                serde_json::json!({ "kind": "video" }),
                None,
                false,
            )
            .expect("track.add writes project.json and history.jsonl");

        // Both files changed; the debounce has to fold that into a single wake-up.
        assert_eq!(
            collect(&mut stream, Duration::from_millis(600)),
            1,
            "one op must wake the window exactly once"
        );
    }

    #[test]
    fn unrelated_files_in_the_project_directory_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let paths = project(dir.path());
        let mut stream = Box::pin(watch(&paths));
        std::thread::sleep(Duration::from_millis(150));

        std::fs::write(paths.root().join("notes.txt"), b"scratch").unwrap();
        std::fs::write(paths.cache_dir().join("proxy.log"), b"noise").unwrap();

        assert_eq!(
            collect(&mut stream, Duration::from_millis(400)),
            0,
            "only the document and the journal are worth a reload"
        );
    }

    #[test]
    fn two_edits_separated_by_the_debounce_wake_the_window_twice() {
        let dir = tempfile::tempdir().unwrap();
        let paths = project(dir.path());
        let mut stream = Box::pin(watch(&paths));
        std::thread::sleep(Duration::from_millis(150));

        for _ in 0..2 {
            std::fs::write(paths.history(), b"").unwrap();
            std::thread::sleep(DEBOUNCE * 3);
        }

        // Debouncing must coalesce a burst, not swallow a later edit.
        assert_eq!(collect(&mut stream, Duration::from_millis(400)), 2);
    }

    #[test]
    fn a_rewritten_document_is_still_seen_after_the_first_edit() {
        let dir = tempfile::tempdir().unwrap();
        let paths = project(dir.path());
        let vfs = dvs_core::vfs::FsVfs::shared();
        let mut stream = Box::pin(watch(&paths));
        std::thread::sleep(Duration::from_millis(150));

        // Two atomic writes: each one replaces the inode. A watch on the file itself would
        // report the first and go deaf; this asserts the directory watch does not.
        for _ in 0..2 {
            let bytes = std::fs::read(paths.project_json()).unwrap();
            dvs_core::vfs::Vfs::write(&*Arc::clone(&vfs), &paths.project_json(), &bytes).unwrap();
            std::thread::sleep(DEBOUNCE * 3);
        }

        assert_eq!(
            collect(&mut stream, Duration::from_millis(400)),
            2,
            "an atomic rewrite must stay visible after the inode is replaced"
        );
    }
}
