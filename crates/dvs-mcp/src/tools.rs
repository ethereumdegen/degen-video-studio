//! Tool names, tool definitions, and the loop tools themselves.
//!
//! Two things live here that both front ends need. The first is the *name mapping*: an op
//! id is dotted and kebab-cased (`title.set-text`), an MCP tool name is an identifier
//! (`title_set_text`). The mapping is lossy, so [`tool_name`] is paired with
//! [`op_for_tool`] and with a test that the whole registry maps injectively — a silent
//! collision would route `tools/call` to the wrong op.
//!
//! The second is the loop tools' implementations, as plain synchronous functions taking
//! typed arguments. `dvs frame` and the MCP `dvs_frame` tool are the same code with
//! different argument plumbing; writing them twice is how the two surfaces start
//! disagreeing about what a frame index means.

use dvs_core::engine::Workspace;
use dvs_core::error::{Error, Result};
use dvs_core::ids::SequenceId;
use dvs_core::op::{Op, Registry};
use dvs_core::time::{Fps, Span, Time};
use dvs_inspect::{Finding, Severity};
use dvs_media::Toolchain;
use rmcp::model::{JsonObject, Tool};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The hand-written tools from `PLAN.md` §5. They are not ops: each one is several ops or
/// several analyses fused into a single round trip, which is the resource an agent is
/// actually short of.
pub const LOOP_TOOLS: &[&str] = &[
    "dvs_overview",
    "dvs_apply",
    "dvs_render",
    "dvs_lint",
    "dvs_frame",
    "dvs_transcript",
    "dvs_history",
];

/// MCP tool name for an op id: `clip.split` → `clip_split`, `title.set-text` →
/// `title_set_text`.
///
/// Both separators collapse to `_` because the tool-name grammar an agent can rely on is
/// `[a-z0-9_]+` — dots and dashes survive the MCP spec but not every client's function
/// naming rules, and a name that needs quoting is a name that gets mistyped.
pub fn tool_name(op_id: &str) -> String {
    op_id
        .chars()
        .map(|c| if c == '.' || c == '-' { '_' } else { c })
        .collect()
}

/// The op a tool name refers to. The reverse of [`tool_name`] cannot be computed (both
/// separators became `_`), so it is a lookup over the registry, and a miss reports the
/// names that do exist rather than "unknown tool".
pub fn op_for_tool<'a>(registry: &'a Registry, tool: &str) -> Result<&'a Arc<dyn Op>> {
    let id = registry
        .ids()
        .into_iter()
        .find(|id| tool_name(id) == tool)
        .ok_or_else(|| {
            let candidates = registry
                .suggestions(&tool.replace('_', "."))
                .into_iter()
                .map(|id| tool_name(&id))
                .collect();
            Error::no_match("tool", tool, candidates)
        })?;
    registry.get(id)
}

/// Everything `tools/list` advertises: the loop tools first, because they are what an
/// agent should reach for before driving ninety ops by hand, then one tool per op.
pub fn full_catalog(registry: &Registry) -> Vec<Tool> {
    let mut tools = loop_tools();
    tools.extend(registry.catalog().into_iter().map(|info| {
        Tool::new(
            tool_name(info.id),
            info.about,
            schema_object(info.schema),
        )
    }));
    tools
}

/// A JSON Schema as rmcp wants it. A schema that is not an object would make the tool
/// uncallable, so a malformed one degrades to "any object" rather than dropping the tool.
fn schema_object(schema: Value) -> JsonObject {
    match schema {
        Value::Object(map) => map,
        _ => match json!({ "type": "object" }) {
            Value::Object(map) => map,
            _ => unreachable!("object literal is an object"),
        },
    }
}

fn time_field(description: &str) -> Value {
    json!({
        "type": ["string", "number"],
        "description": format!("{description} — seconds, 'num/den', 'mm:ss.mmm', '1m12s' or '1800f'")
    })
}

