use super::{Line, Map, PermissionRequest, PromptBody, Value, escape_terminal_controls};

pub(super) const MAX_REVIEW_CHARS: usize = 32_768;
const MAX_REVIEW_NODES: usize = 1024;
const MAX_REVIEW_DEPTH: usize = 16;
pub(super) const TRUNCATED: &str = "[truncated: review limit reached]";
pub(super) const INCOMPLETE_REDACTION: &str = "[incomplete command preview: credential boundary or expansion is ambiguous; hidden text may execute actions]";
const REDACTED: &str = "<redacted>";
const MISSING_BEFORE: &str = "Supplied changes only. Before-state and applied diff are unavailable; no files were read for this preview.";

pub(super) fn mask_secrets(value: &Value) -> Value {
    let mut nodes = MAX_REVIEW_NODES;
    let mut chars = MAX_REVIEW_CHARS;
    bounded_value(value, 0, &mut nodes, &mut chars)
}

fn bounded_value(value: &Value, depth: usize, nodes: &mut usize, chars: &mut usize) -> Value {
    if *nodes == 0 || *chars == 0 || depth == MAX_REVIEW_DEPTH {
        return Value::String(TRUNCATED.into());
    }
    *nodes -= 1;
    match value {
        Value::Object(object) => {
            let mut result = Map::new();
            for (key, value) in object {
                if *nodes == 0 || *chars == 0 {
                    result.insert(TRUNCATED.into(), Value::Bool(true));
                    break;
                }
                let name = bounded_text(key, chars);
                let value = if likely_secret_key(key) {
                    *nodes -= 1;
                    Value::String(format!("<redacted:{}>", json_type(value)))
                } else {
                    bounded_value(value, depth + 1, nodes, chars)
                };
                result.insert(name, value);
            }
            Value::Object(result)
        }
        Value::Array(values) => {
            let mut result = Vec::new();
            for value in values {
                if *nodes == 0 || *chars == 0 {
                    result.push(Value::String(TRUNCATED.into()));
                    break;
                }
                result.push(bounded_value(value, depth + 1, nodes, chars));
            }
            Value::Array(result)
        }
        Value::String(value) => Value::String(redact_text(&bounded_text(value, chars))),
        _ => value.clone(),
    }
}

pub(super) fn redact_url_query(value: &str) -> String {
    let Some(start) = value
        .find("https://")
        .into_iter()
        .chain(value.find("http://"))
        .min()
    else {
        return value.to_owned();
    };
    let quote = value[..start]
        .chars()
        .next_back()
        .or_else(|| value.chars().next())
        .filter(|character| matches!(*character, '\'' | '"'));
    let end = if quote.is_some_and(|quote| value.ends_with(quote)) {
        value.len() - 1
    } else {
        value.len()
    };
    let raw = &value[start..end];
    let Ok(mut url) = url::Url::parse(raw) else {
        return format!(
            "{}{REDACTED}{INCOMPLETE_REDACTION}{}",
            &value[..start],
            &value[end..]
        );
    };
    let hides_content = !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some();
    if !url.username().is_empty() || url.password().is_some() {
        let _ = url.set_username("redacted");
        let _ = url.set_password(None);
    }
    let query = url
        .query_pairs()
        .map(|(key, _)| format!("{key}=<redacted>"))
        .collect::<Vec<_>>()
        .join("&");
    if url.query().is_some() {
        url.set_query(Some(&query));
    }
    if url.fragment().is_some() {
        url.set_fragment(Some("redacted"));
    }
    let caution =
        if hides_content && possible_expansion(raw) || value.contains(INCOMPLETE_REDACTION) {
            format!(" {INCOMPLETE_REDACTION}")
        } else {
            String::new()
        };
    format!("{}{url}{}{caution}", &value[..start], &value[end..])
}

fn possible_expansion(value: &str) -> bool {
    let mut chars = value.chars().peekable();
    let mut previous = None;
    while let Some(character) = chars.next() {
        if character == '\\' && chars.next_if_eq(&'\n').is_some() {
            continue;
        }
        if matches!(character, '$' | '`') || character == '(' && matches!(previous, Some('<' | '>'))
        {
            return true;
        }
        previous = Some(character);
    }
    false
}

fn bounded_text(value: &str, remaining: &mut usize) -> String {
    let mut chars = value.chars();
    let mut result: String = chars.by_ref().take(*remaining).collect();
    *remaining = remaining.saturating_sub(result.chars().count());
    if chars.next().is_some() {
        result.push_str(TRUNCATED);
    }
    result
}

