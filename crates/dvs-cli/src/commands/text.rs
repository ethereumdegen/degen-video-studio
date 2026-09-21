//! `dvs transcript` and `dvs caption`: editing through words rather than through time.
//!
//! Every verb here is sugar over a registered op (`transcript.*`, `caption.*`), so the
//! ripple rules, the padding defaults and the reading-rate limits live in one place and
//! the CLI only decides which keys to fill. The argument objects are built by hand rather
//! than by the generic scanner because these are the verbs a person types most often, and
//! `--words um,uh` should not require knowing the op id.

use crate::cli::{
    CaptionCommand, CaptionExportArgs, CaptionGenerateArgs, CaptionImportArgs, CutWordsArgs,
    KeepPhrasesArgs, TranscriptCommand, TranscriptFindArgs, TranscriptImportArgs,
    TranscriptRunArgs,
};
use crate::commands::Ctx;
use dvs_core::error::Result;
use serde_json::{json, Map, Value};

pub fn transcript(ctx: &Ctx, command: TranscriptCommand) -> Result<()> {
    match command {
        TranscriptCommand::Run(args) => run(ctx, args),
        TranscriptCommand::Import(args) => import(ctx, args),
        TranscriptCommand::Find(args) => find(ctx, args),
        TranscriptCommand::CutWords(args) => cut_words(ctx, args),
        TranscriptCommand::KeepPhrases(args) => keep_phrases(ctx, args),
    }
}

pub fn caption(ctx: &Ctx, command: CaptionCommand) -> Result<()> {
    match command {
        CaptionCommand::Generate(args) => caption_generate(ctx, args),
        CaptionCommand::Import(args) => caption_import(ctx, args),
        CaptionCommand::Export(args) => caption_export(ctx, args),
    }
}

fn run(ctx: &Ctx, args: TranscriptRunArgs) -> Result<()> {
    let mut object = Map::new();
    object.insert("asset".into(), json!(args.asset));
    insert_opt(&mut object, "model", args.model);
    insert_opt(&mut object, "language", args.language);
    ctx.apply("transcript.run", Value::Object(object)).map(|_| ())
}

fn import(ctx: &Ctx, args: TranscriptImportArgs) -> Result<()> {
    let mut object = Map::new();
    object.insert("asset".into(), json!(args.asset));
    object.insert("path".into(), json!(args.path.display().to_string()));
    insert_opt(&mut object, "language", args.language);
    insert_opt(&mut object, "model", args.model);
    ctx.apply("transcript.import", Value::Object(object))
        .map(|_| ())
}

fn find(ctx: &Ctx, args: TranscriptFindArgs) -> Result<()> {
    let mut object = Map::new();
    object.insert("phrase".into(), json!(args.phrase));
    insert_opt(&mut object, "asset", args.asset);
    insert_opt(&mut object, "target", args.target);
    ctx.apply("transcript.find", Value::Object(object)).map(|_| ())
}

fn cut_words(ctx: &Ctx, args: CutWordsArgs) -> Result<()> {
    let mut object = Map::new();
    object.insert("target".into(), json!(args.target));
    insert_opt(&mut object, "words", args.words);
    insert_opt(&mut object, "min-gap", args.min_gap);
    insert_opt(&mut object, "pad", args.pad);
    ctx.apply("transcript.cut-words", Value::Object(object))
        .map(|_| ())
}

fn keep_phrases(ctx: &Ctx, args: KeepPhrasesArgs) -> Result<()> {
    let mut object = Map::new();
    object.insert("target".into(), json!(args.target));
    object.insert("phrases".into(), json!(args.phrases));
    insert_opt(&mut object, "pad", args.pad);
    ctx.apply("transcript.keep-phrases", Value::Object(object))
        .map(|_| ())
}

fn caption_generate(ctx: &Ctx, args: CaptionGenerateArgs) -> Result<()> {
    let mut object = Map::new();
    insert_opt(&mut object, "asset", args.asset);
    insert_opt(&mut object, "target", args.target);
    insert_opt(&mut object, "track", args.track);
    insert_opt(&mut object, "style", args.style);
    if let Some(max_cps) = args.max_cps {
        object.insert("max-cps".into(), json!(max_cps));
    }
    ctx.apply("caption.generate", Value::Object(object))
        .map(|_| ())
}

fn caption_import(ctx: &Ctx, args: CaptionImportArgs) -> Result<()> {
    let mut object = Map::new();
    object.insert("path".into(), json!(args.path.display().to_string()));
    insert_opt(&mut object, "track", args.track);
    insert_opt(&mut object, "format", args.format);
    ctx.apply("caption.import", Value::Object(object)).map(|_| ())
}

fn caption_export(ctx: &Ctx, args: CaptionExportArgs) -> Result<()> {
    let mut object = Map::new();
    object.insert("out".into(), json!(args.out.display().to_string()));
    insert_opt(&mut object, "format", args.format);
    insert_opt(&mut object, "track", args.track);
    ctx.apply("caption.export", Value::Object(object)).map(|_| ())
}

/// Only send keys the caller actually gave. An explicit `null` would override the op's
/// default with nothing, which is not what omitting a flag means.
fn insert_opt(object: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        object.insert(key.to_string(), Value::String(value));
    }
}
