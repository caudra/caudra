//! The provider every slug resolves to: it sends each conversation lineage to
//! one account of the slug's pool and keeps it there while its prompt cache is
//! warm, moving it only at a cold point or when the account runs out.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use flume::Sender;
use futures::future::join_all;
use serde_json::Value;
use tracing::{info, warn};

use super::across::{self, CrossRoute, Departure};
use super::credential::AccountCredential;
use super::health::{AccountHealth, BLOCKED_BILLING, BLOCKED_RATE_LIMITED, QUOTA_STALE};
use super::policy::{
    self, Assessment, Candidate, Decision, Lineage, NoAccountReason, PolicyInput, Selection,
    Unusable,
};
use super::registry::{PoolSnapshot, SlugState, no_account_error, percent, slug_state};
use super::route::{
    AccountAlternative, AccountExhaustion, AccountSwitch, AccountTier, Failover, Reroute,
    RoutedAccount, SwitchReason,
};
use crate::AgentError;
use crate::model::{Model, ModelInfo};
use crate::provider::{BoxFuture, Provider, WireRequest};
use crate::providers::Timeouts;
use crate::types::{
    CacheKey, Message, ProviderEvent, ProviderUsage, ReasoningTransport, RequestOptions,
    StreamResponse,
};
use caudra_storage::accounts::AccountSelection;

/// A rate limit this short waits on the same account, which keeps its cache.
pub(crate) const MAX_SAME_ACCOUNT_WAIT: Duration = Duration::from_secs(30);
/// How long a cache lives where the provider does not say.
pub(crate) const UNKNOWN_CACHE_IDLE: Duration = Duration::from_secs(60 * 60);
/// A billing refusal parks the account this long before it is tried again.
const BILLING_BLOCK: Duration = Duration::from_secs(60 * 60);
const LINEAGE_EVICT_IDLE: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_LINEAGES: usize = 1024;
const LINEAGE_SEPARATOR: char = '/';
const STATUS_RATE_LIMITED: u16 = 429;
const STATUS_PAYMENT_REQUIRED: u16 = 402;
const STATUS_UNAUTHORIZED: u16 = 401;
const STATUS_FORBIDDEN: u16 = 403;
const STATUS_NOT_FOUND: u16 = 404;
const EVENT_ACCOUNT_SWITCHED: &str = "account_switched";
const EVENT_ACCOUNT_FAILOVER: &str = "account_failover";
const EVENT_PROVIDER_REROUTED: &str = "provider_rerouted";

/// What the router remembers about one conversation's last request.
struct LineageState {
    account: String,
    model: String,
    system_hash: u64,
    messages: usize,
    used_at: Instant,
    prompt_tokens: u32,
    /// Set by a failover, reported with the request that follows it.
    pending: Option<(String, SwitchReason)>,
    /// Where the conversation may go once this provider runs out.
    cross: Option<Arc<CrossRoute>>,
}

#[derive(Default)]
pub(crate) struct Lineages {
    map: HashMap<String, LineageState>,
}

impl Lineages {
    fn insert(&mut self, key: String, state: LineageState) {
        if self.map.len() >= MAX_LINEAGES {
            self.map
                .retain(|_, lineage| lineage.used_at.elapsed() < LINEAGE_EVICT_IDLE);
        }
        if self.map.len() >= MAX_LINEAGES
            && let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, lineage)| lineage.used_at)
                .map(|(key, _)| key.clone())
        {
            self.map.remove(&oldest);
        }
        self.map.insert(key, state);
    }

    /// A subagent's lineage is its session's key with the task appended.
    fn parent_account(&self, key: &str) -> Option<String> {
        let (parent, _) = key.rsplit_once(LINEAGE_SEPARATOR)?;
        self.map.get(parent).map(|lineage| lineage.account.clone())
    }

    #[cfg(test)]
    pub(crate) fn age(&mut self, key: &str, by: Duration) {
        if let Some(lineage) = self.map.get_mut(key) {
            lineage.used_at -= by;
        }
    }
}

