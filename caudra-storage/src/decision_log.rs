use crate::sessions::{SESSIONS_DB_FILE, SessionDatabase, SessionError};
use crate::{StateClass, StateDir, StorageError, now_epoch};
use rusqlite::{OptionalExtension, Row, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const SECONDS_PER_DAY: u64 = 86_400;
const MAX_RECORD_BYTES: usize = 1_048_576;
const MAX_QUESTIONS: usize = 64;
const DEFAULT_NOUL_THRESHOLD: f64 = 0.5;
const DEFAULT_SCORE_TOLERANCE: f64 = 0.1;
/// Binds a [`DecisionFilter`] as parameters 1 to 3; an unset field matches every row.
const FILTER: &str = "(?1 IS NULL OR feature = ?1) AND (?2 IS NULL OR session = ?2) \
    AND (?3 IS NULL OR project = ?3)";
/// Every stored column, in the order [`logged_decision`] reads them.
const ROW_COLUMNS: &str = "id, ts, session, project, feature, question_set_id, \
    question_set_version, endpoint_kind, model, state, questions, answers, error, latency_ms, \
    mode, effect, meta, label, label_source, label_ts, label_meta";
pub(crate) const SCHEMA: &str = "
CREATE TABLE decisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts INTEGER NOT NULL,
    session TEXT,
    project TEXT,
    feature TEXT NOT NULL,
    question_set_id TEXT NOT NULL,
    question_set_version TEXT NOT NULL,
    endpoint_kind TEXT NOT NULL,
    model TEXT NOT NULL,
    state TEXT NOT NULL,
    questions TEXT NOT NULL,
    answers TEXT,
    error TEXT,
    latency_ms INTEGER NOT NULL,
    mode TEXT NOT NULL,
    effect TEXT NOT NULL,
    meta TEXT NOT NULL,
    label TEXT,
    label_source TEXT,
    label_ts INTEGER,
    label_meta TEXT
);
CREATE INDEX decisions_ts ON decisions(ts);
CREATE INDEX decisions_feature ON decisions(feature);
";

