//! The examples in `tests/examples/` as acceptance tests. Each validates, and each replays a
//! scripted event sequence against a fake host, which pins the exact actions while the test
//! tracks the committed state. Recorded firings also replay from their journals.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

use caudra_automation::args::resolve;
use caudra_automation::engine::{
    ErrorKind, Firing, FiringEnd, FiringError, FiringLimits, FiringOutcome, run_firing,
};
use caudra_automation::event::{
    Admission, ArmedReason, Audience, Delivery, ErrorKind as ProviderError, Event, EventDetail,
    GoalFinishedDetail, GoalVerdict, GoalView, HeldWork, IdleDetail, InputKind, MessageDetail,
    PauseReason, SenderKind, SessionStatus, SessionView, StartedBy, TurnOutcome,
    WorkFinishedDetail, WorkOutcome, WorkReport, WorkState, WorkView, WorkflowFinishedDetail,
    WorkflowStatus,
};
use caudra_automation::host::{
    ActionKind, ActionReply, ActionRequest, AutomationHost, CallSite, Failure, FailureKind,
    GoalSet, HostError, HostResult, HttpResponse, Interruption, PublishReceipt, SendReceipt,
    SendStatus, WorkflowStarted, request_hash,
};
use caudra_automation::limits::{LimitReason, LimitRefusal};
use caudra_automation::matcher::{TopicMatcher, first_match};
use caudra_automation::meta::{AutomationMeta, Trigger, parse_meta};
use caudra_automation::replay::{
    Answer, DryRun, DryRunReport, JournalEntry, JournalResult, dry_run,
};
use caudra_automation::untrusted::{UNTRUSTED_TAG, Untrusted};
use caudra_automation::validate::validate;
use jiff::civil::{Date, date};
use jiff::tz::TimeZone;
use serde_json::{Map, Value, json};
use test_case::test_case;

const EXAMPLES_DIR: &str = "tests/examples";
const EXAMPLE_EXTENSION: &str = "rhai";
const CI_TRIAGE: &str = "ci-triage";
const CI_WATCH: &str = "ci-watch";
const GOAL_CHAIN: &str = "goal-chain";
const GOAL_WEBHOOK: &str = "goal-webhook";
const JOIN_SWARM: &str = "join-swarm";
const KEEP_GOING: &str = "keep-going";
const NIGHTLY_REVIEW: &str = "nightly-review";
const PAGE_ME: &str = "page-me";
const RESEARCH_DESK: &str = "research-desk";
const RETRY_OVERLOAD: &str = "retry-overload";
const SPEND_GUARD: &str = "spend-guard";
const STANDUP: &str = "standup";
const STATUS_BEACON: &str = "status-beacon";
const STATUS_DESK: &str = "status-desk";
const TASK_TRACKER: &str = "task-tracker";
const TIMEBOX: &str = "timebox";
const WORK_NUDGE: &str = "work-nudge";
const EXAMPLES: [&str; 17] = [
    CI_TRIAGE,
    CI_WATCH,
    GOAL_CHAIN,
    GOAL_WEBHOOK,
    JOIN_SWARM,
    KEEP_GOING,
    NIGHTLY_REVIEW,
    PAGE_ME,
    RESEARCH_DESK,
    RETRY_OVERLOAD,
    SPEND_GUARD,
    STANDUP,
    STATUS_BEACON,
    STATUS_DESK,
    TASK_TRACKER,
    TIMEBOX,
    WORK_NUDGE,
];

/// 2026-10-05 08:30 UTC, a Monday.
const NOW_MS: i64 = 1_791_189_000_000;
const AT: i64 = 1_791_189_000;
const MONDAY: Date = date(2026, 10, 5);
const SATURDAY: Date = date(2026, 10, 10);
const EARLY_HOUR: i8 = 7;
const WORK_HOUR: i8 = 10;
const EVENING_HOUR: i8 = 20;
const SECONDS_PER_MINUTE: i64 = 60;
const SHORT_TURN_MINUTES: i64 = 30;
const LONG_TURN_MINUTES: i64 = 65;
const LIMITED_UNTIL: i64 = NOW_MS + 3_600_000;

const FIRE_ID: &str = "fire-1";
const SESSION_ID: &str = "session-1";
const SESSION_NAME: &str = "@builder";
const MODE: &str = "build";
const TITLE: &str = "Fix the login flow";
const IDLE: &str = "idle";
const LAST_RESPONSE: &str = "Fixed the session cookie. The login tests pass.";
const RUNS: u32 = 1;
const BUSY_S: u64 = 300;
const OK_STATUS: u16 = 200;
const SERVER_ERROR_STATUS: u16 = 500;
const BAD_GATEWAY_STATUS: u16 = 502;
const HTTP_TIMEOUT: &str = "30s";
const MESSAGE_ID: &str = "message-1";
const RECEIPT_ID: &str = "message-9";
const RUN_ID: &str = "run-1";
const SECOND_RUN_ID: &str = "run-2";
const OTHER_RUN_ID: &str = "run-3";

const GOAL_A: &str = "The login tests pass";
const GOAL_B: &str = "The signup tests pass";
const GOAL_C: &str = "The changelog covers both fixes";
const OTHER_GOAL: &str = "The flaky CI job is fixed";
const GOAL_REASON: &str = "The tests ran green twice";
const CONTINUATION_LIMIT: u32 = 24;
const EVALUATIONS: u32 = 2;
const GOAL_DURATION_S: u64 = 1_800;
const GOAL_ACTIVE: &str = "another goal is active";
const CHAIN_IMPOSSIBLE: &str = "goal-chain stopped: verdict impossible";
const CHAIN_CLEARED: &str = "goal-chain stopped: verdict cleared";
const CHAIN_DONE: &str = "goal-chain: all 3 goals are met";

const BACKLOG_PROMPT: &str =
    "Continue with the next unchecked item in TODO.md. When none remain, reply with BACKLOG EMPTY.";
const CUSTOM_BACKLOG: &str = "BACKLOG.md";
const CUSTOM_BACKLOG_PROMPT: &str = "Continue with the next unchecked item in BACKLOG.md. When none remain, reply with BACKLOG EMPTY.";
const BACKLOG_EMPTY_RESPONSE: &str = "Every item is checked. BACKLOG EMPTY";
const OUTSIDE_WORK_HOURS: &str = "outside work hours";
const LAST_TURN_ERRORED: &str = "the last turn ended error";
const BACKLOG_EMPTY: &str = "the backlog is empty";

const RUNS_URL: &str = "https://api.github.com/repos/acme/app/actions/runs";
const RUN_PAGE: &str = "https://github.com/acme/app/actions/runs/";
const RUN_TITLE: &str = "Fix the flaky login test";
const FAILURE: &str = "failure";
const SUCCESS: &str = "success";
const FIRST_RUN: i64 = 42;
const SECOND_RUN: i64 = 43;
const THIRD_RUN: i64 = 44;
const CI_FAILURES: &str = "ci.failures";
const GITHUB_UNAVAILABLE: &str = "GitHub returned 502";
const HISTORY_UNAVAILABLE: &str = "the history could not record the publication";
const CI_PUBLISH_LINE: u32 = 21;
const CI_PUBLISH_COLUMN: u32 = 5;

const HOOK_URL: &str = "https://hooks.example.com/caudra";
const BLOCKERS_PROMPT: &str =
    "The goal was judged impossible. Record what blocked it in BLOCKERS.md.";
const TIMED_OUT: &str = "the request timed out after 30s";

