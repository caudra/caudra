//! Non-interactive (headless) mode: `caudra --print --prompt "..."`.
//!
//! Wire format intentionally matches Claude Code so existing scripts work
//! unchanged. Keep `PrintResult` fields a strict subset of theirs. `StreamJson`
//! is JSONL with the same shape, `Text` prints the raw response only.
//!
//! We adopt new fields when Claude Code adds them but never invent our own,
//! save for what a Caudra-only flag such as `--autopilot` reports, and only
//! when it is passed. Check their docs before changing anything here.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use caudra_agent::autopilot::{ActionName, AutopilotHandle};
use caudra_agent::headless::{HeadlessHandle, HeadlessParams};
use caudra_agent::permissions::PluginRuleStore;
use caudra_agent::tools::QUESTION_TOOL_NAME;
use caudra_agent::types::AutopilotEvent;
use caudra_agent::{
    AgentConfig, AgentEvent, DoneReason, Envelope, GoalHandle, GoalVerdict, ImageSource,
    PermissionsConfig,
};
use caudra_config::decisions::DecisionsConfig;
use caudra_config::{AutopilotConfig, ModelPolicy, SnapshotsConfig};
use caudra_lua::EventHandle;
use caudra_providers::model::Model;
use caudra_providers::{Billing, TokenUsage, add_cost};
use caudra_storage::autopilot::{AutopilotBudget, AutopilotEnd};
use caudra_storage::id::SessionRef;
use caudra_storage::sessions::PermissionMode;
use caudra_storage::usage_ledger::LedgerPurpose;
use clap::ValueEnum;
use color_eyre::Result;
use color_eyre::eyre::eyre;
use serde::Serialize;
use serde_json::{Value, json};

use crate::sdk_mode::{AUTOPILOT_SYSTEM_SUBTYPE, AutopilotSystemPayload};

const AGENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const NO_PROMPT: &str = "no prompt: pass --prompt \"<text>\" or pipe text on stdin";
const SYSTEM_EVENT: &str = "system";
const AUTOPILOT_ENDED: &str = "Autopilot ended";
const CONTINUATIONS_SPENT: &str = "continuation limit reached";
const COST_SPENT: &str = "cost limit reached";
const TIME_SPENT: &str = "time limit reached";

// Fails fast: silently dropping an image the caller explicitly attached
// would be worse than erroring.
fn load_images(paths: &[PathBuf]) -> Result<Vec<ImageSource>> {
    paths
        .iter()
        .map(|path| {
            let media_type = caudra_ui::image::media_type_for(path)
                .ok_or_else(|| eyre!("unsupported image type: {}", path.display()))?;
            caudra_ui::image::load_file_image(path, media_type)
                .map_err(|e| eyre!("failed to load image: {e}"))
        })
        .collect()
}

fn add_spend(
    billed: &mut Option<f64>,
    subscription: &mut Option<f64>,
    amount: Option<f64>,
    billing: Billing,
) {
    match billing {
        Billing::Api => add_cost(billed, amount),
        Billing::Subscription => add_cost(subscription, amount),
    }
}

#[derive(Clone, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

#[derive(Serialize)]
struct PrintResult {
    #[serde(rename = "type")]
    result_type: &'static str,
    subtype: &'static str,
    is_error: bool,
    duration_ms: u128,
    num_turns: u32,
    result: String,
    stop_reason: Option<DoneReason>,
    session_id: SessionRef,
    total_cost_usd: f64,
    /// What a subscription covered, at API list rates. Reported beside
    /// `total_cost_usd` and never added to it, which stays actual spend.
    subscription_cost_usd: f64,
    usage: TokenUsage,
    /// Only when `--autopilot` engaged the run.
    #[serde(skip_serializing_if = "Option::is_none")]
    autopilot: Option<AutopilotSummary>,
}

/// How autopilot fared over the run: whether it ended and why, the
/// continuations it spent, and the decisions it made, escalations included.
#[derive(Debug, PartialEq, Serialize)]
struct AutopilotSummary {
    ended: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    end: Option<AutopilotEnd>,
    continuations: u32,
    decisions: u32,
    escalations: u32,
}

/// An autopilot event, in the body an SDK client reads it in.
#[derive(Serialize)]
struct AutopilotSystemEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    subtype: &'static str,
    #[serde(flatten)]
    payload: AutopilotSystemPayload<'a>,
    session_id: &'a SessionRef,
}

/// What the run's autopilot events add up to.
#[derive(Default)]
struct AutopilotTally {
    decisions: u32,
    escalations: u32,
    ended: Option<AutopilotEnd>,
}

