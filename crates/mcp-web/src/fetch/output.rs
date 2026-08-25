use std::sync::LazyLock;

use regex::Regex;
use url::Url;

const MAX_MODEL_OUTPUT_BYTES: usize = 50 * 1024;
const MAX_MODEL_OUTPUT_LINES: usize = 2_000;
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

pub(super) fn truncate_model_output(output: &str) -> TruncatedText {
    // Every MCP text block is independently line- and byte-bounded after
    // extraction; structured summaries remain bounded by the response cap.
    let line_count = output.split('\n').count();
    let mut text = if line_count > MAX_MODEL_OUTPUT_LINES {
        output
            .split('\n')
            .take(MAX_MODEL_OUTPUT_LINES)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        output.to_owned()
    };
    let mut truncated = line_count > MAX_MODEL_OUTPUT_LINES;
    if text.len() > MAX_MODEL_OUTPUT_BYTES {
        text = utf8_prefix(&text, MAX_MODEL_OUTPUT_BYTES).to_owned();
        truncated = true;
    }
    TruncatedText { text, truncated }
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
    fn summary_input_is_byte_bounded_at_utf8_boundary() {
        let summary = truncate_summary_input(&"é".repeat(MAX_SUMMARY_INPUT_BYTES));
        assert!(summary.len() <= MAX_SUMMARY_INPUT_BYTES);
        assert!(summary.is_char_boundary(summary.len()));
    }
}
