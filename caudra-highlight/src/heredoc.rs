//! Heredoc bodies a shell line hands to another language, such as the Python
//! in `python3 - <<'PY'`, coloured in that language until the delimiter.

use syntect::highlighting::{HighlightState, Highlighter as SynHighlighter};
use syntect::parsing::{ParseState, ScopeStack};

use crate::syntax_for_token;

/// Commands a heredoc commonly feeds, and the token of the language its body
/// is written in. A command is matched by name, then without a version suffix.
const LANGUAGES: &[(&str, &str)] = &[
    ("python", "python"),
    ("pypy", "python"),
    ("node", "js"),
    ("nodejs", "js"),
    ("deno", "js"),
    ("bun", "js"),
    ("ruby", "ruby"),
    ("perl", "perl"),
    ("php", "php"),
    ("lua", "lua"),
    ("bash", "bash"),
    ("sh", "bash"),
    ("zsh", "bash"),
    ("psql", "sql"),
    ("sqlite3", "sql"),
    ("mysql", "sql"),
];
const OPERATOR: &str = "<<";
const HERE_STRING: char = '<';
const STRIP_TABS: char = '-';
const COMMENT: char = '#';
const ESCAPE: char = '\\';
const ASSIGNMENT: char = '=';
const PATH_SEPARATOR: char = '/';
const VERSION_SEPARATOR: char = '.';
const QUOTES: [char; 2] = ['\'', '"'];
const BLANKS: [char; 2] = [' ', '\t'];
const LINE_ENDINGS: [char; 2] = ['\n', '\r'];
/// What ends one command and starts the next, so the last of these before the
/// operator starts the command the heredoc feeds.
const COMMAND_BREAKS: [char; 5] = ['|', '&', ';', '(', ')'];
const WORD_ENDS: [char; 7] = ['|', '&', ';', '(', ')', '<', '>'];

/// What a shell highlighter is in the middle of, from one line to the next.
#[derive(Clone)]
pub(crate) enum Embedding {
    /// Ordinary shell lines, any of which may open a heredoc.
    Shell,
    /// The body of a heredoc, up to the line that closes it.
    Heredoc(Box<Heredoc>),
}

#[derive(Clone)]
pub(crate) struct Heredoc {
    delimiter: String,
    strip_tabs: bool,
    /// The body's own grammar, or `None` when the shell grammar colours it.
    pub(crate) body: Option<(ParseState, HighlightState)>,
}

impl Embedding {
    /// Where a shell line leaves the highlighter: in the body of the heredoc
    /// it opens, or still in shell.
    pub(crate) fn after(line: &str, highlighter: &SynHighlighter) -> Self {
        opened_by(line).map_or(Self::Shell, |opening| {
            Self::Heredoc(Box::new(Heredoc {
                body: opening.language.map(|token| {
                    (
                        ParseState::new(syntax_for_token(token)),
                        HighlightState::new(highlighter, ScopeStack::new()),
                    )
                }),
                delimiter: opening.delimiter,
                strip_tabs: opening.strip_tabs,
            }))
        })
    }
}

impl Heredoc {
    /// Whether `line` is the delimiter, after the leading tabs `<<-` strips.
    pub(crate) fn closed_by(&self, line: &str) -> bool {
        let line = line.trim_end_matches(LINE_ENDINGS);
        let line = match self.strip_tabs {
            true => line.trim_start_matches('\t'),
            false => line,
        };
        line == self.delimiter
    }
}

#[derive(Debug, PartialEq)]
struct Opening {
    delimiter: String,
    strip_tabs: bool,
    language: Option<&'static str>,
}

