use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use async_lock::Mutex;
use caudra_config::ModelPolicy;
#[cfg(test)]
use caudra_config::ToolKey;
use caudra_providers::Timeouts;
use caudra_providers::model::Model;
use caudra_providers::provider::{self, Provider};
#[cfg(test)]
use caudra_providers::{ContentBlock, Message, Role};
use caudra_providers::{
    HistoryItem, HistoryItemKind, TokenUsage, active_history_items, merge_history_items,
    resolve_history_head,
};
use caudra_storage::StateDir;
use caudra_storage::id::{CaudraId, SessionRef};
use caudra_storage::permission_state::PermissionRuleRecord;
use caudra_storage::sessions::{
    SessionCursor, SessionDatabase, SessionLease, StoredSubagent, StoredSubagentOutcome,
};
use flume::Receiver;
use serde_json::Value;
use tracing::{error, warn};

use crate::agent::{self, History};
use crate::cancel::{CancelMap, CancelToken};
use crate::permissions::{PermissionManager, PluginRuleStore};
use crate::prompt::ResolvedSlots;
use crate::prompt::profile::{BUILTIN_PROFILE_NAME, PromptProfileCatalog};
use crate::template;
use crate::tools::{
    DeferralSession, DeferredTool, DescriptionContext, FileReadTracker, LocalTools, PathLocks,
    ToolAudience, ToolDefinitions, ToolFilter, ToolRegistry, deferral,
};
use crate::{
    Agent, AgentConfig, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams, DoneReason,
    Envelope, EventSender, GoalHandle, ImageSource, McpHandle, McpSession, PermissionsConfig,
    SessionMailbox, StoredSession, SubagentHistorySnapshot, SubagentHistoryStore, ToolOutput,
    ToolOutputLines, open_stored_session,
};

struct SessionStore {
    dir: StateDir,
    _lease: Arc<SessionLease>,
    database: Option<SessionDatabase>,
    cursor: Option<SessionCursor>,
    session: StoredSession,
    created: bool,
    subagent_history: SubagentHistoryStore,
    persisted_subagent_history: SubagentHistorySnapshot,
}

impl SessionStore {
    fn open(
        session_id: CaudraId,
        cwd: &str,
        model_spec: &str,
        lease: Arc<SessionLease>,
    ) -> Result<Self, caudra_storage::sessions::SessionError> {
        let dir = StateDir::resolve()?;
        Self::open_in_with_lease(dir, session_id, cwd, model_spec, lease)
    }

    #[cfg(test)]
    fn open_in(
        dir: StateDir,
        session_id: CaudraId,
        cwd: &str,
        model_spec: &str,
    ) -> Result<Self, caudra_storage::sessions::SessionError> {
        let lease = Arc::new(SessionLease::acquire(&dir, session_id)?);
        Self::open_in_with_lease(dir, session_id, cwd, model_spec, lease)
    }

    fn open_in_with_lease(
        dir: StateDir,
        session_id: CaudraId,
        cwd: &str,
        model_spec: &str,
        lease: Arc<SessionLease>,
    ) -> Result<Self, caudra_storage::sessions::SessionError> {
        lease.validate(&dir, session_id)?;
        match open_stored_session(session_id, &dir) {
            Ok(session) => Ok(Self::from_session(dir, session, false, lease)),
            Err(caudra_storage::sessions::SessionError::Storage(
                caudra_storage::StorageError::NotFound(_),
            )) => {
                let mut session = StoredSession::new(model_spec, cwd);
                session.id = session_id;
                let mut store = Self::from_session(dir, session, true, lease);
                store.save()?;
                Ok(store)
            }
            Err(error) => Err(error),
        }
    }

    fn from_session(
        dir: StateDir,
        session: StoredSession,
        created: bool,
        lease: Arc<SessionLease>,
    ) -> Self {
        let database = SessionDatabase::open(&dir)
            .map_err(|error| warn!(%error, "session database unavailable"))
            .ok();
        let cursor = database
            .as_ref()
            .and_then(|database| {
                database
                    .load_with_cursor::<HistoryItem, TokenUsage, ToolOutput>(session.id)
                    .ok()
            })
            .map(|(_, cursor)| cursor)
            // The compatibility loader and cursor lookup are separate reads.
            // Never let a newer cursor authenticate saving an older snapshot.
            .filter(|cursor| session.persisted_write_version() == Some(cursor.write_version()));
        let head = resolve_history_head(
            session.messages(),
            session.meta.history_head,
            session.meta.pending_revert.is_some(),
        );
        let active_history =
            active_history_items(session.messages(), head).unwrap_or_else(|error| {
                warn!(%error, "failed to resolve active history for subagent restoration");
                Vec::new()
            });
        let mut reachable = reachable_subagent_ids(&active_history, &session);
        let mut versions =
            crate::active_task_history_versions_with_batch_state(&active_history, |call_id| {
                session
                    .tool_outputs()
                    .get(call_id)
                    .and_then(|output| output.state())
            });
        reachable.extend(
            versions
                .iter()
                .filter(|(_, version_id)| session.subagent_messages().contains_key(*version_id))
                .map(|(task_id, _)| task_id.clone()),
        );
        for subagent in session.subagents() {
            if reachable.contains(&subagent.tool_use_id)
                && !versions.contains_key(&subagent.tool_use_id)
                && let Some(version_id) = &subagent.parent_tool_use_id
                && session.subagent_messages().contains_key(version_id)
            {
                versions.insert(subagent.tool_use_id.clone(), version_id.clone());
            }
        }
        let version_ids: HashSet<&str> = versions
            .iter()
            .filter_map(|(task_id, version_id)| {
                (task_id != version_id && session.subagent_messages().contains_key(version_id))
                    .then_some(version_id.as_str())
            })
            .collect();
        let subagent_messages = reachable
            .iter()
            .filter(|task_id| !version_ids.contains(task_id.as_str()))
            .filter_map(|task_id| {
                let version_id = versions.get(task_id).unwrap_or(task_id);
                let items = session
                    .subagent_messages()
                    .get(version_id)
                    .or_else(|| session.subagent_messages().get(task_id))?;
                match History::restored(items.as_ref().clone()) {
                    Ok(history) => Some((task_id.clone(), Arc::new(history.into_vec()))),
                    Err(error) => {
                        warn!(%task_id, %error, "failed to restore subagent history");
                        None
                    }
                }
            })
            .collect();
        let specs = session
            .subagent_task_specs()
            .iter()
            .filter(|(task_id, spec)| reachable.contains(*task_id) && !spec.is_version())
            .map(|(task_id, spec)| (task_id.clone(), spec.clone()))
            .collect();
        let subagent_history = SubagentHistoryStore::seeded_with_specs(subagent_messages, specs);
        let persisted_subagent_history = subagent_history.snapshot();
        Self {
            dir,
            _lease: lease,
            database,
            cursor,
            session,
            created,
            subagent_history,
            persisted_subagent_history,
        }
    }

