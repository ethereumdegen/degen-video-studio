//! The `ai.*` ops: the only surface the rest of the system sees.
//!
//! Each generation op runs the same five steps in the same order, and the order is the
//! safety property:
//!
//! 1. **Cache first.** A repeat of an identical request is served from
//!    `cache/ai/<key>/` without a credential, without a network call and without a charge.
//!    An agent that re-applies a batch pays once, not once per iteration.
//! 2. **Credential second.** A missing key is reported as a tool error naming the variable,
//!    before anything is estimated or sent.
//! 3. **Budget third.** The estimated cost is checked against the project ceiling and the
//!    request is refused with exit code 6 if it would cross it. Nothing has been sent yet.
//! 4. **Dry run stops here**, reporting the estimate and the remaining budget.
//! 5. **Send, cache, record, import.** The charge is recorded only after the provider
//!    actually produced a file, and the file enters the document as an ordinary hashed
//!    asset with provenance — never as a URL.
//!
//! `ai.budget` is a query op so an agent can ask what it has left before planning a batch.

use crate::budget::{self, Budget, Kind};
use crate::cache::{AiCache, CachedRequest};
use crate::fal::{self, Fal, PollSpec};
use crate::provider::{default_transport, Provider, ProviderConfig, Transport};
use crate::tts::{self, Engine, Piper};
use crate::{import_generated, Generated};
use chrono::Utc;
use dvs_core::error::{Error, Result};
use dvs_core::ids::SequenceId;
use dvs_core::op::{args, Op, OpCx, OpEffect, Registry};
use dvs_core::ops::util::{assert_free, assert_takes_clips, assert_unlocked, sequence_fps};
use dvs_core::project::{Clip, Project, Source, Track, TrackKind};
use dvs_core::selector;
use dvs_core::time::{Span, Time};
use std::sync::Arc;

/// Model ids used when the caller does not name one.
///
/// The fal catalog changes weekly, so these are conveniences, not contracts: every op takes
/// `--model`, the id is passed through untouched, and a retired default fails with the
/// queue URL — which contains the model id — in the error. Their schema entries say so.
pub const DEFAULT_VIDEO_MODEL: &str = "fal-ai/kling-video/v1/standard/text-to-video";
pub const DEFAULT_IMAGE_MODEL: &str = "fal-ai/flux/schnell";
pub const DEFAULT_SPEECH_MODEL: &str = "fal-ai/kokoro/american-english";

/// Clip length for a still image placed on the timeline, when none is given.
const DEFAULT_IMAGE_SECONDS: i64 = 4;

/// Duration assumed for a video generation when the caller does not say, both for the
/// request and for the cost estimate.
const DEFAULT_VIDEO_SECONDS: f64 = 5.0;

/// Register the `ai.*` ops with the real HTTP transport.
pub fn register(registry: &mut Registry) {
    register_with(registry, default_transport());
}

/// Register with an injected transport.
///
/// This exists because the alternative — a global client — makes it impossible to prove
/// that the cache, the dry run and the budget refusal send nothing. It is a design
/// requirement rather than test scaffolding: the tests hand in a transport that panics.
pub fn register_with(registry: &mut Registry, transport: Arc<dyn Transport>) {
    registry.register(GenerateVideo {
        transport: Arc::clone(&transport),
    });
    registry.register(GenerateImage {
        transport: Arc::clone(&transport),
    });
    registry.register(Tts { transport });
    registry.register(BudgetQuery);
}

/// One request, fully described before anything is sent.
struct Request<'a> {
    kind: Kind,
    provider: &'a str,
    model: String,
    params: serde_json::Value,
    prompt: Option<String>,
    seed: Option<i64>,
    /// Units the estimate is priced in: seconds, images, or thousands of characters.
    units: f64,
    /// Extension to store the result under when the producer is local.
    local_extension: &'static str,
}

/// What the op reports back, alongside the document changes.
struct Outcome {
    generated: Option<Generated>,
    data: serde_json::Map<String, serde_json::Value>,
}

/// Steps 1–5 for a hosted (fal) request.
fn run_hosted(cx: &OpCx, transport: &Arc<dyn Transport>, request: &Request) -> Result<Outcome> {
    let cache = AiCache::new(cx.paths, Arc::clone(cx.assets.vfs()));
    let key = crate::request_key(request.provider, &request.model, &request.params);
    let budget = Budget::new(cx.paths, Arc::clone(cx.assets.vfs()));

    if let Some(hit) = cache.lookup(&key)? {
        return Ok(Outcome {
            data: report(request, &key, 0.0, 0.0, true, 0, &budget.load()?, None),
            generated: Some(Generated {
                provider: request.provider.to_string(),
                model: request.model.clone(),
                prompt: request.prompt.clone(),
                seed: request.seed,
                cost_usd: 0.0,
                cached: true,
                source_url: hit.record.source_url.clone(),
                media: hit.media,
            }),
        });
    }

    let config = ProviderConfig::load()?;
    let fal = Fal::new(Arc::clone(transport), &config).with_poll(PollSpec::default());
    // Ask for the credential before estimating: "no key" is a more useful answer than "you
    // cannot afford it", and it is the one an unconfigured machine always gets.
    fal.auth()?;

    let ledger = budget.load()?;
    let estimate = ledger.estimate(request.kind, &request.model, request.units);
    let ledger = budget.check(estimate)?;
    if cx.dry_run {
        return Ok(Outcome {
            generated: None,
            data: report(request, &key, estimate, estimate, false, 0, &ledger, None),
        });
    }

    let mut output = fal.run(&request.model, &request.params)?;
    let bytes = fal.download(&mut output.media)?;
    let cost = output.cost_usd.unwrap_or(estimate);
    let record = CachedRequest {
        provider: request.provider.to_string(),
        model: request.model.clone(),
        params: request.params.clone(),
        media: format!("media.{}", output.media.extension()),
        source_url: Some(output.media.url.clone()),
        cost_usd: cost,
        created: Utc::now(),
    };
    let media = cache.store(&key, &record, &bytes)?;
    // Recorded after the provider did the work: a failed request costs nothing, and a
    // cached one is never charged twice.
    let ledger = budget.record(request.provider, &request.model, cost)?;
    Ok(Outcome {
        data: report(
            request,
            &key,
            cost,
            estimate,
            false,
            fal.retries(),
            &ledger,
            Some(&output.media.url),
        ),
        generated: Some(Generated {
            provider: request.provider.to_string(),
            model: request.model.clone(),
            prompt: request.prompt.clone(),
            seed: request.seed,
            cost_usd: cost,
            cached: false,
            media,
            source_url: Some(output.media.url),
        }),
    })
}

