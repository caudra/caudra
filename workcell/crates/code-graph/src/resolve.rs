//! Turns references into weighted edges.
//!
//! This is the approximate part, and it is approximate on purpose. tree-sitter is syntax-only: no
//! types, no name resolution, no cross-file knowledge. Linking a reference to the definition it
//! means is undecidable without a per-language semantic analyzer, which this crate does not have
//! and does not pretend to have.
//!
//! The deliverable is an importance *ranking*, not a sound call graph. False edges are expected.
//! What is not acceptable is a false edge that looks like a certain one, so every approximation
//! here is either bounded by a rule that makes it sound, or disclosed.

use std::collections::{HashMap, HashSet};

use workcell_source_languages::{Language, ReferenceKind, SymbolKind};

use crate::{
    extract::STANDARD_CRATE_ROOTS,
    model::{CallForm, Definition, Facts, FileId, NodeId, Reference, SourceFile},
};

/// How a reference was matched to its target, in descending confidence.
///
/// The tier is not decoration: it becomes the reference's contribution to the edge weight, so a
/// guess and a certainty do not push the ranking equally hard.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ResolutionTier {
    /// The syntax named the target unambiguously. Only shipped where a language makes it sound.
    Precise,
    /// A definition in the same file.
    SameFile,
    /// A definition in the same directory.
    SameDirectory,
    /// The single same-language definition anywhere in the tree.
    UniqueGlobal,
}

impl ResolutionTier {
    /// Base confidence contributed by one reference resolved at this tier.
    #[must_use]
    pub const fn confidence(self) -> f64 {
        match self {
            Self::Precise => 1.0,
            Self::SameFile => 0.9,
            Self::SameDirectory => 0.7,
            Self::UniqueGlobal => 0.5,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Precise => "precise",
            Self::SameFile => "same-file",
            Self::SameDirectory => "same-directory",
            Self::UniqueGlobal => "unique-global",
        }
    }
}

/// A name defined in so many places that matching it says almost nothing.
///
/// `new`, `build`, `get`, `run`. Ripwire measured this threshold; the deboost keeps a common name
/// from dominating a ranking through sheer multiplicity rather than importance.
const OVER_COMMON_DEFINITIONS: usize = 16;
const OVER_COMMON_DEBOOST: f64 = 0.5;
/// A leading underscore is the near-universal spelling for "private to this module".
const PRIVATE_DEBOOST: f64 = 0.8;
/// Edge weight ceiling. Bounds the tail so one hot call site cannot swamp the graph.
const MAX_EDGE_WEIGHT: f64 = 8.0;
/// Rust method names the standard library's own types define: `Vec`, `HashMap`, `Option`,
/// `Result`, `String`, `str`, `Mutex`, iterators, channels, and the operator and formatting traits.
///
/// A member call by one of these names, on a receiver whose type the syntax does not state, is at
/// least as likely to reach the standard library as a lone in-tree method of that name. Before this
/// gate one in-tree `fn clone` collected a thousand callers. Such a call binds an in-tree method only
/// when the calling file names the type that owns it; otherwise it is declined. Sorted by byte,
/// because it is binary searched.
const RUST_BUILTIN_METHODS: &[&str] = &[
    "add",
    "all",
    "and_then",
    "any",
    "as_bytes",
    "as_deref",
    "as_deref_mut",
    "as_mut",
    "as_ref",
    "as_slice",
    "as_str",
    "binary_search",
    "borrow",
    "borrow_mut",
    "bytes",
    "capacity",
    "chain",
    "chars",
    "chunks",
    "clear",
    "clone",
    "cloned",
    "cmp",
    "collect",
    "contains",
    "contains_key",
    "context",
    "copied",
    "count",
    "dedup",
    "default",
    "deref",
    "deref_mut",
    "display",
    "div",
    "drain",
    "drop",
    "ends_with",
    "entry",
    "enumerate",
    "eq",
    "err",
    "expect",
    "extend",
    "extend_from_slice",
    "filter",
    "filter_map",
    "find",
    "finish",
    "first",
    "flat_map",
    "flush",
    "fmt",
    "fold",
    "from",
    "get",
    "get_mut",
    "get_or_insert_with",
    "hash",
    "index",
    "index_mut",
    "insert",
    "into",
    "into_iter",
    "is_empty",
    "is_err",
    "is_none",
    "is_ok",
    "is_some",
    "iter",
    "iter_mut",
    "join",
    "keys",
    "last",
    "len",
    "lines",
    "lock",
    "map",
    "map_err",
    "map_or",
    "map_or_else",
    "max",
    "min",
    "mul",
    "ne",
    "neg",
    "next",
    "not",
    "notify_all",
    "notify_one",
    "ok",
    "ok_or",
    "ok_or_else",
    "or_default",
    "or_insert",
    "or_insert_with",
    "parse",
    "partial_cmp",
    "peekable",
    "pop",
    "position",
    "push",
    "push_str",
    "read",
    "read_line",
    "read_to_string",
    "recv",
    "remove",
    "replace",
    "reserve",
    "retain",
    "rev",
    "send",
    "skip",
    "sort",
    "sort_by",
    "sort_by_key",
    "sort_unstable",
    "split",
    "split_whitespace",
    "starts_with",
    "strip_prefix",
    "strip_suffix",
    "sub",
    "sum",
    "swap",
    "take",
    "to_lowercase",
    "to_owned",
    "to_str",
    "to_string",
    "to_uppercase",
    "to_vec",
    "trim",
    "trim_end",
    "trim_start",
    "truncate",
    "try_from",
    "try_into",
    "try_lock",
    "try_recv",
    "try_send",
    "unwrap",
    "unwrap_err",
    "unwrap_or",
    "unwrap_or_default",
    "unwrap_or_else",
    "values",
    "values_mut",
    "wait",
    "windows",
    "with_context",
    "write",
    "write_all",
    "zip",
];
/// The qualifier naming the enclosing `impl`'s own type.
const SELF_TYPE: &str = "Self";
/// The receiver naming the current value, which is also the path root naming the current module.
const SELF_VALUE: &str = "self";
/// Path roots that name a module of this crate rather than a type.
const RUST_MODULE_ROOTS: &[&str] = &["crate", "self", "super"];
const RUST_EXTENSION: &str = ".rs";

/// One reference that produced an edge, for the refs surfaces to cite.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallSite {
    pub file: FileId,
    pub line: usize,
    pub byte: usize,
}

/// One resolved edge, before it is laid out as CSR.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Edge {
    pub from: NodeId,
    pub to: NodeId,
    /// Zero when every contributing reference was a name-only member guess.
    pub weight: f64,
    /// References that contributed, name-only guesses included.
    pub references: u32,
}

