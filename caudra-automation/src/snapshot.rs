//! The read model of a session's automations: what the runtime, storage, the TUI, the SDK and the
//! `automation` tool agree the session's automations look like, independent of the script engine.
//! Every type serializes both ways, so an SDK can carry them unchanged. Times are unix
//! milliseconds, as the runtime's clock reads them.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::args::ArgDecl;
use crate::catalog::{Scope, Trust};
use crate::event::{InputKind, TurnOutcome};
use crate::host::{ActionKind, DeliveryMode};
use crate::limits::{
    ActingMarks, DeliveryBackoff, LimitRefusal, ROLLING_WINDOW_MS, TurnWindow, UnattendedTurns,
};
use crate::meta::{AutomationLimits, MessagingCaps, TriggerKind};
use crate::replay::Answer;

/// Firings the mirror keeps across every automation of the session, newest first.
pub const MAX_RECENT_FIRINGS: usize = 50;
/// Firings the swarm view lists for each other session, newest first.
pub const MAX_SWARM_FIRINGS: usize = 20;
/// How many characters of a request a one-line summary quotes.
pub const MAX_SUMMARY_CHARS: usize = 120;
const SUMMARY_MARKER: &str = "…";
const SUMMARY_SEPARATOR: &str = ": ";
const TARGET_SEPARATOR: &str = " ";
const METHOD_FIELD: &str = "method";
/// Request fields naming where an action goes, in the order a summary prefers them.
const TARGET_FIELDS: [&str; 5] = ["to", "topic", "url", "url_env", "name"];
/// Request fields carrying an action's text, in the order a summary prefers them.
const TEXT_FIELDS: [&str; 3] = ["text", "condition", "reason"];

/// A closed set of names that storage keeps as text, with one spelling for serde, `as_str` and
/// `Display`.
macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident = $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $text)] $variant),+
        }

        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

text_enum! {
    /// How a session armed an automation.
    ArmOrigin {
        Cli = "cli",
        Profile = "profile",
        Manual = "manual",
        Always = "always",
        Sdk = "sdk",
    }
}

text_enum! {
    /// Where a firing is in its life: queued, deferred and running firings are pending.
    FiringStatus {
        Queued = "queued",
        Deferred = "deferred",
        Running = "running",
        Completed = "completed",
        Skipped = "skipped",
        Released = "released",
        Failed = "failed",
        RateLimited = "rate_limited",
        Cancelled = "cancelled",
        Paused = "paused",
        Dropped = "dropped",
        Interrupted = "interrupted",
    }
}

impl FiringStatus {
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Queued | Self::Deferred | Self::Running)
    }
}

text_enum! {
    /// Where an action is in its life: running actions and queued deliveries are pending.
    ActionStatus {
        Running = "running",
        Done = "done",
        Failed = "failed",
        Refused = "refused",
        Queued = "queued",
        Delivered = "delivered",
        Deduplicated = "deduplicated",
        Dropped = "dropped",
        Expired = "expired",
        Interrupted = "interrupted",
    }
}

text_enum! {
    /// Whether a completed firing's state change landed, or lost to a newer revision.
    StateOutcome {
        Committed = "committed",
        Conflict = "conflict",
    }
}

/// Who set the session's pause latch.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PauseSource {
    /// Esc Esc, Ctrl-C while streaming, or the question form's Cancel.
    User,
    /// An SDK client's `interrupt` or `automation_pause`.
    Sdk,
    /// `pause_automations()` in a firing of `automation`.
    Script {
        automation: String,
    },
    Inspector,
}

/// Holds every automation of the session until human input clears it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseLatch {
    pub reason: String,
    pub source: PauseSource,
    pub at: i64,
}

/// The session-wide counters the runtime owns and the session meta persists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionControls {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause: Option<PauseLatch>,
    /// Automation-started turns in the rolling hour.
    #[serde(default)]
    pub turn_window: TurnWindow,
    /// Automation-started turns since the last human input.
    #[serde(default)]
    pub unattended: UnattendedTurns,
    #[serde(default)]
    pub delivery_backoff: DeliveryBackoff,
}