const SWARM_STATUS: &str = "swarm.status";
const ONLINE: &str = "@builder is online";
const ARMED_BY_HAND: &str = "armed by hand";
const IDLE_ANNOUNCEMENT: &str = "@builder is idle";
const HOLDING: &str = "working on swarm-tasks/fix-login";
const HOLDING_ANNOUNCEMENT: &str = "@builder is working on swarm-tasks/fix-login";
const BEACON_PUBLISH_LINE: u32 = 12;
const BEACON_PUBLISH_COLUMN: u32 = 1;

const REVIEW_CHANGES: &str = "review-changes";
const REVIEW_SCOPE: &str = "commits from the last 24 hours on main";
const REVIEW_REPORT: &str = "No regressions in the last 24 hours";
const REVIEW_URL_ENV: &str = "SLACK_REVIEW_URL";
const NIGHTLY_POST: &str = "Nightly review completed\n\nNo regressions in the last 24 hours";

const SHELL_TOOL: &str = "shell";
const APPROVE_SHELL: &str = "approve shell";
const REVIEW_PLAN: &str = "review a plan";
const SIGN_IN: &str = "sign in again";
const ANSWER_QUESTION: &str = "answer a question";
const WAITING_S: u64 = 600;

const DEEP_RESEARCH: &str = "deep-research";
const LEAD: &str = "@lead";
const PEER_TITLE: &str = "Plan the release";
const RESEARCH_REQUEST: &str = "research: why do the login tests flake?";
const RESEARCH_QUERY: &str = " why do the login tests flake?";
const GREETING: &str = "Good morning";
const NOT_RESEARCH: &str = "not a research request";
const ACKNOWLEDGED: &str = "Started deep-research. The report follows when it finishes.";
const SENDER_LEFT: &str = "@lead closed its session";
const ACK_FAILED: &str = "Could not acknowledge the request: @lead closed its session";
const RESEARCH_REPORT: &str = "The tests share one temp directory.";
const REPORT_SENT: &str =
    "Report from deep-research (completed)\n\nThe tests share one temp directory.";
const AGENTS: u32 = 4;
const TOKENS: u64 = 120_000;

const RESUME_PROMPT: &str =
    "The previous turn stopped on a provider error. Continue where you left off.";

const UNDER_BUDGET: f64 = 12.5;
const OVER_BUDGET: f64 = 20.5;
const SPEND_PAUSE: &str = "spend-guard: this session passed $20";
const SPEND_NOTICE: &str = "Automations paused: this session has spent over $20.";

const STANDUP_PROMPT: &str = "Summarize yesterday's commits in this repository as three standup bullets. Reply with only the bullets.";
const STANDUP_URL_ENV: &str = "SLACK_STANDUP_URL";
const BULLETS: &str = "- Fixed the login flow\n- Reviewed the signup change\n- Next: release notes";

const STATUS_QUESTION: &str = "status?";
const PADDED_STATUS_QUESTION: &str = " Status\n";
const IDLE_STATUS: &str = "@builder (Fix the login flow) is idle. Goal: none.";
const WORKING_STATUS: &str =
    "@builder (Fix the login flow) is working. Goal: The login tests pass.";
const NOT_STATUS: &str = "not a status question";
const SCRIPT_SENDER: &str = "a script cannot take a reply";
const STATUS_REPLY_LINE: u32 = 14;
const STATUS_REPLY_COLUMN: u32 = 1;

const SWARM_TASKS: &str = "swarm-tasks";
const SWARM_TASKS_TOPIC: &str = "swarm.tasks";
const WORKER: &str = "@worker-1";
const FIX_LOGIN: &str = "fix-login";
const FIX_SIGNUP: &str = "fix-signup";
const FIRST_ATTEMPT: u32 = 1;
const MAX_ATTEMPTS: u32 = 3;
const TASK_DETAIL: &str = "The login tests pass again";
const TASK_DONE_PROMPT: &str = "A swarm task finished. Check its result, then publish follow-up tasks to swarm.tasks if any remain.";
const TASK_FAILED: &str = "Swarm task fix-login is failed after 3 attempts";
const TASK_PAUSED: &str = "Swarm task fix-login is paused after 1 attempts";
const NUDGE_LOGIN: &str = "Work item fix-login in group swarm-tasks paused because your turn ended without an outcome. Finish it, then report with work_assignment.";
const NUDGE_SIGNUP: &str = "Work item fix-signup in group swarm-tasks paused because your turn ended without an outcome. Finish it, then report with work_assignment.";
const PAUSED_AGAIN: &str = "Work item fix-login in swarm-tasks paused again and needs you";

const STILL_WORKING: &str = "Still working after 65 minutes";
const WRAP_UP_PROMPT: &str = "You have worked on this for an hour. Finish the current step, then summarize progress and what remains.";

const ROOT_CAUSE: &str = "root-cause";
const CI_WATCHER: &str = "@ci-watcher";
const NIGHTLY_CI: &str = "nightly-ci";
const STRANGER: &str = "@intern";
const AGENT_BUDGET: u32 = 24;

const MUST_READ: &str = "the example is readable";
const MUST_LIST: &str = "the examples directory is listable";
const MUST_PARSE: &str = "the example's header parses";
const MUST_RESOLVE: &str = "the args resolve";
const MUST_EXIST: &str = "the local time exists";
const UNROUTED: &str = "no trigger of the automation routes this event";
const MISNAMED: &str = "meta.name differs from the file name:";
const NOT_HTTP: &str = "an http request answers with a response or a failure";

type Fired = (FiringEnd, Vec<Value>);

/// Answers each request with the first scripted answer of its kind, or else a success, and
/// records both.
struct ScriptedHost {
    now_ms: i64,
    admission: Result<(), LimitRefusal>,
    answers: RefCell<Vec<(ActionKind, HostResult<ActionReply>)>>,
    calls: RefCell<Vec<(ActionRequest, HostResult<ActionReply>)>>,
}

/// One example armed in one session.
struct Automation {
    source: String,
    meta: AutomationMeta,
    args: Map<String, Value>,
    /// The committed state, as tagged JSON.
    state: Value,
}

/// A row of goal-chain's step-by-step table.
struct Row {
    what: &'static str,
    event: Event,
    host: ScriptedHost,
    fired: Fired,
    state: Value,
}

struct ExactTopics;

impl TopicMatcher for ExactTopics {
    fn matches(&self, pattern: &str, topic: &str) -> bool {
        pattern == topic
    }
}

impl AutomationHost for ScriptedHost {
    fn now_ms(&self) -> i64 {
        self.now_ms
    }

    fn interrupted(&self) -> Option<Interruption> {
        None
    }

    fn admit(&self) -> Result<(), LimitRefusal> {
        self.admission.clone()
    }

    fn act(&self, _site: CallSite, request: ActionRequest) -> HostResult<ActionReply> {
        let mut answers = self.answers.borrow_mut();
        let answer = match answers.iter().position(|(kind, _)| *kind == request.kind()) {
            Some(index) => answers.remove(index).1,
            None => Ok(success(&request)),
        };
        self.calls.borrow_mut().push((request, answer.clone()));
        answer
    }
}

impl Default for ScriptedHost {
    fn default() -> Self {
        Self::at(NOW_MS)
    }
}

impl ScriptedHost {
    fn at(now_ms: i64) -> Self {
        Self {
            now_ms,
            admission: Ok(()),
            answers: RefCell::default(),
            calls: RefCell::default(),
        }
    }

    fn answering(self, kind: ActionKind, answer: HostResult<ActionReply>) -> Self {
        self.answers.borrow_mut().push((kind, answer));
        self
    }

    fn failing(self, kind: ActionKind, failure: FailureKind, message: &str) -> Self {
        self.answering(kind, Err(Failure::new(failure, message).into()))
    }

