//! `dvs op <id> --key value` and `dvs schema`: the registry as a command line.
//!
//! An op's arguments are a JSON Schema decided at runtime, so they cannot be clap flags.
//! This module reads the schema and interprets the leftover tokens against it, which buys
//! two things a hand-written flag table cannot:
//!
//! * a new op is immediately callable — no CLI change, no drift between what
//!   `dvs schema --op` prints and what `dvs op` accepts;
//! * an unknown flag fails with the op's *real* argument names (exit 2) instead of being
//!   silently dropped, which for an agent is the difference between a corrected retry and
//!   a confident belief in an edit that never happened.
//!
//! Global flags stay global even here: `dvs op clip.split --at 1 --json` works, because
//! the trailing token list is scanned for the handful of reserved names before the schema
//! is consulted. An op cannot declare an argument called `json`, `seq`, `project`,
//! `quiet`, `dry-run`, `list` or `json-args`; nothing in the catalog does.

use crate::cli::{OpArgs, SchemaArgs};
use crate::commands::{registry, Ctx};
use crate::output::Out;
use dvs_core::error::{Error, Result};
use dvs_mcp::tool_name;
use serde_json::{json, Map, Value};
use std::path::PathBuf;

/// Flag names the generic form never passes to an op.
const RESERVED: &[&str] = &[
    "json",
    "dry-run",
    "quiet",
    "seq",
    "project",
    "list",
    "json-args",
];

pub fn run(ctx: &Ctx, args: OpArgs) -> Result<()> {
    if args.list {
        return catalog(ctx);
    }
    let id = args
        .id
        .as_deref()
        .ok_or_else(|| Error::bad_args("missing op id; 'dvs op --list' shows every op"))?;
    let op = registry().get(id)?;

    let mut overrides = Overrides::default();
    let mut json_args = args.json_args.clone();
    let flags = scan(&args.rest, &mut overrides, &mut json_args)?;
    let ctx = overrides.apply(ctx);

    let schema = op.schema();
    let mut object = match json_args {
        Some(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map,
            Ok(_) => return Err(Error::bad_args("--json-args must be a JSON object")),
            Err(error) => {
                return Err(Error::bad_args(format!("--json-args is not valid JSON: {error}")))
            }
        },
        None => Map::new(),
    };
    for (key, values) in flags {
        object.insert(key.clone(), coerce(&schema, &key, &values, id)?);
    }

    ctx.apply(id, Value::Object(object)).map(|_| ())
}

/// `dvs op --list`: every op with its arguments, which is how an agent discovers the
/// surface without a docs round trip. The MCP tool name travels with each entry so one
/// listing serves both front ends.
fn catalog(ctx: &Ctx) -> Result<()> {
    let mut summary = Vec::new();
    let ops: Vec<Value> = registry()
        .catalog()
        .into_iter()
        .map(|info| {
            summary.push(format!("{:<24} {}", info.id, info.about));
            json!({
                "id": info.id,
                "about": info.about,
                "query": info.query,
                "network": info.network,
                "tools": info.tools,
                "mcpTool": tool_name(info.id),
                "schema": info.schema,
            })
        })
        .collect();
    let value = json!({ "count": ops.len(), "ops": ops });
    ctx.out.report(&value, || summary.join("\n"));
    Ok(())
}

/// `dvs schema`: argument schemas by default, the document schema on request. Both are
/// published so a caller can validate before it calls rather than after it fails.
pub fn schema(ctx: &Ctx, args: SchemaArgs) -> Result<()> {
    let value = match (&args.op, args.project_schema) {
        (Some(id), _) => {
            let op = registry().get(id)?;
            json!({
                "id": op.id(),
                "about": op.about(),
                "query": op.is_query(),
                "mcpTool": tool_name(op.id()),
                "schema": op.schema(),
            })
        }
        (None, true) => dvs_core::project_schema(),
        (None, false) => {
            let mut ops = Map::new();
            for op in registry().iter() {
                ops.insert(op.id().to_string(), op.schema());
            }
            json!({ "ops": Value::Object(ops) })
        }
    };
    ctx.out.report(&value, || {
        serde_json::to_string_pretty(&value).unwrap_or_default()
    });
    Ok(())
}

