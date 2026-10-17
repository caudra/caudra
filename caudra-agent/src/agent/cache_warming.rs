//! Prompt-cache warming: while a conversation waits on a long tool, a
//! subagent, a question or the user, its last request is sent again with no
//! output so the provider's prompt cache outlives the wait. A warm bills a
//! cache read; letting the entry expire bills a full rewrite at the next turn.
//!
//! Each conversation keeps one open episode, from the request that armed it to
//! the next one. With warming off the episode is a shadow: at its end the
//! scheduler replays the real timeline arithmetically, so `/usage` can say what
//! warming would have cost and saved before anyone turns it on.

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::mem;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use caudra_config::CacheWarmingMode;
use caudra_providers::model::{Billing, Model, TokenUsage};
use caudra_providers::provider::CacheWarm;
use caudra_providers::{AgentError, RequestOptions};
use caudra_storage::auth::now_millis;
use caudra_storage::cache_warming::{
    Boundary, CacheWarmEpisode, EpisodeKind, EpisodeOutcome, HitSamples,
};
use caudra_storage::usage_ledger::LedgerPurpose;
use serde_json::Value;
use smol::{Task, Timer};
use tracing::{info, warn};

use crate::types::{AgentEvent, EventSender};

pub const MAIN_AGENT: &str = "main";
const WARM_TARGET: &str = "caudra.cache_warm";
const EPISODE_TARGET: &str = "caudra.cache_warm_episode";
const REFRESHED: &str = "refreshed";
/// The share of a prompt a request must read from cache to count as a hit,
/// and a warm to count as a refresh.
const HIT_SHARE: f64 = 0.9;
/// The most of its prompt a refresh may write before it counts as a new entry.
const MAX_WRITE_SHARE: f64 = 0.05;
const REFRESH_SHARE: f64 = 0.9;
/// Past the lifetime by up to this much, about half of requests still hit,
/// so an episode ending there is never credited with a saving.
const LIFETIME_EDGE: Duration = Duration::from_secs(30);
/// How likely an idle conversation is to be picked up before the cap.
const IDLE_CONTINUATION: f64 = 0.15;
/// The largest minimum prompt Anthropic caches.
const MIN_CACHEABLE_TOKENS: u32 = 4_096;
const BAD_REQUEST: u16 = 400;
const DEFAULT_LEAD: Duration = Duration::from_secs(10);
const DEFAULT_CAP: Duration = Duration::from_secs(30 * 60);
const DEFAULT_MIN_SAVING: f64 = 0.05;
const MAIN_MODES: &[CacheWarmingMode] = &[CacheWarmingMode::Running, CacheWarmingMode::Idle];
const SUBAGENT_MODES: &[CacheWarmingMode] = &[CacheWarmingMode::Running];

/// A subagent's run is its whole conversation, so idle warming never applies.
pub fn subagent_mode(mode: CacheWarmingMode) -> CacheWarmingMode {
    match mode {
        CacheWarmingMode::Idle => CacheWarmingMode::Running,
        mode => mode,
    }
}

/// When a refresh goes out and when warming gives up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WarmPolicy {
    /// How long before the entry expires a refresh is sent.
    pub lead: Duration,
    /// How long warming runs past the last request, or past the end of the
    /// run in idle mode.
    pub cap: Duration,
    /// The least a refresh must be expected to save, at list price.
    pub min_saving: f64,
}

impl Default for WarmPolicy {
    fn default() -> Self {
        Self {
            lead: DEFAULT_LEAD,
            cap: DEFAULT_CAP,
            min_saving: DEFAULT_MIN_SAVING,
        }
    }
}

/// Why warming stopped before the next request, as the ledger stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmStop {
    NextRequest,
    RunEnded,
    Cap,
    NotWorthIt,
    SpendBound,
    Late,
    Failed,
    Unsupported,
    Wrote,
    PrefixChanged,
    RequestFailed,
    Closed,
}

impl WarmStop {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NextRequest => "next_request",
            Self::RunEnded => "run_ended",
            Self::Cap => "cap",
            Self::NotWorthIt => "not_worth_it",
            Self::SpendBound => "spend_bound",
            Self::Late => "late",
            Self::Failed => "failed",
            Self::Unsupported => "unsupported",
            Self::Wrote => "wrote",
            Self::PrefixChanged => "prefix_changed",
            Self::RequestFailed => "request_failed",
            Self::Closed => "closed",
        }
    }
}

/// Whose conversation a warmer keeps warm.
#[derive(Debug, Clone, Default)]
pub struct WarmerContext {
    pub session_id: String,
    /// The subagent's tool use id, `None` for the main conversation.
    pub subagent: Option<String>,
    pub cwd: String,
}

/// Whether the conversation's prefix is still the one a warm would refresh,
/// asked before every warm by the loop that can change it between runs.
pub type PrefixGuard = Arc<dyn Fn() -> bool + Send + Sync>;

/// A warm's spend, for the goal and autopilot budgets of the run that next
/// takes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WarmSpend {
    pub usage: TokenUsage,
    pub cost: Option<f64>,
    pub billing: Billing,
}

/// What a finished request can be warmed with.
pub enum Warmable {
    Handle(Box<dyn CacheWarm>),
    /// Warming is off, so only how long a warm would have kept the entry,
    /// which is all a shadow episode needs.
    Shadow(Duration),
}

/// A main-turn request that returned, with what it takes to warm it.
pub struct FinishedRequest<'a> {
    pub started: Instant,
    /// The history epoch it was sent at; a change means a compaction or a
    /// revert replaced the prefix.
    pub epoch: u64,
    /// [`prefix_fingerprint`] of the request, `None` when it cannot warm.
    pub prefix: Option<u64>,
    pub boundary: Boundary,
    pub usage: TokenUsage,
    pub model: &'a Arc<Model>,
    pub fast: bool,
    pub billing: Billing,
    pub account: Option<String>,
    pub warm: Option<Warmable>,
    pub events: &'a EventSender,
}

