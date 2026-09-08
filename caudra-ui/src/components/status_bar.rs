use std::borrow::Cow;
use std::env;
use std::path::Path;
use std::time::{Duration, Instant};

use super::{RetryInfo, Status, hover_style};

use crate::animation::spinner_frame;
use crate::theme;

use caudra_providers::format_tokens;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::repaint::{Cadence, Dirty};
use caudra_agent::GoalSnapshot;

const TRUNCATE_PREFIX: &str = "..";
const CWD_MODEL_SEPARATOR: &str = "  ";
const BACK_TO_MAIN_LABEL: &str = "[< Main]";
const FAST_LABEL: &str = " [fast]";
const WORKFLOW_LABEL: &str = " [workflow]";
const WORKFLOW_SHORT_LABEL: &str = " [wf]";
const YOLO_LABEL: &str = " [yolo]";
const YOLO_SHORT_LABEL: &str = " [!]";
const THINKING_PREFIX: &str = "thinking: ";
/// A chip's leading space and its two brackets, which no tier sheds.
const CHIP_OVERHEAD: usize = 3;
const BRACKET_WIDTH: usize = 2;
/// Enough for `[.]`, so a bar too narrow to name the model still offers the
/// control that changes it.
const CLICKABLE_MODEL_FLOOR: usize = 3;
const PLAIN_MODEL_FLOOR: usize = 1;
/// Marks a figure a subscription already covers. One column is all the bar can
/// spare to say the number is a price rather than a bill.
const NOT_BILLED_MARK: &str = "~";

/// What to draw in the bar's one cost slot. Billed spend wins the slot when a
/// session mixes the two, because that is the number someone pays; the tilde
/// would otherwise claim the whole figure is notional when part of it is real.
fn spend(billed: Option<f64>, subscription: Option<f64>) -> Option<String> {
    match (billed, subscription) {
        (Some(billed), _) => Some(format!("${billed:.3}")),
        (None, Some(subscription)) => Some(format!("{NOT_BILLED_MARK}${subscription:.3}")),
        (None, None) => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusBarHitTarget {
    BackToMain,
    Mode,
    Model,
    Thinking,
    Goal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusBarHit {
    pub area: Rect,
    pub target: StatusBarHitTarget,
}

pub struct UsageStats {
    /// The whole session's bill, drawn next to the focused chat's own once
    /// subagents make the two differ.
    pub global_cost: Option<f64>,
    pub global_subscription_cost: Option<f64>,
    pub context_size: u32,
    pub cost: Option<f64>,
    pub subscription_cost: Option<f64>,
    pub context_window: u32,
    pub show_global: bool,
}

pub struct StatusBarContext<'a> {
    pub status: &'a Status,
    pub mode_label: Cow<'static, str>,
    pub mode_style: Style,
    pub model_id: &'a str,
    pub stats: UsageStats,
    pub auto_scroll: bool,
    pub chat_name: Option<&'a str>,
    pub back_to_main: bool,
    pub retry_info: Option<&'a RetryInfo>,
    /// The effective level alone (`off`, `xhigh`, `8192`). The bar spells the
    /// word "thinking" in front of it only when it has the columns to spare.
    pub thinking: Option<Cow<'static, str>>,
    pub fast: bool,
    pub workflow: bool,
    pub yolo: bool,
    pub restoring: bool,
    pub goal: Option<&'a GoalSnapshot>,
    pub mode_clickable: bool,
    pub settings_clickable: bool,
    pub hovered: Option<StatusBarHitTarget>,
    pub hover_url: Option<&'a str>,
}

/// How much of the thinking chip survives: `[thinking: xhigh]`, `[xhigh]`, or
/// nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThinkingTier {
    Named,
    Level,
    Hidden,
}

/// `[anthropic/claude-opus-5]`, then the last path segment, then whatever the
/// leftover columns hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelTier {
    Full,
    Leaf,
    Chopped,
}

/// `[workflow]` and `[yolo]` spelled out, or squeezed to `[wf]` and `[!]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlagTier {
    Named,
    Sigil,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextTier {
    /// `12.0k/200.0k (6%)` beside the spend.
    Counts,
    /// `6%` beside the spend.
    Percent,
    /// `6%` alone.
    Bare,
    Hidden,
}

/// One step down the ladder. Every step either abbreviates something or drops
/// it, so the bar's width falls monotonically and the search terminates.
#[derive(Debug, Clone, Copy)]
enum Reduction {
    DropGlobalSpend,
    CompactContext,
    ShortThinking,
    ShortFlags,
    LeafModel,
    BareContext,
    DropContext,
    DropWorkflow,
    DropFast,
    DropThinking,
    ChopModel,
    DropYolo,
}

/// Cheap abbreviations come before anything is lost, and yolo goes last: a
/// session that skips permission prompts has to say so at any width that can
/// hold three columns.
const LADDER: [Reduction; 12] = [
    Reduction::DropGlobalSpend,
    Reduction::CompactContext,
    Reduction::ShortThinking,
    Reduction::ShortFlags,
    Reduction::LeafModel,
    Reduction::BareContext,
    Reduction::DropContext,
    Reduction::DropWorkflow,
    Reduction::DropFast,
    Reduction::DropThinking,
    Reduction::ChopModel,
    Reduction::DropYolo,
];

/// The counters and prices, rendered once per frame so walking the ladder is
/// pure arithmetic over widths the bar already knows.
struct SpendText {
    counts: String,
    percent: String,
    bare: String,
    global: Option<String>,
}

impl SpendText {
    fn new(stats: &UsageStats) -> Self {
        let pct = if stats.context_window > 0 {
            (stats.context_size as f64 / stats.context_window as f64 * 100.0) as u32
        } else {
            0
        };
        let bare = format!("  {pct}% ");
        let counted = format!(
            "  {}/{} ({pct}%)",
            format_tokens(stats.context_size),
            format_tokens(stats.context_window),
        );
        let (counts, percent) = match spend(stats.cost, stats.subscription_cost) {
            Some(cost) => (format!("{counted} {cost} "), format!("  {pct}% {cost} ")),
            None => (format!("{counted} "), bare.clone()),
        };
        Self {
            counts,
            percent,
            bare,
            global: spend(stats.global_cost, stats.global_subscription_cost)
                .filter(|_| stats.show_global)
                .map(|global| format!(" \u{03a3}{global} ")),
        }
    }
}

