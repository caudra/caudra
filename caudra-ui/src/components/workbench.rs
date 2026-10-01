//! The seam between Caudra's theme and overlay lifecycle and the workbench.
//!
//! `caudra-workbench` cannot depend on this crate without a cycle, so the
//! translation of theme roles into the palette it paints with lives here, and
//! so does the Markdown painter its rendered view borrows.

use caudra_workbench::{PaintedMarkdown, Workbench, WorkbenchStyles};
use ratatui::style::{Modifier, Style};

use crate::components::Overlay;
use crate::markdown::text_to_rows;
use crate::provenance::Provenance;
use crate::repaint::Cadence;
use crate::theme;

pub(crate) fn styles() -> WorkbenchStyles {
    let t = theme::current();
    WorkbenchStyles {
        background: t.surface_style(),
        text: Style::new().fg(t.foreground),
        dim: t.status_dim,
        border: t.panel_border,
        title: t.panel_title,
        selected: t.item_selected,
        hover: Style::new().add_modifier(Modifier::REVERSED),
        accent: t.accent,
        directory: t.tool_path,
        tab_active: t.active,
        tab_inactive: t.item_desc,
        gutter: t.code_gutter,
        cursor: t.cursor,
        selection: Style::new().add_modifier(Modifier::REVERSED),
        error: t.error,
        git_modified: t.todo_in_progress,
        git_added: t.diff_new,
        git_deleted: t.diff_old,
        git_untracked: t.diff_new,
        git_conflicted: t.error,
        diff_old: t.diff_old,
        diff_new: t.diff_new,
        diff_old_emphasis: t.diff_old_emphasis,
        diff_new_emphasis: t.diff_new_emphasis,
        diff_line_nr: t.diff_line_nr,
        match_highlight: t.item_match,
        match_highlight_selected: t.item_match_selected,
        current_match: t.item_selected,
        agent_touched: t.accent,
    }
}

/// The transcript's own Markdown renderer, lent to the workbench so a Markdown
/// tab's rendered view reads the way an answer in the transcript does.
pub(crate) fn paint_markdown(text: &str, width: u16) -> PaintedMarkdown {
    let (painted, source) = text_to_rows(text, theme::current().assistant, width, Vec::new());
    let provenance = Provenance::new(source, painted.provenance);
    PaintedMarkdown::new(painted.lines, move |rows, start, end| {
        let last = rows.last()?;
        let last_chars = last
            .spans
            .iter()
            .map(|span| span.content.chars().count())
            .sum();
        if start == (0, 0) && end == (rows.len() - 1, last_chars) {
            Some(provenance.source().to_string())
        } else {
            provenance.extract_rows(rows, start, end)
        }
    })
}

impl Overlay for Workbench {
    fn is_open(&self) -> bool {
        Workbench::is_open(self)
    }

    fn close(&mut self) {
        Workbench::close(self);
    }

    fn cadence(&self) -> Cadence {
        Cadence::when(self.is_busy(), Cadence::PENDING)
    }
}

#[cfg(test)]
mod tests {
    use super::{PaintedMarkdown, Workbench, paint_markdown, styles};
    use caudra_markdown::render::{CODE_BAR, TOOL_OUTPUT_MAX_LINE_BYTES};
    use caudra_workbench::{DocumentKey, TabLabel, WorkbenchAction, keys};
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::{Terminal, backend::TestBackend, style::Modifier};
    use test_case::test_case;

