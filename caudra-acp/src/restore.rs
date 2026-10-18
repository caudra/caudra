use std::collections::HashMap;
use std::path::{Path, PathBuf};

use agent_client_protocol_schema::v1::{Error as AcpError, SessionUpdate};
use caudra_agent::agent::estimate_message_tokens;
use caudra_agent::tools::ToolRegistry;
use caudra_agent::tools::native::plan::PlanTarget;
use caudra_agent::{AgentMode, History, open_stored_session};
use caudra_config::ModelPolicy;
use caudra_providers::model::Model;
use caudra_providers::{
    HistoryItem, TokenUsage, active_history_items, resolve_history_head, settle_session,
};
use caudra_storage::id::{CaudraId, SessionRef};
use caudra_storage::permission_state::PermissionRuleRecord;
use caudra_storage::sessions::{PermissionMode, StoredMode, StoredPlanTarget, StoredTokenUsage};
use caudra_storage::thinking::StoredThinking;
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_storage::{StateDir, paths};
use caudra_workspace::LocalDocumentRef;
use tracing::warn;

use crate::errors::{internal, json_str};
use crate::listing::rfc3339;
use crate::{AcpRuntime, translate};

#[derive(Debug)]
pub(crate) struct Restored {
    pub(crate) history: Vec<HistoryItem>,
    /// Only set when the session recorded an absolute cwd.
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) usage: TokenUsage,
    pub(crate) by_model: HashMap<String, StoredTokenUsage>,
    pub(crate) model: String,
    pub(crate) structured_permission_rules: Vec<PermissionRuleRecord>,
    pub(crate) permission_mode: Option<PermissionMode>,
    pub(crate) system_prompt_profile: Option<String>,
    pub(crate) write_version: Option<i64>,
    pub(crate) workspace_binding: Option<StoredWorkspaceBinding>,
    pub(crate) mode: Option<StoredMode>,
    pub(crate) plan_target: Option<StoredPlanTarget>,
    pub(crate) plan_path: Option<String>,
    pub(crate) thinking: Option<StoredThinking>,
    pub(crate) fast: bool,
    pub(crate) title: String,
    /// Unix seconds.
    pub(crate) updated_at: u64,
}

impl Restored {
    /// The stored name the client is told once the session is open, unless
    /// the session was never named.
    pub(crate) fn info(&self) -> Option<SessionUpdate> {
        let title = self.title.trim();
        (!title.is_empty())
            .then(|| translate::session_info(title.to_owned(), rfc3339(self.updated_at)))
    }
}

/// Remote cwd stays logical; only local legacy relative recordings fall back
/// to the caller's cwd when replayed.
pub(crate) fn load_history_from(
    storage: &StateDir,
    session_id: CaudraId,
) -> Result<Restored, AcpError> {
    let session = open_stored_session(session_id, storage).map_err(|e| {
        AcpError::resource_not_found(Some(format!("session/{session_id}"))).data(json_str(&e))
    })?;
    let recorded = if session
        .workspace_binding()
        .is_some_and(|binding| !binding.is_local())
        || Path::new(&session.cwd).is_absolute()
    {
        Some(PathBuf::from(&session.cwd))
    } else {
        None
    };
    let head = resolve_history_head(
        session.messages(),
        session.meta.history_head,
        session.meta.pending_revert.is_some(),
    );
    let history = active_history_items(session.messages(), head).map_err(|e| internal(&e))?;
    Ok(Restored {
        cwd: recorded,
        usage: session.token_usage,
        by_model: session.usage_by_model().clone(),
        model: session.model.clone(),
        structured_permission_rules: session.meta.structured_permission_rules.clone(),
        permission_mode: session.meta.permission_mode.clone(),
        system_prompt_profile: session.meta.system_prompt_profile.clone(),
        write_version: session.persisted_write_version(),
        workspace_binding: session.workspace_binding().cloned(),
        mode: session.meta.mode,
        plan_target: session.meta.plan_target.clone(),
        plan_path: session.meta.plan_path.clone(),
        thinking: session.meta.thinking.clone(),
        fast: session.meta.fast,
        title: session.title.clone(),
        updated_at: session.updated_at,
        history,
    })
}

/// What shows a stored conversation to the client that just opened it.
/// Relative paths resolve against `cwd`.
pub(crate) fn replay(registry: &ToolRegistry, history: &History, cwd: &Path) -> Vec<SessionUpdate> {
    translate::replay_history(registry, history, cwd, paths::home().as_deref())
}

