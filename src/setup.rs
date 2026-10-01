use std::fmt::Display;
use std::sync::Arc;
use std::time::Duration;

use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};

use caudra_providers::manifest::ManifestRegistry;
use caudra_providers::model::{Model, ModelError, ModelPurpose};
use caudra_providers::{HistoryItem, active_history_items, resolve_history_head};
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::log::{LogSinkGuard, RotatingFileWriter};
use caudra_storage::model::read_model;
use caudra_storage::sessions::change_stores;
use caudra_storage::sessions::{StoredMode, set_eager_load_limit};
use caudra_workcell::LocalChangeStores;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

const RUST_LOG: &str = "RUST_LOG";
const MAX_EAGER_LOAD_MB: &str = "CAUDRA_MAX_EAGER_LOAD_MB";
const EVENT_STARTED: &str = "caudra_started";
pub const MODE_TUI: &str = "tui";
pub const MODE_ACP: &str = "acp";
/// Long enough for a queued burst to reach the disk, short enough that a
/// wedged filesystem does not hold up exit.
const LOG_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

const PROVIDER_PRIORITY: &[&str] = &[
    "anthropic",
    "openai",
    "xai",
    "copilot",
    "zai",
    "synthetic",
    "deepseek",
];

pub type StoredSession = caudra_agent::StoredSession;

pub fn session_history_head(session: &StoredSession) -> Option<CaudraId> {
    resolve_history_head(
        session.messages(),
        session.meta.history_head,
        session.meta.pending_revert.is_some(),
    )
}

pub fn active_session_history(session: &StoredSession) -> Result<Vec<HistoryItem>> {
    active_history_items(session.messages(), session_history_head(session))
        .context("resolve active session history")
}

pub fn load_session(id: CaudraId, storage: &StateDir) -> Result<StoredSession> {
    caudra_agent::open_stored_session(id, storage).context("load persisted session")
}

/// The resume paths, which hand the cursor to the storage writer so the first
/// save of the session is a delta rather than a rewrite of every payload.
pub fn load_session_with_cursor(
    id: CaudraId,
    storage: &StateDir,
) -> Result<(StoredSession, caudra_storage::sessions::SessionCursor)> {
    caudra_agent::open_stored_session_with_cursor(id, storage).context("load persisted session")
}

/// The model a run opens on. `mode` picks which remembered choice answers, since
/// plan and build each keep their own.
pub fn resolve_model(
    explicit: Option<&str>,
    provider_config: &caudra_config::ProviderConfig,
    storage: &StateDir,
    mode: StoredMode,
) -> Result<Model> {
    let policy = &provider_config.model_policy;
    if let Some(spec) = explicit {
        if !policy.allows(spec) {
            return Err(eyre!(
                "model {spec:?} is not allowed by provider model policy"
            ));
        }
        return from_spec_or_warm_catalog(spec).context("invalid --model spec");
    }
    if let Some(spec) = read_model(storage, mode) {
        if policy.allows(&spec)
            && let Ok(m) = from_spec_or_warm_catalog(&spec)
        {
            return Ok(m);
        }
        tracing::warn!(
            spec,
            "saved model unavailable or disallowed, falling back to default"
        );
    }
    if let Some(spec) = provider_config.default_model.as_deref() {
        if !policy.allows(spec) {
            return Err(eyre!(
                "default model {spec:?} is not allowed by provider model policy"
            ));
        }
        return from_spec_or_warm_catalog(spec).context("invalid default_model in config");
    }
    auto_detect_model(policy).ok_or_else(|| {
        let policy_note = if policy.is_restrictive() {
            "\nnote: an allowed_models/excluded_models policy is active and may exclude every candidate"
        } else {
            ""
        };
        color_eyre::eyre::eyre!(
            "no provider available - set an API key (e.g. ANTHROPIC_API_KEY), run `caudra auth login`, or use -m to specify a model{policy_note}\n\nSee https://caudra.ai/docs/providers/ for setup instructions"
        )
    })
}

