//! Shows the system prompt the agent actually bound, as markdown or as source.
//!
//! The text is whatever the run published, never a rebuild of it, so what the
//! modal shows and what the provider was sent cannot drift. Before the first
//! turn the published prompt is assembled with empty prompt slots, so a plugin
//! that fills one only appears once a turn has started.

use std::sync::Arc;

use caudra_grab::grab_scope;
use caudra_markdown::render::SpanSource;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::components::document_view::{
    COPIED_SELECTION, COPY_HINT, COPY_LABEL, DocumentMouse, DocumentView, Painted, UNBOUND_HINT,
    body_width,
};
use crate::components::keybindings::key;
use crate::components::modal::{CLOSE_HINT, ESC_LABEL, FooterHits, FooterLine, SEPARATOR};
use crate::components::{Overlay, escape_terminal_controls, plain_char};
use crate::markdown::text_to_wrapped;
use crate::provenance::LineProvenance;
use crate::theme::{self, Theme};

const TITLE_PREFIX: &str = " System Prompt - ";
const GUTTER_GAP: &str = " ";
const SOURCE_HINT: &str = " source";
const RENDERED_HINT: &str = " rendered";
const PROFILE_HINT: &str = " profile";
const RAW_LABEL: &str = "r";
const PROFILE_LABEL: &str = "p";
const EMPTY: &str = "No system prompt has been published yet.";
pub(crate) const COPIED: &str = "Copied the system prompt";

const RAW_TARGET: usize = 0;
const COPY_TARGET: usize = 1;
const PROFILE_TARGET: usize = 2;
const CLOSE_TARGET: usize = 3;

/// What the host has to carry out. Closing, selecting and switching view are
/// the modal's own business and never reach here.
pub enum SystemPromptAction {
    Consumed,
    Copy {
        text: String,
        label: &'static str,
    },
    /// Hand off to the prompt profile picker.
    Profile,
}

#[derive(Default)]
pub struct SystemPromptModal {
    open: bool,
    /// What the run published, snapshotted when the modal opened. An inspector
    /// that re-read the live value would shift under the reader the moment the
    /// next turn bound its prompt.
    source: Arc<str>,
    title: String,
    raw: bool,
    /// Columns the numbers hold beside the body, fixed while the modal is open
    /// because the source it counts cannot change under it.
    gutter: u16,
    /// Painted per view, wrap width and theme generation.
    document: DocumentView<(bool, u16, u64)>,
    popup: Rect,
    footer: FooterHits,
}

impl SystemPromptModal {
    pub fn open(&mut self, source: Arc<str>, profile: &str) {
        self.gutter = gutter_width(&source);
        self.open = true;
        self.source = source;
        self.title = format!("{TITLE_PREFIX}{profile} ");
        self.raw = false;
        self.document.reset();
        self.footer.clear();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.source = Arc::from("");
        self.document.reset();
        self.footer.reset();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.open && self.popup.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.document.scroll(delta);
    }

