//! Optional AI providers: fal.ai generation, text to speech, request cache, spend budget.
//!
//! `PLAN.md` §2 calls AI "optional and additive", which in practice means three properties
//! this crate has to hold, because each of them fails quietly if nobody enforces it:
//!
//! 1. **The editor is complete with no keys configured.** Nothing else in the workspace
//!    depends on this crate succeeding. Every op here fails with [`dvs_core::Error::Tool`]
//!    (exit code 5) naming the exact environment variable and how to set it, or with
//!    [`dvs_core::Error::Budget`] (exit code 6). None of them panics, and none of them is a
//!    prerequisite for editing, rendering or inspecting.
//! 2. **Generated output enters the document as native structure.** A generation is hashed
//!    into the asset store, probed by ffprobe, recorded with [`dvs_core::Provenance`], and —
//!    when `--at` is given — placed as an ordinary clip. The document never holds a remote
//!    URL: an edit that depends on a provider's CDN staying up is not an edit, it is a
//!    bookmark.
//! 3. **Spend is bounded and re-running is free.** [`cache`] identifies a request by
//!    `blake3(provider, model, canonical params)` so a repeated request is served from disk
//!    at zero cost, and [`budget`] refuses a request that would cross a per-project ceiling
//!    before anything is sent.
//!
//! The layering follows those properties: [`provider`] owns credentials and the retrying,
//! injectable HTTP transport; [`fal`] and [`tts`] speak to providers; [`cache`] and
//! [`budget`] bound the cost; [`ops`] is the only surface the rest of the system sees.

pub mod budget;
pub mod cache;
pub mod fal;
pub mod ops;
pub mod provider;
pub mod tts;

pub use budget::{Budget, Kind, Ledger};
pub use cache::{canonical_json, request_key, AiCache, CachedRequest};
pub use fal::{Fal, FalOutput, MediaRef, PollSpec};
pub use ops::{register, register_with};
pub use provider::{
    default_transport, Auth, HttpClient, HttpRequest, HttpResponse, Method, Provider,
    ProviderConfig, ReqwestTransport, RetryPolicy, Transport,
};
pub use tts::{Engine, Piper};

use chrono::Utc;
use dvs_core::error::{Error, Result};
use dvs_core::ids::AssetId;
use dvs_core::op::OpCx;
use dvs_core::project::{Asset, Project, Provenance};
use dvs_media::probe::probe;
use dvs_media::toolchain::Toolchain;
use std::path::PathBuf;

/// A finished generation on local disk, before it becomes part of the document.
///
/// The file is already in the request cache, so importing it is a copy inside the project
/// rather than a second download, and a failed import can be retried without paying again.
#[derive(Debug, Clone)]
pub struct Generated {
    pub provider: String,
    pub model: String,
    pub prompt: Option<String>,
    pub seed: Option<i64>,
    /// What this project was charged for this file: zero for a cache hit or a local engine.
    pub cost_usd: f64,
    /// Whether the cache answered instead of the provider.
    pub cached: bool,
    /// Local path of the generated media, inside `cache/ai/<key>/`.
    pub media: PathBuf,
    /// Where the provider served it from. Kept for forensics and reported in the op
    /// effect; deliberately not stored in the document.
    pub source_url: Option<String>,
}

/// Bring a generated file into the document as an ordinary asset.
///
/// This is the whole of property 2, and the order matters: the bytes are hashed into the
/// content-addressed store first, then probed *at their stored path*, so what the document
/// describes is exactly what the store holds. A generation that probes as neither audio nor
/// video is refused rather than recorded — an asset with a zero duration is a clip that
/// renders as nothing, and finding that out at render time is how an agent wastes an hour.
pub fn import_generated(
    project: &mut Project,
    cx: &OpCx,
    tool: &Toolchain,
    generated: &Generated,
    name: Option<&str>,
) -> Result<AssetId> {
    let bytes = cx.assets.vfs().read(&generated.media)?;
    let extension = generated
        .media
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("bin");
    let hash = cx.assets.import_bytes(&bytes, extension)?;
    let stored = cx.assets.find(&hash)?;
    let probed = probe(tool, &stored)?;
    if probed.probe.video.is_none() && probed.probe.audio.is_none() {
        return Err(Error::tool(
            generated.provider.clone(),
            format!(
                "{} returned {} bytes that ffprobe reads as neither audio nor video; nothing \
                 was added to the project (the file is kept at {})",
                generated.model,
                bytes.len(),
                generated.media.display()
            ),
        ));
    }

    let id = AssetId::new();
    let asset = Asset {
        id: id.clone(),
        name: name
            .map(str::to_string)
            .unwrap_or_else(|| suggested_name(generated, extension)),
        hash,
        kind: probed.kind,
        probe: probed.probe,
        proxy: None,
        source_path: Some(cx.paths.relativize(&generated.media)),
        imported: Utc::now(),
        provenance: Some(Provenance {
            provider: generated.provider.clone(),
            model: generated.model.clone(),
            prompt: generated.prompt.clone(),
            seed: generated.seed,
            cost_usd: Some(generated.cost_usd),
        }),
    };
    project.assets.insert(id.clone(), asset);
    Ok(id)
}