/// What resolution could not do, kept so the caller can disclose it.
#[derive(Clone, Debug, Default)]
pub struct ResolutionDiagnostics {
    /// References that resolved to more than one candidate at the chosen tier.
    ///
    /// The resolver does not pick one. It splits the weight `1/k` across all of them, which
    /// tolerates the ambiguity instead of inventing a tiebreak.
    pub ambiguous: usize,
    /// References whose name matched no compatible definition anywhere.
    pub unresolved: usize,
    /// Per-symbol ambiguity counts, for the `amb=` annotation.
    pub ambiguity_by_node: HashMap<NodeId, u32>,
    /// Edges dropped because a symbol referenced itself.
    pub self_loops_dropped: usize,
    /// Call references whose name the tree defines, but where a scope rule showed no in-tree
    /// definition can be the target: `Vec::new()`, a `.clone()` on a value of unnamed type, a
    /// name imported from outside the crate.
    pub declined: usize,
    /// Declined call references per called name.
    pub declined_by_name: HashMap<String, u32>,
}

/// The resolved call graph in in-edge compressed sparse row form.
///
/// Keyed by **target**, not source. PageRank propagates rank along incoming edges, so the power
/// iteration multiplies the transpose; an in-edge layout makes that a per-row gather with one
/// sequential write per target. A source-keyed layout would force a scatter with random writes and
/// write races, which is the opposite of what the iteration wants.
#[derive(Clone, Debug, Default)]
pub struct Graph {
    /// Per-target start offsets, length `nodes + 1`.
    pub row_offsets: Vec<u32>,
    /// Source node of each in-edge.
    pub col_indices: Vec<NodeId>,
    /// Weight of each in-edge, parallel to `col_indices`.
    pub values: Vec<f64>,
    /// Weighted out-degree per source, for the `1/outdeg` normalization.
    pub weighted_out_degree: Vec<f64>,
    /// Sources with no outgoing edge of positive weight. Their rank mass has nowhere to flow and
    /// must be redistributed.
    pub dangling: Vec<bool>,
    pub diagnostics: ResolutionDiagnostics,
    /// Per-edge start offsets into `sites`, parallel to `col_indices`, length `edges + 1`.
    site_offsets: Vec<u32>,
    /// Every reference that produced an edge, grouped by edge and sorted by `(file, byte)`.
    sites: Vec<CallSite>,
}

impl Graph {
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.row_offsets.len().saturating_sub(1)
    }

    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.col_indices.len()
    }

    /// Out-edge targets per source, built by transposing the in-edge CSR.
    ///
    /// The CSR is keyed by target because that is what the power iteration wants. Answering "what
    /// does this symbol call" is the other direction, and it is a one-pass transpose rather than a
    /// second stored layout: keeping both in the graph would double the memory every ranking run
    /// pays for, to serve a question only the reference tools ask.
    ///
    /// Each row is ascending, so a caller reading it produces the same order on every run.
    #[must_use]
    pub fn out_adjacency(&self) -> Vec<Vec<NodeId>> {
        let mut out: Vec<Vec<NodeId>> = vec![Vec::new(); self.node_count()];
        for target in 0..self.node_count() {
            let Ok(target_id) = NodeId::try_from(target) else {
                continue;
            };
            for (source, _) in self.in_edges(target_id) {
                if let Some(row) = out.get_mut(source as usize) {
                    row.push(target_id);
                }
            }
        }
        // The CSR is ascending by target within a row, so pushes arrive ascending per source
        // already; the sort is cheap insurance against that layout changing.
        for row in &mut out {
            row.sort_unstable();
            row.dedup();
        }
        out
    }

    /// The in-edges of one target: `(source, weight)` pairs.
    pub fn in_edges(&self, target: NodeId) -> impl Iterator<Item = (NodeId, f64)> + '_ {
        let index = target as usize;
        let (start, end) = match (self.row_offsets.get(index), self.row_offsets.get(index + 1)) {
            (Some(&start), Some(&end)) => (start as usize, end as usize),
            _ => (0, 0),
        };
        self.col_indices[start..end]
            .iter()
            .copied()
            .zip(self.values[start..end].iter().copied())
    }

    /// Every reference that produced the edge `from -> to`, sorted by `(file, byte)`. Empty when
    /// there is no such edge.
    #[must_use]
    pub fn call_sites(&self, from: NodeId, to: NodeId) -> &[CallSite] {
        let Some(edge) = self.edge_index(from, to) else {
            return &[];
        };
        match (self.site_offsets.get(edge), self.site_offsets.get(edge + 1)) {
            (Some(&start), Some(&end)) => &self.sites[start as usize..end as usize],
            _ => &[],
        }
    }

    /// Whether every reference producing the edge `from -> to` was a name-only member guess: a
    /// call through a receiver whose type nothing in the calling file names.
    ///
    /// Such an edge is listed so the refs surfaces can show it, but it carries no rank. False when
    /// there is no such edge.
    #[must_use]
    pub fn name_only(&self, from: NodeId, to: NodeId) -> bool {
        // A proven reference always contributes a positive weight, so a zero weight is exactly an
        // edge made of guesses.
        self.edge_index(from, to)
            .is_some_and(|edge| self.values[edge] <= 0.0)
    }

    /// The CSR slot of the edge `from -> to`. A row is ascending by source, so this is a binary
    /// search within the target's row.
    fn edge_index(&self, from: NodeId, to: NodeId) -> Option<usize> {
        let index = to as usize;
        let start = *self.row_offsets.get(index)? as usize;
        let end = *self.row_offsets.get(index + 1)? as usize;
        self.col_indices[start..end]
            .binary_search(&from)
            .ok()
            .map(|offset| start + offset)
    }
}

/// An index from name to the definitions that spell it.
struct NameIndex {
    by_name: HashMap<String, Vec<NodeId>>,
}

impl NameIndex {
    fn build(facts: &Facts) -> Self {
        let mut by_name: HashMap<String, Vec<NodeId>> = HashMap::new();
        for definition in &facts.definitions {
            // A section is data, not a call target. Indexing one would let a YAML key answer a
            // function call that happens to share its spelling.
            if !definition.kind.callable() {
                continue;
            }
            by_name
                .entry(definition.name.clone())
                .or_default()
                .push(definition.node);
        }
        for nodes in by_name.values_mut() {
            nodes.sort_unstable();
        }
        Self { by_name }
    }

    fn candidates(&self, name: &str) -> &[NodeId] {
        self.by_name.get(name).map_or(&[], Vec::as_slice)
    }
}

/// What the tree says about Rust scopes, for the qualified-call and outside-import rules.
struct RustScope<'facts> {
    /// Every struct, enum, union, alias and trait name, and every `impl` self type.
    types: HashSet<&'facts str>,
    /// Inline `mod` names, and every file stem and directory a `.rs` path passes through, with
    /// `-` read as `_` the way Cargo names a crate after its directory.
    modules: HashSet<String>,
    /// `(file, name)` for every name a file's `use` brings in from outside the crate.
    outside_imports: HashSet<(FileId, &'facts str)>,
}

