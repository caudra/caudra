//! How far each model's own tokenizer is from the local o200k estimate, learned
//! from what providers billed. One row per provider and model, rewritten whole
//! each time a sample moves it.
//!
//! Writes are last-writer-wins: two processes learning the same model at once
//! can lose each other's latest samples. The row is an estimate's correction
//! and converges again from the next ones, so that is cheaper than merging.

use rusqlite::params;
use serde::Serialize;

use crate::sessions::{SessionDatabase, SessionError, from_i64, to_i64};
use crate::{StateClass, StateDir};

const SAMPLES_FIELD: &str = "token_calibration.samples";
const UPDATED_AT_FIELD: &str = "token_calibration.updated_at";

pub(crate) const TABLE: &str = r#"
CREATE TABLE token_calibration (
    provider   TEXT NOT NULL,
    model      TEXT NOT NULL,
    ratio      REAL NOT NULL,
    weight     REAL NOT NULL,
    samples    INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY(provider, model)
) STRICT, WITHOUT ROWID;
"#;

/// A model's learned ratio of billed tokens to estimated ones, with the
/// decayed weight of the samples behind it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoredCalibration {
    pub provider: String,
    pub model: String,
    pub ratio: f64,
    pub weight: f64,
    pub samples: u64,
    pub updated_at: u64,
}

/// Opened on the persistent root like the usage ledger: what a model's
/// tokenizer is like does not depend on which session found out.
pub struct TokenCalibrationStore {
    database: SessionDatabase,
}

impl TokenCalibrationStore {
    pub fn open(dir: &StateDir) -> Result<Self, SessionError> {
        Ok(Self {
            database: SessionDatabase::open_state(&dir.for_class(StateClass::Persistent))?,
        })
    }

    pub fn load_all(&self) -> Result<Vec<StoredCalibration>, SessionError> {
        let mut statement = self.database.connection().prepare(
            "SELECT provider, model, ratio, weight, samples, updated_at FROM token_calibration",
        )?;
        let mut rows = statement.query([])?;
        let mut calibrations = Vec::new();
        while let Some(row) = rows.next()? {
            calibrations.push(StoredCalibration {
                provider: row.get(0)?,
                model: row.get(1)?,
                ratio: row.get(2)?,
                weight: row.get(3)?,
                samples: from_i64(row.get(4)?, SAMPLES_FIELD)?,
                updated_at: from_i64(row.get(5)?, UPDATED_AT_FIELD)?,
            });
        }
        Ok(calibrations)
    }

    pub fn upsert(&self, calibration: &StoredCalibration) -> Result<(), SessionError> {
        self.database.connection().execute(
            "INSERT INTO token_calibration (provider, model, ratio, weight, samples, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(provider, model) DO UPDATE SET \
             ratio = excluded.ratio, weight = excluded.weight, samples = excluded.samples, \
             updated_at = excluded.updated_at",
            params![
                calibration.provider,
                calibration.model,
                calibration.ratio,
                calibration.weight,
                to_i64(calibration.samples, SAMPLES_FIELD)?,
                to_i64(calibration.updated_at, UPDATED_AT_FIELD)?,
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{StoredCalibration, TokenCalibrationStore};
    use crate::StateDir;

    const PROVIDER: &str = "anthropic";
    const MODEL: &str = "claude-opus-5-5";
    const OTHER_MODEL: &str = "claude-sonnet-5";
    const ROUND_TRIP: &str = "a stored calibration loads back as written";
    const REPLACED: &str = "a second write for the same model replaces the first";

    fn store() -> (TempDir, TokenCalibrationStore) {
        let temp = TempDir::new().unwrap();
        let store =
            TokenCalibrationStore::open(&StateDir::from_path(temp.path().to_path_buf())).unwrap();
        (temp, store)
    }

    fn calibration(model: &str, ratio: f64, samples: u64) -> StoredCalibration {
        StoredCalibration {
            provider: PROVIDER.to_owned(),
            model: model.to_owned(),
            ratio,
            weight: 4096.0,
            samples,
            updated_at: 1_700_000_000_000,
        }
    }

    #[test]
    fn an_empty_store_loads_nothing() {
        let (_temp, store) = store();
        assert!(store.load_all().unwrap().is_empty());
    }

    #[test_case(&[(MODEL, 1.2, 1)], &[(MODEL, 1.2, 1)], ROUND_TRIP ; "one_model")]
    #[test_case(&[(MODEL, 1.2, 1), (MODEL, 1.3, 2)], &[(MODEL, 1.3, 2)], REPLACED ; "rewritten")]
    #[test_case(
        &[(MODEL, 1.2, 1), (OTHER_MODEL, 0.9, 3)],
        &[(MODEL, 1.2, 1), (OTHER_MODEL, 0.9, 3)],
        ROUND_TRIP ;
        "two_models"
    )]
    fn writes_load_back(writes: &[(&str, f64, u64)], expected: &[(&str, f64, u64)], message: &str) {
        let (_temp, store) = store();
        for (model, ratio, samples) in writes {
            store.upsert(&calibration(model, *ratio, *samples)).unwrap();
        }
        let mut loaded = store.load_all().unwrap();
        loaded.sort_by(|a, b| a.model.cmp(&b.model));
        let expected: Vec<_> = expected
            .iter()
            .map(|(model, ratio, samples)| calibration(model, *ratio, *samples))
            .collect();
        assert_eq!(loaded, expected, "{message}");
    }
}
