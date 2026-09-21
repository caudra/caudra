use crate::{highlight::highlight_line, theme};
use caudra_highlight::Highlighter;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde::de::IgnoredAny;
use std::ops::Range;

const JSON_TOKEN: &str = "json";
const MAX_LINE_BYTES: usize = 16 * 1024;

pub(crate) fn scalar_spans(value: &str) -> Vec<Span<'static>> {
    if value.contains(['\t', '\n']) {
        return vec![Span::styled(value.to_owned(), theme::current().code_block)];
    }
    let end = value.floor_char_boundary(value.len().min(MAX_LINE_BYTES));
    let mut spans = highlight_line(&mut Highlighter::for_token(JSON_TOKEN), &value[..end]);
    if end < value.len() {
        spans.push(Span::raw(value[end..].to_owned()));
    }
    spans
}

pub(crate) fn line(value: &str) -> Line<'static> {
    line_with_budget(value, MAX_LINE_BYTES)
}

pub(crate) fn overlays(value: &str) -> Vec<(Range<usize>, Style)> {
    let end = value.floor_char_boundary(value.len().min(MAX_LINE_BYTES));
    let mut start = 0;
    line(&value[..end])
        .spans
        .into_iter()
        .filter_map(|span| {
            let end = start + span.content.chars().count();
            let range = start..end;
            start = end;
            (span.style != Style::default()).then_some((range, span.style))
        })
        .collect()
}

fn line_with_budget(value: &str, budget: usize) -> Line<'static> {
    let end = value.floor_char_boundary(budget.min(value.len()));
    let input = &value[..end];
    let bytes = input.as_bytes();
    let theme = theme::current();
    let mut spans = Vec::new();
    let mut index = 0;
    let mut plain = 0;
    let mut json = input.trim_start().starts_with(['"', '{', '[', '}', ']'])
        || serde_json::from_str::<IgnoredAny>(input.trim().trim_end_matches(',')).is_ok();
    while index < bytes.len() {
        let start = index;
        let ch = bytes[index];
        if matches!(ch, b'{' | b'[') {
            json = true;
        }
        if !json || ch.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        let painted = match ch {
            b'"' => {
                index += 1;
                while index < bytes.len() {
                    match bytes[index] {
                        b'\\' => index = (index + 2).min(bytes.len()),
                        b'"' => {
                            index += 1;
                            break;
                        }
                        _ => index += 1,
                    }
                }
                let token = &input[start..index];
                if input[index..].trim_start().starts_with(':') {
                    vec![Span::styled(token.to_owned(), theme.accent)]
                } else {
                    scalar_spans(token)
                }
            }
            b'{' | b'}' | b'[' | b']' | b':' | b',' => {
                index += 1;
                vec![Span::styled(input[start..index].to_owned(), theme.tool_dim)]
            }
            b'-' | b'0'..=b'9' | b't' | b'f' | b'n' => {
                index += 1;
                while index < bytes.len()
                    && matches!(bytes[index], b'a'..=b'z' | b'0'..=b'9' | b'.' | b'+' | b'-' | b'E')
                {
                    index += 1;
                }
                let token = &input[start..index];
                if serde_json::from_str::<IgnoredAny>(token).is_ok() {
                    scalar_spans(token)
                } else {
                    json = false;
                    continue;
                }
            }
            _ => {
                json = false;
                index += 1;
                continue;
            }
        };
        if start > plain {
            spans.push(Span::raw(input[plain..start].to_owned()));
        }
        spans.extend(painted);
        plain = index;
    }
    if plain < value.len() {
        spans.push(Span::raw(value[plain..].to_owned()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::{MAX_LINE_BYTES, line, overlays, scalar_spans};
    use crate::{highlight::refresh_syntax_theme, theme};
    use ratatui::style::Style;
    use test_case::test_case;

    const MIXED: &str = "Immutable roots:\n{\n  \"name\": \"value\",\n  \"count\": 7,\n  \"ready\": true\n}\nNo automatic rollback.";

    #[test_case(MIXED; "mixed_document")]
    #[test_case("{\"draft\": \"literal\ttab\"}"; "invalid_draft_tab_keeps_editor_coordinates")]
    #[test_case("Ownership: {\"nested\": [null, -1.2e+3, {\"a\\\"b\": \"é界\"}]} done"; "inline_nested_and_escaped")]
    #[test_case("{\n  \"unfinished\": \"draft"; "incomplete_draft")]
    #[test_case("\n\n"; "blank_lines")]
    fn highlighting_preserves_text(value: &str) {
        assert_eq!(
            value
                .split('\n')
                .map(|value| line(value).to_string())
                .collect::<Vec<_>>()
                .join("\n"),
            value
        );
    }

    #[test]
    fn mixed_details_use_workflow_key_and_scalar_styles() {
        refresh_syntax_theme();
        let rendered: Vec<_> = MIXED.split('\n').map(line).collect();
        let key = rendered[2]
            .spans
            .iter()
            .find(|span| span.content == "\"name\"")
            .unwrap();
        let string = rendered[2]
            .spans
            .iter()
            .find(|span| span.content == "value")
            .unwrap();
        let number = rendered[3]
            .spans
            .iter()
            .find(|span| span.content == "7")
            .unwrap();
        let literal = rendered[4]
            .spans
            .iter()
            .find(|span| span.content == "true")
            .unwrap();
        assert_eq!(key.style, theme::current().accent);
        assert_ne!(key.style.fg, string.style.fg);
        assert_ne!(string.style.fg, number.style.fg);
        assert_ne!(number.style.fg, literal.style.fg);
        assert!(
            rendered[0]
                .spans
                .iter()
                .all(|span| span.style == Style::default())
        );
        assert!(
            rendered[6]
                .spans
                .iter()
                .all(|span| span.style == Style::default())
        );
    }

    #[test_case("  7,"; "number_array_member")]
    #[test_case("  true,"; "boolean_array_member")]
    #[test_case("null"; "absent_live_descriptor")]
    fn standalone_scalars_are_highlighted(value: &str) {
        assert!(line(value).spans.iter().any(|span| span.style.fg.is_some()));
    }

    #[test]
    fn budgets_keep_the_complete_unicode_tail_readable() {
        let value = format!("{{\"large\":\"{}\"}}", "界".repeat(MAX_LINE_BYTES));
        let rendered = line(&value);
        assert_eq!(rendered.to_string(), value);
        assert_eq!(rendered.spans.last().unwrap().style, Style::default());
    }

    #[test]
    fn editor_overlays_never_cover_an_unbounded_plain_tail() {
        let prefix = "{\"large\": \"";
        let value = format!("{prefix}{}\"}}", "界".repeat(MAX_LINE_BYTES));
        let painted = overlays(&value);
        assert!(!painted.is_empty());
        assert!(painted.iter().all(|(range, _)| range.end <= MAX_LINE_BYTES));
        assert_eq!(
            painted,
            overlays(&value[..value.floor_char_boundary(MAX_LINE_BYTES)])
        );
    }

    #[test]
    fn workflow_scalar_budget_retains_the_complete_value() {
        let value = format!("\"{}\"", "界".repeat(MAX_LINE_BYTES));
        let spans = scalar_spans(&value);
        assert_eq!(
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            value
        );
        assert_eq!(spans.last().unwrap().style, Style::default());
    }
}
