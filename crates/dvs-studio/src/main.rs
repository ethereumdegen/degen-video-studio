//! `dvs-studio` — watch a project being edited.
//!
//! Two modes, and the second is not an afterthought: `--describe` prints everything the
//! window would show as text, so the studio is usable from a terminal, over ssh, by a
//! screen reader, and in CI.

use clap::Parser;
use dvs_studio::StudioOptions;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "dvs-studio",
    about = "Watch a degen-video-studio project being edited, by you or by an agent",
    version
)]
struct Args {
    /// Project directory; defaults to the nearest project at or above the working directory.
    #[arg(long, value_name = "DIR")]
    project: Option<PathBuf>,
    /// Sequence to open, by id or name; defaults to the project's active sequence.
    #[arg(long, value_name = "ID")]
    seq: Option<String>,
    /// Viewport render scale. Below 1.0 trades resolution for a responsive scrub.
    #[arg(long, default_value_t = 0.5)]
    scale: f64,
    /// Decode originals instead of proxies. Sharper, slower.
    #[arg(long)]
    no_proxy: bool,
    /// Print the timeline, the activity and the findings as text, and exit.
    #[arg(long)]
    describe: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let options = StudioOptions {
        root: args
            .project
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
        sequence: args.seq,
        scale: args.scale.clamp(0.05, 2.0),
        use_proxy: !args.no_proxy,
        describe: args.describe,
    };

    let outcome = if options.describe {
        dvs_studio::describe(&options).map(|text| print!("{text}"))
    } else {
        dvs_studio::run(options)
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(error.exit_code() as u8)
        }
    }
}
