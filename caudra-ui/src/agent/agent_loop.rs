use std::path::PathBuf;
use std::{env, sync::Arc};

use crate::app::background_delivery::DeliveryFence;
use crate::app::file_revert::RecorderSlot;
use arc_swap::ArcSwap;
use caudra_agent::agent;
use caudra_agent::agent::task_runner::{HostExtras, WorkflowHostContext};
use caudra_agent::background::BackgroundTasks;
use caudra_agent::context::{
    BuiltinToolsInput, ContextCapture, ContextInventory, ContextMcpInventory, ContextPublisher,
    ContextReadiness, ContextSnapshot,
};
use caudra_agent::mcp::config::McpServerStatus;
use caudra_agent::mcp::{McpHandle, McpRequestSnapshot, McpSession};
use caudra_agent::memory::baseline::MemoryBaseline;
use caudra_agent::memory::compactor::Pump;
use caudra_agent::permissions::PermissionManager;
use caudra_agent::prompt::ResolvedSlots;
use caudra_agent::prompt::profile::{
    BUILTIN_PROFILE_NAME, PromptProfileCatalog, SystemPromptProfile, TaskProfileBindings,
};
use caudra_agent::template;
use caudra_agent::template::Vars;
use caudra_agent::tools::execution::{configure_tools, execution_slots};
use caudra_agent::tools::native::plan::PlanTarget;
use caudra_agent::tools::{
    BuiltinDeferral, DeferralSession, DeferredTool, DescriptionContext, FileReadTracker, PathLocks,
    ToolAudience, ToolDefinitions, ToolFilter, ToolRegistry, deferral,
};
use caudra_agent::types::{MEMORY_EVENT_RUN_ID, TodoItem};
use caudra_agent::workflow::WorkflowHandle;
use caudra_agent::{
    Agent, AgentConfig, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams,
    BackgroundReminderContext, CancelMap, CancelToken, CancelTrigger, DoneReason, Envelope,
    EventSender, GoalHandle, History, InstructionBaseline, Instructions, McpCommand, Nudge,
    PromptRole, SessionMailbox, SharedHistory, SubagentHistoryStore, ToolOutputLines,
};
use caudra_config::{ModelPolicy, ProfileToolPolicy};
use caudra_lua::EventHandle;
use caudra_providers::{
    AgentError, CacheKey, HistoryItem, Message, Model, ModelPurpose, RequestOptions, ThinkingConfig,
};
use caudra_storage::id::SessionRef;
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_storage::tool_outputs::ToolOutputStore;
use caudra_workspace::WorkspaceSession;
use serde_json::Value;
use tracing::error;

use super::cancel_map::RunCancelMap;
use super::shared_queue::{QueueItem, QueueReceiver};
use super::workflow::SharedMode;
use super::{BtwPrompt, ModelSlot, SharedBtwPrompt, refresh_remote_context};

const TASK_CONTEXT_UNAVAILABLE: &str = "Remote project context unavailable for task continuation";

pub(crate) struct ToolsPreviewSource {
    pub(crate) registry: Arc<ToolRegistry>,
    config: AgentConfig,
    profile_name: Arc<str>,
    profile: Arc<ProfileToolPolicy>,
    profiles: Arc<PromptProfileCatalog>,
    model_policy: Arc<ModelPolicy>,
    workflows_available: bool,
    background_available: bool,
    pub(crate) mcp: Option<McpSession>,
}

impl ToolsPreviewSource {
    pub(crate) fn profile_name(&self) -> &str {
        &self.profile_name
    }

