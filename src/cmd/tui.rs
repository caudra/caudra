use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver as ShutdownReceiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use color_eyre::Result;
use color_eyre::eyre::{Context, bail, eyre};
use flume::{RecvTimeoutError as PatternRecvError, Sender as ChannelSender};

use caudra_agent::command::{self, CustomCommand};
use caudra_agent::decisions::Decisions;
use caudra_agent::herdr::HerdrEnv;
use caudra_agent::peers::topics::add_patterns;
use caudra_agent::peers::{MAX_MEMBERSHIPS, PeerHost, TOO_MANY_MEMBERSHIPS};
use caudra_agent::permissions::pattern_recognition::{PatternCandidate, RecognitionExclusion};
use caudra_agent::prompt::profile::{PromptProfileCatalog, SystemPromptProfile};
use caudra_agent::tools::{DescriptionContext, ToolAudience, ToolFilter, ToolRegistry};
use caudra_agent::worktree::Backend;
use caudra_config::providers::builtin_provider;
use caudra_config::sandbox::SandboxName;
use caudra_config::{Config, Feature, StorageConfig};
use caudra_lua::PluginHost;
use caudra_providers::model::Model;
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::sweep::{SweepPolicy, sweep_if_due};
use caudra_storage::sessions::{
    SessionDatabase, SessionLease, SessionLocation, SessionRelocation, SessionRelocationResult,
    StoredMode,
};
use caudra_storage::state::{WorkspaceTabs, read_workspace_tabs};
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_storage::{StateClass, StateDir};
use caudra_ui::{
    AppSession, ChangeServiceFactory, ExitSummary, HerdrReporter, PatternDiscoveryMode,
    PatternDiscoveryOutcome, PatternDiscoveryReport, PatternSuggestionLoader,
    PermissionAuthorityBinding, RunOutcome, SessionRelocationHandoff, SessionTab,
};
use caudra_workcell::editor_adapter::{
    PermissionEditorContext, PermissionEditorRuntime, permission_authority_provider,
};
use caudra_workcell::{PatternObligationKind, RemoteConnectionStatus};

use super::worktree::{self, Executed, Handled};
use crate::cli::Cli;
use crate::cmd::load_config;
use crate::cmd::permissions::discover::{
    DiscoveryLimits, DiscoveryReport, RECOGNIZER_CAPACITY, RECOGNIZER_ORDER_BIAS,
    discover_for_project_cancellable,
};
use crate::docs;
use crate::progress::SandboxProgress;
use crate::setup;

const LOGIN_PLACEHOLDER_PROVIDER: &str = "anthropic";
const LOGIN_PLACEHOLDER_UNAVAILABLE: &str = "built-in login placeholder provider is not registered";
const LOGIN_PLACEHOLDER_INVALID: &str = "resolve built-in login placeholder model";
const CONFIG_FALLBACK_WARNING: &str = "config reload failed, using previous config";
const TIGHTENED_DIRS_WARNING: &str =
    "Caudra directories were group- or world-writable and have been set to owner-only";
const MODEL_FALLBACK_WARNING: &str = "model resolution failed, keeping previous model";
/// The first sweep waits for startup and the first prompt to settle.
const SWEEP_STARTUP_DELAY: Duration = Duration::from_secs(60);
/// How often the sweep thread re-checks whether the interval has elapsed.
const SWEEP_POLL_INTERVAL: Duration = Duration::from_secs(60 * 60);
const SECONDS_PER_HOUR: u64 = 60 * 60;
const RELOCATION_ABORTED: &str = "Session relocation was not committed";
const RELOCATION_DONOR_CHANGED: &str =
    "Destination session disappeared or changed directory; refresh the relocation preview";
const RELOCATION_VERSION_CHANGED: &str =
    "Session changed outside the stopped live writer; refresh the relocation preview";
const RELOCATION_DESTINATION_CHANGED: &str =
    "Destination directory changed; refresh the relocation preview";
const RELOCATION_ROLLBACK_FAILED: &str =
    "Could not restore the original working directory; the UI was not restarted";
const PROJECT_ENV_PATH: &str = ".caudra/.env";
const MESSAGING_UNAVAILABLE: &str = "Cross-session messaging unavailable";
const LAUNCH_NEEDS_LOCAL_MESSAGING: &str = "--name, --topic, and --receive-broadcasts need cross-session messaging, which only local sessions support";
const RELOCATION_ENV_RESTART: &str =
    "Run caudra --continue from the destination to load its environment safely";
const RELOCATION_USAGE_UNCHANGED: &str = "Historical project usage attribution was left unchanged";
const RELOCATION_USAGE_EMPTY: &str =
    "No historical project usage was recorded for the source directory";
const RELOCATION_PLANS_DETACHED: &str = "Active source plans and approvals were detached";
const RELOCATION_APPROVALS_DETACHED: &str =
    "Approvals were detached; plans stay attached, since both directories share project state";
/// Relocation clears record coverage, so file revert cannot reach a change
/// recorded in the old directory's store.
const RELOCATION_FILES_UNMOVED: &str =
    "Files were not moved, and file changes made before the move can no longer be reverted";
const PATTERN_LOAD_QUEUE: usize = 32;
const PATTERN_CACHE_PROJECTS: usize = 8;
const PATTERN_CACHE_BYTES_PER_PROJECT: usize = 256 * 1024;
const PATTERN_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const PATTERN_RETRY_DELAY: Duration = Duration::from_secs(30);
const PATTERN_LOAD_POLL: Duration = Duration::from_millis(25);
const PATTERN_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const PATTERN_HISTORY_SESSIONS: usize = 64;
const PATTERN_HISTORY_ROWS: usize = 2048;
const PATTERN_HISTORY_BYTES: usize = 4 * 1024 * 1024;
const PATTERN_HISTORY_ROW_BYTES: usize = 128 * 1024;
const PATTERN_HISTORY_CALLS: usize = 512;
const PATTERN_ANALYSIS_BYTES: usize = 512 * 1024;
const PATTERN_SCAN_UNAVAILABLE: &str = "History could not be read or analyzed. Refresh to retry.";
const PATTERN_PROJECT_UNAVAILABLE: &str = "Discovery requires a canonical local project directory.";
const PATTERN_CACHE_LIMIT: &str = "Proposal display/cache limit reached";
const PATTERN_SESSION_LIMIT: &str = "Per-parent history row limit";
const PATTERN_OMITTED_SCOPES: &str = "Omitted command scopes";
const PATTERN_SOURCE_OBLIGATIONS: &str = "Source/effect obligations";

struct PatternLoadRequest {
    project: PathBuf,
    mode: PatternDiscoveryMode,
    requested_at: Instant,
    reply: ChannelSender<Arc<PatternDiscoveryOutcome>>,
}

struct CachedPatternSuggestions {
    project: PathBuf,
    loaded_at: Instant,
    outcome: Arc<PatternDiscoveryOutcome>,
}

impl CachedPatternSuggestions {
    fn fresh_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.loaded_at)
            < if matches!(self.outcome.as_ref(), PatternDiscoveryOutcome::Ready(_)) {
                PATTERN_CACHE_TTL
            } else {
                PATTERN_RETRY_DELAY
            }
    }
}

