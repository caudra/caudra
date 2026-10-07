//! Dry runs: a finished firing's event run again against the current script and args and a copy
//! of the current state, with nothing leaving the session. `now()` answers the original firing's
//! time, `http` answers from its journal, and every other request is recorded, not performed.
//! Capability checks and per-firing limits apply; the automation's limits are only reported.

use std::cell::RefCell;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::engine::{Firing, FiringLimits, FiringOutcome, run_firing};
use crate::event::{Audience, Event};
use crate::host::{
    ActionKind, ActionReply, ActionRequest, AutomationHost, CallSite, Failure, GoalSet, HostError,
    HostResult, HttpResponse, Interruption, PublishReceipt, SendReceipt, SendStatus,
    WorkflowStarted, request_hash,
};
use crate::limits::LimitRefusal;
use crate::meta::AutomationMeta;
use crate::snapshot::ActionStatus;

/// The run id `start_workflow` answers in a dry run.
pub const DRY_RUN_ID: &str = "dry-run";
/// The message id replies, sends and publications answer in a dry run.
pub const DRY_MESSAGE_ID: &str = "dry-run";
/// The status of an `http` request the journal does not hold.
pub const STUB_STATUS: u16 = 0;

/// An `http` request the original firing made, as its journal recorded the outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct JournalEntry {
    /// [`request_hash`] of the request's [`ActionRequest::to_journal`] body.
    pub request_hash: String,
    pub result: JournalResult,
}

/// What the original firing's `http` request got.
#[derive(Debug, Clone, PartialEq)]
pub enum JournalResult {
    Response(HttpResponse),
    Failure(Failure),
    /// A response too large to store, which a dry run answers with the stub, marked
    /// [`Answer::Cut`].
    Cut,
}

/// One action of the firing a dry run replays, as storage keeps it.
#[derive(Debug)]
pub struct StoredAction<'a> {
    pub kind: ActionKind,
    pub status: ActionStatus,
    pub request_hash: &'a str,
    /// The result's JSON text, or a preview of it when `result_cut`.
    pub result: Option<&'a str>,
    pub result_cut: bool,
    /// The error's text, which reads back as a [`Failure`] when the action failed.
    pub error: Option<&'a str>,
}

/// A finished firing to run again.
pub struct DryRun<'a> {
    pub source: &'a str,
    pub meta: &'a AutomationMeta,
    pub event: &'a Event,
    /// A copy of the current state, as tagged JSON.
    pub state: &'a Value,
    pub args: &'a Map<String, Value>,
    pub limits: &'a FiringLimits,
    /// The original firing's time, in unix milliseconds.
    pub now_ms: i64,
    /// The original firing's `http` requests in call order, as [`journal_entry`] reads them.
    pub journal: &'a [JournalEntry],
    /// What the automation's limits answer now.
    pub admission: Result<(), LimitRefusal>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DryRunReport {
    /// How the firing would end, with the state change it would commit.
    pub outcome: FiringOutcome,
    /// Every request in call order, logs included.
    pub actions: Vec<DryAction>,
    /// The limit that would have refused the firing's first charging action.
    pub limited: Option<LimitRefusal>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DryAction {
    pub site: CallSite,
    pub request: ActionRequest,
    pub answer: Answer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    /// Not performed. Replies carry the placeholder ids.
    Recorded,
    /// An `http` request answered from the original firing's journal.
    Journal,
    /// An `http` request the journal does not hold, answered with [`STUB_STATUS`], an empty
    /// body and no JSON.
    Stubbed,
    /// An `http` request whose journaled response was too large to store, answered like a
    /// [`Answer::Stubbed`] one.
    Cut,
}

struct DryRunHost<'a> {
    now_ms: i64,
    /// Entries no request has matched yet, in journal order.
    unmatched: RefCell<Vec<&'a JournalEntry>>,
    actions: RefCell<Vec<DryAction>>,
}

/// Runs the firing again. Nothing is performed or stored.
pub fn dry_run(run: DryRun<'_>) -> DryRunReport {
    let host = DryRunHost {
        now_ms: run.now_ms,
        unmatched: RefCell::new(run.journal.iter().collect()),
        actions: RefCell::default(),
    };
    let outcome = run_firing(Firing {
        source: run.source,
        meta: run.meta,
        event: run.event,
        state: run.state,
        args: run.args,
        limits: run.limits,
        host: &host,
    });
    let limited = run.admission.err().filter(|_| outcome.charged);
    DryRunReport {
        outcome,
        actions: host.actions.into_inner(),
        limited,
    }
}

