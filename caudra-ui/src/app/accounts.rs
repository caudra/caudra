//! Where a session's requests go among a provider's accounts: `/accounts`,
//! the row a switch leaves in the transcript, kept so a restore draws it
//! again, and the card and row a spent account raises so the user can move
//! elsewhere.

use std::sync::Arc;

use caudra_agent::automation::clock::{Clock, SystemClock};
use caudra_config::providers::{ProvidersConfig, resolve_display_name};
use caudra_providers::accounts::{
    AccountAlternative, AccountExhaustion, AccountKind, AccountStatus, AccountSwitch, PoolStatus,
    RoutedAccount, SelectionMode, SwitchReason, fetch_account_usage, forget_account, move_account,
    pool_status, rename_account, select_account, set_process_selection, set_strategy,
};
use caudra_providers::{AgentError, Billing, Timeouts};
use caudra_storage::account_switches::StoredAccountSwitch;
use caudra_storage::accounts::AccountSelection;
use caudra_storage::sessions::SessionDatabase;
use crossterm::event::{KeyEvent, MouseEvent};
use tracing::warn;

use super::App;
use super::goal::LogPlace;
use crate::agent::AgentCommand;
use crate::components::accounts_modal::{AccountsAction, AccountsView};
use crate::components::status_bar::AccountChip;
use crate::components::{Action, DisplayMessage, DisplayRole, format_compact, format_elapsed};
use crate::repaint::Dirty;

const ACCOUNT_SWITCH_LOG: &str = "account switches";
const SWITCH_HINT: &str = "Ctrl+X i chooses another account";
const MILLIS_PER_SECOND: u64 = 1_000;
const NO_SUBSCRIPTION: &str = "No signed-in subscription to ask for its quota.";
const MIN_POOL_FOR_CHIP: usize = 2;

impl App {
    pub(super) fn open_accounts(&mut self) {
        let provider = self.state.model.provider.to_string();
        match self.accounts_view(&provider) {
            Ok(view) => self.accounts_modal.open(view),
            Err(error) => self.flash(error.to_string()),
        }
    }

    pub(super) fn accounts_key(&mut self, key: KeyEvent) -> Vec<Action> {
        let provider = self.accounts_modal.provider().map(str::to_owned);
        let action = self.accounts_modal.handle_key(key);
        self.handle_accounts_action(provider, action)
    }

    pub(super) fn accounts_mouse(&mut self, event: MouseEvent) -> Vec<Action> {
        let provider = self.accounts_modal.provider().map(str::to_owned);
        let action = self.accounts_modal.handle_mouse(event);
        self.handle_accounts_action(provider, action)
    }

    /// `provider` is the one the modal showed before the key, since a choice
    /// on the card closes it.
    fn handle_accounts_action(
        &mut self,
        provider: Option<String>,
        action: AccountsAction,
    ) -> Vec<Action> {
        let Some(provider) = provider else {
            return Vec::new();
        };
        let display = display_name(&provider);
        let mut keep = None;
        let result = match action {
            AccountsAction::Consumed | AccountsAction::Closed => return Vec::new(),
            AccountsAction::Copy(text) => {
                self.copy_to_clipboard(&text);
                return Vec::new();
            }
            AccountsAction::SignIn(account) => {
                self.accounts_modal.close();
                let action =
                    self.login_picker
                        .open_provider(self.storage.clone(), &provider, account);
                return self.handle_login_picker_action(action);
            }
            AccountsAction::Refresh => {
                self.refresh_accounts(provider);
                return Vec::new();
            }
            AccountsAction::Select { selection, retry } => {
                select_account(&self.storage, &provider, &selection).map(|()| {
                    self.flash(selected_message(&display, &selection));
                    if retry {
                        self.retry_now();
                    }
                })
            }
            AccountsAction::UseForRun(name) => {
                self.flash(format!(
                    "{display} requests go to {name} until Caudra exits"
                ));
                set_process_selection(&provider, Some(AccountSelection::Pinned(name)));
                self.retry_now();
                Ok(())
            }
            AccountsAction::Rename { from, to } => {
                keep = Some(to.to_string());
                rename_account(&self.storage, &provider, &from, &to)
            }
            AccountsAction::Remove(name) => {
                forget_account(&self.storage, &provider, &name).map(drop)
            }
            AccountsAction::Move { name, offset } => {
                keep = Some(name.to_string());
                move_account(&provider, &name, offset).map(drop)
            }
            AccountsAction::Strategy(strategy) => set_strategy(&provider, strategy),
            AccountsAction::Browse => Ok(()),
        };
        self.reload_accounts(&provider, result.err().map(|error| error.to_string()));
        if let Some(name) = keep {
            self.accounts_modal.select(&name);
        }
        Vec::new()
    }

