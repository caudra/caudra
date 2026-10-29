use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Instant;
use std::{env, sync::Arc};

use crate::app::background_delivery::DeliveryFence;
use crate::app::file_revert::RecorderSlot;
use arc_swap::{ArcSwap, ArcSwapOption};
use caudra_agent::agent;
use caudra_agent::agent::cache_warming::{CacheWarmer, WarmStop, WarmerContext, WarmerRef};
use caudra_agent::agent::task_runner::{HostExtras, ModelResolver, WorkflowHostContext};
use caudra_agent::agent::{
    ActiveContext, ContextRefresh, PrefixKey, RefreshedContext, StandingState, Surface,
    SurfaceBaseline,
};
use caudra_agent::background::BackgroundTasks;
use caudra_agent::code::CodeSession;
use caudra_agent::context::{
    BuiltinToolsInput, ContextCapture, ContextInventory, ContextMcpInventory, ContextPublisher,
    ContextReadiness, ContextSnapshot,
};
use caudra_agent::learning::runner::Spend;
use caudra_agent::learning::{Learner, LearnerDeps, LearnerHandle};
use caudra_agent::mcp::config::McpServerStatus;
use caudra_agent::mcp::elicitor::QuestionForms;
use caudra_agent::mcp::{McpHandle, McpSession, prompt_preamble};
use caudra_agent::memory::baseline::MemoryBaseline;
use caudra_agent::memory::compactor::Pump;
use caudra_agent::permissions::PermissionManager;
use caudra_agent::prompt::ResolvedSlots;
use caudra_agent::prompt::profile::{
    BUILTIN_PROFILE_NAME, PromptProfileCatalog, SystemPromptProfile, TaskProfileBindings,
};
use caudra_agent::remote_project_context::RemoteProjectContext;
use caudra_agent::template;
use caudra_agent::template::Vars;
use caudra_agent::tools::execution::{configure_declared, execution_slots};
use caudra_agent::tools::native::plan::PlanTarget;
use caudra_agent::tools::{
    DescriptionContext, FileReadTracker, PathLocks, TASK_TOOL_NAME, ToolAudience, ToolDefinitions,
    ToolFilter, ToolRegistry,
};
use caudra_agent::types::{LEARNING_EVENT_RUN_ID, MEMORY_EVENT_RUN_ID, TodoItem};
use caudra_agent::workflow::WorkflowHandle;
use caudra_agent::{
    Agent, AgentConfig, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams,
    BackgroundReminderContext, CancelMap, CancelToken, CancelTrigger, ConversationControl,
    DoneReason, Envelope, EventSender, GoalHandle, History, InstructionBaseline, Instructions,
    McpCommand, SessionMailbox, SharedHistory, SubagentHistoryStore, ToolOutputLines,
};
use caudra_config::{ModelPolicy, ProfileToolPolicy};
use caudra_lua::EventHandle;
use caudra_providers::{
    AgentError, Billing, CacheKey, HistoryItem, Model, ModelPurpose, RequestOptions,
    ThinkingConfig, Timeouts,
};
use caudra_storage::id::SessionRef;
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_storage::media::MediaStore;
use caudra_storage::tool_outputs::ToolOutputStore;
use caudra_workspace::WorkspaceSession;
use serde_json::Value;
use tracing::{error, warn};

use super::cancel_map::RunCancelMap;
use super::settings::{LiveSettings, Settings};
use super::shared_queue::{QueueItem, QueueReceiver};
use super::workflow::SharedMode;
use super::{BtwPrompt, ModelSlot, SharedBtwPrompt, refresh_remote_context};

const TASK_CONTEXT_UNAVAILABLE: &str = "Remote project context unavailable for task continuation";

struct CompactionRefresh<'a> {
    vars: &'a mut Vars,
    instructions: &'a mut InstructionBaseline,
    memory: &'a mut MemoryBaseline,
    surface: &'a mut SurfaceBaseline,
    tools: &'a mut Value,
    hosted: &'a mut Vec<String>,
    bound_profile: &'a mut Option<Arc<SystemPromptProfile>>,
    remote_project_context: &'a mut Option<Arc<RemoteProjectContext>>,
    context_system: &'a mut String,
    context_options: &'a mut RequestOptions,
    source: ToolsPreviewSource,
    profile: Option<Arc<SystemPromptProfile>>,
    workspace: Option<&'a WorkspaceSession>,
    host_cwd: Option<&'a Path>,
    permissions: &'a PermissionManager,
    workflow: Option<&'a WorkflowHandle>,
    subagent_cancels: &'a CancelMap<String>,
    local_documents: Option<Arc<LocalDocumentStore>>,
    tool_output_store: Option<&'a ToolOutputStore>,
    session_id: Option<&'a SessionRef>,
    lua_handle: &'a EventHandle,
    timeouts: Timeouts,
    chat_model: Model,
    btw_prompt: &'a SharedBtwPrompt,
}

