//! Console progress for schema migrations.
//!
//! A migration rewrites every payload in the database and can run for half a
//! minute on a large one. Without this the terminal sits blank and the upgrade
//! is indistinguishable from a hang, which is exactly when someone reaches for
//! the interrupt that would leave the database part-migrated.

use std::io::IsTerminal;
use std::io::stderr;
use std::sync::Mutex;
use std::time::Duration;

use caudra_storage::sessions::migration::{MigrationEvent, set_reporter};
use indicatif::{HumanDuration, ProgressBar, ProgressDrawTarget, ProgressStyle};

/// Wide enough for `rewriting subagent_history_items`, the longest label, so the
/// bar does not jump sideways when that phase starts.
const BAR_TEMPLATE: &str = "  {msg:<32} [{bar:20}] {pos}/{len} {elapsed_precise}";
const BAR_CHARS: &str = "=> ";
const BAR_REFRESH: Duration = Duration::from_millis(100);
const BACKUP_MESSAGE: &str = "backing up (pages)";

/// Installs the console reporter, unless stderr is redirected. A progress bar
/// drawn into a pipe is noise in a log, and headless callers own their output.
pub fn install_progress() {
    if !stderr().is_terminal() {
        return;
    }
    let bar: Mutex<Option<ProgressBar>> = Mutex::new(None);
    set_reporter(move |event| {
        let Ok(mut bar) = bar.lock() else {
            return;
        };
        render(&mut bar, event);
    });
}

fn render(bar: &mut Option<ProgressBar>, event: MigrationEvent) {
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
        MigrationEvent::Finished { from, to } => {
            if let Some(bar) = bar.take() {
                bar.finish_and_clear();
                let elapsed = HumanDuration(bar.elapsed());
                eprintln!("Session database upgraded from schema {from} to {to} in {elapsed}.");
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
