//! Titles: SVG documents with `{{field}}` placeholders, placed on the timeline as clips.
//!
//! A title is markup in `project.json`, not a rendered image, for two reasons: the text stays
//! diffable and searchable (so `digest` can report what the title says and lint can measure
//! whether it overflows), and the same document can be re-rendered at any size or re-worded
//! without touching the asset store.
//!
//! The built-in templates below are complete SVG documents. They use `font-family` and no
//! external reference of any kind — no `<image>`, no `xlink:href`, no web font — so the
//! renderer can rasterize one with nothing but the document in hand. Their geometry is
//! resolved against the sequence size when the title is created; only `{{title}}` and
//! `{{subtitle}}` survive as live fields.
//!
//! Setting a field the SVG never references is reported as an `unused-field` warning rather
//! than accepted in silence: it is nearly always a misspelled placeholder, and the alternative
//! is an op that reports success and changes nothing on screen.

use crate::error::{Error, Result};
use crate::ids::{SequenceId, TitleId};
use crate::op::{args, Op, OpCx, OpEffect, Registry};
use crate::ops::util::{
    assert_free, assert_takes_clips, assert_unlocked, resolve_title, sequence_fps,
};
use crate::project::{Clip, Project, Source, Title, Track, TrackKind};
use crate::selector;
use crate::time::{Span, Time};
use indexmap::IndexMap;

pub fn register(registry: &mut Registry) {
    registry
        .register(TitleAdd)
        .register(TitleSetText)
        .register(TitleLowerThird)
        .register(TitleRemove);
}

/// A name bar in the lower third: one rounded slab, one accent stripe, two text runs.
const LOWER_THIRD: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="{{w}}" height="{{h}}" viewBox="0 0 {{w}} {{h}}">
  <rect x="{{barX}}" y="{{barY}}" width="{{barW}}" height="{{barH}}" rx="{{barR}}" ry="{{barR}}" fill="#12161c" fill-opacity="0.85"/>
  <rect x="{{barX}}" y="{{barY}}" width="{{accentW}}" height="{{barH}}" rx="{{accentR}}" ry="{{accentR}}" fill="#fb8500"/>
  <text x="{{textX}}" y="{{titleY}}" font-family="sans-serif" font-size="{{titleSize}}" font-weight="700" fill="#ffffff">{{title}}</text>
  <text x="{{textX}}" y="{{subY}}" font-family="sans-serif" font-size="{{subSize}}" fill="#b9c4cf">{{subtitle}}</text>
</svg>
"##;

/// A centered card, for a section break or a quote.
const CAPTION_CARD: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="{{w}}" height="{{h}}" viewBox="0 0 {{w}} {{h}}">
  <rect x="{{cardX}}" y="{{cardY}}" width="{{cardW}}" height="{{cardH}}" rx="{{cardR}}" ry="{{cardR}}" fill="#12161c" fill-opacity="0.9"/>
  <text x="{{midX}}" y="{{cardTitleY}}" text-anchor="middle" font-family="sans-serif" font-size="{{titleSize}}" font-weight="700" fill="#ffffff">{{title}}</text>
  <text x="{{midX}}" y="{{cardSubY}}" text-anchor="middle" font-family="sans-serif" font-size="{{subSize}}" fill="#b9c4cf">{{subtitle}}</text>
</svg>
"##;

/// Type over the picture, no background, for an opening card.
const FULL_SCREEN: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="{{w}}" height="{{h}}" viewBox="0 0 {{w}} {{h}}">
  <text x="{{midX}}" y="{{heroY}}" text-anchor="middle" font-family="sans-serif" font-size="{{heroSize}}" font-weight="700" fill="#ffffff">{{title}}</text>
  <text x="{{midX}}" y="{{heroSubY}}" text-anchor="middle" font-family="sans-serif" font-size="{{titleSize}}" fill="#b9c4cf">{{subtitle}}</text>
</svg>
"##;

pub const TEMPLATES: &[&str] = &["lower-third", "caption-card", "full-screen"];

fn template_named(name: &str) -> Result<&'static str> {
    match name.trim() {
        "lower-third" => Ok(LOWER_THIRD),
        "caption-card" => Ok(CAPTION_CARD),
        "full-screen" => Ok(FULL_SCREEN),
        other => Err(Error::no_match(
            "title template",
            other,
            TEMPLATES.iter().map(|name| name.to_string()).collect(),
        )),
    }
}

