use std::borrow::Cow;
use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tracing::{debug, error, warn};

use crate::mcp::{McpSession, TOOL_SEARCH_TOOL_NAME, UNKNOWN_MCP};
use crate::permissions::canonical_json;
use crate::task_set::TaskSet;
use crate::tools::registry::{PlanModeAccess, ToolInvocation, ToolRegistry};
use crate::tools::{
    DOOM_LOOP_MESSAGE, LocalToolEntry, READ_ONLY_TOOL_RESTRICTED, ToolContext, ToolEffect,
};
use crate::{AgentError, AgentEvent, LuaToolProvenance, ToolDoneEvent, ToolOutput, ToolStartEvent};
use caudra_config::ToolKey;

/// Where a tool's start presentation goes: the transcript, the caller that
/// asked for it, or nowhere.
pub enum Emit<'a> {
    Notify,
    Silent,
    /// Hand the presentation to this callback instead of the transcript, the
    /// moment it is built rather than when the call ends. The batch roster
    /// draws its children itself, and a child that has not introduced itself
    /// is a bare tool name for as long as it runs.
    Capture(&'a mut (dyn FnMut(&ToolStartEvent) + Send)),
}

impl Emit<'_> {
    fn wanted(&self) -> bool {
        !matches!(self, Emit::Silent)
    }

    fn deliver(&mut self, ctx: &ToolContext, start: ToolStartEvent) {
        match self {
            Emit::Notify => {
                let _ = ctx.event_tx.send(AgentEvent::ToolStart(Box::new(start)));
            }
            Emit::Capture(on_start) => on_start(&start),
            Emit::Silent => {}
        }
    }
}

const DOOM_LOOP_THRESHOLD: usize = 3;
const MCP_BLOCKED_IN_PLAN: &str = "MCP tools are not available in plan mode";
const UNKNOWN_TOOL_PREFIX: &str = "unknown tool";
const TOOL_DISABLED_SUFFIX: &str = "is disabled for the current agent";
const SOURCE_NATIVE: &str = "native";
const SOURCE_LOCAL: &str = "local";
const SOURCE_UNKNOWN: &str = "unknown";
/// A name that still carries one means the MCP server behind it is gone.
const MCP_NAME_SEPARATOR: &str = "__";
const BASH_TOOL: &str = "bash";
const SHELL_TOOL: &str = "shell";
const BASH_COMMAND_FIELD: &str = "command";
const GIT_COMMIT: &str = "git commit";
const GH_PR_CREATE: &str = "gh pr create";

const ERROR_CANCELLED: &str = "cancelled";
const ERROR_TIMEOUT: &str = "timeout";
const ERROR_DENIED: &str = "permission_denied";
const ERROR_NOT_FOUND: &str = "not_found";
const ERROR_INVALID_INPUT: &str = "invalid_input";
const ERROR_OTHER: &str = "error";

/// A telemetry counter is not worth an unbounded diff; past this,
/// `similar` returns a coarser but still valid one.
const DIFF_TIMEOUT: Duration = Duration::from_millis(100);

pub(super) struct RecentCalls(VecDeque<(String, u64)>);

impl RecentCalls {
    pub(super) fn new() -> Self {
        Self(VecDeque::new())
    }

    fn hash_input(input: &Value) -> u64 {
        let mut h = DefaultHasher::new();
        input.to_string().hash(&mut h);
        h.finish()
    }

    fn is_doom_loop(&self, name: &str, input: &Value) -> bool {
        let hash = Self::hash_input(input);
        self.0.len() >= DOOM_LOOP_THRESHOLD - 1
            && self
                .0
                .iter()
                .rev()
                .take(DOOM_LOOP_THRESHOLD - 1)
                .all(|(n, h)| n == name && *h == hash)
    }

    fn record(&mut self, name: String, input: &Value) {
        self.0.push_back((name, Self::hash_input(input)));
        if self.0.len() > DOOM_LOOP_THRESHOLD {
            self.0.pop_front();
        }
    }
}

/// Every tool call in caudra lands here (native, Lua, MCP, subagents, batch
/// children), which makes it the one place telemetry has to wrap.
pub async fn run(
    registry: &ToolRegistry,
    mcp: Option<&McpSession>,
    id: String,
    name: &str,
    input: &Value,
    ctx: &ToolContext,
    mut emit: Emit<'_>,
) -> ToolDoneEvent {
    let telemetry = caudra_otel::enabled();
    let canonical = telemetry.then(|| canonical_tool_name(name, ctx));
    let source = canonical.map(|name| tool_source(registry, ctx, name));
    let started = Instant::now();
    let mut done = run_inner(registry, mcp, id, name, input, ctx, &mut emit).await;
    crate::tool_output::limit(&mut done, ctx).await;
    if let (Some(canonical), Some(source)) = (canonical, source) {
        report(&done, canonical, &source, input, started.elapsed());
    }
    done
}

