//! Compilation of the vendored corpus into bounded, ready-to-run rules.
//!
//! Compilation happens once for the process. Bounds are checked here rather than
//! at apply time so a malformed corpus cannot reach a live tool call, and so the
//! per-call path performs no allocation-heavy validation.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use regex::Regex;

use crate::rule::{CorpusDocument, RuleTest};

/// The build script concatenates `rules/*.toml` in file-name order.
const RULES: &str = include_str!(concat!(env!("OUT_DIR"), "/rules.toml"));

/// Bounds mirror the shell permission policy so both operator-facing and
/// vendored inputs are constrained the same way.
const MAX_RULES: usize = 256;
const MAX_PATTERNS_PER_RULE: usize = 64;
const MAX_PATTERN_BYTES: usize = 512;
/// Compiled program size ceiling per pattern. `regex` is linear-time, so this
/// bounds memory rather than defending against backtracking.
const MAX_COMPILED_PATTERN_BYTES: usize = 64 * 1024;

/// A short-circuit rule with its patterns already compiled.
#[derive(Debug)]
pub(crate) struct CompiledMatchOutput {
    pub(crate) pattern: Regex,
    pub(crate) message: String,
    pub(crate) unless: Option<Regex>,
}

/// A substitution with its pattern already compiled.
#[derive(Debug)]
pub(crate) struct CompiledReplace {
    pub(crate) pattern: Regex,
    pub(crate) replacement: String,
}

/// A single rule, validated and ready to apply.
#[derive(Debug)]
pub struct Rule {
    pub(crate) name: String,
    pub(crate) match_command: Regex,
    pub(crate) strip_ansi: bool,
    pub(crate) filter_stderr: bool,
    pub(crate) replace: Vec<CompiledReplace>,
    pub(crate) match_output: Vec<CompiledMatchOutput>,
    pub(crate) strip_lines_matching: Vec<Regex>,
    pub(crate) keep_lines_matching: Vec<Regex>,
    pub(crate) truncate_lines_at: Option<usize>,
    pub(crate) head_lines: Option<usize>,
    pub(crate) tail_lines: Option<usize>,
    pub(crate) max_lines: Option<usize>,
    pub(crate) on_empty: Option<String>,
}

impl Rule {
    /// Corpus rule name, safe to disclose because the corpus is built in.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// The compiled built-in rule set.
#[derive(Debug)]
pub struct Corpus {
    rules: Vec<Rule>,
    tests: BTreeMap<String, Vec<RuleTest>>,
}

impl Corpus {
    /// Returns the first rule whose `match_command` accepts `normalized`.
    ///
    /// `normalized` must be a single normalized command scope, not a raw
    /// request string. Matching a normalized scope means an absolute path,
    /// quote splicing, or a leading assignment cannot evade a rule, and equally
    /// cannot cause one to be applied to a command it was not written for.
    #[must_use]
    pub fn find(&self, normalized: &str) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|rule| rule.match_command.is_match(normalized))
    }

    /// Number of compiled rules. Disclosed for diagnostics; the corpus is built
    /// in, so this reveals nothing about the deployment.
    #[must_use]
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Inline expectations shipped with the corpus, keyed by rule name.
    #[must_use]
    pub fn tests(&self) -> &BTreeMap<String, Vec<RuleTest>> {
        &self.tests
    }

    /// Looks a rule up by corpus name.
    #[must_use]
    pub fn rule(&self, name: &str) -> Option<&Rule> {
        self.rules.iter().find(|rule| rule.name == name)
    }
}

/// Returns the process-wide compiled corpus, compiling it on first use.
///
/// # Panics
///
/// Panics when the embedded corpus is invalid. The corpus is a build artifact
/// validated by `build.rs` and by the corpus test suite, so a failure here is a
/// build defect rather than a runtime condition.
#[must_use]
pub fn builtin() -> &'static Corpus {
    static CORPUS: OnceLock<Corpus> = OnceLock::new();
    CORPUS.get_or_init(|| compile(RULES).expect("embedded rule corpus is valid"))
}