/// How long a title stays on screen when the caller does not say. Four seconds is the
/// broadcast habit for a lower third and long enough to read two lines.
fn default_duration() -> Time {
    Time::from_secs(4)
}

/// Resolve a template's geometry against the sequence size.
///
/// The templates are laid out in fractions of the frame rather than at a fixed 1080p, so a
/// vertical sequence gets a title proportioned for it instead of a stretched one. Substituting
/// here rather than at render time keeps the stored SVG a complete document: what is in
/// `project.json` is what gets rasterized, minus the two text fields.
fn instantiate(template: &str, size: [u32; 2]) -> String {
    let w = f64::from(size[0]);
    let h = f64::from(size[1]);
    let px = |value: f64| (value.round() as i64).to_string();
    let bar_x = 0.06 * w;
    let bar_y = 0.74 * h;
    let bar_h = 0.16 * h;
    let accent_w = (0.005 * w).max(4.0);
    let card_x = 0.15 * w;
    let card_y = 0.38 * h;
    let tokens = [
        ("w", px(w)),
        ("h", px(h)),
        ("barX", px(bar_x)),
        ("barY", px(bar_y)),
        ("barW", px(0.56 * w)),
        ("barH", px(bar_h)),
        ("barR", px(0.018 * h)),
        ("accentW", px(accent_w)),
        ("accentR", px(accent_w / 2.0)),
        ("textX", px(bar_x + 0.035 * w)),
        ("titleY", px(bar_y + 0.062 * h)),
        ("titleSize", px(0.058 * h)),
        ("subY", px(bar_y + 0.115 * h)),
        ("subSize", px(0.034 * h)),
        ("cardX", px(card_x)),
        ("cardY", px(card_y)),
        ("cardW", px(0.7 * w)),
        ("cardH", px(0.24 * h)),
        ("cardR", px(0.02 * h)),
        ("midX", px(0.5 * w)),
        ("cardTitleY", px(card_y + 0.11 * h)),
        ("cardSubY", px(card_y + 0.175 * h)),
        ("heroY", px(0.46 * h)),
        ("heroSize", px(0.11 * h)),
        ("heroSubY", px(0.56 * h)),
    ];
    let mut out = template.to_string();
    for (token, value) in tokens {
        out = out.replace(&format!("{{{{{token}}}}}"), &value);
    }
    out
}

/// The `{{field}}` names a document references, in order of first appearance. This is what
/// `set-text` reports back and what an unused field is checked against.
fn placeholders(svg: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = svg;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            break;
        };
        let name = after[..close].trim();
        let plausible = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
        if plausible && !out.iter().any(|existing| existing == name) {
            out.push(name.to_string());
        }
        rest = &after[close + 2..];
    }
    out
}

/// `--field title=Andy --field sub="degen labs"` from a CLI, `{"title": "Andy"}` from MCP.
/// A single string is one pair, deliberately: splitting on commas would mangle every value
/// that contains one.
fn parse_fields(args: &serde_json::Value, key: &str) -> Result<IndexMap<String, String>> {
    let mut out: IndexMap<String, String> = IndexMap::new();
    let add_pair = |text: &str, out: &mut IndexMap<String, String>| -> Result<()> {
        let (name, value) = text.split_once('=').ok_or_else(|| {
            Error::bad_args(format!(
                "field '{key}' entries look like name=value; got '{text}'"
            ))
        })?;
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::bad_args(format!(
                "field '{key}' entry '{text}' has no name"
            )));
        }
        out.insert(name.to_string(), value.to_string());
        Ok(())
    };
    match args.get(key) {
        None | Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::Object(map)) => {
            for (name, value) in map {
                let text = match value {
                    serde_json::Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                out.insert(name.clone(), text);
            }
        }
        Some(serde_json::Value::Array(items)) => {
            for item in items {
                let text = item.as_str().ok_or_else(|| {
                    Error::bad_args(format!(
                        "field '{key}' entries must be 'name=value' strings"
                    ))
                })?;
                add_pair(text, &mut out)?;
            }
        }
        Some(serde_json::Value::String(text)) => add_pair(text, &mut out)?,
        Some(other) => {
            return Err(Error::bad_args(format!(
                "field '{key}' must be name=value, a list of them, or an object; got {other}"
            )))
        }
    }
    Ok(out)
}