/// What the provider keys its cache on besides the history: a change to any
/// of these writes a new entry even though the messages read the same.
pub fn prefix_fingerprint(
    model: &Model,
    system: &str,
    tools: &Value,
    opts: &RequestOptions,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    model.spec().hash(&mut hasher);
    system.hash(&mut hasher);
    opts.thinking.to_string().hash(&mut hasher);
    opts.fast.hash(&mut hasher);
    if serde_json::to_writer(HashWriter(&mut hasher), tools).is_err() {
        tools.to_string().hash(&mut hasher);
    }
    hasher.finish()
}

/// Feeds serialized JSON to a hasher without building the string.
struct HashWriter<'a>(&'a mut DefaultHasher);

impl Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Hasher::write(self.0, bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Time since `started`, by the wall clock as well as the monotonic one: a
/// monotonic clock stands still while the machine sleeps, and the provider's
/// cache does not.
fn elapsed_since(started: Instant, started_ms: u64) -> Duration {
    started.elapsed().max(Duration::from_millis(
        now_millis().saturating_sub(started_ms),
    ))
}

/// The wall-clock moment `started` happened.
fn wall_ms(started: Instant) -> u64 {
    now_millis().saturating_sub(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX))
}

/// The whole prompt's price as a cache read, and what a miss costs on top.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Prices {
    read: f64,
    miss: f64,
}

impl Prices {
    fn of(model: &Model, prompt: u32, fast: bool) -> Option<Self> {
        let read = model.billed_cost(
            &TokenUsage {
                cache_read: prompt,
                ..TokenUsage::default()
            },
            fast,
        )?;
        let write = model.billed_cost(
            &TokenUsage {
                cache_creation: prompt,
                ..TokenUsage::default()
            },
            fast,
        )?;
        (write > read).then_some(Self {
            read,
            miss: write - read,
        })
    }

    fn miss_of(self, tokens: u32, prompt: u32) -> f64 {
        self.miss * f64::from(tokens) / f64::from(prompt.max(1))
    }
}

/// Where an episode's warming stands, timed from the request that armed it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Progress {
    last_refresh: Duration,
    spent: f64,
    warms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Next {
    Warm(Duration),
    Stop(WarmStop),
}

/// `min(0.9·ttl, ttl − lead)`, so the refresh lands before the entry expires
/// with room for the request to travel.
fn refresh_delay(ttl: Duration, lead: Duration) -> Option<Duration> {
    (ttl > lead).then(|| ttl.mul_f64(REFRESH_SHARE).min(ttl - lead))
}

/// A refresh this far past its time likely finds the entry gone, and would
/// pay a full rewrite: the machine slept, or the executor stalled.
fn too_late(delay: Duration, ttl: Duration, scheduled: Duration, fired: Duration) -> bool {
    fired > scheduled + ttl.saturating_sub(delay) / 2
}

fn next_warm(
    policy: &WarmPolicy,
    mode: CacheWarmingMode,
    delay: Duration,
    prices: Prices,
    settled: Option<Duration>,
    progress: &Progress,
) -> Next {
    let at = progress.last_refresh + delay;
    let (cap_from, continuation) = match settled {
        Some(settled) if at >= settled => match mode {
            CacheWarmingMode::Idle => (settled, IDLE_CONTINUATION),
            CacheWarmingMode::Off | CacheWarmingMode::Running => {
                return Next::Stop(WarmStop::RunEnded);
            }
        },
        _ => (Duration::ZERO, 1.0),
    };
    if at > cap_from + policy.cap {
        Next::Stop(WarmStop::Cap)
    } else if continuation * prices.miss - prices.read < policy.min_saving {
        Next::Stop(WarmStop::NotWorthIt)
    } else if progress.spent + prices.read > prices.miss {
        Next::Stop(WarmStop::SpendBound)
    } else {
        Next::Warm(at)
    }
}

/// The warms `mode` would have sent before `until`, on the same rules the
/// timer follows, and why it would have stopped first if it had.
fn simulate(
    policy: &WarmPolicy,
    mode: CacheWarmingMode,
    ttl: Duration,
    prices: Prices,
    settled: Option<Duration>,
    until: Duration,
) -> (Progress, Option<WarmStop>) {
    let mut progress = Progress::default();
    let Some(delay) = refresh_delay(ttl, policy.lead) else {
        return (progress, Some(WarmStop::NotWorthIt));
    };
    loop {
        match next_warm(policy, mode, delay, prices, settled, &progress) {
            Next::Warm(at) if at < until => {
                progress.last_refresh = at;
                progress.spent += prices.read;
                progress.warms += 1;
            }
            Next::Warm(_) => return (progress, None),
            Next::Stop(reason) => return (progress, Some(reason)),
        }
    }
}

fn is_hit(read: u32, prompt: u32) -> bool {
    f64::from(read) >= HIT_SHARE * f64::from(prompt)
}

/// How a warm's reply reads: a refresh reads nearly the whole prompt and
/// writes next to nothing.
fn refreshed(usage: &TokenUsage, prompt: u32) -> bool {
    is_hit(usage.cache_read, prompt)
        && f64::from(usage.cache_creation) <= MAX_WRITE_SHARE * f64::from(prompt)
}

/// The request that ended an episode on the same prefix.
#[derive(Debug, Clone, Copy, PartialEq)]
struct NextRequest {
    gap: Duration,
    cache_read: u32,
    prompt: u32,
}

/// The outcome of an episode with at least one warm, and the tokens it saved.
fn classify(
    kind: EpisodeKind,
    ttl: Duration,
    prompt: u32,
    progress: &Progress,
    wrote: bool,
    next: Option<&NextRequest>,
) -> (EpisodeOutcome, u32) {
    let Some(next) = next.filter(|_| !wrote) else {
        let outcome = if wrote {
            EpisodeOutcome::Wrote
        } else {
            EpisodeOutcome::Unused
        };
        return (outcome, 0);
    };
    if next.gap <= ttl {
        (EpisodeOutcome::Unneeded, 0)
    } else if next.gap > progress.last_refresh + ttl {
        (EpisodeOutcome::Lapsed, 0)
    } else if kind == EpisodeKind::Real && !is_hit(next.cache_read, prompt) {
        (EpisodeOutcome::Missed, 0)
    } else if next.gap <= ttl + LIFETIME_EDGE {
        (EpisodeOutcome::Marginal, 0)
    } else {
        let saved = match kind {
            EpisodeKind::Real => next.cache_read,
            EpisodeKind::Shadow => next.prompt,
        };
        (EpisodeOutcome::Saved, saved.min(prompt))
    }
}

