use std::borrow::Cow;
use std::ops::Range;
use std::path::{MAIN_SEPARATOR, Path};
use std::time::{Duration, Instant};

use super::command::ChatScope;
use super::{RetryInfo, Status, escape_terminal_controls, hover_style};

use crate::animation::spinner_frame;
use crate::theme;

use caudra_providers::format_tokens;
use caudra_storage::sessions::PermissionMode;
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
const HOME_ABBREVIATION: &str = "~";
const CWD_MODEL_SEPARATOR: &str = "  ";
/// What separates one thing the bar draws from the next.
const GAP: &str = " ";
/// Below this many terminal rows a line of transcript is worth more than the
/// abbreviations a second footer row would spare.
const SPLIT_MIN_TERMINAL_ROWS: u16 = 20;
const SPLIT_ROWS: u16 = 2;
const SINGLE_ROW: u16 = 1;
/// The spinner's two columns while nothing spins, so the chips after it hold
/// still when a turn starts or ends.
const SPINNER_SLOT_BLANK: &str = "  ";
const BACK_TO_MAIN_LABEL: &str = "[< Main]";
const TASKS_LABEL: &str = "tasks";
const SHELLS_LABEL: &str = "shell";
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
const AUTO_LABEL: &str = " [auto]";
const AUTO_SHORT_LABEL: &str = " [a]";
const DECISIONS_OFFLINE_LABEL: &str = "[decisions offline]";
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
const SANDBOX_PREFIX: &str = "[sandbox: ";
const SANDBOX_BARE_PREFIX: &str = "[";
const SANDBOX_SUFFIX: &str = "]";
/// The chip's rungs, widest first: `[sandbox: name]`, then `[name]`, then the
/// nothing a bar too narrow for either gets. The word goes before the name
/// because it reads the same in every session, while the name is the half that
/// says which sandbox this one is on; below the bare name all that is left to
/// draw is a stub naming no instance at all.
const SANDBOX_TIERS: [&str; 2] = [SANDBOX_PREFIX, SANDBOX_BARE_PREFIX];
/// Columns the instance name keeps at either rung, the slot the chat name gets:
/// a name is bounded at 64 bytes, and one left-side name taking the bar would
/// leave the other with nothing.
const SANDBOX_NAME_MAX_WIDTH: usize = CHAT_NAME_MAX_WIDTH;
const MARQUEE_STEP: Duration = Duration::from_millis(120);
const MARQUEE_PAUSE: Duration = Duration::from_millis(600);
/// Marks a figure a subscription already covers. One column is all the bar can
/// spare to say the number is a price rather than a bill.
const NOT_BILLED_MARK: &str = "~";
/// Joins the model a pending mode switch leaves to the one it arrives on,
/// matching the glyph the mode label uses for the same switch.
const MODEL_TRANSITION_ARROW: &str = "\u{2192}";
/// Cells in the context gauge. Each fills in eighths, so the gauge resolves
/// the window to one part in eighty.
const GAUGE_CELLS: u8 = 10;
const CELL_EIGHTHS: u8 = 8;
const GAUGE_EIGHTHS: u8 = GAUGE_CELLS * CELL_EIGHTHS;
/// A cell filled one eighth to seven eighths.
const PARTIAL_CELLS: [char; 7] = [
    '\u{258f}', '\u{258e}', '\u{258d}', '\u{258c}', '\u{258b}', '\u{258a}', '\u{2589}',
];
const FULL_CELL: char = '\u{2588}';
const EMPTY_CELL: char = '\u{2591}';
const GAUGE_TICK: &str = "\u{2502}";
const GAUGE_OPEN: &str = "\u{2595}";
const GAUGE_CLOSE: &str = "\u{258f}";

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

