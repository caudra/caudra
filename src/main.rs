// Guards in this crate own real state: the ephemeral root erases the volatile
// data directory on drop. A `process::exit` anywhere below `main` skips it.
#![deny(clippy::exit)]

mod cli;
mod cmd;
mod print;
mod sdk_mode;
mod setup;
mod update;

use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;

use cli::Cli;

/// How long a final telemetry export may take before caudra stops waiting.
const TELEMETRY_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Every exit runs through this return so command guards drop normally; a
/// `process::exit` deeper in the tree would skip them.
fn main() -> ExitCode {
    color_eyre::install().ok();
    let result = cmd::dispatch(Cli::parse());
    // Detached export tasks die with the process, so drain them once every
    // command has released its resources.
    caudra_otel::shutdown(TELEMETRY_SHUTDOWN_TIMEOUT);
    match result {
        Ok(code) => code,
        Err(e) => {
            print_error(&e);
            ExitCode::FAILURE
        }
    }
}

fn print_error(e: &color_eyre::Report) {
    const RED: &str = "\x1b[31m";
    const BOLD_RED: &str = "\x1b[1;31m";
    const DIM: &str = "\x1b[2m";
    const RESET: &str = "\x1b[0m";

    eprintln!();
    eprintln!("{BOLD_RED}✖ {e}{RESET}");
    let causes: Vec<_> = e.chain().skip(1).collect();
    let last = causes.len().saturating_sub(1);
    for (i, cause) in causes.iter().enumerate() {
        let branch = if i == last { "└─" } else { "├─" };
        eprintln!("{DIM}{branch}{RESET} {RED}{cause}{RESET}");
    }
    eprintln!();
}