/// What the conversation's last successful request armed.
struct Episode {
    id: u64,
    kind: EpisodeKind,
    started: Instant,
    started_ms: u64,
    epoch: u64,
    prefix: u64,
    prompt: u32,
    model: Arc<Model>,
    fast: bool,
    account: Option<String>,
    billing: Billing,
    prices: Prices,
    ttl: Duration,
    settled: Option<Duration>,
    progress: Progress,
    usage: TokenUsage,
    cost: f64,
    wrote: bool,
    stopped: Option<WarmStop>,
    handle: Option<Arc<dyn CacheWarm>>,
    events: EventSender,
}

impl Episode {
    fn delay(&self, policy: &WarmPolicy) -> Option<Duration> {
        refresh_delay(self.ttl, policy.lead)
    }

    fn elapsed(&self) -> Duration {
        elapsed_since(self.started, self.started_ms)
    }

    fn phase(&self) -> &'static str {
        match self.settled {
            Some(_) => CacheWarmingMode::Idle.as_str(),
            None => CacheWarmingMode::Running.as_str(),
        }
    }
}

/// A warm on its way, with everything booking it takes, so a warm that lands
/// after its episode ended is still billed where it belongs.
struct Shot {
    episode: u64,
    handle: Arc<dyn CacheWarm>,
    fired: Duration,
    model: Arc<Model>,
    fast: bool,
    billing: Billing,
    account: Option<String>,
    prompt: u32,
    phase: &'static str,
    events: EventSender,
}

#[derive(Default)]
struct State {
    episode: Option<Episode>,
    episodes_armed: u64,
    timer: Option<Task<()>>,
    /// A 400 or a warm that wrote: the provider does not warm this
    /// conversation the way the policy assumes, so it never tries again.
    disabled: bool,
    samples: BTreeMap<(String, String, Boundary), (u64, u64)>,
    spend: Vec<WarmSpend>,
    guard: Option<PrefixGuard>,
    events: Option<EventSender>,
}

/// What a close produced, emitted once the lock is released.
#[derive(Default)]
struct Records {
    episodes: Vec<(CacheWarmEpisode, EventSender)>,
}

struct Inner {
    mode: CacheWarmingMode,
    policy: WarmPolicy,
    context: WarmerContext,
    state: Mutex<State>,
}

/// One conversation's warming, shared by every run of it. Dropping the last
/// handle ends the open episode and aborts any warm in flight.
#[derive(Clone)]
pub struct CacheWarmer {
    inner: Arc<Inner>,
}

/// Reaches a conversation's warming without keeping it alive, for whoever
/// hears of a settings change outside the loop that owns it.
#[derive(Clone, Default)]
pub struct WarmerRef(Weak<Inner>);

impl WarmerRef {
    pub fn stop(&self, reason: WarmStop) {
        if let Some(inner) = self.0.upgrade() {
            CacheWarmer { inner }.stop(reason);
        }
    }
}

impl CacheWarmer {
    pub fn new(mode: CacheWarmingMode, context: WarmerContext) -> Self {
        Self::with_policy(mode, WarmPolicy::default(), context)
    }

    pub fn with_policy(mode: CacheWarmingMode, policy: WarmPolicy, context: WarmerContext) -> Self {
        Self {
            inner: Arc::new(Inner {
                mode,
                policy,
                context,
                state: Mutex::default(),
            }),
        }
    }

    pub fn downgrade(&self) -> WarmerRef {
        WarmerRef(Arc::downgrade(&self.inner))
    }

    /// Whether a finished request should bring a [`Warmable`]. With warming
    /// off one is still wanted: it says the request could have been warmed.
    pub fn wants_warm(&self) -> bool {
        !self.inner.lock().disabled
    }

    /// Whether warms are sent, so a request must bring a [`Warmable::Handle`]
    /// rather than the shadow's cheap lifetime.
    pub fn sends(&self) -> bool {
        self.inner.mode != CacheWarmingMode::Off
    }

    pub fn set_guard(&self, guard: PrefixGuard) {
        self.inner.lock().guard = Some(guard);
    }

    /// Stops the timer before a real request. A warm already sent still lands
    /// and is booked.
    pub fn hold(&self) {
        self.inner.lock().timer = None;
    }

    /// Warm spend since the last call, for the run's budgets.
    pub fn take_spend(&self) -> Vec<WarmSpend> {
        mem::take(&mut self.inner.lock().spend)
    }

    /// Ends the open episode at `request` and arms the next one from it.
    pub fn finish_request(&self, request: FinishedRequest<'_>) {
        let records = {
            let mut state = self.inner.lock();
            state.timer = None;
            state.events = Some(request.events.clone());
            let mut records = Records::default();
            if let Some(episode) = state.episode.take() {
                let next = (episode.epoch == request.epoch
                    && Some(episode.prefix) == request.prefix)
                    .then(|| NextRequest {
                        gap: request
                            .started
                            .saturating_duration_since(episode.started)
                            .max(Duration::from_millis(
                                wall_ms(request.started).saturating_sub(episode.started_ms),
                            )),
                        cache_read: request.usage.cache_read,
                        prompt: request.usage.total_input(),
                    });
                let reason = if next.is_some() {
                    WarmStop::NextRequest
                } else {
                    WarmStop::PrefixChanged
                };
                self.inner.close(
                    &mut state,
                    episode,
                    next,
                    Some(request.boundary),
                    reason,
                    &mut records,
                );
            }
            if !state.disabled {
                let episode = self.inner.arm(&mut state, request);
                state.episode = episode;
                self.inner.schedule(&mut state);
            }
            records
        };
        records.emit();
    }

