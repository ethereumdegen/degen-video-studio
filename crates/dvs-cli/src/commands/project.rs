//! Project-level verbs: create, undo, redo, history, gc, doctor, and the MCP server.
//!
//! These are the commands that are not ops. `new` has no document to apply an op to;
//! `undo`/`redo` are the journal's own mechanism; `gc` and `doctor` operate on the
//! directory rather than the document; `mcp` hands the same registry to a different
//! transport. Everything that edits a timeline is an op, and lives elsewhere.

use crate::cli::{HistoryArgs, NewArgs};
use crate::commands::Ctx;
use dvs_core::engine::Workspace;
use dvs_core::error::{Error, Result};
use dvs_core::op::OpCx;
use dvs_core::paths::ProjectPaths;
use dvs_core::project::{Project, Tools};
use dvs_core::time::Fps;
use dvs_core::vfs::FsVfs;
use dvs_media::Toolchain;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Lay out a project directory. Without `--project` the directory is the project name in
/// the working directory, which is what `dvs new promo` should obviously do.
pub fn new(ctx: &Ctx, project_flag: &Option<PathBuf>, args: NewArgs) -> Result<()> {
    let root = match project_flag {
        Some(dir) => dir.clone(),
        None => ctx.root.join(&args.name),
    };
    let fps = Fps::parse(&args.fps)?;
    if args.sample_rate == 0 {
        return Err(Error::bad_args("sample rate must be greater than zero"));
    }
    let project = Project::new(&args.name, fps, args.size, args.sample_rate);
    let sequence = project.sequence(&project.active_sequence)?.clone();
    let value = json!({
        "project": project.id,
        "name": project.name,
        "root": root.display().to_string(),
        "format": project.format,
        "sequence": {
            "id": sequence.id,
            "name": sequence.name,
            "fps": sequence.fps,
            "size": sequence.size,
            "sampleRate": sequence.sample_rate,
        },
        "created": !ctx.dry_run,
    });

    if !ctx.dry_run {
        Workspace::create(ProjectPaths::new(&root), project, FsVfs::shared())?;
    }
    ctx.out.report(&value, || {
        format!(
            "{} {} at {} ({} {}x{})",
            if ctx.dry_run { "would create" } else { "created" },
            sequence.name,
            root.display(),
            sequence.fps,
            sequence.size[0],
            sequence.size[1]
        )
    });
    Ok(())
}

pub fn undo(ctx: &Ctx) -> Result<()> {
    let mut engine = ctx.engine()?;
    let pending = engine
        .workspace
        .journal
        .next_undoable()
        .map(|entry| (entry.seq, entry.op.clone()));
    if ctx.dry_run {
        let value = json!({
            "wouldUndo": pending.as_ref().map(|(_, op)| op.clone()),
            "seq": pending.as_ref().map(|(seq, _)| *seq),
            "dryRun": true,
        });
        ctx.out.report(&value, || match &pending {
            Some((seq, op)) => format!("would undo {op} (#{seq})"),
            None => "nothing to undo".to_string(),
        });
        return Ok(());
    }
    let undone = engine.undo()?;
    let value = json!({
        "undone": undone,
        "seq": pending.as_ref().map(|(seq, _)| *seq),
        "entries": engine.workspace.journal.len(),
    });
    ctx.out.report(&value, || match &undone {
        Some(op) => format!("undid {op}"),
        None => "nothing to undo".to_string(),
    });
    Ok(())
}

pub fn redo(ctx: &Ctx) -> Result<()> {
    let mut engine = ctx.engine()?;
    if ctx.dry_run {
        let pending = engine
            .workspace
            .journal
            .next_redoable()
            .map(|entry| entry.op.clone());
        let value = json!({ "wouldRedo": pending, "dryRun": true });
        ctx.out.report(&value, || match &pending {
            Some(op) => format!("would redo {op}"),
            None => "nothing to redo".to_string(),
        });
        return Ok(());
    }
    let redone = engine.redo()?;
    let value = json!({
        "redone": redone,
        "entries": engine.workspace.journal.len(),
    });
    ctx.out.report(&value, || match &redone {
        Some(op) => format!("redid {op}"),
        None => "nothing to redo".to_string(),
    });
    Ok(())
}

