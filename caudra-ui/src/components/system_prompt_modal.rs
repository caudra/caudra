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
use ratatui::widgets::Paragraph;

use crate::components::keybindings::key;
use crate::components::modal::{CLOSE_HINT, ESC_LABEL, FooterHits, FooterLine, Modal, SEPARATOR};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::{ModalScroll, Overlay, bar_area, escape_terminal_controls};
use crate::markdown::text_to_wrapped;
use crate::provenance::LineProvenance;
use crate::theme::{self, Theme};

const TITLE_PREFIX: &str = " System Prompt - ";
const WIDTH_PERCENT: u16 = 78;
const MAX_HEIGHT_PERCENT: u16 = 85;
const H_PAD: u16 = 2;
/// The footer keeps its own row rather than trailing the content: a prompt runs
/// to hundreds of rows, and a footer only reachable at the bottom of them is one
/// the reader never sees.
const FOOTER_ROWS: u16 = 1;
const GUTTER_GAP: &str = " ";
const SOURCE_HINT: &str = " source";
const RENDERED_HINT: &str = " rendered";
const COPY_HINT: &str = " copy";
const PROFILE_HINT: &str = " profile";
const RAW_LABEL: &str = "r";
const COPY_LABEL: &str = "y";
const PROFILE_LABEL: &str = "p";
const EMPTY: &str = "No system prompt has been published yet.";
const EMPTY_HINT: &str = "It appears once the agent has bound a model.";
pub(crate) const COPIED: &str = "Copied the system prompt";

const RAW_TARGET: usize = 0;
const COPY_TARGET: usize = 1;
const PROFILE_TARGET: usize = 2;
const CLOSE_TARGET: usize = 3;

/// What the host has to carry out. Closing and switching view are the modal's
/// own business and never reach here.
pub enum SystemPromptAction {
    Consumed,
    Copy(String),
    /// Hand off to the prompt profile picker.
    Profile,
}

/// The painted body, kept until the thing it was painted for changes. Parsing
/// and laying out a prompt of this size is the one cost here worth avoiding on
/// a keypress that only scrolls.
struct Render {
    width: u16,
    raw: bool,
    theme_generation: u64,
    lines: Vec<Line<'static>>,
    content_width: u16,
}

impl Render {
    fn build(source: &str, raw: bool, width: u16, theme_generation: u64, theme: &Theme) -> Self {
        let digits = gutter_digits(source);
        let gutter = u16::try_from(digits + GUTTER_GAP.len()).unwrap_or(u16::MAX);
        let body = width.saturating_sub(gutter);
        let lines = if source.is_empty() {
            notice_lines(theme)
        } else if raw || body == 0 {
            raw_lines(source, digits, theme)
        } else {
            rendered_lines(source, body, digits, theme)
        };
        let content_width = lines
            .iter()
            .map(Line::width)
            .max()
            .and_then(|width| u16::try_from(width).ok())
            .unwrap_or(u16::MAX);
        Self {
            width,
            raw,
            theme_generation,
            lines,
            content_width,
        }
    }

    fn matches(&self, width: u16, raw: bool, theme_generation: u64) -> bool {
        self.width == width && self.raw == raw && self.theme_generation == theme_generation
    }
}

pub struct SystemPromptModal {
    open: bool,
    /// What the run published, snapshotted when the modal opened. An inspector
    /// that re-read the live value would shift under the reader the moment the
    /// next turn bound its prompt.
    source: Arc<str>,
    title: String,
    raw: bool,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    pan_bar: Scrollbar,
    popup: Rect,
    footer: FooterHits,
    cache: Option<Render>,
}

impl Default for SystemPromptModal {
    fn default() -> Self {
        Self {
            open: false,
            source: Arc::from(""),
            title: String::new(),
            raw: false,
            scroll: ModalScroll::new_top(),
            scrollbar: Scrollbar::default(),
            pan_bar: Scrollbar::horizontal(),
            popup: Rect::default(),
            footer: FooterHits::default(),
            cache: None,
        }
    }
}

