//! The `/autopilot` panel, and the wording every autopilot surface shares: a
//! decision's line, why an engagement ended, and the notes a card carries
//! while autopilot decides it or once it left it to the user.

use caudra_agent::autopilot::{ActionName, AutopilotDecision};
use caudra_agent::permissions::AutopilotNote;
use caudra_config::AutopilotConfig;
use caudra_grab::grab_scope;
use caudra_providers::model_registry::Binding;
use caudra_storage::autopilot::{AutopilotBudget, AutopilotEnd, StoredAutopilot};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::modal::{CLOSE_HINT, ESC_LABEL, FooterHits, FooterLine, Modal, SEPARATOR};
use super::scrollbar::{Scrollbar, ScrollbarMouse};
use super::{ModalScroll, Overlay};
use crate::theme;

const DECIDING_NOTE: &str = "Autopilot is deciding. Press any key to answer yourself.";
const LEFT_TO_YOU_NOTE: &str = "Autopilot left this to you";
const SENTENCE_END: char = '.';
pub(crate) const STOPPED: &str = "stopped";
pub(crate) const GOAL_MET: &str = "goal met";
pub(crate) const GOAL_IMPOSSIBLE: &str = "goal impossible";
pub(crate) const CONTINUATIONS_SPENT: &str = "continuation limit reached";
pub(crate) const COST_SPENT: &str = "cost limit reached";
pub(crate) const TIME_SPENT: &str = "time limit reached";
pub(crate) const ERROR_END: &str = "error";
const LEFT_TO_YOU_LEAD: &str = "left the";
const LEFT_TO_YOU_TAIL: &str = "to you";
const REASON_SEPARATOR: &str = " · ";
/// Joins the words of a kind's wire name, such as `second_opinion`.
const NAME_WORD_JOINER: char = '_';
const WORD_SEPARATOR: &str = " ";
pub(crate) const ENGAGED_STATE: &str = "Engaged";
pub(crate) const DECIDING_STATE: &str = "Deciding";
pub(crate) const WAITING_STATE: &str = "Waiting on you";
const ENDED_STATE: &str = "Ended";
const OWNED_GOAL_HEADING: &str = "Goal (autopilot's own)";
const GOAL_HEADING: &str = "Goal";
const NO_GOAL: &str = "No goal";
const BUDGET_HEADING: &str = "Budget";
const CONTINUATIONS_LABEL: &str = "Continuations  ";
const API_COST_LABEL: &str = "API cost       ";
const SUBSCRIPTION_LABEL: &str = "Subscription   ";
const TIME_LABEL: &str = "Time           ";
const MODEL_LABEL: &str = "Model          ";
const NO_LIMIT: &str = "no limit";
const NOT_CAPPED: &str = "covered, not capped";
const MINUTES: &str = "min";
const UNBOUND_MODEL: &str = "unbound";
const WAITING_HEADING: &str = "Waiting on you";
const DECISIONS_HEADING: &str = "Recent decisions";
const NO_DECISIONS: &str = "None yet";
const ITEM_BULLET: &str = "- ";
const LEFT_SUFFIX: &str = "left";
const BUDGET_SEPARATOR: &str = " · ";
const CONTINUATIONS_WORD: &str = "continuations";
const MS_PER_MINUTE: u64 = 60_000;
const TITLE: &str = "Autopilot";
const WIDTH_PERCENT: u16 = 72;
const MAX_HEIGHT_PERCENT: u16 = 70;
const H_PAD: u16 = 2;
pub(crate) const STOP_KEY: char = 's';
pub(crate) const MODEL_KEY: char = 'm';
const CLOSE_KEY: char = 'q';
pub(crate) const STOP_LABEL: &str = "s";
const STOP_HINT: &str = " stop";
pub(crate) const MODEL_LABEL_KEY: &str = "m";
pub(crate) const CLOSE_LABEL: &str = "q";
const MODEL_HINT: &str = " model";
const TARGETS: [PanelTarget; 3] = [PanelTarget::Stop, PanelTarget::Model, PanelTarget::Close];

/// What the panel asks the host to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelAction {
    Stop,
    Model,
}

/// A footer control, indexed the way [`FooterLine`] targets are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelTarget {
    Stop,
    Model,
    Close,
}

/// What the panel reads, gathered by the App each frame it is drawn.
pub(crate) struct PanelContent<'a> {
    pub stored: &'a StoredAutopilot,
    pub limits: &'a AutopilotConfig,
    pub goal: Option<&'a str>,
    pub model: Option<&'a Binding>,
    pub deciding: bool,
    /// What autopilot left to the user and is still waiting, one line each.
    pub escalations: &'a [String],
    pub decisions: &'a [AutopilotDecision],
}

