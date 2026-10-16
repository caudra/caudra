//! Tool inputs and outputs.
//!
//! # The honesty vocabulary is structural, not prose
//!
//! Every count this group reports is a floor. Call edges are recovered from source text by name, so
//! dynamic dispatch, callbacks, function pointers, trait objects, reflection, and macro-generated
//! call sites contribute no edge at all. `countsFloor` is therefore `true` on every result that
//! carries a count, and it is a field rather than a sentence in a description because a caller that
//! renders the number without the caveat would otherwise have no way to know.
//!
//! Zero means "none found". It never means "none exists".

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The task-shaped bundle.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodeContextInput {
    /// What the caller is about to do, in their own words.
    pub task: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Which way to walk the call graph.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum Direction {
    /// Symbols that reference the selected one.
    #[default]
    Callers,
    /// Symbols the selected one references.
    Callees,
}

impl Direction {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Callers => "callers",
            Self::Callees => "callees",
        }
    }

    /// What one row means, named for this direction.
    ///
    /// The two directions do not measure the same thing and must not share a noun. A caller count
    /// is evidence about the rest of the repository; a callee count is a property of one body.
    #[must_use]
    pub const fn unit(self) -> &'static str {
        match self {
            Self::Callers => "referencing symbol",
            Self::Callees => "referenced symbol",
        }
    }
}

/// Callers or callees of one symbol, and with `depth` what a change to it could reach.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodeRefsInput {
    /// A symbol name, optionally qualified by path as `path::name` or `path:name`.
    pub symbol: String,
    #[serde(default)]
    pub direction: Direction,
    /// Hops to walk backwards along call edges. Callers only; absent means 1. Bounded by the host.
    #[serde(default)]
    pub depth: Option<usize>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// What the graph run itself had to say.
///
/// Present on every result. A ranking produced from a truncated crawl and one produced from a
/// complete crawl are different claims, and nothing downstream can tell them apart without this.
#[derive(Clone, Debug, Default, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphSummary {
    pub files_indexed: usize,
    pub files_skipped: usize,
    pub symbols: usize,
    /// References that resolved to a definition.
    pub resolved_references: usize,
    /// References naming something the map never defined. Usually a call into a dependency.
    pub unresolved_references: usize,
    /// References matching more than one definition, whose weight was split rather than guessed.
    pub ambiguous_references: usize,
    /// Calls whose receiver or qualifier points outside the tree, so they were never linked to the
    /// in-tree symbol that happens to share their name.
    #[serde(skip_serializing_if = "is_zero")]
    pub declined_references: usize,
    pub edges: usize,
    /// Power iterations performed.
    pub pr_iterations: usize,
    /// Whether the iteration reached its residual tolerance.
    ///
    /// A truncated run produces a document that looks identical to a converged one and does not
    /// mean the same thing: it is a rank vector caught mid-descent.
    pub pr_converged: bool,
    /// Bounds that stopped the crawl, extraction, or ranking, by name. Empty means none did.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub truncated_by: Vec<String>,
    /// Whether the traversal examined every candidate it produced.
    pub scan_complete: bool,
    /// Files the tree's own `.gitignore` rules kept out of the map.
    #[serde(skip_serializing_if = "is_zero")]
    pub files_ignored: usize,
    /// Directories not mapped because they are separate repositories.
    ///
    /// A vendored repository's symbols would compete with this project's in one rank vector, so the
    /// map stops at the boundary. Naming one as `path` maps it on its own terms.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pruned_repositories: Vec<String>,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// One ranked symbol.
#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RankedSymbol {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line_start: usize,
    pub line_end: usize,
    /// Personalized PageRank score. Comparable within one result, meaningless across two.
    pub rank: f64,
    /// Distinct symbols referencing this one. A floor.
    pub callers: usize,
    /// Distinct symbols this one references. A floor.
    pub calls: usize,
    /// Whether the definition sits in test scope.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub test_scope: bool,
}

/// A symbol named without a rank, used where ordering comes from something else.
#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolRef {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line_start: usize,
    pub line_end: usize,
}

/// One line of source where a reference was made.
#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SiteRef {
    pub path: String,
    pub line: usize,
}

impl SiteRef {
    /// The site as a reader with `home` in view needs it: a bare line in that file, else `path:line`.
    #[must_use]
    pub fn label(&self, home: &str) -> String {
        site_label(&self.path, self.line, home)
    }
}

/// [`SiteRef::label`] for a site that arrived as separate fields.
#[must_use]
pub fn site_label(path: &str, line: usize, home: &str) -> String {
    if path == home {
        line.to_string()
    } else {
        format!("{path}:{line}")
    }
}