    /// Ends the open episode with no request to read it.
    pub fn stop(&self, reason: WarmStop) {
        let records = {
            let mut state = self.inner.lock();
            state.timer = None;
            let mut records = Records::default();
            if let Some(episode) = state.episode.take() {
                self.inner
                    .close(&mut state, episode, None, None, reason, &mut records);
            }
            records
        };
        records.emit();
    }

    /// The run is over: running mode stops warming, idle mode carries on
    /// under its own cap. A live idle timer is left alone, since it re-reads
    /// the end of the run before each warm.
    pub fn run_settled(&self) {
        let mut state = self.inner.lock();
        if let Some(episode) = state.episode.as_mut() {
            episode.settled = Some(episode.elapsed());
            if episode.kind == EpisodeKind::Real && self.inner.mode != CacheWarmingMode::Idle {
                episode.stopped.get_or_insert(WarmStop::RunEnded);
            }
        }
        if self.inner.mode != CacheWarmingMode::Idle || state.timer.is_none() {
            self.inner.schedule(&mut state);
        }
        self.inner.flush_samples(&mut state);
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn agent(&self) -> &str {
        self.context.subagent.as_deref().unwrap_or(MAIN_AGENT)
    }

    fn simulated_modes(&self) -> &'static [CacheWarmingMode] {
        if self.context.subagent.is_some() {
            SUBAGENT_MODES
        } else {
            MAIN_MODES
        }
    }

    fn arm(&self, state: &mut State, request: FinishedRequest<'_>) -> Option<Episode> {
        let prefix = request.prefix?;
        let prompt = request.usage.total_input();
        if prompt < MIN_CACHEABLE_TOKENS {
            return None;
        }
        let prices = Prices::of(request.model, prompt, request.fast)?;
        let (handle, ttl): (Option<Arc<dyn CacheWarm>>, Duration) = match request.warm? {
            Warmable::Handle(handle) if self.mode != CacheWarmingMode::Off => {
                let ttl = handle.ttl();
                (Some(Arc::from(handle)), ttl)
            }
            Warmable::Handle(handle) => (None, handle.ttl()),
            Warmable::Shadow(ttl) => (None, ttl),
        };
        let routed = handle.as_ref().and_then(|handle| handle.account());
        state.episodes_armed += 1;
        Some(Episode {
            id: state.episodes_armed,
            kind: if handle.is_some() {
                EpisodeKind::Real
            } else {
                EpisodeKind::Shadow
            },
            started: request.started,
            started_ms: wall_ms(request.started),
            epoch: request.epoch,
            prefix,
            prompt,
            model: Arc::clone(request.model),
            fast: request.fast,
            account: routed
                .map(|account| account.name.clone())
                .or(request.account),
            billing: routed.map_or(request.billing, |account| account.billing),
            prices,
            ttl,
            settled: None,
            progress: Progress::default(),
            usage: TokenUsage::default(),
            cost: 0.0,
            wrote: false,
            stopped: None,
            handle,
            events: request.events.clone(),
        })
    }

    fn schedule(self: &Arc<Self>, state: &mut State) {
        state.timer = None;
        if state
            .episode
            .as_ref()
            .is_some_and(|episode| episode.handle.is_some() && episode.stopped.is_none())
        {
            state.timer = Some(smol::spawn(warm_loop(Arc::downgrade(self))));
        }
    }

    /// When the next warm is due, or `None` once warming stopped.
    fn plan(&self) -> Option<(Instant, Duration)> {
        let mut state = self.lock();
        let episode = state.episode.as_mut()?;
        if episode.stopped.is_some() {
            return None;
        }
        let Some(delay) = episode.delay(&self.policy) else {
            episode.stopped = Some(WarmStop::NotWorthIt);
            return None;
        };
        match next_warm(
            &self.policy,
            self.mode,
            delay,
            episode.prices,
            episode.settled,
            &episode.progress,
        ) {
            Next::Warm(at) => Some((episode.started + at, at)),
            Next::Stop(reason) => {
                episode.stopped = Some(reason);
                None
            }
        }
    }

    /// The warm due at `scheduled`, once it may still go: the prefix is the
    /// same, the timer is not late, and the policy still wants it now that
    /// the run may have ended while the timer waited.
    fn fire(&self, scheduled: Duration) -> Option<Shot> {
        let mut state = self.lock();
        let prefix_current = state.guard.as_ref().is_none_or(|guard| guard());
        let episode = state.episode.as_mut()?;
        let fired = episode.elapsed();
        let delay = episode.delay(&self.policy)?;
        let stop = if !prefix_current {
            Some(WarmStop::PrefixChanged)
        } else if too_late(delay, episode.ttl, scheduled, fired) {
            Some(WarmStop::Late)
        } else if let Next::Stop(reason) = next_warm(
            &self.policy,
            self.mode,
            delay,
            episode.prices,
            episode.settled,
            &episode.progress,
        ) {
            Some(reason)
        } else {
            None
        };
        if let Some(reason) = stop {
            episode.stopped = Some(reason);
            return None;
        }
        Some(Shot {
            episode: episode.id,
            handle: Arc::clone(episode.handle.as_ref()?),
            fired,
            model: Arc::clone(&episode.model),
            fast: episode.fast,
            billing: episode.billing,
            account: episode.account.clone(),
            prompt: episode.prompt,
            phase: episode.phase(),
            events: episode.events.clone(),
        })
    }