/// The first heredoc `line` opens outside quotes and comments. A here-string
/// (`<<<`) feeds one word, not the lines after it, so it opens nothing.
fn opened_by(line: &str) -> Option<Opening> {
    let mut quote = None;
    let mut escaped = false;
    let mut command = 0;
    let mut word_start = true;
    let mut characters = line.char_indices();
    while let Some((index, character)) = characters.next() {
        let starts_word = word_start;
        word_start = character.is_whitespace();
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, character) {
            (Some(open), _) if character == open => quote = None,
            (Some('\''), _) => {}
            (_, ESCAPE) => escaped = true,
            (Some(_), _) => {}
            (None, _) if QUOTES.contains(&character) => quote = Some(character),
            (None, COMMENT) if starts_word => return None,
            (None, _) if line[index..].starts_with(OPERATOR) => {
                let rest = &line[index + OPERATOR.len()..];
                if rest.starts_with(HERE_STRING) {
                    characters.nth(1);
                    continue;
                }
                return opening(&line[command..index], rest);
            }
            (None, _) if COMMAND_BREAKS.contains(&character) => {
                command = index + character.len_utf8();
                word_start = true;
            }
            _ => {}
        }
    }
    None
}

fn opening(command: &str, rest: &str) -> Option<Opening> {
    let (strip_tabs, rest) = match rest.strip_prefix(STRIP_TABS) {
        Some(rest) => (true, rest),
        None => (false, rest),
    };
    let delimiter = delimiter(rest);
    if delimiter.is_empty() {
        return None;
    }
    Some(Opening {
        delimiter,
        strip_tabs,
        language: command
            .split_whitespace()
            .find(|word| !word.contains(ASSIGNMENT))
            .and_then(language),
    })
}

/// The delimiter word with its quotes and escapes removed, which is the line
/// that ends the body.
fn delimiter(rest: &str) -> String {
    let mut delimiter = String::new();
    let mut quote = None;
    for character in rest.trim_start_matches(BLANKS).chars() {
        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => delimiter.push(character),
            None if QUOTES.contains(&character) => quote = Some(character),
            None if character == ESCAPE => {}
            None if character.is_whitespace() || WORD_ENDS.contains(&character) => break,
            None => delimiter.push(character),
        }
    }
    delimiter
}

fn language(command: &str) -> Option<&'static str> {
    let name = command.rsplit(PATH_SEPARATOR).next().unwrap_or(command);
    let unversioned = name.trim_end_matches(|character: char| {
        character.is_ascii_digit() || character == VERSION_SEPARATOR
    });
    LANGUAGES
        .iter()
        .find(|(known, _)| *known == name || *known == unversioned)
        .map(|(_, token)| *token)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{Opening, opened_by};

    #[test_case("python3 - <<'PY'", "PY", false, Some("python"); "a_quoted_python_delimiter")]
    #[test_case("cd /tmp && /usr/bin/python3.12 - <<EOF", "EOF", false, Some("python"); "a_versioned_path_after_a_list")]
    #[test_case("PYTHONPATH=src python3 <<-\"END\"", "END", true, Some("python"); "an_assignment_and_tab_stripping")]
    #[test_case("sqlite3 db.sqlite <<'SQL'", "SQL", false, Some("sql"); "an_exact_name_with_a_digit")]
    #[test_case("x=$(node <<JS", "JS", false, Some("js"); "inside_a_substitution")]
    #[test_case("cat <<EOF > notes.md", "EOF", false, None; "a_command_with_no_language")]
    #[test_case("cat <<\\E'N'D", "END", false, None; "an_escaped_and_partly_quoted_delimiter")]
    #[test_case("cat <<<'x' && python3 - <<'PY'", "PY", false, Some("python"); "after_a_here_string")]
    fn a_heredoc_names_its_delimiter_and_language(
        line: &str,
        delimiter: &str,
        strip_tabs: bool,
        language: Option<&'static str>,
    ) {
        assert_eq!(
            opened_by(line),
            Some(Opening {
                delimiter: delimiter.into(),
                strip_tabs,
                language,
            })
        );
    }

    #[test_case("python3 - <<<'print(1)'"; "a_here_string")]
    #[test_case("echo 'python3 - <<PY'"; "inside_single_quotes")]
    #[test_case("echo \"a <<PY\""; "inside_double_quotes")]
    #[test_case("echo a \\<<PY"; "an_escaped_operator")]
    #[test_case("ls # python3 <<PY"; "inside_a_comment")]
    #[test_case("python3 - <<"; "no_delimiter")]
    fn a_line_opens_no_heredoc(line: &str) {
        assert_eq!(opened_by(line), None);
    }
}
