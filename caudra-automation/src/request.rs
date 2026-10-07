//! The control surface an automation runtime exposes: requests that answer, signals the
//! frontends send without waiting, and what a delivery claim carries. Everything serializes, so
//! an SDK can carry it unchanged.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::{GoalFinishedDetail, IdleDetail, InputKind, SessionView, TurnOutcome};
use crate::host::DeliveryMode;
use crate::snapshot::{
    ActionBody, ArmOrigin, AutomationDetail, AutomationHistoryEntry, AutomationSnapshot,
    DryRunDetail, FiringDetail, FiringSummary, PauseSource, SessionControlsView, SettleBlocker,
    WaitReason,
};

pub const UNAVAILABLE: &str = "automations are unavailable here: the experimental automations \
     feature is off, or the session's runtime stopped";
pub const LIST_HINT: &str = "list the automations to see the scripts this session can arm";
pub const TRUST_HINT: &str = "review the script, then trust this digest";
pub const CONFLICT_HINT: &str =
    "a firing or another edit changed it since it was loaded; reload it and edit again";
pub const NOT_WAITING: &str = "is no longer waiting: it already ran, was delivered or was dropped";
pub const NOT_SAVED: &str = "is not saved yet, and its automations persist only with it";
pub const REPLAY_NOT_FINISHED: &str = "it is still queued, deferred or running";
pub const REPLAY_EVENT_CUT: &str = "its event was too large to keep";
pub const REPLAY_EVENT_UNREADABLE: &str =
    "its stored event no longer reads as an event of this build";
