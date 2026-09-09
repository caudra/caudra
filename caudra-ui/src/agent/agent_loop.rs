use std::{env, sync::Arc};

use arc_swap::ArcSwap;
use caudra_agent::agent;
use caudra_agent::context::{
    BuiltinToolsInput, ContextCapture, ContextInventory, ContextPublisher, ContextReadiness,
    ContextSnapshot,
};
use caudra_agent::mcp::config::McpServerStatus;
use caudra_agent::mcp::{McpHandle, McpRequestSnapshot, McpSession};
use caudra_agent::permissions::PermissionManager;
use caudra_agent::prompt::profile::{
    BUILTIN_PROFILE_NAME, PromptProfileCatalog, SystemPromptProfile,
};
use caudra_agent::template;
use caudra_agent::template::Vars;
use caudra_agent::tools::{
    DeferralSession, DeferredTool, DescriptionContext, FileReadTracker, PathLocks, ToolAudience,
    ToolDefinitions, ToolFilter, ToolRegistry, deferral,
};
use caudra_agent::{
    Agent, AgentConfig, AgentEvent, AgentInput, AgentParams, AgentRunParams, CancelMap,
    CancelToken, CancelTrigger, DoneReason, Envelope, EventSender, GoalHandle, History,
    InstructionBaseline, Instructions, McpCommand, Nudge, PromptRole, SessionMailbox,
    SharedHistory, SubagentHistoryStore, ToolOutputLines,
};
use caudra_config::ModelPolicy;
use caudra_lua::EventHandle;
use caudra_providers::{AgentError, HistoryItem, Message, Model, RequestOptions};
use caudra_storage::id::SessionRef;
use serde_json::Value;
use tracing::error;

use super::cancel_map::RunCancelMap;
use super::shared_queue::{QueueItem, QueueReceiver};
use super::{BtwPrompt, ModelSlot, SharedBtwPrompt};

