//! Native multi-account providers. Every provider slug resolves to a pool of
//! named accounts, and a routing provider sends each conversation to one of
//! them, keeping it there while its prompt cache is warm.

pub mod across;
pub mod credential;
mod health;
pub mod login;
pub mod manage;
pub mod policy;
pub mod quota;
pub mod registry;
pub mod route;
mod router;

pub use across::{CrossRoute, cross_route};
pub use credential::{AccountCredential, AccountSecret, CredentialSource};
pub use login::{
    LoginCredential, LoginTarget, SavedLogin, login_target, save_login, suggest_account_name,
};
pub use manage::{
    SignedOut, forget_account, move_account, rename_account, set_provider_order, set_strategy,
    sign_out,
};
pub use quota::{QuotaWindow, WindowKind};
pub use registry::{
    AccountKind, AccountOverrideError, AccountStatus, PoolStatus, account_providers,
    fetch_account_usage, invalidate, oauth_account, parse_account_override,
    parse_account_overrides, pool_status, require_account, select_account, set_process_selection,
};
pub use route::{
    AccountAlternative, AccountExhaustion, AccountSwitch, AccountTier, Failover, Reroute,
    RerouteReason, RoutedAccount, SelectionMode, SwitchReason,
};
pub(crate) use router::RoutedProvider;
