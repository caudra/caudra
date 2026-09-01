//! `caudra.agent` exposes subagent primitives to Lua plugins. Policy (retries,
//! validation, concurrency) lives in the task plugin, not here.

use std::collections::HashMap;
use std::pin::pin;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_lock::Mutex as AsyncMutex;
use caudra_agent::agent::tool_dispatch::{self, Emit};
use caudra_agent::cancel::{CancelMap, CancelSlot};
use caudra_agent::tools::interpreter_bridge;
use caudra_agent::tools::registry::ToolRegistry;
use caudra_agent::tools::schema::sanitize_tool_input_schema;
use caudra_agent::tools::{
    Deadline, DescriptionContext, FileReadTracker, LocalToolFn, LocalTools, ToolAudience,
    ToolContext, ToolEffect, ToolFilter, ToolLive, audited_local_tool,
};
use caudra_agent::{
    Agent, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams, DoneReason,
    EMPTY_RESPONSE_MARKER, Envelope, EventSender, History, InterruptSource, McpSession,
    SteeringQueue, SteeringQueueReceiver, SubagentHistoryError, SubagentHistoryLease, SubagentInfo,
    SubagentTaskMode, SubagentTaskSpec, SubagentTaskSpecCandidate, ToolDoneEvent, steering_queue,
};
use caudra_lua_macro::{lua_class, lua_fn, lua_table};
use caudra_providers::model::ModelTier;
use caudra_providers::provider;
use caudra_providers::{
    ContentBlock, HistoryItem, Message, Model, ModelError, Role, ThinkingConfig, TokenUsage,
    add_cost, expand_message,
};
use caudra_storage::id::CaudraId;
use caudra_storage::thinking::StoredThinking;
use futures::future::{Either, select};
use mlua::{Function, IntoLuaMulti, Lua, Result as LuaResult, Table, Value as LuaValue};
use serde_json::Value as JsonValue;
use tracing::info;

use crate::api::ui::buf::BufHandle;
use crate::api::util::convert::{json_to_lua, lua_to_json, lua_tool_result};
use crate::api::util::ctx::{AgentContext, LuaCtx};
use crate::api::util::pair::{Pair, err_pair, try_pair};
use crate::runtime::CANCELLED_MSG;

const SESSION_CLOSED_ERR: &str = "session closed";
const DEFAULT_SESSION_AUDIENCE: ToolAudience = ToolAudience::GENERAL_SUB;
const STRUCTURED_OUTPUT_TOOL: &str = "structured_output";
const BUILTIN_TASK_PROFILE_DESCRIPTION: &str = "Caudra's built-in task prompt";

fn parse_task_mode(mode: Option<&str>) -> Result<Option<SubagentTaskMode>, String> {
    match mode {
        Some("plan") => Ok(Some(SubagentTaskMode::Plan)),
        Some("build") => Ok(Some(SubagentTaskMode::Build)),
        Some(other) => Err(format!("unknown task mode: {other}")),
        None => Ok(None),
    }
}

fn parse_local_tool_effect(effect: Option<&str>) -> Result<ToolEffect, String> {
    match effect {
        Some("read_only") => Ok(ToolEffect::ReadOnly),
        Some("isolated") => Ok(ToolEffect::Isolated),
        Some("orchestrator") => Ok(ToolEffect::Orchestrator),
        Some("mutating") => Ok(ToolEffect::Mutating),
        Some(other) => Err(format!("unknown local tool effect: {other}")),
        None => Ok(ToolEffect::Unknown),
    }
}

fn expand_history(messages: &[Message]) -> Vec<HistoryItem> {
    let mut items: Vec<HistoryItem> = Vec::new();
    for message in messages {
        items.extend(expand_message(message, items.last().map(|item| item.id)));
    }
    items
}

fn resolve_model_from_ctx(ctx: &AgentContext, tier: Option<&str>) -> Result<Model, String> {
    let Some(tier_str) = tier else {
        return Ok(Model::clone(&ctx.model));
    };
    let requested: ModelTier = tier_str.parse().map_err(|e: ModelError| e.to_string())?;
    let effective = requested.min(ctx.model.tier);
    Model::from_tier_with_policy(&ctx.model.provider, effective, &ctx.model_policy)
        .map_err(|e| e.to_string())
}

fn model_to_lua_table(lua: &Lua, model: &Model) -> LuaResult<Table> {
    let tbl = lua.create_table()?;
    tbl.set("id", model.id.clone())?;
    tbl.set("tier", model.tier.to_string())?;
    tbl.set("provider", model.provider.to_string())?;
    tbl.set("spec", model.spec())?;
    Ok(tbl)
}

fn dispatch_ctx<'a>(ctx: &'a LuaCtx, method: &str) -> Result<&'a AgentContext, String> {
    ctx.agent()
        .ok_or_else(|| ctx.cap_err(&format!("caudra.agent.{method}")))
}

/// Forwards subagent events to the parent, stamped with the subagent identity.
/// Usage takes two paths: live on the tool header while the run goes on (last
/// turn's tokens plus the run's summed cost), and one total per run on
/// `usage_tx`, which `prompt` waits for.
async fn relay_session_events(
    sub_rx: flume::Receiver<Envelope>,
    parent_tx: EventSender,
    subagent_info: Arc<OnceLock<SubagentInfo>>,
    usage_tx: flume::Sender<TokenUsage>,
    live_sink: Option<flume::Sender<ToolLive>>,
) {
    let mut cost = None;
    while let Ok(mut envelope) = sub_rx.recv_async().await {
        match &envelope.event {
            AgentEvent::TurnComplete(turn) => {
                add_cost(&mut cost, turn.cost);
                if let Some(sink) = &live_sink {
                    let _ = sink.send(ToolLive::Usage(turn.usage.format_sum_cost(cost)));
                }
            }
            AgentEvent::Done { usage, .. } => {
                let _ = usage_tx.send(*usage);
                continue;
            }
            AgentEvent::Error { .. }
            | AgentEvent::ToolOutput { .. }
            | AgentEvent::ToolPending { .. } => continue,
            _ => {}
        }
        envelope.subagent = subagent_info.get().cloned();
        let _ = parent_tx.send_envelope(envelope);
    }
}

