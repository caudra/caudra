use std::cmp::Reverse;
use std::collections::HashMap;

use arc_swap::ArcSwapOption;

use caudra_config::ClockFormat;
use caudra_grab::grab_scope;
use caudra_providers::{
    Billing, Model, ModelSpend, ProviderUsage, TokenUsage, add_cost, format_hit_rate,
    format_tokens, format_tokens_u64, model_cost,
};
use caudra_storage::sessions::StoredTokenUsage;
use caudra_storage::usage_ledger::{LifetimeUsage, UsageSlice};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::components::keybindings::{Bind, key};
use crate::components::modal::Modal;
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::{ModalScroll, bar_area};
use crate::repaint::{Dirty, Watch};
use crate::theme;

const TITLE: &str = " Token usage ";
const LIFETIME_TITLE: &str = " Token usage - lifetime ";
const SLICE_LIMIT: usize = 8;
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
/// What a subscription figure is: the API list price for the same tokens, with
/// no invoice behind it.
const SUBSCRIPTION_LINE: &str = "subscription (not billed)";
const NOT_BILLED_MARK: &str = "~";
const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;
const WEEK: i64 = 7 * DAY;

/// Live provider quota fetch, shared from the event loop. A detached task
/// drops the answer into the slot, and [`UsageModal::poll`] is what notices.
pub enum UsageFetchState {
    Loading,
    Ready(ProviderUsage),
    Unsupported,
    Error(String),
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
    pub model: &'a Model,
    pub fast: bool,
    pub clock_format: ClockFormat,
    /// `None` when the ledger could not be read, which is different from an
    /// empty ledger and says so.
    pub lifetime: Option<&'a LifetimeUsage>,
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
    popup: Rect,
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
        self.scroll.reset();
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
        self.scroll.reset();
    }

    /// The modal reads nothing else from the pointer, so the two bars are all
    /// there is to offer and a bool is all there is to say. They sit on different
    /// rows, so at most one of them answers a press.
    pub fn handle_mouse(&mut self, event: &MouseEvent) -> bool {
        match self.scrollbar.handle(event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return true,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                return true;
            }
        }
        match self.pan_bar.handle(event) {
            ScrollbarMouse::Ignored => false,
            ScrollbarMouse::Consumed => true,
            ScrollbarMouse::ScrollTo(column) => {
                self.scroll.pan_to(column as u16);
                true
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
        self.scroll.handle_key(key_event);
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect, ctx: &UsageModalContext) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("usage_modal", area);

        let theme = theme::current();
        let lines = match self.scope {
            UsageScope::Session => build_lines(ctx, self.quota.get(), &theme),
            UsageScope::Lifetime => build_lifetime_lines(ctx.lifetime, &theme),
        };

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

        frame.render_widget(Paragraph::new(lines).scroll((scroll, pan)), inner);

        self.scrollbar.draw(frame, inner, total, scroll);
        // Handed the table and its bottom border, the way the bar above is handed
        // the whole body: it paints on the last row, which is the only row it can
        // have without taking one from the table, and a fingertip gets its margin
        // out of the rows over it. Nothing is painted while the table fits,
        // because a track is only built for content that overflows.
        self.pan_bar.draw(frame, bar_area(inner), content_w, pan);

        let hint = Line::from(vec![
            Span::raw(" "),
            Span::styled("Ctrl+R", theme.keybind_key),
            Span::styled(" reload ", theme.tool_dim),
            Span::styled(SCOPE_KEY.label, theme.keybind_key),
            Span::styled(
                match self.scope {
                    UsageScope::Session => " lifetime ",
                    UsageScope::Lifetime => " session ",
                },
                theme.tool_dim,
            ),
        ]);
        let hint_w = hint.width() as u16;
        // Beside the title rather than under the table: the bottom border row
        // belongs to the pan bar, and a hint sharing it would be overpainted
        // exactly when the modal is narrow enough to need both.
        let hint_area = Rect {
            x: popup.x + popup.width.saturating_sub(hint_w + 1),
            y: popup.y,
            width: hint_w,
            height: 1,
        };
        frame.render_widget(Paragraph::new(hint), hint_area);

        self.popup = popup;
        popup
    }
}

