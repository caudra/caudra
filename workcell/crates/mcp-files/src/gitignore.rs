//! Bounded `.gitignore` evaluation for broad traversal.
//!
//! Only per-directory `.gitignore` files inside the configured root are read. `$GIT_DIR/info/exclude`
//! is unreachable because every path carrying a `.git` component is protected by
//! [`crate::path_policy`], and `core.excludesFile` lives outside the root. Neither is worked around:
//! a traversal filter is not a reason to widen what the process can open.
//!
//! Every axis that can grow has a ceiling, and a bound that bites is reported rather than absorbed.
//! A partially collected rule set applied as though it were complete is the failure worth refusing,
//! because it silently hides files from a search that claims to have looked.

use std::{path::Path, sync::Arc};

use tokio::io::AsyncReadExt;

use crate::{
    FilesystemLimits,
    glob::{CharClass, Token, matches_tokens},
};

/// One pattern line, compiled.
#[derive(Debug)]
struct IgnorePattern {
    tokens: Vec<Token>,
    classes: Vec<CharClass>,
    /// A leading `!`. The last matching pattern decides, so this can re-include.
    negated: bool,
    /// A trailing `/`. Matches a directory and never a file of the same name.
    directory_only: bool,
}

impl IgnorePattern {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.tokens.capacity().saturating_mul(size_of::<Token>()))
            .saturating_add(
                self.classes
                    .iter()
                    .map(CharClass::retained_bytes)
                    .fold(0, usize::saturating_add),
            )
    }
}

/// Every pattern from one `.gitignore`, with the directory that owns them.
#[derive(Debug)]
pub(crate) struct IgnoreFile {
    /// The owning directory relative to the seed base, `/`-separated and without a trailing
    /// separator. Empty when the file sits at the base itself.
    prefix: String,
    patterns: Vec<IgnorePattern>,
}

/// A `.gitignore` and every one above it.
///
/// Git resolves deeper files first and, within one file, the last matching line. The chain is built
/// child-to-parent so walking it from a candidate's own directory upwards visits them in exactly
/// that order.
#[derive(Debug)]
pub(crate) struct IgnoreScope {
    file: Arc<IgnoreFile>,
    parent: Option<Arc<IgnoreScope>>,
}

impl IgnoreScope {
    fn nested(file: Arc<IgnoreFile>, parent: Option<Arc<Self>>) -> Arc<Self> {
        Arc::new(Self { file, parent })
    }

    /// Decides one candidate, or reports that no rule named it.
    ///
    /// `relative` is `/`-separated and relative to the seed base. Returns `Some(true)` when the
    /// candidate is ignored, `Some(false)` when a negation re-included it, and `None` when the
    /// budget ran out or nothing matched.
    pub(crate) fn decide(
        &self,
        relative: &str,
        is_directory: bool,
        budget: &mut IgnoreBudget,
        scratch: &mut IgnoreScratch,
    ) -> Option<bool> {
        let mut scope = Some(self);
        while let Some(current) = scope {
            let file = &current.file;
            if let Some(sub) = strip_prefix(relative, &file.prefix) {
                scratch.value.clear();
                scratch.value.extend(sub.encode_utf16());
                // Reverse order: within one file the last matching line wins, and stopping at the
                // first hit from the end is the same decision for a fraction of the work.
                for pattern in file.patterns.iter().rev() {
                    if pattern.directory_only && !is_directory {
                        continue;
                    }
                    let steps = pattern
                        .tokens
                        .len()
                        .saturating_mul(scratch.value.len().saturating_add(1));
                    if steps > budget.match_steps {
                        budget.complete = false;
                        return None;
                    }
                    budget.match_steps -= steps;
                    if matches_tokens(
                        &pattern.tokens,
                        &pattern.classes,
                        &scratch.value,
                        &mut scratch.current,
                        &mut scratch.next,
                    ) {
                        return Some(!pattern.negated);
                    }
                }
            }
            scope = current.parent.as_deref();
        }
        None
    }
}