/// A file name a human can recognise in a bin listing: the start of the prompt, or the
/// model when there is no prompt.
fn suggested_name(generated: &Generated, extension: &str) -> String {
    let source = generated
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .unwrap_or(&generated.model);
    let mut slug = String::new();
    // Splitting on the separators a model id uses as well as whitespace keeps
    // `fal-ai/flux/schnell` readable instead of collapsing it to one run of letters.
    let words = source.split(|c: char| c.is_whitespace() || matches!(c, '/' | '-' | '_'));
    for word in words.take(6) {
        let cleaned: String = word
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect();
        if cleaned.is_empty() {
            continue;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&cleaned);
        if slug.len() >= 40 {
            break;
        }
    }
    if slug.is_empty() {
        slug.push_str("generated");
    }
    format!("{}-{slug}.{extension}", generated.provider)
}

/// Shared test doubles.
///
/// Credentials, the budget ceiling and the piper paths all come from process environment
/// variables, and `cargo test` runs tests as threads in one process: without a lock, one
/// test's `FAL_KEY` is another's surprise, and a developer's real key would make the
/// "no key configured" tests pass for the wrong reason.
#[cfg(test)]
pub(crate) mod testkit {
    use crate::provider::{HttpRequest, HttpResponse, Transport};
    use dvs_core::error::{Error, Result};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    static ENV: Mutex<()> = Mutex::new(());

    /// Serialize environment access, clear every variable this crate reads, and point the
    /// credentials file at a path that does not exist.
    ///
    /// Both halves matter. Poisoning is recovered from deliberately — the lock guards
    /// ordering, not data, so a test that panicked while holding it has left nothing
    /// inconsistent. And the config override is *set* rather than removed, because
    /// clearing it would fall back to `~/.config/dvs/providers.json`: on a machine where
    /// the developer has a real fal key, the "no key configured" tests would then pass for
    /// entirely the wrong reason.
    pub(crate) fn isolate() -> MutexGuard<'static, ()> {
        let guard = ENV.lock().unwrap_or_else(PoisonError::into_inner);
        for name in [
            crate::fal::KEY_ENV,
            crate::budget::CEILING_ENV,
            crate::tts::PIPER_ENV,
            crate::tts::VOICES_ENV,
        ] {
            std::env::remove_var(name);
        }
        std::env::set_var(
            crate::provider::CONFIG_ENV,
            "/nonexistent/dvs-test/providers.json",
        );
        guard
    }

    /// A transport that fails the test if it is used at all. Every path that must be
    /// offline — a cache hit, a dry run, a refused budget, a missing key — is proved
    /// offline by handing it this.
    pub(crate) struct Forbidden;

    impl Transport for Forbidden {
        fn send(&self, request: &HttpRequest) -> Result<HttpResponse> {
            panic!("this path must not reach the network, but it requested {request:?}");
        }
    }

    /// Answers by URL substring, so one stub can script a whole submit/poll/fetch/download
    /// conversation.
    pub(crate) struct Routed {
        routes: Vec<(&'static str, HttpResponse)>,
        pub(crate) calls: AtomicU32,
    }

    impl Routed {
        pub(crate) fn new(routes: Vec<(&'static str, HttpResponse)>) -> Arc<Routed> {
            Arc::new(Routed {
                routes,
                calls: AtomicU32::new(0),
            })
        }

        pub(crate) fn calls(&self) -> u32 {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl Transport for Routed {
        fn send(&self, request: &HttpRequest) -> Result<HttpResponse> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.routes
                .iter()
                .find(|(pattern, _)| request.url.contains(pattern))
                .map(|(_, response)| response.clone())
                .ok_or_else(|| Error::op(format!("no stub route for {}", request.url)))
        }
    }

    pub(crate) fn json_response(value: serde_json::Value) -> HttpResponse {
        HttpResponse::new(200, value.to_string().into_bytes())
            .with_header("content-type", "application/json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generated(prompt: Option<&str>) -> Generated {
        Generated {
            provider: "fal".into(),
            model: "fal-ai/flux/schnell".into(),
            prompt: prompt.map(str::to_string),
            seed: Some(7),
            cost_usd: 0.05,
            cached: false,
            media: PathBuf::from("/p/cache/ai/key/media.png"),
            source_url: None,
        }
    }

    #[test]
    fn a_generated_name_carries_the_prompt_into_the_bin_listing() {
        assert_eq!(
            suggested_name(&generated(Some("A Cat, riding a SKATEBOARD downtown at dusk")), "png"),
            "fal-a-cat-riding-a-skateboard-downtown.png"
        );
    }

    #[test]
    fn a_nameless_generation_falls_back_to_the_model_then_to_a_constant() {
        assert_eq!(
            suggested_name(&generated(None), "mp4"),
            "fal-fal-ai-flux-schnell.mp4"
        );
        assert_eq!(
            suggested_name(&generated(Some("   ***   ")), "mp4"),
            "fal-generated.mp4",
            "a prompt of punctuation must still yield a usable file name"
        );
    }
}