    fn refusing(self) -> Self {
        Self {
            admission: Err(refusal()),
            ..self
        }
    }

    /// The requests in call order, as the journal records them.
    fn requests(&self) -> Vec<Value> {
        self.calls
            .borrow()
            .iter()
            .map(|(request, _)| request.to_journal())
            .collect()
    }

    /// The `http` calls as the runtime journals them.
    fn http_journal(&self) -> Vec<JournalEntry> {
        self.calls
            .borrow()
            .iter()
            .filter(|(request, _)| request.kind() == ActionKind::Http)
            .map(|(request, answer)| JournalEntry {
                request_hash: request_hash(&request.to_journal()),
                result: match answer {
                    Ok(ActionReply::Http(response)) => JournalResult::Response(response.clone()),
                    Err(HostError::Failure(failure)) => JournalResult::Failure(failure.clone()),
                    other => panic!("{NOT_HTTP}: {other:?}"),
                },
            })
            .collect()
    }

    /// How a dry run answers what this host was asked: `http` from the journal, the rest
    /// recorded.
    fn replay_answers(&self) -> Vec<(Value, Answer)> {
        self.calls
            .borrow()
            .iter()
            .map(|(request, _)| {
                let answer = match request.kind() {
                    ActionKind::Http => Answer::Journal,
                    _ => Answer::Recorded,
                };
                (request.to_journal(), answer)
            })
            .collect()
    }
}

impl Automation {
    fn load(name: &str) -> Self {
        Self::armed_with(name, Value::Null)
    }

    fn armed_with(name: &str, args: Value) -> Self {
        let source = source(name);
        let meta = parse_meta(&source).expect(MUST_PARSE);
        let args = resolve(&meta.args, &args).expect(MUST_RESOLVE);
        Self {
            source,
            meta,
            args,
            state: json!({}),
        }
    }

    /// Fires an event that a trigger routes here, keeps the state the firing commits, and
    /// returns how it ended with the requests it made.
    fn fire(&mut self, event: &Event, host: &ScriptedHost) -> Fired {
        assert!(self.routes(event), "{UNROUTED}: {event:?}");
        let outcome = self.outcome(event, host);
        if let Some(change) = outcome.state {
            self.state = change.state;
        }
        (outcome.end, host.requests())
    }

    fn outcome(&self, event: &Event, host: &ScriptedHost) -> FiringOutcome {
        run_firing(Firing {
            source: &self.source,
            meta: &self.meta,
            event,
            state: &self.state,
            args: &self.args,
            limits: &FiringLimits::default(),
            host,
        })
    }

    fn replay(&self, event: &Event, journal: &[JournalEntry]) -> DryRunReport {
        dry_run(DryRun {
            source: &self.source,
            meta: &self.meta,
            event,
            state: &self.state,
            args: &self.args,
            limits: &FiringLimits::default(),
            now_ms: NOW_MS,
            journal,
            admission: Ok(()),
        })
    }

    /// Whether a trigger routes the event here. The scheduler routes schedule events itself.
    fn routes(&self, event: &Event) -> bool {
        match event.detail {
            EventDetail::Schedule { .. } => self
                .meta
                .triggers
                .iter()
                .any(|trigger| matches!(trigger, Trigger::Schedule(_))),
            _ => first_match(&self.meta.triggers, &event.detail, &ExactTopics).is_some(),
        }
    }
}

fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(EXAMPLES_DIR)
}

fn source(name: &str) -> String {
    let path = examples_dir().join(name).with_extension(EXAMPLE_EXTENSION);
    fs::read_to_string(path).expect(MUST_READ)
}

/// `hour:00` on `day` in the system time zone, which scripts without `meta.timezone` use.
fn local_ms(day: Date, hour: i8) -> i64 {
    day.at(hour, 0, 0, 0)
        .to_zoned(TimeZone::system())
        .expect(MUST_EXIST)
        .timestamp()
        .as_millisecond()
}

fn refusal() -> LimitRefusal {
    LimitRefusal {
        reason: LimitReason::MaxPerHour,
        until: LIMITED_UNTIL,
    }
}

fn success(request: &ActionRequest) -> ActionReply {
    match request {
        ActionRequest::SetGoal(goal) => ActionReply::GoalSet(GoalSet {
            condition: goal.condition.clone(),
        }),
        ActionRequest::Http(_) => ActionReply::Http(HttpResponse {
            status: OK_STATUS,
            body: String::new(),
            json: None,
        }),
        ActionRequest::Reply { .. } | ActionRequest::Send(_) => ActionReply::Sent(SendReceipt {
            status: SendStatus::Queued,
            message_id: RECEIPT_ID.to_owned(),
            reason: None,
        }),
        ActionRequest::Publish { .. } | ActionRequest::Broadcast { .. } => {
            ActionReply::Published(PublishReceipt {
                message_id: RECEIPT_ID.to_owned(),
                audience: Audience::Topic,
                recipients: Vec::new(),
                skipped: 0,
                queued: Vec::new(),
            })
        }
        ActionRequest::StartWorkflow(run) => ActionReply::WorkflowStarted(WorkflowStarted {
            run_id: RUN_ID.to_owned(),
            name: run.name.clone(),
        }),
        ActionRequest::Message(_)
        | ActionRequest::Notify { .. }
        | ActionRequest::Pause { .. }
        | ActionRequest::Log { .. } => ActionReply::Done,
    }
}

fn responded(status: u16, json: Value) -> HostResult<ActionReply> {
    Ok(ActionReply::Http(HttpResponse {
        status,
        body: json.to_string(),
        json: Some(json),
    }))
}

fn run_started(run_id: &str, name: &str) -> HostResult<ActionReply> {
    Ok(ActionReply::WorkflowStarted(WorkflowStarted {
        run_id: run_id.to_owned(),
        name: name.to_owned(),
    }))
}

fn session() -> SessionView {
    SessionView {
        id: SESSION_ID.to_owned(),
        title: Untrusted::text(TITLE),
        name: Some(SESSION_NAME.to_owned()),
        mode: MODE.to_owned(),
        status: SessionStatus::Idle,
        status_since: AT,
        goal: None,
        cost: None,
        groups: Vec::new(),
        work: WorkView::default(),
    }
}

fn event_in(session: SessionView, detail: EventDetail) -> Event {
    Event {
        fire_id: FIRE_ID.to_owned(),
        at: AT,
        session,
        detail,
    }
}

fn event(detail: EventDetail) -> Event {
    event_in(session(), detail)
}

fn armed(reason: ArmedReason) -> Event {
    event(EventDetail::Armed { reason })
}

fn pursuing(mut event: Event, condition: &str) -> Event {
    event.session.goal = Some(GoalView {
        condition: condition.to_owned(),
        evaluations: EVALUATIONS,
    });
    event
}

fn scheduled_in(session: SessionView) -> Event {
    event_in(
        session,
        EventDetail::Schedule {
            scheduled_for: AT,
            late_by_s: 0,
        },
    )
}

fn scheduled() -> Event {
    scheduled_in(session())
}

/// A completed turn that the user started.
fn turn() -> IdleDetail {
    IdleDetail {
        outcome: TurnOutcome::Completed,
        error_kind: None,
        error: None,
        started_by: StartedBy::User,
        automations: Vec::new(),
        runs: RUNS,
        busy_s: BUSY_S,
        cost: None,
        work: Vec::new(),
        last_response: Untrusted::text(LAST_RESPONSE),
    }
}

fn idle(detail: IdleDetail) -> Event {
    event(EventDetail::Idle(detail))
}

