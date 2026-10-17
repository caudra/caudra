//! What each account's provider last said about its quota, kept in the
//! persistent state database so every Caudra process routes on the same
//! picture. Rows name no session: they outlive transcripts and go with the
//! account they describe.

use rusqlite::{Connection, Row, params};
use serde::{Deserialize, Serialize};

use crate::sessions::{SessionDatabase, SessionError, deserialize_json, from_i64, to_i64};
use crate::{StateClass, StateDir, StorageError};

const WINDOWS_FIELD: &str = "account_health.windows";
const BLOCKED_UNTIL_FIELD: &str = "account_health.blocked_until";
const OBSERVED_AT_FIELD: &str = "account_health.observed_at";

pub(crate) const TABLE: &str = r#"
CREATE TABLE account_health (
    provider       TEXT NOT NULL,
    account        TEXT NOT NULL,
    windows        TEXT NOT NULL CHECK(json_valid(windows)),
    blocked_until  INTEGER,
    blocked_reason TEXT,
    observed_at    INTEGER NOT NULL,
    PRIMARY KEY(provider, account)
) STRICT, WITHOUT ROWID;
"#;

/// One quota window as the provider reported it. `kind` and `scope` are the
/// provider crate's window names, kept as text so a new window needs no
/// migration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredQuotaWindow {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub used_percent: f32,
    /// Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredAccountHealth {
    pub provider: String,
    pub account: String,
    pub windows: Vec<StoredQuotaWindow>,
    /// Unix milliseconds until which the account must not be routed to.
    pub blocked_until: Option<u64>,
    pub blocked_reason: Option<String>,
    /// Unix milliseconds of the response this was read from.
    pub observed_at: u64,
}

fn with_database<T>(
    dir: &StateDir,
    operation: impl FnOnce(&Connection) -> Result<T, SessionError>,
) -> Result<T, StorageError> {
    let database = SessionDatabase::open_state(&dir.for_class(StateClass::Persistent))?;
    Ok(operation(database.connection())?)
}

/// Every account's health, by provider then account.
pub fn load_account_health(dir: &StateDir) -> Result<Vec<StoredAccountHealth>, StorageError> {
    with_database(dir, |connection| {
        let mut statement = connection.prepare(
            "SELECT provider, account, windows, blocked_until, blocked_reason, observed_at \
             FROM account_health ORDER BY provider, account",
        )?;
        let mut rows = statement.query([])?;
        let mut health = Vec::new();
        while let Some(row) = rows.next()? {
            health.push(read_health(row)?);
        }
        Ok(health)
    })
}

/// Replaces the account's row, since each observation supersedes the last.
pub fn save_account_health(
    dir: &StateDir,
    health: &StoredAccountHealth,
) -> Result<(), StorageError> {
    let windows = serde_json::to_string(&health.windows)?;
    with_database(dir, |connection| {
        connection.execute(
            "INSERT OR REPLACE INTO account_health \
                 (provider, account, windows, blocked_until, blocked_reason, observed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                health.provider,
                health.account,
                windows,
                health
                    .blocked_until
                    .map(|until| to_i64(until, BLOCKED_UNTIL_FIELD))
                    .transpose()?,
                health.blocked_reason,
                to_i64(health.observed_at, OBSERVED_AT_FIELD)?,
            ],
        )?;
        Ok(())
    })
}

pub fn delete_account_health(
    dir: &StateDir,
    provider: &str,
    account: &str,
) -> Result<bool, StorageError> {
    with_database(dir, |connection| {
        Ok(connection.execute(
            "DELETE FROM account_health WHERE provider = ?1 AND account = ?2",
            params![provider, account],
        )? > 0)
    })
}

/// Follows an account rename. A leftover row under the new name belonged to
/// an account since removed, so it gives way.
pub fn rename_account_health(
    dir: &StateDir,
    provider: &str,
    from: &str,
    to: &str,
) -> Result<(), StorageError> {
    with_database(dir, |connection| {
        connection.execute(
            "UPDATE OR REPLACE account_health SET account = ?3 \
             WHERE provider = ?1 AND account = ?2",
            params![provider, from, to],
        )?;
        Ok(())
    })
}