    /// A sideways wheel over the modal, which only reaches anything while a row
    /// runs past the body: rendered markdown is already broken to the columns
    /// it was given.
    pub fn pan(&mut self, delta: i32) {
        self.document.pan(delta);
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> SystemPromptAction {
        // Before the close chord, because a sweep the reader can see is what
        // the chord is for. With nothing selected it is the way out, as it is
        // everywhere else.
        if key::QUIT.matches(key_event)
            && let Some(copy) = self.copy_selection()
        {
            return copy;
        }
        if key_event.code == KeyCode::Esc || key::QUIT.matches(key_event) {
            self.close();
            return SystemPromptAction::Consumed;
        }
        if key::SELECT_ALL.matches(key_event) {
            self.document.select_all();
            return SystemPromptAction::Consumed;
        }
        match plain_char(&key_event) {
            Some('r') => self.toggle_raw(),
            Some('y') => return self.copy(),
            Some('p') => return SystemPromptAction::Profile,
            _ => {
                self.document.handle_scroll_key(key_event);
            }
        }
        SystemPromptAction::Consumed
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> SystemPromptAction {
        match self.document.handle_mouse(event) {
            DocumentMouse::Consumed => return SystemPromptAction::Consumed,
            DocumentMouse::Copy(text) => return copy_swept(text),
            DocumentMouse::Passthrough => {}
        }
        match self.footer.handle_mouse(event) {
            Some(RAW_TARGET) => self.toggle_raw(),
            Some(COPY_TARGET) => return self.copy(),
            Some(PROFILE_TARGET) => return SystemPromptAction::Profile,
            Some(CLOSE_TARGET) => self.close(),
            _ => {}
        }
        SystemPromptAction::Consumed
    }

    /// The sweep if there is one, the whole document otherwise. A reader who
    /// marked a passage asked for that passage, which in the rendered view is
    /// the rendering; `y` with nothing marked is what reaches the source.
    fn copy(&self) -> SystemPromptAction {
        self.copy_selection()
            .unwrap_or_else(|| SystemPromptAction::Copy {
                text: self.source.to_string(),
                label: COPIED,
            })
    }

    fn copy_selection(&self) -> Option<SystemPromptAction> {
        self.document.selected_text().map(copy_swept)
    }

    /// Neither offset nor sweep survives the switch: the two views agree on the
    /// source behind a row, never on the row a source line landed on.
    fn toggle_raw(&mut self) {
        self.raw = !self.raw;
        self.document.reset();
        self.footer.clear();
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("system_prompt_modal", area);

        let theme = theme::current();
        let width = body_width(area.width, self.gutter);
        self.document
            .ensure((self.raw, width, theme::generation()), self.gutter, || {
                paint(&self.source, self.raw, width, self.gutter, &theme)
            });
        let footer = footer(self.raw, &theme);
        self.popup = self
            .document
            .render(frame, area, &self.title, &footer, &mut self.footer);
        self.popup
    }

    #[cfg(test)]
    pub(crate) fn footer_hit(&self, index: usize) -> Rect {
        self.footer.hit(index)
    }
}

impl Overlay for SystemPromptModal {
    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        self.close();
    }
}

/// Hands over the text a sweep covers, under the label every sweep copies
/// with, whether a key asked or the pointer let go.
fn copy_swept(text: String) -> SystemPromptAction {
    SystemPromptAction::Copy {
        text,
        label: COPIED_SELECTION,
    }
}

fn footer(raw: bool, theme: &Theme) -> FooterLine {
    let mut footer = FooterLine::default();
    footer.command(RAW_LABEL, theme.keybind_key);
    footer.describe(
        if raw { RENDERED_HINT } else { SOURCE_HINT },
        theme.tool_dim,
    );
    footer.text(SEPARATOR, theme.tool_dim);
    footer.command(COPY_LABEL, theme.keybind_key);
    footer.describe(COPY_HINT, theme.tool_dim);
    footer.text(SEPARATOR, theme.tool_dim);
    footer.command(PROFILE_LABEL, theme.keybind_key);
    footer.describe(PROFILE_HINT, theme.tool_dim);
    footer.text(SEPARATOR, theme.tool_dim);
    footer.command(ESC_LABEL, theme.keybind_key);
    footer.describe(CLOSE_HINT, theme.tool_dim);
    footer
}

/// The view `raw` names, painted to `width`, with each row's source line number
/// in the gutter beside it.
fn paint(source: &str, raw: bool, width: u16, gutter: u16, theme: &Theme) -> Painted {
    let (lines, numbers) = numbered_rows(source, raw, width, theme);
    let cells = numbers
        .into_iter()
        .map(|number| gutter_line(number, gutter, theme.diff_line_nr))
        .collect();
    Painted::new(lines, cells, Vec::new())
}

fn numbered_rows(
    source: &str,
    raw: bool,
    width: u16,
    theme: &Theme,
) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    let (lines, numbers) = if source.is_empty() {
        (notice_lines(theme), Vec::new())
    } else if raw || width == 0 {
        raw_lines(source, theme)
    } else {
        rendered_lines(source, width, theme)
    };
    (lines, first_of_each_line(numbers))
}

fn notice_lines(theme: &Theme) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(EMPTY, theme.status_dim)),
        Line::from(Span::styled(UNBOUND_HINT, theme.tool_dim)),
    ]
}

