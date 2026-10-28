use std::array;
use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use arc_swap::ArcSwapOption;

use caudra_config::ClockFormat;
use caudra_grab::grab_scope;
use caudra_providers::accounts::AccountStatus;
use caudra_providers::{
    AgentError, Billing, Model, ModelSpend, NOT_BILLED_MARK, ProviderUsage, TokenUsage, add_cost,
    format_hit_rate, format_tokens, format_tokens_u64, model_cost,
};
use caudra_storage::cache_warming::{EpisodeKind, EpisodeOutcome, WarmingSummary};
use caudra_storage::sessions::StoredTokenUsage;
use caudra_storage::usage_ledger::{LifetimeUsage, UsageSlice};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::components::accounts_modal::account_detail;
use crate::components::docs_link::DocsTopic;
use crate::components::input::control_tip;
use crate::components::keybindings::{Bind, key};
use crate::components::modal::{FooterHits, FooterLine, Modal};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::tooltip::{Tip, TipArea, TipKey, area_tip};
use crate::components::{ModalScroll, bar_area, hover_style};
use crate::repaint::{Dirty, Watch};
use crate::theme::{self, Theme};

const TITLE: &str = " Token usage ";
const ACCOUNTS_TITLE: &str = "Accounts";
const LIFETIME_TITLE: &str = " Token usage - lifetime ";
const SLICE_LIMIT: usize = 8;
const TIP_RELOAD: &str = "Rereads recorded spend and the provider's usage limits";
const TIP_LIFETIME: &str = "Shows spend across every session and project ever recorded";
const TIP_SESSION: &str = "Shows the spend of this session alone";
const TIP_MORE: &str = "Shows every row of the lists cut at ";
const TIP_LESS: &str = "Folds every list back to its costliest ";
/// A cap hides rows only when it hides at least two: the line saying one row
/// is hidden takes the room the row would have.
const COLLAPSE_ABOVE: usize = SLICE_LIMIT + 1;
const SHOW_LESS: &str = "show less";
const PROVIDER_COLUMN: &str = "provider";
const MODEL_COLUMN: &str = "model";
const PURPOSE_COLUMN: &str = "purpose";
/// Expands every capped lifetime breakdown, or collapses them again.
pub(crate) const EXPAND_KEY: Bind = Bind {
    code: KeyCode::Char('e'),
    modifiers: KeyModifiers::NONE,
    label: "e",
};
const NO_LEDGER: &str = "no spend recorded yet";
const LEDGER_UNAVAILABLE: &str = "lifetime spend unavailable";
/// Switches the modal between this session and everything ever recorded.
pub(crate) const SCOPE_KEY: Bind = key::SCOPE;
const PREFIX: &str = "  ";
const MODEL_COL_MIN: usize = 16;
const NUM_COL: usize = 7;
const CACHE_READ_COL: usize = 10;
const CACHE_WRITE_COL: usize = 11;
const COST_COL: usize = 6;
const LIFETIME_COST_COL: usize = 8;
const TURNS_COL: usize = 6;
const TOKEN_INTEGER_PADDING: &str = "   ";
/// Wide enough for `100%`, and for the dash that says a provider reported no
/// prompt tokens to score.
const RATE_COL: usize = 4;
const COL_GAP: usize = 2;
const NO_USAGE_ENDPOINT: &str = "no usage endpoint for this provider";
const LOADING: &str = "loading…";
const REFRESHING: &str = "refreshing…";
const STALE_PREFIX: &str = "couldn't refresh: ";
const STALE_FROM: &str = "showing figures from ";
const FRESH_FROM: &str = "as of ";
const NOTE_SEPARATOR: &str = " · ";
/// Follows [`NOT_BILLED_MARK`] to say what a subscription figure is: the API
/// list price for the same tokens, with no invoice behind it.
const SUBSCRIPTION_LEGEND: &str = " is what a subscription covered, not billed";
const SESSION_DECIMALS: usize = 3;
const LIFETIME_DECIMALS: usize = 2;
const WARMING_HEADING: &str = "Cache warming";
const WARMING_HEADER: [&str; 7] = ["mode", "episodes", "warms", "cost", "saved", "net", "hit"];
/// A shadow row is what warming would have done, never what it did.
const SHADOW_SUFFIX: &str = " (estimated if enabled)";
const UNCALIBRATED_NOTE: &str =
    " of estimated savings left out: too few cache hits observed on that model yet";
const TIP_BASE_IN: &str =
    "Input tokens sent at full price, neither read from nor written to the cache";
const TIP_OUT: &str = "Tokens the model wrote";
const TIP_CACHE_READ: &str =
    "Prompt tokens served from the provider's cache, billed at a fraction of base input";
const TIP_CACHE_WRITE: &str =
    "Prompt tokens written to the cache, billed above base input so later requests can read them";
const TIP_CACHE: &str =
    "Cache read plus cache write: prompt tokens read from or written to the provider's cache";
const TIP_TOTAL: &str = "Base input, output, cache read and cache write together";
const TIP_TOKENS: &str = "Every token of the row: base input, output, cache read and cache write";
const TIP_HIT: &str =
    "Share of prompt tokens read from the cache. — means the provider reported no prompt tokens";
const TIP_TURNS: &str = "Model requests recorded, each priced when it happened";
const TIP_COST: &str = "List price of these tokens in USD. ≈ marks what a subscription covered, and — means no price is known";
const TIP_BILLED: &str = "Billed at API list prices";
const TIP_COVERED: &str = "What a subscription covered, at API list prices. Nothing was billed";
const TIP_USED: &str = "Share of this window's quota used, as the provider reports it";
const TIP_RESETS: &str = "When this window's quota starts over";
const TIP_QUOTA: &str = "The usage limits of the account in use, as the provider reports them";
const TIP_REFRESH_AGE: &str = "Asks the provider for its usage limits again\nClick";
const TIP_ACCOUNTS: &str = "Where each of this provider's accounts stands as of its latest reading. /accounts manages them";
const TIP_PURPOSE: &str = "Why Caudra called the model: chat is the conversation, the rest is work Caudra did on your behalf";
const TIP_PER_ACCOUNT: &str =
    "Turns recorded before Caudra tracked accounts have none and stay out of this list";
const TIP_PER_PROJECT: &str = "Spend by the directory each session ran in";
const TIP_SHOW_ALL: &str = "Shows every row of this list";
const TIP_FOLD: &str = "Folds this list back to its costliest ";
const TIP_TOGGLE_CLICK: &str = "Click, or press ";
const TIP_TOGGLE_EVERY: &str = " for every list";
const TIP_EPHEMERAL: &str =
    "Spend from --ephemeral runs, which keep no session. Already counted in the totals above";
const TIP_UNPRICED: &str = "A turn has no price when its model is missing from the price table, so the total understates by an unknown amount";
const TIP_UNCALIBRATED: &str = "Estimated savings count only once enough cache hits on a model show how often it hits without warming";
const TIP_WARMING: &str = "Resends a request just before its prompt cache expires, so the next one reads it. While off, rows estimate what it would save";
const TIP_SHADOW: &str = "What warming would have done had it been on. Nothing was sent";
/// One per [`WARMING_HEADER`] column.
const WARMING_TIPS: [&str; 7] = [
    "When warming runs: while a run waits (running), or also after it ends (idle)",
    "Waits long enough for the prompt cache to expire, each ending at the next request",
    "Warm requests sent",
    "What the warms cost, in USD",
    "The cache writes the warms avoided, in USD",
    "Saved minus cost",
    "Of the requests that came after the cache would have expired, the share that read it",
];
const TIP_SAVED: &str = "The cache would have expired, and the next request read it";
const TIP_MARGINAL: &str = "The next request read the cache near its expiry, where it might have hit without a warm. Never counted as saved";
const TIP_UNNEEDED: &str = "The next request came before the cache expired, so no warm was needed";
const TIP_MISSED: &str = "The next request missed the cache although the warms kept it";
const TIP_LAPSED: &str = "Warming stopped, and the next request came after the cache expired";
const TIP_UNUSED: &str = "No next request read the prompt: the run ended, the conversation changed, or the session closed";
const TIP_WROTE: &str = "A warm wrote a new cache entry instead of refreshing the existing one";
const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
const WEEK: i64 = 7 * DAY;

/// A quota answer the provider gave, and when, so a failed refresh can keep
/// showing it with its age.
#[derive(Clone)]
pub struct FetchedUsage {
    pub usage: ProviderUsage,
    pub fetched_at: Timestamp,
}

/// Live provider quota fetch, shared from the event loop. A detached task
/// drops the answer into the slot, and [`UsageModal::poll`] is what notices.
///
/// Usage endpoints rate-limit far harder than chat does, so a refresh carries
/// the last good answer forward instead of trading it for an error.
pub enum UsageFetchState {
    Loading {
        last: Option<FetchedUsage>,
    },
    Ready(FetchedUsage),
    Unsupported,
    Error {
        message: String,
        /// When the provider said to ask again; until then a refresh stays home.
        retry_at: Option<Timestamp>,
        last: Option<FetchedUsage>,
    },
}

impl UsageFetchState {
    pub fn settled(
        last: Option<FetchedUsage>,
        result: Result<Option<ProviderUsage>, AgentError>,
        now: Timestamp,
    ) -> Self {
        match result {
            Ok(Some(usage)) => Self::Ready(FetchedUsage {
                usage,
                fetched_at: now,
            }),
            Ok(None) => Self::Unsupported,
            Err(error) => Self::Error {
                message: error.user_message_without_wait(),
                retry_at: error
                    .retry_after()
                    .and_then(|after| now.checked_add(after).ok()),
                last,
            },
        }
    }

    pub fn last_good(&self) -> Option<&FetchedUsage> {
        match self {
            Self::Ready(fetched) => Some(fetched),
            Self::Loading { last } | Self::Error { last, .. } => last.as_ref(),
            Self::Unsupported => None,
        }
    }

    /// Whether the figures' age is on screen as something to click. Not while
    /// a fetch is in flight, which a second one would only race.
    fn offers_refresh(&self) -> bool {
        matches!(self, Self::Ready(_) | Self::Error { last: Some(_), .. })
    }

    pub fn cooling_down(&self, now: Timestamp) -> bool {
        matches!(self, Self::Error { retry_at: Some(at), .. } if *at > now)
    }
}

/// Which of the two answers the modal is showing: what this session spent, or
/// what every session ever spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UsageScope {
    #[default]
    Session,
    Lifetime,
}

pub struct UsageModalContext<'a> {
    pub total: &'a TokenUsage,
    /// What the session billed, from [`caudra_providers::session_cost`]. `None`
    /// means nothing here is priced, so the modal shows tokens only.
    pub total_cost: Option<f64>,
    /// The same for turns a subscription covered, shown on its own line rather
    /// than folded into `total_cost`.
    pub subscription_cost: Option<f64>,
    pub by_model: &'a HashMap<String, StoredTokenUsage>,
    /// The same spend keyed by purpose, empty for turns recorded before the
    /// session kept it.
    pub by_purpose: &'a HashMap<String, StoredTokenUsage>,
    pub model: &'a Model,
    pub fast: bool,
    pub clock_format: ClockFormat,
    /// `None` when the ledger could not be read, which is different from an
    /// empty ledger and says so.
    pub lifetime: Option<&'a LifetimeUsage>,
    /// Every account of the model's provider, the one requests go to first.
    /// Listed only once there are two.
    pub accounts: &'a [AccountStatus],
    /// Cache-warming episodes for the scope on screen. `None` until read, or
    /// when the table could not be.
    pub warming: Option<&'a WarmingSummary>,
}

pub struct UsageModal {
    open: bool,
    scope: UsageScope,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    /// The token table is as wide as its longest model id, which owes nothing to
    /// the terminal, so on a small screen the right-hand columns run off the
    /// modal and this is what reaches them.
    pan_bar: Scrollbar,
    quota: Watch<UsageFetchState>,
    /// The quota's age, which a click turns into a refresh.
    refresh: FooterHits,
    /// Lifetime breakdowns showing every row rather than the first few.
    expanded: HashSet<LifetimeSection>,
    /// The `... and N more` and `show less` lines, which a click flips.
    toggles: FooterHits,
    /// Which section each of `toggles` flips, in the same order.
    toggle_sections: Vec<LifetimeSection>,
    /// The key hints on the top border, which a click presses.
    title: FooterHits,
    /// What each of `title` does, in the same order.
    title_actions: Vec<TitleAction>,
    /// Where each explained span of the body landed on screen, and what it says.
    body_tips: Vec<TipArea>,
    /// Where the pointer last moved to, `None` after anything but a move.
    pointer: Option<Position>,
    popup: Rect,
}

/// A key hint on the title row, answering a click as its key does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TitleAction {
    Reload,
    Scope,
    Expand,
}

/// A lifetime breakdown long enough to be capped. Purposes are a short fixed
/// list and always show whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum LifetimeSection {
    Provider,
    Account,
    Model,
    Project,
    Month,
}

/// What the pointer asked of the host. `Ignored` hands the event back.
/// `Refresh` asks the provider for its quota again, and `Reload` rereads
/// everything, as `Ctrl+R` does.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UsageMouse {
    Ignored,
    Consumed,
    Refresh,
    Reload,
}

/// What span `span` of line `row` means, for the box the pointer raises.
struct SpanTip {
    row: usize,
    span: usize,
    text: Cow<'static, str>,
}

/// The modal's lines, and the spans among them that explain themselves.
#[derive(Default)]
struct Content {
    lines: Vec<Line<'static>>,
    tips: Vec<SpanTip>,
}

impl Content {
    fn push(&mut self, line: Line<'static>) {
        self.lines.push(line);
    }

    fn push_tipped(&mut self, line: TippedLine) {
        let row = self.lines.len();
        self.tips.extend(
            line.tips
                .into_iter()
                .map(|(span, text)| SpanTip { row, span, text }),
        );
        self.lines.push(Line::from(line.spans));
    }

    fn tip(&mut self, row: usize, span: usize, text: impl Into<Cow<'static, str>>) {
        self.tips.push(SpanTip {
            row,
            span,
            text: text.into(),
        });
    }

    fn extend(&mut self, other: Content) {
        let offset = self.lines.len();
        self.tips.extend(other.tips.into_iter().map(|tip| SpanTip {
            row: tip.row + offset,
            ..tip
        }));
        self.lines.extend(other.lines);
    }

    fn len(&self) -> usize {
        self.lines.len()
    }
}

/// One line being built, and which of its spans explain themselves.
#[derive(Default)]
struct TippedLine {
    spans: Vec<Span<'static>>,
    tips: Vec<(usize, Cow<'static, str>)>,
}

impl TippedLine {
    fn push(&mut self, span: Span<'static>) -> &mut Self {
        self.spans.push(span);
        self
    }

    fn tipped(&mut self, span: Span<'static>, text: impl Into<Cow<'static, str>>) -> &mut Self {
        self.tips.push((self.spans.len(), text.into()));
        self.push(span)
    }

    fn width(&self) -> usize {
        self.spans.iter().map(Span::width).sum()
    }
}

impl From<Line<'static>> for TippedLine {
    fn from(line: Line<'static>) -> Self {
        Self {
            spans: line.spans,
            tips: Vec::new(),
        }
    }
}

/// A section heading, explained by `tip` when it has one.
fn section_heading(
    text: impl Into<Cow<'static, str>>,
    tip: Option<&'static str>,
    theme: &Theme,
) -> TippedLine {
    let style = theme.keybind_section;
    let mut line = TippedLine::default();
    line.push(Span::styled(PREFIX, style));
    match tip {
        Some(tip) => line.tipped(Span::styled(text.into(), style), tip),
        None => line.push(Span::styled(text.into(), style)),
    };
    line
}

/// A dim note whose text, past the indent, explains itself with `tip`.
fn tipped_note(text: String, tip: &'static str, style: Style) -> TippedLine {
    let mut line = TippedLine::default();
    line.push(Span::styled(PREFIX, style))
        .tipped(Span::styled(text, style), tip);
    line
}

impl UsageModal {
    pub fn new() -> Self {
        Self {
            open: false,
            scope: UsageScope::default(),
            scroll: ModalScroll::new_top(),
            scrollbar: Scrollbar::default(),
            pan_bar: Scrollbar::horizontal(),
            quota: Watch::default(),
            refresh: FooterHits::default(),
            expanded: HashSet::new(),
            toggles: FooterHits::default(),
            toggle_sections: Vec::new(),
            title: FooterHits::default(),
            title_actions: Vec::new(),
            body_tips: Vec::new(),
            pointer: None,
            popup: Rect::default(),
        }
    }

    /// Picks up a finished quota fetch. Nothing wakes the loop when the task
    /// stores its result, so an unpolled modal sits on `Loading` until the
    /// user happens to press a key.
    pub fn poll(&mut self, slot: &ArcSwapOption<UsageFetchState>) -> Dirty {
        if !self.open {
            return Dirty::NO;
        }
        self.quota.poll(slot.load_full())
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.open && self.popup.contains(pos)
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.reset_view();
    }

    fn reset_view(&mut self) {
        self.scroll.reset();
        self.refresh.reset();
        self.toggles.reset();
        self.title.reset();
        self.expanded.clear();
        self.body_tips.clear();
        self.pointer = None;
    }

    fn flip_section(&mut self, section: LifetimeSection) {
        if !self.expanded.remove(&section) {
            self.expanded.insert(section);
        }
    }

    /// Opens every capped breakdown at once, or closes them all once any is
    /// open, so one key always undoes the last press.
    fn flip_all_sections(&mut self) {
        if self.expanded.is_empty() {
            self.expanded.extend(self.toggle_sections.iter().copied());
        } else {
            self.expanded.clear();
        }
    }

    /// Where the title row's control at `index` was drawn.
    #[cfg(test)]
    pub(crate) fn title_hit(&self, index: usize) -> Rect {
        self.title.hit(index)
    }

