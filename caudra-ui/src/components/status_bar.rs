use std::borrow::Cow;
use std::path::Path;
use std::time::{Duration, Instant};

use super::command::ChatScope;
use super::{RetryInfo, Status, escape_terminal_controls, hover_style};

use crate::animation::spinner_frame;
use crate::theme;

use caudra_providers::format_tokens;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::repaint::{Cadence, Dirty};
use caudra_agent::GoalSnapshot;
use caudra_workflow::{RunSnapshot, RunStatus};

const TRUNCATE_PREFIX: &str = "..";
const CWD_MODEL_SEPARATOR: &str = "  ";
const BACK_TO_MAIN_LABEL: &str = "[< Main]";
/// Replaces the countdown under the pointer: the control has to say what a
/// click does, and the seconds left stop mattering once you mean to skip them.
const RETRY_NOW_LABEL: &str = " · retry now";
const FAST_LABEL: &str = " [fast]";
const WORKFLOW_PREFIX: &str = " [wf:";
const WORKFLOW_WAITING_SEPARATOR: &str = "+";
const WORKFLOW_PHASE_SEPARATOR: &str = " \u{b7} ";
const WORKFLOW_SUFFIX: &str = "]";
const YOLO_LABEL: &str = " [yolo]";
const YOLO_SHORT_LABEL: &str = " [!]";
/// Says the transcript has stopped following, and clicking it starts again.
const AUTO_SCROLL_PAUSED_LABEL: &str = "auto-scroll paused";
/// A level narrower than this is already its own shortest unambiguous form, so
/// squeezing it would trade legibility for a single column.
const THINKING_SHORT_FLOOR: usize = 4;
/// Columns a squeezed level keeps. Two, because the catalog's levels collide on
/// one - `minimal`, `medium` and `max` all lead with `m` - and separate on two.
const THINKING_SHORT_WIDTH: usize = 2;
/// A chip's leading space and its two brackets, which no tier sheds.
const CHIP_OVERHEAD: usize = 3;
const BRACKET_WIDTH: usize = 2;
/// Enough for `[.]`, so a bar too narrow to name the model still offers the
/// control that changes it.
const CLICKABLE_MODEL_FLOOR: usize = 3;
const PLAIN_MODEL_FLOOR: usize = 1;
const CHAT_NAME_MAX_WIDTH: usize = 24;
const CHAT_NAME_WIDTH_DIVISOR: usize = 4;
const MARQUEE_STEP: Duration = Duration::from_millis(120);
const MARQUEE_PAUSE: Duration = Duration::from_millis(600);
/// Marks a figure a subscription already covers. One column is all the bar can
/// spare to say the number is a price rather than a bill.
const NOT_BILLED_MARK: &str = "~";
/// Joins the model a pending mode switch leaves to the one it arrives on,
/// matching the glyph the mode label uses for the same switch.
const MODEL_TRANSITION_ARROW: &str = "\u{2192}";

/// What the bar says about the session's runs: the chip that names what is
/// going on, and the count it falls back to when the columns run out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowChip {
    pub named: String,
    pub counts: String,
}

/// `[wf: deep-research · Research 2/4]` for the one run working now,
/// `[wf:2+1 · Research]` once there are several or some are parked waiting on
/// someone, with the newest run's phase; `[wf:2+1]` is the count alone.
/// Nothing while the session has neither. Runs come newest first.
pub fn workflow_chip(runs: &[RunSnapshot]) -> Option<WorkflowChip> {
    let active: Vec<&RunSnapshot> = runs
        .iter()
        .filter(|run| run.status == RunStatus::Active)
        .collect();
    let waiting = runs
        .iter()
        .filter(|run| matches!(run.status, RunStatus::Paused | RunStatus::BudgetLimited))
        .count();
    if active.is_empty() && waiting == 0 {
        return None;
    }
    let mut count = active.len().to_string();
    if waiting > 0 {
        count.push_str(WORKFLOW_WAITING_SEPARATOR);
        count.push_str(&waiting.to_string());
    }
    let head = match active.as_slice() {
        [only] if waiting == 0 => format!(" {}", escape_terminal_controls(&only.display_name)),
        _ => count.clone(),
    };
    let mut named = format!("{WORKFLOW_PREFIX}{head}");
    if let Some(run) = active.first()
        && let Some(phase) = &run.phase
    {
        named.push_str(WORKFLOW_PHASE_SEPARATOR);
        named.push_str(&escape_terminal_controls(phase));
        if let Some((at, of)) = run.phase_position() {
            named.push_str(&format!(" {at}/{of}"));
        }
    }
    named.push_str(WORKFLOW_SUFFIX);
    Some(WorkflowChip {
        named,
        counts: format!("{WORKFLOW_PREFIX}{count}{WORKFLOW_SUFFIX}"),
    })
}

/// What to draw in the bar's one cost slot. Billed spend wins the slot when a
/// session mixes the two, because that is the number someone pays; the tilde
/// would otherwise claim the whole figure is notional when part of it is real.
fn spend(billed: Option<f64>, subscription: Option<f64>) -> Option<String> {
    match (billed, subscription) {
        (Some(billed), _) => Some(format!("${}", compact_price(billed))),
        (None, Some(subscription)) => {
            Some(format!("{NOT_BILLED_MARK}${}", compact_price(subscription)))
        }
        (None, None) => None,
    }
}

fn compact_price(value: f64) -> String {
    let fixed = format!("{value:.3}");
    if value > 0.0 && fixed == "0.000" {
        return fixed;
    }
    fixed.trim_end_matches('0').trim_end_matches('.').to_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusBarHitTarget {
    BackToMain,
    Mode,
    Model,
    Thinking,
    Goal,
    Context,
    Usage,
    Workflows,
    Retry,
    ChatName,
    Cwd,
    ResumeAutoScroll,
}

impl StatusBarHitTarget {
    /// Which chat a control acts on, mirroring the [`ChatScope`] of the command
    /// it opens so the bar cannot refuse a click the palette would accept.
    ///
    /// [`Self::Thinking`] is main-only although `/thinking` is [`ChatScope::Any`]:
    /// the command edits a session setting, while the chip renders and cycles
    /// the effective level of the session model, which is not the model a task
    /// runs.
    pub fn scope(self) -> ChatScope {
        match self {
            Self::BackToMain
            | Self::Context
            | Self::Usage
            | Self::Retry
            | Self::ChatName
            | Self::Cwd
            | Self::ResumeAutoScroll => ChatScope::Any,
            Self::Mode | Self::Model | Self::Thinking | Self::Goal | Self::Workflows => {
                ChatScope::MainOnly
            }
        }
    }

    pub fn accepts_click(self) -> bool {
        !matches!(self, Self::ChatName | Self::Cwd)
    }
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
    /// Where auto-compaction fires, as a share of the window. `None` when it is
    /// switched off and the window is the only limit that matters.
    pub compaction_border: Option<u32>,
    pub show_global: bool,
}

/// The mode label at both widths. The bar draws one of them, and the choice
/// belongs here rather than at build time because the label sits on the left and
/// every column it takes is a column the right side does not get.
pub struct ModeLabel {
    pub full: Cow<'static, str>,
    pub short: Cow<'static, str>,
    pub style: Style,
}

impl ModeLabel {
    fn full(&self) -> &Cow<'static, str> {
        &self.full
    }

    fn short(&self) -> &Cow<'static, str> {
        &self.short
    }
}

pub struct StatusBarContext<'a> {
    pub status: &'a Status,
    pub mode: ModeLabel,
    pub model_id: &'a str,
    /// The model the pending mode switch leaves behind, drawn as
    /// `[leaving\u{2192}arriving]`. `None` once the switch has settled, which is
    /// every frame where the mode label names a mode rather than a transition.
    pub pending_model: Option<Cow<'a, str>>,
    pub stats: UsageStats,
    pub auto_scroll: bool,
    pub chat_name: Option<&'a str>,
    pub main_chat: bool,
    pub retry_info: Option<&'a RetryInfo>,
    /// The effective level alone (`off`, `xhigh`, `8192`), drawn directly as a
    /// compact chip such as `[xhigh]`.
    pub thinking: Option<Cow<'static, str>>,
    pub fast: bool,
    /// Already rendered by [`workflow_chip`], so fitting the bar measures
    /// strings rather than formatting one per rung.
    pub workflows: Option<WorkflowChip>,
    pub yolo: bool,
    pub restoring: bool,
    pub goal: Option<&'a GoalSnapshot>,
    /// The composer is running a shell line, so the chip names bash rather than
    /// a mode and there is nothing for a click to toggle.
    pub bash_input: bool,
    pub hovered: Option<StatusBarHitTarget>,
    pub hover_hint: Option<&'a str>,
}

/// `[anthropic/claude-opus-5]`, then the last path segment, then whatever the
/// leftover columns hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelTier {
    Full,
    Leaf,
    Chopped,
}

/// `[wf: deep-research · Research 2/4]`, squeezed to `[wf:1]`, or nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowTier {
    Named,
    Counts,
    Hidden,
}

/// `[yolo]` spelled out, squeezed to `[!]`, or nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum YoloTier {
    Named,
    Sigil,
    Hidden,
}

/// `[xhigh]`, squeezed to `[xh]`, or nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThinkingTier {
    Full,
    Short,
    Hidden,
}

