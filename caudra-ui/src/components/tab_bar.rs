use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::components::hover_style;
use crate::theme;

const SEPARATOR: &str = "│";
const DONE_MARK: &str = " ✓ ";
const REVIEW: &str = " Review ";
const PLACE_SEPARATOR: &str = " · ";

/// One page of a paged form: its label, whether it is done, and what
/// selecting its tab does.
pub(crate) struct Tab<T> {
    pub label: String,
    pub done: bool,
    pub target: T,
}

/// A tab per page and a closing Review tab, as spans paired with what each
/// selects, so drawing and hit testing read the same thing. Separators select
/// nothing. `active` is the page shown, or `None` while Review is.
///
/// When the bar is wider than `width`, it names only the active page and its
/// place, like `cargo clippy · 2 of 6`.
pub(crate) fn tab_spans<T: Clone + PartialEq>(
    tabs: &[Tab<T>],
    active: Option<usize>,
    review: T,
    hover: Option<&T>,
    width: u16,
) -> Vec<(Span<'static>, Option<T>)> {
    let t = theme::current();
    let hovered = |span: Span<'static>, target: &T| {
        let style = hover_style(span.style, hover == Some(target));
        Span::styled(span.content, style)
    };
    let mut spans = Vec::with_capacity(tabs.len() * 2 + 1);
    for (index, tab) in tabs.iter().enumerate() {
        let span = match (active == Some(index), tab.done) {
            (true, _) => Span::styled(format!(" {} ", tab.label), t.active),
            (false, true) => Span::styled(format!(" {}{DONE_MARK}", tab.label), t.todo_completed),
            (false, false) => Span::styled(format!(" {} ", tab.label), t.tool_dim),
        };
        spans.push((hovered(span, &tab.target), Some(tab.target.clone())));
        spans.push((Span::styled(SEPARATOR, t.tool_dim), None));
    }
    let review_style = if active.is_none() {
        t.active
    } else {
        t.tool_dim
    };
    spans.push((
        hovered(Span::styled(REVIEW, review_style), &review),
        Some(review),
    ));
    let total: usize = spans.iter().map(|(span, _)| span.content.width()).sum();
    if total <= usize::from(width) {
        return spans;
    }
    let pages = tabs.len() + 1;
    let (label, place) = match active.and_then(|index| tabs.get(index)) {
        Some(tab) => (tab.label.as_str(), active.unwrap_or_default() + 1),
        None => (REVIEW.trim(), pages),
    };
    vec![(
        Span::styled(
            format!(" {label}{PLACE_SEPARATOR}{place} of {pages} "),
            t.active,
        ),
        None,
    )]
}

#[cfg(test)]
mod tests {
    use super::{Tab, tab_spans};

    fn text(spans: &[(ratatui::text::Span<'static>, Option<usize>)]) -> String {
        spans
            .iter()
            .map(|(span, _)| span.content.as_ref())
            .collect()
    }

    fn tabs() -> Vec<Tab<usize>> {
        ["cargo fmt", "cargo clippy", "rm"]
            .into_iter()
            .enumerate()
            .map(|(index, label)| Tab {
                label: label.into(),
                done: index == 0,
                target: index,
            })
            .collect()
    }

    #[test]
    fn a_bar_that_fits_lists_every_page_then_review() {
        let spans = tab_spans(&tabs(), Some(1), usize::MAX, None, 80);
        assert_eq!(text(&spans), " cargo fmt ✓ │ cargo clippy │ rm │ Review ");
        assert_eq!(
            spans
                .iter()
                .filter_map(|(_, target)| *target)
                .collect::<Vec<_>>(),
            [0, 1, 2, usize::MAX]
        );
    }

    #[test]
    fn a_bar_that_does_not_fit_names_the_active_page_and_its_place() {
        let spans = tab_spans(&tabs(), Some(1), usize::MAX, None, 20);
        assert_eq!(text(&spans), " cargo clippy · 2 of 4 ");
        assert!(spans.iter().all(|(_, target)| target.is_none()));
    }
}
