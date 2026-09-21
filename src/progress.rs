//! Console progress for the storage operations that would otherwise run silent.
//!
//! A migration rewrites every payload and a prune returns freelist pages one at
//! a time; on a large database either runs for minutes. Without this the
//! terminal sits blank and the work is indistinguishable from a hang, which is
//! exactly when someone reaches for the interrupt that would leave the database
//! part-migrated.

use std::io::IsTerminal;
use std::io::{stderr, stdout};
use std::sync::Mutex;
use std::time::Duration;

use caudra_storage::sessions::progress::{MIGRATION, MigrationEvent, PRUNE, PruneEvent};
use indicatif::{HumanCount, HumanDuration, ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::cli::Cli;

/// Wide enough for `rewriting subagent_history_items`, the longest label, so the
/// bar does not jump sideways when that phase starts.
const BAR_TEMPLATE: &str = "  {msg:<32} [{bar:20}] {pos}/{len} {elapsed_precise}";
const BAR_CHARS: &str = "=> ";
const BAR_REFRESH: Duration = Duration::from_millis(100);
const BACKUP_MESSAGE: &str = "backing up (pages)";
const RECLAIM_MESSAGE: &str = "reclaiming (pages)";
const SANDBOX_TEMPLATE: &str = "  {spinner} {msg} [{elapsed_precise}]";
const SANDBOX_TICKS: &str = "|/-\\ ";

pub struct SandboxProgress(ProgressBar);

impl SandboxProgress {
    pub fn start(cli: &Cli) -> Option<Self> {
        let message = sandbox_message(cli, stdout().is_terminal(), stderr().is_terminal())?;
        let progress = Self::new(message, ProgressDrawTarget::stderr());
        progress.0.enable_steady_tick(BAR_REFRESH);
        Some(progress)
    }

    fn new(message: String, target: ProgressDrawTarget) -> Self {
        let bar = ProgressBar::new_spinner()
            .with_style(
                ProgressStyle::with_template(SANDBOX_TEMPLATE)
                    .expect("static template")
                    .tick_chars(SANDBOX_TICKS),
            )
            .with_message(message);
        bar.set_draw_target(target);
        bar.tick();
        Self(bar)
    }
}

impl Drop for SandboxProgress {
    fn drop(&mut self) {
        self.0.finish_and_clear();
    }
}

fn sandbox_message(cli: &Cli, stdout_terminal: bool, stderr_terminal: bool) -> Option<String> {
    if cli.print || cli.command.is_some() || !stdout_terminal || !stderr_terminal {
        return None;
    }
    let name = cli.workcell.sandbox.as_deref()?;
    Some(if cli.workcell.sandbox_resume {
        format!("Resuming sandbox '{name}' and connecting to Workcell")
    } else {
        format!("Connecting to sandbox '{name}'")
    })
}

/// Installs the console reporters, unless stderr is redirected. A progress bar
/// drawn into a pipe is noise in a log, and headless callers own their output.
pub fn install() {
    if !stderr().is_terminal() {
        return;
    }
    let migration: Mutex<Option<ProgressBar>> = Mutex::new(None);
    MIGRATION.set(move |event| {
        if let Ok(mut bar) = migration.lock() {
            render_migration(&mut bar, event);
        }
    });
    let prune: Mutex<Option<ProgressBar>> = Mutex::new(None);
    PRUNE.set(move |event| {
        if let Ok(mut bar) = prune.lock() {
            render_prune(&mut bar, event);
        }
    });
}

fn render_migration(bar: &mut Option<ProgressBar>, event: MigrationEvent) {
    match event {
        MigrationEvent::Started { from, to } => {
            eprintln!("Upgrading session database from schema {from} to {to}.");
            eprintln!("Leave caudra running until it finishes; the original is kept alongside.");
            *bar = Some(new_bar());
        }
        MigrationEvent::Backup { done, total } => {
            advance(bar.as_ref(), BACKUP_MESSAGE, done, total);
        }
        MigrationEvent::Step { from, to } => {
            if let Some(bar) = bar.as_ref() {
                bar.set_message(format!("schema {from} to {to}"));
            }
        }
        MigrationEvent::Rewrite { table, done, total } => {
            advance(bar.as_ref(), &format!("rewriting {table}"), done, total);
        }
        MigrationEvent::Reclaiming { done, total } => {
            advance(bar.as_ref(), RECLAIM_MESSAGE, done, total);
        }
        MigrationEvent::Finished { from, to } => {
            if let Some(bar) = bar.take() {
                bar.finish_and_clear();
                let elapsed = HumanDuration(bar.elapsed());
                eprintln!("Session database upgraded from schema {from} to {to} in {elapsed}.");
            }
        }
    }
}

/// A prune reclaims pages one at a time and only the reclaim has a total, so
/// the earlier stages are plain lines and the bar belongs to the reclaim alone.
/// A reclaim of nothing prints nothing, which keeps a dry run and an
/// already-compact database quiet.
fn render_prune(bar: &mut Option<ProgressBar>, event: PruneEvent) {
    match event {
        PruneEvent::Phase { label } => eprintln!("  {label}"),
        PruneEvent::Reclaiming { done, total } => {
            let bar = bar.get_or_insert_with(new_bar);
            advance(Some(bar), RECLAIM_MESSAGE, done, total);
        }
        PruneEvent::Reclaimed { pages } => {
            if let Some(bar) = bar.take() {
                bar.finish_and_clear();
                let elapsed = HumanDuration(bar.elapsed());
                let pages = HumanCount(pages);
                eprintln!("  reclaimed {pages} pages in {elapsed}");
            }
        }
    }
}

fn new_bar() -> ProgressBar {
    let bar = ProgressBar::new(0).with_style(
        ProgressStyle::with_template(BAR_TEMPLATE)
            .expect("static template")
            .progress_chars(BAR_CHARS),
    );
    bar.set_draw_target(ProgressDrawTarget::stderr());
    bar.enable_steady_tick(BAR_REFRESH);
    bar
}

fn advance(bar: Option<&ProgressBar>, message: &str, done: u64, total: u64) {
    let Some(bar) = bar else {
        return;
    };
    if bar.length() != Some(total) {
        bar.set_length(total);
    }
    if bar.message() != message {
        bar.set_message(message.to_owned());
    }
    bar.set_position(done);
}

#[cfg(test)]
mod tests {
    use super::{SandboxProgress, sandbox_message};
    use crate::cli::Cli;
    use clap::Parser;
    use indicatif::ProgressDrawTarget;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use test_case::test_case;

    const SANDBOX_NAME: &str = "saved-dev";
    const CONNECT_MESSAGE: &str = "Connecting to sandbox 'saved-dev'";
    const RESUME_MESSAGE: &str = "Resuming sandbox 'saved-dev' and connecting to Workcell";
    const STARTUP_ERROR: &str = "startup failed";

    #[test_case(false, CONNECT_MESSAGE; "connect")]
    #[test_case(true, RESUME_MESSAGE; "resume")]
    fn sandbox_label(resume: bool, expected: &str) {
        let mut cli = Cli::parse_from(["caudra", "--sandbox", SANDBOX_NAME]);
        cli.workcell.sandbox_resume = resume;
        assert_eq!(sandbox_message(&cli, true, true).as_deref(), Some(expected));
    }

    #[test_case(&["caudra"], true, true, false; "no_sandbox")]
    #[test_case(&["caudra", "--sandbox", SANDBOX_NAME], true, true, true; "interactive")]
    #[test_case(&["caudra", "--sandbox", SANDBOX_NAME], false, true, false; "stdout_redirected")]
    #[test_case(&["caudra", "--sandbox", SANDBOX_NAME], true, false, false; "stderr_redirected")]
    #[test_case(&["caudra", "--sandbox", SANDBOX_NAME, "--print"], true, true, false; "print")]
    #[test_case(&["caudra", "--sandbox", SANDBOX_NAME, "--print", "--input-format", "stream-json"], true, true, false; "sdk")]
    #[test_case(&["caudra", "acp", "--sandbox", SANDBOX_NAME], true, true, false; "acp")]
    fn sandbox_enablement(args: &[&str], stdout: bool, stderr: bool, enabled: bool) {
        let cli = Cli::parse_from(args);
        assert_eq!(sandbox_message(&cli, stdout, stderr).is_some(), enabled);
    }

    #[test_case(&["caudra", "--continue"]; "continue_session")]
    #[test_case(&["caudra", "--session", "saved-session"]; "session")]
    fn sandbox_label_uses_recovered_selector(args: &[&str]) {
        let mut cli = Cli::parse_from(args);
        assert!(sandbox_message(&cli, true, true).is_none());
        cli.workcell.sandbox = Some(SANDBOX_NAME.to_owned());
        assert_eq!(
            sandbox_message(&cli, true, true).as_deref(),
            Some(CONNECT_MESSAGE)
        );
    }

    #[test_case(false; "success")]
    #[test_case(true; "early_error")]
    fn sandbox_cleanup(fail: bool) {
        let progress =
            SandboxProgress::new(CONNECT_MESSAGE.to_owned(), ProgressDrawTarget::hidden());
        let bar = progress.0.clone();
        assert!(!bar.is_finished());
        let result = (move || {
            let _progress = progress;
            if fail {
                Err(STARTUP_ERROR)?;
            }
            Ok::<_, &str>(())
        })();
        assert_eq!(result, if fail { Err(STARTUP_ERROR) } else { Ok(()) });
        assert!(bar.is_finished());
    }

    #[test]
    fn sandbox_cleanup_on_unwind() {
        let progress =
            SandboxProgress::new(CONNECT_MESSAGE.to_owned(), ProgressDrawTarget::hidden());
        let bar = progress.0.clone();
        let result = catch_unwind(AssertUnwindSafe(move || {
            let _progress = progress;
            panic!("{STARTUP_ERROR}");
        }));
        assert!(result.is_err());
        assert!(bar.is_finished());
    }
}