    /// Books a warm's reply, and folds it into its episode if that is still
    /// open. `false` stops warming.
    fn warmed(&self, shot: &Shot, result: Result<TokenUsage, AgentError>) -> bool {
        let mut state = self.lock();
        let usage = match result {
            Ok(usage) => usage,
            Err(error) => {
                let unsupported = error.status() == Some(BAD_REQUEST);
                warn!(
                    target: WARM_TARGET,
                    session = %self.context.session_id,
                    agent = self.agent(),
                    model = %shot.model.id,
                    %error,
                    "cache warm failed"
                );
                if let Some(episode) = state
                    .episode
                    .as_mut()
                    .filter(|open| open.id == shot.episode)
                {
                    episode.stopped = Some(if unsupported {
                        WarmStop::Unsupported
                    } else {
                        WarmStop::Failed
                    });
                }
                state.disabled |= unsupported;
                return false;
            }
        };
        let cost = shot.model.billed_cost(&usage, shot.fast);
        let refreshed = refreshed(&usage, shot.prompt);
        info!(
            target: WARM_TARGET,
            session = %self.context.session_id,
            agent = self.agent(),
            provider = %shot.model.provider,
            model = %shot.model.id,
            account = shot.account.as_deref().unwrap_or_default(),
            phase = shot.phase,
            age_ms = u64::try_from(shot.fired.as_millis()).unwrap_or(u64::MAX),
            prompt_tokens = shot.prompt,
            input_tokens = usage.input,
            cache_read_tokens = usage.cache_read,
            cache_creation_tokens = usage.cache_creation,
            cost_usd = cost.unwrap_or_default(),
            outcome = if refreshed {
                REFRESHED
            } else {
                EpisodeOutcome::Wrote.as_str()
            },
            "cache warmed"
        );
        shot.events.try_send(AgentEvent::ModelUsage {
            usage,
            cost,
            billing: shot.billing,
            provider: shot.model.provider.to_string(),
            model: shot.model.id.clone(),
            purpose: LedgerPurpose::CacheWarm,
            account: shot.account.clone(),
        });
        if self.context.subagent.is_none() {
            state.spend.push(WarmSpend {
                usage,
                cost,
                billing: shot.billing,
            });
        }
        state.disabled |= !refreshed;
        let Some(episode) = state
            .episode
            .as_mut()
            .filter(|open| open.id == shot.episode)
        else {
            return false;
        };
        episode.progress.warms += 1;
        episode.progress.last_refresh = shot.fired;
        episode.progress.spent += cost.unwrap_or(episode.prices.read);
        episode.usage += usage;
        episode.cost += cost.unwrap_or_default();
        if !refreshed {
            episode.wrote = true;
            episode.stopped = Some(WarmStop::Wrote);
        }
        refreshed
    }

