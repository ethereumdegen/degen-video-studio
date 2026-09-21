//! Interchange with the tools a human finishes the job in.
//!
//! An agent can cut a video, but the last ten percent — the taste pass — happens in
//! Kdenlive, Final Cut or Resolve, and the project has to arrive there intact. This crate
//! is that bridge: it writes the timeline as MLT XML (which is literally what a
//! `.kdenlive` file is), FCPXML, OpenTimelineIO and CMX3600 EDL, reads back the MLT
//! subset a human's later GUI edits come home in, and moves captions in and out as SRT
//! and WebVTT.
//!
//! It is also the cheapest end-to-end proof the project has: `project.json` → MLT XML →
//! `melt` → mp4 exercises the whole document model without a single line of the native
//! renderer, which is why the `.kdenlive` writer was written first.
//!
//! Every writer returns its warnings rather than swallowing them. An export that quietly
//! drops a color grade, a title or a transition is worse than one that refuses: the human
//! opening the file has no way to know what used to be there. So each unrepresentable
//! feature becomes a [`dvs_core::op::Warning`] on the op's result, and the placeholder
//! that replaces it keeps the original length so no cut moves.

pub mod edl;
pub mod fcpxml;
pub mod mlt;
pub mod ops;
pub mod otio;
pub mod subtitle;
pub mod timeline;

#[cfg(test)]
mod tests_support;

use dvs_core::asset::AssetStore;
use dvs_core::error::Result;
use dvs_core::ids::SequenceId;
use dvs_core::op::{Registry, Warning};
use dvs_core::paths::ProjectPaths;
use dvs_core::project::Project;

pub use mlt::{from_mlt, ImportedClip, ImportedTimeline, ImportedTrack};
pub use subtitle::{from_srt, from_vtt, to_srt, to_vtt};

/// A written interchange document plus what could not be written into it.
#[derive(Debug, Clone)]
pub struct Export {
    pub text: String,
    pub warnings: Vec<Warning>,
}

/// MLT XML for a sequence. `kdenlive` adds the `kdenlive:*` properties that make the same
/// document a Kdenlive 26.08 project file.
pub fn to_mlt(
    project: &Project,
    sequence: &SequenceId,
    paths: &ProjectPaths,
    assets: &AssetStore,
    kdenlive: bool,
) -> Result<String> {
    Ok(mlt::export(project, sequence, paths, assets, kdenlive)?.text)
}

/// FCPXML 1.11, for Final Cut Pro and Resolve.
pub fn to_fcpxml(
    project: &Project,
    sequence: &SequenceId,
    paths: &ProjectPaths,
    assets: &AssetStore,
) -> Result<String> {
    Ok(fcpxml::export(project, sequence, paths, assets)?.text)
}

/// OpenTimelineIO JSON, for everything else.
pub fn to_otio(
    project: &Project,
    sequence: &SequenceId,
    paths: &ProjectPaths,
    assets: &AssetStore,
) -> Result<String> {
    Ok(otio::export(project, sequence, paths, assets)?.text)
}

/// A CMX3600 EDL: the oldest, dumbest, most universally readable description of a cut.
pub fn to_edl(project: &Project, sequence: &SequenceId) -> Result<String> {
    Ok(edl::export(project, sequence)?.text)
}

/// Add this crate's ops (`export.*`, `import.mlt`) to a registry.
pub fn register(registry: &mut Registry) {
    ops::register(registry);
}
