//! A model or thinking level the user picks while a conversation runs. The
//! running agent takes it at its next request, and a request waiting out a
//! backoff stops waiting, so a conversation stuck behind a spent quota can
//! move to another model without being stopped first.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use caudra_providers::{ThinkingConfig, model::Model, provider::Provider};
use event_listener::EventListener;

use crate::nudge::Nudge;

/// What the user asked to change, already resolved by whoever asked, so a
/// model that cannot run is refused at the pick rather than mid-run. A field
/// left `None` keeps what runs.
#[derive(Clone, Default)]
pub struct SettingsChange {
    pub route: Option<(Arc<dyn Provider>, Model)>,
    pub thinking: Option<ThinkingConfig>,
}

impl SettingsChange {
    pub fn model(provider: Arc<dyn Provider>, model: Model) -> Self {
        Self {
            route: Some((provider, model)),
            thinking: None,
        }
    }

    pub fn thinking(thinking: ThinkingConfig) -> Self {
        Self {
            route: None,
            thinking: Some(thinking),
        }
    }

    pub fn model_spec(&self) -> Option<String> {
        self.route.as_ref().map(|(_, model)| model.spec())
    }

    pub fn is_empty(&self) -> bool {
        self.route.is_none() && self.thinking.is_none()
    }

    fn merge(&mut self, later: Self) {
        if later.route.is_some() {
            self.route = later.route;
        }
        if later.thinking.is_some() {
            self.thinking = later.thinking;
        }
    }
}

impl fmt::Debug for SettingsChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsChange")
            .field("model", &self.model_spec())
            .field("thinking", &self.thinking)
            .finish()
    }
}

#[derive(Default)]
struct State {
    pending: Option<SettingsChange>,
    applied: Option<SettingsChange>,
}

/// Shared between the conversation's owner, which requests changes, and the
/// agents running it, which apply them and leave behind what they applied
/// for the next agent of the same conversation to start from.
#[derive(Clone, Default)]
pub struct LiveSwitch {
    state: Arc<Mutex<State>>,
    wake: Nudge,
}

impl LiveSwitch {
    /// Queues `change` on top of any not yet taken, and wakes a waiting request.
    pub fn request(&self, change: SettingsChange) {
        merge_into(&mut self.lock().pending, change);
        self.wake.notify();
    }

    pub fn is_pending(&self) -> bool {
        self.lock().pending.is_some()
    }

    /// The model a change not yet taken moves to.
    pub fn pending_model(&self) -> Option<String> {
        self.lock().pending.as_ref()?.model_spec()
    }

    /// Registers interest before a wait, so a change requested between the
    /// check and the wait still wakes it.
    pub(crate) fn listen(&self) -> EventListener {
        self.wake.listen()
    }

    pub(crate) fn take(&self) -> Option<SettingsChange> {
        self.lock().pending.take()
    }

    /// Hands back a change no agent took, for an owner whose own state is
    /// what the next run starts from.
    pub fn take_pending(&self) -> Option<SettingsChange> {
        self.take()
    }

    pub(crate) fn record_applied(&self, applied: SettingsChange) {
        merge_into(&mut self.lock().applied, applied);
    }

    /// What agents applied since the last call.
    pub fn take_applied(&self) -> Option<SettingsChange> {
        self.lock().applied.take()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn merge_into(slot: &mut Option<SettingsChange>, change: SettingsChange) {
    slot.get_or_insert_with(SettingsChange::default)
        .merge(change);
}

impl fmt::Debug for LiveSwitch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveSwitch")
            .field("pending", &self.lock().pending)
            .finish_non_exhaustive()
    }
}

/// What a conversation's owner holds to steer it while it runs: the picks
/// it takes, and the wake that cuts a backoff short.
#[derive(Clone, Default)]
pub struct ConversationControl {
    pub live: LiveSwitch,
    pub retry_now: Nudge,
}

impl fmt::Debug for ConversationControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConversationControl")
            .field("live", &self.live)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_later_level_replaces_an_untaken_one() {
        let live = LiveSwitch::default();
        live.request(SettingsChange::thinking(ThinkingConfig::Adaptive));
        live.request(SettingsChange::thinking(ThinkingConfig::Off));

        let change = live.take().unwrap();
        assert_eq!(change.thinking, Some(ThinkingConfig::Off));
        assert!(change.route.is_none());
        assert!(!live.is_pending());
    }

    #[test]
    fn a_request_wakes_a_listener_registered_before_it() {
        smol::block_on(async {
            let live = LiveSwitch::default();
            let listener = live.listen();
            live.request(SettingsChange::thinking(ThinkingConfig::Adaptive));
            listener.await;
            assert!(live.is_pending());
        });
    }
}
