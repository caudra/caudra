use std::borrow::Cow;
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use async_lock::Mutex;
use caudra_config::ModelPolicy;
#[cfg(test)]
use caudra_config::ToolKey;
use caudra_providers::Timeouts;
use caudra_providers::model::{Model, ModelPurpose};
use caudra_providers::provider::{self, Provider};
use caudra_providers::{
    CacheKey, HistoryItem, HistoryItemKind, TokenUsage, active_history_items, merge_history_items,
    resolve_history_head,
};
#[cfg(test)]
use caudra_providers::{ContentBlock, Message, Role};
use caudra_storage::id::{CaudraId, SessionRef};
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_storage::permission_state::PermissionRuleRecord;
use caudra_storage::permission_state::mutation::{
    PermissionCommitReceipt, PermissionOwner, PermissionSnapshot, PreparedPermissionMutation,
};
use caudra_storage::sessions::{
    SessionCursor, SessionDatabase, SessionError, SessionLease, StoredMode, StoredPlanTarget,
    StoredSubagent, StoredSubagentOutcome,
};
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_storage::{StateDir, StorageError};
use caudra_workflow::{RunSnapshot, WorkflowRequest, WorkflowResponse};
use caudra_workspace::{
    CommandText, DirectoryNavigation, ExecRequest, LocalDocumentRef, OperationProgressKind,
    OperationState, OperationStatus, WorkspaceCursor, WorkspaceError, WorkspaceSession,
};
use flume::Receiver;
use serde::Deserialize;
use serde_json::Value;
use tracing::{error, warn};

use crate::agent::task_runner::{
    HostExtras, ModeResolver, ModelResolver, SubagentTaskRunner, WorkflowHostContext,
};
use crate::agent::{self, History};
use crate::cancel::{CancelMap, CancelToken};
use crate::commits;
use crate::mentions;
use crate::permissions::editor::{PermissionEditError, PermissionPublication};
use crate::permissions::{PermissionManager, PluginRuleStore};
use crate::prompt::ResolvedSlots;
use crate::prompt::profile::{BUILTIN_PROFILE_NAME, PromptProfileCatalog};
use crate::template;
use crate::tools::{
    BuiltinDeferral, DeferralSession, DeferredTool, DescriptionContext, FileReadTracker,
    LocalTools, PathLocks, ToolAudience, ToolDefinitions, ToolFilter, ToolRegistry, deferral,
};
use crate::workflow::{RuntimeDeps, WorkflowHandle, WorkflowRuntime, WorkspaceRebind};
use crate::{
    Agent, AgentConfig, AgentError, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams,
    BaselineGate, DoneReason, Envelope, EventSender, GoalHandle, ImageSource, McpHandle,
    McpSession, PermissionsConfig, SessionMailbox, StoredSession, SubagentHistorySnapshot,
    SubagentHistoryStore, ToolOutput, ToolOutputLines, WorkspaceBaseline, open_stored_session,
};

/// Bytes of a run's report or result carried into the next prompt.
const COMPLETION_TEXT_LIMIT: usize = 8 * 1024;
const TRUNCATED_SUFFIX: &str = "…[truncated]";
const COMPLETION_SEPARATOR: &str = "\n\n";
const REMOTE_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
const REMOTE_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(50);
const REMOTE_COMMAND_PROGRESS_GAP: &str = "[remote progress gap]";
const REMOTE_COMMAND_CANCELLED: &str = "Remote command cancelled";
const REMOTE_COMMAND_FAILED: &str = "Remote command failed";
const REMOTE_COMMAND_INDETERMINATE: &str =
    "Remote command outcome is indeterminate; it will not be retried";
const REMOTE_COMMAND_TRUNCATED: &str = "[truncated]";
const SESSION_DATABASE_UNAVAILABLE: &str = "session database unavailable";

struct SessionPermissionPublication {
    database: Arc<StdMutex<SessionDatabase>>,
    lease: Arc<SessionLease>,
}

impl PermissionPublication for SessionPermissionPublication {
    fn snapshot(&self) -> Result<PermissionSnapshot, PermissionEditError> {
        Ok(self
            .database
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .permission_snapshot(PermissionOwner::Conversation(self.lease.id()))?)
    }

    fn commit(
        &self,
        prepared: &PreparedPermissionMutation,
    ) -> Result<PermissionCommitReceipt, PermissionEditError> {
        if prepared.expected().iter().any(|snapshot| {
            matches!(snapshot.revision.owner, PermissionOwner::Conversation(id) if id != self.lease.id())
        }) {
            return Err(PermissionEditError::Conflict);
        }
        Ok(self
            .database
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .commit_permission_mutation(prepared)?)
    }

    fn receipt(
        &self,
        operation_id: CaudraId,
    ) -> Result<Option<PermissionCommitReceipt>, PermissionEditError> {
        Ok(self
            .database
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .permission_receipt(operation_id)?)
    }
}

struct SessionStore {
    dir: StateDir,
    lease: Arc<SessionLease>,
    database: Option<Arc<StdMutex<SessionDatabase>>>,
    cursor: Option<SessionCursor>,
    session: StoredSession,
    created: bool,
    subagent_history: SubagentHistoryStore,
    persisted_subagent_history: SubagentHistorySnapshot,
}

impl SessionStore {
    fn history_head(&self) -> Option<CaudraId> {
        self.session
            .meta
            .history_head
            .or_else(|| self.session.messages().last().map(|item| item.id))
    }

    fn open(
        session_id: CaudraId,
        cwd: &str,
        model_spec: &str,
        lease: Arc<SessionLease>,
        workspace_binding: Option<&StoredWorkspaceBinding>,
    ) -> Result<Self, caudra_storage::sessions::SessionError> {
        let dir = StateDir::resolve()?;
        Self::open_in_with_lease(dir, session_id, cwd, model_spec, lease, workspace_binding)
    }

    #[cfg(test)]
    fn open_in(
        dir: StateDir,
        session_id: CaudraId,
        cwd: &str,
        model_spec: &str,
    ) -> Result<Self, caudra_storage::sessions::SessionError> {
        let lease = Arc::new(SessionLease::acquire(&dir, session_id)?);
        Self::open_in_with_lease(dir, session_id, cwd, model_spec, lease, None)
    }

