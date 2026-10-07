//! What an automation runtime sees of its session's peer traffic and how it
//! acts on it: the messages it observes and may consume, the origin it sends
//! with, and the outcomes of group work.

use std::fmt::{self, Display, Formatter};
use std::ops::Not;
use std::path::PathBuf;
use std::str::FromStr;

use caudra_automation::event::WorkOutcome;
use caudra_automation::meta::{INVALID_NAME, is_valid_name};
use caudra_providers::PeerAudience;
use caudra_storage::messages::{PAUSED_BY_USER, WorkItem, WorkRefusal, WorkState};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::work::{COMPLETION_REQUIRED, PAUSED_BY_CANCEL, SESSION_CLOSED, TURN_FAILED, TURN_LIMIT};
use super::{Delivery, MESSAGE_WORDS, Route, valid_name, valid_token};

const KEY_SEPARATOR: char = ':';
const INVALID_KEY: &str =
    "Invalid message key; it joins the sender's route and the message ID with a colon";

/// Who a message goes out as. An automation sends as its session, marked with
/// its own name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOrigin {
    Session,
    Automation(String),
}

impl SendOrigin {
    pub(super) fn automation(&self) -> Option<&str> {
        match self {
            Self::Session => None,
            Self::Automation(name) => Some(name),
        }
    }

    /// The marker check, which runs before any other.
    pub(super) fn check(&self) -> Result<(), SendFailure> {
        match self.automation() {
            Some(name) if !is_valid_name(name) => Err(SendFailure::new(
                SendFailureKind::Invalid,
                format!("Automation name {name:?} {INVALID_NAME}"),
            )),
            _ => Ok(()),
        }
    }
}

/// A message as its sender identifies it, in the form of the receiver's
/// deduplication key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageKey {
    pub sender_route: String,
    pub message_id: String,
}

impl MessageKey {
    pub(super) fn of(delivery: &Delivery) -> Self {
        Self {
            sender_route: delivery.sender.route.target(),
            message_id: delivery.message_id.clone(),
        }
    }
}

impl Display for MessageKey {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}{KEY_SEPARATOR}{}",
            self.sender_route, self.message_id
        )
    }
}

impl FromStr for MessageKey {
    type Err = SendFailure;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .rsplit_once(KEY_SEPARATOR)
            .filter(|(route, message_id)| {
                Route::parse(route).is_ok()
                    && (valid_name(message_id, MESSAGE_WORDS) || valid_token(message_id))
            })
            .map(|(route, message_id)| Self {
                sender_route: route.to_owned(),
                message_id: message_id.to_owned(),
            })
            .ok_or_else(|| SendFailure::new(SendFailureKind::Invalid, INVALID_KEY))
    }
}

/// A consumed message as release admits it again. Only peers reads it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredDelivery {
    pub(super) delivery: Delivery,
    #[serde(default, skip_serializing_if = "Not::not")]
    pub(super) catch_up: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservedSender {
    /// `handle` is the address that reaches the sender: its `@name`, or a
    /// word target in this session when it has none.
    Session {
        handle: String,
    },
    Automation {
        handle: String,
        automation: String,
    },
    Script {
        label: String,
    },
}

#[derive(Debug, Clone)]
pub struct ObservedMessage {
    pub key: MessageKey,
    pub audience: PeerAudience,
    pub sender: ObservedSender,
    pub title: String,
    pub cwd: Option<PathBuf>,
    pub text: String,
    /// This session's name for the message it replies to.
    pub reply_to: Option<String>,
    pub held: bool,
    pub catch_up: bool,
    pub delivery: StoredDelivery,
}

/// Sees each message as it becomes held and as it becomes queued, never group
/// work. It runs under the session state lock, so it never blocks and never
/// calls back into the session.
pub trait MessageObserver: Send + Sync {
    /// The automation that takes `message` instead of the model. Only a
    /// message that is not held can be taken.
    fn observe(&self, message: &ObservedMessage) -> Option<String>;
}

#[derive(Debug, Error)]
#[error("{reason}")]
pub struct SendFailure {
    pub kind: SendFailureKind,
    pub reason: String,
}

impl SendFailure {
    pub fn new(kind: SendFailureKind, reason: impl Into<String>) -> Self {
        Self {
            kind,
            reason: reason.into(),
        }
    }

    pub(super) fn invalid(reason: impl Into<String>) -> Self {
        Self::new(SendFailureKind::Invalid, reason)
    }

