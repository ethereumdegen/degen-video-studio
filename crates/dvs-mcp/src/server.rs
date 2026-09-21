//! The MCP server: stdio in, JSON out, one tool per op plus the loop tools.
//!
//! Three properties are deliberate.
//!
//! **Every call re-opens the project.** The Tauri app and a second `dvs` process write the
//! same directory; holding a `Workspace` across calls would serve an agent a document that
//! no longer exists on disk and let it compound edits onto a stale base.
//!
//! **Tool work runs on a blocking thread.** Ops spawn ffmpeg and block for seconds to
//! minutes; running one on the async executor would stall the stdio transport and make the
//! server look hung.
//!
//! **A failure is a result, not a protocol error.** MCP clients render protocol errors
//! opaquely ("tool result missing"), so an op failure comes back as a structured error
//! result carrying the same `kind`/`code`/`candidates` the CLI prints — an agent that
//! mistypes a selector gets the list of real clip names either way. Panics are caught at
//! the join boundary for the same reason.

use crate::tools::{self, FrameShot};
use crate::full_registry;
use base64::Engine as _;
use dvs_core::engine::{Engine, Workspace};
use dvs_core::error::{Error, Result};
use dvs_core::ids::SequenceId;
use dvs_core::op::args;
use dvs_core::op::Registry;
use dvs_core::time::{Fps, Time};
use dvs_inspect::{DigestOptions, LintOptions, LoudnessProfile};
use dvs_media::{Encoder, Toolchain};
use dvs_render::RenderSpec;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What a tool call produced: the JSON body every tool returns, plus a PNG for the tools
/// that render one.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub value: Value,
    pub image: Option<PathBuf>,
}

impl Outcome {
    fn json(value: Value) -> Outcome {
        Outcome { value, image: None }
    }
}

struct Inner {
    registry: Registry,
    root: PathBuf,
    tools: Vec<Tool>,
}

/// The server. Cloning is cheap — the registry and the tool list are shared — which is
/// what lets a call move a handle onto a blocking thread.
#[derive(Clone)]
pub struct DvsServer {
    inner: Arc<Inner>,
}

impl DvsServer {
    /// A server over the project containing `root`. The directory is resolved per call, so
    /// `root` may be any directory inside the project, exactly like the CLI's `--project`.
    pub fn new(root: impl Into<PathBuf>) -> DvsServer {
        DvsServer::with_registry(full_registry(), root)
    }

    pub fn with_registry(registry: Registry, root: impl Into<PathBuf>) -> DvsServer {
        let tools = tools::full_catalog(&registry);
        DvsServer {
            inner: Arc::new(Inner {
                registry,
                root: root.into(),
                tools,
            }),
        }
    }

    pub fn registry(&self) -> &Registry {
        &self.inner.registry
    }

    pub fn tools(&self) -> &[Tool] {
        &self.inner.tools
    }

    fn workspace(&self) -> Result<Workspace> {
        Workspace::open_native(&self.inner.root)
    }

    fn engine(&self) -> Result<Engine> {
        Ok(Engine::new(self.inner.registry.clone(), self.workspace()?))
    }

    fn sequence(&self, workspace: &Workspace, args: &Value) -> Result<SequenceId> {
        workspace
            .project
            .resolve_sequence(args::opt_str(args, "seq"))
    }

    /// Run one tool. This is the whole server minus the transport, so it is what the tests
    /// and the CLI's in-process callers use.
    pub fn call(&self, tool: &str, args: Value) -> Result<Outcome> {
        if !args.is_object() && !args.is_null() {
            return Err(Error::bad_args("tool arguments must be a JSON object"));
        }
        let args = if args.is_null() { json!({}) } else { args };
        match tool {
            "dvs_overview" => self.overview(&args),
            "dvs_apply" => self.apply(&args),
            "dvs_render" => self.render(&args),
            "dvs_lint" => self.lint(&args),
            "dvs_frame" => self.frame(&args),
            "dvs_transcript" => self.transcript(&args),
            "dvs_history" => self.history(&args),
            other => self.run_op(other, args),
        }
    }