impl ThinkingTier {
    /// A prefix rather than a table, so a level a model declared itself squeezes
    /// the same way as one from the catalog. Two arms pass the level through
    /// whatever the pressure: a name already short enough that cutting it would
    /// buy a column and cost a word, and a token budget, whose leading digits
    /// would read as a budget orders of magnitude smaller.
    fn label(self, level: &str) -> Option<&str> {
        match self {
            Self::Hidden => None,
            Self::Full => Some(level),
            Self::Short if level.width() < THINKING_SHORT_FLOOR => Some(level),
            Self::Short if level.bytes().all(|byte| byte.is_ascii_digit()) => Some(level),
            Self::Short => {
                let mut used = 0;
                let mut end = 0;
                for (index, character) in level.char_indices() {
                    let width = character.width().unwrap_or(0);
                    if used + width > THINKING_SHORT_WIDTH {
                        break;
                    }
                    used += width;
                    end = index + character.len_utf8();
                }
                Some(&level[..end])
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextTier {
    /// `12k/200k (6%/90%)`.
    Counts,
    /// `6%/90%`.
    Percent,
    Hidden,
}

/// One step down the ladder. Every step either abbreviates something or drops
/// it, so the bar's width falls monotonically and the search terminates.
#[derive(Debug, Clone, Copy)]
enum Reduction {
    LeafModel,
    DropGlobalSpend,
    DropCompactionBorder,
    CompactContext,
    ShortThinking,
    ShortWorkflows,
    ShortYolo,
    DropTransition,
    DropSpend,
    DropContext,
    DropFast,
    DropWorkflows,
    DropThinking,
    ChopModel,
    DropYolo,
}

/// A provider prefix is the first thing pressure takes: the model leaf carries
/// the useful identity, and the recovered columns keep every other full tier.
/// Yolo goes last because a session that skips permission prompts has to say so
/// at any width that can hold three columns. The pending pair goes early: once
/// the chips are already abbreviating, the model the next turn runs on is worth
/// more than the one it is leaving.
///
/// The three abbreviations go in order of what they cost to read. A reasoning
/// level is a setting, and the chip's presence already carries the load-bearing
/// half of it, so the depth is the cheapest word on the bar to shorten. A
/// workflow phase is live state, but it is also on screen in the workflow view
/// and its counts survive. Yolo is a warning, and `[!]` is the only rung that
/// leaves a chip with no word at all.
const LADDER: [Reduction; 15] = [
    Reduction::LeafModel,
    Reduction::DropGlobalSpend,
    Reduction::DropCompactionBorder,
    Reduction::CompactContext,
    Reduction::ShortThinking,
    Reduction::ShortWorkflows,
    Reduction::ShortYolo,
    Reduction::DropTransition,
    Reduction::DropSpend,
    Reduction::DropContext,
    Reduction::DropFast,
    Reduction::DropWorkflows,
    Reduction::DropThinking,
    Reduction::ChopModel,
    Reduction::DropYolo,
];

/// The counters and prices, rendered once per frame so walking the ladder is
/// pure arithmetic over widths the bar already knows. The counter and the money
/// are separate strings because they are separate controls, and each carries the
/// padding that keeps the two apart once they sit side by side.
struct SpendText {
    counts: String,
    percent: String,
    /// The share of the window auto-compaction fires at, drawn beside the share
    /// in use so the bar says how far off it is rather than only how full it is.
    border: Option<String>,
    over_border: bool,
    spend: Option<String>,
    global: Option<String>,
}

impl SpendText {
    fn new(stats: &UsageStats) -> Self {
        let share = |tokens: u32| {
            if stats.context_window == 0 {
                return 0;
            }
            (f64::from(tokens) / f64::from(stats.context_window) * 100.0) as u32
        };
        let pct = share(stats.context_size);
        Self {
            counts: format!(
                "{}/{}",
                format_tokens(stats.context_size),
                format_tokens(stats.context_window),
            ),
            percent: format!("{pct}%"),
            border: stats
                .compaction_border
                .map(|border| format!("{}%", share(border))),
            over_border: stats
                .compaction_border
                .is_some_and(|border| stats.context_size >= border),
            spend: spend(stats.cost, stats.subscription_cost),
            global: spend(stats.global_cost, stats.global_subscription_cost)
                .filter(|_| stats.show_global)
                .map(|global| format!("\u{03a3}{global}")),
        }
    }
}

/// Which tier each slot of the right-hand side settled on. Widths and spans
/// both read from this, so a chip cannot be measured as one size and drawn as
/// another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fit {
    spend: bool,
    global_spend: bool,
    context: ContextTier,
    compaction_border: bool,
    thinking: ThinkingTier,
    model: ModelTier,
    transition: bool,
    fast: bool,
    workflows: WorkflowTier,
    yolo: YoloTier,
}

impl Fit {
    const FULL: Self = Self {
        spend: true,
        global_spend: true,
        context: ContextTier::Counts,
        compaction_border: true,
        thinking: ThinkingTier::Full,
        model: ModelTier::Full,
        transition: true,
        fast: true,
        workflows: WorkflowTier::Named,
        yolo: YoloTier::Named,
    };

    fn apply(&mut self, step: Reduction) {
        match step {
            Reduction::LeafModel => self.model = ModelTier::Leaf,
            Reduction::DropGlobalSpend => self.global_spend = false,
            Reduction::DropCompactionBorder => self.compaction_border = false,
            Reduction::CompactContext => self.context = ContextTier::Percent,
            Reduction::ShortThinking => self.thinking = ThinkingTier::Short,
            Reduction::ShortWorkflows => self.workflows = WorkflowTier::Counts,
            Reduction::ShortYolo => self.yolo = YoloTier::Sigil,
            Reduction::DropTransition => self.transition = false,
            Reduction::DropSpend => self.spend = false,
            Reduction::DropContext => self.context = ContextTier::Hidden,
            Reduction::DropFast => self.fast = false,
            Reduction::DropWorkflows => self.workflows = WorkflowTier::Hidden,
            Reduction::DropThinking => self.thinking = ThinkingTier::Hidden,
            Reduction::ChopModel => self.model = ModelTier::Chopped,
            Reduction::DropYolo => self.yolo = YoloTier::Hidden,
        }
    }

    /// Everything right of the cwd, which takes whatever this leaves behind.
    fn width(self, ctx: &StatusBarContext<'_>, spend: &SpendText, pair: Option<&str>) -> usize {
        self.model_width(ctx, pair) + self.chip_width(ctx) + self.spend_width(spend)
    }

    /// Clamped up to the floor so [`Reduction::ChopModel`] can never widen a
    /// short id, which would let the ladder grow instead of shrink.
    fn model_width(self, ctx: &StatusBarContext<'_>, pair: Option<&str>) -> usize {
        let floor = model_floor(ctx);
        if self.model == ModelTier::Chopped {
            return floor;
        }
        let named = self.model_id(ctx, pair).width()
            + usize::from(clickable(ctx, StatusBarHitTarget::Model)) * BRACKET_WIDTH;
        named.max(floor)
    }

    /// What the model slot names: the pending pair while it survives the ladder,
    /// else the selection at whatever length is left. The pair is already two
    /// leaves, so [`Reduction::LeafModel`] has nothing left to take from it.
    fn model_id<'a>(self, ctx: &'a StatusBarContext<'_>, pair: Option<&'a str>) -> &'a str {
        match pair.filter(|_| self.transition) {
            Some(pair) => pair,
            None if self.model == ModelTier::Full => ctx.model_id,
            None => model_leaf(ctx.model_id),
        }
    }

    fn chip_width(self, ctx: &StatusBarContext<'_>) -> usize {
        self.thinking_width(ctx)
            + usize::from(ctx.fast && self.fast) * FAST_LABEL.width()
            + self.workflow_label(ctx).map_or(0, UnicodeWidthStr::width)
            + self.yolo_label(ctx).map_or(0, UnicodeWidthStr::width)
    }

    fn workflow_label<'a>(self, ctx: &'a StatusBarContext<'_>) -> Option<&'a str> {
        let chip = ctx.workflows.as_ref()?;
        match self.workflows {
            WorkflowTier::Named => Some(&chip.named),
            WorkflowTier::Counts => Some(&chip.counts),
            WorkflowTier::Hidden => None,
        }
    }

    fn yolo_label(self, ctx: &StatusBarContext<'_>) -> Option<&'static str> {
        self.yolo.label().filter(|_| ctx.yolo)
    }

    fn thinking_label<'a>(self, ctx: &'a StatusBarContext<'_>) -> Option<&'a str> {
        self.thinking.label(ctx.thinking.as_deref()?)
    }

    fn thinking_width(self, ctx: &StatusBarContext<'_>) -> usize {
        self.thinking_label(ctx)
            .map_or(0, |level| CHIP_OVERHEAD + level.width())
    }

    fn spend_width(self, spend: &SpendText) -> usize {
        self.context_text(spend)
            .map_or(0, |context| context.width())
            + self.money_text(spend).map_or(0, |money| money.width())
    }

    fn context_text(self, spend: &SpendText) -> Option<Cow<'_, str>> {
        let share = match spend.border.as_deref().filter(|_| self.compaction_border) {
            Some(border) => Cow::Owned(format!("{}/{border}", spend.percent)),
            None => Cow::Borrowed(spend.percent.as_str()),
        };
        match self.context {
            ContextTier::Counts => Some(Cow::Owned(format!(" {} ({share})", spend.counts))),
            ContextTier::Percent => Some(Cow::Owned(format!(" {share}"))),
            ContextTier::Hidden => None,
        }
    }

    /// Both figures answer the same question, so they are drawn and hit as one
    /// control rather than as a price with a total stuck to it.
    fn money_text(self, spend: &SpendText) -> Option<Cow<'_, str>> {
        let chat = spend.spend.as_deref().filter(|_| self.spend);
        let session = spend.global.as_deref().filter(|_| self.global_spend);
        match (chat, session) {
            (Some(chat), Some(session)) => Some(Cow::Owned(format!(" {chat} {session}"))),
            (Some(only), None) | (None, Some(only)) => Some(Cow::Owned(format!(" {only}"))),
            (None, None) => None,
        }
    }
}

impl YoloTier {
    fn label(self) -> Option<&'static str> {
        match self {
            Self::Named => Some(YOLO_LABEL),
            Self::Sigil => Some(YOLO_SHORT_LABEL),
            Self::Hidden => None,
        }
    }
}

pub struct StatusBar {
    flash: Option<(String, Instant)>,
    started_at: Instant,
    cwd_branch: String,
    pub flash_duration: Duration,
    branch_update_rx: Option<flume::Receiver<()>>,
    cwd: Option<String>,
    marquee: Marquee,
}

#[derive(Default)]
struct Marquee {
    active: Option<MarqueeState>,
    used: bool,
}

struct MarqueeState {
    target: StatusBarHitTarget,
    source: String,
    started_at: Instant,
}

impl Marquee {
    fn retain(&mut self, target: Option<StatusBarHitTarget>) {
        self.used = false;
        if self.active.as_ref().map(|state| state.target) != target {
            self.active = None;
        }
    }

    fn render(
        &mut self,
        target: StatusBarHitTarget,
        source: &str,
        width: usize,
        fallback: Cow<'_, str>,
        hovered: bool,
    ) -> Cow<'static, str> {
        if !hovered || source.width() <= width {
            if self
                .active
                .as_ref()
                .is_some_and(|state| state.target == target)
            {
                self.active = None;
            }
            return Cow::Owned(fallback.into_owned());
        }
        let changed = self
            .active
            .as_ref()
            .is_none_or(|state| state.target != target || state.source != source);
        if changed {
            self.active = Some(MarqueeState {
                target,
                source: source.to_owned(),
                started_at: Instant::now(),
            });
        }
        self.used = true;
        let elapsed = self
            .active
            .as_ref()
            .map_or(Duration::ZERO, |state| state.started_at.elapsed());
        Cow::Owned(marquee_window(source, width, elapsed))
    }

    fn active(&self) -> bool {
        self.active.is_some()
    }

    fn finish_frame(&mut self) {
        if !self.used {
            self.active = None;
        }
    }
}

impl StatusBar {
    pub fn new(flash_duration: Duration, cwd: &str, remote: bool) -> Self {
        Self {
            flash: None,
            started_at: Instant::now(),
            cwd_branch: if remote {
                cwd.to_owned()
            } else {
                cwd_branch_label(cwd)
            },
            flash_duration,
            branch_update_rx: (!remote).then(|| spawn_branch_watcher(cwd)).flatten(),
            cwd: (!remote).then(|| cwd.to_owned()),
            marquee: Marquee::default(),
        }
    }

