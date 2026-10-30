use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use caudra_storage::StateDir;
use caudra_storage::deferred;
use caudra_storage::shell_runs::reading::{Call, Reading, read};
use caudra_storage::shell_runs::{HostSnapshot, Prediction, RunOutcome, ShellRun, ShellRuns};
use tempfile::TempDir;
use test_case::test_case;

use super::sleep::{Timing, analyze};
use super::{
    Guess, INSTANT, INSTANT_BAR, MINUTES, MINUTES_WORD, Model, SECONDS, SECONDS_WORD, ShellEta,
    WORD_BAR, class_of, class_word,
};
use crate::tools::Deadline;

const QUICK: &str = "git status";
const BUILD: &str = "cargo build --release";
const UNKNOWN: &str = "frobnicate --all";
const QUICK_MS: u64 = 20;
const BUILD_MS: u64 = 39_500;
const BUILD_ETA: &str = "~40s";
const REPEATS: usize = 60;
const SLEEP: &str = "sleep 30";
const SLEEP_MS: u64 = 30_000;
const SLEEP_ETA: &str = "~30s";
const SLEEP_FLOOR: &str = "at least 30s";
const MAKE_BUILD: &str = "make build";
const MAKE_BUILD_MS: u64 = 40_000;
const ADDITIVE_ETA: &str = "~1m10s";
const ADDITIVE_MS: u64 = SLEEP_MS + MAKE_BUILD_MS;

fn reading(command: &str) -> Reading {
    read(&call(command))
}

fn call(command: &str) -> Call<'_> {
    Call {
        command,
        workdir: None,
        cwd: None,
        timeout_secs: None,
    }
}

fn run(command: &str, elapsed_ms: u64) -> ShellRun {
    ShellRun {
        reading: reading(command),
        host: None,
        elapsed_ms,
        outcome: RunOutcome::Completed,
    }
}

fn history() -> Vec<ShellRun> {
    (0..REPEATS)
        .flat_map(|_| [run(QUICK, QUICK_MS), run(BUILD, BUILD_MS)])
        .collect()
}

#[test_case(RunOutcome::Completed, 500, Some(0); "instant")]
#[test_case(RunOutcome::Completed, 30_000, Some(1); "seconds")]
#[test_case(RunOutcome::Completed, 300_000, Some(2); "minutes")]
#[test_case(RunOutcome::Stopped, 30_000, None; "a_short_stop_says_nothing")]
#[test_case(RunOutcome::TimedOut, 300_000, Some(2); "a_long_timeout_is_minutes")]
fn runs_teach_the_class_they_prove(outcome: RunOutcome, elapsed_ms: u64, expected: Option<usize>) {
    assert_eq!(class_of(outcome, elapsed_ms), expected);
}

#[test]
fn a_quick_command_gets_no_eta() {
    let guess = Model::trained(&history()).guess(reading(QUICK));
    assert_eq!(guess.eta, None);
    let prediction = guess.prediction.unwrap();
    assert_eq!(usize::from(prediction.class), INSTANT);
    assert!(prediction.confidence >= INSTANT_BAR);
}

#[test_case(SECONDS, WORD_BAR, Some(SECONDS_WORD); "seconds_at_the_bar")]
#[test_case(MINUTES, WORD_BAR, Some(MINUTES_WORD); "minutes_at_the_bar")]
#[test_case(SECONDS, WORD_BAR - 0.01, None; "seconds_below_the_bar")]
#[test_case(INSTANT, INSTANT_BAR, None; "instant_has_no_word")]
fn a_word_needs_the_word_bar(class: usize, confidence: f64, expected: Option<&str>) {
    assert_eq!(class_word(class, confidence), expected);
}

#[test]
fn a_repeated_build_gets_its_own_time() {
    let guess = Model::trained(&history()).guess(reading(BUILD));
    assert_eq!(guess.eta.as_deref(), Some(BUILD_ETA));
}

