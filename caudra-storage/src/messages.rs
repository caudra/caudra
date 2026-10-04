//! The local history of peer messages: every message a session sends, what
//! became of it for each recipient, and how far each session has caught up
//! on every topic. One database serves every project, so retention is global.

use crate::StateDir;
use rusqlite::types::{ToSql, Type};
use rusqlite::{Connection, OpenFlags, Row, TransactionBehavior, params};
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MESSAGES_DB_FILE: &str = "messages.db";
const SCHEMA_VERSION: i64 = 1;
const APPLICATION_ID: i64 = 0x4341_4d4c;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(unix)]
const OWNER_FILE_MODE: u32 = 0o600;
const MS_PER_DAY: u64 = 86_400_000;
const DIRECT: &str = "direct";
const TOPIC: &str = "topic";
const BROADCAST: &str = "broadcast";
const PENDING: &str = "pending";
/// Every message column of `messages m`, in the order [`stored_message`] reads them.
const MESSAGE_COLUMNS: &str = "m.seq, m.sender_route, m.message_id, m.kind, m.topic, \
    m.sender_session, m.sender_name, m.sender_handle, m.sender_cwd, m.sender_mode, \
    m.sender_permission, m.external, m.text, m.reply_to, m.created_ms";
/// Topics arrive as one JSON array rather than a parameter each, so a pattern
/// may match more topics than SQLite allows parameters.
const TOPICS_FILTER: &str = "m.kind = 'topic' AND m.topic IN (SELECT value FROM json_each(?3))";
const BROADCAST_FILTER: &str = "m.kind = 'broadcast'";
const DIRECT_FILTER: &str = "m.kind = 'direct' AND EXISTS (
    SELECT 1 FROM deliveries d WHERE d.seq = m.seq AND (
        (m.sender_session = ?3 AND d.recipient_session = ?4)
        OR (m.sender_session = ?4 AND d.recipient_session = ?3)))";
const NAMED_FILTER: &str = "m.kind = 'direct' AND (m.sender_handle = ?3 OR EXISTS (
    SELECT 1 FROM deliveries d WHERE d.seq = m.seq AND d.recipient_handle = ?3))";
const ALL_FILTER: &str = "1";
const SCHEMA: &str = "
CREATE TABLE messages (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    sender_route TEXT NOT NULL,
    message_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('direct', 'topic', 'broadcast')),
    topic TEXT,
    sender_session TEXT NOT NULL,
    sender_name TEXT NOT NULL,
    sender_handle TEXT,
    sender_cwd TEXT,
    sender_mode TEXT NOT NULL,
    sender_permission TEXT NOT NULL,
    external INTEGER NOT NULL,
    text TEXT NOT NULL,
    reply_to TEXT,
    created_ms INTEGER NOT NULL,
    UNIQUE (sender_route, message_id),
    CHECK ((kind = 'topic') = (topic IS NOT NULL))
);
CREATE INDEX messages_topic ON messages(topic, seq) WHERE topic IS NOT NULL;
CREATE INDEX messages_kind ON messages(kind, seq);
CREATE INDEX messages_created ON messages(created_ms);
CREATE TABLE deliveries (
    seq INTEGER NOT NULL REFERENCES messages(seq) ON DELETE CASCADE,
    recipient_session TEXT NOT NULL,
    recipient_name TEXT,
    recipient_handle TEXT,
    status TEXT NOT NULL,
    reason TEXT,
    updated_ms INTEGER NOT NULL,
    PRIMARY KEY (seq, recipient_session)
);
CREATE TABLE cursors (
    session TEXT NOT NULL,
    topic TEXT NOT NULL,
    seq INTEGER NOT NULL,
    PRIMARY KEY (session, topic)
);
";

#[derive(Debug, thiserror::Error)]
pub enum MessageLogError {
    #[error("message history I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("message history database operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("unsupported message history schema")]
    UnsupportedSchema,
    #[error("unsafe message history file; expected an owner-only regular file")]
    UnsafeFile,
    #[error("invalid message history field: {0}")]
    Invalid(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageAudience {
    Direct,
    Topic(String),
    Broadcast,
}

impl MessageAudience {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Direct => DIRECT,
            Self::Topic(_) => TOPIC,
            Self::Broadcast => BROADCAST,
        }
    }

    pub fn topic(&self) -> Option<&str> {
        match self {
            Self::Topic(topic) => Some(topic),
            Self::Direct | Self::Broadcast => None,
        }
    }
}