/// The same steps for the local TTS engine, minus the two that involve money.
///
/// A local synthesis is free, so nothing is charged and no ledger entry is written — the
/// ledger stays a record of money actually spent rather than a log of generations. It is
/// still cached, because re-running piper over a paragraph costs seconds an agent loop
/// does not have to spend either. The ledger is read only so the report can show the same
/// budget fields as a hosted request.
fn run_local_speech(cx: &OpCx, request: &Request, text: &str, voice: Option<&str>) -> Result<Outcome> {
    let cache = AiCache::new(cx.paths, Arc::clone(cx.assets.vfs()));
    let key = crate::request_key(request.provider, &request.model, &request.params);
    let budget = Budget::new(cx.paths, Arc::clone(cx.assets.vfs()));

    if let Some(hit) = cache.lookup(&key)? {
        return Ok(Outcome {
            data: report(request, &key, 0.0, 0.0, true, 0, &budget.load()?, None),
            generated: Some(Generated {
                provider: request.provider.to_string(),
                // The record holds the `.onnx` piper actually used, which is what the
                // first run wrote into provenance; the request only carries the voice name.
                model: hit.record.model.clone(),
                prompt: Some(text.to_string()),
                seed: None,
                cost_usd: 0.0,
                cached: true,
                source_url: None,
                media: hit.media,
            }),
        });
    }

    let piper = Piper::discover();
    piper.binary()?;
    let ledger = budget.load()?;
    if cx.dry_run {
        return Ok(Outcome {
            generated: None,
            data: report(request, &key, 0.0, 0.0, false, 0, &ledger, None),
        });
    }

    let media = cache.prepare(&key, &format!("media.{}", request.local_extension))?;
    let model = piper.speak(text, voice, &media)?;
    let record = CachedRequest {
        provider: request.provider.to_string(),
        model: model.display().to_string(),
        params: request.params.clone(),
        media: media
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("media.{}", request.local_extension)),
        source_url: None,
        cost_usd: 0.0,
        created: Utc::now(),
    };
    cache.commit(&key, &record)?;
    Ok(Outcome {
        data: report(request, &key, 0.0, 0.0, false, 0, &ledger, None),
        generated: Some(Generated {
            provider: request.provider.to_string(),
            model: record.model.clone(),
            prompt: Some(text.to_string()),
            seed: None,
            cost_usd: 0.0,
            cached: false,
            media,
            source_url: None,
        }),
    })
}

/// The JSON an agent reads after a generation. Money is always reported, including the zero
/// of a cache hit, so a loop can see that its repeats are free.
#[allow(clippy::too_many_arguments)]
fn report(
    request: &Request,
    key: &str,
    cost: f64,
    estimate: f64,
    cached: bool,
    retries: u32,
    ledger: &budget::Ledger,
    source_url: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut data = serde_json::Map::new();
    data.insert("provider".into(), request.provider.into());
    data.insert("model".into(), request.model.clone().into());
    data.insert("kind".into(), request.kind.as_str().into());
    data.insert("cached".into(), cached.into());
    data.insert("costUsd".into(), cost.into());
    data.insert("estimateUsd".into(), estimate.into());
    // An unexplained number invites an agent to ignore it: saying "5 × second" next to the
    // estimate makes the pre-flight check auditable against the provider's price list.
    data.insert("units".into(), request.units.into());
    data.insert("unit".into(), request.kind.unit().into());
    data.insert("ceilingUsd".into(), ledger.ceiling_usd.into());
    data.insert("spentUsd".into(), ledger.spent_usd.into());
    data.insert("remainingUsd".into(), ledger.remaining().into());
    data.insert("retries".into(), retries.into());
    data.insert("cacheKey".into(), key.into());
    if let Some(url) = source_url {
        data.insert("sourceUrl".into(), url.into());
    }
    data
}

/// Put a generated asset on the timeline.
///
/// With no `track`, the first track of the right kind wins and an empty sequence gets one:
/// refusing to place a generation because the project has no tracks yet would fail the
/// first thing an agent does with a fresh project.
#[allow(clippy::too_many_arguments)]
fn place(
    project: &mut Project,
    seq: &SequenceId,
    source: Source,
    label: &str,
    at: Time,
    duration: Time,
    kind: TrackKind,
    track: Option<&str>,
    mut effect: OpEffect,
) -> Result<OpEffect> {
    let target = match track {
        Some(text) => selector::resolve_track(project, seq, text)?,
        None => {
            let existing = project
                .sequence(seq)?
                .tracks
                .iter()
                .find(|track| track.kind == kind)
                .map(|track| track.id.clone());
            match existing {
                Some(id) => id,
                None => {
                    let sequence = project.sequence_mut(seq)?;
                    let track = Track::new(sequence.next_track_name(kind), kind);
                    let id = track.id.clone();
                    sequence.tracks.push(track);
                    effect = effect.created(&id);
                    id
                }
            }
        }
    };

    let track = project.sequence_mut(seq)?.track_mut(&target)?;
    assert_unlocked(track)?;
    assert_takes_clips(track)?;
    if track.kind != kind {
        return Err(Error::op(format!(
            "track '{}' is {:?}; a generated {} clip belongs on a {:?} track",
            track.name,
            track.kind,
            label,
            kind
        )));
    }
    let span = Span::from_duration(at, duration);
    assert_free(track, span, None)?;

    let mut clip = Clip::new(source, at, duration);
    clip.name = Some(label.to_string());
    let clip_id = clip.id.clone();
    track.place(clip);
    Ok(effect.created(&clip_id).changed(&target))
}