    const WIDTH: u16 = 80;
    const HEIGHT: u16 = 24;
    const NARROW: u16 = 16;
    const HEADING: &str = "## Heading";
    const PARAGRAPH: &str = "Some **bold** words and a [link](./target) that wrap.";
    const FENCE: &str = "```rust\nlet x = 1;\n```";
    const TABLE: &str = "| left | right |\n| --- | --- |\n| alpha | beta |";
    const DIAGRAM: &str = "```mermaid\nflowchart LR\n    Start --> Finish\n```";
    const LIST: &str = "- first\n- **second**";
    const UNICODE: &str = "A **café界e\u{301}lan** word.";
    const EXACT_SOURCE: &str =
        "\r\n\r\n# Heading  \r\n\r\n[link][target]\r\n\r\n[target]: ./target  \r\n\r\n \t\r\n";
    const PARTIAL_PAINTED: &str = "Some bold wo";
    const PARTIAL_SOURCE: &str = "Some **bold** wo";
    const FIRST_WORD: &str = "Some";
    const WRAPPED_START: &str = "words";
    const WRAPPED_END: &str = "wrap.";
    const WRAPPED_PARTIAL: &str = "words and a [link](./target) that wrap";
    const LONG_WORD: &str = "word ";
    const LONG_TAIL: &str = "beyond_cap";
    const HEAD: &str = "Before.";
    const TAIL: &str = "After.";
    const UNSAVED: &str = "Unsaved **edit**";
    const NO_COPY: &str = "the rendered selection did not copy its source";
    const NO_ROW: &str = "the rendered fixture did not contain the selected text";
    const NO_FRAME: &str = "the workbench did not render";
    const COPIED_CHROME: &str = "empty selections and decorative cells must not copy screen text";
    const NO_HIGHLIGHT: &str = "the rendered selection was not highlighted";
    const NO_EDIT: &str = "the source document did not accept the unsaved edit";
    const DOCUMENT_KEY: &str = "markdown-selection";
    const DOCUMENT_TITLE: &str = "Markdown selection";

    fn last(painted: &PaintedMarkdown) -> (usize, usize) {
        let row = painted.lines().len() - 1;
        (row, painted.lines()[row].to_string().chars().count())
    }

    fn ends(painted: &PaintedMarkdown, needle: &str) -> ((usize, usize), (usize, usize)) {
        painted
            .lines()
            .iter()
            .enumerate()
            .find_map(|(row, line)| {
                let text = line.to_string();
                let byte = text.find(needle)?;
                let start = text[..byte].chars().count();
                Some(((row, start), (row, start + needle.chars().count())))
            })
            .expect(NO_ROW)
    }

    #[test_case(HEADING, WIDTH ; "heading")]
    #[test_case(PARAGRAPH, NARROW ; "wrapped paragraph")]
    #[test_case(FENCE, NARROW ; "code fence")]
    #[test_case(TABLE, WIDTH ; "table")]
    #[test_case(DIAGRAM, WIDTH ; "diagram")]
    #[test_case(LIST, NARROW ; "list")]
    #[test_case(UNICODE, NARROW ; "unicode")]
    #[test_case(EXACT_SOURCE, NARROW ; "whitespace crlf and reference definition")]
    fn a_whole_document_copies_the_original_source(source: &str, width: u16) {
        let painted = paint_markdown(source, width);
        assert_eq!(
            painted.selected_text((0, 0), last(&painted)).as_deref(),
            Some(source),
            "{NO_COPY}"
        );
    }

    #[test_case(HEADING, WIDTH ; "heading")]
    #[test_case(PARAGRAPH, NARROW ; "wrapped paragraph")]
    #[test_case(FENCE, NARROW ; "code fence")]
    #[test_case(TABLE, WIDTH ; "table")]
    #[test_case(TABLE, NARROW ; "compact table")]
    #[test_case(DIAGRAM, WIDTH ; "diagram")]
    #[test_case(LIST, NARROW ; "list")]
    fn complete_blocks_inside_a_document_keep_their_syntax(block: &str, width: u16) {
        let painted = paint_markdown(&format!("{HEAD}\n\n{block}\n\n{TAIL}"), width);
        let after_head = ends(&painted, HEAD).0.0 + 1;
        let before_tail = ends(&painted, TAIL).0.0;
        let mut rows = (after_head..before_tail)
            .filter(|&row| !painted.lines()[row].to_string().trim().is_empty());
        let first = rows.next().expect(NO_ROW);
        let last = rows.next_back().unwrap_or(first);
        let start = (first, 0);
        let end = (last, painted.lines()[last].to_string().chars().count());

        assert_eq!(
            painted.selected_text(start, end).as_deref(),
            Some(block),
            "{NO_COPY}"
        );
        assert_eq!(
            painted.selected_text(end, start).as_deref(),
            Some(block),
            "{NO_COPY}"
        );
    }