/// Where a lineage stands before a request, owned so no lock is held.
enum Standing {
    New { parent: Option<String> },
    Cold { current: String, prompt_tokens: u32 },
    Warm { current: String, prompt_tokens: u32 },
}

impl Standing {
    fn current(&self) -> Option<&str> {
        match self {
            Self::New { .. } => None,
            Self::Cold { current, .. } | Self::Warm { current, .. } => Some(current),
        }
    }

    fn prompt_tokens(&self) -> u32 {
        match self {
            Self::New { .. } => 0,
            Self::Cold { prompt_tokens, .. } | Self::Warm { prompt_tokens, .. } => *prompt_tokens,
        }
    }

    fn lineage(&self) -> Lineage<'_> {
        match self {
            Self::New { parent } => Lineage::New {
                parent: parent.as_deref(),
            },
            Self::Cold { current, .. } => Lineage::Cold { current },
            Self::Warm { current, .. } => Lineage::Warm { current },
        }
    }
}

/// The account a request resolved to.
struct Routed {
    index: usize,
    credential: AccountCredential,
    switch: Option<SwitchReason>,
}

pub struct RoutedProvider {
    state: Arc<SlugState>,
    timeouts: Timeouts,
}

impl RoutedProvider {
    /// Fails when the slug has no signed-in account, so a provider with no
    /// credential reads as unavailable just as before accounts existed.
    pub(crate) fn new(slug: &str, timeouts: Timeouts) -> Result<Self, AgentError> {
        Self::with_state(slug_state(slug)?, timeouts)
    }

    pub(crate) fn with_state(
        state: Arc<SlugState>,
        timeouts: Timeouts,
    ) -> Result<Self, AgentError> {
        let router = Self { state, timeouts };
        let snapshot = router.state.snapshot();
        let credential = router
            .preferred(&snapshot, "")
            .map(|routed| routed.credential)
            .or_else(|_| {
                snapshot
                    .accounts
                    .iter()
                    .find_map(|slot| slot.credential.clone())
                    .ok_or_else(|| no_account_error(&router.state.slug))
            })?;
        router.state.instance(&credential, timeouts)?;
        Ok(router)
    }

    fn healths(&self, snapshot: &PoolSnapshot) -> Vec<AccountHealth> {
        snapshot
            .accounts
            .iter()
            .map(|slot| self.state.health.get(slot.name.as_str()))
            .collect()
    }

