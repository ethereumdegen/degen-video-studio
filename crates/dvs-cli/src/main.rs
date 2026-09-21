//! `dvs`: the command line for degen-video-studio.
//!
//! The binary is deliberately thin. Argument parsing is [`cli`], the answer goes through
//! [`output`], and the work is one function per verb in [`commands`] — most of which do
//! nothing but build a JSON argument object and hand it to an op from the shared
//! registry. That is what keeps `dvs`, the MCP server and the GUI from growing three
//! different ideas of what "split a clip" means.
//!
//! The exit code is the contract: 0 ok, 1 op error, 2 bad arguments, 3 selector matched
//! nothing, 4 lint findings, 5 tool missing, 6 budget exceeded. Every failure path in
//! this file ends in [`dvs_core::Error::exit_code`], including clap's own usage errors,
//! which map to 2 because that is what a bad argument is.

mod cli;
mod commands;
mod output;

use clap::Parser;
use cli::Cli;
use dvs_core::exit;
use output::Out;
use std::process::ExitCode;

fn main() -> ExitCode {
    let parsed = match Cli::try_parse() {
        Ok(parsed) => parsed,
        Err(error) => {
            // `--help` and `--version` are successful requests for output, not failures;
            // clap distinguishes them from usage errors by kind.
            let usage = !matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp
                    | clap::error::ErrorKind::DisplayVersion
                    | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            );
            let _ = error.print();
            return code(if usage { exit::BAD_ARGS } else { exit::OK });
        }
    };

    let out = Out::new(parsed.json, parsed.quiet);
    match commands::dispatch(parsed) {
        Ok(()) => code(exit::OK),
        Err(error) => {
            out.fail(&error);
            code(error.exit_code())
        }
    }
}

/// Exit codes are small and non-negative by construction, so the conversion cannot lose
/// anything; going through `u8` is only about `ExitCode`'s type.
fn code(value: i32) -> ExitCode {
    ExitCode::from(value.clamp(0, 255) as u8)
}