/// Look up the model that the current agent is using, or pick a cheaper one.
/// You might want a cheaper model for simple subtasks (summaries, classification)
/// without hard-coding a model name.
///
/// The returned table has fields: `id` (string), `tier` (string),
/// `provider` (string), `spec` (string).
///
/// @param ctx LuaCtx Agent context.
/// @param opts table? Optional fields:
///   `tier` (string?) - target tier, one of `"weak"`, `"medium"`, `"strong"`. Clamped to
///     the parent tier so you cannot escalate.
///   `spec` (string?) - exact `provider/model` spec, e.g. `"anthropic/claude-haiku-4-5"`.
///     Takes precedence over `tier`.
/// @return (table?, string?) Model table on success, or `(nil, err)` on failure.
/// @example
/// local model, err = caudra.agent.resolve_model(ctx, { tier = "weak" })
/// if err then error(err) end
/// print(model.spec, model.tier)
#[lua_fn]
async fn resolve_model(
    lua: Lua,
    ctx: mlua::UserDataRef<LuaCtx>,
    opts: Option<Table>,
) -> LuaResult<Pair<Table>> {
    let agent = try_pair!(dispatch_ctx(&ctx, "resolve_model"));
    let tier_str = opts
        .as_ref()
        .map(|table| table.get::<Option<String>>("tier"))
        .transpose()?
        .flatten();
    let spec_str = opts
        .as_ref()
        .map(|table| table.get::<Option<String>>("spec"))
        .transpose()?
        .flatten();

    let model = match spec_str {
        Some(ref spec) => try_pair!(Model::from_spec_with_policy(spec, &agent.model_policy)),
        None => try_pair!(resolve_model_from_ctx(agent, tier_str.as_deref())),
    };
    Ok((Some(model_to_lua_table(&lua, &model)?), None))
}

/// Build a system prompt from a built-in template. Environment variables like
/// `{cwd}` are substituted automatically. Use this when you need a ready-made
/// prompt for a subagent session.
///
/// @param ctx LuaCtx Agent context.
/// @param opts table Required fields:
///   `prompt_id` (string) - one of `"research"`, `"general"`, `"system"`.
/// Optional fields:
///   `instructions` (string|boolean?) - extra text appended to the prompt.
///     `true` loads instructions from the project `.caudra/instructions` file.
///     `false` or nil omits them.
/// @return (string?, string?) The assembled prompt string, or `(nil, err)` on failure.
/// @example
/// local prompt, err = caudra.agent.system_prompt(ctx, {
///   prompt_id = "research",
///   instructions = true,
/// })
/// if err then error(err) end
#[lua_fn]
async fn system_prompt(
    _lua: Lua,
    ctx: mlua::UserDataRef<LuaCtx>,
    opts: Table,
) -> LuaResult<Pair<String>> {
    let slots = Arc::clone(&try_pair!(dispatch_ctx(&ctx, "system_prompt")).prompt_slots);
    // Nothing may hold the ctx borrow across the wait: a cancel hook firing
    // meanwhile needs `ctx:finish`, which takes it mutably.
    drop(ctx);
    let prompt_id_str: String = opts.get("prompt_id")?;
    let prompt_id = match prompt_id_str.as_str() {
        "research" => caudra_agent::prompt::PromptId::Research,
        "general" => caudra_agent::prompt::PromptId::General,
        "system" => caudra_agent::prompt::PromptId::System,
        other => return Ok(err_pair(format!("unknown prompt_id: {other}"))),
    };

    let vars = caudra_agent::template::env_vars();
    let instructions_val: LuaValue = opts.get("instructions")?;
    let instructions = match instructions_val {
        LuaValue::Boolean(true) => {
            let cwd = vars.apply("{cwd}").into_owned();
            smol::unblock(move || caudra_agent::agent::load_instruction_text(&cwd)).await
        }
        LuaValue::Boolean(false) | LuaValue::Nil => String::new(),
        LuaValue::String(s) => s.to_str()?.to_owned(),
        _ => return Err(mlua::Error::runtime("instructions must be bool or string")),
    };

    let assembled = caudra_agent::prompt::assemble(prompt_id, &slots, &instructions);
    Ok((Some(vars.apply(&assembled).into_owned()), None))
}

/// Get the list of tool definitions for a given audience. Pass the result
/// straight into `caudra.agent.session()` or use it to inspect what tools are
/// available.
///
/// @param ctx LuaCtx Agent context.
/// @param opts table Required fields:
///   `audience` (string) - tool audience filter, e.g. `"general"`, `"subagent"`,
///     `"general_sub"`.
/// Optional fields:
///   `only` (string[]?) - include only these tool names.
///   `except` (string[]?) - exclude these tool names.
///   `workflow` (boolean?) - use workflow-mode descriptions. Default: `false`.
///   `spec` (string?) - evaluate capability exclusions against this model spec.
/// @return (table?, string?) Array of tool definition tables, or `(nil, err)` on failure.
/// @example
/// local defs, err = caudra.agent.tools(ctx, {
///   audience = "general_sub",
///   except = { "bash", "write" },
/// })
/// if err then error(err) end
/// print(#defs .. " tools available")
#[lua_fn]
async fn tools(lua: Lua, ctx: mlua::UserDataRef<LuaCtx>, opts: Table) -> LuaResult<Pair<LuaValue>> {
    let agent = try_pair!(dispatch_ctx(&ctx, "tools"));
    let audience_str: String = opts.get("audience")?;
    let audience = try_pair!(
        ToolAudience::parse_name(&audience_str)
            .ok_or_else(|| format!("unknown audience: {audience_str}"))
    );

    let only: Option<Vec<String>> = opts.get("only")?;
    let except: Option<Vec<String>> = opts.get("except")?;
    let workflow: bool = opts.get::<Option<bool>>("workflow")?.unwrap_or(false);
    let spec_str: Option<String> = opts.get("spec")?;

    let mut parsed = match spec_str.as_deref() {
        Some(spec) => Some(try_pair!(Model::from_spec_with_policy(
            spec,
            &agent.model_policy
        ))),
        None => None,
    };
    if let Some(model) = &mut parsed {
        try_pair!(provider::adjust_model(model, agent.timeouts));
    }
    let model = parsed.as_ref().unwrap_or(&agent.model);

    let base = match (only, except) {
        (Some(o), _) => ToolFilter::Only(o),
        (_, Some(e)) => ToolFilter::AllExcept(e),
        _ => ToolFilter::All,
    };
    let filter = base
        .intersect(&ToolFilter::from_config(&agent.config, model, &[]))
        .with_internal_companions();

    let vars = caudra_agent::template::env_vars();
    let ctx_desc = DescriptionContext {
        filter: &filter,
        audience,
        workflow,
    };
    // Base definitions only: the session injects MCP definitions per
    // request, so baking them into a tools array would freeze the catalog.
    let defs = ToolRegistry::global().definitions(&vars, &ctx_desc, model.supports_tool_examples());

    Ok((Some(json_to_lua(&lua, &defs)?), None))
}