/// The sender as its recipients judged it when the message arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSender {
    /// Unique to one live registration, so it keys a message with its id.
    pub route: String,
    /// The sending session. A script sends under the session its label
    /// names, so each label keeps one conversation.
    pub session: String,
    pub name: String,
    pub handle: Option<String>,
    pub cwd: Option<String>,
    pub mode: String,
    pub permission: String,
    /// Sent by a script outside every session, which nothing can reply to.
    pub external: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMessage {
    pub message_id: String,
    pub audience: MessageAudience,
    pub sender: MessageSender,
    pub text: String,
    pub reply_to: Option<String>,
    pub created_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMessage {
    pub seq: i64,
    pub message: NewMessage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRecipient {
    pub session: String,
    pub name: Option<String>,
    pub handle: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicSummary {
    pub topic: String,
    pub count: u64,
    pub last_seq: i64,
    pub last_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retention {
    pub days: u64,
    pub max_messages: u64,
}

/// Which messages a history read returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryChannel {
    /// Messages on any of these exact topics.
    Topics(Vec<String>),
    Broadcast,
    /// Direct messages between `session` and `peer`, in either direction.
    Direct {
        session: String,
        peer: String,
    },
    /// Direct messages to or from whichever session held this messaging
    /// name at the time.
    Named(String),
    /// Every message.
    All,
}

/// One channel of the history as a session browses it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MessageChannel {
    Topic(String),
    Broadcast,
    /// The session's direct conversation with the other party's session.
    Direct(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSummary {
    pub channel: MessageChannel,
    /// The other party's newest known title, in a direct conversation.
    pub name: Option<String>,
    /// The other party's newest known messaging name.
    pub handle: Option<String>,
    pub count: u64,
    pub last_seq: i64,
    pub last_ms: u64,
}

/// What became of a message for one recipient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryRecord {
    pub seq: i64,
    pub recipient_session: String,
    pub recipient_name: Option<String>,
    pub recipient_handle: Option<String>,
    pub status: String,
    pub reason: Option<String>,
    pub updated_ms: u64,
}

/// Changes whenever the history does, whichever connection wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryVersion {
    data: i64,
    local: u64,
}

pub struct MessageLog {
    connection: Connection,
}

impl MessageLog {
    pub fn file_path(state_dir: &StateDir) -> PathBuf {
        state_dir.path().join(MESSAGES_DB_FILE)
    }

    /// Creates the history on first use and prunes it to `retention`.
    pub fn open(
        state_dir: &StateDir,
        retention: &Retention,
        now_ms: u64,
    ) -> Result<Self, MessageLogError> {
        fs::create_dir_all(state_dir.path())?;
        let path = Self::file_path(state_dir);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(OWNER_FILE_MODE);
        match options.open(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let log = Self::connect(&path)?;
        log.prune(retention, now_ms)?;
        Ok(log)
    }

    fn connect(path: &Path) -> Result<Self, MessageLogError> {
        verify_files(path)?;
        let mut connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.pragma_update(None, "trusted_schema", false)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 =
            transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let application: i64 =
            transaction.pragma_query_value(None, "application_id", |row| row.get(0))?;
        if version == 0 && application == 0 {
            if schema_objects(&transaction)? != 0 {
                return Err(MessageLogError::UnsupportedSchema);
            }
            transaction.execute_batch(SCHEMA)?;
            transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else if version != SCHEMA_VERSION || application != APPLICATION_ID {
            return Err(MessageLogError::UnsupportedSchema);
        }
        transaction.commit()?;
        // Every live session writes here, so readers must not wait on a writer.
        connection.pragma_update(None, "journal_mode", "WAL")?;
        Ok(Self { connection })
    }

    /// Records `message` with a pending row for each recipient. Recording a
    /// message again returns its first sequence number and adds only the
    /// recipients it lacks.
    pub fn record(
        &mut self,
        message: &NewMessage,
        recipients: &[MessageRecipient],
    ) -> Result<i64, MessageLogError> {
        let created = sql_ms(message.created_ms)?;
        let sender = &message.sender;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO messages (sender_route, message_id, kind, topic, sender_session,
                sender_name, sender_handle, sender_cwd, sender_mode, sender_permission, external,
                text, reply_to, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT (sender_route, message_id) DO NOTHING",
            params![
                sender.route,
                message.message_id,
                message.audience.kind(),
                message.audience.topic(),
                sender.session,
                sender.name,
                sender.handle,
                sender.cwd,
                sender.mode,
                sender.permission,
                sender.external,
                message.text,
                message.reply_to,
                created,
            ],
        )?;
        let seq: i64 = transaction.query_row(
            "SELECT seq FROM messages WHERE sender_route = ?1 AND message_id = ?2",
            params![sender.route, message.message_id],
            |row| row.get(0),
        )?;
        for recipient in recipients {
            transaction.execute(
                "INSERT INTO deliveries (seq, recipient_session, recipient_name, recipient_handle,
                    status, updated_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (seq, recipient_session) DO NOTHING",
                params![
                    seq,
                    recipient.session,
                    recipient.name,
                    recipient.handle,
                    PENDING,
                    created
                ],
            )?;
        }
        transaction.commit()?;
        Ok(seq)
    }

    /// Applies the sender's receipt for one recipient, unless that recipient
    /// has already reported what became of the message.
    pub fn record_receipt(
        &self,
        sender_route: &str,
        message_id: &str,
        recipient_session: &str,
        status: &str,
        reason: Option<&str>,
        now_ms: u64,
    ) -> Result<(), MessageLogError> {
        self.connection.execute(
            "UPDATE deliveries SET status = ?4, reason = ?5, updated_ms = ?6
             WHERE seq = (SELECT seq FROM messages WHERE sender_route = ?1 AND message_id = ?2)
               AND recipient_session = ?3 AND status IN ('pending', 'unknown')",
            params![
                sender_route,
                message_id,
                recipient_session,
                status,
                reason,
                sql_ms(now_ms)?
            ],
        )?;
        Ok(())
    }

    /// Records a recipient's own report of what became of a message. A
    /// message missing from the history records nothing.
    pub fn transition(
        &self,
        sender_route: &str,
        message_id: &str,
        recipient: &MessageRecipient,
        status: &str,
        reason: Option<&str>,
        now_ms: u64,
    ) -> Result<(), MessageLogError> {
        self.connection.execute(
            "INSERT INTO deliveries (seq, recipient_session, recipient_name, recipient_handle,
                status, reason, updated_ms)
             SELECT seq, ?3, ?4, ?5, ?6, ?7, ?8 FROM messages
             WHERE sender_route = ?1 AND message_id = ?2
             ON CONFLICT (seq, recipient_session) DO UPDATE SET
                 recipient_name = COALESCE(excluded.recipient_name, recipient_name),
                 recipient_handle = COALESCE(excluded.recipient_handle, recipient_handle),
                 status = excluded.status,
                 reason = excluded.reason,
                 updated_ms = excluded.updated_ms",
            params![
                sender_route,
                message_id,
                recipient.session,
                recipient.name,
                recipient.handle,
                status,
                reason,
                sql_ms(now_ms)?
            ],
        )?;
        Ok(())
    }

    /// Moves `session`'s cursor on the message's topic forward to it.
    pub fn mark_seen(
        &self,
        session: &str,
        sender_route: &str,
        message_id: &str,
    ) -> Result<(), MessageLogError> {
        self.connection.execute(
            "INSERT INTO cursors (session, topic, seq)
             SELECT ?1, topic, seq FROM messages
             WHERE sender_route = ?2 AND message_id = ?3 AND kind = 'topic'
             ON CONFLICT (session, topic) DO UPDATE SET seq = MAX(seq, excluded.seq)",
            params![session, sender_route, message_id],
        )?;
        Ok(())
    }

    /// The newest message on each topic `matches` accepts that `session`
    /// did not send and has not seen, newest first.
    pub fn unseen(
        &self,
        session: &str,
        matches: impl Fn(&str) -> bool,
        limit: usize,
    ) -> Result<Vec<StoredMessage>, MessageLogError> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM (
                 SELECT topic, MAX(seq) AS seq FROM messages
                 WHERE kind = 'topic' AND sender_session != ?1
                 GROUP BY topic
             ) AS newest
             JOIN messages m ON m.seq = newest.seq
             LEFT JOIN cursors c ON c.session = ?1 AND c.topic = newest.topic
             WHERE newest.seq > COALESCE(c.seq, 0)
             ORDER BY newest.seq DESC"
        ))?;
        let mut unseen = Vec::new();
        for message in statement.query_map([session], stored_message)? {
            let message = message?;
            if unseen.len() == limit {
                break;
            }
            if message.message.audience.topic().is_some_and(&matches) {
                unseen.push(message);
            }
        }
        Ok(unseen)
    }

    /// Every topic with a message, most recently active first.
    pub fn directory(&self) -> Result<Vec<TopicSummary>, MessageLogError> {
        let mut statement = self.connection.prepare(
            "SELECT topic, COUNT(*), MAX(seq), MAX(created_ms) FROM messages
             WHERE kind = 'topic' GROUP BY topic ORDER BY MAX(seq) DESC",
        )?;
        let topics = statement.query_map([], |row| {
            Ok(TopicSummary {
                topic: row.get(0)?,
                count: row_u64(row, 1)?,
                last_seq: row.get(2)?,
                last_ms: row_u64(row, 3)?,
            })
        })?;
        Ok(topics.collect::<Result<_, _>>()?)
    }

    /// Up to `limit` messages on `channel` before sequence `before`, newest first.
    pub fn history(
        &self,
        channel: &HistoryChannel,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<StoredMessage>, MessageLogError> {
        let before = before.unwrap_or(i64::MAX);
        let limit = i64::try_from(limit).map_err(|_| MessageLogError::Invalid("limit"))?;
        let (filter, values) = match channel {
            HistoryChannel::Topics(topics) if topics.is_empty() => return Ok(Vec::new()),
            HistoryChannel::Topics(topics) => (TOPICS_FILTER, vec![json_list(topics, "topics")?]),
            HistoryChannel::Broadcast => (BROADCAST_FILTER, Vec::new()),
            HistoryChannel::Direct { session, peer } => {
                (DIRECT_FILTER, vec![session.clone(), peer.clone()])
            }
            HistoryChannel::Named(handle) => (NAMED_FILTER, vec![handle.clone()]),
            HistoryChannel::All => (ALL_FILTER, Vec::new()),
        };
        let mut statement = self.connection.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages m
             WHERE m.seq < ?1 AND {filter} ORDER BY m.seq DESC LIMIT ?2"
        ))?;
        let mut bound: Vec<&dyn ToSql> = vec![&before, &limit];
        bound.extend(values.iter().map(|value| value as &dyn ToSql));
        let messages = statement.query_map(bound.as_slice(), stored_message)?;
        Ok(messages.collect::<Result<_, _>>()?)
    }

    /// Every stored topic, the broadcasts, and `session`'s direct
    /// conversations, most recently active first.
    pub fn channels(&self, session: &str) -> Result<Vec<ChannelSummary>, MessageLogError> {
        let mut channels: Vec<ChannelSummary> = self
            .directory()?
            .into_iter()
            .map(|topic| ChannelSummary {
                channel: MessageChannel::Topic(topic.topic),
                name: None,
                handle: None,
                count: topic.count,
                last_seq: topic.last_seq,
                last_ms: topic.last_ms,
            })
            .collect();
        channels.extend(self.connection.query_row(
            "SELECT COUNT(*), MAX(seq), MAX(created_ms) FROM messages WHERE kind = 'broadcast'",
            [],
            |row| {
                let Some(last_seq) = row.get(1)? else {
                    return Ok(None);
                };
                Ok(Some(ChannelSummary {
                    channel: MessageChannel::Broadcast,
                    name: None,
                    handle: None,
                    count: row_u64(row, 0)?,
                    last_seq,
                    last_ms: row_u64(row, 2)?,
                }))
            },
        )?);
        channels.extend(self.conversations(session)?);
        channels.sort_by_key(|channel| Reverse(channel.last_seq));
        Ok(channels)
    }

    fn conversations(&self, session: &str) -> Result<Vec<ChannelSummary>, MessageLogError> {
        let mut statement = self.connection.prepare(
            "SELECT CASE WHEN m.sender_session = ?1 THEN d.recipient_session
                         ELSE m.sender_session END,
                    m.sender_session = ?1, m.seq, m.created_ms, m.sender_name,
                    m.sender_handle, d.recipient_name, d.recipient_handle
             FROM messages m JOIN deliveries d ON d.seq = m.seq
             WHERE m.kind = 'direct' AND (m.sender_session = ?1 OR d.recipient_session = ?1)
             ORDER BY m.seq DESC",
        )?;
        let mut rows = statement.query([session])?;
        let mut conversations: Vec<ChannelSummary> = Vec::new();
        let mut positions: HashMap<String, usize> = HashMap::new();
        while let Some(row) = rows.next()? {
            let peer: String = row.get(0)?;
            let (name, handle) = if row.get(1)? {
                (row.get(6)?, row.get(7)?)
            } else {
                (Some(row.get(4)?), row.get(5)?)
            };
            match positions.get(&peer) {
                Some(&position) => {
                    let conversation = &mut conversations[position];
                    conversation.count += 1;
                    conversation.name = conversation.name.take().or(name);
                    conversation.handle = conversation.handle.take().or(handle);
                }
                None => {
                    positions.insert(peer.clone(), conversations.len());
                    conversations.push(ChannelSummary {
                        channel: MessageChannel::Direct(peer),
                        name,
                        handle,
                        count: 1,
                        last_seq: row.get(2)?,
                        last_ms: row_u64(row, 3)?,
                    });
                }
            }
        }
        Ok(conversations)
    }

    /// Every recipient outcome of the messages `seqs`, by message and then
    /// recipient.
    pub fn deliveries(&self, seqs: &[i64]) -> Result<Vec<DeliveryRecord>, MessageLogError> {
        if seqs.is_empty() {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare(
            "SELECT seq, recipient_session, recipient_name, recipient_handle, status, reason,
                updated_ms
             FROM deliveries WHERE seq IN (SELECT value FROM json_each(?1))
             ORDER BY seq DESC, recipient_name, recipient_session",
        )?;
        let deliveries = statement.query_map([json_list(seqs, "seqs")?], |row| {
            Ok(DeliveryRecord {
                seq: row.get(0)?,
                recipient_session: row.get(1)?,
                recipient_name: row.get(2)?,
                recipient_handle: row.get(3)?,
                status: row.get(4)?,
                reason: row.get(5)?,
                updated_ms: row_u64(row, 6)?,
            })
        })?;
        Ok(deliveries.collect::<Result<_, _>>()?)
    }

    pub fn version(&self) -> Result<HistoryVersion, MessageLogError> {
        Ok(HistoryVersion {
            data: self
                .connection
                .query_row("PRAGMA data_version", [], |row| row.get(0))?,
            local: self.connection.total_changes(),
        })
    }

    /// Deletes messages older than `retention.days`, except the newest on
    /// each topic, then the oldest beyond `retention.max_messages`.
    pub fn prune(&self, retention: &Retention, now_ms: u64) -> Result<usize, MessageLogError> {
        let cutoff = sql_ms(now_ms.saturating_sub(retention.days.saturating_mul(MS_PER_DAY)))?;
        let aged = self.connection.execute(
            "DELETE FROM messages WHERE created_ms < ?1
               AND seq NOT IN (SELECT MAX(seq) FROM messages WHERE kind = 'topic' GROUP BY topic)",
            [cutoff],
        )?;
        let kept = i64::try_from(retention.max_messages).unwrap_or(i64::MAX);
        let excess = self.connection.execute(
            "DELETE FROM messages
             WHERE seq <= (SELECT seq FROM messages ORDER BY seq DESC LIMIT 1 OFFSET ?1)",
            [kept],
        )?;
        Ok(aged + excess)
    }
}

