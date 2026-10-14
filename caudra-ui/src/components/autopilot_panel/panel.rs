//! The autopilot panel: a Status tab for the engagement, and a Log tab
//! listing every decision the session's log keeps.

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

use super::log::AutopilotLog;
use super::status::{PanelContent, status_lines};
use crate::components::audit_log::{
    self, AuditPanel, CLOSE_GLOSS, COPY_GLOSS, COPY_LABEL, FILTER_GLOSS, FILTER_LABEL, JUMP_GLOSS,
    PanelTab, SECTION_GLOSS,
};
use crate::components::keybindings::key;
use crate::components::modal::ESC_LABEL;
use crate::components::{ascii_key, plain_char};
use crate::theme;

pub(crate) const STOP_LABEL: &str = "s";
pub(crate) const MODEL_LABEL_KEY: &str = "m";
pub(crate) const STOP_KEY: char = ascii_key(STOP_LABEL);
pub(crate) const MODEL_KEY: char = ascii_key(MODEL_LABEL_KEY);
const STOP_GLOSS: &str = "Stop";
const MODEL_GLOSS: &str = "Model";
const STOP: FooterCommand = FooterCommand::Panel(PanelCommand::Stop);
const MODEL: FooterCommand = FooterCommand::Panel(PanelCommand::Model);
const STATUS_FOOTER: &[FooterEntry] = &[
    (key::TAB.label, SECTION_GLOSS, FooterCommand::Section),
    (STOP_LABEL, STOP_GLOSS, STOP),
    (MODEL_LABEL_KEY, MODEL_GLOSS, MODEL),
    (ESC_LABEL, CLOSE_GLOSS, FooterCommand::Close),
];
const LOG_FOOTER: &[FooterEntry] = &[
    (key::TAB.label, SECTION_GLOSS, FooterCommand::Section),
    (FILTER_LABEL, FILTER_GLOSS, FooterCommand::Filter),
    (COPY_LABEL, COPY_GLOSS, FooterCommand::Copy),
    (key::ENTER.label, JUMP_GLOSS, FooterCommand::Jump),
    (STOP_LABEL, STOP_GLOSS, STOP),
    (MODEL_LABEL_KEY, MODEL_GLOSS, MODEL),
    (ESC_LABEL, CLOSE_GLOSS, FooterCommand::Close),
];

/// What the autopilot panel's own commands ask the host to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PanelCommand {
    Stop,
    Model,
}

pub(crate) type AutopilotPanel = AuditPanel<AutopilotLog, PanelCommand>;
pub(crate) type PanelAction = audit_log::PanelAction<PanelCommand>;
pub(crate) type FooterCommand = audit_log::FooterCommand<PanelCommand>;
type FooterEntry = audit_log::FooterEntry<PanelCommand>;

impl AutopilotPanel {
    /// Stop and Model answer on either tab; the rest as every panel does.
    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Option<PanelAction> {
        match plain_char(&key) {
            Some(STOP_KEY) => Some(PanelAction::Command(PanelCommand::Stop)),
            Some(MODEL_KEY) => Some(PanelAction::Command(PanelCommand::Model)),
            _ => self.answer_key(key),
        }
    }

    pub(crate) fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        content: &PanelContent<'_>,
    ) -> Rect {
        let footer = match self.tab() {
            PanelTab::Status => STATUS_FOOTER,
            PanelTab::Log => LOG_FOOTER,
        };
        let status = status_lines(content, &theme::current());
        self.render(frame, area, status, footer)
    }
}

#[cfg(test)]
mod tests {
    use caudra_agent::autopilot::ActionName;
    use caudra_config::AutopilotConfig;
    use caudra_config::ClockFormat;
    use caudra_storage::autopilot::{
        DecisionOutcome, StoredAutopilotDecision, StoredAutopilotEngagement,
    };
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use test_case::test_case;

    use super::super::log::{LogFilter, REASON_HEADING};
    use super::super::text::fixtures::{REASON, SUBJECT, decision, engagement};
    use super::*;
    use crate::components::audit_log::{
        COPY_KEY, DOWN_KEY, FILTER_KEY, NOTHING_MATCHES, NOTHING_TO_COPY, Pane, UP_KEY,
    };
    use crate::components::{Overlay, buffer_text, key};
    use crate::repaint::Cadence;
    use crate::storage_writer::AutopilotWrite;