/// Store fields, warning about the ones the document never asked for. The value is kept even
/// when unused — it is data the caller meant — but the warning is what stops a typo'd
/// placeholder name from looking like a successful edit.
fn apply_fields(
    title: &mut Title,
    fields: IndexMap<String, String>,
    mut effect: OpEffect,
) -> OpEffect {
    let known = placeholders(&title.svg);
    for (name, value) in fields {
        if !known.iter().any(|placeholder| *placeholder == name) {
            let has = if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            };
            effect = effect.warn(
                "unused-field",
                &title.id,
                format!(
                    "field '{name}' is not referenced by title '{}'; its placeholders are {has}",
                    title.name
                ),
            );
        }
        title.fields.insert(name, value);
    }
    effect
}

/// `--svg` is either the document itself, which is what an agent pastes, or a path to one.
///
/// The markup is copied into `project.json` rather than imported into the asset store so the
/// title stays self-contained and editable. The read goes through the asset store's
/// [`crate::vfs::Vfs`] like every other read in this crate, which is what lets the same op
/// run against an in-memory tree in a browser build instead of failing at run time.
fn load_svg(text: &str, cx: &OpCx) -> Result<String> {
    let trimmed = text.trim();
    let svg = if trimmed.starts_with('<') {
        trimmed.to_string()
    } else {
        let path = cx.paths.resolve(trimmed);
        let bytes = cx.assets.vfs().read(&path)?;
        String::from_utf8(bytes)
            .map_err(|_| Error::bad_args(format!("'{trimmed}' is not UTF-8 text")))?
    };
    if !svg.contains("<svg") {
        return Err(Error::bad_args(
            "svg has no <svg> element; pass SVG markup or the path to an SVG file",
        ));
    }
    Ok(svg)
}

fn title_name_taken(project: &Project, name: &str) -> bool {
    project.titles.values().any(|title| title.name == name)
}

fn unique_title_name(project: &Project, base: &str) -> String {
    if !title_name_taken(project, base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base} {n}"))
        .find(|candidate| !title_name_taken(project, candidate))
        .expect("an unbounded search terminates")
}

/// The name a new title gets: the caller's, else its own headline, else a generic one. Names
/// address titles, so an explicit collision is an error while a derived one is made unique.
fn new_title_name(
    project: &Project,
    explicit: Option<&str>,
    fields: &IndexMap<String, String>,
) -> Result<String> {
    if let Some(name) = explicit.map(str::trim).filter(|name| !name.is_empty()) {
        if title_name_taken(project, name) {
            return Err(Error::bad_args(format!(
                "title name '{name}' is already used; names address titles"
            )));
        }
        return Ok(name.to_string());
    }
    let base = fields
        .get("title")
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())
        .unwrap_or("title");
    Ok(unique_title_name(project, base))
}

/// Put a title on the timeline.
///
/// With no `track`, the first video track wins, and an empty timeline gets one: refusing to
/// place a title because the sequence has no tracks yet would fail the very first thing an
/// agent does with a fresh project, and the created track is reported like any other effect.
#[allow(clippy::too_many_arguments)]
fn place_title(
    project: &mut Project,
    seq: &SequenceId,
    title: &TitleId,
    label: &str,
    at: Time,
    duration: Time,
    track: Option<&str>,
    mut effect: OpEffect,
) -> Result<OpEffect> {
    let target = match track {
        Some(text) => selector::resolve_track(project, seq, text)?,
        None => {
            let existing = project
                .sequence(seq)?
                .tracks
                .iter()
                .find(|track| track.kind == TrackKind::Video)
                .map(|track| track.id.clone());
            match existing {
                Some(id) => id,
                None => {
                    let sequence = project.sequence_mut(seq)?;
                    let track = Track::new(
                        sequence.next_track_name(TrackKind::Video),
                        TrackKind::Video,
                    );
                    let id = track.id.clone();
                    sequence.tracks.push(track);
                    effect = effect.created(&id);
                    id
                }
            }
        }
    };

    let track = project.sequence_mut(seq)?.track_mut(&target)?;
    assert_unlocked(track)?;
    assert_takes_clips(track)?;
    if track.kind == TrackKind::Audio {
        return Err(Error::op(format!(
            "track '{}' carries audio; a title has nothing to play there",
            track.name
        )));
    }
    let span = Span::from_duration(at, duration);
    assert_free(track, span, None)?;

    let mut clip = Clip::new(
        Source::Title {
            title: title.clone(),
        },
        at,
        duration,
    );
    clip.name = Some(label.to_string());
    let clip_id = clip.id.clone();
    track.place(clip);
    Ok(effect.created(&clip_id).changed(&target))
}

