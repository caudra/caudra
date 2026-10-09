use std::borrow::Cow;
use std::collections::HashSet;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::decisions::Decisions;
use arc_swap::{ArcSwap, ArcSwapOption};
use async_lock::Mutex;
use caudra_automation::event::{GoalView, SessionStatus, SessionView, StartedBy, WorkView};
use caudra_automation::request::{AutomationError, OutboxClaim, SessionSignal};
use caudra_automation::snapshot::{AutomationEvent, PauseSource, SettleBlocker};
use caudra_automation::untrusted::Untrusted;
#[cfg(test)]
use caudra_config::ToolKey;
use caudra_config::decisions::DecisionsConfig;
use caudra_config::{AutomationsConfig, Feature, FeatureFlags, ModelPolicy, SnapshotsConfig};
use caudra_providers::Timeouts;
use caudra_providers::model::{Billing, Model, ModelPurpose};
use caudra_providers::pricing::settle_session;
use caudra_providers::provider::{self, Provider};
use caudra_providers::{
    AutomationEventOrigin, CacheKey, HistoryItem, HistoryItemKind, Message, TokenUsage,
    WorkflowEventOrigin, active_history_items, merge_history_items, resolve_history_head,
    transcript_history_items,
};
#[cfg(test)]
use caudra_providers::{ContentBlock, Role};
use caudra_storage::id::{CaudraId, SessionRef};
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_storage::permission_state::PermissionRuleRecord;
use caudra_storage::permission_state::mutation::{
    PermissionCommitReceipt, PermissionOwner, PermissionSnapshot, PreparedPermissionMutation,
};
use caudra_storage::sessions::{
    PermissionMode, SessionCursor, SessionDatabase, SessionError, SessionLease, StoredMode,
    StoredPlanTarget, StoredSubagent, StoredSubagentOutcome, add_cost,
};
use caudra_storage::tool_outputs::ToolOutputStore;
use caudra_storage::usage_ledger::LedgerPurpose;
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_storage::{StateDir, StorageError};
use caudra_workflow::{
    RunSnapshot, RunStatus, WorkflowError, WorkflowEvent, WorkflowRequest, WorkflowResponse,
};
use caudra_workspace::{
    CommandText, DirectoryNavigation, ExecRequest, LocalDocumentRef, OperationProgressKind,
    OperationState, OperationStatus, RecordHolder, UNREVEALED_ROOT, WorkspaceCursor,
    WorkspaceError, WorkspaceSession,
};
use event_listener::Event;
use flume::Receiver;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, error, info, warn};

use crate::agent::change_recording::{ChangeRecorder, ChangeSource, cover_records};
use crate::agent::task_runner::{
    HostExtras, ModeResolver, ModelResolver, SubagentTaskRunner, WorkflowHostContext,
};
use crate::agent::{self, History};
use crate::automation::catalog::Frontend;
use crate::automation::clock::Clock;
use crate::automation::frontend::{
    SessionSignals, claim_next, delivery_due, goal_finished, launch_armings, mode_name, run_end,
    saved_controls, set_claimed_goal, stop_runtime, wake_origin,
};
use crate::automation::handle::AutomationHandle;
use crate::automation::http::HttpClient;
use crate::automation::manager::{AutomationRuntime, RuntimeDeps as AutomationDeps};
use crate::automation::outbox::claim_message;
use crate::automation::workflows::Workflows;
use crate::background::{BackgroundTasks, BackgroundTransition, SessionWork};
use crate::cancel::{CancelMap, CancelToken, CancelTrigger};
use crate::commits;
use crate::memory::baseline::{MemoryBaseline, open_store as open_memory_store};
use crate::memory::compactor::{Ledger, Pump};
use crate::mentions;
use crate::peers::{PeerDescriptor, PeerHost};
use crate::permissions::editor::{PermissionEditError, PermissionPublication};
use crate::permissions::{PermissionManager, PluginRuleStore};
use crate::prompt::ResolvedSlots;
use crate::prompt::profile::{BUILTIN_PROFILE_NAME, PromptProfileCatalog, SystemPromptProfile};
use crate::template;
use crate::tools::native::plan::PlanTarget;
use crate::tools::{
    BuiltinDeferral, DeferralSession, DeferredTool, DescriptionContext, FileReadTracker,
    LocalTools, PathLocks, ToolAudience, ToolDefinitions, ToolFilter, ToolRegistry, deferral,
};
use crate::types::{BACKGROUND_EVENT_RUN_ID, MEMORY_EVENT_RUN_ID, TodoItem};
use crate::workflow::{RuntimeDeps, WorkflowHandle, WorkflowRuntime, WorkspaceRebind};
use crate::{
    Agent, AgentConfig, AgentError, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams,
    DoneReason, Envelope, EventSender, ExtractedCommand, GoalHandle, ImageSource, InterruptSource,
    McpHandle, McpSession, PermissionsConfig, SessionMailbox, StoredSession,
    SubagentHistorySnapshot, SubagentHistoryStore, ThinkingConfig, ToolOutput, ToolOutputLines,
    open_stored_session,
};

/// Bytes of a run's report or result carried into the next prompt.
const COMPLETION_TEXT_LIMIT: usize = 8 * 1024;
const TRUNCATED_SUFFIX: &str = "…[truncated]";
const REMOTE_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
const REMOTE_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(50);
const REMOTE_COMMAND_PROGRESS_GAP: &str = "[remote progress gap]";
const REMOTE_COMMAND_CANCELLED: &str = "Remote command cancelled";
const REMOTE_COMMAND_FAILED: &str = "Remote command failed";
const REMOTE_COMMAND_INDETERMINATE: &str =
    "Remote command outcome is indeterminate; it will not be retried";
const REMOTE_COMMAND_TRUNCATED: &str = "[truncated]";
const SESSION_DATABASE_UNAVAILABLE: &str = "session database unavailable";
const DECISIONS_STARTUP_FAILED: &str = "Decision engine initialization failed; check decisions configuration, credentials and question overrides";
const STALE_WORKFLOW_CONTROL: &str =
    "Workflow control was superseded by a session stop or mode change";
const PERSIST_FAILED: &str = "Failed to persist session";
/// Automation events the host has not taken yet; past this the oldest is dropped, as the
/// runtime drops its own.
const AUTOMATION_EVENT_CAPACITY: usize = 1024;
const MILLIS_PER_SECOND: i64 = 1_000;

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
    /// Restored from the session and written back on every save.
    goal: GoalHandle,
    /// The runtime serving the session, whose controls every save keeps; `None` leaves the
    /// stored controls as they are.
    automations: Option<AutomationHandle>,
}

impl SessionStore {
    /// The todo list the selected transcript last committed, read back across
    /// its compaction seams; see [`agent::stored_todos`].
    fn todos(&self) -> Option<Vec<TodoItem>> {
        let messages = self.session.messages();
        let head = resolve_history_head(
            messages,
            self.session.meta.history_head,
            self.session.meta.pending_revert.is_some(),
        );
        let transcript = transcript_history_items(messages, head)
            .inspect_err(|error| warn!(%error, "failed to read the transcript for its todo list"))
            .ok()?;
        agent::stored_todos(transcript.iter(), |call_id| {
            self.session.tool_outputs().get(call_id).map(Arc::as_ref)
        })
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
            crate::active_task_history_versions_with_outputs(&active_history, |call_id| {
                session.tool_outputs().get(call_id).map(Arc::as_ref)
            });
        reachable.extend(versions.keys().cloned());
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
                let items = session.subagent_messages().get(version_id)?;
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
        let subagent_history =
            SubagentHistoryStore::seeded_with_versions(subagent_messages, specs, versions);
        let persisted_subagent_history = subagent_history.snapshot();
        Self {
            dir,
            lease,
            database: database.map(|database| Arc::new(StdMutex::new(database))),
            cursor,
            goal: GoalHandle::from_meta(&session.meta),
            session,
            created,
            subagent_history,
            persisted_subagent_history,
            automations: None,
        }
    }

    fn save(&mut self) -> Result<(), SessionError> {
        self.session.updated_at = caudra_storage::now_epoch();
        let goal = self.goal.stored();
        let meta = &mut self.session.meta;
        meta.active_goal = goal.active;
        meta.goal_result = goal.result;
        meta.goal_continuation_limit = goal.continuation_limit;
        if let Some(automations) = &self.automations {
            meta.automations = Some(saved_controls(automations));
        }
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
        self.session.meta.permission_mode = permissions.persisted_mode();
    }

    fn set_system_prompt_profile(&mut self, name: Option<&str>) {
        if let Some(name) = name {
            self.session.meta.system_prompt_profile = Some(name.to_owned());
        }
    }

    fn set_mode(&mut self, mode: &AgentMode) {
        self.session.meta.mode = match mode {
            AgentMode::Build => Some(StoredMode::Build),
            AgentMode::Plan(_) | AgentMode::RemotePlan(_) => Some(StoredMode::Plan),
            AgentMode::ReadOnly => return,
        };
    }

