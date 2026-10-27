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
pub use components::docs_link::DocsTopic;
pub use components::keybindings;
mod config_problems;
mod exit_summary;
pub use exit_summary::ExitSummary;
mod highlight;
pub mod image;
mod inline_image;
mod input_document;
mod loading_screen;
mod markdown;
mod pane_signals;
mod provenance;
mod render_worker;
pub mod repaint;
pub mod sandbox;
mod selection;
pub mod splash;
mod storage_writer;
mod theme;
pub use theme::{BUNDLED_THEMES, DEFAULT_THEME, THEME_PAIRS};
mod tty_query;
pub mod update;

mod agent;
mod event_loop;
mod input;
mod terminal;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use caudra_agent::AgentMode;
use caudra_agent::permissions::editor::{PermissionAuthorityProvider, PermissionEditError};
use caudra_agent::permissions::pattern_recognition::{
    PatternCandidate, RecognitionStats, RecognizerLimits,
};
use caudra_agent::tools::ToolFilter;
use caudra_agent::tools::native::plan::PlanTarget;
use caudra_agent::types::TodoItem;
use caudra_agent::worktree::Request as WorktreeRequest;
use caudra_config::ProfileToolPolicy;
use caudra_providers::{
    HistoryItem, HistoryProjectionError, Message, Model, active_history_items, expand_message,
    resolve_history_head, transcript_history_items,
};
use caudra_storage::StateDir;
use caudra_storage::id::{CaudraId, SessionRef};
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_storage::sessions::{
    HistoryReadLimits, HistoryReadReport, SessionCursor, SessionLease, SessionRelocation,
};
use caudra_storage::version::UpdateChannel;
use caudra_workspace::{WorkspaceChangeService, WorkspaceSession};
use color_eyre::Result;
use color_eyre::eyre::Context;
use flume::Receiver;
use ratatui::buffer::Buffer;
use ratatui::widgets::Block;

use event_loop::Handoff;
use loading_screen::{LoadProgress, LoadingScreen};

#[cfg(test)]
const PATTERN_TEST_SAMPLE_LIMIT: usize = 64;

pub type AppSession = caudra_agent::StoredSession;

#[derive(Clone)]
pub struct PermissionAuthorityBinding {
    pub provider: Arc<dyn PermissionAuthorityProvider>,
    pub tool_filter: ToolFilter,
    pub available: bool,
    pub registry_revision: u64,
}

#[derive(Clone)]
pub struct PermissionAuthoritySession {
    pub id: SessionRef,
    pub local_documents: Option<Arc<LocalDocumentStore>>,
    pub plan: Option<PlanTarget>,
}

pub type PermissionAuthorityFactory = Arc<
    dyn Fn(
            PathBuf,
            AgentMode,
            Model,
            Option<WorkspaceSession>,
            ProfileToolPolicy,
            PermissionAuthoritySession,
        ) -> Result<PermissionAuthorityBinding, PermissionEditError>
        + Send
        + Sync,
>;

/// The change records of a local session directory, `None` where the runtime
/// keeps none. Doing no I/O, it is called whenever a session or its directory
/// changes.
pub type ChangeServiceFactory =
    Arc<dyn Fn(&Path) -> Option<Arc<dyn WorkspaceChangeService>> + Send + Sync>;

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
#[cfg(test)]
pub(crate) fn open_app_session_with_cursor(
    id: CaudraId,
    storage: &StateDir,
) -> Result<(AppSession, SessionCursor)> {
    open_app_session_with_cursor_reporting(id, storage, LoadProgress::SILENT)
}

