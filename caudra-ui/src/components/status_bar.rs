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
const YOLO_LABEL: &str = " [yolo]";

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
    pub context_size: u32,
    pub cost: Option<f64>,
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
    pub thinking_label: Option<Cow<'static, str>>,
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
                let pct = if ctx.stats.context_window > 0 {
                    (ctx.stats.context_size as f64 / ctx.stats.context_window as f64 * 100.0) as u32
                } else {
                    0
                };

                let left_width = left_spans.iter().map(Span::width).sum::<usize>();
                let right_budget = (area.width as usize).saturating_sub(left_width);
                let min_model_width = if ctx.settings_clickable { 3 } else { 1 };
                let thinking_width = ctx
                    .thinking_label
                    .as_ref()
                    .map_or(0, |label| label.width() + 3);
                let mut show_thinking = ctx.thinking_label.is_some();
                let mut show_fast = ctx.fast;
                let mut show_workflow = ctx.workflow;
                let mut show_yolo = ctx.yolo;
                let core_width = |thinking, fast, workflow, yolo| {
                    thinking_width * usize::from(thinking)
                        + FAST_LABEL.width() * usize::from(fast)
                        + WORKFLOW_LABEL.width() * usize::from(workflow)
                        + YOLO_LABEL.width() * usize::from(yolo)
                };
                if core_width(show_thinking, show_fast, show_workflow, show_yolo) + min_model_width
                    > right_budget
                {
                    show_workflow = false;
                }
                if core_width(show_thinking, show_fast, show_workflow, show_yolo) + min_model_width
                    > right_budget
                {
                    show_fast = false;
                }
                if core_width(show_thinking, show_fast, show_workflow, show_yolo) + min_model_width
                    > right_budget
                {
                    show_thinking = false;
                }
                if core_width(show_thinking, show_fast, show_workflow, show_yolo) + min_model_width
                    > right_budget
                {
                    show_yolo = false;
                }

                let mut rest_spans = Vec::new();
                let mut thinking_offset = None;

                if show_thinking && let Some(ref label) = ctx.thinking_label {
                    thinking_offset = Some(rest_spans.iter().map(Span::width).sum::<usize>() + 1);
                    rest_spans.push(Span::raw(" "));
                    rest_spans.push(Span::styled(
                        format!("[{label}]"),
                        hover_style(
                            if ctx.settings_clickable {
                                theme::current().status_notice
                            } else {
                                theme::current().status_dim
                            },
                            ctx.settings_clickable
                                && ctx.hovered == Some(StatusBarHitTarget::Thinking),
                        ),
                    ));
                }

                if show_fast {
                    rest_spans.push(Span::styled(FAST_LABEL, theme::current().status_dim));
                }
                if show_workflow {
                    rest_spans.push(Span::styled(WORKFLOW_LABEL, theme::current().status_dim));
                }
                if show_yolo {
                    rest_spans.push(Span::styled(YOLO_LABEL, theme::current().error));
                }

                let context_text = format!(
                    "  {}/{} ({}%)",
                    format_tokens(ctx.stats.context_size),
                    format_tokens(ctx.stats.context_window),
                    pct,
                );
                let rest_text = match ctx.stats.cost {
                    Some(cost) => format!("{context_text} ${cost:.3} "),
                    None => format!("{context_text} "),
                };
                let global_text = ctx
                    .stats
                    .global_cost
                    .filter(|_| ctx.stats.show_global)
                    .map(|global| format!(" \u{03a3}${global:.3} "));
                let core_width = rest_spans.iter().map(Span::width).sum::<usize>();
                let full_width = rest_text.width();
                let global_width = global_text.as_ref().map_or(0, |text| text.width());
                if core_width + min_model_width + full_width + global_width <= right_budget {
                    rest_spans.push(Span::styled(
                        rest_text,
                        Style::new().fg(theme::current().foreground),
                    ));
                    if let Some(global_text) = global_text {
                        rest_spans.push(Span::styled(
                            global_text,
                            Style::new().fg(theme::current().foreground),
                        ));
                    }
                } else if core_width + min_model_width + full_width <= right_budget {
                    rest_spans.push(Span::styled(
                        rest_text,
                        Style::new().fg(theme::current().foreground),
                    ));
                } else {
                    let compact_context = format!("  {pct}% ");
                    if core_width + min_model_width + compact_context.width() <= right_budget {
                        rest_spans.push(Span::styled(
                            compact_context,
                            Style::new().fg(theme::current().foreground),
                        ));
                    }
                }

                let reserved = left_spans
                    .iter()
                    .chain(rest_spans.iter())
                    .map(Span::width)
                    .sum::<usize>();
                let available = (area.width as usize).saturating_sub(reserved);
                let model_budget = (available / 2).max(min_model_width).min(available);
                let model = if ctx.settings_clickable {
                    bracketed_tail(ctx.model_id, model_budget)
                } else {
                    truncate_tail(ctx.model_id, model_budget)
                };
                let separator = if model.is_empty() {
                    ""
                } else {
                    CWD_MODEL_SEPARATOR
                };
                let cwd = truncate_tail(
                    &self.cwd_branch,
                    available
                        .saturating_sub(model.width())
                        .saturating_sub(separator.width()),
                );
                let separator = if cwd.is_empty() { "" } else { separator };

                let model_offset = cwd.width() + separator.width();
                let model_width = model.width();
                let thinking_offset =
                    thinking_offset.map(|offset| model_offset + model_width + offset);

                right_spans.push(Span::styled(cwd, theme::current().status_dim));
                right_spans.push(Span::raw(separator));
                right_spans.push(Span::styled(
                    model,
                    hover_style(
                        if ctx.settings_clickable {
                            theme::current().status_notice
                        } else {
                            theme::current().status_dim
                        },
                        ctx.settings_clickable && ctx.hovered == Some(StatusBarHitTarget::Model),
                    ),
                ));
                right_spans.append(&mut rest_spans);
                model_hit = Some((model_offset, model_width));
                thinking_hit =
                    thinking_offset.zip(ctx.thinking_label.as_ref().map(|label| label.width() + 2));
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
    const MODEL_ID: &str = "test-model";
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
            mode_label: "[BUILD]".into(),
            mode_style: Style::new(),
            model_id: MODEL_ID,
            stats: UsageStats {
                global_cost,
                context_size: CONTEXT_SIZE,
                cost: Some(CHAT_COST),
                context_window: crate::components::TEST_CONTEXT_WINDOW,
                show_global,
            },
            auto_scroll: true,
            chat_name: None,
            back_to_main: false,
            retry_info: None,
            thinking_label: Some("thinking: off".into()),
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