fn read_health(row: &Row<'_>) -> Result<StoredAccountHealth, SessionError> {
    Ok(StoredAccountHealth {
        provider: row.get(0)?,
        account: row.get(1)?,
        windows: deserialize_json(&row.get::<_, String>(2)?, WINDOWS_FIELD)?,
        blocked_until: row
            .get::<_, Option<i64>>(3)?
            .map(|until| from_i64(until, BLOCKED_UNTIL_FIELD))
            .transpose()?,
        blocked_reason: row.get(4)?,
        observed_at: from_i64(row.get(5)?, OBSERVED_AT_FIELD)?,
    })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    const ANTHROPIC: &str = "anthropic";
    const OPENAI: &str = "openai";
    const PERSONAL: &str = "personal";
    const WORK: &str = "work";
    const TEAM: &str = "team";
    const FIVE_HOUR: &str = "five_hour";
    const WEEKLY: &str = "weekly";
    const OPUS_SCOPE: &str = "opus";
    const OBSERVED_AT: u64 = 1_767_225_600_000;
    const RESETS_AT: u64 = OBSERVED_AT + 3_600_000;
    const FIVE_HOURS_SECS: u64 = 5 * 60 * 60;
    const BLOCKED_REASON: &str = "rate limited";
    const ROUND_TRIPS: &str = "health must load back exactly as saved";
    const UPSERTS: &str = "a newer observation must replace the account's row";
    const RENAME_FOLLOWS: &str = "health must follow its account to the new name";
    const DELETE_ONE: &str = "deleting must remove only that account's row";

    fn state() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        (temp, dir)
    }

    fn health(provider: &str, account: &str, used_percent: f32) -> StoredAccountHealth {
        StoredAccountHealth {
            provider: provider.into(),
            account: account.into(),
            windows: vec![
                StoredQuotaWindow {
                    kind: FIVE_HOUR.into(),
                    scope: None,
                    used_percent,
                    resets_at: Some(RESETS_AT),
                    length_secs: Some(FIVE_HOURS_SECS),
                },
                StoredQuotaWindow {
                    kind: WEEKLY.into(),
                    scope: Some(OPUS_SCOPE.into()),
                    used_percent,
                    resets_at: None,
                    length_secs: None,
                },
            ],
            blocked_until: Some(RESETS_AT),
            blocked_reason: Some(BLOCKED_REASON.into()),
            observed_at: OBSERVED_AT,
        }
    }

    #[test]
    fn health_round_trips_sorted_by_provider_then_account() {
        let (_temp, dir) = state();
        let rows = [
            health(OPENAI, PERSONAL, 10.0),
            health(ANTHROPIC, WORK, 20.0),
            StoredAccountHealth {
                windows: Vec::new(),
                blocked_until: None,
                blocked_reason: None,
                ..health(ANTHROPIC, PERSONAL, 0.0)
            },
        ];
        for row in &rows {
            save_account_health(&dir, row).unwrap();
        }

        assert_eq!(
            load_account_health(&dir).unwrap(),
            [rows[2].clone(), rows[1].clone(), rows[0].clone()],
            "{ROUND_TRIPS}"
        );
    }

    #[test]
    fn saving_again_replaces_the_row() {
        let (_temp, dir) = state();
        save_account_health(&dir, &health(ANTHROPIC, PERSONAL, 10.0)).unwrap();
        let newer = health(ANTHROPIC, PERSONAL, 90.0);

        save_account_health(&dir, &newer).unwrap();

        assert_eq!(load_account_health(&dir).unwrap(), [newer], "{UPSERTS}");
    }

    #[test]
    fn rename_moves_the_row_over_a_leftover() {
        let (_temp, dir) = state();
        save_account_health(&dir, &health(ANTHROPIC, PERSONAL, 10.0)).unwrap();
        save_account_health(&dir, &health(ANTHROPIC, TEAM, 99.0)).unwrap();

        rename_account_health(&dir, ANTHROPIC, PERSONAL, TEAM).unwrap();

        assert_eq!(
            load_account_health(&dir).unwrap(),
            [health(ANTHROPIC, TEAM, 10.0)],
            "{RENAME_FOLLOWS}"
        );
    }

    #[test]
    fn delete_removes_one_row() {
        let (_temp, dir) = state();
        save_account_health(&dir, &health(ANTHROPIC, PERSONAL, 10.0)).unwrap();
        save_account_health(&dir, &health(ANTHROPIC, WORK, 10.0)).unwrap();

        assert!(delete_account_health(&dir, ANTHROPIC, PERSONAL).unwrap());
        assert!(!delete_account_health(&dir, ANTHROPIC, PERSONAL).unwrap());

        assert_eq!(
            load_account_health(&dir).unwrap(),
            [health(ANTHROPIC, WORK, 10.0)],
            "{DELETE_ONE}"
        );
    }
}
