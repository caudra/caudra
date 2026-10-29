//! One learner run: a headless, read-only child agent on the Learn job that
//! reads the digest, corroborates it through `history` in `code`, and reports
//! through `structured_output`. Its conversation never reaches the session:
//! its history lives in memory only, its events reach the frontends only as
//! [`LearnerActivity`] to watch live, and its transcript is kept with the run.

use std::fmt::Write;
use std::sync::Arc;

use caudra_config::{CODE_TOOL_NAME, LearningTarget};
use caudra_providers::ModelPurpose;
use caudra_storage::usage_ledger::LedgerPurpose;
use futures_lite::future;
use serde_json::Value;
use tracing::warn;

use super::apply::{OUTPUT_SCHEMA, Report};
use super::digest::Digest;
use super::targets::{InstructionFile, heading};
use super::{LearnerActivity, LearnerStep};
use crate::AgentMode;
pub use crate::agent::budget::Spend;
use crate::agent::budget::{Overrun, RunBudget};
use crate::agent::subagent::{TaskIdentity, TaskModel};
use crate::agent::task_runner::{SubagentTaskRunner, TaskRequest, TaskRunner, WorkflowHostContext};
use crate::cancel::CancelToken;
use crate::subagent_history::{SubagentHistoryStore, SubagentTaskMode};
use crate::tools::{MEMORY_TOOL_NAME, ToolFilter};
use crate::types::{AgentEvent, EventSender};

/// Tool calls one run may make before it is stopped.
pub const MAX_TOOL_CALLS: usize = 40;
/// Input and output tokens one run may spend before it is stopped.
pub const MAX_TOKENS: u64 = 300_000;
const BUDGET: RunBudget = RunBudget::new(MAX_TOOL_CALLS, MAX_TOKENS);
pub const TOO_MANY_CALLS: &str = "the learner made too many tool calls";
pub const TOO_MANY_TOKENS: &str = "the learner spent too many tokens";
pub const NO_REPORT: &str = "the learner sent no report";
const LABEL: &str = "learner";
const CALL_PREFIX: &str = "learn:";
const PROMPT: &str = include_str!("../prompts/learner.md");
const NO_FILE: &str = "(no such file yet)";
const NO_BLOCK: &str = "(no learned block yet)";

/// A session's task host narrowed to what a learner may do: `code` and
/// `memory` only, no recall, nobody to ask, history held to this project,
/// and a task history of its own that nothing persists.
pub fn learner_host(mut host: WorkflowHostContext) -> WorkflowHostContext {
    host.config.history_project_only = true;
    host.config.no_memory_recall = true;
    host.user_response_rx = None;
    host.subagent_history = SubagentHistoryStore::default();
    host.jobs = None;
    host.plan = None;
    host.mode = Arc::new(|| AgentMode::Build);
    host.tool_ceiling =
        ToolFilter::Only(vec![CODE_TOOL_NAME.to_owned(), MEMORY_TOOL_NAME.to_owned()])
            .intersect(&host.tool_ceiling);
    host
}

/// What the learner is shown besides its instructions.
pub struct Shown<'a> {
    pub digest: &'a Digest,
    pub memory_view: Option<String>,
    pub files: &'a [InstructionFile],
    pub targets: &'a [LearningTarget],
}

/// The task prompt: the instructions, the targets enabled here with each
/// file's block, the memory view, and the digest.
pub fn prompt(shown: &Shown<'_>) -> String {
    let mut prompt = format!("{PROMPT}\n## Enabled targets\n\n");
    let enabled: Vec<&str> = shown.targets.iter().map(|target| target.as_str()).collect();
    let _ = writeln!(prompt, "{}\n", enabled.join(", "));
    for file in shown.files {
        let _ = writeln!(
            prompt,
            "### `{}`: {} ({})\n",
            file.target.as_str(),
            heading(file.target).trim_start_matches("## "),
            file.path.display()
        );
        let bullets = file.bullets();
        match (&file.content, bullets.is_empty()) {
            (None, _) => prompt.push_str(NO_FILE),
            (Some(_), true) => prompt.push_str(NO_BLOCK),
            (Some(_), false) => {
                for bullet in bullets {
                    let _ = writeln!(prompt, "- {bullet}");
                }
            }
        }
        prompt.push_str("\n\n");
    }
    if let Some(view) = &shown.memory_view {
        let _ = writeln!(prompt, "## Memory view\n\n{view}\n");
    }
    let _ = write!(
        prompt,
        "## Digest\n\n{} exchanges from {} sessions, oldest first.\n\n{}",
        shown.digest.exchanges, shown.digest.sessions, shown.digest.text
    );
    prompt
}