    pub fn scope(&self) -> UsageScope {
        self.scope
    }

    fn toggle_scope(&mut self) {
        self.scope = match self.scope {
            UsageScope::Session => UsageScope::Lifetime,
            UsageScope::Lifetime => UsageScope::Session,
        };
        self.scroll.reset();
    }

    /// Keeps the last answer: `/usage` refetches on every open, and until that
    /// lands it beats a blank panel.
    pub fn close(&mut self) {
        self.open = false;
        self.reset_view();
    }

    /// The quota's age is asked first, the way a footer control is: a press on
    /// it is never the start of a sweep. The two bars sit on different rows, so
    /// at most one of them answers a press.
    pub(crate) fn handle_mouse(&mut self, event: &MouseEvent) -> UsageMouse {
        if !self.open {
            return UsageMouse::Ignored;
        }
        self.pointer =
            (event.kind == MouseEventKind::Moved).then_some(Position::new(event.column, event.row));
        if let Some(index) = self.title.handle_mouse(*event) {
            match self.title_actions.get(index) {
                Some(TitleAction::Reload) => return UsageMouse::Reload,
                Some(TitleAction::Scope) => self.toggle_scope(),
                Some(TitleAction::Expand) => self.flip_all_sections(),
                None => {}
            }
            return UsageMouse::Consumed;
        }
        if self.refresh.handle_mouse(*event).is_some() {
            return UsageMouse::Refresh;
        }
        if let Some(index) = self.toggles.handle_mouse(*event) {
            if let Some(&section) = self.toggle_sections.get(index) {
                self.flip_section(section);
            }
            return UsageMouse::Consumed;
        }
        if event.kind == MouseEventKind::Down(MouseButton::Left)
            && [&self.title, &self.refresh, &self.toggles]
                .iter()
                .any(|hits| hits.hovered().is_some())
        {
            return UsageMouse::Consumed;
        }
        match self.scrollbar.handle(event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return UsageMouse::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                return UsageMouse::Consumed;
            }
        }
        match self.pan_bar.handle(event) {
            ScrollbarMouse::Ignored => UsageMouse::Ignored,
            ScrollbarMouse::Consumed => UsageMouse::Consumed,
            ScrollbarMouse::ScrollTo(column) => {
                self.scroll.pan_to(column as u16);
                UsageMouse::Consumed
            }
        }
    }

    /// A sideways wheel over the modal, which the app routes here rather than
    /// dropping now that the content can run off the edge.
    pub fn pan(&mut self, delta: i32) {
        self.scroll.pan_by(delta);
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) {
        if key_event.code == KeyCode::Esc || key::QUIT.matches(key_event) {
            self.close();
            return;
        }
        if SCOPE_KEY.matches(key_event) {
            self.toggle_scope();
            return;
        }
        if self.scope == UsageScope::Lifetime && EXPAND_KEY.matches(key_event) {
            self.flip_all_sections();
            return;
        }
        self.scroll.handle_key(key_event);
    }

    /// `Ctrl+R reload`, the scope to switch to, and in lifetime scope with a
    /// capped breakdown, `e more` or `e less`. Records what each control does.
    fn title_line(&mut self, theme: &Theme) -> FooterLine {
        let mut line = FooterLine::default();
        let mut actions = Vec::new();
        let mut control = |line: &mut FooterLine, bind: Bind, gloss: &'static str, action| {
            line.command(bind.label, theme.keybind_key);
            line.describe(gloss, theme.tool_dim);
            actions.push(action);
        };
        line.text(" ", theme.tool_dim);
        control(&mut line, key::REFRESH, " reload ", TitleAction::Reload);
        let scope = match self.scope {
            UsageScope::Session => " lifetime ",
            UsageScope::Lifetime => " session ",
        };
        control(&mut line, SCOPE_KEY, scope, TitleAction::Scope);
        if !self.toggle_sections.is_empty() {
            let expand = match self.expanded.is_empty() {
                true => " more ",
                false => " less ",
            };
            control(&mut line, EXPAND_KEY, expand, TitleAction::Expand);
        }
        self.title_actions = actions;
        line
    }

    /// Mirrors `title_line`, so a tip always explains the gloss drawn beside it.
    fn title_tip(&self) -> Option<Tip> {
        self.title.tip(TipKey::TitleHint, |index| {
            let (meaning, bind) = match self.title_actions.get(index)? {
                TitleAction::Reload => (TIP_RELOAD.into(), key::REFRESH),
                TitleAction::Scope => match self.scope {
                    UsageScope::Session => (TIP_LIFETIME.into(), SCOPE_KEY),
                    UsageScope::Lifetime => (TIP_SESSION.into(), SCOPE_KEY),
                },
                TitleAction::Expand => match self.expanded.is_empty() {
                    true => (format!("{TIP_MORE}{SLICE_LIMIT}"), EXPAND_KEY),
                    false => (format!("{TIP_LESS}{SLICE_LIMIT}"), EXPAND_KEY),
                },
            };
            Some(control_tip(&meaning, bind.label))
        })
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect, ctx: &UsageModalContext) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("usage_modal", area);

        let theme = theme::current();
        let (mut content, refresh_row, toggle_rows) = match self.scope {
            UsageScope::Session => {
                let (content, refresh_row) = build_lines(ctx, self.quota.get(), &theme);
                (content, refresh_row, Vec::new())
            }
            UsageScope::Lifetime => {
                let (content, toggles) = build_lifetime_lines(ctx.lifetime, &self.expanded, &theme);
                (content, None, toggles)
            }
        };
        content.extend(warming_lines(ctx.warming, &theme));
        let Content { mut lines, tips } = content;

        let total = lines.len() as u16;
        let modal = Modal {
            title: match self.scope {
                UsageScope::Session => TITLE,
                UsageScope::Lifetime => LIFETIME_TITLE,
            },
            width_percent: 70,
            max_height_percent: 70,
        };
        let (popup, inner) = modal.render(frame, area, total);
        let content_w = lines
            .iter()
            .map(Line::width)
            .max()
            .and_then(|width| u16::try_from(width).ok())
            .unwrap_or(u16::MAX);
        self.scroll.update_dimensions(total, inner.height);
        self.scroll.fit_width(content_w, inner.width);
        let scroll = self.scroll.offset();
        let pan = self.scroll.pan();

        self.body_tips = tips
            .into_iter()
            .filter_map(|tip| {
                let line = lines.get(tip.row)?;
                Some(TipArea {
                    area: span_hit(line, tip.span, tip.row, inner, scroll, pan)?,
                    text: tip.text.into_owned(),
                })
            })
            .collect();
        let hit = |row: usize| {
            let line = lines.get(row)?;
            span_hit(
                line,
                line.spans.len().checked_sub(1)?,
                row,
                inner,
                scroll,
                pan,
            )
        };
        self.refresh
            .set(refresh_row.and_then(hit).into_iter().collect());
        // An off-screen toggle keeps an empty rect so the indices still line up
        // with `toggle_sections`.
        let (toggle_rows, sections): (Vec<usize>, Vec<LifetimeSection>) =
            toggle_rows.into_iter().unzip();
        self.toggles.set(
            toggle_rows
                .iter()
                .map(|&row| hit(row).unwrap_or_default())
                .collect(),
        );
        self.toggle_sections = sections;
        let hovered = [
            self.refresh.hovered().and(refresh_row),
            self.toggles
                .hovered()
                .and_then(|index| toggle_rows.get(index).copied()),
        ];
        for row in hovered.into_iter().flatten() {
            if let Some(target) = lines.get_mut(row).and_then(|line| line.spans.last_mut()) {
                target.style = hover_style(target.style, true);
            }
        }

        frame.render_widget(Paragraph::new(lines).scroll((scroll, pan)), inner);

        self.scrollbar.draw(frame, inner, total, scroll);
        // Handed the table and its bottom border, the way the bar above is handed
        // the whole body: it paints on the last row, which is the only row it can
        // have without taking one from the table, and a fingertip gets its margin
        // out of the rows over it. Nothing is painted while the table fits,
        // because a track is only built for content that overflows.
        self.pan_bar.draw(frame, bar_area(inner), content_w, pan);

        // Beside the title rather than under the table: the bottom border row
        // belongs to the pan bar, and a hint sharing it would be overpainted
        // exactly when the modal is narrow enough to need both.
        let hint = self.title_line(&theme);
        let hits = hint.draw_on_title(frame, popup, self.title.hovered());
        self.title.set(hits);

        self.popup = popup;
        popup
    }
}

/// The lines, and which of them ends in the quota's clickable age.
fn build_lines(
    ctx: &UsageModalContext,
    quota: Option<&UsageFetchState>,
    theme: &crate::theme::Theme,
) -> (Content, Option<usize>) {
    let mut lines = Content::default();
    let mut refresh_row = None;

    lines.push(Line::from(Span::styled(
        format!("{PREFIX}Session total"),
        theme.keybind_section,
    )));

    let covered = covered_spend(ctx.subscription_cost);
    lines.extend(spend_lines(
        totals_row(ctx.total, theme),
        ctx.total_cost,
        covered,
        SESSION_DECIMALS,
        theme,
    ));

    if let Some(state) = quota {
        lines.push(Line::default());
        lines.push_tipped(section_heading(
            format!("{} quota", ctx.model.provider_display_name()),
            Some(TIP_QUOTA),
            theme,
        ));
        lines.extend(quota_lines(
            state,
            theme,
            ctx.clock_format,
            Timestamp::now(),
        ));
        if state.offers_refresh()
            && let Some(row) = lines.len().checked_sub(1)
        {
            refresh_row = Some(row);
            let age = lines.lines[row].spans.len().saturating_sub(1);
            lines.tip(row, age, TIP_REFRESH_AGE);
        }
    }
    if ctx.accounts.len() > 1 {
        lines.extend(account_lines(ctx.accounts, theme));
    }

    if ctx.by_model.is_empty() {
        return (lines, refresh_row);
    }

    let mut models = model_rows(ctx);
    let providers = provider_rows(&models, &ctx.model.provider);
    // One provider row would only restate the session total, which already
    // carries its own rate. The fold earns its lines once there are two.
    if providers.len() > 1 {
        lines.extend(breakdown_table(
            "Per provider",
            PROVIDER_COLUMN,
            &providers,
            theme,
        ));
    }
    for row in &mut models {
        row.trim_provider(&ctx.model.provider);
    }
    lines.extend(breakdown_table("Per model", MODEL_COLUMN, &models, theme));
    if !ctx.by_purpose.is_empty() {
        lines.extend(breakdown_table(
            "Per purpose",
            PURPOSE_COLUMN,
            &purpose_rows(ctx.by_purpose),
            theme,
        ));
    }

    (lines, refresh_row)
}

/// Where span `span` of `line`, drawn at `row` of an unwrapped paragraph
/// scrolled to `scroll` and panned to `pan`, landed inside `area`, clipped to
/// what is visible.
fn span_hit(
    line: &Line,
    span: usize,
    row: usize,
    area: Rect,
    scroll: u16,
    pan: u16,
) -> Option<Rect> {
    let target = line.spans.get(span)?;
    let row = u16::try_from(row).ok()?.checked_sub(scroll)?;
    if row >= area.height {
        return None;
    }
    let before: usize = line.spans[..span].iter().map(Span::width).sum();
    let start = u16::try_from(before).ok()?;
    let end = start.saturating_add(u16::try_from(target.width()).ok()?);
    let left = start.max(pan) - pan;
    let right = end.saturating_sub(pan).min(area.width);
    (left < right).then(|| Rect::new(area.x + left, area.y + row, right - left, 1))
}

/// One line of a session breakdown, before it is laid out.
struct BreakdownRow {
    label: String,
    usage: StoredTokenUsage,
    spend: ModelSpend,
}

impl BreakdownRow {
    /// Drops the prefix once it names the session's own provider, so the common
    /// case reads as a bare model id. Presentation only, and run after the fold
    /// that needs the slug.
    fn trim_provider(&mut self, provider: &str) {
        if let Some(model) = self.label.strip_prefix(&format!("{provider}/")) {
            self.label = model.to_owned();
        }
    }
}

/// The session's models, dearest in tokens first, labelled by the
/// `provider/model` spec they were recorded under.
fn model_rows(ctx: &UsageModalContext) -> Vec<BreakdownRow> {
    let mut entries: Vec<(&String, &StoredTokenUsage)> = ctx.by_model.iter().collect();
    entries.sort_by_key(|(_, usage)| Reverse(usage.total()));
    entries
        .into_iter()
        .map(|(id, usage)| BreakdownRow {
            label: id.clone(),
            usage: *usage,
            spend: model_cost(id, usage, ctx.model, ctx.fast),
        })
        .collect()
}

/// The same spend folded by whoever served it. Built from the model rows rather
/// than from the map, so a provider row is exactly the sum of the rows printed
/// beneath it and the two tables cannot disagree. A slug is everything before
/// the first slash, matching how a spec is parsed; `fallback` owns anything
/// recorded before the keys named a provider.
fn provider_rows(models: &[BreakdownRow], fallback: &str) -> Vec<BreakdownRow> {
    let mut folded: Vec<BreakdownRow> = Vec::new();
    for model in models {
        let provider = model
            .label
            .split_once('/')
            .map_or(fallback, |(slug, _)| slug);
        match folded.iter_mut().find(|row| row.label == provider) {
            Some(row) => {
                row.usage += model.usage;
                add_cost(&mut row.spend.usd, model.spend.usd);
                // Any invoiced member makes the figure partly a bill, so the
                // row must not wear the mark that says nobody was charged.
                if !model.spend.billing.is_subscription() {
                    row.spend.billing = Billing::Api;
                }
            }
            None => folded.push(BreakdownRow {
                label: provider.to_owned(),
                usage: model.usage,
                spend: model.spend,
            }),
        }
    }
    folded.sort_by_key(|row| Reverse(row.usage.total()));
    folded
}

/// The session's spend by why each model was called, dearest first, then by
/// tokens, ties by name so the order holds still between frames. Only what was
/// recorded: no model names a purpose, so an unpriced row is never estimated.
fn purpose_rows(by_purpose: &HashMap<String, StoredTokenUsage>) -> Vec<BreakdownRow> {
    let mut rows: Vec<BreakdownRow> = by_purpose
        .iter()
        .map(|(purpose, usage)| BreakdownRow {
            label: purpose.clone(),
            usage: *usage,
            spend: ModelSpend::recorded(usage).unwrap_or_default(),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.spend
            .usd
            .unwrap_or(0.0)
            .total_cmp(&a.spend.usd.unwrap_or(0.0))
            .then_with(|| b.usage.total().cmp(&a.usage.total()))
            .then_with(|| a.label.cmp(&b.label))
    });
    rows
}

fn breakdown_table(
    heading: &str,
    column: &str,
    rows: &[BreakdownRow],
    theme: &crate::theme::Theme,
) -> Content {
    let fg = Style::new().fg(theme.foreground);
    let label_w = rows
        .iter()
        .map(|row| row.label.chars().count())
        .max()
        .unwrap_or(0)
        .max(MODEL_COL_MIN);
    let label_tip = (column == PURPOSE_COLUMN).then_some(TIP_PURPOSE);
    let cells: Vec<_> = rows.iter().map(breakdown_cells).collect();
    let widths = column_widths(
        &cells,
        [
            NUM_COL,
            NUM_COL,
            CACHE_READ_COL,
            CACHE_WRITE_COL,
            NUM_COL,
            COST_COL,
        ],
    );

    let mut lines = Content::default();
    lines.push(Line::default());
    lines.push_tipped(section_heading(heading.to_owned(), label_tip, theme));
    lines.push_tipped(header_row(column, label_tip, label_w, &widths, theme));
    for (row, cells) in rows.iter().zip(&cells) {
        lines.push(Line::from(model_row(
            row,
            cells,
            &widths,
            label_w,
            fg,
            theme.status_dim,
        )));
    }
    lines
}

