//! The session's workflow runtime as the app sees it. Requests leave as
//! spawned tasks and answer through a reply channel the tick drains, so the
//! UI thread never waits on the runtime. The run list is a mirror of what the
//! runtime published, kept current by its events and by the replies that
//! carry a snapshot; the runtime's own read model stays the quiescence
//! authority in `AgentHandles`.

use std::borrow::Cow;
use std::collections::HashSet;

use caudra_agent::workflow::WorkflowHandle;
use caudra_config::Feature;
use caudra_providers::{Message, WorkflowEventOrigin};
use caudra_workflow::{
    LaunchRequest, LogLine, MAX_AGENT_BUDGET, MAX_RUN_LOG_ENTRIES, RunSnapshot, RunStatus,
    WorkflowError, WorkflowEvent, WorkflowRequest, WorkflowResponse,
};
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::app::App;
use crate::components::Action;
use crate::components::command::CommandAction;
use crate::components::document_view::COPIED_SELECTION;
use crate::components::logs_modal::LogsAction;
use crate::components::workflow_catalog_picker::WorkflowCatalogAction;
use crate::components::workflow_inspector::{InspectorAction, RunControl};
use crate::components::{DisplayMessage, DisplayRole};
use crate::repaint::Dirty;

pub(crate) const UNAVAILABLE_MSG: &str = "Workflows are unavailable in this session";
pub(crate) const WORKFLOW_USAGE: &str = "Usage: /workflow [runs | <name> [--agent-budget N] [args] | pause|stop <run> | resume <run> [budget]]";
const UNKNOWN_RUN: &str = "Unknown workflow run: ";
const AMBIGUOUS: &str = "Several runs match";
const BUDGET_RANGE: &str = "An agent budget must be between 1 and";
const MAX_CANDIDATES: usize = 5;
pub(crate) const TRUST_HINT: &str = "run /workflows to review and trust it";
pub(crate) const DEEP_RESEARCH_WORKFLOW: &str = "deep-research";
pub(crate) const REVIEW_CHANGES_WORKFLOW: &str = "review-changes";
pub(crate) const ROOT_CAUSE_WORKFLOW: &str = "root-cause";
/// What a built-in workflow wants after its own slash command, as the word the
/// usage line asks for.
const DEEP_RESEARCH_SUBJECT: &str = "query";
const REVIEW_CHANGES_SUBJECT: &str = "scope";
const ROOT_CAUSE_SUBJECT: &str = "failure";
const RUNS_SUBCOMMAND: &str = "runs";
const RETURN_HINT: &str = "/workflow returns to this run";
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
const INVALID_ACK: &str = "Workflow completion acknowledgment returned an unexpected response";

