use caudra_storage::StateDir;
use caudra_storage::shell_runs::reading::{Call, Reading, read};
use caudra_storage::shell_runs::{RunOutcome, ShellRun, ShellRuns};
use tempfile::TempDir;
use test_case::test_case;

use super::{
    INSTANT, INSTANT_BAR, MINUTES, MINUTES_WORD, Model, SECONDS, SECONDS_WORD, ShellEta, WORD_BAR,
    class_of, class_word,
};

const QUICK: &str = "git status";
const BUILD: &str = "cargo build --release";
const UNKNOWN: &str = "frobnicate --all";
const QUICK_MS: u64 = 20;
const BUILD_MS: u64 = 39_500;
const BUILD_ETA: &str = "~40s";
const REPEATS: usize = 60;

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
        let forecast = eta.forecast(&call(BUILD)).await.unwrap();
        assert_eq!(forecast.eta(), Some(BUILD_ETA));
        eta.observe(forecast.guess, RunOutcome::Completed, BUILD_MS)
            .await;
    });
    let stored = runs.newest(2 * REPEATS + 2).unwrap();
    assert_eq!(stored.len(), 2 * REPEATS + 1);
    assert_eq!(stored.last().map(|run| run.elapsed_ms), Some(BUILD_MS));
}