pub struct TitleAdd;

impl Op for TitleAdd {
    fn id(&self) -> &'static str {
        "title.add"
    }

    fn about(&self) -> &'static str {
        "Create a title from a template or an SVG document, optionally placing it"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "template": {
                    "type": "string",
                    "enum": TEMPLATES,
                    "description": "Built-in template, sized to the sequence"
                },
                "svg": {
                    "type": "string",
                    "description": "SVG markup, or a path to an SVG file; alternative to template"
                },
                "field": {
                    "description": "Placeholder values: 'name=value', a list of those, or an object",
                    "examples": [["title=Andy Mazzola", "subtitle=degen labs"], { "title": "Andy Mazzola" }]
                },
                "name": { "type": "string", "description": "Title name; defaults to its own title text" },
                "at": { "type": "string", "description": "Place it as a clip at this time; omit to only create the document" },
                "duration": { "type": "string", "description": "Clip length when placing; defaults to 4s" },
                "track": { "type": "string", "description": "Track to place it on; defaults to the first video track" }
            },
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(
            &args,
            &["template", "svg", "field", "name", "at", "duration", "track"],
        )?;
        let seq = cx.sequence(project)?;
        let fps = sequence_fps(project, &seq)?;
        let size = project.sequence(&seq)?.size;

        let svg = match (args::opt_str(&args, "svg"), args::opt_str(&args, "template")) {
            (Some(_), Some(_)) => {
                return Err(Error::bad_args(
                    "title.add takes either svg or template, not both",
                ))
            }
            (Some(text), None) => load_svg(text, cx)?,
            (None, Some(name)) => instantiate(template_named(name)?, size),
            (None, None) => {
                return Err(Error::bad_args(format!(
                    "title.add needs svg or template (one of {})",
                    TEMPLATES.join(", ")
                )))
            }
        };
        let fields = parse_fields(&args, "field")?;
        let name = new_title_name(project, args::opt_str(&args, "name"), &fields)?;

        let mut title = Title {
            id: TitleId::new(),
            name: name.clone(),
            size,
            svg,
            fields: IndexMap::new(),
        };
        // Every placeholder starts as an empty field: an unset `{{title}}` would otherwise
        // render the placeholder text itself.
        for placeholder in placeholders(&title.svg) {
            title.fields.insert(placeholder, String::new());
        }
        let mut effect = apply_fields(&mut title, fields, OpEffect::new());
        let title_id = title.id.clone();
        effect = effect
            .created(&title_id)
            .data(serde_json::json!({ "placeholders": placeholders(&title.svg) }));
        project.titles.insert(title_id.clone(), title);

        if let Some(requested_at) = args::opt_time(&args, "at", fps)? {
            let at = requested_at.snap(fps);
            let requested_duration =
                args::opt_time(&args, "duration", fps)?.unwrap_or_else(default_duration);
            let duration = requested_duration.snap(fps);
            if !duration.is_positive() {
                return Err(Error::bad_args(format!(
                    "title duration {requested_duration} snaps to {duration} at {fps} fps"
                )));
            }
            effect = effect
                .snap("at", requested_at, at, fps)
                .snap("duration", requested_duration, duration, fps);
            effect = place_title(
                project,
                &seq,
                &title_id,
                &name,
                at,
                duration,
                args::opt_str(&args, "track"),
                effect,
            )?;
        }
        Ok(effect)
    }
}

pub struct TitleSetText;

