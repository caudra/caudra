//! `caudra message`: cross-session messages from scripts and CI jobs.

use std::collections::HashMap;
use std::env;
use std::io::{self, IsTerminal, Read, Write};
use std::path::Path;
use std::process::ExitCode;

use caudra_agent::peers::script::ScriptSender;
use caudra_agent::peers::topics::pattern_matches;
use caudra_agent::peers::{
    MAX_BODY_BYTES, PublishReceipt, RecipientReceipt, handle_address, history_retention, literal,
};
use caudra_config::MessagingConfig;
use caudra_lua::PluginHost;
use caudra_providers::{PEER_SCRIPT_SENDER, PEER_SESSION_SENDER, PeerAudience};
use caudra_storage::StateDir;
use caudra_storage::messages::{DeliveryRecord, HistoryChannel, MessageLog, StoredMessage};
use color_eyre::Result;
use color_eyre::eyre::{Context, bail, eyre};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::Serialize;

use super::load_config;
use crate::cli::{Cli, MessageAction};

const LOCAL_ONLY: &str = "cross-session messaging uses this machine's persistent storage; drop --ephemeral and any remote Workcell selector";
const NEEDS_TEXT: &str = "pass the message text as an argument or pipe it on stdin";
const TEXT_TOO_LONG: &str = "message text exceeds 32 KiB";
const NOT_UTF8: &str = "message text on stdin is not UTF-8";
const NEWLINE: &[u8] = b"\n";
const CARRIAGE_RETURN: &[u8] = b"\r";
const TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";
const ADMITTED: [&str; 2] = ["queued", "held"];
const INDENT: &str = "  ";
const RECIPIENT_PREFIX: &str = "  -> ";
const SEPARATOR: &str = " · ";
const UNKNOWN_SESSION: &str = "unknown session";
const UNKNOWN_TIME: &str = "-";
const NO_MESSAGES: &str = "No recorded messages";
const RECIPIENT: &str = "recipient";
const RECIPIENTS: &str = "recipients";
const SKIPPED: &str = "skipped past max_fanout";

pub fn run(action: MessageAction, cli: &Cli) -> Result<ExitCode> {
    if cli.workcell.is_set() || cli.ephemeral {
        bail!(LOCAL_ONLY);
    }
    let cwd = env::current_dir().context("read the working directory")?;
    let messaging = load_config(&PluginHost::disabled(), cli, &cwd, false)?
        .agent
        .messaging;
    let (audience, name, message) = match action {
        MessageAction::Log {
            topic,
            broadcast,
            with,
            limit,
            json,
        } => {
            let filter = Filter {
                topic,
                broadcast,
                with,
            };
            print_log(&messaging, filter, limit, json)?;
            return Ok(ExitCode::SUCCESS);
        }
        MessageAction::Publish { topic, message } => (PeerAudience::Topic { topic }, None, message),
        MessageAction::Broadcast { message } => (PeerAudience::Broadcast, None, message),
        MessageAction::Send { to, message } => (PeerAudience::Direct, Some(to), message),
    };
    let text = match message.text {
        Some(text) => text,
        None if io::stdin().is_terminal() => return Err(eyre!(NEEDS_TEXT)),
        None => read_text(io::stdin().lock())?,
    };
    let receipt = send(&messaging, &cwd, &message.from, audience, name, &text)?;
    let mut out = io::stdout().lock();
    if message.json {
        writeln!(out, "{}", serde_json::to_string(&receipt)?)?;
    } else {
        write!(out, "{}", render_receipt(&receipt))?;
    }
    Ok(if succeeded(&receipt) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn send(
    messaging: &MessagingConfig,
    cwd: &Path,
    label: &str,
    audience: PeerAudience,
    name: Option<String>,
    text: &str,
) -> Result<PublishReceipt> {
    let sender = ScriptSender::open(messaging, label, cwd).map_err(|error| eyre!(error))?;
    smol::block_on(async {
        match name {
            Some(name) => sender.send(&name, text).await,
            None => sender.publish(audience, text).await,
        }
    })
    .map_err(|error| eyre!(error))
}

/// Reads at most the body limit plus the line ending a pipe such as `echo`
/// adds, which the message drops.
fn read_text(reader: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_BODY_BYTES + CARRIAGE_RETURN.len() + NEWLINE.len() + 1) as u64)
        .read_to_end(&mut bytes)
        .context("read the message text from stdin")?;
    let body = bytes.strip_suffix(NEWLINE).map_or(&bytes[..], |line| {
        line.strip_suffix(CARRIAGE_RETURN).unwrap_or(line)
    });
    if body.len() > MAX_BODY_BYTES {
        bail!(TEXT_TOO_LONG);
    }
    String::from_utf8(body.to_vec()).map_err(|_| eyre!(NOT_UTF8))
}

