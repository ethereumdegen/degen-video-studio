//! End-to-end tests against the built `dvs` binary, on real media.
//!
//! These run the binary the way an agent does — argv in, JSON and an exit code out — so
//! they cover the two things unit tests in the library crates cannot: the argument
//! plumbing (`--json` reaching every command, unknown flags failing as *bad arguments*
//! rather than being dropped) and the exit-code contract a caller branches on.
//!
//! Media is synthesized with ffmpeg rather than mocked. The P0/P1 acceptance path in
//! `PLAN.md` §10 is "new → import → edit → render a playable file", and a mocked decoder
//! would prove none of it.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// `30000/1001` is the frame rate that breaks naive time handling, so the tests that care
/// about snapping use it rather than a friendly 30.
const NDF: &str = "30000/1001";

struct Run {
    output: Output,
}

impl Run {
    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.output.stdout).to_string()
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).to_string()
    }

    fn code(&self) -> i32 {
        self.output.status.code().expect("dvs was not signalled")
    }

    fn ok(self) -> Run {
        assert_eq!(
            self.code(),
            0,
            "expected success\nstdout: {}\nstderr: {}",
            self.stdout(),
            self.stderr()
        );
        self
    }

    /// The `--json` contract: exactly one JSON object on stdout.
    fn json(&self) -> Value {
        let text = self.stdout();
        serde_json::from_str(&text).unwrap_or_else(|error| {
            panic!("stdout was not one JSON object ({error})\nstdout: {text}\nstderr: {}", self.stderr())
        })
    }
}

fn dvs(cwd: &Path, args: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_dvs"))
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("dvs binary runs");
    Run { output }
}

/// The same binary with nothing on `PATH`, for the one exit code that is about the
/// machine rather than the request.
fn dvs_without_ffmpeg(cwd: &Path, args: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_dvs"))
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/nonexistent")
        .args(args)
        .output()
        .expect("dvs binary runs");
    Run { output }
}

/// A real, decodable file: colour bars plus a tone, constant frame rate, yuv420p.
fn synthesize(dir: &Path, name: &str, seconds: f64, fps: &str) -> PathBuf {
    let path = dir.join(name);
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-nostdin",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=size=320x240:rate={fps}:duration={seconds}"),
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:duration={seconds}"),
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
            "-shortest",
        ])
        .arg(&path)
        .status()
        .expect("ffmpeg runs");
    assert!(status.success(), "ffmpeg could not synthesize {name}");
    path
}

fn probe_duration(path: &Path) -> f64 {
    let output = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("ffprobe runs");
    assert!(output.status.success(), "ffprobe failed on {}", path.display());
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("ffprobe gave no duration for {}", path.display()))
}

/// A project with one video track, ready for edits.
fn project(dir: &Path, fps: &str) -> PathBuf {
    dvs(dir, &["new", "promo", "--fps", fps, "--size", "320x240"]).ok();
    let root = dir.join("promo");
    dvs(&root, &["op", "track.add", "--kind", "video"]).ok();
    root
}

/// `project.json` with the write stamp removed. `modified` is an mtime, not document
/// state: the engine bumps it on every save and undo deliberately does not rewind it, so
/// "byte-for-byte identical" means identical modulo that one field.
fn document(root: &Path) -> Value {
    let bytes = std::fs::read(root.join("project.json")).expect("project.json exists");
    let mut value: Value = serde_json::from_slice(&bytes).expect("project.json is JSON");
    value
        .as_object_mut()
        .expect("the document is an object")
        .remove("modified");
    value
}

// ---------------------------------------------------------------- P0/P1 acceptance

/// `PLAN.md` §10, P0 and P1: new → import → edit → a playable file whose length is the
/// length of the timeline.
#[test]
fn new_import_insert_render_produces_a_playable_file() {
    let dir = tempfile::tempdir().unwrap();
    let media = synthesize(dir.path(), "talk.mp4", 3.0, "30");
    let root = project(dir.path(), "30");

    let imported = dvs(
        &root,
        &["--json", "asset", "import", media.to_str().unwrap()],
    )
    .ok()
    .json();
    let asset = imported["created"][0]
        .as_str()
        .expect("import reports the asset id")
        .to_string();

    dvs(
        &root,
        &[
            "--json", "op", "clip.insert", "--track", "V1", "--source", &asset, "--at", "0",
            "--duration", "2",
        ],
    )
    .ok();

    let out = root.join("out.mp4");
    let report = dvs(
        &root,
        &["--json", "render", out.to_str().unwrap(), "--preset", "ultrafast"],
    )
    .ok()
    .json();

    assert!(out.is_file(), "render wrote no file");
    assert_eq!(report["frames"], serde_json::json!(60), "2 s at 30 fps");
    let probed = probe_duration(&out);
    assert!(
        (probed - 2.0).abs() < 0.1,
        "rendered file is {probed} s, timeline is 2 s"
    );
}

