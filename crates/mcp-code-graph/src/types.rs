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

/// Ranked symbols for a tree.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodeMapInput {
    /// Root-relative subdirectory. Absent means the whole configured root.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

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

/// Callers or callees of one symbol.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodeRefsInput {
    /// A symbol name, optionally qualified by path as `path::name` or `path:name`.
    pub symbol: String,
    #[serde(default)]
    pub direction: Direction,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Blast radius of a change to one symbol.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodeImpactInput {
    pub symbol: String,
    /// Hops to walk backwards along call edges. Bounded by the host.
    #[serde(default)]
    pub depth: Option<usize>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One symbol's body and its immediate neighbourhood.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodeExpandInput {
    pub symbol: String,
    #[serde(default)]
    pub path: Option<String>,
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

/// A symbol reached by walking the graph, with how far away it is.
#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReachedSymbol {
    #[serde(flatten)]
    pub symbol: SymbolRef,
    /// Edges traversed from the seed. One means a direct caller.
    pub hops: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub test_scope: bool,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeMapOutput {
    /// The subtree that was mapped, root-relative. `.` is the whole root.
    pub path: String,
    pub symbols: Vec<RankedSymbol>,
    pub shown: usize,
    /// Symbols in the graph. Exceeds `shown` when the result window withheld some.
    pub total: usize,
    pub truncated: bool,
    /// Always true. See the module docs.
    pub counts_floor: bool,
    pub estimated_tokens: usize,
    pub graph: GraphSummary,
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
    /// Definitions the selector matched. More than one means the answer is a union.
    pub matched: Vec<SymbolRef>,
    pub references: Vec<RankedSymbol>,
    pub shown: usize,
    pub total: usize,
    pub truncated: bool,
    pub counts_floor: bool,
    pub estimated_tokens: usize,
    pub graph: GraphSummary,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeImpactOutput {
    pub symbol: String,
    pub depth: usize,
    pub matched: Vec<SymbolRef>,
    /// Symbols that reach the seed within `depth` hops, nearest first.
    pub reached: Vec<ReachedSymbol>,
    /// The subset of `reached` sitting in test scope, which is the existing coverage of this change.
    pub tests_reaching: Vec<ReachedSymbol>,
    pub shown: usize,
    pub total: usize,
    pub truncated: bool,
    pub counts_floor: bool,
    pub estimated_tokens: usize,
    pub graph: GraphSummary,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeExpandOutput {
    pub symbol: String,
    pub path: String,
    pub kind: String,
    pub line_start: usize,
    pub line_end: usize,
    pub source: String,
    /// Symbols referencing this one. A floor.
    pub callers: Vec<SymbolRef>,
    /// Symbols this one references. A floor.
    pub callees: Vec<SymbolRef>,
    /// Set when the whole file was served instead of the ranked bundle.
    ///
    /// The bundle can cost more than the file it was assembled from. When it does, the file is the
    /// cheaper and more complete answer, and pretending otherwise sells a summary at a premium.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub served_whole_file: Option<String>,
    pub truncated: bool,
    pub counts_floor: bool,
    pub estimated_tokens: usize,
    pub graph: GraphSummary,
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
