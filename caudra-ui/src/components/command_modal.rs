//! The centered command palette, opened by a keybinding rather than by
//! typing `/`. Shares its command list with the inline dropdown in
//! [`crate::components::command`]; only the surface differs.

use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use crate::components::command::{CommandRow, ParsedCommand};
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction};
use crate::components::modal::Modal;
use crate::components::{CHEVRON, Overlay, hint_line, input_line_with_cursor, visual_line_count};
use crate::repaint::Cadence;
use crate::text_buffer::TextBuffer;
use crate::theme;
use unicode_width::UnicodeWidthStr;

const TITLE: &str = " Commands ";
const MAX_VISIBLE: u16 = 15;
const ARGS_WIDTH_PERCENT: u16 = 65;
const PICK_WIDTH_PERCENT: u16 = 80;
const FOOTER_ROWS: u16 = 1;
const ARGS_MAX_HEIGHT_PERCENT: u16 = 40;

pub enum CommandModalAction {
    Consumed,
    Closed,
    Execute(ParsedCommand),
}

enum Stage {
    Closed,
    Pick(Box<ListPicker<CommandRow>>),
    Args { input: TextBuffer, row: CommandRow },
}

/// Describes a stage change without touching `self`, so `handle_key` can
/// return it after the `&mut self.stage` borrow ends.
enum StageAction {
    None,
    Close,
    Back,
    AskArgs { row: Box<CommandRow>, query: String },
    Run(ParsedCommand),
}

pub struct CommandModal {
    stage: Stage,
    /// `PickerAction::Select` consumes the picker, so stage one is rebuilt
    /// from here when the argument prompt is dismissed.
    rows: Vec<CommandRow>,
    query: String,
}

impl CommandModal {
    pub fn new() -> Self {
        Self {
            stage: Stage::Closed,
            rows: Vec::new(),
            query: String::new(),
        }
    }

    pub fn open(&mut self, rows: Vec<CommandRow>) {
        self.rows = rows;
        self.query.clear();
        self.stage = Stage::Pick(self.build_picker());
    }

    fn build_picker(&self) -> Box<ListPicker<CommandRow>> {
        let mut picker = ListPicker::new()
            .with_max_visible(MAX_VISIBLE)
            .with_width_percent(PICK_WIDTH_PERCENT)
            .with_footer_builder(footer);
        picker.open(self.rows.clone(), TITLE);
        Box::new(picker)
    }

    pub fn is_open(&self) -> bool {
        !matches!(self.stage, Stage::Closed)
    }