/// A publication succeeds once recorded, even with no live recipient,
/// because its record serves catch-up. A direct message succeeds only when
/// its recipient admitted it.
fn succeeded(receipt: &PublishReceipt) -> bool {
    !receipt.audience.is_direct()
        || receipt
            .recipients
            .iter()
            .all(|recipient| ADMITTED.contains(&recipient.status.as_str()))
}

fn render_receipt(receipt: &PublishReceipt) -> String {
    let count = receipt.recipients.len();
    let noun = if count == 1 { RECIPIENT } else { RECIPIENTS };
    let mut summary = format!("{count} {noun}");
    if receipt.skipped > 0 {
        summary.push_str(&format!(", {} {SKIPPED}", receipt.skipped));
    }
    let name = &receipt.message_id;
    let mut text = match &receipt.audience {
        PeerAudience::Direct => format!("Sent {name} ({summary})\n"),
        audience => format!(
            "Published {name} to {} ({summary})\n",
            literal(&audience.to_string(), false)
        ),
    };
    for recipient in &receipt.recipients {
        text.push_str(&format!(
            "{INDENT}{}: {}",
            recipient_name(recipient),
            literal(&recipient.status, false)
        ));
        if let Some(reason) = &recipient.reason {
            text.push_str(SEPARATOR);
            text.push_str(&literal(reason, false));
        }
        text.push('\n');
    }
    text
}

fn recipient_name(recipient: &RecipientReceipt) -> String {
    match &recipient.handle {
        Some(handle) => literal(&handle_address(handle), false),
        None => literal(&recipient.title, false),
    }
}

struct Filter {
    topic: Option<String>,
    broadcast: bool,
    with: Option<String>,
}

fn print_log(messaging: &MessagingConfig, filter: Filter, limit: u32, json: bool) -> Result<()> {
    let state = StateDir::resolve().context("resolve data directory")?;
    let now = u64::try_from(Timestamp::now().as_millisecond()).unwrap_or_default();
    let log = MessageLog::open(&state, &history_retention(messaging), now)?;
    let channel = match filter {
        Filter {
            topic: Some(pattern),
            ..
        } => HistoryChannel::Topics(
            log.directory()?
                .into_iter()
                .map(|summary| summary.topic)
                .filter(|topic| pattern_matches(&pattern, topic))
                .collect(),
        ),
        Filter {
            with: Some(name), ..
        } => HistoryChannel::Named(name),
        Filter {
            broadcast: true, ..
        } => HistoryChannel::Broadcast,
        Filter { .. } => HistoryChannel::All,
    };
    let mut messages = log.history(&channel, None, limit as usize)?;
    messages.reverse();
    let seqs: Vec<i64> = messages.iter().map(|message| message.seq).collect();
    let mut deliveries: HashMap<i64, Vec<DeliveryRecord>> = HashMap::new();
    for delivery in log.deliveries(&seqs)? {
        deliveries.entry(delivery.seq).or_default().push(delivery);
    }
    let mut out = io::stdout().lock();
    if messages.is_empty() && !json {
        writeln!(out, "{NO_MESSAGES}")?;
    }
    for message in &messages {
        let recipients = deliveries.remove(&message.seq).unwrap_or_default();
        if json {
            let entry = LogEntry::new(message, &recipients);
            writeln!(out, "{}", serde_json::to_string(&entry)?)?;
        } else {
            write!(out, "{}", render_entry(message, &recipients))?;
        }
    }
    Ok(())
}