struct PatternSuggestionWorker {
    loader: PatternSuggestionLoader,
    requests: Option<ChannelSender<PatternLoadRequest>>,
    stop: Arc<AtomicBool>,
    finished: ShutdownReceiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl PatternSuggestionWorker {
    fn spawn(storage: &StateDir, remote: bool) -> Option<Self> {
        if remote || storage.is_ephemeral() {
            return None;
        }
        Self::spawn_with(
            storage.for_class(StateClass::Persistent),
            load_history_patterns,
        )
    }

    fn spawn_with(
        storage: StateDir,
        scan: impl Fn(&StateDir, &Path, &dyn Fn() -> bool) -> PatternDiscoveryOutcome + Send + 'static,
    ) -> Option<Self> {
        let (requests, incoming) = flume::bounded::<PatternLoadRequest>(PATTERN_LOAD_QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let (done, finished) = mpsc::channel();
        let stopping = Arc::clone(&stop);
        let thread = match thread::Builder::new()
            .name("pattern-suggestions".into())
            .spawn(move || {
                let mut cache = VecDeque::<CachedPatternSuggestions>::new();
                while !stopping.load(Ordering::Acquire) {
                    let request = match incoming.recv_timeout(PATTERN_LOAD_POLL) {
                        Ok(request) => request,
                        Err(PatternRecvError::Timeout) => continue,
                        Err(PatternRecvError::Disconnected) => break,
                    };
                    let cancelled =
                        || stopping.load(Ordering::Acquire) || request.reply.is_disconnected();
                    if cancelled() {
                        continue;
                    }
                    if !request.project.is_absolute()
                        || !fs::canonicalize(&request.project)
                            .is_ok_and(|path| path == request.project && path.is_dir())
                    {
                        let _ =
                            request
                                .reply
                                .try_send(Arc::new(PatternDiscoveryOutcome::Unavailable(
                                    PATTERN_PROJECT_UNAVAILABLE,
                                )));
                        continue;
                    }
                    let now = Instant::now();
                    cache.retain(|entry| entry.fresh_at(now));
                    if let Some(entry) = cache.iter().find(|entry| {
                        entry.project == request.project
                            && (request.mode == PatternDiscoveryMode::Cached
                                || entry.loaded_at >= request.requested_at)
                    }) {
                        let _ = request.reply.try_send(Arc::clone(&entry.outcome));
                        continue;
                    }
                    let mut outcome = scan(&storage, &request.project, &cancelled);
                    if cancelled() {
                        continue;
                    }
                    if let PatternDiscoveryOutcome::Ready(report) = &mut outcome {
                        let count = report.candidates.len();
                        report.candidates = cacheable_pattern_candidates(
                            &request.project,
                            report.candidates.to_vec(),
                        );
                        if report.candidates.len() < count {
                            report.partial_reasons.push(PATTERN_CACHE_LIMIT.into());
                        }
                    }
                    if cancelled() {
                        continue;
                    }
                    let outcome = Arc::new(outcome);
                    cache.retain(|entry| entry.project != request.project);
                    if cache.len() == PATTERN_CACHE_PROJECTS {
                        cache.pop_front();
                    }
                    cache.push_back(CachedPatternSuggestions {
                        project: request.project,
                        loaded_at: Instant::now(),
                        outcome: Arc::clone(&outcome),
                    });
                    let _ = request.reply.try_send(outcome);
                }
                drop(cache);
                let _ = done.send(());
            }) {
            Ok(thread) => thread,
            Err(error) => {
                tracing::warn!(kind = ?error.kind(), "history pattern suggestion worker unavailable");
                return None;
            }
        };
        let stopping = Arc::clone(&stop);
        let weak_requests = requests.downgrade();
        let loader = Arc::new(move |project, mode| {
            let (reply, receiver) = flume::bounded(1);
            if !stopping.load(Ordering::Acquire)
                && let Some(requests) = weak_requests.upgrade()
            {
                let _ = requests.try_send(PatternLoadRequest {
                    project,
                    mode,
                    requested_at: Instant::now(),
                    reply,
                });
            }
            receiver
        });
        Some(Self {
            loader,
            requests: Some(requests),
            stop,
            finished,
            thread: Some(thread),
        })
    }

    fn shutdown(&mut self, budget: Duration) -> bool {
        self.stop.store(true, Ordering::Release);
        self.requests = None;
        let Some(thread) = self.thread.take() else {
            return true;
        };
        if matches!(
            self.finished.recv_timeout(budget),
            Err(RecvTimeoutError::Timeout)
        ) {
            // Rust cannot preempt a thread stuck in kernel I/O. The worker has
            // no manager handles; the UI has already dropped its reply receivers.
            tracing::warn!(
                budget_ms = budget.as_millis(),
                "pattern discovery did not stop within shutdown budget"
            );
            return false;
        }
        if thread.join().is_err() {
            tracing::warn!("history pattern suggestion worker panicked");
        }
        true
    }
}

impl Drop for PatternSuggestionWorker {
    fn drop(&mut self) {
        self.shutdown(PATTERN_SHUTDOWN_TIMEOUT);
    }
}

fn cacheable_pattern_candidates(
    project: &Path,
    candidates: Vec<PatternCandidate>,
) -> Arc<[PatternCandidate]> {
    let mut fingerprints = HashSet::new();
    let mut bytes = 0;
    let mut retained = Vec::new();
    for candidate in candidates
        .into_iter()
        .take(DiscoveryLimits::default().max_suggestions)
    {
        if Path::new(&candidate.definition.context.path_binding) != project {
            continue;
        }
        let Ok(fingerprint) = candidate.definition.fingerprint() else {
            continue;
        };
        let Ok(encoded) = serde_json::to_vec(&candidate) else {
            continue;
        };
        if encoded.len() > PATTERN_CACHE_BYTES_PER_PROJECT.saturating_sub(bytes)
            || !fingerprints.insert(fingerprint)
        {
            continue;
        }
        bytes += encoded.len();
        retained.push(candidate);
    }
    Arc::from(retained)
}

fn load_history_patterns(
    storage: &StateDir,
    project: &Path,
    cancelled: &dyn Fn() -> bool,
) -> PatternDiscoveryOutcome {
    let mut limits = DiscoveryLimits {
        max_calls: PATTERN_HISTORY_CALLS,
        max_analysis_bytes: PATTERN_ANALYSIS_BYTES,
        ..DiscoveryLimits::default()
    };
    limits.history.max_sessions = PATTERN_HISTORY_SESSIONS;
    limits.history.max_rows = PATTERN_HISTORY_ROWS;
    limits.history.max_bytes = PATTERN_HISTORY_BYTES;
    limits.history.max_row_bytes = PATTERN_HISTORY_ROW_BYTES;
    let report = match discover_for_project_cancellable(storage, project, limits, cancelled) {
        Ok(report) => report,
        Err(_) => {
            tracing::debug!("history pattern discovery unavailable");
            return PatternDiscoveryOutcome::Unavailable(PATTERN_SCAN_UNAVAILABLE);
        }
    };
    tracing::debug!(
        rows = report.sample.rows,
        calls = report.processing.calls,
        candidates = report.candidates.len(),
        unavailable = report.processing.storage_unavailable,
        cancelled = report.processing.cancelled,
        timed_out = report.processing.timed_out,
        "history pattern discovery completed"
    );
    discovery_outcome(report)
}

fn discovery_outcome(report: DiscoveryReport) -> PatternDiscoveryOutcome {
    if report.processing.storage_unavailable || report.processing.recognition_failed {
        return PatternDiscoveryOutcome::Unavailable(PATTERN_SCAN_UNAVAILABLE);
    }
    let mut partial_reasons: Vec<String> = [
        (report.sample.truncated, "History sample incomplete"),
        (report.sample.stopped, "History sampling stopped early"),
        (report.processing.call_limit, "Tool-call limit reached"),
        (
            report.processing.analysis_byte_limit,
            "Analysis byte limit reached",
        ),
        (
            report.processing.observation_limit,
            "Observation limit reached",
        ),
        (report.processing.timed_out, "Time budget reached"),
        (
            report.processing.sampling_time_limit,
            "Sampling time budget reached",
        ),
        (
            report.processing.recognition_time_limit,
            "Recognition time budget reached",
        ),
        (report.processing.cancelled, "Scan cancelled"),
    ]
    .into_iter()
    .filter(|(active, _)| *active)
    .map(|(_, reason)| reason.to_string())
    .collect();
    if report.sample.session_row_cutoffs > 0 {
        partial_reasons.push(format!(
            "{PATTERN_SESSION_LIMIT}: {} sessions cut short at {} rows (main + subagents)",
            report.sample.session_row_cutoffs, report.limits.max_rows_per_session
        ));
    }
    for (label, count) in [
        (
            "Oversized history rows skipped",
            report.sample.oversized_rows,
        ),
        (
            "Invalid history records skipped",
            report.sample.invalid_records,
        ),
        ("Nonlocal sessions skipped", report.sample.nonlocal_sessions),
        (
            "Repeated history rows skipped",
            report.processing.duplicate_records,
        ),
        (
            "Conflicting history identities quarantined",
            report.processing.colliding_records,
        ),
        (
            "Previously quarantined rows skipped",
            report.processing.quarantined_records,
        ),
        (
            "Shell analysis failures",
            report.processing.analysis_failures,
        ),
    ] {
        if count > 0 {
            partial_reasons.push(format!("{label}: {count}"));
        }
    }
    let stats = &report.processing;
    if stats.calls_with_omitted_commands > 0
        || stats.calls_with_incomplete_source > 0
        || stats.calls_with_incomplete_context > 0
    {
        partial_reasons.push(format!(
            "Observed {}/{} represented command scopes; {} calls omit commands; {} incomplete source, {} incomplete context (not full-call authorization)",
            stats.observed_commands,
            stats.represented_commands,
            stats.calls_with_omitted_commands,
            stats.calls_with_incomplete_source,
            stats.calls_with_incomplete_context
        ));
    }
    for (reason, count) in &stats.exclusions {
        partial_reasons.push(format!("Excluded records/calls ({reason:?}): {count}"));
    }
    for (reason, count) in &stats.omission_counts {
        partial_reasons.push(format!("{PATTERN_OMITTED_SCOPES} ({reason:?}): {count}"));
    }
    for obligation in &stats.obligation_counts {
        if obligation.kind != PatternObligationKind::ProgramEffectsNotAssessed {
            partial_reasons.push(format!(
                "{PATTERN_SOURCE_OBLIGATIONS} ({:?}): {} (not full-call authorization)",
                obligation.kind, obligation.count
            ));
        }
    }
    for (reason, count) in &report.recognition.exclusions {
        partial_reasons.push(match reason {
            RecognitionExclusion::Capacity => {
                format!("{RECOGNIZER_CAPACITY}: {count}. {RECOGNIZER_ORDER_BIAS}")
            }
            _ => format!("Recognizer exclusions ({reason:?}): {count}"),
        });
    }
    PatternDiscoveryOutcome::Ready(Box::new(PatternDiscoveryReport {
        candidates: Arc::from(report.candidates),
        sample: report.sample,
        history_limits: report.limits.history,
        recognition: report.recognition,
        recognizer_limits: report.recognizer_limits,
        calls: report.processing.calls,
        max_calls: report.limits.max_calls,
        analysis_bytes: report.processing.analysis_bytes,
        max_analysis_bytes: report.limits.max_analysis_bytes,
        max_elapsed_ms: report.limits.max_elapsed_ms,
        partial_reasons,
    }))
}

/// Runs the retention sweep on its own thread while the TUI is open. Dropping
/// the sender wakes and stops the thread. Every step the sweep takes is
/// crash-safe, so shutdown never waits for a deletion in progress.
struct RetentionSweeper {
    _stop: Option<Sender<()>>,
}

impl RetentionSweeper {
    fn spawn(storage: StateDir, config: StorageConfig) -> Self {
        if storage.is_ephemeral() {
            return Self { _stop: None };
        }
        let retention = config.retention;
        let policy = SweepPolicy {
            group_by: retention.group_by,
            interval: Duration::from_secs(retention.sweep_interval_hours * SECONDS_PER_HOUR),
            trim: retention.trim,
            forget: retention.forget,
            store_budget: config.snapshots.store_budget(),
        };
        if policy.interval.is_zero() {
            return Self { _stop: None };
        }
        let (stop, stopped) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("retention-sweep".into())
            .spawn(move || {
                let mut delay = SWEEP_STARTUP_DELAY;
                loop {
                    match stopped.recv_timeout(delay) {
                        Err(RecvTimeoutError::Timeout) => {}
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                    }
                    if let Err(error) = sweep_if_due(&storage, &policy, &jiff::Zoned::now()) {
                        tracing::warn!(%error, "retention sweep failed");
                    }
                    delay = SWEEP_POLL_INTERVAL.min(policy.interval);
                }
            });
        match spawned {
            Ok(_) => Self { _stop: Some(stop) },
            Err(error) => {
                tracing::warn!(%error, "retention sweep thread not started");
                Self { _stop: None }
            }
        }
    }
}

/// One generation of the app: everything torn down and rebuilt on `/reload`.
/// Dropping it joins the Lua thread via `PluginHost::drop`.
struct Stack {
    plugin_host: PluginHost,
    config: Config,
    commands: Vec<CustomCommand>,
    model: Model,
    needs_login: bool,
    prompt_profiles: Arc<PromptProfileCatalog>,
    default_prompt_profile: Option<Arc<SystemPromptProfile>>,
}

type StackFallback = (
    Config,
    Model,
    Arc<PromptProfileCatalog>,
    Option<Arc<SystemPromptProfile>>,
);

impl Stack {
    fn timeouts(&self) -> caudra_providers::Timeouts {
        caudra_providers::Timeouts {
            connect: self.config.provider.connect_timeout,
            stream: self.config.provider.stream_timeout,
        }
    }
}

/// Background teardown of the previous generation. `defer` keeps the slow
/// drop (a Lua thread join, capped at 2s in `PluginHost::drop`) off the
/// `/reload` hot path. Joining on replace and on drop covers every exit
/// path, including `?` unwinds, so no VM is abandoned mid-shutdown and at
/// most one teardown is ever in flight.
#[derive(Default)]
struct Teardown(Option<JoinHandle<()>>);

impl Teardown {
    fn defer(&mut self, work: impl FnOnce() + Send + 'static) {
        self.join();
        self.0 = Some(thread::spawn(work));
    }

    fn join(&mut self) {
        if let Some(handle) = self.0.take()
            && handle.join().is_err()
        {
            tracing::warn!("background teardown panicked");
        }
    }
}

impl Drop for Teardown {
    fn drop(&mut self) {
        self.join();
    }
}

fn discover_commands(disable: bool) -> Vec<CustomCommand> {
    if disable {
        return Vec::new();
    }
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    command::discover_commands(&cwd)
}

fn login_placeholder_model() -> Result<Model> {
    let provider = builtin_provider(LOGIN_PLACEHOLDER_PROVIDER)
        .ok_or_else(|| eyre!(LOGIN_PLACEHOLDER_UNAVAILABLE))?;
    Model::from_spec(provider.default_model)
        .with_context(|| format!("{LOGIN_PLACEHOLDER_INVALID}: {}", provider.default_model))
}

fn config_or_fallback<T>(
    loaded: Result<T>,
    fallback: Option<T>,
    warnings: &mut Vec<String>,
) -> Result<T> {
    match (loaded, fallback) {
        (Ok(config), _) => Ok(config),
        (Err(e), Some(last_good)) => {
            warnings.push(format!("{CONFIG_FALLBACK_WARNING}: {e:#}"));
            Ok(last_good)
        }
        (Err(e), None) => Err(e),
    }
}

/// The one construction path for a generation: first startup passes
/// `fallback: None` (fail-fast); `/reload` passes the last-good config and
/// model so a broken config reopens the UI with a warning instead of exiting.
fn build_stack(
    cli: &Cli,
    cwd: &Path,
    storage: &StateDir,
    fallback: Option<StackFallback>,
    remote: bool,
) -> Result<(Stack, Vec<String>)> {
    let mut warnings = Vec::new();

    let mut plugin_host = super::cli_plugin_host(cli, Arc::clone(ToolRegistry::global_arc()))?;

    let (fallback_config, fallback_model) = match fallback {
        Some((config, model, profiles, selected)) => {
            (Some((config, profiles, selected)), Some(model))
        }
        None => (None, None),
    };
    let reloading = fallback_model.is_some();
    let loaded = load_config(&plugin_host, cli, cwd, remote).and_then(|config| {
        let prompt_profiles = Arc::new(PromptProfileCatalog::discover_user());
        let selected_name = cli
            .system_prompt_profile
            .as_deref()
            .or(config.agent.system_prompt_profile.as_deref());
        let default_prompt_profile = if cli.is_sdk_mode() {
            None
        } else {
            prompt_profiles
                .resolve(selected_name)
                .context("resolve system prompt profile")?
        };
        Ok((config, prompt_profiles, default_prompt_profile))
    });
    let (config, prompt_profiles, default_prompt_profile) =
        config_or_fallback(loaded, fallback_config, &mut warnings)?;
    let notices = super::settings_notices(cli, cwd, remote)
        .into_iter()
        .chain(super::auto_notice(&config));
    if cli.print {
        for notice in notices {
            eprintln!("warning: {notice}");
        }
    } else {
        warnings.extend(notices);
    }
    super::configure_native_tools(&config.agent);
    super::install_native_permission_rules(&plugin_host.plugin_rules(), cwd);

    if let Err(e) = plugin_host.load_production_builtins(&config.plugins) {
        let e = color_eyre::eyre::Report::from(e).wrap_err("load builtin plugins");
        if reloading {
            warnings.push(format!("{e:#}"));
        } else {
            return Err(e);
        }
    }

    let commands = discover_commands(cli.no_commands || remote);

    // A fresh session opens in plan; a resumed one overrides this with the model
    // it stored.
    let model_result = setup::resolve_model(
        cli.model.as_deref(),
        &config.provider,
        storage,
        StoredMode::Plan,
    );
    let (model, needs_login) = match (model_result, fallback_model) {
        (Ok(m), _) => (m, false),
        (Err(e), Some(last_model)) => {
            warnings.push(format!("{MODEL_FALLBACK_WARNING}: {e:#}"));
            (last_model, false)
        }
        (Err(_), None) if !cli.print => (login_placeholder_model()?, true),
        (Err(e), None) => return Err(e),
    };

    Ok((
        Stack {
            plugin_host,
            config,
            commands,
            model,
            needs_login,
            prompt_profiles,
            default_prompt_profile,
        },
        warnings,
    ))
}

fn resolve_session(
    continue_session: bool,
    session_id: Option<&str>,
    model: &str,
    cwd: &str,
    storage: &StateDir,
) -> Result<SessionTab> {
    if let Some(raw) = session_id {
        let id: CaudraId = raw
            .parse()
            .map_err(|e| color_eyre::eyre::eyre!("invalid session id {raw:?}: {e}"))?;
        let lease = Arc::new(SessionLease::acquire(storage, id)?);
        let (session, cursor) = setup::load_session_with_cursor(id, storage)?;
        StoredWorkspaceBinding::validate_resume(session.workspace_binding(), None)?;
        setup::report_session_start(caudra_otel::emit::START_RESUME, Some(session.id));
        return Ok(SessionTab {
            session,
            lease,
            cursor: Some(cursor),
        });
    }
    if continue_session {
        if let Some(summary) = AppSession::list(cwd, storage)?.into_iter().next() {
            let lease = Arc::new(SessionLease::acquire(storage, summary.id)?);
            let (session, cursor) = setup::load_session_with_cursor(summary.id, storage)?;
            StoredWorkspaceBinding::validate_resume(session.workspace_binding(), None)?;
            setup::report_session_start(caudra_otel::emit::START_CONTINUE, Some(session.id));
            return Ok(SessionTab {
                session,
                lease,
                cursor: Some(cursor),
            });
        }
        tracing::info!("no previous session found for this directory, starting new");
    }
    let session = AppSession::new(model, cwd);
    let lease = Arc::new(SessionLease::acquire(storage, session.id)?);
    setup::report_session_start(caudra_otel::emit::START_FRESH, Some(session.id));
    Ok(SessionTab {
        session,
        lease,
        cursor: None,
    })
}

struct ResolvedSessions {
    tabs: Vec<SessionTab>,
    focused: usize,
    warnings: Vec<String>,
}

/// A new session for a UI generation left without a tab.
fn fresh_tab(model_spec: &str, cwd: &Path, storage: &StateDir) -> Result<SessionTab> {
    let session = AppSession::new(model_spec, &cwd.to_string_lossy());
    let lease = Arc::new(SessionLease::acquire(storage, session.id)?);
    setup::report_session_start(caudra_otel::emit::START_FRESH, Some(session.id));
    Ok(SessionTab {
        session,
        lease,
        cursor: None,
    })
}

fn local_runtime_cwd(
    tabs: &[SessionTab],
    focused: usize,
    current: io::Result<PathBuf>,
) -> Result<PathBuf> {
    match tabs.get(focused).or_else(|| tabs.first()) {
        Some(tab) => Ok(PathBuf::from(&tab.session.cwd)),
        None => current.context("resolve current working directory for reload"),
    }
}

/// `--session` resumes a local session where it works, whichever directory it
/// was typed in, so config, plugins and MCP servers come from its project. A
/// Herdr restore types it into whatever directory the pane's shell was left in.
/// A session left in a removed worktree is moved back first, which is also
/// what the returned messages report.
fn resumed_session_cwd(cli: &Cli, storage: &StateDir) -> Result<(Option<PathBuf>, Vec<String>)> {
    if cli.is_sdk_mode() || cli.print || cli.ephemeral || cli.workcell.is_set() {
        return Ok((None, Vec::new()));
    }
    let Some(id) = cli
        .session
        .as_deref()
        .and_then(|raw| raw.parse::<CaudraId>().ok())
    else {
        return Ok((None, Vec::new()));
    };
    let moved = caudra_storage::worktrees::reconcile_session(storage, id)
        .map(|moved| moved.iter().map(ToString::to_string).collect())
        .unwrap_or_else(|error| {
            tracing::warn!(%error, session_id = %id, "could not move the resumed session back out of a removed worktree");
            Vec::new()
        });
    let cwd = SessionDatabase::open_state(storage)?.local_session_cwd(id)?;
    Ok((cwd.map(PathBuf::from).filter(|path| path.is_dir()), moved))
}

fn project_env_present(cwd: &Path) -> bool {
    match fs::symlink_metadata(cwd.join(PROJECT_ENV_PATH)) {
        Ok(_) => true,
        Err(error) => error.kind() != io::ErrorKind::NotFound,
    }
}

/// Holds `name` for the session before the UI starts, so a conflict stops
/// startup instead of leaving scripts to address an unnamed session.
fn reserve_launch_name(host: &PeerHost, session: &mut AppSession, name: &str) -> Result<()> {
    smol::block_on(host.reserve_handle(session.id, name)).map_err(|error| eyre!(error))?;
    session.meta.peer_controls.get_or_insert_default().handle = Some(name.to_owned());
    if setup::session_history_head(session).is_none() {
        session.set_title_if_auto(name.to_owned());
    }
    Ok(())
}

/// Adds the launch subscriptions to the ones the session already keeps, so a
/// resumed session never loses a topic it subscribed to before.
fn subscribe_launch(session: &mut AppSession, topics: &[String], broadcasts: bool) -> Result<()> {
    if topics.is_empty() && !broadcasts {
        return Ok(());
    }
    let current = session
        .meta
        .peer_controls
        .as_ref()
        .map(|controls| controls.topics.as_slice())
        .unwrap_or_default();
    let topics = add_patterns(current, topics).map_err(|error| eyre!(error))?;
    let controls = session.meta.peer_controls.get_or_insert_default();
    controls.topics = topics;
    controls.broadcasts |= broadcasts;
    Ok(())
}

/// Adds the launch groups to the ones the session already belongs to, once
/// each exists, so a session never waits on a group nobody created.
fn join_launch_groups(host: &PeerHost, session: &mut AppSession, groups: &[String]) -> Result<()> {
    if groups.is_empty() {
        return Ok(());
    }
    smol::block_on(host.check_groups(groups)).map_err(|error| eyre!(error))?;
    let current = session
        .meta
        .peer_controls
        .as_ref()
        .map(|controls| controls.groups.as_slice())
        .unwrap_or_default();
    let groups = add_groups(current, groups)?;
    session.meta.peer_controls.get_or_insert_default().groups = groups;
    Ok(())
}

/// `current` followed by each group of `added` it lacks.
fn add_groups(current: &[String], added: &[String]) -> Result<Vec<String>> {
    let mut groups = current.to_vec();
    for group in added {
        if !groups.contains(group) {
            groups.push(group.clone());
        }
    }
    if groups.len() > MAX_MEMBERSHIPS {
        bail!(TOO_MANY_MEMBERSHIPS);
    }
    Ok(groups)
}

fn relocation_moves_live_tabs(tabs: &[SessionTab], request: &SessionRelocation) -> bool {
    tabs.iter().any(|tab| {
        request
            .sessions
            .iter()
            .any(|expected| expected.id == tab.session.id && expected.cwd != request.destination)
    })
}

fn relocation_requires_env_restart(
    tabs: &[SessionTab],
    request: &SessionRelocation,
    startup_cwd: &Path,
    startup_project_env: bool,
) -> bool {
    relocation_moves_live_tabs(tabs, request)
        && (startup_project_env
            || project_env_present(startup_cwd)
            || project_env_present(Path::new(&request.destination))
            || request
                .sessions
                .iter()
                .any(|source| project_env_present(Path::new(&source.cwd))))
}

fn rebase_relocation_versions(
    request: &mut SessionRelocation,
    tabs: &[SessionTab],
    inventory: &[SessionLocation],
) -> Result<()> {
    for expected in &mut request.sessions {
        let Some(tab) = tabs.iter().find(|tab| tab.session.id == expected.id) else {
            continue;
        };
        let committed = tab.session.clone().persisted_write_version();
        let stored = inventory.iter().find(|stored| stored.id == expected.id);
        if tab.lease.id() != expected.id
            || tab.session.cwd != expected.cwd
            || !stored.is_some_and(|stored| {
                stored.cwd == expected.cwd
                    && Some(stored.write_version) == committed
                    && stored.write_version >= expected.write_version
            })
        {
            bail!("{RELOCATION_VERSION_CHANGED}: {}", expected.id);
        }
        expected.write_version = committed.ok_or_else(|| eyre!(RELOCATION_VERSION_CHANGED))?;
    }
    Ok(())
}

fn relocate_stopped_sessions(
    tabs: Vec<SessionTab>,
    focused: usize,
    mut relocation: SessionRelocationHandoff,
    storage: &StateDir,
    original_cwd: &Path,
    mut change_cwd: impl FnMut(&Path) -> io::Result<()>,
) -> Result<(ResolvedSessions, Option<String>)> {
    let focused_id = tabs.get(focused).map(|tab| tab.session.id);
    let open: Vec<_> = tabs
        .iter()
        .filter(|tab| {
            relocation.request.sessions.iter().any(|expected| {
                expected.id == tab.session.id && expected.cwd != relocation.request.destination
            })
        })
        .map(|tab| tab.session.id)
        .collect();
    let destination_tabs = (!open.is_empty()).then(|| WorkspaceTabs {
        focused: focused_id
            .filter(|id| open.contains(id))
            .or_else(|| open.first().copied()),
        open,
    });
    let mut installed_cwd = false;
    let result = (|| -> Result<SessionRelocationResult> {
        if storage.is_ephemeral()
            || tabs.iter().any(|tab| {
                tab.session
                    .workspace_binding()
                    .is_some_and(|binding| !binding.is_local())
            })
        {
            bail!("Relocation requires persistent local sessions");
        }
        let leased: HashSet<_> = tabs
            .iter()
            .filter(|tab| tab.session.id == tab.lease.id())
            .map(|tab| tab.lease.id())
            .chain(relocation.leases.iter().map(|lease| lease.id()))
            .collect();
        for expected in &relocation.request.sessions {
            if !leased.contains(&expected.id) {
                bail!("Session relocation lease is missing: {}", expected.id);
            }
        }
        let mut database = SessionDatabase::open_state(storage)?;
        let inventory = database.local_session_locations()?;
        rebase_relocation_versions(&mut relocation.request, &tabs, &inventory)?;
        let destination = Path::new(&relocation.request.destination);
        if fs::canonicalize(destination).context("revalidate destination directory")? != destination
        {
            bail!(RELOCATION_DESTINATION_CHANGED);
        }
        fs::read_dir(destination).context("access destination directory")?;
        if destination_tabs.is_some() {
            change_cwd(destination).context("install destination working directory")?;
            installed_cwd = true;
        }
        if let Some((id, cwd)) = &relocation.donor
            && !database
                .local_session_locations()?
                .iter()
                .any(|entry| entry.id == *id && entry.cwd == *cwd)
        {
            bail!(RELOCATION_DONOR_CHANGED);
        }
        database
            .relocate_sessions_with_tabs(&relocation.request, &destination_tabs)
            .context("commit session relocation")
    })();
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            if installed_cwd {
                change_cwd(original_cwd).wrap_err_with(|| {
                    format!(
                        "{RELOCATION_ABORTED}: {error:#}. {RELOCATION_ROLLBACK_FAILED}: {}",
                        original_cwd.display()
                    )
                })?;
            }
            return Ok((
                ResolvedSessions {
                    tabs,
                    focused,
                    warnings: vec![format!("{RELOCATION_ABORTED}: {error:#}")],
                },
                None,
            ));
        }
    };
    if result.sessions_moved == 0 {
        return Ok((
            ResolvedSessions {
                tabs,
                focused,
                warnings: vec![format!(
                    "No sessions moved; the selection is empty or already at the destination. {RELOCATION_USAGE_UNCHANGED}"
                )],
            },
            None,
        ));
    }
    let usage = match result.project_usage {
        Some(usage) if usage.buckets_moved == 0 && usage.tool_buckets_moved == 0 => {
            RELOCATION_USAGE_EMPTY.into()
        }
        Some(usage) => format!(
            "Historical project usage migrated: {} spend bucket(s) and {} tool bucket(s) moved ({} and {} merged into existing destination buckets)",
            usage.buckets_moved,
            usage.tool_buckets_moved,
            usage.buckets_merged,
            usage.tool_buckets_merged
        ),
        None => RELOCATION_USAGE_UNCHANGED.into(),
    };
    let detached = if relocation.request.keep_plan {
        RELOCATION_APPROVALS_DETACHED
    } else {
        RELOCATION_PLANS_DETACHED
    };
    let committed = format!(
        "Relocation committed: moved {} session(s) to {}. {usage}. {detached}. {RELOCATION_FILES_UNMOVED}",
        result.sessions_moved, relocation.request.destination
    );
    let mut retained = Vec::new();
    for tab in tabs {
        let affected = destination_tabs
            .as_ref()
            .is_some_and(|tabs| tabs.open.contains(&tab.session.id));
        if affected {
            let session = setup::load_session(tab.session.id, storage).wrap_err_with(|| {
                format!("{committed}. Could not reopen {}; reopen the committed session at the destination", tab.session.id)
            })?;
            retained.push(SessionTab {
                session,
                lease: tab.lease,
                cursor: None,
            });
        } else if !installed_cwd {
            retained.push(tab);
        }
    }
    let focused = retained
        .iter()
        .position(|tab| Some(tab.session.id) == focused_id)
        .unwrap_or(0);
    Ok((
        ResolvedSessions {
            tabs: retained,
            focused,
            warnings: vec![committed.clone()],
        },
        Some(committed),
    ))
}

