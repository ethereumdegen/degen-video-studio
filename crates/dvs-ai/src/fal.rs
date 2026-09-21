//! fal.ai's queue API: submit, poll, fetch, download.
//!
//! Model ids and parameters pass straight through. A crate that enumerates fal's catalog is
//! a crate that is wrong by next month — models are added and retired weekly, and their
//! parameter names differ per family — so the caller names the model and this module owns
//! only the shape of the conversation.
//!
//! What it does enforce is that a result actually contains media. The failure mode worth
//! preventing is a "successful" generation that produces an empty asset: fal answers a
//! rejected prompt or an unsupported parameter with HTTP 200 and a payload that carries a
//! `detail` instead of a URL, and importing that silently would put a zero-byte clip on the
//! timeline. [`media_ref`] refuses such a payload and names the keys it did find.
//!
//! One security property, because we hold an API key: the `Authorization` header is only
//! ever sent to a fal host. The queue response supplies the status and result URLs, so a
//! compromised or confused response could otherwise redirect the key to an attacker. Media
//! downloads go out with no credential at all.

use crate::provider::{
    url_host, validate_url, Auth, HttpClient, HttpRequest, Provider, ProviderConfig, RetryPolicy,
    Transport,
};
use dvs_core::error::{Error, Result};
use secrecy::{ExposeSecret, SecretString};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Queue endpoint root. Requests are `POST {QUEUE_BASE}/{model}`.
pub const QUEUE_BASE: &str = "https://queue.fal.run";

/// Provider name, as recorded in [`dvs_core::Provenance`].
pub const PROVIDER: &str = "fal";

/// The environment variable holding the API key.
pub const KEY_ENV: &str = "FAL_KEY";

/// Hosts the API key may be sent to.
const TRUSTED_SUFFIXES: [&str; 2] = ["fal.run", "fal.ai"];

/// How long to wait for a queued job, and how often to ask.
///
/// A text-to-video job routinely takes minutes, so the default timeout is long; it is a
/// timeout nonetheless, because an agent blocked forever on a job fal silently dropped is
/// worse than a failed op.
#[derive(Debug, Clone, Copy)]
pub struct PollSpec {
    pub interval: Duration,
    pub timeout: Duration,
}

impl Default for PollSpec {
    fn default() -> PollSpec {
        PollSpec {
            interval: Duration::from_secs(2),
            timeout: Duration::from_secs(900),
        }
    }
}

/// An accepted request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    pub request_id: String,
    pub status_url: String,
    pub response_url: String,
}

/// The media a completed request produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRef {
    pub url: String,
    pub content_type: Option<String>,
    pub file_name: Option<String>,
}

impl MediaRef {
    /// File extension to store the download under.
    ///
    /// The content type is preferred over the URL because fal's CDN serves signed URLs
    /// whose path can end in a query string or a bare id; the extension only has to be
    /// good enough for a human to recognise the file, since ffprobe sniffs the content.
    pub fn extension(&self) -> String {
        if let Some(from_type) = self
            .content_type
            .as_deref()
            .and_then(|value| extension_for_type(value.split(';').next().unwrap_or(value).trim()))
        {
            return from_type.to_string();
        }
        let path = self
            .file_name
            .as_deref()
            .unwrap_or_else(|| self.url.split(['?', '#']).next().unwrap_or(&self.url));
        path.rsplit('/')
            .next()
            .and_then(|name| name.rsplit_once('.'))
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .filter(|ext| !ext.is_empty() && ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or_else(|| "bin".to_string())
    }
}

fn extension_for_type(content_type: &str) -> Option<&'static str> {
    match content_type {
        "video/mp4" => Some("mp4"),
        "video/webm" => Some("webm"),
        "video/quicktime" => Some("mov"),
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "audio/wav" | "audio/x-wav" | "audio/wave" => Some("wav"),
        "audio/mpeg" | "audio/mp3" => Some("mp3"),
        "audio/mp4" | "audio/x-m4a" => Some("m4a"),
        "audio/flac" => Some("flac"),
        "audio/ogg" => Some("ogg"),
        _ => None,
    }
}

