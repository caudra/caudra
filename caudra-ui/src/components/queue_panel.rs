use std::borrow::Cow;

use caudra_agent::{PromptAdmission, QueueItemId};
use caudra_grab::grab_scope;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::hover_style;
use super::tooltip::{CutRow, Tip, TipKey};
use crate::theme;

const ELLIPSIS: &str = "...";
const GUIDE_SECTION: &str = "Guide";
const MAX_VISIBLE_ENTRIES: usize = 4;
const MAX_VISIBLE_ROWS: usize = 6;
const MENU_GAP: &str = " ";
const MENU_LABEL: &str = "⋮";
const REPLACING_SECTION: &str = "Replacing";
const SEPARATE_LABEL: &str = "[Mode: Separate]";
const TOGETHER_LABEL: &str = "[Mode: Together]";
const UP_NEXT_SECTION: &str = "Up next";
pub(crate) const TIP_SEPARATE: &str = "Separate: each waiting prompt runs in its own turn";
pub(crate) const TIP_TOGETHER: &str =
    "Together: compatible waiting prompts share one model turn, then this resets";
const CLICK_TOGGLE: &str = "Click or press b in the queue to switch";
const TIP_MENU: &str = "Actions for this prompt, like edit, move or delete";
const CLICK_MENU: &str = "Click or press . on the focused row";

pub struct QueueEntry<'a> {
    pub id: QueueItemId,
    pub text: Cow<'a, str>,
    pub color: ratatui::style::Color,
    pub editable: bool,
    pub movable: bool,
    pub can_move_up: bool,
    pub can_move_down: bool,
    pub admission: Option<PromptAdmission>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueAction {
    Select,
    Menu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueHitTarget {
    Item {
        id: QueueItemId,
        action: QueueAction,
    },
    ToggleTogether,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueHit {
    pub area: Rect,
    pub target: QueueHitTarget,
}

impl QueueHit {
    /// What the control means and how to reach it without the mouse. A row
    /// says itself; only one drawn short has more to say, see [`QueueView`].
    pub(crate) fn tip(self, together: bool) -> Option<Tip> {
        let (meaning, click) = match self.target {
            QueueHitTarget::ToggleTogether if together => (TIP_TOGETHER, CLICK_TOGGLE),
            QueueHitTarget::ToggleTogether => (TIP_SEPARATE, CLICK_TOGGLE),
            QueueHitTarget::Item {
                action: QueueAction::Menu,
                ..
            } => (TIP_MENU, CLICK_MENU),
            QueueHitTarget::Item {
                action: QueueAction::Select,
                ..
            } => return None,
        };
        Some(Tip::at(
            TipKey::Queue(self.target),
            self.area,
            format!("{meaning}\n{click}"),
        ))
    }
}

/// What a frame of the panel left behind for the pointer.
#[derive(Default)]
pub(crate) struct QueueView {
    pub hits: Vec<QueueHit>,
    /// Prompts too long for their row, for a tooltip with the whole text.
    pub cut_rows: Vec<CutRow>,
}

#[derive(Clone, Copy, Default)]
pub struct QueuePanelState {
    pub focus: Option<usize>,
    pub viewport: usize,
    pub together: Option<bool>,
    pub hovered: Option<QueueHitTarget>,
}

/// One drawn line: either a lane header or an entry, addressed by its index in
/// the entry slice so focus and viewport stay in entry space app-side.
enum PanelRow {
    Section { label: &'static str, count: usize },
    Entry(usize),
}

pub fn height(entries: &[QueueEntry<'_>]) -> u16 {
    if entries.is_empty() {
        0
    } else {
        panel_rows(entries).len().min(MAX_VISIBLE_ROWS) as u16 + 2
    }
}

pub fn max_visible_entries() -> usize {
    MAX_VISIBLE_ENTRIES
}

pub(crate) fn view(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    entries: &[QueueEntry],
    state: QueuePanelState,
) -> QueueView {
    if entries.is_empty() || area.width < 2 || area.height < 2 {
        return QueueView::default();
    }
    grab_scope!("queue_panel", area);

    let left = if area.width >= 32 {
        3
    } else if area.width >= 16 {
        2
    } else {
        1
    };
    let right = u16::from(area.width >= 32);
    let content_area = Rect::new(
        area.x.saturating_add(left),
        area.y.saturating_add(1),
        area.width.saturating_sub(left.saturating_add(right)),
        area.height.saturating_sub(2),
    );
    let rows = panel_rows(entries);
    let visible_rows = usize::from(content_area.height);
    let start = window_start(&rows, state, visible_rows);
    let end = start.saturating_add(visible_rows).min(rows.len());
    let mut hits = Vec::new();
    let mut cut_rows = Vec::new();
    let lines = rows
        .get(start..end)
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(offset, row)| {
            let row_area = Rect::new(
                content_area.x,
                content_area.y + offset as u16,
                content_area.width,
                1,
            );
            match row {
                PanelRow::Section { label, count } => Line::from(Span::styled(
                    format!("{label} · {count}"),
                    theme::current().tool_dim,
                )),
                PanelRow::Entry(index) => entry_line(
                    &entries[*index],
                    state.focus == Some(*index),
                    state.hovered,
                    row_area,
                    &mut hits,
                    &mut cut_rows,
                ),
            }
        })
        .collect::<Vec<_>>();

    frame.render_widget(Block::default().style(theme::current().panel_style()), area);
    let rail_style = if state.focus.is_some() {
        theme::current().item_selected
    } else {
        theme::current().panel_border
    };
    for y in area.y..area.bottom() {
        if let Some(cell) = frame.buffer_mut().cell_mut((area.x, y)) {
            cell.set_char('┃').set_style(rail_style);
        }
    }
    let header_area = Rect::new(
        content_area.x,
        area.y,
        content_area.width,
        1.min(area.height),
    );
    frame.render_widget(
        Paragraph::new(Line::from(format!("{title} · {}", entries.len())))
            .style(theme::current().panel_title),
        header_area,
    );
    if let Some(together) = state.together {
        let label = if together {
            TOGETHER_LABEL
        } else {
            SEPARATE_LABEL
        };
        let width = label.width() as u16;
        if content_area.width > width + 4 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    label,
                    hover_style(
                        theme::current().keybind_key,
                        state.hovered == Some(QueueHitTarget::ToggleTogether),
                    ),
                )))
                .right_aligned(),
                header_area,
            );
            hits.push(QueueHit {
                area: Rect::new(content_area.right().saturating_sub(width), area.y, width, 1),
                target: QueueHitTarget::ToggleTogether,
            });
        }
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::new().fg(theme::current().foreground)),
        content_area,
    );
    QueueView { hits, cut_rows }
}