fn stored_message(row: &Row<'_>) -> rusqlite::Result<StoredMessage> {
    let kind: String = row.get(3)?;
    let audience = match (kind.as_str(), row.get::<_, Option<String>>(4)?) {
        (DIRECT, None) => MessageAudience::Direct,
        (TOPIC, Some(topic)) => MessageAudience::Topic(topic),
        (BROADCAST, None) => MessageAudience::Broadcast,
        _ => return Err(rusqlite::Error::InvalidColumnType(3, kind, Type::Text)),
    };
    Ok(StoredMessage {
        seq: row.get(0)?,
        message: NewMessage {
            message_id: row.get(2)?,
            audience,
            sender: MessageSender {
                route: row.get(1)?,
                session: row.get(5)?,
                name: row.get(6)?,
                handle: row.get(7)?,
                cwd: row.get(8)?,
                mode: row.get(9)?,
                permission: row.get(10)?,
                external: row.get(11)?,
            },
            text: row.get(12)?,
            reply_to: row.get(13)?,
            created_ms: row_u64(row, 14)?,
        },
    })
}

fn row_u64(row: &Row<'_>, column: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(column)?;
    u64::try_from(value)
        .map_err(|_| rusqlite::Error::InvalidColumnType(column, value.to_string(), Type::Integer))
}

