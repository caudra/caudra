use std::borrow::Cow;

use maki_agent::{PromptAdmission, QueueItemId};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::theme;

const DELETE_LABEL: &str = "[Delete]";
const DELETE_LABEL_COMPACT: &str = "[D]";
const ACTION_SEPARATOR: &str = " ";
const EDIT_LABEL: &str = "[Edit]";
const EDIT_LABEL_COMPACT: &str = "[E]";
const ELLIPSIS: &str = "...";
const MAX_VISIBLE_ROWS: usize = 4;
const MOVE_MAIN_LABEL: &str = "[Main]";
const MOVE_MAIN_LABEL_COMPACT: &str = "[M]";
const SEPARATE_LABEL: &str = "[Mode: Separate]";
const TOGETHER_LABEL: &str = "[Mode: Together]";

pub struct QueueEntry<'a> {
    pub id: QueueItemId,
    pub text: Cow<'a, str>,
    pub color: ratatui::style::Color,
    pub editable: bool,
    pub movable: bool,
    pub admission: Option<PromptAdmission>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueAction {
    Select,
    Edit,
    Delete,
    MoveMain,
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

#[derive(Clone, Copy, Default)]
pub struct QueuePanelState {
    pub focus: Option<usize>,
    pub viewport: usize,
    pub together: Option<bool>,
    pub hovered: Option<QueueHitTarget>,
}

pub fn height(queue_len: usize) -> u16 {
    if queue_len == 0 {
        0
    } else {
        queue_len.min(MAX_VISIBLE_ROWS) as u16 + 2
    }
}

pub fn max_visible_rows() -> usize {
    MAX_VISIBLE_ROWS
}

pub fn view(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    entries: &[QueueEntry],
    state: QueuePanelState,
) -> Vec<QueueHit> {
    if entries.is_empty() || area.width < 2 || area.height < 2 {
        return Vec::new();
    }

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
    let visible_rows = usize::from(content_area.height);
    let viewport = state
        .viewport
        .min(entries.len().saturating_sub(visible_rows));
    let end = (viewport + visible_rows).min(entries.len());
    let content_width = content_area.width as usize;
    let mut hits = Vec::new();
    let lines = entries[viewport..end]
        .iter()
        .enumerate()
        .map(|(visible_index, entry)| {
            let index = viewport + visible_index;
            let selected = state.focus == Some(index);
            let select_target = QueueHitTarget::Item {
                id: entry.id,
                action: QueueAction::Select,
            };
            let row = content_area.y + visible_index as u16;
            let row_area = Rect::new(content_area.x, row, content_area.width, 1);
            hits.push(QueueHit {
                area: row_area,
                target: select_target,
            });

            let mut actions = if selected {
                actions(entry, content_width < 48)
            } else {
                Vec::new()
            };
            let prefix = entry_prefix(entries, index);
            let mut action_width = actions
                .iter()
                .map(|(label, _)| label.width() + ACTION_SEPARATOR.width())
                .sum::<usize>();
            while !actions.is_empty() && prefix.width() + action_width > content_width {
                actions.remove(0);
                action_width = actions
                    .iter()
                    .map(|(label, _)| label.width() + ACTION_SEPARATOR.width())
                    .sum();
            }
            let available = content_width
                .saturating_sub(prefix.width())
                .saturating_sub(action_width);
            let style = if selected {
                theme::current().item_selected
            } else if state.hovered == Some(select_target) {
                Style::new()
                    .fg(entry.color)
                    .add_modifier(Modifier::REVERSED)
            } else {
                Style::new().fg(entry.color)
            };
            let flat = entry.text.replace('\n', " ");
            let text = truncate_span(&flat, available, style);
            let padding = available.saturating_sub(text.content.width());
            let mut spans = vec![
                Span::styled(prefix, theme::current().tool_dim),
                text,
                Span::raw(" ".repeat(padding)),
            ];

            let mut action_x = content_area.right().saturating_sub(action_width as u16);
            for (label, action) in actions {
                spans.push(Span::raw(ACTION_SEPARATOR));
                let action_style = if action == QueueAction::Delete {
                    theme::current().queue_delete
                } else {
                    theme::current().keybind_key
                };
                let action_style = if state.hovered
                    == Some(QueueHitTarget::Item {
                        id: entry.id,
                        action,
                    }) {
                    action_style.add_modifier(Modifier::REVERSED)
                } else {
                    action_style
                };
                spans.push(Span::styled(label, action_style));
                let width = label.width() as u16;
                action_x = action_x.saturating_add(ACTION_SEPARATOR.width() as u16);
                hits.push(QueueHit {
                    area: Rect::new(action_x, row, width, 1),
                    target: QueueHitTarget::Item {
                        id: entry.id,
                        action,
                    },
                });
                action_x = action_x.saturating_add(width);
            }
            Line::from(spans)
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
                    if state.hovered == Some(QueueHitTarget::ToggleTogether) {
                        theme::current()
                            .keybind_key
                            .add_modifier(Modifier::REVERSED)
                    } else {
                        theme::current().keybind_key
                    },
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
    hits
}

fn actions(entry: &QueueEntry<'_>, compact: bool) -> Vec<(&'static str, QueueAction)> {
    let mut actions = Vec::with_capacity(3);
    if entry.editable {
        actions.push((
            if compact {
                EDIT_LABEL_COMPACT
            } else {
                EDIT_LABEL
            },
            QueueAction::Edit,
        ));
    }
    if entry.movable {
        actions.push((
            if compact {
                MOVE_MAIN_LABEL_COMPACT
            } else {
                MOVE_MAIN_LABEL
            },
            QueueAction::MoveMain,
        ));
    }
    actions.push((
        if compact {
            DELETE_LABEL_COMPACT
        } else {
            DELETE_LABEL
        },
        QueueAction::Delete,
    ));
    actions
}

fn entry_prefix(entries: &[QueueEntry<'_>], index: usize) -> String {
    let admission = entries[index].admission;
    let position = entries[..index]
        .iter()
        .filter(|entry| admission_group(entry.admission) == admission_group(admission))
        .count()
        + 1;
    match admission {
        Some(PromptAdmission::Queue) | None if position == 1 => "Up next ".into(),
        Some(PromptAdmission::Queue) | None => format!("Up next {position} "),
        Some(PromptAdmission::Steer) if position == 1 => "Guide ".into(),
        Some(PromptAdmission::Steer) => format!("Guide {position} "),
        Some(PromptAdmission::Interrupt) => "Replacing ".into(),
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

    #[test]
    fn height_is_bounded() {
        assert_eq!(height(0), 0);
        assert_eq!(height(1), 3);
        assert_eq!(height(4), 6);
        assert_eq!(height(20), 6);
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
        let entries = [QueueEntry {
            id: QueueItemId::new(),
            text: Cow::Borrowed("queued prompt"),
            color: theme::current().foreground,
            editable: true,
            movable: false,
            admission: Some(PromptAdmission::Queue),
        }];
        let backend = TestBackend::new(60, 3);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut hits = Vec::new();
        terminal
            .draw(|frame| {
                hits = view(
                    frame,
                    frame.area(),
                    "Queue - Main",
                    &entries,
                    QueuePanelState {
                        together: Some(false),
                        ..QueuePanelState::default()
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer.cell((0, 0)).unwrap().symbol(), "┃");
        assert_eq!(buffer.cell((3, 1)).unwrap().symbol(), "U");
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
    fn action_hover_reverses_label_without_leading_space() {
        let id = QueueItemId::new();
        let entries = [QueueEntry {
            id,
            text: Cow::Borrowed("queued prompt"),
            color: theme::current().foreground,
            editable: true,
            movable: false,
            admission: Some(PromptAdmission::Queue),
        }];
        let target = QueueHitTarget::Item {
            id,
            action: QueueAction::Edit,
        };
        let backend = TestBackend::new(60, 3);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut hits = Vec::new();
        terminal
            .draw(|frame| {
                hits = view(
                    frame,
                    frame.area(),
                    "Queue - Main",
                    &entries,
                    QueuePanelState {
                        focus: Some(0),
                        hovered: Some(target),
                        ..QueuePanelState::default()
                    },
                );
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
                .cell((hit.area.x - 1, hit.area.y))
                .unwrap()
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn narrow_queue_drops_actions_that_cannot_match_their_hit_area() {
        let id = QueueItemId::new();
        let entries = [QueueEntry {
            id,
            text: Cow::Borrowed("queued prompt"),
            color: theme::current().foreground,
            editable: true,
            movable: false,
            admission: Some(PromptAdmission::Queue),
        }];
        let backend = TestBackend::new(16, 3);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut hits = Vec::new();
        terminal
            .draw(|frame| {
                hits = view(
                    frame,
                    frame.area(),
                    "Queue - Main",
                    &entries,
                    QueuePanelState {
                        focus: Some(0),
                        ..QueuePanelState::default()
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = (0..buffer.area.width)
            .map(|x| buffer.cell((x, 1)).unwrap().symbol())
            .collect::<String>();

        assert!(!row.contains(EDIT_LABEL_COMPACT));
        let delete = hits
            .iter()
            .find(|hit| {
                matches!(
                    hit.target,
                    QueueHitTarget::Item {
                        id: hit_id,
                        action: QueueAction::Delete,
                    } if hit_id == id
                )
            })
            .unwrap();
        let rendered_delete = (delete.area.x..delete.area.right())
            .map(|x| buffer.cell((x, delete.area.y)).unwrap().symbol())
            .collect::<String>();
        assert_eq!(rendered_delete, DELETE_LABEL_COMPACT);
    }
}