/// The journal entry a stored action answers a dry run's `http` request from. Only an `http`
/// action that got a response or a [`Failure`] has one; a dry run stubs a request whose
/// stored result or error does not read back.
pub fn journal_entry(action: StoredAction<'_>) -> Option<JournalEntry> {
    if action.kind != ActionKind::Http {
        return None;
    }
    let result = match action.status {
        ActionStatus::Done if action.result_cut => JournalResult::Cut,
        ActionStatus::Done => JournalResult::Response(serde_json::from_str(action.result?).ok()?),
        ActionStatus::Failed => JournalResult::Failure(action.error?.parse().ok()?),
        ActionStatus::Running
        | ActionStatus::Refused
        | ActionStatus::Queued
        | ActionStatus::Delivered
        | ActionStatus::Deduplicated
        | ActionStatus::Dropped
        | ActionStatus::Expired
        | ActionStatus::Interrupted => return None,
    };
    Some(JournalEntry {
        request_hash: action.request_hash.to_owned(),
        result,
    })
}

impl AutomationHost for DryRunHost<'_> {
    fn now_ms(&self) -> i64 {
        self.now_ms
    }

    fn interrupted(&self) -> Option<Interruption> {
        None
    }

    fn admit(&self) -> Result<(), LimitRefusal> {
        Ok(())
    }

    fn act(&self, site: CallSite, request: ActionRequest) -> HostResult<ActionReply> {
        let (answer, reply) = self.answer(&request);
        self.actions.borrow_mut().push(DryAction {
            site,
            request,
            answer,
        });
        reply.map_err(HostError::Failure)
    }
}

impl DryRunHost<'_> {
    fn answer(&self, request: &ActionRequest) -> (Answer, Result<ActionReply, Failure>) {
        let reply = match request {
            ActionRequest::Http(_) => {
                return match self.journaled(request) {
                    Some(JournalResult::Response(response)) => {
                        (Answer::Journal, Ok(ActionReply::Http(response)))
                    }
                    Some(JournalResult::Failure(failure)) => (Answer::Journal, Err(failure)),
                    Some(JournalResult::Cut) => (Answer::Cut, Ok(stub_reply())),
                    None => (Answer::Stubbed, Ok(stub_reply())),
                };
            }
            ActionRequest::SetGoal(goal) => ActionReply::GoalSet(GoalSet {
                condition: goal.condition.clone(),
            }),
            ActionRequest::Reply { .. } | ActionRequest::Send(_) => {
                ActionReply::Sent(SendReceipt {
                    status: SendStatus::Queued,
                    message_id: DRY_MESSAGE_ID.to_owned(),
                    reason: None,
                })
            }
            ActionRequest::Publish { .. } => published(Audience::Topic),
            ActionRequest::Broadcast { .. } => published(Audience::Broadcast),
            ActionRequest::StartWorkflow(run) => ActionReply::WorkflowStarted(WorkflowStarted {
                run_id: DRY_RUN_ID.to_owned(),
                name: run.name.clone(),
            }),
            ActionRequest::Message(_)
            | ActionRequest::Notify { .. }
            | ActionRequest::Pause { .. }
            | ActionRequest::Log { .. } => ActionReply::Done,
        };
        (Answer::Recorded, Ok(reply))
    }

    /// The first unmatched entry journaled for the same request, which it then answers no more.
    fn journaled(&self, request: &ActionRequest) -> Option<JournalResult> {
        let hash = request_hash(&request.to_journal());
        let mut unmatched = self.unmatched.borrow_mut();
        let index = unmatched
            .iter()
            .position(|entry| entry.request_hash == hash)?;
        Some(unmatched.remove(index).result.clone())
    }
}

fn stub_reply() -> ActionReply {
    ActionReply::Http(HttpResponse {
        status: STUB_STATUS,
        body: String::new(),
        json: None,
    })
}

