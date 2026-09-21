//! Media I/O for degen-video-studio: probe, decode, encode, proxy.
//!
//! This crate is the only place in the workspace that spawns ffmpeg. Everything above it
//! works on [`Frame`] (premultiplied linear f32 RGBA) and interleaved f32 PCM, so the
//! compositor, the mixer and the analysers never know what a container or a codec is.
//!
//! The boundary is deliberate: what an agent *measures* — pixels, samples, loudness,
//! similarity — is computed in Rust and is deterministic, while ffmpeg only moves bytes in
//! and out. That is what makes golden frames meaningful even though encoded bytes differ
//! between ffmpeg builds.

pub mod decode;
pub mod encode;
pub mod frame;
pub mod ops;
pub mod probe;
pub mod proxy;
pub mod toolchain;

pub use decode::{decode_audio, decode_image, DecodeSpec, VideoDecoder};
pub use encode::{concat, mux_audio, write_pcm, AudioSpec, EncodeSpec, Encoder, EncoderSession};
pub use frame::Frame;
pub use probe::{probe, Probed};
pub use proxy::{make_proxy, read_png, thumbnails, waveform, write_png, ProxySpec, Waveform};
pub use toolchain::{ToolReport, Toolchain};

/// Add this crate's ops (`asset.*`) to a registry.
pub fn register(registry: &mut dvs_core::op::Registry) {
    ops::register(registry);
}
