//! Running whisper.cpp, and saying something useful when it is not available.
//!
//! Transcription is the one capability in this crate that needs a model on disk and a C++
//! build, so it sits behind the off-by-default `whisper` feature: `PLAN.md` §2 locks "the
//! editor is complete with no models and no keys", and making every install pay for a CUDA
//! toolchain would break that. Everything else here — search, filler cuts, phrase keeps,
//! captions — works on words that arrived from anywhere, which is why the unavailable path
//! points at `transcript.import` instead of just failing.
//!
//! Model resolution is deliberate rather than clever. A wrong or missing model is the most
//! likely failure, and "file not found" is not the actionable part of it, so the error
//! carries the exact file name, the exact place it was looked for, and the exact URL to
//! fetch it from.

use dvs_core::error::{Error, Result};
use dvs_core::ids::AssetId;
use dvs_core::time::Time;
use std::path::{Path, PathBuf};

/// Sample rate whisper's mel front-end requires. Feeding it anything else silently
/// transcribes chipmunks, so the rate is stated here and never taken from the sequence.
pub const SAMPLE_RATE: u32 = 16_000;

/// Model used when the caller names none: English-only base, the smallest model whose
/// Whether this build links a transcriber. Ops consult it before promising a run, so a
/// `--dry-run` cannot report that transcription would succeed in a build that cannot do it.
pub const AVAILABLE: bool = cfg!(feature = "whisper");

/// output is worth editing against.
pub const DEFAULT_MODEL: &str = "base.en";

/// Environment override for the model, checked after `--model` and before the cache.
pub const MODEL_ENV: &str = "DVS_WHISPER_MODEL";

/// Where whisper.cpp publishes the GGML conversions.
const MODEL_BASE_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

/// The whisper.cpp model list, for the candidate hint on a typo'd `--model`.
pub const MODELS: &[&str] = &[
    "tiny",
    "tiny.en",
    "tiny-q5_1",
    "base",
    "base.en",
    "base-q5_1",
    "small",
    "small.en",
    "small-q5_1",
    "medium",
    "medium.en",
    "medium-q5_0",
    "large-v2",
    "large-v3",
    "large-v3-turbo",
    "large-v3-turbo-q5_0",
];

/// The file name whisper.cpp gives a model, and the name this crate stores in
/// [`crate::Transcript::model`].
pub fn model_file_name(name: &str) -> String {
    format!("ggml-{name}.bin")
}

/// Where to download a model from. Printed in the missing-model error, because the fix is
/// one `curl` and the agent should not have to search for the URL.
pub fn download_url(name: &str) -> String {
    format!("{MODEL_BASE_URL}/{}", model_file_name(name))
}

/// `$XDG_CACHE_HOME/dvs/models`, else `~/.cache/dvs/models`.
pub fn model_cache_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(xdg).join("dvs").join("models");
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".cache").join("dvs").join("models")
}

/// A model the runner is prepared to load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    /// Where the weights are.
    pub path: PathBuf,
    /// The whisper.cpp model name (`base.en`), recorded in the transcript as provenance.
    pub name: String,
}

/// Whether a `--model` value names a file rather than a model.
fn looks_like_path(value: &str) -> bool {
    value.ends_with(".bin")
        || value.contains(std::path::MAIN_SEPARATOR)
        || value.contains('/')
        || value.starts_with('~')
}

/// `ggml-base.en.bin` → `base.en`, anything else → its own file stem.
fn name_from_path(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .map(|stem| stem.strip_prefix("ggml-").unwrap_or(&stem).to_string())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

/// Expand a leading `~`, so `--model ~/models/ggml-base.en.bin` works from a shell that
/// quoted it.
fn expand_home(value: &str) -> PathBuf {
    match value.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME").filter(|home| !home.is_empty()) {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(value),
        },
        None => PathBuf::from(value),
    }
}

/// Where a `--model` value points, without checking that it is there.
///
/// `--model` accepts both spellings an agent will write: a path to weights, or a
/// whisper.cpp model name that resolves into the shared cache directory. Resolution order
/// is `--model`, then `DVS_WHISPER_MODEL`, then the default model in the cache.
pub fn locate_model(explicit: Option<&str>) -> Model {
    // The environment is read only when no model was named, so `--model` never pays for a
    // process-wide lookup and a caller that passes one is not affected by a stale variable.
    let requested = explicit
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var(MODEL_ENV)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        });

    match requested {
        Some(value) if looks_like_path(&value) => {
            let path = expand_home(&value);
            Model {
                name: name_from_path(&path),
                path,
            }
        }
        Some(name) => Model {
            path: model_cache_dir().join(model_file_name(&name)),
            name,
        },
        None => Model {
            path: model_cache_dir().join(model_file_name(DEFAULT_MODEL)),
            name: DEFAULT_MODEL.to_string(),
        },
    }
}