/// Import the generation and, when `at` is given, place it. Shared by all three generation
/// ops so the document side of "a generation is a normal asset" has exactly one
/// implementation.
fn absorb(
    project: &mut Project,
    cx: &mut OpCx,
    outcome: Outcome,
    at: Option<Time>,
    duration: Option<Time>,
    track: Option<&str>,
    name: Option<&str>,
) -> Result<OpEffect> {
    let mut effect = OpEffect::new();
    // A dry run stops here even when the cache could have answered. The engine throws away
    // document changes for a dry run, but the asset store is not part of the document:
    // importing would leave a real blob under `assets/` for an edit that never happened.
    let Some(generated) = outcome.generated.filter(|_| !cx.dry_run) else {
        let mut data = outcome.data;
        data.insert("dryRun".into(), true.into());
        data.insert("sent".into(), false.into());
        return Ok(effect.data(serde_json::Value::Object(data)));
    };

    let tool = dvs_media::toolchain::Toolchain::shared()?;
    let asset_id = import_generated(project, cx, tool, &generated, name)?;
    effect = effect.created(&asset_id);
    let mut data = outcome.data;
    data.insert("asset".into(), asset_id.to_string().into());

    if let Some(at) = at {
        let seq = cx.sequence(project)?;
        let fps = sequence_fps(project, &seq)?;
        let asset = project.asset(&asset_id)?;
        let is_audio = asset.probe.video.is_none();
        let intrinsic = asset.probe.duration;
        let label = asset.name.clone();
        let source = Source::Asset {
            asset: asset_id.clone(),
            stream: None,
        };
        // A still has no duration of its own, and a generated clip's natural length is
        // whatever the provider produced; an explicit `duration` overrides both.
        let length = duration
            .or_else(|| intrinsic.is_positive().then_some(intrinsic))
            .unwrap_or_else(|| Time::from_secs(DEFAULT_IMAGE_SECONDS));
        let snapped_at = at.snap(fps);
        let snapped_length = Time::from_frames(length.frame_ceil(fps).max(1), fps);
        let kind = if is_audio {
            TrackKind::Audio
        } else {
            TrackKind::Video
        };
        effect = place(
            project,
            &seq,
            source,
            &label,
            snapped_at,
            snapped_length,
            kind,
            track,
            effect,
        )?;
        effect = effect
            .snap("at", at, snapped_at, fps)
            .snap("duration", length, snapped_length, fps);
    }
    Ok(effect.data(serde_json::Value::Object(data)))
}

/// `--model`, or the documented default.
fn model_of(args: &serde_json::Value, default: &str) -> Result<String> {
    match args::opt_str(args, "model").map(str::trim) {
        Some("") => Err(Error::bad_args(
            "empty --model; omit it to use the default or pass a fal catalog id",
        )),
        Some(model) => Ok(model.to_string()),
        None => Ok(default.to_string()),
    }
}

struct GenerateVideo {
    transport: Arc<dyn Transport>,
}

impl Op for GenerateVideo {
    fn id(&self) -> &'static str {
        "ai.generate-video"
    }

    fn about(&self) -> &'static str {
        "Generate a video with a hosted model and import it as a normal asset"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["prompt"],
            "properties": {
                "prompt": { "type": "string", "description": "What to generate" },
                "model": {
                    "type": "string",
                    "description": "fal catalog id; the catalog changes often, so pass one \
                                    explicitly if the default has been retired",
                    "default": DEFAULT_VIDEO_MODEL
                },
                "duration": { "type": "string", "description": "Requested length, e.g. '5s'" },
                "seed": { "type": "integer", "description": "Seed, for a repeatable generation" },
                "at": { "type": "string", "description": "Place the result as a clip at this time" },
                "track": { "type": "string", "description": "Track to place it on; defaults to the first video track" },
                "name": { "type": "string", "description": "Asset name; defaults to a slug of the prompt" }
            },
            "additionalProperties": false
        })
    }

    fn is_network(&self) -> bool {
        true
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        &["ffprobe"]
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(
            &args,
            &["prompt", "model", "duration", "seed", "at", "track", "name"],
        )?;
        let prompt = args::str_field(&args, "prompt")?.trim().to_string();
        if prompt.is_empty() {
            return Err(Error::bad_args("empty prompt"));
        }
        let model = model_of(&args, DEFAULT_VIDEO_MODEL)?;
        let seq = cx.sequence(project)?;
        let fps = sequence_fps(project, &seq)?;
        let duration = args::opt_time(&args, "duration", fps)?;
        let seconds = duration
            .map(|time| time.as_secs_f64())
            .filter(|seconds| *seconds > 0.0)
            .unwrap_or(DEFAULT_VIDEO_SECONDS);
        let seed = args::opt_f64(&args, "seed")?.map(|seed| seed as i64);

        let mut params = serde_json::Map::new();
        params.insert("prompt".into(), prompt.clone().into());
        params.insert("duration".into(), seconds.into());
        if let Some(seed) = seed {
            params.insert("seed".into(), seed.into());
        }
        let request = Request {
            kind: Kind::Video,
            provider: fal::PROVIDER,
            model,
            params: serde_json::Value::Object(params),
            prompt: Some(prompt),
            seed,
            units: seconds,
            local_extension: "mp4",
        };

        let outcome = run_hosted(cx, &self.transport, &request)?;
        let at = args::opt_time(&args, "at", fps)?;
        absorb(
            project,
            cx,
            outcome,
            at,
            duration,
            args::opt_str(&args, "track"),
            args::opt_str(&args, "name"),
        )
    }
}

