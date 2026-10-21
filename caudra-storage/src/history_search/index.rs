//! Brings the index up to date with stored history, newest sessions first, in
//! short write transactions so a session save never waits long behind it.
//!
//! A stream's cursor names the next ordinal to index and the id and size of
//! the last item indexed. While that item still sits where the cursor says,
//! the stream only grew and just the new items are indexed; anything else,
//! such as a full save renumbering the stream, re-indexes it from the start.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::Value;

use super::extract::{call_id, classify, index_body, item_id};
use super::{CatchUp, HistoryKind, HistoryScope, INDEX_VERSION, source};
use crate::id::CaudraId;
use crate::sessions::{
    RuntimeRetry, SessionDatabase, SessionError, decompress_payload, from_i64_usize, id_from_row,
    to_i64,
};

const BATCH_ITEMS: usize = 256;
const BATCH_TIME: Duration = Duration::from_millis(50);
/// How far before a batch a tool result looks for the call that named it.
const TOOL_CALL_LOOKBACK: usize = 64;
const CATCH_UP_OPERATION: &str = "history search catch-up";
const STATE_SCOPE: &str = "global";
const INDEX_VERSION_KEY: &str = "history_search.index_version";
const PAYLOAD_FIELD: &str = "history payload";
const ORDINAL_FIELD: &str = "history ordinal";
const CURSOR_FIELD: &str = "history search cursor";
const BUSY_TIMEOUT_PRAGMA: &str = "busy_timeout";

/// One stream of one session.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamKey {
    session: CaudraId,
    stream: String,
}

struct Cursor {
    indexed: usize,
    last_item: Option<Vec<u8>>,
    last_bytes: Option<i64>,
}

struct StreamStep {
    items: usize,
    done: bool,
}

struct Indexed {
    ordinal: usize,
    item: Option<CaudraId>,
    bytes: i64,
}

impl SessionDatabase {
    /// Indexes what changed in `scope` until `deadline`. Needs a writable
    /// connection; searches never write.
    pub fn catch_up_history(
        &self,
        scope: &HistoryScope,
        deadline: Instant,
    ) -> Result<CatchUp, SessionError> {
        let connection = self.connection();
        let busy: i64 =
            connection.query_row(&format!("PRAGMA {BUSY_TIMEOUT_PRAGMA}"), [], |row| {
                row.get(0)
            })?;
        connection.busy_timeout(Duration::ZERO)?;
        let result = catch_up(connection, scope, deadline);
        connection.pragma_update(None, BUSY_TIMEOUT_PRAGMA, busy)?;
        result
    }
}

fn catch_up(
    connection: &Connection,
    scope: &HistoryScope,
    deadline: Instant,
) -> Result<CatchUp, SessionError> {
    let retry = RuntimeRetry::new(Some(deadline), &|| false);
    let mut queue = VecDeque::new();
    let mut indexed_items = 0;
    let outcome = (|| {
        ensure_index_version(connection, &retry)?;
        queue = changed_streams(connection, scope)?;
        while !queue.is_empty() && Instant::now() < deadline {
            let (items, finished) = immediate(connection, &retry, |transaction| {
                batch(transaction, &queue, deadline)
            })?;
            indexed_items += items;
            queue.drain(..finished);
        }
        Ok(())
    })();
    match outcome {
        Ok(())
        | Err(SessionError::RuntimeDeadline { .. } | SessionError::RuntimeContention { .. }) => {}
        Err(error) => return Err(error),
    }
    let pending: HashSet<CaudraId> = queue.iter().map(|key| key.session).collect();
    Ok(CatchUp {
        indexed_items,
        pending_sessions: pending.len(),
    })
}