/// Replays a session this process already runs, from what it has stored so far.
pub(crate) fn replay_stored(
    registry: &ToolRegistry,
    storage: &StateDir,
    session: CaudraId,
    request_cwd: &Path,
) -> Result<Vec<SessionUpdate>, AcpError> {
    let restored = load_history_from(storage, session)?;
    let history = History::restored(restored.history).map_err(|e| internal(&e))?;
    Ok(replay(
        registry,
        &history,
        restored.cwd.as_deref().unwrap_or(request_cwd),
    ))
}

/// The model the session was recorded on, unless policy now forbids it or it
/// no longer resolves; then the server's own model carries the session on.
pub(crate) fn stored_model(policy: &ModelPolicy, recorded: &str, fallback: &Model) -> Model {
    if policy.allows(recorded)
        && let Ok(model) = Model::from_spec(recorded)
    {
        return model;
    }
    warn!(
        recorded,
        fallback = %fallback.spec(),
        "stored session model is unavailable; loading on the default model"
    );
    fallback.clone()
}

/// Priced against the model the session recorded, not the one selected now
/// (which may cost 10x more or less). Later turns add their own exact cost.
pub(crate) fn restored_cost(restored: &mut Restored, fallback: &Model) -> Option<f64> {
    let recorded = Model::from_spec(&restored.model).unwrap_or_else(|_| fallback.clone());
    let fast = restored.fast && recorded.supports_fast();
    settle_session(&restored.usage, &mut restored.by_model, &recorded, fast).billed
}

/// The gauge and spend a reopened session starts from. Nothing stores the
/// last measured context, so it is estimated from the history, as the TUI does.
pub(crate) fn restored_usage(history: &History, model: &Model, cost: Option<f64>) -> SessionUpdate {
    translate::context_usage(
        estimate_message_tokens(history.as_slice()),
        model.context_window,
        cost,
    )
}

/// The stored plan if this session can still use it, whatever mode it was
/// saved in: a remote plan it can read, a local plan file that exists, or a
/// legacy local plan adopted into the remote workspace's document store.
pub(crate) fn restored_plan(
    target: Option<&StoredPlanTarget>,
    legacy_path: Option<&str>,
    runtime: &AcpRuntime,
    session: &SessionRef,
) -> Option<PlanTarget> {
    let path = match target {
        Some(StoredPlanTarget::PlanRef { reference }) => {
            let workspace = runtime.workspace_session.as_ref()?;
            return runtime
                .local_documents
                .as_ref()?
                .read(
                    workspace.binding().project().key(),
                    Some(session.as_str()),
                    &LocalDocumentRef::Plan(reference.clone()),
                )
                .ok()
                .map(|_| PlanTarget::Remote(reference.clone()));
        }
        Some(StoredPlanTarget::LocalPath { path }) => Path::new(path),
        None => Path::new(legacy_path?),
    };
    match &runtime.workspace_session {
        Some(workspace) => runtime
            .local_documents
            .as_ref()?
            .adopt_legacy_plan(workspace.binding().project().key(), session.as_str(), path)
            .ok()
            .map(PlanTarget::Remote),
        None => path
            .is_file()
            .then(|| PlanTarget::Local(path.to_path_buf())),
    }
}

pub(crate) fn restored_mode(stored: Option<StoredMode>, plan: Option<&PlanTarget>) -> AgentMode {
    match (stored, plan) {
        (Some(StoredMode::Plan), Some(plan)) => AgentMode::planning(plan.clone()),
        _ => AgentMode::Build,
    }
}

#[cfg(test)]
mod tests {
    use caudra_agent::ToolOutput;
    use caudra_agent::permissions::PermissionRequest;
    use caudra_providers::{ContentBlock as MsgBlock, Message, Role, expand_message};
    use caudra_storage::sessions::Session;
    use caudra_storage::usage_ledger::LedgerPurpose;
    use caudra_workspace::PlanRef;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::test_support::{
        OFFLINE_SPEC, PLAN_FILE, SELECTED_SPEC, history_items, stored_local_plan,
    };