impl Op for TitleSetText {
    fn id(&self) -> &'static str {
        "title.set-text"
    }

    fn about(&self) -> &'static str {
        "Set fields on a title, reporting the placeholders it really has"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Title id or name" },
                "field": {
                    "description": "Placeholder values: 'name=value', a list of those, or an object",
                    "examples": [["title=Andy Mazzola"], { "subtitle": "degen labs" }]
                }
            },
            "required": ["title", "field"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        _cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["title", "field"])?;
        let id = resolve_title(project, args::str_field(&args, "title")?)?;
        let fields = parse_fields(&args, "field")?;
        if fields.is_empty() {
            return Err(Error::bad_args(
                "title.set-text needs at least one field, e.g. title=Andy Mazzola",
            ));
        }
        let title = project
            .titles
            .get_mut(&id)
            .expect("the title resolved above");
        let effect = apply_fields(title, fields, OpEffect::new()).changed(&id);
        Ok(effect.data(serde_json::json!({
            "placeholders": placeholders(&title.svg),
            "fields": title.fields,
        })))
    }
}

pub struct TitleLowerThird;

impl Op for TitleLowerThird {
    fn id(&self) -> &'static str {
        "title.lower-third"
    }

    fn about(&self) -> &'static str {
        "Create and place a lower-third name bar in one call"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Headline, usually a name" },
                "sub": { "type": "string", "description": "Second line, usually a role or company" },
                "at": { "type": "string", "description": "Where it appears", "examples": ["00:00:02.000"] },
                "for": { "type": "string", "description": "How long it stays; defaults to 4s" },
                "track": { "type": "string", "description": "Track to place it on; defaults to the first video track" },
                "name": { "type": "string", "description": "Title name; defaults to the headline" }
            },
            "required": ["text", "at"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["text", "sub", "at", "for", "track", "name"])?;
        let seq = cx.sequence(project)?;
        let fps = sequence_fps(project, &seq)?;
        let size = project.sequence(&seq)?.size;
        let text = args::str_field(&args, "text")?.to_string();
        let sub = args::opt_str(&args, "sub").unwrap_or("").to_string();
        let requested_at = args::time_field(&args, "at", fps)?;
        let at = requested_at.snap(fps);
        if at.is_negative() {
            return Err(Error::bad_args(format!(
                "title position {requested_at} is before the start of the sequence"
            )));
        }
        let requested_duration = args::opt_time(&args, "for", fps)?.unwrap_or_else(default_duration);
        let duration = requested_duration.snap(fps);
        if !duration.is_positive() {
            return Err(Error::bad_args(format!(
                "title duration {requested_duration} snaps to {duration} at {fps} fps"
            )));
        }

        let mut fields: IndexMap<String, String> = IndexMap::new();
        fields.insert("title".into(), text.clone());
        // The subtitle is set even when empty: an unset placeholder would render as
        // literal `{{subtitle}}`.
        fields.insert("subtitle".into(), sub);
        let name = new_title_name(project, args::opt_str(&args, "name"), &fields)?;

        let mut title = Title {
            id: TitleId::new(),
            name: name.clone(),
            size,
            svg: instantiate(LOWER_THIRD, size),
            fields: IndexMap::new(),
        };
        let mut effect = apply_fields(&mut title, fields, OpEffect::new());
        let title_id = title.id.clone();
        project.titles.insert(title_id.clone(), title);
        effect = effect
            .created(&title_id)
            .snap("at", requested_at, at, fps)
            .snap("for", requested_duration, duration, fps);
        place_title(
            project,
            &seq,
            &title_id,
            &name,
            at,
            duration,
            args::opt_str(&args, "track"),
            effect,
        )
    }
}

pub struct TitleRemove;

