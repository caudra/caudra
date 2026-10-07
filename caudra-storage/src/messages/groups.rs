//! Consumer groups turn topic publications into tracked work. A group holds
//! topic patterns and a policy every member shares. Recording a matching
//! publication queues one work item per group in the transaction that
//! records the message, so a group sees every publication made after it was
//! created, whether or not a member is online. A member claims an item under
//! a lease it keeps renewing until it reports an outcome. Each claim issues a
//! fresh token, and every later transition requires it, so a worker whose
//! lease lapsed can no longer change an item that has moved on without it.

use super::{
    MESSAGE_COLUMNS, MessageLog, MessageLogError, StoredMessage, json_list, row_u64, sql_ms,
    stored_message,
};
use crate::topics::{MAX_PATTERNS, parse_pattern, pattern_matches};
use crate::words::derived_phrase;
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const DEFAULT_CONCURRENCY: u32 = 1;
pub const MAX_CONCURRENCY: u32 = 16;
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;
pub const MAX_ATTEMPTS: u32 = 10;
pub const MAX_BACKLOG: u32 = 1_000;
/// Unfinished work across every group, which pruning never deletes.
pub const MAX_OUTSTANDING: i64 = 10_000;
pub const MAX_GROUPS: i64 = 32;
pub const LEASE_MS: u64 = 120_000;
const FIRST_RETRY_DELAY_MS: i64 = 5_000;
const LATER_RETRY_DELAY_MS: i64 = 30_000;
const WORK_NAME_DOMAIN: &str = "caudra.work-name.v1";
const MAX_NAME_ATTEMPTS: u64 = 32;
/// Pending items a claim inspects for one the worker may take.
const MAX_CLAIM_SCAN: i64 = 256;
const TOKEN_BYTES: usize = 16;
const LEASE_EXPIRED: &str = "Its worker stopped renewing the lease";
const NO_ATTEMPTS_LEFT: &str = "no attempts are left";
const ORPHANED_PAUSE: &str = "Its worker stopped responding while pausing it";
const CANCELLED_BY_USER: &str = "Cancelled by the user";
/// The reason of an item a person took out of the queue.
pub const PAUSED_BY_USER: &str = "Paused by the user";
const ATTEMPT_COMPLETED: &str = "completed";
const ATTEMPT_RETRIED: &str = "retried";
const ATTEMPT_FAILED: &str = "failed";
const ATTEMPT_PAUSED: &str = "paused";
const ATTEMPT_EXPIRED: &str = "expired";
const FIRST_WORK_COLUMN: usize = 16;
const WORK_COLUMNS: &str = "w.id, w.name, g.name, w.state, w.attempts, g.max_attempts, \
    w.available_ms, w.owner_session, w.owner_name, w.owner_handle, w.lease_until_ms, w.reason, \
    w.result, w.created_ms, w.updated_ms, w.changed";
const WORK_FROM: &str =
    "FROM group_work w JOIN work_groups g ON g.id = w.group_id JOIN messages m ON m.seq = w.seq";
/// What a [`WorkFilter`] selects besides its publisher, over `?2` to `?4`.
const WORK_FILTER: &str = "(?2 IS NULL OR g.name = ?2) AND (?3 IS NULL OR w.owner_session = ?3)
    AND (?4 = '[]' OR w.state IN (SELECT value FROM json_each(?4)))";
/// Narrows a work query to the publications of session `?5`, which
/// `messages_publisher` finds.
const PUBLISHER_FILTER: &str = "AND m.kind = 'topic' AND m.sender_session = ?5";
const GROUP_COLUMNS: &str = "g.name, g.patterns, g.concurrency, g.max_attempts, g.max_backlog, \
    g.paused, g.created_ms, \
    COALESCE(SUM(w.state = 'pending'), 0), \
    COALESCE(SUM(w.state IN ('leased', 'pausing')), 0), \
    COALESCE(SUM(w.state = 'paused'), 0), \
    COALESCE(SUM(w.state = 'completed'), 0), \
    COALESCE(SUM(w.state = 'failed'), 0), \
    COALESCE(SUM(w.state = 'cancelled'), 0)";
/// Messages that unfinished work still needs, which pruning keeps.
pub(super) const UNFINISHED_SEQS: &str =
    "SELECT seq FROM group_work WHERE state IN ('pending', 'leased', 'pausing', 'paused')";
