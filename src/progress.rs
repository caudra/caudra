//! Console progress for the storage operations that would otherwise run silent.
//!
//! A migration rewrites every payload and a prune returns freelist pages one at
//! a time; on a large database either runs for minutes. Without this the
//! terminal sits blank and the work is indistinguishable from a hang, which is
//! exactly when someone reaches for the interrupt that would leave the database
//! part-migrated.

use std::io::IsTerminal;
use std::io::stderr;
use std::sync::Mutex;
use std::time::Duration;

use caudra_storage::sessions::progress::{MIGRATION, MigrationEvent, PRUNE, PruneEvent};
use indicatif::{HumanCount, HumanDuration, ProgressBar, ProgressDrawTarget, ProgressStyle};

/// Wide enough for `rewriting subagent_history_items`, the longest label, so the
/// bar does not jump sideways when that phase starts.
const BAR_TEMPLATE: &str = "  {msg:<32} [{bar:20}] {pos}/{len} {elapsed_precise}";
const BAR_CHARS: &str = "=> ";
const BAR_REFRESH: Duration = Duration::from_millis(100);
const BACKUP_MESSAGE: &str = "backing up (pages)";
const RECLAIM_MESSAGE: &str = "reclaiming (pages)";

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