pub struct AutopilotPanel {
    open: bool,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    popup: Rect,
    footer: FooterHits,
}

impl Default for AutopilotPanel {
    fn default() -> Self {
        Self {
            open: false,
            scroll: ModalScroll::new_top(),
            scrollbar: Scrollbar::default(),
            popup: Rect::default(),
            footer: FooterHits::default(),
        }
    }
}

impl AutopilotPanel {
    pub fn open(&mut self) {
        self.open = true;
        self.scroll.reset();
        self.footer.clear();
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Option<PanelAction> {
        match key.code {
            KeyCode::Esc | KeyCode::Char(CLOSE_KEY) => {
                self.close();
                None
            }
            KeyCode::Char(STOP_KEY) => Some(PanelAction::Stop),
            KeyCode::Char(MODEL_KEY) => Some(PanelAction::Model),
            _ => {
                self.scroll.handle_key(key);
                None
            }
        }
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.open && self.popup.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    #[cfg(test)]
    pub(crate) fn footer_hit(&self, target: PanelTarget) -> Rect {
        TARGETS
            .iter()
            .position(|candidate| *candidate == target)
            .map(|index| self.footer.hit(index))
            .unwrap_or_default()
    }

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> Option<PanelAction> {
        match self.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return None,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                return None;
            }
        }
        match self
            .footer
            .handle_mouse(event)
            .and_then(|index| TARGETS.get(index).copied())?
        {
            PanelTarget::Stop => Some(PanelAction::Stop),
            PanelTarget::Model => Some(PanelAction::Model),
            PanelTarget::Close => {
                self.close();
                None
            }
        }
    }

    pub(crate) fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        content: &PanelContent<'_>,
    ) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("autopilot_panel", area);
        let width = Modal::inner_width(area.width, WIDTH_PERCENT).saturating_sub(H_PAD * 2);
        let mut lines = content_lines(content);
        let footer = footer();
        lines.push(Line::default());
        lines.push(footer.line(None));
        let total = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(width) as u16;
        let modal = Modal {
            title: TITLE,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, total);
        let padded = Rect {
            x: inner.x + H_PAD,
            width: inner.width.saturating_sub(H_PAD * 2),
            ..inner
        };
        self.scroll.update_dimensions(total, padded.height);
        let offset = self.scroll.offset();
        self.footer.set(footer.hits(padded, offset, total));
        if let Some(index) = self.footer.hovered()
            && let Some(last) = lines.last_mut()
        {
            *last = footer.line(Some(index));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((offset, 0)),
            padded,
        );
        self.scrollbar.draw(frame, inner, total, offset);
        self.popup = popup;
        popup
    }
}

impl Overlay for AutopilotPanel {
    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        self.open = false;
        self.scroll.reset();
        self.footer.reset();
    }
}

pub(crate) fn end_reason(reason: &AutopilotEnd) -> &str {
    match reason {
        AutopilotEnd::Error { message } => message,
        other => end_notice(other),
    }
}

/// The end in fixed words, for text that can leave the terminal: an
/// error's message stays out of it.
pub(crate) fn end_notice(reason: &AutopilotEnd) -> &'static str {
    match reason {
        AutopilotEnd::Stopped { .. } => STOPPED,
        AutopilotEnd::GoalMet => GOAL_MET,
        AutopilotEnd::GoalImpossible => GOAL_IMPOSSIBLE,
        AutopilotEnd::Budget {
            kind: AutopilotBudget::Continuations,
        } => CONTINUATIONS_SPENT,
        AutopilotEnd::Budget {
            kind: AutopilotBudget::Cost,
        } => COST_SPENT,
        AutopilotEnd::Budget {
            kind: AutopilotBudget::Time,
        } => TIME_SPENT,
        AutopilotEnd::Error { .. } => ERROR_END,
    }
}

/// What was decided about what, and why. An escalation reads as the thing
/// handed back to the user. `subject` goes in as given, so a caller drawing
/// markdown quotes it first.
pub(crate) fn decision_text(kind: &str, action: &str, subject: &str, reason: &str) -> String {
    let kind = kind.replace(NAME_WORD_JOINER, WORD_SEPARATOR);
    let head = if action == ActionName::Escalate.name() {
        format!("{LEFT_TO_YOU_LEAD} {kind} {LEFT_TO_YOU_TAIL}")
    } else {
        format!("{kind} {action}")
    };
    if subject.is_empty() {
        format!("{head}{REASON_SEPARATOR}{reason}")
    } else {
        format!("{head}: {subject}{REASON_SEPARATOR}{reason}")
    }
}