/// An unknown slug may just mean the models.dev catalog has not been loaded
/// yet, so retry once with a warm catalog. `Model::from_spec` itself must stay
/// non-blocking: the UI draws with it.
fn from_spec_or_warm_catalog(spec: &str) -> Result<Model, ModelError> {
    match Model::from_spec(spec) {
        Err(ModelError::UnsupportedProvider(_)) => {
            caudra_providers::warm_catalog();
            Model::from_spec(spec)
        }
        result => result,
    }
}

fn auto_detect_model(policy: &caudra_config::ModelPolicy) -> Option<Model> {
    for purpose in [ModelPurpose::Best, ModelPurpose::Fast] {
        for &slug in PROVIDER_PRIORITY {
            if caudra_providers::provider::provider_available(slug)
                && let Some(model) = Model::curated_default(slug, purpose)
                && policy.allows(&model.spec())
            {
                return Some(model);
            }
        }
    }
    None
}

/// Built-in slugs keep their compiled protocol, model catalog and auth wiring,
/// so a `providers.toml` entry setting those fields is only partly honored
/// (#597). Call this after `init_logging`, otherwise the warning has no
/// subscriber to reach.
pub fn warn_ignored_provider_fields() {
    for (slug, def) in &caudra_config::providers::ProvidersConfig::load().providers {
        if ManifestRegistry::get(slug).is_none() {
            continue;
        }
        let ignored = caudra_config::providers::ignored_builtin_fields(slug, def);
        if ignored.is_empty() {
            continue;
        }
        tracing::warn!(
            slug,
            fields = %ignored.join(", "),
            "providers.toml entry for built-in provider ignores these fields \
             (base_url/plan/api_key still apply), use a custom slug to set \
             protocol or models"
        );
    }
}

pub fn install_panic_log_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_owned()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic payload".into()
        };
        let location = info.location().map(|l| l.to_string());
        tracing::error!(
            panic.payload = %payload,
            panic.location = location.as_deref().unwrap_or("<unknown>"),
            "panic occurred"
        );
        // The writer thread is asynchronous, and an abort after this hook would
        // drop the one record that explains the crash.
        caudra_storage::log::flush_blocking(LOG_FLUSH_TIMEOUT);
        prev(info);
    }));
}

/// Telemetry is opt-in and must never stop caudra from starting, so a bad
/// setting is a warning in the log, not an error to the user. Call this after
/// `init_logging` or the warning has nowhere to go.
pub fn init_telemetry(config: &caudra_config::TelemetryConfig) {
    if let Err(error) = caudra_otel::init(config) {
        tracing::warn!(%error, "telemetry disabled");
    }
}

/// Pushes the configured session load ceiling into storage, which cannot read
/// the config itself: `caudra-config` depends on `caudra-storage`. Call this
/// after `init_logging` and before anything opens a session, so a rejected
/// override has somewhere to complain.
pub fn apply_storage_limits(storage_config: &caudra_config::StorageConfig) {
    let configured = storage_config.max_eager_load_bytes;
    let megabytes = match std::env::var(MAX_EAGER_LOAD_MB) {
        Err(_) => configured / (1024 * 1024),
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(parsed) if parsed >= caudra_config::MIN_MAX_EAGER_LOAD_MB => parsed,
            _ => {
                tracing::warn!(
                    env = MAX_EAGER_LOAD_MB,
                    value = %raw,
                    minimum = caudra_config::MIN_MAX_EAGER_LOAD_MB,
                    "ignoring unusable session load ceiling override"
                );
                configured / (1024 * 1024)
            }
        },
    };
    set_eager_load_limit(megabytes.saturating_mul(1024 * 1024) as usize);
}

/// Hands storage the local change stores, which it cannot reach itself:
/// `caudra-workcell` depends on it. Deleting or trimming a session queues the
/// release of its records, and the job waits for a process that has done
/// this. Call it once, after `init_logging`.
pub fn register_change_stores() {
    match LocalChangeStores::new() {
        Ok(stores) => change_stores::register_change_stores(Arc::new(stores)),
        Err(error) => tracing::warn!(%error, "change stores unreachable; record releases wait"),
    }
}

/// Headless runs without a session id still count, they just stay
/// unattributed.
pub fn report_session_start(start_type: &'static str, session_id: Option<impl Display>) {
    let id = session_id.map(|id| id.to_string());
    caudra_otel::emit::session_started(start_type, id.as_deref());
}

