use std::collections::VecDeque;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(any(target_os = "linux", test))]
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use caudra_storage::shell_runs::HostSnapshot;
use sysinfo::{CpuRefreshKind, MINIMUM_CPU_UPDATE_INTERVAL, System};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const MAX_SAMPLE_AGE: Duration = Duration::from_secs(5);
const MAX_COUNTER_INTERVAL: Duration = Duration::from_secs(5);
const CPU_USAGE_WINDOW: Duration = Duration::from_secs(10);
const MAX_CPU_INTERVALS: usize =
    (CPU_USAGE_WINDOW.as_nanos() / SAMPLE_INTERVAL.as_nanos()) as usize + 1;
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const PERCENT_SCALE: f32 = 100.0;
const WORKER_NAME: &str = "shell-host-sampler";
#[cfg(target_os = "linux")]
const PRESSURE_PATH: &str = "/proc/pressure/cpu";
#[cfg(any(target_os = "linux", test))]
const MAX_PRESSURE_BYTES: usize = 4096;
static SAMPLER: Sampler = Sampler::new();

struct Sampler {
    state: Mutex<State>,
    running: AtomicBool,
}

struct State {
    last_request: Option<Instant>,
    sample: Option<CachedSnapshot>,
}

struct CachedSnapshot {
    value: HostSnapshot,
    sampled_at: Instant,
    completed_at: Instant,
}

struct WorkerLease<'a> {
    sampler: &'a Sampler,
    active: bool,
}

struct CpuWindow {
    started: Instant,
    finished: Instant,
}

struct CpuBaseline {
    window: CpuWindow,
    host_cpus: u32,
}

struct CpuInterval {
    started: Instant,
    finished: Instant,
    usage: f32,
}

#[derive(Default)]
struct CpuHistory {
    previous: Option<CpuBaseline>,
    intervals: VecDeque<CpuInterval>,
}

pub(super) fn snapshot() -> Option<HostSnapshot> {
    if !sysinfo::IS_SUPPORTED_SYSTEM {
        return None;
    }
    let (sample, worker) = SAMPLER.request(Instant::now());
    if let Some(worker) = worker
        && let Err(error) = thread::Builder::new()
            .name(WORKER_NAME.into())
            .spawn(move || run(worker))
    {
        tracing::debug!(%error, "could not start shell host sampler");
    }
    sample
}

impl Sampler {
    const fn new() -> Self {
        Self {
            state: Mutex::new(State {
                last_request: None,
                sample: None,
            }),
            running: AtomicBool::new(false),
        }
    }

    fn request(&self, now: Instant) -> (Option<HostSnapshot>, Option<WorkerLease<'_>>) {
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return (None, None),
        };
        state.last_request = Some(state.last_request.map_or(now, |last| last.max(now)));
        let worker = if self
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            state.sample = None;
            Some(WorkerLease {
                sampler: self,
                active: true,
            })
        } else {
            None
        };
        let sample = state.sample.as_ref().and_then(|sample| sample.at(now));
        (sample, worker)
    }
}

impl CachedSnapshot {
    fn at(&self, now: Instant) -> Option<HostSnapshot> {
        if now < self.completed_at {
            return None;
        }
        let age = now.checked_duration_since(self.sampled_at)?;
        if age > MAX_SAMPLE_AGE {
            return None;
        }
        Some(HostSnapshot {
            cpu_usage: self.value.cpu_usage,
            cpu_usage_mean: self.value.cpu_usage_mean,
            cpu_usage_window_ms: self.value.cpu_usage_window_ms,
            normalized_load: self.value.normalized_load,
            cpu_pressure: self.value.cpu_pressure,
            host_cpus: self.value.host_cpus,
            available_cpus: self.value.available_cpus,
            sample_age_ms: self
                .value
                .cpu_usage
                .and_then(|_| age.as_millis().try_into().ok()),
        })
    }
}

impl WorkerLease<'_> {
    fn stop_if_idle(&mut self, now: Instant) -> bool {
        let sampler = self.sampler;
        let state = sampler
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let idle = state.last_request.is_none_or(|last_request| {
            now.checked_duration_since(last_request)
                .is_some_and(|age| age >= IDLE_TIMEOUT)
        });
        if idle {
            self.release();
        }
        idle
    }

    fn release(&mut self) {
        if self.active {
            self.sampler.running.store(false, Ordering::Release);
            self.active = false;
        }
    }
}

