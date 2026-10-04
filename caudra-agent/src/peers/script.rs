//! Messages from a process outside every session, such as a script or a CI
//! job. A script binds no socket, so nothing can reply to it.

use std::path::{Path, PathBuf};

use caudra_config::MessagingConfig;
use caudra_providers::PeerAudience;
use caudra_storage::id::CaudraId;
use caudra_storage::messages::MessageRecipient;
use caudra_storage::sessions::PermissionMode;
use sha2::{Digest, Sha256};

#[cfg(not(unix))]
use super::UNAVAILABLE;
use super::history::{HistoryWriter, MessageHistory};
#[cfg(unix)]
use super::unix::{self, Directory};
use super::{
    Delivery, FANOUT_CONCURRENCY, MAX_LABEL_BYTES, NOT_RECORDED, PeerInfo, PublishReceipt,
    RecipientReceipt, Route, SendReceipt, Sender, WireMode, audience_members, check_publication,
    check_text, deceptive, holder, message_name, parse_handle_address, recipient_room, token,
    wall_ms,
};

const SESSION_DOMAIN: &[u8] = b"caudra script sender\0";
const SESSION_BYTES: usize = 16;
pub const DEFAULT_LABEL: &str = "script";
pub const INVALID_LABEL: &str =
    "A sender label needs 1 to 256 bytes of text without control or invisible characters";

pub struct ScriptSender {
    label: String,
    cwd: Option<PathBuf>,
    max_fanout: usize,
    #[cfg(unix)]
    directory: Directory,
    history: MessageHistory,
    /// Declared after `history`, so the queue closes before the join.
    _writer: HistoryWriter,
}

impl ScriptSender {
    /// Sends as `label` from `cwd`, recording into the history every
    /// session of this user shares.
    pub fn open(messaging: &MessagingConfig, label: &str, cwd: &Path) -> Result<Self, String> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            Self::bind(
                unix::runtime_directory()?,
                MessageHistory::open_shared(messaging)?,
                messaging,
                label,
                cwd,
            )
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (messaging, label, cwd);
            Err(super::UNAVAILABLE.into())
        }
    }

    #[cfg(unix)]
    fn bind(
        directory: PathBuf,
        (history, writer): (MessageHistory, HistoryWriter),
        messaging: &MessagingConfig,
        label: &str,
        cwd: &Path,
    ) -> Result<Self, String> {
        Ok(Self {
            label: parse_label(label)?,
            cwd: cwd.canonicalize().ok(),
            max_fanout: messaging.max_fanout,
            directory: Directory::open(directory)?,
            history,
            _writer: writer,
        })
    }

    /// Sends `text` to every live session `audience` reaches, up to
    /// `max_fanout`. The message is recorded even when it reaches no one, so
    /// a topic message still serves catch-up.
    pub async fn publish(
        &self,
        audience: PeerAudience,
        text: &str,
    ) -> Result<PublishReceipt, String> {
        check_publication(&audience, text)?;
        let groups = self
            .history
            .group_destinations(&audience)
            .await
            .map_err(|error| format!("{NOT_RECORDED}: {error}"))?;
        let room = recipient_room(groups, self.max_fanout)?;
        let (selected, skipped) = audience_members(self.discover().await?, &audience, room);
        self.fan_out(audience, text, selected, skipped).await
    }

    /// Sends `text` to the live session holding the messaging name `handle`,
    /// with or without its `@`.
    pub async fn send(&self, handle: &str, text: &str) -> Result<PublishReceipt, String> {
        let handle = parse_handle_address(handle)?;
        check_text(text)?;
        let recipient = holder(self.discover().await?, &handle)?;
        self.fan_out(PeerAudience::Direct, text, vec![recipient], 0)
            .await
    }

    async fn discover(&self) -> Result<Vec<PeerInfo>, String> {
        #[cfg(unix)]
        {
            unix::discover(&self.directory).await
        }
        #[cfg(not(unix))]
        Err(UNAVAILABLE.into())
    }

    fn deliver(&self, delivery: Delivery) -> smol::Task<SendReceipt> {
        #[cfg(unix)]
        {
            let directory = self.directory.clone();
            smol::spawn(async move { unix::deliver(&directory, delivery, || Ok(())).await })
        }
        #[cfg(not(unix))]
        smol::spawn(async move {
            SendReceipt::new("unavailable", &delivery.message_id, Some(UNAVAILABLE))
        })
    }

    /// Records the message for `peers`, with the work it queues for consumer
    /// groups, first, then delivers it to each.
    async fn fan_out(
        &self,
        audience: PeerAudience,
        text: &str,
        peers: Vec<PeerInfo>,
        skipped: usize,
    ) -> Result<PublishReceipt, String> {
        let template = Delivery {
            message_id: message_name()?,
            issued_ms: wall_ms(),
            target: String::new(),
            sender: self.sender()?,
            text: text.to_owned(),
            reply_to: None,
            reply_sender: None,
            audience,
        };
        let recipients = peers
            .iter()
            .map(|peer| MessageRecipient {
                session: peer.session_id.to_string(),
                name: Some(peer.name.clone()),
                handle: peer.handle.clone(),
            })
            .collect();
        let max_work = self.max_fanout.saturating_sub(peers.len());
        let queued = self
            .history
            .record_publication(template.history_entry(), recipients, max_work)
            .await
            .map_err(|error| format!("{NOT_RECORDED}: {error}"))?;
        let sender = template.sender.route.target();
        let mut receipts = Vec::with_capacity(peers.len());
        for batch in peers.chunks(FANOUT_CONCURRENCY) {
            let sends: Vec<_> = batch
                .iter()
                .map(|peer| {
                    self.deliver(Delivery {
                        target: peer.target.clone(),
                        ..template.clone()
                    })
                })
                .collect();
            for (peer, send) in batch.iter().zip(sends) {
                let sent = send.await;
                self.history.receipt(
                    sender.clone(),
                    template.message_id.clone(),
                    peer.session_id.to_string(),
                    sent.status.clone(),
                    sent.reason.clone(),
                );
                receipts.push(RecipientReceipt {
                    target: String::new(),
                    title: peer.name.clone(),
                    handle: peer.handle.clone(),
                    status: sent.status,
                    reason: sent.reason,
                });
            }
        }
        Ok(PublishReceipt {
            message_id: template.message_id,
            audience: template.audience,
            recipients: receipts,
            skipped,
            queued,
        })
    }

    /// A fresh route nothing listens on, under the session `label` names.
    /// Recipients never weigh a script's mode or permissions, because no
    /// cohort includes a sender outside every session.
    fn sender(&self) -> Result<Sender, String> {
        Ok(Sender {
            route: Route {
                host: token()?,
                session: script_session(&self.label),
                generation: token()?,
            },
            name: self.label.clone(),
            handle: None,
            canonical_cwd: self.cwd.clone(),
            mode: WireMode::Build,
            permission_mode: PermissionMode::Ask,
            external: true,
        })
    }
}

