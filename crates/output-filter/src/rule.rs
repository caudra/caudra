//! Declarative rule shape decoded from the vendored corpus.
//!
//! Every field is rejected unless it is declared here. A corpus refresh that
//! introduces an unrecognized field therefore fails loudly at load time instead
//! of silently dropping a transformation the rule author expected to run.

use serde::Deserialize;

/// One short-circuit rule: when `pattern` matches the accumulated output, the
/// whole result collapses to `message` unless `unless` also matches.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MatchOutputRule {
    pub(crate) pattern: String,
    pub(crate) message: String,
    /// Guard that prevents a success message from swallowing a failure.
    #[serde(default)]
    pub(crate) unless: Option<String>,
}

/// One regex substitution applied line by line.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplaceRule {
    pub(crate) pattern: String,
    pub(crate) replacement: String,
}

/// A single command-output filter as authored in the corpus.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuleDocument {
    #[allow(
        dead_code,
        reason = "corpus documentation, retained so decoding stays strict"
    )]
    pub(crate) description: String,
    pub(crate) match_command: String,
    #[serde(default)]
    pub(crate) strip_ansi: bool,
    /// Merge stderr into stdout before filtering, for tools that emit banners
    /// on stderr.
    #[serde(default)]
    pub(crate) filter_stderr: bool,
    #[serde(default)]
    pub(crate) replace: Vec<ReplaceRule>,
    #[serde(default)]
    pub(crate) match_output: Vec<MatchOutputRule>,
    #[serde(default)]
    pub(crate) strip_lines_matching: Vec<String>,
    #[serde(default)]
    pub(crate) keep_lines_matching: Vec<String>,
    #[serde(default)]
    pub(crate) truncate_lines_at: Option<usize>,
    #[serde(default)]
    pub(crate) head_lines: Option<usize>,
    #[serde(default)]
    pub(crate) tail_lines: Option<usize>,
    #[serde(default)]
    pub(crate) max_lines: Option<usize>,
    #[serde(default)]
    pub(crate) on_empty: Option<String>,
}

/// Inline expectation shipped beside each rule in the corpus.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleTest {
    pub name: String,
    pub input: String,
    pub expected: String,
}

/// The concatenated corpus document.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CorpusDocument {
    pub(crate) filters: std::collections::BTreeMap<String, RuleDocument>,
    #[serde(default)]
    pub(crate) tests: std::collections::BTreeMap<String, Vec<RuleTest>>,
}