fn restore_warning(warnings: &mut Vec<String>, message: String) {
    tracing::warn!(warning = %message, "workspace tab not restored");
    warnings.push(message);
}

fn restore_workspace_tabs(
    stored: WorkspaceTabs,
    cwd: &Path,
    storage: &StateDir,
) -> ResolvedSessions {
    let mut warnings = Vec::new();
    let facts = match SessionDatabase::open_state(storage)
        .and_then(|database| database.session_facts(None))
    {
        Ok(facts) => facts
            .into_iter()
            .map(|facts| (facts.id, facts.cwd))
            .collect::<HashMap<_, _>>(),
        Err(error) => {
            restore_warning(
                &mut warnings,
                format!("Could not inspect stored workspace tabs: {error}"),
            );
            return ResolvedSessions {
                tabs: Vec::new(),
                focused: 0,
                warnings,
            };
        }
    };
    let canonical_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut seen = HashSet::new();
    let mut tabs = Vec::new();

    for id in stored.open {
        if !seen.insert(id) {
            restore_warning(
                &mut warnings,
                format!("Stored workspace tab {id} is duplicated"),
            );
            continue;
        }
        let Some(_) = facts.get(&id) else {
            restore_warning(
                &mut warnings,
                format!("Stored workspace tab {id} no longer exists"),
            );
            continue;
        };
        let opened = (|| -> Result<SessionTab> {
            let lease = Arc::new(SessionLease::acquire(storage, id)?);
            let session = caudra_agent::load_stored_session(id, storage)?;
            StoredWorkspaceBinding::validate_resume(session.workspace_binding(), None)?;
            Ok(SessionTab {
                session,
                lease,
                cursor: None,
            })
        })();
        let tab = match opened {
            Ok(tab) => tab,
            Err(error) => {
                restore_warning(
                    &mut warnings,
                    format!("Stored workspace tab {id} is unavailable: {error}"),
                );
                continue;
            }
        };
        let stored_cwd = match Path::new(&tab.session.cwd).canonicalize() {
            Ok(stored_cwd) => stored_cwd,
            Err(error) => {
                restore_warning(
                    &mut warnings,
                    format!("Stored workspace tab {id} has a stale working directory: {error}"),
                );
                continue;
            }
        };
        if stored_cwd != canonical_cwd {
            restore_warning(
                &mut warnings,
                format!(
                    "Stored workspace tab {id} belongs to {}, not {}",
                    stored_cwd.display(),
                    canonical_cwd.display()
                ),
            );
            continue;
        }
        if let Err(error) = caudra_storage::sessions::mark_opened(id, storage) {
            restore_warning(
                &mut warnings,
                format!("Stored workspace tab {id} could not be opened: {error}"),
            );
            continue;
        }
        setup::report_session_start(caudra_otel::emit::START_CONTINUE, Some(tab.session.id));
        tabs.push(tab);
    }

    let focused = stored
        .focused
        .and_then(|id| tabs.iter().position(|tab| tab.session.id == id));
    if let Some(id) = stored.focused
        && focused.is_none()
        && !tabs.is_empty()
    {
        restore_warning(
            &mut warnings,
            format!("Stored focused workspace tab {id} could not be restored"),
        );
    }
    ResolvedSessions {
        tabs,
        focused: focused.unwrap_or(0),
        warnings,
    }
}

fn resolve_sessions(
    continue_session: bool,
    session_id: Option<&str>,
    model: &str,
    cwd: &Path,
    storage: &StateDir,
    workspace_binding: Option<&caudra_storage::workspace_binding::StoredWorkspaceBinding>,
) -> Result<ResolvedSessions> {
    if let Some(binding) = workspace_binding {
        return resolve_remote_sessions(
            continue_session,
            session_id,
            model,
            cwd.to_string_lossy().as_ref(),
            storage,
            binding,
        );
    }
    let cwd_str = cwd.to_string_lossy();
    if continue_session && session_id.is_none() {
        let mut warnings = Vec::new();
        if !storage.is_ephemeral() {
            match read_workspace_tabs(storage, cwd) {
                Ok(Some(stored)) => {
                    let restored = restore_workspace_tabs(stored, cwd, storage);
                    warnings.extend(restored.warnings);
                    if !restored.tabs.is_empty() {
                        return Ok(ResolvedSessions {
                            warnings,
                            ..restored
                        });
                    }
                }
                Ok(None) => {}
                Err(error) => restore_warning(
                    &mut warnings,
                    format!("Could not read stored workspace tabs: {error}"),
                ),
            }
        }
        let tab = resolve_session(true, None, model, &cwd_str, storage)?;
        return Ok(ResolvedSessions {
            tabs: vec![tab],
            focused: 0,
            warnings,
        });
    }
    Ok(ResolvedSessions {
        tabs: vec![resolve_session(
            false, session_id, model, &cwd_str, storage,
        )?],
        focused: 0,
        warnings: Vec::new(),
    })
}

fn resolve_remote_sessions(
    continue_session: bool,
    session_id: Option<&str>,
    model: &str,
    cwd: &str,
    storage: &StateDir,
    binding: &caudra_storage::workspace_binding::StoredWorkspaceBinding,
) -> Result<ResolvedSessions> {
    let resume_id = if let Some(raw) = session_id {
        Some(
            raw.parse::<CaudraId>()
                .map_err(|error| color_eyre::eyre::eyre!("invalid session id {raw:?}: {error}"))?,
        )
    } else if continue_session {
        SessionDatabase::open_state(storage)?
            .list_for_workspace_identity(binding)?
            .first()
            .map(|session| session.id)
    } else {
        None
    };
    let tab = if let Some(id) = resume_id {
        let lease = Arc::new(SessionLease::acquire(storage, id)?);
        let session = setup::load_session(id, storage)?;
        StoredWorkspaceBinding::validate_resume_identity(
            session.workspace_binding(),
            Some(binding),
        )?;
        setup::report_session_start(caudra_otel::emit::START_RESUME, Some(session.id));
        SessionTab {
            session,
            lease,
            cursor: None,
        }
    } else {
        let session = AppSession::new_with_workspace(model, cwd, binding.clone());
        let lease = Arc::new(SessionLease::acquire(storage, session.id)?);
        setup::report_session_start(caudra_otel::emit::START_FRESH, Some(session.id));
        SessionTab {
            session,
            lease,
            cursor: None,
        }
    };
    Ok(ResolvedSessions {
        tabs: vec![tab],
        focused: 0,
        warnings: Vec::new(),
    })
}

fn resolve_prompt(flag: Option<String>) -> Result<Option<String>> {
    let piped = if io::stdin().is_terminal() {
        None
    } else {
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf).context("read stdin")?;
        Some(buf)
    };
    Ok(merge_prompt(flag, piped))
}

fn merge_prompt(flag: Option<String>, piped: Option<String>) -> Option<String> {
    let merged = match (flag, piped) {
        (Some(flag), Some(piped)) if !piped.trim().is_empty() => {
            format!("{}\n\n{}", flag.trim_end(), piped.trim_end())
        }
        (Some(text), _) | (None, Some(text)) => text,
        (None, None) => return None,
    };
    (!merged.trim().is_empty()).then_some(merged)
}

fn sandbox_phase<T>(name: &str, phase: &str, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let _progress = SandboxProgress::start_named(format!("Sandbox '{name}': {phase}"));
    operation().wrap_err_with(|| format!(
        "Sandbox '{name}': {phase} failed. Sessions saved; active TUI ended. Inspect/Reconcile the sandbox before explicitly reopening or retrying. No automatic retry, acknowledgement or local fallback."
    ))
}

fn after_runtime_release<T, U>(
    runtime: Arc<T>,
    operation: impl FnOnce() -> Result<U>,
) -> Result<U> {
    let runtime = Arc::try_unwrap(runtime).map_err(|_| {
        eyre!("Runtime still has owners; no control sent. Inspect/Reconcile before reopening explicitly")
    })?;
    drop(runtime);
    operation()
}