pub(crate) fn open_app_session_with_cursor_reporting(
    id: CaudraId,
    storage: &StateDir,
    progress: LoadProgress<'_>,
) -> Result<(AppSession, SessionCursor)> {
    caudra_agent::open_stored_session_with_cursor_observed(id, storage, &|event| {
        progress.session_open(event);
    })
    .context("load persisted session")
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

/// The todo list the agent restarts with: the last one `archived` and then
/// `active` committed, read from the session's stored outputs.
pub(crate) fn session_todos(
    session: &AppSession,
    archived: &[HistoryItem],
    active: &[HistoryItem],
) -> Option<Vec<TodoItem>> {
    caudra_agent::stored_todos(archived.iter().chain(active), |call_id| {
        session.tool_outputs().get(call_id).map(AsRef::as_ref)
    })
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
pub use event_loop::{EventLoopParams, SettingsReloader};

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
    /// Every session is saved and stopped so the caller can install the
    /// release from `channel`, then start it on the same sessions.
    Update {
        tabs: Vec<SessionTab>,
        focused: usize,
        channel: UpdateChannel,
    },
    Relocate {
        tabs: Vec<SessionTab>,
        focused: usize,
        relocation: SessionRelocationHandoff,
    },
    /// Every session is saved and stopped for a `/worktree` change, which only
    /// the caller can carry out: it may move this process or hand sessions to
    /// another pane of the terminal host.
    Worktree {
        tabs: Vec<SessionTab>,
        focused: usize,
        request: WorktreeRequest,
    },
    Sandbox {
        tabs: Vec<SessionTab>,
        attachment: sandbox::SandboxAttachment,
    },
    SandboxControl {
        tabs: Vec<SessionTab>,
        focused: usize,
        control: sandbox::SandboxControl,
    },
}

/// Paints the themed, empty screen the loading card sits on before the event
/// loop is built, because building it restores every session it was handed
/// and that is not instant on a large transcript.
fn draw_backdrop<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
) -> Option<Buffer> {
    terminal
        .draw(|frame| {
            let area = frame.area();
            frame.render_widget(Block::new().style(theme::current().surface_style()), area);
            color_compat::downgrade_if_needed(frame.buffer_mut());
        })
        .ok()
        .map(|frame| frame.buffer.clone())
}

/// A fresh session restores nothing, so it gets the backdrop alone.
fn startup_loading(params: &EventLoopParams, backdrop: Option<&Buffer>) -> LoadingScreen {
    let default_mode = params.ui_config.default_mode;
    match backdrop {
        Some(screen)
            if params
                .sessions
                .iter()
                .any(|tab| app::session_has_content(&tab.session, default_mode)) =>
        {
            let title = params
                .sessions
                .get(params.focused)
                .map_or("", |tab| tab.session.title.as_str());
            LoadingScreen::show(screen, title)
        }
        _ => LoadingScreen::idle(),
    }
}

pub fn run(mut params: EventLoopParams, initial_prompt: Option<String>) -> Result<RunOutcome> {
    let initial_prompt = if params.launch_autopilot {
        initial_prompt.map(app::autopilot::launch_prompt)
    } else {
        initial_prompt
    };
    let default_mode = params.ui_config.default_mode;
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
        // Before the first frame, so the backdrop and the card wear the theme
        // the session will, and before the input reader that would eat the
        // background probe's reply.
        let auto_theme = event_loop::start_theme(&params.ui_config, &mut params.startup_warnings);
        let backdrop = draw_backdrop(&mut terminal);
        let loading = startup_loading(&params, backdrop.as_ref());
        let el = event_loop::EventLoop::new(&mut terminal, params, auto_theme, loading)?;
        tracing::info!(
            terminal_ms,
            color_compat_ms,
            event_loop_new_ms = lap(),
            total_ms = started.elapsed().as_millis() as u64,
            "ui startup phases"
        );
        el.run(initial_prompt)?
    };
    match report.relocation {
        Some(Handoff::Relocation(relocation)) => {
            return Ok(RunOutcome::Relocate {
                tabs: report.tabs,
                focused: report.focused,
                relocation,
            });
        }
        Some(Handoff::Worktree(request)) => {
            return Ok(RunOutcome::Worktree {
                tabs: report.tabs,
                focused: report.focused,
                request,
            });
        }
        None => {}
    }
    if let Some(attachment) = report.sandbox {
        return Ok(RunOutcome::Sandbox {
            tabs: report.tabs,
            attachment,
        });
    }
    if let Some(control) = report.sandbox_control {
        return Ok(RunOutcome::SandboxControl {
            tabs: report.tabs,
            focused: report.focused,
            control,
        });
    }
    Ok(match report.exit {
        components::ExitRequest::Reload => RunOutcome::Reload {
            tabs: report.tabs,
            focused: report.focused,
        },
        components::ExitRequest::Update(channel) => RunOutcome::Update {
            tabs: report.tabs,
            focused: report.focused,
            channel,
        },
        exit => {
            let summary = report
                .tabs
                .get(report.focused)
                .filter(|tab| app::session_has_content(&tab.session, default_mode))
                .map(|tab| {
                    let others = report
                        .tabs
                        .iter()
                        .enumerate()
                        .filter(|&(index, other)| {
                            index != report.focused
                                && app::session_has_content(&other.session, default_mode)
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