/// One stored message as a script reads it. Session ids and routes stay out.
#[derive(Serialize)]
struct LogEntry<'a> {
    seq: i64,
    message_id: &'a str,
    sent_ms: u64,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    topic: Option<&'a str>,
    sender: LogSender<'a>,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<&'a str>,
    recipients: Vec<LogRecipient<'a>>,
}

#[derive(Serialize)]
struct LogSender<'a> {
    name: &'a str,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    handle: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<&'a str>,
}

#[derive(Serialize)]
struct LogRecipient<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    handle: Option<&'a str>,
    status: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
}

impl<'a> LogEntry<'a> {
    fn new(stored: &'a StoredMessage, recipients: &'a [DeliveryRecord]) -> Self {
        let message = &stored.message;
        let sender = &message.sender;
        Self {
            seq: stored.seq,
            message_id: &message.message_id,
            sent_ms: message.created_ms,
            kind: message.audience.kind(),
            topic: message.audience.topic(),
            sender: LogSender {
                name: &sender.name,
                kind: if sender.external {
                    PEER_SCRIPT_SENDER
                } else {
                    PEER_SESSION_SENDER
                },
                handle: sender.handle.as_deref(),
                cwd: sender.cwd.as_deref(),
            },
            text: &message.text,
            reply_to: message.reply_to.as_deref(),
            recipients: recipients
                .iter()
                .map(|recipient| LogRecipient {
                    name: recipient.recipient_name.as_deref(),
                    handle: recipient.recipient_handle.as_deref(),
                    status: &recipient.status,
                    reason: recipient.reason.as_deref(),
                })
                .collect(),
        }
    }
}

fn render_entry(stored: &StoredMessage, recipients: &[DeliveryRecord]) -> String {
    let message = &stored.message;
    let sender = &message.sender;
    let mut from = literal(&sender.name, false);
    if let Some(handle) = &sender.handle {
        from.push(' ');
        from.push_str(&literal(&handle_address(handle), false));
    }
    if sender.external {
        from.push_str(&format!(" ({PEER_SCRIPT_SENDER})"));
    }
    let audience = match message.audience.topic() {
        Some(topic) => format!("{} {}", message.audience.kind(), literal(topic, false)),
        None => message.audience.kind().to_owned(),
    };
    let mut text = format!(
        "{}{SEPARATOR}{audience}{SEPARATOR}{from}\n",
        local_time(message.created_ms)
    );
    for line in literal(&message.text, true).lines() {
        text.push_str(INDENT);
        text.push_str(line);
        text.push('\n');
    }
    for recipient in recipients {
        let name = match (&recipient.recipient_handle, &recipient.recipient_name) {
            (Some(handle), _) => literal(&handle_address(handle), false),
            (None, Some(name)) => literal(name, false),
            (None, None) => UNKNOWN_SESSION.to_owned(),
        };
        text.push_str(&format!(
            "{RECIPIENT_PREFIX}{name}: {}",
            literal(&recipient.status, false)
        ));
        if let Some(reason) = &recipient.reason {
            text.push_str(SEPARATOR);
            text.push_str(&literal(reason, false));
        }
        text.push('\n');
    }
    text
}

fn local_time(ms: u64) -> String {
    i64::try_from(ms)
        .ok()
        .and_then(|ms| Timestamp::from_millisecond(ms).ok())
        .map_or_else(
            || UNKNOWN_TIME.to_owned(),
            |timestamp| {
                timestamp
                    .to_zoned(TimeZone::system())
                    .strftime(TIME_FORMAT)
                    .to_string()
            },
        )
}

#[cfg(test)]
mod tests {
    use caudra_agent::peers::{MAX_BODY_BYTES, PublishReceipt, RecipientReceipt};
    use caudra_providers::{PEER_SCRIPT_SENDER, PeerAudience};
    use caudra_storage::messages::{
        DeliveryRecord, MessageAudience, MessageSender, NewMessage, StoredMessage,
    };
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::{
        LogEntry, NOT_UTF8, SKIPPED, TEXT_TOO_LONG, read_text, render_entry, render_receipt,
        succeeded,
    };