    fn definitions(
        &self,
        model: &Model,
        vars: &Vars,
        mode: &AgentMode,
        session_plan: bool,
        bindings: &TaskProfileBindings,
    ) -> ToolDefinitions {
        let filter = ToolFilter::from_config(&self.config, model, &[]).for_mode(mode);
        let vars = vars.clone().set(
            "{task_system_prompt_profiles}",
            bindings.task_tool_summary("Caudra's built-in task prompt"),
        );
        let mut definitions = self.registry.definitions_split_with_policy(
            &vars,
            &DescriptionContext {
                filter: &filter,
                audience: ToolAudience::MAIN,
                workflows_available: self.workflows_available,
            },
            model.supports_tool_examples(),
            &deferral::deferred_names(
                &self.config.allowed_tools,
                BuiltinDeferral::resolve(&self.config, model),
            ),
            &self.profile,
            session_plan,
        );
        configure_tools(
            &mut definitions.declared,
            &mut definitions.deferred,
            &self.config,
            self.background_available,
            self.background_available,
        );
        definitions
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn snapshot(
        &self,
        model: &Model,
        chat_model: &Model,
        thinking: &ThinkingConfig,
        mode: &AgentMode,
        session_plan: bool,
        cwd: &str,
        history: &[Message],
    ) -> ContextSnapshot {
        let bindings = self.profiles.bind_for_tasks_for_inspection(
            model,
            chat_model,
            thinking,
            &self.model_policy,
        );
        let definitions = self.definitions(
            model,
            &template::env_vars().set("{cwd}", cwd),
            mode,
            session_plan,
            &bindings,
        );
        let mcp = self.mcp.clone().map(|mcp| {
            mcp.with_profile_policy(
                Arc::clone(&self.profile),
                ToolFilter::All.for_mcp_mode(mode),
            )
            .request_snapshot()
        });
        let full_tools = request_tools_from(
            &self.registry,
            history,
            definitions.declared.clone(),
            definitions.deferred.clone(),
            mcp.as_ref(),
        );
        let inventory = ContextInventory {
            builtins: BuiltinToolsInput {
                registry: &self.registry,
                filter: &ToolFilter::from_config(&self.config, model, &[]).for_mode(mode),
                config: &self.config,
                model,
                session_plan,
                audience: ToolAudience::MAIN,
                deferral: BuiltinDeferral::resolve(&self.config, model),
                deferred: &definitions.deferred,
            }
            .inventory(&self.profile),
            mcp: mcp
                .as_ref()
                .map(|mcp| ContextMcpInventory::from_statuses(mcp.tool_inventory()))
                .unwrap_or_default(),
            ..ContextInventory::default()
        };
        ContextSnapshot::capture(ContextCapture {
            readiness: ContextReadiness::PreparedNextRequest,
            mode,
            audience: ToolAudience::MAIN,
            model,
            auto_compact: false,
            compaction_buffer: None,
            system: "",
            base_tools: &DeferralSession::new(definitions.deferred, std::iter::empty())
                .accounting_definitions(&definitions.declared),
            full_tools: &full_tools,
            projected_messages: &[],
            measured: None,
            inventory,
        })
    }
}

pub(super) struct AgentLoop {
    model_slot: Arc<ArcSwap<ModelSlot>>,
    effective_model_slot: Arc<ArcSwap<ModelSlot>>,
    config: AgentConfig,
    tool_output_lines: ToolOutputLines,
    tool_output_store: Option<Arc<ToolOutputStore>>,
    vars: Vars,
    instructions: InstructionBaseline,
    memory: MemoryBaseline,
    /// Summarizes `memory`'s store while the loop lives. `None` when
    /// summaries are off or there is no memory.
    memory_pump: Option<Pump>,
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
    workflow: Option<WorkflowHandle>,
    background: Option<BackgroundTasks>,
    delivery_fence: Arc<DeliveryFence>,
    /// Published on every run so workflow agents start under the mode the
    /// user last committed, however long ago their run was launched.
    mode: SharedMode,
    /// The session plan binding the latest run carried, which keeps `plan`
    /// in the tools prepared between runs.
    plan: Option<PlanTarget>,
    /// Read when each run starts, so a run records for the session and
    /// workspace bound at that moment.
    change_recorder: RecorderSlot,
    workspace_session: Option<WorkspaceSession>,
    remote_project_context: Option<Arc<caudra_agent::remote_project_context::RemoteProjectContext>>,
    /// The directory Caudra itself runs in. Set only in a sandbox session,
    /// where `{cwd}` names a path inside the VM and this one does not.
    host_cwd: Option<PathBuf>,
    local_documents: Option<Arc<LocalDocumentStore>>,
}

impl AgentLoop {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        model_slot: Arc<ArcSwap<ModelSlot>>,
        effective_model_slot: Arc<ArcSwap<ModelSlot>>,
        config: AgentConfig,
        tool_output_lines: ToolOutputLines,
        tool_output_store: Option<Arc<ToolOutputStore>>,
        initial_history: Vec<HistoryItem>,
        archived_history: Vec<HistoryItem>,
        todos: Option<Vec<TodoItem>>,
        shared_history: SharedHistory,
        btw_prompt: SharedBtwPrompt,
        context_publisher: ContextPublisher,
        mcp_handle: Option<McpHandle>,
        permissions: Arc<PermissionManager>,
        agent_tx: flume::Sender<Envelope>,
        answer_rx: Arc<async_lock::Mutex<flume::Receiver<String>>>,
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
        workflow: Option<WorkflowHandle>,
        background: Option<BackgroundTasks>,
        delivery_fence: Arc<DeliveryFence>,
        mode: SharedMode,
        path_locks: Arc<PathLocks>,
        change_recorder: RecorderSlot,
        workspace_session: Option<WorkspaceSession>,
        remote_project_context: Option<
            Arc<caudra_agent::remote_project_context::RemoteProjectContext>,
        >,
        host_cwd: Option<PathBuf>,
        local_documents: Option<Arc<LocalDocumentStore>>,
    ) -> Self {
        let restored_history = History::restored(initial_history);
        let initial_messages = restored_history
            .as_ref()
            .map(|history| history.as_slice())
            .unwrap_or_default();
        let mcp = mcp_handle.map(|h| {
            McpSession::new(h, initial_messages)
                .with_disabled_tools(&config.disabled_tools)
                .with_profile_policy(
                    Arc::new(
                        system_prompt_profile
                            .as_ref()
                            .map(|profile| profile.tools().clone())
                            .unwrap_or_default(),
                    ),
                    ToolFilter::All.for_mcp_mode(&mode.load()),
                )
        });
        let (history, history_restore_error) = match restored_history {
            Ok(history) => (
                history
                    .with_mirror(shared_history)
                    .with_archived(archived_history)
                    .with_todos(todos),
                None,
            ),
            Err(error) => (History::default(), Some(error.to_string())),
        };
        Self {
            model_slot,
            effective_model_slot,
            config,
            tool_output_lines,
            tool_output_store,
            vars: Vars::default(),
            instructions: InstructionBaseline::default(),
            memory: MemoryBaseline::default(),
            memory_pump: None,
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
            path_locks,
            min_run_id: 0,
            agent_tx,
            answer_rx,
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
            workflow,
            background,
            delivery_fence,
            mode,
            plan: None,
            change_recorder,
            workspace_session,
            remote_project_context,
            host_cwd,
            local_documents,
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
                self.queue.hold_next_turn();
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
        let proceed = self.queue.finish_run(run_id, result.is_err());
        if let Err(error) = result {
            self.emit_error(run_id, error);
        }
        proceed
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
            self.queue.finish_run(run_id, false);
            return true;
        }
        let _ = event_tx.send(AgentEvent::QueueBatchConsumed { items: consumed });
        let result = self.do_agent_batch(inputs, event_tx, run_id, initial).await;
        let proceed = self.queue.finish_run(run_id, result.is_err());
        if let Err(error) = result {
            self.emit_error(run_id, error);
        }
        proceed
    }

