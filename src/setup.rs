use std::fmt::Display;
use std::sync::Mutex;

use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};

use caudra_providers::manifest::ManifestRegistry;
use caudra_providers::model::{Model, ModelError, ModelTier};
use caudra_providers::{HistoryItem, active_history_items, resolve_history_head};
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::log::RotatingFileWriter;
use caudra_storage::model::read_model;
use tracing_subscriber::EnvFilter;

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
    caudra_agent::load_stored_session(id, storage).context("load persisted session")
}

pub fn latest_session(cwd: &str, storage: &StateDir) -> Result<Option<StoredSession>> {
    caudra_agent::latest_stored_session(cwd, storage).context("load latest persisted session")
}

pub fn resolve_model(
    explicit: Option<&str>,
    provider_config: &caudra_config::ProviderConfig,
    storage: &StateDir,
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
    if let Some(spec) = read_model(storage) {
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
    for tier in [ModelTier::Strong, ModelTier::Medium] {
        for &slug in PROVIDER_PRIORITY {
            if caudra_providers::provider::provider_available(slug)
                && let Ok(model) = Model::from_tier(slug, tier)
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

/// Headless runs without a session id still count, they just stay
/// unattributed.
pub fn report_session_start(start_type: &'static str, session_id: Option<impl Display>) {
    let id = session_id.map(|id| id.to_string());
    caudra_otel::emit::session_started(start_type, id.as_deref());
}

pub fn init_logging(storage_config: &caudra_config::StorageConfig) {
    let Ok(writer) =
        RotatingFileWriter::new(storage_config.max_log_bytes, storage_config.max_log_files)
    else {
        return;
    };
    let writer = Mutex::new(writer);
    let filter = EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(writer)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_agent::{History, ToolOutput};
    use caudra_providers::{ContentBlock, Message, Role, TokenUsage, project_messages};
    use caudra_storage::sessions::{SESSIONS_DIR, Session, SessionError};

    const CWD: &str = "/repo";
    const MAIN_PROMPT: &str = "main prompt";
    const MODEL_SPEC: &str = "anthropic/test-model";
    const SUBAGENT_PROMPT: &str = "subagent prompt";
    const TITLE: &str = "Migrated session";
    const CURRENT_LOG_VERSION: &str = r#""v":3"#;
    const PREVIOUS_LOG_VERSION: &str = r#""v":2"#;
    const PREVIOUS_LOG_FORMAT_VERSION: u32 = 2;

    type LegacySession = Session<Message, TokenUsage, ToolOutput>;

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
    fn legacy_session_load_expands_all_message_collections() {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let messages = messages();
        let expected = serde_json::to_value(&messages).unwrap();
        let mut session = LegacySession::new(MODEL_SPEC, CWD);
        let id = session.id;
        session.set_title(TITLE.into());
        session.replace_messages(messages);
        session.set_subagent_messages("task-1".into(), vec![Message::user(SUBAGENT_PROMPT.into())]);
        let sessions_dir = storage.path().join(SESSIONS_DIR);
        std::fs::create_dir_all(&sessions_dir).unwrap();
        session.save_to(&sessions_dir).unwrap();
        let path = sessions_dir.join(format!("{id}.jsonl"));
        let data = std::fs::read_to_string(&path).unwrap();
        assert!(data.contains(CURRENT_LOG_VERSION));
        std::fs::write(
            &path,
            data.replacen(CURRENT_LOG_VERSION, PREVIOUS_LOG_VERSION, 1),
        )
        .unwrap();

        assert!(matches!(
            StoredSession::load(id, &storage),
            Err(SessionError::VersionMismatch {
                found: PREVIOUS_LOG_FORMAT_VERSION,
                ..
            })
        ));

        let loaded = load_session(id, &storage).unwrap();

        assert_eq!(loaded.title, TITLE);
        assert_eq!(
            serde_json::to_value(project_messages(loaded.messages()).unwrap()).unwrap(),
            expected
        );
        let subagent = project_messages(&loaded.subagent_messages()["task-1"]).unwrap();
        assert_eq!(subagent[0].user_text(), Some(SUBAGENT_PROMPT));
        assert!(
            loaded
                .messages()
                .windows(2)
                .all(|pair| pair[1].parent_id == Some(pair[0].id))
        );
        assert!(StoredSession::load(id, &storage).is_ok());
        assert_eq!(latest_session(CWD, &storage).unwrap().unwrap().id, id);
    }
}