/// The columns the numbers hold, sized to the whole source rather than to the
/// rows on screen so scrolling cannot re-gutter what is already drawn.
fn gutter_width(source: &str) -> u16 {
    let digits = source.lines().count().max(1).ilog10() as usize + 1;
    u16::try_from(digits + GUTTER_GAP.len()).unwrap_or(u16::MAX)
}

fn gutter_line(number: Option<usize>, width: u16, style: Style) -> Line<'static> {
    let width = usize::from(width).saturating_sub(GUTTER_GAP.len());
    let text = match number {
        Some(number) => format!("{number:>width$}{GUTTER_GAP}"),
        None => String::new(),
    };
    Line::from(Span::styled(text, style))
}

/// Drops the number from a row that continues the line above, leaving a wrapped
/// line numbered once at its head the way an editor does. A row that opens no
/// line of its own carried none to begin with.
fn first_of_each_line(numbers: Vec<Option<usize>>) -> Vec<Option<usize>> {
    let mut previous = None;
    numbers
        .into_iter()
        .map(|number| {
            let first = number.filter(|number| Some(*number) != previous);
            previous = number.or(previous);
            first
        })
        .collect()
}

fn raw_lines(source: &str, theme: &Theme) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    source
        .lines()
        .enumerate()
        .map(|(index, text)| {
            (
                Line::from(Span::styled(
                    escape_terminal_controls(text),
                    theme.assistant,
                )),
                Some(index + 1),
            )
        })
        .unzip()
}

/// Painted markdown, each row numbered by the source line behind it rather than
/// by its own position: the renderer drops fences and folds paragraphs, so the
/// two only agree on a document that happens to need neither.
fn rendered_lines(
    source: &str,
    width: u16,
    theme: &Theme,
) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    let (painted, parsed) = text_to_wrapped(
        source,
        theme.assistant,
        width,
        caudra_markdown::render::TOOL_OUTPUT_MAX_LINE_BYTES,
    );
    let starts = line_starts(&parsed);
    let numbers = painted
        .provenance
        .iter()
        .map(|row| source_byte(row).map(|byte| line_number_at(&starts, byte)))
        .collect();
    (painted.lines, numbers)
}

/// The byte a row is numbered by: the first of its spans that still points at
/// the source. A row's own range carries the syntax its spans dropped, so for
/// the first row of a fenced block it opens on the fence a line earlier than
/// the code the reader is looking at. Only a row that painted no source at all
/// — a blank between blocks — falls back to it.
fn source_byte(row: &LineProvenance) -> Option<u32> {
    row.spans
        .iter()
        .find_map(|span| match span {
            SpanSource::Range(source) => Some(source.range.start),
            SpanSource::Chrome | SpanSource::Unknown => None,
        })
        .or_else(|| row.line.as_ref().map(|range| range.start))
}

/// Where every line of `text` starts, so a provenance byte can be read back as
/// the line number a reader would count to.
fn line_starts(text: &str) -> Vec<u32> {
    std::iter::once(0)
        .chain(
            text.match_indices('\n')
                .filter_map(|(index, _)| u32::try_from(index + 1).ok()),
        )
        .collect()
}

