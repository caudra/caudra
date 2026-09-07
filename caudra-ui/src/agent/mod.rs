mod agent_loop;
mod cancel_map;
mod command_router;
pub(crate) mod shared_queue;

use std::collections::HashSet;
use std::mem;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use caudra_agent::context::{ContextKey, ContextStore};
use caudra_agent::permissions::PermissionManager;
use caudra_agent::prompt::profile::PromptProfileCatalog;
use caudra_agent::prompt::profile::SystemPromptProfile;
use caudra_agent::{
    AgentConfig, CancelMap, CancelToken, Envelope, HistorySnapshot, McpCommand, McpConfigErrors,
    McpHandle, McpSnapshotReader, SessionMailbox, SharedHistory, SubagentHistoryStore,
    ToolOutputLines,
};
use caudra_config::ModelPolicy;
use caudra_lua::EventHandle;
use caudra_storage::id::SessionRef;
use caudra_storage::sessions::SessionLease;

use self::cancel_map::new_run_cancel_map;
use caudra_providers::provider::Provider;
use caudra_providers::{HistoryItem, Message, Model, RequestOptions, project_messages};
use serde_json::Value;
use tracing::{info, warn};

use crate::app::App;

use self::agent_loop::AgentLoop;
use self::command_router::spawn_command_router;
pub(crate) use self::shared_queue::{QueueSender, QueuedMessage};

pub(crate) struct ModelSlot {
    pub(crate) model: Model,
    pub(crate) provider: Arc<dyn Provider>,
}

/// Every input the provider hashes into its cache prefix, published as one
/// unit. Swapping system and tools separately could pair a stale system with
/// fresh tools, which is exactly the mismatch that costs `/btw` a cache hit.
#[derive(Default)]
pub(crate) struct BtwPrompt {
    pub(crate) system: String,
    pub(crate) tools: Value,
    pub(crate) opts: RequestOptions,
}

pub(crate) type SharedBtwPrompt = Arc<ArcSwap<BtwPrompt>>;

pub(crate) enum AgentCommand {
    Cancel { run_id: u64 },
    CancelAll,
    CancelSubagent { tool_use_id: String },
}

/// Input channels (`cmd_tx`, `answer_tx`, `queue`) are per-agent, so an old
/// loop can never steal new input. The output channel (`agent_tx`/`agent_rx`)
/// is per-tab: `respawn` reuses it, so anyone still holding a sender (a Lua
/// restore reply, a click, an old agent winding down) can always deliver.
/// Stale events are filtered by `run_id`, not by killing the channel.
pub(crate) struct AgentHandles {
    pub(crate) cmd_tx: flume::Sender<AgentCommand>,
    pub(crate) agent_rx: flume::Receiver<Envelope>,
    pub(crate) agent_tx: flume::Sender<Envelope>,
    pub(crate) answer_tx: flume::Sender<String>,
    pub(crate) history: SharedHistory,
    pub(crate) btw_prompt: SharedBtwPrompt,
    pub(crate) context_store: ContextStore,
    pub(crate) mcp_handle: Option<McpHandle>,
    pub(crate) mcp_config_errors: McpConfigErrors,
    pub(crate) queue: QueueSender,
    pub(crate) goal: caudra_agent::GoalHandle,
    subagent_cancels: Arc<CancelMap<String>>,
    pub(crate) timeouts: caudra_providers::Timeouts,
    model_policy: Arc<ModelPolicy>,
    prompt_profiles: Arc<PromptProfileCatalog>,
    mailbox: Option<SessionMailbox>,
    task: smol::Task<()>,
}