    fn candidates<'s>(
        &self,
        snapshot: &'s PoolSnapshot,
        healths: &'s [AccountHealth],
        model_id: &str,
    ) -> Vec<Candidate<'s>> {
        snapshot
            .accounts
            .iter()
            .zip(healths)
            .map(|(slot, health)| Candidate {
                name: slot.name.as_str(),
                billing: slot.billing(),
                fallback: slot.fallback,
                unusable: self.state.unusable(slot, model_id),
                windows: &health.windows,
                blocked_until: health.blocked_until,
            })
            .collect()
    }

    fn decide(
        &self,
        snapshot: &PoolSnapshot,
        model_id: &str,
        lineage: Lineage<'_>,
        now_ms: u64,
    ) -> Decision {
        let healths = self.healths(snapshot);
        let candidates = self.candidates(snapshot, &healths, model_id);
        policy::decide(&PolicyInput {
            candidates: &candidates,
            selection: match &snapshot.selection {
                AccountSelection::Auto => Selection::Auto,
                AccountSelection::Pinned(name) => Selection::Pinned(name.as_str()),
            },
            strategy: snapshot.strategy,
            thresholds: snapshot.thresholds,
            watched: snapshot.watched.as_deref(),
            model_id,
            lineage,
            now_ms,
        })
    }

    /// The account's tier and headroom, whether or not it is usable.
    fn assess(
        &self,
        snapshot: &PoolSnapshot,
        index: usize,
        model_id: &str,
        now_ms: u64,
    ) -> (AccountHealth, Assessment) {
        let slot = &snapshot.accounts[index];
        let health = self.state.health.get(slot.name.as_str());
        let assessment = policy::assess(
            &Candidate {
                name: slot.name.as_str(),
                billing: slot.billing(),
                fallback: slot.fallback,
                unusable: None,
                windows: &health.windows,
                blocked_until: health.blocked_until,
            },
            snapshot.thresholds,
            snapshot.watched.as_deref(),
            model_id,
            now_ms,
        );
        (health, assessment)
    }

    fn resolve(
        &self,
        snapshot: &PoolSnapshot,
        decision: Decision,
        model_id: &str,
    ) -> Result<Routed, AgentError> {
        let slug = &self.state.slug;
        match decision {
            Decision::Use { index, switch, .. } => {
                let credential = snapshot.accounts[index]
                    .credential
                    .clone()
                    .ok_or_else(|| no_account_error(slug))?;
                Ok(Routed {
                    index,
                    credential,
                    switch,
                })
            }
            Decision::NoAccount { reason } => Err(no_route_error(slug, snapshot, reason, model_id)),
        }
    }

    /// The account a request with no lineage goes to.
    fn preferred(&self, snapshot: &PoolSnapshot, model_id: &str) -> Result<Routed, AgentError> {
        let decision = self.decide(
            snapshot,
            model_id,
            Lineage::New { parent: None },
            self.state.now_ms(),
        );
        self.resolve(snapshot, decision, model_id)
    }

    fn preferred_instance(&self, model_id: &str) -> Result<Arc<dyn Provider>, AgentError> {
        let snapshot = self.state.snapshot();
        let routed = self.preferred(&snapshot, model_id)?;
        self.state.instance(&routed.credential, self.timeouts)
    }

    fn standing(
        &self,
        key: Option<&str>,
        model: &Model,
        system_hash: u64,
        messages: usize,
    ) -> Standing {
        let Some(key) = key else {
            return Standing::New { parent: None };
        };
        let (current, model_id, hash, count, used_at, prompt_tokens) = {
            let lineages = self.state.lineages.lock().unwrap();
            match lineages.map.get(key) {
                None => {
                    return Standing::New {
                        parent: lineages.parent_account(key),
                    };
                }
                Some(lineage) => (
                    lineage.account.clone(),
                    lineage.model.clone(),
                    lineage.system_hash,
                    lineage.messages,
                    lineage.used_at,
                    lineage.prompt_tokens,
                ),
            }
        };
        let snapshot = self.state.snapshot();
        let ttl = snapshot
            .position(&current)
            .and_then(|index| snapshot.accounts[index].credential.as_ref())
            .and_then(|credential| self.state.instance(credential, self.timeouts).ok())
            .and_then(|instance| instance.prompt_cache_ttl(model))
            .unwrap_or(UNKNOWN_CACHE_IDLE);
        let cold = model_id != model.id
            || hash != system_hash
            || messages < count
            || used_at.elapsed() >= ttl;
        if cold {
            Standing::Cold {
                current,
                prompt_tokens,
            }
        } else {
            Standing::Warm {
                current,
                prompt_tokens,
            }
        }
    }

    fn routed_account(
        &self,
        snapshot: &PoolSnapshot,
        index: usize,
        model_id: &str,
        now_ms: u64,
    ) -> RoutedAccount {
        let slot = &snapshot.accounts[index];
        let (_, assessment) = self.assess(snapshot, index, model_id, now_ms);
        RoutedAccount {
            provider: self.state.slug.clone(),
            name: slot.name.to_string(),
            billing: slot.billing(),
            mode: snapshot.mode(),
            left_percent: assessment.left.map(percent),
            tightest_window: assessment.tightest,
            tier: assessment.tier,
            pool_size: snapshot.accounts.len(),
        }
    }

    /// Asks the other signed-in accounts for their quota when their reading
    /// is stale, so the next decision has something to go on. Never awaited.
    fn refresh_stale_quota(&self, snapshot: &PoolSnapshot, chosen: usize, now_ms: u64) {
        if !self.state.shares_health() {
            return;
        }
        for (index, slot) in snapshot.accounts.iter().enumerate() {
            let Some(credential) = slot.credential.clone() else {
                continue;
            };
            if index == chosen || !self.state.health.claim_fetch(slot.name.as_str(), now_ms) {
                continue;
            }
            let state = Arc::clone(&self.state);
            let timeouts = self.timeouts;
            smol::spawn(async move {
                let name = credential.id.name.to_string();
                let Ok(instance) = state.instance(&credential, timeouts) else {
                    return;
                };
                match instance.fetch_usage().await {
                    Ok(Some(usage)) => {
                        let windows: Vec<_> = usage
                            .limits
                            .iter()
                            .filter_map(crate::types::UsageLimit::quota_window)
                            .collect();
                        if windows.is_empty() {
                            state.health.mark_no_usage(&name);
                        }
                        let row = state.health.observe(&name, windows, state.now_ms());
                        state.persist_health(row);
                    }
                    Ok(None) => state.health.mark_no_usage(&name),
                    Err(error) => {
                        warn!(provider = %state.slug, account = %name, %error, "background quota check failed");
                    }
                }
            })
            .detach();
        }
    }

    fn record_lineage(&self, key: &str, lineage: LineageState) {
        self.state
            .lineages
            .lock()
            .unwrap()
            .insert(key.to_owned(), lineage);
    }

    /// The other provider the conversation should move to, reading their
    /// pools only when this one gives a reason to.
    async fn cross_reroute(
        &self,
        model_id: &str,
        cross: Arc<CrossRoute>,
        cold: bool,
        cached_tokens: u32,
    ) -> Option<Reroute> {
        let departure = Departure {
            state: &self.state,
            model_id,
            cold,
            cached_tokens,
        };
        if !across::worth_weighing(&departure, &cross) {
            return None;
        }
        let state = Arc::clone(&self.state);
        let model_id = model_id.to_owned();
        let reroute = smol::unblock(move || {
            across::weigh(
                &Departure {
                    state: &state,
                    model_id: &model_id,
                    cold,
                    cached_tokens,
                },
                &cross,
            )
        })
        .await?;
        info!(
            event = EVENT_PROVIDER_REROUTED,
            from = %reroute.from,
            to = %reroute.to,
            reason = ?reroute.reason,
            cached_tokens = reroute.cached_tokens,
            "moving a conversation to another provider"
        );
        Some(reroute)
    }

    fn take_pending(&self, key: Option<&str>) -> Option<(String, SwitchReason)> {
        let key = key?;
        self.state
            .lineages
            .lock()
            .unwrap()
            .map
            .get_mut(key)
            .and_then(|lineage| lineage.pending.take())
    }

    fn exhaustion(
        &self,
        snapshot: &PoolSnapshot,
        current: usize,
        model_id: &str,
        until_ms: Option<u64>,
    ) -> AccountExhaustion {
        let healths = self.healths(snapshot);
        let now_ms = self.state.now_ms();
        let current_name = snapshot.accounts[current].name.to_string();
        let candidates = self.candidates(snapshot, &healths, model_id);
        let alternatives = policy::alternatives(
            &PolicyInput {
                candidates: &candidates,
                selection: Selection::Auto,
                strategy: snapshot.strategy,
                thresholds: snapshot.thresholds,
                watched: snapshot.watched.as_deref(),
                model_id,
                lineage: Lineage::Warm {
                    current: &current_name,
                },
                now_ms,
            },
            current,
        );
        AccountExhaustion {
            provider: self.state.slug.clone(),
            account: current_name,
            mode: snapshot.mode(),
            until_ms,
            alternatives: alternatives
                .into_iter()
                .map(|index| {
                    let routed = self.routed_account(snapshot, index, model_id, now_ms);
                    AccountAlternative {
                        name: routed.name,
                        billing: routed.billing,
                        left_percent: routed.left_percent,
                        fallback: snapshot.accounts[index].fallback,
                    }
                })
                .collect(),
        }
    }
}

