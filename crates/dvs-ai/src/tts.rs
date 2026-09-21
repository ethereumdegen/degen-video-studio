//! Text to speech, with a path that needs no key.
//!
//! Narration is the one AI capability an agent reaches for constantly — a script, a voice,
//! an audio track — so it is the worst one to make conditional on a credential. Two engines
//! therefore answer the same op: fal-hosted models through the queue API, and a local
//! `piper` binary. Both produce an ordinary hashed audio asset with provenance; nothing
//! downstream can tell which one made it.
//!
//! The failure mode this module is careful about is the silent half-success. `piper` exits
//! 0 while writing nothing when its voice model is wrong, so the output file is checked for
//! actual bytes, and a missing binary or voice is reported as [`dvs_core::Error::Tool`]
//! naming the install and the download rather than as an empty clip on the timeline.

use crate::provider::Provider;
use dvs_core::error::{Error, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Provider name recorded in provenance for the local engine.
pub const PIPER: &str = "piper";

/// Overrides the `piper` binary location, mirroring `DVS_FFMPEG` in `dvs-media`.
pub const PIPER_ENV: &str = "DVS_PIPER";

/// Directory of `.onnx` voice models to search.
pub const VOICES_ENV: &str = "DVS_PIPER_VOICES";

/// Which backend synthesizes the speech.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// A fal-hosted TTS model, through the same queue path as video and image generation.
    Fal,
    /// A local `piper` install: no key, no network, no cost.
    Piper,
}

impl Engine {
    pub fn parse(text: &str) -> Result<Engine> {
        match text.trim().to_ascii_lowercase().as_str() {
            "fal" => Ok(Engine::Fal),
            "piper" | "local" => Ok(Engine::Piper),
            other => Err(Error::bad_args(format!(
                "unknown tts engine '{other}'; expected 'fal' or 'piper'"
            ))),
        }
    }
}

/// The local `piper` engine.
#[derive(Debug, Clone)]
pub struct Piper {
    binary: Option<PathBuf>,
}

impl Provider for Piper {
    fn name(&self) -> &'static str {
        PIPER
    }

    fn key_env(&self) -> &'static str {
        PIPER_ENV
    }

    fn available(&self) -> bool {
        self.binary.is_some()
    }
}

impl Piper {
    /// `$DVS_PIPER`, then `PATH`. Resolved per call rather than cached, because an operator
    /// who just installed piper should not have to restart a long-lived MCP session.
    pub fn discover() -> Piper {
        let binary = std::env::var(PIPER_ENV)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_file())
            .or_else(|| which::which(PIPER).ok());
        Piper { binary }
    }

    pub fn binary(&self) -> Result<&Path> {
        self.binary.as_deref().ok_or_else(|| {
            Error::tool(
                PIPER,
                format!(
                    "no 'piper' binary on PATH: install it (`pacman -S piper-tts`, \
                     `brew install piper-tts`, or a release from \
                     https://github.com/rhasspy/piper) or point {PIPER_ENV} at the executable"
                ),
            )
        })
    }

    /// Resolve a voice to an `.onnx` model file.
    ///
    /// A path is taken as given; a name is looked up in the voice directories. With no
    /// voice at all, the first directory that holds any model wins and its
    /// lexicographically first `.onnx` is used — so `$DVS_PIPER_VOICES` overrides a
    /// system-wide install rather than competing with it, and the choice is deterministic
    /// instead of depending on directory order on disk.
    pub fn voice_model(&self, voice: Option<&str>) -> Result<PathBuf> {
        if let Some(voice) = voice.map(str::trim).filter(|voice| !voice.is_empty()) {
            let direct = Path::new(voice);
            if direct.is_file() {
                return Ok(direct.to_path_buf());
            }
            for dir in voice_dirs() {
                let candidate = dir.join(format!("{voice}.onnx"));
                if candidate.is_file() {
                    return Ok(candidate);
                }
                let bare = dir.join(voice);
                if bare.is_file() {
                    return Ok(bare);
                }
            }
            return Err(Error::tool(
                PIPER,
                format!(
                    "voice '{voice}' is not a file and was not found as '{voice}.onnx' in {}; \
                     download a voice from https://huggingface.co/rhasspy/piper-voices and pass \
                     --voice <path>.onnx or set {VOICES_ENV}",
                    describe(&voice_dirs())
                ),
            ));
        }
        for dir in voice_dirs() {
            let mut models: Vec<PathBuf> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "onnx"))
                .collect();
            models.sort();
            if let Some(first) = models.into_iter().next() {
                return Ok(first);
            }
        }
        Err(Error::tool(
            PIPER,
            format!(
                "no piper voice models found in {}; download one from \
                 https://huggingface.co/rhasspy/piper-voices, then pass --voice <path>.onnx \
                 or set {VOICES_ENV} to its directory",
                describe(&voice_dirs())
            ),
        ))
    }

    /// Synthesize `text` into `output` as a WAV file.
    pub fn speak(&self, text: &str, voice: Option<&str>, output: &Path) -> Result<PathBuf> {
        let binary = self.binary()?;
        let model = self.voice_model(voice)?;
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let mut child = Command::new(binary)
            .arg("--model")
            .arg(&model)
            .arg("--output_file")
            .arg(output)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::tool(PIPER, format!("cannot run {}: {e}", binary.display())))?;
        child
            .stdin
            .take()
            .ok_or_else(|| Error::tool(PIPER, "piper did not accept text on stdin"))?
            .write_all(text.as_bytes())
            .map_err(|e| Error::tool(PIPER, format!("cannot write text to piper: {e}")))?;
        let finished = child
            .wait_with_output()
            .map_err(|e| Error::tool(PIPER, format!("piper did not finish: {e}")))?;
        if !finished.status.success() {
            return Err(Error::tool(
                PIPER,
                format!(
                    "piper failed ({}) with voice {}: {}",
                    finished.status,
                    model.display(),
                    String::from_utf8_lossy(&finished.stderr).trim()
                ),
            ));
        }
        // piper exits 0 having written nothing when the model and its `.json` config
        // disagree, which would otherwise import as a zero-length audio asset.
        let written = std::fs::metadata(output).map(|meta| meta.len()).unwrap_or(0);
        if written == 0 {
            return Err(Error::tool(
                PIPER,
                format!(
                    "piper wrote no audio with voice {}: {}",
                    model.display(),
                    String::from_utf8_lossy(&finished.stderr).trim()
                ),
            ));
        }
        Ok(model)
    }
}