#[derive(Debug, thiserror::Error)]
pub enum DecisionLogError {
    #[error("decision log storage operation failed: {0}")]
    Session(#[from] SessionError),
    #[error("decision log I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("decision log database operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("decision log JSON encoding failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid decision log field: {0}")]
    Invalid(&'static str),
    #[error("decision log row not found: {0}")]
    NotFound(i64),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    Local,
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionEffect {
    None,
    Advised,
    Escalated,
    Rerouted,
    Skipped,
}

/// All payloads, including errors and metadata, must already be redacted and bounded.
/// Storage preserves the state and full question definitions actually sent to the engine.
#[derive(Clone, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub timestamp: u64,
    pub session: Option<String>,
    pub project: Option<String>,
    pub feature: String,
    pub question_set_id: String,
    pub question_set_version: String,
    pub endpoint_kind: EndpointKind,
    pub model: String,
    pub state: Value,
    pub questions: Value,
    /// The response's answers object, keyed by question id, not the response envelope.
    pub answers: Option<Value>,
    pub error: Option<String>,
    pub latency_ms: u64,
    pub mode: String,
    pub effect: DecisionEffect,
    pub meta: Value,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DecisionLabel {
    /// Ground truth keyed by original question id: bool, choice string, or score number.
    pub expected: Value,
    pub source: String,
    pub timestamp: u64,
    pub meta: Value,
}

#[derive(Debug, Clone)]
pub struct StatsThresholds {
    pub default_noul: f64,
    /// Question-id overrides; use a feature filter when features use different thresholds.
    pub noul_by_question: BTreeMap<String, f64>,
    pub score_tolerance: f64,
}

impl Default for StatsThresholds {
    fn default() -> Self {
        Self {
            default_noul: DEFAULT_NOUL_THRESHOLD,
            noul_by_question: BTreeMap::new(),
            score_tolerance: DEFAULT_SCORE_TOLERANCE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DecisionStats {
    pub feature: String,
    pub count: u64,
    pub error_count: u64,
    pub error_rate: f64,
    /// Rows whose recorded effect is anything but `none`. Effects are a partial
    /// record, so this is a floor on what the engine changed.
    pub acted_count: u64,
    pub labelled_count: u64,
    pub latency_p50_ms: u64,
    pub latency_p95_ms: u64,
    pub compared_labels: u64,
    pub agreeing_labels: u64,
    pub agreement_rate: Option<f64>,
    /// Epoch seconds of the newest row.
    pub last_timestamp: u64,
}

/// Narrows a query to one feature, session or project. An unset field matches
/// every row, so the default filter reads the whole log.
#[derive(Debug, Clone, Default)]
pub struct DecisionFilter<'a> {
    pub feature: Option<&'a str>,
    pub session: Option<&'a str>,
    pub project: Option<&'a str>,
}

/// One stored row, with its label when it has one.
#[derive(Clone, Serialize)]
pub struct LoggedDecision {
    pub id: i64,
    #[serde(flatten)]
    pub record: DecisionRecord,
    pub label: Option<DecisionLabel>,
}

pub struct DecisionLog {
    database: SessionDatabase,
    path: PathBuf,
}

impl DecisionLog {
    /// Where the log lives in `state_dir`, whether or not it exists.
    pub fn file_path(state_dir: &StateDir) -> PathBuf {
        state_dir.persistent_path().join(SESSIONS_DB_FILE)
    }

    /// Disabled logging does no filesystem I/O. Retention uses row creation time in epoch seconds.
    pub fn open(
        state_dir: &StateDir,
        enabled: bool,
        retention_days: u64,
    ) -> Result<Option<Self>, DecisionLogError> {
        if !enabled {
            return Ok(None);
        }
        let database = SessionDatabase::open_state(&state_dir.for_class(StateClass::Persistent))?;
        let log = Self {
            path: database.path(),
            database,
        };
        log.prune_before(
            now_epoch().saturating_sub(retention_days.saturating_mul(SECONDS_PER_DAY)),
        )?;
        Ok(Some(log))
    }

    /// Maintenance access never creates a database or implicitly enables logging.
    pub fn open_existing(state_dir: &StateDir) -> Result<Option<Self>, DecisionLogError> {
        Ok(
            SessionDatabase::open_existing_state(&state_dir.for_class(StateClass::Persistent))?
                .map(|database| Self {
                    path: database.path(),
                    database,
                }),
        )
    }

    pub fn open_read_only(state_dir: &StateDir) -> Result<Option<Self>, DecisionLogError> {
        match SessionDatabase::open_read_only(&state_dir.for_class(StateClass::Persistent)) {
            Ok(database) => Ok(Some(Self {
                path: database.path(),
                database,
            })),
            Err(SessionError::Storage(StorageError::NotFound(_))) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn insert(&self, record: &DecisionRecord) -> Result<i64, DecisionLogError> {
        bounded_json(record)?;
        let questions = question_map(&record.questions)?;
        if questions.is_empty() || questions.len() > MAX_QUESTIONS {
            return Err(DecisionLogError::Invalid("questions"));
        }
        for (id, question) in questions {
            if id.is_empty()
                || !matches!(question["type"].as_str(), Some("noul" | "choice" | "score"))
            {
                return Err(DecisionLogError::Invalid("questions"));
            }
        }
        if record
            .answers
            .as_ref()
            .is_some_and(|answers| !answers.is_object())
        {
            return Err(DecisionLogError::Invalid("answers"));
        }
        for value in [
            &record.feature,
            &record.question_set_id,
            &record.question_set_version,
            &record.model,
            &record.mode,
        ] {
            if value.is_empty() {
                return Err(DecisionLogError::Invalid("identity"));
            }
        }
        self.database.connection().execute(
            "INSERT INTO decisions (ts, session, project, feature, question_set_id,
                question_set_version, endpoint_kind, model, state, questions, answers,
                error, latency_ms, mode, effect, meta)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                sql_u64(record.timestamp)?,
                record.session,
                record.project,
                record.feature,
                record.question_set_id,
                record.question_set_version,
                serde_json::to_value(&record.endpoint_kind)?.as_str(),
                record.model,
                serde_json::to_string(&record.state)?,
                serde_json::to_string(&record.questions)?,
                record
                    .answers
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                record.error,
                sql_u64(record.latency_ms)?,
                record.mode,
                serde_json::to_value(&record.effect)?.as_str(),
                serde_json::to_string(&record.meta)?,
            ],
        )?;
        Ok(self.database.connection().last_insert_rowid())
    }

    pub fn update_effect(&self, id: i64, effect: DecisionEffect) -> Result<(), DecisionLogError> {
        let changed = self.database.connection().execute(
            "UPDATE decisions SET effect = CASE WHEN effect = 'escalated' AND ?2 = 'advised' THEN effect ELSE ?2 END WHERE id = ?1",
            params![id, serde_json::to_value(effect)?.as_str()],
        )?;
        if changed == 0 {
            return Err(DecisionLogError::NotFound(id));
        }
        Ok(())
    }

    /// Replaces a row's label atomically; unknown ids and mismatched question ids are rejected.
    pub fn attach_label(&mut self, id: i64, label: &DecisionLabel) -> Result<(), DecisionLogError> {
        bounded_json(label)?;
        if label.source.is_empty() {
            return Err(DecisionLogError::Invalid("label_source"));
        }
        let transaction = self
            .database
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let questions: String = transaction
            .query_row(
                "SELECT questions FROM decisions WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(DecisionLogError::NotFound(id))?;
        validate_label(&serde_json::from_str(&questions)?, &label.expected)?;
        transaction.execute(
            "UPDATE decisions SET label = ?2, label_source = ?3, label_ts = ?4, label_meta = ?5 WHERE id = ?1",
            params![id, serde_json::to_string(&label.expected)?, label.source,
                sql_u64(label.timestamp)?, serde_json::to_string(&label.meta)?],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Exports only labelled questions, preserving their complete original definitions.
    /// Engine failures remain exportable; predictions are never substituted for ground truth.
    pub fn export_jsonl(
        &self,
        mut writer: impl Write,
        feature: Option<&str>,
    ) -> Result<u64, DecisionLogError> {
        let mut statement = self.database.connection().prepare(&format!(
            "SELECT {ROW_COLUMNS} FROM decisions
             WHERE label IS NOT NULL AND (?1 IS NULL OR feature = ?1) ORDER BY id"
        ))?;
        let mut rows = statement.query([feature])?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            let LoggedDecision { id, record, label } = logged_decision(row)?;
            let label = label.ok_or(DecisionLogError::Invalid("label"))?;
            validate_label(&record.questions, &label.expected)?;
            let selected: Map<String, Value> = question_map(&label.expected)?
                .keys()
                .map(|id| (id.clone(), record.questions[id].clone()))
                .collect();
            let example = json!({
                "state": record.state,
                "questions": selected,
                "expected": label.expected,
                "model": record.model,
                "tags": ["caudra", format!("feature:{}", record.feature),
                    format!("qset:{}@{}", record.question_set_id, record.question_set_version),
                    format!("label:{}", label.source)],
                "caudra": {
                    "id": id, "timestamp": record.timestamp,
                    "session": record.session, "project": record.project,
                    "endpoint_kind": record.endpoint_kind, "answers": record.answers,
                    "error": record.error, "latency_ms": record.latency_ms,
                    "mode": record.mode, "effect": record.effect,
                    "meta": record.meta, "label_timestamp": label.timestamp,
                    "label_meta": label.meta,
                },
            });
            serde_json::to_writer(&mut writer, &example)?;
            writer.write_all(b"\n")?;
            count += 1;
        }
        Ok(count)
    }

    /// Nearest-rank latency percentiles and per-answer agreement; missing answers are not votes.
    /// `thresholds` is asked once for each feature the filter leaves, because
    /// features read the same question id against different thresholds.
    pub fn stats(
        &self,
        filter: &DecisionFilter<'_>,
        thresholds: impl Fn(&str) -> StatsThresholds,
    ) -> Result<Vec<DecisionStats>, DecisionLogError> {
        let mut statement = self.database.connection().prepare(&format!(
            "WITH ranked AS (
                SELECT feature, error, label, latency_ms, effect, ts,
                    ROW_NUMBER() OVER (PARTITION BY feature ORDER BY latency_ms) AS rank,
                    COUNT(*) OVER (PARTITION BY feature) AS n
                FROM decisions WHERE {FILTER}
             ) SELECT feature, COUNT(*), SUM(error IS NOT NULL), SUM(label IS NOT NULL),
                MAX(CASE WHEN rank = (n + 1) / 2 THEN latency_ms END),
                MAX(CASE WHEN rank = (n * 95 + 99) / 100 THEN latency_ms END),
                SUM(effect <> 'none'), MAX(ts)
             FROM ranked GROUP BY feature ORDER BY feature"
        ))?;
        let mut stats = BTreeMap::new();
        let mut rows = statement.query(filter_params(filter))?;
        while let Some(row) = rows.next()? {
            let feature: String = row.get(0)?;
            let feature_thresholds = thresholds(&feature);
            validate_thresholds(&feature_thresholds)?;
            let count = row_u64(row, 1)?;
            let error_count = row_u64(row, 2)?;
            stats.insert(
                feature.clone(),
                (
                    DecisionStats {
                        feature,
                        count,
                        error_count,
                        error_rate: error_count as f64 / count as f64,
                        acted_count: row_u64(row, 6)?,
                        labelled_count: row_u64(row, 3)?,
                        latency_p50_ms: row_u64(row, 4)?,
                        latency_p95_ms: row_u64(row, 5)?,
                        compared_labels: 0,
                        agreeing_labels: 0,
                        agreement_rate: None,
                        last_timestamp: row_u64(row, 7)?,
                    },
                    feature_thresholds,
                ),
            );
        }
        let mut statement = self.database.connection().prepare(&format!(
            "SELECT feature, questions, answers, label FROM decisions
             WHERE label IS NOT NULL AND answers IS NOT NULL AND error IS NULL AND {FILTER}"
        ))?;
        let mut rows = statement.query(filter_params(filter))?;
        while let Some(row) = rows.next()? {
            let feature: String = row.get(0)?;
            let questions = row_json(row, 1)?;
            let answers = row_json(row, 2)?;
            let expected = row_json(row, 3)?;
            if let Some((stats, thresholds)) = stats.get_mut(&feature) {
                for (id, label) in question_map(&expected)? {
                    if let Some(agrees) =
                        answer_agrees(id, &questions[id], &answers[id], label, thresholds)
                    {
                        stats.compared_labels += 1;
                        stats.agreeing_labels += u64::from(agrees);
                    }
                }
            }
        }
        Ok(stats
            .into_values()
            .map(|(mut stats, _)| {
                if stats.compared_labels > 0 {
                    stats.agreement_rate =
                        Some(stats.agreeing_labels as f64 / stats.compared_labels as f64);
                }
                stats
            })
            .collect())
    }

    /// The newest rows the filter leaves, newest first and at most `limit` of them.
    pub fn recent(
        &self,
        filter: &DecisionFilter<'_>,
        limit: usize,
    ) -> Result<Vec<LoggedDecision>, DecisionLogError> {
        let mut statement = self.database.connection().prepare(&format!(
            "SELECT {ROW_COLUMNS} FROM decisions WHERE {FILTER} ORDER BY id DESC LIMIT ?4"
        ))?;
        let limit = i64::try_from(limit).map_err(|_| DecisionLogError::Invalid("limit"))?;
        let mut rows = statement.query(params![
            filter.feature,
            filter.session,
            filter.project,
            limit
        ])?;
        let mut decisions = Vec::new();
        while let Some(row) = rows.next()? {
            decisions.push(logged_decision(row)?);
        }
        Ok(decisions)
    }

    /// Removes rows strictly older than the cutoff, including attached labels.
    pub fn prune_before(&self, cutoff: u64) -> Result<usize, DecisionLogError> {
        Ok(self
            .database
            .connection()
            .execute("DELETE FROM decisions WHERE ts < ?1", [sql_u64(cutoff)?])?)
    }

    pub fn purge(&self) -> Result<usize, DecisionLogError> {
        Ok(self
            .database
            .connection()
            .execute("DELETE FROM decisions", [])?)
    }
}

fn filter_params<'a>(
    filter: &DecisionFilter<'a>,
) -> (Option<&'a str>, Option<&'a str>, Option<&'a str>) {
    (filter.feature, filter.session, filter.project)
}

fn validate_thresholds(thresholds: &StatsThresholds) -> Result<(), DecisionLogError> {
    if !valid_probability(thresholds.default_noul)
        || thresholds
            .noul_by_question
            .values()
            .any(|value| !valid_probability(*value))
        || !thresholds.score_tolerance.is_finite()
        || thresholds.score_tolerance < 0.0
    {
        return Err(DecisionLogError::Invalid("thresholds"));
    }
    Ok(())
}

/// Reads a row selected with [`ROW_COLUMNS`].
fn logged_decision(row: &Row<'_>) -> Result<LoggedDecision, DecisionLogError> {
    let label = row
        .get::<_, Option<String>>(17)?
        .map(|expected| {
            Ok::<_, DecisionLogError>(DecisionLabel {
                expected: serde_json::from_str(&expected)?,
                source: row.get(18)?,
                timestamp: row_u64(row, 19)?,
                meta: row_json(row, 20)?,
            })
        })
        .transpose()?;
    Ok(LoggedDecision {
        id: row.get(0)?,
        record: DecisionRecord {
            timestamp: row_u64(row, 1)?,
            session: row.get(2)?,
            project: row.get(3)?,
            feature: row.get(4)?,
            question_set_id: row.get(5)?,
            question_set_version: row.get(6)?,
            endpoint_kind: row_variant(row, 7)?,
            model: row.get(8)?,
            state: row_json(row, 9)?,
            questions: row_json(row, 10)?,
            answers: row
                .get::<_, Option<String>>(11)?
                .map(|answers| serde_json::from_str(&answers))
                .transpose()?,
            error: row.get(12)?,
            latency_ms: row_u64(row, 13)?,
            mode: row.get(14)?,
            effect: row_variant(row, 15)?,
            meta: row_json(row, 16)?,
        },
        label,
    })
}

/// A unit enum stored as its serde name.
fn row_variant<T: DeserializeOwned>(row: &Row<'_>, column: usize) -> Result<T, DecisionLogError> {
    Ok(serde_json::from_value(Value::String(row.get(column)?))?)
}

fn bounded_json(value: &impl Serialize) -> Result<(), DecisionLogError> {
    if serde_json::to_vec(value)?.len() > MAX_RECORD_BYTES {
        return Err(DecisionLogError::Invalid("record_size"));
    }
    Ok(())
}

fn sql_u64(value: u64) -> Result<i64, DecisionLogError> {
    i64::try_from(value).map_err(|_| DecisionLogError::Invalid("integer_range"))
}

fn row_u64(row: &Row<'_>, column: usize) -> Result<u64, DecisionLogError> {
    u64::try_from(row.get::<_, i64>(column)?)
        .map_err(|_| DecisionLogError::Invalid("integer_range"))
}

fn question_map(value: &Value) -> Result<&Map<String, Value>, DecisionLogError> {
    value
        .as_object()
        .ok_or(DecisionLogError::Invalid("question_map"))
}

fn validate_label(questions: &Value, expected: &Value) -> Result<(), DecisionLogError> {
    let questions = question_map(questions)?;
    let labels = question_map(expected)?;
    if labels.is_empty() {
        return Err(DecisionLogError::Invalid("expected"));
    }
    for (id, label) in labels {
        let question = questions
            .get(id)
            .ok_or(DecisionLogError::Invalid("label_question_id"))?;
        let valid = match question["type"].as_str() {
            Some("noul") => label.is_boolean(),
            Some("choice") => label.is_string(),
            Some("score") => label.is_number(),
            _ => false,
        };
        if !valid {
            return Err(DecisionLogError::Invalid("label_type"));
        }
    }
    Ok(())
}

fn row_json(row: &Row<'_>, column: usize) -> Result<Value, DecisionLogError> {
    row.get::<_, Option<String>>(column)?
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .map(|value| value.unwrap_or(Value::Null))
        .map_err(Into::into)
}

fn valid_probability(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn answer_agrees(
    id: &str,
    question: &Value,
    answer: &Value,
    expected: &Value,
    thresholds: &StatsThresholds,
) -> Option<bool> {
    if answer["type"] != question["type"] {
        return None;
    }
    match question["type"].as_str()? {
        "noul" => {
            let probability = answer["noul"]
                .as_f64()
                .filter(|value| valid_probability(*value))?;
            let threshold = thresholds
                .noul_by_question
                .get(id)
                .copied()
                .unwrap_or(thresholds.default_noul);
            Some((probability >= threshold) == expected.as_bool()?)
        }
        "choice" => Some(answer["choice"].as_str()? == expected.as_str()?),
        "score" => Some(
            (answer["score"].as_f64()? - expected.as_f64()?).abs() <= thresholds.score_tolerance,
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DecisionEffect, DecisionFilter, DecisionLabel, DecisionLog, DecisionLogError,
        DecisionRecord, DecisionStats, EndpointKind, MAX_RECORD_BYTES, SECONDS_PER_DAY,
        StatsThresholds,
    };
    use crate::id::CaudraId;
    use crate::messages::{
        HistoryChannel, MessageAudience, MessageLog, MessageRecipient, MessageSender, NewMessage,
        Retention,
    };
    use crate::sessions::{SESSIONS_DB_FILE, SessionError};
    use crate::{StateDir, now_epoch};
    use rusqlite::{Connection, params};
    use serde_json::{Value, json};
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
    use tempfile::{TempDir, tempdir};
    use test_case::test_case;

    const TIMESTAMP: u64 = 1_000_000;
    const RETENTION_DAYS: u64 = 90;
    const ENGINE_ERROR: &str = "engine timeout";
    const PAYLOAD_MARKER: &str = "decision-payload-marker-for-purge-test";
    const SESSION_A: &str = "session-a";
    const SESSION_B: &str = "session-b";
    const PROJECT_A: &str = "project-a";
    const PROJECT_B: &str = "project-b";
    const PERMISSION: &str = "permission";
    const CONTENT: &str = "content";
    const ROW_LIMIT: usize = 10;
    const STATE_KEY: &str = "decision-purge-preserved";
    const STATE_VALUE: &str = "main database state";
    const SESSION_ID: [u8; 16] = [1; 16];
    const MESSAGE_ID: &str = "coexisting-message";

    fn record() -> DecisionRecord {
        DecisionRecord {
            timestamp: TIMESTAMP,
            session: Some(SESSION_A.into()),
            project: Some(PROJECT_A.into()),
            feature: PERMISSION.into(),
            question_set_id: "permission.v1".into(),
            question_set_version: "content-hash-a".into(),
            endpoint_kind: EndpointKind::Local,
            model: "english".into(),
            state: json!({"command": "echo [REDACTED]"}),
            questions: json!({
                "user_approves": {
                    "type": "noul", "instructions": {"task": "Would the user approve?"},
                    "criteria": {
                        "true": ["consistent with the user's request"],
                        "false": "contrary to the user's request"
                    }
                },
                "uploads": {"type": "noul", "instructions": "Sends data to a remote service"}
            }),
            answers: Some(json!({
                "user_approves": {"type": "noul", "noul": 0.7},
                "uploads": {"type": "noul", "noul": 0.1}
            })),
            error: None,
            latency_ms: 45,
            mode: "shadow".into(),
            effect: DecisionEffect::None,
            meta: json!({"auto_eligible": true}),
        }
    }

    fn label(expected: Value) -> DecisionLabel {
        DecisionLabel {
            expected,
            source: "user".into(),
            timestamp: TIMESTAMP + 1,
            meta: json!({"lifetime": "once"}),
        }
    }

    fn fixture() -> (TempDir, StateDir, DecisionLog) {
        let root = tempdir().unwrap();
        let state = StateDir::from_path(root.path().to_path_buf());
        let log = DecisionLog::open(&state, true, RETENTION_DAYS)
            .unwrap()
            .unwrap();
        (root, state, log)
    }

    fn all_stats(log: &DecisionLog) -> Vec<DecisionStats> {
        log.stats(&DecisionFilter::default(), |_| StatsThresholds::default())
            .unwrap()
    }

    fn ids(log: &DecisionLog, filter: &DecisionFilter, limit: usize) -> Vec<i64> {
        log.recent(filter, limit)
            .unwrap()
            .into_iter()
            .map(|decision| decision.id)
            .collect()
    }

    /// Rows across two sessions, two projects and two features, in id order:
    /// A/A/permission, B/A/permission, B/B/content.
    fn scoped_rows(log: &DecisionLog) -> [i64; 3] {
        let mut record = record();
        let first = log.insert(&record).unwrap();
        record.session = Some(SESSION_B.into());
        let second = log.insert(&record).unwrap();
        record.project = Some(PROJECT_B.into());
        record.feature = CONTENT.into();
        let third = log.insert(&record).unwrap();
        [first, second, third]
    }

    #[test_case(DecisionFilter::default(), &[2, 1, 0]; "all")]
    #[test_case(DecisionFilter { session: Some(SESSION_B), ..Default::default() }, &[2, 1]; "session")]
    #[test_case(DecisionFilter { project: Some(PROJECT_A), ..Default::default() }, &[1, 0]; "project")]
    #[test_case(DecisionFilter { feature: Some(CONTENT), ..Default::default() }, &[2]; "feature")]
    #[test_case(DecisionFilter { session: Some(SESSION_A), project: Some(PROJECT_B), ..Default::default() }, &[]; "disjoint")]
    fn stats_and_recent_honour_scope_filters(filter: DecisionFilter, expected: &[usize]) {
        let (_root, _state, log) = fixture();
        let rows = scoped_rows(&log);
        let expected_ids: Vec<i64> = expected.iter().map(|index| rows[*index]).collect();
        assert_eq!(ids(&log, &filter, ROW_LIMIT), expected_ids);
        let counted: u64 = log
            .stats(&filter, |_| StatsThresholds::default())
            .unwrap()
            .iter()
            .map(|stats| stats.count)
            .sum();
        assert_eq!(counted, expected.len() as u64);
    }

    #[test]
    fn stats_count_acted_rows_and_last_seen() {
        let (_root, _state, log) = fixture();
        let mut record = record();
        let advised = log.insert(&record).unwrap();
        record.timestamp += 5;
        let escalated = log.insert(&record).unwrap();
        record.timestamp -= 3;
        log.insert(&record).unwrap();
        log.update_effect(advised, DecisionEffect::Advised).unwrap();
        log.update_effect(escalated, DecisionEffect::Escalated)
            .unwrap();
        let stats = all_stats(&log);
        assert_eq!(stats[0].count, 3);
        assert_eq!(stats[0].acted_count, 2);
        assert_eq!(stats[0].last_timestamp, TIMESTAMP + 5);
    }

    #[test]
    fn stats_apply_each_features_thresholds() {
        let (_root, _state, mut log) = fixture();
        let mut record = record();
        for feature in [PERMISSION, CONTENT] {
            record.feature = feature.into();
            let id = log.insert(&record).unwrap();
            log.attach_label(id, &label(json!({"user_approves": true})))
                .unwrap();
        }
        let stats = log
            .stats(&DecisionFilter::default(), |feature| {
                let mut thresholds = StatsThresholds::default();
                if feature == CONTENT {
                    thresholds
                        .noul_by_question
                        .insert("user_approves".into(), 0.9);
                }
                thresholds
            })
            .unwrap();
        let agreement = |feature: &str| {
            stats
                .iter()
                .find(|stats| stats.feature == feature)
                .unwrap()
                .agreement_rate
        };
        assert_eq!(agreement(PERMISSION), Some(1.0));
        assert_eq!(agreement(CONTENT), Some(0.0));
    }

    #[test]
    fn recent_is_newest_first_bounded_and_round_trips_labels() {
        let (_root, _state, mut log) = fixture();
        let original = record();
        let first = log.insert(&original).unwrap();
        let second = log.insert(&original).unwrap();
        let newest = log.insert(&original).unwrap();
        let expected = label(json!({"user_approves": false}));
        log.attach_label(second, &expected).unwrap();
        log.update_effect(second, DecisionEffect::Rerouted).unwrap();
        assert_eq!(
            ids(&log, &DecisionFilter::default(), 2),
            vec![newest, second]
        );
        let all = log.recent(&DecisionFilter::default(), ROW_LIMIT).unwrap();
        assert_eq!(all.last().unwrap().id, first);
        let labelled = all.iter().find(|decision| decision.id == second).unwrap();
        let mut stored = original.clone();
        stored.effect = DecisionEffect::Rerouted;
        assert_eq!(
            serde_json::to_value(&labelled.record).unwrap(),
            serde_json::to_value(&stored).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&labelled.label).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert!(all[0].label.is_none());
    }

    #[test]
    fn read_only_open_never_creates_or_writes() {
        let root = tempdir().unwrap();
        let absent = StateDir::from_path(root.path().join("absent"));
        assert!(DecisionLog::open_read_only(&absent).unwrap().is_none());
        assert!(!absent.path().exists());
        let (_root, state, log) = fixture();
        log.insert(&record()).unwrap();
        drop(log);
        let reader = DecisionLog::open_read_only(&state).unwrap().unwrap();
        assert_eq!(all_stats(&reader)[0].count, 1);
        assert!(reader.insert(&record()).is_err());
        assert!(reader.purge().is_err());
        assert_eq!(all_stats(&reader)[0].count, 1);
    }

    #[cfg(unix)]
    #[test]
    fn read_only_open_refuses_an_uninitialized_database_without_writing() {
        let root = tempdir().unwrap();
        let state = StateDir::from_path(root.path().to_path_buf());
        fs::create_dir_all(state.persistent_path()).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(state.persistent_path().join(SESSIONS_DB_FILE))
            .unwrap();
        assert!(matches!(
            DecisionLog::open_read_only(&state),
            Err(DecisionLogError::Session(_))
        ));
        assert!(fs::read(DecisionLog::file_path(&state)).unwrap().is_empty());
        assert_eq!(fs::read_dir(state.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn existing_open_refuses_an_uninitialized_database_without_initializing() {
        let root = tempdir().unwrap();
        let state = StateDir::from_path(root.path().to_path_buf());
        let path = state.path().join(SESSIONS_DB_FILE);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        assert!(matches!(
            DecisionLog::open_existing(&state),
            Err(DecisionLogError::Session(_))
        ));
        assert!(fs::read(path).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn read_only_open_rejects_a_foreign_schema() {
        let root = tempdir().unwrap();
        let state = StateDir::from_path(root.path().to_path_buf());
        fs::create_dir_all(state.persistent_path()).unwrap();
        let path = state.persistent_path().join(SESSIONS_DB_FILE);
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE foreign_rows (id INTEGER)")
            .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            DecisionLog::open_read_only(&state),
            Err(DecisionLogError::Session(_))
        ));
    }

    fn export(log: &DecisionLog, feature: Option<&str>) -> Vec<Value> {
        let mut output = Vec::new();
        let count = log.export_jsonl(&mut output, feature).unwrap();
        let lines: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(count as usize, lines.len());
        lines
    }

    #[test]
    fn applied_effect_updates_only_its_original_row() {
        let (_root, _state, mut log) = fixture();
        let id = log.insert(&record()).unwrap();
        log.attach_label(id, &label(json!({"user_approves": true})))
            .unwrap();
        log.update_effect(id, DecisionEffect::Advised).unwrap();
        assert_eq!(export(&log, None)[0]["caudra"]["effect"], "advised");
        log.update_effect(id, DecisionEffect::Escalated).unwrap();
        log.update_effect(id, DecisionEffect::Advised).unwrap();
        assert_eq!(export(&log, None)[0]["caudra"]["effect"], "escalated");
        log.purge().unwrap();
        let replacement = log.insert(&record()).unwrap();
        assert!(replacement > id);
        assert!(
            matches!(log.update_effect(id, DecisionEffect::Escalated), Err(DecisionLogError::NotFound(missing)) if missing == id)
        );
    }

    #[test]
    fn disabled_and_missing_maintenance_access_do_not_create_state() {
        let root = tempdir().unwrap();
        let state = StateDir::from_path(root.path().join("absent"));
        assert!(
            DecisionLog::open(&state, false, RETENTION_DAYS)
                .unwrap()
                .is_none()
        );
        assert!(DecisionLog::open_existing(&state).unwrap().is_none());
        assert!(!state.path().exists());
    }

    #[test]
    fn decision_database_lives_only_in_persistent_root() {
        let root = tempdir().unwrap();
        let state = StateDir::split(root.path().join("volatile"), root.path().join("persistent"));
        let log = DecisionLog::open(&state, true, RETENTION_DAYS)
            .unwrap()
            .unwrap();
        assert_eq!(log.path(), state.persistent_path().join(SESSIONS_DB_FILE));
        assert!(!state.path().exists());
        log.insert(&record()).unwrap();
        let existing = DecisionLog::open_existing(&state).unwrap().unwrap();
        assert_eq!(all_stats(&existing)[0].count, 1);
        let reader = DecisionLog::open_read_only(&state).unwrap().unwrap();
        assert_eq!(all_stats(&reader)[0].count, 1);
        assert!(!state.path().exists());
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(log.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test_case(true; "messages_first")]
    #[test_case(false; "decisions_first")]
    fn repositories_coexist_and_purge_preserves_sessions_state_and_messages(messages_first: bool) {
        let root = tempdir().unwrap();
        let state = StateDir::from_path(root.path().to_path_buf());
        let retention = Retention {
            days: u64::MAX,
            max_messages: u64::MAX,
        };
        let early_messages =
            messages_first.then(|| MessageLog::open(&state, &retention, TIMESTAMP).unwrap());
        let mut log = DecisionLog::open(&state, true, RETENTION_DAYS)
            .unwrap()
            .unwrap();
        let mut messages = early_messages
            .unwrap_or_else(|| MessageLog::open(&state, &retention, TIMESTAMP).unwrap());
        assert_eq!(log.path(), MessageLog::file_path(&state));
        let session = CaudraId::from_bytes(SESSION_ID).to_string();
        let mut stored_record = record();
        stored_record.session = Some(session.clone());

        log.database
            .global_state_set(STATE_KEY, &STATE_VALUE)
            .unwrap();
        log.database.connection().execute(
            "INSERT INTO sessions (id, format_version, title, cwd, model, created_at, updated_at,
                token_usage, metadata) VALUES (?1, 1, ?2, ?3, ?2, ?4, ?4, '{}', '{}')",
            params![SESSION_ID.as_slice(), STATE_VALUE, PROJECT_A, i64::try_from(TIMESTAMP).unwrap()],
        ).unwrap();
        let seq = messages
            .record(
                &NewMessage {
                    message_id: MESSAGE_ID.into(),
                    audience: MessageAudience::Topic(CONTENT.into()),
                    sender: MessageSender {
                        route: SESSION_A.into(),
                        session,
                        name: SESSION_A.into(),
                        handle: None,
                        cwd: Some(PROJECT_A.into()),
                        mode: "build".into(),
                        permission: "ask".into(),
                        external: false,
                    },
                    text: STATE_VALUE.into(),
                    reply_to: None,
                    created_ms: TIMESTAMP,
                },
                &[MessageRecipient {
                    session: SESSION_B.into(),
                    name: None,
                    handle: None,
                }],
            )
            .unwrap();
        messages
            .mark_seen(SESSION_B, SESSION_A, MESSAGE_ID)
            .unwrap();
        let id = log.insert(&stored_record).unwrap();
        log.attach_label(id, &label(json!({"user_approves": true})))
            .unwrap();
        let schema_version: i64 = log
            .database
            .connection()
            .pragma_query_value(None, "schema_version", |row| row.get(0))
            .unwrap();
        assert_eq!(log.purge().unwrap(), 1);
        assert!(all_stats(&log).is_empty());
        assert!(export(&log, None).is_empty());
        assert_eq!(
            log.database
                .global_state_get::<String>(STATE_KEY)
                .unwrap()
                .as_deref(),
            Some(STATE_VALUE)
        );
        let session_count: i64 = log
            .database
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE id = ?1",
                [SESSION_ID.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(session_count, 1);
        let after_schema_version: i64 = log
            .database
            .connection()
            .pragma_query_value(None, "schema_version", |row| row.get(0))
            .unwrap();
        assert_eq!(after_schema_version, schema_version);
        assert_eq!(
            messages
                .history(&HistoryChannel::All, None, ROW_LIMIT)
                .unwrap()[0]
                .seq,
            seq
        );
        assert_eq!(messages.deliveries(&[seq]).unwrap().len(), 1);
        assert!(
            messages
                .unseen(SESSION_B, |_| true, ROW_LIMIT)
                .unwrap()
                .is_empty()
        );

        let replacement = log.insert(&stored_record).unwrap();
        assert!(replacement > id);
        log.database
            .connection()
            .execute(
                "DELETE FROM sessions WHERE id = ?1",
                [SESSION_ID.as_slice()],
            )
            .unwrap();
        assert_eq!(
            ids(&log, &DecisionFilter::default(), ROW_LIMIT),
            [replacement]
        );
        assert_eq!(
            messages
                .history(&HistoryChannel::All, None, ROW_LIMIT)
                .unwrap()[0]
                .seq,
            seq
        );
    }

    #[test]
    fn export_matches_laya_schema_and_keeps_full_target_definition() {
        let (_root, _state, mut log) = fixture();
        let record = record();
        let id = log.insert(&record).unwrap();
        log.insert(&record).unwrap();
        let expected = label(json!({"user_approves": false}));
        log.attach_label(id, &expected).unwrap();
        let rows = export(&log, Some("permission"));
        assert_eq!(
            rows,
            vec![json!({
                "state": record.state,
                "questions": {"user_approves": record.questions["user_approves"]},
                "expected": {"user_approves": false},
                "model": "english",
                "tags": ["caudra", "feature:permission", "qset:permission.v1@content-hash-a", "label:user"],
                "caudra": {
                    "id": id, "timestamp": TIMESTAMP, "session": "session-a", "project": "project-a",
                    "endpoint_kind": "local", "answers": record.answers, "error": null,
                    "latency_ms": 45, "mode": "shadow", "effect": "none", "meta": record.meta,
                    "label_timestamp": TIMESTAMP + 1, "label_meta": {"lifetime": "once"}
                }
            })]
        );
        assert!(export(&log, Some("other_feature")).is_empty());
    }

    #[test]
    fn labels_attach_by_row_not_session_or_question_set() {
        let (_root, _state, mut log) = fixture();
        let first = log.insert(&record()).unwrap();
        let mut second_record = record();
        second_record.question_set_version = "content-hash-b".into();
        let second = log.insert(&second_record).unwrap();
        log.attach_label(second, &label(json!({"user_approves": true})))
            .unwrap();
        log.attach_label(first, &label(json!({"uploads": false})))
            .unwrap();
        let rows = export(&log, None);
        assert_eq!(rows[0]["caudra"]["id"], first);
        assert_eq!(rows[0]["expected"], json!({"uploads": false}));
        assert_eq!(rows[1]["caudra"]["id"], second);
        assert_eq!(rows[1]["tags"][2], "qset:permission.v1@content-hash-b");
        log.attach_label(first, &label(json!({"user_approves": false})))
            .unwrap();
        assert_eq!(
            export(&log, None)[0]["expected"],
            json!({"user_approves": false})
        );
    }

    #[test_case(json!({"unknown": true}); "unknown_question")]
    #[test_case(json!({"user_approves": "yes"}); "wrong_noul_type")]
    #[test_case(json!({}); "empty_labels")]
    #[test_case(json!(true); "not_a_map")]
    fn invalid_label_does_not_replace_ground_truth(expected: Value) {
        let (_root, _state, mut log) = fixture();
        let id = log.insert(&record()).unwrap();
        log.attach_label(id, &label(json!({"user_approves": false})))
            .unwrap();
        assert!(matches!(
            log.attach_label(id, &label(expected)),
            Err(DecisionLogError::Invalid(_))
        ));
        assert_eq!(
            export(&log, None)[0]["expected"],
            json!({"user_approves": false})
        );
    }

    #[test]
    fn failures_can_receive_labels_without_fabricating_answers() {
        let (_root, _state, mut log) = fixture();
        let mut record = record();
        record.answers = None;
        record.error = Some(ENGINE_ERROR.into());
        let id = log.insert(&record).unwrap();
        log.attach_label(id, &label(json!({"user_approves": false})))
            .unwrap();
        let rows = export(&log, None);
        assert!(rows[0]["caudra"]["answers"].is_null());
        assert_eq!(rows[0]["caudra"]["error"], ENGINE_ERROR);
        let stats = all_stats(&log);
        assert_eq!(stats[0].labelled_count, 1);
        assert_eq!(stats[0].error_rate, 1.0);
        assert_eq!(stats[0].agreement_rate, None);
    }

    #[test]
    fn choice_and_score_export_and_agreement_use_actual_labels() {
        let (_root, _state, mut log) = fixture();
        let mut record = record();
        record.questions = json!({
            "duration": {"type": "choice", "instructions": "Choose duration", "criteria": {
                "short": {"description": "Finishes quickly"}, "long": ["Takes minutes"]
            }},
            "difficulty": {"type": "score", "criteria": ["easy", "medium", "hard"]}
        });
        record.answers = Some(json!({
            "duration": {"type": "choice", "choice": "short"},
            "difficulty": {"type": "score", "score": 1.05}
        }));
        let id = log.insert(&record).unwrap();
        log.attach_label(id, &label(json!({"duration": "long", "difficulty": 1})))
            .unwrap();
        let rows = export(&log, None);
        assert_eq!(rows[0]["questions"], record.questions);
        assert_eq!(
            rows[0]["expected"],
            json!({"duration": "long", "difficulty": 1})
        );
        let stats = all_stats(&log);
        assert_eq!(stats[0].compared_labels, 2);
        assert_eq!(stats[0].agreeing_labels, 1);
    }

    #[test]
    fn stats_filter_percentiles_errors_and_configured_thresholds() {
        let (_root, _state, mut log) = fixture();
        let mut record = record();
        for latency in 1..=20 {
            record.latency_ms = latency;
            let id = log.insert(&record).unwrap();
            log.attach_label(id, &label(json!({"user_approves": true})))
                .unwrap();
        }
        record.feature = "workflow".into();
        record.error = Some(ENGINE_ERROR.into());
        record.answers = None;
        log.insert(&record).unwrap();
        let stats = all_stats(&log);
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].feature, "permission");
        assert_eq!(stats[0].count, 20);
        assert_eq!(stats[0].latency_p50_ms, 10);
        assert_eq!(stats[0].latency_p95_ms, 19);
        assert_eq!(stats[0].agreement_rate, Some(1.0));
        assert_eq!(stats[1].error_count, 1);
        let mut thresholds = StatsThresholds::default();
        thresholds
            .noul_by_question
            .insert("user_approves".into(), 0.8);
        let filter = DecisionFilter {
            feature: Some("permission"),
            ..Default::default()
        };
        let stats = log.stats(&filter, |_| thresholds.clone()).unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].agreement_rate, Some(0.0));
        thresholds.default_noul = f64::NAN;
        assert!(matches!(
            log.stats(&DecisionFilter::default(), |_| thresholds.clone()),
            Err(DecisionLogError::Invalid(_))
        ));
    }

    #[test]
    fn retention_boundary_and_startup_pruning_remove_attached_labels() {
        let (_root, state, mut log) = fixture();
        let mut record = record();
        let first = log.insert(&record).unwrap();
        log.attach_label(first, &label(json!({"user_approves": true})))
            .unwrap();
        record.timestamp += 1;
        let boundary = log.insert(&record).unwrap();
        assert_eq!(log.prune_before(record.timestamp).unwrap(), 1);
        assert!(
            matches!(log.attach_label(first, &label(json!({"user_approves": true}))), Err(DecisionLogError::NotFound(id)) if id == first)
        );
        log.attach_label(boundary, &label(json!({"user_approves": false})))
            .unwrap();
        assert_eq!(export(&log, None)[0]["caudra"]["id"], boundary);
        record.timestamp = now_epoch() + SECONDS_PER_DAY;
        log.insert(&record).unwrap();
        drop(log);
        let existing = DecisionLog::open_existing(&state).unwrap().unwrap();
        assert_eq!(all_stats(&existing)[0].count, 2);
        drop(existing);
        let reopened = DecisionLog::open(&state, true, RETENTION_DAYS)
            .unwrap()
            .unwrap();
        assert_eq!(all_stats(&reopened)[0].count, 1);
        assert!(export(&reopened, None).is_empty());
    }

    #[test]
    fn purge_removes_rows_and_does_not_reuse_label_identities() {
        let (_root, _state, mut log) = fixture();
        let mut record = record();
        record.state = json!(PAYLOAD_MARKER);
        let old = log.insert(&record).unwrap();
        log.attach_label(old, &label(json!({"user_approves": true})))
            .unwrap();
        assert_eq!(log.purge().unwrap(), 1);
        assert!(all_stats(&log).is_empty());
        assert!(export(&log, None).is_empty());
        let new = log.insert(&record).unwrap();
        assert!(new > old);
        assert!(
            matches!(log.attach_label(old, &label(json!({"user_approves": true}))), Err(DecisionLogError::NotFound(id)) if id == old)
        );
    }

    #[test]
    fn oversized_or_invalid_records_are_rejected_without_truncation() {
        let (_root, _state, log) = fixture();
        let mut record = record();
        record.state = json!("x".repeat(MAX_RECORD_BYTES));
        assert!(matches!(
            log.insert(&record),
            Err(DecisionLogError::Invalid(_))
        ));
        record.state = Value::Null;
        record.questions = json!({"bad": {"type": "invented"}});
        assert!(matches!(
            log.insert(&record),
            Err(DecisionLogError::Invalid(_))
        ));
        assert!(all_stats(&log).is_empty());
    }

    #[test]
    fn future_schema_is_rejected_without_pruning() {
        let (_root, state, log) = fixture();
        log.insert(&record()).unwrap();
        log.database
            .connection()
            .pragma_update(None, "user_version", i32::MAX)
            .unwrap();
        assert!(matches!(
            DecisionLog::open(&state, true, RETENTION_DAYS),
            Err(DecisionLogError::Session(
                SessionError::UnsupportedSchemaVersion { .. }
            ))
        ));
        assert_eq!(all_stats(&log)[0].count, 1);
    }

    #[cfg(unix)]
    #[test]
    fn database_symlinks_and_public_permissions_are_rejected() {
        let (root, state, log) = fixture();
        let path = log.path().to_path_buf();
        drop(log);
        let target = root.path().join("target.db");
        fs::rename(&path, &target).unwrap();
        symlink(&target, &path).unwrap();
        assert!(matches!(
            DecisionLog::open_existing(&state),
            Err(DecisionLogError::Session(SessionError::Storage(_)))
        ));
        fs::remove_file(&path).unwrap();
        fs::rename(target, &path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            DecisionLog::open_existing(&state),
            Err(DecisionLogError::Session(SessionError::Storage(_)))
        ));
    }
}