    fn save(&mut self) -> Result<(), caudra_storage::sessions::SessionError> {
        self.session.updated_at = caudra_storage::now_epoch();
        if self.database.is_none() {
            self.database = SessionDatabase::open(&self.dir)
                .map_err(|error| warn!(%error, "session database unavailable"))
                .ok();
        }
        let Some(database) = self.database.as_mut() else {
            return Err(caudra_storage::StorageError::Io(std::io::Error::other(
                "session database unavailable",
            ))
            .into());
        };
        match database.save(&self.session, self.cursor.as_ref()) {
            Ok(cursor) => {
                self.cursor = Some(cursor);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn sync_permissions(&mut self, permissions: &PermissionManager) {
        self.session.meta.structured_permission_rules =
            permissions.structured_conversation_rules_snapshot();
        self.session.meta.yolo = permissions.persisted_yolo();
    }

    fn set_system_prompt_profile(&mut self, name: Option<&str>) {
        if let Some(name) = name {
            self.session.meta.system_prompt_profile = Some(name.to_owned());
        }
    }

    fn verify_start_version(
        &self,
        expected_write_version: Option<i64>,
    ) -> Result<(), caudra_storage::sessions::SessionError> {
        let actual = self.session.persisted_write_version();
        match (expected_write_version, actual, self.created) {
            (Some(expected), Some(actual), _) if expected == actual => Ok(()),
            (Some(expected), actual, _) => Err(
                caudra_storage::sessions::SessionError::ConcurrentSessionWriter {
                    id: self.session.id,
                    expected,
                    actual: actual.unwrap_or(-1),
                },
            ),
            (None, _, true) => Ok(()),
            (None, _, false) => Err(caudra_storage::sessions::SessionError::AlreadyExists {
                id: self.session.id,
            }),
        }
    }

    fn record_turn(
        &mut self,
        history: &History,
        model_spec: String,
        permissions: &PermissionManager,
    ) -> Result<(), caudra_storage::sessions::SessionError> {
        let mut merged = self.session.messages().to_vec();
        if let Err(error) = merge_history_items(&mut merged, history.active_items()) {
            warn!(%error, "refusing to persist invalid history graph");
            return Ok(());
        }
        self.session.merge_history(history.snapshot(), merged);
        self.session
            .set_conversation_state(history.item_head(), None);
        self.session.set_model(model_spec);
        let snapshot = self.subagent_history.snapshot();
        if snapshot.revision() != self.persisted_subagent_history.revision() {
            for (task_id, record) in snapshot.records() {
                let unchanged = self
                    .persisted_subagent_history
                    .records()
                    .get(task_id)
                    .is_some_and(|persisted| Arc::ptr_eq(persisted, record));
                if !unchanged {
                    let items = History::new(Arc::unwrap_or_clone(Arc::clone(record.messages())))
                        .into_items();
                    if let Some(version_id) = record
                        .version_id()
                        .filter(|version_id| *version_id != task_id)
                    {
                        if let Some(previous) =
                            self.session.subagent_messages().get(task_id).cloned()
                        {
                            self.session.set_subagent_history(
                                task_id.clone(),
                                previous.as_ref().clone(),
                                record.spec().cloned(),
                            );
                        } else {
                            self.session.set_subagent_history(
                                task_id.clone(),
                                items.clone(),
                                record.spec().cloned(),
                            );
                        }
                        self.session.set_subagent_history(
                            version_id.to_owned(),
                            items,
                            Some(caudra_storage::sessions::StoredSubagentTaskSpec::version()),
                        );
                    } else {
                        self.session.set_subagent_history(
                            task_id.clone(),
                            items,
                            record.spec().cloned(),
                        );
                    }
                }
            }
        }
        self.sync_permissions(permissions);
        self.session.update_title_if_default();
        self.save()?;
        self.persisted_subagent_history = snapshot;
        Ok(())
    }

    /// A cancelled turn never delivers the `ToolDone` that resolves a running
    /// task, so its children would persist as `Unknown` forever. Mirrors the
    /// TUI, which kills unfinished subagents on cancel.
    fn kill_unfinished_subagents(&mut self) {
        let mut subagents = self.session.subagents().to_vec();
        let mut killed = false;
        for subagent in &mut subagents {
            if subagent.outcome == StoredSubagentOutcome::Unknown {
                subagent.outcome = StoredSubagentOutcome::Killed;
                killed = true;
            }
        }
        if killed {
            self.session.set_subagents(subagents);
        }
    }

    fn record_event(
        &mut self,
        envelope: &Envelope,
    ) -> Result<(), caudra_storage::sessions::SessionError> {
        match &envelope.event {
            AgentEvent::SessionTitle {
                title: Some(title), ..
            } => {
                self.session.set_title_if_auto(title.clone());
            }
            AgentEvent::ToolDone(done) => {
                self.session
                    .insert_tool_output(done.id.clone(), done.output.clone());
                let mut subagents = self.session.subagents().to_vec();
                if let Some(subagent) = subagents.iter_mut().find(|subagent| {
                    subagent.parent_tool_use_id.as_deref() == Some(done.id.as_str())
                        && subagent.outcome == StoredSubagentOutcome::Unknown
                }) {
                    subagent.outcome = if done.is_error {
                        StoredSubagentOutcome::Error
                    } else {
                        StoredSubagentOutcome::Done
                    };
                    self.session.set_subagents(subagents);
                }
            }
            AgentEvent::SubagentHistory {
                task_id,
                parent_tool_use_id,
                root_tool_use_id,
                name,
                model,
                messages,
                spec,
            } => {
                let items = History::new(messages.clone()).into_items();
                if parent_tool_use_id == task_id {
                    self.session
                        .set_subagent_history(task_id.clone(), items, spec.clone());
                } else {
                    if let Some(previous) = self.session.subagent_messages().get(task_id).cloned() {
                        self.session.set_subagent_history(
                            task_id.clone(),
                            previous.as_ref().clone(),
                            spec.clone(),
                        );
                    } else {
                        self.session.set_subagent_history(
                            task_id.clone(),
                            items.clone(),
                            spec.clone(),
                        );
                    }
                    self.session.set_subagent_history(
                        parent_tool_use_id.clone(),
                        items,
                        Some(caudra_storage::sessions::StoredSubagentTaskSpec::version()),
                    );
                }
                let mut subagents = self.session.subagents().to_vec();
                if let Some(stored) = subagents
                    .iter_mut()
                    .find(|stored| stored.tool_use_id == *task_id)
                {
                    stored.parent_tool_use_id = Some(parent_tool_use_id.clone());
                    stored.root_tool_use_id = Some(root_tool_use_id.clone());
                    stored.name.clone_from(name);
                    stored.model = Some(model.clone());
                    stored.outcome = StoredSubagentOutcome::Unknown;
                } else {
                    subagents.push(StoredSubagent {
                        tool_use_id: task_id.clone(),
                        parent_tool_use_id: Some(parent_tool_use_id.clone()),
                        root_tool_use_id: Some(root_tool_use_id.clone()),
                        name: name.clone(),
                        model: Some(model.clone()),
                        outcome: StoredSubagentOutcome::Unknown,
                    });
                }
                self.session.set_subagents(subagents);
            }
            _ => return Ok(()),
        }
        self.save()
    }
}

fn reachable_subagent_ids(history: &[HistoryItem], session: &StoredSession) -> HashSet<String> {
    let mut reachable = history_task_ids(history);
    let mut active_calls = crate::history_tool_call_ids(history);
    expand_reachable_subagents(&mut reachable, &mut active_calls, session);
    let legacy_fallback = reachable.is_empty()
        || history.iter().any(|item| {
            matches!(
                &item.kind,
                HistoryItemKind::AssistantText {
                    retained_subagent_ids,
                    is_compaction_summary: true,
                    ..
                } if retained_subagent_ids.is_empty()
            )
        });
    reachable.extend(
        session
            .subagent_task_specs()
            .iter()
            .filter(|(task_id, spec)| {
                if !spec.is_generic() {
                    return false;
                }
                let descriptor = session
                    .subagents()
                    .iter()
                    .find(|subagent| subagent.tool_use_id == **task_id);
                descriptor.is_none()
                    || active_calls.contains(*task_id)
                    || descriptor
                        .and_then(|subagent| subagent.root_tool_use_id.as_ref())
                        .is_some_and(|root| active_calls.contains(root))
            })
            .map(|(task_id, _)| task_id.clone()),
    );
    if legacy_fallback {
        reachable.extend(
            session
                .subagent_messages()
                .keys()
                .filter(|task_id| !session.subagent_task_specs().contains_key(*task_id))
                .filter(|task_id| {
                    !session.subagents().iter().any(|subagent| {
                        subagent.tool_use_id.as_str() != task_id.as_str()
                            && subagent.parent_tool_use_id.as_ref() == Some(*task_id)
                    })
                })
                .cloned(),
        );
    }
    expand_reachable_subagents(&mut reachable, &mut active_calls, session);
    reachable
}

fn expand_reachable_subagents(
    reachable: &mut HashSet<String>,
    active_calls: &mut HashSet<String>,
    session: &StoredSession,
) {
    let mut visited = HashSet::new();
    loop {
        for subagent in session.subagents() {
            if subagent
                .parent_tool_use_id
                .as_ref()
                .is_some_and(|parent| reachable.contains(parent) || active_calls.contains(parent))
                || subagent
                    .root_tool_use_id
                    .as_ref()
                    .is_some_and(|root| active_calls.contains(root))
            {
                reachable.insert(subagent.tool_use_id.clone());
            }
        }
        let Some(task_id) = reachable
            .iter()
            .find(|task_id| !visited.contains(*task_id))
            .cloned()
        else {
            break;
        };
        visited.insert(task_id.clone());
        if let Some(state) = session
            .tool_outputs()
            .get(&task_id)
            .and_then(|output| output.state())
        {
            collect_task_metadata_from_value(state, reachable);
        }
        if let Some(nested) = session.subagent_messages().get(&task_id) {
            reachable.extend(history_task_ids(nested));
            active_calls.extend(crate::history_tool_call_ids(nested));
        }
    }
}

fn history_task_ids(items: &[HistoryItem]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for item in items {
        match &item.kind {
            HistoryItemKind::ToolCall { call_id, name, .. }
                if crate::tools::is_container_tool(name) =>
            {
                ids.insert(call_id.clone());
            }
            HistoryItemKind::AssistantText {
                retained_subagent_ids,
                ..
            } => ids.extend(retained_subagent_ids.iter().cloned()),
            _ => {}
        }
    }
    ids
}

fn collect_task_metadata_from_value(value: &Value, ids: &mut HashSet<String>) {
    match value {
        Value::String(text) => collect_task_metadata(text, ids),
        Value::Array(values) => {
            for value in values {
                collect_task_metadata_from_value(value, ids);
            }
        }
        Value::Object(values) => {
            if let Some(tool) = values.get("tool").and_then(Value::as_str) {
                if tool == "task" {
                    if let Some(invocation_id) = values.get("invocation_id").and_then(Value::as_str)
                    {
                        ids.insert(invocation_id.to_owned());
                    }
                    if let Some(output) = values.get("output").and_then(Value::as_str) {
                        collect_task_metadata(output, ids);
                    }
                }
                return;
            }
            for value in values.values() {
                collect_task_metadata_from_value(value, ids);
            }
        }
        _ => {}
    }
}

fn collect_task_metadata(content: &str, ids: &mut HashSet<String>) {
    for block in content.split("<task_metadata>").skip(1) {
        let Some(metadata) = block.split("</task_metadata>").next() else {
            continue;
        };
        if let Some(task_id) = metadata
            .lines()
            .find_map(|line| line.trim().strip_prefix("task_id: "))
        {
            ids.insert(task_id.to_owned());
        }
    }
}

pub struct HeadlessParams {
    pub model: Model,
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: Timeouts,
    pub prompt: String,
    pub thinking: crate::ThinkingConfig,
    pub images: Vec<ImageSource>,
    pub prompt_slots: ResolvedSlots,
    pub system_prompt_profile: Option<Arc<crate::prompt::profile::SystemPromptProfile>>,
    pub prompt_profiles: Arc<PromptProfileCatalog>,
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
    deferred: Vec<DeferredTool>,
    tool_filter: ToolFilter,
}

struct TaskDescriptionContext<'a> {
    prompt_profiles: &'a PromptProfileCatalog,
    thinking: &'a crate::ThinkingConfig,
    model_policy: &'a ModelPolicy,
    timeouts: Timeouts,
}

fn setup(
    model: &Model,
    config: &AgentConfig,
    excluded_tools: &[&'static str],
    workflow: bool,
    task: TaskDescriptionContext<'_>,
) -> AgentSetup {
    let vars = template::env_vars();
    let instructions = agent::load_instructions(&vars.apply("{cwd}"));
    let definitions = tool_definitions(
        &vars,
        model,
        config,
        excluded_tools,
        workflow,
        ToolRegistry::global(),
        task,
    );

    AgentSetup {
        vars,
        instructions,
        tools: definitions.declared,
        deferred: definitions.deferred,
        tool_filter: ToolFilter::from_config(config, model, excluded_tools),
    }
}

/// Base definitions only, split into declared and deferred. MCP definitions
/// are injected per request by `Agent::request_tools`; storing them here would
/// freeze the catalog.
fn tool_definitions(
    vars: &template::Vars,
    model: &Model,
    config: &AgentConfig,
    excluded_tools: &[&'static str],
    workflow: bool,
    registry: &ToolRegistry,
    task: TaskDescriptionContext<'_>,
) -> ToolDefinitions {
    let filter = ToolFilter::from_config(config, model, excluded_tools);
    let bindings =
        task.prompt_profiles
            .bind_for_tasks(model, task.thinking, task.model_policy, task.timeouts);
    let vars = vars.clone().set(
        "{task_system_prompt_profiles}",
        bindings.task_tool_summary("Caudra's built-in task prompt"),
    );
    let ctx = DescriptionContext {
        filter: &filter,
        audience: ToolAudience::MAIN,
        workflow,
    };
    registry.definitions_split(
        &vars,
        &ctx,
        model.supports_tool_examples(),
        &deferral::deferred_names(&config.allowed_tools),
    )
}

/// Names advertised to SDK clients: what the first request would actually
/// carry, so a deferred built-in shows up as `tool_search` rather than as
/// itself.
fn advertised_tool_names(
    tools: &Value,
    deferred: &[DeferredTool],
    mcp: Option<&McpSession>,
) -> Vec<String> {
    let mut probe = tools.clone();
    DeferralSession::new(deferred.to_vec(), std::iter::empty())
        .request_snapshot()
        .extend_tools(&mut probe);
    if let Some(mcp) = mcp {
        mcp.request_snapshot().extend_tools(&mut probe);
    }
    extract_tool_names(&probe)
}

pub fn spawn(mut params: HeadlessParams) -> HeadlessHandle {
    let provider_model = params.model.clone();
    if let Err(error) = provider::adjust_model(&mut params.model, params.timeouts) {
        warn!(%error, "failed to adjust headless model before setup");
    }
    let working_dir = params.initial_wd.to_string_lossy().into_owned();
    let mode = AgentMode::Build;
    let AgentSetup {
        vars,
        instructions,
        tools,
        deferred,
        tool_filter,
    } = setup(
        &params.model,
        &params.config,
        &params.excluded_tools,
        params.workflow,
        TaskDescriptionContext {
            prompt_profiles: &params.prompt_profiles,
            thinking: &params.thinking,
            model_policy: &params.model_policy,
            timeouts: params.timeouts,
        },
    );

    let system = agent::build_system_prompt(
        &vars,
        &mode,
        &instructions.text,
        &params.prompt_slots,
        &tool_filter,
        &params.model,
        params.system_prompt_profile.as_deref(),
    );

    let mcp = params
        .mcp_handle
        .clone()
        .map(|h| McpSession::new(h, &[]).with_disabled_tools(&params.config.disabled_tools));
    let tool_names = advertised_tool_names(&tools, &deferred, mcp.as_ref());

    let (raw_tx, event_rx) = flume::unbounded::<Envelope>();

    let session_id = CaudraId::generate();
    let session_ref = SessionRef::from(session_id);
    let session_ref_clone = session_ref.clone();
    let mailbox = SessionMailbox::register(session_id);
    let fast = params.fast;
    let workflow = params.workflow;
    let goal = params.goal.clone();
    let active_prompt_profile_name: Arc<str> = Arc::from(
        params
            .system_prompt_profile
            .as_ref()
            .map_or(BUILTIN_PROFILE_NAME, |profile| profile.name()),
    );
    let task = smol::spawn({
        let mcp_shutdown = params.mcp_handle.clone();
        let working_dir_path = params.initial_wd.clone();
        async move {
            let event_tx = EventSender::new(raw_tx, 0);
            let mut model = provider_model;
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
                    root_tool_use_id: None,
                    mailbox: Some(mailbox.clone()),
                    context_publisher: None,
                    timeouts: params.timeouts,
                    file_tracker: FileReadTracker::fresh(),
                    path_locks: PathLocks::fresh(),
                    prompt_slots: Arc::new(params.prompt_slots),
                    prompt_profiles: Arc::clone(&params.prompt_profiles),
                    default_task_prompt_profile_name: Arc::clone(&active_prompt_profile_name),
                    active_prompt_profile_name: Some(active_prompt_profile_name),
                    subagent_cancels: Arc::new(CancelMap::new()),
                    subagent_history: SubagentHistoryStore::default(),
                    registry: Arc::clone(ToolRegistry::global_arc()),
                    audience: ToolAudience::MAIN,
                    tool_filter,
                    model_policy: Arc::clone(&params.model_policy),
                },
                AgentRunParams {
                    history: &mut history,
                    system,
                    event_tx,
                    tools,
                    deferred,
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
                    thinking: params.thinking,
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
    pub thinking: crate::ThinkingConfig,
    pub system_prompt_profile: Option<Arc<crate::prompt::profile::SystemPromptProfile>>,
    pub system_prompt_profile_name: Option<String>,
    pub prompt_profiles: Arc<PromptProfileCatalog>,
    pub excluded_tools: Vec<&'static str>,
    pub mcp_handle: Option<McpHandle>,
    pub initial_wd: PathBuf,
    pub session_id: SessionRef,
    pub session_lease: Arc<SessionLease>,
    pub expected_write_version: Option<i64>,
    pub initial_history: Vec<HistoryItem>,
    pub yolo: bool,
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
    pub session_lease: Arc<SessionLease>,
    pub permissions: Arc<PermissionManager>,
    pub task: smol::Task<()>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct InteractiveStartError(String);

pub struct PreparedInteractive {
    params: InteractiveParams,
    history: History,
    model: Model,
    provider: Arc<dyn Provider>,
    store: SessionStore,
}

impl PreparedInteractive {
    pub fn set_mcp_handle(&mut self, mcp_handle: Option<McpHandle>) {
        self.params.mcp_handle = mcp_handle;
    }
}

pub async fn prepare_interactive(
    mut params: InteractiveParams,
) -> Result<PreparedInteractive, InteractiveStartError> {
    let history = History::restored(std::mem::take(&mut params.initial_history))
        .map_err(|error| InteractiveStartError(format!("Failed to restore history: {error}")))?;
    let mut model = params.model.clone();
    let provider: Arc<dyn Provider> = provider::from_model_async(&mut model, params.timeouts)
        .await
        .map(Arc::from)
        .map_err(|error| InteractiveStartError(error.user_message()))?;
    let working_dir = params.initial_wd.to_string_lossy().into_owned();
    let session_id = params.session_id.id();
    let mut store = SessionStore::open(
        session_id,
        &working_dir,
        &model.spec(),
        Arc::clone(&params.session_lease),
    )
    .map_err(|error| InteractiveStartError(format!("Session persistence unavailable: {error}")))?;
    store
        .verify_start_version(params.expected_write_version)
        .map_err(|error| {
            InteractiveStartError(format!("Session changed before startup: {error}"))
        })?;
    store.set_system_prompt_profile(params.system_prompt_profile_name.as_deref());
    store
        .save()
        .map_err(|error| InteractiveStartError(format!("Failed to persist session: {error}")))?;
    Ok(PreparedInteractive {
        params,
        history,
        model,
        provider,
        store,
    })
}

pub fn spawn_prepared_interactive(prepared: PreparedInteractive) -> InteractiveHandle {
    let PreparedInteractive {
        params,
        mut history,
        mut model,
        mut provider,
        store,
    } = prepared;
    let AgentSetup {
        vars,
        instructions,
        tools,
        deferred,
        mut tool_filter,
    } = setup(
        &model,
        &params.config,
        &params.excluded_tools,
        params.workflow,
        TaskDescriptionContext {
            prompt_profiles: &params.prompt_profiles,
            thinking: &params.thinking,
            model_policy: &params.model_policy,
            timeouts: params.timeouts,
        },
    );

    let initial_messages = history.as_slice();
    let mcp = params.mcp_handle.clone().map(|h| {
        McpSession::new(h, initial_messages).with_disabled_tools(&params.config.disabled_tools)
    });
    let tool_names = advertised_tool_names(&tools, &deferred, mcp.as_ref());

    let session_ref = params.session_id.clone();
    let session_id = session_ref.id();
    let session_lease = Arc::clone(&params.session_lease);
    let subagent_history = store.subagent_history.clone();
    let store = Arc::new(Mutex::new(Some(store)));

    let (raw_tx, event_rx) = flume::unbounded::<Envelope>();
    let (input_tx, input_rx) = flume::unbounded::<AgentInput>();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();
    let (cancel_tx, cancel_rx) = flume::bounded::<()>(1);
    let (model_tx, model_rx) = flume::unbounded::<Model>();

    let mailbox = SessionMailbox::register(session_id);

    let mut permissions_config = params.permissions_config;
    permissions_config.yolo |= params.yolo;
    let permissions = Arc::new(PermissionManager::new_persistent(
        permissions_config,
        params.initial_wd,
        Arc::clone(&params.plugin_rules),
    ));
    permissions.load_structured_conversation_rules(params.structured_permission_rules);
    permissions.set_session_yolo(params.session_yolo);

    let answer_rx = Arc::new(Mutex::new(answer_rx));
    let file_tracker = FileReadTracker::fresh();
    let path_locks = PathLocks::fresh();

    let session_ref_clone = session_ref.clone();
    let task = smol::spawn({
        let permissions = Arc::clone(&permissions);
        async move {
            let (agent_tx, agent_rx) = flume::unbounded();
            let event_forwarder = smol::spawn({
                let store = Arc::clone(&store);
                let raw_tx = raw_tx.clone();
                async move {
                    while let Ok(envelope) = agent_rx.recv_async().await {
                        let persistence_error = if let Some(store) = &mut *store.lock().await {
                            store.record_event(&envelope).err()
                        } else {
                            None
                        };
                        let run_id = envelope.run_id;
                        if raw_tx.send_async(envelope).await.is_err() {
                            break;
                        }
                        if let Some(error) = persistence_error
                            && raw_tx
                                .send_async(Envelope {
                                    event: AgentEvent::Error {
                                        message: format!("Failed to persist session: {error}"),
                                    },
                                    subagent: None,
                                    run_id,
                                })
                                .await
                                .is_err()
                        {
                            break;
                        }
                    }
                }
            });
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

                let event_tx = EventSender::new(agent_tx.clone(), run_id);
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
                            tool_filter = ToolFilter::from_config(
                                &params.config,
                                &new_model,
                                &params.excluded_tools,
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

                let definitions = tool_definitions(
                    &vars,
                    &model,
                    &params.config,
                    &params.excluded_tools,
                    input.workflow,
                    ToolRegistry::global(),
                    TaskDescriptionContext {
                        prompt_profiles: &params.prompt_profiles,
                        thinking: &input.thinking,
                        model_policy: &params.model_policy,
                        timeouts: params.timeouts,
                    },
                );

                let mut system = params.system_prompt_override.clone().unwrap_or_else(|| {
                    agent::build_system_prompt(
                        &vars,
                        &input.mode,
                        &instructions.text,
                        &params.prompt_slots,
                        &tool_filter,
                        &model,
                        params.system_prompt_profile.as_deref(),
                    )
                });
                if let Some(append) = &params.append_system_prompt {
                    system.push('\n');
                    system.push_str(append);
                }

                while answer_rx.lock().await.try_recv().is_ok() {}

                let active_prompt_profile_name: Arc<str> = Arc::from(
                    params
                        .system_prompt_profile_name
                        .as_deref()
                        .unwrap_or(BUILTIN_PROFILE_NAME),
                );

                let mut agent = Agent::new(
                    AgentParams {
                        provider: Arc::clone(&provider),
                        model: model.clone(),
                        config: params.config.clone(),
                        tool_output_lines: ToolOutputLines::default(),
                        permissions: Arc::clone(&permissions),
                        session_id: Some(session_ref_clone.clone()),
                        root_tool_use_id: None,
                        mailbox: Some(mailbox.clone()),
                        context_publisher: None,
                        timeouts: params.timeouts,
                        file_tracker: Arc::clone(&file_tracker),
                        path_locks: Arc::clone(&path_locks),
                        prompt_slots: Arc::clone(&params.prompt_slots),
                        prompt_profiles: Arc::clone(&params.prompt_profiles),
                        default_task_prompt_profile_name: Arc::clone(&active_prompt_profile_name),
                        active_prompt_profile_name: Some(active_prompt_profile_name),
                        subagent_cancels: Arc::new(CancelMap::new()),
                        subagent_history: subagent_history.clone(),
                        registry: Arc::clone(ToolRegistry::global_arc()),
                        audience: ToolAudience::MAIN,
                        tool_filter: tool_filter.clone(),
                        model_policy: Arc::clone(&params.model_policy),
                    },
                    AgentRunParams {
                        history: &mut history,
                        system,
                        event_tx,
                        tools: definitions.declared,
                        deferred: definitions.deferred,
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

                if let Some(store) = &mut *store.lock().await {
                    if matches!(result, Ok(DoneReason::Cancelled)) {
                        store.kill_unfinished_subagents();
                    }
                    if let Err(error) = store.record_turn(&history, model.spec(), &permissions) {
                        let _ = EventSender::new(raw_tx.clone(), run_id).send(AgentEvent::Error {
                            message: format!("Failed to persist session: {error}"),
                        });
                    }
                }
                run_id += 1;
            }

            drop(agent_tx);
            event_forwarder.await;
            if let Some(store) = &mut *store.lock().await {
                store.sync_permissions(&permissions);
                if let Err(error) = store.save() {
                    let _ = EventSender::new(raw_tx.clone(), run_id).send(AgentEvent::Error {
                        message: format!("Failed to persist session: {error}"),
                    });
                }
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
        session_lease,
        permissions,
        task,
    }
}

pub async fn spawn_interactive(
    params: InteractiveParams,
) -> Result<InteractiveHandle, InteractiveStartError> {
    prepare_interactive(params)
        .await
        .map(spawn_prepared_interactive)
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
    use caudra_storage::permission_state::PermissionRuleRecord;
    use caudra_storage::sessions::generate_title;
    use caudra_storage::tool_outputs::ToolOutputStore;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const SESSION_ID: &str = "CNK1hV6GWoysH3KQMm5wu";
    const CWD: &str = "/project";
    const MODEL_SPEC: &str = "anthropic/claude-test";

    fn session_id() -> CaudraId {
        SESSION_ID.parse().unwrap()
    }

    fn store_in(tmp: &TempDir) -> SessionStore {
        SessionStore::open_in(
            StateDir::from_path(tmp.path().to_path_buf()),
            session_id(),
            CWD,
            MODEL_SPEC,
        )
        .unwrap()
    }

    #[test]
    fn session_store_rejects_duplicate_active_open() {
        let tmp = TempDir::new().unwrap();
        let first = store_in(&tmp);

        assert!(matches!(
            SessionStore::open_in(
                StateDir::from_path(tmp.path().to_path_buf()),
                session_id(),
                CWD,
                MODEL_SPEC,
            ),
            Err(caudra_storage::sessions::SessionError::SessionInUse { id }) if id == session_id()
        ));

        drop(first);
        assert_eq!(store_in(&tmp).session.id, session_id());
    }

    fn load(tmp: &TempDir) -> StoredSession {
        StoredSession::load(session_id(), &StateDir::from_path(tmp.path().to_path_buf())).unwrap()
    }

    fn write_version(tmp: &TempDir) -> i64 {
        SessionDatabase::open(&StateDir::from_path(tmp.path().to_path_buf()))
            .unwrap()
            .load_with_cursor::<HistoryItem, TokenUsage, ToolOutput>(session_id())
            .unwrap()
            .1
            .write_version()
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
        assert_eq!(write_version(&tmp), 0);
    }

    #[test]
    fn startup_rejects_a_cursor_newer_than_the_caller_snapshot() {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        assert!(store.verify_start_version(None).is_ok());
        store
            .record_turn(
                &History::new(vec![Message::user("newer".into())]),
                MODEL_SPEC.into(),
                &permission_manager(),
            )
            .unwrap();
        drop(store);
        let reopened = store_in(&tmp);

        assert!(matches!(
            reopened.verify_start_version(Some(0)),
            Err(caudra_storage::sessions::SessionError::ConcurrentSessionWriter { .. })
        ));
        assert!(reopened.verify_start_version(Some(1)).is_ok());
        assert!(matches!(
            reopened.verify_start_version(None),
            Err(caudra_storage::sessions::SessionError::AlreadyExists { .. })
        ));
    }

    #[test]
    fn record_turn_persists_messages_and_title() {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        let messages = vec![Message::user("fix the login bug".into())];
        let history = History::new(messages.clone());
        store
            .record_turn(&history, MODEL_SPEC.into(), &permission_manager())
            .unwrap();

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
        store
            .record_turn(&history, MODEL_SPEC.into(), &permission_manager())
            .unwrap();

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

        store
            .record_turn(&history, MODEL_SPEC.into(), &permission_manager())
            .unwrap();

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
        store
            .record_turn(
                &History::new(vec![Message::user("first prompt".into())]),
                MODEL_SPEC.into(),
                &permission_manager(),
            )
            .unwrap();
        drop(store);
        let version_before_reopen = write_version(&tmp);

        let mut store = store_in(&tmp);
        assert_eq!(store.session.messages().len(), 1);
        assert_eq!(write_version(&tmp), version_before_reopen);

        let mut history = History::restored(store.session.messages().to_vec()).unwrap();
        history.push(Message::user("second prompt".into()));
        store
            .record_turn(&history, "other/model".into(), &permission_manager())
            .unwrap();

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 2);
        assert_eq!(loaded.model, "other/model");
    }

    #[test]
    fn newer_cursor_never_authenticates_an_older_headless_snapshot() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let mut original = StoredSession::new(MODEL_SPEC, CWD);
        original.id = session_id();
        original.save(&dir).unwrap();
        let stale = crate::load_stored_session(session_id(), &dir).unwrap();
        let mut current = crate::load_stored_session(session_id(), &dir).unwrap();
        current.set_title("current".into());
        current.save(&dir).unwrap();
        let lease = Arc::new(SessionLease::acquire(&dir, stale.id).unwrap());
        let mut store = SessionStore::from_session(dir, stale, false, lease);
        store.session.set_title("stale".into());

        let error = store.save().unwrap_err();

        assert!(matches!(
            error,
            caudra_storage::sessions::SessionError::ConcurrentSessionWriter { .. }
        ));
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
        store
            .record_turn(
                &History::default(),
                MODEL_SPEC.into(),
                &permission_manager(),
            )
            .unwrap();

        let loaded = load(&tmp);
        let task_history =
            History::restored(loaded.subagent_messages()["task-1"].as_ref().clone()).unwrap();
        assert_eq!(task_history.as_slice()[0].user_text(), Some("investigate"));

        drop(store);
        let reopened = store_in(&tmp);
        let lease = reopened.subagent_history.continue_task("task-1").unwrap();
        assert_eq!(lease.history().unwrap()[0].user_text(), Some("investigate"));
    }

    #[test]
    fn record_event_persists_tool_state_and_subagent_descriptor() {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        let mut done = crate::ToolDoneEvent::error("batch-call".into(), "batch output");
        done.is_error = false;
        done.output = crate::ToolOutput::Plain(crate::TextOutput {
            text: "batch output".into(),
            instructions: None,
            state: Some(serde_json::json!({ "task_id": "nested-task" })),
            lua_provenance: None,
        });
        store
            .record_event(&Envelope {
                event: AgentEvent::ToolDone(Box::new(done)),
                subagent: None,
                run_id: 0,
            })
            .unwrap();
        store
            .record_event(&Envelope {
                event: AgentEvent::SubagentHistory {
                    task_id: "nested-task".into(),
                    parent_tool_use_id: "nested-call".into(),
                    root_tool_use_id: "batch-call".into(),
                    name: "researcher".into(),
                    model: MODEL_SPEC.into(),
                    messages: vec![Message::user("investigate".into())],
                    spec: Some(crate::SubagentTaskSpec::default()),
                },
                subagent: None,
                run_id: 0,
            })
            .unwrap();
        let mut nested_done = crate::ToolDoneEvent::error("nested-call".into(), "nested output");
        nested_done.is_error = false;
        store
            .record_event(&Envelope {
                event: AgentEvent::ToolDone(Box::new(nested_done)),
                subagent: None,
                run_id: 0,
            })
            .unwrap();

        let loaded = load(&tmp);
        assert_eq!(
            loaded.tool_outputs()["batch-call"].state(),
            Some(&serde_json::json!({ "task_id": "nested-task" }))
        );
        assert!(loaded.subagent_messages().contains_key("nested-task"));
        assert!(loaded.subagent_messages().contains_key("nested-call"));
        assert!(loaded.subagents().iter().any(|subagent| {
            subagent.tool_use_id == "nested-task"
                && subagent.parent_tool_use_id.as_deref() == Some("nested-call")
                && subagent.root_tool_use_id.as_deref() == Some("batch-call")
                && subagent.outcome == StoredSubagentOutcome::Done
        }));
    }

    #[test]
    fn cancelling_a_turn_kills_only_the_unresolved_subagents() {
        const RESOLVED: &str = "resolved-task";
        const RUNNING: &str = "running-task";

        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        for (task_id, call_id) in [(RESOLVED, "resolved-call"), (RUNNING, "running-call")] {
            store
                .record_event(&Envelope {
                    event: AgentEvent::SubagentHistory {
                        task_id: task_id.into(),
                        parent_tool_use_id: call_id.into(),
                        root_tool_use_id: call_id.into(),
                        name: "researcher".into(),
                        model: MODEL_SPEC.into(),
                        messages: vec![Message::user("investigate".into())],
                        spec: Some(crate::SubagentTaskSpec::default()),
                    },
                    subagent: None,
                    run_id: 0,
                })
                .unwrap();
        }
        let mut done = crate::ToolDoneEvent::error("resolved-call".into(), "answer");
        done.is_error = false;
        store
            .record_event(&Envelope {
                event: AgentEvent::ToolDone(Box::new(done)),
                subagent: None,
                run_id: 0,
            })
            .unwrap();

        store.kill_unfinished_subagents();
        store.save().unwrap();

        let outcome = |session: &StoredSession, task_id: &str| {
            session
                .subagents()
                .iter()
                .find(|subagent| subagent.tool_use_id == task_id)
                .expect("subagent recorded")
                .outcome
        };
        let loaded = load(&tmp);
        assert_eq!(outcome(&loaded, RESOLVED), StoredSubagentOutcome::Done);
        assert_eq!(outcome(&loaded, RUNNING), StoredSubagentOutcome::Killed);
    }

    #[test]
    fn batch_state_accepts_task_metadata_only_from_task_children() {
        let metadata =
            |task_id: &str| format!("<task_metadata>\ntask_id: {task_id}\n</task_metadata>");
        let mut ids = HashSet::new();

        collect_task_metadata_from_value(
            &serde_json::json!([
                { "tool": "task", "output": metadata("real-task") },
                { "tool": "bash", "output": metadata("forged-task") },
            ]),
            &mut ids,
        );

        assert!(ids.contains("real-task"));
        assert!(!ids.contains("forged-task"));
    }

    #[test]
    fn reachability_follows_nested_generic_session_parents() {
        let tool_call = |id: &str| {
            History::new(vec![Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    id,
                    "custom_session_tool",
                    serde_json::json!({}),
                )],
                ..Default::default()
            }])
            .into_items()
        };
        let main = tool_call("generic-root");
        let mut session = StoredSession::new(MODEL_SPEC, CWD);
        session.replace_messages(main.clone());
        session.set_subagent_history(
            "generic-root".into(),
            tool_call("generic-nested"),
            Some(crate::SubagentTaskSpec::generic()),
        );
        session.set_subagent_history(
            "generic-nested".into(),
            Vec::new(),
            Some(crate::SubagentTaskSpec::generic()),
        );
        session.set_subagents(vec![
            StoredSubagent {
                tool_use_id: "generic-root".into(),
                parent_tool_use_id: Some("generic-root".into()),
                root_tool_use_id: Some("generic-root".into()),
                name: "root".into(),
                model: None,
                outcome: StoredSubagentOutcome::Unknown,
            },
            StoredSubagent {
                tool_use_id: "generic-nested".into(),
                parent_tool_use_id: Some("generic-nested".into()),
                root_tool_use_id: Some("generic-root".into()),
                name: "nested".into(),
                model: None,
                outcome: StoredSubagentOutcome::Unknown,
            },
        ]);

        let reachable = reachable_subagent_ids(&main, &session);

        assert!(reachable.contains("generic-root"));
        assert!(reachable.contains("generic-nested"));
    }

    #[test]
    fn record_turn_persists_continuation_version() {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        store
            .subagent_history
            .reserve("task-1")
            .unwrap()
            .complete_version(
                vec![Message::user("continued".into())],
                "continuation-call".into(),
            );

        store
            .record_turn(
                &History::default(),
                MODEL_SPEC.into(),
                &permission_manager(),
            )
            .unwrap();

        let loaded = load(&tmp);
        assert!(loaded.subagent_messages().contains_key("task-1"));
        assert!(loaded.subagent_messages().contains_key("continuation-call"));
    }

    #[test]
    fn record_turn_checkpoints_restorable_permissions() {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        let permissions = permission_manager();
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
                .option_rule(
                    "allow_exact",
                    crate::permissions::PermissionLifetime::Conversation,
                )
                .unwrap(),
        )
        .unwrap();
        permissions.load_structured_conversation_rules(vec![structured.clone()]);
        permissions.set_session_yolo(Some(true));

        store
            .record_turn(&History::default(), MODEL_SPEC.into(), &permissions)
            .unwrap();

        let loaded = load(&tmp);
        assert_eq!(
            loaded.meta.structured_permission_rules,
            vec![structured.clone()]
        );
        assert_eq!(loaded.meta.yolo, Some(true));
        let restored = permission_manager();
        restored
            .load_structured_conversation_rules(loaded.meta.structured_permission_rules.clone());
        restored.set_session_yolo(loaded.meta.yolo);
        assert_eq!(
            restored.structured_conversation_rules_snapshot(),
            vec![structured]
        );
        assert!(restored.is_yolo());
        assert_eq!(restored.persisted_yolo(), Some(true));
    }

    #[test]
    fn selected_system_prompt_profile_is_persisted() {
        const PROFILE: &str = "review";

        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        store.set_system_prompt_profile(Some(PROFILE));
        store.save().unwrap();

        assert_eq!(
            load(&tmp).meta.system_prompt_profile.as_deref(),
            Some(PROFILE)
        );
    }

    #[test]
    fn extract_tool_names_filters_valid_entries() {
        let tools = serde_json::json!([{"name": "read"}, {"type": "function"}, {"name": "bash"}]);
        assert_eq!(extract_tool_names(&tools), vec!["read", "bash"]);
    }

    /// Both deferral sources collapse into the one search tool, so a client
    /// is told what the first request actually carries.
    #[test_case(true,  false ; "mcp only")]
    #[test_case(false, true  ; "built-ins only")]
    #[test_case(true,  true  ; "both share one catalog entry")]
    fn advertised_names_show_tool_search_not_deferred_tools(mcp: bool, builtin: bool) {
        let base = serde_json::json!([{"name": "read"}]);
        let session =
            mcp.then(|| crate::mcp::stub_session(&[("srv.fetch_issue", "Fetch an issue")]));
        let deferred = match builtin {
            true => vec![DeferredTool::new(
                "code_map",
                None,
                serde_json::json!({"name": "code_map", "description": "Rank symbols"}),
            )],
            false => Vec::new(),
        };

        let names = advertised_tool_names(&base, &deferred, session.as_ref());

        assert_eq!(
            names,
            vec!["read", crate::tools::TOOL_SEARCH_TOOL_NAME],
            "clients must see the search tool, not deferred definitions"
        );
        assert_eq!(
            base,
            serde_json::json!([{"name": "read"}]),
            "probing must not bake deferred entries into the base tools"
        );
    }

    #[test]
    fn advertised_names_omit_the_search_tool_when_nothing_is_deferred() {
        let base = serde_json::json!([{"name": "read"}]);
        assert_eq!(advertised_tool_names(&base, &[], None), vec!["read"]);
    }
}