    fn close(
        &self,
        state: &mut State,
        episode: Episode,
        next: Option<NextRequest>,
        boundary: Option<Boundary>,
        reason: WarmStop,
        records: &mut Records,
    ) {
        if let (Some(next), Some(boundary)) = (next, boundary)
            && next.gap <= episode.ttl
        {
            let key = (
                episode.model.provider.to_string(),
                episode.model.id.clone(),
                boundary,
            );
            let (pairs, hits) = state.samples.entry(key).or_default();
            *pairs += 1;
            *hits += u64::from(is_hit(next.cache_read, episode.prompt));
        }
        let ended = episode.elapsed();
        let stop_reason = episode.stopped.unwrap_or(reason);
        match episode.kind {
            EpisodeKind::Real => {
                let row = self.row(
                    &episode,
                    EpisodeKind::Real,
                    self.mode,
                    &episode.progress,
                    episode.wrote,
                    next.as_ref(),
                    boundary,
                    stop_reason,
                );
                records.push(row, &episode.events);
            }
            EpisodeKind::Shadow => {
                let until = next.map_or(ended, |next| next.gap);
                for &mode in self.simulated_modes() {
                    let (progress, simulated_stop) = simulate(
                        &self.policy,
                        mode,
                        episode.ttl,
                        episode.prices,
                        episode.settled,
                        until,
                    );
                    let row = self.row(
                        &episode,
                        EpisodeKind::Shadow,
                        mode,
                        &progress,
                        false,
                        next.as_ref(),
                        boundary,
                        simulated_stop.unwrap_or(stop_reason),
                    );
                    records.push(row, &episode.events);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn row(
        &self,
        episode: &Episode,
        kind: EpisodeKind,
        mode: CacheWarmingMode,
        progress: &Progress,
        wrote: bool,
        next: Option<&NextRequest>,
        boundary: Option<Boundary>,
        stop_reason: WarmStop,
    ) -> Option<CacheWarmEpisode> {
        if progress.warms == 0 {
            return None;
        }
        let (outcome, saved_tokens) =
            classify(kind, episode.ttl, episode.prompt, progress, wrote, next);
        let (usage, warm_cost) = match kind {
            EpisodeKind::Real => (episode.usage, episode.cost),
            EpisodeKind::Shadow => (
                TokenUsage {
                    cache_read: episode.prompt.saturating_mul(progress.warms),
                    ..TokenUsage::default()
                },
                progress.spent,
            ),
        };
        Some(CacheWarmEpisode {
            session_id: self.context.session_id.clone(),
            agent: self.agent().to_owned(),
            kind,
            mode: mode.as_str().to_owned(),
            provider: episode.model.provider.to_string(),
            account: episode.account.clone().unwrap_or_default(),
            model: episode.model.id.clone(),
            cwd: self.context.cwd.clone(),
            started_at: episode.started_ms,
            ended_at: now_millis(),
            gap_ms: next.map(|next| u64::try_from(next.gap.as_millis()).unwrap_or(u64::MAX)),
            boundary: next.and(boundary),
            prompt_tokens: episode.prompt,
            warms: progress.warms,
            input_tokens: usage.input,
            cache_read: usage.cache_read,
            cache_creation: usage.cache_creation,
            warm_cost,
            subscription: episode.billing == Billing::Subscription,
            outcome,
            next_cache_read: next.map(|next| next.cache_read),
            saved_tokens,
            saved_cost: episode.prices.miss_of(saved_tokens, episode.prompt),
            stop_reason: stop_reason.as_str().to_owned(),
        })
    }

    fn flush_samples(&self, state: &mut State) {
        let Some(events) = state.events.as_ref() else {
            return;
        };
        let samples: Vec<HitSamples> = mem::take(&mut state.samples)
            .into_iter()
            .map(|((provider, model, boundary), (pairs, hits))| HitSamples {
                provider,
                model,
                boundary,
                pairs,
                hits,
            })
            .collect();
        if !samples.is_empty() {
            events.try_send(AgentEvent::CacheHitSamples(samples));
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|error| error.into_inner());
        state.timer = None;
        let mut state = mem::take(state);
        let mut records = Records::default();
        if let Some(episode) = state.episode.take() {
            self.close(
                &mut state,
                episode,
                None,
                None,
                WarmStop::Closed,
                &mut records,
            );
        }
        self.flush_samples(&mut state);
        records.emit();
    }
}

impl Records {
    fn push(&mut self, row: Option<CacheWarmEpisode>, events: &EventSender) {
        if let Some(row) = row {
            self.episodes.push((row, events.clone()));
        }
    }

    fn emit(self) {
        for (episode, events) in self.episodes {
            info!(
                target: EPISODE_TARGET,
                session = %episode.session_id,
                agent = %episode.agent,
                kind = episode.kind.as_str(),
                mode = %episode.mode,
                provider = %episode.provider,
                account = %episode.account,
                model = %episode.model,
                gap_ms = episode.gap_ms,
                prompt_tokens = episode.prompt_tokens,
                warms = episode.warms,
                warm_cost = episode.warm_cost,
                outcome = episode.outcome.as_str(),
                next_cache_read = episode.next_cache_read,
                saved_cost = episode.saved_cost,
                stop_reason = %episode.stop_reason,
                "cache warming episode ended"
            );
            events.try_send(AgentEvent::CacheWarmEpisode(Box::new(episode)));
        }
    }
}

async fn warm_loop(inner: Weak<Inner>) {
    loop {
        let Some((due, scheduled)) = inner.upgrade().and_then(|inner| inner.plan()) else {
            return;
        };
        Timer::at(due).await;
        let Some(shot) = inner.upgrade().and_then(|inner| inner.fire(scheduled)) else {
            return;
        };
        // Sent on its own task: a real request that stops this loop must not
        // abort a warm the provider may already be billing.
        let (done_tx, done_rx) = flume::bounded(1);
        let booker = inner.clone();
        smol::spawn(async move {
            let result = shot.handle.warm().await;
            let carry_on = booker
                .upgrade()
                .is_some_and(|inner| inner.warmed(&shot, result));
            done_tx.send(carry_on).ok();
        })
        .detach();
        if !done_rx.recv_async().await.unwrap_or(false) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use caudra_providers::provider::BoxFuture;
    use flume::{Receiver, Sender};
    use test_case::test_case;

    use super::*;
    use crate::types::Envelope;

    const TTL: Duration = Duration::from_secs(300);
    const WARM_AT: Duration = Duration::from_secs(270);
    const PROMPT: u32 = 200_000;
    const MODEL_SPEC: &str = "anthropic/claude-opus-4-8";
    const SESSION: &str = "session";
    const PREFIX: u64 = 1;
    const OTHER_PREFIX: u64 = 2;
    const CHEAP: Prices = Prices {
        read: 0.01,
        miss: 0.04,
    };
    const DEAR: Prices = Prices {
        read: 0.1,
        miss: 1.2,
    };
    const MID: Prices = Prices {
        read: 0.05,
        miss: 0.5,
    };
    const TEST_TTL: Duration = Duration::from_secs(2);
    const TEST_LEAD: Duration = Duration::from_millis(1900);
    const EVENT_TIMEOUT: Duration = Duration::from_secs(10);
    const SHADOW_GAP: Duration = Duration::from_secs(1000);
    const WARM_BOOKED: &str = "a warm is booked as cache_warm usage on the warmed model";
    const HELD_WARM_BOOKED: &str = "a warm sent before a real request is booked when it lands";
    const RUN_END_STOPS: &str = "running mode stops warming when the run settles";
    const WRITE_STOPS: &str = "a warm that wrote stops warming for the conversation";
    const SHADOW_SIMULATES: &str = "warming off still records what each mode would have done";

    fn timeline(warms: u32) -> Progress {
        Progress {
            last_refresh: WARM_AT * warms,
            spent: DEAR.read * f64::from(warms),
            warms,
        }
    }

    #[test_case(TTL, Duration::from_secs(10), Some(WARM_AT) ; "five_minutes")]
    #[test_case(Duration::from_secs(60), Duration::from_secs(10), Some(Duration::from_secs(50)) ; "lead_bounds_a_short_lifetime")]
    #[test_case(Duration::from_secs(10), Duration::from_secs(10), None ; "no_room_for_a_refresh")]
    fn a_refresh_lands_before_the_entry_expires(
        ttl: Duration,
        lead: Duration,
        expected: Option<Duration>,
    ) {
        assert_eq!(refresh_delay(ttl, lead), expected);
    }

    #[test_case(Duration::from_secs(284), false ; "within_the_slack")]
    #[test_case(Duration::from_secs(286), true ; "past_half_the_remaining_lifetime")]
    fn a_late_timer_gives_up(fired: Duration, late: bool) {
        assert_eq!(too_late(WARM_AT, TTL, WARM_AT, fired), late);
    }

    #[test_case(CacheWarmingMode::Running, None, DEAR, timeline(0), Next::Warm(WARM_AT) ; "first_refresh")]
    #[test_case(CacheWarmingMode::Running, None, DEAR, timeline(6), Next::Stop(WarmStop::Cap) ; "running_cap")]
    #[test_case(CacheWarmingMode::Running, Some(WARM_AT), DEAR, timeline(0), Next::Stop(WarmStop::RunEnded) ; "running_stops_at_the_end_of_the_run")]
    #[test_case(CacheWarmingMode::Idle, Some(WARM_AT), DEAR, timeline(6), Next::Warm(WARM_AT * 7) ; "idle_cap_counts_from_the_end_of_the_run")]
    #[test_case(CacheWarmingMode::Idle, Some(WARM_AT), MID, timeline(0), Next::Stop(WarmStop::NotWorthIt) ; "idle_needs_a_bigger_prompt")]
    #[test_case(CacheWarmingMode::Running, None, MID, timeline(0), Next::Warm(WARM_AT) ; "running_warms_the_same_prompt")]
    #[test_case(CacheWarmingMode::Running, None, CHEAP, timeline(0), Next::Stop(WarmStop::NotWorthIt) ; "a_small_prompt_never_warms")]
    #[test_case(CacheWarmingMode::Running, None, DEAR, Progress { spent: 1.15, ..timeline(1) }, Next::Stop(WarmStop::SpendBound) ; "spend_bound")]
    fn the_policy_schedules_each_refresh(
        mode: CacheWarmingMode,
        settled: Option<Duration>,
        prices: Prices,
        progress: Progress,
        expected: Next,
    ) {
        let policy = WarmPolicy::default();
        assert_eq!(
            next_warm(&policy, mode, WARM_AT, prices, settled, &progress),
            expected
        );
    }

    #[test_case(CacheWarmingMode::Running, None, Duration::from_secs(1000), (3, None) ; "every_refresh_inside_the_gap")]
    #[test_case(CacheWarmingMode::Running, Some(Duration::from_secs(400)), Duration::from_secs(1000), (1, Some(WarmStop::RunEnded)) ; "running_stops_at_the_end_of_the_run")]
    #[test_case(CacheWarmingMode::Running, None, Duration::from_secs(4000), (6, Some(WarmStop::Cap)) ; "running_cap")]
    fn a_shadow_counts_the_warms_the_timer_would_send(
        mode: CacheWarmingMode,
        settled: Option<Duration>,
        until: Duration,
        expected: (u32, Option<WarmStop>),
    ) {
        let (progress, stop) = simulate(&WarmPolicy::default(), mode, TTL, DEAR, settled, until);
        assert_eq!((progress.warms, stop), expected);
    }

    #[test]
    fn time_asleep_counts_toward_the_entry_lifetime() {
        let asleep = Duration::from_secs(60);
        let started_ms = now_millis() - u64::try_from(asleep.as_millis()).unwrap();
        assert!(elapsed_since(Instant::now(), started_ms) >= asleep);
    }

    fn next(gap_secs: u64, cache_read: u32) -> NextRequest {
        NextRequest {
            gap: Duration::from_secs(gap_secs),
            cache_read,
            prompt: PROMPT,
        }
    }

    #[test_case(EpisodeKind::Real, false, Some(next(600, PROMPT)), (EpisodeOutcome::Saved, PROMPT) ; "saved")]
    #[test_case(EpisodeKind::Real, false, Some(next(320, PROMPT)), (EpisodeOutcome::Marginal, 0) ; "inside_the_lifetime_edge")]
    #[test_case(EpisodeKind::Real, false, Some(next(290, PROMPT)), (EpisodeOutcome::Unneeded, 0) ; "unneeded")]
    #[test_case(EpisodeKind::Real, false, Some(next(600, 0)), (EpisodeOutcome::Missed, 0) ; "missed")]
    #[test_case(EpisodeKind::Real, false, Some(next(900, PROMPT)), (EpisodeOutcome::Lapsed, 0) ; "lapsed")]
    #[test_case(EpisodeKind::Real, false, None, (EpisodeOutcome::Unused, 0) ; "unused")]
    #[test_case(EpisodeKind::Real, true, Some(next(600, PROMPT)), (EpisodeOutcome::Wrote, 0) ; "wrote")]
    #[test_case(EpisodeKind::Shadow, false, Some(next(600, 0)), (EpisodeOutcome::Saved, PROMPT) ; "a_shadow_cannot_observe_a_miss")]
    fn episodes_are_classified_by_what_came_next(
        kind: EpisodeKind,
        wrote: bool,
        next: Option<NextRequest>,
        expected: (EpisodeOutcome, u32),
    ) {
        assert_eq!(
            classify(kind, TTL, PROMPT, &timeline(2), wrote, next.as_ref()),
            expected
        );
    }

    /// Signals when each warm is sent and replies once the test releases it.
    struct ScriptedWarm {
        usage: TokenUsage,
        sent: Sender<()>,
        release: Receiver<()>,
    }

    impl CacheWarm for ScriptedWarm {
        fn ttl(&self) -> Duration {
            TEST_TTL
        }

        fn warm(&self) -> BoxFuture<'_, Result<TokenUsage, AgentError>> {
            Box::pin(async move {
                self.sent.send(()).ok();
                self.release.recv_async().await.ok();
                Ok(self.usage)
            })
        }
    }

    struct Harness {
        warmer: CacheWarmer,
        model: Arc<Model>,
        events: EventSender,
        rx: Receiver<Envelope>,
        sent: (Sender<()>, Receiver<()>),
        release: (Sender<()>, Receiver<()>),
    }

    impl Harness {
        fn new(mode: CacheWarmingMode) -> Self {
            let (tx, rx) = flume::unbounded();
            Self {
                warmer: CacheWarmer::with_policy(
                    mode,
                    WarmPolicy {
                        lead: TEST_LEAD,
                        ..WarmPolicy::default()
                    },
                    WarmerContext {
                        session_id: SESSION.into(),
                        ..WarmerContext::default()
                    },
                ),
                model: Arc::new(Model::from_spec(MODEL_SPEC).unwrap()),
                events: EventSender::new(tx, 0),
                rx,
                sent: flume::unbounded(),
                release: flume::unbounded(),
            }
        }

        fn handle(&self, usage: TokenUsage) -> Warmable {
            Warmable::Handle(Box::new(ScriptedWarm {
                usage,
                sent: self.sent.0.clone(),
                release: self.release.1.clone(),
            }))
        }

        fn finish(&self, usage: TokenUsage, warm: TokenUsage) {
            self.finish_at(Instant::now(), PREFIX, usage, self.handle(warm));
        }

        fn finish_at(&self, started: Instant, prefix: u64, usage: TokenUsage, warm: Warmable) {
            self.warmer.finish_request(FinishedRequest {
                started,
                epoch: 0,
                prefix: Some(prefix),
                boundary: Boundary::WithinRun,
                usage,
                model: &self.model,
                fast: false,
                billing: Billing::Api,
                account: None,
                warm: Some(warm),
                events: &self.events,
            });
        }

        fn await_send(&self) {
            self.sent.1.recv_timeout(EVENT_TIMEOUT).unwrap();
        }

        fn release_one(&self) {
            self.release.0.send(()).unwrap();
        }

        fn next_event(&self) -> AgentEvent {
            self.rx.recv_timeout(EVENT_TIMEOUT).unwrap().event
        }

        fn next_episode(&self) -> CacheWarmEpisode {
            loop {
                if let AgentEvent::CacheWarmEpisode(episode) = self.next_event() {
                    return *episode;
                }
            }
        }

        fn timer_armed(&self) -> bool {
            self.warmer.inner.lock().timer.is_some()
        }
    }

    fn cached(tokens: u32) -> TokenUsage {
        TokenUsage {
            cache_read: tokens,
            ..TokenUsage::default()
        }
    }

    fn written(tokens: u32) -> TokenUsage {
        TokenUsage {
            cache_creation: tokens,
            ..TokenUsage::default()
        }
    }

    #[test]
    fn a_warm_fires_while_the_run_waits_and_is_booked_as_cache_warm() {
        let harness = Harness::new(CacheWarmingMode::Running);
        harness.finish(cached(PROMPT), cached(PROMPT));
        harness.await_send();
        harness.release_one();

        let AgentEvent::ModelUsage {
            usage,
            purpose,
            model,
            ..
        } = harness.next_event()
        else {
            panic!("{WARM_BOOKED}");
        };
        assert_eq!(
            (usage, purpose, model.as_str()),
            (
                cached(PROMPT),
                LedgerPurpose::CacheWarm,
                harness.model.id.as_str()
            ),
            "{WARM_BOOKED}"
        );
        assert!(!harness.warmer.take_spend().is_empty(), "{WARM_BOOKED}");
    }

    #[test]
    fn a_warm_in_flight_when_a_request_starts_is_still_booked() {
        let harness = Harness::new(CacheWarmingMode::Running);
        harness.finish(cached(PROMPT), cached(PROMPT));
        harness.await_send();

        harness.warmer.hold();
        harness.release_one();

        assert!(
            matches!(
                harness.next_event(),
                AgentEvent::ModelUsage {
                    purpose: LedgerPurpose::CacheWarm,
                    ..
                }
            ),
            "{HELD_WARM_BOOKED}"
        );
        assert!(!harness.timer_armed(), "{HELD_WARM_BOOKED}");
    }

    #[test_case(PREFIX, WarmStop::NextRequest ; "the_next_request_reads_the_same_prefix")]
    #[test_case(OTHER_PREFIX, WarmStop::PrefixChanged ; "the_next_request_changed_the_prefix")]
    fn an_episode_ends_with_why_it_stopped(next_prefix: u64, reason: WarmStop) {
        let harness = Harness::new(CacheWarmingMode::Running);
        harness.finish(cached(PROMPT), cached(PROMPT));
        harness.await_send();
        harness.release_one();
        harness.await_send();

        harness.finish_at(
            Instant::now(),
            next_prefix,
            cached(PROMPT),
            harness.handle(cached(PROMPT)),
        );

        assert_eq!(harness.next_episode().stop_reason, reason.as_str());
    }

    #[test]
    fn running_mode_stops_when_the_run_settles() {
        let harness = Harness::new(CacheWarmingMode::Running);
        harness.finish(cached(PROMPT), cached(PROMPT));
        assert!(harness.timer_armed(), "{RUN_END_STOPS}");

        harness.warmer.run_settled();

        assert!(!harness.timer_armed(), "{RUN_END_STOPS}");
    }

    #[test]
    fn idle_mode_keeps_warming_after_the_run_settles() {
        let harness = Harness::new(CacheWarmingMode::Idle);
        harness.finish(cached(PROMPT), cached(PROMPT));

        harness.warmer.run_settled();

        harness.await_send();
    }

    #[test]
    fn a_warm_that_writes_stops_warming_for_good() {
        let harness = Harness::new(CacheWarmingMode::Running);
        harness.finish(cached(PROMPT), written(PROMPT));
        harness.await_send();
        harness.release_one();
        assert!(matches!(
            harness.next_event(),
            AgentEvent::ModelUsage { .. }
        ));

        harness.warmer.stop(WarmStop::Closed);

        let episode = harness.next_episode();
        assert_eq!(episode.outcome, EpisodeOutcome::Wrote, "{WRITE_STOPS}");
        assert!(!harness.warmer.wants_warm(), "{WRITE_STOPS}");
    }

    #[test]
    fn warming_off_records_a_shadow_for_each_mode() {
        let harness = Harness::new(CacheWarmingMode::Off);
        let started = Instant::now();
        harness.finish_at(started, PREFIX, cached(PROMPT), Warmable::Shadow(TEST_TTL));
        assert!(!harness.timer_armed(), "{SHADOW_SIMULATES}");

        harness.finish_at(
            started + SHADOW_GAP,
            PREFIX,
            cached(PROMPT),
            Warmable::Shadow(TEST_TTL),
        );

        let modes: Vec<(EpisodeKind, String)> = harness
            .rx
            .try_iter()
            .filter_map(|envelope| match envelope.event {
                AgentEvent::CacheWarmEpisode(episode) => Some((episode.kind, episode.mode)),
                _ => None,
            })
            .collect();
        assert_eq!(
            modes,
            [
                (
                    EpisodeKind::Shadow,
                    CacheWarmingMode::Running.as_str().to_owned()
                ),
                (
                    EpisodeKind::Shadow,
                    CacheWarmingMode::Idle.as_str().to_owned()
                ),
            ],
            "{SHADOW_SIMULATES}"
        );
        assert!(harness.sent.1.is_empty(), "{SHADOW_SIMULATES}");
    }
}
