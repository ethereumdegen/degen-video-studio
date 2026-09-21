//! Who can be called, with what credential, over what transport.
//!
//! This is the only outbound network path in the workspace, and every failure mode it has
//! costs either money or truth, so each one is handled explicitly rather than inherited
//! from an HTTP library's defaults:
//!
//! - **A missing key must name the variable to set.** A 401 body from a provider tells an
//!   agent nothing actionable; [`ProviderConfig::require`] tells it exactly which
//!   environment variable or config entry is absent.
//! - **A key must not leak.** Credentials live in [`SecretString`] and never enter a header
//!   map or a `Debug` rendering; [`HttpRequest`] and [`ProviderConfig`] implement `Debug`
//!   by hand so a log line or a panic message cannot print one.
//! - **A 429 or a 5xx must be retried, boundedly.** An agent loop that retries forever is
//!   a denial of service against the provider and a hang for its operator, so the policy is
//!   a fixed attempt count with jittered exponential backoff.
//! - **A test must not reach the network.** [`Transport`] is a trait and the generation ops
//!   take one as a constructor argument, so the retry policy, the cache and the budget are
//!   all testable against a stub that would panic if a real request were made.

use dvs_core::error::{Error, Result};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

/// Overrides the credentials file location. Exists so a test run — and a CI runner — can
/// point at an empty file instead of inheriting a developer's real keys.
pub const CONFIG_ENV: &str = "DVS_PROVIDERS_FILE";

/// Ceiling on one round trip. Generous, because a queue submit against a video model can
/// sit for a while; bounded, because a hung request inside an agent loop is
/// indistinguishable from a hung agent.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// A generation backend, hosted or local.
pub trait Provider {
    /// Stable short name: the `provider` field recorded in [`dvs_core::Provenance`] and the
    /// key under which a credential is looked up in `providers.json`.
    fn name(&self) -> &'static str;

    /// The environment variable that configures this provider: an API key for a hosted one,
    /// a binary path for a local one. Named in every "not configured" error.
    fn key_env(&self) -> &'static str;

    /// Whether a request could be made right now. False is a normal state, not an error:
    /// the editor is complete with no providers configured at all.
    fn available(&self) -> bool;
}

/// One entry in `providers.json`. Both shapes appear in the wild — a bare string is what
/// people write, an object is what a generator writes — and rejecting either would be a
/// silent "your key is ignored".
#[derive(Deserialize)]
#[serde(untagged)]
enum Entry {
    Key(String),
    Object { key: String },
}

/// Credentials resolved from the environment and from `~/.config/dvs/providers.json`.
///
/// The environment wins, so a one-off `FAL_KEY=… dvs op …` overrides a stored key without
/// editing a file.
pub struct ProviderConfig {
    keys: BTreeMap<String, SecretString>,
    source: Option<PathBuf>,
}

impl ProviderConfig {
    /// No stored credentials. The environment is still consulted by [`ProviderConfig::key`].
    pub fn empty() -> ProviderConfig {
        ProviderConfig {
            keys: BTreeMap::new(),
            source: None,
        }
    }

    /// Read the credentials file if there is one. A malformed file is an error rather than
    /// an empty result: "your JSON has a trailing comma" is a fixable complaint, whereas
    /// silently behaving as if the key were absent sends the operator looking at the wrong
    /// layer.
    pub fn load() -> Result<ProviderConfig> {
        let Some(path) = ProviderConfig::path() else {
            return Ok(ProviderConfig::empty());
        };
        if !path.is_file() {
            return Ok(ProviderConfig::empty());
        }
        let bytes = std::fs::read(&path).map_err(|e| Error::io(&path, e))?;
        let raw: BTreeMap<String, Entry> =
            serde_json::from_slice(&bytes).map_err(|e| Error::json(&path, e))?;
        let keys = raw
            .into_iter()
            .filter_map(|(name, entry)| {
                let key = match entry {
                    Entry::Key(key) => key,
                    Entry::Object { key } => key,
                };
                let key = key.trim().to_string();
                (!key.is_empty()).then(|| (name, SecretString::from(key)))
            })
            .collect();
        Ok(ProviderConfig {
            keys,
            source: Some(path),
        })
    }