    pub fn flash(&mut self, msg: String) {
        self.flash = Some((msg, Instant::now()));
    }

    #[cfg(test)]
    pub fn flash_text(&self) -> Option<&str> {
        self.flash.as_ref().map(|(s, _)| s.as_str())
    }

    pub fn refresh_cwd(&mut self, cwd: &str) {
        self.cwd_branch = cwd_branch_label(cwd);
        self.branch_update_rx = spawn_branch_watcher(cwd);
        self.cwd = Some(cwd.to_owned());
    }

    pub fn set_remote_cwd(&mut self, cwd: String) {
        self.cwd_branch = cwd;
        self.branch_update_rx = None;
        self.cwd = None;
    }

    pub fn poll_branch_update(&mut self) -> Dirty {
        let Some(rx) = &self.branch_update_rx else {
            return Dirty::NO;
        };
        if rx.try_iter().next().is_none() {
            return Dirty::NO;
        }
        let Some(cwd) = &self.cwd else {
            return Dirty::NO;
        };
        let branch = cwd_branch_label(cwd);
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
    pub fn cadence(
        &self,
        status: &Status,
        restoring: bool,
        retrying: bool,
        goal_active: bool,
    ) -> Cadence {
        Cadence::any([
            Cadence::when(
                *status == Status::Streaming || restoring || retrying,
                Cadence::SPINNER,
            ),
            Cadence::when(goal_active, Cadence::CLOCK),
            Cadence::when(self.marquee.active(), Cadence::due(MARQUEE_STEP)),
        ])
    }

    pub fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        ctx: &StatusBarContext,
    ) -> Vec<StatusBarHit> {
        self.marquee.retain(ctx.hovered.filter(|target| {
            matches!(
                target,
                StatusBarHitTarget::ChatName | StatusBarHitTarget::Cwd | StatusBarHitTarget::Model
            )
        }));
        if let Some(url) = ctx.hover_hint.filter(|_| self.flash.is_none()) {
            self.marquee.finish_frame();
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

        let mut mode_width = ctx.mode.full().width();
        let mode_offset = left_spans.iter().map(Span::width).sum::<usize>() + " ".width();
        left_spans.push(Span::raw(" "));
        let mode_span = left_spans.len();
        left_spans.push(Span::styled(
            ctx.mode.full().clone(),
            hover_style(
                ctx.mode.style,
                clickable(ctx, StatusBarHitTarget::Mode)
                    && ctx.hovered == Some(StatusBarHitTarget::Mode),
            ),
        ));

        let mut back_offset = (!ctx.main_chat)
            .then(|| left_spans.iter().map(Span::width).sum::<usize>() + " ".width());
        if !ctx.main_chat {
            left_spans.push(Span::raw(" "));
            left_spans.push(Span::styled(
                BACK_TO_MAIN_LABEL,
                hover_style(
                    theme::current().status_notice,
                    ctx.hovered == Some(StatusBarHitTarget::BackToMain),
                ),
            ));
        }

        let mut chat_hit = None;
        if let Some(name) = ctx.chat_name {
            let wrapper_width = usize::from(ctx.main_chat) * BRACKET_WIDTH;
            let critical_right = model_floor(ctx)
                + if ctx.yolo {
                    YOLO_SHORT_LABEL.width()
                } else {
                    0
                };
            let available = (area.width as usize)
                .saturating_sub(left_spans.iter().map(Span::width).sum::<usize>())
                .saturating_sub(critical_right)
                .saturating_sub(1);
            let slot_total = (area.width as usize / CHAT_NAME_WIDTH_DIVISOR)
                .min(CHAT_NAME_MAX_WIDTH)
                .min(available);
            if slot_total > wrapper_width {
                let slot_width = slot_total - wrapper_width;
                let clipped = name.width() > slot_width;
                let fallback = truncate_head(name, slot_width);
                let visible = self.marquee.render(
                    StatusBarHitTarget::ChatName,
                    name,
                    slot_width,
                    fallback,
                    ctx.hovered == Some(StatusBarHitTarget::ChatName),
                );
                let label = if ctx.main_chat {
                    format!("[{visible}]")
                } else {
                    visible.into_owned()
                };
                let offset = left_spans.iter().map(Span::width).sum::<usize>() + 1;
                left_spans.push(Span::raw(" "));
                left_spans.push(Span::styled(label, theme::current().status_dim));
                if clipped {
                    chat_hit = Some((offset, slot_width + wrapper_width));
                }
            }
        }

        let mut resume_hit = (!ctx.auto_scroll).then(|| {
            let offset = left_spans.iter().map(Span::width).sum::<usize>() + " ".width();
            left_spans.push(Span::raw(" "));
            left_spans.push(Span::styled(
                AUTO_SCROLL_PAUSED_LABEL,
                hover_style(
                    theme::current().status_dim,
                    ctx.hovered == Some(StatusBarHitTarget::ResumeAutoScroll),
                ),
            ));
            (offset, AUTO_SCROLL_PAUSED_LABEL.width())
        });

        let mut goal_hit = ctx.goal.map(|goal| {
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
                    clickable(ctx, StatusBarHitTarget::Goal)
                        && ctx.hovered == Some(StatusBarHitTarget::Goal),
                ),
            ));
            (offset, width)
        });

        let mut retry_hit = ctx.retry_info.map(|retry| {
            let hovered = ctx.hovered == Some(StatusBarHitTarget::Retry);
            let countdown = if hovered {
                RETRY_NOW_LABEL.to_owned()
            } else {
                let secs = retry
                    .deadline
                    .saturating_duration_since(Instant::now())
                    .as_secs();
                format!(" · retrying in {secs}s (#{})", retry.attempt)
            };
            let offset = left_spans.iter().map(Span::width).sum::<usize>() + " ".width();
            let width = retry.message.width() + countdown.width();
            left_spans.push(Span::raw(" "));
            left_spans.push(Span::styled(
                retry.message.clone(),
                hover_style(theme::current().status_retry_error, hovered),
            ));
            left_spans.push(Span::styled(
                countdown,
                hover_style(theme::current().status_retry_info, hovered),
            ));
            (offset, width)
        });

        let full_left_width = left_spans.iter().map(Span::width).sum::<usize>();
        let full_budget = (area.width as usize).saturating_sub(full_left_width);
        let short_saving = mode_width.saturating_sub(ctx.mode.short().width());
        let full_rank = right_fit_rank(ctx, full_budget);
        let short_rank = right_fit_rank(ctx, full_budget.saturating_add(short_saving));
        if full_rank > 1 && short_rank > 0 && short_rank < full_rank {
            left_spans[mode_span] = Span::styled(
                ctx.mode.short().clone(),
                hover_style(
                    ctx.mode.style,
                    clickable(ctx, StatusBarHitTarget::Mode)
                        && ctx.hovered == Some(StatusBarHitTarget::Mode),
                ),
            );
            mode_width = ctx.mode.short().width();
            back_offset = back_offset.map(|offset| offset.saturating_sub(short_saving));
            chat_hit = chat_hit.map(|(offset, width)| (offset.saturating_sub(short_saving), width));
            resume_hit =
                resume_hit.map(|(offset, width)| (offset.saturating_sub(short_saving), width));
            goal_hit = goal_hit.map(|(offset, width)| (offset.saturating_sub(short_saving), width));
            retry_hit =
                retry_hit.map(|(offset, width)| (offset.saturating_sub(short_saving), width));
        }

        let mut right_spans = Vec::new();
        let mut right_hits = Vec::new();

        match ctx.status {
            Status::Error { message: e, .. } => {
                left_spans.push(Span::styled(format!(" {e}"), theme::current().error));
            }
            _ => {
                let left_width = left_spans.iter().map(Span::width).sum::<usize>();
                let side = right_side_animated(
                    ctx,
                    &self.cwd_branch,
                    (area.width as usize).saturating_sub(left_width),
                    Some(&mut self.marquee),
                );
                right_spans = side.spans;
                right_hits = side.hits;
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
            ctx,
            left_area,
            mode_offset,
            mode_width,
            StatusBarHitTarget::Mode,
        );
        push_hit(
            &mut hits,
            ctx,
            left_area,
            back_offset.unwrap_or_default(),
            BACK_TO_MAIN_LABEL.width(),
            StatusBarHitTarget::BackToMain,
        );
        for (target, offset, width) in right_hits {
            push_hit(&mut hits, ctx, right_area, offset, width, target);
        }
        if let Some((offset, width)) = goal_hit {
            push_hit(
                &mut hits,
                ctx,
                left_area,
                offset,
                width,
                StatusBarHitTarget::Goal,
            );
        }
        if let Some((offset, width)) = retry_hit {
            push_hit(
                &mut hits,
                ctx,
                left_area,
                offset,
                width,
                StatusBarHitTarget::Retry,
            );
        }
        if let Some((offset, width)) = chat_hit {
            push_hit(
                &mut hits,
                ctx,
                left_area,
                offset,
                width,
                StatusBarHitTarget::ChatName,
            );
        }
        if let Some((offset, width)) = resume_hit {
            push_hit(
                &mut hits,
                ctx,
                left_area,
                offset,
                width,
                StatusBarHitTarget::ResumeAutoScroll,
            );
        }
        self.marquee.finish_frame();
        hits
    }
}

/// The right-hand half of the bar, already fitted to `budget`. Hits are measured
/// on the glyphs that were drawn, so a control cannot claim columns a shorter
/// tier never used.
struct RightSide<'a> {
    spans: Vec<Span<'a>>,
    hits: Vec<(StatusBarHitTarget, usize, usize)>,
}

/// Draws one control: the padding stays plain so a hover reverses the figure
/// alone, and the returned hit covers exactly the glyphs that were highlighted.
fn push_control<'a>(chips: &mut Vec<Span<'a>>, text: &str, style: Style) -> Option<(usize, usize)> {
    let offset = chips.iter().map(Span::width).sum::<usize>();
    let body = text.trim();
    if body.is_empty() {
        chips.push(Span::raw(text.to_owned()));
        return None;
    }
    let lead = text.len() - text.trim_start().len();
    let tail = lead + body.len();
    if lead > 0 {
        chips.push(Span::raw(text[..lead].to_owned()));
    }
    chips.push(Span::styled(body.to_owned(), style));
    if tail < text.len() {
        chips.push(Span::raw(text[tail..].to_owned()));
    }
    Some((offset + lead, body.width()))
}

/// Walks [`LADDER`] until the fixed chips and the model fit, then hands the cwd
/// whatever is left. The cwd goes last because the model names what answers you
/// and the path is usually already in the shell prompt.
#[cfg(test)]
fn right_side<'a>(
    ctx: &'a StatusBarContext<'_>,
    cwd_label: &'a str,
    budget: usize,
) -> RightSide<'a> {
    right_side_animated(ctx, cwd_label, budget, None)
}