fn entry_line(
    entry: &QueueEntry<'_>,
    selected: bool,
    hovered: Option<QueueHitTarget>,
    row: Rect,
    hits: &mut Vec<QueueHit>,
    cut_rows: &mut Vec<CutRow>,
) -> Line<'static> {
    let select_target = QueueHitTarget::Item {
        id: entry.id,
        action: QueueAction::Select,
    };
    let menu_target = QueueHitTarget::Item {
        id: entry.id,
        action: QueueAction::Menu,
    };
    hits.push(QueueHit {
        area: row,
        target: select_target,
    });
    hits.push(QueueHit {
        area: Rect::new(row.x, row.y, (MENU_LABEL.width() as u16).min(row.width), 1),
        target: menu_target,
    });

    let text_offset = MENU_LABEL.width() + MENU_GAP.width();
    let available = usize::from(row.width).saturating_sub(text_offset);
    let style = if selected {
        theme::current().item_selected
    } else {
        hover_style(Style::new().fg(entry.color), hovered == Some(select_target))
    };
    let flat = entry.text.replace('\n', " ");
    let text = truncate_span(&flat, available, style);
    if flat.width() > available && available > 0 {
        cut_rows.push(CutRow {
            area: Rect::new(
                row.x.saturating_add(text_offset as u16),
                row.y,
                available as u16,
                1,
            ),
            text: flat,
        });
    }
    let padding = available.saturating_sub(text.content.width());
    Line::from(vec![
        Span::styled(
            MENU_LABEL,
            hover_style(theme::current().keybind_key, hovered == Some(menu_target)),
        ),
        Span::raw(MENU_GAP),
        text,
        Span::raw(" ".repeat(padding)),
    ])
}

