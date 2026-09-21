//! The render scheduler: a timeline in, a file out, and as little re-encoding as possible.
//!
//! An agent loop makes twenty small edits and asks for the video after each one. Re-encoding
//! ten minutes twenty times is not an editor, it is a build farm. So a render is not one
//! ffmpeg run over the whole timeline: [`plan::segment_plan`] cuts the range at the points
//! where the composition changes discontinuously, [`cache::segment_key`] hashes everything
//! that can affect a segment's pixels, and [`pipeline::render`] encodes only the segments
//! whose key is not already on disk before joining them with `concat -c copy`.
//!
//! Three properties make that safe rather than merely fast:
//!
//! - **Closed-GOP chunks.** Every segment is encoded with `EncodeSpec::closed_gop`, so no
//!   frame references across a segment boundary and stream-copy concatenation is a valid
//!   operation rather than a gamble on the encoder's lookahead.
//! - **A key that over-includes.** A false cache miss costs seconds; a false hit ships the
//!   wrong video. [`cache`] therefore hashes the document slice, the asset hashes, the
//!   engine version, the ffmpeg version and every encoder setting — see its module doc.
//! - **One audio bounce.** Audio is mixed once over the whole range and muxed in at the end,
//!   because splitting a mix at video segment boundaries would put a seam in the sound
//!   wherever a reverb tail or a cross-fade crossed one.
//!
//! The compositor ([`dvs_comp`]) owns pixels and this crate owns scheduling, so the
//! interactive viewport and a headless render share one frame path and differ only in how
//! aggressively they cache.

pub mod cache;
pub mod pipeline;
pub mod plan;

pub use cache::{prune, report_path, segment_key, segment_path, status, CacheEntry, CacheStatus};
pub use pipeline::{
    plan_keys, preview_frames, render, render_frame, render_with, Progress, RenderReport,
    RenderSpec, Silent,
};
pub use plan::{frame_range, min_segment, segment_plan, segment_plan_with, DEFAULT_MIN_SEGMENT};
