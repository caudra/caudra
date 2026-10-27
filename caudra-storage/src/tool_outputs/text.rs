//! Paging, searching, and previewing one saved output's text. A line is what
//! lies between `\n`s with one trailing `\r` dropped, and an empty final line
//! does not count.

use std::collections::VecDeque;
use std::ops::Range;

use regex::Regex;

use super::{
    ToolOutputError, ToolOutputGrepResult, ToolOutputGrepRow, ToolOutputPreview,
    ToolOutputReadResult, line_count,
};

const MAX_READ_LINES: usize = 2_000;
const MAX_READ_BYTES: usize = 50 * 1024;
const MAX_DISPLAY_LINE_BYTES: usize = 2_000;
const GREP_FORMAT_RESERVE_BYTES: usize = 8 * 1024;
const GREP_ROW_BYTES: usize = MAX_READ_BYTES - GREP_FORMAT_RESERVE_BYTES;
const MAX_GREP_ROWS: usize = MAX_READ_LINES - 2;
const OMITTED: &str = "[...]";

/// Which matches a search returns: from the 0-indexed line `start`, at most
/// `limit` of them, each with up to `before` and `after` lines of context.
pub(super) struct GrepWindow {
    pub(super) start: usize,
    pub(super) limit: usize,
    pub(super) before: usize,
    pub(super) after: usize,
}

struct GrepRow<'a> {
    index: usize,
    line: &'a str,
    matched: Option<Range<usize>>,
}

fn lines(text: &str) -> impl Iterator<Item = &str> {
    let body = text.strip_suffix('\n').unwrap_or(text);
    (!text.is_empty())
        .then(|| body.split('\n'))
        .into_iter()
        .flatten()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
}

/// Up to `limit` lines from the 1-indexed `offset`, the first starting at
/// `byte_offset`. A line too long for the page ends it, with the byte to
/// resume from.
pub(super) fn read_page(
    text: &str,
    offset: usize,
    limit: usize,
    byte_offset: usize,
) -> Result<ToolOutputReadResult, ToolOutputError> {
    let start = offset - 1;
    let limit = limit.min(MAX_READ_LINES);
    let mut page = String::new();
    let mut returned_lines = 0;
    let mut next_offset = None;
    let mut next_byte_offset = 0;
    for (index, line) in lines(text).enumerate().skip(start) {
        if returned_lines >= limit {
            next_offset = Some(index + 1);
            break;
        }
        let from = if index == start { byte_offset } else { 0 };
        if from > line.len() || !line.is_char_boundary(from) {
            return Err(ToolOutputError::InvalidByteOffset {
                line_number: index + 1,
                byte_offset: from,
            });
        }
        let separator = usize::from(returned_lines > 0);
        let capacity = MAX_READ_BYTES
            .saturating_sub(page.len() + separator)
            .min(MAX_DISPLAY_LINE_BYTES);
        let end = line.floor_char_boundary(from.saturating_add(capacity).min(line.len()));
        if end == from && from < line.len() {
            next_offset = Some(index + 1);
            next_byte_offset = from;
            break;
        }
        if returned_lines > 0 {
            page.push('\n');
        }
        page.push_str(&line[from..end]);
        returned_lines += 1;
        if end < line.len() {
            next_offset = Some(index + 1);
            next_byte_offset = end;
            break;
        }
    }
    Ok(ToolOutputReadResult {
        text: page,
        offset,
        byte_offset,
        returned_lines,
        next_offset,
        next_byte_offset,
        total_lines: line_count(text),
        total_bytes: text.len(),
    })
}