/// Which tier each slot of the right-hand side settled on. Widths and spans
/// both read from this, so a chip cannot be measured as one size and drawn as
/// another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fit {
    global_spend: bool,
    context: ContextTier,
    thinking: ThinkingTier,
    flags: FlagTier,
    model: ModelTier,
    fast: bool,
    workflow: bool,
    yolo: bool,
}

impl Fit {
    const FULL: Self = Self {
        global_spend: true,
        context: ContextTier::Counts,
        thinking: ThinkingTier::Named,
        flags: FlagTier::Named,
        model: ModelTier::Full,
        fast: true,
        workflow: true,
        yolo: true,
    };

    fn apply(&mut self, step: Reduction) {
        match step {
            Reduction::DropGlobalSpend => self.global_spend = false,
            Reduction::CompactContext => self.context = ContextTier::Percent,
            Reduction::ShortThinking => self.thinking = ThinkingTier::Level,
            Reduction::ShortFlags => self.flags = FlagTier::Sigil,
            Reduction::LeafModel => self.model = ModelTier::Leaf,
            Reduction::BareContext => self.context = ContextTier::Bare,
            Reduction::DropContext => self.context = ContextTier::Hidden,
            Reduction::DropWorkflow => self.workflow = false,
            Reduction::DropFast => self.fast = false,
            Reduction::DropThinking => self.thinking = ThinkingTier::Hidden,
            Reduction::ChopModel => self.model = ModelTier::Chopped,
            Reduction::DropYolo => self.yolo = false,
        }
    }

    /// Everything right of the cwd, which takes whatever this leaves behind.
    fn width(self, ctx: &StatusBarContext<'_>, spend: &SpendText) -> usize {
        self.model_width(ctx) + self.chip_width(ctx) + self.spend_width(spend)
    }

    /// Clamped up to the floor so [`Reduction::ChopModel`] can never widen a
    /// short id, which would let the ladder grow instead of shrink.
    fn model_width(self, ctx: &StatusBarContext<'_>) -> usize {
        let floor = model_floor(ctx);
        let named = |id: &str| id.width() + usize::from(ctx.settings_clickable) * BRACKET_WIDTH;
        match self.model {
            ModelTier::Full => named(ctx.model_id).max(floor),
            ModelTier::Leaf => named(model_leaf(ctx.model_id)).max(floor),
            ModelTier::Chopped => floor,
        }
    }

    fn chip_width(self, ctx: &StatusBarContext<'_>) -> usize {
        self.thinking_width(ctx)
            + usize::from(ctx.fast && self.fast) * FAST_LABEL.width()
            + usize::from(ctx.workflow && self.workflow) * self.flags.workflow_label().width()
            + usize::from(ctx.yolo && self.yolo) * self.flags.yolo_label().width()
    }

    fn thinking_width(self, ctx: &StatusBarContext<'_>) -> usize {
        let Some(level) = ctx.thinking.as_deref() else {
            return 0;
        };
        match self.thinking {
            ThinkingTier::Named => CHIP_OVERHEAD + THINKING_PREFIX.width() + level.width(),
            ThinkingTier::Level => CHIP_OVERHEAD + level.width(),
            ThinkingTier::Hidden => 0,
        }
    }

    fn spend_width(self, spend: &SpendText) -> usize {
        self.context_text(spend).map_or(0, UnicodeWidthStr::width)
            + self.global_text(spend).map_or(0, UnicodeWidthStr::width)
    }

    fn context_text(self, spend: &SpendText) -> Option<&str> {
        match self.context {
            ContextTier::Counts => Some(&spend.counts),
            ContextTier::Percent => Some(&spend.percent),
            ContextTier::Bare => Some(&spend.bare),
            ContextTier::Hidden => None,
        }
    }

    fn global_text(self, spend: &SpendText) -> Option<&str> {
        spend.global.as_deref().filter(|_| self.global_spend)
    }
}

impl FlagTier {
    fn workflow_label(self) -> &'static str {
        match self {
            Self::Named => WORKFLOW_LABEL,
            Self::Sigil => WORKFLOW_SHORT_LABEL,
        }
    }

    fn yolo_label(self) -> &'static str {
        match self {
            Self::Named => YOLO_LABEL,
            Self::Sigil => YOLO_SHORT_LABEL,
        }
    }
}

pub struct StatusBar {
    flash: Option<(String, Instant)>,
    started_at: Instant,
    cwd_branch: String,
    pub flash_duration: Duration,
    branch_update_rx: Option<flume::Receiver<()>>,
}

impl StatusBar {
    pub fn new(flash_duration: Duration) -> Self {
        Self {
            flash: None,
            started_at: Instant::now(),
            cwd_branch: cwd_branch_label(),
            flash_duration,
            branch_update_rx: spawn_branch_watcher(),
        }
    }

    pub fn flash(&mut self, msg: String) {
        self.flash = Some((msg, Instant::now()));
    }

    #[cfg(test)]
    pub fn flash_text(&self) -> Option<&str> {
        self.flash.as_ref().map(|(s, _)| s.as_str())
    }

    pub fn refresh_cwd(&mut self) {
        self.cwd_branch = cwd_branch_label();
    }

    pub fn poll_branch_update(&mut self) -> Dirty {
        let Some(rx) = &self.branch_update_rx else {
            return Dirty::NO;
        };
        if rx.try_iter().next().is_none() {
            return Dirty::NO;
        }
        let branch = cwd_branch_label();
        let changed = branch != self.cwd_branch;
        self.cwd_branch = branch;
        Dirty::from(changed)
    }

    pub fn clear_flash(&mut self) {
        self.flash = None;
    }

    pub fn clear_expired_hint(&mut self) -> Dirty {
        if self
            .flash
            .as_ref()
            .is_none_or(|(_, t)| t.elapsed() < self.flash_duration)
        {
            return Dirty::NO;
        }
        self.flash = None;
        Dirty::YES
    }

