//! What the shell ETA model knows of a command: its words, adjacent word
//! pairs and a length bucket, each kept only as an FNV-1a hash. Heredoc bodies
//! and redirections are removed first, so a script's contents never count.

use std::sync::LazyLock;

use regex::{Captures, Regex};

const HEREDOC_MARK: &str = " HEREDOC ";
const LENGTH_PREFIX: &str = "LEN";
const MAX_LENGTH_BUCKET: u32 = 6;
const MAX_COMMAND_BYTES: usize = 4096;
pub const MAX_FEATURES: usize = 256;
const OPERATORS: [&str; 5] = ["|", "||", "&&", ";", "&"];
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
/// Present in every command, so the classifier has a prior to learn.
pub const BIAS: u64 = fnv1a(b"BIAS");

static REDIRECT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d*>>?&?\d*|&>>?|\d*<+").expect("literal"));
static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"--?\w[\w.\-]*|[A-Za-z_./~][\w.\-/~+:@=]*|\d+(?:\.\d+)?[smhd]?|\|\||&&|[|;&]")
        .expect("literal")
});

/// The sorted, distinct feature hashes of `command`, at most [`MAX_FEATURES`]
/// of them and always including [`BIAS`].
pub fn features(command: &str) -> Vec<u64> {
    let body = collapse_heredocs(prefix(command, MAX_COMMAND_BYTES));
    let length = u64::try_from(body.chars().count()).unwrap_or(u64::MAX);
    let stripped = REDIRECT.replace_all(&body, |caps: &Captures| strip_redirect(&caps[0]));
    let words: Vec<&str> = TOKEN
        .find_iter(&stripped)
        .map(|token| token.as_str())
        .filter(|token| !OPERATORS.contains(token))
        .collect();
    let mut hashes: Vec<u64> = words.iter().map(|word| fnv1a(word.as_bytes())).collect();
    hashes.extend(
        words
            .windows(2)
            .map(|pair| fnv1a(format!("{} {}", pair[0], pair[1]).as_bytes())),
    );
    hashes.push(fnv1a(
        format!(
            "{LENGTH_PREFIX}{}",
            bit_length(length).min(MAX_LENGTH_BUCKET)
        )
        .as_bytes(),
    ));
    hashes.sort_unstable();
    hashes.dedup();
    hashes.retain(|hash| *hash != BIAS);
    hashes.truncate(MAX_FEATURES - 1);
    hashes.push(BIAS);
    hashes.sort_unstable();
    hashes
}

pub(crate) const fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    let mut index = 0;
    while index < bytes.len() {
        hash = (hash ^ bytes[index] as u64).wrapping_mul(FNV_PRIME);
        index += 1;
    }
    hash
}

fn prefix(text: &str, max_bytes: usize) -> &str {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Drops a `<` that no `<` follows, with its digits only when it stands alone.
fn strip_redirect(found: &str) -> String {
    let arrows = found.chars().rev().take_while(|c| *c == '<').count();
    match arrows {
        0 | 1 => " ".to_owned(),
        _ => format!("{} ", &found[..found.len() - 1]),
    }
}

fn bit_length(value: u64) -> u32 {
    u64::BITS - value.leading_zeros()
}

/// Replaces each `<<[-]DELIM ... DELIM` with a marker.
fn collapse_heredocs(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let mut rest = command;
    while let Some(start) = rest.find("<<") {
        match heredoc_end(&rest[start..]) {
            Some(end) => {
                out.push_str(&rest[..start]);
                out.push_str(HEREDOC_MARK);
                rest = &rest[start + end..];
            }
            None => {
                out.push_str(&rest[..start + 1]);
                rest = &rest[start + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn heredoc_end(text: &str) -> Option<usize> {
    let mut at = 2;
    if text[at..].starts_with('-') {
        at += 1;
    }
    at += whitespace_len(&text[at..]);
    let quote = text[at..]
        .chars()
        .next()
        .filter(|c| *c == '\'' || *c == '"');
    if quote.is_some() {
        at += 1;
    }
    let delimiter_len: usize = text[at..]
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .map(char::len_utf8)
        .sum();
    if delimiter_len == 0 {
        return None;
    }
    let delimiter = &text[at..at + delimiter_len];
    at += delimiter_len;
    if let Some(quote) = quote {
        if !text[at..].starts_with(quote) {
            return None;
        }
        at += 1;
    }
    let body_start = at + text[at..].find('\n')? + 1;
    let mut search = body_start;
    while let Some(offset) = text[search..].find('\n') {
        let newline = search + offset;
        let candidate = newline + 1 + whitespace_len(&text[newline + 1..]);
        if text[candidate..].starts_with(delimiter)
            && let Some(end) = closing_end(text, candidate + delimiter.len())
        {
            return Some(end);
        }
        search = newline + 1;
    }
    None
}

/// The whitespace after a closing delimiter must reach a newline or the end.
fn closing_end(text: &str, after: usize) -> Option<usize> {
    let end = after + whitespace_len(&text[after..]);
    if end == text.len() {
        return Some(end);
    }
    let last_newline = text[after..end].rfind('\n')?;
    Some(after + last_newline)
}

fn whitespace_len(text: &str) -> usize {
    text.chars()
        .take_while(|c| c.is_whitespace())
        .map(char::len_utf8)
        .sum()
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{BIAS, MAX_FEATURES, collapse_heredocs, features, fnv1a};

    fn hashed(words: &[&str]) -> Vec<u64> {
        let mut hashes: Vec<u64> = words.iter().map(|word| fnv1a(word.as_bytes())).collect();
        hashes.push(BIAS);
        hashes.sort_unstable();
        hashes
    }

    #[test_case("", &["LEN0"]; "empty")]
    #[test_case("git status", &["git", "status", "git status", "LEN4"]; "words_and_pair")]
    #[test_case("cargo test 2>&1 | tail -20", &["cargo", "test", "tail", "-20", "cargo test", "test tail", "tail -20", "LEN5"]; "redirect_and_pipe_dropped")]
    #[test_case("python3 - <<'PY'\nimport os\nPY\necho done", &["python3", "HEREDOC", "echo", "done", "python3 HEREDOC", "HEREDOC echo", "echo done", "LEN5"]; "heredoc_body_dropped")]
    #[test_case("RUST_LOG=debug make test && echo ok", &["RUST_LOG=debug", "make", "test", "echo", "ok", "RUST_LOG=debug make", "make test", "test echo", "echo ok", "LEN6"]; "env_prefix_and_operators")]
    fn features_are_words_pairs_and_length(command: &str, expected: &[&str]) {
        assert_eq!(features(command), hashed(expected));
    }

    #[test]
    fn heredoc_body_is_collapsed() {
        assert_eq!(
            collapse_heredocs("cat <<-\"EOF\"\n  secret\n  EOF\nls"),
            "cat  HEREDOC \nls"
        );
    }

    #[test]
    fn a_long_command_keeps_the_bias_within_the_cap() {
        let command: String = (0..MAX_FEATURES).map(|word| format!("w{word} ")).collect();
        let hashes = features(&command);
        assert_eq!(hashes.len(), MAX_FEATURES);
        assert!(hashes.contains(&BIAS));
    }

    #[test]
    fn fnv1a_matches_the_reference_vector() {
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