    fn run_op(&self, tool: &str, args: Value) -> Result<Outcome> {
        let op = tools::op_for_tool(&self.inner.registry, tool)?;
        let id = op.id().to_string();
        let mut engine = self.engine()?;
        // Per-op tools act on the active sequence: their schema is the op's own, and
        // widening it with a `seq` key here would make the MCP schema differ from the one
        // `dvs schema --op` publishes. Batches (`dvs_apply`) carry a per-op `seq`.
        let applied = engine.apply(&id, args, None, false)?;
        Ok(Outcome::json(
            serde_json::to_value(&applied).map_err(|e| Error::json(&id, e))?,
        ))
    }

    fn overview(&self, args: &Value) -> Result<Outcome> {
        let workspace = self.workspace()?;
        // A missing ffmpeg must not hide the document: the overview is also how an agent
        // finds out the toolchain is the problem.
        let tool = Toolchain::shared().ok();
        let mut value = tools::overview(&workspace, tool)?;
        if let Some(seq) = args::opt_str(args, "seq") {
            value["sequence"] = json!(workspace.project.resolve_sequence(Some(seq))?);
        }
        Ok(Outcome::json(value))
    }

    fn apply(&self, args: &Value) -> Result<Outcome> {
        let list = args
            .get("ops")
            .and_then(|ops| ops.as_array())
            .ok_or_else(|| Error::bad_args("'ops' must be an array of {op, args, seq}"))?;
        let mut batch = Vec::with_capacity(list.len());
        for (index, entry) in list.iter().enumerate() {
            let id = entry
                .get("op")
                .and_then(|op| op.as_str())
                .ok_or_else(|| Error::bad_args(format!("ops[{index}] has no 'op' id")))?;
            let op_args = entry.get("args").cloned().unwrap_or_else(|| json!({}));
            let seq = entry
                .get("seq")
                .and_then(|seq| seq.as_str())
                .map(str::to_string);
            batch.push((id.to_string(), op_args, seq));
        }
        let dry_run = args::opt_bool(args, "dry-run")?.unwrap_or(false);

        let mut engine = self.engine()?;
        let applied = engine.apply_batch(batch, dry_run)?;
        let mut value = json!({
            "applied": applied,
            "committed": !dry_run,
            "ops": applied.len(),
        });
        if args::opt_bool(args, "digest")?.unwrap_or(false) {
            let tool = Toolchain::shared()?;
            let sequence = self.sequence(&engine.workspace, args)?;
            let digest = dvs_inspect::digest(
                &engine.workspace,
                tool,
                &sequence,
                &DigestOptions::default(),
            )?;
            value["digest"] = serde_json::to_value(&digest)
                .map_err(|e| Error::json(engine.workspace.paths.project_json(), e))?;
        }
        Ok(Outcome::json(value))
    }

    fn render(&self, args: &Value) -> Result<Outcome> {
        let workspace = self.workspace()?;
        let tool = Toolchain::shared()?;
        let sequence = self.sequence(&workspace, args)?;
        let fps = workspace.project.sequence(&sequence)?.fps;
        let output = PathBuf::from(args::str_field(args, "output")?);
        let mut spec = RenderSpec::new(output, sequence.clone());
        spec.with_digest = true;
        apply_render_args(&mut spec, args, fps)?;

        let report = dvs_render::render(&workspace, tool, &spec)?;
        let mut value = json!({
            "render": serde_json::to_value(&report)
                .map_err(|e| Error::json(&report.output, e))?,
        });
        if let Some(sheet) = args::opt_str(args, "sheet") {
            let sheet_path = PathBuf::from(sheet);
            let every = args::opt_time(args, "sheet-every", fps)?.unwrap_or(Time::from_secs(5));
            let columns = opt_u32(args, "sheet-columns")?.unwrap_or(6);
            let width = opt_u32(args, "sheet-width")?.unwrap_or(1920);
            let report =
                dvs_inspect::contact_sheet(&workspace, tool, &sequence, every, columns, width, &sheet_path)?;
            value["sheet"] =
                serde_json::to_value(&report).map_err(|e| Error::json(&sheet_path, e))?;
        }
        Ok(Outcome::json(value))
    }