/// Run a tool by name and wait for the result. This is how you call built-in
/// tools (like `file_read`, `shell`, `file_glob`) from Lua without going through the LLM.
///
/// Live events (streaming output, annotations, cumulative usage) are delivered
/// through optional callbacks while the tool runs.
///
/// @param ctx LuaCtx Agent context.
/// @param name string Tool name, e.g. `"bash"`, `"read"`.
/// @param input table|any Tool input (JSON-serializable). Must match the tool's `input_schema`.
/// @param opts table? Optional fields:
///   `timeout` (integer?) - deadline in seconds.
///   `on_live_buf` (function?) - called with a `BufHandle` for each live buffer
///     the tool publishes. Must not yield.
///   `on_annotation` (function?) - called with an annotation string for each
///     annotation event. Must not yield.
///   `on_usage` (function?) - called with a formatted cumulative token usage
///     string. Must not yield.
/// @return (string?, string?, string?, boolean?) Tool output text, error, generated call ID, and whether an error restore is authorized.
/// @example
/// local out, err = caudra.agent.call_tool(ctx, "bash", {
///   command = "ls -la",
///   timeout = 10,
/// })
/// if err then error(err) end
/// print(out)
#[lua_fn]
async fn call_tool(
    lua: Lua,
    ctx: mlua::UserDataRef<LuaCtx>,
    name: String,
    input: LuaValue,
    opts: Option<Table>,
) -> LuaResult<(Option<String>, Option<String>, Option<String>, Option<bool>)> {
    let input_json = lua_to_json(&lua, &input)?;
    let agent = match dispatch_ctx(&ctx, "call_tool") {
        Ok(agent) => agent,
        Err(error) => return Ok((None, Some(error), None, None)),
    };
    let mut tctx = agent.to_tool_context();
    let (mut on_buf, mut on_ann, mut on_usage, mut rx) = (None, None, None, None);
    if let Some(o) = opts {
        if let Some(secs) = o.get::<Option<u64>>("timeout")? {
            tctx.deadline = Deadline::after(Duration::from_secs(secs));
        }
        on_buf = o.get::<Option<Function>>("on_live_buf")?;
        on_ann = o.get::<Option<Function>>("on_annotation")?;
        on_usage = o.get::<Option<Function>>("on_usage")?;
        if on_buf.is_some() || on_ann.is_some() || on_usage.is_some() {
            let (tx, r) = flume::unbounded();
            tctx.live_sink = Some(tx);
            rx = Some(r);
        }
    }
    drop(ctx);
    if let Err(e) = tctx.deadline.check() {
        return Ok((None, Some(e), None, None));
    }
    let cbs = LiveCallbacks {
        tool: &name,
        on_buf,
        on_ann,
        on_usage,
    };
    let done = dispatch_racing_live(&tctx, &name, &input_json, rx, &cbs).await;
    // Same fallback the UI applies on tool completion, so a batch child's
    // header carries the annotation its standalone run would get.
    let annotation = done
        .annotation
        .clone()
        .or_else(|| (!done.is_error).then(|| done.output.annotation()).flatten());
    if let Some(a) = annotation {
        cbs.deliver(ToolLive::Annotation(a)).await;
    }
    let suffix = done.model_suffix().map(str::to_owned);
    let error_restore_allowed = done
        .output
        .lua_provenance()
        .is_none_or(|provenance| provenance.error_restore_allowed);
    match interpreter_bridge::flatten(&done) {
        Ok(mut text) => {
            if let Some(suffix) = suffix {
                text.push_str("\n\n");
                text.push_str(&suffix);
            }
            Ok((Some(text), None, Some(done.id), Some(error_restore_allowed)))
        }
        Err(mut err) => {
            if let Some(suffix) = suffix {
                err.push_str("\n\n");
                err.push_str(&suffix);
            }
            Ok((None, Some(err), Some(done.id), Some(error_restore_allowed)))
        }
    }
}