/// [`locate_model`], plus the check that the weights are actually there.
///
/// The error is a tool error (exit code 5, not 1) because the fix is an install step rather
/// than a document change, and it names the download URL so the fix is copy-pasteable.
pub fn resolve_model(explicit: Option<&str>) -> Result<Model> {
    let model = locate_model(explicit);
    if model.path.is_file() {
        return Ok(model);
    }
    let known = MODELS.contains(&model.name.as_str());
    let hint = if known {
        format!(
            "download it with: curl -L --create-dirs -o {} {}",
            model.path.display(),
            download_url(&model.name)
        )
    } else {
        format!(
            "'{}' is not a whisper.cpp model name; known models: {}",
            model.name,
            MODELS.join(", ")
        )
    };
    Err(Error::tool(
        "whisper",
        format!(
            "model '{}' is not at {}; {hint}",
            model_file_name(&model.name),
            model.path.display()
        ),
    ))
}

/// What to transcribe, and with what.
#[derive(Debug, Clone)]
pub struct RunSpec<'a> {
    /// `--model`: a path to weights or a whisper.cpp model name. `None` falls back to
    /// [`MODEL_ENV`] and then [`DEFAULT_MODEL`].
    pub model: Option<&'a str>,
    /// `--language`: a whisper language code, or `None` to let the model detect one.
    pub language: Option<&'a str>,
    /// How much of the media to read. Comes from the asset's probe, so a truncated file
    /// cannot make the decoder wait on a stream that never ends.
    pub duration: Time,
}

#[cfg(feature = "whisper")]
mod runner {
    use super::*;
    use crate::transcript::{Transcript, Word};
    use dvs_core::time::Span;
    use dvs_media::toolchain::Toolchain;
    use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

    /// Whisper reports token times in centiseconds.
    fn centiseconds(value: i64) -> Time {
        Time::new(value, 100).unwrap_or(Time::ZERO)
    }

    /// Transcribe an asset's audio into a word-timestamped [`Transcript`].
    ///
    /// Audio is extracted through ffmpeg at 16 kHz mono f32 — the only format whisper's
    /// front-end accepts — and word times come from whisper.cpp's own word splitting
    /// (`max_len = 1` with `split_on_word`) rather than from post-hoc alignment, so a
    /// filler cut lands where the model heard the word and not where a heuristic guessed.
    pub fn run(
        tool: &Toolchain,
        media: &Path,
        asset: &AssetId,
        spec: &RunSpec<'_>,
    ) -> Result<Transcript> {
        if !spec.duration.is_positive() {
            return Err(Error::op(format!(
                "asset {asset} has no duration to transcribe; re-import it so it is probed"
            )));
        }
        let model = resolve_model(spec.model)?;
        let samples = dvs_media::decode_audio(
            tool,
            media,
            Span::new(Time::ZERO, spec.duration),
            SAMPLE_RATE,
            1,
        )?;
        if samples.is_empty() {
            return Err(Error::op(format!(
                "asset {asset} has no audio stream to transcribe"
            )));
        }

        // `new_with_params` is generic over `AsRef<Path>`, so the path goes in as a path:
        // a lossy `String` would mangle a non-UTF-8 model directory.
        let context =
            WhisperContext::new_with_params(&model.path, WhisperContextParameters::default())
                .map_err(|error| {
                    Error::tool(
                        "whisper",
                        format!("cannot load model {}: {error}", model.path.display()),
                    )
                })?;
        let mut state = context.create_state().map_err(|error| {
            Error::tool("whisper", format!("cannot create a decoder state: {error}"))
        })?;

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(threads());
        params.set_translate(false);
        params.set_language(spec.language);
        params.set_token_timestamps(true);
        params.set_split_on_word(true);
        // One word per segment: this is how whisper.cpp itself produces word-level output.
        params.set_max_len(1);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_print_special(false);
        state
            .full(params, &samples)
            .map_err(|error| Error::tool("whisper", format!("transcription failed: {error}")))?;

        let eot = context.token_eot();
        let mut words = Vec::new();
        for index in 0..state.full_n_segments() {
            let Some(segment) = state.get_segment(index) else {
                continue;
            };
            let text = segment
                .to_str_lossy()
                .map_err(|error| {
                    Error::tool("whisper", format!("segment {index} is not text: {error}"))
                })?
                .trim()
                .to_string();
            if text.is_empty() {
                continue;
            }
            let mut probability = 0.0f32;
            let mut counted = 0u32;
            for token_index in 0..segment.n_tokens() {
                let Some(token) = segment.get_token(token_index) else {
                    continue;
                };
                if token.token_id() >= eot {
                    continue;
                }
                probability += token.token_probability();
                counted += 1;
            }
            let start = centiseconds(segment.start_timestamp());
            let end = centiseconds(segment.end_timestamp());
            // A model that emits a zero-length or inverted span would make every later
            // span computation nonsense; clamping keeps the document valid.
            let end = end.max(start);
            words.push(Word {
                text,
                start,
                end,
                confidence: if counted == 0 {
                    1.0
                } else {
                    probability / counted as f32
                },
            });
        }

        let language = spec
            .language
            .filter(|code| !code.is_empty() && *code != "auto")
            .map(str::to_string)
            .or_else(|| {
                whisper_rs::get_lang_str(state.full_lang_id_from_state()).map(str::to_string)
            })
            .unwrap_or_else(|| "und".to_string());
        Ok(Transcript::new(asset.clone(), language, model.name, words))
    }