pub fn history(ctx: &Ctx, args: HistoryArgs) -> Result<()> {
    let workspace = ctx.workspace()?;
    let value = dvs_mcp::tools::history(&workspace, args.limit);
    ctx.out.report(&value, || {
        let mut lines = Vec::new();
        for entry in value["entries"].as_array().into_iter().flatten() {
            lines.push(format!(
                "#{:<4} {:<8} {}{}",
                entry["seq"].as_u64().unwrap_or_default(),
                entry["actor"].as_str().unwrap_or("agent"),
                entry["op"].as_str().unwrap_or_default(),
                if entry["undoable"] == Value::Bool(true) {
                    "   <- undo"
                } else {
                    ""
                }
            ));
        }
        lines.join("\n")
    });
    Ok(())
}

/// Drop what can be rebuilt. Proxies are kept: the document points at them by path, so
/// deleting one turns a working project into a slow one until every import is re-run.
/// Everything else under `cache/` is derived from the document and the assets.
pub fn gc(ctx: &Ctx) -> Result<()> {
    let workspace = ctx.workspace()?;
    let regenerable = [
        workspace.paths.segment_dir(),
        workspace.paths.thumb_dir(),
        workspace.paths.waveform_dir(),
        workspace.paths.cache_dir().join("frame"),
    ];

    let mut files = 0u64;
    let mut bytes = 0u64;
    for dir in &regenerable {
        let (count, size) = measure(dir)?;
        files += count;
        bytes += size;
        if !ctx.dry_run && dir.exists() {
            std::fs::remove_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        }
    }

    let referenced = workspace.project.referenced_hashes();
    let orphans: Vec<String> = if ctx.dry_run {
        workspace
            .assets
            .list()?
            .into_iter()
            .filter(|hash| !referenced.contains(hash))
            .collect()
    } else {
        workspace.assets.gc(&referenced)?
    };

    let value = json!({
        "cache": { "files": files, "bytes": bytes },
        "assets": { "removed": orphans, "kept": referenced.len() },
        "dryRun": ctx.dry_run,
    });
    ctx.out.report(&value, || {
        format!(
            "{} {files} cache file(s), {bytes} bytes, and {} unreferenced asset(s)",
            if ctx.dry_run { "would drop" } else { "dropped" },
            orphans.len()
        )
    });
    Ok(())
}