/// The lines, and the rows that expand or collapse a capped section.
fn build_lifetime_lines(
    lifetime: Option<&LifetimeUsage>,
    expanded: &HashSet<LifetimeSection>,
    theme: &crate::theme::Theme,
) -> (Content, Vec<(usize, LifetimeSection)>) {
    let fg = Style::new().fg(theme.foreground);
    let mut lines = Content::default();
    let Some(lifetime) = lifetime else {
        lines.push(Line::from(Span::styled(
            format!("{PREFIX}{LEDGER_UNAVAILABLE}"),
            theme.status_dim,
        )));
        return (lines, Vec::new());
    };
    if lifetime.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("{PREFIX}{NO_LEDGER}"),
            theme.status_dim,
        )));
        return (lines, Vec::new());
    }

    let covered = covered_spend(Some(lifetime.subscription_cost));
    let totals = headline(
        [
            (
                "base in",
                format_tokens_u64(lifetime.input),
                NUM_COL,
                TIP_BASE_IN,
            ),
            ("out", format_tokens_u64(lifetime.output), NUM_COL, TIP_OUT),
            (
                "cache",
                format_tokens_u64(lifetime.cache_read + lifetime.cache_creation),
                NUM_COL,
                TIP_CACHE,
            ),
            (
                "total",
                format_tokens_u64(lifetime.total_tokens()),
                NUM_COL,
                TIP_TOTAL,
            ),
            (
                "hit",
                format_hit_rate(lifetime.cache_hit_rate()),
                RATE_COL,
                TIP_HIT,
            ),
            ("turns", lifetime.turns().to_string(), NUM_COL, TIP_TURNS),
        ],
        theme,
    );
    lines.push(Line::from(Span::styled(
        format!("{PREFIX}All time"),
        theme.keybind_section,
    )));
    lines.extend(spend_lines(
        totals,
        Some(lifetime.cost),
        covered,
        LIFETIME_DECIMALS,
        theme,
    ));
    if lifetime.ephemeral_cost > 0.0 {
        lines.push_tipped(tipped_note(
            format!("of which ephemeral runs: ${:.2}", lifetime.ephemeral_cost),
            TIP_EPHEMERAL,
            theme.status_dim,
        ));
    }
    if lifetime.unpriced_turns > 0 {
        lines.push_tipped(tipped_note(
            format!(
                "{} turns had no price, so the total is a floor",
                lifetime.unpriced_turns
            ),
            TIP_UNPRICED,
            theme.status_dim,
        ));
    }

    // A lone provider row only restates the all-time line above it, rate and
    // all, so that fold alone has to earn its heading with a second row.
    // Purposes have no section: there are few enough to always show whole.
    let mut toggles = Vec::new();
    for (heading, tip, slices, min_rows, section) in [
        (
            "Per provider",
            None,
            &lifetime.by_provider,
            2,
            Some(LifetimeSection::Provider),
        ),
        (
            "Per account",
            Some(TIP_PER_ACCOUNT),
            &lifetime.by_account,
            2,
            Some(LifetimeSection::Account),
        ),
        (
            "Per model",
            None,
            &lifetime.by_model,
            1,
            Some(LifetimeSection::Model),
        ),
        (
            "Per project",
            Some(TIP_PER_PROJECT),
            &lifetime.by_project,
            1,
            Some(LifetimeSection::Project),
        ),
        (
            "Per purpose",
            Some(TIP_PURPOSE),
            &lifetime.by_purpose,
            1,
            None,
        ),
        (
            "Per month",
            None,
            &lifetime.by_month,
            1,
            Some(LifetimeSection::Month),
        ),
    ] {
        if slices.len() < min_rows {
            continue;
        }
        lines.push(Line::default());
        lines.push_tipped(section_heading(heading, tip, theme));
        let section = section.filter(|_| slices.len() > COLLAPSE_ABOVE);
        let shown = match section {
            Some(section) if !expanded.contains(&section) => SLICE_LIMIT,
            _ => slices.len(),
        };
        lines.extend(slice_rows(&slices[..shown], fg, theme.status_dim));
        if let Some(section) = section {
            toggles.push((lines.len(), section));
            let (label, meaning) = match slices.len() - shown {
                0 => (SHOW_LESS.to_owned(), format!("{TIP_FOLD}{SLICE_LIMIT}")),
                hidden => (format!("... and {hidden} more"), TIP_SHOW_ALL.to_owned()),
            };
            let mut line = TippedLine::default();
            line.push(Span::raw(PREFIX)).tipped(
                Span::styled(label, theme.status_dim),
                format!(
                    "{meaning}\n{TIP_TOGGLE_CLICK}{}{TIP_TOGGLE_EVERY}",
                    EXPAND_KEY.label
                ),
            );
            lines.push_tipped(line);
        }
    }
    (lines, toggles)
}

/// Lifetime rows have no header, so each figure explains its own column.
fn slice_rows(slices: &[UsageSlice], fg: Style, dim: Style) -> Content {
    let label_w = slices
        .iter()
        .map(|slice| slice.label.chars().count())
        .max()
        .unwrap_or(0)
        .max(MODEL_COL_MIN);
    let cells: Vec<_> = slices
        .iter()
        .map(|slice| {
            [
                table_tokens(slice.tokens),
                slice.turns.to_string(),
                table_cost(
                    Some(slice.priced()),
                    slice.cost == 0.0 && slice.subscription_cost > 0.0,
                ),
            ]
        })
        .collect();
    let [token_w, turns_w, cost_w] = column_widths(&cells, [NUM_COL, TURNS_COL, LIFETIME_COST_COL]);
    let gap = || Span::raw(" ".repeat(COL_GAP));
    let mut lines = Content::default();
    for (slice, [tokens, turns, cost]) in slices.iter().zip(&cells) {
        let mut line = TippedLine::default();
        line.push(Span::raw(PREFIX))
            .push(Span::styled(format!("{:<label_w$}", slice.label), fg))
            .push(gap())
            .tipped(Span::styled(format!("{tokens:>token_w$}"), fg), TIP_TOKENS)
            .push(gap())
            .tipped(
                Span::styled(
                    format!("{:>RATE_COL$}", format_hit_rate(slice.cache_hit_rate())),
                    fg,
                ),
                TIP_HIT,
            )
            .push(gap())
            .tipped(Span::styled(format!("{turns:>turns_w$}"), dim), TIP_TURNS)
            .push(gap())
            // A slice can hold both payers, so the column shows what the
            // work was worth and marks it when none of it was billed.
            .tipped(
                Span::styled(
                    format!("{cost:>cost_w$}"),
                    if slice.cost == 0.0 && slice.subscription_cost > 0.0 {
                        dim
                    } else {
                        fg
                    },
                ),
                TIP_COST,
            );
        lines.push_tipped(line);
    }
    lines
}

/// What warming sent and saved, and beside it what it would have done while it
/// was off. Absent until a conversation has recorded an episode.
fn warming_lines(summary: Option<&WarmingSummary>, theme: &crate::theme::Theme) -> Content {
    let mut lines = Content::default();
    let Some(summary) = summary.filter(|summary| !summary.is_empty()) else {
        return lines;
    };
    let fg = Style::new().fg(theme.foreground);
    let labels: Vec<String> = summary
        .keys()
        .map(|(kind, mode)| match kind {
            EpisodeKind::Real => mode.clone(),
            EpisodeKind::Shadow => format!("{mode}{SHADOW_SUFFIX}"),
        })
        .collect();
    let cells: Vec<[String; 6]> = summary
        .iter()
        .map(|((kind, _), totals)| {
            [
                totals.episodes.to_string(),
                totals.warms.to_string(),
                format!("{:.3}", totals.warm_cost),
                format!("{:.3}", totals.saved_cost),
                format!("{:.3}", totals.net()),
                // A shadow never sees whether its warms would have hit.
                format_hit_rate(
                    totals
                        .post_warm_hit_rate()
                        .filter(|_| *kind == EpisodeKind::Real),
                ),
            ]
        })
        .collect();
    let label_w = labels
        .iter()
        .map(|label| label.chars().count())
        .chain([WARMING_HEADER[0].len()])
        .max()
        .unwrap_or(0);
    let widths = column_widths(
        &cells,
        array::from_fn(|index| WARMING_HEADER[index + 1].len()),
    );
    let gap = " ".repeat(COL_GAP);
    let dim = theme.status_dim;
    let mut header = TippedLine::default();
    header.push(Span::styled(PREFIX, dim)).tipped(
        Span::styled(format!("{:<label_w$}", WARMING_HEADER[0]), dim),
        WARMING_TIPS[0],
    );
    for ((title, width), tip) in WARMING_HEADER[1..]
        .iter()
        .zip(widths)
        .zip(&WARMING_TIPS[1..])
    {
        header
            .push(Span::styled(gap.clone(), dim))
            .tipped(Span::styled(format!("{title:>width$}"), dim), *tip);
    }
    lines.push(Line::default());
    lines.push_tipped(section_heading(WARMING_HEADING, Some(TIP_WARMING), theme));
    lines.push_tipped(header);
    for (((kind, _), totals), (label, row)) in summary.iter().zip(labels.iter().zip(&cells)) {
        let mut text = String::new();
        for (cell, width) in row.iter().zip(widths) {
            text.push_str(&format!("{gap}{cell:>width$}"));
        }
        let mut line = TippedLine::default();
        line.push(Span::styled(PREFIX, fg));
        let label = Span::styled(format!("{label:<label_w$}"), fg);
        match kind {
            EpisodeKind::Real => line.push(label),
            EpisodeKind::Shadow => line.tipped(label, TIP_SHADOW),
        };
        line.push(Span::styled(text, fg));
        lines.push_tipped(line);

        let mut outcomes = TippedLine::default();
        outcomes.push(Span::styled(format!("{PREFIX}{PREFIX}"), dim));
        for (index, (outcome, count)) in totals.outcomes.iter().enumerate() {
            if index > 0 {
                outcomes.push(Span::styled(NOTE_SEPARATOR, dim));
            }
            outcomes.tipped(
                Span::styled(format!("{} {count}", outcome.as_str()), dim),
                outcome_tip(*outcome),
            );
        }
        lines.push_tipped(outcomes);
    }
    let uncalibrated: f64 = summary
        .values()
        .map(|totals| totals.uncalibrated_cost)
        .sum();
    if uncalibrated > 0.0 {
        lines.push_tipped(tipped_note(
            format!("${uncalibrated:.3}{UNCALIBRATED_NOTE}"),
            TIP_UNCALIBRATED,
            dim,
        ));
    }
    lines
}

fn outcome_tip(outcome: EpisodeOutcome) -> &'static str {
    match outcome {
        EpisodeOutcome::Saved => TIP_SAVED,
        EpisodeOutcome::Marginal => TIP_MARGINAL,
        EpisodeOutcome::Unneeded => TIP_UNNEEDED,
        EpisodeOutcome::Missed => TIP_MISSED,
        EpisodeOutcome::Lapsed => TIP_LAPSED,
        EpisodeOutcome::Unused => TIP_UNUSED,
        EpisodeOutcome::Wrote => TIP_WROTE,
    }
}

/// Subscription spend worth reporting: none at all draws nothing, rather than
/// a "≈$0.000" beside the bill.
fn covered_spend(cost: Option<f64>) -> Option<f64> {
    cost.filter(|cost| *cost > 0.0)
}

/// The totals with the bill beside them, and under it what a subscription
/// covered beside the legend that explains its mark.
fn spend_lines(
    totals: TippedLine,
    billed: Option<f64>,
    covered: Option<f64>,
    decimals: usize,
    theme: &Theme,
) -> Content {
    let legend = subscription_legend(covered, theme);
    let billed = billed.map(|cost| (format!("${cost:.decimals$}"), TIP_BILLED));
    let covered = covered.map(|cost| (format!("{NOT_BILLED_MARK}${cost:.decimals$}"), TIP_COVERED));
    let amounts: Vec<_> = [billed, covered]
        .into_iter()
        .flatten()
        .map(|(text, tip)| (Span::styled(text, theme.accent), tip))
        .collect();
    let width = amounts
        .iter()
        .map(|(amount, _)| amount.width())
        .max()
        .unwrap_or_default();
    let offset = totals.width().max(
        legend
            .as_ref()
            .filter(|_| amounts.len() == 2)
            .map_or(0, Line::width),
    ) + COL_GAP;
    let mut rows = vec![totals];
    rows.extend(legend.map(TippedLine::from));
    for (row, (amount, tip)) in rows.iter_mut().zip(amounts) {
        let spacing = offset + width - amount.width() - row.width();
        row.push(Span::raw(" ".repeat(spacing))).tipped(amount, tip);
    }
    let mut lines = Content::default();
    for row in rows {
        lines.push_tipped(row);
    }
    lines
}

/// Says what the mark means, once there is a subscription figure to mark.
fn subscription_legend(covered: Option<f64>, theme: &Theme) -> Option<Line<'static>> {
    covered?;
    Some(Line::from(Span::styled(
        format!("{PREFIX}{NOT_BILLED_MARK}{SUBSCRIPTION_LEGEND}"),
        theme.status_dim,
    )))
}

fn totals_row(total: &TokenUsage, theme: &Theme) -> TippedLine {
    headline(
        [
            ("base in", format_tokens(total.input), NUM_COL, TIP_BASE_IN),
            ("out", format_tokens(total.output), NUM_COL, TIP_OUT),
            (
                "cache read",
                format_tokens(total.cache_read),
                NUM_COL,
                TIP_CACHE_READ,
            ),
            (
                "cache write",
                format_tokens(total.cache_creation),
                NUM_COL,
                TIP_CACHE_WRITE,
            ),
            (
                "total",
                format_tokens(total.context_tokens()),
                NUM_COL,
                TIP_TOTAL,
            ),
            (
                "hit",
                format_hit_rate(total.cache_hit_rate()),
                RATE_COL,
                TIP_HIT,
            ),
        ],
        theme,
    )
}

/// A totals line, one span per `(label, value, width, tip)` field so each
/// explains itself, spaced exactly as one formatted string would be.
fn headline<const N: usize>(
    fields: [(&str, String, usize, &'static str); N],
    theme: &Theme,
) -> TippedLine {
    let fg = Style::new().fg(theme.foreground);
    let mut line = TippedLine::default();
    line.push(Span::raw(PREFIX));
    for (index, (label, value, width, tip)) in fields.into_iter().enumerate() {
        if index > 0 {
            line.push(Span::styled(" ", fg));
        }
        line.tipped(Span::styled(format!("{label} {value:<width$}"), fg), tip);
    }
    line
}

fn table_tokens(tokens: u64) -> String {
    let mut text = format_tokens_u64(tokens);
    if text.ends_with(['k', 'm']) {
        if !text.contains('.') {
            text.insert_str(text.len() - 1, ".0");
        }
    } else {
        text.push_str(TOKEN_INTEGER_PADDING);
    }
    text
}

fn table_cost(cost: Option<f64>, subscription: bool) -> String {
    match cost {
        Some(cost) => {
            let mark = if subscription { NOT_BILLED_MARK } else { "" };
            format!("{mark}{cost:.3}")
        }
        None => "—".to_owned(),
    }
}

fn column_widths<const N: usize>(rows: &[[String; N]], minimums: [usize; N]) -> [usize; N] {
    rows.iter().fold(minimums, |mut widths, row| {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
        widths
    })
}

fn breakdown_cells(row: &BreakdownRow) -> [String; 6] {
    let usage = &row.usage;
    [
        table_tokens(u64::from(usage.input)),
        table_tokens(u64::from(usage.output)),
        table_tokens(u64::from(usage.cache_read)),
        table_tokens(u64::from(usage.cache_creation)),
        table_tokens(u64::from(usage.total())),
        table_cost(row.spend.usd, row.spend.billing.is_subscription()),
    ]
}

/// Each column's title, explained by what it counts.
fn header_row(
    column: &str,
    label_tip: Option<&'static str>,
    label_w: usize,
    widths: &[usize; 6],
    theme: &crate::theme::Theme,
) -> TippedLine {
    let [input_w, output_w, read_w, write_w, total_w, cost_w] = *widths;
    let dim = theme.status_dim;
    let mut line = TippedLine::default();
    line.push(Span::raw(PREFIX));
    let label = Span::styled(format!("{column:label_w$}"), dim);
    match label_tip {
        Some(tip) => line.tipped(label, tip),
        None => line.push(label),
    };
    for (title, width, tip) in [
        ("base in", input_w, TIP_BASE_IN),
        ("out", output_w, TIP_OUT),
        ("cache read", read_w, TIP_CACHE_READ),
        ("cache write", write_w, TIP_CACHE_WRITE),
        ("total", total_w, TIP_TOTAL),
        ("hit", RATE_COL, TIP_HIT),
        ("cost", cost_w, TIP_COST),
    ] {
        line.push(Span::raw(" ".repeat(COL_GAP)))
            .tipped(Span::styled(format!("{title:>width$}"), dim), tip);
    }
    line
}

fn model_row(
    row: &BreakdownRow,
    cells: &[String; 6],
    widths: &[usize; 6],
    model_w: usize,
    fg: Style,
    dim: Style,
) -> Vec<Span<'static>> {
    let [input, output, read, write, total, cost] = cells;
    let [input_w, output_w, read_w, write_w, total_w, cost_w] = *widths;
    let num = |text: &str, width: usize| Span::styled(format!("{text:>width$}"), fg);
    let gap = || Span::raw(" ".repeat(COL_GAP));
    vec![
        Span::raw(PREFIX),
        Span::styled(format!("{:<model_w$}", row.label), fg),
        gap(),
        num(input, input_w),
        gap(),
        num(output, output_w),
        gap(),
        num(read, read_w),
        gap(),
        num(write, write_w),
        gap(),
        num(total, total_w),
        gap(),
        Span::styled(
            format!("{:>RATE_COL$}", format_hit_rate(row.usage.cache_hit_rate())),
            fg,
        ),
        gap(),
        Span::styled(
            format!("{cost:>cost_w$}"),
            if row.spend.usd.is_some() { fg } else { dim },
        ),
    ]
}

impl crate::components::Overlay for UsageModal {
    fn docs_topic(&self) -> Option<DocsTopic> {
        Some(DocsTopic::Usage)
    }

    /// A title control first, then whatever explained span the pointer rests on.
    fn tooltip(&self) -> Option<Tip> {
        self.title_tip()
            .or_else(|| area_tip(&self.body_tips, self.pointer))
    }

    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

/// `now` decides whether a retry window is still ahead, so the countdown is
/// worked out on every frame rather than frozen when the error arrived.
fn quota_lines(
    state: &UsageFetchState,
    theme: &crate::theme::Theme,
    clock: ClockFormat,
    now: Timestamp,
) -> Content {
    let dim = theme.status_dim;
    let note = |text: String| Line::from(Span::styled(format!("{PREFIX}{text}"), dim));
    let alone = |line: Line<'static>| {
        let mut out = Content::default();
        out.push(line);
        out
    };
    match state {
        UsageFetchState::Loading { last: None } => alone(note(LOADING.into())),
        UsageFetchState::Loading {
            last: Some(fetched),
        } => {
            let mut out = limit_lines(&fetched.usage, theme, clock);
            out.push(note(format!(
                "{REFRESHING}{NOTE_SEPARATOR}{FRESH_FROM}{}",
                fetched_clock(fetched, clock)
            )));
            out
        }
        UsageFetchState::Unsupported => alone(note(NO_USAGE_ENDPOINT.into())),
        UsageFetchState::Ready(fetched) => {
            let mut out = limit_lines(&fetched.usage, theme, clock);
            out.push(aged_note(
                String::new(),
                format!("{FRESH_FROM}{}", fetched_clock(fetched, clock)),
                dim,
            ));
            out
        }
        UsageFetchState::Error {
            message,
            retry_at,
            last,
        } => {
            let mut text = match last {
                Some(_) => format!("{STALE_PREFIX}{message}"),
                None => message.clone(),
            };
            if let Some(wait) = retry_at
                .map(|at| at.as_second() - now.as_second())
                .filter(|secs| *secs > 0)
            {
                text.push_str(&format!("{NOTE_SEPARATOR}retry {}", retry_wait(wait)));
            }
            let Some(fetched) = last else {
                return alone(note(text));
            };
            let mut out = limit_lines(&fetched.usage, theme, clock);
            text.push_str(NOTE_SEPARATOR);
            out.push(aged_note(
                text,
                format!("{STALE_FROM}{}", fetched_clock(fetched, clock)),
                dim,
            ));
            out
        }
    }
}

/// Where each account stands as of its latest reading, which the quota above
/// covers for one of them only.
fn account_lines(accounts: &[AccountStatus], theme: &crate::theme::Theme) -> Content {
    let now_ms = u64::try_from(Timestamp::now().as_millisecond()).unwrap_or_default();
    let fg = Style::new().fg(theme.foreground);
    let width = accounts
        .iter()
        .map(|account| account.name.chars().count())
        .max()
        .unwrap_or_default();
    let mut lines = Content::default();
    lines.push(Line::default());
    lines.push_tipped(section_heading(ACCOUNTS_TITLE, Some(TIP_ACCOUNTS), theme));
    for account in accounts {
        lines.push(Line::from(vec![
            Span::styled(format!("{PREFIX}{:<width$}", account.name), fg),
            Span::styled(
                format!("  {}", account_detail(account, now_ms)),
                theme.status_dim,
            ),
        ]));
    }
    lines
}

/// The figures' age is the line's last span on its own, which is what
/// [`span_hit`] measures and the pointer marks.
fn aged_note(lead: String, age: String, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{PREFIX}{lead}"), style),
        Span::styled(age, style),
    ])
}

