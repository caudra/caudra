//! The session's workflow runtime as the app sees it. Requests leave as
//! spawned tasks and answer through a reply channel the tick drains, so the
//! UI thread never waits on the runtime. The run list is a mirror of what the
//! runtime published, kept current by its events and by the replies that
//! carry a snapshot; the runtime's own read model stays the quiescence
//! authority in `AgentHandles`.

use std::borrow::Cow;

use caudra_agent::workflow::WorkflowHandle;
use caudra_providers::Message;
#[cfg(test)]
use caudra_workflow::RunStatus;
use caudra_workflow::{
    LaunchRequest, LogLine, MAX_AGENT_BUDGET, MAX_RUN_LOG_ENTRIES, RunSnapshot, WorkflowError,
    WorkflowEvent, WorkflowRequest, WorkflowResponse,
};
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::app::App;
use crate::components::Action;
use crate::components::command::CommandAction;
use crate::components::logs_modal::LogsAction;
use crate::components::workflow_catalog_picker::WorkflowCatalogAction;
use crate::components::workflow_inspector::{InspectorAction, RunControl};
use crate::repaint::Dirty;

pub(crate) const UNAVAILABLE_MSG: &str = "Workflows are unavailable in this session";
pub(crate) const WORKFLOW_USAGE: &str = "Usage: /workflow [runs | <name> [--agent-budget N] [args] | pause|stop <run> | resume <run> [budget]]";
const UNKNOWN_RUN: &str = "Unknown workflow run: ";
const AMBIGUOUS: &str = "Several runs match";
const BUDGET_RANGE: &str = "An agent budget must be between 1 and";
const MAX_CANDIDATES: usize = 5;
pub(crate) const DEEP_RESEARCH_USAGE: &str = "Usage: /deep-research <query>";
pub(crate) const TRUST_HINT: &str = "run /workflows to review and trust it";
pub(crate) const DEEP_RESEARCH_WORKFLOW: &str = "deep-research";
const RUNS_SUBCOMMAND: &str = "runs";
const HISTORY_LIMIT: Option<usize> = None;
const BUDGET_FLAG: &str = "--agent-budget";
const QUERY_ARG: &str = "query";
const OBJECTIVE_ARG: &str = "objective";
const REPORT_FIELD: &str = "report";
const REPORT_LABEL: &str = "\nReport: ";
const RESULT_LABEL: &str = "\nResult: ";
const SCRATCH_LABEL: &str = "\nScratch file: ";
const PAUSED_LABEL: &str = "\nPaused: ";
const ERROR_LABEL: &str = "\nError: ";
/// What a completion notice may carry into the turn: enough for a report,
/// not enough to crowd out the conversation it lands in. The same shape and
/// bound the headless surface uses, so a model sees one format everywhere.
const NOTICE_BODY_LIMIT: usize = 8 * 1024;
const TRUNCATED_MARKER: &str = "\u{2026}[truncated]";

pub(crate) struct WorkflowUi {
    handle: Option<WorkflowHandle>,
    runs: Vec<RunSnapshot>,
    reply_tx: flume::Sender<Reply>,
    reply_rx: flume::Receiver<Reply>,
    /// Requests a test would have shipped, and the switch that lets it ship
    /// them without a runtime to answer.
    #[cfg(test)]
    pub(crate) sent: Vec<WorkflowRequest>,
    #[cfg(test)]
    scripted: bool,
}

/// What the app meant by a request, so its answer knows where to land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Intent {
    Catalog,
    Launch,
    Trust,
    Control(RunControl),
    Inspect,
    History,
    Ack,
}

pub(crate) struct Reply {
    pub(crate) intent: Intent,
    pub(crate) result: Result<WorkflowResponse, WorkflowError>,
}

/// What a run selector named: nothing, the run id of exactly one run, or the
/// display names of the several it could have meant.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resolution {
    Miss,
    One(String),
    Many(Vec<String>),
}