    /// Neither resolves in the price tables, so nothing can re-price a restored
    /// session back onto the recorded number by luck.
    const RETIRED_SPEC: &str = "retired-vendor/retired-model-9000";
    const RETIRED_MODEL_ID: &str = "retired-model-9000";
    const RECORDED_COST: f64 = 1.25;
    const REMOTE_PLAN_REF: &str = "plan-test";
    const RESTORED_PROMPT: &str = "rename foo to bar";

    #[test_case(true; "missing_local_file")]
    #[test_case(false; "remote_plan_without_a_workspace")]
    fn an_unusable_stored_plan_is_not_bound(local: bool) {
        let temp = TempDir::new().unwrap();
        let target = if local {
            stored_local_plan(&temp.path().join(PLAN_FILE))
        } else {
            StoredPlanTarget::PlanRef {
                reference: PlanRef::new(REMOTE_PLAN_REF).unwrap(),
            }
        };
        let session_ref = SessionRef::from(CaudraId::generate());
        let plan = restored_plan(Some(&target), None, &AcpRuntime::default(), &session_ref);
        assert_eq!(plan, None);
        assert_eq!(
            restored_mode(Some(StoredMode::Plan), None),
            AgentMode::Build
        );
    }

    #[test_case(OFFLINE_SPEC, OFFLINE_SPEC ; "recorded_model_resumes")]
    #[test_case(RETIRED_SPEC, SELECTED_SPEC ; "unresolvable_model_falls_back")]
    fn a_loaded_session_starts_on_its_recorded_model(recorded: &str, expected: &str) {
        let fallback = Model::from_spec(SELECTED_SPEC).unwrap();
        let model = stored_model(&ModelPolicy::default(), recorded, &fallback);
        assert_eq!(model.spec(), expected);
    }

    #[test_case(Some(RECORDED_COST) ; "with_spend")]
    #[test_case(None ; "without_spend")]
    fn a_reopened_session_reports_its_estimated_context_and_spend(cost: Option<f64>) {
        let history = History::new(vec![Message::user(RESTORED_PROMPT.into())]);
        let model = Model::from_spec(OFFLINE_SPEC).unwrap();

        let SessionUpdate::UsageUpdate(usage) = restored_usage(&history, &model, cost) else {
            panic!("a reopened session reports usage");
        };

        assert_eq!(
            usage.used,
            u64::from(estimate_message_tokens(history.as_slice()))
        );
        assert_eq!(usage.size, u64::from(model.context_window));
        assert_eq!(usage.cost.map(|cost| cost.amount), cost);
    }

    #[test]
    fn load_history_round_trips_stored_items() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let messages = vec![
            Message::user("rename foo to bar".into()),
            Message {
                role: Role::Assistant,
                content: vec![MsgBlock::Text {
                    text: "done".into(),
                }],
                display_text: None,
                ..Default::default()
            },
        ];
        let items = history_items(&messages);
        let mut session: Session<HistoryItem, TokenUsage, ToolOutput> =
            Session::new("anthropic/test-model", "/project");
        session.replace_messages(items.clone());
        session.token_usage = TokenUsage {
            input: 1_000,
            output: 200,
            ..Default::default()
        };
        let request = PermissionRequest::from_legacy(
            "stored-structured".into(),
            caudra_config::ToolKey::native("bash"),
            vec!["cargo test".into()],
            serde_json::json!({"command": "cargo test"}),
            Path::new("/project"),
            false,
        );
        session.meta.structured_permission_rules = vec![
            PermissionRuleRecord::conversation(
                request
                    .option_rule(
                        "allow_exact",
                        caudra_agent::permissions::PermissionLifetime::Conversation,
                    )
                    .unwrap(),
            )
            .unwrap(),
        ];
        session.meta.permission_mode = Some(PermissionMode::Yolo);
        session.meta.system_prompt_profile = Some("review".into());
        session.save(&dir).unwrap();