fn limit_lines(usage: &ProviderUsage, theme: &crate::theme::Theme, clock: ClockFormat) -> Content {
    let fg = Style::new().fg(theme.foreground);
    let dim = theme.status_dim;
    let mut out = Content::default();
    if let Some(plan) = &usage.plan {
        out.push(Line::from(Span::styled(
            format!("{PREFIX}plan: {plan}"),
            fg,
        )));
    }
    let tz = TimeZone::system();
    let label_w = usage
        .limits
        .iter()
        .map(|l| l.label.chars().count())
        .max()
        .unwrap_or(0);
    for limit in &usage.limits {
        let mut line = TippedLine::default();
        line.push(Span::styled(
            format!("{PREFIX}{:<label_w$}", limit.label),
            fg,
        ));
        if let Some(pct) = limit.percentage {
            line.tipped(Span::styled(format!("{pct:>3}%"), theme.accent), TIP_USED)
                .tipped(Span::styled(" used", dim), TIP_USED);
        }
        if let Some(detail) = &limit.detail {
            line.push(Span::styled(format!("  {detail}"), dim));
        }
        if let Some(ms) = limit.reset_at {
            line.tipped(
                Span::styled(format!("  Resets {}", format_reset(ms, &tz, clock)), dim),
                TIP_RESETS,
            );
        }
        out.push_tipped(line);
    }
    out
}

fn fetched_clock(fetched: &FetchedUsage, clock: ClockFormat) -> String {
    fetched
        .fetched_at
        .to_zoned(TimeZone::system())
        .strftime(crate::clock::hm(clock))
        .to_string()
}

fn retry_wait(seconds: i64) -> String {
    if seconds < MINUTE {
        format!("in {seconds}s")
    } else {
        relative(seconds)
    }
}

pub(crate) fn format_reset(epoch_ms: u64, tz: &TimeZone, clock: ClockFormat) -> String {
    let secs = (epoch_ms / 1000) as i64;
    let Ok(ts) = Timestamp::from_second(secs) else {
        return epoch_ms.to_string();
    };
    let delta = secs - Timestamp::now().as_second();
    if (1..DAY).contains(&delta) {
        return relative(delta);
    }
    let zoned = ts.to_zoned(tz.clone());
    let clock = crate::clock::hm(clock);
    let fmt = if delta < WEEK {
        format!("%a {clock}")
    } else {
        format!("%b %-d, {clock}")
    };
    zoned.strftime(&fmt).to_string()
}