/// Parse errors and unknown tools skip the start event so the UI never
/// shows a phantom spinner.
async fn run_inner(
    registry: &ToolRegistry,
    mcp: Option<&McpSession>,
    id: String,
    name: &str,
    input: &Value,
    ctx: &ToolContext,
    emit: &mut Emit<'_>,
) -> ToolDoneEvent {
    // Covers names re-entering from model JSON (batch children, `call_tool`,
    // the interpreter bridge); streamed names are canonicalized in streaming.rs.
    let name = canonical_tool_name(name, ctx);
    let local = ctx.local_tools.get(name);
    let entry = registry.get(name);
    // LLM providers send tool names in wire format (server__tool) but our
    // internal index uses server.tool. Only convert if the name isn't a
    // native tool — avoids mangling native names that happen to contain __.
    let mcp_name;
    let mcp_lookup = if entry.is_none() && name.contains("__") && mcp.is_some() {
        mcp_name = crate::mcp::internal_tool_name(name);
        mcp_name.as_str()
    } else {
        name
    };
    let tool_id: Arc<str> = entry
        .as_ref()
        .map(|e| Arc::from(e.tool.name()))
        .or_else(|| local.map(|_| Arc::from(name)))
        .or_else(|| mcp.map(|m| m.interned_name(mcp_lookup)))
        .unwrap_or_else(|| Arc::from(UNKNOWN_MCP));
    let started = Instant::now();

    let done_error = |msg: String| {
        let mut output = ToolOutput::Plain(msg.into());
        if let Some(entry) = &entry {
            set_lua_provenance(&mut output, &entry.source, false);
        }
        ToolDoneEvent {
            id: id.clone(),
            tool: Arc::clone(&tool_id),
            output,
            is_error: true,
            annotation: None,
            written_path: None,
            written_paths: Vec::new(),
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
        }
    };

    // Before the read-only gate: a tool the config turned off should say so
    // even when the mode would have refused it for another reason.
    if entry.is_none() && local.is_none() && mcp.is_some_and(|mcp| mcp.is_disabled(mcp_lookup)) {
        return done_error(format!("tool {mcp_lookup} {TOOL_DISABLED_SUFFIX}"));
    }

    if ctx.policy().is_read_only() {
        // A registry entry is gated once its input parses, so a tool whose
        // commands differ in effect is judged per call, not per registration.
        let allowed = match (local, &entry) {
            (Some(local), _) => local.effect.is_safe_in_read_only(),
            (None, Some(_)) => true,
            (None, None) => {
                !mcp.is_some_and(|mcp| name == TOOL_SEARCH_TOOL_NAME || mcp.has_tool(mcp_lookup))
            }
        };
        if !allowed {
            warn!(tool = %name, "blocked tool in strict read-only mode");
            return done_error(format!("{READ_ONLY_TOOL_RESTRICTED}: {name}"));
        }
    }

    if (local.is_some() || entry.is_some()) && !ctx.tool_filter.matches(name) {
        return done_error(format!("tool {name} {TOOL_DISABLED_SUFFIX}"));
    }
    if let Some(local) = local {
        return run_local_tool(local, id, name, input, ctx, emit).await;
    }

    if let Some(ref entry) = entry {
        if !entry.tool.audience().contains(ctx.audience) {
            return done_error(format!(
                "tool {name} is unavailable to the current agent audience"
            ));
        }
        let invocation = match entry.tool.parse(input) {
            Ok(inv) => inv,
            Err(e) => {
                warn!(
                    tool = %name,
                    source = %entry.source.as_log_field(),
                    input_preview = %crate::tools::schema::preview(&input.to_string()),
                    error = %e,
                    "tool input parse failed"
                );
                return done_error(e.to_string());
            }
        };

        let call_effect = entry.effect_for(invocation.as_ref());
        if ctx.policy().is_read_only() && !entry.is_safe_in_read_only_with(call_effect) {
            warn!(tool = %name, effect = call_effect.as_str(), "blocked tool in strict read-only mode");
            return done_error(format!("{READ_ONLY_TOOL_RESTRICTED}: {name}"));
        }

        let mut prepared_intent = match invocation.preflight(ctx).await {
            Ok(intent) => intent,
            Err(error) => return done_error(error),
        };

        let planning = ctx.mode.plan_path().is_some();
        let plan_access = invocation.plan_mode_access();
        if planning && plan_access == PlanModeAccess::Refused {
            warn!(tool = %name, "blocked tool in plan mode");
            return done_error(crate::tools::PLAN_WRITE_RESTRICTED.into());
        }
        // A plan-mode grant must not outlive the plan, so an "allow always"
        // answered while building cannot silently cover this call.
        if planning
            && plan_access == PlanModeAccess::Prompted
            && let Some(intent) = prepared_intent.as_mut()
        {
            intent.scopes.force_prompt = true;
        }

        let mutation_targets = invocation.mutation_targets(ctx);
        if planning && !call_effect.is_safe_in_read_only() && !entry.source.is_trusted() {
            warn!(tool = %name, "blocked untrusted effect in plan mode");
            return done_error(crate::tools::PLAN_WRITE_RESTRICTED.into());
        }
        // A call that named no target cannot be checked against the plan file,
        // unless it already accounted for itself above.
        if planning
            && plan_access == PlanModeAccess::Standard
            && !call_effect.is_safe_in_read_only()
            && mutation_targets.is_empty()
        {
            warn!(tool = %name, "blocked unscoped effect in plan mode");
            return done_error(crate::tools::PLAN_WRITE_RESTRICTED.into());
        }

        for target in &mutation_targets {
            let is_plan_target = ctx
                .mode
                .plan_path()
                .is_some_and(|plan_path| target == plan_path);
            if !is_plan_target {
                if ctx.mode.plan_path().is_some() {
                    warn!(
                        tool = %name,
                        target = %target.display(),
                        "blocked write in plan mode"
                    );
                    return done_error(crate::tools::PLAN_WRITE_RESTRICTED.into());
                }
                if let Some(reason) = ctx.permissions.boundary_block_reason(target) {
                    return done_error(reason);
                }
            }
        }

        if let Err(e) = enforce_permission(
            invocation.as_ref(),
            prepared_intent.as_ref(),
            &entry.source,
            name,
            input,
            ctx,
            &id,
        )
        .await
        {
            return done_error(e);
        }

        let header_result = invocation.start_header().await;
        let start = ToolStartEvent {
            id: id.clone(),
            tool: Arc::clone(&tool_id),
            effect: call_effect,
            summary: header_result.text(),
            render_header: header_result.snapshot(),
            annotation: invocation.start_annotation(),
            input: invocation.start_input(),
            raw_input: Some(input.clone()),
            output: invocation.start_output(ctx),
        };
        emit.deliver(ctx, start);

        invocation.start(ctx).await;

        // Taken after the permission verdict, so a prompt never blocks a
        // sibling's write, and after the start event, so a call waiting on a
        // contended file still renders as a running row. Held across execute:
        // a tool's own stale check, write, and mtime record must not interleave
        // with a concurrent call naming the same file. Not gated on
        // `stale_read_check`; turning that off must not re-enable clobbering.
        let _guards = ctx
            .path_locks
            .acquire(&mutation_targets, &invocation.read_targets(ctx))
            .await;

        let result = invocation.execute(ctx).await;

        let elapsed = started.elapsed();
        match result.output {
            Ok(mut output) => {
                set_lua_provenance(&mut output, &entry.source, true);
                let written_path = result
                    .written_path
                    .or_else(|| result.written_paths.first().cloned());
                debug!(
                    tool = %name,
                    source = %entry.source.as_log_field(),
                    elapsed_ms = elapsed.as_millis() as u64,
                    "tool ok"
                );
                ToolDoneEvent {
                    id,
                    tool: tool_id,
                    output,
                    is_error: result.is_error,
                    annotation: result.annotation,
                    written_path,
                    written_paths: result.written_paths,
                    output_ref: result.output_ref,
                    output_limits: result.output_limits,
                    model_suffix: result.model_suffix,
                    model_output: result.model_output,
                    model_output_from_ref: result.model_output_from_ref,
                }
            }
            Err(message) => {
                warn!(
                    tool = %name,
                    source = %entry.source.as_log_field(),
                    elapsed_ms = elapsed.as_millis() as u64,
                    error = %message,
                    "tool failed"
                );
                let mut done = done_error(message).with_model_suffix(result.model_suffix);
                set_lua_provenance(&mut done.output, &entry.source, true);
                done.output_limits = result.output_limits;
                done.output_ref = result.output_ref;
                done.model_output = result.model_output;
                done.model_output_from_ref = result.model_output_from_ref;
                done
            }
        }
    } else if let Some(mcp) = mcp.filter(|_| name == TOOL_SEARCH_TOOL_NAME) {
        run_tool_search(mcp, id, input, ctx, emit)
    } else if mcp.is_some_and(|m| m.has_tool(mcp_lookup)) {
        emit_raw_start(
            ctx,
            emit,
            &id,
            &tool_id,
            ToolEffect::Unknown,
            format!("mcp: {mcp_lookup}"),
            input,
        );
        execute_mcp_tool(ctx, &id, tool_id, mcp_lookup, input).await
    } else {
        let msg = format!("{UNKNOWN_TOOL_PREFIX}: {mcp_lookup}");
        warn!(tool = %mcp_lookup, "unknown tool");
        done_error(msg)
    }
}

fn canonical_tool_name<'a>(name: &'a str, ctx: &'a ToolContext) -> &'a str {
    let name = super::streaming::canonical_tool_name(name);
    ctx.resolve_tool_name_alias(name)
}

fn set_lua_provenance(
    output: &mut ToolOutput,
    source: &crate::tools::ToolSource,
    error_restore_allowed: bool,
) {
    if let crate::tools::ToolSource::Lua {
        plugin, contract, ..
    } = source
    {
        output.set_lua_provenance(LuaToolProvenance {
            plugin: plugin.to_string(),
            contract: contract.to_string(),
            error_restore_allowed,
        });
    }
}

/// MCP, local, and search tools never go through invocation parsing,
/// so there is no parsed input to show; the UI gets the raw JSON instead.
fn emit_raw_start(
    ctx: &ToolContext,
    emit: &mut Emit<'_>,
    id: &str,
    tool: &Arc<str>,
    effect: ToolEffect,
    summary: String,
    input: &Value,
) {
    if !emit.wanted() {
        return;
    }
    let start = ToolStartEvent {
        id: id.to_owned(),
        tool: Arc::clone(tool),
        effect,
        summary,
        render_header: None,
        annotation: None,
        input: None,
        raw_input: Some(input.clone()),
        output: None,
    };
    emit.deliver(ctx, start);
}

/// Runs without a permission gate: search only reveals names the deferred
/// catalog already showed the model.
fn run_tool_search(
    mcp: &McpSession,
    id: String,
    input: &Value,
    ctx: &ToolContext,
    emit: &mut Emit<'_>,
) -> ToolDoneEvent {
    let tool_id: Arc<str> = Arc::from(TOOL_SEARCH_TOOL_NAME);
    let query = input["query"].as_str().unwrap_or_default();
    emit_raw_start(
        ctx,
        emit,
        &id,
        &tool_id,
        ToolEffect::ReadOnly,
        query.to_owned(),
        input,
    );
    let (output, is_error) = match mcp.search_tools(query) {
        Ok(out) => (out, false),
        Err(e) => (e, true),
    };
    ToolDoneEvent {
        id,
        tool: tool_id,
        output: ToolOutput::Markdown(output.into()),
        is_error,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    }
}

async fn run_local_tool(
    local: &LocalToolEntry,
    id: String,
    name: &str,
    input: &Value,
    ctx: &ToolContext,
    emit: &mut Emit<'_>,
) -> ToolDoneEvent {
    let tool_id: Arc<str> = Arc::from(name);
    emit_raw_start(
        ctx,
        emit,
        &id,
        &tool_id,
        local.effect,
        name.to_owned(),
        input,
    );
    let tool_ctx = ToolContext {
        tool_use_id: Some(id.clone()),
        ..ctx.clone()
    };
    let (output, is_error) = match local.call(input.clone(), tool_ctx).await {
        Ok(output) => (output, false),
        Err(e) => {
            warn!(tool = %name, error = %e, "local tool failed");
            (e, true)
        }
    };
    let mut output = ToolOutput::Plain(output.into());
    output.set_lua_provenance(LuaToolProvenance {
        plugin: "__session_local__".into(),
        contract: String::new(),
        error_restore_allowed: false,
    });
    ToolDoneEvent {
        id,
        tool: tool_id,
        output,
        is_error,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    }
}