    /// Whisper is compute-bound and scales to physical cores; one thread per core, at
    /// least one.
    fn threads() -> std::ffi::c_int {
        std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)
            .min(i32::MAX as usize) as std::ffi::c_int
    }
}

#[cfg(not(feature = "whisper"))]
mod runner {
    use super::*;
    use crate::transcript::Transcript;
    use dvs_media::toolchain::Toolchain;

    /// Transcription is not linked into this build.
    ///
    /// This is a capability statement, not a stub: every other transcript-driven op works
    /// on imported words, so the message names the feature flag that adds the runner *and*
    /// the op that gets words in without it.
    pub fn run(
        _tool: &Toolchain,
        _media: &Path,
        _asset: &AssetId,
        spec: &RunSpec<'_>,
    ) -> Result<Transcript> {
        let model = locate_model(spec.model);
        Err(Error::tool(
            "whisper",
            format!(
                "this build has no transcriber: rebuild with `cargo build --features whisper` \
                 to link whisper.cpp (it would load '{}'). Without it, bring words in from \
                 anywhere with 'transcript.import --asset <id> --path words.json', which \
                 accepts whisper.cpp, WhisperX and plain [{{text,start,end}}] JSON; every \
                 other transcript op works on imported words.",
                model.path.display()
            ),
        ))
    }
}

pub use runner::run;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_name_resolves_into_the_cache_and_a_path_is_taken_as_given() {
        let named = locate_model(Some("small.en"));
        assert_eq!(named.name, "small.en");
        assert_eq!(named.path, model_cache_dir().join("ggml-small.en.bin"));

        let direct = locate_model(Some("/opt/models/ggml-medium.bin"));
        assert_eq!(direct.path, PathBuf::from("/opt/models/ggml-medium.bin"));
        assert_eq!(
            direct.name, "medium",
            "the model name is recovered from the file name for provenance"
        );
    }

    #[test]
    fn the_environment_is_used_only_when_no_model_was_named() {
        // Safe to mutate without serializing the suite: this is the only test in the crate
        // that reads or writes the variable, because every other one names a model and
        // `locate_model` consults the environment only when none was named.
        std::env::set_var(MODEL_ENV, "/srv/ggml-large-v3.bin");
        let from_env = locate_model(None);
        let explicit = locate_model(Some("tiny.en"));
        std::env::remove_var(MODEL_ENV);

        assert_eq!(from_env.path, PathBuf::from("/srv/ggml-large-v3.bin"));
        assert_eq!(from_env.name, "large-v3");
        assert_eq!(
            explicit.name, "tiny.en",
            "--model must win over the environment"
        );
        assert_eq!(locate_model(None).name, DEFAULT_MODEL);
    }

    #[test]
    fn a_missing_model_names_the_file_the_place_and_the_download() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("ggml-base.en.bin");
        let error = resolve_model(Some(&missing.to_string_lossy()))
            .expect_err("a model that is not on disk cannot resolve");
        let message = error.to_string();
        assert_eq!(
            error.exit_code(),
            dvs_core::error::exit::TOOL_MISSING,
            "a missing model is an install problem, not a document problem"
        );
        assert!(message.contains("ggml-base.en.bin"), "{message}");
        assert!(message.contains(&missing.display().to_string()), "{message}");
        assert!(
            message.contains("huggingface.co/ggerganov/whisper.cpp"),
            "the error must carry the download URL: {message}"
        );
    }

    #[test]
    fn an_unknown_model_name_lists_the_real_ones() {
        let error = resolve_model(Some("huge.en")).expect_err("no such model");
        let message = error.to_string();
        assert!(message.contains("large-v3"), "{message}");
        assert!(
            !message.contains("curl"),
            "offering a download URL for a model that does not exist would be a dead end: {message}"
        );
    }

    #[test]
    fn a_model_that_exists_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ggml-tiny.en.bin");
        std::fs::write(&path, b"not really weights").unwrap();
        let model = resolve_model(Some(&path.to_string_lossy())).unwrap();
        assert_eq!(model.path, path);
        assert_eq!(model.name, "tiny.en");
    }

    #[cfg(not(feature = "whisper"))]
    #[test]
    fn without_the_feature_the_runner_explains_both_ways_forward() {
        let Ok(tool) = dvs_media::toolchain::Toolchain::discover() else {
            return;
        };
        let error = run(
            &tool,
            Path::new("/nonexistent.mp4"),
            &AssetId::new(),
            &RunSpec {
                model: Some("base.en"),
                language: Some("en"),
                duration: Time::from_secs(1),
            },
        )
        .expect_err("a build without the feature cannot transcribe");
        let message = error.to_string();
        assert_eq!(error.exit_code(), dvs_core::error::exit::TOOL_MISSING);
        assert!(message.contains("--features whisper"), "{message}");
        assert!(message.contains("transcript.import"), "{message}");
    }
}