impl AutopilotTally {
    fn observe(&mut self, event: &AutopilotEvent) {
        match event {
            AutopilotEvent::Decided(decision) => {
                self.decisions += 1;
                if decision.action == ActionName::Escalate {
                    self.escalations += 1;
                }
            }
            AutopilotEvent::Ended { reason, .. } => self.ended = Some(reason.clone()),
            _ => {}
        }
    }

    /// Why autopilot fails the run: a limit or an error ended it. A goal it
    /// gave up on fails as the goal's own result, and a stop does not fail.
    fn failure(&self) -> Option<String> {
        let reason = match self.ended.as_ref()? {
            AutopilotEnd::Budget {
                kind: AutopilotBudget::Continuations,
            } => CONTINUATIONS_SPENT,
            AutopilotEnd::Budget {
                kind: AutopilotBudget::Cost,
            } => COST_SPENT,
            AutopilotEnd::Budget {
                kind: AutopilotBudget::Time,
            } => TIME_SPENT,
            AutopilotEnd::Error { message } => message,
            AutopilotEnd::Stopped { .. } | AutopilotEnd::GoalMet | AutopilotEnd::GoalImpossible => {
                return None;
            }
        };
        Some(format!("{AUTOPILOT_ENDED}: {reason}"))
    }

    fn summary(self, autopilot: &AutopilotHandle) -> AutopilotSummary {
        AutopilotSummary {
            ended: self.ended.is_some(),
            end: self.ended,
            continuations: autopilot
                .stored()
                .map_or(0, |engagement| engagement.continuations),
            decisions: self.decisions,
            escalations: self.escalations,
        }
    }
}

/// The autopilot `--autopilot` engages for the run, under the `[autopilot]`
/// limits read once at startup. It owns the goal a `/goal <condition>` prompt
/// set.
fn launch_autopilot(
    limits: AutopilotConfig,
    goal: &GoalHandle,
) -> (AutopilotHandle, Option<AutopilotEvent>) {
    let autopilot = AutopilotHandle::default();
    autopilot.set_limits(limits);
    let condition = goal.snapshot().map(|goal| goal.condition.to_string());
    let engaged = autopilot.engage(condition.is_some(), condition);
    (autopilot, engaged)
}

/// What a subagent spent, which an engaged autopilot counts as the goal does,
/// and the end it reached. The delegate's spend is counted where it is made.
fn subagent_spend(
    autopilot: Option<&AutopilotHandle>,
    cost: Option<f64>,
    billing: Billing,
    purpose: LedgerPurpose,
) -> Option<AutopilotEvent> {
    autopilot
        .filter(|_| purpose != LedgerPurpose::Autopilot)
        .and_then(|autopilot| autopilot.record_spend(cost, billing, &autopilot.limits()))
}

/// Counts an autopilot event towards the result, and shows it where the run
/// shows its events.
fn note_autopilot(
    event: &AutopilotEvent,
    tally: &mut AutopilotTally,
    out: Option<&mut VerboseOutput>,
    session_id: &SessionRef,
) -> Result<()> {
    tally.observe(event);
    match (out, AutopilotSystemPayload::from_event(event)) {
        (Some(out), Some(payload)) => out.emit(&AutopilotSystemEvent {
            event_type: SYSTEM_EVENT,
            subtype: AUTOPILOT_SYSTEM_SUBTYPE,
            payload,
            session_id,
        }),
        _ => Ok(()),
    }
}

#[derive(Serialize)]
struct InitEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    subtype: &'static str,
    cwd: &'a str,
    session_id: &'a SessionRef,
    tools: &'a [String],
    model: &'a str,
}

#[derive(Serialize)]
struct AssistantEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    message: AssistantMessage<'a>,
    session_id: &'a SessionRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_tool_use_id: Option<&'a str>,
}

#[derive(Serialize)]
struct AssistantMessage<'a> {
    model: &'a str,
    role: &'static str,
    content: &'a Value,
    usage: &'a TokenUsage,
}

#[derive(Serialize)]
struct UserEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    message: UserMessage<'a>,
    session_id: &'a SessionRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_tool_use_id: Option<&'a str>,
}

#[derive(Serialize)]
struct UserMessage<'a> {
    role: &'static str,
    content: &'a Value,
}

#[derive(Serialize)]
struct RetryEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    subtype: &'static str,
    attempt: u32,
    retry_delay_ms: u64,
    error: &'a str,
    session_id: &'a SessionRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_tool_use_id: Option<&'a str>,
}

enum VerboseOutput {
    StreamJson,
    Json(Vec<Value>),
}