fn hash_text(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn no_route_error(
    slug: &str,
    snapshot: &PoolSnapshot,
    reason: NoAccountReason,
    model_id: &str,
) -> AgentError {
    let pinned = match &snapshot.selection {
        AccountSelection::Pinned(name) => name.to_string(),
        AccountSelection::Auto => String::new(),
    };
    let message = match reason {
        NoAccountReason::Empty => return no_account_error(slug),
        NoAccountReason::PinnedMissing => format!(
            "{slug} account `{pinned}` is selected but does not exist; run `caudra auth use {slug} auto` or `caudra auth login {slug} --name {pinned}`"
        ),
        NoAccountReason::PinnedUnusable(Unusable::SignedOut) => format!(
            "{slug} account `{pinned}` is selected but signed out; run `caudra auth login {slug} --name {pinned}`"
        ),
        NoAccountReason::PinnedUnusable(Unusable::AuthFailed) => {
            return AgentError::api(
                STATUS_UNAUTHORIZED,
                format!("{slug} account `{pinned}` was refused; sign in again"),
            );
        }
        NoAccountReason::PinnedUnusable(Unusable::ModelUnsupported) => {
            return AgentError::api(
                STATUS_NOT_FOUND,
                format!("{slug} account `{pinned}` does not offer {model_id}"),
            );
        }
        NoAccountReason::NothingUsable => format!(
            "no {slug} account can take this request; check `caudra auth status` or sign in again"
        ),
    };
    AgentError::Config { message }
}

/// The reason a refused account leaves, and for how long it stays parked.
enum Refusal {
    RateLimited { until_ms: u64 },
    Billing { until_ms: u64 },
    Auth,
    Model,
}

impl Provider for RoutedProvider {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        cache_key: Option<&'a CacheKey>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let key = cache_key.map(CacheKey::as_str);
            let system_hash = hash_text(system);
            let standing = self.standing(key, model, system_hash, messages.len());
            if !matches!(standing, Standing::Warm { .. }) {
                let state = Arc::clone(&self.state);
                smol::unblock(move || {
                    state.refresh(false);
                    state.reload_health();
                })
                .await;
            }
            if let Some(cross) = opts.cross.clone()
                && let Some(reroute) = self
                    .cross_reroute(
                        &model.id,
                        cross,
                        !matches!(standing, Standing::Warm { .. }),
                        standing.prompt_tokens(),
                    )
                    .await
            {
                return Err(AgentError::Rerouted(Box::new(reroute)));
            }
            let snapshot = self.state.snapshot();
            let now_ms = self.state.now_ms();
            let decision = self.decide(&snapshot, &model.id, standing.lineage(), now_ms);
            let routed = self.resolve(&snapshot, decision, &model.id)?;
            let name = routed.credential.id.name.to_string();
            let before = self.routed_account(&snapshot, routed.index, &model.id, now_ms);
            let pending = self.take_pending(key);
            let moved_from = standing
                .current()
                .filter(|current| *current != name)
                .map(str::to_owned)
                .or_else(|| pending.as_ref().map(|(from, _)| from.clone()))
                .filter(|from| *from != name);
            if let Some(key) = key {
                self.record_lineage(
                    key,
                    LineageState {
                        account: name.clone(),
                        model: model.id.clone(),
                        system_hash,
                        messages: messages.len(),
                        used_at: Instant::now(),
                        prompt_tokens: standing.prompt_tokens(),
                        pending: None,
                        cross: opts.cross.clone(),
                    },
                );
                if moved_from.is_some() || matches!(standing, Standing::New { .. }) {
                    let switch = moved_from.map(|from| AccountSwitch {
                        reason: routed
                            .switch
                            .clone()
                            .or_else(|| pending.map(|(_, reason)| reason))
                            .unwrap_or(SwitchReason::Rebalanced),
                        from,
                        cached_tokens: standing.prompt_tokens(),
                    });
                    if let Some(switch) = &switch {
                        info!(
                            event = EVENT_ACCOUNT_SWITCHED,
                            provider = %self.state.slug,
                            from = %switch.from,
                            to = %name,
                            reason = ?switch.reason,
                            cached_tokens = switch.cached_tokens,
                            "moved a conversation to another account"
                        );
                    }
                    let _ = event_tx.send(ProviderEvent::AccountRouted {
                        account: before.clone(),
                        switch,
                    });
                }
            }
            self.refresh_stale_quota(&snapshot, routed.index, now_ms);
            let instance = self.state.instance(&routed.credential, self.timeouts)?;
            let mut response = instance
                .stream_message(model, messages, system, tools, event_tx, opts, cache_key)
                .await?;
            let now_ms = self.state.now_ms();
            let quota = std::mem::take(&mut response.quota);
            let observed = self.state.health.observe(&name, quota, now_ms);
            self.state.persist_health(observed);
            let served = self.state.health.served(&name, now_ms);
            self.state.persist_health(served);
            if let Some(key) = key
                && let Some(lineage) = self.state.lineages.lock().unwrap().map.get_mut(key)
            {
                lineage.prompt_tokens = response.usage.total_input();
                lineage.used_at = Instant::now();
            }
            let after = self.routed_account(&snapshot, routed.index, &model.id, now_ms);
            if key.is_some()
                && (after.left_percent, after.tier) != (before.left_percent, before.tier)
            {
                let _ = event_tx.send(ProviderEvent::AccountRouted {
                    account: after.clone(),
                    switch: None,
                });
            }
            response.account = Some(after);
            Ok(response)
        })
    }

    fn wire_request(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        opts: &RequestOptions,
        cache_key: Option<&CacheKey>,
    ) -> Result<WireRequest, AgentError> {
        let key = cache_key.map(CacheKey::as_str);
        let account = key.and_then(|key| {
            self.state
                .lineages
                .lock()
                .unwrap()
                .map
                .get(key)
                .map(|lineage| lineage.account.clone())
        });
        let snapshot = self.state.snapshot();
        let credential = match account
            .and_then(|name| snapshot.position(&name))
            .and_then(|index| snapshot.accounts[index].credential.clone())
        {
            Some(credential) => credential,
            None => self.preferred(&snapshot, &model.id)?.credential,
        };
        self.state
            .instance(&credential, self.timeouts)?
            .wire_request(model, messages, system, tools, opts, cache_key)
    }

    /// Every signed-in account's models, the preferred account's first.
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async move {
            let snapshot = self.state.snapshot();
            let preferred = self
                .preferred(&snapshot, "")
                .ok()
                .map(|routed| routed.index);
            let mut order: Vec<usize> = preferred.into_iter().collect();
            order.extend(
                (0..snapshot.accounts.len())
                    .filter(|index| Some(*index) != preferred)
                    .filter(|index| snapshot.accounts[*index].credential.is_some()),
            );
            let instances: Vec<Arc<dyn Provider>> = order
                .into_iter()
                .filter_map(|index| snapshot.accounts[index].credential.clone())
                .filter_map(|credential| self.state.instance(&credential, self.timeouts).ok())
                .collect();
            let listed = join_all(instances.iter().map(|instance| instance.list_models())).await;
            let mut models: Vec<ModelInfo> = Vec::new();
            let mut first_error = None;
            for result in listed {
                match result {
                    Ok(listing) => {
                        for model in listing {
                            if models.iter().all(|known| known.id != model.id) {
                                models.push(model);
                            }
                        }
                    }
                    Err(error) => {
                        first_error.get_or_insert(error);
                    }
                }
            }
            match (models.is_empty(), first_error) {
                (true, Some(error)) => Err(error),
                _ => Ok(models),
            }
        })
    }

    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        Box::pin(async move {
            let snapshot = self.state.snapshot();
            let routed = self.preferred(&snapshot, "")?;
            let name = routed.credential.id.name.to_string();
            let usage = self
                .state
                .instance(&routed.credential, self.timeouts)?
                .fetch_usage()
                .await?;
            if let Some(usage) = &usage {
                let windows: Vec<_> = usage
                    .limits
                    .iter()
                    .filter_map(crate::types::UsageLimit::quota_window)
                    .collect();
                let row = self
                    .state
                    .health
                    .observe(&name, windows, self.state.now_ms());
                self.state.persist_health(row);
            }
            Ok(usage)
        })
    }

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            self.state.clear_auth_failures();
            let state = Arc::clone(&self.state);
            smol::unblock(move || state.refresh(true)).await;
            let mut last_error = None;
            let mut refreshed = false;
            for instance in self.state.built_instances() {
                match instance.refresh_auth().await {
                    Ok(()) => refreshed = true,
                    Err(error) => last_error = Some(error),
                }
            }
            match (refreshed, last_error) {
                (false, Some(error)) => Err(error),
                _ => Ok(()),
            }
        })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            self.state.clear_auth_failures();
            let state = Arc::clone(&self.state);
            smol::unblock(move || state.refresh(true)).await;
            for instance in self.state.built_instances() {
                instance.reload_auth().await?;
            }
            Ok(())
        })
    }

    fn reload_auth_if_changed(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
        Box::pin(async move {
            let state = Arc::clone(&self.state);
            let mut changed = smol::unblock(move || state.refresh(true)).await;
            for instance in self.state.built_instances() {
                changed |= instance.reload_auth_if_changed().await?;
            }
            if changed {
                self.state.clear_auth_failures();
            }
            Ok(changed)
        })
    }

    fn failover<'a>(
        &'a self,
        error: &'a AgentError,
        cache_key: Option<&'a CacheKey>,
    ) -> BoxFuture<'a, Failover> {
        Box::pin(async move {
            let key = cache_key.map(CacheKey::as_str);
            let snapshot = self.state.snapshot();
            let lineage = key.and_then(|key| {
                self.state
                    .lineages
                    .lock()
                    .unwrap()
                    .map
                    .get(key)
                    .map(|lineage| {
                        (
                            lineage.account.clone(),
                            lineage.model.clone(),
                            lineage.cross.clone(),
                            lineage.prompt_tokens,
                        )
                    })
            });
            let (current_name, model_id, cross, prompt_tokens) = match lineage {
                Some(found) => found,
                None => match self.preferred(&snapshot, "") {
                    Ok(routed) => (
                        routed.credential.id.name.to_string(),
                        String::new(),
                        None,
                        0,
                    ),
                    Err(_) => return Failover::STAY,
                },
            };
            let Some(current) = snapshot.position(&current_name) else {
                return Failover::STAY;
            };
            let now_ms = self.state.now_ms();
            let pinned = matches!(snapshot.selection, AccountSelection::Pinned(_));
            let Some(refusal) = self.classify(error, &snapshot, current, &model_id, now_ms) else {
                return Failover::STAY;
            };
            let until_ms = match &refusal {
                Refusal::RateLimited { until_ms } | Refusal::Billing { until_ms } => {
                    let reason = match refusal {
                        Refusal::Billing { .. } => BLOCKED_BILLING,
                        _ => BLOCKED_RATE_LIMITED,
                    };
                    let row = self
                        .state
                        .health
                        .block(&current_name, *until_ms, reason, now_ms);
                    self.state.persist_health(row);
                    Some(*until_ms)
                }
                Refusal::Auth if !pinned => {
                    self.state.mark_auth_failed(&current_name);
                    None
                }
                Refusal::Model if !pinned => {
                    self.state.mark_model_unsupported(&current_name, &model_id);
                    None
                }
                Refusal::Auth | Refusal::Model => return Failover::STAY,
            };
            let exhausted = |router: &Self| Failover::Stay {
                exhausted: until_ms
                    .map(|until| router.exhaustion(&snapshot, current, &model_id, Some(until))),
            };
            if pinned {
                return exhausted(self);
            }
            if let Some(cross) = cross
                && let Some(reroute) = self
                    .cross_reroute(&model_id, cross, false, prompt_tokens)
                    .await
            {
                return Failover::Reroute(Box::new(reroute));
            }
            let decision = self.decide(
                &snapshot,
                &model_id,
                Lineage::Warm {
                    current: &current_name,
                },
                now_ms,
            );
            let (index, reason) = match decision {
                Decision::Use {
                    index,
                    switch: Some(reason),
                    ..
                } if index != current => (index, reason),
                Decision::Use { .. } | Decision::NoAccount { .. } => {
                    if until_ms.is_none() {
                        self.state.forgive(&current_name, &model_id);
                    }
                    return exhausted(self);
                }
            };
            let next = snapshot.accounts[index].name.to_string();
            warn!(
                event = EVENT_ACCOUNT_FAILOVER,
                provider = %self.state.slug,
                from = %current_name,
                to = %next,
                status = error.status(),
                "moving a refused conversation to another account"
            );
            if let Some(key) = key
                && let Some(lineage) = self.state.lineages.lock().unwrap().map.get_mut(key)
            {
                lineage.account = next;
                lineage.pending = Some((current_name, reason));
            }
            Failover::Switched
        })
    }

    fn adjust_model(&self, model: &mut Model) {
        let snapshot = self.state.snapshot();
        let Ok(routed) = self.preferred(&snapshot, &model.id) else {
            return;
        };
        if let Ok(instance) = self.state.instance(&routed.credential, self.timeouts) {
            instance.adjust_model(model);
        }
        model.billing = routed.credential.billing();
    }

    fn reasoning_transport(&self, model: &Model) -> ReasoningTransport {
        self.preferred_instance(&model.id)
            .map_or(ReasoningTransport::Other, |instance| {
                instance.reasoning_transport(model)
            })
    }

    fn prompt_cache_ttl(&self, model: &Model) -> Option<Duration> {
        self.preferred_instance(&model.id)
            .ok()
            .and_then(|instance| instance.prompt_cache_ttl(model))
    }
}

