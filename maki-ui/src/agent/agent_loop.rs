use std::sync::Arc;

use arc_swap::ArcSwap;
use maki_agent::agent;
use maki_agent::mcp::config::McpServerStatus;
use maki_agent::mcp::{McpHandle, McpSession};
use maki_agent::permissions::PermissionManager;
use maki_agent::prompt::profile::{
    BUILTIN_PROFILE_NAME, PromptProfileCatalog, SystemPromptProfile,
};
use maki_agent::template;
use maki_agent::template::Vars;
use maki_agent::tools::{
    DescriptionContext, FileReadTracker, ToolAudience, ToolFilter, ToolRegistry,
};
use maki_agent::{
    Agent, AgentConfig, AgentEvent, AgentInput, AgentParams, AgentRunParams, CancelMap,
    CancelToken, CancelTrigger, DoneReason, Envelope, EventSender, GoalHandle, History,
    Instructions, McpCommand, PromptRole, SessionMailbox, SharedHistory, SubagentHistoryStore,
    ToolOutputLines,
};
use maki_config::ModelPolicy;
use maki_lua::EventHandle;
use maki_providers::{AgentError, HistoryItem, Message, Model};
use maki_storage::id::SessionRef;
use serde_json::Value;
use tracing::error;

use super::ModelSlot;
use super::cancel_map::RunCancelMap;
use super::shared_queue::{QueueItem, QueueReceiver};

pub(super) struct AgentLoop {
    model_slot: Arc<ArcSwap<ModelSlot>>,
    config: AgentConfig,
    tool_output_lines: ToolOutputLines,
    vars: Vars,
    instructions: Instructions,
    tools: Value,
    mcp: Option<McpSession>,
    history: History,
    history_restore_error: Option<String>,
    btw_system: Arc<ArcSwap<String>>,
    cancel_map: Arc<RunCancelMap>,
    init_cancel: CancelToken,
    permissions: Arc<PermissionManager>,
    file_tracker: Arc<FileReadTracker>,
    min_run_id: u64,
    agent_tx: flume::Sender<Envelope>,
    answer_rx: Arc<async_lock::Mutex<flume::Receiver<String>>>,
    queue: Arc<QueueReceiver>,
    session_id: Option<SessionRef>,
    mailbox: Option<SessionMailbox>,
    timeouts: maki_providers::Timeouts,
    lua_handle: EventHandle,
    subagent_cancels: Arc<CancelMap<String>>,
    subagent_history: SubagentHistoryStore,
    model_policy: Arc<ModelPolicy>,
    goal: GoalHandle,
    system_prompt_profile: Option<Arc<SystemPromptProfile>>,
    prompt_profiles: Arc<PromptProfileCatalog>,
}

