//! Automations in cross-session messaging. The runtime sends, settles, gives back, releases and
//! reads the work the session published through a [`Messaging`], which [`PeerSession`]
//! implements, while the session's [`Observer`] turns each message the session receives into
//! `message_received` events. The observer runs under the session's state lock: it reads the
//! snapshot of armed triggers the actor last published, never blocks, and only `try_send`s to the
//! actor.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use arc_swap::ArcSwap;
use caudra_automation::catalog::CatalogEntry;
use caudra_automation::event::{
    Admission, Audience, Delivery, Event, EventDetail, MessageDetail, SenderKind,
};
use caudra_automation::host::{
    ActionReply, ActionRequest, Failure, FailureKind, HostError, HostResult, PublishReceipt,
    QueuedWork, RecipientReceipt, RecipientStatus, SendReceipt, SendStatus,
};
use caudra_automation::matcher::first_match;
use caudra_automation::meta::Trigger;
use caudra_automation::untrusted::Untrusted;
use caudra_providers::PeerAudience;
use thiserror::Error;

use super::handle::Command;
use super::host::not_in_this_build;
use super::manager::{Topics, spelled};
use crate::peers::{
    HistoryVersion, MessageKey, MessageObserver, ObservedMessage, ObservedSender, PeerSession,
    PublishReceipt as PeerPublishReceipt, STATUS_CONSUMED, STATUS_HELD, STATUS_QUEUED,
    STATUS_RATE_LIMITED, STATUS_REFUSED, STATUS_UNAVAILABLE, SendFailure, SendFailureKind,
    SendOrigin, SendReceipt as PeerSendReceipt, StoredDelivery, WorkCursor, WorkItem,
};

const EVENT_KEY_SEPARATOR: char = ':';
const REQUEST_ID_PREFIX: &str = "automation";
pub const NO_MESSAGING: &str = "cross-session messaging is off in this session";
pub const NO_REPLY_SENDER: &str = "the consumed message names no sender to reply to";

pub type MessagingFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// The session's peers as its automations reach them. Each future owns what it needs, so it
/// runs off the actor.
pub trait Messaging: Send + Sync {
    /// [`PeerSession::send_from`].
    fn send(
        &self,
        origin: SendOrigin,
        target: String,
        text: String,
        reply_to: Option<MessageKey>,
        request_id: String,
    ) -> MessagingFuture<Result<PeerSendReceipt, SendFailure>>;
    /// [`PeerSession::publish_from`].
    fn publish(
        &self,
        origin: SendOrigin,
        audience: PeerAudience,
        text: String,
        request_id: String,
    ) -> MessagingFuture<Result<PeerPublishReceipt, SendFailure>>;
    /// [`PeerSession::settle_consumed`], which is cheap and never blocks.
    fn settle(&self, key: &MessageKey, automation: &str) -> bool;
    /// [`PeerSession::return_consumed`], which is cheap and never blocks.
    fn give_back(&self, key: &MessageKey) -> bool;
    /// [`PeerSession::release_consumed`] of a delivery as a firing's row stores it.
    fn release(&self, delivery: &str) -> MessagingFuture<Result<(), ReleaseError>>;
    /// [`PeerSession::history_version`].
    fn history_version(&self) -> MessagingFuture<Result<HistoryVersion, String>>;
    /// [`PeerSession::work_cursor`].
    fn work_cursor(&self) -> MessagingFuture<Result<WorkCursor, String>>;
    /// [`PeerSession::published_work_since`].
    fn published_work_since(
        &self,
        after: WorkCursor,
        limit: usize,
    ) -> MessagingFuture<Result<Vec<(WorkCursor, WorkItem)>, String>>;
}

#[derive(Debug, Error)]
pub enum ReleaseError {
    /// Another build wrote it, or it is damaged: no release can read it.
    #[error("the stored delivery no longer reads as a message: {0}")]
    Unreadable(String),
    /// The session closed, so a later one takes it back.
    #[error("{0}")]
    Refused(String),
}

/// What the observers match against: the armed automations with a `message_received` trigger
/// in name order and the pause latch, which the actor publishes, and whether a session is
/// attached, which the handle sets before the actor learns of it, so the replay of an observer
/// installed right after may take messages. A consuming trigger takes one only while a session
/// is attached and the latch is clear.
#[derive(Default)]
pub(super) struct Watching {
    pub(super) attached: bool,
    pub(super) paused: bool,
    pub(super) armed: Vec<(String, Arc<CatalogEntry>)>,
}