/// The lines `regex` matches inside `window`, with their context, within the
/// row and byte budget of one page. `next_offset` names the first match the
/// page left out.
pub(super) fn grep(text: &str, regex: &Regex, window: GrepWindow) -> ToolOutputGrepResult {
    let mut rows: Vec<GrepRow<'_>> = Vec::new();
    let mut matches = Vec::new();
    let mut previous = VecDeque::with_capacity(window.before + 1);
    let mut remaining_after = 0;
    let mut next_match = None;
    for (index, line) in lines(text).enumerate() {
        let matched = (index >= window.start)
            .then(|| regex.find(line).map(|found| found.range()))
            .flatten();
        if let Some(matched) = matched {
            if matches.len() >= window.limit {
                next_match = Some(index);
                break;
            }
            for &(index, line) in &previous {
                push(
                    &mut rows,
                    GrepRow {
                        index,
                        line,
                        matched: None,
                    },
                );
            }
            push(
                &mut rows,
                GrepRow {
                    index,
                    line,
                    matched: Some(matched),
                },
            );
            matches.push(index);
            remaining_after = window.after;
        } else if remaining_after > 0 {
            push(
                &mut rows,
                GrepRow {
                    index,
                    line,
                    matched: None,
                },
            );
            remaining_after -= 1;
        }
        previous.push_back((index, line));
        if previous.len() > window.before {
            previous.pop_front();
        }
    }

    let mut bounded = Vec::with_capacity(rows.len().min(MAX_GREP_ROWS));
    let mut bytes = 0;
    let mut budget_omitted = None;
    for row in &rows {
        let line_number = row.index + 1;
        let text = excerpt(row.line, row.matched.as_ref());
        let row_bytes = line_number.to_string().len() + 2 + text.len();
        let separator = usize::from(!bounded.is_empty());
        if bounded.len() >= MAX_GREP_ROWS || bytes + separator + row_bytes > GREP_ROW_BYTES {
            budget_omitted = matches.iter().copied().find(|index| *index >= row.index);
            break;
        }
        bytes += separator + row_bytes;
        bounded.push(ToolOutputGrepRow {
            line_number,
            text,
            is_match: row.matched.is_some(),
        });
    }
    let first_omitted = match (budget_omitted, next_match) {
        (Some(budget), Some(next)) => Some(budget.min(next)),
        (budget, next) => budget.or(next),
    };
    if let Some(omitted) = first_omitted {
        bounded.retain(|row| row.line_number <= omitted);
    }
    ToolOutputGrepResult {
        rows: bounded,
        next_offset: first_omitted.map(|index| index + 1),
    }
}

/// Context lines can overlap the rows already taken, which keep their place.
fn push<'a>(rows: &mut Vec<GrepRow<'a>>, row: GrepRow<'a>) {
    if rows.last().is_none_or(|last| last.index < row.index) {
        rows.push(row);
    }
}

/// `line` whole when it fits a display row, otherwise the part around
/// `matched`, with markers where text was cut.
fn excerpt(line: &str, matched: Option<&Range<usize>>) -> String {
    if line.len() <= MAX_DISPLAY_LINE_BYTES {
        return line.to_owned();
    }
    let content_bytes = MAX_DISPLAY_LINE_BYTES - OMITTED.len() * 2;
    let window = |start: usize| {
        let start = line.ceil_char_boundary(start);
        (
            start,
            line.floor_char_boundary((start + content_bytes).min(line.len())),
        )
    };
    let centered = matched.map_or(0, |matched| {
        if matched.len() >= content_bytes {
            matched.start
        } else {
            matched
                .start
                .saturating_sub((content_bytes - matched.len()) / 2)
        }
    });
    let (mut start, mut end) = window(centered.min(line.len() - content_bytes));
    if let Some(matched) = matched
        && matched.len() <= content_bytes
        && end < matched.end
    {
        (start, end) = window(matched.end.saturating_sub(content_bytes));
    }
    let lead = if start > 0 { OMITTED } else { "" };
    let trail = if end < line.len() { OMITTED } else { "" };
    format!("{lead}{}{trail}", &line[start..end])
}

/// The first and last lines of `text`, each half read from at most half of
/// `max_bytes`.
pub(super) fn preview(text: &str, max_lines: usize, max_bytes: usize) -> ToolOutputPreview {
    let payload_bytes = max_bytes.saturating_sub(1);
    let head_end = text.floor_char_boundary(payload_bytes.div_ceil(2).min(text.len()));
    let tail_start = text.ceil_char_boundary(text.len() - (payload_bytes / 2).min(text.len()));
    ToolOutputPreview {
        head: head_lines(&text[..head_end], max_lines.div_ceil(2)).to_owned(),
        tail: tail_lines(&text[tail_start..], max_lines / 2).to_owned(),
    }
}

fn head_lines(text: &str, max_lines: usize) -> &str {
    if max_lines == 0 {
        return "";
    }
    let end = text
        .match_indices('\n')
        .nth(max_lines - 1)
        .map_or(text.len(), |(index, _)| index);
    text[..end].trim_end_matches(['\r', '\n'])
}

fn tail_lines(text: &str, max_lines: usize) -> &str {
    if max_lines == 0 {
        return "";
    }
    let text = text.strip_suffix('\n').unwrap_or(text);
    let start = text
        .rmatch_indices('\n')
        .nth(max_lines - 1)
        .map_or(0, |(index, _)| index + 1);
    text[start..].trim_matches(['\r', '\n'])
}

#[cfg(test)]
mod tests {
    use std::iter;

    use test_case::test_case;

    use super::*;

