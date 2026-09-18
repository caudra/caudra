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

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use caudra_agent::AgentMode;
use caudra_agent::permissions::editor::{PermissionAuthorityProvider, PermissionEditError};
use caudra_agent::permissions::pattern_recognition::{
    PatternCandidate, RecognitionStats, RecognizerLimits,
};
use caudra_agent::tools::ToolFilter;
use caudra_providers::{
    HistoryItem, HistoryProjectionError, Message, Model, active_history_items, expand_message,
    resolve_history_head, transcript_history_items,
};
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::{
    HistoryReadLimits, HistoryReadReport, SessionLease, SessionRelocation,
};
use caudra_workspace::WorkspaceSession;
use color_eyre::Result;
use color_eyre::eyre::Context;
use flume::Receiver;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Stylize;
use ratatui::widgets::Paragraph;

#[cfg(test)]
const PATTERN_TEST_SAMPLE_LIMIT: usize = 64;
const LOADING_PREFIX: &str = "Loading";

pub type AppSession = caudra_agent::StoredSession;

#[derive(Clone)]
pub struct PermissionAuthorityBinding {
    pub provider: Arc<dyn PermissionAuthorityProvider>,
    pub tool_filter: ToolFilter,
    pub available: bool,
    pub registry_revision: u64,
}

pub type PermissionAuthorityFactory = Arc<
    dyn Fn(
            PathBuf,
            AgentMode,
            Model,
            Option<WorkspaceSession>,
        ) -> Result<PermissionAuthorityBinding, PermissionEditError>
        + Send
        + Sync,
>;

/// Enqueues bounded discovery, never performs it on the UI thread. Dropping the
/// reply receiver cancels interest; replies are proposals, not permission rules.
pub type PatternSuggestionLoader = Arc<
    dyn Fn(PathBuf, PatternDiscoveryMode) -> Receiver<Arc<PatternDiscoveryOutcome>> + Send + Sync,
>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternDiscoveryMode {
    Cached,
    Refresh,
}

#[derive(Debug)]
pub enum PatternDiscoveryOutcome {
    Ready(Box<PatternDiscoveryReport>),
    Unavailable(&'static str),
}

#[derive(Debug)]
pub struct PatternDiscoveryReport {
    pub candidates: Arc<[PatternCandidate]>,
    pub sample: HistoryReadReport,
    pub history_limits: HistoryReadLimits,
    pub recognition: RecognitionStats,
    pub recognizer_limits: RecognizerLimits,
    pub calls: usize,
    pub max_calls: usize,
    pub analysis_bytes: usize,
    pub max_analysis_bytes: usize,
    pub max_elapsed_ms: u64,
    pub partial_reasons: Vec<String>,
}

#[cfg(test)]
pub(crate) fn test_pattern_discovery_report(
    candidates: Vec<PatternCandidate>,
) -> Box<PatternDiscoveryReport> {
    Box::new(PatternDiscoveryReport {
        candidates: candidates.into(),
        sample: HistoryReadReport::default(),
        history_limits: HistoryReadLimits {
            max_sessions: PATTERN_TEST_SAMPLE_LIMIT,
            max_rows: PATTERN_TEST_SAMPLE_LIMIT,
            max_bytes: PATTERN_TEST_SAMPLE_LIMIT,
            max_row_bytes: PATTERN_TEST_SAMPLE_LIMIT,
        },
        recognition: RecognitionStats::default(),
        recognizer_limits: RecognizerLimits::default(),
        calls: 0,
        max_calls: PATTERN_TEST_SAMPLE_LIMIT,
        analysis_bytes: 0,
        max_analysis_bytes: PATTERN_TEST_SAMPLE_LIMIT,
        max_elapsed_ms: PATTERN_TEST_SAMPLE_LIMIT as u64,
        partial_reasons: Vec::new(),
    })
}

pub(crate) fn load_app_session(id: CaudraId, storage: &StateDir) -> Result<AppSession> {
    caudra_agent::load_stored_session(id, storage).context("load persisted session")
}

/// The picker path: the user chose this session, so it counts as opened. The
/// cursor comes back too, so the first save stays a delta.
pub(crate) fn open_app_session_with_cursor(
    id: CaudraId,
    storage: &StateDir,
) -> Result<(AppSession, caudra_storage::sessions::SessionCursor)> {
    caudra_agent::open_stored_session_with_cursor(id, storage).context("load persisted session")
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

/// The transcript's prefix that the request no longer carries: every stretch
/// a compaction replaced. Empty when the session never compacted or cannot be
/// read, so a broken graph costs the archive and nothing else.
pub(crate) fn archived_session_history(session: &AppSession) -> Vec<HistoryItem> {
    let Ok(mut transcript) = transcript_session_history(session) else {
        return Vec::new();
    };
    let active = active_session_history(session).map_or(0, |items| items.len());
    transcript.truncate(transcript.len().saturating_sub(active));
    transcript
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
    /// The cursor the load produced, when this tab came from storage. The
    /// writer needs it to make the first save a delta instead of a rewrite.
    pub cursor: Option<caudra_storage::sessions::SessionCursor>,
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

/// Paints one frame before the event loop is built, because building it
/// restores every session it was handed and that is not instant on a large
/// transcript. Without this the terminal sits on a bare alternate screen with
/// nothing to say it is working.
fn draw_loading_screen<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    message: &str,
) {
    let _ = terminal.draw(|frame| {
        let area = frame.area();
        let line = Rect {
            y: area.height / 2,
            height: 1,
            ..area
        };
        frame.render_widget(
            Paragraph::new(message)
                .alignment(Alignment::Center)
                .dim(),
            line,
        );
    });
}

/// Names the session being restored when there is one, since on startup that
/// is the only thing distinguishing a slow open from a hung one.
fn loading_message(params: &EventLoopParams) -> String {
    loading_text(
        params
            .sessions
            .get(params.focused)
            .map(|tab| tab.session.title.as_str()),
    )
}

fn loading_text(title: Option<&str>) -> String {
    match title.map(str::trim).filter(|title| !title.is_empty()) {
        Some(title) => format!("{LOADING_PREFIX} {title}"),
        None => LOADING_PREFIX.to_owned(),
    }
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
        draw_loading_screen(&mut terminal, &loading_message(&params));
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

#[cfg(test)]
mod loading_screen_tests {
    use super::{draw_loading_screen, loading_text};

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use test_case::test_case;

    const FRAME_SHOWS_MESSAGE: &str = "the frame drawn before the event loop must show the message";
    const SESSION_TITLE: &str = "Add echo to allowed tool calls";

    #[test_case(Some(SESSION_TITLE), "Loading Add echo to allowed tool calls"; "a titled session is named")]
    #[test_case(Some("  "), "Loading"; "a blank title is dropped")]
    #[test_case(None, "Loading"; "a session without a title still reports work")]
    fn loading_text_names_the_session(title: Option<&str>, expected: &str) {
        assert_eq!(loading_text(title), expected);
    }

    #[test]
    fn the_loading_frame_reaches_the_buffer() {
        let mut terminal = Terminal::new(TestBackend::new(40, 5)).unwrap();

        draw_loading_screen(&mut terminal, &loading_text(None));

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(rendered.contains("Loading"), "{FRAME_SHOWS_MESSAGE}");
    }
}