impl WorkflowUi {
    pub(crate) fn new() -> Self {
        let (reply_tx, reply_rx) = flume::unbounded();
        Self {
            handle: None,
            runs: Vec::new(),
            reply_tx,
            reply_rx,
            #[cfg(test)]
            sent: Vec::new(),
            #[cfg(test)]
            scripted: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn script(&mut self) {
        self.scripted = true;
    }

    /// Adopts the runtime's read model. A run the mirror already knows at the
    /// same revision keeps its local acknowledgement, so a respawn that hands
    /// the same runtime back cannot re-announce a completion whose ack is
    /// still in flight.
    pub(crate) fn set_handle(&mut self, handle: Option<WorkflowHandle>) {
        let published = handle
            .as_ref()
            .map(|handle| handle.state().runs.clone())
            .unwrap_or_default();
        let previous = std::mem::take(&mut self.runs);
        self.runs = published
            .into_iter()
            .map(|mut run| {
                if let Some(known) = previous
                    .iter()
                    .find(|known| known.run_id == run.run_id && known.revision == run.revision)
                {
                    run.outbox_pending = known.outbox_pending;
                }
                run
            })
            .collect();
        self.handle = handle;
    }

    pub(crate) fn available(&self) -> bool {
        #[cfg(test)]
        if self.scripted {
            return true;
        }
        self.handle.is_some()
    }

    pub(crate) fn runs(&self) -> &[RunSnapshot] {
        &self.runs
    }

    #[cfg(test)]
    pub(crate) fn count(&self, status: RunStatus) -> usize {
        self.runs.iter().filter(|run| run.status == status).count()
    }

    pub(crate) fn apply(&mut self, snapshot: RunSnapshot) {
        match self
            .runs
            .iter()
            .position(|run| run.run_id == snapshot.run_id)
        {
            Some(index) => self.runs[index] = snapshot,
            None => self.runs.insert(0, snapshot),
        }
    }

    /// A line the run logged since its last snapshot, kept to the same tail
    /// the runtime keeps. The run it names, when the mirror knows it.
    pub(crate) fn apply_log(&mut self, run_id: &str, line: LogLine) -> Option<&RunSnapshot> {
        let run = self.runs.iter_mut().find(|run| run.run_id == run_id)?;
        if run.logs.len() >= MAX_RUN_LOG_ENTRIES {
            run.logs.remove(0);
        }
        run.logs.push(line);
        Some(run)
    }

    /// The run a user named, by display name first and run id second.
    fn find(&self, target: &str) -> Option<&RunSnapshot> {
        self.runs
            .iter()
            .find(|run| run.display_name == target)
            .or_else(|| self.runs.iter().find(|run| run.run_id == target))
    }

    /// What a selector named. An exact display name or run id wins outright;
    /// otherwise a prefix of either names the run, so a user can type the
    /// part of it they can read. A prefix several runs answer to is reported
    /// as the choice it is rather than resolved to the first of them.
    pub(crate) fn resolve(&self, target: &str) -> Resolution {
        if let Some(run) = self.find(target) {
            return Resolution::One(run.run_id.clone());
        }
        let candidates: Vec<&RunSnapshot> = self
            .runs
            .iter()
            .filter(|run| run.display_name.starts_with(target) || run.run_id.starts_with(target))
            .collect();
        match candidates.as_slice() {
            [] => Resolution::Miss,
            [run] => Resolution::One(run.run_id.clone()),
            many => Resolution::Many(
                many.iter()
                    .take(MAX_CANDIDATES)
                    .map(|run| run.display_name.clone())
                    .collect(),
            ),
        }
    }

    /// Ships `request` and answers through the reply channel. `false` when
    /// there is no runtime, in which case nothing was sent.
    pub(crate) fn dispatch(&mut self, intent: Intent, request: WorkflowRequest) -> bool {
        #[cfg(test)]
        {
            self.sent.push(request.clone());
            if self.scripted {
                return true;
            }
        }
        let Some(handle) = self.handle.clone() else {
            return false;
        };
        let reply_tx = self.reply_tx.clone();
        smol::spawn(async move {
            let result = handle.request(request).await;
            let _ = reply_tx.send(Reply { intent, result });
        })
        .detach();
        true
    }

    /// Every notice waiting for a quiet turn, each acknowledged at the
    /// revision it was read at. The mirror clears them on the spot so the
    /// next claim cannot repeat one whose ack has not landed yet.
    pub(crate) fn claim_completions(&mut self) -> Vec<Message> {
        let pending: Vec<(String, u64, String)> = self
            .runs
            .iter_mut()
            .filter(|run| run.outbox_pending)
            .map(|run| {
                run.outbox_pending = false;
                (run.run_id.clone(), run.revision, completion_notice(run))
            })
            .collect();
        pending
            .into_iter()
            .map(|(run_id, revision, notice)| {
                self.dispatch(
                    Intent::Ack,
                    WorkflowRequest::AckCompletion { run_id, revision },
                );
                Message::observation(notice)
            })
            .collect()
    }

    fn poll(&self) -> Option<Reply> {
        self.reply_rx.try_recv().ok()
    }

    #[cfg(test)]
    pub(crate) fn inject_reply(&self, reply: Reply) {
        self.reply_tx.send(reply).unwrap();
    }
}

impl App {
    pub(super) fn workflows_browse(&mut self) -> Vec<Action> {
        if !self.workflow.available() {
            self.flash(UNAVAILABLE_MSG.into());
            return Vec::new();
        }
        self.workflow_catalog_picker.open();
        self.workflow
            .dispatch(Intent::Catalog, WorkflowRequest::List);
        Vec::new()
    }

    pub(super) fn execute_workflow(&mut self, args: &str) -> Vec<Action> {
        let args = args.trim();
        if !self.workflow.available() {
            self.flash(UNAVAILABLE_MSG.into());
            return Vec::new();
        }
        if args.is_empty() || args == RUNS_SUBCOMMAND {
            self.open_workflow_inspector(None);
            return Vec::new();
        }
        let (head, rest) = split_word(args);
        if let Some(control) = RunControl::parse(head) {
            self.control_workflow_by_name(control, rest.trim());
            return Vec::new();
        }
        match parse_launch(args) {
            Ok(launch) => {
                self.workflow
                    .dispatch(Intent::Launch, WorkflowRequest::Start(launch));
            }
            Err(message) => self.flash(message),
        }
        Vec::new()
    }

    pub(super) fn execute_deep_research(&mut self, query: &str) -> Vec<Action> {
        let query = query.trim();
        if query.is_empty() {
            self.flash(DEEP_RESEARCH_USAGE.into());
            return Vec::new();
        }
        self.execute_workflow(&format!("{DEEP_RESEARCH_WORKFLOW} {query}"))
    }

    fn control_workflow_by_name(&mut self, control: RunControl, target: &str) {
        if target.is_empty() {
            self.flash(WORKFLOW_USAGE.into());
            return;
        }
        // Resume is the only control that takes an argument, because a
        // budget-limited run has no other way on.
        let (target, agent_budget) = match control {
            RunControl::Resume => trailing_budget(target),
            RunControl::Pause | RunControl::Stop => (target, None),
        };
        if agent_budget.is_some_and(|budget| !(1..=MAX_AGENT_BUDGET).contains(&budget)) {
            self.flash(format!("{BUDGET_RANGE} {MAX_AGENT_BUDGET}"));
            return;
        }
        match self.workflow.resolve(target) {
            Resolution::One(run_id) => self.control_workflow(control, run_id, agent_budget),
            Resolution::Miss => self.flash(format!("{UNKNOWN_RUN}{target}")),
            Resolution::Many(names) => {
                self.flash(format!("{AMBIGUOUS} {target}: {}", names.join(", ")));
            }
        }
    }

    fn control_workflow(&mut self, control: RunControl, run_id: String, agent_budget: Option<u32>) {
        let request = match control {
            RunControl::Pause => WorkflowRequest::Pause { run_id },
            RunControl::Resume => WorkflowRequest::Resume {
                run_id,
                agent_budget,
            },
            RunControl::Stop => WorkflowRequest::Stop { run_id },
        };
        self.dispatch_control(control, request);
    }

    fn dispatch_control(&mut self, control: RunControl, request: WorkflowRequest) {
        if !self.workflow.dispatch(Intent::Control(control), request) {
            self.flash(UNAVAILABLE_MSG.into());
        }
    }

    /// Opens the inspector on the mirror and asks the runtime for what the
    /// mirror does not hold: earlier sessions' runs once, and the selected
    /// run's detail whenever the selection or the run moves.
    pub(super) fn open_workflow_inspector(&mut self, preferred: Option<&str>) {
        if !self.workflow.available() {
            self.flash(UNAVAILABLE_MSG.into());
            return;
        }
        let first = self
            .workflow_inspector
            .open(self.workflow.runs().to_vec(), preferred);
        self.workflow.dispatch(
            Intent::History,
            WorkflowRequest::History {
                limit: HISTORY_LIMIT,
            },
        );
        if let Some(run_id) = first {
            self.inspect_workflow(run_id);
        }
    }

    fn inspect_workflow(&mut self, run_id: String) {
        self.workflow
            .dispatch(Intent::Inspect, WorkflowRequest::Inspect { run_id });
    }

    pub(super) fn handle_workflow_inspector_action(
        &mut self,
        action: InspectorAction,
    ) -> Vec<Action> {
        match action {
            InspectorAction::Consumed => {}
            InspectorAction::Close => self.workflow_inspector.close(),
            InspectorAction::Inspect(run_id) => self.inspect_workflow(run_id),
            InspectorAction::Control { control, run_id } => {
                self.control_workflow(control, run_id, None);
            }
            InspectorAction::OpenTranscript(task_id) => {
                self.workflow_inspector.close();
                self.preview_task(&task_id);
            }
            InspectorAction::ResumeWithBudget {
                run_id,
                agent_budget,
            } => {
                self.dispatch_control(
                    RunControl::Resume,
                    WorkflowRequest::Resume {
                        run_id,
                        agent_budget: Some(agent_budget),
                    },
                );
            }
            InspectorAction::OpenFile(path) => {
                self.workflow_inspector.close();
                self.open_workbench_file(&path, None);
            }
            InspectorAction::Copy { text, label } => {
                self.handle_logs_action(LogsAction::Copy { text, label });
            }
            InspectorAction::Flash(message) => self.flash(message.into()),
        }
        Vec::new()
    }

    pub(super) fn handle_workflow_catalog_action(
        &mut self,
        action: WorkflowCatalogAction,
    ) -> Vec<Action> {
        match action {
            WorkflowCatalogAction::Consumed | WorkflowCatalogAction::Close => Vec::new(),
            WorkflowCatalogAction::Launch(name) => self
                .handle_command_action(CommandAction::Complete(format!("/workflow {name} ")))
                .unwrap_or_default(),
            WorkflowCatalogAction::Trust { name, digest } => {
                self.workflow
                    .dispatch(Intent::Trust, WorkflowRequest::Trust { name, digest });
                Vec::new()
            }
        }
    }

    /// A run moved, or said something. Both reach the mirror and the run's
    /// transcript card; the inspector re-reads the mirror on a snapshot.
    pub(super) fn on_workflow_event(&mut self, event: WorkflowEvent) {
        match event {
            WorkflowEvent::Snapshot(snapshot) => {
                self.main_chat().workflow_card_update(&snapshot);
                self.workflow.apply(*snapshot);
                self.refresh_workflow_inspector();
            }
            WorkflowEvent::Log {
                run_id,
                at,
                message,
                ..
            } => {
                let line = LogLine { at, message };
                if let Some(run) = self.workflow.apply_log(&run_id, line).cloned() {
                    self.main_chat().workflow_card_update(&run);
                }
            }
        }
    }

    /// Brings every card the transcript holds up to what the mirror knows,
    /// for a restored session whose cards were drawn from stored results.
    pub(crate) fn refresh_workflow_cards(&mut self) {
        let runs = self.workflow.runs().to_vec();
        for run in &runs {
            self.main_chat().workflow_card_update(run);
        }
    }

    pub(super) fn poll_workflow_replies(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        while let Some(reply) = self.workflow.poll() {
            self.on_workflow_reply(reply);
            dirty = Dirty::YES;
        }
        dirty
    }

    pub(super) fn on_workflow_reply(&mut self, reply: Reply) {
        match (reply.intent, reply.result) {
            (Intent::Catalog, Ok(WorkflowResponse::Catalog(catalog))) => {
                self.workflow_catalog_picker.fill(catalog);
            }
            (Intent::Catalog, Err(error)) => self.workflow_catalog_picker.fail(error.to_string()),
            (Intent::Launch, Ok(WorkflowResponse::Started(snapshot))) => {
                self.flash(format!("Started {}", snapshot.display_name));
                self.main_chat().workflow_card_start(&snapshot);
                self.workflow.apply(*snapshot);
                self.refresh_workflow_inspector();
            }
            (Intent::Launch, Err(WorkflowError::TrustRequired { name, .. })) => {
                self.flash(format!("Workflow {name} is not trusted; {TRUST_HINT}"));
            }
            (Intent::Trust, Ok(WorkflowResponse::Trusted { name })) => {
                self.flash(format!("Trusted {name}"));
                if self.workflow_catalog_picker.is_open() {
                    self.workflow
                        .dispatch(Intent::Catalog, WorkflowRequest::List);
                }
            }
            (Intent::Control(control), Ok(WorkflowResponse::Run(snapshot))) => {
                self.flash(format!(
                    "{} {}",
                    control.past_tense(),
                    snapshot.display_name
                ));
                self.main_chat().workflow_card_update(&snapshot);
                self.workflow.apply(*snapshot);
                self.refresh_workflow_inspector();
            }
            (Intent::Inspect, Ok(WorkflowResponse::Detail(detail))) => {
                self.workflow_inspector.fill_detail(*detail);
            }
            (Intent::History, Ok(WorkflowResponse::History(history))) => {
                if let Some(run_id) = self.workflow_inspector.fill_history(history) {
                    self.inspect_workflow(run_id);
                }
            }
            (Intent::Inspect | Intent::History, Err(error)) => {
                debug!(%error, "workflow inspection could not be answered");
            }
            (Intent::Ack, Ok(WorkflowResponse::Acked(acked))) => {
                if !acked {
                    debug!("workflow completion moved on before its acknowledgement");
                }
            }
            (Intent::Ack, Err(error)) => {
                warn!(%error, "workflow completion could not be acknowledged");
            }
            (_, Err(error)) => self.flash(error.to_string()),
            (intent, Ok(response)) => {
                warn!(?intent, ?response, "unexpected workflow reply");
            }
        }
    }

    fn refresh_workflow_inspector(&mut self) {
        if !self.workflow_inspector.is_open() {
            return;
        }
        if let Some(run_id) = self
            .workflow_inspector
            .refresh(self.workflow.runs().to_vec())
        {
            self.inspect_workflow(run_id);
        }
    }

    /// Notices for the runs that settled since the last quiet turn, as the
    /// preamble of the one about to start.
    pub(crate) fn claim_workflow_completions(&mut self) -> Vec<Message> {
        self.workflow.claim_completions()
    }

    /// Runs working now, and runs waiting on someone.
    #[cfg(test)]
    pub(crate) fn workflow_counts(&self) -> (usize, usize) {
        (
            self.workflow.count(RunStatus::Active),
            self.workflow.count(RunStatus::Paused) + self.workflow.count(RunStatus::BudgetLimited),
        )
    }
}

impl RunControl {
    fn parse(word: &str) -> Option<Self> {
        [Self::Pause, Self::Resume, Self::Stop]
            .into_iter()
            .find(|control| control.verb() == word)
    }

    fn past_tense(self) -> &'static str {
        match self {
            Self::Pause => "Paused",
            Self::Resume => "Resumed",
            Self::Stop => "Stopped",
        }
    }
}

/// A run selector and the budget written after it, when the last word is a
/// number. A run named by a number alone is still a selector, because a
/// selector is the one argument resume cannot do without.
fn trailing_budget(target: &str) -> (&str, Option<u32>) {
    let Some((head, tail)) = target.rsplit_once(char::is_whitespace) else {
        return (target, None);
    };
    match tail.parse::<u32>() {
        Ok(budget) => (head.trim_end(), Some(budget)),
        Err(_) => (target, None),
    }
}

fn split_word(text: &str) -> (&str, &str) {
    text.split_once(char::is_whitespace).unwrap_or((text, ""))
}

/// `<name> [--agent-budget N] [rest]`. A JSON object after the name is the
/// script's arguments as given; anything else is its query and objective.
pub(crate) fn parse_launch(args: &str) -> Result<LaunchRequest, String> {
    let (name, rest) = split_word(args.trim());
    if name.is_empty() {
        return Err(WORKFLOW_USAGE.to_owned());
    }
    let mut rest = rest.trim();
    let mut agent_budget = None;
    if let Some(after_flag) = rest.strip_prefix(BUDGET_FLAG) {
        let (budget, tail) = split_word(after_flag.trim_start());
        let parsed = budget
            .parse::<u32>()
            .map_err(|_| format!("{BUDGET_FLAG} needs a positive number, got {budget:?}"))?;
        agent_budget = Some(parsed);
        rest = tail.trim();
    }
    let args = if rest.is_empty() {
        json!({})
    } else {
        match serde_json::from_str::<Value>(rest) {
            Ok(value) if value.is_object() => value,
            _ => json!({ QUERY_ARG: rest, OBJECTIVE_ARG: rest }),
        }
    };
    Ok(LaunchRequest {
        name: name.to_owned(),
        args,
        agent_budget,
    })
}

/// What the model is told when a run settles: the outcome first, then what
/// it produced, bounded so a long report cannot swamp the turn.
pub(crate) fn completion_notice(run: &RunSnapshot) -> String {
    let mut text = format!(
        "Workflow {} ({}) finished with status {}.",
        run.display_name, run.workflow_name, run.status
    );
    if let Some(result) = &run.result {
        match result.get(REPORT_FIELD).and_then(Value::as_str) {
            Some(report) => {
                text.push_str(REPORT_LABEL);
                text.push_str(&bounded(report));
            }
            None => {
                text.push_str(RESULT_LABEL);
                text.push_str(&bounded(&result.to_string()));
            }
        }
    }
    if let Some(path) = run.scratch_path() {
        text.push_str(SCRATCH_LABEL);
        text.push_str(path);
    }
    if let Some(message) = &run.pause_message {
        text.push_str(PAUSED_LABEL);
        text.push_str(message);
    }
    if let Some(error) = &run.error {
        text.push_str(ERROR_LABEL);
        text.push_str(error);
    }
    text
}

fn bounded(body: &str) -> Cow<'_, str> {
    if body.len() <= NOTICE_BODY_LIMIT {
        return Cow::Borrowed(body);
    }
    let end = body.floor_char_boundary(NOTICE_BODY_LIMIT);
    Cow::Owned(format!("{}{TRUNCATED_MARKER}", &body[..end]))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use caudra_agent::types::{WORKFLOW_EVENT_RUN_ID, WorkflowProvenance};
    use caudra_agent::{AgentEvent, Envelope, SubagentActivity, SubagentProgress};
    use caudra_workflow::{
        CatalogEntry, RunDetail, RunHistoryEntry, RunUsage, SourceKind, WorkflowCatalog,
    };
    use crossterm::event::{KeyCode, MouseEventKind};
    use test_case::test_case;

