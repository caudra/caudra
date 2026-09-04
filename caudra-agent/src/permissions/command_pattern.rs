pub(super) const MAX_PATTERN_TOKENS: usize = 8;
const MAX_PREFIX_LITERALS: usize = 3;

pub(crate) const BUILTIN_ASK_PATTERNS: &[&str] = &[
    "rm *",
    "git push *",
    "git reset *",
    "git checkout *",
    "git rebase *",
    "git clean *",
    "chmod *",
    "chown *",
    "dd *",
    "mkfs *",
    "curl *",
    "wget *",
    "ssh *",
    "scp *",
    "kill *",
    "pkill *",
];

#[derive(Clone, Copy)]
enum Quote {
    Single,
    Double,
}

struct Pattern<'a> {
    literals: Vec<&'a str>,
    wildcard: bool,
}

pub(crate) fn tokenize(command: &str) -> Option<Vec<&str>> {
    let mut tokens = Vec::new();
    let mut token_start = None;
    let mut quote = None;
    let mut chars = command.char_indices().peekable();

    while let Some((index, character)) = chars.next() {
        match quote {
            None if matches!(character, '\n' | '\r') => return None,
            None if character.is_whitespace() => {
                if let Some(start) = token_start.take() {
                    tokens.push(&command[start..index]);
                }
            }
            None => {
                token_start.get_or_insert(index);
                match character {
                    '$' if chars
                        .peek()
                        .is_some_and(|(_, next)| matches!(next, '\'' | '"')) =>
                    {
                        return None;
                    }
                    '\'' => quote = Some(Quote::Single),
                    '"' => quote = Some(Quote::Double),
                    '\\' => {
                        chars.next()?;
                    }
                    _ => {}
                }
            }
            Some(Quote::Single) => {
                if character == '\'' {
                    quote = None;
                }
            }
            Some(Quote::Double) => match character {
                '"' => quote = None,
                '\\' if chars
                    .peek()
                    .is_some_and(|(_, next)| matches!(next, '$' | '`' | '"' | '\\' | '\n')) =>
                {
                    chars.next();
                }
                _ => {}
            },
        }
    }

    if quote.is_some() {
        return None;
    }
    if let Some(start) = token_start {
        tokens.push(&command[start..]);
    }
    Some(tokens)
}

pub(crate) fn matches(pattern: &str, command: &str) -> bool {
    let Some(pattern) = parse_pattern(pattern) else {
        return false;
    };
    let Some(command_tokens) = tokenize(command) else {
        return false;
    };
    if command_tokens.len() < pattern.literals.len()
        || (!pattern.wildcard && command_tokens.len() != pattern.literals.len())
    {
        return false;
    }

    let prefix_matches = pattern
        .literals
        .iter()
        .zip(&command_tokens)
        .all(|(literal, token)| decode_static_token(token).as_deref() == Some(*literal));
    prefix_matches
        && (!pattern.wildcard
            || command_tokens[pattern.literals.len()..]
                .iter()
                .all(|token| !contains_unquoted_shell_operator(token)))
}

pub(crate) fn reusable_prefix(command: &str) -> Option<String> {
    let tokens = tokenize(command)?;
    let decoded: Vec<String> = tokens
        .into_iter()
        .map(decode_static_token)
        .collect::<Option<_>>()?;
    if !is_pattern_literal(decoded.first()?) {
        return None;
    }

    let literals = match super::command_arity::curated_literals(&decoded) {
        Some(curated) => curated_literals(&decoded, curated)?,
        None => heuristic_literals(&decoded)?,
    };
    if overlaps_builtin_ask(literals) {
        return None;
    }

    Some(format!("{} *", literals.join(" ")))
}

/// A curated entry is a deliberate decision, so it may keep a bare executable or
/// the whole command where the heuristic may not. It still refuses to reach past
/// a flag, because `git -C /repo commit` would otherwise yield `git -C *`.
fn curated_literals(decoded: &[String], literals: usize) -> Option<&[String]> {
    (literals <= decoded.len()
        && decoded[1..literals]
            .iter()
            .all(|token| is_subcommand_word(token)))
    .then(|| &decoded[..literals])
}

/// Without curation the leading tokens are only guessed to be subcommands, so a
/// guess that degenerates to the bare executable or swallows every operand is
/// discarded rather than offered.
fn heuristic_literals(decoded: &[String]) -> Option<&[String]> {
    if decoded.get(1)?.starts_with('-') {
        return None;
    }
    let literals = decoded
        .iter()
        .skip(1)
        .take(MAX_PREFIX_LITERALS - 1)
        .take_while(|token| is_subcommand_word(token))
        .count()
        + 1;
    (literals > 1 && literals < decoded.len()).then(|| &decoded[..literals])
}

/// A pattern that shares a prefix with an ask family in either direction covers
/// or is covered by it, and a stored rule silences the ask entirely.
fn overlaps_builtin_ask(literals: &[String]) -> bool {
    BUILTIN_ASK_PATTERNS.iter().any(|pattern| {
        pattern
            .strip_suffix(" *")
            .unwrap_or(pattern)
            .split_whitespace()
            .zip(literals)
            .all(|(ask, literal)| ask == literal.as_str())
    })
}