impl<'facts> RustScope<'facts> {
    fn build(facts: &'facts Facts) -> Self {
        let is_rust = |file: FileId| {
            facts
                .file(file)
                .is_some_and(|file| file.language == Language::Rust)
        };
        let mut types = HashSet::new();
        let mut modules = HashSet::new();
        for definition in facts
            .definitions
            .iter()
            .filter(|definition| is_rust(definition.file))
        {
            match definition.kind {
                SymbolKind::Class | SymbolKind::Interface => {
                    types.insert(definition.name.as_str());
                }
                SymbolKind::Module => {
                    modules.insert(definition.name.clone());
                }
                _ => {}
            }
            if let Some(owner) = &definition.owner {
                types.insert(owner.as_str());
            }
        }
        for file in facts
            .files
            .iter()
            .filter(|file| file.language == Language::Rust)
        {
            let stem = file.path.strip_suffix(RUST_EXTENSION).unwrap_or(&file.path);
            for segment in stem.split('/').filter(|segment| !segment.is_empty()) {
                modules.insert(segment.replace('-', "_"));
            }
        }

        let mut scope = Self {
            types,
            modules,
            outside_imports: HashSet::new(),
        };
        scope.outside_imports = facts
            .references
            .iter()
            .filter(|reference| reference.kind == ReferenceKind::Import && is_rust(reference.file))
            .filter_map(|reference| {
                let root = reference.qualifier.as_deref()?;
                (!scope.reaches_this_tree(root))
                    .then_some((reference.file, reference.name.as_str()))
            })
            .collect();
        scope
    }

    /// Whether a `use` path's first segment reaches into this tree.
    ///
    /// A workspace crate is named for its directory, sometimes behind a prefix
    /// (`workcell_code_graph` lives in `code-graph`), so every `_`-separated suffix of the root is
    /// tried. That errs toward in-tree on purpose: a mistaken match keeps the plain name ladder,
    /// while a mistaken miss would decline a real call.
    fn reaches_this_tree(&self, root: &str) -> bool {
        if STANDARD_CRATE_ROOTS.contains(&root) {
            return false;
        }
        RUST_MODULE_ROOTS.contains(&root)
            || self.types.contains(root)
            || std::iter::once(root)
                .chain(root.match_indices('_').map(|(index, _)| &root[index + 1..]))
                .any(|suffix| self.modules.contains(suffix))
    }

    fn imported_from_outside(&self, file: FileId, name: &str) -> bool {
        self.outside_imports.contains(&(file, name))
    }
}

/// A definition a reference was bound to, and whether anything beyond its name supports it.
struct Target {
    node: NodeId,
    /// False for a member call bound by name alone. Such an edge is listed but carries no rank.
    proven: bool,
}

/// What resolution decided for one call reference.
enum Selection {
    Bound(ResolutionTier, Vec<Target>),
    /// The tree defines the name, but a scope rule showed none of those definitions is the target.
    Declined,
    /// The tree defines no compatible definition, or several it cannot choose between.
    Unresolved,
}

/// Per-edge accumulation, before the weight formula runs.
#[derive(Default)]
struct Accumulated {
    /// Summed confidence of the proven references.
    confidence: f64,
    /// Proven references: the weight's sublinear term.
    proven: u32,
    /// Every reference, name-only guesses included.
    references: u32,
}

/// Resolves every call reference and lays the result out as an in-edge CSR.
///
/// Non-call references are not edges. An import or an inheritance clause is a use site with its own
/// `file:line`, reported by the refs surface, but it does not describe control flow and minting an
/// edge from one would inflate every ranking.
#[must_use]
pub fn resolve(facts: &Facts) -> Graph {
    let index = NameIndex::build(facts);
    let rust = RustScope::build(facts);
    let node_count = facts.definitions.len();
    let mut diagnostics = ResolutionDiagnostics::default();

    // Accumulated per (source, target) pair rather than appended per reference. A duplicate pair
    // was never a second entry to merge later; it is the reference count in the weight formula.
    let mut accumulator: HashMap<(NodeId, NodeId), Accumulated> = HashMap::new();
    let mut sites: Vec<(NodeId, NodeId, CallSite)> = Vec::new();

    for reference in &facts.references {
        if reference.kind != ReferenceKind::Call {
            continue;
        }
        let Some(source) = reference.enclosing else {
            // A call at file scope has no caller to attribute the edge to. It is still a use site;
            // it is simply not an edge between two symbols.
            continue;
        };
        let (tier, targets) = match select(facts, &index, &rust, reference, source) {
            Selection::Bound(tier, targets) => (tier, targets),
            Selection::Declined => {
                diagnostics.declined += 1;
                *diagnostics
                    .declined_by_name
                    .entry(reference.name.clone())
                    .or_default() += 1;
                continue;
            }
            Selection::Unresolved => {
                diagnostics.unresolved += 1;
                continue;
            }
        };

        let split = targets.len();
        if split > 1 {
            diagnostics.ambiguous += 1;
            *diagnostics.ambiguity_by_node.entry(source).or_default() += 1;
        }

        let mut confidence = tier.confidence();
        if index.candidates(&reference.name).len() >= OVER_COMMON_DEFINITIONS {
            confidence *= OVER_COMMON_DEBOOST;
        }
        if reference.name.starts_with('_') {
            confidence *= PRIVATE_DEBOOST;
        }
        // The ambiguity split. Weight is divided evenly rather than assigned to an arbitrary
        // winner, so the ranking absorbs the uncertainty instead of hiding it.
        #[expect(
            clippy::cast_precision_loss,
            reason = "candidate counts are bounded by the definition ceiling, far inside f64"
        )]
        let contribution = confidence / split as f64;

        for target in targets {
            if target.node == source {
                // A self-loop is a rank sink in the Google matrix: it inflates the recursive node
                // and steals mass from everything it should be pointing at.
                diagnostics.self_loops_dropped += 1;
                continue;
            }
            let entry = accumulator.entry((source, target.node)).or_default();
            entry.references += 1;
            if target.proven {
                entry.confidence += contribution;
                entry.proven += 1;
            }
            sites.push((
                target.node,
                source,
                CallSite {
                    file: reference.file,
                    line: reference.line,
                    byte: reference.byte,
                },
            ));
        }
    }

    build_csr(node_count, accumulator, sites, diagnostics)
}

/// Whether a definition can only be reached through a receiver or a type path.
///
/// Rust records the owning type, which is exact; elsewhere the query's method capture decides.
fn is_method(definition: &Definition) -> bool {
    definition.owner.is_some() || definition.kind == SymbolKind::Method
}

/// Languages besides Rust where a bare `name()` can never reach a method: a method needs a receiver
/// or a type path. Not C++, Java, C#, Kotlin, Swift or Ruby, whose methods call siblings through an
/// implicit `this`; not Python, whose query records methods as functions.
const fn bare_call_never_reaches_a_method(language: Language) -> bool {
    matches!(
        language,
        Language::Go | Language::TypeScript | Language::JavaScript
    )
}

