use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::Value;
use tracing::{Instrument, debug, error, info_span, warn};

use crate::mcp::{McpSession, UNKNOWN_MCP};
use crate::permissions::{PermissionAuthorityProfile, RemotePermissionIdentity, canonical_json};
use crate::task_set::TaskSet;
use crate::tools::json_repair::RepairError;
use crate::tools::registry::{
    PlanModeAccess, RegisteredTool, ToolInvocation, ToolRegistry, TrustedToolSource,
};
use crate::tools::{
    DOOM_LOOP_GUIDANCE, LocalToolEntry, LockKey, READ_ONLY_CALL_GUIDANCE,
    READ_ONLY_TOOL_RESTRICTED, TOOL_SEARCH_TOOL_NAME, ToolContext, ToolEffect, ToolSource,
};
use crate::workspace_baseline::BaselineOutcome;
use crate::{
    AgentError, AgentEvent, LuaToolProvenance, ToolAccounting, ToolDoneEvent, ToolOutput,
    ToolStartEvent,
};
use caudra_config::ToolKey;
use caudra_providers::estimate_tokens_cached;
use caudra_storage::tool_ledger::ToolOutcome as LedgerOutcome;

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

#[cfg(test)]
const DOOM_LOOP_THRESHOLD: usize = 3;
const MCP_BLOCKED_IN_PLAN: &str = "MCP tools are not available in plan mode";
const UNKNOWN_TOOL_PREFIX: &str = "unknown tool";
const TOOL_DISABLED_SUFFIX: &str = "is disabled for the current agent";
const INVALID_INPUT_MESSAGE: &str = "arguments were not valid JSON, so the tool did not run. Call it again with complete \
     arguments; if the input is large, split it across several calls. Raw text received:";
const MAX_INVALID_INPUT_CHARS: usize = 2_000;
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

const SNAPSHOT_FAILED: &str = "could not snapshot the workspace before changing it";

const ERROR_CANCELLED: &str = "cancelled";
const ERROR_TIMEOUT: &str = "timeout";
const ERROR_DENIED: &str = "permission_denied";
const ERROR_NOT_FOUND: &str = "not_found";
const ERROR_INVALID_INPUT: &str = "invalid_input";
const ERROR_OTHER: &str = "error";

/// A telemetry counter is not worth an unbounded diff; past this,
/// `similar` returns a coarser but still valid one.
const DIFF_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolOutcome {
    Success,
    Repairable,
    Failure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolObservation {
    pub name: String,
    pub fingerprint: u64,
    pub outcome: ToolOutcome,
}

#[derive(Clone)]
pub struct ResponseObservations {
    shared: Arc<Mutex<ObservationState>>,
    attempt: Option<Arc<Mutex<ObservationAttempt>>>,
}

struct ObservationState {
    limit: usize,
    facts: BTreeMap<Vec<usize>, ToolObservation>,
    attempts: usize,
    repairable: usize,
}

struct ObservationAttempt {
    order: Vec<usize>,
    outcome: ToolOutcome,
    repairable: bool,
    expanded: bool,
}

impl ResponseObservations {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            shared: Arc::new(Mutex::new(ObservationState {
                limit,
                facts: BTreeMap::new(),
                attempts: 0,
                repairable: 0,
            })),
            attempt: None,
        }
    }

    fn lock(&self) -> MutexGuard<'_, ObservationState> {
        self.shared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Retain only the earliest leaf paths, not the first tasks to finish. A
    /// reservation starts as Failure so a panic or cancellation cannot look
    /// repairable merely because validation marked it before unwinding.
    fn reserve(&self, order: Vec<usize>, name: &str, input: &Value) -> Self {
        let mut state = self.lock();
        state.attempts += 1;
        if state.limit > 0
            && (state.facts.len() < state.limit
                || state
                    .facts
                    .last_key_value()
                    .is_some_and(|(last, _)| &order < last))
        {
            let mut hasher = DefaultHasher::new();
            name.hash(&mut hasher);
            canonical_json(input).hash(&mut hasher);
            state.facts.insert(
                order.clone(),
                ToolObservation {
                    name: name.to_owned(),
                    fingerprint: hasher.finish(),
                    outcome: ToolOutcome::Failure,
                },
            );
            if state.facts.len() > state.limit {
                state.facts.pop_last();
            }
        }
        Self {
            shared: Arc::clone(&self.shared),
            attempt: Some(Arc::new(Mutex::new(ObservationAttempt {
                order,
                outcome: ToolOutcome::Failure,
                repairable: false,
                expanded: false,
            }))),
        }
    }

    pub(crate) fn mark_repairable(&self) {
        if let Some(attempt) = &self.attempt {
            attempt
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .repairable = true;
        }
    }

    pub(super) fn expanded_context(&self, order: Vec<usize>) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            attempt: Some(Arc::new(Mutex::new(ObservationAttempt {
                order,
                outcome: ToolOutcome::Failure,
                repairable: false,
                expanded: true,
            }))),
        }
    }

    /// Response-wide totals include attempts evicted from the bounded pattern
    /// window; one ordinary failure or success anywhere prevents all-repairable.
    pub(crate) fn finish(&self, is_error: bool) {
        let Some(attempt) = &self.attempt else { return };
        let mut attempt = attempt.lock().unwrap_or_else(|error| error.into_inner());
        if attempt.expanded {
            return;
        }
        let outcome = if !is_error {
            ToolOutcome::Success
        } else if attempt.repairable {
            ToolOutcome::Repairable
        } else {
            ToolOutcome::Failure
        };
        let mut state = self.lock();
        state.repairable -= usize::from(attempt.outcome == ToolOutcome::Repairable);
        state.repairable += usize::from(outcome == ToolOutcome::Repairable);
        if let Some(fact) = state.facts.get_mut(&attempt.order) {
            fact.outcome = outcome.clone();
        }
        attempt.outcome = outcome;
    }

    /// A parsed batch contributes its children, not a successful wrapper.
    /// Removing the parent before reserving its ordered descendants preserves
    /// the deterministic prefix even when later roots expand first.
    pub(crate) fn expand(&self) {
        let Some(attempt) = &self.attempt else { return };
        let mut attempt = attempt.lock().unwrap_or_else(|error| error.into_inner());
        if !attempt.expanded {
            let mut state = self.lock();
            state.attempts -= 1;
            state.repairable -= usize::from(attempt.outcome == ToolOutcome::Repairable);
            state.facts.remove(&attempt.order);
            attempt.expanded = true;
        }
    }

    pub(crate) fn take(&self) -> (Vec<ToolObservation>, bool) {
        let mut state = self.lock();
        let all_repairable = state.attempts > 0 && state.attempts == state.repairable;
        let facts = std::mem::take(&mut state.facts).into_values().collect();
        state.attempts = 0;
        state.repairable = 0;
        (facts, all_repairable)
    }
}

pub(crate) fn observe_context(ctx: &mut ToolContext, name: &str, input: &Value) {
    if let Some(observations) = &ctx.steering_observations {
        let name = canonical_tool_name(name, ctx);
        let name = if ctx.registry.get(name).is_none()
            && !ctx.local_tools.contains_key(name)
            && ctx.mcp.is_some()
            && name.contains(MCP_NAME_SEPARATOR)
        {
            Cow::Owned(crate::mcp::internal_tool_name(name))
        } else {
            Cow::Borrowed(name)
        };
        ctx.steering_observations =
            Some(observations.reserve(ctx.steering_order.clone(), &name, input));
    }
}

#[derive(Clone)]
pub(super) struct RecentCalls {
    calls: VecDeque<(String, String)>,
    threshold: usize,
}

