use std::path::PathBuf;
use std::sync::Arc;

use async_lock::Mutex;
use flume::Receiver;
use maki_config::{Effect, ModelPolicy, PermissionRule, ToolKey};
use maki_providers::Timeouts;
use maki_providers::model::Model;
use maki_providers::provider::{self, Provider};
#[cfg(test)]
use maki_providers::{ContentBlock, HistoryItemKind, Message, Role};
use maki_providers::{HistoryItem, merge_history_items};
use maki_storage::StateDir;
use maki_storage::id::{MakiId, SessionRef};
use maki_storage::permission_state::PermissionRuleRecord;
use maki_storage::sessions::{StoredEffect, StoredRule};
use serde_json::Value;
use tracing::{error, warn};

use crate::agent::{self, History};
use crate::cancel::{CancelMap, CancelToken};
use crate::permissions::{PermissionManager, PluginRuleStore};
use crate::prompt::ResolvedSlots;
use crate::template;
use crate::tools::{
    DescriptionContext, FileReadTracker, LocalTools, ToolAudience, ToolFilter, ToolRegistry,
};
use crate::{
    Agent, AgentConfig, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams, Envelope,
    EventSender, GoalHandle, ImageSource, McpHandle, McpSession, PermissionsConfig, SessionMailbox,
    StoredSession, SubagentHistorySnapshot, SubagentHistoryStore, ToolOutputLines,
    load_stored_session,
};

struct SessionStore {
    dir: StateDir,
    session: StoredSession,
    subagent_history: SubagentHistoryStore,
    persisted_subagent_history: SubagentHistorySnapshot,
}

impl SessionStore {
    fn open(session_id: MakiId, cwd: &str, model_spec: &str) -> Option<Self> {
        let dir = StateDir::resolve()
            .map_err(|e| warn!(error = %e, "state dir unavailable; session will not be persisted"))
            .ok()?;
        Some(Self::open_in(dir, session_id, cwd, model_spec))
    }

    fn open_in(dir: StateDir, session_id: MakiId, cwd: &str, model_spec: &str) -> Self {
        match load_stored_session(session_id, &dir) {
            Ok(session) => Self::from_session(dir, session),
            Err(_) => {
                let mut session = StoredSession::new(model_spec, cwd);
                session.id = session_id;
                let mut store = Self::from_session(dir, session);
                store.save();
                store
            }
        }
    }

    fn from_session(dir: StateDir, session: StoredSession) -> Self {
        let subagent_messages = session
            .subagent_messages()
            .iter()
            .filter_map(
                |(task_id, items)| match History::restored(items.as_ref().clone()) {
                    Ok(history) => Some((task_id.clone(), Arc::new(history.into_vec()))),
                    Err(error) => {
                        warn!(%task_id, %error, "failed to restore subagent history");
                        None
                    }
                },
            )
            .collect();
        let subagent_history = SubagentHistoryStore::seeded(subagent_messages);
        let persisted_subagent_history = subagent_history.snapshot();
        Self {
            dir,
            session,
            subagent_history,
            persisted_subagent_history,
        }
    }

    fn save(&mut self) {
        if let Err(e) = self.session.save(&self.dir) {
            warn!(error = %e, session_id = %self.session.id, "failed to persist session");
        }
    }

    fn sync_permissions(&mut self, permissions: &PermissionManager) {
        self.session.meta.session_rules = rules_to_stored(&permissions.session_rules_snapshot());
        self.session.meta.structured_permission_rules =
            permissions.structured_conversation_rules_snapshot();
        self.session.meta.yolo = permissions.persisted_yolo();
    }