impl Op for TitleRemove {
    fn id(&self) -> &'static str {
        "title.remove"
    }

    fn about(&self) -> &'static str {
        "Remove a title document; refuses while a clip still shows it"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Title id or name" }
            },
            "required": ["title"],
            "additionalProperties": false
        })
    }

    fn apply(
        &self,
        project: &mut Project,
        args: serde_json::Value,
        _cx: &mut OpCx,
    ) -> Result<OpEffect> {
        args::reject_unknown(&args, &["title"])?;
        let id = resolve_title(project, args::str_field(&args, "title")?)?;
        let name = project
            .titles
            .get(&id)
            .expect("the title resolved above")
            .name
            .clone();

        // Removing the document out from under a clip would leave a clip that renders
        // nothing, so the clips are named and the caller decides.
        let mut referrers: Vec<String> = Vec::new();
        for sequence in project.sequences.values() {
            for track in &sequence.tracks {
                for clip in &track.clips {
                    if matches!(&clip.source, Source::Title { title } if *title == id) {
                        referrers.push(format!(
                            "{}/{}/{}",
                            sequence.name,
                            track.name,
                            clip.label()
                        ));
                    }
                }
            }
        }
        if !referrers.is_empty() {
            return Err(Error::op(format!(
                "title '{name}' is still shown by clip(s) {}; remove those clips first",
                referrers.join(", ")
            )));
        }

        project.titles.shift_remove(&id);
        Ok(OpEffect::new().removed(&id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::AssetStore;
    use crate::ids::TrackId;
    use crate::paths::ProjectPaths;
    use crate::project::escape_xml;
    use crate::time::Fps;
    use crate::vfs::{MemVfs, Vfs};
    use std::sync::Arc;

    fn apply(op: &dyn Op, project: &mut Project, args: serde_json::Value) -> Result<OpEffect> {
        apply_in("/p", op, project, args)
    }

    fn apply_in(
        root: &str,
        op: &dyn Op,
        project: &mut Project,
        args: serde_json::Value,
    ) -> Result<OpEffect> {
        let paths = ProjectPaths::new(root);
        let assets = AssetStore::new(paths.assets_dir(), Arc::new(MemVfs::default()));
        let mut cx = OpCx::new(&paths, &assets);
        op.apply(project, args, &mut cx)
    }

    /// Same as [`apply_in`] but against a caller-supplied storage backend, so a test can
    /// place a file where the op will actually look for it.
    fn apply_with_vfs(
        vfs: Arc<MemVfs>,
        op: &dyn Op,
        project: &mut Project,
        args: serde_json::Value,
    ) -> Result<OpEffect> {
        let paths = ProjectPaths::new("/p");
        let assets = AssetStore::new(paths.assets_dir(), vfs);
        let mut cx = OpCx::new(&paths, &assets);
        op.apply(project, args, &mut cx)
    }

    fn project() -> Project {
        let mut project = Project::new(
            "promo",
            Fps::new(30, 1).expect("30 fps"),
            [1920, 1080],
            48_000,
        );
        let mut track = Track::new("V1", TrackKind::Video);
        track.id = TrackId::from_raw("trk_v1");
        let seq = project.active_sequence.clone();
        project.sequence_mut(&seq).expect("main").tracks.push(track);
        project
    }

    fn only_title(project: &Project) -> &Title {
        assert_eq!(project.titles.len(), 1, "expected exactly one title");
        project.titles.values().next().expect("one title")
    }

    fn clips(project: &Project) -> Vec<Clip> {
        let seq = project.active_sequence.clone();
        project.sequence(&seq).expect("main").tracks[0].clips.clone()
    }

    #[test]
    fn a_lower_third_carries_the_text_escaped_and_lands_as_a_clip() {
        let mut project = project();
        let effect = apply(
            &TitleLowerThird,
            &mut project,
            serde_json::json!({
                "text": "Andy & \"Co\" <degen>",
                "sub": "degen labs",
                "at": "2",
                "for": "3"
            }),
        )
        .expect("lower third");

        let title = only_title(&project);
        let resolved = title.resolved_svg();
        assert!(
            resolved.contains(&escape_xml("Andy & \"Co\" <degen>")),
            "the headline should be XML-escaped in the document: {resolved}"
        );
        assert!(
            !resolved.contains("Andy & \"Co\""),
            "raw ampersands and angle brackets would not parse as SVG: {resolved}"
        );
        assert!(resolved.contains("degen labs"));
        assert!(
            !resolved.contains("{{"),
            "every placeholder should be resolved: {resolved}"
        );

        let clips = clips(&project);
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].start, Time::from_secs(2));
        assert_eq!(clips[0].duration, Time::from_secs(3));
        assert_eq!(
            clips[0].source,
            Source::Title {
                title: title.id.clone()
            }
        );
        assert!(effect.created.contains(&title.id.to_string()));
        assert!(effect.created.contains(&clips[0].id.to_string()));
    }

    #[test]
    fn a_lower_third_defaults_to_four_seconds() {
        let mut project = project();
        apply(
            &TitleLowerThird,
            &mut project,
            serde_json::json!({ "text": "Andy", "at": "0" }),
        )
        .expect("lower third");
        assert_eq!(clips(&project)[0].duration, Time::from_secs(4));
    }

    #[test]
    fn setting_a_field_the_svg_never_mentions_warns() {
        let mut project = project();
        apply(
            &TitleAdd,
            &mut project,
            serde_json::json!({ "template": "lower-third", "field": ["title=Andy"] }),
        )
        .expect("add");
        let id = only_title(&project).id.clone();

        let effect = apply(
            &TitleSetText,
            &mut project,
            serde_json::json!({ "title": id.to_string(), "field": { "subtitel": "degen labs" } }),
        )
        .expect("set-text stores the value and warns");

        let warning = effect
            .warnings
            .first()
            .expect("a field the SVG never references is worth a warning");
        assert_eq!(warning.code, "unused-field");
        assert!(warning.detail.contains("subtitel"), "{}", warning.detail);
        assert!(
            warning.detail.contains("subtitle"),
            "the real placeholders should be listed: {}",
            warning.detail
        );
        let placeholders = effect
            .data
            .as_ref()
            .and_then(|data| data.get("placeholders").cloned())
            .expect("set-text reports the placeholders");
        assert_eq!(placeholders, serde_json::json!(["title", "subtitle"]));

        // The value is kept: it was data the caller meant, just not referenced yet.
        assert_eq!(
            only_title(&project).fields.get("subtitel").map(String::as_str),
            Some("degen labs")
        );
    }

    #[test]
    fn a_field_that_is_referenced_does_not_warn() {
        let mut project = project();
        apply(
            &TitleAdd,
            &mut project,
            serde_json::json!({ "template": "caption-card" }),
        )
        .expect("add");
        let id = only_title(&project).id.clone();
        let effect = apply(
            &TitleSetText,
            &mut project,
            serde_json::json!({ "title": id.to_string(), "field": "title=Chapter one" }),
        )
        .expect("set-text");
        assert!(effect.warnings.is_empty(), "{:?}", effect.warnings);
        assert!(only_title(&project)
            .resolved_svg()
            .contains("Chapter one"));
    }

    #[test]
    fn the_built_in_templates_need_nothing_but_the_document() {
        for name in TEMPLATES {
            let svg = instantiate(template_named(name).expect("template"), [1080, 1920]);
            assert!(svg.starts_with("<svg"), "{name}: {svg}");
            assert_eq!(
                placeholders(&svg),
                vec!["title".to_string(), "subtitle".to_string()],
                "{name} should leave exactly the two text fields live"
            );
            // `href` covers both `xlink:href` and plain `href`; `url(` is how SVG reaches
            // for an external paint server. The only URL allowed is the SVG namespace,
            // which resolves nothing.
            for external in ["<image", "href", "url(", "@font-face", "<use"] {
                assert!(
                    !svg.contains(external),
                    "{name} references {external}, which the renderer cannot resolve: {svg}"
                );
            }
            // Geometry is resolved against the sequence, not left at a 1080p default.
            assert!(svg.contains("width=\"1080\"") && svg.contains("height=\"1920\""), "{svg}");
        }
    }

    #[test]
    fn a_template_sizes_itself_to_the_sequence() {
        let mut project = Project::new(
            "vertical",
            Fps::new(30, 1).expect("30 fps"),
            [1080, 1920],
            48_000,
        );
        apply(
            &TitleAdd,
            &mut project,
            serde_json::json!({ "template": "lower-third", "field": "title=Andy" }),
        )
        .expect("add");
        let title = only_title(&project);
        assert_eq!(title.size, [1080, 1920]);
        assert!(title.svg.contains("viewBox=\"0 0 1080 1920\""), "{}", title.svg);
    }

    #[test]
    fn an_unknown_template_lists_the_ones_that_exist() {
        let mut project = project();
        let err = apply(
            &TitleAdd,
            &mut project,
            serde_json::json!({ "template": "lowerthird" }),
        )
        .expect_err("unknown template");
        assert_eq!(err.exit_code(), crate::error::exit::NO_MATCH);
        assert!(err.to_string().contains("lower-third"), "{err}");
    }

    #[test]
    fn a_title_needs_exactly_one_source() {
        let mut project = project();
        let err = apply(&TitleAdd, &mut project, serde_json::json!({})).expect_err("no source");
        assert!(err.to_string().contains("template"), "{err}");
        let err = apply(
            &TitleAdd,
            &mut project,
            serde_json::json!({ "template": "full-screen", "svg": "<svg></svg>" }),
        )
        .expect_err("two sources");
        assert!(err.to_string().contains("not both"), "{err}");
        assert!(project.titles.is_empty());
    }

    #[test]
    fn an_svg_argument_reads_a_file_and_refuses_something_that_is_not_svg() {
        let vfs = Arc::new(MemVfs::default());
        vfs.write(
            std::path::Path::new("/p/card.svg"),
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"><text>{{who}}</text></svg>",
        )
        .expect("stage the svg");
        vfs.write(std::path::Path::new("/p/notes.txt"), b"just notes")
            .expect("stage the decoy");
        let mut project = project();

        apply_with_vfs(
            vfs.clone(),
            &TitleAdd,
            &mut project,
            serde_json::json!({ "svg": "card.svg", "field": "who=Andy", "name": "card" }),
        )
        .expect("read the file");
        assert!(only_title(&project).resolved_svg().contains("Andy"));

        let err = apply_with_vfs(
            vfs,
            &TitleAdd,
            &mut project,
            serde_json::json!({ "svg": "notes.txt" }),
        )
        .expect_err("not an svg");
        assert_eq!(err.exit_code(), crate::error::exit::BAD_ARGS);
    }

    #[test]
    fn placing_a_title_where_a_clip_already_is_names_the_occupant() {
        let mut project = project();
        apply(
            &TitleLowerThird,
            &mut project,
            serde_json::json!({ "text": "first", "at": "0", "for": "4" }),
        )
        .expect("first title");
        let err = apply(
            &TitleLowerThird,
            &mut project,
            serde_json::json!({ "text": "second", "at": "2", "for": "4" }),
        )
        .expect_err("the range is taken");
        assert_eq!(clips(&project).len(), 1, "{err}");
    }

    #[test]
    fn an_empty_timeline_gets_a_video_track_to_hold_the_title() {
        let mut project = Project::new(
            "promo",
            Fps::new(30, 1).expect("30 fps"),
            [1920, 1080],
            48_000,
        );
        apply(
            &TitleLowerThird,
            &mut project,
            serde_json::json!({ "text": "Andy", "at": "0" }),
        )
        .expect("lower third on an empty timeline");
        let seq = project.active_sequence.clone();
        let sequence = project.sequence(&seq).expect("main");
        assert_eq!(sequence.track_names(), vec!["V1"]);
        assert_eq!(sequence.tracks[0].clips.len(), 1);
    }

    #[test]
    fn a_title_a_clip_still_shows_cannot_be_removed() {
        let mut project = project();
        apply(
            &TitleLowerThird,
            &mut project,
            serde_json::json!({ "text": "Andy", "at": "0", "name": "name-bar" }),
        )
        .expect("lower third");

        let err = apply(
            &TitleRemove,
            &mut project,
            serde_json::json!({ "title": "name-bar" }),
        )
        .expect_err("a clip still shows it");
        assert!(err.to_string().contains("name-bar"), "{err}");
        assert_eq!(project.titles.len(), 1);

        let seq = project.active_sequence.clone();
        project.sequence_mut(&seq).expect("main").tracks[0].clips.clear();
        apply(
            &TitleRemove,
            &mut project,
            serde_json::json!({ "title": "name-bar" }),
        )
        .expect("removable once nothing shows it");
        assert!(project.titles.is_empty());
    }

    #[test]
    fn title_times_snap_to_the_sequence_grid_and_report_it() {
        let mut project = project();
        let effect = apply(
            &TitleLowerThird,
            &mut project,
            serde_json::json!({ "text": "Andy", "at": "2.51", "for": "1.02" }),
        )
        .expect("lower third");
        let fields: Vec<&str> = effect.snapped.iter().map(|snap| snap.field).collect();
        assert!(fields.contains(&"at") && fields.contains(&"for"), "{fields:?}");
        let clip = &clips(&project)[0];
        // 2.51 s is frame 75 at 30 fps and 1.02 s is 31 frames.
        assert_eq!(clip.start, Time::from_frames(75, Fps::new(30, 1).unwrap()));
        assert_eq!(clip.duration, Time::new(31, 30).unwrap());
    }
}