// ---------------------------------------------------------------- the JSON contract

#[test]
fn every_command_can_answer_in_json() {
    let dir = tempfile::tempdir().unwrap();
    let media = synthesize(dir.path(), "talk.mp4", 2.0, "30");

    let created = dvs(dir.path(), &["--json", "new", "promo", "--size", "320x240"])
        .ok()
        .json();
    assert_eq!(created["name"], serde_json::json!("promo"));
    let root = dir.path().join("promo");

    dvs(&root, &["--json", "op", "track.add", "--kind", "video"])
        .ok()
        .json();
    dvs(
        &root,
        &["--json", "asset", "import", media.to_str().unwrap()],
    )
    .ok()
    .json();

    let assets = dvs(&root, &["--json", "asset", "list"]).ok().json();
    assert_eq!(assets["count"], serde_json::json!(1));

    let id = assets["assets"][0]["id"].as_str().unwrap().to_string();
    dvs(
        &root,
        &[
            "--json", "op", "clip.insert", "--track", "V1", "--source", &id, "--at", "0",
            "--duration", "1",
        ],
    )
    .ok()
    .json();

    let out = root.join("out.mp4");
    dvs(
        &root,
        &["--json", "render", out.to_str().unwrap(), "--preset", "ultrafast"],
    )
    .ok()
    .json();

    let digest = dvs(&root, &["--json", "digest"]).ok().json();
    assert!(digest.is_object(), "digest must be one object");

    // Lint may or may not find something here; either way its stdout is one object.
    let lint = dvs(&root, &["--json", "lint"]);
    assert!(lint.json()["counts"].is_object());

    let history = dvs(&root, &["--json", "history"]).ok().json();
    assert!(history["entries"].is_array());

    let doctor = dvs(&root, &["--json", "doctor"]).ok().json();
    assert!(doctor["ffmpeg"]["version"].is_string());
}

/// `doctor` is how an agent finds out what this machine can do; the ffmpeg version is the
/// part that decides whether an encode is reproducible.
#[test]
fn doctor_names_the_local_ffmpeg() {
    let dir = tempfile::tempdir().unwrap();
    let root = project(dir.path(), "30");
    let report = dvs(&root, &["--json", "doctor"]).ok().json();

    let version = report["ffmpeg"]["version"].as_str().unwrap();
    assert!(
        version.contains("ffmpeg version"),
        "doctor reported '{version}'"
    );
    assert!(report["ffmpeg"]["ffprobe"].as_str().unwrap().contains("ffprobe"));
    assert!(report["whisper"]["compiled"].is_boolean());
    assert!(report["project"]["format"].is_number());
}

/// With no ffmpeg, `doctor` still answers — that is the whole point of asking it — but
/// exits 5 so a caller stops before discovering the same thing one render at a time.
#[test]
fn doctor_exits_five_when_ffmpeg_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let root = project(dir.path(), "30");
    let run = dvs_without_ffmpeg(&root, &["--json", "doctor"]);
    assert_eq!(run.code(), 5, "stderr: {}", run.stderr());

    let report = run.json();
    assert_eq!(
        report["ffmpeg"]["error"]["kind"],
        serde_json::json!("tool-missing")
    );
    assert!(
        report["ffmpeg"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("DVS_FFMPEG"),
        "the report must say how to fix it"
    );
    let error: Value = serde_json::from_str(run.stderr().trim()).expect("errors are JSON too");
    assert_eq!(error["error"]["code"], serde_json::json!(5));
}

// ---------------------------------------------------------------- exit codes

#[test]
fn a_bad_flag_is_exit_two() {
    let dir = tempfile::tempdir().unwrap();
    let root = project(dir.path(), "30");
    assert_eq!(dvs(&root, &["--bogus"]).code(), 2);
    assert_eq!(dvs(&root, &["lint", "--profile", "cinema"]).code(), 2);
}