pub(crate) const SCHEMA: &str = "
CREATE TABLE work_groups (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    patterns TEXT NOT NULL,
    concurrency INTEGER NOT NULL CHECK (concurrency >= 1),
    max_attempts INTEGER NOT NULL CHECK (max_attempts >= 1),
    max_backlog INTEGER NOT NULL CHECK (max_backlog >= 1),
    paused INTEGER NOT NULL CHECK (paused IN (0, 1)),
    created_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE group_work (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id INTEGER NOT NULL REFERENCES work_groups(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL REFERENCES messages(seq) ON DELETE CASCADE,
    name TEXT NOT NULL UNIQUE,
    state TEXT NOT NULL CHECK (state IN
        ('pending', 'leased', 'pausing', 'paused', 'completed', 'failed', 'cancelled')),
    attempts INTEGER NOT NULL CHECK (attempts >= 0),
    available_ms INTEGER NOT NULL,
    owner_session TEXT,
    owner_route TEXT,
    owner_name TEXT,
    owner_handle TEXT,
    token TEXT,
    lease_until_ms INTEGER,
    reason TEXT,
    result TEXT,
    created_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL,
    UNIQUE (group_id, seq),
    CHECK ((state IN ('leased', 'pausing')) = (lease_until_ms IS NOT NULL))
) STRICT;
CREATE INDEX group_work_queue ON group_work(group_id, state, id);
CREATE INDEX group_work_owner ON group_work(owner_session, state) WHERE owner_session IS NOT NULL;
CREATE INDEX group_work_message ON group_work(seq);
CREATE TABLE work_attempts (
    work_id INTEGER NOT NULL REFERENCES group_work(id) ON DELETE CASCADE,
    attempt INTEGER NOT NULL CHECK (attempt >= 1),
    owner_session TEXT NOT NULL,
    owner_name TEXT,
    owner_handle TEXT,
    started_ms INTEGER NOT NULL,
    ended_ms INTEGER,
    outcome TEXT,
    detail TEXT,
    PRIMARY KEY (work_id, attempt)
) STRICT;
CREATE TRIGGER work_groups_insert_revision AFTER INSERT ON work_groups BEGIN
    UPDATE message_history_revision SET revision = revision + 1 WHERE id = 1;
END;
CREATE TRIGGER work_groups_update_revision AFTER UPDATE ON work_groups BEGIN
    UPDATE message_history_revision SET revision = revision + 1 WHERE id = 1;
END;
CREATE TRIGGER work_groups_delete_revision AFTER DELETE ON work_groups BEGIN
    UPDATE message_history_revision SET revision = revision + 1 WHERE id = 1;
END;
CREATE TRIGGER group_work_insert_revision AFTER INSERT ON group_work BEGIN
    UPDATE message_history_revision SET revision = revision + 1 WHERE id = 1;
END;
CREATE TRIGGER group_work_update_revision
AFTER UPDATE OF state, attempts, available_ms, owner_session, reason, result ON group_work BEGIN
    UPDATE message_history_revision SET revision = revision + 1 WHERE id = 1;
END;
CREATE TRIGGER group_work_delete_revision AFTER DELETE ON group_work BEGIN
    UPDATE message_history_revision SET revision = revision + 1 WHERE id = 1;
END;
";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkRefusal {
    #[error("A consumer group named {0:?} already exists")]
    GroupExists(String),
    #[error("No consumer group is named {0:?}")]
    UnknownGroup(String),
    #[error("No work item is named {0:?}")]
    UnknownWork(String),
    #[error("At most {MAX_GROUPS} consumer groups may exist")]
    TooManyGroups,
    #[error("A consumer group needs at least one topic pattern")]
    NoPatterns,
    #[error("A consumer group has at most {MAX_PATTERNS} topic patterns")]
    TooManyPatterns,
    #[error("{0}")]
    InvalidPattern(String),
    #[error("Consumer group {0:?} still has unfinished work; finish or cancel it first")]
    GroupBusy(String),
    #[error("Consumer group {0:?} already holds its maximum backlog of unfinished work")]
    BacklogFull(String),
    #[error("Consumer groups already hold {MAX_OUTSTANDING} unfinished work items")]
    OutstandingFull,
    #[error(
        "The topic feeds {groups} consumer groups, but this publication may queue work for at most {room}"
    )]
    GroupFanout { groups: usize, room: usize },
    #[error("Invalid consumer group policy: {0}")]
    InvalidPolicy(&'static str),
    #[error("Work item {work:?} is no longer assigned to this session; it is {state}")]
    Stale { work: String, state: WorkState },
    #[error("Work item {work:?} is {state}, so it cannot be {action}")]
    WrongState {
        work: String,
        state: WorkState,
        action: &'static str,
    },
    #[error("Could not name a new work item")]
    NamesExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WorkState {
    /// Queued for any member, once `available_ms` passes.
    Pending,
    /// Owned by a worker that keeps renewing its lease.
    Leased,
    /// Its owner was told to stop; it pauses instead of returning to the queue.
    Pausing,
    /// Waiting for a person to retry or cancel it. Its last owner may still
    /// report an outcome until then.
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl WorkState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Leased => "leased",
            Self::Pausing => "pausing",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "pending" => Self::Pending,
            "leased" => Self::Leased,
            "pausing" => Self::Pausing,
            "paused" => Self::Paused,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

impl fmt::Display for WorkState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What every member of a group shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupPolicy {
    /// Items leased at once across the whole group.
    pub concurrency: u32,
    /// Claims an item gets before it fails, counted since it was last retried.
    pub max_attempts: u32,
    /// Unfinished items the group holds before publishing to it is refused.
    pub max_backlog: u32,
}

impl Default for GroupPolicy {
    fn default() -> Self {
        Self {
            concurrency: DEFAULT_CONCURRENCY,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            max_backlog: MAX_BACKLOG,
        }
    }
}

impl GroupPolicy {
    fn validate(&self) -> Result<(), WorkRefusal> {
        if !(1..=MAX_CONCURRENCY).contains(&self.concurrency) {
            return Err(WorkRefusal::InvalidPolicy("concurrency must be 1 to 16"));
        }
        if !(1..=MAX_ATTEMPTS).contains(&self.max_attempts) {
            return Err(WorkRefusal::InvalidPolicy("attempts must be 1 to 10"));
        }
        if !(1..=MAX_BACKLOG).contains(&self.max_backlog) {
            return Err(WorkRefusal::InvalidPolicy("backlog must be 1 to 1000"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkCounts {
    pub pending: u64,
    /// Leased or pausing.
    pub active: u64,
    pub paused: u64,
    pub completed: u64,
    pub failed: u64,
    pub cancelled: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkGroup {
    pub name: String,
    pub patterns: Vec<String>,
    pub policy: GroupPolicy,
    /// Still queues publications, but hands no work out.
    pub paused: bool,
    pub created_ms: u64,
    pub counts: WorkCounts,
}

/// Fields of a group to change; `None` keeps the current value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupChange {
    pub patterns: Option<Vec<String>>,
    pub concurrency: Option<u32>,
    pub max_attempts: Option<u32>,
    pub max_backlog: Option<u32>,
    pub paused: Option<bool>,
}

/// The live registration claiming work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worker {
    pub session: String,
    pub route: String,
    pub name: Option<String>,
    pub handle: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkOwner {
    pub session: String,
    pub name: Option<String>,
    pub handle: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItem {
    pub id: i64,
    pub name: String,
    pub group: String,
    pub state: WorkState,
    /// Claims since the item was queued or last retried by a person.
    pub attempts: u32,
    pub max_attempts: u32,
    pub available_ms: u64,
    pub owner: Option<WorkOwner>,
    pub lease_until_ms: Option<u64>,
    pub reason: Option<String>,
    pub result: Option<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
    /// The stamp of the item's insert or latest state change. Every such
    /// change in any group takes a larger one, and none is ever reused.
    pub changed: u64,
    pub message: StoredMessage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkAttempt {
    /// Numbers every claim the item ever had, across retries.
    pub attempt: u32,
    pub owner: WorkOwner,
    pub started_ms: u64,
    pub ended_ms: Option<u64>,
    pub outcome: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkDetail {
    pub item: WorkItem,
    pub attempts: Vec<WorkAttempt>,
}

/// A claimed item and the token every later transition of it requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub work: WorkItem,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedWork {
    pub group: String,
    pub work: String,
}

/// A recorded message and the work it queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub seq: i64,
    pub work: Vec<QueuedWork>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkFilter {
    pub group: Option<String>,
    pub owner: Option<String>,
    /// Every state when empty.
    pub states: Vec<WorkState>,
    /// The session whose publication queued the item.
    pub publisher: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkOutcome {
    Completed(Option<String>),
    /// Back to the queue while attempts remain.
    Retry(String),
    Failed(String),
}

/// What entitles a caller to report an outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkFence {
    /// The token of the claim, valid until the item moves on.
    Lease(String),
    /// The owning session of a paused item, which no longer holds a lease.
    Owner(String),
}

struct Current {
    id: i64,
    state: WorkState,
    token: Option<String>,
    owner: Option<String>,
    attempts: u32,
    max_attempts: u32,
}

impl WorkFence {
    fn admits(&self, current: &Current) -> bool {
        match self {
            Self::Lease(token) => current.token.as_deref() == Some(token),
            Self::Owner(session) => {
                current.state == WorkState::Paused && current.owner.as_deref() == Some(session)
            }
        }
    }

    /// Whether this caller reported the completion a repeated report names.
    fn completed(&self, current: &Current) -> bool {
        current.state == WorkState::Completed
            && match self {
                Self::Lease(token) => current.token.as_deref() == Some(token),
                Self::Owner(session) => current.owner.as_deref() == Some(session),
            }
    }
}

impl MessageLog {
    pub fn create_group(
        &mut self,
        name: &str,
        patterns: &[String],
        policy: &GroupPolicy,
        now_ms: u64,
    ) -> Result<WorkGroup, MessageLogError> {
        policy.validate()?;
        check_patterns(patterns)?;
        let now = sql_ms(now_ms)?;
        let transaction = self
            .database
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count: i64 =
            transaction.query_row("SELECT COUNT(*) FROM work_groups", [], |row| row.get(0))?;
        if count >= MAX_GROUPS {
            return Err(WorkRefusal::TooManyGroups.into());
        }
        if group_id(&transaction, name)?.is_some() {
            return Err(WorkRefusal::GroupExists(name.into()).into());
        }
        transaction.execute(
            "INSERT INTO work_groups (name, patterns, concurrency, max_attempts, max_backlog,
                paused, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?6)",
            params![
                name,
                json_list(patterns, "patterns")?,
                policy.concurrency,
                policy.max_attempts,
                policy.max_backlog,
                now
            ],
        )?;
        transaction.commit()?;
        self.group(name)
    }

    pub fn change_group(
        &mut self,
        name: &str,
        change: &GroupChange,
        now_ms: u64,
    ) -> Result<WorkGroup, MessageLogError> {
        let now = sql_ms(now_ms)?;
        let transaction = self
            .database
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_group(&transaction, name)?;
        let policy = GroupPolicy {
            concurrency: change.concurrency.unwrap_or(current.policy.concurrency),
            max_attempts: change.max_attempts.unwrap_or(current.policy.max_attempts),
            max_backlog: change.max_backlog.unwrap_or(current.policy.max_backlog),
        };
        policy.validate()?;
        let patterns = change.patterns.as_ref().unwrap_or(&current.patterns);
        check_patterns(patterns)?;
        transaction.execute(
            "UPDATE work_groups SET patterns = ?2, concurrency = ?3, max_attempts = ?4,
                max_backlog = ?5, paused = ?6, updated_ms = ?7
             WHERE name = ?1",
            params![
                name,
                json_list(patterns, "patterns")?,
                policy.concurrency,
                policy.max_attempts,
                policy.max_backlog,
                change.paused.unwrap_or(current.paused),
                now
            ],
        )?;
        let changed = read_group(&transaction, name)?;
        transaction.commit()?;
        Ok(changed)
    }

    /// Deletes a group whose work has all finished, with that work. A group
    /// created later under the same name starts empty.
    pub fn delete_group(&mut self, name: &str) -> Result<(), MessageLogError> {
        let transaction = self
            .database
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id =
            group_id(&transaction, name)?.ok_or_else(|| WorkRefusal::UnknownGroup(name.into()))?;
        let unfinished: bool = transaction.query_row(
            "SELECT EXISTS (SELECT 1 FROM group_work WHERE group_id = ?1
                AND state IN ('pending', 'leased', 'pausing', 'paused'))",
            [id],
            |row| row.get(0),
        )?;
        if unfinished {
            return Err(WorkRefusal::GroupBusy(name.into()).into());
        }
        transaction.execute("DELETE FROM work_groups WHERE id = ?1", [id])?;
        transaction.commit()?;
        Ok(())
    }

    /// Every group, by name.
    pub fn groups(&self) -> Result<Vec<WorkGroup>, MessageLogError> {
        let mut statement = self.database.connection().prepare(&format!(
            "SELECT {GROUP_COLUMNS} FROM work_groups g LEFT JOIN group_work w ON w.group_id = g.id
             GROUP BY g.id ORDER BY g.name"
        ))?;
        let groups = statement.query_map([], group_row)?;
        Ok(groups.collect::<Result<_, _>>()?)
    }

    /// The names of the groups a publication on `topic` would queue work for.
    pub fn groups_matching(&self, topic: &str) -> Result<Vec<String>, MessageLogError> {
        Ok(matching(self.database.connection(), topic)?
            .into_iter()
            .map(|(_, name, _)| name)
            .collect())
    }

    pub fn group(&self, name: &str) -> Result<WorkGroup, MessageLogError> {
        read_group(self.database.connection(), name)
    }

    /// Up to `limit` items `filter` selects queued before item `before`,
    /// newest first.
    pub fn work(
        &self,
        filter: &WorkFilter,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<WorkItem>, MessageLogError> {
        self.filtered_work(
            filter,
            "w.id < ?1",
            "w.id DESC",
            before.unwrap_or(i64::MAX),
            limit,
        )
    }

    /// Up to `limit` items `filter` selects whose latest change took a stamp
    /// after `after`, oldest change first.
    pub fn work_changed_after(
        &self,
        filter: &WorkFilter,
        after: u64,
        limit: usize,
    ) -> Result<Vec<WorkItem>, MessageLogError> {
        let after = i64::try_from(after).map_err(|_| MessageLogError::Invalid("stamp"))?;
        self.filtered_work(filter, "w.changed > ?1", "w.changed", after, limit)
    }

    /// The newest stamp any insert or state change of work took.
    pub fn last_work_change(&self) -> Result<u64, MessageLogError> {
        Ok(self.database.connection().query_row(
            "SELECT changed FROM work_change_clock WHERE id = 1",
            [],
            |row| row_u64(row, 0),
        )?)
    }

    fn filtered_work(
        &self,
        filter: &WorkFilter,
        cursor: &str,
        order: &str,
        position: i64,
        limit: usize,
    ) -> Result<Vec<WorkItem>, MessageLogError> {
        let limit = i64::try_from(limit).map_err(|_| MessageLogError::Invalid("limit"))?;
        let states: Vec<&str> = filter.states.iter().map(WorkState::as_str).collect();
        let publisher = if filter.publisher.is_some() {
            PUBLISHER_FILTER
        } else {
            ""
        };
        let mut statement = self.database.connection().prepare(&format!(
            "SELECT {MESSAGE_COLUMNS}, {WORK_COLUMNS} {WORK_FROM}
             WHERE {cursor} AND {WORK_FILTER} {publisher}
             ORDER BY {order} LIMIT ?6"
        ))?;
        let items = statement.query_map(
            params![
                position,
                filter.group,
                filter.owner,
                json_list(&states, "states")?,
                filter.publisher,
                limit
            ],
            work_row,
        )?;
        Ok(items.collect::<Result<_, _>>()?)
    }

    pub fn work_detail(&self, name: &str) -> Result<WorkDetail, MessageLogError> {
        let item = self.work_item(name)?;
        let mut statement = self.database.connection().prepare(
            "SELECT attempt, owner_session, owner_name, owner_handle, started_ms, ended_ms,
                outcome, detail
             FROM work_attempts WHERE work_id = ?1 ORDER BY attempt",
        )?;
        let attempts = statement.query_map([item.id], |row| {
            Ok(WorkAttempt {
                attempt: row.get(0)?,
                owner: WorkOwner {
                    session: row.get(1)?,
                    name: row.get(2)?,
                    handle: row.get(3)?,
                },
                started_ms: row_u64(row, 4)?,
                ended_ms: optional_u64(row, 5)?,
                outcome: row.get(6)?,
                detail: row.get(7)?,
            })
        })?;
        let attempts = attempts.collect::<Result<_, _>>()?;
        Ok(WorkDetail { item, attempts })
    }

    pub fn work_item(&self, name: &str) -> Result<WorkItem, MessageLogError> {
        self.database
            .connection()
            .query_row(
                &format!("SELECT {MESSAGE_COLUMNS}, {WORK_COLUMNS} {WORK_FROM} WHERE w.name = ?1"),
                [name],
                work_row,
            )
            .optional()?
            .ok_or_else(|| WorkRefusal::UnknownWork(name.into()).into())
    }

    /// Hands each pending item of `groups` that `session` did not publish to
    /// `visit`, one at a time, so a caller can tally a long queue.
    pub fn for_each_pending(
        &self,
        groups: &[String],
        session: &str,
        mut visit: impl FnMut(WorkItem),
    ) -> Result<(), MessageLogError> {
        let mut statement = self.database.connection().prepare(&format!(
            "SELECT {MESSAGE_COLUMNS}, {WORK_COLUMNS} {WORK_FROM}
             WHERE w.state = 'pending' AND g.name IN (SELECT value FROM json_each(?1))
               AND m.sender_session != ?2"
        ))?;
        let mut rows = statement.query(params![json_list(groups, "groups")?, session])?;
        while let Some(row) = rows.next()? {
            visit(work_row(row)?);
        }
        Ok(())
    }

    /// Leases the oldest available item of `groups` that `eligible` accepts,
    /// unless `worker`'s session already holds a lease or every group is at
    /// its concurrency. Items whose leases lapsed return to the queue first.
    /// A worker never claims its own session's publications.
    pub fn claim_work(
        &mut self,
        worker: &Worker,
        groups: &[String],
        now_ms: u64,
        eligible: impl Fn(&WorkItem) -> bool,
    ) -> Result<Option<Assignment>, MessageLogError> {
        let now = sql_ms(now_ms)?;
        let transaction = self
            .database
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        recover_expired(&transaction, now)?;
        let busy: bool = transaction.query_row(
            "SELECT EXISTS (SELECT 1 FROM group_work
                WHERE owner_session = ?1 AND state IN ('leased', 'pausing'))",
            [&worker.session],
            |row| row.get(0),
        )?;
        let candidate = if busy || groups.is_empty() {
            None
        } else {
            let mut statement = transaction.prepare(&format!(
                "SELECT {MESSAGE_COLUMNS}, {WORK_COLUMNS} {WORK_FROM}
                 WHERE w.state = 'pending' AND w.available_ms <= ?1 AND g.paused = 0
                   AND g.name IN (SELECT value FROM json_each(?2))
                   AND m.sender_session != ?3
                   AND (SELECT COUNT(*) FROM group_work a WHERE a.group_id = w.group_id
                        AND a.state IN ('leased', 'pausing')) < g.concurrency
                 ORDER BY w.id LIMIT ?4"
            ))?;
            let mut rows = statement.query(params![
                now,
                json_list(groups, "groups")?,
                worker.session,
                MAX_CLAIM_SCAN
            ])?;
            let mut found = None;
            while let Some(row) = rows.next()? {
                let item = work_row(row)?;
                if eligible(&item) {
                    found = Some(item);
                    break;
                }
            }
            found
        };
        let Some(mut work) = candidate else {
            transaction.commit()?;
            return Ok(None);
        };
        let token = lease_token()?;
        let lease_until = now + sql_ms(LEASE_MS)?;
        transaction.execute(
            "UPDATE group_work SET state = 'leased', attempts = attempts + 1, owner_session = ?2,
                owner_route = ?3, owner_name = ?4, owner_handle = ?5, token = ?6,
                lease_until_ms = ?7, reason = NULL, updated_ms = ?8
             WHERE id = ?1",
            params![
                work.id,
                worker.session,
                worker.route,
                worker.name,
                worker.handle,
                token,
                lease_until,
                now
            ],
        )?;
        transaction.execute(
            "INSERT INTO work_attempts (work_id, attempt, owner_session, owner_name, owner_handle,
                started_ms)
             SELECT ?1, COALESCE(MAX(attempt), 0) + 1, ?2, ?3, ?4, ?5
             FROM work_attempts WHERE work_id = ?1",
            params![work.id, worker.session, worker.name, worker.handle, now],
        )?;
        work.changed = transaction.query_row(
            "SELECT changed FROM group_work WHERE id = ?1",
            [work.id],
            |row| row_u64(row, 0),
        )?;
        transaction.commit()?;
        work.state = WorkState::Leased;
        work.attempts += 1;
        work.lease_until_ms = Some(now_ms + LEASE_MS);
        work.reason = None;
        work.owner = Some(WorkOwner {
            session: worker.session.clone(),
            name: worker.name.clone(),
            handle: worker.handle.clone(),
        });
        Ok(Some(Assignment { work, token }))
    }

    /// Extends the lease `token` holds and returns its new end.
    pub fn renew_work(&self, name: &str, token: &str, now_ms: u64) -> Result<u64, MessageLogError> {
        let lease_until = now_ms + LEASE_MS;
        let changed = self.database.connection().execute(
            "UPDATE group_work SET lease_until_ms = ?3
             WHERE name = ?1 AND token = ?2 AND state IN ('leased', 'pausing')",
            params![name, token, sql_ms(lease_until)?],
        )?;
        if changed == 0 {
            return Err(stale(self.database.connection(), name)?);
        }
        Ok(lease_until)
    }

    /// Reports how the work went. Repeating a completion is harmless; any
    /// other report about an item that moved on is refused.
    pub fn finish_work(
        &mut self,
        name: &str,
        fence: &WorkFence,
        outcome: &WorkOutcome,
        now_ms: u64,
    ) -> Result<WorkItem, MessageLogError> {
        let now = sql_ms(now_ms)?;
        let transaction = self
            .database
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = current(&transaction, name)?;
        if matches!(outcome, WorkOutcome::Completed(_)) && fence.completed(&current) {
            drop(transaction);
            return self.work_item(name);
        }
        if !matches!(
            current.state,
            WorkState::Leased | WorkState::Pausing | WorkState::Paused
        ) || !fence.admits(&current)
        {
            return Err(WorkRefusal::Stale {
                work: name.into(),
                state: current.state,
            }
            .into());
        }
        let (attempt, detail) = match outcome {
            WorkOutcome::Completed(summary) => {
                transaction.execute(
                    "UPDATE group_work SET state = 'completed', lease_until_ms = NULL,
                        reason = NULL, result = ?2, updated_ms = ?3
                     WHERE id = ?1",
                    params![current.id, summary, now],
                )?;
                (ATTEMPT_COMPLETED, summary.clone())
            }
            WorkOutcome::Retry(reason) if current.attempts < current.max_attempts => {
                transaction.execute(
                    "UPDATE group_work SET state = 'pending', owner_session = NULL,
                        owner_route = NULL, owner_name = NULL, owner_handle = NULL, token = NULL,
                        lease_until_ms = NULL, reason = ?2, available_ms = ?3, updated_ms = ?4
                     WHERE id = ?1",
                    params![current.id, reason, now + retry_delay(current.attempts), now],
                )?;
                (ATTEMPT_RETRIED, Some(reason.clone()))
            }
            WorkOutcome::Retry(reason) | WorkOutcome::Failed(reason) => {
                let reason = match outcome {
                    WorkOutcome::Retry(_) => format!("{reason}; {NO_ATTEMPTS_LEFT}"),
                    _ => reason.clone(),
                };
                transaction.execute(
                    "UPDATE group_work SET state = 'failed', owner_route = NULL, token = NULL,
                        lease_until_ms = NULL, reason = ?2, updated_ms = ?3
                     WHERE id = ?1",
                    params![current.id, reason, now],
                )?;
                (ATTEMPT_FAILED, Some(reason))
            }
        };
        end_attempt(&transaction, current.id, now, attempt, detail.as_deref())?;
        transaction.commit()?;
        self.work_item(name)
    }

    /// Stops the item from returning to the queue. While its owner is still
    /// stopping, the item keeps its lease as `pausing`; once `stopped`, it
    /// is `paused` and holds no slot of the group's concurrency.
    pub fn pause_work(
        &mut self,
        name: &str,
        token: &str,
        stopped: bool,
        reason: &str,
        now_ms: u64,
    ) -> Result<WorkState, MessageLogError> {
        let now = sql_ms(now_ms)?;
        let transaction = self
            .database
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = current(&transaction, name)?;
        if !matches!(current.state, WorkState::Leased | WorkState::Pausing)
            || current.token.as_deref() != Some(token)
        {
            return Err(WorkRefusal::Stale {
                work: name.into(),
                state: current.state,
            }
            .into());
        }
        let state = if stopped {
            transaction.execute(
                "UPDATE group_work SET state = 'paused', lease_until_ms = NULL, reason = ?2,
                    updated_ms = ?3
                 WHERE id = ?1",
                params![current.id, reason, now],
            )?;
            end_attempt(&transaction, current.id, now, ATTEMPT_PAUSED, Some(reason))?;
            WorkState::Paused
        } else {
            transaction.execute(
                "UPDATE group_work SET state = 'pausing', reason = ?2, updated_ms = ?3
                 WHERE id = ?1",
                params![current.id, reason, now],
            )?;
            WorkState::Pausing
        };
        transaction.commit()?;
        Ok(state)
    }

    /// Returns a claim its worker never started back to the queue, without
    /// spending one of the item's attempts.
    pub fn release_work(
        &mut self,
        name: &str,
        token: &str,
        now_ms: u64,
    ) -> Result<(), MessageLogError> {
        let now = sql_ms(now_ms)?;
        let transaction = self
            .database
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = current(&transaction, name)?;
        if current.state != WorkState::Leased || current.token.as_deref() != Some(token) {
            return Err(WorkRefusal::Stale {
                work: name.into(),
                state: current.state,
            }
            .into());
        }
        transaction.execute(
            "UPDATE group_work SET state = 'pending', attempts = attempts - 1,
                owner_session = NULL, owner_route = NULL, owner_name = NULL, owner_handle = NULL,
                token = NULL, lease_until_ms = NULL, available_ms = ?2, updated_ms = ?2
             WHERE id = ?1",
            params![current.id, now],
        )?;
        transaction.execute(
            "DELETE FROM work_attempts WHERE work_id = ?1 AND ended_ms IS NULL",
            [current.id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Queues a paused, failed, or cancelled item again with a full set of
    /// attempts. Its previous owner can no longer report on it.
    pub fn retry_work(&mut self, name: &str, now_ms: u64) -> Result<WorkItem, MessageLogError> {
        self.reset_work(
            name,
            &[WorkState::Paused, WorkState::Failed, WorkState::Cancelled],
            "retried",
            "UPDATE group_work SET state = 'pending', attempts = 0, available_ms = ?2,
                owner_session = NULL, owner_route = NULL, owner_name = NULL, owner_handle = NULL,
                token = NULL, lease_until_ms = NULL, reason = NULL, result = NULL, updated_ms = ?2
             WHERE id = ?1",
            None,
            now_ms,
        )
    }

    pub fn cancel_work(&mut self, name: &str, now_ms: u64) -> Result<WorkItem, MessageLogError> {
        self.reset_work(
            name,
            &[WorkState::Pending, WorkState::Paused, WorkState::Failed],
            "cancelled",
            "UPDATE group_work SET state = 'cancelled', owner_route = NULL, token = NULL,
                lease_until_ms = NULL, reason = ?3, updated_ms = ?2
             WHERE id = ?1",
            Some(CANCELLED_BY_USER),
            now_ms,
        )
    }

    /// Takes a queued item out of the queue until a person retries it.
    pub fn hold_work(&mut self, name: &str, now_ms: u64) -> Result<WorkItem, MessageLogError> {
        self.reset_work(
            name,
            &[WorkState::Pending],
            "paused",
            "UPDATE group_work SET state = 'paused', reason = ?3, updated_ms = ?2 WHERE id = ?1",
            Some(PAUSED_BY_USER),
            now_ms,
        )
    }

    fn reset_work(
        &mut self,
        name: &str,
        from: &[WorkState],
        action: &'static str,
        update: &str,
        reason: Option<&str>,
        now_ms: u64,
    ) -> Result<WorkItem, MessageLogError> {
        let now = sql_ms(now_ms)?;
        let transaction = self
            .database
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = current(&transaction, name)?;
        if !from.contains(&current.state) {
            return Err(WorkRefusal::WrongState {
                work: name.into(),
                state: current.state,
                action,
            }
            .into());
        }
        match reason {
            Some(reason) => transaction.execute(update, params![current.id, now, reason])?,
            None => transaction.execute(update, params![current.id, now])?,
        };
        transaction.commit()?;
        self.work_item(name)
    }
}

/// Queues one item of `seq` for every group with a pattern matching
/// `topic`, or nothing at all when one of them is full or more than
/// `max_work` match.
pub(super) fn enqueue(
    connection: &Connection,
    seq: i64,
    topic: &str,
    now: i64,
    max_work: usize,
) -> Result<Vec<QueuedWork>, MessageLogError> {
    let matched = matching(connection, topic)?;
    if matched.is_empty() {
        return Ok(Vec::new());
    }
    if matched.len() > max_work {
        return Err(WorkRefusal::GroupFanout {
            groups: matched.len(),
            room: max_work,
        }
        .into());
    }
    let unfinished = |group: Option<i64>| -> Result<i64, MessageLogError> {
        Ok(connection.query_row(
            "SELECT COUNT(*) FROM group_work
             WHERE (?1 IS NULL OR group_id = ?1)
               AND state IN ('pending', 'leased', 'pausing', 'paused')",
            [group],
            |row| row.get(0),
        )?)
    };
    let outstanding = unfinished(None)?;
    if outstanding + i64::try_from(matched.len()).unwrap_or(i64::MAX) > MAX_OUTSTANDING {
        return Err(WorkRefusal::OutstandingFull.into());
    }
    for (id, name, backlog) in &matched {
        if unfinished(Some(*id))? >= *backlog {
            return Err(WorkRefusal::BacklogFull(name.clone()).into());
        }
    }
    let mut queued = Vec::with_capacity(matched.len());
    for (id, group, _) in matched {
        let work = work_name(connection, id, seq)?;
        connection.execute(
            "INSERT INTO group_work (group_id, seq, name, state, attempts, available_ms,
                created_ms, updated_ms)
             VALUES (?1, ?2, ?3, 'pending', 0, ?4, ?4, ?4)",
            params![id, seq, work, now],
        )?;
        queued.push(QueuedWork { group, work });
    }
    Ok(queued)
}

/// The id, name, and backlog of every group with a pattern matching `topic`.
fn matching(
    connection: &Connection,
    topic: &str,
) -> Result<Vec<(i64, String, i64)>, MessageLogError> {
    let mut statement = connection
        .prepare("SELECT id, name, patterns, max_backlog FROM work_groups ORDER BY name")?;
    let groups = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            patterns(row, 2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    let mut matched = Vec::new();
    for group in groups {
        let (id, name, patterns, backlog) = group?;
        if patterns
            .iter()
            .any(|pattern| pattern_matches(pattern, topic))
        {
            matched.push((id, name, backlog));
        }
    }
    Ok(matched)
}

/// The work recording `seq` queued.
pub(super) fn queued(
    connection: &Connection,
    seq: i64,
) -> Result<Vec<QueuedWork>, MessageLogError> {
    let mut statement = connection.prepare(
        "SELECT g.name, w.name FROM group_work w JOIN work_groups g ON g.id = w.group_id
         WHERE w.seq = ?1 ORDER BY g.name",
    )?;
    let queued = statement.query_map([seq], |row| {
        Ok(QueuedWork {
            group: row.get(0)?,
            work: row.get(1)?,
        })
    })?;
    Ok(queued.collect::<Result<_, _>>()?)
}

/// Settles every lease that lapsed: an item its owner was pausing pauses,
/// any other returns to the queue a delay after its lease ended, or fails
/// once it has used its attempts.
fn recover_expired(connection: &Connection, now: i64) -> Result<(), MessageLogError> {
    connection.execute(
        "UPDATE work_attempts SET ended_ms = ?1, outcome = ?2
         WHERE ended_ms IS NULL AND work_id IN (SELECT id FROM group_work
            WHERE state IN ('leased', 'pausing') AND lease_until_ms < ?1)",
        params![now, ATTEMPT_EXPIRED],
    )?;
    connection.execute(
        "UPDATE group_work SET state = 'paused', lease_until_ms = NULL,
            reason = COALESCE(reason, ?2), updated_ms = ?1
         WHERE state = 'pausing' AND lease_until_ms < ?1",
        params![now, ORPHANED_PAUSE],
    )?;
    connection.execute(
        "UPDATE group_work SET state = 'pending', owner_session = NULL, owner_route = NULL,
            owner_name = NULL, owner_handle = NULL, token = NULL, lease_until_ms = NULL,
            reason = ?2, updated_ms = ?1,
            available_ms = lease_until_ms + CASE WHEN attempts <= 1 THEN ?3 ELSE ?4 END
         WHERE state = 'leased' AND lease_until_ms < ?1
           AND attempts < (SELECT max_attempts FROM work_groups g WHERE g.id = group_id)",
        params![
            now,
            LEASE_EXPIRED,
            FIRST_RETRY_DELAY_MS,
            LATER_RETRY_DELAY_MS
        ],
    )?;
    connection.execute(
        "UPDATE group_work SET state = 'failed', owner_route = NULL, token = NULL,
            lease_until_ms = NULL, reason = ?2, updated_ms = ?1
         WHERE state = 'leased' AND lease_until_ms < ?1",
        params![now, format!("{LEASE_EXPIRED}; {NO_ATTEMPTS_LEFT}")],
    )?;
    Ok(())
}

fn check_patterns(patterns: &[String]) -> Result<(), WorkRefusal> {
    if patterns.is_empty() {
        return Err(WorkRefusal::NoPatterns);
    }
    if patterns.len() > MAX_PATTERNS {
        return Err(WorkRefusal::TooManyPatterns);
    }
    patterns
        .iter()
        .try_for_each(|pattern| parse_pattern(pattern).map(drop))
        .map_err(WorkRefusal::InvalidPattern)
}

fn retry_delay(attempts: u32) -> i64 {
    if attempts <= 1 {
        FIRST_RETRY_DELAY_MS
    } else {
        LATER_RETRY_DELAY_MS
    }
}

fn end_attempt(
    connection: &Connection,
    work: i64,
    now: i64,
    outcome: &str,
    detail: Option<&str>,
) -> Result<(), MessageLogError> {
    connection.execute(
        "UPDATE work_attempts SET ended_ms = ?2, outcome = ?3, detail = ?4
         WHERE work_id = ?1
           AND attempt = (SELECT MAX(attempt) FROM work_attempts WHERE work_id = ?1)",
        params![work, now, outcome, detail],
    )?;
    Ok(())
}

fn current(connection: &Connection, name: &str) -> Result<Current, MessageLogError> {
    connection
        .query_row(
            "SELECT w.id, w.state, w.token, w.owner_session, w.attempts, g.max_attempts
             FROM group_work w JOIN work_groups g ON g.id = w.group_id WHERE w.name = ?1",
            [name],
            |row| {
                Ok(Current {
                    id: row.get(0)?,
                    state: state(row, 1)?,
                    token: row.get(2)?,
                    owner: row.get(3)?,
                    attempts: row.get(4)?,
                    max_attempts: row.get(5)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| WorkRefusal::UnknownWork(name.into()).into())
}

fn stale(connection: &Connection, name: &str) -> Result<MessageLogError, MessageLogError> {
    let current = current(connection, name)?;
    Ok(WorkRefusal::Stale {
        work: name.into(),
        state: current.state,
    }
    .into())
}

fn read_group(connection: &Connection, name: &str) -> Result<WorkGroup, MessageLogError> {
    connection
        .query_row(
            &format!(
                "SELECT {GROUP_COLUMNS} FROM work_groups g
                 LEFT JOIN group_work w ON w.group_id = g.id
                 WHERE g.name = ?1 GROUP BY g.id"
            ),
            [name],
            group_row,
        )
        .optional()?
        .ok_or_else(|| WorkRefusal::UnknownGroup(name.into()).into())
}

fn group_id(connection: &Connection, name: &str) -> Result<Option<i64>, MessageLogError> {
    Ok(connection
        .query_row(
            "SELECT id FROM work_groups WHERE name = ?1",
            [name],
            |row| row.get(0),
        )
        .optional()?)
}

/// A readable name derived from the group and message, unique among the
/// stored items.
fn work_name(connection: &Connection, group: i64, seq: i64) -> Result<String, MessageLogError> {
    for attempt in 0..MAX_NAME_ATTEMPTS {
        let mut value = Vec::with_capacity(3 * size_of::<i64>());
        value.extend(group.to_be_bytes());
        value.extend(seq.to_be_bytes());
        value.extend(attempt.to_be_bytes());
        let name = derived_phrase(WORK_NAME_DOMAIN, &value);
        let taken: bool = connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM group_work WHERE name = ?1)",
            [&name],
            |row| row.get(0),
        )?;
        if !taken {
            return Ok(name);
        }
    }
    Err(WorkRefusal::NamesExhausted.into())
}

fn lease_token() -> Result<String, MessageLogError> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(MessageLogError::Random)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn group_row(row: &Row<'_>) -> rusqlite::Result<WorkGroup> {
    Ok(WorkGroup {
        name: row.get(0)?,
        patterns: patterns(row, 1)?,
        policy: GroupPolicy {
            concurrency: row.get(2)?,
            max_attempts: row.get(3)?,
            max_backlog: row.get(4)?,
        },
        paused: row.get(5)?,
        created_ms: row_u64(row, 6)?,
        counts: WorkCounts {
            pending: row_u64(row, 7)?,
            active: row_u64(row, 8)?,
            paused: row_u64(row, 9)?,
            completed: row_u64(row, 10)?,
            failed: row_u64(row, 11)?,
            cancelled: row_u64(row, 12)?,
        },
    })
}

fn work_row(row: &Row<'_>) -> rusqlite::Result<WorkItem> {
    let column = |offset: usize| FIRST_WORK_COLUMN + offset;
    let owner = row
        .get::<_, Option<String>>(column(7))?
        .map(|session| -> rusqlite::Result<WorkOwner> {
            Ok(WorkOwner {
                session,
                name: row.get(column(8))?,
                handle: row.get(column(9))?,
            })
        })
        .transpose()?;
    Ok(WorkItem {
        id: row.get(column(0))?,
        name: row.get(column(1))?,
        group: row.get(column(2))?,
        state: state(row, column(3))?,
        attempts: row.get(column(4))?,
        max_attempts: row.get(column(5))?,
        available_ms: row_u64(row, column(6))?,
        owner,
        lease_until_ms: optional_u64(row, column(10))?,
        reason: row.get(column(11))?,
        result: row.get(column(12))?,
        created_ms: row_u64(row, column(13))?,
        updated_ms: row_u64(row, column(14))?,
        changed: row_u64(row, column(15))?,
        message: stored_message(row)?,
    })
}

fn state(row: &Row<'_>, column: usize) -> rusqlite::Result<WorkState> {
    let value: String = row.get(column)?;
    WorkState::parse(&value)
        .ok_or_else(|| rusqlite::Error::InvalidColumnType(column, value, Type::Text))
}

fn patterns(row: &Row<'_>, column: usize) -> rusqlite::Result<Vec<String>> {
    let value: String = row.get(column)?;
    serde_json::from_str(&value)
        .map_err(|_| rusqlite::Error::InvalidColumnType(column, value, Type::Text))
}

fn optional_u64(row: &Row<'_>, column: usize) -> rusqlite::Result<Option<u64>> {
    row.get::<_, Option<i64>>(column)?
        .map(|value| {
            u64::try_from(value).map_err(|_| {
                rusqlite::Error::InvalidColumnType(column, value.to_string(), Type::Integer)
            })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::{
        Assignment, FIRST_RETRY_DELAY_MS, GroupChange, GroupPolicy, LEASE_EXPIRED, LEASE_MS,
        MAX_BACKLOG, MAX_CONCURRENCY, MAX_GROUPS, NO_ATTEMPTS_LEFT, PAUSED_BY_USER, WorkFence,
        WorkFilter, WorkOutcome, WorkRefusal, WorkState, Worker,
    };
    use crate::StateDir;
    use crate::messages::{
        HistoryChannel, MessageAudience, MessageLog, MessageLogError, MessageSender, NewMessage,
        Retention,
    };
    use crate::topics::{INVALID_PATTERN, MAX_PATTERNS};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use tempfile::{TempDir, tempdir};
    use test_case::test_case;

    const NOW_MS: u64 = 10_000_000_000;
    const RETENTION_DAYS: u64 = 30;
    const MAX_MESSAGES: u64 = 50_000;
    const GROUP: &str = "reviewers";
    const OTHER_GROUP: &str = "auditors";
    const UNMATCHED_GROUP: &str = "deployers";
    const TOPIC: &str = "ci.failures";
    const EVERY_CI_TOPIC: &str = "ci.*";
    const EVERY_TOPIC: &str = "**";
    const DEPLOY_TOPICS: &str = "deploy.*";
    const INVALID_PATTERN_TEXT: &str = "CI..failures";
    const ROUTE: &str = "host:session:generation";
    const PUBLISHER: &str = "publisher-session";
    const OTHER_PUBLISHER: &str = "other-publisher-session";
    const WORKER: &str = "worker-session";
    const OTHER_WORKER: &str = "other-worker-session";
    const TEXT: &str = "Review the change";
    const SUMMARY: &str = "Reviewed and approved";
    const REASON: &str = "The repository was unreachable";
    const PAGE: usize = 10;

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

    fn owned(values: &[&str]) -> Vec<String> {
        values.iter().copied().map(str::to_owned).collect()
    }

    fn message(id: &str, audience: MessageAudience, session: &str) -> NewMessage {
        NewMessage {
            message_id: id.into(),
            audience,
            sender: MessageSender {
                route: ROUTE.into(),
                session: session.into(),
                name: session.into(),
                handle: None,
                cwd: Some("/project".into()),
                mode: "build".into(),
                permission: "ask".into(),
                external: false,
                automation: None,
            },
            text: TEXT.into(),
            reply_to: None,
            created_ms: NOW_MS,
        }
    }

    fn create(log: &mut MessageLog, name: &str, patterns: &[&str], policy: &GroupPolicy) {
        log.create_group(name, &owned(patterns), policy, NOW_MS)
            .unwrap();
    }

    fn group(log: &mut MessageLog, policy: &GroupPolicy) {
        create(log, GROUP, &[TOPIC], policy);
    }

    /// Publishes on `TOPIC` and returns the names of the work it queued.
    fn publish(log: &mut MessageLog, id: &str) -> Vec<String> {
        log.record_publication(
            &message(id, MessageAudience::Topic(TOPIC.into()), PUBLISHER),
            &[],
            usize::MAX,
        )
        .unwrap()
        .work
        .into_iter()
        .map(|queued| queued.work)
        .collect()
    }

    fn worker(session: &str) -> Worker {
        Worker {
            session: session.into(),
            route: format!("route-{session}"),
            name: Some(session.into()),
            handle: None,
        }
    }

    fn claim(log: &mut MessageLog, session: &str, now_ms: u64) -> Option<Assignment> {
        log.claim_work(&worker(session), &[GROUP.into()], now_ms, |_| true)
            .unwrap()
    }

    fn refusal<T: std::fmt::Debug>(result: Result<T, MessageLogError>) -> WorkRefusal {
        match result.unwrap_err() {
            MessageLogError::Refused(refusal) => refusal,
            error => panic!("expected a refusal, got {error}"),
        }
    }

    fn stale(work: &str, state: WorkState) -> WorkRefusal {
        WorkRefusal::Stale {
            work: work.into(),
            state,
        }
    }

    fn lease(assignment: &Assignment) -> WorkFence {
        WorkFence::Lease(assignment.token.clone())
    }

    fn state_of(log: &MessageLog, work: &str) -> WorkState {
        log.work_item(work).unwrap().state
    }

    #[test]
    fn publications_queue_one_item_per_matching_group() {
        let (_root, _state, mut log) = fixture();
        let policy = GroupPolicy::default();
        create(&mut log, GROUP, &[EVERY_CI_TOPIC, TOPIC], &policy);
        create(&mut log, OTHER_GROUP, &[EVERY_TOPIC], &policy);
        create(&mut log, UNMATCHED_GROUP, &[DEPLOY_TOPICS], &policy);
        let entry = message("a", MessageAudience::Topic(TOPIC.into()), PUBLISHER);

        let recorded = log.record_publication(&entry, &[], usize::MAX).unwrap();
        let again = log.record_publication(&entry, &[], 0).unwrap();

        let groups: Vec<&str> = recorded.work.iter().map(|q| q.group.as_str()).collect();
        assert_eq!(groups, [OTHER_GROUP, GROUP]);
        assert_eq!(again, recorded);
        assert_eq!(log.groups_matching(TOPIC).unwrap(), [OTHER_GROUP, GROUP]);
        assert_eq!(log.group(GROUP).unwrap().counts.pending, 1);
        assert_eq!(log.group(UNMATCHED_GROUP).unwrap().counts.pending, 0);
    }

    #[test]
    fn publications_matching_more_groups_than_their_room_are_refused() {
        let (_root, _state, mut log) = fixture();
        let policy = GroupPolicy::default();
        create(&mut log, GROUP, &[TOPIC], &policy);
        create(&mut log, OTHER_GROUP, &[EVERY_TOPIC], &policy);
        let entry = message("a", MessageAudience::Topic(TOPIC.into()), PUBLISHER);

        assert_eq!(
            refusal(log.record_publication(&entry, &[], 1)),
            WorkRefusal::GroupFanout { groups: 2, room: 1 }
        );
        assert!(
            log.history(&HistoryChannel::All, None, PAGE)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn groups_only_see_publications_made_after_they_exist() {
        let (_root, _state, mut log) = fixture();
        assert!(publish(&mut log, "before").is_empty());
        group(&mut log, &GroupPolicy::default());
        assert_eq!(publish(&mut log, "after").len(), 1);
        assert_eq!(log.group(GROUP).unwrap().counts.pending, 1);
    }

    #[test_case(MessageAudience::Direct; "direct")]
    #[test_case(MessageAudience::Broadcast; "broadcast")]
    fn only_topic_messages_queue_work(audience: MessageAudience) {
        let (_root, _state, mut log) = fixture();
        create(&mut log, GROUP, &[EVERY_TOPIC], &GroupPolicy::default());
        let recorded = log
            .record_publication(&message("a", audience, PUBLISHER), &[], usize::MAX)
            .unwrap();
        assert!(recorded.work.is_empty());
    }

    #[test]
    fn a_full_backlog_refuses_the_whole_publication() {
        let (_root, _state, mut log) = fixture();
        let small = GroupPolicy {
            max_backlog: 1,
            ..GroupPolicy::default()
        };
        group(&mut log, &small);
        create(
            &mut log,
            OTHER_GROUP,
            &[EVERY_TOPIC],
            &GroupPolicy::default(),
        );
        publish(&mut log, "a");

        let full = log.record_publication(
            &message("b", MessageAudience::Topic(TOPIC.into()), PUBLISHER),
            &[],
            usize::MAX,
        );

        assert_eq!(refusal(full), WorkRefusal::BacklogFull(GROUP.into()));
        assert_eq!(
            log.history(&HistoryChannel::All, None, PAGE).unwrap().len(),
            1
        );
        assert_eq!(log.group(OTHER_GROUP).unwrap().counts.pending, 1);
    }

    #[test]
    fn claims_respect_group_concurrency_and_one_lease_per_session() {
        let (_root, _state, mut log) = fixture();
        group(&mut log, &GroupPolicy::default());
        let names: Vec<String> = ["a", "b", "c"]
            .into_iter()
            .flat_map(|id| publish(&mut log, id))
            .collect();

        let first = claim(&mut log, WORKER, NOW_MS).unwrap();
        assert_eq!(first.work.name, names[0]);
        assert!(claim(&mut log, OTHER_WORKER, NOW_MS).is_none());

        let wider = GroupChange {
            concurrency: Some(2),
            ..GroupChange::default()
        };
        log.change_group(GROUP, &wider, NOW_MS).unwrap();
        assert!(claim(&mut log, WORKER, NOW_MS).is_none());
        let second = claim(&mut log, OTHER_WORKER, NOW_MS).unwrap();
        assert_eq!(second.work.name, names[1]);
        assert_eq!(second.work.state, WorkState::Leased);
        assert_eq!(second.work.attempts, 1);
    }

    #[test]
    fn concurrent_claims_on_separate_connections_lease_an_item_once() {
        let (_root, state, mut log) = fixture();
        group(&mut log, &GroupPolicy::default());
        publish(&mut log, "a");
        let barrier = Arc::new(Barrier::new(2));

        let claims: Vec<Option<Assignment>> = [WORKER, OTHER_WORKER]
            .map(|session| {
                let state = state.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let mut log = MessageLog::open(&state, &retention(), NOW_MS).unwrap();
                    barrier.wait();
                    log.claim_work(&worker(session), &[GROUP.into()], NOW_MS, |_| true)
                        .unwrap()
                })
            })
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        assert_eq!(claims.iter().flatten().count(), 1);
    }

    #[test]
    fn publishers_never_claim_their_own_publications() {
        let (_root, _state, mut log) = fixture();
        group(&mut log, &GroupPolicy::default());
        publish(&mut log, "a");
        assert!(claim(&mut log, PUBLISHER, NOW_MS).is_none());
        assert!(claim(&mut log, WORKER, NOW_MS).is_some());
    }

    #[test]
    fn pending_scans_leave_out_claimed_items_and_own_publications() {
        let (_root, _state, mut log) = fixture();
        group(&mut log, &GroupPolicy::default());
        publish(&mut log, "a");
        let pending = publish(&mut log, "b");
        claim(&mut log, OTHER_WORKER, NOW_MS).unwrap();
        let scanned = |session: &str| {
            let mut names = Vec::new();
            log.for_each_pending(&[GROUP.into()], session, |item| names.push(item.name))
                .unwrap();
            names
        };

        assert_eq!(scanned(WORKER), pending);
        assert!(scanned(PUBLISHER).is_empty());
    }

    #[test]
    fn ineligible_items_wait_without_blocking_later_ones() {
        let (_root, _state, mut log) = fixture();
        group(&mut log, &GroupPolicy::default());
        let skipped = publish(&mut log, "a").remove(0);
        let taken = publish(&mut log, "b").remove(0);

        let assignment = log
            .claim_work(&worker(WORKER), &[GROUP.into()], NOW_MS, |item| {
                item.name != skipped
            })
            .unwrap()
            .unwrap();

        assert_eq!(assignment.work.name, taken);
        assert_eq!(state_of(&log, &skipped), WorkState::Pending);
    }

    #[test]
    fn lapsed_leases_return_to_the_queue_until_attempts_run_out() {
        let (_root, _state, mut log) = fixture();
        let twice = GroupPolicy {
            max_attempts: 2,
            ..GroupPolicy::default()
        };
        group(&mut log, &twice);
        let name = publish(&mut log, "a").remove(0);
        claim(&mut log, WORKER, NOW_MS).unwrap();

        let lapsed = NOW_MS + LEASE_MS + 1;
        assert!(claim(&mut log, OTHER_WORKER, lapsed).is_none());
        let queued = log.work_item(&name).unwrap();
        assert_eq!(queued.state, WorkState::Pending);
        assert_eq!(queued.reason.as_deref(), Some(LEASE_EXPIRED));
        assert_eq!(queued.owner, None);

        let available = lapsed + FIRST_RETRY_DELAY_MS as u64;
        let second = claim(&mut log, OTHER_WORKER, available).unwrap();
        assert_eq!(second.work.attempts, 2);

        assert!(claim(&mut log, WORKER, available + LEASE_MS + 1).is_none());
        let failed = log.work_detail(&name).unwrap();
        assert_eq!(failed.item.state, WorkState::Failed);
        assert_eq!(
            failed.item.reason,
            Some(format!("{LEASE_EXPIRED}; {NO_ATTEMPTS_LEFT}"))
        );
        let outcomes: Vec<Option<&str>> = failed
            .attempts
            .iter()
            .map(|attempt| attempt.outcome.as_deref())
            .collect();
        assert_eq!(outcomes, [Some("expired"), Some("expired")]);
    }

    #[test]
    fn a_lapsed_owner_cannot_change_reassigned_work() {
        let (_root, _state, mut log) = fixture();
        group(&mut log, &GroupPolicy::default());
        let name = publish(&mut log, "a").remove(0);
        let first = claim(&mut log, WORKER, NOW_MS).unwrap();
        let reassigned = NOW_MS + LEASE_MS + 1 + FIRST_RETRY_DELAY_MS as u64;
        let second = claim(&mut log, OTHER_WORKER, reassigned).unwrap();
        let leased = stale(&name, WorkState::Leased);

        assert_eq!(
            refusal(log.renew_work(&name, &first.token, reassigned)),
            leased
        );
        assert_eq!(
            refusal(log.finish_work(
                &name,
                &lease(&first),
                &WorkOutcome::Completed(None),
                reassigned
            )),
            leased
        );
        assert_eq!(
            refusal(log.pause_work(&name, &first.token, true, REASON, reassigned)),
            leased
        );
        assert_eq!(
            refusal(log.release_work(&name, &first.token, reassigned)),
            leased
        );
        let done = log
            .finish_work(
                &name,
                &lease(&second),
                &WorkOutcome::Completed(None),
                reassigned,
            )
            .unwrap();
        assert_eq!(done.state, WorkState::Completed);
    }

    #[test]
    fn repeating_a_completion_changes_nothing() {
        let (_root, _state, mut log) = fixture();
        group(&mut log, &GroupPolicy::default());
        let name = publish(&mut log, "a").remove(0);
        let assignment = claim(&mut log, WORKER, NOW_MS).unwrap();
        let completed = WorkOutcome::Completed(Some(SUMMARY.into()));

        let first = log
            .finish_work(&name, &lease(&assignment), &completed, NOW_MS)
            .unwrap();
        let repeated = log
            .finish_work(&name, &lease(&assignment), &completed, NOW_MS + 1)
            .unwrap();

        assert_eq!(first.result.as_deref(), Some(SUMMARY));
        assert_eq!(repeated, first);
        assert_eq!(
            refusal(log.finish_work(
                &name,
                &lease(&assignment),
                &WorkOutcome::Failed(REASON.into()),
                NOW_MS
            )),
            stale(&name, WorkState::Completed)
        );
        let detail = log.work_detail(&name).unwrap();
        assert_eq!(detail.attempts[0].outcome.as_deref(), Some("completed"));
    }

    #[test]
    fn retryable_failures_requeue_until_attempts_run_out() {
        let (_root, _state, mut log) = fixture();
        let twice = GroupPolicy {
            max_attempts: 2,
            ..GroupPolicy::default()
        };
        group(&mut log, &twice);
        let name = publish(&mut log, "a").remove(0);
        let retry = WorkOutcome::Retry(REASON.into());

        let first = claim(&mut log, WORKER, NOW_MS).unwrap();
        let queued = log
            .finish_work(&name, &lease(&first), &retry, NOW_MS)
            .unwrap();
        assert_eq!(queued.state, WorkState::Pending);
        assert_eq!(queued.reason.as_deref(), Some(REASON));
        assert_eq!(queued.available_ms, NOW_MS + FIRST_RETRY_DELAY_MS as u64);
        assert!(claim(&mut log, WORKER, NOW_MS).is_none());

        let second = claim(&mut log, WORKER, queued.available_ms).unwrap();
        let failed = log
            .finish_work(&name, &lease(&second), &retry, queued.available_ms)
            .unwrap();
        assert_eq!(failed.state, WorkState::Failed);
        assert_eq!(failed.reason, Some(format!("{REASON}; {NO_ATTEMPTS_LEFT}")));
    }

    #[test]
    fn work_its_owner_was_pausing_never_returns_to_the_queue() {
        let (_root, _state, mut log) = fixture();
        group(&mut log, &GroupPolicy::default());
        let name = publish(&mut log, "a").remove(0);
        let assignment = claim(&mut log, WORKER, NOW_MS).unwrap();

        let state = log
            .pause_work(&name, &assignment.token, false, REASON, NOW_MS)
            .unwrap();
        assert_eq!(state, WorkState::Pausing);
        assert!(claim(&mut log, OTHER_WORKER, NOW_MS).is_none());

        let lapsed = NOW_MS + LEASE_MS + 1;
        assert!(claim(&mut log, OTHER_WORKER, lapsed).is_none());
        let paused = log.work_item(&name).unwrap();
        assert_eq!(paused.state, WorkState::Paused);
        assert_eq!(paused.reason.as_deref(), Some(REASON));
    }

    #[test]
    fn paused_work_frees_its_slot_and_stays_with_its_owner() {
        let (_root, _state, mut log) = fixture();
        group(&mut log, &GroupPolicy::default());
        let paused = publish(&mut log, "a").remove(0);
        let next = publish(&mut log, "b").remove(0);
        let assignment = claim(&mut log, WORKER, NOW_MS).unwrap();

        log.pause_work(&paused, &assignment.token, true, REASON, NOW_MS)
            .unwrap();
        assert_eq!(
            claim(&mut log, OTHER_WORKER, NOW_MS).unwrap().work.name,
            next
        );
        assert_eq!(
            refusal(log.finish_work(
                &paused,
                &WorkFence::Owner(OTHER_WORKER.into()),
                &WorkOutcome::Completed(None),
                NOW_MS
            )),
            stale(&paused, WorkState::Paused)
        );
        let done = log
            .finish_work(
                &paused,
                &WorkFence::Owner(WORKER.into()),
                &WorkOutcome::Completed(Some(SUMMARY.into())),
                NOW_MS,
            )
            .unwrap();
        assert_eq!(done.state, WorkState::Completed);
        assert_eq!(
            log.work_detail(&paused).unwrap().attempts[0]
                .outcome
                .as_deref(),
            Some("completed")
        );
    }

    #[test]
    fn retrying_paused_work_takes_it_from_its_owner() {
        let (_root, _state, mut log) = fixture();
        group(&mut log, &GroupPolicy::default());
        let name = publish(&mut log, "a").remove(0);
        let assignment = claim(&mut log, WORKER, NOW_MS).unwrap();
        log.pause_work(&name, &assignment.token, true, REASON, NOW_MS)
            .unwrap();

        let retried = log.retry_work(&name, NOW_MS).unwrap();

        assert_eq!(retried.state, WorkState::Pending);
        assert_eq!(retried.attempts, 0);
        assert_eq!(retried.owner, None);
        assert_eq!(
            refusal(log.finish_work(
                &name,
                &WorkFence::Owner(WORKER.into()),
                &WorkOutcome::Completed(None),
                NOW_MS
            )),
            stale(&name, WorkState::Pending)
        );
        assert_eq!(
            claim(&mut log, OTHER_WORKER, NOW_MS).unwrap().work.attempts,
            1
        );
    }

    #[test]
    fn releasing_an_unstarted_claim_spends_no_attempt() {
        let (_root, _state, mut log) = fixture();
        group(&mut log, &GroupPolicy::default());
        let name = publish(&mut log, "a").remove(0);
        let assignment = claim(&mut log, WORKER, NOW_MS).unwrap();

        log.release_work(&name, &assignment.token, NOW_MS).unwrap();

        let released = log.work_detail(&name).unwrap();
        assert_eq!(released.item.state, WorkState::Pending);
        assert_eq!(released.item.attempts, 0);
        assert!(released.attempts.is_empty());
        assert_eq!(
            claim(&mut log, OTHER_WORKER, NOW_MS).unwrap().work.attempts,
            1
        );
    }

    #[test]
    fn people_hold_cancel_and_retry_queued_work() {
        let (_root, _state, mut log) = fixture();
        group(&mut log, &GroupPolicy::default());
        let name = publish(&mut log, "a").remove(0);

        let held = log.hold_work(&name, NOW_MS).unwrap();
        assert_eq!(held.state, WorkState::Paused);
        assert_eq!(held.reason.as_deref(), Some(PAUSED_BY_USER));
        assert!(claim(&mut log, WORKER, NOW_MS).is_none());
        assert_eq!(
            log.cancel_work(&name, NOW_MS).unwrap().state,
            WorkState::Cancelled
        );
        assert_eq!(
            log.retry_work(&name, NOW_MS).unwrap().state,
            WorkState::Pending
        );
        claim(&mut log, WORKER, NOW_MS).unwrap();
        assert_eq!(
            refusal(log.cancel_work(&name, NOW_MS)),
            WorkRefusal::WrongState {
                work: name.clone(),
                state: WorkState::Leased,
                action: "cancelled",
            }
        );
        assert_eq!(
            refusal(log.retry_work(&name, NOW_MS)),
            WorkRefusal::WrongState {
                work: name,
                state: WorkState::Leased,
                action: "retried",
            }
        );
    }

    #[test_case(Retention { days: 0, max_messages: MAX_MESSAGES }; "by_age")]
    #[test_case(Retention { days: RETENTION_DAYS, max_messages: 0 }; "by_count")]
    fn pruning_keeps_messages_unfinished_work_needs(limit: Retention) {
        let (_root, _state, mut log) = fixture();
        publish(&mut log, "before");
        group(&mut log, &GroupPolicy::default());
        let name = publish(&mut log, "queued").remove(0);
        let later = NOW_MS + 1;

        log.prune(&limit, later).unwrap();
        let ids = |log: &MessageLog| -> Vec<String> {
            log.history(&HistoryChannel::All, None, PAGE)
                .unwrap()
                .into_iter()
                .map(|stored| stored.message.message_id)
                .collect()
        };
        assert_eq!(ids(&log), ["queued"]);

        let assignment = claim(&mut log, WORKER, NOW_MS).unwrap();
        log.finish_work(
            &name,
            &lease(&assignment),
            &WorkOutcome::Completed(None),
            NOW_MS,
        )
        .unwrap();
        publish(&mut log, "newest");
        log.prune(&limit, later).unwrap();
        assert_eq!(ids(&log), ["newest"]);
        assert_eq!(
            refusal(log.work_item(&name)),
            WorkRefusal::UnknownWork(name)
        );
    }

    #[test]
    fn deleting_a_group_waits_for_its_work_to_finish() {
        let (_root, _state, mut log) = fixture();
        group(&mut log, &GroupPolicy::default());
        let name = publish(&mut log, "a").remove(0);

        assert_eq!(
            refusal(log.delete_group(GROUP)),
            WorkRefusal::GroupBusy(GROUP.into())
        );
        log.cancel_work(&name, NOW_MS).unwrap();
        log.delete_group(GROUP).unwrap();
        group(&mut log, &GroupPolicy::default());

        let recreated = log.group(GROUP).unwrap();
        assert_eq!(recreated.counts.cancelled, 0);
        assert!(
            log.work(&WorkFilter::default(), None, PAGE)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn paused_groups_queue_work_but_hand_none_out() {
        let (_root, _state, mut log) = fixture();
        group(&mut log, &GroupPolicy::default());
        let pause = |paused| GroupChange {
            paused: Some(paused),
            ..GroupChange::default()
        };
        log.change_group(GROUP, &pause(true), NOW_MS).unwrap();

        assert_eq!(publish(&mut log, "a").len(), 1);
        assert!(claim(&mut log, WORKER, NOW_MS).is_none());
        log.change_group(GROUP, &pause(false), NOW_MS).unwrap();
        assert!(claim(&mut log, WORKER, NOW_MS).is_some());
    }

    #[test_case(GroupPolicy { concurrency: 0, ..GroupPolicy::default() }; "no_concurrency")]
    #[test_case(GroupPolicy { concurrency: MAX_CONCURRENCY + 1, ..GroupPolicy::default() }; "too_much_concurrency")]
    #[test_case(GroupPolicy { max_attempts: 0, ..GroupPolicy::default() }; "no_attempts")]
    #[test_case(GroupPolicy { max_backlog: MAX_BACKLOG + 1, ..GroupPolicy::default() }; "oversized_backlog")]
    fn invalid_policies_are_refused(policy: GroupPolicy) {
        let (_root, _state, mut log) = fixture();
        assert!(matches!(
            refusal(log.create_group(GROUP, &owned(&[TOPIC]), &policy, NOW_MS)),
            WorkRefusal::InvalidPolicy(_)
        ));
        group(&mut log, &GroupPolicy::default());
        let change = GroupChange {
            concurrency: Some(policy.concurrency),
            max_attempts: Some(policy.max_attempts),
            max_backlog: Some(policy.max_backlog),
            ..GroupChange::default()
        };
        assert!(matches!(
            refusal(log.change_group(GROUP, &change, NOW_MS)),
            WorkRefusal::InvalidPolicy(_)
        ));
    }

    #[test]
    fn group_names_and_patterns_are_checked() {
        let (_root, _state, mut log) = fixture();
        let policy = GroupPolicy::default();
        assert_eq!(
            refusal(log.create_group(GROUP, &[], &policy, NOW_MS)),
            WorkRefusal::NoPatterns
        );
        assert_eq!(
            refusal(log.create_group(GROUP, &owned(&[INVALID_PATTERN_TEXT]), &policy, NOW_MS)),
            WorkRefusal::InvalidPattern(INVALID_PATTERN.into())
        );
        let crowded = vec![TOPIC.to_owned(); MAX_PATTERNS + 1];
        assert_eq!(
            refusal(log.create_group(GROUP, &crowded, &policy, NOW_MS)),
            WorkRefusal::TooManyPatterns
        );
        group(&mut log, &policy);
        let invalid = GroupChange {
            patterns: Some(owned(&[INVALID_PATTERN_TEXT])),
            ..GroupChange::default()
        };
        assert_eq!(
            refusal(log.change_group(GROUP, &invalid, NOW_MS)),
            WorkRefusal::InvalidPattern(INVALID_PATTERN.into())
        );
        assert_eq!(
            refusal(log.create_group(GROUP, &owned(&[TOPIC]), &policy, NOW_MS)),
            WorkRefusal::GroupExists(GROUP.into())
        );
        assert_eq!(
            refusal(log.group(OTHER_GROUP)),
            WorkRefusal::UnknownGroup(OTHER_GROUP.into())
        );
        for index in 1..MAX_GROUPS {
            create(&mut log, &format!("group-{index}"), &[TOPIC], &policy);
        }
        assert_eq!(
            refusal(log.create_group(OTHER_GROUP, &owned(&[TOPIC]), &policy, NOW_MS)),
            WorkRefusal::TooManyGroups
        );
    }

    #[test]
    fn lease_renewals_leave_the_history_revision_alone() {
        let (_root, _state, mut log) = fixture();
        group(&mut log, &GroupPolicy::default());
        let name = publish(&mut log, "a").remove(0);
        let queued = log.version().unwrap();

        let assignment = claim(&mut log, WORKER, NOW_MS).unwrap();
        let claimed = log.version().unwrap();
        let lease_until = log
            .renew_work(&name, &assignment.token, NOW_MS + LEASE_MS)
            .unwrap();

        assert_ne!(claimed, queued);
        assert_eq!(log.version().unwrap(), claimed);
        assert_eq!(lease_until, NOW_MS + 2 * LEASE_MS);
        assert_eq!(
            log.work_item(&name).unwrap().lease_until_ms,
            Some(lease_until)
        );
    }

    #[test]
    fn listings_count_and_filter_work_by_state_and_owner() {
        let (_root, _state, mut log) = fixture();
        group(&mut log, &GroupPolicy::default());
        for id in ["a", "b", "c"] {
            publish(&mut log, id);
        }
        let first = claim(&mut log, WORKER, NOW_MS).unwrap();
        log.finish_work(
            &first.work.name,
            &lease(&first),
            &WorkOutcome::Completed(None),
            NOW_MS,
        )
        .unwrap();
        let second = claim(&mut log, OTHER_WORKER, NOW_MS).unwrap();

        let counts = log.group(GROUP).unwrap().counts;
        assert_eq!((counts.pending, counts.active, counts.completed), (1, 1, 1));
        let owned = log
            .work(
                &WorkFilter {
                    owner: Some(OTHER_WORKER.into()),
                    states: vec![WorkState::Leased, WorkState::Pausing, WorkState::Paused],
                    ..WorkFilter::default()
                },
                None,
                PAGE,
            )
            .unwrap();
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].name, second.work.name);
        assert_eq!(owned[0].message.message.text, TEXT);
        let newest_first: Vec<i64> = log
            .work(&WorkFilter::default(), None, PAGE)
            .unwrap()
            .iter()
            .map(|item| item.id)
            .collect();
        assert!(newest_first.is_sorted_by(|newer, older| newer > older));
        assert_eq!(
            log.work(&WorkFilter::default(), Some(newest_first[1]), PAGE)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn every_insert_and_state_change_takes_a_new_larger_stamp_across_reopens() {
        let (_root, state, mut log) = fixture();
        group(&mut log, &GroupPolicy::default());
        let name = publish(&mut log, "a").remove(0);
        let stamp = |log: &MessageLog, name: &str| log.work_item(name).unwrap().changed;
        let queued = stamp(&log, &name);
        let assignment = claim(&mut log, WORKER, NOW_MS).unwrap();
        let claimed = stamp(&log, &name);
        log.renew_work(&name, &assignment.token, NOW_MS).unwrap();
        let renewed = stamp(&log, &name);
        log.finish_work(
            &name,
            &lease(&assignment),
            &WorkOutcome::Completed(None),
            NOW_MS,
        )
        .unwrap();
        let completed = stamp(&log, &name);
        assert_eq!(log.last_work_change().unwrap(), completed);
        log.delete_group(GROUP).unwrap();
        drop(log);

        let mut reopened = MessageLog::open(&state, &retention(), NOW_MS).unwrap();
        group(&mut reopened, &GroupPolicy::default());
        let later = publish(&mut reopened, "b").remove(0);
        let republished = stamp(&reopened, &later);

        assert!(queued < claimed);
        assert_eq!(assignment.work.changed, claimed);
        assert_eq!(renewed, claimed);
        assert!(claimed < completed);
        assert!(completed < republished);
        assert_eq!(reopened.last_work_change().unwrap(), republished);
    }

    #[test]
    fn changed_work_filters_by_publisher_and_pages_by_stamp() {
        let (_root, _state, mut log) = fixture();
        group(&mut log, &GroupPolicy::default());
        let mine: Vec<String> = ["a", "b", "c"]
            .into_iter()
            .flat_map(|id| publish(&mut log, id))
            .collect();
        log.record_publication(
            &message("d", MessageAudience::Topic(TOPIC.into()), OTHER_PUBLISHER),
            &[],
            usize::MAX,
        )
        .unwrap();
        claim(&mut log, WORKER, NOW_MS).unwrap();
        let filter = WorkFilter {
            publisher: Some(PUBLISHER.into()),
            ..WorkFilter::default()
        };

        let mut paged = Vec::new();
        let mut after = 0;
        while let Some(item) = log.work_changed_after(&filter, after, 1).unwrap().pop() {
            after = item.changed;
            paged.push(item.name);
        }

        assert_eq!(
            paged,
            [mine[1].as_str(), mine[2].as_str(), mine[0].as_str()]
        );
        let every = log
            .work_changed_after(&WorkFilter::default(), 0, PAGE)
            .unwrap();
        assert_eq!(every.len(), mine.len() + 1);
        assert!(every.is_sorted_by(|older, newer| older.changed < newer.changed));
    }
}
