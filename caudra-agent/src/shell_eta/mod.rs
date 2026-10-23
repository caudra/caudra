//! Shell ETAs from a small model trained on this machine's own shell runs.
//!
//! The model is never saved: the first shell call of a process rebuilds it
//! from the newest runs in `shell_runs`, so startup never pays for it, and every finished run teaches it once more
//! and joins that log. A classifier says whether a command exits at once,
//! takes seconds or takes minutes; similar earlier runs give a time to show.
//! It only labels the card: timeouts and delivery never depend on it.

mod classifier;
mod neighbours;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use async_lock::OnceCell;
use caudra_storage::StateDir;
use caudra_storage::shell_runs::features::{BIAS, features};
use caudra_storage::shell_runs::{MAX_SHELL_RUNS, Prediction, RunOutcome, ShellRun, ShellRuns};

use crate::ToolOutput;
use crate::tools::{ToolExecResult, humanize_duration};
use classifier::{CLASSES, Classifier, Probabilities};
use neighbours::Neighbours;

const INSTANT: usize = 0;
const SECONDS: usize = 1;
const MINUTES: usize = 2;
const INSTANT_MS: u64 = 1_000;
const MINUTES_MS: u64 = 120_000;
const MILLIS_PER_SECOND: u64 = 1_000;
/// Probability of exiting at once that hides the ETA, kept high so a long
/// run is rarely left unlabelled.
const INSTANT_BAR: f64 = 0.9;
/// Probability that shows `seconds` or `minutes`. In recorded runs those
/// words were right about 85% of the time from here, and little better than
/// a guess below 0.7.
const WORD_BAR: f64 = 0.75;
/// Similarity the nearest earlier run needs before its time is shown.
const NEIGHBOUR_TRUST: f64 = 0.5;
const NEIGHBOUR_CAPACITY: usize = 5_000;
const ETA_PREFIX: &str = "ETA ";
const APPROXIMATELY: &str = "~";
const SECONDS_WORD: &str = "seconds";
const MINUTES_WORD: &str = "minutes";

/// One model per state dir and process, shared by every session and subagent
/// that runs a local shell, and kept for the life of the process.
#[derive(Clone)]
pub struct ShellEta(Arc<Inner>);

struct Inner {
    state_dir: StateDir,
    model: OnceCell<Option<Mutex<Model>>>,
}

/// What the model expects of one call, kept until the call finishes so the
/// run is recorded beside the prediction it got.
pub struct Forecast {
    owner: ShellEta,
    guess: Guess,
}

struct Guess {
    features: Vec<u64>,
    prediction: Prediction,
    eta: Option<String>,
}

impl Forecast {
    /// The card annotation: nothing for a command expected to exit at once or
    /// one the model is unsure of.
    pub fn eta(&self) -> Option<&str> {
        self.guess.eta.as_deref()
    }

    /// Called as the command starts, so a call refused before then is never
    /// recorded.
    pub fn begin(self) -> ShellEtaRun {
        ShellEtaRun {
            owner: self.owner,
            guess: Some(self.guess),
            started: Instant::now(),
        }
    }
}

struct Model {
    classifier: Classifier,
    neighbours: Neighbours,
}

impl Model {
    fn trained(runs: Vec<ShellRun>) -> Self {
        let mut model = Self {
            classifier: Classifier::new(),
            neighbours: Neighbours::new(NEIGHBOUR_CAPACITY),
        };
        for run in runs {
            model.learn(run.features, run.outcome, run.elapsed_ms);
        }
        model
    }

    fn learn(&mut self, features: Vec<u64>, outcome: RunOutcome, elapsed_ms: u64) {
        if let Some(class) = class_of(outcome, elapsed_ms) {
            self.classifier.learn(&features, class);
        }
        if outcome == RunOutcome::Completed {
            self.neighbours.insert(query(features), log2_ms(elapsed_ms));
        }
    }

    fn guess(&self, features: Vec<u64>) -> Guess {
        let probabilities = self.classifier.predict(&features);
        let (class, confidence) = most_likely(&probabilities);
        let instant = class == INSTANT && confidence >= INSTANT_BAR;
        let neighbour_ms = (!instant)
            .then(|| self.neighbours.estimate(&query(features.clone())))
            .flatten()
            .filter(|estimate| estimate.similarity >= NEIGHBOUR_TRUST)
            .map(|estimate| (estimate.log2_ms.exp2() - 1.0).max(0.0).round() as u64);
        let eta = match neighbour_ms {
            Some(ms) => (ms > INSTANT_MS).then(|| {
                let seconds = Duration::from_secs(ms.div_ceil(MILLIS_PER_SECOND));
                format!("{ETA_PREFIX}{APPROXIMATELY}{}", humanize_duration(seconds))
            }),
            None => class_word(class, confidence).map(|word| format!("{ETA_PREFIX}{word}")),
        };
        Guess {
            features,
            prediction: Prediction {
                class: u8::try_from(class).unwrap_or_default(),
                confidence,
                eta_ms: neighbour_ms,
            },
            eta,
        }
    }
}