/// The line a card carries while autopilot decides it or once autopilot
/// handed it back.
pub(crate) fn card_note(note: &AutopilotNote) -> String {
    match note {
        AutopilotNote::Deciding => DECIDING_NOTE.to_owned(),
        AutopilotNote::Escalated { reason } => format!(
            "{LEFT_TO_YOU_NOTE}: {}{SENTENCE_END}",
            reason.trim().trim_end_matches(SENTENCE_END)
        ),
    }
}

/// The goal in one line, saying when autopilot set it.
pub(crate) fn goal_line(goal: Option<&str>, owns_goal: bool) -> String {
    match goal {
        Some(goal) if owns_goal => format!("{OWNED_GOAL_HEADING}: {goal}"),
        Some(goal) => format!("{GOAL_HEADING}: {goal}"),
        None => NO_GOAL.to_owned(),
    }
}

/// The engagement's state in a word or two.
pub(crate) fn state_label(stored: &StoredAutopilot, deciding: bool, escalated: bool) -> String {
    match &stored.last_end {
        Some(end) if !stored.engaged => format!("{ENDED_STATE}: {}", end_reason(end)),
        _ if escalated => WAITING_STATE.to_owned(),
        _ if deciding => DECIDING_STATE.to_owned(),
        _ => ENGAGED_STATE.to_owned(),
    }
}

/// What the engagement may still spend, limit by limit, such as
/// `88 continuations left · $1.50 left · 25 min left`.
pub(crate) fn budget_left(stored: &StoredAutopilot, limits: &AutopilotConfig) -> String {
    let mut left = vec![format!(
        "{} {CONTINUATIONS_WORD} {LEFT_SUFFIX}",
        limits
            .max_continuations
            .saturating_sub(stored.continuations)
    )];
    if let Some(max) = limits.max_cost {
        left.push(format!(
            "${:.2} {LEFT_SUFFIX}",
            (max - stored.cost_usd).max(0.0)
        ));
    }
    if let Some(max) = limits.max_minutes {
        left.push(format!(
            "{} {MINUTES} {LEFT_SUFFIX}",
            u64::from(max).saturating_sub(stored.elapsed_ms / MS_PER_MINUTE)
        ));
    }
    left.join(BUDGET_SEPARATOR)
}

fn footer() -> FooterLine {
    let theme = theme::current();
    let mut footer = FooterLine::default();
    footer.command(STOP_LABEL, theme.keybind_key);
    footer.describe(STOP_HINT, theme.tool_dim);
    footer.text(SEPARATOR, theme.tool_dim);
    footer.command(MODEL_LABEL_KEY, theme.keybind_key);
    footer.describe(MODEL_HINT, theme.tool_dim);
    footer.text(SEPARATOR, theme.tool_dim);
    footer.command(ESC_LABEL, theme.keybind_key);
    footer.describe(CLOSE_HINT, theme.tool_dim);
    footer
}

fn content_lines(content: &PanelContent<'_>) -> Vec<Line<'static>> {
    let theme = theme::current();
    let stored = content.stored;
    let escalated = !content.escalations.is_empty();
    let state_style = if !stored.engaged {
        theme.status_dim
    } else if escalated {
        theme.todo_in_progress
    } else {
        theme.status_notice
    };
    let goal_heading = if stored.owns_goal {
        OWNED_GOAL_HEADING
    } else {
        GOAL_HEADING
    };
    let mut lines = vec![
        Line::from(Span::styled(
            state_label(stored, content.deciding, escalated),
            state_style,
        )),
        Line::default(),
        Line::from(Span::styled(goal_heading, theme.panel_title)),
        match content.goal {
            Some(goal) => Line::from(goal.to_owned()),
            None => Line::from(Span::styled(NO_GOAL, theme.status_dim)),
        },
        Line::default(),
        Line::from(Span::styled(BUDGET_HEADING, theme.panel_title)),
        labelled(
            CONTINUATIONS_LABEL,
            format!(
                "{}/{}",
                stored.continuations, content.limits.max_continuations
            ),
        ),
        labelled(
            API_COST_LABEL,
            match content.limits.max_cost {
                Some(max) => format!("${:.2}/${max:.2}", stored.cost_usd),
                None => format!("${:.2} ({NO_LIMIT})", stored.cost_usd),
            },
        ),
        labelled(
            SUBSCRIPTION_LABEL,
            format!("${:.2} ({NOT_CAPPED})", stored.subscription_cost_usd),
        ),
        labelled(TIME_LABEL, {
            let minutes = stored.elapsed_ms / MS_PER_MINUTE;
            match content.limits.max_minutes {
                Some(max) => format!("{minutes}/{max} {MINUTES}"),
                None => format!("{minutes} {MINUTES} ({NO_LIMIT})"),
            }
        }),
        labelled(
            MODEL_LABEL,
            content
                .model
                .map_or_else(|| UNBOUND_MODEL.to_owned(), Binding::to_string),
        ),
    ];
    if escalated {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(WAITING_HEADING, theme.panel_title)));
        lines.extend(
            content
                .escalations
                .iter()
                .map(|line| item(line.clone(), theme.todo_in_progress)),
        );
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        DECISIONS_HEADING,
        theme.panel_title,
    )));
    if content.decisions.is_empty() {
        lines.push(Line::from(Span::styled(NO_DECISIONS, theme.status_dim)));
    }
    lines.extend(content.decisions.iter().rev().map(|decision| {
        item(
            decision_text(
                decision.kind.name(),
                decision.action.name(),
                &decision.subject,
                &decision.reason,
            ),
            Style::new(),
        )
    }));
    lines
}