    fn record_turn(
        &mut self,
        history: &History,
        model_spec: String,
        permissions: &PermissionManager,
    ) {
        let mut merged = self.session.messages().to_vec();
        if let Err(error) = merge_history_items(&mut merged, history.active_items()) {
            warn!(%error, "refusing to persist invalid history graph");
            return;
        }
        if merged.as_slice() != self.session.messages() {
            self.session.replace_messages(merged);
        }
        self.session
            .set_conversation_state(history.item_head(), None);
        self.session.set_model(model_spec);
        let snapshot = self.subagent_history.snapshot();
        if snapshot.revision() != self.persisted_subagent_history.revision() {
            for (task_id, history) in snapshot.histories() {
                let unchanged = self
                    .persisted_subagent_history
                    .histories()
                    .get(task_id)
                    .is_some_and(|persisted| Arc::ptr_eq(persisted, history));
                if !unchanged {
                    let items =
                        History::new(Arc::unwrap_or_clone(Arc::clone(history))).into_items();
                    self.session.set_subagent_messages(task_id.clone(), items);
                }
            }
            self.persisted_subagent_history = snapshot;
        }
        self.sync_permissions(permissions);
        self.session.update_title_if_default();
        self.save();
    }
}

fn rules_to_stored(rules: &[PermissionRule]) -> Vec<StoredRule> {
    rules
        .iter()
        .map(|rule| StoredRule {
            tool: rule.tool.to_string(),
            scope: rule.scope.clone(),
            effect: match rule.effect {
                Effect::Allow => StoredEffect::Allow,
                Effect::Deny => StoredEffect::Deny,
            },
        })
        .collect()
}

fn stored_to_rules(rules: &[StoredRule]) -> Vec<PermissionRule> {
    rules
        .iter()
        .filter_map(|rule| {
            let tool = ToolKey::parse(&rule.tool)
                .map_err(|error| {
                    warn!(tool = %rule.tool, %error, "skipping malformed stored permission rule")
                })
                .ok()?;
            Some(PermissionRule {
                tool,
                scope: rule.scope.clone(),
                effect: match rule.effect {
                    StoredEffect::Allow => Effect::Allow,
                    StoredEffect::Deny => Effect::Deny,
                },
            })
        })
        .collect()
}

pub struct HeadlessParams {
    pub model: Model,
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: Timeouts,
    pub prompt: String,
    pub images: Vec<ImageSource>,
    pub prompt_slots: ResolvedSlots,
    pub excluded_tools: Vec<&'static str>,
    pub mcp_handle: Option<McpHandle>,
    pub initial_wd: PathBuf,
    pub fast: bool,
    pub workflow: bool,
    pub model_policy: Arc<ModelPolicy>,
    pub plugin_rules: Arc<PluginRuleStore>,
    pub goal: GoalHandle,
}

pub struct HeadlessHandle {
    pub event_rx: Receiver<Envelope>,
    pub tool_names: Vec<String>,
    pub session_id: SessionRef,
    pub cwd: String,
    pub goal: GoalHandle,
    pub task: smol::Task<()>,
}

struct AgentSetup {
    vars: template::Vars,
    instructions: agent::Instructions,
    tools: Value,
}