pub(crate) fn specificity(pattern: &str) -> Option<(usize, usize)> {
    let pattern = parse_pattern(pattern)?;
    Some((
        pattern.literals.len(),
        pattern.literals.iter().map(|literal| literal.len()).sum(),
    ))
}

fn parse_pattern(pattern: &str) -> Option<Pattern<'_>> {
    let mut tokens: Vec<_> = pattern.split_whitespace().collect();
    if tokens.is_empty() || tokens.len() > MAX_PATTERN_TOKENS {
        return None;
    }
    let wildcard = tokens.last() == Some(&"*");
    if wildcard {
        tokens.pop();
    }
    if (!wildcard && tokens.is_empty()) || !tokens.iter().all(|token| is_pattern_literal(token)) {
        return None;
    }
    Some(Pattern {
        literals: tokens,
        wildcard,
    })
}

fn is_pattern_literal(token: &str) -> bool {
    !token.is_empty()
        && token.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b'/' | b'@' | b':' | b'=' | b'+' | b'-')
        })
}

fn is_subcommand_word(token: &str) -> bool {
    let mut bytes = token.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn contains_unquoted_shell_operator(token: &str) -> bool {
    let mut quote = None;
    let mut chars = token.chars().peekable();
    while let Some(character) = chars.next() {
        let substitutes = character == '`' || (character == '$' && chars.peek() == Some(&'('));
        match quote {
            None => match character {
                '\'' => quote = Some(Quote::Single),
                '"' => quote = Some(Quote::Double),
                '\\' => {
                    chars.next();
                }
                '<' | '>' | '|' | '&' | ';' => return true,
                _ if substitutes => return true,
                _ => {}
            },
            Some(Quote::Single) if character == '\'' => quote = None,
            Some(Quote::Double) if character == '"' => quote = None,
            Some(Quote::Double) if character == '\\' => {
                chars.next();
            }
            Some(Quote::Double) if substitutes => return true,
            Some(Quote::Single | Quote::Double) => {}
        }
    }
    false
}

fn decode_static_token(token: &str) -> Option<String> {
    let mut decoded = String::with_capacity(token.len());
    let mut quote = None;
    let mut chars = token.chars().peekable();

    while let Some(character) = chars.next() {
        match quote {
            None => match character {
                '\'' => quote = Some(Quote::Single),
                '"' => quote = Some(Quote::Double),
                '\\' => match chars.next()? {
                    '\n' => {}
                    escaped => decoded.push(escaped),
                },
                '$' | '`' | '*' | '?' | '[' | '{' | '~' | '(' | ')' => return None,
                '<' | '>' if chars.peek() == Some(&'(') => return None,
                _ => decoded.push(character),
            },
            Some(Quote::Single) => {
                if character == '\'' {
                    quote = None;
                } else {
                    decoded.push(character);
                }
            }
            Some(Quote::Double) => match character {
                '"' => quote = None,
                '\\' => match chars.next()? {
                    '\n' => {}
                    escaped @ ('$' | '`' | '"' | '\\') => decoded.push(escaped),
                    escaped => {
                        decoded.push('\\');
                        decoded.push(escaped);
                    }
                },
                '$' | '`' => return None,
                _ => decoded.push(character),
            },
        }
    }

    quote.is_none().then_some(decoded)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{BUILTIN_ASK_PATTERNS, matches, reusable_prefix, specificity, tokenize};

    #[test]
    fn tokenize_preserves_source_spelling() {
        assert_eq!(
            tokenize(r#"git commit -m "message" 'other value' escaped\ space"#),
            Some(vec![
                "git",
                "commit",
                "-m",
                r#""message""#,
                "'other value'",
                r"escaped\ space",
            ])
        );
        assert_eq!(
            tokenize(r#"say '' a"b"c"#),
            Some(vec!["say", "''", r#"a"b"c"#])
        );
        assert_eq!(tokenize(""), Some(Vec::new()));
    }

    #[test_case("echo 'unterminated"; "single quote")]
    #[test_case(r#"echo "unterminated"#; "double quote")]
    #[test_case(r"echo trailing\"; "trailing escape")]
    fn tokenize_rejects_malformed_input(command: &str) {
        assert_eq!(tokenize(command), None);
    }

    #[test_case("git commit -m message", r#"git commit -m "message""#)]
    #[test_case("git commit -m message", "git commit -m 'message'")]
    #[test_case("git commit -m message", r"git commit -m mes\sage")]
    #[test_case("git status *", "git status")]
    #[test_case("git status *", "git status --short")]
    #[test_case("git status *", r#"git status "$FORMAT""#)]
    fn patterns_match_static_literals_and_optional_wildcard(pattern: &str, command: &str) {
        assert!(matches(pattern, command));
    }

    #[test_case("git status", "git status --short"; "exact token count")]
    #[test_case("git status*", "git status"; "wildcard inside token")]
    #[test_case("git * status", "git status"; "nonfinal wildcard")]
    #[test_case("ls*", "lsof"; "no joined wildcard semantics")]
    #[test_case("ls", "lsof"; "no textual prefix semantics")]
    #[test_case("git commit -m message", r#"git commit -m "$MESSAGE""#; "parameter expansion")]
    #[test_case("git status", "git `status`"; "command expansion")]
    #[test_case("git status", r#"git "unterminated"#; "malformed command")]
    #[test_case("git status *", "git status > /tmp/status"; "output redirect")]
    #[test_case("git status *", "git status 2>>errors"; "fd redirect")]
    #[test_case("git status *", "git status <input"; "input redirect")]
    #[test_case("git status *", "git status\nrm -rf /"; "command separator newline")]
    #[test_case("git status *", r"git status $'a\'b' > victim"; "ansi c quote")]
    #[test_case("git status *", "/tmp/git status"; "path qualified executable")]
    #[test_case("git status *", "git status $(id)"; "command substitution")]
    #[test_case("git status *", r#"git status "$(id)""#; "quoted command substitution")]
    #[test_case("git status *", "git status `id`"; "backtick substitution")]
    #[test_case("git status *", r#"git status "`id`""#; "quoted backtick substitution")]
    fn patterns_reject_invalid_or_nonliteral_matches(pattern: &str, command: &str) {
        assert!(!matches(pattern, command));
    }

    #[test_case("git diff --check -- file", Some("git diff *"))]
    #[test_case("git status --short", Some("git status *"))]
    #[test_case("git -c foo.bar=1 push", None)]
    #[test_case("rm -rf build", None)]
    #[test_case(r"printf '%s\n' x", None)]
    #[test_case("cargo nextest run --workspace", Some("cargo nextest run *"))]
    #[test_case(r#"git "status" --short"#, Some("git status *"))]
    #[test_case("git status", None; "complete prefix")]
    #[test_case(r#"git status "$FORMAT""#, None; "parameter expansion")]
    #[test_case("git status $(format)", None; "command substitution")]
    #[test_case("git status *.rs", None; "glob expansion")]
    #[test_case(r#"git status "unterminated"#, None; "malformed input")]
    #[test_case("rg foo src/", Some("rg *"); "curated tool keeps its operand out")]
    #[test_case("rg -n foo", Some("rg *"); "curated tool reaches past a leading flag")]
    #[test_case("wc -l src/lib.rs", Some("wc *"))]
    #[test_case("ls -la", Some("ls *"))]
    #[test_case("npm run build", Some("npm run build *"); "curated prefix takes every operand")]
    #[test_case("npm install react", Some("npm install *"))]
    #[test_case("uv run pytest tests/", Some("uv run pytest *"))]
    #[test_case("just check", Some("just check *"); "curated prefix is the whole command")]
    #[test_case("make lint", Some("make lint *"))]
    #[test_case("docker run nginx", Some("docker run *"); "curated prefix caps the heuristic")]
    #[test_case("docker compose up -d", Some("docker compose up *"))]
    #[test_case("git stash pop", Some("git stash pop *"))]
    #[test_case("go build ./...", Some("go build *"))]
    #[test_case("go run ./cmd/app", None; "curated prefix stops at a path operand")]
    #[test_case("docker -H tcp://host run nginx", None; "curated prefix stops at a flag")]
    #[test_case("git checkout main --force", None; "prefix of a builtin ask family")]
    #[test_case("ssh host run backup", None; "extends a builtin ask family")]
    fn derives_only_reusable_static_prefixes(command: &str, expected: Option<&str>) {
        let derived = reusable_prefix(command);
        assert_eq!(derived.as_deref(), expected);
        if let Some(pattern) = derived {
            assert!(
                specificity(&pattern).is_some(),
                "{pattern} is not a pattern"
            );
            assert!(matches(&pattern, command), "{pattern} misses {command}");
        }
    }

    #[test]
    fn validates_pattern_grammar_and_calculates_specificity() {
        assert_eq!(specificity("git push *"), Some((2, 7)));
        assert_eq!(specificity("a b c d e f g *"), Some((7, 7)));
        assert_eq!(specificity("a b c d e f g h i"), None);
        assert_eq!(specificity(""), None);
        assert_eq!(specificity("*"), Some((0, 0)));
        assert_eq!(specificity("git status*"), None);
        assert_eq!(specificity("git * status"), None);
        assert_eq!(specificity("echo comma,"), None);
        assert_eq!(specificity("azAZ09._/@:=+-"), Some((1, 14)));
    }

    #[test]
    fn builtin_ask_patterns_are_exact() {
        assert_eq!(
            BUILTIN_ASK_PATTERNS,
            &[
                "rm *",
                "git push *",
                "git reset *",
                "git checkout *",
                "git rebase *",
                "git clean *",
                "chmod *",
                "chown *",
                "dd *",
                "mkfs *",
                "curl *",
                "wget *",
                "ssh *",
                "scp *",
                "kill *",
                "pkill *",
            ]
        );
    }
}
