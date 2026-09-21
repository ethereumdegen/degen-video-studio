//! Text as an editing surface: transcripts, word-level edits, captions.
//!
//! `PLAN.md` §1.2 names the gap this crate closes. An agent cannot watch the video, but
//! most agent video work — talking-head, screencast, social cuts — is driven by what was
//! said and when. So a word-timestamped transcript is a first-class document layer
//! ([`transcript`]), the timeline ops that read it cut and keep by *phrase* rather than by
//! timecode ([`ops`]), and captions are generated from the same words under reading-rate
//! and line-length constraints an agent can check ([`captions`]).
//!
//! Two boundaries are deliberate:
//!
//! - **Transcripts belong to assets, not clips.** One transcription survives every trim,
//!   split and retime, and "the words in this clip" is a query
//!   ([`Transcript::words_for_clip`]) rather than a copy that some op will forget to update.
//! - **Transcription is optional.** The [`whisper`] runner is behind the off-by-default
//!   `whisper` feature, which links whisper.cpp and therefore needs cmake and a C++
//!   toolchain to build. Everything else here works on words that arrived from anywhere, so
//!   `transcript.import` is a complete keyless, modelless path into this layer.
//!
//! SRT and VTT serialization is not here: it lives in `dvs-interop` beside the other
//! interchange formats, and `caption.import`/`caption.export` call into it rather than
//! growing a second subtitle parser.

pub mod captions;
pub mod ops;
pub mod transcript;
pub mod whisper;

pub use captions::{cue_svg, cues_from_words, wrap_lines};
pub use transcript::{Transcript, Word};
pub use whisper::RunSpec;

/// Add this crate's ops (`transcript.*`, `caption.*`) to a registry.
pub fn register(registry: &mut dvs_core::op::Registry) {
    ops::register(registry);
}