impl AgentLoop {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        model_slot: Arc<ArcSwap<ModelSlot>>,
        config: AgentConfig,
        tool_output_lines: ToolOutputLines,
        initial_history: Vec<HistoryItem>,
        shared_history: SharedHistory,
        btw_system: Arc<ArcSwap<String>>,
        mcp_handle: Option<McpHandle>,
        permissions: Arc<PermissionManager>,
        agent_tx: flume::Sender<Envelope>,
        answer_rx: flume::Receiver<String>,
        queue: Arc<QueueReceiver>,
        cancel_map: Arc<RunCancelMap>,
        init_cancel: CancelToken,
        session_id: Option<SessionRef>,
        mailbox: Option<SessionMailbox>,
        timeouts: maki_providers::Timeouts,
        lua_handle: EventHandle,
        subagent_cancels: Arc<CancelMap<String>>,
        subagent_history: SubagentHistoryStore,
        model_policy: Arc<ModelPolicy>,
        goal: GoalHandle,
        system_prompt_profile: Option<Arc<SystemPromptProfile>>,
        prompt_profiles: Arc<PromptProfileCatalog>,
    ) -> Self {
        let restored_history = History::restored(initial_history);
        let initial_messages = restored_history
            .as_ref()
            .map(|history| history.as_slice())
            .unwrap_or_default();
        let mcp = mcp_handle.map(|h| McpSession::new(h, initial_messages));
        let (history, history_restore_error) = match restored_history {
            Ok(history) => (history.with_mirror(shared_history), None),
            Err(error) => (History::default(), Some(error.to_string())),
        };
        Self {
            model_slot,
            config,
            tool_output_lines,
            vars: Vars::default(),
            instructions: Instructions::default(),
            tools: Value::Null,
            mcp,
            history,
            history_restore_error,
            btw_system,
            cancel_map,
            init_cancel,
            permissions,
            file_tracker: FileReadTracker::fresh(),
            min_run_id: 0,
            agent_tx,
            answer_rx: Arc::new(async_lock::Mutex::new(answer_rx)),
            queue,
            session_id,
            mailbox,
            timeouts,
            lua_handle,
            subagent_cancels,
            subagent_history,
            model_policy,
            goal,
            system_prompt_profile,
            prompt_profiles,
        }
    }

    pub(super) async fn run(mut self) {
        if let Some(error) = self.history_restore_error.take() {
            error!(%error, "failed to restore history");
            let _ = EventSender::new(self.agent_tx, self.min_run_id).send(AgentEvent::Error {
                message: format!("Failed to restore history: {error}"),
            });
            return;
        }
        if !self.initialize().await {
            return;
        }

        let mut drain_run_id = None;
        while let Ok(()) = self.queue.recv_notify().await {
            let mut last_run_id = None;
            let mut paused = false;
            loop {
                let mut claimed = self.queue.claim_idle(self.min_run_id);
                if claimed.is_empty() {
                    break;
                }
                let Some((_, last)) = claimed.last() else {
                    continue;
                };
                last_run_id = Some(last.run_id());
                if claimed.len() == 1 {
                    let Some((id, entry)) = claimed.pop() else {
                        continue;
                    };
                    paused = !self.process_entry(id, entry).await;
                } else {
                    paused = !self.process_batch(claimed).await;
                }
                if paused {
                    break;
                }
            }
            if last_run_id.is_some() {
                drain_run_id = last_run_id;
            }
            if let Some(run_id) = drain_run_id {
                let event_tx = EventSender::new(self.agent_tx.clone(), run_id);
                if self
                    .queue
                    .publish_if_empty(|| event_tx.try_send(AgentEvent::QueueDrained))
                {
                    drain_run_id = None;
                }
            }
            if paused {
                continue;
            }
        }
    }

    async fn process_entry(&mut self, id: maki_agent::QueueItemId, entry: QueueItem) -> bool {
        let run_id = entry.run_id();
        let event_tx = EventSender::new(self.agent_tx.clone(), run_id);

        let result = match entry {
            QueueItem::Message {
                text,
                image_count,
                input,
                displayed,
                ..
            } => {
                if !displayed {
                    let _ = event_tx.send(AgentEvent::QueueItemConsumed {
                        id,
                        text,
                        image_count,
                    });
                }
                self.do_agent_run(input, event_tx, run_id).await
            }
            QueueItem::Compact { .. } => self.do_compact(&event_tx).await,
        };
        self.queue.clear_active_run();

        if let Err(error) = result {
            let superseded = self.queue.has_newer_interrupt(run_id);
            if !superseded {
                self.queue.pause();
            }
            self.emit_error(run_id, error);
            return superseded;
        }
        true
    }

    async fn process_batch(&mut self, entries: Vec<(maki_agent::QueueItemId, QueueItem)>) -> bool {
        let Some(run_id) = entries.last().map(|(_, entry)| entry.run_id()) else {
            return true;
        };
        let event_tx = EventSender::new(self.agent_tx.clone(), run_id);
        let initial = entries.iter().any(|(_, entry)| {
            matches!(
                entry,
                QueueItem::Message {
                    admission: maki_agent::PromptAdmission::Interrupt,
                    ..
                }
            )
        });
        let mut consumed = Vec::with_capacity(entries.len());
        let mut inputs = Vec::with_capacity(entries.len());
        for (id, entry) in entries {
            let QueueItem::Message {
                text,
                image_count,
                input,
                displayed: false,
                ..
            } = entry
            else {
                continue;
            };
            consumed.push(maki_agent::QueueConsumedItem {
                id,
                text,
                image_count,
            });
            inputs.push(input);
        }
        if inputs.is_empty() {
            self.queue.clear_active_run();
            return true;
        }
        let _ = event_tx.send(AgentEvent::QueueBatchConsumed { items: consumed });
        let result = self.do_agent_batch(inputs, event_tx, run_id, initial).await;
        self.queue.clear_active_run();
        if let Err(error) = result {
            let superseded = self.queue.has_newer_interrupt(run_id);
            if !superseded {
                self.queue.pause();
            }
            self.emit_error(run_id, error);
            return superseded;
        }
        true
    }

    async fn initialize(&mut self) -> bool {
        self.vars = template::env_vars();
        self.reload_instructions().await;
        if self.init_cancel.is_cancelled() {
            return false;
        }
        self.publish_btw_system(&maki_agent::prompt::ResolvedSlots::default());

        let slot = self.model_slot.load();
        self.tools = self.build_tools(
            &slot.model,
            &maki_providers::ThinkingConfig::default(),
            false,
        );
        if let Some(ref mcp) = self.mcp {
            // The queue is drained right after this, and a prompt typed during
            // startup must still carry the MCP tools.
            if self.init_cancel.race(mcp.ready()).await.is_err() {
                return false;
            }
            spawn_oauth_for_needs_auth(mcp);
        }
        !self.init_cancel.is_cancelled()
    }

    async fn do_compact(&mut self, event_tx: &EventSender) -> Result<(), AgentError> {
        let slot = self.model_slot.load();
        let (provider, model) = agent::resolve_compaction_model(
            &slot.provider,
            &slot.model,
            self.timeouts,
            &self.model_policy,
        )?;
        let usage = agent::compact(
            &*provider,
            &model,
            &mut self.history,
            event_tx,
            &self.config,
        )
        .await?;
        self.goal
            .record_external_usage(usage, model.billed_cost(&usage, false));
        Ok(())
    }

    async fn do_agent_run(
        &mut self,
        input: AgentInput,
        event_tx: EventSender,
        run_id: u64,
    ) -> Result<(), AgentError> {
        self.do_agent_inputs(vec![input], event_tx, run_id, false)
            .await
    }

    async fn do_agent_batch(
        &mut self,
        inputs: Vec<AgentInput>,
        event_tx: EventSender,
        run_id: u64,
        initial: bool,
    ) -> Result<(), AgentError> {
        self.do_agent_inputs(inputs, event_tx, run_id, initial)
            .await
    }

    async fn do_agent_inputs(
        &mut self,
        mut inputs: Vec<AgentInput>,
        event_tx: EventSender,
        run_id: u64,
        initial_batch: bool,
    ) -> Result<(), AgentError> {
        let Some(input) = inputs.last_mut() else {
            return Ok(());
        };
        let slot = self.model_slot.load();

        let old_cwd = self.vars.apply("{cwd}").into_owned();
        self.vars = template::env_vars();
        if *self.vars.apply("{cwd}") != old_cwd {
            self.reload_instructions().await;
        }
        self.rebuild_tools(&slot.model, &input.thinking, input.workflow);

        if let Some(ref prompt_ref) = input.prompt {
            let Some(ref mcp) = self.mcp else {
                return Err(AgentError::Tool {
                    tool: "mcp_prompt".into(),
                    message: "MCP not available".into(),
                });
            };
            let messages = mcp
                .get_prompt(&prompt_ref.qualified_name, &prompt_ref.arguments)
                .await
                .map_err(|e| AgentError::Tool {
                    tool: "mcp_prompt".into(),
                    message: e.to_string(),
                })?;
            for pm in messages {
                let text = pm.content.text.unwrap_or_default();
                let msg = match pm.role {
                    PromptRole::Assistant => Message {
                        role: maki_providers::Role::Assistant,
                        content: vec![maki_providers::ContentBlock::Text { text }],
                        ..Default::default()
                    },
                    PromptRole::User => Message::user(text),
                };
                input.preamble.push(msg);
            }
        }

        let prompt_slots = self
            .lua_handle
            .collect_prompt_slots_async(&self.config)
            .await;
        let system = agent::build_system_prompt(
            &self.vars,
            &input.mode,
            &self.instructions.text,
            &prompt_slots,
            &slot.model,
            self.system_prompt_profile.as_deref(),
        );
        self.publish_btw_system(&prompt_slots);
        let (trigger, cancel) = CancelToken::new();
        self.set_cancel_trigger(run_id, trigger);

        while self.answer_rx.lock().await.try_recv().is_ok() {}

        let mut agent = Agent::new(
            AgentParams {
                provider: Arc::clone(&slot.provider),
                model: slot.model.clone(),
                config: self.config.clone(),
                tool_output_lines: self.tool_output_lines,
                permissions: Arc::clone(&self.permissions),
                session_id: self.session_id.clone(),
                root_tool_use_id: None,
                mailbox: self.mailbox.clone(),
                timeouts: self.timeouts,
                file_tracker: Arc::clone(&self.file_tracker),
                prompt_slots: Arc::new(prompt_slots),
                prompt_profiles: Arc::clone(&self.prompt_profiles),
                system_prompt_profile_name: Arc::from(
                    self.system_prompt_profile
                        .as_ref()
                        .map_or(BUILTIN_PROFILE_NAME, |profile| profile.name()),
                ),
                subagent_cancels: Arc::clone(&self.subagent_cancels),
                subagent_history: self.subagent_history.clone(),
                registry: Arc::clone(maki_agent::tools::ToolRegistry::global_arc()),
                audience: ToolAudience::MAIN,
                tool_filter: ToolFilter::from_config(&self.config, &slot.model, &[]),
                model_policy: Arc::clone(&self.model_policy),
            },
            AgentRunParams {
                history: &mut self.history,
                system,
                event_tx,
                tools: self.tools.clone(),
            },
        )
        .with_loaded_instructions(self.instructions.loaded.clone())
        .with_user_response_rx(Arc::clone(&self.answer_rx))
        .with_interrupt_source(Arc::clone(&self.queue) as Arc<dyn maki_agent::InterruptSource>)
        .with_cancel(cancel)
        .with_goal(self.goal.clone())
        .with_mcp(self.mcp.clone());

        let result = if inputs.len() == 1 {
            let Some(input) = inputs.pop() else {
                return Ok(());
            };
            agent.run(input).await
        } else if initial_batch {
            let first = inputs.remove(0);
            agent.run_initial_batch(first, inputs).await
        } else {
            let first = inputs.remove(0);
            agent.run_batch(first, inputs).await
        };
        drop(agent);

        self.clear_cancel_trigger(run_id);

        if matches!(result, Ok(DoneReason::Cancelled)) {
            self.min_run_id = run_id + 1;
        }

        result.map(|_| ())
    }

    /// Base tools only. MCP definitions are injected per request by
    /// `Agent::request_tools`; baking them here would freeze the catalog.
    fn rebuild_tools(
        &mut self,
        model: &Model,
        thinking: &maki_providers::ThinkingConfig,
        workflow: bool,
    ) {
        self.tools = self.build_tools(model, thinking, workflow);
    }

    fn build_tools(
        &self,
        model: &Model,
        thinking: &maki_providers::ThinkingConfig,
        workflow: bool,
    ) -> Value {
        let examples = model.supports_tool_examples();
        let filter = ToolFilter::from_config(&self.config, model, &[]);
        let bindings =
            self.prompt_profiles
                .bind_for_tasks(model, thinking, &self.model_policy, self.timeouts);
        let vars = self.vars.clone().set(
            "{task_system_prompt_profiles}",
            bindings.task_tool_summary("Maki's built-in task prompt"),
        );
        let ctx = DescriptionContext {
            filter: &filter,
            audience: ToolAudience::MAIN,
            workflow,
        };
        ToolRegistry::global().definitions(&vars, &ctx, examples)
    }

    async fn reload_instructions(&mut self) {
        let cwd = self.vars.apply("{cwd}").into_owned();
        self.instructions = smol::unblock(move || agent::load_instructions(&cwd)).await;
    }

    /// Always pins `Build` mode: btw runs no tools, so Plan-mode constraints would only confuse
    /// the model. Everything else matches the live prompt.
    fn publish_btw_system(&self, prompt_slots: &maki_agent::prompt::ResolvedSlots) {
        let slot = self.model_slot.load();
        let system = agent::build_system_prompt(
            &self.vars,
            &maki_agent::AgentMode::Build,
            &self.instructions.text,
            prompt_slots,
            &slot.model,
            self.system_prompt_profile.as_deref(),
        );
        self.btw_system.store(Arc::new(system));
    }

    fn set_cancel_trigger(&self, run_id: u64, trigger: CancelTrigger) {
        // One trigger per run, and `clear_cancel_trigger` drops the whole
        // key, so the slot is not worth carrying around.
        let _ = self.cancel_map.insert(run_id, trigger);
    }

    fn clear_cancel_trigger(&self, run_id: u64) {
        self.cancel_map.remove(&run_id);
    }

    fn emit_error(&self, run_id: u64, error: AgentError) {
        error!(error = %error, "agent error");
        let event_tx = EventSender::new(self.agent_tx.clone(), run_id);
        let _ = event_tx.send(AgentEvent::Error {
            message: error.user_message(),
        });
    }
}

