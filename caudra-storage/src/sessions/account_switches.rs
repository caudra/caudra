//! Account-switch transcript rows: when routing moved a session's requests
//! from one account of a provider to another, kept for display only and never
//! sent to the model. Rows belong to a session and go with it through the
//! schema's cascade. Each session keeps its newest
//! [`MAX_ACCOUNT_SWITCHES_PER_SESSION`], a session rewrite keeps them, and a
//! fork starts with none.

use rusqlite::{Row, Transaction, TransactionBehavior, params};

use crate::id::CaudraId;
use crate::sessions::{SessionDatabase, SessionError, from_i64, from_i64_usize, to_i64};

/// Rows each session keeps. An insert past it drops the oldest.
pub const MAX_ACCOUNT_SWITCHES_PER_SESSION: usize = 1000;
const ACCOUNT_SWITCH_COLUMNS: &str =
    "seq, at, history_len, provider, from_account, to_account, summary";
const SEQ_FIELD: &str = "account_switches.seq";
const AT_FIELD: &str = "account_switches.at";
const HISTORY_LEN_FIELD: &str = "account_switches.history_len";
const LIMIT_FIELD: &str = "account_switches.limit";

pub(crate) const TABLE: &str = r#"
CREATE TABLE account_switches (
    session_id   BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    seq          INTEGER NOT NULL CHECK(seq > 0),
    at           INTEGER NOT NULL,
    history_len  INTEGER NOT NULL CHECK(history_len >= 0),
    provider     TEXT NOT NULL,
    from_account TEXT NOT NULL,
    to_account   TEXT NOT NULL,
    summary      TEXT NOT NULL,
    PRIMARY KEY(session_id, seq)
) STRICT, WITHOUT ROWID;
"#;

/// One account-switch transcript row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAccountSwitch {
    /// Assigned by the store on insert, ignored on input.
    pub seq: u64,
    /// Unix milliseconds.
    pub at_ms: u64,
    /// Transcript items the session held when it switched, which places the
    /// row on restore.
    pub history_len: usize,
    pub provider: String,
    pub from_account: String,
    pub to_account: String,
    pub summary: String,
}