    /// Asks every signed-in subscription for its quota off the UI thread;
    /// [`Self::poll_accounts_refresh`] redraws the list once all have answered.
    fn refresh_accounts(&mut self, provider: String) {
        let accounts: Vec<String> = match pool_status(&provider, None) {
            Ok(pool) => pool
                .accounts
                .into_iter()
                .filter(|account| account.kind == AccountKind::Subscription)
                .map(|account| account.name)
                .collect(),
            Err(error) => {
                self.accounts_modal.show_error(error.to_string());
                return;
            }
        };
        if accounts.is_empty() {
            self.accounts_modal.show_error(NO_SUBSCRIPTION.into());
            return;
        }
        let slot = Arc::clone(&self.accounts_refresh);
        smol::spawn(async move {
            let mut failures = Vec::new();
            for account in accounts {
                if let Err(error) =
                    fetch_account_usage(&provider, &account, Timeouts::default()).await
                {
                    failures.push(format!("{account}: {error}"));
                }
            }
            slot.store(Some(Arc::new(failures)));
        })
        .detach();
    }

    pub(super) fn poll_accounts_refresh(&mut self) -> Dirty {
        let Some(failures) = self.accounts_refresh.swap(None) else {
            return Dirty::NO;
        };
        let Some(provider) = self.accounts_modal.provider().map(str::to_owned) else {
            return Dirty::NO;
        };
        self.reload_accounts(
            &provider,
            (!failures.is_empty()).then(|| failures.join("; ")),
        );
        Dirty::YES
    }

    fn reload_accounts(&mut self, provider: &str, error: Option<String>) {
        match self.accounts_view(provider) {
            Ok(view) => self.accounts_modal.refresh(view, error),
            Err(error) => self.accounts_modal.show_error(error.to_string()),
        }
    }

    fn accounts_view(&self, provider: &str) -> Result<AccountsView, AgentError> {
        let model = &self.state.model;
        let model_id = (*model.provider == *provider).then_some(model.id.as_str());
        Ok(AccountsView {
            provider: provider.to_owned(),
            display: display_name(provider),
            pool: pool_status(provider, model_id)?,
            current: self
                .routed_account
                .as_ref()
                .filter(|account| account.provider == provider)
                .map(|account| account.name.clone()),
            now_ms: now_ms(),
            clock: self.ui_config.clock_format,
        })
    }

    /// The model's provider's accounts for `/usage`, the one requests go to
    /// first. None when its pool cannot be read: the list is a footnote to
    /// the quota, which says what went wrong on its own.
    pub(super) fn reload_usage_accounts(&mut self) {
        let model = &self.state.model;
        let current = self
            .routed_account
            .as_ref()
            .filter(|account| *account.provider == *model.provider)
            .map(|account| account.name.as_str());
        self.usage_accounts = pool_status(&model.provider, Some(&model.id))
            .map(|pool| current_first(pool, current))
            .unwrap_or_default();
    }

    /// Cuts the wait of the request a spent account holds up, as the status
    /// bar's "retry now" does.
    fn retry_now(&mut self) {
        self.main_chat().clear_retry();
        if let Some(cmd_tx) = &self.cmd_tx {
            let _ = cmd_tx.try_send(AgentCommand::RetryNow);
        }
    }

    /// A switch is drawn in the chat that made it, and kept for a restore
    /// only when it is the main agent's: a subagent starts on its parent's
    /// account, so its own moves are live detail.
    pub(super) fn account_routed(
        &mut self,
        chat_idx: usize,
        main_agent: bool,
        account: &RoutedAccount,
        switch: Option<AccountSwitch>,
    ) {
        if main_agent {
            self.routed_account = Some(account.clone());
            self.accounts_modal.stop_waiting();
        }
        let Some(switch) = switch else {
            return;
        };
        let summary = switch_row(
            &display_name(&account.provider),
            &switch,
            &account.name,
            now_ms(),
        );
        if main_agent {
            self.checkpoint_now();
            self.storage_writer.record_account_switch(
                self.state.session.id,
                StoredAccountSwitch {
                    seq: 0,
                    at_ms: now_ms(),
                    history_len: self.autopilot_history_len(),
                    provider: account.provider.clone(),
                    from_account: switch.from,
                    to_account: account.name.clone(),
                    summary: summary.clone(),
                },
            );
        }
        let chat = &mut self.chats[chat_idx];
        chat.flush();
        chat.push(DisplayMessage::new(DisplayRole::Notice, summary));
    }

