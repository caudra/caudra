use thiserror::Error;

use super::structured::{BROAD_SHELL_PHRASE, PermissionCaution};

pub(super) const MAX_PATTERN_TOKENS: usize = 8;
const MAX_PREFIX_LITERALS: usize = 3;
const WILDCARD_TOKEN: &str = "*";
pub(super) const WILDCARD_SUFFIX: &str = " *";
const SED: &str = "sed";

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

/// Command families a trusted native shell tool may run without a prompt.
///
/// An entry is honored only when every token is literal after quoting, so the
/// reviewed text is exactly what the shell runs. A configured ask or deny still
/// overrides the default.
pub(crate) const BUILTIN_ALLOW_PATTERNS: &[&str] = &["echo *"];

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

/// `matches` rejects operators, separators, newlines, and path-qualified
/// executables; decoding every token additionally rejects any expansion the
/// shell would perform after the command was reviewed.
pub(crate) fn builtin_allowed(command: &str) -> bool {
    builtin_allow_pattern(command).is_some()
}

/// The allowlist entry that admits this command, so a prompt can name the
/// authority rather than assert one.
pub(crate) fn builtin_allow_pattern(command: &str) -> Option<&'static str> {
    if !tokenize(command).is_some_and(|tokens| {
        tokens
            .iter()
            .all(|token| decode_static_token(token).is_some())
    }) {
        return None;
    }
    BUILTIN_ALLOW_PATTERNS
        .iter()
        .copied()
        .find(|pattern| matches(pattern, command))
}

/// A pattern keeps only leading literals, so an operand the shell would expand
/// disqualifies nothing: decoding stops at the first non-static token and the
/// prefix is derived from what came before it. `matches` is then the acceptance
/// boundary, exactly as it is at enforcement, so a suggestion is never offered
/// that the rule it becomes could not cover.
pub(crate) fn reusable_prefix(command: &str) -> Option<String> {
    let tokens = tokenize(command)?;
    let prefix: Vec<String> = tokens
        .iter()
        .map_while(|token| decode_static_token(token))
        .collect();
    if !is_pattern_literal(prefix.first()?) {
        return None;
    }

    let literals = match super::command_arity::curated_literals(&prefix) {
        Some((named, curated)) => curated_literals(&prefix, named, curated)?,
        None => heuristic_literals(&prefix, tokens.len())?,
    };
    if overlaps_builtin_ask(literals) || !sed_prefix_is_offerable(&prefix) {
        return None;
    }

    let pattern = format!("{}{WILDCARD_SUFFIX}", literals.join(" "));
    matches(&pattern, command).then_some(pattern)
}

/// A curated entry is a deliberate decision, so it may keep a bare executable or
/// the whole command where the heuristic may not. Past the tokens the entry
/// named, it still refuses to reach over a flag, because `git -C /repo commit`
/// would otherwise yield `git -C *`, and over a token that did not decode, which
/// is why it counts against the prefix rather than against the command.
fn curated_literals(prefix: &[String], named: usize, literals: usize) -> Option<&[String]> {
    (literals <= prefix.len()
        && prefix[named..literals]
            .iter()
            .all(|token| is_subcommand_word(token)))
    .then(|| &prefix[..literals])
}

/// `sed -n *` covers `sed -n '1w /etc/x'` too, because `-n` says nothing about
/// the script. Offering the rung only for a call whose own script only prints
/// keeps the suggestion and the read-only classifier on one definition.
fn sed_prefix_is_offerable(prefix: &[String]) -> bool {
    prefix.first().is_none_or(|executable| executable != SED)
        || super::sed_only_prints(
            &prefix[1..]
                .iter()
                .map(String::as_str)
                .collect::<Vec<&str>>(),
        )
}

/// Without curation the leading tokens are only guessed to be subcommands, so a
/// guess that degenerates to the bare executable or swallows every operand is
/// discarded rather than offered. Operands are counted from the command, not
/// from the decoded prefix, so a trailing glob still counts as one.
fn heuristic_literals(prefix: &[String], tokens: usize) -> Option<&[String]> {
    if prefix.get(1)?.starts_with('-') {
        return None;
    }
    let literals = prefix
        .iter()
        .skip(1)
        .take(MAX_PREFIX_LITERALS - 1)
        .take_while(|token| is_subcommand_word(token))
        .count()
        + 1;
    (literals > 1 && literals < tokens).then(|| &prefix[..literals])
}