/// Something that keeps the session from settling, so no `next` delivery starts a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum SettleBlocker {
    /// A turn, a tool or a compaction is running.
    Busy,
    NeedsInput(InputKind),
    /// A human prompt waits in the queue.
    PromptQueued,
    /// Peer messages wait to be claimed.
    PeerMessages,
    /// Group work is offered to the session.
    GroupWork,
    MailboxWake,
    GoalCheckin,
    /// Agent events wait to be handled.
    AgentEvents,
}

/// The session row of the inspector: the counters with the limits they run against, and what
/// keeps the session from settling.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionControlsView {
    pub controls: SessionControls,
    /// `[automations] turns_per_hour`.
    pub turns_per_hour: u32,
    /// `[automations] max_unattended_turns`; no cap when unset.
    pub max_unattended_turns: Option<u32>,
    /// Empty once the session has settled.
    pub blockers: Vec<SettleBlocker>,
}

/// Which group of the inspector list a catalog name belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Availability {
    Armed,
    Available,
    /// A project script whose digest still needs approval.
    NeedsTrust,
    /// A file that cannot load, and why.
    Invalid {
        reason: String,
    },
}

/// What an automation is doing, as its status glyph shows it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutomationStatus {
    #[default]
    Idle,
    Running,
    Queued {
        waiting: u32,
    },
    /// A limit holds its waiting one-shot event.
    Deferred {
        until: i64,
    },
    /// Its last firing failed, so the next acting firing waits.
    BackingOff {
        until: i64,
    },
    Paused,
    Failed,
}

/// One entry of `meta.triggers` and when it fires next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerView {
    pub index: u32,
    pub kind: TriggerKind,
    /// When a schedule is next due.
    pub next_due: Option<i64>,
    /// When the pending `after` delay of an `idle` or `needs_input` trigger runs out.
    pub after_until: Option<i64>,
    /// Matching messages go to the automation instead of the model.
    pub consumes: bool,
}

/// What the header lets the script reach.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Origins `http()` may reach.
    pub network: Vec<String>,
    /// Environment variables `http()` may read.
    pub secrets: Vec<String>,
    pub messaging: MessagingCaps,
    /// Workflows `start_workflow` may launch.
    pub workflows: Vec<String>,
}

/// One catalog name as this session sees it: the script, its arming and what it is doing. The
/// script's fields stay empty for a file that cannot load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutomationSnapshot {
    pub name: String,
    pub description: String,
    pub scope: Scope,
    pub path: PathBuf,
    /// The trust digest of the file.
    pub digest: String,
    pub trust: Trust,
    pub availability: Availability,
    /// How the session armed it; `None` while it is not armed.
    pub armed: Option<ArmOrigin>,
    /// The args the session gave it; `None` before it was first armed here.
    pub args: Option<Value>,
    /// `meta.args`, in declaration order.
    pub declared_args: Vec<ArgDecl>,
    pub status: AutomationStatus,
    pub last_firing: Option<FiringSummary>,
    pub triggers: Vec<TriggerView>,
    pub limits: Option<AutomationLimits>,
    /// How much of the limits the rolling hour used, and the failure backoff.
    pub limiter: ActingMarks,
    pub capabilities: Capabilities,
    /// The host functions the script calls.
    pub host_functions: Vec<String>,
    /// Catalog warnings about the header.
    pub warnings: Vec<String>,
    /// The scopes whose script of the same name this one hides.
    pub shadowed: Vec<Scope>,
}

/// How a firing failed or was stopped, at the script position Rhai reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorView {
    pub kind: String,
    pub message: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