/// Lint exits 4 when it finds something fatal, and the findings are still on stdout: a
/// caller that branches on the code also wants to read what broke. The fixture nests a
/// two-second sequence, then trims it to one second behind the clip that uses it, which
/// is exactly the mistake `past-source-end` exists to catch — and it needs no media, so
/// the check is document-only.
#[test]
fn lint_exits_four_when_a_finding_is_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let root = project(dir.path(), "30");

    dvs(&root, &["op", "seq.new", "--name", "inner"]).ok();
    dvs(&root, &["--seq", "inner", "op", "track.add", "--kind", "video"]).ok();
    dvs(
        &root,
        &[
            "--seq", "inner", "op", "clip.insert", "--track", "V1", "--source",
            "color:#ffffff", "--at", "0", "--duration", "2", "--name", "bed",
        ],
    )
    .ok();
    dvs(
        &root,
        &[
            "op", "clip.insert", "--track", "V1", "--source", "seq:inner", "--at", "0",
            "--duration", "2", "--name", "nested",
        ],
    )
    .ok();

    let clean = dvs(&root, &["--json", "lint", "--no-render"]);
    assert_eq!(clean.code(), 0, "nothing is wrong yet: {}", clean.stdout());

    // Shorten what the nested clip is reading from, behind its back.
    dvs(
        &root,
        &["--seq", "inner", "op", "clip.trim", "--target", "#bed", "--out", "1"],
    )
    .ok();

    let run = dvs(&root, &["--json", "lint", "--no-render"]);
    assert_eq!(run.code(), 4, "stdout: {}", run.stdout());
    let report = run.json();
    assert_eq!(report["counts"]["error"], serde_json::json!(1));
    assert_eq!(
        report["findings"][0]["rule"],
        serde_json::json!("past-source-end")
    );
    assert_eq!(
        report["findings"][0]["severity"],
        serde_json::json!("error")
    );
}

/// An unknown argument to a known op must fail loudly and name the arguments that exist;
/// a dropped flag would leave the agent believing in an edit that never happened.
#[test]
fn an_unknown_op_argument_is_exit_two_and_names_the_real_ones() {
    let dir = tempfile::tempdir().unwrap();
    let root = project(dir.path(), "30");
    let run = dvs(&root, &["--json", "op", "clip.split", "--when", "1"]);
    assert_eq!(run.code(), 2);
    let error: Value = serde_json::from_str(run.stderr().trim()).expect("errors are JSON too");
    let message = error["error"]["message"].as_str().unwrap();
    assert!(message.contains("clip"), "{message}");
    assert!(message.contains("at"), "{message}");
}

#[test]
fn a_selector_that_matches_nothing_is_exit_three() {
    let dir = tempfile::tempdir().unwrap();
    let root = project(dir.path(), "30");
    let run = dvs(&root, &["--json", "op", "clip.split", "--target", "#nope", "--at", "1"]);
    assert_eq!(run.code(), 3);
    let error: Value = serde_json::from_str(run.stderr().trim()).unwrap();
    assert_eq!(error["error"]["kind"], serde_json::json!("no-match"));
}

/// A wrong verb must cost one round trip: exit 3 and a list of ops that really exist, in
/// the namespace that was asked for. The candidate list is checked against `op --list`
/// rather than a hardcoded name, so a suggestion that no longer resolves fails here.
#[test]
fn an_unknown_op_is_exit_three_and_names_real_op_ids() {
    let dir = tempfile::tempdir().unwrap();
    let root = project(dir.path(), "30");
    let run = dvs(&root, &["--json", "op", "clip.cut", "--at", "1"]);
    assert_eq!(run.code(), 3);

    let error: Value = serde_json::from_str(run.stderr().trim()).unwrap();
    assert_eq!(error["error"]["kind"], serde_json::json!("no-match"));
    let candidates: Vec<String> = error["error"]["candidates"]
        .as_array()
        .expect("candidates are listed")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    assert!(!candidates.is_empty(), "no candidates were offered");

    let catalog = dvs(&root, &["--json", "op", "--list"]).ok().json();
    let known: Vec<String> = catalog["ops"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|op| op["id"].as_str())
        .map(str::to_string)
        .collect();
    for candidate in &candidates {
        assert!(
            known.contains(candidate),
            "suggested op '{candidate}' is not registered"
        );
        assert!(
            candidate.starts_with("clip."),
            "suggested op '{candidate}' is from another namespace"
        );
    }
}

// ---------------------------------------------------------------- frame snapping