fn goal_finished(verdict: GoalVerdict, condition: &str) -> Event {
    event(EventDetail::GoalFinished(GoalFinishedDetail {
        verdict,
        condition: condition.to_owned(),
        reason: Untrusted::text(GOAL_REASON),
        evaluations: EVALUATIONS,
        duration_s: GOAL_DURATION_S,
        cost: None,
    }))
}

fn needs_input(input: InputKind, tool: Option<&str>) -> Event {
    event_in(
        SessionView {
            status: SessionStatus::NeedsInput,
            ..session()
        },
        EventDetail::NeedsInput {
            input,
            tool: tool.map(str::to_owned),
            waiting_s: WAITING_S,
        },
    )
}

/// A queued message from a peer session, consumed by the automation.
fn peer_message(
    audience: Audience,
    topic: Option<&str>,
    sender: &str,
    text: &str,
) -> MessageDetail {
    MessageDetail {
        message_id: MESSAGE_ID.to_owned(),
        audience,
        topic: topic.map(str::to_owned),
        sender_kind: SenderKind::Session,
        sender: Some(sender.to_owned()),
        sender_automation: None,
        sender_label: None,
        sender_title: Some(Untrusted::text(PEER_TITLE)),
        sender_cwd: None,
        text: Untrusted::text(text),
        reply_to: None,
        admission: Admission::Queued,
        delivery: Delivery::Live,
        consumed: true,
    }
}

fn from_script(message: MessageDetail, label: &str) -> MessageDetail {
    MessageDetail {
        sender_kind: SenderKind::Script,
        sender: None,
        sender_label: Some(Untrusted::text(label)),
        sender_title: None,
        ..message
    }
}

fn received(message: MessageDetail) -> Event {
    event(EventDetail::MessageReceived(message))
}

fn work_finished(state: WorkState, attempts: u32) -> Event {
    event(EventDetail::WorkFinished(WorkFinishedDetail {
        group: SWARM_TASKS.to_owned(),
        work: FIX_LOGIN.to_owned(),
        message_id: MESSAGE_ID.to_owned(),
        topic: Some(SWARM_TASKS_TOPIC.to_owned()),
        state,
        attempts,
        max_attempts: MAX_ATTEMPTS,
        member: Some(WORKER.to_owned()),
        pause_reason: (state == WorkState::Paused).then_some(PauseReason::CompletionRequired),
        detail: Some(Untrusted::text(TASK_DETAIL)),
    }))
}

fn reported(work: &str, outcome: WorkOutcome, pause_reason: Option<PauseReason>) -> WorkReport {
    WorkReport {
        group: SWARM_TASKS.to_owned(),
        work: work.to_owned(),
        outcome,
        pause_reason,
        detail: None,
    }
}

fn workflow_finished(run_id: &str, workflow: &str, report: &str) -> Event {
    event(EventDetail::WorkflowFinished(WorkflowFinishedDetail {
        run_id: run_id.to_owned(),
        name: workflow.to_owned(),
        workflow: workflow.to_owned(),
        status: WorkflowStatus::Completed,
        report: Some(Untrusted::text(report)),
        result: None,
        error: None,
        scratch_dir: None,
        agents: AGENTS,
        tokens: TOKENS,
    }))
}

fn run(id: i64, conclusion: &str) -> Value {
    json!({
        "id": id,
        "conclusion": conclusion,
        "display_title": RUN_TITLE,
        "html_url": format!("{RUN_PAGE}{id}"),
    })
}

fn polled(runs: &[Value]) -> HostResult<ActionReply> {
    responded(OK_STATUS, json!({ "workflow_runs": runs }))
}

fn ci_failure(id: i64) -> String {
    format!("CI failed on main: {RUN_TITLE} {RUN_PAGE}{id}")
}

fn skipped(reason: &str) -> FiringEnd {
    FiringEnd::Skipped {
        reason: reason.to_owned(),
    }
}

fn released(reason: &str) -> FiringEnd {
    FiringEnd::Released {
        reason: reason.to_owned(),
    }
}

fn failed(kind: FailureKind, message: &str, line: u32, column: u32) -> FiringEnd {
    FiringEnd::Failed(FiringError {
        kind: ErrorKind::Failure(kind),
        message: message.to_owned(),
        line: Some(line),
        column: Some(column),
    })
}

fn goal_set(condition: &str) -> Value {
    json!({
        "kind": "set_goal",
        "condition": condition,
        "continuation_limit": CONTINUATION_LIMIT,
        "replace": false,
    })
}

fn messaged(text: &str) -> Value {
    json!({ "kind": "message", "text": text, "delivery": "next" })
}

/// A message that attaches the event, as plain JSON.
fn attaching(text: &str, event: &Event) -> Value {
    json!({ "kind": "message", "text": text, "attach": unmarked(event.to_tagged()), "delivery": "next" })
}

fn notified(text: &str) -> Value {
    json!({ "kind": "notify", "text": text })
}

fn paused(reason: &str) -> Value {
    json!({ "kind": "pause", "reason": reason })
}

fn logged(text: &str) -> Value {
    json!({ "kind": "log", "text": text })
}

fn published(topic: &str, text: &str) -> Value {
    json!({ "kind": "publish", "topic": topic, "text": text })
}

fn replied(text: &str) -> Value {
    json!({ "kind": "reply", "text": text })
}

fn started(name: &str, args: Value) -> Value {
    json!({ "kind": "start_workflow", "name": name, "args": args })
}

fn runs_request() -> Value {
    json!({
        "kind": "http",
        "method": "GET",
        "url": RUNS_URL,
        "query": { "branch": "main", "per_page": "1" },
        "headers": { "Accept": "application/vnd.github+json", "User-Agent": "caudra-ci-watch" },
        "bearer_env": "GITHUB_TOKEN",
        "timeout": HTTP_TIMEOUT,
    })
}

fn goal_posted(verdict: &str) -> Value {
    json!({
        "kind": "http",
        "method": "POST",
        "url": HOOK_URL,
        "bearer_env": "HOOK_TOKEN",
        "json": { "session": TITLE, "verdict": verdict, "reason": GOAL_REASON },
        "timeout": HTTP_TIMEOUT,
    })
}

/// A Slack post through a webhook URL kept in a secret.
fn posted(url_env: &str, text: &str) -> Value {
    json!({
        "kind": "http",
        "method": "POST",
        "url_env": url_env,
        "json": { "text": text },
        "timeout": HTTP_TIMEOUT,
    })
}

fn paged(ask: &str) -> Fired {
    let page = json!({
        "kind": "http",
        "method": "POST",
        "url_env": "NTFY_URL",
        "headers": { "Title": "Caudra is waiting" },
        "bearer_env": "NTFY_TOKEN",
        "body": format!("{TITLE} needs you to {ask}"),
        "timeout": HTTP_TIMEOUT,
    });
    (FiringEnd::Completed, vec![page])
}

/// Tagged JSON without the untrusted marks, as an outgoing payload carries it.
fn unmarked(value: Value) -> Value {
    match value {
        Value::Object(entries) => {
            if let (1, Some(inner)) = (entries.len(), entries.get(UNTRUSTED_TAG)) {
                return inner.clone();
            }
            entries
                .into_iter()
                .map(|(key, item)| (key, unmarked(item)))
                .collect()
        }
        Value::Array(items) => items.into_iter().map(unmarked).collect(),
        other => other,
    }
}

fn answers(report: &DryRunReport) -> Vec<(Value, Answer)> {
    report
        .actions
        .iter()
        .map(|action| (action.request.to_journal(), action.answer))
        .collect()
}

#[test]
fn the_examples_are_the_ones_scripted_here() {
    let mut names = fs::read_dir(examples_dir())
        .expect(MUST_LIST)
        .map(|entry| entry.expect(MUST_LIST).path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == EXAMPLE_EXTENSION)
        })
        .filter_map(|path| Some(path.file_stem()?.to_str()?.to_owned()))
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, EXAMPLES);
}