/// A firing as listings show it: everything but its event, state patch and actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FiringSummary {
    pub fire_id: String,
    pub automation: String,
    /// The digest of the script version that ran.
    pub digest: String,
    pub trigger: TriggerKind,
    /// The entry of `meta.triggers` the event matched.
    pub trigger_index: u32,
    /// The delivery a message event carries.
    pub event_key: Option<String>,
    /// The firing owns the message it handles.
    pub consumed: bool,
    pub status: FiringStatus,
    /// Why it skipped, released, was dropped or was interrupted.
    pub reason: Option<String>,
    pub error: Option<ErrorView>,
    /// How many firings the row stands for, counting the quiet skips it absorbed.
    pub repeats: u64,
    /// How often a limit deferred it.
    pub attempts: u64,
    pub operations: u64,
    pub state_outcome: Option<StateOutcome>,
    pub queued_at: i64,
    pub deferred_until: Option<i64>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub action_count: u64,
    pub first_action: Option<ActionKind>,
}

/// One firing as a trace: its event, what it did and what it changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FiringDetail {
    pub firing: FiringSummary,
    /// Tagged JSON: untrusted text keeps its `$untrusted` wrapper.
    pub event: Value,
    /// The event was too large to store, and `event` holds a preview of its text.
    pub event_cut: bool,
    /// The RFC 7396 patch the firing made to its state.
    pub state_patch: Option<Value>,
    pub patch_cut: bool,
    /// In call order.
    pub actions: Vec<ActionRow>,
    /// The script line the error points at, from the version that ran.
    pub error_source: Option<String>,
}

/// A finished firing's event run again against the current script and args and a copy of the
/// current state. Nothing was performed or stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DryRunDetail {
    /// The firing it replays.
    pub fire_id: String,
    /// What the dry run did, as a trace. `firing.digest` is the digest of the script it ran,
    /// which may differ from the replayed firing's; `state_patch` is the change it would
    /// commit, and `firing.error` how it would fail or stop.
    pub trace: FiringDetail,
    /// How each of `trace.actions` was answered, in the same order.
    pub answers: Vec<Answer>,
    /// The automation limit that would have refused a real firing's first charging action.
    /// Reported only: a dry run neither enforces nor charges it.
    pub limited: Option<LimitRefusal>,
    /// The revision of the state copy the dry run ran against, which `trace.state_patch`
    /// applies to.
    pub state_revision: u64,
}

/// Why a queued delivery has not started a turn yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitReason {
    Paused,
    HumanPromptQueued,
    ModalOpen,
    /// The session is working, or has not settled.
    Busy,
    /// Peer messages or group work go first.
    PeersFirst,
    TurnRateFull {
        until: i64,
    },
    UnattendedCap {
        cap: u32,
    },
    Backoff {
        until: i64,
    },
    /// Nothing holds it but time: it expires unless delivered first.
    ExpiresAt {
        at: i64,
    },
}

/// One action of a firing's trace. The request and result load on their own, as an
/// [`ActionBody`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionRow {
    pub seq: u64,
    pub kind: ActionKind,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub status: ActionStatus,
    /// What the request asked for, as [`request_summary`] puts it.
    pub summary: String,
    pub error: Option<String>,
    /// The run id, the message id, the item it was deduplicated into, or the origin an `http`
    /// request went to.
    pub target: Option<String>,
    pub delivery: Option<DeliveryMode>,
    pub expires_at: Option<i64>,
    /// Why a queued delivery still waits; only the live runtime knows.
    pub wait: Option<WaitReason>,
    /// What the turn a delivery started led to, once it settled.
    pub turn_outcome: Option<TurnOutcome>,
    /// USD.
    pub turn_cost: Option<f64>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub delivered_at: Option<i64>,
    pub request_cut: bool,
    pub result_cut: bool,
}

/// An action's journaled request and result, loaded when asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionBody {
    pub fire_id: String,
    pub seq: u64,
    pub request: Value,
    pub request_cut: bool,
    pub result: Option<Value>,
    pub result_cut: bool,
}

