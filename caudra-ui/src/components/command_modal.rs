//! The centered command palette, opened by a keybinding rather than by
//! typing `/`. Shares its command list with the inline dropdown in
//! [`crate::components::command`]; only the surface differs.

use caudra_grab::grab_scope;
use caudra_workbench::text_field::{FieldKind, TextField, TextKey};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::components::command::{CommandRow, ParsedCommand};
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction};
use crate::components::modal::Modal;
use crate::components::tooltip::Tip;
use crate::components::{
    CHEVRON, Hint, HintBar, Overlay, chevron_span, field_styles, input_text_style,
    visual_line_count,
};
use crate::repaint::Cadence;
use crate::theme;
use unicode_width::UnicodeWidthStr;

const TITLE: &str = " Commands ";
const ARGS_WIDTH_PERCENT: u16 = 65;
const PICK_WIDTH_PERCENT: u16 = 80;
const FOOTER_ROWS: u16 = 1;
const ARGS_MAX_HEIGHT_PERCENT: u16 = 40;

pub enum CommandModalAction {
    Consumed,
    Closed,
    Execute(ParsedCommand),
    Copy(String),
}

enum Stage {
    Closed,
    Pick(Box<ListPicker<CommandRow>>),
    Args {
        input: Box<TextField>,
        row: CommandRow,
        footer: HintBar,
    },
}

/// Describes a stage change without touching `self`, so `handle_key` can
/// return it after the `&mut self.stage` borrow ends.
enum StageAction {
    None,
    Close,
    Back,
    AskArgs { row: Box<CommandRow>, query: String },
    Run(ParsedCommand),
    Copy(String),
}

pub struct CommandModal {
    stage: Stage,
    /// `PickerAction::Select` consumes the picker, so stage one is rebuilt
    /// from here when the argument prompt is dismissed.
    rows: Vec<CommandRow>,
    query: String,
    popup: Rect,
}

impl CommandModal {
    pub fn new() -> Self {
        Self {
            stage: Stage::Closed,
            rows: Vec::new(),
            query: String::new(),
            popup: Rect::default(),
        }
    }

    pub fn open(&mut self, rows: Vec<CommandRow>) {
        self.rows = rows;
        self.query.clear();
        self.stage = Stage::Pick(self.build_picker());
    }

    fn build_picker(&self) -> Box<ListPicker<CommandRow>> {
        // No visible-row cap, so the list grows with the terminal and is
        // bounded only by the picker's own max-height clamp.
        let mut picker = ListPicker::new()
            .with_width_percent(PICK_WIDTH_PERCENT)
            .with_relevance_order()
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
            Stage::Args { input, row, .. } => match key_event.code {
                KeyCode::Enter => StageAction::Run(ParsedCommand {
                    name: row.name.clone(),
                    args: input.text().trim().to_string(),
                }),
                KeyCode::Esc => StageAction::Back,
                _ => match input.handle_key(key_event) {
                    TextKey::Copy(text) | TextKey::Cut(text) => StageAction::Copy(text),
                    TextKey::Ignored if key::QUIT.matches(key_event) => StageAction::Close,
                    _ => StageAction::None,
                },
            },
        };

