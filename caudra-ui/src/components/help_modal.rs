use crate::components::Overlay;
use crate::components::docs_link::DocsTopic;
use crate::components::keybindings::{
    ALT_SEP, KEYBINDS, KeyLabel, KeybindContext, all_contexts, key,
};
use crate::components::modal::{CLOSE_HINT, ESC_LABEL, FooterHits, FooterLine, Modal, SEPARATOR};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::{
    ModalScroll, bar_area, chevron_span, field_styles, input_text_style, match_spans, plain_char,
};
use crate::theme::{self, Theme};

use std::borrow::Cow;
use std::ops::Range;

use caudra_config::FeatureFlags;
use caudra_grab::grab_scope;
use caudra_workbench::text_field::{FieldKind, TextField, TextKey};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

const TITLE: &str = " Keybindings ";
/// Wide enough on a common terminal for the longest descriptions to sit
/// beside their keys without panning.
const WIDTH_PERCENT: u16 = 80;
const MAX_HEIGHT_PERCENT: u16 = 80;
const KEY_COL_GAP: usize = 2;
/// The widest a section's key column grows. A longer label wraps between its
/// keys rather than pushing every description in its section away from the
/// key it belongs to.
const KEY_COL_MAX: usize = 20;
const CONTINUED: &str = " /";
const MODIFIER_SIGILS: [(&str, &str); 4] = [
    ("Ctrl+", "⌃"),
    ("Shift+", "⇧"),
    ("Alt+", "⌥"),
    ("Super+", "⌘"),
];
const PREFIX: &str = "  ";
const FOOTER_ROWS: u16 = 1;
const QUERY_ROWS: u16 = 1;
const DOCS_COMMAND: &str = "/docs";
const DOCS_HINT: &str = " read the documentation";
const SEARCH_KEY: char = '/';
const SEARCH_LABEL: &str = "/";
const SEARCH_HINT: &str = " search";
const CLEAR_HINT: &str = " clear";
const KEEP_LABEL: &str = "Enter";
const KEEP_HINT: &str = " keep";
const QUERY_PLACEHOLDER: &str = "Filter by key, action, or section";
const NO_MATCHES: &str = "No keybinding matches";
const INPUT_PREFIXES_LABEL: &str = "Input Prefixes";
const BROWSE_TARGETS: [HelpTarget; 3] = [
    HelpTarget::Search,
    HelpTarget::Command(DOCS_COMMAND),
    HelpTarget::Close,
];
const FILTERED_TARGETS: [HelpTarget; 3] = [
    HelpTarget::Search,
    HelpTarget::Command(DOCS_COMMAND),
    HelpTarget::Clear,
];
const QUERY_TARGETS: [HelpTarget; 2] = [HelpTarget::Keep, HelpTarget::Clear];

const INPUT_PREFIXES: &[(&str, &str)] = &[
    ("!", "Run shell command (visible to agent)"),
    ("!!", "Run shell command (hidden from agent)"),
];

/// What a footer control does when pressed, indexed the way [`FooterLine`]
/// targets are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HelpTarget {
    Search,
    /// A session command line, left to the host to run.
    Command(&'static str),
    Close,
    /// Drops the filter, the way `Esc` does while one is set.
    Clear,
    /// Hands the keys back to the reference and keeps the filter.
    Keep,
}

impl HelpTarget {
    fn words(self) -> (&'static str, &'static str) {
        match self {
            Self::Search => (SEARCH_LABEL, SEARCH_HINT),
            Self::Command(command) => (command, DOCS_HINT),
            Self::Close => (ESC_LABEL, CLOSE_HINT),
            Self::Clear => (ESC_LABEL, CLEAR_HINT),
            Self::Keep => (KEEP_LABEL, KEEP_HINT),
        }
    }
}

/// What the pointer asked of the host. `Ignored` hands the event back, which
/// is how a sweep through the reference and the sideways wheel still reach it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HelpMouse {
    Ignored,
    Consumed,
    Command(&'static str),
}

