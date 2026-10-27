//! Shell ETAs from small models trained on this machine's own shell runs.
//!
//! Nothing is saved but the run log: the first shell call of a process trains
//! the models from the newest runs in `shell_runs`, so startup never pays for
//! them, and every few hundred runs they are trained again in the background.
//! Each command is read into facts (sleeps, timeouts, chains, programs and
//! flags); gradient-boosted trees turn them into the chance it exits at once,
//! takes seconds or takes minutes, and into a range for its time. A table of
//! part times, learned after every run, adds up a new chain of known commands.
//! It only labels the card: timeouts and delivery never depend on it.

mod gbdt;
mod host;
mod parts;
mod sleep;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use async_lock::OnceCell;
use caudra_storage::StateDir;
use caudra_storage::sessions::SessionError;
use caudra_storage::shell_runs::reading::{Call, NUMBERS, Reading, read};
use caudra_storage::shell_runs::{
    HostSnapshot, MAX_SHELL_RUNS, Prediction, RunOutcome, ShellRun, ShellRuns,
};

use crate::ToolOutput;
use crate::tools::{Deadline, ToolExecResult, humanize_duration};
use gbdt::{CLASSES, Estimate, Forest, Sample};
use parts::Parts;
use sleep::{Analysis, Timing};

const INSTANT: usize = 0;
const SECONDS: usize = 1;
const MINUTES: usize = 2;
const INSTANT_MS: u64 = 1_000;
const MINUTES_MS: u64 = 120_000;
const MILLIS_PER_SECOND: u64 = 1_000;
/// Probability of exiting at once that hides the ETA, kept high so a long
/// run is rarely left unlabelled.
const INSTANT_BAR: f64 = 0.9;
/// Probability that shows `seconds` or `minutes`.
const WORD_BAR: f64 = 0.8;
/// Widest gap, in doublings, between the 10th and 90th percentile time that
/// still shows the median as a time.
const SPREAD_BAR: f64 = 2.0;
const NUMERIC: usize = NUMBERS + parts::FEATURES;
/// Most frequent tokens the trees may split on.
const VOCABULARY: usize = 300;
/// Labelled and finished runs each needed before the trees are trained.
pub(crate) const MIN_TRAINING_RUNS: usize = 100;
const RETRAIN_EVERY: usize = 500;
/// How often an untrained model looks again for enough runs.
const RETRY_UNTRAINED_EVERY: usize = 50;
pub const ETA_LABEL: &str = "ETA";
const APPROXIMATELY: &str = "~";
const AT_LEAST: &str = "at least ";
const UNCLASSIFIED_CONFIDENCE: f64 = 0.0;
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
    reading: Reading,
    host: Option<HostSnapshot>,
    prediction: Option<Prediction>,
    eta: Option<String>,
}

impl Guess {
    fn empty(reading: Reading) -> Self {
        Self {
            reading,
            host: None,
            prediction: None,
            eta: None,
        }
    }

    fn with_remainder(
        mut self,
        sleep_ms: u64,
        remainder_ms: Option<u64>,
        deadline: Option<Duration>,
    ) -> Self {
        self.prediction = remainder_ms
            .and_then(|remaining| sleep_ms.checked_add(remaining))
            .map(|total| Prediction {
                class: u8::try_from(class_of(RunOutcome::Completed, total).unwrap_or(INSTANT))
                    .unwrap_or_default(),
                confidence: UNCLASSIFIED_CONFIDENCE,
                eta_ms: Some(total),
            });
        self.with_sleep(Timing::AtLeast(sleep_ms), deadline)
    }

    fn with_sleep(mut self, timing: Timing, deadline: Option<Duration>) -> Self {
        let (Timing::Exact(ms) | Timing::AtLeast(ms)) = timing;
        if deadline.is_some_and(|limit| Duration::from_millis(ms) >= limit) {
            self.prediction = None;
            self.eta = None;
            return self;
        }
        match timing {
            Timing::Exact(ms) => {
                self.eta = time_label(ms, APPROXIMATELY);
                self.prediction = Some(Prediction {
                    class: if ms <= INSTANT_MS {
                        INSTANT as u8
                    } else if ms < MINUTES_MS {
                        SECONDS as u8
                    } else {
                        MINUTES as u8
                    },
                    confidence: 1.0,
                    eta_ms: Some(ms),
                });
            }
            Timing::AtLeast(ms) => {
                let estimate = self
                    .prediction
                    .as_ref()
                    .and_then(|prediction| prediction.eta_ms);
                if let Some(estimate) = estimate.filter(|estimate| {
                    *estimate >= ms
                        && deadline.is_none_or(|limit| Duration::from_millis(*estimate) < limit)
                }) {
                    self.eta = time_label(estimate, APPROXIMATELY);
                } else {
                    self.prediction = None;
                    self.eta = time_label(ms, AT_LEAST);
                }
            }
        }
        self
    }
}