/// Turns the messages a session receives into events for the automations they match.
pub(super) struct Observer {
    pub(super) watching: Arc<ArcSwap<Watching>>,
    pub(super) commands: flume::Sender<Command>,
}

/// A message the session offered, with the automations whose triggers it matched.
pub(super) struct Observed {
    pub(super) key: MessageKey,
    /// The key with the admission, so a message fires each trigger once as it is held and once
    /// as it is queued.
    pub(super) event_key: String,
    /// As an automation that did not take it sees it.
    pub(super) detail: MessageDetail,
    /// In name order.
    pub(super) matches: Vec<Matched>,
}

/// An automation a message matched, with the index of the trigger that matched it and, when
/// the automation took the message from the model, the delivery that releases it.
pub(super) struct Matched {
    pub(super) automation: String,
    pub(super) index: usize,
    pub(super) taken: Option<String>,
}

/// A `reply`, `send`, `publish` or `broadcast` the actor admitted, which goes out as its
/// automation off the actor.
pub(super) struct Outgoing {
    origin: SendOrigin,
    request_id: String,
    text: String,
    recipients: Recipients,
}

enum Recipients {
    Direct {
        target: String,
        reply_to: Option<MessageKey>,
    },
    Published(PeerAudience),
}

impl Messaging for PeerSession {
    fn send(
        &self,
        origin: SendOrigin,
        target: String,
        text: String,
        reply_to: Option<MessageKey>,
        request_id: String,
    ) -> MessagingFuture<Result<PeerSendReceipt, SendFailure>> {
        let session = self.clone();
        Box::pin(async move {
            session
                .send_from(&origin, &target, &text, reply_to.as_ref(), &request_id)
                .await
        })
    }

    fn publish(
        &self,
        origin: SendOrigin,
        audience: PeerAudience,
        text: String,
        request_id: String,
    ) -> MessagingFuture<Result<PeerPublishReceipt, SendFailure>> {
        let session = self.clone();
        Box::pin(async move {
            session
                .publish_from(&origin, audience, &text, &request_id)
                .await
        })
    }

    fn settle(&self, key: &MessageKey, automation: &str) -> bool {
        self.settle_consumed(key, automation)
    }

    fn give_back(&self, key: &MessageKey) -> bool {
        self.return_consumed(key)
    }

    fn release(&self, delivery: &str) -> MessagingFuture<Result<(), ReleaseError>> {
        let stored = serde_json::from_str::<StoredDelivery>(delivery);
        let session = self.clone();
        Box::pin(async move {
            let stored = stored.map_err(|error| ReleaseError::Unreadable(error.to_string()))?;
            session
                .release_consumed(&stored)
                .await
                .map_err(ReleaseError::Refused)
        })
    }

    fn history_version(&self) -> MessagingFuture<Result<HistoryVersion, String>> {
        let session = self.clone();
        Box::pin(async move { session.history_version().await })
    }

    fn work_cursor(&self) -> MessagingFuture<Result<WorkCursor, String>> {
        let session = self.clone();
        Box::pin(async move { session.work_cursor().await })
    }

    fn published_work_since(
        &self,
        after: WorkCursor,
        limit: usize,
    ) -> MessagingFuture<Result<Vec<(WorkCursor, WorkItem)>, String>> {
        let session = self.clone();
        Box::pin(async move { session.published_work_since(after, limit).await })
    }
}

/// Every automation whose triggers match a message observes it. The first by name whose
/// matching trigger consumes takes it, if the message is queued and taking is allowed; the
/// session hands it over only once the actor has the event.
impl MessageObserver for Observer {
    fn observe(&self, message: &ObservedMessage) -> Option<String> {
        let watching = self.watching.load();
        if watching.armed.is_empty() {
            return None;
        }
        let event = EventDetail::MessageReceived(message_detail(message));
        let mut takes = watching.attached && !watching.paused && !message.held;
        let mut matches = Vec::new();
        for (name, entry) in &watching.armed {
            let triggers = &entry.meta.triggers;
            let Some(index) = first_match(triggers, &event, &Topics) else {
                continue;
            };
            let taken = (takes && triggers.get(index).is_some_and(consumes))
                .then(|| serde_json::to_string(&message.delivery).ok())
                .flatten();
            takes &= taken.is_none();
            matches.push(Matched {
                automation: name.clone(),
                index,
                taken,
            });
        }
        let EventDetail::MessageReceived(detail) = event else {
            return None;
        };
        if matches.is_empty() {
            return None;
        }
        let taker = matches
            .iter()
            .find(|matched| matched.taken.is_some())
            .map(|matched| matched.automation.clone());
        let observed = Observed {
            key: message.key.clone(),
            event_key: event_key(&message.key, detail.admission),
            detail,
            matches,
        };
        let sent = self
            .commands
            .try_send(Command::Observed(Box::new(observed)))
            .is_ok();
        taker.filter(|_| sent)
    }
}

