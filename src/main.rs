// Guards in this crate own real state: the ephemeral root erases the volatile
// data directory on drop. A `process::exit` anywhere below `main` skips it.
#![deny(clippy::exit)]

mod cli;
mod cmd;
mod print;
mod progress;
mod sdk_mode;
mod setup;
mod startup;
mod update;

use std::process::ExitCode;
use std::time::Duration;

use caudra_config::FeatureFlags;
use clap::error::ErrorKind;

use cli::Cli;
use startup::Startup;

/// How long a final telemetry export may take before caudra stops waiting.
const TELEMETRY_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the queued log records may take to reach the file before caudra
/// stops waiting.
const LOG_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// A bare word is a mistyped subcommand far more often than a message, so point
/// at the flag that does send one.
const PROMPT_HINT: &str = "tip: to open a session with a message, use `caudra --prompt \"<text>\"`";

/// Every exit runs through this return so command guards drop normally; a
/// `process::exit` deeper in the tree would skip them.
fn main() -> ExitCode {
    color_eyre::install().ok();
    // Read before parsing, so help lists only the experiments this process
    // turned on. A file that cannot be read shows none of them; dispatch then
    // reports why.
    let startup = Startup::load();
    let features = startup
        .as_ref()
        .map_or(FeatureFlags::NONE, |startup| startup.features);
    let cli = match Cli::parse_for(features) {
        Ok(cli) => cli,
        Err(err) => return report_parse_error(&err),
    };
    progress::install();
    let result = cmd::dispatch(cli, startup);
    // Detached export tasks die with the process, so drain them once every
    // command has released its resources.
    caudra_otel::shutdown(TELEMETRY_SHUTDOWN_TIMEOUT);
    caudra_storage::log::flush_blocking(LOG_FLUSH_TIMEOUT);
    match result {
        Ok(code) => code,
        Err(e) => {
            print_error(&e);
            ExitCode::FAILURE
        }
    }
}

fn report_parse_error(err: &clap::Error) -> ExitCode {
    err.print().ok();
    if err.kind() == ErrorKind::InvalidSubcommand {
        eprintln!("{PROMPT_HINT}");
    }
    ExitCode::from(err.exit_code() as u8)
}

fn print_error(e: &color_eyre::Report) {
    const RED: &str = "\x1b[31m";
    const BOLD_RED: &str = "\x1b[1;31m";
    const DIM: &str = "\x1b[2m";
    const RESET: &str = "\x1b[0m";

    let chain: Vec<String> = e.chain().map(ToString::to_string).collect();
    let causes = visible_causes(&chain);

    eprintln!();
    eprintln!("{BOLD_RED}✖ {e}{RESET}");
    let last = causes.len().saturating_sub(1);
    for (i, cause) in causes.iter().enumerate() {
        let branch = if i == last { "└─" } else { "├─" };
        eprintln!("{DIM}{branch}{RESET} {RED}{cause}{RESET}");
    }
    eprintln!();
}

/// Drops causes whose parent already ends with their message, which is what a
/// `#[error("context: {0}")]` variant over a `#[source]` field produces.
fn visible_causes(chain: &[String]) -> Vec<&str> {
    chain
        .windows(2)
        .filter(|pair| !pair[0].ends_with(&pair[1]))
        .map(|pair| pair[1].as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::visible_causes;
    use test_case::test_case;

    const ROOT: &str = "session database is at schema 8";
    const STATE: &str = "state database: session database is at schema 8";
    const PURPOSE: &str = "model purpose storage: state database: session database is at schema 8";
    const CONTEXT: &str = "load model purpose bindings";

    #[test_case(&[CONTEXT, PURPOSE, STATE], &[PURPOSE]; "embedded causes collapse to one line")]
    #[test_case(&[CONTEXT, PURPOSE, STATE, ROOT], &[PURPOSE]; "a whole embedded chain collapses")]
    #[test_case(&[CONTEXT, ROOT], &[ROOT]; "a cause that adds text is kept")]
    #[test_case(&[CONTEXT, CONTEXT], &[]; "a transparent wrapper is dropped")]
    #[test_case(&[CONTEXT], &[]; "a lone error has no causes")]
    fn visible_causes_drops_repeated_text(chain: &[&str], expected: &[&str]) {
        let chain: Vec<String> = chain.iter().map(ToString::to_string).collect();
        assert_eq!(visible_causes(&chain), expected);
    }
}
