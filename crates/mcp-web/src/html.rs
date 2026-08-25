use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::LazyLock;

use dom_query::Document;
use dom_smoothie::{Config, Readability, TextMode};
use htmd::HtmlToMarkdown;
use htmd::options::{BulletListMarker, CodeBlockStyle, HeadingStyle, Options};
use regex::Regex;
use url::Url;

static SOURCE_NOISE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(bootloader|rsrcmap|cdninstagram\.com/rsrc\.php|webpack|__next_data__|window\.__|sourceMappingURL=|__NUXT__)",
    )
    .expect("source-noise regex")
});
static WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z\u{00C0}-\u{017F}]{3,}").expect("word regex"));
static URL_HIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)https?://").expect("URL regex"));
static BRACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[{}\[\]]").expect("brace regex"));
static SYMBOL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"[^a-zA-Z0-9\s.,;:!?"'()\[\]{}\-_/]"#).expect("symbol regex"));

const SKIP_TAGS: &[&str] = &[
    "script", "style", "meta", "link", "noscript", "iframe", "object", "embed",
];

#[derive(Clone, Debug)]
pub(crate) struct HtmlExtraction {
    pub output: String,
    pub title: Option<String>,
    pub method: &'static str,
    pub low_signal: bool,
}

/// Parse and sanitize HTML. The parsers are panic-contained because malformed
/// remote markup is untrusted input even after the network body is bounded.
pub(crate) fn extract_html_for_prompt(
    html: &str,
    markdown: bool,
    base_url: &str,
) -> HtmlExtraction {
    let normalized = normalize_source_text(html);
    let fallback_title = catch_unwind(AssertUnwindSafe(|| extract_title(&normalized)))
        .ok()
        .flatten();
    let readability = catch_unwind(AssertUnwindSafe(|| {
        let config = Config {
            char_threshold: 180,
            max_elements_to_parse: 100_000,
            text_mode: TextMode::Formatted,
            ..Config::default()
        };
        let mut parser = Readability::new(normalized.clone(), Some(base_url), Some(config)).ok()?;
        let article = parser.parse().ok()?;
        let output = if markdown {
            clean_markdown(&convert_to_markdown(article.content.as_ref()).ok()?)
        } else {
            clean_text(article.text_content.as_ref())
        };
        if output.is_empty() {
            return None;
        }
        let title = normalize_title(&article.title).or_else(|| fallback_title.clone());
        Some(HtmlExtraction {
            low_signal: is_low_signal(&output),
            output,
            title,
            method: "readability",
        })
    }))
    .ok()
    .flatten();

    if let Some(extracted) = readability.as_ref().filter(|value| !value.low_signal) {
        return extracted.clone();
    }

    let fallback = catch_unwind(AssertUnwindSafe(|| {
        let document = sanitized_document(&normalized, base_url);
        let body = document.body();
        if markdown {
            let html = body.map_or_else(|| document.html(), |body| body.html());
            convert_to_markdown(html.as_ref())
                .map(|output| clean_markdown(&output))
                .unwrap_or_default()
        } else {
            let text = body.map_or_else(|| document.formatted_text(), |body| body.formatted_text());
            clean_text(text.as_ref())
        }
    }))
    .unwrap_or_default();
    if fallback.is_empty() {
        return HtmlExtraction {
            output: "Content extraction was limited for this page.".to_owned(),
            title: readability.and_then(|value| value.title).or(fallback_title),
            method: "fallback",
            low_signal: true,
        };
    }
    HtmlExtraction {
        low_signal: is_low_signal(&fallback),
        output: fallback,
        title: readability.and_then(|value| value.title).or(fallback_title),
        method: "fallback",
    }
}

fn extract_title(html: &str) -> Option<String> {
    let document = Document::from(html);
    normalize_title(document.select_single("title").text().as_ref())
}

pub(crate) fn add_title_context(output: &str, title: Option<&str>, markdown: bool) -> String {
    let Some(title) = title.and_then(normalize_title) else {
        return output.to_owned();
    };
    let first_line = output
        .trim_start()
        .lines()
        .next()
        .unwrap_or_default()
        .trim_start_matches('#')
        .trim();
    if first_line == title {
        return output.to_owned();
    }
    let prefix = if markdown {
        format!("# {title}")
    } else {
        title
    };
    format!("{prefix}\n\n{output}").trim().to_owned()
}

fn sanitized_document(html: &str, base_url: &str) -> Document {
    let document = Document::from(html);
    document.select(&SKIP_TAGS.join(",")).remove();

    if let Ok(base) = Url::parse(base_url) {
        for anchor in &document.select("a[href]") {
            let Some(href) = anchor.attr("href") else {
                continue;
            };
            if let Ok(resolved) = base.join(href.as_ref()) {
                anchor.set_attr("href", resolved.as_str());
            }
        }
    }

    document
}

