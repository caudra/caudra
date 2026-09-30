/// Zola's default `on` heading ids: ASCII letters and digits lowercased, every other run of characters one dash,
/// and no dash at either end.
pub(crate) fn slugify(text: &str) -> String {
    let mut slug = String::with_capacity(text.len());
    let mut pending_dash = false;
    for ch in text.chars() {
        if !ch.is_ascii_alphanumeric() {
            pending_dash = true;
            continue;
        }
        if pending_dash && !slug.is_empty() {
            slug.push('-');
        }
        pending_dash = false;
        slug.push(ch.to_ascii_lowercase());
    }
    slug
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::slugify;

    #[test_case("Stored rules", "stored-rules" ; "spaces")]
    #[test_case("Agent & Knowledge", "agent-knowledge" ; "punctuation run")]
    #[test_case("SDK / Stream Mode", "sdk-stream-mode" ; "slash")]
    #[test_case("max_output_bytes", "max-output-bytes" ; "underscores")]
    #[test_case("  (Leading) and trailing!  ", "leading-and-trailing" ; "trimmed dashes")]
    #[test_case("caudra.agent.Session", "caudra-agent-session" ; "dots and case")]
    fn slugify_follows_zola(text: &str, expected: &str) {
        assert_eq!(slugify(text), expected);
    }
}
