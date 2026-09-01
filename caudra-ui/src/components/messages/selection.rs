use super::segment::SegmentCache;
use crate::selection::{self, LineBreaks, ScreenSelection, Selection};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Widget, Wrap};

pub(super) fn extract_selection_text(
    cache: &SegmentCache,
    viewport_width: u16,
    sel: &Selection,
    msg_area: Rect,
) -> String {
    let (doc_start, doc_end) = sel.normalized();
    let width = viewport_width;

    let heights: Vec<u16> = cache.segments().iter().map(|s| s.height(width)).collect();

    let mut out = String::new();
    let mut doc_row: u32 = 0;

    for (i, &h) in heights.iter().enumerate() {
        let seg_start = doc_row;
        let seg_end = doc_row + h as u32;
        doc_row = seg_end;

        if seg_end <= doc_start.row || seg_start > doc_end.row {
            continue;
        }

        let Some(seg) = cache.get(i) else { continue };

        if seg.lines().is_empty() {
            continue;
        }

        // `h` is the document layout height, which can predate a resize (see
        // `Segment::height`), while the wrap below happens at the real width.
        // The rows we copy come from that wrap, so measure and clamp against
        // it, and take the whole segment whenever the selection covers it.
        let drawn = seg.drawn_height(width);
        let chrome = seg.chrome(width);
        let content_width = chrome.content_width(width);
        let content_height = seg.content_height(width);
        if content_width == 0 || content_height == 0 {
            continue;
        }
        let content_start = chrome.content_start();
        let content_end = content_start.saturating_add(content_height);
        let selected_start = (doc_start.row.saturating_sub(seg_start) as u16).min(drawn);
        let selected_end = if doc_end.row + 1 >= seg_end {
            drawn
        } else {
            ((doc_end.row + 1 - seg_start) as u16).min(drawn)
        };
        let selected_start = selected_start.max(content_start);
        let selected_end = selected_end.min(content_end);
        if selected_start >= selected_end {
            continue;
        }
        let rel_start = selected_start - content_start;
        let rel_end = selected_end - content_start;
        let content_doc_start = seg_start + content_start as u32;
        let content_doc_end = seg_start + content_end as u32;

        let start_col = if content_doc_start > doc_start.row {
            0
        } else {
            doc_start
                .col
                .saturating_sub(msg_area.x.saturating_add(chrome.left))
        };
        let end_col = if content_doc_end < doc_end.row + 1 {
            content_width.saturating_sub(1)
        } else {
            doc_end
                .col
                .saturating_sub(msg_area.x.saturating_add(chrome.left))
        };

        let ss = ScreenSelection {
            start_row: rel_start,
            start_col,
            end_row: rel_end.saturating_sub(1),
            end_col,
        };

        // Markdown segments copy their source. Everything else (tool buffers,
        // images, plain text) has no ranges to read, so it scrapes cells.
        if let Some(text) = seg
            .provenance()
            .and_then(|p| p.extract(seg.lines(), content_width, &ss, rel_start, rel_end))
        {
            append_segment(&mut out, &text);
            continue;
        }

        let tmp_area = Rect::new(0, 0, content_width, content_height);
        let mut tmp = Buffer::empty(tmp_area);
        Paragraph::new(seg.lines().to_vec())
            .wrap(Wrap { trim: false })
            .render(tmp_area, &mut tmp);

        let breaks = LineBreaks::from_lines(seg.lines(), content_width);
        let mut text = String::new();
        selection::append_rows(&tmp, tmp_area, &ss, rel_start, rel_end, &mut text, &breaks);
        append_segment(&mut out, &text);
    }
    out
}

fn append_segment(out: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(text);
}