    const WIDE: u16 = 160;
    const NARROW: u16 = 60;
    const HEIGHT: u16 = 30;
    const FIRST: u32 = 1;
    const SECOND: u32 = 2;
    const OLDER: u64 = 0;
    const NEWER: u64 = 1;
    const LANDED: u64 = 2;
    const LANDED_LATER: u64 = 3;
    const HEADERS_SKIPPED: &str = "the cursor moves between entries, never onto a header";
    const ONE_PANE: &str = "a body too narrow for both panes shows one";

    fn logged(seq: u64, engagement: u32) -> StoredAutopilotDecision {
        StoredAutopilotDecision {
            seq,
            engagement: Some(engagement),
            ..decision().into()
        }
    }

    fn engaged(number: u32) -> StoredAutopilotEngagement {
        StoredAutopilotEngagement {
            engagement: number,
            ..engagement(0, 0.0, 0.0, 0, 0)
        }
    }

    /// Two engagements of one decision each, so a header sits between them.
    fn log() -> AutopilotLog {
        AutopilotLog::new(
            vec![logged(OLDER, FIRST), logged(NEWER, SECOND)],
            vec![engaged(FIRST), engaged(SECOND)],
        )
    }

    fn opened(tab: PanelTab, unwritten: Vec<AutopilotWrite>) -> AutopilotPanel {
        let mut panel = AutopilotPanel::default();
        panel.open(
            tab,
            smol::unblock(|| Ok(log())),
            unwritten,
            ClockFormat::default(),
        );
        panel
    }

    fn loaded() -> AutopilotPanel {
        let mut panel = opened(PanelTab::Log, Vec::new());
        panel.finish_loading();
        panel
    }

    fn draw(panel: &mut AutopilotPanel, width: u16) -> String {
        let limits = AutopilotConfig::default();
        let content = PanelContent {
            stored: None,
            limits: &limits,
            goal: None,
            model: None,
            deciding: false,
            escalations: &[],
        };
        let mut terminal = Terminal::new(TestBackend::new(width, HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                panel.view(frame, frame.area(), &content);
            })
            .unwrap();
        buffer_text(terminal.backend().buffer())
    }

    fn press(panel: &mut AutopilotPanel, code: KeyCode) -> Option<PanelAction> {
        panel.handle_key(key(code))
    }

    fn selected(panel: &AutopilotPanel) -> Option<u64> {
        panel.selected_entry().map(|decision| decision.seq)
    }

    fn written(seq: u64) -> AutopilotWrite {
        AutopilotWrite::Decision(Box::new(logged(seq, SECOND)))
    }

    #[test_case(&[], NEWER ; "the_newest_entry_first")]
    #[test_case(&[KeyCode::Char(DOWN_KEY)], OLDER ; "j_steps_over_the_header")]
    #[test_case(&[KeyCode::Down, KeyCode::Down], OLDER ; "down_stops_at_the_last")]
    #[test_case(&[KeyCode::Down, KeyCode::Char(UP_KEY)], NEWER ; "k_steps_back")]
    #[test_case(&[KeyCode::End], OLDER ; "end")]
    #[test_case(&[KeyCode::End, KeyCode::Home], NEWER ; "home")]
    fn the_cursor_moves_between_entries(keys: &[KeyCode], expected: u64) {
        let mut panel = loaded();
        draw(&mut panel, WIDE);

        for code in keys {
            press(&mut panel, *code);
        }

        assert_eq!(selected(&panel), Some(expected), "{HEADERS_SKIPPED}");
    }

    #[test]
    fn tab_and_digits_switch_tabs() {
        let mut panel = opened(PanelTab::Status, Vec::new());

        press(&mut panel, KeyCode::Tab);
        assert_eq!(panel.tab(), PanelTab::Log);
        press(&mut panel, KeyCode::Tab);
        assert_eq!(panel.tab(), PanelTab::Status);
        press(&mut panel, KeyCode::Char('2'));
        assert_eq!(panel.tab(), PanelTab::Log);
        press(&mut panel, KeyCode::BackTab);
        assert_eq!(panel.tab(), PanelTab::Status);
    }

    #[test]
    fn a_narrow_panel_shows_the_list_then_the_detail() {
        let mut panel = loaded();
        draw(&mut panel, NARROW);
        assert!(panel.list_area().width > 0, "{ONE_PANE}");
        assert_eq!(panel.body_area().width, 0, "{ONE_PANE}");

        assert_eq!(press(&mut panel, KeyCode::Enter), None);
        let shown = draw(&mut panel, NARROW);
        assert_eq!(panel.pane(), Pane::Detail);
        assert_eq!(panel.list_area().width, 0, "{ONE_PANE}");
        assert!(shown.contains(REASON_HEADING), "{shown}");

        press(&mut panel, KeyCode::Esc);
        assert_eq!(panel.pane(), Pane::List);
        assert!(panel.is_open());
        press(&mut panel, KeyCode::Esc);
        assert!(!panel.is_open());
    }

