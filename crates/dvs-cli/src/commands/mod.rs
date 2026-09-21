//! Command implementations and the context they share.
//!
//! Every command that changes the document goes through [`Ctx::apply`], which runs the op
//! on the engine and prints the [`Applied`] record: op id, journal seq, what was created,
//! changed and removed, the warnings, and — the reason this is not optional — the frame
//! snapping. An agent that asks for a cut at 42.5 s on a 30000/1001 timeline gets frame
//! 1274; discovering that three edits later is how a timeline drifts.

pub mod asset;
pub mod export;
pub mod inspect;
pub mod op;
pub mod project;
pub mod render;
pub mod text;

use crate::cli::{Cli, Command};
use crate::output::{list, Out};
use dvs_core::engine::{Applied, Engine, Workspace};
use dvs_core::error::{Error, Result};
use dvs_core::ids::SequenceId;
use dvs_core::op::Registry;
use dvs_core::time::Fps;
use dvs_media::Toolchain;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::LazyLock;

/// The one op list, built once per process. Construction walks every capability crate's
/// registration, which is cheap but not free, and three commands ask for it.
static REGISTRY: LazyLock<Registry> = LazyLock::new(dvs_mcp::full_registry);

pub fn registry() -> &'static Registry {
    &REGISTRY
}

/// Everything the global flags decided, resolved once so no command re-reads argv.
pub struct Ctx {
    /// Where to look for the project. Commands open it per use rather than holding it,
    /// so a long-running shell session cannot serve a document another writer replaced.
    pub root: PathBuf,
    pub seq: Option<String>,
    pub dry_run: bool,
    pub out: Out,
}

impl Ctx {
    pub fn workspace(&self) -> Result<Workspace> {
        Workspace::open_native(&self.root)
    }

    pub fn engine(&self) -> Result<Engine> {
        Ok(Engine::new(REGISTRY.clone(), self.workspace()?))
    }

    /// The sequence `--seq` names, or the active one.
    pub fn sequence(&self, workspace: &Workspace) -> Result<SequenceId> {
        workspace.project.resolve_sequence(self.seq.as_deref())
    }

    pub fn fps(&self, workspace: &Workspace, sequence: &SequenceId) -> Result<Fps> {
        Ok(workspace.project.sequence(sequence)?.fps)
    }

    /// ffmpeg, or an error that says how to install it. Commands that need pixels call
    /// this first so the failure arrives before any work is done.
    pub fn toolchain(&self) -> Result<&'static Toolchain> {
        Toolchain::shared()
    }

    /// Run one op and report it. This is the only path by which the CLI mutates a
    /// document.
    pub fn apply(&self, id: &str, args: Value) -> Result<Applied> {
        let mut engine = self.engine()?;
        let applied = engine.apply(id, args, self.seq.clone(), self.dry_run)?;
        self.report_applied(&applied)?;
        Ok(applied)
    }

    pub fn report_applied(&self, applied: &Applied) -> Result<()> {
        let mut value =
            serde_json::to_value(applied).map_err(|e| Error::json(applied.op.clone(), e))?;
        if self.dry_run {
            value["dryRun"] = Value::Bool(true);
        }
        let dry_run = self.dry_run;
        let applied = applied.clone();
        self.out.report(&value, || describe(&applied, dry_run));
        Ok(())
    }
}

/// The human rendering of an [`Applied`]: the ids first, then the two things that are
/// easy to miss — what the op snapped, and what it warned about.
pub fn describe(applied: &Applied, dry_run: bool) -> String {
    let mut lines = Vec::new();
    let head = match applied.seq {
        Some(seq) => format!("{} #{seq}", applied.op),
        None if dry_run => format!("{} (dry run)", applied.op),
        None => applied.op.clone(),
    };
    lines.push(head);
    let effect = &applied.effect;
    if !effect.created.is_empty() {
        lines.push(format!("  created  {}", list(&effect.created)));
    }
    if !effect.changed.is_empty() {
        lines.push(format!("  changed  {}", list(&effect.changed)));
    }
    if !effect.removed.is_empty() {
        lines.push(format!("  removed  {}", list(&effect.removed)));
    }
    for snap in &effect.snapped {
        lines.push(format!(
            "  snapped  {} {} -> {} (frame {})",
            snap.field, snap.requested, snap.applied, snap.frame
        ));
    }
    for warning in &effect.warnings {
        lines.push(format!(
            "  warning  {} {}: {}",
            warning.code, warning.target, warning.detail
        ));
    }
    if let Some(data) = &effect.data {
        lines.push(format!(
            "  data     {}",
            serde_json::to_string(data).unwrap_or_else(|_| "<unserializable>".to_string())
        ));
    }
    lines.join("\n")
}

/// Turn the parsed command line into work. `new` is the only command that runs without an
/// existing project, which is why the context carries a directory rather than a workspace.
pub fn dispatch(cli: Cli) -> Result<()> {
    let out = Out::new(cli.json, cli.quiet);
    let root = match &cli.project {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().map_err(|e| Error::io(".", e))?,
    };
    let ctx = Ctx {
        root,
        seq: cli.seq.clone(),
        dry_run: cli.dry_run,
        out,
    };

    match cli.command {
        Command::New(args) => project::new(&ctx, &cli.project, args),
        Command::Asset { command } => asset::run(&ctx, command),
        Command::Op(args) => op::run(&ctx, args),
        Command::Schema(args) => op::schema(&ctx, args),
        Command::Render(args) => render::render(&ctx, args),
        Command::Frame(args) => render::frame(&ctx, args),
        Command::Sheet(args) => render::sheet(&ctx, args),
        Command::Digest(args) => inspect::digest(&ctx, args),
        Command::Lint(args) => inspect::lint(&ctx, args),
        Command::Diff(args) => inspect::diff(&ctx, args),
        Command::Transcript { command } => text::transcript(&ctx, command),
        Command::Caption { command } => text::caption(&ctx, command),
        Command::Export(args) => export::run(&ctx, args),
        Command::Undo => project::undo(&ctx),
        Command::Redo => project::redo(&ctx),
        Command::History(args) => project::history(&ctx, args),
        Command::Gc => project::gc(&ctx),
        Command::Doctor => project::doctor(&ctx),
        Command::Mcp => project::mcp(&ctx),
    }
}
