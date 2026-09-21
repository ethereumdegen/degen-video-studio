//! A window that shows a project being edited.
//!
//! The premise of this engine is that the operator is a machine, which leaves a human with
//! nothing to look at. That is a real problem: an agent quietly getting the timeline wrong
//! looks exactly like an agent getting it right, until somebody renders and watches. This
//! window is the instrument for the human side of that loop — the composited viewport, the
//! timeline, and the op journal streaming past as the edits land.
//!
//! **Why a webview.** The view layer is HTML in a Tauri window rather than a Rust-drawn
//! widget set, and that is an accessibility decision before it is a rendering one. HTML
//! semantics map to the platform accessibility APIs — ATK/Orca on Linux,
//! NSAccessibility/VoiceOver on macOS — so a clip can be a labelled control a screen
//! reader announces. As of this writing no Rust-drawn toolkit in this space gives that:
//! iced has no AccessKit integration (issue #552, open since 2020), GPUI and Makepad have
//! none at all, and egui's is partial and would treat a custom timeline canvas as one
//! opaque rectangle. The webview is not a browser dependency either — it is the OS one
//! (WebKitGTK here, WKWebView on macOS).
//!
//! There is a second accessible path that needs no window at all: `dvs-studio --describe`
//! prints the same timeline, activity and findings as text, and the CLI and MCP surfaces
//! remain the authoritative way to *drive* the editor.
//!
//! Human and agent share one document: anything typed into the window's console goes
//! through the same op registry the CLI and MCP server use and lands in the same
//! `history.jsonl`, so either party can undo the other's work.

pub mod app;
pub mod engine;
pub mod monitor;
pub mod state;
pub mod watch;

pub use engine::Handle;
pub use monitor::Monitor;
pub use state::{Applied, Finding, Snapshot, StudioOptions};

/// Open a project in a window. Returns when the window closes.
pub fn run(options: StudioOptions) -> dvs_core::error::Result<()> {
    app::run(options)
}

/// Print the window's contents as text and return, for terminals and screen readers.
pub fn describe(options: &StudioOptions) -> dvs_core::error::Result<String> {
    app::describe(options)
}