    #[test_case(PARAGRAPH, "bol", "bol" ; "partial styled word")]
    #[test_case(PARAGRAPH, PARTIAL_PAINTED, PARTIAL_SOURCE ; "syntax between spans")]
    #[test_case(PARAGRAPH, "lin", "[link](./target)" ; "atomic link")]
    #[test_case(HEADING, "eadi", "eadi" ; "heading fragment")]
    #[test_case(LIST, "fir", "fir" ; "list item fragment")]
    #[test_case(LIST, "sec", "sec" ; "styled list item fragment")]
    #[test_case(FENCE, "x = 1", "x = 1" ; "fenced code fragment")]
    #[test_case(TABLE, "alp", "| alpha | beta |" ; "atomic table row")]
    #[test_case(DIAGRAM, "tar", DIAGRAM ; "atomic diagram")]
    #[test_case(UNICODE, "é界e\u{301}", "é界e\u{301}" ; "unicode scalar endpoints")]
    fn partial_selections_follow_transcript_provenance(source: &str, shown: &str, expected: &str) {
        let painted = paint_markdown(source, WIDTH);
        let (start, end) = ends(&painted, shown);
        assert_eq!(
            painted.selected_text(start, end).as_deref(),
            Some(expected),
            "{NO_COPY}"
        );
        assert_eq!(
            painted.selected_text(end, start).as_deref(),
            Some(expected),
            "{NO_COPY}"
        );
    }

    #[test_case(FENCE, CODE_BAR ; "code gutter")]
    #[test_case(TABLE, "╭" ; "table border")]
    fn decorative_cells_do_not_fall_back_to_screen_text(source: &str, shown: &str) {
        let painted = paint_markdown(source, WIDTH);
        let (start, end) = ends(&painted, shown);

        assert_eq!(painted.selected_text(start, start), None, "{COPIED_CHROME}");
        assert_eq!(painted.selected_text(start, end), None, "{COPIED_CHROME}");
    }

    #[test]
    fn a_wrapped_paragraph_is_copied_once_without_soft_wrap_newlines() {
        let source = format!("{HEADING}\n\n{PARAGRAPH}\n\n{TAIL}");
        let painted = paint_markdown(&source, NARROW);
        let (start, _) = ends(&painted, FIRST_WORD);
        let (_, end) = ends(&painted, WRAPPED_END);
        assert!(end.0 > start.0, "{NO_ROW}");
        assert_eq!(
            painted.selected_text(start, end).as_deref(),
            Some(PARAGRAPH),
            "{NO_COPY}"
        );
        let (_, first_word) = ends(&painted, FIRST_WORD);
        assert_eq!(
            painted.selected_text(start, first_word).as_deref(),
            Some(FIRST_WORD),
            "{NO_COPY}"
        );
        let (start, _) = ends(&painted, WRAPPED_START);
        let end = (end.0, end.1 - 1);
        assert_eq!(
            painted.selected_text(start, end).as_deref(),
            Some(WRAPPED_PARTIAL),
            "{NO_COPY}"
        );
        assert_eq!(
            painted.selected_text(end, start).as_deref(),
            Some(WRAPPED_PARTIAL),
            "{NO_COPY}"
        );
    }

    #[test]
    fn long_lines_are_not_copied_from_truncated_tool_output() {
        let source = format!(
            "{}{LONG_TAIL}  \r\n",
            LONG_WORD.repeat(TOOL_OUTPUT_MAX_LINE_BYTES)
        );
        let painted = paint_markdown(&source, WIDTH);
        assert_eq!(
            painted.selected_text((0, 0), last(&painted)).as_deref(),
            Some(source.as_str()),
            "{NO_COPY}"
        );
        let (start, end) = ends(&painted, LONG_TAIL);
        assert_eq!(
            painted.selected_text(start, end).as_deref(),
            Some(LONG_TAIL),
            "{NO_COPY}"
        );
    }

    fn document(text: &str) -> Workbench {
        let mut workbench = Workbench::new(styles());
        workbench.set_markdown_painter(paint_markdown);
        workbench.open_document(
            DocumentKey(DOCUMENT_KEY.to_owned()),
            TabLabel {
                title: DOCUMENT_TITLE.to_owned(),
                status: DOCUMENT_TITLE.to_owned(),
            },
            text,
        );
        workbench.handle_leader(keys::TOGGLE_RENDERED.to_key_event());
        workbench
    }

