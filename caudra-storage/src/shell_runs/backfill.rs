//! Fills `shell_runs` from the shell calls session history still holds, so the
//! model starts out knowing this machine. Sessions are read newest first until
//! the log is full; a call counts once its output shows how long it ran,
//! whether it ran in the foreground or as a background task.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::thread::{self, available_parallelism};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde::de::IgnoredAny;

use super::reading::{Call, Reading, read};
use super::{MAX_SHELL_RUNS, RunOutcome, ShellRun, insert};
use crate::sessions::{SessionError, decompress_payload};

const ITEM_FIELD: &str = "shell run backfill history item";
const OUTPUT_FIELD: &str = "shell run backfill tool output";
const SHELL_TOOL: &str = "shell";
const TOOL_CALL: &str = "tool_call";
const SHELL_MARKER: &str = "\"shell\"";
const TOOL_CALL_MARKER: &str = "\"tool_call\"";
/// Rows per decoding thread before another one pays for itself.
const ROWS_PER_WORKER: usize = 256;
const MAIN_ITEMS: &str =
    "SELECT payload FROM main_history_items WHERE session_id = ?1 ORDER BY ordinal";
const SUBAGENT_ITEMS: &str = "SELECT payload FROM subagent_history_items \
     WHERE session_id = ?1 ORDER BY subagent_id, ordinal";

type CallKey = (Vec<u8>, String);

#[derive(Deserialize)]
struct HistoryCall {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    call_id: String,
    #[serde(default)]
    input: CallInput,
}

#[derive(Deserialize, Default)]
struct CallInput {
    command: Option<String>,
    workdir: Option<String>,
    #[serde(rename = "timeoutSec")]
    timeout_secs: Option<f64>,
}

struct ShellCall {
    call_id: String,
    command: String,
    workdir: Option<String>,
    timeout_secs: Option<f64>,
}

impl ShellCall {
    fn reading(&self, cwd: &str) -> Reading {
        read(&Call {
            command: &self.command,
            workdir: self.workdir.as_deref(),
            cwd: Some(cwd),
            timeout_secs: self.timeout_secs,
        })
    }
}

#[derive(Deserialize)]
struct ShellOutput {
    #[serde(rename = "Shell")]
    shell: Option<Measured>,
}

#[derive(Deserialize)]
struct Measured {
    duration_ms: u64,
    exit_code: Option<i64>,
    #[serde(default)]
    signal: Option<IgnoredAny>,
    #[serde(default)]
    timed_out: bool,
    #[serde(default)]
    output_limit_exceeded: bool,
}

impl Measured {
    fn outcome(&self) -> Option<RunOutcome> {
        if self.timed_out {
            Some(RunOutcome::TimedOut)
        } else if self.signal.is_some() || self.output_limit_exceeded {
            Some(RunOutcome::Stopped)
        } else {
            self.exit_code.map(|_| RunOutcome::Completed)
        }
    }
}

#[derive(Deserialize)]
struct BackgroundTask {
    payload: BackgroundPayload,
    outcome: Option<BackgroundOutcome>,
}

#[derive(Deserialize)]
struct BackgroundPayload {
    kind: String,
    metadata: Option<BackgroundMetadata>,
}

#[derive(Deserialize)]
struct BackgroundMetadata {
    call_id: Option<String>,
}

#[derive(Deserialize)]
struct BackgroundOutcome {
    shell: Option<ShellOutput>,
}

/// How many runs it stored. Unreadable rows are skipped rather than failing
/// the migration: a run the model never sees only costs it one example.
pub(super) fn backfill(connection: &Connection) -> Result<usize, SessionError> {
    let mut background = background_runs(connection)?;
    let sessions: Vec<(Vec<u8>, i64, String)> = connection
        .prepare("SELECT id, updated_at, cwd FROM sessions ORDER BY updated_at DESC, id DESC")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let mut newest_first = Vec::new();
    let mut found = 0;
    for (session, updated_at, cwd) in sessions {
        if found >= MAX_SHELL_RUNS {
            break;
        }
        let runs = session_runs(connection, &session, &cwd, &mut background)?;
        found += runs.len();
        newest_first.push((u64::try_from(updated_at).unwrap_or_default(), runs));
    }
    let mut stored = 0;
    for (recorded_at, run) in newest_first
        .into_iter()
        .rev()
        .flat_map(|(recorded_at, runs)| runs.into_iter().map(move |run| (recorded_at, run)))
        .skip(found.saturating_sub(MAX_SHELL_RUNS))
    {
        insert(connection, recorded_at, &run, None)?;
        stored += 1;
    }
    Ok(stored)
}