/// Create a new subagent session. The session inherits the parent model and
/// MCP handle unless you override them. You get back a `Session` object that
/// you can send messages to with `:prompt()`.
///
/// This is the main way to spin up a sub-conversation with its own history
/// and tool set.
///
/// @param ctx LuaCtx Agent context.
/// @param opts table Optional fields:
///   `model_spec` (string?) - model spec string to use instead of the parent model.
///   `system` (string?) - system prompt. Defaults to empty.
///   `tools` (table?) - tool definitions array (from `caudra.agent.tools()`).
///   `local_tools` (table?) - map of `name -> spec` for Lua-backed tools. Each spec
///     requires `description` (string), `input_schema` (table), and
///     `handler` (function). Optional `effect` is `read_only`, `isolated`,
///     `orchestrator`, or `mutating`. The handler receives the input table and
///     must return `(string)` or `(nil, err)`.
///   `name` (string?) - display name for logs and UI.
///   `task_id` (string?) - completed task to continue with its existing history.
///   `audience` (string?) - tool audience for capability gating. Default: `"general_sub"`.
///   `mcp` (boolean?) - give the session access to MCP tools. Their
///     definitions are injected automatically each turn (deferred behind
///     `tool_search`), so don't put MCP definitions in `tools`. The session
///     starts with no loaded tools of its own. Default: `true`.
///   `thinking` (string|integer?) - thinking mode: `"off"`, `"adaptive"`, an
///     effort level (`"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`,
///     `"max"`), or a budget integer (token count). Inherits parent setting
///     if omitted.
///   `fast` (boolean?) - use fast mode. Inherits parent setting if omitted.
///   `task` (boolean?) - enable the host-owned task path. Default: `false`.
///   `profile` (string?) - task system prompt profile. Requires `task = true`.
///   `mode` (string?) - task mode: `plan` or `build`. Requires `task = true`.
/// Task sessions derive model, thinking, system prompt, tools, audience, and
/// MCP access from the profile and mode. Do not combine `task = true` with the
/// corresponding generic session options.
/// @return (Session?, string?) Session handle, or `(nil, err)` on failure.
/// @example
/// local tools = caudra.agent.tools(ctx, { audience = "general_sub" })
/// local sess, err = caudra.agent.session(ctx, {
///   system = "You are a research assistant.",
///   tools = tools,
///   name = "researcher",
/// })
/// if err then error(err) end
/// local result = sess:prompt("Summarize this file.")
/// sess:close()
#[lua_fn]
async fn session(
    lua: Lua,
    ctx: mlua::UserDataRef<LuaCtx>,
    opts: Table,
) -> LuaResult<Pair<mlua::AnyUserData>> {
    let agent_ctx = try_pair!(dispatch_ctx(&ctx, "session")).clone();
    drop(ctx);
    let model_spec: Option<String> = opts.get("model_spec")?;
    let system: Option<String> = opts.get("system")?;
    let tools_val: Option<LuaValue> = opts.get("tools")?;
    let local_tools_tbl: Option<Table> = opts.get("local_tools")?;
    let name: Option<String> = opts.get("name")?;
    let continued_task_id: Option<String> = opts.get("task_id")?;
    let thinking_val: Option<LuaValue> = opts.get("thinking")?;
    let task: bool = opts.get::<Option<bool>>("task")?.unwrap_or(false);
    let requested_profile: Option<String> = opts.get("profile")?;
    let requested_mode: Option<String> = opts.get("mode")?;
    if !task && (requested_profile.is_some() || requested_mode.is_some()) {
        return Ok(err_pair("profile and mode require task = true"));
    }
    if task && local_tools_tbl.is_some() && !agent_ctx.caller_is_bundled_tool("task") {
        return Ok(err_pair(
            "task-local tools are reserved for Caudra's bundled task tool",
        ));
    }
    if !task && !matches!(agent_ctx.mode, AgentMode::Build) {
        return Ok(err_pair(
            "generic subagent sessions cannot be launched from a read-only or plan-mode parent",
        ));
    }
    let requested_mode = try_pair!(parse_task_mode(requested_mode.as_deref()));
    let audience_name: Option<String> = opts.get("audience")?;
    let requested_audience = match audience_name.as_deref() {
        Some(s) => {
            try_pair!(ToolAudience::parse_name(s).ok_or_else(|| format!("unknown audience: {s}")))
        }
        None => DEFAULT_SESSION_AUDIENCE,
    };
    if !task && requested_audience == ToolAudience::RESEARCH_SUB && local_tools_tbl.is_some() {
        return Ok(err_pair(
            "generic research sessions cannot install caller-defined local tools",
        ));
    }
    let fast: bool = opts
        .get::<Option<bool>>("fast")?
        .unwrap_or(agent_ctx.opts.fast);
    let mcp_option: Option<bool> = opts.get("mcp")?;
    let mcp_enabled = mcp_option.unwrap_or(true);
    if task
        && (model_spec.is_some()
            || system.is_some()
            || tools_val.is_some()
            || thinking_val.is_some()
            || audience_name.is_some()
            || mcp_option.is_some())
    {
        return Ok(err_pair(
            "task sessions derive model, thinking, system prompt, tools, audience, and MCP policy from their profile and mode",
        ));
    }

    let parent_tool_use_id = agent_ctx
        .tool_use_id
        .clone()
        .unwrap_or_else(|| format!("session-{}", CaudraId::generate()));
    let root_tool_use_id = agent_ctx
        .root_tool_use_id
        .clone()
        .unwrap_or_else(|| parent_tool_use_id.clone());
    let mut task_id = continued_task_id
        .clone()
        .unwrap_or_else(|| parent_tool_use_id.clone());
    let default_task_spec = SubagentTaskSpec {
        profile_name: agent_ctx.system_prompt_profile_name.to_string(),
        mode: SubagentTaskMode::Plan,
        ..SubagentTaskSpec::default()
    };
    let history_lease = if task {
        match continued_task_id {
            Some(_) => try_pair!(agent_ctx.subagent_history.continue_task_with_defaults(
                &task_id,
                SubagentTaskSpecCandidate {
                    profile_name: requested_profile,
                    mode: requested_mode,
                },
                default_task_spec,
            )),
            None => {
                let spec = SubagentTaskSpec {
                    profile_name: requested_profile.unwrap_or(default_task_spec.profile_name),
                    mode: requested_mode.unwrap_or(SubagentTaskMode::Plan),
                    ..SubagentTaskSpec::default()
                };
                match agent_ctx
                    .subagent_history
                    .reserve_with_spec(task_id.clone(), spec.clone())
                {
                    Ok(lease) => lease,
                    Err(
                        SubagentHistoryError::AlreadyActive { .. }
                        | SubagentHistoryError::AlreadyCompleted { .. },
                    ) => {
                        task_id = format!("session-{}", CaudraId::generate());
                        try_pair!(
                            agent_ctx
                                .subagent_history
                                .reserve_with_spec(task_id.clone(), spec)
                        )
                    }
                    Err(error) => return Ok(err_pair(error.to_string())),
                }
            }
        }
    } else {
        match continued_task_id {
            Some(_) => try_pair!(agent_ctx.subagent_history.continue_task(&task_id)),
            None => match agent_ctx.subagent_history.reserve(task_id.clone()) {
                Ok(lease) => lease,
                Err(
                    SubagentHistoryError::AlreadyActive { .. }
                    | SubagentHistoryError::AlreadyCompleted { .. },
                ) => {
                    task_id = format!("session-{}", CaudraId::generate());
                    try_pair!(agent_ctx.subagent_history.reserve(task_id.clone()))
                }
                Err(error) => return Ok(err_pair(error.to_string())),
            },
        }
    };

    let task_spec = task.then(|| {
        history_lease
            .spec()
            .cloned()
            .expect("task leases always carry a specification")
    });
    if task_spec
        .as_ref()
        .is_some_and(|spec| spec.mode == SubagentTaskMode::Build)
        && !matches!(agent_ctx.mode, AgentMode::Build)
    {
        return Ok(err_pair(
            "build-mode task cannot be launched from a read-only or plan-mode parent",
        ));
    }
    let task_bindings = agent_ctx.prompt_profiles.bind_for_tasks(
        &agent_ctx.model,
        &agent_ctx.opts.thinking,
        &agent_ctx.model_policy,
        agent_ctx.timeouts,
    );
    let task_profile = match &task_spec {
        Some(spec) => {
            try_pair!(agent_ctx.prompt_profiles.resolve(Some(&spec.profile_name)));
            Some(try_pair!(task_bindings.resolve(&spec.profile_name)))
        }
        None => None,
    }
    .flatten();

    let effective_model_spec = task_profile
        .as_deref()
        .and_then(|profile| profile.subagent_model())
        .map(str::to_owned)
        .or(model_spec);

    let (model, provider): (Model, Arc<dyn provider::Provider>) =
        if let Some(ref spec) = effective_model_spec {
            let mut m = try_pair!(Model::from_spec_with_policy(spec, &agent_ctx.model_policy));
            let p = try_pair!(provider::from_model_async(&mut m, agent_ctx.timeouts).await);
            (m, Arc::from(p))
        } else {
            (
                Model::clone(&agent_ctx.model),
                Arc::clone(&agent_ctx.provider),
            )
        };
    // A standalone task shows its model via SubagentInfo on the header;
    // a dispatching caller (batch) gets the same thing as a live annotation.
    if let Some(sink) = &agent_ctx.live_sink {
        let _ = sink.send(ToolLive::Annotation(model.spec()));
    }

    let mut tools_json: JsonValue = match tools_val {
        Some(val) => {
            let tools = lua_to_json(&lua, &val)?;
            if !tools.is_array() {
                return Err(mlua::Error::runtime("tools must be an array"));
            }
            tools
        }
        None => JsonValue::Array(vec![]),
    };

    let mut local_map: HashMap<String, LocalToolFn> = HashMap::new();
    if let Some(tbl) = local_tools_tbl {
        let defs = tools_json.as_array_mut().expect("checked above");
        for pair in tbl.pairs::<String, Table>() {
            let (name, spec) = pair?;
            let description = try_pair!(
                spec.get::<String>("description")
                    .map_err(|_| format!("local_tools.{name}: 'description' is required"))
            );
            let input_schema = lua_to_json(&lua, &spec.get::<LuaValue>("input_schema")?)?;
            let sanitized_schema = sanitize_tool_input_schema(input_schema);
            let handler = try_pair!(
                spec.get::<Function>("handler")
                    .map_err(|_| format!("local_tools.{name}: 'handler' is required"))
            );
            let effect_name: Option<String> = spec.get("effect")?;
            let effect = try_pair!(parse_local_tool_effect(effect_name.as_deref()));
            if task_spec
                .as_ref()
                .is_some_and(|task| task.mode == SubagentTaskMode::Plan)
                && (name != STRUCTURED_OUTPUT_TOOL || effect != ToolEffect::ReadOnly)
            {
                return Ok(err_pair(format!(
                    "local tool {name:?} is not an allowed plan-mode task output tool"
                )));
            }
            defs.push(serde_json::json!({
                "name": name,
                "description": description,
                "input_schema": sanitized_schema,
            }));
            let weak = lua.weak();
            local_map.insert(
                name,
                audited_local_tool(effect, move |input, _ctx| {
                    let result = call_local_tool(&weak, &handler, &input);
                    Box::pin(async move { result })
                }),
            );
        }
    }
    let requested_thinking = match thinking_val {
        Some(LuaValue::String(s)) => match StoredThinking::parse_setting(&s.to_str()?) {
            Ok(stored) => ThinkingConfig::from(stored),
            Err(e) => return Ok(err_pair(format!("invalid thinking: {e}"))),
        },
        Some(LuaValue::Integer(n)) => match u32::try_from(n) {
            Ok(tokens) if tokens > 0 => ThinkingConfig::Budget(tokens),
            _ => return Ok(err_pair(format!("invalid thinking budget: {n}"))),
        },
        Some(LuaValue::Number(n)) if n.fract() == 0.0 && n >= 1.0 && n <= f64::from(u32::MAX) => {
            ThinkingConfig::Budget(n as u32)
        }
        Some(LuaValue::Number(n)) => {
            return Ok(err_pair(format!("invalid thinking budget: {n}")));
        }
        Some(_) => return Err(mlua::Error::runtime("thinking must be string or number")),
        None => agent_ctx.opts.thinking.clone(),
    };

    let thinking = task_profile
        .as_deref()
        .and_then(|profile| profile.subagent_thinking().cloned())
        .map(ThinkingConfig::from)
        .unwrap_or(requested_thinking);
    if task_profile.as_ref().is_some_and(|profile| {
        profile.subagent_model().is_some() || profile.subagent_thinking().is_some()
    }) && let Err(error) = thinking.resolve_exact(&model)
    {
        let profile_name = task_spec
            .as_ref()
            .map_or("builtin", |spec| spec.profile_name.as_str());
        return Ok(err_pair(format!(
            "system prompt profile {profile_name:?} is unavailable for subagents: thinking {thinking} is incompatible with model {:?}: {error}",
            model.spec()
        )));
    }

    let (agent_mode, audience, system, task_mcp_enabled) = match task_spec.as_ref() {
        Some(spec) => {
            let (mode, prompt_id, contract, audience) = match spec.mode {
                SubagentTaskMode::Plan => (
                    AgentMode::ReadOnly,
                    caudra_agent::prompt::PromptId::Research,
                    caudra_agent::prompt::TASK_PLAN_CONTRACT,
                    ToolAudience::RESEARCH_SUB,
                ),
                SubagentTaskMode::Build => (
                    AgentMode::Build,
                    caudra_agent::prompt::PromptId::General,
                    caudra_agent::prompt::TASK_BUILD_CONTRACT,
                    ToolAudience::GENERAL_SUB,
                ),
            };
            let vars = caudra_agent::template::env_vars().set(
                "{task_system_prompt_profiles}",
                task_bindings.task_tool_summary(BUILTIN_TASK_PROFILE_DESCRIPTION),
            );
            let cwd = vars.apply("{cwd}").into_owned();
            let instructions =
                smol::unblock(move || caudra_agent::agent::load_instruction_text(&cwd)).await;
            let base_filter =
                ToolFilter::from_config(&agent_ctx.config, &model, &[]).for_mode(&mode);
            let assembled = caudra_agent::prompt::assemble_task_with_filter(
                prompt_id,
                &agent_ctx.prompt_slots,
                &base_filter,
                &instructions,
                task_profile.as_deref(),
                contract,
            );

            let local_definitions = std::mem::take(
                tools_json
                    .as_array_mut()
                    .expect("tools were validated as an array"),
            );
            let description_context = DescriptionContext {
                filter: &base_filter,
                audience,
                workflow: false,
            };
            tools_json = ToolRegistry::global().definitions(
                &vars,
                &description_context,
                model.supports_tool_examples(),
            );
            tools_json
                .as_array_mut()
                .expect("definitions return an array")
                .extend(local_definitions);
            (
                mode,
                audience,
                vars.apply(&assembled).into_owned(),
                spec.mode == SubagentTaskMode::Build,
            )
        }
        None => (
            AgentMode::Build,
            requested_audience,
            system.unwrap_or_default(),
            mcp_enabled,
        ),
    };

    let tool_filter = ToolFilter::Only(
        tools_json
            .as_array()
            .expect("tools were validated as an array")
            .iter()
            .filter_map(|definition| definition.get("name")?.as_str().map(str::to_owned))
            .collect(),
    )
    .intersect(&ToolFilter::from_config(&agent_ctx.config, &model, &[]))
    .including(local_map.keys().cloned())
    .for_mode(&agent_mode);

    let (sub_tx, sub_rx) = flume::unbounded::<Envelope>();
    let sub_event_tx = EventSender::new(sub_tx, agent_ctx.event_tx.run_id());
    let parent_tx = agent_ctx.event_tx.clone();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();
    let (steer_tx, steer_rx) = steering_queue();

    let subagent_info: Arc<OnceLock<SubagentInfo>> = Arc::new(OnceLock::new());
    let (usage_tx, usage_rx) = flume::unbounded();

    smol::spawn(relay_session_events(
        sub_rx,
        parent_tx.clone(),
        Arc::clone(&subagent_info),
        usage_tx,
        agent_ctx.live_sink.clone(),
    ))
    .detach();

    let history_items = history_lease
        .history()
        .map_or_else(Vec::new, |messages| expand_history(messages));
    let history = try_pair!(History::restored(history_items));

    // Register a cancel trigger so the child token does not fire on drop
    // and kill the subagent at birth.
    let (child_trigger, child_cancel) = agent_ctx.cancel.child();
    // Several sessions can share one task id, so keep the slot and retire
    // only ours on close instead of clearing the whole key.
    let cancel_slot = agent_ctx
        .subagent_cancels
        .insert(task_id.clone(), child_trigger);

    let name = name.unwrap_or_default();
    info!(name = %name, model = %model.id, "subagent session opened");

    let state = SessionState {
        params: AgentParams {
            provider,
            model,
            config: agent_ctx.config.clone(),
            tool_output_lines: caudra_config::ToolOutputLines::default(),
            permissions: Arc::clone(&agent_ctx.permissions),
            session_id: agent_ctx.session_id.clone(),
            root_tool_use_id: Some(root_tool_use_id.clone()),
            mailbox: None,
            timeouts: agent_ctx.timeouts,
            file_tracker: FileReadTracker::fresh(),
            prompt_slots: Arc::clone(&agent_ctx.prompt_slots),
            prompt_profiles: Arc::clone(&agent_ctx.prompt_profiles),
            system_prompt_profile_name: Arc::from(task_spec.as_ref().map_or_else(
                || agent_ctx.system_prompt_profile_name.as_ref(),
                |spec| spec.profile_name.as_str(),
            )),
            subagent_cancels: Arc::new(CancelMap::new()),
            subagent_history: agent_ctx.subagent_history.clone(),
            registry: Arc::clone(caudra_agent::tools::ToolRegistry::global_arc()),
            audience,
            tool_filter,
            model_policy: Arc::clone(&agent_ctx.model_policy),
        },
        system,
        tools: tools_json,
        mode: agent_mode,
        thinking,
        fast,
        mcp: agent_ctx
            .mcp
            .as_ref()
            .filter(|_| task_mcp_enabled)
            .map(McpSession::fresh),
        history,
        history_lease: Some(history_lease),
        sub_event_tx,
        child_cancel,
        interrupt_source: Arc::new(steer_rx),
        answer_rx: Arc::new(AsyncMutex::new(answer_rx)),
        answer_tx: Some(answer_tx),
        steer_tx: Some(steer_tx),
        parent_cancels: Arc::clone(&agent_ctx.subagent_cancels),
        parent_tool_use_id,
        root_tool_use_id,
        task_id,
        cancel_slot,
        parent_event_tx: parent_tx,
        subagent_info,
        local_tools: Arc::new(local_map),
        name,
        usage: TokenUsage::default(),
        usage_rx,
        start: Instant::now(),
        closed: false,
    };

    let sess = lua.create_userdata(LuaSession {
        inner: Arc::new(AsyncMutex::new(state)),
    })?;
    Ok((Some(sess), None))
}

