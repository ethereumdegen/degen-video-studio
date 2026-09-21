//! Finding and describing ffmpeg.
//!
//! The locked decision in `PLAN.md` §2: codecs are the ffmpeg *binary* over pipes, not
//! linked libav. The cost is process spawns; the benefit is that the hardest-to-build part
//! of the stack is a package manager away on both Linux and macOS, hardware encoders come
//! free, and nothing an agent measures depends on a C ABI.
//!
//! What ffmpeg was used is recorded in the project and in every segment cache key, because
//! encoded bytes are not reproducible across builds even when our frames are.

use dvs_core::error::{Error, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

/// Resolved external tools plus the capabilities they actually have on this machine.
#[derive(Debug, Clone)]
pub struct Toolchain {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    version: String,
    encoders: Vec<String>,
    filters: Vec<String>,
}

/// One discovery per process. Spawning `ffmpeg -encoders` costs ~40 ms and the answer
/// cannot change while we run.
/// The cached failure keeps only the *detail*: `Error::Tool` already renders as
/// "ffmpeg unavailable: …", so storing the rendered error and re-wrapping it would print
/// the prefix twice in `dvs doctor` on a machine with no ffmpeg.
static SHARED: LazyLock<std::result::Result<Toolchain, (String, String)>> =
    LazyLock::new(|| {
        Toolchain::discover().map_err(|error| match error {
            Error::Tool { tool, detail } => (tool, detail),
            other => ("ffmpeg".to_string(), other.to_string()),
        })
    });

impl Toolchain {
    /// `DVS_FFMPEG`/`DVS_FFPROBE` win, then `PATH`. The error names the install command for
    /// the platform rather than saying "not found", because that is the whole content of
    /// the fix.
    pub fn discover() -> Result<Toolchain> {
        let ffmpeg = locate("ffmpeg", "DVS_FFMPEG")?;
        let ffprobe = locate("ffprobe", "DVS_FFPROBE")?;
        let version = first_line(&ffmpeg, &["-hide_banner", "-version"])?;
        let encoders = list_names(&ffmpeg, "-encoders")?;
        let filters = list_names(&ffmpeg, "-filters")?;
        Ok(Toolchain {
            ffmpeg,
            ffprobe,
            version,
            encoders,
            filters,
        })
    }

    /// Process-wide cached discovery.
    pub fn shared() -> Result<&'static Toolchain> {
        match &*SHARED {
            Ok(tool) => Ok(tool),
            Err((tool, detail)) => Err(Error::tool(tool.clone(), detail.clone())),
        }
    }

    pub fn ffmpeg(&self) -> &Path {
        &self.ffmpeg
    }

    pub fn ffprobe(&self) -> &Path {
        &self.ffprobe
    }

    /// `ffmpeg version n9.0.1 …`, trimmed to the version token when recognisable.
    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn encoders(&self) -> &[String] {
        &self.encoders
    }

    pub fn has_encoder(&self, name: &str) -> bool {
        self.encoders.iter().any(|e| e == name)
    }

    pub fn has_filter(&self, name: &str) -> bool {
        self.filters.iter().any(|f| f == name)
    }

    /// A fresh `Command` for ffmpeg with the noise switched off. Every call site starts
    /// here so no invocation forgets `-nostdin` — an ffmpeg that steals the terminal's
    /// stdin breaks an interactive agent session in a way that is very hard to diagnose.
    pub fn ffmpeg_command(&self) -> Command {
        let mut command = Command::new(&self.ffmpeg);
        command.args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"]);
        command
    }

    pub fn ffprobe_command(&self) -> Command {
        let mut command = Command::new(&self.ffprobe);
        command.args(["-hide_banner", "-loglevel", "error"]);
        command
    }

    /// What `dvs doctor` prints.
    pub fn report(&self) -> ToolReport {
        ToolReport {
            ffmpeg: self.ffmpeg.display().to_string(),
            ffprobe: self.ffprobe.display().to_string(),
            version: self.version.clone(),
            hardware_encoders: HW_ENCODERS
                .iter()
                .filter(|name| self.has_encoder(name))
                .map(|name| name.to_string())
                .collect(),
            software_encoders: SW_ENCODERS
                .iter()
                .filter(|name| self.has_encoder(name))
                .map(|name| name.to_string())
                .collect(),
        }
    }
}