fn right_side_animated<'a>(
    ctx: &'a StatusBarContext<'_>,
    cwd_label: &'a str,
    budget: usize,
    mut marquee: Option<&mut Marquee>,
) -> RightSide<'a> {
    let spend = SpendText::new(&ctx.stats);
    let pair = model_pair(ctx);
    let pair = pair.as_deref();
    let (fit, _) = fit_right(ctx, &spend, pair, budget);

    let mut chips = Vec::new();
    let mut chip_hits = Vec::new();
    let mut control = |chips: &mut Vec<Span<'a>>, target, text: &str, style| {
        let hovered = clickable(ctx, target) && ctx.hovered == Some(target);
        if let Some(hit) = push_control(chips, text, hover_style(style, hovered)) {
            chip_hits.push((target, hit.0, hit.1));
        }
    };

    if let Some(level) = fit.thinking_label(ctx) {
        let label = format!(" [{level}]");
        control(
            &mut chips,
            StatusBarHitTarget::Thinking,
            &label,
            control_style(ctx, StatusBarHitTarget::Thinking),
        );
    }
    if ctx.fast && fit.fast {
        chips.push(Span::styled(FAST_LABEL, theme::current().status_dim));
    }
    if let Some(label) = fit.workflow_label(ctx) {
        control(
            &mut chips,
            StatusBarHitTarget::Workflows,
            label,
            control_style(ctx, StatusBarHitTarget::Workflows),
        );
    }
    if let Some(label) = fit.yolo_label(ctx) {
        chips.push(Span::styled(label, theme::current().error));
    }
    let counters = Style::new().fg(theme::current().foreground);
    if let Some(text) = fit.context_text(&spend) {
        // Past the border the next turn compacts, which is worth saying even at
        // a width that had to drop the border itself.
        let style = if spend.over_border {
            theme::current().todo_in_progress
        } else {
            counters
        };
        control(&mut chips, StatusBarHitTarget::Context, &text, style);
    }
    if let Some(text) = fit.money_text(&spend) {
        control(&mut chips, StatusBarHitTarget::Usage, &text, counters);
    }

    let residue = budget.saturating_sub(chips.iter().map(Span::width).sum::<usize>());
    let model_source = fit.model_id(ctx, pair);
    let model = if clickable(ctx, StatusBarHitTarget::Model)
        && residue >= model_floor(ctx)
        && model_source.width() + BRACKET_WIDTH > residue
        && ctx.hovered == Some(StatusBarHitTarget::Model)
    {
        let width = residue - BRACKET_WIDTH;
        let fallback = truncate_tail(model_source, width);
        let inner = marquee.as_deref_mut().map_or(fallback.clone(), |marquee| {
            marquee.render(
                StatusBarHitTarget::Model,
                model_source,
                width,
                fallback,
                true,
            )
        });
        Cow::Owned(format!("[{inner}]"))
    } else {
        model_text(ctx, fit, residue, pair)
    };
    let separator = if model.is_empty() {
        ""
    } else {
        CWD_MODEL_SEPARATOR
    };
    let cwd_static = cwd_text(
        cwd_label,
        residue
            .saturating_sub(model.width())
            .saturating_sub(separator.width()),
    );
    let cwd = if !cwd_static.is_empty()
        && cwd_static != cwd_label
        && ctx.hovered == Some(StatusBarHitTarget::Cwd)
    {
        marquee
            .as_mut()
            .map_or(Cow::Borrowed(cwd_static), |marquee| {
                (*marquee).render(
                    StatusBarHitTarget::Cwd,
                    cwd_label,
                    cwd_static.width(),
                    Cow::Borrowed(cwd_static),
                    true,
                )
            })
    } else {
        Cow::Borrowed(cwd_static)
    };
    let separator = if cwd.is_empty() { "" } else { separator };

    let model_offset = cwd.width() + separator.width();
    let model_width = model.width();
    let cwd_width = cwd.width();
    let mut spans = Vec::with_capacity(chips.len() + 3);
    spans.push(Span::styled(cwd, theme::current().status_dim));
    spans.push(Span::raw(separator));
    spans.push(Span::styled(
        model,
        hover_style(
            control_style(ctx, StatusBarHitTarget::Model),
            clickable(ctx, StatusBarHitTarget::Model)
                && ctx.hovered == Some(StatusBarHitTarget::Model),
        ),
    ));
    spans.append(&mut chips);

    let chips_at = model_offset + model_width;
    let mut hits = Vec::new();
    if cwd_static != cwd_label && !cwd_static.is_empty() {
        hits.push((StatusBarHitTarget::Cwd, 0, cwd_width));
    }
    hits.push((StatusBarHitTarget::Model, model_offset, model_width));
    hits.extend(
        chip_hits
            .into_iter()
            .map(|(target, offset, width)| (target, chips_at + offset, width)),
    );
    RightSide { spans, hits }
}

/// Both sides drop the provider they share. Two providers stay because a model
/// served by both is a switch the leaves alone would draw as a no-op.
fn model_pair(ctx: &StatusBarContext<'_>) -> Option<String> {
    ctx.pending_model.as_deref().map(|leaving| {
        let shared = model_provider(leaving) == model_provider(ctx.model_id);
        let named = |id| if shared { model_leaf(id) } else { id };
        format!(
            "{}{MODEL_TRANSITION_ARROW}{}",
            named(leaving),
            named(ctx.model_id)
        )
    })
}

fn fit_right(
    ctx: &StatusBarContext<'_>,
    spend: &SpendText,
    pair: Option<&str>,
    budget: usize,
) -> (Fit, usize) {
    let mut fit = Fit::FULL;
    for (index, step) in LADDER.into_iter().enumerate() {
        if fit.width(ctx, spend, pair) <= budget {
            return (fit, index);
        }
        fit.apply(step);
    }
    (fit, LADDER.len())
}

fn right_fit_rank(ctx: &StatusBarContext<'_>, budget: usize) -> usize {
    let spend = SpendText::new(&ctx.stats);
    let pair = model_pair(ctx);
    fit_right(ctx, &spend, pair.as_deref(), budget).1
}

/// Whether a control answers the pointer in the chat being drawn. Every
/// enablement question the bar and [`crate::app::App`] ask goes through here,
/// so the glyphs, the hit rects and the click cannot disagree.
fn clickable(ctx: &StatusBarContext<'_>, target: StatusBarHitTarget) -> bool {
    if !target.accepts_click() {
        return false;
    }
    match target {
        StatusBarHitTarget::BackToMain => !ctx.main_chat,
        StatusBarHitTarget::Mode => ctx.main_chat && !ctx.bash_input,
        _ => ctx.main_chat || target.scope() == ChatScope::Any,
    }
}

fn hoverable(ctx: &StatusBarContext<'_>, target: StatusBarHitTarget) -> bool {
    matches!(
        target,
        StatusBarHitTarget::ChatName | StatusBarHitTarget::Cwd
    ) || clickable(ctx, target)
}

fn control_style(ctx: &StatusBarContext<'_>, target: StatusBarHitTarget) -> Style {
    if clickable(ctx, target) {
        theme::current().status_notice
    } else {
        theme::current().status_dim
    }
}

fn model_floor(ctx: &StatusBarContext<'_>) -> usize {
    if clickable(ctx, StatusBarHitTarget::Model) {
        CLICKABLE_MODEL_FLOOR
    } else {
        PLAIN_MODEL_FLOOR
    }
}

fn model_text<'a>(
    ctx: &'a StatusBarContext<'_>,
    fit: Fit,
    budget: usize,
    pair: Option<&str>,
) -> Cow<'a, str> {
    match pair.filter(|_| fit.transition) {
        // The pair is assembled per frame and outlives neither the context nor
        // this call, so what survives into the span has to be owned.
        Some(pair) => Cow::Owned(model_fitted(ctx, pair, budget).into_owned()),
        None => model_fitted(ctx, fit.model_id(ctx, None), budget),
    }
}

