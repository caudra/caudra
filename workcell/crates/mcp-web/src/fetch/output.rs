use std::sync::LazyLock;

use regex::Regex;
use url::Url;

const KIB: usize = 1024;
const MAX_MODEL_OUTPUT_BYTES: usize = 50 * KIB;
const MAX_MODEL_OUTPUT_LINES: usize = 2_000;
/// Room kept for the truncation notice, which is longer than any notice built here.
const NOTICE_RESERVE_BYTES: usize = 128;
const MAX_SUMMARY_INPUT_BYTES: usize = 4 * 1024;
const MAX_ATTACHMENT_FILENAME_BYTES: usize = 200;

static PDF_WHITESPACE_BEFORE_NEWLINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+\n").expect("PDF whitespace-before-newline regex"));
static PDF_HORIZONTAL_WHITESPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[ \t]+").expect("PDF horizontal-whitespace regex"));
static PDF_EXCESS_NEWLINES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n{3,}").expect("PDF newline regex"));

pub(super) struct TruncatedText {
    pub text: String,
    pub truncated: bool,
}

/// Bounds the model text and ends incomplete text with a notice naming the limit
/// that cut it, so a cut page never reads as a complete one. `source_cut`
/// describes a read limit that already cut the source, such as "the first 5 MiB
/// of the response".
///
/// Every MCP text block is independently line- and byte-bounded after
/// extraction, and the notice counts inside both bounds. Structured summaries
/// remain bounded by the response cap.
pub(super) fn truncate_model_output(output: &str, source_cut: Option<&str>) -> TruncatedText {
    let total_lines = output.split('\n').count();
    if source_cut.is_none()
        && total_lines <= MAX_MODEL_OUTPUT_LINES
        && output.len() <= MAX_MODEL_OUTPUT_BYTES
    {
        return TruncatedText {
            text: output.to_owned(),
            truncated: false,
        };
    }
    let kept_lines = total_lines.min(MAX_MODEL_OUTPUT_LINES - 1);
    let mut text = output
        .split('\n')
        .take(kept_lines)
        .collect::<Vec<_>>()
        .join("\n");
    let byte_budget = MAX_MODEL_OUTPUT_BYTES - NOTICE_RESERVE_BYTES;
    let shown = if text.len() > byte_budget {
        text.truncate(utf8_prefix(&text, byte_budget).len());
        Some(format!(
            "showing {} of {} KiB",
            text.len() / KIB,
            output.len().div_ceil(KIB)
        ))
    } else if kept_lines < total_lines {
        Some(format!("showing {kept_lines} of {total_lines} lines"))
    } else {
        None
    };
    let notice = match (shown, source_cut) {
        (Some(shown), Some(cut)) => format!("[truncated: {shown}, and only {cut} was read]"),
        (Some(shown), None) => format!("[truncated: {shown}]"),
        (None, Some(cut)) => format!("[truncated: only {cut} was read]"),
        (None, None) => {
            return TruncatedText {
                text,
                truncated: false,
            };
        }
    };
    debug_assert!(notice.len() < NOTICE_RESERVE_BYTES);
    text.push('\n');
    text.push_str(&notice);
    TruncatedText {
        text,
        truncated: true,
    }
}

pub(super) fn truncate_summary_input(output: &str) -> String {
    utf8_prefix(output, MAX_SUMMARY_INPUT_BYTES).to_owned()
}

pub(super) fn normalize_pdf_text(input: &str) -> String {
    let normalized = input
        .replace('\u{000c}', "\n")
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let normalized = PDF_WHITESPACE_BEFORE_NEWLINE.replace_all(&normalized, "\n");
    let normalized = PDF_HORIZONTAL_WHITESPACE.replace_all(&normalized, " ");
    PDF_EXCESS_NEWLINES
        .replace_all(&normalized, "\n\n")
        .trim()
        .to_owned()
}

pub(super) fn filename_from_url(url: &Url) -> Option<String> {
    let raw = url.path_segments()?.rfind(|part| !part.is_empty())?;
    let bytes = percent_decode(raw.as_bytes());
    let decoded = String::from_utf8_lossy(&bytes);
    let normalized = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    let sanitized = normalized
        .chars()
        .filter(|character| !character.is_control())
        .map(|character| {
            if matches!(character, '/' | '\\') {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    let sanitized = sanitized.trim().trim_matches('.');
    if sanitized.is_empty() {
        return None;
    }
    let sanitized = utf8_prefix(sanitized, MAX_ATTACHMENT_FILENAME_BYTES).trim_end();
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized.to_owned())
    }
}

pub(crate) fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let boundary = value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= max_bytes)
        .last()
        .unwrap_or(0);
    &value[..boundary]
}

fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%'
            && index + 2 < input.len()
            && let (Some(high), Some(low)) = (hex(input[index + 1]), hex(input[index + 2]))
        {
            output.push((high << 4) | low);
            index += 3;
            continue;
        }
        output.push(input[index]);
        index += 1;
    }
    output
}

const fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_filename_removes_decoded_traversal_and_controls() {
        let url = Url::parse("https://example.com/%2E%2E%2F..%5Csecret%00.pdf").unwrap();
        let filename = filename_from_url(&url).unwrap();
        assert_eq!(filename, "_.._secret.pdf");
        assert!(!filename.contains(['/', '\\', '\0']));
    }

    #[test]
    fn attachment_filename_is_byte_bounded_at_utf8_boundary() {
        let name = "é".repeat(200);
        let url = Url::parse(&format!("https://example.com/{name}.pdf")).unwrap();
        let filename = filename_from_url(&url).unwrap();
        assert!(filename.len() <= MAX_ATTACHMENT_FILENAME_BYTES);
        assert!(filename.is_char_boundary(filename.len()));
    }

    #[test]
    fn attachment_filename_rejects_name_emptied_by_final_trimming() {
        let url = Url::parse("https://example.com/%2E%20%2E").unwrap();
        assert_eq!(filename_from_url(&url), None);
    }

    #[test]
    fn text_over_the_line_bound_ends_with_a_notice_inside_the_bound() {
        let lines = (1..=2_105)
            .map(|line| format!("line-{line}"))
            .collect::<Vec<_>>();
        let bounded = truncate_model_output(&lines.join("\n"), None);
        assert!(bounded.truncated);
        assert_eq!(bounded.text.split('\n').count(), MAX_MODEL_OUTPUT_LINES);
        assert!(
            bounded
                .text
                .ends_with("line-1999\n[truncated: showing 1999 of 2105 lines]")
        );
    }

    #[test]
    fn text_over_the_byte_bound_ends_with_a_notice_inside_the_bound() {
        let bounded = truncate_model_output(&"é".repeat(30 * KIB), None);
        assert!(bounded.truncated);
        assert!(bounded.text.len() <= MAX_MODEL_OUTPUT_BYTES);
        assert!(
            bounded
                .text
                .ends_with("\n[truncated: showing 49 of 60 KiB]")
        );
    }

    #[test]
    fn a_read_limit_that_cut_the_source_is_named_when_the_text_fits() {
        let bounded = truncate_model_output("first part", Some("the first 5 MiB of the response"));
        assert!(bounded.truncated);
        assert_eq!(
            bounded.text,
            "first part\n[truncated: only the first 5 MiB of the response was read]"
        );
    }

    #[test]
    fn a_read_limit_and_an_output_bound_are_named_together() {
        let bounded = truncate_model_output(
            &"é".repeat(30 * KIB),
            Some("the first 5 MiB of the response"),
        );
        assert!(bounded.text.len() <= MAX_MODEL_OUTPUT_BYTES);
        assert!(bounded.text.ends_with(
            "\n[truncated: showing 49 of 60 KiB, and only the first 5 MiB of the response was read]"
        ));
    }

    #[test]
    fn a_page_of_exactly_the_line_bound_is_complete_and_carries_no_notice() {
        let line = "x".repeat(MAX_MODEL_OUTPUT_BYTES / MAX_MODEL_OUTPUT_LINES - 1);
        let page = vec![line; MAX_MODEL_OUTPUT_LINES].join("\n");
        assert_eq!(page.split('\n').count(), MAX_MODEL_OUTPUT_LINES);
        assert!(page.len() <= MAX_MODEL_OUTPUT_BYTES);
        let bounded = truncate_model_output(&page, None);
        assert!(!bounded.truncated);
        assert_eq!(bounded.text, page);
    }

    #[test]
    fn summary_input_is_byte_bounded_at_utf8_boundary() {
        let summary = truncate_summary_input(&"é".repeat(MAX_SUMMARY_INPUT_BYTES));
        assert!(summary.len() <= MAX_SUMMARY_INPUT_BYTES);
        assert!(summary.is_char_boundary(summary.len()));
    }
}
