//! The goal panel: a Status tab for the goal and the session's continuation
//! limit, and a Log tab listing every check the session's goal log keeps.

use caudra_agent::{GoalStatus, MAX_GOAL_CONTINUATION_LIMIT};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;

use super::log::GoalLog;
use super::status::{StatusContent, status_lines};
use crate::components::audit_log::{
    self, AuditPanel, CLOSE_GLOSS, COPY_GLOSS, COPY_LABEL, FILTER_GLOSS, FILTER_LABEL, JUMP_GLOSS,
    PanelTab, SECTION_GLOSS,
};
use crate::components::keybindings::key;
use crate::components::modal::ESC_LABEL;
use crate::components::{ascii_key, plain_char};
use crate::theme;

pub(crate) const GOAL_CLEAR: &str = "/goal clear";
pub(crate) const GOAL_MODEL: &str = "/goal model";
pub(crate) const LOWER_LABEL: &str = "-";
pub(crate) const RAISE_LABEL: &str = "+";
const LOWER_KEY: char = ascii_key(LOWER_LABEL);
const RAISE_KEY: char = ascii_key(RAISE_LABEL);
/// `+` unshifted, so the limit rises without Shift.
const RAISE_ALT_KEY: char = '=';
const CLEAR: FooterCommand = FooterCommand::Panel(PanelCommand::Run(GOAL_CLEAR));
const MODEL: FooterCommand = FooterCommand::Panel(PanelCommand::Run(GOAL_MODEL));
const ACTIVE_FOOTER: &[FooterEntry] = &[
    (key::TAB.label, SECTION_GLOSS, FooterCommand::Section),
    (GOAL_CLEAR, "", CLEAR),
    (GOAL_MODEL, "", MODEL),
    (ESC_LABEL, CLOSE_GLOSS, FooterCommand::Close),
];
/// A goal that is over cannot be cleared, so the footer does not offer it.
const FINISHED_FOOTER: &[FooterEntry] = &[
    (key::TAB.label, SECTION_GLOSS, FooterCommand::Section),
    (GOAL_MODEL, "", MODEL),
    (ESC_LABEL, CLOSE_GLOSS, FooterCommand::Close),
];
const LOG_FOOTER: &[FooterEntry] = &[
    (key::TAB.label, SECTION_GLOSS, FooterCommand::Section),
    (FILTER_LABEL, FILTER_GLOSS, FooterCommand::Filter),
    (COPY_LABEL, COPY_GLOSS, FooterCommand::Copy),
    (key::ENTER.label, JUMP_GLOSS, FooterCommand::Jump),
    (ESC_LABEL, CLOSE_GLOSS, FooterCommand::Close),
];

/// What the goal panel's own commands ask the host to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PanelCommand {
    /// A session command line, left to the host to run.
    Run(&'static str),
    /// Set the session's automatic continuation limit.
    Limit(u32),
}

pub(crate) type GoalPanel = AuditPanel<GoalLog, PanelCommand>;
pub(crate) type PanelAction = audit_log::PanelAction<PanelCommand>;
pub(crate) type FooterCommand = audit_log::FooterCommand<PanelCommand>;
type FooterEntry = audit_log::FooterEntry<PanelCommand>;

impl GoalPanel {
    /// On Status, ←/→ and -/+ move `continuation_limit` within its bounds;
    /// the rest as every panel does.
    pub(crate) fn handle_key(
        &mut self,
        key: KeyEvent,
        continuation_limit: u32,
    ) -> Option<PanelAction> {
        if self.tab() != PanelTab::Status {
            return self.answer_key(key);
        }
        let limit = match (key.code, plain_char(&key)) {
            (KeyCode::Left, _) | (_, Some(LOWER_KEY)) => continuation_limit.saturating_sub(1),
            (KeyCode::Right, _) | (_, Some(RAISE_KEY | RAISE_ALT_KEY)) => continuation_limit
                .saturating_add(1)
                .min(MAX_GOAL_CONTINUATION_LIMIT),
            _ => return self.answer_key(key),
        };
        Some(PanelAction::Command(PanelCommand::Limit(limit)))
    }

