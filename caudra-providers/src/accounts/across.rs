//! Moving a conversation to another provider's subscription once every
//! subscription of its own provider runs low, along the provider order that
//! the top-level `[switching]` of `providers.toml` lists, and back again.

use std::sync::Arc;

use caudra_config::ModelPolicy;
use caudra_config::providers::ProvidersConfig;
use caudra_storage::accounts::AccountSelection;

use super::policy::{self, Assessment, Candidate};
use super::registry::{PoolSnapshot, SlugState, peer_state, percent};
use super::route::{AccountTier, Reroute, RerouteReason};
use crate::model::{Model, ModelPurpose};
use crate::model_registry;

const SPEC_SEPARATOR: char = '/';

/// Where a conversation may go: the model the user chose, and one model on
/// each provider of the order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossRoute {
    /// The model the user chose. A conversation that moved away returns to it.
    pub home: String,
    /// One model spec per listed provider, in the order. The home provider's
    /// entry is `home`.
    pub targets: Vec<String>,
}

impl CrossRoute {
    /// The models in the order a conversation prefers them: home first.
    fn preference(&self) -> Vec<&str> {
        let mut preference = vec![self.home.as_str()];
        preference.extend(
            self.targets
                .iter()
                .map(String::as_str)
                .filter(|spec| *spec != self.home),
        );
        preference
    }
}

/// The route for a conversation on `home`, or `None` while `providers.toml`
/// lists no order or leaves `home`'s provider out of it. On each other
/// provider the conversation takes the first of the user's favorites in the
/// same size lane as `home`, else that provider's own default for the lane.
pub fn cross_route(home: &Model, policy: &ModelPolicy) -> Option<CrossRoute> {
    let config = ProvidersConfig::load();
    let order = config.provider_order();
    if !order.iter().any(|slug| *slug == *home.provider) {
        return None;
    }
    let lane = lane_of(&home.provider, &home.id);
    let favorites = model_registry::favorites();
    let targets = order
        .iter()
        .filter_map(|slug| {
            if *slug == *home.provider {
                return Some(home.spec());
            }
            favorites
                .iter()
                .find(|spec| {
                    policy.allows(spec)
                        && spec
                            .split_once(SPEC_SEPARATOR)
                            .is_some_and(|(provider, id)| {
                                provider == slug && lane_of(provider, id) == lane
                            })
                })
                .cloned()
                .or_else(|| Model::default_on(slug, lane, &home.id, policy).map(|m| m.spec()))
        })
        .collect();
    Some(CrossRoute {
        home: home.spec(),
        targets,
    })
}

/// A model nobody classified counts as a flagship, which is what a user
/// picks for a conversation far more often than a small model.
fn lane_of(provider: &str, model_id: &str) -> ModelPurpose {
    match Model::class_of(provider, model_id) {
        Some(ModelPurpose::Fast) => ModelPurpose::Fast,
        _ => ModelPurpose::Best,
    }
}

/// Where each candidate provider stands, by its best subscription.
#[derive(Debug, Clone, Copy)]
pub struct AcrossInput<'a> {
    /// Home first, then the order. `None` for a provider with no usable
    /// subscription, which is never a target.
    pub tiers: &'a [Option<AccountTier>],
    pub current: usize,
    /// Whether the conversation's prompt cache is cold, so a move is free.
    pub cold: bool,
}

/// The provider to move to, if any. A spent provider is left for the first
/// healthy one, else the first low one. At a cold point the conversation
/// takes the first healthy provider, home first, so it returns once an
/// earlier one has quota again.
pub fn decide_across(input: &AcrossInput) -> Option<usize> {
    let current = input.tiers[input.current]?;
    let others = || {
        input
            .tiers
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != input.current)
    };
    let first_other = |wanted: AccountTier| {
        others()
            .find(|(_, tier)| **tier == Some(wanted))
            .map(|(index, _)| index)
    };
    if current == AccountTier::Spent {
        return first_other(AccountTier::Healthy).or_else(|| first_other(AccountTier::Low));
    }
    if !input.cold {
        return None;
    }
    input
        .tiers
        .iter()
        .position(|tier| *tier == Some(AccountTier::Healthy))
        .filter(|index| *index != input.current)
}

/// The best standing any usable subscription of the pool has for
/// `model_id`, or the account a pin names. A pool whose every subscription
/// is refused counts as spent.
pub(crate) fn subscription_standing(
    state: &SlugState,
    snapshot: &PoolSnapshot,
    model_id: &str,
    now_ms: u64,
) -> Option<(AccountTier, Option<Assessment>)> {
    let subscriptions: Vec<_> = snapshot
        .accounts
        .iter()
        .filter(|slot| slot.billing().is_subscription())
        .filter(|slot| match &snapshot.selection {
            AccountSelection::Auto => true,
            AccountSelection::Pinned(name) => slot.name == *name,
        })
        .collect();
    if subscriptions.is_empty() {
        return None;
    }
    let best = subscriptions
        .into_iter()
        .filter(|slot| state.unusable(slot, model_id).is_none())
        .map(|slot| {
            let health = state.health.get(slot.name.as_str());
            policy::assess(
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
            )
        })
        .min_by(|first, second| {
            first.tier.cmp(&second.tier).then_with(|| {
                let left = |assessment: &Assessment| assessment.left.unwrap_or(f32::MAX);
                left(second).total_cmp(&left(first))
            })
        });
    Some(match best {
        Some(best) => (best.tier, Some(best)),
        None => (AccountTier::Spent, None),
    })
}