pub const REPLAY_NO_TRIGGER: &str = "the current script has no trigger for its event";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum AutomationRequest {
    /// Every catalog name as this session sees it.
    List,
    Validate {
        name: String,
    },
    /// Arms, or arms again with new args. Without args, the stored ones or the defaults apply.
    Arm {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<Value>,
        origin: ArmOrigin,
    },
    Disarm {
        name: String,
    },
    Trust {
        name: String,
        digest: String,
    },
    /// One automation's binding, state and newest firings: this session's, or another session's
    /// when `session_id` names one.
    Inspect {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    /// One firing's trace, from any session.
    Firing {
        fire_id: String,
    },
    ActionBody {
        fire_id: String,
        seq: u64,
    },
    /// Replaces the args and arms again.
    SetArgs {
        name: String,
        args: Value,
    },
    /// A human edit, applied only at the revision the editor loaded.
    SetState {
        name: String,
        state: Value,
        expected_revision: u64,
    },
    ClearState {
        name: String,
        expected_revision: u64,
    },
    Drop(DropTarget),
    Pause {
        by: PauseSource,
    },
    Resume,
    /// Re-runs a finished firing against the current script, args and state, performing nothing.
    DryRun {
        fire_id: String,
    },
    /// This session's newest firings, of one automation or all; or one firing's trace when
    /// `fire_id` names it.
    History {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fire_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    /// Other sessions with automations, newest activity first, for the swarm view.
    Sessions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
}

/// What a human drops from the inspector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DropTarget {
    /// A queued or deferred firing. A consumed message it holds is released.
    Firing {
        fire_id: String,
    },
    OutboxItem {
        fire_id: String,
        seq: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum AutomationResponse {
    Automations(Vec<AutomationSnapshot>),
    Validation {
        name: String,
        ok: bool,
        report: String,
    },
    Automation(Box<AutomationSnapshot>),
    Detail(Box<AutomationDetail>),
    Firing(Box<FiringDetail>),
    /// A finished firing replayed for [`AutomationRequest::DryRun`].
    DryRun(Box<DryRunDetail>),
    Firings(Vec<FiringSummary>),
    ActionBody(Box<ActionBody>),
    /// The state's revision after a write or a clear.
    State {
        revision: u64,
    },
    Controls(Box<SessionControlsView>),
    Sessions(Vec<AutomationHistoryEntry>),
    Ack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum AutomationError {
    #[error("{UNAVAILABLE}")]
    Unavailable,
    #[error("unknown automation {name:?}; {LIST_HINT}")]
    UnknownAutomation { name: String },
    #[error("automation {name:?} at {path} is not trusted (digest {digest}); {TRUST_HINT}")]
    TrustRequired {
        name: String,
        digest: String,
        path: String,
    },
    #[error("automation {name:?} is invalid: {reason}")]
    Invalid { name: String, reason: String },
    #[error("automation {name:?} rejected its args: {reason}")]
    Args { name: String, reason: String },
    #[error("automation {name:?} state is at revision {current}; {CONFLICT_HINT}")]
    StateConflict { name: String, current: u64 },
    #[error("unknown automation firing {fire_id:?}")]
    UnknownFiring { fire_id: String },
    #[error("automation firing {fire_id:?} cannot be replayed: {reason}")]
    NotReplayable {
        fire_id: String,
        /// [`REPLAY_NOT_FINISHED`], [`REPLAY_EVENT_CUT`], [`REPLAY_EVENT_UNREADABLE`] or
        /// [`REPLAY_NO_TRIGGER`].
        reason: String,
    },
    #[error("{} {NOT_WAITING}", waiting_label(.fire_id, .seq))]
    NotWaiting {
        fire_id: String,
        /// Names one of the firing's actions, such as an outbox item, rather than the firing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
    },
    /// Bindings, sources and firings belong to the session's saved record and cascade from it.
    #[error("automation storage: session {session_id} {NOT_SAVED}")]
    SessionNotSaved { session_id: String },
    #[error("automation storage: {0}")]
    Storage(String),
    #[error("automation runtime: {0}")]
    Internal(String),
}

/// What the frontends tell the runtime without waiting for an answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum SessionSignal {
    /// The session as events show it, sent at spawn and whenever a field changes. The runtime
    /// keeps the latest and copies it into every event.
    Facts(Box<SessionView>),
    /// The session's record exists now, so the runtime may write.
    Saved,
    /// The session settled after a busy period, with what the period led to.
    Settled(Box<IdleDetail>),
    /// The session is not settled, and why, for the inspector's Overview. An empty list reports
    /// a session that settled with no run behind it, so no `idle` fires.
    Busy {
        blockers: Vec<SettleBlocker>,
    },
    /// The session waits on a person; `tool` names the tool a `permission` prompt is for. The
    /// same `input` and `tool` again is the same wait, and restarts its `after` delays without
    /// firing twice; a different tool is a new wait.
    NeedsInput {
        input: InputKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
    },
    InputResolved,
    GoalFinished(Box<GoalFinishedDetail>),
    /// Human input clears the pause latch; the runtime then fires `armed` with reason
    /// `unpaused`.
    HumanInput,
    Pause {
        by: PauseSource,
    },
    /// The profile's `automations:` list changed.
    ProfileAutomations(Vec<ProfileArming>),
    /// A run ended, so the delivery backoff can count an error or reset.
    RunEnded(TurnOutcome),
}

/// One entry of a profile's `automations:` list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileArming {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
}

/// What the claimant knows when it asks the outbox for an item. The runtime adds its own latch,
/// turn rate, unattended cap and backoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryGate {
    /// `Next` when a frontend would start a turn; `Guide` when a running turn claims before its
    /// next model request, which only `guide` items join.
    pub mode: DeliveryMode,
    pub settled: bool,
    pub prompt_queued: bool,
    pub modal_open: bool,
    /// Peer messages or group work wait, and go first.
    pub peers_first: bool,
    pub now: i64,
}

/// A delivery handed to the frontend, which turns it into an automation message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxClaim {
    pub automation: String,
    pub fire_id: String,
    pub seq: u64,
    /// Trusted: it becomes the session's instructions.
    pub text: String,
    /// The item's `attach` value, already framed as an untrusted block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach: Option<String>,
    /// The goal a `set_goal` item sets before its kickoff turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalClaim>,
}

/// The goal of a `set_goal` item, as its journaled request holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalClaim {
    pub condition: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_limit: Option<u32>,
    /// Replaces an active goal instead of failing.
    #[serde(default)]
    pub replace: bool,
}

impl DeliveryGate {
    /// Why the claimant cannot take an item now. A running turn takes `guide` items as they
    /// come; anything that starts a turn waits for a settled session with nobody ahead of it.
    pub fn wait_reason(&self) -> Option<WaitReason> {
        if self.mode == DeliveryMode::Guide && !self.settled {
            None
        } else if self.prompt_queued {
            Some(WaitReason::HumanPromptQueued)
        } else if self.modal_open {
            Some(WaitReason::ModalOpen)
        } else if !self.settled {
            Some(WaitReason::Busy)
        } else if self.peers_first {
            Some(WaitReason::PeersFirst)
        } else {
            None
        }
    }
}

