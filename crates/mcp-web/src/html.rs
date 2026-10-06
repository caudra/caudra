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
static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\p{L}{3,}").expect("word regex"));
static URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)https?://[^\s)\]>"']+"#).expect("URL regex"));
/// A Markdown link this module's own converter wrote. Its brackets and target
/// are structure, not the payload the line filter looks for.
static MARKDOWN_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]*)\]\([^)\s]*\)").expect("Markdown link regex"));
static BRACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[{}\[\]]").expect("brace regex"));
/// Letters, marks, and digits of every script are text, so only punctuation and
/// symbols outside ordinary prose count.
static SYMBOL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"[^\p{L}\p{M}\p{N}\s.,;:!?"'()\[\]{}\-_/]"#).expect("symbol regex")
});

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
    if is_mostly_urls(trimmed, length) {
        return true;
    }
    let prose = MARKDOWN_LINK.replace_all(trimmed, "$1");
    let length = prose.chars().count();
    let braces = BRACE.find_iter(&prose).count();
    let symbols = SYMBOL.find_iter(&prose).count();
    let ratio = symbols as f64 / length.max(1) as f64;
    (length >= 220 && (braces >= 8 || ratio > 0.22)) || ratio > 0.33
}

/// A line of three or more URLs that make up most of its characters is a list
/// of asset or tracking addresses. Prose that links three sources is not.
fn is_mostly_urls(line: &str, length: usize) -> bool {
    let (count, characters) = URL
        .find_iter(line)
        .fold((0, 0), |(count, characters), url| {
            (count + 1, characters + url.as_str().chars().count())
        });
    count >= 3 && characters * 2 > length
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const CYRILLIC_PARAGRAPH: &str = "Исследователи из нескольких университетов опубликовали подробный отчёт о том, как изменение климата влияет на сельское хозяйство в северных регионах. В отчёте приводятся данные за двадцать лет наблюдений, а также рекомендации для фермеров и местных властей.";
    const CHINESE_PARAGRAPH: &str = "研究人员在多所大学发表了一份详细报告，说明气候变化如何影响北方地区的农业生产。报告列出了二十年来的观测数据，并为农民和地方政府提出了具体建议。报告指出，气温上升使生长季节延长，但降水模式的变化也带来了新的风险。作者建议加强灌溉系统建设，推广耐旱作物品种，并建立区域性的气象预警网络。他们还强调，农业保险制度需要根据新的气候条件进行调整，以保护小规模农户的收入。此外，报告呼吁各级政府加大对农业科研的投入，支持高校与企业合作开发适应性技术，并通过培训帮助农民掌握新的种植方法。";
    const GREEK_PARAGRAPH: &str = "Οι ερευνητές από πολλά πανεπιστήμια δημοσίευσαν μια λεπτομερή έκθεση για το πώς η κλιματική αλλαγή επηρεάζει τη γεωργία στις βόρειες περιοχές. Η έκθεση παρουσιάζει δεδομένα είκοσι ετών παρατηρήσεων και προτείνει συγκεκριμένα μέτρα για τους αγρότες και τις τοπικές αρχές.";
    const ARABIC_PARAGRAPH: &str = "نشر باحثون من عدة جامعات تقريرا مفصلا حول كيفية تأثير تغير المناخ على الزراعة في المناطق الشمالية. ويعرض التقرير بيانات عشرين عاما من الرصد، ويقدم توصيات محددة للمزارعين والسلطات المحلية، مع التركيز على أنظمة الري والمحاصيل المقاومة للجفاف.";
    const VIETNAMESE_PARAGRAPH: &str = "Các nhà nghiên cứu từ nhiều trường đại học đã công bố một báo cáo chi tiết về việc biến đổi khí hậu ảnh hưởng đến nông nghiệp ở các vùng phía bắc như thế nào. Báo cáo trình bày dữ liệu quan sát trong hai mươi năm và đưa ra các khuyến nghị cụ thể cho nông dân và chính quyền địa phương.";
    const LINKED_PARAGRAPH: &str = "The survey combines three sources: the [national weather archive](https://weather.example.test/archive/2025), the [regional crop yield tables](https://agri.example.test/yields/regional), and the [farm insurance claims register](https://insurance.example.test/claims). Each source covers the same twenty-year period.";
    const MINIFIED_SCRIPT: &str = "!function(e){var t={};function n(r){if(t[r])return t[r].exports;var o=t[r]={i:r,l:!1,exports:{}};return e[r].call(o.exports,o,o.exports,n),o.l=!0,o.exports}n.m=e,n.c=t,n.d=function(e,t,r){n.o(e,t)||Object.defineProperty(e,t,{enumerable:!0,get:r})}}([]);";
    const ASSET_ADDRESSES: &str = "https://cdn.example.test/assets/app.3f9a1c.js https://cdn.example.test/assets/vendor.77b2e0.js https://cdn.example.test/assets/styles.1d04aa.css https://cdn.example.test/fonts/inter.woff2";
    const TRACKING_LINKS: &str = "[](https://t.example.test/p?id=1) [](https://t.example.test/p?id=2) [](https://t.example.test/p?id=3) [](https://t.example.test/p?id=4) [](https://t.example.test/p?id=5)";
    const INLINE_STATE: &str = r#"{"props":{"pageProps":{"items":[{"id":1,"name":"alpha"},{"id":2,"name":"beta"},{"id":3,"name":"gamma"}],"meta":{"total":3,"page":1}}},"page":"/catalog","query":{},"buildId":"a1b2c3","isFallback":false,"gssp":true,"locale":"en"}"#;
    const FRAMEWORK_STATE: &str = r#"window.__INITIAL_STATE__={"user":null}"#;

    #[test_case(CYRILLIC_PARAGRAPH ; "a Cyrillic paragraph")]
    #[test_case(CHINESE_PARAGRAPH ; "a Chinese paragraph")]
    #[test_case(GREEK_PARAGRAPH ; "a Greek paragraph")]
    #[test_case(ARABIC_PARAGRAPH ; "an Arabic paragraph")]
    #[test_case(VIETNAMESE_PARAGRAPH ; "a Vietnamese paragraph")]
    #[test_case(LINKED_PARAGRAPH ; "a paragraph that links three sources")]
    fn prose_is_never_mistaken_for_a_source_payload(paragraph: &str) {
        assert!(paragraph.chars().count() > 220);
        assert!(!is_likely_source_payload_line(paragraph));
    }

    #[test_case(MINIFIED_SCRIPT ; "a minified script")]
    #[test_case(ASSET_ADDRESSES ; "a list of asset addresses")]
    #[test_case(TRACKING_LINKS ; "a row of empty tracking links")]
    #[test_case(INLINE_STATE ; "inline JSON state")]
    #[test_case(FRAMEWORK_STATE ; "a framework state marker")]
    fn source_payloads_are_dropped(line: &str) {
        assert!(is_likely_source_payload_line(line));
    }

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