impl RoutedProvider {
    fn classify(
        &self,
        error: &AgentError,
        snapshot: &PoolSnapshot,
        current: usize,
        model_id: &str,
        now_ms: u64,
    ) -> Option<Refusal> {
        match error.status()? {
            STATUS_RATE_LIMITED => {
                let wait = match error.retry_after() {
                    Some(wait) if wait <= MAX_SAME_ACCOUNT_WAIT => return None,
                    Some(wait) => wait,
                    None => self.spent_wait(snapshot, current, model_id, now_ms)?,
                };
                Some(Refusal::RateLimited {
                    until_ms: now_ms.saturating_add(wait.as_millis() as u64),
                })
            }
            STATUS_PAYMENT_REQUIRED => Some(Refusal::Billing {
                until_ms: now_ms.saturating_add(BILLING_BLOCK.as_millis() as u64),
            }),
            STATUS_UNAUTHORIZED | STATUS_FORBIDDEN => Some(Refusal::Auth),
            STATUS_NOT_FOUND if error.is_model_unavailable() => Some(Refusal::Model),
            _ => None,
        }
    }

    /// A rate limit without a wait counts as running out only when the
    /// account's own readings say it is spent; it then waits for the window
    /// that binds it, or a stale period when that is unknown.
    fn spent_wait(
        &self,
        snapshot: &PoolSnapshot,
        current: usize,
        model_id: &str,
        now_ms: u64,
    ) -> Option<Duration> {
        let (health, assessment) = self.assess(snapshot, current, model_id, now_ms);
        if assessment.tier != AccountTier::Spent {
            return None;
        }
        let reset = health
            .windows
            .iter()
            .filter(|window| Some(&window.kind) == assessment.tightest.as_ref())
            .find_map(|window| window.resets_at)
            .filter(|reset| *reset > now_ms);
        Some(reset.map_or(QUOTA_STALE, |reset| Duration::from_millis(reset - now_ms)))
    }
}

#[cfg(test)]
mod tests;