/// One heading of the reference and the rows under it. Every section sits at
/// the same depth: a nested heading reads as part of the one above it, and a
/// picker's keys are no less its own for the pickers sharing some.
struct Section {
    label: &'static str,
    rows: Vec<(KeyLabel, &'static str)>,
}

pub struct HelpModal {
    open: bool,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    /// A binding's description is as long as it is, so a narrow modal cuts the
    /// half of the reference that says what a key does.
    pan_bar: Scrollbar,
    footer: FooterHits,
    popup: Rect,
    query: TextField,
    query_focused: bool,
}

/// Every section the reference lists while `features` are on, in the order
/// the contexts are declared, each parent followed by its children.
fn sections(features: FeatureFlags) -> Vec<Section> {
    let rows = |context: KeybindContext| -> Vec<(KeyLabel, &'static str)> {
        KEYBINDS
            .iter()
            .filter(|kb| kb.context == context && kb.is_visible(features))
            .map(|kb| (kb.label, kb.description))
            .collect()
    };
    let section = |context: KeybindContext| Section {
        label: context.label(),
        rows: rows(context),
    };
    let mut sections = Vec::new();
    for context in all_contexts().filter(|context| {
        context.parent().is_none() && context.feature().is_none_or(|f| features.enabled(f))
    }) {
        sections.push(section(context));
        sections.extend(
            all_contexts()
                .filter(|child| child.parent() == Some(context))
                .map(section),
        );
        if context == KeybindContext::Editing {
            sections.push(Section {
                label: INPUT_PREFIXES_LABEL,
                rows: INPUT_PREFIXES
                    .iter()
                    .map(|&(prefix, description)| (KeyLabel::Single(prefix), description))
                    .collect(),
            });
        }
    }
    sections.retain(|section| !section.rows.is_empty());
    sections
}

/// The query's words, lowercased the way every haystack is.
fn terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Whether every term is somewhere in the row's section, keys or description,
/// so a section's name finds all of its rows.
fn holds_every(
    section: &'static str,
    (label, description): (KeyLabel, &'static str),
    terms: &[String],
) -> bool {
    if terms.is_empty() {
        return true;
    }
    let mut haystack = format!("{section} {description}");
    for part in label.parts() {
        for word in [part, &*with_sigils(part)] {
            haystack.push(' ');
            haystack.push_str(word);
        }
    }
    haystack.make_ascii_lowercase();
    terms.iter().all(|term| haystack.contains(term.as_str()))
}

/// A chord as the reference draws it, each spelled modifier traded for its
/// symbol. The spelling stays searchable: [`holds_every`] reads both.
fn with_sigils(part: &'static str) -> Cow<'static, str> {
    MODIFIER_SIGILS
        .iter()
        .fold(Cow::Borrowed(part), |part, (spelled, sigil)| {
            match part.contains(spelled) {
                true => Cow::Owned(part.replace(spelled, sigil)),
                false => part,
            }
        })
}

fn shown_parts(label: KeyLabel) -> Vec<Cow<'static, str>> {
    label.parts().map(with_sigils).collect()
}

fn joined_width(parts: &[Cow<'static, str>]) -> usize {
    parts.iter().map(|part| part.width()).sum::<usize>()
        + ALT_SEP.width() * parts.len().saturating_sub(1)
}

/// The characters of `text` any term covers. Lowercasing ASCII keeps every
/// byte where it was, so a match in the lowered text marks the original.
fn term_indices(text: &str, terms: &[String]) -> Vec<u32> {
    let lowered = text.to_ascii_lowercase();
    let ranges: Vec<Range<usize>> = terms
        .iter()
        .flat_map(|term| {
            lowered
                .match_indices(term.as_str())
                .map(|(start, _)| start..start + term.len())
        })
        .collect();
    text.char_indices()
        .enumerate()
        .filter(|(_, (byte, _))| ranges.iter().any(|range| range.contains(byte)))
        .filter_map(|(index, _)| u32::try_from(index).ok())
        .collect()
}

fn marked(text: &str, terms: &[String], base: Style, theme: &Theme) -> Vec<Span<'static>> {
    match_spans(
        text,
        &term_indices(text, terms),
        base,
        base.patch(theme.item_match),
        None,
    )
}

/// A section's key column: its widest label, over every row so the column
/// holds while the filter narrows the section, and never past
/// [`KEY_COL_MAX`].
fn key_column(section: &Section) -> usize {
    section
        .rows
        .iter()
        .map(|(label, _)| joined_width(&shown_parts(*label)))
        .max()
        .unwrap_or(0)
        .min(KEY_COL_MAX)
}

/// A label's keys packed into lines that fit `column`, leaving room on each
/// for the separator that says it goes on. A key wider than `column` takes a
/// line to itself.
fn wrapped<'a>(parts: &'a [Cow<'static, str>], column: usize) -> Vec<Vec<&'a str>> {
    if joined_width(parts) <= column {
        return vec![parts.iter().map(AsRef::as_ref).collect()];
    }
    let joint = ALT_SEP.width();
    let mut lines: Vec<Vec<&str>> = Vec::new();
    let mut used = 0;
    for part in parts.iter().map(AsRef::as_ref) {
        match lines.last_mut() {
            Some(line) if used + joint + part.width() + CONTINUED.width() <= column => {
                line.push(part);
                used += joint + part.width();
            }
            _ => {
                lines.push(vec![part]);
                used = part.width();
            }
        }
    }
    lines
}

/// A label's lines in the key column. Every line but the last ends in
/// [`CONTINUED`], and the first is padded out to where its description starts.
fn key_lines(
    label: KeyLabel,
    column: usize,
    terms: &[String],
    theme: &Theme,
) -> Vec<Line<'static>> {
    let parts = shown_parts(label);
    let lines = wrapped(&parts, column);
    let last = lines.len().saturating_sub(1);
    lines
        .into_iter()
        .enumerate()
        .map(|(index, parts)| {
            let mut spans = vec![Span::styled(PREFIX, theme.keybind_key)];
            for (position, part) in parts.into_iter().enumerate() {
                if position > 0 {
                    spans.push(Span::styled(ALT_SEP, theme.keybind_desc));
                }
                spans.extend(marked(part, terms, theme.keybind_key, theme));
            }
            if index < last {
                spans.push(Span::styled(CONTINUED, theme.keybind_desc));
            }
            let mut line = Line::from(spans);
            if index == 0 {
                let pad = column.saturating_sub(line.width() - PREFIX.width()) + KEY_COL_GAP;
                line.spans
                    .push(Span::styled(" ".repeat(pad), theme.keybind_key));
            }
            line
        })
        .collect()
}

/// The rows holding every term, under their section headings, with what the
/// terms matched marked.
fn reference(sections: &[Section], terms: &[String], theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for section in sections {
        let column = key_column(section);
        let rows: Vec<_> = section
            .rows
            .iter()
            .copied()
            .filter(|&row| holds_every(section.label, row, terms))
            .collect();
        if rows.is_empty() {
            continue;
        }
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        let mut heading = vec![Span::styled(PREFIX, theme.keybind_section)];
        heading.extend(marked(section.label, terms, theme.keybind_section, theme));
        lines.push(Line::from(heading));
        for (label, description) in rows {
            let mut key = key_lines(label, column, terms, theme);
            if let Some(first) = key.first_mut() {
                first
                    .spans
                    .extend(marked(description, terms, theme.keybind_desc, theme));
            }
            lines.extend(key);
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("{PREFIX}{NO_MATCHES}"),
            theme.tool_dim,
        )));
    }
    lines
}