    /// The row goes in the chat whose request waits; the card comes up only
    /// for the main agent's, and never over another overlay.
    pub(super) fn account_exhausted(
        &mut self,
        chat_idx: usize,
        main_agent: bool,
        exhaustion: AccountExhaustion,
    ) {
        let chat = &mut self.chats[chat_idx];
        chat.flush();
        chat.push(DisplayMessage::new(
            DisplayRole::Error,
            exhausted_row(&display_name(&exhaustion.provider), &exhaustion, now_ms()),
        ));
        if !main_agent || (self.any_overlay_open() && !self.accounts_modal.is_open()) {
            return;
        }
        match self.accounts_view(&exhaustion.provider) {
            Ok(view) => self.accounts_modal.show_exhaustion(view, exhaustion),
            Err(error) => warn!(%error, "spent account card unavailable"),
        }
    }

    pub(super) fn account_switch_rows(&self) -> Vec<(LogPlace, DisplayMessage)> {
        self.read_session_log(ACCOUNT_SWITCH_LOG, SessionDatabase::load_account_switches)
            .into_iter()
            .map(|switch| {
                (
                    (switch.history_len, switch.at_ms),
                    DisplayMessage::new(DisplayRole::Notice, switch.summary),
                )
            })
            .collect()
    }
}

fn now_ms() -> u64 {
    u64::try_from(SystemClock::new().now_ms()).unwrap_or_default()
}

/// Only while `provider`, the model's, has another account to move to, and
/// only once a request has said which one it went to.
pub(super) fn account_chip<'a>(
    routed: Option<&'a RoutedAccount>,
    provider: &str,
) -> Option<AccountChip<'a>> {
    routed
        .filter(|account| account.pool_size >= MIN_POOL_FOR_CHIP && account.provider == provider)
        .map(|account| AccountChip {
            name: &account.name,
            auto: account.mode == SelectionMode::Auto,
            left_percent: account.left_percent,
            tier: account.tier,
        })
}

/// `current`, else the pinned account, moved to the front.
fn current_first(pool: PoolStatus, current: Option<&str>) -> Vec<AccountStatus> {
    let pinned = match &pool.selection {
        AccountSelection::Pinned(name) => Some(name.as_str()),
        AccountSelection::Auto => None,
    };
    let lead = current.or(pinned).map(str::to_owned);
    let mut accounts = pool.accounts;
    if let Some(index) =
        lead.and_then(|lead| accounts.iter().position(|account| account.name == lead))
    {
        let account = accounts.remove(index);
        accounts.insert(0, account);
    }
    accounts
}

fn selected_message(display: &str, selection: &AccountSelection) -> String {
    match selection {
        AccountSelection::Pinned(name) => format!("{display} requests go to {name}"),
        AccountSelection::Auto => format!("{display} accounts switch automatically"),
    }
}

fn switch_row(provider: &str, switch: &AccountSwitch, to: &str, now_ms: u64) -> String {
    let from = &switch.from;
    let reason = match &switch.reason {
        SwitchReason::Spent {
            window,
            used_percent,
        } => format!("{from} {} {used_percent}% used", window.label()),
        SwitchReason::Low {
            window,
            used_percent,
        } => format!(
            "{from} {} {used_percent}% used while the cache was cold",
            window.label()
        ),
        SwitchReason::RateLimited {
            until_ms: Some(until_ms),
        } if *until_ms > now_ms => format!(
            "{from} rate limited for {}",
            format_elapsed((until_ms - now_ms) / MILLIS_PER_SECOND)
        ),
        SwitchReason::RateLimited { .. } => format!("{from} rate limited"),
        SwitchReason::AuthFailed => format!("{from} refused the sign-in"),
        SwitchReason::ModelUnsupported => format!("{from} does not offer the model"),
        SwitchReason::Rebalanced => format!("{to} had more room while the cache was cold"),
        SwitchReason::Selected => format!("you selected {to}"),
    };
    let mut row = format!("Switched {provider} account {from} → {to}: {reason}.");
    if switch.cached_tokens > 0 {
        row.push_str(&format!(
            " Prompt cache rebuilds (~{} tokens).",
            format_compact(u64::from(switch.cached_tokens))
        ));
    }
    row
}

fn exhausted_row(provider: &str, exhaustion: &AccountExhaustion, now_ms: u64) -> String {
    let mut row = match exhaustion.mode {
        SelectionMode::Pinned => format!("{provider} account {} is spent", exhaustion.account),
        SelectionMode::Auto => format!("Every {provider} account is spent"),
    };
    if let Some(until_ms) = exhaustion.until_ms.filter(|until_ms| *until_ms > now_ms) {
        row.push_str(&format!(
            ", refills in {}",
            format_elapsed((until_ms - now_ms) / MILLIS_PER_SECOND)
        ));
    }
    row.push('.');
    if !exhaustion.alternatives.is_empty() {
        let alternatives: Vec<String> = exhaustion.alternatives.iter().map(alternative).collect();
        row.push_str(&format!(" Others: {}.", alternatives.join(", ")));
    }
    row.push_str(&format!(" {SWITCH_HINT}."));
    row
}