/// Where voice models are looked for, most specific first.
fn voice_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(explicit) = std::env::var(VOICES_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        dirs.push(PathBuf::from(explicit));
    }
    if let Ok(data_home) = std::env::var("XDG_DATA_HOME") {
        if !data_home.trim().is_empty() {
            dirs.push(Path::new(data_home.trim()).join("piper").join("voices"));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            dirs.push(
                Path::new(home.trim())
                    .join(".local")
                    .join("share")
                    .join("piper")
                    .join("voices"),
            );
        }
    }
    dirs.push(PathBuf::from("/usr/share/piper-voices"));
    dirs
}

fn describe(dirs: &[PathBuf]) -> String {
    dirs.iter()
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Request parameters for a speech synthesis.
///
/// Field names follow what fal's speech models accept (`text`, `voice`). The local engine
/// does not read them, but it is keyed on them: two piper requests differing only in voice
/// are two different generations, and an empty voice must hash the same as no voice at all
/// or a cache hit would depend on how the argument was spelled.
pub fn speech_params(text: &str, voice: Option<&str>) -> serde_json::Value {
    let mut params = serde_json::Map::new();
    params.insert("text".to_string(), serde_json::Value::String(text.to_string()));
    if let Some(voice) = voice.map(str::trim).filter(|voice| !voice.is_empty()) {
        params.insert("voice".to_string(), serde_json::Value::String(voice.to_string()));
    }
    serde_json::Value::Object(params)
}

/// Thousands of characters, the unit hosted TTS is priced in.
pub fn speech_units(text: &str) -> f64 {
    text.chars().count() as f64 / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;

    /// A stand-in `piper` on disk. The subject under test is our end of the protocol —
    /// which flags are passed, that the text goes on stdin, that a zero-length result is
    /// caught — and none of that is exercised by asserting on a struct. `piper` itself is
    /// not installed on every machine this suite runs on, so the binary is scripted.
    #[cfg(unix)]
    fn fake_piper(dir: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("piper");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("write script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make it executable");
        path
    }

    #[cfg(unix)]
    #[test]
    fn piper_is_given_the_voice_and_the_text_on_stdin() {
        let _env = testkit::isolate();
        let dir = tempfile::tempdir().expect("tempdir");
        let voice = dir.path().join("en_US-amy-medium.onnx");
        std::fs::write(&voice, b"model").unwrap();
        let seen = dir.path().join("seen.txt");
        let binary = fake_piper(
            dir.path(),
            &format!(
                "cat > {seen}\n\
                 echo \"ARGS: $*\" >> {seen}\n\
                 out=\"\"\n\
                 while [ $# -gt 0 ]; do\n\
                 \tif [ \"$1\" = \"--output_file\" ]; then out=\"$2\"; fi\n\
                 \tshift\n\
                 done\n\
                 printf 'RIFF....WAVE' > \"$out\"",
                seen = seen.display()
            ),
        );
        std::env::set_var(PIPER_ENV, &binary);
        std::env::set_var(VOICES_ENV, dir.path());
        let piper = Piper::discover();
        let output = dir.path().join("out.wav");

        let used = piper
            .speak("hello there", Some("en_US-amy-medium"), &output)
            .expect("the scripted engine succeeds");

        assert_eq!(used, voice, "the resolved .onnx is reported back");
        assert_eq!(std::fs::read(&output).unwrap(), b"RIFF....WAVE");
        let transcript = std::fs::read_to_string(&seen).unwrap();
        assert!(
            transcript.starts_with("hello there"),
            "the text must arrive on stdin: {transcript:?}"
        );
        assert!(
            transcript.contains(&format!("--model {}", voice.display())),
            "the voice must be passed as --model: {transcript:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_piper_that_exits_zero_without_writing_audio_is_a_failure() {
        let _env = testkit::isolate();
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("voice.onnx"), b"model").unwrap();
        let binary = fake_piper(dir.path(), "cat > /dev/null\necho 'bad config' >&2\nexit 0");
        std::env::set_var(PIPER_ENV, &binary);
        std::env::set_var(VOICES_ENV, dir.path());
        let piper = Piper::discover();

        let error = piper
            .speak("hello", None, &dir.path().join("out.wav"))
            .expect_err("silence must not import as audio");

        assert_eq!(error.exit_code(), dvs_core::exit::TOOL_MISSING);
        assert!(error.to_string().contains("no audio"), "{error}");
        assert!(error.to_string().contains("bad config"), "{error}");
    }

    #[test]
    fn an_absent_piper_is_reported_with_the_install_not_as_a_panic() {
        let _env = testkit::isolate();
        let piper = Piper { binary: None };

        assert!(!piper.available());
        let error = piper.binary().expect_err("no binary");

        assert_eq!(error.exit_code(), dvs_core::exit::TOOL_MISSING);
        assert!(error.to_string().contains("piper"), "{error}");
        assert!(error.to_string().contains(PIPER_ENV), "{error}");
    }

    #[test]
    fn an_unknown_voice_names_the_directories_it_looked_in() {
        let _env = testkit::isolate();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var(VOICES_ENV, dir.path());
        let piper = Piper {
            binary: Some(PathBuf::from("/nonexistent/piper")),
        };

        let error = piper
            .voice_model(Some("en_US-amy-medium"))
            .expect_err("no such voice");

        let message = error.to_string();
        assert!(message.contains("en_US-amy-medium"), "{message}");
        assert!(message.contains(&dir.path().display().to_string()), "{message}");
        assert!(message.contains(VOICES_ENV), "{message}");
    }

    #[test]
    fn a_voice_directory_resolves_a_name_and_prefers_an_explicit_path() {
        let _env = testkit::isolate();
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("en_US-amy-medium.onnx"), b"model").unwrap();
        std::fs::write(dir.path().join("aa_first.onnx"), b"model").unwrap();
        std::env::set_var(VOICES_ENV, dir.path());
        let piper = Piper {
            binary: Some(PathBuf::from("/nonexistent/piper")),
        };

        assert_eq!(
            piper.voice_model(Some("en_US-amy-medium")).unwrap(),
            dir.path().join("en_US-amy-medium.onnx")
        );
        assert_eq!(
            piper.voice_model(None).unwrap(),
            dir.path().join("aa_first.onnx"),
            "no voice given picks deterministically, not at random"
        );
        let explicit = dir.path().join("en_US-amy-medium.onnx");
        assert_eq!(
            piper.voice_model(Some(explicit.to_str().unwrap())).unwrap(),
            explicit
        );
    }

    #[test]
    fn engines_parse_from_the_cli_spelling_only() {
        assert_eq!(Engine::parse("FAL").unwrap(), Engine::Fal);
        assert_eq!(Engine::parse(" piper ").unwrap(), Engine::Piper);
        let error = Engine::parse("elevenlabs").expect_err("unknown engine");
        assert_eq!(error.exit_code(), dvs_core::exit::BAD_ARGS);
    }

    #[test]
    fn speech_is_priced_per_thousand_characters() {
        assert!((speech_units(&"a".repeat(1500)) - 1.5).abs() < 1e-9);
        assert_eq!(speech_units(""), 0.0);
    }

    #[test]
    fn speech_params_omit_an_empty_voice_so_the_cache_key_is_stable() {
        assert_eq!(
            speech_params("hello", Some("  ")),
            serde_json::json!({ "text": "hello" })
        );
        assert_eq!(
            speech_params("hello", Some("amy")),
            serde_json::json!({ "text": "hello", "voice": "amy" })
        );
    }
}