impl Drop for WorkerLease<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

fn run(mut worker: WorkerLease<'static>) {
    let mut system = System::new();
    let mut history = CpuHistory::default();
    loop {
        if worker.stop_if_idle(Instant::now()) {
            return;
        }
        let sample = measure(&mut system, &mut history);
        worker
            .sampler
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .sample = Some(sample);
        thread::sleep(SAMPLE_INTERVAL.max(MINIMUM_CPU_UPDATE_INTERVAL));
    }
}

fn measure(system: &mut System, history: &mut CpuHistory) -> CachedSnapshot {
    let started = Instant::now();
    if online_cpu_count().is_some_and(|count| Some(count) != cpu_count(system.cpus().len())) {
        system.refresh_cpu_list(CpuRefreshKind::nothing().with_cpu_usage());
        history.clear();
    } else {
        system.refresh_cpu_usage();
    }
    let current = CpuWindow {
        started,
        finished: Instant::now(),
    };
    let sampled_at = current.finished;
    let host_cpus = cpu_count(system.cpus().len());
    let cpu_usage = history.record(host_cpus, system.global_cpu_usage(), current);
    let (cpu_usage_mean, cpu_usage_window_ms) =
        history.summary().map_or((None, None), |(mean, coverage)| {
            (Some(mean), Some(coverage))
        });
    CachedSnapshot {
        value: HostSnapshot {
            cpu_usage,
            cpu_usage_mean,
            cpu_usage_window_ms,
            normalized_load: host_cpus.and_then(|cpus| normalized_load(load_average()?, cpus)),
            cpu_pressure: pressure(),
            host_cpus,
            available_cpus: thread::available_parallelism()
                .ok()
                .and_then(|cpus| cpu_count(cpus.get())),
            sample_age_ms: None,
        },
        sampled_at,
        completed_at: Instant::now(),
    }
}

impl CpuHistory {
    fn record(&mut self, host_cpus: Option<u32>, percent: f32, current: CpuWindow) -> Option<f32> {
        let Some(host_cpus) = host_cpus.filter(|count| *count > 0).filter(|_| {
            percent_fraction(percent).is_some()
                && current
                    .finished
                    .checked_duration_since(current.started)
                    .is_some_and(|duration| duration <= MAX_COUNTER_INTERVAL)
        }) else {
            self.clear();
            return None;
        };
        let previous = self.previous.take();
        let usage = match previous {
            Some(previous) if previous.host_cpus == host_cpus => {
                let Some(usage) = usage_in_window(percent, Some(&previous.window), &current) else {
                    self.clear();
                    return None;
                };
                self.push(
                    CpuInterval {
                        started: previous.window.finished,
                        finished: current.started,
                        usage,
                    },
                    current.finished,
                );
                Some(usage)
            }
            _ => {
                self.intervals.clear();
                None
            }
        };
        self.previous = Some(CpuBaseline {
            window: current,
            host_cpus,
        });
        usage
    }

    fn clear(&mut self) {
        self.previous = None;
        self.intervals.clear();
    }

    fn push(&mut self, mut interval: CpuInterval, now: Instant) {
        if let Some(previous) = self.intervals.back() {
            interval.started = interval.started.max(previous.finished);
        }
        if interval.finished > interval.started {
            self.intervals.push_back(interval);
        }
        if let Some(edge) = now.checked_sub(CPU_USAGE_WINDOW) {
            while self
                .intervals
                .front()
                .is_some_and(|old| old.finished <= edge)
            {
                self.intervals.pop_front();
            }
            if let Some(oldest) = self.intervals.front_mut() {
                oldest.started = oldest.started.max(edge);
            }
        }
        while self.intervals.len() > MAX_CPU_INTERVALS {
            self.intervals.pop_front();
        }
    }

    fn summary(&self) -> Option<(f32, u64)> {
        let mut weighted_usage = 0.0;
        let mut coverage = Duration::ZERO;
        for interval in &self.intervals {
            let duration = interval.finished.duration_since(interval.started);
            weighted_usage += f64::from(interval.usage) * duration.as_secs_f64();
            coverage += duration;
        }
        if coverage.is_zero() {
            return None;
        }
        Some((
            (weighted_usage / coverage.as_secs_f64()) as f32,
            coverage.as_millis().try_into().ok()?,
        ))
    }
}

fn cpu_count(count: usize) -> Option<u32> {
    u32::try_from(count).ok().filter(|count| *count > 0)
}