pub(super) struct AgentLoop {
    model_slot: Arc<ArcSwap<ModelSlot>>,
    config: AgentConfig,
    tool_output_lines: ToolOutputLines,
    vars: Vars,
    instructions: InstructionBaseline,
    tools: Value,
    /// Withheld from `tools` until `tool_search` loads them. Rebuilt with
    /// `tools`, so a model switch reconsiders both halves together.
    deferred: Vec<DeferredTool>,
    mcp: Option<McpSession>,
    history: History,
    history_restore_error: Option<String>,
    btw_prompt: SharedBtwPrompt,
    context_publisher: ContextPublisher,
    context_system: String,
    context_options: RequestOptions,
    cancel_map: Arc<RunCancelMap>,
    retry_now: Nudge,
    init_cancel: CancelToken,
    permissions: Arc<PermissionManager>,
    file_tracker: Arc<FileReadTracker>,
    path_locks: Arc<PathLocks>,
    min_run_id: u64,
    agent_tx: flume::Sender<Envelope>,
    answer_rx: Arc<async_lock::Mutex<flume::Receiver<String>>>,
    queue: Arc<QueueReceiver>,
    session_id: Option<SessionRef>,
    mailbox: Option<SessionMailbox>,
    timeouts: caudra_providers::Timeouts,
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
        btw_prompt: SharedBtwPrompt,
        context_publisher: ContextPublisher,
        mcp_handle: Option<McpHandle>,
        permissions: Arc<PermissionManager>,
        agent_tx: flume::Sender<Envelope>,
        answer_rx: flume::Receiver<String>,
        queue: Arc<QueueReceiver>,
        cancel_map: Arc<RunCancelMap>,
        retry_now: Nudge,
        init_cancel: CancelToken,
        session_id: Option<SessionRef>,
        mailbox: Option<SessionMailbox>,
        timeouts: caudra_providers::Timeouts,
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
        let mcp = mcp_handle.map(|h| {
            McpSession::new(h, initial_messages).with_disabled_tools(&config.disabled_tools)
        });
        let (history, history_restore_error) = match restored_history {
            Ok(history) => (history.with_mirror(shared_history), None),
            Err(error) => (History::default(), Some(error.to_string())),
        };
        Self {
            model_slot,
            config,
            tool_output_lines,
            vars: Vars::default(),
            instructions: InstructionBaseline::default(),
            tools: Value::Null,
            deferred: Vec::new(),
            mcp,
            history,
            history_restore_error,
            btw_prompt,
            context_publisher,
            context_system: String::new(),
            context_options: RequestOptions::default(),
            cancel_map,
            retry_now,
            init_cancel,
            permissions,
            file_tracker: FileReadTracker::fresh(),
            path_locks: PathLocks::fresh(),
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

    async fn process_entry(&mut self, id: caudra_agent::QueueItemId, entry: QueueItem) -> bool {
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
                self.do_agent_run(*input, event_tx, run_id).await
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

    async fn process_batch(
        &mut self,
        entries: Vec<(caudra_agent::QueueItemId, QueueItem)>,
    ) -> bool {
        let Some(run_id) = entries.last().map(|(_, entry)| entry.run_id()) else {
            return true;
        };
        let event_tx = EventSender::new(self.agent_tx.clone(), run_id);
        let initial = entries.iter().any(|(_, entry)| {
            matches!(
                entry,
                QueueItem::Message {
                    admission: caudra_agent::PromptAdmission::Interrupt,
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
            consumed.push(caudra_agent::QueueConsumedItem {
                id,
                text,
                image_count,
            });
            inputs.push(*input);
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
        if let Some(ref mcp) = self.mcp {
            // The queue is drained right after this, and a prompt typed during
            // startup must still carry the MCP tools.
            if self.init_cancel.race(mcp.ready()).await.is_err() {
                return false;
            }
            spawn_oauth_for_needs_auth(mcp);
        }
        // Built once MCP has settled, so a `/btw` fired before the first prompt
        // carries the same tools the live request will.
        let slot = self.model_slot.load();
        self.rebuild_tools(
            &slot.model,
            &caudra_providers::ThinkingConfig::default(),
            false,
        );
        self.context_system = self.publish_btw_prompt(
            &caudra_agent::prompt::ResolvedSlots::default(),
            RequestOptions::default(),
        );
        self.publish_prepared_context(&slot);
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
            .record_external_usage(usage, model.billed_cost(&usage, false), model.billing);
        self.publish_prepared_context(&slot);
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
        let instructions = if *self.vars.apply("{cwd}") != old_cwd {
            self.reload_instructions().await;
            None
        } else {
            let current = self.read_instructions().await;
            self.instructions.drift(current, self.history.epoch())
        };
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
                        role: caudra_providers::Role::Assistant,
                        content: vec![caudra_providers::ContentBlock::Text { text }],
                        ..Default::default()
                    },
                    PromptRole::User => Message::user(text),
                };
                input.preamble.push(msg);
            }
        }

        let opts = RequestOptions {
            thinking: input.thinking.clone(),
            fast: input.fast,
        };
        let prompt_slots = self
            .lua_handle
            .collect_prompt_slots_async(&self.config)
            .await;
        let tool_filter = ToolFilter::from_config(&self.config, &slot.model, &[]);
        let system = agent::build_system_prompt(
            self.instructions.text(),
            &prompt_slots,
            &tool_filter,
            self.system_prompt_profile.as_deref(),
        );
        self.context_system.clone_from(&system);
        self.context_options = opts.clone();
        self.publish_btw_prompt(&prompt_slots, opts.clone());
        let (trigger, cancel) = CancelToken::new();
        self.set_cancel_trigger(run_id, trigger);

        while self.answer_rx.lock().await.try_recv().is_ok() {}

        let active_prompt_profile_name: Arc<str> = Arc::from(
            self.system_prompt_profile
                .as_ref()
                .map_or(BUILTIN_PROFILE_NAME, |profile| profile.name()),
        );

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
                context_publisher: Some(self.context_publisher.clone()),
                timeouts: self.timeouts,
                file_tracker: Arc::clone(&self.file_tracker),
                path_locks: Arc::clone(&self.path_locks),
                prompt_slots: Arc::new(prompt_slots),
                prompt_profiles: Arc::clone(&self.prompt_profiles),
                default_task_prompt_profile_name: Arc::clone(&active_prompt_profile_name),
                active_prompt_profile_name: Some(active_prompt_profile_name),
                subagent_cancels: Arc::clone(&self.subagent_cancels),
                subagent_history: self.subagent_history.clone(),
                registry: Arc::clone(caudra_agent::tools::ToolRegistry::global_arc()),
                audience: ToolAudience::MAIN,
                tool_filter,
                model_policy: Arc::clone(&self.model_policy),
            },
            AgentRunParams {
                history: &mut self.history,
                system,
                environment: Some(agent::environment_block(&self.vars, &slot.model)),
                instructions,
                event_tx,
                tools: self.tools.clone(),
                deferred: self.deferred.clone(),
            },
        )
        .with_loaded_instructions(self.instructions.loaded().clone())
        .with_user_response_rx(Arc::clone(&self.answer_rx))
        .with_interrupt_source(Arc::clone(&self.queue) as Arc<dyn caudra_agent::InterruptSource>)
        .with_cancel(cancel)
        .with_retry_now(self.retry_now.clone())
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
        thinking: &caudra_providers::ThinkingConfig,
        workflow: bool,
    ) {
        let definitions = self.build_tools(model, thinking, workflow);
        self.tools = definitions.declared;
        self.deferred = definitions.deferred;
    }

