//! `/accounts`: where each of a provider's accounts stands, which one
//! requests go to, and the card a spent account raises so the user can move
//! elsewhere instead of waiting for it to refill.

use std::mem;

use caudra_config::ClockFormat;
use caudra_config::providers::SwitchStrategy;
use caudra_grab::grab_scope;
use caudra_providers::Billing;
use caudra_providers::accounts::{
    AccountExhaustion, AccountKind, AccountStatus, AccountTier, PoolStatus, QuotaWindow,
    SelectionMode,
};
use caudra_storage::accounts::{AccountIdentity, AccountName, AccountSelection};
use caudra_workbench::keys::list_end;
use caudra_workbench::text_field::{FieldKind, TextField, TextKey};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use jiff::tz::TimeZone;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::gauge::{CELL_EIGHTHS, GAUGE_CLOSE, GAUGE_OPEN, fill, track};
use super::keybindings::{Bind, key};
use super::list_picker::{ListPicker, PickerAction, PickerItem};
use super::tooltip::Tip;
use super::usage_modal::format_reset;
use super::{Hint, Overlay, format_elapsed};
use crate::repaint::Cadence;
use crate::theme;

const MAX_VISIBLE: u16 = 10;
const WIDTH_PERCENT: u16 = 80;
const IN_USE_MARK: &str = "●";
const IDLE_MARK: &str = "○";
const MARK_CHARS: usize = 1;
const SEPARATOR: &str = " · ";
const INDENT: &str = "  ";
const GAUGE_CELLS: usize = 10;
const FULL_PERCENT: f32 = 100.0;
const MILLIS_PER_SECOND: u64 = 1_000;
const CARD_SWITCH_LIMIT: usize = 3;
const RENAME_HINT: &str = "Lowercase letters, digits, - and _, then Enter";
const SIGNED_OUT_HINT: &str = "Signed out. Enter signs in again.";
const NO_READING_HINT: &str = "No quota reading yet. Ctrl+R asks for one.";
const METERED_HINT: &str = "Pay per token.";
const FALLBACK_HINT: &str = "Auto moves a session onto it once every subscription is spent.";
const NOT_FALLBACK_HINT: &str =
    "Auto never moves a subscription session onto it unless providers.toml marks it fallback.";
const KEYLESS_HINT: &str = "The provider needs no key.";
const OVERRIDDEN_NOTE: &str = "--account or CAUDRA_ACCOUNT set it for this run.";
const NOT_SAVED: &str = "is not a saved account";
const WAIT: &str = "Wait for it to refill";
const BROWSE: &str = "Choose another account…";
const TURN_ON_AUTO: &str = "Switch and turn on Auto";

const AUTO_KEY: Bind = letter('a', "a");
const ADD_KEY: Bind = letter('n', "n");
const RENAME_KEY: Bind = letter('r', "r");
const REMOVE_KEY: Bind = letter('d', "d");
const STRATEGY_KEY: Bind = letter('s', "s");
const MOVE_UP_KEY: Bind = Bind {
    code: KeyCode::Up,
    modifiers: KeyModifiers::CONTROL,
    label: "Ctrl+↑",
};
const MOVE_DOWN_KEY: Bind = Bind {
    code: KeyCode::Down,
    modifiers: KeyModifiers::CONTROL,
    label: "Ctrl+↓",
};

const fn letter(char: char, label: &'static str) -> Bind {
    Bind {
        code: KeyCode::Char(char),
        modifiers: KeyModifiers::NONE,
        label,
    }
}

/// What the user asked of the provider's accounts. The modal changes
/// nothing itself: the app applies it and hands back a fresh [`AccountsView`].
#[derive(Debug, PartialEq, Eq)]
pub enum AccountsAction {
    Consumed,
    Closed,
    Copy(String),
    /// `retry` cuts the wait of a request a spent account holds up.
    Select {
        selection: AccountSelection,
        retry: bool,
    },
    /// For the rest of this process only, as `--account` would.
    UseForRun(AccountName),
    /// `None` adds an account; a name signs in to that one again.
    SignIn(Option<AccountName>),
    Rename {
        from: AccountName,
        to: AccountName,
    },
    Remove(AccountName),
    Move {
        name: AccountName,
        offset: isize,
    },
    Strategy(SwitchStrategy),
    Refresh,
    /// The card's way to the full list.
    Browse,
}

/// One provider's pool as `/accounts` lists it.
#[derive(Debug, Clone)]
pub struct AccountsView {
    pub provider: String,
    pub display: String,
    pub pool: PoolStatus,
    /// The account this session's last request went to.
    pub current: Option<String>,
    pub now_ms: u64,
    pub clock: ClockFormat,
}

struct AccountRow {
    name: String,
    label: String,
    detail: String,
    info: String,
    tier: AccountTier,
    signed_out: bool,
}

impl PickerItem for AccountRow {
    fn label(&self) -> &str {
        &self.label
    }

    fn detail(&self) -> Option<&str> {
        Some(&self.detail)
    }

    fn lead(&self) -> Option<(usize, Style)> {
        Some((MARK_CHARS, tier_style(self.tier)))
    }

    fn is_disabled(&self) -> bool {
        self.signed_out
    }

    fn info(&self) -> Option<&str> {
        Some(&self.info)
    }
}

