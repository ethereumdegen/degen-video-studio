//! The command grammar.
//!
//! Two rules shape it. Every command is available to a machine — `--json` anywhere, one
//! object on stdout, exit codes from [`dvs_core::exit`] — and every *editing* verb is a
//! thin wrapper over one registered op rather than a second implementation. `dvs op <id>`
//! is the general form; `dvs asset import`, `dvs export kdenlive` and the transcript and
//! caption verbs exist because they are what a person types, and they build the same
//! argument object the generic form would.
//!
//! The generic form is why [`OpArgs::rest`] is a raw token list instead of declared clap
//! flags: an op's arguments come from its JSON Schema at runtime, so they cannot be known
//! at compile time. [`crate::commands::op`] validates them against that schema, which is
//! how `--durations 4` becomes an error naming `duration` instead of a silently dropped
//! edit.

use clap::{Args, Parser, Subcommand, ValueEnum};
use dvs_core::error::{Error, Result};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "dvs",
    version,
    about = "degen-video-studio: an agent-native video editor",
    long_about = "Edit video as a JSON document. Every verb is an op from one registry; \
                  add --json to any command for a machine-readable answer.\n\n\
                  Exit codes: 0 ok, 1 op error, 2 bad arguments, 3 selector matched nothing, \
                  4 lint findings, 5 tool missing, 6 budget exceeded."
)]
pub struct Cli {
    /// Project directory; defaults to the nearest project at or above the working directory.
    #[arg(long, global = true, value_name = "DIR")]
    pub project: Option<PathBuf>,

    /// Sequence to act on, by id or name; defaults to the project's active sequence.
    #[arg(long, global = true, value_name = "ID")]
    pub seq: Option<String>,

    /// Emit one JSON object on stdout instead of human text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Report what would happen and write nothing.
    #[arg(long = "dry-run", global = true)]
    pub dry_run: bool,

    /// Suppress human text; exit codes and --json output still work.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a project directory with one empty sequence.
    New(NewArgs),
    /// Import, list and remove media.
    Asset {
        #[command(subcommand)]
        command: AssetCommand,
    },
    /// Run any registered op by id, with its schema's arguments as flags.
    Op(OpArgs),
    /// Print JSON Schemas: op arguments, or the document itself.
    Schema(SchemaArgs),
    /// Render the sequence to a file.
    Render(RenderArgs),
    /// Render one frame to a PNG.
    Frame(FrameArgs),
    /// Render a grid of frames with burned-in timecodes.
    Sheet(SheetArgs),
    /// Measure the timeline and report what is in it.
    Digest(DigestArgs),
    /// Check the timeline against the rules a blind editor breaks.
    Lint(LintArgs),
    /// Compare two media files frame by frame.
    Diff(DiffArgs),
    /// Word-timestamped transcripts, and editing through them.
    Transcript {
        #[command(subcommand)]
        command: TranscriptCommand,
    },
    /// Caption cues: generate, import, export.
    Caption {
        #[command(subcommand)]
        command: CaptionCommand,
    },
    /// Write the sequence out for another editor.
    Export(ExportArgs),
    /// Reverse the newest op still in effect.
    Undo,
    /// Re-apply the newest undone op.
    Redo,
    /// Show the op journal.
    History(HistoryArgs),
    /// Drop regenerable cache and unreferenced media.
    Gc,
    /// Report the toolchain, the project's health, and what is missing.
    Doctor,
    /// Serve the same op registry over MCP on stdio.
    Mcp,
}

#[derive(Debug, Args)]
pub struct NewArgs {
    /// Project name; also the directory name unless --project says otherwise.
    pub name: String,
    /// Frame rate: `30`, `30000/1001`, or the shorthand `29.97`.
    #[arg(long, default_value = "30")]
    pub fps: String,
    /// Frame size as `WIDTHxHEIGHT`.
    #[arg(long, default_value = "1920x1080", value_parser = parse_size)]
    pub size: [u32; 2],
    /// Audio sample rate in Hz.
    #[arg(long = "sample-rate", default_value_t = 48_000)]
    pub sample_rate: u32,
}