    fn mouse(kind: MouseEventKind, at: (u16, u16)) -> MouseEvent {
        MouseEvent {
            kind,
            column: at.0,
            row: at.1,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test_case(false ; "forward")]
    #[test_case(true ; "backward")]
    fn the_real_workbench_copies_raw_markdown_on_release(reverse: bool) {
        let mut workbench = document(PARAGRAPH);
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect(NO_FRAME);
        terminal
            .draw(|frame| workbench.view(frame, frame.area()))
            .expect(NO_FRAME);
        let surface = terminal.backend().buffer();
        let start = (0..HEIGHT)
            .find_map(|row| {
                let text: String = (0..WIDTH)
                    .map(|column| surface[(column, row)].symbol())
                    .collect();
                let byte = text.find(PARTIAL_PAINTED)?;
                Some((text[..byte].chars().count() as u16, row))
            })
            .expect(NO_ROW);
        let end = (start.0 + PARTIAL_PAINTED.len() as u16, start.1);
        let (anchor, moving) = if reverse { (end, start) } else { (start, end) };

        workbench.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), anchor));
        terminal
            .draw(|frame| workbench.view(frame, frame.area()))
            .expect(NO_FRAME);
        workbench.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), moving));
        terminal
            .draw(|frame| workbench.view(frame, frame.area()))
            .expect(NO_FRAME);
        let copied = workbench.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), moving));

        assert_eq!(
            copied,
            WorkbenchAction::Copy(PARTIAL_SOURCE.to_owned()),
            "{NO_COPY}"
        );
        terminal
            .draw(|frame| workbench.view(frame, frame.area()))
            .expect(NO_FRAME);
        for column in start.0..end.0 {
            assert!(
                terminal.backend().buffer()[(column, start.1)]
                    .modifier
                    .contains(Modifier::REVERSED),
                "{NO_HIGHLIGHT}"
            );
        }
        assert!(
            !terminal.backend().buffer()[end]
                .modifier
                .contains(Modifier::REVERSED),
            "{NO_HIGHLIGHT}"
        );
        assert_eq!(
            workbench.handle_key(keys::COPY.to_key_event()),
            copied,
            "{NO_COPY}"
        );
        workbench.handle_key(keys::SELECT_ALL.to_key_event());
        assert_eq!(
            workbench.handle_key(keys::COPY.to_key_event()),
            WorkbenchAction::Copy(PARAGRAPH.to_owned()),
            "{NO_COPY}"
        );
    }

    #[test_case(EXACT_SOURCE, false ; "crlf whitespace and reference definition")]
    #[test_case("\n\n", false ; "only blank lines")]
    #[test_case(EXACT_SOURCE, true ; "unsaved crlf document")]
    #[test_case(HEADING, true ; "unsaved document without a final newline")]
    fn select_all_copies_the_exact_current_document(source: &str, edit: bool) {
        let mut workbench = document(source);
        let expected = if edit {
            workbench.handle_leader(keys::TOGGLE_RENDERED.to_key_event());
            workbench.handle_key(keys::TEXT_START.to_key_event());
            assert!(workbench.paste(UNSAVED), "{NO_EDIT}");
            workbench.handle_leader(keys::TOGGLE_RENDERED.to_key_event());
            format!("{UNSAVED}{source}")
        } else {
            source.to_owned()
        };
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect(NO_FRAME);
        terminal
            .draw(|frame| workbench.view(frame, frame.area()))
            .expect(NO_FRAME);

        workbench.handle_key(keys::SELECT_ALL.to_key_event());

        assert_eq!(
            workbench.handle_key(keys::COPY.to_key_event()),
            WorkbenchAction::Copy(expected.clone()),
            "{NO_COPY}"
        );
        assert_eq!(
            workbench.handle_key(keys::SAVE.to_key_event()),
            WorkbenchAction::SaveDocument {
                key: DocumentKey(DOCUMENT_KEY.to_owned()),
                text: expected,
                close: false,
            },
            "{NO_COPY}"
        );
    }
}