/// A `BEGIN IMMEDIATE` transaction retried while another writer holds the
/// lock. Unlike [`SessionDatabase::runtime_transaction`] it never discards a
/// finished batch because the deadline passed while it ran.
fn immediate<T>(
    connection: &Connection,
    retry: &RuntimeRetry<'_>,
    mut write: impl FnMut(&Transaction<'_>) -> Result<T, SessionError>,
) -> Result<T, SessionError> {
    retry.run(CATCH_UP_OPERATION, || {
        let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
        let value = write(&transaction)?;
        transaction.commit()?;
        Ok(value)
    })
}

fn ensure_index_version(
    connection: &Connection,
    retry: &RuntimeRetry<'_>,
) -> Result<(), SessionError> {
    let version = INDEX_VERSION.to_string();
    let current: Option<String> = connection
        .query_row(
            "SELECT value FROM state WHERE scope = ?1 AND key = ?2",
            params![STATE_SCOPE, INDEX_VERSION_KEY],
            |row| row.get(0),
        )
        .optional()?;
    if current.as_deref() == Some(version.as_str()) {
        return Ok(());
    }
    immediate(connection, retry, |transaction| {
        transaction.execute_batch(
            "DELETE FROM history_search_docs; DELETE FROM history_search_cursors;",
        )?;
        transaction.execute(
            "INSERT INTO state (scope, key, value, updated_at) VALUES (?1, ?2, ?3, unixepoch()) \
             ON CONFLICT(scope, key) DO UPDATE SET \
                 value = excluded.value, updated_at = excluded.updated_at",
            params![STATE_SCOPE, INDEX_VERSION_KEY, version],
        )?;
        Ok(())
    })
}

/// Streams in scope whose items differ from their cursor, newest session
/// first, plus indexed subagent streams a full save removed.
fn changed_streams(
    connection: &Connection,
    scope: &HistoryScope,
) -> Result<VecDeque<StreamKey>, SessionError> {
    let (filter, values) = scope.filter();
    let sql = format!(
        "SELECT s.id, '' AS stream, s.updated_at, \
             (SELECT max(h.ordinal) + 1 FROM main_history_items h WHERE h.session_id = s.id), \
             c.indexed, c.last_bytes, \
             (SELECT h.byte_count FROM main_history_items h \
              WHERE h.session_id = s.id AND h.ordinal = c.indexed - 1) \
         FROM sessions s \
         LEFT JOIN history_search_cursors c ON c.session_id = s.id AND c.stream = '' \
         WHERE {filter} \
         UNION ALL \
         SELECT s.id, st.subagent_id, s.updated_at, \
             (SELECT max(h.ordinal) + 1 FROM subagent_history_items h \
              WHERE h.session_id = st.session_id AND h.subagent_id = st.subagent_id), \
             c.indexed, c.last_bytes, \
             (SELECT h.byte_count FROM subagent_history_items h \
              WHERE h.session_id = st.session_id AND h.subagent_id = st.subagent_id \
                AND h.ordinal = c.indexed - 1) \
         FROM subagent_streams st JOIN sessions s ON s.id = st.session_id \
         LEFT JOIN history_search_cursors c \
             ON c.session_id = st.session_id AND c.stream = st.subagent_id \
         WHERE {filter} \
         UNION ALL \
         SELECT s.id, c.stream, s.updated_at, NULL, c.indexed, c.last_bytes, NULL \
         FROM history_search_cursors c JOIN sessions s ON s.id = c.session_id \
         WHERE c.stream <> '' AND NOT EXISTS (SELECT 1 FROM subagent_streams st \
             WHERE st.session_id = c.session_id AND st.subagent_id = c.stream) AND {filter} \
         ORDER BY 3 DESC, 1, 2"
    );
    let mut statement = connection.prepare(&sql)?;
    let parameters = values.iter().chain(&values).chain(&values);
    let mut rows = statement.query(rusqlite::params_from_iter(parameters))?;
    let mut changed = VecDeque::new();
    while let Some(row) = rows.next()? {
        let count: Option<i64> = row.get(3)?;
        let indexed: Option<i64> = row.get(4)?;
        let last_bytes: Option<i64> = row.get(5)?;
        let current_bytes: Option<i64> = row.get(6)?;
        let indexed = indexed.unwrap_or(0);
        if count.unwrap_or(0) != indexed || (indexed > 0 && current_bytes != last_bytes) {
            changed.push_back(StreamKey {
                session: id_from_row(row, 0)?,
                stream: row.get(1)?,
            });
        }
    }
    Ok(changed)
}

/// Indexes the front of `queue` for one transaction's worth of items or time,
/// returning the items indexed and how many streams it finished.
fn batch(
    transaction: &Transaction<'_>,
    queue: &VecDeque<StreamKey>,
    deadline: Instant,
) -> Result<(usize, usize), SessionError> {
    let batch_deadline = deadline.min(Instant::now() + BATCH_TIME);
    let (mut items, mut finished) = (0, 0);
    for key in queue {
        let step = index_stream(transaction, key, BATCH_ITEMS - items, batch_deadline)?;
        items += step.items;
        if !step.done {
            break;
        }
        finished += 1;
        if items >= BATCH_ITEMS || Instant::now() >= batch_deadline {
            break;
        }
    }
    Ok((items, finished))
}

fn index_stream(
    transaction: &Transaction<'_>,
    key: &StreamKey,
    budget: usize,
    deadline: Instant,
) -> Result<StreamStep, SessionError> {
    let session = key.session.as_bytes().as_slice();
    let source = source(&key.stream);
    let count: Option<i64> = transaction.query_row(
        &format!("SELECT max(ordinal) + 1 FROM {source}"),
        params![session, key.stream],
        |row| row.get(0),
    )?;
    let count = count.map_or(Ok(0), |count| from_i64_usize(count, ORDINAL_FIELD))?;
    let cursor = transaction
        .query_row(
            "SELECT indexed, last_item, last_bytes FROM history_search_cursors \
             WHERE session_id = ?1 AND stream = ?2",
            params![session, key.stream],
            |row| Ok((row.get::<_, i64>(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
        .map(|(indexed, last_item, last_bytes)| {
            Ok::<_, SessionError>(Cursor {
                indexed: from_i64_usize(indexed, CURSOR_FIELD)?,
                last_item,
                last_bytes,
            })
        })
        .transpose()?;
    let start = match &cursor {
        Some(cursor) if cursor.indexed <= count && extends(transaction, key, cursor)? => {
            cursor.indexed
        }
        Some(_) => {
            transaction.execute(
                "DELETE FROM history_search_docs WHERE session_id = ?1 AND stream = ?2",
                params![session, key.stream],
            )?;
            0
        }
        None => 0,
    };
    let mut statement = transaction.prepare(&format!(
        "SELECT ordinal, byte_count, payload FROM {source} AND ordinal >= ?3 \
         ORDER BY ordinal LIMIT ?4"
    ))?;
    let mut rows = statement.query(params![
        session,
        key.stream,
        to_i64(start, ORDINAL_FIELD)?,
        to_i64(budget, ORDINAL_FIELD)?
    ])?;
    let mut calls = HashMap::new();
    let mut last = None;
    let mut items = 0;
    while let Some(row) = rows.next()? {
        let ordinal = from_i64_usize(row.get(0)?, ORDINAL_FIELD)?;
        let bytes: i64 = row.get(1)?;
        let stored: Vec<u8> = row.get(2)?;
        let payload: Option<Value> =
            serde_json::from_str(&decompress_payload(&stored, PAYLOAD_FIELD)?).ok();
        let item = payload.as_ref().and_then(item_id);
        if let (Some(payload), Some(item)) = (&payload, item) {
            index_item(transaction, key, start, ordinal, item, payload, &mut calls)?;
        }
        last = Some(Indexed {
            ordinal,
            item,
            bytes,
        });
        items += 1;
        if items >= budget || Instant::now() >= deadline {
            break;
        }
    }
    drop(rows);
    let next = last.as_ref().map_or(start, |last| last.ordinal + 1);
    match last {
        Some(last) => {
            transaction.execute(
                "INSERT INTO history_search_cursors \
                     (session_id, stream, indexed, last_item, last_bytes) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(session_id, stream) DO UPDATE SET indexed = excluded.indexed, \
                     last_item = excluded.last_item, last_bytes = excluded.last_bytes",
                params![
                    session,
                    key.stream,
                    to_i64(next, CURSOR_FIELD)?,
                    last.item.as_ref().map(|item| item.as_bytes().as_slice()),
                    last.bytes
                ],
            )?;
        }
        None if count == 0 => {
            transaction.execute(
                "DELETE FROM history_search_cursors WHERE session_id = ?1 AND stream = ?2",
                params![session, key.stream],
            )?;
        }
        None => {}
    }
    Ok(StreamStep {
        items,
        done: next >= count,
    })
}

/// Whether the last item the cursor indexed is still where it was, so the
/// stream only grew since.
fn extends(
    transaction: &Transaction<'_>,
    key: &StreamKey,
    cursor: &Cursor,
) -> Result<bool, SessionError> {
    let Some(ordinal) = cursor.indexed.checked_sub(1) else {
        return Ok(true);
    };
    let row: Option<(i64, Vec<u8>)> = transaction
        .query_row(
            &format!(
                "SELECT byte_count, payload FROM {} AND ordinal = ?3",
                source(&key.stream)
            ),
            params![
                key.session.as_bytes().as_slice(),
                key.stream,
                to_i64(ordinal, ORDINAL_FIELD)?
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((bytes, stored)) = row else {
        return Ok(false);
    };
    if Some(bytes) != cursor.last_bytes {
        return Ok(false);
    }
    let Some(last_item) = &cursor.last_item else {
        return Ok(true);
    };
    Ok(stored_item_id(&stored)?.is_some_and(|item| item.as_bytes().as_slice() == last_item))
}

fn stored_item_id(stored: &[u8]) -> Result<Option<CaudraId>, SessionError> {
    let text = decompress_payload(stored, PAYLOAD_FIELD)?;
    Ok(serde_json::from_str::<Value>(&text)
        .ok()
        .as_ref()
        .and_then(item_id))
}

fn index_item(
    transaction: &Transaction<'_>,
    key: &StreamKey,
    batch_start: usize,
    ordinal: usize,
    item: CaudraId,
    payload: &Value,
    calls: &mut HashMap<String, String>,
) -> Result<(), SessionError> {
    let Some((kind, call_tool)) = classify(payload) else {
        return Ok(());
    };
    if let (Some(call), Some(tool)) = (call_id(payload), call_tool) {
        calls.insert(call.to_owned(), tool.to_owned());
    }
    let Some(body) = index_body(payload) else {
        return Ok(());
    };
    let tool = match kind {
        HistoryKind::ToolCall => call_tool.map(str::to_owned),
        HistoryKind::ToolResult => match call_id(payload) {
            Some(call) => match calls.get(call) {
                Some(tool) => Some(tool.clone()),
                None => called_tool(transaction, key, batch_start, ordinal, call)?,
            },
            None => None,
        },
        _ => None,
    };
    let at_ms = item.created_ms().unwrap_or_default();
    let inserted = transaction.execute(
        "INSERT INTO history_search_docs \
             (session_id, stream, ordinal, item_id, kind, tool, at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT DO NOTHING",
        params![
            key.session.as_bytes().as_slice(),
            key.stream,
            to_i64(ordinal, ORDINAL_FIELD)?,
            item.as_bytes().as_slice(),
            kind.code(),
            tool,
            to_i64(at_ms, ORDINAL_FIELD)?
        ],
    )?;
    if inserted == 1 {
        transaction.execute(
            "INSERT INTO history_search (rowid, body) VALUES (?1, ?2)",
            params![transaction.last_insert_rowid(), body],
        )?;
    }
    Ok(())
}

/// The tool a result answers, from a call indexed before this batch began.
fn called_tool(
    transaction: &Transaction<'_>,
    key: &StreamKey,
    batch_start: usize,
    ordinal: usize,
    call: &str,
) -> Result<Option<String>, SessionError> {
    let earliest = ordinal.saturating_sub(TOOL_CALL_LOOKBACK);
    if earliest >= batch_start {
        return Ok(None);
    }
    let mut statement = transaction.prepare(&format!(
        "SELECT payload FROM {} AND ordinal >= ?3 AND ordinal < ?4 ORDER BY ordinal DESC",
        source(&key.stream)
    ))?;
    let mut rows = statement.query(params![
        key.session.as_bytes().as_slice(),
        key.stream,
        to_i64(earliest, ORDINAL_FIELD)?,
        to_i64(batch_start, ORDINAL_FIELD)?
    ])?;
    while let Some(row) = rows.next()? {
        let stored: Vec<u8> = row.get(0)?;
        let Ok(payload) =
            serde_json::from_str::<Value>(&decompress_payload(&stored, PAYLOAD_FIELD)?)
        else {
            continue;
        };
        if let Some((HistoryKind::ToolCall, Some(tool))) = classify(&payload)
            && call_id(&payload) == Some(call)
        {
            return Ok(Some(tool.to_owned()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use test_case::test_case;

    use super::super::fixtures::{
        PROJECT, SHELL, TestSession, assistant, catch_up, count, database, session_with, tool_call,
        tool_result, user,
    };
    use super::super::{HistoryQuery, HistoryStream, ProjectScope};
    use super::*;

    const DOCS: &str = "SELECT count(*) FROM history_search_docs";
    const TERM_ROWS: &str =
        "SELECT count(*) FROM history_search WHERE history_search MATCH 'zebra'";
    const AGENT: &str = "toolu_agent";
    const MIN_SQLITE: (u32, u32) = (3, 43);
    const SYNTHETIC_ITEMS: usize = 100_000;
    const SYNTHETIC_SESSIONS: usize = 100;
    const VOCABULARY: usize = 20_000;
    const SYLLABLES: [&str; 16] = [
        "ka", "lo", "mi", "ren", "tu", "sa", "vel", "or", "qui", "dan", "pe", "zor", "il", "mu",
        "tex", "ba",
    ];
    const QUERIES: usize = 400;
    const SEED: u64 = 0x9e37_79b9_7f4a_7c15;
    const MEDIAN: f64 = 0.5;
    const PERCENTILE_95: f64 = 0.95;
    const SLOWEST: f64 = 1.0;
    const POOL: usize = 64;
    const POOL_PER_SESSION: usize = 8;
    const MIB: f64 = 1024.0 * 1024.0;
    const MILLIS_PER_SECOND: f64 = 1_000.0;

    fn search(database: &SessionDatabase, text: &str) -> Vec<HistoryKind> {
        database
            .search_history(&HistoryQuery::new(text, HistoryScope::All))
            .unwrap()
            .hits
            .into_iter()
            .map(|hit| hit.kind)
            .collect()
    }

    #[test]
    fn bundled_sqlite_supports_contentless_delete() {
        let (_temp, database) = database();
        let version: String = database
            .connection()
            .query_row("SELECT sqlite_version()", [], |row| row.get(0))
            .unwrap();
        let mut parts = version.split('.').map(|part| part.parse::<u32>().unwrap());
        assert!((parts.next().unwrap(), parts.next().unwrap()) >= MIN_SQLITE);
    }

    #[test]
    fn appended_items_are_indexed_once() {
        let (_temp, mut database) = database();
        let mut session = session_with(PROJECT, "t", vec![user("zebra one"), assistant("two")]);
        let cursor = database.save(&session, None).unwrap();
        assert_eq!(catch_up(&database, &HistoryScope::All).indexed_items, 2);

        session.push_message(user("zebra three"));
        database.save(&session, Some(&cursor)).unwrap();

        assert_eq!(catch_up(&database, &HistoryScope::All).indexed_items, 1);
        assert_eq!(catch_up(&database, &HistoryScope::All), CatchUp::default());
        assert_eq!(count(&database, DOCS), 3);
        assert_eq!(count(&database, TERM_ROWS), 2);
    }

    #[test]
    fn a_rewritten_stream_is_indexed_again() {
        let (_temp, mut database) = database();
        let mut session = session_with(
            PROJECT,
            "t",
            vec![user("zebra"), assistant("old"), user("older")],
        );
        database.save(&session, None).unwrap();
        catch_up(&database, &HistoryScope::All);

        session.replace_messages(vec![user("fresh"), assistant("answer")]);
        database.save(&session, None).unwrap();

        assert_eq!(catch_up(&database, &HistoryScope::All).indexed_items, 2);
        assert_eq!(count(&database, DOCS), 2);
        assert_eq!(count(&database, TERM_ROWS), 0);
        assert_eq!(search(&database, "fresh"), [HistoryKind::User]);
    }

    #[test]
    fn subagent_streams_are_indexed_apart() {
        let (_temp, mut database) = database();
        let mut session = session_with(PROJECT, "t", vec![user("parent")]);
        session.set_subagent_messages(AGENT.to_owned(), vec![assistant("zebra child")]);
        database.save(&session, None).unwrap();

        assert_eq!(catch_up(&database, &HistoryScope::All).indexed_items, 2);

        let page = database
            .search_history(&HistoryQuery::new(
                "zebra",
                HistoryScope::Session(session.id),
            ))
            .unwrap();
        assert_eq!(
            page.hits[0].reference.stream,
            HistoryStream::Subagent(AGENT.to_owned())
        );
    }

    #[test]
    fn forgetting_a_session_clears_its_index() {
        let (_temp, mut database) = database();
        let session = session_with(PROJECT, "t", vec![user("zebra")]);
        database.save(&session, None).unwrap();
        catch_up(&database, &HistoryScope::All);
        assert_eq!(count(&database, TERM_ROWS), 1);

        database.delete(session.id, None).unwrap();

        assert_eq!(count(&database, DOCS), 0);
        assert_eq!(count(&database, TERM_ROWS), 0);
        assert_eq!(
            count(&database, "SELECT count(*) FROM history_search_cursors"),
            0
        );
    }

    #[test]
    fn a_new_index_version_rebuilds_everything() {
        let (_temp, mut database) = database();
        let session = session_with(PROJECT, "t", vec![user("zebra"), assistant("two")]);
        database.save(&session, None).unwrap();
        catch_up(&database, &HistoryScope::All);
        database
            .connection()
            .execute(
                "UPDATE state SET value = '0' WHERE key = ?1",
                [INDEX_VERSION_KEY],
            )
            .unwrap();

        assert_eq!(catch_up(&database, &HistoryScope::All).indexed_items, 2);
        assert_eq!(count(&database, TERM_ROWS), 1);
    }

    #[test]
    fn a_passed_deadline_leaves_sessions_pending() {
        let (_temp, mut database) = database();
        for _ in 0..2 {
            database
                .save(&session_with(PROJECT, "t", vec![user("zebra")]), None)
                .unwrap();
        }
        catch_up(&database, &HistoryScope::All);
        database
            .save(&session_with(PROJECT, "t", vec![user("zebra")]), None)
            .unwrap();

        let progress = database
            .catch_up_history(&HistoryScope::All, Instant::now())
            .unwrap();

        assert_eq!(progress.indexed_items, 0);
        assert_eq!(progress.pending_sessions, 1);
    }

    #[test_case(true; "same_batch")]
    #[test_case(false; "earlier_batch")]
    fn a_tool_result_carries_its_calls_tool(same_batch: bool) {
        let (_temp, mut database) = database();
        let mut session = session_with(PROJECT, "t", vec![tool_call("c1", SHELL, "ls")]);
        let cursor = database.save(&session, None).unwrap();
        if !same_batch {
            catch_up(&database, &HistoryScope::All);
        }
        session.push_message(tool_result("c1", "zebra.txt"));
        database.save(&session, Some(&cursor)).unwrap();
        catch_up(&database, &HistoryScope::All);

        let mut query = HistoryQuery::new("zebra", HistoryScope::All);
        query.tools = Some(vec![SHELL.to_owned()]);
        let page = database.search_history(&query).unwrap();

        assert_eq!(page.hits.len(), 1);
        assert_eq!(page.hits[0].kind, HistoryKind::ToolResult);
        assert_eq!(page.hits[0].tool.as_deref(), Some(SHELL));
    }

    #[test]
    fn scope_limits_what_is_indexed() {
        let (_temp, mut database) = database();
        database
            .save(&session_with(PROJECT, "t", vec![user("zebra")]), None)
            .unwrap();
        database
            .save(&session_with("/elsewhere", "t", vec![user("zebra")]), None)
            .unwrap();

        let scope = HistoryScope::Project(ProjectScope::Local(vec![PROJECT.into()]));
        assert_eq!(catch_up(&database, &scope).indexed_items, 1);
        assert_eq!(catch_up(&database, &HistoryScope::All).indexed_items, 1);
    }

    /// Run with `--ignored --nocapture` to measure catch-up throughput.
    #[test]
    #[ignore = "benchmark"]
    fn catch_up_throughput() {
        let (_temp, mut database) = database();
        let per_session = SYNTHETIC_ITEMS / SYNTHETIC_SESSIONS;
        for session_index in 0..SYNTHETIC_SESSIONS {
            let items = (0..per_session)
                .map(|index| match index % 3 {
                    0 => user(&format!(
                        "session {session_index} asks about retry loop {index}"
                    )),
                    1 => tool_call(&format!("c{index}"), SHELL, "cargo nextest run -p caudra"),
                    _ => tool_result(&format!("c{}", index - 1), &"test result ok\n".repeat(40)),
                })
                .collect();
            let session: TestSession = session_with(PROJECT, "bench", items);
            database.save(&session, None).unwrap();
        }
        let started = Instant::now();
        let progress = catch_up(&database, &HistoryScope::All);
        let elapsed = started.elapsed();
        println!(
            "indexed {} items in {elapsed:?} ({:.0} items/s)",
            progress.indexed_items,
            progress.indexed_items as f64 / elapsed.as_secs_f64()
        );
        assert_eq!(progress.indexed_items, SYNTHETIC_ITEMS);
    }

    /// A deterministic generator, so every run measures the same corpus.
    struct Corpus(u64);

    impl Corpus {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }

        /// A word's rank, log-uniform over the vocabulary, which gives the long tail of
        /// conversational text: a few words everywhere and most of them rare.
        fn rank(&mut self) -> usize {
            let uniform = self.next() as f64 / u64::MAX as f64;
            ((VOCABULARY as f64).powf(uniform) as usize).clamp(1, VOCABULARY) - 1
        }

        /// `least` words and up to `spread` more.
        fn text(&mut self, vocabulary: &[String], least: usize, spread: usize) -> String {
            let words = least + self.below(spread);
            (0..words)
                .map(|_| vocabulary[self.rank()].as_str())
                .collect::<Vec<_>>()
                .join(" ")
        }
    }

    fn vocabulary() -> Vec<String> {
        (0..VOCABULARY)
            .map(|rank| {
                let mut word = String::new();
                let mut rest = rank + SYLLABLES.len();
                while rest > 0 {
                    word.push_str(SYLLABLES[rest % SYLLABLES.len()]);
                    rest /= SYLLABLES.len();
                }
                word
            })
            .collect()
    }

    fn pages(database: &SessionDatabase) -> u64 {
        let pragma = |name: &str| -> u64 {
            database
                .connection()
                .query_row(&format!("PRAGMA {name}"), [], |row| row.get::<_, i64>(0))
                .unwrap()
                .unsigned_abs()
        };
        (pragma("page_count") - pragma("freelist_count")) * pragma("page_size")
    }

    /// Run with `--ignored --nocapture`: index size against the text it covers, and lexical
    /// query latency over a varied 100k-item corpus, as the funnel asks for its pool.
    #[test]
    #[ignore = "benchmark"]
    fn query_latency_and_index_size() {
        let (_temp, mut database) = database();
        let vocabulary = vocabulary();
        let mut corpus = Corpus(SEED);
        let per_session = SYNTHETIC_ITEMS / SYNTHETIC_SESSIONS;
        let mut text_bytes = 0;
        for session_index in 0..SYNTHETIC_SESSIONS {
            let items = (0..per_session)
                .map(|index| {
                    let item = match index % 4 {
                        0 => user(&corpus.text(&vocabulary, 10, 30)),
                        1 => assistant(&corpus.text(&vocabulary, 30, 120)),
                        2 => {
                            tool_call(&format!("c{index}"), SHELL, &corpus.text(&vocabulary, 3, 8))
                        }
                        _ => tool_result(
                            &format!("c{}", index - 1),
                            &corpus.text(&vocabulary, 50, 350),
                        ),
                    };
                    text_bytes += item.to_string().len();
                    item
                })
                .collect();
            let session: TestSession =
                session_with(PROJECT, &format!("bench {session_index}"), items);
            database.save(&session, None).unwrap();
        }
        let before = pages(&database);
        let started = Instant::now();
        assert_eq!(
            catch_up(&database, &HistoryScope::All).indexed_items,
            SYNTHETIC_ITEMS
        );
        let indexed_in = started.elapsed();
        let index_bytes = pages(&database) - before;

        let mut latencies: Vec<f64> = (0..QUERIES)
            .map(|index| {
                let words = 1 + index % 3;
                let text = (0..words)
                    .map(|_| vocabulary[corpus.rank()].as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                let query = HistoryQuery {
                    limit: POOL,
                    per_session_cap: Some(POOL_PER_SESSION),
                    ..HistoryQuery::new(text, HistoryScope::All)
                };
                let started = Instant::now();
                database.search_history(&query).unwrap();
                started.elapsed().as_secs_f64() * MILLIS_PER_SECOND
            })
            .collect();
        latencies.sort_by(f64::total_cmp);
        let at = |share: f64| latencies[((latencies.len() - 1) as f64 * share) as usize];
        println!(
            "corpus: {SYNTHETIC_ITEMS} items, {:.1} MiB of item JSON; indexed in {indexed_in:?}",
            text_bytes as f64 / MIB
        );
        println!(
            "index: {:.1} MiB, {:.2}x the item JSON",
            index_bytes as f64 / MIB,
            index_bytes as f64 / text_bytes as f64
        );
        println!(
            "query ({QUERIES} queries, pool {POOL}, {POOL_PER_SESSION} per session): p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms",
            at(MEDIAN),
            at(PERCENTILE_95),
            at(SLOWEST)
        );
    }
}
