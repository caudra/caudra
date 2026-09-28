use crate::{StateDir, now_epoch};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DECISIONS_DB_FILE: &str = "decisions.db";
const SCHEMA_VERSION: i64 = 1;
const APPLICATION_ID: i64 = 0x4341444c;
const SECONDS_PER_DAY: u64 = 86_400;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(unix)]
const OWNER_FILE_MODE: u32 = 0o600;
const MAX_RECORD_BYTES: usize = 1_048_576;
const MAX_QUESTIONS: usize = 64;
const DEFAULT_NOUL_THRESHOLD: f64 = 0.5;
const DEFAULT_SCORE_TOLERANCE: f64 = 0.1;
const SCHEMA: &str = "
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
    #[error("decision log I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("decision log database operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("decision log JSON encoding failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported decision log schema")]
    UnsupportedSchema,
    #[error("unsafe decision log file; expected an owner-only regular file")]
    UnsafeFile,
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
    pub labelled_count: u64,
    pub latency_p50_ms: u64,
    pub latency_p95_ms: u64,
    pub compared_labels: u64,
    pub agreeing_labels: u64,
    pub agreement_rate: Option<f64>,
}

pub struct DecisionLog {
    connection: Connection,
    path: PathBuf,
}

impl DecisionLog {
    /// Disabled logging does no filesystem I/O. Retention uses row creation time in epoch seconds.
    pub fn open(
        state_dir: &StateDir,
        enabled: bool,
        retention_days: u64,
    ) -> Result<Option<Self>, DecisionLogError> {
        if !enabled {
            return Ok(None);
        }
        fs::create_dir_all(state_dir.persistent_path())?;
        let path = state_dir.persistent_path().join(DECISIONS_DB_FILE);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(OWNER_FILE_MODE);
        match options.open(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let log = Self::connect(path, true)?;
        log.prune_before(
            now_epoch().saturating_sub(retention_days.saturating_mul(SECONDS_PER_DAY)),
        )?;
        Ok(Some(log))
    }

    /// Maintenance access never creates a database or implicitly enables logging.
    pub fn open_existing(state_dir: &StateDir) -> Result<Option<Self>, DecisionLogError> {
        let path = state_dir.persistent_path().join(DECISIONS_DB_FILE);
        match fs::symlink_metadata(&path) {
            Ok(_) => Self::connect(path, false).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn connect(path: PathBuf, initialize: bool) -> Result<Self, DecisionLogError> {
        verify_file(&path)?;
        for suffix in ["-journal", "-wal", "-shm"] {
            let mut sidecar = path.as_os_str().to_owned();
            sidecar.push(suffix);
            match verify_file(Path::new(&sidecar)) {
                Err(DecisionLogError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
                result => result?,
            }
        }
        let mut connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.pragma_update(None, "trusted_schema", false)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 =
            transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let application: i64 =
            transaction.pragma_query_value(None, "application_id", |row| row.get(0))?;
        if version == 0 && application == 0 && initialize {
            let objects: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )?;
            if objects != 0 {
                return Err(DecisionLogError::UnsupportedSchema);
            }
            transaction.execute_batch(SCHEMA)?;
            transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else if version != SCHEMA_VERSION || application != APPLICATION_ID {
            return Err(DecisionLogError::UnsupportedSchema);
        }
        transaction.commit()?;
        // Rollback journals avoid keeping deleted payloads in a persistent WAL after purge.
        connection.pragma_update(None, "journal_mode", "DELETE")?;
        connection.pragma_update(None, "secure_delete", true)?;
        Ok(Self { connection, path })
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
        self.connection.execute(
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
        Ok(self.connection.last_insert_rowid())
    }

    pub fn update_effect(&self, id: i64, effect: DecisionEffect) -> Result<(), DecisionLogError> {
        let changed = self.connection.execute(
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
            .connection
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
        let mut statement = self.connection.prepare(
            "SELECT id, ts, session, project, feature, question_set_id, question_set_version,
                endpoint_kind, model, state, questions, answers, error, latency_ms, mode,
                effect, meta, label, label_source, label_ts, label_meta
             FROM decisions WHERE label IS NOT NULL AND (?1 IS NULL OR feature = ?1) ORDER BY id",
        )?;
        let mut rows = statement.query([feature])?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            let questions = row_json(row, 10)?;
            let expected = row_json(row, 17)?;
            validate_label(&questions, &expected)?;
            let selected: Map<String, Value> = question_map(&expected)?
                .keys()
                .map(|id| (id.clone(), questions[id].clone()))
                .collect();
            let feature: String = row.get(4)?;
            let question_set_id: String = row.get(5)?;
            let question_set_version: String = row.get(6)?;
            let source: String = row.get(18)?;
            let example = json!({
                "state": row_json(row, 9)?,
                "questions": selected,
                "expected": expected,
                "model": row.get::<_, String>(8)?,
                "tags": ["caudra", format!("feature:{feature}"),
                    format!("qset:{question_set_id}@{question_set_version}"), format!("label:{source}")],
                "caudra": {
                    "id": row.get::<_, i64>(0)?, "timestamp": row_u64(row, 1)?,
                    "session": row.get::<_, Option<String>>(2)?, "project": row.get::<_, Option<String>>(3)?,
                    "endpoint_kind": row.get::<_, String>(7)?, "answers": row_json(row, 11)?,
                    "error": row.get::<_, Option<String>>(12)?, "latency_ms": row_u64(row, 13)?,
                    "mode": row.get::<_, String>(14)?, "effect": row.get::<_, String>(15)?,
                    "meta": row_json(row, 16)?, "label_timestamp": row_u64(row, 19)?,
                    "label_meta": row_json(row, 20)?,
                },
            });
            serde_json::to_writer(&mut writer, &example)?;
            writer.write_all(b"\n")?;
            count += 1;
        }
        Ok(count)
    }

    /// Nearest-rank latency percentiles and per-answer agreement; missing answers are not votes.
    pub fn stats(
        &self,
        feature: Option<&str>,
        thresholds: &StatsThresholds,
    ) -> Result<Vec<DecisionStats>, DecisionLogError> {
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
        let mut statement = self.connection.prepare(
            "WITH ranked AS (
                SELECT feature, error, label, latency_ms,
                    ROW_NUMBER() OVER (PARTITION BY feature ORDER BY latency_ms) AS rank,
                    COUNT(*) OVER (PARTITION BY feature) AS n
                FROM decisions WHERE ?1 IS NULL OR feature = ?1
             ) SELECT feature, COUNT(*), SUM(error IS NOT NULL), SUM(label IS NOT NULL),
                MAX(CASE WHEN rank = (n + 1) / 2 THEN latency_ms END),
                MAX(CASE WHEN rank = (n * 95 + 99) / 100 THEN latency_ms END)
             FROM ranked GROUP BY feature ORDER BY feature",
        )?;
        let mut stats = BTreeMap::new();
        let mut rows = statement.query([feature])?;
        while let Some(row) = rows.next()? {
            let feature: String = row.get(0)?;
            let count = row_u64(row, 1)?;
            let error_count = row_u64(row, 2)?;
            stats.insert(
                feature.clone(),
                DecisionStats {
                    feature,
                    count,
                    error_count,
                    error_rate: error_count as f64 / count as f64,
                    labelled_count: row_u64(row, 3)?,
                    latency_p50_ms: row_u64(row, 4)?,
                    latency_p95_ms: row_u64(row, 5)?,
                    compared_labels: 0,
                    agreeing_labels: 0,
                    agreement_rate: None,
                },
            );
        }
        let mut statement = self.connection.prepare(
            "SELECT feature, questions, answers, label FROM decisions
             WHERE label IS NOT NULL AND answers IS NOT NULL AND error IS NULL AND (?1 IS NULL OR feature = ?1)",
        )?;
        let mut rows = statement.query([feature])?;
        while let Some(row) = rows.next()? {
            let feature: String = row.get(0)?;
            let questions = row_json(row, 1)?;
            let answers = row_json(row, 2)?;
            let expected = row_json(row, 3)?;
            if let Some(stats) = stats.get_mut(&feature) {
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
        for stats in stats.values_mut() {
            if stats.compared_labels > 0 {
                stats.agreement_rate =
                    Some(stats.agreeing_labels as f64 / stats.compared_labels as f64);
            }
        }
        Ok(stats.into_values().collect())
    }

    /// Removes rows strictly older than the cutoff, including attached labels.
    pub fn prune_before(&self, cutoff: u64) -> Result<usize, DecisionLogError> {
        Ok(self
            .connection
            .execute("DELETE FROM decisions WHERE ts < ?1", [sql_u64(cutoff)?])?)
    }

    /// Deletes payloads and reclaims database pages without reusing old row identities.
    pub fn purge(&self) -> Result<usize, DecisionLogError> {
        let count = self.connection.execute("DELETE FROM decisions", [])?;
        self.connection.execute_batch("VACUUM")?;
        Ok(count)
    }
}

fn verify_file(path: &Path) -> Result<(), DecisionLogError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(DecisionLogError::UnsafeFile);
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o777 != OWNER_FILE_MODE {
        return Err(DecisionLogError::UnsafeFile);
    }
    Ok(())
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
        DECISIONS_DB_FILE, DecisionEffect, DecisionLabel, DecisionLog, DecisionLogError,
        DecisionRecord, EndpointKind, MAX_RECORD_BYTES, SCHEMA_VERSION, SECONDS_PER_DAY,
        StatsThresholds,
    };
    use crate::{StateDir, now_epoch};
    use serde_json::{Value, json};
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::{TempDir, tempdir};
    use test_case::test_case;

    const TIMESTAMP: u64 = 1_000_000;
    const RETENTION_DAYS: u64 = 90;
    const ENGINE_ERROR: &str = "engine timeout";
    const PAYLOAD_MARKER: &str = "decision-payload-marker-for-purge-test";

    fn record() -> DecisionRecord {
        DecisionRecord {
            timestamp: TIMESTAMP,
            session: Some("session-a".into()),
            project: Some("project-a".into()),
            feature: "permission".into(),
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
        assert_eq!(log.path(), state.persistent_path().join(DECISIONS_DB_FILE));
        assert!(!state.path().exists());
        assert_eq!(fs::read_dir(state.persistent_path()).unwrap().count(), 1);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(log.path()).unwrap().permissions().mode() & 0o777,
            0o600
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
        let stats = log.stats(None, &StatsThresholds::default()).unwrap();
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
        let stats = log.stats(None, &StatsThresholds::default()).unwrap();
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
        let stats = log.stats(None, &StatsThresholds::default()).unwrap();
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
        let stats = log.stats(Some("permission"), &thresholds).unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].agreement_rate, Some(0.0));
        thresholds.default_noul = f64::NAN;
        assert!(matches!(
            log.stats(None, &thresholds),
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
        assert_eq!(
            existing.stats(None, &StatsThresholds::default()).unwrap()[0].count,
            2
        );
        drop(existing);
        let reopened = DecisionLog::open(&state, true, RETENTION_DAYS)
            .unwrap()
            .unwrap();
        assert_eq!(
            reopened.stats(None, &StatsThresholds::default()).unwrap()[0].count,
            1
        );
        assert!(export(&reopened, None).is_empty());
    }

    #[test]
    fn purge_erases_payloads_and_does_not_reuse_label_identities() {
        let (_root, _state, mut log) = fixture();
        let mut record = record();
        record.state = json!(PAYLOAD_MARKER);
        let old = log.insert(&record).unwrap();
        log.attach_label(old, &label(json!({"user_approves": true})))
            .unwrap();
        assert_eq!(log.purge().unwrap(), 1);
        assert!(
            log.stats(None, &StatsThresholds::default())
                .unwrap()
                .is_empty()
        );
        assert!(export(&log, None).is_empty());
        assert!(
            !fs::read(log.path())
                .unwrap()
                .windows(PAYLOAD_MARKER.len())
                .any(|bytes| bytes == PAYLOAD_MARKER.as_bytes())
        );
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
        assert!(
            log.stats(None, &StatsThresholds::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn future_schema_is_rejected_without_pruning() {
        let (_root, state, log) = fixture();
        log.insert(&record()).unwrap();
        log.connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        assert!(matches!(
            DecisionLog::open(&state, true, RETENTION_DAYS),
            Err(DecisionLogError::UnsupportedSchema)
        ));
        assert_eq!(
            log.stats(None, &StatsThresholds::default()).unwrap()[0].count,
            1
        );
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
            Err(DecisionLogError::UnsafeFile)
        ));
        fs::remove_file(&path).unwrap();
        fs::rename(target, &path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            DecisionLog::open_existing(&state),
            Err(DecisionLogError::UnsafeFile)
        ));
    }
}