/// Applies the scope rules and then the resolution ladder to one reference.
///
/// If nothing matches at any tier the edge is dropped: no phantom node is ever invented for a name
/// the tree does not define.
fn select(
    facts: &Facts,
    index: &NameIndex,
    rust: &RustScope<'_>,
    reference: &Reference,
    source: NodeId,
) -> Selection {
    let Some(referring_file) = facts.file(reference.file) else {
        return Selection::Unresolved;
    };
    // Same-language only. A YAML key and a Go function that share a spelling are not the same
    // symbol, and joining them manufactures an edge out of a coincidence.
    let compatible: Vec<&Definition> = index
        .candidates(&reference.name)
        .iter()
        .filter_map(|&node| facts.definition(node))
        .filter(|definition| {
            facts
                .file(definition.file)
                .is_some_and(|file| referring_file.language.compatible_with(file.language))
        })
        .collect();
    if compatible.is_empty() {
        return Selection::Unresolved;
    }

    if referring_file.language == Language::Rust {
        return select_rust(facts, rust, reference, source, referring_file, compatible);
    }

    let compatible = if reference.call_form == CallForm::Bare
        && bare_call_never_reaches_a_method(referring_file.language)
    {
        let free = free_functions(&compatible);
        if free.is_empty() {
            return Selection::Declined;
        }
        free
    } else {
        compatible
    };
    if let Some(precise) = select_precise(facts, &compatible, reference, source) {
        return bound(ResolutionTier::Precise, &precise, true);
    }
    ladder(facts, reference, referring_file, &compatible, true)
}

/// Rust's scope rules, which its syntax makes sound without type information.
///
/// Rust has no implicit receiver, no argument-dependent lookup and no using-directive, so the call
/// form alone says what kind of definition a call can reach: a bare call a free function, a member
/// call a method, and a path call a member of the scope it spells.
fn select_rust(
    facts: &Facts,
    rust: &RustScope<'_>,
    reference: &Reference,
    source: NodeId,
    referring_file: &SourceFile,
    compatible: Vec<&Definition>,
) -> Selection {
    let caller_owner = facts
        .definition(source)
        .and_then(|definition| definition.owner.as_deref());
    match reference.call_form {
        CallForm::Bare => {
            if rust.imported_from_outside(reference.file, &reference.name) {
                return Selection::Declined;
            }
            free_ladder(facts, reference, referring_file, &compatible)
        }
        CallForm::Member => {
            select_rust_member(reference, caller_owner, facts, referring_file, compatible)
        }
        CallForm::Qualified => {
            let Some(qualifier) = reference.qualifier.as_deref() else {
                // `<T as Trait>::f()` names its scope in a form this crate does not read, so it is
                // treated as the member call it is equivalent to.
                return select_rust_member(
                    reference,
                    caller_owner,
                    facts,
                    referring_file,
                    compatible,
                );
            };
            if qualifier == SELF_TYPE {
                if let Some(owner) = caller_owner {
                    let siblings = owned_by(&compatible, owner);
                    if !siblings.is_empty() {
                        return bound(ResolutionTier::Precise, &siblings, true);
                    }
                }
                let methods = methods(&compatible);
                if methods.is_empty() {
                    return Selection::Declined;
                }
                return ladder(facts, reference, referring_file, &methods, true);
            }
            if RUST_MODULE_ROOTS.contains(&qualifier) {
                return free_ladder(facts, reference, referring_file, &compatible);
            }
            if rust.imported_from_outside(reference.file, qualifier) {
                return Selection::Declined;
            }
            if rust.types.contains(qualifier) {
                let owned = owned_by(&compatible, qualifier);
                if !owned.is_empty() {
                    return bound(ResolutionTier::Precise, &owned, true);
                }
            }
            if rust.modules.contains(qualifier) {
                return free_ladder(facts, reference, referring_file, &compatible);
            }
            // `Vec::new()`, `Arc::clone(&x)`, `std::mem::take(..)`: the path names a scope this tree
            // does not define, or a type none of whose members carries this name.
            Selection::Declined
        }
    }
}

/// A Rust member call: `value.name()`.
///
/// The receiver's type is not in the syntax. `self.name()` names the caller's own type; otherwise
/// the only evidence is a file that names the type owning a candidate. A call with evidence binds
/// at full weight. Without it, a name the standard library's types also define is declined, and any
/// other name binds by name alone: listed, but carrying no rank.
fn select_rust_member(
    reference: &Reference,
    caller_owner: Option<&str>,
    facts: &Facts,
    referring_file: &SourceFile,
    compatible: Vec<&Definition>,
) -> Selection {
    let methods = methods(&compatible);
    if methods.is_empty() {
        return Selection::Declined;
    }
    if reference.receiver.as_deref() == Some(SELF_VALUE)
        && let Some(owner) = caller_owner
    {
        let siblings = owned_by(&methods, owner);
        if !siblings.is_empty() {
            return bound(ResolutionTier::Precise, &siblings, true);
        }
    }
    let evidenced: Vec<&Definition> = methods
        .iter()
        .copied()
        .filter(|definition| {
            definition
                .owner
                .as_deref()
                .is_some_and(|owner| referring_file.mentions(owner))
        })
        .collect();
    if !evidenced.is_empty() {
        return ladder(facts, reference, referring_file, &evidenced, true);
    }
    if RUST_BUILTIN_METHODS
        .binary_search(&reference.name.as_str())
        .is_ok()
    {
        return Selection::Declined;
    }
    ladder(facts, reference, referring_file, &methods, false)
}

fn methods<'facts>(candidates: &[&'facts Definition]) -> Vec<&'facts Definition> {
    candidates
        .iter()
        .copied()
        .filter(|definition| is_method(definition))
        .collect()
}

fn free_functions<'facts>(candidates: &[&'facts Definition]) -> Vec<&'facts Definition> {
    candidates
        .iter()
        .copied()
        .filter(|definition| !is_method(definition))
        .collect()
}

fn owned_by<'facts>(candidates: &[&'facts Definition], owner: &str) -> Vec<&'facts Definition> {
    candidates
        .iter()
        .copied()
        .filter(|definition| definition.owner.as_deref() == Some(owner))
        .collect()
}

/// The ladder over the free functions only, declining when the name names nothing else.
fn free_ladder(
    facts: &Facts,
    reference: &Reference,
    referring_file: &SourceFile,
    candidates: &[&Definition],
) -> Selection {
    let free = free_functions(candidates);
    if free.is_empty() {
        return Selection::Declined;
    }
    ladder(facts, reference, referring_file, &free, true)
}

fn bound(tier: ResolutionTier, chosen: &[&Definition], proven: bool) -> Selection {
    Selection::Bound(
        tier,
        chosen
            .iter()
            .map(|definition| Target {
                node: definition.node,
                proven,
            })
            .collect(),
    )
}

/// Same file, then same directory, then the single candidate anywhere.
fn ladder(
    facts: &Facts,
    reference: &Reference,
    referring_file: &SourceFile,
    candidates: &[&Definition],
    proven: bool,
) -> Selection {
    let same_file: Vec<&Definition> = candidates
        .iter()
        .copied()
        .filter(|definition| definition.file == reference.file)
        .collect();
    if !same_file.is_empty() {
        return bound(ResolutionTier::SameFile, &same_file, proven);
    }

    let directory = referring_file.directory();
    let same_directory: Vec<&Definition> = candidates
        .iter()
        .copied()
        .filter(|definition| {
            facts
                .file(definition.file)
                .is_some_and(|file| file.directory() == directory)
        })
        .collect();
    if !same_directory.is_empty() {
        return bound(ResolutionTier::SameDirectory, &same_directory, proven);
    }

    if candidates.len() == 1 {
        return bound(ResolutionTier::UniqueGlobal, candidates, proven);
    }
    // More than one same-language global definition and nothing to choose between them. Splitting
    // across every candidate in the tree would spray weight over unrelated files, so this is a
    // deliberate ambiguity: the reference is recorded as unresolved rather than guessed.
    Selection::Unresolved
}