impl SystemPromptModal {
    pub fn open(&mut self, source: Arc<str>, profile: &str) {
        self.open = true;
        self.source = source;
        self.title = format!("{TITLE_PREFIX}{profile} ");
        self.raw = false;
        self.cache = None;
        self.scroll.reset();
        self.footer.clear();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.source = Arc::from("");
        self.cache = None;
        self.scroll.reset();
        self.footer.reset();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.open && self.popup.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    /// A sideways wheel over the modal, which only reaches anything while a row
    /// runs past the body: rendered markdown is already broken to the columns
    /// it was given.
    pub fn pan(&mut self, delta: i32) {
        self.scroll.pan_by(delta);
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> SystemPromptAction {
        if key_event.code == KeyCode::Esc || key::QUIT.matches(key_event) {
            self.close();
            return SystemPromptAction::Consumed;
        }
        match key_event.code {
            KeyCode::Char('r') => self.toggle_raw(),
            KeyCode::Char('y') => return SystemPromptAction::Copy(self.source.to_string()),
            KeyCode::Char('p') => return SystemPromptAction::Profile,
            _ => {
                self.scroll.handle_key(key_event);
            }
        }
        SystemPromptAction::Consumed
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> SystemPromptAction {
        match self.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return SystemPromptAction::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                return SystemPromptAction::Consumed;
            }
        }
        match self.pan_bar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return SystemPromptAction::Consumed,
            ScrollbarMouse::ScrollTo(column) => {
                self.scroll.pan_to(column as u16);
                return SystemPromptAction::Consumed;
            }
        }
        match self.footer.handle_mouse(event) {
            Some(RAW_TARGET) => self.toggle_raw(),
            Some(COPY_TARGET) => return SystemPromptAction::Copy(self.source.to_string()),
            Some(PROFILE_TARGET) => return SystemPromptAction::Profile,
            Some(CLOSE_TARGET) => self.close(),
            _ => {}
        }
        SystemPromptAction::Consumed
    }

    /// Neither offset survives the switch: the two views agree on the source
    /// behind a row, never on the row a source line landed on.
    fn toggle_raw(&mut self) {
        self.raw = !self.raw;
        self.scroll.reset();
        self.footer.clear();
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("system_prompt_modal", area);

        let theme = theme::current();
        let generation = theme::generation();
        let width = Modal::inner_width(area.width, WIDTH_PERCENT).saturating_sub(H_PAD * 2);
        if !self
            .cache
            .as_ref()
            .is_some_and(|render| render.matches(width, self.raw, generation))
        {
            self.cache = Some(Render::build(
                &self.source,
                self.raw,
                width,
                generation,
                &theme,
            ));
        }
        let render = self.cache.as_ref().expect("cache filled above");
        let rows = u16::try_from(render.lines.len()).unwrap_or(u16::MAX);

        let modal = Modal {
            title: &self.title,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, rows.saturating_add(FOOTER_ROWS));
        let body = Rect {
            x: inner.x.saturating_add(H_PAD),
            y: inner.y,
            width: inner.width.saturating_sub(H_PAD * 2),
            height: inner.height.saturating_sub(FOOTER_ROWS),
        };
        let footer_area = Rect {
            y: inner.y.saturating_add(body.height),
            height: inner.height.min(FOOTER_ROWS),
            ..body
        };

        self.scroll.update_dimensions(rows, body.height);
        self.scroll.fit_width(render.content_width, body.width);
        let offset = self.scroll.offset();
        let pan = self.scroll.pan();

        let footer = footer(self.raw, &theme);
        self.footer.set(footer.hits(footer_area, 0, FOOTER_ROWS));

        // Only the rows on screen are cloned. The painted body is kept whole so
        // a scroll does not repaint it, and handing the paragraph the whole of
        // it would spend on every row what the cache saved.
        let visible: Vec<Line<'static>> = render
            .lines
            .iter()
            .skip(offset as usize)
            .take(body.height as usize)
            .cloned()
            .collect();
        frame.render_widget(Paragraph::new(visible).scroll((0, pan)), body);
        frame.render_widget(
            Paragraph::new(footer.line(self.footer.hovered())),
            footer_area,
        );

        self.scrollbar.draw(frame, inner, rows, offset);
        self.pan_bar
            .draw(frame, bar_area(inner), render.content_width, pan);

        self.popup = popup;
        popup
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

fn notice_lines(theme: &Theme) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(EMPTY, theme.status_dim)),
        Line::from(Span::styled(EMPTY_HINT, theme.tool_dim)),
    ]
}