fn redact_text(value: &str) -> String {
    if let Some(start) = value.find("-----BEGIN")
        && value[start..].contains("PRIVATE KEY-----")
    {
        if let Some(end) = value[start..].find("-----END")
            && let Some(close) = value[start + end..].find("PRIVATE KEY-----")
        {
            let end = start + end + close + "PRIVATE KEY-----".len();
            let caution = if possible_expansion(&value[start..end]) {
                INCOMPLETE_REDACTION
            } else {
                ""
            };
            return format!(
                "{}{REDACTED}{caution}{}",
                redact_text(&value[..start]),
                redact_text(&value[end..])
            );
        }
        return format!(
            "{}{REDACTED}{INCOMPLETE_REDACTION}",
            redact_text(&value[..start])
        );
    }
    let words = review_words(value);
    let mut result = String::new();
    let mut copied = 0;
    let mut index = 0;
    let mut incomplete = false;
    while let Some(&(start, end, closed)) = words.get(index) {
        let word = &value[start..end];
        let inner = word.trim_matches(['\'', '"']);
        let separator = inner.find(['=', ':']);
        let name = separator.map_or(inner, |offset| &inner[..offset]);
        let spaced_assignment = words
            .get(index + 1)
            .is_some_and(|&(start, end, _)| &value[start..end] == "=");
        if !likely_secret_key(name)
            || (separator.is_none() && !name.starts_with("--") && !spaced_assignment)
        {
            result.push_str(&value[copied..start]);
            let redacted = redact_url_query(word);
            result.push_str(&redacted);
            if redacted.contains(INCOMPLETE_REDACTION) {
                return result;
            }
            copied = end;
            index += 1;
            continue;
        }
        let mut secret_start = separator.map(|offset| {
            start + (word.len() - word.trim_start_matches(['\'', '"']).len()) + offset + 1
        });
        let mut secret_end = end;
        if secret_start
            .is_none_or(|position| value[position..end].trim_matches(['\'', '"']).is_empty())
        {
            index += 1 + usize::from(spaced_assignment);
            if separator.is_some_and(|offset| inner.as_bytes()[offset] == b':')
                && words.get(index).is_some_and(|&(start, end, _)| {
                    matches!(&value[start..end], "Bearer" | "Basic")
                })
            {
                index += 1;
            }
            if let Some(&(start, end, closed)) = words.get(index)
                && !(end - start == 1 && value[start..end].chars().any(is_shell_operator))
            {
                secret_start = Some(start);
                secret_end = end;
                incomplete |= !closed;
            } else {
                incomplete = true;
                index += 1;
                continue;
            }
        }
        let start = secret_start.unwrap_or(end);
        let secret = &value[start..secret_end];
        incomplete |= !closed || possible_expansion(secret);
        result.push_str(&value[copied..start]);
        let opening = secret.starts_with(['\'', '"']);
        let closing = secret.ends_with(['\'', '"']);
        if opening {
            result.push_str(&secret[..1]);
        }
        result.push_str(REDACTED);
        if closing && secret.len() > usize::from(opening) {
            result.push_str(&secret[secret.len() - 1..]);
        }
        if possible_expansion(secret) {
            result.push_str(INCOMPLETE_REDACTION);
            return result;
        }
        copied = secret_end;
        index += 1;
    }
    result.push_str(&value[copied..]);
    if incomplete {
        result.push_str(INCOMPLETE_REDACTION);
    }
    result
}

fn is_shell_operator(character: char) -> bool {
    matches!(character, ';' | '&' | '|' | '<' | '>' | '(' | ')')
}

fn review_words(value: &str) -> Vec<(usize, usize, bool)> {
    let mut words = Vec::new();
    let mut chars = value.char_indices().peekable();
    while let Some((start, first)) = chars.next() {
        if first.is_whitespace() {
            continue;
        }
        let mut quote = matches!(first, '\'' | '"').then_some(first);
        let mut escaped = first == '\\';
        let mut end = start + first.len_utf8();
        if !is_shell_operator(first) {
            while let Some(&(offset, character)) = chars.peek() {
                if !escaped
                    && quote.is_none()
                    && (character.is_whitespace() || is_shell_operator(character))
                {
                    break;
                }
                chars.next();
                end = offset + character.len_utf8();
                if escaped {
                    escaped = false;
                } else if character == '\\' && quote != Some('\'') {
                    escaped = true;
                } else if Some(character) == quote {
                    quote = None;
                } else if quote.is_none() && matches!(character, '\'' | '"') {
                    quote = Some(character);
                }
            }
        }
        words.push((start, end, quote.is_none() && !escaped));
    }
    words
}

