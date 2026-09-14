//! Single-threaded ratatui event loop; the agent runs on smol tasks in a separate thread.
//! `AgentHandles` bundles all flume channels to the agent. `dispatch()` processes
//! `Action`s returned by `App::update()`. Scroll and drag events are coalesced from
//! the queue to avoid jank.

pub mod animation;
pub mod app;
mod appearance;
pub mod chat;
mod clipboard;
mod clock;
mod color_compat;
mod components;
pub use components::command::{BUILTIN_COMMANDS, BuiltinCommand, ChatScope};
pub use components::keybindings;
mod exit_summary;
pub use exit_summary::ExitSummary;
mod highlight;
pub use highlight::highlight_ansi;
mod herdr;
pub use herdr::{HerdrReporter, HerdrReporterHandle};
pub mod image;
mod input_document;
mod markdown;
mod provenance;
mod render_worker;
pub mod repaint;
mod selection;
pub mod splash;
mod storage_writer;
mod text_buffer;
mod theme;
pub use theme::{BUNDLED_THEMES, DEFAULT_THEME, THEME_PAIRS};
mod tty_query;
pub mod update;

mod agent;
mod event_loop;
mod input;
mod terminal;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use caudra_providers::{
    HistoryItem, HistoryProjectionError, Message, active_history_items, expand_message,
    resolve_history_head, transcript_history_items,
};
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::{SessionLease, SessionRelocation};
use color_eyre::Result;
use color_eyre::eyre::Context;

pub type AppSession = caudra_agent::StoredSession;

pub(crate) fn load_app_session(id: CaudraId, storage: &StateDir) -> Result<AppSession> {
    caudra_agent::load_stored_session(id, storage).context("load persisted session")
}

/// The picker path: the user chose this session, so it counts as opened.
pub(crate) fn open_app_session(id: CaudraId, storage: &StateDir) -> Result<AppSession> {
    caudra_agent::open_stored_session(id, storage).context("load persisted session")
}

pub(crate) fn session_history_head(session: &AppSession) -> Option<CaudraId> {
    resolve_history_head(
        session.messages(),
        session.meta.history_head,
        session.meta.pending_revert.is_some(),
    )
}

pub(crate) fn active_session_history(
    session: &AppSession,
) -> Result<Vec<HistoryItem>, HistoryProjectionError> {
    active_history_items(session.messages(), session_history_head(session))
}

/// What the transcript draws, which is more than what the next request carries.
/// A compaction replaces turns in the request and leaves them in the store, so
/// this crosses back over that seam while [`active_session_history`] stops at it.
pub(crate) fn transcript_session_history(
    session: &AppSession,
) -> Result<Vec<HistoryItem>, HistoryProjectionError> {
    transcript_history_items(session.messages(), session_history_head(session))
}

pub(crate) fn history_items(messages: &[Message]) -> Vec<HistoryItem> {
    let mut items = Vec::new();
    for message in messages {
        items.extend(expand_message(
            message,
            items.last().map(|item: &HistoryItem| item.id),
        ));
    }
    items
}

#[cfg(test)]
pub(crate) fn push_history_message(session: &mut AppSession, message: Message) {
    for item in expand_message(&message, session.messages().last().map(|item| item.id)) {
        session.push_message(item);
    }
}

pub(crate) use agent::AgentCommand;
pub use event_loop::EventLoopParams;

pub struct SessionTab {
    pub session: AppSession,
    pub lease: Arc<caudra_storage::sessions::SessionLease>,
}

pub struct SessionRelocationHandoff {
    pub request: SessionRelocation,
    pub donor: Option<(CaudraId, String)>,
    pub leases: Vec<Arc<SessionLease>>,
}

/// How a UI generation ended. On `Reload`, each tab carries its in-memory
/// session so the caller reopens everything without re-reading from disk.
pub enum RunOutcome {
    Exit {
        summary: Option<ExitSummary>,
        code: ExitCode,
    },
    Reload {
        tabs: Vec<SessionTab>,
        focused: usize,
    },
    Relocate {
        tabs: Vec<SessionTab>,
        focused: usize,
        relocation: SessionRelocationHandoff,
    },
}

pub fn run(params: EventLoopParams, initial_prompt: Option<String>) -> Result<RunOutcome> {
    let report = {
        // Nothing between the last `caudra` startup phase and the first frame is
        // logged otherwise, so a slow open has nowhere left to hide.
        let started = Instant::now();
        let mut phase_start = started;
        let mut lap = || {
            let elapsed = phase_start.elapsed().as_millis() as u64;
            phase_start = Instant::now();
            elapsed
        };
        let (_guard, mut terminal) = terminal::TerminalGuard::init()?;
        let terminal_ms = lap();
        color_compat::init();
        let color_compat_ms = lap();
        let el = event_loop::EventLoop::new(&mut terminal, params)?;
        tracing::info!(
            terminal_ms,
            color_compat_ms,
            event_loop_new_ms = lap(),
            total_ms = started.elapsed().as_millis() as u64,
            "ui startup phases"
        );
        el.run(initial_prompt)?
    };
    if let Some(relocation) = report.relocation {
        return Ok(RunOutcome::Relocate {
            tabs: report.tabs,
            focused: report.focused,
            relocation,
        });
    }
    Ok(match report.exit {
        components::ExitRequest::Reload => RunOutcome::Reload {
            tabs: report.tabs,
            focused: report.focused,
        },
        exit => {
            let summary = report
                .tabs
                .get(report.focused)
                .filter(|tab| app::session_has_content(&tab.session))
                .map(|tab| {
                    let others = report
                        .tabs
                        .iter()
                        .enumerate()
                        .filter(|&(index, other)| {
                            index != report.focused && app::session_has_content(&other.session)
                        })
                        .count();
                    ExitSummary::new(&tab.session, report.run_time, others)
                });
            let started = Instant::now();
            drop(report);
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "session buffers dropped"
            );
            RunOutcome::Exit {
                summary,
                code: exit.code(),
            }
        }
    })
}