pub(crate) struct WorkflowUi {
    handle: Option<WorkflowHandle>,
    runs: Vec<RunSnapshot>,
    claimed: Vec<WorkflowEventOrigin>,
    ready: Vec<Message>,
    suppressed: HashSet<String>,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Intent {
    Catalog,
    Launch,
    Trust,
    Control(RunControl),
    Inspect,
    /// The run and call the bodies were asked about, so a reply that outlived
    /// its selection can be told apart from one that still applies.
    CallBodies {
        run_id: String,
        call_key: Option<u64>,
    },
    History,
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
            claimed: Vec::new(),
            ready: Vec::new(),
            suppressed: HashSet::new(),
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

    pub(super) fn runtime_handle(&self) -> Option<WorkflowHandle> {
        self.handle.clone()
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

    pub(super) fn delivery_snapshot(&self) -> WorkflowDelivery {
        let pending: Vec<_> = self
            .runs
            .iter()
            .filter(|run| {
                run.outbox_pending
                    && !self.suppressed.contains(&run.run_id)
                    && !self.claimed.iter().any(|origin| {
                        origin.run_id == run.run_id && origin.revision == run.revision
                    })
                    && !self.ready.iter().any(|message| {
                        message.workflow_event.as_ref().is_some_and(|origin| {
                            origin.run_id == run.run_id && origin.revision == run.revision
                        })
                    })
            })
            .map(|run| {
                (
                    WorkflowEventOrigin {
                        run_id: run.run_id.clone(),
                        revision: run.revision,
                    },
                    completion_notice(run),
                )
            })
            .collect();
        WorkflowDelivery {
            handle: self.handle.clone(),
            claims: self.claimed.clone(),
            pending,
            #[cfg(test)]
            scripted: self.scripted,
        }
    }

    pub(crate) fn claim_completions(&mut self) -> Result<Vec<Message>, String> {
        if self.handle.is_none() {
            self.ready.extend(
                self.delivery_snapshot()
                    .pending
                    .into_iter()
                    .map(|(origin, text)| Message::workflow_observation(text, origin)),
            );
        }
        let messages: Vec<_> = std::mem::take(&mut self.ready)
            .into_iter()
            .filter(|message| {
                message.workflow_event.as_ref().is_some_and(|origin| {
                    !self.suppressed.contains(&origin.run_id)
                        && self.runs.iter().any(|run| {
                            run.run_id == origin.run_id && run.revision == origin.revision
                        })
                })
            })
            .collect();
        for message in &messages {
            if let Some(origin) = &message.workflow_event {
                self.set_outbox(origin, false);
                self.claimed.push(origin.clone());
            }
        }
        Ok(messages)
    }

    pub(crate) fn has_claims(&self) -> bool {
        !self.claimed.is_empty()
    }

    pub(crate) fn has_delivery(&self) -> bool {
        self.has_claims()
            || !self.ready.is_empty()
            || self
                .runs
                .iter()
                .any(|run| run.outbox_pending && !self.suppressed.contains(&run.run_id))
    }

    pub(super) fn apply_delivery(&mut self, result: WorkflowDeliveryResult) {
        for origin in result.acked {
            #[cfg(test)]
            self.sent.push(WorkflowRequest::AckCompletion {
                run_id: origin.run_id.clone(),
                revision: origin.revision,
            });
            self.set_outbox(&origin, false);
            self.claimed.retain(|claim| claim != &origin);
        }
        for origin in result.released {
            self.set_outbox(&origin, true);
            self.claimed.retain(|claim| claim != &origin);
        }
        for message in result.ready {
            if let Some(origin) = &message.workflow_event
                && !self.suppressed.contains(&origin.run_id)
                && self
                    .runs
                    .iter()
                    .any(|run| run.run_id == origin.run_id && run.revision == origin.revision)
                && !self.claimed.contains(origin)
                && !self
                    .ready
                    .iter()
                    .any(|ready| ready.workflow_event == message.workflow_event)
            {
                self.set_outbox(origin, false);
                self.ready.push(message);
            }
        }
    }

    fn set_outbox(&mut self, origin: &WorkflowEventOrigin, pending: bool) {
        if let Some(run) = self
            .runs
            .iter_mut()
            .find(|run| run.run_id == origin.run_id && run.revision == origin.revision)
        {
            run.outbox_pending = pending;
        }
    }

    pub(crate) fn release_completions(&mut self) {
        for origin in std::mem::take(&mut self.claimed) {
            self.set_outbox(&origin, true);
        }
        for message in std::mem::take(&mut self.ready) {
            if let Some(origin) = message.workflow_event {
                self.set_outbox(&origin, true);
            }
        }
    }

    pub(crate) fn suppress_completions(&mut self) {
        self.ready.clear();
        self.suppressed
            .extend(self.runs.iter().map(|run| run.run_id.clone()));
    }

    pub(crate) fn stop_all(&mut self) -> Result<(), String> {
        self.suppress_completions();
        let Some(handle) = self.handle.clone() else {
            return Ok(());
        };
        for run in &handle.state().runs {
            self.suppressed.insert(run.run_id.clone());
            if matches!(
                run.status,
                RunStatus::Active | RunStatus::Paused | RunStatus::BudgetLimited
            ) {
                smol::block_on(handle.request(WorkflowRequest::Stop {
                    run_id: run.run_id.clone(),
                }))
                .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }

    fn poll(&self) -> Option<Reply> {
        self.reply_rx.try_recv().ok()
    }

    #[cfg(test)]
    pub(crate) fn inject_reply(&self, reply: Reply) {
        self.reply_tx.send(reply).unwrap();
    }
}

pub(super) struct WorkflowDelivery {
    handle: Option<WorkflowHandle>,
    pub claims: Vec<WorkflowEventOrigin>,
    pub pending: Vec<(WorkflowEventOrigin, String)>,
    #[cfg(test)]
    scripted: bool,
}

#[derive(Default)]
pub(super) struct WorkflowDeliveryResult {
    acked: Vec<WorkflowEventOrigin>,
    released: Vec<WorkflowEventOrigin>,
    ready: Vec<Message>,
}

impl WorkflowDelivery {
    pub(super) async fn run(&self, final_save: bool) -> Result<WorkflowDeliveryResult, String> {
        let mut result = WorkflowDeliveryResult::default();
        for origin in &self.claims {
            if self.received(origin).await? {
                self.ack(origin).await?;
                result.acked.push(origin.clone());
            } else if final_save {
                result.released.push(origin.clone());
            }
        }
        for (origin, notice) in &self.pending {
            if self.received(origin).await? {
                self.ack(origin).await?;
                result.acked.push(origin.clone());
            } else {
                result.ready.push(Message::workflow_observation(
                    notice.clone(),
                    origin.clone(),
                ));
            }
        }
        Ok(result)
    }

    async fn received(&self, origin: &WorkflowEventOrigin) -> Result<bool, String> {
        match &self.handle {
            Some(handle) => handle
                .received_completion(origin.clone())
                .await
                .map_err(|error| error.to_string()),
            None => Ok(false),
        }
    }

    async fn ack(&self, origin: &WorkflowEventOrigin) -> Result<(), String> {
        #[cfg(test)]
        if self.scripted {
            return Ok(());
        }
        let handle = self.handle.as_ref().ok_or(UNAVAILABLE_MSG)?;
        match handle
            .request(WorkflowRequest::AckCompletion {
                run_id: origin.run_id.clone(),
                revision: origin.revision,
            })
            .await
            .map_err(|error| error.to_string())?
        {
            WorkflowResponse::Acked(_) => Ok(()),
            _ => Err(INVALID_ACK.into()),
        }
    }
}

impl App {
    pub(super) fn workflows_browse(&mut self) -> Vec<Action> {
        if self.refuse_disabled(Feature::Workflows) {
            return Vec::new();
        }
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
        if self.refuse_disabled(Feature::Workflows) {
            return Vec::new();
        }
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
                self.rearm_background();
                self.workflow
                    .dispatch(Intent::Launch, WorkflowRequest::Start(launch));
            }
            Err(message) => self.flash(message),
        }
        Vec::new()
    }

    /// A built-in workflow's own slash command takes free text and nothing else,
    /// so it forwards to `/workflow <name> <text>` once the text is non-empty.
    pub(super) fn execute_builtin_workflow(&mut self, name: &str, text: &str) -> Vec<Action> {
        let text = text.trim();
        if text.is_empty() {
            self.flash(builtin_workflow_usage(name));
            return Vec::new();
        }
        self.execute_workflow(&format!("{name} {text}"))
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
        if control == RunControl::Resume {
            self.rearm_background();
            self.workflow.suppressed.remove(&run_id);
        }
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
        if self.refuse_disabled(Feature::Workflows) {
            return;
        }
        if !self.workflow.available() {
            self.flash(UNAVAILABLE_MSG.into());
            return;
        }
        let returning = self.workflow_return.take();
        let preferred = preferred.or(returning.as_deref());
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
                // A transcript is a chat, not an overlay, so the inspector has
                // to give up the screen. Remembering the run makes the trip a
                // round one: reopening lands back where the reader left.
                self.workflow_return = self.workflow_inspector.selected().map(str::to_owned);
                self.workflow_inspector.close();
                self.preview_task(&task_id);
                self.flash(RETURN_HINT.into());
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
            InspectorAction::LoadCallBody { run_id, call_key } => {
                self.workflow.dispatch(
                    Intent::CallBodies {
                        run_id: run_id.clone(),
                        call_key,
                    },
                    WorkflowRequest::CallBodies { run_id, call_key },
                );
            }
            InspectorAction::Copy { text, label } => {
                self.handle_logs_action(LogsAction::Copy { text, label });
            }
            InspectorAction::Cut { text, inspect } => {
                self.handle_logs_action(LogsAction::Copy {
                    text,
                    label: COPIED_SELECTION,
                });
                if let Some(run_id) = inspect {
                    self.inspect_workflow(run_id);
                }
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
            WorkflowCatalogAction::Copy(text) => {
                self.copy_to_clipboard(&text);
                Vec::new()
            }
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

    /// Puts this session's runs back on a transcript that has just been
    /// rebuilt. A card the `workflow` tool drew is stored with the tool result
    /// and comes back frozen at the moment of launch, so the mirror brings it
    /// up to date. A card a slash command drew was never stored, and the run
    /// outlived the transcript that showed it, so what it came to is kept as a
    /// notice rather than as a card no restore can rebuild.
    pub(crate) fn refresh_workflow_cards(&mut self) {
        let runs = self.workflow.runs().to_vec();
        for run in &runs {
            if !self.main_chat().workflow_card_update(run) {
                let notice = DisplayMessage::new(DisplayRole::Notice, outcome_headline(run));
                self.main_chat().push(notice);
            }
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
            (Intent::CallBodies { run_id, call_key }, Ok(WorkflowResponse::CallBodies(bodies))) => {
                if let Some(action) = self
                    .workflow_inspector
                    .fill_call_bodies(&run_id, call_key, bodies)
                {
                    let _ = self.handle_workflow_inspector_action(action);
                }
            }
            (Intent::History, Ok(WorkflowResponse::History(history))) => {
                if let Some(run_id) = self.workflow_inspector.fill_history(history) {
                    self.inspect_workflow(run_id);
                }
            }
            (Intent::Inspect | Intent::CallBodies { .. } | Intent::History, Err(error)) => {
                debug!(%error, "workflow inspection could not be answered");
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
    pub(crate) fn claim_workflow_completions(&mut self) -> Result<Vec<Message>, String> {
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

pub(crate) fn builtin_workflow_usage(name: &str) -> String {
    let subject = match name {
        REVIEW_CHANGES_WORKFLOW => REVIEW_CHANGES_SUBJECT,
        ROOT_CAUSE_WORKFLOW => ROOT_CAUSE_SUBJECT,
        _ => DEEP_RESEARCH_SUBJECT,
    };
    format!("Usage: /{name} <{subject}>")
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

/// The sentence a settled run is named and reported by, which the transcript
/// keeps on its own when the card that drew the run is gone.
pub(crate) fn outcome_headline(run: &RunSnapshot) -> String {
    format!(
        "Workflow {} ({}) finished with status {}.",
        run.display_name, run.workflow_name, run.status
    )
}

/// What the model is told when a run settles: the outcome first, then what
/// it produced, bounded so a long report cannot swamp the turn.
pub(crate) fn completion_notice(run: &RunSnapshot) -> String {
    let mut text = outcome_headline(run);
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
    use arc_swap::ArcSwap;
    use std::sync::Arc;
    use std::time::Duration;

    use caudra_agent::agent::task_runner::{TaskFuture, TaskRequest, TaskRunner};
    use caudra_agent::types::{WORKFLOW_EVENT_RUN_ID, WorkflowProvenance};
    use caudra_agent::workflow::{RuntimeDeps, WorkflowRuntime};
    use caudra_agent::{
        AgentEvent, AgentMode, CancelMap, CancelToken, Envelope, EventSender, HistorySnapshot,
        SubagentActivity, SubagentInfo, SubagentProgress,
    };
    use caudra_config::FeatureFlags;
    use caudra_workflow::{
        AgentRosterEntry, CatalogEntry, RosterState, RunDetail, RunHistoryEntry, RunUsage,
        SourceKind, WorkflowCatalog,
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
    use crate::components::{DisplayMessage, DisplayRole, Overlay, ToolStatus, workflow_card};
    use caudra_agent::ToolOutput;

    const RUN_ID: &str = "run-1";
    const UNEXPECTED_AGENT: &str = "Receipt fixture must not launch an agent";
    const COMPACTED: &str = "Compacted conversation without a workflow outcome.";
    const DISPLAY_NAME: &str = "deep-research-1";
    const REPORT: &str = "Findings: the answer is 42.";
    const REPORT_PATH: &str = "/tmp/scratch/report.md";
    const PAUSE_MESSAGE: &str = "waiting for a decision";
    const FAILURE: &str = "script raised";
    const ONE_ANNOUNCEMENT: &str = "a pending completion is announced exactly once";
    const ACK_AT_READ_REVISION: &str = "the ack must name the revision the notice was read at";
    const LOG_MESSAGE: &str = "searching the docs";
    const BUILTIN_SUBJECT: &str = "why is the sky blue";
    const AGENT_LABEL: &str = "change-surveyor";
    const AGENT_PHASE: &str = "Survey";
    const PERMISSION_ID: &str = "perm-1";
    const PERMISSION_COMMAND: &str = "git diff";
    const PROMPT_IS_ANSWERABLE: &str = "a workflow agent's permission request must reach the prompt, or the run hangs unanswerably";
    const AUTH_IS_REPORTED: &str =
        "a workflow agent's auth failure must be reported, not dropped with the run still waiting";
    const LAUNCHES_ITS_OWN: &str =
        "a built-in slash command launches the workflow it is named after, with its free text";
    const CARD_DRAWN: &str = "a slash launch draws the run's card in the transcript";
    const CARD_IS_BROUGHT_UP_TO_DATE: &str =
        "a restored card reads the state the runtime has, not the one it was stored at";
    const RUN_IS_NOT_LOST: &str = "a run whose card was never stored is still accounted for";
    const NOTICE_IS_NOT_A_SECOND_CARD: &str =
        "a run the transcript already draws is not announced again";
    const CARD_FOLLOWS: &str = "the card must follow the run's snapshots";
    const LOG_MIRRORED: &str = "a log line must reach the mirror's tail";
    const NO_CARD_CHURN: &str = "an agent's activity must not touch the transcript";
    const CARD_SURVIVES: &str = "a run that is still going keeps its card";
    const OTHER_RUN_ID: &str = "run-2";
    const TASK_ID: &str = "run-2:1";
    const TRANSCRIPT_TAKES_OVER: &str =
        "a transcript is a chat, so the inspector must give up the screen";
    const RETURNS_WHERE_IT_LEFT: &str =
        "reopening must land on the run the transcript was opened from";
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
            source_path: None,
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

    struct NoAgent;

    impl TaskRunner for NoAgent {
        fn run(&self, _: TaskRequest, _: CancelToken, _: EventSender) -> TaskFuture<'_> {
            Box::pin(async { panic!("{UNEXPECTED_AGENT}") })
        }
    }

    fn receipt_app() -> (App, WorkflowRuntime) {
        let mut app = test_app();
        app.storage_writer
            .save_sync(Arc::clone(&app.state.session))
            .unwrap();
        let runtime = receipt_runtime(&app);
        app.workflow.set_handle(Some(runtime.handle()));
        app.workflow.script();
        (app, runtime)
    }

    fn receipt_runtime(app: &App) -> WorkflowRuntime {
        smol::block_on(WorkflowRuntime::spawn(
            RuntimeDeps {
                state_dir: app.storage.clone(),
                session_id: app.state.session.id,
                cwd: app.state.session.cwd.clone().into(),
                user_config_dir: Some(app.storage.path().join("receipt-test-config")),
                remote_project_context: None,
                runner: Arc::new(NoAgent),
                events: flume::unbounded().0,
                mode: Arc::new(AgentMode::default),
                subagent_cancels: Arc::new(CancelMap::default()),
                features: FeatureFlags::NONE.with(Feature::Workflows),
            },
            None,
        ))
        .unwrap()
    }

    fn snapshot_messages(app: &mut App, messages: &[Message]) {
        app.shared_history = Some(Arc::new(ArcSwap::from_pointee(HistorySnapshot::new(
            crate::history_items(messages),
        ))));
        app.checkpoint_now();
    }

    fn claim(app: &mut App) -> Vec<Message> {
        smol::block_on(app.flush_background_delivery(false)).unwrap();
        app.claim_workflow_completions().unwrap()
    }

    #[test]
    fn claiming_completions_announces_once_and_acks_at_the_read_revision() {
        let (mut app, runtime) = receipt_app();
        let mut done = run(RunStatus::Completed);
        done.outbox_pending = true;
        app.workflow.apply(done);

        let first = claim(&mut app);
        let second = claim(&mut app);

        assert_eq!(first.len(), 1, "{ONE_ANNOUNCEMENT}");
        assert!(first[0].is_observation());
        assert!(second.is_empty(), "{ONE_ANNOUNCEMENT}");
        assert!(app.workflow.sent.is_empty(), "{ACK_AT_READ_REVISION}");
        smol::block_on(app.flush_background_delivery(false)).unwrap();
        assert!(app.workflow.sent.is_empty(), "{ACK_AT_READ_REVISION}");
        snapshot_messages(&mut app, &first);
        smol::block_on(app.flush_background_delivery(false)).unwrap();
        assert_eq!(
            app.workflow.sent,
            vec![WorkflowRequest::AckCompletion {
                run_id: RUN_ID.into(),
                revision: 7,
            }],
            "{ACK_AT_READ_REVISION}"
        );
        smol::block_on(runtime.shutdown());
    }

    #[test]
    fn a_newer_snapshot_reopens_the_notice() {
        let mut ui = WorkflowUi::new();
        let mut paused = run(RunStatus::Paused);
        paused.outbox_pending = true;
        ui.apply(paused);
        assert_eq!(ui.claim_completions().unwrap().len(), 1);

        let mut done = run(RunStatus::Completed);
        done.revision = 9;
        done.outbox_pending = true;
        ui.apply(done);

        assert_eq!(
            ui.claim_completions().unwrap().len(),
            1,
            "{ONE_ANNOUNCEMENT}"
        );
        assert_eq!(ui.runs().len(), 1);
    }

    #[test_case(false; "released_claim_is_retryable")]
    #[test_case(true; "stop_suppresses_old_claim")]
    fn rejected_delivery_never_acknowledges_a_workflow(stopped: bool) {
        let mut ui = WorkflowUi::new();
        let mut done = run(RunStatus::Completed);
        done.outbox_pending = true;
        ui.apply(done);
        assert_eq!(ui.claim_completions().unwrap().len(), 1);
        if stopped {
            ui.suppress_completions();
        }
        ui.release_completions();
        assert!(ui.sent.is_empty());
        assert_eq!(ui.claim_completions().unwrap().is_empty(), stopped);
    }

    #[test]
    fn a_completed_workflow_does_not_wait_for_its_active_sibling() {
        let mut app = streaming_app();
        let mut active = run(RunStatus::Active);
        active.run_id = OTHER_DISPLAY_NAME.into();
        app.workflow.apply(active);
        let mut done = run(RunStatus::Completed);
        done.outbox_pending = true;
        app.workflow.apply(done);
        end_turn(&mut app);
        let messages = claim(&mut app);
        let actions = app.start_mailbox_run(messages);
        assert!(
            matches!(actions.as_slice(), [Action::SendMessage(input)] if input.message.is_empty() && input.preamble.len() == 1)
        );
        assert!(app.workflow.sent.is_empty());
        assert_eq!(app.workflow.count(RunStatus::Active), 1);
    }

    #[test]
    fn workflow_claim_is_acknowledged_only_after_durable_parent_history() {
        let (mut app, runtime) = receipt_app();
        let mut done = run(RunStatus::Completed);
        done.outbox_pending = true;
        app.workflow.apply(done);
        let messages = claim(&mut app);
        app.shared_history = Some(Arc::new(ArcSwap::from_pointee(HistorySnapshot::new(
            crate::history_items(&messages),
        ))));
        app.checkpoint_now();
        assert!(app.workflow.sent.is_empty());
        smol::block_on(app.flush_background_delivery(true)).unwrap();
        assert!(
            matches!(app.workflow.sent.as_slice(), [WorkflowRequest::AckCompletion { run_id, .. }] if run_id == RUN_ID)
        );
        assert!(!app.workflow.has_claims());
        let saved = crate::load_app_session(app.state.session.id, &app.storage).unwrap();
        assert!(
            caudra_providers::project_messages(saved.messages())
                .unwrap()
                .iter()
                .any(|message| message.workflow_event == messages[0].workflow_event)
        );
        smol::block_on(runtime.shutdown());
    }

    #[test_case(false; "claim_saved_before_ack_and_compacted")]
    #[test_case(true; "receipt_saved_before_claim_and_compacted")]
    fn compacted_receipt_prevents_redelivery(saved_before_claim: bool) {
        let (mut app, runtime) = receipt_app();
        let mut done = run(RunStatus::Completed);
        done.outbox_pending = true;
        let origin = WorkflowEventOrigin {
            run_id: done.run_id.clone(),
            revision: done.revision,
        };
        let notice = completion_notice(&done);
        app.workflow.apply(done);
        let messages = if saved_before_claim {
            vec![Message::workflow_observation(notice, origin.clone())]
        } else {
            claim(&mut app)
        };
        snapshot_messages(&mut app, &messages);
        app.storage_writer
            .save_sync(Arc::clone(&app.state.session))
            .unwrap();
        app.shared_history = None;
        app.state.session_mut().meta.history_head = None;
        app.state
            .session_mut()
            .replace_messages(crate::history_items(&[Message::synthetic(
                COMPACTED.into(),
            )]));
        app.storage_writer
            .save_sync(Arc::clone(&app.state.session))
            .unwrap();
        if saved_before_claim {
            assert!(claim(&mut app).is_empty());
        } else {
            smol::block_on(app.flush_background_delivery(true)).unwrap();
        }
        assert!(!app.workflow.has_claims());
        assert!(!app.workflow.runs()[0].outbox_pending);
        assert_eq!(
            app.workflow.sent,
            vec![WorkflowRequest::AckCompletion {
                run_id: origin.run_id,
                revision: origin.revision
            }]
        );
        smol::block_on(runtime.shutdown());
    }

    #[test_case("text"; "legacy_text_does_not_ack")]
    #[test_case("revision"; "other_revision_does_not_ack")]
    #[test_case("run"; "other_run_does_not_ack")]
    fn identical_text_cannot_accept_a_workflow_claim(change: &str) {
        let (mut app, runtime) = receipt_app();
        let mut done = run(RunStatus::Completed);
        done.outbox_pending = true;
        app.workflow.apply(done);
        let mut messages = claim(&mut app);
        match change {
            "text" => messages[0].workflow_event = None,
            "revision" => messages[0].workflow_event.as_mut().unwrap().revision += 1,
            "run" => {
                messages[0].workflow_event.as_mut().unwrap().run_id = OTHER_DISPLAY_NAME.into()
            }
            _ => unreachable!(),
        }
        snapshot_messages(&mut app, &messages);
        smol::block_on(app.flush_background_delivery(false)).unwrap();
        assert!(app.workflow.has_claims());
        let saved_revision = app.background_saved_revision;
        smol::block_on(app.flush_background_delivery(true)).unwrap();
        assert_eq!(app.background_saved_revision, saved_revision);
        assert!(!app.workflow.has_claims());
        assert!(app.workflow.sent.is_empty());
        assert_eq!(claim(&mut app).len(), 1);
        smol::block_on(runtime.shutdown());
    }

    #[test]
    fn unchanged_history_retries_failed_workflow_reconciliation() {
        let (mut app, runtime) = receipt_app();
        let mut done = run(RunStatus::Completed);
        done.outbox_pending = true;
        app.workflow.apply(done);
        let messages = claim(&mut app);
        snapshot_messages(&mut app, &messages);
        smol::block_on(runtime.shutdown());
        assert!(smol::block_on(app.flush_background_delivery(false)).is_err());
        assert!(app.workflow.has_claims());
        let saved_revision = app.background_saved_revision;
        let runtime = receipt_runtime(&app);
        app.workflow.handle = Some(runtime.handle());
        smol::block_on(app.flush_background_delivery(true)).unwrap();
        assert_eq!(app.background_saved_revision, saved_revision);
        assert!(!app.workflow.has_claims());
        assert_eq!(app.workflow.sent.len(), 1);
        smol::block_on(runtime.shutdown());
    }

    #[test]
    fn stale_worker_failure_after_stop_is_not_reported() {
        let (mut app, runtime) = receipt_app();
        let mut done = run(RunStatus::Completed);
        done.outbox_pending = true;
        app.workflow.apply(done);
        smol::block_on(runtime.shutdown());
        app.persist_background_delivery(false).unwrap();
        let reply = smol::block_on(app.background_delivery.replies.recv_async()).unwrap();
        app.handle_cancel();
        assert!(app.apply_delivery_reply(reply).is_ok());
        assert!(!app.background_delivery.pending());
    }

    #[test]
    fn saved_old_revision_cannot_clear_a_newer_pending_completion() {
        let (mut app, runtime) = receipt_app();
        let mut paused = run(RunStatus::Paused);
        paused.outbox_pending = true;
        app.workflow.apply(paused);
        let messages = claim(&mut app);
        let mut done = run(RunStatus::Completed);
        done.revision += 1;
        done.outbox_pending = true;
        let latest = done.revision;
        app.workflow.apply(done);
        snapshot_messages(&mut app, &messages);
        smol::block_on(app.flush_background_delivery(true)).unwrap();
        assert!(app.workflow.ready.iter().any(|message| {
            message
                .workflow_event
                .as_ref()
                .is_some_and(|origin| origin.revision == latest)
        }));
        let latest_messages = claim(&mut app);
        assert_eq!(
            latest_messages[0].workflow_event.as_ref().unwrap().revision,
            latest
        );
        smol::block_on(runtime.shutdown());
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
            task: None,
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
            task: None,
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

    /// A workflow agent reports under the run's id rather than the turn's, so
    /// the stale-run filter used to drop everything it sent. An interaction
    /// nobody can answer is a run that hangs until it is stopped.
    fn workflow_agent_envelope(event: AgentEvent) -> Msg {
        Msg::Agent(Box::new(Envelope {
            task: None,
            event,
            subagent: Some(SubagentInfo {
                parent_tool_use_id: TASK_ID.into(),
                task_id: TASK_ID.into(),
                name: AGENT_LABEL.into(),
                prompt: None,
                model: None,
                thinking: None,
                fast: false,
                answer_tx: None,
                steer_tx: None,
            }),
            run_id: WORKFLOW_EVENT_RUN_ID,
            workflow: Some(WorkflowProvenance {
                run_id: RUN_ID.into(),
                epoch: 0,
                call_key: 1,
                phase: Some(AGENT_PHASE.into()),
            }),
        }))
    }

    #[test]
    fn a_workflow_agents_permission_request_reaches_the_prompt() {
        let mut app = scripted_app();

        app.update(workflow_agent_envelope(
            crate::app::tests::permission_event(PERMISSION_ID, PERMISSION_COMMAND),
        ));

        assert!(app.permission_prompt.is_open(), "{PROMPT_IS_ANSWERABLE}");
        assert_eq!(app.main_chat().message_count(), 0, "{NO_CARD_CHURN}");
    }

    /// The task id a subtask prompt shows is `<run>:<key>`, which names nothing
    /// a reader recognises. The phase and the label are what the inspector uses.
    #[test]
    fn the_prompt_names_the_phase_and_the_agent() {
        let mut app = scripted_app();

        app.update(workflow_agent_envelope(
            crate::app::tests::permission_event(PERMISSION_ID, PERMISSION_COMMAND),
        ));

        let requester = app.permission_prompt.requester().expect("a requester");
        assert!(requester.contains(AGENT_PHASE), "{requester}");
        assert!(requester.contains(AGENT_LABEL), "{requester}");
        assert!(!requester.contains(TASK_ID), "{requester}");
    }

    #[test]
    fn a_workflow_agents_auth_failure_is_not_swallowed() {
        let mut app = scripted_app();

        app.update(workflow_agent_envelope(AgentEvent::AuthRequired));

        assert_eq!(
            app.status_bar.flash_text(),
            Some(crate::app::WORKFLOW_AUTH_REQUIRED),
            "{AUTH_IS_REPORTED}"
        );
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

    #[test_case(DEEP_RESEARCH_WORKFLOW ; "deep_research")]
    #[test_case(REVIEW_CHANGES_WORKFLOW ; "review_changes")]
    #[test_case(ROOT_CAUSE_WORKFLOW ; "root_cause")]
    fn a_builtin_slash_command_launches_its_own_workflow(name: &str) {
        let mut app = scripted_app();

        workflow_command(&mut app, &format!("/{name}"), BUILTIN_SUBJECT);

        assert!(
            matches!(
                &app.workflow.sent[..],
                [WorkflowRequest::Start(launch)]
                    if launch.name == name && launch.args[QUERY_ARG] == BUILTIN_SUBJECT
            ),
            "{LAUNCHES_ITS_OWN}"
        );
    }

    /// The usage line names what that workflow wants, so a user who typed the
    /// command bare is told what to put after it rather than the word "query".
    #[test_case(DEEP_RESEARCH_WORKFLOW, "Usage: /deep-research <query>" ; "deep_research")]
    #[test_case(REVIEW_CHANGES_WORKFLOW, "Usage: /review-changes <scope>" ; "review_changes")]
    #[test_case(ROOT_CAUSE_WORKFLOW, "Usage: /root-cause <failure>" ; "root_cause")]
    fn a_builtin_slash_command_without_a_subject_shows_its_own_usage(name: &str, usage: &str) {
        let mut app = scripted_app();

        workflow_command(&mut app, &format!("/{name}"), "  ");

        assert_eq!(app.status_bar.flash_text(), Some(usage));
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

    /// A workflow agent with no chat still counts on the tasks chip, so the
    /// picker the chip opens lists it, and choosing it opens its run instead
    /// of a transcript that is not there.
    #[test_case(RosterState::Running, 1; "running_agent")]
    #[test_case(RosterState::Completed, 0; "finished_agent")]
    fn a_roster_agent_without_a_chat_is_a_task_that_opens_its_run(
        state: RosterState,
        active: usize,
    ) {
        let mut app = scripted_app();
        let mut active_run = run(RunStatus::Active);
        active_run.roster.push(AgentRosterEntry {
            call_key: 1,
            label: AGENT_LABEL.into(),
            phase: None,
            task_id: Some(TASK_ID.into()),
            state,
            tokens_used: 0,
            duration_ms: 0,
        });
        app.workflow.apply(active_run);
        assert_eq!(app.task_activity().agents, active);
        assert!(app.task_hint_text().is_some());

        workflow_command(&mut app, "/tasks", "");
        assert!(app.task_picker.select(TASK_ID));
        app.update(Msg::Key(key(KeyCode::Enter)));

        assert!(!app.task_picker.is_open());
        assert_eq!(app.active_chat, 0);
        assert_eq!(app.chats.len(), 1);
        assert!(app.workflow_inspector.is_open());
        assert_eq!(app.workflow.sent, inspector_requests());
    }

    /// A transcript is a chat, so the inspector has to close. Reopening must
    /// land back on the run the reader left, not on whichever is newest.
    #[test]
    fn reopening_returns_to_the_run_a_transcript_was_opened_from() {
        let mut app = scripted_app();
        app.workflow.apply(run(RunStatus::Active));
        let mut older = run(RunStatus::Completed);
        older.run_id = OTHER_RUN_ID.into();
        app.workflow.apply(older);
        app.open_workflow_inspector(Some(OTHER_RUN_ID));

        let _ = app
            .handle_workflow_inspector_action(InspectorAction::OpenTranscript(TASK_ID.to_owned()));
        assert!(!app.workflow_inspector.is_open(), "{TRANSCRIPT_TAKES_OVER}");
        app.open_workflow_inspector(None);

        assert_eq!(
            app.workflow_inspector.selected(),
            Some(OTHER_RUN_ID),
            "{RETURNS_WHERE_IT_LEFT}"
        );
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

    /// A card is stored with the tool result that drew it, which freezes it at
    /// the moment of launch. The runtime outlives the process, so a restored
    /// transcript reads the run from the mirror rather than from the snapshot
    /// it was written with.
    #[test]
    fn a_restored_card_is_brought_up_to_date_from_the_runtime() {
        let mut app = app_watching_a_run();
        app.workflow.apply(run(RunStatus::Completed));

        app.refresh_workflow_cards();

        let card = drawn_card(&mut app);
        assert!(
            matches!(&card.role, DisplayRole::Tool(tool) if tool.status == ToolStatus::Success),
            "{CARD_IS_BROUGHT_UP_TO_DATE}"
        );
    }

    /// A slash launch draws a card the session never stored, so a resume has
    /// nothing to rebuild it from. The run itself is durable, so the outcome
    /// is kept as a notice rather than dropped.
    #[test]
    fn a_run_whose_card_was_never_stored_comes_back_as_a_notice() {
        let mut app = scripted_app();
        app.workflow.apply(run(RunStatus::Completed));

        app.refresh_workflow_cards();

        let notice = drawn_card(&mut app);
        assert_eq!(notice.role, DisplayRole::Notice, "{RUN_IS_NOT_LOST}");
        assert_eq!(
            notice.text,
            outcome_headline(&run(RunStatus::Completed)),
            "{RUN_IS_NOT_LOST}"
        );
    }

    #[test]
    fn a_run_the_transcript_still_draws_is_not_announced_again() {
        let mut app = app_watching_a_run();
        app.workflow.apply(run(RunStatus::Completed));

        app.refresh_workflow_cards();

        assert!(
            app.main_chat().message_at(1).is_none(),
            "{NOTICE_IS_NOT_A_SECOND_CARD}"
        );
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
                path: "/project/.caudra/workflows/review.rhai".into(),
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
        assert_eq!(claim(&mut app).len(), 1);
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
