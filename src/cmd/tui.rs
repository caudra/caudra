use std::collections::{HashMap, HashSet};
use std::env;
use std::io::{self, IsTerminal, Read};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use color_eyre::Result;
use color_eyre::eyre::Context;

use caudra_agent::command::{self, CustomCommand};
use caudra_agent::prompt::profile::{PromptProfileCatalog, SystemPromptProfile};
use caudra_agent::tools::ToolRegistry;
use caudra_config::{Config, RetentionConfig, load_env_files};
use caudra_lua::PluginHost;
use caudra_providers::model::Model;
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::sweep::{SweepPolicy, sweep_if_due};
use caudra_storage::sessions::{SessionDatabase, SessionLease};
use caudra_storage::state::{WorkspaceTabs, read_workspace_tabs};
use caudra_ui::{AppSession, ExitSummary, HerdrReporter, RunOutcome, SessionTab};

use crate::cli::Cli;
use crate::cmd::load_config;
use crate::setup;

const FALLBACK_MODEL_SPEC: &str = "anthropic/claude-sonnet-4-20250514";
const CONFIG_FALLBACK_WARNING: &str = "config reload failed, using previous config";
const MODEL_FALLBACK_WARNING: &str = "model resolution failed, keeping previous model";
/// The first sweep waits for startup and the first prompt to settle.
const SWEEP_STARTUP_DELAY: Duration = Duration::from_secs(60);
/// How often the sweep thread re-checks whether the interval has elapsed.
const SWEEP_POLL_INTERVAL: Duration = Duration::from_secs(60 * 60);
const SECONDS_PER_HOUR: u64 = 60 * 60;

/// Runs the retention sweep on its own thread while the TUI is open. Dropping
/// the sender wakes and stops the thread. Every step the sweep takes is
/// crash-safe, so shutdown never waits for a deletion in progress.
struct RetentionSweeper {
    _stop: Option<Sender<()>>,
}