    const TEXT: &str = "Nightly build 1042 failed";
    const HOSTILE: &str = "evil\u{1b}]0;forged\u{7}\u{202e}name";
    const HOSTILE_LINES: &str = "first\nsecond\u{1b}[31m";
    const MESSAGE_NAME: &str = "brisk-calm-otter";
    const TOPIC: &str = "ci.failures";
    const HANDLE: &str = "ci-watcher";
    const LABEL: &str = "nightly-ci";
    const TITLE: &str = "Parser review";
    const SESSION: &str = "0193a5f6-0000-7000-8000-000000000001";
    const ROUTE: &str = "p1:host:session:generation";
    const CWD: &str = "/work/project";
    const QUEUED: &str = "queued";
    const HELD: &str = "held";
    const REFUSED: &str = "refused";
    const UNAVAILABLE: &str = "unavailable";
    const RATE_LIMITED: &str = "rate_limited";
    const UNKNOWN: &str = "unknown";
    const REASON: &str = "Receiver policy requires local approval";
    const SENT_MS: u64 = 1_790_000_000_000;

    fn recipient(status: &str, handle: Option<&str>, title: &str) -> RecipientReceipt {
        RecipientReceipt {
            target: String::new(),
            title: title.into(),
            handle: handle.map(str::to_owned),
            status: status.into(),
            reason: None,
        }
    }

    fn receipt(audience: PeerAudience, statuses: &[&str]) -> PublishReceipt {
        PublishReceipt {
            message_id: MESSAGE_NAME.into(),
            audience,
            recipients: statuses
                .iter()
                .map(|status| recipient(status, Some(HANDLE), TITLE))
                .collect(),
            skipped: 0,
        }
    }

    fn topic() -> PeerAudience {
        PeerAudience::Topic {
            topic: TOPIC.into(),
        }
    }

    fn stored(name: &str, text: &str, external: bool) -> StoredMessage {
        StoredMessage {
            seq: 7,
            message: NewMessage {
                message_id: MESSAGE_NAME.into(),
                audience: MessageAudience::Topic(TOPIC.into()),
                sender: MessageSender {
                    route: ROUTE.into(),
                    session: SESSION.into(),
                    name: name.into(),
                    handle: None,
                    cwd: Some(CWD.into()),
                    mode: "build".into(),
                    permission: "ask".into(),
                    external,
                },
                text: text.into(),
                reply_to: None,
                created_ms: SENT_MS,
            },
        }
    }

    fn delivery(name: &str, handle: Option<&str>) -> DeliveryRecord {
        DeliveryRecord {
            seq: 7,
            recipient_session: SESSION.into(),
            recipient_name: Some(name.into()),
            recipient_handle: handle.map(str::to_owned),
            status: HELD.into(),
            reason: Some(REASON.into()),
            updated_ms: SENT_MS,
        }
    }

    #[test_case(b"text\n", Ok("text"); "line_feed")]
    #[test_case(b"text\r\n", Ok("text"); "carriage_return_line_feed")]
    #[test_case(b"text\n\n", Ok("text\n"); "only_one_line_end")]
    #[test_case(b"text", Ok("text"); "no_line_end")]
    #[test_case(b"\xff\xfe", Err(NOT_UTF8); "invalid_utf8")]
    fn stdin_text_drops_one_line_end(input: &[u8], expected: Result<&str, &str>) {
        let read = read_text(input).map_err(|error| error.to_string());
        assert_eq!(read, expected.map(str::to_owned).map_err(str::to_owned));
    }

    #[test_case(MAX_BODY_BYTES, "\r\n", true; "limit_with_line_end")]
    #[test_case(MAX_BODY_BYTES + 1, "", false; "one_byte_over")]
    #[test_case(MAX_BODY_BYTES * 4, "", false; "far_over")]
    fn stdin_text_is_bounded(length: usize, line_end: &str, fits: bool) {
        let input = format!("{}{line_end}", "a".repeat(length));
        let read = read_text(input.as_bytes());
        match read {
            Ok(text) => {
                assert!(fits);
                assert_eq!(text.len(), MAX_BODY_BYTES);
            }
            Err(error) => {
                assert!(!fits);
                assert_eq!(error.to_string(), TEXT_TOO_LONG);
            }
        }
    }

