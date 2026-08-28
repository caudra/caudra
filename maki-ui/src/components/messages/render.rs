use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph, Wrap};

use super::layout::SegmentChrome;

pub(super) struct RenderCursor {
    skip: u16,
    y: u16,
    bottom: u16,
    viewport: Rect,
}

impl RenderCursor {
    pub fn new(scroll_top: u16, viewport: Rect) -> Self {
        Self {
            skip: scroll_top,
            y: viewport.y,
            bottom: viewport.y + viewport.height,
            viewport,
        }
    }

    pub fn past_bottom(&self) -> bool {
        self.y >= self.bottom
    }

    pub fn render(
        &mut self,
        lines: &[Line<'static>],
        h: u16,
        chrome: SegmentChrome,
        styles: (Option<Style>, Option<Style>),
        highlight: bool,
        frame: &mut Frame,
    ) {
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
        if highlight {
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
            if style.is_some() || highlight {
                frame.render_widget(Block::default().style(base), card_area);
            }
            if chrome.rail {
                let rail = rail_style.unwrap_or_default();
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
            let paragraph = Paragraph::new(lines.to_vec())
                .wrap(Wrap { trim: false })
                .style(base)
                .scroll((content_visible_start - content_start, 0));
            frame.render_widget(paragraph, content_area);
        }
        self.skip = 0;
        self.y += visible_h;
    }
}