/// The keys a focused query leaves to the reference, so a filtered list can
/// still be read past its first screen without leaving the field.
fn scrolls_reference(key_event: KeyEvent) -> bool {
    matches!(key_event.code, KeyCode::Up | KeyCode::Down)
        || key::PAGE_UP.matches(key_event)
        || key::PAGE_DOWN.matches(key_event)
        || key::SCROLL_HALF_UP.matches(key_event)
}

impl HelpModal {
    pub fn new() -> Self {
        Self {
            open: false,
            scroll: ModalScroll::new_top(),
            scrollbar: Scrollbar::default(),
            pan_bar: Scrollbar::horizontal(),
            footer: FooterHits::default(),
            popup: Rect::default(),
            query: TextField::new(FieldKind::Line),
            query_focused: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.open && self.popup.contains(pos)
    }

    pub(crate) fn text_input_active(&self) -> bool {
        self.open && self.query_focused
    }

    pub fn toggle(&mut self) {
        let open = !self.open;
        self.close();
        self.open = open;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.scroll.reset();
        self.footer.reset();
        self.query.clear();
        self.query_focused = false;
    }

    #[cfg(test)]
    pub(crate) fn footer_hit(&self, target: HelpTarget) -> Rect {
        self.footer_targets()
            .iter()
            .position(|candidate| *candidate == target)
            .map(|index| self.footer.hit(index))
            .unwrap_or_default()
    }

    /// The footer is asked first: on a touch screen the pan bar's margin
    /// reaches up over it, and a press on a control is never the start of a
    /// sweep. The two bars sit on different rows, so at most one of them
    /// answers a press.
    pub(crate) fn handle_mouse(&mut self, event: &MouseEvent) -> HelpMouse {
        if !self.open {
            return HelpMouse::Ignored;
        }
        if let Some(index) = self.footer.handle_mouse(*event) {
            let target = self.footer_targets().get(index).copied();
            self.footer.clear();
            match target {
                Some(HelpTarget::Command(cmdline)) => return HelpMouse::Command(cmdline),
                Some(HelpTarget::Search) => self.query_focused = true,
                Some(HelpTarget::Close) => self.close(),
                Some(HelpTarget::Clear) => self.clear_query(),
                Some(HelpTarget::Keep) => self.query_focused = false,
                None => {}
            }
            return HelpMouse::Consumed;
        }
        if event.kind == MouseEventKind::Down(MouseButton::Left) && self.footer.hovered().is_some()
        {
            return HelpMouse::Consumed;
        }
        match self.scrollbar.handle(event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return HelpMouse::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                return HelpMouse::Consumed;
            }
        }
        match self.pan_bar.handle(event) {
            ScrollbarMouse::Ignored => HelpMouse::Ignored,
            ScrollbarMouse::Consumed => HelpMouse::Consumed,
            ScrollbarMouse::ScrollTo(column) => {
                self.scroll.pan_to(column as u16);
                HelpMouse::Consumed
            }
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    /// A sideways wheel over the modal, which the app routes here rather than
    /// dropping now that the content can run off the edge.
    pub fn pan(&mut self, delta: i32) {
        self.scroll.pan_by(delta);
    }

    pub fn handle_paste(&mut self, text: &str) {
        self.handle_text(text, true);
    }

    pub fn handle_text(&mut self, text: &str, paste: bool) {
        let text = text.replace(['\r', '\n'], " ");
        if self.query_focused && self.query.handle_text(&text, paste).changed() {
            self.refilter();
        }
    }

    /// Returns text for the clipboard: a selection copied or cut out of the
    /// query.
    pub fn handle_key(&mut self, key_event: KeyEvent) -> Option<String> {
        if key::HELP.matches(key_event) {
            self.close();
            return None;
        }
        if self.query_focused {
            return self.query_key(key_event);
        }
        match key_event.code {
            KeyCode::Esc if !self.query.is_empty() => self.clear_query(),
            KeyCode::Esc => self.close(),
            _ if key::QUIT.matches(key_event) => self.close(),
            _ if plain_char(&key_event) == Some(SEARCH_KEY) => self.query_focused = true,
            _ => {
                self.scroll.handle_key(key_event);
            }
        }
        None
    }

    fn query_key(&mut self, key_event: KeyEvent) -> Option<String> {
        match key_event.code {
            KeyCode::Esc => self.clear_query(),
            KeyCode::Enter => self.query_focused = false,
            _ if scrolls_reference(key_event) => {
                self.scroll.handle_key(key_event);
            }
            _ => {
                let edit = self.query.handle_key(key_event);
                if edit.changed() {
                    self.refilter();
                }
                match edit {
                    TextKey::Copy(text) | TextKey::Cut(text) => return Some(text),
                    TextKey::Ignored if key::QUIT.matches(key_event) => self.close(),
                    _ => {}
                }
            }
        }
        None
    }

    fn clear_query(&mut self) {
        self.query.clear();
        self.query_focused = false;
        self.refilter();
    }

    fn refilter(&mut self) {
        self.scroll.scroll_to(0);
    }

    fn footer_targets(&self) -> &'static [HelpTarget] {
        match (self.query_focused, self.query.is_empty()) {
            (true, _) => &QUERY_TARGETS,
            (false, true) => &BROWSE_TARGETS,
            (false, false) => &FILTERED_TARGETS,
        }
    }

    /// Pinned under the reference rather than ending it, so it never pans away
    /// with a description or waits at the bottom of a long scroll. Glossed
    /// while that fits, and keys alone once it does not.
    fn footer_line(&self, width: u16, theme: &Theme) -> FooterLine {
        let glossed = self.footer_rung(true, theme);
        match glossed.fits(width) {
            true => glossed,
            false => self.footer_rung(false, theme),
        }
    }

    fn footer_rung(&self, glossed: bool, theme: &Theme) -> FooterLine {
        let mut footer = FooterLine::default();
        for (index, target) in self.footer_targets().iter().enumerate() {
            if index > 0 {
                footer.text(SEPARATOR, theme.tool_dim);
            }
            let (label, gloss) = target.words();
            footer.command(label, theme.keybind_key);
            if glossed {
                footer.describe(gloss, theme.tool_dim);
            }
        }
        footer
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect, features: FeatureFlags) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("help_modal", area);
        let theme = theme::current();

        let lines = reference(&sections(features), &terms(&self.query.text()), &theme);

        let modal = Modal {
            title: TITLE,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        // As tall as it may be whatever the filter leaves, so narrowing the
        // list never moves the footer out from under the pointer.
        let (popup, inner) = modal.render(frame, area, area.height);
        let searching = self.query_focused || !self.query.is_empty();
        let [query_area, body, footer_area] = Layout::vertical([
            Constraint::Length(if searching { QUERY_ROWS } else { 0 }),
            Constraint::Fill(1),
            Constraint::Length(FOOTER_ROWS),
        ])
        .areas(inner);

        if searching {
            let chevron = chevron_span();
            let width = usize::from(query_area.width).saturating_sub(chevron.width());
            let styles = field_styles(input_text_style());
            let mut line = self
                .query
                .paint(width, &styles, self.query_focused, QUERY_PLACEHOLDER);
            line.spans.insert(0, chevron);
            frame.render_widget(Paragraph::new(line), query_area);
        }

        let total = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        let content_w = lines
            .iter()
            .map(Line::width)
            .max()
            .and_then(|width| u16::try_from(width).ok())
            .unwrap_or(u16::MAX);
        self.scroll.update_dimensions(total, body.height);
        self.scroll.fit_width(content_w, body.width);
        let scroll = self.scroll.offset();
        let pan = self.scroll.pan();

        frame.render_widget(Paragraph::new(lines).scroll((scroll, pan)), body);

        self.scrollbar.draw(frame, body, total, scroll);
        // Handed the reference, the footer and the bottom border: the bar paints
        // on the last row, which is the only row it can have without taking one
        // from the reference, and a fingertip's hit margin comes out of the rows
        // over it. Nothing is painted while the rows fit, because a track is only
        // built for content that overflows.
        self.pan_bar.draw(frame, bar_area(inner), content_w, pan);

        let footer = self.footer_line(footer_area.width, &theme);
        self.footer.set(footer.hits(footer_area, 0, FOOTER_ROWS));
        frame.render_widget(
            Paragraph::new(footer.line(self.footer.hovered())),
            footer_area,
        );

        self.popup = popup;
        popup
    }
}

impl Overlay for HelpModal {
    fn docs_topic(&self) -> Option<DocsTopic> {
        Some(DocsTopic::Keybindings)
    }

    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::keybindings::LEADER_PREFIX;
    use crate::components::{buffer_text, key as key_ev};
    use caudra_workbench::scroll::SCROLLBAR_THUMB_HORIZONTAL;
    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Modifier;
    use test_case::test_case;