fn labelled(label: &'static str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(label, theme::current().tool_dim),
        Span::raw(value),
    ])
}

fn item(text: String, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(ITEM_BULLET, theme::current().tool_dim),
        Span::styled(text, style),
    ])
}

#[cfg(test)]
mod tests {
    use caudra_agent::autopilot::DecisionKind;
    use caudra_storage::autopilot::AutopilotStopper;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use test_case::test_case;

    use super::*;

    const WIDTH: u16 = 120;
    const HEIGHT: u16 = 40;
    const GOAL: &str = "the suite is green";
    const SUBJECT: &str = "shell: cargo test";
    const REASON: &str = "the tests need running";
    const REASON_SENTENCE: &str = "the tests need running.";
    const ESCALATION: &str = "permission: the user must choose";
    const BOUND_MODEL: &str = "anthropic/claude-haiku-4-5";
    const CONTINUATIONS: u32 = 12;
    const MAX_CONTINUATIONS: u32 = 100;
    const COST: f64 = 0.5;
    const MAX_COST: f64 = 2.0;
    const ELAPSED_MS: u64 = 5 * MS_PER_MINUTE;
    const MAX_MINUTES: u32 = 30;
    const MISSING: &str = "the panel must show every part of the engagement";

    fn stored(engaged: bool, last_end: Option<AutopilotEnd>) -> StoredAutopilot {
        StoredAutopilot {
            engaged,
            owns_goal: true,
            engaged_at_ms: None,
            continuations: CONTINUATIONS,
            cost_usd: COST,
            subscription_cost_usd: 0.0,
            elapsed_ms: ELAPSED_MS,
            last_end,
        }
    }

    fn limits() -> AutopilotConfig {
        AutopilotConfig {
            max_continuations: MAX_CONTINUATIONS,
            max_cost: Some(MAX_COST),
            max_minutes: Some(MAX_MINUTES),
            ..AutopilotConfig::default()
        }
    }

    fn decision() -> AutopilotDecision {
        AutopilotDecision {
            seq: 0,
            at_ms: 0,
            kind: DecisionKind::Permission,
            action: ActionName::Allow,
            subject: SUBJECT.into(),
            reason: REASON.into(),
            agent: None,
            tool_use_id: None,
            history_len: 0,
            model: None,
            usage: None,
            cost: None,
        }
    }

