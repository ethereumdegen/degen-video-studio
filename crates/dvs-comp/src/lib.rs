//! The compositor: the one renderer in degen-video-studio.
//!
//! Given a project, a sequence and a frame index, [`Compositor`] produces a [`Frame`] —
//! premultiplied linear f32 RGBA. Everything a frame can contain lives here: placement
//! ([`geometry`]), blend modes ([`blend`]), transitions ([`transition`]), titles and
//! captions ([`title`]), synthetic sources ([`generator`]) and the per-clip effect chain
//! ([`effects`]).
//!
//! No caching, encoding or scheduling happens at this level; `dvs-render` owns those. That
//! split is what lets the interactive viewport and a headless render share one code path
//! while differing in how aggressively they cache.

pub mod blend;
pub mod compositor;
pub mod effects;
pub mod generator;
pub mod geometry;
pub mod ops;
pub mod title;
pub mod transition;

pub use compositor::{CompOptions, Compositor, FrameReport, LayerReport};
pub use effects::{
    analyze_stabilization, parse_trf, stabilize_cache_path, EffectCx, Lut, CATALOG,
};
pub use geometry::{place, Placement, Rect};
pub use title::{Rasterizer, TextReport};

pub use dvs_media::Frame;

/// Add this crate's ops (`fx.analyze`) to a registry.
pub fn register(registry: &mut dvs_core::op::Registry) {
    ops::register(registry);
}