/// Enforce permission for a registry tool. MCP tools bypass this — they go
/// through `execute_mcp_tool` which handles permission checking internally.
///
/// Returns an error if `name` contains dots (not a valid native tool name).
async fn enforce_permission(
    inv: &dyn ToolInvocation,
    prepared_intent: Option<&crate::tools::PermissionIntent>,
    source: &crate::tools::ToolSource,
    name: &str,
    input: &Value,
    ctx: &ToolContext,
    id: &str,
) -> Result<(), String> {
    if name.contains('.') {
        return Err(format!(
            "enforce_permission called with dotted name: {name}"
        ));
    }
    let input = inv.permission_input().unwrap_or(input);
    let tool_key = ToolKey::native(name);
    let identity = match source {
        crate::tools::ToolSource::Native {
            owner, contract, ..
        } => Some((
            crate::permissions::PermissionSubject::Native {
                owner: owner.to_string(),
                contract: contract.to_string(),
            },
            crate::permissions::PermissionExecutorKind::Native,
        )),
        crate::tools::ToolSource::Lua {
            plugin,
            contract,
            bundled: _,
        } => Some((
            crate::permissions::PermissionSubject::Lua {
                plugin: plugin.to_string(),
                tool: name.to_owned(),
                contract: contract.to_string(),
            },
            crate::permissions::PermissionExecutorKind::Lua,
        )),
        crate::tools::ToolSource::Mcp { .. } => None,
    };
    let include_builtin_allows = matches!(
        source,
        crate::tools::ToolSource::Native { trusted: true, .. }
            | crate::tools::ToolSource::Lua { bundled: true, .. }
    );
    let computed_intent;
    let intent = match prepared_intent {
        Some(intent) => Some(intent),
        None => {
            computed_intent = inv.permission_intent(ctx).await;
            computed_intent.as_ref()
        }
    };
    if let Some(intent) = intent {
        ctx.permissions
            .enforce_with_intent(
                &tool_key,
                intent,
                input,
                &ctx.event_tx,
                ctx.user_response_rx.as_deref(),
                id,
                &ctx.cancel,
                ctx.mode.plan_path(),
                identity,
                include_builtin_allows,
            )
            .await
            .map_err(|e| e.to_string())?;
    } else {
        let scopes = inv.permission_scopes().await.unwrap_or_else(|| {
            crate::tools::PermissionScopes::single(crate::permissions::canonical_json(input))
        });
        ctx.permissions
            .enforce_with_identity(
                &tool_key,
                &scopes,
                input,
                &ctx.event_tx,
                ctx.user_response_rx.as_deref(),
                id,
                &ctx.cancel,
                ctx.mode.plan_path(),
                identity,
                include_builtin_allows,
            )
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn execute_mcp_tool(
    ctx: &ToolContext,
    id: &str,
    tool_id: Arc<str>,
    tool_name: &str,
    input: &Value,
) -> ToolDoneEvent {
    let done = |output: String, is_error: bool| ToolDoneEvent {
        id: id.to_owned(),
        tool: Arc::clone(&tool_id),
        output: ToolOutput::Plain(output.into()),
        is_error,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    };

    if ctx.policy().is_read_only() {
        return done(format!("{READ_ONLY_TOOL_RESTRICTED}: {tool_name}"), true);
    }

    if ctx.mode.plan_path().is_some() {
        return done(MCP_BLOCKED_IN_PLAN.into(), true);
    }

    let perm_tool = match ToolKey::parse(tool_name) {
        Ok(k) => k,
        Err(e) => {
            return done(format!("invalid MCP tool key '{tool_name}': {e}"), true);
        }
    };
    let perm_scope = canonical_json(input);
    let perm_scopes = crate::tools::PermissionScopes::single(perm_scope);
    let Some(mcp) = &ctx.mcp else {
        return done(format!("MCP manager not available for {tool_name}"), true);
    };
    let binding = match mcp.bind_tool(tool_name) {
        Ok(binding) => binding,
        Err(error) => return done(error.to_string(), true),
    };

    if let Err(e) = ctx
        .permissions
        .enforce_with_identity(
            &perm_tool,
            &perm_scopes,
            input,
            &ctx.event_tx,
            ctx.user_response_rx.as_deref(),
            id,
            &ctx.cancel,
            ctx.mode.plan_path(),
            Some((
                binding.subject().clone(),
                crate::permissions::PermissionExecutorKind::Mcp,
            )),
            false,
        )
        .await
    {
        return done(e.to_string(), true);
    }

    // A permitted call to a deferred tool counts as loading it, so its full
    // definition joins the next request; a denied call must not load anything.
    mcp.mark_loaded(tool_name);
    match binding.call(input).await {
        Ok(text) => done(text, false),
        Err(e) => done(e.to_string(), true),
    }
}

/// Deduplicates doom-loop repeats, then runs remaining calls in parallel.
pub(super) async fn process_tool_calls(
    tool_uses: Vec<(String, String, Value)>,
    recent_calls: &mut RecentCalls,
    mcp: Option<&McpSession>,
    history: &mut super::history::History,
    event_tx: &crate::EventSender,
    ctx: &ToolContext,
) -> Result<(), AgentError> {
    let mut immediate_errors: Vec<ToolDoneEvent> = Vec::new();
    let mut runnable: Vec<(String, String, Value)> = Vec::new();

    for (id, name, input) in tool_uses {
        debug!(
            tool = %name,
            id = %id,
            input_preview = %crate::tools::schema::preview(&input.to_string()),
            "parsing tool call"
        );
        if recent_calls.is_doom_loop(&name, &input) {
            warn!(tool = %name, "doom loop detected, skipping execution");
            immediate_errors.push(ToolDoneEvent::error(id.clone(), DOOM_LOOP_MESSAGE));
        } else {
            runnable.push((id, name.clone(), input.clone()));
        }
        recent_calls.record(name, &input);
    }

    for err in &immediate_errors {
        event_tx.try_send(AgentEvent::ToolDone(Box::new(err.clone())));
    }

    let mut set = TaskSet::new();
    let mut spawned_ids: Vec<String> = Vec::new();
    for (id, name, input) in runnable {
        spawned_ids.push(id.clone());
        let event_tx_clone = ctx.event_tx.clone();
        let tool_ctx = ToolContext {
            tool_use_id: Some(id.clone()),
            root_tool_use_id: ctx.root_tool_use_id.clone().or_else(|| Some(id.clone())),
            ..ctx.clone()
        };
        let mcp_owned = mcp.cloned();
        set.spawn(async move {
            let done = run(
                &tool_ctx.registry,
                mcp_owned.as_ref(),
                id,
                &name,
                &input,
                &tool_ctx,
                Emit::Notify,
            )
            .await;
            event_tx_clone.try_send(AgentEvent::ToolDone(Box::new(done.clone())));
            done
        });
    }

    let mut results = Vec::with_capacity(spawned_ids.len());
    for (result, id) in set.join_all().await.into_iter().zip(spawned_ids) {
        match result {
            Ok(done) => results.push(done),
            Err(error) => {
                error!(%error, "tool task panicked");
                let done = limited_panic_result(id, error, ctx).await;
                event_tx.try_send(AgentEvent::ToolDone(Box::new(done.clone())));
                results.push(done);
            }
        }
    }

    let mut all_results = results;
    all_results.extend(immediate_errors);
    let tool_msg = crate::types::tool_results(all_results);
    event_tx.send(AgentEvent::ToolResultsSubmitted {
        message: Box::new(tool_msg.clone()),
    })?;
    history.push(tool_msg);
    Ok(())
}

async fn limited_panic_result(id: String, error: String, ctx: &ToolContext) -> ToolDoneEvent {
    let mut done = ToolDoneEvent::error(id, format!("internal error: tool panicked: {error}"));
    crate::tool_output::limit(&mut done, ctx).await;
    done
}

fn tool_source(registry: &ToolRegistry, ctx: &ToolContext, name: &str) -> Cow<'static, str> {
    if ctx.local_tools.contains_key(name) {
        return Cow::Borrowed(SOURCE_LOCAL);
    }
    match registry.get(name) {
        Some(entry) => entry.source.as_log_field(),
        None if name.contains(MCP_NAME_SEPARATOR) => Cow::Borrowed(SOURCE_UNKNOWN),
        None => Cow::Borrowed(SOURCE_NATIVE),
    }
}

/// Low-cardinality buckets, because a raw error message would give the
/// collector a new attribute value on every call.
fn classify_error(text: &str) -> &'static str {
    let text = text.to_ascii_lowercase();
    if text.contains("cancel") {
        ERROR_CANCELLED
    } else if text.contains("timed out") || text.contains("timeout") {
        ERROR_TIMEOUT
    } else if text.contains("permission denied") || text.contains("not allowed") {
        ERROR_DENIED
    } else if text.contains("no such file") || text.contains("not found") {
        ERROR_NOT_FOUND
    } else if text.contains("invalid") || text.contains("expected") {
        ERROR_INVALID_INPUT
    } else {
        ERROR_OTHER
    }
}