fn setup(
    model: &Model,
    config: &AgentConfig,
    excluded_tools: &[&'static str],
    workflow: bool,
) -> AgentSetup {
    let vars = template::env_vars();
    let instructions = agent::load_instructions(&vars.apply("{cwd}"));
    let tools = tool_definitions(
        &vars,
        model,
        config,
        excluded_tools,
        workflow,
        ToolRegistry::global(),
    );

    AgentSetup {
        vars,
        instructions,
        tools,
    }
}

/// Base definitions only. MCP definitions are injected per request by
/// `Agent::request_tools`; storing them here would freeze the catalog.
fn tool_definitions(
    vars: &template::Vars,
    model: &Model,
    config: &AgentConfig,
    excluded_tools: &[&'static str],
    workflow: bool,
    registry: &ToolRegistry,
) -> Value {
    let filter = ToolFilter::from_config(config, model, excluded_tools);
    let ctx = DescriptionContext {
        filter: &filter,
        audience: ToolAudience::MAIN,
        workflow,
    };
    registry.definitions(vars, &ctx, model.supports_tool_examples())
}

/// Names advertised to SDK clients: base tools plus what the first request
/// would carry from MCP (always-load definitions and `tool_search`).
fn advertised_tool_names(tools: &Value, mcp: Option<&McpSession>) -> Vec<String> {
    let mut probe = tools.clone();
    if let Some(mcp) = mcp {
        mcp.extend_tools(&mut probe);
    }
    extract_tool_names(&probe)
}

pub fn spawn(params: HeadlessParams) -> HeadlessHandle {
    let working_dir = params.initial_wd.to_string_lossy().into_owned();
    let mode = AgentMode::Build;
    let AgentSetup {
        vars,
        instructions,
        tools,
    } = setup(
        &params.model,
        &params.config,
        &params.excluded_tools,
        params.workflow,
    );

    let system = agent::build_system_prompt(
        &vars,
        &mode,
        &instructions.text,
        &params.prompt_slots,
        &params.model,
    );

    let mcp = params.mcp_handle.clone().map(|h| McpSession::new(h, &[]));
    let tool_names = advertised_tool_names(&tools, mcp.as_ref());

    let (raw_tx, event_rx) = flume::unbounded::<Envelope>();

    let session_id = MakiId::generate();
    let session_ref = SessionRef::from(session_id);
    let session_ref_clone = session_ref.clone();
    let mailbox = SessionMailbox::register(session_id);
    let fast = params.fast;
    let workflow = params.workflow;
    let goal = params.goal.clone();
    let task = smol::spawn({
        let mcp_shutdown = params.mcp_handle.clone();
        let working_dir_path = params.initial_wd.clone();
        async move {
            let event_tx = EventSender::new(raw_tx, 0);
            let mut model = params.model;
            let provider: Arc<dyn Provider> =
                match provider::from_model_async(&mut model, params.timeouts).await {
                    Ok(p) => Arc::from(p),
                    Err(e) => {
                        error!(error = %e, "provider error");
                        let _ = event_tx.send(AgentEvent::Error {
                            message: e.user_message(),
                        });
                        return;
                    }
                };
            let error_tx = event_tx.clone();
            let mut history = History::new(Vec::new());
            let mut agent = Agent::new(
                AgentParams {
                    provider,
                    model,
                    config: params.config,
                    tool_output_lines: ToolOutputLines::default(),
                    permissions: Arc::new(PermissionManager::new_persistent(
                        params.permissions_config,
                        working_dir_path,
                        params.plugin_rules,
                    )),
                    session_id: Some(session_ref_clone.clone()),
                    mailbox: Some(mailbox.clone()),
                    timeouts: params.timeouts,
                    file_tracker: FileReadTracker::fresh(),
                    prompt_slots: Arc::new(params.prompt_slots),
                    subagent_cancels: Arc::new(CancelMap::new()),
                    subagent_history: SubagentHistoryStore::default(),
                    registry: Arc::clone(ToolRegistry::global_arc()),
                    audience: ToolAudience::MAIN,
                    model_policy: Arc::clone(&params.model_policy),
                },
                AgentRunParams {
                    history: &mut history,
                    system,
                    event_tx,
                    tools,
                },
            )
            .with_loaded_instructions(instructions.loaded)
            .with_goal(params.goal)
            .with_background_wait()
            .with_mcp(mcp);

            let result = agent
                .run(AgentInput {
                    message: params.prompt,
                    mode,
                    images: params.images,
                    preamble: Vec::new(),
                    thinking: Default::default(),
                    fast,
                    workflow,
                    prompt: None,
                })
                .await;
            drop(agent);

            if let Err(e) = result {
                error!(error = %e, "agent error");
                let _ = error_tx.send(AgentEvent::Error {
                    message: e.user_message(),
                });
            }

            if let Some(handle) = mcp_shutdown {
                handle.shutdown().await;
            }
        }
    });

    HeadlessHandle {
        event_rx,
        tool_names,
        session_id: session_ref,
        cwd: working_dir,
        goal,
        task,
    }
}

pub struct InteractiveParams {
    pub model: Model,
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: Timeouts,
    pub prompt_slots: Arc<ResolvedSlots>,
    pub excluded_tools: Vec<&'static str>,
    pub mcp_handle: Option<McpHandle>,
    pub initial_wd: PathBuf,
    pub session_id: Option<SessionRef>,
    pub initial_history: Vec<HistoryItem>,
    pub yolo: bool,
    pub session_rules: Vec<StoredRule>,
    pub structured_permission_rules: Vec<PermissionRuleRecord>,
    pub session_yolo: Option<bool>,
    pub system_prompt_override: Option<String>,
    pub append_system_prompt: Option<String>,
    pub workflow: bool,
    pub model_policy: Arc<ModelPolicy>,
    pub plugin_rules: Arc<PluginRuleStore>,
    /// Host-side overrides that shadow a registered tool's execution while
    /// keeping its advertised schema (e.g. ACP answers `question` via elicitation).
    pub local_tools: LocalTools,
}

pub struct InteractiveHandle {
    pub event_rx: Receiver<Envelope>,
    pub tool_names: Vec<String>,
    pub input_tx: flume::Sender<AgentInput>,
    pub answer_tx: flume::Sender<String>,
    pub cancel_tx: flume::Sender<()>,
    pub model_tx: flume::Sender<Model>,
    pub session_id: SessionRef,
    pub permissions: Arc<PermissionManager>,
    pub task: smol::Task<()>,
}

pub fn spawn_interactive(params: InteractiveParams) -> InteractiveHandle {
    let AgentSetup {
        vars,
        instructions,
        mut tools,
    } = setup(
        &params.model,
        &params.config,
        &params.excluded_tools,
        params.workflow,
    );

    let restored_history = History::restored(params.initial_history);
    let initial_messages = restored_history
        .as_ref()
        .map(|history| history.as_slice())
        .unwrap_or_default();
    let mcp = params
        .mcp_handle
        .clone()
        .map(|h| McpSession::new(h, initial_messages));
    let tool_names = advertised_tool_names(&tools, mcp.as_ref());

    let (raw_tx, event_rx) = flume::unbounded::<Envelope>();
    let (input_tx, input_rx) = flume::unbounded::<AgentInput>();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();
    let (cancel_tx, cancel_rx) = flume::bounded::<()>(1);
    let (model_tx, model_rx) = flume::unbounded::<Model>();

    let (session_id, session_ref) = match params.session_id.clone() {
        Some(w) => (w.id(), w),
        None => {
            let id = MakiId::generate();
            (id, SessionRef::from(id))
        }
    };
    let mailbox = SessionMailbox::register(session_id);

    let working_dir = params.initial_wd.to_string_lossy().into_owned();
    let mut permissions_config = params.permissions_config;
    permissions_config.yolo |= params.yolo;
    let permissions = Arc::new(PermissionManager::new_persistent(
        permissions_config,
        params.initial_wd,
        Arc::clone(&params.plugin_rules),
    ));
    permissions.load_session_rules(stored_to_rules(&params.session_rules));
    permissions.load_structured_conversation_rules(params.structured_permission_rules);
    permissions.set_session_yolo(params.session_yolo);

    let answer_rx = Arc::new(Mutex::new(answer_rx));
    let file_tracker = FileReadTracker::fresh();

    let session_ref_clone = session_ref.clone();
    let task = smol::spawn({
        let permissions = Arc::clone(&permissions);
        async move {
            let mut history = match restored_history {
                Ok(history) => history,
                Err(error) => {
                    error!(%error, "failed to restore history");
                    let _ = EventSender::new(raw_tx, 0).send(AgentEvent::Error {
                        message: format!("Failed to restore history: {error}"),
                    });
                    return;
                }
            };
            let mut model = params.model;
            let mut provider: Arc<dyn Provider> =
                match provider::from_model_async(&mut model, params.timeouts).await {
                    Ok(p) => Arc::from(p),
                    Err(e) => {
                        error!(error = %e, "provider error");
                        let _ = EventSender::new(raw_tx, 0).send(AgentEvent::Error {
                            message: e.user_message(),
                        });
                        return;
                    }
                };

            let mut store = SessionStore::open(session_id, &working_dir, &model.spec());
            let subagent_history = store
                .as_ref()
                .map(|store| store.subagent_history.clone())
                .unwrap_or_default();
            let mut run_id: u64 = 0;

            while let Ok(input) = input_rx.recv_async().await {
                let (trigger, cancel) = CancelToken::new();
                let cancel_task = smol::spawn({
                    let cancel_rx = cancel_rx.clone();
                    async move {
                        if cancel_rx.recv_async().await.is_ok() {
                            trigger.cancel();
                        }
                    }
                });

                // MCP connects in the background, so a prompt that beats it waits
                // here instead of shipping a turn without the MCP tools. The wait
                // is racing cancel: a slow server must not pin the whole session.
                if let Some(mcp) = &mcp {
                    let _ = cancel.race(mcp.ready()).await;
                }

                let event_tx = EventSender::new(raw_tx.clone(), run_id);
                let error_tx = event_tx.clone();

                if let Some(mut new_model) = model_rx
                    .try_iter()
                    .last()
                    .filter(|candidate| params.model_policy.allows(&candidate.spec()))
                    && new_model.spec() != model.spec()
                {
                    match provider::from_model_async(&mut new_model, params.timeouts).await {
                        Ok(p) => {
                            provider = Arc::from(p);
                            tools = tool_definitions(
                                &vars,
                                &new_model,
                                &params.config,
                                &params.excluded_tools,
                                params.workflow,
                                ToolRegistry::global(),
                            );
                            model = new_model;
                        }
                        Err(e) => {
                            error!(error = %e, "provider error");
                            let _ = error_tx.send(AgentEvent::Error {
                                message: e.user_message(),
                            });
                            run_id += 1;
                            continue;
                        }
                    }
                }

                let mut system = params.system_prompt_override.clone().unwrap_or_else(|| {
                    agent::build_system_prompt(
                        &vars,
                        &input.mode,
                        &instructions.text,
                        &params.prompt_slots,
                        &model,
                    )
                });
                if let Some(append) = &params.append_system_prompt {
                    system.push('\n');
                    system.push_str(append);
                }

                while answer_rx.lock().await.try_recv().is_ok() {}

                let mut agent = Agent::new(
                    AgentParams {
                        provider: Arc::clone(&provider),
                        model: model.clone(),
                        config: params.config.clone(),
                        tool_output_lines: ToolOutputLines::default(),
                        permissions: Arc::clone(&permissions),
                        session_id: Some(session_ref_clone.clone()),
                        mailbox: Some(mailbox.clone()),
                        timeouts: params.timeouts,
                        file_tracker: Arc::clone(&file_tracker),
                        prompt_slots: Arc::clone(&params.prompt_slots),
                        subagent_cancels: Arc::new(CancelMap::new()),
                        subagent_history: subagent_history.clone(),
                        registry: Arc::clone(ToolRegistry::global_arc()),
                        audience: ToolAudience::MAIN,
                        model_policy: Arc::clone(&params.model_policy),
                    },
                    AgentRunParams {
                        history: &mut history,
                        system,
                        event_tx,
                        tools: tools.clone(),
                    },
                )
                .with_loaded_instructions(instructions.loaded.clone())
                .with_user_response_rx(Arc::clone(&answer_rx))
                .with_cancel(cancel)
                .with_local_tools(Arc::clone(&params.local_tools))
                .with_mcp(mcp.clone());

                let result = agent.run(input).await;
                drop(agent);
                cancel_task.cancel().await;

                if let Err(ref e) = result {
                    error!(error = %e, "agent error");
                    let _ = error_tx.send(AgentEvent::Error {
                        message: e.user_message(),
                    });
                }

                if let Some(store) = &mut store {
                    store.record_turn(&history, model.spec(), &permissions);
                }
                run_id += 1;
            }

            if let Some(store) = &mut store {
                store.sync_permissions(&permissions);
                store.save();
            }

            if let Some(handle) = params.mcp_handle {
                handle.shutdown().await;
            }
        }
    });

    InteractiveHandle {
        event_rx,
        tool_names,
        input_tx,
        answer_tx,
        cancel_tx,
        model_tx,
        session_id: session_ref,
        permissions,
        task,
    }
}

fn extract_tool_names(tools: &Value) -> Vec<String> {
    tools
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use maki_storage::permission_state::PermissionRuleRecord;
    use maki_storage::sessions::generate_title;
    use maki_storage::tool_outputs::ToolOutputStore;
    use tempfile::TempDir;

    use super::*;

    const SESSION_ID: &str = "01965087-4c71-7f00-8000-000000000000";
    const CWD: &str = "/project";
    const MODEL_SPEC: &str = "anthropic/claude-test";
    const SESSION_SCOPE: &str = "cargo *";

    fn session_id() -> MakiId {
        SESSION_ID.parse().unwrap()
    }

    fn store_in(tmp: &TempDir) -> SessionStore {
        SessionStore::open_in(
            StateDir::from_path(tmp.path().to_path_buf()),
            session_id(),
            CWD,
            MODEL_SPEC,
        )
    }

    fn load(tmp: &TempDir) -> StoredSession {
        StoredSession::load(session_id(), &StateDir::from_path(tmp.path().to_path_buf())).unwrap()
    }

    fn permission_manager() -> PermissionManager {
        PermissionManager::new_nonpersistent(
            PermissionsConfig::default(),
            PathBuf::from(CWD),
            Arc::default(),
        )
    }

    #[test]
    fn new_session_is_loadable_before_first_turn() {
        let tmp = TempDir::new().unwrap();
        store_in(&tmp);
        let loaded = load(&tmp);
        assert_eq!(loaded.id, session_id());
        assert_eq!(loaded.cwd, CWD);
        assert_eq!(loaded.model, MODEL_SPEC);
        assert!(loaded.messages().is_empty());
    }

    #[test]
    fn record_turn_persists_messages_and_title() {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        let messages = vec![Message::user("fix the login bug".into())];
        let history = History::new(messages.clone());
        store.record_turn(&history, MODEL_SPEC.into(), &permission_manager());

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 1);
        assert_eq!(loaded.title, generate_title(&messages));
    }

    #[test]
    fn record_turn_persists_observations() {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        let history = History::new(vec![
            Message::user("fix the login bug".into()),
            Message::observation("build failed".into()),
        ]);
        store.record_turn(&history, MODEL_SPEC.into(), &permission_manager());

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 2);
        let restored = History::restored(loaded.messages().to_vec()).unwrap();
        assert!(restored.as_slice()[1].is_observation());
    }

    #[test]
    fn record_turn_round_trips_managed_output_ref() {
        let tmp = TempDir::new().unwrap();
        let state_dir = StateDir::from_path(tmp.path().to_path_buf());
        let output_ref = ToolOutputStore::new(state_dir)
            .put(session_id(), "full output")
            .unwrap();
        let mut store = store_in(&tmp);
        let history = History::new(vec![
            Message::user("inspect".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    "read-1",
                    "read",
                    serde_json::json!({}),
                )],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "read-1".into(),
                    content: "preview".into(),
                    is_error: false,
                    output_ref: Some(output_ref.clone()),
                }],
                ..Default::default()
            },
        ]);

        store.record_turn(&history, MODEL_SPEC.into(), &permission_manager());

        let loaded = load(&tmp);
        let restored = loaded.messages().iter().find_map(|item| match &item.kind {
            HistoryItemKind::ToolResult { output_ref, .. } => output_ref.as_ref(),
            _ => None,
        });
        assert_eq!(restored, Some(&output_ref));
    }

    #[test]
    fn reopening_resumes_existing_session() {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        store.record_turn(
            &History::new(vec![Message::user("first prompt".into())]),
            MODEL_SPEC.into(),
            &permission_manager(),
        );
        drop(store);

        let mut store = store_in(&tmp);
        assert_eq!(store.session.messages().len(), 1);

        let mut history = History::restored(store.session.messages().to_vec()).unwrap();
        history.push(Message::user("second prompt".into()));
        store.record_turn(&history, "other/model".into(), &permission_manager());

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 2);
        assert_eq!(loaded.model, "other/model");
    }

    #[test]
    fn record_turn_syncs_and_reloads_subagent_history() {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        store
            .subagent_history
            .reserve("task-1")
            .unwrap()
            .complete(vec![Message::user("investigate".into())]);
        store.record_turn(
            &History::default(),
            MODEL_SPEC.into(),
            &permission_manager(),
        );

        let loaded = load(&tmp);
        let task_history =
            History::restored(loaded.subagent_messages()["task-1"].as_ref().clone()).unwrap();
        assert_eq!(task_history.as_slice()[0].user_text(), Some("investigate"));

        let reopened = store_in(&tmp);
        let lease = reopened.subagent_history.continue_task("task-1").unwrap();
        assert_eq!(lease.history().unwrap()[0].user_text(), Some("investigate"));
    }

    #[test]
    fn record_turn_checkpoints_restorable_permissions() {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        let permissions = permission_manager();
        let rule = PermissionRule {
            tool: ToolKey::native("bash"),
            scope: Some(SESSION_SCOPE.into()),
            effect: Effect::Allow,
        };
        permissions.load_session_rules(vec![rule.clone()]);
        let request = crate::permissions::PermissionRequest::from_legacy(
            "request".into(),
            ToolKey::native("bash"),
            vec!["cargo test".into()],
            serde_json::json!({"command": "cargo test"}),
            PathBuf::from(CWD).as_path(),
            false,
        );
        let structured = PermissionRuleRecord::conversation(
            request
                .options
                .iter()
                .find(|option| option.id == "allow_conversation")
                .unwrap()
                .rule
                .clone(),
        )
        .unwrap();
        permissions.load_structured_conversation_rules(vec![structured.clone()]);
        permissions.set_session_yolo(Some(true));

        store.record_turn(&History::default(), MODEL_SPEC.into(), &permissions);

        let loaded = load(&tmp);
        assert_eq!(loaded.meta.session_rules.len(), 1);
        assert_eq!(
            loaded.meta.structured_permission_rules,
            vec![structured.clone()]
        );
        assert_eq!(loaded.meta.yolo, Some(true));
        let restored = permission_manager();
        restored.load_session_rules(stored_to_rules(&loaded.meta.session_rules));
        restored
            .load_structured_conversation_rules(loaded.meta.structured_permission_rules.clone());
        restored.set_session_yolo(loaded.meta.yolo);
        let restored_rules = restored.session_rules_snapshot();
        assert_eq!(restored_rules.len(), 1);
        assert_eq!(restored_rules[0].tool, rule.tool);
        assert_eq!(restored_rules[0].scope, rule.scope);
        assert_eq!(restored_rules[0].effect, rule.effect);
        assert_eq!(
            restored.structured_conversation_rules_snapshot(),
            vec![structured]
        );
        assert!(restored.is_yolo());
        assert_eq!(restored.persisted_yolo(), Some(true));
    }

    #[test]
    fn extract_tool_names_filters_valid_entries() {
        let tools = serde_json::json!([{"name": "read"}, {"type": "function"}, {"name": "bash"}]);
        assert_eq!(extract_tool_names(&tools), vec!["read", "bash"]);
    }

    #[test]
    fn advertised_names_show_tool_search_not_deferred_tools() {
        let base = serde_json::json!([{"name": "read"}]);
        let mcp = crate::mcp::stub_session(&[("srv.fetch_issue", "Fetch a GitHub issue")]);
        let names = advertised_tool_names(&base, Some(&mcp));
        assert_eq!(
            names,
            vec!["read", crate::mcp::TOOL_SEARCH_TOOL_NAME],
            "clients must see the search tool, not deferred definitions"
        );
        assert_eq!(
            base,
            serde_json::json!([{"name": "read"}]),
            "probing must not bake MCP entries into the base tools"
        );
        assert_eq!(advertised_tool_names(&base, None), vec!["read"]);
    }
}
