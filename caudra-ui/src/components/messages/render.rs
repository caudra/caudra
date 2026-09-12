use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::components::hover_style;
use crate::components::tool_display::{FILTERED_AFFORDANCE, RAW_AFFORDANCE};
use crate::markdown::{LinkMap, TerminalLink};
use crate::theme;

use super::layout::SegmentChrome;

pub(super) use crate::markdown::EXPAND_AFFORDANCE;

pub(super) const MESSAGE_ACTION_GLYPH: &str = "⋮";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HoverFeedback {
    Affordance,
    /// Only the raw/filtered switch, so hovering it does not also light up an
    /// expand affordance elsewhere on the same card.
    ShellToggle,
    /// The row that names a control nested inside a card, by its line index. A
    /// batch child is a control in its own right, so it marks itself rather
    /// than the card around it, the same way a card marks its own header.
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
    pub message_action: Option<(bool, Color)>,
}

/// Where a segment landed on screen, for placing anything that has to line up
/// with its rows.
#[derive(Clone, Copy)]
pub(super) struct Placement {
    /// Screen row of the segment's first visible row.
    pub y: u16,
    /// Rows of the segment above the viewport.
    pub skipped: u16,
    pub visible: u16,
}

impl Placement {
    /// The screen rows a run of segment rows takes, clipped to what is on
    /// screen. `None` when the run is entirely scrolled out.
    pub fn clip(self, start: u16, span: u16) -> Option<(u16, u16)> {
        let top = start.max(self.skipped);
        let bottom = start.saturating_add(span).min(self.skipped + self.visible);
        (top < bottom).then(|| (self.y + top - self.skipped, bottom - top))
    }
}

pub(super) struct RenderCursor {
    skip: u32,
    y: u16,
    bottom: u16,
    viewport: Rect,
    terminal_links: Vec<TerminalLink>,
}

impl RenderCursor {
    pub fn new(scroll_top: u32, viewport: Rect, terminal_links: Vec<TerminalLink>) -> Self {
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

    /// Where a segment of height `h` is about to land: the screen row its
    /// first visible row takes, how much of it is above the viewport, and how
    /// many of its rows show. `None` when none of it does.
    ///
    /// [`Self::render`] places itself through this, so anything drawn beside a
    /// segment agrees with it by construction rather than by two copies of the
    /// same arithmetic staying in step.
    pub fn placement(&self, h: u16) -> Option<Placement> {
        if self.skip >= u32::from(h) || self.y >= self.bottom {
            return None;
        }
        let skipped = self.skip.min(u32::from(h)) as u16;
        Some(Placement {
            y: self.y,
            skipped,
            visible: h
                .saturating_sub(skipped)
                .min(self.bottom.saturating_sub(self.y)),
        })
    }

    /// Consumes a segment sitting entirely above the viewport, reporting
    /// whether it did. Lets a caller drop the per-segment hover and action
    /// lookups for rows that will never be painted, which is most of them in
    /// a long transcript.
    pub fn skip_above(&mut self, h: u16) -> bool {
        let h = u32::from(h);
        if self.skip >= h {
            self.skip -= h;
            return true;
        }
        false
    }

    pub fn into_terminal_links(self) -> Vec<TerminalLink> {
        self.terminal_links
    }

    pub fn render(
        &mut self,
        content: (&[Line<'static>], Option<&LinkMap>),
        h: u16,
        chrome: SegmentChrome,
        styles: (Option<Style>, Option<Style>),
        feedback: RenderFeedback,
        frame: &mut Frame,
    ) -> Option<Rect> {
        let (lines, links) = content;
        let placement = self.placement(h);
        if self.skip_above(h) {
            return None;
        }
        let Placement {
            skipped,
            visible: visible_h,
            ..
        } = placement?;
        let (style, rail_style) = styles;
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
                let rail = match (feedback.hover, feedback.message_action) {
                    (Some((HoverFeedback::Chrome, accent)), _) | (_, Some((true, accent))) => {
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
        let mut message_action_hit = None;
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
            if let (Some((hovered, _)), Some(offset)) =
                (feedback.message_action, chrome.action_offset())
            {
                let area = Rect::new(seg_area.x, content_area.y, chrome.left, content_area.height);
                if let Some(cell) = frame
                    .buffer_mut()
                    .cell_mut((area.x.saturating_add(offset), area.y))
                {
                    cell.set_symbol(MESSAGE_ACTION_GLYPH)
                        .set_style(hover_style(theme::current().tool_dim, hovered));
                    message_action_hit = Some(area);
                }
            }
        }
        self.skip = 0;
        self.y += visible_h;
        message_action_hit
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
        Some((HoverFeedback::Row(line), accent)) => tint_line(lines.get_mut(line), accent),
        Some((HoverFeedback::Chrome, accent)) => tint_line(lines.first_mut(), accent),
        None => {}
    }
    lines
}

/// How a control says the pointer is on it when the control is a whole card or
/// a whole child: the row that names it takes the accent. That row is the
/// label and not the thing, so reversing it would shout about one line while
/// the press acts on everything under it.
fn tint_line(line: Option<&mut Line<'static>>, accent: Color) {
    let Some(line) = line else {
        return;
    };
    line.style = line.style.fg(accent);
    for span in &mut line.spans {
        span.style = span.style.fg(accent);
    }
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