fn time_label(ms: u64, prefix: &str) -> Option<String> {
    (ms > INSTANT_MS).then(|| {
        let duration = if prefix == AT_LEAST {
            Duration::from_millis(ms)
        } else {
            Duration::from_secs(ms.div_ceil(MILLIS_PER_SECOND))
        };
        format!("{prefix}{}", humanize_duration(duration))
    })
}

/// An estimate as the card says it, such as `ETA ~40s`.
pub fn eta_label(estimate: &str) -> String {
    format!("{ETA_LABEL} {estimate}")
}

impl Forecast {
    /// The estimate without its label, such as `~40s`: nothing for a command
    /// expected to exit at once or one the model is unsure of.
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

struct Trees {
    vocabulary: HashMap<u64, u16>,
    forest: Forest,
}

struct Model {
    parts: Parts,
    trees: Option<Trees>,
    runs_since_training: usize,
    training: bool,
}

impl Model {
    /// Replays the runs in order, so each one meets the part times known
    /// before it, then fits the trees on what that replay saw.
    fn trained(runs: &[ShellRun]) -> Self {
        let mut parts = Parts::default();
        let part_features: Vec<[f32; parts::FEATURES]> = runs
            .iter()
            .map(|run| {
                let features = parts.features(run.reading.steps.as_deref());
                if let Some(steps) = &run.reading.steps
                    && run.outcome == RunOutcome::Completed
                {
                    parts.learn(steps, run.elapsed_ms);
                }
                features
            })
            .collect();
        Self {
            parts,
            trees: fit(runs, &part_features),
            runs_since_training: 0,
            training: false,
        }
    }

    fn learn(&mut self, reading: &Reading, outcome: RunOutcome, elapsed_ms: u64) {
        if let Some(steps) = &reading.steps
            && outcome == RunOutcome::Completed
        {
            self.parts.learn(steps, elapsed_ms);
        }
        self.runs_since_training += 1;
    }

    fn retrain_due(&self) -> bool {
        let every = if self.trees.is_some() {
            RETRAIN_EVERY
        } else {
            RETRY_UNTRAINED_EVERY
        };
        !self.training && self.runs_since_training >= every
    }

    /// Takes a fresh training's trees and part times.
    fn replace(&mut self, fresh: Model) {
        self.trees = fresh.trees;
        self.parts = fresh.parts;
        self.runs_since_training = 0;
        self.training = false;
    }

    fn estimate(&self, reading: &Reading) -> Option<Estimate> {
        let trees = self.trees.as_ref()?;
        let sample = sample(
            &trees.vocabulary,
            reading,
            self.parts.features(reading.steps.as_deref()),
        );
        Some(trees.forest.estimate(&sample))
    }