lua_table! {
    /// Subagent primitives for plugins that need to talk to an LLM.
    ///
    /// This module gives you the building blocks: resolve which model to use,
    /// build a system prompt, list available tools, call a tool directly, or
    /// open a full session with its own conversation history.
    ///
    /// Policy like retries, validation, and concurrency lives in the calling
    /// plugin, not here.
    ///
    /// ```lua
    /// local tools = caudra.agent.tools(ctx, { audience = "general_sub" })
    /// local sess = caudra.agent.session(ctx, {
    ///   system = "You are a helpful assistant.",
    ///   tools = tools,
    /// })
    /// local r = sess:prompt("Hello!")
    /// print(r.text)
    /// sess:close()
    /// ```
    "caudra.agent" => pub(crate) fn create_agent_table(), DOCS [
        resolve_model, system_prompt, tools, call_tool, session,
    ]
}

/// Must use `call_async`, not `call`: callbacks that yield (highlight,
/// markdown) hit the C-call boundary otherwise.
struct LiveCallbacks<'a> {
    tool: &'a str,
    on_buf: Option<Function>,
    on_ann: Option<Function>,
    on_usage: Option<Function>,
}

impl LiveCallbacks<'_> {
    async fn deliver(&self, ev: ToolLive) {
        let res = match ev {
            ToolLive::Buf(buf) => call_opt(&self.on_buf, BufHandle::foreign(buf)).await,
            ToolLive::Annotation(ann) => call_opt(&self.on_ann, ann).await,
            ToolLive::Usage(usage) => call_opt(&self.on_usage, usage).await,
        };
        if let Some(Err(e)) = res {
            tracing::warn!(tool = self.tool, error = %e, "call_tool callback failed");
        }
    }
}

