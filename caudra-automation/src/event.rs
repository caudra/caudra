//! The events automations react to. Serialised, an event is the tagged JSON a firing stores and
//! the `event` map a script reads: untrusted fields are `{"$untrusted": value}` and absent ones
//! are `null`, which scripts see as `()`.

use rhai::Dynamic;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::{StateError, from_tagged};
use crate::untrusted::Untrusted;

/// The most of a turn's final response an `idle` event carries.
pub const MAX_LAST_RESPONSE_BYTES: usize = 32 * 1024;
/// The most of a workflow report a `workflow_finished` event carries.
pub const MAX_REPORT_BYTES: usize = 256 * 1024;
/// The largest compact tagged JSON a stored event may take.
pub const MAX_EVENT_BYTES: usize = 64 * 1024;
/// Ends text that was cut to fit a limit.
pub const CUT_MARKER: &str = "…[truncated]";
const PLAIN_JSON: &str = "an event is plain JSON data";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub fire_id: String,
    /// Unix seconds.
    pub at: i64,
    pub session: SessionView,
    #[serde(flatten)]
    pub detail: EventDetail,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "trigger", rename_all = "snake_case")]
pub enum EventDetail {
    Armed {
        reason: ArmedReason,
    },
    Idle(IdleDetail),
    NeedsInput {
        input: InputKind,
        /// The tool a permission prompt is for.
        tool: Option<String>,
        waiting_s: u64,
    },
    GoalFinished(GoalFinishedDetail),
    MessageReceived(MessageDetail),
    WorkFinished(WorkFinishedDetail),
    WorkflowFinished(WorkflowFinishedDetail),
    Schedule {
        /// Unix seconds.
        scheduled_for: i64,
        late_by_s: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionView {
    pub id: String,
    pub title: Untrusted,
    /// The session's `@name`, absent while messaging is off.
    pub name: Option<String>,
    pub mode: String,
    /// As peers see it: held messages count as `needs_input`, a ready plan does not.
    pub status: SessionStatus,
    /// Unix seconds.
    pub status_since: i64,
    pub goal: Option<GoalView>,
    /// USD.
    pub cost: Option<f64>,
    pub groups: Vec<String>,
    pub work: WorkView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalView {
    pub condition: String,
    pub evaluations: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkView {
    pub held: Option<HeldWork>,
    /// Items this session paused that still wait for an outcome.
    pub paused: Vec<PausedWork>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldWork {
    pub group: String,
    pub work: String,
    pub attempt: u32,
    pub max_attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PausedWork {
    pub group: String,
    pub work: String,
    pub pause_reason: PauseReason,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IdleDetail {
    pub outcome: TurnOutcome,
    pub error_kind: Option<ErrorKind>,
    pub error: Option<Untrusted>,
    pub started_by: StartedBy,
    /// Automations whose messages were delivered during the busy period.
    pub automations: Vec<String>,
    pub runs: u32,
    pub busy_s: u64,
    /// USD.
    pub cost: Option<f64>,
    /// Group work outcomes during the busy period.
    pub work: Vec<WorkReport>,
    pub last_response: Untrusted,
}

/// The first run of a busy period.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StartedBy {
    User,
    Automation {
        automation: String,
        fire_id: String,
    },
    Peer {
        message_id: String,
        sender: Option<String>,
        sender_kind: SenderKind,
        audience: Audience,
        topic: Option<String>,
    },
    Work {
        group: String,
        work: String,
        attempt: u32,
        max_attempts: u32,
        message_id: String,
        topic: Option<String>,
    },
    Background,
    Workflow,
    Goal,
    Mailbox,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkReport {
    pub group: String,
    pub work: String,
    pub outcome: WorkOutcome,
    pub pause_reason: Option<PauseReason>,
    /// The agent's summary or reason.
    pub detail: Option<Untrusted>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoalFinishedDetail {
    pub verdict: GoalVerdict,
    pub condition: String,
    /// The evaluator's reason, or the error that cleared the goal.
    pub reason: Untrusted,
    pub evaluations: u32,
    pub duration_s: u64,
    /// USD.
    pub cost: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageDetail {
    pub message_id: String,
    pub audience: Audience,
    pub topic: Option<String>,
    pub sender_kind: SenderKind,
    /// The sender's `@name`, absent for a script.
    pub sender: Option<String>,
    pub sender_automation: Option<String>,
    pub sender_label: Option<Untrusted>,
    pub sender_title: Option<Untrusted>,
    pub sender_cwd: Option<Untrusted>,
    pub text: Untrusted,
    pub reply_to: Option<String>,
    pub admission: Admission,
    pub delivery: Delivery,
    /// Whether this automation took the message instead of the model.
    pub consumed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkFinishedDetail {
    pub group: String,
    pub work: String,
    pub message_id: String,
    pub topic: Option<String>,
    pub state: WorkState,
    pub attempts: u32,
    pub max_attempts: u32,
    /// The last owner's `@name`.
    pub member: Option<String>,
    pub pause_reason: Option<PauseReason>,
    pub detail: Option<Untrusted>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowFinishedDetail {
    pub run_id: String,
    pub name: String,
    pub workflow: String,
    pub status: WorkflowStatus,
    pub report: Option<Untrusted>,
    pub result: Option<Untrusted>,
    pub error: Option<Untrusted>,
    pub scratch_dir: Option<String>,
    pub agents: u32,
    pub tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArmedReason {
    Launch,
    Resume,
    Manual,
    Unpaused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Working,
    NeedsInput,
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    Error,
    Cancelled,
    MaxTurns,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    RateLimit,
    Overloaded,
    Auth,
    Timeout,
    Network,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Permission,
    Question,
    Plan,
    Auth,
    Plugin,
    Messages,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalVerdict {
    Met,
    Impossible,
    Cleared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    Direct,
    Topic,
    Broadcast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Admission {
    Queued,
    Held,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Live,
    CatchUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderKind {
    Session,
    Automation,
    Script,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkState {
    Completed,
    Failed,
    Cancelled,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkOutcome {
    Completed,
    Retry,
    Failed,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    CompletionRequired,
    Cancelled,
    TurnLimit,
    TurnFailed,
    SessionClosed,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl Event {
    pub fn to_tagged(&self) -> Value {
        serde_json::to_value(self).expect(PLAIN_JSON)
    }

    /// The `event` map a script reads.
    pub fn to_dynamic(&self) -> Result<Dynamic, StateError> {
        from_tagged(&self.to_tagged())
    }

    /// Cuts the largest untrusted texts, reading a structured `result` as its JSON text, until
    /// the compact tagged JSON takes at most `max_bytes`. Returns whether anything was cut.
    pub fn fit(&mut self, max_bytes: usize) -> bool {
        let mut cut = false;
        loop {
            let excess = self.to_tagged().to_string().len().saturating_sub(max_bytes);
            if excess == 0 {
                return cut;
            }
            let Some(field) = self
                .untrusted_texts_mut()
                .into_iter()
                .max_by_key(|field| field.to_text().len())
            else {
                return cut;
            };
            let text = field.to_text().into_owned();
            if text.len() <= CUT_MARKER.len() {
                return cut;
            }
            let keep = text.len().saturating_sub(excess);
            *field = Untrusted::Text(cap_text(text, keep).0);
            cut = true;
        }
    }

    fn untrusted_texts_mut(&mut self) -> Vec<&mut Untrusted> {
        let mut texts = vec![&mut self.session.title];
        match &mut self.detail {
            EventDetail::Idle(idle) => {
                texts.push(&mut idle.last_response);
                texts.extend(idle.error.as_mut());
                texts.extend(
                    idle.work
                        .iter_mut()
                        .filter_map(|report| report.detail.as_mut()),
                );
            }
            EventDetail::GoalFinished(goal) => texts.push(&mut goal.reason),
            EventDetail::MessageReceived(message) => {
                texts.push(&mut message.text);
                texts.extend(
                    [
                        message.sender_label.as_mut(),
                        message.sender_title.as_mut(),
                        message.sender_cwd.as_mut(),
                    ]
                    .into_iter()
                    .flatten(),
                );
            }
            EventDetail::WorkFinished(work) => texts.extend(work.detail.as_mut()),
            EventDetail::WorkflowFinished(run) => texts.extend(
                [run.report.as_mut(), run.result.as_mut(), run.error.as_mut()]
                    .into_iter()
                    .flatten(),
            ),
            EventDetail::Armed { .. }
            | EventDetail::NeedsInput { .. }
            | EventDetail::Schedule { .. } => {}
        }
        texts
    }
}

/// `text` within `max` bytes, cut at a char boundary and ended with [`CUT_MARKER`] when it is
/// longer, and whether it was cut.
pub fn cap_text(mut text: String, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text, false);
    }
    text.truncate(text.floor_char_boundary(max.saturating_sub(CUT_MARKER.len())));
    text.push_str(CUT_MARKER);
    (text, true)
}

#[cfg(test)]
mod tests {
    use rhai::Map as ScriptMap;
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::untrusted::UNTRUSTED_TAG;

    const FIRE_ID: &str = "fire-1";
    const AT: i64 = 1_760_000_000;
    const SESSION_ID: &str = "session-1";
    const TITLE: &str = "Fix the flaky nightly build";
    const NAME: &str = "builder";
    const MODE: &str = "build";
    const CONDITION: &str = "the nightly build passes";
    const GROUP: &str = "ci";
    const WORK: &str = "triage-42";
    const MESSAGE_ID: &str = "message-1";
    const SENDER: &str = "librarian";
    const AUTOMATION: &str = "ci-triage";
    const TOPIC: &str = "ci/failures";
    const TOOL: &str = "shell";
    const ERROR: &str = "rate limited";
    const DETAIL: &str = "needs a human";
    const RESPONSE: &str = "done";
    const REASON: &str = "the runner is offline";
    const CWD: &str = "/srv/ci";
    const TEXT: &str = "build 812 failed";
    const RUN_ID: &str = "run-1";
    const WORKFLOW: &str = "triage";
    const REPORT: &str = "two flaky tests";
    const SCRATCH: &str = "/tmp/run-1";
    const COUNT: u32 = 2;
    const SECONDS: u64 = 30;
    const TOKENS: u64 = 1_200;
    const COST: f64 = 0.25;
    const FILLER: &str = "x";
    const COMMON_FIELDS: [&str; 3] = ["fire_id", "at", "session"];
    const MUST_PARSE: &str = "a stored event parses";
    const MUST_CONVERT: &str = "an event converts";
    const MUST_SERIALIZE: &str = "a value serializes";
    const NOT_A_MAP: &str = "expected a map";
    const NOT_TEXT: &str = "expected untrusted text";

    fn session() -> SessionView {
        SessionView {
            id: SESSION_ID.to_owned(),
            title: Untrusted::text(TITLE),
            name: Some(NAME.to_owned()),
            mode: MODE.to_owned(),
            status: SessionStatus::NeedsInput,
            status_since: AT,
            goal: Some(GoalView {
                condition: CONDITION.to_owned(),
                evaluations: COUNT,
            }),
            cost: Some(COST),
            groups: vec![GROUP.to_owned()],
            work: WorkView {
                held: Some(HeldWork {
                    group: GROUP.to_owned(),
                    work: WORK.to_owned(),
                    attempt: COUNT,
                    max_attempts: COUNT,
                }),
                paused: vec![PausedWork {
                    group: GROUP.to_owned(),
                    work: WORK.to_owned(),
                    pause_reason: PauseReason::CompletionRequired,
                }],
            },
        }
    }

    fn event(detail: EventDetail) -> Event {
        Event {
            fire_id: FIRE_ID.to_owned(),
            at: AT,
            session: session(),
            detail,
        }
    }

    fn armed_detail() -> EventDetail {
        EventDetail::Armed {
            reason: ArmedReason::Unpaused,
        }
    }

    fn idle_detail() -> EventDetail {
        idle_with(Untrusted::text(RESPONSE))
    }

    fn idle_with(last_response: Untrusted) -> EventDetail {
        EventDetail::Idle(IdleDetail {
            outcome: TurnOutcome::MaxTurns,
            error_kind: Some(ErrorKind::RateLimit),
            error: Some(Untrusted::text(ERROR)),
            started_by: StartedBy::Peer {
                message_id: MESSAGE_ID.to_owned(),
                sender: Some(SENDER.to_owned()),
                sender_kind: SenderKind::Session,
                audience: Audience::Topic,
                topic: Some(TOPIC.to_owned()),
            },
            automations: vec![AUTOMATION.to_owned()],
            runs: COUNT,
            busy_s: SECONDS,
            cost: Some(COST),
            work: vec![WorkReport {
                group: GROUP.to_owned(),
                work: WORK.to_owned(),
                outcome: WorkOutcome::Paused,
                pause_reason: Some(PauseReason::TurnLimit),
                detail: Some(Untrusted::text(DETAIL)),
            }],
            last_response,
        })
    }

    fn needs_input_detail() -> EventDetail {
        EventDetail::NeedsInput {
            input: InputKind::Permission,
            tool: Some(TOOL.to_owned()),
            waiting_s: SECONDS,
        }
    }

    fn goal_finished_detail() -> EventDetail {
        EventDetail::GoalFinished(GoalFinishedDetail {
            verdict: GoalVerdict::Impossible,
            condition: CONDITION.to_owned(),
            reason: Untrusted::text(REASON),
            evaluations: COUNT,
            duration_s: SECONDS,
            cost: None,
        })
    }

    fn message_received_detail() -> EventDetail {
        EventDetail::MessageReceived(MessageDetail {
            message_id: MESSAGE_ID.to_owned(),
            audience: Audience::Broadcast,
            topic: None,
            sender_kind: SenderKind::Automation,
            sender: Some(SENDER.to_owned()),
            sender_automation: Some(AUTOMATION.to_owned()),
            sender_label: None,
            sender_title: Some(Untrusted::text(TITLE)),
            sender_cwd: Some(Untrusted::text(CWD)),
            text: Untrusted::text(TEXT),
            reply_to: Some(MESSAGE_ID.to_owned()),
            admission: Admission::Held,
            delivery: Delivery::CatchUp,
            consumed: true,
        })
    }

    fn work_finished_detail() -> EventDetail {
        EventDetail::WorkFinished(WorkFinishedDetail {
            group: GROUP.to_owned(),
            work: WORK.to_owned(),
            message_id: MESSAGE_ID.to_owned(),
            topic: Some(TOPIC.to_owned()),
            state: WorkState::Paused,
            attempts: COUNT,
            max_attempts: COUNT,
            member: Some(SENDER.to_owned()),
            pause_reason: Some(PauseReason::SessionClosed),
            detail: Some(Untrusted::text(DETAIL)),
        })
    }

    fn workflow_finished_detail() -> EventDetail {
        workflow_finished_with(Untrusted::Json(json!({"passed": true})))
    }

    fn workflow_finished_with(result: Untrusted) -> EventDetail {
        EventDetail::WorkflowFinished(WorkflowFinishedDetail {
            run_id: RUN_ID.to_owned(),
            name: WORKFLOW.to_owned(),
            workflow: WORKFLOW.to_owned(),
            status: WorkflowStatus::Interrupted,
            report: Some(Untrusted::text(REPORT)),
            result: Some(result),
            error: None,
            scratch_dir: Some(SCRATCH.to_owned()),
            agents: COUNT,
            tokens: TOKENS,
        })
    }

    fn schedule_detail() -> EventDetail {
        EventDetail::Schedule {
            scheduled_for: AT,
            late_by_s: SECONDS,
        }
    }

    #[test]
    fn common_fields_use_the_plan_names() {
        let tagged = event(armed_detail()).to_tagged();
        assert_eq!(tagged["fire_id"], FIRE_ID);
        assert_eq!(tagged["at"], AT);
        assert_eq!(
            tagged["session"],
            json!({
                "id": SESSION_ID,
                "title": { UNTRUSTED_TAG: TITLE },
                "name": NAME,
                "mode": MODE,
                "status": "needs_input",
                "status_since": AT,
                "goal": {"condition": CONDITION, "evaluations": COUNT},
                "cost": COST,
                "groups": [GROUP],
                "work": {
                    "held": {"group": GROUP, "work": WORK, "attempt": COUNT, "max_attempts": COUNT},
                    "paused": [{"group": GROUP, "work": WORK, "pause_reason": "completion_required"}],
                },
            })
        );
    }

    #[test_case(armed_detail() => json!({"trigger": "armed", "reason": "unpaused"}); "armed")]
    #[test_case(idle_detail() => json!({
        "trigger": "idle",
        "outcome": "max_turns",
        "error_kind": "rate_limit",
        "error": { UNTRUSTED_TAG: ERROR },
        "started_by": {
            "kind": "peer",
            "message_id": MESSAGE_ID,
            "sender": SENDER,
            "sender_kind": "session",
            "audience": "topic",
            "topic": TOPIC,
        },
        "automations": [AUTOMATION],
        "runs": COUNT,
        "busy_s": SECONDS,
        "cost": COST,
        "work": [{
            "group": GROUP,
            "work": WORK,
            "outcome": "paused",
            "pause_reason": "turn_limit",
            "detail": { UNTRUSTED_TAG: DETAIL },
        }],
        "last_response": { UNTRUSTED_TAG: RESPONSE },
    }); "idle")]
    #[test_case(needs_input_detail() => json!({
        "trigger": "needs_input",
        "input": "permission",
        "tool": TOOL,
        "waiting_s": SECONDS,
    }); "needs_input")]
    #[test_case(goal_finished_detail() => json!({
        "trigger": "goal_finished",
        "verdict": "impossible",
        "condition": CONDITION,
        "reason": { UNTRUSTED_TAG: REASON },
        "evaluations": COUNT,
        "duration_s": SECONDS,
        "cost": null,
    }); "goal_finished")]
    #[test_case(message_received_detail() => json!({
        "trigger": "message_received",
        "message_id": MESSAGE_ID,
        "audience": "broadcast",
        "topic": null,
        "sender_kind": "automation",
        "sender": SENDER,
        "sender_automation": AUTOMATION,
        "sender_label": null,
        "sender_title": { UNTRUSTED_TAG: TITLE },
        "sender_cwd": { UNTRUSTED_TAG: CWD },
        "text": { UNTRUSTED_TAG: TEXT },
        "reply_to": MESSAGE_ID,
        "admission": "held",
        "delivery": "catch_up",
        "consumed": true,
    }); "message_received")]
    #[test_case(work_finished_detail() => json!({
        "trigger": "work_finished",
        "group": GROUP,
        "work": WORK,
        "message_id": MESSAGE_ID,
        "topic": TOPIC,
        "state": "paused",
        "attempts": COUNT,
        "max_attempts": COUNT,
        "member": SENDER,
        "pause_reason": "session_closed",
        "detail": { UNTRUSTED_TAG: DETAIL },
    }); "work_finished")]
    #[test_case(workflow_finished_detail() => json!({
        "trigger": "workflow_finished",
        "run_id": RUN_ID,
        "name": WORKFLOW,
        "workflow": WORKFLOW,
        "status": "interrupted",
        "report": { UNTRUSTED_TAG: REPORT },
        "result": { UNTRUSTED_TAG: {"passed": true} },
        "error": null,
        "scratch_dir": SCRATCH,
        "agents": COUNT,
        "tokens": TOKENS,
    }); "workflow_finished")]
    #[test_case(schedule_detail() => json!({
        "trigger": "schedule",
        "scheduled_for": AT,
        "late_by_s": SECONDS,
    }); "schedule")]
    fn detail_fields_use_the_plan_names(detail: EventDetail) -> Value {
        let mut tagged = event(detail).to_tagged();
        let fields = tagged.as_object_mut().expect(NOT_A_MAP);
        for common in COMMON_FIELDS {
            assert!(fields.shift_remove(common).is_some(), "{common}");
        }
        tagged
    }

    #[test_case(StartedBy::User => json!({"kind": "user"}); "user")]
    #[test_case(StartedBy::Automation {
        automation: AUTOMATION.to_owned(),
        fire_id: FIRE_ID.to_owned(),
    } => json!({"kind": "automation", "automation": AUTOMATION, "fire_id": FIRE_ID}); "automation")]
    #[test_case(StartedBy::Work {
        group: GROUP.to_owned(),
        work: WORK.to_owned(),
        attempt: COUNT,
        max_attempts: COUNT,
        message_id: MESSAGE_ID.to_owned(),
        topic: None,
    } => json!({
        "kind": "work",
        "group": GROUP,
        "work": WORK,
        "attempt": COUNT,
        "max_attempts": COUNT,
        "message_id": MESSAGE_ID,
        "topic": null,
    }); "work")]
    #[test_case(StartedBy::Background => json!({"kind": "background"}); "background")]
    #[test_case(StartedBy::Workflow => json!({"kind": "workflow"}); "workflow")]
    #[test_case(StartedBy::Goal => json!({"kind": "goal"}); "goal")]
    #[test_case(StartedBy::Mailbox => json!({"kind": "mailbox"}); "mailbox")]
    fn started_by_uses_the_plan_names(started_by: StartedBy) -> Value {
        serde_json::to_value(started_by).expect(MUST_SERIALIZE)
    }

    #[test_case(armed_detail(); "armed")]
    #[test_case(idle_detail(); "idle")]
    #[test_case(needs_input_detail(); "needs_input")]
    #[test_case(goal_finished_detail(); "goal_finished")]
    #[test_case(message_received_detail(); "message_received")]
    #[test_case(work_finished_detail(); "work_finished")]
    #[test_case(workflow_finished_detail(); "workflow_finished")]
    #[test_case(schedule_detail(); "schedule")]
    fn stored_events_round_trip(detail: EventDetail) {
        let event = event(detail);
        let stored = event.to_tagged().to_string();
        assert_eq!(
            serde_json::from_str::<Event>(&stored).expect(MUST_PARSE),
            event
        );
    }

    #[test]
    fn script_view_marks_untrusted_and_absent_fields() {
        let view = event(message_received_detail())
            .to_dynamic()
            .expect(MUST_CONVERT)
            .try_cast::<ScriptMap>()
            .expect(NOT_A_MAP);
        for untrusted in ["text", "sender_title", "sender_cwd"] {
            assert!(view[untrusted].is::<Untrusted>(), "{untrusted}");
        }
        for absent in ["topic", "sender_label"] {
            assert!(view[absent].is_unit(), "{absent}");
        }
        assert_eq!(
            view["trigger"].clone().into_string().as_deref(),
            Ok("message_received")
        );
        assert_eq!(view["at"].as_int(), Ok(AT));
        assert_eq!(view["consumed"].as_bool(), Ok(true));
        let session = view["session"].read_lock::<ScriptMap>().expect(NOT_A_MAP);
        assert!(session["title"].is::<Untrusted>());
        assert!(session["goal"].is::<ScriptMap>());
    }

    #[test_case(FILLER; "plain")]
    #[test_case("\""; "escaped_quote")]
    #[test_case("\u{1}"; "escaped_control")]
    #[test_case("é"; "multibyte")]
    fn fit_cuts_the_largest_untrusted_text(filler: &str) {
        let mut event = event(idle_with(Untrusted::text(filler.repeat(MAX_EVENT_BYTES))));
        assert!(event.fit(MAX_EVENT_BYTES));
        let tagged = event.to_tagged();
        assert!(tagged.to_string().len() <= MAX_EVENT_BYTES);
        let last_response = tagged["last_response"][UNTRUSTED_TAG]
            .as_str()
            .expect(NOT_TEXT);
        assert!(last_response.ends_with(CUT_MARKER));
        assert_eq!(tagged["error"][UNTRUSTED_TAG], ERROR);
    }

    #[test]
    fn fit_reads_a_structured_result_as_text() {
        let items = vec![Value::from(FILLER); MAX_EVENT_BYTES];
        let mut event = event(workflow_finished_with(Untrusted::Json(Value::Array(items))));
        assert!(event.fit(MAX_EVENT_BYTES));
        let tagged = event.to_tagged();
        assert!(tagged.to_string().len() <= MAX_EVENT_BYTES);
        let result = tagged["result"][UNTRUSTED_TAG].as_str().expect(NOT_TEXT);
        assert!(result.ends_with(CUT_MARKER));
        assert_eq!(tagged["report"][UNTRUSTED_TAG], REPORT);
    }

    #[test]
    fn fit_leaves_an_event_within_the_limit_alone() {
        let mut fitted = event(idle_detail());
        assert!(!fitted.fit(MAX_EVENT_BYTES));
        assert_eq!(fitted, event(idle_detail()));
    }

    #[test]
    fn fit_never_cuts_trusted_fields() {
        let mut fitted = event(schedule_detail());
        assert!(fitted.fit(0));
        let mut expected = event(schedule_detail());
        expected.session.title = Untrusted::text(CUT_MARKER);
        assert_eq!(fitted, expected);
    }

    #[test_case("abc", "abc".len() => ("abc".to_owned(), false); "within_the_limit")]
    #[test_case(&FILLER.repeat(20), CUT_MARKER.len() + 2 => (format!("xx{CUT_MARKER}"), true); "cut")]
    #[test_case(&"é".repeat(10), CUT_MARKER.len() + 3 => (format!("é{CUT_MARKER}"), true); "cut_at_a_char_boundary")]
    fn cap_text_ends_cut_text_with_the_marker(text: &str, max: usize) -> (String, bool) {
        cap_text(text.to_owned(), max)
    }
}