    fn mouse(kind: MouseEventKind, at: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: at.x,
            row: at.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn drawn(panel: &mut AutopilotPanel) -> String {
        let stored = stored(true, None);
        let limits = limits();
        let model = Binding::Exact(BOUND_MODEL.into());
        let decisions = [decision()];
        let escalations = [ESCALATION.to_owned()];
        let content = PanelContent {
            stored: &stored,
            limits: &limits,
            goal: Some(GOAL),
            model: Some(&model),
            deciding: false,
            escalations: &escalations,
            decisions: &decisions,
        };
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                panel.view(frame, frame.area(), &content);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..HEIGHT)
            .map(|y| {
                (0..WIDTH)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_panel_shows_state_goal_budget_model_escalations_and_decisions() {
        let mut panel = AutopilotPanel::default();
        panel.open();

        let screen = drawn(&mut panel);

        for part in [
            WAITING_STATE.to_owned(),
            OWNED_GOAL_HEADING.to_owned(),
            GOAL.to_owned(),
            format!("{CONTINUATIONS}/{MAX_CONTINUATIONS}"),
            format!("${COST:.2}/${MAX_COST:.2}"),
            format!("{}/{MAX_MINUTES} {MINUTES}", ELAPSED_MS / MS_PER_MINUTE),
            BOUND_MODEL.to_owned(),
            ESCALATION.to_owned(),
            decision_text(
                DecisionKind::Permission.name(),
                ActionName::Allow.name(),
                SUBJECT,
                REASON,
            ),
        ] {
            assert!(screen.contains(&part), "{MISSING}: {part}\n{screen}");
        }
    }

    #[test_case(KeyCode::Char(STOP_KEY), Some(PanelAction::Stop), true ; "s_stops")]
    #[test_case(KeyCode::Char(MODEL_KEY), Some(PanelAction::Model), true ; "m_picks_the_model")]
    #[test_case(KeyCode::Esc, None, false ; "esc_closes")]
    fn a_key_asks_for_its_action(code: KeyCode, action: Option<PanelAction>, open: bool) {
        let mut panel = AutopilotPanel::default();
        panel.open();

        assert_eq!(panel.handle_key(KeyEvent::from(code)), action);
        assert_eq!(panel.is_open(), open);
    }

    #[test_case(PanelTarget::Stop, Some(PanelAction::Stop) ; "stop")]
    #[test_case(PanelTarget::Model, Some(PanelAction::Model) ; "model")]
    #[test_case(PanelTarget::Close, None ; "close")]
    fn a_footer_click_asks_for_its_action(target: PanelTarget, action: Option<PanelAction>) {
        let mut panel = AutopilotPanel::default();
        panel.open();
        drawn(&mut panel);
        let hit = panel.footer_hit(target);
        assert!(!hit.is_empty(), "{target:?}");

        panel.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), hit));

        assert_eq!(
            panel.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), hit)),
            action
        );
        assert_eq!(panel.is_open(), target != PanelTarget::Close);
    }

    #[test_case("permission", "allow", SUBJECT, format!("permission allow: {SUBJECT}{REASON_SEPARATOR}{REASON}") ; "a_decision")]
    #[test_case("second_opinion", "agree", "", format!("second opinion agree{REASON_SEPARATOR}{REASON}") ; "words_of_a_kind_and_no_subject")]
    #[test_case("question", "escalate", SUBJECT, format!("{LEFT_TO_YOU_LEAD} question {LEFT_TO_YOU_TAIL}: {SUBJECT}{REASON_SEPARATOR}{REASON}") ; "an_escalation")]
    fn a_decision_reads_as_what_was_decided_and_why(
        kind: &str,
        action: &str,
        subject: &str,
        expected: String,
    ) {
        assert_eq!(decision_text(kind, action, subject, REASON), expected);
    }

    #[test_case(stored(true, None), false, false, ENGAGED_STATE ; "engaged")]
    #[test_case(stored(true, None), true, false, DECIDING_STATE ; "deciding")]
    #[test_case(stored(true, None), true, true, WAITING_STATE ; "an_escalation_outranks_deciding")]
    fn the_state_names_what_autopilot_is_doing(
        stored: StoredAutopilot,
        deciding: bool,
        escalated: bool,
        expected: &str,
    ) {
        assert_eq!(state_label(&stored, deciding, escalated), expected);
    }

    #[test]
    fn an_ended_engagement_names_why() {
        let ended = stored(
            false,
            Some(AutopilotEnd::Stopped {
                by: AutopilotStopper::Command,
            }),
        );
        assert_eq!(
            state_label(&ended, false, false),
            format!("{ENDED_STATE}: {STOPPED}")
        );
    }

    #[test]
    fn the_budget_left_counts_down_every_limit() {
        assert_eq!(
            budget_left(&stored(true, None), &limits()),
            [
                format!(
                    "{} {CONTINUATIONS_WORD} {LEFT_SUFFIX}",
                    MAX_CONTINUATIONS - CONTINUATIONS
                ),
                format!("${:.2} {LEFT_SUFFIX}", MAX_COST - COST),
                format!(
                    "{} {MINUTES} {LEFT_SUFFIX}",
                    u64::from(MAX_MINUTES) - ELAPSED_MS / MS_PER_MINUTE
                ),
            ]
            .join(BUDGET_SEPARATOR)
        );
    }

    #[test_case(REASON ; "bare")]
    #[test_case(REASON_SENTENCE ; "already_a_sentence")]
    fn the_left_to_you_note_is_one_sentence(reason: &str) {
        let note = AutopilotNote::Escalated {
            reason: reason.into(),
        };
        assert_eq!(
            card_note(&note),
            format!("{LEFT_TO_YOU_NOTE}: {REASON}{SENTENCE_END}")
        );
    }
}