    /// The bar spins for a whole turn, again while a restore is in flight, and
    /// it counts a retry down by the second. It sits next to [`Self::view`] so
    /// a new moving span cannot forget to claim its frames.
    pub fn cadence(status: &Status, restoring: bool, retrying: bool, goal_active: bool) -> Cadence {
        Cadence::any([
            Cadence::when(
                *status == Status::Streaming || restoring || retrying,
                Cadence::SPINNER,
            ),
            Cadence::when(goal_active, Cadence::CLOCK),
        ])
    }

    pub fn view(&self, frame: &mut Frame, area: Rect, ctx: &StatusBarContext) -> Vec<StatusBarHit> {
        if let Some(url) = ctx.hover_url.filter(|_| self.flash.is_none()) {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!(" {url}"),
                    theme::current().status_notice,
                ))),
                area,
            );
            return Vec::new();
        }
        let mut left_spans = Vec::new();

        if *ctx.status == Status::Streaming {
            let ch = spinner_frame(self.started_at.elapsed().as_millis());
            left_spans.push(Span::styled(format!(" {ch}"), theme::current().spinner));
        }

        if ctx.restoring {
            let ch = spinner_frame(self.started_at.elapsed().as_millis());
            left_spans.push(Span::styled(
                format!(" {ch}"),
                theme::current().status_notice,
            ));
        }

        let mode_offset = left_spans.iter().map(Span::width).sum::<usize>() + " ".width();
        left_spans.push(Span::raw(" "));
        left_spans.push(Span::styled(
            ctx.mode_label.clone(),
            hover_style(
                ctx.mode_style,
                ctx.mode_clickable && ctx.hovered == Some(StatusBarHitTarget::Mode),
            ),
        ));

        let back_offset = ctx
            .back_to_main
            .then(|| left_spans.iter().map(Span::width).sum::<usize>() + " ".width());
        if ctx.back_to_main {
            left_spans.push(Span::raw(" "));
            left_spans.push(Span::styled(
                BACK_TO_MAIN_LABEL,
                hover_style(
                    theme::current().status_notice,
                    ctx.hovered == Some(StatusBarHitTarget::BackToMain),
                ),
            ));
        }

        if let Some(name) = ctx.chat_name {
            left_spans.push(Span::styled(
                if ctx.back_to_main {
                    format!(" {name}")
                } else {
                    format!(" [{name}]")
                },
                theme::current().status_dim,
            ));
        }

        if !ctx.auto_scroll {
            left_spans.push(Span::styled(
                " auto-scroll paused",
                theme::current().status_dim,
            ));
        }

        let goal_hit = ctx.goal.map(|goal| {
            let label = format!(
                "[goal · {} · {}]",
                goal.evaluations,
                format_goal_elapsed(goal.elapsed())
            );
            let offset = left_spans.iter().map(Span::width).sum::<usize>() + " ".width();
            let width = label.width();
            left_spans.push(Span::raw(" "));
            left_spans.push(Span::styled(
                label,
                hover_style(
                    theme::current().status_notice,
                    ctx.settings_clickable && ctx.hovered == Some(StatusBarHitTarget::Goal),
                ),
            ));
            (offset, width)
        });

        if let Some(retry) = ctx.retry_info {
            let secs = retry
                .deadline
                .saturating_duration_since(Instant::now())
                .as_secs();
            left_spans.push(Span::styled(
                format!(" {}", retry.message),
                theme::current().status_retry_error,
            ));
            left_spans.push(Span::styled(
                format!(" · retrying in {secs}s (#{})", retry.attempt),
                theme::current().status_retry_info,
            ));
        }

        let mut right_spans = Vec::new();
        let mut model_hit = None;
        let mut thinking_hit = None;

        match ctx.status {
            Status::Error { message: e, .. } => {
                left_spans.push(Span::styled(format!(" {e}"), theme::current().error));
            }
            _ => {
                let left_width = left_spans.iter().map(Span::width).sum::<usize>();
                let side = right_side(
                    ctx,
                    &self.cwd_branch,
                    (area.width as usize).saturating_sub(left_width),
                );
                right_spans = side.spans;
                model_hit = Some(side.model_hit);
                thinking_hit = side.thinking_hit;
            }
        }

        if let Some((ref msg, _)) = self.flash {
            left_spans.push(Span::styled(
                format!(" {msg}"),
                theme::current().status_notice,
            ));
        }

        let [left_area, right_area] = status_areas(area, &right_spans);

        frame.render_widget(Paragraph::new(Line::from(left_spans)), left_area);
        frame.render_widget(
            Paragraph::new(Line::from(right_spans)).alignment(Alignment::Right),
            right_area,
        );

        let mut hits = Vec::with_capacity(2);
        push_hit(
            &mut hits,
            left_area,
            mode_offset,
            ctx.mode_label.width(),
            ctx.mode_clickable,
            StatusBarHitTarget::Mode,
        );
        push_hit(
            &mut hits,
            left_area,
            back_offset.unwrap_or_default(),
            BACK_TO_MAIN_LABEL.width(),
            ctx.back_to_main,
            StatusBarHitTarget::BackToMain,
        );
        if let Some((offset, width)) = model_hit {
            push_hit(
                &mut hits,
                right_area,
                offset,
                width,
                ctx.settings_clickable,
                StatusBarHitTarget::Model,
            );
        }
        if let Some((offset, width)) = thinking_hit {
            push_hit(
                &mut hits,
                right_area,
                offset,
                width,
                ctx.settings_clickable,
                StatusBarHitTarget::Thinking,
            );
        }
        if let Some((offset, width)) = goal_hit {
            push_hit(
                &mut hits,
                left_area,
                offset,
                width,
                ctx.settings_clickable,
                StatusBarHitTarget::Goal,
            );
        }
        hits
    }
}

/// The right-hand half of the bar, already fitted to `budget`.
struct RightSide<'a> {
    spans: Vec<Span<'a>>,
    /// Offsets measured on the glyphs that were drawn, so a hit cannot claim
    /// columns a shorter tier never used.
    model_hit: (usize, usize),
    thinking_hit: Option<(usize, usize)>,
}