async fn call_opt(f: &Option<Function>, arg: impl IntoLuaMulti) -> Option<LuaResult<()>> {
    match f {
        Some(f) => Some(f.call_async::<()>(arg).await),
        None => None,
    }
}

/// Like `interpreter_bridge::dispatch`, but keeps the full `ToolDoneEvent`
/// (the annotation lives there) and feeds live events to `cbs` while the
/// child runs.
async fn dispatch_racing_live(
    tctx: &ToolContext,
    name: &str,
    input: &JsonValue,
    rx: Option<flume::Receiver<ToolLive>>,
    cbs: &LiveCallbacks<'_>,
) -> ToolDoneEvent {
    let call_id = CaudraId::generate().to_string();
    let run = tool_dispatch::run(
        &tctx.registry,
        tctx.mcp.as_ref(),
        call_id,
        name,
        input,
        tctx,
        Emit::Silent,
    );
    let Some(rx) = rx else {
        return run.await;
    };
    let mut run = pin!(run);
    loop {
        match select(run.as_mut(), pin!(rx.recv_async())).await {
            Either::Left((done, _)) => {
                while let Ok(ev) = rx.try_recv() {
                    cbs.deliver(ev).await;
                }
                return done;
            }
            Either::Right((Ok(ev), _)) => cbs.deliver(ev).await,
            // The sender is gone but no result arrived: just wait for the run.
            Either::Right((Err(_), _)) => return run.await,
        }
    }
}

struct SessionState {
    params: AgentParams,
    system: String,
    tools: JsonValue,
    mode: AgentMode,
    thinking: ThinkingConfig,
    fast: bool,
    /// Fresh per session so `tool_search` loads never leak between a
    /// subagent and its parent.
    mcp: Option<McpSession>,
    history: History,
    history_lease: Option<SubagentHistoryLease>,
    sub_event_tx: EventSender,
    child_cancel: caudra_agent::cancel::CancelToken,
    interrupt_source: Arc<SteeringQueueReceiver>,
    answer_rx: Arc<AsyncMutex<flume::Receiver<String>>>,
    answer_tx: Option<flume::Sender<String>>,
    steer_tx: Option<SteeringQueue>,
    parent_cancels: Arc<CancelMap<String>>,
    parent_tool_use_id: String,
    root_tool_use_id: String,
    task_id: String,
    /// Which cancellation registration under `task_id` is ours.
    cancel_slot: CancelSlot,
    parent_event_tx: EventSender,
    subagent_info: Arc<OnceLock<SubagentInfo>>,
    local_tools: LocalTools,
    name: String,
    usage: TokenUsage,
    usage_rx: flume::Receiver<TokenUsage>,
    start: Instant,
    closed: bool,
}