    pub fn close(&mut self) {
        self.stage = Stage::Closed;
        self.rows.clear();
        self.query.clear();
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> CommandModalAction {
        let action = match &mut self.stage {
            Stage::Closed => return CommandModalAction::Consumed,
            Stage::Pick(picker) => {
                let query = picker.search_text();
                Self::map_pick_action(picker.handle_key(key_event), query)
            }
            Stage::Args { input, row } => match key_event.code {
                KeyCode::Enter => StageAction::Run(ParsedCommand {
                    name: row.name.clone(),
                    args: input.value().trim().to_string(),
                }),
                KeyCode::Esc => StageAction::Back,
                _ if key::QUIT.matches(key_event) => StageAction::Close,
                _ => {
                    input.handle_key(key_event);
                    return CommandModalAction::Consumed;
                }
            },
        };

        self.transition(action)
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> CommandModalAction {
        let action = match &mut self.stage {
            Stage::Pick(picker) => {
                let query = picker.search_text();
                Self::map_pick_action(picker.handle_mouse(event), query)
            }
            Stage::Closed | Stage::Args { .. } => return CommandModalAction::Consumed,
        };

        self.transition(action)
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        match &mut self.stage {
            Stage::Closed => false,
            Stage::Pick(picker) => picker.handle_paste(text),
            Stage::Args { input, .. } => {
                input.insert_text(text);
                true
            }
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        if let Stage::Pick(picker) = &mut self.stage {
            picker.scroll(delta);
        }
    }

    /// Only the list scrolls; the argument prompt is two fixed rows.
    pub fn contains(&self, pos: Position) -> bool {
        match &self.stage {
            Stage::Pick(picker) => picker.contains(pos),
            Stage::Closed | Stage::Args { .. } => false,
        }
    }

    /// A command that takes no arguments runs straight away; anything with
    /// an argument slot gets the prompt, where an empty answer is still
    /// valid so `/model` keeps opening its own picker. `query` is captured
    /// before the picker sees the event, because selecting consumes it.
    fn map_pick_action(action: PickerAction<CommandRow>, query: String) -> StageAction {
        match action {
            PickerAction::Select(row) if row.takes_args() => StageAction::AskArgs {
                row: Box::new(row),
                query,
            },
            PickerAction::Select(row) => StageAction::Run(ParsedCommand {
                name: row.name,
                args: String::new(),
            }),
            PickerAction::Close => StageAction::Close,
            PickerAction::Consumed | PickerAction::Toggle(..) => StageAction::None,
        }
    }

    fn transition(&mut self, action: StageAction) -> CommandModalAction {
        match action {
            StageAction::None => CommandModalAction::Consumed,
            StageAction::AskArgs { row, query } => {
                self.query = query;
                self.stage = Stage::Args {
                    input: TextBuffer::new(String::new()),
                    row: *row,
                };
                CommandModalAction::Consumed
            }
            StageAction::Back => {
                let mut picker = self.build_picker();
                picker.set_search_text(&self.query);
                self.stage = Stage::Pick(picker);
                CommandModalAction::Consumed
            }
            StageAction::Run(cmd) => {
                self.close();
                CommandModalAction::Execute(cmd)
            }
            StageAction::Close => {
                self.close();
                CommandModalAction::Closed
            }
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        match &mut self.stage {
            Stage::Closed => Rect::default(),
            Stage::Pick(picker) => picker.view(frame, area),
            Stage::Args { input, row } => {
                let t = theme::current();
                let title = format!(" {} ", row.name);
                let modal = Modal {
                    title: &title,
                    width_percent: ARGS_WIDTH_PERCENT,
                    max_height_percent: ARGS_MAX_HEIGHT_PERCENT,
                };
                // Both rows wrap, so the modal is sized from their wrapped
                // heights; a fixed guess clips the input line out of view.
                let width = modal_inner_width(area, ARGS_WIDTH_PERCENT);
                let hint_rows = wrapped_rows(&row.description, width);
                let input_rows =
                    wrapped_rows(&input.value(), width - CHEVRON.width() as u16).max(1);
                let (popup, inner) =
                    modal.render(frame, area, hint_rows + input_rows + FOOTER_ROWS);

                let [hint_area, input_area, footer_area] = Layout::vertical([
                    Constraint::Length(hint_rows),
                    Constraint::Min(1),
                    Constraint::Length(FOOTER_ROWS),
                ])
                .areas(inner);
                let bg = Style::new().bg(t.background);
                frame.render_widget(
                    Paragraph::new(row.description.as_str())
                        .style(bg.patch(t.input_placeholder))
                        .wrap(Wrap { trim: true }),
                    hint_area,
                );
                frame.render_widget(
                    Paragraph::new(input_line_with_cursor(input))
                        .style(bg)
                        .wrap(Wrap { trim: false }),
                    input_area,
                );
                frame.render_widget(Paragraph::new(args_footer()).style(bg), footer_area);
                popup
            }
        }
    }
}

impl Overlay for CommandModal {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }

    fn cadence(&self) -> Cadence {
        match &self.stage {
            Stage::Pick(picker) => picker.cadence(),
            Stage::Closed | Stage::Args { .. } => Cadence::IDLE,
        }
    }
}

/// Columns inside the border, floored above the chevron so the input row
/// always has somewhere to wrap into.
fn modal_inner_width(area: Rect, width_percent: u16) -> u16 {
    ((area.width as u32 * width_percent as u32 / 100).saturating_sub(2) as u16)
        .max(CHEVRON.width() as u16 + 1)
}

/// Zero for empty text, so a command with no description spends no row on it.
fn wrapped_rows(text: &str, width: u16) -> u16 {
    if text.is_empty() {
        return 0;
    }
    visual_line_count(text.width(), width.max(1) as usize) as u16
}

fn footer() -> Line<'static> {
    hint_line(&[("Enter", "Run"), ("Esc", "Close")])
}

fn args_footer() -> Line<'static> {
    hint_line(&[("Enter", "Run"), ("Esc", "Back")])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::command::SECTION_BUILTIN;
    use crate::components::key;
    use crossterm::event::KeyModifiers;
    use test_case::test_case;

    const ZERO_ARG: &str = "/help";
    const WITH_ARGS: &str = "/btw";
    const WITH_ARGS_DESC: &str = "Ask a side question";
    const TEST_WIDTH: u16 = 80;
    const TEST_HEIGHT: u16 = 24;

    fn row(name: &str, description: &str, max_args: usize) -> CommandRow {
        CommandRow {
            name: name.to_string(),
            description: description.to_string(),
            max_args,
            section: SECTION_BUILTIN,
        }
    }

    fn opened() -> CommandModal {
        let mut modal = CommandModal::new();
        modal.open(vec![
            row(ZERO_ARG, "Show keybindings", 0),
            row(WITH_ARGS, WITH_ARGS_DESC, usize::MAX),
        ]);
        modal
    }

    fn type_text(modal: &mut CommandModal, text: &str) {
        for c in text.chars() {
            modal.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn render(modal: &mut CommandModal) -> (Rect, String) {
        let backend = ratatui::backend::TestBackend::new(TEST_WIDTH, TEST_HEIGHT);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut popup = Rect::default();
        terminal
            .draw(|frame| popup = modal.view(frame, frame.area()))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        (popup, text)
    }

    fn stage_name(modal: &CommandModal) -> &'static str {
        match modal.stage {
            Stage::Closed => "closed",
            Stage::Pick(_) => "pick",
            Stage::Args { .. } => "args",
        }
    }

    #[test]
    fn zero_arg_command_runs_immediately() {
        let mut modal = opened();
        type_text(&mut modal, "help");
        let action = modal.handle_key(key(KeyCode::Enter));
        let CommandModalAction::Execute(cmd) = action else {
            panic!("expected execute, stage is {}", stage_name(&modal));
        };
        assert_eq!(cmd.name, ZERO_ARG);
        assert_eq!(cmd.args, "");
        assert!(!modal.is_open());
    }

    #[test]
    fn command_with_args_opens_prompt_without_running() {
        let mut modal = opened();
        type_text(&mut modal, "btw");
        let action = modal.handle_key(key(KeyCode::Enter));
        assert!(matches!(action, CommandModalAction::Consumed));
        assert_eq!(stage_name(&modal), "args");
    }

    #[test]
    fn prompt_submits_typed_arguments() {
        let mut modal = opened();
        type_text(&mut modal, "btw");
        modal.handle_key(key(KeyCode::Enter));
        type_text(&mut modal, "why");
        let action = modal.handle_key(key(KeyCode::Enter));
        let CommandModalAction::Execute(cmd) = action else {
            panic!("expected execute, stage is {}", stage_name(&modal));
        };
        assert_eq!(cmd.name, WITH_ARGS);
        assert_eq!(cmd.args, "why");
    }

    #[test]
    fn empty_prompt_runs_with_no_arguments() {
        let mut modal = opened();
        type_text(&mut modal, "btw");
        modal.handle_key(key(KeyCode::Enter));
        let action = modal.handle_key(key(KeyCode::Enter));
        let CommandModalAction::Execute(cmd) = action else {
            panic!("expected execute, stage is {}", stage_name(&modal));
        };
        assert_eq!(cmd.args, "");
    }

    #[test]
    fn escaping_the_prompt_restores_the_search() {
        let mut modal = opened();
        type_text(&mut modal, "btw");
        modal.handle_key(key(KeyCode::Enter));
        modal.handle_key(key(KeyCode::Esc));
        assert_eq!(stage_name(&modal), "pick");
        let Stage::Pick(picker) = &modal.stage else {
            unreachable!()
        };
        assert_eq!(picker.search_text(), "btw");
        assert_eq!(
            picker.selected_item().map(|r| r.name.as_str()),
            Some(WITH_ARGS)
        );
    }

    #[test_case(key(KeyCode::Esc) ; "escape")]
    #[test_case(key::QUIT.to_key_event() ; "ctrl_c")]
    fn cancel_closes_the_picker(cancel: KeyEvent) {
        let mut modal = opened();
        let action = modal.handle_key(cancel);
        assert!(matches!(action, CommandModalAction::Closed));
        assert!(!modal.is_open());
    }

    #[test]
    fn ctrl_c_closes_the_argument_prompt() {
        let mut modal = opened();
        type_text(&mut modal, "btw");
        modal.handle_key(key(KeyCode::Enter));
        let action = modal.handle_key(key::QUIT.to_key_event());
        assert!(matches!(action, CommandModalAction::Closed));
        assert!(!modal.is_open());
    }

    #[test]
    fn paste_reaches_both_stages() {
        let mut modal = opened();
        assert!(modal.handle_paste("btw"));
        let Stage::Pick(picker) = &modal.stage else {
            unreachable!()
        };
        assert_eq!(picker.search_text(), "btw");

        modal.handle_key(key(KeyCode::Enter));
        assert!(modal.handle_paste("pasted"));
        let Stage::Args { input, .. } = &modal.stage else {
            unreachable!()
        };
        assert_eq!(input.value(), "pasted");
    }

    #[test]
    fn picker_stage_draws_the_command_list() {
        let mut modal = opened();
        let (popup, text) = render(&mut modal);
        assert!(popup.width > 0 && popup.height > 0);
        assert!(text.contains(TITLE.trim()), "title missing from {text:?}");
        assert!(text.contains(ZERO_ARG), "command missing from {text:?}");
    }

    #[test]
    fn prompt_stage_draws_the_command_and_its_description() {
        let mut modal = opened();
        type_text(&mut modal, "btw");
        modal.handle_key(key(KeyCode::Enter));
        let (popup, text) = render(&mut modal);
        assert!(popup.width > 0 && popup.height > 0);
        assert!(text.contains(WITH_ARGS), "command missing from {text:?}");
        assert!(
            text.contains(WITH_ARGS_DESC),
            "description missing from {text:?}"
        );
    }

    /// The hint and the typed value both wrap, so the modal has to grow
    /// with them or the input row falls off the bottom.
    #[test]
    fn prompt_grows_to_keep_the_input_and_footer_visible() {
        let mut modal = opened();
        type_text(&mut modal, "btw");
        modal.handle_key(key(KeyCode::Enter));
        let (short, _) = render(&mut modal);

        type_text(&mut modal, &"word ".repeat(30));
        let (tall, text) = render(&mut modal);

        assert!(
            tall.height > short.height,
            "modal did not grow: {} -> {}",
            short.height,
            tall.height
        );
        assert!(
            text.contains(CHEVRON.trim()),
            "input row missing from {text:?}"
        );
        assert!(text.contains("Back"), "footer missing from {text:?}");
    }

    #[test]
    fn typing_in_the_prompt_is_not_treated_as_a_search() {
        let mut modal = opened();
        type_text(&mut modal, "btw");
        modal.handle_key(key(KeyCode::Enter));
        modal.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        let Stage::Args { input, .. } = &modal.stage else {
            panic!("expected args stage, got {}", stage_name(&modal));
        };
        assert_eq!(input.value(), "h");
    }
}
