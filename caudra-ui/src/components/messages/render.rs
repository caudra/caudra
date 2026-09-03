use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::components::hover_style;
use crate::components::tool_display::{FILTERED_AFFORDANCE, RAW_AFFORDANCE};
use crate::markdown::{LinkMap, TerminalLink};

use super::layout::SegmentChrome;

pub(super) use crate::markdown::EXPAND_AFFORDANCE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HoverFeedback {
    Affordance,
    /// Only the raw/filtered switch, so hovering it does not also light up an
    /// expand affordance elsewhere on the same card.
    ShellToggle,
    /// One body line, by its index. A batch child is a control in its own
    /// right, so it marks itself rather than the card around it.
    Row(usize),
    Chrome,
}

impl HoverFeedback {
    fn needles(self) -> &'static [&'static str] {
        match self {
            Self::Affordance => &[EXPAND_AFFORDANCE],
            Self::ShellToggle => &[RAW_AFFORDANCE, FILTERED_AFFORDANCE],
            Self::Row(_) | Self::Chrome => &[],
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct RenderFeedback {
    pub highlight: bool,
    pub hover: Option<(HoverFeedback, Color)>,
}

pub(super) struct RenderCursor {
    skip: u16,
    y: u16,
    bottom: u16,
    viewport: Rect,
    terminal_links: Vec<TerminalLink>,
}

impl RenderCursor {
    pub fn new(scroll_top: u16, viewport: Rect, terminal_links: Vec<TerminalLink>) -> Self {
        Self {
            skip: scroll_top,
            y: viewport.y,
            bottom: viewport.y + viewport.height,
            viewport,
            terminal_links,
        }
    }

    pub fn past_bottom(&self) -> bool {
        self.y >= self.bottom
    }

    pub fn into_terminal_links(self) -> Vec<TerminalLink> {
        self.terminal_links
    }

    /// Advances past rows that are part of the layout but have nothing to
    /// draw, which is what keeps a closed card from pulling the transcript
    /// up under the reader.
    pub fn skip_rows(&mut self, rows: u16) {
        if rows == 0 {
            return;
        }
        if self.skip >= rows {
            self.skip -= rows;
            return;
        }
        let visible = rows
            .saturating_sub(self.skip)
            .min(self.bottom.saturating_sub(self.y));
        self.skip = 0;
        self.y += visible;
    }

    pub fn render(
        &mut self,
        content: (&[Line<'static>], Option<&LinkMap>),
        h: u16,
        chrome: SegmentChrome,
        styles: (Option<Style>, Option<Style>),
        feedback: RenderFeedback,
        frame: &mut Frame,
    ) {
        let (lines, links) = content;
        if self.skip >= h {
            self.skip -= h;
            return;
        }
        if self.y >= self.bottom {
            return;
        }
        let (style, rail_style) = styles;
        let skipped = self.skip;
        let visible_h = h
            .saturating_sub(skipped)
            .min(self.bottom.saturating_sub(self.y));
        let seg_area = Rect::new(self.viewport.x, self.y, self.viewport.width, visible_h);
        let mut base = style.unwrap_or_default();
        if feedback.highlight {
            base = base.add_modifier(Modifier::REVERSED);
        }

        let card_start = chrome.margin_top;
        let card_visible_start = skipped.max(card_start);
        let card_visible_end = skipped.saturating_add(visible_h).min(h);
        if card_visible_start < card_visible_end {
            let card_area = Rect::new(
                seg_area.x,
                seg_area.y + card_visible_start - skipped,
                seg_area.width,
                card_visible_end - card_visible_start,
            );
            if style.is_some() || feedback.highlight {
                frame.render_widget(Block::default().style(base), card_area);
            }
            if chrome.rail {
                let rail = match feedback.hover {
                    Some((HoverFeedback::Chrome, accent)) => {
                        rail_style.unwrap_or_default().fg(accent)
                    }
                    _ => rail_style.unwrap_or_default(),
                };
                let buffer = frame.buffer_mut();
                for y in card_area.y..card_area.bottom() {
                    if let Some(cell) = buffer.cell_mut((card_area.x, y)) {
                        cell.set_char('┃').set_style(rail);
                    }
                }
            }
        }

        let content_start = chrome.content_start();
        let content_end = h.saturating_sub(chrome.bottom);
        let content_visible_start = skipped.max(content_start);
        let content_visible_end = skipped.saturating_add(visible_h).min(content_end);
        if content_visible_start < content_visible_end {
            let content_area = Rect::new(
                seg_area.x.saturating_add(chrome.left),
                seg_area.y + content_visible_start - skipped,
                chrome.content_width(seg_area.width),
                content_visible_end - content_visible_start,
            );
            let paragraph = Paragraph::new(hover_lines(lines, feedback.hover))
                .wrap(Wrap { trim: false })
                .style(base)
                .scroll((content_visible_start - content_start, 0));
            frame.render_widget(paragraph, content_area);
            if let Some(links) = links {
                links.append_terminal_links(
                    lines,
                    content_area.width,
                    content_visible_start - content_start,
                    content_area,
                    &mut self.terminal_links,
                );
            }
        }
        self.skip = 0;
        self.y += visible_h;
    }
}

fn hover_lines(
    lines: &[Line<'static>],
    hover: Option<(HoverFeedback, Color)>,
) -> Vec<Line<'static>> {
    let mut lines = lines.to_vec();
    match hover {
        Some((feedback @ (HoverFeedback::Affordance | HoverFeedback::ShellToggle), _)) => {
            for line in &mut lines {
                let mut spans = Vec::with_capacity(line.spans.len());
                for span in line.spans.drain(..) {
                    spans.extend(reverse_affordance(span, feedback.needles()));
                }
                line.spans = spans;
            }
        }
        Some((HoverFeedback::Row(line), _)) => {
            if let Some(row) = lines.get_mut(line) {
                row.style = hover_style(row.style, true);
                for span in &mut row.spans {
                    span.style = hover_style(span.style, true);
                }
            }
        }
        Some((HoverFeedback::Chrome, accent)) => {
            if let Some(header) = lines.first_mut() {
                header.style = header.style.fg(accent);
                for span in &mut header.spans {
                    span.style = span.style.fg(accent);
                }
            }
        }
        None => {}
    }
    lines
}

fn reverse_affordance(span: Span<'static>, needles: &[&str]) -> Vec<Span<'static>> {
    let style = span.style;
    let text = span.content.into_owned();
    let Some((start, needle)) = needles
        .iter()
        .find_map(|needle| Some((text.find(needle)?, *needle)))
    else {
        return vec![Span::styled(text, style)];
    };
    let end = start + needle.len();
    let mut spans = Vec::with_capacity(3);
    if start > 0 {
        spans.push(Span::styled(text[..start].to_owned(), style));
    }
    spans.push(Span::styled(
        text[start..end].to_owned(),
        style.add_modifier(Modifier::REVERSED),
    ));
    if end < text.len() {
        spans.push(Span::styled(text[end..].to_owned(), style));
    }
    spans
}