fn loop_tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "dvs_overview",
            "Project at a glance: sequences, tracks, clip counts, assets, history depth and the local ffmpeg. Start here.",
            schema_object(json!({
                "type": "object",
                "properties": {
                    "seq": { "type": "string", "description": "sequence id or name; default is the active one" }
                },
                "additionalProperties": false
            })),
        ),
        Tool::new(
            "dvs_apply",
            "Apply a batch of ops as one transaction: all of them land or none do, and one digest comes back.",
            schema_object(json!({
                "type": "object",
                "required": ["ops"],
                "properties": {
                    "ops": {
                        "type": "array",
                        "description": "ops in order; each is applied to the result of the previous one",
                        "items": {
                            "type": "object",
                            "required": ["op"],
                            "properties": {
                                "op": { "type": "string", "description": "op id, e.g. 'clip.split'" },
                                "args": { "type": "object", "description": "arguments for that op" },
                                "seq": { "type": "string", "description": "sequence for this op; default is the active one" }
                            },
                            "additionalProperties": false
                        }
                    },
                    "dry-run": { "type": "boolean", "description": "report the effects without writing anything" },
                    "digest": { "type": "boolean", "description": "return a digest of the resulting timeline (renders sampled frames)" }
                },
                "additionalProperties": false
            })),
        ),
        Tool::new(
            "dvs_render",
            "Render the sequence to a file and return the render report, its digest, and optionally a contact sheet — one call instead of three.",
            schema_object(json!({
                "type": "object",
                "required": ["output"],
                "properties": {
                    "output": { "type": "string", "description": "output file, e.g. 'out.mp4'" },
                    "seq": { "type": "string", "description": "sequence id or name" },
                    "range": { "type": "string", "description": "'start-end' on the timeline, e.g. '0-30' or '00:10-00:20'" },
                    "scale": { "type": "number", "description": "output scale; 0.5 renders half size" },
                    "encoder": { "type": "string", "description": "auto, x264, x265, av1, nvenc, vaapi, videotoolbox, prores, or an ffmpeg encoder name" },
                    "quality": { "type": "integer", "description": "CRF or the encoder's equivalent; lower is better" },
                    "preset": { "type": "string", "description": "encoder preset" },
                    "proxy": { "type": "boolean", "description": "decode from proxies: fast and lower quality" },
                    "no-cache": { "type": "boolean", "description": "re-encode every segment, ignoring the segment cache" },
                    "sheet": { "type": "string", "description": "also write a contact sheet to this path" },
                    "sheet-every": time_field("contact sheet sampling interval"),
                    "sheet-columns": { "type": "integer", "description": "contact sheet columns" },
                    "sheet-width": { "type": "integer", "description": "width of the whole contact sheet in pixels; cells are this divided by sheet-columns" }
                },
                "additionalProperties": false
            })),
        ),
        Tool::new(
            "dvs_lint",
            "Check the timeline against the mistakes a blind editor makes: gaps, overlaps, upscales, loudness, captions, titles.",
            schema_object(json!({
                "type": "object",
                "properties": {
                    "seq": { "type": "string", "description": "sequence id or name" },
                    "profile": { "type": "string", "enum": ["youtube", "podcast", "broadcast"], "description": "loudness target; default youtube (-14 LUFS)" },
                    "render": { "type": "boolean", "description": "render sampled frames for the pixel rules; false is document-only and needs no ffmpeg" }
                },
                "additionalProperties": false
            })),
        ),
        Tool::new(
            "dvs_frame",
            "Render one frame as a PNG and return it as an image, with the clip and title boxes that are in it and their ids.",
            schema_object(json!({
                "type": "object",
                "required": ["at"],
                "properties": {
                    "at": time_field("timeline position of the frame"),
                    "seq": { "type": "string", "description": "sequence id or name" },
                    "output": { "type": "string", "description": "where to write the PNG; default is under the project cache" },
                    "annotate": { "type": "boolean", "description": "overlay clip/title boxes with their ids; default true" },
                    "scale": { "type": "number", "description": "output scale; 0.5 renders half size" }
                },
                "additionalProperties": false
            })),
        ),
        Tool::new(
            "dvs_transcript",
            "Query a word-timestamped transcript: find a phrase and get timeline ranges, or read the words inside a span.",
            schema_object(json!({
                "type": "object",
                "required": ["asset"],
                "properties": {
                    "asset": { "type": "string", "description": "asset id or name whose transcript to read" },
                    "phrase": { "type": "string", "description": "phrase to locate; returns one span per occurrence" },
                    "span": { "type": "string", "description": "'start-end' in source time; returns the words inside it" },
                    "limit": { "type": "integer", "description": "maximum words or matches to return; default 200" }
                },
                "additionalProperties": false
            })),
        ),
        Tool::new(
            "dvs_history",
            "The op journal, newest last: what was applied, by whom, and what an undo would reverse.",
            schema_object(json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "description": "how many entries to return; default 20" }
                },
                "additionalProperties": false
            })),
        ),
    ]
}