/// Every message under one label shares a session, which groups its direct
/// messages into one conversation and its arrivals under one rate limit.
fn script_session(label: &str) -> CaudraId {
    let digest = Sha256::new()
        .chain_update(SESSION_DOMAIN)
        .chain_update(label)
        .finalize();
    let mut bytes = [0; SESSION_BYTES];
    bytes.copy_from_slice(&digest[..SESSION_BYTES]);
    CaudraId::from_bytes(bytes)
}

/// Validates the name a script sends under, as `--from` spells it.
pub fn parse_label(label: &str) -> Result<String, String> {
    if label.is_empty()
        || label.len() > MAX_LABEL_BYTES
        || label
            .chars()
            .any(|character| character.is_control() || deceptive(character))
    {
        Err(INVALID_LABEL.into())
    } else {
        Ok(label.to_owned())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::Path;

    use caudra_config::{InboundPolicy, MessagingConfig};
    use caudra_providers::PeerAudience;
    use caudra_storage::sessions::StoredPeerControls;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{INVALID_LABEL, MAX_LABEL_BYTES, ScriptSender, parse_label};
    use crate::peers::history::MessageHistory;
    use crate::peers::tests::{descriptor, directory, host};
    use crate::peers::{
        DUPLICATE, HELD_EXTERNAL, HELD_POLICY, INVALID_HANDLE, NOT_RECORDED, PeerHost, PeerSession,
        RATE_EXCEEDED, REFUSED_POLICY, TEST_HISTORY_DIRECTORY, UNKNOWN_HANDLE,
    };

    const TEXT: &str = "Nightly build 1042 failed in the parser tests";
    const OTHER_TEXT: &str = "Nightly build 1043 passed";
    const THIRD_TEXT: &str = "Nightly build 1044 is running";
    const TOPIC: &str = "ci.failures";
    const PATTERN: &str = "ci.*";
    const HANDLE: &str = "ci-watcher";
    const ADDRESSED_HANDLE: &str = "@ci-watcher";
    const MISSING_HANDLE: &str = "nobody-here";
    const MALFORMED_HANDLE: &str = "CI_Watcher";
    const LABEL: &str = "nightly-ci";
    const OTHER_LABEL: &str = "release bot";
    const QUEUED: &str = "queued";
    const HELD: &str = "held";
    const REFUSED: &str = "refused";
    const RATE_LIMITED: &str = "rate_limited";
    const LOW_RATE: usize = 2;
    const LOW_FANOUT: usize = 1;

    fn topic() -> PeerAudience {
        PeerAudience::Topic {
            topic: TOPIC.into(),
        }
    }

    fn script(directory: &TempDir, messaging: &MessagingConfig, label: &str) -> ScriptSender {
        let path = directory.path();
        let history = MessageHistory::open_in(&path.join(TEST_HISTORY_DIRECTORY), messaging);
        ScriptSender::bind(path.to_owned(), history.unwrap(), messaging, label, path).unwrap()
    }

    fn session(
        host: &PeerHost,
        cwd: &Path,
        inbound: InboundPolicy,
        messaging: &MessagingConfig,
        controls: StoredPeerControls,
    ) -> PeerSession {
        let session = host
            .register_with_controls(descriptor(cwd, inbound), messaging, Some(controls))
            .unwrap();
        session.claim_handle().unwrap();
        session
    }

    fn subscriber(host: &PeerHost, cwd: &Path) -> PeerSession {
        let controls = StoredPeerControls {
            topics: vec![PATTERN.into()],
            ..StoredPeerControls::default()
        };
        session(
            host,
            cwd,
            InboundPolicy::Accept,
            &MessagingConfig::default(),
            controls,
        )
    }

    fn named(
        host: &PeerHost,
        cwd: &Path,
        inbound: InboundPolicy,
        messaging: &MessagingConfig,
    ) -> PeerSession {
        let controls = StoredPeerControls {
            handle: Some(HANDLE.into()),
            ..StoredPeerControls::default()
        };
        session(host, cwd, inbound, messaging, controls)
    }

    /// One run of a script under `label`, as a fresh process sends it.
    async fn send_as(directory: &TempDir, label: &str, text: &str) -> (String, Option<String>) {
        let receipt = script(directory, &MessagingConfig::default(), label)
            .send(HANDLE, text)
            .await
            .unwrap();
        let recipient = receipt.recipients.into_iter().next().unwrap();
        (recipient.status, recipient.reason)
    }

    #[test]
    fn publications_reach_subscribed_sessions_up_to_max_fanout() {
        smol::block_on(async {
            let directory = directory();
            let host = host(directory.path());
            let subscribers = [
                subscriber(&host, directory.path()),
                subscriber(&host, directory.path()),
            ];
            let unsubscribed = session(
                &host,
                directory.path(),
                InboundPolicy::Accept,
                &MessagingConfig::default(),
                StoredPeerControls::default(),
            );
            let messaging = MessagingConfig {
                max_fanout: LOW_FANOUT,
                ..MessagingConfig::default()
            };
            let receipt = script(&directory, &messaging, LABEL)
                .publish(topic(), TEXT)
                .await
                .unwrap();
            assert_eq!(receipt.recipients.len(), LOW_FANOUT);
            assert_eq!(receipt.recipients[0].status, QUEUED);
            assert!(receipt.recipients[0].target.is_empty());
            assert_eq!(receipt.skipped, subscribers.len() - LOW_FANOUT);
            let reached = subscribers.iter().filter(|session| session.has_pending());
            assert_eq!(reached.count(), LOW_FANOUT);
            assert!(!unsubscribed.has_pending());
        });
    }

    #[test]
    fn a_publication_reaching_no_one_is_caught_up_as_a_script_message() {
        smol::block_on(async {
            let directory = directory();
            let host = host(directory.path());
            let receipt = script(&directory, &MessagingConfig::default(), LABEL)
                .publish(topic(), TEXT)
                .await
                .unwrap();
            assert!(receipt.recipients.is_empty());
            let late = subscriber(&host, directory.path());
            late.catch_up().await.unwrap();
            assert!(!late.has_pending());
            let claim = late.claim().unwrap();
            let origin = claim.messages()[0].peer_event.clone().unwrap();
            assert!(origin.external);
            assert_eq!(origin.message_id, receipt.message_id);
            assert_eq!(origin.sender_name, LABEL);
            assert!(origin.reply_target.is_empty());
            claim.commit();
        });
    }

    #[test_case(HANDLE, None; "bare_name")]
    #[test_case(ADDRESSED_HANDLE, None; "addressed_name")]
    #[test_case(MISSING_HANDLE, Some(UNKNOWN_HANDLE); "unknown_name")]
    #[test_case(MALFORMED_HANDLE, Some(INVALID_HANDLE); "malformed_name")]
    fn direct_messages_reach_the_session_holding_a_name(name: &str, refusal: Option<&str>) {
        smol::block_on(async {
            let directory = directory();
            let host = host(directory.path());
            let receiver = named(
                &host,
                directory.path(),
                InboundPolicy::Accept,
                &MessagingConfig::default(),
            );
            let sent = script(&directory, &MessagingConfig::default(), LABEL)
                .send(name, TEXT)
                .await;
            let Some(refusal) = refusal else {
                let receipt = sent.unwrap();
                assert!(receipt.audience.is_direct());
                assert_eq!(receipt.recipients[0].handle.as_deref(), Some(HANDLE));
                assert_eq!(receipt.recipients[0].status, QUEUED);
                let claim = receiver.claim().unwrap();
                let origin = claim.messages()[0].peer_event.clone().unwrap();
                assert!(origin.external);
                assert!(origin.reply_target.is_empty());
                assert_eq!(origin.sender_name, LABEL);
                claim.commit();
                return;
            };
            assert_eq!(sent.unwrap_err(), refusal);
            assert!(!receiver.has_pending());
        });
    }

    #[test_case(InboundPolicy::Auto, HELD, Some(HELD_EXTERNAL); "auto_holds")]
    #[test_case(InboundPolicy::Accept, QUEUED, None; "accept_queues")]
    #[test_case(InboundPolicy::Hold, HELD, Some(HELD_POLICY); "hold_holds")]
    #[test_case(InboundPolicy::Refuse, REFUSED, Some(REFUSED_POLICY); "refuse_refuses")]
    fn script_messages_never_join_a_cohort(
        inbound: InboundPolicy,
        status: &str,
        reason: Option<&str>,
    ) {
        smol::block_on(async {
            let directory = directory();
            let host = host(directory.path());
            let receiver = named(
                &host,
                directory.path(),
                inbound,
                &MessagingConfig::default(),
            );
            let outcome = send_as(&directory, LABEL, TEXT).await;
            assert_eq!(outcome, (status.to_owned(), reason.map(str::to_owned)));
            let held = receiver.inbox_snapshot().unwrap().messages;
            assert_eq!(held.len(), usize::from(status == HELD));
            assert!(
                held.iter()
                    .all(|message| message.external && message.reply_target.is_empty())
            );
        });
    }

    #[test]
    fn one_label_shares_duplicate_suppression_and_rate_limits_across_runs() {
        smol::block_on(async {
            let directory = directory();
            let host = host(directory.path());
            let messaging = MessagingConfig {
                sender_per_minute: LOW_RATE,
                ..MessagingConfig::default()
            };
            let _receiver = named(&host, directory.path(), InboundPolicy::Accept, &messaging);
            for (label, text, status, reason) in [
                (LABEL, TEXT, QUEUED, None),
                (LABEL, TEXT, REFUSED, Some(DUPLICATE)),
                (LABEL, OTHER_TEXT, QUEUED, None),
                (LABEL, THIRD_TEXT, RATE_LIMITED, Some(RATE_EXCEEDED)),
                (OTHER_LABEL, THIRD_TEXT, QUEUED, None),
            ] {
                assert_eq!(
                    send_as(&directory, label, text).await,
                    (status.to_owned(), reason.map(str::to_owned)),
                    "{label}: {text}"
                );
            }
        });
    }

    #[test]
    fn messages_the_history_cannot_record_are_never_sent() {
        smol::block_on(async {
            let directory = directory();
            let host = host(directory.path());
            let receiver = subscriber(&host, directory.path());
            let messaging = MessagingConfig::default();
            let unrecorded = ScriptSender::bind(
                directory.path().to_owned(),
                MessageHistory::stopped(),
                &messaging,
                LABEL,
                directory.path(),
            )
            .unwrap();
            let error = unrecorded.publish(topic(), TEXT).await.unwrap_err();
            assert!(error.starts_with(NOT_RECORDED), "{error}");
            assert!(!receiver.has_pending());
        });
    }

    #[test_case(LABEL, true; "plain_label")]
    #[test_case(OTHER_LABEL, true; "spaced_label")]
    #[test_case("", false; "empty_label")]
    #[test_case("ci\u{1b}[31m", false; "terminal_escape")]
    #[test_case("ci\u{202e}bot", false; "bidirectional_override")]
    fn labels_reject_control_and_invisible_characters(label: &str, valid: bool) {
        let expected = if valid {
            Ok(label.to_owned())
        } else {
            Err(INVALID_LABEL.to_owned())
        };
        assert_eq!(parse_label(label), expected);
    }

    #[test]
    fn labels_are_bounded() {
        assert!(parse_label(&"a".repeat(MAX_LABEL_BYTES)).is_ok());
        assert_eq!(
            parse_label(&"a".repeat(MAX_LABEL_BYTES + 1)).unwrap_err(),
            INVALID_LABEL
        );
    }
}