/// The property `PLAN.md` §5 promises by name: an agent asks for 42.5 s on a 30000/1001
/// timeline and is told it got frame 1274, rather than discovering the half-frame later.
#[test]
fn a_split_reports_the_frame_it_snapped_to() {
    let dir = tempfile::tempdir().unwrap();
    let root = project(dir.path(), NDF);
    dvs(
        &root,
        &[
            "op", "clip.insert", "--track", "V1", "--source", "color:#204080", "--at", "0",
            "--duration", "60", "--name", "bed",
        ],
    )
    .ok();

    let applied = dvs(
        &root,
        &["--json", "op", "clip.split", "--target", "#bed", "--at", "42.5"],
    )
    .ok()
    .json();

    let snap = &applied["snapped"][0];
    assert_eq!(snap["field"], serde_json::json!("at"));
    assert_eq!(snap["frame"], serde_json::json!(1274));
    assert_eq!(snap["requested"], serde_json::json!("85/2"));
    assert_eq!(
        snap["applied"],
        serde_json::json!("637637/15000"),
        "1274 frames at 30000/1001 is 1274*1001/30000 s, reduced"
    );
    assert_eq!(applied["created"].as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------- undo

#[test]
fn undo_restores_the_document_and_shows_up_in_history() {
    let dir = tempfile::tempdir().unwrap();
    let root = project(dir.path(), "30");
    dvs(
        &root,
        &[
            "op", "clip.insert", "--track", "V1", "--source", "color:#ffffff", "--at", "0",
            "--duration", "5", "--name", "bed",
        ],
    )
    .ok();
    let before = document(&root);

    dvs(&root, &["op", "marker.add", "--at", "2", "--name", "beat"]).ok();
    assert_ne!(document(&root), before, "the marker must have landed");

    let undone = dvs(&root, &["--json", "undo"]).ok().json();
    assert_eq!(undone["undone"], serde_json::json!("marker.add"));
    assert_eq!(
        document(&root),
        before,
        "undo must restore the document exactly"
    );

    let history = dvs(&root, &["--json", "history", "-n", "5"]).ok().json();
    let ops: Vec<&str> = history["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["op"].as_str())
        .collect();
    assert_eq!(
        ops.last(),
        Some(&"project.undo"),
        "the undo is itself an event in the journal: {ops:?}"
    );

    let redone = dvs(&root, &["--json", "redo"]).ok().json();
    assert_eq!(redone["redone"], serde_json::json!("marker.add"));
    assert_ne!(document(&root), before, "redo must put the marker back");
}

// ---------------------------------------------------------------- discovery

#[test]
fn op_list_and_schema_publish_the_real_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let root = project(dir.path(), "30");

    let catalog = dvs(&root, &["--json", "op", "--list"]).ok().json();
    let ops = catalog["ops"].as_array().expect("a list of ops");
    assert!(ops.len() > 40, "only {} ops registered", ops.len());
    let split = ops
        .iter()
        .find(|op| op["id"] == serde_json::json!("clip.split"))
        .expect("clip.split is in the catalog");
    assert_eq!(split["mcpTool"], serde_json::json!("clip_split"));
    assert!(split["schema"]["properties"]["at"].is_object());

    let schema = dvs(&root, &["--json", "schema", "--op", "clip.split"])
        .ok()
        .json();
    let properties = schema["schema"]["properties"]
        .as_object()
        .expect("clip.split declares properties");
    assert!(properties.contains_key("target"));
    assert!(properties.contains_key("at"));

    let document_schema = dvs(&root, &["--json", "schema", "--project-schema"])
        .ok()
        .json();
    assert!(
        document_schema["properties"]["sequences"].is_object()
            || document_schema["$defs"].is_object(),
        "the project schema must describe the document"
    );
}

// ---------------------------------------------------------------- dry run

#[test]
fn a_dry_run_reports_the_effect_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = project(dir.path(), "30");
    let before = std::fs::read(root.join("project.json")).unwrap();
    let history_before = std::fs::read(root.join("history.jsonl")).unwrap_or_default();

    let applied = dvs(
        &root,
        &[
            "--json", "--dry-run", "op", "clip.insert", "--track", "V1", "--source",
            "color:#101010", "--at", "0", "--duration", "3",
        ],
    )
    .ok()
    .json();
    assert_eq!(applied["dryRun"], serde_json::json!(true));
    assert_eq!(
        applied["created"].as_array().map(Vec::len),
        Some(1),
        "a dry run still reports what it would create"
    );
    assert!(applied["seq"].is_null(), "a dry run earns no journal entry");

    assert_eq!(
        std::fs::read(root.join("project.json")).unwrap(),
        before,
        "--dry-run wrote to project.json"
    );
    assert_eq!(
        std::fs::read(root.join("history.jsonl")).unwrap_or_default(),
        history_before,
        "--dry-run wrote to the journal"
    );
}

/// The reverse of the dry run, so the test above cannot pass by the op silently failing.
#[test]
fn the_same_op_without_dry_run_does_write() {
    let dir = tempfile::tempdir().unwrap();
    let root = project(dir.path(), "30");
    let before = std::fs::read(root.join("project.json")).unwrap();
    dvs(
        &root,
        &[
            "op", "clip.insert", "--track", "V1", "--source", "color:#101010", "--at", "0",
            "--duration", "3",
        ],
    )
    .ok();
    assert_ne!(std::fs::read(root.join("project.json")).unwrap(), before);
}