/// A delivery waiting in the session's outbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxItem {
    pub automation: String,
    pub fire_id: String,
    pub seq: u64,
    /// `message` or `set_goal`.
    pub kind: ActionKind,
    pub summary: String,
    pub delivery: DeliveryMode,
    pub queued_at: i64,
    pub expires_at: Option<i64>,
    pub wait: Option<WaitReason>,
}

/// How a session armed an automation, without its state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingView {
    pub name: String,
    pub scope: Scope,
    pub origin: ArmOrigin,
    pub armed: bool,
    pub args: Value,
    /// The script digest the args were last validated against.
    pub args_digest: Option<String>,
}

/// An automation's committed state and who wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateView {
    /// Tagged JSON: untrusted values keep their `$untrusted` wrapper.
    pub value: Value,
    pub revision: u64,
    /// The firing that wrote it; `None` after a human edit or a clear.
    pub writer: Option<String>,
    /// The script digest the writing firing ran.
    pub digest: Option<String>,
    pub written_at: Option<i64>,
}

/// One automation of this session or another: its binding, state and newest firings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutomationDetail {
    pub session_id: String,
    pub name: String,
    /// `None` until the session first arms it.
    pub binding: Option<BindingView>,
    pub state: Option<StateView>,
    pub firings: Vec<FiringSummary>,
}

/// Another session with automations, as the swarm view lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutomationHistoryEntry {
    pub session_id: String,
    pub title: String,
    /// The messaging name the session reclaims, known while it is offline: the one it stored, or
    /// its default one.
    pub handle: String,
    pub last_activity_at: i64,
    pub bindings: Vec<BindingView>,
    /// Newest first, at most [`MAX_SWARM_FIRINGS`].
    pub firings: Vec<FiringSummary>,
}

/// The mirror a runtime publishes for the frontends.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AutomationState {
    pub session: SessionControlsView,
    /// One per catalog name, sorted by name.
    pub automations: Vec<AutomationSnapshot>,
    /// Oldest first.
    pub outbox: Vec<OutboxItem>,
    /// Newest first, at most [`MAX_RECENT_FIRINGS`].
    pub recent: Vec<FiringSummary>,
}

/// What changed, for the frontends and an open inspector. The mirror already holds the change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum AutomationEvent {
    Session(Box<SessionControlsView>),
    Automation(Box<AutomationSnapshot>),
    Firing {
        firing: Box<FiringSummary>,
        /// The earlier quiet skip this firing absorbed: its row is gone, and this one counts its
        /// repeats.
        absorbed: Option<String>,
    },
    Outbox(Vec<OutboxItem>),
    /// What `notify()` asked the frontend to show, or why an arming or a claimed goal was
    /// refused. `fire_id` names the firing it came from, when one did.
    Notice {
        automation: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fire_id: Option<String>,
        text: String,
    },
    /// The runtime needs the session's saved record before it writes anything: the frontend
    /// saves the session, then signals
    /// [`SessionSignal::Saved`](crate::request::SessionSignal::Saved).
    SaveSession,
}

impl SessionControlsView {
    /// Automation-started turns in the rolling hour that ends at `now`.
    pub fn turns_used(&self, now: i64) -> usize {
        let window_start = now.saturating_sub(ROLLING_WINDOW_MS);
        let turns = &self.controls.turn_window.turns;
        turns.len() - turns.partition_point(|at| *at <= window_start)
    }

    /// When the next automation-started turn fits, while the rolling hour is full.
    pub fn next_turn_at(&self, now: i64) -> Option<i64> {
        self.controls
            .turn_window
            .check(self.turns_per_hour, now)
            .err()
    }
}

impl AutomationState {
    pub fn find(&self, name: &str) -> Option<&AutomationSnapshot> {
        self.automations
            .iter()
            .find(|automation| automation.name == name)
    }

    /// The armed names, sorted, as the session meta keeps them.
    pub fn armed_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .automations
            .iter()
            .filter(|automation| automation.armed.is_some())
            .map(|automation| automation.name.clone())
            .collect();
        names.sort_unstable();
        names
    }
}

