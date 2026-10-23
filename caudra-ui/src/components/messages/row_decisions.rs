//! What the decision engine decided about a turn, drawn on the text row the
//! transcript ended on when it was asked: chips at the right end of the row's
//! first line, and the details a click on them opens under the row.

use std::iter;

use ratatui::text::{Line, Span};

use super::BuiltMessage;
use super::segment::ChipsHit;
use crate::components::card_decisions::{self, CardDecision, Chips};

/// The fewest blank columns between a row's own text and its chips.
const MIN_GAP: usize = 2;

/// Draws `decisions` on what a row `built` at `width` content columns. Their
/// chips go at the end of its first line, counted when they do not fit whole,
/// and on a line of their own under the row only when neither fits, so the
/// row's text never reflows and chips coming and going leave its height
/// alone. Once `open`, each decision's details follow, and search and copy
/// reach them. The chips themselves are neither searched nor copied.
pub(super) fn decorate(
    built: &mut BuiltMessage,
    decisions: &[CardDecision],
    open: bool,
    width: u16,
) {
    let width = usize::from(width);
    let [whole, counted] = card_decisions::row_chips(decisions);
    let fits = |used: usize, chips: &Chips| used + MIN_GAP + chips.width() <= width;
    match built.lines.first().map(Line::width) {
        Some(used) if fits(used, &whole) => place(built, 0, used, whole, width),
        Some(used) if fits(used, &counted) => place(built, 0, used, counted, width),
        _ => {
            let chips = match whole.width() <= width {
                true => whole,
                false => counted,
            };
            built.lines.push(Line::default());
            built.links.rows.push(Vec::new());
            if let Some(provenance) = built.provenance.as_mut() {
                provenance.push_chrome_line(0);
            }
            let line = built.lines.len() - 1;
            place(built, line, 0, chips, width);
        }
    }
    if open {
        for (lines, text) in card_decisions::details(decisions) {
            if let Some(provenance) = built.provenance.as_mut() {
                provenance.push_verbatim_lines(&lines, text);
            }
            built
                .links
                .rows
                .extend(lines.iter().map(|line| vec![None; line.spans.len()]));
            built.search_text.push('\n');
            built.search_text.push_str(text);
            built.lines.extend(lines);
        }
    }
}

/// Pads line `line`, `used` columns of it already drawn, out to `chips`
/// ending at the right edge.
fn place(built: &mut BuiltMessage, line: usize, used: usize, chips: Chips, width: usize) {
    let start = width.saturating_sub(chips.width());
    let mut spans = vec![Span::raw(" ".repeat(start.saturating_sub(used)))];
    spans.extend(chips.spans);
    if let Some(links) = built.links.rows.get_mut(line) {
        links.extend(iter::repeat_n(None, spans.len()));
    }
    if let Some(provenance) = built.provenance.as_mut() {
        provenance.append_chrome_spans(line, spans.len());
    }
    built.lines[line].spans.extend(spans);
    built.decision_chips = Some(ChipsHit {
        line,
        columns: start as u16..width as u16,
    });
}

#[cfg(test)]
mod tests {
    use caudra_agent::decisions::{DecisionFeature, NoteOutcome};
    use ratatui::text::Line;
    use test_case::test_case;

    use super::{MIN_GAP, decorate};
    use crate::components::card_decisions::{self, CardDecision, Chips, DecisionSubject};
    use crate::components::messages::BuiltMessage;

    const WIDTH: u16 = 40;
    const SUMMARY: &str = "2 notes recalled";
    const DETAIL: &str = "0.97  release-notes";
    const TEXT: char = 'x';

    enum Room {
        Whole,
        Counted,
        None,
    }

    fn decisions() -> Vec<CardDecision> {
        [
            DecisionFeature::MemoryRecall,
            DecisionFeature::SkillSuggestions,
        ]
        .into_iter()
        .map(|feature| {
            CardDecision::settled(
                DecisionSubject::Turn,
                feature,
                NoteOutcome::Acted,
                SUMMARY,
                &[DETAIL.into()],
            )
        })
        .collect()
    }

    fn drawn(chips: &Chips) -> String {
        chips
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn built(used: usize) -> BuiltMessage {
        let text = TEXT.to_string().repeat(used);
        BuiltMessage::bare(vec![Line::from(text.clone())], text)
    }

    /// The row's text keeps its line, the chips end at the right edge: whole
    /// when they fit, counted when only that fits, and on a line of their own
    /// when neither does.
    #[test_case(Room::Whole ; "whole_chips_on_the_first_line")]
    #[test_case(Room::Counted ; "counted_chips_on_the_first_line")]
    #[test_case(Room::None ; "chips_under_a_full_first_line")]
    fn chips_take_the_room_the_first_line_leaves(room: Room) {
        let decisions = decisions();
        let [whole, counted] = card_decisions::row_chips(&decisions);
        let width = usize::from(WIDTH);
        let used = match room {
            Room::Whole => 1,
            Room::Counted => width - MIN_GAP - counted.width(),
            Room::None => width - 1,
        };
        let mut built = built(used);

        decorate(&mut built, &decisions, false, WIDTH);

        let (line, chips) = match room {
            Room::Whole => (0, whole),
            Room::Counted => (0, counted),
            Room::None => (1, whole),
        };
        let shown = line_text(&built.lines[line]);
        assert!(shown.ends_with(&drawn(&chips)), "{shown}");
        assert_eq!(built.lines[line].width(), width);
        assert_eq!(built.lines.len(), line + 1);
        assert_eq!(built.links.rows.len(), built.lines.len());
        assert_eq!(built.decision_chips.map(|hit| hit.line), Some(line));
        assert!(line_text(&built.lines[0]).starts_with(&TEXT.to_string().repeat(used)));
    }

    #[test_case(false ; "folded")]
    #[test_case(true ; "open")]
    fn only_open_details_are_drawn_and_searched(open: bool) {
        let decisions = decisions();
        let mut built = built(1);

        decorate(&mut built, &decisions, open, WIDTH);

        let shown: Vec<String> = built.lines.iter().map(line_text).collect();
        let details = shown.iter().filter(|line| line.contains(DETAIL)).count();
        assert_eq!(details, if open { decisions.len() } else { 0 });
        assert_eq!(built.search_text.contains(DETAIL), open);
        assert_eq!(built.links.rows.len(), built.lines.len());
    }
}