    use super::*;
    use crate::app::tests::{
        cancel_app, click_status, end_turn, mouse_event, status_hit, streaming_app, test_app,
    };
    use crate::app::{MISSING_TOOL_COMPLETION, Msg};
    use crate::components::command::ParsedCommand;
    use crate::components::key;
    use crate::components::keybindings::{key as kb, leader};
    use crate::components::status_bar::StatusBarHitTarget;
    use crate::components::{DisplayMessage, DisplayRole, ToolStatus, workflow_card};
    use caudra_agent::ToolOutput;

    const RUN_ID: &str = "run-1";
    const DISPLAY_NAME: &str = "deep-research-1";
    const REPORT: &str = "Findings: the answer is 42.";
    const REPORT_PATH: &str = "/tmp/scratch/report.md";
    const PAUSE_MESSAGE: &str = "waiting for a decision";
    const FAILURE: &str = "script raised";
    const ONE_ANNOUNCEMENT: &str = "a pending completion is announced exactly once";
    const ACK_AT_READ_REVISION: &str = "the ack must name the revision the notice was read at";
    const LOG_MESSAGE: &str = "searching the docs";
    const CARD_DRAWN: &str = "a slash launch draws the run's card in the transcript";
    const CARD_FOLLOWS: &str = "the card must follow the run's snapshots";
    const LOG_MIRRORED: &str = "a log line must reach the mirror's tail";
    const NO_CARD_CHURN: &str = "an agent's activity must not touch the transcript";
    const CARD_SURVIVES: &str = "a run that is still going keeps its card";
    const OTHER_RUN_ID: &str = "run-2";
    const OTHER_DISPLAY_NAME: &str = "deep-research-2";
    const NAME_PREFIX: &str = "deep-research-";
    const RAISED_BUDGET: u32 = 24;
    const AMBIGUITY_IS_NOT_A_GUESS: &str =
        "a selector several runs answer to controls none of them";