impl SessionDatabase {
    /// Appends `switches` in order after the session's newest, then drops its
    /// oldest past [`MAX_ACCOUNT_SWITCHES_PER_SESSION`] in the same
    /// transaction.
    pub fn insert_account_switches(
        &self,
        session_id: CaudraId,
        switches: &[StoredAccountSwitch],
    ) -> Result<(), SessionError> {
        if switches.is_empty() {
            return Ok(());
        }
        let session_id = session_id.as_bytes().as_slice();
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        {
            let mut insert = transaction.prepare(&format!(
                "INSERT INTO account_switches (session_id, {ACCOUNT_SWITCH_COLUMNS}) \
                 SELECT ?1, coalesce(max(seq), 0) + 1, ?2, ?3, ?4, ?5, ?6, ?7 \
                 FROM account_switches WHERE session_id = ?1"
            ))?;
            for switch in switches {
                insert.execute(params![
                    session_id,
                    to_i64(switch.at_ms, AT_FIELD)?,
                    to_i64(switch.history_len, HISTORY_LEN_FIELD)?,
                    switch.provider,
                    switch.from_account,
                    switch.to_account,
                    switch.summary,
                ])?;
            }
        }
        transaction.execute(
            "DELETE FROM account_switches WHERE session_id = ?1 AND seq <= (\
                 SELECT seq FROM account_switches WHERE session_id = ?1 \
                 ORDER BY seq DESC LIMIT 1 OFFSET ?2)",
            params![
                session_id,
                to_i64(MAX_ACCOUNT_SWITCHES_PER_SESSION, LIMIT_FIELD)?
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// A session's rows, oldest first.
    pub fn load_account_switches(
        &self,
        session_id: CaudraId,
    ) -> Result<Vec<StoredAccountSwitch>, SessionError> {
        let mut statement = self.connection().prepare(&format!(
            "SELECT {ACCOUNT_SWITCH_COLUMNS} FROM account_switches WHERE session_id = ?1 \
             ORDER BY seq"
        ))?;
        let mut rows = statement.query(params![session_id.as_bytes().as_slice()])?;
        let mut switches = Vec::new();
        while let Some(row) = rows.next()? {
            switches.push(read_switch(row)?);
        }
        Ok(switches)
    }
}

fn read_switch(row: &Row<'_>) -> Result<StoredAccountSwitch, SessionError> {
    Ok(StoredAccountSwitch {
        seq: from_i64(row.get(0)?, SEQ_FIELD)?,
        at_ms: from_i64(row.get(1)?, AT_FIELD)?,
        history_len: from_i64_usize(row.get(2)?, HISTORY_LEN_FIELD)?,
        provider: row.get(3)?,
        from_account: row.get(4)?,
        to_account: row.get(5)?,
        summary: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::*;
    use crate::StateDir;
    use crate::sessions::Session;

    const CWD: &str = "/project";
    const MODEL: &str = "test/model";
    const PROVIDER: &str = "anthropic";
    const PERSONAL: &str = "personal";
    const WORK: &str = "work";
    const SUMMARY: &str = "anthropic:personal hit its 5h limit; switched to anthropic:work";
    const AT_MS: u64 = 1_767_225_600_000;
    const HISTORY_LEN: usize = 12;
    const IGNORED_SEQ: u64 = 99;
    const FIRST_SEQ: u64 = 1;
    const EXTRA_SWITCHES: usize = 2;
    const ROUND_TRIPS: &str = "rows must load whole, in order, with store-assigned seqs";
    const TRIM_KEEPS_NEWEST: &str = "an insert must keep only the session's newest rows";
    const REWRITE_KEEPS_ROWS: &str = "saving a session must keep its account switches";
    const CASCADE: &str = "account switches must go with their session";

    type TestSession = Session<Value, Value, Value>;

    fn open() -> (TempDir, SessionDatabase, CaudraId) {
        let temp = TempDir::new().unwrap();
        let mut database =
            SessionDatabase::open(&StateDir::from_path(temp.path().to_path_buf())).unwrap();
        let session = TestSession::new(MODEL, CWD);
        database.save(&session, None).unwrap();
        (temp, database, session.id)
    }

    fn switch(from_account: &str, to_account: &str) -> StoredAccountSwitch {
        StoredAccountSwitch {
            seq: IGNORED_SEQ,
            at_ms: AT_MS,
            history_len: HISTORY_LEN,
            provider: PROVIDER.into(),
            from_account: from_account.into(),
            to_account: to_account.into(),
            summary: SUMMARY.into(),
        }
    }

    fn numbered(switches: &[StoredAccountSwitch], first: u64) -> Vec<StoredAccountSwitch> {
        (first..)
            .zip(switches)
            .map(|(seq, switch)| StoredAccountSwitch {
                seq,
                ..switch.clone()
            })
            .collect()
    }

    #[test]
    fn switches_round_trip_in_order() {
        let (_temp, database, session_id) = open();
        let written = [switch(PERSONAL, WORK), switch(WORK, PERSONAL)];

        database
            .insert_account_switches(session_id, &written[..1])
            .unwrap();
        database
            .insert_account_switches(session_id, &written[1..])
            .unwrap();

        assert_eq!(
            database.load_account_switches(session_id).unwrap(),
            numbered(&written, FIRST_SEQ),
            "{ROUND_TRIPS}"
        );
    }

    #[test]
    fn an_insert_keeps_only_the_newest_switches() {
        let (_temp, database, session_id) = open();
        let written =
            vec![switch(PERSONAL, WORK); MAX_ACCOUNT_SWITCHES_PER_SESSION + EXTRA_SWITCHES];

        database
            .insert_account_switches(session_id, &written)
            .unwrap();

        let kept = database.load_account_switches(session_id).unwrap();
        assert_eq!(
            kept.len(),
            MAX_ACCOUNT_SWITCHES_PER_SESSION,
            "{TRIM_KEEPS_NEWEST}"
        );
        assert_eq!(
            kept.first().map(|switch| switch.seq),
            Some(EXTRA_SWITCHES as u64 + FIRST_SEQ),
            "{TRIM_KEEPS_NEWEST}"
        );
    }

    #[test]
    fn a_rewritten_session_keeps_its_switches() {
        let (_temp, mut database, session_id) = open();
        let written = [switch(PERSONAL, WORK)];
        database
            .insert_account_switches(session_id, &written)
            .unwrap();
        let mut session = database.load::<Value, Value, Value>(session_id).unwrap();
        session.push_message(json!({"role": "user", "content": []}));

        database.save(&session, None).unwrap();

        assert_eq!(
            database.load_account_switches(session_id).unwrap(),
            numbered(&written, FIRST_SEQ),
            "{REWRITE_KEEPS_ROWS}"
        );
    }

    #[test]
    fn deleting_a_session_removes_its_switches() {
        let (_temp, mut database, session_id) = open();
        database
            .insert_account_switches(session_id, &[switch(PERSONAL, WORK)])
            .unwrap();

        database.delete(session_id, None).unwrap();

        assert!(
            database
                .load_account_switches(session_id)
                .unwrap()
                .is_empty(),
            "{CASCADE}"
        );
    }
}