    fn bind_plan(&mut self, plan: PlanTarget) {
        let meta = &mut self.session.meta;
        match plan {
            PlanTarget::Local(path) => {
                let path = path.to_string_lossy().into_owned();
                meta.plan_path = Some(path.clone());
                meta.plan_target = Some(StoredPlanTarget::LocalPath { path });
            }
            PlanTarget::Remote(reference) => {
                meta.plan_path = None;
                meta.plan_target = Some(StoredPlanTarget::PlanRef { reference });
            }
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
            return Err(SessionError::CorruptDatabaseValue {
                field: "history",
                reason: error.to_string(),
            });
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
    pub decisions_config: DecisionsConfig,
    pub seed_permission_mode: Option<PermissionMode>,
    pub snapshots: SnapshotsConfig,
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

/// What a headless run records through. A remote run records on the host for
/// its session; a local one records nothing, since no headless mode offers a
/// file revert.
fn remote_recorder(
    remote: bool,
    session_id: CaudraId,
    config: &SnapshotsConfig,
) -> Option<ChangeRecorder> {
    if !remote {
        return None;
    }
    let holder = RecordHolder::new(session_id.to_string())
        .inspect_err(|error| warn!(%session_id, %error, "session id is not a record holder"))
        .ok()?;
    ChangeRecorder::new(
        ChangeSource::Remote,
        holder,
        PathBuf::from(UNREVEALED_ROOT),
        config,
    )
}

/// [`remote_recorder`] for a stored session, whose record coverage follows
/// where it records.
fn session_recorder(
    session: &mut StoredSession,
    remote: bool,
    config: &SnapshotsConfig,
) -> Option<ChangeRecorder> {
    let recorder = remote_recorder(remote, session.id, config);
    let store = recorder
        .as_ref()
        .and(session.workspace_binding())
        .map(StoredWorkspaceBinding::change_store_key);
    cover_records(session, store);
    recorder
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
    command: &str,
    cancel: &CancelToken,
    max_output_lines: usize,
    max_output_bytes: usize,
    mut progress: impl FnMut(&str),
) -> RemoteCommandOutput {
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
    profile: Option<&'a SystemPromptProfile>,
    session_plan: bool,
    prompt_profiles: &'a PromptProfileCatalog,
    chat_model: &'a Model,
    thinking: &'a crate::ThinkingConfig,
    model_policy: &'a ModelPolicy,
    timeouts: Timeouts,
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

    let tool_filter = definitions.available_filter();
    AgentSetup {
        vars,
        instructions,
        tools: definitions.declared,
        deferred: definitions.deferred,
        tool_filter,
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
    let filter = ToolFilter::from_config(config, model, excluded_tools);
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
    registry.definitions_split_with_policy(
        &vars,
        &ctx,
        model.supports_tool_examples(),
        &deferral::deferred_names(
            &config.allowed_tools,
            BuiltinDeferral::resolve(config, model),
        ),
        &task
            .profile
            .map(|profile| profile.tools().clone())
            .unwrap_or_default(),
        task.session_plan,
    )
}

/// Names advertised to SDK clients: what the first request would actually
/// carry, so a deferred built-in shows up as `tool_search` rather than as
/// itself.
fn advertised_tool_names(
    tools: &Value,
    deferred: &[DeferredTool],
    mcp: Option<&McpSession>,
    local_search_binding: bool,
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
    crate::tools::deferral::push_unbound_catalog(
        &mut probe,
        &sections,
        local_search_binding || ToolRegistry::global().has(crate::tools::TOOL_SEARCH_TOOL_NAME),
    );
    extract_tool_names(&probe)
}

pub fn spawn(mut params: HeadlessParams) -> Result<HeadlessHandle, InteractiveStartError> {
    let profile_tool_policy = Arc::new(
        params
            .system_prompt_profile
            .as_ref()
            .map(|profile| profile.tools().clone())
            .unwrap_or_default(),
    );
    profile_tool_policy
        .validate_bindings(
            ToolRegistry::global()
                .iter()
                .iter()
                .map(|entry| entry.name()),
        )
        .map_err(InteractiveStartError)?;
    let tool_ceiling = ToolFilter::ceiling_from_config(&params.config, &params.excluded_tools);
    let peer_host = if params.workspace_session.is_some() || params.host_cwd.is_some() {
        None
    } else {
        match PeerHost::start(params.config.features, &params.config.messaging) {
            Ok(host) => host,
            Err(error) => {
                warn!(%error, "cross-session messaging unavailable");
                None
            }
        }
    };
    if peer_host.is_none() {
        params
            .excluded_tools
            .extend_from_slice(crate::tools::native::peers::TOOL_NAMES);
    }
    let state_dir =
        StateDir::resolve().map_err(|error| InteractiveStartError(error.to_string()))?;
    let decisions = initialize_decisions(
        params.decisions_config.clone(),
        &state_dir,
        params.config.features,
    )?;
    let provider_model = params.model.clone();
    if let Err(error) = provider::adjust_model(&mut params.model, params.timeouts) {
        warn!(%error, "failed to adjust headless model before setup");
    }
    let working_dir = params.initial_wd.to_string_lossy().into_owned();
    let mode = AgentMode::Build;
    let AgentSetup {
        vars,
        instructions,
        mut tools,
        mut deferred,
        tool_filter,
    } = setup(
        &params.model,
        &params.config,
        &params.excluded_tools,
        false,
        TaskDescriptionContext {
            profile: params.system_prompt_profile.as_deref(),
            session_plan: false,
            prompt_profiles: &params.prompt_profiles,
            chat_model: &params.model,
            thinking: &params.thinking,
            model_policy: &params.model_policy,
            timeouts: params.timeouts,
            remote_project_context: params.remote_project_context.as_ref(),
            host_cwd: params.host_cwd.as_deref(),
        },
        params.remote_environment.as_ref(),
    );

    crate::tools::execution::configure_tools(
        &mut tools,
        &mut deferred,
        &params.config,
        false,
        false,
    );
    params.prompt_slots = crate::tools::execution::execution_slots(
        &params.prompt_slots,
        &params.config,
        false,
        false,
        &tools,
        &deferred,
    );
    let memory = MemoryBaseline::adopt(
        open_memory_store(
            params.local_documents.as_ref(),
            Some(&state_dir),
            &params.initial_wd,
        ),
        None,
    );
    let system = agent::build_system_prompt(
        &instructions.text,
        &params.prompt_slots,
        &tool_filter,
        params.system_prompt_profile.as_deref(),
        memory.view(),
    );

    let mcp = params.mcp_handle.clone().map(|h| {
        McpSession::new(h, &[])
            .with_disabled_tools(&params.config.disabled_tools)
            .with_actor_policy(Arc::clone(&profile_tool_policy))
    });
    let tool_names = advertised_tool_names(&tools, &deferred, mcp.as_ref(), false);

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
            if let Some(mode) = params.seed_permission_mode {
                permissions.set_seed_mode(mode);
            }
            permissions.set_decisions(decisions.map(|service| service.for_session(session_id)));
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
            let changes = remote_recorder(
                params.workspace_session.is_some(),
                session_id,
                &params.snapshots,
            );
            let peer_session = peer_host.as_ref().and_then(|host| {
                match host.register_with_controls(
                    PeerDescriptor {
                        session_id,
                        name: "Print session".into(),
                        cwd: params.initial_wd.clone(),
                        mode: mode.clone(),
                        permission_mode: permissions.mode(),
                        inbound: params.config.messaging.inbound.clone(),
                        blocked: false,
                        busy: true,
                    },
                    &params.config.messaging,
                    None,
                ) {
                    Ok(session) => {
                        if let Err(error) = session.claim_handle() {
                            warn!(%error, "cross-session messaging name unavailable");
                        }
                        Some(session)
                    }
                    Err(error) => {
                        warn!(%error, "cross-session messaging registration unavailable");
                        None
                    }
                }
            });
            let mut agent = Agent::new(
                AgentParams {
                    provider: Arc::clone(&provider),
                    model: model.clone(),
                    chat_provider: provider,
                    chat_model: model,
                    config: params.config,
                    tool_output_lines: ToolOutputLines::default(),
                    tool_output_store: crate::tool_output::default_store(),
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
                    changes,
                    prompt_slots: Arc::new(params.prompt_slots),
                    prompt_profiles: Arc::clone(&params.prompt_profiles),
                    default_task_prompt_profile_name: Arc::clone(&active_prompt_profile_name),
                    active_prompt_profile_name: Some(active_prompt_profile_name),
                    subagent_cancels: Arc::new(CancelMap::new()),
                    subagent_history: SubagentHistoryStore::default(),
                    registry: Arc::clone(ToolRegistry::global_arc()),
                    audience: ToolAudience::MAIN,
                    tool_filter,
                    tool_ceiling,
                    profile_tool_policy,
                    model_policy: Arc::clone(&params.model_policy),
                    workflow: None,
                    background: None,
                    jobs: None,
                    task_id: None,
                },
                AgentRunParams {
                    history: &mut history,
                    system,
                    environment: Some(agent::environment_block(&vars, &params.model)),
                    instructions: None,
                    memory: None,
                    mode_notice: None,
                    event_tx,
                    tools,
                    deferred,
                },
            )
            .with_loaded_instructions(instructions.loaded)
            .with_goal(params.goal)
            .with_background_wait()
            .with_peer_exit()
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
                    plan: None,
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
            if let Some(peer) = peer_session {
                peer.close();
            }

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

    Ok(HeadlessHandle {
        event_rx,
        tool_names,
        session_id: session_ref,
        cwd: working_dir,
        goal,
        task,
    })
}

pub struct InteractiveParams {
    pub model: Model,
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub decisions_config: DecisionsConfig,
    pub snapshots: SnapshotsConfig,
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
    pub seed_permission_mode: Option<PermissionMode>,
    pub structured_permission_rules: Vec<PermissionRuleRecord>,
    pub session_permission_mode: Option<PermissionMode>,
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
    /// `Some` serves the session's automations while `experimental.automations` is on.
    pub automations: Option<AutomationParams>,
}

/// What a session's automation runtime needs from its host.
pub struct AutomationParams {
    /// The mode an automation run starts in while no prompt has set the session's mode,
    /// thinking and speed. The session's facts name it too.
    pub mode: ModeResolver,
    pub fast: bool,
    pub config: AutomationsConfig,
    pub clock: Arc<dyn Clock>,
    /// Performs `http()`; without one every `http()` fails as unavailable.
    pub http: Option<Arc<dyn HttpClient>>,
    /// Holds the user's `automations/`; `None` reads the real config directory.
    pub user_config_dir: Option<PathBuf>,
}

pub struct InteractiveHandle {
    pub event_rx: Receiver<Envelope>,
    pub run_rx: Receiver<InteractiveRun>,
    pub background: Option<BackgroundTasks>,
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
    /// The session's goal, the one every run of it pursues.
    pub goal: GoalHandle,
    /// The session's automation runtime, when `automations` asked for one and it started.
    pub automations: Option<AutomationHandle>,
    /// What the runtime fired and what its scripts asked the host to show. Past its capacity
    /// the oldest event is dropped; disconnected once no runtime serves the session.
    pub automation_events: Receiver<AutomationEvent>,
    mode_route: Arc<InteractiveModeRoute>,
    workspace_change_tx: flume::Sender<WorkspaceChangeRequest>,
    remote_workspace: Option<Arc<StdMutex<RemoteWorkspaceState>>>,
    pub task: smol::Task<()>,
}

pub struct InteractiveRun {
    pub run_id: u64,
    pub started: Instant,
    pub automatic: bool,
    pub task_event_ids: Vec<String>,
    pub workflow_events: Vec<WorkflowEventOrigin>,
    /// The automation deliveries the run starts with.
    pub automation_events: Vec<AutomationEventOrigin>,
}

#[derive(Default)]
struct InteractiveModeRoute {
    control_epoch: AtomicU64,
    mode: ArcSwapOption<AgentMode>,
    admission: Mutex<()>,
    turn: Mutex<()>,
    active: StdMutex<Option<CancelTrigger>>,
}

impl InteractiveModeRoute {
    fn cancel_run(&self) {
        self.active
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
}

struct ActiveInteractiveRun(Arc<InteractiveModeRoute>);

impl Drop for ActiveInteractiveRun {
    fn drop(&mut self) {
        self.0.cancel_run();
    }
}

struct BackgroundDelivery {
    tasks: BackgroundTasks,
    messages: Vec<Message>,
}

impl Drop for BackgroundDelivery {
    fn drop(&mut self) {
        self.tasks.release_messages(&self.messages);
    }
}

/// The session's prompt queue as a run sees it: a waiting prompt holds back the goal check and
/// the guide and peer deliveries a run takes. Each prompt starts a run of its own, so a run never
/// takes one.
struct QueuedPrompts(Receiver<AgentInput>);

impl InterruptSource for QueuedPrompts {
    fn poll(&self) -> Option<ExtractedCommand> {
        None
    }

    fn has_pending_input(&self) -> bool {
        !self.0.is_empty()
    }
}

/// The main run whose goal evaluation last deferred behind session work, as
/// the event stream reports it. Every forwarded event wakes the check-in and
/// the automations' settle watch, since any of them can be that work settling.
#[derive(Default)]
struct GoalDeferral {
    run_id: StdMutex<Option<u64>>,
    changed: Event,
}

impl GoalDeferral {
    fn observe(&self, envelope: &Envelope) {
        if matches!(envelope.event, AgentEvent::GoalDeferred { .. }) {
            *self.lock() = Some(envelope.run_id);
        }
        self.changed.notify(usize::MAX);
    }

    fn deferred(&self, run_id: u64) -> bool {
        *self.lock() == Some(run_id)
    }

    fn lock(&self) -> MutexGuard<'_, Option<u64>> {
        self.run_id
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

/// Resolves once `due` holds, looking again whenever an event is forwarded,
/// background work changes, or the last foreground task ends.
async fn until_due(
    due: impl Fn() -> bool,
    deferral: &GoalDeferral,
    background: Option<&BackgroundTasks>,
    tasks: &CancelMap<String>,
) {
    loop {
        let forwarded = deferral.changed.listen();
        let background_changed = background.map(BackgroundTasks::listen);
        if due() {
            return;
        }
        futures_lite::future::or(
            futures_lite::future::or(forwarded, async {
                match background_changed {
                    Some(changed) => changed.await,
                    None => futures_lite::future::pending().await,
                }
            }),
            async {
                if tasks.active_count() == 0 {
                    futures_lite::future::pending::<()>().await;
                }
                tasks.wait_for_idle().await;
            },
        )
        .await;
    }
}

/// A run nobody typed, under the mode, thinking and speed of the last prompt
/// that was typed.
fn automatic_input(
    (mode, thinking, fast): &(AgentMode, ThinkingConfig, bool),
    preamble: Vec<Message>,
) -> AgentInput {
    AgentInput {
        message: String::new(),
        mode: mode.clone(),
        plan: None,
        thinking: thinking.clone(),
        fast: *fast,
        images: Vec::new(),
        mentions: Vec::new(),
        commits: Vec::new(),
        preamble,
        prompt: None,
        resume: false,
    }
}

/// The main agent records its own turns into the goal; what its subagents
/// spend is added here, as the TUI and print mode add it.
fn record_subagent_goal_usage(goal: &GoalHandle, envelope: &Envelope) {
    if envelope.subagent.is_none() {
        return;
    }
    match &envelope.event {
        AgentEvent::TurnComplete(turn) => {
            goal.record_external_usage(turn.usage, turn.cost, turn.billing);
        }
        AgentEvent::ModelUsage {
            usage,
            cost,
            billing,
            ..
        } => goal.record_external_usage(*usage, *cost, *billing),
        _ => {}
    }
}

/// `mode`, unless a control has set the session's mode since.
fn routed_mode(route: &Arc<InteractiveModeRoute>, mode: ModeResolver) -> ModeResolver {
    let route = Arc::clone(route);
    Arc::new(move || {
        route
            .mode
            .load_full()
            .map_or_else(|| mode(), |mode| (*mode).clone())
    })
}

/// What `event` adds to the session's API spend, in USD; a subscription pays for the rest.
fn billed_cost(event: &AgentEvent) -> Option<f64> {
    let (cost, billing) = match event {
        AgentEvent::TurnComplete(turn) => (turn.cost, turn.billing),
        AgentEvent::ModelUsage { cost, billing, .. }
        | AgentEvent::GoalEvaluation { cost, billing, .. }
        | AgentEvent::GoalEvaluationFailed { cost, billing, .. }
        | AgentEvent::SessionTitle { cost, billing, .. } => (*cost, *billing),
        _ => return None,
    };
    cost.filter(|_| matches!(billing, Billing::Api))
}

/// What the session billed before it resumed here, priced as the TUI prices a resume, without
/// touching the stored record.
fn restored_cost(session: &StoredSession, model: &Model) -> Option<f64> {
    let fast = session.meta.fast && model.supports_fast();
    settle_session(
        &session.token_usage,
        &mut session.usage_by_model().clone(),
        model,
        fast,
    )
    .billed
}

/// The session as automation events show it. Headless sessions have no `@name`, no groups and
/// no question tool, so they never wait on input.
fn session_view(
    session_id: CaudraId,
    title: &str,
    mode: &AgentMode,
    goal: &GoalHandle,
    cost: Option<f64>,
    status: SessionStatus,
    status_since: i64,
) -> SessionView {
    SessionView {
        id: session_id.to_string(),
        title: Untrusted::text(title),
        name: None,
        mode: mode_name(mode).into(),
        status,
        status_since,
        goal: goal.snapshot().map(|goal| GoalView {
            condition: goal.condition.to_string(),
            evaluations: goal.evaluations,
        }),
        cost,
        groups: Vec::new(),
        work: WorkView::default(),
    }
}

/// Events for the host, bounded: past capacity the oldest is dropped, and the runtime's mirror
/// still holds it.
#[derive(Clone)]
struct HostEvents {
    sender: flume::Sender<AutomationEvent>,
    oldest: Receiver<AutomationEvent>,
}

impl HostEvents {
    fn new() -> (Self, Receiver<AutomationEvent>) {
        let (sender, receiver) = flume::bounded(AUTOMATION_EVENT_CAPACITY);
        let events = Self {
            sender,
            oldest: receiver.clone(),
        };
        (events, receiver)
    }

    fn push(&self, event: AutomationEvent) {
        let Err(flume::TrySendError::Full(event)) = self.sender.try_send(event) else {
            return;
        };
        debug!("oldest automation event for the host dropped");
        let _ = self.oldest.try_recv();
        let _ = self.sender.try_send(event);
    }
}

/// A session's automation runtime as the loop, its forwarder and its events task share it.
struct SessionAutomations {
    handle: AutomationHandle,
    /// The mode an automation run starts in while no prompt has set the session's continuation.
    mode: ModeResolver,
    fast: bool,
    host: HostEvents,
    tally: StdMutex<AutomationTally>,
}

/// What the runtime heard from the session, so each signal goes out on a change only.
struct AutomationTally {
    signals: SessionSignals,
    /// The main run the runtime last heard start; it is in flight until the forwarder hands
    /// over its end.
    run: Option<u64>,
    /// The session's running API spend in USD, the bill it resumed with included.
    cost: Option<f64>,
}

impl SessionAutomations {
    fn tally(&self) -> MutexGuard<'_, AutomationTally> {
        self.tally.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Tells the runtime what the session looks like now: its facts, then what keeps it from
    /// settling. A `Busy` session is working.
    fn observe(&self, title: &str, goal: &GoalHandle, blockers: Vec<SettleBlocker>) {
        let now = self.handle.now_ms();
        let status = if blockers.contains(&SettleBlocker::Busy) {
            SessionStatus::Working
        } else {
            SessionStatus::Idle
        };
        let mode = (self.mode)();
        let signals = {
            let mut tally = self.tally();
            tally.signals.observe_goal(goal.snapshot());
            let facts = session_view(
                self.handle.session_id(),
                title,
                &mode,
                goal,
                tally.cost,
                status,
                tally.signals.status_since(status, now),
            );
            [
                tally.signals.facts(facts),
                tally.signals.settle(blockers, now),
            ]
        };
        for signal in signals.into_iter().flatten() {
            self.handle.signal(signal);
        }
    }

    /// A main run started, so the session is busy until the forwarder hands over its end.
    fn run_started(&self, run_id: u64, started_by: StartedBy, title: &str, goal: &GoalHandle) {
        {
            let mut tally = self.tally();
            tally.run = Some(run_id);
            let cost = tally.cost;
            tally
                .signals
                .run_started(started_by, self.handle.now_ms(), cost);
        }
        self.observe(title, goal, vec![SettleBlocker::Busy]);
    }

    /// Follows a forwarded event: the session's spend, the goal's end, and for the main run in
    /// flight the automation messages it took, its response so far and its end.
    fn forwarded(&self, envelope: &Envelope, goal: &GoalHandle) {
        let signal = {
            let mut tally = self.tally();
            add_cost(&mut tally.cost, billed_cost(&envelope.event));
            let main_run = envelope.subagent.is_none()
                && envelope.task.is_none()
                && tally.signals.running()
                && tally.run == Some(envelope.run_id);
            match &envelope.event {
                AgentEvent::GoalFinished { result } => Some(goal_finished(result)),
                AgentEvent::GoalClearedAfterError { condition, message } => {
                    Some(tally.signals.goal_cleared(condition, message))
                }
                AgentEvent::GoalEvaluation { .. } | AgentEvent::GoalEvaluationFailed { .. } => {
                    tally.signals.observe_goal(goal.snapshot());
                    None
                }
                AgentEvent::Injected {
                    automation_event: Some(origin),
                    ..
                } if main_run => {
                    tally.signals.injected(origin);
                    None
                }
                AgentEvent::TurnComplete(turn)
                    if main_run && matches!(turn.purpose, LedgerPurpose::Chat) =>
                {
                    tally.signals.turn_complete(&turn.message);
                    None
                }
                event if main_run => run_end(event).map(|(outcome, error)| {
                    let cost = tally.cost;
                    tally.signals.run_ended(outcome, error, cost)
                }),
                _ => None,
            }
        };
        if let Some(signal) = signal {
            self.handle.signal(signal);
        }
    }

    /// Whether the blockers would tell the runtime anything new.
    fn changes(&self, blockers: &[SettleBlocker]) -> bool {
        self.tally().signals.changes(blockers)
    }

    /// What keeps the session from settling: `Busy` while session work is pending or a run is,
    /// then `others`.
    fn blockers(&self, work_pending: bool, mut others: Vec<SettleBlocker>) -> Vec<SettleBlocker> {
        if work_pending || self.running() {
            others.insert(0, SettleBlocker::Busy);
        }
        others
    }

    /// Whether a main run started whose end the forwarder has not handed over yet. Its last
    /// response and cost come with that end, so the session stays busy until then.
    fn running(&self) -> bool {
        self.tally().signals.running()
    }

    /// The run a failure outside the loop's own runs reports under: the one in flight, or the
    /// last to end.
    fn last_run(&self) -> u64 {
        self.tally().run.unwrap_or_default()
    }

    /// Whether a `next` item could start a turn now: the runtime heard the session settle, and
    /// an item waits on nothing that still holds.
    fn due(&self) -> bool {
        self.tally().signals.settled() && delivery_due(&self.handle)
    }

    /// The mode, thinking and speed of an automation run no prompt has set them for.
    fn fallback(&self, thinking: &ThinkingConfig) -> (AgentMode, ThinkingConfig, bool) {
        ((self.mode)(), thinking.clone(), self.fast)
    }

    /// Claims the next item for the turn about to start. A stopping runtime keeps its items.
    async fn claim(&self) -> Option<OutboxClaim> {
        let session_id = self.handle.session_id();
        match claim_next(&self.handle).await {
            Ok(claim) => claim,
            Err(AutomationError::Unavailable) => {
                debug!(%session_id, "automation runtime stopped before its delivery claim");
                None
            }
            Err(error) => {
                warn!(%error, %session_id, "automation delivery claim failed");
                None
            }
        }
    }

    /// Sets the goal a claim asks for; a refusal reaches the host as a notice, and the turn
    /// starts either way.
    fn set_goal(&self, goal: &GoalHandle, claim: &OutboxClaim) {
        if let Some(requested) = &claim.goal
            && let Some(refusal) = set_claimed_goal(goal, requested)
        {
            self.host.push(AutomationEvent::Notice {
                automation: claim.automation.clone(),
                fire_id: Some(claim.fire_id.clone()),
                text: refusal,
            });
        }
    }
}

/// Serves the runtime's events, taking what is queued before `stop` closes: saves the session
/// the runtime asks to be saved, wakes the loop for outbox and session news, and hands firings
/// and notices to the host. The session's record exists before its runtime starts, so a save
/// request only writes it again.
async fn serve_automation_events(
    automations: Arc<SessionAutomations>,
    stop: Receiver<()>,
    store: Arc<Mutex<Option<SessionStore>>>,
    wake: flume::Sender<()>,
    errors: flume::Sender<Envelope>,
) {
    let events = automations.handle.events();
    loop {
        let event = futures_lite::future::or(async { events.recv_async().await.ok() }, async {
            let _ = stop.recv_async().await;
            None
        })
        .await;
        match event {
            None => break,
            Some(AutomationEvent::SaveSession) => {
                let Some(saved) = store.lock().await.as_mut().map(SessionStore::save) else {
                    continue;
                };
                match saved {
                    Ok(()) => automations.handle.signal(SessionSignal::Saved),
                    Err(error) => {
                        let _ = EventSender::new(errors.clone(), automations.last_run()).send(
                            AgentEvent::Error {
                                message: format!("{PERSIST_FAILED}: {error}"),
                            },
                        );
                    }
                }
            }
            Some(AutomationEvent::Outbox(_) | AutomationEvent::Session(_)) => {
                let _ = wake.try_send(());
            }
            Some(event @ (AutomationEvent::Firing { .. } | AutomationEvent::Notice { .. })) => {
                automations.host.push(event);
            }
            Some(AutomationEvent::Automation(_)) => {}
        }
    }
}

/// A session's automation runtime, ready to start beside its workflow runtime.
struct PendingAutomations {
    deps: AutomationDeps,
    mode: ModeResolver,
    fast: bool,
}

impl PendingAutomations {
    /// What the runtime starts from: the session as its record has it on opening.
    fn new(
        automation: AutomationParams,
        params: &InteractiveParams,
        store: &SessionStore,
        mode_route: &Arc<InteractiveModeRoute>,
        model: &Model,
    ) -> Self {
        let session_id = store.session.id;
        let mode = routed_mode(mode_route, automation.mode);
        let facts = session_view(
            session_id,
            &store.session.title,
            &mode(),
            &store.goal,
            restored_cost(&store.session, model),
            SessionStatus::Idle,
            automation.clock.now_ms() / MILLIS_PER_SECOND,
        );
        let deps = AutomationDeps {
            state_dir: store.dir.clone(),
            session_id,
            cwd: params.initial_wd.clone(),
            user_config_dir: automation.user_config_dir,
            remote: params.workspace_session.is_some()
                || params
                    .workspace_binding
                    .as_ref()
                    .is_some_and(|binding| binding.sandbox_record().is_some()),
            features: params.config.features,
            frontend: Frontend::Sdk,
            config: automation.config,
            controls: store.session.meta.automations.clone(),
            launch: launch_armings(params.system_prompt_profile.as_deref(), Vec::new()),
            facts,
            clock: automation.clock,
            http: automation.http,
            workflows: None,
        };
        Self {
            deps,
            mode,
            fast: automation.fast,
        }
    }

    /// Starts the runtime, which starts its workflows through `workflow`, and has every save of
    /// `store` keep its controls. One that cannot start leaves the session without automations.
    async fn spawn(
        self,
        workflow: Option<&WorkflowHandle>,
        store: &mut SessionStore,
    ) -> Option<(
        AutomationRuntime,
        SessionAutomations,
        Receiver<AutomationEvent>,
    )> {
        let Self {
            mut deps,
            mode,
            fast,
        } = self;
        deps.workflows = workflow.map(|workflow| Arc::new(workflow.clone()) as Arc<dyn Workflows>);
        let session_id = deps.session_id;
        let facts = deps.facts.clone();
        let runtime = AutomationRuntime::spawn(deps)
            .await
            .map_err(
                |error| warn!(%error, %session_id, "automation runtime unavailable for this session"),
            )
            .ok()?;
        info!(%session_id, "automation runtime started");
        let handle = runtime.handle();
        store.automations = Some(handle.clone());
        let (host, events) = HostEvents::new();
        let automations = SessionAutomations {
            handle,
            mode,
            fast,
            host,
            tally: StdMutex::new(AutomationTally {
                cost: facts.cost,
                signals: SessionSignals::new(facts),
                run: None,
            }),
        };
        Some((runtime, automations, events))
    }
}

struct RemoteWorkspaceState {
    session: WorkspaceSession,
    binding: StoredWorkspaceBinding,
    cwd: String,
    features: FeatureFlags,
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
            run_rx: flume::unbounded().1,
            background: None,
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
            goal: GoalHandle::default(),
            automations: None,
            automation_events: flume::unbounded().1,
            mode_route: Arc::default(),
            workspace_change_tx: flume::unbounded().0,
            remote_workspace: None,
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

    pub async fn set_mode(
        &self,
        mode: AgentMode,
        permission_mode: PermissionMode,
    ) -> Result<(), String> {
        self.mode_route.control_epoch.fetch_add(1, Ordering::AcqRel);
        let _admission = self.mode_route.admission.lock().await;
        self.mode_route.mode.store(Some(Arc::new(mode)));
        self.mode_route.cancel_run();
        let _turn = self.mode_route.turn.lock().await;
        let _background_transition = self
            .background
            .as_ref()
            .map(BackgroundTasks::suspend)
            .transpose()?;
        drain_session_work(self.background.as_ref(), self.workflow.as_ref()).await?;
        self.permissions.set_session_mode(Some(permission_mode));
        Ok(())
    }

    /// The host's only explicit stop: it holds every automation of the session until the next
    /// prompt, idle or not, before it stops the session's work.
    pub async fn interrupt(&self) -> Result<(), String> {
        if let Some(automations) = &self.automations {
            automations.signal(SessionSignal::Pause {
                by: PauseSource::Sdk,
            });
        }
        self.mode_route.control_epoch.fetch_add(1, Ordering::AcqRel);
        let _admission = self.mode_route.admission.lock().await;
        self.mode_route.cancel_run();
        let _turn = self.mode_route.turn.lock().await;
        drain_session_work(self.background.as_ref(), self.workflow.as_ref()).await
    }

    pub fn workflow_control(
        &self,
        request: WorkflowRequest,
    ) -> impl Future<Output = Result<WorkflowResponse, WorkflowError>> + Send + 'static + use<>
    {
        let route = Arc::clone(&self.mode_route);
        let epoch = route.control_epoch.load(Ordering::Acquire);
        let background = self.background.clone();
        let workflow = self.workflow.clone();
        async move {
            let workflow = workflow.ok_or(WorkflowError::Unavailable)?;
            if !matches!(
                &request,
                WorkflowRequest::Start(_) | WorkflowRequest::Resume { .. }
            ) {
                return workflow.request(request).await;
            }
            loop {
                let admission = route.admission.lock().await;
                if route.control_epoch.load(Ordering::Acquire) != epoch {
                    return Err(WorkflowError::Internal(STALE_WORKFLOW_CONTROL.into()));
                }
                if let Some(_turn) = route.turn.try_lock() {
                    if let Some(background) = &background {
                        background.rearm();
                    }
                    return workflow.request(request).await;
                }
                drop(admission);
                drop(route.turn.lock().await);
            }
        }
    }

    pub async fn change_remote_directory(&self, path: &str) -> Result<String, String> {
        let state = self
            .remote_workspace
            .as_ref()
            .ok_or_else(|| "cd: remote workspace is unavailable".to_owned())?;
        let (workspace, stored_binding, features) = {
            let state = state
                .lock()
                .map_err(|_| "cd: remote workspace state is unavailable".to_owned())?;
            (state.session.clone(), state.binding.clone(), state.features)
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
            match crate::remote_project_context::load_remote_project_context(&workspace, features)
                .await
            {
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

/// `None` while the decision engine is off, before anything engine-specific
/// is read, so no endpoint, credential, or question file can switch it on.
fn initialize_decisions(
    config: DecisionsConfig,
    state_dir: &StateDir,
    features: FeatureFlags,
) -> Result<Option<Decisions>, InteractiveStartError> {
    if !features.enabled(Feature::DecisionEngine) {
        return Ok(None);
    }
    Decisions::new(config, state_dir)
        .map(Some)
        .map_err(|error| InteractiveStartError(format!("{DECISIONS_STARTUP_FAILED}: {error}")))
}

pub struct PreparedInteractive {
    params: InteractiveParams,
    history: History,
    model: Model,
    provider: Arc<dyn Provider>,
    store: SessionStore,
    changes: Option<ChangeRecorder>,
}

impl PreparedInteractive {
    pub fn set_mcp_handle(&mut self, mcp_handle: Option<McpHandle>) {
        self.params.mcp_handle = mcp_handle;
    }
}

pub async fn prepare_interactive(
    mut params: InteractiveParams,
) -> Result<PreparedInteractive, InteractiveStartError> {
    params
        .prompt_profiles
        .resolve(params.system_prompt_profile_name.as_deref())
        .map_err(|error| InteractiveStartError(error.to_string()))?;
    if let Some(profile) = &params.system_prompt_profile {
        profile
            .tools()
            .validate_bindings(
                ToolRegistry::global()
                    .iter()
                    .iter()
                    .map(|entry| entry.name()),
            )
            .map_err(InteractiveStartError)?;
    }
    params
        .excluded_tools
        .extend_from_slice(crate::tools::native::peers::TOOL_NAMES);
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
        let workspace = crate::resume_workspace_session(&mut session, &workspace)
            .await
            .map_err(InteractiveStartError)?;
        params.initial_wd = session.cwd.clone().into();
        if let Some(environment) = &mut params.remote_environment {
            environment.cwd = session.cwd.clone();
        }
        params.remote_project_context = Some(
            crate::remote_project_context::load_remote_project_context(
                &workspace,
                params.config.features,
            )
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
    let changes = session_recorder(
        &mut store.session,
        params.workspace_session.is_some(),
        &params.snapshots,
    );
    store
        .save()
        .map_err(|error| InteractiveStartError(format!("{PERSIST_FAILED}: {error}")))?;
    Ok(PreparedInteractive {
        params,
        history: history.with_todos(store.todos()),
        model,
        provider,
        store,
        changes,
    })
}

pub async fn spawn_prepared_interactive(
    prepared: PreparedInteractive,
) -> Result<InteractiveHandle, InteractiveStartError> {
    spawn_prepared_session(prepared, false).await
}

pub async fn spawn_persistent_interactive(
    params: InteractiveParams,
) -> Result<InteractiveHandle, InteractiveStartError> {
    spawn_prepared_session(prepare_interactive(params).await?, true).await
}

async fn spawn_prepared_session(
    prepared: PreparedInteractive,
    background_enabled: bool,
) -> Result<InteractiveHandle, InteractiveStartError> {
    let PreparedInteractive {
        mut params,
        mut history,
        mut model,
        mut provider,
        mut store,
        changes,
    } = prepared;
    let decisions = initialize_decisions(
        params.decisions_config.clone(),
        &store.dir,
        params.config.features,
    )?;
    if !params.config.features.enabled(Feature::Workflows) {
        params.workflow_mode = None;
    }
    let workflows_available = params.workflow_mode.is_some();
    let AgentSetup {
        mut vars,
        instructions,
        mut tools,
        mut deferred,
        tool_filter,
    } = setup(
        &model,
        &params.config,
        &params.excluded_tools,
        workflows_available,
        TaskDescriptionContext {
            profile: params.system_prompt_profile.as_deref(),
            session_plan: false,
            prompt_profiles: &params.prompt_profiles,
            chat_model: &model,
            thinking: &params.thinking,
            model_policy: &params.model_policy,
            timeouts: params.timeouts,
            remote_project_context: params.remote_project_context.as_ref(),
            host_cwd: params.host_cwd.as_deref(),
        },
        params.remote_environment.as_ref(),
    );

    crate::tools::execution::configure_tools(
        &mut tools,
        &mut deferred,
        &params.config,
        background_enabled,
        background_enabled,
    );
    let mut baseline = agent::InstructionBaseline::adopt(instructions, history.epoch());
    let mut memory = MemoryBaseline::open(
        params.local_documents.clone(),
        Some(store.dir.clone()),
        PathBuf::from(vars.apply("{cwd}").as_ref()),
        &history,
    )
    .await;

    let initial_messages = history.as_slice();
    let mcp = params.mcp_handle.clone().map(|h| {
        McpSession::new(h, initial_messages)
            .with_disabled_tools(&params.config.disabled_tools)
            .with_actor_policy(Arc::new(
                params
                    .system_prompt_profile
                    .as_ref()
                    .map(|profile| profile.tools().clone())
                    .unwrap_or_default(),
            ))
    });
    let tool_names = advertised_tool_names(
        &tools,
        &deferred,
        mcp.as_ref(),
        params
            .local_tools
            .contains_key(crate::tools::TOOL_SEARCH_TOOL_NAME),
    );

    let session_ref = params.session_id.clone();
    let session_id = session_ref.id();
    let memory_origin = session_ref.as_str().to_owned();
    let session_lease = Arc::clone(&params.session_lease);
    let subagent_history = store.subagent_history.clone();
    let goal = store.goal.clone();
    let goal_deferral = Arc::new(GoalDeferral::default());
    let state_dir = store.dir.clone();
    let background = if background_enabled {
        Some(
            BackgroundTasks::spawn(state_dir.clone(), session_id)
                .await
                .map_err(InteractiveStartError)?,
        )
    } else {
        None
    };

    let (raw_tx, event_rx) = flume::unbounded::<Envelope>();
    let (run_tx, run_rx) = flume::unbounded();
    let (agent_tx, agent_rx) = flume::unbounded::<Envelope>();
    let (input_tx, input_rx) = flume::unbounded::<AgentInput>();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();
    let (cancel_tx, cancel_rx) = flume::bounded::<()>(1);
    let (model_tx, model_rx) = flume::unbounded::<Model>();
    let (workspace_change_tx, workspace_change_rx) = flume::unbounded::<WorkspaceChangeRequest>();
    let (workflow_wake_tx, workflow_wake_rx) = flume::bounded::<()>(1);
    let mode_route = Arc::new(InteractiveModeRoute::default());
    let pending_automations = params
        .automations
        .take()
        .filter(|_| params.config.features.enabled(Feature::Automations))
        .map(|automation| {
            PendingAutomations::new(automation, &params, &store, &mode_route, &model)
        });
    let remote_workspace = params
        .workspace_session
        .clone()
        .zip(params.workspace_binding.clone())
        .map(|(session, binding)| {
            Arc::new(StdMutex::new(RemoteWorkspaceState {
                session,
                binding,
                cwd: params.initial_wd.to_string_lossy().into_owned(),
                features: params.config.features,
            }))
        });

    let permissions = Arc::new(PermissionManager::new_persistent_in(
        params.permissions_config,
        params.initial_wd.clone(),
        Arc::clone(&params.plugin_rules),
        state_dir.clone(),
    ));
    if let Some(mode) = params.seed_permission_mode {
        permissions.set_seed_mode(mode);
    }
    permissions.set_decisions(decisions.map(|service| service.for_session(session_id)));
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
    permissions.set_session_mode(params.session_permission_mode);
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
    let memory_pump = memory
        .store()
        .filter(|_| params.config.summarize_memory)
        .map(|memory_store| {
            Pump::start(
                Arc::clone(memory_store),
                Arc::clone(&provider),
                model.clone(),
                Arc::clone(&params.model_policy),
                params.timeouts,
                EventSender::new(agent_tx.clone(), MEMORY_EVENT_RUN_ID),
                Some(Ledger {
                    state: state_dir.clone(),
                    cwd: store.session.cwd.clone(),
                }),
            )
        });
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
        tool_output_store: Some(Arc::new(ToolOutputStore::new(state_dir.clone()))),
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
        changes,
        prompt_slots: Arc::clone(&params.prompt_slots),
        prompt_profiles: Arc::clone(&params.prompt_profiles),
        default_task_prompt_profile_name: Arc::clone(&active_prompt_profile_name),
        active_prompt_profile_name: Some(active_prompt_profile_name),
        subagent_cancels: Arc::new(CancelMap::new()),
        subagent_history,
        registry: Arc::clone(ToolRegistry::global_arc()),
        audience: ToolAudience::MAIN,
        tool_filter: tool_filter.clone(),
        tool_ceiling: ToolFilter::ceiling_from_config(&params.config, &params.excluded_tools),
        profile_tool_policy: Arc::new(
            params
                .system_prompt_profile
                .as_ref()
                .map(|profile| profile.tools().clone())
                .unwrap_or_default(),
        ),
        model_policy: Arc::clone(&params.model_policy),
        workflow: None,
        background: background.clone(),
        jobs: None,
        task_id: None,
    };

    // Workflow agents resolve the provider at launch, so a model switched
    // mid-session reaches runs that outlive the turn which switched it.
    let live_model = Arc::new(ArcSwap::from_pointee((
        Arc::clone(&provider),
        Arc::new(model.clone()),
    )));
    let mut runtime = match params.workflow_mode.take() {
        Some(mode) => {
            let mode = routed_mode(&mode_route, mode);
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
            WorkflowRuntime::spawn(
                RuntimeDeps {
                    state_dir: state_dir.clone(),
                    session_id,
                    cwd: params.initial_wd.clone(),
                    user_config_dir: None,
                    remote_project_context: params.remote_project_context.clone(),
                    runner: Arc::new(SubagentTaskRunner::new(Arc::new(host))),
                    events: agent_tx.clone(),
                    mode,
                    subagent_cancels,
                    features: params.config.features,
                },
                permissions.decisions(),
            )
            .await
            .map_err(|error| warn!(%error, "workflow runtime unavailable for this session"))
            .ok()
        }
        None => None,
    };
    let workflow = runtime.as_ref().map(WorkflowRuntime::handle);
    if let (Some(workflow), Some(background)) = (&workflow, &background)
        && let Err(error) = workflow.bind_background(background.clone()).await
    {
        if let Some(runtime) = runtime.take() {
            runtime.shutdown().await;
        }
        if let Err(error) = background.shutdown().await {
            warn!(%error, "background shutdown after workflow binding failure failed");
        }
        return Err(InteractiveStartError(error.to_string()));
    }
    let started = match (pending_automations, store.lock().await.as_mut()) {
        (Some(pending), Some(session_store)) => {
            pending.spawn(workflow.as_ref(), session_store).await
        }
        _ => None,
    };
    let (automation_runtime, automations, automation_events) = match started {
        Some((runtime, automations, events)) => {
            (Some(runtime), Some(Arc::new(automations)), events)
        }
        None => (None, None, flume::bounded(0).1),
    };
    let (automation_wake_tx, automation_wake_rx) = flume::bounded::<()>(1);
    if background_enabled
        && workflow
            .as_ref()
            .is_some_and(|workflow| workflow.pending_completions() > 0)
    {
        let _ = workflow_wake_tx.try_send(());
    }
    let model_route = workflow.as_ref().map(|_| InteractiveModelRoute {
        model: Arc::clone(&live_model),
        timeouts: params.timeouts,
    });
    let mut base = AgentParams {
        workflow: workflow.clone(),
        ..base
    };

    let task = smol::spawn({
        let mode_route = Arc::clone(&mode_route);
        let permissions = Arc::clone(&permissions);
        let workflow = workflow.clone();
        let background = background.clone();
        let live_model = Arc::clone(&live_model);
        let remote_workspace = remote_workspace.clone();
        let goal = goal.clone();
        let automations = automations.clone();
        async move {
            let event_forwarder = smol::spawn({
                let store = Arc::clone(&store);
                let raw_tx = raw_tx.clone();
                let background = background.clone();
                let workflow_wake_tx = workflow_wake_tx.clone();
                let goal = goal.clone();
                let goal_deferral = Arc::clone(&goal_deferral);
                let automations = automations.clone();
                async move {
                    while let Ok(envelope) = agent_rx.recv_async().await {
                        if (envelope.task.is_some() || envelope.run_id == BACKGROUND_EVENT_RUN_ID)
                            && !background
                                .as_ref()
                                .is_some_and(|tasks| tasks.owns_event(&envelope))
                        {
                            continue;
                        }
                        record_subagent_goal_usage(&goal, &envelope);
                        // Before the deferral wakes the settle watch, so it sees the run's end.
                        if let Some(automations) = &automations {
                            automations.forwarded(&envelope, &goal);
                        }
                        goal_deferral.observe(&envelope);
                        let persistence_error = if let Some(store) = &mut *store.lock().await {
                            store.record_event(&envelope).err()
                        } else {
                            None
                        };
                        let run_id = envelope.run_id;
                        if background_enabled
                            && matches!(&envelope.event, AgentEvent::Workflow(event) if matches!(event.as_ref(), WorkflowEvent::Snapshot(snapshot) if snapshot.outbox_pending))
                        {
                            let _ = workflow_wake_tx.try_send(());
                        }
                        if persistence_error.is_some()
                            && let Some(background) = &background
                            && let Err(error) = background.stop().await
                        {
                            warn!(%error, "background stop after persistence failure failed");
                        }
                        if raw_tx.send_async(envelope).await.is_err() {
                            break;
                        }
                        if let Some(error) = persistence_error
                            && raw_tx
                                .send_async(Envelope {
                                    event: AgentEvent::Error {
                                        message: format!("{PERSIST_FAILED}: {error}"),
                                    },
                                    subagent: None,
                                    run_id,
                                    task: None,
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
            let automation_events_task = automations.as_ref().map(|automations| {
                let (stop, stopped) = flume::bounded::<()>(1);
                let task = smol::spawn(serve_automation_events(
                    Arc::clone(automations),
                    stopped,
                    Arc::clone(&store),
                    automation_wake_tx,
                    raw_tx.clone(),
                ));
                (stop, task)
            });
            let mut run_id: u64 = 0;
            let mut continuation: Option<(AgentMode, ThinkingConfig, bool)> = None;
            // The control epoch the last run started under: a stop, an
            // interrupt or a mode change since then calls its check-in off,
            // as stopping work does in the TUI.
            let mut run_epoch = 0;

            loop {
                if background_enabled && input_rx.is_disconnected() {
                    break;
                }
                enum NextInput {
                    Prompt(Box<Result<AgentInput, flume::RecvError>>),
                    Workspace(Box<Result<WorkspaceChangeRequest, flume::RecvError>>),
                    Stop,
                    Background,
                    Workflow,
                    GoalCheckin,
                    Automation,
                }
                // The TUI's check-in rule: the last run deferred its goal
                // evaluation, the goal is still active, nothing stopped the
                // session since, no prompt waits, and the work it deferred
                // behind has settled.
                let checkin_due = || {
                    run_id
                        .checked_sub(1)
                        .is_some_and(|last| goal_deferral.deferred(last))
                        && mode_route.control_epoch.load(Ordering::Acquire) == run_epoch
                        && continuation.is_some()
                        && input_rx.is_empty()
                        && goal.snapshot().is_some()
                        && !SessionWork::capture(
                            background.as_ref(),
                            workflow.as_ref(),
                            base.subagent_cancels.active_count(),
                        )
                        .pending()
                };
                // What keeps the session from settling, as its automations see it. Completions
                // keep the session busy and wait ahead of them only once a continuation lets a
                // wake deliver them; until then the next run carries them, whoever starts it.
                let settle_blockers = |automations: &SessionAutomations| {
                    let work = SessionWork::capture(
                        background.as_ref(),
                        workflow.as_ref(),
                        base.subagent_cancels.active_count(),
                    );
                    let completions = background
                        .as_ref()
                        .is_some_and(BackgroundTasks::has_pending)
                        || workflow
                            .as_ref()
                            .is_some_and(|workflow| workflow.pending_completions() > 0);
                    let wakes_deliver = background_enabled && continuation.is_some();
                    automations.blockers(
                        work.running
                            || work.unavailable
                            || (work.settling && (wakes_deliver || !completions)),
                        [
                            (!input_rx.is_empty()).then_some(SettleBlocker::PromptQueued),
                            (wakes_deliver && completions).then_some(SettleBlocker::MailboxWake),
                            (background_enabled && checkin_due())
                                .then_some(SettleBlocker::GoalCheckin),
                        ]
                        .into_iter()
                        .flatten()
                        .collect(),
                    )
                };
                if let Some(automations) = &automations {
                    let title = session_title(&store).await;
                    automations.observe(&title, &goal, settle_blockers(automations));
                }
                let next = futures_lite::future::or(
                    async {
                        if cancel_rx.recv_async().await.is_err() {
                            futures_lite::future::pending::<()>().await;
                        }
                        NextInput::Stop
                    },
                    async {
                        // What follows a run reaches the runtime after the run's end does.
                        if let Some(automations) = &automations {
                            until_due(
                                || !automations.running(),
                                &goal_deferral,
                                background.as_ref(),
                                &base.subagent_cancels,
                            )
                            .await;
                        }
                        futures_lite::future::or(
                            async { NextInput::Prompt(Box::new(input_rx.recv_async().await)) },
                            futures_lite::future::or(
                                async {
                                    NextInput::Workspace(Box::new(
                                        workspace_change_rx.recv_async().await,
                                    ))
                                },
                                futures_lite::future::or(
                                    async {
                                        if let Some(background) = &background {
                                            background.notified().await;
                                        } else {
                                            futures_lite::future::pending::<()>().await;
                                        }
                                        NextInput::Background
                                    },
                                    futures_lite::future::or(
                                        async {
                                            if !background_enabled {
                                                futures_lite::future::pending::<()>().await;
                                            }
                                            if workflow_wake_rx.recv_async().await.is_err() {
                                                futures_lite::future::pending::<()>().await;
                                            }
                                            NextInput::Workflow
                                        },
                                        futures_lite::future::or(
                                            async {
                                                if !background_enabled {
                                                    futures_lite::future::pending::<()>().await;
                                                }
                                                until_due(
                                                    &checkin_due,
                                                    &goal_deferral,
                                                    background.as_ref(),
                                                    &base.subagent_cancels,
                                                )
                                                .await;
                                                NextInput::GoalCheckin
                                            },
                                            async {
                                                let Some(automations) = &automations else {
                                                    return futures_lite::future::pending().await;
                                                };
                                                // Runtime news, or blockers the runtime has not
                                                // heard.
                                                futures_lite::future::or(
                                                    async {
                                                        if automation_wake_rx
                                                            .recv_async()
                                                            .await
                                                            .is_err()
                                                        {
                                                            futures_lite::future::pending::<()>()
                                                                .await;
                                                        }
                                                    },
                                                    until_due(
                                                        || {
                                                            automations.changes(&settle_blockers(
                                                                automations,
                                                            ))
                                                        },
                                                        &goal_deferral,
                                                        background.as_ref(),
                                                        &base.subagent_cancels,
                                                    ),
                                                )
                                                .await;
                                                NextInput::Automation
                                            },
                                        ),
                                    ),
                                ),
                            ),
                        )
                        .await
                    },
                )
                .await;
                let admission = mode_route.admission.lock().await;
                let _turn = mode_route.turn.lock().await;
                let automatic = matches!(
                    next,
                    NextInput::Background
                        | NextInput::Workflow
                        | NextInput::GoalCheckin
                        | NextInput::Automation
                );
                let origin = match &next {
                    NextInput::Prompt(_) => Some(StartedBy::User),
                    NextInput::GoalCheckin => Some(StartedBy::Goal),
                    _ => None,
                };
                let delivering = matches!(next, NextInput::Automation);
                let mut input = match next {
                    NextInput::Prompt(received) => {
                        let Ok(input) = *received else {
                            break;
                        };
                        if let Some(automations) = &automations {
                            automations.handle.signal(SessionSignal::HumanInput);
                        }
                        if let Some(background) = &background {
                            background.rearm();
                        }
                        input
                    }
                    NextInput::Stop => {
                        mode_route.control_epoch.fetch_add(1, Ordering::AcqRel);
                        stop_session_work(background.as_ref(), workflow.as_ref()).await;
                        continue;
                    }
                    NextInput::Background | NextInput::Workflow => {
                        let Some(continuation) = &continuation else {
                            continue;
                        };
                        let Some(background) = &background else {
                            continue;
                        };
                        match background.workflow_admission().await {
                            Ok(permit) => drop(permit),
                            Err(_) => continue,
                        }
                        automatic_input(continuation, Vec::new())
                    }
                    NextInput::GoalCheckin => {
                        let (Some(continuation), Some(active)) = (&continuation, goal.snapshot())
                        else {
                            continue;
                        };
                        automatic_input(
                            continuation,
                            vec![Message::synthetic(agent::goal_checkin_message(
                                &active.condition,
                            ))],
                        )
                    }
                    NextInput::Automation => {
                        let Some(automations) = &automations else {
                            continue;
                        };
                        if !input_rx.is_empty() || !automations.due() {
                            continue;
                        }
                        match &continuation {
                            Some(continuation) => automatic_input(continuation, Vec::new()),
                            None => {
                                automatic_input(&automations.fallback(&params.thinking), Vec::new())
                            }
                        }
                    }
                    NextInput::Workspace(change) => {
                        let Ok(change) = *change else {
                            continue;
                        };
                        if !input_rx.is_empty()
                            || background.as_ref().is_some_and(|tasks| {
                                tasks.active_count() > 0 || tasks.has_pending()
                            })
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
                        let _background_transition =
                            match prepare_background_transition(background.as_ref()).await {
                                Ok(transition) => transition,
                                Err(error) => {
                                    let _ = change.response.send(Err(error));
                                    continue;
                                }
                            };
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
                if let Some(mode) = mode_route.mode.load_full() {
                    input.mode = (*mode).clone();
                }
                if input.plan.is_none()
                    && let Some(store) = &*store.lock().await
                {
                    input.plan = store
                        .session
                        .meta
                        .plan_target
                        .as_ref()
                        .map(PlanTarget::from);
                }
                if !automatic {
                    continuation = Some((input.mode.clone(), input.thinking.clone(), input.fast));
                }
                let started = Instant::now();
                let workflow_delivery = match &workflow {
                    Some(workflow) => match completion_messages(workflow).await {
                        Ok(messages) => messages,
                        Err(message) => {
                            let _ = EventSender::new(agent_tx.clone(), run_id)
                                .send(AgentEvent::Error { message });
                            stop_session_work(background.as_ref(), Some(workflow)).await;
                            run_id += 1;
                            continue;
                        }
                    },
                    None => Vec::new(),
                };
                let delivery = match &background {
                    Some(tasks) => match tasks.claim_messages() {
                        Ok(messages) => Some(BackgroundDelivery {
                            tasks: tasks.clone(),
                            messages,
                        }),
                        Err(message) => {
                            let _ = EventSender::new(agent_tx.clone(), run_id)
                                .send(AgentEvent::Error { message });
                            if let Err(error) = tasks.stop().await {
                                warn!(%error, "background stop failed");
                            }
                            run_id += 1;
                            continue;
                        }
                    },
                    None => None,
                };
                // The runtime records an item delivered as it hands it over, so the claim comes
                // after everything that could still call this turn off.
                let claim = match &automations {
                    Some(automations) if delivering && input_rx.is_empty() => {
                        automations.claim().await
                    }
                    _ => None,
                };
                if let (Some(automations), Some(claim)) = (&automations, &claim) {
                    automations.set_goal(&goal, claim);
                    input.preamble.push(claim_message(claim));
                }
                if automatic
                    && input.preamble.is_empty()
                    && workflow_delivery.is_empty()
                    && delivery
                        .as_ref()
                        .is_none_or(|delivery| delivery.messages.is_empty())
                {
                    continue;
                }
                continuation.get_or_insert_with(|| {
                    (input.mode.clone(), input.thinking.clone(), input.fast)
                });
                run_epoch = mode_route.control_epoch.load(Ordering::Acquire);
                let background_messages = delivery
                    .as_ref()
                    .map_or(0, |delivery| delivery.messages.len());
                if let Some(delivery) = &delivery {
                    input
                        .preamble
                        .splice(0..0, delivery.messages.iter().cloned());
                }
                input.preamble.extend(workflow_delivery.iter().cloned());
                if background_enabled {
                    let _ = run_tx.send(InteractiveRun {
                        run_id,
                        started,
                        automatic,
                        task_event_ids: input
                            .preamble
                            .iter()
                            .filter_map(|message| {
                                message
                                    .task_event
                                    .as_ref()
                                    .map(|origin| origin.event_id.clone())
                            })
                            .collect(),
                        workflow_events: workflow_delivery
                            .iter()
                            .filter_map(|message| message.workflow_event.clone())
                            .collect(),
                        automation_events: input
                            .preamble
                            .iter()
                            .filter_map(|message| message.automation_event.clone())
                            .collect(),
                    });
                }
                if let Some(automations) = &automations {
                    let started_by = match claim {
                        Some(OutboxClaim {
                            automation,
                            fire_id,
                            ..
                        }) => StartedBy::Automation {
                            automation,
                            fire_id,
                        },
                        None => origin.unwrap_or_else(|| {
                            wake_origin(false, workflow_delivery.len(), background_messages)
                        }),
                    };
                    let title = session_title(&store).await;
                    automations.run_started(run_id, started_by, &title, &goal);
                }
                let input_mode = input.mode.clone();
                let session_plan = input.session_plan();
                let (trigger, cancel) = CancelToken::new();
                *mode_route
                    .active
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(trigger);
                let _active_run = ActiveInteractiveRun(Arc::clone(&mode_route));
                let cancel_task = smol::spawn({
                    let cancel_rx = cancel_rx.clone();
                    let mode_route = Arc::clone(&mode_route);
                    async move {
                        if cancel_rx.recv_async().await.is_ok() {
                            mode_route.control_epoch.fetch_add(1, Ordering::AcqRel);
                            mode_route.cancel_run();
                        }
                    }
                });
                drop(admission);

                // MCP connects in the background, so a prompt that beats it waits
                // here instead of shipping a turn without the MCP tools. The wait
                // is racing cancel: a slow server must not pin the whole session.
                if let Some(mcp) = &mcp {
                    let _ = cancel.race(mcp.ready()).await;
                }

                let event_tx = EventSender::new(agent_tx.clone(), run_id);
                let error_tx = event_tx.clone();

                if let Some(workspace) = &params.workspace_session {
                    match crate::remote_project_context::load_remote_project_context(
                        workspace,
                        params.config.features,
                    )
                    .await
                    {
                        Ok(context) => {
                            let changed =
                                params.remote_project_context.as_ref().is_none_or(|old| {
                                    old.manifest_revision() != context.manifest_revision()
                                });
                            let _background_transition = if changed {
                                match prepare_background_transition(background.as_ref()).await {
                                    Ok(transition) => transition,
                                    Err(message) => {
                                        let _ = error_tx.send(AgentEvent::Error { message });
                                        cancel_task.cancel().await;
                                        stop_session_work(background.as_ref(), workflow.as_ref())
                                            .await;
                                        run_id += 1;
                                        continue;
                                    }
                                }
                            } else {
                                None
                            };
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
                                        stop_session_work(background.as_ref(), workflow.as_ref())
                                            .await;
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
                                stop_session_work(background.as_ref(), workflow.as_ref()).await;
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
                                stop_session_work(background.as_ref(), workflow.as_ref()).await;
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
                            stop_session_work(background.as_ref(), workflow.as_ref()).await;
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
                                cancel_task.cancel().await;
                                stop_session_work(background.as_ref(), workflow.as_ref()).await;
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
                        stop_session_work(background.as_ref(), workflow.as_ref()).await;
                        run_id += 1;
                        continue;
                    }
                };
                let mut definitions = tool_definitions(
                    &vars,
                    &turn_model,
                    &params.config,
                    &params.excluded_tools,
                    ToolRegistry::global(),
                    workflows_available,
                    TaskDescriptionContext {
                        profile: params.system_prompt_profile.as_deref(),
                        session_plan: input.has_session_plan(),
                        prompt_profiles: &params.prompt_profiles,
                        chat_model: &model,
                        thinking: &input.thinking,
                        model_policy: &params.model_policy,
                        timeouts: params.timeouts,
                        remote_project_context: params.remote_project_context.as_ref(),
                        host_cwd: params.host_cwd.as_deref(),
                    },
                );

                crate::tools::execution::configure_tools(
                    &mut definitions.declared,
                    &mut definitions.deferred,
                    &params.config,
                    background.is_some(),
                    background.is_some(),
                );
                let turn_tool_filter = definitions.available_filter().for_mode(&input.mode);
                let execution_slots = crate::tools::execution::execution_slots(
                    &params.prompt_slots,
                    &params.config,
                    background.is_some(),
                    background.is_some(),
                    &definitions.declared,
                    &definitions.deferred,
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
                let memory_notice = memory.refresh(&history, Some(&memory_origin)).await;

                let mut system = params.system_prompt_override.clone().unwrap_or_else(|| {
                    agent::build_system_prompt(
                        baseline.text(),
                        &execution_slots,
                        &turn_tool_filter,
                        params.system_prompt_profile.as_deref(),
                        memory.view(),
                    )
                });
                if let Some(append) = &params.append_system_prompt {
                    system.push('\n');
                    system.push_str(append);
                }

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
                        memory: memory_notice.filter(|_| params.system_prompt_override.is_none()),
                        mode_notice: None,
                        event_tx,
                        tools: definitions.declared,
                        deferred: definitions.deferred,
                    },
                )
                .with_loaded_instructions(baseline.loaded().clone())
                .with_user_response_rx(Arc::clone(&answer_rx))
                .with_cancel(cancel.clone())
                .with_local_tools(Arc::clone(&params.local_tools))
                .with_goal(goal.clone())
                .with_interrupt_source(Arc::new(QueuedPrompts(input_rx.clone())))
                .with_mcp(mcp.clone());

                let result = agent.run(input).await;
                drop(agent);
                cancel_task.cancel().await;
                if let Some(pump) = &memory_pump {
                    pump.nudge();
                }

                if cancel.is_cancelled() || !matches!(result, Ok(DoneReason::EndTurn)) {
                    mode_route.control_epoch.fetch_add(1, Ordering::AcqRel);
                    stop_session_work(background.as_ref(), workflow.as_ref()).await;
                }

                if let Err(ref e) = result {
                    error!(error = %e, "agent error");
                    let _ = error_tx.send(AgentEvent::Error {
                        message: e.user_message(),
                    });
                }

                let mut persisted = false;
                if let Some(store) = &mut *store.lock().await {
                    store.set_mode(&input_mode);
                    if let Some(plan) = session_plan {
                        store.bind_plan(plan);
                    }
                    if matches!(result, Ok(DoneReason::Cancelled)) {
                        store.kill_unfinished_subagents();
                    }
                    if let Err(error) = store.record_turn(&history, model.spec(), &permissions) {
                        let _ = EventSender::new(raw_tx.clone(), run_id).send(AgentEvent::Error {
                            message: format!("{PERSIST_FAILED}: {error}"),
                        });
                    } else {
                        persisted = true;
                    }
                }
                if let Some(tasks) = &background {
                    let accepted = if persisted {
                        finalize_background_history(tasks, history.as_slice()).await
                    } else {
                        if let Err(error) = tasks.finalize_messages(&[]).await {
                            warn!(%error, "background claim reconciliation after failed save failed");
                        }
                        Err(SESSION_DATABASE_UNAVAILABLE.to_owned())
                    };
                    if let Err(message) = accepted {
                        let _ = error_tx.send(AgentEvent::Error { message });
                        stop_session_work(background.as_ref(), workflow.as_ref()).await;
                    }
                }
                if let Some(workflow) = &workflow
                    && let Err(message) =
                        acknowledge_workflow_messages(workflow, &workflow_delivery).await
                {
                    let _ = error_tx.send(AgentEvent::Error { message });
                    stop_session_work(background.as_ref(), Some(workflow)).await;
                }
                if background_enabled
                    && workflow
                        .as_ref()
                        .is_some_and(|workflow| workflow.pending_completions() > 0)
                {
                    let _ = workflow_wake_tx.try_send(());
                }
                run_id += 1;
            }

            // Automations stop first, so no firing outlives the work it may have started, and
            // the final save keeps the stopped runtime's last controls.
            if let Some(runtime) = automation_runtime {
                stop_runtime(runtime).await;
            }
            if let Some((stop, events)) = automation_events_task {
                drop(stop);
                events.await;
            }
            // Active runs are interrupted and their agents drained before the
            // session is saved, so nothing writes to it afterwards.
            base.subagent_cancels.cancel_all();
            if let Some(background) = &background
                && let Err(message) = background.shutdown().await
            {
                let _ =
                    EventSender::new(agent_tx.clone(), run_id).send(AgentEvent::Error { message });
            }
            if let Some(runtime) = runtime {
                runtime.shutdown().await;
            }
            drop(base);
            drop(memory_pump);
            drop(agent_tx);
            event_forwarder.await;
            if let Some(store) = &mut *store.lock().await {
                store.sync_permissions(&permissions);
                if let Err(error) = store.save() {
                    let _ = EventSender::new(raw_tx.clone(), run_id).send(AgentEvent::Error {
                        message: format!("{PERSIST_FAILED}: {error}"),
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
        run_rx,
        background,
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
        goal,
        automations: automations.map(|automations| automations.handle.clone()),
        automation_events,
        mode_route,
        workspace_change_tx,
        remote_workspace,
        task,
    })
}

async fn session_title(store: &Mutex<Option<SessionStore>>) -> String {
    store
        .lock()
        .await
        .as_ref()
        .map(|store| store.session.title.clone())
        .unwrap_or_default()
}

async fn stop_session_work(
    background: Option<&BackgroundTasks>,
    workflow: Option<&WorkflowHandle>,
) {
    if let Err(error) = drain_session_work(background, workflow).await {
        warn!(%error, "session work stop failed");
    }
}

async fn drain_session_work(
    background: Option<&BackgroundTasks>,
    workflow: Option<&WorkflowHandle>,
) -> Result<(), String> {
    if let Some(background) = background {
        background.stop().await?;
    }
    if let Some(workflow) = workflow {
        let response = workflow
            .request(WorkflowRequest::Status { run_id: None })
            .await
            .map_err(|error| error.to_string())?;
        let WorkflowResponse::Runs(runs) = response else {
            return Err("Workflow status unavailable while stopping session".into());
        };
        for run in runs {
            if matches!(
                run.status,
                RunStatus::Active | RunStatus::Paused | RunStatus::BudgetLimited
            ) {
                workflow
                    .request(WorkflowRequest::Stop { run_id: run.run_id })
                    .await
                    .map_err(|error| error.to_string())?;
            }
        }
    }
    Ok(())
}

async fn prepare_background_transition(
    background: Option<&BackgroundTasks>,
) -> Result<Option<BackgroundTransition>, String> {
    let Some(background) = background else {
        return Ok(None);
    };
    if background.active_count() > 0 || background.has_pending() {
        return Err("Workspace changes require quiescent background tasks".into());
    }
    let transition = background.suspend()?;
    transition.drain().await?;
    Ok(Some(transition))
}

async fn finalize_background_history(
    background: &BackgroundTasks,
    messages: &[Message],
) -> Result<(), String> {
    background.finalize_messages(messages).await?;
    background.settle_launches(messages).await?;
    Ok(())
}

pub async fn spawn_interactive(
    params: InteractiveParams,
) -> Result<InteractiveHandle, InteractiveStartError> {
    let prepared = prepare_interactive(params).await?;
    spawn_prepared_interactive(prepared).await
}

async fn completion_messages(workflow: &WorkflowHandle) -> Result<Vec<Message>, String> {
    if workflow.pending_completions() == 0 {
        return Ok(Vec::new());
    }
    let runs = match workflow
        .request(WorkflowRequest::Status { run_id: None })
        .await
    {
        Ok(WorkflowResponse::Runs(runs)) => runs,
        Ok(_) => return Err("Workflow status unavailable while claiming completions".into()),
        Err(error) => return Err(error.to_string()),
    };
    let mut messages = Vec::new();
    for run in runs.into_iter().filter(|run| run.outbox_pending) {
        let origin = WorkflowEventOrigin {
            run_id: run.run_id.clone(),
            revision: run.revision,
        };
        if workflow
            .received_completion(origin.clone())
            .await
            .map_err(|error| error.to_string())?
        {
            workflow
                .request(WorkflowRequest::AckCompletion {
                    run_id: origin.run_id,
                    revision: origin.revision,
                })
                .await
                .map_err(|error| error.to_string())?;
        } else {
            messages.push(Message::workflow_observation(
                completion_block(&run),
                origin,
            ));
        }
    }
    Ok(messages)
}

async fn acknowledge_workflow_messages(
    workflow: &WorkflowHandle,
    claimed: &[Message],
) -> Result<(), String> {
    for origin in claimed
        .iter()
        .filter_map(|message| message.workflow_event.as_ref())
    {
        if workflow
            .received_completion(origin.clone())
            .await
            .map_err(|error| error.to_string())?
        {
            workflow
                .request(WorkflowRequest::AckCompletion {
                    run_id: origin.run_id.clone(),
                    revision: origin.revision,
                })
                .await
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
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
    #[cfg(unix)]
    use std::fs::Permissions;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use caudra_automation::event::{
        ArmedReason, Event as FiredEvent, EventDetail, GoalVerdict as FinishedVerdict,
    };
    use caudra_automation::limits::ROLLING_WINDOW_MS;
    use caudra_automation::request::{AutomationRequest, AutomationResponse};
    use caudra_automation::snapshot::{ArmOrigin, FiringStatus, FiringSummary, WaitReason};
    use caudra_providers::{
        AgentError, ProviderEvent, RequestOptions, StandingReminderKind, StopReason,
        StreamResponse, TaskEventOrigin,
    };
    use caudra_storage::background::TaskRecord;
    use caudra_storage::decision_log::DecisionLog;
    use caudra_storage::permission_state::PermissionRuleRecord;
    use caudra_storage::permission_state::mutation::{
        PermissionMutation, PermissionRecordIdentity, prepare_mutation,
    };
    use caudra_storage::sessions::{RecordCoverage, generate_title};
    use caudra_storage::workflow::WorkflowRunStatus;
    use caudra_workflow::{RunStatus, WorkflowError, WorkflowEvent};
    use caudra_workspace::PlanRef;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::agent::subagent::TaskIdentity;
    use crate::agent::task_runner::TaskRequest;
    use crate::automation::clock::FakeClock;
    use crate::automation::manager::{GOAL_ACTIVE, PAUSED_BY_SDK};
    use crate::automation::testing::AutomationFixture;
    use crate::background::TaskDelivery;
    use crate::permissions::{
        PermissionAnswer, PermissionError, PermissionLifetime, PermissionRequest, RevokedRuleScope,
    };
    use crate::remote_project_context::tests::AssetService;
    use crate::tools::native::plan::{self, PlanTarget, PlanWriteResult};
    use crate::tools::registry::BoxFuture;
    use crate::tools::test_support::stub_ctx_with;
    use crate::tools::{PermissionScopes, TODOWRITE_TOOL_NAME};
    #[cfg(unix)]
    use crate::types::ToolDoneEvent;
    use crate::types::{TodoPriority, TodoStatus};
    use crate::workflow::store::WorkflowStore;
    use crate::{GoalResult, GoalVerdict, TurnCompleteEvent};

    const SESSION_ID: &str = "CNK1hV6GWoysH3KQMm5wu";
    const CWD: &str = "/project";
    const MODEL_SPEC: &str = "anthropic/claude-test";
    const PERMISSION_REQUEST_ID: &str = "headless-permission";
    const PERMISSION_COMMAND: &str = "headless-permission-command";
    const PERMISSION_TOOL: &str = "bash";
    const DEFERRED_MCP_TOOL: &str = "srv.fetch_issue";
    const PERMISSION_OPTION: &str = "allow_exact";
    const LOST_PERMISSION_ACK: &str = "permission committed but acknowledgment lost";
    const PLANNING_PROMPT: &str = "plan the migration";
    const COMPACTION_SUMMARY: &str = "the migration was planned";
    const FIRST_TODO_CALL: &str = "todo-1";
    const REVISED_TODO_CALL: &str = "todo-2";
    const FIRST_TODO: &str = "draft the schema";
    const REVISED_TODO: &str = "backfill the rows";
    const CHANGE_PROMPT: &str = "change the files";
    const ENGINE_ON: FeatureFlags = FeatureFlags::NONE.with(Feature::DecisionEngine);

    #[test_case(true ; "a_remote_run_records_on_its_host")]
    #[test_case(false ; "a_local_run_records_nothing")]
    fn only_a_remote_run_records_changes(remote: bool) {
        let recorder = remote_recorder(remote, session_id(), &SnapshotsConfig::default());

        assert_eq!(
            recorder.as_ref().map(ChangeRecorder::root),
            remote.then_some(Path::new(UNREVEALED_ROOT))
        );
    }

    #[test_case(true ; "a_remote_run_keeps_what_its_host_covers")]
    #[test_case(false ; "a_local_run_covers_nothing")]
    fn a_run_covers_only_what_it_records(remote: bool) {
        let binding = if remote {
            let (workspace, _) = AssetService::permission_fixture();
            StoredWorkspaceBinding::new_with_cursor(
                workspace.binding().clone(),
                workspace.cursor().clone(),
                None,
            )
            .unwrap()
        } else {
            StoredWorkspaceBinding::local_from_cwd(CWD)
        };
        let coverage = Some(RecordCoverage {
            store: binding.change_store_key(),
            since: None,
        });
        let mut session = StoredSession::new_with_workspace(MODEL_SPEC, CWD, binding);
        session
            .replace_messages(History::new(vec![Message::user(CHANGE_PROMPT.into())]).into_items());
        session.meta.record_coverage = coverage.clone();

        session_recorder(&mut session, remote, &SnapshotsConfig::default());

        assert_eq!(session.meta.record_coverage, coverage.filter(|_| remote));
    }

    #[test_case(false; "passive_config")]
    #[test_case(true; "restricted_without_endpoint")]
    fn decision_startup_preserves_marker_only_config_and_session_isolation(restricted: bool) {
        let temp = TempDir::new().unwrap();
        let state = StateDir::from_path(temp.path().to_path_buf());
        let config = DecisionsConfig {
            auto_screening_restricted: restricted,
            ..DecisionsConfig::default()
        };
        let first = initialize_decisions(config.clone(), &state, ENGINE_ON)
            .unwrap()
            .unwrap();
        let second = initialize_decisions(config, &state, ENGINE_ON)
            .unwrap()
            .unwrap();
        let manager = permission_manager();
        manager.set_decisions(Some(first.clone()));
        assert_eq!(
            manager
                .decisions()
                .unwrap()
                .config()
                .auto_screening_restricted,
            restricted
        );
        first.mark_tainted();
        assert!(manager.decisions().unwrap().is_tainted());
        assert!(!second.is_tainted());
        assert!(!DecisionLog::file_path(&state).exists());
    }

    #[test]
    fn invalid_decision_startup_is_an_actionable_error_not_an_absent_service() {
        let temp = TempDir::new().unwrap();
        let state = StateDir::from_path(temp.path().to_path_buf());
        let config = DecisionsConfig {
            model: String::new(),
            ..DecisionsConfig::default()
        };
        let error = initialize_decisions(config, &state, ENGINE_ON)
            .err()
            .unwrap();
        assert!(error.to_string().contains(DECISIONS_STARTUP_FAILED));
    }

    #[test]
    fn disabled_engine_reads_no_decision_config() {
        let temp = TempDir::new().unwrap();
        let state = StateDir::from_path(temp.path().to_path_buf());
        let unusable = DecisionsConfig {
            model: String::new(),
            ..DecisionsConfig::default()
        };
        assert!(
            initialize_decisions(unusable, &state, FeatureFlags::NONE)
                .unwrap()
                .is_none()
        );
        assert!(!DecisionLog::file_path(&state).exists());
    }

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
            PermissionsConfig {
                decision_engine: true,
                ..PermissionsConfig::default()
            },
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

    fn open_todo(content: &str) -> Vec<TodoItem> {
        vec![TodoItem {
            content: content.into(),
            status: TodoStatus::InProgress,
            priority: TodoPriority::High,
        }]
    }

    fn record_todo_update(
        store: &mut SessionStore,
        history: &mut History,
        call_id: &str,
        todos: Vec<TodoItem>,
    ) {
        history.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                call_id,
                TODOWRITE_TOOL_NAME,
                serde_json::json!({}),
            )],
            ..Default::default()
        });
        history.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: call_id.into(),
                content: String::new(),
                is_error: false,
                output_ref: None,
            }],
            ..Default::default()
        });
        store
            .session
            .insert_tool_output(call_id.into(), ToolOutput::TodoList(todos));
        store
            .record_turn(history, MODEL_SPEC.into(), &permission_manager())
            .unwrap();
    }

    /// The first list is compacted away before the second is written, so a
    /// reopened session reads across the seam, and a head rewound to the
    /// compaction summary predates the second list.
    #[test_case(false => Some(open_todo(REVISED_TODO)) ; "the_latest_list")]
    #[test_case(true => Some(open_todo(FIRST_TODO)) ; "the_list_at_a_rewound_head")]
    fn a_reopened_session_restores_its_todo_list(rewound: bool) -> Option<Vec<TodoItem>> {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        let mut history = History::new(vec![Message::user(PLANNING_PROMPT.into())]);
        record_todo_update(
            &mut store,
            &mut history,
            FIRST_TODO_CALL,
            open_todo(FIRST_TODO),
        );
        let seam = history.item_at_message_boundary(history.len());
        history.replace_superseding(vec![Message::user(COMPACTION_SUMMARY.into())], seam);
        let summary_head = history.item_head();
        record_todo_update(
            &mut store,
            &mut history,
            REVISED_TODO_CALL,
            open_todo(REVISED_TODO),
        );
        if rewound {
            store.session.meta.history_head = summary_head;
            store.save().unwrap();
        }
        drop(store);

        store_in(&tmp).todos()
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

    #[test_case(false, false, true, true; "direct_rewind")]
    #[test_case(false, true, true, true; "direct_continuation")]
    #[test_case(true, false, true, true; "batch_rewind")]
    #[test_case(true, true, true, true; "batch_continuation")]
    #[test_case(false, false, false, true; "direct_recover_exact")]
    #[test_case(true, false, false, true; "batch_recover_exact")]
    #[test_case(false, false, false, false; "direct_missing_refuses_latest")]
    #[test_case(true, false, false, false; "batch_missing_refuses_latest")]
    fn reload_phrase_task_selects_typed_history_before_continuing(
        batch: bool,
        continued: bool,
        loaded: bool,
        durable: bool,
    ) {
        const TASK: &str = "happy-cute-tick";
        const LAUNCH: &str = "launch-call";
        const CONTINUATION: &str = "continuation-call";
        const NEXT: &str = "next-call";
        const RUNTIME: &str = "runtime-not-history";
        const STALE: &str = "wrong branch";
        const MISSING: &str = "selected task history version is unavailable";
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        let mut history = History::default();
        let mut first_head = None;
        let database = SessionDatabase::open(&store.dir).unwrap();
        let tool_result = |call: &str| Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: call.into(),
                content: String::new(),
                is_error: false,
                output_ref: None,
            }],
            ..Default::default()
        };
        for call in [LAUNCH, CONTINUATION] {
            let version = if batch {
                format!("{call}-child")
            } else {
                call.into()
            };
            let output: ToolOutput = serde_json::from_value(serde_json::json!({"Tasks": [{
                "task_id": TASK, "call_id": version, "invocation_id": RUNTIME,
                "root_call_id": call, "label": TASK, "state": "succeeded", "mode": "build",
                "background": true, "generation": 1, "created_at": 0, "updated_at": 0
            }]}))
            .unwrap();
            let output = if batch {
                serde_json::from_value(serde_json::json!({"Batch": {
                    "entries": [{"tool": "task", "summary": TASK, "status": "Success", "output": output}],
                    "text": ""
                }})).unwrap()
            } else {
                output
            };
            store.session.insert_tool_output(call.into(), output);
            if loaded || call == CONTINUATION {
                store.session.set_subagent_history(
                    version.clone(),
                    History::new(vec![Message::user(call.into())]).into_items(),
                    Some(crate::SubagentTaskSpec::version()),
                );
            }
            if durable || call == CONTINUATION {
                database
                    .save_background_task(
                        session_id(),
                        &TaskRecord {
                            payload: Default::default(),
                            owner: Default::default(),
                            created_at: 0,
                            updated_at: 0,
                            sequence: if call == LAUNCH { 1 } else { 2 },
                            task_id: TASK.into(),
                            invocation_id: format!("{RUNTIME}-{call}"),
                            root_call_id: call.into(),
                            generation: 1,
                            state: "succeeded".into(),
                            background: true,
                            receipt_accepted: true,
                            mode: "build".into(),
                            request: serde_json::json!({"call_id": version, "label": TASK}),
                            outcome: None,
                            output_ref: None,
                            history: serde_json::to_value(vec![Message::user(call.into())])
                                .unwrap(),
                            spec: serde_json::to_value(crate::SubagentTaskSpec::default()).unwrap(),
                            events: Vec::new(),
                        },
                    )
                    .unwrap();
            }
            history.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    call,
                    if batch { "batch" } else { "task" },
                    serde_json::json!({}),
                )],
                ..Default::default()
            });
            history.push(tool_result(call));
            if call == LAUNCH {
                first_head = history.active_items().last().map(|item| item.id);
            }
        }
        for id in [TASK, RUNTIME] {
            store.session.set_subagent_history(
                id.into(),
                History::new(vec![Message::user(STALE.into())]).into_items(),
                Some(crate::SubagentTaskSpec::default()),
            );
        }
        store
            .session
            .replace_messages(history.active_items().to_vec());
        if !continued {
            store.session.meta.history_head = first_head;
            history = History::restored(
                active_history_items(store.session.messages(), first_head).unwrap(),
            )
            .unwrap();
        }
        store.save().unwrap();
        drop(store);

        let mut reopened = store_in(&tmp);
        let selected = if continued { CONTINUATION } else { LAUNCH };
        let version = if batch {
            format!("{selected}-child")
        } else {
            selected.into()
        };
        assert_eq!(
            reopened.subagent_history.selected_version(TASK).as_deref(),
            Some(version.as_str())
        );
        let provider = WorkflowSession::new(false);
        let mode = AgentMode::Build;
        let mut ctx = stub_ctx_with(&mode, None, Some(NEXT));
        ctx.subagent_history = reopened.subagent_history.clone();
        ctx.provider = provider.provider.clone();
        let tasks =
            smol::block_on(BackgroundTasks::spawn(reopened.dir.clone(), session_id())).unwrap();
        let result = smol::block_on(tasks.execute(
            &ctx,
            TaskRequest {
                task: TaskIdentity::Continue(TASK.into()),
                call_id: NEXT.into(),
                prompt: Some(NEXT.into()),
                label: TASK.into(),
                mode: None,
                profile: None,
                model_job: None,
                output_schema: None,
                provenance: None,
            },
            false,
        ));
        smol::block_on(tasks.shutdown()).unwrap();
        if !loaded && !durable {
            assert!(
                matches!(result, Err(error) if error == format!("{MISSING}: {TASK} at {version}"))
            );
            assert!(provider.provider.requests.lock().unwrap().is_empty());
            assert!(
                !reopened
                    .subagent_history
                    .snapshot()
                    .records()
                    .contains_key(TASK)
            );
            return;
        }
        assert!(matches!(result.unwrap(), TaskDelivery::Foreground(outcome, _) if outcome.success));
        let requests = provider.provider.requests.lock().unwrap();
        let observed = &requests[0];
        assert!(
            observed
                .iter()
                .any(|message| message.user_text() == Some(selected))
        );
        assert!(
            observed
                .iter()
                .any(|message| message.user_text() == Some(NEXT))
        );
        assert!(
            !observed
                .iter()
                .any(|message| message.user_text() == Some(STALE))
        );
        if !continued {
            assert!(
                !observed
                    .iter()
                    .any(|message| message.user_text() == Some(CONTINUATION))
            );
        }
        history.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                NEXT,
                "task",
                serde_json::json!({"task_id": TASK}),
            )],
            ..Default::default()
        });
        history.push(tool_result(NEXT));
        reopened
            .record_turn(&history, MODEL_SPEC.into(), &permission_manager())
            .unwrap();
        drop(reopened);

        let reopened = store_in(&tmp);
        let lease = reopened
            .subagent_history
            .continue_task_with(TASK, Default::default())
            .unwrap();
        let messages = lease.history().unwrap();
        assert_eq!(messages[0].user_text(), Some(selected));
        assert!(
            messages
                .iter()
                .any(|message| message.user_text() == Some(NEXT))
        );
        assert_eq!(
            reopened.subagent_history.selected_version(TASK).as_deref(),
            Some(NEXT)
        );
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
                task: None,
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
                task: None,
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
                task: None,
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
        let expected = PlanRef::new(format!("plan-{}", "a".repeat(32))).expect("plan ref");
        let other = PlanRef::new(format!("plan-{}", "b".repeat(32))).expect("plan ref");
        store.session.meta.plan_target = Some(StoredPlanTarget::PlanRef {
            reference: expected.clone(),
        });

        let record = |store: &mut SessionStore, reference: &PlanRef| {
            let mut done = crate::ToolDoneEvent::error("write".into(), "written");
            done.tool = plan::NAME.into();
            done.is_error = false;
            done.annotation = Some(
                PlanWriteResult::new(
                    PlanTarget::Remote(reference.clone()),
                    PLANNING_PROMPT.into(),
                )
                .annotation()
                .unwrap(),
            );
            store
                .record_event(&Envelope {
                    event: AgentEvent::ToolDone(Box::new(done)),
                    subagent: None,
                    run_id: 0,
                    task: None,
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
                    task: None,
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
                task: None,
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

    #[test_case(false, Some(PermissionMode::Yolo); "acknowledged")]
    #[test_case(true, Some(PermissionMode::Yolo); "lost_acknowledgment")]
    #[test_case(false, Some(PermissionMode::Auto); "auto")]
    #[test_case(false, Some(PermissionMode::Ask); "explicit_ask")]
    #[test_case(false, None; "unset")]
    fn record_turn_checkpoints_restorable_permissions(
        lost_ack: bool,
        mode: Option<PermissionMode>,
    ) {
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
            permissions.set_session_mode(mode.clone());
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
            assert_eq!(loaded.meta.permission_mode, mode);
            let restored = permission_manager();
            restored
                .attach_permission_publication(publication.clone())
                .unwrap();
            restored.set_session_mode(loaded.meta.permission_mode.clone());
            assert_eq!(restored.conversation_permission_snapshot(), Some(approved));
            assert_eq!(restored.mode(), mode.clone().unwrap_or(PermissionMode::Ask));
            assert_eq!(restored.persisted_mode(), mode);
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

        let names = advertised_tool_names(&base, &deferred, session.as_ref(), false);

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
        assert_eq!(advertised_tool_names(&base, &[], None, false), vec!["read"]);
    }

    #[test]
    fn advertised_names_respect_hidden_local_search_binding() {
        let base = serde_json::json!([]);
        let session = crate::mcp::stub_session(&[(DEFERRED_MCP_TOOL, DEFERRED_MCP_TOOL)]);
        assert!(advertised_tool_names(&base, &[], Some(&session), true).is_empty());
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
    const TASK_INJECTION_MISSING: &str = "turn ended without task injection";
    const NO_RUNTIME: &str = "the session must attach a workflow runtime";
    const AGENT_NEVER_STARTED: &str = "the workflow agent never reached the provider";
    const REPORTED_ONCE: &str = "a completion is reported in one prompt only";
    const UPDATED_MODEL_ID: &str = "updated-chat-model";

    /// Answers every request with `ANSWER`, or parks forever once built to
    /// hang. Each request's messages are kept, and `started` fires per
    /// request so a test can wait for an agent to be in flight.
    struct ScriptedProvider {
        hang: bool,
        responses: StdMutex<Option<Receiver<StreamResponse>>>,
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
                let responses = self.responses.lock().unwrap().clone();
                if let Some(responses) = responses {
                    return Ok(responses.recv_async().await.unwrap());
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
        features: FeatureFlags,
        local_documents: Option<Arc<LocalDocumentStore>>,
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
                    responses: StdMutex::new(None),
                    started: started_tx,
                    requests: std::sync::Mutex::new(Vec::new()),
                    models: std::sync::Mutex::new(Vec::new()),
                    systems: std::sync::Mutex::new(Vec::new()),
                }),
                started,
                features: FeatureFlags::all(),
                local_documents: None,
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
            self.spawn_session(workflows, workspace, false).await
        }

        async fn spawn_session(
            &self,
            workflows: bool,
            workspace: Option<WorkspaceSession>,
            background: bool,
        ) -> InteractiveHandle {
            self.spawn_with(workflows, workspace, background, None)
                .await
        }

        async fn spawn_with(
            &self,
            workflows: bool,
            workspace: Option<WorkspaceSession>,
            background: bool,
            automations: Option<AutomationParams>,
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
                    crate::remote_project_context::load_remote_project_context(
                        workspace,
                        FeatureFlags::all(),
                    )
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
                    summarize_memory: false,
                    features: self.features,
                    ..AgentConfig::default()
                },
                permissions_config: PermissionsConfig {
                    decision_engine: self.features.enabled(Feature::DecisionEngine),
                    ..PermissionsConfig::default()
                },
                decisions_config: DecisionsConfig::default(),
                snapshots: SnapshotsConfig::default(),
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
                seed_permission_mode: Some(PermissionMode::Yolo),
                structured_permission_rules: store.session.meta.structured_permission_rules.clone(),
                session_permission_mode: store.session.meta.permission_mode.clone(),
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
                local_documents: self.local_documents.clone(),
                automations,
            };
            spawn_prepared_session(
                PreparedInteractive {
                    params,
                    history: History::default(),
                    model: Model::from_spec(MODEL_SPEC).unwrap(),
                    provider: Arc::clone(&self.provider) as Arc<dyn Provider>,
                    store,
                    changes: None,
                },
                background,
            )
            .await
            .unwrap()
        }
    }

    async fn shutdown_interactive(handle: InteractiveHandle) {
        let InteractiveHandle { input_tx, task, .. } = handle;
        drop(input_tx);
        task.await;
    }

    struct ControlledChild {
        responses: Receiver<StreamResponse>,
        started: flume::Sender<()>,
    }

    impl Provider for ControlledChild {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                self.started.send(()).unwrap();
                Ok(self.responses.recv_async().await.unwrap())
            })
        }

        fn list_models(
            &self,
        ) -> BoxFuture<'_, Result<Vec<caudra_providers::ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    async fn background_child(
        tasks: &BackgroundTasks,
        id: &str,
    ) -> (flume::Sender<StreamResponse>, Receiver<()>, String) {
        background_child_in(tasks, id, AgentMode::Build).await
    }

    async fn background_child_in(
        tasks: &BackgroundTasks,
        id: &str,
        mode: AgentMode,
    ) -> (flume::Sender<StreamResponse>, Receiver<()>, String) {
        let (responses, rx) = flume::unbounded();
        let (started, starts) = flume::unbounded();
        let mut ctx = stub_ctx_with(&mode, None, Some(id));
        ctx.provider = Arc::new(ControlledChild {
            responses: rx,
            started,
        });
        let (event_tx, events) = flume::unbounded();
        ctx.event_tx = EventSender::new(event_tx, 0);
        let TaskDelivery::Background(receipt) = tasks
            .execute(
                &ctx,
                TaskRequest {
                    prompt: Some(PROMPT.into()),
                    label: id.into(),
                    task: TaskIdentity::Derive,
                    mode: None,
                    profile: None,
                    model_job: None,
                    output_schema: None,
                    call_id: id.into(),
                    provenance: None,
                },
                true,
            )
            .await
            .unwrap()
        else {
            panic!("expected background receipt")
        };
        let admission = events.recv_async().await.unwrap();
        let AgentEvent::TaskAdmitted(card) = admission.event else {
            panic!("expected task admission")
        };
        assert_eq!(card.task_id, receipt.task_id);
        assert_eq!(card.invocation_id, receipt.invocation_id);
        assert_eq!(card.call_id, id);
        assert_eq!(card.task_id, id);
        tasks
            .settle_launches(&[Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: id.into(),
                    content: "admitted".into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            }])
            .await
            .unwrap();
        starts.recv_async().await.unwrap();
        (responses, starts, card.task_id)
    }

    #[test]
    fn background_reminders_do_not_repeat_by_default_or_wake_idle_parent() {
        const CHILD: &str = "reminder-child";
        const RESPONSE_GROUPS: u32 = 16;
        smol::block_on(async {
            let session = WorkflowSession::new(false);
            let handle = session.spawn_session(false, None, true).await;
            let tasks = handle.background.as_ref().unwrap();
            let (_child, _, task_id) = background_child(tasks, CHILD).await;
            for index in 0..=RESPONSE_GROUPS {
                handle.input_tx.send(prompt(PROMPT)).unwrap();
                wait_for_turn(&handle.event_rx).await;
                let run = handle.run_rx.recv_async().await.unwrap();
                assert!(!run.automatic);
                assert!(run.task_event_ids.is_empty());
                let requests = session.provider.requests.lock().unwrap();
                assert_eq!(requests.len(), index as usize + 1);
                let snapshots: Vec<_> = requests
                    .last()
                    .unwrap()
                    .iter()
                    .filter(|message| {
                        message.standing_reminder == Some(StandingReminderKind::BackgroundWork)
                    })
                    .collect();
                assert_eq!(snapshots.len(), 1);
                assert!(
                    snapshots
                        .last()
                        .unwrap()
                        .first_text_content()
                        .unwrap()
                        .contains(&task_id)
                );
                assert!(handle.run_rx.is_empty());
            }
            shutdown_interactive(handle).await;
            let database = SessionDatabase::open(&session.state_dir).unwrap();
            let records = database.background_tasks(session_id()).unwrap();
            assert!(
                records
                    .iter()
                    .all(|record| record.events.iter().all(|event| !event.accepted))
            );
        });
    }

    #[test_case(false; "terminal_after_final_answer")]
    #[test_case(true; "report_while_sibling_active")]
    fn background_delivery_starts_another_parent_run_without_user_input(report: bool) {
        const CHILD: &str = "detached-child";
        const SIBLING: &str = "detached-sibling";
        const REPORT_CALL: &str = "report-call";
        smol::block_on(async {
            let session = WorkflowSession::new(false);
            let handle = session.spawn_session(false, None, true).await;
            let tasks = handle.background.as_ref().unwrap();
            handle.input_tx.send(prompt(PROMPT)).unwrap();
            wait_for_turn(&handle.event_rx).await;
            let first = handle.run_rx.recv_async().await.unwrap();
            assert!(!first.automatic);
            let (child, starts, task_id) = background_child(tasks, CHILD).await;
            let sibling = if report {
                Some(background_child(tasks, SIBLING).await)
            } else {
                None
            };
            child
                .send(StreamResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![if report {
                            ContentBlock::tool_use(
                                REPORT_CALL,
                                "report_to_parent",
                                serde_json::json!({"message": ANSWER}),
                            )
                        } else {
                            ContentBlock::Text {
                                text: ANSWER.into(),
                            }
                        }],
                        ..Default::default()
                    },
                    stop_reason: Some(if report {
                        StopReason::ToolUse
                    } else {
                        StopReason::EndTurn
                    }),
                    ..Default::default()
                })
                .unwrap();
            if report {
                starts.recv_async().await.unwrap();
            }
            let (text, origin) = wait_for_task_injection(&handle.event_rx).await;
            wait_for_turn(&handle.event_rx).await;
            let second = handle.run_rx.recv_async().await.unwrap();
            assert!(second.automatic);
            assert!(second.run_id > first.run_id);
            assert_eq!(second.task_event_ids.len(), 1);
            assert_eq!(session.provider.user_prompts(), vec![PROMPT, PROMPT]);
            {
                let requests = session.provider.requests.lock().unwrap();
                let message = requests
                    .last()
                    .unwrap()
                    .iter()
                    .find(|message| message.task_event.as_ref() == Some(&origin))
                    .unwrap();
                assert_eq!(message.user_text(), Some(text.as_str()));
            }
            assert_eq!(origin.task_id, task_id);
            assert_eq!(origin.event_id, second.task_event_ids[0]);
            assert_eq!(
                origin.invocation_id,
                tasks.status(&task_id).unwrap().invocation_id
            );
            assert!(text.contains(ANSWER));
            assert!(!text.contains(&origin.invocation_id));
            assert!(!text.contains(&origin.event_id));
            assert!(!text.contains("tool_output"));
            if report {
                assert_eq!(tasks.active_count(), 2);
            }
            shutdown_interactive(handle).await;
            drop(sibling);
            let database = SessionDatabase::open(&session.state_dir).unwrap();
            assert!(
                database
                    .background_event_accepted(session_id(), &second.task_event_ids[0])
                    .unwrap()
            );
        });
    }

    #[test_case(false; "acp_opt_out")]
    #[test_case(true; "sdk_opt_in")]
    fn background_capability_requires_explicit_session_opt_in(enabled: bool) {
        smol::block_on(async {
            let session = WorkflowSession::new(false);
            let handle = session.spawn_session(false, None, enabled).await;
            assert_eq!(handle.background.is_some(), enabled);
            shutdown_interactive(handle).await;
        });
    }

    #[test]
    fn busy_report_is_acknowledged_from_saved_history_without_an_idle_claim() {
        const CHILD: &str = "busy-child";
        const REPORT_CALL: &str = "busy-report";
        smol::block_on(async {
            let session = WorkflowSession::new(false);
            let (responses, rx) = flume::unbounded();
            *session.provider.responses.lock().unwrap() = Some(rx);
            let handle = session.spawn_session(false, None, true).await;
            handle.input_tx.send(prompt(PROMPT)).unwrap();
            session.started.recv_async().await.unwrap();
            let tasks = handle.background.as_ref().unwrap();
            let (child, starts, task_id) = background_child(tasks, CHILD).await;
            child
                .send(StreamResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::tool_use(
                            REPORT_CALL,
                            "report_to_parent",
                            serde_json::json!({"message": ANSWER}),
                        )],
                        ..Default::default()
                    },
                    stop_reason: Some(StopReason::ToolUse),
                    ..Default::default()
                })
                .unwrap();
            starts.recv_async().await.unwrap();
            assert!(tasks.has_pending());
            let answer = || StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: ANSWER.into(),
                    }],
                    ..Default::default()
                },
                stop_reason: Some(StopReason::EndTurn),
                ..Default::default()
            };
            responses.send(answer()).unwrap();
            session.started.recv_async().await.unwrap();
            assert!(!tasks.has_pending());
            let origin = {
                let requests = session.provider.requests.lock().unwrap();
                requests
                    .last()
                    .unwrap()
                    .iter()
                    .find_map(|message| message.task_event.as_ref().cloned())
                    .unwrap()
            };
            let (text, injected_origin) = wait_for_task_injection(&handle.event_rx).await;
            assert_eq!(injected_origin, origin);
            assert_eq!(origin.task_id, task_id);
            assert!(text.contains(ANSWER));
            responses.send(answer()).unwrap();
            wait_for_turn(&handle.event_rx).await;
            let runs: Vec<_> = handle.run_rx.try_iter().collect();
            assert_eq!(runs.len(), 1);
            assert!(runs[0].task_event_ids.is_empty());
            shutdown_interactive(handle).await;
            let database = SessionDatabase::open(&session.state_dir).unwrap();
            assert!(
                database
                    .background_event_accepted(session_id(), &origin.event_id)
                    .unwrap()
            );
            let record = database
                .background_tasks(session_id())
                .unwrap()
                .into_iter()
                .find(|record| record.task_id == task_id)
                .unwrap();
            assert!(
                record
                    .events
                    .iter()
                    .any(|event| event.event_id == origin.event_id && event.accepted)
            );
        });
    }

    #[test]
    fn stopping_background_work_suppresses_late_wakes_and_next_user_turn_is_not_cancelled() {
        const CHILD: &str = "stopped-child";
        smol::block_on(async {
            let session = WorkflowSession::new(false);
            let handle = session.spawn_session(false, None, true).await;
            handle.input_tx.send(prompt(PROMPT)).unwrap();
            wait_for_turn(&handle.event_rx).await;
            let tasks = handle.background.as_ref().unwrap();
            let _child = background_child(tasks, CHILD).await;
            tasks.stop().await.unwrap();
            assert_eq!(tasks.active_count(), 0);
            assert!(!tasks.has_pending());
            handle.cancel_tx.send(()).unwrap();
            handle.input_tx.send(prompt(SECOND_PROMPT)).unwrap();
            wait_for_turn(&handle.event_rx).await;
            assert_eq!(session.provider.user_prompts(), vec![PROMPT, SECOND_PROMPT]);
            assert!(handle.run_rx.try_iter().all(|run| !run.automatic));
            shutdown_interactive(handle).await;
        });
    }

    const GOAL: &str = "the findings are summarized";
    const OTHER_GOAL: &str = "the findings are filed";
    const GOAL_REASON: &str = "the summary is in the transcript";
    const GOAL_LIMIT: u32 = 3;
    const PROMPT_FIRST: &str = "a prompt sent during a goal run must start before the goal goes on";
    const EVALUATED_AFTER_PROMPT: &str =
        "the goal must be evaluated at the end of the prompt's run";

    fn text_answer() -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: ANSWER.into(),
                }],
                ..Default::default()
            },
            stop_reason: Some(StopReason::EndTurn),
            ..Default::default()
        }
    }

    fn goal_met() -> StreamResponse {
        let mut response = text_answer();
        response.message.content = vec![ContentBlock::Text {
            text: serde_json::json!({"ok": true, "reason": GOAL_REASON, "impossible": false})
                .to_string(),
        }];
        response
    }

    async fn turn_events(events: &Receiver<Envelope>) -> Vec<AgentEvent> {
        let mut seen = Vec::new();
        loop {
            let envelope = events.recv_async().await.expect(EVENTS_CLOSED);
            match envelope.event {
                AgentEvent::Done { .. } if envelope.subagent.is_none() => return seen,
                AgentEvent::Error { message } => panic!("turn failed: {message}"),
                event => seen.push(event),
            }
        }
    }

    #[test]
    fn a_restored_goal_drives_the_next_run_and_is_saved_after_it_and_at_shutdown() {
        smol::block_on(async {
            let session = WorkflowSession::new(false);
            let (responses, rx) = flume::unbounded();
            *session.provider.responses.lock().unwrap() = Some(rx);
            let mut stored = SessionStore::open_in(
                session.state_dir.clone(),
                session_id(),
                &session.project.to_string_lossy(),
                MODEL_SPEC,
            )
            .unwrap();
            stored.goal.set(GOAL).unwrap();
            stored.goal.set_continuation_limit(GOAL_LIMIT);
            stored.save().unwrap();
            drop(stored);
            let handle = session.spawn(false).await;
            assert_eq!(handle.goal.active_condition().as_deref(), Some(GOAL));
            assert_eq!(handle.goal.continuation_limit(), GOAL_LIMIT);

            responses.send(text_answer()).unwrap();
            responses.send(goal_met()).unwrap();
            handle.input_tx.send(prompt(PROMPT)).unwrap();
            let events = turn_events(&handle.event_rx).await;
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, AgentEvent::GoalFinished { .. }))
            );
            handle.interrupt().await.unwrap();
            let meta = StoredSession::load(session_id(), &session.state_dir)
                .unwrap()
                .meta;
            assert!(meta.active_goal.is_none());
            let result = meta.goal_result.unwrap();
            assert_eq!(result.condition, GOAL);
            assert_eq!(result.reason, GOAL_REASON);
            assert_eq!(meta.goal_continuation_limit, Some(GOAL_LIMIT));

            handle.goal.set(OTHER_GOAL).unwrap();
            shutdown_interactive(handle).await;
            let meta = StoredSession::load(session_id(), &session.state_dir)
                .unwrap()
                .meta;
            assert_eq!(meta.active_goal.unwrap().condition, OTHER_GOAL);
            assert!(meta.goal_result.is_none());
            assert_eq!(meta.goal_continuation_limit, Some(GOAL_LIMIT));
        });
    }

    #[test]
    fn a_deferred_goal_checks_in_once_its_work_settles() {
        const CHILD: &str = "goal-child";
        smol::block_on(async {
            let session = WorkflowSession::new(false);
            let (responses, rx) = flume::unbounded();
            *session.provider.responses.lock().unwrap() = Some(rx);
            let handle = session.spawn_session(false, None, true).await;
            let tasks = handle.background.as_ref().unwrap();
            handle.goal.set(GOAL).unwrap();
            handle.input_tx.send(prompt(PROMPT)).unwrap();
            session.started.recv_async().await.unwrap();
            let _child = background_child(tasks, CHILD).await;
            responses.send(text_answer()).unwrap();
            let deferred = turn_events(&handle.event_rx).await;
            assert!(
                deferred
                    .iter()
                    .any(|event| matches!(event, AgentEvent::GoalDeferred { .. }))
            );

            tasks.stop().await.unwrap();
            session.started.recv_async().await.unwrap();
            let checkin = agent::goal_checkin_message(GOAL);
            assert!(
                session
                    .provider
                    .requests
                    .lock()
                    .unwrap()
                    .last()
                    .unwrap()
                    .iter()
                    .any(|message| message.first_text_content() == Some(checkin.as_str()))
            );
            responses.send(text_answer()).unwrap();
            responses.send(goal_met()).unwrap();
            let checked = turn_events(&handle.event_rx).await;
            assert!(
                checked
                    .iter()
                    .any(|event| matches!(event, AgentEvent::GoalFinished { .. }))
            );

            responses.send(text_answer()).unwrap();
            handle.input_tx.send(prompt(SECOND_PROMPT)).unwrap();
            wait_for_turn(&handle.event_rx).await;
            let runs: Vec<_> = handle.run_rx.try_iter().map(|run| run.automatic).collect();
            assert_eq!(runs, [false, true, false]);
            shutdown_interactive(handle).await;
        });
    }

    #[test]
    fn a_goal_deferral_belongs_to_the_run_that_deferred() {
        const DEFERRED_RUN: u64 = 3;
        let deferral = GoalDeferral::default();
        deferral.observe(&Envelope {
            event: AgentEvent::GoalDeferred {
                active_background_tasks: 1,
            },
            subagent: None,
            run_id: DEFERRED_RUN,
            task: None,
            workflow: None,
        });

        assert!(deferral.deferred(DEFERRED_RUN));
        assert!(!deferral.deferred(DEFERRED_RUN + 1));
    }

    #[test]
    fn a_prompt_sent_during_a_goal_run_starts_first_and_its_run_evaluates_the_goal() {
        smol::block_on(async {
            let session = WorkflowSession::new(false);
            let (responses, rx) = flume::unbounded();
            *session.provider.responses.lock().unwrap() = Some(rx);
            let handle = session.spawn_session(false, None, true).await;
            handle.goal.set(GOAL).unwrap();
            handle.input_tx.send(prompt(PROMPT)).unwrap();
            session.started.recv_async().await.unwrap();
            handle.input_tx.send(prompt(SECOND_PROMPT)).unwrap();

            responses.send(text_answer()).unwrap();
            session.started.recv_async().await.unwrap();
            let next = session.provider.requests.lock().unwrap()[1].clone();
            assert!(
                next.iter()
                    .any(|message| message.first_text_content() == Some(SECOND_PROMPT)),
                "{PROMPT_FIRST}"
            );
            responses.send(text_answer()).unwrap();
            responses.send(goal_met()).unwrap();
            let goal_run = turn_events(&handle.event_rx).await;
            let prompt_run = turn_events(&handle.event_rx).await;
            let runs: Vec<_> = handle.run_rx.try_iter().map(|run| run.automatic).collect();
            shutdown_interactive(handle).await;

            assert!(
                goal_run
                    .iter()
                    .any(|event| matches!(event, AgentEvent::GoalDeferred { .. }))
                    && !goal_run
                        .iter()
                        .any(|event| matches!(event, AgentEvent::GoalEvaluating { .. })),
                "{PROMPT_FIRST}"
            );
            assert_eq!(runs, [false, false], "{PROMPT_FIRST}");
            assert!(
                prompt_run
                    .iter()
                    .any(|event| matches!(event, AgentEvent::GoalFinished { .. })),
                "{EVALUATED_AFTER_PROMPT}"
            );
        });
    }

    #[test_case(false, PermissionMode::Ask; "idle_build_parent")]
    #[test_case(true, PermissionMode::Ask; "active_build_parent")]
    #[test_case(false, PermissionMode::Auto; "auto_idle_build_parent")]
    #[test_case(true, PermissionMode::Auto; "auto_active_build_parent")]
    fn acknowledged_plan_transition_drains_build_work_and_clamps_later_reports(
        active: bool,
        permission_mode: PermissionMode,
    ) {
        const CHILD: &str = "downgraded-build-child";
        const PLAN_CHILD: &str = "new-plan-child";
        const PLAN_FILE: &str = "plan.md";
        smol::block_on(async {
            let session = WorkflowSession::new(false);
            let (_responses, rx) = flume::unbounded();
            if active {
                *session.provider.responses.lock().unwrap() = Some(rx);
            }
            let handle = session.spawn_session(false, None, true).await;
            handle.input_tx.send(prompt(PROMPT)).unwrap();
            if active {
                session.started.recv_async().await.unwrap();
            } else {
                wait_for_turn(&handle.event_rx).await;
            }
            let tasks = handle.background.as_ref().unwrap();
            let (child, _, _) = background_child(tasks, CHILD).await;
            assert_eq!(tasks.active_count(), 1);
            let plan = AgentMode::Plan(session.project.join(PLAN_FILE));
            handle
                .set_mode(plan.clone(), permission_mode.clone())
                .await
                .unwrap();
            assert_eq!(handle.permissions.mode(), permission_mode);
            if active {
                wait_for_turn(&handle.event_rx).await;
            }
            assert_eq!(tasks.active_count(), 0);
            assert!(!tasks.has_pending());
            let answer = || StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: ANSWER.into(),
                    }],
                    ..Default::default()
                },
                stop_reason: Some(StopReason::EndTurn),
                ..Default::default()
            };
            assert!(child.send(answer()).is_err());
            *session.provider.responses.lock().unwrap() = None;
            handle.input_tx.send(prompt(SECOND_PROMPT)).unwrap();
            wait_for_turn(&handle.event_rx).await;
            let (child, _, _) = background_child_in(tasks, PLAN_CHILD, plan).await;
            child.send(answer()).unwrap();
            wait_for_turn(&handle.event_rx).await;
            {
                let requests = session.provider.requests.lock().unwrap();
                let latest_mode = requests
                    .last()
                    .unwrap()
                    .iter()
                    .rev()
                    .filter_map(Message::first_text_content)
                    .find(|text| {
                        text.contains(crate::prompt::PLAN_MODE_MARKER)
                            || text.contains(crate::prompt::BUILD_MODE_MARKER)
                    })
                    .unwrap();
                assert!(latest_mode.contains(crate::prompt::PLAN_MODE_MARKER));
            }
            assert!(handle.run_rx.try_iter().last().unwrap().automatic);
            shutdown_interactive(handle).await;
        });
    }

    #[test]
    fn acknowledged_plan_transition_drains_active_workflows() {
        const PLAN_FILE: &str = "plan.md";
        smol::block_on(async {
            let session = WorkflowSession::new(true);
            let handle = session.spawn_session(true, None, true).await;
            let workflow = handle.workflow.as_ref().unwrap();
            trust_and_start(workflow).await;
            session.started.recv_async().await.unwrap();
            assert_eq!(workflow.active_count(), 1);
            handle
                .set_mode(
                    AgentMode::Plan(session.project.join(PLAN_FILE)),
                    PermissionMode::Ask,
                )
                .await
                .unwrap();
            assert_eq!(workflow.active_count(), 0);
            assert_eq!(workflow.state().runs[0].status, RunStatus::Cancelled);
            shutdown_interactive(handle).await;
        });
    }

    #[cfg(unix)]
    #[test_case(true; "after_a_plan_run")]
    #[test_case(false; "without_a_plan")]
    fn build_runs_receive_the_plan_the_session_stored(planned: bool) {
        const WORKSPACE: &str = "stored-plan";
        const PLAN_CONTENT: &str = "# Stored plan";
        const PLAN_CALL: &str = "plan-read";
        const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
        smol::block_on(async {
            crate::tools::native::register(ToolRegistry::global(), FeatureFlags::all()).unwrap();
            let mut session = WorkflowSession::new(false);
            std::fs::set_permissions(
                session.state_dir.persistent_path(),
                Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
            )
            .unwrap();
            let (workspace, _) = crate::stored_session::tests::remote_workspace(WORKSPACE, "");
            let store = Arc::new(LocalDocumentStore::remote(
                session.state_dir.clone(),
                workspace.binding(),
            ));
            let reference = store
                .create_plan_with_content(
                    store.project_key(),
                    &session_id().to_string(),
                    PLAN_CONTENT,
                )
                .unwrap();
            session.local_documents = Some(store);
            let handle = session.spawn_in(false, Some(workspace)).await;
            if planned {
                handle
                    .set_mode(
                        AgentMode::RemotePlan(reference.clone()),
                        PermissionMode::Yolo,
                    )
                    .await
                    .unwrap();
                handle.input_tx.send(prompt(PROMPT)).unwrap();
                wait_for_turn(&handle.event_rx).await;
                handle
                    .set_mode(AgentMode::Build, PermissionMode::Yolo)
                    .await
                    .unwrap();
            }
            let (responses, rx) = flume::unbounded();
            *session.provider.responses.lock().unwrap() = Some(rx);
            for (content, stop_reason) in [
                (
                    ContentBlock::tool_use(
                        PLAN_CALL,
                        plan::NAME,
                        serde_json::json!({"action": "read"}),
                    ),
                    StopReason::ToolUse,
                ),
                (
                    ContentBlock::Text {
                        text: ANSWER.into(),
                    },
                    StopReason::EndTurn,
                ),
            ] {
                responses
                    .send(StreamResponse {
                        message: Message {
                            role: Role::Assistant,
                            content: vec![content],
                            ..Default::default()
                        },
                        stop_reason: Some(stop_reason),
                        ..Default::default()
                    })
                    .unwrap();
            }
            handle.input_tx.send(prompt(SECOND_PROMPT)).unwrap();
            let done = tool_done(&handle.event_rx, PLAN_CALL).await;
            assert_eq!(done.is_error, !planned, "{:?}", done.output);
            if planned {
                assert!(
                    done.output.as_text().contains(PLAN_CONTENT),
                    "{:?}",
                    done.output
                );
            }
            wait_for_turn(&handle.event_rx).await;
            shutdown_interactive(handle).await;
            let stored = open_stored_session(session_id(), &session.state_dir).unwrap();
            assert_eq!(
                stored.meta.plan_target,
                planned.then_some(StoredPlanTarget::PlanRef { reference })
            );
        });
    }

    #[test_case(true; "interrupt_then_explicit_resume")]
    #[test_case(false; "plan_mode_then_explicit_start")]
    fn explicit_workflow_controls_rearm_without_model_requests_rearming(resume: bool) {
        const PLAN_FILE: &str = "plan.md";
        smol::block_on(async {
            let session = WorkflowSession::new(true);
            let handle = session.spawn_session(true, None, true).await;
            let workflow = handle.workflow.as_ref().unwrap();
            let run = trust_and_start(workflow).await;
            session.started.recv_async().await.unwrap();
            let request = || {
                if resume {
                    WorkflowRequest::Resume {
                        run_id: run.run_id.clone(),
                        agent_budget: None,
                    }
                } else {
                    WorkflowRequest::Start(caudra_workflow::LaunchRequest {
                        name: WORKFLOW_NAME.into(),
                        args: serde_json::json!({}),
                        agent_budget: None,
                    })
                }
            };
            let superseded = handle.workflow_control(request());
            if resume {
                handle.interrupt().await.unwrap();
            } else {
                handle
                    .set_mode(
                        AgentMode::Plan(session.project.join(PLAN_FILE)),
                        PermissionMode::Ask,
                    )
                    .await
                    .unwrap();
            }
            assert_eq!(workflow.active_count(), 0);
            assert!(workflow.request(request()).await.is_err());
            assert!(
                handle
                    .background
                    .as_ref()
                    .unwrap()
                    .workflow_admission()
                    .await
                    .is_err()
            );
            assert!(
                matches!(superseded.await, Err(WorkflowError::Internal(message)) if message == STALE_WORKFLOW_CONTROL)
            );
            handle.workflow_control(request()).await.unwrap();
            session.started.recv_async().await.unwrap();
            assert_eq!(workflow.active_count(), 1);
            if !resume {
                let requests = session.provider.requests.lock().unwrap();
                assert!(
                    requests
                        .last()
                        .unwrap()
                        .iter()
                        .filter_map(Message::first_text_content)
                        .any(|text| text.contains(crate::prompt::TASK_PLAN_CONTRACT))
                );
            }
            shutdown_interactive(handle).await;
        });
    }

    #[test]
    fn waiting_explicit_workflow_start_does_not_block_interrupt_or_rearm_after_it() {
        smol::block_on(async {
            let session = WorkflowSession::new(true);
            let handle = session.spawn_session(true, None, true).await;
            handle.input_tx.send(prompt(PROMPT)).unwrap();
            session.started.recv_async().await.unwrap();
            let mut waiting = Box::pin(handle.workflow_control(WorkflowRequest::Start(
                caudra_workflow::LaunchRequest {
                    name: WORKFLOW_NAME.into(),
                    args: serde_json::json!({}),
                    agent_budget: None,
                },
            )));
            assert!(
                futures_lite::future::poll_once(&mut waiting)
                    .await
                    .is_none()
            );
            let waiting = smol::spawn(waiting);
            handle.interrupt().await.unwrap();
            assert!(
                matches!(waiting.await, Err(WorkflowError::Internal(message)) if message == STALE_WORKFLOW_CONTROL)
            );
            assert!(
                handle
                    .background
                    .as_ref()
                    .unwrap()
                    .workflow_admission()
                    .await
                    .is_err()
            );
            shutdown_interactive(handle).await;
        });
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
            handle
                .permissions
                .set_session_mode(Some(PermissionMode::Ask));
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
            {
                if snapshot.status == status {
                    return;
                }
                assert!(
                    !snapshot.status.is_terminal(),
                    "unexpected workflow outcome: {snapshot:?}"
                );
            }
        }
    }

    async fn wait_for_task_injection(events: &Receiver<Envelope>) -> (String, TaskEventOrigin) {
        loop {
            let envelope = events.recv_async().await.expect(EVENTS_CLOSED);
            match envelope.event {
                AgentEvent::Injected {
                    text,
                    task_event: Some(origin),
                    ..
                } => return (text, origin),
                AgentEvent::Done { .. } if envelope.subagent.is_none() => {
                    panic!("{TASK_INJECTION_MISSING}")
                }
                AgentEvent::Error { message } => panic!("turn failed: {message}"),
                _ => {}
            }
        }
    }

    #[cfg(unix)]
    async fn tool_done(events: &Receiver<Envelope>, call: &str) -> ToolDoneEvent {
        loop {
            let envelope = events.recv_async().await.expect(EVENTS_CLOSED);
            match envelope.event {
                AgentEvent::ToolDone(done) if done.id == call => return *done,
                AgentEvent::Error { message } => panic!("turn failed: {message}"),
                _ => {}
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
            plan: None,
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
            wait_for_run(&handle.event_rx, &run.run_id, RunStatus::Completed).await;
            session.started.try_recv().expect(AGENT_NEVER_STARTED);

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

            let read_only_claim = completion_messages(&workflow).await.unwrap();
            assert_eq!(
                read_only_claim[0].workflow_event,
                Some(WorkflowEventOrigin {
                    run_id: run.run_id.clone(),
                    revision: workflow.state().runs[0].revision
                })
            );
            assert_eq!(workflow.pending_completions(), 1);

            handle.input_tx.send(prompt(PROMPT)).unwrap();
            wait_for_turn(&handle.event_rx).await;
            handle.input_tx.send(prompt(SECOND_PROMPT)).unwrap();
            wait_for_turn(&handle.event_rx).await;

            let prompts = session.provider.user_prompts();
            let reported = format!("{COMPLETION_HEADING}\nReport: {ANSWER}");
            assert!(prompts.contains(&PROMPT.to_owned()), "got {prompts:?}");
            {
                let requests = session.provider.requests.lock().unwrap();
                assert!(
                    requests
                        .last()
                        .unwrap()
                        .iter()
                        .any(|message| message.is_observation()
                            && message.first_text_content() == Some(reported.as_str()))
                );
            }
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

    #[test_case(false; "acp_does_not_wake")]
    #[test_case(true; "sdk_wakes_without_task_events")]
    fn workflow_completion_after_parent_done_respects_background_capability(enabled: bool) {
        smol::block_on(async {
            let session = WorkflowSession::new(false);
            let handle = session.spawn_session(true, None, enabled).await;
            handle.input_tx.send(prompt(PROMPT)).unwrap();
            wait_for_turn(&handle.event_rx).await;
            if enabled {
                assert!(!handle.run_rx.recv_async().await.unwrap().automatic);
            }
            let workflow = handle.workflow.as_ref().unwrap();
            let run = trust_and_start(workflow).await;
            wait_for_run(&handle.event_rx, &run.run_id, RunStatus::Completed).await;
            if enabled {
                wait_for_turn(&handle.event_rx).await;
                let automatic = handle.run_rx.recv_async().await.unwrap();
                assert!(automatic.automatic);
                assert!(automatic.task_event_ids.is_empty());
                assert_eq!(automatic.workflow_events.len(), 1);
                assert_eq!(automatic.workflow_events[0].run_id, run.run_id);
            } else {
                assert_eq!(workflow.pending_completions(), 1);
                assert_eq!(session.provider.requests.lock().unwrap().len(), 2);
                handle.input_tx.send(prompt(SECOND_PROMPT)).unwrap();
                wait_for_turn(&handle.event_rx).await;
            }
            {
                let _turn = handle.mode_route.turn.lock().await;
                assert_eq!(workflow.pending_completions(), 0);
                let requests = session.provider.requests.lock().unwrap();
                let report = requests
                    .last()
                    .unwrap()
                    .iter()
                    .find(|message| {
                        message
                            .workflow_event
                            .as_ref()
                            .is_some_and(|origin| origin.run_id == run.run_id)
                    })
                    .unwrap();
                assert!(report.is_observation());
                assert!(report.task_event.is_none());
            }
            shutdown_interactive(handle).await;
        });
    }

    #[test]
    fn saved_workflow_receipt_prevents_reinjection_after_compaction_and_recovery() {
        const SUMMARY: &str = "Compacted workflow conversation";
        smol::block_on(async {
            let session = WorkflowSession::new(false);
            let handle = session.spawn(true).await;
            let workflow = handle.workflow.as_ref().unwrap();
            let run = trust_and_start(workflow).await;
            wait_for_run(&handle.event_rx, &run.run_id, RunStatus::Completed).await;
            let claims = completion_messages(workflow).await.unwrap();
            assert_eq!(claims.len(), 1);
            acknowledge_workflow_messages(workflow, &claims)
                .await
                .unwrap();
            assert_eq!(workflow.pending_completions(), 1);
            shutdown_interactive(handle).await;
            {
                let mut store = SessionStore::open_in(
                    session.state_dir.clone(),
                    session_id(),
                    &session.project.to_string_lossy(),
                    MODEL_SPEC,
                )
                .unwrap();
                let permissions = permission_manager();
                store
                    .record_turn(
                        &History::new(claims.clone()),
                        MODEL_SPEC.into(),
                        &permissions,
                    )
                    .unwrap();
                let compacted = History::new(vec![Message::observation(SUMMARY.into())]);
                store
                    .record_turn(&compacted, MODEL_SPEC.into(), &permissions)
                    .unwrap();
            }
            let handle = session.spawn(true).await;
            let workflow = handle.workflow.as_ref().unwrap();
            assert!(
                workflow
                    .received_completion(claims[0].workflow_event.clone().unwrap())
                    .await
                    .unwrap()
            );
            acknowledge_workflow_messages(workflow, &claims)
                .await
                .unwrap();
            assert!(completion_messages(workflow).await.unwrap().is_empty());
            assert_eq!(workflow.pending_completions(), 0);
            shutdown_interactive(handle).await;
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

    #[test_case(false, FeatureFlags::all(); "without_workflow_mode")]
    #[test_case(true, FeatureFlags::all().without(Feature::Workflows); "with_workflows_off")]
    fn a_session_without_workflows_attaches_no_runtime(
        workflow_mode: bool,
        features: FeatureFlags,
    ) {
        smol::block_on(async {
            let mut session = WorkflowSession::new(false);
            session.features = features;
            let handle = session.spawn(workflow_mode).await;
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

    const AUTOMATION_DESCRIPTION: &str = "Drives a headless session";
    const AUTOMATIONS_DIR: &str = "automations";
    const COURIER: &str = "courier";
    const GREETER: &str = "greeter";
    const WATCHER: &str = "watcher";
    const GOALS: &str = "goals";
    const GUIDE: &str = "guide";
    const AUTOMATION_MESSAGE: &str = "check CI";
    const GUIDANCE: &str = "check the parser first";
    /// A tool no session offers, so a call to it fails and the run asks the model again.
    const UNOFFERED_TOOL: &str = "unoffered_tool";
    const UNOFFERED_CALL: &str = "unoffered-call";
    const ARMED_TRIGGER: &str = r#"#{ kind: "armed" }"#;
    const IDLE_TRIGGER: &str = r#"#{ kind: "idle" }"#;
    const GOAL_FINISHED_TRIGGER: &str = r#"#{ kind: "goal_finished" }"#;
    const ARM_ALWAYS: &str = r#"arm: "always""#;
    const LOG_BODY: &str = r#"log("seen");"#;
    const FALLBACK_PLAN: &str = "fallback-plan.md";
    const AUTOMATION_START_MS: i64 = 1_790_000_000_000;
    const AUTOMATION_TITLE: &str = "Ship the release";
    const AUTOMATION_RUN: u64 = 0;
    /// USD.
    const TURN_COST: f64 = 0.25;
    const TURN_FAILURE: &str = "boom";
    const TURN_WINDOW: Duration = Duration::from_millis(ROLLING_WINDOW_MS.unsigned_abs());

    const NO_AUTOMATIONS: &str = "the session must start its automation runtime";
    const NO_CONTROLS: &str = "every save must keep the runtime's controls";
    const NO_CLAIM: &str = "a settled session must claim the queued message";
    const NOT_A_TRACE: &str = "a firing request must answer with its trace";
    const RUNTIME_CLOSED: &str = "the runtime must answer while the session runs";
    const FALLBACK_RUN: &str = "a run no prompt came before must run under the fallback mode";
    const CLAIM_CARRIED: &str = "the run must carry the message it claimed";
    const REGISTERED: &str = "the runtime must be registered exactly while it serves the session";
    const PAUSED_BY_INTERRUPT: &str = "an interrupt must pause the session's automations";
    const PROMPT_UNPAUSES: &str =
        "a prompt must clear the latch, and armed must fire with reason unpaused";
    const PAUSED_BY_SDK_REQUEST: &str =
        "an SDK pause request must hold the session's automations as the SDK's";
    const RUN_GOES_ON: &str = "an SDK pause request must let the run in progress reach its answer";
    const STOP_NEVER_PAUSES: &str = "neither a stop nor EOF may pause the session's automations";
    const NOTHING_AFTER_EOF: &str = "no automation run may start after EOF";
    const IDLE_ONCE: &str = "idle must fire once per busy period";
    const UNTIL_RUN_END: &str =
        "the session must stay busy until the forwarder hands over the run's end";
    const IDLE_CARRIES_RUN: &str = "idle must carry who started the period and its last run";
    const CLAIMED_GOAL_SET: &str = "a claimed goal must be set before its turn";
    const HELD_UNTIL_CLOCK: &str = "a delivery the turn rate holds must wait for its time";
    const REFUSAL_NOTICED: &str = "a refused claimed goal must reach the host as a notice";
    const GOAL_REPORTED: &str = "a goal that ends must fire goal_finished with its verdict";
    const ERROR_BACKS_OFF: &str =
        "an error ending a claimed run must back deliveries off without pausing";
    const SAVED_FIRST: &str = "a save request must save the session before its arming binds";
    const REARMS_ON_RESUME: &str = "a binding must re-arm on resume";
    const GUIDE_QUEUED: &str = "the guide item must wait in the outbox";
    const GUIDE_HELD: &str = "a guide item must not join a run while a prompt waits";
    const GUIDE_AFTER_PROMPT: &str = "a guide item must join the waiting prompt's run after it";
    const COMPLETION_CARRIED: &str =
        "a completion no wake can deliver must ride with the first automation run";

    /// A [`WorkflowSession`] with its workflow runtime, as an SDK session has one, that serves
    /// automations from its own user scripts on a fake clock, and falls back to plan mode for a
    /// run no prompt came before.
    struct AutomatedSession {
        session: WorkflowSession,
        config_dir: TempDir,
        clock: Arc<FakeClock>,
    }

    impl AutomatedSession {
        fn new() -> Self {
            Self {
                session: WorkflowSession::new(false),
                config_dir: TempDir::new().unwrap(),
                clock: FakeClock::new(AUTOMATION_START_MS),
            }
        }

        fn script(&self, name: &str, triggers: &[&str], fields: &[&str], body: &str) {
            write_script(
                &self.config_dir.path().join(AUTOMATIONS_DIR),
                name,
                triggers,
                fields,
                body,
            );
        }

        async fn spawn(&self, config: AutomationsConfig) -> InteractiveHandle {
            let fallback = AgentMode::Plan(self.session.project.join(FALLBACK_PLAN));
            let automations = AutomationParams {
                mode: Arc::new(move || fallback.clone()),
                fast: false,
                config,
                clock: Arc::clone(&self.clock) as Arc<dyn Clock>,
                http: None,
                user_config_dir: Some(self.config_dir.path().to_path_buf()),
            };
            self.session
                .spawn_with(true, None, true, Some(automations))
                .await
        }
    }

    /// `<name>.rhai` in `dir`, a user script, which needs no trust.
    fn write_script(dir: &Path, name: &str, triggers: &[&str], fields: &[&str], body: &str) {
        let fields: String = fields.iter().map(|field| format!(", {field}")).collect();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(format!("{name}.{SCRIPT_EXTENSION}")),
            format!(
                "let meta = #{{ name: \"{name}\", description: \"{AUTOMATION_DESCRIPTION}\", triggers: [{}]{fields} }};\n{body}",
                triggers.join(", ")
            ),
        )
        .unwrap();
    }

    fn message_body() -> String {
        format!(r#"message("{AUTOMATION_MESSAGE}");"#)
    }

    fn goal_body() -> String {
        format!(r#"set_goal("{GOAL}", #{{ replace: false }});"#)
    }

    fn guide_body() -> String {
        format!(r#"message("{GUIDANCE}", #{{ delivery: "guide" }});"#)
    }

    fn unoffered_call() -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    UNOFFERED_CALL,
                    UNOFFERED_TOOL,
                    serde_json::json!({}),
                )],
                ..Default::default()
            },
            stop_reason: Some(StopReason::ToolUse),
            ..Default::default()
        }
    }

    fn position_of(request: &[Message], text: &str) -> Option<usize> {
        request.iter().position(|message| {
            message
                .first_text_content()
                .is_some_and(|content| content.contains(text))
        })
    }

    /// Ends the session as the SDK does at stdin EOF.
    async fn eof(handle: InteractiveHandle) {
        let InteractiveHandle {
            input_tx,
            cancel_tx,
            task,
            ..
        } = handle;
        drop(input_tx);
        let _ = cancel_tx.try_send(());
        task.await;
    }

    /// Returns once the runtime has handled everything signalled before.
    async fn barrier(handle: &AutomationHandle) {
        handle
            .request(AutomationRequest::List)
            .await
            .expect(RUNTIME_CLOSED);
    }

    /// The next firing of `name` that ended as `status`.
    async fn firing(
        events: &Receiver<AutomationEvent>,
        name: &str,
        status: FiringStatus,
    ) -> FiringSummary {
        loop {
            if let AutomationEvent::Firing { firing, .. } =
                events.recv_async().await.expect(EVENTS_CLOSED)
                && firing.automation == name
                && firing.status == status
            {
                return *firing;
            }
        }
    }

    /// The next notice handed to the host: the automation it names, the firing it came from,
    /// and its text.
    async fn notice(events: &Receiver<AutomationEvent>) -> (String, Option<String>, String) {
        loop {
            if let AutomationEvent::Notice {
                automation,
                fire_id,
                text,
            } = events.recv_async().await.expect(EVENTS_CLOSED)
            {
                return (automation, fire_id, text);
            }
        }
    }

    async fn fired_on(handle: &AutomationHandle, fire_id: &str) -> EventDetail {
        let trace = AutomationRequest::Firing {
            fire_id: fire_id.to_owned(),
        };
        match handle.request(trace).await {
            Ok(AutomationResponse::Firing(detail)) => {
                serde_json::from_value::<FiredEvent>(detail.event)
                    .unwrap()
                    .detail
            }
            other => panic!("{NOT_A_TRACE}: {other:?}"),
        }
    }

    /// The store of the fixture's saved session.
    fn fixture_store(fixture: &AutomationFixture) -> SessionStore {
        SessionStore::open_in(
            fixture.state_dir().clone(),
            fixture.session_id(),
            &fixture.project().to_string_lossy(),
            MODEL_SPEC,
        )
        .unwrap()
    }

    /// `store`'s runtime as the SDK serves it, the events it hands the host, and the store that
    /// keeps its controls.
    async fn served(
        fixture: &AutomationFixture,
        mut store: SessionStore,
    ) -> (
        AutomationRuntime,
        Arc<SessionAutomations>,
        Receiver<AutomationEvent>,
        Arc<Mutex<Option<SessionStore>>>,
    ) {
        let pending = PendingAutomations {
            deps: AutomationDeps {
                frontend: Frontend::Sdk,
                ..fixture.deps(store.session.id, &[])
            },
            mode: Arc::new(|| AgentMode::Build),
            fast: false,
        };
        let (runtime, automations, host) =
            pending.spawn(None, &mut store).await.expect(NO_AUTOMATIONS);
        (
            runtime,
            Arc::new(automations),
            host,
            Arc::new(Mutex::new(Some(store))),
        )
    }

    fn main_run(event: AgentEvent) -> Envelope {
        Envelope {
            event,
            subagent: None,
            run_id: AUTOMATION_RUN,
            task: None,
            workflow: None,
        }
    }

    fn answered(cost: f64) -> AgentEvent {
        AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
            message: text_answer().message,
            usage: TokenUsage::default(),
            model: MODEL_SPEC.into(),
            provider: String::new(),
            purpose: LedgerPurpose::Chat,
            cost: Some(cost),
            billing: Billing::Api,
            context_size: None,
            context_window: 0,
        }))
    }

    fn ended() -> AgentEvent {
        AgentEvent::Done {
            usage: TokenUsage::default(),
            num_turns: 1,
            reason: DoneReason::EndTurn,
        }
    }

    fn arm(name: &str) -> AutomationRequest {
        AutomationRequest::Arm {
            name: name.to_owned(),
            args: None,
            origin: ArmOrigin::Manual,
        }
    }

    #[test]
    fn an_armed_message_starts_the_first_run_under_the_fallback_mode() {
        smol::block_on(async {
            let session = AutomatedSession::new();
            session.script(COURIER, &[ARMED_TRIGGER], &[ARM_ALWAYS], &message_body());
            let handle = session.spawn(AutomationsConfig::default()).await;
            assert!(
                AutomationHandle::lookup(session_id()).is_some(),
                "{REGISTERED}"
            );

            let run = handle.run_rx.recv_async().await.unwrap();
            wait_for_turn(&handle.event_rx).await;
            eof(handle).await;

            assert!(run.automatic);
            let claimed: Vec<_> = run
                .automation_events
                .iter()
                .map(|origin| origin.automation.as_str())
                .collect();
            assert_eq!(claimed, [COURIER], "{CLAIM_CARRIED}");
            let request = session.session.provider.requests.lock().unwrap()[0].clone();
            assert!(
                request
                    .iter()
                    .any(|message| message.automation_event.is_some()
                        && message
                            .first_text_content()
                            .is_some_and(|text| text.contains(AUTOMATION_MESSAGE))),
                "{CLAIM_CARRIED}"
            );
            let meta = StoredSession::load(session_id(), &session.session.state_dir)
                .unwrap()
                .meta;
            assert_eq!(meta.mode, Some(StoredMode::Plan), "{FALLBACK_RUN}");
            assert!(
                AutomationHandle::lookup(session_id()).is_none(),
                "{REGISTERED}"
            );
        });
    }

    #[test_case(false; "idle")]
    #[test_case(true; "mid_run")]
    fn an_interrupt_pauses_automations_until_the_next_prompt(mid_run: bool) {
        smol::block_on(async {
            let session = AutomatedSession::new();
            session.script(GREETER, &[ARMED_TRIGGER], &[ARM_ALWAYS], LOG_BODY);
            let (responses, rx) = flume::unbounded();
            *session.session.provider.responses.lock().unwrap() = Some(rx);
            let handle = session.spawn(AutomationsConfig::default()).await;
            let automations = handle.automations.clone().expect(NO_AUTOMATIONS);
            firing(&handle.automation_events, GREETER, FiringStatus::Completed).await;
            if mid_run {
                handle.input_tx.send(prompt(PROMPT)).unwrap();
                session.session.started.recv_async().await.unwrap();
            }

            handle.interrupt().await.unwrap();
            barrier(&automations).await;
            let latch = automations.state().session.controls.pause.clone();
            assert_eq!(
                latch.map(|latch| latch.reason).as_deref(),
                Some(PAUSED_BY_SDK),
                "{PAUSED_BY_INTERRUPT}"
            );

            handle.input_tx.send(prompt(SECOND_PROMPT)).unwrap();
            session.session.started.recv_async().await.unwrap();
            barrier(&automations).await;
            assert!(
                automations.state().session.controls.pause.is_none(),
                "{PROMPT_UNPAUSES}"
            );
            responses.send(text_answer()).unwrap();
            let unpaused =
                firing(&handle.automation_events, GREETER, FiringStatus::Completed).await;
            assert_eq!(
                fired_on(&automations, &unpaused.fire_id).await,
                EventDetail::Armed {
                    reason: ArmedReason::Unpaused
                },
                "{PROMPT_UNPAUSES}"
            );
            eof(handle).await;
        });
    }

    /// What `automation_pause` asks of the runtime: unlike an interrupt, it holds the
    /// automations and leaves the run in progress alone.
    #[test]
    fn an_sdk_pause_request_holds_automations_while_the_run_goes_on() {
        smol::block_on(async {
            let session = AutomatedSession::new();
            session.script(GREETER, &[ARMED_TRIGGER], &[ARM_ALWAYS], LOG_BODY);
            let (responses, rx) = flume::unbounded();
            *session.session.provider.responses.lock().unwrap() = Some(rx);
            let handle = session.spawn(AutomationsConfig::default()).await;
            let automations = handle.automations.clone().expect(NO_AUTOMATIONS);
            firing(&handle.automation_events, GREETER, FiringStatus::Completed).await;
            handle.input_tx.send(prompt(PROMPT)).unwrap();
            session.session.started.recv_async().await.unwrap();

            let paused = automations
                .request(AutomationRequest::Pause {
                    by: PauseSource::Sdk,
                })
                .await;
            responses.send(text_answer()).unwrap();
            let run = turn_events(&handle.event_rx).await;
            eof(handle).await;

            let Ok(AutomationResponse::Controls(view)) = paused else {
                panic!("{PAUSED_BY_SDK_REQUEST}: {paused:?}");
            };
            assert_eq!(
                view.controls.pause.map(|latch| latch.source),
                Some(PauseSource::Sdk),
                "{PAUSED_BY_SDK_REQUEST}"
            );
            assert!(
                run.iter()
                    .any(|event| matches!(event, AgentEvent::TurnComplete(_))),
                "{RUN_GOES_ON}"
            );
        });
    }

    #[test]
    fn neither_a_stop_nor_eof_pauses_automations() {
        smol::block_on(async {
            let session = AutomatedSession::new();
            session.script(GREETER, &[ARMED_TRIGGER], &[ARM_ALWAYS], LOG_BODY);
            let handle = session.spawn(AutomationsConfig::default()).await;
            firing(&handle.automation_events, GREETER, FiringStatus::Completed).await;

            handle.cancel_tx.send_async(()).await.unwrap();
            // A bounded channel of one takes the second stop once the loop took the first.
            handle.cancel_tx.send_async(()).await.unwrap();
            eof(handle).await;

            let controls = StoredSession::load(session_id(), &session.session.state_dir)
                .unwrap()
                .meta
                .automations
                .expect(NO_CONTROLS);
            assert_eq!(controls.pause, None, "{STOP_NEVER_PAUSES}");
            assert_eq!(controls.armed, [GREETER], "{NO_CONTROLS}");
        });
    }

    #[test]
    fn no_automation_run_starts_after_eof() {
        smol::block_on(async {
            let session = AutomatedSession::new();
            session.script(COURIER, &[ARMED_TRIGGER], &[], &message_body());
            let (_responses, rx) = flume::unbounded();
            *session.session.provider.responses.lock().unwrap() = Some(rx);
            let handle = session.spawn(AutomationsConfig::default()).await;
            let automations = handle.automations.clone().expect(NO_AUTOMATIONS);
            handle.input_tx.send(prompt(PROMPT)).unwrap();
            session.session.started.recv_async().await.unwrap();
            automations.request(arm(COURIER)).await.unwrap();
            firing(&handle.automation_events, COURIER, FiringStatus::Completed).await;

            eof(handle).await;

            assert_eq!(
                session.session.provider.requests.lock().unwrap().len(),
                1,
                "{NOTHING_AFTER_EOF}"
            );
        });
    }

    #[test]
    fn a_guide_item_waits_for_a_queued_prompt_and_joins_its_run() {
        smol::block_on(async {
            let session = AutomatedSession::new();
            session.script(GUIDE, &[ARMED_TRIGGER], &[], &guide_body());
            let (responses, rx) = flume::unbounded();
            *session.session.provider.responses.lock().unwrap() = Some(rx);
            let handle = session.spawn(AutomationsConfig::default()).await;
            let automations = handle.automations.clone().expect(NO_AUTOMATIONS);
            handle.input_tx.send(prompt(PROMPT)).unwrap();
            session.session.started.recv_async().await.unwrap();
            automations.request(arm(GUIDE)).await.unwrap();
            firing(&handle.automation_events, GUIDE, FiringStatus::Completed).await;
            barrier(&automations).await;
            assert_eq!(automations.state().outbox.len(), 1, "{GUIDE_QUEUED}");
            handle.input_tx.send(prompt(SECOND_PROMPT)).unwrap();

            responses.send(unoffered_call()).unwrap();
            session.session.started.recv_async().await.unwrap();
            responses.send(text_answer()).unwrap();
            session.session.started.recv_async().await.unwrap();
            responses.send(text_answer()).unwrap();
            wait_for_turn(&handle.event_rx).await;
            wait_for_turn(&handle.event_rx).await;
            eof(handle).await;

            let requests = session.session.provider.requests.lock().unwrap().clone();
            assert_eq!(position_of(&requests[1], GUIDANCE), None, "{GUIDE_HELD}");
            let prompted = position_of(&requests[2], SECOND_PROMPT).expect(GUIDE_AFTER_PROMPT);
            let guided = position_of(&requests[2], GUIDANCE).expect(GUIDE_AFTER_PROMPT);
            assert!(prompted < guided, "{GUIDE_AFTER_PROMPT}");
        });
    }

    #[test]
    fn a_completion_no_wake_can_deliver_rides_with_the_first_automation_run() {
        smol::block_on(async {
            let session = AutomatedSession::new();
            session.script(COURIER, &[ARMED_TRIGGER], &[], &message_body());
            let handle = session.spawn(AutomationsConfig::default()).await;
            let automations = handle.automations.clone().expect(NO_AUTOMATIONS);
            let workflow = handle.workflow.clone().expect(NO_RUNTIME);
            let finished = trust_and_start(&workflow).await;
            wait_for_run(&handle.event_rx, &finished.run_id, RunStatus::Completed).await;

            automations.request(arm(COURIER)).await.unwrap();
            let run = handle.run_rx.recv_async().await.unwrap();
            wait_for_turn(&handle.event_rx).await;
            eof(handle).await;

            let claimed: Vec<_> = run
                .automation_events
                .iter()
                .map(|origin| origin.automation.as_str())
                .collect();
            let completed: Vec<_> = run
                .workflow_events
                .iter()
                .map(|origin| origin.run_id.as_str())
                .collect();
            assert_eq!(
                (run.automatic, claimed, completed),
                (true, vec![COURIER], vec![finished.run_id.as_str()]),
                "{COMPLETION_CARRIED}"
            );
        });
    }

    #[test_case(false; "a_prompt")]
    #[test_case(true; "an_automation")]
    fn idle_names_who_started_the_busy_period(automated: bool) {
        smol::block_on(async {
            let session = AutomatedSession::new();
            session.script(WATCHER, &[IDLE_TRIGGER], &[ARM_ALWAYS], LOG_BODY);
            if automated {
                session.script(COURIER, &[ARMED_TRIGGER], &[ARM_ALWAYS], &message_body());
            }
            let handle = session.spawn(AutomationsConfig::default()).await;
            let automations = handle.automations.clone().expect(NO_AUTOMATIONS);
            let started_by = if automated {
                let courier =
                    firing(&handle.automation_events, COURIER, FiringStatus::Completed).await;
                StartedBy::Automation {
                    automation: COURIER.into(),
                    fire_id: courier.fire_id,
                }
            } else {
                handle.input_tx.send(prompt(PROMPT)).unwrap();
                StartedBy::User
            };

            let idle = firing(&handle.automation_events, WATCHER, FiringStatus::Completed).await;
            let EventDetail::Idle(detail) = fired_on(&automations, &idle.fire_id).await else {
                panic!("{IDLE_CARRIES_RUN}");
            };
            eof(handle).await;

            assert_eq!(
                (detail.started_by, detail.runs, detail.last_response),
                (started_by, 1, Untrusted::text(ANSWER)),
                "{IDLE_CARRIES_RUN}"
            );
            let idles = automations
                .state()
                .recent
                .iter()
                .filter(|firing| firing.automation == WATCHER)
                .count();
            assert_eq!(idles, 1, "{IDLE_ONCE}");
        });
    }

    #[test]
    fn a_claimed_goal_is_set_before_its_turn() {
        smol::block_on(async {
            let session = AutomatedSession::new();
            session.script(GOALS, &[ARMED_TRIGGER], &[ARM_ALWAYS], &goal_body());
            let (_responses, rx) = flume::unbounded();
            *session.session.provider.responses.lock().unwrap() = Some(rx);
            let handle = session.spawn(AutomationsConfig::default()).await;

            session.session.started.recv_async().await.unwrap();
            let active = handle.goal.active_condition();
            eof(handle).await;

            assert_eq!(active.as_deref(), Some(GOAL), "{CLAIMED_GOAL_SET}");
        });
    }

    #[test]
    fn a_goal_the_turn_rate_holds_waits_for_the_clock_and_a_refusal_reaches_the_host() {
        smol::block_on(async {
            let session = AutomatedSession::new();
            session.script(COURIER, &[ARMED_TRIGGER], &[ARM_ALWAYS], &message_body());
            session.script(GOALS, &[IDLE_TRIGGER], &[ARM_ALWAYS], &goal_body());
            let handle = session
                .spawn(AutomationsConfig {
                    turns_per_hour: 1,
                    ..AutomationsConfig::default()
                })
                .await;
            let automations = handle.automations.clone().expect(NO_AUTOMATIONS);
            let claimed = firing(&handle.automation_events, GOALS, FiringStatus::Completed).await;
            barrier(&automations).await;
            let waits: Vec<_> = automations
                .state()
                .outbox
                .iter()
                .map(|item| item.wait)
                .collect();
            assert!(
                matches!(waits[..], [Some(WaitReason::TurnRateFull { .. })]),
                "{HELD_UNTIL_CLOCK}"
            );

            handle.goal.set(OTHER_GOAL).unwrap();
            session.clock.advance(TURN_WINDOW);
            let refused = notice(&handle.automation_events).await;
            eof(handle).await;

            assert_eq!(
                refused,
                (
                    GOALS.to_owned(),
                    Some(claimed.fire_id),
                    GOAL_ACTIVE.to_owned()
                ),
                "{REFUSAL_NOTICED}"
            );
        });
    }

    #[test]
    fn the_session_settles_only_once_the_forwarder_hands_over_the_run_end() {
        smol::block_on(async {
            let fixture = AutomationFixture::default();
            write_script(
                &fixture.user_scripts(),
                WATCHER,
                &[IDLE_TRIGGER],
                &[ARM_ALWAYS],
                LOG_BODY,
            );
            let (runtime, automations, ..) = served(&fixture, fixture_store(&fixture)).await;
            let events = automations.handle.events();
            let goal = GoalHandle::default();

            automations.run_started(AUTOMATION_RUN, StartedBy::User, AUTOMATION_TITLE, &goal);
            automations.forwarded(&main_run(answered(TURN_COST)), &goal);
            assert_eq!(
                automations.blockers(false, Vec::new()),
                [SettleBlocker::Busy],
                "{UNTIL_RUN_END}"
            );
            automations.forwarded(&main_run(ended()), &goal);
            let blockers = automations.blockers(false, Vec::new());
            automations.observe(AUTOMATION_TITLE, &goal, blockers);
            let idle = firing(&events, WATCHER, FiringStatus::Completed).await;
            let EventDetail::Idle(detail) = fired_on(&automations.handle, &idle.fire_id).await
            else {
                panic!("{IDLE_CARRIES_RUN}");
            };
            stop_runtime(runtime).await;

            assert_eq!(
                (detail.last_response, detail.cost),
                (Untrusted::text(ANSWER), Some(TURN_COST)),
                "{IDLE_CARRIES_RUN}"
            );
        });
    }

    fn met_goal() -> AgentEvent {
        AgentEvent::GoalFinished {
            result: GoalResult {
                condition: GOAL.into(),
                verdict: GoalVerdict::Met,
                reason: GOAL_REASON.into(),
                evaluations: 1,
                duration: Duration::ZERO,
                usage: TokenUsage::default(),
                cost: None,
                subscription_cost: None,
            },
        }
    }

    fn cleared_goal() -> AgentEvent {
        AgentEvent::GoalClearedAfterError {
            condition: GOAL.into(),
            message: TURN_FAILURE.into(),
        }
    }

    #[test_case(met_goal(), FinishedVerdict::Met; "met")]
    #[test_case(cleared_goal(), FinishedVerdict::Cleared; "cleared_after_an_error")]
    fn a_goal_that_ends_fires_goal_finished(event: AgentEvent, verdict: FinishedVerdict) {
        smol::block_on(async {
            let fixture = AutomationFixture::default();
            write_script(
                &fixture.user_scripts(),
                WATCHER,
                &[GOAL_FINISHED_TRIGGER],
                &[ARM_ALWAYS],
                LOG_BODY,
            );
            let (runtime, automations, ..) = served(&fixture, fixture_store(&fixture)).await;
            let events = automations.handle.events();

            automations.forwarded(&main_run(event), &GoalHandle::default());
            let finished = firing(&events, WATCHER, FiringStatus::Completed).await;
            let EventDetail::GoalFinished(detail) =
                fired_on(&automations.handle, &finished.fire_id).await
            else {
                panic!("{GOAL_REPORTED}");
            };
            stop_runtime(runtime).await;

            assert_eq!(
                (detail.verdict, detail.condition.as_str()),
                (verdict, GOAL),
                "{GOAL_REPORTED}"
            );
        });
    }

    #[test]
    fn an_error_ending_a_claimed_run_backs_deliveries_off_without_pausing() {
        smol::block_on(async {
            let fixture = AutomationFixture::default();
            write_script(
                &fixture.user_scripts(),
                COURIER,
                &[ARMED_TRIGGER],
                &[ARM_ALWAYS],
                &message_body(),
            );
            let (runtime, automations, ..) = served(&fixture, fixture_store(&fixture)).await;
            firing(
                &automations.handle.events(),
                COURIER,
                FiringStatus::Completed,
            )
            .await;
            let goal = GoalHandle::default();

            let claim = automations.claim().await.expect(NO_CLAIM);
            let started_by = StartedBy::Automation {
                automation: claim.automation,
                fire_id: claim.fire_id,
            };
            automations.run_started(AUTOMATION_RUN, started_by, AUTOMATION_TITLE, &goal);
            automations.forwarded(
                &main_run(AgentEvent::Error {
                    message: TURN_FAILURE.into(),
                }),
                &goal,
            );
            barrier(&automations.handle).await;
            let controls = automations.handle.state().session.controls.clone();
            stop_runtime(runtime).await;

            assert_eq!(
                (controls.delivery_backoff.errors, controls.pause),
                (1, None),
                "{ERROR_BACKS_OFF}"
            );
        });
    }

    #[test]
    fn a_save_request_saves_the_session_before_its_arming_binds_and_a_resume_rearms_it() {
        smol::block_on(async {
            let fixture = AutomationFixture::default();
            write_script(
                &fixture.user_scripts(),
                GREETER,
                &[ARMED_TRIGGER],
                &[],
                LOG_BODY,
            );
            let unsaved = StoredSession::new(MODEL_SPEC, &fixture.project().to_string_lossy());
            let session = unsaved.id;
            let lease = Arc::new(SessionLease::acquire(fixture.state_dir(), session).unwrap());
            let store =
                SessionStore::from_session(fixture.state_dir().clone(), unsaved, true, lease);
            let (runtime, automations, host, store) = served(&fixture, store).await;
            let (stop, stopped) = flume::bounded(1);
            let (wake, _woken) = flume::bounded(1);
            let (errors, _) = flume::unbounded();
            let events = smol::spawn(serve_automation_events(
                Arc::clone(&automations),
                stopped,
                Arc::clone(&store),
                wake,
                errors,
            ));

            automations.handle.request(arm(GREETER)).await.unwrap();
            firing(&host, GREETER, FiringStatus::Completed).await;
            assert!(
                StoredSession::load(session, fixture.state_dir()).is_ok(),
                "{SAVED_FIRST}"
            );
            stop_runtime(runtime).await;
            drop(stop);
            events.await;
            let mut closing = store.lock().await.take().unwrap();
            closing.save().unwrap();
            drop(closing);

            let controls = StoredSession::load(session, fixture.state_dir())
                .unwrap()
                .meta
                .automations;
            let resumed = AutomationRuntime::spawn(AutomationDeps {
                frontend: Frontend::Sdk,
                controls,
                ..fixture.deps(session, &[])
            })
            .await
            .expect(NO_AUTOMATIONS);
            let rearmed =
                firing(&resumed.handle().events(), GREETER, FiringStatus::Completed).await;
            let reason = fired_on(&resumed.handle(), &rearmed.fire_id).await;
            stop_runtime(resumed).await;

            assert_eq!(
                reason,
                EventDetail::Armed {
                    reason: ArmedReason::Resume
                },
                "{REARMS_ON_RESUME}"
            );
        });
    }
}