fn build_lines(
    ctx: &UsageModalContext,
    quota: Option<&UsageFetchState>,
    theme: &crate::theme::Theme,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(Span::styled(
        format!("{PREFIX}Session total"),
        theme.keybind_section,
    )));

    lines.push(Line::from(totals_row(ctx.total, ctx.total_cost, theme)));
    lines.extend(subscription_line(ctx.subscription_cost, theme));

    if let Some(state) = quota {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!("{PREFIX}{} quota", ctx.model.provider_display_name()),
            theme.keybind_section,
        )));
        lines.extend(quota_lines(state, theme, ctx.clock_format));
    }

    if ctx.by_model.is_empty() {
        return lines;
    }

    let mut models = model_rows(ctx);
    let providers = provider_rows(&models, &ctx.model.provider);
    // One provider row would only restate the session total, which already
    // carries its own rate. The fold earns its lines once there are two.
    if providers.len() > 1 {
        lines.extend(breakdown_table("Per provider", &providers, theme));
    }
    for row in &mut models {
        row.trim_provider(&ctx.model.provider);
    }
    lines.extend(breakdown_table("Per model", &models, theme));

    lines
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

fn breakdown_table(
    heading: &str,
    rows: &[BreakdownRow],
    theme: &crate::theme::Theme,
) -> Vec<Line<'static>> {
    let fg = Style::new().fg(theme.foreground);
    let label_w = rows
        .iter()
        .map(|row| row.label.chars().count())
        .max()
        .unwrap_or(0)
        .max(MODEL_COL_MIN);
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

    let mut lines = vec![
        Line::default(),
        Line::from(Span::styled(
            format!("{PREFIX}{heading}"),
            theme.keybind_section,
        )),
        Line::from(header_row(label_w, &widths, theme)),
    ];
    lines.extend(rows.iter().zip(&cells).map(|(row, cells)| {
        Line::from(model_row(
            row,
            cells,
            &widths,
            label_w,
            fg,
            theme.status_dim,
        ))
    }));
    lines
}

fn build_lifetime_lines(
    lifetime: Option<&LifetimeUsage>,
    theme: &crate::theme::Theme,
) -> Vec<Line<'static>> {
    let fg = Style::new().fg(theme.foreground);
    let Some(lifetime) = lifetime else {
        return vec![Line::from(Span::styled(
            format!("{PREFIX}{LEDGER_UNAVAILABLE}"),
            theme.status_dim,
        ))];
    };
    if lifetime.is_empty() {
        return vec![Line::from(Span::styled(
            format!("{PREFIX}{NO_LEDGER}"),
            theme.status_dim,
        ))];
    }

    let mut lines = vec![
        Line::from(Span::styled(
            format!("{PREFIX}All time"),
            theme.keybind_section,
        )),
        Line::from(vec![
            Span::raw(PREFIX),
            Span::styled(
                format!(
                    "base in {:<7} out {:<7} cache {:<7} total {:<7} hit {:<4} turns {:<7}",
                    format_tokens_u64(lifetime.input),
                    format_tokens_u64(lifetime.output),
                    format_tokens_u64(lifetime.cache_read + lifetime.cache_creation),
                    format_tokens_u64(lifetime.total_tokens()),
                    format_hit_rate(lifetime.cache_hit_rate()),
                    lifetime.turns(),
                ),
                fg,
            ),
            Span::styled(format!("  ${:.2}", lifetime.cost), theme.accent),
        ]),
    ];
    if lifetime.subscription_cost > 0.0 {
        lines.push(Line::from(Span::styled(
            format!(
                "{PREFIX}{SUBSCRIPTION_LINE}: ${:.2}",
                lifetime.subscription_cost
            ),
            theme.status_dim,
        )));
    }
    if lifetime.ephemeral_cost > 0.0 {
        lines.push(Line::from(Span::styled(
            format!(
                "{PREFIX}of which ephemeral runs: ${:.2}",
                lifetime.ephemeral_cost
            ),
            theme.status_dim,
        )));
    }
    if lifetime.unpriced_turns > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "{PREFIX}{} turns had no price, so the total is a floor",
                lifetime.unpriced_turns
            ),
            theme.status_dim,
        )));
    }

    // A lone provider row only restates the all-time line above it, rate and
    // all, so that fold alone has to earn its heading with a second row.
    for (heading, slices, min_rows) in [
        ("Per provider", &lifetime.by_provider, 2),
        ("Per model", &lifetime.by_model, 1),
        ("Per project", &lifetime.by_project, 1),
        ("Per purpose", &lifetime.by_purpose, 1),
        ("Per month", &lifetime.by_month, 1),
    ] {
        if slices.len() < min_rows {
            continue;
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!("{PREFIX}{heading}"),
            theme.keybind_section,
        )));
        lines.extend(slice_rows(slices, fg, theme.status_dim));
    }
    lines
}