#[test]
fn every_example_validates_under_its_own_name() {
    let failures = EXAMPLES
        .iter()
        .filter_map(|name| match validate(&source(name), NOW_MS) {
            Ok(report) if report.meta.name == *name => None,
            Ok(report) => Some(format!("{name}: {MISNAMED} {}", report.meta.name)),
            Err(error) => Some(format!("{name}: {error}")),
        })
        .collect::<Vec<_>>();
    assert_eq!(failures, Vec::<String>::new());
}

#[test]
fn goal_chain_replays_the_step_by_step_table() {
    let mut chain =
        Automation::armed_with(GOAL_CHAIN, json!({ "goals": [GOAL_A, GOAL_B, GOAL_C] }));
    let rows = [
        Row {
            what: "The session starts without a goal",
            event: armed(ArmedReason::Launch),
            host: ScriptedHost::default(),
            fired: (FiringEnd::Completed, vec![goal_set(GOAL_A)]),
            state: json!({ "current": GOAL_A }),
        },
        Row {
            what: "A is met",
            event: goal_finished(GoalVerdict::Met, GOAL_A),
            host: ScriptedHost::default(),
            fired: (FiringEnd::Completed, vec![goal_set(GOAL_B)]),
            state: json!({ "done": [GOAL_A], "current": GOAL_B }),
        },
        Row {
            what: "Caudra exits during B, then resumes with B restored first",
            event: pursuing(armed(ArmedReason::Resume), GOAL_B),
            host: ScriptedHost::default(),
            fired: (skipped(GOAL_ACTIVE), Vec::new()),
            state: json!({ "done": [GOAL_A], "current": GOAL_B }),
        },
        Row {
            what: "B is judged impossible",
            event: goal_finished(GoalVerdict::Impossible, GOAL_B),
            host: ScriptedHost::default(),
            fired: (FiringEnd::Completed, vec![notified(CHAIN_IMPOSSIBLE)]),
            state: json!({ "done": [GOAL_A] }),
        },
        Row {
            what: "You fix the blocker and arm it again",
            event: armed(ArmedReason::Manual),
            host: ScriptedHost::default(),
            fired: (FiringEnd::Completed, vec![goal_set(GOAL_B)]),
            state: json!({ "done": [GOAL_A], "current": GOAL_B }),
        },
        Row {
            what: "An unrecoverable error clears B",
            event: goal_finished(GoalVerdict::Cleared, GOAL_B),
            host: ScriptedHost::default(),
            fired: (FiringEnd::Completed, vec![notified(CHAIN_CLEARED)]),
            state: json!({ "done": [GOAL_A] }),
        },
        Row {
            what: "You arm it again",
            event: armed(ArmedReason::Manual),
            host: ScriptedHost::default(),
            fired: (FiringEnd::Completed, vec![goal_set(GOAL_B)]),
            state: json!({ "done": [GOAL_A], "current": GOAL_B }),
        },
        Row {
            what: "B is met, but set_goal hits max_per_hour",
            event: goal_finished(GoalVerdict::Met, GOAL_B),
            host: ScriptedHost::default().refusing(),
            fired: (FiringEnd::Limited(refusal()), Vec::new()),
            state: json!({ "done": [GOAL_A], "current": GOAL_B }),
        },
        Row {
            what: "The deferred event is retried",
            event: goal_finished(GoalVerdict::Met, GOAL_B),
            host: ScriptedHost::default(),
            fired: (FiringEnd::Completed, vec![goal_set(GOAL_C)]),
            state: json!({ "done": [GOAL_A, GOAL_B], "current": GOAL_C }),
        },
        Row {
            what: "C is met",
            event: goal_finished(GoalVerdict::Met, GOAL_C),
            host: ScriptedHost::default(),
            fired: (FiringEnd::Completed, vec![notified(CHAIN_DONE)]),
            state: json!({ "done": [GOAL_A, GOAL_B, GOAL_C] }),
        },
    ];
    for row in rows {
        assert_eq!(
            (chain.fire(&row.event, &row.host), &chain.state),
            (row.fired, &row.state),
            "{}",
            row.what
        );
    }
}

#[test]
fn goal_chain_waits_out_a_goal_it_did_not_set() {
    let mut chain = Automation::armed_with(GOAL_CHAIN, json!({ "goals": [GOAL_A] }));
    assert_eq!(
        chain.fire(
            &pursuing(armed(ArmedReason::Launch), OTHER_GOAL),
            &ScriptedHost::default()
        ),
        (skipped(GOAL_ACTIVE), Vec::new())
    );
    assert_eq!(
        chain.fire(
            &goal_finished(GoalVerdict::Impossible, OTHER_GOAL),
            &ScriptedHost::default()
        ),
        (FiringEnd::Completed, vec![goal_set(GOAL_A)])
    );
    assert_eq!(chain.state, json!({ "current": GOAL_A }));
}

#[test]
fn keep_going_continues_the_backlog_during_work_hours() {
    let mut keep_going = Automation::load(KEEP_GOING);
    let at_work = || ScriptedHost::at(local_ms(MONDAY, WORK_HOUR));
    assert_eq!(
        keep_going.fire(&idle(turn()), &at_work()),
        (FiringEnd::Completed, vec![messaged(BACKLOG_PROMPT)])
    );
    let errored = IdleDetail {
        outcome: TurnOutcome::Error,
        error_kind: Some(ProviderError::Overloaded),
        ..turn()
    };
    assert_eq!(
        keep_going.fire(&idle(errored), &at_work()),
        (skipped(LAST_TURN_ERRORED), Vec::new())
    );
    let finished = IdleDetail {
        last_response: Untrusted::text(BACKLOG_EMPTY_RESPONSE),
        ..turn()
    };
    assert_eq!(
        keep_going.fire(&idle(finished), &at_work()),
        (skipped(BACKLOG_EMPTY), Vec::new())
    );
    for (day, hour) in [
        (MONDAY, EARLY_HOUR),
        (MONDAY, EVENING_HOUR),
        (SATURDAY, WORK_HOUR),
    ] {
        assert_eq!(
            keep_going.fire(&idle(turn()), &ScriptedHost::at(local_ms(day, hour))),
            (skipped(OUTSIDE_WORK_HOURS), Vec::new()),
            "{day} {hour}:00"
        );
    }
    assert_eq!(keep_going.state, json!({}));
}

#[test]
fn keep_going_takes_its_backlog_and_hours_from_args() {
    let mut keep_going = Automation::armed_with(
        KEEP_GOING,
        json!({ "file": CUSTOM_BACKLOG, "from_hour": EARLY_HOUR, "until_hour": WORK_HOUR }),
    );
    assert_eq!(
        keep_going.fire(
            &idle(turn()),
            &ScriptedHost::at(local_ms(MONDAY, EARLY_HOUR))
        ),
        (FiringEnd::Completed, vec![messaged(CUSTOM_BACKLOG_PROMPT)])
    );
    assert_eq!(
        keep_going.fire(
            &idle(turn()),
            &ScriptedHost::at(local_ms(MONDAY, WORK_HOUR))
        ),
        (skipped(OUTSIDE_WORK_HOURS), Vec::new())
    );
}