/// Reusable match buffers, threaded through one traversal.
#[derive(Debug, Default)]
pub(crate) struct IgnoreScratch {
    value: Vec<u16>,
    current: Vec<bool>,
    next: Vec<bool>,
}

/// What one traversal may still spend on ignore rules.
#[derive(Debug)]
pub(crate) struct IgnoreBudget {
    files: usize,
    retained_bytes: usize,
    match_steps: usize,
    /// False once any bound stopped rule collection or evaluation.
    pub(crate) complete: bool,
}

impl IgnoreBudget {
    pub(crate) fn new(limits: &FilesystemLimits) -> Self {
        Self {
            files: limits.max_gitignore_files,
            retained_bytes: limits.max_gitignore_retained_bytes,
            match_steps: limits.max_gitignore_match_steps,
            complete: true,
        }
    }
}

/// Reads and compiles `<directory>/.gitignore`, extending `parent`.
///
/// Returns `parent` unchanged when there is no readable ignore file, so a caller can chain
/// unconditionally. A file that exists but cannot be admitted marks the budget incomplete; it is
/// never partially applied.
pub(crate) async fn extend_scope(
    directory: &Path,
    prefix: &str,
    parent: Option<Arc<IgnoreScope>>,
    limits: &FilesystemLimits,
    budget: &mut IgnoreBudget,
) -> Option<Arc<IgnoreScope>> {
    let Some(contents) = read_bounded(&directory.join(".gitignore"), limits, budget).await else {
        return parent;
    };
    admit_scope(&contents, prefix, parent, limits, budget)
}

/// Compiles the bytes of one ignore file, read with a bound one byte past the ceiling, onto
/// `parent`. A file over the ceiling, beyond the file budget or not UTF-8 marks the budget
/// incomplete and leaves `parent` unchanged.
pub(crate) fn admit_scope(
    contents: &[u8],
    prefix: &str,
    parent: Option<Arc<IgnoreScope>>,
    limits: &FilesystemLimits,
    budget: &mut IgnoreBudget,
) -> Option<Arc<IgnoreScope>> {
    if budget.files == 0 || contents.len() > limits.max_gitignore_bytes {
        budget.complete = false;
        return parent;
    }
    budget.files -= 1;
    let Ok(contents) = std::str::from_utf8(contents) else {
        budget.complete = false;
        return parent;
    };
    compile_scope(contents, prefix, parent, limits, budget)
}

pub(crate) fn compile_scope(
    contents: &str,
    prefix: &str,
    parent: Option<Arc<IgnoreScope>>,
    limits: &FilesystemLimits,
    budget: &mut IgnoreBudget,
) -> Option<Arc<IgnoreScope>> {
    let (patterns, complete) = parse_patterns(contents, limits);
    if !complete {
        budget.complete = false;
    }
    if patterns.is_empty() {
        return parent;
    }
    let retained = patterns
        .iter()
        .map(IgnorePattern::retained_bytes)
        .fold(prefix.len(), usize::saturating_add);
    if retained > budget.retained_bytes {
        budget.complete = false;
        return parent;
    }
    budget.retained_bytes -= retained;
    Some(IgnoreScope::nested(
        Arc::new(IgnoreFile {
            prefix: prefix.to_owned(),
            patterns,
        }),
        parent,
    ))
}

/// Reads an ignore file without bringing an unbounded one into memory.
///
/// The read asks for one byte more than the ceiling, so a file at exactly the ceiling is admitted
/// and a larger one is detected without a separate `stat` that could race the read.
async fn read_bounded(
    path: &Path,
    limits: &FilesystemLimits,
    budget: &mut IgnoreBudget,
) -> Option<Vec<u8>> {
    let file = tokio::fs::File::open(path).await.ok()?;
    let mut contents = Vec::new();
    let ceiling = u64::try_from(limits.max_gitignore_bytes).unwrap_or(u64::MAX);
    if file
        .take(ceiling.saturating_add(1))
        .read_to_end(&mut contents)
        .await
        .is_err()
    {
        budget.complete = false;
        return None;
    }
    Some(contents)
}