fn changed_lines(before: &str, after: &str) -> (u64, u64) {
    let mut added = 0;
    let mut removed = 0;
    let diff = similar::TextDiff::configure()
        .timeout(DIFF_TIMEOUT)
        .diff_lines(before, after);
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

/// The same heuristic Claude Code uses: look at what the shell was asked to
/// do, not at what it printed.
fn git_activity(name: &str, input: &Value) {
    if !matches!(name, BASH_TOOL | SHELL_TOOL) {
        return;
    }
    let Some(command) = input.get(BASH_COMMAND_FIELD).and_then(Value::as_str) else {
        return;
    };
    if command.contains(GIT_COMMIT) {
        caudra_otel::emit::commit_created();
    }
    if command.contains(GH_PR_CREATE) {
        caudra_otel::emit::pull_request_created();
    }
}

fn report(done: &ToolDoneEvent, name: &str, source: &str, input: &Value, took: Duration) {
    let error_text = done.is_error.then(|| done.output.as_text());
    let tool_input = caudra_otel::logs_tool_details().then(|| input.to_string());
    caudra_otel::emit::tool_result(&caudra_otel::emit::ToolResult {
        tool_name: name,
        tool_source: source,
        success: !done.is_error,
        duration: took,
        error_type: error_text.as_deref().map(classify_error),
        tool_input: tool_input.as_deref(),
    });
    if let ToolOutput::Diff { before, after, .. } = &done.output {
        let (added, removed) = changed_lines(before, after);
        caudra_otel::emit::lines_of_code(added, removed);
    }
    if !done.is_error {
        git_activity(name, input);
    }
}

/// Test-only entry that skips native lookup, letting plan-mode and MCP tests
/// exercise the dispatch path without registering a fake native tool.
#[cfg(test)]
async fn dispatch_mcp(
    ctx: &ToolContext,
    id: &str,
    tool_name: &str,
    input: &Value,
) -> ToolDoneEvent {
    let tool_id = ctx
        .mcp
        .as_ref()
        .map(|m| m.interned_name(tool_name))
        .unwrap_or_else(|| Arc::from(UNKNOWN_MCP));
    execute_mcp_tool(ctx, id, tool_id, tool_name, input).await
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use caudra_config::{Effect, PermissionRule, PermissionsConfig, ToolKey};
    use caudra_storage::StateDir;
    use caudra_storage::id::SessionRef;
    use caudra_storage::tool_outputs::ToolOutputStore;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::AgentMode;
    use crate::permissions::{PERMISSION_DENIED_PREFIX, PermissionManager};
    use crate::tools::registry::ToolSource;
    use crate::tools::test_support::{GUARDED_TOOL_NAME, GuardedMock};

    fn recent_calls(entries: &[(&str, Value)]) -> RecentCalls {
        let mut rc = RecentCalls::new();
        for (n, v) in entries {
            rc.record(n.to_string(), v);
        }
        rc
    }

    #[test_case("read", &[("read", "/a"), ("read", "/a")], true  ; "triggers_at_threshold")]
    #[test_case("read", &[("read", "/a")],                 false ; "below_threshold")]
    #[test_case("read", &[("read", "/a"), ("read", "/b")], false ; "different_input_breaks_chain")]
    #[test_case("grep", &[("glob", "/a"), ("glob", "/a")], false ; "different_tool_name")]
    #[test_case("bash", &[("bash", "/a"), ("bash", "/b"), ("bash", "/a")], false ; "interrupted_chain")]
    fn doom_loop_detection(name: &str, history: &[(&str, &str)], expected: bool) {
        let entries: Vec<_> = history
            .iter()
            .map(|(n, p)| (*n, serde_json::json!({"path": p})))
            .collect();
        let input = serde_json::json!({"path": "/a"});
        assert_eq!(recent_calls(&entries).is_doom_loop(name, &input), expected);
    }

    fn local_ctx(
        name: &str,
        f: impl Fn(&Value) -> Result<String, String> + Send + Sync + 'static,
    ) -> ToolContext {
        let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
        let mut map = std::collections::HashMap::new();
        map.insert(
            name.to_owned(),
            crate::tools::local_tool(move |input, _ctx| {
                let result = f(&input);
                Box::pin(async move { result })
            }),
        );
        ctx.local_tools = Arc::new(map);
        ctx
    }

    #[test]
    fn local_tool_shadows_registry_and_maps_errors() {
        smol::block_on(async {
            let ctx = local_ctx("batch", |input| Ok(format!("local:{}", input["path"])));
            let done = run(
                ToolRegistry::global(),
                None,
                "t1".into(),
                "batch",
                &serde_json::json!({"path": "/a"}),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(!done.is_error);
            assert_eq!(done.output.as_text(), r#"local:"/a""#);
            assert_eq!(
                done.output
                    .lua_provenance()
                    .map(|provenance| provenance.plugin.as_str()),
                Some("__session_local__")
            );

            let ctx = local_ctx("boom", |_| Err("nope".into()));
            let done = run(
                ToolRegistry::global(),
                None,
                "t2".into(),
                "boom",
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(done.is_error);
            assert_eq!(done.output.as_text(), "nope");
        });
    }

    #[test]
    fn oauth_aliases_resolve_losslessly_before_dispatch() {
        const WIRE_NAME: &str = "mcp_File_grep_3e49f5027c6a";

        smol::block_on(async {
            let mut ctx = local_ctx("file_grep", |_| Ok("matched".into()));
            ctx.tool_name_aliases = Some(Arc::new(HashMap::from([(
                WIRE_NAME.into(),
                "file_grep".into(),
            )])));

            let done = run(
                ToolRegistry::global(),
                None,
                "alias".into(),
                WIRE_NAME,
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;

            assert!(!done.is_error);
            assert_eq!(done.output.as_text(), "matched");
            assert_eq!(done.tool.as_ref(), "file_grep");
        });
    }

    #[test]
    fn canonical_tool_name_preserves_unmapped_mcp_prefix() {
        let ctx = local_ctx("mcp_fetch", |_| Ok("matched".into()));
        assert_eq!(canonical_tool_name("mcp_fetch", &ctx), "mcp_fetch");
    }

    #[test]
    fn read_only_local_tools_fail_closed_without_an_audit() {
        smol::block_on(async {
            let mut ctx = local_ctx("forged_local", |_| Ok("ran".into()));
            ctx.mode = AgentMode::ReadOnly;

            let done = run(
                &ctx.registry,
                None,
                "local-read-only".into(),
                "forged_local",
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;

            assert!(done.is_error);
            assert!(done.output.as_text().starts_with(READ_ONLY_TOOL_RESTRICTED));
        });
    }

    #[test]
    fn audited_local_tool_inherits_read_only_context() {
        smol::block_on(async {
            let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::ReadOnly);
            ctx.local_tools = Arc::new(std::collections::HashMap::from([(
                "isolated_local".to_owned(),
                crate::tools::audited_local_tool(crate::tools::ToolEffect::Isolated, |_, ctx| {
                    Box::pin(async move { Ok(format!("{:?}", ctx.mode)) })
                }),
            )]));

            let done = run(
                &ctx.registry,
                None,
                "local-audited".into(),
                "isolated_local",
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;

            assert!(!done.is_error);
            assert_eq!(done.output.as_text(), "ReadOnly");
        });
    }

    #[test]
    fn public_dispatch_limits_and_persists_local_tool_output() {
        smol::block_on(async {
            let temp = TempDir::new().unwrap();
            let store = Arc::new(ToolOutputStore::new(StateDir::from_path(
                temp.path().to_path_buf(),
            )));
            let session = SessionRef::generate();
            let full_output = (0..20)
                .map(|line| format!("dispatch-{line}-{}", "x".repeat(100)))
                .collect::<Vec<_>>()
                .join("\n");
            let tool_output = full_output.clone();
            let mut ctx = local_ctx("large_local", move |_| Ok(tool_output.clone()));
            ctx.config.max_output_lines = 10;
            ctx.config.max_output_bytes = 320;
            ctx.session_id = Some(session.clone());
            ctx.tool_output_store = Some(Arc::clone(&store));

            let done = run(
                ToolRegistry::global(),
                None,
                "t1".into(),
                "large_local",
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;

            assert!(!done.is_error);
            assert!(done.output.as_text().len() <= ctx.config.max_output_bytes);
            assert!(done.model_output.as_ref().unwrap().len() <= ctx.config.max_output_bytes);
            let output_ref = done.output_ref.as_ref().unwrap();
            assert_eq!(
                store
                    .read(session.id(), output_ref.id, 1, 2_000)
                    .unwrap()
                    .text,
                full_output
            );
        });
    }

    #[test]
    fn synthesized_panic_result_is_centrally_limited() {
        smol::block_on(async {
            let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            ctx.config.max_output_lines = 6;
            ctx.config.max_output_bytes = 240;

            let done = limited_panic_result(
                "panic-1".into(),
                format!("panic details\n{}", "backtrace\n".repeat(1_000)),
                &ctx,
            )
            .await;

            assert!(done.is_error);
            assert!(done.output.as_text().len() <= ctx.config.max_output_bytes);
            assert!(done.output.as_text().lines().count() <= ctx.config.max_output_lines);
            let model_output = done.model_output.as_deref().unwrap();
            assert!(model_output.len() <= ctx.config.max_output_bytes);
            assert!(model_output.lines().count() <= ctx.config.max_output_lines);
            assert!(model_output.contains("Full output was unavailable"));
        });
    }

    #[test]
    fn functions_prefixed_name_dispatches_to_canonical_tool() {
        smol::block_on(async {
            let ctx = local_ctx("ok", |_| Ok("ran".into()));
            let done = run(
                ToolRegistry::global(),
                None,
                "t1".into(),
                "functions.ok",
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(!done.is_error);
            assert_eq!(done.output.as_text(), "ran");
        });
    }

    #[test]
    fn local_tool_notify_emits_tool_start_with_raw_input() {
        smol::block_on(async {
            let (tx, rx) = flume::unbounded::<crate::Envelope>();
            let event_tx = crate::EventSender::new(tx, 0);
            let mut ctx =
                crate::tools::test_support::stub_ctx_with(&AgentMode::Build, Some(&event_tx), None);
            let mut map = std::collections::HashMap::new();
            map.insert(
                "local_echo".to_owned(),
                crate::tools::local_tool(|input, _ctx| {
                    let out = input.to_string();
                    Box::pin(async move { Ok(out) })
                }),
            );
            ctx.local_tools = Arc::new(map);

            let input = serde_json::json!({"path": "/a"});
            let done = run(
                ToolRegistry::global(),
                None,
                "t1".into(),
                "local_echo",
                &input,
                &ctx,
                Emit::Notify,
            )
            .await;
            assert!(!done.is_error);

            let envelope = rx
                .try_recv()
                .expect("ToolStart must be emitted before the tool completes");
            let AgentEvent::ToolStart(start) = envelope.event else {
                panic!("expected ToolStart, got {:?}", envelope.event);
            };
            assert_eq!(start.tool.as_ref(), "local_echo");
            assert_eq!(start.summary, "local_echo");
            assert_eq!(start.raw_input, Some(input));
        });
    }

    #[test]
    fn tool_search_routes_and_loads_matches() {
        smol::block_on(async {
            let mcp = crate::mcp::stub_session(&[("srv.fetch_issue", "Fetch a GitHub issue")]);
            let ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            let done = run(
                ToolRegistry::global(),
                Some(&mcp),
                "t1".into(),
                TOOL_SEARCH_TOOL_NAME,
                &serde_json::json!({"query": "issue"}),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(!done.is_error, "got: {}", done.output.as_text());
            assert_eq!(done.tool.as_ref(), TOOL_SEARCH_TOOL_NAME);
            assert!(done.output.as_text().contains("srv__fetch_issue"));

            let mut tools = serde_json::json!([]);
            mcp.request_snapshot().extend_tools(&mut tools);
            assert!(
                crate::mcp::tool_names(&tools).contains(&"srv__fetch_issue"),
                "searched tool must join the next request"
            );
        });
    }

    #[test_case(serde_json::json!({"query": "  "}) ; "blank_query")]
    #[test_case(serde_json::json!({}) ; "missing_query")]
    fn tool_search_bad_query_is_error_event(input: Value) {
        smol::block_on(async {
            let mcp = crate::mcp::stub_session(&[("srv.tool", "")]);
            let ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            let done = run(
                ToolRegistry::global(),
                Some(&mcp),
                "t1".into(),
                TOOL_SEARCH_TOOL_NAME,
                &input,
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(done.is_error);
            assert_eq!(done.output.as_text(), crate::mcp::SEARCH_EMPTY_QUERY);
        });
    }

    /// A name the model kept from earlier in the history must not slip past a
    /// tool the config has since turned off.
    #[test_case("srv.fetch_issue" ; "qualified_name")]
    #[test_case("srv.*" ; "server_wildcard")]
    fn disabled_mcp_tool_from_history_is_refused(disabled: &str) {
        smol::block_on(async {
            let mcp = crate::mcp::stub_session(&[("srv.fetch_issue", "")])
                .with_disabled_tools(&[disabled.to_owned()]);
            let ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            let done = run(
                ToolRegistry::global(),
                Some(&mcp),
                "t1".into(),
                "srv__fetch_issue",
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(done.is_error);
            assert!(
                done.output.as_text().contains(TOOL_DISABLED_SUFFIX),
                "{}",
                done.output.as_text()
            );
        });
    }

    #[test]
    fn calling_deferred_mcp_tool_marks_it_loaded() {
        smol::block_on(async {
            let mcp = crate::mcp::stub_session(&[("srv.fetch_issue", "")]);
            let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            ctx.mcp = Some(mcp.clone());
            let done = run(
                ToolRegistry::global(),
                Some(&mcp),
                "t1".into(),
                "srv__fetch_issue",
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert_eq!(done.tool.as_ref(), "srv.fetch_issue", "must route to MCP");

            let mut tools = serde_json::json!([]);
            mcp.request_snapshot().extend_tools(&mut tools);
            assert_eq!(
                crate::mcp::tool_names(&tools),
                vec!["srv__fetch_issue"],
                "called tool must join the next request"
            );
        });
    }

    #[test]
    fn denied_mcp_call_does_not_load_definition() {
        smol::block_on(async {
            let mcp = crate::mcp::stub_session(&[("srv.fetch_issue", "")]);
            let deny_cfg = PermissionsConfig {
                rules: vec![PermissionRule {
                    tool: ToolKey::parse("srv.fetch_issue").unwrap(),
                    scope: None,
                    effect: Effect::Deny,
                }],
                ..Default::default()
            };
            let dir = TempDir::new().unwrap();
            let permissions = Arc::new(PermissionManager::new_nonpersistent(
                deny_cfg,
                dir.path().to_path_buf(),
                Arc::default(),
            ));
            let mut ctx = crate::tools::test_support::stub_ctx_with_permissions(
                &AgentMode::Build,
                permissions,
            );
            ctx.mcp = Some(mcp.clone());
            let done = run(
                ToolRegistry::global(),
                Some(&mcp),
                "t1".into(),
                "srv__fetch_issue",
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(done.is_error);
            assert!(
                done.output.as_text().starts_with(PERMISSION_DENIED_PREFIX),
                "got: {}",
                done.output.as_text()
            );

            let mut tools = serde_json::json!([]);
            mcp.request_snapshot().extend_tools(&mut tools);
            assert_eq!(
                crate::mcp::tool_names(&tools),
                vec![TOOL_SEARCH_TOOL_NAME],
                "denied call must not load the definition"
            );
        });
    }

    #[test]
    fn local_tool_named_tool_search_shadows_mcp_search() {
        smol::block_on(async {
            let mcp = crate::mcp::stub_session(&[("srv.tool", "")]);
            let ctx = local_ctx(TOOL_SEARCH_TOOL_NAME, |_| Ok("local wins".into()));
            let done = run(
                ToolRegistry::global(),
                Some(&mcp),
                "t1".into(),
                TOOL_SEARCH_TOOL_NAME,
                &serde_json::json!({"query": "tool"}),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert_eq!(done.output.as_text(), "local wins");
        });
    }

    #[test]
    fn unknown_tool_returns_error_event() {
        smol::block_on(async {
            let ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            let done = run(
                &ctx.registry,
                None,
                "t1".into(),
                "nonexistent.tool",
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(done.is_error);
            assert_eq!(done.tool.as_ref(), UNKNOWN_MCP);
            let text = done.output.as_text();
            assert!(text.starts_with(UNKNOWN_TOOL_PREFIX));
            assert!(text.contains("nonexistent.tool"));
        });
    }

    #[test]
    fn mcp_tool_blocked_in_plan_mode() {
        smol::block_on(async {
            let result = dispatch_mcp(
                &crate::tools::test_support::stub_ctx(&AgentMode::Plan(PathBuf::from(
                    "/tmp/plan.md",
                ))),
                "t1",
                "myserver.mytool",
                &serde_json::json!({}),
            )
            .await;
            assert!(result.is_error);
            assert_eq!(result.output.as_text(), MCP_BLOCKED_IN_PLAN);
        });
    }

    #[test]
    fn forged_mcp_tool_is_blocked_in_read_only_mode() {
        smol::block_on(async {
            let result = dispatch_mcp(
                &crate::tools::test_support::stub_ctx(&AgentMode::ReadOnly),
                "t1",
                "myserver.mytool",
                &serde_json::json!({}),
            )
            .await;
            assert!(result.is_error);
            assert_eq!(
                result.output.as_text(),
                format!("{READ_ONLY_TOOL_RESTRICTED}: myserver.mytool")
            );
        });
    }

    #[test]
    fn mcp_tool_errors_without_mcp_manager() {
        smol::block_on(async {
            let result = dispatch_mcp(
                &crate::tools::test_support::stub_ctx(&AgentMode::Build),
                "t1",
                "myserver.mytool",
                &serde_json::json!({}),
            )
            .await;
            assert!(result.is_error);
            assert!(result.output.as_text().contains("not available"));
        });
    }

    #[test]
    fn permission_denial_short_circuits_execute() {
        smol::block_on(async {
            let deny_cfg = PermissionsConfig {
                rules: vec![PermissionRule {
                    tool: ToolKey::native(GUARDED_TOOL_NAME),
                    scope: None,
                    effect: Effect::Deny,
                }],
                ..Default::default()
            };
            let dir = TempDir::new().unwrap();
            let permissions = Arc::new(PermissionManager::new_nonpersistent(
                deny_cfg,
                dir.path().to_path_buf(),
                Arc::default(),
            ));
            let ctx = crate::tools::test_support::stub_ctx_with_permissions(
                &AgentMode::Build,
                permissions,
            );

            let registry = ToolRegistry::new();
            registry
                .register(
                    Arc::new(GuardedMock),
                    ToolSource::Lua {
                        plugin: "test".into(),
                        contract: "test-contract".into(),
                        bundled: false,
                    },
                )
                .unwrap();

            let done = run(
                &registry,
                None,
                "t1".into(),
                GUARDED_TOOL_NAME,
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;

            assert!(done.is_error, "permission denial must produce error event");
            assert!(
                done.output.as_text().starts_with(PERMISSION_DENIED_PREFIX),
                "error should be the permission-denied message, got: {}",
                done.output.as_text()
            );
            assert_eq!(
                done.output.lua_provenance(),
                Some(&LuaToolProvenance {
                    plugin: "test".into(),
                    contract: "test-contract".into(),
                    error_restore_allowed: false,
                })
            );
        });
    }

    const START_PROBE_NAME: &str = "start_probe";

    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::ToolInput;
    use crate::tools::{
        BoxFuture, DescriptionContext, ExecFuture, HeaderFuture, HeaderResult, ParseError,
        PermissionScopes, Tool, ToolExecResult,
    };

    #[derive(Default)]
    struct StartProbe {
        started: Arc<AtomicBool>,
        executed: Arc<AtomicBool>,
    }

    struct StartProbeInvocation {
        started: Arc<AtomicBool>,
        executed: Arc<AtomicBool>,
    }

    impl ToolInvocation for StartProbeInvocation {
        fn start_header(&self) -> HeaderFuture {
            HeaderFuture::Ready(HeaderResult::plain("probe".into()))
        }
        fn start<'a>(&'a self, _ctx: &'a ToolContext) -> BoxFuture<'a, ()> {
            self.started.store(true, Ordering::SeqCst);
            Box::pin(std::future::ready(()))
        }
        fn permission_scopes(&self) -> BoxFuture<'_, Option<PermissionScopes>> {
            Box::pin(std::future::ready(Some(PermissionScopes::single(
                "probe".into(),
            ))))
        }
        fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
            self.executed.store(true, Ordering::SeqCst);
            Box::pin(async {
                ToolExecResult::from(Ok::<_, String>(ToolOutput::Plain("ok".into())))
            })
        }
    }

    impl Tool for StartProbe {
        fn name(&self) -> &str {
            START_PROBE_NAME
        }
        fn description(&self, _ctx: &DescriptionContext) -> std::borrow::Cow<'_, str> {
            "start probe".into()
        }
        fn schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false})
        }
        fn parse(&self, _input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            Ok(Box::new(StartProbeInvocation {
                started: Arc::clone(&self.started),
                executed: Arc::clone(&self.executed),
            }))
        }
    }

    struct ReplacementTaskProbe {
        executed: Arc<AtomicBool>,
    }

    struct ReplacementTaskInvocation {
        executed: Arc<AtomicBool>,
    }

    struct NativeProbe {
        name: &'static str,
        executed: Arc<AtomicBool>,
        targets: Vec<PathBuf>,
        rich_result: bool,
    }

    struct NativeProbeInvocation {
        executed: Arc<AtomicBool>,
        targets: Vec<PathBuf>,
        rich_result: bool,
    }

    impl ToolInvocation for NativeProbeInvocation {
        fn start_header(&self) -> HeaderFuture {
            HeaderFuture::Ready(HeaderResult::plain("native probe".into()))
        }

        fn start_input(&self) -> Option<ToolInput> {
            Some(ToolInput::Code {
                language: "rust".into(),
                code: "fn main() {}".into(),
            })
        }

        fn mutation_targets(&self, _ctx: &ToolContext) -> Vec<PathBuf> {
            self.targets.clone()
        }

        fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
            self.executed.store(true, Ordering::SeqCst);
            Box::pin(async move {
                let result = ToolExecResult::from(Ok::<_, String>(ToolOutput::Plain("ok".into())));
                if self.rich_result {
                    result
                        .with_written_paths(vec!["first.rs".into(), "second.rs".into()])
                        .with_model_output(Some("native model output".into()))
                } else {
                    result
                }
            })
        }
    }

    impl Tool for NativeProbe {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self, _ctx: &DescriptionContext) -> std::borrow::Cow<'_, str> {
            "native probe".into()
        }

        fn schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        fn parse(&self, _input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            Ok(Box::new(NativeProbeInvocation {
                executed: Arc::clone(&self.executed),
                targets: self.targets.clone(),
                rich_result: self.rich_result,
            }))
        }
    }

    const EFFECT_PROBE_NAME: &str = "effect_probe";
    const PLAN_PATH: &str = "/tmp/plan.md";

    /// Registered mutating while each call declares its own effect, the way
    /// `memory` browses and writes through a single registration.
    struct EffectProbe {
        call_effect: ToolEffect,
        executed: Arc<AtomicBool>,
    }

    struct EffectProbeInvocation {
        call_effect: ToolEffect,
        executed: Arc<AtomicBool>,
    }

    impl ToolInvocation for EffectProbeInvocation {
        fn start_header(&self) -> HeaderFuture {
            HeaderFuture::Ready(HeaderResult::plain("effect probe".into()))
        }

        fn call_effect(&self, _registered: ToolEffect) -> ToolEffect {
            self.call_effect
        }

        fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
            self.executed.store(true, Ordering::SeqCst);
            Box::pin(async {
                ToolExecResult::from(Ok::<_, String>(ToolOutput::Plain("ok".into())))
            })
        }
    }

    impl Tool for EffectProbe {
        fn name(&self) -> &str {
            EFFECT_PROBE_NAME
        }

        fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
            "effect probe".into()
        }

        fn schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        fn has_read_only_calls(&self) -> bool {
            true
        }

        fn parse(&self, _input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            Ok(Box::new(EffectProbeInvocation {
                call_effect: self.call_effect,
                executed: Arc::clone(&self.executed),
            }))
        }
    }

    fn trusted_native_source() -> ToolSource {
        ToolSource::Native {
            owner: "caudra".into(),
            contract: "effect-probe/v1".into(),
            trusted: true,
        }
    }

    /// Reports the outcome alongside whether the call reached `execute`.
    async fn run_effect_probe(
        mode: &AgentMode,
        call_effect: ToolEffect,
        source: ToolSource,
    ) -> (ToolDoneEvent, bool) {
        let executed = Arc::new(AtomicBool::new(false));
        let registry = ToolRegistry::new();
        registry
            .register_audited(
                Arc::new(EffectProbe {
                    call_effect,
                    executed: Arc::clone(&executed),
                }),
                source,
                ToolEffect::Mutating,
            )
            .unwrap();
        let done = run(
            &registry,
            None,
            EFFECT_PROBE_NAME.into(),
            EFFECT_PROBE_NAME,
            &serde_json::json!({}),
            &crate::tools::test_support::stub_ctx(mode),
            Emit::Silent,
        )
        .await;
        (done, executed.load(Ordering::SeqCst))
    }

    #[test_case(AgentMode::Plan(PLAN_PATH.into()) ; "plan_mode")]
    #[test_case(AgentMode::ReadOnly ; "read_only_mode")]
    fn a_read_only_call_of_a_mutating_tool_runs(mode: AgentMode) {
        smol::block_on(async {
            let (done, executed) =
                run_effect_probe(&mode, ToolEffect::ReadOnly, trusted_native_source()).await;

            assert!(!done.is_error, "{}", done.output.as_text());
            assert!(executed);
        });
    }

    #[test_case(AgentMode::Plan(PLAN_PATH.into()), crate::tools::PLAN_WRITE_RESTRICTED ; "plan_mode")]
    #[test_case(AgentMode::ReadOnly, READ_ONLY_TOOL_RESTRICTED ; "read_only_mode")]
    fn a_mutating_call_of_the_same_tool_is_refused(mode: AgentMode, expected: &str) {
        smol::block_on(async {
            let (done, executed) =
                run_effect_probe(&mode, ToolEffect::Mutating, trusted_native_source()).await;

            assert!(done.is_error);
            assert!(
                done.output.as_text().starts_with(expected),
                "{}",
                done.output.as_text()
            );
            assert!(!executed);
        });
    }

    #[test_case(AgentMode::Plan(PLAN_PATH.into()), crate::tools::PLAN_WRITE_RESTRICTED ; "plan_mode")]
    #[test_case(AgentMode::ReadOnly, READ_ONLY_TOOL_RESTRICTED ; "read_only_mode")]
    fn an_unbundled_plugin_cannot_downgrade_its_own_call_effect(mode: AgentMode, expected: &str) {
        smol::block_on(async {
            let (done, executed) = run_effect_probe(
                &mode,
                ToolEffect::ReadOnly,
                ToolSource::Lua {
                    plugin: "external".into(),
                    contract: "effect-probe/v1".into(),
                    bundled: false,
                },
            )
            .await;

            assert!(done.is_error);
            assert!(
                done.output.as_text().starts_with(expected),
                "{}",
                done.output.as_text()
            );
            assert!(!executed);
        });
    }

    #[test]
    fn forged_native_and_unbundled_lua_calls_are_blocked_before_execution() {
        smol::block_on(async {
            let registry = ToolRegistry::new();
            let native_executed = Arc::new(AtomicBool::new(false));
            registry
                .register(
                    Arc::new(NativeProbe {
                        name: "unknown_native",
                        executed: Arc::clone(&native_executed),
                        targets: Vec::new(),
                        rich_result: false,
                    }),
                    ToolSource::Native {
                        owner: "caudra".into(),
                        contract: "unknown-native/v1".into(),
                        trusted: true,
                    },
                )
                .unwrap();
            let lua_executed = Arc::new(AtomicBool::new(false));
            registry
                .register_audited(
                    Arc::new(NativeProbe {
                        name: "claimed_safe_lua",
                        executed: Arc::clone(&lua_executed),
                        targets: Vec::new(),
                        rich_result: false,
                    }),
                    ToolSource::Lua {
                        plugin: "external".into(),
                        contract: "claimed-safe/v1".into(),
                        bundled: false,
                    },
                    crate::tools::ToolEffect::ReadOnly,
                )
                .unwrap();
            let ctx = crate::tools::test_support::stub_ctx(&AgentMode::ReadOnly);

            for name in ["unknown_native", "claimed_safe_lua"] {
                let done = run(
                    &registry,
                    None,
                    format!("forged-{name}"),
                    name,
                    &serde_json::json!({}),
                    &ctx,
                    Emit::Silent,
                )
                .await;
                assert!(done.is_error);
                assert!(done.output.as_text().starts_with(READ_ONLY_TOOL_RESTRICTED));
            }
            assert!(!native_executed.load(Ordering::SeqCst));
            assert!(!lua_executed.load(Ordering::SeqCst));
        });
    }

    #[test]
    fn plan_mode_blocks_unscoped_mutating_lua_tool() {
        smol::block_on(async {
            let executed = Arc::new(AtomicBool::new(false));
            let registry = ToolRegistry::new();
            registry
                .register_audited(
                    Arc::new(NativeProbe {
                        name: "memory_like_tool",
                        executed: Arc::clone(&executed),
                        targets: Vec::new(),
                        rich_result: false,
                    }),
                    ToolSource::Lua {
                        plugin: "bundled".into(),
                        contract: "memory-like/v1".into(),
                        bundled: true,
                    },
                    crate::tools::ToolEffect::Mutating,
                )
                .unwrap();
            let ctx = crate::tools::test_support::stub_ctx(&AgentMode::Plan("/tmp/plan.md".into()));

            let done = run(
                &registry,
                None,
                "plan-memory".into(),
                "memory_like_tool",
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;

            assert!(done.is_error);
            assert_eq!(done.output.as_text(), crate::tools::PLAN_WRITE_RESTRICTED);
            assert!(!executed.load(Ordering::SeqCst));
        });
    }

    #[test]
    fn plan_mode_does_not_trust_unbundled_lua_mutation_targets() {
        smol::block_on(async {
            let plan_path = PathBuf::from("/tmp/plan.md");
            let executed = Arc::new(AtomicBool::new(false));
            let registry = ToolRegistry::new();
            registry
                .register_audited(
                    Arc::new(NativeProbe {
                        name: "untrusted_plan_writer",
                        executed: Arc::clone(&executed),
                        targets: vec![plan_path.clone()],
                        rich_result: false,
                    }),
                    ToolSource::Lua {
                        plugin: "external".into(),
                        contract: "untrusted-plan-writer/v1".into(),
                        bundled: false,
                    },
                    crate::tools::ToolEffect::Unknown,
                )
                .unwrap();
            let ctx = crate::tools::test_support::stub_ctx(&AgentMode::Plan(plan_path));

            let done = run(
                &registry,
                None,
                "plan-untrusted".into(),
                "untrusted_plan_writer",
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;

            assert!(done.is_error);
            assert_eq!(done.output.as_text(), crate::tools::PLAN_WRITE_RESTRICTED);
            assert!(!executed.load(Ordering::SeqCst));
        });
    }

    #[test]
    fn bundled_audited_read_only_tool_can_execute_for_research_child() {
        smol::block_on(async {
            let executed = Arc::new(AtomicBool::new(false));
            let registry = ToolRegistry::new();
            registry
                .register_audited(
                    Arc::new(NativeProbe {
                        name: "bundled_read",
                        executed: Arc::clone(&executed),
                        targets: Vec::new(),
                        rich_result: false,
                    }),
                    ToolSource::Lua {
                        plugin: "bundled".into(),
                        contract: "bundled-read/v1".into(),
                        bundled: true,
                    },
                    crate::tools::ToolEffect::ReadOnly,
                )
                .unwrap();
            let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            ctx.audience = crate::tools::ToolAudience::RESEARCH_SUB;

            let done = run(
                &registry,
                None,
                "bundled-read".into(),
                "bundled_read",
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;

            assert!(!done.is_error, "{}", done.output.as_text());
            assert!(executed.load(Ordering::SeqCst));
        });
    }

    impl ToolInvocation for ReplacementTaskInvocation {
        fn start_header(&self) -> HeaderFuture {
            HeaderFuture::Ready(HeaderResult::plain("replacement task".into()))
        }

        fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
            self.executed.store(true, Ordering::SeqCst);
            Box::pin(async {
                ToolExecResult::from(Ok::<_, String>(ToolOutput::Plain("ok".into())))
            })
        }
    }

    impl Tool for ReplacementTaskProbe {
        fn name(&self) -> &str {
            "task"
        }

        fn description(&self, _ctx: &DescriptionContext) -> std::borrow::Cow<'_, str> {
            "replacement task".into()
        }

        fn schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        fn parse(&self, _input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            Ok(Box::new(ReplacementTaskInvocation {
                executed: Arc::clone(&self.executed),
            }))
        }
    }

    #[test]
    fn unscoped_replacement_does_not_inherit_builtin_trust() {
        smol::block_on(async {
            let dir = TempDir::new().unwrap();
            let permissions = Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig::default(),
                dir.path().to_path_buf(),
                Arc::default(),
            ));
            let ctx = crate::tools::test_support::stub_ctx_with_permissions(
                &AgentMode::Build,
                permissions,
            );
            let executed = Arc::new(AtomicBool::new(false));
            let registry = ToolRegistry::new();
            registry
                .register(
                    Arc::new(ReplacementTaskProbe {
                        executed: Arc::clone(&executed),
                    }),
                    ToolSource::Lua {
                        plugin: "replacement".into(),
                        contract: "replacement-contract".into(),
                        bundled: false,
                    },
                )
                .unwrap();

            let done = run(
                &registry,
                None,
                "t1".into(),
                "task",
                &serde_json::json!({"prompt": "do something"}),
                &ctx,
                Emit::Silent,
            )
            .await;

            assert!(done.is_error);
            assert!(done.output.as_text().starts_with(PERMISSION_DENIED_PREFIX));
            assert!(!executed.load(Ordering::SeqCst));
        });
    }

    #[test]
    fn native_trust_controls_builtin_allows() {
        smol::block_on(async {
            for (trusted, expected_error) in [(false, true), (true, false)] {
                let dir = TempDir::new().unwrap();
                let permissions = Arc::new(PermissionManager::new_nonpersistent(
                    PermissionsConfig::default(),
                    dir.path().to_path_buf(),
                    Arc::default(),
                ));
                let ctx = crate::tools::test_support::stub_ctx_with_permissions(
                    &AgentMode::Build,
                    permissions,
                );
                let executed = Arc::new(AtomicBool::new(false));
                let registry = ToolRegistry::new();
                registry
                    .register(
                        Arc::new(ReplacementTaskProbe {
                            executed: Arc::clone(&executed),
                        }),
                        ToolSource::Native {
                            owner: "caudra".into(),
                            contract: "task/v1".into(),
                            trusted,
                        },
                    )
                    .unwrap();

                let done = run(
                    &registry,
                    None,
                    "native-trust".into(),
                    "task",
                    &serde_json::json!({"prompt": "do something"}),
                    &ctx,
                    Emit::Silent,
                )
                .await;

                assert_eq!(done.is_error, expected_error);
                assert_eq!(executed.load(Ordering::SeqCst), !expected_error);
            }
        });
    }

    #[test]
    fn dispatcher_uses_native_owner_and_contract_identity() {
        smol::block_on(async {
            let dir = TempDir::new().unwrap();
            let permissions = Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig::default(),
                dir.path().to_path_buf(),
                Arc::default(),
            ));
            let mut ctx = crate::tools::test_support::stub_ctx_with_permissions(
                &AgentMode::Build,
                Arc::clone(&permissions),
            );
            let (event_tx, event_rx) = flume::unbounded();
            ctx.event_tx = crate::EventSender::new(event_tx, 0);
            let (_response_tx, response_rx) = flume::unbounded();
            ctx.user_response_rx = Some(Arc::new(async_lock::Mutex::new(response_rx)));
            let registry = Arc::new(ToolRegistry::new());
            registry
                .register(
                    Arc::new(GuardedMock),
                    ToolSource::Native {
                        owner: "first-party".into(),
                        contract: "guarded/v1".into(),
                        trusted: false,
                    },
                )
                .unwrap();
            let task = smol::spawn({
                let registry = Arc::clone(&registry);
                async move {
                    run(
                        &registry,
                        None,
                        "native-identity".into(),
                        GUARDED_TOOL_NAME,
                        &serde_json::json!({}),
                        &ctx,
                        Emit::Silent,
                    )
                    .await
                }
            });

            let event = event_rx.recv_async().await.unwrap().event;
            let AgentEvent::PermissionRequest(request) = event else {
                panic!("expected permission request, got {event:?}");
            };
            assert_eq!(
                request.subject,
                crate::permissions::PermissionSubject::Native {
                    owner: "first-party".into(),
                    contract: "guarded/v1".into(),
                }
            );
            assert_eq!(
                request.executor,
                crate::permissions::PermissionExecutorKind::Native
            );
            assert!(permissions.answer(
                "native-identity",
                crate::permissions::PermissionAnswer::Deny
            ));
            assert!(task.await.is_error);
        });
    }

    #[test]
    fn every_mutation_target_is_checked_in_plan_mode() {
        smol::block_on(async {
            let plan_path = PathBuf::from("/tmp/plan.md");
            let executed = Arc::new(AtomicBool::new(false));
            let registry = ToolRegistry::new();
            registry
                .register(
                    Arc::new(NativeProbe {
                        name: "native_patch",
                        executed: Arc::clone(&executed),
                        targets: vec![plan_path.clone(), PathBuf::from("/tmp/other.rs")],
                        rich_result: false,
                    }),
                    ToolSource::Native {
                        owner: "caudra".into(),
                        contract: "patch/v1".into(),
                        trusted: true,
                    },
                )
                .unwrap();
            let ctx = crate::tools::test_support::stub_ctx(&AgentMode::Plan(plan_path));

            let done = run(
                &registry,
                None,
                "mutation-targets".into(),
                "native_patch",
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;

            assert!(done.is_error);
            assert_eq!(done.output.as_text(), crate::tools::PLAN_WRITE_RESTRICTED);
            assert!(!executed.load(Ordering::SeqCst));
        });
    }

    #[test]
    fn native_start_input_and_result_metadata_reach_events() {
        smol::block_on(async {
            let executed = Arc::new(AtomicBool::new(false));
            let registry = ToolRegistry::new();
            registry
                .register(
                    Arc::new(NativeProbe {
                        name: "native_result",
                        executed: Arc::clone(&executed),
                        targets: Vec::new(),
                        rich_result: true,
                    }),
                    ToolSource::Native {
                        owner: "caudra".into(),
                        contract: "result/v1".into(),
                        trusted: true,
                    },
                )
                .unwrap();
            let (event_tx, event_rx) = flume::unbounded();
            let event_tx = crate::EventSender::new(event_tx, 0);
            let ctx =
                crate::tools::test_support::stub_ctx_with(&AgentMode::Build, Some(&event_tx), None);

            let done = run(
                &registry,
                None,
                "native-result".into(),
                "native_result",
                &serde_json::json!({}),
                &ctx,
                Emit::Notify,
            )
            .await;

            let event = event_rx.recv_async().await.unwrap().event;
            let AgentEvent::ToolStart(start) = event else {
                panic!("expected tool start, got {event:?}");
            };
            assert_eq!(
                start.input,
                Some(ToolInput::Code {
                    language: "rust".into(),
                    code: "fn main() {}".into(),
                })
            );
            assert_eq!(done.written_path(), Some("first.rs"));
            assert_eq!(
                done.written_paths().collect::<Vec<_>>(),
                ["first.rs", "second.rs"]
            );
            assert_eq!(done.model_output.as_deref(), Some("native model output"));
            assert!(executed.load(Ordering::SeqCst));
        });
    }

    #[test]
    fn filtered_tool_cannot_be_dispatched_by_name() {
        smol::block_on(async {
            let dir = TempDir::new().unwrap();
            let permissions = Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig {
                    default: caudra_config::DefaultEffect::Allow,
                    ..PermissionsConfig::default()
                },
                dir.path().to_path_buf(),
                Arc::default(),
            ));
            let mut ctx = crate::tools::test_support::stub_ctx_with_permissions(
                &AgentMode::Build,
                permissions,
            );
            ctx.tool_filter = crate::tools::ToolFilter::AllExcept(vec![START_PROBE_NAME.into()]);
            let probe = StartProbe::default();
            let (started, executed) = (Arc::clone(&probe.started), Arc::clone(&probe.executed));
            let registry = ToolRegistry::new();
            registry
                .register(
                    Arc::new(probe),
                    ToolSource::Lua {
                        plugin: "test".into(),
                        contract: "test-contract".into(),
                        bundled: true,
                    },
                )
                .unwrap();

            let done = run(
                &registry,
                None,
                "t1".into(),
                START_PROBE_NAME,
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;

            assert!(done.is_error);
            assert!(done.output.as_text().contains("disabled"));
            assert!(!started.load(Ordering::SeqCst));
            assert!(!executed.load(Ordering::SeqCst));
        });
    }

    /// A denied tool cannot run either lifecycle callback.
    #[test]
    fn permission_denial_blocks_start_and_execute() {
        smol::block_on(async {
            let deny_cfg = PermissionsConfig {
                rules: vec![PermissionRule {
                    tool: ToolKey::native(START_PROBE_NAME),
                    scope: None,
                    effect: Effect::Deny,
                }],
                ..Default::default()
            };
            let dir = TempDir::new().unwrap();
            let permissions = Arc::new(PermissionManager::new_nonpersistent(
                deny_cfg,
                dir.path().to_path_buf(),
                Arc::default(),
            ));
            let ctx = crate::tools::test_support::stub_ctx_with_permissions(
                &AgentMode::Build,
                permissions,
            );

            let probe = StartProbe::default();
            let (started, executed) = (Arc::clone(&probe.started), Arc::clone(&probe.executed));
            let registry = ToolRegistry::new();
            registry
                .register(
                    Arc::new(probe),
                    ToolSource::Lua {
                        plugin: "test".into(),
                        contract: "test-contract".into(),
                        bundled: false,
                    },
                )
                .unwrap();

            let done = run(
                &registry,
                None,
                "t1".into(),
                START_PROBE_NAME,
                &serde_json::json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;

            assert!(done.is_error, "denial must error");
            assert!(
                !started.load(Ordering::SeqCst),
                "start must not run after denial"
            );
            assert!(
                !executed.load(Ordering::SeqCst),
                "execute must not run after denial"
            );
        });
    }
}

#[cfg(test)]
mod telemetry_tests {
    use test_case::test_case;

    use super::*;

    const BEFORE: &str = "a\nb\nc\n";
    const AFTER: &str = "a\nB\nc\nd\n";

    #[test_case("operation was cancelled", ERROR_CANCELLED; "cancelled")]
    #[test_case("command timed out after 120s", ERROR_TIMEOUT; "timed_out")]
    #[test_case("permission denied: bash", ERROR_DENIED; "denied")]
    #[test_case("no such file or directory", ERROR_NOT_FOUND; "missing_file")]
    #[test_case("invalid input: expected a string", ERROR_INVALID_INPUT; "invalid")]
    #[test_case("boom", ERROR_OTHER; "fallback")]
    fn errors_bucket_into_low_cardinality_types(text: &str, expected: &str) {
        assert_eq!(classify_error(text), expected);
    }

    #[test]
    fn diffs_count_added_and_removed_lines() {
        assert_eq!(changed_lines(BEFORE, AFTER), (2, 1));
        assert_eq!(changed_lines(BEFORE, BEFORE), (0, 0));
    }
}