fn slice_rows(slices: &[UsageSlice], fg: Style, dim: Style) -> Vec<Line<'static>> {
    let shown = slices.len().min(SLICE_LIMIT);
    let label_w = slices[..shown]
        .iter()
        .map(|slice| slice.label.chars().count())
        .max()
        .unwrap_or(0)
        .max(MODEL_COL_MIN);
    let cells: Vec<_> = slices[..shown]
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
    let mut lines: Vec<Line> = slices[..shown]
        .iter()
        .zip(&cells)
        .map(|(slice, [tokens, turns, cost])| {
            Line::from(vec![
                Span::raw(PREFIX),
                Span::styled(format!("{:<label_w$}", slice.label), fg),
                Span::raw(" ".repeat(COL_GAP)),
                Span::styled(format!("{tokens:>token_w$}"), fg),
                Span::raw(" ".repeat(COL_GAP)),
                Span::styled(
                    format!("{:>RATE_COL$}", format_hit_rate(slice.cache_hit_rate())),
                    fg,
                ),
                Span::raw(" ".repeat(COL_GAP)),
                Span::styled(format!("{turns:>turns_w$}"), dim),
                Span::raw(" ".repeat(COL_GAP)),
                // A slice can hold both payers, so the column shows what the
                // work was worth and marks it when none of it was billed.
                Span::styled(
                    format!("{cost:>cost_w$}"),
                    if slice.cost == 0.0 && slice.subscription_cost > 0.0 {
                        dim
                    } else {
                        fg
                    },
                ),
            ])
        })
        .collect();
    if slices.len() > shown {
        lines.push(Line::from(Span::styled(
            format!("{PREFIX}... and {} more", slices.len() - shown),
            dim,
        )));
    }
    lines
}

/// Its own line rather than a share of the total: the headline number is money
/// owed, and a subscription owes none. Absent when there is none to report.
fn subscription_line(cost: Option<f64>, theme: &crate::theme::Theme) -> Option<Line<'static>> {
    let cost = cost.filter(|cost| *cost > 0.0)?;
    Some(Line::from(Span::styled(
        format!("{PREFIX}{SUBSCRIPTION_LINE}: ${cost:.3}"),
        theme.status_dim,
    )))
}

fn totals_row(
    total: &TokenUsage,
    cost: Option<f64>,
    theme: &crate::theme::Theme,
) -> Vec<Span<'static>> {
    let mut spans = vec![
        Span::raw(PREFIX),
        Span::styled(
            format!(
                "base in {:<7} out {:<7} cache read {:<7} cache write {:<7} total {:<7} hit {:<4}",
                format_tokens(total.input),
                format_tokens(total.output),
                format_tokens(total.cache_read),
                format_tokens(total.cache_creation),
                format_tokens(total.context_tokens()),
                format_hit_rate(total.cache_hit_rate()),
            ),
            Style::new().fg(theme.foreground),
        ),
    ];
    if let Some(c) = cost {
        spans.push(Span::styled(format!("  ${c:.3}"), theme.accent));
    }
    spans
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

fn header_row(
    model_w: usize,
    widths: &[usize; 6],
    theme: &crate::theme::Theme,
) -> Vec<Span<'static>> {
    let [input_w, output_w, read_w, write_w, total_w, cost_w] = *widths;
    let h = |label: &str, width: usize| Span::styled(format!("{label:>width$}"), theme.status_dim);
    let gap = || Span::raw(" ".repeat(COL_GAP));
    vec![
        Span::raw(PREFIX),
        Span::styled(
            format!("{:width$}", "model", width = model_w),
            theme.status_dim,
        ),
        gap(),
        h("base in", input_w),
        gap(),
        h("out", output_w),
        gap(),
        h("cache read", read_w),
        gap(),
        h("cache write", write_w),
        gap(),
        h("total", total_w),
        gap(),
        Span::styled(format!("{:>RATE_COL$}", "hit"), theme.status_dim),
        gap(),
        h("cost", cost_w),
    ]
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
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