impl SessionState {
    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.parent_cancels.retire(&self.task_id, self.cancel_slot);
        let messages = std::mem::replace(&mut self.history, History::new(Vec::new())).into_vec();
        let persisted_spec = self
            .history_lease
            .as_ref()
            .and_then(|lease| lease.spec().cloned());
        if let Some(lease) = self.history_lease.take() {
            lease.complete_version(Arc::new(messages.clone()), self.parent_tool_use_id.clone());
        }
        let _ = self.parent_event_tx.send(AgentEvent::SubagentHistory {
            task_id: self.task_id.clone(),
            parent_tool_use_id: self.parent_tool_use_id.clone(),
            root_tool_use_id: self.root_tool_use_id.clone(),
            name: self.name.clone(),
            model: self.params.model.spec(),
            messages,
            spec: persisted_spec,
        });
        info!(
            name = %self.name,
            duration_ms = self.start.elapsed().as_millis() as u64,
            input_tokens = self.usage.total_input(),
            output_tokens = self.usage.output,
            "subagent session closed",
        );
    }
}

struct LuaSession {
    inner: Arc<AsyncMutex<SessionState>>,
}

impl Drop for LuaSession {
    fn drop(&mut self) {
        match self.inner.try_lock() {
            Some(mut s) => s.close(),
            // Prompt still in flight: close asynchronously so history
            // and cancel entry are never silently leaked.
            None => {
                let inner = Arc::clone(&self.inner);
                smol::spawn(async move { inner.lock().await.close() }).detach();
            }
        }
    }
}

/// Send a message to the subagent and wait for its full response. The agent
/// loop runs to completion, calling tools as needed. Conversation history is
/// kept across calls, so you can have a multi-turn conversation.
///
/// The returned table has fields: `text` (string), `duration_ms` (integer),
/// `input_tokens` (integer), `output_tokens` (integer). `text` is an empty
/// string when the subagent produced no text block (e.g. it only called
/// tools).
///
/// @param message string User message to send.
/// @return (table?, string?) Result table on success, or `(nil, err)` on
/// failure. A run cut short after streaming some text hands you both: the
/// error and a `{ text = <what it streamed> }` table.
/// @example
/// local r, err = sess:prompt("What files are in this project?")
/// if err then error(err) end
/// print(r.text)
/// print(r.input_tokens .. " input, " .. r.output_tokens .. " output tokens")
#[lua_fn]
async fn prompt(
    lua: Lua,
    this: mlua::UserDataRef<LuaSession>,
    message: String,
) -> LuaResult<Pair<Table>> {
    let inner = Arc::clone(&this.inner);
    drop(this);
    let mut guard = inner.lock().await;
    let s = &mut *guard;
    if s.closed {
        return Ok((None, Some(SESSION_CLOSED_ERR.to_owned())));
    }
    if s.subagent_info.get().is_none() {
        let _ = s.subagent_info.set(SubagentInfo {
            parent_tool_use_id: s.parent_tool_use_id.clone(),
            task_id: s.task_id.clone(),
            name: s.name.clone(),
            prompt: Some(message.clone()),
            model: Some(s.params.model.spec()),
            answer_tx: s.answer_tx.take(),
            steer_tx: s.steer_tx.take(),
        });
    }

    let history_len = s.history.len();
    let interrupt_source: Arc<dyn InterruptSource> = s.interrupt_source.clone();
    let mut agent = Agent::new(
        s.params.clone(),
        AgentRunParams {
            history: &mut s.history,
            system: s.system.clone(),
            event_tx: s.sub_event_tx.clone(),
            tools: s.tools.clone(),
        },
    )
    .with_user_response_rx(Arc::clone(&s.answer_rx))
    .with_interrupt_source(interrupt_source)
    .with_cancel(s.child_cancel.clone())
    .with_mcp(s.mcp.clone())
    .with_local_tools(Arc::clone(&s.local_tools));

    let input = AgentInput {
        message,
        mode: s.mode.clone(),
        images: Vec::new(),
        preamble: Vec::new(),
        thinking: s.thinking.clone(),
        fast: s.fast,
        workflow: false,
        prompt: None,
    };
    let result = agent.run(input).await;
    drop(agent);
    // Only this call's messages count: older turns may hold stale preamble
    // text, and the agent loop's empty-response retry leaves a synthetic
    // "(empty)" assistant marker that must not pass for a real response.
    // Auto-compaction can shrink the history mid-run, so clamp the start:
    // after a rewrite the tail is this call's output either way.
    let turn = &s.history.as_slice()[history_len.min(s.history.len())..];
    // A subagent can be cancelled on its own, and its caller should hear about
    // that instead of taking a half-finished answer for a real one, so cancel
    // reads like an error here even though the run ended normally.
    let cut_short = match &result {
        Err(e) => Some(e.to_string()),
        Ok(DoneReason::Cancelled) => Some(CANCELLED_MSG.to_owned()),
        Ok(_) => None,
    };
    if let Some(err) = cut_short {
        let partial = turn
            .iter()
            .filter(|m| matches!(m.role, Role::Assistant))
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text { text } if text != EMPTY_RESPONSE_MARKER => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let tbl = if partial.is_empty() {
            None
        } else {
            let tbl = lua.create_table()?;
            tbl.set("text", partial)?;
            Some(tbl)
        };
        return Ok((tbl, Some(err)));
    }
    // Waiting here doubles as an ordering barrier: the relay reaches `Done` only
    // after every `TurnComplete`, so all our `ToolLive::Usage` messages sit in the
    // live channel before `dispatch_racing_live` drains it for the last time.
    match s.usage_rx.recv_async().await {
        Ok(usage) => s.usage += usage,
        Err(_) => tracing::warn!(
            name = %s.name,
            "subagent usage tracker stopped, token counts may lag"
        ),
    }

    let text = turn
        .iter()
        .rfind(|m| matches!(m.role, Role::Assistant))
        .and_then(|m| {
            m.content.iter().find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
        });
    let text = text.map_or_else(String::new, str::to_owned);

    let tbl = lua.create_table()?;
    tbl.set("text", text)?;
    tbl.set("duration_ms", s.start.elapsed().as_millis() as u64)?;
    tbl.set("input_tokens", s.usage.total_input())?;
    tbl.set("output_tokens", s.usage.output)?;
    Ok((Some(tbl), None))
}

/// Return the stable task ID used for continuation and UI routing.
///
/// @return string
#[lua_fn]
async fn id(_lua: Lua, this: mlua::UserDataRef<LuaSession>) -> LuaResult<String> {
    let inner = Arc::clone(&this.inner);
    drop(this);
    let task_id = inner.lock().await.task_id.clone();
    Ok(task_id)
}