/// Encoders worth reporting: the platform-accelerated ones on each supported OS.
pub const HW_ENCODERS: &[&str] = &[
    "h264_videotoolbox",
    "hevc_videotoolbox",
    "prores_videotoolbox",
    "h264_nvenc",
    "hevc_nvenc",
    "av1_nvenc",
    "h264_vaapi",
    "hevc_vaapi",
    "h264_qsv",
];

pub const SW_ENCODERS: &[&str] = &[
    "libx264",
    "libx265",
    "libsvtav1",
    "libaom-av1",
    "libvpx-vp9",
    "prores_ks",
    "aac",
    "libopus",
    "pcm_s16le",
];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolReport {
    pub ffmpeg: String,
    pub ffprobe: String,
    pub version: String,
    pub hardware_encoders: Vec<String>,
    pub software_encoders: Vec<String>,
}

fn locate(binary: &str, env_var: &str) -> Result<PathBuf> {
    if let Some(value) = std::env::var_os(env_var) {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Ok(path);
        }
        return Err(Error::tool(
            binary,
            format!("{env_var} points at '{}', which is not a file", path.display()),
        ));
    }
    which::which(binary).map_err(|_| {
        Error::tool(
            binary,
            format!(
                "not on PATH. Install it (Linux: `sudo pacman -S ffmpeg` or `apt install ffmpeg`; \
                 macOS: `brew install ffmpeg`) or set {env_var} to the binary"
            ),
        )
    })
}

fn first_line(binary: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new(binary)
        .args(args)
        .output()
        .map_err(|e| Error::tool(binary.display().to_string(), e.to_string()))?;
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.lines().next().unwrap_or_default().trim().to_string())
}

/// Parse the name column out of `ffmpeg -encoders` / `-filters`. Both formats are
/// `FLAGS name description`, preceded by a header terminated by a line of dashes.
fn list_names(binary: &Path, flag: &str) -> Result<Vec<String>> {
    let output = Command::new(binary)
        .args(["-hide_banner", flag])
        .output()
        .map_err(|e| Error::tool(binary.display().to_string(), e.to_string()))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let body = match text.split_once("------") {
        Some((_, body)) => body,
        None => &text,
    };
    Ok(body
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let flags = parts.next()?;
            // Flag columns never contain '=' and are short; this rejects wrapped
            // description lines that would otherwise look like entries.
            if flags.len() > 8 || flags.contains('=') {
                return None;
            }
            let name = parts.next()?;
            (!name.is_empty()).then(|| name.to_string())
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_finds_the_local_ffmpeg_and_its_encoders() {
        let tool = Toolchain::discover().expect("ffmpeg is required to build this project");
        assert!(tool.version().contains("ffmpeg"), "{}", tool.version());
        // libx264 is the default encoder in PLAN.md §2; without it the render path has no
        // baseline, so this is a real environment assertion rather than a smoke test.
        assert!(tool.has_encoder("libx264"), "libx264 missing");
        assert!(tool.has_encoder("aac"), "aac missing");
        assert!(tool.has_filter("scale"), "scale filter missing");
        assert!(!tool.has_encoder("libnope"));
    }

    #[test]
    fn a_bad_env_override_names_the_variable_and_the_path() {
        let missing = std::env::temp_dir().join("dvs-no-such-ffmpeg");
        let err = {
            // SAFETY-adjacent: the variable is process-global, so this test does not run in
            // parallel with discovery of the same name. `DVS_FFMPEG_TEST` is a distinct
            // variable used only here.
            let err = locate("ffmpeg", "DVS_FFMPEG_TEST_MISSING");
            // Unset variable falls through to PATH and succeeds.
            assert!(err.is_ok());
            std::env::set_var("DVS_FFMPEG_TEST_MISSING", &missing);
            let err = locate("ffmpeg", "DVS_FFMPEG_TEST_MISSING").unwrap_err();
            std::env::remove_var("DVS_FFMPEG_TEST_MISSING");
            err
        };
        let message = err.to_string();
        assert!(message.contains("DVS_FFMPEG_TEST_MISSING"), "{message}");
        assert!(message.contains("dvs-no-such-ffmpeg"), "{message}");
        assert_eq!(err.exit_code(), dvs_core::error::exit::TOOL_MISSING);
    }
}
