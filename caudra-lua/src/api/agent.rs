//! `caudra.agent` exposes subagent primitives to Lua plugins. Policy (retries,
//! validation, concurrency) lives in the task plugin, not here.

use std::collections::HashMap;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use async_lock::Mutex as AsyncMutex;
use caudra_agent::agent::subagent::{self, STRUCTURED_OUTPUT_TOOL, Subagent};
use caudra_agent::agent::tool_dispatch::{self, Emit};
use caudra_agent::tools::interpreter_bridge;
use caudra_agent::tools::registry::ToolRegistry;
use caudra_agent::tools::schema::sanitize_tool_input_schema;
use caudra_agent::tools::{
    Deadline, DescriptionContext, LocalToolFn, LocalTools, ToolAudience, ToolContext, ToolEffect,
    ToolFilter, ToolLive, audited_local_tool,
};
use caudra_agent::{SubagentTaskMode, ToolDoneEvent};
use caudra_config::providers::UnknownPurpose;
use caudra_lua_macro::{lua_class, lua_fn, lua_table};
use caudra_providers::provider;
use caudra_providers::{Model, ModelPurpose, ThinkingConfig};
use caudra_storage::id::CaudraId;
use caudra_storage::thinking::StoredThinking;
use futures::future::{Either, select};
use mlua::{Function, IntoLuaMulti, Lua, Result as LuaResult, Table, Value as LuaValue};
use serde_json::Value as JsonValue;

use crate::api::ui::buf::BufHandle;
use crate::api::util::convert::{json_to_lua, lua_to_json, lua_tool_result};
use crate::api::util::ctx::{AgentContext, LuaCtx};
use crate::api::util::pair::{Pair, err_pair, try_pair};

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

fn resolve_model_from_ctx(ctx: &AgentContext, purpose: Option<&str>) -> Result<Model, String> {
    let Some(name) = purpose else {
        return Ok(Model::clone(&ctx.model));
    };
    let purpose: ModelPurpose = name.parse().map_err(|e: UnknownPurpose| e.to_string())?;
    Model::resolve(purpose, &ctx.model, &ctx.model_policy).map_err(|e| e.to_string())
}

fn model_to_lua_table(lua: &Lua, model: &Model) -> LuaResult<Table> {
    let tbl = lua.create_table()?;
    tbl.set("id", model.id.clone())?;
    tbl.set("provider", model.provider.to_string())?;
    tbl.set("spec", model.spec())?;
    Ok(tbl)
}

fn dispatch_ctx<'a>(ctx: &'a LuaCtx, method: &str) -> Result<&'a AgentContext, String> {
    ctx.agent()
        .ok_or_else(|| ctx.cap_err(&format!("caudra.agent.{method}")))
}