    fn lint(&self, args: &Value) -> Result<Outcome> {
        let workspace = self.workspace()?;
        let tool = Toolchain::shared()?;
        let sequence = self.sequence(&workspace, args)?;
        let mut options = LintOptions::default();
        if let Some(profile) = args::opt_str(args, "profile") {
            options.profile = LoudnessProfile::parse(profile)?;
        }
        if let Some(render) = args::opt_bool(args, "render")? {
            options.render = render;
        }
        let findings = dvs_inspect::lint(&workspace, tool, &sequence, &options)?;
        Ok(Outcome::json(tools::lint_json(
            &findings,
            options.profile.name(),
        )))
    }

    fn frame(&self, args: &Value) -> Result<Outcome> {
        let workspace = self.workspace()?;
        let tool = Toolchain::shared()?;
        let sequence = self.sequence(&workspace, args)?;
        let fps = workspace.project.sequence(&sequence)?.fps;
        let at = args::time_field(args, "at", fps)?;
        let scale = args::opt_f64(args, "scale")?.unwrap_or(1.0);
        let annotate = args::opt_bool(args, "annotate")?.unwrap_or(true);
        let output = match args::opt_str(args, "output") {
            Some(path) => PathBuf::from(path),
            None => tools::cached_frame_path(&workspace, &sequence, at.frame_floor(fps)),
        };
        let shot: FrameShot =
            tools::frame(&workspace, tool, &sequence, at, scale, annotate, &output)?;
        Ok(Outcome {
            value: json!({
                "frame": shot.frame,
                "at": shot.at,
                "timecode": shot.at.timecode(fps),
                "output": shot.output.display().to_string(),
                "annotated": shot.annotated,
                "annotations": shot.annotations,
            }),
            image: Some(shot.output),
        })
    }

    fn transcript(&self, args: &Value) -> Result<Outcome> {
        let workspace = self.workspace()?;
        let asset = args::str_field(args, "asset")?;
        let phrase = args::opt_str(args, "phrase");
        let span = match args::opt_str(args, "span") {
            Some(text) => Some(tools::parse_span(text, Fps::default())?),
            None => None,
        };
        let limit = opt_u32(args, "limit")?.unwrap_or(200) as usize;
        Ok(Outcome::json(tools::transcript_query(
            &workspace, asset, phrase, span, limit,
        )?))
    }

    fn history(&self, args: &Value) -> Result<Outcome> {
        let workspace = self.workspace()?;
        let limit = opt_u32(args, "limit")?.unwrap_or(20) as usize;
        Ok(Outcome::json(tools::history(&workspace, limit)))
    }
}

/// Fill a [`RenderSpec`] from the render arguments both surfaces share.
pub fn apply_render_args(spec: &mut RenderSpec, args: &Value, fps: Fps) -> Result<()> {
    if let Some(range) = args::opt_str(args, "range") {
        spec.range = Some(tools::parse_span(range, fps)?);
    }
    if let Some(scale) = args::opt_f64(args, "scale")? {
        if scale <= 0.0 {
            return Err(Error::bad_args("scale must be greater than zero"));
        }
        spec.scale = scale;
    }
    if let Some(encoder) = args::opt_str(args, "encoder") {
        spec.encoder = Encoder::parse(encoder);
    }
    if let Some(quality) = opt_u32(args, "quality")? {
        spec.quality = quality;
    }
    if let Some(preset) = args::opt_str(args, "preset") {
        spec.preset = Some(preset.to_string());
    }
    if let Some(proxy) = args::opt_bool(args, "proxy")? {
        spec.use_proxy = proxy;
    }
    if let Some(no_cache) = args::opt_bool(args, "no-cache")? {
        spec.no_cache = no_cache;
    }
    Ok(())
}