fn published(audience: Audience) -> ActionReply {
    ActionReply::Published(PublishReceipt {
        message_id: DRY_MESSAGE_ID.to_owned(),
        audience,
        recipients: Vec::new(),
        skipped: 0,
        queued: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::engine::{ErrorKind, FiringEnd, FiringError, StopKind};
    use crate::event::{EventDetail, SessionStatus, SessionView, WorkView};
    use crate::host::{FailureKind, HttpMethod, HttpRequest, HttpTarget, MAX_HTTP_TIMEOUT};
    use crate::limits::LimitReason;
    use crate::meta::parse_meta;
    use crate::untrusted::{UNTRUSTED_TAG, Untrusted};

    const HEADER: &str = concat!(
        r#"let meta = #{ name: "replayed", description: "Runs a journaled firing again", "#,
        r#"triggers: [#{ kind: "schedule", every: "10m" }], network: ["https://api.example.com"], "#,
        r#"messaging: #{ send: ["@peer-*"], publish: ["swarm.status", "broadcast"] }, "#,
        r#"workflows: ["review-changes"] };"#,
        "\n",
    );
    const NOW_MS: i64 = 1_791_189_000_000;
    const AT: i64 = 1_791_189_000;
    const FIRE_ID: &str = "fire-1";
    const SESSION_ID: &str = "session-1";
    const MODE: &str = "build";
    const TITLE: &str = "CI watcher";
    const API_URL: &str = "https://api.example.com/v1/runs";
    const OTHER_URL: &str = "https://api.example.com/v1/jobs";
    const OK_STATUS: u16 = 200;
    const UNAVAILABLE_STATUS: u16 = 503;
    const RESPONSE_BODY: &str = r#"{"state":"failure"}"#;
    const VERDICT: &str = "failure";
    const TIMED_OUT: &str = "the request timed out after 30s";
    const TIMED_OUT_ERROR: &str = "timeout: the request timed out after 30s";
    const REQUEST_HASH: &str = "request-hash-1";
    const STORED_RESPONSE: &str =
        r#"{"status":200,"body":"{\"state\":\"failure\"}","json":{"state":"failure"}}"#;
    const RESPONSE_PREVIEW: &str = r#""{\"status\":200,\"body\":""#;
    const NOT_A_RESPONSE: &str = r#"{"status":"ok"}"#;
    const GOAL: &str = "The build passes";
    const LIMITED_UNTIL: i64 = NOW_MS + 60_000;
    const GET_RUNS: &str = r#"http(#{ method: "GET", url: "https://api.example.com/v1/runs" })"#;
    const MUST_PARSE: &str = "the header parses";
    const MUST_COMMIT: &str = "the dry run would commit a change";
    const EXPECTED_STOP: &str = "expected a stopped dry run";

    fn get(url: &str) -> ActionRequest {
        ActionRequest::Http(HttpRequest {
            method: HttpMethod::Get,
            target: HttpTarget::Url(url.to_owned()),
            query: BTreeMap::new(),
            headers: BTreeMap::new(),
            bearer_env: None,
            secret_headers: BTreeMap::new(),
            payload: None,
            timeout: MAX_HTTP_TIMEOUT,
        })
    }

    fn journaled(request: &ActionRequest, result: JournalResult) -> JournalEntry {
        JournalEntry {
            request_hash: request_hash(&request.to_journal()),
            result,
        }
    }

    fn stored_response() -> StoredAction<'static> {
        StoredAction {
            kind: ActionKind::Http,
            status: ActionStatus::Done,
            request_hash: REQUEST_HASH,
            result: Some(STORED_RESPONSE),
            result_cut: false,
            error: None,
        }
    }

    fn stored_failure() -> StoredAction<'static> {
        StoredAction {
            status: ActionStatus::Failed,
            result: None,
            error: Some(TIMED_OUT_ERROR),
            ..stored_response()
        }
    }

    fn response(status: u16) -> HttpResponse {
        HttpResponse {
            status,
            body: RESPONSE_BODY.to_owned(),
            json: Some(json!({ "state": VERDICT })),
        }
    }

    fn refusal() -> LimitRefusal {
        LimitRefusal {
            reason: LimitReason::Cooldown,
            until: LIMITED_UNTIL,
        }
    }

    fn scheduled() -> Event {
        Event {
            fire_id: FIRE_ID.to_owned(),
            at: AT,
            session: SessionView {
                id: SESSION_ID.to_owned(),
                title: Untrusted::text(TITLE),
                name: None,
                mode: MODE.to_owned(),
                status: SessionStatus::Idle,
                status_since: AT,
                goal: None,
                cost: None,
                groups: Vec::new(),
                work: WorkView::default(),
            },
            detail: EventDetail::Schedule {
                scheduled_for: AT,
                late_by_s: 0,
            },
        }
    }

    fn replay(
        body: &str,
        journal: &[JournalEntry],
        admission: Result<(), LimitRefusal>,
        limits: &FiringLimits,
    ) -> DryRunReport {
        let meta = parse_meta(HEADER).expect(MUST_PARSE);
        dry_run(DryRun {
            source: &format!("{HEADER}{body}"),
            meta: &meta,
            event: &scheduled(),
            state: &json!({}),
            args: &Map::new(),
            limits,
            now_ms: NOW_MS,
            journal,
            admission,
        })
    }

    fn replayed(body: &str, journal: &[JournalEntry]) -> DryRunReport {
        replay(body, journal, Ok(()), &FiringLimits::default())
    }

    fn answers(report: &DryRunReport) -> Vec<(ActionRequest, Answer)> {
        report
            .actions
            .iter()
            .map(|action| (action.request.clone(), action.answer))
            .collect()
    }

    fn committed(report: DryRunReport) -> Value {
        report.outcome.state.expect(MUST_COMMIT).state
    }

    #[test]
    fn http_answers_from_the_journal_when_the_request_matches() {
        let journal = [journaled(
            &get(API_URL),
            JournalResult::Response(response(OK_STATUS)),
        )];
        let body = format!(
            "let response = {GET_RUNS}; state.status = response.status; state.verdict = response.json.state;"
        );
        let report = replayed(&body, &journal);
        assert_eq!(answers(&report), vec![(get(API_URL), Answer::Journal)]);
        assert_eq!(
            committed(report),
            json!({ "status": OK_STATUS, "verdict": { UNTRUSTED_TAG: VERDICT } })
        );
    }

    #[test]
    fn a_request_the_journal_does_not_hold_is_stubbed() {
        let journal = [journaled(
            &get(OTHER_URL),
            JournalResult::Response(response(OK_STATUS)),
        )];
        let body = format!(
            "let response = {GET_RUNS}; state.status = response.status; state.body = response.body; state.parsed = response.json != ();"
        );
        let report = replayed(&body, &journal);
        assert_eq!(answers(&report), vec![(get(API_URL), Answer::Stubbed)]);
        assert_eq!(
            committed(report),
            json!({ "status": STUB_STATUS, "body": { UNTRUSTED_TAG: "" }, "parsed": false })
        );
    }

    #[test]
    fn each_journal_entry_answers_one_request_in_order() {
        let journal = [
            journaled(&get(API_URL), JournalResult::Response(response(OK_STATUS))),
            journaled(
                &get(API_URL),
                JournalResult::Response(response(UNAVAILABLE_STATUS)),
            ),
        ];
        let body =
            format!("state.statuses = [{GET_RUNS}.status, {GET_RUNS}.status, {GET_RUNS}.status];");
        let report = replayed(&body, &journal);
        assert_eq!(
            answers(&report),
            vec![
                (get(API_URL), Answer::Journal),
                (get(API_URL), Answer::Journal),
                (get(API_URL), Answer::Stubbed),
            ]
        );
        assert_eq!(
            committed(report),
            json!({ "statuses": [OK_STATUS, UNAVAILABLE_STATUS, STUB_STATUS] })
        );
    }

    #[test]
    fn a_journaled_failure_is_thrown_again() {
        let journal = [journaled(
            &get(API_URL),
            JournalResult::Failure(Failure::new(FailureKind::Timeout, TIMED_OUT)),
        )];
        let body = format!("try {{ {GET_RUNS}; }} catch (err) {{ state.kind = err.kind; }}");
        let report = replayed(&body, &journal);
        assert_eq!(answers(&report), vec![(get(API_URL), Answer::Journal)]);
        assert_eq!(
            committed(report),
            json!({ "kind": FailureKind::Timeout.as_str() })
        );
    }

    #[test]
    fn a_cut_response_answers_once_with_the_stub_marked_cut() {
        let journal = [journaled(&get(API_URL), JournalResult::Cut)];
        let body = format!(
            "let response = {GET_RUNS}; state.status = response.status; state.body = response.body; state.parsed = response.json != (); state.again = {GET_RUNS}.status;"
        );
        let report = replayed(&body, &journal);
        assert_eq!(
            answers(&report),
            vec![(get(API_URL), Answer::Cut), (get(API_URL), Answer::Stubbed)]
        );
        assert_eq!(
            committed(report),
            json!({ "status": STUB_STATUS, "body": { UNTRUSTED_TAG: "" }, "parsed": false, "again": STUB_STATUS })
        );
    }

    #[test_case(stored_response() => Some(JournalResult::Response(response(OK_STATUS))); "done")]
    #[test_case(stored_failure() => Some(JournalResult::Failure(Failure::new(FailureKind::Timeout, TIMED_OUT))); "failed")]
    #[test_case(StoredAction { result: Some(RESPONSE_PREVIEW), result_cut: true, ..stored_response() } => Some(JournalResult::Cut); "cut")]
    #[test_case(StoredAction { status: ActionStatus::Refused, error: Some(TIMED_OUT_ERROR), ..stored_response() } => None; "refused")]
    #[test_case(StoredAction { status: ActionStatus::Interrupted, error: Some(TIMED_OUT_ERROR), ..stored_response() } => None; "interrupted")]
    #[test_case(StoredAction { kind: ActionKind::Notify, ..stored_response() } => None; "not_http")]
    #[test_case(StoredAction { result: Some(NOT_A_RESPONSE), ..stored_response() } => None; "malformed_result")]
    #[test_case(StoredAction { error: Some(TIMED_OUT), ..stored_failure() } => None; "malformed_error")]
    #[test_case(StoredAction { result: None, ..stored_response() } => None; "done_without_a_result")]
    #[test_case(StoredAction { error: None, ..stored_failure() } => None; "failed_without_an_error")]
    fn stored_actions_become_journal_entries(action: StoredAction<'_>) -> Option<JournalResult> {
        let entry = journal_entry(action)?;
        assert_eq!(entry.request_hash, REQUEST_HASH);
        Some(entry.result)
    }

    #[test]
    fn effects_are_recorded_with_placeholder_replies() {
        let report = replayed(
            r#"message("Fix the build");
            let goal = set_goal("The build passes");
            let sent = send("@peer-1", "hi");
            let published = publish("swarm.status", "online");
            let broadcast = broadcast("online");
            let run = start_workflow("review-changes", #{});
            notify("replayed");
            pause_automations("enough");
            log("done");
            state.ids = [goal.condition, sent.message_id, published.message_id, broadcast.audience, run.run_id];"#,
            &[],
        );
        assert_eq!(
            report
                .actions
                .iter()
                .map(|action| (action.request.kind(), action.answer))
                .collect::<Vec<_>>(),
            [
                ActionKind::Message,
                ActionKind::SetGoal,
                ActionKind::Send,
                ActionKind::Publish,
                ActionKind::Broadcast,
                ActionKind::StartWorkflow,
                ActionKind::Notify,
                ActionKind::Pause,
                ActionKind::Log,
            ]
            .map(|kind| (kind, Answer::Recorded))
        );
        assert_eq!(
            committed(report),
            json!({ "ids": [GOAL, DRY_MESSAGE_ID, DRY_MESSAGE_ID, Audience::Broadcast, DRY_RUN_ID] })
        );
    }

    #[test]
    fn now_answers_the_original_firing_time() {
        let report = replayed("state.unix = now().unix;", &[]);
        assert_eq!(committed(report), json!({ "unix": AT }));
    }

    #[test_case(Err(refusal()), r#"notify("replayed");"# => Some(refusal()); "a_limit_on_an_acting_firing")]
    #[test_case(Err(refusal()), r#"log("replayed");"# => None; "a_limit_on_a_firing_that_does_not_act")]
    #[test_case(Ok(()), r#"notify("replayed");"# => None; "no_limit")]
    fn automation_limits_are_reported_not_enforced(
        admission: Result<(), LimitRefusal>,
        body: &str,
    ) -> Option<LimitRefusal> {
        let report = replay(body, &[], admission, &FiringLimits::default());
        assert_eq!(report.outcome.end, FiringEnd::Completed);
        assert_eq!(report.actions.len(), 1);
        report.limited
    }

    #[test_case(FiringLimits::default(), r#"send("@stranger", "hi");"# => (StopKind::Capability, 0); "capabilities")]
    #[test_case(FiringLimits { max_actions: 1, ..FiringLimits::default() }, r#"notify("one"); notify("two");"# => (StopKind::FiringLimit, 1); "per_firing_limits")]
    fn policy_still_applies(limits: FiringLimits, body: &str) -> (StopKind, usize) {
        let report = replay(body, &[], Ok(()), &limits);
        let FiringEnd::Stopped(FiringError {
            kind: ErrorKind::Stop(kind),
            ..
        }) = report.outcome.end
        else {
            panic!("{EXPECTED_STOP}: {:?}", report.outcome.end);
        };
        (kind, report.actions.len())
    }
}