    #[test_case(topic(), &[], true; "publication_without_recipients")]
    #[test_case(PeerAudience::Broadcast, &[REFUSED, UNKNOWN], true; "publication_any_outcome")]
    #[test_case(PeerAudience::Direct, &[QUEUED], true; "direct_queued")]
    #[test_case(PeerAudience::Direct, &[HELD], true; "direct_held")]
    #[test_case(PeerAudience::Direct, &[REFUSED], false; "direct_refused")]
    #[test_case(PeerAudience::Direct, &[UNAVAILABLE], false; "direct_unavailable")]
    #[test_case(PeerAudience::Direct, &[RATE_LIMITED], false; "direct_rate_limited")]
    #[test_case(PeerAudience::Direct, &[UNKNOWN], false; "direct_unknown")]
    fn exit_status_follows_the_audience(audience: PeerAudience, statuses: &[&str], ok: bool) {
        assert_eq!(succeeded(&receipt(audience, statuses)), ok);
    }

    #[test]
    fn receipts_name_recipients_and_escape_their_titles() {
        let mut receipt = receipt(topic(), &[QUEUED]);
        receipt.recipients.push(RecipientReceipt {
            reason: Some(REASON.into()),
            ..recipient(HELD, None, HOSTILE)
        });
        receipt.skipped = 3;
        let text = render_receipt(&receipt);
        let mut lines = text.lines();
        let header = lines.next().unwrap();
        assert!(
            header.contains(MESSAGE_NAME) && header.contains(TOPIC),
            "{header}"
        );
        assert!(header.contains(&format!("3 {SKIPPED}")), "{header}");
        assert!(
            lines
                .next()
                .unwrap()
                .contains(&format!("@{HANDLE}: {QUEUED}"))
        );
        let held = lines.next().unwrap();
        assert!(held.contains(REASON), "{held}");
        assert!(!text.contains(['\u{1b}', '\u{7}', '\u{202e}']), "{text:?}");
    }

    #[test]
    fn log_entries_escape_stored_text_and_mark_scripts() {
        let message = stored(HOSTILE, HOSTILE_LINES, true);
        let text = render_entry(&message, &[delivery(TITLE, Some(HANDLE))]);
        assert!(!text.contains(['\u{1b}', '\u{7}', '\u{202e}']), "{text:?}");
        assert!(text.contains(&format!("({PEER_SCRIPT_SENDER})")), "{text}");
        assert!(text.contains(&format!("-> @{HANDLE}: {HELD}")), "{text}");
        assert_eq!(
            text.lines().filter(|line| line.starts_with("  ")).count(),
            3
        );
        let session = render_entry(&stored(LABEL, TEXT, false), &[]);
        assert!(!session.contains(PEER_SCRIPT_SENDER), "{session}");
    }

    #[test]
    fn json_log_entries_leave_out_sessions_and_routes() {
        let message = stored(LABEL, TEXT, true);
        let recipients = [delivery(TITLE, Some(HANDLE))];
        let entry: Value = serde_json::to_value(LogEntry::new(&message, &recipients)).unwrap();
        assert_eq!(
            entry,
            json!({
                "seq": 7,
                "message_id": MESSAGE_NAME,
                "sent_ms": SENT_MS,
                "kind": "topic",
                "topic": TOPIC,
                "sender": {"name": LABEL, "kind": PEER_SCRIPT_SENDER, "cwd": CWD},
                "text": TEXT,
                "recipients": [{"name": TITLE, "handle": HANDLE, "status": HELD, "reason": REASON}],
            })
        );
        let encoded = entry.to_string();
        assert!(
            !encoded.contains(SESSION) && !encoded.contains(ROUTE),
            "{encoded}"
        );
    }
}
