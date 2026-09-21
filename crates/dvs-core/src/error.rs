//! Structured errors, because the primary operator is a machine.
//!
//! Two properties matter. First, the exit code is part of the contract, so a shell script or
//! an agent loop can branch without parsing prose. Second, a failure that lists the real
//! candidates (`did you mean '#talk'? (V1: #talk, #title-1)`) saves a round trip, which is
//! the expensive resource in an agent session.

use serde::Serialize;
use std::fmt;
use std::path::Path;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Exit codes. Documented in the CLI reference and depended on by callers.
pub mod exit {
    pub const OK: i32 = 0;
    pub const OP_ERROR: i32 = 1;
    pub const BAD_ARGS: i32 = 2;
    pub const NO_MATCH: i32 = 3;
    pub const LINT: i32 = 4;
    pub const TOOL_MISSING: i32 = 5;
    pub const BUDGET: i32 = 6;
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The op was well-formed but cannot be applied to this document.
    #[error("{0}")]
    Op(String),

    /// Arguments failed schema or semantic validation.
    #[error("{0}")]
    BadArgs(String),

    /// A selector or a name resolved to nothing.
    #[error("{kind} '{query}' matched nothing{}", candidate_hint(.candidates))]
    NoMatch {
        kind: &'static str,
        query: String,
        candidates: Vec<String>,
    },

    /// Lint findings are present and the caller asked for a hard failure.
    #[error("{count} lint finding(s): {summary}")]
    Lint { count: usize, summary: String },

    /// An external tool (ffmpeg, ffprobe, a whisper model) is missing or unusable.
    #[error("{tool} unavailable: {detail}")]
    Tool { tool: String, detail: String },

    /// An AI provider budget or quota stopped the request.
    #[error("{0}")]
    Budget(String),

    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid json in {path}: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}

fn candidate_hint(candidates: &[String]) -> String {
    if candidates.is_empty() {
        String::new()
    } else {
        format!("; candidates: {}", candidates.join(", "))
    }
}

impl Error {
    pub fn op(message: impl Into<String>) -> Self {
        Error::Op(message.into())
    }

    pub fn bad_args(message: impl Into<String>) -> Self {
        Error::BadArgs(message.into())
    }

    pub fn tool(tool: impl Into<String>, detail: impl Into<String>) -> Self {
        Error::Tool {
            tool: tool.into(),
            detail: detail.into(),
        }
    }

    pub fn budget(message: impl Into<String>) -> Self {
        Error::Budget(message.into())
    }

    pub fn no_match(kind: &'static str, query: impl Into<String>, candidates: Vec<String>) -> Self {
        Error::NoMatch {
            kind,
            query: query.into(),
            candidates,
        }
    }

    pub fn io(path: impl AsRef<Path>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.as_ref().display().to_string(),
            source,
        }
    }

    pub fn json(path: impl AsRef<Path>, source: serde_json::Error) -> Self {
        Error::Json {
            path: path.as_ref().display().to_string(),
            source,
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Op(_) | Error::Io { .. } | Error::Json { .. } => exit::OP_ERROR,
            Error::BadArgs(_) => exit::BAD_ARGS,
            Error::NoMatch { .. } => exit::NO_MATCH,
            Error::Lint { .. } => exit::LINT,
            Error::Tool { .. } => exit::TOOL_MISSING,
            Error::Budget(_) => exit::BUDGET,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Error::Op(_) => "op",
            Error::BadArgs(_) => "bad-args",
            Error::NoMatch { .. } => "no-match",
            Error::Lint { .. } => "lint",
            Error::Tool { .. } => "tool-missing",
            Error::Budget(_) => "budget",
            Error::Io { .. } => "io",
            Error::Json { .. } => "json",
        }
    }

    /// The `--json` shape. Stable: agents match on `kind` and read `candidates`.
    pub fn to_report(&self) -> ErrorReport {
        ErrorReport {
            kind: self.kind(),
            code: self.exit_code(),
            message: self.to_string(),
            candidates: match self {
                Error::NoMatch { candidates, .. } => candidates.clone(),
                _ => Vec::new(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorReport {
    pub kind: &'static str,
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<String>,
}

impl fmt::Display for ErrorReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_selector_names_the_alternatives() {
        let err = Error::no_match(
            "clip",
            "#tlk",
            vec!["#talk".into(), "#title-1".into()],
        );
        assert_eq!(err.exit_code(), exit::NO_MATCH);
        assert_eq!(
            err.to_string(),
            "clip '#tlk' matched nothing; candidates: #talk, #title-1"
        );
        assert_eq!(err.to_report().candidates.len(), 2);
    }

    #[test]
    fn exit_codes_are_distinct_per_failure_class() {
        let codes = [
            Error::op("x").exit_code(),
            Error::bad_args("x").exit_code(),
            Error::no_match("clip", "x", vec![]).exit_code(),
            Error::Lint {
                count: 1,
                summary: "gap".into(),
            }
            .exit_code(),
            Error::tool("ffmpeg", "not found").exit_code(),
            Error::budget("x").exit_code(),
        ];
        let mut sorted = codes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), codes.len(), "exit codes collide: {codes:?}");
    }
}