fn line_number_at(starts: &[u32], byte: u32) -> usize {
    starts.partition_point(|&start| start <= byte).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{buffer_text, key as key_ev};
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use test_case::test_case;

    const PROFILE: &str = "review";
    const HEADING: &str = "## Tone and style";
    const BODY: &str = "Be concise and direct.";
    const FENCE: &str = "```";
    const TERMINAL_WIDTH: u16 = 100;
    const TERMINAL_HEIGHT: u16 = 40;
    /// One of every block whose rows the renderer numbers differently: a
    /// heading that loses its hashes, a blank the markdown produced, a blank it
    /// did not, a fenced block, and a list.
    const SAMPLER: &str = "# One\n\npara two\n\n```\nlet a = 1;\nlet b = 2;\nlet c = 3;\n```\n\n- bullet nine\n- bullet ten\n";
    const SAMPLER_WIDTH: u16 = 60;
    /// A paragraph no width in this test can hold on one row, so the renderer
    /// has to fold it and the gutter has to say so once.
    const FOLDING: &str = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa quebec romeo\n";
    const FOLDING_WIDTH: u16 = 30;
    const SYNTAX_SHOWN: &str = "rendered markdown must not draw its own syntax";
    const SOURCE_HIDDEN: &str = "source view must draw the document verbatim";
    const NUMBER_MISSING: &str = "every source line must be reachable by its number";
    const NUMBER_REPEATED: &str = "a folded line must be numbered once, at its head";
    /// Appended [`FILLER_LINES`] times, so the prompt runs past the body.
    const FILLER: &str = "filler\n";
    const FILLER_LINES: usize = 60;
    const COPY_IS_SOURCE: &str = "copy must hand over the source, never the painted rows";
    const SELECTION_WRONG: &str = "a sweep must copy the rows it was drawn over";
    const FOOTER_UNHEARD: &str = "a click on the footer must reach it while a sweep stands";
    const NOT_SCROLLED: &str = "the fixture must start a line down a prompt longer than the body";
    const CHORD_TAKEN: &str = "Ctrl+Y must scroll a line up, never copy";

    fn prompt() -> String {
        format!("{HEADING}\n\n{BODY}\n\n{FENCE}\nlet x = 1;\n{FENCE}\n")
    }

    fn opened() -> SystemPromptModal {
        let mut modal = SystemPromptModal::default();
        modal.open(Arc::from(prompt().as_str()), PROFILE);
        modal
    }

    fn render(modal: &mut SystemPromptModal) -> String {
        let backend = ratatui::backend::TestBackend::new(TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                modal.view(frame, frame.area());
            })
            .unwrap();
        buffer_text(terminal.backend().buffer())
    }

    fn mouse(kind: MouseEventKind, at: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: at.x,
            row: at.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn click(modal: &mut SystemPromptModal, target: usize) -> SystemPromptAction {
        let hit = modal.footer_hit(target);
        modal.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), hit));
        modal.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), hit))
    }

    #[test]
    fn rendered_mode_drops_markdown_syntax() {
        let drawn = render(&mut opened());
        assert!(drawn.contains(BODY));
        assert!(!drawn.contains(FENCE), "{SYNTAX_SHOWN}");
        assert!(!drawn.contains(HEADING), "{SYNTAX_SHOWN}");
    }

    #[test]
    fn raw_mode_shows_the_source_verbatim() {
        let mut modal = opened();
        modal.handle_key(key_ev(KeyCode::Char('r')));
        let drawn = render(&mut modal);
        assert!(drawn.contains(HEADING), "{SOURCE_HIDDEN}");
        assert!(drawn.contains(FENCE), "{SOURCE_HIDDEN}");
    }

    /// Every row of the sampler, numbered by the source line it draws from.
    /// The fenced block is the case that pins the rule: its rows carry the
    /// fences in their own ranges, so numbering by the row would name line 5
    /// for the code on line 6 and line 9 for the code on line 8.
    fn numbered(source: &str, width: u16) -> Vec<(Option<usize>, String)> {
        let (lines, numbers) = numbered_rows(source, false, width, &theme::current());
        numbers
            .into_iter()
            .zip(&lines)
            .map(|(number, line)| (number, line.to_string().trim_end().to_owned()))
            .collect()
    }

    #[test]
    fn rendered_rows_are_numbered_by_the_source_they_draw_from() {
        assert_eq!(
            numbered(SAMPLER, SAMPLER_WIDTH),
            vec![
                (Some(1), "One".to_owned()),
                (Some(2), String::new()),
                (Some(3), "para two".to_owned()),
                (None, String::new()),
                (Some(6), "│ let a = 1;".to_owned()),
                (Some(7), "│ let b = 2;".to_owned()),
                (Some(8), "│ let c = 3;".to_owned()),
                (None, String::new()),
                (Some(11), "• bullet nine".to_owned()),
                (Some(12), "• bullet ten".to_owned()),
            ],
            "{NUMBER_MISSING}"
        );
    }

    /// One source line folded across several rows. Only the row that opens it
    /// carries the number, the way an editor gutters a wrapped line.
    #[test]
    fn a_folded_line_is_numbered_once() {
        let rows = numbered(FOLDING, FOLDING_WIDTH);

        assert!(rows.len() > 1, "{NUMBER_REPEATED}: {rows:?}");
        assert_eq!(rows[0].0, Some(1), "{NUMBER_MISSING}");
        assert!(
            rows[1..].iter().all(|(number, _)| number.is_none()),
            "{NUMBER_REPEATED}: {rows:?}"
        );
    }

    #[test]
    fn raw_rows_are_numbered_by_their_own_position() {
        let theme = theme::current();
        let (lines, numbers) = raw_lines(SAMPLER, &theme);

        assert_eq!(lines.len(), SAMPLER.lines().count());
        assert_eq!(numbers[5], Some(6), "{NUMBER_MISSING}");
        assert_eq!(lines[5].to_string(), "let a = 1;");
        assert_eq!(numbers[11], Some(12), "{NUMBER_MISSING}");
    }

    #[test]
    fn the_title_names_the_active_profile() {
        assert!(render(&mut opened()).contains(PROFILE));
    }

    #[test]
    fn an_unpublished_prompt_says_so() {
        let mut modal = SystemPromptModal::default();
        modal.open(Arc::from(""), PROFILE);
        assert!(render(&mut modal).contains(EMPTY));
    }

    #[test_case(key_ev(KeyCode::Esc)     ; "esc")]
    #[test_case(key::QUIT.to_key_event() ; "ctrl_c")]
    fn the_modal_closes(key_event: KeyEvent) {
        let mut modal = opened();
        modal.handle_key(key_event);
        assert!(!modal.is_open());
    }

    #[test]
    fn an_unclaimed_key_scrolls_rather_than_closing() {
        let mut modal = opened();
        modal.handle_key(key_ev(KeyCode::Char('a')));
        assert!(modal.is_open());
    }

    #[test_case(true  ; "raw")]
    #[test_case(false ; "rendered")]
    fn copy_without_a_sweep_hands_over_the_source(raw: bool) {
        let mut modal = opened();
        if raw {
            modal.handle_key(key_ev(KeyCode::Char('r')));
        }
        let SystemPromptAction::Copy { text, label } = modal.handle_key(key_ev(KeyCode::Char('y')))
        else {
            panic!("{COPY_IS_SOURCE}");
        };
        assert_eq!(text, prompt(), "{COPY_IS_SOURCE}");
        assert_eq!(label, COPIED);
    }

    /// `y` copies, and the chord spelled with it scrolls like it does in every
    /// other modal.
    #[test]
    fn ctrl_y_scrolls_a_line_up_rather_than_copying() {
        let mut modal = SystemPromptModal::default();
        let source = format!("{}{}", prompt(), FILLER.repeat(FILLER_LINES));
        modal.open(Arc::from(source.as_str()), PROFILE);
        modal.handle_key(key_ev(KeyCode::Char('r')));
        render(&mut modal);
        modal.scroll(-1);
        assert!(!render(&mut modal).contains(HEADING), "{NOT_SCROLLED}");

        let action = modal.handle_key(key::SCROLL_LINE_UP.to_key_event());

        assert!(
            matches!(action, SystemPromptAction::Consumed),
            "{CHORD_TAKEN}"
        );
        assert!(render(&mut modal).contains(HEADING), "{CHORD_TAKEN}");
    }

    #[test]
    fn the_profile_key_asks_the_host_for_the_picker() {
        let mut modal = opened();
        assert!(matches!(
            modal.handle_key(key_ev(KeyCode::Char('p'))),
            SystemPromptAction::Profile
        ));
        assert!(modal.is_open());
    }

    #[test]
    fn the_footer_answers_the_pointer() {
        let mut modal = opened();
        render(&mut modal);

        assert!(matches!(
            click(&mut modal, PROFILE_TARGET),
            SystemPromptAction::Profile
        ));
        assert!(matches!(
            click(&mut modal, COPY_TARGET),
            SystemPromptAction::Copy { .. }
        ));

        click(&mut modal, RAW_TARGET);
        assert!(render(&mut modal).contains(FENCE), "{SOURCE_HIDDEN}");

        click(&mut modal, CLOSE_TARGET);
        assert!(!modal.is_open());
    }

    /// Drags the pointer from the first character of one body row to a column
    /// partway along a later one and lets go, the way a reader marks a
    /// passage. Answers with what the release asked of the host.
    fn sweep(
        modal: &mut SystemPromptModal,
        from: (u16, u16),
        to: (u16, u16),
    ) -> SystemPromptAction {
        let content = modal.document.content();
        let at = |(column, row): (u16, u16)| Rect {
            x: content.x.saturating_add(column),
            y: content.y.saturating_add(row),
            width: 1,
            height: 1,
        };
        modal.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), at(from)));
        modal.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), at(to)));
        modal.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), at(to)))
    }

    #[test]
    fn a_sweep_copies_the_rows_it_covers() {
        let mut modal = opened();
        modal.handle_key(key_ev(KeyCode::Char('r')));
        render(&mut modal);

        // The fixture's rows 0 to 2 are the heading, a blank, and the body.
        let SystemPromptAction::Copy { text, label } = sweep(&mut modal, (0, 0), (4, 2)) else {
            panic!("{SELECTION_WRONG}");
        };
        assert_eq!(
            text,
            format!("{HEADING}\n\n{}", &BODY[..4]),
            "{SELECTION_WRONG}"
        );
        assert_eq!(label, COPIED_SELECTION);
    }

    /// The chord closes the modal, except while a sweep is standing, when the
    /// thing it is for is copying that sweep.
    #[test]
    fn the_close_chord_copies_a_sweep_before_it_closes() {
        let mut modal = opened();
        render(&mut modal);
        sweep(&mut modal, (0, 0), (3, 0));

        assert!(matches!(
            modal.handle_key(key::QUIT.to_key_event()),
            SystemPromptAction::Copy { .. }
        ));
        assert!(modal.is_open());

        // A bare click collapses the sweep to nothing.
        sweep(&mut modal, (0, 0), (0, 0));
        modal.handle_key(key::QUIT.to_key_event());
        assert!(!modal.is_open());
    }

    #[test]
    fn select_all_then_copy_hands_over_every_row() {
        let mut modal = opened();
        modal.handle_key(key_ev(KeyCode::Char('r')));
        render(&mut modal);
        modal.handle_key(key::SELECT_ALL.to_key_event());

        let SystemPromptAction::Copy { text, label } = modal.handle_key(key_ev(KeyCode::Char('y')))
        else {
            panic!("{SELECTION_WRONG}");
        };
        assert_eq!(text, prompt().trim_end(), "{SELECTION_WRONG}");
        assert_eq!(label, COPIED_SELECTION);
    }

    #[test]
    fn a_press_on_the_footer_leaves_a_sweep_standing() {
        let mut modal = opened();
        render(&mut modal);
        sweep(&mut modal, (0, 0), (3, 0));

        assert!(matches!(
            click(&mut modal, COPY_TARGET),
            SystemPromptAction::Copy {
                label: COPIED_SELECTION,
                ..
            }
        ));
    }

    /// Only a release that ends a press on the body hands the sweep over. A
    /// click's release is the footer's, or closing would copy instead.
    #[test]
    fn a_footer_click_with_a_sweep_standing_reaches_the_footer() {
        let mut modal = opened();
        render(&mut modal);
        sweep(&mut modal, (0, 0), (3, 0));

        click(&mut modal, CLOSE_TARGET);

        assert!(!modal.is_open(), "{FOOTER_UNHEARD}");
    }

    #[test]
    fn switching_view_drops_a_sweep() {
        let mut modal = opened();
        render(&mut modal);
        sweep(&mut modal, (0, 0), (3, 0));
        modal.handle_key(key_ev(KeyCode::Char('r')));

        assert!(modal.document.selected_text().is_none());
    }

    #[test_case(0,      1 ; "first_byte_is_line_one")]
    #[test_case(3,      1 ; "before_the_break_is_line_one")]
    #[test_case(4,      2 ; "after_the_break_is_line_two")]
    #[test_case(u32::MAX, 3 ; "past_the_end_is_the_last_line")]
    fn a_byte_reads_back_as_its_line(byte: u32, expected: usize) {
        let starts = line_starts("abc\ndef\nghi");
        assert_eq!(line_number_at(&starts, byte), expected);
    }
}