fn quota_lines(
    state: &UsageFetchState,
    theme: &crate::theme::Theme,
    clock: ClockFormat,
) -> Vec<Line<'static>> {
    let fg = Style::new().fg(theme.foreground);
    let dim = theme.status_dim;
    match state {
        UsageFetchState::Loading => {
            vec![Line::from(Span::styled(format!("{PREFIX}loading…"), dim))]
        }
        UsageFetchState::Unsupported => vec![Line::from(Span::styled(
            format!("{PREFIX}{NO_USAGE_ENDPOINT}"),
            dim,
        ))],
        UsageFetchState::Error(msg) => {
            vec![Line::from(Span::styled(format!("{PREFIX}{msg}"), dim))]
        }
        UsageFetchState::Ready(usage) => {
            let mut out = Vec::with_capacity(usage.limits.len() + 1);
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
                let mut spans = vec![Span::styled(
                    format!("{PREFIX}{:<label_w$}", limit.label),
                    fg,
                )];
                if let Some(pct) = limit.percentage {
                    spans.push(Span::styled(format!("{pct:>3}%"), theme.accent));
                    spans.push(Span::styled(" used", dim));
                }
                if let Some(detail) = &limit.detail {
                    spans.push(Span::styled(format!("  {detail}"), dim));
                }
                if let Some(ms) = limit.reset_at {
                    spans.push(Span::styled(
                        format!("  Resets {}", format_reset(ms, &tz, clock)),
                        dim,
                    ));
                }
                out.push(Line::from(spans));
            }
            out
        }
    }
}