/// Walks [`LADDER`] until the fixed chips and the model fit, then hands the cwd
/// whatever is left. The cwd goes last because the model names what answers you
/// and the path is usually already in the shell prompt.
fn right_side<'a>(
    ctx: &'a StatusBarContext<'_>,
    cwd_label: &'a str,
    budget: usize,
) -> RightSide<'a> {
    let spend = SpendText::new(&ctx.stats);
    let mut fit = Fit::FULL;
    for step in LADDER {
        if fit.width(ctx, &spend) <= budget {
            break;
        }
        fit.apply(step);
    }

    let mut chips = Vec::new();
    let mut thinking_chip = None;
    if let Some(level) = ctx.thinking.as_deref()
        && fit.thinking != ThinkingTier::Hidden
    {
        let label = match fit.thinking {
            ThinkingTier::Named => format!("[{THINKING_PREFIX}{level}]"),
            _ => format!("[{level}]"),
        };
        thinking_chip = Some((
            chips.iter().map(Span::width).sum::<usize>() + 1,
            label.width(),
        ));
        chips.push(Span::raw(" "));
        chips.push(Span::styled(
            label,
            hover_style(
                settings_style(ctx),
                ctx.settings_clickable && ctx.hovered == Some(StatusBarHitTarget::Thinking),
            ),
        ));
    }
    if ctx.fast && fit.fast {
        chips.push(Span::styled(FAST_LABEL, theme::current().status_dim));
    }
    if ctx.workflow && fit.workflow {
        chips.push(Span::styled(
            fit.flags.workflow_label(),
            theme::current().status_dim,
        ));
    }
    if ctx.yolo && fit.yolo {
        chips.push(Span::styled(fit.flags.yolo_label(), theme::current().error));
    }
    let counters = Style::new().fg(theme::current().foreground);
    for text in fit
        .context_text(&spend)
        .into_iter()
        .chain(fit.global_text(&spend))
    {
        chips.push(Span::styled(text.to_owned(), counters));
    }

    let residue = budget.saturating_sub(chips.iter().map(Span::width).sum::<usize>());
    let model = model_text(ctx, fit.model, residue);
    let separator = if model.is_empty() {
        ""
    } else {
        CWD_MODEL_SEPARATOR
    };
    let cwd = cwd_text(
        cwd_label,
        residue
            .saturating_sub(model.width())
            .saturating_sub(separator.width()),
    );
    let separator = if cwd.is_empty() { "" } else { separator };

    let model_offset = cwd.width() + separator.width();
    let model_width = model.width();
    let mut spans = Vec::with_capacity(chips.len() + 3);
    spans.push(Span::styled(cwd, theme::current().status_dim));
    spans.push(Span::raw(separator));
    spans.push(Span::styled(
        model,
        hover_style(
            settings_style(ctx),
            ctx.settings_clickable && ctx.hovered == Some(StatusBarHitTarget::Model),
        ),
    ));
    spans.append(&mut chips);

    RightSide {
        spans,
        model_hit: (model_offset, model_width),
        thinking_hit: thinking_chip
            .map(|(offset, width)| (model_offset + model_width + offset, width)),
    }
}

fn settings_style(ctx: &StatusBarContext<'_>) -> Style {
    if ctx.settings_clickable {
        theme::current().status_notice
    } else {
        theme::current().status_dim
    }
}

fn model_floor(ctx: &StatusBarContext<'_>) -> usize {
    if ctx.settings_clickable {
        CLICKABLE_MODEL_FLOOR
    } else {
        PLAIN_MODEL_FLOOR
    }
}

fn model_text<'a>(ctx: &'a StatusBarContext<'_>, tier: ModelTier, budget: usize) -> Cow<'a, str> {
    let id = match tier {
        ModelTier::Full => ctx.model_id,
        ModelTier::Leaf | ModelTier::Chopped => model_leaf(ctx.model_id),
    };
    if ctx.settings_clickable {
        bracketed_tail(id, budget)
    } else {
        truncate_tail(id, budget)
    }
}

/// `anthropic/claude-opus-5` is `claude-opus-5` once the columns run out: the
/// segment that distinguishes two models the user might be switching between.
fn model_leaf(id: &str) -> &str {
    match id.rsplit_once('/') {
        Some((_, leaf)) if !leaf.is_empty() => leaf,
        _ => id,
    }
}

/// `~/projects/caudra:main`, then `caudra:main`, then `caudra`, then nothing.
/// Chopping the head instead would spend the same columns on `..dra:main`.
fn cwd_text(label: &str, budget: usize) -> &str {
    if label.width() <= budget {
        return label;
    }
    let path = &label[..label.rfind(':').unwrap_or(label.len())];
    let leaf = path.rfind('/').map_or(0, |slash| slash + 1);
    [&label[leaf..], &path[leaf..]]
        .into_iter()
        .find(|candidate| candidate.width() <= budget)
        .unwrap_or_default()
}

fn status_areas(area: Rect, right_spans: &[Span<'_>]) -> [Rect; 2] {
    Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(right_spans.iter().map(|span| span.width() as u16).sum()),
    ])
    .areas(area)
}

fn push_hit(
    hits: &mut Vec<StatusBarHit>,
    area: Rect,
    offset: usize,
    width: usize,
    enabled: bool,
    target: StatusBarHitTarget,
) {
    let (Ok(offset), Ok(width)) = (u16::try_from(offset), u16::try_from(width)) else {
        return;
    };
    let Some(x) = area.x.checked_add(offset) else {
        return;
    };
    if enabled && area.height > 0 && width > 0 && x.saturating_add(width) <= area.right() {
        hits.push(StatusBarHit {
            area: Rect::new(x, area.y, width, 1),
            target,
        });
    }
}

fn format_goal_elapsed(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3_600 {
        format!("{}h", seconds / 3_600)
    } else if seconds >= 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

fn truncate_tail(s: &str, max_width: usize) -> Cow<'_, str> {
    if s.width() <= max_width {
        return Cow::Borrowed(s);
    }
    if max_width <= TRUNCATE_PREFIX.width() {
        return Cow::Owned(".".repeat(max_width));
    }
    let budget = max_width.saturating_sub(TRUNCATE_PREFIX.width());
    let mut used = 0;
    let mut start = s.len();
    for (i, c) in s.char_indices().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        used += w;
        start = i;
    }
    Cow::Owned(format!("{TRUNCATE_PREFIX}{}", &s[start..]))
}

fn bracketed_tail(s: &str, max_width: usize) -> Cow<'_, str> {
    if max_width < 3 {
        return Cow::Borrowed("");
    }
    Cow::Owned(format!("[{}]", truncate_tail(s, max_width - 2)))
}