#[test]
fn too_few_runs_train_no_trees() {
    let runs: Vec<ShellRun> = history().into_iter().take(REPEATS).collect();
    let guess = Model::trained(&runs).guess(reading(UNKNOWN));
    assert_eq!(guess.eta, None);
    assert_eq!(guess.prediction, None);
}

#[test]
fn the_first_forecast_trains_on_the_log_and_runs_are_recorded() {
    let temp = TempDir::new().unwrap();
    let state = StateDir::from_path(temp.path().to_path_buf());
    let runs = ShellRuns::open(&state).unwrap();
    for run in history() {
        runs.append(&run, None).unwrap();
    }
    let eta = ShellEta::new(state.clone());
    smol::block_on(async {
        let forecast = eta
            .forecast(&call(BUILD), None, Deadline::None)
            .await
            .unwrap();
        assert_eq!(forecast.eta(), Some(BUILD_ETA));
        eta.observe(forecast.guess, RunOutcome::Completed, BUILD_MS)
            .await;
    });
    deferred::flush(&state).unwrap();
    let stored = runs.newest(2 * REPEATS + 2).unwrap();
    assert_eq!(stored.len(), 2 * REPEATS + 1);
    assert_eq!(stored.last().map(|run| run.elapsed_ms), Some(BUILD_MS));
}

#[test_case(Timing::Exact(SLEEP_MS), Some(QUICK_MS), None, Some(SLEEP_ETA), Some(SLEEP_MS); "exact_overrides_instant")]
#[test_case(Timing::Exact(SLEEP_MS), Some(BUILD_MS), None, Some(SLEEP_ETA), Some(SLEEP_MS); "exact_overrides_longer")]
#[test_case(Timing::Exact(500), Some(BUILD_MS), None, None, Some(500); "short_sleep_hides_eta")]
#[test_case(Timing::AtLeast(SLEEP_MS), Some(QUICK_MS), None, Some(SLEEP_FLOOR), None; "floor_overrides_instant")]
#[test_case(Timing::AtLeast(SLEEP_MS), None, None, Some(SLEEP_FLOOR), None; "floor_without_numeric_prediction")]
#[test_case(Timing::AtLeast(1_500), None, None, Some("at least 1s500ms"), None; "fractional_floor_is_not_rounded_up")]
#[test_case(Timing::AtLeast(500), Some(QUICK_MS), None, None, None; "short_floor_clears_too_short_prediction")]
#[test_case(Timing::AtLeast(500), Some(BUILD_MS), Some(10), None, None; "short_floor_clears_impossible_prediction")]
#[test_case(Timing::AtLeast(SLEEP_MS), Some(BUILD_MS), None, Some(BUILD_ETA), Some(BUILD_MS); "longer_estimate_is_not_double_counted")]
#[test_case(Timing::Exact(SLEEP_MS), Some(BUILD_MS), Some(10), None, None; "exact_exceeds_deadline")]
#[test_case(Timing::Exact(SLEEP_MS), Some(BUILD_MS), Some(30), None, None; "exact_meets_deadline")]
#[test_case(Timing::AtLeast(SLEEP_MS), Some(BUILD_MS), Some(10), None, None; "floor_exceeds_deadline")]
#[test_case(Timing::AtLeast(SLEEP_MS), Some(BUILD_MS), Some(35), Some(SLEEP_FLOOR), None; "discard_impossible_learned_time")]
fn sleep_timing_takes_precedence(
    timing: Timing,
    estimate: Option<u64>,
    deadline: Option<u64>,
    expected: Option<&str>,
    predicted: Option<u64>,
) {
    let guess = Guess {
        reading: reading(SLEEP),
        host: None,
        prediction: estimate.map(|ms| Prediction {
            class: class_of(RunOutcome::Completed, ms).unwrap() as u8,
            confidence: 1.0,
            eta_ms: Some(ms),
        }),
        eta: None,
    }
    .with_sleep(timing, deadline.map(Duration::from_secs));
    assert_eq!(guess.eta.as_deref(), expected);
    assert_eq!(guess.prediction.as_ref().and_then(|p| p.eta_ms), predicted);
    if predicted.is_none() {
        assert_eq!(guess.prediction, None);
    }
}

