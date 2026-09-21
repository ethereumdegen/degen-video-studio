//! `dvs asset import|list|remove`.
//!
//! Import and remove are sugar over `asset.import` and `asset.remove`: the verbs exist
//! because they are what a person types, and they build the same argument object
//! `dvs op` would, so there is one implementation and one journal entry shape.
//!
//! `list` is the exception — it reads the document and answers, with no op, because
//! listing is not an edit and an op that mutates nothing would still need a name in the
//! undo history.

use crate::cli::{AssetCommand, AssetImportArgs, AssetRemoveArgs};
use crate::commands::Ctx;
use dvs_core::error::Result;
use serde_json::{json, Value};

pub fn run(ctx: &Ctx, command: AssetCommand) -> Result<()> {
    match command {
        AssetCommand::Import(args) => import(ctx, args),
        AssetCommand::List => list(ctx),
        AssetCommand::Remove(args) => remove(ctx, args),
    }
}

fn import(ctx: &Ctx, args: AssetImportArgs) -> Result<()> {
    let paths: Vec<Value> = args
        .paths
        .iter()
        .map(|path| Value::String(path.display().to_string()))
        .collect();
    let proxy = match args.proxy.trim().to_ascii_lowercase().as_str() {
        "auto" | "" => Value::String("auto".to_string()),
        "true" | "yes" | "always" => Value::Bool(true),
        "false" | "no" | "never" => Value::Bool(false),
        other => {
            return Err(dvs_core::error::Error::bad_args(format!(
                "--proxy takes auto, yes or no, not '{other}'"
            )))
        }
    };
    ctx.apply("asset.import", json!({ "path": paths, "proxy": proxy }))
        .map(|_| ())
}

fn list(ctx: &Ctx) -> Result<()> {
    let workspace = ctx.workspace()?;
    let assets: Vec<Value> = workspace
        .project
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
                "audio": asset.probe.audio.as_ref().map(|audio| json!({
                    "rate": audio.rate,
                    "channels": audio.channels,
                })),
                "vfr": asset.probe.vfr,
                "proxy": asset.proxy,
                "hash": asset.hash,
                // An asset whose bytes are gone still has a document entry; saying so
                // here is cheaper than finding out during a render.
                "present": workspace.assets.exists(&asset.hash),
            })
        })
        .collect();
    let value = json!({ "count": assets.len(), "assets": assets });
    ctx.out.report(&value, || {
        let mut lines = Vec::new();
        for asset in value["assets"].as_array().into_iter().flatten() {
            lines.push(format!(
                "{:<28} {:<7} {:>10}  {}{}",
                asset["name"].as_str().unwrap_or_default(),
                asset["kind"].as_str().unwrap_or_default(),
                asset["duration"].as_str().unwrap_or("-"),
                asset["id"].as_str().unwrap_or_default(),
                if asset["present"] == Value::Bool(false) {
                    "  MISSING"
                } else {
                    ""
                }
            ));
        }
        lines.join("\n")
    });
    Ok(())
}

fn remove(ctx: &Ctx, args: AssetRemoveArgs) -> Result<()> {
    ctx.apply(
        "asset.remove",
        json!({ "asset": args.asset, "force": args.force }),
    )
    .map(|_| ())
}