pub fn run(mut cli: Cli, tightened: Vec<PathBuf>) -> Result<ExitCode> {
    let permission_mode_override = cli.permission_mode_override();
    // Every phase up to `init_logging` runs without a subscriber, so its cost is
    // invisible unless it is measured here and reported once the sink exists.
    let started = Instant::now();
    let mut phase_start = started;
    let mut lap = || {
        let elapsed = phase_start.elapsed().as_millis() as u64;
        phase_start = Instant::now();
        elapsed
    };
    let persistent_storage = StateDir::resolve().context("resolve data directory")?;
    let state_dir_ms = lap();
    caudra_providers::model_registry::load_from_storage(&persistent_storage)
        .context("load model purpose bindings")?;
    let model_registry_ms = lap();

    let mut cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    let (resumed_cwd, mut moved_back) = resumed_session_cwd(&cli, &persistent_storage)?;
    if let Some(session_cwd) = resumed_cwd {
        env::set_current_dir(&session_cwd).wrap_err_with(|| {
            format!(
                "enter the resumed session's directory {}",
                session_cwd.display()
            )
        })?;
        cwd = session_cwd;
    }
    let startup_project_env = project_env_present(&cwd);

    let resumed_sandbox =
        super::sandbox::recover_session_source(&mut cli, &persistent_storage, &cwd)?;

    let sandbox_progress = SandboxProgress::start(&cli);
    let mut workcell_runtime = Arc::new(super::workcell_runtime::WorkcellRuntime::initialize(
        &cli.workcell,
        &cwd,
        &persistent_storage,
        ToolRegistry::global(),
        cli.startup.features,
    )?);
    if let Some(binding) = &resumed_sandbox {
        StoredWorkspaceBinding::validate_resume_identity(
            Some(binding),
            workcell_runtime.stored_binding(),
        )?;
    }
    // The runtime refused to start unless this parsed, and the footer falls back
    // to the binding's record id rather than claiming a name it does not have.
    let mut sandbox_name = cli
        .workcell
        .sandbox
        .as_deref()
        .and_then(|name| SandboxName::parse(name).ok());
    drop(sandbox_progress);
    let mut initial_seed = None;
    let workcell_runtime_ms = lap();

    let (mut stack, stack_warnings) = build_stack(
        &cli,
        &cwd,
        &persistent_storage,
        None,
        workcell_runtime.is_remote(),
    )?;
    let build_stack_ms = lap();
    let ephemeral = cli.ephemeral || stack.config.storage.ephemeral;
    let (storage, _ephemeral_root) = super::run_storage(persistent_storage, ephemeral)?;
    let run_storage_ms = lap();

    let _logging = setup::init_logging(&stack.config.storage);
    let init_logging_ms = lap();
    setup::apply_storage_limits(&stack.config.storage);
    setup::register_change_stores();
    setup::init_telemetry(&stack.config.telemetry);
    setup::install_panic_log_hook();
    setup::warn_ignored_provider_fields();
    setup::report_startup(setup::MODE_TUI, &stack.model, &cwd);
    if !workcell_runtime.is_remote() {
        super::adopt_checkout_state(&storage, &cwd);
        moved_back.extend(super::reconcile_worktrees(&storage, &cwd));
    }

    let seed_permission_mode = permission_mode_override
        .clone()
        .unwrap_or_else(|| super::permission_mode_seed(&stack.config));

    if cli.is_sdk_mode() {
        let fast = stack.config.always_fast && stack.model.supports_fast();
        let thinking = stack
            .config
            .always_thinking
            .clone()
            .map(caudra_providers::ThinkingConfig::from)
            .unwrap_or_default();
        let prompt_slots = stack
            .plugin_host
            .event_handle()
            .collect_prompt_slots(&stack.config.agent);
        let timeouts = stack.timeouts();
        let features = cli.startup.features;
        crate::sdk_mode::run(crate::sdk_mode::SdkParams {
            cli,
            model: stack.model,
            config: stack.config.agent,
            permissions_config: stack.config.permissions,
            decisions_config: stack.config.decisions,
            seed_permission_mode: Some(seed_permission_mode),
            snapshots: stack.config.storage.snapshots,
            timeouts,
            prompt_slots,
            prompt_profiles: stack.prompt_profiles,
            fast,
            thinking,
            model_policy: Arc::new(stack.config.provider.model_policy.clone()),
            plugin_rules: stack.plugin_host.plugin_rules(),
            workspace_binding: workcell_runtime.stored_binding().cloned(),
            workspace_session: workcell_runtime.workspace_session().cloned(),
            remote_project_context: workcell_runtime.remote_project_context().cloned(),
            local_documents: workcell_runtime.local_documents().cloned(),
            remote_environment: workcell_runtime.is_remote().then(|| {
                caudra_agent::headless::RemoteEnvironment {
                    cwd: workcell_runtime.display().cwd.clone(),
                    platform: workcell_runtime.display().platform.clone(),
                }
            }),
            automations: stack.config.automations,
        })
        .context("run sdk mode")?;
        super::sandbox::remember_session_source(
            &storage,
            &cwd,
            workcell_runtime.stored_binding(),
            features,
        )?;
        return Ok(ExitCode::SUCCESS);
    }

    // Past the SDK branch stdin is no longer a protocol channel, so both
    // remaining paths can consume it as prompt text.
    let mut initial_prompt = resolve_prompt(cli.prompt.take())?;

    if cli.print {
        let fast = stack.config.always_fast && stack.model.supports_fast();
        let thinking = stack
            .config
            .always_thinking
            .clone()
            .map(caudra_providers::ThinkingConfig::from)
            .unwrap_or_default();
        let timeouts = stack.timeouts();
        return crate::print::run(
            &stack.model,
            initial_prompt,
            cli.images,
            cli.output_format,
            cli.verbose,
            stack.config.agent,
            stack.config.permissions,
            stack.config.decisions,
            Some(seed_permission_mode),
            stack.config.storage.snapshots,
            timeouts,
            stack.plugin_host.event_handle(),
            fast,
            thinking,
            stack.default_prompt_profile,
            Arc::clone(&stack.prompt_profiles),
            Arc::new(stack.config.provider.model_policy.clone()),
            stack.plugin_host.plugin_rules(),
            workcell_runtime
                .is_remote()
                .then(|| caudra_agent::headless::RemoteEnvironment {
                    cwd: workcell_runtime.display().cwd.clone(),
                    platform: workcell_runtime.display().platform.clone(),
                }),
            workcell_runtime.workspace_session().cloned(),
            workcell_runtime.remote_project_context().cloned(),
            workcell_runtime.local_documents().cloned(),
        )
        .context("run print mode");
    }

    let session_cwd = workcell_runtime
        .is_remote()
        .then_some(workcell_runtime.display().cwd.as_str());
    let resolved = resolve_sessions(
        cli.continue_session,
        cli.session.as_deref(),
        &stack.model.spec(),
        session_cwd.map_or(cwd.as_path(), Path::new),
        &storage,
        workcell_runtime.stored_binding(),
    )?;
    let resolve_sessions_ms = lap();
    super::sandbox::remember_session_source(
        &storage,
        &cwd,
        workcell_runtime.stored_binding(),
        cli.startup.features,
    )?;
    tracing::info!(
        state_dir_ms,
        model_registry_ms,
        workcell_runtime_ms,
        build_stack_ms,
        run_storage_ms,
        init_logging_ms,
        resolve_sessions_ms,
        total_ms = started.elapsed().as_millis() as u64,
        "startup phases"
    );

    let mut tabs = resolved.tabs;
    let mut focused = resolved.focused;
    if let Some(mode) = &permission_mode_override {
        tabs[focused].session.meta.permission_mode = Some(mode.clone());
    }
    let mut warnings = resolved.warnings;
    warnings.extend(stack_warnings);
    warnings.extend(moved_back);
    if !tightened.is_empty() {
        warnings.push(format!(
            "{TIGHTENED_DIRS_WARNING}: {}",
            tightened
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let mut teardown = Teardown::default();
    let mut herdr_reporter = HerdrReporter::from_env();
    crate::progress::uninstall();
    let mut sweeper = RetentionSweeper::spawn(storage.clone(), stack.config.storage);
    let mut committed_relocation: Option<String> = None;
    let sandboxes = cli.startup.features.enabled(Feature::Sandboxes);
    let messaging_flags = cli.requests_messaging();
    let peer_host = if workcell_runtime.is_remote() {
        None
    } else {
        match PeerHost::start(cli.startup.features, &stack.config.agent.messaging) {
            Ok(host) => host.map(Arc::new),
            Err(error) if messaging_flags => {
                return Err(eyre!("{MESSAGING_UNAVAILABLE}: {error}"));
            }
            Err(error) => {
                warnings.push(format!("{MESSAGING_UNAVAILABLE}: {error}"));
                None
            }
        }
    };
    if messaging_flags {
        let host = peer_host
            .as_deref()
            .ok_or_else(|| eyre!(LAUNCH_NEEDS_LOCAL_MESSAGING))?;
        let session = &mut tabs[focused].session;
        if let Some(name) = &cli.name {
            reserve_launch_name(host, session, name)?;
        }
        subscribe_launch(session, &cli.topics, cli.receive_broadcasts)?;
        join_launch_groups(host, session, &cli.groups)?;
    }
    let mut joined_groups = !cli.groups.is_empty();
    let mut launch_automations = std::mem::take(&mut cli.automations);

    loop {
        let runtime_cwd = if workcell_runtime.is_remote() {
            cwd.clone()
        } else {
            local_runtime_cwd(&tabs, focused, env::current_dir())?
        };
        for tab in &mut tabs {
            let session = &mut tab.session;
            if setup::session_history_head(session).is_none() {
                session.meta.fast |= stack.config.always_fast;
                if let Some(thinking) = &stack.config.always_thinking {
                    session.meta.thinking = Some(thinking.clone());
                }
            }
        }
        let focused_tab = &tabs[focused].session;
        let model = if setup::session_history_head(focused_tab).is_none()
            || !stack
                .config
                .provider
                .model_policy
                .allows(&focused_tab.model)
        {
            stack.model.clone()
        } else {
            Model::from_spec(&focused_tab.model).unwrap_or_else(|_| stack.model.clone())
        };

        let permissions = Arc::new(
            caudra_agent::permissions::PermissionManager::new_persistent(
                stack.config.permissions.clone(),
                runtime_cwd.clone(),
                stack.plugin_host.plugin_rules(),
            ),
        );
        permissions.set_seed_mode(
            permission_mode_override
                .clone()
                .unwrap_or_else(|| super::permission_mode_seed(&stack.config)),
        );
        if stack.config.permissions.decision_engine {
            permissions.set_decisions(Some(
                Decisions::new(stack.config.decisions.clone(), &storage)
                    .context("initialize decision engine; check decisions configuration, credentials and question overrides")?,
            ));
        }
        permissions
            .replace_remote_permission_asset(
                workcell_runtime
                    .remote_project_context()
                    .and_then(|context| context.permissions()),
            )
            .context("install initial remote permission policy")?;
        let pattern_suggestions = if stack.config.storage.ephemeral {
            None
        } else {
            PatternSuggestionWorker::spawn(&storage, workcell_runtime.is_remote())
        };
        let permission_authority_factory: caudra_ui::PermissionAuthorityFactory = {
            let runtime = Arc::clone(&workcell_runtime);
            let config = stack.config.agent.clone();
            Arc::new(move |project, mode, model, workspace, profile, session| {
                let tool_filter = ToolFilter::from_config(&config, &model, &[]).for_mode(&mode);
                let registry = ToolRegistry::global();
                let description = DescriptionContext {
                    filter: &tool_filter,
                    audience: ToolAudience::MAIN,
                    workflows_available: config.features.enabled(Feature::Workflows),
                };
                let tool_filter = registry.profile_filter(
                    &description,
                    &profile,
                    mode.has_session_plan(session.plan.as_ref()),
                );
                let context = Arc::new(PermissionEditorContext::new(PermissionEditorRuntime {
                    project,
                    tool_filter: tool_filter.clone(),
                    mode,
                    plan: session.plan,
                    audience: ToolAudience::MAIN,
                    workspace,
                    session_id: Some(session.id),
                    local_documents: session.local_documents,
                })?);
                Ok(PermissionAuthorityBinding {
                    provider: permission_authority_provider(
                        Arc::clone(ToolRegistry::global_arc()),
                        context,
                        runtime.local_host(),
                    ),
                    tool_filter,
                    available: runtime
                        .connection_status()
                        .is_none_or(|status| status == RemoteConnectionStatus::Connected),
                    registry_revision: ToolRegistry::global().authority_snapshot().revision(),
                })
            })
        };
        let change_factory: Option<ChangeServiceFactory> =
            workcell_runtime.local_host().is_some().then(|| {
                let runtime = Arc::clone(&workcell_runtime);
                let state = storage.clone();
                let factory: ChangeServiceFactory = Arc::new(move |cwd: &Path| {
                    runtime
                        .local_host()
                        .map(|host| host.change_service(cwd, &state))
                });
                factory
            });
        let worktrees = Backend::select(&stack.config.worktrees, HerdrEnv::detect());
        let outcome = caudra_ui::run(
            caudra_ui::EventLoopParams {
                model,
                needs_login: stack.needs_login,
                commands: std::mem::take(&mut stack.commands),
                no_commands: cli.no_commands,
                sessions: std::mem::take(&mut tabs),
                focused,
                startup_warnings: std::mem::take(&mut warnings),
                joined_groups: std::mem::take(&mut joined_groups),
                launch_automations: std::mem::take(&mut launch_automations),
                storage: storage.clone(),
                config: stack.config.agent.clone(),
                automations: stack.config.automations.clone(),
                ui_config: stack.config.ui.clone(),
                snapshots: stack.config.storage.snapshots,
                change_factory,
                allow_workspace_recovery: committed_relocation.is_none(),
                input_history_size: stack.config.storage.input_history_size,
                max_log_files: stack.config.storage.max_log_files,
                docs: docs::library,
                permissions,
                pattern_suggestion_loader: pattern_suggestions
                    .as_ref()
                    .map(|worker| Arc::clone(&worker.loader)),
                permission_authority_factory: Some(permission_authority_factory),
                sandbox_connector: sandboxes.then(|| {
                    super::sandbox::connector(
                        storage.clone(),
                        runtime_cwd.clone(),
                        cli.startup.features,
                    )
                }),
                transfer_connector: sandboxes.then(|| {
                    super::sandbox::transfer_connector(storage.clone(), cli.startup.features)
                }),
                initial_seed: initial_seed.take(),
                sandbox_readiness: Some({
                    let runtime = Arc::clone(&workcell_runtime);
                    Arc::new(move || {
                        runtime.connection_status() == Some(RemoteConnectionStatus::Connected)
                    })
                }),
                sandbox_name: sandbox_name.clone(),
                timeouts: stack.timeouts(),
                exit_on_done: cli.exit_on_done,
                lua_command_reader: stack.plugin_host.command_reader(),
                keymap_reader: stack.plugin_host.keymap_reader(),
                hint_reader: stack.plugin_host.hint_reader(),
                ui_action_rx: stack.plugin_host.ui_action_rx(),
                lua_event_handle: stack.plugin_host.event_handle(),
                model_policy: Arc::new(stack.config.provider.model_policy.clone()),
                prompt_profiles: Arc::clone(&stack.prompt_profiles),
                default_prompt_profile: stack.default_prompt_profile.clone(),
                prompt_profile_override: cli.system_prompt_profile.clone(),
                herdr_reporter: herdr_reporter.as_ref().map(HerdrReporter::handle),
                peer_host: peer_host.clone(),
                worktrees: worktrees.clone(),
                workspace_session: workcell_runtime.workspace_session().cloned(),
                remote_project_context: workcell_runtime.remote_project_context().cloned(),
                local_documents: workcell_runtime.local_documents().cloned(),
            },
            initial_prompt.take(),
        );
        drop(pattern_suggestions);
        let outcome = outcome.wrap_err_with(|| match &committed_relocation {
            Some(committed) => format!(
                "{committed}. UI startup failed; reopen the committed sessions at the destination"
            ),
            None => "run UI".into(),
        })?;

        let (reloaded, f, relocation, mut follow_up) = match outcome {
            RunOutcome::SandboxControl {
                tabs: stopped,
                focused: stopped_focus,
                control,
            } => {
                cli.startup.features.require(Feature::Sandboxes)?;
                let reconnect = control.reconnects();
                let name = control.name.clone();
                let action = control.action_label();
                let session_ids: Vec<_> = stopped.iter().map(|tab| tab.session.id).collect();
                sandbox_phase(
                    name.as_str(),
                    "stopping attached runtime (no control sent)",
                    || {
                        stack
                            .plugin_host
                            .shutdown_checked()
                            .context("Source plugins did not stop")?;
                        teardown.join();
                        drop(stopped);
                        drop(stack);
                        ToolRegistry::global().install_stopped_runtime(&ToolRegistry::default());
                        after_runtime_release(workcell_runtime, || Ok(()))
                    },
                )?;
                let record = sandbox_phase(name.as_str(), &format!("{action} control"), || {
                    control.execute(&storage).map_err(|error| eyre!(error))
                })?;
                if !reconnect {
                    eprintln!(
                        "Sandbox '{name}': {action} completed. Active TUI ended; conversations saved and detached. Inspect the sandbox and explicitly resume/attach when ready. No local fallback."
                    );
                    return Ok(ExitCode::SUCCESS);
                }
                let registry = ToolRegistry::default();
                let runtime = sandbox_phase(
                    name.as_str(),
                    "reconnecting Workcell and verifying identity",
                    || {
                        Ok(
                            super::workcell_runtime::WorkcellRuntime::initialize_sandbox_reviewed(
                                &name,
                                false,
                                &cwd,
                                &storage,
                                &registry,
                                Some(&record.revision()?),
                                cli.startup.features,
                            )?,
                        )
                    },
                )?;
                let restored = sandbox_phase(
                    name.as_str(),
                    "restoring saved conversations and validating bindings",
                    || {
                        session_ids
                            .into_iter()
                            .map(|id| {
                                let lease = Arc::new(SessionLease::acquire(&storage, id)?);
                                let session = setup::load_session(id, &storage)?;
                                StoredWorkspaceBinding::validate_resume_identity(
                                    session.workspace_binding(),
                                    runtime.stored_binding(),
                                )?;
                                Ok(SessionTab {
                                    session,
                                    lease,
                                    cursor: None,
                                })
                            })
                            .collect::<Result<Vec<_>>>()
                    },
                )?;
                ToolRegistry::global().install_stopped_runtime(&registry);
                workcell_runtime = Arc::new(runtime);
                sandbox_name = Some(name.clone());
                let (new_stack, new_warnings) =
                    sandbox_phase(name.as_str(), "rebuilding TUI", || {
                        build_stack(&cli, &cwd, &storage, None, true)
                    })?;
                tabs = restored;
                focused = stopped_focus;
                warnings = new_warnings;
                warnings.push(format!("Sandbox {name}: {action} completed; control postconditions and Workcell identity verified. Runtime rebuilt; same saved conversations reopened."));
                stack = new_stack;
                continue;
            }
            RunOutcome::Sandbox {
                tabs: stopped,
                attachment,
            } => {
                cli.startup.features.require(Feature::Sandboxes)?;
                let prepared = attachment.runtime.downcast::<super::sandbox::PreparedSandbox>().map_err(|_| eyre!("Sandbox transition returned an incompatible runtime; source sessions were preserved"))?;
                StoredWorkspaceBinding::validate_resume_identity(
                    Some(&attachment.binding),
                    prepared.runtime.stored_binding(),
                )?;
                stack
                    .plugin_host
                    .shutdown_checked()
                    .context("Sandbox transition refused: source plugins did not stop")?;
                teardown.join();
                ToolRegistry::global().install_stopped_runtime(&prepared.registry);
                drop(stack);
                workcell_runtime = Arc::new(prepared.runtime);
                sandbox_name = Some(attachment.name.clone());
                initial_seed = attachment.initial_seed;
                let (new_stack, new_warnings) = build_stack(&cli, &cwd, &storage, None, true).context("Sandbox runtime is verified; source sessions remain saved. Reopen --sandbox if UI setup failed")?;
                let resolved = resolve_remote_sessions(
                    false,
                    None,
                    &new_stack.model.spec(),
                    &workcell_runtime.display().cwd,
                    &storage,
                    &attachment.binding,
                )?;
                super::sandbox::remember_session_source(
                    &storage,
                    &cwd,
                    Some(&attachment.binding),
                    cli.startup.features,
                )?;
                tabs = resolved.tabs;
                focused = resolved.focused;
                warnings = new_warnings;
                warnings.push(format!("Attached {} in a new session. Source sessions are saved; no history, grants or files were copied. Exit detaches only.", attachment.name));
                stack = new_stack;
                sweeper = RetentionSweeper::spawn(storage.clone(), stack.config.storage);
                committed_relocation = None;
                drop(stopped);
                continue;
            }
            RunOutcome::Exit { summary, code } => {
                if let Some(summary) = summary {
                    let rich = io::stderr().is_terminal() && !cli.exit_on_done;
                    eprint!("{}", exit_report(&summary, rich));
                }
                let started = Instant::now();
                drop(sweeper);
                drop(stack);
                let stack_ms = started.elapsed().as_millis() as u64;
                teardown.join();
                tracing::info!(
                    stack_ms,
                    teardown_ms = started.elapsed().as_millis() as u64 - stack_ms,
                    "plugin host and teardown joined"
                );
                if let Some(reporter) = herdr_reporter.take() {
                    reporter.release();
                }
                // Returning the code instead of exiting here keeps every guard
                // alive to its scope end, including the ephemeral state root
                // whose `Drop` erases the volatile directory.
                return Ok(code);
            }
            RunOutcome::Reload {
                tabs: reloaded,
                focused: f,
            } => (reloaded, f, None, None),
            RunOutcome::Relocate {
                tabs: reloaded,
                focused: f,
                relocation,
            } => (reloaded, f, Some(relocation), None),
            RunOutcome::Worktree {
                tabs: stopped,
                focused: stopped_focus,
                request,
            } => {
                let Executed { handled, notes } =
                    worktree::execute(request, stopped, stopped_focus, &storage, &worktrees);
                warnings.extend(notes);
                match handled {
                    Handled::Continue {
                        tabs: kept,
                        focused: kept_focus,
                    } => {
                        tabs = kept;
                        focused = kept_focus;
                        if tabs.is_empty() {
                            tabs.push(fresh_tab(&stack.model.spec(), &runtime_cwd, &storage)?);
                        }
                        stack.commands =
                            discover_commands(cli.no_commands || workcell_runtime.is_remote());
                        committed_relocation = None;
                        continue;
                    }
                    Handled::Relocate {
                        tabs: moving,
                        focused: moving_focus,
                        handoff,
                        follow_up,
                    } => (moving, moving_focus, Some(handoff), Some(*follow_up)),
                    Handled::Exit => {
                        for warning in &warnings {
                            eprintln!("{warning}");
                        }
                        drop(sweeper);
                        drop(stack);
                        teardown.join();
                        if let Some(reporter) = herdr_reporter.take() {
                            reporter.release();
                        }
                        return Ok(ExitCode::SUCCESS);
                    }
                }
            }
        };
        let started = Instant::now();
        let restart_for_env = relocation.as_ref().is_some_and(|relocation| {
            relocation_requires_env_restart(
                &reloaded,
                &relocation.request,
                &cwd,
                startup_project_env,
            )
        });
        let relocation = match relocation {
            Some(relocation) if !relocation_moves_live_tabs(&reloaded, &relocation.request) => {
                let original_cwd = env::current_dir().unwrap_or_else(|_| runtime_cwd.clone());
                let (resolved, committed) = relocate_stopped_sessions(
                    reloaded,
                    f,
                    relocation,
                    &storage,
                    &original_cwd,
                    |path| env::set_current_dir(path),
                )?;
                tabs = resolved.tabs;
                focused = resolved.focused;
                warnings.extend(match follow_up.take() {
                    Some(follow_up) => follow_up.finish(
                        resolved.warnings,
                        committed.is_some(),
                        &worktrees,
                        &storage,
                    ),
                    None => resolved.warnings,
                });
                stack.commands = discover_commands(cli.no_commands || workcell_runtime.is_remote());
                committed_relocation = None;
                continue;
            }
            relocation => relocation,
        };
        let last_good = (
            stack.config.clone(),
            stack.model.clone(),
            Arc::clone(&stack.prompt_profiles),
            stack.default_prompt_profile.clone(),
        );
        stack.plugin_host.begin_shutdown();
        ToolRegistry::global().clear_lua();
        committed_relocation = None;
        if let Some(relocation) = relocation {
            stack.plugin_host.shutdown_checked().context(
                "Session relocation aborted before changing directories: source plugins did not stop",
            )?;
            teardown.join();
            drop(stack);
            let original_cwd = env::current_dir().unwrap_or_else(|_| runtime_cwd.clone());
            let (resolved, committed) = relocate_stopped_sessions(
                reloaded,
                f,
                relocation,
                &storage,
                &original_cwd,
                |path| env::set_current_dir(path),
            )?;
            tabs = resolved.tabs;
            focused = resolved.focused;
            let worktree_change = follow_up.is_some();
            warnings.extend(match follow_up {
                Some(follow_up) => {
                    follow_up.finish(resolved.warnings, committed.is_some(), &worktrees, &storage)
                }
                None => resolved.warnings,
            });
            committed_relocation = committed;
            if restart_for_env && let Some(committed) = &committed_relocation {
                drop(sweeper);
                teardown.join();
                if let Some(reporter) = herdr_reporter.take() {
                    reporter.release();
                }
                if worktree_change {
                    for warning in &warnings {
                        eprintln!("{warning}");
                    }
                }
                eprintln!("{committed}. {RELOCATION_ENV_RESTART}.");
                return Ok(ExitCode::SUCCESS);
            }
        } else {
            teardown.defer(move || drop(stack));
            tabs = reloaded;
            focused = f;
        }
        let reload_cwd = if workcell_runtime.is_remote() {
            cwd.clone()
        } else {
            let local_cwd = local_runtime_cwd(&tabs, focused, env::current_dir())?;
            super::adopt_checkout_state(&storage, &local_cwd);
            warnings.extend(super::reconcile_worktrees(&storage, &local_cwd));
            local_cwd
        };
        let fallback =
            (committed_relocation.is_none() && reload_cwd == runtime_cwd).then_some(last_good);
        let (new_stack, new_warnings) = build_stack(
            &cli,
            &reload_cwd,
            &storage,
            fallback,
            workcell_runtime.is_remote(),
        )
        .wrap_err_with(|| match &committed_relocation {
            Some(committed) => format!("{committed}. Runtime initialization failed; reopen the committed sessions at the destination"),
            None => "rebuild runtime".into(),
        })?;
        if tabs.is_empty() {
            tabs.push(fresh_tab(&new_stack.model.spec(), &reload_cwd, &storage)?);
        }
        sweeper = RetentionSweeper::spawn(storage.clone(), new_stack.config.storage);
        stack = new_stack;
        warnings.extend(new_warnings);
        focused = focused.min(tabs.len() - 1);
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            tabs = tabs.len(),
            relocated = committed_relocation.is_some(),
            "rebuilt plugins and config"
        );
    }
}

/// A redirected stderr and `--exit-on-done` both belong to a script, which
/// wants the one line it can act on rather than the block.
fn exit_report(summary: &ExitSummary, rich: bool) -> String {
    if rich {
        summary.banner()
    } else {
        summary.resume_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_agent::permissions::pattern_recognition::{
        CandidateEvidence, InvocationOutcome, ObservationProvenance, SupportCount,
    };
    use caudra_config::RawConfig;
    use caudra_providers::{HistoryItem, HistoryItemKind};
    use caudra_storage::permission_patterns::{
        ArgumentRole, PATTERN_SCHEMA_VERSION, PatternContext, PatternDefinition, PatternToken,
        SlotCombinations,
    };
    use caudra_storage::sessions::{LedgerEntry, PermissionMode, StoredTokenUsage};
    use caudra_storage::state::write_workspace_tabs;
    use caudra_storage::usage_ledger::{BUCKET_SECONDS, LedgerPurpose};
    use caudra_workcell::{PatternObligationCount, PatternOmissionReason};
    use color_eyre::eyre::eyre;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::slice;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use test_case::test_case;

    const TEST_MODEL: &str = "test/model";
    const TEST_CWD: &str = "/tmp";
    const PATTERN_TEST_WAIT: Duration = Duration::from_secs(10);
    const PATTERN_TEST_SOURCE: &str = "history-test";
    const PATTERN_TEST_ANALYSIS: &str = "test-analysis/v1";
    const PATTERN_TEST_COMMAND: &str = "fixture-command";
    const PATTERN_FIRST_COMMAND: &str = "cargo check -p alpha";
    const PATTERN_SECOND_COMMAND: &str = "cargo check -p beta";
    const EXIT_RUN_TIME: Duration = Duration::from_secs(90);
    const SCRIPTABLE: &str = "a redirected stderr gets one line a script can act on";
    const BLOCK: &str = "a terminal gets the full block, not the fallback line";
    const DUPLICATE_TAB_WARNING: &str = "is duplicated";
    const MISSING_TAB_WARNING: &str = "no longer exists";
    const WRONG_CWD_WARNING: &str = "belongs to";
    const INJECTED_CWD_FAILURE: &str = "injected working directory failure";
    const INJECTED_CONFIG_FAILURE: &str = "injected destination config failure";
    const SANDBOX_TEST_NAME: &str = "saved-dev";
    const SANDBOX_TEST_PHASE: &str = "reconnecting Workcell and verifying identity";
    const SANDBOX_TEST_FAILURE: &str = "injected sandbox failure";
    const RELOCATION_COMMITTED: &str = "Relocation committed";
    const RELOCATION_USAGE_MIGRATED: &str = "Historical project usage migrated: 2 spend bucket(s) and 0 tool bucket(s) moved (1 and 0 merged into existing destination buckets)";
    const RELOCATION_TEST_USAGE: StoredTokenUsage = StoredTokenUsage {
        input: 11,
        output: 7,
        cache_creation: 5,
        cache_read: 3,
        cost: Some(0.25),
        subscription_cost: None,
    };

    #[test_case(false; "released_before_operation")]
    #[test_case(true; "retained_owner_blocks_operation")]
    fn sandbox_runtime_handoff(retain_owner: bool) {
        struct Runtime(Arc<AtomicBool>);
        impl Drop for Runtime {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let released = Arc::new(AtomicBool::new(false));
        let runtime = Arc::new(Runtime(Arc::clone(&released)));
        let owner = retain_owner.then(|| Arc::clone(&runtime));
        let mut called = false;
        let result = after_runtime_release(runtime, || {
            assert!(released.load(Ordering::SeqCst));
            called = true;
            Ok(())
        });
        assert_eq!(result.is_ok(), !retain_owner);
        assert_eq!(called, !retain_owner);
        assert_eq!(released.load(Ordering::SeqCst), !retain_owner);
        drop(owner);
    }

    #[test_case(false; "success_once")]
    #[test_case(true; "failure_without_retry")]
    fn sandbox_phase_preserves_result(fail: bool) {
        let mut calls = 0;
        let result = sandbox_phase(SANDBOX_TEST_NAME, SANDBOX_TEST_PHASE, || {
            calls += 1;
            if fail {
                bail!(SANDBOX_TEST_FAILURE);
            }
            Ok(())
        });
        assert_eq!(calls, 1);
        assert_eq!(result.is_err(), fail);
        if let Err(error) = result {
            let report = format!("{error:#}");
            assert!(report.contains(SANDBOX_TEST_NAME));
            assert!(report.contains(SANDBOX_TEST_PHASE));
            assert!(report.contains(SANDBOX_TEST_FAILURE));
        }
    }

    fn suggestion_candidate(project: &Path) -> PatternCandidate {
        PatternCandidate {
            definition: PatternDefinition {
                version: PATTERN_SCHEMA_VERSION,
                name: PATTERN_TEST_COMMAND.into(),
                context: PatternContext {
                    tool_identity: "workcell/shell".into(),
                    executable_identity: PATTERN_TEST_COMMAND.into(),
                    effective_workdir: project.to_string_lossy().into_owned(),
                    path_binding: project.to_string_lossy().into_owned(),
                    analysis_version: PATTERN_TEST_ANALYSIS.into(),
                },
                argv: vec![PatternToken::Exact {
                    value: PATTERN_TEST_COMMAND.into(),
                    role: ArgumentRole::Executable,
                }],
                slots: Vec::new(),
                combinations: SlotCombinations::Independent,
            },
            evidence: CandidateEvidence {
                support: SupportCount {
                    observations: 2,
                    independent_sessions: 2,
                },
                provenance: ObservationProvenance::Imported,
                sources: [PATTERN_TEST_SOURCE.into()].into(),
                outcomes: [(InvocationOutcome::Unknown, 2)].into(),
                first_seen_ms: 1,
                last_seen_ms: 1,
                distributions: Default::default(),
                tuples: Vec::new(),
            },
        }
    }

    fn scan_outcome(candidates: Vec<PatternCandidate>) -> PatternDiscoveryOutcome {
        let limits = DiscoveryLimits::default();
        PatternDiscoveryOutcome::Ready(Box::new(PatternDiscoveryReport {
            candidates: candidates.into(),
            sample: Default::default(),
            history_limits: limits.history,
            recognition: Default::default(),
            recognizer_limits: Default::default(),
            calls: 0,
            max_calls: limits.max_calls,
            analysis_bytes: 0,
            max_analysis_bytes: limits.max_analysis_bytes,
            max_elapsed_ms: limits.max_elapsed_ms,
            partial_reasons: Vec::new(),
        }))
    }

    fn scan_candidates(outcome: &PatternDiscoveryOutcome) -> &[PatternCandidate] {
        let PatternDiscoveryOutcome::Ready(report) = outcome else {
            panic!("expected a successful scan");
        };
        &report.candidates
    }

    #[test_case(false; "ready_cache")]
    #[test_case(true; "unavailable_cache")]
    fn explicit_pattern_refresh_bypasses_cache_without_changing_normal_reuse(fail: bool) {
        let temp = tempfile::tempdir().unwrap();
        let project = fs::canonicalize(temp.path()).unwrap();
        let scans = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&scans);
        let mut worker = PatternSuggestionWorker::spawn_with(
            StateDir::from_path(project.clone()),
            move |_, _, _| {
                count.fetch_add(1, Ordering::Relaxed);
                if fail {
                    PatternDiscoveryOutcome::Unavailable(PATTERN_SCAN_UNAVAILABLE)
                } else {
                    scan_outcome(Vec::new())
                }
            },
        )
        .unwrap();
        for (mode, expected) in [
            (PatternDiscoveryMode::Cached, 1),
            (PatternDiscoveryMode::Cached, 1),
            (PatternDiscoveryMode::Refresh, 2),
            (PatternDiscoveryMode::Cached, 2),
        ] {
            (worker.loader)(project.clone(), mode)
                .recv_timeout(PATTERN_TEST_WAIT)
                .unwrap();
            assert_eq!(scans.load(Ordering::Relaxed), expected);
        }
        assert!(worker.shutdown(PATTERN_TEST_WAIT));
    }

    #[test_case(PatternDiscoveryMode::Refresh; "overlapping_refreshes_share_one_scan")]
    fn pattern_refreshes_coalesce_while_the_scan_is_running(mode: PatternDiscoveryMode) {
        let temp = tempfile::tempdir().unwrap();
        let project = fs::canonicalize(temp.path()).unwrap();
        let scans = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&scans);
        let (entered, ready) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let mut worker = PatternSuggestionWorker::spawn_with(
            StateDir::from_path(project.clone()),
            move |_, _, _| {
                if count.fetch_add(1, Ordering::Relaxed) > 0 {
                    entered.send(()).unwrap();
                    released.recv_timeout(PATTERN_TEST_WAIT).unwrap();
                }
                scan_outcome(Vec::new())
            },
        )
        .unwrap();
        (worker.loader)(project.clone(), PatternDiscoveryMode::Cached)
            .recv_timeout(PATTERN_TEST_WAIT)
            .unwrap();
        let first = (worker.loader)(project.clone(), mode.clone());
        ready.recv_timeout(PATTERN_TEST_WAIT).unwrap();
        let second = (worker.loader)(project, mode);
        release.send(()).unwrap();
        let first = first.recv_timeout(PATTERN_TEST_WAIT).unwrap();
        let second = second.recv_timeout(PATTERN_TEST_WAIT).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(scans.load(Ordering::Relaxed), 2);
        assert!(worker.shutdown(PATTERN_TEST_WAIT));
    }

    #[test_case(PatternDiscoveryMode::Cached; "cancelled_results_never_populate_cache")]
    fn cancelled_pattern_scan_is_not_reused_by_the_next_request(mode: PatternDiscoveryMode) {
        let temp = tempfile::tempdir().unwrap();
        let project = fs::canonicalize(temp.path()).unwrap();
        let scans = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&scans);
        let (entered, ready) = mpsc::channel();
        let mut worker = PatternSuggestionWorker::spawn_with(
            StateDir::from_path(project.clone()),
            move |_, _, stop| {
                if count.fetch_add(1, Ordering::Relaxed) == 0 {
                    entered.send(()).unwrap();
                    while !stop() {
                        thread::yield_now();
                    }
                }
                scan_outcome(Vec::new())
            },
        )
        .unwrap();
        let first = (worker.loader)(project.clone(), mode.clone());
        ready.recv_timeout(PATTERN_TEST_WAIT).unwrap();
        drop(first);
        (worker.loader)(project, mode)
            .recv_timeout(PATTERN_TEST_WAIT)
            .unwrap();
        assert_eq!(scans.load(Ordering::Relaxed), 2);
        assert!(worker.shutdown(PATTERN_TEST_WAIT));
    }

    #[test_case("rows"; "bounded_sample")]
    #[test_case("calls"; "call_limit")]
    #[test_case("time"; "elapsed_budget")]
    #[test_case("sampling"; "sampling_budget")]
    #[test_case("recognition"; "recognition_budget")]
    #[test_case("session_rows"; "per_session_partial_sample")]
    #[test_case("omissions"; "sanitized_scope_omission_counts")]
    #[test_case("obligations"; "unresolved_source_obligation_counts")]
    #[test_case("capacity"; "capacity_exclusions_and_admission_order_bias")]
    #[test_case("storage"; "unavailable_storage")]
    #[test_case("failure"; "recognition_failure")]
    fn discovery_report_preserves_partial_counts_and_distinguishes_failure(reason: &str) {
        let mut report = DiscoveryReport {
            version: 0,
            read_only: true,
            historical_context_verified: false,
            project: TEST_CWD.into(),
            source_identity: PATTERN_TEST_SOURCE.into(),
            limits: DiscoveryLimits::default(),
            recognizer_limits: Default::default(),
            max_command_bytes: PATTERN_HISTORY_ROW_BYTES,
            as_of_ms: 0,
            sample: Default::default(),
            processing: Default::default(),
            recognition: Default::default(),
            candidates: vec![suggestion_candidate(Path::new(TEST_CWD))],
            provenance: Default::default(),
            assumptions: &[],
            limitations: &[],
        };
        report.sample.sessions = 2;
        report.recognition.retained_observations = 2;
        match reason {
            "rows" => report.sample.truncated = true,
            "calls" => report.processing.call_limit = true,
            "time" => report.processing.timed_out = true,
            "sampling" => report.processing.sampling_time_limit = true,
            "recognition" => report.processing.recognition_time_limit = true,
            "session_rows" => report.sample.session_row_cutoffs = 2,
            "omissions" => {
                report.processing.represented_commands = 4;
                report.processing.observed_commands = 2;
                report.processing.calls_with_omitted_commands = 2;
                report
                    .processing
                    .omission_counts
                    .insert(PatternOmissionReason::InterpretedExecutable, 2);
            }
            "obligations" => report
                .processing
                .obligation_counts
                .push(PatternObligationCount {
                    kind: PatternObligationKind::Redirect,
                    count: 2,
                }),
            "capacity" => {
                report
                    .recognition
                    .exclusions
                    .insert(RecognitionExclusion::Capacity, 2);
            }
            "storage" => report.processing.storage_unavailable = true,
            "failure" => report.processing.recognition_failed = true,
            _ => unreachable!(),
        }
        let unavailable =
            report.processing.storage_unavailable || report.processing.recognition_failed;
        match discovery_outcome(report) {
            PatternDiscoveryOutcome::Unavailable(message) => {
                assert!(unavailable);
                assert_eq!(message, PATTERN_SCAN_UNAVAILABLE);
            }
            PatternDiscoveryOutcome::Ready(report) => {
                assert!(!unavailable);
                assert!(!report.partial_reasons.is_empty());
                assert_eq!(report.sample.sessions, 2);
                assert_eq!(report.recognition.retained_observations, 2);
                assert_eq!(report.candidates.len(), 1);
                match reason {
                    "session_rows" => assert!(report.partial_reasons.contains(&format!(
                        "{PATTERN_SESSION_LIMIT}: 2 sessions cut short at {} rows (main + subagents)",
                        DiscoveryLimits::default().max_rows_per_session
                    ))),
                    "omissions" => assert!(report.partial_reasons.contains(&format!(
                        "{PATTERN_OMITTED_SCOPES} ({:?}): 2", PatternOmissionReason::InterpretedExecutable
                    ))),
                    "obligations" => assert!(report.partial_reasons.contains(&format!(
                        "{PATTERN_SOURCE_OBLIGATIONS} ({:?}): 2 (not full-call authorization)", PatternObligationKind::Redirect
                    ))),
                    "capacity" => assert!(report.partial_reasons.contains(&format!(
                        "{RECOGNIZER_CAPACITY}: 2. {RECOGNIZER_ORDER_BIAS}"
                    ))),
                    _ => {}
                }
            }
        }
    }

    #[test_case(false; "coalesces_successful_loads_across_tabs")]
    #[test_case(true; "failed_loads_have_a_bounded_retry_delay")]
    fn pattern_suggestion_worker_caches_one_load_per_project(fail: bool) {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let scans = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&scans);
        let mut worker = PatternSuggestionWorker::spawn_with(
            StateDir::from_path(root.clone()),
            move |_, project, _| {
                count.fetch_add(1, Ordering::Relaxed);
                if fail {
                    PatternDiscoveryOutcome::Unavailable(PATTERN_SCAN_UNAVAILABLE)
                } else {
                    scan_outcome(vec![suggestion_candidate(project)])
                }
            },
        )
        .unwrap();
        let mut prior = None;
        for _ in 0..3 {
            let reply = (worker.loader)(root.clone(), PatternDiscoveryMode::Cached)
                .recv_timeout(PATTERN_TEST_WAIT)
                .unwrap();
            if fail {
                assert!(matches!(
                    reply.as_ref(),
                    PatternDiscoveryOutcome::Unavailable(PATTERN_SCAN_UNAVAILABLE)
                ));
            } else {
                let candidates = reply;
                assert_eq!(scan_candidates(&candidates).len(), 1);
                if let Some(prior) = &prior {
                    assert!(Arc::ptr_eq(prior, &candidates));
                }
                prior = Some(candidates);
            }
        }
        assert_eq!(scans.load(Ordering::Relaxed), 1);
        assert!(worker.shutdown(PATTERN_TEST_WAIT));
        assert!(worker.thread.is_none());
        assert!((worker.loader)(root, PatternDiscoveryMode::Cached).is_disconnected());
    }

    #[test_case(false, PATTERN_CACHE_TTL; "successful_cache_timestamp")]
    #[test_case(true, PATTERN_RETRY_DELAY; "failed_cache_timestamp")]
    fn pattern_cache_expiration_is_explicit(fail: bool, ttl: Duration) {
        let loaded_at = Instant::now();
        let entry = CachedPatternSuggestions {
            project: TEST_CWD.into(),
            loaded_at,
            outcome: Arc::new(if fail {
                PatternDiscoveryOutcome::Unavailable(PATTERN_SCAN_UNAVAILABLE)
            } else {
                scan_outcome(Vec::new())
            }),
        };
        assert!(entry.fresh_at(loaded_at));
        assert!(!entry.fresh_at(loaded_at + ttl));
    }

    #[test_case(PATTERN_CACHE_PROJECTS + 1; "bounded_project_cache")]
    fn pattern_suggestion_worker_evicts_old_projects(projects: usize) {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let scans = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&scans);
        let mut worker = PatternSuggestionWorker::spawn_with(
            StateDir::from_path(root.clone()),
            move |_, project, _| {
                count.fetch_add(1, Ordering::Relaxed);
                scan_outcome(vec![suggestion_candidate(project)])
            },
        )
        .unwrap();
        for index in 0..projects {
            let project = root.join(index.to_string());
            fs::create_dir(&project).unwrap();
            let outcome = (worker.loader)(project.clone(), PatternDiscoveryMode::Cached)
                .recv_timeout(PATTERN_TEST_WAIT)
                .unwrap();
            let candidates = scan_candidates(&outcome);
            assert_eq!(
                Path::new(&candidates[0].definition.context.path_binding),
                project
            );
        }
        let _ = (worker.loader)(root.join("0"), PatternDiscoveryMode::Cached)
            .recv_timeout(PATTERN_TEST_WAIT)
            .unwrap();
        assert_eq!(scans.load(Ordering::Relaxed), projects + 1);
        assert!(worker.shutdown(PATTERN_TEST_WAIT));
    }

    #[test_case(false; "receiver_drop_cancels_stale_context")]
    #[test_case(true; "shutdown_cancels_and_joins_active_scan")]
    fn pattern_suggestion_worker_cancellation_is_owned(shutdown: bool) {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let (entered, ready) = mpsc::channel();
        let (observed_cancel, cancelled) = mpsc::channel();
        let mut worker = PatternSuggestionWorker::spawn_with(
            StateDir::from_path(root.clone()),
            move |_, _, stop| {
                entered.send(()).unwrap();
                while !stop() {
                    thread::yield_now();
                }
                observed_cancel.send(()).unwrap();
                scan_outcome(Vec::new())
            },
        )
        .unwrap();
        let reply = (worker.loader)(root, PatternDiscoveryMode::Cached);
        ready.recv_timeout(PATTERN_TEST_WAIT).unwrap();
        if shutdown {
            assert!(worker.shutdown(PATTERN_TEST_WAIT));
            assert!(reply.is_disconnected());
        } else {
            drop(reply);
            cancelled.recv_timeout(PATTERN_TEST_WAIT).unwrap();
            assert!(worker.shutdown(PATTERN_TEST_WAIT));
        }
    }

    #[test_case(PATTERN_LOAD_QUEUE; "full_queue_never_blocks_startup")]
    fn pattern_suggestion_requests_are_bounded(capacity: usize) {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let (entered, ready) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let mut worker = PatternSuggestionWorker::spawn_with(
            StateDir::from_path(root.clone()),
            move |_, _, _| {
                entered.send(()).unwrap();
                released.recv_timeout(PATTERN_TEST_WAIT).unwrap();
                scan_outcome(Vec::new())
            },
        )
        .unwrap();
        let first = (worker.loader)(root.clone(), PatternDiscoveryMode::Cached);
        ready.recv_timeout(PATTERN_TEST_WAIT).unwrap();
        let pending = (0..capacity)
            .map(|_| (worker.loader)(root.clone(), PatternDiscoveryMode::Cached))
            .collect::<Vec<_>>();
        assert!((worker.loader)(root, PatternDiscoveryMode::Cached).is_disconnected());
        worker.stop.store(true, Ordering::Release);
        release.send(()).unwrap();
        assert!(worker.shutdown(PATTERN_TEST_WAIT));
        assert!(first.is_disconnected());
        assert!(pending.iter().all(flume::Receiver::is_disconnected));
    }

    #[test_case(Duration::ZERO; "blocked_io_cannot_hang_ui_shutdown")]
    fn pattern_suggestion_shutdown_has_a_hard_wait_budget(budget: Duration) {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let (entered, ready) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let mut worker = PatternSuggestionWorker::spawn_with(
            StateDir::from_path(root.clone()),
            move |_, _, _| {
                entered.send(()).unwrap();
                released.recv_timeout(PATTERN_TEST_WAIT).unwrap();
                scan_outcome(Vec::new())
            },
        )
        .unwrap();
        let reply = (worker.loader)(root.clone(), PatternDiscoveryMode::Cached);
        ready.recv_timeout(PATTERN_TEST_WAIT).unwrap();
        assert!(!worker.shutdown(budget));
        assert!((worker.loader)(root, PatternDiscoveryMode::Cached).is_disconnected());
        release.send(()).unwrap();
        worker.finished.recv_timeout(PATTERN_TEST_WAIT).unwrap();
        assert!(reply.recv_timeout(PATTERN_TEST_WAIT).is_err());
    }

    #[cfg(unix)]
    #[test_case("alias"; "startup_requires_a_canonical_local_project")]
    fn pattern_suggestion_worker_refuses_project_aliases(name: &str) {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let alias = root.join(name);
        symlink(&root, &alias).unwrap();
        let scans = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&scans);
        let mut worker =
            PatternSuggestionWorker::spawn_with(StateDir::from_path(root), move |_, _, _| {
                count.fetch_add(1, Ordering::Relaxed);
                scan_outcome(Vec::new())
            })
            .unwrap();
        assert!(matches!(
            (worker.loader)(alias, PatternDiscoveryMode::Cached)
                .recv_timeout(PATTERN_TEST_WAIT)
                .unwrap()
                .as_ref(),
            PatternDiscoveryOutcome::Unavailable(PATTERN_PROJECT_UNAVAILABLE)
        ));
        assert_eq!(scans.load(Ordering::Relaxed), 0);
        assert!(worker.shutdown(PATTERN_TEST_WAIT));
    }

    #[test_case(true, false; "remote_runtime")]
    #[test_case(false, true; "ephemeral_storage")]
    fn pattern_suggestion_worker_respects_privacy(remote: bool, ephemeral: bool) {
        let temp = tempfile::tempdir().unwrap();
        let storage = if ephemeral {
            StateDir::split(temp.path().join("volatile"), temp.path().join("persistent"))
        } else {
            StateDir::from_path(temp.path().into())
        };
        assert!(PatternSuggestionWorker::spawn(&storage, remote).is_none());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test_case("duplicate"; "deduplicates_definition_fingerprints")]
    #[test_case("foreign"; "rejects_another_project")]
    #[test_case("oversized"; "bounds_cached_candidate_bytes")]
    fn pattern_suggestion_cache_rejects_unsafe_or_redundant_entries(case: &str) {
        let root = Path::new(TEST_CWD);
        let candidate = suggestion_candidate(root);
        let mut other = candidate.clone();
        match case {
            "foreign" => other.definition.context.path_binding = "/other/project".into(),
            "oversized" => {
                other
                    .evidence
                    .sources
                    .insert("x".repeat(PATTERN_CACHE_BYTES_PER_PROJECT));
            }
            _ => other.definition.name = "renamed".into(),
        }
        let retained = cacheable_pattern_candidates(root, vec![candidate.clone(), other]);
        assert_eq!(retained.as_ref(), [candidate]);
    }

    #[test_case("history"; "real_read_only_startup_loader_uses_synthetic_database")]
    fn pattern_suggestion_startup_reads_imported_proposals_without_policy_writes(_case: &str) {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let storage = StateDir::from_path(root.join("storage"));
        fs::create_dir(storage.path()).unwrap();
        for command in [PATTERN_FIRST_COMMAND, PATTERN_SECOND_COMMAND] {
            let mut session = AppSession::new(TEST_MODEL, root.to_str().unwrap());
            let id = CaudraId::generate();
            session.push_message(HistoryItem {
                id,
                parent_id: None,
                supersedes: None,
                stands_for: None,
                group_id: id,
                kind: HistoryItemKind::ToolCall {
                    call_id: id.to_string(),
                    name: "shell".into(),
                    input: serde_json::json!({"command": command}),
                    thought_signature: None,
                    source: None,
                },
            });
            session.save(&storage).unwrap();
        }
        let before = SessionDatabase::open_read_only(&storage)
            .unwrap()
            .raw_permission_snapshot()
            .unwrap();
        let mut worker = PatternSuggestionWorker::spawn(&storage, false).unwrap();
        let outcome = (worker.loader)(root, PatternDiscoveryMode::Cached)
            .recv_timeout(PATTERN_TEST_WAIT)
            .unwrap();
        let candidates = scan_candidates(&outcome);
        assert!(worker.shutdown(PATTERN_TEST_WAIT));
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].evidence.provenance,
            ObservationProvenance::Imported
        );
        assert_eq!(candidates[0].evidence.support.independent_sessions, 2);
        assert_eq!(
            before,
            SessionDatabase::open_read_only(&storage)
                .unwrap()
                .raw_permission_snapshot()
                .unwrap()
        );
    }

    fn relocation_test_tab(storage: &StateDir, cwd: &Path) -> SessionTab {
        let mut session = AppSession::new(TEST_MODEL, &cwd.to_string_lossy());
        session.save(storage).unwrap();
        let lease = Arc::new(SessionLease::acquire(storage, session.id).unwrap());
        SessionTab {
            session,
            lease,
            cursor: None,
        }
    }

    fn relocation_handoff(
        storage: &StateDir,
        tabs: &[SessionTab],
        source: &Path,
        destination: &Path,
        bulk: bool,
    ) -> SessionRelocationHandoff {
        let sessions: Vec<_> = SessionDatabase::open_state(storage)
            .unwrap()
            .local_session_locations()
            .unwrap()
            .into_iter()
            .filter(|entry| entry.cwd == source.to_string_lossy())
            .filter(|entry| bulk || entry.id == tabs[0].session.id)
            .collect();
        let leases = sessions
            .iter()
            .filter(|entry| !tabs.iter().any(|tab| tab.session.id == entry.id))
            .map(|entry| Arc::new(SessionLease::acquire(storage, entry.id).unwrap()))
            .collect();
        SessionRelocationHandoff {
            request: SessionRelocation {
                sessions,
                source_cwd: bulk.then(|| source.to_string_lossy().into_owned()),
                destination: destination.to_string_lossy().into_owned(),
                include_project_usage: bulk,
                keep_plan: false,
            },
            donor: None,
            leases,
        }
    }

    #[test_case(PROJECT_ENV_PATH, true; "project_env")]
    #[test_case(".env", false; "root_env_is_not_project_env")]
    #[test_case(".caudra/config.lua", false; "other_project_file")]
    fn project_env_detection_is_scoped_to_project_env(relative: &str, expected: bool) {
        let temp = tempfile::tempdir().unwrap();
        assert!(!project_env_present(temp.path()));
        let path = temp.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "").unwrap();
        assert_eq!(project_env_present(temp.path()), expected);
        fs::remove_file(path).unwrap();
        assert!(!project_env_present(temp.path()));
    }

    #[cfg(unix)]
    #[test_case(false; "existing_target")]
    #[test_case(true; "dangling_target")]
    fn project_env_detection_is_conservative_for_symlinks(dangling: bool) {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        if !dangling {
            fs::write(&target, "").unwrap();
        }
        let path = temp.path().join(PROJECT_ENV_PATH);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        symlink(target, path).unwrap();
        assert!(project_env_present(temp.path()));
    }

    #[test_case(None, true, false; "no_project_env")]
    #[test_case(Some("source"), true, true; "source_project_env")]
    #[test_case(Some("destination"), true, true; "destination_project_env")]
    #[test_case(Some("startup"), true, true; "startup_project_env_appeared")]
    #[test_case(None, false, false; "missing_source_without_env")]
    #[test_case(Some("destination"), false, true; "missing_source_destination_env")]
    #[test_case(Some("startup"), false, true; "missing_source_startup_env")]
    fn live_relocation_checks_project_environments(
        env_directory: Option<&str>,
        source_exists: bool,
        expected: bool,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        let startup = temp.path().join("startup");
        for directory in [&destination, &startup] {
            fs::create_dir(directory).unwrap();
        }
        if source_exists {
            fs::create_dir(&source).unwrap();
        }
        let startup_project_env = project_env_present(&startup);
        if let Some(directory) = env_directory {
            let path = temp.path().join(directory).join(PROJECT_ENV_PATH);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        let tabs = vec![relocation_test_tab(&storage, &source)];
        let handoff = relocation_handoff(&storage, &tabs, &source, &destination, false);
        assert!(relocation_moves_live_tabs(&tabs, &handoff.request));
        assert_eq!(
            relocation_requires_env_restart(&tabs, &handoff.request, &startup, startup_project_env),
            expected
        );
    }

    #[test_case(&[], true, true ; "tui_adopts_the_session_directory")]
    #[test_case(&["--print"], true, false ; "print_mode_stays")]
    #[test_case(&["--ephemeral"], true, false ; "ephemeral_run_stays")]
    #[test_case(&[], false, false ; "removed_directory_stays")]
    fn session_flag_resumes_in_the_session_directory(
        flags: &[&str],
        directory_exists: bool,
        adopted: bool,
    ) {
        use clap::Parser;
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        let tab = relocation_test_tab(&storage, &project);
        if !directory_exists {
            fs::remove_dir(&project).unwrap();
        }
        let id = tab.session.id.to_string();
        let cli = Cli::parse_from(["caudra", "--session", &id].iter().chain(flags));

        assert_eq!(
            resumed_session_cwd(&cli, &storage).unwrap(),
            (adopted.then_some(project), Vec::new())
        );
    }

    #[test_case(false; "startup_env_removed")]
    #[test_case(true; "startup_directory_removed")]
    fn live_relocation_remembers_captured_startup_environment(remove_directory: bool) {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let source = temp.path().join("missing source");
        let destination = temp.path().join("destination");
        fs::create_dir(&destination).unwrap();
        let startup = temp.path().join("startup");
        let path = startup.join(PROJECT_ENV_PATH);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "").unwrap();
        let startup_project_env = project_env_present(&startup);
        if remove_directory {
            fs::remove_dir_all(&startup).unwrap();
        } else {
            fs::remove_file(path).unwrap();
        }
        let tabs = vec![relocation_test_tab(&storage, &source)];
        let handoff = relocation_handoff(&storage, &tabs, &source, &destination, false);
        assert!(!relocation_requires_env_restart(
            &tabs,
            &handoff.request,
            &startup,
            false,
        ));
        assert!(relocation_requires_env_restart(
            &tabs,
            &handoff.request,
            &startup,
            startup_project_env,
        ));
    }

    #[test_case(false, false; "current_only_new_layout")]
    #[test_case(false, true; "current_only_existing_layout")]
    #[test_case(true, false; "active_bulk_new_layout")]
    #[test_case(true, true; "active_bulk_existing_layout")]
    fn relocation_retains_live_tabs_after_destination_config_failure(
        bulk: bool,
        existing_layout: bool,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let source = temp.path().join("missing source");
        let destination = fs::canonicalize(temp.path()).unwrap();
        let tabs = vec![
            relocation_test_tab(&storage, &source),
            relocation_test_tab(&storage, &source),
        ];
        let ids: Vec<_> = tabs.iter().map(|tab| tab.session.id).collect();
        let closed = save_test_session(&storage, &source);
        let donor = save_test_session(&storage, &destination);
        write_workspace_tabs(
            &storage,
            &source,
            &WorkspaceTabs {
                open: ids.clone(),
                focused: Some(ids[1]),
            },
        )
        .unwrap();
        if existing_layout {
            write_workspace_tabs(
                &storage,
                &destination,
                &WorkspaceTabs {
                    open: vec![donor],
                    focused: Some(donor),
                },
            )
            .unwrap();
        }
        let mut handoff = relocation_handoff(&storage, &tabs, &source, &destination, bulk);
        handoff.donor = Some((donor, destination.to_string_lossy().into_owned()));
        let mut old_lineage = tabs[0].session.clone();
        old_lineage.save(&storage).unwrap();
        let mut installed = Vec::new();
        let (mut resolved, committed) =
            relocate_stopped_sessions(tabs, 1, handoff, &storage, &source, |path| {
                installed.push(path.to_path_buf());
                Ok(())
            })
            .unwrap();
        let committed = committed.unwrap();
        assert!(committed.contains(RELOCATION_COMMITTED));
        assert!(committed.contains(if bulk {
            RELOCATION_USAGE_EMPTY
        } else {
            RELOCATION_USAGE_UNCHANGED
        }));
        assert_eq!(resolved.warnings, [committed]);
        assert_eq!(installed, slice::from_ref(&destination));
        assert_eq!(resolved.focused, usize::from(bulk));
        let expected_tabs = WorkspaceTabs {
            open: if bulk { ids.clone() } else { vec![ids[0]] },
            focused: Some(ids[usize::from(bulk)]),
        };
        assert_eq!(
            read_workspace_tabs(&storage, &destination).unwrap(),
            Some(expected_tabs.clone())
        );
        assert_eq!(
            resolved
                .tabs
                .iter()
                .map(|tab| tab.session.id)
                .collect::<Vec<_>>(),
            if bulk { ids.clone() } else { vec![ids[0]] }
        );
        assert!(old_lineage.save(&storage).is_err());
        resolved.tabs[0].session.save(&storage).unwrap();
        for id in [ids[0], ids[1], closed, donor] {
            let stored = setup::load_session(id, &storage).unwrap();
            let moved = id == ids[0] || (bulk && id != donor);
            assert_eq!(
                stored.cwd,
                if moved || id == donor {
                    &destination
                } else {
                    &source
                }
                .to_string_lossy()
            );
        }
        assert_eq!(
            local_runtime_cwd(&resolved.tabs, resolved.focused, Ok(source)).unwrap(),
            destination
        );
        let error =
            config_or_fallback::<()>(Err(eyre!(INJECTED_CONFIG_FAILURE)), None, &mut Vec::new())
                .unwrap_err();
        assert_eq!(error.to_string(), INJECTED_CONFIG_FAILURE);
        drop(resolved);
        let restored =
            resolve_sessions(true, None, TEST_MODEL, &destination, &storage, None).unwrap();
        assert_eq!(
            restored
                .tabs
                .iter()
                .map(|tab| tab.session.id)
                .collect::<Vec<_>>(),
            expected_tabs.open
        );
        assert_eq!(
            Some(restored.tabs[restored.focused].session.id),
            expected_tabs.focused
        );
    }

    #[test_case(true, true, true, RELOCATION_USAGE_MIGRATED; "bulk_usage")]
    #[test_case(true, false, true, RELOCATION_USAGE_UNCHANGED; "bulk_opt_out")]
    #[test_case(false, false, true, RELOCATION_USAGE_UNCHANGED; "single_session")]
    #[test_case(true, true, false, RELOCATION_USAGE_EMPTY; "empty_source_ledger")]
    fn relocation_reports_usage_and_preserves_destination(
        bulk: bool,
        include_usage: bool,
        source_usage: bool,
        expected_report: &str,
    ) {
        for live in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let storage = StateDir::from_path(temp.path().join("state"));
            let source = temp.path().join("source");
            let destination = fs::canonicalize(temp.path()).unwrap();
            let invoking = temp.path().join("invoking");
            let mut tabs = vec![relocation_test_tab(&storage, &source)];
            let id = tabs[0].session.id;
            let mut handoff = relocation_handoff(&storage, &tabs, &source, &destination, bulk);
            assert_eq!(handoff.request.include_project_usage, bulk);
            assert_eq!(handoff.request.source_cwd.is_some(), bulk);
            handoff.request.include_project_usage = include_usage;
            if !live {
                handoff.leases.push(Arc::clone(&tabs[0].lease));
                tabs = vec![relocation_test_tab(&storage, &invoking)];
            }
            let database = SessionDatabase::open_state(&storage).unwrap();
            for (cwd, bucket_start) in [
                (&destination, 0),
                (&source, 0),
                (&source, BUCKET_SECONDS as i64),
            ] {
                if cwd == &source && !source_usage {
                    continue;
                }
                database
                    .record_usage(&LedgerEntry {
                        bucket_start,
                        provider: TEST_MODEL,
                        model: TEST_MODEL,
                        cwd: &cwd.to_string_lossy(),
                        purpose: LedgerPurpose::Chat,
                        ephemeral: false,
                        subscription: false,
                        usage: RELOCATION_TEST_USAGE,
                        cost: RELOCATION_TEST_USAGE.cost,
                    })
                    .unwrap();
            }
            let before = database.usage_buckets(None).unwrap();
            let mut installed = Vec::new();
            let (resolved, committed) =
                relocate_stopped_sessions(tabs, 0, handoff, &storage, &invoking, |path| {
                    installed.push(path.to_path_buf());
                    Ok(())
                })
                .unwrap();
            let committed = committed.unwrap();
            assert!(committed.contains(expected_report), "{committed}");
            assert_eq!(resolved.warnings, [committed]);
            assert_eq!(installed.len(), usize::from(live));
            assert_eq!(
                resolved.tabs[0].session.cwd,
                if live { &destination } else { &invoking }.to_string_lossy()
            );
            assert_eq!(
                setup::load_session(id, &storage).unwrap().cwd,
                destination.to_string_lossy()
            );
            let after = database.usage_buckets(None).unwrap();
            if !include_usage || !source_usage {
                assert_eq!(after, before);
                continue;
            }
            let mut expected = before
                .iter()
                .filter(|bucket| bucket.cwd == source.to_string_lossy())
                .cloned()
                .collect::<Vec<_>>();
            let destination_bucket = before
                .iter()
                .find(|bucket| bucket.cwd == destination.to_string_lossy())
                .unwrap();
            for bucket in &mut expected {
                bucket.cwd = destination.to_string_lossy().into_owned();
                if bucket.bucket_start == destination_bucket.bucket_start {
                    bucket.input += destination_bucket.input;
                    bucket.output += destination_bucket.output;
                    bucket.cache_creation += destination_bucket.cache_creation;
                    bucket.cache_read += destination_bucket.cache_read;
                    bucket.cost += destination_bucket.cost;
                    bucket.priced_turns += destination_bucket.priced_turns;
                    bucket.unpriced_turns += destination_bucket.unpriced_turns;
                }
            }
            assert_eq!(after, expected);
        }
    }

    #[test_case(false, false; "closed_source")]
    #[test_case(true, false; "empty_source")]
    #[test_case(false, true; "closed_source_with_project_env")]
    #[test_case(true, true; "empty_source_with_project_env")]
    fn relocation_of_other_source_keeps_invoking_tabs_and_cwd(empty: bool, with_env: bool) {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let source = temp.path().join("deleted source");
        let destination = fs::canonicalize(temp.path()).unwrap();
        let invoking = temp.path().join("invoking");
        let tabs = vec![
            relocation_test_tab(&storage, &invoking),
            relocation_test_tab(&storage, &invoking),
        ];
        let ids: Vec<_> = tabs.iter().map(|tab| tab.session.id).collect();
        let closed = (!empty).then(|| save_test_session(&storage, &source));
        let donor = save_test_session(&storage, &destination);
        let destination_tabs = WorkspaceTabs {
            open: vec![donor],
            focused: Some(donor),
        };
        write_workspace_tabs(&storage, &destination, &destination_tabs).unwrap();
        let handoff = relocation_handoff(&storage, &tabs, &source, &destination, true);
        if with_env {
            for directory in [&source, &destination, &invoking] {
                let path = directory.join(PROJECT_ENV_PATH);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, "").unwrap();
            }
        }
        assert!(!relocation_moves_live_tabs(&tabs, &handoff.request));
        assert!(!relocation_requires_env_restart(
            &tabs,
            &handoff.request,
            &invoking,
            project_env_present(&invoking),
        ));
        let (resolved, committed) =
            relocate_stopped_sessions(tabs, 1, handoff, &storage, &invoking, |_| {
                panic!("closed-source migration must not change process cwd")
            })
            .unwrap();
        assert_eq!(committed.is_some(), !empty);
        assert_eq!(
            read_workspace_tabs(&storage, &destination).unwrap(),
            Some(destination_tabs)
        );
        assert_eq!(resolved.focused, 1);
        assert_eq!(
            resolved
                .tabs
                .iter()
                .map(|tab| tab.session.id)
                .collect::<Vec<_>>(),
            ids
        );
        assert!(
            resolved
                .tabs
                .iter()
                .all(|tab| tab.session.cwd == invoking.to_string_lossy())
        );
        if let Some(id) = closed {
            assert_eq!(
                setup::load_session(id, &storage).unwrap().cwd,
                destination.to_string_lossy()
            );
        }
    }

    #[test_case(false; "single_session")]
    #[test_case(true; "bulk")]
    fn relocation_same_directory_keeps_siblings_and_versions(bulk: bool) {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let cwd = fs::canonicalize(temp.path()).unwrap();
        let tabs = vec![
            relocation_test_tab(&storage, &cwd),
            relocation_test_tab(&storage, &cwd),
        ];
        let handoff = relocation_handoff(&storage, &tabs, &cwd, &cwd, bulk);
        let path = cwd.join(PROJECT_ENV_PATH);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
        assert!(!relocation_moves_live_tabs(&tabs, &handoff.request));
        assert!(!relocation_requires_env_restart(
            &tabs,
            &handoff.request,
            &cwd,
            project_env_present(&cwd),
        ));
        let layout = WorkspaceTabs {
            open: tabs.iter().map(|tab| tab.session.id).collect(),
            focused: Some(tabs[1].session.id),
        };
        write_workspace_tabs(&storage, &cwd, &layout).unwrap();
        let before = SessionDatabase::open_state(&storage)
            .unwrap()
            .local_session_locations()
            .unwrap();
        let (resolved, committed) =
            relocate_stopped_sessions(tabs, 1, handoff, &storage, &cwd, |_| {
                panic!("same-directory relocation must not change process cwd")
            })
            .unwrap();
        assert!(committed.is_none());
        assert!(resolved.warnings[0].contains(RELOCATION_USAGE_UNCHANGED));
        assert_eq!(resolved.tabs.len(), 2);
        assert_eq!(resolved.focused, 1);
        assert_eq!(read_workspace_tabs(&storage, &cwd).unwrap(), Some(layout));
        assert_eq!(
            SessionDatabase::open_state(&storage)
                .unwrap()
                .local_session_locations()
                .unwrap(),
            before
        );
    }

    #[test_case(false; "donor_changed")]
    #[test_case(true; "donor_deleted")]
    fn relocation_rejects_stale_donor(deleted: bool) {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let source = temp.path().join("source");
        let destination = fs::canonicalize(temp.path()).unwrap();
        let tabs = vec![relocation_test_tab(&storage, &source)];
        let donor = save_test_session(&storage, &destination);
        let mut handoff = relocation_handoff(&storage, &tabs, &source, &destination, false);
        handoff.donor = Some((donor, destination.to_string_lossy().into_owned()));
        if deleted {
            AppSession::delete(donor, &storage).unwrap();
        } else {
            let mut session = setup::load_session(donor, &storage).unwrap();
            session.set_cwd(source.to_string_lossy().into_owned());
            session.save(&storage).unwrap();
        }
        let mut attempted = Vec::new();
        let (resolved, committed) =
            relocate_stopped_sessions(tabs, 0, handoff, &storage, &source, |path| {
                attempted.push(path.to_path_buf());
                Ok(())
            })
            .unwrap();
        assert!(committed.is_none());
        assert_eq!(attempted, [destination, source.clone()]);
        assert!(resolved.warnings[0].contains(RELOCATION_DONOR_CHANGED));
        assert_eq!(resolved.tabs[0].session.cwd, source.to_string_lossy());
    }

    #[test_case(false; "live_external_write")]
    #[test_case(true; "closed_external_write")]
    fn relocation_never_rebases_external_writes(closed: bool) {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let source = temp.path().join("source");
        let destination = fs::canonicalize(temp.path()).unwrap();
        let tabs = vec![relocation_test_tab(&storage, &source)];
        let id = if closed {
            save_test_session(&storage, &source)
        } else {
            tabs[0].session.id
        };
        let handoff = relocation_handoff(&storage, &tabs, &source, &destination, true);
        let mut external = setup::load_session(id, &storage).unwrap();
        external.save(&storage).unwrap();
        let before = SessionDatabase::open_state(&storage)
            .unwrap()
            .local_session_locations()
            .unwrap();
        let (resolved, committed) =
            relocate_stopped_sessions(tabs, 0, handoff, &storage, &source, |_| Ok(())).unwrap();
        assert!(committed.is_none());
        assert!(resolved.warnings[0].contains(RELOCATION_ABORTED));
        if !closed {
            assert!(resolved.warnings[0].contains(RELOCATION_VERSION_CHANGED));
        }
        assert_eq!(
            SessionDatabase::open_state(&storage)
                .unwrap()
                .local_session_locations()
                .unwrap(),
            before
        );
    }

    #[test_case(false; "cwd_install_failure")]
    #[test_case(true; "destination_removed")]
    fn relocation_filesystem_failure_reopens_original_tabs(missing: bool) {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&destination).unwrap();
        let tabs = vec![
            relocation_test_tab(&storage, &source),
            relocation_test_tab(&storage, &source),
        ];
        let handoff = relocation_handoff(&storage, &tabs, &source, &destination, false);
        if missing {
            fs::remove_dir(&destination).unwrap();
        }
        let mut attempted = Vec::new();
        let (resolved, committed) =
            relocate_stopped_sessions(tabs, 1, handoff, &storage, &source, |path| {
                attempted.push(path.to_path_buf());
                Err(io::Error::other(INJECTED_CWD_FAILURE))
            })
            .unwrap();
        assert!(committed.is_none());
        assert_eq!(resolved.tabs.len(), 2);
        assert_eq!(resolved.focused, 1);
        assert!(resolved.warnings[0].contains(RELOCATION_ABORTED));
        assert_eq!(attempted.len(), usize::from(!missing));
        if !missing {
            assert!(resolved.warnings[0].contains(INJECTED_CWD_FAILURE));
        }
        for tab in resolved.tabs {
            assert_eq!(
                setup::load_session(tab.session.id, &storage).unwrap().cwd,
                source.to_string_lossy()
            );
        }
    }

    #[test]
    fn relocation_database_open_failure_reopens_original_tabs() {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let source = temp.path().join("source");
        let destination = fs::canonicalize(temp.path()).unwrap();
        let tabs = vec![relocation_test_tab(&storage, &source)];
        let id = tabs[0].session.id;
        let handoff = relocation_handoff(&storage, &tabs, &source, &destination, false);
        let database_path = SessionDatabase::open_state(&storage).unwrap().path();
        let backup = temp.path().join("database-backup");
        fs::rename(&database_path, &backup).unwrap();
        fs::create_dir(&database_path).unwrap();
        let (resolved, committed) =
            relocate_stopped_sessions(tabs, 0, handoff, &storage, &source, |_| {
                panic!("database open failure must abort before changing cwd")
            })
            .unwrap();
        fs::remove_dir(&database_path).unwrap();
        fs::rename(backup, database_path).unwrap();
        assert!(committed.is_none());
        assert_eq!(resolved.tabs[0].session.id, id);
        assert!(resolved.warnings[0].contains(RELOCATION_ABORTED));
        assert_eq!(
            setup::load_session(id, &storage).unwrap().cwd,
            source.to_string_lossy()
        );
    }

    #[test_case(false; "transaction_failure_restores_cwd")]
    #[test_case(true; "rollback_cwd_failure_stops_restart")]
    fn relocation_transaction_failure_rolls_back_before_restart(rollback_fails: bool) {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let source = temp.path().join("missing source");
        let destination = fs::canonicalize(temp.path()).unwrap();
        let tabs = vec![relocation_test_tab(&storage, &source)];
        let id = tabs[0].session.id;
        let handoff = relocation_handoff(&storage, &tabs, &source, &destination, true);
        let mut attempted = Vec::new();
        let source_tabs = WorkspaceTabs {
            open: vec![id],
            focused: Some(id),
        };
        let donor = save_test_session(&storage, &destination);
        let destination_tabs = WorkspaceTabs {
            open: vec![donor],
            focused: Some(donor),
        };
        write_workspace_tabs(&storage, &source, &source_tabs).unwrap();
        write_workspace_tabs(&storage, &destination, &destination_tabs).unwrap();
        let result = relocate_stopped_sessions(tabs, 0, handoff, &storage, &source, |path| {
            attempted.push(path.to_path_buf());
            if path == destination {
                save_test_session(&storage, &source);
            } else if rollback_fails {
                return Err(io::Error::other(INJECTED_CWD_FAILURE));
            }
            Ok(())
        });
        assert_eq!(attempted, [destination.clone(), source.clone()]);
        if rollback_fails {
            let error = result.err().unwrap().to_string();
            assert!(error.contains(RELOCATION_ROLLBACK_FAILED));
            assert!(error.contains(RELOCATION_ABORTED));
        } else {
            let (resolved, committed) = result.unwrap();
            assert!(committed.is_none());
            assert!(resolved.warnings[0].contains(RELOCATION_ABORTED));
            assert_eq!(resolved.tabs[0].session.id, id);
        }
        assert_eq!(
            setup::load_session(id, &storage).unwrap().cwd,
            source.to_string_lossy()
        );
        assert_eq!(
            read_workspace_tabs(&storage, &source).unwrap(),
            Some(source_tabs)
        );
        assert_eq!(
            read_workspace_tabs(&storage, &destination).unwrap(),
            Some(destination_tabs)
        );
    }

    #[test_case(0; "first_focused")]
    #[test_case(1; "second_focused")]
    fn reload_cwd_comes_from_focused_session_not_startup(focused: usize) {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let tabs = vec![
            relocation_test_tab(&storage, &first),
            relocation_test_tab(&storage, &second),
        ];
        let current = temp.path().to_path_buf();
        assert_eq!(
            local_runtime_cwd(&tabs, focused, Ok(current.clone())).unwrap(),
            if focused == 0 { first } else { second }
        );
        assert_eq!(
            local_runtime_cwd(&[], 0, Ok(current.clone())).unwrap(),
            current
        );
        assert!(local_runtime_cwd(&[], 0, Err(io::Error::other(INJECTED_CWD_FAILURE))).is_err());
    }

    #[test_case(Some("fix it"), None, Some("fix it"); "flag_only")]
    #[test_case(None, Some("piped\n"), Some("piped\n"); "stdin_kept_verbatim")]
    #[test_case(Some("fix it\n"), Some("error output\n"), Some("fix it\n\nerror output"); "both_joined")]
    #[test_case(None, None, None; "neither")]
    #[test_case(Some("   "), None, None; "blank_flag")]
    #[test_case(None, Some("\n\n"), None; "blank_stdin")]
    #[test_case(Some("fix it"), Some("  \n"), Some("fix it"); "blank_stdin_ignored")]
    fn merge_prompt_combines_flag_and_stdin(
        flag: Option<&str>,
        piped: Option<&str>,
        expected: Option<&str>,
    ) {
        let merged = merge_prompt(flag.map(String::from), piped.map(String::from));

        assert_eq!(merged.as_deref(), expected);
    }

    fn save_test_session(storage: &StateDir, cwd: &Path) -> CaudraId {
        let mut session = AppSession::new(TEST_MODEL, &cwd.to_string_lossy());
        let id = session.id;
        session.save(storage).unwrap();
        id
    }

    /// The block is for a human watching the terminal; anything else reading
    /// stderr wants the resume command on a line of its own.
    #[test]
    fn exit_report_answers_the_destination_it_is_written_to() {
        let session = AppSession::new(TEST_MODEL, TEST_CWD);
        let summary = ExitSummary::new(&session, EXIT_RUN_TIME, 0);

        let plain = exit_report(&summary, false);
        assert_eq!(plain.lines().count(), 1, "{SCRIPTABLE}");
        assert!(exit_report(&summary, true).lines().count() > 1, "{BLOCK}");
    }

    /// `second_saw_first` requires both joins: `defer` joining the first
    /// closure before spawning the second, and `Drop` joining the second
    /// before the assert reads the flag.
    #[test]
    fn teardown_defer_joins_previous_and_drop_joins_last() {
        let first_done = Arc::new(AtomicBool::new(false));
        let second_saw_first = Arc::new(AtomicBool::new(false));
        let mut teardown = Teardown::default();

        let set = Arc::clone(&first_done);
        teardown.defer(move || set.store(true, Ordering::Release));

        let read = Arc::clone(&first_done);
        let record = Arc::clone(&second_saw_first);
        teardown.defer(move || record.store(read.load(Ordering::Acquire), Ordering::Release));

        drop(teardown);
        assert!(second_saw_first.load(Ordering::Acquire));
    }

    #[test]
    fn teardown_swallows_panic_and_keeps_working() {
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        let after_panic_ran = Arc::new(AtomicBool::new(false));
        let mut teardown = Teardown::default();
        teardown.defer(|| panic!("intentional"));
        let set = Arc::clone(&after_panic_ran);
        teardown.defer(move || set.store(true, Ordering::Release));
        drop(teardown);

        std::panic::set_hook(prev_hook);
        assert!(after_panic_ran.load(Ordering::Acquire));
    }

    #[test]
    fn ephemeral_storage_disables_the_retention_sweeper() {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::split(temp.path().join("volatile"), temp.path().join("persistent"));

        let sweeper = RetentionSweeper::spawn(storage, StorageConfig::default());

        assert!(sweeper._stop.is_none());
    }

    #[test]
    fn explicit_resume_rejects_an_active_session() {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let mut session = AppSession::new("test/model", "/project");
        let id = session.id;
        session.save(&storage).unwrap();
        let first = resolve_session(
            false,
            Some(&id.to_string()),
            "test/model",
            "/project",
            &storage,
        )
        .unwrap();

        let error = resolve_session(
            false,
            Some(&id.to_string()),
            "test/model",
            "/project",
            &storage,
        )
        .err()
        .unwrap();

        assert!(error.to_string().contains("already open"), "{error}");
        drop(first);
        assert!(
            resolve_session(
                false,
                Some(&id.to_string()),
                "test/model",
                "/project",
                &storage,
            )
            .is_ok()
        );
    }

    #[test]
    fn continue_does_not_fall_back_when_latest_session_is_active() {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let mut session = AppSession::new(TEST_MODEL, "/project");
        session.save(&storage).unwrap();
        let first = resolve_sessions(
            true,
            None,
            TEST_MODEL,
            Path::new("/project"),
            &storage,
            None,
        )
        .unwrap();

        let error = resolve_sessions(
            true,
            None,
            TEST_MODEL,
            Path::new("/project"),
            &storage,
            None,
        )
        .err()
        .unwrap();

        assert!(error.to_string().contains("already open"), "{error}");
        drop(first);
    }

    #[test_case(true)]
    #[test_case(false)]
    fn tui_resume_rejects_cross_authority(stored_remote: bool) {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let local = StoredWorkspaceBinding::local_from_cwd("/project");
        let remote: StoredWorkspaceBinding = serde_json::from_str(
            &serde_json::to_string(&local)
                .unwrap()
                .replace("caudra:local:v1", "https://remote.example"),
        )
        .unwrap();
        let mut session = AppSession::new_with_workspace(
            TEST_MODEL,
            ".",
            if stored_remote { remote.clone() } else { local },
        );
        session.save(&storage).unwrap();
        let result = resolve_sessions(
            false,
            Some(&session.id.to_string()),
            TEST_MODEL,
            Path::new("/project"),
            &storage,
            (!stored_remote).then_some(&remote),
        );
        assert!(result.is_err());
        if stored_remote {
            assert!(resolve_session(true, None, TEST_MODEL, ".", &storage).is_err());
        }
    }

    #[test]
    fn explicit_sandbox_transition_creates_fresh_session_without_local_context_or_grants() {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let mut local = AppSession::new(TEST_MODEL, TEST_CWD);
        local.meta.permission_mode = Some(PermissionMode::Yolo);
        local.meta.input_draft = Some(INJECTED_CONFIG_FAILURE.into());
        local.save(&storage).unwrap();
        let binding = StoredWorkspaceBinding::local_from_cwd(TEST_CWD);
        let remote: StoredWorkspaceBinding = serde_json::from_str(
            &serde_json::to_string(&binding)
                .unwrap()
                .replace("caudra:local:v1", "https://sandbox.test"),
        )
        .unwrap();
        let remote = remote.with_sandbox_record(CaudraId::generate()).unwrap();
        let resolved =
            resolve_remote_sessions(false, None, TEST_MODEL, ".", &storage, &remote).unwrap();
        let session = &resolved.tabs[0].session;
        assert_ne!(session.id, local.id);
        assert_eq!(session.workspace_binding(), Some(&remote));
        assert!(session.messages().is_empty());
        assert_eq!(session.meta.permission_mode, None);
        assert!(session.meta.input_draft.is_none());
        assert!(session.meta.structured_permission_rules.is_empty());
        assert_eq!(
            crate::setup::load_session(local.id, &storage)
                .unwrap()
                .meta
                .input_draft,
            local.meta.input_draft
        );
    }

    #[test]
    fn continue_restores_valid_workspace_tabs_in_order_and_focus() {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let workspace = temp.path().join("workspace");
        let other_workspace = temp.path().join("other-workspace");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&other_workspace).unwrap();
        let first = save_test_session(&storage, &workspace);
        let focused = save_test_session(&storage, &workspace);
        let wrong_cwd = save_test_session(&storage, &other_workspace);
        let missing = AppSession::new(TEST_MODEL, &workspace.to_string_lossy()).id;
        caudra_storage::state::write_workspace_tabs(
            &storage,
            &workspace,
            &WorkspaceTabs {
                open: vec![first, wrong_cwd, missing, first, focused],
                focused: Some(focused),
            },
        )
        .unwrap();

        let resolved =
            resolve_sessions(true, None, TEST_MODEL, &workspace, &storage, None).unwrap();

        assert_eq!(
            resolved
                .tabs
                .iter()
                .map(|tab| tab.session.id)
                .collect::<Vec<_>>(),
            vec![first, focused]
        );
        assert_eq!(resolved.focused, 1);
        assert!(
            resolved
                .warnings
                .iter()
                .any(|warning| warning.contains(DUPLICATE_TAB_WARNING)),
            "{:?}",
            resolved.warnings
        );
        assert!(
            resolved
                .warnings
                .iter()
                .any(|warning| warning.contains(MISSING_TAB_WARNING)),
            "{:?}",
            resolved.warnings
        );
        assert!(
            resolved
                .warnings
                .iter()
                .any(|warning| warning.contains(WRONG_CWD_WARNING)),
            "{:?}",
            resolved.warnings
        );
        let facts = SessionDatabase::open_state(&storage)
            .unwrap()
            .session_facts(None)
            .unwrap();
        assert!(
            facts
                .iter()
                .filter(|facts| facts.id == first || facts.id == focused)
                .all(|facts| facts.last_opened_at.is_some())
        );
        assert_eq!(
            facts
                .iter()
                .find(|facts| facts.id == wrong_cwd)
                .unwrap()
                .last_opened_at,
            None
        );
    }

    #[test]
    fn remote_continue_discovers_nested_cwd_by_durable_identity() {
        use caudra_workspace::{CwdHandle, ResourceId, ResourceScope, WorkspaceCursor};
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().into());
        let local = StoredWorkspaceBinding::local_from_cwd(".");
        let root: StoredWorkspaceBinding = serde_json::from_str(
            &serde_json::to_string(&local)
                .unwrap()
                .replace("caudra:local:v1", "https://remote.example"),
        )
        .unwrap();
        let nested = root
            .with_cursor(WorkspaceCursor::new(
                root.binding(),
                ResourceScope::new(
                    vec![ResourceId::new("root").unwrap()],
                    ResourceId::new("nested").unwrap(),
                )
                .unwrap(),
                0,
                CwdHandle::new("old-nested-handle").unwrap(),
            ))
            .unwrap();
        let mut session = AppSession::new_with_workspace(TEST_MODEL, "nested", nested.clone());
        session.save(&storage).unwrap();
        let resolved =
            resolve_remote_sessions(true, None, TEST_MODEL, ".", &storage, &root).unwrap();
        assert_eq!(resolved.tabs[0].session.id, session.id);
        assert_eq!(resolved.tabs[0].session.cwd, "nested");
        assert_eq!(resolved.tabs[0].session.workspace_binding(), Some(&nested));
    }

    #[test]
    fn continue_falls_back_to_newest_when_no_workspace_tab_is_usable() {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let workspace = temp.path().join("workspace");
        let other_workspace = temp.path().join("other-workspace");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&other_workspace).unwrap();
        let newest = save_test_session(&storage, &workspace);
        let wrong_cwd = save_test_session(&storage, &other_workspace);
        let missing = AppSession::new(TEST_MODEL, &workspace.to_string_lossy()).id;
        caudra_storage::state::write_workspace_tabs(
            &storage,
            &workspace,
            &WorkspaceTabs {
                open: vec![wrong_cwd, missing],
                focused: Some(wrong_cwd),
            },
        )
        .unwrap();

        let resolved =
            resolve_sessions(true, None, TEST_MODEL, &workspace, &storage, None).unwrap();

        assert_eq!(resolved.tabs.len(), 1);
        assert_eq!(resolved.tabs[0].session.id, newest);
        assert_eq!(resolved.focused, 0);
        assert!(
            resolved
                .warnings
                .iter()
                .any(|warning| warning.contains(MISSING_TAB_WARNING)),
            "{:?}",
            resolved.warnings
        );
        assert!(
            resolved
                .warnings
                .iter()
                .any(|warning| warning.contains(WRONG_CWD_WARNING)),
            "{:?}",
            resolved.warnings
        );
    }

    #[test_case("anthropic"; "anthropic_builtin")]
    fn login_placeholder_uses_builtin_default(provider_slug: &str) {
        let model = login_placeholder_model().unwrap();
        let provider = builtin_provider(provider_slug).unwrap();

        assert_eq!(model.provider.to_string(), provider_slug);
        assert_eq!(model.spec(), provider.default_model);
    }

    fn test_config() -> Config {
        RawConfig::default()
            .into_config(false)
            .expect("default config")
    }

    #[test]
    fn broken_config_with_fallback_uses_last_good_and_warns() {
        let mut last_good = test_config();
        last_good.always_fast = true;
        let mut warnings = Vec::new();

        let config = config_or_fallback(Err(eyre!("boom")), Some(last_good), &mut warnings)
            .expect("fallback config");

        assert!(config.always_fast);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].starts_with(CONFIG_FALLBACK_WARNING),
            "{warnings:?}"
        );
        assert!(warnings[0].contains("boom"), "{warnings:?}");
    }

    #[test]
    fn broken_config_without_fallback_is_fatal() {
        let mut warnings = Vec::new();
        let err = match config_or_fallback::<Config>(Err(eyre!("boom")), None, &mut warnings) {
            Err(e) => e,
            Ok(_) => panic!("expected error without fallback"),
        };
        assert!(err.to_string().contains("boom"));
        assert!(warnings.is_empty());
    }

    #[test_case(false, false; "local_default")]
    #[test_case(false, true; "local_disabled")]
    #[test_case(true, false; "remote_default")]
    #[test_case(true, true; "remote_disabled")]
    fn shared_config_applies_no_snapshots(remote: bool, disabled: bool) {
        use clap::Parser;

        let dir = tempfile::tempdir().unwrap();
        let mut cli = Cli::parse_from(["caudra", "--no-plugins"]);
        cli.no_snapshots = disabled;
        let config = load_config(&PluginHost::disabled(), &cli, dir.path(), remote).unwrap();
        assert_eq!(config.storage.snapshots.enabled, !disabled);
    }

    #[test_case(0; "zero_turns")]
    #[test_case(2; "bounded_turns")]
    fn shared_config_applies_cli_turn_limit_and_tool_policy(max: u32) {
        use clap::Parser;

        let dir = tempfile::tempdir().unwrap();
        let max = max.to_string();
        let cli = Cli::parse_from([
            "caudra",
            "--no-plugins",
            "--print",
            "--max-turns",
            &max,
            "--yolo",
            "--allowed-tools",
            "file_index",
            "--disallowed-tools",
            "shell",
        ]);
        let config = load_config(&PluginHost::disabled(), &cli, dir.path(), true).unwrap();
        assert_eq!(config.agent.max_turns, cli.max_turns);
        assert!(config.permissions.yolo);
        assert_eq!(config.agent.allowed_tools, ["file_index"]);
        assert!(
            config
                .agent
                .disabled_tools
                .iter()
                .any(|tool| tool == "shell")
        );
    }

    #[cfg(unix)]
    mod launch_messaging {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;
        use std::sync::Arc;

        use caudra_agent::History;
        use caudra_agent::peers::topics::{MAX_PATTERNS, TOO_MANY_PATTERNS};
        use caudra_agent::peers::{MAX_MEMBERSHIPS, PeerHost, TOO_MANY_MEMBERSHIPS};
        use caudra_providers::Message;
        use caudra_storage::messages::WorkRefusal;
        use caudra_storage::sessions::StoredPeerControls;
        use caudra_ui::AppSession;
        use tempfile::{Builder, TempDir};
        use test_case::test_case;

        use super::super::{add_groups, join_launch_groups, reserve_launch_name, subscribe_launch};
        use super::{TEST_CWD, TEST_MODEL};

        const LAUNCH_NAME: &str = "ci-watcher";
        const LAUNCH_TOPIC: &str = "ci.**";
        const STORED_TOPIC: &str = "deploy";
        const LAUNCH_GROUP: &str = "reviewers";
        const STORED_GROUP: &str = "auditors";
        const RESUMED_PROMPT: &str = "Fix the parser";
        const PRIVATE_MODE: u32 = 0o700;
        /// Short enough that peer socket paths fit the Unix limit.
        const PEER_PARENT: &str = "/tmp";

        fn peer_directory() -> TempDir {
            Builder::new()
                .permissions(Permissions::from_mode(PRIVATE_MODE))
                .tempdir_in(Path::new(PEER_PARENT).canonicalize().unwrap())
                .unwrap()
        }

        fn peer_host(directory: &TempDir) -> PeerHost {
            PeerHost::start_in(directory.path().to_owned(), Arc::default()).unwrap()
        }

        #[test_case(false; "new_session")]
        #[test_case(true; "resumed_session")]
        fn launch_name_is_saved_and_titles_only_a_new_session(resumed: bool) {
            let directory = peer_directory();
            let mut session = AppSession::new(TEST_MODEL, TEST_CWD);
            if resumed {
                session.replace_messages(
                    History::new(vec![Message::user(RESUMED_PROMPT.into())]).into_items(),
                );
            }
            let title = session.title.clone();
            reserve_launch_name(&peer_host(&directory), &mut session, LAUNCH_NAME).unwrap();
            assert_eq!(
                session
                    .meta
                    .peer_controls
                    .as_ref()
                    .and_then(|controls| controls.handle.as_deref()),
                Some(LAUNCH_NAME)
            );
            assert_eq!(
                session.title,
                if resumed { title } else { LAUNCH_NAME.into() }
            );
        }

        #[test]
        fn launch_name_conflict_stops_startup_before_naming_the_session() {
            let directory = peer_directory();
            let holder_host = peer_host(&directory);
            let mut holder = AppSession::new(TEST_MODEL, TEST_CWD);
            reserve_launch_name(&holder_host, &mut holder, LAUNCH_NAME).unwrap();
            let mut session = AppSession::new(TEST_MODEL, TEST_CWD);
            let title = session.title.clone();
            let error =
                reserve_launch_name(&peer_host(&directory), &mut session, LAUNCH_NAME).unwrap_err();
            assert!(error.to_string().contains(LAUNCH_NAME), "{error}");
            assert_eq!(session.meta.peer_controls, None);
            assert_eq!(session.title, title);
        }

        #[test_case(&[], &[], false, None; "no_flags_store_nothing")]
        #[test_case(&[], &[LAUNCH_TOPIC], false, Some(&[LAUNCH_TOPIC][..]); "new_subscription")]
        #[test_case(&[STORED_TOPIC], &[LAUNCH_TOPIC, STORED_TOPIC], true, Some(&[STORED_TOPIC, LAUNCH_TOPIC][..]); "resumed_subscriptions_are_kept")]
        fn launch_subscriptions_add_to_the_stored_ones(
            stored: &[&str],
            launched: &[&str],
            broadcasts: bool,
            expected: Option<&[&str]>,
        ) {
            let mut session = AppSession::new(TEST_MODEL, TEST_CWD);
            if !stored.is_empty() {
                session.meta.peer_controls = Some(StoredPeerControls {
                    topics: owned(stored),
                    ..StoredPeerControls::default()
                });
            }
            subscribe_launch(&mut session, &owned(launched), broadcasts).unwrap();
            let controls = session.meta.peer_controls;
            assert_eq!(
                controls.as_ref().map(|controls| controls.topics.clone()),
                expected.map(owned)
            );
            assert_eq!(
                controls.is_some_and(|controls| controls.broadcasts),
                broadcasts
            );
        }

        #[test]
        fn launch_subscriptions_over_the_limit_stop_startup_unchanged() {
            let mut session = AppSession::new(TEST_MODEL, TEST_CWD);
            let launched: Vec<String> = (0..=MAX_PATTERNS)
                .map(|index| format!("{STORED_TOPIC}-{index}"))
                .collect();
            let error = subscribe_launch(&mut session, &launched, true).unwrap_err();
            assert_eq!(error.to_string(), TOO_MANY_PATTERNS);
            assert_eq!(session.meta.peer_controls, None);
        }

        #[test_case(&[], &[LAUNCH_GROUP], &[LAUNCH_GROUP]; "new_membership")]
        #[test_case(&[STORED_GROUP], &[LAUNCH_GROUP, STORED_GROUP], &[STORED_GROUP, LAUNCH_GROUP]; "saved_memberships_come_first")]
        #[test_case(&[], &[LAUNCH_GROUP, LAUNCH_GROUP], &[LAUNCH_GROUP]; "repeated_group_joins_once")]
        fn launch_groups_add_to_the_saved_ones(
            stored: &[&str],
            launched: &[&str],
            expected: &[&str],
        ) {
            assert_eq!(
                add_groups(&owned(stored), &owned(launched)).unwrap(),
                owned(expected)
            );
        }

        #[test]
        fn launch_groups_over_the_limit_stop_startup() {
            let saved: Vec<String> = (0..MAX_MEMBERSHIPS)
                .map(|index| format!("{STORED_GROUP}-{index}"))
                .collect();
            let error = add_groups(&saved, &owned(&[LAUNCH_GROUP])).unwrap_err();
            assert_eq!(error.to_string(), TOO_MANY_MEMBERSHIPS);
        }

        #[test_case(&[], false; "no_groups_change_nothing")]
        #[test_case(&[LAUNCH_GROUP], true; "unknown_group_stops_startup")]
        fn launch_groups_must_exist_before_the_session_joins(launched: &[&str], refused: bool) {
            let directory = peer_directory();
            let mut session = AppSession::new(TEST_MODEL, TEST_CWD);
            let joined = join_launch_groups(&peer_host(&directory), &mut session, &owned(launched));
            assert_eq!(
                joined.err().map(|error| error.to_string()),
                refused.then(|| WorkRefusal::UnknownGroup(LAUNCH_GROUP.into()).to_string())
            );
            assert_eq!(session.meta.peer_controls, None);
        }

        fn owned(values: &[&str]) -> Vec<String> {
            values.iter().copied().map(str::to_owned).collect()
        }
    }
}