    fn guess(&self, reading: Reading) -> Guess {
        let Some(estimate) = self.estimate(&reading) else {
            return Guess::empty(reading);
        };
        let (class, confidence) = most_likely(&estimate.probabilities);
        let instant = class == INSTANT && confidence >= INSTANT_BAR;
        let time_ms = (!instant).then(|| median_ms(&estimate)).flatten();
        let eta = if instant {
            None
        } else if let Some(ms) = time_ms {
            time_label(ms, APPROXIMATELY)
        } else {
            class_word(class, confidence).map(str::to_owned)
        };
        Guess {
            reading,
            host: None,
            prediction: Some(Prediction {
                class: u8::try_from(class).unwrap_or_default(),
                confidence,
                eta_ms: time_ms,
            }),
            eta,
        }
    }
}

/// The trees, once there are enough runs of each kind to fit them.
fn fit(runs: &[ShellRun], part_features: &[[f32; parts::FEATURES]]) -> Option<Trees> {
    let vocabulary = vocabulary(runs);
    let samples: Vec<Sample<NUMERIC>> = runs
        .iter()
        .zip(part_features)
        .map(|(run, parts)| sample(&vocabulary, &run.reading, *parts))
        .collect();
    let labelled: Vec<(&Sample<NUMERIC>, usize)> = runs
        .iter()
        .zip(&samples)
        .filter_map(|(run, sample)| Some((sample, class_of(run.outcome, run.elapsed_ms)?)))
        .collect();
    let timed: Vec<(&Sample<NUMERIC>, f32)> = runs
        .iter()
        .zip(&samples)
        .filter(|(run, _)| run.outcome == RunOutcome::Completed)
        .map(|(run, sample)| (sample, log2_ms(run.elapsed_ms)))
        .collect();
    if labelled.len() < MIN_TRAINING_RUNS || timed.len() < MIN_TRAINING_RUNS {
        return None;
    }
    let forest = Forest::fit(labelled, timed, vocabulary.len());
    Some(Trees { vocabulary, forest })
}

fn sample(
    vocabulary: &HashMap<u64, u16>,
    reading: &Reading,
    parts: [f32; parts::FEATURES],
) -> Sample<NUMERIC> {
    let mut numbers = [0.0; NUMERIC];
    numbers[..NUMBERS].copy_from_slice(&reading.numbers);
    numbers[NUMBERS..].copy_from_slice(&parts);
    for number in &mut numbers {
        if *number < 0.0 {
            *number = f32::NAN;
        }
    }
    let mut tokens: Vec<u16> = reading
        .tokens
        .iter()
        .filter_map(|token| vocabulary.get(token).copied())
        .collect();
    tokens.sort_unstable();
    Sample { numbers, tokens }
}

/// The [`VOCABULARY`] tokens most runs share, numbered most common first.
fn vocabulary(runs: &[ShellRun]) -> HashMap<u64, u16> {
    let mut counts: HashMap<u64, usize> = HashMap::new();
    for run in runs {
        for token in &run.reading.tokens {
            *counts.entry(*token).or_default() += 1;
        }
    }
    let mut ranked: Vec<(u64, usize)> = counts.into_iter().collect();
    ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked
        .into_iter()
        .take(VOCABULARY)
        .zip(0u16..)
        .map(|((token, _), id)| (token, id))
        .collect()
}

/// The median time, when the 10th and 90th percentiles agree closely enough
/// for it to mean something.
fn median_ms(estimate: &Estimate) -> Option<u64> {
    let [low, median, high] = estimate.log2_ms;
    (high - low <= SPREAD_BAR).then(|| (median.exp2() - 1.0).max(0.0).round() as u64)
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

    pub async fn forecast(
        &self,
        call: &Call<'_>,
        timeout: Option<Duration>,
        deadline: Deadline,
    ) -> Option<Forecast> {
        let analysis = sleep::analyze(call.command);
        let reading = read(call);
        let (guess, remainder_ms) = match self.model().await {
            Some(model) => {
                let model = lock(model);
                match analysis.as_ref().and_then(|analysis| analysis.remainder) {
                    Some(command) => {
                        let remainder_ms = model
                            .estimate(&read(&Call { command, ..*call }))
                            .and_then(|estimate| median_ms(&estimate));
                        (Guess::empty(reading), remainder_ms)
                    }
                    None => (model.guess(reading), None),
                }
            }
            None if analysis.is_some() => (Guess::empty(reading), None),
            None => return None,
        };
        let limit = [
            timeout,
            deadline.remaining().unwrap_or(Some(Duration::ZERO)),
        ]
        .into_iter()
        .flatten()
        .min();
        let mut guess = match analysis {
            Some(Analysis {
                timing: Timing::AtLeast(ms),
                remainder: Some(_),
            }) => guess.with_remainder(ms, remainder_ms, limit),
            Some(Analysis { timing, .. }) => guess.with_sleep(timing, limit),
            None => guess,
        };
        guess.host = host::snapshot();
        Some(Forecast {
            owner: self.clone(),
            guess,
        })
    }

    async fn observe(&self, guess: Guess, outcome: RunOutcome, elapsed_ms: u64) {
        let Some(model) = self.model().await else {
            return;
        };
        lock(model).learn(&guess.reading, outcome, elapsed_ms);
        let run = ShellRun {
            reading: guess.reading,
            host: guess.host,
            elapsed_ms,
            outcome,
        };
        let dir = self.0.state_dir.clone();
        let prediction = guess.prediction;
        if let Err(error) =
            smol::unblock(move || ShellRuns::open(&dir)?.append(&run, prediction.as_ref())).await
        {
            tracing::warn!(%error, "shell run could not be recorded");
        }
        let due = {
            let mut model = lock(model);
            let due = model.retrain_due();
            model.training |= due;
            due
        };
        if due {
            let owner = self.clone();
            smol::spawn(async move { owner.retrain().await }).detach();
        }
    }

    async fn retrain(&self) {
        let Some(model) = self.model().await else {
            return;
        };
        match self.train().await {
            Some(fresh) => lock(model).replace(fresh),
            None => lock(model).training = false,
        }
    }

    async fn train(&self) -> Option<Model> {
        let dir = self.0.state_dir.clone();
        let started = Instant::now();
        let trained = smol::unblock(move || {
            let runs = ShellRuns::open(&dir)?.newest(MAX_SHELL_RUNS)?;
            Ok::<_, SessionError>((runs.len(), Model::trained(&runs)))
        })
        .await;
        match trained {
            Ok((runs, model)) => {
                tracing::debug!(
                    runs,
                    trees = model.trees.is_some(),
                    elapsed_ms = started.elapsed().as_millis(),
                    "shell eta model trained"
                );
                Some(model)
            }
            Err(error) => {
                tracing::warn!(%error, "shell runs unavailable; no shell ETAs");
                None
            }
        }
    }

    async fn model(&self) -> Option<&Mutex<Model>> {
        self.0
            .model
            .get_or_init(|| async { self.train().await.map(Mutex::new) })
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

fn log2_ms(elapsed_ms: u64) -> f32 {
    (elapsed_ms as f64 + 1.0).log2() as f32
}

fn most_likely(probabilities: &[f64; CLASSES]) -> (usize, f64) {
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
