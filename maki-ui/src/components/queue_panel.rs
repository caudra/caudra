use std::borrow::Cow;

use maki_agent::QueueItemId;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::theme;

const DELETE_LABEL: &str = "[Delete]";
const DELETE_LABEL_COMPACT: &str = "[D]";
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
    focus: Option<usize>,
    viewport: usize,
    together: Option<bool>,
) -> Vec<QueueHit> {
    if entries.is_empty() || area.width < 2 || area.height < 2 {
        return Vec::new();
    }

    let visible_rows = usize::from(area.height.saturating_sub(2));
    let viewport = viewport.min(entries.len().saturating_sub(visible_rows));
    let end = (viewport + visible_rows).min(entries.len());
    let content_width = area.width.saturating_sub(2) as usize;
    let mut hits = Vec::new();
    let lines = entries[viewport..end]
        .iter()
        .enumerate()
        .map(|(visible_index, entry)| {
            let index = viewport + visible_index;
            let selected = focus == Some(index);
            let row = area.y + 1 + visible_index as u16;
            let row_area = Rect::new(area.x + 1, row, area.width.saturating_sub(2), 1);
            hits.push(QueueHit {
                area: row_area,
                target: QueueHitTarget::Item {
                    id: entry.id,
                    action: QueueAction::Select,
                },
            });

            let actions = if selected {
                actions(entry, content_width < 48)
            } else {
                Vec::new()
            };
            let action_width = actions
                .iter()
                .map(|(label, _)| label.width() + 1)
                .sum::<usize>();
            let prefix = if index == 0 {
                "Next ".to_string()
            } else {
                format!("{} ", index + 1)
            };
            let available = content_width
                .saturating_sub(prefix.width())
                .saturating_sub(action_width);
            let style = if selected {
                theme::current().item_selected
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

            let mut action_x = area.right().saturating_sub(1 + action_width as u16);
            for (label, action) in actions {
                spans.push(Span::raw(" "));
                let action_style = if action == QueueAction::Delete {
                    theme::current().queue_delete
                } else {
                    theme::current().keybind_key
                };
                spans.push(Span::styled(label, action_style));
                let width = label.width() as u16;
                action_x = action_x.saturating_add(1);
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

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if focus.is_some() {
            theme::current().item_selected
        } else {
            theme::current().panel_border
        })
        .title_top(Line::from(format!(" {title} ({}) ", entries.len())).left_aligned())
        .title_style(theme::current().panel_title);
    if let Some(together) = together {
        let label = if together {
            TOGETHER_LABEL
        } else {
            SEPARATE_LABEL
        };
        let width = label.width() as u16;
        if area.width > width + 4 {
            block = block.title_top(
                Line::from(Span::styled(label, theme::current().keybind_key)).right_aligned(),
            );
            hits.push(QueueHit {
                area: Rect::new(area.right().saturating_sub(width + 1), area.y, width, 1),
                target: QueueHitTarget::ToggleTogether,
            });
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::new().fg(theme::current().foreground))
            .block(block),
        area,
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
}