    const PAGED_TEXT: &str = "one\ntwo\nthree\nfour\n";
    const GREP_TEXT: &str = "before\nerror 42\nbetween\nerror 7\nafter\n";
    const HUGE_LINE_NEEDLE: &str = "needle-at-the-end";
    const HUGE_LINE_BYTES: usize = 32 * 1024 * 1024;
    const MAX_GREP_MATCHES: usize = 200;
    const MAX_GREP_CONTEXT: usize = 5;

    fn search(text: &str, pattern: &str, window: GrepWindow) -> ToolOutputGrepResult {
        grep(text, &Regex::new(pattern).unwrap(), window)
    }

    fn window(start: usize, limit: usize, context: usize) -> GrepWindow {
        GrepWindow {
            start,
            limit,
            before: context,
            after: context,
        }
    }

    fn numbered(rows: &[ToolOutputGrepRow]) -> Vec<(usize, bool)> {
        rows.iter()
            .map(|row| (row.line_number, row.is_match))
            .collect()
    }

    #[test_case("", 0 ; "empty")]
    #[test_case("x", 1 ; "unterminated")]
    #[test_case("x\n", 1 ; "trailing_lf")]
    #[test_case("x\r\n", 1 ; "trailing_crlf")]
    #[test_case("x\n\n", 2 ; "consecutive_blank_lines")]
    fn lines_agree_with_the_line_count(text: &str, expected: usize) {
        assert_eq!(line_count(text), expected);
        assert_eq!(lines(text).count(), expected);
    }

    #[test]
    fn read_respects_utf8_line_and_page_byte_caps() {
        let line = "é".repeat(MAX_DISPLAY_LINE_BYTES / 2);
        let text = iter::repeat_n(line, 30).collect::<Vec<_>>().join("\n");

        let result = read_page(&text, 1, MAX_READ_LINES, 0).unwrap();

        assert!(result.text.len() <= MAX_READ_BYTES);
        assert!(result.text.is_char_boundary(result.text.len()));
        assert!(
            result
                .text
                .lines()
                .all(|displayed| displayed.len() <= MAX_DISPLAY_LINE_BYTES)
        );
        assert_eq!(result.next_offset, Some(result.returned_lines));
        assert!(result.next_byte_offset > 0);
    }

    #[test]
    fn read_at_round_trips_a_long_utf8_line_without_skipping_bytes() {
        let text = format!("start-{}-end", "蟹".repeat(2_000));
        let mut offset = 1;
        let mut byte_offset = 0;
        let mut restored = String::new();

        loop {
            let page = read_page(&text, offset, 1, byte_offset).unwrap();
            restored.push_str(&page.text);
            let Some(next_offset) = page.next_offset else {
                break;
            };
            assert_eq!(next_offset, 1);
            assert!(page.next_byte_offset > byte_offset);
            assert!(text.is_char_boundary(page.next_byte_offset));
            offset = next_offset;
            byte_offset = page.next_byte_offset;
        }

        assert_eq!(restored, text);
    }

    #[test_case(3 ; "past_the_line")]
    #[test_case(1 ; "inside_a_character")]
    fn read_rejects_a_byte_offset_off_the_line(byte_offset: usize) {
        assert!(matches!(
            read_page("é", 1, 1, byte_offset),
            Err(ToolOutputError::InvalidByteOffset { line_number: 1, .. })
        ));
    }

    #[test]
    fn read_paginates_with_one_indexed_offsets() {
        let first = read_page(PAGED_TEXT, 2, 2, 0).unwrap();
        let second = read_page(PAGED_TEXT, first.next_offset.unwrap(), 2, 0).unwrap();

        assert_eq!(first.text, "two\nthree");
        assert_eq!(first.returned_lines, 2);
        assert_eq!(first.next_offset, Some(4));
        assert_eq!(first.total_lines, 4);
        assert_eq!(first.total_bytes, PAGED_TEXT.len());
        assert_eq!(second.text, "four");
        assert_eq!(second.next_offset, None);
    }

    #[test]
    fn read_and_grep_reach_the_end_of_a_maximum_sized_line() {
        let prefix_bytes = HUGE_LINE_BYTES - HUGE_LINE_NEEDLE.len();
        let text = "a".repeat(prefix_bytes) + HUGE_LINE_NEEDLE;

        let read = read_page(&text, 1, 1, prefix_bytes).unwrap();
        let found = search(&text, HUGE_LINE_NEEDLE, window(0, 1, 0));

        assert_eq!(read.text, HUGE_LINE_NEEDLE);
        assert_eq!(read.total_bytes, HUGE_LINE_BYTES);
        assert_eq!(read.total_lines, 1);
        assert_eq!(found.rows.len(), 1);
        assert!(found.rows[0].text.contains(HUGE_LINE_NEEDLE));
    }