/// A completed request: everything the caller needs to cache and import the result.
#[derive(Debug, Clone)]
pub struct FalOutput {
    pub payload: serde_json::Value,
    pub media: MediaRef,
    /// What fal said it charged, when it says anything. Most models report nothing, in
    /// which case the caller's estimate stands as the recorded cost.
    pub cost_usd: Option<f64>,
}

/// The fal.ai provider.
pub struct Fal {
    client: HttpClient,
    key: Option<SecretString>,
    poll: PollSpec,
}

impl std::fmt::Debug for Fal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fal")
            .field("available", &self.key.is_some())
            .field("poll", &self.poll)
            .finish_non_exhaustive()
    }
}

impl Provider for Fal {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    fn key_env(&self) -> &'static str {
        KEY_ENV
    }

    fn available(&self) -> bool {
        self.key.is_some()
    }
}

impl Fal {
    /// Construction never fails on a missing key: a project with no credentials must still
    /// load the op catalog, and the op reports the missing variable when it is actually
    /// asked to spend money.
    pub fn new(transport: Arc<dyn Transport>, config: &ProviderConfig) -> Fal {
        Fal::with_policy(transport, config, RetryPolicy::default())
    }

    pub fn with_policy(
        transport: Arc<dyn Transport>,
        config: &ProviderConfig,
        policy: RetryPolicy,
    ) -> Fal {
        Fal {
            client: HttpClient::with_policy(PROVIDER, transport, policy),
            key: config.key(PROVIDER, KEY_ENV),
            poll: PollSpec::default(),
        }
    }

    pub fn with_poll(mut self, poll: PollSpec) -> Fal {
        self.poll = poll;
        self
    }

    /// Attempts beyond the first, for the op's report.
    pub fn retries(&self) -> u32 {
        self.client.retries()
    }

    /// The credential, or the instruction for supplying it. Cloning the secret per call
    /// keeps it inside `SecretString` everywhere except the header assembly in the
    /// transport.
    pub fn auth(&self) -> Result<Auth> {
        let key = self
            .key
            .as_ref()
            .ok_or_else(|| crate::provider::missing_key(PROVIDER, KEY_ENV))?;
        Ok(Auth::new(
            "Key",
            SecretString::from(key.expose_secret().to_string()),
        ))
    }

    /// Submit a request to the queue.
    pub fn submit(&self, model: &str, params: &serde_json::Value) -> Result<Submission> {
        let model = check_model(model)?;
        let url = format!("{QUEUE_BASE}/{model}");
        let request = HttpRequest::post_json(&url, params)?.with_auth(Some(self.auth()?));
        let payload = self.client.send(request)?.json()?;
        let request_id = payload
            .get("request_id")
            .or_else(|| payload.get("requestId"))
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                Error::tool(
                    PROVIDER,
                    format!(
                        "queue accepted '{model}' but returned no request_id: {}",
                        summarize(&payload)
                    ),
                )
            })?
            .to_string();
        let fallback_status = format!("{QUEUE_BASE}/{model}/requests/{request_id}/status");
        let fallback_result = format!("{QUEUE_BASE}/{model}/requests/{request_id}");
        Ok(Submission {
            status_url: trusted_url(payload.get("status_url"), &fallback_status),
            response_url: trusted_url(payload.get("response_url"), &fallback_result),
            request_id,
        })
    }

    /// Wait for a submission to complete and return its result payload.
    pub fn wait(&self, submission: &Submission) -> Result<serde_json::Value> {
        let deadline = Instant::now() + self.poll.timeout;
        loop {
            let status = self
                .client
                .send(HttpRequest::get(&submission.status_url).with_auth(Some(self.auth()?)))?
                .json()?;
            let state = status
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("UNKNOWN");
            match state {
                "COMPLETED" => break,
                // `UNKNOWN` covers a status shape we do not recognise: treated as "still
                // working" rather than fatal, because the timeout below bounds it anyway
                // and fal has changed this field's vocabulary before.
                "IN_QUEUE" | "IN_PROGRESS" | "UNKNOWN" => {}
                other => {
                    return Err(Error::tool(
                        PROVIDER,
                        format!(
                            "request {} reported status '{other}': {}",
                            submission.request_id,
                            summarize(&status)
                        ),
                    ))
                }
            }
            if Instant::now() >= deadline {
                return Err(Error::tool(
                    PROVIDER,
                    format!(
                        "request {} was still '{state}' after {}s; it may still finish — \
                         re-run the op to pick up the result, or raise the wait",
                        submission.request_id,
                        self.poll.timeout.as_secs()
                    ),
                ));
            }
            std::thread::sleep(self.poll.interval);
        }
        self.client
            .send(HttpRequest::get(&submission.response_url).with_auth(Some(self.auth()?)))?
            .json()
    }

    /// Submit, wait, and validate that there is media to download.
    pub fn run(&self, model: &str, params: &serde_json::Value) -> Result<FalOutput> {
        let submission = self.submit(model, params)?;
        let payload = self.wait(&submission)?;
        let media = media_ref(&payload)?;
        Ok(FalOutput {
            cost_usd: reported_cost(&payload),
            media,
            payload,
        })
    }

    /// Fetch the produced file, recording what the server said it is.
    ///
    /// No credential goes out with this: the CDN URL is already signed, and a media host is
    /// not a host we trust with the key. `media` is borrowed mutably so a payload that
    /// omitted `content_type` — which happens, and leaves a signed URL with no usable
    /// extension — still yields a recognisable file name from the response header.
    pub fn download(&self, media: &mut MediaRef) -> Result<Vec<u8>> {
        validate_url(&media.url)?;
        let response = self.client.send(HttpRequest::get(&media.url))?;
        if response.body.is_empty() {
            return Err(Error::tool(
                PROVIDER,
                format!("download of {} returned no bytes", media.url),
            ));
        }
        if media.content_type.is_none() {
            media.content_type = response.header("content-type").map(str::to_string);
        }
        Ok(response.body)
    }
}

