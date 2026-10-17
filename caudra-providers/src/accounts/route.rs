//! What routing tells the agent and the UI: which account served a request,
//! why it moved, and what is left when every candidate is spent.

use serde::{Deserialize, Serialize};

use super::quota::WindowKind;
use crate::model::Billing;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionMode {
    Auto,
    Pinned,
}

/// Where an account stands against the switching thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountTier {
    /// At or above `prefer_below` in every watched window.
    Healthy,
    /// Between `switch_below` and `prefer_below` somewhere.
    Low,
    /// Below `switch_below`, rate limited, or refused.
    Spent,
}

/// The account a request went to, as the status chip shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutedAccount {
    pub provider: String,
    pub name: String,
    pub billing: Billing,
    pub mode: SelectionMode,
    /// Left in the tightest watched window, when any is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tightest_window: Option<WindowKind>,
    pub tier: AccountTier,
    /// Every account of the provider, signed in or not. One means there is
    /// nothing to show.
    pub pool_size: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum SwitchReason {
    /// A watched window fell below `switch_below`.
    Spent {
        window: WindowKind,
        used_percent: u8,
    },
    /// Below `prefer_below` at a point where the cache was already cold.
    Low {
        window: WindowKind,
        used_percent: u8,
    },
    /// The provider refused for longer than is worth waiting.
    RateLimited {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until_ms: Option<u64>,
    },
    AuthFailed,
    ModelUnsupported,
    /// A better account was free at a cold point: the primary after its reset,
    /// or one further under its schedule.
    Rebalanced,
    /// The user chose another account.
    Selected,
}

/// A move between accounts in the middle of a conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountSwitch {
    pub from: String,
    pub reason: SwitchReason,
    /// The prompt the old account had cached, which the new one rebuilds.
    pub cached_tokens: u32,
}

/// Another account the user could move to when the current one is spent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountAlternative {
    pub name: String,
    pub billing: Billing,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_percent: Option<u8>,
    pub fallback: bool,
}

/// The account that is spent, when it refills, and where else to go.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountExhaustion {
    pub provider: String,
    pub account: String,
    pub mode: SelectionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<u64>,
    pub alternatives: Vec<AccountAlternative>,
}

/// Why a conversation moved to another provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum RerouteReason {
    /// Every subscription of the provider it left is spent or refused.
    Spent {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<WindowKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        used_percent: Option<u8>,
    },
    /// Every subscription of the provider it left runs low, and the cache
    /// was cold anyway.
    Low {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<WindowKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        used_percent: Option<u8>,
    },
    /// Back to the model the user chose, whose provider has quota again.
    Home,
    /// A provider earlier in the order has quota again.
    Rebalanced,
}

/// A conversation leaving one provider for another, which the agent carries
/// out by switching models before it sends the request again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reroute {
    /// The model spec the conversation leaves.
    pub from: String,
    /// The model spec it moves to.
    pub to: String,
    pub reason: RerouteReason,
    /// The prompt the old provider had cached, which the new one rebuilds.
    pub cached_tokens: u32,
}

/// What a provider did about a failed request before the agent retries it.
#[derive(Debug, Clone, PartialEq)]
pub enum Failover {
    /// Another account takes the retry, which should go out at once.
    Switched,
    /// Another provider takes the retry, on another model.
    Reroute(Box<Reroute>),
    /// The same account retries after the usual wait. `exhausted` is set when
    /// it is spent and the wait is long, so the user can choose instead.
    Stay {
        exhausted: Option<AccountExhaustion>,
    },
}

impl Failover {
    pub const STAY: Self = Self::Stay { exhausted: None };
}