fn format_reset(epoch_ms: u64, tz: &TimeZone, clock: ClockFormat) -> String {
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
    use crate::components::{buffer_text, test_model};
    use crate::repaint::expect::{OWED, QUIET};
    use caudra_providers::UsageLimit;
    use caudra_workbench::scroll::{SCROLLBAR_STEP_FORWARD, SCROLLBAR_THUMB_HORIZONTAL};
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use std::sync::Arc;
    use test_case::test_case;

    const RECORDED_COST: f64 = 0.123;
    const RECORDED_TEXT: &str = "0.123";
    const SUBSCRIPTION_COST: f64 = 4.567;
    const SUBSCRIPTION_TEXT: &str = "4.567";
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
    };
    const BUCKET_HEADERS: &str = "base in      out  cache read  cache write    total   hit    cost";
    const BUCKET_VALUES: &str = " 101       37         211           53      402      58%   0.123";
    const FOLDED_BUCKET_VALUES: &str =
        " 124       56         254           70      504      57%   0.246";
    const BASE_INPUT_PREFIX: &str = "  base in 101 ";
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
    const LONG_MODEL: &str = "a-model-with-a-deliberately-long-identifier";
    const PANS_TO_THE_END: usize = 8;
    /// A tap covers half a viewport, so a couple reach the end of any table this
    /// modal draws and the rest are absorbed by the clamp.
    const TAPS_TO_THE_END: usize = 4;
    const RELOAD_HINT: &str = "Ctrl+R reload";
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
        (Some(120.646), true, "~120.646"),
        (Some(219.226), true, "~219.226"),
        (Some(0.400), false, "0.400"),
        (None, false, NO_COST_TEXT),
        (Some(0.002), true, "~0.002"),
        (Some(999.9996), false, "1000.000"),
    ];

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
        let lines = breakdown_table(PROVIDER_HEADING, &rows, &theme);
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
        let lines = slice_rows(&slices, fg, theme.status_dim);
        let expected_costs = if subscription {
            [" ~120.646", "   ~0.002", "~1000.000"]
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
        let mut slices = vec![slice(LIFETIME_MODEL, RECORDED_COST); SLICE_LIMIT];
        if mixed {
            slices[0].subscription_cost = SUBSCRIPTION_COST;
        }
        let fg = Style::default();
        let visible = slice_rows(&slices, fg, fg);
        slices.push(UsageSlice {
            label: LONG_MODEL.to_owned(),
            tokens: u64::MAX,
            turns: u64::MAX,
            cost: 1_000_000.0,
            ..UsageSlice::default()
        });
        let capped = slice_rows(&slices, fg, fg);
        assert_eq!(&capped[..SLICE_LIMIT], visible);
        assert_eq!(
            line_texts(&capped[SLICE_LIMIT..]),
            [format!("{PREFIX}... and 1 more")]
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
        line_texts(&build_lifetime_lines(lifetime, &crate::theme::current()))
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

    #[test]
    fn quota_ready_lines_include_labels_and_percentages() {
        let theme = crate::theme::current();
        let usage = ProviderUsage {
            plan: Some("lite".into()),
            limits: vec![
                UsageLimit {
                    label: "Current session".into(),
                    percentage: Some(16),
                    reset_at: Some(0),
                    detail: None,
                },
                UsageLimit {
                    label: "Usage credits".into(),
                    percentage: Some(4),
                    reset_at: None,
                    detail: Some("$2.33 spent".into()),
                },
            ],
        };
        let lines = quota_lines(&UsageFetchState::Ready(usage), &theme, ClockFormat::Hour24);
        assert_eq!(lines.len(), 3);
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
    }

    #[test]
    fn quota_non_terminal_states_render_single_line() {
        let theme = crate::theme::current();
        let clock = ClockFormat::Hour24;
        assert_eq!(
            quota_lines(&UsageFetchState::Loading, &theme, clock).len(),
            1
        );
        let unsupported = quota_lines(&UsageFetchState::Unsupported, &theme, clock);
        assert_eq!(unsupported.len(), 1);
        assert!(
            unsupported[0]
                .spans
                .iter()
                .any(|s| s.content.contains(NO_USAGE_ENDPOINT))
        );
        let err = quota_lines(&UsageFetchState::Error("nope".into()), &theme, clock);
        assert_eq!(err.len(), 1);
        assert!(err[0].spans.iter().any(|s| s.content.contains("nope")));
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
            model,
            fast: false,
            clock_format: ClockFormat::Hour24,
            lifetime: None,
        };
        line_texts(&build_lines(&ctx, None, &crate::theme::current()))
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
        let header = format!("{PREFIX}{:<MODEL_COL_MIN$}  {BUCKET_HEADERS}", "model");
        let own_model = format!("{PREFIX}{:<MODEL_COL_MIN$}  {BUCKET_VALUES}", model.id);
        assert!(rows.contains(&own_model), "{rows:?}");
        assert_eq!(
            rows.iter().filter(|row| **row == header).count(),
            if include_foreign { 2 } else { 1 },
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
            UsageScope::Session => line_texts(&[Line::from(totals_row(&total, None, &theme))]),
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

    /// A subscription owes nothing, so folding its figure into the headline
    /// would report money that no invoice will ever ask for.
    #[test]
    fn a_subscription_gets_its_own_line_and_never_joins_the_total() {
        let model = test_model();
        let total = TokenUsage {
            input: ONE_MILLION,
            ..Default::default()
        };
        let rows = |subscription| {
            modal_rows_with_subscription(
                &total,
                Some(RECORDED_COST),
                subscription,
                &HashMap::new(),
                &model,
            )
        };

        let quiet = rows(None).join("\n");
        assert!(!quiet.contains(SUBSCRIPTION_LINE), "{quiet}");

        let loud = rows(Some(SUBSCRIPTION_COST));
        let line = loud
            .iter()
            .find(|t| t.contains(SUBSCRIPTION_LINE))
            .unwrap_or_else(|| panic!("no subscription line: {loud:?}"));
        assert!(line.contains(SUBSCRIPTION_TEXT), "{line}");
        assert!(
            !line.contains(ONE_MILLION_TEXT),
            "{TOTAL_STAYS_BILLED}: {line}"
        );

        let totals = loud
            .iter()
            .find(|t| t.contains(ONE_MILLION_TEXT))
            .unwrap_or_else(|| panic!("no totals row: {loud:?}"));
        assert!(totals.contains(RECORDED_TEXT), "{totals}");
        assert!(
            !totals.contains(SUBSCRIPTION_TEXT),
            "{TOTAL_STAYS_BILLED}: {totals}"
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
        let backend = ratatui::backend::TestBackend::new(width, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let model = test_model();
        let ctx = UsageModalContext {
            total: &TokenUsage::default(),
            total_cost: None,
            subscription_cost: None,
            by_model,
            model: &model,
            fast: false,
            clock_format: ClockFormat::Hour24,
            lifetime: None,
        };
        terminal
            .draw(|f| {
                modal.view(f, f.area(), &ctx);
            })
            .unwrap();
        buffer_text(terminal.backend().buffer())
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
        assert!(modal.handle_mouse(&MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: arrow_column(&clipped, SCROLLBAR_STEP_FORWARD) - 2,
            row: modal.popup.bottom() - 1,
            modifiers: KeyModifiers::NONE,
        }));

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
        let slot = slot(UsageFetchState::Loading);
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

    #[test]
    fn relative_formats_future_windows() {
        assert_eq!(relative(30), "in 0 min");
        assert_eq!(relative(120), "in 2 min");
        assert_eq!(relative(3 * HOUR + 36 * 60), "in 3 hr 36 min");
        assert_eq!(relative(5 * HOUR), "in 5 hr 0 min");
    }
}