impl RecentCalls {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::with_threshold(DOOM_LOOP_THRESHOLD)
    }

    pub(super) fn with_threshold(threshold: usize) -> Self {
        Self {
            calls: VecDeque::new(),
            threshold,
        }
    }

    pub(super) fn threshold(&self) -> usize {
        self.threshold
    }

    pub(super) fn is_doom_loop(&self, name: &str, input: &Value) -> bool {
        if self.threshold == 0 {
            return false;
        }
        let canonical = canonical_json(input);
        self.calls.len() >= self.threshold - 1
            && self
                .calls
                .iter()
                .rev()
                .take(self.threshold - 1)
                .all(|(n, value)| n == name && *value == canonical)
    }

    pub(super) fn may_repeat_name(&self, name: &str) -> bool {
        self.threshold > 0
            && self.calls.len() >= self.threshold - 1
            && self
                .calls
                .iter()
                .rev()
                .take(self.threshold - 1)
                .all(|(previous, _)| previous == name)
    }

    pub(super) fn record(&mut self, name: String, input: &Value) {
        if self.threshold == 0 {
            return;
        }
        self.calls.push_back((name, canonical_json(input)));
        if self.calls.len() > self.threshold {
            self.calls.pop_front();
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
    let mut observed_ctx;
    let ctx = if ctx
        .steering_observations
        .as_ref()
        .is_some_and(|observations| observations.attempt.is_none())
    {
        observed_ctx = ctx.clone();
        observe_context(&mut observed_ctx, name, input);
        &observed_ctx
    } else {
        ctx
    };
    // Resolved unconditionally now that the report also feeds the log file,
    // which is where "which tool took nine seconds" gets answered.
    let canonical = canonical_tool_name(name, ctx);
    let source = tool_source(registry, ctx, canonical);
    let span = info_span!("tool", tool = canonical, tool_use_id = %id);
    let started = Instant::now();
    let entry = registry.get(canonical);
    let local = ctx.local_tools.get(canonical);
    let mcp_name = crate::mcp::internal_tool_name(canonical);
    let eligible = ctx.config.tool_json_repair
        && ctx.tool_filter.matches(canonical)
        && (entry
            .as_ref()
            .is_some_and(|entry| entry.tool.audience().contains(ctx.audience))
            || local.is_some()
            || (canonical == TOOL_SEARCH_TOOL_NAME && searchable(mcp, ctx))
            || mcp.is_some_and(|mcp| mcp.has_tool(&mcp_name) && !mcp.is_disabled(&mcp_name)))
        && (!ctx.policy().is_read_only()
            || entry.is_some()
            || local.is_some_and(|local| local.effect.is_safe_in_read_only()));
    let invalid = ctx.json_repair.invalid_input(&id);
    let repair = if eligible && invalid.is_some() {
        let schema = repair_schema(registry, mcp, canonical, ctx);
        Some(match schema {
            Some(schema) => ctx.json_repair.repair(&id, canonical, &schema, ctx).await,
            None => Err(RepairError::Schema),
        })
    } else {
        None
    };
    let refusal = if canonical == crate::tools::BATCH_TOOL_NAME
        && local.is_none()
        && invalid.is_some()
        && let Some(runs) = &ctx.speculative
        && !runs.has_admitted()
        && !repair.as_ref().is_some_and(|repair| {
            repair.as_ref().is_ok_and(|repair| {
                entry
                    .as_ref()
                    .is_some_and(|entry| entry.tool.parse(&repair.effective).is_ok())
            })
        }) {
        runs.admit(ctx, name, input)
    } else {
        None
    };
    let mut done = match refusal {
        Some(message) => ToolDoneEvent::error(id, message),
        None if invalid.is_some() && !matches!(repair, Some(Ok(_))) => {
            ctx.mark_tool_result_repairable();
            let mut message = format!("{canonical} {INVALID_INPUT_MESSAGE}");
            if let Some(invalid) = &invalid {
                message.push(' ');
                message.extend(invalid.raw.chars().take(MAX_INVALID_INPUT_CHARS));
            }
            if let Some(Err(error)) = &repair {
                message.push_str(&format!("\n{error}"));
            }
            let mut done = ToolDoneEvent::error(id, message);
            done.tool = Arc::from(canonical);
            done
        }
        _ => {
            let effective = repair
                .as_ref()
                .and_then(|repair| repair.as_ref().ok())
                .map_or(input, |repair| &repair.effective);
            run_inner(registry, mcp, id, name, effective, ctx, &mut emit)
                .instrument(span)
                .await
        }
    };
    if let Some(Ok(repair)) = repair
        && repair.method != "unchanged"
    {
        let provenance = repair.provenance();
        done.model_suffix = Some(match done.model_suffix.take() {
            Some(suffix) => format!("{suffix}\n\n{provenance}"),
            None => provenance,
        });
        done.annotation = Some(match done.annotation.take() {
            Some(annotation) => format!("{annotation}; JSON repaired"),
            None => "JSON repaired".into(),
        });
    }
    crate::tool_output::limit(&mut done, ctx).await;
    if !ctx.cancel.is_cancelled()
        && let Some(observations) = &ctx.steering_observations
    {
        observations.finish(done.is_error);
    }
    let logged_input = if invalid.is_some() {
        &Value::Null
    } else {
        input
    };
    let took = started.elapsed();
    account(&mut done, &source, took);
    report(&done, canonical, &source, logged_input, took);
    done
}

pub(crate) fn repair_schema(
    registry: &ToolRegistry,
    mcp: Option<&McpSession>,
    name: &str,
    ctx: &ToolContext,
) -> Option<Value> {
    if ctx.local_tools.contains_key(name) {
        return ctx.json_repair.schema(name);
    }
    if let Some(entry) = registry.get(name) {
        return Some(entry.tool.schema());
    }
    if let Some(schema) = ctx.json_repair.schema(name) {
        return Some(schema);
    }
    let mut definitions = Value::Array(Vec::new());
    if name == TOOL_SEARCH_TOOL_NAME {
        if let Some(mcp) = mcp {
            mcp.request_snapshot().extend_tools(&mut definitions);
        }
        if let Some(deferral) = &ctx.deferral {
            deferral.request_snapshot().extend_tools(&mut definitions);
        }
    } else if let Some(mcp) = mcp {
        let lookup = mcp.fresh();
        lookup.mark_loaded(&crate::mcp::internal_tool_name(name));
        lookup.request_snapshot().extend_tools(&mut definitions);
    }
    let wire_name = crate::mcp::wire_tool_name(name);
    definitions.as_array()?.iter().find_map(|definition| {
        (definition["name"] == name || definition["name"] == wire_name)
            .then(|| definition.get("input_schema").cloned())
            .flatten()
    })
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
            remote_written_paths: false,
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
            accounting: ToolAccounting::default(),
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
                !(name == TOOL_SEARCH_TOOL_NAME && searchable(mcp, ctx))
                    && !mcp.is_some_and(|mcp| mcp.has_tool(mcp_lookup))
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
        // Guessing a deferred tool's name right is as good as searching for
        // it: keep it declared so the model is not told to look for what it
        // just called.
        if let Some(deferral) = &ctx.deferral {
            announce_loads(ctx, deferral.mark_loaded(name));
        }

        let invocation = match entry.tool.parse(input) {
            Ok(inv) => inv,
            Err(e) => {
                warn!(
                    tool = %name,
                    source = %entry.source.as_log_field(),
                    error = %e,
                    "tool input parse failed"
                );
                ctx.mark_tool_result_repairable();
                return done_error(e.to_string());
            }
        };

        // A remote write is prepared against the file as the host sees it now,
        // and the host refuses to publish it once the file has changed. Taken
        // after the verdict like the guards below, two writes to one file
        // would prepare against the same version and the second would always
        // be refused, so these are taken before preparation instead.
        let _preparation_guards = ctx
            .path_locks
            .acquire(&invocation.preflight_write_keys(ctx), &[])
            .await;
        let mut prepared_intent = match invocation.preflight(ctx).await {
            Ok(intent) => intent,
            Err(error) => return done_error(error),
        };

        // Judged after preflight, beside the plan gate below: an invocation
        // that can only narrow its effect once its input is parsed gets to
        // answer, which is what lets a read-only agent run a confined read.
        let call_effect = entry.effect_for(invocation.as_ref());
        if ctx.policy().is_read_only() && !entry.is_safe_in_read_only_with(call_effect) {
            warn!(tool = %name, effect = call_effect.as_str(), "blocked tool in strict read-only mode");
            invocation.abandon(ctx).await;
            return done_error(format!(
                "{READ_ONLY_TOOL_RESTRICTED}: {name}. This call is {}, and {READ_ONLY_CALL_GUIDANCE}.",
                call_effect.as_str()
            ));
        }

        let planning = ctx.mode.is_planning();
        let plan_access = invocation.plan_mode_access();
        if planning && plan_access == PlanModeAccess::Refused {
            warn!(tool = %name, "blocked tool in plan mode");
            invocation.abandon(ctx).await;
            return done_error(crate::tools::PLAN_WRITE_RESTRICTED.into());
        }
        // A plan-mode grant must not outlive the plan, and no authority from
        // before the plan may quietly cover this call. Both are containment,
        // not a veto: an authority granted while planning still applies.
        if planning
            && plan_access == PlanModeAccess::Prompted
            && let Some(intent) = prepared_intent.as_mut()
        {
            intent.scopes.plan_scoped = true;
        }

        let mutation_targets = invocation.mutation_targets(ctx);
        let remote_plan_target = ctx.mode.plan_ref().is_some_and(|expected| {
            invocation.local_document_target()
                == Some(&caudra_workspace::LocalDocumentRef::Plan(expected.clone()))
        });
        if ctx.mode.plan_ref().is_some()
            && !call_effect.is_safe_in_read_only()
            && !remote_plan_target
        {
            warn!(tool = %name, "blocked non-plan local document write in remote plan mode");
            invocation.abandon(ctx).await;
            return done_error(crate::tools::PLAN_WRITE_RESTRICTED.into());
        }
        if planning && !call_effect.is_safe_in_read_only() && !entry.source.is_trusted() {
            warn!(tool = %name, "blocked untrusted effect in plan mode");
            invocation.abandon(ctx).await;
            return done_error(crate::tools::PLAN_WRITE_RESTRICTED.into());
        }
        // A call that named no target cannot be checked against the plan file,
        // unless it already accounted for itself above.
        if planning
            && plan_access == PlanModeAccess::Standard
            && !call_effect.is_safe_in_read_only()
            && mutation_targets.is_empty()
            && !remote_plan_target
        {
            warn!(tool = %name, "blocked unscoped effect in plan mode");
            invocation.abandon(ctx).await;
            return done_error(crate::tools::PLAN_WRITE_RESTRICTED.into());
        }

        for target in &mutation_targets {
            let is_plan_target = ctx
                .mode
                .plan_path()
                .is_some_and(|plan_path| target == plan_path);
            if !is_plan_target {
                if planning {
                    warn!(
                        tool = %name,
                        target = %target.display(),
                        "blocked write in plan mode"
                    );
                    invocation.abandon(ctx).await;
                    return done_error(crate::tools::PLAN_WRITE_RESTRICTED.into());
                }
                if let Some(reason) = ctx.permissions.boundary_block_reason(target) {
                    invocation.abandon(ctx).await;
                    return done_error(reason);
                }
            }
        }

        if !remote_plan_target
            && let Err(e) = enforce_permission(
                invocation.as_ref(),
                prepared_intent.as_ref(),
                entry,
                name,
                input,
                ctx,
                &id,
            )
            .await
        {
            invocation.abandon(ctx).await;
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

        if !remote_plan_target && let Err(message) = ensure_revert_point(ctx, call_effect).await {
            invocation.abandon(ctx).await;
            return done_error(message);
        }
        if let Err(message) = ctx.deadline.remaining() {
            invocation.abandon(ctx).await;
            return done_error(message);
        }

        // Taken after the permission verdict, so a prompt never blocks a
        // sibling's write, and after the start event, so a call waiting on a
        // contended file still renders as a running row. Held across execute:
        // a tool's own stale check, write, and mtime record must not interleave
        // with a concurrent call naming the same file. Not gated on
        // `stale_read_check`; turning that off must not re-enable clobbering.
        // Remote writes are the exception above: their prompt holds back later
        // writes to the same file, and they wait before their start event.
        let _guards = ctx
            .path_locks
            .acquire(
                &local_keys(&mutation_targets),
                &local_keys(&invocation.read_targets(ctx)),
            )
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
                    remote_written_paths: result.remote_written_paths,
                    output_ref: result.output_ref,
                    output_limits: result.output_limits,
                    model_suffix: result.model_suffix,
                    model_output: result.model_output,
                    model_output_from_ref: result.model_output_from_ref,
                    accounting: ToolAccounting::default(),
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
                done.annotation = result.annotation;
                done.remote_written_paths = result.remote_written_paths;
                done.output_limits = result.output_limits;
                done.output_ref = result.output_ref;
                done.model_output = result.model_output;
                done.model_output_from_ref = result.model_output_from_ref;
                done
            }
        }
    } else if name == TOOL_SEARCH_TOOL_NAME && searchable(mcp, ctx) {
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
        if let Err(message) = ensure_revert_point(ctx, ToolEffect::Unknown).await {
            return done_error(message);
        }
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

fn local_keys(paths: &[PathBuf]) -> Vec<LockKey> {
    paths.iter().cloned().map(LockKey::Local).collect()
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

/// Says what a load just cost. Silent when nothing changed, so a repeat
/// search or a second call to an already-loaded tool draws no card.
fn announce_loads(ctx: &ToolContext, loaded: Vec<Arc<str>>) {
    if loaded.is_empty() {
        return;
    }
    let names = loaded.iter().map(|name| name.to_string()).collect();
    let _ = ctx.event_tx.send(AgentEvent::ToolsLoaded { names });
}

/// Whether anything is deferred at all. The search tool is declared only when
/// it has something to find, so being asked for it otherwise is an unknown
/// tool rather than an empty answer.
fn searchable(mcp: Option<&McpSession>, ctx: &ToolContext) -> bool {
    mcp.is_some() || ctx.deferral.as_ref().is_some_and(|d| !d.is_empty())
}

/// One search over both catalogs: the model is offered one tool, so it must
/// not have to know whether what it wants is a built-in or an MCP tool.
/// Built-ins rank first because their catalog is the smaller, curated one.
///
/// Runs without a permission gate: search only reveals names the catalog
/// already showed the model.
fn run_tool_search(
    mcp: Option<&McpSession>,
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
    // Search bypasses invocation parsing. Only invalid input is repairable;
    // a valid search with no catalog match is still a successful attempt.
    if !query.chars().any(char::is_alphanumeric) {
        ctx.mark_tool_result_repairable();
    }
    let builtin = ctx
        .deferral
        .as_ref()
        .filter(|deferral| !deferral.is_empty())
        .map(|deferral| deferral.search(query));
    if let Some(Ok(outcome)) = &builtin {
        announce_loads(ctx, outcome.loaded.clone());
    }
    let (output, is_error) = match (builtin, mcp) {
        (Some(Ok(outcome)), _) if !outcome.loaded.is_empty() => (outcome.message, false),
        (_, Some(mcp)) => match mcp.search_tools(query) {
            Ok(outcome) => {
                announce_loads(ctx, outcome.loaded);
                (outcome.message, false)
            }
            Err(e) => (e, true),
        },
        (Some(Ok(outcome)), None) => (outcome.message, false),
        (Some(Err(e)), None) => (e, true),
        (None, None) => (crate::tools::deferral::SEARCH_EMPTY_QUERY.into(), true),
    };
    ToolDoneEvent {
        id,
        tool: tool_id,
        output: ToolOutput::Markdown(output.into()),
        is_error,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
    let (output, is_error) = match ensure_revert_point(ctx, local.effect).await {
        Err(message) => (message, true),
        Ok(()) => match local.call(input.clone(), tool_ctx).await {
            Ok(output) => (output, false),
            Err(e) => {
                warn!(tool = %name, error = %e, "local tool failed");
                (e, true)
            }
        },
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
    }
}

/// The workspace as it stands before the first call of this session that could
/// change it, which is what a file revert restores.
///
/// Asked after the permission verdict, so a denied call captures nothing, and
/// before execution, so no file changes ahead of the record of its old contents.
/// A call that cannot change a file skips it, which is the whole point: a
/// conversational turn leaves no store on disk.
async fn ensure_revert_point(ctx: &ToolContext, effect: ToolEffect) -> Result<(), String> {
    if effect.is_safe_in_read_only() {
        return Ok(());
    }
    let Some(gate) = ctx.baseline.as_ref() else {
        if ctx.workspace_session.is_some() {
            return Err(format!(
                "{SNAPSHOT_FAILED}: remote snapshot baseline is unavailable"
            ));
        }
        return Ok(());
    };
    match gate.ensure().await {
        // A workspace Caudra will not snapshot costs file revert, not the
        // user's work, and it has already said so once.
        BaselineOutcome::Ready => Ok(()),
        BaselineOutcome::Unavailable(_) if !gate.is_remote() => Ok(()),
        BaselineOutcome::Unavailable(reason) => Err(format!("{SNAPSHOT_FAILED}: {reason}")),
        BaselineOutcome::Failed(error) => Err(format!("{SNAPSHOT_FAILED}: {error}")),
    }
}

/// Enforce permission for a registry tool. MCP tools bypass this — they go
/// through `execute_mcp_tool` which handles permission checking internally.
///
/// Returns an error if `name` contains dots (not a valid native tool name).
async fn enforce_permission(
    inv: &dyn ToolInvocation,
    prepared_intent: Option<&crate::tools::PermissionIntent>,
    entry: &RegisteredTool,
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
    let remote = if matches!(entry.source, ToolSource::Native { .. })
        && prepared_intent
            .is_some_and(|intent| intent.authority == PermissionAuthorityProfile::RemoteResource)
    {
        let workspace = ctx
            .workspace_session
            .as_ref()
            .ok_or_else(|| "remote permission intent has no workspace identity".to_owned())?;
        Some(RemotePermissionIdentity::from_binding(workspace.binding()))
    } else {
        None
    };
    let source = TrustedToolSource::from_registered(entry, remote.as_ref());
    let identity = source
        .as_ref()
        .map(|source| (source.subject().clone(), source.executor().clone()));
    let include_builtin_allows = source
        .as_ref()
        .is_some_and(TrustedToolSource::builtin_allows);
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
    };

    if ctx.policy().is_read_only() {
        return done(format!("{READ_ONLY_TOOL_RESTRICTED}: {tool_name}"), true);
    }

    if ctx.mode.is_planning() {
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
    if mcp.mark_loaded(tool_name) {
        announce_loads(ctx, vec![Arc::from(tool_name)]);
    }
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
    if let Some(runs) = &ctx.speculative {
        runs.set_aliases(ctx.tool_name_aliases.clone());
        for (id, name, input) in &tool_uses {
            runs.ready(id, name, input.clone());
        }
        let mut results = Vec::with_capacity(tool_uses.len());
        for (id, name, input) in tool_uses {
            let Some(adopted) = runs.claim(&id, &name, &input) else {
                return Err(AgentError::Tool {
                    tool: name,
                    message: "eager tool slot was not available for adoption".into(),
                });
            };
            results.push(adopted.finish().await);
        }
        *recent_calls = runs.recent();
        let tool_msg = crate::types::tool_results(results);
        history.push(tool_msg.clone());
        event_tx.send(AgentEvent::ToolResultsSubmitted {
            message: Box::new(tool_msg),
        })?;
        return Ok(());
    }
    let mut immediate_errors: Vec<ToolDoneEvent> = Vec::new();
    let mut runnable = Vec::new();
    let mut repeat_message = None;

    for (index, (id, name, input)) in tool_uses.into_iter().enumerate() {
        let mut tool_ctx = ToolContext {
            tool_use_id: Some(id.clone()),
            root_tool_use_id: ctx.root_tool_use_id.clone().or_else(|| Some(id.clone())),
            steering_order: vec![index],
            ..ctx.clone()
        };
        observe_context(&mut tool_ctx, &name, &input);
        debug!(
            tool = %name,
            id = %id,
            "parsing tool call"
        );
        if recent_calls.is_doom_loop(&name, &input) {
            warn!(tool = %name, "doom loop detected, skipping execution");
            tool_ctx.mark_tool_result_repairable();
            if !ctx.cancel.is_cancelled()
                && let Some(observations) = &tool_ctx.steering_observations
            {
                observations.finish(true);
            }
            // Resolve model overrides only if a refusal needs guidance, and
            // reuse the message for every blocked call in this response.
            let message = repeat_message.get_or_insert_with(|| {
                let policy = ctx.config.steering.resolve(&ctx.model.spec());
                let guidance = policy
                    .rules
                    .repeated_tool_call
                    .prompt
                    .as_deref()
                    .unwrap_or(DOOM_LOOP_GUIDANCE);
                format!(
                    "You have called this tool with identical input {} times in a row. {guidance}",
                    recent_calls.threshold(),
                )
            });
            immediate_errors.push(ToolDoneEvent::error(id.clone(), message.clone()));
        } else {
            runnable.push((id, name.clone(), input.clone(), tool_ctx));
        }
        recent_calls.record(name, &input);
    }

    for err in &immediate_errors {
        event_tx.try_send(AgentEvent::ToolDone(Box::new(err.clone())));
    }

    let mut set = TaskSet::new();
    let mut spawned_ids: Vec<String> = Vec::new();
    for (id, name, input, tool_ctx) in runnable {
        spawned_ids.push(id.clone());
        let event_tx_clone = ctx.event_tx.clone();
        let mcp_owned = mcp.cloned();
        set.spawn(async move {
            let done = super::speculative::with_live(&tool_ctx, |live_ctx| async move {
                run(
                    &live_ctx.registry,
                    mcp_owned.as_ref(),
                    id,
                    &name,
                    &input,
                    &live_ctx,
                    Emit::Notify,
                )
                .await
            })
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
/// collector a new attribute value on every call, and because the ledger keeps
/// one row per class.
fn classify_error(text: &str) -> LedgerOutcome {
    let text = text.to_ascii_lowercase();
    if text.contains("cancel") {
        LedgerOutcome::Cancelled
    } else if text.contains("timed out") || text.contains("timeout") {
        LedgerOutcome::Timeout
    } else if text.contains("permission denied") || text.contains("not allowed") {
        LedgerOutcome::Denied
    } else if text.contains("no such file") || text.contains("not found") {
        LedgerOutcome::NotFound
    } else if text.contains("invalid") || text.contains("expected") {
        LedgerOutcome::InvalidInput
    } else {
        LedgerOutcome::Other
    }
}

/// The attribute value collectors already index on. Kept apart from
/// [`LedgerOutcome::storage_name`] so renaming a storage value can never
/// silently rewrite a dashboard's history.
fn error_type(outcome: LedgerOutcome) -> Option<&'static str> {
    match outcome {
        LedgerOutcome::Ok => None,
        LedgerOutcome::Cancelled => Some(ERROR_CANCELLED),
        LedgerOutcome::Timeout => Some(ERROR_TIMEOUT),
        LedgerOutcome::Denied => Some(ERROR_DENIED),
        LedgerOutcome::NotFound => Some(ERROR_NOT_FOUND),
        LedgerOutcome::InvalidInput => Some(ERROR_INVALID_INPUT),
        LedgerOutcome::Other => Some(ERROR_OTHER),
    }
}

/// What an editing tool moved, in the shape the metric counts, or `None` for a
/// call that edited nothing.
///
/// A patch already carries its own counts, computed by Workcell over the same
/// content, so they are summed rather than recomputed from text this side never
/// holds whole.
fn edited_lines(output: &ToolOutput) -> Option<(u64, u64)> {
    match output {
        ToolOutput::Diff { before, after, .. } => Some(changed_lines(before, after)),
        ToolOutput::Patch { files } => Some((
            files.iter().map(|file| file.additions as u64).sum(),
            files.iter().map(|file| file.deletions as u64).sum(),
        )),
        _ => None,
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

/// Fills in what only the dispatch site knows, once, so the telemetry event and
/// the durable ledger describe the same call with the same numbers.
///
/// The token estimate runs on `composed_model_output`, which is the exact text
/// the model will read and which [`crate::tool_output::limit`] has already
/// bounded, so this counts what the context window is actually charged.
fn account(done: &mut ToolDoneEvent, source: &str, took: Duration) {
    let outcome = match done.is_error {
        true => classify_error(&done.output.as_text()),
        false => LedgerOutcome::Ok,
    };
    done.accounting = ToolAccounting {
        duration_ms: took.as_millis() as u64,
        source: Some(Arc::from(source)),
        outcome: Some(outcome),
        model_tokens: estimate_tokens_cached(&done.composed_model_output()),
    };
}

fn report(done: &ToolDoneEvent, name: &str, source: &str, input: &Value, took: Duration) {
    let tool_input = caudra_otel::logs_tool_details().then(|| input.to_string());
    caudra_otel::emit::tool_result(&caudra_otel::emit::ToolResult {
        tool_name: name,
        tool_source: source,
        success: !done.is_error,
        duration: took,
        error_type: done.accounting.outcome.and_then(error_type),
        tool_input: tool_input.as_deref(),
    });
    if !done.is_error {
        if let Some((added, removed)) = edited_lines(&done.output) {
            caudra_otel::emit::lines_of_code(added, removed);
        }
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use caudra_config::{DefaultEffect, Effect, PermissionRule, PermissionsConfig, ToolKey};
    use caudra_providers::{INVALID_TOOL_JSON_KEY, InvalidToolInput};
    use caudra_storage::StateDir;
    use caudra_storage::id::SessionRef;
    use caudra_storage::tool_outputs::ToolOutputStore;
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::agent::history::History;
    use crate::agent::speculative::SpeculativeRuns;
    use crate::cancel::CancelToken;
    use crate::permissions::{PERMISSION_DENIED_PREFIX, PermissionManager};
    use crate::snapshots::{SnapshotLimits, SnapshotStore};
    use crate::tools::BATCH_TOOL_NAME;
    use crate::tools::native::batch::BatchTool;
    use crate::tools::registry::{PermissionIntent, ToolSource};
    use crate::tools::test_support::{GUARDED_TOOL_NAME, GuardedMock};
    use crate::{AgentMode, EventSender};

    const OBSERVED_TOOL: &str = "observed";
    const OBSERVED_ERROR: &str = "invalid JSON permission denied";
    const OBSERVATION_WINDOW: usize = 3;
    const REPEAT_GUIDANCE: &str = "Inspect the previous result before choosing another tool.";
    const MODEL_REPEAT_GUIDANCE: &str = "Use a different query for this model.";
    const REPEAT_TWO_PREFIX: &str =
        "You have called this tool with identical input 2 times in a row. ";
    const OBSERVED_BATCH_ID: &str = "observed-batch";
    const OBSERVED_CHILDREN: usize = 2;

    #[test_case(false; "valid_children")]
    #[test_case(true; "repaired_fallback_children")]
    fn eager_dispatch_retains_final_child_history_and_single_reservations(malformed: bool) {
        smol::block_on(async {
            let count = Arc::new(AtomicUsize::new(0));
            let executions = Arc::clone(&count);
            let mut ctx = local_ctx(OBSERVED_TOOL, move |_| {
                executions.fetch_add(1, Ordering::SeqCst);
                Ok(String::new())
            });
            Arc::make_mut(&mut ctx.local_tools)
                .get_mut(OBSERVED_TOOL)
                .unwrap()
                .effect = ToolEffect::Mutating;
            ctx.registry
                .register(
                    Arc::new(BatchTool),
                    ToolSource::Native {
                        owner: crate::tools::native::OWNER.into(),
                        contract: BATCH_TOOL_NAME.into(),
                        trusted: true,
                    },
                )
                .unwrap();
            let (tx, _rx) = flume::unbounded();
            ctx.event_tx = EventSender::new(tx, 0);
            ctx.config.tool_json_repair = true;
            let observations = ResponseObservations::new(OBSERVED_CHILDREN);
            ctx.steering_observations = Some(observations.clone());
            let mut recent = RecentCalls::with_threshold(OBSERVED_CHILDREN);
            ctx.speculative = Some(Arc::new(
                SpeculativeRuns::new(&ctx, None).with_recent(recent.clone()),
            ));
            let params = json!({});
            let input = json!({"tool_calls": vec![json!({"tool": OBSERVED_TOOL, "parameters": params}); OBSERVED_CHILDREN]});
            if malformed {
                ctx.json_repair.register_invalid(
                    OBSERVED_BATCH_ID,
                    InvalidToolInput {
                        raw: input
                            .to_string()
                            .replacen("\"tool_calls\"", "tool_calls", 1),
                        complete: true,
                        clipped: false,
                    },
                );
            }
            let mut history = History::new(Vec::new());
            process_tool_calls(
                vec![(
                    OBSERVED_BATCH_ID.into(),
                    BATCH_TOOL_NAME.into(),
                    if malformed { json!({}) } else { input },
                )],
                &mut recent,
                None,
                &mut history,
                &ctx.event_tx,
                &ctx,
            )
            .await
            .unwrap();
            assert_eq!(count.load(Ordering::SeqCst), 1);
            assert!(recent.is_doom_loop(OBSERVED_TOOL, &params));
            assert_eq!(observations.lock().attempts, OBSERVED_CHILDREN);
            let (facts, all_repairable) = observations.take();
            assert_eq!(facts.len(), OBSERVED_CHILDREN);
            assert_eq!(facts[0].outcome, ToolOutcome::Success);
            assert_eq!(facts[1].outcome, ToolOutcome::Repairable);
            assert!(!all_repairable);
        });
    }

    #[test_case(&[0, 1, 2, 3]; "input_order")]
    #[test_case(&[3, 1, 0, 2]; "shuffled_completion")]
    fn observations_follow_reserved_order_and_count_truncated_success(order: &[usize]) {
        let observations = ResponseObservations::new(OBSERVATION_WINDOW);
        let slots: Vec<_> = (0..=OBSERVATION_WINDOW)
            .map(|index| observations.reserve(vec![index], &index.to_string(), &Value::Null))
            .collect();
        for &index in order {
            if index < OBSERVATION_WINDOW {
                slots[index].mark_repairable();
            }
            slots[index].finish(index < OBSERVATION_WINDOW);
        }
        let (facts, all_repairable) = observations.take();
        assert_eq!(
            facts
                .iter()
                .map(|fact| fact.name.as_str())
                .collect::<Vec<_>>(),
            ["0", "1", "2"]
        );
        assert!(
            facts
                .iter()
                .all(|fact| fact.outcome == ToolOutcome::Repairable)
        );
        assert!(!all_repairable);
    }

    #[test_case(&[0, 1, 2]; "input_order")]
    #[test_case(&[2, 0, 1]; "shuffled_expansion")]
    fn observations_bound_reservations_by_leaf_order(order: &[usize]) {
        let observations = ResponseObservations::new(OBSERVATION_WINDOW);
        let parents: Vec<_> = (0..OBSERVATION_WINDOW)
            .map(|index| observations.reserve(vec![index], "batch", &Value::Null))
            .collect();
        for &index in order {
            parents[index].expand();
            for child in 0..OBSERVATION_WINDOW {
                let slot = observations.reserve(
                    vec![index, child],
                    &format!("{index}:{child}"),
                    &Value::Null,
                );
                slot.mark_repairable();
                slot.finish(true);
            }
            parents[index].finish(false);
        }
        let (facts, all_repairable) = observations.take();
        assert_eq!(
            facts
                .iter()
                .map(|fact| fact.name.as_str())
                .collect::<Vec<_>>(),
            ["0:0", "0:1", "0:2"]
        );
        assert!(all_repairable);
    }

    #[test_case(0; "no_pattern_slots")]
    #[test_case(1; "one_pattern_slot")]
    fn observations_account_for_unsettled_attempts_outside_the_window(limit: usize) {
        let observations = ResponseObservations::new(limit);
        let first = observations.reserve(vec![0], OBSERVED_TOOL, &Value::Null);
        first.mark_repairable();
        first.finish(true);
        let unsettled = observations.reserve(vec![1], OBSERVED_TOOL, &Value::Null);
        unsettled.mark_repairable();
        assert!(!observations.take().1);
        assert_eq!(ResponseObservations::new(limit).take(), (Vec::new(), false));
    }

    #[test_case(&[0, 1], false; "earlier_root_first")]
    #[test_case(&[1, 0], false; "later_root_first")]
    #[test_case(&[1, 0], true; "completion_after_eviction")]
    fn observations_bound_uneven_batches(order: &[usize], finish_after_expansion: bool) {
        const BATCH_SIZES: [usize; 2] = [2, 3];
        let observations = ResponseObservations::new(OBSERVATION_WINDOW);
        let parents: Vec<_> = (0..BATCH_SIZES.len())
            .map(|index| observations.reserve(vec![index], "batch", &Value::Null))
            .collect();
        let mut children = Vec::new();
        for &index in order {
            parents[index].expand();
            for child in 0..BATCH_SIZES[index] {
                let slot = observations.reserve(
                    vec![index, child],
                    &format!("{index}:{child}"),
                    &Value::Null,
                );
                let repairable = (index, child) != (1, 2);
                if repairable {
                    slot.mark_repairable();
                }
                if !finish_after_expansion {
                    slot.finish(repairable);
                }
                children.push((slot, repairable));
            }
            parents[index].finish(false);
        }
        if finish_after_expansion {
            for (slot, repairable) in children.into_iter().rev() {
                slot.finish(repairable);
            }
        }
        let (facts, all_repairable) = observations.take();
        assert_eq!(
            facts
                .iter()
                .map(|fact| fact.name.as_str())
                .collect::<Vec<_>>(),
            ["0:0", "0:1", "1:0"]
        );
        assert!(
            facts
                .iter()
                .all(|fact| fact.outcome == ToolOutcome::Repairable)
        );
        assert!(!all_repairable);
    }

    #[test_case(3, None, None; "legacy_message")]
    #[test_case(2, None, None; "nondefault_threshold")]
    #[test_case(2, Some(REPEAT_GUIDANCE), None; "custom_guidance")]
    #[test_case(2, Some(REPEAT_GUIDANCE), Some(MODEL_REPEAT_GUIDANCE); "model_guidance")]
    fn repeat_refusal_message_uses_threshold_and_resolved_guidance(
        threshold: usize,
        prompt: Option<&str>,
        model_prompt: Option<&str>,
    ) {
        smol::block_on(async {
            let mut ctx = local_ctx(OBSERVED_TOOL, |_| Ok(String::new()));
            let (tx, rx) = flume::unbounded();
            ctx.event_tx = EventSender::new(tx, 0);
            Arc::make_mut(&mut ctx.config.steering)
                .rules
                .repeated_tool_call
                .prompt = prompt.map(str::to_owned);
            if let Some(prompt) = model_prompt {
                Arc::make_mut(&mut ctx.config.steering)
                    .models
                    .entry(ctx.model.spec())
                    .or_default()
                    .rules
                    .repeated_tool_call
                    .prompt = Some(prompt.into());
            }
            let mut recent = RecentCalls::with_threshold(threshold);
            for _ in 1..threshold {
                recent.record(OBSERVED_TOOL.into(), &Value::Null);
            }
            let mut history = History::new(Vec::new());
            process_tool_calls(
                (0..2)
                    .map(|index| (index.to_string(), OBSERVED_TOOL.into(), Value::Null))
                    .collect(),
                &mut recent,
                None,
                &mut history,
                &ctx.event_tx,
                &ctx,
            )
            .await
            .unwrap();
            let results: Vec<_> = rx
                .try_iter()
                .filter_map(|envelope| match envelope.event {
                    AgentEvent::ToolDone(done) => Some(done),
                    _ => None,
                })
                .collect();
            assert_eq!(results.len(), 2);
            let expected = if threshold == DOOM_LOOP_THRESHOLD {
                crate::tools::DOOM_LOOP_MESSAGE.to_owned()
            } else {
                format!(
                    "{REPEAT_TWO_PREFIX}{}",
                    model_prompt.or(prompt).unwrap_or(DOOM_LOOP_GUIDANCE)
                )
            };
            for done in results {
                assert!(done.is_error);
                assert_eq!(done.output.as_text(), expected);
            }
        });
    }

    #[test_case(0, false; "disabled")]
    #[test_case(3, true; "legacy_threshold")]
    #[test_case(4, false; "higher_threshold")]
    fn repeat_threshold_is_configurable(threshold: usize, blocked: bool) {
        let mut recent = RecentCalls::with_threshold(threshold);
        for _ in 0..2 {
            recent.record(OBSERVED_TOOL.into(), &Value::Null);
        }
        assert_eq!(recent.is_doom_loop(OBSERVED_TOOL, &Value::Null), blocked);
        if threshold == 0 {
            assert!(recent.calls.is_empty());
        }
    }

    #[test_case(0, ToolOutcome::Success; "disabled_guard_executes")]
    #[test_case(3, ToolOutcome::Repairable; "repeat_refusal_is_observed")]
    fn top_level_repeat_policy_contributes_observations(threshold: usize, expected: ToolOutcome) {
        smol::block_on(async {
            let observations = ResponseObservations::new(1);
            let mut ctx = local_ctx(OBSERVED_TOOL, |_| Ok(String::new()));
            let (tx, _rx) = flume::unbounded();
            ctx.event_tx = EventSender::new(tx, 0);
            ctx.steering_observations = Some(observations.clone());
            let mut recent = RecentCalls::with_threshold(threshold);
            for _ in 0..2 {
                recent.record(OBSERVED_TOOL.into(), &Value::Null);
            }
            let mut history = History::new(Vec::new());
            process_tool_calls(
                vec![(OBSERVED_TOOL.into(), OBSERVED_TOOL.into(), Value::Null)],
                &mut recent,
                None,
                &mut history,
                &ctx.event_tx,
                &ctx,
            )
            .await
            .unwrap();
            let (facts, all_repairable) = observations.take();
            assert_eq!(all_repairable, expected == ToolOutcome::Repairable);
            assert_eq!(facts[0].outcome, expected);
        });
    }

    #[test_case(false, false, ToolOutcome::Failure; "error_text_is_not_a_signal")]
    #[test_case(true, false, ToolOutcome::Repairable; "host_validation_error")]
    #[test_case(true, true, ToolOutcome::Failure; "cancellation_is_not_repairable")]
    fn local_validation_observations_are_explicit(
        mark: bool,
        cancelled: bool,
        expected: ToolOutcome,
    ) {
        smol::block_on(async {
            let observations = ResponseObservations::new(1);
            let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            ctx.local_tools = Arc::new(HashMap::from([(
                OBSERVED_TOOL.into(),
                crate::tools::local_tool(move |_, ctx| {
                    Box::pin(async move {
                        if mark {
                            ctx.mark_tool_result_repairable();
                        }
                        Err(OBSERVED_ERROR.into())
                    })
                }),
            )]));
            ctx.steering_observations = Some(observations.clone());
            if cancelled {
                let (trigger, token) = CancelToken::new();
                ctx.cancel = token;
                trigger.cancel();
            }
            let done = run(
                &ctx.registry,
                None,
                OBSERVED_TOOL.into(),
                OBSERVED_TOOL,
                &Value::Null,
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(done.is_error);
            let (facts, all_repairable) = observations.take();
            assert_eq!(all_repairable, expected == ToolOutcome::Repairable);
            assert_eq!(facts[0].outcome, expected);
        });
    }

    #[test]
    fn observations_canonicalize_aliases_and_json_keys() {
        let observations = ResponseObservations::new(2);
        let mut ctx = local_ctx(OBSERVED_TOOL, |_| Ok(String::new()));
        ctx.tool_name_aliases = Some(Arc::new(HashMap::from([(
            "alias".into(),
            OBSERVED_TOOL.into(),
        )])));
        ctx.steering_observations = Some(observations.clone());
        ctx.steering_order = vec![0];
        observe_context(
            &mut ctx,
            "functions.alias",
            &serde_json::from_str(r#"{"b":{"z":1,"a":2},"a":0}"#).unwrap(),
        );
        ctx.steering_order = vec![1];
        observe_context(
            &mut ctx,
            OBSERVED_TOOL,
            &serde_json::from_str(r#"{"a":0,"b":{"a":2,"z":1}}"#).unwrap(),
        );
        let (facts, _) = observations.take();
        assert_eq!(facts[0], facts[1]);
        assert_eq!(facts[0].name, OBSERVED_TOOL);
    }

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

    const RAN_ANYWAY: &str = "a tool whose arguments never parsed must not run";
    const RAW_TEXT_LOST: &str = "the model needs the raw text back to see where it went wrong";

    /// Eager tool streaming turns off Anthropic's per-argument validation, so a
    /// truncated body reaches dispatch instead of being rejected upstream.
    #[test]
    fn an_unparseable_tool_input_is_reported_rather_than_run() {
        const TRUNCATED: &str = r#"{"path": "/a", "content": "half a fi"#;

        smol::block_on(async {
            let mut ctx = local_ctx("batch", |_| panic!("{RAN_ANYWAY}"));
            ctx.json_repair.register_invalid(
                "t1",
                InvalidToolInput {
                    raw: TRUNCATED.into(),
                    complete: false,
                    clipped: false,
                },
            );
            let observations = ResponseObservations::new(1);
            ctx.steering_observations = Some(observations.clone());
            let done = run(
                ToolRegistry::global(),
                None,
                "t1".into(),
                "batch",
                &serde_json::json!({ INVALID_TOOL_JSON_KEY: TRUNCATED }),
                &ctx,
                Emit::Silent,
            )
            .await;

            assert!(done.is_error, "{RAN_ANYWAY}");
            assert!(done.output.as_text().contains(TRUNCATED), "{RAW_TEXT_LOST}");
            let (facts, all_repairable) = observations.take();
            assert!(all_repairable);
            assert_eq!(facts[0].outcome, ToolOutcome::Repairable);
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
    #[test_case(serde_json::json!({"query": 42}) ; "nonstring_query")]
    #[test_case(serde_json::json!({"query": "!?"}) ; "no_search_tokens")]
    fn tool_search_bad_query_is_error_event(input: Value) {
        smol::block_on(async {
            let mcp = crate::mcp::stub_session(&[("srv.tool", "")]);
            let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            let observations = ResponseObservations::new(1);
            ctx.steering_observations = Some(observations.clone());
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
            assert_eq!(
                done.output.as_text(),
                crate::tools::deferral::SEARCH_EMPTY_QUERY
            );
            let (facts, all_repairable) = observations.take();
            assert!(all_repairable);
            assert_eq!(facts[0].outcome, ToolOutcome::Repairable);
        });
    }

    #[test_case(serde_json::json!({}), ToolOutcome::Repairable; "missing_query")]
    #[test_case(serde_json::json!({"query": false}), ToolOutcome::Repairable; "nonstring_query")]
    #[test_case(serde_json::json!({"query": " \t"}), ToolOutcome::Repairable; "blank_query")]
    #[test_case(serde_json::json!({"query": "!?"}), ToolOutcome::Repairable; "no_search_tokens")]
    #[test_case(serde_json::json!({"query": "unmatched"}), ToolOutcome::Success; "valid_no_match")]
    #[test_case(serde_json::json!({"query": "issue"}), ToolOutcome::Success; "valid_match")]
    fn deferred_builtin_search_observes_input_validation(input: Value, expected: ToolOutcome) {
        use crate::tools::{DeferralSession, DeferredTool};

        smol::block_on(async {
            let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            ctx.deferral = Some(DeferralSession::new(
                vec![DeferredTool::new(
                    "fetch_issue",
                    None,
                    serde_json::json!({
                        "name": "fetch_issue", "description": "Fetch an issue", "input_schema": {"type": "object"}
                    }),
                )],
                std::iter::empty(),
            ));
            let observations = ResponseObservations::new(1);
            ctx.steering_observations = Some(observations.clone());
            let done = run(
                &ctx.registry,
                None,
                "t1".into(),
                TOOL_SEARCH_TOOL_NAME,
                &input,
                &ctx,
                Emit::Silent,
            )
            .await;
            assert_eq!(done.is_error, expected == ToolOutcome::Repairable);
            let (facts, all_repairable) = observations.take();
            assert_eq!(all_repairable, expected == ToolOutcome::Repairable);
            assert_eq!(facts[0].outcome, expected);
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

    use crate::ToolInput;
    use crate::tools::{
        BoxFuture, DescriptionContext, ExecFuture, HeaderFuture, HeaderResult, ParseError,
        PermissionScopes, Tool, ToolExecResult,
    };

    #[derive(Default)]
    struct StartProbe {
        started: Arc<AtomicBool>,
        executed: Arc<AtomicBool>,
        abandoned: Arc<AtomicUsize>,
    }

    struct StartProbeInvocation {
        started: Arc<AtomicBool>,
        executed: Arc<AtomicBool>,
        abandoned: Arc<AtomicUsize>,
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
        fn abandon<'a>(&'a self, _ctx: &'a ToolContext) -> BoxFuture<'a, ()> {
            self.abandoned.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::ready(()))
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
                abandoned: Arc::clone(&self.abandoned),
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
    const REFUSAL_RELEASES_PREFLIGHT: &str =
        "a refused call must release the state its preflight took";
    const REFUSAL_NAMES_THE_CALL: &str =
        "a tool that stays listed must say which call was refused and why";

    /// Registered mutating while each call declares its own effect, the way
    /// `memory` browses and writes through a single registration. Like a shell
    /// line it can only name its effect once `preflight` has parsed it, so it
    /// answers with the registered worst case until then.
    struct EffectProbe {
        call_effect: ToolEffect,
        trace: Arc<ProbeTrace>,
    }

    #[derive(Default)]
    struct ProbeTrace {
        executed: AtomicBool,
        abandoned: AtomicBool,
    }

    struct EffectProbeInvocation {
        call_effect: ToolEffect,
        preflighted: AtomicBool,
        trace: Arc<ProbeTrace>,
    }

    impl ToolInvocation for EffectProbeInvocation {
        fn start_header(&self) -> HeaderFuture {
            HeaderFuture::Ready(HeaderResult::plain("effect probe".into()))
        }

        fn call_effect(&self, registered: ToolEffect) -> ToolEffect {
            if self.preflighted.load(Ordering::SeqCst) {
                self.call_effect
            } else {
                registered
            }
        }

        fn preflight<'a>(
            &'a self,
            _ctx: &'a ToolContext,
        ) -> BoxFuture<'a, Result<Option<PermissionIntent>, String>> {
            self.preflighted.store(true, Ordering::SeqCst);
            Box::pin(std::future::ready(Ok(None)))
        }

        fn abandon<'a>(&'a self, _ctx: &'a ToolContext) -> BoxFuture<'a, ()> {
            self.trace.abandoned.store(true, Ordering::SeqCst);
            Box::pin(std::future::ready(()))
        }

        fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
            self.trace.executed.store(true, Ordering::SeqCst);
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
                preflighted: AtomicBool::new(false),
                trace: Arc::clone(&self.trace),
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

    /// Reports the outcome alongside what the call reached.
    async fn run_effect_probe(
        mode: &AgentMode,
        call_effect: ToolEffect,
        source: ToolSource,
    ) -> (ToolDoneEvent, Arc<ProbeTrace>) {
        let trace = Arc::new(ProbeTrace::default());
        let registry = ToolRegistry::new();
        registry
            .register_audited(
                Arc::new(EffectProbe {
                    call_effect,
                    trace: Arc::clone(&trace),
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
        (done, trace)
    }

    #[test_case(AgentMode::Plan(PLAN_PATH.into()) ; "plan_mode")]
    #[test_case(AgentMode::ReadOnly ; "read_only_mode")]
    fn a_read_only_call_of_a_mutating_tool_runs(mode: AgentMode) {
        smol::block_on(async {
            let (done, trace) =
                run_effect_probe(&mode, ToolEffect::ReadOnly, trusted_native_source()).await;

            assert!(!done.is_error, "{}", done.output.as_text());
            assert!(trace.executed.load(Ordering::SeqCst));
        });
    }

    #[test_case(AgentMode::Plan(PLAN_PATH.into()), crate::tools::PLAN_WRITE_RESTRICTED ; "plan_mode")]
    #[test_case(AgentMode::ReadOnly, READ_ONLY_TOOL_RESTRICTED ; "read_only_mode")]
    fn a_mutating_call_of_the_same_tool_is_refused(mode: AgentMode, expected: &str) {
        smol::block_on(async {
            let (done, trace) =
                run_effect_probe(&mode, ToolEffect::Mutating, trusted_native_source()).await;

            assert!(done.is_error);
            assert!(
                done.output.as_text().starts_with(expected),
                "{}",
                done.output.as_text()
            );
            assert!(!trace.executed.load(Ordering::SeqCst));
            assert!(
                trace.abandoned.load(Ordering::SeqCst),
                "{REFUSAL_RELEASES_PREFLIGHT}"
            );
            if matches!(mode, AgentMode::ReadOnly) {
                let text = done.output.as_text();
                assert!(
                    text.contains(ToolEffect::Mutating.as_str())
                        && text.contains(READ_ONLY_CALL_GUIDANCE),
                    "{REFUSAL_NAMES_THE_CALL}"
                );
            }
        });
    }

    #[test_case(AgentMode::Plan(PLAN_PATH.into()), crate::tools::PLAN_WRITE_RESTRICTED ; "plan_mode")]
    #[test_case(AgentMode::ReadOnly, READ_ONLY_TOOL_RESTRICTED ; "read_only_mode")]
    fn an_unbundled_plugin_cannot_downgrade_its_own_call_effect(mode: AgentMode, expected: &str) {
        smol::block_on(async {
            let (done, trace) = run_effect_probe(
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
            assert!(!trace.executed.load(Ordering::SeqCst));
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
            let (started, executed, abandoned) = (
                Arc::clone(&probe.started),
                Arc::clone(&probe.executed),
                Arc::clone(&probe.abandoned),
            );
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
            assert_eq!(abandoned.load(Ordering::SeqCst), 0);
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
            let (started, executed, abandoned) = (
                Arc::clone(&probe.started),
                Arc::clone(&probe.executed),
                Arc::clone(&probe.abandoned),
            );
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
            assert_eq!(abandoned.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn execution_start_prevents_preflight_abandon() {
        smol::block_on(async {
            let permissions = Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig {
                    default: DefaultEffect::Allow,
                    ..PermissionsConfig::default()
                },
                PathBuf::from("/tmp"),
                Arc::default(),
            ));
            let ctx = crate::tools::test_support::stub_ctx_with_permissions(
                &AgentMode::Build,
                permissions,
            );
            let probe = StartProbe::default();
            let abandoned = Arc::clone(&probe.abandoned);
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
            assert!(!done.is_error);
            assert_eq!(abandoned.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn plan_refusal_abandons_preflight_once() {
        smol::block_on(async {
            let ctx = crate::tools::test_support::stub_ctx(&AgentMode::Plan("/tmp/plan.md".into()));
            let probe = StartProbe::default();
            let abandoned = Arc::clone(&probe.abandoned);
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
            assert_eq!(abandoned.load(Ordering::SeqCst), 1);
        });
    }

    const BASELINE_TOOL: &str = "baseline_probe";
    const NO_CAPTURE_MSG: &str = "a call that cannot change a file captures nothing";
    const CAPTURE_MSG: &str = "a call that can change a file captures first";
    const PROCEED_MSG: &str = "a refused workspace costs revert, not the call";
    const BLOCKED_MSG: &str = "a failed capture leaves nothing to revert to, so nothing may change";

    /// A store under a regular file can never be created, which is the one
    /// capture failure a test can provoke without racing the filesystem.
    enum BaselineStore {
        Usable(SnapshotLimits),
        Broken,
    }

    fn baseline_ctx(
        effect: ToolEffect,
        store: BaselineStore,
        executed: Arc<AtomicBool>,
    ) -> (TempDir, ToolContext, Arc<SnapshotStore>) {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("tracked.txt"), "alpha").unwrap();
        let store = match store {
            BaselineStore::Usable(limits) => {
                Arc::new(SnapshotStore::new(temp.path().join("snapshots")).with_limits(limits))
            }
            BaselineStore::Broken => {
                let path = temp.path().join("not-a-directory");
                std::fs::write(&path, "").unwrap();
                Arc::new(SnapshotStore::new(path))
            }
        };
        let baseline =
            crate::workspace_baseline::WorkspaceBaseline::new(Arc::clone(&store), root, true);
        let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
        ctx.baseline = Some(crate::workspace_baseline::BaselineGate::new(baseline, None));
        ctx.local_tools = Arc::new(HashMap::from([(
            BASELINE_TOOL.to_owned(),
            crate::tools::audited_local_tool(effect, move |_input, _ctx| {
                let executed = Arc::clone(&executed);
                Box::pin(async move {
                    executed.store(true, Ordering::SeqCst);
                    Ok(String::new())
                })
            }),
        )]));
        (temp, ctx, store)
    }

    async fn run_baseline_probe(ctx: &ToolContext) -> ToolDoneEvent {
        run(
            ToolRegistry::global(),
            None,
            "t1".into(),
            BASELINE_TOOL,
            &serde_json::json!({}),
            ctx,
            Emit::Silent,
        )
        .await
    }

    #[test_case(ToolEffect::ReadOnly, false ; "read_only")]
    #[test_case(ToolEffect::Isolated, false ; "isolated")]
    #[test_case(ToolEffect::Mutating, true  ; "mutating")]
    #[test_case(ToolEffect::Unknown,  true  ; "unclassified")]
    fn only_a_call_that_could_change_a_file_captures_a_baseline(
        effect: ToolEffect,
        expect_capture: bool,
    ) {
        smol::block_on(async {
            let executed = Arc::new(AtomicBool::new(false));
            let (_temp, ctx, store) = baseline_ctx(
                effect,
                BaselineStore::Usable(SnapshotLimits::default()),
                Arc::clone(&executed),
            );

            let done = run_baseline_probe(&ctx).await;

            assert!(!done.is_error, "{}", done.output.as_text());
            assert!(executed.load(Ordering::SeqCst), "{CAPTURE_MSG}");
            assert_eq!(
                store.has_session_start(),
                expect_capture,
                "{}",
                if expect_capture {
                    CAPTURE_MSG
                } else {
                    NO_CAPTURE_MSG
                }
            );
        });
    }

    #[test]
    fn a_refused_workspace_runs_the_call_without_a_baseline() {
        smol::block_on(async {
            let executed = Arc::new(AtomicBool::new(false));
            let (_temp, ctx, store) = baseline_ctx(
                ToolEffect::Mutating,
                BaselineStore::Usable(SnapshotLimits {
                    max_files: 0,
                    ..SnapshotLimits::default()
                }),
                Arc::clone(&executed),
            );

            let done = run_baseline_probe(&ctx).await;

            assert!(!done.is_error, "{PROCEED_MSG}: {}", done.output.as_text());
            assert!(executed.load(Ordering::SeqCst), "{PROCEED_MSG}");
            assert!(!store.has_session_start(), "{PROCEED_MSG}");
        });
    }

    #[test]
    fn a_failed_capture_blocks_the_call() {
        smol::block_on(async {
            let executed = Arc::new(AtomicBool::new(false));
            let (_temp, ctx, _store) = baseline_ctx(
                ToolEffect::Mutating,
                BaselineStore::Broken,
                Arc::clone(&executed),
            );

            let done = run_baseline_probe(&ctx).await;

            assert!(done.is_error, "{BLOCKED_MSG}");
            assert!(
                done.output.as_text().contains(SNAPSHOT_FAILED),
                "{BLOCKED_MSG}: {}",
                done.output.as_text()
            );
            assert!(!executed.load(Ordering::SeqCst), "{BLOCKED_MSG}");
        });
    }
}

#[cfg(test)]
mod telemetry_tests {
    use test_case::test_case;

    use super::*;

    const BEFORE: &str = "a\nb\nc\n";
    const AFTER: &str = "a\nB\nc\nd\n";
    const CALL_ID: &str = "call-1";

    #[test_case("operation was cancelled", LedgerOutcome::Cancelled, ERROR_CANCELLED; "cancelled")]
    #[test_case("command timed out after 120s", LedgerOutcome::Timeout, ERROR_TIMEOUT; "timed_out")]
    #[test_case("permission denied: bash", LedgerOutcome::Denied, ERROR_DENIED; "denied")]
    #[test_case("no such file or directory", LedgerOutcome::NotFound, ERROR_NOT_FOUND; "missing_file")]
    #[test_case("invalid input: expected a string", LedgerOutcome::InvalidInput, ERROR_INVALID_INPUT; "invalid")]
    #[test_case("boom", LedgerOutcome::Other, ERROR_OTHER; "fallback")]
    fn errors_bucket_into_low_cardinality_types(
        text: &str,
        outcome: LedgerOutcome,
        attribute: &str,
    ) {
        assert_eq!(classify_error(text), outcome);
        assert_eq!(error_type(outcome), Some(attribute));
    }

    #[test]
    fn a_successful_call_reports_no_error_type() {
        assert_eq!(error_type(LedgerOutcome::Ok), None);
    }

    #[test]
    fn accounting_records_the_outcome_and_what_the_result_costs_the_window() {
        let mut done = ToolDoneEvent::error(CALL_ID.into(), "no such file or directory");
        account(&mut done, SOURCE_NATIVE, Duration::from_millis(42));

        assert_eq!(done.accounting.duration_ms, 42);
        assert_eq!(done.accounting.outcome, Some(LedgerOutcome::NotFound));
        assert_eq!(done.accounting.source.as_deref(), Some(SOURCE_NATIVE));
        assert!(done.accounting.model_tokens > 0);
    }

    #[test]
    fn a_call_that_succeeded_is_accounted_as_such() {
        let mut done = ToolDoneEvent::error(CALL_ID.into(), "");
        done.is_error = false;
        account(&mut done, SOURCE_NATIVE, Duration::ZERO);

        assert_eq!(done.accounting.outcome, Some(LedgerOutcome::Ok));
        assert_eq!(done.accounting.model_tokens, 0);
    }

    #[test]
    fn diffs_count_added_and_removed_lines() {
        assert_eq!(changed_lines(BEFORE, AFTER), (2, 1));
        assert_eq!(changed_lines(BEFORE, BEFORE), (0, 0));
    }

    fn patched(additions: usize, deletions: usize) -> crate::PatchedFile {
        crate::PatchedFile {
            path: "src/lib.rs".into(),
            patch: String::new(),
            additions,
            deletions,
            truncated: false,
        }
    }

    /// Most editing lands as a patch — every `file_apply_patch`, every
    /// `file_edit --replace_all`, and a write whose old side was too large to
    /// carry — so counting only the two-sided form reported close to nothing.
    #[test]
    fn a_patch_counts_the_lines_it_moved_across_every_file() {
        assert_eq!(
            edited_lines(&ToolOutput::Patch {
                files: vec![patched(3, 1), patched(4, 2)],
            }),
            Some((7, 3))
        );
        assert_eq!(
            edited_lines(&ToolOutput::Diff {
                path: "src/lib.rs".into(),
                before: BEFORE.into(),
                after: AFTER.into(),
                summary: String::new(),
            }),
            Some((2, 1))
        );
        assert_eq!(
            edited_lines(&ToolOutput::WriteCode {
                path: "src/lib.rs".into(),
                byte_count: AFTER.len(),
                lines: AFTER.lines().map(str::to_owned).collect(),
            }),
            None,
            "a created file replaced nothing, so it moved no lines"
        );
    }
}
