//! Document ops: everything that mutates `project.json` without touching media.
//!
//! Ops that need ffmpeg, a model, or pixels live in the crate that owns that capability and
//! register themselves into the same [`Registry`], so `dvs op --list` is the whole surface
//! regardless of which crate implements what.

use crate::op::Registry;

pub mod clip;
pub mod fx;
pub mod marker;
pub mod seq;
pub mod title;
pub mod track;
pub mod util;

pub fn register(registry: &mut Registry) {
    seq::register(registry);
    track::register(registry);
    clip::register(registry);
    fx::register(registry);
    marker::register(registry);
    title::register(registry);
}