fn collapse_home(path: &str) -> String {
    let Some(home) = caudra_storage::paths::home() else {
        return path.to_string();
    };
    collapse_home_with(path, &home.to_string_lossy())
}

fn collapse_home_with(path: &str, home: &str) -> String {
    path.strip_prefix(home)
        .map(|rest| format!("~{rest}"))
        .unwrap_or_else(|| path.to_string())
}

fn cwd_branch_label() -> String {
    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());
    let label = collapse_home(&cwd);
    match detect_branch(&cwd) {
        Some(branch) => format!("{label}:{branch}"),
        None => label,
    }
}

fn detect_branch(cwd: &str) -> Option<String> {
    let head = std::fs::read_to_string(find_git_dir(Path::new(cwd))?.join("HEAD")).ok()?;
    let head = head.trim();
    head.strip_prefix("ref: refs/heads/")
        .map(str::to_string)
        .or_else(|| Some(head.get(..7)?.to_string()))
}

fn find_git_dir(cwd: &Path) -> Option<std::path::PathBuf> {
    let mut dir = cwd;
    loop {
        let git = dir.join(".git");
        if git.is_dir() {
            return Some(git);
        }
        dir = dir.parent()?;
    }
}

fn spawn_branch_watcher() -> Option<flume::Receiver<()>> {
    use notify::{RecursiveMode, Watcher};

    let cwd = env::current_dir().ok()?;
    let git_dir = find_git_dir(&cwd)?;
    let (tx, rx) = flume::bounded(1);

    std::thread::spawn(move || {
        let Ok(mut watcher) = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
            if res.is_ok_and(|e| e.paths.iter().any(|p| p.ends_with("HEAD"))) {
                let _ = tx.try_send(());
            }
        }) else {
            return;
        };
        if watcher.watch(&git_dir, RecursiveMode::NonRecursive).is_ok() {
            std::thread::park();
        }
    });

    Some(rx)
}

#[cfg(test)]
mod tests {
    use ratatui::style::Modifier;
    use std::fs;

    use super::*;
    use crate::repaint::expect::QUIET;
    use tempfile::TempDir;
    use test_case::test_case;

    const FLASH_TTL: Duration = Duration::from_secs(3600);
    const FLASH_MSG: &str = "Copied";
    const STALE_BRANCH: &str = "/nowhere:gone";
    const BAR_WIDTH: u16 = 120;
    const MODE_LABEL: &str = "[BUILD]";
    const MODEL_ID: &str = "test-model";
    const THINKING_LEVEL: &str = "off";
    const LADDER_MODEL_ID: &str = "anthropic/claude-opus-5";
    const LADDER_MODEL_LEAF: &str = "claude-opus-5";
    const LADDER_THINKING: &str = "xhigh";
    const LADDER_CWD: &str = "~/projects/caudra:main";
    const PERCENT_MARK: &str = "%";
    /// A budget the ladder answers with a squeezed thinking chip: wide enough
    /// to keep the control, too narrow to spell the word in front of it.
    const SHORT_THINKING_BUDGET: usize = 40;
    const SHORT_THINKING_CHIP: &str = "[xhigh]";
    const OVER_BUDGET_MSG: &str = "the right side claimed more columns than its budget";
    const MONOTONE_MSG: &str = "a narrower bar showed a chip the wider one had dropped";
    const YOLO_LAST_MSG: &str = "yolo must outlive every other chip";
    const SHORT_THINKING_MSG: &str = "a squeezed thinking chip must still own its own glyphs";
    const CONTEXT_SIZE: u32 = 12_000;
    const CHAT_COST: f64 = 0.25;
    const CHAT_COST_TEXT: &str = "$0.250";
    const SESSION_COST: f64 = 1.5;
    const SESSION_COST_TEXT: &str = "\u{03a3}$1.500";
    const SIGMA: char = '\u{03a3}';
    const GOAL_CONDITION: &str = "all focused tests pass";
    const GOAL_CHIP_PREFIX: &str = "[goal \u{b7}";

    fn active_goal() -> GoalSnapshot {
        caudra_agent::GoalHandle::default()
            .set(GOAL_CONDITION)
            .unwrap()
    }