/// Close the session and flush its history back to the parent agent. You can
/// call this multiple times safely. If you forget, it runs automatically when
/// the session is garbage collected.
///
/// @return
#[lua_fn]
async fn close(_lua: Lua, this: mlua::UserDataRef<LuaSession>) -> LuaResult<()> {
    let inner = Arc::clone(&this.inner);
    drop(this);
    let mut s = inner.lock().await;
    s.close();
    Ok(())
}

lua_class! {
    /// A subagent session with its own conversation history.
    ///
    /// Create one with `caudra.agent.session()`, then send messages with
    /// `:prompt()`. The session remembers previous turns, so you can have
    /// a multi-step conversation. Call `:close()` when you are done, or let
    /// garbage collection handle it.
    "caudra.agent.Session" => LuaSession, SESSION_DOCS [id, prompt, close]
}

/// Weak Lua ref avoids a reference cycle when the session is stored in userdata.
fn call_local_tool(
    weak: &mlua::WeakLua,
    f: &Function,
    input: &JsonValue,
) -> Result<String, String> {
    let lua = weak.try_upgrade().ok_or("Lua runtime shut down")?;
    let arg = json_to_lua(&lua, input).map_err(|e| e.to_string())?;
    let values = f.call::<mlua::MultiValue>(arg).map_err(|e| e.to_string())?;
    lua_tool_result(values)
}

#[cfg(test)]
mod tests {
    use caudra_agent::{DoneReason, TurnCompleteEvent};
    use serde_json::json;

    use super::*;

    fn call(src: &str, input: JsonValue) -> Result<String, String> {
        let lua = Lua::new();
        let f: Function = lua.load(src).eval().unwrap();
        call_local_tool(&lua.weak(), &f, &input)
    }

    #[test]
    fn local_tool_handler_result_conventions() {
        let input = json!({"x": "1"});
        assert_eq!(
            call("function(v) return 'ok:' .. v.x end", input.clone()),
            Ok("ok:1".into())
        );
        assert_eq!(
            call("function() return nil, 'bad' end", input.clone()),
            Err("bad".into())
        );
        assert_eq!(
            call("function() end", input.clone()),
            Err(crate::api::util::convert::NIL_TOOL_RESULT_ERR.into())
        );
        let raised = call("function() error('boom') end", input.clone()).unwrap_err();
        assert!(raised.contains("boom"), "got: {raised}");
        let wrong = call("function() return 42 end", input).unwrap_err();
        assert!(wrong.contains("expected string"), "got: {wrong}");
    }

    #[test]
    fn grouped_history_expands_with_stable_parent_chain_and_round_trips() {
        const CALL_ID: &str = "call-1";
        const TOOL_NAME: &str = "read";
        let messages = vec![
            Message::user("inspect".into()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "checking".into(),
                    },
                    ContentBlock::tool_use(CALL_ID, TOOL_NAME, json!({"path": "src/lib.rs"})),
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: CALL_ID.into(),
                    content: "contents".into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            },
        ];

        let items = expand_history(&messages);

        assert!(
            items
                .windows(2)
                .all(|pair| pair[1].parent_id == Some(pair[0].id))
        );
        let projected = History::restored(items).unwrap().into_vec();
        assert_eq!(
            serde_json::to_value(projected).unwrap(),
            serde_json::to_value(messages).unwrap()
        );
    }

    const RUN_ID: u64 = 7;
    const PARENT_ID: &str = "task-1";
    const IGNORED_ERROR: &str = "handled by the session caller";
    const DONE_USAGE: TokenUsage = tokens(150, 30);

    const fn tokens(input: u32, output: u32) -> TokenUsage {
        TokenUsage {
            input,
            output,
            cache_creation: 0,
            cache_read: 0,
        }
    }

    fn envelope(event: AgentEvent) -> Envelope {
        Envelope {
            event,
            subagent: None,
            run_id: RUN_ID,
        }
    }

    fn turn(usage: TokenUsage, cost: f64) -> AgentEvent {
        AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
            message: Message::default(),
            usage,
            model: "test-model".into(),
            cost: Some(cost),
            context_size: None,
            context_window: 0,
        }))
    }

    #[test]
    fn relay_session_events_reports_live_usage_and_done_total() {
        let (sub_tx, sub_rx) = flume::unbounded();
        let (parent_raw_tx, parent_rx) = flume::unbounded();
        let subagent_info = Arc::new(OnceLock::new());
        subagent_info
            .set(SubagentInfo {
                parent_tool_use_id: PARENT_ID.into(),
                task_id: PARENT_ID.into(),
                name: "research".into(),
                prompt: None,
                model: None,
                answer_tx: None,
                steer_tx: None,
            })
            .unwrap();
        let (usage_tx, usage_rx) = flume::unbounded();
        let (live_tx, live_rx) = flume::unbounded();

        for event in [
            turn(tokens(100, 20), 0.25),
            turn(tokens(50, 10), 0.5),
            AgentEvent::Error {
                message: IGNORED_ERROR.into(),
            },
            AgentEvent::SubagentHistory {
                task_id: "nested-task".into(),
                parent_tool_use_id: "nested-call".into(),
                root_tool_use_id: PARENT_ID.into(),
                name: "nested".into(),
                model: "provider/model".into(),
                messages: Vec::new(),
                spec: None,
            },
            AgentEvent::Done {
                usage: DONE_USAGE,
                num_turns: 2,
                reason: DoneReason::EndTurn,
            },
        ] {
            sub_tx.send(envelope(event)).unwrap();
        }
        drop(sub_tx);

        smol::block_on(relay_session_events(
            sub_rx,
            EventSender::new(parent_raw_tx, RUN_ID),
            subagent_info,
            usage_tx,
            Some(live_tx),
        ));

        let live = live_rx
            .drain()
            .map(|event| match event {
                ToolLive::Usage(usage) => usage,
                _ => panic!("relay must only publish usage"),
            })
            .collect::<Vec<_>>();
        let expected = [
            tokens(100, 20).format_sum_cost(Some(0.25)),
            tokens(50, 10).format_sum_cost(Some(0.75)),
        ];
        assert_eq!(live, expected);
        assert_eq!(usage_rx.try_recv(), Ok(DONE_USAGE));

        let forwarded = parent_rx.drain().collect::<Vec<_>>();
        assert_eq!(forwarded.len(), expected.len() + 1);
        assert!(forwarded.iter().all(|envelope| {
            matches!(
                envelope.event,
                AgentEvent::TurnComplete(_) | AgentEvent::SubagentHistory { .. }
            ) && envelope
                .subagent
                .as_ref()
                .is_some_and(|info| info.parent_tool_use_id == PARENT_ID)
        }));
    }
}