fn relative(seconds: i64) -> String {
    let hrs = seconds / HOUR;
    let mins = (seconds % HOUR) / 60;
    if hrs > 0 {
        format!("in {hrs} hr {mins} min")
    } else {
        format!("in {mins} min")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::tooltip::TipKey;
    use crate::components::{Overlay, buffer_text, test_model};
    use crate::repaint::expect::{OWED, QUIET};
    use caudra_providers::UsageLimit;
    use caudra_storage::cache_warming::{EpisodeOutcome, WarmingTotals};
    use caudra_workbench::scroll::{SCROLLBAR_STEP_FORWARD, SCROLLBAR_THUMB_HORIZONTAL};
    use crossterm::event::KeyModifiers;
    use ratatui::buffer::Buffer;
    use ratatui::style::Modifier;
    use std::sync::Arc;
    use std::time::Duration;
    use test_case::test_case;

    const QUOTA_NOW_SECS: i64 = 1_800_000_000;
    const RETRY_AHEAD_SECS: i64 = 240;
    const RETRY_AHEAD_TEXT: &str = "in 4 min";
    const ERROR_TEXT: &str = "rate limited";
    const SESSION_LABEL: &str = "Current session";
    const FROZEN_WAIT: &str = "retry after";
    const STALE_KEPT: &str = "a failed refresh must not throw away the figures it had";
    const AGE_SHOWN: &str = "quota figures always say when they were fetched";
    const AGE_UNREACHABLE: &str = "the quota's age must be where a pointer reaches it";
    const AGE_HOVER_MISSED: &str = "the whole age, and only it, must reverse under the pointer";
    const AGE_PRESS_LEAKED: &str = "a press on the age is the modal's, not the start of a sweep";
    /// After the prefix, seven columns: `start` lands at 9, `end` at 20.
    const AGE_LEAD: &str = "lead · ";
    const AGE_PHRASE: &str = "as of 12:00";
    const AGE_AREA: Rect = Rect::new(10, 5, 30, 4);
    const RECORDED_COST: f64 = 0.123;
    const RECORDED_TEXT: &str = "0.123";
    const SUBSCRIPTION_COST: f64 = 4.567;
    const SUBSCRIPTION_TEXT: &str = "4.567";
    /// [`RECORDED_COST`] and [`SUBSCRIPTION_COST`] at the lifetime view's cents.
    const LIFETIME_RECORDED: &str = "0.12";
    const LIFETIME_COVERED: &str = "4.57";
    const TOTAL_STAYS_BILLED: &str =
        "the headline total is money owed, and a subscription owes none";
    /// 1M input tokens at the test model's $3/1M: what the modal would print if
    /// it re-priced the counters.
    const REPRICED_TEXT: &str = "3.000";
    const ONE_MILLION: u32 = 1_000_000;
    const ONE_MILLION_TEXT: &str = "1m";
    const UNKNOWN_MODEL: &str = "a-model-no-table-has-ever-heard-of";
    const NO_COST_TEXT: &str = "—";
    const ALL_BUCKETS: StoredTokenUsage = StoredTokenUsage {
        input: 101,
        output: 37,
        cache_read: 211,
        cache_creation: 53,
        cost: Some(RECORDED_COST),
        subscription_cost: None,
        no_cost_estimate: false,
    };
    const BUCKET_HEADERS: &str = "base in      out  cache read  cache write    total   hit    cost";
    const BUCKET_VALUES: &str = " 101       37         211           53      402      58%   0.123";
    const FOLDED_BUCKET_VALUES: &str =
        " 124       56         254           70      504      57%   0.246";
    const BASE_INPUT_PREFIX: &str = "  base in 101 ";
    const BASE_INPUT_LABEL: &str = "base in";
    const LIFETIME_MODEL: &str = "anthropic/claude-opus-5";
    const LIFETIME_PROJECT: &str = "/home/dev/caudra";
    const SCOPE_SURVIVES_CLOSE: &str =
        "reopening should not silently change what is being measured";
    const FLOOR_IS_STATED: &str =
        "an unpriced turn makes the total a floor, and the modal must say so";
    const CTRL_G_STILL_SCROLLS: &str =
        "Ctrl+g is scroll-to-top and must not reach the bare-g scope key";
    const FOREIGN_PROVIDER: &str = "openrouter";
    const PROVIDER_HEADING: &str = "Per provider";
    /// 3M cached reads against 1M uncached input.
    const WARM_RATE: &str = "75%";
    const COLD_RATE: &str = "0%";
    /// Two models of 1M input and 1M cached reads each, summed: the fold scores
    /// 2M of 4M prompt tokens.
    const FOLDED_TOKENS: &str = "4.0m";
    const FOLDED_RATE: &str = "50%";
    const UNKNOWN_IS_NOT_ZERO: &str =
        "a provider that reported no prompt tokens has no rate, which is not a rate of zero";
    /// Wide enough that the table below fits inside the modal with room to spare.
    const WIDE_TERMINAL: u16 = 180;
    /// A small screen: the modal fills its floor and the table still runs past it.
    const NARROW_TERMINAL: u16 = 80;
    const TERMINAL_HEIGHT: u16 = 30;
    const PURPOSE_HEADING: &str = "Per purpose";
    const GOAL_PURPOSE: &str = "goal";
    const CHAT_PURPOSE: &str = "chat";
    const DECISION_PURPOSE: &str = "decision";
    const OPENAI_PROVIDER: &str = "openai";
    const OPENAI_DECISION_MODEL: &str = "openai/gpt-6-luna";
    const OPENAI_CHAT_MODEL: &str = "openai/gpt-6-astra";
    const LEGACY_DECISION_MODEL: &str = "decision/gpt-6-luna";
    const LEGACY_DECISION_USAGE: StoredTokenUsage = StoredTokenUsage {
        input: 23,
        output: 19,
        cache_read: 43,
        cache_creation: 17,
        cost: Some(RECORDED_COST),
        subscription_cost: None,
        no_cost_estimate: false,
    };
    const HIDDEN_ROWS: usize = 2;
    const NONE_HIDDEN: &str = "a cap that would hide one row shows it instead";
    const PURPOSES_WHOLE: &str = "purposes are a short fixed list and always show whole";
    const TOGGLE_UNREACHABLE: &str = "the more line must be where a pointer reaches it";
    const EXPANDED_WHOLE: &str = "an expanded section shows every row and offers to collapse";
    const COLLAPSED_AGAIN: &str = "a second click puts the cap back";
    const PURPOSE_ORDER: &str = "the purpose that cost the most comes first";
    const LONG_MODEL: &str = "a-model-with-a-deliberately-long-identifier";
    const PANS_TO_THE_END: usize = 8;
    /// A tap covers half a viewport, so a couple reach the end of any table this
    /// modal draws and the rest are absorbed by the clamp.
    const TAPS_TO_THE_END: usize = 4;
    const RELOAD_HINT: &str = "Ctrl+R reload";
    const TITLE_ACTION_MISSING: &str = "the title row must draw the hint under test";
    const TITLE_CLICK_IGNORED: &str = "a click on a title hint must do what its key does";
    const HOVER_SPILLED: &str = "only the hovered hint's own cells may be reversed";
    const TIP_MISSING: &str = "a title hint must explain what its gloss says";
    const TIP_OFF_HINT: &str = "a pointer off every title hint must raise no tooltip";
    const COST_UNREACHABLE: &str = "panning must bring the cost column into view";
    const HINT_MISPLACED: &str = "the hint must share the title row, not the bar's row";
    const BAR_UNWANTED: &str = "a table that fits must not wear a pan bar";
    const BAR_MISSING: &str = "a table running off the edge must show what reaches it";
    const TOUCH_MARGIN: &str = "only a finger may press the rows beside the bar";
    const TOKEN_SPANS: [usize; 5] = [3, 5, 7, 9, 11];
    const COST_SPAN: usize = 15;
    const COST_DECIMAL_TAIL: usize = 4;
    const TABLE_HEADER: usize = 2;
    const TABLE_BODY: usize = 3;
    const DECIMAL_ALIGNMENT: &str = "numeric columns must share a decimal position";
    const TABLE_COST_CASES: [(Option<f64>, bool, &str); 6] = [
        (Some(120.646), true, "\u{2248}120.646"),
        (Some(219.226), true, "\u{2248}219.226"),
        (Some(0.400), false, "0.400"),
        (None, false, NO_COST_TEXT),
        (Some(0.002), true, "\u{2248}0.002"),
        (Some(999.9996), false, "1000.000"),
    ];

    const RUNNING_MODE: &str = "running";
    const WARMING_ABSENT: &str = "no episode means no warming block";
    const SHADOW_LABELLED: &str = "a shadow row must say it is an estimate";
    const OUTCOMES_LISTED: &str = "each row lists how its episodes ended";
    const UNCALIBRATED_SHOWN: &str = "savings left out of the estimate are named, not dropped";

    fn warming_summary() -> WarmingSummary {
        let totals = |outcomes: &[(EpisodeOutcome, u64)], uncalibrated_cost| WarmingTotals {
            episodes: outcomes.iter().map(|(_, count)| count).sum(),
            warms: 4,
            warm_cost: 0.12,
            saved_cost: 1.5,
            uncalibrated_cost,
            outcomes: outcomes.iter().copied().collect(),
        };
        WarmingSummary::from([
            (
                (EpisodeKind::Real, RUNNING_MODE.to_owned()),
                totals(
                    &[(EpisodeOutcome::Saved, 3), (EpisodeOutcome::Missed, 1)],
                    0.0,
                ),
            ),
            (
                (EpisodeKind::Shadow, RUNNING_MODE.to_owned()),
                totals(&[(EpisodeOutcome::Saved, 2)], 0.4),
            ),
        ])
    }

    #[test_case(None ; "unread")]
    #[test_case(Some(WarmingSummary::new()) ; "empty")]
    fn warming_is_absent_without_episodes(summary: Option<WarmingSummary>) {
        assert!(
            warming_lines(summary.as_ref(), &crate::theme::current())
                .lines
                .is_empty(),
            "{WARMING_ABSENT}"
        );
    }

    #[test]
    fn warming_rows_tell_real_from_estimated() {
        let summary = warming_summary();
        let texts = line_texts(&warming_lines(Some(&summary), &crate::theme::current()).lines);
        let row = |label: &str| {
            texts
                .iter()
                .find(|text| text.trim_start().starts_with(label))
                .unwrap_or_else(|| panic!("missing row {label}: {texts:#?}"))
                .clone()
        };
        assert!(
            row(&format!("{RUNNING_MODE}{SHADOW_SUFFIX}")).contains("—"),
            "{SHADOW_LABELLED}"
        );
        let real = row(&format!("{RUNNING_MODE} "));
        assert!(real.contains("1.380") && real.contains("75%"), "{real}");
        assert!(
            texts.iter().any(|text| text.contains("saved 3 · missed 1")),
            "{OUTCOMES_LISTED}"
        );
        assert!(
            texts
                .iter()
                .any(|text| text.contains(&format!("$0.400{UNCALIBRATED_NOTE}"))),
            "{UNCALIBRATED_SHOWN}"
        );
    }

    #[test_case(0, "0   " ; "zero")]
    #[test_case(999, "999   " ; "raw_integer")]
    #[test_case(3_000, "3.0k" ; "whole_thousands")]
    #[test_case(7_700, "7.7k" ; "fractional_thousands")]
    #[test_case(7_000_000, "7.0m" ; "whole_millions")]
    #[test_case(246_100_000, "246.1m" ; "fractional_millions")]
    #[test_case(999_949, "999.9k" ; "below_suffix_rounding")]
    #[test_case(999_950, "1.0m" ; "suffix_rounding")]
    #[test_case(u32::MAX as u64, "4295.0m" ; "maximum_session_counter")]
    #[test_case(u64::MAX, "18446744073709.6m" ; "maximum_lifetime_counter")]
    fn table_tokens_reserve_fraction_and_suffix(tokens: u64, expected: &str) {
        assert_eq!(table_tokens(tokens), expected);
    }

    #[test_case(false ; "per_model")]
    #[test_case(true ; "per_provider")]
    fn breakdown_decimals_align_across_every_numeric_column(providers: bool) {
        let theme = crate::theme::current();
        let tokens = [3_000, 7_000_000, 4_700, 0, 15_500, u32::MAX];
        let mut rows: Vec<_> = TABLE_COST_CASES
            .iter()
            .zip(tokens)
            .enumerate()
            .map(|(index, (&(cost, subscription, _), tokens))| BreakdownRow {
                label: format!("provider-{index}/model"),
                usage: StoredTokenUsage {
                    input: tokens,
                    output: tokens,
                    cache_read: tokens,
                    cache_creation: tokens,
                    ..StoredTokenUsage::default()
                },
                spend: ModelSpend {
                    usd: cost,
                    billing: Billing::from_oauth(subscription),
                },
            })
            .collect();
        if providers {
            rows = provider_rows(&rows, FOREIGN_PROVIDER);
        }
        let lines = breakdown_table(PROVIDER_HEADING, PROVIDER_COLUMN, &rows, &theme).lines;
        let header = &lines[TABLE_HEADER];
        for line in &lines[TABLE_BODY..] {
            assert_eq!(line.width(), header.width(), "{DECIMAL_ALIGNMENT}");
            for index in TOKEN_SPANS {
                let text = line.spans[index].content.as_ref();
                let decimal = text.find('.').unwrap_or_else(|| text.trim_end().len());
                assert_eq!(text.len(), header.spans[index].width());
                assert_eq!(
                    decimal,
                    text.len() - TOKEN_INTEGER_PADDING.len(),
                    "{DECIMAL_ALIGNMENT}"
                );
            }
            let label = line.spans[1].content.trim();
            let case = TABLE_COST_CASES
                .iter()
                .enumerate()
                .find(|(index, _)| label.starts_with(&format!("provider-{index}")))
                .unwrap()
                .1;
            let cost = line.spans[COST_SPAN].content.as_ref();
            assert_eq!(cost.trim(), case.2);
            assert_eq!(
                line.spans[COST_SPAN].width(),
                header.spans[COST_SPAN].width()
            );
            if case.0.is_some() {
                assert_eq!(
                    cost.find('.'),
                    Some(cost.len() - COST_DECIMAL_TAIL),
                    "{DECIMAL_ALIGNMENT}"
                );
            } else {
                assert_eq!(line.spans[COST_SPAN].style, theme.status_dim);
            }
        }
    }

    #[test_case(false ; "api_cost")]
    #[test_case(true ; "subscription_cost")]
    fn lifetime_columns_expand_for_wide_counts_and_costs(subscription: bool) {
        let theme = crate::theme::current();
        let fg = Style::new().fg(theme.foreground);
        let slices: Vec<_> = [
            (u64::MAX, u64::MAX, 120.646),
            (7_000_000, 1, 0.002),
            (0, 0, 999.9996),
        ]
        .into_iter()
        .map(|(tokens, turns, cost)| UsageSlice {
            label: LIFETIME_MODEL.to_owned(),
            tokens,
            turns,
            cost: if subscription { 0.0 } else { cost },
            subscription_cost: if subscription { cost } else { 0.0 },
            ..UsageSlice::default()
        })
        .collect();
        let lines = slice_rows(&slices, fg, theme.status_dim).lines;
        let expected_costs = if subscription {
            [" \u{2248}120.646", "   \u{2248}0.002", "\u{2248}1000.000"]
        } else {
            [" 120.646", "   0.002", "1000.000"]
        };
        let expected_tokens = [
            "18446744073709.6m",
            "             7.0m",
            "             0   ",
        ];
        let expected_turns = [
            "18446744073709551615",
            "                   1",
            "                   0",
        ];
        for (index, line) in lines.iter().enumerate() {
            assert_eq!(line.width(), lines[0].width(), "{DECIMAL_ALIGNMENT}");
            assert_eq!(line.spans[3].content, expected_tokens[index]);
            assert_eq!(line.spans[7].content, expected_turns[index]);
            assert_eq!(line.spans[9].content, expected_costs[index]);
            assert_eq!(
                line.spans[9].style,
                if subscription { theme.status_dim } else { fg }
            );
        }
    }

    #[test_case(false ; "billed")]
    #[test_case(true ; "mixed_payers")]
    fn lifetime_widths_ignore_hidden_rows(mixed: bool) {
        let theme = crate::theme::current();
        let mut slices = vec![slice(LIFETIME_MODEL, RECORDED_COST); SLICE_LIMIT];
        if mixed {
            slices[0].subscription_cost = SUBSCRIPTION_COST;
        }
        let visible =
            slice_rows(&slices, Style::new().fg(theme.foreground), theme.status_dim).lines;
        let wide = UsageSlice {
            label: LONG_MODEL.to_owned(),
            tokens: u64::MAX,
            turns: u64::MAX,
            cost: 1_000_000.0,
            ..UsageSlice::default()
        };
        slices.extend(vec![wide; HIDDEN_ROWS]);
        let usage = LifetimeUsage {
            by_model: slices,
            ..lifetime()
        };

        let (Content { lines, .. }, toggles) =
            build_lifetime_lines(Some(&usage), &HashSet::new(), &theme);

        let [(toggle, LifetimeSection::Model)] = toggles[..] else {
            panic!("only the model breakdown is long enough to cap: {toggles:?}");
        };
        assert_eq!(&lines[toggle - SLICE_LIMIT..toggle], visible);
        assert_eq!(
            line_texts(&lines[toggle..=toggle]),
            [format!("{PREFIX}... and {HIDDEN_ROWS} more")]
        );
        if mixed {
            assert_eq!(visible[0].spans[9].content.trim(), "4.690");
        }
    }

    /// Where the arrow was painted, read back from the frame so the tap aims at
    /// what the reader sees rather than at a second copy of the layout.
    fn arrow_column(rendered: &str, arrow: &str) -> u16 {
        let arrow = arrow.chars().next().expect("an arrow glyph");
        let cell = rendered
            .chars()
            .position(|symbol| symbol == arrow)
            .expect("the pan bar painted its arrow");
        (cell % NARROW_TERMINAL as usize) as u16
    }

    fn slice(label: &str, cost: f64) -> UsageSlice {
        UsageSlice {
            label: label.to_string(),
            cost,
            input: ONE_MILLION as u64 / 2,
            cache_read: ONE_MILLION as u64 / 2,
            tokens: ONE_MILLION as u64,
            turns: 1,
            ..UsageSlice::default()
        }
    }

    fn lifetime() -> LifetimeUsage {
        LifetimeUsage {
            input: ONE_MILLION as u64,
            cost: RECORDED_COST,
            priced_turns: 1,
            by_model: vec![slice(LIFETIME_MODEL, RECORDED_COST)],
            by_project: vec![slice(LIFETIME_PROJECT, RECORDED_COST)],
            ..LifetimeUsage::default()
        }
    }

    fn lifetime_texts(lifetime: Option<&LifetimeUsage>) -> Vec<String> {
        line_texts(
            &build_lifetime_lines(lifetime, &HashSet::new(), &crate::theme::current())
                .0
                .lines,
        )
    }

    fn line_texts(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn the_scope_key_swaps_between_this_session_and_every_session() {
        let mut modal = UsageModal::new();
        modal.toggle();
        assert_eq!(modal.scope(), UsageScope::Session);

        modal.handle_key(SCOPE_KEY.to_key_event());
        assert_eq!(modal.scope(), UsageScope::Lifetime);
        assert!(modal.is_open(), "the scope key is not a close key");

        modal.handle_key(SCOPE_KEY.to_key_event());
        assert_eq!(modal.scope(), UsageScope::Session);
    }

    #[test]
    fn scrolling_to_the_top_is_not_a_scope_change() {
        let mut modal = UsageModal::new();
        modal.toggle();
        modal.handle_key(key::SCROLL_TOP.to_key_event());
        assert_eq!(modal.scope(), UsageScope::Session, "{CTRL_G_STILL_SCROLLS}");
    }

    #[test]
    fn the_chosen_scope_outlives_closing_the_modal() {
        let mut modal = UsageModal::new();
        modal.toggle();
        modal.handle_key(SCOPE_KEY.to_key_event());
        modal.close();
        modal.toggle();
        assert_eq!(
            modal.scope(),
            UsageScope::Lifetime,
            "{SCOPE_SURVIVES_CLOSE}"
        );
    }

    #[test]
    fn a_lifetime_view_names_the_models_and_projects_that_cost_the_most() {
        let texts = lifetime_texts(Some(&lifetime())).join("\n");
        assert!(texts.contains(LIFETIME_MODEL), "{texts}");
        assert!(texts.contains(LIFETIME_PROJECT), "{texts}");
        assert!(texts.contains(RECORDED_TEXT), "{texts}");
    }

    #[test]
    fn unpriced_turns_are_called_out_as_a_floor_on_the_lifetime_total() {
        let usage = LifetimeUsage {
            unpriced_turns: 3,
            ..lifetime()
        };
        let texts = lifetime_texts(Some(&usage)).join("\n");
        assert!(texts.contains("floor"), "{FLOOR_IS_STATED}: {texts}");
    }

    #[test]
    fn ephemeral_spend_is_named_but_only_once_it_exists() {
        let quiet = lifetime_texts(Some(&lifetime())).join("\n");
        assert!(!quiet.contains("ephemeral"), "{quiet}");

        let usage = LifetimeUsage {
            ephemeral_cost: RECORDED_COST,
            ..lifetime()
        };
        let loud = lifetime_texts(Some(&usage)).join("\n");
        assert!(loud.contains("ephemeral"), "{loud}");
    }

    #[test_case(None, LEDGER_UNAVAILABLE ; "an_unreadable_ledger_says_so")]
    #[test_case(Some(LifetimeUsage::default()), NO_LEDGER ; "an_empty_ledger_says_so")]
    fn a_lifetime_view_without_numbers_explains_why(usage: Option<LifetimeUsage>, expected: &str) {
        let texts = lifetime_texts(usage.as_ref()).join("\n");
        assert!(texts.contains(expected), "{texts}");
    }

    /// The lifetime ledger knows the provider outright, so its fold obeys the
    /// same rule as the session's: one row is the all-time line again.
    #[test]
    fn the_lifetime_view_folds_providers_once_there_are_two() {
        let alone = LifetimeUsage {
            by_provider: vec![slice(FOREIGN_PROVIDER, RECORDED_COST)],
            ..lifetime()
        };
        assert!(
            !lifetime_texts(Some(&alone))
                .join("\n")
                .contains(PROVIDER_HEADING),
            "{PROVIDER_HEADING} restates the all-time line when only one provider ever ran"
        );

        let shared = LifetimeUsage {
            by_provider: vec![
                slice(FOREIGN_PROVIDER, RECORDED_COST),
                slice(LIFETIME_MODEL, RECORDED_COST),
            ],
            ..lifetime()
        };
        let texts = lifetime_texts(Some(&shared)).join("\n");
        assert!(texts.contains(PROVIDER_HEADING), "{texts}");
        assert!(texts.contains(FOREIGN_PROVIDER), "{texts}");
    }

    /// Every slice carries its own counters now, so a breakdown row scores its
    /// cache the same way the all-time line above it does.
    #[test]
    fn lifetime_rows_carry_a_cache_rate() {
        let usage = LifetimeUsage {
            input: ONE_MILLION as u64,
            cache_read: ONE_MILLION as u64,
            ..lifetime()
        };
        let texts = lifetime_texts(Some(&usage));

        let all_time = texts
            .iter()
            .find(|text| text.contains("turns"))
            .unwrap_or_else(|| panic!("no all-time line: {texts:?}"));
        assert!(all_time.contains(FOLDED_RATE), "{all_time}");

        let model = texts
            .iter()
            .find(|text| text.contains(LIFETIME_MODEL))
            .unwrap_or_else(|| panic!("no model row: {texts:?}"));
        assert!(model.contains(FOLDED_RATE), "{model}");
    }

    #[test]
    fn a_long_breakdown_is_capped_and_says_what_it_left_out() {
        let usage = LifetimeUsage {
            by_model: (0..SLICE_LIMIT + 2)
                .map(|i| slice(&format!("model-{i}"), i as f64))
                .collect(),
            ..lifetime()
        };
        let texts = lifetime_texts(Some(&usage)).join("\n");
        assert!(texts.contains("and 2 more"), "{texts}");
    }

    fn numbered(prefix: &str, count: usize) -> Vec<UsageSlice> {
        (0..count)
            .map(|index| slice(&format!("{prefix}-{index}"), index as f64))
            .collect()
    }

    #[test]
    fn a_cap_never_hides_a_single_row() {
        let usage = LifetimeUsage {
            by_model: numbered(LIFETIME_MODEL, COLLAPSE_ABOVE),
            ..lifetime()
        };
        let (Content { lines, .. }, toggles) =
            build_lifetime_lines(Some(&usage), &HashSet::new(), &crate::theme::current());
        let texts = line_texts(&lines).join("\n");
        assert!(toggles.is_empty(), "{NONE_HIDDEN}: {texts}");
        for index in 0..COLLAPSE_ABOVE {
            assert!(
                texts.contains(&format!("{LIFETIME_MODEL}-{index}")),
                "{NONE_HIDDEN}: {texts}"
            );
        }
    }

    #[test]
    fn purposes_always_show_whole() {
        let purposes = SLICE_LIMIT + HIDDEN_ROWS;
        let usage = LifetimeUsage {
            by_purpose: numbered(GOAL_PURPOSE, purposes),
            ..lifetime()
        };
        let (Content { lines, .. }, toggles) =
            build_lifetime_lines(Some(&usage), &HashSet::new(), &crate::theme::current());
        let texts = line_texts(&lines).join("\n");
        assert!(toggles.is_empty(), "{PURPOSES_WHOLE}: {texts}");
        assert!(texts.contains(PURPOSE_HEADING), "{texts}");
        for index in 0..purposes {
            assert!(
                texts.contains(&format!("{GOAL_PURPOSE}-{index}")),
                "{PURPOSES_WHOLE}: {texts}"
            );
        }
    }

    fn long_lifetime() -> LifetimeUsage {
        LifetimeUsage {
            by_model: numbered(LIFETIME_MODEL, SLICE_LIMIT + HIDDEN_ROWS),
            ..lifetime()
        }
    }

    fn shown_models(modal: &mut UsageModal, usage: &LifetimeUsage) -> (String, usize) {
        let rendered = buffer_text(&draw_with(
            modal,
            WIDE_TERMINAL,
            &HashMap::new(),
            Some(usage),
        ));
        let shown = (0..SLICE_LIMIT + HIDDEN_ROWS)
            .filter(|index| rendered.contains(&format!("{LIFETIME_MODEL}-{index} ")))
            .count();
        (rendered, shown)
    }

    #[test]
    fn a_click_on_the_more_line_expands_and_collapses_its_section() {
        let usage = long_lifetime();
        let mut modal = UsageModal::new();
        modal.toggle();
        modal.handle_key(SCOPE_KEY.to_key_event());
        let (rendered, shown) = shown_models(&mut modal, &usage);
        assert_eq!(shown, SLICE_LIMIT, "{rendered}");
        let hit = modal.toggles.hit(0);
        assert!(!hit.is_empty(), "{TOGGLE_UNREACHABLE}");

        let press = modal.handle_mouse(&pointer(MouseEventKind::Down(MouseButton::Left), hit));
        assert_eq!(press, UsageMouse::Consumed);
        let release = modal.handle_mouse(&pointer(MouseEventKind::Up(MouseButton::Left), hit));
        assert_eq!(release, UsageMouse::Consumed);
        let (rendered, shown) = shown_models(&mut modal, &usage);
        assert_eq!(
            shown,
            SLICE_LIMIT + HIDDEN_ROWS,
            "{EXPANDED_WHOLE}: {rendered}"
        );
        assert!(rendered.contains(SHOW_LESS), "{EXPANDED_WHOLE}: {rendered}");

        let hit = modal.toggles.hit(0);
        modal.handle_mouse(&pointer(MouseEventKind::Down(MouseButton::Left), hit));
        modal.handle_mouse(&pointer(MouseEventKind::Up(MouseButton::Left), hit));
        let (rendered, shown) = shown_models(&mut modal, &usage);
        assert_eq!(shown, SLICE_LIMIT, "{COLLAPSED_AGAIN}: {rendered}");
    }

    #[test]
    fn the_expand_key_opens_and_closes_every_capped_section() {
        let usage = LifetimeUsage {
            by_project: numbered(LIFETIME_PROJECT, SLICE_LIMIT + HIDDEN_ROWS),
            ..long_lifetime()
        };
        let mut modal = UsageModal::new();
        modal.toggle();
        modal.handle_key(SCOPE_KEY.to_key_event());
        let theme = crate::theme::current();
        shown_models(&mut modal, &usage);

        modal.handle_key(EXPAND_KEY.to_key_event());
        let (Content { lines, .. }, toggles) =
            build_lifetime_lines(Some(&usage), &modal.expanded, &theme);
        assert_eq!(toggles.len(), 2);
        assert!(
            toggles
                .iter()
                .all(|&(row, _)| line_texts(&lines[row..=row])[0].ends_with(SHOW_LESS)),
            "{EXPANDED_WHOLE}"
        );

        modal.handle_key(EXPAND_KEY.to_key_event());
        assert!(modal.expanded.is_empty(), "{COLLAPSED_AGAIN}");
    }

    fn click_title(modal: &mut UsageModal, action: TitleAction) -> UsageMouse {
        let index = modal
            .title_actions
            .iter()
            .position(|&drawn| drawn == action)
            .expect(TITLE_ACTION_MISSING);
        let hit = modal.title.hit(index);
        modal.handle_mouse(&pointer(MouseEventKind::Down(MouseButton::Left), hit));
        modal.handle_mouse(&pointer(MouseEventKind::Up(MouseButton::Left), hit))
    }

    #[test]
    fn a_click_on_each_title_hint_does_what_its_key_does() {
        let usage = long_lifetime();
        let mut modal = UsageModal::new();
        modal.toggle();
        shown_models(&mut modal, &usage);

        assert_eq!(
            click_title(&mut modal, TitleAction::Reload),
            UsageMouse::Reload
        );
        assert_eq!(
            click_title(&mut modal, TitleAction::Scope),
            UsageMouse::Consumed
        );
        assert_eq!(modal.scope(), UsageScope::Lifetime, "{TITLE_CLICK_IGNORED}");

        shown_models(&mut modal, &usage);
        assert_eq!(
            click_title(&mut modal, TitleAction::Expand),
            UsageMouse::Consumed
        );
        assert!(!modal.expanded.is_empty(), "{TITLE_CLICK_IGNORED}");
    }

    #[test_case(TitleAction::Reload, false, false, TIP_RELOAD.into(), key::REFRESH ; "reload")]
    #[test_case(TitleAction::Scope, false, false, TIP_LIFETIME.into(), SCOPE_KEY ; "scope_from_session")]
    #[test_case(TitleAction::Scope, true, false, TIP_SESSION.into(), SCOPE_KEY ; "scope_from_lifetime")]
    #[test_case(TitleAction::Expand, true, false, format!("{TIP_MORE}{SLICE_LIMIT}"), EXPAND_KEY ; "expand_when_collapsed")]
    #[test_case(TitleAction::Expand, true, true, format!("{TIP_LESS}{SLICE_LIMIT}"), EXPAND_KEY ; "collapse_when_expanded")]
    fn a_title_hint_tip_explains_what_its_gloss_says(
        action: TitleAction,
        lifetime: bool,
        expanded: bool,
        meaning: String,
        bind: Bind,
    ) {
        let usage = long_lifetime();
        let mut modal = UsageModal::new();
        modal.toggle();
        shown_models(&mut modal, &usage);
        if lifetime {
            modal.toggle_scope();
            shown_models(&mut modal, &usage);
        }
        if expanded {
            modal.flip_all_sections();
            shown_models(&mut modal, &usage);
        }
        let index = modal
            .title_actions
            .iter()
            .position(|&drawn| drawn == action)
            .expect(TITLE_ACTION_MISSING);
        modal.handle_mouse(&pointer(MouseEventKind::Moved, Rect::new(0, 0, 1, 1)));
        assert_eq!(Overlay::tooltip(&modal), None, "{TIP_OFF_HINT}");

        modal.handle_mouse(&pointer(MouseEventKind::Moved, modal.title.hit(index)));

        let tip = Overlay::tooltip(&modal).expect(TIP_MISSING);
        assert_eq!(tip.key, TipKey::TitleHint(index), "{TIP_MISSING}");
        assert_eq!(tip.text, control_tip(&meaning, bind.label), "{TIP_MISSING}");
    }

    #[test]
    fn the_hovered_title_hint_alone_is_reversed() {
        let mut modal = UsageModal::new();
        modal.toggle();
        render(&mut modal);
        let index = modal
            .title_actions
            .iter()
            .position(|&action| action == TitleAction::Scope)
            .expect(TITLE_ACTION_MISSING);
        let hit = modal.title.hit(index);
        modal.handle_mouse(&pointer(MouseEventKind::Moved, hit));

        let buffer = draw_at(&mut modal, WIDE_TERMINAL, &HashMap::new());

        let reversed: Vec<u16> = (0..buffer.area.width)
            .filter(|&x| buffer[(x, hit.y)].modifier.contains(Modifier::REVERSED))
            .collect();
        assert_eq!(
            reversed,
            (hit.x..hit.right()).collect::<Vec<_>>(),
            "{HOVER_SPILLED}"
        );
    }

    #[test]
    fn the_session_view_breaks_spend_down_by_purpose() {
        let model = test_model();
        let by_model = HashMap::from([(model.spec(), stored(Some(RECORDED_COST)))]);
        let by_purpose = HashMap::from([
            (CHAT_PURPOSE.to_owned(), stored(Some(RECORDED_COST))),
            (
                GOAL_PURPOSE.to_owned(),
                StoredTokenUsage {
                    input: ONE_MILLION / 2,
                    subscription_cost: Some(SUBSCRIPTION_COST),
                    ..StoredTokenUsage::default()
                },
            ),
        ]);
        let ctx = UsageModalContext {
            total: &TokenUsage::default(),
            total_cost: None,
            subscription_cost: None,
            by_model: &by_model,
            by_purpose: &by_purpose,
            model: &model,
            fast: false,
            clock_format: ClockFormat::Hour24,
            lifetime: None,
            accounts: &[],
            warming: None,
        };
        let rows = line_texts(&build_lines(&ctx, None, &crate::theme::current()).0.lines);

        let heading = rows
            .iter()
            .position(|row| row.ends_with(PURPOSE_HEADING))
            .unwrap_or_else(|| panic!("no purpose table: {rows:?}"));
        assert!(rows[heading + 1].trim_start().starts_with(PURPOSE_COLUMN));
        assert!(
            rows[heading + 2].trim_start().starts_with(GOAL_PURPOSE),
            "{PURPOSE_ORDER}: {rows:?}"
        );
        assert!(
            rows[heading + 2].ends_with(&format!("{NOT_BILLED_MARK}{SUBSCRIPTION_TEXT}")),
            "{rows:?}"
        );
        assert!(
            rows[heading + 3].trim_start().starts_with(CHAT_PURPOSE),
            "{PURPOSE_ORDER}: {rows:?}"
        );
        assert!(rows[heading + 3].ends_with(RECORDED_TEXT), "{rows:?}");
    }

    #[test_case(false; "new_session")]
    #[test_case(true; "resumed_session_with_legacy_decisions")]
    fn decision_usage_groups_by_provider_without_rewriting_historical_models(legacy: bool) {
        let model = test_model();
        let ingested_decision = StoredTokenUsage {
            no_cost_estimate: true,
            ..ALL_BUCKETS
        };
        let mut by_model = HashMap::from([
            (OPENAI_DECISION_MODEL.to_owned(), ingested_decision),
            (OPENAI_CHAT_MODEL.to_owned(), ALL_BUCKETS),
        ]);
        let decision_usage = if legacy {
            by_model.insert(LEGACY_DECISION_MODEL.to_owned(), LEGACY_DECISION_USAGE);
            StoredTokenUsage {
                input: 124,
                output: 56,
                cache_read: 254,
                cache_creation: 70,
                cost: Some(2.0 * RECORDED_COST),
                subscription_cost: None,
                no_cost_estimate: true,
            }
        } else {
            ingested_decision
        };
        let by_purpose = HashMap::from([
            (CHAT_PURPOSE.to_owned(), ALL_BUCKETS),
            (DECISION_PURPOSE.to_owned(), decision_usage),
        ]);
        let ctx = UsageModalContext {
            total: &TokenUsage::default(),
            total_cost: None,
            subscription_cost: None,
            by_model: &by_model,
            by_purpose: &by_purpose,
            model: &model,
            fast: false,
            clock_format: ClockFormat::Hour24,
            lifetime: None,
            accounts: &[],
            warming: None,
        };

        let models = model_rows(&ctx);
        assert_eq!(models.len(), if legacy { 3 } else { 2 });
        for row in &models {
            assert_eq!(row.usage, by_model[&row.label]);
            assert_eq!(row.spend.usd, row.usage.cost);
            assert_eq!(row.spend.billing, Billing::Api);
        }

        let providers = provider_rows(&models, &model.provider);
        assert_eq!(providers.len(), if legacy { 2 } else { 1 });
        let openai = providers
            .iter()
            .find(|row| row.label == OPENAI_PROVIDER)
            .unwrap();
        assert_eq!(
            openai.usage,
            StoredTokenUsage {
                input: 202,
                output: 74,
                cache_read: 422,
                cache_creation: 106,
                cost: Some(2.0 * RECORDED_COST),
                subscription_cost: None,
                no_cost_estimate: true,
            }
        );
        assert_eq!(openai.spend.usd, Some(2.0 * RECORDED_COST));
        assert_eq!(openai.spend.billing, Billing::Api);
        let historical = providers.iter().find(|row| row.label == DECISION_PURPOSE);
        assert_eq!(
            historical.map(|row| row.usage),
            legacy.then_some(LEGACY_DECISION_USAGE)
        );

        let purposes = purpose_rows(&by_purpose);
        assert_eq!(purposes.len(), 2);
        let decision = purposes
            .iter()
            .find(|row| row.label == DECISION_PURPOSE)
            .unwrap();
        assert_eq!(decision.usage, decision_usage);
        assert_eq!(decision.spend.usd, decision_usage.cost);
        assert_eq!(decision.spend.billing, Billing::Api);
        let chat = purposes
            .iter()
            .find(|row| row.label == CHAT_PURPOSE)
            .unwrap();
        assert_eq!(chat.usage, ALL_BUCKETS);

        let lines = line_texts(&build_lines(&ctx, None, &crate::theme::current()).0.lines);
        assert_eq!(
            lines.iter().any(|line| line.ends_with(PROVIDER_HEADING)),
            legacy
        );
        assert!(
            lines
                .iter()
                .any(|line| line.trim_start().starts_with(OPENAI_DECISION_MODEL))
        );
        assert_eq!(
            lines
                .iter()
                .any(|line| line.trim_start().starts_with(LEGACY_DECISION_MODEL)),
            legacy
        );
        let heading = lines
            .iter()
            .position(|line| line.ends_with(PURPOSE_HEADING))
            .unwrap();
        let decision_line = lines[heading..]
            .iter()
            .find(|line| line.split_whitespace().next() == Some(DECISION_PURPOSE))
            .unwrap();
        assert!(decision_line.ends_with(if legacy {
            FOLDED_BUCKET_VALUES
        } else {
            BUCKET_VALUES
        }));
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test_case(key(KeyCode::Esc, KeyModifiers::NONE) ; "esc_closes")]
    #[test_case(key(KeyCode::Char('c'), KeyModifiers::CONTROL) ; "ctrl_c_closes")]
    fn handle_key_closes(k: KeyEvent) {
        let mut modal = UsageModal::new();
        modal.toggle();
        assert!(modal.is_open());
        modal.handle_key(k);
        assert!(!modal.is_open());
    }

    #[test]
    fn toggle_open_close() {
        let mut modal = UsageModal::new();
        assert!(!modal.is_open());
        modal.toggle();
        assert!(modal.is_open());
        modal.toggle();
        assert!(!modal.is_open());
    }

    #[test]
    fn handle_key_ignores_arbitrary() {
        let mut modal = UsageModal::new();
        modal.toggle();
        modal.handle_key(key(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(modal.is_open());
    }

    fn quota_usage() -> ProviderUsage {
        ProviderUsage {
            plan: Some("lite".into()),
            limits: vec![
                UsageLimit {
                    label: SESSION_LABEL.into(),
                    percentage: Some(16),
                    reset_at: Some(0),
                    detail: None,
                    window: None,
                    length_secs: None,
                },
                UsageLimit {
                    label: "Usage credits".into(),
                    percentage: Some(4),
                    reset_at: None,
                    detail: Some("$2.33 spent".into()),
                    window: None,
                    length_secs: None,
                },
            ],
        }
    }

    fn fetched() -> FetchedUsage {
        FetchedUsage {
            usage: quota_usage(),
            fetched_at: quota_now(),
        }
    }

    fn quota_now() -> Timestamp {
        Timestamp::from_second(QUOTA_NOW_SECS).unwrap()
    }

    fn rate_limited(retry_after: Option<Duration>) -> AgentError {
        AgentError::Api {
            status: 429,
            message: String::new(),
            retry_after,
        }
    }

    fn lines_text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn quota_text(state: &UsageFetchState, now: Timestamp) -> Vec<String> {
        let theme = crate::theme::current();
        lines_text(&quota_lines(state, &theme, ClockFormat::Hour24, now).lines)
    }

    #[test]
    fn xai_weekly_quota_and_observed_restriction_render_separately() {
        const WEEKLY: &str = "Weekly included credits";
        const OBSERVED: &str = "grok-4.7 free usage (observed)";
        const DETAIL: &str = "639471/600000 tokens; exhausted at 2026-10-28T11:23:17Z; rolling 24-hour window, reset time unknown";
        let usage = ProviderUsage {
            plan: None,
            limits: vec![
                UsageLimit {
                    label: WEEKLY.into(),
                    percentage: Some(76),
                    reset_at: Some(QUOTA_NOW_SECS as u64 * 1000),
                    detail: None,
                    window: None,
                    length_secs: None,
                },
                UsageLimit {
                    label: OBSERVED.into(),
                    percentage: None,
                    reset_at: None,
                    detail: Some(DETAIL.into()),
                    window: None,
                    length_secs: None,
                },
            ],
        };
        let state = UsageFetchState::settled(None, Ok(Some(usage)), quota_now());
        let lines = quota_text(&state, quota_now());
        assert!(lines[0].contains(WEEKLY));
        assert!(lines[0].contains("76% used"));
        assert!(lines[0].contains("Resets"));
        assert!(lines[1].contains(OBSERVED));
        assert!(lines[1].contains(DETAIL));
        assert!(!lines[1].contains("Resets"));
        assert!(!lines[1].contains('%'));
        assert!(lines.iter().all(|line| !line.contains(NO_USAGE_ENDPOINT)));
    }

    #[test]
    fn quota_ready_lines_include_labels_and_percentages() {
        let theme = crate::theme::current();
        let lines = quota_lines(
            &UsageFetchState::Ready(fetched()),
            &theme,
            ClockFormat::Hour24,
            quota_now(),
        )
        .lines;
        assert_eq!(lines.len(), 4);
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.content.contains("plan: lite"))
        );
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|s| s.content.contains("Current session"))
        );
        assert!(lines[1].spans.iter().any(|s| s.content.contains("16%")));
        assert!(lines[1].spans.iter().any(|s| s.content.contains("used")));
        assert!(
            lines[2]
                .spans
                .iter()
                .any(|s| s.content.contains("Usage credits"))
        );
        assert!(lines[2].spans.iter().any(|s| s.content.contains("4%")));
        assert!(
            lines[2]
                .spans
                .iter()
                .any(|s| s.content.contains("$2.33 spent"))
        );
        assert_eq!(
            lines_text(&lines[3..]),
            [format!(
                "{PREFIX}{FRESH_FROM}{}",
                fetched_clock(&fetched(), ClockFormat::Hour24)
            )],
            "{AGE_SHOWN}"
        );
    }

    #[test]
    fn quota_non_terminal_states_render_single_line() {
        let now = quota_now();
        assert_eq!(
            quota_text(&UsageFetchState::Loading { last: None }, now),
            [format!("{PREFIX}{LOADING}")]
        );
        assert_eq!(
            quota_text(&UsageFetchState::Unsupported, now),
            [format!("{PREFIX}{NO_USAGE_ENDPOINT}")]
        );
        let error = UsageFetchState::Error {
            message: ERROR_TEXT.into(),
            retry_at: None,
            last: None,
        };
        assert_eq!(quota_text(&error, now), [format!("{PREFIX}{ERROR_TEXT}")]);
    }

    #[test]
    fn a_refresh_in_flight_keeps_the_last_figures_on_screen() {
        let text = quota_text(
            &UsageFetchState::Loading {
                last: Some(fetched()),
            },
            quota_now(),
        );
        assert_eq!(text.len(), 4, "{text:?}");
        assert!(text[1].contains(SESSION_LABEL), "{text:?}");
        assert_eq!(
            text[3],
            format!(
                "{PREFIX}{REFRESHING}{NOTE_SEPARATOR}{FRESH_FROM}{}",
                fetched_clock(&fetched(), ClockFormat::Hour24)
            ),
            "{AGE_SHOWN}"
        );
    }

    #[test_case(RETRY_AHEAD_SECS, true ; "inside_the_window")]
    #[test_case(-RETRY_AHEAD_SECS, false ; "after_the_window")]
    fn a_failed_refresh_keeps_the_figures_and_counts_the_window_down(offset: i64, counting: bool) {
        let now = quota_now();
        let state = UsageFetchState::Error {
            message: ERROR_TEXT.into(),
            retry_at: Some(Timestamp::from_second(QUOTA_NOW_SECS + offset).unwrap()),
            last: Some(fetched()),
        };
        let text = quota_text(&state, now);
        assert_eq!(text.len(), 4, "{text:?}");
        assert!(text[1].contains(SESSION_LABEL), "{text:?}");
        let note = &text[3];
        assert!(
            note.starts_with(&format!("{PREFIX}{STALE_PREFIX}{ERROR_TEXT}")),
            "{note}"
        );
        assert_eq!(note.contains(RETRY_AHEAD_TEXT), counting, "{note}");
        assert!(note.contains(STALE_FROM), "{note}");
    }

    #[test]
    fn a_first_fetch_that_fails_counts_the_window_down() {
        let state = UsageFetchState::Error {
            message: ERROR_TEXT.into(),
            retry_at: Some(Timestamp::from_second(QUOTA_NOW_SECS + RETRY_AHEAD_SECS).unwrap()),
            last: None,
        };
        assert_eq!(
            quota_text(&state, quota_now()),
            [format!(
                "{PREFIX}{ERROR_TEXT}{NOTE_SEPARATOR}retry {RETRY_AHEAD_TEXT}"
            )]
        );
    }

    #[test_case(Some(Duration::from_secs(RETRY_AHEAD_SECS as u64)), true ; "provider_named_a_window")]
    #[test_case(None, false ; "no_window")]
    fn a_failed_fetch_carries_the_last_answer_and_the_window(
        retry_after: Option<Duration>,
        cooling: bool,
    ) {
        let now = quota_now();
        let state = UsageFetchState::settled(Some(fetched()), Err(rate_limited(retry_after)), now);
        assert!(state.last_good().is_some(), "{STALE_KEPT}");
        let UsageFetchState::Error { message, .. } = &state else {
            panic!("a failed fetch settles as an error");
        };
        assert!(!message.contains(FROZEN_WAIT), "{message}");
        assert_eq!(state.cooling_down(now), cooling);
        let after = Timestamp::from_second(QUOTA_NOW_SECS + RETRY_AHEAD_SECS).unwrap();
        assert!(!state.cooling_down(after), "the window ends when it said");
    }

    #[test]
    fn a_successful_fetch_replaces_the_last_answer() {
        let later = Timestamp::from_second(QUOTA_NOW_SECS + RETRY_AHEAD_SECS).unwrap();
        let state = UsageFetchState::settled(Some(fetched()), Ok(Some(quota_usage())), later);
        assert_eq!(state.last_good().map(|f| f.fetched_at), Some(later));
        assert!(!state.cooling_down(quota_now()));
    }

    #[test]
    fn a_provider_without_an_endpoint_drops_the_last_answer() {
        let state = UsageFetchState::settled(Some(fetched()), Ok(None), quota_now());
        assert!(state.last_good().is_none());
    }

    fn stored(cost: Option<f64>) -> StoredTokenUsage {
        StoredTokenUsage {
            input: ONE_MILLION,
            cost,
            ..Default::default()
        }
    }

    fn modal_rows(
        total: &TokenUsage,
        total_cost: Option<f64>,
        by_model: &HashMap<String, StoredTokenUsage>,
        model: &Model,
    ) -> Vec<String> {
        modal_rows_with_subscription(total, total_cost, None, by_model, model)
    }

    fn modal_rows_with_subscription(
        total: &TokenUsage,
        total_cost: Option<f64>,
        subscription_cost: Option<f64>,
        by_model: &HashMap<String, StoredTokenUsage>,
        model: &Model,
    ) -> Vec<String> {
        let ctx = UsageModalContext {
            total,
            total_cost,
            subscription_cost,
            by_model,
            by_purpose: &HashMap::new(),
            model,
            fast: false,
            clock_format: ClockFormat::Hour24,
            lifetime: None,
            accounts: &[],
            warming: None,
        };
        line_texts(&build_lines(&ctx, None, &crate::theme::current()).0.lines)
    }

    #[test_case(false; "single_provider")]
    #[test_case(true; "multiple_providers")]
    fn session_breakdowns_show_all_token_buckets(include_foreign: bool) {
        let model = test_model();
        let mut by_model = HashMap::from([
            (format!("{}/{}", model.provider, model.id), ALL_BUCKETS),
            (
                format!("{}/second", model.provider),
                StoredTokenUsage {
                    input: 23,
                    output: 19,
                    cache_read: 43,
                    cache_creation: 17,
                    ..ALL_BUCKETS
                },
            ),
        ]);
        if include_foreign {
            by_model.insert(format!("{FOREIGN_PROVIDER}/other"), ALL_BUCKETS);
        }

        let rows = modal_rows(&TokenUsage::default(), None, &by_model, &model);
        let header = |column: &str| format!("{PREFIX}{column:<MODEL_COL_MIN$}  {BUCKET_HEADERS}");
        let own_model = format!("{PREFIX}{:<MODEL_COL_MIN$}  {BUCKET_VALUES}", model.id);
        assert!(rows.contains(&own_model), "{rows:?}");
        assert!(rows.contains(&header(MODEL_COLUMN)), "{rows:?}");
        assert_eq!(
            rows.contains(&header(PROVIDER_COLUMN)),
            include_foreign,
            "{rows:?}"
        );
        assert_eq!(
            rows.iter().any(|row| row.contains(PROVIDER_HEADING)),
            include_foreign,
        );
        if include_foreign {
            let own_provider = format!(
                "{PREFIX}{:<MODEL_COL_MIN$}  {FOLDED_BUCKET_VALUES}",
                model.provider,
            );
            let foreign_provider =
                format!("{PREFIX}{FOREIGN_PROVIDER:<MODEL_COL_MIN$}  {BUCKET_VALUES}");
            assert!(rows.contains(&own_provider), "{rows:?}");
            assert!(rows.contains(&foreign_provider), "{rows:?}");
        }
    }

    #[test_case(UsageScope::Session; "session")]
    #[test_case(UsageScope::Lifetime; "lifetime")]
    fn totals_name_base_input(scope: UsageScope) {
        let theme = crate::theme::current();
        let total = TokenUsage::from(ALL_BUCKETS);
        let rows = match scope {
            UsageScope::Session => line_texts(&[Line::from(totals_row(&total, &theme).spans)]),
            UsageScope::Lifetime => lifetime_texts(Some(&LifetimeUsage {
                input: u64::from(total.input),
                output: u64::from(total.output),
                cache_read: u64::from(total.cache_read),
                cache_creation: u64::from(total.cache_creation),
                ..lifetime()
            })),
        };
        assert!(
            rows.iter().any(|row| row.starts_with(BASE_INPUT_PREFIX)),
            "{rows:?}"
        );
    }

    /// A recorded cost is what the turn was billed, and re-pricing its tokens
    /// restates the bill every time a provider moves its rates (DeepSeek moves
    /// them twice a day). A model the tables cannot resolve shows nothing,
    /// since charging it the selected model's rates invents a bill.
    #[test]
    fn model_rows_show_what_was_recorded_and_never_todays_price() {
        let model = test_model();
        let total = TokenUsage {
            input: 2 * ONE_MILLION,
            ..Default::default()
        };
        let by_model = HashMap::from([
            (model.id.clone(), stored(Some(RECORDED_COST))),
            (UNKNOWN_MODEL.to_string(), stored(None)),
        ]);

        let rows = modal_rows(&total, Some(RECORDED_COST), &by_model, &model);
        let row = |id: &str| {
            rows.iter()
                .find(|t| t.contains(id))
                .unwrap_or_else(|| panic!("no row for {id}: {rows:?}"))
                .clone()
        };

        let recorded_row = row(&model.id);
        assert!(recorded_row.contains(RECORDED_TEXT), "{recorded_row}");
        assert!(!recorded_row.contains(REPRICED_TEXT), "{recorded_row}");

        let unknown_row = row(UNKNOWN_MODEL);
        assert!(unknown_row.contains(NO_COST_TEXT), "{unknown_row}");
        assert!(!unknown_row.contains(REPRICED_TEXT), "{unknown_row}");
    }

    fn cached(cache_read: u32, cache_creation: u32) -> StoredTokenUsage {
        StoredTokenUsage {
            input: ONE_MILLION,
            cache_read,
            cache_creation,
            ..Default::default()
        }
    }

    /// The rate is per row, so a warm model and a cold one are told apart even
    /// though the session total averages them together.
    #[test]
    fn each_row_scores_its_own_cache() {
        let model = test_model();
        let by_model = HashMap::from([
            (
                format!("{}/warm", model.provider),
                cached(3 * ONE_MILLION, 0),
            ),
            (format!("{}/cold", model.provider), cached(0, 0)),
        ]);

        let rows = modal_rows(&TokenUsage::default(), None, &by_model, &model);
        let row = |id: &str| {
            rows.iter()
                .find(|text| text.contains(id))
                .unwrap_or_else(|| panic!("no row for {id}: {rows:?}"))
                .clone()
        };

        assert!(row("warm").contains(WARM_RATE), "{:?}", row("warm"));
        assert!(row("cold").contains(COLD_RATE), "{:?}", row("cold"));
    }

    /// A provider that reports no prompt tokens at all has no rate to report,
    /// which is not the same claim as a rate of zero.
    #[test]
    fn a_row_with_nothing_cacheable_reports_no_rate() {
        let model = test_model();
        let by_model = HashMap::from([(
            format!("{}/output-only", model.provider),
            StoredTokenUsage {
                output: ONE_MILLION,
                ..Default::default()
            },
        )]);

        let rows = modal_rows(&TokenUsage::default(), None, &by_model, &model);
        let row = rows
            .iter()
            .find(|text| text.contains("output-only"))
            .unwrap_or_else(|| panic!("no row: {rows:?}"));
        assert!(!row.contains(COLD_RATE), "{UNKNOWN_IS_NOT_ZERO}: {row}");
    }

    /// The prefix is noise while one provider serves everything, and the only
    /// thing telling two rows apart once a second one does.
    #[test]
    fn a_model_row_drops_the_prefix_of_the_sessions_own_provider() {
        let model = test_model();
        let by_model = HashMap::from([
            (format!("{}/{}", model.provider, model.id), stored(None)),
            (format!("{FOREIGN_PROVIDER}/{UNKNOWN_MODEL}"), stored(None)),
        ]);

        let rows = modal_rows(&TokenUsage::default(), None, &by_model, &model).join("\n");

        assert!(
            !rows.contains(&format!("{}/{}", model.provider, model.id)),
            "{rows}"
        );
        assert!(rows.contains(&model.id), "{rows}");
        assert!(
            rows.contains(&format!("{FOREIGN_PROVIDER}/{UNKNOWN_MODEL}")),
            "{rows}"
        );
    }

    /// A lone provider row would only repeat the session total, so the fold
    /// shows up exactly when it has something to say, and sums its members.
    #[test]
    fn the_provider_fold_appears_once_a_second_provider_serves_the_session() {
        let model = test_model();
        let mut by_model = HashMap::from([
            (format!("{}/one", model.provider), cached(ONE_MILLION, 0)),
            (format!("{}/two", model.provider), cached(ONE_MILLION, 0)),
        ]);

        let alone = modal_rows(&TokenUsage::default(), None, &by_model, &model).join("\n");
        assert!(!alone.contains(PROVIDER_HEADING), "{alone}");

        by_model.insert(
            format!("{FOREIGN_PROVIDER}/{UNKNOWN_MODEL}"),
            cached(0, ONE_MILLION),
        );
        let shared = modal_rows(&TokenUsage::default(), None, &by_model, &model);
        let heading = shared
            .iter()
            .position(|text| text.contains(PROVIDER_HEADING))
            .unwrap_or_else(|| panic!("no provider fold: {shared:?}"));

        // The fold scores the provider over both its models, not over whichever
        // row it happened to see first.
        let own = shared[heading..]
            .iter()
            .find(|text| text.contains(&*model.provider))
            .unwrap_or_else(|| panic!("no row for the session's provider: {shared:?}"));
        assert!(own.contains(FOLDED_TOKENS), "{own}");
        assert!(own.contains(FOLDED_RATE), "{own}");
    }

    #[test_case(Some(RECORDED_COST), None,                    Some(format!("  ${RECORDED_TEXT}"))                                ; "billed_only")]
    #[test_case(None,                Some(SUBSCRIPTION_COST), Some(format!("  {NOT_BILLED_MARK}${SUBSCRIPTION_TEXT}"))                           ; "covered_only")]
    #[test_case(Some(RECORDED_COST), Some(SUBSCRIPTION_COST), Some(format!("  ${RECORDED_TEXT}")) ; "both_stacked")]
    #[test_case(Some(RECORDED_COST), Some(0.0),               Some(format!("  ${RECORDED_TEXT}"))                                ; "nothing_covered_is_not_drawn")]
    #[test_case(None,                None,                    None                                                               ; "unpriced")]
    fn the_session_headline_shows_each_payer(
        billed: Option<f64>,
        covered: Option<f64>,
        money: Option<String>,
    ) {
        let total = TokenUsage {
            input: ONE_MILLION,
            ..Default::default()
        };
        let rows =
            modal_rows_with_subscription(&total, billed, covered, &HashMap::new(), &test_model());
        let totals = rows
            .iter()
            .find(|t| t.contains(ONE_MILLION_TEXT))
            .unwrap_or_else(|| panic!("no totals row: {rows:?}"));

        let drawn = totals
            .find("  $")
            .or_else(|| totals.find(&format!("  {NOT_BILLED_MARK}")))
            .map(|at| &totals[at..]);
        assert_eq!(
            drawn.map(str::trim),
            money.as_deref().map(str::trim),
            "{TOTAL_STAYS_BILLED}"
        );
        assert_eq!(
            rows.iter().any(|t| t.contains(SUBSCRIPTION_LEGEND)),
            covered_spend(covered).is_some(),
            "{rows:?}"
        );
    }

    #[test_case(0.0,               None                                                    ; "billed_only")]
    #[test_case(SUBSCRIPTION_COST, Some(format!("{NOT_BILLED_MARK}${LIFETIME_COVERED}")) ; "both_stacked")]
    fn the_lifetime_headline_shows_each_payer(covered: f64, both: Option<String>) {
        let texts = lifetime_texts(Some(&LifetimeUsage {
            subscription_cost: covered,
            ..lifetime()
        }));
        let totals = texts
            .iter()
            .find(|t| t.contains(BASE_INPUT_LABEL))
            .unwrap_or_else(|| panic!("no totals row: {texts:?}"));

        assert!(
            totals
                .trim_end()
                .ends_with(&format!("${LIFETIME_RECORDED}")),
            "{totals}"
        );
        match both {
            Some(both) => {
                let index = texts.iter().position(|text| text == totals).unwrap();
                assert!(texts[index + 1].trim_end().ends_with(&both));
            }
            None => assert!(!totals.contains(NOT_BILLED_MARK), "{totals}"),
        }
        assert_eq!(
            texts.iter().any(|t| t.contains(SUBSCRIPTION_LEGEND)),
            covered > 0.0,
            "{texts:?}"
        );
    }

    #[test_case(UsageScope::Session, 0.013, 15.352 ; "session_subscription_wider")]
    #[test_case(UsageScope::Lifetime, 0.013, 15.352 ; "lifetime_subscription_wider")]
    #[test_case(UsageScope::Session, 153.013, 1.352 ; "session_bill_wider")]
    #[test_case(UsageScope::Lifetime, 153.013, 1.352 ; "lifetime_bill_wider")]
    #[test_case(UsageScope::Session, 0.0, 9.9996 ; "session_rounding_carry")]
    #[test_case(UsageScope::Lifetime, 0.0, 9.9996 ; "lifetime_rounding_carry")]
    fn mixed_totals_align_decimal_points(scope: UsageScope, billed: f64, covered: f64) {
        let (rows, decimals) = match scope {
            UsageScope::Session => (
                modal_rows_with_subscription(
                    &TokenUsage::default(),
                    Some(billed),
                    Some(covered),
                    &HashMap::new(),
                    &test_model(),
                ),
                SESSION_DECIMALS,
            ),
            UsageScope::Lifetime => (
                lifetime_texts(Some(&LifetimeUsage {
                    cost: billed,
                    subscription_cost: covered,
                    ..lifetime()
                })),
                LIFETIME_DECIMALS,
            ),
        };
        let index = rows
            .iter()
            .position(|row| row.contains(BASE_INPUT_LABEL))
            .unwrap();
        let top = &rows[index];
        let bottom = &rows[index + 1];
        assert!(
            top.trim_end().ends_with(&format!("${billed:.decimals$}")),
            "{top}"
        );
        assert!(!top.contains(NOT_BILLED_MARK), "{top}");
        assert!(
            bottom
                .trim_end()
                .ends_with(&format!("{NOT_BILLED_MARK}${covered:.decimals$}")),
            "{bottom}"
        );
        assert!(bottom.contains(SUBSCRIPTION_LEGEND), "{bottom}");
        assert_eq!(
            rows.iter()
                .filter(|row| row.contains(SUBSCRIPTION_LEGEND))
                .count(),
            1
        );
        let decimal_column = |row: &str| row.rsplit_once('.').unwrap().0.chars().count();
        assert_eq!(
            decimal_column(top),
            decimal_column(bottom),
            "{top}\n{bottom}"
        );
    }

    /// The session's bill arrives already computed, from the turns that paid it.
    /// These counters would price to [`REPRICED_TEXT`] against the selected
    /// model, so a modal doing its own arithmetic prints a different number,
    /// and "$0.000" for a session nothing priced.
    #[test_case(Some(RECORDED_COST) => Some(RECORDED_TEXT.to_string()) ; "prints_the_bill_it_was_handed")]
    #[test_case(None                => None                            ; "unpriced_session_shows_tokens_only")]
    fn totals_row_never_re_prices_the_counters(total_cost: Option<f64>) -> Option<String> {
        let model = test_model();
        assert!(!model.pricing.is_zero(), "the fallback must be tempting");
        let total = TokenUsage {
            input: ONE_MILLION,
            ..Default::default()
        };

        let rows = modal_rows(&total, total_cost, &HashMap::new(), &model);
        // With no breakdown, the totals row is the only one carrying counters.
        let totals = rows
            .iter()
            .find(|t| t.contains(ONE_MILLION_TEXT))
            .unwrap_or_else(|| panic!("no totals row: {rows:?}"));
        totals
            .split_once('$')
            .map(|(_, cost)| cost.trim().to_string())
    }

    fn slot(state: UsageFetchState) -> ArcSwapOption<UsageFetchState> {
        ArcSwapOption::from_pointee(state)
    }

    fn render(modal: &mut UsageModal) -> String {
        render_at(modal, WIDE_TERMINAL, &HashMap::new())
    }

    fn render_at(
        modal: &mut UsageModal,
        width: u16,
        by_model: &HashMap<String, StoredTokenUsage>,
    ) -> String {
        buffer_text(&draw_at(modal, width, by_model))
    }

    fn draw_at(
        modal: &mut UsageModal,
        width: u16,
        by_model: &HashMap<String, StoredTokenUsage>,
    ) -> Buffer {
        draw_with(modal, width, by_model, None)
    }

    fn draw_with(
        modal: &mut UsageModal,
        width: u16,
        by_model: &HashMap<String, StoredTokenUsage>,
        lifetime: Option<&LifetimeUsage>,
    ) -> Buffer {
        let backend = ratatui::backend::TestBackend::new(width, TERMINAL_HEIGHT);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let model = test_model();
        let ctx = UsageModalContext {
            total: &TokenUsage::default(),
            total_cost: None,
            subscription_cost: None,
            by_model,
            by_purpose: &HashMap::new(),
            model: &model,
            fast: false,
            clock_format: ClockFormat::Hour24,
            lifetime,
            accounts: &[],
            warming: None,
        };
        terminal
            .draw(|f| {
                modal.view(f, f.area(), &ctx);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// A breakdown whose model id is long enough that the cost column cannot fit
    /// beside it on a small screen.
    fn wide_breakdown() -> HashMap<String, StoredTokenUsage> {
        HashMap::from([(LONG_MODEL.to_string(), ALL_BUCKETS)])
    }

    #[test_case(NARROW_TERMINAL, 0, 0; "narrow_clipped")]
    #[test_case(NARROW_TERMINAL, i32::MAX, 41; "narrow_panned")]
    #[test_case(WIDE_TERMINAL, 0, 0; "wide_unpanned")]
    fn token_columns_stay_aligned_when_panned(width: u16, pan: i32, expected_pan: u16) {
        let mut modal = UsageModal::new();
        modal.toggle();
        let breakdown = wide_breakdown();
        render_at(&mut modal, width, &breakdown);
        modal.pan(pan);
        let rendered = render_at(&mut modal, width, &breakdown);
        assert_eq!(modal.scroll.pan(), expected_pan);

        let header = format!(
            "{PREFIX}{:<label_w$}  {BUCKET_HEADERS}",
            "model",
            label_w = LONG_MODEL.len()
        );
        let row = format!("{PREFIX}{LONG_MODEL}  {BUCKET_VALUES}");
        for expected in [header, row] {
            let visible: String = expected
                .chars()
                .skip(usize::from(expected_pan))
                .take(usize::from(modal.popup.width - 2))
                .collect();
            assert!(rendered.contains(&visible), "{visible:?}: {rendered}");
        }
    }

    /// The table is as wide as its longest model id, which owes nothing to the
    /// terminal: on a small screen its right-hand columns are off the modal, and
    /// the only way to read them is to pan.
    #[test]
    fn a_table_wider_than_the_screen_is_reachable_by_panning() {
        let mut modal = UsageModal::new();
        modal.toggle();
        let breakdown = wide_breakdown();

        let clipped = render_at(&mut modal, NARROW_TERMINAL, &breakdown);
        assert!(
            !clipped.contains(RECORDED_TEXT),
            "the cost column should be off the edge to begin with"
        );

        for _ in 0..PANS_TO_THE_END {
            modal.handle_key(key::PAN_RIGHT.to_key_event());
        }
        let panned = render_at(&mut modal, NARROW_TERMINAL, &breakdown);
        assert!(panned.contains(RECORDED_TEXT), "{COST_UNREACHABLE}");
    }

    /// Panning past the end would scroll the table off the left edge and show a
    /// blank modal, so it stops at the widest line whatever the reader does.
    #[test]
    fn panning_never_runs_off_the_end_of_the_widest_line() {
        let mut modal = UsageModal::new();
        modal.toggle();
        let breakdown = wide_breakdown();
        render_at(&mut modal, NARROW_TERMINAL, &breakdown);

        for _ in 0..PANS_TO_THE_END * 4 {
            modal.handle_key(key::PAN_RIGHT.to_key_event());
        }

        assert!(
            render_at(&mut modal, NARROW_TERMINAL, &breakdown).contains(RECORDED_TEXT),
            "{COST_UNREACHABLE}"
        );
    }

    /// The bar takes the bottom border row, so the hint it displaced has to be
    /// somewhere a reader can still see it whether or not the bar is showing.
    #[test_case(NARROW_TERMINAL ; "narrow enough to wear a bar")]
    #[test_case(WIDE_TERMINAL   ; "wide enough to need none")]
    fn the_reload_hint_sits_on_the_title_row(width: u16) {
        let mut modal = UsageModal::new();
        modal.toggle();

        let rendered = render_at(&mut modal, width, &wide_breakdown());
        let title_row = rendered
            .lines()
            .find(|line| line.contains(TITLE.trim()))
            .expect("a drawn modal wears its title");

        assert!(title_row.contains(RELOAD_HINT), "{HINT_MISPLACED}");
    }

    /// A table that fits has nothing to reach, and a bar over a border it does
    /// not need is noise.
    #[test]
    fn a_table_that_fits_wears_no_pan_bar() {
        let mut modal = UsageModal::new();
        modal.toggle();

        let wide = render_at(&mut modal, WIDE_TERMINAL, &wide_breakdown());
        let narrow = render_at(&mut modal, NARROW_TERMINAL, &wide_breakdown());

        assert!(!wide.contains(SCROLLBAR_THUMB_HORIZONTAL), "{BAR_UNWANTED}");
        assert!(narrow.contains(SCROLLBAR_THUMB_HORIZONTAL), "{BAR_MISSING}");
    }

    /// A fingertip covers several rows and reports their centroid, so the one
    /// border row the bar paints is unhittable by touch. The vertical bar is
    /// handed the whole body and gets its margin out of it; the horizontal one
    /// has to get the same margin out of the rows above the border.
    ///
    /// Touch also never reports a held drag, so this tap is the whole gesture:
    /// if it misses, there is no pointer route to the clipped columns at all.
    #[test_case(true,  true  ; "a finger reaches the row above the border")]
    #[test_case(false, false ; "a pointer gets the painted row alone")]
    fn the_pan_bar_takes_a_touch_beside_it(touch: bool, panned: bool) {
        caudra_workbench::scroll::set_touch(touch);
        let mut modal = UsageModal::new();
        modal.toggle();
        let breakdown = wide_breakdown();
        let clipped = render_at(&mut modal, NARROW_TERMINAL, &breakdown);
        modal.handle_mouse(&MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            // The last cell of the seek track, which the arrow beside it keeps
            // out of: pressing an end of the track is what reaches an end of
            // the document.
            column: arrow_column(&clipped, SCROLLBAR_STEP_FORWARD) - 2,
            row: modal.popup.bottom() - 2,
            modifiers: KeyModifiers::NONE,
        });

        let reached = render_at(&mut modal, NARROW_TERMINAL, &breakdown).contains(RECORDED_TEXT);
        caudra_workbench::scroll::set_touch(false);
        assert_eq!(reached, panned, "{TOUCH_MARGIN}");
    }

    /// The reported defect: Termux emits no horizontal wheel code at all, so a
    /// sideways swipe sends nothing and a tap is the only gesture left. It also
    /// cannot be detected through SSH, so the arrow has to be reachable with
    /// touch off — including from the row above the one it is painted on, which
    /// is the whole reason a step zone is two rows tall.
    #[test_case(0 ; "on the arrow itself")]
    #[test_case(1 ; "on the row above it")]
    fn a_tap_on_the_pan_arrow_steps_the_table(rows_up: u16) {
        let mut modal = UsageModal::new();
        modal.toggle();
        let breakdown = wide_breakdown();
        let clipped = render_at(&mut modal, NARROW_TERMINAL, &breakdown);
        assert!(!clipped.contains(RECORDED_TEXT), "{COST_UNREACHABLE}");
        let arrow = arrow_column(&clipped, SCROLLBAR_STEP_FORWARD);

        for _ in 0..TAPS_TO_THE_END {
            modal.handle_mouse(&MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: arrow,
                row: modal.popup.bottom() - 1 - rows_up,
                modifiers: KeyModifiers::NONE,
            });
            render_at(&mut modal, NARROW_TERMINAL, &breakdown);
        }

        assert!(
            render_at(&mut modal, NARROW_TERMINAL, &breakdown).contains(RECORDED_TEXT),
            "{COST_UNREACHABLE}"
        );
    }

    /// A press on the bar seeks, the same way the vertical one does.
    #[test]
    fn a_press_on_the_pan_bar_moves_the_table() {
        let mut modal = UsageModal::new();
        modal.toggle();
        let breakdown = wide_breakdown();
        let clipped = render_at(&mut modal, NARROW_TERMINAL, &breakdown);
        // The bar runs along the bottom border row, inside the corners, and the
        // last cell of a track is the end of the document by definition.
        assert_eq!(
            modal.handle_mouse(&MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: arrow_column(&clipped, SCROLLBAR_STEP_FORWARD) - 2,
                row: modal.popup.bottom() - 1,
                modifiers: KeyModifiers::NONE,
            }),
            UsageMouse::Consumed
        );

        assert!(
            render_at(&mut modal, NARROW_TERMINAL, &breakdown).contains(RECORDED_TEXT),
            "{COST_UNREACHABLE}"
        );
    }

    /// Nothing wakes the loop when the fetch stores its answer, so the modal
    /// has to notice on its own, and exactly once: the slot keeps holding the
    /// same `Arc` for as long as the modal stays open, and a poll that cannot
    /// tell "still there" from "just arrived" repaints on every tick. A closed
    /// modal is not on screen, so it must not pick anything up.
    #[test]
    fn poll_owes_a_frame_only_for_a_value_the_open_modal_has_not_seen() {
        let slot = slot(UsageFetchState::Loading { last: None });
        let mut modal = UsageModal::new();

        assert_eq!(
            modal.poll(&slot),
            Dirty::NO,
            "a closed modal ignores the slot"
        );
        assert!(modal.quota.get().is_none());

        modal.toggle();
        assert_eq!(modal.poll(&slot), Dirty::YES, "{OWED}");
        assert_eq!(modal.poll(&slot), Dirty::NO, "{QUIET}");

        slot.store(Some(Arc::new(UsageFetchState::Unsupported)));
        assert_eq!(
            modal.poll(&slot),
            Dirty::YES,
            "a refetch owes a frame, whatever it holds"
        );
    }

    /// `view` renders what the modal owns, never the shared slot. Reading the
    /// slot mid render is what forced the old loop to paint constantly. Closing
    /// keeps the last answer, so a reopen has something to show while the
    /// refetch is on its way, and owes no frame for what is already drawn.
    #[test]
    fn quota_reaches_the_screen_only_after_a_poll_and_survives_a_reopen() {
        let slot = slot(UsageFetchState::Unsupported);
        let mut modal = UsageModal::new();
        modal.toggle();

        assert!(
            !render(&mut modal).contains(NO_USAGE_ENDPOINT),
            "an unpolled quota must not appear on screen"
        );
        assert_eq!(modal.poll(&slot), Dirty::YES, "{OWED}");
        assert!(render(&mut modal).contains(NO_USAGE_ENDPOINT));

        modal.close();
        modal.toggle();
        assert_eq!(modal.poll(&slot), Dirty::NO, "{QUIET}");
        assert!(
            render(&mut modal).contains(NO_USAGE_ENDPOINT),
            "a reopened modal still shows the last answer it saw"
        );
    }

    fn pointer(kind: MouseEventKind, at: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: at.x,
            row: at.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn stale(retry_at: Option<Timestamp>) -> UsageFetchState {
        UsageFetchState::Error {
            message: ERROR_TEXT.into(),
            retry_at,
            last: Some(fetched()),
        }
    }

    fn opened_on(state: UsageFetchState) -> UsageModal {
        let mut modal = UsageModal::new();
        modal.toggle();
        assert_eq!(modal.poll(&slot(state)), Dirty::YES, "{OWED}");
        modal
    }

    /// The age marks as one phrase under the pointer, and a press and release
    /// on it asks for a refresh, even inside the retry window: that window is
    /// on screen beside it, so the click is a decision, not an accident.
    #[test_case(UsageFetchState::Ready(fetched()) ; "fresh")]
    #[test_case(stale(Some(Timestamp::MAX)) ; "stale_inside_the_window")]
    fn a_click_on_the_quota_age_asks_for_a_refresh(state: UsageFetchState) {
        let mut modal = opened_on(state);
        draw_at(&mut modal, WIDE_TERMINAL, &HashMap::new());
        let hit = modal.refresh.hit(0);
        assert!(!hit.is_empty(), "{AGE_UNREACHABLE}");

        modal.handle_mouse(&pointer(MouseEventKind::Moved, hit));
        let buffer = draw_at(&mut modal, WIDE_TERMINAL, &HashMap::new());
        let reversed: Vec<Position> = buffer
            .area
            .positions()
            .filter(|at| buffer[*at].modifier.contains(Modifier::REVERSED))
            .collect();
        assert!(
            reversed.len() == usize::from(hit.width) && reversed.iter().all(|at| hit.contains(*at)),
            "{AGE_HOVER_MISSED}: hit={hit:?} reversed={reversed:?}"
        );

        let press = modal.handle_mouse(&pointer(MouseEventKind::Down(MouseButton::Left), hit));
        assert_eq!(press, UsageMouse::Consumed, "{AGE_PRESS_LEAKED}");
        let release = modal.handle_mouse(&pointer(MouseEventKind::Up(MouseButton::Left), hit));
        assert_eq!(release, UsageMouse::Refresh);
    }

    #[test_case(UsageFetchState::Loading { last: Some(fetched()) } ; "already_refreshing")]
    #[test_case(UsageFetchState::Error { message: ERROR_TEXT.into(), retry_at: None, last: None } ; "nothing_fetched_yet")]
    #[test_case(UsageFetchState::Unsupported ; "no_endpoint")]
    fn only_figures_that_can_be_refreshed_offer_a_click(state: UsageFetchState) {
        let mut modal = opened_on(state);
        draw_at(&mut modal, WIDE_TERMINAL, &HashMap::new());
        assert!(modal.refresh.hit(0).is_empty());
    }

    #[test_case(1, 0, 0, AGE_AREA, Some(Rect::new(19, 6, 11, 1)) ; "whole_phrase")]
    #[test_case(1, 0, 12, AGE_AREA, Some(Rect::new(10, 6, 8, 1)) ; "panned_into")]
    #[test_case(1, 0, 20, AGE_AREA, None ; "panned_past")]
    #[test_case(1, 2, 0, AGE_AREA, None ; "scrolled_above")]
    #[test_case(5, 0, 0, AGE_AREA, None ; "below_the_area")]
    #[test_case(1, 0, 0, Rect::new(10, 5, 15, 4), Some(Rect::new(19, 6, 6, 1)) ; "cut_by_the_edge")]
    fn the_age_hit_covers_only_what_is_drawn(
        row: usize,
        scroll: u16,
        pan: u16,
        area: Rect,
        expected: Option<Rect>,
    ) {
        let line = aged_note(AGE_LEAD.into(), AGE_PHRASE.into(), Style::new());
        assert_eq!(
            span_hit(&line, line.spans.len() - 1, row, area, scroll, pan),
            expected
        );
    }

    /// A span in the middle of a line starts after every span before it, and
    /// panning clips it like the last one.
    #[test_case(0, Some(Rect::new(17, 6, 11, 1)) ; "unpanned")]
    #[test_case(10, Some(Rect::new(10, 6, 8, 1)) ; "panned_into")]
    #[test_case(18, None ; "panned_past")]
    fn a_middle_span_hit_starts_after_the_spans_before_it(pan: u16, expected: Option<Rect>) {
        let line = Line::from(vec![
            Span::raw(AGE_LEAD),
            Span::raw(AGE_PHRASE),
            Span::raw(AGE_LEAD),
        ]);
        assert_eq!(span_hit(&line, 1, 1, AGE_AREA, 0, pan), expected);
    }

    const TIP_OFF_GLYPHS: &str = "a tip must cover the glyphs it explains";
    const BODY_TIP_MISSING: &str = "the hovered span must raise its tip";

    fn hover(modal: &mut UsageModal, at: Rect) -> Option<Tip> {
        modal.handle_mouse(&pointer(MouseEventKind::Moved, at));
        Overlay::tooltip(modal)
    }

    fn drawn_text(buffer: &Buffer, area: Rect) -> String {
        area.positions().map(|at| buffer[at].symbol()).collect()
    }

    /// Every column and figure answers the pointer, and each answer sits on
    /// the very cells that name it, in the header and in the headline alike.
    #[test_case(TIP_BASE_IN, BASE_INPUT_LABEL ; "base_input")]
    #[test_case(TIP_CACHE_READ, "cache read" ; "cache_read")]
    #[test_case(TIP_HIT, "hit" ; "hit")]
    #[test_case(TIP_COST, "cost" ; "cost")]
    fn a_column_explains_itself_where_it_is_drawn(tip: &str, label: &str) {
        let mut modal = UsageModal::new();
        modal.toggle();
        let buffer = draw_at(&mut modal, WIDE_TERMINAL, &wide_breakdown());
        let areas: Vec<Rect> = modal
            .body_tips
            .iter()
            .filter(|area| area.text == tip)
            .map(|area| area.area)
            .collect();

        assert!(!areas.is_empty(), "{BODY_TIP_MISSING}");
        for area in &areas {
            let text = drawn_text(&buffer, *area);
            assert!(text.trim().starts_with(label), "{TIP_OFF_GLYPHS}: {text:?}");
        }
        let shown = hover(&mut modal, areas[0]).expect(BODY_TIP_MISSING);
        assert_eq!(shown.text, tip);
    }

    #[test]
    fn the_quota_age_says_a_click_asks_again() {
        let mut modal = opened_on(UsageFetchState::Ready(fetched()));
        draw_at(&mut modal, WIDE_TERMINAL, &HashMap::new());
        let age = modal.refresh.hit(0);

        let tip = hover(&mut modal, age).expect(BODY_TIP_MISSING);

        assert_eq!(tip.text, TIP_REFRESH_AGE);
    }

    #[test]
    fn a_more_line_says_what_its_click_shows() {
        let usage = long_lifetime();
        let mut modal = UsageModal::new();
        modal.toggle();
        modal.handle_key(SCOPE_KEY.to_key_event());
        shown_models(&mut modal, &usage);
        let more = modal.toggles.hit(0);

        let tip = hover(&mut modal, more).expect(BODY_TIP_MISSING);

        assert!(tip.text.starts_with(TIP_SHOW_ALL), "{}", tip.text);
        assert!(tip.text.ends_with(TIP_TOGGLE_EVERY), "{}", tip.text);
    }

    /// The headline is drawn one span per figure so each can explain itself,
    /// and must still read exactly as the one formatted line it replaced.
    #[test]
    fn a_split_headline_reads_as_one_formatted_line() {
        let theme = crate::theme::current();
        let total = TokenUsage::from(ALL_BUCKETS);
        let whole = format!(
            "{PREFIX}base in {:<7} out {:<7} cache read {:<7} cache write {:<7} total {:<7} hit {:<4}",
            format_tokens(total.input),
            format_tokens(total.output),
            format_tokens(total.cache_read),
            format_tokens(total.cache_creation),
            format_tokens(total.context_tokens()),
            format_hit_rate(total.cache_hit_rate()),
        );

        let split = line_texts(&[Line::from(totals_row(&total, &theme).spans)]);

        assert_eq!(split, [whole]);
    }

    #[test]
    fn relative_formats_future_windows() {
        assert_eq!(relative(30), "in 0 min");
        assert_eq!(relative(120), "in 2 min");
        assert_eq!(relative(3 * HOUR + 36 * 60), "in 3 hr 36 min");
        assert_eq!(relative(5 * HOUR), "in 5 hr 0 min");
    }
}