pub(crate) fn sensitive_text(value: &str) -> bool {
    let redacted = redact_text(value);
    redacted.contains(REDACTED)
        || redacted.contains(INCOMPLETE_REDACTION)
        || redacted.contains("redacted") && redacted != value
}

pub(super) fn review_text(value: &str) -> String {
    let mut chars = MAX_REVIEW_CHARS;
    let mut redacted = redact_text(&bounded_text(value, &mut chars));
    if value.contains(INCOMPLETE_REDACTION) && !redacted.contains(INCOMPLETE_REDACTION) {
        redacted.push_str(INCOMPLETE_REDACTION);
    }
    escape_terminal_controls(&redacted)
}

pub(super) fn details_body(request: &PermissionRequest) -> PromptBody {
    let mut lines = vec![Line::from(
        "Details: supplied request data (secrets redacted)",
    )];
    let mut remaining = MAX_REVIEW_CHARS;
    for resource in request.resources.iter().take(MAX_REVIEW_NODES) {
        if remaining == 0 {
            lines.push(Line::from(TRUNCATED));
            break;
        }
        lines.push(Line::from(format!(
            "Resource: {}",
            review_text(&bounded_text(&resource.value, &mut remaining))
        )));
        if let Some(workdir) = resource.attributes.get("workdir") {
            lines.push(Line::from(format!(
                "Working directory: {}",
                review_text(&bounded_text(workdir, &mut remaining))
            )));
        }
    }
    if request.resources.len() > MAX_REVIEW_NODES {
        lines.push(Line::from(TRUNCATED));
    }
    if request.input.as_object().is_some_and(|input| {
        [
            "content",
            "patch",
            "patchText",
            "old_string",
            "new_string",
            "oldText",
            "newText",
            "edits",
        ]
        .iter()
        .any(|key| input.contains_key(*key))
    }) {
        lines.push(Line::from(MISSING_BEFORE));
    }
    let input = mask_secrets(&request.input);
    lines.push(Line::from("Supplied input"));
    detail_fields("input", &input, &mut lines, &mut remaining);
    lines.push(Line::from("Technical authority and request data:"));
    let technical = mask_secrets(&serde_json::json!({
        "subject": request.subject,
        "executor": request.executor,
        "input_digest": request.input_digest,
        "resources": request.resources,
        "options": request.options,
    }));
    detail_fields("request", &technical, &mut lines, &mut remaining);
    if remaining == 0 {
        lines.push(Line::from(TRUNCATED));
    }
    PromptBody {
        lines,
        entries: Vec::new(),
    }
}

fn detail_fields(path: &str, value: &Value, lines: &mut Vec<Line<'static>>, remaining: &mut usize) {
    if *remaining == 0 {
        return;
    }
    match value {
        Value::Object(fields) if !fields.is_empty() => {
            for (key, value) in fields {
                detail_fields(&format!("{path}.{key}"), value, lines, remaining);
                if *remaining == 0 {
                    break;
                }
            }
        }
        Value::Array(values) if !values.is_empty() => {
            for (index, value) in values.iter().enumerate() {
                detail_fields(&format!("{path}[{index}]"), value, lines, remaining);
                if *remaining == 0 {
                    break;
                }
            }
        }
        _ => {
            let text = match value {
                Value::String(text) => text.clone(),
                Value::Object(_) | Value::Array(_) => "(empty)".into(),
                Value::Null => "(not supplied)".into(),
                _ => value.to_string(),
            };
            lines.push(Line::from(escape_terminal_controls(&bounded_text(
                path, remaining,
            ))));
            lines.extend(
                bounded_text(&text, remaining)
                    .lines()
                    .map(|line| Line::from(format!("  {}", escape_terminal_controls(line)))),
            );
        }
    }
}