/// Compiles every line, reporting whether the whole file was admitted.
fn parse_patterns(contents: &str, limits: &FilesystemLimits) -> (Vec<IgnorePattern>, bool) {
    let mut patterns = Vec::new();
    for line in contents.lines() {
        if patterns.len() >= limits.max_gitignore_patterns {
            return (patterns, false);
        }
        // An unparseable line is dropped rather than approximated. A pattern that half-compiles
        // matches something its author never wrote, and over-matching hides files.
        if let Some(pattern) = parse_pattern(line) {
            patterns.push(pattern);
        }
    }
    (patterns, true)
}

fn parse_pattern(line: &str) -> Option<IgnorePattern> {
    let line = trim_trailing_unescaped_whitespace(line);
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (negated, body) = match line.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    let (directory_only, body) = match body.strip_suffix('/') {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    if body.is_empty() {
        return None;
    }
    // A separator anywhere but the stripped trailing position anchors the pattern to the directory
    // owning the ignore file. Without one it matches a name at any depth below that directory.
    let anchored = body.contains('/');
    let body = body.strip_prefix('/').unwrap_or(body);
    if body.is_empty() {
        return None;
    }
    let (mut tokens, classes) = tokenize(body)?;
    if !anchored {
        tokens.insert(0, Token::RecursiveDirectories);
    }
    Some(IgnorePattern {
        tokens,
        classes,
        negated,
        directory_only,
    })
}

/// Drops trailing spaces and tabs that no backslash protects.
fn trim_trailing_unescaped_whitespace(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut end = bytes.len();
    while end > 0 && matches!(bytes[end - 1], b' ' | b'\t') {
        // A run of backslashes immediately before the space protects it only when that run is odd,
        // because each pair is itself an escaped backslash.
        let mut backslashes = 0usize;
        while backslashes < end - 1 && bytes[end - 2 - backslashes] == b'\\' {
            backslashes += 1;
        }
        if backslashes % 2 == 1 {
            break;
        }
        end -= 1;
    }
    &line[..end]
}

/// The gitignore dialect: glob metacharacters, bracket expressions, and backslash escapes.
///
/// Returns `None` for anything it cannot represent exactly, which drops the pattern.
fn tokenize(pattern: &str) -> Option<(Vec<Token>, Vec<CharClass>)> {
    let characters = pattern.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut classes: Vec<CharClass> = Vec::new();
    let mut index = 0usize;
    while index < characters.len() {
        match characters[index] {
            '\\' => {
                let escaped = *characters.get(index + 1)?;
                push_literal(&mut tokens, escaped);
                index += 2;
            }
            '*' if characters.get(index + 1) == Some(&'*')
                && characters.get(index + 2) == Some(&'/') =>
            {
                tokens.push(Token::RecursiveDirectories);
                index += 3;
            }
            '*' if characters.get(index + 1) == Some(&'*') => {
                tokens.push(Token::Recursive);
                index += 2;
            }
            '*' => {
                tokens.push(Token::SegmentStar);
                index += 1;
            }
            '?' => {
                tokens.push(Token::Any);
                index += 1;
            }
            '[' => {
                let (class, next) = parse_class(&characters, index)?;
                let slot = u16::try_from(classes.len()).ok()?;
                classes.push(class);
                tokens.push(Token::Class(slot));
                index = next;
            }
            character => {
                push_literal(&mut tokens, character);
                index += 1;
            }
        }
    }
    (!tokens.is_empty()).then_some((tokens, classes))
}

fn push_literal(tokens: &mut Vec<Token>, character: char) {
    let mut units = [0; 2];
    tokens.extend(
        character
            .encode_utf16(&mut units)
            .iter()
            .copied()
            .map(Token::Literal),
    );
}

/// Parses `[...]` starting at `open`, returning the class and the index after `]`.
///
/// POSIX named classes such as `[[:alpha:]]` are refused rather than approximated: treating the
/// name as a set of literal characters would match paths the author never described.
fn parse_class(characters: &[char], open: usize) -> Option<(CharClass, usize)> {
    let mut index = open + 1;
    let mut class = CharClass::default();
    if characters
        .get(index)
        .is_some_and(|c| *c == '!' || *c == '^')
    {
        class.negated = true;
        index += 1;
    }
    // A `]` in the first position is a member, not the terminator.
    let mut first = true;
    while index < characters.len() {
        let character = characters[index];
        if character == ']' && !first {
            return (!class.ranges.is_empty()).then_some((class, index + 1));
        }
        first = false;
        if character == '[' && characters.get(index + 1) == Some(&':') {
            return None;
        }
        let low = bmp_unit(character)?;
        index += 1;
        // `a-z`, but a `-` immediately before the terminator is a literal member.
        if characters.get(index) == Some(&'-')
            && characters.get(index + 1).is_some_and(|c| *c != ']')
        {
            let high = bmp_unit(characters[index + 1])?;
            if high < low {
                return None;
            }
            class.ranges.push((low, high));
            index += 2;
        } else {
            class.ranges.push((low, low));
        }
    }
    None
}

/// Rejects anything outside the basic multilingual plane.
///
/// Ranges compare UTF-16 code units, and a supplementary character is a surrogate pair whose halves
/// order differently from the character itself.
fn bmp_unit(character: char) -> Option<u16> {
    u16::try_from(u32::from(character)).ok()
}

/// Re-bases `relative` onto the directory owning an ignore file.
fn strip_prefix<'a>(relative: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        return Some(relative);
    }
    relative
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(lines: &[&str]) -> Arc<IgnoreScope> {
        compile_at("", lines, None)
    }

    fn compile_at(
        prefix: &str,
        lines: &[&str],
        parent: Option<Arc<IgnoreScope>>,
    ) -> Arc<IgnoreScope> {
        let (patterns, complete) = parse_patterns(&lines.join("\n"), &FilesystemLimits::default());
        assert!(complete, "fixture patterns must compile");
        IgnoreScope::nested(
            Arc::new(IgnoreFile {
                prefix: prefix.to_owned(),
                patterns,
            }),
            parent,
        )
    }

    fn decide(scope: &IgnoreScope, relative: &str, is_directory: bool) -> Option<bool> {
        let mut budget = IgnoreBudget::new(&FilesystemLimits::default());
        let mut scratch = IgnoreScratch::default();
        scope.decide(relative, is_directory, &mut budget, &mut scratch)
    }

    #[test]
    fn a_pattern_without_a_slash_matches_at_every_depth() {
        let scope = compile(&["build"]);
        assert_eq!(decide(&scope, "build", false), Some(true));
        assert_eq!(decide(&scope, "a/b/build", false), Some(true));
    }

    #[test]
    fn a_pattern_with_an_interior_slash_is_anchored_to_its_ignore_file() {
        let scope = compile(&["src/build"]);
        assert_eq!(decide(&scope, "src/build", false), Some(true));
        assert_eq!(decide(&scope, "a/src/build", false), None);
    }

    #[test]
    fn a_leading_slash_anchors_without_requiring_an_interior_one() {
        let scope = compile(&["/build"]);
        assert_eq!(decide(&scope, "build", false), Some(true));
        assert_eq!(decide(&scope, "a/build", false), None);
    }

    #[test]
    fn a_trailing_slash_matches_only_a_directory() {
        let scope = compile(&["build/"]);
        assert_eq!(decide(&scope, "build", true), Some(true));
    }

    #[test]
    fn a_file_sharing_a_name_with_a_directory_only_pattern_is_not_ignored() {
        let scope = compile(&["build/"]);
        assert_eq!(decide(&scope, "build", false), None);
    }

    #[test]
    fn a_later_negation_re_includes_a_file() {
        let scope = compile(&["*.log", "!keep.log"]);
        assert_eq!(decide(&scope, "drop.log", false), Some(true));
        assert_eq!(decide(&scope, "keep.log", false), Some(false));
    }

    #[test]
    fn an_earlier_negation_loses_to_a_later_exclusion() {
        let scope = compile(&["!keep.log", "*.log"]);
        assert_eq!(decide(&scope, "keep.log", false), Some(true));
    }

    #[test]
    fn a_deeper_ignore_file_overrides_a_shallower_one() {
        let root = compile(&["*.log"]);
        let nested = compile_at("logs", &["!*.log"], Some(root));
        assert_eq!(decide(&nested, "logs/keep.log", false), Some(false));
        assert_eq!(decide(&nested, "other/drop.log", false), Some(true));
    }

    #[test]
    fn a_single_star_does_not_cross_a_separator() {
        let scope = compile(&["src/*.rs"]);
        assert_eq!(decide(&scope, "src/a.rs", false), Some(true));
        assert_eq!(decide(&scope, "src/inner/a.rs", false), None);
    }

    #[test]
    fn a_double_star_crosses_separators() {
        let scope = compile(&["src/**/a.rs"]);
        assert_eq!(decide(&scope, "src/inner/deep/a.rs", false), Some(true));
    }

    #[test]
    fn a_bracket_expression_matches_its_members() {
        let scope = compile(&["*.[oa]"]);
        assert_eq!(decide(&scope, "x.o", false), Some(true));
        assert_eq!(decide(&scope, "x.a", false), Some(true));
        assert_eq!(decide(&scope, "x.b", false), None);
    }

    #[test]
    fn a_negated_bracket_expression_excludes_its_members() {
        let scope = compile(&["x.[!oa]"]);
        assert_eq!(decide(&scope, "x.b", false), Some(true));
        assert_eq!(decide(&scope, "x.o", false), None);
    }

    #[test]
    fn a_bracket_range_matches_its_interval() {
        let scope = compile(&["log[0-9]"]);
        assert_eq!(decide(&scope, "log4", false), Some(true));
        assert_eq!(decide(&scope, "logx", false), None);
    }

    #[test]
    fn a_bracket_expression_never_matches_a_separator() {
        let scope = compile(&["a[!z]b"]);
        assert_eq!(decide(&scope, "a/b", false), None);
    }

    #[test]
    fn an_escaped_metacharacter_is_a_literal() {
        let scope = compile(&["a\\*b"]);
        assert_eq!(decide(&scope, "a*b", false), Some(true));
        assert_eq!(decide(&scope, "axb", false), None);
    }

    #[test]
    fn a_comment_is_not_a_pattern_but_an_escaped_hash_is() {
        let scope = compile(&["#notes", "\\#real"]);
        assert_eq!(decide(&scope, "notes", false), None);
        assert_eq!(decide(&scope, "#real", false), Some(true));
    }

    #[test]
    fn trailing_whitespace_is_dropped_unless_it_is_escaped() {
        let scope = compile(&["plain   "]);
        assert_eq!(decide(&scope, "plain", false), Some(true));
        let escaped = compile(&["spaced\\ "]);
        assert_eq!(decide(&escaped, "spaced ", false), Some(true));
        assert_eq!(decide(&escaped, "spaced", false), None);
    }

    #[test]
    fn an_unparseable_pattern_is_dropped_rather_than_matched_broadly() {
        for line in ["[unterminated", "[[:alpha:]]", "[z-a]", "trailing\\"] {
            assert!(
                parse_pattern(line).is_none(),
                "{line} must not compile into a pattern"
            );
        }
    }

    #[test]
    fn exceeding_the_pattern_ceiling_reports_an_incomplete_file() {
        let limits = FilesystemLimits {
            max_gitignore_patterns: 2,
            ..FilesystemLimits::default()
        };
        let (patterns, complete) = parse_patterns("a\nb\nc\n", &limits);
        assert_eq!(patterns.len(), 2);
        assert!(!complete);
    }

    #[test]
    fn exhausting_the_match_budget_declines_to_decide() {
        let scope = compile(&["*.log"]);
        let mut budget = IgnoreBudget::new(&FilesystemLimits::default());
        budget.match_steps = 0;
        let mut scratch = IgnoreScratch::default();
        assert_eq!(
            scope.decide("drop.log", false, &mut budget, &mut scratch),
            None
        );
        assert!(!budget.complete);
    }
}