#[derive(Debug, Subcommand)]
pub enum AssetCommand {
    /// Probe, hash and copy media into the project.
    Import(AssetImportArgs),
    /// List the project's assets and whether their bytes are present.
    List,
    /// Remove an asset that nothing references.
    Remove(AssetRemoveArgs),
}

#[derive(Debug, Args)]
pub struct AssetImportArgs {
    /// Files to import.
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,
    /// Proxy policy: `auto` builds one for VFR or above-1080p sources.
    #[arg(long, default_value = "auto")]
    pub proxy: String,
}

#[derive(Debug, Args)]
pub struct AssetRemoveArgs {
    /// Asset id, name or file stem.
    pub asset: String,
    /// Remove it even though clips still use it.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct OpArgs {
    /// Op id, e.g. `clip.split`. Omit it with --list.
    pub id: Option<String>,
    /// Print every op with its arguments as JSON and exit.
    #[arg(long)]
    pub list: bool,
    /// Argument object as JSON; flags override its keys.
    #[arg(long = "json-args", value_name = "JSON")]
    pub json_args: Option<String>,
    /// Arguments from the op's schema, as `--key value` (booleans need no value).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_name = "ARGS")]
    pub rest: Vec<String>,
}

#[derive(Debug, Args)]
pub struct SchemaArgs {
    /// One op's argument schema.
    #[arg(long, value_name = "ID")]
    pub op: Option<String>,
    /// The `project.json` document schema.
    #[arg(long = "project-schema")]
    pub project_schema: bool,
}

#[derive(Debug, Args)]
pub struct RenderArgs {
    /// Output file; the container is chosen by its extension.
    pub out: PathBuf,
    /// Timeline range as `start-end`; default is the whole sequence.
    #[arg(long, value_name = "A-B")]
    pub range: Option<String>,
    /// Output scale; `0.5` renders half size.
    #[arg(long, default_value_t = 1.0)]
    pub scale: f64,
    /// `auto`, `x264`, `x265`, `av1`, `nvenc`, `vaapi`, `videotoolbox`, `prores`, or an ffmpeg encoder name.
    #[arg(long, default_value = "auto")]
    pub encoder: String,
    /// CRF or the encoder's equivalent; lower is better.
    #[arg(long, default_value_t = 18)]
    pub quality: u32,
    /// Encoder preset.
    #[arg(long)]
    pub preset: Option<String>,
    /// Decode from proxies: fast, lower quality, wrong for delivery.
    #[arg(long)]
    pub proxy: bool,
    /// Re-encode every segment instead of reusing the cache.
    #[arg(long = "no-cache")]
    pub no_cache: bool,
    /// Also write the digest of what was rendered to this file.
    #[arg(long, value_name = "FILE")]
    pub digest: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct FrameArgs {
    /// Timeline position: seconds, `mm:ss.mmm`, `1m12s` or `1800f`.
    pub at: String,
    /// PNG to write.
    pub out: PathBuf,
    /// Overlay clip and title boxes with their ids.
    #[arg(long)]
    pub annotate: bool,
    /// Output scale.
    #[arg(long, default_value_t = 1.0)]
    pub scale: f64,
}

#[derive(Debug, Args)]
pub struct SheetArgs {
    /// PNG to write.
    pub out: PathBuf,
    /// Sampling interval.
    #[arg(long, default_value = "5s")]
    pub every: String,
    /// Columns in the grid.
    #[arg(long, default_value_t = 6)]
    pub cols: u32,
    /// Width of the whole sheet in pixels; cells are this divided by --cols.
    #[arg(long, default_value_t = 1920)]
    pub width: u32,
}

#[derive(Debug, Args)]
pub struct DigestArgs {
    /// Write the digest here as well as to stdout.
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
    /// Skip the audio mix and loudness pass.
    #[arg(long = "no-audio")]
    pub no_audio: bool,
    /// Sampling interval for the frame analysis.
    #[arg(long, default_value = "1s")]
    pub every: String,
}

#[derive(Debug, Args)]
pub struct LintArgs {
    /// Loudness target the mix is checked against.
    #[arg(long, value_enum, default_value_t = Profile::Youtube)]
    pub profile: Profile,
    /// Document rules only: no frames are rendered.
    #[arg(long = "no-render")]
    pub no_render: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Profile {
    Youtube,
    Podcast,
    Broadcast,
}

impl Profile {
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Youtube => "youtube",
            Profile::Podcast => "podcast",
            Profile::Broadcast => "broadcast",
        }
    }
}