impl AgentHandles {
    /// MCP is shared across sessions and agent respawns; the event loop starts it
    /// once and shuts it down at exit. Only the agent loop task lives here.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn(
        model_slot: &Arc<ArcSwap<ModelSlot>>,
        initial_history: Vec<HistoryItem>,
        config: AgentConfig,
        tool_output_lines: ToolOutputLines,
        permissions: &Arc<PermissionManager>,
        session_id: Option<SessionRef>,
        session_lease: Option<Arc<SessionLease>>,
        timeouts: caudra_providers::Timeouts,
        lua_handle: EventHandle,
        mcp_handle: Option<McpHandle>,
        mcp_config_errors: McpConfigErrors,
        model_policy: Arc<ModelPolicy>,
        goal: caudra_agent::GoalHandle,
        subagent_history: SubagentHistoryStore,
        system_prompt_profile: Option<Arc<SystemPromptProfile>>,
        prompt_profiles: Arc<PromptProfileCatalog>,
    ) -> Self {
        spawn_agent_internal(
            flume::unbounded(),
            model_slot,
            initial_history,
            config,
            tool_output_lines,
            permissions,
            mcp_handle,
            mcp_config_errors,
            session_id,
            session_lease,
            timeouts,
            lua_handle,
            model_policy,
            goal,
            subagent_history,
            system_prompt_profile,
            Arc::clone(&prompt_profiles),
        )
    }

    pub(crate) fn mcp_reader(&self) -> McpSnapshotReader {
        self.mcp_handle
            .as_ref()
            .map(McpHandle::reader)
            .unwrap_or_else(McpSnapshotReader::empty)
    }

    pub(crate) fn apply_to_app(&self, app: &mut App) {
        app.answer_tx = Some(self.answer_tx.clone());
        app.cmd_tx = Some(self.cmd_tx.clone());
        app.shared_history = Some(Arc::clone(&self.history));
        app.btw_prompt = Some(Arc::clone(&self.btw_prompt));
        app.context_store = Some(self.context_store.clone());
        app.queue.set_shared(self.queue.clone());
        if self.goal.status().is_none() {
            match app.state.goal.status() {
                Some(caudra_agent::GoalStatus::Active(goal)) => {
                    let _ = self.goal.set(&goal.condition);
                }
                Some(caudra_agent::GoalStatus::Finished(result)) => {
                    self.goal.restore_finished(result);
                }
                None => {}
            }
        }
        app.state.goal = self.goal.clone();
        let restore_tx =
            caudra_agent::EventSender::new(self.agent_tx.clone(), crate::app::RESTORE_RUN_ID);
        app.restore_event_tx = Some(restore_tx.clone());
        for chat in &mut app.chats {
            chat.set_restore_channel(Some(restore_tx.clone()));
        }
    }

    pub(crate) fn cancel(self) {
        let _ = self.cmd_tx.try_send(AgentCommand::CancelAll);
    }

    pub(crate) fn send_mcp(&self, cmd: McpCommand) {
        if let Some(ref h) = self.mcp_handle {
            h.send(cmd);
        }
    }

    pub(crate) fn claim_mailbox_wake(&self) -> Vec<Message> {
        self.mailbox
            .as_ref()
            .map(SessionMailbox::claim_wake)
            .unwrap_or_default()
    }

    pub(crate) fn active_background_tasks(&self) -> usize {
        self.subagent_cancels.active_count()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn respawn(
        &mut self,
        history: Vec<HistoryItem>,
        model_slot: &Arc<ArcSwap<ModelSlot>>,
        config: AgentConfig,
        tool_output_lines: ToolOutputLines,
        permissions: &Arc<PermissionManager>,
        app: &mut App,
        lua_handle: EventHandle,
        session_lease: Option<Arc<SessionLease>>,
    ) {
        // The output channel survives the respawn, so this bump is the only
        // thing that makes the old loop's in-flight envelopes stale. It lives
        // here so no caller can respawn without it.
        app.run_id += 1;
        let queue_snapshot_ready = app.state.session.meta.queued_messages.is_empty()
            || match app.snapshot_history_head() {
                Ok(()) => true,
                Err(error) => {
                    app.flash(format!("Failed to snapshot workspace: {error}"));
                    false
                }
            };
        let slot = model_slot.load();
        if let Err(e) = smol::block_on(slot.provider.reload_auth()) {
            warn!(error = %e, "failed to reload auth, continuing with existing credentials");
        }
        let subagent_history = stored_subagent_history(&app.state.session);
        let new = spawn_agent_internal(
            (self.agent_tx.clone(), self.agent_rx.clone()),
            model_slot,
            history,
            config,
            tool_output_lines,
            permissions,
            self.mcp_handle.clone(),
            self.mcp_config_errors.clone(),
            Some(SessionRef::from(app.state.session.id)),
            session_lease,
            self.timeouts,
            lua_handle,
            Arc::clone(&self.model_policy),
            app.state.goal.clone(),
            subagent_history,
            app.state.system_prompt_profile.clone(),
            Arc::clone(&self.prompt_profiles),
        );
        let old = mem::replace(self, new);
        // Repoint the app at the new queue before dropping `old`, otherwise the app keeps
        // the last old `QueueSender` alive and the old loop parks in `recv_notify` forever.
        self.apply_to_app(app);
        if queue_snapshot_ready {
            app.flush_restored_queue();
        }
        old.cancel();
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// Hand back the agent task, dropping every channel so the loop can
    /// wind down. The caller sends `CancelAll` first and then awaits all
    /// tabs at once via [`join_all`] instead of paying a serial timeout
    /// per tab.
    pub(crate) fn into_task(self) -> smol::Task<()> {
        self.task
    }
}

/// Wait for every agent task under one shared timeout, not one per task.
pub(crate) fn join_all(tasks: Vec<smol::Task<()>>, timeout: Duration) {
    info!(
        count = tasks.len(),
        "waiting for agents to finish (timeout {timeout:?})"
    );
    smol::block_on(async {
        let finished = futures_lite::future::or(
            async {
                for task in tasks {
                    task.await;
                }
                true
            },
            async {
                smol::Timer::after(timeout).await;
                false
            },
        )
        .await;
        if !finished {
            warn!("agents did not finish within {timeout:?}, forcing shutdown");
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_agent_internal(
    (agent_tx, agent_rx): (flume::Sender<Envelope>, flume::Receiver<Envelope>),
    model_slot: &Arc<ArcSwap<ModelSlot>>,
    initial_history: Vec<HistoryItem>,
    config: AgentConfig,
    tool_output_lines: ToolOutputLines,
    permissions: &Arc<PermissionManager>,
    mcp_handle: Option<McpHandle>,
    mcp_config_errors: McpConfigErrors,
    session_id: Option<SessionRef>,
    session_lease: Option<Arc<SessionLease>>,
    timeouts: caudra_providers::Timeouts,
    lua_handle: EventHandle,
    model_policy: Arc<ModelPolicy>,
    goal: caudra_agent::GoalHandle,
    subagent_history: SubagentHistoryStore,
    system_prompt_profile: Option<Arc<SystemPromptProfile>>,
    prompt_profiles: Arc<PromptProfileCatalog>,
) -> AgentHandles {
    let (cmd_tx, cmd_rx) = flume::unbounded::<AgentCommand>();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();
    let (queue_tx, queue_rx) = shared_queue::queue();
    let queue_rx = Arc::new(queue_rx);
    // Keep the incoming items visible if restore validation fails. A valid
    // AgentLoop synchronously replaces this with its sanitized snapshot.
    let shared_history: SharedHistory = Arc::new(ArcSwap::from_pointee(HistorySnapshot::new(
        initial_history.clone(),
    )));
    let btw_prompt: SharedBtwPrompt = Arc::new(ArcSwap::from_pointee(BtwPrompt::default()));
    let context_store = ContextStore::new();
    let context_publisher = context_store.publisher(ContextKey::Main);
    let (init_trigger, init_cancel) = CancelToken::new();
    let cancel_map = Arc::new(new_run_cancel_map(0, init_trigger));
    let subagent_cancels: Arc<CancelMap<String>> = Arc::new(CancelMap::new());
    let mailbox = session_id
        .as_ref()
        .map(|session_id| SessionMailbox::register(session_id.id()));

    spawn_command_router(
        cmd_rx,
        Arc::clone(&cancel_map),
        Arc::clone(&subagent_cancels),
    );

    let agent_loop = AgentLoop::new(
        Arc::clone(model_slot),
        config,
        tool_output_lines,
        initial_history,
        Arc::clone(&shared_history),
        Arc::clone(&btw_prompt),
        context_publisher,
        mcp_handle.clone(),
        Arc::clone(permissions),
        agent_tx.clone(),
        answer_rx,
        queue_rx,
        cancel_map,
        init_cancel,
        session_id,
        mailbox.clone(),
        timeouts,
        lua_handle,
        Arc::clone(&subagent_cancels),
        subagent_history.clone(),
        Arc::clone(&model_policy),
        goal.clone(),
        system_prompt_profile,
        Arc::clone(&prompt_profiles),
    );

    let task = smol::spawn(async move {
        let _session_lease = session_lease;
        agent_loop.run().await;
    });

    AgentHandles {
        cmd_tx,
        agent_rx,
        agent_tx,
        answer_tx,
        history: shared_history,
        btw_prompt,
        context_store,
        mcp_handle,
        mcp_config_errors,
        queue: queue_tx,
        goal,
        subagent_cancels,
        timeouts,
        model_policy,
        prompt_profiles,
        mailbox,
        task,
    }
}

pub(crate) fn stored_subagent_history(session: &crate::AppSession) -> SubagentHistoryStore {
    let active_history = crate::active_session_history(session).unwrap_or_else(|error| {
        warn!(%error, "failed to resolve active history for subagent restoration");
        Vec::new()
    });
    let mut versions =
        caudra_agent::active_task_history_versions_with_batch_state(&active_history, |call_id| {
            session
                .tool_outputs()
                .get(call_id)
                .and_then(|output| output.state())
        });
    let mut reachable = crate::app::reachable_subagent_ids(
        &active_history,
        session.subagent_messages(),
        session.tool_outputs(),
        session.subagents(),
    );
    reachable.extend(
        versions
            .iter()
            .filter(|(_, version_id)| session.subagent_messages().contains_key(*version_id))
            .map(|(task_id, _)| task_id.clone()),
    );
    let legacy_fallback = reachable.is_empty()
        || active_history.iter().any(|item| {
            matches!(
                &item.kind,
                caudra_providers::HistoryItemKind::AssistantText {
                    retained_subagent_ids,
                    is_compaction_summary: true,
                    ..
                } if retained_subagent_ids.is_empty()
            )
        });
    let mut active_calls = caudra_agent::history_tool_call_ids(&active_history);
    for task_id in &reachable {
        if let Some(history) = session.subagent_messages().get(task_id) {
            active_calls.extend(caudra_agent::history_tool_call_ids(history));
        }
    }
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
    let histories = reachable
        .iter()
        .filter(|task_id| !version_ids.contains(task_id.as_str()))
        .filter_map(|task_id| {
            let version_id = versions.get(task_id).unwrap_or(task_id);
            let items = session
                .subagent_messages()
                .get(version_id)
                .or_else(|| session.subagent_messages().get(task_id))?;
            match project_messages(items) {
                Ok(messages) => Some((task_id.clone(), Arc::new(messages))),
                Err(error) => {
                    warn!(%error, %task_id, "failed to restore subagent history");
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
    SubagentHistoryStore::seeded_with_specs(histories, specs)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Instant;

    use caudra_agent::AgentEvent;
    use caudra_config::PermissionsConfig;
    use caudra_providers::provider::BoxFuture;
    use caudra_providers::{AgentError, ModelInfo, ProviderEvent, RequestOptions, StreamResponse};

    use super::*;

    const LONG_TIMEOUT: Duration = Duration::from_secs(60);
    const SHORT_TIMEOUT: Duration = Duration::from_millis(50);
    const PROBE_TEXT: &str = "probe-through-old-sender";
    const RESTORED_TEXT: &str = "restored-queued-message";
    const RESUMED_HISTORY_TEXT: &str = "resumed-conversation";

    struct StubProvider;

    impl Provider for StubProvider {
        fn stream_message<'a>(
            &'a self,
            _model: &'a Model,
            _messages: &'a [Message],
            _system: &'a str,
            _tools: &'a serde_json::Value,
            _event_tx: &'a flume::Sender<ProviderEvent>,
            _opts: RequestOptions,
            _session_id: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(std::future::pending())
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn stub_spawn() -> (
        AgentHandles,
        Arc<ArcSwap<ModelSlot>>,
        Arc<PermissionManager>,
    ) {
        stub_spawn_with(Vec::new())
    }

    fn stub_spawn_with(
        initial_history: Vec<Message>,
    ) -> (
        AgentHandles,
        Arc<ArcSwap<ModelSlot>>,
        Arc<PermissionManager>,
    ) {
        stub_spawn_with_session(initial_history, None, None)
    }

    fn stub_spawn_with_session(
        initial_history: Vec<Message>,
        session_id: Option<SessionRef>,
        session_lease: Option<Arc<SessionLease>>,
    ) -> (
        AgentHandles,
        Arc<ArcSwap<ModelSlot>>,
        Arc<PermissionManager>,
    ) {
        let model_slot = Arc::new(ArcSwap::from_pointee(ModelSlot {
            model: crate::components::test_model(),
            provider: Arc::new(StubProvider),
        }));
        let permissions = Arc::new(PermissionManager::new_nonpersistent(
            PermissionsConfig::default(),
            PathBuf::from("/tmp"),
            Arc::default(),
        ));
        let handles = AgentHandles::spawn(
            &model_slot,
            crate::history_items(&initial_history),
            AgentConfig::default(),
            ToolOutputLines::default(),
            &permissions,
            session_id,
            session_lease,
            caudra_providers::Timeouts::default(),
            EventHandle::disconnected_for_test(),
            None,
            McpConfigErrors::new(PathBuf::new()),
            Arc::new(ModelPolicy::default()),
            caudra_agent::GoalHandle::default(),
            SubagentHistoryStore::default(),
            None,
            Arc::new(PromptProfileCatalog::default()),
        );
        (handles, model_slot, permissions)
    }

    #[test]
    fn agent_task_holds_the_session_lease_until_termination() {
        let temp = tempfile::TempDir::new().unwrap();
        let state_dir = caudra_storage::StateDir::from_path(temp.path().to_path_buf());
        let id = caudra_storage::id::CaudraId::generate();
        let lease = Arc::new(SessionLease::acquire(&state_dir, id).unwrap());
        let (handles, _, _) = stub_spawn_with_session(
            Vec::new(),
            Some(SessionRef::from(id)),
            Some(Arc::clone(&lease)),
        );
        drop(lease);

        assert!(matches!(
            SessionLease::acquire(&state_dir, id),
            Err(caudra_storage::sessions::SessionError::SessionInUse { .. })
        ));

        smol::block_on(handles.into_task().cancel());
        assert!(SessionLease::acquire(&state_dir, id).is_ok());
    }

    fn respawn(
        handles: &mut AgentHandles,
        model_slot: &Arc<ArcSwap<ModelSlot>>,
        permissions: &Arc<PermissionManager>,
        app: &mut App,
    ) {
        handles.respawn(
            Vec::new(),
            model_slot,
            AgentConfig::default(),
            ToolOutputLines::default(),
            permissions,
            app,
            EventHandle::disconnected_for_test(),
            None,
        );
    }

    /// Senders captured before any respawn (Lua restore replies, clicks) must
    /// still reach the live receiver, and restored queue items must land in
    /// the freshly wired queue, not the one that just died.
    #[test]
    fn respawn_twice_keeps_channel_and_delivers_restored_queue() {
        let (mut handles, model_slot, permissions) = stub_spawn();
        let pre_gen1_sender =
            caudra_agent::EventSender::new(handles.agent_tx.clone(), crate::app::RESTORE_RUN_ID);

        let mut app = crate::app::tests::test_app();
        let run_id_before = app.run_id;
        respawn(&mut handles, &model_slot, &permissions, &mut app);
        assert_eq!(app.run_id, run_id_before + 1);

        app.state.session_mut().meta.queued_messages =
            vec![caudra_storage::sessions::StoredQueuedPrompt {
                text: RESTORED_TEXT.into(),
                images: Vec::new(),
                paste_ranges: Vec::new(),
            }];
        respawn(&mut handles, &model_slot, &permissions, &mut app);
        assert_eq!(
            app.run_id,
            run_id_before + 2,
            "each respawn must bump run_id exactly once"
        );

        pre_gen1_sender
            .send(AgentEvent::TextDelta {
                text: PROBE_TEXT.into(),
            })
            .expect("pre-generation-1 sender must still deliver after two respawns");

        // The respawned loop claims the restored item as soon as the flush
        // wakes it, so reading the queue here races that claim. Count what
        // the loop reports instead: `claim_idle` removes an item before
        // `QueueItemConsumed` announces it, so the queue has settled by the
        // time the event lands and a second copy would still be sitting in
        // it.
        let mut probe_seen = false;
        let mut consumed = Vec::new();
        while !(probe_seen && !consumed.is_empty()) {
            let envelope = handles
                .agent_rx
                .recv_timeout(LONG_TIMEOUT)
                .expect("probe or restored queue item never reached the tab channel");
            match envelope.event {
                AgentEvent::TextDelta { ref text } if text == PROBE_TEXT => probe_seen = true,
                AgentEvent::QueueItemConsumed { text, .. } => {
                    assert_eq!(envelope.run_id, app.run_id);
                    consumed.push(text);
                }
                _ => {}
            }
        }
        assert_eq!(
            consumed,
            [RESTORED_TEXT],
            "the restored item is delivered exactly once"
        );
        assert!(
            app.queue.text_messages().is_empty(),
            "no duplicate of the restored item is left queued"
        );
    }

    /// If the seeded empty snapshot ever outlived `spawn`, the next checkpoint
    /// would adopt it and wipe a resumed conversation from disk.
    #[test]
    fn spawn_publishes_the_resumed_history_before_the_handles_escape() {
        let (handles, _model_slot, _permissions) =
            stub_spawn_with(vec![Message::user(RESUMED_HISTORY_TEXT.into())]);
        let snapshot = handles.history.load();
        assert_eq!(
            snapshot.messages.len(),
            1,
            "the seeded empty snapshot must be replaced synchronously"
        );
        assert!(matches!(
            &snapshot.messages[0].kind,
            caudra_providers::HistoryItemKind::User { text, .. }
                if text == RESUMED_HISTORY_TEXT
        ));
    }

    #[test]
    fn respawn_publishes_the_new_history_into_the_app_mirror() {
        let (mut handles, model_slot, permissions) = stub_spawn();
        let mut app = crate::app::tests::test_app();
        handles.respawn(
            crate::history_items(&[Message::user(RESUMED_HISTORY_TEXT.into())]),
            &model_slot,
            AgentConfig::default(),
            ToolOutputLines::default(),
            &permissions,
            &mut app,
            EventHandle::disconnected_for_test(),
            None,
        );

        let mirror = app
            .shared_history
            .as_ref()
            .expect("respawn wires the live mirror into the app");
        let snapshot = mirror.load();
        assert_eq!(
            snapshot.messages.len(),
            1,
            "a checkpoint right after respawn must not see the seeded empty snapshot"
        );
        assert!(matches!(
            &snapshot.messages[0].kind,
            caudra_providers::HistoryItemKind::User { text, .. }
                if text == RESUMED_HISTORY_TEXT
        ));
    }

    #[test]
    fn join_all_returns_when_all_tasks_complete() {
        join_all(Vec::new(), LONG_TIMEOUT);
        join_all(
            (0..3).map(|_| smol::spawn(async {})).collect(),
            LONG_TIMEOUT,
        );
    }

    #[test]
    fn join_all_stuck_task_returns_after_shared_timeout() {
        let start = Instant::now();
        join_all(
            vec![
                smol::spawn(async {}),
                smol::spawn(futures_lite::future::pending::<()>()),
            ],
            SHORT_TIMEOUT,
        );
        assert!(start.elapsed() >= SHORT_TIMEOUT);
    }
}