/// `start-end`, the range syntax both surfaces accept (`--range`, `"range"`).
///
/// Split on the first `-` rather than the last: every accepted time spelling is
/// non-negative on a timeline, so a `-` can only be the separator, and splitting on the
/// first one keeps `00:10-00:20` unambiguous.
pub fn parse_span(text: &str, fps: Fps) -> Result<Span> {
    let (start, end) = text.split_once('-').ok_or_else(|| {
        Error::bad_args(format!(
            "range '{text}' needs a start and an end, as 'start-end' (e.g. '0-30' or '00:10-00:20')"
        ))
    })?;
    let span = Span::new(
        Time::parse_with_fps(start.trim(), fps)?,
        Time::parse_with_fps(end.trim(), fps)?,
    );
    if span.is_empty() {
        return Err(Error::bad_args(format!(
            "range '{text}' ends at or before it starts"
        )));
    }
    Ok(span)
}

/// What the project contains, without rendering a pixel.
///
/// The counts and durations here are what an agent needs before its first edit: which
/// sequence is active, what is on each track, which assets resolve. `tool` is optional so
/// the overview still answers on a machine with no ffmpeg — the one case where an agent
/// most needs to be told what is wrong.
pub fn overview(workspace: &Workspace, tool: Option<&Toolchain>) -> Result<Value> {
    let project = &workspace.project;
    let sequences: Vec<Value> = project
        .sequences
        .values()
        .map(|sequence| {
            let tracks: Vec<Value> = sequence
                .tracks
                .iter()
                .map(|track| {
                    json!({
                        "id": track.id,
                        "name": track.name,
                        "kind": track.kind,
                        "clips": track.clips.len(),
                        "cues": track.cues.len(),
                        "muted": track.muted,
                        "locked": track.locked,
                        "duration": track.clips.last().map(|clip| clip.end()),
                    })
                })
                .collect();
            json!({
                "id": sequence.id,
                "name": sequence.name,
                "fps": sequence.fps,
                "size": sequence.size,
                "sampleRate": sequence.sample_rate,
                "duration": sequence.duration(),
                "frames": sequence.frame_count(),
                "timecode": sequence.duration().timecode(sequence.fps),
                "tracks": tracks,
                "markers": sequence.markers.len(),
                "active": sequence.id == project.active_sequence,
            })
        })
        .collect();

    let assets: Vec<Value> = project
        .assets
        .values()
        .map(|asset| {
            json!({
                "id": asset.id,
                "name": asset.name,
                "kind": asset.kind,
                "duration": asset.probe.duration,
                "size": asset.probe.video.as_ref().map(|video| video.size),
                "fps": asset.probe.video.as_ref().map(|video| video.fps),
                "vfr": asset.probe.vfr,
                "proxy": asset.proxy.is_some(),
                "present": workspace.assets.exists(&asset.hash),
            })
        })
        .collect();

    let entries = workspace.journal.entries();
    Ok(json!({
        "project": {
            "id": project.id,
            "name": project.name,
            "format": project.format,
            "root": workspace.paths.root().display().to_string(),
            "activeSequence": project.active_sequence,
            "modified": project.modified,
        },
        "sequences": sequences,
        "assets": assets,
        "history": {
            "entries": entries.len(),
            "last": entries
                .iter()
                .rev()
                .take(5)
                .map(|entry| json!({ "seq": entry.seq, "op": entry.op, "actor": entry.actor }))
                .collect::<Vec<_>>(),
        },
        "tools": tool.map(|tool| tool.report()),
    }))
}