        let id: CaudraId = session.id;
        let restored = load_history_from(&dir, id).unwrap();
        assert_eq!(restored.model, "anthropic/test-model");
        assert_eq!(restored.history, items);
        assert_eq!(restored.cwd, Some(PathBuf::from("/project")));
        assert_eq!(restored.usage, session.token_usage);
        assert_eq!(
            restored.structured_permission_rules,
            session.meta.structured_permission_rules
        );
        assert_eq!(restored.permission_mode, Some(PermissionMode::Yolo));
        assert_eq!(restored.system_prompt_profile.as_deref(), Some("review"));
    }

    #[test_case(None; "unset")]
    #[test_case(Some(PermissionMode::Ask); "ask")]
    #[test_case(Some(PermissionMode::Auto); "auto")]
    #[test_case(Some(PermissionMode::Yolo); "yolo")]
    fn restored_permission_mode_preserves_stored_values(mode: Option<PermissionMode>) {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let mut session: Session<HistoryItem, TokenUsage, ToolOutput> =
            Session::new(OFFLINE_SPEC, "/project");
        session.meta.permission_mode = mode.clone();
        session.save(&storage).unwrap();
        assert_eq!(
            load_history_from(&storage, session.id)
                .unwrap()
                .permission_mode,
            mode
        );
    }

    #[test]
    fn load_history_returns_only_the_persisted_active_path() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let root = history_items(&[Message::user("root".into())])[0].clone();
        let abandoned =
            expand_message(&Message::user("abandoned".into()), Some(root.id))[0].clone();
        let branch = expand_message(&Message::user("branch".into()), Some(root.id))[0].clone();
        let mut session: Session<HistoryItem, TokenUsage, ToolOutput> =
            Session::new("anthropic/test-model", "/project");
        session.replace_messages(vec![root.clone(), abandoned, branch.clone()]);
        session.meta.history_head = Some(branch.id);
        session.save(&dir).unwrap();

        let restored = load_history_from(&dir, session.id).unwrap();

        assert_eq!(restored.history, [root, branch]);
    }

    /// Resuming must bill what the session actually paid. If `by_model` came
    /// back empty or lost its recorded costs, ACP would re-price the restored
    /// total against today's table and disagree with the TUI.
    #[test]
    fn load_history_prices_a_resumed_session_at_what_it_paid() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let mut session: Session<HistoryItem, TokenUsage, ToolOutput> =
            Session::new(RETIRED_SPEC, "/project");
        session.token_usage = TokenUsage {
            input: 1_000_000,
            output: 200_000,
            ..Default::default()
        };
        session.add_model_usage(
            RETIRED_MODEL_ID,
            LedgerPurpose::Chat,
            StoredTokenUsage {
                input: 1_000_000,
                output: 200_000,
                cost: Some(RECORDED_COST),
                ..Default::default()
            },
        );
        session.save(&dir).unwrap();

        let mut restored = load_history_from(&dir, session.id).unwrap();
        assert_eq!(
            restored.by_model[RETIRED_MODEL_ID].cost,
            Some(RECORDED_COST),
            "the per-model breakdown survives the file"
        );

        // The recorded spec no longer parses, so the selected model stands
        // in, and that must not change the bill.
        let selected = Model::from_spec(SELECTED_SPEC).expect("a shipped model");
        assert_eq!(restored_cost(&mut restored, &selected), Some(RECORDED_COST));
    }

    #[test]
    fn remote_history_keeps_logical_cwd_and_rejects_embedded_resume() {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().into());
        let local = StoredWorkspaceBinding::local_from_cwd(".");
        let remote: StoredWorkspaceBinding = serde_json::from_str(
            &serde_json::to_string(&local)
                .unwrap()
                .replace("caudra:local:v1", "https://remote.example"),
        )
        .unwrap();
        let mut session = caudra_agent::StoredSession::new_with_workspace(
            "anthropic/test",
            "nested/child",
            remote.clone(),
        );
        session.save(&storage).unwrap();
        let restored = load_history_from(&storage, session.id).unwrap();
        assert_eq!(restored.cwd, Some(PathBuf::from("nested/child")));
        assert!(
            StoredWorkspaceBinding::validate_resume_identity(
                restored.workspace_binding.as_ref(),
                None
            )
            .is_err()
        );
        assert!(
            StoredWorkspaceBinding::validate_resume_identity(Some(&local), Some(&remote)).is_err()
        );
    }

    #[test]
    fn load_history_records_absolute_cwd_only() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let mut session: Session<HistoryItem, TokenUsage, ToolOutput> =
            Session::new("anthropic/test-model", "relative/project");
        session.save(&dir).unwrap();
        assert_eq!(load_history_from(&dir, session.id).unwrap().cwd, None);
    }

    #[test]
    fn load_missing_session_is_resource_not_found() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let err = load_history_from(&dir, CaudraId::generate()).unwrap_err();
        assert_eq!(err.code, AcpError::resource_not_found(None).code);
    }
}