/// The precise tiers for languages without Rust's scope rules: qualified forms whose syntax names
/// the target soundly.
///
/// Only shipped where a language's syntax actually supports the rule. Go's qualified calls are
/// deliberately absent: `pkg.Fn()` requires reading the import block to know which package `pkg`
/// binds to, and this crate does not, so a match on the final segment would be a guess wearing a
/// precise label.
fn select_precise<'facts>(
    facts: &Facts,
    compatible: &[&'facts Definition],
    reference: &Reference,
    source: NodeId,
) -> Option<Vec<&'facts Definition>> {
    let qualifier = reference.qualifier.as_deref()?;
    let source_file = facts.definition(source).map(|definition| definition.file)?;

    // `Self::helper()` names a sibling in the same enclosing type, which is the same file by
    // construction. This is the one qualifier that is sound without cross-file knowledge.
    if qualifier == SELF_TYPE || qualifier == SELF_VALUE {
        let siblings: Vec<&Definition> = compatible
            .iter()
            .copied()
            .filter(|definition| definition.file == source_file)
            .collect();
        return (siblings.len() == 1).then_some(siblings);
    }

    // A qualifier that names a type defined in this tree, where exactly one definition of the
    // called name sits in the same file as that type. The pairing is what makes it sound: both the
    // qualifier and the member resolve, and they agree on a file.
    let qualifier_files: Vec<_> = facts
        .definitions
        .iter()
        .filter(|definition| definition.name == qualifier)
        .map(|definition| definition.file)
        .collect();
    if qualifier_files.len() != 1 {
        return None;
    }
    let target_file = qualifier_files[0];
    let members: Vec<&Definition> = compatible
        .iter()
        .copied()
        .filter(|definition| definition.file == target_file)
        .collect();
    (members.len() == 1).then_some(members)
}

