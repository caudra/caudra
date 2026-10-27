use serde::{Deserialize, Serialize};

use crate::sessions::SessionError;

const HOST_VERSION: u8 = 1;
const HOST_FIELD: &str = "shell_runs.host";
const MAX_HOST_BYTES: usize = 512;
const MAX_CPU_USAGE_WINDOW_MS: u64 = 10_000;
const INVALID_VERSION: &str = "unsupported host snapshot version";
const INVALID_RANGE: &str = "host snapshot measurements are outside their valid ranges";
const TOO_LARGE: &str = "host snapshot exceeds its byte limit";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostSnapshot {
    pub cpu_usage: Option<f32>,
    pub cpu_usage_mean: Option<f32>,
    pub cpu_usage_window_ms: Option<u64>,
    pub normalized_load: Option<f32>,
    pub cpu_pressure: Option<f32>,
    pub host_cpus: Option<u32>,
    pub available_cpus: Option<u32>,
    pub sample_age_ms: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredHost<T> {
    version: u8,
    snapshot: T,
}

impl HostSnapshot {
    fn validate(&self) -> Result<(), SessionError> {
        let valid = [self.cpu_usage, self.cpu_usage_mean, self.cpu_pressure]
            .into_iter()
            .flatten()
            .all(|value| value.is_finite() && (0.0..=1.0).contains(&value))
            && self
                .normalized_load
                .is_none_or(|value| value.is_finite() && value >= 0.0)
            && self.host_cpus != Some(0)
            && self.available_cpus != Some(0)
            && self.cpu_usage_mean.is_some() == self.cpu_usage_window_ms.is_some()
            && self
                .cpu_usage_window_ms
                .is_none_or(|value| (1..=MAX_CPU_USAGE_WINDOW_MS).contains(&value));
        if !valid {
            return Err(corrupt(INVALID_RANGE));
        }
        Ok(())
    }
}

pub(super) fn encode(snapshot: &HostSnapshot) -> Result<String, SessionError> {
    snapshot.validate()?;
    let value = serde_json::to_string(&StoredHost {
        version: HOST_VERSION,
        snapshot,
    })
    .map_err(|error| corrupt(error.to_string()))?;
    if value.len() > MAX_HOST_BYTES {
        return Err(corrupt(TOO_LARGE));
    }
    Ok(value)
}

pub(super) fn decode(value: &str) -> Result<HostSnapshot, SessionError> {
    if value.len() > MAX_HOST_BYTES {
        return Err(corrupt(TOO_LARGE));
    }
    let stored: StoredHost<HostSnapshot> =
        serde_json::from_str(value).map_err(|error| corrupt(error.to_string()))?;
    if stored.version != HOST_VERSION {
        return Err(corrupt(INVALID_VERSION));
    }
    stored.snapshot.validate()?;
    Ok(stored.snapshot)
}

fn corrupt(reason: impl Into<String>) -> SessionError {
    SessionError::CorruptDatabaseValue {
        field: HOST_FIELD,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{
        HostSnapshot, INVALID_RANGE, MAX_CPU_USAGE_WINDOW_MS, MAX_HOST_BYTES, decode, encode,
    };
    use crate::sessions::SessionError;

    const LEGACY_SNAPSHOT: &str = r#"{"version":1,"snapshot":{"cpu_usage":0.5,"normalized_load":2.0,"cpu_pressure":null,"host_cpus":16,"available_cpus":2,"sample_age_ms":null}}"#;

    #[test_case(f32::NAN; "nan")]
    #[test_case(f32::INFINITY; "infinity")]
    #[test_case(f32::NEG_INFINITY; "negative_infinity")]
    #[test_case(-0.1; "negative")]
    fn nonfinite_or_negative_measurements_are_rejected(value: f32) {
        for snapshot in [
            HostSnapshot {
                cpu_usage: Some(value),
                ..HostSnapshot::default()
            },
            HostSnapshot {
                cpu_usage_mean: Some(value),
                cpu_usage_window_ms: Some(MAX_CPU_USAGE_WINDOW_MS),
                ..HostSnapshot::default()
            },
            HostSnapshot {
                normalized_load: Some(value),
                ..HostSnapshot::default()
            },
            HostSnapshot {
                cpu_pressure: Some(value),
                ..HostSnapshot::default()
            },
        ] {
            assert!(matches!(
                encode(&snapshot),
                Err(SessionError::CorruptDatabaseValue { reason, .. }) if reason == INVALID_RANGE
            ));
        }
    }

    #[test_case(HostSnapshot { cpu_pressure: Some(1.1), ..HostSnapshot::default() }; "pressure_above_one")]
    #[test_case(HostSnapshot { cpu_usage_mean: Some(1.1), cpu_usage_window_ms: Some(MAX_CPU_USAGE_WINDOW_MS), ..HostSnapshot::default() }; "mean_above_one")]
    #[test_case(HostSnapshot { host_cpus: Some(0), ..HostSnapshot::default() }; "zero_host_cpus")]
    #[test_case(HostSnapshot { available_cpus: Some(0), ..HostSnapshot::default() }; "zero_available_cpus")]
    fn invalid_measurement_ranges_are_rejected(snapshot: HostSnapshot) {
        assert!(matches!(
            encode(&snapshot),
            Err(SessionError::CorruptDatabaseValue { reason, .. }) if reason == INVALID_RANGE
        ));
    }

    #[test_case(Some(0.5), Some(0); "zero_coverage")]
    #[test_case(Some(0.5), Some(MAX_CPU_USAGE_WINDOW_MS + 1); "coverage_above_window")]
    #[test_case(Some(0.5), Some(u64::MAX); "maximum_coverage")]
    #[test_case(Some(0.5), None; "mean_without_coverage")]
    #[test_case(None, Some(MAX_CPU_USAGE_WINDOW_MS); "coverage_without_mean")]
    fn invalid_rolling_coverage_is_rejected(mean: Option<f32>, coverage: Option<u64>) {
        let snapshot = HostSnapshot {
            cpu_usage_mean: mean,
            cpu_usage_window_ms: coverage,
            ..HostSnapshot::default()
        };
        assert!(matches!(
            encode(&snapshot),
            Err(SessionError::CorruptDatabaseValue { reason, .. }) if reason == INVALID_RANGE
        ));
    }

    #[test_case(0.0, 1; "zero_mean_minimum_coverage")]
    #[test_case(1.0, MAX_CPU_USAGE_WINDOW_MS; "full_mean_full_coverage")]
    fn rolling_measurements_round_trip(mean: f32, coverage: u64) {
        let snapshot = HostSnapshot {
            cpu_usage_mean: Some(mean),
            cpu_usage_window_ms: Some(coverage),
            ..HostSnapshot::default()
        };
        assert_eq!(decode(&encode(&snapshot).unwrap()).unwrap(), snapshot);
    }

    #[test_case(LEGACY_SNAPSHOT; "legacy_partial_snapshot")]
    fn legacy_snapshot_keeps_rolling_measurements_unknown(value: &str) {
        assert_eq!(
            decode(value).unwrap(),
            HostSnapshot {
                cpu_usage: Some(0.5),
                normalized_load: Some(2.0),
                host_cpus: Some(16),
                available_cpus: Some(2),
                ..HostSnapshot::default()
            }
        );
    }

    #[test_case(r#"{"version":1,"snapshot":{"normalized_load":-1}}"#; "negative_load")]
    #[test_case(r#"{"version":1,"snapshot":{"cpu_pressure":1.1}}"#; "pressure_above_one")]
    #[test_case(r#"{"version":1,"snapshot":{"host_cpus":0}}"#; "zero_host_cpus")]
    #[test_case(r#"{"version":1,"snapshot":{"available_cpus":0}}"#; "zero_available_cpus")]
    #[test_case(r#"{"version":1,"snapshot":{"sample_age_ms":-1}}"#; "negative_age")]
    #[test_case(r#"{"version":1,"snapshot":{"cpu_usage":1e100}}"#; "float_overflow")]
    #[test_case(r#"{"version":1,"snapshot":{"cpu_usage_mean":1e100,"cpu_usage_window_ms":10000}}"#; "mean_overflow")]
    #[test_case(r#"{"version":1,"snapshot":{"cpu_usage_mean":-0.1,"cpu_usage_window_ms":10000}}"#; "negative_mean")]
    #[test_case(r#"{"version":1,"snapshot":{"cpu_usage_mean":1.1,"cpu_usage_window_ms":10000}}"#; "mean_above_one")]
    #[test_case(r#"{"version":1,"snapshot":{"cpu_usage_mean":0.5,"cpu_usage_window_ms":0}}"#; "zero_coverage")]
    #[test_case(r#"{"version":1,"snapshot":{"cpu_usage_mean":0.5,"cpu_usage_window_ms":10001}}"#; "excessive_coverage")]
    #[test_case(r#"{"version":1,"snapshot":{"cpu_usage_mean":0.5}}"#; "missing_coverage")]
    #[test_case(r#"{"version":1,"snapshot":{"cpu_usage_window_ms":10000}}"#; "missing_mean")]
    fn invalid_stored_measurements_are_rejected(value: &str) {
        assert!(matches!(
            decode(value),
            Err(SessionError::CorruptDatabaseValue { .. })
        ));
    }

    #[test_case(0.0; "zero_load")]
    #[test_case(f32::MAX; "maximum_load")]
    fn largest_integers_and_load_fit_the_encoding(normalized_load: f32) {
        let snapshot = HostSnapshot {
            cpu_usage: Some(1.0),
            cpu_usage_mean: Some(1.0),
            cpu_usage_window_ms: Some(MAX_CPU_USAGE_WINDOW_MS),
            normalized_load: Some(normalized_load),
            cpu_pressure: Some(1.0),
            host_cpus: Some(u32::MAX),
            available_cpus: Some(u32::MAX),
            sample_age_ms: Some(u64::MAX),
        };
        let encoded = encode(&snapshot).unwrap();
        assert!(encoded.len() <= MAX_HOST_BYTES);
        assert_eq!(decode(&encoded).unwrap(), snapshot);
    }
}