/// What this machine can do, and what this project is missing.
///
/// Exit 5 when ffmpeg is absent, because every render path is then unavailable and an
/// agent should stop rather than discover it one op at a time. The found toolchain is
/// recorded into the document as provenance — encoded bytes depend on the ffmpeg build,
/// so a project that was rendered here says so. That write is not journaled for the same
/// reason `modified` is not: it describes the machine, not an edit, and an undo that
/// rewound it would undo nothing a user did.
pub fn doctor(ctx: &Ctx) -> Result<()> {
    let workspace = ctx.workspace().ok();
    let toolchain = Toolchain::shared();
    let whisper = cfg!(feature = "whisper");

    let mut value = json!({
        "ffmpeg": match &toolchain {
            Ok(tool) => json!(tool.report()),
            Err(error) => json!({ "error": error.to_report() }),
        },
        "whisper": {
            "compiled": whisper,
            "note": if whisper {
                "whisper-rs is compiled in; 'transcript.run' works offline"
            } else {
                "built without --features whisper; use 'transcript.import' with word JSON from any ASR"
            },
        },
        "engine": dvs_core::ENGINE_VERSION,
    });

    match &workspace {
        Some(workspace) => {
            let cx = OpCx::new(&workspace.paths, &workspace.assets);
            let missing: Vec<Value> = dvs_media::ops::missing_assets(&workspace.project, &cx)
                .into_iter()
                .map(|(id, name)| json!({ "asset": id, "name": name }))
                .collect();
            let stale: Vec<Value> = workspace
                .project
                .assets
                .values()
                .filter_map(|asset| {
                    let proxy = asset.proxy.as_ref()?;
                    let path = workspace.paths.resolve(proxy);
                    (!path.exists()).then(|| json!({ "asset": asset.id, "proxy": proxy }))
                })
                .collect();
            value["project"] = json!({
                "root": workspace.paths.root().display().to_string(),
                "format": workspace.project.format,
                "supportedFormat": dvs_core::project::FORMAT_VERSION,
                "sequences": workspace.project.sequences.len(),
                "assets": workspace.project.assets.len(),
                "missingAssets": missing,
                "staleProxies": stale,
                "history": workspace.journal.len(),
            });
        }
        None => {
            value["project"] = json!({
                "error": format!("no project at or above {}", ctx.root.display())
            });
        }
    }

    let tool = match toolchain {
        Ok(tool) => tool,
        Err(error) => {
            // Print the report before failing: "what is missing" is the answer, and the
            // exit code is how a script branches on it. The detail line is left to the
            // error path so it is not printed twice.
            ctx.out.report(&value, || "ffmpeg: MISSING".to_string());
            return Err(error);
        }
    };

    // Provenance, recorded once per doctor run and only when it changed.
    if let Some(mut workspace) = workspace {
        let tools = Tools {
            engine: Some(dvs_core::ENGINE_VERSION.to_string()),
            ffmpeg: Some(tool.version().to_string()),
            encoders: tool.report().hardware_encoders,
        };
        if workspace.project.tools != tools && !ctx.dry_run {
            workspace.project.tools = tools;
            workspace.save()?;
            value["recorded"] = Value::Bool(true);
        }
    }

    ctx.out.report(&value, || {
        let report = tool.report();
        let mut lines = vec![
            format!("ffmpeg   {} ({})", report.version, report.ffmpeg),
            format!("ffprobe  {}", report.ffprobe),
            format!(
                "hardware {}",
                if report.hardware_encoders.is_empty() {
                    "none detected".to_string()
                } else {
                    report.hardware_encoders.join(", ")
                }
            ),
            format!("whisper  {}", if whisper { "compiled in" } else { "not built" }),
        ];
        // Two shapes live under `project`: the survey, and `{ "error": … }` when there is no
        // project at or above the cwd. Indexing the survey's keys on the error shape panicked
        // with `no entry found for key`, which made `dvs doctor` — the command whose whole job
        // is to run when things are wrong — the one command that could not run outside a
        // project. Match on what is actually there.
        match &value["project"] {
            Value::Object(p) if p.contains_key("error") => {
                lines.push(format!("project  {}", p["error"].as_str().unwrap_or("unavailable")));
            }
            Value::Object(p) => lines.push(format!(
                "project  format {} · {} asset(s) · {} missing · {} stale prox(ies)",
                p["format"],
                p["assets"],
                p["missingAssets"].as_array().map_or(0, Vec::len),
                p["staleProxies"].as_array().map_or(0, Vec::len),
            )),
            _ => {}
        }
        lines.join("\n")
    });
    Ok(())
}

/// Serve the registry over MCP. The runtime is built here rather than wrapping `main` in
/// `#[tokio::main]` because this is the only command that needs one, and paying for an
/// executor on `dvs op clip.split` would be silly.
pub fn mcp(ctx: &Ctx) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::io("tokio runtime", e))?;
    ctx.out.note("dvs mcp: serving on stdio");
    runtime.block_on(dvs_mcp::serve(ctx.root.clone()))
}

/// Files and bytes under a directory, without deleting it. Used so `gc --dry-run` can
/// report exactly what the real run would free.
fn measure(dir: &Path) -> Result<(u64, u64)> {
    if !dir.exists() {
        return Ok((0, 0));
    }
    let mut files = 0;
    let mut bytes = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current).map_err(|e| Error::io(&current, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| Error::io(&current, e))?;
            let metadata = entry.metadata().map_err(|e| Error::io(entry.path(), e))?;
            if metadata.is_dir() {
                stack.push(entry.path());
            } else {
                files += 1;
                bytes += metadata.len();
            }
        }
    }
    Ok((files, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measuring_a_missing_directory_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(measure(&dir.path().join("nope")).unwrap(), (0, 0));
    }

    #[test]
    fn measuring_counts_nested_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/one"), b"12345").unwrap();
        std::fs::write(dir.path().join("a/b/two"), b"123").unwrap();
        assert_eq!(measure(dir.path()).unwrap(), (2, 8));
    }
}
