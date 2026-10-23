//! Python-compatible lexing, so a reading matches the reference model's:
//! `re.sub(HEREDOC, ...)` and `shlex.shlex(posix=True,
//! punctuation_chars=';&|()<>')` with `whitespace_split` and `#` comments.

use std::borrow::Cow;
use std::iter::Peekable;
use std::str::Chars;

const HEREDOC_MARK: &str = " __HEREDOC__ ";

/// Python's `str.isspace`, which `\s` and `str.split()` use.
pub(super) fn py_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\x1c'..='\x1f')
}

/// Python's `\w` for `str` patterns, except for `Other_Alphabetic` marks.
pub(super) fn py_word(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

fn skip_while(text: &str, from: usize, keep: impl Fn(char) -> bool) -> usize {
    text[from..]
        .find(|c| !keep(c))
        .map_or(text.len(), |offset| from + offset)
}

/// `HEREDOC.sub(' __HEREDOC__ ', text)` for
/// `<<-?\s*(['"]?)(\w+)\1[^\n]*\n.*?\n\s*\2[ \t]*(?=\n|$)` under `re.S`.
pub(super) fn strip_heredocs(text: &str) -> Cow<'_, str> {
    let mut out = String::new();
    let mut copied = 0;
    let mut pos = 0;
    while let Some(offset) = text[pos..].find("<<") {
        let start = pos + offset;
        match heredoc_end(text, start) {
            Some(end) => {
                out.push_str(&text[copied..start]);
                out.push_str(HEREDOC_MARK);
                copied = end;
                pos = end;
            }
            None => pos = start + 1,
        }
    }
    if copied == 0 {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

/// The end of the heredoc match starting at `start`, following the backtracking
/// order of Python's regex engine: only the unquoted delimiter's length is a
/// real choice.
fn heredoc_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut pos = start + 2;
    if bytes.get(pos) == Some(&b'-') {
        pos += 1;
    }
    pos = skip_while(text, pos, py_space);
    let quote = bytes
        .get(pos)
        .copied()
        .filter(|b| matches!(b, b'\'' | b'"'));
    if quote.is_some() {
        pos += 1;
    }
    let word_end = skip_while(text, pos, py_word);
    let mut delimiter_ends: Vec<usize> = text[pos..word_end]
        .char_indices()
        .map(|(i, c)| pos + i + c.len_utf8())
        .collect();
    if quote.is_some() {
        delimiter_ends.drain(..delimiter_ends.len().saturating_sub(1));
    }
    for &delimiter_end in delimiter_ends.iter().rev() {
        let mut after = delimiter_end;
        if let Some(quote) = quote {
            if bytes.get(after) != Some(&quote) {
                continue;
            }
            after += 1;
        }
        let opening_newline = after + text[after..].find('\n')?;
        if let Some(end) = closing_delimiter(text, opening_newline + 1, &text[pos..delimiter_end]) {
            return Some(end);
        }
    }
    None
}

fn closing_delimiter(text: &str, body_start: usize, delimiter: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut from = body_start;
    while let Some(offset) = text[from..].find('\n') {
        from += offset + 1;
        let word = skip_while(text, from, py_space);
        if !text[word..].starts_with(delimiter) {
            continue;
        }
        let end = skip_while(text, word + delimiter.len(), |c| c == ' ' || c == '\t');
        if end == text.len() || bytes[end] == b'\n' {
            return Some(end);
        }
    }
    None
}

/// The tokens Python's shlex yields, or `body.split()` when shlex raises
/// `ValueError`.
pub(super) fn tokens(body: &str) -> Vec<String> {
    shlex(body).unwrap_or_else(|| {
        body.split(py_space)
            .filter(|word| !word.is_empty())
            .map(String::from)
            .collect()
    })
}

fn is_punctuation(c: char) -> bool {
    matches!(c, ';' | '&' | '|' | '(' | ')' | '<' | '>')
}

fn is_shlex_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

fn skip_comment(chars: &mut Peekable<Chars>) {
    for c in chars.by_ref() {
        if c == '\n' {
            break;
        }
    }
}

/// The body of a quoted span after its opening quote; `None` is shlex's
/// `ValueError`.
fn read_quoted(chars: &mut Peekable<Chars>, quote: char, token: &mut String) -> Option<()> {
    loop {
        match chars.next()? {
            c if c == quote => return Some(()),
            '\\' if quote == '"' => {
                let escaped = chars.next()?;
                if escaped != '\\' && escaped != '"' {
                    token.push('\\');
                }
                token.push(escaped);
            }
            c => token.push(c),
        }
    }
}

fn shlex(body: &str) -> Option<Vec<String>> {
    let mut chars = body.chars().peekable();
    let mut tokens = Vec::new();
    loop {
        let mut token = String::new();
        let punctuation = loop {
            let Some(c) = chars.next() else {
                return Some(tokens);
            };
            match c {
                c if is_shlex_space(c) => {}
                '#' => skip_comment(&mut chars),
                '\\' => {
                    token.push(chars.next()?);
                    break false;
                }
                c if is_punctuation(c) => {
                    token.push(c);
                    break true;
                }
                '\'' | '"' => {
                    read_quoted(&mut chars, c, &mut token)?;
                    break false;
                }
                c => {
                    token.push(c);
                    break false;
                }
            }
        };
        let ended = if punctuation {
            read_punctuation(&mut chars, &mut token)
        } else {
            read_word(&mut chars, &mut token)?
        };
        tokens.push(token);
        if ended {
            return Some(tokens);
        }
    }
}

/// Extends a punctuation run; returns whether the input ended.
fn read_punctuation(chars: &mut Peekable<Chars>, token: &mut String) -> bool {
    loop {
        match chars.peek().copied() {
            None => return true,
            Some(c) if is_shlex_space(c) => {
                chars.next();
                return false;
            }
            Some('#') => {
                chars.next();
                skip_comment(chars);
                return false;
            }
            Some(c) if is_punctuation(c) => {
                chars.next();
                token.push(c);
            }
            Some(_) => return false,
        }
    }
}

/// Extends a word; returns whether the input ended, or `None` for shlex's
/// `ValueError`.
fn read_word(chars: &mut Peekable<Chars>, token: &mut String) -> Option<bool> {
    loop {
        match chars.peek().copied() {
            None => return Some(true),
            Some(c) if is_shlex_space(c) => {
                chars.next();
                return Some(false);
            }
            Some('#') => {
                chars.next();
                skip_comment(chars);
                return Some(false);
            }
            Some(c) if is_punctuation(c) => return Some(false),
            Some(c) => {
                chars.next();
                match c {
                    '\'' | '"' => read_quoted(chars, c, token)?,
                    '\\' => token.push(chars.next()?),
                    c => token.push(c),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{strip_heredocs, tokens};

    #[test_case("a#b c", &["a"]; "comment_ends_the_word")]
    #[test_case("a \"\" b", &["a", "", "b"]; "empty_quotes_are_a_word")]
    #[test_case("x 2>&1|head", &["x", "2", ">&", "1", "|", "head"]; "redirect_and_pipe")]
    #[test_case("a\\\nb", &["a\nb"]; "escaped_newline")]
    #[test_case("a;#c\nd", &["a", ";", "d"]; "comment_after_punctuation")]
    #[test_case("echo \"a\\$b\\\"c\"", &["echo", "a\\$b\"c"]; "double_quote_escapes")]
    #[test_case("&&#x\ny", &["&&", "y"]; "comment_after_operator")]
    #[test_case("a|&b", &["a", "|&", "b"]; "pipe_with_stderr")]
    #[test_case("echo 'a b", &["echo", "'a", "b"]; "unclosed_quote_falls_back_to_split")]
    fn tokens_match_python_shlex(input: &str, expected: &[&str]) {
        assert_eq!(tokens(input), expected);
    }

    #[test_case("cat <<EOF x\nbody\n  EO\nrest", "cat  __HEREDOC__ \nrest"; "unquoted_delimiter_backtracks")]
    #[test_case("cat <<'EOF'\nbody\nEOF", "cat  __HEREDOC__ "; "quoted_delimiter")]
    #[test_case("cat <<EOF\nnever closed", "cat <<EOF\nnever closed"; "unclosed_is_kept")]
    fn heredocs_are_stripped(input: &str, expected: &str) {
        assert_eq!(strip_heredocs(input), expected);
    }
}