fn convert_to_markdown(html: &str) -> Result<String, std::io::Error> {
    let converter = HtmlToMarkdown::builder()
        .options(Options {
            heading_style: HeadingStyle::Atx,
            bullet_list_marker: BulletListMarker::Dash,
            ul_bullet_spacing: 1,
            ol_number_spacing: 1,
            code_block_style: CodeBlockStyle::Fenced,
            ..Options::default()
        })
        .skip_tags(SKIP_TAGS.to_vec())
        .build();
    converter.convert(html)
}

fn clean_markdown(input: &str) -> String {
    let lines = normalize_source_text(input)
        .lines()
        .map(|line| line.replace('\t', " ").trim_end().to_owned())
        .filter(|line| !is_likely_source_payload_line(line))
        .collect::<Vec<_>>();
    collapse_blank_lines(&lines.join("\n")).trim().to_owned()
}

fn clean_text(input: &str) -> String {
    let lines = normalize_source_text(input)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !is_likely_source_payload_line(line))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    collapse_blank_lines(&lines.join("\n")).trim().to_owned()
}

fn collapse_blank_lines(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut newlines = 0;
    for character in input.chars() {
        if character == '\n' {
            newlines += 1;
            if newlines <= 2 {
                output.push(character);
            }
        } else {
            newlines = 0;
            output.push(character);
        }
    }
    output
}

fn normalize_source_text(input: &str) -> String {
    input
        .replace('\0', "")
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

fn normalize_title(input: &str) -> Option<String> {
    let title = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        None
    } else {
        Some(title.chars().take(200).collect())
    }
}

fn is_low_signal(output: &str) -> bool {
    let normalized = output.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() < 180 {
        return true;
    }
    let words = WORD.find_iter(&normalized).count();
    if words < 35 {
        return true;
    }
    SOURCE_NOISE.is_match(&normalized) && words < 120
}

fn is_likely_source_payload_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    if SOURCE_NOISE.is_match(trimmed) {
        return true;
    }
    let length = trimmed.chars().count();
    if length < 140 {
        return false;
    }
    if URL_HIT.find_iter(trimmed).count() >= 3 {
        return true;
    }
    let braces = BRACE.find_iter(trimmed).count();
    let symbols = SYMBOL.find_iter(trimmed).count();
    let ratio = symbols as f64 / length as f64;
    (length >= 220 && (braces >= 8 || ratio > 0.22)) || ratio > 0.33
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_and_fallback_use_html_parsing() {
        let html = r#"
            <html>
              <head><title>Fish &amp; Chips</title></head>
              <body>
                <p>Keep 2 &lt; 3 and follow <a data-href="/wrong" href=/right?x=1&amp;y=2>the source</a>.</p>
                <script>window.fake = '<p>discard me</p>';</script>
                <style>.discard { display: block; }</style>
                <iframe>discard frame</iframe>
              </body>
            </html>
        "#;

        let extracted = extract_html_for_prompt(html, true, "https://example.test/base/page");

        assert_eq!(extracted.title.as_deref(), Some("Fish & Chips"));
        assert_eq!(extracted.method, "fallback");
        assert!(extracted.output.contains("Keep 2 < 3"));
        assert!(
            extracted
                .output
                .contains("[the source](https://example.test/right?x=1&y=2)")
        );
        assert!(!extracted.output.contains("discard me"));
        assert!(!extracted.output.contains("discard frame"));
    }

    #[test]
    fn link_resolution_only_changes_href_attributes() {
        let document = sanitized_document(
            r#"<a data-href="/telemetry" href="../source">Source</a>"#,
            "https://example.test/research/page",
        );
        let anchor = document.select_single("a");

        assert_eq!(
            anchor.attr("href").as_deref(),
            Some("https://example.test/source")
        );
        assert_eq!(anchor.attr("data-href").as_deref(), Some("/telemetry"));
    }

    #[test]
    fn markdown_converter_preserves_structured_content() {
        let markdown = convert_to_markdown(
            r#"
                <h2>Details</h2>
                <ul><li>First</li><li><strong>Second</strong></li></ul>
                <pre><code class="language-rust">let value = 1;</code></pre>
                <table><thead><tr><th>Name</th><th>Value</th></tr></thead>
                <tbody><tr><td>alpha</td><td>one</td></tr></tbody></table>
            "#,
        )
        .expect("markdown conversion");

        assert!(markdown.contains("## Details"));
        assert!(markdown.contains("- First"));
        assert!(markdown.contains("- **Second**"));
        assert!(markdown.contains("```rust"));
        assert!(markdown.contains("| Name"));
        assert!(markdown.contains("| alpha"));
    }
}