/// Lays the accumulated pairs out as an in-edge CSR in the two documented passes.
fn build_csr(
    node_count: usize,
    accumulator: HashMap<(NodeId, NodeId), Accumulated>,
    mut sites: Vec<(NodeId, NodeId, CallSite)>,
    diagnostics: ResolutionDiagnostics,
) -> Graph {
    // Sorted so the CSR layout, and therefore every traversal over it, is identical across runs.
    // A HashMap iteration order is not reproducible and must never reach the output.
    let mut edges: Vec<Edge> = accumulator
        .into_iter()
        .map(|((from, to), accumulated)| {
            // A name-only guess contributes no weight: the edge is listed so the refs surfaces can
            // show it, and PageRank never sees it.
            let weight = if accumulated.proven == 0 {
                0.0
            } else {
                let count = f64::from(accumulated.proven);
                // Mean confidence times the square root of the reference count. The square root is
                // the point: repeated calls should strengthen an edge sublinearly, so a hot loop
                // raises a weight without letting call-site multiplicity alone dominate the ranking.
                ((accumulated.confidence / count) * count.sqrt()).min(MAX_EDGE_WEIGHT)
            };
            Edge {
                from,
                to,
                weight,
                references: accumulated.references,
            }
        })
        .collect();
    edges.sort_by_key(|edge| (edge.to, edge.from));

    let mut row_offsets = vec![0u32; node_count + 1];
    let mut weighted_out_degree = vec![0.0f64; node_count];

    // Pass one: count in-degree per target and prefix-sum into the row offsets.
    for edge in &edges {
        if let Some(slot) = row_offsets.get_mut(edge.to as usize + 1) {
            *slot += 1;
        }
    }
    for index in 1..row_offsets.len() {
        row_offsets[index] += row_offsets[index - 1];
    }

    // Pass two: scatter each edge into `row = target` at `column = source`. Edges are already
    // sorted by target, so this fills each row contiguously and in ascending source order.
    let mut col_indices = vec![0u32; edges.len()];
    let mut values = vec![0.0f64; edges.len()];
    let mut cursor = row_offsets.clone();
    for edge in &edges {
        let slot = cursor[edge.to as usize] as usize;
        col_indices[slot] = edge.from;
        values[slot] = edge.weight;
        cursor[edge.to as usize] += 1;
        if let Some(degree) = weighted_out_degree.get_mut(edge.from as usize) {
            *degree += edge.weight;
        }
    }

    let dangling = weighted_out_degree
        .iter()
        .map(|&degree| degree <= 0.0)
        .collect();

    // Sorted the way the edges are, so slot `i` of the CSR owns the `i`th contiguous run.
    sites.sort_unstable_by_key(|(to, from, site)| (*to, *from, site.file, site.byte, site.line));
    let mut site_offsets = Vec::with_capacity(edges.len() + 1);
    let mut end = 0u32;
    site_offsets.push(end);
    for edge in &edges {
        end += edge.references;
        site_offsets.push(end);
    }

    Graph {
        row_offsets,
        col_indices,
        values,
        weighted_out_degree,
        dangling,
        diagnostics,
        site_offsets,
        sites: sites.into_iter().map(|(_, _, site)| site).collect(),
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::{
        ingest::{IngestLimits, SourceInput, ingest},
        rank::{Teleport, pagerank},
    };

    fn graph_of(files: &[(&str, &str)]) -> (Facts, Graph) {
        let inputs = files
            .iter()
            .map(|(path, source)| SourceInput {
                path: (*path).to_owned(),
                source: (*source).to_owned(),
            })
            .collect();
        let ingested = ingest(inputs, IngestLimits::default());
        let graph = resolve(&ingested.facts);
        (ingested.facts, graph)
    }

    fn node(facts: &Facts, name: &str) -> NodeId {
        facts
            .definitions
            .iter()
            .find(|definition| definition.name == name)
            .unwrap_or_else(|| panic!("no definition named {name}"))
            .node
    }

    fn has_edge(graph: &Graph, from: NodeId, to: NodeId) -> bool {
        graph.in_edges(to).any(|(source, _)| source == from)
    }

    fn owned(facts: &Facts, owner: &str, name: &str) -> NodeId {
        facts
            .definitions
            .iter()
            .find(|definition| {
                definition.name == name && definition.owner.as_deref() == Some(owner)
            })
            .unwrap_or_else(|| panic!("no {owner}::{name}"))
            .node
    }

    fn free(facts: &Facts, name: &str) -> NodeId {
        facts
            .definitions
            .iter()
            .find(|definition| definition.name == name && definition.owner.is_none())
            .unwrap_or_else(|| panic!("no free {name}"))
            .node
    }

    #[test_case(&[
        ("src/thing.rs", "pub struct Thing;\nimpl Thing { pub fn clone(&self) -> Thing { Thing } }"),
        ("src/user.rs", "fn caller(values: Vec<u32>) { let _ = values.clone(); }"),
    ], "clone" ; "a clone on a std value")]
    #[test_case(&[
        ("src/a.rs", "fn new() {}\nfn caller() { let _ = Vec::<u32>::new(); }"),
    ], "new" ; "a std constructor beside a free new")]
    #[test_case(&[
        ("src/w.rs", "pub struct Widget;\nimpl Widget { pub fn new() -> Self { Widget } }"),
        ("src/a.rs", "fn caller() { let _ = Vec::new(); }"),
    ], "new" ; "a std constructor beside an in tree new")]
    #[test_case(&[
        ("src/a.rs", "fn take() {}\nfn caller(mut v: u32) { std::mem::take(&mut v); }"),
    ], "take" ; "a std path whose last module is not in the tree")]
    #[test_case(&[
        ("src/map.rs", "pub struct HashMap;\nimpl HashMap { pub fn new() -> Self { HashMap } }"),
        ("src/user.rs", "use std::collections::HashMap;\nfn caller() { let _ = HashMap::new(); }"),
    ], "new" ; "a type imported from std that shadows an in tree type")]
    #[test_case(&[
        ("src/a.rs", "use std::mem::swap;\nfn swap() {}\nfn caller(a: &mut u8, b: &mut u8) { swap(a, b); }"),
    ], "swap" ; "a bare call to a name imported from std")]
    #[test_case(&[
        ("src/a.rs", "fn render() {}\nfn caller(x: X) { x.render(); }"),
    ], "render" ; "a member call with only a free function of its name")]
    fn a_call_outside_the_tree_is_declined_rather_than_bound(files: &[(&str, &str)], name: &str) {
        let (_, graph) = graph_of(files);
        assert_eq!(graph.edge_count(), 0);
        assert_eq!(graph.diagnostics.declined, 1);
        assert_eq!(graph.diagnostics.declined_by_name.get(name), Some(&1));
        assert_eq!(graph.diagnostics.unresolved, 0);
    }

    #[test]
    fn a_path_naming_an_in_tree_type_binds_that_types_member() {
        let (facts, graph) = graph_of(&[
            (
                "src/w.rs",
                "pub struct Widget;\nimpl Widget { pub fn new() -> Self { Widget } }\npub struct Other;\nimpl Other { pub fn new() -> Self { Other } }",
            ),
            (
                "src/c.rs",
                "fn caller() { let _ = crate::w::Widget::new(); }",
            ),
        ]);
        let caller = node(&facts, "caller");
        assert!(has_edge(&graph, caller, owned(&facts, "Widget", "new")));
        assert!(!has_edge(&graph, caller, owned(&facts, "Other", "new")));
        assert!(!graph.name_only(caller, owned(&facts, "Widget", "new")));
    }

    #[test_case("Self::helper()" ; "a self type path")]
    #[test_case("self.helper()" ; "a self receiver")]
    fn a_call_through_self_binds_the_sibling_in_the_same_impl(call: &str) {
        let source = format!(
            "struct W;\nimpl W {{ fn bump(&self) {{ {call}; }} fn helper(&self) {{}} }}\nstruct V;\nimpl V {{ fn helper(&self) {{}} }}"
        );
        let (facts, graph) = graph_of(&[("src/a.rs", source.as_str())]);
        let bump = node(&facts, "bump");
        assert!(has_edge(&graph, bump, owned(&facts, "W", "helper")));
        assert!(!has_edge(&graph, bump, owned(&facts, "V", "helper")));
        assert!(!graph.name_only(bump, owned(&facts, "W", "helper")));
    }

    #[test]
    fn a_bare_call_binds_the_free_function_rather_than_the_method() {
        let (facts, graph) = graph_of(&[(
            "src/a.rs",
            "struct W;\nimpl W { fn helper(&self) {} }\nfn helper() {}\nfn caller() { helper(); }",
        )]);
        let caller = node(&facts, "caller");
        assert!(has_edge(&graph, caller, free(&facts, "helper")));
        assert!(!has_edge(&graph, caller, owned(&facts, "W", "helper")));
    }

    #[test]
    fn a_free_function_in_an_inline_module_still_answers_a_bare_call() {
        let (facts, graph) = graph_of(&[(
            "src/a.rs",
            "mod tests {\n    fn helper() {}\n    fn check() { helper(); }\n}",
        )]);
        assert!(has_edge(
            &graph,
            node(&facts, "check"),
            free(&facts, "helper")
        ));
    }

    #[test_case(&[
        ("src/store.rs", "pub struct Store;\nimpl Store { pub fn get(&self) -> u32 { 1 } }"),
        ("app/user.rs", "use crate::store::Store;\nfn caller(s: &Store) { s.get(); }"),
    ] ; "a file that imports the owner")]
    #[test_case(&[
        ("src/store.rs", "pub struct Store;\nimpl Store { pub fn get(&self) -> u32 { 1 } }"),
        ("app/user.rs", "fn caller(s: &Store) { s.get(); }"),
    ] ; "a file that spells the owner in a signature")]
    fn a_std_named_member_call_binds_when_the_caller_names_the_owner(files: &[(&str, &str)]) {
        let (facts, graph) = graph_of(files);
        let caller = node(&facts, "caller");
        let get = owned(&facts, "Store", "get");
        assert!(has_edge(&graph, caller, get));
        assert!(!graph.name_only(caller, get));
        assert_eq!(graph.diagnostics.declined, 0);
    }

    #[test]
    fn a_name_only_member_edge_is_listed_at_zero_weight_and_moves_no_rank() {
        let (facts, graph) = graph_of(&[
            (
                "src/w.rs",
                "pub struct Widget;\nimpl Widget { pub fn render(&self) {} }",
            ),
            ("src/c.rs", "fn caller(x: Thing) { x.render(); }"),
        ]);
        let caller = node(&facts, "caller");
        let render = owned(&facts, "Widget", "render");
        assert_eq!(graph.in_edges(render).collect::<Vec<_>>(), [(caller, 0.0)]);
        assert!(graph.name_only(caller, render));
        assert_eq!(graph.out_adjacency()[caller as usize], [render]);
        assert!(graph.dangling[caller as usize]);

        let ranking = pagerank(&graph, &Teleport::Uniform);
        assert_eq!(
            ranking.scores[render as usize].to_bits(),
            ranking.scores[caller as usize].to_bits(),
            "a guess must not move rank"
        );
    }

    #[test_case("fn caller(x: Thing) { x.render(); }", true ; "a receiver nothing types")]
    #[test_case("fn caller(x: &Widget) { x.render(); }", false ; "a receiver whose owner the file names")]
    #[test_case("fn caller() { render(); }", false ; "a bare call")]
    fn name_only_marks_exactly_the_unproven_member_edges(caller_source: &str, expected: bool) {
        let (facts, graph) = graph_of(&[
            (
                "src/w.rs",
                "pub struct Widget;\nimpl Widget { pub fn render(&self) {} }\npub fn render() {}",
            ),
            ("src/c.rs", caller_source),
        ]);
        let caller = node(&facts, "caller");
        let target = facts
            .definitions
            .iter()
            .find(|definition| {
                definition.name == "render" && has_edge(&graph, caller, definition.node)
            })
            .expect("an edge to a render")
            .node;
        assert_eq!(graph.name_only(caller, target), expected);
    }

    #[test]
    fn call_sites_list_every_reference_in_file_and_byte_order() {
        let (facts, graph) = graph_of(&[(
            "src/a.rs",
            "fn t() {}\nfn c() {\n    t();\n    t();\n    t();\n}",
        )]);
        let (caller, target) = (node(&facts, "c"), node(&facts, "t"));
        let sites = graph.call_sites(caller, target);
        assert_eq!(
            sites.iter().map(|site| site.line).collect::<Vec<_>>(),
            [3, 4, 5]
        );
        assert!(
            sites
                .windows(2)
                .all(|pair| (pair[0].file, pair[0].byte) < (pair[1].file, pair[1].byte))
        );
        assert!(graph.call_sites(target, caller).is_empty());
    }

    #[test]
    fn call_sites_stay_attached_to_their_own_edge() {
        let (facts, graph) = graph_of(&[(
            "src/a.rs",
            "fn t() {}\nfn u() {}\nfn a() { t(); u(); }\nfn b() { t(); }",
        )]);
        let target = node(&facts, "t");
        for caller in ["a", "b"] {
            let caller = node(&facts, caller);
            let sites = graph.call_sites(caller, target);
            assert_eq!(sites.len(), 1);
            let enclosing = facts
                .references
                .iter()
                .find(|reference| reference.byte == sites[0].byte)
                .and_then(|reference| reference.enclosing);
            assert_eq!(enclosing, Some(caller));
        }
    }

    #[test]
    fn the_builtin_method_table_is_sorted_for_binary_search() {
        assert!(
            RUST_BUILTIN_METHODS
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
    }

    #[test]
    fn tier_one_prefers_a_definition_in_the_same_file() {
        let (facts, graph) = graph_of(&[
            ("a.rs", "fn target() {}\nfn caller() { target(); }"),
            ("b.rs", "fn target() {}"),
        ]);
        let caller = node(&facts, "caller");
        let local = facts
            .definitions
            .iter()
            .find(|definition| {
                definition.name == "target" && facts.path_of(definition.node) == Some("a.rs")
            })
            .expect("local target")
            .node;
        let remote = facts
            .definitions
            .iter()
            .find(|definition| {
                definition.name == "target" && facts.path_of(definition.node) == Some("b.rs")
            })
            .expect("remote target")
            .node;
        assert!(has_edge(&graph, caller, local));
        assert!(!has_edge(&graph, caller, remote));
    }

    #[test]
    fn tier_two_prefers_the_same_directory_over_a_distant_definition() {
        let (facts, graph) = graph_of(&[
            ("src/a.rs", "fn caller() { target(); }"),
            ("src/b.rs", "fn target() {}"),
            ("other/c.rs", "fn target() {}"),
        ]);
        let caller = node(&facts, "caller");
        let near = facts
            .definitions
            .iter()
            .find(|definition| {
                definition.name == "target" && facts.path_of(definition.node) == Some("src/b.rs")
            })
            .expect("near")
            .node;
        let far = facts
            .definitions
            .iter()
            .find(|definition| {
                definition.name == "target" && facts.path_of(definition.node) == Some("other/c.rs")
            })
            .expect("far")
            .node;
        assert!(has_edge(&graph, caller, near));
        assert!(!has_edge(&graph, caller, far));
    }

    #[test]
    fn tier_three_accepts_a_unique_global_definition() {
        let (facts, graph) = graph_of(&[
            ("src/a.rs", "fn caller() { faraway(); }"),
            ("other/b.rs", "fn faraway() {}"),
        ]);
        assert!(has_edge(
            &graph,
            node(&facts, "caller"),
            node(&facts, "faraway")
        ));
    }

    #[test]
    fn tier_four_drops_the_edge_rather_than_inventing_a_node() {
        let (facts, graph) = graph_of(&[("a.rs", "fn caller() { nowhere_defined(); }")]);
        assert_eq!(graph.edge_count(), 0);
        assert_eq!(graph.diagnostics.unresolved, 1);
        assert_eq!(graph.node_count(), facts.definitions.len());
    }

    #[test]
    fn an_ambiguous_name_splits_weight_rather_than_picking_a_winner() {
        // Two same-directory candidates. Both get an edge, each at half the confidence, and the
        // ambiguity is counted so the caller can see the graph was unsure.
        let (facts, graph) = graph_of(&[
            ("src/a.rs", "fn caller() { target(); }"),
            ("src/b.rs", "fn target() {}"),
            ("src/c.rs", "fn target() {}"),
        ]);
        let caller = node(&facts, "caller");
        let targets: Vec<_> = facts
            .definitions
            .iter()
            .filter(|definition| definition.name == "target")
            .map(|definition| definition.node)
            .collect();
        assert_eq!(targets.len(), 2);
        for target in &targets {
            assert!(has_edge(&graph, caller, *target));
            let weight = graph
                .in_edges(*target)
                .find(|(source, _)| *source == caller)
                .expect("edge")
                .1;
            assert!(
                (weight - ResolutionTier::SameDirectory.confidence() / 2.0).abs() < 1e-9,
                "expected a halved weight, got {weight}"
            );
        }
        assert_eq!(graph.diagnostics.ambiguous, 1);
        assert_eq!(graph.diagnostics.ambiguity_by_node.get(&caller), Some(&1));
    }

    #[test]
    fn a_self_loop_is_dropped_because_it_is_a_rank_sink() {
        let (_, graph) = graph_of(&[("a.rs", "fn recurse(n: u32) { recurse(n - 1); }")]);
        assert_eq!(graph.edge_count(), 0);
        assert_eq!(graph.diagnostics.self_loops_dropped, 1);
    }

    #[test]
    fn repeated_calls_raise_the_weight_sublinearly() {
        let (facts, single) = graph_of(&[("a.rs", "fn t() {}\nfn c() { t(); }")]);
        let (_, quadruple) = graph_of(&[("a.rs", "fn t() {}\nfn c() { t(); t(); t(); t(); }")]);
        let caller = node(&facts, "c");
        let target = node(&facts, "t");
        let weight = |graph: &Graph| {
            graph
                .in_edges(target)
                .find(|(source, _)| *source == caller)
                .expect("edge")
                .1
        };
        let one = weight(&single);
        let four = weight(&quadruple);
        // sqrt(4) == 2, so four calls is twice one call, not four times.
        assert!((four - one * 2.0).abs() < 1e-9, "{one} -> {four}");
    }

    #[test]
    fn an_over_common_name_is_deboosted_where_it_still_resolves() {
        // The deboost only matters when the reference resolves anyway. The caller's own file
        // defines `new`, so tier one answers it; the weight is halved because the name says almost
        // nothing about which `new` was meant.
        let mut files: Vec<(String, String)> = (0..OVER_COMMON_DEFINITIONS)
            .map(|index| (format!("d{index}/m.rs"), "fn new() {}".to_owned()))
            .collect();
        files.push((
            "caller/c.rs".to_owned(),
            "fn new() {}\nfn c() { new(); }".to_owned(),
        ));
        let borrowed: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, source)| (path.as_str(), source.as_str()))
            .collect();
        let (facts, graph) = graph_of(&borrowed);

        let caller = node(&facts, "c");
        let local = facts
            .definitions
            .iter()
            .find(|definition| {
                definition.name == "new" && facts.path_of(definition.node) == Some("caller/c.rs")
            })
            .expect("local new")
            .node;
        let weight = graph
            .in_edges(local)
            .find(|(source, _)| *source == caller)
            .expect("edge")
            .1;
        let undeboosted = ResolutionTier::SameFile.confidence();
        assert!(
            (weight - undeboosted * OVER_COMMON_DEBOOST).abs() < 1e-9,
            "expected the over-common deboost, got {weight} against a base of {undeboosted}"
        );
    }

    #[test]
    fn a_private_name_is_deboosted() {
        let (facts, graph) = graph_of(&[("a.rs", "fn _helper() {}\nfn c() { _helper(); }")]);
        let weight = graph
            .in_edges(node(&facts, "_helper"))
            .find(|(source, _)| *source == node(&facts, "c"))
            .expect("edge")
            .1;
        assert!(
            (weight - ResolutionTier::SameFile.confidence() * PRIVATE_DEBOOST).abs() < 1e-9,
            "expected the private deboost, got {weight}"
        );
    }

    #[test]
    fn many_indistinguishable_globals_are_unresolved_rather_than_sprayed() {
        let files: Vec<(String, String)> = (0..OVER_COMMON_DEFINITIONS)
            .map(|index| (format!("d{index}/m.rs"), "fn new() {}".to_owned()))
            .chain(std::iter::once((
                "caller/c.rs".to_owned(),
                "fn c() { new(); }".to_owned(),
            )))
            .collect();
        let borrowed: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, source)| (path.as_str(), source.as_str()))
            .collect();
        let (_, graph) = graph_of(&borrowed);
        // Nothing distinguishes sixteen same-language globals, so no edge is minted at all.
        // Splitting across every candidate would spray weight over unrelated files and read as
        // sixteen real relationships.
        assert_eq!(graph.diagnostics.unresolved, 1);
        assert_eq!(graph.edge_count(), 0);
    }

    #[test]
    fn a_config_key_never_answers_a_code_call() {
        let (_, graph) = graph_of(&[
            ("Cargo.toml", "[build]\nname = \"x\"\n"),
            ("a.rs", "fn c() { build(); }"),
        ]);
        assert_eq!(graph.edge_count(), 0);
    }

    #[test_case("a.c", "b.h" ; "c resolves a header")]
    #[test_case("a.cu", "b.cpp" ; "cuda resolves cpp")]
    #[test_case("a.cpp", "b.cu" ; "cpp resolves cuda")]
    #[test_case("a.cu", "b.c" ; "cuda resolves c")]
    #[test_case("a.m", "b.h" ; "objc resolves a header")]
    fn a_c_family_call_resolves_across_the_family(caller_path: &str, callee_path: &str) {
        let (facts, graph) = graph_of(&[
            (caller_path, "void caller(void) { shared_helper(); }"),
            (callee_path, "void shared_helper(void) {}"),
        ]);
        assert!(has_edge(
            &graph,
            node(&facts, "caller"),
            node(&facts, "shared_helper")
        ));
    }

    #[test_case("__global__ void scale(float* d) {}\nvoid run(float* d) { scale<<<1, 32>>>(d); }" ; "a kernel launch")]
    #[test_case("template <int N> __global__ void scale(float* d) {}\nvoid run(float* d) { scale<32><<<1, 32>>>(d); }" ; "a templated kernel launch")]
    fn a_cuda_kernel_launch_is_a_call_edge(source: &str) {
        let (facts, graph) = graph_of(&[("kernels.cu", source)]);
        assert!(has_edge(&graph, node(&facts, "run"), node(&facts, "scale")));
    }

    #[test]
    fn an_objc_message_send_is_a_call_edge() {
        let (facts, graph) = graph_of(&[(
            "Canvas.m",
            "@implementation Canvas\n- (void)run { [self drawInContext:nil]; }\n- (void)drawInContext:(id)context {}\n@end\n",
        )]);
        assert!(has_edge(
            &graph,
            node(&facts, "run"),
            node(&facts, "drawInContext")
        ));
    }

    #[test]
    fn a_cmake_command_invocation_is_a_call_edge() {
        let (facts, graph) = graph_of(&[
            (
                "CMakeLists.txt",
                "function(canvas_add_test name)\n  add_executable(${name} a.c)\n  canvas_warn(${name})\nendfunction()\n",
            ),
            (
                "cmake/warn.cmake",
                "macro(canvas_warn target)\nendmacro()\n",
            ),
        ]);
        assert!(has_edge(
            &graph,
            node(&facts, "canvas_add_test"),
            node(&facts, "canvas_warn")
        ));
    }

    #[test]
    fn csr_rows_are_contiguous_and_offsets_are_monotonic() {
        let (facts, graph) = graph_of(&[(
            "a.rs",
            "fn t() {}\nfn one() { t(); }\nfn two() { t(); }\nfn three() { t(); }",
        )]);
        assert_eq!(graph.row_offsets.len(), facts.definitions.len() + 1);
        for window in graph.row_offsets.windows(2) {
            assert!(window[0] <= window[1], "row offsets must be non-decreasing");
        }
        assert_eq!(
            *graph.row_offsets.last().expect("offsets"),
            u32::try_from(graph.edge_count()).expect("fits")
        );
        let target = node(&facts, "t");
        assert_eq!(graph.in_edges(target).count(), 3);
        let sources: Vec<_> = graph.in_edges(target).map(|(source, _)| source).collect();
        let mut sorted = sources.clone();
        sorted.sort_unstable();
        assert_eq!(sources, sorted, "in-edges must be laid out in source order");
    }

    #[test]
    fn dangling_marks_every_symbol_with_no_outgoing_edge() {
        let (facts, graph) = graph_of(&[("a.rs", "fn leaf() {}\nfn caller() { leaf(); }")]);
        assert!(graph.dangling[node(&facts, "leaf") as usize]);
        assert!(!graph.dangling[node(&facts, "caller") as usize]);
    }

    #[test]
    fn resolution_is_byte_identical_across_repeated_runs() {
        // A HashMap accumulator has no reproducible iteration order, so the CSR is built from a
        // sorted edge list. Without that sort this test fails intermittently, which is the worst
        // possible way for a determinism bug to present.
        let files = [
            ("src/lib.rs", include_str!("lib.rs")),
            ("src/resolve.rs", include_str!("resolve.rs")),
            ("src/extract.rs", include_str!("extract.rs")),
        ];
        let shape = || {
            let (_, graph) = graph_of(&files);
            format!(
                "{:?}|{:?}|{:?}",
                graph.row_offsets, graph.col_indices, graph.values
            )
        };
        let first = shape();
        for _ in 0..4 {
            assert_eq!(first, shape());
        }
    }
}