#[derive(Debug, Args)]
pub struct DiffArgs {
    /// Reference file.
    pub a: PathBuf,
    /// File to compare against it.
    pub b: PathBuf,
    /// Sampling interval.
    #[arg(long, default_value = "1s")]
    pub every: String,
    /// Fail (exit 4) when any sampled frame scores below this SSIM.
    #[arg(long)]
    pub threshold: Option<f64>,
}

#[derive(Debug, Subcommand)]
pub enum TranscriptCommand {
    /// Transcribe an asset with whisper.
    Run(TranscriptRunArgs),
    /// Load a word-timestamped transcript produced elsewhere.
    Import(TranscriptImportArgs),
    /// Locate a phrase and report the ranges it covers.
    Find(TranscriptFindArgs),
    /// Remove filler words from clips, closing the gaps.
    CutWords(CutWordsArgs),
    /// Keep only the clip ranges that say these phrases.
    KeepPhrases(KeepPhrasesArgs),
}

#[derive(Debug, Args)]
pub struct TranscriptRunArgs {
    /// Asset id, name or file stem.
    #[arg(long)]
    pub asset: String,
    /// Whisper model, e.g. `base.en`.
    #[arg(long)]
    pub model: Option<String>,
    /// Spoken language; auto-detected when omitted.
    #[arg(long)]
    pub language: Option<String>,
}

#[derive(Debug, Args)]
pub struct TranscriptImportArgs {
    /// Asset the transcript belongs to.
    #[arg(long)]
    pub asset: String,
    /// Word JSON to read.
    pub path: PathBuf,
    /// Spoken language, recorded with the transcript.
    #[arg(long)]
    pub language: Option<String>,
    /// Name of the ASR that produced it, recorded with the transcript.
    #[arg(long)]
    pub model: Option<String>,
}

#[derive(Debug, Args)]
pub struct TranscriptFindArgs {
    /// Phrase to locate.
    pub phrase: String,
    /// Asset to search; defaults to the assets the target clips use.
    #[arg(long)]
    pub asset: Option<String>,
    /// Clip selector to search within.
    #[arg(long)]
    pub target: Option<String>,
}

#[derive(Debug, Args)]
pub struct CutWordsArgs {
    /// Clip selector to edit.
    #[arg(long)]
    pub target: String,
    /// Words to remove; defaults to the usual fillers.
    #[arg(long)]
    pub words: Option<String>,
    /// Leave cuts closer together than this alone.
    #[arg(long = "min-gap")]
    pub min_gap: Option<String>,
    /// Padding kept around each surviving word.
    #[arg(long)]
    pub pad: Option<String>,
}