    #[test]
    fn grep_supports_regex_context_and_pagination() {
        let first = search(GREP_TEXT, r"error \d+", window(0, 1, 1));
        let second = search(
            GREP_TEXT,
            r"error \d+",
            window(first.next_offset.unwrap() - 1, 1, 1),
        );

        assert_eq!(
            numbered(&first.rows),
            vec![(1, false), (2, true), (3, false)]
        );
        assert_eq!(first.next_offset, Some(4));
        assert_eq!(
            numbered(&second.rows),
            vec![(3, false), (4, true), (5, false)]
        );
        assert_eq!(second.next_offset, None);
    }

    #[test_case(r"(?m)^b", "a\rb", false ; "lf_anchor_ignores_carriage_return")]
    #[test_case(r"\bβeta\b", "x βeta y", true ; "unicode_word_boundary")]
    #[test_case(r"^$", "", false ; "empty_text_has_no_line")]
    #[test_case(r"^$", "\n", true ; "one_empty_line")]
    fn grep_matches_each_line_as_the_regex_crate_does(pattern: &str, text: &str, found: bool) {
        assert_eq!(
            !search(text, pattern, window(0, 1, 0)).rows.is_empty(),
            found
        );
    }

    #[test]
    fn grep_excerpt_contains_a_match_beyond_the_display_prefix() {
        let text = format!("{}needle{}", "a".repeat(3_000), "b".repeat(3_000));

        let result = search(&text, "needle", window(0, 1, 0));

        assert_eq!(result.rows.len(), 1);
        assert!(result.rows[0].text.contains("needle"));
        assert!(result.rows[0].text.starts_with(OMITTED));
        assert!(result.rows[0].text.ends_with(OMITTED));
        assert!(result.rows[0].text.len() <= MAX_DISPLAY_LINE_BYTES);
    }

    #[test]
    fn grep_aggregate_is_bounded_and_next_offset_replays_first_omitted_match() {
        let text = (1..=MAX_GREP_MATCHES)
            .map(|line| format!("match-{line}-{}", "x".repeat(MAX_DISPLAY_LINE_BYTES)))
            .collect::<Vec<_>>()
            .join("\n");

        let first = search(&text, "match-", window(0, MAX_GREP_MATCHES, 0));
        let rendered_bytes = first
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                usize::from(index > 0) + row.line_number.to_string().len() + 2 + row.text.len()
            })
            .sum::<usize>();
        let next_offset = first.next_offset.unwrap();

        assert!(rendered_bytes <= GREP_ROW_BYTES);
        assert!(first.rows.len() <= MAX_READ_LINES);
        assert_eq!(next_offset, first.rows.len() + 1);
        let second = search(
            &text,
            "match-",
            window(next_offset - 1, MAX_GREP_MATCHES, 0),
        );
        assert_eq!(second.rows[0].line_number, next_offset);
        assert!(second.rows[0].is_match);
    }

    #[test]
    fn grep_enforces_the_aggregate_row_cap() {
        let mut lines = Vec::new();
        for _ in 0..MAX_GREP_MATCHES {
            lines.extend(iter::repeat_n("context", MAX_GREP_CONTEXT));
            lines.push("match");
            lines.extend(iter::repeat_n("context", MAX_GREP_CONTEXT));
        }
        let text = lines.join("\n");

        let result = search(
            &text,
            "match",
            window(0, MAX_GREP_MATCHES, MAX_GREP_CONTEXT),
        );

        assert!(result.rows.len() <= MAX_READ_LINES);
        let next_offset = result.next_offset.unwrap();
        let next = search(&text, "match", window(next_offset - 1, 1, 0));
        assert_eq!(next.rows[0].line_number, next_offset);
    }

    #[test]
    fn preview_keeps_bounded_utf8_head_and_tail() {
        let text = format!("HEAD{}TAIL", "蟹".repeat(100_000));

        let preview = preview(&text, 8, 240);

        assert!(preview.head.starts_with("HEAD"));
        assert!(preview.tail.ends_with("TAIL"));
        assert!(preview.head.len() + preview.tail.len() < 240);
    }

    #[test_case("a\nb\nc\nd\n", 2, "a", "d" ; "one_line_each")]
    #[test_case("a\r\nb\r\nc\r\n", 4, "a\r\nb", "b\r\nc" ; "crlf_lines")]
    fn preview_splits_its_lines_between_head_and_tail(
        text: &str,
        max_lines: usize,
        head: &str,
        tail: &str,
    ) {
        let preview = preview(text, max_lines, usize::MAX);

        assert_eq!((preview.head.as_str(), preview.tail.as_str()), (head, tail));
    }
}