    pub(crate) fn run(status: RunStatus) -> RunSnapshot {
        RunSnapshot {
            run_id: RUN_ID.into(),
            display_name: DISPLAY_NAME.into(),
            workflow_name: DEEP_RESEARCH_WORKFLOW.into(),
            source_kind: SourceKind::Builtin,
            objective: None,
            status,
            pause_kind: None,
            pause_message: None,
            revision: 7,
            execution_epoch: 0,
            phase: None,
            phases: Vec::new(),
            phase_history: Vec::new(),
            agent_budget: 4,
            usage: RunUsage::default(),
            roster: Vec::new(),
            result: None,
            error: None,
            logs: Vec::new(),
            outbox_pending: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test_case("deep-research what is up", "deep-research", None, json!({"query": "what is up", "objective": "what is up"}); "free_text_becomes_query_and_objective")]
    #[test_case("review --agent-budget 3 {\"target\": \"src\"}", "review", Some(3), json!({"target": "src"}); "json_object_is_passed_through")]
    #[test_case("review --agent-budget 12", "review", Some(12), json!({}); "budget_alone_leaves_empty_args")]
    #[test_case("review [1, 2]", "review", None, json!({"query": "[1, 2]", "objective": "[1, 2]"}); "json_that_is_not_an_object_is_text")]
    #[test_case("review", "review", None, json!({}); "bare_name")]
    fn parse_launch_reads_name_budget_and_args(
        args: &str,
        name: &str,
        agent_budget: Option<u32>,
        expected: Value,
    ) {
        let launch = parse_launch(args).unwrap();
        assert_eq!(launch.name, name);
        assert_eq!(launch.agent_budget, agent_budget);
        assert_eq!(launch.args, expected);
    }

    #[test_case("" => WORKFLOW_USAGE.to_owned(); "empty")]
    #[test_case("review --agent-budget lots" => format!("{BUDGET_FLAG} needs a positive number, got \"lots\""); "non_numeric_budget")]
    fn parse_launch_rejects(args: &str) -> String {
        parse_launch(args).unwrap_err()
    }

    #[test]
    fn completed_notice_carries_the_report_and_its_path() {
        let mut run = run(RunStatus::Completed);
        run.result = Some(json!({ "report": REPORT, "path": REPORT_PATH, "status": "complete" }));

        let notice = completion_notice(&run);

        assert_eq!(
            notice,
            format!(
                "Workflow {DISPLAY_NAME} ({DEEP_RESEARCH_WORKFLOW}) finished with status completed.{REPORT_LABEL}{REPORT}{SCRATCH_LABEL}{REPORT_PATH}"
            )
        );
    }

    #[test]
    fn completed_notice_without_a_report_field_carries_the_json() {
        let mut run = run(RunStatus::Completed);
        run.result = Some(json!(["one", "two"]));

        let notice = completion_notice(&run);

        assert!(
            notice.ends_with(&format!("{RESULT_LABEL}[\"one\",\"two\"]")),
            "{notice}"
        );
    }

    #[test_case(RunStatus::Paused, Some(PAUSE_MESSAGE), None => format!("{PAUSED_LABEL}{PAUSE_MESSAGE}"); "paused_carries_the_pause_message")]
    #[test_case(RunStatus::Failed, None, Some(FAILURE) => format!("{ERROR_LABEL}{FAILURE}"); "failed_carries_the_error")]
    #[test_case(RunStatus::Cancelled, None, None => String::new(); "cancelled_is_the_status_alone")]
    fn notice_tail(status: RunStatus, pause: Option<&str>, error: Option<&str>) -> String {
        let mut run = run(status);
        run.pause_message = pause.map(str::to_owned);
        run.error = error.map(str::to_owned);

        let notice = completion_notice(&run);
        let head = format!(
            "Workflow {DISPLAY_NAME} ({DEEP_RESEARCH_WORKFLOW}) finished with status {status}."
        );
        notice.strip_prefix(&head).unwrap().to_owned()
    }

    #[test]
    fn a_long_report_is_bounded() {
        let mut run = run(RunStatus::Completed);
        run.result = Some(json!({ "report": "x".repeat(NOTICE_BODY_LIMIT * 2) }));

        let notice = completion_notice(&run);

        assert!(notice.ends_with(TRUNCATED_MARKER));
        assert!(notice.len() <= head_len() + NOTICE_BODY_LIMIT + TRUNCATED_MARKER.len());
    }

    fn head_len() -> usize {
        format!(
            "Workflow {DISPLAY_NAME} ({DEEP_RESEARCH_WORKFLOW}) finished with status completed.{REPORT_LABEL}"
        )
        .len()
    }

    #[test]
    fn claiming_completions_announces_once_and_acks_at_the_read_revision() {
        let mut ui = WorkflowUi::new();
        let mut done = run(RunStatus::Completed);
        done.outbox_pending = true;
        ui.apply(done);

        let first = ui.claim_completions();
        let second = ui.claim_completions();

        assert_eq!(first.len(), 1, "{ONE_ANNOUNCEMENT}");
        assert!(first[0].is_observation());
        assert!(second.is_empty(), "{ONE_ANNOUNCEMENT}");
        assert_eq!(
            ui.sent,
            vec![WorkflowRequest::AckCompletion {
                run_id: RUN_ID.into(),
                revision: 7,
            }],
            "{ACK_AT_READ_REVISION}"
        );
    }

    #[test]
    fn a_newer_snapshot_reopens_the_notice() {
        let mut ui = WorkflowUi::new();
        let mut paused = run(RunStatus::Paused);
        paused.outbox_pending = true;
        ui.apply(paused);
        assert_eq!(ui.claim_completions().len(), 1);

        let mut done = run(RunStatus::Completed);
        done.revision = 9;
        done.outbox_pending = true;
        ui.apply(done);

        assert_eq!(ui.claim_completions().len(), 1, "{ONE_ANNOUNCEMENT}");
        assert_eq!(ui.runs().len(), 1);
    }

    #[test]
    fn find_prefers_display_name_over_run_id() {
        let mut ui = WorkflowUi::new();
        let mut by_id = run(RunStatus::Active);
        by_id.run_id = DISPLAY_NAME.into();
        by_id.display_name = "other".into();
        ui.apply(by_id);
        ui.apply(run(RunStatus::Active));

        assert_eq!(ui.find(DISPLAY_NAME).unwrap().run_id, RUN_ID);
        assert_eq!(ui.find("other").unwrap().run_id, DISPLAY_NAME);
        assert!(ui.find("missing").is_none());
    }

    #[test]
    fn dispatch_without_a_runtime_sends_nothing() {
        let mut ui = WorkflowUi::new();

        assert!(!ui.dispatch(Intent::Catalog, WorkflowRequest::List));
        assert!(ui.poll().is_none());
    }

    fn scripted_app() -> App {
        let mut app = test_app();
        app.workflow.script();
        app
    }

    fn workflow_command(app: &mut App, name: &str, args: &str) -> Vec<Action> {
        app.execute_command(
            ParsedCommand {
                name: name.to_owned(),
                args: args.to_owned(),
            },
            0,
        )
    }

    /// What opening the inspector on one mirrored run ships: the earlier
    /// sessions once, then the selected run's detail.
    fn inspector_requests() -> Vec<WorkflowRequest> {
        vec![
            WorkflowRequest::History {
                limit: HISTORY_LIMIT,
            },
            WorkflowRequest::Inspect {
                run_id: RUN_ID.into(),
            },
        ]
    }

    fn snapshot_envelope(snapshot: RunSnapshot) -> Msg {
        workflow_envelope(WorkflowEvent::Snapshot(Box::new(snapshot)))
    }

    fn log_envelope(message: &str) -> Msg {
        workflow_envelope(WorkflowEvent::Log {
            run_id: RUN_ID.into(),
            revision: 7,
            at: 5,
            message: message.into(),
        })
    }

    fn workflow_envelope(event: WorkflowEvent) -> Msg {
        Msg::Agent(Box::new(Envelope {
            event: AgentEvent::Workflow(Box::new(event)),
            subagent: None,
            run_id: WORKFLOW_EVENT_RUN_ID,
            workflow: None,
        }))
    }

    /// The progress digest of an agent a workflow run launched, stamped with
    /// the run and call it belongs to.
    fn agent_progress_envelope() -> Msg {
        Msg::Agent(Box::new(Envelope {
            event: AgentEvent::SubagentProgress {
                progress: SubagentProgress {
                    activity: SubagentActivity::Responding,
                    tools: 1,
                    elapsed: Duration::ZERO,
                },
            },
            subagent: None,
            run_id: WORKFLOW_EVENT_RUN_ID,
            workflow: Some(WorkflowProvenance {
                run_id: RUN_ID.into(),
                epoch: 0,
                call_key: 1,
                phase: None,
            }),
        }))
    }

    /// A workflow agent has no task header, so its digest is addressed to the
    /// inspector's roster instead of to the transcript.
    #[test]
    fn a_workflow_agents_progress_reaches_the_inspector() {
        let mut app = scripted_app();
        app.update(agent_progress_envelope());

        assert_eq!(app.workflow_inspector.live_count(), 1);
        assert_eq!(app.main_chat().message_count(), 0, "{NO_CARD_CHURN}");
    }

    #[test_case("/workflow", "review" ; "workflow")]
    #[test_case("/deep-research", "what is up" ; "deep_research")]
    #[test_case("/workflows", "" ; "workflows")]
    fn without_a_runtime_every_workflow_command_says_so(name: &str, args: &str) {
        let mut app = test_app();

        workflow_command(&mut app, name, args);

        assert_eq!(app.status_bar.flash_text(), Some(UNAVAILABLE_MSG));
        assert!(app.workflow.sent.is_empty());
    }

    #[test]
    fn the_workflow_command_launches_with_query_and_budget() {
        let mut app = scripted_app();

        workflow_command(
            &mut app,
            "/workflow",
            "review --agent-budget 3 the auth module",
        );

        assert_eq!(
            app.workflow.sent,
            vec![WorkflowRequest::Start(LaunchRequest {
                name: "review".into(),
                args: json!({ "query": "the auth module", "objective": "the auth module" }),
                agent_budget: Some(3),
            })]
        );
    }

    #[test]
    fn deep_research_is_the_builtin_workflow_with_the_query() {
        let mut app = scripted_app();

        workflow_command(&mut app, "/deep-research", "why is the sky blue");

        assert!(matches!(
            &app.workflow.sent[..],
            [WorkflowRequest::Start(launch)]
                if launch.name == DEEP_RESEARCH_WORKFLOW
                    && launch.args[QUERY_ARG] == "why is the sky blue"
        ));
    }

    #[test]
    fn deep_research_without_a_query_shows_usage() {
        let mut app = scripted_app();

        workflow_command(&mut app, "/deep-research", "  ");

        assert_eq!(app.status_bar.flash_text(), Some(DEEP_RESEARCH_USAGE));
        assert!(app.workflow.sent.is_empty());
    }

    #[test_case("" ; "bare")]
    #[test_case("runs" ; "runs")]
    fn the_workflow_command_alone_opens_the_inspector(args: &str) {
        let mut app = scripted_app();
        app.workflow.apply(run(RunStatus::Active));

        workflow_command(&mut app, "/workflow", args);

        assert!(app.workflow_inspector.is_open());
        assert_eq!(app.workflow.sent, inspector_requests());
    }

    #[test]
    fn the_footer_chip_hovers_and_opens_the_inspector() {
        let mut app = scripted_app();
        app.workflow.apply(run(RunStatus::Active));
        let hit = status_hit(&mut app, StatusBarHitTarget::Workflows);
        app.update(mouse_event(MouseEventKind::Moved, hit.area.x, hit.area.y));
        assert_eq!(app.status_hover, Some(StatusBarHitTarget::Workflows));

        assert!(click_status(&mut app, StatusBarHitTarget::Workflows).is_empty());

        assert!(app.workflow_inspector.is_open());
        assert_eq!(app.status_hover, None);
        assert_eq!(app.workflow.sent, inspector_requests());
    }

    #[test]
    fn the_leader_key_opens_the_inspector() {
        let mut app = scripted_app();

        app.update(Msg::Key(kb::LEADER.to_key_event()));
        app.update(Msg::Key(leader::WORKFLOWS.to_key_event()));

        assert!(app.workflow_inspector.is_open());
        assert_eq!(
            app.workflow.sent,
            vec![WorkflowRequest::History {
                limit: HISTORY_LIMIT
            }]
        );
    }

    #[test]
    fn a_detail_reply_lands_in_the_inspector_and_history_lists_earlier_runs() {
        let mut app = scripted_app();
        app.workflow.apply(run(RunStatus::Active));
        workflow_command(&mut app, "/workflow", "");

        app.on_workflow_reply(Reply {
            intent: Intent::Inspect,
            result: Ok(WorkflowResponse::Detail(Box::new(RunDetail {
                run: run(RunStatus::Active),
                calls: Vec::new(),
                events: Vec::new(),
                journal_trimmed: false,
            }))),
        });
        app.on_workflow_reply(Reply {
            intent: Intent::History,
            result: Ok(WorkflowResponse::History(vec![RunHistoryEntry {
                run: run(RunStatus::Completed),
                session_id: "session-old".into(),
                session_title: "earlier".into(),
            }])),
        });

        assert_eq!(app.workflow_inspector.history_count(), 1);
        assert_eq!(app.workflow_inspector.selected(), Some(RUN_ID));
    }

    #[test_case("pause", RunStatus::Active, WorkflowRequest::Pause { run_id: RUN_ID.into() } ; "pause")]
    #[test_case("resume", RunStatus::Paused, WorkflowRequest::Resume { run_id: RUN_ID.into(), agent_budget: None } ; "resume")]
    #[test_case("stop", RunStatus::Active, WorkflowRequest::Stop { run_id: RUN_ID.into() } ; "stop")]
    fn run_controls_resolve_the_display_name(
        verb: &str,
        status: RunStatus,
        expected: WorkflowRequest,
    ) {
        let mut app = scripted_app();
        app.workflow.apply(run(status));

        workflow_command(&mut app, "/workflow", &format!("{verb} {DISPLAY_NAME}"));

        assert_eq!(app.workflow.sent, vec![expected]);
    }

    /// A second run whose display name shares the first one's prefix.
    fn sibling() -> RunSnapshot {
        let mut other = run(RunStatus::Active);
        other.run_id = OTHER_RUN_ID.into();
        other.display_name = OTHER_DISPLAY_NAME.into();
        other
    }

    #[test]
    fn a_prefix_names_the_only_run_it_matches() {
        let mut app = scripted_app();
        app.workflow.apply(run(RunStatus::Active));

        workflow_command(&mut app, "/workflow", &format!("stop {NAME_PREFIX}"));

        assert_eq!(
            app.workflow.sent,
            vec![WorkflowRequest::Stop {
                run_id: RUN_ID.into()
            }]
        );
    }

    #[test]
    fn a_prefix_several_runs_answer_to_is_a_choice() {
        let mut app = scripted_app();
        app.workflow.apply(run(RunStatus::Active));
        app.workflow.apply(sibling());

        workflow_command(&mut app, "/workflow", &format!("stop {NAME_PREFIX}"));

        let flash = app.status_bar.flash_text().unwrap_or_default().to_owned();
        assert!(flash.starts_with(AMBIGUOUS), "{flash}");
        assert!(flash.contains(DISPLAY_NAME), "{flash}");
        assert!(flash.contains(OTHER_DISPLAY_NAME), "{flash}");
        assert!(app.workflow.sent.is_empty(), "{AMBIGUITY_IS_NOT_A_GUESS}");
    }

    #[test]
    fn resume_takes_the_budget_written_after_the_run() {
        let mut app = scripted_app();
        app.workflow.apply(run(RunStatus::BudgetLimited));

        workflow_command(
            &mut app,
            "/workflow",
            &format!("resume {DISPLAY_NAME} {RAISED_BUDGET}"),
        );

        assert_eq!(
            app.workflow.sent,
            vec![WorkflowRequest::Resume {
                run_id: RUN_ID.into(),
                agent_budget: Some(RAISED_BUDGET),
            }]
        );
    }

    #[test_case(0 ; "zero")]
    #[test_case(MAX_AGENT_BUDGET + 1 ; "above_the_ceiling")]
    fn a_budget_the_runtime_would_refuse_is_refused_here(budget: u32) {
        let mut app = scripted_app();
        app.workflow.apply(run(RunStatus::BudgetLimited));

        workflow_command(
            &mut app,
            "/workflow",
            &format!("resume {DISPLAY_NAME} {budget}"),
        );

        assert_eq!(
            app.status_bar.flash_text(),
            Some(format!("{BUDGET_RANGE} {MAX_AGENT_BUDGET}").as_str())
        );
        assert!(app.workflow.sent.is_empty());
    }

    #[test]
    fn a_control_on_an_unknown_run_is_refused() {
        let mut app = scripted_app();

        workflow_command(&mut app, "/workflow", "stop nothing-here");

        assert_eq!(
            app.status_bar.flash_text(),
            Some("Unknown workflow run: nothing-here")
        );
        assert!(app.workflow.sent.is_empty());
    }

    #[test]
    fn workflows_opens_the_catalog_and_asks_for_it() {
        let mut app = scripted_app();

        workflow_command(&mut app, "/workflows", "");

        assert!(app.workflow_catalog_picker.is_open());
        assert_eq!(app.workflow.sent, vec![WorkflowRequest::List]);
    }

    #[test]
    fn choosing_a_trusted_workflow_prefills_the_composer() {
        let mut app = scripted_app();
        workflow_command(&mut app, "/workflows", "");
        app.on_workflow_reply(Reply {
            intent: Intent::Catalog,
            result: Ok(WorkflowResponse::Catalog(WorkflowCatalog {
                entries: vec![CatalogEntry {
                    name: "review".into(),
                    description: "Review a change".into(),
                    when_to_use: None,
                    phases: Vec::new(),
                    source_kind: SourceKind::Project,
                    path: None,
                    digest: "abc".into(),
                    trusted: true,
                    shadowed: Vec::new(),
                }],
                invalid: Vec::new(),
                ..WorkflowCatalog::default()
            })),
        });

        app.update(Msg::Key(key(KeyCode::Enter)));

        assert!(!app.workflow_catalog_picker.is_open());
        assert_eq!(app.input_box.buffer.value(), "/workflow review ");
    }

    #[test]
    fn confirming_trust_ships_the_digest_shown() {
        let mut app = scripted_app();
        workflow_command(&mut app, "/workflows", "");
        app.on_workflow_reply(Reply {
            intent: Intent::Catalog,
            result: Ok(WorkflowResponse::Catalog(WorkflowCatalog {
                entries: vec![CatalogEntry {
                    name: "review".into(),
                    description: String::new(),
                    when_to_use: None,
                    phases: Vec::new(),
                    source_kind: SourceKind::Project,
                    path: None,
                    digest: "abc".into(),
                    trusted: false,
                    shadowed: Vec::new(),
                }],
                invalid: Vec::new(),
                ..WorkflowCatalog::default()
            })),
        });

        app.update(Msg::Key(key(KeyCode::Enter)));
        app.update(Msg::Key(key(KeyCode::Enter)));

        assert_eq!(
            app.workflow.sent,
            vec![
                WorkflowRequest::List,
                WorkflowRequest::Trust {
                    name: "review".into(),
                    digest: "abc".into(),
                }
            ]
        );
    }

    #[test]
    fn a_started_run_is_announced_and_mirrored() {
        let mut app = scripted_app();

        app.on_workflow_reply(Reply {
            intent: Intent::Launch,
            result: Ok(WorkflowResponse::Started(Box::new(run(RunStatus::Active)))),
        });

        assert_eq!(
            app.status_bar.flash_text(),
            Some(format!("Started {DISPLAY_NAME}").as_str())
        );
        assert_eq!(app.workflow_counts(), (1, 0));
        let card = app.main_chat().message_at(0).unwrap().clone();
        assert!(
            matches!(&card.role, DisplayRole::Tool(tool) if tool.id == workflow_card::card_id(RUN_ID)),
            "{CARD_DRAWN}"
        );
        assert!(
            matches!(
                card.tool_output.as_deref(),
                Some(ToolOutput::WorkflowRun(_))
            ),
            "{CARD_DRAWN}"
        );
    }

    /// A launched run mid-turn, which is when both sweeps can reach its card.
    fn app_watching_a_run() -> App {
        let mut app = streaming_app();
        app.workflow.script();
        app.on_workflow_reply(Reply {
            intent: Intent::Launch,
            result: Ok(WorkflowResponse::Started(Box::new(run(RunStatus::Active)))),
        });
        app
    }

    fn drawn_card(app: &mut App) -> DisplayMessage {
        app.main_chat().message_at(0).unwrap().clone()
    }

    fn assert_card_is_live(card: &DisplayMessage) {
        assert!(
            matches!(
                card.tool_output.as_deref(),
                Some(ToolOutput::WorkflowRun(_))
            ),
            "{CARD_SURVIVES}"
        );
        assert!(
            matches!(&card.role, DisplayRole::Tool(tool) if tool.status == ToolStatus::InProgress),
            "{CARD_SURVIVES}"
        );
    }

    /// The card of a run that is still going is a live view, not a call the
    /// turn is waiting on: the sweep that ends a turn would otherwise stamp
    /// it as unfinished and replace the card with that error.
    #[test]
    fn a_live_run_card_survives_the_end_of_the_turn() {
        let mut app = app_watching_a_run();

        end_turn(&mut app);

        let card = drawn_card(&mut app);
        assert_card_is_live(&card);
        assert!(!card.text.contains(MISSING_TOOL_COMPLETION), "{card:?}");
    }

    /// Cancelling the turn leaves workflow runs alone, so the card of one
    /// still going must not be marked failed either.
    #[test]
    fn a_live_run_card_survives_a_cancelled_turn() {
        let mut app = app_watching_a_run();

        cancel_app(&mut app);

        assert_card_is_live(&drawn_card(&mut app));
    }

    #[test]
    fn snapshots_and_logs_move_the_card_along() {
        let mut app = scripted_app();
        app.on_workflow_reply(Reply {
            intent: Intent::Launch,
            result: Ok(WorkflowResponse::Started(Box::new(run(RunStatus::Active)))),
        });

        app.update(log_envelope(LOG_MESSAGE));
        assert_eq!(
            app.workflow.runs()[0]
                .logs
                .last()
                .map(|line| line.message.as_str()),
            Some(LOG_MESSAGE),
            "{LOG_MIRRORED}"
        );
        assert!(
            matches!(
                app.main_chat().message_at(0).unwrap().tool_output.as_deref(),
                Some(ToolOutput::WorkflowRun(drawn)) if drawn.logs.len() == 1
            ),
            "{CARD_FOLLOWS}"
        );

        let mut done = run(RunStatus::Completed);
        done.revision = 8;
        app.update(snapshot_envelope(done));

        let card = app.main_chat().message_at(0).unwrap().clone();
        let Some(ToolOutput::WorkflowRun(drawn)) = card.tool_output.as_deref() else {
            panic!("{CARD_DRAWN}");
        };
        assert_eq!(drawn.status, RunStatus::Completed, "{CARD_FOLLOWS}");
        assert!(
            matches!(&card.role, DisplayRole::Tool(tool) if tool.status == ToolStatus::Success),
            "{CARD_FOLLOWS}"
        );
    }

    #[test]
    fn a_log_for_an_unknown_run_is_ignored() {
        let mut app = scripted_app();

        app.update(log_envelope(LOG_MESSAGE));

        assert!(app.workflow.runs().is_empty());
        assert_eq!(app.main_chat().message_count(), 0);
    }

    #[test]
    fn an_untrusted_launch_points_at_the_catalog() {
        let mut app = scripted_app();

        app.on_workflow_reply(Reply {
            intent: Intent::Launch,
            result: Err(WorkflowError::TrustRequired {
                name: "review".into(),
                digest: "abc".into(),
                path: PathBuf::from("/project/.caudra/workflows/review.rhai"),
            }),
        });

        let flash = app.status_bar.flash_text().unwrap();
        assert!(flash.contains(TRUST_HINT), "{flash}");
    }

    #[test]
    fn snapshots_arrive_under_their_own_run_id() {
        let mut app = scripted_app();
        app.run_id = 42;
        let mut paused = run(RunStatus::Paused);
        paused.outbox_pending = true;

        app.update(snapshot_envelope(run(RunStatus::Active)));
        assert_eq!(app.workflow_counts(), (1, 0));

        app.update(snapshot_envelope(paused));
        assert_eq!(app.workflow_counts(), (0, 1));
        assert_eq!(app.claim_workflow_completions().len(), 1);
    }

    #[test]
    fn a_snapshot_refreshes_an_open_inspector() {
        let mut app = scripted_app();
        workflow_command(&mut app, "/workflow", "");

        app.update(snapshot_envelope(run(RunStatus::Active)));

        assert!(app.workflow_inspector.is_open());
        assert_eq!(app.workflow_inspector.run_count(), 1);
        assert_eq!(app.workflow_inspector.selected(), Some(RUN_ID));
    }

    #[test]
    fn replies_are_drained_on_tick() {
        let mut app = scripted_app();
        app.workflow.inject_reply(Reply {
            intent: Intent::Control(RunControl::Stop),
            result: Ok(WorkflowResponse::Run(Box::new(run(RunStatus::Cancelled)))),
        });

        let dirty = app.tick();

        assert_eq!(dirty, Dirty::YES);
        assert_eq!(
            app.status_bar.flash_text(),
            Some(format!("Stopped {DISPLAY_NAME}").as_str())
        );
    }
}