/// One line saying what a journaled request asks for: where it goes, then its text. A request
/// cut for storage is a preview of its JSON text, which the line quotes instead.
pub fn request_summary(request: &Value) -> String {
    let line = match request {
        Value::Object(fields) => {
            let first = |names: &[&str]| names.iter().find_map(|name| fields.get(*name)?.as_str());
            let target = [first(&[METHOD_FIELD]), first(&TARGET_FIELDS)]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(TARGET_SEPARATOR);
            match first(&TEXT_FIELDS) {
                Some(text) if !target.is_empty() => format!("{target}{SUMMARY_SEPARATOR}{text}"),
                Some(text) => text.to_owned(),
                None => target,
            }
        }
        Value::String(preview) => preview.clone(),
        _ => String::new(),
    };
    one_line(&line)
}

/// The first non-blank line of `text`, marked when more follows or it was cut.
fn one_line(text: &str) -> String {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let line = lines.next().unwrap_or_default();
    match line.char_indices().nth(MAX_SUMMARY_CHARS) {
        Some((end, _)) => format!("{}{SUMMARY_MARKER}", &line[..end]),
        None if lines.next().is_some() => format!("{line}{SUMMARY_MARKER}"),
        None => line.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde::de::DeserializeOwned;
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::args::{ArgSpec, ArgType};

    const NAME: &str = "goal-chain";
    const OTHER: &str = "ci-watch";
    const FIRE_ID: &str = "fire-1";
    const DIGEST: &str = "digest-1";
    const REASON: &str = "stopped by the user";
    const HOUR: i64 = 60 * 60 * 1000;
    const NOW: i64 = 10 * HOUR;
    const LONG_LINE_CHARS: usize = MAX_SUMMARY_CHARS + 10;
    const NAMES_MATCH_SERDE: &str = "as_str must spell a variant as serde does";
    const ROUND_TRIP: &str = "the read model must survive serialization unchanged";

    fn firing() -> FiringSummary {
        FiringSummary {
            fire_id: FIRE_ID.into(),
            automation: NAME.into(),
            digest: DIGEST.into(),
            trigger: TriggerKind::GoalFinished,
            trigger_index: 1,
            event_key: None,
            consumed: false,
            status: FiringStatus::Failed,
            reason: None,
            error: Some(ErrorView {
                kind: "runtime".into(),
                message: "boom".into(),
                line: Some(12),
                column: Some(3),
            }),
            repeats: 1,
            attempts: 0,
            operations: 42,
            state_outcome: Some(StateOutcome::Conflict),
            queued_at: NOW - 2,
            deferred_until: None,
            started_at: Some(NOW - 1),
            finished_at: Some(NOW),
            action_count: 1,
            first_action: Some(ActionKind::Message),
        }
    }

    fn automation(name: &str, armed: Option<ArmOrigin>) -> AutomationSnapshot {
        AutomationSnapshot {
            name: name.into(),
            description: "Continues a goal chain".into(),
            scope: Scope::User,
            path: PathBuf::from(format!("/home/user/.config/caudra/automations/{name}.rhai")),
            digest: DIGEST.into(),
            trust: Trust::Location,
            availability: if armed.is_some() {
                Availability::Armed
            } else {
                Availability::Available
            },
            armed,
            args: Some(json!({"steps": ["a", "b"]})),
            declared_args: vec![ArgDecl {
                name: "steps".into(),
                spec: ArgSpec {
                    kind: ArgType::List,
                    default: None,
                    min: Some(1.into()),
                    max: None,
                    choices: Vec::new(),
                    description: Some("Goals to chain".into()),
                    example: Some(json!(["write tests"])),
                },
            }],
            status: AutomationStatus::BackingOff { until: NOW + HOUR },
            last_firing: Some(firing()),
            triggers: vec![TriggerView {
                index: 0,
                kind: TriggerKind::Schedule,
                next_due: Some(NOW + HOUR),
                after_until: None,
                consumes: false,
            }],
            limits: Some(AutomationLimits {
                cooldown: Duration::from_secs(60),
                max_per_hour: 6,
            }),
            limiter: ActingMarks {
                acting: vec![NOW - 1],
                failure_streak: 1,
                backoff_until: Some(NOW + HOUR),
            },
            capabilities: Capabilities {
                network: vec!["https://api.example.com".into()],
                secrets: vec!["GITHUB_TOKEN".into()],
                messaging: MessagingCaps {
                    reply: true,
                    send: vec!["@reviewer".into()],
                    publish: Vec::new(),
                },
                workflows: vec!["review".into()],
            },
            host_functions: vec!["message".into(), "http".into()],
            warnings: Vec::new(),
            shadowed: vec![Scope::User],
        }
    }

    fn controls(turns: Vec<i64>, turns_per_hour: u32) -> SessionControlsView {
        SessionControlsView {
            controls: SessionControls {
                turn_window: TurnWindow { turns },
                ..SessionControls::default()
            },
            turns_per_hour,
            ..SessionControlsView::default()
        }
    }

    fn round_trip<T: Serialize + DeserializeOwned + PartialEq + fmt::Debug>(value: &T) {
        let json = serde_json::to_value(value).unwrap();
        let back: T = serde_json::from_value(json).unwrap();
        assert_eq!(&back, value, "{ROUND_TRIP}");
    }

    fn names_match_serde<T: Serialize + fmt::Display>(values: &[T]) {
        for value in values {
            assert_eq!(
                serde_json::to_value(value).unwrap(),
                value.to_string(),
                "{NAMES_MATCH_SERDE}"
            );
        }
    }

    #[test]
    fn text_enums_spell_every_variant_as_serde_does() {
        names_match_serde(ArmOrigin::ALL);
        names_match_serde(FiringStatus::ALL);
        names_match_serde(ActionStatus::ALL);
        names_match_serde(StateOutcome::ALL);
    }

    #[test_case(Vec::new(), 2 => (0, None); "empty_window")]
    #[test_case(vec![NOW - HOUR, NOW - 1], 2 => (1, None); "oldest_left_the_hour")]
    #[test_case(vec![NOW - HOUR + 5, NOW - 1], 2 => (2, Some(NOW + 5)); "full_until_oldest_leaves")]
    #[test_case(vec![NOW - 1], 0 => (1, Some(i64::MAX)); "zero_rate_never_has_room")]
    fn turn_window_use_and_next_slot(turns: Vec<i64>, per_hour: u32) -> (usize, Option<i64>) {
        let view = controls(turns, per_hour);
        (view.turns_used(NOW), view.next_turn_at(NOW))
    }

    #[test]
    fn armed_names_are_sorted_and_skip_unarmed() {
        let state = AutomationState {
            automations: vec![
                automation(NAME, Some(ArmOrigin::Manual)),
                automation("idle-nudge", None),
                automation(OTHER, Some(ArmOrigin::Always)),
            ],
            ..AutomationState::default()
        };

        assert_eq!(state.armed_names(), [OTHER, NAME]);
        assert_eq!(
            state.find(OTHER).map(|found| found.armed),
            Some(Some(ArmOrigin::Always))
        );
        assert!(state.find("missing").is_none());
    }

    #[test]
    fn the_mirror_round_trips() {
        let mut session = controls(vec![NOW - 1], 20);
        session.controls.pause = Some(PauseLatch {
            reason: REASON.into(),
            source: PauseSource::Script {
                automation: OTHER.into(),
            },
            at: NOW,
        });
        session.controls.delivery_backoff = DeliveryBackoff {
            errors: 2,
            until: Some(NOW + HOUR),
        };
        session.max_unattended_turns = Some(10);
        session.blockers = vec![
            SettleBlocker::NeedsInput(InputKind::Permission),
            SettleBlocker::PeerMessages,
        ];
        let state = AutomationState {
            session,
            automations: vec![automation(NAME, Some(ArmOrigin::Profile))],
            outbox: vec![OutboxItem {
                automation: NAME.into(),
                fire_id: FIRE_ID.into(),
                seq: 0,
                kind: ActionKind::SetGoal,
                summary: "ship it".into(),
                delivery: DeliveryMode::Next,
                queued_at: NOW,
                expires_at: Some(NOW + HOUR),
                wait: Some(WaitReason::TurnRateFull { until: NOW + 5 }),
            }],
            recent: vec![firing()],
        };

        round_trip(&state);
    }

    #[test]
    fn events_are_adjacently_tagged() {
        let event = AutomationEvent::Firing {
            firing: Box::new(firing()),
            absorbed: Some("fire-0".into()),
        };

        let json = serde_json::to_value(&event).unwrap();

        assert_eq!(json["kind"], "firing");
        assert_eq!(json["detail"]["firing"]["status"], "failed");
        assert_eq!(json["detail"]["firing"]["trigger"], "goal_finished");
        round_trip(&event);
        assert_eq!(
            serde_json::to_value(SettleBlocker::NeedsInput(InputKind::Plan)).unwrap(),
            json!({"kind": "needs_input", "detail": "plan"})
        );
    }

    #[test]
    fn a_trace_round_trips() {
        let detail = FiringDetail {
            firing: firing(),
            event: json!({"detail": {"kind": "idle"}}),
            event_cut: false,
            state_patch: Some(json!({"done": 1})),
            patch_cut: false,
            actions: vec![ActionRow {
                seq: 0,
                kind: ActionKind::Message,
                line: Some(4),
                column: Some(5),
                status: ActionStatus::Delivered,
                summary: "continue".into(),
                error: None,
                target: None,
                delivery: Some(DeliveryMode::Guide),
                expires_at: None,
                wait: None,
                turn_outcome: Some(TurnOutcome::MaxTurns),
                turn_cost: Some(0.25),
                started_at: NOW - 1,
                finished_at: Some(NOW),
                delivered_at: Some(NOW),
                request_cut: false,
                result_cut: true,
            }],
            error_source: Some("message(\"continue\");".into()),
        };

        round_trip(&detail);
    }

    #[test_case(json!({"kind": "message", "text": "continue", "delivery": "next"}), "continue"; "message")]
    #[test_case(json!({"kind": "set_goal", "condition": "tests pass", "replace": false}), "tests pass"; "set_goal")]
    #[test_case(json!({"kind": "send", "to": "@reviewer", "text": "ready"}), "@reviewer: ready"; "send")]
    #[test_case(json!({"kind": "publish", "topic": "ci.failures", "text": "red"}), "ci.failures: red"; "publish")]
    #[test_case(json!({"kind": "http", "method": "POST", "url": "https://api.example.com/x", "timeout": "30s"}), "POST https://api.example.com/x"; "http")]
    #[test_case(json!({"kind": "http", "method": "GET", "url_env": "CI_URL"}), "GET CI_URL"; "secret_url")]
    #[test_case(json!({"kind": "start_workflow", "name": "review", "args": {}}), "review"; "start_workflow")]
    #[test_case(json!({"kind": "pause", "reason": "budget spent"}), "budget spent"; "pause")]
    #[test_case(json!({"kind": "message", "text": "\n  first\nsecond"}), "first…"; "multi_line_text")]
    #[test_case(json!("{\"kind\":\"message\",\"text\":\"cut"), "{\"kind\":\"message\",\"text\":\"cut"; "cut_preview")]
    #[test_case(json!(null), ""; "no_request")]
    fn request_summaries(request: Value, summary: &str) {
        assert_eq!(request_summary(&request), summary);
    }

    #[test]
    fn long_summaries_are_cut_on_a_character() {
        let text = "é".repeat(LONG_LINE_CHARS);

        let summary = request_summary(&json!({"text": text}));

        assert_eq!(
            summary,
            format!("{}{SUMMARY_MARKER}", "é".repeat(MAX_SUMMARY_CHARS))
        );
    }
}