/// What weighing a move needs from the provider the conversation is on.
pub(crate) struct Departure<'a> {
    pub state: &'a SlugState,
    pub model_id: &'a str,
    pub cold: bool,
    pub cached_tokens: u32,
}

/// Whether the conversation should look at other providers at all, without
/// reading any of them: never on a pinned provider, nor while it is healthy
/// on the model the user chose, nor while warm and not spent.
pub(crate) fn worth_weighing(departure: &Departure, cross: &CrossRoute) -> bool {
    let state = departure.state;
    let snapshot = state.snapshot();
    if matches!(snapshot.selection, AccountSelection::Pinned(_)) {
        return false;
    }
    let Some((tier, _)) =
        subscription_standing(state, &snapshot, departure.model_id, state.now_ms())
    else {
        return false;
    };
    let home = cross.home == spec(&state.slug, departure.model_id);
    match tier {
        AccountTier::Spent => true,
        AccountTier::Healthy if home => false,
        _ => departure.cold,
    }
}

/// The move the conversation should make, reading the other providers'
/// pools. Blocks on storage, so callers run it unblocked.
pub(crate) fn weigh(departure: &Departure, cross: &CrossRoute) -> Option<Reroute> {
    let state = departure.state;
    let snapshot = state.snapshot();
    if matches!(snapshot.selection, AccountSelection::Pinned(_)) {
        return None;
    }
    let now_ms = state.now_ms();
    let (tier, assessment) = subscription_standing(state, &snapshot, departure.model_id, now_ms)?;
    let preference = cross.preference();
    let current = preference
        .iter()
        .position(|spec| provider_of(spec) == state.slug)?;
    let tiers: Vec<Option<AccountTier>> = preference
        .iter()
        .enumerate()
        .map(|(index, spec)| {
            if index == current {
                return Some(tier);
            }
            let (slug, model_id) = spec.split_once(SPEC_SEPARATOR)?;
            let peer: Arc<SlugState> = peer_state(slug)?;
            let peer_snapshot = peer.snapshot();
            subscription_standing(&peer, &peer_snapshot, model_id, now_ms).map(|(tier, _)| tier)
        })
        .collect();
    let target = decide_across(&AcrossInput {
        tiers: &tiers,
        current,
        cold: departure.cold,
    })?;
    let to = preference[target].to_owned();
    let window = assessment.as_ref().and_then(|a| a.tightest.clone());
    let used_percent = assessment.as_ref().and_then(|a| a.used).map(percent);
    let reason = match tier {
        AccountTier::Spent => RerouteReason::Spent {
            window,
            used_percent,
        },
        _ if to == cross.home => RerouteReason::Home,
        AccountTier::Low => RerouteReason::Low {
            window,
            used_percent,
        },
        AccountTier::Healthy => RerouteReason::Rebalanced,
    };
    Some(Reroute {
        from: spec(&state.slug, departure.model_id),
        to,
        reason,
        cached_tokens: departure.cached_tokens,
    })
}

fn spec(slug: &str, model_id: &str) -> String {
    format!("{slug}{SPEC_SEPARATOR}{model_id}")
}

fn provider_of(spec: &str) -> &str {
    spec.split_once(SPEC_SEPARATOR)
        .map_or(spec, |(provider, _)| provider)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use AccountTier::{Healthy, Low, Spent};

    const HOME: &str = "anthropic/claude-opus-5-5";
    const OTHER: &str = "openai/gpt-6-astra";

    #[test_case(&[Some(Spent), Some(Healthy)], 0, false, Some(1) ; "spent_leaves_warm")]
    #[test_case(&[Some(Spent), Some(Low), Some(Healthy)], 0, false, Some(2) ; "spent_prefers_a_healthy_provider")]
    #[test_case(&[Some(Spent), Some(Low)], 0, false, Some(1) ; "spent_takes_a_low_one_last")]
    #[test_case(&[Some(Spent), Some(Spent), None], 0, true, None ; "nowhere_to_go")]
    #[test_case(&[Some(Low), Some(Healthy)], 0, false, None ; "low_stays_while_warm")]
    #[test_case(&[Some(Low), Some(Healthy)], 0, true, Some(1) ; "low_leaves_at_a_cold_point")]
    #[test_case(&[Some(Low), Some(Low)], 0, true, None ; "low_everywhere_stays")]
    #[test_case(&[Some(Healthy), Some(Healthy)], 1, true, Some(0) ; "returns_home_at_a_cold_point")]
    #[test_case(&[Some(Healthy), Some(Healthy)], 1, false, None ; "stays_away_while_warm")]
    #[test_case(&[Some(Low), Some(Healthy), Some(Healthy)], 2, true, Some(1) ; "returns_to_an_earlier_provider")]
    #[test_case(&[Some(Healthy), Some(Spent)], 0, true, None ; "healthy_home_stays")]
    #[test_case(&[None, Some(Healthy)], 0, true, None ; "no_subscription_here_stays")]
    fn decides_across_providers(
        tiers: &[Option<AccountTier>],
        current: usize,
        cold: bool,
        expected: Option<usize>,
    ) {
        assert_eq!(
            decide_across(&AcrossInput {
                tiers,
                current,
                cold,
            }),
            expected
        );
    }

    #[test]
    fn home_comes_first_in_the_preference() {
        let route = CrossRoute {
            home: OTHER.into(),
            targets: vec![HOME.into(), OTHER.into()],
        };

        assert_eq!(route.preference(), [OTHER, HOME]);
    }
}