#[test]
fn sleep_forecast_without_training_is_recorded() {
    let temp = TempDir::new().unwrap();
    let state = StateDir::from_path(temp.path().to_path_buf());
    let eta = ShellEta::new(state.clone());
    let host = HostSnapshot {
        cpu_usage: Some(0.75),
        host_cpus: Some(8),
        sample_age_ms: Some(50),
        ..Default::default()
    };
    smol::block_on(async {
        let mut forecast = eta
            .forecast(&call(SLEEP), None, Deadline::None)
            .await
            .unwrap();
        assert_eq!(forecast.eta(), Some(SLEEP_ETA));
        assert_eq!(
            forecast.guess.prediction.as_ref().unwrap().eta_ms,
            Some(SLEEP_MS)
        );
        forecast.guess.host = Some(host.clone());
        eta.observe(forecast.guess, RunOutcome::Completed, SLEEP_MS)
            .await;
    });
    deferred::flush(&state).unwrap();
    let mut expected = run(SLEEP, SLEEP_MS);
    expected.host = Some(host);
    assert_eq!(
        ShellRuns::open(&state).unwrap().newest(1).unwrap(),
        vec![expected]
    );
}

#[test]
fn unavailable_model_does_not_hide_literal_sleep() {
    let temp = TempDir::new().unwrap();
    let eta = ShellEta::new(StateDir::from_path(temp.path().to_path_buf()));
    smol::block_on(async {
        eta.0
            .model
            .get_or_init(|| async { None::<Mutex<Model>> })
            .await;
        assert_eq!(
            eta.forecast(&call(SLEEP), None, Deadline::None)
                .await
                .unwrap()
                .eta(),
            Some(SLEEP_ETA)
        );
        assert!(
            eta.forecast(&call(UNKNOWN), None, Deadline::None)
                .await
                .is_none()
        );
    });
}

#[test]
fn contradictory_training_does_not_change_literal_sleep() {
    let runs = vec![run(SLEEP, QUICK_MS); super::MIN_TRAINING_RUNS];
    let guess = Model::trained(&runs).guess(reading(SLEEP));
    assert_eq!(guess.prediction.as_ref().unwrap().class, INSTANT as u8);
    let guess = guess.with_sleep(analyze(SLEEP).unwrap().timing, None);
    assert_eq!(guess.eta.as_deref(), Some(SLEEP_ETA));
    assert_eq!(guess.prediction.unwrap().class, SECONDS as u8);
}

#[test]
fn an_expired_caller_deadline_suppresses_sleep_prediction() {
    let temp = TempDir::new().unwrap();
    let eta = ShellEta::new(StateDir::from_path(temp.path().to_path_buf()));
    smol::block_on(async {
        let forecast = eta
            .forecast(&call(SLEEP), None, Deadline::after(Duration::ZERO))
            .await
            .unwrap();
        assert_eq!(forecast.eta(), None);
        assert_eq!(forecast.guess.prediction, None);
    });
}

#[test_case(-1.0, true; "missing_sentinel")]
#[test_case(0.0, false; "observed_zero")]
#[test_case(2.0, false; "observed_positive")]
fn numeric_adapter_preserves_missingness(value: f32, missing: bool) {
    let mut reading = reading(UNKNOWN);
    reading.numbers.fill(value);
    let sample = super::sample(&HashMap::new(), &reading, [value; super::parts::FEATURES]);
    for number in sample.numbers {
        if missing {
            assert!(number.is_nan());
        } else {
            assert_eq!(number, value);
        }
    }
}