    fn open_in_with_lease(
        dir: StateDir,
        session_id: CaudraId,
        cwd: &str,
        model_spec: &str,
        lease: Arc<SessionLease>,
        workspace_binding: Option<&StoredWorkspaceBinding>,
    ) -> Result<Self, caudra_storage::sessions::SessionError> {
        lease.validate(&dir, session_id)?;
        match open_stored_session(session_id, &dir) {
            Ok(mut session) => {
                StoredWorkspaceBinding::validate_resume(
                    session.workspace_binding(),
                    workspace_binding,
                )?;
                if let Some(binding) = workspace_binding.filter(|binding| !binding.is_local()) {
                    session.replace_workspace_cursor(binding.clone())?;
                }
                Ok(Self::from_session(dir, session, false, lease))
            }
            Err(caudra_storage::sessions::SessionError::Storage(
                caudra_storage::StorageError::NotFound(_),
            )) => {
                let mut session = workspace_binding.map_or_else(
                    || StoredSession::new(model_spec, cwd),
                    |binding| StoredSession::new_with_workspace(model_spec, cwd, binding.clone()),
                );
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
            lease,
            database: database.map(|database| Arc::new(StdMutex::new(database))),
            cursor,
            session,
            created,
            subagent_history,
            persisted_subagent_history,
        }
    }

    fn save(&mut self) -> Result<(), SessionError> {
        self.session.updated_at = caudra_storage::now_epoch();
        if self.database.is_none() {
            self.database = SessionDatabase::open(&self.dir)
                .map_err(|error| warn!(%error, "session database unavailable"))
                .ok()
                .map(|database| Arc::new(StdMutex::new(database)));
        }
        let Some(database) = &self.database else {
            return Err(StorageError::Io(io::Error::other(SESSION_DATABASE_UNAVAILABLE)).into());
        };
        let mut database = database.lock().unwrap_or_else(|error| error.into_inner());
        database
            .permission_snapshot(PermissionOwner::Conversation(self.session.id))
            .and_then(|snapshot| {
                if snapshot.revision.row_present {
                    snapshot.apply_to_meta(self.session.id, &mut self.session.meta)?;
                }
                Ok(())
            })
            .map_err(|error| StorageError::Io(io::Error::other(error)))?;
        match database.save(&self.session, self.cursor.as_ref()) {
            Ok(cursor) => {
                self.cursor = Some(cursor);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn permission_publication(
        &mut self,
    ) -> Result<Arc<SessionPermissionPublication>, PermissionEditError> {
        self.lease
            .validate(&self.dir, self.session.id)
            .map_err(|error| PermissionEditError::Storage(error.to_string()))?;
        self.save()
            .map_err(|error| PermissionEditError::Storage(error.to_string()))?;
        let database = self
            .database
            .as_ref()
            .ok_or_else(|| PermissionEditError::Storage(SESSION_DATABASE_UNAVAILABLE.into()))?;
        Ok(Arc::new(SessionPermissionPublication {
            database: Arc::clone(database),
            lease: Arc::clone(&self.lease),
        }))
    }

    fn sync_permissions(&mut self, permissions: &PermissionManager) {
        self.session.meta.yolo = permissions.persisted_yolo();
    }

    fn set_system_prompt_profile(&mut self, name: Option<&str>) {
        if let Some(name) = name {
            self.session.meta.system_prompt_profile = Some(name.to_owned());
        }
    }

    fn set_mode(&mut self, mode: &AgentMode) {
        match mode {
            AgentMode::Build => self.session.meta.mode = Some(StoredMode::Build),
            AgentMode::Plan(path) => {
                let path = path.to_string_lossy().into_owned();
                self.session.meta.mode = Some(StoredMode::Plan);
                self.session.meta.plan_path = Some(path.clone());
                self.session.meta.plan_target = Some(StoredPlanTarget::LocalPath { path });
            }
            AgentMode::RemotePlan(reference) => {
                self.session.meta.mode = Some(StoredMode::Plan);
                self.session.meta.plan_path = None;
                self.session.meta.plan_target = Some(StoredPlanTarget::PlanRef {
                    reference: reference.clone(),
                });
            }
            AgentMode::ReadOnly => {}
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
                let local_plan_written = self
                    .session
                    .meta
                    .plan_target
                    .as_ref()
                    .and_then(|target| match target {
                        StoredPlanTarget::PlanRef { reference } => {
                            Some(LocalDocumentRef::Plan(reference.clone()))
                        }
                        StoredPlanTarget::LocalPath { .. } => None,
                    })
                    .is_some_and(|reference| done.wrote_document(&reference));
                let local_path_written = self
                    .session
                    .meta
                    .plan_path
                    .as_deref()
                    .is_some_and(|path| done.wrote_to(Path::new(path)));
                self.session.meta.plan_written |= local_plan_written || local_path_written;
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
                        thinking: None,
                        fast: false,
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
    pub model_policy: Arc<ModelPolicy>,
    pub plugin_rules: Arc<PluginRuleStore>,
    pub goal: GoalHandle,
    pub remote_environment: Option<RemoteEnvironment>,
    pub workspace_session: Option<WorkspaceSession>,
    pub remote_project_context: Option<Arc<crate::remote_project_context::RemoteProjectContext>>,
    /// The directory Caudra itself runs in. Set only in a sandbox session,
    /// where `{cwd}` names a path inside the VM and this one does not.
    pub host_cwd: Option<PathBuf>,
    pub local_documents: Option<Arc<LocalDocumentStore>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteEnvironment {
    pub cwd: String,
    pub platform: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteCommandOutput {
    pub output: String,
    pub is_error: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteCommandResult {
    exit_code: Option<i32>,
    signal: Option<i32>,
    timed_out: bool,
    output_limit_exceeded: bool,
    stdout: String,
    stderr: String,
}

struct BoundedRemoteOutput {
    text: String,
    max_lines: usize,
    max_bytes: usize,
    lines: usize,
    ends_with_newline: bool,
    truncated: bool,
}

impl BoundedRemoteOutput {
    fn new(max_lines: usize, max_bytes: usize) -> Self {
        Self {
            text: String::new(),
            max_lines: max_lines.max(1),
            max_bytes: max_bytes.max(1),
            lines: 0,
            ends_with_newline: false,
            truncated: false,
        }
    }

    fn push(&mut self, text: &str) {
        if self.truncated {
            return;
        }
        for character in text.chars() {
            let starts_line = self.text.is_empty() || self.ends_with_newline;
            if self.text.len().saturating_add(character.len_utf8()) > self.max_bytes
                || self.lines + usize::from(starts_line) > self.max_lines
            {
                self.truncated = true;
                return;
            }
            self.text.push(character);
            if starts_line {
                self.lines += 1;
            }
            self.ends_with_newline = character == '\n';
        }
    }

    fn finish(mut self) -> String {
        if self.truncated {
            self.marker(REMOTE_COMMAND_TRUNCATED);
        }
        self.text
    }

    fn marker(&mut self, marker: &str) {
        let separator = usize::from(!self.text.is_empty() && !self.text.ends_with('\n'));
        while self
            .text
            .len()
            .saturating_add(separator)
            .saturating_add(marker.len())
            > self.max_bytes
            || remote_output_line_count(&self.text)
                .saturating_add(separator)
                .saturating_add(1)
                > self.max_lines
        {
            if self.text.pop().is_none() {
                break;
            }
        }
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.text.push('\n');
        }
        let remaining = self.max_bytes.saturating_sub(self.text.len());
        self.text.extend(marker.chars().take(remaining));
        self.truncated = false;
    }
}

fn remote_output_line_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.bytes().filter(|byte| *byte == b'\n').count() + usize::from(!text.ends_with('\n'))
}

pub fn direct_shell_command(prompt: &str) -> Option<&str> {
    let command = prompt
        .strip_prefix("!!")
        .or_else(|| prompt.strip_prefix('!'))?
        .trim();
    (!command.is_empty()).then_some(command)
}

pub async fn execute_remote_command(
    workspace: &WorkspaceSession,
    baseline: &WorkspaceBaseline,
    command: &str,
    cancel: &CancelToken,
    max_output_lines: usize,
    max_output_bytes: usize,
    mut progress: impl FnMut(&str),
) -> RemoteCommandOutput {
    let baseline = match baseline.ensure_current().await {
        crate::BaselineOutcome::Ready => Ok(()),
        crate::BaselineOutcome::Unavailable(reason) => Err(reason.to_string()),
        crate::BaselineOutcome::Failed(error) => Err(error.to_string()),
    };
    if let Err(output) = baseline {
        return RemoteCommandOutput {
            output,
            is_error: true,
        };
    }
    let result = run_remote_command(
        workspace,
        command,
        cancel,
        max_output_lines,
        max_output_bytes,
        &mut progress,
    )
    .await;
    match result {
        Ok(output) => RemoteCommandOutput {
            output,
            is_error: false,
        },
        Err(output) => RemoteCommandOutput {
            output,
            is_error: true,
        },
    }
}

async fn run_remote_command(
    workspace: &WorkspaceSession,
    command: &str,
    cancel: &CancelToken,
    max_output_lines: usize,
    max_output_bytes: usize,
    progress: &mut impl FnMut(&str),
) -> Result<String, String> {
    if cancel.is_cancelled() {
        return Err(REMOTE_COMMAND_CANCELLED.into());
    }
    let service = workspace
        .workspace()
        .services()
        .exec
        .as_ref()
        .ok_or_else(|| "Remote command execution is unavailable".to_owned())?;
    let request = ExecRequest {
        command: CommandText::new(command).map_err(|error| error.to_string())?,
        timeout_ms: Some(REMOTE_COMMAND_TIMEOUT.as_millis() as u64),
    };
    let mut status = service
        .execute(workspace.binding(), workspace.cursor(), &request)
        .await
        .map_err(remote_command_error)?;
    let mut output = BoundedRemoteOutput::new(max_output_lines, max_output_bytes);
    let mut next_sequence = None;
    let mut reported_gap = false;
    let mut cancelling = cancel.is_cancelled();

    loop {
        append_remote_command_progress(&status, &mut output, &mut next_sequence, &mut reported_gap);
        if !output.text.is_empty() {
            progress(&output.text);
        }
        match &status.state {
            OperationState::Completed { result, .. } => {
                let terminal = remote_command_terminal(result, output)?;
                progress(&terminal);
                return Ok(terminal);
            }
            OperationState::Failed { .. } => {
                output.marker(REMOTE_COMMAND_FAILED);
                return Err(output.finish());
            }
            OperationState::Cancelled {
                side_effects_possible,
            } => {
                output.marker(if *side_effects_possible {
                    REMOTE_COMMAND_INDETERMINATE
                } else {
                    REMOTE_COMMAND_CANCELLED
                });
                return Err(output.finish());
            }
            OperationState::Indeterminate { .. }
            | OperationState::Forgotten
            | OperationState::NeverSeen => {
                output.marker(REMOTE_COMMAND_INDETERMINATE);
                return Err(output.finish());
            }
            OperationState::Prepared => {
                output.marker("Remote command did not start");
                return Err(output.finish());
            }
            OperationState::Running => {}
        }

        if cancelling {
            let _ = service
                .cancel(workspace.binding(), workspace.cursor(), &status.handle)
                .await;
            status = service
                .status(workspace.binding(), workspace.cursor(), &status.handle)
                .await
                .map_err(remote_command_error)?;
            smol::Timer::after(REMOTE_COMMAND_POLL_INTERVAL).await;
            continue;
        }
        match cancel
            .race(async {
                smol::Timer::after(REMOTE_COMMAND_POLL_INTERVAL).await;
                service
                    .status(workspace.binding(), workspace.cursor(), &status.handle)
                    .await
            })
            .await
        {
            Ok(result) => status = result.map_err(remote_command_error)?,
            Err(_) => cancelling = true,
        }
    }
}

fn append_remote_command_progress(
    status: &OperationStatus<Value>,
    output: &mut BoundedRemoteOutput,
    next_sequence: &mut Option<u64>,
    reported_gap: &mut bool,
) {
    let first = status.progress_metadata.first_retained_sequence;
    let gap = status.progress_metadata.gap_before_first
        || next_sequence
            .zip(first)
            .is_some_and(|(expected, actual)| actual > expected);
    if gap && !*reported_gap {
        output.push(REMOTE_COMMAND_PROGRESS_GAP);
        output.push("\n");
        *reported_gap = true;
    }
    if next_sequence.is_none() {
        *next_sequence = first;
    }
    for item in &status.progress {
        if next_sequence.is_some_and(|expected| item.sequence < expected) {
            continue;
        }
        if next_sequence.is_some_and(|expected| item.sequence > expected) && !*reported_gap {
            output.push(REMOTE_COMMAND_PROGRESS_GAP);
            output.push("\n");
            *reported_gap = true;
        }
        *next_sequence = Some(item.sequence.saturating_add(1));
        match &item.kind {
            OperationProgressKind::Stdout | OperationProgressKind::Stderr => {
                output.push(&item.chunk)
            }
            OperationProgressKind::Started
            | OperationProgressKind::Exited
            | OperationProgressKind::Unknown(_)
                if !item.chunk.is_empty() =>
            {
                output.push("[");
                output.push(&item.chunk);
                output.push("]\n");
            }
            OperationProgressKind::Started
            | OperationProgressKind::Exited
            | OperationProgressKind::Unknown(_) => {}
        }
    }
}

fn remote_command_terminal(
    value: &Value,
    mut output: BoundedRemoteOutput,
) -> Result<String, String> {
    let result: RemoteCommandResult = serde_json::from_value(value.clone())
        .map_err(|_| "Remote command returned an invalid result".to_owned())?;
    if output.text.is_empty() {
        output.push(&result.stdout);
        output.push(&result.stderr);
    }
    let mut statuses = Vec::new();
    if result.timed_out {
        statuses.push("timed out".to_owned());
    }
    if result.output_limit_exceeded {
        statuses.push("output limit exceeded".to_owned());
    }
    if let Some(code) = result.exit_code {
        statuses.push(format!("exit code {code}"));
    } else if let Some(signal) = result.signal {
        statuses.push(format!("signal {signal}"));
    } else if statuses.is_empty() {
        statuses.push("exit unknown".to_owned());
    }
    output.push("\n[shell status: ");
    output.push(&statuses.join("; "));
    output.push("]");
    let output = output.finish();
    if result.exit_code == Some(0) && !result.timed_out && !result.output_limit_exceeded {
        Ok(output)
    } else {
        Err(output)
    }
}

fn remote_command_error(error: WorkspaceError) -> String {
    match error {
        WorkspaceError::PolicyDenied | WorkspaceError::PermissionDenied => {
            "Remote command was denied by policy".into()
        }
        WorkspaceError::Cancelled => REMOTE_COMMAND_CANCELLED.into(),
        WorkspaceError::IndeterminateOutcome => REMOTE_COMMAND_INDETERMINATE.into(),
        WorkspaceError::PendingOperation { .. } => {
            "Remote command is blocked by a pending remote operation".into()
        }
        _ => "Remote command execution failed".into(),
    }
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
    chat_model: &'a Model,
    thinking: &'a crate::ThinkingConfig,
    model_policy: &'a ModelPolicy,
    timeouts: Timeouts,
    remote_workspace: bool,
    remote_project_context: Option<&'a Arc<crate::remote_project_context::RemoteProjectContext>>,
    host_cwd: Option<&'a Path>,
}

fn setup(
    model: &Model,
    config: &AgentConfig,
    excluded_tools: &[&'static str],
    workflows_available: bool,
    task: TaskDescriptionContext<'_>,
    remote_environment: Option<&RemoteEnvironment>,
    remote_workspace: bool,
) -> AgentSetup {
    let vars = if let Some(environment) = remote_environment {
        template::env_vars()
            .set("{cwd}", environment.cwd.clone())
            .set("{platform}", environment.platform.clone())
    } else {
        template::env_vars()
    };
    let instructions = match task.remote_project_context {
        Some(context) => agent::load_remote_instructions(context, task.host_cwd),
        None if remote_environment.is_none() => agent::load_instructions(&vars.apply("{cwd}")),
        None => agent::Instructions::default(),
    };
    let definitions = tool_definitions(
        &vars,
        model,
        config,
        excluded_tools,
        ToolRegistry::global(),
        workflows_available,
        task,
    );

    AgentSetup {
        vars,
        instructions,
        tools: definitions.declared,
        deferred: definitions.deferred,
        tool_filter: ToolFilter::from_config(config, model, excluded_tools)
            .for_remote_workspace(remote_workspace),
    }
}

fn main_turn_purpose(mode: &AgentMode) -> Option<ModelPurpose> {
    mode.is_planning().then_some(ModelPurpose::Plan)
}

async fn resolve_main_turn_model(
    mode: &AgentMode,
    chat_provider: &Arc<dyn Provider>,
    chat_model: &Model,
    timeouts: Timeouts,
    model_policy: &ModelPolicy,
) -> Result<(Arc<dyn Provider>, Model), AgentError> {
    match main_turn_purpose(mode) {
        Some(purpose) => {
            agent::resolve_model_for_purpose(
                agent::ModelRoute {
                    provider: chat_provider,
                    model: chat_model,
                },
                agent::ModelRoute {
                    provider: chat_provider,
                    model: chat_model,
                },
                purpose,
                None,
                timeouts,
                model_policy,
            )
            .await
        }
        None => Ok((Arc::clone(chat_provider), chat_model.clone())),
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
    registry: &ToolRegistry,
    workflows_available: bool,
    task: TaskDescriptionContext<'_>,
) -> ToolDefinitions {
    let filter = ToolFilter::from_config(config, model, excluded_tools)
        .for_remote_workspace(task.remote_workspace);
    let bindings = task.prompt_profiles.bind_for_tasks(
        model,
        task.chat_model,
        task.thinking,
        task.model_policy,
        task.timeouts,
    );
    let vars = vars.clone().set(
        "{task_system_prompt_profiles}",
        bindings.task_tool_summary("Caudra's built-in task prompt"),
    );
    let ctx = DescriptionContext {
        filter: &filter,
        audience: ToolAudience::MAIN,
        workflows_available,
    };
    registry.definitions_split(
        &vars,
        &ctx,
        model.supports_tool_examples(),
        &deferral::deferred_names(
            &config.allowed_tools,
            BuiltinDeferral::resolve(config, model),
        ),
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
    let mut sections: Vec<String> = DeferralSession::new(deferred.to_vec(), std::iter::empty())
        .request_snapshot()
        .extend_declared(&mut probe)
        .into_iter()
        .collect();
    if let Some(mcp) = mcp {
        sections.extend(mcp.request_snapshot().extend_declared(&mut probe));
    }
    crate::tools::deferral::push_catalog(&mut probe, &sections);
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
        false,
        TaskDescriptionContext {
            prompt_profiles: &params.prompt_profiles,
            chat_model: &params.model,
            thinking: &params.thinking,
            model_policy: &params.model_policy,
            timeouts: params.timeouts,
            remote_workspace: params.workspace_session.is_some(),
            remote_project_context: params.remote_project_context.as_ref(),
            host_cwd: params.host_cwd.as_deref(),
        },
        params.remote_environment.as_ref(),
        params.workspace_session.is_some(),
    );

    let system = params.local_documents.as_ref().map_or_else(
        || {
            agent::build_system_prompt(
                &instructions.text,
                &params.prompt_slots,
                &tool_filter,
                params.system_prompt_profile.as_deref(),
            )
        },
        |store| {
            agent::build_system_prompt_for_remote(
                &instructions.text,
                &params.prompt_slots,
                &tool_filter,
                params.system_prompt_profile.as_deref(),
                store,
            )
        },
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
            let permissions = Arc::new(PermissionManager::new_persistent(
                params.permissions_config,
                working_dir_path,
                params.plugin_rules,
            ));
            if let Err(error) = permissions.replace_remote_permission_asset(
                params
                    .remote_project_context
                    .as_ref()
                    .and_then(|context| context.permissions()),
            ) {
                let _ = error_tx.send(AgentEvent::Error {
                    message: format!("Remote permission policy unavailable: {error}"),
                });
                return;
            }
            let workspace_baseline = match params.workspace_session.as_ref() {
                Some(workspace) => {
                    let binding = match StoredWorkspaceBinding::new_with_cursor(
                        workspace.binding().clone(),
                        workspace.cursor().clone(),
                        None,
                    ) {
                        Ok(binding) => binding,
                        Err(error) => {
                            let _ = error_tx.send(AgentEvent::Error {
                                message: format!(
                                    "Remote workspace snapshot identity is invalid: {error}"
                                ),
                            });
                            return;
                        }
                    };
                    let storage = match StateDir::resolve() {
                        Ok(storage) => storage,
                        Err(error) => {
                            let _ = error_tx.send(AgentEvent::Error {
                                message: format!(
                                    "Remote workspace snapshot storage is unavailable: {error}"
                                ),
                            });
                            return;
                        }
                    };
                    Some(WorkspaceBaseline::new_workspace_session(
                        storage,
                        session_id,
                        workspace.clone(),
                        binding,
                        true,
                    ))
                }
                None => None,
            };
            let mut agent = Agent::new(
                AgentParams {
                    provider: Arc::clone(&provider),
                    model: model.clone(),
                    chat_provider: provider,
                    chat_model: model,
                    config: params.config,
                    tool_output_lines: ToolOutputLines::default(),
                    permissions,
                    session_id: Some(session_ref_clone.clone()),
                    cache_key: Some(CacheKey::session(&session_ref_clone)),
                    workspace_session: params.workspace_session.clone(),
                    remote_project_context: params.remote_project_context.clone(),
                    host_cwd: params.host_cwd.clone(),
                    local_documents: params.local_documents.clone(),
                    task_environment: vars.clone(),
                    root_tool_use_id: None,
                    mailbox: Some(mailbox.clone()),
                    context_publisher: None,
                    timeouts: params.timeouts,
                    file_tracker: FileReadTracker::fresh(),
                    path_locks: PathLocks::fresh(),
                    baseline: workspace_baseline.map(|baseline| BaselineGate::new(baseline, None)),
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
                    workflow: None,
                },
                AgentRunParams {
                    history: &mut history,
                    system,
                    environment: Some(agent::environment_block(&vars, &params.model)),
                    instructions: None,
                    mode_notice: None,
                    event_tx,
                    tools,
                    deferred,
                },
            )
            .with_loaded_instructions(instructions.loaded)
            .with_goal(params.goal)
            .with_background_wait()
            .with_mcp(mcp);

            let mentions = if params.remote_environment.is_some() {
                mentions::scan_remote(&params.prompt)
                    .into_iter()
                    .map(|(_, mention)| mention)
                    .collect()
            } else {
                let root = params.initial_wd.clone();
                mentions::scan(&params.prompt, |path| root.join(path).exists())
                    .into_iter()
                    .map(|(_, mention)| mention)
                    .collect()
            };
            // Every well-formed hash is admitted here rather than checked
            // against a log window. A headless prompt is scanned once and named
            // a revision on purpose, so an unknown one is worth an error note;
            // the composer is the surface that must not misread prose, because
            // it rescans on every keystroke.
            let commits = commits::scan(&params.prompt, |_| true)
                .into_iter()
                .map(|(_, commit)| commit)
                .collect();
            let result = agent
                .run(AgentInput {
                    message: params.prompt,
                    mode,
                    images: params.images,
                    mentions,
                    commits,
                    preamble: Vec::new(),
                    thinking: params.thinking,
                    fast,
                    prompt: None,
                    resume: false,
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
    pub model_policy: Arc<ModelPolicy>,
    pub plugin_rules: Arc<PluginRuleStore>,
    /// Host-side overrides that shadow a registered tool's execution while
    /// keeping its advertised schema (e.g. ACP answers `question` via elicitation).
    pub local_tools: LocalTools,
    /// `Some` attaches a workflow runtime to the session, whose agents are
    /// capped by the mode this returns as each of them starts. `None` leaves
    /// the `workflow` tool reporting unavailable.
    pub workflow_mode: Option<ModeResolver>,
    pub workspace_binding: Option<StoredWorkspaceBinding>,
    pub remote_environment: Option<RemoteEnvironment>,
    pub workspace_session: Option<WorkspaceSession>,
    pub remote_project_context: Option<Arc<crate::remote_project_context::RemoteProjectContext>>,
    /// The directory Caudra itself runs in. Set only in a sandbox session,
    /// where `{cwd}` names a path inside the VM and this one does not.
    pub host_cwd: Option<PathBuf>,
    pub local_documents: Option<Arc<LocalDocumentStore>>,
}

pub struct InteractiveHandle {
    pub event_rx: Receiver<Envelope>,
    pub tool_names: Vec<String>,
    pub input_tx: flume::Sender<AgentInput>,
    pub answer_tx: flume::Sender<String>,
    pub cancel_tx: flume::Sender<()>,
    pub model_tx: flume::Sender<Model>,
    pub model_route: Option<InteractiveModelRoute>,
    pub session_id: SessionRef,
    pub session_lease: Arc<SessionLease>,
    pub permissions: Arc<PermissionManager>,
    /// The session's workflow runtime, when `workflow_mode` asked for one.
    pub workflow: Option<WorkflowHandle>,
    workspace_change_tx: flume::Sender<WorkspaceChangeRequest>,
    remote_workspace: Option<Arc<StdMutex<RemoteWorkspaceState>>>,
    workspace_baseline: Option<Arc<WorkspaceBaseline>>,
    pub task: smol::Task<()>,
}

struct RemoteWorkspaceState {
    session: WorkspaceSession,
    binding: StoredWorkspaceBinding,
    cwd: String,
}

struct WorkspaceChangeRequest {
    previous: WorkspaceCursor,
    session: WorkspaceSession,
    binding: StoredWorkspaceBinding,
    context: Arc<crate::remote_project_context::RemoteProjectContext>,
    cwd: String,
    response: flume::Sender<Result<(), String>>,
}

type LiveModel = (Arc<dyn Provider>, Arc<Model>);

#[derive(Clone)]
pub struct InteractiveModelRoute {
    model: Arc<ArcSwap<LiveModel>>,
    timeouts: Timeouts,
}

impl InteractiveModelRoute {
    fn install(&self, provider: Arc<dyn Provider>, model: Model) {
        self.model.store(Arc::new((provider, Arc::new(model))));
    }

    async fn set(&self, mut model: Model) -> Result<Model, AgentError> {
        let provider = provider::from_model_async(&mut model, self.timeouts).await?;
        self.install(Arc::from(provider), model.clone());
        Ok(model)
    }
}

impl InteractiveHandle {
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(
        session_lease: Arc<SessionLease>,
        permissions: Arc<PermissionManager>,
        answer_tx: flume::Sender<String>,
    ) -> Self {
        Self {
            event_rx: flume::unbounded().1,
            tool_names: Vec::new(),
            input_tx: flume::unbounded().0,
            answer_tx,
            cancel_tx: flume::unbounded().0,
            model_tx: flume::unbounded().0,
            model_route: None,
            session_id: SessionRef::from(session_lease.id()),
            session_lease,
            permissions,
            workflow: None,
            workspace_change_tx: flume::unbounded().0,
            remote_workspace: None,
            workspace_baseline: None,
            task: smol::spawn(async {}),
        }
    }
    pub fn remote_cwd(&self) -> Option<String> {
        self.remote_workspace
            .as_ref()
            .and_then(|state| state.lock().ok().map(|state| state.cwd.clone()))
    }
    pub fn remote_workspace_session(&self) -> Option<WorkspaceSession> {
        self.remote_workspace
            .as_ref()
            .and_then(|state| state.lock().ok().map(|state| state.session.clone()))
    }

    pub fn remote_workspace_baseline(&self) -> Option<Arc<WorkspaceBaseline>> {
        self.workspace_baseline.clone()
    }

    pub async fn set_model(&self, model: Model) -> Result<Model, AgentError> {
        let model = match &self.model_route {
            Some(route) => route.set(model).await?,
            None => model,
        };
        self.model_tx
            .send(model.clone())
            .map_err(|_| AgentError::Channel)?;
        Ok(model)
    }

    pub async fn change_remote_directory(&self, path: &str) -> Result<String, String> {
        let state = self
            .remote_workspace
            .as_ref()
            .ok_or_else(|| "cd: remote workspace is unavailable".to_owned())?;
        let (workspace, stored_binding) = {
            let state = state
                .lock()
                .map_err(|_| "cd: remote workspace state is unavailable".to_owned())?;
            (state.session.clone(), state.binding.clone())
        };
        let path = DirectoryNavigation::new(path)
            .map_err(|_| "cd: invalid remote workspace path".to_owned())?;
        let service = workspace
            .workspace()
            .services()
            .read
            .as_ref()
            .ok_or_else(|| "cd: remote directory resolution is unavailable".to_owned())?;
        let resolved = service
            .navigate_directory(workspace.binding(), workspace.cursor(), &path)
            .await
            .map_err(|_| "cd: remote directory could not be resolved".to_owned())?;
        let cwd = resolved
            .resource
            .path
            .as_ref()
            .map(ToString::to_string)
            .ok_or_else(|| "cd: remote directory response is invalid".to_owned())?;
        let workspace = workspace
            .with_cursor(resolved)
            .map_err(|_| "cd: remote directory response is invalid or stale".to_owned())?;
        let binding = stored_binding
            .with_cursor(workspace.cursor().clone())
            .map_err(|_| "cd: remote workspace identity changed".to_owned())?;
        let context =
            match crate::remote_project_context::load_remote_project_context(&workspace).await {
                Ok(context) => context,
                Err(_) => {
                    return Err("cd: remote project context could not be refreshed".to_owned());
                }
            };
        let (response, received) = flume::bounded(1);
        self.workspace_change_tx
            .send_async(WorkspaceChangeRequest {
                previous: stored_binding
                    .cursor()
                    .ok_or("cd: remote cursor is unavailable")?
                    .clone(),
                session: workspace.clone(),
                binding: binding.clone(),
                context,
                cwd: cwd.clone(),
                response,
            })
            .await
            .map_err(|_| "cd: session is no longer running".to_owned())?;
        received
            .recv_async()
            .await
            .map_err(|_| "cd: session is no longer running".to_owned())??;
        Ok(cwd)
    }

    pub async fn remote_control(&self, args: &str) -> Result<String, String> {
        let workspace = self
            .remote_workspace_session()
            .ok_or_else(|| "Remote workspace control is unavailable".to_owned())?;
        caudra_workspace::execute_workspace_control(&workspace, args).await
    }
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
    workspace_baseline: Option<Arc<WorkspaceBaseline>>,
}

impl PreparedInteractive {
    pub fn set_mcp_handle(&mut self, mcp_handle: Option<McpHandle>) {
        self.params.mcp_handle = mcp_handle;
    }
}

pub async fn prepare_interactive(
    mut params: InteractiveParams,
) -> Result<PreparedInteractive, InteractiveStartError> {
    if params.remote_environment.is_some() != params.workspace_session.is_some() {
        return Err(InteractiveStartError(
            "remote workspace and environment must be supplied together".into(),
        ));
    }
    let history = History::restored(std::mem::take(&mut params.initial_history))
        .map_err(|error| InteractiveStartError(format!("Failed to restore history: {error}")))?;
    let mut model = params.model.clone();
    let provider: Arc<dyn Provider> = provider::from_model_async(&mut model, params.timeouts)
        .await
        .map(Arc::from)
        .map_err(|error| InteractiveStartError(error.user_message()))?;
    let working_dir = params.initial_wd.to_string_lossy().into_owned();
    let session_id = params.session_id.id();
    let mut store = if let Some(workspace) = params.workspace_session.clone() {
        let dir = StateDir::resolve().map_err(|error| InteractiveStartError(error.to_string()))?;
        params
            .session_lease
            .validate(&dir, session_id)
            .map_err(|error| InteractiveStartError(error.to_string()))?;
        let mut session = match crate::load_stored_session(session_id, &dir) {
            Ok(session) => session,
            Err(caudra_storage::sessions::SessionError::Storage(
                caudra_storage::StorageError::NotFound(_),
            )) => {
                let binding = params.workspace_binding.clone().ok_or_else(|| {
                    InteractiveStartError("remote workspace binding missing".into())
                })?;
                let mut session =
                    StoredSession::new_with_workspace(&model.spec(), &working_dir, binding);
                session.id = session_id;
                session
            }
            Err(error) => return Err(InteractiveStartError(error.to_string())),
        };
        StoredWorkspaceBinding::validate_resume_identity(
            session.workspace_binding(),
            params.workspace_binding.as_ref(),
        )
        .map_err(|error| InteractiveStartError(error.to_string()))?;
        let workspace = crate::resume_workspace_session(&mut session, &dir, &workspace)
            .await
            .map_err(InteractiveStartError)?;
        params.initial_wd = session.cwd.clone().into();
        if let Some(environment) = &mut params.remote_environment {
            environment.cwd = session.cwd.clone();
        }
        params.remote_project_context = Some(
            crate::remote_project_context::load_remote_project_context(&workspace)
                .await
                .map_err(|error| InteractiveStartError(error.to_string()))?,
        );
        params.workspace_session = Some(workspace);
        params.workspace_binding = session.workspace_binding().cloned();
        let created = session.persisted_write_version().is_none();
        SessionStore::from_session(dir, session, created, Arc::clone(&params.session_lease))
    } else {
        SessionStore::open(
            session_id,
            &working_dir,
            &model.spec(),
            Arc::clone(&params.session_lease),
            params.workspace_binding.as_ref(),
        )
        .map_err(|error| {
            InteractiveStartError(format!("Session persistence unavailable: {error}"))
        })?
    };
    store
        .verify_start_version(params.expected_write_version)
        .map_err(|error| {
            InteractiveStartError(format!("Session changed before startup: {error}"))
        })?;
    store.set_system_prompt_profile(params.system_prompt_profile_name.as_deref());
    store
        .save()
        .map_err(|error| InteractiveStartError(format!("Failed to persist session: {error}")))?;
    let workspace_baseline = match (
        params.workspace_session.clone(),
        params.workspace_binding.clone(),
    ) {
        (Some(workspace), Some(binding)) => Some(WorkspaceBaseline::new_workspace_session(
            store.dir.clone(),
            session_id,
            workspace,
            binding,
            true,
        )),
        (None, None) => None,
        _ => {
            return Err(InteractiveStartError(
                "Remote workspace snapshot identity is unavailable".into(),
            ));
        }
    };
    if let Some(baseline) = &workspace_baseline {
        baseline.set_current_head(store.history_head());
        if let Some(status) = baseline.reconcile_remote_restore().await.map_err(|error| {
            InteractiveStartError(format!("Remote snapshot recovery failed: {error}"))
        })? {
            return Err(InteractiveStartError(format!(
                "Remote snapshot recovery is required before this session can mutate ({:?})",
                status.state
            )));
        }
    }
    Ok(PreparedInteractive {
        params,
        history,
        model,
        provider,
        store,
        workspace_baseline,
    })
}

pub async fn spawn_prepared_interactive(
    prepared: PreparedInteractive,
) -> Result<InteractiveHandle, InteractiveStartError> {
    let PreparedInteractive {
        mut params,
        mut history,
        mut model,
        mut provider,
        mut store,
        workspace_baseline,
    } = prepared;
    let workflows_available = params.workflow_mode.is_some();
    let AgentSetup {
        mut vars,
        instructions,
        tools,
        deferred,
        tool_filter,
    } = setup(
        &model,
        &params.config,
        &params.excluded_tools,
        workflows_available,
        TaskDescriptionContext {
            prompt_profiles: &params.prompt_profiles,
            chat_model: &model,
            thinking: &params.thinking,
            model_policy: &params.model_policy,
            timeouts: params.timeouts,
            remote_workspace: params.workspace_session.is_some(),
            remote_project_context: params.remote_project_context.as_ref(),
            host_cwd: params.host_cwd.as_deref(),
        },
        params.remote_environment.as_ref(),
        params.workspace_session.is_some(),
    );

    let mut baseline = agent::InstructionBaseline::adopt(instructions, history.epoch());

    let initial_messages = history.as_slice();
    let mcp = params.mcp_handle.clone().map(|h| {
        McpSession::new(h, initial_messages).with_disabled_tools(&params.config.disabled_tools)
    });
    let tool_names = advertised_tool_names(&tools, &deferred, mcp.as_ref());

    let session_ref = params.session_id.clone();
    let session_id = session_ref.id();
    let session_lease = Arc::clone(&params.session_lease);
    let subagent_history = store.subagent_history.clone();
    let state_dir = store.dir.clone();
    let initial_history_head = store.history_head();

    let (raw_tx, event_rx) = flume::unbounded::<Envelope>();
    let (agent_tx, agent_rx) = flume::unbounded::<Envelope>();
    let (input_tx, input_rx) = flume::unbounded::<AgentInput>();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();
    let (cancel_tx, cancel_rx) = flume::bounded::<()>(1);
    let (model_tx, model_rx) = flume::unbounded::<Model>();
    let (workspace_change_tx, workspace_change_rx) = flume::unbounded::<WorkspaceChangeRequest>();
    let remote_workspace = params
        .workspace_session
        .clone()
        .zip(params.workspace_binding.clone())
        .map(|(session, binding)| {
            Arc::new(StdMutex::new(RemoteWorkspaceState {
                session,
                binding,
                cwd: params.initial_wd.to_string_lossy().into_owned(),
            }))
        });

    let mut permissions_config = params.permissions_config;
    permissions_config.yolo |= params.yolo;
    let permissions = Arc::new(PermissionManager::new_persistent_in(
        permissions_config,
        params.initial_wd.clone(),
        Arc::clone(&params.plugin_rules),
        state_dir.clone(),
    ));
    if let Err(error) = permissions.replace_remote_permission_asset(
        params
            .remote_project_context
            .as_ref()
            .and_then(|context| context.permissions()),
    ) {
        return Err(InteractiveStartError(format!(
            "Remote permission policy unavailable: {error}"
        )));
    }
    permissions.load_structured_conversation_rules(std::mem::take(
        &mut params.structured_permission_rules,
    ));
    permissions.set_session_yolo(params.session_yolo);
    permissions
        .attach_permission_publication(store.permission_publication().map_err(|error| {
            InteractiveStartError(format!(
                "Conversation permission storage unavailable: {error}"
            ))
        })?)
        .map_err(|error| {
            InteractiveStartError(format!(
                "Failed to attach conversation permissions: {error}"
            ))
        })?;
    let store = Arc::new(Mutex::new(Some(store)));

    let answer_rx = Arc::new(Mutex::new(answer_rx));
    let active_prompt_profile_name: Arc<str> = Arc::from(
        params
            .system_prompt_profile_name
            .as_deref()
            .unwrap_or(BUILTIN_PROFILE_NAME),
    );
    // What every top-level agent of the session shares; a turn swaps in its
    // provider, model, and filter, and gets a cancel map of its own.
    let base = AgentParams {
        provider: Arc::clone(&provider),
        model: model.clone(),
        chat_provider: Arc::clone(&provider),
        chat_model: model.clone(),
        config: params.config.clone(),
        tool_output_lines: ToolOutputLines::default(),
        permissions: Arc::clone(&permissions),
        session_id: Some(session_ref.clone()),
        cache_key: Some(CacheKey::session(&session_ref)),
        workspace_session: params.workspace_session.clone(),
        remote_project_context: params.remote_project_context.clone(),
        host_cwd: params.host_cwd.clone(),
        local_documents: params.local_documents.clone(),
        task_environment: vars.clone(),
        root_tool_use_id: None,
        mailbox: Some(SessionMailbox::register(session_id)),
        context_publisher: None,
        timeouts: params.timeouts,
        file_tracker: FileReadTracker::fresh(),
        path_locks: PathLocks::fresh(),
        baseline: workspace_baseline
            .as_ref()
            .map(|baseline| BaselineGate::new(Arc::clone(baseline), initial_history_head)),
        prompt_slots: Arc::clone(&params.prompt_slots),
        prompt_profiles: Arc::clone(&params.prompt_profiles),
        default_task_prompt_profile_name: Arc::clone(&active_prompt_profile_name),
        active_prompt_profile_name: Some(active_prompt_profile_name),
        subagent_cancels: Arc::new(CancelMap::new()),
        subagent_history,
        registry: Arc::clone(ToolRegistry::global_arc()),
        audience: ToolAudience::MAIN,
        tool_filter: tool_filter.clone(),
        model_policy: Arc::clone(&params.model_policy),
        workflow: None,
    };

    // Workflow agents resolve the provider at launch, so a model switched
    // mid-session reaches runs that outlive the turn which switched it.
    let live_model = Arc::new(ArcSwap::from_pointee((
        Arc::clone(&provider),
        Arc::new(model.clone()),
    )));
    let runtime = match params.workflow_mode.take() {
        Some(mode) => {
            let model: ModelResolver = Arc::new({
                let live_model = Arc::clone(&live_model);
                move || {
                    let live = live_model.load();
                    (Arc::clone(&live.0), Arc::clone(&live.1))
                }
            });
            let subagent_cancels = Arc::new(CancelMap::new());
            let host = WorkflowHostContext::from_agent_params(
                &base,
                HostExtras {
                    mcp: mcp.clone(),
                    loaded_instructions: baseline.loaded().clone(),
                    user_response_rx: Some(Arc::clone(&answer_rx)),
                },
                model,
                Arc::clone(&mode),
                Arc::clone(&subagent_cancels),
            );
            // A runtime that fails to open leaves the `workflow` tool reporting
            // unavailable rather than taking the session down.
            WorkflowRuntime::spawn(RuntimeDeps {
                state_dir: state_dir.clone(),
                session_id,
                cwd: params.initial_wd.clone(),
                user_config_dir: None,
                remote_project_context: params.remote_project_context.clone(),
                runner: Arc::new(SubagentTaskRunner::new(Arc::new(host))),
                events: agent_tx.clone(),
                mode,
                subagent_cancels,
            })
            .await
            .map_err(|error| warn!(%error, "workflow runtime unavailable for this session"))
            .ok()
        }
        None => None,
    };
    let workflow = runtime.as_ref().map(WorkflowRuntime::handle);
    let model_route = workflow.as_ref().map(|_| InteractiveModelRoute {
        model: Arc::clone(&live_model),
        timeouts: params.timeouts,
    });
    let mut base = AgentParams {
        workflow: workflow.clone(),
        ..base
    };

    let handle_workspace_baseline = workspace_baseline.clone();
    let task = smol::spawn({
        let permissions = Arc::clone(&permissions);
        let workflow = workflow.clone();
        let live_model = Arc::clone(&live_model);
        let remote_workspace = remote_workspace.clone();
        async move {
            let event_forwarder = smol::spawn({
                let store = Arc::clone(&store);
                let raw_tx = raw_tx.clone();
                let workspace_baseline = workspace_baseline.clone();
                async move {
                    while let Ok(envelope) = agent_rx.recv_async().await {
                        let persistence_error = if let Some(store) = &mut *store.lock().await {
                            let result = store.record_event(&envelope).err();
                            if let Some(baseline) = &workspace_baseline {
                                baseline.set_current_head(store.history_head());
                            }
                            result
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
                                    workflow: None,
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
            let mut acked_completions = HashSet::new();

            loop {
                enum NextInput {
                    Prompt(Result<AgentInput, flume::RecvError>),
                    Workspace(Box<Result<WorkspaceChangeRequest, flume::RecvError>>),
                }
                let next = futures_lite::future::race(
                    async { NextInput::Prompt(input_rx.recv_async().await) },
                    async {
                        NextInput::Workspace(Box::new(workspace_change_rx.recv_async().await))
                    },
                )
                .await;
                let mut input = match next {
                    NextInput::Prompt(Ok(input)) => input,
                    NextInput::Prompt(Err(_)) => break,
                    NextInput::Workspace(change) => {
                        let Ok(change) = *change else {
                            continue;
                        };
                        if !input_rx.is_empty()
                            || params
                                .workspace_session
                                .as_ref()
                                .is_none_or(|workspace| workspace.cursor() != &change.previous)
                        {
                            let _ = change.response.send(Err(
                                "cd: session has queued work or its cursor changed".into(),
                            ));
                            continue;
                        }
                        let transition = match crate::workflow::prepare_workspace_transition(
                            workflow.as_ref(),
                            base.subagent_cancels.active_count(),
                            WorkspaceRebind {
                                workspace: change.session.clone(),
                                context: Arc::clone(&change.context),
                                cwd: change.cwd.clone(),
                            },
                        )
                        .await
                        {
                            Ok(transition) => transition,
                            Err(error) => {
                                let _ = change.response.send(Err(error));
                                continue;
                            }
                        };
                        let result = if permissions
                            .replace_remote_permission_asset(change.context.permissions())
                            .is_err()
                        {
                            Err("cd: remote permission policy could not be refreshed".to_owned())
                        } else {
                            let persisted = if let Some(store) = &mut *store.lock().await {
                                let previous = store.session.clone();
                                let result = store
                                    .session
                                    .replace_workspace_cursor(change.binding.clone())
                                    .and_then(|()| {
                                        store.session.set_cwd(change.cwd.clone());
                                        store.save()
                                    });
                                if result.is_err() {
                                    store.session = previous;
                                }
                                result.map_err(|_| {
                                    "cd: remote workspace cursor could not be persisted".to_owned()
                                })
                            } else {
                                Err("cd: session persistence is unavailable".to_owned())
                            };
                            if persisted.is_ok() {
                                if let Some(baseline) = &workspace_baseline {
                                    baseline.rebind_workspace_session(
                                        state_dir.clone(),
                                        session_id,
                                        change.session.clone(),
                                        change.binding.clone(),
                                    );
                                }
                                vars = vars.clone().set("{cwd}", change.cwd.clone());
                                base.workspace_session = Some(change.session.clone());
                                base.remote_project_context = Some(Arc::clone(&change.context));
                                base.task_environment = vars.clone();
                                params.workspace_session = Some(change.session.clone());
                                params.remote_project_context = Some(change.context);
                                if let Some(environment) = &mut params.remote_environment {
                                    environment.cwd = change.cwd.clone();
                                }
                            }
                            persisted
                        };
                        let persisted = result.is_ok();
                        let result = match (result, transition) {
                            (Ok(()), Some(transition)) => {
                                transition.commit().await.map_err(|error| error.to_string())
                            }
                            (result, _) => result,
                        };
                        if result.is_ok() {
                            if let Some(state) = &remote_workspace {
                                match state.lock() {
                                    Ok(mut state) => {
                                        state.session = change.session;
                                        state.binding = change.binding;
                                        state.cwd = change.cwd;
                                    }
                                    Err(_) => {
                                        permissions.invalidate_remote_permission_asset();
                                        let _ = change.response.send(Err(
                                            "remote workspace state is unavailable".into(),
                                        ));
                                        break;
                                    }
                                }
                            }
                        } else {
                            permissions.invalidate_remote_permission_asset();
                        }
                        let fatal = persisted && result.is_err();
                        let _ = change.response.send(result);
                        if fatal {
                            break;
                        }
                        continue;
                    }
                };
                let input_mode = input.mode.clone();
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

                if let Some(workspace) = &params.workspace_session {
                    match crate::remote_project_context::load_remote_project_context(workspace)
                        .await
                    {
                        Ok(context) => {
                            let changed =
                                params.remote_project_context.as_ref().is_none_or(|old| {
                                    old.manifest_revision() != context.manifest_revision()
                                });
                            let transition = if changed {
                                match crate::workflow::prepare_workspace_transition(
                                    workflow.as_ref(),
                                    base.subagent_cancels.active_count(),
                                    WorkspaceRebind {
                                        workspace: workspace.clone(),
                                        context: Arc::clone(&context),
                                        cwd: vars.apply("{cwd}").into_owned(),
                                    },
                                )
                                .await
                                {
                                    Ok(transition) => transition,
                                    Err(message) => {
                                        permissions.invalidate_remote_permission_asset();
                                        let _ = error_tx.send(AgentEvent::Error { message });
                                        cancel_task.cancel().await;
                                        run_id += 1;
                                        continue;
                                    }
                                }
                            } else {
                                None
                            };
                            if let Err(error) =
                                permissions.replace_remote_permission_asset(context.permissions())
                            {
                                let _ = error_tx.send(AgentEvent::Error {
                                    message: format!(
                                        "Remote permission policy unavailable: {error}"
                                    ),
                                });
                                cancel_task.cancel().await;
                                run_id += 1;
                                continue;
                            }
                            base.remote_project_context = Some(Arc::clone(&context));
                            params.remote_project_context = Some(context);
                            if let Some(transition) = transition
                                && let Err(error) = transition.commit().await
                            {
                                let _ = error_tx.send(AgentEvent::Error {
                                    message: error.to_string(),
                                });
                                cancel_task.cancel().await;
                                run_id += 1;
                                continue;
                            }
                        }
                        Err(error) => {
                            permissions.invalidate_remote_permission_asset();
                            let _ = error_tx.send(AgentEvent::Error {
                                message: format!("Remote project context unavailable: {error}"),
                            });
                            cancel_task.cancel().await;
                            run_id += 1;
                            continue;
                        }
                    }
                }

                if let Some(mut new_model) = model_rx
                    .try_iter()
                    .last()
                    .filter(|candidate| params.model_policy.allows(&candidate.spec()))
                    && new_model.spec() != model.spec()
                {
                    let routed = live_model.load_full();
                    if routed.1.spec() == new_model.spec() {
                        provider = Arc::clone(&routed.0);
                        model = Model::clone(&routed.1);
                    } else {
                        match provider::from_model_async(&mut new_model, params.timeouts).await {
                            Ok(p) => {
                                provider = Arc::from(p);
                                model = new_model;
                                live_model.store(Arc::new((
                                    Arc::clone(&provider),
                                    Arc::new(model.clone()),
                                )));
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
                }

                let (turn_provider, turn_model) = match resolve_main_turn_model(
                    &input.mode,
                    &provider,
                    &model,
                    params.timeouts,
                    &params.model_policy,
                )
                .await
                {
                    Ok(resolved) => resolved,
                    Err(e) => {
                        error!(error = %e, "failed to resolve turn model");
                        let _ = error_tx.send(AgentEvent::Error {
                            message: e.user_message(),
                        });
                        cancel_task.cancel().await;
                        run_id += 1;
                        continue;
                    }
                };
                let turn_tool_filter =
                    ToolFilter::from_config(&params.config, &turn_model, &params.excluded_tools)
                        .for_remote_workspace(params.workspace_session.is_some());

                if let Some(workflow) = &workflow
                    && let Some(context) =
                        completion_context(workflow, &mut acked_completions).await
                {
                    input.message = format!("{context}{COMPLETION_SEPARATOR}{}", input.message);
                }

                let definitions = tool_definitions(
                    &vars,
                    &turn_model,
                    &params.config,
                    &params.excluded_tools,
                    ToolRegistry::global(),
                    workflows_available,
                    TaskDescriptionContext {
                        prompt_profiles: &params.prompt_profiles,
                        chat_model: &model,
                        thinking: &input.thinking,
                        model_policy: &params.model_policy,
                        timeouts: params.timeouts,
                        remote_workspace: params.workspace_session.is_some(),
                        remote_project_context: params.remote_project_context.as_ref(),
                        host_cwd: params.host_cwd.as_deref(),
                    },
                );

                let instructions = {
                    let current = match &params.remote_project_context {
                        Some(context) => {
                            agent::load_remote_instructions(context, params.host_cwd.as_deref())
                        }
                        None => {
                            let cwd = vars.apply("{cwd}").into_owned();
                            smol::unblock(move || agent::load_instructions(&cwd)).await
                        }
                    };
                    baseline.drift(current, history.epoch())
                };

                let mut system = params.system_prompt_override.clone().unwrap_or_else(|| {
                    params.local_documents.as_ref().map_or_else(
                        || {
                            agent::build_system_prompt(
                                baseline.text(),
                                &params.prompt_slots,
                                &turn_tool_filter,
                                params.system_prompt_profile.as_deref(),
                            )
                        },
                        |store| {
                            agent::build_system_prompt_for_remote(
                                baseline.text(),
                                &params.prompt_slots,
                                &turn_tool_filter,
                                params.system_prompt_profile.as_deref(),
                                store,
                            )
                        },
                    )
                });
                if let Some(append) = &params.append_system_prompt {
                    system.push('\n');
                    system.push_str(append);
                }

                while answer_rx.lock().await.try_recv().is_ok() {}

                let history_head = store
                    .lock()
                    .await
                    .as_ref()
                    .and_then(SessionStore::history_head);
                if let Some(workspace_baseline) = &workspace_baseline {
                    workspace_baseline.set_current_head(history_head);
                }
                base.baseline = workspace_baseline
                    .as_ref()
                    .map(|baseline| BaselineGate::new(Arc::clone(baseline), history_head));

                let mut agent = Agent::new(
                    AgentParams {
                        provider: turn_provider,
                        model: turn_model.clone(),
                        chat_provider: Arc::clone(&provider),
                        chat_model: model.clone(),
                        tool_filter: turn_tool_filter,
                        ..base.clone()
                    },
                    AgentRunParams {
                        history: &mut history,
                        system,
                        environment: Some(agent::environment_block(&vars, &turn_model)),
                        instructions,
                        mode_notice: None,
                        event_tx,
                        tools: definitions.declared,
                        deferred: definitions.deferred,
                    },
                )
                .with_loaded_instructions(baseline.loaded().clone())
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
                    store.set_mode(&input_mode);
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

            // Active runs are interrupted and their agents drained before the
            // session is saved, so nothing writes to it afterwards.
            base.subagent_cancels.cancel_all();
            if let Some(runtime) = runtime {
                runtime.shutdown().await;
            }
            drop(base);
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

    Ok(InteractiveHandle {
        event_rx,
        tool_names,
        input_tx,
        answer_tx,
        cancel_tx,
        model_tx,
        model_route,
        session_id: session_ref,
        session_lease,
        permissions,
        workflow,
        workspace_change_tx,
        remote_workspace,
        workspace_baseline: handle_workspace_baseline,
        task,
    })
}

pub async fn spawn_interactive(
    params: InteractiveParams,
) -> Result<InteractiveHandle, InteractiveStartError> {
    let prepared = prepare_interactive(params).await?;
    spawn_prepared_interactive(prepared).await
}

/// What finished since the last prompt, as one block per run for the next
/// one. Every notice it carries is acknowledged, so a run is reported once
/// per revision even when the runtime loses the ack.
async fn completion_context(
    workflow: &WorkflowHandle,
    acked: &mut HashSet<(String, u64)>,
) -> Option<String> {
    if workflow.pending_completions() == 0 {
        return None;
    }
    let runs = match workflow
        .request(WorkflowRequest::Status { run_id: None })
        .await
    {
        Ok(WorkflowResponse::Runs(runs)) => runs,
        Ok(_) => Vec::new(),
        Err(error) => {
            warn!(%error, "workflow completions could not be read");
            return None;
        }
    };
    let mut blocks = Vec::new();
    for run in runs.into_iter().filter(|run| run.outbox_pending) {
        let notice = (run.run_id.clone(), run.revision);
        if acked.contains(&notice) {
            continue;
        }
        blocks.push(completion_block(&run));
        if let Err(error) = workflow
            .request(WorkflowRequest::AckCompletion {
                run_id: run.run_id,
                revision: run.revision,
            })
            .await
        {
            warn!(%error, "workflow completion could not be acknowledged");
        }
        acked.insert(notice);
    }
    (!blocks.is_empty()).then(|| blocks.join(COMPLETION_SEPARATOR))
}

fn completion_block(run: &RunSnapshot) -> String {
    let mut block = format!(
        "Workflow {} ({}) finished with status {}.",
        run.display_name, run.workflow_name, run.status
    );
    if let Some(result) = &run.result {
        match result.get("report").and_then(Value::as_str) {
            Some(report) => block.push_str(&format!("\nReport: {}", bounded(report))),
            None => block.push_str(&format!("\nResult: {}", bounded(&result.to_string()))),
        }
        if let Some(path) = result.get("path").and_then(Value::as_str) {
            block.push_str(&format!("\nScratch file: {path}"));
        }
    }
    if let Some(message) = &run.pause_message {
        block.push_str(&format!("\nPaused: {message}"));
    }
    if let Some(error) = &run.error {
        block.push_str(&format!("\nError: {error}"));
    }
    block
}

fn bounded(text: &str) -> Cow<'_, str> {
    if text.len() <= COMPLETION_TEXT_LIMIT {
        return Cow::Borrowed(text);
    }
    let end = text.floor_char_boundary(COMPLETION_TEXT_LIMIT);
    Cow::Owned(format!("{}{TRUNCATED_SUFFIX}", &text[..end]))
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
    use caudra_providers::{AgentError, ProviderEvent, RequestOptions, StopReason, StreamResponse};
    use caudra_storage::permission_state::PermissionRuleRecord;
    use caudra_storage::permission_state::mutation::{
        PermissionMutation, PermissionRecordIdentity, prepare_mutation,
    };
    use caudra_storage::sessions::generate_title;
    use caudra_storage::tool_outputs::ToolOutputStore;
    use caudra_storage::workflow::WorkflowRunStatus;
    use caudra_workflow::{RunStatus, WorkflowError, WorkflowEvent};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::permissions::{
        PermissionAnswer, PermissionError, PermissionLifetime, PermissionRequest, RevokedRuleScope,
    };
    use crate::tools::PermissionScopes;
    use crate::tools::registry::BoxFuture;
    use crate::workflow::store::WorkflowStore;

    const SESSION_ID: &str = "CNK1hV6GWoysH3KQMm5wu";
    const CWD: &str = "/project";
    const MODEL_SPEC: &str = "anthropic/claude-test";
    const PERMISSION_REQUEST_ID: &str = "headless-permission";
    const PERMISSION_COMMAND: &str = "headless-permission-command";
    const PERMISSION_TOOL: &str = "bash";
    const PERMISSION_OPTION: &str = "allow_exact";
    const LOST_PERMISSION_ACK: &str = "permission committed but acknowledgment lost";

    #[test_case("!pwd", Some("pwd"); "visible_command")]
    #[test_case("!! pwd", Some("pwd"); "hidden_command")]
    #[test_case("!  printf ok", Some("printf ok"); "extra_spacing")]
    #[test_case("!", None; "empty_command")]
    #[test_case("not a command", None; "ordinary_prompt")]
    #[test_case(" !pwd", None; "leading_space")]
    fn direct_shell_commands_require_a_nonempty_leading_sigil(
        prompt: &str,
        expected: Option<&str>,
    ) {
        assert_eq!(direct_shell_command(prompt), expected);
    }

    #[test]
    fn remote_command_terminal_reports_nonzero_exit_without_duplicate_streams() {
        let mut output = BoundedRemoteOutput::new(20, 1024);
        output.push("streamed\n");
        let result = remote_command_terminal(
            &serde_json::json!({
                "exitCode": 7,
                "signal": null,
                "timedOut": false,
                "outputLimitExceeded": false,
                "stdout": "duplicate stdout",
                "stderr": "duplicate stderr"
            }),
            output,
        )
        .expect_err("nonzero exit must fail");

        assert!(result.contains("streamed"));
        assert!(result.contains("exit code 7"));
        assert!(!result.contains("duplicate"));
    }

    #[test_case(AgentMode::Build, None ; "build_uses_chat")]
    #[test_case(AgentMode::ReadOnly, None ; "read_only_uses_chat")]
    #[test_case(
        AgentMode::Plan(PathBuf::from("plan.md")),
        Some(ModelPurpose::Plan);
        "plan_uses_plan"
    )]
    fn main_turn_model_purpose_depends_only_on_plan_mode(
        mode: AgentMode,
        expected: Option<ModelPurpose>,
    ) {
        assert_eq!(main_turn_purpose(&mode), expected);
    }

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

    fn conversation_permission() -> PermissionRuleRecord {
        let request = PermissionRequest::from_legacy(
            PERMISSION_REQUEST_ID.into(),
            ToolKey::native(PERMISSION_TOOL),
            vec![PERMISSION_COMMAND.into()],
            serde_json::json!({"command": PERMISSION_COMMAND}),
            Path::new(CWD),
            false,
        );
        PermissionRuleRecord::conversation(
            request
                .option_rule(PERMISSION_OPTION, PermissionLifetime::Conversation)
                .unwrap(),
        )
        .unwrap()
    }

    async fn enforce_test_permission(
        permissions: &PermissionManager,
        response: Option<&Mutex<Receiver<String>>>,
        events: &EventSender,
    ) -> Result<(), PermissionError> {
        permissions
            .enforce_with_identity(
                &ToolKey::native(PERMISSION_TOOL),
                &PermissionScopes {
                    scopes: vec![PERMISSION_COMMAND.into()],
                    force_prompt: false,
                    plan_scoped: false,
                },
                &serde_json::json!({"command": PERMISSION_COMMAND}),
                events,
                response,
                PERMISSION_REQUEST_ID,
                &CancelToken::none(),
                None,
                None,
                false,
            )
            .await
    }

    async fn pending_permission(
        permissions: &Arc<PermissionManager>,
    ) -> smol::Task<Result<(), PermissionError>> {
        let (event_tx, event_rx) = flume::unbounded();
        let permissions = Arc::clone(permissions);
        let task = smol::spawn(async move {
            let (_answer_tx, answer_rx) = flume::unbounded();
            enforce_test_permission(
                &permissions,
                Some(&Mutex::new(answer_rx)),
                &EventSender::new(event_tx, 0),
            )
            .await
        });
        assert!(matches!(
            event_rx.recv_async().await.unwrap().event,
            AgentEvent::PermissionRequest(request) if request.id == PERMISSION_REQUEST_ID
        ));
        task
    }

    struct LostAckPublication(Arc<SessionPermissionPublication>);

    impl PermissionPublication for LostAckPublication {
        fn snapshot(&self) -> Result<PermissionSnapshot, PermissionEditError> {
            self.0.snapshot()
        }

        fn commit(
            &self,
            prepared: &PreparedPermissionMutation,
        ) -> Result<PermissionCommitReceipt, PermissionEditError> {
            self.0.commit(prepared)?;
            Err(PermissionEditError::Storage(LOST_PERMISSION_ACK.into()))
        }

        fn receipt(
            &self,
            operation_id: CaudraId,
        ) -> Result<Option<PermissionCommitReceipt>, PermissionEditError> {
            self.0.receipt(operation_id)
        }
    }

    #[test_case(false; "revoked")]
    #[test_case(true; "deleted")]
    fn publication_receipts_never_replay_retired_authority(deleted: bool) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let lease = Arc::new(SessionLease::acquire(&dir, session_id()).unwrap());
        let mut session = StoredSession::new(MODEL_SPEC, CWD);
        session.id = session_id();
        let mut store = SessionStore::from_session(dir, session, true, lease);
        let publication = store.permission_publication().unwrap();
        let initial = publication.snapshot().unwrap();
        assert!(initial.revision.row_present);
        assert_eq!(load(&tmp).id, session_id());
        let record = conversation_permission();
        let owner = PermissionOwner::Conversation(session_id());
        let approval = prepare_mutation(
            vec![initial],
            PermissionMutation::Create {
                destination: owner.clone(),
                records: Box::new([record.clone()]),
            },
        )
        .unwrap();
        let receipt = publication.commit(&approval).unwrap();
        let approved = publication.snapshot().unwrap();
        if deleted {
            publication
                .database
                .lock()
                .unwrap()
                .delete(session_id(), None)
                .unwrap();
        } else {
            let revoke = prepare_mutation(
                vec![approved.clone()],
                PermissionMutation::Revoke {
                    source: PermissionRecordIdentity {
                        owner,
                        record_id: record.id,
                    },
                },
            )
            .unwrap();
            publication.commit(&revoke).unwrap();
        }
        let retired = publication.snapshot().unwrap();
        assert_eq!(
            publication.receipt(approval.operation_id()).unwrap(),
            Some(receipt.clone())
        );
        assert_eq!(publication.commit(&approval).unwrap(), receipt);
        assert_eq!(publication.snapshot().unwrap(), retired);
        assert!(!retired.records.iter().any(PermissionRuleRecord::is_active));
        approved
            .apply_to_meta(session_id(), &mut store.session.meta)
            .unwrap();
        if deleted {
            assert!(store.save().is_err());
            assert!(!publication.snapshot().unwrap().revision.row_present);
        } else {
            store.save().unwrap();
            assert_eq!(
                store.session.meta.structured_permission_rules,
                retired.records
            );
            assert_eq!(
                store.session.meta.permission_generation,
                retired.revision.generation
            );
            assert_eq!(load(&tmp).meta.structured_permission_rules, retired.records);
        }
    }

    #[test_case(false; "other_conversation")]
    #[test_case(true; "other_database")]
    fn publication_rejects_foreign_permission_owners(other_database: bool) {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        let publication = store.permission_publication().unwrap();
        let other_tmp = TempDir::new().unwrap();
        let mut other = SessionStore::open_in(
            if other_database {
                StateDir::from_path(other_tmp.path().to_path_buf())
            } else {
                store.dir.clone()
            },
            if other_database {
                session_id()
            } else {
                CaudraId::generate()
            },
            CWD,
            MODEL_SPEC,
        )
        .unwrap();
        let other_publication = other.permission_publication().unwrap();
        let before = publication.snapshot().unwrap();
        let other_before = other_publication.snapshot().unwrap();
        let mutation = prepare_mutation(
            vec![other_before.clone()],
            PermissionMutation::Create {
                destination: other_before.revision.owner.clone(),
                records: Box::new([conversation_permission()]),
            },
        )
        .unwrap();
        assert!(publication.commit(&mutation).is_err());
        assert_eq!(publication.snapshot().unwrap(), before);
        assert_eq!(other_publication.snapshot().unwrap(), other_before);
        assert!(
            publication
                .receipt(mutation.operation_id())
                .unwrap()
                .is_none()
        );
    }

    #[test_case(false; "publisher")]
    #[test_case(true; "attached_manager")]
    fn publication_retains_the_session_lease(attached: bool) {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        let publication = store.permission_publication().unwrap();
        let permissions = permission_manager();
        if attached {
            permissions
                .attach_permission_publication(publication.clone())
                .unwrap();
        }
        drop(store);
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        assert!(matches!(
            SessionLease::acquire(&dir, session_id()),
            Err(SessionError::SessionInUse { .. })
        ));
        drop(publication);
        if attached {
            assert!(matches!(
                SessionLease::acquire(&dir, session_id()),
                Err(SessionError::SessionInUse { .. })
            ));
        }
        drop(permissions);
        assert!(SessionLease::acquire(&dir, session_id()).is_ok());
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

    #[test_case(true, false)]
    #[test_case(false, true)]
    fn session_store_resume_rejects_cross_authority(stored_remote: bool, expected_remote: bool) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let local = StoredWorkspaceBinding::local_from_cwd(CWD);
        let remote: StoredWorkspaceBinding = serde_json::from_str(
            &serde_json::to_string(&local)
                .unwrap()
                .replace("caudra:local:v1", "https://remote.example"),
        )
        .unwrap();
        let mut session = StoredSession::new_with_workspace(
            MODEL_SPEC,
            ".",
            if stored_remote { remote.clone() } else { local },
        );
        session.save(&dir).unwrap();
        let lease = Arc::new(SessionLease::acquire(&dir, session.id).unwrap());
        let result = SessionStore::open_in_with_lease(
            dir,
            session.id,
            CWD,
            MODEL_SPEC,
            lease,
            expected_remote.then_some(&remote),
        );
        assert!(matches!(
            result,
            Err(caudra_storage::sessions::SessionError::WorkspaceRebindRequired)
        ));
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
                workflow: None,
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
                workflow: None,
            })
            .unwrap();
        let mut nested_done = crate::ToolDoneEvent::error("nested-call".into(), "nested output");
        nested_done.is_error = false;
        store
            .record_event(&Envelope {
                event: AgentEvent::ToolDone(Box::new(nested_done)),
                subagent: None,
                run_id: 0,
                workflow: None,
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
    fn only_the_stored_remote_plan_marks_the_session_written() {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        let expected =
            caudra_workspace::PlanRef::new(format!("plan-{}", "a".repeat(32))).expect("plan ref");
        let other =
            caudra_workspace::PlanRef::new(format!("plan-{}", "b".repeat(32))).expect("plan ref");
        store.session.meta.plan_target = Some(StoredPlanTarget::PlanRef {
            reference: expected.clone(),
        });

        let record = |store: &mut SessionStore, reference: &caudra_workspace::PlanRef| {
            let mut done = crate::ToolDoneEvent::error("write".into(), "written");
            done.is_error = false;
            done.annotation = Some(format!(
                "local_document:plan:{};revision:{}",
                reference.as_str(),
                "c".repeat(64)
            ));
            store
                .record_event(&Envelope {
                    event: AgentEvent::ToolDone(Box::new(done)),
                    subagent: None,
                    run_id: 0,
                    workflow: None,
                })
                .unwrap();
        };

        record(&mut store, &other);
        assert!(!store.session.meta.plan_written);
        record(&mut store, &expected);
        assert!(store.session.meta.plan_written);
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
                    workflow: None,
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
                workflow: None,
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
                thinking: None,
                fast: false,
                outcome: StoredSubagentOutcome::Unknown,
            },
            StoredSubagent {
                tool_use_id: "generic-nested".into(),
                parent_tool_use_id: Some("generic-nested".into()),
                root_tool_use_id: Some("generic-root".into()),
                name: "nested".into(),
                model: None,
                thinking: None,
                fast: false,
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

    #[test_case(false; "acknowledged")]
    #[test_case(true; "lost_acknowledgment")]
    fn record_turn_checkpoints_restorable_permissions(lost_ack: bool) {
        smol::block_on(async {
            let tmp = TempDir::new().unwrap();
            let mut store = store_in(&tmp);
            let permissions = Arc::new(permission_manager());
            let publication = store.permission_publication().unwrap();
            let publisher: Arc<dyn PermissionPublication> = if lost_ack {
                Arc::new(LostAckPublication(Arc::clone(&publication)))
            } else {
                publication.clone()
            };
            permissions
                .attach_permission_publication(publisher)
                .unwrap();
            let pending = pending_permission(&permissions).await;
            assert!(permissions.answer(
                PERMISSION_REQUEST_ID,
                PermissionAnswer::AllowOption {
                    option_id: PERMISSION_OPTION.into(),
                    lifetime: PermissionLifetime::Conversation,
                },
            ));
            pending.await.unwrap();
            let approved = publication.snapshot().unwrap();
            assert_eq!(approved.records.len(), 1);
            assert_eq!(
                load(&tmp).meta.structured_permission_rules,
                approved.records
            );
            permissions.set_session_yolo(Some(true));
            store
                .record_turn(&History::default(), MODEL_SPEC.into(), &permissions)
                .unwrap();
            let loaded = load(&tmp);
            assert_eq!(loaded.meta.structured_permission_rules, approved.records);
            assert_eq!(
                store.session.meta.permission_generation,
                approved.revision.generation
            );
            assert_eq!(
                loaded.meta.permission_generation,
                approved.revision.generation
            );
            assert_eq!(loaded.meta.yolo, Some(true));
            let restored = permission_manager();
            restored
                .attach_permission_publication(publication.clone())
                .unwrap();
            restored.set_session_yolo(loaded.meta.yolo);
            assert_eq!(restored.conversation_permission_snapshot(), Some(approved));
            assert!(restored.is_yolo());
            assert_eq!(restored.persisted_yolo(), Some(true));
        });
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

    const WORKFLOW_NAME: &str = "echo";
    const WORKFLOW_SOURCE: &str = r#"
let meta = #{ name: "echo", description: "A test workflow" };
let first = agent("hello", #{ label: "worker" });
complete(#{ report: first.output });
"#;
    const PROJECT_WORKFLOWS: &str = ".caudra/workflows";
    const SCRIPT_EXTENSION: &str = "rhai";
    const ANSWER: &str = "the worker's findings";
    const PROMPT: &str = "what did the workflow find?";
    const SECOND_PROMPT: &str = "and now?";
    const COMPLETION_HEADING: &str = "Workflow echo (echo) finished with status completed.";
    const EVENTS_CLOSED: &str = "the session dropped its event channel";
    const NO_RUNTIME: &str = "the session must attach a workflow runtime";
    const AGENT_NEVER_STARTED: &str = "the workflow agent never reached the provider";
    const REPORTED_ONCE: &str = "a completion is reported in one prompt only";
    const UPDATED_MODEL_ID: &str = "updated-chat-model";

    /// Answers every request with `ANSWER`, or parks forever once built to
    /// hang. Each request's messages are kept, and `started` fires per
    /// request so a test can wait for an agent to be in flight.
    struct ScriptedProvider {
        hang: bool,
        started: flume::Sender<()>,
        requests: std::sync::Mutex<Vec<Vec<Message>>>,
        models: std::sync::Mutex<Vec<String>>,
        systems: std::sync::Mutex<Vec<String>>,
    }

    impl ScriptedProvider {
        /// The text of the last prompt the caller sent in every request, in
        /// order. Caudra's own injections trail that prompt and carry the same
        /// role, so they have to be skipped rather than mistaken for it.
        fn user_prompts(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|messages| {
                    messages
                        .iter()
                        .rev()
                        .find(|message| {
                            matches!(message.role, Role::User)
                                && !message.is_observation()
                                && !message.is_mention()
                                && message.display_text.as_deref() != Some("")
                        })
                        .and_then(Message::first_text_content)
                        .unwrap_or_default()
                        .to_owned()
                })
                .collect()
        }

        fn models(&self) -> Vec<String> {
            self.models.lock().unwrap().clone()
        }
    }

    impl Provider for ScriptedProvider {
        fn stream_message<'a>(
            &'a self,
            model: &'a Model,
            messages: &'a [Message],
            system: &'a str,
            _: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(messages.to_vec());
                self.models.lock().unwrap().push(model.id.clone());
                self.systems.lock().unwrap().push(system.to_owned());
                let _ = self.started.send(());
                if self.hang {
                    futures_lite::future::pending().await
                }
                Ok(StreamResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: ANSWER.into(),
                        }],
                        ..Default::default()
                    },
                    stop_reason: Some(StopReason::EndTurn),
                    ..Default::default()
                })
            })
        }

        fn list_models(
            &self,
        ) -> BoxFuture<'_, Result<Vec<caudra_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    /// An interactive session over a scripted provider, with one project
    /// workflow on disk and a runtime attached.
    struct WorkflowSession {
        _temp: TempDir,
        state_dir: StateDir,
        project: PathBuf,
        provider: Arc<ScriptedProvider>,
        started: flume::Receiver<()>,
    }

    impl WorkflowSession {
        fn new(hang: bool) -> Self {
            let temp = TempDir::new().unwrap();
            let project = temp.path().join("project");
            let workflows = project.join(PROJECT_WORKFLOWS);
            std::fs::create_dir_all(&workflows).unwrap();
            std::fs::write(
                workflows.join(format!("{WORKFLOW_NAME}.{SCRIPT_EXTENSION}")),
                WORKFLOW_SOURCE,
            )
            .unwrap();
            let (started_tx, started) = flume::unbounded();
            Self {
                state_dir: StateDir::from_path(temp.path().to_path_buf()),
                project: project.canonicalize().unwrap(),
                provider: Arc::new(ScriptedProvider {
                    hang,
                    started: started_tx,
                    requests: std::sync::Mutex::new(Vec::new()),
                    models: std::sync::Mutex::new(Vec::new()),
                    systems: std::sync::Mutex::new(Vec::new()),
                }),
                started,
                _temp: temp,
            }
        }

        async fn spawn(&self, workflows: bool) -> InteractiveHandle {
            self.spawn_in(workflows, None).await
        }

        async fn spawn_in(
            &self,
            workflows: bool,
            workspace: Option<WorkspaceSession>,
        ) -> InteractiveHandle {
            let binding = workspace.as_ref().map(|workspace| {
                StoredWorkspaceBinding::new_with_cursor(
                    workspace.binding().clone(),
                    workspace.cursor().clone(),
                    None,
                )
                .unwrap()
            });
            let cwd = if workspace.is_some() {
                PathBuf::from(".")
            } else {
                self.project.clone()
            };
            let context = match &workspace {
                Some(workspace) => Some(
                    crate::remote_project_context::load_remote_project_context(workspace)
                        .await
                        .unwrap(),
                ),
                None => None,
            };
            let lease = Arc::new(SessionLease::acquire(&self.state_dir, session_id()).unwrap());
            let store = SessionStore::open_in_with_lease(
                self.state_dir.clone(),
                session_id(),
                &cwd.to_string_lossy(),
                MODEL_SPEC,
                Arc::clone(&lease),
                binding.as_ref(),
            )
            .unwrap();
            let params = InteractiveParams {
                model: Model::from_spec(MODEL_SPEC).unwrap(),
                config: AgentConfig {
                    generate_titles: false,
                    ..AgentConfig::default()
                },
                permissions_config: PermissionsConfig::default(),
                timeouts: Timeouts::default(),
                prompt_slots: Arc::new(ResolvedSlots::default()),
                thinking: crate::ThinkingConfig::default(),
                system_prompt_profile: None,
                system_prompt_profile_name: None,
                prompt_profiles: Arc::new(PromptProfileCatalog::default()),
                excluded_tools: Vec::new(),
                mcp_handle: None,
                initial_wd: cwd.clone(),
                session_id: SessionRef::from(session_id()),
                session_lease: lease,
                expected_write_version: None,
                initial_history: Vec::new(),
                yolo: true,
                structured_permission_rules: store.session.meta.structured_permission_rules.clone(),
                session_yolo: store.session.meta.yolo,
                system_prompt_override: None,
                append_system_prompt: None,
                model_policy: Arc::new(ModelPolicy::default()),
                plugin_rules: Arc::default(),
                local_tools: LocalTools::default(),
                workflow_mode: workflows.then(|| Arc::new(|| AgentMode::Build) as ModeResolver),
                workspace_binding: binding,
                remote_environment: workspace.as_ref().map(|_| RemoteEnvironment {
                    cwd: cwd.to_string_lossy().into_owned(),
                    platform: "remote".into(),
                }),
                workspace_session: workspace,
                remote_project_context: context,
                host_cwd: None,
                local_documents: None,
            };
            spawn_prepared_interactive(PreparedInteractive {
                params,
                history: History::default(),
                model: Model::from_spec(MODEL_SPEC).unwrap(),
                provider: Arc::clone(&self.provider) as Arc<dyn Provider>,
                store,
                workspace_baseline: None,
            })
            .await
            .unwrap()
        }
    }

    async fn shutdown_interactive(handle: InteractiveHandle) {
        let InteractiveHandle { input_tx, task, .. } = handle;
        drop(input_tx);
        task.await;
    }

    #[test_case(false, PermissionLifetime::Once; "acp_once")]
    #[test_case(false, PermissionLifetime::Conversation; "acp_conversation")]
    #[test_case(true, PermissionLifetime::Conversation; "headless_conversation")]
    fn interactive_permission_approval_and_revocation_survive_restart(
        workflows: bool,
        lifetime: PermissionLifetime,
    ) {
        smol::block_on(async {
            let session = WorkflowSession::new(false);
            let handle = session.spawn(workflows).await;
            handle.permissions.set_session_yolo(Some(false));
            let initial = handle
                .permissions
                .conversation_permission_snapshot()
                .unwrap();
            assert!(initial.revision.row_present);
            let database = SessionDatabase::open_read_only(&session.state_dir).unwrap();
            let version = database.write_version(session_id()).unwrap();
            let pending = pending_permission(&handle.permissions).await;
            let reusable = lifetime == PermissionLifetime::Conversation;
            assert!(handle.permissions.answer(
                PERMISSION_REQUEST_ID,
                if reusable {
                    PermissionAnswer::AllowSession
                } else {
                    PermissionAnswer::AllowOnce
                },
            ));
            pending.await.unwrap();
            let approved = handle
                .permissions
                .conversation_permission_snapshot()
                .unwrap();
            assert_eq!(approved.records.len(), usize::from(reusable));
            assert_eq!(database.write_version(session_id()).unwrap(), version);
            assert_eq!(
                database
                    .permission_snapshot(initial.revision.owner)
                    .unwrap(),
                approved,
            );
            let (events, _rx) = flume::unbounded();
            let events = EventSender::new(events, 0);
            assert_eq!(
                enforce_test_permission(&handle.permissions, None, &events)
                    .await
                    .is_ok(),
                reusable,
            );
            shutdown_interactive(handle).await;
            let stored: StoredSession = database.load(session_id()).unwrap();
            assert_eq!(stored.meta.structured_permission_rules, approved.records);
            assert_eq!(
                stored.meta.permission_generation,
                approved.revision.generation
            );

            let handle = session.spawn(workflows).await;
            assert!(!handle.permissions.is_yolo());
            assert_eq!(
                handle.permissions.conversation_permission_snapshot(),
                Some(approved.clone())
            );
            assert_eq!(
                enforce_test_permission(&handle.permissions, None, &events)
                    .await
                    .is_ok(),
                reusable,
            );
            if let Some(record) = approved.records.first() {
                assert_eq!(
                    handle
                        .permissions
                        .revoke_structured_rule(&record.id)
                        .unwrap(),
                    Some(RevokedRuleScope::Conversation),
                );
            }
            let revoked = handle
                .permissions
                .conversation_permission_snapshot()
                .unwrap();
            assert!(!revoked.records.iter().any(PermissionRuleRecord::is_active));
            let stored: StoredSession = database.load(session_id()).unwrap();
            assert_eq!(stored.meta.structured_permission_rules, revoked.records);
            handle.input_tx.send_async(prompt(PROMPT)).await.unwrap();
            wait_for_turn(&handle.event_rx).await;
            shutdown_interactive(handle).await;
            let stored: StoredSession = database.load(session_id()).unwrap();
            assert_eq!(stored.meta.structured_permission_rules, revoked.records);
            assert_eq!(
                stored.meta.permission_generation,
                revoked.revision.generation
            );

            let handle = session.spawn(workflows).await;
            assert_eq!(
                handle.permissions.conversation_permission_snapshot(),
                Some(revoked)
            );
            assert!(
                enforce_test_permission(&handle.permissions, None, &events)
                    .await
                    .is_err()
            );
            shutdown_interactive(handle).await;
        });
    }

    /// Trusts the project script by the digest the catalog reports, as an SDK
    /// client would, and starts it.
    async fn trust_and_start(workflow: &WorkflowHandle) -> RunSnapshot {
        let Ok(WorkflowResponse::Catalog(catalog)) = workflow.request(WorkflowRequest::List).await
        else {
            panic!("expected the catalog");
        };
        let digest = catalog
            .entries
            .iter()
            .find(|entry| entry.name == WORKFLOW_NAME)
            .expect("the project workflow is listed")
            .digest
            .clone();
        workflow
            .request(WorkflowRequest::Trust {
                name: WORKFLOW_NAME.into(),
                digest,
            })
            .await
            .unwrap();
        match workflow
            .request(WorkflowRequest::Start(caudra_workflow::LaunchRequest {
                name: WORKFLOW_NAME.into(),
                args: serde_json::json!({}),
                agent_budget: None,
            }))
            .await
        {
            Ok(WorkflowResponse::Started(run)) => *run,
            other => panic!("expected a started run, got {other:?}"),
        }
    }

    async fn wait_for_run(events: &Receiver<Envelope>, run_id: &str, status: RunStatus) {
        loop {
            let envelope = events.recv_async().await.expect(EVENTS_CLOSED);
            if let AgentEvent::Workflow(event) = &envelope.event
                && let WorkflowEvent::Snapshot(snapshot) = event.as_ref()
                && snapshot.run_id == run_id
                && snapshot.status == status
            {
                return;
            }
        }
    }

    async fn wait_for_turn(events: &Receiver<Envelope>) {
        loop {
            let envelope = events.recv_async().await.expect(EVENTS_CLOSED);
            match &envelope.event {
                AgentEvent::Done { .. } if envelope.subagent.is_none() => return,
                AgentEvent::Error { message } => panic!("turn failed: {message}"),
                _ => {}
            }
        }
    }

    fn prompt(message: &str) -> AgentInput {
        AgentInput {
            message: message.into(),
            mode: AgentMode::Build,
            images: Vec::new(),
            mentions: Vec::new(),
            commits: Vec::new(),
            preamble: Vec::new(),
            thinking: crate::ThinkingConfig::default(),
            fast: false,
            prompt: None,
            resume: false,
        }
    }

    #[test]
    fn workflow_model_route_updates_without_another_main_prompt() {
        smol::block_on(async {
            let session = WorkflowSession::new(false);
            let handle = session.spawn(true).await;
            let workflow = handle.workflow.clone().expect(NO_RUNTIME);
            let route = handle.model_route.as_ref().expect(NO_RUNTIME);
            let mut model = Model::from_spec(MODEL_SPEC).unwrap();
            model.id = UPDATED_MODEL_ID.into();
            route.install(Arc::clone(&session.provider) as Arc<dyn Provider>, model);

            let run = trust_and_start(&workflow).await;
            session
                .started
                .recv_async()
                .await
                .expect(AGENT_NEVER_STARTED);
            wait_for_run(&handle.event_rx, &run.run_id, RunStatus::Completed).await;

            assert_eq!(session.provider.models(), vec![UPDATED_MODEL_ID]);
            let InteractiveHandle { input_tx, task, .. } = handle;
            drop(input_tx);
            task.await;
        });
    }

    #[test]
    fn interactive_policy_is_installed_before_dispatch_removed_and_failed_closed_on_refresh() {
        smol::block_on(async {
            let session = WorkflowSession::new(false);
            let (workspace, service) =
                crate::remote_project_context::tests::AssetService::permission_fixture();
            let handle = session.spawn_in(false, Some(workspace)).await;
            let (events, _) = flume::unbounded();
            let events = EventSender::new(events, 0);
            let check = || async {
                handle
                    .permissions
                    .enforce(
                        &ToolKey::native("file_read"),
                        &crate::tools::PermissionScopes::single("opaque-file".into()),
                        &serde_json::json!({}),
                        &events,
                        None,
                        "initial-deny",
                        &CancelToken::none(),
                        None,
                    )
                    .await
            };
            assert!(check().await.is_err());
            assert!(session.provider.models().is_empty());
            service.remove_permissions();
            handle.input_tx.send(prompt(PROMPT)).unwrap();
            wait_for_turn(&handle.event_rx).await;
            assert!(check().await.is_ok());
            let requests = session.provider.models().len();
            service.corrupt_permissions();
            handle.input_tx.send(prompt(PROMPT)).unwrap();
            loop {
                if let AgentEvent::Error { .. } = handle.event_rx.recv_async().await.unwrap().event
                {
                    break;
                }
            }
            assert_eq!(session.provider.models().len(), requests);
            assert!(check().await.is_err());
            service.remove_permissions();
            handle.input_tx.send(prompt(PROMPT)).unwrap();
            wait_for_turn(&handle.event_rx).await;
            assert!(check().await.is_ok());
            let InteractiveHandle { input_tx, task, .. } = handle;
            drop(input_tx);
            task.await;
        });
    }

    #[test]
    fn remote_cd_rebuilds_subsequent_workflow_agents_and_persists_logical_cursor() {
        smol::block_on(async {
            let session = WorkflowSession::new(false);
            let (workspace, service) =
                crate::stored_session::tests::remote_workspace("workflow-cd", WORKFLOW_SOURCE);
            let handle = session.spawn_in(true, Some(workspace)).await;
            let workflow = handle.workflow.clone().unwrap();
            let transition = workflow.suspend().await.unwrap();
            assert!(workflow.suspend().await.is_err());
            assert!(
                workflow
                    .request(WorkflowRequest::Start(caudra_workflow::LaunchRequest {
                        name: WORKFLOW_NAME.into(),
                        args: serde_json::json!({}),
                        agent_budget: None,
                    }))
                    .await
                    .is_err()
            );
            drop(transition);
            assert_eq!(
                handle.change_remote_directory("nested").await.unwrap(),
                "nested"
            );
            let run = trust_and_start(&workflow).await;
            wait_for_run(&handle.event_rx, &run.run_id, RunStatus::Completed).await;
            // The directory reaches an agent as an announcement now, not as
            // part of the prompt it caches.
            let announced = format!("{:?}", session.provider.requests.lock().unwrap());
            assert!(announced.contains("nested"), "{announced}");
            let loaded = crate::load_stored_session(session_id(), &session.state_dir).unwrap();
            assert_eq!(loaded.cwd, "nested");
            assert_eq!(
                loaded.workspace_binding().unwrap().cursor(),
                Some(handle.remote_workspace_session().unwrap().cursor())
            );
            service
                .revision
                .store(2, std::sync::atomic::Ordering::SeqCst);
            handle.input_tx.send(prompt(PROMPT)).unwrap();
            wait_for_turn(&handle.event_rx).await;
            let run = trust_and_start(&workflow).await;
            wait_for_run(&handle.event_rx, &run.run_id, RunStatus::Completed).await;
            assert_eq!(handle.remote_cwd().as_deref(), Some("nested"));
            assert!(
                session
                    .provider
                    .requests
                    .lock()
                    .unwrap()
                    .last()
                    .unwrap()
                    .iter()
                    .filter_map(Message::first_text_content)
                    .any(|text| text.contains("nested"))
            );
            let InteractiveHandle { input_tx, task, .. } = handle;
            drop(input_tx);
            task.await;
        });
    }

    #[test]
    fn remote_cd_persistence_conflict_keeps_the_previous_cursor() {
        const PERSISTENCE_ERROR: &str = "cd: remote workspace cursor could not be persisted";
        smol::block_on(async {
            let session = WorkflowSession::new(false);
            let (workspace, _) =
                crate::stored_session::tests::remote_workspace("cd-persistence", "");
            let handle = session.spawn_in(false, Some(workspace)).await;
            let before = handle.remote_workspace_session().unwrap();
            let mut competing =
                crate::load_stored_session(session_id(), &session.state_dir).unwrap();
            competing.set_title("competing writer".into());
            competing.save(&session.state_dir).unwrap();

            assert_eq!(
                handle.change_remote_directory("nested").await.unwrap_err(),
                PERSISTENCE_ERROR
            );
            assert_eq!(handle.remote_cwd().as_deref(), Some("."));
            assert_eq!(
                handle.remote_workspace_session().unwrap().cursor(),
                before.cursor()
            );
            let stored = crate::load_stored_session(session_id(), &session.state_dir).unwrap();
            assert_eq!(stored.cwd, ".");
            assert!(session.provider.models().is_empty());
            let InteractiveHandle { input_tx, task, .. } = handle;
            drop(input_tx);
            task.await;
        });
    }

    #[test]
    fn active_remote_workflow_blocks_cwd_and_authoritative_revision_changes() {
        smol::block_on(async {
            let session = WorkflowSession::new(true);
            let (workspace, service) =
                crate::stored_session::tests::remote_workspace("workflow-busy", WORKFLOW_SOURCE);
            let handle = session.spawn_in(true, Some(workspace)).await;
            let workflow = handle.workflow.clone().unwrap();
            trust_and_start(&workflow).await;
            session.started.recv_async().await.unwrap();
            assert!(handle.change_remote_directory("nested").await.is_err());
            assert_eq!(handle.remote_cwd().as_deref(), Some("."));
            service
                .revision
                .store(2, std::sync::atomic::Ordering::SeqCst);
            handle.input_tx.send(prompt(PROMPT)).unwrap();
            loop {
                let envelope = handle.event_rx.recv_async().await.unwrap();
                if let AgentEvent::Error { message } = envelope.event {
                    assert!(message.contains("quiescent"), "{message}");
                    break;
                }
            }
            assert_eq!(session.provider.models().len(), 1);
            let InteractiveHandle { input_tx, task, .. } = handle;
            drop(input_tx);
            task.await;
        });
    }

    #[test]
    fn a_finished_run_is_reported_in_the_next_prompt_once_and_acknowledged() {
        smol::block_on(async {
            let session = WorkflowSession::new(false);
            let handle = session.spawn(true).await;
            let workflow = handle.workflow.clone().expect(NO_RUNTIME);
            let run = trust_and_start(&workflow).await;
            wait_for_run(&handle.event_rx, &run.run_id, RunStatus::Completed).await;
            assert_eq!(workflow.pending_completions(), 1);

            handle.input_tx.send(prompt(PROMPT)).unwrap();
            wait_for_turn(&handle.event_rx).await;
            handle.input_tx.send(prompt(SECOND_PROMPT)).unwrap();
            wait_for_turn(&handle.event_rx).await;

            let prompts = session.provider.user_prompts();
            let reported =
                format!("{COMPLETION_HEADING}\nReport: {ANSWER}{COMPLETION_SEPARATOR}{PROMPT}");
            assert!(prompts.contains(&reported), "got {prompts:?}");
            assert!(
                prompts.contains(&SECOND_PROMPT.to_owned()),
                "{REPORTED_ONCE}: got {prompts:?}"
            );
            assert_eq!(workflow.pending_completions(), 0);
            assert!(!workflow.state().runs[0].outbox_pending);

            let InteractiveHandle { input_tx, task, .. } = handle;
            drop(input_tx);
            task.await;
            assert_eq!(
                workflow.request(WorkflowRequest::List).await,
                Err(WorkflowError::Unavailable)
            );
        });
    }

    #[test]
    fn client_eof_interrupts_an_active_run_before_the_session_closes() {
        smol::block_on(async {
            let session = WorkflowSession::new(true);
            let handle = session.spawn(true).await;
            let workflow = handle.workflow.clone().expect(NO_RUNTIME);
            let run = trust_and_start(&workflow).await;
            session
                .started
                .recv_async()
                .await
                .expect(AGENT_NEVER_STARTED);
            assert_eq!(workflow.active_count(), 1);

            let InteractiveHandle { input_tx, task, .. } = handle;
            drop(input_tx);
            task.await;

            let store = WorkflowStore::spawn(session.state_dir.clone(), session_id()).unwrap();
            let row = store.load_run(run.run_id).await.unwrap().unwrap();
            store.shutdown().await;
            assert_eq!(row.status, WorkflowRunStatus::Interrupted);
        });
    }

    #[test]
    fn a_session_without_workflow_mode_attaches_no_runtime() {
        smol::block_on(async {
            let session = WorkflowSession::new(false);
            let handle = session.spawn(false).await;
            assert!(handle.workflow.is_none());
            let InteractiveHandle { input_tx, task, .. } = handle;
            drop(input_tx);
            task.await;
        });
    }

    fn completed_run(result: Option<Value>) -> RunSnapshot {
        RunSnapshot {
            run_id: "run-1".into(),
            display_name: "echo-2".into(),
            workflow_name: WORKFLOW_NAME.into(),
            source_kind: caudra_workflow::SourceKind::Project,
            source_path: None,
            objective: None,
            status: RunStatus::Completed,
            pause_kind: None,
            pause_message: None,
            revision: 1,
            execution_epoch: 0,
            phase: None,
            phases: Vec::new(),
            phase_history: Vec::new(),
            agent_budget: 1,
            usage: caudra_workflow::RunUsage::default(),
            roster: Vec::new(),
            result,
            error: None,
            logs: Vec::new(),
            outbox_pending: true,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn a_completion_block_names_the_run_and_carries_its_report_and_scratch_path() {
        let run = completed_run(Some(serde_json::json!({
            "report": ANSWER,
            "path": "/tmp/scratch/notes.md",
        })));

        assert_eq!(
            completion_block(&run),
            format!(
                "Workflow echo-2 (echo) finished with status completed.\nReport: {ANSWER}\nScratch file: /tmp/scratch/notes.md"
            )
        );
    }

    #[test]
    fn a_completion_block_bounds_a_result_without_a_report() {
        let run = completed_run(Some(Value::String("x".repeat(COMPLETION_TEXT_LIMIT + 1))));

        let block = completion_block(&run);

        assert!(block.ends_with(TRUNCATED_SUFFIX));
        assert!(block.len() < COMPLETION_TEXT_LIMIT + COMPLETION_HEADING.len() + 64);
    }

    #[test]
    fn a_completion_block_reports_a_pause_and_an_error() {
        let run = RunSnapshot {
            status: RunStatus::Paused,
            pause_message: Some("check the draft".into()),
            error: Some("boom".into()),
            ..completed_run(None)
        };

        assert_eq!(
            completion_block(&run),
            "Workflow echo-2 (echo) finished with status paused.\nPaused: check the draft\nError: boom"
        );
    }
}