/// Rows the footer takes on a terminal `terminal_rows` tall. Two once there is
/// room, so what the next message runs with never gives up columns to what the
/// agent is doing. It depends on nothing else, so a turn starting or a task
/// spawning never moves the composer.
pub fn height(terminal_rows: u16) -> u16 {
    if terminal_rows >= SPLIT_MIN_TERMINAL_ROWS {
        SPLIT_ROWS
    } else {
        SINGLE_ROW
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusBarHitTarget {
    BackToMain,
    Mode,
    Model,
    Thinking,
    Goal,
    Tasks,
    Shells,
    Context,
    Usage,
    Workflows,
    Retry,
    ChatName,
    Cwd,
    ResumeAutoScroll,
    Yolo,
    Auto,
    Fast,
    Sandbox,
}

impl StatusBarHitTarget {
    /// Which chat a control acts on, mirroring the [`ChatScope`] of the command
    /// it opens so the bar cannot refuse a click the palette would accept.
    ///
    /// [`Self::Thinking`] is main-only although `/thinking` is [`ChatScope::Any`]:
    /// the chip names the level whichever chat is on screen runs at, and a task
    /// runs at the one its own model resolved. Clicking would edit the session
    /// setting instead, so on a task the chip is a label.
    ///
    /// [`Self::Retry`] is main-only although every chat draws its own countdown:
    /// asking for an immediate retry reaches the top-level agent alone, so on a
    /// task the chip is a label and a click there would shorten the main
    /// conversation's backoff instead of the one on screen.
    ///
    /// [`Self::Fast`] is main-only for the same reason as [`Self::Thinking`]:
    /// a task draws the fast flag it runs under, which its spawner chose, so a
    /// click there would turn the session's fast mode off instead.
    pub fn scope(self) -> ChatScope {
        match self {
            Self::BackToMain
            | Self::Context
            | Self::Usage
            | Self::ChatName
            | Self::Cwd
            | Self::ResumeAutoScroll
            | Self::Yolo
            | Self::Auto
            | Self::Tasks
            | Self::Shells
            | Self::Sandbox => ChatScope::Any,
            Self::Mode
            | Self::Model
            | Self::Thinking
            | Self::Goal
            | Self::Workflows
            | Self::Fast
            | Self::Retry => ChatScope::MainOnly,
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
    /// The sandbox instance the runtime is attached to, named as the manager
    /// names it, or identified by id when nothing on this side has read the
    /// name yet. `None` unless this runtime's own authenticated connection is
    /// up, so the chip cannot claim a sandbox that is merely running.
    pub sandbox: Option<&'a str>,
    pub main_chat: bool,
    pub retry_info: Option<&'a RetryInfo>,
    /// The effective level alone (`off`, `xhigh`, `8192`), drawn directly as a
    /// compact chip such as `[xhigh]`.
    pub thinking: Option<Cow<'static, str>>,
    pub fast: bool,
    /// Already rendered by [`workflow_chip`], so fitting the bar measures
    /// strings rather than formatting one per rung.
    pub workflows: Option<WorkflowChip>,
    pub permission_mode: PermissionMode,
    pub decisions_offline: bool,
    pub restoring: bool,
    pub goal: Option<&'a GoalSnapshot>,
    pub active_tasks: usize,
    pub active_shells: usize,
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
enum PermissionTier {
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
    /// `▕▌░░░░░░░░│▏ 12k/200k (6%/90%)`.
    Gauge,
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
    DropGauge,
    LeafModel,
    DropGlobalSpend,
    DropCompactionBorder,
    CompactContext,
    ShortThinking,
    ShortWorkflows,
    ShortPermission,
    DropTransition,
    DropSpend,
    DropContext,
    DropFast,
    DropWorkflows,
    DropThinking,
    ChopModel,
    DropPermission,
}

/// The two rows of a split footer. Settings are what the user chose, which is
/// what answers the next message. The live row is what the agent is doing and
/// what it has cost so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Band {
    Settings,
    Live,
}

impl Reduction {
    /// The row whose chip the step shortens. Each row walks only its own
    /// rungs, so pressure on one never costs the other a column.
    fn band(self) -> Band {
        match self {
            Self::LeafModel
            | Self::ShortThinking
            | Self::ShortPermission
            | Self::DropTransition
            | Self::DropFast
            | Self::DropThinking
            | Self::ChopModel
            | Self::DropPermission => Band::Settings,
            Self::DropGauge
            | Self::DropGlobalSpend
            | Self::DropCompactionBorder
            | Self::CompactContext
            | Self::ShortWorkflows
            | Self::DropSpend
            | Self::DropContext
            | Self::DropWorkflows => Band::Live,
        }
    }
}

/// The gauge is the first thing pressure takes, because it only draws the
/// counter beside it a second time. A provider prefix is next: the model leaf
/// carries the useful identity, and the recovered columns keep every other full
/// tier. Yolo goes last because a session that skips permission prompts has to say so
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
///
/// A split footer walks this same ladder once per row, skipping the rungs of
/// the other [`Band`], so both layouts give up their chips in one order.
const LADDER: [Reduction; 16] = [
    Reduction::DropGauge,
    Reduction::LeafModel,
    Reduction::DropGlobalSpend,
    Reduction::DropCompactionBorder,
    Reduction::CompactContext,
    Reduction::ShortThinking,
    Reduction::ShortWorkflows,
    Reduction::ShortPermission,
    Reduction::DropTransition,
    Reduction::DropSpend,
    Reduction::DropContext,
    Reduction::DropFast,
    Reduction::DropWorkflows,
    Reduction::DropThinking,
    Reduction::ChopModel,
    Reduction::DropPermission,
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
    /// `None` without a window to measure against.
    gauge: Option<Gauge>,
}

/// The share of the window `tokens` fill, in whole percent.
pub(crate) fn context_share(tokens: u32, window: u32) -> u32 {
    if window == 0 {
        return 0;
    }
    (f64::from(tokens) / f64::from(window) * 100.0) as u32
}

impl SpendText {
    fn new(stats: &UsageStats) -> Self {
        let share = |tokens: u32| context_share(tokens, stats.context_window);
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
            gauge: Gauge::new(stats),
        }
    }
}

/// The window as [`GAUGE_CELLS`] cells filled in eighths, with a tick in the
/// cell auto-compaction fires at for as long as the fill has not reached it.
struct Gauge {
    fill: String,
    /// The empty cells ahead of the tick, or all of them without one.
    track: String,
    tick: bool,
    rest: String,
}

impl Gauge {
    fn new(stats: &UsageStats) -> Option<Self> {
        let window = stats.context_window;
        if window == 0 {
            return None;
        }
        let eighths = gauge_eighths(stats.context_size, window);
        let (whole, partial) = (eighths / CELL_EIGHTHS, eighths % CELL_EIGHTHS);
        let mut fill = cells(FULL_CELL, whole);
        if let Some(index) = partial.checked_sub(1) {
            fill.push(PARTIAL_CELLS[usize::from(index)]);
        }
        let used = whole + u8::from(partial > 0);
        let tick = stats
            .compaction_border
            .map(|border| gauge_eighths(border, window) / CELL_EIGHTHS)
            .filter(|cell| (used..GAUGE_CELLS).contains(cell));
        let (track, rest) = match tick {
            Some(cell) => (cell - used, GAUGE_CELLS - cell - 1),
            None => (GAUGE_CELLS - used, 0),
        };
        Some(Self {
            fill,
            track: cells(EMPTY_CELL, track),
            tick: tick.is_some(),
            rest: cells(EMPTY_CELL, rest),
        })
    }

    fn width(&self) -> usize {
        GAUGE_OPEN.width()
            + self.fill.width()
            + self.track.width()
            + usize::from(self.tick) * GAUGE_TICK.width()
            + self.rest.width()
            + GAUGE_CLOSE.width()
    }

    /// The fill and the tick take the counter's own style, so both turn amber
    /// with it once the border is behind the session. The frame and the empty
    /// cells stay dim.
    fn spans<'a>(&self, style: Style) -> [Span<'a>; 6] {
        let dim = theme::current().status_dim;
        [
            Span::styled(GAUGE_OPEN, dim),
            Span::styled(self.fill.clone(), style),
            Span::styled(self.track.clone(), dim),
            Span::styled(if self.tick { GAUGE_TICK } else { "" }, style),
            Span::styled(self.rest.clone(), dim),
            Span::styled(GAUGE_CLOSE, dim),
        ]
    }
}

/// The eighths of the gauge `tokens` fill, rounded down so a sliver of a cell
/// never reads as the cell.
fn gauge_eighths(tokens: u32, window: u32) -> u8 {
    let eighths = u64::from(tokens) * u64::from(GAUGE_EIGHTHS) / u64::from(window);
    u8::try_from(eighths).map_or(GAUGE_EIGHTHS, |eighths| eighths.min(GAUGE_EIGHTHS))
}

fn cells(glyph: char, count: u8) -> String {
    std::iter::repeat_n(glyph, usize::from(count)).collect()
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
    permission: PermissionTier,
}

impl Fit {
    const FULL: Self = Self {
        spend: true,
        global_spend: true,
        context: ContextTier::Gauge,
        compaction_border: true,
        thinking: ThinkingTier::Full,
        model: ModelTier::Full,
        transition: true,
        fast: true,
        workflows: WorkflowTier::Named,
        permission: PermissionTier::Named,
    };

    fn apply(&mut self, step: Reduction) {
        match step {
            Reduction::DropGauge => self.context = ContextTier::Counts,
            Reduction::LeafModel => self.model = ModelTier::Leaf,
            Reduction::DropGlobalSpend => self.global_spend = false,
            Reduction::DropCompactionBorder => self.compaction_border = false,
            Reduction::CompactContext => self.context = ContextTier::Percent,
            Reduction::ShortThinking => self.thinking = ThinkingTier::Short,
            Reduction::ShortWorkflows => self.workflows = WorkflowTier::Counts,
            Reduction::ShortPermission => self.permission = PermissionTier::Sigil,
            Reduction::DropTransition => self.transition = false,
            Reduction::DropSpend => self.spend = false,
            Reduction::DropContext => self.context = ContextTier::Hidden,
            Reduction::DropFast => self.fast = false,
            Reduction::DropWorkflows => self.workflows = WorkflowTier::Hidden,
            Reduction::DropThinking => self.thinking = ThinkingTier::Hidden,
            Reduction::ChopModel => self.model = ModelTier::Chopped,
            Reduction::DropPermission => self.permission = PermissionTier::Hidden,
        }
    }

    /// The columns the chips of `scope` take: every chip on a one-row bar,
    /// else the chips of the one row of a split footer the band names.
    fn width(
        self,
        ctx: &StatusBarContext<'_>,
        spend: &SpendText,
        pair: Option<&str>,
        scope: Option<Band>,
    ) -> usize {
        match scope {
            None => self.settings_width(ctx, pair) + self.live_width(ctx, spend),
            Some(Band::Settings) => self.settings_width(ctx, pair),
            Some(Band::Live) => self.live_width(ctx, spend),
        }
    }

    /// The model and the settings beside it. The cwd takes whatever columns
    /// this leaves.
    fn settings_width(self, ctx: &StatusBarContext<'_>, pair: Option<&str>) -> usize {
        self.model_width(ctx, pair)
            + self.thinking_width(ctx)
            + usize::from(ctx.fast && self.fast) * FAST_LABEL.width()
            + self.permission_label(ctx).map_or(0, UnicodeWidthStr::width)
    }

    fn live_width(self, ctx: &StatusBarContext<'_>, spend: &SpendText) -> usize {
        self.workflow_label(ctx).map_or(0, UnicodeWidthStr::width) + self.spend_width(spend)
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

    fn workflow_label<'a>(self, ctx: &'a StatusBarContext<'_>) -> Option<&'a str> {
        let chip = ctx.workflows.as_ref()?;
        match self.workflows {
            WorkflowTier::Named => Some(&chip.named),
            WorkflowTier::Counts => Some(&chip.counts),
            WorkflowTier::Hidden => None,
        }
    }

    fn permission_label(self, ctx: &StatusBarContext<'_>) -> Option<&'static str> {
        self.permission.label(&ctx.permission_mode)
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
            + self
                .gauge(spend)
                .map_or(0, |gauge| gauge.width() + GAP.width())
            + self.money_text(spend).map_or(0, |money| money.width())
    }

    /// Drawn ahead of the counts, which stay as they are with or without it.
    fn gauge(self, spend: &SpendText) -> Option<&Gauge> {
        spend
            .gauge
            .as_ref()
            .filter(|_| self.context == ContextTier::Gauge)
    }

    fn context_text(self, spend: &SpendText) -> Option<Cow<'_, str>> {
        let share = match spend.border.as_deref().filter(|_| self.compaction_border) {
            Some(border) => Cow::Owned(format!("{}/{border}", spend.percent)),
            None => Cow::Borrowed(spend.percent.as_str()),
        };
        match self.context {
            ContextTier::Gauge | ContextTier::Counts => {
                Some(Cow::Owned(format!(" {} ({share})", spend.counts)))
            }
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

impl PermissionTier {
    fn label(self, mode: &PermissionMode) -> Option<&'static str> {
        match (self, mode) {
            (Self::Named, PermissionMode::Yolo) => Some(YOLO_LABEL),
            (Self::Sigil, PermissionMode::Yolo) => Some(YOLO_SHORT_LABEL),
            (Self::Named, PermissionMode::Auto) => Some(AUTO_LABEL),
            (Self::Sigil, PermissionMode::Auto) => Some(AUTO_SHORT_LABEL),
            (Self::Hidden, _) | (_, PermissionMode::Ask) => None,
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

    /// The bar spins for a whole turn and again while a restore is in flight.
    /// It sits next to [`Self::view`] so a new moving span cannot forget to
    /// claim its frames; the retry countdown is the exception, claimed by the
    /// chat that owns it because the bar only borrows it to draw.
    pub fn cadence(&self, status: &Status, restoring: bool, goal_active: bool) -> Cadence {
        Cadence::any([
            Cadence::when(*status == Status::Streaming || restoring, Cadence::SPINNER),
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
        let mut hits = Vec::new();
        if area.height >= SPLIT_ROWS {
            let [settings, live] = Layout::vertical([
                Constraint::Length(SINGLE_ROW),
                Constraint::Length(SINGLE_ROW),
            ])
            .areas(area);
            self.settings_row(frame, settings, ctx, &mut hits);
            self.live_row(frame, live, ctx, &mut hits);
        } else {
            self.single_row(frame, area, ctx, &mut hits);
        }
        self.marquee.finish_frame();
        hits
    }

    /// Every chip on one line, for a terminal too short to spare a second row.
    /// The left side is laid out first and the right side fits whatever columns
    /// it leaves, walking the whole ladder.
    fn single_row(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        ctx: &StatusBarContext<'_>,
        hits: &mut Vec<StatusBarHit>,
    ) {
        if self.draw_hint(frame, area, ctx) {
            return;
        }
        let mut left = Strip::default();
        if *ctx.status == Status::Streaming {
            left.push(Span::styled(
                format!("{GAP}{}", self.spinner()),
                theme::current().spinner,
            ));
        }
        if ctx.restoring {
            left.push(Span::styled(
                format!("{GAP}{}", self.spinner()),
                theme::current().status_notice,
            ));
        }
        self.push_settings(&mut left, ctx, area.width);
        push_resume(&mut left, ctx);
        push_goal(&mut left, ctx);
        push_activity(&mut left, ctx);
        push_retry(&mut left, ctx);
        push_decisions_status(&mut left, ctx);
        shorten_mode(&mut left, ctx, area.width, false);
        let right = match ctx.status {
            Status::Error { message, .. } => {
                left.push(error_span(message));
                Strip::default()
            }
            _ => right_side_animated(
                ctx,
                &self.cwd_branch,
                right_budget(area.width, &left),
                Some(&mut self.marquee),
                false,
            ),
        };
        if let Some(flash) = self.flash_span() {
            left.push(flash);
        }
        draw_row(frame, area, ctx, left, right, hits);
    }

    /// The mode, the model and the settings the next message runs with. The
    /// live row owns the meters and the activity, so nothing here gives up a
    /// column when a turn starts, a retry backs off, or a task spawns.
    fn settings_row(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        ctx: &StatusBarContext<'_>,
        hits: &mut Vec<StatusBarHit>,
    ) {
        let mut left = Strip::default();
        self.push_settings(&mut left, ctx, area.width);
        shorten_mode(&mut left, ctx, area.width, true);
        let right = right_side_animated(
            ctx,
            &self.cwd_branch,
            right_budget(area.width, &left),
            Some(&mut self.marquee),
            true,
        );
        draw_row(frame, area, ctx, left, right, hits);
    }

    /// What the agent is doing and what it has cost. Messages land here too,
    /// so an error, a backoff or a hovered link never takes a setting's
    /// columns, and the meters fit whatever the activity and messages leave.
    fn live_row(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        ctx: &StatusBarContext<'_>,
        hits: &mut Vec<StatusBarHit>,
    ) {
        if self.draw_hint(frame, area, ctx) {
            return;
        }
        let mut activity = Strip::default();
        activity.push(self.spinner_slot(ctx));
        push_goal(&mut activity, ctx);
        push_activity(&mut activity, ctx);
        let mut messages = Strip::default();
        push_resume(&mut messages, ctx);
        push_retry(&mut messages, ctx);
        if let Status::Error { message, .. } = ctx.status {
            messages.push(error_span(message));
        }
        if let Some(flash) = self.flash_span() {
            messages.push(flash);
        }
        push_decisions_status(&mut messages, ctx);
        let spend = SpendText::new(&ctx.stats);
        let budget = usize::from(area.width).saturating_sub(activity.width() + messages.width());
        let (fit, _) = fit_right(ctx, &spend, None, budget, Some(Band::Live));
        push_workflows(&mut activity, ctx, fit);
        activity.append(messages);
        let mut meters = Strip::default();
        push_meters(&mut meters, ctx, fit, &spend);
        draw_row(frame, area, ctx, activity, meters, hits);
    }

    /// A hovered link takes its row whole, unless a flash is already speaking.
    fn draw_hint(&self, frame: &mut Frame, area: Rect, ctx: &StatusBarContext<'_>) -> bool {
        let Some(url) = ctx.hover_hint.filter(|_| self.flash.is_none()) else {
            return false;
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("{GAP}{url}"),
                theme::current().status_notice,
            ))),
            area,
        );
        true
    }

    fn flash_span(&self) -> Option<Span<'static>> {
        self.flash.as_ref().map(|(message, _)| {
            Span::styled(format!("{GAP}{message}"), theme::current().status_notice)
        })
    }

    fn spinner(&self) -> char {
        spinner_frame(self.started_at.elapsed().as_millis())
    }

    /// The spinner's columns, held open while nothing spins so the chips after
    /// them stay under the pointer when a turn starts or ends.
    fn spinner_slot(&self, ctx: &StatusBarContext<'_>) -> Span<'static> {
        let style = if *ctx.status == Status::Streaming {
            theme::current().spinner
        } else if ctx.restoring {
            theme::current().status_notice
        } else {
            return Span::raw(SPINNER_SLOT_BLANK);
        };
        Span::styled(format!("{GAP}{}", self.spinner()), style)
    }

    /// The mode, the way back from a task, the sandbox and the chat name, in
    /// the order both layouts draw them.
    fn push_settings(&mut self, left: &mut Strip<'_>, ctx: &StatusBarContext<'_>, width: u16) {
        left.chip(
            ctx,
            StatusBarHitTarget::Mode,
            [Span::styled(ctx.mode.full().clone(), ctx.mode.style)],
        );
        if !ctx.main_chat {
            left.chip(
                ctx,
                StatusBarHitTarget::BackToMain,
                [Span::styled(
                    BACK_TO_MAIN_LABEL,
                    theme::current().status_notice,
                )],
            );
        }
        let width = usize::from(width);
        if let Some(name) = ctx.sandbox {
            let budget = width
                .saturating_sub(left.width() + GAP.width())
                .saturating_sub(critical_right(ctx));
            if let Some(label) = sandbox_label(name, budget) {
                left.chip(
                    ctx,
                    StatusBarHitTarget::Sandbox,
                    [Span::styled(
                        label,
                        control_style(ctx, StatusBarHitTarget::Sandbox),
                    )],
                );
            }
        }
        if let Some(name) = ctx.chat_name {
            self.push_chat_name(left, ctx, name, width);
        }
    }

    /// Bounded to a slot of its own, so a long name narrows itself rather than
    /// the rest of the bar. A name cut short scrolls under the pointer, which
    /// is all its hit is for.
    fn push_chat_name(
        &mut self,
        left: &mut Strip<'_>,
        ctx: &StatusBarContext<'_>,
        name: &str,
        width: usize,
    ) {
        let wrapper_width = usize::from(ctx.main_chat) * BRACKET_WIDTH;
        let available = width
            .saturating_sub(left.width())
            .saturating_sub(critical_right(ctx))
            .saturating_sub(GAP.width());
        let slot_total = (width / CHAT_NAME_WIDTH_DIVISOR)
            .min(CHAT_NAME_MAX_WIDTH)
            .min(available);
        if slot_total <= wrapper_width {
            return;
        }
        let slot_width = slot_total - wrapper_width;
        let visible = self.marquee.render(
            StatusBarHitTarget::ChatName,
            name,
            slot_width,
            truncate_head(name, slot_width),
            ctx.hovered == Some(StatusBarHitTarget::ChatName),
        );
        let label = if ctx.main_chat {
            format!("[{visible}]")
        } else {
            visible.into_owned()
        };
        let span = Span::styled(label, theme::current().status_dim);
        if name.width() > slot_width {
            left.chip(ctx, StatusBarHitTarget::ChatName, [span]);
        } else {
            left.push(Span::raw(GAP));
            left.push(span);
        }
    }
}

/// One side of a row, laid out left to right. A control names the run of spans
/// it drew rather than the columns they landed on, so respelling a span moves
/// every hit after it along with its glyphs.
#[derive(Default)]
struct Strip<'a> {
    spans: Vec<Span<'a>>,
    controls: Vec<(StatusBarHitTarget, Range<usize>)>,
}

impl<'a> Strip<'a> {
    fn width(&self) -> usize {
        self.spans.iter().map(Span::width).sum()
    }

    fn push(&mut self, span: Span<'a>) {
        self.spans.push(span);
    }

    /// Draws `spans` as one control, reversed together under the pointer.
    fn control(
        &mut self,
        ctx: &StatusBarContext<'_>,
        target: StatusBarHitTarget,
        spans: impl IntoIterator<Item = Span<'a>>,
    ) {
        let hovered = clickable(ctx, target) && ctx.hovered == Some(target);
        let start = self.spans.len();
        self.spans.extend(spans.into_iter().map(|span| Span {
            style: hover_style(span.style, hovered),
            ..span
        }));
        self.controls.push((target, start..self.spans.len()));
    }

    /// A control a gap after whatever came before it. The gap stays plain, so
    /// a hover reverses the label alone.
    fn chip(
        &mut self,
        ctx: &StatusBarContext<'_>,
        target: StatusBarHitTarget,
        spans: impl IntoIterator<Item = Span<'a>>,
    ) {
        self.push(Span::raw(GAP));
        self.control(ctx, target, spans);
    }

    /// A chip whose label carries its own padding, as the measured labels do.
    fn padded(
        &mut self,
        ctx: &StatusBarContext<'_>,
        target: StatusBarHitTarget,
        text: &str,
        style: Style,
    ) {
        let body = text.trim();
        if body.is_empty() {
            self.push(Span::raw(text.to_owned()));
            return;
        }
        let lead = text.len() - text.trim_start().len();
        let tail = lead + body.len();
        if lead > 0 {
            self.push(Span::raw(text[..lead].to_owned()));
        }
        self.control(ctx, target, [Span::styled(body.to_owned(), style)]);
        if tail < text.len() {
            self.push(Span::raw(text[tail..].to_owned()));
        }
    }

    /// Swaps the text of a control's first span and keeps its style.
    fn respell(&mut self, target: StatusBarHitTarget, text: Cow<'a, str>) {
        if let Some((_, range)) = self.controls.iter().find(|(drawn, _)| *drawn == target) {
            self.spans[range.start].content = text;
        }
    }

    fn append(&mut self, other: Self) {
        let base = self.spans.len();
        self.spans.extend(other.spans);
        self.controls.extend(
            other
                .controls
                .into_iter()
                .map(|(target, range)| (target, range.start + base..range.end + base)),
        );
    }

    /// Each control's offset and width, in columns from the strip's left edge.
    fn hits(&self) -> impl Iterator<Item = (StatusBarHitTarget, usize, usize)> + '_ {
        let columns = |spans: &[Span<'_>]| spans.iter().map(Span::width).sum::<usize>();
        self.controls.iter().map(move |(target, range)| {
            (
                *target,
                columns(&self.spans[..range.start]),
                columns(&self.spans[range.clone()]),
            )
        })
    }
}

fn push_resume(strip: &mut Strip<'_>, ctx: &StatusBarContext<'_>) {
    if !ctx.auto_scroll {
        strip.chip(
            ctx,
            StatusBarHitTarget::ResumeAutoScroll,
            [Span::styled(
                AUTO_SCROLL_PAUSED_LABEL,
                theme::current().status_dim,
            )],
        );
    }
}

fn push_goal(strip: &mut Strip<'_>, ctx: &StatusBarContext<'_>) {
    if let Some(goal) = ctx.goal {
        let label = format!(
            "[goal · {} · {}]",
            goal.evaluations,
            format_goal_elapsed(goal.elapsed())
        );
        strip.chip(
            ctx,
            StatusBarHitTarget::Goal,
            [Span::styled(label, theme::current().status_notice)],
        );
    }
}

fn push_activity(strip: &mut Strip<'_>, ctx: &StatusBarContext<'_>) {
    for (count, label, target) in [
        (ctx.active_tasks, TASKS_LABEL, StatusBarHitTarget::Tasks),
        (ctx.active_shells, SHELLS_LABEL, StatusBarHitTarget::Shells),
    ] {
        if count > 0 {
            strip.chip(
                ctx,
                target,
                [Span::styled(
                    format!("[{label} · {count}]"),
                    theme::current().status_notice,
                )],
            );
        }
    }
}

/// The error and its countdown answer as one control, and under the pointer
/// the countdown says what a click does instead.
fn push_retry(strip: &mut Strip<'_>, ctx: &StatusBarContext<'_>) {
    let Some(retry) = ctx.retry_info else {
        return;
    };
    let countdown = if clickable(ctx, StatusBarHitTarget::Retry)
        && ctx.hovered == Some(StatusBarHitTarget::Retry)
    {
        RETRY_NOW_LABEL.to_owned()
    } else {
        let secs = retry
            .deadline
            .saturating_duration_since(Instant::now())
            .as_secs();
        format!(" · retrying in {secs}s (#{})", retry.attempt)
    };
    strip.chip(
        ctx,
        StatusBarHitTarget::Retry,
        [
            Span::styled(retry.message.clone(), theme::current().status_retry_error),
            Span::styled(countdown, theme::current().status_retry_info),
        ],
    );
}

fn push_workflows(strip: &mut Strip<'_>, ctx: &StatusBarContext<'_>, fit: Fit) {
    if let Some(label) = fit.workflow_label(ctx) {
        strip.padded(
            ctx,
            StatusBarHitTarget::Workflows,
            label,
            control_style(ctx, StatusBarHitTarget::Workflows),
        );
    }
}

fn push_meters(strip: &mut Strip<'_>, ctx: &StatusBarContext<'_>, fit: Fit, spend: &SpendText) {
    let counters = Style::new().fg(theme::current().foreground);
    if let Some(text) = fit.context_text(spend) {
        // Past the border the next turn compacts, which is worth saying even at
        // a width that had to drop the border itself.
        let style = if spend.over_border {
            theme::current().todo_in_progress
        } else {
            counters
        };
        match fit.gauge(spend) {
            Some(gauge) => {
                let counts = text.trim_start();
                strip.push(Span::raw(text[..text.len() - counts.len()].to_owned()));
                strip.control(
                    ctx,
                    StatusBarHitTarget::Context,
                    gauge
                        .spans(style)
                        .into_iter()
                        .chain([Span::styled(format!("{GAP}{counts}"), style)]),
                );
            }
            None => strip.padded(ctx, StatusBarHitTarget::Context, &text, style),
        }
    }
    if let Some(text) = fit.money_text(spend) {
        strip.padded(ctx, StatusBarHitTarget::Usage, &text, counters);
    }
}

fn error_span(message: &str) -> Span<'static> {
    Span::styled(format!("{GAP}{message}"), theme::current().error)
}

/// Respells the mode short when the columns that frees win back a rung past
/// the model leaf. The provider prefix alone is not worth a word of the mode,
/// and a row that fits without the saving keeps the word.
fn shorten_mode(left: &mut Strip<'_>, ctx: &StatusBarContext<'_>, width: u16, split: bool) {
    let budget = right_budget(width, left);
    let saving = ctx
        .mode
        .full()
        .width()
        .saturating_sub(ctx.mode.short().width());
    let spend = SpendText::new(&ctx.stats);
    let pair = model_pair(ctx);
    let pair = pair.as_deref();
    let scope = split.then_some(Band::Settings);
    let (_, full_rank) = fit_right(ctx, &spend, pair, budget, scope);
    let (short, short_rank) = fit_right(ctx, &spend, pair, budget.saturating_add(saving), scope);
    if short.model != ModelTier::Full && short_rank < full_rank {
        left.respell(StatusBarHitTarget::Mode, ctx.mode.short().clone());
    }
}

/// The columns the right side of a row may fill. The cwd and the model carry
/// no padding of their own, so a gap is held back or a right side that fits
/// exactly would run into the last chip on the left.
fn right_budget(width: u16, left: &Strip<'_>) -> usize {
    usize::from(width).saturating_sub(left.width() + GAP.width())
}

/// Draws a row's two sides, the right one flush against the edge, and records
/// a hit for every control that survived the clipping.
fn draw_row(
    frame: &mut Frame,
    area: Rect,
    ctx: &StatusBarContext<'_>,
    left: Strip<'_>,
    right: Strip<'_>,
    hits: &mut Vec<StatusBarHit>,
) {
    let [left_area, right_area] = status_areas(area, right.width());
    for (side, side_area) in [(&left, left_area), (&right, right_area)] {
        for (target, offset, width) in side.hits() {
            push_hit(hits, ctx, side_area, offset, width, target);
        }
    }
    frame.render_widget(Paragraph::new(Line::from(left.spans)), left_area);
    frame.render_widget(
        Paragraph::new(Line::from(right.spans)).alignment(Alignment::Right),
        right_area,
    );
}

/// Walks [`LADDER`] until the fixed chips and the model fit, then hands the cwd
/// whatever is left. The cwd goes last because the model names what answers you
/// and the path is usually already in the shell prompt.
#[cfg(test)]
fn right_side<'a>(ctx: &'a StatusBarContext<'_>, cwd_label: &'a str, budget: usize) -> Strip<'a> {
    right_side_animated(ctx, cwd_label, budget, None, false)
}

/// On a split footer the workflow chip and the meters belong to the live row,
/// so the settings row fits and draws the rest alone.
fn right_side_animated<'a>(
    ctx: &'a StatusBarContext<'_>,
    cwd_label: &'a str,
    budget: usize,
    mut marquee: Option<&mut Marquee>,
    split: bool,
) -> Strip<'a> {
    let spend = SpendText::new(&ctx.stats);
    let pair = model_pair(ctx);
    let pair = pair.as_deref();
    let (fit, _) = fit_right(ctx, &spend, pair, budget, split.then_some(Band::Settings));

    let mut chips = Strip::default();
    if let Some(level) = fit.thinking_label(ctx) {
        chips.padded(
            ctx,
            StatusBarHitTarget::Thinking,
            &format!(" [{level}]"),
            control_style(ctx, StatusBarHitTarget::Thinking),
        );
    }
    if ctx.fast && fit.fast {
        chips.padded(
            ctx,
            StatusBarHitTarget::Fast,
            FAST_LABEL,
            control_style(ctx, StatusBarHitTarget::Fast),
        );
    }
    if !split {
        push_workflows(&mut chips, ctx, fit);
    }
    if let Some(label) = fit.permission_label(ctx) {
        let (target, style) = if ctx.permission_mode == PermissionMode::Yolo {
            (StatusBarHitTarget::Yolo, theme::current().error)
        } else {
            (StatusBarHitTarget::Auto, theme::current().tool_warning)
        };
        chips.padded(ctx, target, label, style);
    }
    if !split {
        push_meters(&mut chips, ctx, fit, &spend);
    }

    let residue = budget.saturating_sub(chips.width());
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
    let clipped = !cwd_static.is_empty() && cwd_static != cwd_label;
    let cwd = if clipped && ctx.hovered == Some(StatusBarHitTarget::Cwd) {
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

    let mut side = Strip::default();
    let cwd = Span::styled(cwd, theme::current().status_dim);
    if clipped {
        side.control(ctx, StatusBarHitTarget::Cwd, [cwd]);
    } else {
        side.push(cwd);
    }
    side.push(Span::raw(separator));
    side.control(
        ctx,
        StatusBarHitTarget::Model,
        [Span::styled(
            model,
            control_style(ctx, StatusBarHitTarget::Model),
        )],
    );
    side.append(chips);
    side
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

/// Walks the rungs of `scope` until its chips fit `budget`, and says how many
/// it took. `None` walks the whole ladder, for a footer that is one row.
fn fit_right(
    ctx: &StatusBarContext<'_>,
    spend: &SpendText,
    pair: Option<&str>,
    budget: usize,
    scope: Option<Band>,
) -> (Fit, usize) {
    let mut fit = Fit::FULL;
    let mut rank = 0;
    for step in LADDER
        .into_iter()
        .filter(|step| scope.is_none_or(|band| step.band() == band))
    {
        if fit.width(ctx, spend, pair, scope) <= budget {
            return (fit, rank);
        }
        fit.apply(step);
        rank += 1;
    }
    (fit, rank)
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

fn push_decisions_status(strip: &mut Strip, ctx: &StatusBarContext<'_>) {
    if ctx.decisions_offline && ctx.permission_mode != PermissionMode::Yolo {
        strip.push(Span::styled(
            format!("{GAP}{DECISIONS_OFFLINE_LABEL}"),
            theme::current().tool_warning,
        ));
    }
}

fn control_style(ctx: &StatusBarContext<'_>, target: StatusBarHitTarget) -> Style {
    if clickable(ctx, target) {
        theme::current().status_notice
    } else {
        theme::current().status_dim
    }
}

/// The columns the right-hand side keeps whatever the left side asks for: the
/// gap that parts the two sides, the model control's own floor, plus the yolo
/// sigil a bypassed session has to carry at every width.
fn critical_right(ctx: &StatusBarContext<'_>) -> usize {
    GAP.width()
        + model_floor(ctx)
        + PermissionTier::Sigil
            .label(&ctx.permission_mode)
            .map_or(0, UnicodeWidthStr::width)
}

/// The widest rung the columns hold, with the name cut into its own slot first
/// so a long one narrows the chip rather than the rest of the bar. The head is
/// kept because an instance name separates on its prefix.
fn sandbox_label(name: &str, budget: usize) -> Option<String> {
    let name = truncate_head(name, SANDBOX_NAME_MAX_WIDTH);
    SANDBOX_TIERS.into_iter().find_map(|prefix| {
        (prefix.width() + name.width() + SANDBOX_SUFFIX.width() <= budget)
            .then(|| format!("{prefix}{name}{SANDBOX_SUFFIX}"))
    })
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
pub(crate) fn model_leaf(id: &str) -> &str {
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

fn status_areas(area: Rect, right_width: usize) -> [Rect; 2] {
    Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(u16::try_from(right_width).unwrap_or(u16::MAX)),
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

pub(super) fn collapse_home(path: &str) -> String {
    let Some(home) = caudra_storage::paths::home() else {
        return path.to_string();
    };
    collapse_home_with(path, &home.to_string_lossy())
}

/// Matched by component, so a sibling that only shares the text of the home
/// directory's name is left as it is.
fn collapse_home_with(path: &str, home: &str) -> String {
    match Path::new(path).strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => HOME_ABBREVIATION.to_owned(),
        Ok(rest) => format!("{HOME_ABBREVIATION}{MAIN_SEPARATOR}{}", rest.display()),
        Err(_) => path.to_owned(),
    }
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
    use ratatui::buffer::Cell;
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
    const GAUGE_COUNTS_GLYPHS: &str = "▕▌░░░░░░░░│▏ 12k/200k (6%/90%)";
    /// Past [`HALF_WINDOW`], with the fill covering the cell the border is in.
    const PAST_BORDER_SIZE: u32 = 120_000;
    const HALF_WINDOW: u32 = 100_000;
    const GAUGE_MISSING_MSG: &str = "a known window must draw its gauge";
    const AMBER_FILL_MSG: &str = "past the border the fill must turn amber with the counter";
    const DIM_FRAME_MSG: &str = "the frame and the track stay dim whatever the fill says";
    const GAUGE_FIRST_MSG: &str = "the gauge must be the first thing pressure takes";
    const UNKNOWN_WINDOW_MSG: &str = "a window of zero has nothing to fill and nothing to measure";
    /// Where one row has long since abbreviated everything.
    const SPLIT_BAR_WIDTH: u16 = 80;
    const SETTINGS_ROW: u16 = 0;
    const LIVE_ROW: u16 = 1;
    const SPLIT_TIER_MSG: &str = "a split footer must keep every chip at its widest at 80 columns";
    const LIVE_PRESSURE_MSG: &str = "activity on the live row must never cost a setting a column";
    const SETTINGS_PRESSURE_MSG: &str = "a crowded settings row must never cost a meter a column";
    const PRESSURE_PREMISE_MSG: &str = "the crowded row did not have to give anything up";
    const MODEL_MOVED_MSG: &str = "a backoff moved the control that switches away from it";
    const ERROR_MESSAGE: &str = "Stream error: overloaded_error (/continue resumes the turn)";
    const ERROR_CUT_MSG: &str = "an error must be drawn whole, remedy included";
    const ERROR_CROWDED_MSG: &str = "an error must leave the settings row as it was";
    const HOVERED_URL: &str = "https://example.com/docs";
    const HINT_MISSING_MSG: &str = "a hovered link must take the live row";
    const HINT_SPREAD_MSG: &str = "a hovered link must leave the settings row as it was";
    const ACTIVITY_MOVED_MSG: &str = "a turn starting moved a chip out from under the pointer";
    const SPINNER_MISSING_MSG: &str = "a streaming turn must spin in the slot held open for it";
    /// A chip's closing bracket against the model's opening one, or against
    /// the cwd the test bar is drawn in.
    const TOUCHING_SIDES: [&str; 2] = ["][", "]."];
    const TOUCHING_MSG: &str = "a right side that fits exactly ran into the left side";
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
    const MISSING_YOLO_HIT_MSG: &str = "a bypassed session must be switchable back from the footer";
    const MISSING_AUTO_HIT_MSG: &str = "an auto session must be switchable back from the footer";
    const MISSING_FAST_HIT_MSG: &str = "a fast session must be switchable back from the footer";
    const UNCLICKABLE_YOLO_MSG: &str = "the bar drew the yolo chip without a hit to click it";
    const TASK_LEVEL_MISSING: &str = "a task footer must name the level that task runs at";
    const TASK_LEVEL_CLICKABLE: &str = "a task footer's level is a label, not a control";
    const SANDBOX_NAME: &str = "prowlix-instance";
    const SANDBOX_CHIP: &str = "[sandbox: prowlix-instance]";
    const SANDBOX_BARE_CHIP: &str = "[prowlix-instance]";
    /// Longer than the name's slot, so the chip has to cut it rather than take
    /// the columns the rest of the left side is drawn in.
    const LONG_SANDBOX_NAME: &str = "prowlix-instance-with-a-very-long-name";
    const LONG_SANDBOX_CHIP: &str = "[sandbox: prowlix-instance-with-..]";
    const LONG_SANDBOX_BARE_CHIP: &str = "[prowlix-instance-with-..]";
    const MISSING_SANDBOX_HIT_MSG: &str =
        "an attached session must reach its sandbox from the footer";
    const SANDBOX_CLIPPED_MSG: &str =
        "the bar drew part of the sandbox chip with no hit to click it";
    const SANDBOX_CROWDED_MSG: &str = "the sandbox chip pushed a footer control off the bar";
    const SANDBOX_GREW_MSG: &str = "narrowing the bar widened the sandbox chip";
    const NO_SHORT_MODE_MSG: &str = "no width drew the short mode label beside the sandbox chip";

    /// The glyphs a hit claims, read back out of the bar it was measured on.
    fn bar_glyphs(text: &str, hit: &StatusBarHit) -> String {
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
        status: &'a Status,
        context_size: u32,
        context_window: u32,
        compaction_border: Option<u32>,
        global_cost: Option<f64>,
        show_global: bool,
        permission_mode: PermissionMode,
        decisions_offline: bool,
        fast: bool,
        hovered: Option<StatusBarHitTarget>,
        hover_hint: Option<&'a str>,
        goal: Option<&'a GoalSnapshot>,
        active_tasks: usize,
        active_shells: usize,
        retry_info: Option<&'a RetryInfo>,
        workflows: Option<WorkflowChip>,
        main_chat: bool,
        model_id: &'a str,
        pending_model: Option<&'a str>,
        chat_name: Option<&'a str>,
        sandbox: Option<&'a str>,
        auto_scroll: bool,
    }

    impl Default for Fixture<'_> {
        fn default() -> Self {
            Self {
                width: BAR_WIDTH,
                status: &Status::Idle,
                context_size: CONTEXT_SIZE,
                context_window: crate::components::TEST_CONTEXT_WINDOW,
                compaction_border: None,
                global_cost: None,
                show_global: false,
                permission_mode: PermissionMode::Ask,
                decisions_offline: false,
                fast: false,
                hovered: None,
                hover_hint: None,
                goal: None,
                active_tasks: 0,
                active_shells: 0,
                retry_info: None,
                workflows: None,
                main_chat: true,
                model_id: MODEL_ID,
                pending_model: None,
                chat_name: None,
                sandbox: None,
                auto_scroll: true,
            }
        }
    }

    impl<'a> Fixture<'a> {
        fn into_ctx(self) -> StatusBarContext<'a> {
            StatusBarContext {
                status: self.status,
                mode: ModeLabel {
                    full: MODE_LABEL.into(),
                    short: MODE_SHORT_LABEL.into(),
                    style: Style::new(),
                },
                model_id: self.model_id,
                pending_model: self.pending_model.map(Cow::Borrowed),
                stats: UsageStats {
                    global_cost: self.global_cost,
                    show_global: self.show_global,
                    ..usage(
                        self.context_size,
                        self.context_window,
                        self.compaction_border,
                    )
                },
                auto_scroll: self.auto_scroll,
                chat_name: self.chat_name,
                sandbox: self.sandbox,
                main_chat: self.main_chat,
                retry_info: self.retry_info,
                thinking: Some(THINKING_LEVEL.into()),
                fast: self.fast,
                workflows: self.workflows,
                permission_mode: self.permission_mode,
                decisions_offline: self.decisions_offline,
                restoring: false,
                goal: self.goal,
                active_tasks: self.active_tasks,
                active_shells: self.active_shells,
                bash_input: false,
                hovered: self.hovered,
                hover_hint: self.hover_hint,
            }
        }
    }

    /// The chat's own price and no session total, which is what every test
    /// that is not about the money wants.
    fn usage(context_size: u32, context_window: u32, compaction_border: Option<u32>) -> UsageStats {
        UsageStats {
            global_cost: None,
            global_subscription_cost: None,
            context_size,
            cost: Some(CHAT_COST),
            subscription_cost: None,
            context_window,
            compaction_border,
            show_global: false,
        }
    }

    /// One frame of the footer: each row's glyphs and cell styles, and the
    /// hits the frame recorded.
    struct Drawn {
        rows: Vec<String>,
        styles: Vec<Vec<Style>>,
        hits: Vec<StatusBarHit>,
    }

    impl Drawn {
        fn row(&self, row: u16) -> &str {
            &self.rows[usize::from(row)]
        }

        fn hits_on(&self, row: u16) -> Vec<StatusBarHit> {
            self.hits
                .iter()
                .filter(|hit| hit.area.y == row)
                .copied()
                .collect()
        }

        fn hit(&self, target: StatusBarHitTarget) -> StatusBarHit {
            *self
                .hits
                .iter()
                .find(|hit| hit.target == target)
                .expect(MISSING_HIT_MSG)
        }

        fn glyphs(&self, hit: StatusBarHit) -> String {
            bar_glyphs(self.row(hit.area.y), &hit)
        }

        fn styles(&self, hit: StatusBarHit) -> &[Style] {
            &self.styles[usize::from(hit.area.y)]
                [usize::from(hit.area.x)..usize::from(hit.area.right())]
        }
    }

    fn draw(ctx: &StatusBarContext<'_>, width: u16, rows: u16) -> Drawn {
        let mut bar = StatusBar::new(FLASH_TTL, ".", false);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, rows)).unwrap();
        let mut hits = Vec::new();
        terminal
            .draw(|f| {
                hits = bar.view(f, f.area(), ctx);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let cells = |row: u16| (0..width).filter_map(move |column| buffer.cell((column, row)));
        Drawn {
            rows: (0..rows)
                .map(|row| cells(row).map(Cell::symbol).collect())
                .collect(),
            styles: (0..rows)
                .map(|row| cells(row).map(Cell::style).collect())
                .collect(),
            hits,
        }
    }

    fn render_at(fixture: Fixture<'_>) -> (String, Vec<StatusBarHit>, Vec<Style>) {
        let width = fixture.width;
        let Drawn {
            mut rows,
            mut styles,
            hits,
        } = draw(&fixture.into_ctx(), width, SINGLE_ROW);
        (rows.swap_remove(0), hits, styles.swap_remove(0))
    }

    /// The fixture on a terminal tall enough to split the footer.
    fn render_rows(fixture: Fixture<'_>) -> Drawn {
        let width = fixture.width;
        draw(&fixture.into_ctx(), width, SPLIT_ROWS)
    }

    fn render(global_cost: Option<f64>, show_global: bool, yolo: bool) -> String {
        render_at(Fixture {
            global_cost,
            show_global,
            permission_mode: PermissionMode::from(yolo),
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
            sandbox: None,
            main_chat: true,
            retry_info: None,
            thinking: Some(LADDER_THINKING.into()),
            fast: true,
            workflows: ladder_workflows(),
            permission_mode: PermissionMode::Yolo,
            decisions_offline: false,
            restoring: false,
            goal: None,
            active_tasks: 0,
            active_shells: 0,
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

    fn side_text(side: &Strip<'_>) -> String {
        side.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn side_hit(side: &Strip<'_>, target: StatusBarHitTarget) -> Option<(usize, usize)> {
        side.hits()
            .find(|(hit, _, _)| *hit == target)
            .map(|(_, offset, width)| (offset, width))
    }

    /// The glyphs a hit claims, read back out of the spans it was measured on.
    fn hit_glyphs(side: &Strip<'_>, target: StatusBarHitTarget) -> String {
        drawn_glyphs(side, target).expect(MISSING_HIT_MSG)
    }

    /// The same, for a control the bar is free to have dropped entirely.
    fn drawn_glyphs(side: &Strip<'_>, target: StatusBarHitTarget) -> Option<String> {
        let (offset, width) = side_hit(side, target)?;
        Some(side_text(side).chars().skip(offset).take(width).collect())
    }

    /// Reads the drawn glyphs rather than the [`Fit`], so a tier that measures
    /// one way and draws another still counts as absent.
    fn visible_chips(side: &Strip<'_>) -> Vec<Chip> {
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
    /// columns to a modal. The gauge draws the counter a second time, so the
    /// two are one control.
    #[test_case(StatusBarHitTarget::Context, GAUGE_COUNTS_GLYPHS ; "gauge_and_counter_open_context")]
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

    /// The gauge draws the counter a second time, so it goes before anything
    /// that only one chip says, on the live row and on a one-row footer alike.
    #[test_case(Some(Band::Live) ; "live_row")]
    #[test_case(None             ; "one_row")]
    fn the_gauge_is_the_first_thing_pressure_takes(scope: Option<Band>) {
        with_ladder_ctx(|ctx| {
            let spend = SpendText::new(&ctx.stats);
            let full_width = Fit::FULL.width(ctx, &spend, None, scope);
            let (fit, _) = fit_right(ctx, &spend, None, full_width - 1, scope);

            assert_eq!(
                fit,
                Fit {
                    context: ContextTier::Counts,
                    ..Fit::FULL
                },
                "{GAUGE_FIRST_MSG}"
            );
        });
    }

    #[test]
    fn provider_is_the_first_full_tier_to_go() {
        with_ladder_ctx(|ctx| {
            let spend = SpendText::new(&ctx.stats);
            let counts = Fit {
                context: ContextTier::Counts,
                ..Fit::FULL
            };
            let counts_width = counts.width(ctx, &spend, None, None);
            let (fit, _) = fit_right(ctx, &spend, None, counts_width - 1, None);

            assert_eq!(
                fit,
                Fit {
                    model: ModelTier::Leaf,
                    ..counts
                }
            );
        });
    }

    #[test_case(0       => "▕░░░░░░░░░░▏" ; "an_empty_window")]
    #[test_case(12_000  => "▕▌░░░░░░░░░▏" ; "a_sliver_rounds_down_to_its_eighths")]
    #[test_case(25_000  => "▕█▎░░░░░░░░▏" ; "a_cell_and_a_quarter")]
    #[test_case(100_000 => "▕█████░░░░░▏" ; "half_the_window")]
    #[test_case(199_999 => "▕█████████▉▏" ; "a_token_short_of_full")]
    #[test_case(200_000 => "▕██████████▏" ; "a_full_window")]
    #[test_case(300_000 => "▕██████████▏" ; "an_overfull_window_stays_full")]
    fn the_gauge_fills_in_eighths(context_size: u32) -> String {
        gauge_glyphs(context_size, None)
    }

    /// The tick marks the cell auto-compaction fires in for as long as the
    /// fill has not entered it.
    #[test_case(12_000,  COMPACTION_BORDER => "▕▌░░░░░░░░│▏" ; "the_default_border")]
    #[test_case(12_000,  20_000            => "▕▌│░░░░░░░░▏" ; "the_cell_after_the_fill")]
    #[test_case(180_000, COMPACTION_BORDER => "▕█████████│▏" ; "a_fill_that_just_reached_it")]
    #[test_case(12_000,  10_000            => "▕▌░░░░░░░░░▏" ; "a_border_inside_the_last_filled_cell")]
    #[test_case(190_000, COMPACTION_BORDER => "▕█████████▌▏" ; "a_fill_past_it")]
    fn the_gauge_marks_the_compaction_border(context_size: u32, border: u32) -> String {
        gauge_glyphs(context_size, Some(border))
    }

    fn gauge_glyphs(context_size: u32, compaction_border: Option<u32>) -> String {
        let stats = usage(
            context_size,
            crate::components::TEST_CONTEXT_WINDOW,
            compaction_border,
        );
        Gauge::new(&stats)
            .expect(GAUGE_MISSING_MSG)
            .spans(Style::new())
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// The fill turns amber with the counter it draws, and the frame and the
    /// track stay dim, so the fill is what catches the eye.
    #[test]
    fn a_gauge_past_the_border_turns_amber() {
        let drawn = render_rows(Fixture {
            context_size: PAST_BORDER_SIZE,
            compaction_border: Some(HALF_WINDOW),
            ..Default::default()
        });
        let hit = drawn.hit(StatusBarHitTarget::Context);
        let glyphs = drawn.glyphs(hit);
        let colour = |glyph: char| {
            let column = glyphs
                .chars()
                .position(|drawn| drawn == glyph)
                .expect(GAUGE_MISSING_MSG);
            drawn.styles(hit)[column].fg
        };
        let theme = theme::current();

        assert_eq!(
            colour(FULL_CELL),
            theme.todo_in_progress.fg,
            "{AMBER_FILL_MSG}"
        );
        for dim in [EMPTY_CELL, GAUGE_OPEN.chars().next().unwrap()] {
            assert_eq!(colour(dim), theme.status_dim.fg, "{DIM_FRAME_MSG}");
        }
    }

    /// Without a window there is nothing to fill, and the gauge measures as
    /// nothing too, so the ladder never budgets columns nobody draws on.
    #[test]
    fn an_unknown_window_draws_no_gauge() {
        let fixture = || Fixture {
            context_window: 0,
            ..Default::default()
        };
        let ctx = fixture().into_ctx();
        let spend = SpendText::new(&ctx.stats);
        let counts = Fit {
            context: ContextTier::Counts,
            ..Fit::FULL
        };

        assert_eq!(
            Fit::FULL.width(&ctx, &spend, None, Some(Band::Live)),
            counts.width(&ctx, &spend, None, Some(Band::Live)),
            "{UNKNOWN_WINDOW_MSG}"
        );
        let drawn = render_rows(fixture());
        assert!(
            !drawn.row(LIVE_ROW).contains(GAUGE_OPEN),
            "{UNKNOWN_WINDOW_MSG}: {}",
            drawn.row(LIVE_ROW)
        );
    }

    /// At 80 columns a single row has shed most of its tiers. Split, each row
    /// only has its own chips to fit, and every one of them fits whole.
    #[test]
    fn a_split_footer_keeps_every_chip_at_eighty_columns() {
        with_ladder_ctx(|ctx| {
            let drawn = draw(ctx, SPLIT_BAR_WIDTH, SPLIT_ROWS);
            for (row, chips) in [
                (
                    SETTINGS_ROW,
                    [
                        MODE_LABEL,
                        LADDER_MODEL_ID,
                        FULL_THINKING_CHIP,
                        FAST_LABEL.trim(),
                        YOLO_LABEL.trim(),
                    ]
                    .as_slice(),
                ),
                (
                    LIVE_ROW,
                    [LADDER_WORKFLOW_CHIP, GAUGE_COUNTS_GLYPHS, MONEY_GLYPHS].as_slice(),
                ),
            ] {
                for chip in chips {
                    assert!(
                        drawn.row(row).contains(chip),
                        "{SPLIT_TIER_MSG}: {chip} missing from {}",
                        drawn.row(row)
                    );
                }
            }
        });
    }

    /// A turn's worth of activity and a paused transcript crowd the live row
    /// until its meters give way, and the settings row keeps every column.
    #[test]
    fn live_pressure_never_shortens_a_setting() {
        let goal = active_goal();
        let quiet = render_rows(Fixture {
            width: SPLIT_BAR_WIDTH,
            ..Default::default()
        });
        let busy = render_rows(Fixture {
            width: SPLIT_BAR_WIDTH,
            goal: Some(&goal),
            active_tasks: 1,
            active_shells: 1,
            workflows: ladder_workflows(),
            auto_scroll: false,
            ..Default::default()
        });

        assert!(
            quiet.row(LIVE_ROW).contains(GAUGE_OPEN) && !busy.row(LIVE_ROW).contains(GAUGE_OPEN),
            "{PRESSURE_PREMISE_MSG}: {}",
            busy.row(LIVE_ROW)
        );
        assert_eq!(
            busy.row(SETTINGS_ROW),
            quiet.row(SETTINGS_ROW),
            "{LIVE_PRESSURE_MSG}"
        );
        assert_eq!(
            busy.hits_on(SETTINGS_ROW),
            quiet.hits_on(SETTINGS_ROW),
            "{LIVE_PRESSURE_MSG}"
        );
    }

    /// A long sandbox and chat name squeeze the settings row down to the
    /// model's floor, and the live row keeps every meter it had.
    #[test]
    fn settings_pressure_never_compacts_a_meter() {
        let quiet = render_rows(Fixture {
            width: SPLIT_BAR_WIDTH,
            ..Default::default()
        });
        let crowded = render_rows(Fixture {
            width: SPLIT_BAR_WIDTH,
            model_id: LADDER_MODEL_ID,
            sandbox: Some(LONG_SANDBOX_NAME),
            chat_name: Some(LONG_SANDBOX_NAME),
            fast: true,
            permission_mode: PermissionMode::Yolo,
            ..Default::default()
        });

        assert!(
            !crowded.row(SETTINGS_ROW).contains(LADDER_MODEL_ID),
            "{PRESSURE_PREMISE_MSG}: {}",
            crowded.row(SETTINGS_ROW)
        );
        assert_eq!(
            crowded.row(LIVE_ROW),
            quiet.row(LIVE_ROW),
            "{SETTINGS_PRESSURE_MSG}"
        );
        assert_eq!(
            crowded.hits_on(LIVE_ROW),
            quiet.hits_on(LIVE_ROW),
            "{SETTINGS_PRESSURE_MSG}"
        );
    }

    /// A backoff is when switching models matters most, so the control that
    /// does it stays exactly where it was while the countdown runs.
    #[test]
    fn a_retry_never_moves_the_model_control() {
        let retry = RetryInfo {
            attempt: RETRY_ATTEMPT,
            message: RETRY_MESSAGE.into(),
            deadline: Instant::now() + RETRY_REMAINING,
        };
        let settled = render_rows(Fixture {
            width: SPLIT_BAR_WIDTH,
            ..Default::default()
        });
        let backing_off = render_rows(Fixture {
            width: SPLIT_BAR_WIDTH,
            retry_info: Some(&retry),
            ..Default::default()
        });

        assert_eq!(
            backing_off.hit(StatusBarHitTarget::Model),
            settled.hit(StatusBarHitTarget::Model),
            "{MODEL_MOVED_MSG}"
        );
    }

    /// An error ends in the remedy that clears it, so it is never cut short,
    /// and the columns it takes come out of the live row alone.
    #[test]
    fn an_error_leaves_the_settings_row_whole() {
        let status = Status::error(ERROR_MESSAGE.into());
        let idle = render_rows(Fixture {
            width: SPLIT_BAR_WIDTH,
            ..Default::default()
        });
        let failed = render_rows(Fixture {
            width: SPLIT_BAR_WIDTH,
            status: &status,
            ..Default::default()
        });

        assert!(
            failed.row(LIVE_ROW).contains(ERROR_MESSAGE),
            "{ERROR_CUT_MSG}: {}",
            failed.row(LIVE_ROW)
        );
        assert_eq!(
            failed.row(SETTINGS_ROW),
            idle.row(SETTINGS_ROW),
            "{ERROR_CROWDED_MSG}"
        );
        assert_eq!(
            failed.hits_on(SETTINGS_ROW),
            idle.hits_on(SETTINGS_ROW),
            "{ERROR_CROWDED_MSG}"
        );
    }

    #[test]
    fn a_hovered_link_takes_the_live_row_only() {
        let plain = render_rows(Fixture::default());
        let hovering = render_rows(Fixture {
            hover_hint: Some(HOVERED_URL),
            ..Default::default()
        });

        assert_eq!(
            hovering.row(LIVE_ROW).trim(),
            HOVERED_URL,
            "{HINT_MISSING_MSG}"
        );
        assert!(hovering.hits_on(LIVE_ROW).is_empty(), "{HINT_MISSING_MSG}");
        assert_eq!(
            hovering.row(SETTINGS_ROW),
            plain.row(SETTINGS_ROW),
            "{HINT_SPREAD_MSG}"
        );
        assert_eq!(
            hovering.hits_on(SETTINGS_ROW),
            plain.hits_on(SETTINGS_ROW),
            "{HINT_SPREAD_MSG}"
        );
    }

    /// The spinner's slot is held open while nothing spins, so a chip under
    /// the pointer stays under it when a turn starts or ends.
    #[test]
    fn activity_chips_stay_put_when_a_turn_starts() {
        let idle = render_rows(Fixture {
            active_tasks: 1,
            ..Default::default()
        });
        let streaming = render_rows(Fixture {
            status: &Status::Streaming,
            active_tasks: 1,
            ..Default::default()
        });

        assert_ne!(
            streaming.row(LIVE_ROW),
            idle.row(LIVE_ROW),
            "{SPINNER_MISSING_MSG}"
        );
        assert_eq!(
            streaming.hit(StatusBarHitTarget::Tasks),
            idle.hit(StatusBarHitTarget::Tasks),
            "{ACTIVITY_MOVED_MSG}"
        );
    }

    /// The cwd and the model carry no padding of their own, so wherever the
    /// right side fits exactly it would run into the last chip on the left
    /// unless the row holds a gap back for it.
    #[test_case(SINGLE_ROW ; "one_row")]
    #[test_case(SPLIT_ROWS ; "split")]
    fn the_right_side_never_runs_into_the_left(rows: u16) {
        for width in 1..=BAR_WIDTH {
            let ctx = Fixture {
                width,
                sandbox: Some(SANDBOX_NAME),
                active_tasks: 1,
                active_shells: 1,
                permission_mode: PermissionMode::Yolo,
                ..Default::default()
            }
            .into_ctx();
            for row in draw(&ctx, width, rows).rows {
                assert!(
                    TOUCHING_SIDES.iter().all(|touch| !row.contains(touch)),
                    "{TOUCHING_MSG} at {width}: {row}"
                );
            }
        }
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
            bar.cadence(&Status::Idle, false, false),
            Cadence::due(MARQUEE_STEP)
        );
    }

    #[test]
    fn a_long_chat_name_is_bounded_and_hover_only() {
        const LONG_NAME: &str = "a-session-name-longer-than-the-footer-can-afford";
        let (_, hits, _) = render_at(Fixture {
            chat_name: Some(LONG_NAME),
            permission_mode: PermissionMode::Yolo,
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
            permission_mode: PermissionMode::Yolo,
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
        let (text, hits, _) = render_at(Fixture {
            hover_hint: Some(HOVERED_URL),
            ..Default::default()
        });

        assert!(text.trim_start().starts_with(HOVERED_URL));
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

    #[test_case(0, 0; "empty")]
    #[test_case(1, 0; "tasks_only")]
    #[test_case(0, 1; "shells_only")]
    #[test_case(9, 10; "mixed_digits")]
    #[test_case(100, 99; "three_digits")]
    fn activity_chips_have_independent_counts_and_exact_hits(tasks: usize, shells: usize) {
        for main_chat in [true, false] {
            let (text, hits, _) = render_at(Fixture {
                active_tasks: tasks,
                active_shells: shells,
                main_chat,
                ..Default::default()
            });
            for (count, label, target) in [
                (tasks, TASKS_LABEL, StatusBarHitTarget::Tasks),
                (shells, SHELLS_LABEL, StatusBarHitTarget::Shells),
            ] {
                let hit = hits.iter().find(|hit| hit.target == target);
                assert_eq!(hit.is_some(), count > 0);
                assert_eq!(target.scope(), ChatScope::Any);
                if let Some(hit) = hit {
                    assert_eq!(bar_glyphs(&text, hit), format!("[{label} · {count}]"));
                    assert_eq!(text.chars().nth(usize::from(hit.area.x) - 1), Some(' '));
                } else {
                    assert!(!text.contains(&format!("[{label} ·")));
                }
            }
        }
    }

    #[test_case(StatusBarHitTarget::Tasks; "tasks")]
    #[test_case(StatusBarHitTarget::Shells; "shells")]
    fn activity_chips_keep_notice_style_and_hover_in_any_chat(target: StatusBarHitTarget) {
        for main_chat in [true, false] {
            for hovered in [None, Some(target)] {
                let (_, hits, styles) = render_at(Fixture {
                    active_tasks: 1,
                    active_shells: 1,
                    main_chat,
                    hovered,
                    ..Default::default()
                });
                let hit = hits.iter().find(|hit| hit.target == target).unwrap();
                let start = usize::from(hit.area.x);
                let end = usize::from(hit.area.right());
                assert!(!styles[start - 1].add_modifier.contains(Modifier::REVERSED));
                for style in &styles[start..end] {
                    assert_eq!(style.fg, theme::current().status_notice.fg);
                    assert_eq!(
                        style.add_modifier.contains(Modifier::REVERSED),
                        hovered.is_some()
                    );
                }
            }
        }
    }

    #[test_case(false; "plain")]
    #[test_case(true; "goal_retry_workflows_sandbox")]
    fn activity_hits_follow_mode_shortening_and_never_claim_clipped_chips(crowded: bool) {
        let goal = active_goal();
        let retry = RetryInfo {
            message: RETRY_MESSAGE.into(),
            deadline: Instant::now() + RETRY_REMAINING,
            attempt: RETRY_ATTEMPT,
        };
        let mut shortened = false;
        for width in 0..=240 {
            let (text, hits, _) = render_at(Fixture {
                width,
                active_tasks: 10,
                active_shells: 2,
                goal: crowded.then_some(&goal),
                retry_info: crowded.then_some(&retry),
                workflows: crowded.then(ladder_workflows).flatten(),
                sandbox: crowded.then_some(SANDBOX_NAME),
                ..Default::default()
            });
            for (target, label, count) in [
                (StatusBarHitTarget::Tasks, TASKS_LABEL, 10),
                (StatusBarHitTarget::Shells, SHELLS_LABEL, 2),
            ] {
                let label = format!("[{label} · {count}]");
                if let Some(hit) = hits.iter().find(|hit| hit.target == target) {
                    assert_eq!(bar_glyphs(&text, hit), label);
                    assert!(hit.area.right() <= width);
                    shortened |= text.contains(MODE_SHORT_LABEL);
                    assert!(
                        hits.iter()
                            .filter(|other| other.target != target)
                            .all(|other| { other.area.intersection(hit.area).is_empty() })
                    );
                } else {
                    assert!(!text.contains(&label), "{MISSING_HIT_MSG}");
                }
            }
            if width == 240 && crowded {
                for target in [
                    StatusBarHitTarget::Goal,
                    StatusBarHitTarget::Tasks,
                    StatusBarHitTarget::Shells,
                    StatusBarHitTarget::Retry,
                    StatusBarHitTarget::Workflows,
                    StatusBarHitTarget::Sandbox,
                ] {
                    assert!(hits.iter().any(|hit| hit.target == target), "{target:?}");
                }
                assert!(text.find(GOAL_CHIP_PREFIX) < text.find("[tasks ·"));
                assert!(text.find("[tasks ·") < text.find("[shell ·"));
            }
        }
        assert!(shortened, "{NO_SHORT_MODE_MSG}");
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

        assert_eq!(bar_glyphs(&text, hit), AUTO_SCROLL_PAUSED_LABEL);
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
            Some(hit) => assert_eq!(bar_glyphs(&text, hit), AUTO_SCROLL_PAUSED_LABEL),
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

    /// The chip is the whole control: the space ahead of it separates it from
    /// whatever the bar drew last and must not answer the pointer.
    #[test]
    fn a_bypassed_session_offers_a_yolo_control() {
        let (text, hits, _) = render_at(Fixture {
            permission_mode: PermissionMode::Yolo,
            ..Default::default()
        });
        let hit = hits
            .iter()
            .find(|hit| hit.target == StatusBarHitTarget::Yolo)
            .expect(MISSING_YOLO_HIT_MSG);

        assert_eq!(bar_glyphs(&text, hit), YOLO_LABEL.trim());
        assert_eq!(text.chars().nth(usize::from(hit.area.x) - 1), Some(' '));
        assert!(hit.target.accepts_click());
    }

    #[test]
    fn a_prompting_session_has_no_yolo_control() {
        let (text, hits, _) = render_at(Fixture::default());

        assert!(!text.contains(YOLO_LABEL.trim()));
        assert!(!text.contains(AUTO_LABEL.trim()));
        assert!(hits.iter().all(|hit| !matches!(
            hit.target,
            StatusBarHitTarget::Yolo | StatusBarHitTarget::Auto
        )));
    }

    #[test_case(PermissionMode::Ask, SINGLE_ROW; "ask_single")]
    #[test_case(PermissionMode::Auto, SINGLE_ROW; "auto_single")]
    #[test_case(PermissionMode::Yolo, SINGLE_ROW; "yolo_single")]
    #[test_case(PermissionMode::Ask, SPLIT_ROWS; "ask_split")]
    #[test_case(PermissionMode::Auto, SPLIT_ROWS; "auto_split")]
    #[test_case(PermissionMode::Yolo, SPLIT_ROWS; "yolo_split")]
    fn decisions_offline_is_a_passive_warning_except_in_yolo(
        permission_mode: PermissionMode,
        rows: u16,
    ) {
        let yolo = permission_mode == PermissionMode::Yolo;
        let ctx = Fixture {
            permission_mode,
            decisions_offline: true,
            ..Default::default()
        }
        .into_ctx();
        let drawn = draw(&ctx, BAR_WIDTH, rows);
        let row = rows - 1;
        let text = drawn.row(row);
        if yolo {
            assert!(!text.contains(DECISIONS_OFFLINE_LABEL));
            return;
        }
        let offset = text.find(DECISIONS_OFFLINE_LABEL).unwrap();
        let start = text[..offset].width();
        let end = start + DECISIONS_OFFLINE_LABEL.width();
        assert!(
            drawn.styles[usize::from(row)][start..end]
                .iter()
                .all(|style| style.fg == theme::current().tool_warning.fg)
        );
        assert!(drawn.hits_on(row).iter().all(|hit| usize::from(hit.area.right()) <= start || usize::from(hit.area.x) >= end));
    }

    #[test_case(SINGLE_ROW; "single")]
    #[test_case(SPLIT_ROWS; "split")]
    fn decisions_status_is_absent_without_a_cached_failure(rows: u16) {
        let drawn = draw(&Fixture::default().into_ctx(), BAR_WIDTH, rows);
        assert!(
            drawn
                .rows
                .iter()
                .all(|row| !row.contains(DECISIONS_OFFLINE_LABEL))
        );
    }

    #[test_case(true; "main_chat")]
    #[test_case(false; "subagent_chat")]
    fn auto_permission_mode_has_its_own_footer_control(main_chat: bool) {
        let (text, hits, styles) = render_at(Fixture {
            permission_mode: PermissionMode::Auto,
            main_chat,
            hovered: Some(StatusBarHitTarget::Auto),
            ..Default::default()
        });
        let hit = hits
            .iter()
            .find(|hit| hit.target == StatusBarHitTarget::Auto)
            .expect(MISSING_AUTO_HIT_MSG);
        assert_eq!(bar_glyphs(&text, hit), AUTO_LABEL.trim());
        assert_eq!(hit.target.scope(), ChatScope::Any);
        assert!(hit.target.accepts_click());
        assert!(!text.contains(YOLO_LABEL.trim()));
        assert!(
            hits.iter()
                .all(|hit| hit.target != StatusBarHitTarget::Yolo)
        );
        let start = usize::from(hit.area.x);
        let end = usize::from(hit.area.right());
        assert!(!styles[start - 1].add_modifier.contains(Modifier::REVERSED));
        assert!(
            styles[start..end]
                .iter()
                .all(|style| style.add_modifier.contains(Modifier::REVERSED))
        );
    }

    #[test_case(3; "model_floor_only")]
    #[test_case(8; "very_narrow")]
    #[test_case(24; "cramped")]
    #[test_case(60; "roomy")]
    #[test_case(WIDE_BUDGET; "everything_fits")]
    fn auto_permission_hit_tracks_its_label_at_every_tier(budget: usize) {
        let ctx = Fixture {
            permission_mode: PermissionMode::Auto,
            ..Default::default()
        }
        .into_ctx();
        let side = right_side(&ctx, LADDER_CWD, budget);
        let text: String = side
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        match drawn_glyphs(&side, StatusBarHitTarget::Auto) {
            Some(glyphs) => {
                assert!([AUTO_LABEL.trim(), AUTO_SHORT_LABEL.trim()].contains(&glyphs.as_str()))
            }
            None => {
                assert!(!text.contains(AUTO_LABEL.trim()));
                assert!(!text.contains(AUTO_SHORT_LABEL.trim()));
            }
        }
        assert!(drawn_glyphs(&side, StatusBarHitTarget::Yolo).is_none());
    }

    /// The chip keeps its warning colour, so the hover has only the reversal to
    /// say the pointer is on a control.
    #[test]
    fn hovering_the_yolo_control_highlights_its_chip_alone() {
        let (_, hits, styles) = render_at(Fixture {
            permission_mode: PermissionMode::Yolo,
            hovered: Some(StatusBarHitTarget::Yolo),
            ..Default::default()
        });
        let hit = hits
            .iter()
            .find(|hit| hit.target == StatusBarHitTarget::Yolo)
            .expect(MISSING_YOLO_HIT_MSG);
        let start = usize::from(hit.area.x);
        let end = usize::from(hit.area.right());

        assert!(!styles[start - 1].add_modifier.contains(Modifier::REVERSED));
        assert!(
            styles[start..end]
                .iter()
                .all(|style| style.add_modifier.contains(Modifier::REVERSED))
        );
    }

    /// The chip is the whole control, exactly as the yolo one is: the space
    /// ahead of it separates it from the level chip and must not answer.
    #[test]
    fn a_fast_session_offers_a_fast_control() {
        let (text, hits, _) = render_at(Fixture {
            fast: true,
            ..Default::default()
        });
        let hit = hits
            .iter()
            .find(|hit| hit.target == StatusBarHitTarget::Fast)
            .expect(MISSING_FAST_HIT_MSG);

        assert_eq!(bar_glyphs(&text, hit), FAST_LABEL.trim());
        assert_eq!(text.chars().nth(usize::from(hit.area.x) - 1), Some(' '));
        assert!(hit.target.accepts_click());
    }

    #[test]
    fn hovering_the_fast_control_highlights_its_chip_alone() {
        let (_, hits, styles) = render_at(Fixture {
            fast: true,
            hovered: Some(StatusBarHitTarget::Fast),
            ..Default::default()
        });
        let hit = hits
            .iter()
            .find(|hit| hit.target == StatusBarHitTarget::Fast)
            .expect(MISSING_FAST_HIT_MSG);
        let start = usize::from(hit.area.x);
        let end = usize::from(hit.area.right());

        assert!(!styles[start - 1].add_modifier.contains(Modifier::REVERSED));
        assert!(
            styles[start..end]
                .iter()
                .all(|style| style.add_modifier.contains(Modifier::REVERSED))
        );
    }

    /// A task draws the fast flag its spawner chose, so the chip is a label
    /// there and the bar must not measure a hit a click could land on.
    #[test]
    fn a_subagent_footer_names_fast_mode_without_offering_a_control() {
        let (text, hits, _) = render_at(Fixture {
            fast: true,
            main_chat: false,
            ..Default::default()
        });

        assert!(text.contains(FAST_LABEL.trim()));
        assert_eq!(StatusBarHitTarget::Fast.scope(), ChatScope::MainOnly);
        assert!(
            hits.iter()
                .all(|hit| hit.target != StatusBarHitTarget::Fast)
        );
    }

    /// A task runs at a level of its own, so its footer names one. The setting
    /// behind the chip is still the session's, so the label answers nothing.
    #[test]
    fn a_subagent_footer_names_its_level_without_offering_a_control() {
        let (text, hits, _) = render_at(Fixture {
            main_chat: false,
            ..Default::default()
        });

        assert!(
            text.contains(&format!("[{THINKING_LEVEL}]")),
            "{TASK_LEVEL_MISSING}: {text}"
        );
        assert_eq!(StatusBarHitTarget::Thinking.scope(), ChatScope::MainOnly);
        assert!(
            hits.iter()
                .all(|hit| hit.target != StatusBarHitTarget::Thinking),
            "{TASK_LEVEL_CLICKABLE}"
        );
    }

    /// The bypass belongs to the session rather than to one transcript, so a
    /// task footer switches off the same one the main chat would.
    #[test]
    fn a_subagent_footer_offers_the_yolo_control() {
        let (_, hits, _) = render_at(Fixture {
            permission_mode: PermissionMode::Yolo,
            main_chat: false,
            ..Default::default()
        });

        assert_eq!(StatusBarHitTarget::Yolo.scope(), ChatScope::Any);
        assert!(
            hits.iter()
                .any(|hit| hit.target == StatusBarHitTarget::Yolo)
        );
    }

    /// A squeezed bar spells the chip `[!]` before it drops it, so the hit has
    /// to cover whichever spelling was drawn and go when neither is.
    #[test_case(3           ; "model_floor_only")]
    #[test_case(8           ; "very_narrow")]
    #[test_case(16          ; "narrow")]
    #[test_case(24          ; "cramped")]
    #[test_case(40          ; "medium")]
    #[test_case(60          ; "roomy")]
    #[test_case(WIDE_BUDGET ; "everything_fits")]
    fn a_yolo_hit_tracks_the_tier_it_was_drawn_on(budget: usize) {
        with_ladder_ctx(|ctx| {
            let side = right_side(ctx, LADDER_CWD, budget);
            match drawn_glyphs(&side, StatusBarHitTarget::Yolo) {
                Some(glyphs) => assert!(
                    [YOLO_LABEL.trim(), YOLO_SHORT_LABEL.trim()].contains(&glyphs.as_str()),
                    "{STALE_HIT_MSG}: {glyphs:?}"
                ),
                None => assert!(
                    !visible_chips(&side).contains(&Chip::Yolo),
                    "{UNCLICKABLE_YOLO_MSG}"
                ),
            }
        });
    }

    /// The chip is the whole control: the space ahead of it separates it from
    /// whatever the bar drew last and must not answer the pointer.
    #[test]
    fn an_attached_session_names_its_sandbox() {
        let (text, hits, _) = render_at(Fixture {
            sandbox: Some(SANDBOX_NAME),
            ..Default::default()
        });
        let hit = hits
            .iter()
            .find(|hit| hit.target == StatusBarHitTarget::Sandbox)
            .expect(MISSING_SANDBOX_HIT_MSG);

        assert_eq!(bar_glyphs(&text, hit), SANDBOX_CHIP);
        assert_eq!(text.chars().nth(usize::from(hit.area.x) - 1), Some(' '));
        assert!(hit.target.accepts_click());
    }

    /// A local runtime, a remote one with no sandbox and one whose connection
    /// has not finished authenticating all arrive here as `None`, so a bar that
    /// was told nothing can never say the session is on a sandbox.
    #[test]
    fn a_detached_session_has_no_sandbox_control() {
        let (text, hits, _) = render_at(Fixture::default());

        assert!(!text.contains(SANDBOX_PREFIX.trim()));
        assert!(
            hits.iter()
                .all(|hit| hit.target != StatusBarHitTarget::Sandbox)
        );
    }

    #[test]
    fn hovering_the_sandbox_control_highlights_its_chip_alone() {
        let (_, hits, styles) = render_at(Fixture {
            sandbox: Some(SANDBOX_NAME),
            hovered: Some(StatusBarHitTarget::Sandbox),
            ..Default::default()
        });
        let hit = hits
            .iter()
            .find(|hit| hit.target == StatusBarHitTarget::Sandbox)
            .expect(MISSING_SANDBOX_HIT_MSG);
        let start = usize::from(hit.area.x);
        let end = usize::from(hit.area.right());

        assert!(!styles[start - 1].add_modifier.contains(Modifier::REVERSED));
        assert!(
            styles[start..end]
                .iter()
                .all(|style| style.add_modifier.contains(Modifier::REVERSED))
        );
    }

    #[test]
    fn the_sandbox_chip_is_plain_without_the_pointer() {
        let (_, hits, styles) = render_at(Fixture {
            sandbox: Some(SANDBOX_NAME),
            ..Default::default()
        });
        let hit = hits
            .iter()
            .find(|hit| hit.target == StatusBarHitTarget::Sandbox)
            .expect(MISSING_SANDBOX_HIT_MSG);

        assert!(
            styles[usize::from(hit.area.x)..usize::from(hit.area.right())]
                .iter()
                .all(|style| !style.add_modifier.contains(Modifier::REVERSED))
        );
    }

    /// The runtime is the session's, not one transcript's, so a task footer
    /// names the same sandbox and opens the same manager.
    #[test]
    fn a_subagent_footer_names_the_sandbox() {
        let (_, hits, _) = render_at(Fixture {
            sandbox: Some(SANDBOX_NAME),
            main_chat: false,
            ..Default::default()
        });

        assert_eq!(StatusBarHitTarget::Sandbox.scope(), ChatScope::Any);
        assert!(
            hits.iter()
                .any(|hit| hit.target == StatusBarHitTarget::Sandbox)
        );
    }

    /// The chip gives up its word before its name and its name before the bar,
    /// and whatever survives keeps a hit that covers exactly what was drawn. A
    /// bar too narrow drops it rather than crowding the mode and model controls
    /// the rest of the footer is built around.
    #[test_case(12,        None                    ; "no_room_beside_the_model")]
    #[test_case(30,        None                    ; "too_narrow_for_the_bare_name")]
    #[test_case(31,        Some(SANDBOX_BARE_CHIP) ; "bare_name_exactly_fits")]
    #[test_case(39,        Some(SANDBOX_BARE_CHIP) ; "word_dropped_for_the_name")]
    #[test_case(40,        Some(SANDBOX_CHIP)      ; "named_in_full_exactly_fits")]
    #[test_case(BAR_WIDTH, Some(SANDBOX_CHIP)      ; "wide")]
    fn a_narrow_bar_squeezes_the_sandbox_chip_before_dropping_it(
        width: u16,
        expected: Option<&str>,
    ) {
        let (text, hits, _) = render_at(Fixture {
            width,
            sandbox: Some(SANDBOX_NAME),
            ..Default::default()
        });
        let drawn = hits
            .iter()
            .find(|hit| hit.target == StatusBarHitTarget::Sandbox)
            .map(|hit| bar_glyphs(&text, hit));

        assert_eq!(drawn.as_deref(), expected, "{STALE_HIT_MSG}");
        if expected.is_none() {
            assert!(
                !text.contains(SANDBOX_PREFIX.trim()),
                "{SANDBOX_CLIPPED_MSG}"
            );
            assert!(!text.contains(SANDBOX_NAME), "{SANDBOX_CLIPPED_MSG}");
        }
        for target in [StatusBarHitTarget::Mode, StatusBarHitTarget::Model] {
            assert!(
                hits.iter().any(|hit| hit.target == target),
                "{SANDBOX_CROWDED_MSG}: {target:?}"
            );
        }
    }

    /// Shortening the mode label moves every left-hand control that follows it,
    /// so the chip's hit has to travel with its glyphs rather than stay on the
    /// columns another control now draws.
    #[test]
    fn the_sandbox_hit_follows_a_shortened_mode_label() {
        let short_mode = format!(" {MODE_SHORT_LABEL}");
        let mut checked = 0;
        for width in 1..=BAR_WIDTH {
            let (text, hits, _) = render_at(Fixture {
                width,
                sandbox: Some(SANDBOX_NAME),
                ..Default::default()
            });
            let Some(hit) = hits
                .iter()
                .find(|hit| hit.target == StatusBarHitTarget::Sandbox)
                .filter(|_| text.starts_with(&short_mode))
            else {
                continue;
            };
            let glyphs = bar_glyphs(&text, hit);

            assert!(
                [SANDBOX_CHIP, SANDBOX_BARE_CHIP].contains(&glyphs.as_str()),
                "{STALE_HIT_MSG}: {glyphs:?}"
            );
            checked += 1;
        }

        assert!(checked > 0, "{NO_SHORT_MODE_MSG}");
    }

    /// Taking columns away can only cost the chip glyphs, never buy it any. A
    /// rung that came back on a narrower bar would mean something other than
    /// the width was deciding what fits.
    #[test]
    fn narrowing_the_bar_never_widens_the_sandbox_chip() {
        let mut widest = usize::MAX;
        for width in (1..=BAR_WIDTH).rev() {
            let (_, hits, _) = render_at(Fixture {
                width,
                sandbox: Some(SANDBOX_NAME),
                ..Default::default()
            });
            let drawn = hits
                .iter()
                .find(|hit| hit.target == StatusBarHitTarget::Sandbox)
                .map_or(0, |hit| usize::from(hit.area.width));

            assert!(drawn <= widest, "{SANDBOX_GREW_MSG}: {width} holds {drawn}");
            widest = drawn;
        }
    }

    #[test_case(48, Some(SANDBOX_CHIP)      ; "named_in_full")]
    #[test_case(27, Some(SANDBOX_CHIP)      ; "named_exactly_fits")]
    #[test_case(26, Some(SANDBOX_BARE_CHIP) ; "word_dropped_for_the_name")]
    #[test_case(18, Some(SANDBOX_BARE_CHIP) ; "bare_name_exactly_fits")]
    #[test_case(17, None                    ; "below_the_bare_name")]
    #[test_case(0,  None                    ; "no_columns")]
    fn sandbox_label_cases(budget: usize, expected: Option<&str>) {
        assert_eq!(sandbox_label(SANDBOX_NAME, budget).as_deref(), expected);
    }

    /// A name past its slot is cut before any rung is measured, so the chip
    /// stays a chip however long the instance was called: the columns beyond
    /// the slot belong to the rest of the bar, not to one more syllable.
    #[test_case(48, Some(LONG_SANDBOX_CHIP)      ; "named_in_full")]
    #[test_case(35, Some(LONG_SANDBOX_CHIP)      ; "named_exactly_fits")]
    #[test_case(34, Some(LONG_SANDBOX_BARE_CHIP) ; "word_dropped_for_the_name")]
    #[test_case(25, None                         ; "below_the_bare_name")]
    fn a_long_instance_name_is_cut_into_the_chips_slot(budget: usize, expected: Option<&str>) {
        assert_eq!(
            sandbox_label(LONG_SANDBOX_NAME, budget).as_deref(),
            expected
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
    #[test_case("/home/user2/app", "/home/user", "/home/user2/app"        ; "sibling_sharing_the_home_prefix")]
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
                permission_mode: PermissionMode::Yolo,
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
    const TASK_CONTROLS: [StatusBarHitTarget; 3] = [
        StatusBarHitTarget::BackToMain,
        StatusBarHitTarget::Context,
        StatusBarHitTarget::Usage,
    ];
    const TASK_HIT_MSG: &str = "a task's bar offers exactly the controls a task owns";
    const INERT_RETRY_MSG: &str = "a task's countdown is drawn even though it cannot be clicked";
    /// Wide enough that every chip survives the ladder, so a control missing
    /// from the hits is one the scope refused rather than one the width dropped.
    const TASK_BAR_WIDTH: u16 = 200;

    /// A task's bar still draws the session's model, reasoning level, workflow
    /// count and goal, because they describe the run the task belongs to, and
    /// its own backoff, because that is the task the user is watching. Only the
    /// controls that read the transcript in front of you, or leave it, answer
    /// the pointer.
    #[test]
    fn a_task_bar_offers_only_the_controls_a_task_owns() {
        let goal = active_goal();
        let retry = RetryInfo {
            attempt: RETRY_ATTEMPT,
            message: RETRY_MESSAGE.into(),
            deadline: Instant::now() + RETRY_REMAINING,
        };
        let (text, hits, _) = render_at(Fixture {
            width: TASK_BAR_WIDTH,
            main_chat: false,
            goal: Some(&goal),
            retry_info: Some(&retry),
            workflows: ladder_workflows(),
            ..Default::default()
        });

        assert!(text.contains(RETRY_COUNTDOWN_PREFIX), "{INERT_RETRY_MSG}");
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