    #[test]
    fn enter_jumps_when_both_panes_show() {
        let mut panel = loaded();
        let shown = draw(&mut panel, WIDE);

        assert!(panel.list_area().width > 0 && panel.body_area().width > 0);
        assert!(shown.contains(REASON_HEADING), "{shown}");
        assert_eq!(
            press(&mut panel, KeyCode::Enter),
            Some(PanelAction::Jump(NEWER))
        );
    }

    #[test]
    fn copy_takes_the_selected_entry_as_text() {
        let mut panel = loaded();

        let Some(PanelAction::Copy(text)) = press(&mut panel, KeyCode::Char(COPY_KEY)) else {
            panic!("a selected entry is copied");
        };

        assert!(text.contains(SUBJECT) && text.contains(REASON), "{text}");
    }

    #[test]
    fn copy_with_nothing_loaded_says_so() {
        let mut panel = opened(PanelTab::Log, Vec::new());

        assert_eq!(
            press(&mut panel, KeyCode::Char(COPY_KEY)),
            Some(PanelAction::Flash(NOTHING_TO_COPY))
        );
    }

    #[test]
    fn writes_before_during_and_after_the_read_all_land() {
        let mut panel = opened(PanelTab::Log, vec![written(LANDED)]);
        panel.note(&written(LANDED_LATER));
        panel.finish_loading();
        panel.note(&AutopilotWrite::Settle {
            seq: LANDED_LATER,
            outcome: DecisionOutcome::TakenOver,
        });

        assert_eq!(panel.listed(), [LANDED_LATER, LANDED, NEWER, OLDER]);
        assert_eq!(
            panel
                .selected_entry()
                .and_then(|decision| decision.outcome.clone()),
            Some(DecisionOutcome::TakenOver)
        );
    }

    #[test]
    fn a_row_landing_above_keeps_the_chosen_entry_selected() {
        let mut panel = loaded();
        press(&mut panel, KeyCode::Down);

        panel.note(&written(LANDED));

        assert_eq!(selected(&panel), Some(OLDER));
    }

    #[test]
    fn a_filter_with_nothing_to_list_says_so() {
        let mut panel = loaded();

        press(&mut panel, KeyCode::Char(FILTER_KEY));
        let shown = draw(&mut panel, WIDE);

        assert_eq!(panel.filter(), LogFilter::LeftToYou);
        assert!(shown.contains(NOTHING_MATCHES), "{shown}");
        assert_eq!(selected(&panel), None);
    }

    #[test]
    fn an_escalation_is_listed_under_left_to_you() {
        let escalated = StoredAutopilotDecision {
            action: Some(ActionName::Escalate.name().to_owned()),
            ..logged(LANDED, SECOND)
        };
        let mut panel = loaded();
        panel.note(&AutopilotWrite::Decision(Box::new(escalated)));

        press(&mut panel, KeyCode::Char(FILTER_KEY));

        assert_eq!(panel.listed(), [LANDED]);
    }

    #[test_case(FooterCommand::Filter, PanelTab::Log, LogFilter::LeftToYou ; "filter")]
    #[test_case(FooterCommand::Section, PanelTab::Status, LogFilter::All ; "section")]
    fn a_footer_click_runs_its_command(command: FooterCommand, tab: PanelTab, filter: LogFilter) {
        let mut panel = loaded();
        draw(&mut panel, WIDE);
        let hit = panel.footer_hit(command);

        let actions = [MouseEventKind::Down, MouseEventKind::Up].map(|kind| {
            panel.handle_mouse(MouseEvent {
                kind: kind(MouseButton::Left),
                column: hit.x,
                row: hit.y,
                modifiers: KeyModifiers::NONE,
            })
        });

        assert_eq!(actions, [None, None]);
        assert_eq!((panel.tab(), panel.filter()), (tab, filter));
    }

    #[test]
    fn the_loading_panel_keeps_the_loop_ticking() {
        let mut panel = opened(PanelTab::Status, Vec::new());
        assert_ne!(panel.cadence(), Cadence::IDLE);

        panel.finish_loading();

        assert_eq!(panel.cadence(), Cadence::IDLE);
    }
}
