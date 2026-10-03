//! The numbered strip a tabbed modal draws above its body, `1 Overview  2 …`,
//! which falls back to bare digits when the names do not fit. It reports the
//! cells each tab landed on, so a click names the tab it hit.

use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::components::hover_style;
use crate::theme;

const TAB_GAP: &str = "  ";

/// A modal's sections in strip order, which is also the order the digit keys
/// and Tab walk them in.
pub(crate) trait SectionTab: Copy + Eq + 'static {
    const ALL: &'static [Self];

    fn label(self) -> &'static str;

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|tab| *tab == self)
            .unwrap_or_default()
    }

    /// The section a digit key names, counting from one.
    fn from_digit(digit: char) -> Option<Self> {
        let number = digit.to_digit(10)? as usize;
        Self::ALL.get(number.checked_sub(1)?).copied()
    }

    fn step(self, delta: isize) -> Self {
        let len = Self::ALL.len() as isize;
        Self::ALL[(self.index() as isize + delta).rem_euclid(len) as usize]
    }
}

/// The strip with `current` marked and the tab under `pointer` hovered, and
/// the cells each tab claims. A tab drawn past the edge of `area` claims none,
/// because a click there lands on whatever the terminal actually shows.
pub(crate) fn tab_strip<T: SectionTab>(
    current: T,
    pointer: Option<Position>,
    area: Rect,
) -> (Line<'static>, Vec<(Rect, T)>) {
    let t = theme::current();
    let named = strip_cols::<T>() <= area.width;
    let gap = u16::try_from(TAB_GAP.width()).unwrap_or(u16::MAX);
    let mut spans = Vec::with_capacity(T::ALL.len() * 2);
    let mut hits = Vec::with_capacity(T::ALL.len());
    let mut x = area.x;
    for &tab in T::ALL {
        let digit = tab.index() + 1;
        let text = match named {
            true => format!("{digit} {}", tab.label()),
            false => digit.to_string(),
        };
        let width = u16::try_from(text.width()).unwrap_or(u16::MAX);
        let style = match tab == current {
            true => t.item_selected,
            false => t.tool_dim,
        };
        let hit = Rect::new(x, area.y, width, 1);
        if hit.right() <= area.right() {
            hits.push((hit, tab));
        }
        spans.push(Span::styled(
            text,
            hover_style(style, pointer.is_some_and(|at| hit.contains(at))),
        ));
        spans.push(Span::raw(TAB_GAP));
        x = x.saturating_add(width).saturating_add(gap);
    }
    (Line::from(spans), hits)
}

/// What the strip needs to name every section, the gap after the last one
/// included.
fn strip_cols<T: SectionTab>() -> u16 {
    let named: usize = T::ALL
        .iter()
        .map(|tab| (tab.index() + 1).to_string().len() + 1 + tab.label().width() + TAB_GAP.width())
        .sum();
    u16::try_from(named).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use ratatui::layout::{Position, Rect};
    use test_case::test_case;

    use super::{SectionTab, tab_strip};

    const NAMED_WIDTH: u16 = 40;
    const DIGITS_WIDTH: u16 = 12;
    const CRAMPED_WIDTH: u16 = 4;
    const NAMES_WHEN_THEY_FIT: &str = "a strip with room names its tabs";
    const DIGITS_OTHERWISE: &str = "a cramped strip falls back to digits";
    const HITS_STAY_IN_THE_AREA: &str = "a tab past the edge claims no cells";

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Tab {
        First,
        Second,
        Third,
    }

    impl SectionTab for Tab {
        const ALL: &'static [Self] = &[Self::First, Self::Second, Self::Third];

        fn label(self) -> &'static str {
            match self {
                Self::First => "First",
                Self::Second => "Second",
                Self::Third => "Third",
            }
        }
    }

    #[test_case('1', Some(Tab::First); "first")]
    #[test_case('3', Some(Tab::Third); "last")]
    #[test_case('0', None; "zero")]
    #[test_case('4', None; "past_the_end")]
    fn digits_count_from_one(digit: char, expected: Option<Tab>) {
        assert_eq!(Tab::from_digit(digit), expected);
    }

    #[test_case(Tab::First, 1, Tab::Second; "forward")]
    #[test_case(Tab::Third, 1, Tab::First; "wraps_forward")]
    #[test_case(Tab::First, -1, Tab::Third; "wraps_backward")]
    fn stepping_wraps(from: Tab, delta: isize, expected: Tab) {
        assert_eq!(from.step(delta), expected);
    }

    #[test_case(NAMED_WIDTH, true; "named")]
    #[test_case(DIGITS_WIDTH, false; "digits")]
    fn names_fall_back_to_digits(width: u16, named: bool) {
        let (line, hits) = tab_strip(Tab::First, None, Rect::new(0, 0, width, 1));

        let text = line.to_string();
        assert_eq!(
            text.contains(Tab::Second.label()),
            named,
            "{NAMES_WHEN_THEY_FIT}: {text}"
        );
        assert!(named || text.starts_with('1'), "{DIGITS_OTHERWISE}: {text}");
        assert_eq!(hits.len(), Tab::ALL.len());
    }

    #[test]
    fn a_tab_past_the_edge_claims_no_cells() {
        let area = Rect::new(0, 0, CRAMPED_WIDTH, 1);

        let (_, hits) = tab_strip(Tab::First, None, area);

        assert!(hits.len() < Tab::ALL.len(), "{HITS_STAY_IN_THE_AREA}");
        assert!(hits.iter().all(|(hit, _)| hit.right() <= area.right()));
    }

    #[test]
    fn only_the_hovered_tab_changes_style() {
        let area = Rect::new(0, 0, NAMED_WIDTH, 1);
        let (plain, hits) = tab_strip(Tab::First, None, area);
        let (second, _) = hits[Tab::Second.index()];

        let (hovered, _) = tab_strip(Tab::First, Some(Position::new(second.x, second.y)), area);

        let changed: Vec<bool> = plain
            .spans
            .iter()
            .zip(&hovered.spans)
            .step_by(2)
            .map(|(before, after)| before.style != after.style)
            .collect();
        assert_eq!(changed, [false, true, false]);
    }
}