fn session_runs(
    connection: &Connection,
    session: &[u8],
    cwd: &str,
    background: &mut HashMap<CallKey, Measured>,
) -> Result<Vec<ShellRun>, SessionError> {
    let mut calls = shell_calls(connection, MAIN_ITEMS, session)?;
    calls.extend(shell_calls(connection, SUBAGENT_ITEMS, session)?);
    let mut output = connection.prepare_cached(
        "SELECT payload FROM tool_outputs WHERE session_id = ?1 AND tool_id = ?2",
    )?;
    let mut runs = Vec::new();
    for call in calls {
        let foreground = output
            .query_row(params![session, call.call_id], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .optional()?
            .and_then(|payload| foreground_output(&payload));
        let Some(measured) =
            foreground.or_else(|| background.remove(&(session.to_vec(), call.call_id.clone())))
        else {
            continue;
        };
        if let Some(outcome) = measured.outcome() {
            runs.push(ShellRun {
                reading: call.reading(cwd),
                host: None,
                elapsed_ms: measured.duration_ms,
                outcome,
            });
        }
    }
    Ok(runs)
}

fn shell_calls(
    connection: &Connection,
    query: &str,
    session: &[u8],
) -> Result<Vec<ShellCall>, SessionError> {
    let payloads: Vec<Vec<u8>> = connection
        .prepare_cached(query)?
        .query_map([session], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(decode_in_parallel(&payloads, shell_call))
}

fn shell_call(payload: &[u8]) -> Option<ShellCall> {
    let text = decompress_payload(payload, ITEM_FIELD).ok()?;
    if !text.contains(TOOL_CALL_MARKER) || !text.contains(SHELL_MARKER) {
        return None;
    }
    let call: HistoryCall = serde_json::from_str(&text).ok()?;
    if call.kind != TOOL_CALL || call.name != SHELL_TOOL {
        return None;
    }
    Some(ShellCall {
        call_id: call.call_id,
        command: call.input.command?,
        workdir: call.input.workdir,
        timeout_secs: call.input.timeout_secs,
    })
}

fn foreground_output(payload: &[u8]) -> Option<Measured> {
    let text = decompress_payload(payload, OUTPUT_FIELD).ok()?;
    serde_json::from_str::<ShellOutput>(&text).ok()?.shell
}

/// Background shell runs by the call that started them: their tool output is
/// only the admission receipt, and the task keeps what the run measured.
fn background_runs(connection: &Connection) -> Result<HashMap<CallKey, Measured>, SessionError> {
    let mut statement = connection.prepare("SELECT session_id, payload FROM background_tasks")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut runs = HashMap::new();
    for row in rows {
        let (session, payload) = row?;
        if !payload.contains(SHELL_MARKER) {
            continue;
        }
        let Ok(task) = serde_json::from_str::<BackgroundTask>(&payload) else {
            continue;
        };
        if task.payload.kind != SHELL_TOOL {
            continue;
        }
        if let Some(call_id) = task.payload.metadata.and_then(|metadata| metadata.call_id)
            && let Some(measured) = task
                .outcome
                .and_then(|outcome| outcome.shell)
                .and_then(|output| output.shell)
        {
            runs.insert((session, call_id), measured);
        }
    }
    Ok(runs)
}

/// History payloads dominate the migration's time, and each decodes on its own.
fn decode_in_parallel<T: Send>(rows: &[Vec<u8>], decode: fn(&[u8]) -> Option<T>) -> Vec<T> {
    let workers = available_parallelism()
        .map_or(1, NonZeroUsize::get)
        .min(rows.len().div_ceil(ROWS_PER_WORKER))
        .max(1);
    let chunk = rows.len().div_ceil(workers).max(1);
    thread::scope(|scope| {
        let handles: Vec<_> = rows
            .chunks(chunk)
            .map(|rows| scope.spawn(move || rows.iter().filter_map(|row| decode(row)).collect()))
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_else(|_| Vec::new()))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, params};
    use serde_json::{Value, json};

    use super::backfill;
    use crate::sessions::compress_payload;
    use crate::shell_runs::reading::{Call, Reading, decode, read};
    use crate::shell_runs::{RunOutcome, TABLE};

    const HISTORY_SCHEMA: &str = "
        CREATE TABLE sessions (id BLOB PRIMARY KEY, updated_at INTEGER NOT NULL, cwd TEXT NOT NULL);
        CREATE TABLE main_history_items (session_id BLOB, ordinal INTEGER, payload BLOB);
        CREATE TABLE subagent_history_items
            (session_id BLOB, subagent_id TEXT, ordinal INTEGER, payload BLOB);
        CREATE TABLE tool_outputs (session_id BLOB, tool_id TEXT, payload BLOB);
        CREATE TABLE background_tasks (session_id BLOB, payload TEXT);
    ";
    const OLDER: &[u8] = b"older-session-id";
    const NEWER: &[u8] = b"newer-session-id";
    const OLDER_AT: i64 = 100;
    const NEWER_AT: i64 = 200;
    const OLDER_CWD: &str = "/work/older";
    const NEWER_CWD: &str = "/work/newer";
    const QUICK_MS: u64 = 20;
    const BACKGROUND_MS: u64 = 40_000;
    const TIMED_OUT_MS: u64 = 120_000;
    const SUBAGENT_MS: u64 = 5_000;
    const READ_MS: u64 = 7;
    const SUBAGENT: &str = "toolu_task";
    const NOT_ZSTD: &[u8] = b"not a compressed payload";
    const STATUS: &str = "git status";
    const BUILD: &str = "cargo build --release";
    const BUILD_WORKDIR: &str = "/work/newer/crate";
    const BUILD_TIMEOUT_SECS: f64 = 600.0;
    const SLEEP: &str = "sleep 999";
    const TEST: &str = "cargo test";

    fn call(call_id: &str, name: &str, input: Value) -> Vec<u8> {
        let item = json!({"type": "tool_call", "name": name, "call_id": call_id, "input": input});
        compress_payload(&item.to_string()).unwrap()
    }

    fn shell(call_id: &str, command: &str) -> Vec<u8> {
        call(call_id, "shell", json!({"command": command}))
    }

    fn measured(duration_ms: u64, exit_code: Option<i64>, timed_out: bool) -> Value {
        json!({"Shell": {"duration_ms": duration_ms, "exit_code": exit_code, "signal": null, "timed_out": timed_out, "output_limit_exceeded": false}})
    }

    fn ms(elapsed_ms: u64) -> i64 {
        i64::try_from(elapsed_ms).unwrap()
    }

    fn in_session(command: &str, cwd: &str) -> Reading {
        read(&Call {
            command,
            workdir: None,
            cwd: Some(cwd),
            timeout_secs: None,
        })
    }

    fn history() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(HISTORY_SCHEMA).unwrap();
        connection.execute_batch(TABLE).unwrap();
        for (id, at, cwd) in [(OLDER, OLDER_AT, OLDER_CWD), (NEWER, NEWER_AT, NEWER_CWD)] {
            connection
                .execute(
                    "INSERT INTO sessions VALUES (?1, ?2, ?3)",
                    params![id, at, cwd],
                )
                .unwrap();
        }
        let build = json!({
            "command": BUILD,
            "workdir": BUILD_WORKDIR,
            "timeoutSec": BUILD_TIMEOUT_SECS,
        });
        let main = [
            (OLDER, shell("c1", STATUS)),
            (NEWER, call("c2", "shell", build)),
            (
                NEWER,
                call("c3", "file_read", json!({"command": "src/lib.rs"})),
            ),
            (NEWER, shell("c4", SLEEP)),
            (NEWER, shell("c5", "echo never finished")),
            (NEWER, NOT_ZSTD.to_vec()),
        ];
        for (ordinal, (session, payload)) in main.into_iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO main_history_items VALUES (?1, ?2, ?3)",
                    params![session, i64::try_from(ordinal).unwrap(), payload],
                )
                .unwrap();
        }
        connection
            .execute(
                "INSERT INTO subagent_history_items VALUES (?1, ?2, 0, ?3)",
                params![NEWER, SUBAGENT, shell("c6", TEST)],
            )
            .unwrap();
        let outputs = [
            (OLDER, "c1", measured(QUICK_MS, Some(0), false)),
            (NEWER, "c2", json!({"Tasks": []})),
            (NEWER, "c3", measured(READ_MS, Some(0), false)),
            (NEWER, "c4", measured(TIMED_OUT_MS, None, true)),
            (NEWER, "c6", measured(SUBAGENT_MS, Some(1), false)),
        ];
        for (session, call_id, output) in outputs {
            connection
                .execute(
                    "INSERT INTO tool_outputs VALUES (?1, ?2, ?3)",
                    params![
                        session,
                        call_id,
                        compress_payload(&output.to_string()).unwrap()
                    ],
                )
                .unwrap();
        }
        let task = json!({
            "payload": {"kind": "shell", "metadata": {"call_id": "c2"}},
            "outcome": {"shell": measured(BACKGROUND_MS, Some(0), false)},
        });
        connection
            .execute(
                "INSERT INTO background_tasks VALUES (?1, ?2)",
                params![NEWER, task.to_string()],
            )
            .unwrap();
        connection
    }

    #[test]
    fn shell_calls_are_imported_oldest_session_first() {
        let connection = history();

        assert_eq!(backfill(&connection).unwrap(), 4);

        let stored: Vec<(i64, Vec<u8>, i64, i64)> = connection
            .prepare("SELECT recorded_at, reading, elapsed_ms, outcome FROM shell_runs ORDER BY id")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let stored: Vec<(i64, Reading, i64, i64)> = stored
            .into_iter()
            .map(|(at, reading, elapsed, outcome)| {
                (at, decode(&reading).unwrap(), elapsed, outcome)
            })
            .collect();
        let build = read(&Call {
            command: BUILD,
            workdir: Some(BUILD_WORKDIR),
            cwd: Some(NEWER_CWD),
            timeout_secs: Some(BUILD_TIMEOUT_SECS),
        });
        let completed = RunOutcome::Completed.code();
        assert_eq!(
            stored,
            [
                (
                    OLDER_AT,
                    in_session(STATUS, OLDER_CWD),
                    ms(QUICK_MS),
                    completed
                ),
                (NEWER_AT, build, ms(BACKGROUND_MS), completed),
                (
                    NEWER_AT,
                    in_session(SLEEP, NEWER_CWD),
                    ms(TIMED_OUT_MS),
                    RunOutcome::TimedOut.code()
                ),
                (
                    NEWER_AT,
                    in_session(TEST, NEWER_CWD),
                    ms(SUBAGENT_MS),
                    completed
                ),
            ]
        );
    }
}