/// A non-negative integer argument. `args::opt_f64` accepts the string forms an agent
/// writes; this rejects the fractional and negative values that only make sense as typos.
pub fn opt_u32(args: &Value, key: &str) -> Result<Option<u32>> {
    let Some(value) = args::opt_f64(args, key)? else {
        return Ok(None);
    };
    if value < 0.0 || value.fract() != 0.0 || value > f64::from(u32::MAX) {
        return Err(Error::bad_args(format!(
            "field '{key}' must be a whole number, got {value}"
        )));
    }
    Ok(Some(value as u32))
}

impl ServerHandler for DvsServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("dvs", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Edit video as a JSON document. Call dvs_overview first: it names the sequences, \
                 tracks and assets every other tool addresses. Batch edits through dvs_apply \
                 (all-or-nothing, one digest back) rather than one op per turn, then look at the \
                 result with dvs_frame, dvs_render or dvs_lint. Times accept seconds, 'mm:ss.mmm', \
                 '1m12s' or '1800f', and every edit reports the frame it snapped to.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.inner.tools.clone()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.inner
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, McpError> {
        let name = request.name.to_string();
        let args = Value::Object(request.arguments.unwrap_or_default());
        let server = self.clone();
        let tool = name.clone();
        let joined =
            tokio::task::spawn_blocking(move || server.call(&tool, args)).await;
        let result = match joined {
            Ok(Ok(outcome)) => success(outcome),
            Ok(Err(error)) => CallToolResult::structured_error(json!({
                "error": error.to_report(),
                "tool": name,
            })),
            // A panicking op is a bug, but reporting it as a tool failure keeps the
            // session alive and tells the agent which tool to stop calling.
            Err(join) => CallToolResult::structured_error(json!({
                "error": {
                    "kind": "panic",
                    "code": dvs_core::exit::OP_ERROR,
                    "message": format!("tool '{name}' panicked: {join}"),
                },
                "tool": name,
            })),
        };
        Ok(result.into())
    }
}

/// A successful call, with the PNG attached as an image block when there is one. The
/// structured body is always present: a vision model reads the picture, the agent's code
/// reads the ids.
fn success(outcome: Outcome) -> CallToolResult {
    let mut result = CallToolResult::structured(outcome.value);
    if let Some(path) = outcome.image {
        match encode_png(&path) {
            Ok(data) => result
                .content
                .insert(0, ContentBlock::image(data, "image/png")),
            Err(error) => result.content.push(ContentBlock::text(format!(
                "the frame was written to {} but could not be attached: {error}",
                path.display()
            ))),
        }
    }
    result
}