    /// A small screen: the modal fills its floor and the reference still runs
    /// past it.
    const NARROW_TERMINAL: u16 = 80;
    /// Wide enough that every row fits inside the modal.
    const WIDE_TERMINAL: u16 = 300;
    const TERMINAL_HEIGHT: u16 = 40;
    /// Finds the sandbox manager's save row, whose description alone is as
    /// wide as a narrow modal, so its end is off the edge whatever the key
    /// column.
    const LONG_ROW_QUERY: &str = "export preview";
    const CUT_DESCRIPTION: &str = "as a new file";
    /// Enough steps to bring a row's end into view past the widest key column.
    const PANS_PAST_KEY_COLUMN: usize = 3;
    const NESTED_HEADING: &str = "    ";
    const MODEL_PICKER: &str = "Model Picker";
    const WORKBENCH_EDITOR: &str = "Workbench Editor";
    const GENERAL: &str = "General";
    const MODEL_QUERY: &str = "model picker";
    const UNMATCHED_QUERY: &str = "zzqx";
    const TYPED: &str = "theme";
    const DESCRIPTION_UNREACHABLE: &str = "panning must bring the description into view";
    const BAR_UNWANTED: &str = "a reference that fits must not wear a pan bar";
    const BAR_MISSING: &str = "a row running off the edge must show what reaches it";
    const CONTROL_MISSING: &str = "the footer must draw the control where a pointer reaches it";
    const HOVER_MISSED: &str = "the whole phrase, and only it, must reverse under the pointer";
    const PRESS_LEAKED: &str = "a press on a control is the modal's, not the start of a sweep";
    const HEADING_NESTED: &str = "every heading and row must start at the same column";
    const HEADING_MISSING: &str = "a child context must still get its own heading";
    const MATCH_MISSING: &str = "a row holding every term must stay";
    const MISMATCH_KEPT: &str = "a section with no row holding every term must go";
    const FILTERED_UNASKED: &str = "typing into the reference must not filter it";
    const FIELD_UNFOCUSED: &str = "the query must take the keys it was opened for";
    const FILTER_LOST: &str = "the filter must outlast the field losing focus";