impl Outgoing {
    /// `request` as its automation sends it: a reply answers the sender of the consumed message
    /// `event` carries, citing it.
    pub(super) fn new(
        request: &ActionRequest,
        event: &Event,
        automation: &str,
        fire_id: &str,
        seq: u64,
    ) -> HostResult<Self> {
        let (text, recipients) = match request {
            ActionRequest::Reply { text } => (text, reply_recipient(event)?),
            ActionRequest::Send(send) => (
                &send.text,
                Recipients::Direct {
                    target: send.to.clone(),
                    reply_to: send
                        .reply_to
                        .as_deref()
                        .map(str::parse::<MessageKey>)
                        .transpose()
                        .map_err(failure)?,
                },
            ),
            ActionRequest::Publish { topic, text } => (
                text,
                Recipients::Published(PeerAudience::Topic {
                    topic: topic.clone(),
                }),
            ),
            ActionRequest::Broadcast { text } => {
                (text, Recipients::Published(PeerAudience::Broadcast))
            }
            _ => return Err(not_in_this_build(request.kind())),
        };
        Ok(Self {
            origin: SendOrigin::Automation(automation.to_owned()),
            request_id: request_id(fire_id, seq),
            text: text.clone(),
            recipients,
        })
    }

    /// What the trace says it went to: the recipient or the topic.
    pub(super) fn target(&self) -> Option<String> {
        match &self.recipients {
            Recipients::Direct { target, .. } => Some(target.clone()),
            Recipients::Published(PeerAudience::Topic { topic }) => Some(topic.clone()),
            Recipients::Published(PeerAudience::Direct | PeerAudience::Broadcast) => None,
        }
    }

    /// Sends it through `messaging`, answering as the script reads it: a receipt, or a failure
    /// it may catch.
    pub(super) fn send(
        self,
        messaging: &dyn Messaging,
    ) -> MessagingFuture<HostResult<ActionReply>> {
        match self.recipients {
            Recipients::Direct { target, reply_to } => {
                let sending =
                    messaging.send(self.origin, target, self.text, reply_to, self.request_id);
                Box::pin(async move {
                    sending
                        .await
                        .map(|receipt| ActionReply::Sent(send_receipt(receipt)))
                        .map_err(failure)
                })
            }
            Recipients::Published(audience) => {
                let publishing =
                    messaging.publish(self.origin, audience, self.text, self.request_id);
                Box::pin(async move {
                    publishing
                        .await
                        .map(|receipt| ActionReply::Published(publish_receipt(receipt)))
                        .map_err(failure)
                })
            }
        }
    }
}

/// The idempotency key of a firing's action `seq`, which a retry within the peers' window
/// reuses.
pub(super) fn request_id(fire_id: &str, seq: u64) -> String {
    format!("{REQUEST_ID_PREFIX}:{fire_id}:{seq}")
}

pub(super) fn failure_kind(kind: &SendFailureKind) -> FailureKind {
    match kind {
        SendFailureKind::Refused => FailureKind::Refused,
        SendFailureKind::RateLimited => FailureKind::RateLimited,
        SendFailureKind::Unavailable => FailureKind::Unavailable,
        SendFailureKind::UnknownRecipient => FailureKind::UnknownRecipient,
        SendFailureKind::GroupFull => FailureKind::GroupFull,
        SendFailureKind::ReadOnly => FailureKind::ReadOnly,
        SendFailureKind::Invalid => FailureKind::InvalidArgument,
    }
}

fn failure(failure: SendFailure) -> HostError {
    Failure::new(failure_kind(&failure.kind), failure.reason).into()
}

