//! Where the answer goes.
//!
//! The contract an agent depends on: with `--json`, **exactly one** JSON object reaches
//! stdout and nothing else does — no progress lines, no warnings, no "wrote out.mp4".
//! Anything the run wants to say about itself goes to stderr, where a pipe into `jq`
//! cannot trip over it. Without `--json`, the same information is printed as text and the
//! JSON is not produced at all.
//!
//! Errors are the mirror image: the structured report on stderr under `--json`, a plain
//! message otherwise, and in both cases the exit code carries the same meaning.

use dvs_core::error::Error;
use serde_json::Value;
use std::io::Write;

#[derive(Debug, Clone, Copy)]
pub struct Out {
    json: bool,
    quiet: bool,
}

impl Out {
    pub fn new(json: bool, quiet: bool) -> Out {
        Out { json, quiet }
    }

    pub fn is_json(self) -> bool {
        self.json
    }

    pub fn is_quiet(self) -> bool {
        self.quiet
    }

    /// The command's answer. `human` is only formatted when it will be printed, so a
    /// summary that costs work does not cost it in `--json` mode.
    pub fn report(self, value: &Value, human: impl FnOnce() -> String) {
        if self.json {
            let text = serde_json::to_string_pretty(value)
                .unwrap_or_else(|error| format!("{{\"error\":\"unserializable report: {error}\"}}"));
            write_line(&text);
            return;
        }
        if self.quiet {
            return;
        }
        let text = human();
        if !text.is_empty() {
            write_line(&text);
        }
    }

    /// A line about the run rather than its result: goes to stderr so it never pollutes a
    /// piped answer, and is silent under `-q`.
    pub fn note(self, text: impl AsRef<str>) {
        if self.quiet {
            return;
        }
        let _ = writeln!(std::io::stderr(), "{}", text.as_ref());
    }

    /// The failure path. `-q` does not suppress it: a silent failure is the one thing a
    /// caller can never recover from.
    pub fn fail(self, error: &Error) {
        let report = error.to_report();
        let mut stderr = std::io::stderr();
        if self.json {
            let body = serde_json::json!({ "error": report });
            let text = serde_json::to_string(&body)
                .unwrap_or_else(|_| format!("{{\"error\":{{\"message\":\"{}\"}}}}", report.message));
            let _ = writeln!(stderr, "{text}");
            return;
        }
        let _ = writeln!(stderr, "error: {}", report.message);
        if !report.candidates.is_empty() {
            let _ = writeln!(stderr, "  candidates: {}", report.candidates.join(", "));
        }
    }
}

/// Write one line to stdout, exiting quietly when the reader has gone away.
///
/// `println!` panics on a closed pipe, so `dvs render --json | head -5` ends in a Rust
/// panic message instead of the output the caller asked for. A broken pipe is the reader's
/// decision, not an error in this process.
fn write_line(text: &str) {
    use std::io::ErrorKind;
    let mut stdout = std::io::stdout().lock();
    match writeln!(stdout, "{text}").and_then(|()| stdout.flush()) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::BrokenPipe => std::process::exit(0),
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "error: cannot write to stdout: {error}");
            std::process::exit(dvs_core::error::exit::OP_ERROR);
        }
    }
}

/// Render a list as `a, b, c`, or `-` when it is empty. Used by every human summary, so
/// "nothing changed" reads the same everywhere.
pub fn list(items: &[String]) -> String {
    if items.is_empty() {
        "-".to_string()
    } else {
        items.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_lists_read_as_a_dash() {
        assert_eq!(list(&[]), "-");
        assert_eq!(list(&["a".to_string(), "b".to_string()]), "a, b");
    }

}