/// One symbol reached from the selected one, with where the calls are.
#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceRow {
    #[serde(flatten)]
    pub symbol: RankedSymbol,
    /// Edges walked from the selected symbol. One means a direct caller or callee.
    pub hops: usize,
    /// The call sites between this row and the selected symbol, in source order. Direct rows only.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sites: Vec<SiteRef>,
    /// Every call site, when `sites` was capped.
    #[serde(skip_serializing_if = "is_zero")]
    pub sites_total: usize,
    /// Linked by name alone: the receiver's type was never seen, so the call may mean another
    /// symbol with the same name. Such a link never moves a ranking.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub name_only: bool,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeContextOutput {
    pub task: String,
    pub path: String,
    /// How the router read the task: `nameExact` or `conceptual`.
    pub shape: String,
    /// Why it read it that way, in one sentence.
    pub shape_reason: String,
    /// `high`, `medium`, or `low`, derived from score separation. Never a claim of correctness.
    pub confidence: String,
    /// How far the top score sits above the head's median, as a percentage.
    pub margin_percent: u32,
    pub results: Vec<RankedSymbol>,
    pub shown: usize,
    /// Symbols with any lexical match. Not every one is returned.
    pub total_matched: usize,
    pub truncated: bool,
    pub counts_floor: bool,
    pub estimated_tokens: usize,
    pub graph: GraphSummary,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeRefsOutput {
    pub symbol: String,
    pub direction: &'static str,
    /// What one row counts, named for the direction.
    pub unit: &'static str,
    /// Hops walked. One is the direct neighbourhood.
    pub depth: usize,
    /// Definitions the selector matched. More than one means the answer is a union.
    pub matched: Vec<SymbolRef>,
    /// Nearest first; within one hop, callers by rank and callees in source order.
    pub references: Vec<ReferenceRow>,
    /// Rows found at each hop, from hop one. Present when more than one hop was walked.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub by_depth: Vec<usize>,
    /// Rows sitting in test scope: the existing coverage of a change. Present when more than one
    /// hop was walked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tests_reaching: Option<usize>,
    /// Calls spelling the selected name that were not linked to it, because their receiver or
    /// qualifier points at a type outside the tree.
    #[serde(skip_serializing_if = "is_zero")]
    pub declined: usize,
    pub shown: usize,
    pub total: usize,
    pub truncated: bool,
    pub counts_floor: bool,
    pub estimated_tokens: usize,
    pub graph: GraphSummary,
}

impl CodeRefsOutput {
    /// The file a row's call sites sit in when they need no path: the caller's own body, or for
    /// callees the selected symbol's.
    #[must_use]
    pub fn site_home<'a>(&'a self, row: &'a ReferenceRow) -> &'a str {
        if self.direction == Direction::Callees.name() {
            self.matched.first().map_or("", |seed| seed.path.as_str())
        } else {
            row.symbol.path.as_str()
        }
    }

    /// A row's call sites labelled against [`Self::site_home`].
    #[must_use]
    pub fn site_labels(&self, row: &ReferenceRow) -> Vec<String> {
        let home = self.site_home(row);
        row.sites.iter().map(|site| site.label(home)).collect()
    }
}

impl ReferenceRow {
    /// Call sites the per-row cap left out.
    #[must_use]
    pub fn sites_withheld(&self) -> usize {
        self.sites_total.saturating_sub(self.sites.len())
    }
}

/// A selector that named nothing.
///
/// Distinct from an empty result on purpose. `code_refs` on a symbol with no callers is the answer
/// `0`; `code_refs` on a misspelled symbol is not an answer at all, and returning `0` for it would
/// let a caller conclude something false about code that exists.
#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectorRefusal {
    /// Always true, so a consumer can branch without inspecting the shape.
    pub refused: bool,
    pub selector: String,
    pub reason: String,
    /// Nearest known symbol names, by edit distance. Empty when nothing is close.
    pub did_you_mean: Vec<String>,
    pub symbols_known: usize,
}

impl SelectorRefusal {
    #[must_use]
    pub fn new(selector: String, did_you_mean: Vec<String>, symbols_known: usize) -> Self {
        let reason = if did_you_mean.is_empty() {
            format!("no symbol named `{selector}` was found in the indexed tree")
        } else {
            format!(
                "no symbol named `{selector}` was found; the nearest known names are listed in \
                 didYouMean"
            )
        };
        Self {
            refused: true,
            selector,
            reason,
            did_you_mean,
            symbols_known,
        }
    }
}