impl ShellEta {
    pub fn shared(state_dir: &StateDir) -> Self {
        static MODELS: OnceLock<Mutex<HashMap<PathBuf, ShellEta>>> = OnceLock::new();
        lock(MODELS.get_or_init(Mutex::default))
            .entry(state_dir.persistent_path().to_path_buf())
            .or_insert_with(|| Self::new(state_dir.clone()))
            .clone()
    }

    fn new(state_dir: StateDir) -> Self {
        Self(Arc::new(Inner {
            state_dir,
            model: OnceCell::new(),
        }))
    }

    /// The first call trains the model and waits for it. `None` only when
    /// the run log could not be read.
    pub async fn forecast(&self, command: &str) -> Option<Forecast> {
        let model = self.model().await?;
        let guess = lock(model).guess(features(command));
        Some(Forecast {
            owner: self.clone(),
            guess,
        })
    }

    async fn observe(&self, guess: Guess, outcome: RunOutcome, elapsed_ms: u64) {
        let Some(model) = self.model().await else {
            return;
        };
        let run = ShellRun {
            features: guess.features,
            elapsed_ms,
            outcome,
        };
        lock(model).learn(run.features.clone(), outcome, elapsed_ms);
        let dir = self.0.state_dir.clone();
        let prediction = guess.prediction;
        if let Err(error) =
            smol::unblock(move || ShellRuns::open(&dir)?.append(&run, Some(&prediction))).await
        {
            tracing::warn!(%error, "shell run could not be recorded");
        }
    }

    async fn model(&self) -> Option<&Mutex<Model>> {
        self.0
            .model
            .get_or_init(|| async {
                let dir = self.0.state_dir.clone();
                let started = Instant::now();
                match smol::unblock(move || ShellRuns::open(&dir)?.newest(MAX_SHELL_RUNS)).await {
                    Ok(runs) => {
                        let count = runs.len();
                        let model = Model::trained(runs);
                        tracing::debug!(
                            runs = count,
                            elapsed_ms = started.elapsed().as_millis(),
                            "shell eta model trained"
                        );
                        Some(Mutex::new(model))
                    }
                    Err(error) => {
                        tracing::warn!(%error, "shell runs unavailable; no shell ETAs");
                        None
                    }
                }
            })
            .await
            .as_ref()
    }
}

/// A started shell call. Finishing records how it ended; dropping it records
/// a stop, since the call was abandoned on its way.
pub struct ShellEtaRun {
    owner: ShellEta,
    guess: Option<Guess>,
    started: Instant,
}

impl ShellEtaRun {
    pub async fn finish(mut self, result: &ToolExecResult, cancelled: bool) {
        let measured = match &result.output {
            Ok(ToolOutput::Shell(shell)) => {
                let outcome = if shell.timed_out {
                    Some(RunOutcome::TimedOut)
                } else if cancelled || shell.signal.is_some() || shell.output_limit_exceeded {
                    Some(RunOutcome::Stopped)
                } else {
                    shell.exit_code.map(|_| RunOutcome::Completed)
                };
                outcome.map(|outcome| (outcome, shell.duration_ms))
            }
            _ if cancelled => Some((RunOutcome::Stopped, self.elapsed_ms())),
            _ => None,
        };
        if let Some(guess) = self.guess.take()
            && let Some((outcome, elapsed_ms)) = measured
        {
            self.owner.observe(guess, outcome, elapsed_ms).await;
        }
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

impl Drop for ShellEtaRun {
    fn drop(&mut self) {
        if !thread::panicking()
            && let Some(guess) = self.guess.take()
        {
            let elapsed_ms = self.elapsed_ms();
            let owner = self.owner.clone();
            smol::spawn(async move { owner.observe(guess, RunOutcome::Stopped, elapsed_ms).await })
                .detach();
        }
    }
}

/// A run that was stopped only says it lasted at least this long, so it
/// teaches the classifier only once that already means minutes.
fn class_of(outcome: RunOutcome, elapsed_ms: u64) -> Option<usize> {
    match outcome {
        RunOutcome::Completed if elapsed_ms <= INSTANT_MS => Some(INSTANT),
        RunOutcome::Completed if elapsed_ms < MINUTES_MS => Some(SECONDS),
        _ if elapsed_ms >= MINUTES_MS => Some(MINUTES),
        _ => None,
    }
}

fn class_word(class: usize, confidence: f64) -> Option<&'static str> {
    match class {
        SECONDS if confidence >= WORD_BAR => Some(SECONDS_WORD),
        MINUTES if confidence >= WORD_BAR => Some(MINUTES_WORD),
        _ => None,
    }
}

/// Every command shares the bias feature, so it says nothing about similarity.
fn query(mut features: Vec<u64>) -> Vec<u64> {
    features.retain(|feature| *feature != BIAS);
    features
}

fn log2_ms(elapsed_ms: u64) -> f64 {
    (elapsed_ms as f64 + 1.0).log2()
}

fn most_likely(probabilities: &Probabilities) -> (usize, f64) {
    (0..CLASSES).fold((INSTANT, f64::MIN), |best, class| {
        if probabilities[class] > best.1 {
            (class, probabilities[class])
        } else {
            best
        }
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests;
