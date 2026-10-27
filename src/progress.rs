//! Console progress for the storage operations that would otherwise run silent.
//!
//! A migration rewrites every payload and a prune returns freelist pages one at
//! a time; on a large database either runs for minutes. Without this the
//! terminal sits blank and the work is indistinguishable from a hang, which is
//! exactly when someone reaches for the interrupt that would leave the database
//! part-migrated.

use std::cell::Cell;
use std::io::{IsTerminal, Write};
use std::io::{stderr, stdout};
use std::sync::Mutex;
use std::time::Duration;

use caudra_storage::sessions::SessionOpenEvent;
use caudra_storage::sessions::progress::{MIGRATION, MigrationEvent, PRUNE, PruneEvent};
use indicatif::{HumanCount, HumanDuration, ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::cli::Cli;

/// Wide enough for `rewriting subagent_history_items`, the longest label, so the
/// bar does not jump sideways when that phase starts. The spinner keeps turning
/// through a stage with nothing to count, so a long one never looks frozen.
const MEASURED_TEMPLATE: &str = "  {spinner} {msg:<32} [{bar:20}] {pos}/{len} {elapsed_precise}";
const UNMEASURED_TEMPLATE: &str = "  {spinner} {msg:<32} {elapsed_precise}";
pub(crate) const BAR_CHARS: &str = "=> ";
pub(crate) const BAR_REFRESH: Duration = Duration::from_millis(100);
const BACKUP_MESSAGE: &str = "backing up (pages)";
const RECLAIM_MESSAGE: &str = "reclaiming (pages)";
const SPINNER_TEMPLATE: &str = "  {spinner} {msg} [{elapsed_precise}]";
const RESUME_MESSAGE: &str = "Loading session";
const SPINNER_TICKS: &str = "|/-\\ ";
const DATABASE_WAIT_MESSAGE: &str = "Session database busy; retrying";
const DATABASE_RECOVERED_MESSAGE: &str = "Session database available; continuing load";

/// One line that keeps turning while startup works on something slow, so the
/// wait before the full-screen UI never looks like a hang.
pub struct ConsoleSpinner(ProgressBar);

impl ConsoleSpinner {
    pub fn sandbox(cli: &Cli) -> Option<Self> {
        let message = sandbox_message(cli, stdout().is_terminal(), stderr().is_terminal())?;
        Self::start_named(message)
    }

    pub fn start_named(message: String) -> Option<Self> {
        if !stderr().is_terminal() {
            return None;
        }
        let progress = Self::new(message, ProgressDrawTarget::stderr());
        progress.0.enable_steady_tick(BAR_REFRESH);
        Some(progress)
    }

    fn new(message: String, target: ProgressDrawTarget) -> Self {
        let bar = ProgressBar::new_spinner()
            .with_style(
                ProgressStyle::with_template(SPINNER_TEMPLATE)
                    .expect("static template")
                    .tick_chars(SPINNER_TICKS),
            )
            .with_message(message);
        bar.set_draw_target(target);
        bar.tick();
        Self(bar)
    }
}

impl Drop for ConsoleSpinner {
    fn drop(&mut self) {
        self.0.finish_and_clear();
    }
}

pub struct SessionLoadProgress {
    spinner: Option<ConsoleSpinner>,
    waiting: Cell<bool>,
}

impl SessionLoadProgress {
    pub fn new() -> Self {
        Self {
            spinner: ConsoleSpinner::start_named(RESUME_MESSAGE.to_owned()),
            waiting: Cell::new(false),
        }
    }

    pub fn report(&self, event: SessionOpenEvent) {
        if let Some(line) = self.update(event) {
            let _ = writeln!(stderr().lock(), "{line}");
        }
    }

    fn update(&self, event: SessionOpenEvent) -> Option<String> {
        match event {
            SessionOpenEvent::Waiting {
                operation,
                waited,
                budget,
            } => {
                let already_waiting = self.waiting.replace(true);
                if let Some(spinner) = &self.spinner {
                    spinner.0.set_message(format!(
                        "{DATABASE_WAIT_MESSAGE} ({operation}, {:.1}s / {}s)",
                        waited.as_secs_f64(),
                        budget.as_secs()
                    ));
                } else if !already_waiting {
                    return Some(format!(
                        "{DATABASE_WAIT_MESSAGE} ({operation}; up to {}s)",
                        budget.as_secs()
                    ));
                }
            }
            SessionOpenEvent::Recovered | SessionOpenEvent::Finished => {
                let was_waiting = self.waiting.replace(false);
                if let Some(spinner) = &self.spinner {
                    spinner.0.set_message(RESUME_MESSAGE);
                } else if was_waiting && matches!(event, SessionOpenEvent::Recovered) {
                    return Some(DATABASE_RECOVERED_MESSAGE.to_owned());
                }
            }
        }
        None
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

pub fn uninstall() {
    MIGRATION.clear();
    PRUNE.clear();
}

fn render_migration(bar: &mut Option<ProgressBar>, event: MigrationEvent) {
    match event {
        MigrationEvent::Started { from, to } => {
            eprintln!("Upgrading session database from schema {from} to {to}.");
            eprintln!("Leave caudra running until it finishes; the original is kept alongside.");
            *bar = Some(new_bar(ProgressDrawTarget::stderr()));
        }
        MigrationEvent::Backup { done, total } => {
            advance(bar.as_ref(), BACKUP_MESSAGE, done, total);
        }
        MigrationEvent::Step { from, to } => {
            enter_phase(bar.as_ref(), format!("schema {from} to {to}"));
        }
        MigrationEvent::Phase { label } => enter_phase(bar.as_ref(), label.to_owned()),
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
            let bar = bar.get_or_insert_with(|| new_bar(ProgressDrawTarget::stderr()));
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

fn style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template)
        .expect("static template")
        .progress_chars(BAR_CHARS)
        .tick_chars(SPINNER_TICKS)
}

fn new_bar(target: ProgressDrawTarget) -> ProgressBar {
    let bar = ProgressBar::with_draw_target(None, target).with_style(style(UNMEASURED_TEMPLATE));
    bar.enable_steady_tick(BAR_REFRESH);
    bar
}

/// A stage without a total drops the bar, so the previous stage's full one
/// does not sit on screen claiming work that is already over.
fn enter_phase(bar: Option<&ProgressBar>, message: String) {
    let Some(bar) = bar else {
        return;
    };
    bar.unset_length();
    bar.set_position(0);
    bar.set_style(style(UNMEASURED_TEMPLATE));
    bar.set_message(message);
}

fn advance(bar: Option<&ProgressBar>, message: &str, done: u64, total: u64) {
    let Some(bar) = bar else {
        return;
    };
    match bar.length() {
        Some(length) if length == total => {}
        Some(_) => bar.set_length(total),
        None => {
            bar.set_length(total);
            bar.set_style(style(MEASURED_TEMPLATE));
        }
    }
    if bar.message() != message {
        bar.set_message(message.to_owned());
    }
    bar.set_position(done);
}

#[cfg(test)]
mod tests {
    use super::{
        ConsoleSpinner, DATABASE_RECOVERED_MESSAGE, DATABASE_WAIT_MESSAGE, SessionLoadProgress,
        advance, enter_phase, new_bar, sandbox_message, uninstall,
    };
    use crate::cli::Cli;
    use caudra_config::StorageConfig;
    use caudra_storage::StateDir;
    use caudra_storage::sessions::progress::{MIGRATION, PRUNE, PruneEvent};
    use caudra_storage::sessions::sweep::prune;
    use caudra_storage::sessions::{SessionDatabase, SessionOpenEvent};
    use clap::Parser;
    use indicatif::ProgressDrawTarget;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::{cell::Cell, env, time::Duration};
    use test_case::test_case;

    const SANDBOX_NAME: &str = "saved-dev";
    const CONNECT_MESSAGE: &str = "Connecting to sandbox 'saved-dev'";
    const RESUME_MESSAGE: &str = "Resuming sandbox 'saved-dev' and connecting to Workcell";
    const STARTUP_ERROR: &str = "startup failed";
    const PROGRESS_CHILD: &str = "CAUDRA_TEST_PROGRESS_CHILD";
    const PROGRESS_TEST: &str = "progress::tests::uninstall_detaches_storage_progress";
    const CHECKPOINT_PHASE: &str = "checkpointing the WAL";
    const MIGRATION_DETACHED: &str = "uninstall must release the migration sink";
    const PRUNE_DETACHED: &str = "uninstall must release the prune sink";
    const PRUNE_QUIET: &str = "pruning after uninstall must not report console progress";
    const MEASURED_STAGE: &str = "rewriting background_transcripts";
    const UNMEASURED_STAGE: &str = "committing";
    const PHASE_DROPS_BAR: &str = "a stage without a total must not keep the previous bar";

    #[test_case(None; "fresh")]
    #[test_case(Some((7, 7)); "after_a_full_bar")]
    #[test_case(Some((3, 9)); "after_a_partial_bar")]
    fn stages_switch_between_bar_and_spinner(earlier: Option<(u64, u64)>) {
        let bar = new_bar(ProgressDrawTarget::hidden());
        if let Some((done, total)) = earlier {
            advance(Some(&bar), MEASURED_STAGE, done, total);
        }

        enter_phase(Some(&bar), UNMEASURED_STAGE.to_owned());
        assert_eq!(bar.length(), None, "{PHASE_DROPS_BAR}");
        assert_eq!(bar.position(), 0, "{PHASE_DROPS_BAR}");
        assert_eq!(bar.message(), UNMEASURED_STAGE);

        advance(Some(&bar), MEASURED_STAGE, 1, 2);
        assert_eq!(bar.length(), Some(2));
        assert_eq!(bar.position(), 1);
        assert_eq!(bar.message(), MEASURED_STAGE);
    }

    #[test]
    fn uninstall_detaches_storage_progress() {
        if env::var_os(PROGRESS_CHILD).is_none() {
            let output = Command::new(env::current_exe().unwrap())
                .args(["--exact", PROGRESS_TEST, "--nocapture"])
                .env(PROGRESS_CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let migration = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&migration);
        MIGRATION.set(move |event| seen.lock().unwrap().push(event));
        let phases = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&phases);
        PRUNE.set(move |event| seen.lock().unwrap().push(event));

        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let mut database = SessionDatabase::open(&storage).unwrap();
        let budget = StorageConfig::default().snapshots.store_budget();
        let report = prune(&mut database, &storage, budget, false).unwrap();
        assert!(report.checkpoint.is_some());
        assert!(phases.lock().unwrap().contains(&PruneEvent::Phase {
            label: CHECKPOINT_PHASE,
        }));

        for _ in 0..2 {
            uninstall();
            assert_eq!(Arc::strong_count(&migration), 1, "{MIGRATION_DETACHED}");
            assert_eq!(Arc::strong_count(&phases), 1, "{PRUNE_DETACHED}");
            phases.lock().unwrap().clear();
            let report = prune(&mut database, &storage, budget, false).unwrap();
            assert!(report.checkpoint.is_some());
            assert!(phases.lock().unwrap().is_empty(), "{PRUNE_QUIET}");
        }
    }

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

    fn database_wait() -> SessionOpenEvent {
        SessionOpenEvent::Waiting {
            operation: UNMEASURED_STAGE,
            waited: Duration::from_secs(6),
            budget: Duration::from_secs(30),
        }
    }

    #[test_case(SessionOpenEvent::Recovered; "recovered")]
    #[test_case(SessionOpenEvent::Finished; "failed")]
    fn session_spinner_restores_loading_phase(event: SessionOpenEvent) {
        let progress = SessionLoadProgress {
            spinner: Some(ConsoleSpinner::new(
                super::RESUME_MESSAGE.to_owned(),
                ProgressDrawTarget::hidden(),
            )),
            waiting: Cell::new(false),
        };
        let bar = progress.spinner.as_ref().unwrap().0.clone();
        assert!(progress.update(database_wait()).is_none());
        assert_eq!(
            bar.message(),
            format!("{DATABASE_WAIT_MESSAGE} ({UNMEASURED_STAGE}, 6.0s / 30s)")
        );
        assert!(progress.update(event).is_none());
        assert_eq!(bar.message(), super::RESUME_MESSAGE);
        drop(progress);
        assert!(bar.is_finished());
    }

    #[test]
    fn redirected_session_progress_is_concise_and_scoped() {
        let progress = SessionLoadProgress {
            spinner: None,
            waiting: Cell::new(false),
        };
        let other = SessionLoadProgress {
            spinner: None,
            waiting: Cell::new(false),
        };
        assert_eq!(
            progress.update(database_wait()).unwrap(),
            format!("{DATABASE_WAIT_MESSAGE} ({UNMEASURED_STAGE}; up to 30s)")
        );
        assert!(progress.update(database_wait()).is_none());
        assert!(other.update(SessionOpenEvent::Recovered).is_none());
        assert_eq!(
            progress.update(SessionOpenEvent::Recovered).as_deref(),
            Some(DATABASE_RECOVERED_MESSAGE)
        );
        assert!(progress.update(SessionOpenEvent::Finished).is_none());
        assert!(progress.update(database_wait()).is_some());
        assert!(progress.update(SessionOpenEvent::Finished).is_none());
    }

    #[test_case(false; "success")]
    #[test_case(true; "early_error")]
    fn sandbox_cleanup(fail: bool) {
        let progress =
            ConsoleSpinner::new(CONNECT_MESSAGE.to_owned(), ProgressDrawTarget::hidden());
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
            ConsoleSpinner::new(CONNECT_MESSAGE.to_owned(), ProgressDrawTarget::hidden());
        let bar = progress.0.clone();
        let result = catch_unwind(AssertUnwindSafe(move || {
            let _progress = progress;
            panic!("{STARTUP_ERROR}");
        }));
        assert!(result.is_err());
        assert!(bar.is_finished());
    }
}