/// Global flags that appeared after the op id, where clap could not see them.
#[derive(Debug, Default)]
struct Overrides {
    json: bool,
    quiet: bool,
    dry_run: bool,
    seq: Option<String>,
    project: Option<PathBuf>,
}

impl Overrides {
    fn apply(self, ctx: &Ctx) -> Ctx {
        Ctx {
            root: self.project.unwrap_or_else(|| ctx.root.clone()),
            seq: self.seq.or_else(|| ctx.seq.clone()),
            dry_run: ctx.dry_run || self.dry_run,
            out: Out::new(
                ctx.out.is_json() || self.json,
                ctx.out.is_quiet() || self.quiet,
            ),
        }
    }
}

/// Split the trailing tokens into `(key, values)` pairs, pulling out the reserved global
/// flags on the way. Values are kept as raw strings: what they mean depends on the
/// schema, which the caller consults.
fn scan(
    rest: &[String],
    overrides: &mut Overrides,
    json_args: &mut Option<String>,
) -> Result<Vec<(String, Vec<String>)>> {
    let mut flags: Vec<(String, Vec<String>)> = Vec::new();
    let mut index = 0;
    while index < rest.len() {
        let token = &rest[index];
        index += 1;
        let body = match token.strip_prefix("--") {
            Some(body) => body,
            None if token == "-q" => {
                overrides.quiet = true;
                continue;
            }
            None => {
                return Err(Error::bad_args(format!(
                    "unexpected value '{token}'; op arguments are given as --key value"
                )))
            }
        };
        let (key, inline) = match body.split_once('=') {
            Some((key, value)) => (key.to_string(), Some(value.to_string())),
            None => (body.to_string(), None),
        };
        if key.is_empty() {
            return Err(Error::bad_args("'--' is not an argument name"));
        }

        if RESERVED.contains(&key.as_str()) {
            match key.as_str() {
                "json" => overrides.json = true,
                "quiet" => overrides.quiet = true,
                "dry-run" => overrides.dry_run = true,
                "list" => {
                    return Err(Error::bad_args(
                        "--list takes no op id; run 'dvs op --list' on its own",
                    ))
                }
                "seq" => overrides.seq = Some(required_value(rest, &mut index, &inline, &key)?),
                "project" => {
                    overrides.project =
                        Some(PathBuf::from(required_value(rest, &mut index, &inline, &key)?))
                }
                "json-args" => {
                    *json_args = Some(required_value(rest, &mut index, &inline, &key)?)
                }
                other => unreachable!("reserved flag '{other}' is unhandled"),
            }
            continue;
        }

        let value = match inline {
            Some(value) => Some(value),
            None => match rest.get(index) {
                Some(next) if !next.starts_with("--") => {
                    index += 1;
                    Some(next.clone())
                }
                _ => None,
            },
        };
        match flags.iter_mut().find(|(name, _)| *name == key) {
            Some((_, values)) => values.push(value.unwrap_or_else(|| "true".to_string())),
            None => flags.push((key, vec![value.unwrap_or_else(|| "true".to_string())])),
        }
    }
    Ok(flags)
}

/// What an argument's schema says it is. A union of types (`["string", "number"]`, which
/// every time field uses) stays a string: ops parse `"42.5"` and `42.5` identically, and
/// keeping the text avoids turning `00:01:12` into nonsense.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bool,
    Number,
    Array,
    Text,
}