fn model_fitted<'a>(ctx: &StatusBarContext<'_>, id: &'a str, budget: usize) -> Cow<'a, str> {
    if clickable(ctx, StatusBarHitTarget::Model) {
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

/// Exactly what [`model_leaf`] drops, so the two agree on where an id ends and
/// cannot disagree about whether a pair shares a provider.
fn model_provider(id: &str) -> Option<&str> {
    match id.rsplit_once('/') {
        Some((provider, leaf)) if !leaf.is_empty() => Some(provider),
        _ => None,
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
    ctx: &StatusBarContext<'_>,
    area: Rect,
    offset: usize,
    width: usize,
    target: StatusBarHitTarget,
) {
    let (Ok(offset), Ok(width)) = (u16::try_from(offset), u16::try_from(width)) else {
        return;
    };
    let Some(x) = area.x.checked_add(offset) else {
        return;
    };
    if hoverable(ctx, target)
        && area.height > 0
        && width > 0
        && x.saturating_add(width) <= area.right()
    {
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

fn marquee_window(text: &str, width: usize, elapsed: Duration) -> String {
    if width == 0 {
        return String::new();
    }
    if text.width() <= width {
        let mut out = text.to_owned();
        out.push_str(&" ".repeat(width - text.width()));
        return out;
    }
    let mut starts = vec![0];
    let mut column = 0;
    for (byte, grapheme) in text.grapheme_indices(true) {
        if byte > 0 && column <= text.width() - width {
            starts.push(byte);
        }
        column += grapheme.width();
    }
    let pause_steps = MARQUEE_PAUSE.as_millis() / MARQUEE_STEP.as_millis();
    let travel_steps = starts.len().saturating_sub(1) as u128;
    let cycle = pause_steps * 2 + travel_steps * 2;
    let tick = elapsed.as_millis() / MARQUEE_STEP.as_millis() % cycle;
    let frame = if tick < pause_steps {
        0
    } else if tick < pause_steps + travel_steps {
        tick - pause_steps
    } else if tick < pause_steps * 2 + travel_steps {
        travel_steps
    } else {
        cycle - tick
    } as usize;

    let mut used = 0;
    let mut out = String::new();
    for grapheme in text[starts[frame]..].graphemes(true) {
        let grapheme_width = grapheme.width();
        if used + grapheme_width > width {
            break;
        }
        out.push_str(grapheme);
        used += grapheme_width;
    }
    out.push_str(&" ".repeat(width - used));
    out
}

fn truncate_head(s: &str, max_width: usize) -> Cow<'_, str> {
    if s.width() <= max_width {
        return Cow::Borrowed(s);
    }
    if max_width <= TRUNCATE_PREFIX.width() {
        return Cow::Owned(".".repeat(max_width));
    }
    let budget = max_width - TRUNCATE_PREFIX.width();
    let mut used = 0;
    let mut end = 0;
    for (index, character) in s.char_indices() {
        let width = character.width().unwrap_or(0);
        if used + width > budget {
            break;
        }
        used += width;
        end = index + character.len_utf8();
    }
    Cow::Owned(format!("{}{TRUNCATE_PREFIX}", &s[..end]))
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

fn cwd_branch_label(cwd: &str) -> String {
    let label = collapse_home(cwd);
    match detect_branch(cwd) {
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

fn spawn_branch_watcher(cwd: &str) -> Option<flume::Receiver<()>> {
    use notify::{RecursiveMode, Watcher};

    let git_dir = find_git_dir(Path::new(cwd))?;
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
    use caudra_storage::thinking::EFFORT_LEVELS;
    use tempfile::TempDir;
    use test_case::test_case;

    const FLASH_TTL: Duration = Duration::from_secs(3600);
    const FLASH_MSG: &str = "Copied";
    const STALE_BRANCH: &str = "/nowhere:gone";
    const BAR_WIDTH: u16 = 120;
    const MODE_LABEL: &str = "[BUILD]";
    const MODE_SHORT_LABEL: &str = "[B]";
    const EXPECTED_MODE_HIT: &str = "the mode label is always clickable here";
    const MODEL_ID: &str = "test-model";
    const THINKING_LEVEL: &str = "off";
    const LADDER_MODEL_ID: &str = "anthropic/claude-opus-5";
    const LADDER_MODEL_LEAF: &str = "claude-opus-5";
    const LEAVING_MODEL_ID: &str = "anthropic/claude-sonnet-5";
    const MODEL_PAIR: &str = "claude-sonnet-5\u{2192}claude-opus-5";
    /// The ladder model under another provider, so the leaves alone cannot tell
    /// the two apart.
    const REHOSTED_MODEL_ID: &str = "openrouter/claude-opus-5";
    const REHOSTED_PAIR: &str = "openrouter/claude-opus-5\u{2192}anthropic/claude-opus-5";
    const PAIR_MISSING: &str = "a pending switch must name both models";
    const PROVIDER_DROPPED: &str = "a switch that only changes provider must name both";
    const PROVIDER_KEPT: &str = "a pending switch must spend no columns on the provider";
    const PAIR_KEPT: &str = "the pair must go before the model it is arriving on is shortened";
    const PAIR_RETURNED: &str = "a narrower bar must never show the pair again";
    const LADDER_THINKING: &str = "xhigh";
    const LADDER_WORKFLOWS_ACTIVE: usize = 2;
    const LADDER_WORKFLOWS_WAITING: usize = 1;
    const LADDER_WORKFLOW_CHIP: &str = "[wf:2+1 \u{b7} Research 2/3]";
    const LADDER_WORKFLOW_COUNTS: &str = "[wf:2+1]";
    const RUN_NAME: &str = "deep-research";
    const RUN_PHASE: &str = "Research";
    const SHORT_WORKFLOWS_MSG: &str = "a squeezed workflow chip must fall back to its counts";
    const LADDER_CWD: &str = "~/projects/caudra:main";
    const PERCENT_MARK: &str = "%";
    /// A budget that keeps the compact thinking control on a squeezed bar.
    const SHORT_THINKING_BUDGET: usize = 36;
    const FULL_THINKING_CHIP: &str = "[xhigh]";
    const SHORT_THINKING_CHIP: &str = "[xh]";
    const LEVEL_COLLISION_MSG: &str = "two catalog levels squeeze to the same spelling";
    /// Room for every rung, so both figures are on screen at their full tier,
    /// the counter's border included.
    const WIDE_BUDGET: usize = 124;
    /// 90% of [`crate::components::TEST_CONTEXT_WINDOW`], the share a window
    /// that already excludes its output allowance compacts at.
    const COMPACTION_BORDER: u32 = 180_000;
    const COUNTS_GLYPHS: &str = "12k/200k (6%/90%)";
    const BARE_COUNTS_GLYPHS: &str = "12k/200k (6%)";
    const MONEY_GLYPHS: &str = "$0.25 \u{03a3}$1.5";
    const MISSING_HIT_MSG: &str = "the control was drawn without a hit";
    const FIGURE_HIT_MSG: &str = "a figure's hit must cover its glyphs and no padding";
    const STALE_HIT_MSG: &str = "a hit outlived the figure it was measured on";
    const OVER_BUDGET_MSG: &str = "the right side claimed more columns than its budget";
    const MONOTONE_MSG: &str = "a narrower bar showed a chip the wider one had dropped";
    const YOLO_LAST_MSG: &str = "yolo must outlive every other chip";
    const SHORT_THINKING_MSG: &str = "a squeezed thinking chip must still own its own glyphs";
    const CONTEXT_SIZE: u32 = 12_000;
    const CHAT_COST: f64 = 0.25;
    const CHAT_COST_TEXT: &str = "$0.25";
    const SESSION_COST: f64 = 1.5;
    const SESSION_COST_TEXT: &str = "\u{03a3}$1.5";
    const SIGMA: char = '\u{03a3}';
    const GOAL_CONDITION: &str = "all focused tests pass";
    const GOAL_CHIP_PREFIX: &str = "[goal \u{b7}";
    const RETRY_MESSAGE: &str = "Rate limited: rate_limit_error";
    const RETRY_ATTEMPT: u32 = 3;
    const RETRY_REMAINING: Duration = Duration::from_secs(9);
    /// Asserted without the seconds: the deadline ticks down between
    /// construction and render, so the digit is not the test's business.
    const RETRY_COUNTDOWN_PREFIX: &str = "retrying in";
    const RETRY_ATTEMPT_MARK: &str = "(#3)";
    const MISSING_RETRY_HIT_MSG: &str = "a visible retry countdown must be clickable";
    const MISSING_RESUME_HIT_MSG: &str = "a paused transcript must be resumable from the footer";
    const UNCLICKABLE_LABEL_MSG: &str = "the bar drew the resume label without a hit to click it";

    fn resume_glyphs(text: &str, hit: &StatusBarHit) -> String {
        text.chars()
            .skip(usize::from(hit.area.x))
            .take(usize::from(hit.area.width))
            .collect()
    }

    fn active_goal() -> GoalSnapshot {
        caudra_agent::GoalHandle::default()
            .set(GOAL_CONDITION)
            .unwrap()
    }

    fn run(status: RunStatus, phase: Option<&str>) -> RunSnapshot {
        RunSnapshot {
            run_id: RUN_NAME.into(),
            display_name: RUN_NAME.into(),
            workflow_name: RUN_NAME.into(),
            source_kind: caudra_workflow::SourceKind::Builtin,
            source_path: None,
            objective: None,
            status,
            pause_kind: None,
            pause_message: None,
            revision: 1,
            execution_epoch: 1,
            phase: phase.map(str::to_owned),
            phases: vec!["Plan".into(), RUN_PHASE.into(), "Report".into()],
            phase_history: Vec::new(),
            agent_budget: 8,
            usage: caudra_workflow::RunUsage::default(),
            roster: Vec::new(),
            result: None,
            error: None,
            logs: Vec::new(),
            outbox_pending: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    /// `active` runs in their research phase ahead of `waiting` paused ones.
    fn runs(active: usize, waiting: usize) -> Vec<RunSnapshot> {
        std::iter::repeat_with(|| run(RunStatus::Active, Some(RUN_PHASE)))
            .take(active)
            .chain(std::iter::repeat_with(|| run(RunStatus::Paused, None)).take(waiting))
            .collect()
    }

    fn ladder_workflows() -> Option<WorkflowChip> {
        workflow_chip(&runs(LADDER_WORKFLOWS_ACTIVE, LADDER_WORKFLOWS_WAITING))
    }

    /// Everything the bar's tests vary, defaulting to a plain idle bar at
    /// `BAR_WIDTH`, so each test names only what it actually exercises.
    struct Fixture<'a> {
        width: u16,
        global_cost: Option<f64>,
        show_global: bool,
        yolo: bool,
        hovered: Option<StatusBarHitTarget>,
        hover_hint: Option<&'a str>,
        goal: Option<&'a GoalSnapshot>,
        retry_info: Option<&'a RetryInfo>,
        workflows: Option<WorkflowChip>,
        main_chat: bool,
        model_id: &'a str,
        pending_model: Option<&'a str>,
        chat_name: Option<&'a str>,
        auto_scroll: bool,
    }

    impl Default for Fixture<'_> {
        fn default() -> Self {
            Self {
                width: BAR_WIDTH,
                global_cost: None,
                show_global: false,
                yolo: false,
                hovered: None,
                hover_hint: None,
                goal: None,
                retry_info: None,
                workflows: None,
                main_chat: true,
                model_id: MODEL_ID,
                pending_model: None,
                chat_name: None,
                auto_scroll: true,
            }
        }
    }

    fn render_at(fixture: Fixture<'_>) -> (String, Vec<StatusBarHit>, Vec<Style>) {
        let Fixture {
            width,
            global_cost,
            show_global,
            yolo,
            hovered,
            hover_hint,
            goal,
            retry_info,
            workflows,
            main_chat,
            model_id,
            pending_model,
            chat_name,
            auto_scroll,
        } = fixture;
        let mut bar = StatusBar::new(FLASH_TTL, ".", false);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 1)).unwrap();
        let ctx = StatusBarContext {
            status: &Status::Idle,
            mode: ModeLabel {
                full: MODE_LABEL.into(),
                short: MODE_SHORT_LABEL.into(),
                style: Style::new(),
            },
            model_id,
            pending_model: pending_model.map(Cow::Borrowed),
            stats: UsageStats {
                global_cost,
                global_subscription_cost: None,
                context_size: CONTEXT_SIZE,
                cost: Some(CHAT_COST),
                subscription_cost: None,
                context_window: crate::components::TEST_CONTEXT_WINDOW,
                compaction_border: None,
                show_global,
            },
            auto_scroll,
            chat_name,
            main_chat,
            retry_info,
            thinking: Some(THINKING_LEVEL.into()),
            fast: false,
            workflows,
            yolo,
            restoring: false,
            goal,
            bash_input: false,
            hovered,
            hover_hint,
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
        render_at(Fixture {
            global_cost,
            show_global,
            yolo,
            ..Default::default()
        })
        .0
    }

    /// Every flag on, a long model id and a real path: the widest the bar ever
    /// has to squeeze, so each rung of the ladder gets exercised.
    fn with_ladder_ctx(f: impl FnOnce(&StatusBarContext<'_>)) {
        with_ladder_ctx_leaving(None, f);
    }

    /// The same bar mid mode switch, `leaving` naming the model it is leaving.
    fn with_ladder_ctx_leaving(leaving: Option<&str>, f: impl FnOnce(&StatusBarContext<'_>)) {
        let ctx = StatusBarContext {
            status: &Status::Idle,
            mode: ModeLabel {
                full: MODE_LABEL.into(),
                short: MODE_SHORT_LABEL.into(),
                style: Style::new(),
            },
            model_id: LADDER_MODEL_ID,
            pending_model: leaving.map(Cow::Borrowed),
            stats: UsageStats {
                global_cost: Some(SESSION_COST),
                global_subscription_cost: None,
                context_size: CONTEXT_SIZE,
                cost: Some(CHAT_COST),
                subscription_cost: None,
                context_window: crate::components::TEST_CONTEXT_WINDOW,
                compaction_border: Some(COMPACTION_BORDER),
                show_global: true,
            },
            auto_scroll: true,
            chat_name: None,
            main_chat: true,
            retry_info: None,
            thinking: Some(LADDER_THINKING.into()),
            fast: true,
            workflows: ladder_workflows(),
            yolo: true,
            restoring: false,
            goal: None,
            bash_input: false,
            hovered: None,
            hover_hint: None,
        };
        f(&ctx);
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Chip {
        Thinking,
        Fast,
        Workflows,
        Yolo,
        Context,
    }

    fn side_text(side: &RightSide<'_>) -> String {
        side.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn side_hit(side: &RightSide<'_>, target: StatusBarHitTarget) -> Option<(usize, usize)> {
        side.hits
            .iter()
            .find(|(hit, _, _)| *hit == target)
            .map(|(_, offset, width)| (*offset, *width))
    }

    /// The glyphs a hit claims, read back out of the spans it was measured on.
    fn hit_glyphs(side: &RightSide<'_>, target: StatusBarHitTarget) -> String {
        let (offset, width) = side_hit(side, target).expect(MISSING_HIT_MSG);
        side_text(side).chars().skip(offset).take(width).collect()
    }

    /// Reads the drawn glyphs rather than the [`Fit`], so a tier that measures
    /// one way and draws another still counts as absent.
    fn visible_chips(side: &RightSide<'_>) -> Vec<Chip> {
        let text = side_text(side);
        [
            Chip::Thinking,
            Chip::Fast,
            Chip::Workflows,
            Chip::Yolo,
            Chip::Context,
        ]
        .into_iter()
        .filter(|chip| match chip {
            Chip::Thinking => text.contains(LADDER_THINKING),
            Chip::Fast => text.contains(FAST_LABEL.trim()),
            Chip::Workflows => text.contains(WORKFLOW_PREFIX.trim()),
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

    #[test_case(0, 0 => None ; "nothing_running_draws_nothing")]
    #[test_case(1, 0 => Some((" [wf: deep-research \u{b7} Research 2/3]".to_owned(), " [wf:1]".to_owned())) ; "one_active_run_is_named_with_its_phase")]
    #[test_case(3, 0 => Some((" [wf:3 \u{b7} Research 2/3]".to_owned(), " [wf:3]".to_owned())) ; "several_active_runs_are_counted_with_the_newest_phase")]
    #[test_case(0, 2 => Some((" [wf:0+2]".to_owned(), " [wf:0+2]".to_owned())) ; "waiting_alone_keeps_the_active_count")]
    #[test_case(2, 1 => Some((format!(" {LADDER_WORKFLOW_CHIP}"), format!(" {LADDER_WORKFLOW_COUNTS}"))) ; "both")]
    fn the_workflow_chip_names_the_runs_and_counts_them(
        active: usize,
        waiting: usize,
    ) -> Option<(String, String)> {
        workflow_chip(&runs(active, waiting)).map(|chip| (chip.named, chip.counts))
    }

    #[test]
    fn a_run_without_a_declared_phase_is_named_alone() {
        let chip = workflow_chip(&[run(RunStatus::Active, None)]).unwrap();

        assert_eq!(chip.named, " [wf: deep-research]");
    }

    #[test]
    fn a_wide_bar_draws_the_workflow_chip() {
        with_ladder_ctx(|ctx| {
            let side = right_side(ctx, LADDER_CWD, WIDE_BUDGET);
            assert!(side_text(&side).contains(LADDER_WORKFLOW_CHIP));
        });
    }

    /// Narrowing the bar squeezes the named chip to its counts before it
    /// drops the chip, so the tiers seen walking down are named, then counts,
    /// then nothing, with the counts tier actually visited.
    #[test]
    fn a_narrowing_bar_squeezes_the_workflow_chip_before_dropping_it() {
        with_ladder_ctx(|ctx| {
            let tiers: Vec<WorkflowTier> = (0..=WIDE_BUDGET)
                .rev()
                .map(|budget| {
                    let text = side_text(&right_side(ctx, LADDER_CWD, budget));
                    if text.contains(LADDER_WORKFLOW_CHIP) {
                        WorkflowTier::Named
                    } else if text.contains(LADDER_WORKFLOW_COUNTS) {
                        WorkflowTier::Counts
                    } else {
                        WorkflowTier::Hidden
                    }
                })
                .collect();
            let mut seen = tiers.clone();
            seen.dedup();

            assert_eq!(
                seen,
                vec![
                    WorkflowTier::Named,
                    WorkflowTier::Counts,
                    WorkflowTier::Hidden
                ],
                "{SHORT_WORKFLOWS_MSG}: {tiers:?}"
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
            let (offset, _) = side_hit(&side, StatusBarHitTarget::Thinking).expect(MISSING_HIT_MSG);

            assert_eq!(
                hit_glyphs(&side, StatusBarHitTarget::Thinking),
                SHORT_THINKING_CHIP,
                "{SHORT_THINKING_MSG}"
            );
            assert_eq!(
                side_text(&side).chars().nth(offset - 1),
                Some(' '),
                "{SHORT_THINKING_MSG}"
            );
        });
    }

    /// Both figures open a view, so each needs a target of its own measured on
    /// the glyphs alone. Padding inside a hit would hand clicks on empty
    /// columns to a modal.
    #[test_case(StatusBarHitTarget::Context, COUNTS_GLYPHS ; "counter_opens_context")]
    #[test_case(StatusBarHitTarget::Usage, MONEY_GLYPHS    ; "money_opens_usage")]
    #[test_case(StatusBarHitTarget::Workflows, LADDER_WORKFLOW_CHIP ; "workflow_chip_opens_runs")]
    fn a_figure_is_hit_on_its_own_glyphs(target: StatusBarHitTarget, expected: &str) {
        with_ladder_ctx(|ctx| {
            let side = right_side(ctx, LADDER_CWD, WIDE_BUDGET);
            let (offset, _) = side_hit(&side, target).expect(MISSING_HIT_MSG);

            assert_eq!(hit_glyphs(&side, target), expected, "{FIGURE_HIT_MSG}");
            assert_eq!(
                side_text(&side).chars().nth(offset - 1),
                Some(' '),
                "{FIGURE_HIT_MSG}"
            );
        });
    }

    /// The session total sits beside the chat's own price with two columns
    /// between them. One control covers both, or hovering leaves a gap that
    /// still answers the click.
    #[test]
    fn the_money_control_covers_both_figures() {
        with_ladder_ctx(|ctx| {
            let side = right_side(ctx, LADDER_CWD, WIDE_BUDGET);
            let glyphs = hit_glyphs(&side, StatusBarHitTarget::Usage);

            assert!(glyphs.starts_with(CHAT_COST_TEXT), "{glyphs}");
            assert!(glyphs.ends_with(SESSION_COST_TEXT), "{glyphs}");
        });
    }

    /// A figure the ladder dropped must not leave a control behind, or the bar
    /// hands clicks to columns another chip is using.
    #[test_case(WIDE_BUDGET, true, true              ; "both_figures_fit")]
    #[test_case(SHORT_THINKING_BUDGET, false, false  ; "neither_figure_fits")]
    fn a_dropped_figure_drops_its_control(budget: usize, context: bool, money: bool) {
        with_ladder_ctx(|ctx| {
            let side = right_side(ctx, LADDER_CWD, budget);

            assert_eq!(
                side_hit(&side, StatusBarHitTarget::Context).is_some(),
                context,
                "{STALE_HIT_MSG}"
            );
            assert_eq!(
                side_hit(&side, StatusBarHitTarget::Usage).is_some(),
                money,
                "{STALE_HIT_MSG}"
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

    #[test]
    fn provider_is_the_first_full_tier_to_go() {
        with_ladder_ctx(|ctx| {
            let spend = SpendText::new(&ctx.stats);
            let full_width = Fit::FULL.width(ctx, &spend, None);
            let (fit, reductions) = fit_right(ctx, &spend, None, full_width - 1);

            assert_eq!(reductions, 1);
            assert_eq!(fit.model, ModelTier::Leaf);
            assert_eq!(fit.global_spend, Fit::FULL.global_spend);
            assert_eq!(fit.context, Fit::FULL.context);
            assert_eq!(fit.workflows, Fit::FULL.workflows);
            assert_eq!(fit.yolo, Fit::FULL.yolo);
        });
    }

    #[test]
    fn thinking_never_spends_columns_naming_itself() {
        with_ladder_ctx(|ctx| {
            let side = right_side(ctx, LADDER_CWD, WIDE_BUDGET);
            assert_eq!(
                hit_glyphs(&side, StatusBarHitTarget::Thinking),
                FULL_THINKING_CHIP
            );
            assert!(!side_text(&side).contains("thinking:"));
        });
    }

    /// Two letters is the shortest form that still separates the catalog, and
    /// the names already that short keep their word rather than buying a single
    /// column with it.
    #[test_case("none",     "no"    ; "shortens_a_four_letter_level")]
    #[test_case("minimal",  "mi"    ; "shortens_the_longest_level")]
    #[test_case("medium",   "me"    ; "separates_medium_from_minimal")]
    #[test_case("xhigh",    "xh"    ; "keeps_the_x_that_ranks_it")]
    #[test_case("high",     "hi"    ; "shortens_high")]
    #[test_case("adaptive", "ad"    ; "shortens_the_model_decides_level")]
    #[test_case("off",      "off"   ; "keeps_off_whole")]
    #[test_case("low",      "low"   ; "keeps_low_whole")]
    #[test_case("max",      "max"   ; "keeps_max_whole")]
    fn a_squeezed_level_keeps_its_shortest_unambiguous_form(level: &str, expected: &str) {
        assert_eq!(ThinkingTier::Short.label(level), Some(expected));
    }

    /// A budget is a count, not a name: cutting `32768` to `32` would name a
    /// budget a thousandth the size, which is worse than spending the columns.
    #[test_case("32768" ; "five_digits")]
    #[test_case("8192"  ; "four_digits")]
    fn a_squeezed_budget_keeps_every_digit(budget: &str) {
        assert_eq!(ThinkingTier::Short.label(budget), Some(budget));
    }

    /// The property that justifies two letters instead of one. Fails if the
    /// catalog ever declares a level that collides with one already there.
    #[test]
    fn squeezed_catalog_levels_stay_distinct() {
        let mut seen: Vec<&str> = Vec::new();
        for level in EFFORT_LEVELS {
            let short = ThinkingTier::Short.label(level).expect(LEVEL_COLLISION_MSG);
            assert!(!seen.contains(&short), "{LEVEL_COLLISION_MSG}: {short}");
            seen.push(short);
        }
    }

    #[test_case(0,    "abc" ; "holds_at_the_start")]
    #[test_case(720,  "bcd" ; "moves_forward")]
    #[test_case(960,  "def" ; "holds_at_the_end")]
    #[test_case(1680, "cde" ; "moves_backward")]
    fn marquee_bounces_between_both_ends(elapsed_ms: u64, expected: &str) {
        assert_eq!(
            marquee_window("abcdef", 3, Duration::from_millis(elapsed_ms)),
            expected
        );
    }

    #[test]
    fn marquee_keeps_a_unicode_slots_width() {
        let text = marquee_window("你好世界", 4, Duration::from_millis(720));
        assert_eq!(text.width(), 4);
        assert_eq!(text, "好世");
    }

    #[test]
    fn marquee_pads_text_that_is_shorter_than_its_slot() {
        let text = marquee_window("ok", 4, Duration::ZERO);
        assert_eq!(text, "ok  ");
        assert_eq!(text.width(), 4);
    }

    #[test]
    fn marquee_resets_when_its_source_changes() {
        let mut marquee = Marquee::default();
        let _ = marquee.render(
            StatusBarHitTarget::ChatName,
            "abcdef",
            3,
            Cow::Borrowed("a.."),
            true,
        );
        marquee.active.as_mut().unwrap().started_at = Instant::now() - Duration::from_millis(720);
        assert_eq!(
            marquee.render(
                StatusBarHitTarget::ChatName,
                "uvwxyz",
                3,
                Cow::Borrowed("u.."),
                true,
            ),
            "uvw"
        );
    }

    #[test]
    fn an_active_marquee_claims_only_its_step_cadence() {
        let mut bar = StatusBar::new(FLASH_TTL, ".", false);
        bar.marquee.active = Some(MarqueeState {
            target: StatusBarHitTarget::ChatName,
            source: "a long chat name".into(),
            started_at: Instant::now(),
        });

        assert_eq!(
            bar.cadence(&Status::Idle, false, false, false),
            Cadence::due(MARQUEE_STEP)
        );
    }

    #[test]
    fn a_long_chat_name_is_bounded_and_hover_only() {
        const LONG_NAME: &str = "a-session-name-longer-than-the-footer-can-afford";
        let (_, hits, _) = render_at(Fixture {
            chat_name: Some(LONG_NAME),
            yolo: true,
            ..Fixture::default()
        });
        let chat = hits
            .iter()
            .find(|hit| hit.target == StatusBarHitTarget::ChatName)
            .expect("a clipped chat name needs a hover target");

        assert!(usize::from(chat.area.width) <= CHAT_NAME_MAX_WIDTH);
        assert!(!chat.target.accepts_click());
        assert!(
            hits.iter()
                .any(|hit| hit.target == StatusBarHitTarget::Model)
        );
    }

    #[test_case("anthropic/claude-opus-5", "claude-opus-5" ; "strips_provider_and_org")]
    #[test_case("claude-opus-5", "claude-opus-5"           ; "bare_id_is_its_own_leaf")]
    #[test_case("anthropic/", "anthropic/"                 ; "trailing_slash_keeps_the_id")]
    fn model_leaf_cases(id: &str, expected: &str) {
        assert_eq!(model_leaf(id), expected);
    }

    /// A switch names two models the user just chose between, so the provider
    /// both share is the first thing to go: those columns say nothing and the
    /// cwd has better uses for them.
    #[test]
    fn a_pending_model_switch_drops_the_provider_on_both_sides() {
        with_ladder_ctx_leaving(Some(LEAVING_MODEL_ID), |ctx| {
            let text = side_text(&right_side(ctx, LADDER_CWD, WIDE_BUDGET));

            assert!(text.contains(MODEL_PAIR), "{PAIR_MISSING}: {text}");
            assert!(!text.contains(LADDER_MODEL_ID), "{PROVIDER_KEPT}: {text}");
            assert!(!text.contains(LEAVING_MODEL_ID), "{PROVIDER_KEPT}: {text}");
        });
    }

    /// The same model served by two providers is a switch the leaves alone
    /// would draw as `claude-opus-5\u{2192}claude-opus-5`: a no-op, and the wrong
    /// answer about which endpoint the next turn reaches.
    #[test]
    fn a_switch_between_providers_keeps_them_both() {
        with_ladder_ctx_leaving(Some(REHOSTED_MODEL_ID), |ctx| {
            let text = side_text(&right_side(ctx, LADDER_CWD, WIDE_BUDGET));

            assert!(text.contains(REHOSTED_PAIR), "{PROVIDER_DROPPED}: {text}");
        });
    }

    /// A settled bar is the bar as it was: one model, provider and all.
    #[test]
    fn a_settled_model_draws_no_arrow() {
        with_ladder_ctx(|ctx| {
            let text = side_text(&right_side(ctx, LADDER_CWD, WIDE_BUDGET));

            assert!(text.contains(LADDER_MODEL_ID), "{text}");
            assert!(!text.contains(MODEL_TRANSITION_ARROW), "{text}");
        });
    }

    const BORDER_MISSING: &str =
        "the bar must say where auto-compaction fires, not only how full the window is";
    const BORDER_OUTLIVED_THE_COUNTS: &str =
        "a bar too narrow for the counts is too narrow for the border beside them";
    const BORDER_WITHOUT_COMPACTION: &str =
        "a session that never auto-compacts has no border to draw";

    /// The window is the denominator, so the share in use says nothing about
    /// when the transcript will be summarised. The border does.
    #[test]
    fn the_counter_says_where_auto_compaction_fires() {
        with_ladder_ctx(|ctx| {
            let text = side_text(&right_side(ctx, LADDER_CWD, WIDE_BUDGET));
            assert!(text.contains(COUNTS_GLYPHS), "{BORDER_MISSING}: {text}");
        });
    }

    /// First rung after the session total: the border is the cheapest thing on
    /// the bar to lose, and the counts it annotates must outlive it.
    #[test]
    fn a_narrowing_bar_drops_the_border_before_the_counts() {
        with_ladder_ctx(|ctx| {
            let texts: Vec<String> = (0..=WIDE_BUDGET)
                .rev()
                .map(|budget| side_text(&right_side(ctx, LADDER_CWD, budget)))
                .collect();

            let dropped = texts
                .iter()
                .position(|text| !text.contains(COUNTS_GLYPHS))
                .expect(BORDER_MISSING);
            assert!(
                texts[dropped].contains(BARE_COUNTS_GLYPHS),
                "{BORDER_OUTLIVED_THE_COUNTS}: {}",
                texts[dropped]
            );
        });
    }

    /// The plain fixture leaves auto-compaction off, so the counter has nothing
    /// to annotate and must say only what it knows.
    #[test]
    fn a_session_without_auto_compaction_draws_no_border() {
        let text = render(None, false, false);
        assert!(
            text.contains(BARE_COUNTS_GLYPHS),
            "{BORDER_WITHOUT_COMPACTION}: {text}"
        );
    }

    /// The provider is already gone at the first pressure rung. The pair still
    /// goes before the bar starts character-clipping the arriving model.
    #[test]
    fn a_narrowing_bar_drops_the_transition_before_it_shortens_the_model() {
        with_ladder_ctx_leaving(Some(LEAVING_MODEL_ID), |ctx| {
            let widths: Vec<usize> = (0..=WIDE_BUDGET).rev().collect();
            let texts: Vec<String> = widths
                .iter()
                .map(|budget| side_text(&right_side(ctx, LADDER_CWD, *budget)))
                .collect();

            let dropped = texts
                .iter()
                .position(|text| !text.contains(MODEL_TRANSITION_ARROW))
                .expect(PAIR_KEPT);
            assert!(
                texts[dropped].contains(LADDER_MODEL_LEAF),
                "{PAIR_KEPT}: {}",
                texts[dropped]
            );
            assert!(
                texts[dropped..]
                    .iter()
                    .all(|text| !text.contains(MODEL_TRANSITION_ARROW)),
                "{PAIR_RETURNED}"
            );
        });
    }

    /// Whatever the ladder leaves, the model still answers the pointer on
    /// exactly the glyphs it drew.
    #[test_case(WIDE_BUDGET, MODEL_PAIR              ; "the_pair_while_it_fits")]
    #[test_case(SHORT_THINKING_BUDGET, LADDER_MODEL_LEAF ; "the_leaf_once_it_does_not")]
    fn the_model_control_covers_what_was_drawn(budget: usize, expected: &str) {
        with_ladder_ctx_leaving(Some(LEAVING_MODEL_ID), |ctx| {
            let side = right_side(ctx, LADDER_CWD, budget);

            assert_eq!(
                hit_glyphs(&side, StatusBarHitTarget::Model),
                format!("[{expected}]"),
                "{FIGURE_HIT_MSG}"
            );
        });
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
        let (_, hits, _) = render_at(Fixture {
            width,
            global_cost: Some(SESSION_COST),
            show_global: true,
            yolo: true,
            goal: Some(&goal),
            ..Default::default()
        });
        let area = Rect::new(0, 0, width, 1);
        assert!(hits.iter().all(|hit| {
            hit.area.width > 0
                && area.contains(ratatui::layout::Position::new(hit.area.x, hit.area.y))
                && hit.area.right() <= area.right()
        }));
    }

    #[test]
    fn compact_status_preserves_mode_control() {
        let (_, hits, _) = render_at(Fixture {
            width: 20,
            ..Default::default()
        });
        assert!(
            hits.iter()
                .any(|hit| hit.target == StatusBarHitTarget::Mode)
        );
    }

    #[test]
    fn wide_status_exposes_all_main_controls() {
        let goal = active_goal();
        let (_, hits, _) = render_at(Fixture {
            goal: Some(&goal),
            ..Default::default()
        });
        for target in [
            StatusBarHitTarget::Mode,
            StatusBarHitTarget::Model,
            StatusBarHitTarget::Thinking,
            StatusBarHitTarget::Goal,
            StatusBarHitTarget::Context,
            StatusBarHitTarget::Usage,
        ] {
            assert!(hits.iter().any(|hit| hit.target == target), "{target:?}");
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
            StatusBarHitTarget::Context,
            StatusBarHitTarget::Usage,
        ] {
            let (_, hits, styles) = render_at(Fixture {
                hovered: Some(target),
                goal: Some(&goal),
                ..Default::default()
            });
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
        let (text, hits, _) = render_at(Fixture {
            hover_hint: Some(URL),
            ..Default::default()
        });

        assert!(text.trim_start().starts_with(URL));
        assert!(hits.is_empty());
    }

    /// The chip is the only left-side control whose width comes from live
    /// numbers, so the hit is measured against what was drawn rather than
    /// against a re-formatted label whose elapsed time may already have moved.
    #[test]
    fn goal_chip_hit_covers_the_chip_alone() {
        let goal = active_goal();
        let (text, hits, _) = render_at(Fixture {
            goal: Some(&goal),
            ..Default::default()
        });
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

    /// The label is the whole control: the space ahead of it separates it from
    /// whatever the bar drew last and must not answer the pointer.
    #[test]
    fn a_paused_transcript_offers_a_resume_control() {
        let (text, hits, _) = render_at(Fixture {
            auto_scroll: false,
            ..Default::default()
        });
        let hit = hits
            .iter()
            .find(|hit| hit.target == StatusBarHitTarget::ResumeAutoScroll)
            .expect(MISSING_RESUME_HIT_MSG);

        assert_eq!(resume_glyphs(&text, hit), AUTO_SCROLL_PAUSED_LABEL);
        assert_eq!(text.chars().nth(usize::from(hit.area.x) - 1), Some(' '));
        assert!(hit.target.accepts_click());
    }

    #[test]
    fn a_following_transcript_has_no_resume_control() {
        let (text, hits, _) = render_at(Fixture::default());

        assert!(!text.contains(AUTO_SCROLL_PAUSED_LABEL));
        assert!(
            hits.iter()
                .all(|hit| hit.target != StatusBarHitTarget::ResumeAutoScroll)
        );
    }

    /// A squeezed bar shortens the mode label the resume label is measured
    /// from, so the hit has to travel with the glyphs. A bar too narrow to
    /// draw the label whole offers nothing to click instead of a clipped
    /// target that lies about what it covers.
    #[test_case(20        ; "too_narrow_to_draw_the_label")]
    #[test_case(24        ; "shortened_mode")]
    #[test_case(30        ; "full_mode")]
    #[test_case(36        ; "shortened_mode_beside_the_model")]
    #[test_case(60        ; "roomy")]
    #[test_case(BAR_WIDTH ; "wide")]
    fn a_resume_hit_tracks_the_label_it_was_drawn_on(width: u16) {
        let (text, hits, _) = render_at(Fixture {
            width,
            auto_scroll: false,
            ..Default::default()
        });

        match hits
            .iter()
            .find(|hit| hit.target == StatusBarHitTarget::ResumeAutoScroll)
        {
            Some(hit) => assert_eq!(resume_glyphs(&text, hit), AUTO_SCROLL_PAUSED_LABEL),
            None => assert!(
                !text.contains(AUTO_SCROLL_PAUSED_LABEL),
                "{UNCLICKABLE_LABEL_MSG}"
            ),
        }
    }

    #[test]
    fn hovering_the_resume_control_highlights_its_label_alone() {
        let (_, hits, styles) = render_at(Fixture {
            auto_scroll: false,
            hovered: Some(StatusBarHitTarget::ResumeAutoScroll),
            ..Default::default()
        });
        let hit = hits
            .iter()
            .find(|hit| hit.target == StatusBarHitTarget::ResumeAutoScroll)
            .expect(MISSING_RESUME_HIT_MSG);
        let start = usize::from(hit.area.x);
        let end = usize::from(hit.area.right());

        assert!(!styles[start - 1].add_modifier.contains(Modifier::REVERSED));
        assert!(
            styles[start..end]
                .iter()
                .all(|style| style.add_modifier.contains(Modifier::REVERSED))
        );
    }

    /// A task transcript pauses and resumes on its own, so the control is not
    /// one of the session settings a subagent chat draws inert.
    #[test]
    fn a_subagent_transcript_resumes_from_its_own_footer() {
        let (_, hits, _) = render_at(Fixture {
            auto_scroll: false,
            main_chat: false,
            ..Default::default()
        });

        assert_eq!(StatusBarHitTarget::ResumeAutoScroll.scope(), ChatScope::Any);
        assert!(
            hits.iter()
                .any(|hit| hit.target == StatusBarHitTarget::ResumeAutoScroll)
        );
    }

    #[test]
    fn a_session_without_a_goal_has_no_goal_control() {
        let (text, hits, _) = render_at(Fixture::default());

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
    #[test_case(Some(0.250), None,        Some("$0.25")   ; "empty_hundredth_is_trimmed")]
    #[test_case(Some(1.500), None,        Some("$1.5")    ; "empty_hundredths_are_trimmed")]
    #[test_case(Some(0.0004), None,       Some("$0.000")  ; "tiny_real_cost_does_not_claim_zero")]
    #[test_case(Some(0.0), None,          Some("$0")      ; "known_zero_is_zero")]
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
        let mut bar = StatusBar::new(ttl, ".", false);
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
        let cwd = std::env::current_dir().unwrap();
        let label = cwd_branch_label(&cwd.to_string_lossy());
        let (tx, rx) = flume::bounded(1);
        let mut bar = StatusBar::new(FLASH_TTL, ".", false);
        bar.cwd_branch = if stale {
            STALE_BRANCH.into()
        } else {
            label.clone()
        };
        bar.cwd = Some(cwd.to_string_lossy().into_owned());
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
        let mut bar = StatusBar::new(Duration::from_secs(999), ".", false);
        bar.flash("Copied".into());
        bar.clear_flash();
        assert!(bar.flash.is_none());
    }

    fn render_retry(hovered: bool) -> (String, Vec<StatusBarHit>, Vec<Style>) {
        let retry = RetryInfo {
            attempt: RETRY_ATTEMPT,
            message: RETRY_MESSAGE.into(),
            deadline: Instant::now() + RETRY_REMAINING,
        };
        render_at(Fixture {
            hovered: hovered.then_some(StatusBarHitTarget::Retry),
            retry_info: Some(&retry),
            ..Default::default()
        })
    }

    #[test]
    fn a_retry_countdown_is_clickable() {
        let (text, hits, _) = render_retry(false);
        assert!(text.contains(RETRY_MESSAGE));
        assert!(text.contains(RETRY_COUNTDOWN_PREFIX));
        assert!(text.contains(RETRY_ATTEMPT_MARK));
        assert!(!text.contains(RETRY_NOW_LABEL.trim()));
        hits.iter()
            .find(|hit| hit.target == StatusBarHitTarget::Retry)
            .expect(MISSING_RETRY_HIT_MSG);
    }

    /// The countdown is the only thing that changes: the error keeps saying
    /// what went wrong while the chip says what the click will do.
    #[test]
    fn hovering_a_retry_offers_to_retry_now() {
        let (text, _, _) = render_retry(true);
        assert!(text.contains(RETRY_MESSAGE));
        assert!(text.contains(RETRY_NOW_LABEL.trim()));
        assert!(!text.contains(RETRY_COUNTDOWN_PREFIX));
    }

    #[test]
    fn hovering_a_retry_highlights_the_whole_control() {
        let (_, hits, styles) = render_retry(true);
        let hit = hits
            .iter()
            .find(|hit| hit.target == StatusBarHitTarget::Retry)
            .expect(MISSING_RETRY_HIT_MSG);
        let start = usize::from(hit.area.x);
        let end = usize::from(hit.area.right());
        assert!(!styles[start - 1].add_modifier.contains(Modifier::REVERSED));
        assert!(
            styles[start..end]
                .iter()
                .all(|style| style.add_modifier.contains(Modifier::REVERSED))
        );
    }

    /// Width alone does not choose the abbreviation: the same terminal keeps
    /// the full mode on a quiet bar and spends those columns when they preserve
    /// a richer right-side tier.
    #[test]
    fn footer_pressure_abbreviates_the_mode() {
        let found = (20..=200).find_map(|width| {
            let quiet = render_at(Fixture {
                width,
                ..Fixture::default()
            });
            let busy = render_at(Fixture {
                width,
                workflows: ladder_workflows(),
                yolo: true,
                ..Fixture::default()
            });
            (quiet.0.contains(MODE_LABEL) && busy.0.contains(MODE_SHORT_LABEL))
                .then_some((width, quiet, busy))
        });
        let (width, _, (_, hits, _)) = found.expect("pressure never abbreviated the mode");
        let hit = hits
            .iter()
            .find(|hit| hit.target == StatusBarHitTarget::Mode)
            .expect(EXPECTED_MODE_HIT);
        assert_eq!(
            usize::from(hit.area.width),
            MODE_SHORT_LABEL.width(),
            "{width}"
        );
    }

    #[test]
    fn no_retry_means_no_retry_hit() {
        let (_, hits, _) = render_at(Fixture::default());
        assert!(
            hits.iter()
                .all(|hit| hit.target != StatusBarHitTarget::Retry)
        );
    }

    const ALL_CONTROLS: [StatusBarHitTarget; 9] = [
        StatusBarHitTarget::BackToMain,
        StatusBarHitTarget::Mode,
        StatusBarHitTarget::Model,
        StatusBarHitTarget::Thinking,
        StatusBarHitTarget::Goal,
        StatusBarHitTarget::Context,
        StatusBarHitTarget::Usage,
        StatusBarHitTarget::Workflows,
        StatusBarHitTarget::Retry,
    ];
    const TASK_CONTROLS: [StatusBarHitTarget; 4] = [
        StatusBarHitTarget::BackToMain,
        StatusBarHitTarget::Context,
        StatusBarHitTarget::Usage,
        StatusBarHitTarget::Retry,
    ];
    const TASK_HIT_MSG: &str = "a task's bar offers exactly the controls a task owns";
    /// Wide enough that every chip survives the ladder, so a control missing
    /// from the hits is one the scope refused rather than one the width dropped.
    const TASK_BAR_WIDTH: u16 = 200;

    /// A task's bar still draws the session's model, reasoning level, workflow
    /// count and goal, because they describe the run the task belongs to. Only
    /// the controls that read the transcript in front of you, or leave it,
    /// answer the pointer.
    #[test]
    fn a_task_bar_offers_only_the_controls_a_task_owns() {
        let goal = active_goal();
        let retry = RetryInfo {
            attempt: RETRY_ATTEMPT,
            message: RETRY_MESSAGE.into(),
            deadline: Instant::now() + RETRY_REMAINING,
        };
        let (_, hits, _) = render_at(Fixture {
            width: TASK_BAR_WIDTH,
            main_chat: false,
            goal: Some(&goal),
            retry_info: Some(&retry),
            workflows: ladder_workflows(),
            ..Default::default()
        });

        for target in ALL_CONTROLS {
            assert_eq!(
                hits.iter().any(|hit| hit.target == target),
                TASK_CONTROLS.contains(&target),
                "{TASK_HIT_MSG}: {target:?}"
            );
        }
    }

    /// Every control that opens a command carries that command's scope, so the
    /// bar cannot refuse a click the palette accepts. `Mode`, `BackToMain`,
    /// `Retry` and `Thinking` are absent because their click runs no command.
    const SCOPED_COMMANDS: [(StatusBarHitTarget, &str); 5] = [
        (StatusBarHitTarget::Model, "/model"),
        (StatusBarHitTarget::Goal, "/goal"),
        (StatusBarHitTarget::Context, "/context"),
        (StatusBarHitTarget::Usage, "/usage"),
        (StatusBarHitTarget::Workflows, "/workflows"),
    ];
    const SCOPE_DRIFT_MSG: &str = "a control and its command must agree on which chat they act on";
    const UNKNOWN_COMMAND_MSG: &str = "a control names a command the builtin table does not have";

    #[test]
    fn every_control_shares_the_scope_of_the_command_it_opens() {
        for (target, name) in SCOPED_COMMANDS {
            let command = crate::components::command::BUILTIN_COMMANDS
                .iter()
                .find(|builtin| builtin.name == name)
                .expect(UNKNOWN_COMMAND_MSG);
            assert_eq!(target.scope(), command.scope, "{SCOPE_DRIFT_MSG}: {name}");
        }
    }
}