struct GenerateImage {
    transport: Arc<dyn Transport>,
}

impl Op for GenerateImage {
    fn id(&self) -> &'static str {
        "ai.generate-image"
    }

    fn about(&self) -> &'static str {
        "Generate a still with a hosted model and import it as a normal asset"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["prompt"],
            "properties": {
                "prompt": { "type": "string", "description": "What to generate" },
                "model": {
                    "type": "string",
                    "description": "fal catalog id; pass one explicitly if the default has been retired",
                    "default": DEFAULT_IMAGE_MODEL
                },
                "size": {
                    "type": "string",
                    "description": "WxH, or a model size name such as 'landscape_16_9'; \
                                    defaults to the sequence size"
                },
                "seed": { "type": "integer", "description": "Seed, for a repeatable generation" },
                "at": { "type": "string", "description": "Place the result as a clip at this time" },
                "duration": { "type": "string", "description": "Clip length when placing; defaults to 4s" },
                "track": { "type": "string", "description": "Track to place it on; defaults to the first video track" },
                "name": { "type": "string", "description": "Asset name; defaults to a slug of the prompt" }
            },
            "additionalProperties": false
        })
    }

    fn is_network(&self) -> bool {
        true
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        &["ffprobe"]
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(
            &args,
            &[
                "prompt", "model", "size", "seed", "at", "duration", "track", "name",
            ],
        )?;
        let prompt = args::str_field(&args, "prompt")?.trim().to_string();
        if prompt.is_empty() {
            return Err(Error::bad_args("empty prompt"));
        }
        let model = model_of(&args, DEFAULT_IMAGE_MODEL)?;
        let seq = cx.sequence(project)?;
        let fps = sequence_fps(project, &seq)?;
        let sequence_size = project.sequence(&seq)?.size;

        let mut params = serde_json::Map::new();
        params.insert("prompt".into(), prompt.clone().into());
        match args::opt_str(&args, "size").map(str::trim) {
            None | Some("") => {
                params.insert(
                    "image_size".into(),
                    serde_json::json!({ "width": sequence_size[0], "height": sequence_size[1] }),
                );
            }
            Some(size) => match parse_size(size) {
                Some([width, height]) => {
                    params.insert(
                        "image_size".into(),
                        serde_json::json!({ "width": width, "height": height }),
                    );
                }
                // A model-specific size name (`square_hd`, `landscape_16_9`) is passed
                // through: enumerating them here would be one more thing to go stale.
                None => {
                    params.insert("image_size".into(), size.into());
                }
            },
        }
        let seed = args::opt_f64(&args, "seed")?.map(|seed| seed as i64);
        if let Some(seed) = seed {
            params.insert("seed".into(), seed.into());
        }
        let request = Request {
            kind: Kind::Image,
            provider: fal::PROVIDER,
            model,
            params: serde_json::Value::Object(params),
            prompt: Some(prompt),
            seed,
            units: 1.0,
            local_extension: "png",
        };

        let outcome = run_hosted(cx, &self.transport, &request)?;
        let at = args::opt_time(&args, "at", fps)?;
        let duration = args::opt_time(&args, "duration", fps)?;
        absorb(
            project,
            cx,
            outcome,
            at,
            duration,
            args::opt_str(&args, "track"),
            args::opt_str(&args, "name"),
        )
    }
}

/// `1920x1080`, in either case, with or without spaces.
fn parse_size(text: &str) -> Option<[u32; 2]> {
    let lowered = text.to_ascii_lowercase();
    let (width, height) = lowered.split_once('x')?;
    let width: u32 = width.trim().parse().ok()?;
    let height: u32 = height.trim().parse().ok()?;
    (width > 0 && height > 0).then_some([width, height])
}

struct Tts {
    transport: Arc<dyn Transport>,
}

impl Op for Tts {
    fn id(&self) -> &'static str {
        "ai.tts"
    }

    fn about(&self) -> &'static str {
        "Synthesize speech and import it as a normal audio asset"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["text"],
            "properties": {
                "text": { "type": "string", "description": "What to say" },
                "voice": {
                    "type": "string",
                    "description": "Provider voice id, or for --engine piper an .onnx model path or name"
                },
                "engine": {
                    "type": "string",
                    "enum": ["fal", "piper"],
                    "default": "fal",
                    "description": "'piper' shells out to a local install and needs no key"
                },
                "model": {
                    "type": "string",
                    "description": "fal catalog id for --engine fal",
                    "default": DEFAULT_SPEECH_MODEL
                },
                "at": { "type": "string", "description": "Place the result as a clip at this time" },
                "track": { "type": "string", "description": "Track to place it on; defaults to the first audio track" },
                "name": { "type": "string", "description": "Asset name; defaults to a slug of the text" }
            },
            "additionalProperties": false
        })
    }

    fn is_network(&self) -> bool {
        true
    }

    fn needs_tools(&self) -> &'static [&'static str] {
        &["ffprobe"]
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(
            &args,
            &["text", "voice", "engine", "model", "at", "track", "name"],
        )?;
        let text = args::str_field(&args, "text")?.trim().to_string();
        if text.is_empty() {
            return Err(Error::bad_args("empty text; nothing to say"));
        }
        let engine = match args::opt_str(&args, "engine") {
            Some(value) => Engine::parse(value)?,
            None => Engine::Fal,
        };
        let voice = args::opt_str(&args, "voice");
        let seq = cx.sequence(project)?;
        let fps = sequence_fps(project, &seq)?;
        let at = args::opt_time(&args, "at", fps)?;
        let track = args::opt_str(&args, "track");
        let name = args::opt_str(&args, "name");

        let outcome = match engine {
            Engine::Fal => {
                let request = Request {
                    kind: Kind::Speech,
                    provider: fal::PROVIDER,
                    model: model_of(&args, DEFAULT_SPEECH_MODEL)?,
                    params: tts::speech_params(&text, voice),
                    prompt: Some(text.clone()),
                    seed: None,
                    units: tts::speech_units(&text),
                    local_extension: "wav",
                };
                run_hosted(cx, &self.transport, &request)?
            }
            Engine::Piper => {
                // The voice is part of the identity of a local synthesis, so it belongs in
                // the cache key; the model field is filled in with the resolved `.onnx`
                // once piper has told us which one it used.
                let request = Request {
                    kind: Kind::Speech,
                    provider: tts::PIPER,
                    model: voice.unwrap_or("default").to_string(),
                    params: tts::speech_params(&text, voice),
                    prompt: Some(text.clone()),
                    seed: None,
                    units: tts::speech_units(&text),
                    local_extension: "wav",
                };
                run_local_speech(cx, &request, &text, voice)?
            }
        };
        absorb(project, cx, outcome, at, None, track, name)
    }
}

