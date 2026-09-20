use crate::theme::Theme;

use caudra_grab::grab_scope;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

/// A blank row and the footer row itself, so scrolling content never reaches
/// the line the footer is drawn on.
const FOOTER_ROWS: u16 = 2;

/// Draws the form's chrome and body, and returns where the footer landed so a
/// caller that makes its footer clickable can hit test it. The rect is empty
/// when there is no footer.
pub(crate) fn render_form(
    t: &Theme,
    title: &str,
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    scroll: (u16, u16),
    footer: Option<Line<'static>>,
) -> Rect {
    grab_scope!("form", area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(t.panel_border)
        .title_top(Line::from(title.to_string()).left_aligned())
        .title_style(t.panel_title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let reserved = if footer.is_some() { FOOTER_ROWS } else { 0 };
    let [body, _] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(reserved.min(inner.height)),
    ])
    .areas(inner);

    let paragraph = Paragraph::new(lines)
        .style(Style::new().fg(t.foreground))
        .wrap(Wrap { trim: false })
        .scroll(scroll);
    frame.render_widget(paragraph, body);

    let line = if footer.is_some() {
        footer_row(area)
    } else {
        Rect::default()
    };
    if let Some(footer) = footer {
        frame.render_widget(Paragraph::new(footer), line);
    }
    line
}

/// The row [`render_form`] draws a footer on, known before drawing so a footer
/// that records its own geometry can be built for the row it will land on.
pub(crate) fn footer_row(area: Rect) -> Rect {
    let inner = Block::bordered().inner(area);
    Rect {
        y: inner.bottom().saturating_sub(1),
        height: inner.height.min(1),
        ..inner
    }
}

pub(crate) fn selected_prefix(t: &Theme, is_selected: bool) -> (&'static str, Style) {
    if is_selected {
        ("▸ ", t.active)
    } else {
        ("  ", Style::new().fg(t.foreground))
    }
}