/// A model id must be a path segment sequence, not a URL and not a traversal: it is
/// interpolated into the queue URL.
fn check_model(model: &str) -> Result<&str> {
    let trimmed = model.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Err(Error::bad_args(
            "empty model id; pass a fal catalog id such as 'fal-ai/flux/schnell'",
        ));
    }
    if trimmed.contains("://") || trimmed.contains("..") || trimmed.contains(char::is_whitespace) {
        return Err(Error::bad_args(format!(
            "'{model}' is not a fal model id; expected something like 'fal-ai/flux/schnell'"
        )));
    }
    Ok(trimmed)
}

/// Use the URL fal returned when it points at fal, else the one we derived. Anything else
/// would mean sending the API key wherever a response says.
fn trusted_url(candidate: Option<&serde_json::Value>, fallback: &str) -> String {
    let Some(url) = candidate.and_then(|value| value.as_str()) else {
        return fallback.to_string();
    };
    if validate_url(url).is_ok() && url_host(url).is_some_and(is_trusted_host) {
        url.to_string()
    } else {
        fallback.to_string()
    }
}

fn is_trusted_host(host: &str) -> bool {
    TRUSTED_SUFFIXES
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

/// The media URL in a result payload.
///
/// fal's output shape is per-model: `{"video": {"url": …}}`, `{"images": [{"url": …}]}`,
/// `{"audio_url": {"url": …}}`, sometimes a bare `{"url": …}`. Rather than a table of
/// models, this walks the known containers first and then searches the payload for any
/// object carrying a `url` string — deterministic, because `serde_json` preserves the
/// response's key order in this workspace.
pub fn media_ref(payload: &serde_json::Value) -> Result<MediaRef> {
    const CONTAINERS: [&str; 10] = [
        "video", "audio", "image", "file", "output", "videos", "audios", "images", "files",
        "outputs",
    ];
    for key in CONTAINERS {
        let Some(value) = payload.get(key) else {
            continue;
        };
        let candidate = match value {
            serde_json::Value::Array(items) => items.first(),
            other => Some(other),
        };
        if let Some(found) = candidate.and_then(as_media) {
            return Ok(found);
        }
    }
    if let Some(found) = as_media(payload) {
        return Ok(found);
    }
    if let Some(found) = search(payload, 0) {
        return Ok(found);
    }
    Err(Error::tool(
        PROVIDER,
        format!(
            "the result carries no media url, so there is nothing to import: {}",
            summarize(payload)
        ),
    ))
}

/// An object with a usable `url`, or a bare URL string.
fn as_media(value: &serde_json::Value) -> Option<MediaRef> {
    match value {
        serde_json::Value::String(url) => validate_url(url).ok().map(|()| MediaRef {
            url: url.clone(),
            content_type: None,
            file_name: None,
        }),
        serde_json::Value::Object(map) => {
            let url = map.get("url")?.as_str()?;
            validate_url(url).ok()?;
            Some(MediaRef {
                url: url.to_string(),
                content_type: map
                    .get("content_type")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                file_name: map
                    .get("file_name")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
            })
        }
        _ => None,
    }
}

/// Depth-limited search for media anywhere in the payload. The limit keeps a pathological
/// response from turning into deep recursion.
fn search(value: &serde_json::Value, depth: usize) -> Option<MediaRef> {
    if depth > 6 {
        return None;
    }
    match value {
        serde_json::Value::Object(map) => {
            if let Some(found) = as_media(value) {
                return Some(found);
            }
            map.values().find_map(|child| search(child, depth + 1))
        }
        serde_json::Value::Array(items) => items.iter().find_map(|item| search(item, depth + 1)),
        _ => None,
    }
}

/// A cost fal reported, if it reported one.
pub fn reported_cost(payload: &serde_json::Value) -> Option<f64> {
    const PATHS: [&[&str]; 4] = [
        &["cost_usd"],
        &["cost"],
        &["metrics", "cost"],
        &["metrics", "cost_usd"],
    ];
    PATHS.iter().find_map(|path| {
        path.iter()
            .try_fold(payload, |cursor, key| cursor.get(key))
            .and_then(serde_json::Value::as_f64)
            .filter(|cost| cost.is_finite() && *cost >= 0.0)
    })
}

/// A payload rendered for an error message: keys at the top level, plus any `detail` or
/// `message` the provider put there, capped in length.
fn summarize(payload: &serde_json::Value) -> String {
    let complaint = ["detail", "message", "error"]
        .iter()
        .filter_map(|key| payload.get(*key))
        .map(|value| value.to_string())
        .next();
    let keys = match payload.as_object() {
        Some(map) => map.keys().cloned().collect::<Vec<_>>().join(", "),
        None => payload.to_string(),
    };
    let mut text = match complaint {
        Some(detail) => format!("keys [{keys}], detail {detail}"),
        None => format!("keys [{keys}]"),
    };
    if text.chars().count() > 400 {
        text = text.chars().take(400).collect::<String>() + "…";
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, json_response, Forbidden, Routed};
    use std::sync::atomic::Ordering;

    #[test]
    fn a_missing_key_names_the_variable_and_sends_nothing() {
        let _env = testkit::isolate();
        let fal = Fal::new(Arc::new(Forbidden), &ProviderConfig::empty());

        assert!(!fal.available());
        let error = fal
            .run("fal-ai/flux/schnell", &serde_json::json!({ "prompt": "x" }))
            .expect_err("no key, no request");

        assert_eq!(error.exit_code(), dvs_core::exit::TOOL_MISSING);
        assert!(error.to_string().contains(KEY_ENV), "{error}");
    }

    #[test]
    fn a_submission_polls_until_completed_and_extracts_the_media() {
        let _env = testkit::isolate();
        let transport = Routed::new(vec![
            (
                "/status",
                json_response(serde_json::json!({ "status": "COMPLETED" })),
            ),
            (
                "/requests/req-1",
                json_response(serde_json::json!({
                    "video": { "url": "https://fal.media/files/out.mp4", "content_type": "video/mp4" }
                })),
            ),
            (
                "queue.fal.run/fal-ai/kling",
                json_response(serde_json::json!({ "request_id": "req-1" })),
            ),
        ]);
        std::env::set_var(KEY_ENV, "test-key");
        let fal = Fal::new(transport.clone(), &ProviderConfig::empty()).with_poll(PollSpec {
            interval: Duration::ZERO,
            timeout: Duration::from_secs(1),
        });

        let output = fal
            .run("fal-ai/kling", &serde_json::json!({ "prompt": "a cat" }))
            .expect("scripted run completes");

        assert_eq!(output.media.url, "https://fal.media/files/out.mp4");
        assert_eq!(output.media.extension(), "mp4");
        assert_eq!(transport.calls.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn a_result_without_media_fails_loudly_and_quotes_the_payload() {
        let error = media_ref(&serde_json::json!({
            "detail": "prompt rejected by the safety checker",
            "seed": 4
        }))
        .expect_err("a payload with no url must not import");

        let message = error.to_string();
        assert!(message.contains("safety checker"), "{message}");
        assert!(message.contains("seed"), "{message}");
        assert_eq!(error.exit_code(), dvs_core::exit::TOOL_MISSING);
    }

    #[test]
    fn media_is_found_in_every_shape_fal_actually_returns() {
        let cases = [
            serde_json::json!({ "video": { "url": "https://f.media/a.mp4" } }),
            serde_json::json!({ "images": [{ "url": "https://f.media/a.mp4" }] }),
            serde_json::json!({ "url": "https://f.media/a.mp4" }),
            serde_json::json!({ "audio": "https://f.media/a.mp4" }),
            serde_json::json!({ "data": { "nested": { "url": "https://f.media/a.mp4" } } }),
        ];
        for payload in cases {
            let found = media_ref(&payload).unwrap_or_else(|e| panic!("{payload}: {e}"));
            assert_eq!(found.url, "https://f.media/a.mp4");
        }
    }

    #[test]
    fn a_data_url_in_a_response_is_not_treated_as_media() {
        let error = media_ref(&serde_json::json!({
            "image": { "url": "data:image/png;base64,iVBORw0KGgo=" }
        }))
        .expect_err("only http(s) downloads are followed");
        assert!(error.to_string().contains("no media url"), "{error}");
    }

    #[test]
    fn poll_urls_off_a_fal_host_are_replaced_by_derived_ones() {
        assert_eq!(
            trusted_url(
                Some(&serde_json::json!("https://evil.example/steal")),
                "https://queue.fal.run/m/requests/1/status"
            ),
            "https://queue.fal.run/m/requests/1/status",
            "the api key must never be sent to a host the response chose"
        );
        assert_eq!(
            trusted_url(
                Some(&serde_json::json!("https://queue.fal.run/m/requests/1/status")),
                "fallback"
            ),
            "https://queue.fal.run/m/requests/1/status"
        );
        assert!(is_trusted_host("queue.fal.run"));
        assert!(!is_trusted_host("notfal.run.example.com"));
    }

    #[test]
    fn a_model_id_cannot_smuggle_a_url_or_a_traversal() {
        assert_eq!(check_model(" fal-ai/flux/schnell ").unwrap(), "fal-ai/flux/schnell");
        for bad in ["", "  ", "https://evil.example/x", "../../admin", "two words"] {
            let error = check_model(bad).expect_err("rejected");
            assert_eq!(error.exit_code(), dvs_core::exit::BAD_ARGS, "{bad}");
        }
    }

    #[test]
    fn a_reported_cost_is_preferred_over_nothing() {
        assert_eq!(
            reported_cost(&serde_json::json!({ "metrics": { "cost": 0.37 } })),
            Some(0.37)
        );
        assert_eq!(reported_cost(&serde_json::json!({ "cost": -1 })), None);
        assert_eq!(reported_cost(&serde_json::json!({ "video": {} })), None);
    }

    #[test]
    fn an_extension_comes_from_the_content_type_before_the_url() {
        let signed = MediaRef {
            url: "https://fal.media/files/abc?token=1".into(),
            content_type: Some("audio/wav; charset=binary".into()),
            file_name: None,
        };
        assert_eq!(signed.extension(), "wav");

        let bare = MediaRef {
            url: "https://fal.media/files/abc.webm?token=1".into(),
            content_type: None,
            file_name: None,
        };
        assert_eq!(bare.extension(), "webm");

        let unknown = MediaRef {
            url: "https://fal.media/files/abc".into(),
            content_type: Some("application/octet-stream".into()),
            file_name: None,
        };
        assert_eq!(unknown.extension(), "bin");
    }
}