#[test_case("sleep 30 && make build"; "and_chain")]
#[test_case("sleep 10 && sleep 20 && make build"; "multiple_sleeps")]
#[test_case("sleep 30; make build"; "semicolon")]
#[test_case("sleep 30\nmake build"; "newline")]
fn sleep_adds_the_standalone_remainder_estimate(command: &str) {
    let temp = TempDir::new().unwrap();
    let state = StateDir::from_path(temp.path().to_path_buf());
    let eta = ShellEta::new(state.clone());
    let model = Model::trained(&vec![
        run(MAKE_BUILD, MAKE_BUILD_MS);
        super::MIN_TRAINING_RUNS
    ]);
    smol::block_on(async {
        eta.0
            .model
            .get_or_init(|| async { Some(Mutex::new(model)) })
            .await;
        let standalone = eta
            .forecast(&call(MAKE_BUILD), None, Deadline::None)
            .await
            .unwrap();
        let remainder = standalone.guess.prediction.unwrap().eta_ms.unwrap();
        let forecast = eta
            .forecast(&call(command), None, Deadline::None)
            .await
            .unwrap();
        assert_eq!(forecast.eta(), Some(ADDITIVE_ETA));
        let predicted = forecast.guess.prediction.as_ref().unwrap();
        assert_eq!(predicted.eta_ms, Some(SLEEP_MS + remainder));
        assert_eq!(predicted.class, SECONDS as u8);
        assert_eq!(predicted.confidence, super::UNCLASSIFIED_CONFIDENCE);
        assert_eq!(forecast.guess.reading, reading(command));
        eta.observe(forecast.guess, RunOutcome::Completed, ADDITIVE_MS)
            .await;
    });
    deferred::flush(&state).unwrap();
    let stored = ShellRuns::open(&state).unwrap().newest(2).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].reading, reading(command));
    assert_eq!(stored[0].elapsed_ms, ADDITIVE_MS);
}

#[test_case(Some(MAKE_BUILD_MS), None, Some(ADDITIVE_ETA), Some(ADDITIVE_MS); "sum_not_whole_command")]
#[test_case(Some(500), None, Some("~31s"), Some(30_500); "instant_remainder_still_contributes")]
#[test_case(None, None, Some(SLEEP_FLOOR), None; "unknown_remainder")]
#[test_case(Some(u64::MAX), None, Some(SLEEP_FLOOR), None; "overflow")]
#[test_case(Some(MAKE_BUILD_MS), Some(60), Some(SLEEP_FLOOR), None; "total_exceeds_deadline")]
#[test_case(Some(MAKE_BUILD_MS), Some(30), None, None; "sleep_meets_deadline")]
fn additive_prediction_falls_back_safely(
    remainder_ms: Option<u64>,
    deadline: Option<u64>,
    expected: Option<&str>,
    predicted_ms: Option<u64>,
) {
    let guess = Guess {
        reading: reading(SLEEP),
        host: None,
        eta: Some(BUILD_ETA.into()),
        prediction: Some(Prediction {
            class: SECONDS as u8,
            confidence: 1.0,
            eta_ms: Some(BUILD_MS),
        }),
    }
    .with_remainder(SLEEP_MS, remainder_ms, deadline.map(Duration::from_secs));
    assert_eq!(guess.eta.as_deref(), expected);
    assert_eq!(
        guess
            .prediction
            .as_ref()
            .and_then(|prediction| prediction.eta_ms),
        predicted_ms
    );
    if predicted_ms.is_none() {
        assert_eq!(guess.prediction, None);
    }
}

#[test]
fn additive_prediction_updates_duration_class() {
    let guess =
        Guess::empty(reading(SLEEP)).with_remainder(super::MINUTES_MS, Some(MAKE_BUILD_MS), None);
    let prediction = guess.prediction.unwrap();
    assert_eq!(prediction.class, MINUTES as u8);
    assert_eq!(prediction.eta_ms, Some(super::MINUTES_MS + MAKE_BUILD_MS));
}

#[test]
fn untrained_additive_forecast_keeps_the_sleep_floor() {
    let temp = TempDir::new().unwrap();
    let eta = ShellEta::new(StateDir::from_path(temp.path().to_path_buf()));
    smol::block_on(async {
        let forecast = eta
            .forecast(&call("sleep 30 && make build"), None, Deadline::None)
            .await
            .unwrap();
        assert_eq!(forecast.eta(), Some(SLEEP_FLOOR));
        assert_eq!(forecast.guess.prediction, None);
    });
}