struct BudgetQuery;

impl Op for BudgetQuery {
    fn id(&self) -> &'static str {
        "ai.budget"
    }

    fn about(&self) -> &'static str {
        "Report the project's AI spend ceiling, spend so far and remaining budget"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn is_query(&self) -> bool {
        true
    }

    fn apply(
        &self,
        _project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &[])?;
        let budget = Budget::new(cx.paths, Arc::clone(cx.assets.vfs()));
        let ledger = budget.load()?;
        let mut data = ledger.report(budget.path());
        if let Some(object) = data.as_object_mut() {
            let config = ProviderConfig::load()?;
            let fal = Fal::new(default_transport(), &config);
            let piper = Piper::discover();
            object.insert(
                "providers".into(),
                serde_json::json!([
                    { "name": fal.name(), "keyEnv": fal.key_env(), "available": fal.available() },
                    { "name": piper.name(), "keyEnv": piper.key_env(), "available": piper.available() },
                ]),
            );
        }
        Ok(OpEffect::new().data(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::request_key;
    use crate::testkit::{self, json_response, Forbidden, Routed};
    use dvs_core::engine::Workspace;
    use dvs_core::project::Project;
    use dvs_core::vfs::FsVfs;
    use dvs_media::toolchain::Toolchain;
    use std::path::{Path, PathBuf};

    struct Fixture {
        _dir: tempfile::TempDir,
        workspace: Workspace,
    }

    impl Fixture {
        fn new() -> Fixture {
            let dir = tempfile::tempdir().expect("tempdir");
            let paths = dvs_core::paths::ProjectPaths::new(dir.path());
            let project = Project::new("promo", dvs_core::time::Fps::new(30, 1).unwrap(), [1920, 1080], 48_000);
            let workspace =
                Workspace::create(paths, project, FsVfs::shared()).expect("fresh project");
            Fixture {
                _dir: dir,
                workspace,
            }
        }

        fn apply(
            &mut self,
            transport: Arc<dyn Transport>,
            op: &str,
            args: serde_json::Value,
            dry_run: bool,
        ) -> Result<OpEffect> {
            let mut registry = Registry::new();
            register_with(&mut registry, transport);
            let op = registry.get(op).expect("registered op").clone();
            let mut cx = OpCx::new(&self.workspace.paths, &self.workspace.assets).dry_run(dry_run);
            op.apply(&mut self.workspace.project, args, &mut cx)
        }

        fn budget(&self) -> Budget {
            Budget::new(&self.workspace.paths, self.workspace.vfs())
        }

        fn write_budget(&self, json: serde_json::Value) {
            std::fs::write(
                self.workspace.paths.root().join(budget::BUDGET_FILE),
                json.to_string(),
            )
            .expect("budget file");
        }
    }

    /// A real one-second mp4, because the import path runs ffprobe over whatever a provider
    /// returned and a fake byte string would exercise none of it.
    fn sample_video(tool: &Toolchain, dir: &Path) -> PathBuf {
        let path = dir.join("sample.mp4");
        let status = tool
            .ffmpeg_command()
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=30:duration=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&path)
            .status()
            .expect("ffmpeg runs");
        assert!(status.success(), "ffmpeg could not build the fixture");
        path
    }

    fn sample_audio(tool: &Toolchain, dir: &Path) -> PathBuf {
        let path = dir.join("sample.wav");
        let status = tool
            .ffmpeg_command()
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=1"])
            .arg(&path)
            .status()
            .expect("ffmpeg runs");
        assert!(status.success(), "ffmpeg could not build the fixture");
        path
    }

    fn video_args() -> serde_json::Value {
        serde_json::json!({
            "prompt": "a cat on a skateboard",
            "model": "fal-ai/kling",
            "duration": "5s",
            "seed": 7
        })
    }

    /// The submit/poll/result/download conversation, with real media bytes as the download.
    fn fal_routes(media: &[u8]) -> Vec<(&'static str, crate::provider::HttpResponse)> {
        vec![
            (
                "/status",
                json_response(serde_json::json!({ "status": "COMPLETED" })),
            ),
            (
                "fal.media",
                crate::provider::HttpResponse::new(200, media.to_vec())
                    .with_header("content-type", "video/mp4"),
            ),
            (
                "/requests/req-1",
                json_response(serde_json::json!({
                    "video": { "url": "https://fal.media/files/out.mp4", "content_type": "video/mp4" }
                })),
            ),
            (
                "queue.fal.run",
                json_response(serde_json::json!({ "request_id": "req-1" })),
            ),
        ]
    }

    #[test]
    fn with_no_key_every_generation_op_fails_naming_the_variable() {
        let _env = testkit::isolate();
        let mut fixture = Fixture::new();
        let cases = [
            ("ai.generate-video", serde_json::json!({ "prompt": "a cat" })),
            ("ai.generate-image", serde_json::json!({ "prompt": "a cat" })),
            ("ai.tts", serde_json::json!({ "text": "hello" })),
        ];

        for (op, args) in cases {
            let error = fixture
                .apply(Arc::new(Forbidden), op, args, false)
                .expect_err("no key means no generation");
            assert_eq!(
                error.exit_code(),
                dvs_core::exit::TOOL_MISSING,
                "{op} must exit 5, got {error}"
            );
            assert!(
                error.to_string().contains(fal::KEY_ENV),
                "{op} must name {}: {error}",
                fal::KEY_ENV
            );
        }
        assert!(fixture.workspace.project.assets.is_empty());
    }

    #[test]
    fn the_piper_engine_reports_its_install_instead_of_a_key() {
        let _env = testkit::isolate();
        std::env::set_var(tts::PIPER_ENV, "/nonexistent/piper");
        let mut fixture = Fixture::new();

        let error = fixture
            .apply(
                Arc::new(Forbidden),
                "ai.tts",
                serde_json::json!({ "text": "hello", "engine": "piper" }),
                false,
            )
            .expect_err("no piper installed in the test environment");

        assert_eq!(error.exit_code(), dvs_core::exit::TOOL_MISSING);
        assert!(error.to_string().contains("piper"), "{error}");
    }

    /// The keyless path, end to end: a scripted `piper` stands in for an install this
    /// machine does not have, and what lands in the document must be indistinguishable
    /// from an imported recording — hashed blob, probe, provenance, audio track.
    #[cfg(unix)]
    #[test]
    fn the_local_engine_produces_a_normal_audio_asset_with_provenance() {
        use std::os::unix::fs::PermissionsExt;

        let _env = testkit::isolate();
        let tool = Toolchain::shared().expect("ffmpeg is installed");
        let staging = tempfile::tempdir().expect("tempdir");
        let wav = sample_audio(tool, staging.path());
        let voice = staging.path().join("en_US-test.onnx");
        std::fs::write(&voice, b"model").unwrap();
        let binary = staging.path().join("piper");
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\ncat > /dev/null\nout=\"\"\nwhile [ $# -gt 0 ]; do\n\
                 \tif [ \"$1\" = \"--output_file\" ]; then out=\"$2\"; fi\n\tshift\ndone\n\
                 cp {} \"$out\"\n",
                wav.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var(tts::PIPER_ENV, &binary);
        std::env::set_var(tts::VOICES_ENV, staging.path());
        let mut fixture = Fixture::new();
        let args = serde_json::json!({
            "text": "one small step",
            "engine": "piper",
            "voice": "en_US-test",
            "at": "0"
        });

        let effect = fixture
            .apply(Arc::new(Forbidden), "ai.tts", args.clone(), false)
            .expect("local synthesis needs no key and no network");

        let data = effect.data.clone().expect("report");
        assert_eq!(data["provider"], serde_json::json!("piper"));
        assert_eq!(data["cached"], serde_json::json!(false));
        assert_eq!(data["costUsd"].as_f64(), Some(0.0));
        let asset = fixture
            .workspace
            .project
            .assets
            .values()
            .next()
            .expect("one asset");
        assert!(asset.probe.audio.is_some(), "probed as audio");
        assert!(
            fixture.workspace.assets.exists(&asset.hash),
            "a local generation is hashed into the store like any import"
        );
        let provenance = asset.provenance.as_ref().expect("provenance");
        assert_eq!(provenance.provider, "piper");
        assert_eq!(provenance.model, voice.display().to_string());
        assert_eq!(provenance.prompt.as_deref(), Some("one small step"));
        let seq = fixture.workspace.project.active_sequence.clone();
        let sequence = fixture.workspace.project.sequence(&seq).unwrap();
        assert_eq!(
            sequence
                .tracks
                .iter()
                .filter(|track| track.kind == TrackKind::Audio)
                .flat_map(|track| track.clips.iter())
                .count(),
            1
        );

        // The second call is served from the cache, so piper is not re-run: pointing the
        // binary at nothing proves it. No `at` this time — the first clip owns 0s, and a
        // second one there would be an overlap the timeline rightly refuses.
        std::env::set_var(tts::PIPER_ENV, "/nonexistent/piper");
        let repeat = fixture
            .apply(
                Arc::new(Forbidden),
                "ai.tts",
                serde_json::json!({
                    "text": "one small step",
                    "engine": "piper",
                    "voice": "en_US-test"
                }),
                false,
            )
            .expect("the repeat comes from the cache");
        let repeat = repeat.data.expect("report");
        assert_eq!(repeat["cached"], serde_json::json!(true));
        assert_eq!(repeat["costUsd"].as_f64(), Some(0.0));
    }

    #[test]
    fn a_generation_is_hashed_probed_and_recorded_with_provenance() {
        let _env = testkit::isolate();
        std::env::set_var(fal::KEY_ENV, "test-key");
        let tool = Toolchain::shared().expect("ffmpeg is installed");
        let staging = tempfile::tempdir().expect("tempdir");
        let media = std::fs::read(sample_video(tool, staging.path())).expect("fixture bytes");
        let mut fixture = Fixture::new();
        fixture.write_budget(serde_json::json!({ "ceilingUsd": 10.0 }));
        let transport = Routed::new(fal_routes(&media));

        let effect = fixture
            .apply(transport.clone(), "ai.generate-video", video_args(), false)
            .expect("scripted generation succeeds");

        let data = effect.data.clone().expect("a generation reports its cost");
        assert_eq!(data["cached"], serde_json::json!(false));
        assert_eq!(data["provider"], serde_json::json!("fal"));
        assert!(data["costUsd"].as_f64().unwrap() > 0.0);

        let asset_id = data["asset"].as_str().expect("asset id");
        let asset = fixture
            .workspace
            .project
            .assets
            .values()
            .find(|asset| asset.id.as_str() == asset_id)
            .expect("asset in the document");
        let provenance = asset.provenance.as_ref().expect("provenance recorded");
        assert_eq!(provenance.provider, "fal");
        assert_eq!(provenance.model, "fal-ai/kling");
        assert_eq!(provenance.prompt.as_deref(), Some("a cat on a skateboard"));
        assert_eq!(provenance.seed, Some(7));
        assert!(provenance.cost_usd.unwrap() > 0.0);
        assert!(
            fixture.workspace.assets.exists(&asset.hash),
            "the generation must be an ordinary hashed blob in the store"
        );
        assert!(asset.probe.video.is_some(), "probed as video");
        assert!(
            !asset
                .source_path
                .as_deref()
                .unwrap_or_default()
                .starts_with("http"),
            "the document must not point at a remote url"
        );
    }

    #[test]
    fn a_repeat_request_is_served_from_the_cache_at_no_cost_and_no_network() {
        let _env = testkit::isolate();
        std::env::set_var(fal::KEY_ENV, "test-key");
        let tool = Toolchain::shared().expect("ffmpeg is installed");
        let staging = tempfile::tempdir().expect("tempdir");
        let media = std::fs::read(sample_video(tool, staging.path())).expect("fixture bytes");
        let mut fixture = Fixture::new();
        fixture.write_budget(serde_json::json!({ "ceilingUsd": 10.0 }));
        let transport = Routed::new(fal_routes(&media));
        let first = fixture
            .apply(transport.clone(), "ai.generate-video", video_args(), false)
            .expect("first generation");
        let charged = fixture.budget().load().unwrap().spent_usd;
        assert!(charged > 0.0);
        assert_eq!(transport.calls(), 4, "submit, status, result, download");

        // The key is absent for the repeat: a cache hit must not need a credential either.
        std::env::remove_var(fal::KEY_ENV);
        let second = fixture
            .apply(Arc::new(Forbidden), "ai.generate-video", video_args(), false)
            .expect("the repeat is served from the cache");

        let data = second.data.clone().expect("report");
        assert_eq!(data["cached"], serde_json::json!(true));
        assert_eq!(data["costUsd"].as_f64(), Some(0.0));
        assert_eq!(
            data["cacheKey"], first.data.unwrap()["cacheKey"],
            "the same parameters must produce the same key"
        );
        assert_eq!(
            fixture.budget().load().unwrap().spent_usd,
            charged,
            "a cache hit must not move the spend"
        );
        assert_eq!(
            fixture.workspace.project.assets.len(),
            2,
            "the repeat still produces a document asset"
        );
    }

    #[test]
    fn a_dry_run_reports_an_estimate_and_sends_nothing() {
        let _env = testkit::isolate();
        std::env::set_var(fal::KEY_ENV, "test-key");
        let mut fixture = Fixture::new();
        fixture.write_budget(serde_json::json!({ "ceilingUsd": 10.0 }));

        let effect = fixture
            .apply(Arc::new(Forbidden), "ai.generate-video", video_args(), true)
            .expect("a dry run must not need a provider");

        let data = effect.data.expect("dry run reports");
        assert_eq!(data["dryRun"], serde_json::json!(true));
        assert_eq!(data["sent"], serde_json::json!(false));
        let estimate = data["estimateUsd"].as_f64().expect("an estimate");
        assert!((estimate - 5.0 * Kind::Video.default_price()).abs() < 1e-9, "{estimate}");
        assert!(data.get("asset").is_none(), "nothing was imported");
        assert!(fixture.workspace.project.assets.is_empty());
        assert_eq!(fixture.budget().load().unwrap().spent_usd, 0.0);
    }

    #[test]
    fn a_request_over_the_ceiling_is_refused_before_anything_is_sent() {
        let _env = testkit::isolate();
        std::env::set_var(fal::KEY_ENV, "test-key");
        let mut fixture = Fixture::new();
        fixture.write_budget(serde_json::json!({ "ceilingUsd": 0.30, "spentUsd": 0.20 }));

        let error = fixture
            .apply(Arc::new(Forbidden), "ai.generate-video", video_args(), false)
            .expect_err("5 seconds of video costs more than $0.10");

        assert_eq!(error.exit_code(), dvs_core::exit::BUDGET);
        let message = error.to_string();
        for expected in ["$0.30", "$0.20", "$0.50"] {
            assert!(message.contains(expected), "{expected} missing: {message}");
        }
        assert_eq!(
            fixture.budget().load().unwrap().spent_usd,
            0.20,
            "a refusal must not charge"
        );
    }

    /// The awkward combination: a dry run whose result is already cached. Reporting it is
    /// free, importing it is not — a blob under `assets/` outlives the discarded document.
    #[test]
    fn a_dry_run_over_a_cache_hit_reports_but_imports_nothing() {
        let _env = testkit::isolate();
        let tool = Toolchain::shared().expect("ffmpeg is installed");
        let staging = tempfile::tempdir().expect("tempdir");
        let wav = std::fs::read(sample_audio(tool, staging.path())).expect("fixture bytes");
        let fixture = Fixture::new();
        let params = tts::speech_params("hello there", None);
        let key = request_key(fal::PROVIDER, DEFAULT_SPEECH_MODEL, &params);
        let cache = AiCache::new(&fixture.workspace.paths, fixture.workspace.vfs());
        cache
            .store(
                &key,
                &CachedRequest {
                    provider: fal::PROVIDER.into(),
                    model: DEFAULT_SPEECH_MODEL.into(),
                    params,
                    media: "media.wav".into(),
                    source_url: None,
                    cost_usd: 0.02,
                    created: Utc::now(),
                },
                &wav,
            )
            .expect("seed the cache");
        let mut fixture = fixture;

        let effect = fixture
            .apply(
                Arc::new(Forbidden),
                "ai.tts",
                serde_json::json!({ "text": "hello there", "at": "0" }),
                true,
            )
            .expect("a dry run over a cache hit still reports");

        let data = effect.data.expect("report");
        assert_eq!(data["cached"], serde_json::json!(true));
        assert_eq!(data["dryRun"], serde_json::json!(true));
        assert!(data.get("asset").is_none());
        assert!(fixture.workspace.project.assets.is_empty());
        assert!(
            fixture.workspace.assets.list().unwrap().is_empty(),
            "a dry run must not leave a blob in the store"
        );
    }

    #[test]
    fn a_placed_generation_lands_on_a_track_as_an_ordinary_clip() {
        let _env = testkit::isolate();
        std::env::set_var(fal::KEY_ENV, "test-key");
        let tool = Toolchain::shared().expect("ffmpeg is installed");
        let staging = tempfile::tempdir().expect("tempdir");
        let media = std::fs::read(sample_video(tool, staging.path())).expect("fixture bytes");
        let mut fixture = Fixture::new();
        fixture.write_budget(serde_json::json!({ "ceilingUsd": 10.0 }));
        let transport = Routed::new(fal_routes(&media));
        let mut args = video_args();
        args["at"] = serde_json::json!("00:02");

        let effect = fixture
            .apply(transport, "ai.generate-video", args, false)
            .expect("generation with placement");

        let seq = fixture.workspace.project.active_sequence.clone();
        let sequence = fixture.workspace.project.sequence(&seq).unwrap();
        let track = sequence
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .expect("a video track was created");
        let clip = track.clips.first().expect("one clip");
        assert_eq!(clip.start, Time::from_secs(2));
        assert!(clip.duration.is_positive());
        assert!(matches!(clip.source, Source::Asset { .. }));
        assert!(
            effect.created.iter().any(|id| id == clip.id.as_str()),
            "the clip is reported as created: {:?}",
            effect.created
        );
    }

    #[test]
    fn tts_through_the_cache_becomes_an_audio_asset_on_an_audio_track() {
        let _env = testkit::isolate();
        let tool = Toolchain::shared().expect("ffmpeg is installed");
        let staging = tempfile::tempdir().expect("tempdir");
        let wav = std::fs::read(sample_audio(tool, staging.path())).expect("fixture bytes");
        let mut fixture = Fixture::new();

        // Pre-seed the cache the way a previous identical request would have, then run with
        // a transport that panics: proving the audio path is cacheable and keyless.
        let params = tts::speech_params("hello there", None);
        let key = request_key(fal::PROVIDER, DEFAULT_SPEECH_MODEL, &params);
        let cache = AiCache::new(&fixture.workspace.paths, fixture.workspace.vfs());
        cache
            .store(
                &key,
                &CachedRequest {
                    provider: fal::PROVIDER.into(),
                    model: DEFAULT_SPEECH_MODEL.into(),
                    params,
                    media: "media.wav".into(),
                    source_url: None,
                    cost_usd: 0.02,
                    created: Utc::now(),
                },
                &wav,
            )
            .expect("seed the cache");

        let effect = fixture
            .apply(
                Arc::new(Forbidden),
                "ai.tts",
                serde_json::json!({ "text": "hello there", "at": "0" }),
                false,
            )
            .expect("cached speech needs no key");

        let data = effect.data.expect("report");
        assert_eq!(data["cached"], serde_json::json!(true));
        let seq = fixture.workspace.project.active_sequence.clone();
        let sequence = fixture.workspace.project.sequence(&seq).unwrap();
        let track = sequence
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .expect("an audio track was created for audio");
        assert_eq!(track.clips.len(), 1);
        let asset = fixture
            .workspace
            .project
            .assets
            .values()
            .next()
            .expect("asset");
        assert!(asset.probe.audio.is_some());
        assert_eq!(asset.provenance.as_ref().unwrap().cost_usd, Some(0.0));
    }

    #[test]
    fn the_budget_query_reports_the_numbers_and_the_provider_state() {
        let _env = testkit::isolate();
        let mut fixture = Fixture::new();
        fixture.write_budget(serde_json::json!({ "ceilingUsd": 2.0, "spentUsd": 0.5 }));

        let effect = fixture
            .apply(Arc::new(Forbidden), "ai.budget", serde_json::json!({}), false)
            .expect("a query never spends");

        let data = effect.data.expect("report");
        assert_eq!(data["ceilingUsd"].as_f64(), Some(2.0));
        assert_eq!(data["spentUsd"].as_f64(), Some(0.5));
        assert_eq!(data["remainingUsd"].as_f64(), Some(1.5));
        let providers = data["providers"].as_array().expect("providers");
        let fal_entry = providers
            .iter()
            .find(|entry| entry["name"] == serde_json::json!("fal"))
            .expect("fal is listed");
        assert_eq!(fal_entry["available"], serde_json::json!(false));
        assert_eq!(fal_entry["keyEnv"], serde_json::json!(fal::KEY_ENV));
    }

    #[test]
    fn the_registry_declares_the_network_ops_as_network() {
        let mut registry = Registry::new();
        register_with(&mut registry, Arc::new(Forbidden));

        for id in ["ai.generate-video", "ai.generate-image", "ai.tts"] {
            let op = registry.get(id).expect("registered");
            assert!(op.is_network(), "{id} must declare itself network");
            assert!(!op.needs_tools().is_empty(), "{id} must declare its tools");
        }
        let query = registry.get("ai.budget").expect("registered");
        assert!(query.is_query());
        assert!(!query.is_network());
    }

    #[test]
    fn unknown_arguments_are_refused_rather_than_ignored() {
        let _env = testkit::isolate();
        let mut fixture = Fixture::new();
        let error = fixture
            .apply(
                Arc::new(Forbidden),
                "ai.generate-image",
                serde_json::json!({ "prompt": "x", "sizes": "1920x1080" }),
                false,
            )
            .expect_err("a typo must not be silently dropped");
        assert_eq!(error.exit_code(), dvs_core::exit::BAD_ARGS);
        assert!(error.to_string().contains("sizes"), "{error}");
    }

    #[test]
    fn a_pixel_size_becomes_dimensions_and_a_model_size_name_passes_through() {
        assert_eq!(parse_size("1920X1080"), Some([1920, 1080]));
        assert_eq!(parse_size(" 512 x 512 "), Some([512, 512]));
        assert_eq!(parse_size("landscape_16_9"), None);
        assert_eq!(parse_size("0x100"), None);
    }
}