    /// Where credentials are read from: `$DVS_PROVIDERS_FILE`, else
    /// `$XDG_CONFIG_HOME/dvs/providers.json`, else `~/.config/dvs/providers.json`.
    pub fn path() -> Option<PathBuf> {
        if let Some(explicit) = env_value(CONFIG_ENV) {
            return Some(PathBuf::from(explicit));
        }
        if let Some(config_home) = env_value("XDG_CONFIG_HOME") {
            return Some(Path::new(&config_home).join("dvs").join("providers.json"));
        }
        env_value("HOME").map(|home| {
            Path::new(&home)
                .join(".config")
                .join("dvs")
                .join("providers.json")
        })
    }

    /// The file this config came from, when it came from one.
    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// A provider's key: `$<env_var>` first, then the file under the provider's name, then
    /// under the environment variable's name — because someone who read the error message
    /// naming `FAL_KEY` will reasonably use that as the JSON key.
    pub fn key(&self, provider: &str, env_var: &str) -> Option<SecretString> {
        if let Some(value) = env_value(env_var) {
            return Some(SecretString::from(value));
        }
        self.keys
            .get(provider)
            .or_else(|| self.keys.get(env_var))
            .map(|secret| SecretString::from(secret.expose_secret().to_string()))
    }

    /// The key, or the exact instruction for supplying it.
    pub fn require(&self, provider: &str, env_var: &str) -> Result<SecretString> {
        self.key(provider, env_var)
            .ok_or_else(|| missing_key(provider, env_var))
    }
}

/// The "not configured" error, in one place because it is the first thing an operator sees
/// when they try an AI op and the only thing that tells them what to do about it. Exit code
/// 5 (`tool missing`) — an unconfigured provider is an unusable tool, not a bad document.
pub fn missing_key(provider: &str, env_var: &str) -> Error {
    let file = ProviderConfig::path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "~/.config/dvs/providers.json".to_string());
    Error::tool(
        provider,
        format!(
            "no API key configured: export {env_var}=<key>, or add {{\"{provider}\": \"<key>\"}} to {file}"
        ),
    )
}

/// Redacted by hand. `SecretString` already prints as `[REDACTED]`, but this type is the
/// one that ends up in a context struct someone derives `Debug` on, so the guarantee is
/// pinned here and tested.
impl fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("source", &self.source)
            .field("providers", &self.keys.keys().collect::<Vec<_>>())
            .field("keys", &"[REDACTED]")
            .finish()
    }
}

impl fmt::Display for ProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.keys.keys().map(String::as_str).collect();
        match (&self.source, names.is_empty()) {
            (_, true) => write!(f, "no stored provider credentials"),
            (Some(path), false) => write!(
                f,
                "credentials for {} from {}",
                names.join(", "),
                path.display()
            ),
            (None, false) => write!(f, "credentials for {}", names.join(", ")),
        }
    }
}

/// A non-empty environment variable, trimmed. An exported-but-empty variable means "unset"
/// here: `FAL_KEY=` in a shell profile should not turn into a 401.
fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }
}

/// An `Authorization` header, kept unassembled so the secret exists as a plain string only
/// for the moment the transport writes it onto the wire.
pub struct Auth {
    scheme: &'static str,
    secret: SecretString,
}

impl Auth {
    pub fn new(scheme: &'static str, secret: SecretString) -> Auth {
        Auth { scheme, secret }
    }

    /// The header value. Called once per attempt, inside the transport.
    pub fn header_value(&self) -> String {
        format!("{} {}", self.scheme, self.secret.expose_secret())
    }
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Auth {{ scheme: {:?}, secret: [REDACTED] }}", self.scheme)
    }
}

/// A request, transport-agnostic so that retries, caching and tests all see the same value.
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub auth: Option<Auth>,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

impl HttpRequest {
    pub fn get(url: impl Into<String>) -> HttpRequest {
        HttpRequest {
            method: Method::Get,
            url: url.into(),
            auth: None,
            headers: Vec::new(),
            body: None,
        }
    }

    /// A JSON POST. The body is serialized once, not per attempt.
    pub fn post_json(url: impl Into<String>, body: &serde_json::Value) -> Result<HttpRequest> {
        let bytes = serde_json::to_vec(body)
            .map_err(|e| Error::op(format!("request body is not serializable: {e}")))?;
        Ok(HttpRequest {
            method: Method::Post,
            url: url.into(),
            auth: None,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Some(bytes),
        })
    }

    pub fn with_auth(mut self, auth: Option<Auth>) -> HttpRequest {
        self.auth = auth;
        self
    }

}