    fn render_at(
        width: u16,
        global_cost: Option<f64>,
        show_global: bool,
        yolo: bool,
        hovered: Option<StatusBarHitTarget>,
        hover_url: Option<&str>,
        goal: Option<&GoalSnapshot>,
    ) -> (String, Vec<StatusBarHit>, Vec<Style>) {
        let bar = StatusBar::new(FLASH_TTL);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 1)).unwrap();
        let ctx = StatusBarContext {
            status: &Status::Idle,
            mode_label: MODE_LABEL.into(),
            mode_style: Style::new(),
            model_id: MODEL_ID,
            stats: UsageStats {
                global_cost,
                global_subscription_cost: None,
                context_size: CONTEXT_SIZE,
                cost: Some(CHAT_COST),
                subscription_cost: None,
                context_window: crate::components::TEST_CONTEXT_WINDOW,
                show_global,
            },
            auto_scroll: true,
            chat_name: None,
            back_to_main: false,
            retry_info: None,
            thinking: Some(THINKING_LEVEL.into()),
            fast: false,
            workflow: false,
            yolo,
            restoring: false,
            goal,
            mode_clickable: true,
            settings_clickable: true,
            hovered,
            hover_url,
        };
        let mut hits = Vec::new();
        terminal
            .draw(|f| {
                hits = bar.view(f, f.area(), &ctx);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let styles = (0..width)
            .map(|column| buffer.cell((column, 0)).unwrap().style())
            .collect();
        (crate::components::buffer_text(buffer), hits, styles)
    }

    fn render(global_cost: Option<f64>, show_global: bool, yolo: bool) -> String {
        render_at(BAR_WIDTH, global_cost, show_global, yolo, None, None, None).0
    }

    /// Every flag on, a long model id and a real path: the widest the bar ever
    /// has to squeeze, so each rung of the ladder gets exercised.
    fn with_ladder_ctx(f: impl FnOnce(&StatusBarContext<'_>)) {
        let ctx = StatusBarContext {
            status: &Status::Idle,
            mode_label: MODE_LABEL.into(),
            mode_style: Style::new(),
            model_id: LADDER_MODEL_ID,
            stats: UsageStats {
                global_cost: Some(SESSION_COST),
                global_subscription_cost: None,
                context_size: CONTEXT_SIZE,
                cost: Some(CHAT_COST),
                subscription_cost: None,
                context_window: crate::components::TEST_CONTEXT_WINDOW,
                show_global: true,
            },
            auto_scroll: true,
            chat_name: None,
            back_to_main: false,
            retry_info: None,
            thinking: Some(LADDER_THINKING.into()),
            fast: true,
            workflow: true,
            yolo: true,
            restoring: false,
            goal: None,
            mode_clickable: true,
            settings_clickable: true,
            hovered: None,
            hover_url: None,
        };
        f(&ctx);
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Chip {
        Thinking,
        Fast,
        Workflow,
        Yolo,
        Context,
    }

    fn side_text(side: &RightSide<'_>) -> String {
        side.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// Reads the drawn glyphs rather than the [`Fit`], so a tier that measures
    /// one way and draws another still counts as absent.
    fn visible_chips(side: &RightSide<'_>) -> Vec<Chip> {
        let text = side_text(side);
        [
            Chip::Thinking,
            Chip::Fast,
            Chip::Workflow,
            Chip::Yolo,
            Chip::Context,
        ]
        .into_iter()
        .filter(|chip| match chip {
            Chip::Thinking => text.contains(LADDER_THINKING),
            Chip::Fast => text.contains(FAST_LABEL.trim()),
            Chip::Workflow => {
                text.contains(WORKFLOW_LABEL.trim()) || text.contains(WORKFLOW_SHORT_LABEL.trim())
            }
            Chip::Yolo => {
                text.contains(YOLO_LABEL.trim()) || text.contains(YOLO_SHORT_LABEL.trim())
            }
            Chip::Context => text.contains(PERCENT_MARK),
        })
        .collect()
    }

    /// An overdrawn bar does not fail on screen: ratatui clips it, and the
    /// columns come out of the mode label at the other end.
    #[test_case(0   ; "no_room")]
    #[test_case(3   ; "model_floor_only")]
    #[test_case(8   ; "very_narrow")]
    #[test_case(16  ; "narrow")]
    #[test_case(24  ; "cramped")]
    #[test_case(40  ; "medium")]
    #[test_case(60  ; "roomy")]
    #[test_case(80  ; "wide")]
    #[test_case(120 ; "everything_fits")]
    fn the_right_side_never_exceeds_its_budget(budget: usize) {
        with_ladder_ctx(|ctx| {
            let side = right_side(ctx, LADDER_CWD, budget);
            let width = side.spans.iter().map(Span::width).sum::<usize>();
            assert!(width <= budget, "{OVER_BUDGET_MSG}: {width} > {budget}");
        });
    }

    /// The ladder is a prefix chain, so a wider bar can only settle on an
    /// earlier rung. The cwd is left out: it spends whatever the chips and the
    /// model did not, so a wider bar that keeps the full model id legitimately
    /// has less room for the path.
    #[test_case(0,  3   ; "nothing_to_something")]
    #[test_case(3,  8   ; "model_floor_to_narrow")]
    #[test_case(8,  16  ; "narrow_steps_up")]
    #[test_case(16, 24  ; "cramped_steps_up")]
    #[test_case(24, 40  ; "medium_steps_up")]
    #[test_case(40, 60  ; "roomy_steps_up")]
    #[test_case(60, 80  ; "wide_steps_up")]
    #[test_case(80, 120 ; "everything_steps_up")]
    fn a_narrower_bar_never_shows_more(narrow: usize, wide: usize) {
        with_ladder_ctx(|ctx| {
            let fewer = visible_chips(&right_side(ctx, LADDER_CWD, narrow));
            let more = visible_chips(&right_side(ctx, LADDER_CWD, wide));
            assert!(
                fewer.iter().all(|chip| more.contains(chip)),
                "{MONOTONE_MSG}: {fewer:?} at {narrow}, {more:?} at {wide}"
            );
        });
    }

    /// Yolo skips permission prompts for the rest of the session, so the bar
    /// may not trade that warning for a token count or a reasoning level.
    #[test_case(3   ; "model_floor_only")]
    #[test_case(8   ; "very_narrow")]
    #[test_case(16  ; "narrow")]
    #[test_case(24  ; "cramped")]
    #[test_case(40  ; "medium")]
    #[test_case(60  ; "roomy")]
    fn a_bypassed_session_keeps_its_warning_longest(budget: usize) {
        with_ladder_ctx(|ctx| {
            let chips = visible_chips(&right_side(ctx, LADDER_CWD, budget));
            let others = chips.iter().any(|chip| *chip != Chip::Yolo);
            assert!(
                !others || chips.contains(&Chip::Yolo),
                "{YOLO_LAST_MSG}: {chips:?} at {budget}"
            );
        });
    }

    /// Squeezing the chip must move its click target with it, or the bar hands
    /// clicks to columns the short spelling never drew on.
    #[test]
    fn a_squeezed_thinking_chip_keeps_its_own_hit() {
        with_ladder_ctx(|ctx| {
            let side = right_side(ctx, LADDER_CWD, SHORT_THINKING_BUDGET);
            let (offset, width) = side.thinking_hit.expect(SHORT_THINKING_MSG);
            let text = side_text(&side);
            let chip: String = text.chars().skip(offset).take(width).collect();

            assert_eq!(chip, SHORT_THINKING_CHIP, "{SHORT_THINKING_MSG}");
            assert_eq!(
                text.chars().nth(offset - 1),
                Some(' '),
                "{SHORT_THINKING_MSG}"
            );
        });
    }

    /// The whole point of the ladder: the columns freed by abbreviating a chip
    /// go to the strings that actually identify the session.
    #[test]
    fn a_squeezed_bar_still_names_the_model() {
        with_ladder_ctx(|ctx| {
            let text = side_text(&right_side(ctx, LADDER_CWD, SHORT_THINKING_BUDGET));
            assert!(text.contains(LADDER_MODEL_LEAF), "{text}");
        });
    }

    #[test_case("anthropic/claude-opus-5", "claude-opus-5" ; "strips_provider_and_org")]
    #[test_case("claude-opus-5", "claude-opus-5"           ; "bare_id_is_its_own_leaf")]
    #[test_case("anthropic/", "anthropic/"                 ; "trailing_slash_keeps_the_id")]
    fn model_leaf_cases(id: &str, expected: &str) {
        assert_eq!(model_leaf(id), expected);
    }

    #[test_case("~/projects/caudra:main", 30, "~/projects/caudra:main" ; "fits_untouched")]
    #[test_case("~/projects/caudra:main", 12, "caudra:main"            ; "leaf_keeps_the_branch")]
    #[test_case("~/projects/caudra:main", 8,  "caudra"                 ; "leaf_alone")]
    #[test_case("~/projects/caudra:main", 3,  ""                       ; "nothing_readable_fits")]
    #[test_case("~/projects/caudra", 8, "caudra"                       ; "no_branch_to_shed")]
    #[test_case("caudra:main", 6, "caudra"                             ; "no_path_to_shed")]
    fn cwd_text_cases(label: &str, budget: usize, expected: &str) {
        assert_eq!(cwd_text(label, budget), expected);
    }

    /// The sigma is the whole session's bill, and only the session can hand it
    /// over. Pricing the focused chat's counters instead (what the bar used to
    /// do) tells the user a paid session was free, or bills another chat's
    /// tokens at this model's rates. A lone chat has nothing extra to show.
    #[test_case(Some(SESSION_COST), true  => true  ; "subagents_add_the_session_total")]
    #[test_case(Some(SESSION_COST), false => false ; "single_chat_shows_its_own_cost_only")]
    #[test_case(None,               true  => false ; "unpriced_session_claims_nothing")]
    fn session_total_appears_only_when_there_is_one_to_show(
        global_cost: Option<f64>,
        show_global: bool,
    ) -> bool {
        let text = render(global_cost, show_global, false);
        assert_eq!(text.matches(CHAT_COST_TEXT).count(), 1, "{text}");
        let shown = text.matches(SESSION_COST_TEXT).count() == 1;
        assert_eq!(
            text.contains(SIGMA),
            shown,
            "a sigma carrying another number is a misrender: {text}"
        );
        shown
    }

    /// Yolo now outlives the process that turned it on, so the one-shot flash
    /// is no longer enough to tell the user their prompts are being skipped.
    #[test_case(true  => true  ; "a_bypassed_session_says_so")]
    #[test_case(false => false ; "a_prompting_session_stays_quiet")]
    fn the_bar_advertises_yolo(yolo: bool) -> bool {
        render(None, false, yolo).contains(YOLO_LABEL.trim())
    }

    #[test_case(0 ; "zero_width")]
    #[test_case(1 ; "one_column")]
    #[test_case(2 ; "two_columns")]
    #[test_case(20 ; "compact")]
    #[test_case(40 ; "medium")]
    #[test_case(BAR_WIDTH ; "wide")]
    fn status_hits_stay_inside_the_rendered_area(width: u16) {
        let goal = active_goal();
        let (_, hits, _) = render_at(
            width,
            Some(SESSION_COST),
            true,
            true,
            None,
            None,
            Some(&goal),
        );
        let area = Rect::new(0, 0, width, 1);
        assert!(hits.iter().all(|hit| {
            hit.area.width > 0
                && area.contains(ratatui::layout::Position::new(hit.area.x, hit.area.y))
                && hit.area.right() <= area.right()
        }));
    }

    #[test]
    fn compact_status_preserves_mode_control() {
        let (_, hits, _) = render_at(20, None, false, false, None, None, None);
        assert!(
            hits.iter()
                .any(|hit| hit.target == StatusBarHitTarget::Mode)
        );
    }

    #[test]
    fn wide_status_exposes_all_main_controls() {
        let goal = active_goal();
        let (_, hits, _) = render_at(BAR_WIDTH, None, false, false, None, None, Some(&goal));
        for target in [
            StatusBarHitTarget::Mode,
            StatusBarHitTarget::Model,
            StatusBarHitTarget::Thinking,
            StatusBarHitTarget::Goal,
        ] {
            assert!(hits.iter().any(|hit| hit.target == target));
        }
    }

    #[test]
    fn hovered_control_style_is_reversed() {
        let style = hover_style(Style::new(), true);
        assert!(style.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn hover_highlights_labels_without_their_leading_spaces() {
        let goal = active_goal();
        for target in [
            StatusBarHitTarget::Mode,
            StatusBarHitTarget::Model,
            StatusBarHitTarget::Thinking,
            StatusBarHitTarget::Goal,
        ] {
            let (_, hits, styles) = render_at(
                BAR_WIDTH,
                None,
                false,
                false,
                Some(target),
                None,
                Some(&goal),
            );
            let hit = hits.iter().find(|hit| hit.target == target).unwrap();
            let start = usize::from(hit.area.x);
            let end = usize::from(hit.area.right());

            assert!(!styles[start - 1].add_modifier.contains(Modifier::REVERSED));
            assert!(
                styles[start..end]
                    .iter()
                    .all(|style| style.add_modifier.contains(Modifier::REVERSED))
            );
        }
    }

    #[test]
    fn hovered_url_replaces_status_content() {
        const URL: &str = "https://example.com/docs";
        let (text, hits, _) = render_at(BAR_WIDTH, None, false, false, None, Some(URL), None);

        assert!(text.trim_start().starts_with(URL));
        assert!(hits.is_empty());
    }

    /// The chip is the only left-side control whose width comes from live
    /// numbers, so the hit is measured against what was drawn rather than
    /// against a re-formatted label whose elapsed time may already have moved.
    #[test]
    fn goal_chip_hit_covers_the_chip_alone() {
        let goal = active_goal();
        let (text, hits, _) = render_at(BAR_WIDTH, None, false, false, None, None, Some(&goal));
        let hit = hits
            .iter()
            .find(|hit| hit.target == StatusBarHitTarget::Goal)
            .expect("an active goal is a footer control");

        let chip: String = text
            .chars()
            .skip(usize::from(hit.area.x))
            .take(usize::from(hit.area.width))
            .collect();
        assert!(chip.starts_with(GOAL_CHIP_PREFIX), "{chip}");
        assert!(chip.ends_with(']'), "{chip}");
        assert_eq!(text.chars().nth(usize::from(hit.area.x) - 1), Some(' '));
    }

    #[test]
    fn a_session_without_a_goal_has_no_goal_control() {
        let (text, hits, _) = render_at(BAR_WIDTH, None, false, false, None, None, None);

        assert!(!text.contains(GOAL_CHIP_PREFIX));
        assert!(
            hits.iter()
                .all(|hit| hit.target != StatusBarHitTarget::Goal)
        );
    }

    #[test_case("/home/user/projects/app", "/home/user", "~/projects/app" ; "inside_home")]
    #[test_case("/tmp/other", "/home/user", "/tmp/other"                  ; "outside_home")]
    #[test_case("/home/user", "/home/user", "~"                           ; "exact_home")]
    fn collapse_home_cases(path: &str, home: &str, expected: &str) {
        assert_eq!(collapse_home_with(path, home), expected);
    }

    #[test_case("~/projects/caudra:main", 30, "~/projects/caudra:main" ; "fits_untouched")]
    #[test_case("~/projects/caudra:main", 10, "..dra:main"           ; "ascii_tail")]
    #[test_case("~/文档/proj:分支", 8, "..j:分支"                  ; "cjk_path_and_branch")]
    #[test_case("release/🚀-v2", 6, "..-v2"                        ; "emoji_branch")]
    #[test_case("abc", 2, ".."                                     ; "prefix_only")]
    #[test_case("abc", 1, "."                                      ; "single_column")]
    #[test_case("abc", 0, ""                                       ; "zero_columns")]
    #[test_case("", 0, ""                                          ; "empty")]
    fn truncate_tail_cases(input: &str, max_width: usize, expected: &str) {
        assert_eq!(truncate_tail(input, max_width), expected);
    }

    /// The tilde is the bar's only room to say a figure is a price rather than
    /// a bill, so a session that owes real money must not wear one.
    #[test_case(Some(0.123), None,        Some("$0.123")  ; "billed_spend_is_bare")]
    #[test_case(None,        Some(4.567), Some("~$4.567") ; "subscription_is_marked")]
    #[test_case(Some(0.123), Some(4.567), Some("$0.123")  ; "billed_wins_a_mixed_slot")]
    #[test_case(None,        None,        None            ; "nothing_spent_shows_nothing")]
    fn spend_cases(billed: Option<f64>, subscription: Option<f64>, expected: Option<&str>) {
        assert_eq!(spend(billed, subscription).as_deref(), expected);
    }

    #[test_case("model", 7, "[model]" ; "fits")]
    #[test_case("model", 5, "[..l]"   ; "truncates_inside_brackets")]
    #[test_case("model", 2, ""        ; "cannot_fit_button")]
    fn bracketed_tail_cases(input: &str, max_width: usize, expected: &str) {
        assert_eq!(bracketed_tail(input, max_width), expected);
    }

    fn tmp_with_head(content: Option<&str>) -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        if let Some(head) = content {
            let git = dir.path().join(".git");
            fs::create_dir(&git).unwrap();
            fs::write(git.join("HEAD"), head).unwrap();
        }
        let path = dir.path().to_string_lossy().into_owned();
        (dir, path)
    }

    #[test_case(Some("ref: refs/heads/feature/foo\n"), Some("feature/foo") ; "regular_ref")]
    #[test_case(Some("abc1234deadbeef\n"),            Some("abc1234")      ; "detached_head")]
    #[test_case(None,                                 None                 ; "no_git_dir")]
    fn detect_branch_cases(head: Option<&str>, expected: Option<&str>) {
        let (_dir, path) = tmp_with_head(head);
        assert_eq!(detect_branch(&path), expected.map(String::from));
    }

    #[test]
    fn detect_branch_from_subdirectory() {
        let (_dir, path) = tmp_with_head(Some("ref: refs/heads/main\n"));
        let sub = Path::new(&path).join("sub");
        fs::create_dir(&sub).unwrap();
        assert_eq!(
            detect_branch(&sub.to_string_lossy()),
            Some("main".to_string())
        );
    }

    /// Once the flash is gone nothing clears the debt, so only the tick that
    /// removes it may report a change, or the loop never settles. The two
    /// lifetimes stand in for time passing: rewinding an `Instant` by an hour
    /// panics on a machine that booted less than an hour ago.
    #[test_case(false, FLASH_TTL      => Dirty::NO  ; "no_flash")]
    #[test_case(true,  FLASH_TTL      => Dirty::NO  ; "flash_still_visible")]
    #[test_case(true,  Duration::ZERO => Dirty::YES ; "flash_expired")]
    fn clear_expired_hint_owes_the_frame_only_once(flashing: bool, ttl: Duration) -> Dirty {
        let mut bar = StatusBar::new(ttl);
        if flashing {
            bar.flash(FLASH_MSG.into());
        }

        let first = bar.clear_expired_hint();
        assert_eq!(bar.clear_expired_hint(), Dirty::NO, "{QUIET}");
        first
    }

    /// The watcher fires for any write near `.git/HEAD`, most of which leave
    /// the branch alone, so repainting on each one means a repaint per commit,
    /// stash and index refresh while a build touches the repo. Either way the
    /// poll has to leave the bounded channel empty, or the watcher's
    /// `try_send` drops the next real switch.
    #[test_case(false => Dirty::NO  ; "unchanged_branch")]
    #[test_case(true  => Dirty::YES ; "switched_branch")]
    fn poll_branch_update_reports_only_real_changes(stale: bool) -> Dirty {
        let label = cwd_branch_label();
        let (tx, rx) = flume::bounded(1);
        let mut bar = StatusBar::new(FLASH_TTL);
        bar.cwd_branch = if stale {
            STALE_BRANCH.into()
        } else {
            label.clone()
        };
        bar.branch_update_rx = Some(rx);
        tx.send(()).unwrap();

        let dirty = bar.poll_branch_update();
        assert_eq!(bar.cwd_branch, label);
        assert!(
            tx.try_send(()).is_ok(),
            "a full channel makes the watcher drop the next switch"
        );
        dirty
    }

    #[test]
    fn clear_flash_removes_flash() {
        let mut bar = StatusBar::new(Duration::from_secs(999));
        bar.flash("Copied".into());
        bar.clear_flash();
        assert!(bar.flash.is_none());
    }
}