fn coerce(schema: &Value, key: &str, values: &[String], op: &str) -> Result<Value> {
    let property = property(schema, key).ok_or_else(|| unknown_flag(schema, key, op))?;
    let kind = kind_of(property);
    if values.len() > 1 && kind != Kind::Array {
        return Err(Error::bad_args(format!(
            "'--{key}' was given {} times but takes one value",
            values.len()
        )));
    }
    let raw = &values[0];
    match kind {
        // One occurrence stays a string so the op's own comma splitting applies;
        // repeating the flag is the explicit list form.
        Kind::Array if values.len() == 1 => Ok(Value::String(raw.clone())),
        Kind::Array => Ok(Value::Array(
            values.iter().cloned().map(Value::String).collect(),
        )),
        Kind::Bool => match raw.as_str() {
            "true" | "yes" | "1" => Ok(Value::Bool(true)),
            "false" | "no" | "0" => Ok(Value::Bool(false)),
            other => Err(Error::bad_args(format!(
                "'--{key}' is a flag; write '--{key}' or '--{key}=false', not '{other}'"
            ))),
        },
        Kind::Number => {
            let number: f64 = raw.trim().parse().map_err(|_| {
                Error::bad_args(format!("'--{key}' must be a number, got '{raw}'"))
            })?;
            // Integral values stay integers: an op that reads a count should not have to
            // defend against `6.0`, and the journal reads better without it.
            if number.fract() == 0.0 && number.abs() < 9.0e15 {
                Ok(json!(number as i64))
            } else {
                Ok(json!(number))
            }
        }
        Kind::Text => Ok(Value::String(raw.clone())),
    }
}

/// The value of a flag that must have one: inline after `=`, or the next token when that
/// token is not itself a flag. `--gain -6` works; `--gain --track A1` is a missing value.
fn required_value(
    rest: &[String],
    index: &mut usize,
    inline: &Option<String>,
    key: &str,
) -> Result<String> {
    if let Some(value) = inline {
        return Ok(value.clone());
    }
    match rest.get(*index) {
        Some(next) if !next.starts_with("--") => {
            *index += 1;
            Ok(next.clone())
        }
        _ => Err(Error::bad_args(format!("'--{key}' needs a value"))),
    }
}

fn property<'a>(schema: &'a Value, key: &str) -> Option<&'a Value> {
    schema.get("properties")?.get(key)
}

fn kind_of(property: &Value) -> Kind {
    let mut types: Vec<&str> = Vec::new();
    collect_types(property, &mut types);
    if types.iter().any(|kind| *kind == "array") || property.get("items").is_some() {
        return Kind::Array;
    }
    if types.iter().any(|kind| *kind == "string") {
        return Kind::Text;
    }
    if types
        .iter()
        .any(|kind| *kind == "number" || *kind == "integer")
    {
        return Kind::Number;
    }
    if types.iter().any(|kind| *kind == "boolean") {
        return Kind::Bool;
    }
    Kind::Text
}

/// Gather the type names a property allows, following the `oneOf`/`anyOf` unions the
/// catalog uses for "a path or a list of paths".
fn collect_types<'a>(property: &'a Value, into: &mut Vec<&'a str>) {
    match property.get("type") {
        Some(Value::String(name)) => into.push(name),
        Some(Value::Array(names)) => into.extend(names.iter().filter_map(Value::as_str)),
        _ => {}
    }
    for branch in ["oneOf", "anyOf", "allOf"] {
        if let Some(Value::Array(items)) = property.get(branch) {
            for item in items {
                collect_types(item, into);
            }
        }
    }
}