/// Header *names* only, and no body: a request body can carry a prompt, and an
/// `Authorization` value carries the key.
impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("authenticated", &self.auth.is_some())
            .field("headers", &names)
            .field("bodyBytes", &self.body.as_ref().map(Vec::len).unwrap_or(0))
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    /// Lower-cased header names, so lookups do not depend on what the server capitalized.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// A response built from parts; the shape a test stub returns.
    pub fn new(status: u16, body: Vec<u8>) -> HttpResponse {
        HttpResponse {
            status,
            headers: BTreeMap::new(),
            body,
        }
    }

    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> HttpResponse {
        self.headers.insert(name.to_ascii_lowercase(), value.into());
        self
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn json(&self) -> Result<serde_json::Value> {
        serde_json::from_slice(&self.body).map_err(|e| {
            Error::op(format!(
                "expected a JSON response, got {} bytes that do not parse ({e}): {}",
                self.body.len(),
                self.snippet(200)
            ))
        })
    }

    /// A single-line, length-capped rendering for an error message. Provider error bodies
    /// are occasionally megabytes of HTML.
    pub fn snippet(&self, limit: usize) -> String {
        let text = String::from_utf8_lossy(&self.body);
        let flat: String = text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let trimmed = flat.trim();
        if trimmed.chars().count() <= limit {
            return trimmed.to_string();
        }
        let head: String = trimmed.chars().take(limit).collect();
        format!("{head}…")
    }
}

/// One attempt at one request. Retries live above this, so a stub in a test observes
/// exactly the attempts the policy decided to make.
pub trait Transport: Send + Sync {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse>;
}

/// How hard to try. Bounded by construction: there is no "retry until it works" setting,
/// because that setting is how an agent loop turns a provider outage into an infinite bill.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> RetryPolicy {
        RetryPolicy {
            attempts: 4,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        }
    }
}

/// Statuses worth a second attempt: rate limiting, request timeouts, and anything the
/// server blames on itself. A 4xx that is not 408/425/429 is a bug in our request and
/// retrying it only multiplies the log noise.
fn retryable(status: u16) -> bool {
    matches!(status, 408 | 425 | 429) || (500..600).contains(&status)
}

/// Retrying HTTP against an injectable transport.
pub struct HttpClient {
    label: &'static str,
    transport: Arc<dyn Transport>,
    policy: RetryPolicy,
    retries: AtomicU32,
}

impl HttpClient {
    pub fn new(label: &'static str, transport: Arc<dyn Transport>) -> HttpClient {
        HttpClient::with_policy(label, transport, RetryPolicy::default())
    }

    pub fn with_policy(
        label: &'static str,
        transport: Arc<dyn Transport>,
        policy: RetryPolicy,
    ) -> HttpClient {
        HttpClient {
            label,
            transport,
            policy,
            retries: AtomicU32::new(0),
        }
    }

    /// How many attempts beyond the first this client has made. Reported in the op effect,
    /// because "it took four tries" explains a slow generation that otherwise looks hung.
    pub fn retries(&self) -> u32 {
        self.retries.load(Ordering::Relaxed)
    }

    /// Send, retrying retryable failures. `Ok` always carries a 2xx response.
    pub fn send(&self, request: HttpRequest) -> Result<HttpResponse> {
        let attempts = self.policy.attempts.max(1);
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let detail = match self.transport.send(&request) {
                Ok(response) if response.is_success() => return Ok(response),
                Ok(response) => {
                    let detail = format!("HTTP {} {}", response.status, response.snippet(200));
                    if !retryable(response.status) {
                        return Err(self.failure(&request, attempt, &detail));
                    }
                    detail
                }
                // A malformed URL or an unserializable body will fail identically on every
                // attempt; only transient transport faults are worth repeating.
                Err(error @ Error::BadArgs(_)) => return Err(error),
                Err(error) => error.to_string(),
            };
            if attempt >= attempts {
                return Err(self.failure(&request, attempt, &detail));
            }
            self.retries.fetch_add(1, Ordering::Relaxed);
            let delay = self.backoff(attempt);
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
        }
    }

    /// Exponential with full jitter, clamped. Jitter matters even for a single client:
    /// several ops fired from one batch would otherwise retry in lockstep and be rate
    /// limited together.
    fn backoff(&self, attempt: u32) -> Duration {
        if self.policy.base_delay.is_zero() {
            return Duration::ZERO;
        }
        let factor = 1u32 << (attempt - 1).min(16);
        let raw = self
            .policy
            .base_delay
            .saturating_mul(factor)
            .min(self.policy.max_delay);
        let half = raw / 2;
        half + half.mul_f64(jitter_fraction())
    }

    fn failure(&self, request: &HttpRequest, attempts: u32, detail: &str) -> Error {
        Error::tool(
            self.label,
            format!(
                "{} {} failed after {attempts} attempt(s): {detail}",
                request.method.as_str(),
                request.url
            ),
        )
    }
}