fn waiting_label(fire_id: &str, seq: &Option<u64>) -> String {
    match seq {
        Some(seq) => format!("automation action {fire_id}#{seq}"),
        None => format!("automation firing {fire_id:?}"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::host::{ActionKind, ActionRequest, GoalRequest};
    use crate::limits::{LimitReason, LimitRefusal};
    use crate::meta::TriggerKind;
    use crate::replay::Answer;
    use crate::snapshot::{ActionRow, ActionStatus, FiringStatus};

    const NAME: &str = "goal-chain";
    const FIRE_ID: &str = "fire-1";
    const DIGEST: &str = "abc123";
    const PATH: &str = "/project/.caudra/automations/goal-chain.rhai";
    const GOAL: &str = "the tests pass";
    const TOOL: &str = "shell";
    const STORAGE_DETAIL: &str = "database is locked";
    const AT: i64 = 1_791_189_000_000;
    const LIMITED_UNTIL: i64 = AT + 60_000;
    const RUNS_SUMMARY: &str = "GET https://api.example.com/v1/runs";
    const UNKNOWN_PREFIX: &str = "unknown automation \"goal-chain\"; ";
    const TRUST_PREFIX: &str = "automation \"goal-chain\" at /project/.caudra/automations/goal-chain.rhai is not trusted (digest abc123); ";
    const NOT_REPLAYABLE_PREFIX: &str = "automation firing \"fire-1\" cannot be replayed: ";
    const CONFLICT_MESSAGE: &str = "automation \"goal-chain\" state is at revision 4; a firing or another edit changed it since it was loaded; reload it and edit again";
    const FIRING_NOT_WAITING: &str = "automation firing \"fire-1\" is no longer waiting: it already ran, was delivered or was dropped";
    const ACTION_NOT_WAITING: &str = "automation action fire-1#2 is no longer waiting: it already ran, was delivered or was dropped";
    const STORAGE_MESSAGE: &str = "automation storage: database is locked";
    const SESSION_ID: &str = "JhkVuqgZTbEZ6dQ5uNBnBs";
    const NOT_SAVED_MESSAGE: &str = "automation storage: session JhkVuqgZTbEZ6dQ5uNBnBs is not saved yet, and its automations persist only with it";

    fn clear_gate(mode: DeliveryMode) -> DeliveryGate {
        DeliveryGate {
            mode,
            settled: true,
            prompt_queued: false,
            modal_open: false,
            peers_first: false,
            now: 0,
        }
    }

    #[test]
    fn requests_are_adjacently_tagged() {
        let request = AutomationRequest::Arm {
            name: NAME.into(),
            args: Some(json!({"steps": ["a"]})),
            origin: ArmOrigin::Manual,
        };

        let json = serde_json::to_value(&request).unwrap();

        assert_eq!(json["kind"], "arm");
        assert_eq!(json["detail"]["origin"], "manual");
        let back: AutomationRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, request);
        assert_eq!(
            serde_json::to_value(AutomationRequest::Resume).unwrap(),
            json!({"kind": "resume"})
        );
        let drop = AutomationRequest::Drop(DropTarget::OutboxItem {
            fire_id: FIRE_ID.into(),
            seq: 2,
        });
        let json = serde_json::to_value(&drop).unwrap();
        assert_eq!(
            json,
            json!({"kind": "drop", "detail": {"kind": "outbox_item", "fire_id": FIRE_ID, "seq": 2}})
        );
        assert_eq!(
            serde_json::from_value::<AutomationRequest>(json).unwrap(),
            drop
        );
    }

    #[test]
    fn optional_request_fields_may_be_omitted() {
        let history: AutomationRequest =
            serde_json::from_value(json!({"kind": "history", "detail": {}})).unwrap();
        let inspect: AutomationRequest =
            serde_json::from_value(json!({"kind": "inspect", "detail": {"name": NAME}})).unwrap();

        assert_eq!(
            history,
            AutomationRequest::History {
                name: None,
                fire_id: None,
                limit: None,
            }
        );
        assert_eq!(
            inspect,
            AutomationRequest::Inspect {
                name: NAME.into(),
                session_id: None,
            }
        );
    }

    #[test]
    fn responses_round_trip() {
        let response = AutomationResponse::State { revision: 3 };

        let json = serde_json::to_value(&response).unwrap();

        assert_eq!(json, json!({"kind": "state", "detail": {"revision": 3}}));
        assert_eq!(
            serde_json::from_value::<AutomationResponse>(json).unwrap(),
            response
        );
    }

    #[test]
    fn a_dry_run_keeps_its_wire_shape() {
        let response = AutomationResponse::DryRun(Box::new(DryRunDetail {
            fire_id: FIRE_ID.into(),
            trace: FiringDetail {
                firing: FiringSummary {
                    fire_id: FIRE_ID.into(),
                    automation: NAME.into(),
                    digest: DIGEST.into(),
                    trigger: TriggerKind::Schedule,
                    trigger_index: 0,
                    event_key: None,
                    consumed: false,
                    status: FiringStatus::Completed,
                    reason: None,
                    error: None,
                    repeats: 1,
                    attempts: 0,
                    operations: 12,
                    state_outcome: None,
                    queued_at: AT,
                    deferred_until: None,
                    started_at: Some(AT),
                    finished_at: Some(AT),
                    action_count: 1,
                    first_action: Some(ActionKind::Http),
                },
                event: json!({"fire_id": FIRE_ID}),
                event_cut: false,
                state_patch: Some(json!({"seen": 2})),
                patch_cut: false,
                actions: vec![ActionRow {
                    seq: 0,
                    kind: ActionKind::Http,
                    line: Some(3),
                    column: Some(16),
                    status: ActionStatus::Done,
                    summary: RUNS_SUMMARY.into(),
                    error: None,
                    target: None,
                    delivery: None,
                    expires_at: None,
                    wait: None,
                    turn_outcome: None,
                    turn_cost: None,
                    started_at: AT,
                    finished_at: Some(AT),
                    delivered_at: None,
                    request_cut: false,
                    result_cut: false,
                }],
                error_source: None,
            },
            answers: vec![Answer::Cut],
            limited: Some(LimitRefusal {
                reason: LimitReason::Cooldown,
                until: LIMITED_UNTIL,
            }),
            state_revision: 4,
        }));

        let json = serde_json::to_value(&response).unwrap();

        assert_eq!(
            json,
            json!({
                "kind": "dry_run",
                "detail": {
                    "fire_id": FIRE_ID,
                    "trace": {
                        "firing": {
                            "fire_id": FIRE_ID,
                            "automation": NAME,
                            "digest": DIGEST,
                            "trigger": "schedule",
                            "trigger_index": 0,
                            "event_key": null,
                            "consumed": false,
                            "status": "completed",
                            "reason": null,
                            "error": null,
                            "repeats": 1,
                            "attempts": 0,
                            "operations": 12,
                            "state_outcome": null,
                            "queued_at": AT,
                            "deferred_until": null,
                            "started_at": AT,
                            "finished_at": AT,
                            "action_count": 1,
                            "first_action": "http"
                        },
                        "event": {"fire_id": FIRE_ID},
                        "event_cut": false,
                        "state_patch": {"seen": 2},
                        "patch_cut": false,
                        "actions": [{
                            "seq": 0,
                            "kind": "http",
                            "line": 3,
                            "column": 16,
                            "status": "done",
                            "summary": RUNS_SUMMARY,
                            "error": null,
                            "target": null,
                            "delivery": null,
                            "expires_at": null,
                            "wait": null,
                            "turn_outcome": null,
                            "turn_cost": null,
                            "started_at": AT,
                            "finished_at": AT,
                            "delivered_at": null,
                            "request_cut": false,
                            "result_cut": false
                        }],
                        "error_source": null
                    },
                    "answers": ["cut"],
                    "limited": {"reason": "cooldown", "until": LIMITED_UNTIL},
                    "state_revision": 4
                }
            })
        );
        assert_eq!(
            serde_json::from_value::<AutomationResponse>(json).unwrap(),
            response
        );
    }

    #[test]
    fn a_refused_replay_keeps_its_wire_kind() {
        let error = AutomationError::NotReplayable {
            fire_id: FIRE_ID.into(),
            reason: REPLAY_EVENT_CUT.into(),
        };

        assert_eq!(
            serde_json::to_value(&error).unwrap(),
            json!({"kind": "not_replayable", "detail": {"fire_id": FIRE_ID, "reason": REPLAY_EVENT_CUT}})
        );
    }

    #[test_case(AutomationError::Unavailable, UNAVAILABLE; "unavailable")]
    #[test_case(AutomationError::UnknownAutomation { name: NAME.into() }, &format!("{UNKNOWN_PREFIX}{LIST_HINT}"); "unknown_automation")]
    #[test_case(AutomationError::TrustRequired { name: NAME.into(), digest: DIGEST.into(), path: PATH.into() }, &format!("{TRUST_PREFIX}{TRUST_HINT}"); "trust_required")]
    #[test_case(AutomationError::StateConflict { name: NAME.into(), current: 4 }, CONFLICT_MESSAGE; "state_conflict")]
    #[test_case(AutomationError::NotReplayable { fire_id: FIRE_ID.into(), reason: REPLAY_NOT_FINISHED.into() }, &format!("{NOT_REPLAYABLE_PREFIX}{REPLAY_NOT_FINISHED}"); "not_replayable")]
    #[test_case(AutomationError::NotWaiting { fire_id: FIRE_ID.into(), seq: None }, FIRING_NOT_WAITING; "firing_not_waiting")]
    #[test_case(AutomationError::NotWaiting { fire_id: FIRE_ID.into(), seq: Some(2) }, ACTION_NOT_WAITING; "action_not_waiting")]
    #[test_case(AutomationError::SessionNotSaved { session_id: SESSION_ID.into() }, NOT_SAVED_MESSAGE; "session_not_saved")]
    #[test_case(AutomationError::Storage(STORAGE_DETAIL.into()), STORAGE_MESSAGE; "storage")]
    fn errors_read_well_and_round_trip(error: AutomationError, message: &str) {
        assert_eq!(error.to_string(), message);
        let json = serde_json::to_value(&error).unwrap();
        assert_eq!(
            serde_json::from_value::<AutomationError>(json).unwrap(),
            error
        );
    }

    #[test]
    fn a_goal_claim_reads_the_journaled_set_goal_request() {
        let request = ActionRequest::SetGoal(GoalRequest {
            condition: GOAL.into(),
            continuation_limit: Some(5),
            replace: true,
            expires: Some(Duration::from_secs(3600)),
        });

        let journaled = serde_json::to_value(&request).unwrap();
        let claim: GoalClaim = serde_json::from_value(journaled).unwrap();

        assert_eq!(
            claim,
            GoalClaim {
                condition: GOAL.into(),
                continuation_limit: Some(5),
                replace: true,
            }
        );
    }

    #[test_case(clear_gate(DeliveryMode::Next) => None; "settled_and_clear")]
    #[test_case(DeliveryGate { prompt_queued: true, ..clear_gate(DeliveryMode::Next) } => Some(WaitReason::HumanPromptQueued); "human_prompt_first")]
    #[test_case(DeliveryGate { modal_open: true, ..clear_gate(DeliveryMode::Next) } => Some(WaitReason::ModalOpen); "modal_open")]
    #[test_case(DeliveryGate { settled: false, ..clear_gate(DeliveryMode::Next) } => Some(WaitReason::Busy); "busy")]
    #[test_case(DeliveryGate { peers_first: true, ..clear_gate(DeliveryMode::Next) } => Some(WaitReason::PeersFirst); "peers_first")]
    #[test_case(DeliveryGate { settled: false, prompt_queued: true, ..clear_gate(DeliveryMode::Guide) } => None; "running_turn_takes_guide")]
    #[test_case(DeliveryGate { modal_open: true, ..clear_gate(DeliveryMode::Guide) } => Some(WaitReason::ModalOpen); "idle_guide_starts_a_turn")]
    fn delivery_gates(gate: DeliveryGate) -> Option<WaitReason> {
        gate.wait_reason()
    }

    #[test_case(SessionSignal::Saved, json!({"kind": "saved"}); "saved")]
    #[test_case(SessionSignal::Busy { blockers: vec![SettleBlocker::PromptQueued] }, json!({"kind": "busy", "detail": {"blockers": [{"kind": "prompt_queued"}]}}); "busy")]
    #[test_case(SessionSignal::NeedsInput { input: InputKind::Permission, tool: Some(TOOL.into()) }, json!({"kind": "needs_input", "detail": {"input": "permission", "tool": TOOL}}); "needs_input_with_its_tool")]
    #[test_case(SessionSignal::NeedsInput { input: InputKind::Question, tool: None }, json!({"kind": "needs_input", "detail": {"input": "question"}}); "needs_input_without_a_tool")]
    fn signals_keep_their_wire_shape(signal: SessionSignal, expected: Value) {
        let json = serde_json::to_value(&signal).unwrap();

        assert_eq!(json, expected);
        assert_eq!(
            serde_json::from_value::<SessionSignal>(json).unwrap(),
            signal
        );
    }

    #[test]
    fn signals_carry_neutral_profile_entries() {
        let signal = SessionSignal::ProfileAutomations(vec![ProfileArming {
            name: NAME.into(),
            args: None,
        }]);

        let json = serde_json::to_value(&signal).unwrap();

        assert_eq!(
            json,
            json!({"kind": "profile_automations", "detail": [{"name": NAME}]})
        );
        assert_eq!(
            serde_json::from_value::<SessionSignal>(json).unwrap(),
            signal
        );
    }
}