fn compile(document: &str) -> Result<Corpus, String> {
    let parsed: CorpusDocument =
        toml::from_str(document).map_err(|error| format!("corpus is not valid TOML: {error}"))?;
    if parsed.filters.len() > MAX_RULES {
        return Err(format!(
            "corpus defines {} rules, exceeding the maximum of {MAX_RULES}",
            parsed.filters.len()
        ));
    }

    let mut rules = Vec::with_capacity(parsed.filters.len());
    for (name, document) in parsed.filters {
        let patterns = document.replace.len()
            + document.match_output.len() * 2
            + document.strip_lines_matching.len()
            + document.keep_lines_matching.len()
            + 1;
        if patterns > MAX_PATTERNS_PER_RULE {
            return Err(format!(
                "rule `{name}` declares {patterns} patterns, exceeding the maximum of {MAX_PATTERNS_PER_RULE}"
            ));
        }
        // A rule that both strips and keeps has no defined precedence; rejecting
        // it is better than silently choosing one.
        if !document.strip_lines_matching.is_empty() && !document.keep_lines_matching.is_empty() {
            return Err(format!(
                "rule `{name}` sets both strip_lines_matching and keep_lines_matching"
            ));
        }

        let compile_one = |pattern: &str| compile_pattern(&name, pattern);
        let mut match_output = Vec::with_capacity(document.match_output.len());
        for entry in &document.match_output {
            match_output.push(CompiledMatchOutput {
                pattern: compile_one(&entry.pattern)?,
                message: entry.message.clone(),
                unless: entry.unless.as_deref().map(&compile_one).transpose()?,
            });
        }
        let mut replace = Vec::with_capacity(document.replace.len());
        for entry in &document.replace {
            replace.push(CompiledReplace {
                pattern: compile_one(&entry.pattern)?,
                replacement: entry.replacement.clone(),
            });
        }

        rules.push(Rule {
            match_command: compile_one(&document.match_command)?,
            strip_ansi: document.strip_ansi,
            filter_stderr: document.filter_stderr,
            replace,
            match_output,
            strip_lines_matching: document
                .strip_lines_matching
                .iter()
                .map(|pattern| compile_one(pattern))
                .collect::<Result<_, _>>()?,
            keep_lines_matching: document
                .keep_lines_matching
                .iter()
                .map(|pattern| compile_one(pattern))
                .collect::<Result<_, _>>()?,
            truncate_lines_at: document.truncate_lines_at,
            head_lines: document.head_lines,
            tail_lines: document.tail_lines,
            max_lines: document.max_lines,
            on_empty: document.on_empty,
            name,
        });
    }

    Ok(Corpus {
        rules,
        tests: parsed.tests,
    })
}

fn compile_pattern(rule: &str, pattern: &str) -> Result<Regex, String> {
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err(format!(
            "rule `{rule}` has a {}-byte pattern, exceeding the maximum of {MAX_PATTERN_BYTES}",
            pattern.len()
        ));
    }
    regex::RegexBuilder::new(pattern)
        .size_limit(MAX_COMPILED_PATTERN_BYTES)
        .build()
        .map_err(|error| format!("rule `{rule}` has an invalid pattern: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_corpus_compiles_within_bounds() {
        let corpus = builtin();
        assert!(corpus.rule_count() > 0);
        assert!(corpus.rule_count() <= MAX_RULES);
    }

    #[test]
    fn every_rule_ships_inline_expectations() {
        let corpus = builtin();
        for rule in &corpus.rules {
            assert!(
                corpus.tests.contains_key(&rule.name),
                "rule `{}` has no inline expectations",
                rule.name
            );
        }
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let error = compile(
            "[filters.demo]\ndescription = \"d\"\nmatch_command = \"^demo\"\nfuture_field = 1\n",
        )
        .expect_err("unknown field must fail");
        assert!(error.contains("future_field"), "{error}");
    }

    #[test]
    fn conflicting_line_selectors_are_rejected() {
        let error = compile(
            "[filters.demo]\ndescription = \"d\"\nmatch_command = \"^demo\"\nstrip_lines_matching = [\"a\"]\nkeep_lines_matching = [\"b\"]\n",
        )
        .expect_err("conflicting selectors must fail");
        assert!(error.contains("both"), "{error}");
    }

    #[test]
    fn oversized_patterns_are_rejected() {
        let pattern = "a".repeat(MAX_PATTERN_BYTES + 1);
        let error = compile(&format!(
            "[filters.demo]\ndescription = \"d\"\nmatch_command = \"{pattern}\"\n"
        ))
        .expect_err("oversized pattern must fail");
        assert!(error.contains("exceeding the maximum"), "{error}");
    }
}
