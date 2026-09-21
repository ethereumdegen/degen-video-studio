//! degen-video-studio core: the document, the op registry, the journal.
//!
//! This crate knows nothing about ffmpeg, pixels or samples. It owns the canonical
//! `project.json` model ([`project`]), exact time ([`time`]), addressing ([`ids`],
//! [`selector`]), the op spine ([`op`], [`engine`]) and storage boundaries ([`vfs`],
//! [`asset`], [`journal`]). Every other crate registers ops into [`op::Registry`] and is
//! reached only through them, which is what keeps the CLI, the MCP server and the GUI on
//! one code path.

pub mod asset;
pub mod color;
pub mod engine;
pub mod error;
pub mod ids;
pub mod journal;
pub mod op;
pub mod ops;
pub mod paths;
pub mod project;
pub mod selector;
pub mod time;
pub mod vfs;

pub use asset::AssetStore;
pub use color::Rgba;
pub use engine::{Applied, Engine, Workspace};
pub use error::{exit, Error, ErrorReport, Result};
pub use ids::{
    AssetId, ClipId, CueId, EffectId, MarkerId, ProjectId, SequenceId, StyleId, TitleId, TrackId,
};
pub use journal::{Actor, Entry, Journal};
pub use op::{args, Op, OpCx, OpEffect, OpInfo, Registry, Warning};
pub use paths::ProjectPaths;
pub use project::{
    Asset, AssetKind, AudioStream, Blend, CaptionCue, CaptionPosition, CaptionStyle, Clip,
    ColorMatrix, ColorRange, Crop, Direction, Ducking, Easing, Effect, Fit, Generator, Keyframe,
    Marker, Probe, Project, Provenance, Sequence, Source, Title, Tools, Track, TrackKind,
    Transform, Transition, TransitionKind, VideoStream, FORMAT_VERSION,
};
pub use selector::{resolve, Match, Selector};
pub use time::{Fps, Rat, Span, Time};
pub use vfs::{FsVfs, MemVfs, Vfs};

/// Engine version recorded in render provenance and in segment cache keys, so a cached
/// segment produced by an older compositor is never reused after a behaviour change.
pub const ENGINE_VERSION: &str = concat!("dvs-", env!("CARGO_PKG_VERSION"));

/// A registry with the document ops this crate owns. Other crates add theirs on top:
/// `dvs-media` the import ops, `dvs-audio` the mix ops, and so on.
pub fn registry() -> Registry {
    let mut registry = Registry::new();
    ops::register(&mut registry);
    registry
}

/// JSON Schema for `project.json`, published so an agent can validate a document it wrote
/// by hand and so editors can offer completion.
pub fn project_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(Project)).expect("schema is serializable")
}