    fn build_tools(
        &self,
        model: &Model,
        thinking: &caudra_providers::ThinkingConfig,
        workflow: bool,
    ) -> ToolDefinitions {
        let examples = model.supports_tool_examples();
        let filter = ToolFilter::from_config(&self.config, model, &[]);
        let bindings =
            self.prompt_profiles
                .bind_for_tasks(model, thinking, &self.model_policy, self.timeouts);
        let vars = self.vars.clone().set(
            "{task_system_prompt_profiles}",
            bindings.task_tool_summary("Caudra's built-in task prompt"),
        );
        let ctx = DescriptionContext {
            filter: &filter,
            audience: ToolAudience::MAIN,
            workflow,
        };
        ToolRegistry::global().definitions_split(
            &vars,
            &ctx,
            examples,
            &deferral::deferred_names(&self.config.allowed_tools),
        )
    }

    async fn read_instructions(&self) -> Instructions {
        let cwd = self.vars.apply("{cwd}").into_owned();
        smol::unblock(move || agent::load_instructions(&cwd)).await
    }

    /// Rewrites the system prompt to match disk, so it is only free while the
    /// prefix cache is cold anyway: at startup, or on a change of directory.
    async fn reload_instructions(&mut self) {
        let current = self.read_instructions().await;
        self.instructions = InstructionBaseline::adopt(current, self.history.epoch());
    }

    /// The array a request actually carries. `self.tools` is the declared base, and a run
    /// appends the deferred catalog and then MCP on top, so anything that has to match the live
    /// array has to append them in that order. The loaded set comes from history, which is what
    /// the agent seeds its own session from. A load that happens mid-turn is not reflected until
    /// the next publish.
    fn request_tools(&self, mcp: Option<&McpRequestSnapshot>) -> Value {
        let mut tools = self.tools.clone();
        let mut sections: Vec<String> = DeferralSession::new(
            self.deferred.clone(),
            deferral::loaded_tool_names(self.history.as_slice()),
        )
        .request_snapshot()
        .extend_declared(&mut tools)
        .into_iter()
        .collect();
        if let Some(mcp) = mcp {
            sections.extend(mcp.extend_declared(&mut tools));
        }
        deferral::push_catalog(&mut tools, &sections);
        tools
    }

    /// Always pins `Build` mode: btw never acts on tools, so Plan-mode constraints would only
    /// confuse the model. Everything else must match the live request byte for byte, because a
    /// divergent tools array or system prompt costs btw the provider cache prefix and makes it
    /// re-read the whole history as fresh input tokens.
    fn publish_btw_prompt(
        &self,
        prompt_slots: &caudra_agent::prompt::ResolvedSlots,
        opts: RequestOptions,
    ) -> String {
        let slot = self.model_slot.load();
        let tool_filter = ToolFilter::from_config(&self.config, &slot.model, &[]);
        let system = agent::build_system_prompt(
            self.instructions.text(),
            prompt_slots,
            &tool_filter,
            self.system_prompt_profile.as_deref(),
        );
        let mcp = self.mcp.as_ref().map(McpSession::request_snapshot);
        self.btw_prompt.store(Arc::new(BtwPrompt {
            system: system.clone(),
            tools: self.request_tools(mcp.as_ref()),
            opts,
        }));
        system
    }

    fn publish_prepared_context(&self, slot: &ModelSlot) {
        let mcp = self.mcp.as_ref().map(McpSession::request_snapshot);
        let tools = self.request_tools(mcp.as_ref());
        let messages = agent::project_for_target(
            self.history.as_slice(),
            &tools,
            &slot.model,
            slot.provider.reasoning_transport(&slot.model),
        );
        let options = self.context_options.clamped(&slot.model);
        let task_profiles = self.prompt_profiles.bind_for_tasks(
            &slot.model,
            &options.thinking,
            &self.model_policy,
            self.timeouts,
        );
        let cwd = env::current_dir().unwrap_or_else(|_| self.permissions.project_cwd());
        let inventory = ContextInventory::collect(
            &cwd,
            ToolRegistry::global(),
            &self.prompt_profiles,
            &task_profiles,
            Some(
                self.system_prompt_profile
                    .as_deref()
                    .map_or(BUILTIN_PROFILE_NAME, SystemPromptProfile::name),
            ),
            Some(&BuiltinToolsInput {
                registry: ToolRegistry::global(),
                filter: &ToolFilter::from_config(&self.config, &slot.model, &[]),
                config: &self.config,
                model: &slot.model,
                deferred: &self.deferred,
            }),
            mcp.as_ref(),
        );
        self.context_publisher
            .publish(ContextSnapshot::capture(ContextCapture {
                readiness: ContextReadiness::PreparedNextRequest,
                model: &slot.model,
                auto_compact: agent::auto_compact_enabled(),
                compaction_buffer: self.config.compaction_buffer,
                system: &self.context_system,
                base_tools: &self.tools,
                full_tools: &tools,
                projected_messages: messages.as_ref(),
                inventory,
            }));
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
            let storage = match caudra_storage::StateDir::resolve() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(server = %server_name, error = %e, "cannot resolve storage for OAuth");
                    return;
                }
            };
            if let Err(e) = caudra_agent::mcp::oauth::authenticate(
                &server_name,
                &server_url,
                www_auth.as_deref(),
                &storage,
                caudra_agent::mcp::oauth::Interaction::Background,
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