#[test]
fn ci_watch_publishes_each_failed_run_once() {
    let mut watch = Automation::load(CI_WATCH);
    let polling =
        |runs: &[Value]| ScriptedHost::default().answering(ActionKind::Http, polled(runs));
    let failed_run = |id| {
        (
            FiringEnd::Completed,
            vec![runs_request(), published(CI_FAILURES, &ci_failure(id))],
        )
    };
    assert_eq!(
        watch.fire(&scheduled(), &polling(&[run(FIRST_RUN, FAILURE)])),
        failed_run(FIRST_RUN)
    );
    assert_eq!(watch.state, json!({ "last_run": FIRST_RUN }));
    assert_eq!(
        watch.fire(&scheduled(), &polling(&[run(FIRST_RUN, FAILURE)])),
        (FiringEnd::Completed, vec![runs_request()])
    );
    let unrecorded = polling(&[run(SECOND_RUN, FAILURE)]).failing(
        ActionKind::Publish,
        FailureKind::Unavailable,
        HISTORY_UNAVAILABLE,
    );
    assert_eq!(
        watch.fire(&scheduled(), &unrecorded),
        (
            failed(
                FailureKind::Unavailable,
                HISTORY_UNAVAILABLE,
                CI_PUBLISH_LINE,
                CI_PUBLISH_COLUMN
            ),
            failed_run(SECOND_RUN).1
        )
    );
    assert_eq!(watch.state, json!({ "last_run": FIRST_RUN }));
    assert_eq!(
        watch.fire(&scheduled(), &polling(&[run(SECOND_RUN, FAILURE)])),
        failed_run(SECOND_RUN)
    );
    assert_eq!(watch.state, json!({ "last_run": SECOND_RUN }));
    assert_eq!(
        watch.fire(&scheduled(), &polling(&[run(THIRD_RUN, SUCCESS)])),
        (FiringEnd::Completed, vec![runs_request()])
    );
    assert_eq!(watch.state, json!({ "last_run": THIRD_RUN }));
    let unavailable = ScriptedHost::default()
        .answering(ActionKind::Http, responded(BAD_GATEWAY_STATUS, json!({})));
    assert_eq!(
        watch.fire(&scheduled(), &unavailable),
        (
            FiringEnd::Completed,
            vec![runs_request(), logged(GITHUB_UNAVAILABLE)]
        )
    );
    assert_eq!(
        watch.fire(&scheduled(), &polling(&[])),
        (FiringEnd::Completed, vec![runs_request()])
    );
    assert_eq!(watch.state, json!({ "last_run": THIRD_RUN }));
}

#[test]
fn goal_webhook_posts_every_verdict_and_records_impossible_goals() {
    let mut webhook = Automation::load(GOAL_WEBHOOK);
    assert_eq!(
        webhook.fire(
            &goal_finished(GoalVerdict::Met, GOAL_A),
            &ScriptedHost::default()
        ),
        (FiringEnd::Completed, vec![goal_posted("met")])
    );
    let impossible = goal_finished(GoalVerdict::Impossible, GOAL_A);
    assert_eq!(
        webhook.fire(&impossible, &ScriptedHost::default()),
        (
            FiringEnd::Completed,
            vec![
                goal_posted("impossible"),
                attaching(BLOCKERS_PROMPT, &impossible)
            ]
        )
    );
    let rejected = ScriptedHost::default()
        .answering(ActionKind::Http, responded(SERVER_ERROR_STATUS, json!({})));
    assert_eq!(
        webhook.fire(&impossible, &rejected),
        (FiringEnd::Completed, vec![goal_posted("impossible")])
    );
    assert_eq!(webhook.state, json!({}));
}

#[test_case(ArmedReason::Launch => (FiringEnd::Completed, vec![published(SWARM_STATUS, ONLINE)]); "launch")]
#[test_case(ArmedReason::Resume => (FiringEnd::Completed, vec![published(SWARM_STATUS, ONLINE)]); "resume")]
#[test_case(ArmedReason::Manual => (skipped(ARMED_BY_HAND), Vec::new()); "manual")]
#[test_case(ArmedReason::Unpaused => (skipped(ARMED_BY_HAND), Vec::new()); "unpaused")]
fn join_swarm_announces_launches_and_resumes(reason: ArmedReason) -> Fired {
    let mut join = Automation::load(JOIN_SWARM);
    let fired = join.fire(&armed(reason), &ScriptedHost::default());
    assert_eq!(join.state, json!({}));
    fired
}

#[test]
fn nightly_review_posts_the_report_of_the_run_it_started() {
    let mut review = Automation::load(NIGHTLY_REVIEW);
    let starting = ScriptedHost::default().answering(
        ActionKind::StartWorkflow,
        run_started(SECOND_RUN_ID, REVIEW_CHANGES),
    );
    assert_eq!(
        review.fire(&scheduled(), &starting),
        (
            FiringEnd::Completed,
            vec![started(REVIEW_CHANGES, json!({ "scope": REVIEW_SCOPE }))]
        )
    );
    assert_eq!(review.state, json!({ "run": SECOND_RUN_ID }));
    assert_eq!(
        review.fire(
            &workflow_finished(RUN_ID, REVIEW_CHANGES, REVIEW_REPORT),
            &ScriptedHost::default()
        ),
        (FiringEnd::Completed, Vec::new())
    );
    assert_eq!(
        review.fire(
            &workflow_finished(SECOND_RUN_ID, REVIEW_CHANGES, REVIEW_REPORT),
            &ScriptedHost::default()
        ),
        (
            FiringEnd::Completed,
            vec![posted(REVIEW_URL_ENV, NIGHTLY_POST)]
        )
    );
    assert_eq!(review.state, json!({ "run": SECOND_RUN_ID }));
}

#[test_case(InputKind::Permission, Some(SHELL_TOOL) => paged(APPROVE_SHELL); "permission")]
#[test_case(InputKind::Plan, None => paged(REVIEW_PLAN); "plan")]
#[test_case(InputKind::Auth, None => paged(SIGN_IN); "auth")]
#[test_case(InputKind::Question, None => paged(ANSWER_QUESTION); "question")]
fn page_me_names_what_the_session_waits_for(input: InputKind, tool: Option<&str>) -> Fired {
    let mut page_me = Automation::load(PAGE_ME);
    let fired = page_me.fire(&needs_input(input, tool), &ScriptedHost::default());
    assert_eq!(page_me.state, json!({}));
    fired
}

#[test]
fn research_desk_answers_research_requests_with_reports() {
    let mut desk = Automation::load(RESEARCH_DESK);
    let request = received(peer_message(Audience::Direct, None, LEAD, RESEARCH_REQUEST));
    let research = started(DEEP_RESEARCH, json!({ "query": RESEARCH_QUERY }));
    let pending = json!({ "to": LEAD, "message": MESSAGE_ID });
    assert_eq!(
        desk.fire(&request, &ScriptedHost::default()),
        (
            FiringEnd::Completed,
            vec![research.clone(), replied(ACKNOWLEDGED)]
        )
    );
    assert_eq!(desk.state, json!({ "requests": { RUN_ID: pending } }));
    let scripted = from_script(
        peer_message(Audience::Direct, None, LEAD, RESEARCH_REQUEST),
        NIGHTLY_CI,
    );
    assert_eq!(
        desk.fire(&received(scripted), &ScriptedHost::default()),
        (released(NOT_RESEARCH), Vec::new())
    );
    assert_eq!(
        desk.fire(
            &received(peer_message(Audience::Direct, None, LEAD, GREETING)),
            &ScriptedHost::default()
        ),
        (released(NOT_RESEARCH), Vec::new())
    );
    let unacknowledged = ScriptedHost::default()
        .answering(
            ActionKind::StartWorkflow,
            run_started(SECOND_RUN_ID, DEEP_RESEARCH),
        )
        .failing(ActionKind::Reply, FailureKind::Unavailable, SENDER_LEFT);
    assert_eq!(
        desk.fire(&request, &unacknowledged),
        (
            FiringEnd::Completed,
            vec![research, replied(ACKNOWLEDGED), logged(ACK_FAILED)]
        )
    );
    assert_eq!(
        desk.state,
        json!({ "requests": { RUN_ID: pending, SECOND_RUN_ID: pending } })
    );
    assert_eq!(
        desk.fire(
            &workflow_finished(RUN_ID, DEEP_RESEARCH, RESEARCH_REPORT),
            &ScriptedHost::default()
        ),
        (
            FiringEnd::Completed,
            vec![
                json!({ "kind": "send", "to": LEAD, "text": REPORT_SENT, "reply_to": MESSAGE_ID })
            ]
        )
    );
    assert_eq!(
        desk.state,
        json!({ "requests": { SECOND_RUN_ID: pending } })
    );
    assert_eq!(
        desk.fire(
            &workflow_finished(OTHER_RUN_ID, DEEP_RESEARCH, RESEARCH_REPORT),
            &ScriptedHost::default()
        ),
        (FiringEnd::Completed, Vec::new())
    );
    assert_eq!(
        desk.state,
        json!({ "requests": { SECOND_RUN_ID: pending } })
    );
}