/// Keeps the writer thread alive for as long as the command runs. Dropping it
/// drains the queue and stops the thread.
pub struct LoggingGuard(#[expect(dead_code, reason = "held for its Drop")] Option<LogSinkGuard>);

/// Installs the file sink and the telemetry layer as separate layers with
/// separate filters. `RUST_LOG` must not be able to switch telemetry off, and a
/// log file that cannot be opened must not take the telemetry layer with it.
pub fn init_logging(storage_config: &caudra_config::StorageConfig) -> LoggingGuard {
    let sink = RotatingFileWriter::new(storage_config.max_log_bytes, storage_config.max_log_files)
        .ok()
        .map(|writer| {
            caudra_storage::log::spawn(writer, caudra_storage::log::DEFAULT_QUEUE_CAPACITY)
        });
    let (writer, guard) = match sink {
        Some((writer, guard)) => (Some(writer), Some(guard)),
        None => (None, None),
    };

    let file_layer = writer.map(|writer| {
        tracing_subscriber::fmt::layer()
            .json()
            .with_writer(move || writer.clone())
            .with_filter(file_filter(storage_config.log_level))
    });

    tracing_subscriber::registry()
        .with(file_layer)
        .with(caudra_otel::layer().with_filter(caudra_otel::telemetry_targets()))
        .init();

    LoggingGuard(guard)
}

/// `RUST_LOG` still wins so a one-off debugging session needs no config edit.
fn file_filter(level: caudra_config::LogLevel) -> EnvFilter {
    EnvFilter::try_from_env(RUST_LOG).unwrap_or_else(|_| EnvFilter::new(level.as_str()))
}

/// The first line of every run. Reading a log without knowing the version, the
/// model, and the directory costs more time than writing it does.
pub fn report_startup(mode: &'static str, model: &Model, cwd: &std::path::Path) {
    tracing::info!(
        target: caudra_storage::log::target::AGENT,
        event = EVENT_STARTED,
        version = env!("CARGO_PKG_VERSION"),
        mode,
        model = %model.id,
        provider = %model.provider,
        cwd = %cwd.display(),
        pid = std::process::id(),
        "caudra started"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_agent::History;
    use caudra_providers::{ContentBlock, Message, Role, project_messages};

    const CWD: &str = "/repo";
    const MAIN_PROMPT: &str = "main prompt";
    const MODEL_SPEC: &str = "anthropic/test-model";
    const SUBAGENT_PROMPT: &str = "subagent prompt";
    const TITLE: &str = "Restored session";

    fn messages() -> Vec<Message> {
        vec![
            Message::user(MAIN_PROMPT.into()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "first".into(),
                    },
                    ContentBlock::Text {
                        text: "second".into(),
                    },
                ],
                ..Default::default()
            },
        ]
    }

    #[test]
    fn current_session_load_preserves_item_identity() {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let items = History::new(messages()).into_items();
        let mut session = StoredSession::new(MODEL_SPEC, CWD);
        let id = session.id;
        session.replace_messages(items.clone());
        session.save(&storage).unwrap();

        assert_eq!(load_session(id, &storage).unwrap().messages(), items);
    }

    #[test]
    fn load_session_restores_the_title_and_every_subagent_stream() {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let items = History::new(messages()).into_items();
        let mut session = StoredSession::new(MODEL_SPEC, CWD);
        let id = session.id;
        session.set_title(TITLE.into());
        session.replace_messages(items);
        session.set_subagent_messages(
            "task-1".into(),
            History::new(vec![Message::user(SUBAGENT_PROMPT.into())]).into_items(),
        );
        session.save(&storage).unwrap();

        let loaded = load_session(id, &storage).unwrap();

        assert_eq!(loaded.title, TITLE);
        assert_eq!(
            serde_json::to_value(project_messages(loaded.messages()).unwrap()).unwrap(),
            serde_json::to_value(messages()).unwrap()
        );
        let subagent = project_messages(&loaded.subagent_messages()["task-1"]).unwrap();
        assert_eq!(subagent[0].user_text(), Some(SUBAGENT_PROMPT));
        assert!(
            loaded
                .messages()
                .windows(2)
                .all(|pair| pair[1].parent_id == Some(pair[0].id))
        );
    }
}