/// Values bound as one JSON array, however many there are.
fn json_list(values: &[impl Serialize], field: &'static str) -> Result<String, MessageLogError> {
    serde_json::to_string(values).map_err(|_| MessageLogError::Invalid(field))
}

fn sql_ms(value: u64) -> Result<i64, MessageLogError> {
    i64::try_from(value).map_err(|_| MessageLogError::Invalid("timestamp"))
}

fn verify_file(path: &Path) -> Result<(), MessageLogError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(MessageLogError::UnsafeFile);
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o777 != OWNER_FILE_MODE {
        return Err(MessageLogError::UnsafeFile);
    }
    Ok(())
}

/// The database and whichever of its sidecars exist.
fn verify_files(path: &Path) -> Result<(), MessageLogError> {
    verify_file(path)?;
    for suffix in ["-journal", "-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        match verify_file(Path::new(&sidecar)) {
            Err(MessageLogError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
            result => result?,
        }
    }
    Ok(())
}

fn schema_objects(connection: &Connection) -> Result<i64, MessageLogError> {
    Ok(connection.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::{
        HistoryChannel, MESSAGES_DB_FILE, MessageAudience, MessageChannel, MessageLog,
        MessageLogError, MessageRecipient, MessageSender, NewMessage, OWNER_FILE_MODE, Retention,
        TopicSummary,
    };
    use crate::StateDir;
    use rusqlite::{Connection, params};
    use std::fs::{self, Permissions};
    use std::os::unix::fs::PermissionsExt;
    use tempfile::{TempDir, tempdir};
    use test_case::test_case;

    const NOW_MS: u64 = 10_000_000_000;
    const DAY_MS: u64 = 86_400_000;
    const RETENTION_DAYS: u64 = 30;
    const MAX_MESSAGES: u64 = 50_000;
    const ROUTE: &str = "host:session:generation";
    const OTHER_ROUTE: &str = "other-host:other-session:generation";
    const SESSION: &str = "sender-session";
    const OTHER_SESSION: &str = "other-session";
    const READER: &str = "reader-session";
    const RECIPIENT: &str = "recipient-session";
    const RECIPIENT_NAME: &str = "ci-watcher";
    const SENDER_NAME: &str = "CI watcher";
    const RENAMED: &str = "Release watcher";
    const RENAMED_HANDLE: &str = "release-watcher";
    const TOPIC: &str = "ci.failures";
    const OTHER_TOPIC: &str = "deploy.done";
    const TEXT: &str = "The build failed";
    const QUEUED: &str = "queued";
    const DELIVERED: &str = "delivered";
    const UNKNOWN: &str = "unknown";
    const PENDING: &str = "pending";
    const HELD: &str = "held";
    const HOLD_REASON: &str = "Receiver policy requires local approval";
    const LIMIT: usize = 10;
    /// Above SQLite's default limit of 32,766 bound parameters.
    const MANY_TOPICS: usize = 40_000;

    fn retention() -> Retention {
        Retention {
            days: RETENTION_DAYS,
            max_messages: MAX_MESSAGES,
        }
    }

    fn fixture() -> (TempDir, StateDir, MessageLog) {
        let root = tempdir().unwrap();
        let state = StateDir::from_path(root.path().to_path_buf());
        let log = MessageLog::open(&state, &retention(), NOW_MS).unwrap();
        (root, state, log)
    }

    fn message(id: &str, audience: MessageAudience, route: &str, session: &str) -> NewMessage {
        NewMessage {
            message_id: id.into(),
            audience,
            sender: MessageSender {
                route: route.into(),
                session: session.into(),
                name: SENDER_NAME.into(),
                handle: Some(RECIPIENT_NAME.into()),
                cwd: Some("/project".into()),
                mode: "build".into(),
                permission: "ask".into(),
                external: false,
            },
            text: TEXT.into(),
            reply_to: None,
            created_ms: NOW_MS,
        }
    }

    fn topic(name: &str) -> MessageAudience {
        MessageAudience::Topic(name.into())
    }

    fn record_direct(log: &mut MessageLog, id: &str, from: &str, to: &str) -> i64 {
        log.record(
            &message(id, MessageAudience::Direct, ROUTE, from),
            &[MessageRecipient {
                session: to.into(),
                name: None,
                handle: None,
            }],
        )
        .unwrap()
    }

    fn recipient() -> MessageRecipient {
        MessageRecipient {
            session: RECIPIENT.into(),
            name: None,
            handle: None,
        }
    }

    fn delivery(log: &MessageLog, seq: i64) -> (String, Option<String>, Option<String>) {
        log.connection
            .query_row(
                "SELECT status, reason, recipient_name FROM deliveries
                 WHERE seq = ?1 AND recipient_session = ?2",
                params![seq, RECIPIENT],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
    }

    fn ids(messages: &[super::StoredMessage]) -> Vec<&str> {
        messages
            .iter()
            .map(|message| message.message.message_id.as_str())
            .collect()
    }

    #[test]
    fn opening_creates_an_owner_only_history_that_reopens() {
        let (root, state, mut log) = fixture();
        let path = root.path().join(MESSAGES_DB_FILE);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            OWNER_FILE_MODE
        );
        let seq = log
            .record(&message("a", topic(TOPIC), ROUTE, SESSION), &[])
            .unwrap();
        drop(log);
        let reopened = MessageLog::open(&state, &retention(), NOW_MS).unwrap();
        let history = reopened
            .history(&HistoryChannel::Topics(vec![TOPIC.into()]), None, LIMIT)
            .unwrap();
        assert_eq!(history[0].seq, seq);
        assert_eq!(
            history[0].message,
            message("a", topic(TOPIC), ROUTE, SESSION)
        );
    }

    #[test]
    fn unsafe_or_foreign_files_are_refused() {
        let root = tempdir().unwrap();
        let state = StateDir::from_path(root.path().to_path_buf());
        let path = root.path().join(MESSAGES_DB_FILE);
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE other (value TEXT);")
            .unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            MessageLog::open(&state, &retention(), NOW_MS),
            Err(MessageLogError::UnsafeFile)
        ));
        fs::set_permissions(&path, Permissions::from_mode(OWNER_FILE_MODE)).unwrap();
        assert!(matches!(
            MessageLog::open(&state, &retention(), NOW_MS),
            Err(MessageLogError::UnsupportedSchema)
        ));
    }

    #[test]
    fn recording_again_keeps_the_first_message_and_adds_new_recipients() {
        let (_root, _state, mut log) = fixture();
        let first = message("a", topic(TOPIC), ROUTE, SESSION);
        let seq = log.record(&first, &[recipient()]).unwrap();
        let mut changed = first.clone();
        changed.text = "Changed".into();
        let other = MessageRecipient {
            session: OTHER_SESSION.into(),
            name: Some(RECIPIENT_NAME.into()),
            handle: None,
        };
        assert_eq!(log.record(&changed, &[recipient(), other]).unwrap(), seq);
        let history = log
            .history(&HistoryChannel::Topics(vec![TOPIC.into()]), None, LIMIT)
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].message.text, TEXT);
        assert_eq!(delivery(&log, seq).0, PENDING);
        let recipients: i64 = log
            .connection
            .query_row(
                "SELECT COUNT(*) FROM deliveries WHERE seq = ?1",
                [seq],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(recipients, 2);
    }

    #[test_case(true; "receipt_first")]
    #[test_case(false; "report_first")]
    fn receipts_never_overwrite_a_recipient_report(receipt_first: bool) {
        let (_root, _state, mut log) = fixture();
        let seq = log
            .record(
                &message("a", MessageAudience::Direct, ROUTE, SESSION),
                &[recipient()],
            )
            .unwrap();
        let named = MessageRecipient {
            session: RECIPIENT.into(),
            name: Some(RECIPIENT_NAME.into()),
            handle: None,
        };
        let receipt = |log: &MessageLog| {
            log.record_receipt(ROUTE, "a", RECIPIENT, QUEUED, None, NOW_MS)
                .unwrap();
        };
        if receipt_first {
            receipt(&log);
            assert_eq!(delivery(&log, seq).0, QUEUED);
        }
        log.transition(ROUTE, "a", &named, DELIVERED, None, NOW_MS)
            .unwrap();
        receipt(&log);
        assert_eq!(
            delivery(&log, seq),
            (DELIVERED.into(), None, Some(RECIPIENT_NAME.into()))
        );
        log.transition(ROUTE, "a", &recipient(), HELD, Some(HOLD_REASON), NOW_MS)
            .unwrap();
        assert_eq!(
            delivery(&log, seq),
            (
                HELD.into(),
                Some(HOLD_REASON.into()),
                Some(RECIPIENT_NAME.into())
            )
        );
    }

    #[test]
    fn unknown_receipts_resolve_on_a_later_receipt() {
        let (_root, _state, mut log) = fixture();
        let seq = log
            .record(
                &message("a", MessageAudience::Direct, ROUTE, SESSION),
                &[recipient()],
            )
            .unwrap();
        log.record_receipt(ROUTE, "a", RECIPIENT, UNKNOWN, None, NOW_MS)
            .unwrap();
        log.record_receipt(ROUTE, "a", RECIPIENT, QUEUED, None, NOW_MS)
            .unwrap();
        assert_eq!(delivery(&log, seq).0, QUEUED);
    }

    #[test]
    fn reports_on_unrecorded_messages_record_nothing() {
        let (_root, _state, log) = fixture();
        log.transition(ROUTE, "missing", &recipient(), DELIVERED, None, NOW_MS)
            .unwrap();
        log.mark_seen(READER, ROUTE, "missing").unwrap();
        let rows: i64 = log
            .connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM deliveries) + (SELECT COUNT(*) FROM cursors)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn unseen_messages_are_the_newest_per_matching_topic_from_others() {
        let (_root, _state, mut log) = fixture();
        for (id, audience, route, session) in [
            ("old", topic(TOPIC), ROUTE, SESSION),
            ("new", topic(TOPIC), OTHER_ROUTE, OTHER_SESSION),
            ("own", topic(TOPIC), ROUTE, READER),
            ("deploy", topic(OTHER_TOPIC), ROUTE, SESSION),
            ("broadcast", MessageAudience::Broadcast, ROUTE, SESSION),
        ] {
            log.record(&message(id, audience, route, session), &[])
                .unwrap();
        }
        let everything = |_: &str| true;
        assert_eq!(
            ids(&log.unseen(READER, everything, LIMIT).unwrap()),
            ["deploy", "new"]
        );
        assert_eq!(ids(&log.unseen(READER, everything, 1).unwrap()), ["deploy"]);
        assert_eq!(
            ids(&log.unseen(READER, |topic| topic == TOPIC, LIMIT).unwrap()),
            ["new"]
        );
        log.mark_seen(READER, OTHER_ROUTE, "new").unwrap();
        log.mark_seen(READER, ROUTE, "old").unwrap();
        assert_eq!(
            ids(&log.unseen(READER, everything, LIMIT).unwrap()),
            ["deploy"]
        );
        assert_eq!(
            ids(&log.unseen(SESSION, everything, LIMIT).unwrap()),
            ["own"]
        );
    }

    #[test]
    fn history_pages_newest_first_and_counts_topics() {
        let (_root, _state, mut log) = fixture();
        let mut seqs = Vec::new();
        for (id, audience) in [
            ("first", topic(TOPIC)),
            ("deploy", topic(OTHER_TOPIC)),
            ("second", topic(TOPIC)),
            ("broadcast", MessageAudience::Broadcast),
        ] {
            seqs.push(
                log.record(&message(id, audience, ROUTE, SESSION), &[])
                    .unwrap(),
            );
        }
        let ci = HistoryChannel::Topics(vec![TOPIC.into()]);
        assert_eq!(
            ids(&log.history(&ci, None, LIMIT).unwrap()),
            ["second", "first"]
        );
        assert_eq!(
            ids(&log.history(&ci, Some(seqs[2]), LIMIT).unwrap()),
            ["first"]
        );
        assert_eq!(ids(&log.history(&ci, None, 1).unwrap()), ["second"]);
        let both = HistoryChannel::Topics(vec![TOPIC.into(), OTHER_TOPIC.into()]);
        assert_eq!(
            ids(&log.history(&both, None, LIMIT).unwrap()),
            ["second", "deploy", "first"]
        );
        assert_eq!(
            ids(&log
                .history(&HistoryChannel::Broadcast, None, LIMIT)
                .unwrap()),
            ["broadcast"]
        );
        assert!(
            log.history(&HistoryChannel::Topics(Vec::new()), None, LIMIT)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            log.directory().unwrap(),
            [
                TopicSummary {
                    topic: TOPIC.into(),
                    count: 2,
                    last_seq: seqs[2],
                    last_ms: NOW_MS,
                },
                TopicSummary {
                    topic: OTHER_TOPIC.into(),
                    count: 1,
                    last_seq: seqs[1],
                    last_ms: NOW_MS,
                },
            ]
        );
    }

    #[test]
    fn history_reads_more_topics_than_sqlite_allows_parameters() {
        let (_root, _state, mut log) = fixture();
        log.record(&message("first", topic(TOPIC), ROUTE, SESSION), &[])
            .unwrap();
        let topics = (0..MANY_TOPICS)
            .map(|index| format!("unused.topic-{index}"))
            .chain([TOPIC.to_owned()])
            .collect();
        assert_eq!(
            ids(&log
                .history(&HistoryChannel::Topics(topics), None, LIMIT)
                .unwrap()),
            ["first"]
        );
    }

    #[test]
    fn channels_list_topics_broadcasts_and_only_this_sessions_conversations() {
        let (_root, _state, mut log) = fixture();
        log.record(&message("topic", topic(TOPIC), ROUTE, OTHER_SESSION), &[])
            .unwrap();
        log.record(
            &message(
                "broadcast",
                MessageAudience::Broadcast,
                ROUTE,
                OTHER_SESSION,
            ),
            &[],
        )
        .unwrap();
        record_direct(&mut log, "sent", SESSION, RECIPIENT);
        record_direct(&mut log, "received", OTHER_SESSION, SESSION);
        record_direct(&mut log, "unrelated", OTHER_SESSION, RECIPIENT);
        assert_eq!(
            log.channels(SESSION)
                .unwrap()
                .into_iter()
                .map(|summary| (summary.channel, summary.count))
                .collect::<Vec<_>>(),
            [
                (MessageChannel::Direct(OTHER_SESSION.into()), 1),
                (MessageChannel::Direct(RECIPIENT.into()), 1),
                (MessageChannel::Broadcast, 1),
                (MessageChannel::Topic(TOPIC.into()), 1),
            ]
        );
    }

    #[test]
    fn conversations_name_the_other_party_from_its_newest_known_details() {
        let (_root, _state, mut log) = fixture();
        record_direct(&mut log, "received", RECIPIENT, SESSION);
        let sent = record_direct(&mut log, "sent", SESSION, RECIPIENT);
        let conversation = |log: &MessageLog| log.channels(SESSION).unwrap().remove(0);
        let before_report = conversation(&log);
        assert_eq!(before_report.count, 2);
        assert_eq!(before_report.last_seq, sent);
        assert_eq!(before_report.name.as_deref(), Some(SENDER_NAME));
        assert_eq!(before_report.handle.as_deref(), Some(RECIPIENT_NAME));
        log.transition(
            ROUTE,
            "sent",
            &MessageRecipient {
                session: RECIPIENT.into(),
                name: Some(RENAMED.into()),
                handle: Some(RENAMED_HANDLE.into()),
            },
            DELIVERED,
            None,
            NOW_MS,
        )
        .unwrap();
        let reported = conversation(&log);
        assert_eq!(reported.name.as_deref(), Some(RENAMED));
        assert_eq!(reported.handle.as_deref(), Some(RENAMED_HANDLE));
    }

    #[test]
    fn recipient_handles_are_recorded_and_kept_until_a_report_names_another() {
        let (_root, _state, mut log) = fixture();
        let named = MessageRecipient {
            handle: Some(RECIPIENT_NAME.into()),
            ..recipient()
        };
        let seq = log
            .record(&message("a", topic(TOPIC), ROUTE, SESSION), &[named])
            .unwrap();
        let handle = |log: &MessageLog| log.deliveries(&[seq]).unwrap()[0].recipient_handle.clone();
        assert_eq!(handle(&log).as_deref(), Some(RECIPIENT_NAME));
        log.transition(ROUTE, "a", &recipient(), QUEUED, None, NOW_MS)
            .unwrap();
        assert_eq!(handle(&log).as_deref(), Some(RECIPIENT_NAME));
        let renamed = MessageRecipient {
            handle: Some(RENAMED_HANDLE.into()),
            ..recipient()
        };
        log.transition(ROUTE, "a", &renamed, DELIVERED, None, NOW_MS)
            .unwrap();
        assert_eq!(handle(&log).as_deref(), Some(RENAMED_HANDLE));
    }

    #[test_case(HistoryChannel::Named(RECIPIENT_NAME.into()), &["to_name", "by_name"]; "named")]
    #[test_case(HistoryChannel::All, &["to_name", "unrelated", "topic", "by_name"]; "all")]
    fn named_reads_find_a_names_direct_messages_and_all_reads_everything(
        channel: HistoryChannel,
        expected: &[&str],
    ) {
        let (_root, _state, mut log) = fixture();
        let unnamed = |id, audience| {
            let mut stored = message(id, audience, ROUTE, OTHER_SESSION);
            stored.sender.handle = None;
            stored
        };
        log.record(
            &message("by_name", MessageAudience::Direct, ROUTE, SESSION),
            &[recipient()],
        )
        .unwrap();
        log.record(&message("topic", topic(TOPIC), ROUTE, SESSION), &[])
            .unwrap();
        log.record(
            &unnamed("unrelated", MessageAudience::Direct),
            &[recipient()],
        )
        .unwrap();
        let named = MessageRecipient {
            handle: Some(RECIPIENT_NAME.into()),
            ..recipient()
        };
        log.record(&unnamed("to_name", MessageAudience::Direct), &[named])
            .unwrap();
        assert_eq!(ids(&log.history(&channel, None, LIMIT).unwrap()), expected);
    }

    #[test]
    fn direct_history_reads_one_conversation_in_both_directions() {
        let (_root, _state, mut log) = fixture();
        record_direct(&mut log, "sent", SESSION, RECIPIENT);
        record_direct(&mut log, "elsewhere", SESSION, OTHER_SESSION);
        let received = record_direct(&mut log, "received", RECIPIENT, SESSION);
        record_direct(&mut log, "unrelated", RECIPIENT, OTHER_SESSION);
        let conversation = HistoryChannel::Direct {
            session: SESSION.into(),
            peer: RECIPIENT.into(),
        };
        assert_eq!(
            ids(&log.history(&conversation, None, LIMIT).unwrap()),
            ["received", "sent"]
        );
        assert_eq!(
            ids(&log.history(&conversation, Some(received), LIMIT).unwrap()),
            ["sent"]
        );
    }

    #[test]
    fn deliveries_list_each_recipient_of_the_requested_messages() {
        let (_root, _state, mut log) = fixture();
        let recipients = [OTHER_SESSION, RECIPIENT].map(|session| MessageRecipient {
            session: session.into(),
            name: None,
            handle: None,
        });
        let mut record = |id| {
            log.record(&message(id, topic(TOPIC), ROUTE, SESSION), &recipients)
                .unwrap()
        };
        let first = record("first");
        let second = record("second");
        record("unrequested");
        let deliveries = log.deliveries(&[first, second]).unwrap();
        assert_eq!(
            deliveries
                .iter()
                .map(|delivery| (delivery.seq, delivery.recipient_session.as_str()))
                .collect::<Vec<_>>(),
            [
                (second, OTHER_SESSION),
                (second, RECIPIENT),
                (first, OTHER_SESSION),
                (first, RECIPIENT),
            ]
        );
        assert!(deliveries.iter().all(|delivery| delivery.status == PENDING));
        assert!(log.deliveries(&[]).unwrap().is_empty());
    }

    #[test]
    fn the_version_changes_with_each_write_from_any_connection() {
        let (_root, state, mut log) = fixture();
        let mut other = MessageLog::open(&state, &retention(), NOW_MS).unwrap();
        let initial = log.version().unwrap();
        assert_eq!(log.version().unwrap(), initial);
        log.record(&message("own", topic(TOPIC), ROUTE, SESSION), &[])
            .unwrap();
        let own = log.version().unwrap();
        assert_ne!(own, initial);
        other
            .record(
                &message("other", topic(TOPIC), OTHER_ROUTE, OTHER_SESSION),
                &[],
            )
            .unwrap();
        assert_ne!(log.version().unwrap(), own);
    }

    #[test]
    fn pruning_keeps_each_topics_newest_message_until_the_count_bound() {
        let (_root, _state, mut log) = fixture();
        let old = NOW_MS - (RETENTION_DAYS + 1) * DAY_MS;
        for (id, audience, created_ms) in [
            ("old-ci", topic(TOPIC), old),
            ("newest-ci", topic(TOPIC), old),
            ("old-direct", MessageAudience::Direct, old),
            ("recent", MessageAudience::Broadcast, NOW_MS),
        ] {
            let mut stored = message(id, audience, ROUTE, SESSION);
            stored.created_ms = created_ms;
            log.record(&stored, &[recipient()]).unwrap();
        }
        assert_eq!(log.prune(&retention(), NOW_MS).unwrap(), 2);
        let remaining = |log: &MessageLog| {
            let mut statement = log
                .connection
                .prepare("SELECT message_id FROM messages ORDER BY seq")
                .unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(remaining(&log), ["newest-ci", "recent"]);
        let bound = Retention {
            days: RETENTION_DAYS,
            max_messages: 1,
        };
        assert_eq!(log.prune(&bound, NOW_MS).unwrap(), 1);
        assert_eq!(remaining(&log), ["recent"]);
        let deliveries: i64 = log
            .connection
            .query_row("SELECT COUNT(*) FROM deliveries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(deliveries, 1);
    }

    #[test]
    fn two_connections_share_one_history() {
        let (_root, state, mut first) = fixture();
        let mut second = MessageLog::open(&state, &retention(), NOW_MS).unwrap();
        first
            .record(&message("a", topic(TOPIC), ROUTE, SESSION), &[])
            .unwrap();
        second
            .record(&message("b", topic(TOPIC), OTHER_ROUTE, OTHER_SESSION), &[])
            .unwrap();
        let ci = HistoryChannel::Topics(vec![TOPIC.into()]);
        assert_eq!(ids(&first.history(&ci, None, LIMIT).unwrap()), ["b", "a"]);
        assert_eq!(ids(&second.history(&ci, None, LIMIT).unwrap()), ["b", "a"]);
    }
}
