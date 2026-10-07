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
use caudra_storage::messages::{
    DeliveryRecord, GroupChange, GroupPolicy, HistoryChannel, MessageLog, MessageSender,
    StoredMessage, WorkAttempt, WorkCounts, WorkDetail, WorkFilter, WorkGroup, WorkItem, WorkOwner,
    WorkState,
};
use color_eyre::Result;
use color_eyre::eyre::{Context, bail, eyre};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::Serialize;

use super::load_config;
use crate::cli::{Cli, GroupAction, MessageAction, WorkAction, WorkTarget};

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
const QUEUED_WORK: &str = "Queued work";
const NO_GROUPS: &str = "No consumer groups";
const NO_WORK: &str = "No work items";
const NO_WORK_YET: &str = "no work yet";
const PAUSED_GROUP: &str = "Paused group";
const RESUMED_GROUP: &str = "Resumed group";
const DELETED_GROUP: &str = "Deleted group";
const PAUSED: &str = "paused";
/// Work a member holds, whether leased or pausing.
const ACTIVE: &str = "active";
const PATTERN_SEPARATOR: &str = ", ";
const REASON_LABEL: &str = "reason";
const RESULT_LABEL: &str = "result";
const ATTEMPT_LABEL: &str = "attempt";
const RETRY_REPEATS: &str = "warning: earlier attempts may already have had effects; the next member repeats the work from the start";

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
        MessageAction::Group { action } => {
            let now = wall_ms();
            let mut log = open_log(&messaging, now)?;
            manage_group(&mut log, action, now, &mut io::stdout().lock())?;
            return Ok(ExitCode::SUCCESS);
        }
        MessageAction::Work { action } => {
            let now = wall_ms();
            let mut log = open_log(&messaging, now)?;
            let mut out = io::stdout().lock();
            manage_work(&mut log, action, now, &mut out, &mut io::stderr().lock())?;
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
    for queued in &receipt.queued {
        text.push_str(&format!(
            "{QUEUED_WORK} {} for group {}\n",
            queued.work,
            literal(&queued.group, false)
        ));
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

fn wall_ms() -> u64 {
    u64::try_from(Timestamp::now().as_millisecond()).unwrap_or_default()
}

fn open_log(messaging: &MessagingConfig, now_ms: u64) -> Result<MessageLog> {
    let state = StateDir::resolve().context("resolve data directory")?;
    Ok(MessageLog::open(
        &state,
        &history_retention(messaging),
        now_ms,
    )?)
}

fn print_log(messaging: &MessagingConfig, filter: Filter, limit: u32, json: bool) -> Result<()> {
    let log = open_log(messaging, wall_ms())?;
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

fn manage_group(
    log: &mut MessageLog,
    action: GroupAction,
    now_ms: u64,
    out: &mut impl Write,
) -> Result<()> {
    match action {
        GroupAction::Create {
            group,
            topics,
            policy,
            json,
        } => {
            let defaults = GroupPolicy::default();
            let policy = GroupPolicy {
                concurrency: policy.concurrency.unwrap_or(defaults.concurrency),
                max_attempts: policy.attempts.unwrap_or(defaults.max_attempts),
                max_backlog: policy.backlog.unwrap_or(defaults.max_backlog),
            };
            let created = log.create_group(&group.name, &topics, &policy, now_ms)?;
            write_group(out, &created, json)
        }
        GroupAction::List { json } => {
            let groups = log.groups()?;
            if groups.is_empty() && !json {
                writeln!(out, "{NO_GROUPS}")?;
            }
            groups
                .iter()
                .try_for_each(|group| write_group(out, group, json))
        }
        GroupAction::Show { group, json } => write_group(out, &log.group(&group.name)?, json),
        GroupAction::Update {
            group,
            topics,
            policy,
            json,
        } => {
            let change = GroupChange {
                patterns: (!topics.is_empty()).then_some(topics),
                concurrency: policy.concurrency,
                max_attempts: policy.attempts,
                max_backlog: policy.backlog,
                paused: None,
            };
            let changed = log.change_group(&group.name, &change, now_ms)?;
            write_group(out, &changed, json)
        }
        GroupAction::Pause(group) => pause_group(log, &group.name, true, now_ms, out),
        GroupAction::Resume(group) => pause_group(log, &group.name, false, now_ms, out),
        GroupAction::Delete(group) => {
            log.delete_group(&group.name)?;
            Ok(writeln!(
                out,
                "{DELETED_GROUP} {}",
                literal(&group.name, false)
            )?)
        }
    }
}

fn pause_group(
    log: &mut MessageLog,
    name: &str,
    paused: bool,
    now_ms: u64,
    out: &mut impl Write,
) -> Result<()> {
    let change = GroupChange {
        paused: Some(paused),
        ..GroupChange::default()
    };
    log.change_group(name, &change, now_ms)?;
    let done = if paused { PAUSED_GROUP } else { RESUMED_GROUP };
    Ok(writeln!(out, "{done} {}", literal(name, false))?)
}

/// Runs a work command. Retrying an item that was attempted before warns on
/// `warnings`, because the next member starts it over.
fn manage_work(
    log: &mut MessageLog,
    action: WorkAction,
    now_ms: u64,
    out: &mut impl Write,
    warnings: &mut impl Write,
) -> Result<()> {
    match action {
        WorkAction::List {
            group,
            states,
            limit,
            before,
            json,
        } => {
            if let Some(group) = &group {
                log.group(group)?;
            }
            let before = before
                .map(|name| log.work_item(&name).map(|item| item.id))
                .transpose()?;
            let filter = WorkFilter {
                group,
                states: states.into_iter().map(WorkState::from).collect(),
                ..WorkFilter::default()
            };
            let items = log.work(&filter, before, limit as usize)?;
            if items.is_empty() && !json {
                writeln!(out, "{NO_WORK}")?;
            }
            items
                .iter()
                .try_for_each(|item| write_work(out, item, json))
        }
        WorkAction::Show(WorkTarget { work, json }) => {
            let detail = log.work_detail(&work)?;
            if json {
                let entry = WorkDetailEntry::new(&detail);
                writeln!(out, "{}", serde_json::to_string(&entry)?)?;
            } else {
                write!(out, "{}", render_work_detail(&detail))?;
            }
            Ok(())
        }
        WorkAction::Retry(WorkTarget { work, json }) => {
            let attempted = !log.work_detail(&work)?.attempts.is_empty();
            let item = log.retry_work(&work, now_ms)?;
            if attempted {
                writeln!(warnings, "{RETRY_REPEATS}")?;
            }
            write_work(out, &item, json)
        }
        WorkAction::Pause(WorkTarget { work, json }) => {
            write_work(out, &log.hold_work(&work, now_ms)?, json)
        }
        WorkAction::Cancel(WorkTarget { work, json }) => {
            write_work(out, &log.cancel_work(&work, now_ms)?, json)
        }
    }
}

fn write_group(out: &mut impl Write, group: &WorkGroup, json: bool) -> Result<()> {
    if json {
        writeln!(out, "{}", serde_json::to_string(&GroupEntry::new(group))?)?;
    } else {
        write!(out, "{}", render_group(group))?;
    }
    Ok(())
}

fn write_work(out: &mut impl Write, item: &WorkItem, json: bool) -> Result<()> {
    if json {
        writeln!(out, "{}", serde_json::to_string(&WorkEntry::new(item))?)?;
    } else {
        write!(out, "{}", render_work(item))?;
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
        Self {
            seq: stored.seq,
            message_id: &message.message_id,
            sent_ms: message.created_ms,
            kind: message.audience.kind(),
            topic: message.audience.topic(),
            sender: LogSender::new(&message.sender),
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

impl<'a> LogSender<'a> {
    fn new(sender: &'a MessageSender) -> Self {
        Self {
            name: &sender.name,
            kind: if sender.external {
                PEER_SCRIPT_SENDER
            } else {
                PEER_SESSION_SENDER
            },
            handle: sender.handle.as_deref(),
            cwd: sender.cwd.as_deref(),
        }
    }
}

/// One consumer group as a script reads it.
#[derive(Serialize)]
struct GroupEntry<'a> {
    name: &'a str,
    patterns: &'a [String],
    concurrency: u32,
    max_attempts: u32,
    max_backlog: u32,
    paused: bool,
    created_ms: u64,
    counts: CountsEntry,
}

#[derive(Serialize)]
struct CountsEntry {
    pending: u64,
    active: u64,
    paused: u64,
    completed: u64,
    failed: u64,
    cancelled: u64,
}

/// One work item as a script reads it. Session ids, internal ids, and lease
/// tokens stay out.
#[derive(Serialize)]
struct WorkEntry<'a> {
    name: &'a str,
    group: &'a str,
    state: &'static str,
    attempts: u32,
    max_attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    topic: Option<&'a str>,
    message_id: &'a str,
    publisher: LogSender<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<OwnerEntry<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a str>,
    available_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    lease_until_ms: Option<u64>,
    created_ms: u64,
    updated_ms: u64,
}

#[derive(Serialize)]
struct OwnerEntry<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    handle: Option<&'a str>,
}

/// A work item with the text it carries and every attempt at it, across retries.
#[derive(Serialize)]
struct WorkDetailEntry<'a> {
    #[serde(flatten)]
    item: WorkEntry<'a>,
    text: &'a str,
    attempt_history: Vec<AttemptEntry<'a>>,
}

#[derive(Serialize)]
struct AttemptEntry<'a> {
    attempt: u32,
    owner: OwnerEntry<'a>,
    started_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    ended_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
}

impl<'a> GroupEntry<'a> {
    fn new(group: &'a WorkGroup) -> Self {
        let WorkCounts {
            pending,
            active,
            paused,
            completed,
            failed,
            cancelled,
        } = group.counts;
        Self {
            name: &group.name,
            patterns: &group.patterns,
            concurrency: group.policy.concurrency,
            max_attempts: group.policy.max_attempts,
            max_backlog: group.policy.max_backlog,
            paused: group.paused,
            created_ms: group.created_ms,
            counts: CountsEntry {
                pending,
                active,
                paused,
                completed,
                failed,
                cancelled,
            },
        }
    }
}

impl<'a> WorkEntry<'a> {
    fn new(item: &'a WorkItem) -> Self {
        let message = &item.message.message;
        Self {
            name: &item.name,
            group: &item.group,
            state: item.state.as_str(),
            attempts: item.attempts,
            max_attempts: item.max_attempts,
            topic: message.audience.topic(),
            message_id: &message.message_id,
            publisher: LogSender::new(&message.sender),
            owner: item.owner.as_ref().map(OwnerEntry::new),
            reason: item.reason.as_deref(),
            result: item.result.as_deref(),
            available_ms: item.available_ms,
            lease_until_ms: item.lease_until_ms,
            created_ms: item.created_ms,
            updated_ms: item.updated_ms,
        }
    }
}

impl<'a> OwnerEntry<'a> {
    fn new(owner: &'a WorkOwner) -> Self {
        Self {
            name: owner.name.as_deref(),
            handle: owner.handle.as_deref(),
        }
    }
}

impl<'a> WorkDetailEntry<'a> {
    fn new(detail: &'a WorkDetail) -> Self {
        Self {
            item: WorkEntry::new(&detail.item),
            text: &detail.item.message.message.text,
            attempt_history: detail
                .attempts
                .iter()
                .map(|attempt| AttemptEntry {
                    attempt: attempt.attempt,
                    owner: OwnerEntry::new(&attempt.owner),
                    started_ms: attempt.started_ms,
                    ended_ms: attempt.ended_ms,
                    outcome: attempt.outcome.as_deref(),
                    detail: attempt.detail.as_deref(),
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
        let name = session_name(
            recipient.recipient_handle.as_deref(),
            recipient.recipient_name.as_deref(),
        );
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

fn session_name(handle: Option<&str>, name: Option<&str>) -> String {
    match (handle, name) {
        (Some(handle), _) => literal(&handle_address(handle), false),
        (None, Some(name)) => literal(name, false),
        (None, None) => UNKNOWN_SESSION.to_owned(),
    }
}

fn owner_name(owner: &WorkOwner) -> String {
    session_name(owner.handle.as_deref(), owner.name.as_deref())
}

fn render_group(group: &WorkGroup) -> String {
    let patterns: Vec<String> = group
        .patterns
        .iter()
        .map(|pattern| literal(pattern, false))
        .collect();
    let policy = &group.policy;
    let mut text = format!(
        "{}{SEPARATOR}topics {}{SEPARATOR}concurrency {}{SEPARATOR}attempts {}{SEPARATOR}backlog {}",
        literal(&group.name, false),
        patterns.join(PATTERN_SEPARATOR),
        policy.concurrency,
        policy.max_attempts,
        policy.max_backlog
    );
    if group.paused {
        text.push_str(SEPARATOR);
        text.push_str(PAUSED);
    }
    let counts = &group.counts;
    let tallies: Vec<String> = [
        (counts.pending, WorkState::Pending.as_str()),
        (counts.active, ACTIVE),
        (counts.paused, WorkState::Paused.as_str()),
        (counts.completed, WorkState::Completed.as_str()),
        (counts.failed, WorkState::Failed.as_str()),
        (counts.cancelled, WorkState::Cancelled.as_str()),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, state)| format!("{count} {state}"))
    .collect();
    let tally = if tallies.is_empty() {
        NO_WORK_YET.to_owned()
    } else {
        tallies.join(SEPARATOR)
    };
    text.push_str(&format!("\n{INDENT}{tally}\n"));
    text
}

fn render_work(item: &WorkItem) -> String {
    let mut text = format!(
        "{}{SEPARATOR}{}{SEPARATOR}{}{SEPARATOR}{}{SEPARATOR}{}/{} attempts",
        local_time(item.created_ms),
        item.name,
        literal(&item.group, false),
        item.state,
        item.attempts,
        item.max_attempts
    );
    if let Some(topic) = item.message.message.audience.topic() {
        text.push_str(SEPARATOR);
        text.push_str(&literal(topic, false));
    }
    if let Some(owner) = &item.owner {
        text.push_str(SEPARATOR);
        text.push_str(&owner_name(owner));
    }
    text.push('\n');
    for (label, value) in [(REASON_LABEL, &item.reason), (RESULT_LABEL, &item.result)] {
        if let Some(value) = value {
            text.push_str(&format!("{INDENT}{label}: {}\n", literal(value, false)));
        }
    }
    text
}

/// The item, when it last changed, every attempt at it, and the message it carries.
fn render_work_detail(detail: &WorkDetail) -> String {
    let item = &detail.item;
    let mut text = render_work(item);
    text.push_str(&format!("{INDENT}updated {}", local_time(item.updated_ms)));
    if let Some(until) = item.lease_until_ms {
        text.push_str(&format!("{SEPARATOR}leased until {}", local_time(until)));
    }
    text.push('\n');
    for attempt in &detail.attempts {
        text.push_str(&render_attempt(attempt));
    }
    text.push_str(&render_entry(&item.message, &[]));
    text
}

fn render_attempt(attempt: &WorkAttempt) -> String {
    let mut text = format!(
        "{INDENT}{ATTEMPT_LABEL} {}{SEPARATOR}{}{SEPARATOR}{}",
        attempt.attempt,
        owner_name(&attempt.owner),
        local_time(attempt.started_ms)
    );
    if let Some(ended) = attempt.ended_ms {
        text.push_str(&format!(" to {}", local_time(ended)));
    }
    for value in [&attempt.outcome, &attempt.detail].into_iter().flatten() {
        text.push_str(SEPARATOR);
        text.push_str(&literal(value, false));
    }
    text.push('\n');
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
    use caudra_agent::peers::{
        MAX_BODY_BYTES, PublishReceipt, RecipientReceipt, history_retention,
    };
    use caudra_config::MessagingConfig;
    use caudra_providers::{PEER_SCRIPT_SENDER, PeerAudience};
    use caudra_storage::StateDir;
    use caudra_storage::messages::{
        Assignment, DeliveryRecord, GroupPolicy, MessageAudience, MessageLog, MessageSender,
        NewMessage, QueuedWork, StoredMessage, WorkFence, WorkOutcome, WorkRefusal, WorkState,
        Worker,
    };
    use clap::Parser;
    use color_eyre::Result;
    use serde_json::{Value, json};
    use tempfile::{TempDir, tempdir};
    use test_case::test_case;

    use super::{
        ATTEMPT_LABEL, DELETED_GROUP, INDENT, LogEntry, NO_GROUPS, NO_WORK, NO_WORK_YET, NOT_UTF8,
        PATTERN_SEPARATOR, PAUSED, PAUSED_GROUP, QUEUED_WORK, REASON_LABEL, RESUMED_GROUP,
        RETRY_REPEATS, SEPARATOR, SKIPPED, TEXT_TOO_LONG, manage_group, manage_work, read_text,
        render_entry, render_receipt, succeeded,
    };
    use crate::cli::{Cli, Command, MessageAction};

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
    const NOW_MS: u64 = SENT_MS + 60_000;
    const SECOND_MESSAGE_NAME: &str = "quiet-amber-heron";
    const THIRD_MESSAGE_NAME: &str = "bold-linen-finch";
    const GROUP: &str = "reviewers";
    const OTHER_GROUP: &str = "auditors";
    const EVERY_CI_TOPIC: &str = "ci.*";
    const DEPLOY_TOPIC: &str = "deploy";
    const WORK_NAME: &str = "steady-maple-wren";
    const WORKER_SESSION: &str = "0193a5f6-0000-7000-8000-000000000002";
    const WORKER_ROUTE: &str = "p1:host:worker:generation";
    const NOT_A_GROUP_OR_WORK_COMMAND: &str = "expected a message group or work command";

    fn history() -> (TempDir, MessageLog) {
        let root = tempdir().unwrap();
        let state = StateDir::from_path(root.path().to_path_buf());
        let retention = history_retention(&MessagingConfig::default());
        let log = MessageLog::open(&state, &retention, NOW_MS).unwrap();
        (root, log)
    }

    /// Runs `caudra message ARGS` against `log`, returning stdout and stderr.
    fn run(log: &mut MessageLog, args: &[&str]) -> Result<(String, String)> {
        let cli = Cli::try_parse_from(["caudra", "message"].iter().chain(args)).unwrap();
        let (mut out, mut warnings) = (Vec::new(), Vec::new());
        match cli.command {
            Some(Command::Message {
                action: MessageAction::Group { action },
            }) => manage_group(log, action, NOW_MS, &mut out)?,
            Some(Command::Message {
                action: MessageAction::Work { action },
            }) => manage_work(log, action, NOW_MS, &mut out, &mut warnings)?,
            _ => panic!("{NOT_A_GROUP_OR_WORK_COMMAND}"),
        }
        Ok((
            String::from_utf8(out).unwrap(),
            String::from_utf8(warnings).unwrap(),
        ))
    }

    fn create_group(log: &mut MessageLog) {
        run(log, &["group", "create", GROUP, "--topic", TOPIC]).unwrap();
    }

    fn json_lines(text: &str) -> Vec<Value> {
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Publishes `text` on `TOPIC` as `id` and returns the one work item it queued.
    fn publish(log: &mut MessageLog, id: &str, text: &str) -> String {
        let message = NewMessage {
            message_id: id.into(),
            text: text.into(),
            ..stored(LABEL, TEXT, true).message
        };
        let mut recorded = log.record_publication(&message, &[], usize::MAX).unwrap();
        recorded.work.remove(0).work
    }

    /// Claims the oldest item for a worker session that then reports it failed.
    fn fail_once(
        log: &mut MessageLog,
        owner: &str,
        handle: Option<&str>,
        reason: &str,
    ) -> Assignment {
        let worker = Worker {
            session: WORKER_SESSION.into(),
            route: WORKER_ROUTE.into(),
            name: Some(owner.into()),
            handle: handle.map(str::to_owned),
        };
        let assignment = log
            .claim_work(&worker, &[GROUP.into()], NOW_MS, |_| true)
            .unwrap()
            .unwrap();
        let fence = WorkFence::Lease(assignment.token.clone());
        let outcome = WorkOutcome::Failed(reason.into());
        log.finish_work(&assignment.work.name, &fence, &outcome, NOW_MS)
            .unwrap();
        assignment
    }

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
            queued: Vec::new(),
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
                    automation: None,
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

    #[test]
    fn receipts_list_queued_work_apart_from_recipients() {
        let mut receipt = receipt(topic(), &[QUEUED]);
        receipt.queued = [GROUP, HOSTILE]
            .map(|group| QueuedWork {
                group: group.into(),
                work: WORK_NAME.into(),
            })
            .to_vec();
        let text = render_receipt(&receipt);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].contains(&format!("@{HANDLE}: {QUEUED}")), "{text}");
        assert_eq!(lines.len(), 4, "{text}");
        for queued in &lines[2..] {
            assert!(
                queued.starts_with(QUEUED_WORK) && queued.contains(WORK_NAME),
                "{text}"
            );
        }
        assert!(lines[2].ends_with(GROUP), "{text}");
        assert!(!text.contains(['\u{1b}', '\u{7}', '\u{202e}']), "{text:?}");
    }

    #[test_case(&[], &GroupPolicy::default(); "default_policy")]
    #[test_case(
        &["--concurrency", "2", "--attempts", "5", "--backlog", "10"],
        &GroupPolicy { concurrency: 2, max_attempts: 5, max_backlog: 10 };
        "given_policy"
    )]
    fn created_groups_print_their_policy_as_json(flags: &[&str], policy: &GroupPolicy) {
        let (_root, mut log) = history();
        let args: Vec<&str> = ["group", "create", GROUP, "--topic", TOPIC, "--json"]
            .iter()
            .chain(flags)
            .copied()
            .collect();
        let (created, _) = run(&mut log, &args).unwrap();
        assert_eq!(
            json_lines(&created),
            [json!({
                "name": GROUP,
                "patterns": [TOPIC],
                "concurrency": policy.concurrency,
                "max_attempts": policy.max_attempts,
                "max_backlog": policy.max_backlog,
                "paused": false,
                "created_ms": NOW_MS,
                "counts": {"pending": 0, "active": 0, "paused": 0, "completed": 0, "failed": 0, "cancelled": 0},
            })]
        );
    }

    #[test]
    fn group_updates_change_only_the_given_fields() {
        let (_root, mut log) = history();
        create_group(&mut log);
        let (attempts, _) = run(
            &mut log,
            &["group", "update", GROUP, "--attempts", "5", "--json"],
        )
        .unwrap();
        let group = &json_lines(&attempts)[0];
        assert_eq!(group["patterns"], json!([TOPIC]));
        assert_eq!(group["max_attempts"], 5);
        let args = [
            "group",
            "update",
            GROUP,
            "--topic",
            EVERY_CI_TOPIC,
            "--topic",
            DEPLOY_TOPIC,
            "--json",
        ];
        let (topics, _) = run(&mut log, &args).unwrap();
        let group = &json_lines(&topics)[0];
        assert_eq!(group["patterns"], json!([EVERY_CI_TOPIC, DEPLOY_TOPIC]));
        assert_eq!(group["max_attempts"], 5);
    }

    #[test]
    fn pausing_and_resuming_a_group_round_trips() {
        let (_root, mut log) = history();
        create_group(&mut log);
        for (command, done, paused) in [
            ("pause", PAUSED_GROUP, true),
            ("resume", RESUMED_GROUP, false),
        ] {
            let (out, _) = run(&mut log, &["group", command, GROUP]).unwrap();
            assert_eq!(out, format!("{done} {GROUP}\n"));
            let (shown, _) = run(&mut log, &["group", "show", GROUP, "--json"]).unwrap();
            assert_eq!(json_lines(&shown)[0]["paused"], paused);
        }
    }

    #[test]
    fn groups_with_unfinished_work_are_kept() {
        let (_root, mut log) = history();
        create_group(&mut log);
        let work = publish(&mut log, MESSAGE_NAME, TEXT);
        let refused = run(&mut log, &["group", "delete", GROUP]).unwrap_err();
        assert_eq!(
            refused.to_string(),
            WorkRefusal::GroupBusy(GROUP.into()).to_string()
        );
        run(&mut log, &["work", "cancel", &work]).unwrap();
        let (deleted, _) = run(&mut log, &["group", "delete", GROUP]).unwrap();
        assert_eq!(deleted, format!("{DELETED_GROUP} {GROUP}\n"));
        let (listed, _) = run(&mut log, &["group", "list"]).unwrap();
        assert_eq!(listed, format!("{NO_GROUPS}\n"));
    }

    #[test]
    fn group_lines_count_only_the_work_present() {
        let (_root, mut log) = history();
        run(
            &mut log,
            &[
                "group",
                "create",
                GROUP,
                "--topic",
                TOPIC,
                "--topic",
                DEPLOY_TOPIC,
            ],
        )
        .unwrap();
        let (fresh, _) = run(&mut log, &["group", "show", GROUP]).unwrap();
        assert!(
            fresh.contains(&[TOPIC, DEPLOY_TOPIC].join(PATTERN_SEPARATOR)),
            "{fresh}"
        );
        assert_eq!(
            fresh.lines().nth(1),
            Some(format!("{INDENT}{NO_WORK_YET}").as_str())
        );
        let held = publish(&mut log, MESSAGE_NAME, TEXT);
        publish(&mut log, SECOND_MESSAGE_NAME, TEXT);
        run(&mut log, &["work", "pause", &held]).unwrap();
        run(&mut log, &["group", "pause", GROUP]).unwrap();
        let (listed, _) = run(&mut log, &["group", "list"]).unwrap();
        let lines: Vec<&str> = listed.lines().collect();
        assert!(
            lines[0].ends_with(&format!("{SEPARATOR}{PAUSED}")),
            "{listed}"
        );
        assert_eq!(
            lines[1],
            format!(
                "{INDENT}1 {}{SEPARATOR}1 {}",
                WorkState::Pending,
                WorkState::Paused
            )
        );
    }

    #[test]
    fn work_list_filters_and_pages_back_by_name() {
        let (_root, mut log) = history();
        create_group(&mut log);
        run(
            &mut log,
            &["group", "create", OTHER_GROUP, "--topic", DEPLOY_TOPIC],
        )
        .unwrap();
        let queued = [MESSAGE_NAME, SECOND_MESSAGE_NAME, THIRD_MESSAGE_NAME]
            .map(|id| publish(&mut log, id, TEXT));
        let names = |out: &str| -> Vec<String> {
            json_lines(out)
                .iter()
                .map(|item| item["name"].as_str().unwrap().to_owned())
                .collect()
        };
        let (newest, _) = run(&mut log, &["work", "list", "-n", "2", "--json"]).unwrap();
        assert_eq!(names(&newest), [queued[2].clone(), queued[1].clone()]);
        let (older, _) = run(
            &mut log,
            &["work", "list", "--before", &queued[1], "--json"],
        )
        .unwrap();
        assert_eq!(names(&older), [queued[0].clone()]);
        run(&mut log, &["work", "pause", &queued[0]]).unwrap();
        let (paused, _) = run(&mut log, &["work", "list", "--state", "paused", "--json"]).unwrap();
        assert_eq!(names(&paused), [queued[0].clone()]);
        let (other, _) = run(&mut log, &["work", "list", "--group", OTHER_GROUP]).unwrap();
        assert_eq!(other, format!("{NO_WORK}\n"));
    }

    #[test_case(&["work", "show", WORK_NAME], &WorkRefusal::UnknownWork(WORK_NAME.into()); "show")]
    #[test_case(&["work", "list", "--before", WORK_NAME], &WorkRefusal::UnknownWork(WORK_NAME.into()); "page")]
    #[test_case(&["work", "list", "--group", GROUP], &WorkRefusal::UnknownGroup(GROUP.into()); "group_filter")]
    #[test_case(&["group", "show", GROUP], &WorkRefusal::UnknownGroup(GROUP.into()); "group")]
    fn unknown_names_are_refused(args: &[&str], refusal: &WorkRefusal) {
        let (_root, mut log) = history();
        let error = run(&mut log, args).unwrap_err();
        assert_eq!(error.to_string(), refusal.to_string());
    }

    #[test]
    fn work_moves_through_pause_retry_and_cancel() {
        let (_root, mut log) = history();
        create_group(&mut log);
        let work = publish(&mut log, MESSAGE_NAME, TEXT);
        for (command, state) in [
            ("pause", WorkState::Paused),
            ("retry", WorkState::Pending),
            ("cancel", WorkState::Cancelled),
        ] {
            let (out, warnings) = run(&mut log, &["work", command, &work, "--json"]).unwrap();
            assert_eq!(json_lines(&out)[0]["state"], state.as_str());
            assert!(warnings.is_empty(), "{warnings}");
        }
        let refused = run(&mut log, &["work", "pause", &work]).unwrap_err();
        assert!(
            refused.to_string().contains(WorkState::Cancelled.as_str()),
            "{refused}"
        );
    }

    #[test_case(true; "after_an_attempt")]
    #[test_case(false; "never_attempted")]
    fn retrying_warns_only_after_an_earlier_attempt(attempted: bool) {
        let (_root, mut log) = history();
        create_group(&mut log);
        let work = publish(&mut log, MESSAGE_NAME, TEXT);
        if attempted {
            fail_once(&mut log, TITLE, None, REASON);
        } else {
            run(&mut log, &["work", "pause", &work]).unwrap();
        }
        let (out, warnings) = run(&mut log, &["work", "retry", &work, "--json"]).unwrap();
        assert_eq!(json_lines(&out)[0]["state"], WorkState::Pending.as_str());
        let expected = if attempted {
            format!("{RETRY_REPEATS}\n")
        } else {
            String::new()
        };
        assert_eq!(warnings, expected);
    }

    #[test]
    fn json_work_leaves_out_sessions_ids_and_tokens() {
        let (_root, mut log) = history();
        create_group(&mut log);
        let work = publish(&mut log, MESSAGE_NAME, TEXT);
        let assignment = fail_once(&mut log, TITLE, Some(HANDLE), REASON);
        let owner = json!({"name": TITLE, "handle": HANDLE});
        let mut expected = json!({
            "name": work,
            "group": GROUP,
            "state": WorkState::Failed.as_str(),
            "attempts": 1,
            "max_attempts": GroupPolicy::default().max_attempts,
            "topic": TOPIC,
            "message_id": MESSAGE_NAME,
            "publisher": {"name": LABEL, "kind": PEER_SCRIPT_SENDER, "cwd": CWD},
            "owner": owner,
            "reason": REASON,
            "available_ms": SENT_MS,
            "created_ms": SENT_MS,
            "updated_ms": NOW_MS,
        });
        let (listed, _) = run(&mut log, &["work", "list", "--json"]).unwrap();
        assert_eq!(json_lines(&listed), [expected.clone()]);
        expected["text"] = json!(TEXT);
        expected["attempt_history"] = json!([{
            "attempt": 1,
            "owner": owner,
            "started_ms": NOW_MS,
            "ended_ms": NOW_MS,
            "outcome": WorkState::Failed.as_str(),
            "detail": REASON,
        }]);
        let (shown, _) = run(&mut log, &["work", "show", &work, "--json"]).unwrap();
        assert_eq!(json_lines(&shown), [expected]);
        for private in [
            SESSION,
            ROUTE,
            WORKER_SESSION,
            WORKER_ROUTE,
            assignment.token.as_str(),
        ] {
            assert!(!shown.contains(private), "{shown}");
        }
    }

    #[test]
    fn work_lines_escape_untrusted_text() {
        let (_root, mut log) = history();
        create_group(&mut log);
        let work = publish(&mut log, MESSAGE_NAME, HOSTILE_LINES);
        fail_once(&mut log, HOSTILE, None, HOSTILE);
        let (listed, _) = run(&mut log, &["work", "list"]).unwrap();
        let (shown, _) = run(&mut log, &["work", "show", &work]).unwrap();
        for text in [&listed, &shown] {
            assert!(!text.contains(['\u{1b}', '\u{7}', '\u{202e}']), "{text:?}");
            assert!(
                text.contains(&format!("{INDENT}{REASON_LABEL}: ")),
                "{text}"
            );
        }
        assert_eq!(listed.lines().count(), 2, "{listed}");
        assert!(
            shown.contains(&format!("{INDENT}{ATTEMPT_LABEL} 1{SEPARATOR}")),
            "{shown}"
        );
        assert_eq!(
            shown
                .lines()
                .filter(|line| line.starts_with(INDENT))
                .count(),
            5,
            "{shown}"
        );
    }
}
