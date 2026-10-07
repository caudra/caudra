use std::collections::HashMap;

use url::Url;
use workcell_net::UrlPolicy;

#[derive(Clone, Debug)]
pub(crate) struct HtmlIconCandidate {
    pub(crate) url: Url,
    score: i32,
}

pub(crate) fn discover_html_icons(
    html: &str,
    page_url: &Url,
    policy: UrlPolicy,
) -> Vec<HtmlIconCandidate> {
    let base_url = find_tags(html, "base")
        .into_iter()
        .find_map(|tag| attribute_value(tag, "href"))
        .and_then(|href| policy.parse_url(href.trim(), Some(page_url)).ok())
        .unwrap_or_else(|| page_url.clone());
    let mut candidates = HashMap::<String, HtmlIconCandidate>::new();
    for tag in find_tags(html, "link") {
        let Some(rel) = attribute_value(tag, "rel") else {
            continue;
        };
        let Some(href) = attribute_value(tag, "href") else {
            continue;
        };
        let rel_score = score_rel(rel);
        if rel_score == 0 {
            continue;
        }
        let Ok(url) = policy.parse_url(href.trim(), Some(&base_url)) else {
            continue;
        };
        let kind = attribute_value(tag, "type");
        let sizes = attribute_value(tag, "sizes");
        let score = rel_score + score_sizes(sizes) + score_type_or_path(kind, url.path());
        let key = url.as_str().to_owned();
        if candidates
            .get(&key)
            .is_none_or(|existing| score > existing.score)
        {
            candidates.insert(key, HtmlIconCandidate { url, score });
        }
    }
    let mut candidates = candidates.into_values().collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.score));
    candidates
}

fn find_tags<'a>(html: &'a str, name: &str) -> Vec<&'a str> {
    let bytes = html.as_bytes();
    let name = name.as_bytes();
    let mut tags = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'<' || !starts_ascii_case_insensitive(&bytes[index + 1..], name) {
            index += 1;
            continue;
        }
        let after_name = index + 1 + name.len();
        if after_name >= bytes.len()
            || !matches!(
                bytes[after_name],
                b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>'
            )
        {
            index += 1;
            continue;
        }
        let mut quote = None;
        let mut end = after_name;
        while end < bytes.len() {
            match (quote, bytes[end]) {
                (None, b'\'' | b'"') => quote = Some(bytes[end]),
                (Some(active), byte) if byte == active => quote = None,
                (None, b'>') => {
                    tags.push(&html[index..=end]);
                    break;
                }
                _ => {}
            }
            end += 1;
        }
        index = end.saturating_add(1);
    }
    tags
}

fn attribute_value<'a>(tag: &'a str, wanted: &str) -> Option<&'a str> {
    let bytes = tag.as_bytes();
    let wanted = wanted.as_bytes();
    let mut index = 1;
    while index < bytes.len() {
        while index < bytes.len()
            && (bytes[index].is_ascii_whitespace() || matches!(bytes[index], b'<' | b'/' | b'>'))
        {
            index += 1;
        }
        let start = index;
        while index < bytes.len()
            && !bytes[index].is_ascii_whitespace()
            && !matches!(bytes[index], b'=' | b'/' | b'>')
        {
            index += 1;
        }
        let is_wanted = index - start == wanted.len()
            && starts_ascii_case_insensitive(&bytes[start..index], wanted);
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] != b'=' {
            continue;
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || !matches!(bytes[index], b'\'' | b'"') {
            continue;
        }
        let quote = bytes[index];
        index += 1;
        let value_start = index;
        while index < bytes.len() && bytes[index] != quote {
            index += 1;
        }
        if is_wanted && index <= bytes.len() {
            return tag.get(value_start..index);
        }
        index = index.saturating_add(1);
    }
    None
}

fn starts_ascii_case_insensitive(value: &[u8], prefix: &[u8]) -> bool {
    value.len() >= prefix.len()
        && value[..prefix.len()]
            .iter()
            .zip(prefix)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn score_rel(value: &str) -> i32 {
    let normalized = value.trim().to_ascii_lowercase();
    let tokens = normalized.split_ascii_whitespace().collect::<Vec<_>>();
    if tokens.contains(&"icon") {
        if tokens.contains(&"shortcut") {
            95
        } else {
            100
        }
    } else if normalized.contains("apple-touch-icon") {
        85
    } else if normalized.contains("mask-icon") {
        70
    } else {
        0
    }
}

fn score_sizes(value: Option<&str>) -> i32 {
    let Some(value) = value else { return 0 };
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.contains("any") {
        return 6;
    }
    let largest = normalized
        .split_ascii_whitespace()
        .filter_map(|size| size.split_once('x'))
        .filter_map(|(width, height)| Some(width.parse::<u16>().ok()?.max(height.parse().ok()?)))
        .max()
        .unwrap_or_default();
    match largest {
        32..=192 => 8,
        16..=31 => 5,
        193.. => 4,
        _ => 0,
    }
}

fn score_type_or_path(kind: Option<&str>, path: &str) -> i32 {
    let value = format!("{} {path}", kind.unwrap_or_default()).to_ascii_lowercase();
    if value.contains("png") {
        8
    } else if value.contains("webp") {
        7
    } else if value.contains("jpeg") || value.contains("jpg") {
        6
    } else if value.contains("ico") || value.contains("x-icon") {
        5
    } else if value.contains("svg") {
        2
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respects_base_deduplicates_and_scores_candidates() {
        let page = Url::parse("https://example.com/docs/page").unwrap();
        let icons = discover_html_icons(
            r#"
                <base href="/assets/">
                <link href='large.svg' sizes='any' rel='icon' type='image/svg+xml'>
                <link rel="icon" href="best.png" sizes="32x32" type="image/png">
                <link rel="shortcut icon" href="best.png">
                <link rel="stylesheet" href="ignored.png">
            "#,
            &page,
            UrlPolicy::PublicInternet,
        );
        assert_eq!(icons.len(), 2);
        assert_eq!(icons[0].url.as_str(), "https://example.com/assets/best.png");
        assert_eq!(
            icons[1].url.as_str(),
            "https://example.com/assets/large.svg"
        );
    }

    #[test]
    fn rejects_non_http_base_and_icon_urls() {
        let page = Url::parse("https://example.com/page").unwrap();
        let icons = discover_html_icons(
            r#"<base href="file:///tmp/"><link rel="icon" href="javascript:alert(1)">"#,
            &page,
            UrlPolicy::PublicInternet,
        );
        assert!(icons.is_empty());
    }
}