/// The columns the numbers need, sized to the whole source rather than to the
/// rows on screen so scrolling cannot re-gutter what is already drawn.
fn gutter_digits(source: &str) -> usize {
    source.lines().count().max(1).ilog10() as usize + 1
}

fn gutter_span(number: Option<usize>, digits: usize, style: Style) -> Span<'static> {
    match number {
        Some(number) => Span::styled(format!("{number:>digits$}{GUTTER_GAP}"), style),
        None => Span::styled(" ".repeat(digits + GUTTER_GAP.len()), style),
    }
}

fn raw_lines(source: &str, digits: usize, theme: &Theme) -> Vec<Line<'static>> {
    source
        .lines()
        .enumerate()
        .map(|(index, text)| {
            Line::from(vec![
                gutter_span(Some(index + 1), digits, theme.diff_line_nr),
                Span::styled(escape_terminal_controls(text), theme.assistant),
            ])
        })
        .collect()
}

/// Painted markdown, each row numbered by the source line behind it rather than
/// by its own position: the renderer drops fences and folds paragraphs, so the
/// two only agree on a document that happens to need neither. A row the
/// markdown never produced carries a blank of the same width.
fn rendered_lines(source: &str, width: u16, digits: usize, theme: &Theme) -> Vec<Line<'static>> {
    let (painted, parsed) = text_to_wrapped(
        source,
        theme.assistant,
        width,
        caudra_markdown::render::TOOL_OUTPUT_MAX_LINE_BYTES,
    );
    let starts = line_starts(&parsed);
    let provenance = painted.provenance;
    painted
        .lines
        .into_iter()
        .enumerate()
        .map(|(index, mut line)| {
            let number = provenance
                .get(index)
                .and_then(source_byte)
                .map(|byte| line_number_at(&starts, byte));
            line.spans
                .insert(0, gutter_span(number, digits, theme.diff_line_nr));
            line
        })
        .collect()
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
    const SAMPLER_DIGITS: usize = 2;
    const SYNTAX_SHOWN: &str = "rendered markdown must not draw its own syntax";
    const SOURCE_HIDDEN: &str = "source view must draw the document verbatim";
    const NUMBER_MISSING: &str = "every source line must be reachable by its number";
    const COPY_IS_SOURCE: &str = "copy must hand over the source, never the painted rows";

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
    #[test]
    fn rendered_rows_are_numbered_by_the_source_they_draw_from() {
        let theme = theme::current();
        let numbered: Vec<(Option<usize>, String)> =
            rendered_lines(SAMPLER, SAMPLER_WIDTH, SAMPLER_DIGITS, &theme)
                .iter()
                .map(|line| {
                    let text = line.to_string();
                    let (gutter, rest) = text.split_at(SAMPLER_DIGITS + GUTTER_GAP.len());
                    (gutter.trim().parse().ok(), rest.trim().to_owned())
                })
                .collect();

        assert_eq!(
            numbered,
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

    #[test]
    fn raw_rows_are_numbered_by_their_own_position() {
        let theme = theme::current();
        let numbered: Vec<String> = raw_lines(SAMPLER, SAMPLER_DIGITS, &theme)
            .iter()
            .map(ToString::to_string)
            .collect();

        assert_eq!(numbered.len(), SAMPLER.lines().count());
        assert_eq!(numbered[5], " 6 let a = 1;", "{NUMBER_MISSING}");
        assert_eq!(numbered[11], "12 - bullet ten", "{NUMBER_MISSING}");
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
    fn copy_hands_over_the_source(raw: bool) {
        let mut modal = opened();
        if raw {
            modal.handle_key(key_ev(KeyCode::Char('r')));
        }
        let SystemPromptAction::Copy(text) = modal.handle_key(key_ev(KeyCode::Char('y'))) else {
            panic!("{COPY_IS_SOURCE}");
        };
        assert_eq!(text, prompt(), "{COPY_IS_SOURCE}");
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
            SystemPromptAction::Copy(_)
        ));

        click(&mut modal, RAW_TARGET);
        assert!(render(&mut modal).contains(FENCE), "{SOURCE_HIDDEN}");

        click(&mut modal, CLOSE_TARGET);
        assert!(!modal.is_open());
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