/// The error an agent should never have to ask a follow-up question about: it names the
/// arguments this op really takes.
fn unknown_flag(schema: &Value, key: &str, op: &str) -> Error {
    let mut names: Vec<&str> = schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().map(String::as_str).collect())
        .unwrap_or_default();
    names.sort_unstable();
    Error::bad_args(format!(
        "'{op}' has no argument '{key}'; it accepts: {}",
        if names.is_empty() {
            "(none)".to_string()
        } else {
            names.join(", ")
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_only(tokens: &[&str]) -> Vec<(String, Vec<String>)> {
        let owned: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
        let mut overrides = Overrides::default();
        let mut json_args = None;
        scan(&owned, &mut overrides, &mut json_args).unwrap()
    }

    #[test]
    fn values_come_from_the_next_token_or_an_equals_sign() {
        assert_eq!(
            scan_only(&["--at", "42.5", "--clip=#talk"]),
            vec![
                ("at".to_string(), vec!["42.5".to_string()]),
                ("clip".to_string(), vec!["#talk".to_string()]),
            ]
        );
    }

    /// A negative number is a value, not a flag: `--by -6` is how every gain op is called.
    #[test]
    fn negative_numbers_are_values() {
        assert_eq!(
            scan_only(&["--by", "-6"]),
            vec![("by".to_string(), vec!["-6".to_string()])]
        );
    }

    #[test]
    fn globals_after_the_op_id_are_still_globals() {
        let owned: Vec<String> = ["--at", "1", "--json", "--seq", "main", "--dry-run"]
            .iter()
            .map(|t| t.to_string())
            .collect();
        let mut overrides = Overrides::default();
        let mut json_args = None;
        let flags = scan(&owned, &mut overrides, &mut json_args).unwrap();
        assert_eq!(flags, vec![("at".to_string(), vec!["1".to_string()])]);
        assert!(overrides.json);
        assert!(overrides.dry_run);
        assert_eq!(overrides.seq.as_deref(), Some("main"));
    }

    #[test]
    fn a_flag_without_a_value_is_a_presence_flag() {
        let schema = json!({
            "type": "object",
            "properties": { "ripple": { "type": "boolean" } }
        });
        let value = coerce(&schema, "ripple", &["true".to_string()], "clip.remove").unwrap();
        assert_eq!(value, Value::Bool(true));
        let off = coerce(&schema, "ripple", &["false".to_string()], "clip.remove").unwrap();
        assert_eq!(off, Value::Bool(false));
    }

    /// Time fields are `["string", "number"]`; turning `00:01:12` into a float would be
    /// silent corruption, so unions keep their text.
    #[test]
    fn time_unions_stay_text() {
        let schema = json!({
            "type": "object",
            "properties": { "at": { "type": ["string", "number"] } }
        });
        let value = coerce(&schema, "at", &["00:01:12".to_string()], "clip.split").unwrap();
        assert_eq!(value, json!("00:01:12"));
    }

    #[test]
    fn numbers_keep_integers_integral() {
        let schema = json!({
            "type": "object",
            "properties": { "quality": { "type": "integer" }, "gain": { "type": "number" } }
        });
        assert_eq!(
            coerce(&schema, "quality", &["18".to_string()], "x").unwrap(),
            json!(18)
        );
        assert_eq!(
            coerce(&schema, "gain", &["-6.5".to_string()], "x").unwrap(),
            json!(-6.5)
        );
    }

    /// `asset.import` declares `path` as "a string or a list of strings"; repeating the
    /// flag has to produce the list form.
    #[test]
    fn repeated_flags_build_arrays_for_list_arguments() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" } } ] }
            }
        });
        let value = coerce(
            &schema,
            "path",
            &["a.mp4".to_string(), "b.mp4".to_string()],
            "asset.import",
        )
        .unwrap();
        assert_eq!(value, json!(["a.mp4", "b.mp4"]));
    }

    #[test]
    fn repeating_a_scalar_flag_is_an_error() {
        let schema = json!({ "type": "object", "properties": { "at": { "type": "string" } } });
        let error = coerce(
            &schema,
            "at",
            &["1".to_string(), "2".to_string()],
            "clip.split",
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), dvs_core::exit::BAD_ARGS);
    }

    #[test]
    fn an_unknown_flag_names_the_real_arguments() {
        let schema = json!({
            "type": "object",
            "properties": { "clip": { "type": "string" }, "at": { "type": "string" } }
        });
        let error =
            coerce(&schema, "when", &["1".to_string()], "clip.split").unwrap_err();
        assert_eq!(error.exit_code(), dvs_core::exit::BAD_ARGS);
        let message = error.to_string();
        assert!(message.contains("at"), "{message}");
        assert!(message.contains("clip"), "{message}");
    }

    #[test]
    fn a_bare_value_is_rejected_rather_than_guessed() {
        let owned = vec!["42.5".to_string()];
        let mut overrides = Overrides::default();
        let mut json_args = None;
        let error = scan(&owned, &mut overrides, &mut json_args).unwrap_err();
        assert_eq!(error.exit_code(), dvs_core::exit::BAD_ARGS);
    }
}