#[test_case(TurnOutcome::Error, Some(ProviderError::RateLimit) => vec![messaged(RESUME_PROMPT)]; "rate_limit")]
#[test_case(TurnOutcome::Error, Some(ProviderError::Overloaded) => vec![messaged(RESUME_PROMPT)]; "overloaded")]
#[test_case(TurnOutcome::Error, Some(ProviderError::Auth) => Vec::<Value>::new(); "auth")]
#[test_case(TurnOutcome::Completed, None => Vec::<Value>::new(); "completed")]
#[test_case(TurnOutcome::Cancelled, None => Vec::<Value>::new(); "cancelled")]
fn retry_overload_resumes_after_provider_errors(
    outcome: TurnOutcome,
    error_kind: Option<ProviderError>,
) -> Vec<Value> {
    let mut retry = Automation::load(RETRY_OVERLOAD);
    let turn = IdleDetail {
        outcome,
        error_kind,
        ..turn()
    };
    let (end, actions) = retry.fire(&idle(turn), &ScriptedHost::default());
    assert_eq!((end, &retry.state), (FiringEnd::Completed, &json!({})));
    actions
}

#[test_case(None => Vec::<Value>::new(); "no_cost")]
#[test_case(Some(UNDER_BUDGET) => Vec::<Value>::new(); "under_budget")]
#[test_case(Some(OVER_BUDGET) => vec![paused(SPEND_PAUSE), notified(SPEND_NOTICE)]; "over_budget")]
fn spend_guard_pauses_automations_past_twenty_dollars(cost: Option<f64>) -> Vec<Value> {
    let mut guard = Automation::load(SPEND_GUARD);
    let spent = event_in(SessionView { cost, ..session() }, EventDetail::Idle(turn()));
    let (end, actions) = guard.fire(&spent, &ScriptedHost::default());
    assert_eq!((end, &guard.state), (FiringEnd::Completed, &json!({})));
    actions
}

#[test]
fn standup_asks_for_bullets_then_posts_the_answer() {
    let mut standup = Automation::load(STANDUP);
    assert_eq!(
        standup.fire(&scheduled(), &ScriptedHost::default()),
        (FiringEnd::Completed, vec![messaged(STANDUP_PROMPT)])
    );
    let answered = IdleDetail {
        started_by: StartedBy::Automation {
            automation: STANDUP.to_owned(),
            fire_id: FIRE_ID.to_owned(),
        },
        automations: vec![STANDUP.to_owned()],
        last_response: Untrusted::text(BULLETS),
        ..turn()
    };
    assert_eq!(
        standup.fire(&idle(answered.clone()), &ScriptedHost::default()),
        (FiringEnd::Completed, vec![posted(STANDUP_URL_ENV, BULLETS)])
    );
    assert_eq!(
        standup.fire(&idle(turn()), &ScriptedHost::default()),
        (FiringEnd::Completed, Vec::new())
    );
    let errored = IdleDetail {
        outcome: TurnOutcome::Error,
        ..answered
    };
    assert_eq!(
        standup.fire(&idle(errored), &ScriptedHost::default()),
        (FiringEnd::Completed, Vec::new())
    );
    assert_eq!(standup.state, json!({}));
}

#[test]
fn status_beacon_publishes_each_status_change_once() {
    let mut beacon = Automation::load(STATUS_BEACON);
    assert_eq!(
        beacon.fire(&scheduled(), &ScriptedHost::default()),
        (
            FiringEnd::Completed,
            vec![published(SWARM_STATUS, IDLE_ANNOUNCEMENT)]
        )
    );
    assert_eq!(beacon.state, json!({ "last": IDLE }));
    assert_eq!(
        beacon.fire(&scheduled(), &ScriptedHost::default()),
        (FiringEnd::Completed, Vec::new())
    );
    let holding = scheduled_in(SessionView {
        status: SessionStatus::Working,
        work: WorkView {
            held: Some(HeldWork {
                group: SWARM_TASKS.to_owned(),
                work: FIX_LOGIN.to_owned(),
                attempt: FIRST_ATTEMPT,
                max_attempts: MAX_ATTEMPTS,
            }),
            paused: Vec::new(),
        },
        ..session()
    });
    let unrecorded = ScriptedHost::default().failing(
        ActionKind::Publish,
        FailureKind::Unavailable,
        HISTORY_UNAVAILABLE,
    );
    assert_eq!(
        beacon.fire(&holding, &unrecorded),
        (
            failed(
                FailureKind::Unavailable,
                HISTORY_UNAVAILABLE,
                BEACON_PUBLISH_LINE,
                BEACON_PUBLISH_COLUMN
            ),
            vec![published(SWARM_STATUS, HOLDING_ANNOUNCEMENT)]
        )
    );
    assert_eq!(beacon.state, json!({ "last": IDLE }));
    assert_eq!(
        beacon.fire(&holding, &ScriptedHost::default()),
        (
            FiringEnd::Completed,
            vec![published(SWARM_STATUS, HOLDING_ANNOUNCEMENT)]
        )
    );
    assert_eq!(beacon.state, json!({ "last": HOLDING }));
}

#[test]
fn status_desk_answers_status_questions_and_releases_the_rest() {
    let mut desk = Automation::load(STATUS_DESK);
    let asked = |text| peer_message(Audience::Direct, None, LEAD, text);
    assert_eq!(
        desk.fire(&received(asked(STATUS_QUESTION)), &ScriptedHost::default()),
        (FiringEnd::Completed, vec![replied(IDLE_STATUS)])
    );
    let working = event_in(
        SessionView {
            status: SessionStatus::Working,
            ..session()
        },
        EventDetail::MessageReceived(asked(PADDED_STATUS_QUESTION)),
    );
    assert_eq!(
        desk.fire(&pursuing(working, GOAL_A), &ScriptedHost::default()),
        (FiringEnd::Completed, vec![replied(WORKING_STATUS)])
    );
    assert_eq!(
        desk.fire(&received(asked(GREETING)), &ScriptedHost::default()),
        (released(NOT_STATUS), Vec::new())
    );
    assert_eq!(
        desk.fire(
            &received(from_script(asked(STATUS_QUESTION), NIGHTLY_CI)),
            &ScriptedHost::default()
        ),
        (released(SCRIPT_SENDER), Vec::new())
    );
    let sender_left =
        ScriptedHost::default().failing(ActionKind::Reply, FailureKind::Unavailable, SENDER_LEFT);
    assert_eq!(
        desk.fire(&received(asked(STATUS_QUESTION)), &sender_left),
        (
            failed(
                FailureKind::Unavailable,
                SENDER_LEFT,
                STATUS_REPLY_LINE,
                STATUS_REPLY_COLUMN
            ),
            vec![replied(IDLE_STATUS)]
        )
    );
    assert!(!desk.routes(&received(MessageDetail {
        sender_kind: SenderKind::Automation,
        sender_automation: Some(STATUS_DESK.to_owned()),
        ..asked(STATUS_QUESTION)
    })));
    assert_eq!(desk.state, json!({}));
}

