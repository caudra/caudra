//! Drawing primitives every pane shares.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget, Widget};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const ELLIPSIS: char = '…';
pub const VERTICAL: &str = "│";
pub const SCROLLBAR_THUMB: &str = "▐";

/// Truncates on display width rather than bytes, so a CJK path or an emoji in a
/// filename cannot overflow the pane it is drawn into.
pub fn fit(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(text.len());
    let mut used = 0;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push(ELLIPSIS);
    out
}

/// Truncates from the left, keeping the tail. Paths are more recognisable by
/// their filename than by the repository root they all share.
pub fn fit_end(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut tail: Vec<char> = Vec::new();
    let mut used = 1;
    for ch in text.chars().rev() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width {
            break;
        }
        tail.push(ch);
        used += w;
    }
    let mut out = String::from(ELLIPSIS);
    out.extend(tail.iter().rev());
    out
}

pub fn fill(buf: &mut Buffer, area: Rect, style: Style) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_symbol(" ");
                cell.set_style(style);
            }
        }
    }
}

pub fn vertical_rule(buf: &mut Buffer, area: Rect, style: Style) {
    for y in area.top()..area.bottom() {
        if let Some(cell) = buf.cell_mut((area.x, y)) {
            cell.set_symbol(VERTICAL);
            cell.set_style(style);
        }
    }
}

/// The thumb alone, with no track, no arrows, and no room of its own: callers
/// reserve the column and only call this when the content overflows it.
///
/// `caudra-ui` draws the same bar over a `Frame` for the transcript and its
/// modals. The workbench paints into a `Buffer`, and cannot depend on the crate
/// that owns it, so this is a deliberate second copy rather than a missed reuse.
pub fn vertical_scrollbar(buf: &mut Buffer, area: Rect, total: usize, at: usize, style: Style) {
    let scrolled = total.saturating_sub(area.height as usize);
    let mut state = ScrollbarState::default()
        .content_length(scrolled + 1)
        .position(at);
    Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_symbol(SCROLLBAR_THUMB)
        .thumb_style(style)
        .track_symbol(None)
        .begin_symbol(None)
        .end_symbol(None)
        .render(area, buf, &mut state);
}

pub fn render_line(buf: &mut Buffer, area: Rect, line: Line<'_>) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let row = Rect { height: 1, ..area };
    line.render(row, buf);
}

/// Lays a left group against a right group on one row, dropping the right group
/// when the two would collide rather than letting it wrap.
pub fn status_line<'a>(
    left: Vec<Span<'a>>,
    right: Vec<Span<'a>>,
    width: u16,
    style: Style,
) -> Line<'a> {
    let left_width: usize = left
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let right_width: usize = right
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let mut spans = left;
    if left_width + right_width < width as usize {
        spans.push(Span::styled(
            " ".repeat(width as usize - left_width - right_width),
            style,
        ));
        spans.extend(right);
    }
    Line::from(spans).style(style)
}

#[cfg(test)]
mod tests {
    use super::{ELLIPSIS, fit, fit_end};
    use test_case::test_case;
    use unicode_width::UnicodeWidthStr;

    const WITHIN_BUDGET: &str = "a fitted string must never exceed the width it was given";
    const UNTOUCHED: &str = "a string that already fits must come back unchanged";
    const KEEPS_TAIL: &str = "fit_end must keep the end of the string, which names the file";

    #[test_case("short", 10 ; "shorter_than_width")]
    #[test_case("exactly-ten", 11 ; "equal_to_width")]
    fn a_string_that_fits_is_returned_whole(text: &str, width: usize) {
        assert_eq!(fit(text, width), text, "{UNTOUCHED}");
    }

    #[test_case("abcdefghij", 5 ; "ascii")]
    #[test_case("日本語のファイル名", 6 ; "wide_glyphs")]
    #[test_case("mixed日本語text", 7 ; "mixed_widths")]
    fn an_overlong_string_is_cut_to_width(text: &str, width: usize) {
        let cut = fit(text, width);
        assert!(
            UnicodeWidthStr::width(cut.as_str()) <= width,
            "{WITHIN_BUDGET}"
        );
        assert!(cut.ends_with(ELLIPSIS), "a cut string must say it was cut");
    }

    #[test]
    fn a_zero_width_budget_yields_nothing() {
        assert!(fit("anything", 0).is_empty(), "{WITHIN_BUDGET}");
        assert!(fit_end("anything", 0).is_empty(), "{WITHIN_BUDGET}");
    }

    #[test]
    fn fit_end_keeps_the_filename() {
        let cut = fit_end("caudra-ui/src/components/keybindings.rs", 20);
        assert!(
            UnicodeWidthStr::width(cut.as_str()) <= 20,
            "{WITHIN_BUDGET}"
        );
        assert!(cut.ends_with("keybindings.rs"), "{KEEPS_TAIL}");
        assert!(
            cut.starts_with(ELLIPSIS),
            "a cut string must say it was cut"
        );
    }
}