/// The journal tail, oldest first, with the ops an undo would reverse marked.
pub fn history(workspace: &Workspace, limit: usize) -> Value {
    let undoable = workspace
        .journal
        .next_undoable()
        .map(|entry| entry.seq);
    let entries: Vec<Value> = workspace
        .journal
        .tail(limit)
        .iter()
        .map(|entry| {
            json!({
                "seq": entry.seq,
                "ts": entry.ts,
                "actor": entry.actor,
                "op": entry.op,
                "args": entry.args,
                "target": entry.target,
                "undoable": Some(entry.seq) == undoable,
            })
        })
        .collect();
    json!({
        "entries": entries,
        "total": workspace.journal.len(),
        "undoable": undoable,
    })
}

/// One rendered frame on disk, plus what an agent cannot see in it.
#[derive(Debug, Clone)]
pub struct FrameShot {
    pub frame: i64,
    pub at: Time,
    pub output: PathBuf,
    /// `dvs-inspect` annotations when the frame was annotated, otherwise the compositor's
    /// layer report. Both answer "what is in this frame and what are its ids"; they are
    /// carried as JSON because only the caller cares about the distinction.
    pub annotations: Value,
    pub annotated: bool,
}

/// Render a single frame to a PNG.
///
/// `annotate` is the difference between a picture and a handle: the annotated frame has
/// clip and title boxes with their ids burned in, so a vision model can say "move `#title`
/// left" instead of "move the text left".
pub fn frame(
    workspace: &Workspace,
    tool: &Toolchain,
    sequence: &SequenceId,
    at: Time,
    scale: f64,
    annotate: bool,
    output: &Path,
) -> Result<FrameShot> {
    let fps = workspace.project.sequence(sequence)?.fps;
    let index = at.frame_floor(fps);
    if index < 0 {
        return Err(Error::bad_args(format!(
            "{at} is before the start of the timeline"
        )));
    }
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }

    if annotate {
        let annotations = dvs_inspect::annotate_frame(workspace, tool, sequence, at, output)?;
        return Ok(FrameShot {
            frame: index,
            at: Time::from_frames(index, fps),
            output: output.to_path_buf(),
            annotations: serde_json::to_value(&annotations)
                .map_err(|e| Error::json(output, e))?,
            annotated: true,
        });
    }

    let options = dvs_comp::CompOptions {
        scale,
        ..dvs_comp::CompOptions::default()
    };
    let mut compositor = dvs_comp::Compositor::new(
        tool,
        &workspace.project,
        &workspace.paths,
        &workspace.assets,
        sequence,
        options,
    )?;
    let (frame, report) = compositor.frame_with_report(index)?;
    dvs_media::write_png(tool, &frame, output)?;
    Ok(FrameShot {
        frame: index,
        at: report.at,
        output: output.to_path_buf(),
        annotations: serde_json::to_value(&report.layers).map_err(|e| Error::json(output, e))?,
        annotated: false,
    })
}

/// Default destination for a frame nobody named: inside the project cache, keyed by
/// sequence and frame so repeated looks at the same instant overwrite rather than pile up.
pub fn cached_frame_path(workspace: &Workspace, sequence: &SequenceId, index: i64) -> PathBuf {
    workspace
        .paths
        .cache_dir()
        .join("frame")
        .join(format!("{sequence}-{index}.png"))
}

