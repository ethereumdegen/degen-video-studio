//! The op registry: one spine, four surfaces.
//!
//! An op is a named, schema-carrying, JSON-argument mutation of a [`Project`]. Register it
//! once and it becomes a CLI verb, an MCP tool, a GUI command, a journal entry and a line of
//! generated documentation — with undo for free, because the engine diffs the document
//! instead of asking each op for an inverse. Adding a feature is adding one op.

use crate::asset::AssetStore;
use crate::error::{Error, Result};
use crate::ids::SequenceId;
use crate::paths::ProjectPaths;
use crate::project::Project;
use crate::time::{Fps, Time};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;

/// What an op changed. This is the JSON an agent reads back after every call, so it names
/// ids rather than describing prose.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpEffect {
    /// Ids of things this op modified.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub created: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
    /// Non-fatal observations: a trim that hit the end of the source, a clip pushed by a
    /// ripple, a title whose text was truncated.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
    /// Frame-grid snapping the op performed. An agent asked for 42.5 s; the timeline is at
    /// 30000/1001, so the edit landed at frame 1274. Reporting it is the difference between
    /// a surprise and a known quantity.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub snapped: Vec<Snap>,
    /// Query ops put their payload here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl OpEffect {
    pub fn new() -> Self {
        OpEffect::default()
    }

    pub fn changed(mut self, id: impl ToString) -> Self {
        self.changed.push(id.to_string());
        self
    }

    pub fn created(mut self, id: impl ToString) -> Self {
        self.created.push(id.to_string());
        self
    }

    pub fn removed(mut self, id: impl ToString) -> Self {
        self.removed.push(id.to_string());
        self
    }

    pub fn warn(mut self, code: &'static str, target: impl ToString, detail: impl Into<String>) -> Self {
        self.warnings.push(Warning {
            code,
            target: target.to_string(),
            detail: detail.into(),
        });
        self
    }

    pub fn snap(mut self, field: &'static str, requested: Time, applied: Time, fps: Fps) -> Self {
        if requested != applied {
            self.snapped.push(Snap {
                field,
                requested: requested.to_string(),
                applied: applied.to_string(),
                frame: applied.frame_round(fps),
            });
        }
        self
    }

    pub fn data(mut self, value: serde_json::Value) -> Self {
        self.data = Some(value);
        self
    }

    pub fn merge(&mut self, other: OpEffect) {
        self.changed.extend(other.changed);
        self.created.extend(other.created);
        self.removed.extend(other.removed);
        self.warnings.extend(other.warnings);
        self.snapped.extend(other.snapped);
        if let Some(data) = other.data {
            self.data = Some(data);
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Warning {
    pub code: &'static str,
    pub target: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snap {
    pub field: &'static str,
    pub requested: String,
    pub applied: String,
    pub frame: i64,
}

/// Everything an op may touch besides the document: the project directory, the asset store,
/// the sequence the caller selected, and whether this is a dry run.
pub struct OpCx<'a> {
    pub paths: &'a ProjectPaths,
    pub assets: &'a AssetStore,
    pub dry_run: bool,
    sequence_hint: Option<String>,
}

impl<'a> OpCx<'a> {
    pub fn new(paths: &'a ProjectPaths, assets: &'a AssetStore) -> Self {
        OpCx {
            paths,
            assets,
            dry_run: false,
            sequence_hint: None,
        }
    }

    pub fn with_sequence(mut self, sequence: Option<String>) -> Self {
        self.sequence_hint = sequence.filter(|s| !s.is_empty());
        self
    }

    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    pub fn sequence_hint(&self) -> Option<&str> {
        self.sequence_hint.as_deref()
    }

    /// The sequence this op targets: `--seq` if given, else the project's active one.
    pub fn sequence(&self, project: &Project) -> Result<SequenceId> {
        project.resolve_sequence(self.sequence_hint.as_deref())
    }
}

pub trait Op: Send + Sync {
    /// Stable dotted identifier, e.g. `clip.split`. This is the CLI verb and the MCP tool
    /// name; it is part of the public contract and never renamed silently.
    fn id(&self) -> &'static str;

    /// One line, used by `--help`, the MCP tool description and the generated reference.
    fn about(&self) -> &'static str;

    /// JSON Schema for the argument object.
    fn schema(&self) -> serde_json::Value;

    /// Query ops never mutate and are never journaled.
    fn is_query(&self) -> bool {
        false
    }

    /// Ops that reach the network, so `--offline` and budget checks can gate them.
    fn is_network(&self) -> bool {
        false
    }

    /// Ops that shell out to ffmpeg or a model, so `doctor` can explain a missing tool
    /// before the op fails halfway through.
    fn needs_tools(&self) -> &'static [&'static str] {
        &[]
    }

    fn apply(&self, project: &mut Project, args: serde_json::Value, cx: &mut OpCx)
        -> Result<OpEffect>;
}

#[derive(Default, Clone)]
pub struct Registry {
    ops: BTreeMap<&'static str, Arc<dyn Op>>,
}

impl Registry {
    pub fn new() -> Self {
        Registry::default()
    }

    /// Duplicate ids are a programming error, not a runtime condition: two ops answering to
    /// one name would make the CLI and MCP surfaces ambiguous.
    pub fn register(&mut self, op: impl Op + 'static) -> &mut Self {
        let op: Arc<dyn Op> = Arc::new(op);
        if self.ops.insert(op.id(), op.clone()).is_some() {
            panic!("duplicate op id '{}'", op.id());
        }
        self
    }

    pub fn extend(&mut self, ops: Vec<Box<dyn Op>>) -> &mut Self {
        for op in ops {
            let op: Arc<dyn Op> = Arc::from(op);
            if self.ops.insert(op.id(), op.clone()).is_some() {
                panic!("duplicate op id '{}'", op.id());
            }
        }
        self
    }

    pub fn get(&self, id: &str) -> Result<&Arc<dyn Op>> {
        self.ops.get(id).ok_or_else(|| {
            Error::no_match("op", id, self.suggestions(id))
        })
    }

    pub fn ids(&self) -> Vec<&'static str> {
        self.ops.keys().copied().collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Op>> {
        self.ops.values()
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Ops whose id shares a namespace or a substring with the query. A wrong op name is
    /// usually a wrong namespace, so `clip.cut` should point at the `clip.*` family.
    pub fn suggestions(&self, query: &str) -> Vec<String> {
        let namespace = query.split('.').next().unwrap_or(query);
        let mut matches: Vec<String> = self
            .ops
            .keys()
            .filter(|id| id.starts_with(namespace) || id.contains(query))
            .map(|id| id.to_string())
            .collect();
        matches.truncate(12);
        matches
    }

    /// The machine-readable catalog behind `dvs op --list` and the MCP tool list.
    pub fn catalog(&self) -> Vec<OpInfo> {
        self.ops
            .values()
            .map(|op| OpInfo {
                id: op.id(),
                about: op.about(),
                query: op.is_query(),
                network: op.is_network(),
                tools: op.needs_tools(),
                schema: op.schema(),
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpInfo {
    pub id: &'static str,
    pub about: &'static str,
    pub query: bool,
    pub network: bool,
    pub tools: &'static [&'static str],
    pub schema: serde_json::Value,
}

/// Argument helpers. Ops are handed `serde_json::Value`, and an agent will pass `"12.5"`
/// where the schema says number and `30` where it says string; these accept both and fail
/// with a message that names the field.
pub mod args {
    use super::*;
    use crate::time::{Rat, Span};

    pub fn object(args: &serde_json::Value) -> Result<&serde_json::Map<String, serde_json::Value>> {
        args.as_object()
            .ok_or_else(|| Error::bad_args("arguments must be a JSON object"))
    }

    pub fn opt_str<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
        args.get(key).and_then(|v| v.as_str())
    }

    pub fn str_field<'a>(args: &'a serde_json::Value, key: &str) -> Result<&'a str> {
        opt_str(args, key).ok_or_else(|| Error::bad_args(format!("missing string field '{key}'")))
    }

    pub fn opt_f64(args: &serde_json::Value, key: &str) -> Result<Option<f64>> {
        match args.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::Number(n)) => Ok(n.as_f64()),
            Some(serde_json::Value::String(s)) => s
                .trim()
                .parse::<f64>()
                .map(Some)
                .map_err(|_| Error::bad_args(format!("field '{key}' is not a number: '{s}'"))),
            Some(other) => Err(Error::bad_args(format!(
                "field '{key}' must be a number, got {other}"
            ))),
        }
    }

    pub fn f64_field(args: &serde_json::Value, key: &str) -> Result<f64> {
        opt_f64(args, key)?.ok_or_else(|| Error::bad_args(format!("missing number field '{key}'")))
    }

    pub fn opt_bool(args: &serde_json::Value, key: &str) -> Result<Option<bool>> {
        match args.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::Bool(b)) => Ok(Some(*b)),
            Some(serde_json::Value::String(s)) => match s.to_ascii_lowercase().as_str() {
                "true" | "yes" | "1" => Ok(Some(true)),
                "false" | "no" | "0" => Ok(Some(false)),
                other => Err(Error::bad_args(format!(
                    "field '{key}' is not a boolean: '{other}'"
                ))),
            },
            Some(other) => Err(Error::bad_args(format!(
                "field '{key}' must be a boolean, got {other}"
            ))),
        }
    }

    /// A time field, parsed with the sequence frame rate so `1800f` and `00:01:00:12` work.
    pub fn opt_time(args: &serde_json::Value, key: &str, fps: Fps) -> Result<Option<Time>> {
        match args.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::String(s)) => Time::parse_with_fps(s, fps)
                .map(Some)
                .map_err(|e| Error::bad_args(format!("field '{key}': {e}"))),
            Some(serde_json::Value::Number(n)) => Time::parse(&n.to_string())
                .map(Some)
                .map_err(|e| Error::bad_args(format!("field '{key}': {e}"))),
            Some(other) => Err(Error::bad_args(format!(
                "field '{key}' must be a time, got {other}"
            ))),
        }
    }

    pub fn time_field(args: &serde_json::Value, key: &str, fps: Fps) -> Result<Time> {
        opt_time(args, key, fps)?
            .ok_or_else(|| Error::bad_args(format!("missing time field '{key}'")))
    }

    pub fn opt_rat(args: &serde_json::Value, key: &str) -> Result<Option<Rat>> {
        match args.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::String(s)) => Rat::parse(s)
                .map(Some)
                .map_err(|e| Error::bad_args(format!("field '{key}': {e}"))),
            Some(serde_json::Value::Number(n)) => Rat::parse(&n.to_string())
                .map(Some)
                .map_err(|e| Error::bad_args(format!("field '{key}': {e}"))),
            Some(other) => Err(Error::bad_args(format!(
                "field '{key}' must be a rational, got {other}"
            ))),
        }
    }

    /// A `start`/`end` or `start`/`duration` pair.
    pub fn opt_span(args: &serde_json::Value, fps: Fps) -> Result<Option<Span>> {
        let start = opt_time(args, "start", fps)?;
        let end = opt_time(args, "end", fps)?;
        let duration = opt_time(args, "duration", fps)?;
        match (start, end, duration) {
            (Some(start), Some(end), _) => Ok(Some(Span::new(start, end))),
            (Some(start), None, Some(duration)) => Ok(Some(Span::from_duration(start, duration))),
            (None, None, None) => Ok(None),
            _ => Err(Error::bad_args(
                "a range needs 'start' with either 'end' or 'duration'",
            )),
        }
    }

    pub fn string_list(args: &serde_json::Value, key: &str) -> Result<Vec<String>> {
        match args.get(key) {
            None | Some(serde_json::Value::Null) => Ok(Vec::new()),
            Some(serde_json::Value::String(s)) => {
                Ok(s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
            }
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(|s| s.to_string())
                        .ok_or_else(|| Error::bad_args(format!("field '{key}' must be strings")))
                })
                .collect(),
            Some(other) => Err(Error::bad_args(format!(
                "field '{key}' must be a list, got {other}"
            ))),
        }
    }

    /// Reject argument keys the schema does not declare. A silently ignored typo
    /// (`--durations 4`) is worse than an error: the agent believes the edit happened.
    pub fn reject_unknown(args: &serde_json::Value, known: &[&str]) -> Result<()> {
        let object = object(args)?;
        let unknown: Vec<&String> = object.keys().filter(|key| !known.contains(&key.as_str())).collect();
        if unknown.is_empty() {
            return Ok(());
        }
        Err(Error::bad_args(format!(
            "unknown argument(s): {}; accepted: {}",
            unknown
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            known.join(", ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::Fps;

    struct Noop;

    impl Op for Noop {
        fn id(&self) -> &'static str {
            "clip.split"
        }
        fn about(&self) -> &'static str {
            "split a clip"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        fn apply(
            &self,
            _project: &mut Project,
            _args: serde_json::Value,
            _cx: &mut OpCx,
        ) -> Result<OpEffect> {
            Ok(OpEffect::new())
        }
    }

    #[test]
    fn unknown_op_suggests_the_namespace() {
        let mut registry = Registry::new();
        registry.register(Noop);
        let err = registry.get("clip.cut").err().expect("unknown op must fail");
        assert_eq!(err.exit_code(), crate::error::exit::NO_MATCH);
        assert!(err.to_string().contains("clip.split"), "{err}");
    }

    #[test]
    #[should_panic(expected = "duplicate op id")]
    fn duplicate_registration_is_a_hard_error() {
        let mut registry = Registry::new();
        registry.register(Noop);
        registry.register(Noop);
    }

    #[test]
    fn snap_is_reported_only_when_the_value_moved() {
        let fps = Fps::new(30000, 1001).unwrap();
        let requested = Time::parse("42.5").unwrap();
        let effect = OpEffect::new().snap("at", requested, requested.snap(fps), fps);
        assert_eq!(effect.snapped.len(), 1);
        assert_eq!(effect.snapped[0].frame, 1274);

        let aligned = Time::from_frames(10, fps);
        let effect = OpEffect::new().snap("at", aligned, aligned.snap(fps), fps);
        assert!(effect.snapped.is_empty());
    }

    #[test]
    fn time_args_accept_every_agent_spelling() {
        let fps = Fps::new(30, 1).unwrap();
        let args = serde_json::json!({ "at": "1m12.5s", "num": 42.5, "frames": "300f" });
        assert_eq!(
            args::opt_time(&args, "at", fps).unwrap().unwrap(),
            Time::new(145, 2).unwrap()
        );
        assert_eq!(
            args::opt_time(&args, "num", fps).unwrap().unwrap(),
            Time::new(85, 2).unwrap()
        );
        assert_eq!(
            args::opt_time(&args, "frames", fps).unwrap().unwrap(),
            Time::from_secs(10)
        );
        assert!(args::opt_time(&args, "missing", fps).unwrap().is_none());
    }

    #[test]
    fn typos_in_argument_names_are_rejected() {
        let args = serde_json::json!({ "at": "1", "durations": 4 });
        let err = args::reject_unknown(&args, &["at", "duration"]).unwrap_err();
        assert!(err.to_string().contains("durations"), "{err}");
        assert_eq!(err.exit_code(), crate::error::exit::BAD_ARGS);
    }

    #[test]
    fn a_range_needs_a_consistent_pair() {
        let fps = Fps::new(30, 1).unwrap();
        assert!(args::opt_span(&serde_json::json!({ "start": "1" }), fps).is_err());
        let span = args::opt_span(&serde_json::json!({ "start": "1", "duration": "2" }), fps)
            .unwrap()
            .unwrap();
        assert_eq!(span.end, Time::from_secs(3));
    }
}