    async fn initialize(&mut self) -> bool {
        self.vars = template::env_vars();
        if let Some(workspace) = &self.workspace_session {
            match caudra_agent::workspace_logical_cwd(workspace).await {
                Ok(cwd) => self.vars = self.vars.clone().set("{cwd}", cwd),
                Err(message) => {
                    self.emit_error(
                        self.min_run_id,
                        AgentError::Tool {
                            tool: "remote_project_context".into(),
                            message,
                        },
                    );
                    return false;
                }
            }
        }
        let initialized = self.reload_instructions().await;
        if let Err(error) = initialized {
            self.emit_error(self.min_run_id, error);
            return false;
        }
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
        let slot = self.effective_model_slot.load();
        self.rebuild_tools(
            &slot.model,
            &slot.model,
            &caudra_providers::ThinkingConfig::default(),
        );
        let tool_filter = self.effective_tool_filter();
        self.context_system = self.build_system(
            &caudra_agent::prompt::ResolvedSlots::default(),
            &tool_filter,
        );
        self.publish_btw_prompt(&slot, &self.context_system, RequestOptions::default());
        self.publish_prepared_context(&slot);
        !self.init_cancel.is_cancelled()
    }

    async fn do_compact(&mut self, event_tx: &EventSender) -> Result<(), AgentError> {
        let slot = self.model_slot.load_full();
        let (provider, model) = agent::resolve_compaction_model(
            &slot.provider,
            &slot.model,
            self.timeouts,
            &self.model_policy,
        )?;
        let extractor = agent::resolve_extractor(
            &self.config,
            &slot.provider,
            &slot.model,
            self.timeouts,
            &self.model_policy,
        )
        .await;
        let spend = agent::compact_with_session(
            &*provider,
            &model,
            &mut self.history,
            event_tx,
            &self.config,
            extractor.as_ref(),
            BackgroundReminderContext {
                background: self.background.as_ref(),
                jobs: None,
                workflow: self.workflow.as_ref(),
            },
        )
        .await?;
        self.goal.record_external_usage(
            spend.usage,
            model.billed_cost(&spend.usage, false),
            model.billing,
        );
        if let Some(extraction) = spend.extraction {
            self.goal
                .record_external_usage(extraction.usage, extraction.cost, extraction.billing);
        }
        let effective_slot = self.effective_model_slot.load();
        self.publish_prepared_context(&effective_slot);
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
        let automatic = inputs.iter().all(is_automatic_input);
        let peer_wake = is_peer_wake(&inputs);
        let Some(input) = inputs.last_mut() else {
            return Ok(());
        };
        let delivery_fence = Arc::clone(&self.delivery_fence);
        let _parent = delivery_fence.enter().await;
        if let Some(message) = delivery_fence.admission_error() {
            return Err(AgentError::Tool {
                tool: "session_stop".into(),
                message: message.into(),
            });
        }
        if !automatic && let Some(background) = &self.background {
            background.rearm();
        }
        let selected_slot = if automatic {
            self.effective_model_slot.load_full()
        } else {
            self.model_slot.load_full()
        };
        let purpose = model_purpose(&input.mode);
        let effective_slot = if !automatic && purpose == ModelPurpose::Plan {
            let (provider, model) = agent::resolve_model_for_purpose(
                agent::ModelRoute {
                    provider: &selected_slot.provider,
                    model: &selected_slot.model,
                },
                agent::ModelRoute {
                    provider: &selected_slot.provider,
                    model: &selected_slot.model,
                },
                purpose,
                None,
                self.timeouts,
                &self.model_policy,
            )
            .await?;
            Arc::new(ModelSlot { model, provider })
        } else {
            Arc::clone(&selected_slot)
        };

        let instructions = if self.workspace_session.is_some() {
            let current = self.read_instructions().await?;
            self.instructions.drift(current, self.history.epoch())
        } else {
            let old_cwd = self.vars.apply("{cwd}").into_owned();
            self.vars = template::env_vars();
            if *self.vars.apply("{cwd}") != old_cwd {
                self.reload_instructions().await?;
                None
            } else {
                let current = self.read_instructions().await?;
                self.instructions.drift(current, self.history.epoch())
            }
        };
        let memory = self
            .memory
            .refresh(
                &self.history,
                self.session_id.as_ref().map(SessionRef::as_str),
            )
            .await;
        self.mode.store(Arc::new(input.mode.clone()));
        self.plan = input.plan.clone();
        self.rebuild_tools(&effective_slot.model, &selected_slot.model, &input.thinking);
        self.effective_model_slot.store(Arc::clone(&effective_slot));

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
        let tool_filter = self.effective_tool_filter();
        let system = self.build_system(&prompt_slots, &tool_filter);
        self.context_system.clone_from(&system);
        self.context_options = opts.clone();
        self.publish_btw_prompt(&effective_slot, &system, opts.clone());
        let (trigger, cancel) = CancelToken::new();
        self.set_cancel_trigger(run_id, trigger);

        let mut agent = Agent::new(
            self.agent_params(&effective_slot, &selected_slot, prompt_slots, tool_filter),
            AgentRunParams {
                history: &mut self.history,
                system,
                environment: Some(agent::environment_block(&self.vars, &effective_slot.model)),
                instructions,
                memory,
                mode_notice: None,
                event_tx,
                tools: self.tools.clone(),
                deferred: self.deferred.clone(),
            },
        )
        .with_peer_checkpoint()
        .with_loaded_instructions(self.instructions.loaded().clone())
        .with_user_response_rx(Arc::clone(&self.answer_rx))
        .with_interrupt_source(Arc::clone(&self.queue) as Arc<dyn caudra_agent::InterruptSource>)
        .with_cancel(cancel)
        .with_retry_now(self.retry_now.clone())
        .with_goal(self.goal.clone())
        .with_mcp(self.mcp.clone());

        let result = if peer_wake {
            let Some(input) = inputs.pop() else {
                return Ok(());
            };
            agent.run_peer_wake(input).await
        } else if inputs.len() == 1 {
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
        if let Some(pump) = &self.memory_pump {
            pump.nudge();
        }

        self.clear_cancel_trigger(run_id);

        if matches!(result, Ok(DoneReason::Cancelled)) {
            self.min_run_id = run_id + 1;
        }

        result.map(|_| ())
    }

    fn agent_params(
        &self,
        effective_slot: &ModelSlot,
        selected_slot: &ModelSlot,
        prompt_slots: ResolvedSlots,
        tool_filter: ToolFilter,
    ) -> AgentParams {
        let active_prompt_profile_name: Arc<str> = Arc::from(
            self.system_prompt_profile
                .as_ref()
                .map_or(BUILTIN_PROFILE_NAME, |profile| profile.name()),
        );
        AgentParams {
            provider: Arc::clone(&effective_slot.provider),
            model: effective_slot.model.clone(),
            chat_provider: Arc::clone(&selected_slot.provider),
            chat_model: selected_slot.model.clone(),
            config: self.config.clone(),
            tool_output_lines: self.tool_output_lines,
            tool_output_store: self.tool_output_store.clone(),
            permissions: Arc::clone(&self.permissions),
            session_id: self.session_id.clone(),
            cache_key: self.session_id.as_ref().map(CacheKey::session),
            workspace_session: self.workspace_session.clone(),
            remote_project_context: self.remote_project_context.clone(),
            host_cwd: self.host_cwd.clone(),
            local_documents: self.local_documents.clone(),
            task_environment: template::env_vars(),
            root_tool_use_id: None,
            mailbox: self.mailbox.clone(),
            context_publisher: Some(self.context_publisher.clone()),
            timeouts: self.timeouts,
            file_tracker: Arc::clone(&self.file_tracker),
            path_locks: Arc::clone(&self.path_locks),
            changes: self.change_recorder.load_full().as_deref().cloned(),
            prompt_slots: Arc::new(prompt_slots),
            prompt_profiles: Arc::clone(&self.prompt_profiles),
            default_task_prompt_profile_name: Arc::clone(&active_prompt_profile_name),
            active_prompt_profile_name: Some(active_prompt_profile_name),
            subagent_cancels: Arc::clone(&self.subagent_cancels),
            subagent_history: self.subagent_history.clone(),
            registry: Arc::clone(ToolRegistry::global_arc()),
            audience: ToolAudience::MAIN,
            tool_ceiling: ToolFilter::ceiling_from_config(&self.config, &[]),
            profile_tool_policy: Arc::new(self.profile_tool_policy()),
            tool_filter,
            model_policy: Arc::clone(&self.model_policy),
            workflow: self.workflow.clone(),
            background: self.background.clone(),
            jobs: None,
            task_id: None,
        }
    }

    pub(super) fn task_host(&self) -> Result<WorkflowHostContext, String> {
        let selected_slot = self.model_slot.load();
        let mut params = self.agent_params(
            &self.effective_model_slot.load(),
            &selected_slot,
            self.lua_handle.collect_prompt_slots(&self.config),
            ToolFilter::ceiling_from_config(&self.config, &[]),
        );
        if let Some(workspace) = &self.workspace_session {
            let cwd = smol::block_on(caudra_agent::workspace_logical_cwd(workspace))?;
            params.task_environment = params.task_environment.set("{cwd}", cwd);
        }
        let loaded_instructions = match &self.remote_project_context {
            Some(context) => {
                agent::load_remote_instructions(context, self.host_cwd.as_deref()).loaded
            }
            None if self.workspace_session.is_some() => {
                return Err(TASK_CONTEXT_UNAVAILABLE.into());
            }
            None => agent::load_instructions(&params.task_environment.apply("{cwd}")).loaded,
        };
        Ok(WorkflowHostContext::from_agent_params(
            &params,
            HostExtras {
                mcp: self.mcp.clone(),
                loaded_instructions,
                user_response_rx: Some(Arc::clone(&self.answer_rx)),
            },
            Arc::new({
                let model_slot = Arc::clone(&self.effective_model_slot);
                move || {
                    let slot = model_slot.load();
                    (Arc::clone(&slot.provider), Arc::new(slot.model.clone()))
                }
            }),
            Arc::new({
                let mode = Arc::clone(&self.mode);
                move || AgentMode::clone(&mode.load())
            }),
            Arc::clone(&self.subagent_cancels),
        ))
    }

    /// Base tools only. MCP definitions are injected per request by
    /// `Agent::request_tools`; baking them here would freeze the catalog.
    fn rebuild_tools(
        &mut self,
        model: &Model,
        chat_model: &Model,
        thinking: &caudra_providers::ThinkingConfig,
    ) {
        self.mcp = self.mcp.take().map(|mcp| {
            mcp.with_profile_policy(
                Arc::new(self.profile_tool_policy()),
                ToolFilter::All.for_mcp_mode(&self.mode.load()),
            )
        });
        let definitions = self.build_tools(model, chat_model, thinking);
        self.tools = definitions.declared;
        self.deferred = definitions.deferred;
    }

    fn build_tools(
        &self,
        model: &Model,
        chat_model: &Model,
        thinking: &caudra_providers::ThinkingConfig,
    ) -> ToolDefinitions {
        let bindings = self.prompt_profiles.bind_for_tasks(
            model,
            chat_model,
            thinking,
            &self.model_policy,
            self.timeouts,
        );
        self.tools_preview_source().definitions(
            model,
            &self.vars,
            &self.mode.load(),
            self.has_session_plan(),
            &bindings,
        )
    }

    fn has_session_plan(&self) -> bool {
        self.mode.load().has_session_plan(self.plan.as_ref())
    }

    pub(super) fn tools_preview_source(&self) -> ToolsPreviewSource {
        ToolsPreviewSource {
            registry: Arc::clone(ToolRegistry::global_arc()),
            config: self.config.clone(),
            profile_name: Arc::from(
                self.system_prompt_profile
                    .as_deref()
                    .map_or(BUILTIN_PROFILE_NAME, SystemPromptProfile::name),
            ),
            profile: Arc::new(self.profile_tool_policy()),
            profiles: Arc::clone(&self.prompt_profiles),
            model_policy: Arc::clone(&self.model_policy),
            workflows_available: self.workflow.is_some(),
            background_available: self.background.is_some(),
            mcp: self.mcp.clone(),
        }
    }

    fn profile_tool_policy(&self) -> ProfileToolPolicy {
        self.system_prompt_profile
            .as_ref()
            .map(|profile| profile.tools().clone())
            .unwrap_or_default()
    }

    fn effective_tool_filter(&self) -> ToolFilter {
        ToolFilter::Only(
            self.tools
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                .chain(self.deferred.iter().map(|tool| tool.name.to_string()))
                .collect(),
        )
        .for_mode(&self.mode.load())
    }

    async fn read_instructions(&mut self) -> Result<Instructions, AgentError> {
        if let Some(workspace) = &self.workspace_session {
            let (context, cwd) = refresh_remote_context(
                workspace,
                self.remote_project_context.as_deref(),
                &self.permissions,
                self.workflow.as_ref(),
                &self.subagent_cancels,
                self.config.features,
            )
            .await?;
            let instructions = agent::load_remote_instructions(&context, self.host_cwd.as_deref());
            self.remote_project_context = Some(context);
            self.vars = self.vars.clone().set("{cwd}", cwd);
            return Ok(instructions);
        }
        let cwd = self.vars.apply("{cwd}").into_owned();
        Ok(smol::unblock(move || agent::load_instructions(&cwd)).await)
    }

    /// Rewrites the system prompt to match disk, so it is only free while the
    /// prefix cache is cold anyway: at startup, or on a change of directory,
    /// which can also put the session in another project's memory.
    async fn reload_instructions(&mut self) -> Result<(), AgentError> {
        let current = self.read_instructions().await?;
        self.instructions = InstructionBaseline::adopt(current, self.history.epoch());
        self.memory = MemoryBaseline::open(
            self.local_documents.clone(),
            self.tool_output_store
                .as_deref()
                .map(ToolOutputStore::state_dir)
                .cloned(),
            PathBuf::from(self.vars.apply("{cwd}").as_ref()),
            &self.history,
        )
        .await;
        self.restart_memory_pump();
        Ok(())
    }

    /// One pump per memory: a change of directory that kept the memory keeps
    /// its pump, and one that changed it replaces the pump.
    fn restart_memory_pump(&mut self) {
        let store = self.memory.store().filter(|_| self.config.summarize_memory);
        if let (Some(pump), Some(store)) = (&self.memory_pump, store)
            && Arc::ptr_eq(pump.store(), store)
        {
            return;
        }
        let slot = self.model_slot.load();
        self.memory_pump = store.map(|store| {
            Pump::start(
                Arc::clone(store),
                Arc::clone(&slot.provider),
                slot.model.clone(),
                Arc::clone(&self.model_policy),
                self.timeouts,
                EventSender::new(self.agent_tx.clone(), MEMORY_EVENT_RUN_ID),
                None,
            )
        });
    }

    /// The array a request actually carries. `self.tools` is the declared base, and a run
    /// appends the deferred catalog and then MCP on top, so anything that has to match the live
    /// array has to append them in that order. The loaded set comes from history, which is what
    /// the agent seeds its own session from. A load that happens mid-turn is not reflected until
    /// the next publish.
    fn request_tools(&self, mcp: Option<&McpRequestSnapshot>) -> Value {
        self.request_tools_from(self.tools.clone(), self.deferred.clone(), mcp)
    }

    fn request_tools_from(
        &self,
        tools: Value,
        deferred: Vec<DeferredTool>,
        mcp: Option<&McpRequestSnapshot>,
    ) -> Value {
        request_tools_from(
            ToolRegistry::global(),
            self.history.as_slice(),
            tools,
            deferred,
            mcp,
        )
    }

    fn build_system(
        &self,
        prompt_slots: &caudra_agent::prompt::ResolvedSlots,
        tool_filter: &ToolFilter,
    ) -> String {
        let prompt_slots = execution_slots(
            prompt_slots,
            &self.config,
            self.background.is_some(),
            self.background.is_some(),
            &self.tools,
            &self.deferred,
        );
        agent::build_system_prompt(
            self.instructions.text(),
            &prompt_slots,
            tool_filter,
            self.system_prompt_profile.as_deref(),
            self.memory.view(),
        )
    }

    /// Captures the prefix of the live request as it was built, never a
    /// rebuild of it: the route, system text, and tool array are the ones the
    /// run bound, so a `/btw` continues the same cached prefix and agrees with
    /// the environment reminder the transcript already carries for that model.
    fn publish_btw_prompt(&self, slot: &ModelSlot, system: &str, opts: RequestOptions) {
        let mcp = self.mcp.as_ref().map(McpSession::request_snapshot);
        self.btw_prompt.store(Arc::new(BtwPrompt {
            provider: Arc::clone(&slot.provider),
            model: slot.model.clone(),
            system: system.to_owned(),
            tools: self.request_tools(mcp.as_ref()),
            opts,
        }));
    }

    fn publish_prepared_context(&self, slot: &ModelSlot) {
        let mcp = self.mcp.as_ref().map(McpSession::request_snapshot);
        let tools = self.request_tools(mcp.as_ref());
        let messages = agent::project_request(
            self.history.as_slice(),
            &tools,
            &slot.model,
            slot.provider.reasoning_transport(&slot.model),
        );
        let options = self.context_options.clamped(&slot.model);
        let task_profiles = self.prompt_profiles.bind_for_tasks(
            &slot.model,
            &self.model_slot.load().model,
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
                filter: &self.effective_tool_filter(),
                config: &self.config,
                model: &slot.model,
                session_plan: self.has_session_plan(),
                audience: ToolAudience::MAIN,
                deferral: BuiltinDeferral::resolve(&self.config, &slot.model),
                deferred: &self.deferred,
            }),
            mcp.as_ref(),
        );
        self.context_publisher
            .publish(ContextSnapshot::capture(ContextCapture {
                readiness: ContextReadiness::PreparedNextRequest,
                mode: &self.mode.load(),
                audience: ToolAudience::MAIN,
                model: &slot.model,
                auto_compact: agent::auto_compact_enabled(),
                compaction_buffer: self.config.compaction_buffer,
                system: &self.context_system,
                base_tools: &DeferralSession::new(self.deferred.clone(), std::iter::empty())
                    .accounting_definitions(&self.tools),
                full_tools: &tools,
                projected_messages: messages.as_ref(),
                // Published between runs, when nothing the provider billed
                // describes this transcript: startup has yet to send a request,
                // and a manual compaction just replaced the one it had.
                measured: None,
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

fn request_tools_from(
    registry: &ToolRegistry,
    history: &[Message],
    mut tools: Value,
    deferred: Vec<DeferredTool>,
    mcp: Option<&McpRequestSnapshot>,
) -> Value {
    let mut sections: Vec<String> =
        DeferralSession::new(deferred, deferral::loaded_tool_names(history))
            .request_snapshot()
            .extend_declared(&mut tools)
            .into_iter()
            .collect();
    if let Some(mcp) = mcp {
        sections.extend(mcp.extend_declared(&mut tools));
    }
    deferral::push_unbound_catalog(
        &mut tools,
        &sections,
        registry.has(deferral::TOOL_SEARCH_TOOL_NAME),
    );
    tools
}

fn is_automatic_input(input: &AgentInput) -> bool {
    input.message.is_empty()
        && input.images.is_empty()
        && input.mentions.is_empty()
        && input.commits.is_empty()
        && input.prompt.is_none()
        && !input.resume
}

fn is_peer_wake(inputs: &[AgentInput]) -> bool {
    !inputs.is_empty()
        && inputs
            .iter()
            .all(|input| is_automatic_input(input) && input.preamble.is_empty())
}

fn model_purpose(mode: &AgentMode) -> ModelPurpose {
    match mode {
        AgentMode::Plan(_) | AgentMode::RemotePlan(_) => ModelPurpose::Plan,
        AgentMode::Build | AgentMode::ReadOnly => ModelPurpose::Chat,
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

#[cfg(test)]
pub(super) mod tests {
    use std::path::PathBuf;
    use std::slice::from_ref;

    use caudra_agent::McpPromptRef;
    use caudra_agent::context::{ContextBuiltinState, ContextMcpStatus};
    use caudra_agent::mcp::stub_session;
    use caudra_agent::tools::native::plan;
    use caudra_agent::tools::profile_policy::{
        MODE_DISABLED, PLAN_REQUIRED, PROFILE_DISABLED, PROFILE_LOADING,
    };
    use caudra_agent::tools::report::{REASON_CONFIG, REASON_PROFILE_LOADED};
    use caudra_agent::tools::{ToolEffect, ToolSource};
    use caudra_config::ProfileToolExposure;
    use caudra_providers::{ContentBlock, Role};
    use caudra_workspace::PlanRef;
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const PLAN_PATH: &str = ".caudra/plans/test.md";
    const USER_MESSAGE: &str = "explicit submission";
    const AUTOMATIC_REPORT: &str = "background result";
    const MCP_PROMPT: &str = "test/prompt";
    const PREVIEW_CWD: &str = "/preview-project";
    const PREVIEW_MODEL: &str = "anthropic/claude-sonnet-4-6";
    const PREVIEW_OWNER: &str = "caudra";
    const PREVIEW_CONTRACT: &str = "preview-test";
    const LOAD_PLAN_CALL: &str = "load-plan";
    const PREVIEW_MCP_TOOL: &str = "preview.lookup";
    const PREVIEW_MCP_DESCRIPTION: &str = "Look up preview data";
    const PREVIEW_PLAN_REF: &str = "plan-preview";

    pub(crate) fn tools_preview_source(
        config: AgentConfig,
        policy: ProfileToolPolicy,
    ) -> ToolsPreviewSource {
        let registry = Arc::new(ToolRegistry::new());
        registry
            .register_audited(
                Arc::new(plan::PlanTool),
                ToolSource::Native {
                    owner: PREVIEW_OWNER.into(),
                    contract: PREVIEW_CONTRACT.into(),
                    trusted: true,
                },
                ToolEffect::Mutating,
            )
            .unwrap();
        ToolsPreviewSource {
            registry,
            config,
            profile_name: BUILTIN_PROFILE_NAME.into(),
            profile: Arc::new(policy),
            profiles: Arc::new(PromptProfileCatalog::default()),
            model_policy: Arc::new(ModelPolicy::default()),
            workflows_available: false,
            background_available: false,
            mcp: None,
        }
    }

    #[test_case(ProfileToolExposure::Eager, false, ContextBuiltinState::Declared; "eager")]
    #[test_case(ProfileToolExposure::Lazy, false, ContextBuiltinState::Deferred; "lazy")]
    #[test_case(ProfileToolExposure::Lazy, true, ContextBuiltinState::Declared; "loaded_lazy")]
    #[test_case(ProfileToolExposure::Disabled, true, ContextBuiltinState::Disabled; "disabled_loaded")]
    fn tools_preview_uses_execution_policy_and_accounting(
        exposure: ProfileToolExposure,
        loaded: bool,
        expected: ContextBuiltinState,
    ) {
        let mut policy = ProfileToolPolicy::default();
        policy.overrides.insert(plan::NAME.into(), exposure);
        let source = tools_preview_source(AgentConfig::default(), policy);
        let model = Model::from_spec(PREVIEW_MODEL).unwrap();
        let history = if loaded {
            vec![Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    LOAD_PLAN_CALL,
                    plan::NAME,
                    json!({"action":"read"}),
                )],
                ..Message::default()
            }]
        } else {
            Vec::new()
        };
        for mode in [
            AgentMode::Plan(PLAN_PATH.into()),
            AgentMode::RemotePlan(PlanRef::new(PREVIEW_PLAN_REF).unwrap()),
            AgentMode::Build,
        ] {
            let snapshot = source.snapshot(
                &model,
                &model,
                &ThinkingConfig::Off,
                &mode,
                true,
                PREVIEW_CWD,
                &history,
            );
            assert_eq!(snapshot.mode, mode);
            assert_eq!(snapshot.audience, ToolAudience::MAIN);
            assert_eq!(snapshot.inventory.builtins.tools.len(), 1);
            let plan = &snapshot.inventory.builtins.tools[0];
            assert_eq!(plan.state, expected);
            assert_eq!(
                snapshot.inventory.builtins.request_tokens(),
                snapshot.usage.system_tools
            );
            assert_eq!(snapshot.usage.messages, 0);
            assert_eq!(snapshot.measured, None);
            if exposure == ProfileToolExposure::Disabled {
                assert_eq!(plan.reason, Some(PROFILE_DISABLED));
                assert_eq!(plan.tokens, 0);
            } else {
                assert!(plan.tokens > 0);
            }
        }
    }

    #[test_case(AgentMode::Build, false, PLAN_REQUIRED; "unbound_build")]
    #[test_case(AgentMode::ReadOnly, false, PLAN_REQUIRED; "missing_target")]
    #[test_case(AgentMode::Plan(PLAN_PATH.into()), true, REASON_CONFIG; "globally_disabled")]
    fn tools_preview_cannot_enable_ineligible_plan(mode: AgentMode, disabled: bool, reason: &str) {
        let config = AgentConfig {
            disabled_tools: disabled
                .then(|| plan::NAME.to_owned())
                .into_iter()
                .collect(),
            ..AgentConfig::default()
        };
        let policy = serde_json::from_value(json!({"overrides":{"plan":"eager"}})).unwrap();
        let source = tools_preview_source(config, policy);
        let model = Model::from_spec(PREVIEW_MODEL).unwrap();
        let snapshot = source.snapshot(
            &model,
            &model,
            &ThinkingConfig::Off,
            &mode,
            mode.has_session_plan(None),
            PREVIEW_CWD,
            &[],
        );
        assert_eq!(
            snapshot.inventory.builtins.tools[0].state,
            ContextBuiltinState::Disabled
        );
        assert_eq!(snapshot.inventory.builtins.tools[0].reason, Some(reason));
        assert_eq!(snapshot.inventory.builtins.tools[0].tokens, 0);
        assert_eq!(
            snapshot
                .inventory
                .builtins
                .count(ContextBuiltinState::Declared),
            0
        );
        assert_eq!(
            snapshot
                .inventory
                .builtins
                .count(ContextBuiltinState::Deferred),
            0
        );
    }

    #[test_case(AgentMode::Build, ProfileToolExposure::Eager, false, ContextMcpStatus::LoadedOrEager, PROFILE_LOADING; "eager_does_not_load_source")]
    #[test_case(AgentMode::Build, ProfileToolExposure::Lazy, false, ContextMcpStatus::AvailableOnDemand, PROFILE_LOADING; "lazy_catalog")]
    #[test_case(AgentMode::Build, ProfileToolExposure::Lazy, true, ContextMcpStatus::LoadedOrEager, REASON_PROFILE_LOADED; "loaded_lazy")]
    #[test_case(AgentMode::Build, ProfileToolExposure::Disabled, true, ContextMcpStatus::Disabled, PROFILE_DISABLED; "profile_excludes_loaded")]
    #[test_case(AgentMode::ReadOnly, ProfileToolExposure::Eager, true, ContextMcpStatus::Disabled, MODE_DISABLED; "read_only_excludes_eager")]
    #[test_case(AgentMode::Plan(PLAN_PATH.into()), ProfileToolExposure::Lazy, true, ContextMcpStatus::Disabled, MODE_DISABLED; "local_plan_excludes_loaded")]
    #[test_case(AgentMode::RemotePlan(PlanRef::new(PREVIEW_PLAN_REF).unwrap()), ProfileToolExposure::Eager, false, ContextMcpStatus::Disabled, MODE_DISABLED; "remote_plan_excludes_eager")]
    fn tools_preview_narrows_mcp_without_changing_source_loads(
        mode: AgentMode,
        exposure: ProfileToolExposure,
        loaded: bool,
        expected: ContextMcpStatus,
        reason: &'static str,
    ) {
        let session = stub_session(&[(PREVIEW_MCP_TOOL, PREVIEW_MCP_DESCRIPTION)]);
        if loaded {
            assert!(session.mark_loaded(PREVIEW_MCP_TOOL));
        }
        let before = session.request_snapshot();
        assert_eq!(before.tool_inventory()[0].deferred, !loaded);
        let mut policy = ProfileToolPolicy::default();
        policy
            .overrides
            .insert(plan::NAME.into(), ProfileToolExposure::Disabled);
        policy.overrides.insert(PREVIEW_MCP_TOOL.into(), exposure);
        let mut source = tools_preview_source(AgentConfig::default(), policy);
        source.mcp = Some(session);
        let model = Model::from_spec(PREVIEW_MODEL).unwrap();
        let snapshot = source.snapshot(
            &model,
            &model,
            &ThinkingConfig::Off,
            &mode,
            mode.has_session_plan(None),
            PREVIEW_CWD,
            &[],
        );
        assert_eq!(snapshot.inventory.mcp.tools.len(), 1);
        let tool = &snapshot.inventory.mcp.tools[0];
        assert_eq!(tool.qualified_name, PREVIEW_MCP_TOOL);
        assert_eq!(tool.status, expected);
        assert_eq!(tool.reason, Some(reason));
        assert_eq!(
            tool.request_tokens > 0,
            expected == ContextMcpStatus::LoadedOrEager
        );
        assert_eq!(snapshot.usage.mcp_tools, tool.request_tokens);
        assert_eq!(
            snapshot.inventory.builtins.catalog_tokens > 0,
            expected == ContextMcpStatus::AvailableOnDemand
        );
        assert_eq!(
            snapshot.usage.system_tools,
            snapshot.inventory.builtins.request_tokens()
        );
        let after = source.mcp.as_ref().unwrap().request_snapshot();
        assert_eq!(after.tool_inventory(), before.tool_inventory());
        let mut before_tools = json!([]);
        let mut after_tools = json!([]);
        before.extend_tools(&mut before_tools);
        after.extend_tools(&mut after_tools);
        assert_eq!(after_tools, before_tools);
    }

    #[test_case("", false, false, None, true, true; "peer_wake")]
    #[test_case("", false, false, Some(Message::observation(AUTOMATIC_REPORT.into())), true, false; "automatic_report")]
    #[test_case("", false, false, Some(Message::synthetic(AUTOMATIC_REPORT.into())), true, false; "goal_checkin")]
    #[test_case(USER_MESSAGE, false, false, None, false, false; "explicit_message")]
    #[test_case("", true, false, None, false, false; "explicit_resume")]
    #[test_case("", false, true, None, false, false; "explicit_mcp_prompt")]
    fn only_automatic_input_uses_committed_route(
        message: &str,
        resume: bool,
        prompt: bool,
        preamble: Option<Message>,
        automatic: bool,
        peer_wake: bool,
    ) {
        let input = || AgentInput {
            message: message.into(),
            mode: AgentMode::Build,
            plan: None,
            images: Vec::new(),
            mentions: Vec::new(),
            commits: Vec::new(),
            preamble: preamble.clone().into_iter().collect(),
            thinking: Default::default(),
            fast: false,
            prompt: prompt.then(|| {
                Box::new(McpPromptRef {
                    qualified_name: MCP_PROMPT.into(),
                    arguments: Default::default(),
                })
            }),
            resume,
        };
        assert_eq!(is_automatic_input(&input()), automatic);
        assert_eq!(is_peer_wake(from_ref(&input())), peer_wake);
        assert_eq!(is_peer_wake(&[input(), input()]), peer_wake);
        let mut local_input = input();
        local_input.message = USER_MESSAGE.into();
        assert!(!is_peer_wake(&[input(), local_input]));
    }

    #[test]
    fn empty_input_batch_is_not_a_peer_wake() {
        assert!(!is_peer_wake(&[]));
    }

    #[test_case(AgentMode::Build, ModelPurpose::Chat ; "build_uses_chat")]
    #[test_case(AgentMode::ReadOnly, ModelPurpose::Chat ; "read_only_uses_chat")]
    #[test_case(AgentMode::Plan(PathBuf::from(PLAN_PATH)), ModelPurpose::Plan ; "plan_uses_plan")]
    fn input_mode_selects_model_purpose(mode: AgentMode, purpose: ModelPurpose) {
        assert_eq!(model_purpose(&mode), purpose);
    }
}
