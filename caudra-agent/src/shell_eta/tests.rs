use caudra_storage::StateDir;
use caudra_storage::shell_runs::features::features;
use caudra_storage::shell_runs::{RunOutcome, ShellRun, ShellRuns};
use tempfile::TempDir;
use test_case::test_case;

use super::{Model, ShellEta, class_of};

const QUICK: &str = "git status";
const BUILD: &str = "cargo build --release";
const UNKNOWN: &str = "frobnicate --all";
const QUICK_MS: u64 = 20;
const BUILD_MS: u64 = 40_000;
const BUILD_ETA: &str = "ETA ~40s";
const REPEATS: usize = 30;

fn run(command: &str, elapsed_ms: u64) -> ShellRun {
    ShellRun {
        features: features(command),
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
    let forecast = Model::trained(history()).guess(features(QUICK));
    assert_eq!(forecast.eta, None);
    assert_eq!(forecast.prediction.class, 0);
    assert!(forecast.prediction.confidence >= super::CONFIDENCE_BAR);
}

#[test]
fn a_repeated_build_gets_its_own_time() {
    let forecast = Model::trained(history()).guess(features(BUILD));
    assert_eq!(forecast.eta.as_deref(), Some(BUILD_ETA));
    assert_eq!(forecast.prediction.eta_ms, Some(BUILD_MS));
}

#[test]
fn an_untrained_model_says_nothing() {
    let forecast = Model::trained(Vec::new()).guess(features(UNKNOWN));
    assert_eq!(forecast.eta, None);
    assert_eq!(forecast.prediction.eta_ms, None);
}

#[test]
fn an_observed_run_is_learned_and_recorded() {
    let temp = TempDir::new().unwrap();
    let state = StateDir::from_path(temp.path().to_path_buf());
    let eta = ShellEta::new(state.clone());
    smol::block_on(async {
        for _ in 0..REPEATS {
            let forecast = eta.forecast(BUILD).await.unwrap();
            eta.observe(forecast.guess, RunOutcome::Completed, BUILD_MS)
                .await;
        }
        let forecast = eta.forecast(BUILD).await.unwrap();
        assert_eq!(forecast.eta(), Some(BUILD_ETA));
    });
    let stored = ShellRuns::open(&state)
        .unwrap()
        .newest(REPEATS + 1)
        .unwrap();
    assert_eq!(stored.len(), REPEATS);
    assert!(stored.iter().all(|run| run.elapsed_ms == BUILD_MS));
}
