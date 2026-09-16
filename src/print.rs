//! Non-interactive (headless) mode: `caudra --print --prompt "..."`.
//!
//! Wire format intentionally matches Claude Code so existing scripts work
//! unchanged. Keep `PrintResult` fields a strict subset of theirs. `StreamJson`
//! is JSONL with the same shape, `Text` prints the raw response only.
//!
//! We adopt new fields when Claude Code adds them but never invent our own.
//! Check their docs before changing anything here.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use caudra_agent::headless::{HeadlessHandle, HeadlessParams};
use caudra_agent::permissions::PluginRuleStore;
use caudra_agent::tools::QUESTION_TOOL_NAME;
use caudra_agent::{
    AgentConfig, AgentEvent, DoneReason, Envelope, GoalHandle, GoalVerdict, ImageSource,
    PermissionsConfig,
};
use caudra_config::ModelPolicy;
use caudra_lua::EventHandle;
use caudra_providers::model::Model;
use caudra_providers::{Billing, TokenUsage, add_cost};
use caudra_storage::id::SessionRef;
use clap::ValueEnum;
use color_eyre::Result;
use color_eyre::eyre::eyre;
use serde::Serialize;
use serde_json::Value;

const AGENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const NO_PROMPT: &str = "no prompt: pass --prompt \"<text>\" or pipe text on stdin";

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
) -> Result<()> {
    let prompt = prompt_arg.ok_or_else(|| eyre!(NO_PROMPT))?;

    let images = load_images(&image_paths)?;
    let (prompt, goal) = print_goal(prompt)?;

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
        timeouts,
        prompt,
        thinking,
        images,
        prompt_slots,
        system_prompt_profile,
        prompt_profiles,
        excluded_tools: vec![QUESTION_TOOL_NAME],
        mcp_handle,
        initial_wd: cwd,
        fast,
        model_policy,
        plugin_rules,
        goal,
        remote_environment,
        workspace_session,
        remote_project_context,
        local_documents,
    });

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
            | AgentEvent::BatchProgress(_)
            | AgentEvent::Question(_)
            | AgentEvent::QueueItemConsumed { .. }
            | AgentEvent::QueueBatchConsumed { .. }
            | AgentEvent::QueueDrained
            | AgentEvent::Compacting
            | AgentEvent::CompactionDone
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
            | AgentEvent::Nudge
            | AgentEvent::Injected { .. }
            | AgentEvent::ToolsLoaded { .. }
            | AgentEvent::PromptProgress { .. } => {}
            // One-shot print spawns no workflow runtime (`workflow: None`), so
            // nothing can launch a run here and the event has no consumer.
            AgentEvent::Workflow(_) => {}
            AgentEvent::GoalEvaluating { .. } => {}
            AgentEvent::GoalEvaluation {
                cost: goal_cost,
                billing,
                ..
            } => {
                add_spend(&mut cost, &mut subscription_cost, *goal_cost, *billing);
            }
            AgentEvent::GoalFinished { result } => {
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
            AgentEvent::GoalClearedAfterError { condition, message } => {
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
                    })?;
                }
            }
            AgentEvent::TurnComplete(tc) => {
                add_spend(&mut cost, &mut subscription_cost, tc.cost, tc.billing);
                if parent_tool_use_id.is_some() {
                    goal.record_external_usage(tc.usage, tc.cost, tc.billing);
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
                ..
            } => {
                add_spend(&mut cost, &mut subscription_cost, *repair_cost, *billing);
                if parent_tool_use_id.is_some() {
                    goal.record_external_usage(*repair_usage, *repair_cost, *billing);
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

    Ok(())
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
    use caudra_providers::TokenUsage;

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
        };
        let json: Value = serde_json::to_value(&retry).unwrap();
        for field in RETRY_EVENT_FIELDS {
            assert!(json.get(field).is_some(), "RetryEvent missing: {field}");
        }
    }
}