#[test]
fn task_tracker_reviews_finished_tasks_and_reports_the_rest() {
    let mut tracker = Automation::load(TASK_TRACKER);
    let completed = work_finished(WorkState::Completed, FIRST_ATTEMPT);
    assert_eq!(
        tracker.fire(&completed, &ScriptedHost::default()),
        (
            FiringEnd::Completed,
            vec![attaching(TASK_DONE_PROMPT, &completed)]
        )
    );
    assert_eq!(
        tracker.fire(
            &work_finished(WorkState::Failed, MAX_ATTEMPTS),
            &ScriptedHost::default()
        ),
        (FiringEnd::Completed, vec![notified(TASK_FAILED)])
    );
    assert_eq!(
        tracker.fire(
            &work_finished(WorkState::Paused, FIRST_ATTEMPT),
            &ScriptedHost::default()
        ),
        (FiringEnd::Completed, vec![notified(TASK_PAUSED)])
    );
    assert!(!tracker.routes(&work_finished(WorkState::Cancelled, FIRST_ATTEMPT)));
    assert_eq!(tracker.state, json!({}));
}

#[test]
fn timebox_asks_a_long_turn_to_wrap_up() {
    let mut timebox = Automation::load(TIMEBOX);
    let after = |status, minutes| {
        scheduled_in(SessionView {
            status,
            status_since: AT - minutes * SECONDS_PER_MINUTE,
            ..session()
        })
    };
    assert_eq!(
        timebox.fire(
            &after(SessionStatus::Working, SHORT_TURN_MINUTES),
            &ScriptedHost::default()
        ),
        (FiringEnd::Completed, Vec::new())
    );
    let wrap_up = json!({
        "kind": "message",
        "text": WRAP_UP_PROMPT,
        "delivery": "guide",
        "expires": "10m",
    });
    assert_eq!(
        timebox.fire(
            &after(SessionStatus::Working, LONG_TURN_MINUTES),
            &ScriptedHost::default()
        ),
        (FiringEnd::Completed, vec![notified(STILL_WORKING), wrap_up])
    );
    assert_eq!(
        timebox.fire(
            &after(SessionStatus::Idle, LONG_TURN_MINUTES),
            &ScriptedHost::default()
        ),
        (FiringEnd::Completed, Vec::new())
    );
    assert_eq!(timebox.state, json!({}));
}

#[test]
fn work_nudge_asks_once_per_paused_item_then_notifies() {
    let mut nudge = Automation::load(WORK_NUDGE);
    let reporting = |work| idle(IdleDetail { work, ..turn() });
    let unreported = |work| {
        reported(
            work,
            WorkOutcome::Paused,
            Some(PauseReason::CompletionRequired),
        )
    };
    assert_eq!(
        nudge.fire(&reporting(Vec::new()), &ScriptedHost::default()),
        (FiringEnd::Completed, Vec::new())
    );
    assert_eq!(
        nudge.fire(
            &reporting(vec![unreported(FIX_LOGIN)]),
            &ScriptedHost::default()
        ),
        (FiringEnd::Completed, vec![messaged(NUDGE_LOGIN)])
    );
    assert_eq!(nudge.state, json!({ "last": FIX_LOGIN }));
    assert_eq!(
        nudge.fire(
            &reporting(vec![unreported(FIX_LOGIN)]),
            &ScriptedHost::default()
        ),
        (FiringEnd::Completed, vec![notified(PAUSED_AGAIN)])
    );
    let settled = vec![
        reported(FIX_LOGIN, WorkOutcome::Completed, None),
        reported(
            FIX_SIGNUP,
            WorkOutcome::Paused,
            Some(PauseReason::TurnLimit),
        ),
    ];
    assert_eq!(
        nudge.fire(&reporting(settled), &ScriptedHost::default()),
        (FiringEnd::Completed, Vec::new())
    );
    let next = vec![
        reported(FIX_LOGIN, WorkOutcome::Completed, None),
        unreported(FIX_SIGNUP),
    ];
    assert_eq!(
        nudge.fire(&reporting(next), &ScriptedHost::default()),
        (FiringEnd::Completed, vec![messaged(NUDGE_SIGNUP)])
    );
    assert_eq!(nudge.state, json!({ "last": FIX_SIGNUP }));
}

#[test]
fn ci_triage_starts_a_root_cause_run_for_each_failure() {
    let mut triage = Automation::load(CI_TRIAGE);
    let failure = ci_failure(FIRST_RUN);
    let from_watcher = peer_message(Audience::Topic, Some(CI_FAILURES), CI_WATCHER, &failure);
    let root_cause = (
        FiringEnd::Completed,
        vec![json!({
            "kind": "start_workflow",
            "name": ROOT_CAUSE,
            "args": { "failure": failure },
            "agent_budget": AGENT_BUDGET,
        })],
    );
    assert_eq!(
        triage.fire(&received(from_watcher.clone()), &ScriptedHost::default()),
        root_cause
    );
    assert_eq!(
        triage.fire(
            &received(from_script(from_watcher.clone(), NIGHTLY_CI)),
            &ScriptedHost::default()
        ),
        root_cause
    );
    assert!(!triage.routes(&received(MessageDetail {
        sender: Some(STRANGER.to_owned()),
        ..from_watcher
    })));
    assert_eq!(triage.state, json!({}));
}

#[test_case(CI_WATCH, scheduled(), ScriptedHost::default().answering(ActionKind::Http, polled(&[run(FIRST_RUN, FAILURE)])); "an_answer_and_a_publication")]
#[test_case(GOAL_WEBHOOK, goal_finished(GoalVerdict::Impossible, GOAL_A), ScriptedHost::default(); "an_answer_and_a_message")]
#[test_case(GOAL_WEBHOOK, goal_finished(GoalVerdict::Met, GOAL_A), ScriptedHost::default().failing(ActionKind::Http, FailureKind::Timeout, TIMED_OUT); "a_failure")]
#[test_case(PAGE_ME, needs_input(InputKind::Plan, None), ScriptedHost::default(); "a_secret_url")]
fn a_recorded_firing_replays_from_its_journal(name: &str, event: Event, host: ScriptedHost) {
    let automation = Automation::load(name);
    let recorded = automation.outcome(&event, &host);
    let replayed = automation.replay(&event, &host.http_journal());
    assert_eq!(replayed.outcome, recorded);
    assert_eq!(answers(&replayed), host.replay_answers());
}

#[test]
fn a_replay_runs_against_the_current_state() {
    let mut watch = Automation::load(CI_WATCH);
    let host =
        ScriptedHost::default().answering(ActionKind::Http, polled(&[run(FIRST_RUN, FAILURE)]));
    assert_eq!(
        watch.fire(&scheduled(), &host),
        (
            FiringEnd::Completed,
            vec![
                runs_request(),
                published(CI_FAILURES, &ci_failure(FIRST_RUN))
            ]
        )
    );
    let replayed = watch.replay(&scheduled(), &host.http_journal());
    assert_eq!(
        (replayed.outcome.end.clone(), replayed.outcome.state.clone()),
        (FiringEnd::Completed, None)
    );
    assert_eq!(answers(&replayed), vec![(runs_request(), Answer::Journal)]);
}