#[derive(Debug, Args)]
pub struct KeepPhrasesArgs {
    /// Clip selector to edit.
    #[arg(long)]
    pub target: String,
    /// Phrases to keep.
    #[arg(required = true)]
    pub phrases: Vec<String>,
    /// Padding kept around each phrase.
    #[arg(long)]
    pub pad: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum CaptionCommand {
    /// Build cues from a transcript.
    Generate(CaptionGenerateArgs),
    /// Load cues from SRT or VTT.
    Import(CaptionImportArgs),
    /// Write cues as SRT or VTT.
    Export(CaptionExportArgs),
}

#[derive(Debug, Args)]
pub struct CaptionGenerateArgs {
    /// Asset whose transcript to read.
    #[arg(long)]
    pub asset: Option<String>,
    /// Clip selector whose sources to caption.
    #[arg(long)]
    pub target: Option<String>,
    /// Caption track to write to; created when missing.
    #[arg(long)]
    pub track: Option<String>,
    /// Caption style id.
    #[arg(long)]
    pub style: Option<String>,
    /// Reading-rate ceiling in characters per second.
    #[arg(long = "max-cps")]
    pub max_cps: Option<f64>,
}

#[derive(Debug, Args)]
pub struct CaptionImportArgs {
    /// SRT or VTT file.
    pub path: PathBuf,
    /// Caption track to write to.
    #[arg(long)]
    pub track: Option<String>,
    /// `srt` or `vtt`; inferred from the extension when omitted.
    #[arg(long)]
    pub format: Option<String>,
}

#[derive(Debug, Args)]
pub struct CaptionExportArgs {
    /// File to write.
    pub out: PathBuf,
    /// `srt` or `vtt`; inferred from the extension when omitted.
    #[arg(long)]
    pub format: Option<String>,
    /// Caption track to export; default is every caption track merged.
    #[arg(long)]
    pub track: Option<String>,
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Interchange format.
    #[arg(value_enum)]
    pub format: ExportFormat,
    /// File to write.
    pub out: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ExportFormat {
    /// MLT XML with the `kdenlive:*` document keys.
    Kdenlive,
    /// Plain MLT XML, for `melt` and other MLT hosts.
    Mlt,
    /// FCPXML 1.11, for Final Cut and Resolve.
    Fcpxml,
    /// OpenTimelineIO JSON.
    Otio,
    /// CMX3600 edit decision list.
    Edl,
    /// Subtitles from the caption tracks.
    Srt,
}

impl ExportFormat {
    /// The op that writes it. Export is one op per format rather than one op with a
    /// format switch, because the writers share no arguments beyond the output path.
    pub fn op_id(self) -> &'static str {
        match self {
            ExportFormat::Kdenlive => "export.kdenlive",
            ExportFormat::Mlt => "export.mlt",
            ExportFormat::Fcpxml => "export.fcpxml",
            ExportFormat::Otio => "export.otio",
            ExportFormat::Edl => "export.edl",
            ExportFormat::Srt => "export.srt",
        }
    }
}

#[derive(Debug, Args)]
pub struct HistoryArgs {
    /// How many entries to show, newest last.
    #[arg(short = 'n', long = "limit", default_value_t = 20)]
    pub limit: usize,
}

/// `1920x1080`. Rejected at parse time so a typo cannot reach the document as a zero
/// dimension, which every later stage would have to defend against.
fn parse_size(text: &str) -> Result<[u32; 2]> {
    let (width, height) = text
        .split_once(['x', 'X', '*'])
        .ok_or_else(|| Error::bad_args(format!("size '{text}' must look like 1920x1080")))?;
    let parse = |value: &str, what: &str| -> Result<u32> {
        value
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| Error::bad_args(format!("{what} '{value}' in size '{text}' is not a positive integer")))
    };
    Ok([parse(width, "width")?, parse(height, "height")?])
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_grammar_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn sizes_parse_and_reject_nonsense() {
        assert_eq!(parse_size("1920x1080").unwrap(), [1920, 1080]);
        assert_eq!(parse_size("640X480").unwrap(), [640, 480]);
        assert!(parse_size("1920").is_err());
        assert!(parse_size("0x1080").is_err());
        assert!(parse_size("axb").is_err());
    }

    /// Global flags must survive in front of a subcommand that swallows its trailing
    /// arguments, or `--json` would be unusable with the generic op form.
    #[test]
    fn op_keeps_unknown_flags_for_the_schema_parser() {
        let cli = Cli::try_parse_from(["dvs", "--json", "op", "clip.split", "--at", "42.5"])
            .expect("unknown op flags are data, not usage errors");
        assert!(cli.json);
        match cli.command {
            Command::Op(args) => {
                assert_eq!(args.id.as_deref(), Some("clip.split"));
                assert_eq!(args.rest, vec!["--at".to_string(), "42.5".to_string()]);
            }
            other => panic!("parsed {other:?} instead of an op"),
        }
    }
}