impl RetentionSweeper {
    fn spawn(storage: StateDir, retention: RetentionConfig) -> Self {
        if storage.is_ephemeral() {
            return Self { _stop: None };
        }
        let policy = SweepPolicy {
            group_by: retention.group_by,
            interval: Duration::from_secs(retention.sweep_interval_hours * SECONDS_PER_HOUR),
            trim: retention.trim,
            forget: retention.forget,
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
) -> Result<(Stack, Vec<String>)> {
    let mut warnings = Vec::new();

    let mut plugin_host = PluginHost::with_jit(Arc::clone(ToolRegistry::global_arc()), !cli.no_jit)
        .context("initialize lua plugin host")?;

    let (fallback_config, fallback_model) = match fallback {
        Some((config, model, profiles, selected)) => {
            (Some((config, profiles, selected)), Some(model))
        }
        None => (None, None),
    };
    let reloading = fallback_model.is_some();
    let loaded = load_config(&plugin_host, cli, cwd).and_then(|config| {
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

    let commands = discover_commands(cli.no_commands);

    let model_result = setup::resolve_model(cli.model.as_deref(), &config.provider, storage);
    let (model, needs_login) = match (model_result, fallback_model) {
        (Ok(m), _) => (m, false),
        (Err(e), Some(last_model)) => {
            warnings.push(format!("{MODEL_FALLBACK_WARNING}: {e:#}"));
            (last_model, false)
        }
        (Err(_), None) if !cli.print => {
            let placeholder = Model::from_spec(FALLBACK_MODEL_SPEC).expect("fallback model");
            (placeholder, true)
        }
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
        let session = setup::load_session(id, storage)?;
        setup::report_session_start(caudra_otel::emit::START_RESUME, Some(session.id));
        return Ok(SessionTab { session, lease });
    }
    if continue_session {
        if let Some(summary) = AppSession::list(cwd, storage)?.into_iter().next() {
            let lease = Arc::new(SessionLease::acquire(storage, summary.id)?);
            let session = setup::load_session(summary.id, storage)?;
            setup::report_session_start(caudra_otel::emit::START_CONTINUE, Some(session.id));
            return Ok(SessionTab { session, lease });
        }
        tracing::info!("no previous session found for this directory, starting new");
    }
    let session = AppSession::new(model, cwd);
    let lease = Arc::new(SessionLease::acquire(storage, session.id)?);
    setup::report_session_start(caudra_otel::emit::START_FRESH, Some(session.id));
    Ok(SessionTab { session, lease })
}

struct ResolvedSessions {
    tabs: Vec<SessionTab>,
    focused: usize,
    warnings: Vec<String>,
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
        let Some(stored_cwd) = facts.get(&id) else {
            restore_warning(
                &mut warnings,
                format!("Stored workspace tab {id} no longer exists"),
            );
            continue;
        };
        let stored_cwd = match Path::new(stored_cwd).canonicalize() {
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
        let lease = match SessionLease::acquire(storage, id) {
            Ok(lease) => Arc::new(lease),
            Err(error) => {
                restore_warning(
                    &mut warnings,
                    format!("Stored workspace tab {id} is unavailable: {error}"),
                );
                continue;
            }
        };
        match setup::load_session(id, storage) {
            Ok(session) => {
                setup::report_session_start(caudra_otel::emit::START_CONTINUE, Some(session.id));
                tabs.push(SessionTab { session, lease });
            }
            Err(error) => restore_warning(
                &mut warnings,
                format!("Stored workspace tab {id} could not be loaded: {error:#}"),
            ),
        }
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
) -> Result<ResolvedSessions> {
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

pub fn run(mut cli: Cli) -> Result<ExitCode> {
    let persistent_storage = StateDir::resolve().context("resolve data directory")?;
    caudra_providers::model_registry::load_from_storage(&persistent_storage)
        .context("load model purpose bindings")?;

    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());

    load_env_files(&cwd);
    let _workcell_host = super::register_builtin_tools(&cwd)?;

    let (mut stack, _) = build_stack(&cli, &cwd, &persistent_storage, None)?;
    let ephemeral = cli.ephemeral || stack.config.storage.ephemeral;
    let (storage, _ephemeral_root) = super::run_storage(persistent_storage, ephemeral)?;

    let _logging = setup::init_logging(&stack.config.storage);
    setup::init_telemetry(&stack.config.telemetry);
    setup::install_panic_log_hook();
    setup::warn_ignored_provider_fields();
    setup::report_startup(setup::MODE_TUI, &stack.model, &cwd);

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
        crate::sdk_mode::run(crate::sdk_mode::SdkParams {
            cli,
            model: stack.model,
            config: stack.config.agent,
            permissions_config: stack.config.permissions,
            timeouts,
            prompt_slots,
            prompt_profiles: stack.prompt_profiles,
            fast,
            thinking,
            model_policy: Arc::new(stack.config.provider.model_policy.clone()),
            plugin_rules: stack.plugin_host.plugin_rules(),
        })
        .context("run sdk mode")?;
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
        crate::print::run(
            &stack.model,
            initial_prompt,
            cli.images,
            cli.output_format,
            cli.verbose,
            stack.config.agent,
            stack.config.permissions,
            timeouts,
            stack.plugin_host.event_handle(),
            fast,
            thinking,
            stack.default_prompt_profile,
            Arc::clone(&stack.prompt_profiles),
            Arc::new(stack.config.provider.model_policy.clone()),
            stack.plugin_host.plugin_rules(),
        )
        .context("run print mode")?;
        return Ok(ExitCode::SUCCESS);
    }

    let cwd_str = cwd.to_string_lossy().into_owned();
    let resolved = resolve_sessions(
        cli.continue_session,
        cli.session.as_deref(),
        &stack.model.spec(),
        &cwd,
        &storage,
    )?;
    let mut tabs = resolved.tabs;
    let mut focused = resolved.focused;
    let mut warnings = resolved.warnings;
    let mut teardown = Teardown::default();
    let mut herdr_reporter = HerdrReporter::from_env();
    let mut sweeper = RetentionSweeper::spawn(storage.clone(), stack.config.storage.retention);

    loop {
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

        let outcome = caudra_ui::run(
            caudra_ui::EventLoopParams {
                model,
                needs_login: stack.needs_login,
                commands: std::mem::take(&mut stack.commands),
                sessions: std::mem::take(&mut tabs),
                focused,
                startup_warnings: std::mem::take(&mut warnings),
                storage: storage.clone(),
                config: stack.config.agent.clone(),
                ui_config: stack.config.ui.clone(),
                input_history_size: stack.config.storage.input_history_size,
                max_log_files: stack.config.storage.max_log_files,
                permissions: Arc::new(
                    caudra_agent::permissions::PermissionManager::new_persistent(
                        stack.config.permissions.clone(),
                        cwd.clone(),
                        stack.plugin_host.plugin_rules(),
                    ),
                ),
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
            },
            initial_prompt.take(),
        )
        .context("run UI")?;

        match outcome {
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
                    reporter.shutdown();
                }
                // Returning the code instead of exiting here keeps every guard
                // alive to its scope end, including the ephemeral state root
                // whose `Drop` erases the volatile directory.
                return Ok(code);
            }
            RunOutcome::Reload {
                tabs: reloaded,
                focused: f,
            } => {
                let started = Instant::now();
                let last_good = (
                    stack.config.clone(),
                    stack.model.clone(),
                    Arc::clone(&stack.prompt_profiles),
                    stack.default_prompt_profile.clone(),
                );
                // Shut the old host down first so nothing can repopulate
                // the registry after the clear: its senders disconnect, the
                // watchdog aborts in-flight callbacks, and only this thread
                // issues loads. The old VM then shares nothing with the new
                // stack, so its slow join (up to 2s) can run on a
                // background thread.
                stack.plugin_host.begin_shutdown();
                ToolRegistry::global().clear_lua();
                teardown.defer(move || drop(stack));
                let (new_stack, new_warnings) = build_stack(&cli, &cwd, &storage, Some(last_good))?;
                tabs = reloaded;
                if tabs.is_empty() {
                    let session = AppSession::new(&new_stack.model.spec(), &cwd_str);
                    let lease = Arc::new(SessionLease::acquire(&storage, session.id)?);
                    setup::report_session_start(caudra_otel::emit::START_FRESH, Some(session.id));
                    tabs.push(SessionTab { session, lease });
                }
                sweeper =
                    RetentionSweeper::spawn(storage.clone(), new_stack.config.storage.retention);
                stack = new_stack;
                warnings = new_warnings;
                focused = f.min(tabs.len() - 1);
                tracing::info!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    tabs = tabs.len(),
                    "reload: rebuilt plugins and config"
                );
            }
        }
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
    use caudra_config::RawConfig;
    use color_eyre::eyre::eyre;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use test_case::test_case;

    const TEST_MODEL: &str = "test/model";
    const TEST_CWD: &str = "/tmp";
    const EXIT_RUN_TIME: Duration = Duration::from_secs(90);
    const SCRIPTABLE: &str = "a redirected stderr gets one line a script can act on";
    const BLOCK: &str = "a terminal gets the full block, not the fallback line";
    const DUPLICATE_TAB_WARNING: &str = "is duplicated";
    const MISSING_TAB_WARNING: &str = "no longer exists";
    const WRONG_CWD_WARNING: &str = "belongs to";

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

        let sweeper = RetentionSweeper::spawn(storage, RetentionConfig::default());

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
        let first =
            resolve_sessions(true, None, TEST_MODEL, Path::new("/project"), &storage).unwrap();

        let error = resolve_sessions(true, None, TEST_MODEL, Path::new("/project"), &storage)
            .err()
            .unwrap();

        assert!(error.to_string().contains("already open"), "{error}");
        drop(first);
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

        let resolved = resolve_sessions(true, None, TEST_MODEL, &workspace, &storage).unwrap();

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

        let resolved = resolve_sessions(true, None, TEST_MODEL, &workspace, &storage).unwrap();

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

    /// `--no-plugins` keeps the Lua host live but skips user `init.lua`, so
    /// a broken project `init.lua` must not be executed in that mode.
    #[test]
    fn no_plugins_skips_broken_init_lua_but_keeps_host_alive() {
        use caudra_agent::tools::ToolRegistry;
        use clap::Parser;
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let caudra_dir: PathBuf = dir.path().join(".caudra");
        fs::create_dir_all(&caudra_dir).expect("mkdir .caudra");
        fs::write(
            caudra_dir.join("init.lua"),
            "error('broken init lua must not run')",
        )
        .expect("write init.lua");

        let cli = Cli::parse_from(["caudra", "--no-plugins"]);
        assert!(cli.no_plugins);

        let mut plugin_host = PluginHost::with_jit(Arc::new(ToolRegistry::new()), true)
            .expect("live host boots under --no-plugins");

        let config = load_config(&plugin_host, &cli, dir.path())
            .expect("no-plugins must skip the broken init.lua and still load defaults");
        assert!(
            !config.plugins.names.is_empty(),
            "default builtin plugins must still be enabled under --no-plugins"
        );

        plugin_host
            .load_production_builtins(&config.plugins)
            .expect("builtins load on the live host under --no-plugins");

        plugin_host.begin_shutdown();
    }

    /// Negative control for the test above: without `--no-plugins`, the
    /// same broken `init.lua` must surface as an error so the skip path
    /// cannot silently regress into a tautology.
    #[test]
    fn broken_init_lua_errors_without_no_plugins() {
        use caudra_agent::tools::ToolRegistry;
        use clap::Parser;
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let caudra_dir: PathBuf = dir.path().join(".caudra");
        fs::create_dir_all(&caudra_dir).expect("mkdir .caudra");
        fs::write(
            caudra_dir.join("init.lua"),
            "error('broken init lua must not run')",
        )
        .expect("write init.lua");

        let cli = Cli::parse_from(["caudra"]);
        assert!(!cli.no_plugins);

        let mut plugin_host =
            PluginHost::with_jit(Arc::new(ToolRegistry::new()), true).expect("live host boots");

        match load_config(&plugin_host, &cli, dir.path()) {
            Err(_) => {}
            Ok(_) => panic!("broken init.lua must error without --no-plugins"),
        }

        plugin_host.begin_shutdown();
    }
}