/// Lint findings as the `--json` body, with the counts a caller branches on.
pub fn lint_json(findings: &[Finding], profile: &str) -> Value {
    let count = |wanted: Severity| {
        findings
            .iter()
            .filter(|finding| finding.severity == wanted)
            .count()
    };
    json!({
        "profile": profile,
        "findings": findings,
        "counts": {
            "error": count(Severity::Error),
            "warning": count(Severity::Warning),
            "info": count(Severity::Info),
            "total": findings.len(),
        },
        "worst": findings.iter().map(|finding| finding.severity).max(),
    })
}

/// Whether any finding is fatal. The CLI turns this into exit code 4, which is the whole
/// point of a lint an agent can branch on.
pub fn has_errors(findings: &[Finding]) -> bool {
    findings
        .iter()
        .any(|finding| finding.severity == Severity::Error)
}

/// Search a transcript, or read the words in a span of source time.
///
/// Transcripts are asset-side artifacts, so this answers in *source* time; mapping that
/// onto the timeline is `transcript.find`'s job, which knows which clips use the asset.
pub fn transcript_query(
    workspace: &Workspace,
    asset: &str,
    phrase: Option<&str>,
    span: Option<Span>,
    limit: usize,
) -> Result<Value> {
    let asset_id = workspace.project.resolve_asset(asset)?;
    let transcript = dvs_text::Transcript::load(&workspace.paths, &asset_id)?;
    let mut out = json!({
        "asset": asset_id,
        "language": transcript.language,
        "model": transcript.model,
        "words": transcript.words.len(),
    });

    if let Some(phrase) = phrase {
        let matches: Vec<Value> = transcript
            .find_phrase(phrase)
            .into_iter()
            .take(limit)
            .map(|span| {
                json!({
                    "start": span.start,
                    "end": span.end,
                    "clock": span.start.clock(),
                })
            })
            .collect();
        out["phrase"] = json!(phrase);
        out["matches"] = Value::Array(matches);
    }

    if let Some(span) = span {
        let words: Vec<Value> = transcript
            .in_span(span)
            .iter()
            .take(limit)
            .map(|word| {
                json!({
                    "text": word.text,
                    "start": word.start,
                    "end": word.end,
                    "confidence": word.confidence,
                })
            })
            .collect();
        out["span"] = json!({ "start": span.start, "end": span.end });
        out["text"] = json!(transcript
            .in_span(span)
            .iter()
            .map(|word| word.text.as_str())
            .collect::<Vec<_>>()
            .join(" "));
        out["inSpan"] = Value::Array(words);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_collapse_both_separators() {
        assert_eq!(tool_name("clip.split"), "clip_split");
        assert_eq!(tool_name("title.set-text"), "title_set_text");
        assert_eq!(tool_name("seq.auto-cut-scenes"), "seq_auto_cut_scenes");
    }

    /// A typo must come back as a no-match whose candidates are *callable tool names* —
    /// not op ids. Handing an MCP client `clip.split` when the tool is `clip_split` would
    /// make the retry fail the same way.
    #[test]
    fn unknown_tool_names_suggest_real_tools() {
        let registry = crate::full_registry();
        let Err(error) = op_for_tool(&registry, "clip_spilt") else {
            panic!("a typo must not resolve to a tool");
        };
        let report = error.to_report();
        assert_eq!(report.kind, "no-match");
        assert!(!report.candidates.is_empty(), "no candidates offered");
        for candidate in &report.candidates {
            assert!(
                candidate.starts_with("clip_"),
                "candidate '{candidate}' is not from the namespace that was asked for"
            );
            assert!(
                op_for_tool(&registry, candidate).is_ok(),
                "candidate '{candidate}' is not itself callable"
            );
        }
    }

    #[test]
    fn ranges_parse_and_reject_inverted_ends() {
        let fps = Fps::new(30, 1).unwrap();
        let span = parse_span("00:10-00:20", fps).unwrap();
        assert_eq!(span.start, Time::from_secs(10));
        assert_eq!(span.end, Time::from_secs(20));
        assert_eq!(parse_span("1800f-3600f", fps).unwrap().duration(), Time::from_secs(60));
        assert!(parse_span("20-10", fps).is_err());
        assert!(parse_span("20", fps).is_err());
    }
}