pub(crate) fn likely_secret_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    [
        "password",
        "passwd",
        "secret",
        "token",
        "apikey",
        "authorization",
        "cookie",
        "credential",
        "privatekey",
        "accesskey",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

pub(super) fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::{
        INCOMPLETE_REDACTION, MAX_REVIEW_CHARS, MAX_REVIEW_NODES, REDACTED, TRUNCATED,
        detail_fields, mask_secrets, review_text,
    };

    const SECRET: &str = "never-display-this";

    #[test_case("api_token"; "token")]
    #[test_case("Authorization"; "header")]
    #[test_case("private_key"; "private_key")]
    fn nested_secret_fields_are_redacted(key: &str) {
        let input = json!({"nested": [{key: SECRET, "visible": "keep"}]});
        let shown = mask_secrets(&input).to_string();
        assert!(!shown.contains(SECRET));
        assert!(shown.contains("keep"));
        assert!(shown.contains("redacted:string"));
    }

    #[test_case("curl https://user:never-display-this@example.com/path?q=never-display-this#never-display-this"; "url")]
    #[test_case("curl 'https://user:never-display-this@example.com/path?q=never-display-this'"; "quoted_url")]
    #[test_case("API_TOKEN=never-display-this cargo test"; "assignment")]
    #[test_case("curl --token never-display-this"; "flag")]
    #[test_case("curl -H 'Authorization: Bearer never-display-this'"; "header")]
    #[test_case("-----BEGIN PRIVATE KEY-----\nnever-display-this\n-----END PRIVATE KEY-----"; "pem")]
    fn text_redacts_credentials(text: &str) {
        let shown = review_text(text);
        assert!(!shown.contains(SECRET), "{shown}");
        assert!(shown.contains("redacted"), "{shown}");
    }

    #[test]
    fn details_bound_strings_and_collections_explicitly() {
        let long = "x".repeat(MAX_REVIEW_CHARS + 1);
        assert!(
            mask_secrets(&json!({"command": long}))
                .to_string()
                .contains(TRUNCATED)
        );
        let many = vec![json!(true); MAX_REVIEW_NODES + 1];
        let shown = mask_secrets(&json!(many)).to_string();
        assert!(shown.contains(TRUNCATED));
        assert!(shown.len() < MAX_REVIEW_CHARS);
    }

    #[test]
    fn flattened_paths_cannot_amplify_the_details_budget() {
        let prefix = "x".repeat(MAX_REVIEW_CHARS / 2);
        let value = json!({"rows": vec![true; MAX_REVIEW_NODES]});
        let mut lines = Vec::new();
        let mut remaining = MAX_REVIEW_CHARS;
        detail_fields(&prefix, &value, &mut lines, &mut remaining);
        assert_eq!(remaining, 0);
        let length = lines
            .iter()
            .map(|line| line.to_string().chars().count())
            .sum::<usize>();
        assert!(length <= MAX_REVIEW_CHARS + 3 * TRUNCATED.len());
    }

    #[test]
    fn review_escapes_terminal_controls_without_losing_indentation() {
        assert_eq!(
            review_text("  cargo test\u{1b}[2J"),
            "  cargo test\\u{1b}[2J"
        );
        assert_eq!(
            review_text("-----BEGIN PRIVATE KEY-----"),
            format!("{REDACTED}{INCOMPLETE_REDACTION}")
        );
    }

    #[test_case("env API_TOKEN=never-display-this rm -rf /project", "env API_TOKEN=<redacted> rm -rf /project"; "assignment")]
    #[test_case("env API_TOKEN='never-display-this' rm -rf /project", "env API_TOKEN='<redacted>' rm -rf /project"; "quoted_assignment")]
    #[test_case("env API_TOKEN=never-display-this&&rm -rf /project", "env API_TOKEN=<redacted>&&rm -rf /project"; "operators")]
    #[test_case("curl --token never-display-this ; rm -rf /project", "curl --token <redacted> ; rm -rf /project"; "flag")]
    #[test_case("curl --token 'never-display-this;more' && rm -rf /project", "curl --token '<redacted>' && rm -rf /project"; "quoted_flag")]
    #[test_case("curl -H 'Authorization: Bearer never-display-this' && rm -rf /project", "curl -H 'Authorization:<redacted>' && rm -rf /project"; "header")]
    #[test_case("Authorization: Bearer never-display-this ; rm -rf /project", "Authorization: Bearer <redacted> ; rm -rf /project"; "unquoted_header")]
    #[test_case("API_TOKEN = never-display-this ; rm -rf /project", "API_TOKEN = <redacted> ; rm -rf /project"; "spaced_assignment")]
    fn credential_redaction_preserves_remaining_actions(input: &str, expected: &str) {
        assert_eq!(review_text(input), expected);
    }

    #[test_case("env API_TOKEN='never-display-this rm -rf /project"; "unclosed_value")]
    #[test_case("env API_TOKEN='$(rm -rf /project)' cargo test"; "expansion")]
    fn ambiguous_credential_boundaries_are_explicitly_incomplete(input: &str) {
        let shown = review_text(input);
        assert!(shown.contains(INCOMPLETE_REDACTION));
        assert!(!shown.contains(SECRET));
    }
}