#[cfg(target_os = "linux")]
fn online_cpu_count() -> Option<u32> {
    let count = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    u32::try_from(count).ok().filter(|count| *count > 0)
}

#[cfg(not(target_os = "linux"))]
fn online_cpu_count() -> Option<u32> {
    None
}

fn usage_in_window(percent: f32, previous: Option<&CpuWindow>, current: &CpuWindow) -> Option<f32> {
    let previous = previous?;
    let shortest = current.started.checked_duration_since(previous.finished)?;
    let longest = current.finished.checked_duration_since(previous.started)?;
    if shortest < MINIMUM_CPU_UPDATE_INTERVAL || longest > MAX_COUNTER_INTERVAL {
        return None;
    }
    percent_fraction(percent)
}

fn percent_fraction(percent: f32) -> Option<f32> {
    (percent.is_finite() && (0.0..=PERCENT_SCALE).contains(&percent))
        .then_some(percent / PERCENT_SCALE)
}

fn normalized_load(load: f64, host_cpus: u32) -> Option<f32> {
    if !load.is_finite() || load < 0.0 || host_cpus == 0 {
        return None;
    }
    let normalized = (load / f64::from(host_cpus)) as f32;
    normalized.is_finite().then_some(normalized)
}

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
fn load_average() -> Option<f64> {
    let mut load = 0.0;
    let count = unsafe { libc::getloadavg(&mut load, 1) };
    (count == 1).then_some(load)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
fn load_average() -> Option<f64> {
    None
}

#[cfg(target_os = "linux")]
fn pressure() -> Option<f32> {
    read_pressure(File::open(PRESSURE_PATH).ok()?)
}

#[cfg(not(target_os = "linux"))]
fn pressure() -> Option<f32> {
    None
}

#[cfg(any(target_os = "linux", test))]
fn read_pressure(reader: impl Read) -> Option<f32> {
    let mut text = String::new();
    reader
        .take((MAX_PRESSURE_BYTES + 1) as u64)
        .read_to_string(&mut text)
        .ok()?;
    if text.len() > MAX_PRESSURE_BYTES {
        return None;
    }
    parse_pressure(&text)
}

#[cfg(any(target_os = "linux", test))]
fn parse_pressure(text: &str) -> Option<f32> {
    let mut some = text.lines().filter_map(|line| {
        let mut fields = line.split_ascii_whitespace();
        (fields.next()? == "some").then_some(fields)
    });
    let mut averages = some
        .next()?
        .filter_map(|field| field.strip_prefix("avg10="));
    let percent = averages.next()?.parse().ok()?;
    if averages.next().is_some() || some.next().is_some() {
        return None;
    }
    percent_fraction(percent)
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::{Duration, Instant};

    use caudra_storage::shell_runs::HostSnapshot;
    use sysinfo::{MINIMUM_CPU_UPDATE_INTERVAL, System};
    use test_case::test_case;

    use super::{
        CPU_USAGE_WINDOW, CachedSnapshot, CpuHistory, CpuInterval, CpuWindow, IDLE_TIMEOUT,
        MAX_COUNTER_INTERVAL, MAX_CPU_INTERVALS, MAX_PRESSURE_BYTES, MAX_SAMPLE_AGE,
        SAMPLE_INTERVAL, Sampler, cpu_count, measure, normalized_load, parse_pressure,
        percent_fraction, read_pressure, usage_in_window,
    };

    const PRESSURE_SAMPLE: &str = "some avg10=25.00 avg60=12.00 avg300=3.00 total=100\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
    const BENCHMARK_REFRESHES: usize = 10;
    const TEST_POISON: &str = "test sampler poison";
    const MEAN_TOLERANCE: f32 = 0.000_001;

    fn cached(now: Instant, cpu_usage: Option<f32>) -> CachedSnapshot {
        CachedSnapshot {
            value: HostSnapshot {
                cpu_usage,
                cpu_usage_mean: cpu_usage,
                cpu_usage_window_ms: cpu_usage.map(|_| 1_000),
                normalized_load: Some(1.5),
                cpu_pressure: Some(0.25),
                host_cpus: Some(8),
                available_cpus: Some(2),
                sample_age_ms: None,
            },
            sampled_at: now,
            completed_at: now,
        }
    }

    #[test]
    fn cache_enforces_age_and_usage_freshness() {
        let now = Instant::now();
        let cache = cached(now, Some(0.5));
        assert!(cache.at(now - Duration::from_secs(1)).is_none());
        let fresh = cache.at(now + MAX_SAMPLE_AGE).unwrap();
        assert_eq!(fresh.cpu_usage, Some(0.5));
        assert_eq!(fresh.cpu_usage_mean, Some(0.5));
        assert_eq!(fresh.cpu_usage_window_ms, Some(1_000));
        assert_eq!(fresh.host_cpus, Some(8));
        assert_eq!(fresh.available_cpus, Some(2));
        assert_eq!(fresh.sample_age_ms, Some(MAX_SAMPLE_AGE.as_millis() as u64));
        assert!(
            cache
                .at(now + MAX_SAMPLE_AGE + Duration::from_nanos(1))
                .is_none()
        );
        let partial = cached(now, None).at(now).unwrap();
        assert_eq!(partial.cpu_usage, None);
        assert_eq!(partial.cpu_usage_mean, None);
        assert_eq!(partial.cpu_usage_window_ms, None);
        assert_eq!(partial.sample_age_ms, None);
        assert_eq!(partial.normalized_load, Some(1.5));
        assert_eq!(partial.cpu_pressure, Some(0.25));
    }

    #[test]
    fn cache_rejects_readings_completed_after_forecast_time() {
        let now = Instant::now();
        let mut cache = cached(now, Some(0.5));
        cache.completed_at += SAMPLE_INTERVAL;
        assert!(cache.at(now).is_none());
        let saved = cache.at(now + SAMPLE_INTERVAL).unwrap();
        assert_eq!(saved.sample_age_ms, Some(1_000));
        assert_eq!(saved.cpu_usage_mean, Some(0.5));
    }

    #[test]
    fn worker_lease_serializes_start_and_restarts_after_failure() {
        let sampler = Sampler::new();
        let now = Instant::now();
        let (sample, first) = sampler.request(now);
        assert!(sample.is_none());
        let first = first.unwrap();
        assert!(sampler.request(now).1.is_none());
        assert!(sampler.running.load(Ordering::Acquire));
        drop(first);
        assert!(sampler.request(now).1.is_some());
    }

    #[test]
    fn idle_exit_cannot_release_a_replacement_worker() {
        let sampler = Sampler::new();
        let now = Instant::now();
        let mut first = sampler.request(now).1.unwrap();
        assert!(!first.stop_if_idle(now + IDLE_TIMEOUT - Duration::from_nanos(1)));
        sampler.request(now + IDLE_TIMEOUT);
        sampler.request(now);
        assert!(!first.stop_if_idle(now + IDLE_TIMEOUT));
        assert!(first.stop_if_idle(now + IDLE_TIMEOUT + IDLE_TIMEOUT));
        let second = sampler
            .request(now + IDLE_TIMEOUT + IDLE_TIMEOUT)
            .1
            .unwrap();
        drop(first);
        assert!(sampler.running.load(Ordering::Acquire));
        drop(second);
        assert!(!sampler.running.load(Ordering::Acquire));
    }

    #[test]
    fn cache_contention_returns_without_waiting() {
        let sampler = Sampler::new();
        let _state = sampler.state.lock().unwrap();
        let (sample, worker) = sampler.request(Instant::now());
        assert!(sample.is_none());
        assert!(worker.is_none());
        assert!(!sampler.running.load(Ordering::Acquire));
    }

    #[test]
    fn poisoned_cache_remains_usable() {
        let sampler = Sampler::new();
        let now = Instant::now();
        let worker = sampler.request(now).1.unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut state = sampler.state.lock().unwrap();
            state.sample = Some(cached(now, Some(0.5)));
            panic!("{TEST_POISON}");
        }));
        assert!(result.is_err());
        let (sample, replacement) = sampler.request(now);
        assert_eq!(sample.unwrap().cpu_usage, Some(0.5));
        assert!(replacement.is_none());
        drop(worker);
    }

    fn refresh(at: Instant) -> CpuWindow {
        CpuWindow {
            started: at,
            finished: at,
        }
    }

    fn assert_summary(history: &CpuHistory, mean: f32, coverage_ms: u64) {
        let (actual, coverage) = history.summary().unwrap();
        assert!((actual - mean).abs() <= MEAN_TOLERANCE);
        assert_eq!(coverage, coverage_ms);
    }

    #[test]
    fn rolling_mean_weights_intervals_by_duration() {
        let now = Instant::now();
        let mut history = CpuHistory::default();
        assert_eq!(history.record(Some(8), 20.0, refresh(now)), None);
        for seconds in [4, 8] {
            assert_eq!(
                history.record(Some(8), 20.0, refresh(now + Duration::from_secs(seconds))),
                Some(0.2)
            );
        }
        assert_eq!(
            history.record(Some(8), 90.0, refresh(now + CPU_USAGE_WINDOW)),
            Some(0.9)
        );
        assert_summary(&history, 0.34, 10_000);
    }

    #[test]
    fn rolling_mean_clips_the_oldest_interval() {
        let now = Instant::now();
        let mut history = CpuHistory::default();
        history.record(Some(8), 20.0, refresh(now));
        for seconds in [4, 8] {
            history.record(Some(8), 20.0, refresh(now + Duration::from_secs(seconds)));
        }
        history.record(Some(8), 90.0, refresh(now + Duration::from_secs(12)));
        assert_summary(&history, 0.48, 10_000);
        assert_eq!(
            history.intervals.front().unwrap().started,
            now + Duration::from_secs(2)
        );
    }

    #[test]
    fn warmup_coverage_excludes_refresh_time() {
        let now = Instant::now();
        let mut history = CpuHistory::default();
        assert_eq!(
            history.record(
                Some(8),
                20.0,
                CpuWindow {
                    started: now,
                    finished: now + Duration::from_millis(100),
                }
            ),
            None
        );
        assert!(history.summary().is_none());
        history.record(
            Some(8),
            20.0,
            CpuWindow {
                started: now + Duration::from_millis(1_100),
                finished: now + Duration::from_millis(1_200),
            },
        );
        assert_summary(&history, 0.2, 1_000);
        history.record(
            Some(8),
            80.0,
            CpuWindow {
                started: now + Duration::from_millis(2_200),
                finished: now + Duration::from_millis(2_300),
            },
        );
        assert_summary(&history, 0.5, 2_000);
    }

    #[test_case(Duration::ZERO; "overlapping_refresh")]
    #[test_case(Duration::from_secs(1); "duplicate_refresh")]
    #[test_case(Duration::from_secs(7); "gap")]
    fn invalid_intervals_reset_history_and_baseline(offset: Duration) {
        let now = Instant::now();
        let mut history = CpuHistory::default();
        history.record(Some(8), 20.0, refresh(now));
        history.record(Some(8), 20.0, refresh(now + SAMPLE_INTERVAL));
        assert!(history.summary().is_some());
        assert_eq!(history.record(Some(8), 90.0, refresh(now + offset)), None);
        assert!(history.summary().is_none());
        assert!(history.previous.is_none());
        let next = now + offset + CPU_USAGE_WINDOW;
        assert_eq!(history.record(Some(8), 90.0, refresh(next)), None);
        assert!(history.summary().is_none());
        assert_eq!(
            history.record(Some(8), 90.0, refresh(next + SAMPLE_INTERVAL)),
            Some(0.9)
        );
        assert_summary(&history, 0.9, 1_000);
    }

    #[test_case(None, 50.0; "missing_cpu_count")]
    #[test_case(Some(0), 50.0; "invalid_cpu_count")]
    #[test_case(Some(8), f32::NAN; "missing_usage")]
    #[test_case(Some(8), f32::INFINITY; "infinite_usage")]
    #[test_case(Some(8), 101.0; "invalid_usage")]
    fn failed_readings_reset_history_and_baseline(host_cpus: Option<u32>, percent: f32) {
        let now = Instant::now();
        let mut history = CpuHistory::default();
        history.record(Some(8), 20.0, refresh(now));
        history.record(Some(8), 20.0, refresh(now + SAMPLE_INTERVAL));
        assert!(history.summary().is_some());
        assert_eq!(
            history.record(host_cpus, percent, refresh(now + SAMPLE_INTERVAL * 2)),
            None
        );
        assert!(history.summary().is_none());
        assert!(history.previous.is_none());
        assert_eq!(
            history.record(Some(8), 90.0, refresh(now + SAMPLE_INTERVAL * 3)),
            None
        );
        history.record(Some(8), 90.0, refresh(now + SAMPLE_INTERVAL * 4));
        assert_summary(&history, 0.9, 1_000);
    }

    #[test]
    fn changed_cpu_count_establishes_a_new_baseline() {
        let now = Instant::now();
        let mut history = CpuHistory::default();
        history.record(Some(8), 20.0, refresh(now));
        history.record(Some(8), 20.0, refresh(now + SAMPLE_INTERVAL));
        assert_summary(&history, 0.2, 1_000);
        assert_eq!(
            history.record(Some(4), 90.0, refresh(now + SAMPLE_INTERVAL * 2)),
            None
        );
        assert!(history.summary().is_none());
        history.record(Some(4), 90.0, refresh(now + SAMPLE_INTERVAL * 3));
        assert_summary(&history, 0.9, 1_000);
    }

    #[test]
    fn restart_discards_cached_and_rolling_history() {
        let now = Instant::now();
        let sampler = Sampler::new();
        let worker = sampler.request(now).1.unwrap();
        let mut history = CpuHistory::default();
        history.record(Some(8), 20.0, refresh(now));
        history.record(Some(8), 20.0, refresh(now + SAMPLE_INTERVAL));
        assert_summary(&history, 0.2, 1_000);
        sampler.state.lock().unwrap().sample = Some(cached(now + SAMPLE_INTERVAL, Some(0.2)));
        let saved = sampler.request(now + SAMPLE_INTERVAL).0.unwrap();
        drop(worker);
        drop(history);
        let (sample, replacement) = sampler.request(now + SAMPLE_INTERVAL);
        assert!(sample.is_none());
        assert!(replacement.is_some());
        let mut history = CpuHistory::default();
        assert_eq!(
            history.record(Some(8), 90.0, refresh(now + SAMPLE_INTERVAL)),
            None
        );
        assert!(history.summary().is_none());
        history.record(Some(8), 90.0, refresh(now + SAMPLE_INTERVAL * 2));
        assert_summary(&history, 0.9, 1_000);
        assert_eq!(saved.cpu_usage_mean, Some(0.2));
        assert_eq!(saved.cpu_usage_window_ms, Some(1_000));
    }

    #[test]
    fn rolling_intervals_never_double_count_overlap() {
        let now = Instant::now();
        let mut history = CpuHistory::default();
        for (start, end, usage) in [(0, 4, 0.2), (2, 6, 0.8), (1, 3, 0.9)] {
            history.push(
                CpuInterval {
                    started: now + Duration::from_secs(start),
                    finished: now + Duration::from_secs(end),
                    usage,
                },
                now + Duration::from_secs(6),
            );
        }
        assert_summary(&history, 0.4, 6_000);
        assert_eq!(history.intervals.len(), 2);
    }

    #[test]
    fn rolling_deque_bounds_time_and_interval_count() {
        let now = Instant::now();
        let mut history = CpuHistory::default();
        history.record(Some(8), 50.0, refresh(now));
        for index in 1..=MAX_CPU_INTERVALS * 10 {
            history.record(Some(8), 50.0, refresh(now + SAMPLE_INTERVAL * index as u32));
            assert!(history.intervals.len() <= MAX_CPU_INTERVALS);
        }
        assert_summary(&history, 0.5, CPU_USAGE_WINDOW.as_millis() as u64);
        history.clear();
        for index in 0..MAX_CPU_INTERVALS * 2 {
            let started = now + Duration::from_millis(index as u64);
            let finished = started + Duration::from_millis(1);
            history.push(
                CpuInterval {
                    started,
                    finished,
                    usage: 0.5,
                },
                finished,
            );
            assert!(history.intervals.len() <= MAX_CPU_INTERVALS);
        }
        assert_summary(&history, 0.5, MAX_CPU_INTERVALS as u64);
    }

    #[test]
    fn usage_requires_a_bounded_counter_window() {
        let now = Instant::now();
        let previous = CpuWindow {
            started: now,
            finished: now,
        };
        let mut current = CpuWindow {
            started: now,
            finished: now,
        };
        assert_eq!(usage_in_window(50.0, None, &current), None);
        assert_eq!(usage_in_window(50.0, Some(&previous), &current), None);
        current.started += MINIMUM_CPU_UPDATE_INTERVAL;
        current.finished = current.started;
        assert_eq!(usage_in_window(50.0, Some(&previous), &current), Some(0.5));
        current.finished = now + MAX_COUNTER_INTERVAL;
        assert_eq!(usage_in_window(50.0, Some(&previous), &current), Some(0.5));
        current.finished += Duration::from_nanos(1);
        assert_eq!(usage_in_window(50.0, Some(&previous), &current), None);
    }

    #[test_case(0.0, Some(0.0); "idle")]
    #[test_case(100.0, Some(1.0); "saturated")]
    #[test_case(-1.0, None; "negative")]
    #[test_case(100.1, None; "over_range")]
    #[test_case(f32::NAN, None; "nan")]
    #[test_case(f32::INFINITY, None; "infinite")]
    fn percentages_reject_invalid_readings(input: f32, expected: Option<f32>) {
        assert_eq!(percent_fraction(input), expected);
    }

    #[test_case(12.0, 8, Some(1.5); "host_not_available_count")]
    #[test_case(0.0, 8, Some(0.0); "idle")]
    #[test_case(1.0, 0, None; "unknown_cpu_count")]
    #[test_case(-1.0, 8, None; "negative")]
    #[test_case(f64::NAN, 8, None; "nan")]
    #[test_case(f64::INFINITY, 8, None; "infinite")]
    #[test_case(f64::MAX, 1, None; "overflow")]
    fn load_is_normalized_without_clamping(load: f64, cpus: u32, expected: Option<f32>) {
        assert_eq!(normalized_load(load, cpus), expected);
    }

    #[test_case(0, None; "missing")]
    #[test_case(8, Some(8); "known")]
    fn cpu_counts_are_positive(input: usize, expected: Option<u32>) {
        assert_eq!(cpu_count(input), expected);
    }

    #[test_case(PRESSURE_SAMPLE, Some(0.25); "some_not_full")]
    #[test_case("some avg10=0.00", Some(0.0); "idle")]
    #[test_case("some avg10=100.00", Some(1.0); "saturated")]
    #[test_case("full avg10=25.00", None; "no_some")]
    #[test_case("some avg60=25.00", None; "no_avg10")]
    #[test_case("some avg10=NaN", None; "nan")]
    #[test_case("some avg10=inf", None; "infinite")]
    #[test_case("some avg10=-1", None; "negative")]
    #[test_case("some avg10=101", None; "over_range")]
    #[test_case("some avg10=broken", None; "malformed")]
    #[test_case("some avg10=1 avg10=2", None; "duplicate_average")]
    #[test_case("some avg10=1\nsome avg10=2", None; "duplicate_some")]
    fn pressure_parser(input: &str, expected: Option<f32>) {
        assert_eq!(parse_pressure(input), expected);
    }

    #[test]
    fn pressure_reads_are_bounded_and_fail_closed() {
        assert_eq!(read_pressure(Cursor::new(PRESSURE_SAMPLE)), Some(0.25));
        let oversized = format!("{PRESSURE_SAMPLE}{}", " ".repeat(MAX_PRESSURE_BYTES));
        let mut reader = Cursor::new(oversized);
        assert_eq!(read_pressure(&mut reader), None);
        assert_eq!(reader.position(), (MAX_PRESSURE_BYTES + 1) as u64);
        assert_eq!(read_pressure(Cursor::new([0xff])), None);
        assert_eq!(read_pressure(io::repeat(0)), None);
    }

    #[test]
    #[ignore = "manual CPU-only sampler timing and host smoke test"]
    fn real_sampler_cpu_only_benchmark() {
        let initialized = Instant::now();
        let mut system = System::new();
        eprintln!("host sampler initialization: {:?}", initialized.elapsed());
        let mut history = CpuHistory::default();
        for index in 0..BENCHMARK_REFRESHES {
            if index > 0 {
                thread::sleep(SAMPLE_INTERVAL.max(MINIMUM_CPU_UPDATE_INTERVAL));
            }
            let started = Instant::now();
            let sample = measure(&mut system, &mut history);
            let elapsed = started.elapsed();
            let snapshot = sample.at(Instant::now());
            eprintln!("host sampler refresh {index}: {elapsed:?}, {snapshot:?}");
            assert!(system.processes().is_empty());
            if index == 0 {
                assert_eq!(sample.value.cpu_usage, None);
            }
            match (
                sample.value.cpu_usage,
                sample.value.cpu_usage_mean,
                sample.value.cpu_usage_window_ms,
            ) {
                (Some(_), Some(mean), Some(coverage)) => {
                    assert!((0.0..=1.0).contains(&mean));
                    assert!(coverage > 0);
                    assert!(u128::from(coverage) <= CPU_USAGE_WINDOW.as_millis());
                }
                (None, None, None) => {}
                fields => panic!("inconsistent rolling CPU snapshot: {fields:?}"),
            }
        }
    }
}
