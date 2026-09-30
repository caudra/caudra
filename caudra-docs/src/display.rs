//! The page as the TUI renders it. Link targets inside the docs become absolute site URLs, because the Markdown
//! renderer only keeps http(s) links; the modal recognises them and navigates instead of opening a browser.
//! Lines are never added or removed, so heading line numbers hold for this text as well.

use std::borrow::Cow;

use crate::markdown::{self, Fences};
use crate::{DOCS_PATH, SITE_DOCS_URL};

const BADGE_OPEN: &str = "<span class=\"badge\">";
const SPAN_CLOSE: &str = "</span>";

pub(crate) fn render(body: &str, slug: &str) -> String {
    let mut display = String::with_capacity(body.len());
    let mut fences = Fences::default();
    for line in body.split_inclusive('\n') {
        let (content, newline) = line
            .strip_suffix('\n')
            .map_or((line, ""), |content| (content, "\n"));
        if fences.step(content) {
            display.push_str(line);
            continue;
        }
        let content = match markdown::atx_heading(content) {
            Some(_) => markdown::split_explicit_id(content).0,
            None => content,
        };
        inline(content, slug, &mut display);
        display.push_str(newline);
    }
    display
}

fn inline(text: &str, slug: &str, display: &mut String) {
    let mut rest = text;
    let mut in_badge = false;
    while let Some(ch) = rest.chars().next() {
        match ch {
            '`' => {
                let (code, after) = markdown::code_span(rest);
                display.push_str(code);
                rest = after;
            }
            '[' => match markdown::link(rest) {
                Some((label, destination, after)) => {
                    display.push('[');
                    inline(label, slug, display);
                    display.push_str("](");
                    display.push_str(&site_url(destination, slug));
                    display.push(')');
                    rest = after;
                }
                None => {
                    display.push('[');
                    rest = &rest[1..];
                }
            },
            '<' => match markdown::tag(rest) {
                Some((inner, after)) => {
                    if markdown::is_break(inner) {
                        display.push(' ');
                    } else if rest.starts_with(BADGE_OPEN) {
                        display.push('`');
                        in_badge = true;
                    } else if in_badge && rest.starts_with(SPAN_CLOSE) {
                        display.push('`');
                        in_badge = false;
                    }
                    rest = after;
                }
                None => {
                    display.push('<');
                    rest = &rest[1..];
                }
            },
            _ => {
                display.push(ch);
                rest = &rest[ch.len_utf8()..];
            }
        }
    }
}

fn site_url<'a>(destination: &'a str, slug: &str) -> Cow<'a, str> {
    if let Some(anchor) = destination.strip_prefix('#') {
        return Cow::Owned(format!("{SITE_DOCS_URL}{slug}/#{anchor}"));
    }
    match destination.strip_prefix(DOCS_PATH) {
        Some(path) => Cow::Owned(format!("{SITE_DOCS_URL}{path}")),
        None => Cow::Borrowed(destination),
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::render;

    #[test_case("### `code_map` <span class=\"badge\">on demand</span> {#code_map}", "### `code_map` `on demand`" ; "badge heading")]
    #[test_case("## Modes {#modes}", "## Modes" ; "explicit id")]
    #[test_case("See [modes](/docs/permissions/#modes).", "See [modes](https://caudra.ai/docs/permissions/#modes)." ; "page link")]
    #[test_case("See [below](#details).", "See [below](https://caudra.ai/docs/alpha/#details)." ; "same page link")]
    #[test_case("[`/permissions` manager](/docs/permissions/)", "[`/permissions` manager](https://caudra.ai/docs/permissions/)" ; "code in label")]
    #[test_case("[Maki](https://github.com/tontinton/maki)", "[Maki](https://github.com/tontinton/maki)" ; "external link")]
    #[test_case("| `a` | one<br>two |", "| `a` | one two |" ; "table break")]
    #[test_case("Use `[x](#y)` and `<span>`", "Use `[x](#y)` and `<span>`" ; "inline code untouched")]
    fn display_rewrites_one_line(line: &str, expected: &str) {
        assert_eq!(render(line, "alpha"), expected);
    }

    #[test]
    fn display_leaves_fenced_code_untouched() {
        let body = "```markdown\n## Steps {#steps}\n[a](#b) <br>\n```\n";
        assert_eq!(render(body, "alpha"), body);
    }

    #[test]
    fn display_keeps_line_count() {
        let body = "# Title {#t}\n\nText <span class=\"badge\">x</span>\n<div>\n\n| a<br>b |\n";
        assert_eq!(render(body, "alpha").lines().count(), body.lines().count());
    }
}