    /// Key and gloss mark as one phrase, and a click answers with what the
    /// control names: the host runs `/docs`, `/` opens the query, and `Esc`
    /// closes the modal here.
    #[test_case(HelpTarget::Command(DOCS_COMMAND), HelpMouse::Command(DOCS_COMMAND), true, false ; "docs_is_left_to_the_host")]
    #[test_case(HelpTarget::Search, HelpMouse::Consumed, true, true ; "search_focuses_the_query")]
    #[test_case(HelpTarget::Close, HelpMouse::Consumed, false, false ; "close_closes")]
    fn a_footer_control_hovers_as_a_phrase_and_answers_a_click(
        target: HelpTarget,
        answer: HelpMouse,
        open_after: bool,
        typing_after: bool,
    ) {
        let mut modal = HelpModal::new();
        modal.toggle();
        draw(&mut modal, NARROW_TERMINAL);
        let hit = modal.footer_hit(target);
        assert!(!hit.is_empty(), "{CONTROL_MISSING}");

        modal.handle_mouse(&mouse(MouseEventKind::Moved, hit));
        let buffer = draw(&mut modal, NARROW_TERMINAL);
        let reversed: Vec<Position> = buffer
            .area
            .positions()
            .filter(|at| buffer[*at].modifier.contains(Modifier::REVERSED))
            .collect();
        assert!(
            reversed.len() == usize::from(hit.width) && reversed.iter().all(|at| hit.contains(*at)),
            "{HOVER_MISSED}: hit={hit:?} reversed={reversed:?}"
        );

        let press = modal.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), hit));
        assert_eq!(press, HelpMouse::Consumed, "{PRESS_LEAKED}");
        let release = modal.handle_mouse(&mouse(MouseEventKind::Up(MouseButton::Left), hit));
        assert_eq!(release, answer);
        assert_eq!(modal.is_open(), open_after);
        assert_eq!(modal.text_input_active(), typing_after);
    }

    #[test_case(key_ev(KeyCode::Esc)       ; "esc_closes")]
    #[test_case(key::QUIT.to_key_event()    ; "ctrl_c_closes")]
    #[test_case(key::HELP.to_key_event()    ; "f1_closes")]
    fn handle_key_closes(k: KeyEvent) {
        let mut modal = HelpModal::new();
        modal.toggle();
        modal.handle_key(k);
        assert!(!modal.is_open());
    }

    #[test_case(key::QUIT.to_key_event() ; "ctrl_c_closes")]
    #[test_case(key::HELP.to_key_event() ; "f1_closes")]
    fn an_empty_selection_leaves_the_close_keys_to_the_modal_while_typing(k: KeyEvent) {
        let mut modal = searching(TYPED);
        assert_eq!(modal.handle_key(k), None);
        assert!(!modal.is_open());
    }

    #[test]
    fn typing_without_slash_never_filters() {
        let mut modal = HelpModal::new();
        modal.toggle();
        for character in TYPED.chars() {
            modal.handle_key(key_ev(KeyCode::Char(character)));
        }
        modal.handle_text(TYPED, false);
        assert!(modal.is_open());
        assert!(!modal.text_input_active(), "{FILTERED_UNASKED}");
        assert!(modal.query.is_empty(), "{FILTERED_UNASKED}");
    }

    /// One `Esc` drops the filter, from the field or from the list, and only
    /// the next one closes the modal.
    #[test_case(true  ; "from_the_field")]
    #[test_case(false ; "from_the_list")]
    fn esc_clears_the_filter_before_it_closes(focused: bool) {
        let mut modal = searching(TYPED);
        if !focused {
            modal.handle_key(key_ev(KeyCode::Enter));
        }
        modal.handle_key(key_ev(KeyCode::Esc));
        assert!(modal.is_open());
        assert!(modal.query.is_empty());
        assert!(!modal.text_input_active());

        modal.handle_key(key_ev(KeyCode::Esc));
        assert!(!modal.is_open());
    }

    #[test]
    fn enter_keeps_the_filter_and_returns_keys_to_the_list() {
        let mut modal = searching(TYPED);
        modal.handle_key(key_ev(KeyCode::Enter));
        assert!(!modal.text_input_active());
        modal.handle_key(key_ev(KeyCode::Char('j')));
        assert_eq!(modal.query.text(), TYPED, "{FILTER_LOST}");

        modal.handle_key(key_ev(KeyCode::Char(SEARCH_KEY)));
        assert!(modal.text_input_active(), "{FIELD_UNFOCUSED}");
    }

    #[test_case(true  ; "focused")]
    #[test_case(false ; "browsing")]
    fn a_paste_reaches_the_query_only_while_it_has_focus(focused: bool) {
        let mut modal = HelpModal::new();
        modal.toggle();
        if focused {
            modal.handle_key(key_ev(KeyCode::Char(SEARCH_KEY)));
        }
        modal.handle_paste(TYPED);
        assert_eq!(modal.query.text(), if focused { TYPED } else { "" });
    }

    /// The headings that used to sit under `Pickers` and `Workbench` start
    /// where every other heading does, and so do their rows.
    #[test]
    fn every_heading_starts_at_one_column() {
        let lines = reference_text(&[]);
        for heading in [MODEL_PICKER, WORKBENCH_EDITOR] {
            assert!(
                lines.contains(&format!("{PREFIX}{heading}")),
                "{HEADING_MISSING}: {heading}"
            );
        }
        let nested: Vec<&String> = lines
            .iter()
            .filter(|line| line.starts_with(NESTED_HEADING))
            .collect();
        assert!(nested.is_empty(), "{HEADING_NESTED}: {nested:?}");
    }

    /// A label wider than the column wraps between its keys and says so, one
    /// that fits stays on one line, and every modifier shows as its symbol.
    #[test_case(KeyLabel::Multi(&["Ctrl+Home", "Ctrl+End", "PageUp", "PageDown"]), &["⌃Home / ⌃End /", "PageUp / PageDown"] ; "a_long_label_wraps_between_its_keys")]
    #[test_case(KeyLabel::Multi(&["Shift+↑", "Shift+↓", "Shift+←", "Shift+→"]), &["⇧↑ / ⇧↓ / ⇧← / ⇧→"] ; "symbols_keep_a_label_whole")]
    #[test_case(KeyLabel::Single("Ctrl+Alt+Shift+Super+Z"), &["⌃⌥⇧⌘Z"] ; "every_modifier_becomes_its_symbol")]
    fn a_label_wraps_to_its_key_column(label: KeyLabel, expected: &[&str]) {
        let lines: Vec<String> = key_lines(label, KEY_COL_MAX, &[], &theme::current())
            .iter()
            .map(|line| line.to_string().trim().to_owned())
            .collect();
        assert_eq!(lines, expected);
    }

    #[test_case(MODEL_QUERY,     MODEL_PICKER,  WORKBENCH_EDITOR     ; "a_section_name_keeps_its_rows")]
    #[test_case(LEADER_PREFIX,   LEADER_PREFIX, INPUT_PREFIXES_LABEL ; "a_key_keeps_the_rows_it_names")]
    #[test_case(UNMATCHED_QUERY, NO_MATCHES,    GENERAL              ; "nothing_matching_says_so")]
    fn a_query_keeps_only_rows_holding_every_term(query: &str, kept: &'static str, dropped: &str) {
        let lines = reference_text(&terms(query));
        let kept = with_sigils(kept);
        let kept = kept.as_ref();
        assert!(
            lines.iter().any(|line| line.contains(kept)),
            "{MATCH_MISSING}: {kept}"
        );
        assert!(
            !lines.iter().any(|line| line.trim() == dropped),
            "{MISMATCH_KEPT}: {dropped}"
        );
    }

    /// A description is as long as it is, so on a small screen the half of the
    /// reference that says what a key does is off the edge.
    #[test]
    fn a_description_cut_by_a_narrow_screen_is_reachable_by_panning() {
        let mut modal = searching(LONG_ROW_QUERY);
        modal.handle_key(key_ev(KeyCode::Enter));

        let clipped = render_at(&mut modal, NARROW_TERMINAL);
        assert!(
            !clipped.contains(CUT_DESCRIPTION),
            "the description should start off the edge"
        );

        for _ in 0..PANS_PAST_KEY_COLUMN {
            modal.handle_key(key::PAN_RIGHT.to_key_event());
        }

        assert!(
            render_at(&mut modal, NARROW_TERMINAL).contains(CUT_DESCRIPTION),
            "{DESCRIPTION_UNREACHABLE}"
        );
    }

    /// Enough rows fit that the widest one always runs past a narrow modal, and
    /// never past a wide one.
    #[test]
    fn the_pan_bar_shows_only_while_a_row_runs_past_the_modal() {
        let mut modal = HelpModal::new();
        modal.toggle();

        assert!(
            render_at(&mut modal, NARROW_TERMINAL).contains(SCROLLBAR_THUMB_HORIZONTAL),
            "{BAR_MISSING}"
        );
        assert!(
            !render_at(&mut modal, WIDE_TERMINAL).contains(SCROLLBAR_THUMB_HORIZONTAL),
            "{BAR_UNWANTED}"
        );
    }

    #[test]
    fn the_query_shows_above_the_rows_it_filters() {
        let mut modal = searching(MODEL_QUERY);
        let screen = render_at(&mut modal, WIDE_TERMINAL);
        assert!(screen.contains(MODEL_QUERY), "{FIELD_UNFOCUSED}");
        assert!(screen.contains(MODEL_PICKER), "{MATCH_MISSING}");
    }

    fn searching(query: &str) -> HelpModal {
        let mut modal = HelpModal::new();
        modal.toggle();
        modal.handle_key(key_ev(KeyCode::Char(SEARCH_KEY)));
        modal.handle_text(query, false);
        modal
    }

    fn reference_text(terms: &[String]) -> Vec<String> {
        let theme = theme::current();
        reference(&sections(FeatureFlags::all()), terms, &theme)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    fn mouse(kind: MouseEventKind, at: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: at.x,
            row: at.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn draw(modal: &mut HelpModal, width: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, TERMINAL_HEIGHT)).unwrap();
        terminal
            .draw(|f| {
                modal.view(f, f.area(), FeatureFlags::all());
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn render_at(modal: &mut HelpModal, width: u16) -> String {
        buffer_text(&draw(modal, width))
    }
}