impl ContextRefresh for CompactionRefresh<'_> {
    fn refresh<'a>(
        &'a mut self,
        history: &'a History,
        active: ActiveContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<RefreshedContext, AgentError>> + Send + 'a>> {
        Box::pin(async move {
            let mut vars =
                refreshed_vars(self.vars, template::env_vars(), self.workspace.is_some());
            let (current, remote) = if let Some(workspace) = self.workspace {
                let (context, cwd) = refresh_remote_context(
                    workspace,
                    self.remote_project_context.as_deref(),
                    self.permissions,
                    self.workflow,
                    self.subagent_cancels,
                    self.source.config.features,
                )
                .await?;
                vars = vars.set("{cwd}", cwd);
                (
                    agent::load_remote_instructions(&context, self.host_cwd),
                    Some(context),
                )
            } else {
                let cwd = vars.apply("{cwd}").into_owned();
                (
                    smol::unblock(move || agent::load_instructions(&cwd)).await,
                    None,
                )
            };
            let instructions = InstructionBaseline::adopt(current, history.epoch());
            let instructions_notice = restored_notice(
                history,
                caudra_agent::prompt::INSTRUCTIONS_CHANGED_MARKER,
                caudra_agent::prompt::INSTRUCTIONS_RESTORED_PROMPT,
            );
            let relocated = vars.apply("{cwd}") != self.vars.apply("{cwd}");
            let mut memory = if relocated {
                MemoryBaseline::open(
                    self.local_documents.clone(),
                    self.tool_output_store
                        .map(ToolOutputStore::state_dir)
                        .cloned(),
                    PathBuf::from(vars.apply("{cwd}").as_ref()),
                    history,
                )
                .await
            } else {
                self.memory.clone()
            };
            let memory_notice = memory
                .refresh(history, self.session_id.map(SessionRef::as_str), true)
                .await
                .or_else(|| {
                    restored_notice(
                        history,
                        caudra_agent::prompt::MEMORY_UPDATED_MARKER,
                        if memory.view().is_some() {
                            caudra_agent::prompt::MEMORY_REFRESHED_PROMPT
                        } else {
                            caudra_agent::prompt::MEMORY_UNAVAILABLE_PROMPT
                        },
                    )
                });
            let bindings = self.source.profiles.bind_for_tasks(
                active.model,
                &self.chat_model,
                &active.options.thinking,
                &self.source.model_policy,
                self.timeouts,
            );
            let definitions = self.source.definitions(
                active.model,
                &vars,
                active.mode,
                active.mode.has_session_plan(active.plan),
                &bindings,
            );
            let tool_filter =
                session_tool_filter(&definitions.declared, &definitions.hosted, active.mode);
            let slots = self
                .lua_handle
                .collect_prompt_slots_async(&self.source.config)
                .await;
            let slots = execution_slots(
                &slots,
                &self.source.config,
                self.source.background_available,
                self.source.background_available,
                &definitions.declared,
            );
            let system = agent::build_system_prompt(
                instructions.text(),
                &slots,
                &tool_filter,
                self.profile.as_deref(),
                memory.view(),
            );
            let mut surface = self.surface.clone();
            let settled = surface.settle(
                Surface::new(system.clone(), definitions.declared.clone()),
                true,
                PrefixKey {
                    model: active.model.spec(),
                    chat_model: self.chat_model.spec(),
                    thinking: active.options.thinking.clone(),
                    mode: active.mode.clone(),
                    plan: active.plan.cloned(),
                    cwd: vars.apply("{cwd}").into_owned(),
                },
                history.epoch(),
                history.as_slice(),
            );
            let refreshed = RefreshedContext {
                system: system.clone(),
                environment: Some(agent::environment_block(&vars, active.model)),
                instructions: instructions_notice,
                memory: memory_notice,
                configuration: settled.notice,
                tools: definitions.declared.clone(),
                tool_filter,
                loaded_instructions: instructions.loaded().clone(),
                remote_project_context: remote.clone(),
                task_environment: vars.clone(),
                memory_store: memory.store().cloned(),
            };
            *self.vars = vars;
            *self.instructions = instructions;
            *self.memory = memory;
            *self.surface = surface;
            *self.tools = definitions.declared;
            *self.hosted = definitions.hosted;
            self.bound_profile.clone_from(&self.profile);
            *self.remote_project_context = remote;
            *self.context_system = system.clone();
            *self.context_options = active.options.clone();
            self.btw_prompt.store(Arc::new(BtwPrompt {
                provider: Arc::clone(active.provider),
                model: active.model.clone(),
                system,
                tools: self.tools.clone(),
                opts: active.options.clone(),
                profile: self.profile.clone(),
            }));
            Ok(refreshed)
        })
    }
}

fn refreshed_vars(previous: &Vars, current: Vars, remote: bool) -> Vars {
    if remote {
        previous
            .clone()
            .set("{date}", current.apply("{date}").into_owned())
    } else {
        current
    }
}