/// Spread, not unpredictability: the clock's nanoseconds give retries a different phase per
/// process at none of the cost of a PRNG dependency.
fn jitter_fraction() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos % 1_000_000) / 1_000_000.0
}

/// Reject anything that is not plain HTTP(S) before it reaches the network stack. Provider
/// responses name the file to download, and a `file://` or `data:` URL arriving from a
/// remote service is either a bug or an attack.
pub fn validate_url(url: &str) -> Result<()> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| {
            Error::bad_args(format!("'{url}' is not an http(s) url"))
        })?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() {
        return Err(Error::bad_args(format!("'{url}' has no host")));
    }
    Ok(())
}

/// The host part of an http(s) URL, without port or credentials.
pub fn url_host(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    Some(authority.split(':').next().unwrap_or(authority))
}

/// The real transport: `reqwest` over a process-wide client and runtime.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReqwestTransport;

impl ReqwestTransport {
    pub fn new() -> ReqwestTransport {
        ReqwestTransport
    }
}

/// What the generation ops use when nobody injected anything.
pub fn default_transport() -> Arc<dyn Transport> {
    Arc::new(ReqwestTransport::new())
}

struct Shared {
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
}

/// One runtime and one connection pool per process. Building a client per request would
/// re-do TLS setup for every poll of a queued job.
static SHARED: LazyLock<std::result::Result<Shared, String>> = LazyLock::new(|| {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start an async runtime: {e}"))?;
    let client = reqwest::Client::builder()
        .user_agent(concat!("dvs/", env!("CARGO_PKG_VERSION")))
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|e| format!("cannot build an http client: {e}"))?;
    Ok(Shared { runtime, client })
});

fn shared() -> Result<&'static Shared> {
    match &*SHARED {
        Ok(shared) => Ok(shared),
        Err(message) => Err(Error::tool("http", message.clone())),
    }
}

/// Run a future to completion from synchronous code.
///
/// Ops are synchronous by design — the registry is one code path for the CLI, MCP and GUI —
/// so somewhere a future has to be driven. `Runtime::block_on` panics when called from
/// inside another runtime's worker, which is exactly where the MCP server would call us
/// from, so in that case the work moves to a scratch thread that owns no reactor.
fn block_on<F>(future: F) -> Result<F::Output>
where
    F: Future + Send,
    F::Output: Send,
{
    let shared = shared()?;
    if tokio::runtime::Handle::try_current().is_err() {
        return Ok(shared.runtime.block_on(future));
    }
    std::thread::scope(|scope| scope.spawn(|| shared.runtime.block_on(future)).join())
        .map_err(|_| Error::tool("http", "the http worker thread panicked"))
}

impl Transport for ReqwestTransport {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse> {
        validate_url(&request.url)?;
        let shared = shared()?;
        block_on(execute(&shared.client, request))?
    }
}