fn encode_png(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// Serve MCP over stdio until the client disconnects.
///
/// `root` is any directory inside the project; the server resolves it per call the way the
/// CLI does, so a client that starts the server in a subdirectory still edits the project.
pub async fn serve(root: impl Into<PathBuf>) -> Result<()> {
    let server = DvsServer::new(root);
    let service = rmcp::serve_server(server, rmcp::transport::stdio())
        .await
        .map_err(|error| Error::op(format!("MCP server failed to start: {error}")))?;
    service
        .waiting()
        .await
        .map_err(|error| Error::op(format!("MCP server stopped: {error}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::project::{Project, Track, TrackKind};
    use dvs_core::vfs::FsVfs;
    use dvs_core::ProjectPaths;

    fn project_at(root: &Path) -> DvsServer {
        let paths = ProjectPaths::new(root);
        let mut project = Project::new("batch", Fps::new(30, 1).unwrap(), [320, 240], 48000);
        let seq = project.active_sequence.clone();
        project
            .sequences
            .get_mut(&seq)
            .unwrap()
            .tracks
            .push(Track::new("V1", TrackKind::Video));
        Workspace::create(paths, project, FsVfs::shared()).unwrap();
        DvsServer::new(root)
    }

    fn document(root: &Path) -> Value {
        let bytes = std::fs::read(root.join("project.json")).unwrap();
        let mut value: Value = serde_json::from_slice(&bytes).unwrap();
        // `modified` is a write stamp, not document state; the engine bumps it on every
        // save and undo deliberately does not rewind it.
        value.as_object_mut().unwrap().remove("modified");
        value
    }

    /// The transaction guarantee: a batch whose last op fails must leave nothing behind,
    /// including the history entries the earlier ops would have earned.
    #[test]
    fn a_failing_batch_rolls_back_every_op() {
        let dir = tempfile::tempdir().unwrap();
        let server = project_at(dir.path());
        let before = document(dir.path());

        let error = server
            .call(
                "dvs_apply",
                json!({
                    "ops": [
                        { "op": "track.add", "args": { "kind": "video", "name": "V2" } },
                        { "op": "marker.add", "args": { "at": "1", "name": "cue" } },
                        { "op": "clip.split", "args": { "target": "#nope", "at": "1" } }
                    ]
                }),
            )
            .expect_err("the third op cannot resolve its clip");
        assert_eq!(error.to_report().kind, "no-match");

        assert_eq!(
            document(dir.path()),
            before,
            "an op that failed mid-batch left its predecessors' edits on disk"
        );
        let history = dir.path().join("history.jsonl");
        assert!(
            !history.exists() || std::fs::read(&history).unwrap().is_empty(),
            "a rolled-back batch must not journal anything"
        );
    }

    /// The same batch without the failing op commits all of it, so the rollback test above
    /// is proving rollback rather than a batch that never worked.
    #[test]
    fn a_whole_batch_commits_together() {
        let dir = tempfile::tempdir().unwrap();
        let server = project_at(dir.path());
        let outcome = server
            .call(
                "dvs_apply",
                json!({
                    "ops": [
                        { "op": "track.add", "args": { "kind": "video", "name": "V2" } },
                        { "op": "marker.add", "args": { "at": "1", "name": "cue" } }
                    ]
                }),
            )
            .expect("both ops are valid");
        assert_eq!(outcome.value["ops"], json!(2));

        let reopened = Workspace::open_native(dir.path()).unwrap();
        let sequence = reopened
            .project
            .sequence(&reopened.project.active_sequence)
            .unwrap();
        assert_eq!(sequence.tracks.len(), 2);
        assert_eq!(sequence.markers.len(), 1);
        assert_eq!(reopened.journal.len(), 2, "each op earns its own undo step");
    }

    /// A dry run reports the same effects and writes nothing, which is what makes it safe
    /// to plan a batch before committing it.
    #[test]
    fn a_dry_run_batch_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let server = project_at(dir.path());
        let before = document(dir.path());
        let outcome = server
            .call(
                "dvs_apply",
                json!({
                    "ops": [{ "op": "track.add", "args": { "kind": "audio", "name": "A1" } }],
                    "dry-run": true
                }),
            )
            .unwrap();
        assert_eq!(outcome.value["committed"], json!(false));
        assert!(
            !outcome.value["applied"][0]["created"]
                .as_array()
                .unwrap()
                .is_empty(),
            "a dry run still reports what it would have created"
        );
        assert_eq!(document(dir.path()), before);
    }

    /// An unknown tool must name real ones: a wrong guess should cost one round trip, not
    /// a search through the whole catalog.
    #[test]
    fn an_unknown_tool_is_a_no_match_with_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let server = project_at(dir.path());
        let error = server.call("clip_spilt", json!({})).unwrap_err();
        let report = error.to_report();
        assert_eq!(report.code, dvs_core::exit::NO_MATCH);
        assert!(
            report
                .candidates
                .iter()
                .all(|candidate| candidate.starts_with("clip_")),
            "candidates {:?} are not tools from the namespace that was asked for",
            report.candidates
        );
    }

    #[test]
    fn overview_reports_the_document_without_ffmpeg() {
        let dir = tempfile::tempdir().unwrap();
        let server = project_at(dir.path());
        let outcome = server.call("dvs_overview", Value::Null).unwrap();
        assert_eq!(outcome.value["project"]["name"], json!("batch"));
        assert_eq!(outcome.value["sequences"][0]["tracks"][0]["name"], json!("V1"));
        assert_eq!(outcome.value["sequences"][0]["active"], json!(true));
    }
}