    pub(super) fn unavailable(reason: impl Into<String>) -> Self {
        Self::new(SendFailureKind::Unavailable, reason)
    }

    /// Why the history refused to record a publication.
    pub(super) fn refused_work(refusal: WorkRefusal) -> Self {
        let kind = match refusal {
            WorkRefusal::BacklogFull(_)
            | WorkRefusal::OutstandingFull
            | WorkRefusal::GroupFanout { .. } => SendFailureKind::GroupFull,
            _ => SendFailureKind::Unavailable,
        };
        Self::new(kind, refusal.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendFailureKind {
    Refused,
    RateLimited,
    Unavailable,
    UnknownRecipient,
    GroupFull,
    ReadOnly,
    Invalid,
}

/// Why group work paused without an outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkPause {
    CompletionRequired,
    Cancelled,
    TurnLimit,
    TurnFailed,
    SessionClosed,
    Manual,
}

impl WorkPause {
    /// The pause a stored reason records, if it records one.
    pub fn from_text(reason: &str) -> Option<Self> {
        [
            (COMPLETION_REQUIRED, Self::CompletionRequired),
            (PAUSED_BY_CANCEL, Self::Cancelled),
            (TURN_LIMIT, Self::TurnLimit),
            (TURN_FAILED, Self::TurnFailed),
            (SESSION_CLOSED, Self::SessionClosed),
            (PAUSED_BY_USER, Self::Manual),
        ]
        .into_iter()
        .find_map(|(text, pause)| (text == reason).then_some(pause))
    }
}

/// What became of group work in this session: the outcome its agent
/// reported, the pause a turn ended in, or a pause its person made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkReported {
    pub group: String,
    pub work: String,
    pub outcome: WorkOutcome,
    pub pause: Option<WorkPause>,
    /// The agent's summary or reason.
    pub detail: Option<String>,
}

impl WorkReported {
    /// What the outcome an agent reported made of `item`: a retry without
    /// attempts left fails it.
    pub(super) fn reported(item: &WorkItem) -> Option<Self> {
        let (outcome, detail) = match item.state {
            WorkState::Completed => (WorkOutcome::Completed, &item.result),
            WorkState::Pending => (WorkOutcome::Retry, &item.reason),
            WorkState::Failed => (WorkOutcome::Failed, &item.reason),
            _ => return None,
        };
        Some(Self {
            group: item.group.clone(),
            work: item.name.clone(),
            outcome,
            pause: None,
            detail: detail.clone(),
        })
    }

    pub(super) fn paused(group: String, work: String, reason: &str) -> Self {
        Self {
            group,
            work,
            outcome: WorkOutcome::Paused,
            pause: WorkPause::from_text(reason),
            detail: None,
        }
    }
}

/// A point in the history's sequence of work changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkCursor(pub(super) u64);

#[cfg(test)]
mod tests {
    use caudra_storage::id::CaudraId;
    use test_case::test_case;

    use super::{INVALID_KEY, KEY_SEPARATOR, MessageKey, SendFailureKind};
    use crate::peers::{Route, token};

    const MESSAGE_NAME: &str = "brisk-calm-otter";
    const MALFORMED_MESSAGE_ID: &str = "Brisk-Calm-Otter";
    const MALFORMED_ROUTE: &str = "not-a-route";

    fn route() -> String {
        Route {
            host: token().unwrap(),
            session: CaudraId::generate(),
            generation: token().unwrap(),
        }
        .target()
    }

    #[test_case(MESSAGE_NAME.to_owned(); "message_name")]
    #[test_case(token().unwrap(); "legacy_token")]
    fn message_keys_read_back_as_written(message_id: String) {
        let key = MessageKey {
            sender_route: route(),
            message_id,
        };
        assert_eq!(key.to_string().parse::<MessageKey>().unwrap(), key);
    }

    #[test_case(|route: &str| route.to_owned(); "no_message_id")]
    #[test_case(|route: &str| format!("{route}{KEY_SEPARATOR}{MALFORMED_MESSAGE_ID}"); "malformed_message_id")]
    #[test_case(|_: &str| format!("{MALFORMED_ROUTE}{KEY_SEPARATOR}{MESSAGE_NAME}"); "malformed_route")]
    fn malformed_message_keys_are_invalid(key: fn(&str) -> String) {
        let failure = key(&route()).parse::<MessageKey>().unwrap_err();
        assert_eq!(
            (failure.kind, failure.reason.as_str()),
            (SendFailureKind::Invalid, INVALID_KEY)
        );
    }
}