async fn execute(client: &reqwest::Client, request: &HttpRequest) -> Result<HttpResponse> {
    let mut builder = match request.method {
        Method::Get => client.get(&request.url),
        Method::Post => client.post(&request.url),
    };
    for (name, value) in &request.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    if let Some(auth) = &request.auth {
        builder = builder.header("authorization", auth.header_value());
    }
    if let Some(body) = &request.body {
        builder = builder.body(body.clone());
    }
    let response = builder.send().await.map_err(|error| {
        let what = if error.is_timeout() {
            format!("timed out after {}s", REQUEST_TIMEOUT.as_secs())
        } else if error.is_connect() {
            format!("cannot connect: {error}")
        } else {
            error.to_string()
        };
        Error::tool("http", what)
    })?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_ascii_lowercase(), value.to_string()))
        })
        .collect();
    let body = response
        .bytes()
        .await
        .map_err(|error| Error::tool("http", format!("cannot read response body: {error}")))?
        .to_vec();
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted transport: hands back the responses in order, repeating the last one once
    /// the script runs out. An atomic cursor rather than a lock, so the stub cannot
    /// contribute a failure mode of its own to the test it serves.
    struct Scripted {
        responses: Vec<HttpResponse>,
        calls: AtomicU32,
    }

    impl Scripted {
        fn new(responses: Vec<HttpResponse>) -> Arc<Scripted> {
            Arc::new(Scripted {
                responses,
                calls: AtomicU32::new(0),
            })
        }
    }

    impl Transport for Scripted {
        fn send(&self, _request: &HttpRequest) -> Result<HttpResponse> {
            let index = self.calls.fetch_add(1, Ordering::Relaxed) as usize;
            Ok(self.responses[index.min(self.responses.len() - 1)].clone())
        }
    }

    fn instant() -> RetryPolicy {
        RetryPolicy {
            attempts: 3,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    #[test]
    fn rate_limiting_is_retried_until_it_succeeds() {
        let transport = Scripted::new(vec![
            HttpResponse::new(429, b"slow down".to_vec()),
            HttpResponse::new(429, b"slow down".to_vec()),
            HttpResponse::new(200, br#"{"ok":true}"#.to_vec()),
        ]);
        let client = HttpClient::with_policy("fal", transport.clone(), instant());

        let response = client
            .send(HttpRequest::get("https://queue.fal.run/x"))
            .expect("third attempt succeeds");

        assert_eq!(response.status, 200);
        assert_eq!(client.retries(), 2, "two retries recorded");
        assert_eq!(transport.calls.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn a_permanent_server_error_gives_up_and_names_the_status() {
        let transport = Scripted::new(vec![HttpResponse::new(500, b"boom".to_vec())]);
        let client = HttpClient::with_policy("fal", transport.clone(), instant());

        let error = client
            .send(HttpRequest::get("https://queue.fal.run/x"))
            .expect_err("500 forever must fail");

        let message = error.to_string();
        assert!(message.contains("500"), "{message}");
        assert!(message.contains("3 attempt"), "{message}");
        assert_eq!(
            transport.calls.load(Ordering::Relaxed),
            3,
            "attempts are bounded by the policy"
        );
        assert_eq!(error.exit_code(), dvs_core::exit::TOOL_MISSING);
    }

    #[test]
    fn client_errors_are_not_retried() {
        let transport = Scripted::new(vec![HttpResponse::new(401, b"bad key".to_vec())]);
        let client = HttpClient::with_policy("fal", transport.clone(), instant());

        let error = client
            .send(HttpRequest::get("https://queue.fal.run/x"))
            .expect_err("401 must fail");

        assert!(error.to_string().contains("401"), "{error}");
        assert_eq!(transport.calls.load(Ordering::Relaxed), 1);
        assert_eq!(client.retries(), 0);
    }

    #[test]
    fn a_secret_survives_neither_debug_nor_display() {
        let config = ProviderConfig {
            keys: BTreeMap::from([(
                "fal".to_string(),
                SecretString::from("super-secret-key".to_string()),
            )]),
            source: Some(PathBuf::from("/tmp/providers.json")),
        };

        let debug = format!("{config:?}");
        let display = format!("{config}");
        let request = HttpRequest::get("https://queue.fal.run/x").with_auth(Some(Auth::new(
            "Key",
            SecretString::from("super-secret-key".to_string()),
        )));
        let request_debug = format!("{request:?}");

        for rendering in [&debug, &display, &request_debug] {
            assert!(
                !rendering.contains("super-secret-key"),
                "secret leaked: {rendering}"
            );
        }
        assert!(debug.contains("fal"), "provider names stay visible: {debug}");
        assert!(
            format!("{:?}", Auth::new("Key", SecretString::from("x".to_string())))
                .contains("REDACTED")
        );
    }

    #[test]
    fn non_http_urls_are_refused_as_bad_arguments() {
        let error = validate_url("file:///etc/passwd").expect_err("file urls are refused");
        assert_eq!(error.exit_code(), dvs_core::exit::BAD_ARGS);
        assert!(validate_url("https://fal.media/files/a.mp4").is_ok());
        assert!(validate_url("https://").is_err(), "empty host");
        assert_eq!(url_host("https://user@queue.fal.run:443/x"), Some("queue.fal.run"));
    }
}