fn alternative(alternative: &AccountAlternative) -> String {
    let name = &alternative.name;
    let standing = match (alternative.billing, alternative.left_percent) {
        (Billing::Api, _) => "pay per token".to_owned(),
        (Billing::Subscription, Some(left)) => format!("{left}% left"),
        (Billing::Subscription, None) => "quota unknown".to_owned(),
    };
    if alternative.fallback {
        format!("{name} {standing} (fallback)")
    } else {
        format!("{name} {standing}")
    }
}

/// The name the user knows the provider by, as `/login` lists it.
fn display_name(provider: &str) -> String {
    resolve_display_name(provider, ProvidersConfig::load().get(provider))
}

#[cfg(test)]
mod tests {
    use caudra_providers::accounts::WindowKind;
    use test_case::test_case;

    use super::*;

    const NOW_MS: u64 = 1_767_225_600_000;
    const HOUR_MS: u64 = 3_600_000;
    const PROVIDER: &str = "anthropic";
    const DISPLAY: &str = "Anthropic";
    const PERSONAL: &str = "personal";
    const WORK: &str = "work";
    const API: &str = "api";

    fn switch(reason: SwitchReason, cached_tokens: u32) -> AccountSwitch {
        AccountSwitch {
            from: PERSONAL.into(),
            reason,
            cached_tokens,
        }
    }

    fn exhaustion(
        mode: SelectionMode,
        until_ms: Option<u64>,
        alternatives: Vec<AccountAlternative>,
    ) -> AccountExhaustion {
        AccountExhaustion {
            provider: PROVIDER.into(),
            account: PERSONAL.into(),
            mode,
            until_ms,
            alternatives,
        }
    }

    fn subscription(name: &str, left_percent: Option<u8>) -> AccountAlternative {
        AccountAlternative {
            name: name.into(),
            billing: Billing::Subscription,
            left_percent,
            fallback: false,
        }
    }

    fn metered(name: &str, fallback: bool) -> AccountAlternative {
        AccountAlternative {
            name: name.into(),
            billing: Billing::Api,
            left_percent: None,
            fallback,
        }
    }

    #[test_case(
        switch(SwitchReason::Spent { window: WindowKind::Weekly, used_percent: 96 }, 84_000),
        "Switched Anthropic account personal → work: personal weekly 96% used. Prompt cache rebuilds (~84k tokens)."
        ; "spent_names_the_window_and_the_cache_cost"
    )]
    #[test_case(
        switch(SwitchReason::Low { window: WindowKind::FiveHour, used_percent: 85 }, 0),
        "Switched Anthropic account personal → work: personal 5h 85% used while the cache was cold."
        ; "low_at_a_cold_point_costs_no_cache"
    )]
    #[test_case(
        switch(SwitchReason::RateLimited { until_ms: Some(NOW_MS + 2 * HOUR_MS) }, 0),
        "Switched Anthropic account personal → work: personal rate limited for 2h00m."
        ; "rate_limited_says_for_how_long"
    )]
    #[test_case(
        switch(SwitchReason::RateLimited { until_ms: None }, 0),
        "Switched Anthropic account personal → work: personal rate limited."
        ; "rate_limited_without_a_reset"
    )]
    #[test_case(
        switch(SwitchReason::Selected, 12_000),
        "Switched Anthropic account personal → work: you selected work. Prompt cache rebuilds (~12k tokens)."
        ; "a_selection_names_the_new_account"
    )]
    fn a_switch_row_says_why_and_what_it_cost(switch: AccountSwitch, expected: &str) {
        assert_eq!(switch_row(DISPLAY, &switch, WORK, NOW_MS), expected);
    }

    #[test_case(
        exhaustion(SelectionMode::Pinned, Some(NOW_MS + HOUR_MS), vec![subscription(WORK, Some(62)), metered(API, true)]),
        "Anthropic account personal is spent, refills in 1h00m. Others: work 62% left, api pay per token (fallback). Ctrl+X i chooses another account."
        ; "pinned_lists_where_else_to_go"
    )]
    #[test_case(
        exhaustion(SelectionMode::Auto, None, vec![metered(API, false)]),
        "Every Anthropic account is spent. Others: api pay per token. Ctrl+X i chooses another account."
        ; "auto_offers_the_key_it_will_not_use"
    )]
    #[test_case(
        exhaustion(SelectionMode::Pinned, Some(NOW_MS - HOUR_MS), vec![subscription(WORK, None)]),
        "Anthropic account personal is spent. Others: work quota unknown. Ctrl+X i chooses another account."
        ; "a_past_reset_is_not_shown"
    )]
    fn an_exhausted_row_offers_the_alternatives(exhaustion: AccountExhaustion, expected: &str) {
        assert_eq!(exhausted_row(DISPLAY, &exhaustion, NOW_MS), expected);
    }
}
