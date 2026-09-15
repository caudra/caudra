use super::fenced_text;
use super::layout::SegmentKind;
use super::segment::{Segment, SegmentCache};
use crate::provenance::Provenance;
use crate::selection::{self, LineBreaks, ScreenSelection, Selection};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Widget, Wrap};

pub(super) struct SelectionFragment {
    pub kind: SegmentKind,
    pub msg_index: Option<usize>,
    pub tool_id: Option<String>,
    pub text: String,
    /// The language of the code this fragment is made of, when that is all it
    /// holds, so the markdown form can name the fence it wraps it in.
    pub language: Option<String>,
}

pub(super) fn extract_selection_fragments(
    cache: &SegmentCache,
    viewport_width: u16,
    sel: &Selection,
    msg_area: Rect,
) -> Vec<SelectionFragment> {
    let (doc_start, doc_end) = sel.normalized();
    let heights: Vec<u16> = cache
        .segments()
        .iter()
        .map(|segment| segment.height(viewport_width))
        .collect();
    let mut fragments = Vec::new();
    let mut doc_row: u32 = 0;

    for (i, &h) in heights.iter().enumerate() {
        let seg_start = doc_row;
        let seg_end = doc_row + h as u32;
        doc_row = seg_end;

        if seg_end <= doc_start.row || seg_start > doc_end.row {
            continue;
        }

        let Some(segment) = cache.get(i) else {
            continue;
        };
        if let Some(fragment) =
            extract_segment_fragment(segment, seg_start, viewport_width, sel, msg_area)
        {
            fragments.push(fragment);
        }
    }
    fragments
}

pub(super) fn extract_segment_fragment(
    segment: &Segment,
    segment_start: u32,
    viewport_width: u16,
    sel: &Selection,
    msg_area: Rect,
) -> Option<SelectionFragment> {
    let (doc_start, doc_end) = sel.normalized();
    let segment_height = segment.height(viewport_width);
    let segment_end = segment_start + u32::from(segment_height);
    if segment_end <= doc_start.row || segment_start > doc_end.row || segment.lines().is_empty() {
        return None;
    }

    // The document layout height can predate a resize, while extraction wraps
    // at the real width. Clamp against the rows that would actually be drawn.
    let drawn = segment.drawn_height(viewport_width);
    let chrome = segment.chrome(viewport_width);
    let content_width = chrome.content_width(viewport_width);
    let content_height = segment.content_height(viewport_width);
    if content_width == 0 || content_height == 0 {
        return None;
    }
    let content_start = chrome.content_start();
    let content_end = content_start.saturating_add(content_height);
    let selected_start = (doc_start.row.saturating_sub(segment_start) as u16).min(drawn);
    let selected_end = if doc_end.row + 1 >= segment_end {
        drawn
    } else {
        ((doc_end.row + 1 - segment_start) as u16).min(drawn)
    };
    let selected_start = selected_start.max(content_start);
    let selected_end = selected_end.min(content_end);
    if selected_start >= selected_end {
        return None;
    }
    let rel_start = selected_start - content_start;
    let rel_end = selected_end - content_start;
    let content_doc_start = segment_start + u32::from(content_start);
    let content_doc_end = segment_start + u32::from(content_end);

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

    let screen_selection = ScreenSelection {
        start_row: rel_start,
        start_col,
        end_row: rel_end.saturating_sub(1),
        end_col,
    };

    let (text, language) = segment
        .provenance()
        .and_then(|provenance| {
            extract_with_code_block(
                segment,
                provenance,
                viewport_width,
                content_width,
                &screen_selection,
                rel_start,
                rel_end,
            )
        })
        .unwrap_or_else(|| {
            let area = Rect::new(0, 0, content_width, content_height);
            let mut buffer = Buffer::empty(area);
            Paragraph::new(segment.lines().to_vec())
                .wrap(Wrap { trim: false })
                .render(area, &mut buffer);

            let breaks = LineBreaks::from_lines(segment.lines(), content_width);
            let mut text = String::new();
            selection::append_rows(
                &buffer,
                area,
                &screen_selection,
                rel_start,
                rel_end,
                &mut text,
                &breaks,
            );
            (text, None)
        });
    Some(SelectionFragment {
        kind: segment.kind(),
        msg_index: segment.msg_index,
        tool_id: segment.tool_id.clone(),
        text,
        language,
    })
}

/// Copies the selected rows, keeping a card's code apart from whatever else
/// the selection swept up.
///
/// A selection that stayed inside one block is that code and nothing else, so
/// it copies raw and names its language for the caller to fence. One that ran
/// past a block is a mixture, and every block it touched is fenced where it
/// sits so the prose beside it does not read as more of the script. A batch
/// card carries a block per child, which is why the walk is over all of them
/// rather than the first.
fn extract_with_code_block(
    segment: &Segment,
    provenance: &Provenance,
    viewport_width: u16,
    content_width: u16,
    sel: &ScreenSelection,
    rel_start: u16,
    rel_end: u16,
) -> Option<(String, Option<String>)> {
    let rows = |from, to| provenance.extract(segment.lines(), content_width, sel, from, to);
    let blocks = segment.code_blocks(viewport_width);
    if blocks.is_empty() {
        return Some((rows(rel_start, rel_end)?, None));
    }

    let mut parts: Vec<String> = Vec::new();
    let mut at = rel_start;
    let mut fenced = 0usize;
    let mut lone_language = None;
    for (block, language) in blocks {
        let first = block.start.clamp(at, rel_end);
        let last = block.end.clamp(first, rel_end);
        let code = rows(first, last)?;
        if code.is_empty() {
            continue;
        }
        parts.push(rows(at, first)?);
        parts.push(fenced_text(&code, language));
        fenced += 1;
        lone_language = language.map(str::to_owned);
        at = last;
    }
    parts.push(rows(at, rel_end)?);

    let prose_free = parts.iter().filter(|part| !part.is_empty()).count() == 1;
    if fenced == 1 && prose_free {
        // The selection is one block and nothing else, so the caller fences it
        // with the rest of the card rather than having it fenced in place.
        return Some((rows(rel_start, rel_end)?, lone_language));
    }
    let text = parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Some((text, None))
}

pub(super) fn join_fragments(fragments: &[SelectionFragment]) -> String {
    let mut out = String::new();
    for fragment in fragments {
        append_segment(&mut out, &fragment.text);
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