/// A pattern that shares a prefix with an ask family in either direction covers
/// or is covered by it, and a stored rule silences the ask entirely.
fn overlaps_builtin_ask<S: AsRef<str>>(literals: &[S]) -> bool {
    BUILTIN_ASK_PATTERNS.iter().any(|pattern| {
        pattern
            .strip_suffix(WILDCARD_SUFFIX)
            .unwrap_or(pattern)
            .split_whitespace()
            .zip(literals)
            .all(|(ask, literal)| ask == literal.as_ref())
    })
}

/// Why a hand-written pattern cannot be granted, phrased for the person typing
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PatternFault {
    #[error("name at least one literal token before the `*`")]
    Empty,
    #[error("use at most {} tokens", MAX_PATTERN_TOKENS)]
    TooManyTokens,
    #[error("tokens may only contain letters, digits, and . _ / @ : = + -")]
    NonLiteralToken,
    #[error("end the pattern with ` *`")]
    MissingWildcard,
    #[error("the pattern must match the command on this row")]
    DoesNotMatch,
}

/// How far a hand-written pattern reaches, and what it costs to store it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatternGrade {
    pub caution: Option<PermissionCaution>,
    pub confirmation: Option<&'static str>,
}

/// Judges a pattern a user typed against the command it is being granted for.
///
/// `reusable_prefix` decides what Caudra *suggests* and stays conservative.
/// This decides what Caudra *accepts*, so it admits shapes the suggestion
/// refuses and grades them instead. Requiring the pattern to match the reviewed
/// command is the containment: a prompt about `rg` cannot be turned into a
/// grant for `sed`.
///
/// A single literal is graded gravest. `sed *` or `python *` is every
/// invocation of an interpreter, which is arbitrary execution wearing one name.
/// The exception is a pattern Caudra itself suggests for this command: `ls *` is
/// offered as a rung one keypress away, so typing it out cannot be graver than
/// picking it.
pub fn grade_command_pattern(pattern: &str, command: &str) -> Result<PatternGrade, PatternFault> {
    let mut literals: Vec<&str> = pattern.split_whitespace().collect();
    if literals.len() > MAX_PATTERN_TOKENS {
        return Err(PatternFault::TooManyTokens);
    }
    if literals.pop() != Some(WILDCARD_TOKEN) {
        return Err(PatternFault::MissingWildcard);
    }
    if literals.is_empty() {
        return Err(PatternFault::Empty);
    }
    if !literals.iter().all(|token| is_pattern_literal(token)) {
        return Err(PatternFault::NonLiteralToken);
    }
    if !matches(pattern, command) {
        return Err(PatternFault::DoesNotMatch);
    }
    let normalized = literals.join(" ") + WILDCARD_SUFFIX;
    let caution = if reusable_prefix(command).as_deref() == Some(normalized.as_str()) {
        None
    } else if literals.len() == 1 {
        Some(PermissionCaution::Danger)
    } else if overlaps_builtin_ask(&literals) {
        Some(PermissionCaution::Warn)
    } else {
        None
    };
    Ok(PatternGrade {
        caution,
        confirmation: (caution == Some(PermissionCaution::Danger)).then_some(BROAD_SHELL_PHRASE),
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

    use super::{
        BROAD_SHELL_PHRASE, BUILTIN_ALLOW_PATTERNS, BUILTIN_ASK_PATTERNS, PatternFault,
        PatternGrade, PermissionCaution, builtin_allowed, grade_command_pattern, matches,
        reusable_prefix, specificity, tokenize,
    };

    const SED_SLICE: &str = "sed -n 1,140p src/main.rs";

    #[test_case("sed -n *", SED_SLICE, None ; "flag_prefix_is_plain")]
    #[test_case("sed *", SED_SLICE, Some(PermissionCaution::Danger) ; "bare_executable_is_grave")]
    #[test_case("python *", "python script.py", Some(PermissionCaution::Danger) ; "unsuggested_bare_executable_is_grave")]
    #[test_case("git push *", "git push origin main", Some(PermissionCaution::Warn) ; "ask_family_only_warns")]
    #[test_case("rm *", "rm -rf build", Some(PermissionCaution::Danger) ; "bare_ask_family_is_grave")]
    #[test_case("ls *", "ls -la", None ; "typing_the_suggestion_grades_like_the_rung")]
    #[test_case("wc *", "wc -l a.rs b/*.rs", None ; "suggestion_over_a_globbed_operand")]
    #[test_case("ls   *", "ls -la", None ; "spacing_does_not_change_the_grade")]
    fn a_typed_pattern_is_graded_by_how_much_it_reaches(
        pattern: &str,
        command: &str,
        caution: Option<PermissionCaution>,
    ) {
        assert_eq!(
            grade_command_pattern(pattern, command),
            Ok(PatternGrade {
                caution,
                confirmation: (caution == Some(PermissionCaution::Danger))
                    .then_some(BROAD_SHELL_PHRASE),
            })
        );
    }

    #[test_case("rg *", SED_SLICE, PatternFault::DoesNotMatch ; "another_command")]
    #[test_case("sed -n", SED_SLICE, PatternFault::MissingWildcard ; "no_wildcard")]
    #[test_case("*", SED_SLICE, PatternFault::Empty ; "wildcard_alone")]
    #[test_case("", SED_SLICE, PatternFault::MissingWildcard ; "nothing_typed")]
    #[test_case("sed -n '1,140p' *", SED_SLICE, PatternFault::NonLiteralToken ; "quoted_token")]
    #[test_case("a b c d e f g h i *", SED_SLICE, PatternFault::TooManyTokens ; "past_the_token_cap")]
    fn a_typed_pattern_is_refused_with_the_reason_to_show(
        pattern: &str,
        command: &str,
        fault: PatternFault,
    ) {
        assert_eq!(grade_command_pattern(pattern, command), Err(fault));
    }

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
    #[test_case(r#"git status "$FORMAT""#, Some("git status *"); "parameter expansion")]
    #[test_case("git status $(format)", None; "command substitution")]
    #[test_case("git status *.rs", Some("git status *"); "glob expansion")]
    #[test_case(r#"git status "unterminated"#, None; "malformed input")]
    #[test_case("rg foo src/", Some("rg *"); "curated tool keeps its operand out")]
    #[test_case("rg -n foo", Some("rg *"); "curated tool reaches past a leading flag")]
    #[test_case("wc -l src/lib.rs", Some("wc *"))]
    #[test_case("ls -la", Some("ls *"))]
    #[test_case("ls src/ src/*/", Some("ls *"); "curated tool tolerates a globbed operand")]
    #[test_case("wc -l a.rs b/*.rs", Some("wc *"); "globbed operand past a flag")]
    #[test_case("npm run $(x)", None; "curated prefix stops at a substitution")]
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
    #[test_case("sed -n '1,140p' src/main.rs", Some("sed -n *"); "a curated entry may name a flag")]
    #[test_case(r#"sed -n '1,2p' "$F""#, Some("sed -n *"); "an operand does not decide the question")]
    #[test_case("sed -i 's/a/b/' f.rs", None; "a writing sed is offered nothing")]
    #[test_case("sed -n '1w /tmp/x' f.rs", None; "a printing flag over a writing script")]
    #[test_case(r#"sed -n "$SCRIPT" f.rs"#, None; "a script outside the command is offered nothing")]
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

    #[test]
    fn builtin_allow_patterns_are_exact() {
        assert_eq!(BUILTIN_ALLOW_PATTERNS, &["echo *"]);
    }

    #[test_case("echo", true; "bare command")]
    #[test_case("echo hi", true)]
    #[test_case("echo hello world", true)]
    #[test_case("echo -n hi", true; "leading flag")]
    #[test_case(r#"echo "plain text""#, true; "double quoted literal")]
    #[test_case("echo 'a $HOME b'", true; "single quotes suppress expansion")]
    #[test_case(r#"echo -e "a\tb""#, true; "escape sequence stays literal")]
    #[test_case("echo $HOME", false; "parameter expansion")]
    #[test_case(r#"echo "$HOME""#, false; "quoted parameter expansion")]
    #[test_case("echo ${HOME}", false; "braced parameter expansion")]
    #[test_case("echo $(id)", false; "command substitution")]
    #[test_case("echo `id`", false; "backtick substitution")]
    #[test_case("echo *", false; "glob expansion")]
    #[test_case("echo *.rs", false; "suffixed glob expansion")]
    #[test_case("echo ~", false; "tilde expansion")]
    #[test_case("echo {a,b}", false; "brace expansion")]
    #[test_case("echo hi > victim", false; "output redirect")]
    #[test_case("echo hi | sh", false; "pipeline")]
    #[test_case("echo hi; rm -rf /", false; "command separator")]
    #[test_case("echo hi && rm -rf /", false; "conditional separator")]
    #[test_case("echo hi\nrm -rf /", false; "newline separator")]
    #[test_case("echo 'unterminated", false; "malformed input")]
    #[test_case("/bin/echo hi", false; "path qualified executable")]
    #[test_case("ls", false; "uncovered command")]
    fn builtin_allows_only_fully_literal_commands(command: &str, expected: bool) {
        assert_eq!(builtin_allowed(command), expected);
    }
}