        self.transition(action)
    }

    /// A footer click in either stage is the key it names, so it takes the
    /// key path and the two can never disagree.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> CommandModalAction {
        let action = match &mut self.stage {
            Stage::Pick(picker) => {
                let query = picker.search_text();
                match picker.handle_mouse(event) {
                    PickerAction::Key(key) => return self.handle_key(key),
                    action => Self::map_pick_action(action, query),
                }
            }
            Stage::Args { footer, .. } => {
                return match footer.handle_mouse(event) {
                    Some(key) => self.handle_key(key),
                    None => CommandModalAction::Consumed,
                };
            }
            Stage::Closed => return CommandModalAction::Consumed,
        };

        self.transition(action)
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        match &mut self.stage {
            Stage::Closed => false,
            Stage::Pick(picker) => picker.handle_paste(text),
            Stage::Args { input, .. } => {
                input.paste(text);
                true
            }
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        if let Stage::Pick(picker) = &mut self.stage {
            picker.scroll(delta);
        }
    }

    /// Whichever stage is drawn: the argument prompt is a modal in its own
    /// right, not a hole in the one it replaced.
    pub fn contains(&self, pos: Position) -> bool {
        self.is_open() && self.popup.contains(pos)
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
            PickerAction::Copy(text) => StageAction::Copy(text),
            PickerAction::Consumed | PickerAction::Toggle(..) | PickerAction::Key(_) => {
                StageAction::None
            }
        }
    }

    fn transition(&mut self, action: StageAction) -> CommandModalAction {
        match action {
            StageAction::None => CommandModalAction::Consumed,
            StageAction::Copy(text) => CommandModalAction::Copy(text),
            StageAction::AskArgs { row, query } => {
                self.query = query;
                self.stage = Stage::Args {
                    input: Box::new(TextField::new(FieldKind::Line)),
                    row: *row,
                    footer: HintBar::default(),
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
        grab_scope!("command_modal", area);
        let popup = match &mut self.stage {
            Stage::Closed => Rect::default(),
            Stage::Pick(picker) => picker.view(frame, area),
            Stage::Args { input, row, footer } => {
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
                let input_lines = prompt_lines(input, width);
                let input_rows = input_lines.len() as u16;
                let (popup, inner) =
                    modal.render(frame, area, hint_rows + input_rows + FOOTER_ROWS);

                let [hint_area, input_area, footer_area] = Layout::vertical([
                    Constraint::Length(hint_rows),
                    Constraint::Min(1),
                    Constraint::Length(FOOTER_ROWS),
                ])
                .areas(inner);
                let bg = t.surface_style();
                frame.render_widget(
                    Paragraph::new(row.description.as_str())
                        .style(bg.patch(t.input_placeholder))
                        .wrap(Wrap { trim: true }),
                    hint_area,
                );
                frame.render_widget(Paragraph::new(input_lines).style(bg), input_area);
                frame.render_widget(
                    Paragraph::new(footer.line(footer_area, args_footer())).style(bg),
                    footer_area,
                );
                popup
            }
        };
        self.popup = popup;
        popup
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

    fn tooltip(&self) -> Option<Tip> {
        match &self.stage {
            Stage::Pick(picker) => picker.tooltip(),
            Stage::Closed | Stage::Args { .. } => None,
        }
    }
}

/// Columns inside the border, floored above the chevron so the input row
/// always has somewhere to wrap into.
fn modal_inner_width(area: Rect, width_percent: u16) -> u16 {
    Modal::inner_width(area.width, width_percent).max(CHEVRON.width() as u16 + 1)
}

/// The argument wrapped beside its chevron, every row after the first
/// indented to line up under the text.
fn prompt_lines(input: &TextField, width: u16) -> Vec<Line<'static>> {
    let indent = CHEVRON.width();
    let styles = field_styles(input_text_style());
    let mut lines =
        input.paint_wrapped(usize::from(width).saturating_sub(indent), &styles, true, "");
    for (index, line) in lines.iter_mut().enumerate() {
        let prefix = match index {
            0 => chevron_span(),
            _ => Span::raw(" ".repeat(indent)),
        };
        line.spans.insert(0, prefix);
    }
    lines
}

/// Zero for empty text, so a command with no description spends no row on it.
fn wrapped_rows(text: &str, width: u16) -> u16 {
    if text.is_empty() {
        return 0;
    }
    visual_line_count(text.width(), width.max(1) as usize) as u16
}

fn footer() -> Vec<Hint> {
    vec![Hint::bind(key::ENTER, "Run"), Hint::bind(key::ESC, "Close")]
}

fn args_footer() -> Vec<Hint> {
    vec![Hint::bind(key::ENTER, "Run"), Hint::bind(key::ESC, "Back")]
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
    const TALL_HEIGHT: u16 = 60;
    const MAX_HEIGHT_PERCENT: u16 = 80;
    const OVERFLOW_ROWS: usize = 40;
    const SUBSTRING_QUERY: &str = "view";
    const BETTER_MATCH: &str = "/view";
    const WORSE_MATCH: &str = "/review";
    const ARGUMENT: &str = "why";

    fn row(name: &str, description: &str, max_args: usize) -> CommandRow {
        CommandRow {
            name: name.to_string(),
            description: description.to_string(),
            max_args,
            section: SECTION_BUILTIN,
            disabled: false,
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
        render_at(modal, TEST_HEIGHT)
    }

    fn render_at(modal: &mut CommandModal, height: u16) -> (Rect, String) {
        let backend = ratatui::backend::TestBackend::new(TEST_WIDTH, height);
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

    /// The list is capped only by the picker's max-height clamp, so a taller
    /// terminal shows more commands instead of a fixed window of them.
    #[test]
    fn the_list_grows_with_the_terminal_height() {
        let rows: Vec<CommandRow> = (0..OVERFLOW_ROWS)
            .map(|i| row(&format!("/cmd{i}"), "", 0))
            .collect();

        let mut modal = CommandModal::new();
        modal.open(rows.clone());
        let (short, _) = render_at(&mut modal, TEST_HEIGHT);

        let mut modal = CommandModal::new();
        modal.open(rows);
        let (tall, _) = render_at(&mut modal, TALL_HEIGHT);

        assert!(
            tall.height > short.height,
            "expected the taller terminal to show a taller modal, got {} then {}",
            short.height,
            tall.height
        );
        assert!(
            tall.height <= TALL_HEIGHT * MAX_HEIGHT_PERCENT / 100,
            "modal must stay within the max-height clamp, got {}",
            tall.height
        );
    }

    /// `/review` contains `view` too, and it is listed first in
    /// `BUILTIN_COMMANDS`. Source order must not beat the fuzzy score.
    #[test]
    fn a_better_match_outranks_an_earlier_one() {
        let mut modal = CommandModal::new();
        modal.open(vec![
            row(WORSE_MATCH, "Review the last reply", 0),
            row(BETTER_MATCH, "Cycle transcript view", 0),
        ]);
        type_text(&mut modal, SUBSTRING_QUERY);
        let Stage::Pick(picker) = &modal.stage else {
            unreachable!()
        };
        assert_eq!(
            picker.selected_item().map(|r| r.name.as_str()),
            Some(BETTER_MATCH),
            "the first row should be the better match"
        );
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
        assert_eq!(input.text(), "pasted");
    }

    #[test]
    fn ctrl_c_copies_a_selected_argument_and_keeps_the_prompt() {
        let mut modal = opened();
        type_text(&mut modal, "btw");
        modal.handle_key(key(KeyCode::Enter));
        type_text(&mut modal, ARGUMENT);
        modal.handle_key(key::SELECT_ALL.to_key_event());

        let action = modal.handle_key(key::QUIT.to_key_event());

        assert!(matches!(action, CommandModalAction::Copy(ref text) if text == ARGUMENT));
        assert_eq!(stage_name(&modal), "args");
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
        assert_eq!(input.text(), "h");
    }
}