fn panel_rows(entries: &[QueueEntry<'_>]) -> Vec<PanelRow> {
    let mut rows = Vec::with_capacity(entries.len() + 1);
    let mut lane = None;
    for (index, entry) in entries.iter().enumerate() {
        let group = admission_group(entry.admission);
        if lane != Some(group) {
            lane = Some(group);
            rows.push(PanelRow::Section {
                label: section_label(group),
                count: entries[index..]
                    .iter()
                    .take_while(|entry| admission_group(entry.admission) == group)
                    .count(),
            });
        }
        rows.push(PanelRow::Entry(index));
    }
    rows
}

/// Entry-space viewport and focus translated to the row space that section
/// headers live in, pulling in the header above the first visible entry and
/// keeping the focused entry on screen.
fn window_start(rows: &[PanelRow], state: QueuePanelState, visible_rows: usize) -> usize {
    let mut start = entry_row(rows, state.viewport).unwrap_or(0);
    if start > 0 && matches!(rows[start - 1], PanelRow::Section { .. }) {
        start -= 1;
    }
    start = start.min(rows.len().saturating_sub(visible_rows));
    let Some(focus) = state.focus.and_then(|index| entry_row(rows, index)) else {
        return start;
    };
    if focus < start {
        focus
    } else if visible_rows > 0 && focus >= start + visible_rows {
        focus + 1 - visible_rows
    } else {
        start
    }
}

fn entry_row(rows: &[PanelRow], entry: usize) -> Option<usize> {
    rows.iter()
        .position(|row| matches!(row, PanelRow::Entry(index) if *index == entry))
}

pub(crate) fn set_movement_flags(entries: &mut [QueueEntry<'_>]) {
    let mut previous: [Option<usize>; 3] = [None; 3];
    for index in 0..entries.len() {
        let Some(admission) = entries[index].admission else {
            previous = [None; 3];
            continue;
        };
        let lane = admission_group(Some(admission)) as usize;
        if let Some(previous_index) = previous[lane] {
            entries[index].can_move_up = true;
            entries[previous_index].can_move_down = true;
        }
        previous[lane] = Some(index);
    }
}

fn section_label(group: u8) -> &'static str {
    match group {
        0 => REPLACING_SECTION,
        1 => GUIDE_SECTION,
        _ => UP_NEXT_SECTION,
    }
}

fn admission_group(admission: Option<PromptAdmission>) -> u8 {
    match admission {
        Some(PromptAdmission::Interrupt) => 0,
        Some(PromptAdmission::Steer) => 1,
        Some(PromptAdmission::Queue) | None => 2,
    }
}

fn truncate_span(text: &str, max_width: usize, style: Style) -> Span<'static> {
    if text.width() <= max_width {
        return Span::styled(text.to_string(), style);
    }
    if max_width <= ELLIPSIS.width() {
        return Span::styled(".".repeat(max_width), style);
    }
    let keep = max_width.saturating_sub(ELLIPSIS.width());
    let mut width = 0;
    let end = text
        .char_indices()
        .take_while(|(_, ch)| {
            let next = width + unicode_width::UnicodeWidthChar::width(*ch).unwrap_or(0);
            if next > keep {
                false
            } else {
                width = next;
                true
            }
        })
        .map(|(index, ch)| index + ch.len_utf8())
        .last()
        .unwrap_or(0);
    Span::styled(format!("{}{}", &text[..end], ELLIPSIS), style)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;
    use test_case::test_case;

    const PANEL_TITLE: &str = "Queue - Main";
    const PROMPT: &str = "queued prompt";
    const LONG_PROMPT: &str = "a queued prompt far too long\nfor the narrow row it is drawn on";
    const CUT_MISSED: &str = "a prompt drawn short must keep its whole text for a tooltip";
    const CUT_SPURIOUS: &str = "a prompt drawn whole has nothing more to show";

    fn entry(text: &'static str, admission: Option<PromptAdmission>) -> QueueEntry<'static> {
        QueueEntry {
            id: QueueItemId::new(),
            text: Cow::Borrowed(text),
            color: theme::current().foreground,
            editable: true,
            movable: false,
            can_move_up: false,
            can_move_down: false,
            admission,
        }
    }

    fn draw(
        entries: &[QueueEntry<'_>],
        state: QueuePanelState,
        size: (u16, u16),
    ) -> (Vec<String>, QueueView) {
        let backend = TestBackend::new(size.0, size.1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut drawn = QueueView::default();
        terminal
            .draw(|frame| {
                drawn = view(frame, frame.area(), PANEL_TITLE, entries, state);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows = (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer.cell((column, row)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect();
        (rows, drawn)
    }

    fn shows(rows: &[String], text: &str) -> bool {
        rows.iter().any(|row| row.contains(text))
    }

    fn row_of(rows: &[String], text: &str) -> Option<usize> {
        rows.iter().position(|row| row.contains(text))
    }

    fn symbol_at(rows: &[String], x: u16, y: u16) -> Option<char> {
        rows.get(usize::from(y))?.chars().nth(usize::from(x))
    }

    #[test_case(&[], 0 ; "empty_panel_is_hidden")]
    #[test_case(&[Some(PromptAdmission::Queue)], 4 ; "one_lane_adds_one_header")]
    #[test_case(
        &[Some(PromptAdmission::Steer), Some(PromptAdmission::Queue)],
        6 ; "two_lanes_add_two_headers"
    )]
    #[test_case(
        &[
            Some(PromptAdmission::Steer),
            Some(PromptAdmission::Queue),
            Some(PromptAdmission::Queue),
            Some(PromptAdmission::Queue),
            Some(PromptAdmission::Queue),
        ],
        8 ; "rows_are_bounded"
    )]
    fn height_counts_section_rows(admissions: &[Option<PromptAdmission>], expected: u16) {
        let entries = admissions
            .iter()
            .map(|admission| entry(PROMPT, *admission))
            .collect::<Vec<_>>();

        assert_eq!(height(&entries), expected);
    }

    #[test]
    fn truncation_uses_terminal_width() {
        assert_eq!(truncate_span("abcdef", 5, Style::new()).content, "ab...");
        assert_eq!(
            truncate_span("你好世界", 7, Style::new()).content,
            "你好..."
        );
    }

    #[test]
    fn queue_uses_the_shared_panel_grid() {
        let entries = [entry(PROMPT, Some(PromptAdmission::Queue))];

        let (rendered, QueueView { hits, .. }) = draw(
            &entries,
            QueuePanelState {
                together: Some(false),
                ..QueuePanelState::default()
            },
            (60, 4),
        );

        assert_eq!(symbol_at(&rendered, 0, 0), Some('┃'));
        assert!(shows(&rendered, &format!("{UP_NEXT_SECTION} · 1")));
        assert!(
            hits.iter()
                .any(|hit| hit.target == QueueHitTarget::ToggleTogether)
        );
        assert!(hits.iter().any(|hit| {
            matches!(
                hit.target,
                QueueHitTarget::Item {
                    action: QueueAction::Select,
                    ..
                }
            ) && hit.area.x == 3
        }));
    }

    #[test]
    fn sections_are_headed_in_claim_order_with_their_own_counts() {
        let entries = [
            entry("replacing", Some(PromptAdmission::Interrupt)),
            entry("guiding", Some(PromptAdmission::Steer)),
            entry("first", Some(PromptAdmission::Queue)),
            entry("second", Some(PromptAdmission::Queue)),
        ];

        let (rendered, _) = draw(&entries, QueuePanelState::default(), (60, 9));

        let headers = [
            format!("{REPLACING_SECTION} · 1"),
            format!("{GUIDE_SECTION} · 1"),
            format!("{UP_NEXT_SECTION} · 2"),
        ]
        .map(|header| row_of(&rendered, &header));
        assert!(headers.iter().all(Option::is_some), "{headers:?}");
        assert!(headers[0] < headers[1] && headers[1] < headers[2]);
    }

    #[test]
    fn every_entry_row_carries_the_menu_affordance() {
        let entries = [
            entry("guiding", Some(PromptAdmission::Steer)),
            entry("queued", Some(PromptAdmission::Queue)),
        ];

        let (rendered, QueueView { hits, .. }) =
            draw(&entries, QueuePanelState::default(), (60, 8));

        let menu_hits = hits
            .iter()
            .filter(|hit| {
                matches!(
                    hit.target,
                    QueueHitTarget::Item {
                        action: QueueAction::Menu,
                        ..
                    }
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(menu_hits.len(), entries.len());
        for hit in menu_hits {
            assert_eq!(hit.area.width, MENU_LABEL.width() as u16);
            assert_eq!(
                symbol_at(&rendered, hit.area.x, hit.area.y),
                MENU_LABEL.chars().next()
            );
        }
    }

    #[test_case(LONG_PROMPT, 30, true ; "long_prompt_is_cut")]
    #[test_case(PROMPT, 60, false ; "short_prompt_is_whole")]
    fn a_cut_row_keeps_its_whole_text_beside_the_menu(text: &'static str, width: u16, cut: bool) {
        let entries = [entry(text, Some(PromptAdmission::Queue))];

        let (_, drawn) = draw(&entries, QueuePanelState::default(), (width, 4));

        if !cut {
            assert!(drawn.cut_rows.is_empty(), "{CUT_SPURIOUS}");
            return;
        }
        let [row] = drawn.cut_rows.as_slice() else {
            panic!("{CUT_MISSED}");
        };
        assert_eq!(row.text, text.replace('\n', " "), "{CUT_MISSED}");
        let menu = drawn
            .hits
            .iter()
            .find(|hit| {
                matches!(
                    hit.target,
                    QueueHitTarget::Item {
                        action: QueueAction::Menu,
                        ..
                    }
                )
            })
            .expect(CUT_MISSED);
        assert!(row.area.x > menu.area.right(), "{CUT_MISSED}");
        assert_eq!(row.area.y, menu.area.y, "{CUT_MISSED}");
    }

    #[test_case(QueueHitTarget::ToggleTogether, false, Some(TIP_SEPARATE) ; "separate")]
    #[test_case(QueueHitTarget::ToggleTogether, true, Some(TIP_TOGETHER) ; "together")]
    #[test_case(
        QueueHitTarget::Item { id: QueueItemId::new(), action: QueueAction::Menu },
        false,
        Some(TIP_MENU) ;
        "menu"
    )]
    #[test_case(
        QueueHitTarget::Item { id: QueueItemId::new(), action: QueueAction::Select },
        false,
        None ;
        "row_says_itself"
    )]
    fn a_control_tip_names_what_it_does(
        target: QueueHitTarget,
        together: bool,
        meaning: Option<&str>,
    ) {
        let hit = QueueHit {
            area: Rect::new(1, 1, 4, 1),
            target,
        };
        let tip = hit.tip(together).map(|tip| tip.text);
        assert_eq!(tip.as_deref().and_then(|text| text.lines().next()), meaning);
    }

    #[test]
    fn menu_hover_reverses_the_glyph_only() {
        let entries = [entry(PROMPT, Some(PromptAdmission::Queue))];
        let target = QueueHitTarget::Item {
            id: entries[0].id,
            action: QueueAction::Menu,
        };
        let backend = TestBackend::new(60, 4);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut hits = Vec::new();
        terminal
            .draw(|frame| {
                hits = view(
                    frame,
                    frame.area(),
                    PANEL_TITLE,
                    &entries,
                    QueuePanelState {
                        hovered: Some(target),
                        ..QueuePanelState::default()
                    },
                )
                .hits;
            })
            .unwrap();
        let hit = hits.iter().find(|hit| hit.target == target).unwrap();
        let buffer = terminal.backend().buffer();

        assert!(
            buffer
                .cell((hit.area.x, hit.area.y))
                .unwrap()
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            !buffer
                .cell((hit.area.right(), hit.area.y))
                .unwrap()
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn focused_entry_stays_visible_when_sections_take_rows() {
        let entries = [
            entry("guiding", Some(PromptAdmission::Steer)),
            entry("first", Some(PromptAdmission::Queue)),
            entry("second", Some(PromptAdmission::Queue)),
            entry("third", Some(PromptAdmission::Queue)),
        ];

        let (rendered, QueueView { hits, .. }) = draw(
            &entries,
            QueuePanelState {
                focus: Some(3),
                ..QueuePanelState::default()
            },
            (60, 5),
        );

        assert!(shows(&rendered, "third"));
        assert!(hits.iter().any(|hit| {
            matches!(
                hit.target,
                QueueHitTarget::Item { id, action: QueueAction::Menu } if id == entries[3].id
            )
        }));
    }

    #[test]
    fn viewport_pulls_in_the_section_header_above_it() {
        let entries = [
            entry("guiding", Some(PromptAdmission::Steer)),
            entry("first", Some(PromptAdmission::Queue)),
            entry("second", Some(PromptAdmission::Queue)),
        ];

        let (rendered, _) = draw(
            &entries,
            QueuePanelState {
                viewport: 1,
                ..QueuePanelState::default()
            },
            (60, 4),
        );

        assert!(shows(&rendered, &format!("{UP_NEXT_SECTION} · 2")));
        assert!(shows(&rendered, "first"));
    }

    #[test]
    fn out_of_range_focus_and_viewport_render_without_panicking() {
        let entries = [entry(PROMPT, Some(PromptAdmission::Queue))];

        let (rendered, _) = draw(
            &entries,
            QueuePanelState {
                focus: Some(9),
                viewport: 9,
                ..QueuePanelState::default()
            },
            (60, 4),
        );

        assert!(shows(&rendered, PROMPT));
    }

    #[test]
    fn movement_flags_follow_lane_bounds() {
        let mut entries =
            ["first", "second", "third"].map(|text| entry(text, Some(PromptAdmission::Queue)));

        set_movement_flags(&mut entries);

        assert!(!entries[0].can_move_up);
        assert!(entries[0].can_move_down);
        assert!(entries[1].can_move_up);
        assert!(entries[1].can_move_down);
        assert!(entries[2].can_move_up);
        assert!(!entries[2].can_move_down);
    }
}