fn spawn_oauth_for_needs_auth(handle: &McpHandle) {
    let snapshot = handle.reader().load().clone();
    for info in snapshot.infos.iter() {
        let McpServerStatus::NeedsAuth { ref url } = info.status else {
            continue;
        };
        let Some(ref server_url) = info.url else {
            continue;
        };
        let handle = handle.clone();
        let server_name = info.name.clone();
        let server_url = server_url.clone();
        let www_auth = url.clone();
        let oauth = info.oauth.clone();
        let resolved_addresses = info.resolved_addresses.clone();
        smol::spawn(async move {
            let storage = match maki_storage::StateDir::resolve() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(server = %server_name, error = %e, "cannot resolve storage for OAuth");
                    return;
                }
            };
            if let Err(e) = maki_agent::mcp::oauth::authenticate(
                &server_name,
                &server_url,
                www_auth.as_deref(),
                &storage,
                maki_agent::mcp::oauth::Interaction::Background,
                oauth,
                Some(&resolved_addresses),
            )
            .await
            {
                tracing::warn!(server = %server_name, error = %e, "background OAuth failed");
                return;
            }
            handle.send(McpCommand::Reconnect {
                server: server_name.clone(),
            });
            tracing::info!(server = %server_name, "MCP server authenticated via OAuth");
        })
        .detach();
    }
}