/// The sender of the consumed message, with the message as the reply cites it. The engine let
/// only a session's message through, so a missing sender is a reply with no target.
fn reply_recipient(event: &Event) -> HostResult<Recipients> {
    let EventDetail::MessageReceived(message) = &event.detail else {
        return Err(Failure::new(FailureKind::NoReplyTarget, NO_REPLY_SENDER).into());
    };
    let target = message
        .sender
        .clone()
        .filter(|sender| !sender.is_empty())
        .ok_or_else(|| {
            HostError::from(Failure::new(FailureKind::NoReplyTarget, NO_REPLY_SENDER))
        })?;
    let reply_to = message.message_id.parse::<MessageKey>().map_err(failure)?;
    Ok(Recipients::Direct {
        target,
        reply_to: Some(reply_to),
    })
}

fn consumes(trigger: &Trigger) -> bool {
    matches!(trigger, Trigger::MessageReceived(filter) if filter.consume)
}

fn event_key(key: &MessageKey, admission: Admission) -> String {
    format!("{}{EVENT_KEY_SEPARATOR}{key}", spelled(admission))
}

/// A message as the event of an automation that did not take it carries it. Its id is the
/// message key, which `send()` takes back as `reply_to`.
fn message_detail(message: &ObservedMessage) -> MessageDetail {
    let (sender_kind, sender, sender_automation, sender_label) = match &message.sender {
        ObservedSender::Session { handle } => {
            (SenderKind::Session, Some(handle.clone()), None, None)
        }
        ObservedSender::Automation { handle, automation } => (
            SenderKind::Automation,
            Some(handle.clone()),
            Some(automation.clone()),
            None,
        ),
        ObservedSender::Script { label } => {
            (SenderKind::Script, None, None, Some(Untrusted::text(label)))
        }
    };
    let (audience, topic) = audience(&message.audience);
    MessageDetail {
        message_id: message.key.to_string(),
        audience,
        topic,
        sender_title: (sender_kind != SenderKind::Script).then(|| Untrusted::text(&message.title)),
        sender_cwd: message
            .cwd
            .as_ref()
            .map(|cwd| Untrusted::text(cwd.to_string_lossy())),
        sender_kind,
        sender,
        sender_automation,
        sender_label,
        text: Untrusted::text(&message.text),
        reply_to: message.reply_to.clone(),
        admission: if message.held {
            Admission::Held
        } else {
            Admission::Queued
        },
        delivery: if message.catch_up {
            Delivery::CatchUp
        } else {
            Delivery::Live
        },
        consumed: false,
    }
}

fn audience(audience: &PeerAudience) -> (Audience, Option<String>) {
    match audience {
        PeerAudience::Direct => (Audience::Direct, None),
        PeerAudience::Topic { topic } => (Audience::Topic, Some(topic.clone())),
        PeerAudience::Broadcast => (Audience::Broadcast, None),
    }
}

/// A recipient that consumed the message took it as it would have queued it.
fn send_receipt(receipt: PeerSendReceipt) -> SendReceipt {
    let status = match receipt.status.as_str() {
        STATUS_QUEUED | STATUS_CONSUMED => SendStatus::Queued,
        STATUS_HELD => SendStatus::Held,
        _ => SendStatus::Unknown,
    };
    SendReceipt {
        status,
        message_id: receipt.message_id,
        reason: receipt.reason,
    }
}

fn publish_receipt(receipt: PeerPublishReceipt) -> PublishReceipt {
    PublishReceipt {
        message_id: receipt.message_id,
        audience: audience(&receipt.audience).0,
        recipients: receipt
            .recipients
            .into_iter()
            .map(|recipient| RecipientReceipt {
                name: recipient.target,
                title: recipient.title,
                status: recipient_status(&recipient.status),
                reason: recipient.reason,
            })
            .collect(),
        skipped: u32::try_from(receipt.skipped).unwrap_or(u32::MAX),
        queued: receipt
            .queued
            .into_iter()
            .map(|work| QueuedWork {
                group: work.group,
                work: work.work,
            })
            .collect(),
    }
}

fn recipient_status(status: &str) -> RecipientStatus {
    match status {
        STATUS_QUEUED | STATUS_CONSUMED => RecipientStatus::Queued,
        STATUS_HELD => RecipientStatus::Held,
        STATUS_REFUSED => RecipientStatus::Refused,
        STATUS_UNAVAILABLE => RecipientStatus::Unavailable,
        STATUS_RATE_LIMITED => RecipientStatus::RateLimited,
        _ => RecipientStatus::Unknown,
    }
}