impl VerboseOutput {
    fn emit(&mut self, value: &impl Serialize) -> Result<()> {
        match self {
            Self::StreamJson => println!("{}", serde_json::to_string(value)?),
            Self::Json(events) => events.push(serde_json::to_value(value)?),
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    model: &Model,
    prompt_arg: Option<String>,
    image_paths: Vec<PathBuf>,
    format: OutputFormat,
    verbose: bool,
    mut config: AgentConfig,
    permissions_config: PermissionsConfig,
    decisions_config: DecisionsConfig,
    seed_permission_mode: Option<PermissionMode>,
    snapshots: SnapshotsConfig,
    timeouts: caudra_providers::Timeouts,
    lua_handle: EventHandle,
    fast: bool,
    thinking: caudra_providers::ThinkingConfig,
    system_prompt_profile: Option<Arc<caudra_agent::prompt::profile::SystemPromptProfile>>,
    prompt_profiles: Arc<caudra_agent::prompt::profile::PromptProfileCatalog>,
    model_policy: Arc<ModelPolicy>,
    plugin_rules: Arc<PluginRuleStore>,
    remote_environment: Option<caudra_agent::headless::RemoteEnvironment>,
    workspace_session: Option<caudra_workspace::WorkspaceSession>,
    remote_project_context: Option<Arc<caudra_agent::remote_project_context::RemoteProjectContext>>,
    local_documents: Option<Arc<caudra_storage::local_documents::LocalDocumentStore>>,
    autopilot_limits: Option<AutopilotConfig>,
) -> Result<ExitCode> {
    let prompt = prompt_arg.ok_or_else(|| eyre!(NO_PROMPT))?;

    let images = load_images(&image_paths)?;
    let (prompt, goal) = print_goal(prompt)?;
    let (autopilot, engaged) = autopilot_limits
        .map(|limits| launch_autopilot(limits, &goal))
        .unzip();

    // Print mode mints a throwaway session id and never opens a session store,
    // so a generated title would have nowhere to land.
    config.generate_titles = false;
    let prompt_slots = lua_handle.collect_prompt_slots(&config);

    let cwd = remote_environment.as_ref().map_or_else(
        || std::env::current_dir().unwrap_or_else(|_| ".".into()),
        |environment| environment.cwd.clone().into(),
    );
    let (mcp_handle, mcp_config_errors) = if remote_environment.is_some() {
        smol::block_on(caudra_agent::mcp::start_global_connected(&cwd))
    } else {
        smol::block_on(caudra_agent::mcp::start_connected(&cwd))
    };
    if !mcp_config_errors.is_empty() {
        eprintln!("MCP config error: {mcp_config_errors}");
    }
    if let Some(handle) = &mcp_handle {
        let awaiting: Vec<_> = handle
            .reader()
            .load()
            .infos
            .iter()
            .filter(|info| info.status == caudra_agent::McpServerStatus::AwaitingTrust)
            .map(|info| info.name.clone())
            .collect();
        if !awaiting.is_empty() {
            return Err(eyre!(
                "project MCP servers require startup trust: {}. Run `caudra`, review them with `/mcp`, then retry",
                awaiting.join(", ")
            ));
        }
    }

    let handle = caudra_agent::headless::spawn(HeadlessParams {
        model: model.clone(),
        config,
        permissions_config,
        decisions_config,
        seed_permission_mode,
        snapshots,
        timeouts,
        prompt,
        thinking,
        images,
        prompt_slots,
        system_prompt_profile,
        prompt_profiles,
        excluded_tools: vec![QUESTION_TOOL_NAME],
        mcp_handle,
        // `cwd` is the sandbox's path once a remote environment exists, so the
        // host checkout has to be named separately.
        host_cwd: remote_environment
            .is_some()
            .then(|| std::env::current_dir().ok())
            .flatten(),
        initial_wd: cwd,
        fast,
        model_policy,
        plugin_rules,
        goal,
        remote_environment,
        workspace_session,
        remote_project_context,
        local_documents,
        autopilot: autopilot.clone(),
    })?;

    let HeadlessHandle {
        event_rx,
        tool_names,
        session_id,
        cwd,
        goal,
        task,
    } = handle;
    crate::setup::report_session_start(caudra_otel::emit::START_FRESH, Some(&session_id));
    let start = Instant::now();

    let mut verbose_out = match format {
        OutputFormat::StreamJson => Some(VerboseOutput::StreamJson),
        _ if verbose => Some(VerboseOutput::Json(Vec::new())),
        _ => None,
    };

    if let Some(out) = &mut verbose_out {
        out.emit(&InitEvent {
            event_type: "system",
            subtype: "init",
            cwd: &cwd,
            session_id: &session_id,
            tools: &tool_names,
            model: &model.id,
        })?;
    }
    let mut tally = AutopilotTally::default();
    if let Some(engaged) = engaged.flatten() {
        note_autopilot(&engaged, &mut tally, verbose_out.as_mut(), &session_id)?;
    }

    let mut result_text = String::new();
    let mut is_error = false;
    let mut num_turns: u32 = 0;
    let mut usage = TokenUsage::default();
    // Summed as the turns land: rates move mid-run, and only a turn knows the
    // rate it paid.
    let mut cost = None;
    let mut subscription_cost = None;
    let mut stop_reason: Option<DoneReason> = None;

    while let Ok(envelope) = smol::block_on(event_rx.recv_async()) {
        let Envelope {
            ref event,
            ref subagent,
            ..
        } = envelope;
        let parent_tool_use_id = subagent.as_ref().map(|s| s.parent_tool_use_id.as_str());

        match event {
            AgentEvent::TextDelta { text } => {
                if parent_tool_use_id.is_none() {
                    result_text.push_str(text);
                }
            }
            AgentEvent::ThinkingDelta { .. } | AgentEvent::ThinkingBoundary => {}
            AgentEvent::ToolPending { .. }
            | AgentEvent::ToolInputDelta { .. }
            | AgentEvent::ToolStart(_)
            | AgentEvent::ToolOutput { .. }
            | AgentEvent::ToolAnnotation { .. }
            | AgentEvent::ToolDone(_)
            | AgentEvent::TaskAdmitted(_)
            | AgentEvent::BatchProgress(_)
            | AgentEvent::McpCodeProgress(_)
            | AgentEvent::Question(_)
            | AgentEvent::QuestionClosed
            | AgentEvent::QueueItemConsumed { .. }
            | AgentEvent::QueueBatchConsumed { .. }
            | AgentEvent::QueueDrained
            | AgentEvent::Compacting
            | AgentEvent::CompactionDone
            | AgentEvent::MemoryChanged
            | AgentEvent::SessionTitle { .. }
            | AgentEvent::StreamReset
            | AgentEvent::AuthRequired
            | AgentEvent::AuthRestored
            | AgentEvent::PermissionRequest(_)
            | AgentEvent::PermissionRequestUpdated(_)
            | AgentEvent::PermissionRequestResolved { .. }
            | AgentEvent::SubagentProgress { .. }
            | AgentEvent::SubagentHistory { .. }
            | AgentEvent::ToolSnapshot { .. }
            | AgentEvent::ToolHeaderSnapshot { .. }
            | AgentEvent::LiveToolBuf { .. }
            | AgentEvent::Nudge { .. }
            | AgentEvent::ToolsLoaded { .. }
            | AgentEvent::Unrecorded { .. }
            | AgentEvent::Decision(_)
            | AgentEvent::PromptProgress { .. } => {}
            AgentEvent::Autopilot(event) => {
                note_autopilot(event, &mut tally, verbose_out.as_mut(), &session_id)?;
            }
            AgentEvent::Injected {
                text, peer_event, ..
            } => {
                if let Some(origin) = peer_event {
                    if let Some(out) = &mut verbose_out {
                        out.emit(&json!({
                            "type": "system",
                            "subtype": "peer_message",
                            "session_id": session_id,
                            "peer_event": origin,
                            "text": text,
                        }))?;
                    } else {
                        eprintln!(
                            "Peer message {} received; included in model context",
                            origin.message_id
                        );
                    }
                }
            }
            // One-shot print spawns no workflow runtime (`workflow: None`), so
            // nothing can launch a run here and the event has no consumer.
            AgentEvent::Workflow(_) => {}
            AgentEvent::GoalEvaluating { .. } | AgentEvent::GoalSkipped { .. } => {}
            AgentEvent::GoalEvaluation {
                cost: goal_cost,
                billing,
                ..
            } => {
                add_spend(&mut cost, &mut subscription_cost, *goal_cost, *billing);
            }
            AgentEvent::GoalFinished { result, .. } => {
                if result.verdict != GoalVerdict::Met {
                    is_error = true;
                    result_text = format!("Goal could not be achieved: {}", result.reason);
                }
            }
            AgentEvent::GoalDeferred {
                active_background_tasks: _,
            } => {}
            AgentEvent::GoalLoopCap {
                evaluations,
                continuations,
                limit,
            } => {
                is_error = true;
                result_text = format!(
                    "Goal remains active after {continuations} automatic continuations in this run ({evaluations} total evaluations); the session limit is {limit}"
                );
            }
            AgentEvent::GoalTurnLimit { evaluations } => {
                is_error = true;
                result_text = format!(
                    "Goal remains active after {evaluations} evaluations; the agent turn limit was reached"
                );
            }
            AgentEvent::GoalEvaluationFailed {
                evaluation,
                message,
                applied,
                cost: goal_cost,
                billing,
                ..
            } => {
                add_spend(&mut cost, &mut subscription_cost, *goal_cost, *billing);
                if *applied {
                    is_error = true;
                    result_text = format!("Goal evaluation #{evaluation} failed: {message}");
                }
            }
            AgentEvent::GoalClearedAfterError {
                condition, message, ..
            } => {
                is_error = true;
                result_text =
                    format!("Goal cleared after an unrecoverable error: {condition} ({message})");
            }
            AgentEvent::Retry {
                attempt,
                message,
                delay_ms,
            } => {
                if let Some(out) = &mut verbose_out {
                    out.emit(&RetryEvent {
                        event_type: "system",
                        subtype: "api_retry",
                        attempt: *attempt,
                        retry_delay_ms: *delay_ms,
                        error: message,
                        session_id: &session_id,
                        parent_tool_use_id,
                    })?;
                }
            }
            AgentEvent::TurnComplete(tc) => {
                add_spend(&mut cost, &mut subscription_cost, tc.cost, tc.billing);
                if parent_tool_use_id.is_some() {
                    goal.record_external_usage(tc.usage, tc.cost, tc.billing);
                    if let Some(ended) =
                        subagent_spend(autopilot.as_ref(), tc.cost, tc.billing, tc.purpose)
                    {
                        note_autopilot(&ended, &mut tally, verbose_out.as_mut(), &session_id)?;
                    }
                }
                if let Some(out) = &mut verbose_out {
                    let content_value = serde_json::to_value(&tc.message.content)?;
                    out.emit(&AssistantEvent {
                        event_type: "assistant",
                        message: AssistantMessage {
                            model: &tc.model,
                            role: "assistant",
                            content: &content_value,
                            usage: &tc.usage,
                        },
                        session_id: &session_id,
                        parent_tool_use_id,
                    })?;
                }
            }
            AgentEvent::ModelUsage {
                usage: repair_usage,
                cost: repair_cost,
                billing,
                purpose,
                ..
            } => {
                add_spend(&mut cost, &mut subscription_cost, *repair_cost, *billing);
                if parent_tool_use_id.is_some() {
                    goal.record_external_usage(*repair_usage, *repair_cost, *billing);
                    if let Some(ended) =
                        subagent_spend(autopilot.as_ref(), *repair_cost, *billing, *purpose)
                    {
                        note_autopilot(&ended, &mut tally, verbose_out.as_mut(), &session_id)?;
                    }
                } else {
                    usage += *repair_usage;
                }
                if let Some(out) = &mut verbose_out {
                    out.emit(&serde_json::json!({
                        "type": "system",
                        "subtype": "model_usage",
                        "accounting": event,
                        "session_id": session_id,
                        "parent_tool_use_id": parent_tool_use_id,
                    }))?;
                }
            }
            AgentEvent::ToolResultsSubmitted { message } => {
                if let Some(out) = &mut verbose_out {
                    let content_value = serde_json::to_value(&message.content)?;
                    out.emit(&UserEvent {
                        event_type: "user",
                        message: UserMessage {
                            role: "user",
                            content: &content_value,
                        },
                        session_id: &session_id,
                        parent_tool_use_id,
                    })?;
                }
            }
            AgentEvent::Done {
                usage: u,
                num_turns: turns,
                reason,
            } => {
                num_turns = *turns;
                usage = *u;
                stop_reason = Some(*reason);
                break;
            }
            AgentEvent::Error { message } => {
                is_error = true;
                result_text = message.clone();
                break;
            }
        }
    }
    smol::block_on(async {
        futures_lite::future::or(task, async {
            smol::Timer::after(AGENT_SHUTDOWN_TIMEOUT).await;
        })
        .await;
    });

    if let Some(failure) = tally.failure() {
        if !is_error {
            result_text = failure;
        }
        is_error = true;
    }
    let autopilot = autopilot.map(|autopilot| tally.summary(&autopilot));

    let duration_ms = start.elapsed().as_millis();
    // Zero on an unpriced model, which is what its turns reported too.
    let total_cost_usd = cost.unwrap_or_default();
    let subscription_cost_usd = subscription_cost.unwrap_or_default();

    match format {
        OutputFormat::Text => {
            print!("{result_text}");
        }
        OutputFormat::Json | OutputFormat::StreamJson => {
            let result = PrintResult {
                result_type: "result",
                subtype: if is_error { "error" } else { "success" },
                is_error,
                duration_ms,
                num_turns,
                result: result_text,
                stop_reason,
                session_id,
                total_cost_usd,
                subscription_cost_usd,
                usage,
                autopilot,
            };
            match verbose_out {
                Some(VerboseOutput::Json(mut events)) => {
                    events.push(serde_json::to_value(&result)?);
                    println!("{}", serde_json::to_string(&events)?);
                }
                _ => println!("{}", serde_json::to_string(&result)?),
            }
        }
    }

    Ok(if is_error {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn print_goal(prompt: String) -> Result<(String, GoalHandle)> {
    let trimmed = prompt.trim();
    let Some(args) = trimmed.strip_prefix("/goal") else {
        return Ok((prompt, GoalHandle::default()));
    };
    if !args.is_empty() && !args.starts_with(char::is_whitespace) {
        return Ok((prompt, GoalHandle::default()));
    }
    let condition = args.trim();
    if condition.is_empty() {
        return Err(eyre!("Usage: /goal <condition>"));
    }
    if matches!(
        condition.to_ascii_lowercase().as_str(),
        "clear" | "stop" | "off" | "reset" | "none" | "cancel"
    ) {
        return Err(eyre!("No goal set"));
    }
    let goal = GoalHandle::default();
    goal.set(condition)?;
    Ok((caudra_agent::goal_kickoff_message(condition), goal))
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_agent::autopilot::{AutopilotDecision, DecisionKind, EngagementSpend};
    use caudra_providers::TokenUsage;
    use caudra_storage::autopilot::{AutopilotStopper, DecisionOutcome};
    use test_case::test_case;

    const RETRY_PARENT_ID: &str = "toolu_task_1";
    const RETRY_ERROR: &str = "overloaded";
    const PARENT_KEY: &str = "parent_tool_use_id";

    const PRINT_RESULT_FIELDS: &[&str] = &[
        "type",
        "subtype",
        "is_error",
        "num_turns",
        "result",
        "stop_reason",
        "session_id",
        "total_cost_usd",
        "subscription_cost_usd",
        "usage",
        "duration_ms",
    ];
    const INIT_EVENT_FIELDS: &[&str] = &["type", "subtype", "cwd", "session_id", "tools", "model"];
    const RETRY_EVENT_FIELDS: &[&str] = &[
        "type",
        "subtype",
        "attempt",
        "retry_delay_ms",
        "error",
        "session_id",
    ];

    #[test]
    fn spend_is_attributed_to_its_billing_source() {
        let mut billed = None;
        let mut subscription = None;

        add_spend(&mut billed, &mut subscription, Some(1.0), Billing::Api);
        add_spend(
            &mut billed,
            &mut subscription,
            Some(2.0),
            Billing::Subscription,
        );

        assert_eq!(billed, Some(1.0));
        assert_eq!(subscription, Some(2.0));
    }

    #[test]
    fn wire_format_required_fields() {
        let result = PrintResult {
            result_type: "result",
            subtype: "success",
            is_error: false,
            duration_ms: 1234,
            num_turns: 2,
            result: "done".into(),
            stop_reason: Some(DoneReason::EndTurn),
            session_id: SessionRef::generate(),
            total_cost_usd: 0.003,
            subscription_cost_usd: 0.0,
            usage: TokenUsage::default(),
            autopilot: None,
        };
        let json: Value = serde_json::to_value(&result).unwrap();
        for field in PRINT_RESULT_FIELDS {
            assert!(json.get(field).is_some(), "PrintResult missing: {field}");
        }

        let sid = SessionRef::generate();
        let init = InitEvent {
            event_type: "system",
            subtype: "init",
            cwd: "/tmp",
            session_id: &sid,
            tools: &["bash".into(), "read".into()],
            model: "test-model",
        };
        let json: Value = serde_json::to_value(&init).unwrap();
        for field in INIT_EVENT_FIELDS {
            assert!(json.get(field).is_some(), "InitEvent missing: {field}");
        }

        let retry = RetryEvent {
            event_type: "system",
            subtype: "api_retry",
            attempt: 2,
            retry_delay_ms: 3000,
            error: "rate_limit",
            session_id: &sid,
            parent_tool_use_id: None,
        };
        let json: Value = serde_json::to_value(&retry).unwrap();
        for field in RETRY_EVENT_FIELDS {
            assert!(json.get(field).is_some(), "RetryEvent missing: {field}");
        }
    }

    /// A consumer cannot otherwise tell a task's backoff from the main
    /// conversation's, and the key stays absent for the main one so older
    /// readers see the shape they always did.
    #[test_case(None => None ; "main_retry_omits_the_key")]
    #[test_case(Some(RETRY_PARENT_ID) => Some(RETRY_PARENT_ID.to_owned()) ; "subagent_retry_names_its_parent")]
    fn retry_event_attributes_the_stream_that_backed_off(parent: Option<&str>) -> Option<String> {
        let session_id = SessionRef::generate();
        let json: Value = serde_json::to_value(RetryEvent {
            event_type: "system",
            subtype: "api_retry",
            attempt: 1,
            retry_delay_ms: 1_000,
            error: RETRY_ERROR,
            session_id: &session_id,
            parent_tool_use_id: parent,
        })
        .unwrap();
        json.get(PARENT_KEY)
            .map(|id| id.as_str().expect("serialized as a string").to_owned())
    }

    const GOAL_CONDITION: &str = "the tests pass";
    const PLAIN_PROMPT: &str = "fix the tests";
    const AUTOPILOT_FAILURE: &str = "the delegate kept failing";
    const AUTOPILOT_SEQ: u32 = 4;
    const AUTOPILOT_ENGAGEMENT: u32 = 1;
    const AUTOPILOT_SUBJECT: &str = "cargo test";
    const MAX_COST: f64 = 0.5;
    const SPEND: f64 = 0.75;
    const RETRY_ATTEMPT: u32 = 1;
    const RETRY_LIMIT: u32 = 3;
    const RETRY_DELAY: u64 = 1_000;
    const AUTOPILOT_KEY: &str = "autopilot";

    fn decided(action: ActionName) -> AutopilotEvent {
        AutopilotEvent::Decided(Box::new(AutopilotDecision {
            seq: AUTOPILOT_SEQ,
            at_ms: 0,
            kind: DecisionKind::Permission,
            action,
            subject: AUTOPILOT_SUBJECT.into(),
            reason: String::new(),
            agent: None,
            tool_use_id: None,
            history_len: 0,
            model: None,
            usage: None,
            cost: None,
            engagement: None,
            item: None,
            payload: None,
            outcome: None,
        }))
    }

    fn retried() -> AutopilotEvent {
        AutopilotEvent::Retrying {
            attempt: RETRY_ATTEMPT,
            limit: RETRY_LIMIT,
            delay_ms: RETRY_DELAY,
            error: RETRY_ERROR.into(),
        }
    }

    fn ended(reason: AutopilotEnd) -> AutopilotEvent {
        AutopilotEvent::Ended {
            reason,
            engagement: AUTOPILOT_ENGAGEMENT,
            spend: EngagementSpend::default(),
        }
    }

    fn budget(kind: AutopilotBudget) -> AutopilotEnd {
        AutopilotEnd::Budget { kind }
    }

    #[test_case(None => None; "still_engaged")]
    #[test_case(Some(budget(AutopilotBudget::Continuations)) => Some(format!("{AUTOPILOT_ENDED}: {CONTINUATIONS_SPENT}")); "continuations_spent")]
    #[test_case(Some(budget(AutopilotBudget::Cost)) => Some(format!("{AUTOPILOT_ENDED}: {COST_SPENT}")); "cost_spent")]
    #[test_case(Some(budget(AutopilotBudget::Time)) => Some(format!("{AUTOPILOT_ENDED}: {TIME_SPENT}")); "time_spent")]
    #[test_case(Some(AutopilotEnd::Error { message: AUTOPILOT_FAILURE.into() }) => Some(format!("{AUTOPILOT_ENDED}: {AUTOPILOT_FAILURE}")); "an_error")]
    #[test_case(Some(AutopilotEnd::Stopped { by: AutopilotStopper::Interrupt }) => None; "a_stop")]
    #[test_case(Some(AutopilotEnd::GoalMet) => None; "a_met_goal")]
    #[test_case(Some(AutopilotEnd::GoalImpossible) => None; "an_impossible_goal_fails_as_the_goal")]
    fn only_a_spent_limit_or_an_error_fails_the_run(end: Option<AutopilotEnd>) -> Option<String> {
        let mut tally = AutopilotTally::default();
        tally.observe(&retried());
        tally.observe(&decided(ActionName::Allow));
        if let Some(reason) = end {
            tally.observe(&ended(reason));
        }
        tally.failure()
    }

    #[test]
    fn the_summary_counts_decisions_escalations_and_continuations() {
        let (autopilot, _) = launch_autopilot(AutopilotConfig::default(), &GoalHandle::default());
        autopilot.try_continue(&autopilot.limits()).unwrap();
        let mut tally = AutopilotTally::default();
        for event in [
            retried(),
            decided(ActionName::Allow),
            decided(ActionName::Escalate),
            decided(ActionName::Deny),
            ended(AutopilotEnd::GoalMet),
        ] {
            tally.observe(&event);
        }

        assert_eq!(
            tally.summary(&autopilot),
            AutopilotSummary {
                ended: true,
                end: Some(AutopilotEnd::GoalMet),
                continuations: 1,
                decisions: 3,
                escalations: 1,
            }
        );
    }

    #[test_case(None => None; "without_autopilot")]
    #[test_case(Some(AutopilotSummary { ended: true, end: Some(AutopilotEnd::GoalMet), continuations: 2, decisions: 3, escalations: 1 }) => Some(json!({"ended": true, "end": {"reason": "goal_met"}, "continuations": 2, "decisions": 3, "escalations": 1})); "ended")]
    #[test_case(Some(AutopilotSummary { ended: false, end: None, continuations: 0, decisions: 0, escalations: 0 }) => Some(json!({"ended": false, "continuations": 0, "decisions": 0, "escalations": 0})); "still_engaged")]
    fn the_result_reports_autopilot_only_when_it_engaged(
        autopilot: Option<AutopilotSummary>,
    ) -> Option<Value> {
        let result = PrintResult {
            result_type: "result",
            subtype: "success",
            is_error: false,
            duration_ms: 0,
            num_turns: 1,
            result: String::new(),
            stop_reason: Some(DoneReason::EndTurn),
            session_id: SessionRef::generate(),
            total_cost_usd: 0.0,
            subscription_cost_usd: 0.0,
            usage: TokenUsage::default(),
            autopilot,
        };
        serde_json::to_value(&result)
            .unwrap()
            .get(AUTOPILOT_KEY)
            .cloned()
    }

    #[test_case(format!("/goal {GOAL_CONDITION}") => (Some(GOAL_CONDITION.to_owned()), true); "a_goal_prompt_is_owned")]
    #[test_case(PLAIN_PROMPT.to_owned() => (None, false); "a_plain_prompt_sets_no_goal")]
    fn autopilot_engages_once_owning_the_goal_prompt(prompt: String) -> (Option<String>, bool) {
        let (_, goal) = print_goal(prompt).unwrap();
        let (autopilot, engaged) = launch_autopilot(AutopilotConfig::default(), &goal);

        assert_eq!(
            engaged,
            Some(AutopilotEvent::Engaged {
                engagement: AUTOPILOT_ENGAGEMENT,
                goal: goal.active_condition(),
            })
        );
        assert_eq!(autopilot.engage(false, None), None);
        (goal.active_condition(), autopilot.owns_goal())
    }

    #[test_case(LedgerPurpose::Chat => true; "a_subagent_turn_counts")]
    #[test_case(LedgerPurpose::Autopilot => false; "the_delegate_counts_where_it_is_made")]
    fn a_subagent_s_spend_reaches_the_cost_limit(purpose: LedgerPurpose) -> bool {
        let (autopilot, _) = launch_autopilot(
            AutopilotConfig {
                max_cost: Some(MAX_COST),
                ..AutopilotConfig::default()
            },
            &GoalHandle::default(),
        );

        let ended = subagent_spend(Some(&autopilot), Some(SPEND), Billing::Api, purpose);

        assert_eq!(ended.is_some(), !autopilot.is_engaged());
        matches!(ended, Some(AutopilotEvent::Ended { reason, .. })
            if reason == budget(AutopilotBudget::Cost))
    }

    #[test]
    fn a_verbose_run_shows_autopilot_events_as_an_sdk_client_reads_them() {
        let session_id = SessionRef::generate();
        let mut out = VerboseOutput::Json(Vec::new());
        let mut tally = AutopilotTally::default();

        for event in [
            AutopilotEvent::Engaged {
                engagement: AUTOPILOT_ENGAGEMENT,
                goal: None,
            },
            AutopilotEvent::ImplementPlan { seq: AUTOPILOT_SEQ },
            decided(ActionName::Escalate),
            AutopilotEvent::Settled {
                seq: AUTOPILOT_SEQ,
                outcome: DecisionOutcome::DeniedUnattended,
            },
        ] {
            note_autopilot(&event, &mut tally, Some(&mut out), &session_id).unwrap();
        }

        let VerboseOutput::Json(events) = out else {
            unreachable!("built as Json");
        };
        let shown: Vec<_> = events
            .iter()
            .map(|event| {
                (
                    event["type"].clone(),
                    event["subtype"].clone(),
                    event["kind"].clone(),
                    event["decision"]["action"].clone(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                (
                    json!(SYSTEM_EVENT),
                    json!(AUTOPILOT_SYSTEM_SUBTYPE),
                    json!("engaged"),
                    Value::Null,
                ),
                (
                    json!(SYSTEM_EVENT),
                    json!(AUTOPILOT_SYSTEM_SUBTYPE),
                    json!("decided"),
                    json!(ActionName::Escalate.name()),
                ),
                (
                    json!(SYSTEM_EVENT),
                    json!(AUTOPILOT_SYSTEM_SUBTYPE),
                    json!("settled"),
                    Value::Null,
                ),
            ]
        );
        assert_eq!((tally.decisions, tally.escalations), (1, 1));
    }
}