fn restored_notice(history: &History, marker: &str, restored: &str) -> Option<String> {
    agent::last_announced(history.as_slice(), &[marker])
        .is_some_and(|text| text != restored)
        .then(|| restored.to_owned())
}

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
            &self.profile,
            session_plan,
        );
        configure_declared(
            &mut definitions.declared,
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
        });
        let inventory = ContextInventory {
            builtins: BuiltinToolsInput {
                registry: &self.registry,
                filter: &ToolFilter::from_config(&self.config, model, &[]).for_mode(mode),
                config: &self.config,
                model,
                session_plan,
                audience: ToolAudience::MAIN,
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
            tools: &definitions.declared,
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
    settings: LiveSettings,
    tool_output_lines: ToolOutputLines,
    tool_output_store: Option<Arc<ToolOutputStore>>,
    vars: Vars,
    instructions: InstructionBaseline,
    memory: MemoryBaseline,
    /// Summarizes `memory`'s store while the loop lives. `None` when
    /// summaries are off or there is no memory.
    memory_pump: Option<Pump>,
    /// Continual learning for the session's project. `None` when learning is
    /// off, the session is not stored, or it works in a remote workspace.
    learner: Option<Learner>,
    /// What `/learn` asks for a run through, swapped with the learner.
    learner_handle: Arc<ArcSwapOption<LearnerHandle>>,
    tools: Value,
    /// Tools a `code` module dispatches, absent from `tools` yet available.
    hosted: Vec<String>,
    /// The tools and system prompt every request of the conversation shares,
    /// kept while the configuration changes under them.
    surface: SurfaceBaseline,
    /// The profile the fixed system prompt was built from, which only a
    /// rebuild moves on.
    bound_profile: Option<Arc<SystemPromptProfile>>,
    mcp: Option<McpSession>,
    code: CodeSession,
    history: History,
    history_restore_error: Option<String>,
    btw_prompt: SharedBtwPrompt,
    context_publisher: ContextPublisher,
    context_system: String,
    context_options: RequestOptions,
    cancel_map: Arc<RunCancelMap>,
    control: ConversationControl,
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
    /// Shared by every run, so idle warming outlives the run that armed it.
    /// Dropped with the loop, which a revert, a clear or a session switch
    /// replaces, ending its episode.
    cache_warmer: CacheWarmer,
}

impl AgentLoop {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        model_slot: Arc<ArcSwap<ModelSlot>>,
        effective_model_slot: Arc<ArcSwap<ModelSlot>>,
        config: AgentConfig,
        settings: LiveSettings,
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
        control: ConversationControl,
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
        let code = CodeSession::new(initial_messages);
        let mcp = mcp_handle.map(|h| {
            McpSession::new(h)
                .with_disabled_tools(&config.disabled_tools)
                .with_elicitors(Some(Arc::new(QuestionForms)))
                .with_profile_policy(
                    Arc::new(
                        system_prompt_profile
                            .as_ref()
                            .map(|profile| profile.tools().clone())
                            .unwrap_or_default(),
                    ),
                    ToolFilter::All,
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
        let cache_warmer = CacheWarmer::new(
            config.cache_warming,
            WarmerContext {
                session_id: session_id
                    .as_ref()
                    .map(|id| id.id().to_string())
                    .unwrap_or_default(),
                subagent: None,
                cwd: template::env_vars().apply("{cwd}").into_owned(),
            },
        );
        Self {
            model_slot,
            effective_model_slot,
            config,
            settings,
            tool_output_lines,
            tool_output_store,
            vars: Vars::default(),
            instructions: InstructionBaseline::default(),
            memory: MemoryBaseline::default(),
            memory_pump: None,
            learner: None,
            learner_handle: Arc::default(),
            tools: Value::Null,
            hosted: Vec::new(),
            surface: SurfaceBaseline::default(),
            bound_profile: system_prompt_profile.clone(),
            mcp,
            code,
            history,
            history_restore_error,
            btw_prompt,
            context_publisher,
            context_system: String::new(),
            context_options: RequestOptions::default(),
            cancel_map,
            control,
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
            cache_warmer,
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
            self.adopt_pending().await;
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
                input,
                displayed,
                ..
            } => {
                if !displayed {
                    let _ = event_tx.send(AgentEvent::QueueItemConsumed {
                        id,
                        text,
                        images: input.images.clone(),
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
                images: input.images.clone(),
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
        // A restored PDF stays attached to the request, and so counts toward
        // the gauge, only once its bytes are loaded.
        if let Some(outputs) = &self.tool_output_store {
            self.history
                .load_media(&MediaStore::new(outputs.state_dir()))
                .await;
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
        self.cache_warmer.stop(WarmStop::PrefixChanged);
        self.adopt_settings();
        let slot = self.model_slot.load_full();
        let effective_slot = self.effective_model_slot.load_full();
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
        let retention = agent::Retention::new(&self.permissions, &self.goal, &self.history);
        event_tx.send(AgentEvent::Compacting)?;
        let mut prepared = agent::prepare_compaction(
            &*provider,
            &model,
            &self.history,
            event_tx,
            &self.config,
            extractor.as_ref(),
            retention,
            &CancelToken::none(),
            &self.control.retry_now,
        )
        .await?;
        let spend = prepared.spend;
        let cost = model.billed_cost(&spend.usage, false);
        self.goal
            .record_external_usage(spend.usage, cost, model.billing);
        self.record_autopilot_spend(event_tx, cost, model.billing);
        if let Some(extraction) = spend.extraction {
            self.goal
                .record_external_usage(extraction.usage, extraction.cost, extraction.billing);
            self.record_autopilot_spend(event_tx, extraction.cost, extraction.billing);
        }
        prepared.validate()?;
        self.adopt_settings();
        let listed = if self.config.task_model_choice {
            agent::favorite_models(&self.model_policy).await
        } else {
            None
        };
        let system = self.context_system.clone();
        let tools = self.tools.clone();
        let tool_filter = self.effective_tool_filter();
        let mode = self.mode.load_full();
        let plan = self.plan.clone();
        let options = self.context_options.clone();
        let (_, mut refresh) = self.compaction_parts(slot.model.clone());
        let refreshed = refresh
            .refresh(
                &prepared.history,
                ActiveContext {
                    provider: &effective_slot.provider,
                    model: &effective_slot.model,
                    system: &system,
                    tools: &tools,
                    tool_filter: &tool_filter,
                    mode: &mode,
                    plan: plan.as_ref(),
                    options: &options,
                },
            )
            .await?;
        drop(refresh);
        self.mcp = self.mcp.take().map(|mcp| {
            mcp.with_profile_policy(Arc::new(self.profile_tool_policy()), ToolFilter::All)
        });
        let mcp = self.mcp.clone().map(|mcp| {
            if refreshed.tool_filter.is_read_only() {
                mcp.narrowed(&ToolFilter::ReadOnly(Box::new(ToolFilter::All)))
            } else {
                mcp
            }
        });
        let reminders = StandingState {
            environment: refreshed.environment.as_deref(),
            instructions: refreshed.instructions.as_deref(),
            memory: refreshed.memory.as_deref(),
            configuration: refreshed.configuration.as_deref(),
            mode_notice: None,
            mode: &mode,
            autopilot: self
                .permissions
                .autopilot()
                .map(|handle| handle.is_engaged()),
            favorites: self.config.task_model_choice
                && refreshed.tool_filter.matches(TASK_TOOL_NAME),
            model_policy: &self.model_policy,
            mcp: mcp.as_ref(),
            decisions: self.permissions.decisions().is_some(),
            tools: &refreshed.tool_filter,
        }
        .collect_with_favorites(prepared.history.as_slice(), listed);
        prepared.refresh_reminders(reminders);
        prepared.finish(
            &mut self.history,
            event_tx,
            &self.config,
            BackgroundReminderContext {
                background: self.background.as_ref(),
                jobs: None,
                workflow: self.workflow.as_ref(),
            },
        )?;
        agent::append_post_compaction_instructions(&mut self.history, event_tx, &self.config)?;
        self.restart_memory_pump();
        self.publish_btw_prompt(
            &effective_slot,
            &self.context_system,
            self.context_options.clone(),
        );
        self.publish_prepared_context(&effective_slot);
        event_tx.send(AgentEvent::Done {
            usage: spend.usage,
            num_turns: 1,
            reason: DoneReason::EndTurn,
        })?;
        Ok(())
    }

    fn compaction_parts(&mut self, chat_model: Model) -> (&mut History, CompactionRefresh<'_>) {
        let source = self.tools_preview_source();
        (
            &mut self.history,
            CompactionRefresh {
                vars: &mut self.vars,
                instructions: &mut self.instructions,
                memory: &mut self.memory,
                surface: &mut self.surface,
                tools: &mut self.tools,
                hosted: &mut self.hosted,
                bound_profile: &mut self.bound_profile,
                remote_project_context: &mut self.remote_project_context,
                context_system: &mut self.context_system,
                context_options: &mut self.context_options,
                source,
                profile: self.system_prompt_profile.clone(),
                workspace: self.workspace_session.as_ref(),
                host_cwd: self.host_cwd.as_deref(),
                permissions: &self.permissions,
                workflow: self.workflow.as_ref(),
                subagent_cancels: &self.subagent_cancels,
                local_documents: self.local_documents.clone(),
                tool_output_store: self.tool_output_store.as_deref(),
                session_id: self.session_id.as_ref(),
                lua_handle: &self.lua_handle,
                timeouts: self.timeouts,
                chat_model,
                btw_prompt: &self.btw_prompt,
            },
        )
    }

    /// Spend outside the agent's own turns, which an engaged autopilot counts
    /// as the goal does. A limit it reaches ends autopilot as the agent's own
    /// spend would.
    fn record_autopilot_spend(&self, event_tx: &EventSender, cost: Option<f64>, billing: Billing) {
        if let Some(ended) = self
            .permissions
            .autopilot()
            .and_then(|autopilot| autopilot.record_spend(cost, billing, &autopilot.limits()))
        {
            event_tx.try_send(ended);
        }
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
        self.adopt_settings();
        let (selected_slot, effective_slot) = self.route(&input.mode, automatic).await?;

        let relocated = self.workspace_session.is_none() && {
            let old_cwd = self.vars.apply("{cwd}").into_owned();
            self.vars = template::env_vars();
            *self.vars.apply("{cwd}") != old_cwd
        };
        if relocated {
            self.reload_instructions().await?;
        }
        let current = if relocated {
            None
        } else {
            Some(self.read_instructions().await?)
        };
        let key = PrefixKey {
            model: effective_slot.model.spec(),
            chat_model: selected_slot.model.spec(),
            thinking: input.thinking.clone(),
            mode: input.mode.clone(),
            plan: input.plan.clone(),
            cwd: self.vars.apply("{cwd}").into_owned(),
        };
        let epoch = self.history.epoch();
        let rebuild = self.surface.stale(
            &key,
            epoch,
            effective_slot
                .provider
                .prompt_cache_ttl(&effective_slot.model),
            Instant::now(),
        );
        let instructions = current.and_then(|current| {
            if rebuild {
                self.instructions.rebase(current, epoch)
            } else {
                self.instructions.drift(current, epoch)
            }
        });
        let memory = self
            .memory
            .refresh(
                &self.history,
                self.session_id.as_ref().map(SessionRef::as_str),
                rebuild,
            )
            .await;
        self.install(input, &effective_slot, &selected_slot);

        if let Some(ref prompt_ref) = input.prompt {
            let messages = prompt_preamble(
                self.mcp.as_deref(),
                prompt_ref,
                effective_slot.model.supports_vision(),
            )
            .await?;
            input.preamble.extend(messages);
        }

        let opts = RequestOptions {
            thinking: input.thinking.clone(),
            fast: input.fast,
            ..RequestOptions::default()
        };
        let prompt_slots = self
            .lua_handle
            .collect_prompt_slots_async(&self.config)
            .await;
        let live_system = self.build_system(&prompt_slots, &self.effective_tool_filter());
        let (system, retired, configuration) = self.fix_surface(live_system, rebuild, key);
        let retired: Vec<&str> = retired.iter().map(String::as_str).collect();
        let tool_filter = self.effective_tool_filter().excluding(&retired);
        self.context_system.clone_from(&system);
        self.context_options = opts.clone();
        self.publish_btw_prompt(&effective_slot, &system, opts.clone());
        let (trigger, cancel) = CancelToken::new();
        self.set_cancel_trigger(run_id, trigger);
        let model_slot = Arc::clone(&self.model_slot);
        let selected_spec = selected_slot.model.spec();
        self.cache_warmer.set_guard(Arc::new(move || {
            model_slot.load().model.spec() == selected_spec
        }));

        if let Some(learner) = &self.learner {
            learner.hold();
        }
        let params = self.agent_params(&effective_slot, &selected_slot, prompt_slots, tool_filter);
        let environment = Some(agent::environment_block(&self.vars, &effective_slot.model));
        let tools = self.tools.clone();
        let memory_store = self.memory.store().cloned();
        let loaded_instructions = self.instructions.loaded().clone();
        let answer_rx = Arc::clone(&self.answer_rx);
        let queue = Arc::clone(&self.queue) as Arc<dyn caudra_agent::InterruptSource>;
        let retry_now = self.control.retry_now.clone();
        // A pick made after the last run settled is already in the slots this
        // run starts from, and must not override one made since.
        drop(self.control.live.take_pending());
        let live = self.control.live.clone();
        let goal = self.goal.clone();
        let mcp = self.mcp.clone();
        let code = self.code.clone();
        let cache_warmer = self.cache_warmer.clone();
        let autopilot = self.permissions.autopilot();
        let (history, mut refresh) = self.compaction_parts(selected_slot.model.clone());
        let mut agent = Agent::new(
            params,
            AgentRunParams {
                history,
                system,
                environment,
                instructions,
                memory,
                configuration,
                mode_notice: None,
                event_tx,
                tools,
                tools_fixed: true,
                memory_store,
            },
        )
        .with_peer_checkpoint()
        .with_compaction_refresh(&mut refresh)
        .with_loaded_instructions(loaded_instructions)
        .with_user_response_rx(answer_rx)
        .with_interrupt_source(queue)
        .with_cancel(cancel)
        .with_retry_now(retry_now)
        .with_live_switch(live)
        .with_goal(goal)
        .with_mcp(mcp)
        .with_code(code)
        .with_home_model(selected_slot.home.clone())
        .with_cache_warmer(cache_warmer);
        if let Some(autopilot) = autopilot {
            agent = agent.with_autopilot(autopilot);
        }

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
        drop(refresh);
        self.surface.touch(Instant::now());
        self.restart_memory_pump();
        if let Some(pump) = &self.memory_pump {
            pump.nudge();
        }
        if let Some(learner) = &self.learner {
            learner.nudge();
        }

        self.clear_cancel_trigger(run_id);

        if matches!(result, Ok(DoneReason::Cancelled)) {
            self.min_run_id = run_id + 1;
        }

        result.map(|_| ())
    }

    /// The selected slot and the one a request in `mode` runs on. An automatic
    /// continuation keeps the route already in force.
    async fn route(
        &self,
        mode: &AgentMode,
        automatic: bool,
    ) -> Result<(Arc<ModelSlot>, Arc<ModelSlot>), AgentError> {
        let selected_slot = if automatic {
            self.effective_model_slot.load_full()
        } else {
            self.model_slot.load_full()
        };
        let purpose = model_purpose(mode);
        if automatic || purpose != ModelPurpose::Plan {
            return Ok((Arc::clone(&selected_slot), selected_slot));
        }
        let selected = || agent::ModelRoute {
            provider: &selected_slot.provider,
            model: &selected_slot.model,
        };
        let (provider, model) = agent::resolve_model_for_purpose(
            selected(),
            selected(),
            purpose,
            None,
            self.timeouts,
            &self.model_policy,
        )
        .await?;
        Ok((
            selected_slot,
            Arc::new(ModelSlot {
                model,
                provider,
                home: None,
            }),
        ))
    }

    fn install(
        &mut self,
        input: &AgentInput,
        effective_slot: &Arc<ModelSlot>,
        selected: &ModelSlot,
    ) {
        self.mode.store(Arc::new(input.mode.clone()));
        self.plan = input.plan.clone();
        self.rebuild_tools(&effective_slot.model, &selected.model, &input.thinking);
        self.effective_model_slot.store(Arc::clone(effective_slot));
    }

    /// Puts a settled session's selection in force without a turn, so the
    /// status bar, `/context`, `/btw` and tasks stop describing the old one.
    async fn adopt(&mut self, input: AgentInput) -> Result<(), AgentError> {
        if self.delivery_fence.admission_error().is_some() {
            return Ok(());
        }
        self.adopt_settings();
        let (selected_slot, effective_slot) = self.route(&input.mode, false).await?;
        self.install(&input, &effective_slot, &selected_slot);
        let prompt_slots = self
            .lua_handle
            .collect_prompt_slots_async(&self.config)
            .await;
        self.context_system = self.build_system(&prompt_slots, &self.effective_tool_filter());
        self.context_options = RequestOptions {
            thinking: input.thinking,
            fast: input.fast,
            ..RequestOptions::default()
        };
        self.publish_btw_prompt(
            &effective_slot,
            &self.context_system,
            self.context_options.clone(),
        );
        self.publish_prepared_context(&effective_slot);
        self.queue.set_execution_mode(input.mode);
        Ok(())
    }

    async fn adopt_pending(&mut self) {
        let Some(input) = self.queue.claim_adoption() else {
            return;
        };
        let planning = input.mode.is_planning();
        if let Err(error) = self.adopt(input).await {
            warn!(
                %error,
                planning,
                model = %self.model_slot.load().model.spec(),
                "failed to adopt the settled selection"
            );
        }
        self.queue.finish_adoption();
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

    /// Server tools never join the array: the `code` tool reaches them, and the
    /// mode is judged per call, so the session keeps no mode ceiling.
    fn rebuild_tools(
        &mut self,
        model: &Model,
        chat_model: &Model,
        thinking: &caudra_providers::ThinkingConfig,
    ) {
        self.mcp = self.mcp.take().map(|mcp| {
            mcp.with_profile_policy(Arc::new(self.profile_tool_policy()), ToolFilter::All)
        });
        let definitions = self.build_tools(model, chat_model, thinking);
        self.tools = definitions.declared;
        self.hosted = definitions.hosted;
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

    pub(super) fn learner_handle(&self) -> Arc<ArcSwapOption<LearnerHandle>> {
        Arc::clone(&self.learner_handle)
    }

    pub(super) fn cache_warmer(&self) -> WarmerRef {
        self.cache_warmer.downgrade()
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
        session_tool_filter(&self.tools, &self.hosted, &self.mode.load())
    }

    /// Takes the settings published since the last turn. A profile that is no
    /// longer on disk stays as it was loaded, and an MCP manager that started
    /// after the session declares its tools from the next rebuild on.
    fn adopt_settings(&mut self) {
        let Some(Settings {
            config,
            prompt_profiles,
            mcp,
        }) = self.settings.take()
        else {
            return;
        };
        prompt_profiles.refresh(&mut self.system_prompt_profile);
        if self.mcp.is_none() {
            self.mcp = mcp.map(|handle| {
                McpSession::new(handle).with_elicitors(Some(Arc::new(QuestionForms)))
            });
        }
        self.mcp = self
            .mcp
            .take()
            .map(|mcp| mcp.with_disabled_tools(&config.disabled_tools));
        self.config = config;
        self.prompt_profiles = prompt_profiles.keeping(self.system_prompt_profile.as_ref());
    }

    /// Fixes the prefix just built from the live configuration when `rebuild`,
    /// and otherwise puts back the one already fixed. Leaves `tools` on the
    /// prefix, and returns its system prompt, the tools it retired, and the
    /// configuration notice the turn carries.
    fn fix_surface(
        &mut self,
        system: String,
        rebuild: bool,
        key: PrefixKey,
    ) -> (String, Vec<String>, Option<String>) {
        let live = Surface::new(system, std::mem::take(&mut self.tools));
        if rebuild {
            self.bound_profile.clone_from(&self.system_prompt_profile);
        }
        let settled = self.surface.settle(
            live,
            rebuild,
            key,
            self.history.epoch(),
            self.history.as_slice(),
        );
        self.tools = settled.surface.tools.clone();
        (
            settled.surface.system.clone(),
            settled.retired,
            settled.notice,
        )
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
        self.restart_learner();
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
        let chat: ModelResolver = Arc::new({
            let model_slot = Arc::clone(&self.model_slot);
            move || {
                let slot = model_slot.load();
                (Arc::clone(&slot.provider), Arc::new(slot.model.clone()))
            }
        });
        self.memory_pump = store.map(|store| {
            Pump::start(
                Arc::clone(store),
                chat,
                Arc::clone(&self.model_policy),
                self.timeouts,
                EventSender::new(self.agent_tx.clone(), MEMORY_EVENT_RUN_ID),
                None,
            )
        });
    }

    /// One learner per project directory: a change of directory replaces it,
    /// and drops a run in flight, which then advances nothing.
    fn restart_learner(&mut self) {
        self.learner = None;
        self.learner_handle.store(None);
        let local = self.workspace_session.is_none() && self.remote_project_context.is_none();
        let Some(state) = self
            .tool_output_store
            .as_deref()
            .map(ToolOutputStore::state_dir)
            .filter(|state| !state.is_ephemeral())
            .cloned()
        else {
            return;
        };
        if !self.config.continual_learning || self.session_id.is_none() || !local {
            return;
        }
        let host = match self.task_host() {
            Ok(host) => host,
            Err(error) => {
                warn!(%error, "continual learning unavailable without a task host");
                return;
            }
        };
        let learner = Learner::start(LearnerDeps {
            state,
            cwd: PathBuf::from(self.vars.apply("{cwd}").as_ref()),
            config_dir: caudra_storage::paths::config_dir().ok(),
            host,
            memory: self.memory.store().cloned(),
            targets: self.config.learning_targets.clone(),
            spend: Spend {
                events: EventSender::new(self.agent_tx.clone(), LEARNING_EVENT_RUN_ID),
                ledger: None,
            },
        });
        self.learner_handle.store(Some(Arc::new(learner.handle())));
        self.learner = Some(learner);
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
    /// The tools are the ones the run sends, after it drops what it cannot
    /// use, not the array it was handed.
    fn publish_btw_prompt(&self, slot: &ModelSlot, system: &str, opts: RequestOptions) {
        let mut tools = self.tools.clone();
        self.agent_params(slot, slot, ResolvedSlots::default(), ToolFilter::All)
            .request_tools(&mut tools);
        self.btw_prompt.store(Arc::new(BtwPrompt {
            provider: Arc::clone(&slot.provider),
            model: slot.model.clone(),
            system: system.to_owned(),
            tools,
            opts,
            profile: self.bound_profile.clone(),
        }));
    }

    fn publish_prepared_context(&self, slot: &ModelSlot) {
        let messages = agent::project_request(
            self.history.as_slice(),
            &self.tools,
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
            }),
            self.mcp.as_ref(),
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
                tools: &self.tools,
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

/// The tools a run may call: the declared ones and those its `code` modules dispatch, which the
/// request never declares.
fn session_tool_filter(tools: &Value, hosted: &[String], mode: &AgentMode) -> ToolFilter {
    ToolFilter::Only(
        tools
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
            .chain(hosted.iter().cloned())
            .collect(),
    )
    .for_mode(mode)
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
    use caudra_agent::tools::native::{self, plan};
    use caudra_agent::tools::profile_policy::{
        MODE_DISABLED, PLAN_REQUIRED, PROFILE_DISABLED, PROFILE_LOADING,
    };
    use caudra_agent::tools::report::REASON_CONFIG;
    use caudra_agent::tools::{AUTOMATION_TOOL_NAME, CODE_TOOL_NAME};
    use caudra_agent::tools::{ToolEffect, ToolSource};
    use caudra_config::ProfileToolExposure;
    use caudra_config::{Feature, FeatureFlags};
    use caudra_providers::Message;
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
    const PREVIEW_MCP_TOOL: &str = "preview.lookup";
    const PREVIEW_MCP_DESCRIPTION: &str = "Look up preview data";
    const PREVIEW_PLAN_REF: &str = "plan-preview";
    const OLD_DATE: &str = "2025-01-01";
    const CURRENT_DATE: &str = "2026-10-26";
    const REMOTE_PLATFORM: &str = "remote-platform";
    const LOCAL_PLATFORM: &str = "local-platform";
    const LOCAL_CWD: &str = "/host-project";

    #[test_case(false, LOCAL_CWD, LOCAL_PLATFORM; "local_environment")]
    #[test_case(true, PREVIEW_CWD, REMOTE_PLATFORM; "remote_identity_is_preserved")]
    fn compaction_refreshes_date_without_replacing_remote_identity(
        remote: bool,
        cwd: &str,
        platform: &str,
    ) {
        let previous = Vars::new()
            .set("{cwd}", PREVIEW_CWD)
            .set("{platform}", REMOTE_PLATFORM)
            .set("{date}", OLD_DATE);
        let current = Vars::new()
            .set("{cwd}", LOCAL_CWD)
            .set("{platform}", LOCAL_PLATFORM)
            .set("{date}", CURRENT_DATE);
        let refreshed = refreshed_vars(&previous, current, remote);
        assert_eq!(refreshed.apply("{cwd}"), cwd);
        assert_eq!(refreshed.apply("{platform}"), platform);
        assert_eq!(refreshed.apply("{date}"), CURRENT_DATE);
    }

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

    #[test_case(ProfileToolExposure::Eager, ContextBuiltinState::Declared; "eager")]
    #[test_case(ProfileToolExposure::Disabled, ContextBuiltinState::Disabled; "disabled")]
    fn tools_preview_uses_execution_policy_and_accounting(
        exposure: ProfileToolExposure,
        expected: ContextBuiltinState,
    ) {
        let mut policy = ProfileToolPolicy::default();
        policy.overrides.insert(plan::NAME.into(), exposure);
        let source = tools_preview_source(AgentConfig::default(), policy);
        let model = Model::from_spec(PREVIEW_MODEL).unwrap();
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
    }

    #[test_case(AgentMode::Build, ProfileToolExposure::Eager, ContextMcpStatus::Available, Some(PROFILE_LOADING); "eager")]
    #[test_case(AgentMode::Build, ProfileToolExposure::Disabled, ContextMcpStatus::Disabled, Some(PROFILE_DISABLED); "profile_disables")]
    #[test_case(AgentMode::ReadOnly, ProfileToolExposure::Eager, ContextMcpStatus::Disabled, Some(MODE_DISABLED); "read_only_disables_eager")]
    #[test_case(AgentMode::Plan(PLAN_PATH.into()), ProfileToolExposure::Eager, ContextMcpStatus::Disabled, Some(MODE_DISABLED); "local_plan_disables_eager")]
    #[test_case(AgentMode::RemotePlan(PlanRef::new(PREVIEW_PLAN_REF).unwrap()), ProfileToolExposure::Eager, ContextMcpStatus::Disabled, Some(MODE_DISABLED); "remote_plan_disables_eager")]
    fn tools_preview_reports_mcp_without_declaring_it(
        mode: AgentMode,
        exposure: ProfileToolExposure,
        expected: ContextMcpStatus,
        reason: Option<&'static str>,
    ) {
        let session = stub_session(&[(PREVIEW_MCP_TOOL, PREVIEW_MCP_DESCRIPTION)]);
        let before = session.tool_inventory();
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
        );
        assert_eq!(snapshot.inventory.mcp.tools.len(), 1);
        let tool = &snapshot.inventory.mcp.tools[0];
        assert_eq!(tool.qualified_name, PREVIEW_MCP_TOOL);
        assert_eq!((tool.status, tool.reason), (expected, reason));
        assert_eq!(snapshot.usage.mcp_tools, 0);
        assert_eq!(source.mcp.as_ref().unwrap().tool_inventory(), before);
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

    #[test]
    fn a_code_hosted_tool_stays_callable_without_a_declaration() {
        let registry = ToolRegistry::new();
        native::register(&registry, FeatureFlags::NONE.with(Feature::Automations)).unwrap();
        let definitions = registry.definitions_split(
            &Vars::new(),
            &DescriptionContext {
                filter: &ToolFilter::All,
                audience: ToolAudience::MAIN,
                workflows_available: false,
            },
            false,
        );
        let filter = session_tool_filter(
            &definitions.declared,
            &definitions.hosted,
            &AgentMode::Build,
        );

        assert!(
            !definitions
                .declared
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == AUTOMATION_TOOL_NAME)
        );
        assert!(filter.matches(CODE_TOOL_NAME));
        assert!(filter.matches(AUTOMATION_TOOL_NAME));
    }
}