enum CardChoice {
    Pin(AccountName),
    Auto,
    ForRun(AccountName),
    Wait,
    Browse,
}

struct CardRow {
    label: String,
    choice: CardChoice,
}

impl PickerItem for CardRow {
    fn label(&self) -> &str {
        &self.label
    }
}

enum Stage {
    List { pending_remove: Option<String> },
    Rename(String, Box<TextField>),
    Card(Box<AccountExhaustion>),
}

struct Flow {
    view: AccountsView,
    stage: Stage,
}

pub struct AccountsModal {
    picker: ListPicker<AccountRow>,
    card: ListPicker<CardRow>,
    flow: Option<Flow>,
    /// The spell of exhaustion whose card the user put away: every retry of
    /// the request reports it again, and none of them should raise it.
    waiting: Option<Box<AccountExhaustion>>,
}

impl AccountsModal {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new()
                .with_max_visible(MAX_VISIBLE)
                .with_width_percent(WIDTH_PERCENT),
            card: ListPicker::new().with_width_percent(WIDTH_PERCENT),
            flow: None,
            waiting: None,
        }
    }

    pub fn open(&mut self, view: AccountsView) {
        self.card.close();
        self.flow = Some(Flow {
            view,
            stage: Stage::List {
                pending_remove: None,
            },
        });
        self.show_list(None);
    }

    /// Fresh readings for the list on screen, keeping the selection, or for
    /// the next open while the card or a rename has the modal.
    pub fn refresh(&mut self, view: AccountsView, error: Option<String>) {
        let Some(flow) = &mut self.flow else {
            return;
        };
        if flow.view.provider != view.provider {
            return;
        }
        let keep = self.picker.selected_item().map(|row| row.name.clone());
        flow.view = view;
        if matches!(flow.stage, Stage::List { .. }) {
            flow.stage = Stage::List {
                pending_remove: None,
            };
            self.show_list(keep.as_deref());
            self.picker.set_error_text(error);
        }
    }

    /// Selects `name` once the list is redrawn, as after a move.
    pub fn select(&mut self, name: &str) {
        self.picker.select_item_by(|row| row.name == name);
        self.sync_info();
    }

    pub fn show_error(&mut self, error: String) {
        self.picker.set_error_text(Some(error));
    }

    /// The card a spent account raises. Each retry of the request reports the
    /// same spell again, which changes nothing while the card is up or after
    /// the user chose to wait it out.
    pub fn show_exhaustion(&mut self, view: AccountsView, exhaustion: AccountExhaustion) {
        let shown = match &self.flow {
            Some(Flow {
                stage: Stage::Card(shown),
                ..
            }) => Some(shown),
            _ => self.waiting.as_ref(),
        };
        if shown.is_some_and(|shown| same_spell(shown, &exhaustion)) {
            return;
        }
        let rows = card_rows(&exhaustion, view.now_ms, view.clock);
        let title = card_title(&view.display, &exhaustion);
        let info = card_info(&exhaustion, view.now_ms, view.clock);
        self.picker.close();
        self.flow = Some(Flow {
            view,
            stage: Stage::Card(Box::new(exhaustion)),
        });
        self.card.set_info_text(Some(info));
        self.card.set_footer_builder(card_footer);
        self.card.open(rows, title);
    }

    pub fn provider(&self) -> Option<&str> {
        self.flow.as_ref().map(|flow| flow.view.provider.as_str())
    }

    pub fn is_open(&self) -> bool {
        self.flow.is_some()
    }

    pub(crate) fn text_input_active(&self) -> bool {
        matches!(
            self.flow.as_ref().map(|flow| &flow.stage),
            Some(Stage::Rename(..))
        )
    }

    /// Putting the card away, however it goes, waits out its spell.
    pub fn close(&mut self) {
        if let Some(Flow {
            stage: Stage::Card(shown),
            ..
        }) = self.flow.take()
        {
            self.waiting = Some(shown);
        }
        self.picker.close();
        self.card.close();
    }

    /// A request went through, so the next spell raises its card again.
    pub fn stop_waiting(&mut self) {
        self.waiting = None;
    }

    pub fn contains(&self, pos: Position) -> bool {
        match self.on_card() {
            true => self.card.contains(pos),
            false => self.picker.contains(pos),
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        match self.on_card() {
            true => self.card.scroll(delta),
            false => self.picker.scroll(delta),
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        grab_scope!("accounts_modal", area);
        match self.on_card() {
            true => self.card.view(frame, area),
            false => self.picker.view(frame, area),
        }
    }

    pub fn handle_key(&mut self, event: KeyEvent) -> AccountsAction {
        let Some(flow) = &mut self.flow else {
            return AccountsAction::Consumed;
        };
        match &mut flow.stage {
            Stage::Card(_) => {
                let action = self.card.handle_key(event);
                self.map_card_action(action)
            }
            Stage::Rename(..) if event.code == KeyCode::Enter => self.finish_rename(),
            Stage::Rename(..) if event.code == KeyCode::Esc => {
                self.back_to_list();
                AccountsAction::Consumed
            }
            Stage::Rename(_, field) => {
                let edit = field.handle_key(event);
                self.picker.mirror_search(field);
                match edit {
                    TextKey::Copy(text) | TextKey::Cut(text) => AccountsAction::Copy(text),
                    TextKey::Ignored if key::QUIT.matches(event) => {
                        self.close();
                        AccountsAction::Closed
                    }
                    _ => AccountsAction::Consumed,
                }
            }
            Stage::List { pending_remove } => {
                let confirming = pending_remove.take();
                if confirming.is_some() {
                    self.picker.set_error_text(None);
                }
                self.list_key(event, confirming)
            }
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.handle_text(text, true)
    }

    pub fn handle_text(&mut self, text: &str, paste: bool) -> bool {
        match self.flow.as_mut().map(|flow| &mut flow.stage) {
            Some(Stage::Rename(_, field)) => {
                field.handle_text(text, paste);
                self.picker.mirror_search(field);
                true
            }
            Some(_) => true,
            None => false,
        }
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> AccountsAction {
        match self.flow.as_ref().map(|flow| &flow.stage) {
            None => AccountsAction::Consumed,
            Some(Stage::Card(_)) => {
                let action = self.card.handle_mouse(event);
                self.map_card_action(action)
            }
            Some(Stage::Rename(..)) => match self.picker.handle_footer_mouse(event) {
                Some(key) => self.handle_key(key),
                None => AccountsAction::Consumed,
            },
            Some(Stage::List { .. }) => {
                let action = self.picker.handle_mouse(event);
                let action = self.map_list_action(action);
                self.sync_info();
                action
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn selected_name(&self) -> Option<&str> {
        self.picker.selected_item().map(|row| row.name.as_str())
    }

    #[cfg(test)]
    pub(crate) fn card_labels(&self) -> Vec<String> {
        (0..)
            .map_while(|index| self.card.item(index))
            .map(|row| row.label.clone())
            .collect()
    }

    fn on_card(&self) -> bool {
        matches!(
            self.flow.as_ref().map(|flow| &flow.stage),
            Some(Stage::Card(_))
        )
    }

    fn list_key(&mut self, event: KeyEvent, confirming: Option<String>) -> AccountsAction {
        let selected = self.selected_account().cloned();
        if key::REFRESH.matches(event) {
            return AccountsAction::Refresh;
        }
        if AUTO_KEY.matches(event) {
            return AccountsAction::Select {
                selection: AccountSelection::Auto,
                retry: false,
            };
        }
        if ADD_KEY.matches(event) {
            return AccountsAction::SignIn(None);
        }
        if STRATEGY_KEY.matches(event) {
            let strategy = match self.flow.as_ref().map(|flow| flow.view.pool.strategy) {
                Some(SwitchStrategy::Ordered) => SwitchStrategy::Paced,
                _ => SwitchStrategy::Ordered,
            };
            return AccountsAction::Strategy(strategy);
        }
        let Some(account) = selected else {
            return self.forward_to_list(event);
        };
        if RENAME_KEY.matches(event) {
            return match orderable(&account) {
                true => self.start_rename(account.name),
                false => self.refuse(&account),
            };
        }
        if REMOVE_KEY.matches(event) {
            if !orderable(&account) {
                return self.refuse(&account);
            }
            if confirming.as_deref() == Some(account.name.as_str()) {
                return parse_name(&account.name).map_or(AccountsAction::Consumed, |name| {
                    AccountsAction::Remove(name)
                });
            }
            self.picker.set_error_text(Some(format!(
                "Press d again to sign out of {} and forget it.",
                account.name
            )));
            if let Some(Flow {
                stage: Stage::List { pending_remove },
                ..
            }) = &mut self.flow
            {
                *pending_remove = Some(account.name);
            }
            return AccountsAction::Consumed;
        }
        for (bind, offset) in [(MOVE_UP_KEY, -1), (MOVE_DOWN_KEY, 1)] {
            if bind.matches(event) {
                if !orderable(&account) {
                    return self.refuse(&account);
                }
                return parse_name(&account.name).map_or(AccountsAction::Consumed, |name| {
                    AccountsAction::Move { name, offset }
                });
            }
        }
        self.forward_to_list(event)
    }

    /// The list has nothing to search, so it hears only the keys that move
    /// or answer it.
    fn forward_to_list(&mut self, event: KeyEvent) -> AccountsAction {
        let for_list = matches!(
            event.code,
            KeyCode::Enter
                | KeyCode::Esc
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::PageUp
                | KeyCode::PageDown
        ) && event.modifiers == KeyModifiers::NONE
            || list_end(event, true).is_some()
            || key::QUIT.matches(event);
        if !for_list {
            return AccountsAction::Consumed;
        }
        let action = self.picker.handle_key(event);
        let action = self.map_list_action(action);
        self.sync_info();
        action
    }

    fn map_list_action(&mut self, action: PickerAction<AccountRow>) -> AccountsAction {
        match action {
            PickerAction::Close => {
                self.close();
                AccountsAction::Closed
            }
            PickerAction::Select(row) => {
                let Some(name) = parse_name(&row.name) else {
                    return AccountsAction::Consumed;
                };
                match row.signed_out {
                    true => AccountsAction::SignIn(Some(name)),
                    false => AccountsAction::Select {
                        selection: AccountSelection::Pinned(name),
                        retry: false,
                    },
                }
            }
            PickerAction::Key(key) => self.handle_key(key),
            PickerAction::Copy(text) => AccountsAction::Copy(text),
            PickerAction::Consumed | PickerAction::Toggle(..) => AccountsAction::Consumed,
        }
    }

    fn map_card_action(&mut self, action: PickerAction<CardRow>) -> AccountsAction {
        match action {
            PickerAction::Close => {
                self.close();
                AccountsAction::Closed
            }
            PickerAction::Select(row) => {
                if matches!(row.choice, CardChoice::Browse) {
                    self.card.close();
                    self.back_to_list();
                    return AccountsAction::Browse;
                }
                self.close();
                match row.choice {
                    CardChoice::Pin(name) => AccountsAction::Select {
                        selection: AccountSelection::Pinned(name),
                        retry: true,
                    },
                    CardChoice::Auto => AccountsAction::Select {
                        selection: AccountSelection::Auto,
                        retry: true,
                    },
                    CardChoice::ForRun(name) => AccountsAction::UseForRun(name),
                    CardChoice::Wait | CardChoice::Browse => AccountsAction::Closed,
                }
            }
            PickerAction::Key(key) => self.handle_key(key),
            PickerAction::Copy(text) => AccountsAction::Copy(text),
            PickerAction::Consumed | PickerAction::Toggle(..) => AccountsAction::Consumed,
        }
    }

    fn selected_account(&self) -> Option<&AccountStatus> {
        let name = &self.picker.selected_item()?.name;
        self.flow
            .as_ref()?
            .view
            .pool
            .accounts
            .iter()
            .find(|account| &account.name == name)
    }

    fn refuse(&mut self, account: &AccountStatus) -> AccountsAction {
        let source = match (&account.kind, &account.env_var) {
            (AccountKind::EnvKey, Some(var)) => format!(", it comes from {var}"),
            (AccountKind::ConfigKey, _) => {
                ", it is the provider's api_key in providers.toml".into()
            }
            _ => String::new(),
        };
        self.picker
            .set_error_text(Some(format!("{} {NOT_SAVED}{source}.", account.name)));
        AccountsAction::Consumed
    }

    fn start_rename(&mut self, name: String) -> AccountsAction {
        let Some(flow) = &mut self.flow else {
            return AccountsAction::Consumed;
        };
        let field = Box::new(TextField::with_text(FieldKind::Line, &name));
        let title = format!(" Rename {name} ");
        flow.stage = Stage::Rename(name, field);
        self.picker.set_error_text(None);
        self.picker.set_info_text(Some(RENAME_HINT.into()));
        self.picker.set_footer_builder(rename_footer);
        self.picker.open(Vec::new(), title);
        if let Some(Flow {
            stage: Stage::Rename(_, field),
            ..
        }) = &self.flow
        {
            self.picker.mirror_search(field);
        }
        AccountsAction::Consumed
    }

    fn finish_rename(&mut self) -> AccountsAction {
        let Some(Flow {
            stage: Stage::Rename(from, field),
            ..
        }) = &self.flow
        else {
            return AccountsAction::Consumed;
        };
        let typed = field.text().trim().to_owned();
        let from = from.clone();
        if typed == from {
            self.back_to_list();
            return AccountsAction::Consumed;
        }
        match (AccountName::new(&from), AccountName::new(&typed)) {
            (Ok(from), Ok(to)) => {
                self.back_to_list();
                AccountsAction::Rename { from, to }
            }
            (_, Err(error)) => {
                self.picker.set_error_text(Some(error.to_string()));
                AccountsAction::Consumed
            }
            (Err(_), _) => AccountsAction::Consumed,
        }
    }

    fn back_to_list(&mut self) {
        let Some(flow) = &mut self.flow else {
            return;
        };
        let keep = match mem::replace(
            &mut flow.stage,
            Stage::List {
                pending_remove: None,
            },
        ) {
            Stage::Rename(name, _) => Some(name),
            Stage::Card(shown) => {
                self.waiting = Some(shown);
                None
            }
            Stage::List { .. } => None,
        };
        self.show_list(keep.as_deref());
    }

    fn show_list(&mut self, keep: Option<&str>) {
        let Some(flow) = &self.flow else {
            return;
        };
        let view = &flow.view;
        let rows: Vec<AccountRow> = view
            .pool
            .accounts
            .iter()
            .map(|account| account_row(view, account))
            .collect();
        let in_use = in_use(view).map(str::to_owned);
        self.picker.set_error_text(None);
        self.picker.set_footer_builder(list_footer);
        self.picker
            .open(rows, format!(" Accounts{SEPARATOR}{} ", view.display));
        let target = keep.map(str::to_owned).or(in_use);
        if let Some(target) = target {
            self.picker.select_item_by(|row| row.name == target);
        }
        self.sync_info();
    }

    /// The info area follows the selection; each row's own `info` only sizes it.
    fn sync_info(&mut self) {
        let (Some(flow), Some(row)) = (&self.flow, self.picker.selected_item()) else {
            return;
        };
        if !matches!(flow.stage, Stage::List { .. }) {
            return;
        }
        let view = &flow.view;
        let lines = view
            .pool
            .accounts
            .iter()
            .find(|account| account.name == row.name)
            .map(|account| info_lines(view, account))
            .unwrap_or_default();
        self.picker.set_info_lines(Some(lines));
    }
}

impl Overlay for AccountsModal {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }

    fn cadence(&self) -> Cadence {
        match self.on_card() {
            true => self.card.cadence(),
            false => self.picker.cadence(),
        }
    }

    fn tooltip(&self) -> Option<Tip> {
        match self.on_card() {
            true => self.card.tooltip(),
            false => self.picker.tooltip(),
        }
    }
}

fn parse_name(name: &str) -> Option<AccountName> {
    AccountName::new(name).ok()
}

fn same_spell(left: &AccountExhaustion, right: &AccountExhaustion) -> bool {
    left.provider == right.provider
        && left.account == right.account
        && left.until_ms == right.until_ms
}

/// What `providers.toml` and the credential store hold, so what a rename, a
/// removal or a move can act on.
fn orderable(account: &AccountStatus) -> bool {
    account.configured
        || matches!(
            account.kind,
            AccountKind::Subscription | AccountKind::StoredKey
        )
}

fn in_use(view: &AccountsView) -> Option<&str> {
    match &view.pool.selection {
        AccountSelection::Pinned(name) => Some(name.as_str()),
        AccountSelection::Auto => view.current.as_deref(),
    }
}

fn tier_style(tier: AccountTier) -> Style {
    let theme = theme::current();
    match tier {
        AccountTier::Healthy => theme.tool_success,
        AccountTier::Low => theme.tool_warning,
        AccountTier::Spent => theme.error,
    }
}

fn policy_line(pool: &PoolStatus) -> String {
    let line = match &pool.selection {
        AccountSelection::Pinned(name) => {
            format!("Pinned to {name}: every request goes to it. a turns Auto on.")
        }
        AccountSelection::Auto if pool.accounts.len() < 2 => {
            "One account: every request goes to it. n adds another.".to_owned()
        }
        AccountSelection::Auto => {
            let order = match pool.strategy {
                SwitchStrategy::Ordered => "tries them top to bottom",
                SwitchStrategy::Paced => "uses the one furthest ahead of its quota",
            };
            format!(
                "Auto, {}: {order}, moves below {}% left, or below {}% once the prompt cache is cold.",
                pool.strategy.as_str(),
                pool.switch_below,
                pool.prefer_below
            )
        }
    };
    match pool.overridden {
        true => format!("{line} {OVERRIDDEN_NOTE}"),
        false => line,
    }
}

fn account_row(view: &AccountsView, account: &AccountStatus) -> AccountRow {
    let mark = match in_use(view) == Some(account.name.as_str()) {
        true => IN_USE_MARK,
        false => IDLE_MARK,
    };
    let info = info_lines(view, account)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    AccountRow {
        name: account.name.clone(),
        label: format!("{mark} {}", account.name),
        detail: account_detail(account, view.now_ms),
        info,
        tier: account.tier,
        signed_out: account.kind == AccountKind::SignedOut,
    }
}

pub(crate) fn account_detail(account: &AccountStatus, now_ms: u64) -> String {
    let masked = account.masked_key.as_deref().unwrap_or_default();
    let AccountIdentity {
        email,
        organization,
        plan,
        ..
    } = &account.identity;
    let mut details = match account.kind {
        AccountKind::Subscription => vec![plan.clone().unwrap_or_else(|| "subscription".into())],
        AccountKind::StoredKey => vec![format!("key {masked}")],
        AccountKind::ConfigKey => vec![format!("key {masked} from providers.toml")],
        AccountKind::EnvKey => vec![format!(
            "via {}",
            account.env_var.as_deref().unwrap_or_default()
        )],
        AccountKind::Keyless => vec!["no key needed".into()],
        AccountKind::SignedOut => vec!["signed out".into()],
    };
    details.extend(email.as_ref().or(organization.as_ref()).cloned());
    if account.fallback {
        details.push("fallback".into());
    }
    details.extend(account.windows.iter().map(|window| {
        format!(
            "{} {}% left",
            window.kind.short_label(),
            left_percent(window, now_ms)
        )
    }));
    match account.blocked_until {
        Some(until_ms) => details.push(format!(
            "blocked for {}",
            format_elapsed(until_ms.saturating_sub(now_ms) / MILLIS_PER_SECOND)
        )),
        None if account.tier == AccountTier::Spent && account.kind != AccountKind::SignedOut => {
            details.push("spent".into());
        }
        None => {}
    }
    details.join(SEPARATOR)
}

fn left_percent(window: &QuotaWindow, now_ms: u64) -> u8 {
    window.left_percent(now_ms).round().clamp(0.0, FULL_PERCENT) as u8
}

fn info_lines(view: &AccountsView, account: &AccountStatus) -> Vec<Line<'static>> {
    let theme = theme::current();
    let dim = theme.status_dim;
    let mut lines = vec![Line::from(policy_line(&view.pool))];
    let note = |text: &str| Line::from(Span::styled(format!("{INDENT}{text}"), dim));
    if account.kind == AccountKind::SignedOut {
        lines.push(note(SIGNED_OUT_HINT));
        return lines;
    }
    if let Some(until_ms) = account.blocked_until {
        lines.push(Line::from(Span::styled(
            format!(
                "{INDENT}Rate limited until {}.",
                format_reset(until_ms, &TimeZone::system(), view.clock)
            ),
            theme.tool_warning,
        )));
    }
    match (account.billing, account.kind) {
        (_, AccountKind::Keyless) => lines.push(note(KEYLESS_HINT)),
        (Billing::Api, _) => {
            lines.push(note(METERED_HINT));
            lines.push(note(match account.fallback {
                true => FALLBACK_HINT,
                false => NOT_FALLBACK_HINT,
            }));
        }
        (Billing::Subscription, _) if account.windows.is_empty() => {
            lines.push(note(NO_READING_HINT));
        }
        (Billing::Subscription, _) => {
            let width = account
                .windows
                .iter()
                .map(|window| window.kind.label().chars().count())
                .max()
                .unwrap_or_default();
            lines.extend(
                account
                    .windows
                    .iter()
                    .map(|window| quota_line(window, width, view)),
            );
        }
    }
    lines
}

fn quota_line(window: &QuotaWindow, width: usize, view: &AccountsView) -> Line<'static> {
    let theme = theme::current();
    let left = left_percent(window, view.now_ms);
    let used = usize::from(100 - left);
    let (filled, cells) = fill(used * GAUGE_CELLS * CELL_EIGHTHS / 100);
    let tier = match left {
        left if left < view.pool.switch_below => AccountTier::Spent,
        left if left < view.pool.prefer_below => AccountTier::Low,
        _ => AccountTier::Healthy,
    };
    let mut spans = vec![
        Span::raw(format!("{INDENT}{:<width$} ", window.kind.label())),
        Span::styled(GAUGE_OPEN, theme.status_dim),
        Span::styled(filled, tier_style(tier)),
        Span::styled(track(GAUGE_CELLS.saturating_sub(cells)), theme.status_dim),
        Span::styled(GAUGE_CLOSE, theme.status_dim),
        Span::raw(format!(" {left}% left")),
    ];
    if let Some(reset) = window.resets_at.filter(|reset| *reset > view.now_ms) {
        spans.push(Span::styled(
            format!(
                "{SEPARATOR}resets {}",
                format_reset(reset, &TimeZone::system(), view.clock)
            ),
            theme.status_dim,
        ));
    }
    Line::from(spans)
}

fn card_title(display: &str, exhaustion: &AccountExhaustion) -> String {
    match exhaustion.mode {
        SelectionMode::Pinned => format!(" {display} account {} is spent ", exhaustion.account),
        SelectionMode::Auto => format!(" Every {display} account is spent "),
    }
}

fn card_info(exhaustion: &AccountExhaustion, now_ms: u64, clock: ClockFormat) -> String {
    match exhaustion.until_ms.filter(|until| *until > now_ms) {
        Some(until) => format!(
            "The request waits until {} refills, {}.",
            exhaustion.account,
            format_reset(until, &TimeZone::system(), clock)
        ),
        None => format!("The request waits until {} refills.", exhaustion.account),
    }
}

fn card_rows(exhaustion: &AccountExhaustion, now_ms: u64, clock: ClockFormat) -> Vec<CardRow> {
    let mut rows = Vec::new();
    let subscriptions = exhaustion
        .alternatives
        .iter()
        .filter(|alternative| alternative.billing == Billing::Subscription)
        .filter_map(|alternative| Some((parse_name(&alternative.name)?, alternative)));
    for (name, alternative) in subscriptions.take(CARD_SWITCH_LIMIT) {
        let label = match alternative.left_percent {
            Some(left) => format!("Switch to {name} ({left}% left)"),
            None => format!("Switch to {name}"),
        };
        rows.push(CardRow {
            label,
            choice: CardChoice::Pin(name),
        });
    }
    if exhaustion.mode == SelectionMode::Pinned && !rows.is_empty() {
        rows.push(CardRow {
            label: TURN_ON_AUTO.into(),
            choice: CardChoice::Auto,
        });
    }
    let metered = exhaustion
        .alternatives
        .iter()
        .filter(|alternative| alternative.billing == Billing::Api)
        .filter_map(|alternative| parse_name(&alternative.name));
    for name in metered {
        rows.push(CardRow {
            label: format!("Use {name} (pay per token) for this run"),
            choice: CardChoice::ForRun(name),
        });
    }
    let wait = match exhaustion.until_ms.filter(|until| *until > now_ms) {
        Some(until) => format!(
            "Wait until {}",
            format_reset(until, &TimeZone::system(), clock)
        ),
        None => WAIT.into(),
    };
    rows.push(CardRow {
        label: wait,
        choice: CardChoice::Wait,
    });
    rows.push(CardRow {
        label: BROWSE.into(),
        choice: CardChoice::Browse,
    });
    rows
}

fn list_footer() -> Vec<Hint> {
    vec![
        Hint::bind(key::ENTER, "use"),
        Hint::bind(AUTO_KEY, "auto"),
        Hint::bind(ADD_KEY, "add"),
        Hint::bind(RENAME_KEY, "rename"),
        Hint::bind(REMOVE_KEY, "remove"),
        Hint::inert("Ctrl+↑↓", "order"),
        Hint::bind(STRATEGY_KEY, "strategy"),
        Hint::bind(key::REFRESH, "refresh"),
        Hint::bind(key::ESC, "close"),
    ]
}

fn rename_footer() -> Vec<Hint> {
    vec![
        Hint::bind(key::ENTER, "rename"),
        Hint::bind(key::ESC, "cancel"),
    ]
}

fn card_footer() -> Vec<Hint> {
    vec![
        Hint::inert("↑↓", "select"),
        Hint::bind(key::ENTER, "choose"),
        Hint::bind(key::ESC, "wait"),
    ]
}

#[cfg(test)]
pub(crate) mod tests {
    use caudra_providers::accounts::{AccountAlternative, WindowKind};
    use crossterm::event::KeyEvent;
    use test_case::test_case;

    use super::*;

    const PROVIDER: &str = "anthropic";
    const DISPLAY: &str = "Anthropic";
    const PERSONAL: &str = "personal";
    const WORK: &str = "work";
    const API: &str = "api";
    const ENV: &str = "env";
    const ENV_VAR: &str = "ANTHROPIC_API_KEY";
    const NOW_MS: u64 = 1_767_225_600_000;
    const HOUR_MS: u64 = 3_600_000;

    fn account(name: &str, kind: AccountKind, billing: Billing) -> AccountStatus {
        AccountStatus {
            name: name.into(),
            kind,
            billing,
            fallback: false,
            configured: false,
            identity: AccountIdentity::default(),
            masked_key: None,
            env_var: (kind == AccountKind::EnvKey).then(|| ENV_VAR.into()),
            windows: Vec::new(),
            blocked_until: None,
            tier: AccountTier::Healthy,
            left_percent: None,
        }
    }

    fn subscription(name: &str) -> AccountStatus {
        account(name, AccountKind::Subscription, Billing::Subscription)
    }

    fn view(selection: AccountSelection, accounts: Vec<AccountStatus>) -> AccountsView {
        AccountsView {
            provider: PROVIDER.into(),
            display: DISPLAY.into(),
            pool: PoolStatus {
                provider: PROVIDER.into(),
                selection,
                overridden: false,
                strategy: SwitchStrategy::Ordered,
                switch_below: 5,
                prefer_below: 20,
                accounts,
            },
            current: None,
            now_ms: NOW_MS,
            clock: ClockFormat::default(),
        }
    }

    pub(crate) fn pool() -> AccountsView {
        view(
            AccountSelection::Auto,
            vec![
                subscription(PERSONAL),
                subscription(WORK),
                account(ENV, AccountKind::EnvKey, Billing::Api),
            ],
        )
    }

    fn name(name: &str) -> AccountName {
        AccountName::new(name).unwrap()
    }

    fn press(modal: &mut AccountsModal, bind: Bind) -> AccountsAction {
        modal.handle_key(bind.to_key_event())
    }

    fn down(modal: &mut AccountsModal) {
        modal.handle_key(KeyEvent::from(KeyCode::Down));
    }

    fn opened(view: AccountsView) -> AccountsModal {
        let mut modal = AccountsModal::new();
        modal.open(view);
        modal
    }

    #[test]
    fn enter_pins_the_selected_account() {
        let mut modal = opened(pool());
        down(&mut modal);

        assert_eq!(
            press(&mut modal, key::ENTER),
            AccountsAction::Select {
                selection: AccountSelection::Pinned(name(WORK)),
                retry: false,
            }
        );
    }

    #[test]
    fn enter_on_a_signed_out_account_signs_in_to_it() {
        let mut modal = opened(view(
            AccountSelection::Auto,
            vec![account(PERSONAL, AccountKind::SignedOut, Billing::Api)],
        ));

        assert_eq!(
            press(&mut modal, key::ENTER),
            AccountsAction::SignIn(Some(name(PERSONAL)))
        );
    }

    #[test]
    fn the_list_opens_on_the_pinned_account() {
        let modal = opened(view(
            AccountSelection::Pinned(name(WORK)),
            vec![subscription(PERSONAL), subscription(WORK)],
        ));

        assert_eq!(modal.selected_name(), Some(WORK));
    }

    #[test_case(AUTO_KEY, AccountsAction::Select { selection: AccountSelection::Auto, retry: false } ; "a_turns_auto_on")]
    #[test_case(ADD_KEY, AccountsAction::SignIn(None) ; "n_adds_an_account")]
    #[test_case(STRATEGY_KEY, AccountsAction::Strategy(SwitchStrategy::Paced) ; "s_flips_the_strategy")]
    #[test_case(key::REFRESH, AccountsAction::Refresh ; "ctrl_r_refreshes")]
    #[test_case(MOVE_DOWN_KEY, AccountsAction::Move { name: name(PERSONAL), offset: 1 } ; "ctrl_down_moves_later")]
    fn a_key_asks_for_its_change(bind: Bind, expected: AccountsAction) {
        let mut modal = opened(pool());

        assert_eq!(press(&mut modal, bind), expected);
    }

    #[test]
    fn removal_takes_a_second_press() {
        let mut modal = opened(pool());

        assert_eq!(press(&mut modal, REMOVE_KEY), AccountsAction::Consumed);
        assert_eq!(
            press(&mut modal, REMOVE_KEY),
            AccountsAction::Remove(name(PERSONAL))
        );
    }

    #[test]
    fn another_key_between_the_presses_cancels_the_removal() {
        let mut modal = opened(pool());

        press(&mut modal, REMOVE_KEY);
        down(&mut modal);
        modal.handle_key(KeyEvent::from(KeyCode::Up));

        assert_eq!(press(&mut modal, REMOVE_KEY), AccountsAction::Consumed);
    }

    #[test_case(REMOVE_KEY ; "remove")]
    #[test_case(RENAME_KEY ; "rename")]
    #[test_case(MOVE_UP_KEY ; "move")]
    fn an_account_from_the_environment_is_left_alone(bind: Bind) {
        let mut modal = opened(pool());
        down(&mut modal);
        down(&mut modal);

        assert_eq!(press(&mut modal, bind), AccountsAction::Consumed);
        assert!(!modal.text_input_active());
    }

    #[test]
    fn a_rename_answers_with_the_typed_name() {
        let mut modal = opened(pool());
        press(&mut modal, RENAME_KEY);
        assert!(modal.text_input_active());
        for _ in PERSONAL.chars() {
            modal.handle_key(KeyEvent::from(KeyCode::Backspace));
        }
        modal.handle_text(WORK, false);

        assert_eq!(
            press(&mut modal, key::ENTER),
            AccountsAction::Rename {
                from: name(PERSONAL),
                to: name(WORK),
            }
        );
        assert!(!modal.text_input_active());
    }

    #[test]
    fn letters_never_reach_a_search() {
        let mut modal = opened(pool());

        assert!(modal.handle_text(WORK, false));
        assert_eq!(modal.selected_name(), Some(PERSONAL));
    }

    fn exhaustion(mode: SelectionMode, alternatives: Vec<AccountAlternative>) -> AccountExhaustion {
        AccountExhaustion {
            provider: PROVIDER.into(),
            account: PERSONAL.into(),
            mode,
            until_ms: None,
            alternatives,
        }
    }

    fn alternative(name: &str, billing: Billing, left_percent: Option<u8>) -> AccountAlternative {
        AccountAlternative {
            name: name.into(),
            billing,
            left_percent,
            fallback: false,
        }
    }

    #[test_case(
        SelectionMode::Pinned,
        vec![alternative(WORK, Billing::Subscription, Some(88)), alternative(API, Billing::Api, None)],
        &["Switch to work (88% left)", TURN_ON_AUTO, "Use api (pay per token) for this run", WAIT, BROWSE]
        ; "pinned_offers_the_others_and_auto"
    )]
    #[test_case(
        SelectionMode::Auto,
        vec![alternative(API, Billing::Api, None)],
        &["Use api (pay per token) for this run", WAIT, BROWSE]
        ; "auto_offers_only_the_key"
    )]
    fn the_card_offers_somewhere_else_to_go(
        mode: SelectionMode,
        alternatives: Vec<AccountAlternative>,
        expected: &[&str],
    ) {
        let mut modal = AccountsModal::new();
        modal.show_exhaustion(pool(), exhaustion(mode, alternatives));

        assert_eq!(modal.card_labels(), expected);
    }

    #[test]
    fn choosing_a_card_switch_retries_at_once() {
        let mut modal = AccountsModal::new();
        modal.show_exhaustion(
            pool(),
            exhaustion(
                SelectionMode::Pinned,
                vec![alternative(WORK, Billing::Subscription, Some(88))],
            ),
        );

        assert_eq!(
            press(&mut modal, key::ENTER),
            AccountsAction::Select {
                selection: AccountSelection::Pinned(name(WORK)),
                retry: true,
            }
        );
        assert!(!modal.is_open());
    }

    #[test]
    fn a_card_put_away_stays_down_until_a_request_goes_through() {
        let spent = || exhaustion(SelectionMode::Pinned, Vec::new());
        let mut modal = AccountsModal::new();
        modal.show_exhaustion(pool(), spent());

        assert_eq!(press(&mut modal, key::ESC), AccountsAction::Closed);
        modal.show_exhaustion(pool(), spent());
        assert!(!modal.is_open());

        modal.stop_waiting();
        modal.show_exhaustion(pool(), spent());
        assert!(modal.is_open());
    }

    #[test]
    fn browsing_from_the_card_lists_the_accounts() {
        let mut modal = AccountsModal::new();
        modal.show_exhaustion(pool(), exhaustion(SelectionMode::Auto, Vec::new()));
        modal.handle_key(KeyEvent::from(KeyCode::Down));

        assert_eq!(press(&mut modal, key::ENTER), AccountsAction::Browse);
        assert!(modal.is_open());
        assert_eq!(modal.selected_name(), Some(PERSONAL));
    }

    #[test_case(None, "One account: every request goes to it. n adds another." ; "a_single_account")]
    #[test_case(Some(SwitchStrategy::Paced), "Auto, paced: uses the one furthest ahead of its quota, moves below 5% left, or below 20% once the prompt cache is cold." ; "paced")]
    fn the_policy_line_says_how_requests_are_routed(
        strategy: Option<SwitchStrategy>,
        expected: &str,
    ) {
        let mut shown = match strategy {
            Some(_) => pool(),
            None => view(AccountSelection::Auto, vec![subscription(PERSONAL)]),
        };
        if let Some(strategy) = strategy {
            shown.pool.strategy = strategy;
        }

        assert_eq!(policy_line(&shown.pool), expected);
    }

    #[test]
    fn the_detail_names_the_quota_left_and_a_block() {
        let mut account = subscription(PERSONAL);
        account.identity.plan = Some("Max".into());
        account.identity.email = Some("me@example.com".into());
        account.windows = vec![QuotaWindow::new(WindowKind::FiveHour, 59.0, None)];
        account.blocked_until = Some(NOW_MS + 2 * HOUR_MS);

        assert_eq!(
            account_detail(&account, NOW_MS),
            "Max · me@example.com · 5h 41% left · blocked for 2h00m"
        );
    }
}