/// How a run went.
#[derive(Debug)]
pub struct Ran {
    pub report: Result<Report, String>,
    pub model: String,
    pub tokens: u64,
    /// The learner's conversation as JSON messages, when the child closed.
    pub transcript: Option<String>,
}

/// Runs the learner on `host` with `prompt`, stopping it at the first cap it
/// reaches.
pub async fn run(
    host: Arc<WorkflowHostContext>,
    run_id: i64,
    prompt: String,
    spend: &Spend,
) -> Ran {
    let (events_tx, events_rx) = flume::unbounded();
    let (trigger, cancel) = CancelToken::new();
    let request = TaskRequest {
        prompt: Some(prompt),
        label: LABEL.to_owned(),
        task: TaskIdentity::Derive,
        mode: Some(SubagentTaskMode::Plan),
        profile: None,
        model: TaskModel {
            job: Some(ModelPurpose::Learn),
            ..TaskModel::default()
        },
        output_schema: Some(Value::clone(&OUTPUT_SCHEMA)),
        call_id: format!("{CALL_PREFIX}{run_id}"),
        provenance: None,
    };
    let runner = SubagentTaskRunner::new(host);
    let task = runner.run(request, cancel, EventSender::new(events_tx, 0));
    let mut watched = Watched {
        spend,
        run_id,
        budget: BUDGET,
        transcript: None,
    };
    let mut stopped: Option<&str> = None;
    let mut trigger = Some(trigger);
    let watch = async {
        while let Ok(envelope) = events_rx.recv_async().await {
            watched.take(envelope.event).await;
            if let Some(reason) = watched.over()
                && let Some(trigger) = trigger.take()
            {
                stopped = Some(reason);
                trigger.cancel();
            }
        }
        future::pending().await
    };
    let outcome = future::or(task, watch).await;
    while let Ok(envelope) = events_rx.try_recv() {
        watched.take(envelope.event).await;
    }
    let report = match (stopped, outcome.success) {
        (Some(reason), _) => Err(reason.to_owned()),
        (None, false) => Err(outcome.error.unwrap_or_else(|| NO_REPORT.to_owned())),
        (None, true) => serde_json::from_value(outcome.output).map_err(|error| error.to_string()),
    };
    Ran {
        report,
        model: outcome.model.unwrap_or_default(),
        tokens: watched.budget.tokens().max(outcome.tokens_used),
        transcript: watched.transcript,
    }
}

/// What a run has done so far: its spend and calls against the caps, and its
/// conversation once the child hands it back. Everything else it does goes
/// on to the frontends as [`LearnerActivity`].
struct Watched<'a> {
    spend: &'a Spend,
    run_id: i64,
    budget: RunBudget,
    transcript: Option<String>,
}

impl Watched<'_> {
    async fn take(&mut self, event: AgentEvent) {
        self.budget.observe(&event);
        match &event {
            AgentEvent::SubagentHistory { messages, .. } => {
                self.transcript = serde_json::to_string(messages)
                    .inspect_err(
                        |error| warn!(%error, run = self.run_id, "learner transcript not kept"),
                    )
                    .ok();
                return;
            }
            AgentEvent::TurnComplete(turn) => {
                self.spend.report(turn, LedgerPurpose::Learning).await;
            }
            _ => {}
        }
        self.spend
            .events
            .try_send(AgentEvent::LearnerActivity(Box::new(LearnerActivity {
                run_id: self.run_id,
                step: LearnerStep::Event(Box::new(event)),
            })));
    }

    fn over(&self) -> Option<&'static str> {
        self.budget.overrun().map(|overrun| match overrun {
            Overrun::ToolCalls => TOO_MANY_CALLS,
            Overrun::Tokens => TOO_MANY_TOKENS,
        })
    }
}