    pub(crate) fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        content: &StatusContent<'_>,
    ) -> Rect {
        let footer = match (self.tab(), content.status) {
            (PanelTab::Log, _) => LOG_FOOTER,
            (PanelTab::Status, Some(GoalStatus::Active(_))) => ACTIVE_FOOTER,
            (PanelTab::Status, Some(GoalStatus::Finished(_)) | None) => FINISHED_FOOTER,
        };
        let status = status_lines(content, &theme::current());
        self.render(frame, area, status, footer)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use caudra_agent::{GoalResult, GoalVerdict};
    use caudra_config::ClockFormat;
    use caudra_providers::TokenUsage;
    use caudra_storage::goal_log::{GoalEvaluationOutcome, GoalLogWrite};
    use caudra_workbench::scroll::SCROLLBAR_THUMB;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;
    use test_case::test_case;

    use super::super::log::fixtures::{CONDITION, REASON, evaluation, goal};
    use super::super::log::{LogFilter, NO_GOAL_LOG};
    use super::*;
    use crate::components::audit_log::{COPY_KEY, DOWN_KEY, FILTER_KEY, NOTHING_TO_COPY, Pane};
    use crate::components::{Overlay, buffer_text, key};
    use crate::repaint::Cadence;

    const WIDTH: u16 = 120;
    const NARROW: u16 = 60;
    const HEIGHT: u16 = 30;
    /// Short enough that the status outgrows the panel and earns a bar.
    const CRAMPED_HEIGHT: u16 = 12;
    const LIMIT: u32 = 16;
    const FIRST: u32 = 1;
    const OLDER: u64 = 1;
    const NEWER: u64 = 2;
    const LANDED: u64 = 3;
    const BAR_SCROLL_MISSED: &str = "a press on the bar's column must scroll the body";
    const ONE_PANE: &str = "a body too narrow for both panes shows one";

    fn finished() -> GoalStatus {
        GoalStatus::Finished(GoalResult {
            goal: FIRST,
            set_at_ms: 0,
            condition: CONDITION.into(),
            verdict: GoalVerdict::Met,
            reason: REASON.into(),
            evaluations: 3,
            duration: Duration::from_secs(12),
            usage: TokenUsage::default(),
            cost: None,
            subscription_cost: None,
        })
    }

    fn log() -> GoalLog {
        GoalLog::new(
            vec![goal(FIRST, None)],
            vec![
                evaluation(OLDER, FIRST, GoalEvaluationOutcome::Applied),
                evaluation(NEWER, FIRST, GoalEvaluationOutcome::Applied),
            ],
        )
    }

    fn opened(tab: PanelTab, unwritten: Vec<GoalLogWrite>) -> GoalPanel {
        let mut panel = GoalPanel::default();
        panel.open(
            tab,
            smol::unblock(|| Ok(log())),
            unwritten,
            ClockFormat::default(),
        );
        panel
    }

    fn loaded() -> GoalPanel {
        let mut panel = opened(PanelTab::Log, Vec::new());
        panel.finish_loading();
        panel
    }

    fn draw(panel: &mut GoalPanel, status: Option<&GoalStatus>, size: (u16, u16)) -> String {
        let content = StatusContent {
            status,
            evaluator: None,
            continuation_limit: LIMIT,
        };
        let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
        terminal
            .draw(|frame| {
                panel.view(frame, frame.area(), &content);
            })
            .unwrap();
        buffer_text(terminal.backend().buffer())
    }

    fn mouse(kind: MouseEventKind, at: Position) -> MouseEvent {
        MouseEvent {
            kind,
            column: at.x,
            row: at.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn click(panel: &mut GoalPanel, hit: Rect) -> [Option<PanelAction>; 2] {
        [MouseEventKind::Down, MouseEventKind::Up]
            .map(|kind| panel.handle_mouse(mouse(kind(MouseButton::Left), hit.as_position())))
    }

    fn selected(panel: &GoalPanel) -> Option<u64> {
        panel.selected_entry().map(|evaluation| evaluation.seq)
    }

    #[test_case(KeyCode::Right, LIMIT, LIMIT + 1 ; "right_raises")]
    #[test_case(KeyCode::Char(RAISE_KEY), LIMIT, LIMIT + 1 ; "plus_raises")]
    #[test_case(KeyCode::Char(RAISE_ALT_KEY), LIMIT, LIMIT + 1 ; "equals_raises")]
    #[test_case(KeyCode::Left, 0, 0 ; "left_stops_at_zero")]
    #[test_case(KeyCode::Char(LOWER_KEY), LIMIT, LIMIT - 1 ; "minus_lowers")]
    #[test_case(KeyCode::Right, MAX_GOAL_CONTINUATION_LIMIT, MAX_GOAL_CONTINUATION_LIMIT ; "right_stops_at_the_cap")]
    fn status_keys_adjust_the_limit_within_bounds(code: KeyCode, from: u32, to: u32) {
        let mut panel = opened(PanelTab::Status, Vec::new());

        assert_eq!(
            panel.handle_key(key(code), from),
            Some(PanelAction::Command(PanelCommand::Limit(to)))
        );
    }

    #[test]
    fn on_the_log_left_and_right_move_between_panes() {
        let mut panel = loaded();

        assert_eq!(panel.handle_key(key(KeyCode::Right), LIMIT), None);
        assert_eq!(panel.pane(), Pane::Detail);
        assert_eq!(panel.handle_key(key(KeyCode::Left), LIMIT), None);
        assert_eq!(panel.pane(), Pane::List);
    }

    #[test_case(Some(finished()), GOAL_MODEL, false ; "a_finished_goal_offers_the_model")]
    #[test_case(None, GOAL_CLEAR, true ; "no_goal_offers_no_clear")]
    #[test_case(Some(finished()), GOAL_CLEAR, true ; "a_finished_goal_offers_no_clear")]
    fn the_status_footer_offers_what_applies(
        status: Option<GoalStatus>,
        command: &'static str,
        missing: bool,
    ) {
        let mut panel = opened(PanelTab::Status, Vec::new());
        draw(&mut panel, status.as_ref(), (WIDTH, HEIGHT));

        let hit = panel.footer_hit(FooterCommand::Panel(PanelCommand::Run(command)));

        assert_eq!(hit.is_empty(), missing);
        if !missing {
            assert_eq!(
                click(&mut panel, hit),
                [None, Some(PanelAction::Command(PanelCommand::Run(command)))]
            );
        }
    }

    #[test_case(FooterCommand::Filter, PanelTab::Log, LogFilter::Verdicts ; "filter")]
    #[test_case(FooterCommand::Section, PanelTab::Status, LogFilter::All ; "section")]
    fn a_log_footer_click_runs_its_command(
        command: FooterCommand,
        tab: PanelTab,
        filter: LogFilter,
    ) {
        let mut panel = loaded();
        draw(&mut panel, None, (WIDTH, HEIGHT));
        let hit = panel.footer_hit(command);

        assert_eq!(click(&mut panel, hit), [None, None]);
        assert_eq!((panel.tab(), panel.filter()), (tab, filter));
    }

    #[test]
    fn the_close_control_closes() {
        let mut panel = opened(PanelTab::Status, Vec::new());
        draw(&mut panel, Some(&finished()), (WIDTH, HEIGHT));

        let hit = panel.footer_hit(FooterCommand::Close);

        assert_eq!(click(&mut panel, hit), [None, None]);
        assert!(!panel.is_open());
    }

    #[test]
    fn a_press_on_the_bar_scrolls_the_status() {
        let mut panel = opened(PanelTab::Status, Vec::new());
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, CRAMPED_HEIGHT)).unwrap();
        let content = StatusContent {
            status: Some(&finished()),
            evaluator: None,
            continuation_limit: LIMIT,
        };
        let mut popup = Rect::default();
        terminal
            .draw(|frame| popup = panel.view(frame, frame.area(), &content))
            .unwrap();
        let thumb = (0..CRAMPED_HEIGHT)
            .flat_map(|y| (0..WIDTH).map(move |x| Position::new(x, y)))
            .find(|at| terminal.backend().buffer()[(at.x, at.y)].symbol() == SCROLLBAR_THUMB)
            .expect(BAR_SCROLL_MISSED);

        panel.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            Position::new(thumb.x, popup.bottom() - 3),
        ));

        assert!(panel.body_offset() > 0, "{BAR_SCROLL_MISSED}");
    }

    #[test]
    fn the_log_lists_newest_first_and_steps_down() {
        let mut panel = loaded();
        draw(&mut panel, None, (WIDTH, HEIGHT));
        assert_eq!(selected(&panel), Some(NEWER));

        panel.handle_key(key(KeyCode::Char(DOWN_KEY)), LIMIT);

        assert_eq!(selected(&panel), Some(OLDER));
    }

    #[test]
    fn a_narrow_panel_opens_the_detail_before_it_jumps() {
        let mut panel = loaded();
        draw(&mut panel, None, (NARROW, HEIGHT));
        assert_eq!(panel.body_area().width, 0, "{ONE_PANE}");

        assert_eq!(panel.handle_key(key(KeyCode::Enter), LIMIT), None);
        draw(&mut panel, None, (NARROW, HEIGHT));
        assert_eq!(panel.pane(), Pane::Detail);
        assert_eq!(
            panel.handle_key(key(KeyCode::Enter), LIMIT),
            Some(PanelAction::Jump(NEWER))
        );
    }

    #[test]
    fn copy_takes_the_selected_check_as_text() {
        let mut panel = loaded();

        let Some(PanelAction::Copy(text)) = panel.handle_key(key(KeyCode::Char(COPY_KEY)), LIMIT)
        else {
            panic!("a selected check is copied");
        };

        assert!(text.contains(CONDITION) && text.contains(REASON), "{text}");
    }

    #[test]
    fn copy_with_nothing_loaded_says_so() {
        let mut panel = opened(PanelTab::Log, Vec::new());

        assert_eq!(
            panel.handle_key(key(KeyCode::Char(COPY_KEY)), LIMIT),
            Some(PanelAction::Flash(NOTHING_TO_COPY))
        );
    }

    #[test]
    fn checks_made_while_open_land_in_the_log() {
        let landed = GoalLogWrite::Evaluated(Box::new(evaluation(
            LANDED,
            FIRST,
            GoalEvaluationOutcome::Stale,
        )));
        let mut panel = opened(PanelTab::Log, Vec::new());
        panel.note(&landed);
        panel.finish_loading();

        assert_eq!(panel.listed(), [LANDED, NEWER, OLDER]);
        panel.handle_key(key(KeyCode::Char(FILTER_KEY)), LIMIT);
        assert_eq!(panel.listed(), [NEWER, OLDER]);
    }

    #[test]
    fn an_empty_log_says_so() {
        let mut panel = GoalPanel::default();
        panel.open(
            PanelTab::Log,
            smol::unblock(|| Ok(GoalLog::default())),
            Vec::new(),
            ClockFormat::default(),
        );
        panel.finish_loading();

        assert!(draw(&mut panel, None, (WIDTH, HEIGHT)).contains(NO_GOAL_LOG));
    }

    #[test]
    fn the_loading_panel_keeps_the_loop_ticking() {
        let mut panel = opened(PanelTab::Status, Vec::new());
        assert_ne!(panel.cadence(), Cadence::IDLE);

        panel.finish_loading();

        assert_eq!(panel.cadence(), Cadence::IDLE);
    }
}