/// Look up the model that the current agent is using, or the one bound to
/// another purpose. Ask for `"fast"` when a subtask is simple (summaries,
/// classification) instead of hard-coding a model name.
///
/// The returned table has fields: `id` (string), `provider` (string),
/// `spec` (string).
///
/// @param ctx LuaCtx Agent context.
/// @param opts table? Optional fields:
///   `purpose` (string?) - which binding to resolve, one of `"chat"`, `"plan"`,
///     `"subagent"`, `"compact"`, `"title"`, `"goal"`, `"fast"`, `"best"`.
///     Resolves to the model the user bound, or that purpose's default.
///   `spec` (string?) - exact `provider/model` spec, e.g. `"anthropic/claude-haiku-4-5"`.
///     Takes precedence over `purpose`.
/// @return (table?, string?) Model table on success, or `(nil, err)` on failure.
/// @example
/// local model, err = caudra.agent.resolve_model(ctx, { purpose = "fast" })
/// if err then error(err) end
/// print(model.spec)
#[lua_fn]
async fn resolve_model(
    lua: Lua,
    ctx: mlua::UserDataRef<LuaCtx>,
    opts: Option<Table>,
) -> LuaResult<Pair<Table>> {
    let agent = try_pair!(dispatch_ctx(&ctx, "resolve_model"));
    let purpose = opts
        .as_ref()
        .map(|table| table.get::<Option<String>>("purpose"))
        .transpose()?
        .flatten();
    let spec_str = opts
        .as_ref()
        .map(|table| table.get::<Option<String>>("spec"))
        .transpose()?
        .flatten();

    let model = match spec_str {
        Some(ref spec) => try_pair!(Model::from_spec_with_policy(spec, &agent.model_policy)),
        None => try_pair!(resolve_model_from_ctx(agent, purpose.as_deref())),
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
        workflows_available: false,
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
///   `on_progress` (function?) - called with `(label, detail, tally)` whenever a
///     dispatched subagent moves. `label` is a tool name or one of `"thinking"`,
///     `"responding"`, `"compacting"`, `"retrying"`, `"awaiting permission"`;
///     `detail` is the tool header, or nil; `tally` reads like `"3 tools · 12.4s"`.
///     Must not yield.
/// @return (string?, string?, string?, boolean?, string?) Tool output text as
///   the model sees it, error, generated call ID, whether an error restore is
///   authorized, and the same result written for a reader. The last differs
///   for tools whose model output is a structured record: show it instead of
///   the first when presenting the call to a person.
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
) -> LuaResult<(
    Option<String>,
    Option<String>,
    Option<String>,
    Option<bool>,
    Option<String>,
)> {
    let input_json = lua_to_json(&lua, &input)?;
    let agent = match dispatch_ctx(&ctx, "call_tool") {
        Ok(agent) => agent,
        Err(error) => return Ok((None, Some(error), None, None, None)),
    };
    let mut tctx = agent.to_tool_context();
    let (mut on_buf, mut on_ann, mut on_usage, mut on_progress, mut rx) =
        (None, None, None, None, None);
    if let Some(o) = opts {
        if let Some(secs) = o.get::<Option<u64>>("timeout")? {
            tctx.deadline = Deadline::after(Duration::from_secs(secs));
        }
        on_buf = o.get::<Option<Function>>("on_live_buf")?;
        on_ann = o.get::<Option<Function>>("on_annotation")?;
        on_usage = o.get::<Option<Function>>("on_usage")?;
        on_progress = o.get::<Option<Function>>("on_progress")?;
        if on_buf.is_some() || on_ann.is_some() || on_usage.is_some() || on_progress.is_some() {
            let (tx, r) = flume::unbounded();
            tctx.live_sink = Some(tx);
            rx = Some(r);
        }
    }
    drop(ctx);
    if let Err(e) = tctx.deadline.check() {
        return Ok((None, Some(e), None, None, None));
    }
    let cbs = LiveCallbacks {
        tool: &name,
        on_buf,
        on_ann,
        on_usage,
        on_progress,
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
    // Workcell tools answer the model with a structured record, which is the
    // wrong thing to show a reader. The display form comes off the same
    // output the transcript renders a standalone call from.
    let display = done.output.as_display_text();
    match interpreter_bridge::flatten(&done) {
        Ok(mut text) => {
            if let Some(suffix) = suffix {
                text.push_str("\n\n");
                text.push_str(&suffix);
            }
            let display = (display != text).then_some(display);
            Ok((
                Some(text),
                None,
                Some(done.id),
                Some(error_restore_allowed),
                display,
            ))
        }
        Err(mut err) => {
            if let Some(suffix) = suffix {
                err.push_str("\n\n");
                err.push_str(&suffix);
            }
            Ok((
                None,
                Some(err),
                Some(done.id),
                Some(error_restore_allowed),
                None,
            ))
        }
    }
}

/// Create a new subagent session. The session uses the global Subagent model,
/// which inherits the parent model when unbound, and inherits the MCP handle.
/// You can override either. You get back a `Session` object that you can send
/// messages to with `:prompt()`.
///
/// This is the main way to spin up a sub-conversation with its own history
/// and tool set.
///
/// @param ctx LuaCtx Agent context.
/// @param opts table Optional fields:
///   `model_spec` (string?) - exact model spec to use instead of the Subagent binding.
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
/// Task sessions derive their model from the Subagent binding or an explicit
/// profile selector. Thinking may also come from the profile; system prompt,
/// tools, audience, and MCP access follow profile and mode. Do not combine
/// `task = true` with the corresponding generic session options.
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
    let task: bool = opts.get::<Option<bool>>("task")?.unwrap_or(false);
    let name: Option<String> = opts.get("name")?;
    let task_id: Option<String> = opts.get("task_id")?;
    let local_tools_tbl: Option<Table> = opts.get("local_tools")?;
    if task && local_tools_tbl.is_some() && !agent_ctx.caller_is_bundled_tool("task") {
        return Ok(err_pair(
            "task-local tools are reserved for Caudra's bundled task tool",
        ));
    }

    let subagent = if task {
        try_pair!(open_lua_task(&lua, &agent_ctx, &opts, name, task_id, local_tools_tbl).await)
    } else {
        try_pair!(open_lua_generic(&lua, &agent_ctx, &opts, name, task_id, local_tools_tbl).await)
    };
    let sess = lua.create_userdata(LuaSession {
        inner: Arc::new(AsyncMutex::new(subagent)),
    })?;
    Ok((Some(sess), None))
}

async fn open_lua_task(
    lua: &Lua,
    agent_ctx: &AgentContext,
    opts: &Table,
    name: Option<String>,
    task_id: Option<String>,
    local_tools_tbl: Option<Table>,
) -> Result<Subagent, String> {
    if [
        "model_spec",
        "system",
        "tools",
        "thinking",
        "audience",
        "mcp",
    ]
    .iter()
    .any(|key| opts.get::<Option<LuaValue>>(*key).ok().flatten().is_some())
    {
        return Err(TASK_DERIVES_ITS_OWN.into());
    }
    let mode = parse_task_mode(
        opts.get::<Option<String>>("mode")
            .map_err(lua_err)?
            .as_deref(),
    )?;
    // Plan-mode tasks are read-only, so the one tool they may install is the
    // structured-output sink: anything else would be an effect in disguise.
    let plan_mode = mode.unwrap_or(SubagentTaskMode::Plan) == SubagentTaskMode::Plan;
    let (local_definitions, local_tools) = build_local_tools(
        lua,
        local_tools_tbl,
        plan_mode.then_some(STRUCTURED_OUTPUT_TOOL),
    )?;
    subagent::open_task(
        agent_ctx,
        subagent::TaskOptions {
            name: name.unwrap_or_default(),
            task_id: subagent::TaskIdentity::continue_or_derive(task_id),
            profile: opts.get("profile").map_err(lua_err)?,
            mode,
            local_definitions,
            local_tools,
        },
    )
    .await
}

async fn open_lua_generic(
    lua: &Lua,
    agent_ctx: &AgentContext,
    opts: &Table,
    name: Option<String>,
    task_id: Option<String>,
    local_tools_tbl: Option<Table>,
) -> Result<Subagent, String> {
    if opts
        .get::<Option<LuaValue>>("profile")
        .ok()
        .flatten()
        .is_some()
        || opts
            .get::<Option<LuaValue>>("mode")
            .ok()
            .flatten()
            .is_some()
    {
        return Err("profile and mode require task = true".into());
    }
    let audience = match opts
        .get::<Option<String>>("audience")
        .map_err(lua_err)?
        .as_deref()
    {
        Some(name) => Some(
            ToolAudience::parse_name(name).ok_or_else(|| format!("unknown audience: {name}"))?,
        ),
        None => None,
    };
    if audience == Some(ToolAudience::RESEARCH_SUB) && local_tools_tbl.is_some() {
        return Err("generic research sessions cannot install caller-defined local tools".into());
    }
    let mut tools = match opts.get::<Option<LuaValue>>("tools").map_err(lua_err)? {
        Some(value) => {
            let tools = lua_to_json(lua, &value).map_err(lua_err)?;
            if !tools.is_array() {
                return Err("tools must be an array".into());
            }
            tools
        }
        None => JsonValue::Array(Vec::new()),
    };
    let (local_definitions, local_tools) = build_local_tools(lua, local_tools_tbl, None)?;
    tools
        .as_array_mut()
        .expect("checked above")
        .extend(local_definitions);
    subagent::open_generic(
        agent_ctx,
        subagent::GenericOptions {
            name: name.unwrap_or_default(),
            task_id,
            model_spec: opts.get("model_spec").map_err(lua_err)?,
            system: opts
                .get::<Option<String>>("system")
                .map_err(lua_err)?
                .unwrap_or_default(),
            tools,
            audience,
            thinking: parse_thinking(opts.get("thinking").map_err(lua_err)?)?,
            fast: opts.get("fast").map_err(lua_err)?,
            mcp: opts.get("mcp").map_err(lua_err)?,
            local_tools,
        },
    )
    .await
}

const TASK_DERIVES_ITS_OWN: &str = "task sessions derive model, thinking, system prompt, tools, audience, and MCP policy from the Subagent binding, profile, and mode";

fn lua_err(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn parse_thinking(value: Option<LuaValue>) -> Result<Option<ThinkingConfig>, String> {
    match value {
        None | Some(LuaValue::Nil) => Ok(None),
        Some(LuaValue::String(s)) => {
            let text = s.to_str().map_err(lua_err)?;
            StoredThinking::parse_setting(&text)
                .map(|stored| Some(ThinkingConfig::from(stored)))
                .map_err(|error| format!("invalid thinking: {error}"))
        }
        Some(LuaValue::Integer(n)) => match u32::try_from(n) {
            Ok(tokens) if tokens > 0 => Ok(Some(ThinkingConfig::Budget(tokens))),
            _ => Err(format!("invalid thinking budget: {n}")),
        },
        Some(LuaValue::Number(n)) if n.fract() == 0.0 && n >= 1.0 && n <= f64::from(u32::MAX) => {
            Ok(Some(ThinkingConfig::Budget(n as u32)))
        }
        Some(LuaValue::Number(n)) => Err(format!("invalid thinking budget: {n}")),
        Some(_) => Err("thinking must be string or number".into()),
    }
}

/// Turns the Lua `local_tools` table into tool definitions plus their
/// handlers. `only` restricts which name may be installed, which is how a
/// read-only task keeps its single output sink and nothing else.
fn build_local_tools(
    lua: &Lua,
    table: Option<Table>,
    only: Option<&str>,
) -> Result<(Vec<JsonValue>, LocalTools), String> {
    let Some(table) = table else {
        return Ok((Vec::new(), LocalTools::default()));
    };
    let mut definitions = Vec::new();
    let mut handlers: HashMap<String, LocalToolFn> = HashMap::new();
    for pair in table.pairs::<String, Table>() {
        let (name, spec) = pair.map_err(lua_err)?;
        let description = spec
            .get::<String>("description")
            .map_err(|_| format!("local_tools.{name}: 'description' is required"))?;
        let input_schema =
            lua_to_json(lua, &spec.get::<LuaValue>("input_schema").map_err(lua_err)?)
                .map_err(lua_err)?;
        let handler = spec
            .get::<Function>("handler")
            .map_err(|_| format!("local_tools.{name}: 'handler' is required"))?;
        let effect = parse_local_tool_effect(
            spec.get::<Option<String>>("effect")
                .map_err(lua_err)?
                .as_deref(),
        )?;
        if only.is_some_and(|allowed| name != allowed || effect != ToolEffect::ReadOnly) {
            return Err(format!(
                "local tool {name:?} is not an allowed plan-mode task output tool"
            ));
        }
        definitions.push(serde_json::json!({
            "name": name,
            "description": description,
            "input_schema": sanitize_tool_input_schema(input_schema),
        }));
        let weak = lua.weak();
        handlers.insert(
            name,
            audited_local_tool(effect, move |input, _ctx| {
                let result = call_local_tool(&weak, &handler, &input);
                Box::pin(async move { result })
            }),
        );
    }
    Ok((definitions, Arc::new(handlers)))
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
    on_progress: Option<Function>,
}

impl LiveCallbacks<'_> {
    async fn deliver(&self, ev: ToolLive) {
        let res = match ev {
            ToolLive::Buf(buf) => call_opt(&self.on_buf, BufHandle::foreign(buf)).await,
            ToolLive::Annotation(ann) => call_opt(&self.on_ann, ann).await,
            ToolLive::Usage(usage) => call_opt(&self.on_usage, usage).await,
            ToolLive::Progress(progress) => {
                let args = (
                    progress.activity.label().to_owned(),
                    progress.activity.detail().map(str::to_owned),
                    progress.tally_now(),
                );
                call_opt(&self.on_progress, args).await
            }
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

struct LuaSession {
    inner: Arc<AsyncMutex<Subagent>>,
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
    match inner.lock().await.prompt(Some(message)).await {
        Ok(result) => {
            let tbl = lua.create_table()?;
            tbl.set("text", result.text)?;
            tbl.set("duration_ms", result.duration.as_millis() as u64)?;
            tbl.set("input_tokens", result.input_tokens)?;
            tbl.set("output_tokens", result.output_tokens)?;
            Ok((Some(tbl), None))
        }
        Err(failure) => {
            let partial = match failure.partial {
                Some(text) => {
                    let tbl = lua.create_table()?;
                    tbl.set("text", text)?;
                    Some(tbl)
                }
                None => None,
            };
            Ok((partial, Some(failure.error)))
        }
    }
}

/// Return the stable task ID used for continuation and UI routing.
///
/// @return string
#[lua_fn]
async fn id(_lua: Lua, this: mlua::UserDataRef<LuaSession>) -> LuaResult<String> {
    let inner = Arc::clone(&this.inner);
    drop(this);
    let task_id = inner.lock().await.id().to_owned();
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
}
