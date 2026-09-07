//! The code-graph tool group.

use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;
use workcell_code_graph::FactsCache;
use workcell_mcp_files::{FileResourceAccess, FileToolGroup};

use crate::{
    crawl::{crawl, crawl_filesystem_limits},
    engine::{CodeGraph, estimate_tokens},
    error::CodeGraphError,
    limits::CodeGraphLimits,
    model_text::ModelText,
    types::{
        CodeContextInput, CodeContextOutput, CodeExpandInput, CodeExpandOutput, CodeImpactInput,
        CodeImpactOutput, CodeMapInput, CodeMapOutput, CodeRefsInput, CodeRefsOutput, RankedSymbol,
        SelectorRefusal, SymbolRef,
    },
};

/// Deepest impact walk, regardless of what the caller asks for.
///
/// Beyond a handful of hops a backwards reachability set stops describing a blast radius and starts
/// describing the repository: in a connected call graph almost everything reaches almost everything
/// given enough hops, and the answer is true, useless, and expensive.
const MAX_IMPACT_DEPTH: usize = 8;
const DEFAULT_IMPACT_DEPTH: usize = 3;

/// Repository-scale symbol map, retrieval, and impact tools over one confined root.
///
/// Holds no per-call state. The extraction cache is shared across calls and cannot change an
/// answer; see the engine's warm==cold gate.
pub struct CodeGraphToolGroup {
    files: Arc<FileToolGroup>,
    limits: CodeGraphLimits,
    // Shared rather than owned so the blocking build task can hold it across a await-free section
    // without borrowing the group.
    cache: Arc<Mutex<FactsCache>>,
}

impl std::fmt::Debug for CodeGraphToolGroup {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never prints the root: a debug line is a log line, and logs must not carry paths.
        formatter
            .debug_struct("CodeGraphToolGroup")
            .finish_non_exhaustive()
    }
}

impl CodeGraphToolGroup {
    /// Builds a group confined to `root`.
    ///
    /// Confinement belongs to the filesystem group constructed here, which is the only resolver in
    /// the process. This type never opens a path it did not receive from that group.
    pub async fn new(
        root: impl AsRef<std::path::Path>,
        limits: Option<CodeGraphLimits>,
    ) -> Result<Self, CodeGraphError> {
        let limits = limits.unwrap_or_default();
        let files = FileToolGroup::new(root, false, Some(crawl_filesystem_limits(&limits)))
            .await
            .map_err(CodeGraphError::from)?;
        Ok(Self::from_files(Arc::new(files), limits))
    }

    /// Builds a group over an existing filesystem group.
    ///
    /// A host that already constructed one shares it rather than creating a second resolver with
    /// possibly different confinement, which would be two answers to one authorization question.
    #[must_use]
    pub fn from_files(files: Arc<FileToolGroup>, limits: CodeGraphLimits) -> Self {
        let cache = FactsCache::new(limits.cache);
        Self {
            files,
            limits,
            cache: Arc::new(Mutex::new(cache)),
        }
    }

    #[must_use]
    pub const fn limits(&self) -> &CodeGraphLimits {
        &self.limits
    }

    #[must_use]
    pub fn root(&self) -> &std::path::Path {
        self.files.root()
    }

    /// Crawls and builds a graph for one request.
    async fn graph_for(
        &self,
        path: Option<&str>,
        token: &CancellationToken,
    ) -> Result<CodeGraph, CodeGraphError> {
        let crawled = crawl(&self.files, path, &self.limits, token).await?;
        let cache = Arc::clone(&self.cache);
        let limits = self.limits;
        // Parsing and ranking a whole tree is CPU-bound for as long as the tree is large, and the
        // extraction it starts owns threads of its own. Left on a runtime worker it would stall
        // every other tool call this process is serving for that entire time.
        tokio::task::spawn_blocking(move || {
            let mut cache = cache
                .lock()
                .map_err(|_| CodeGraphError::Internal("extraction cache is poisoned"))?;
            Ok(CodeGraph::build(crawled, &limits, &mut cache))
        })
        .await
        .map_err(|_| CodeGraphError::Internal("graph construction did not complete"))?
    }

    /// Ranked symbols for a tree.
    pub async fn code_map(
        &self,
        input: CodeMapInput,
        token: &CancellationToken,
    ) -> Result<CodeMapOutput, CodeGraphError> {
        let limit = self.limits.resolve_limit(input.limit);
        let graph = self.graph_for(input.path.as_deref(), token).await?;

        let ordered = graph.ordered();
        let total = ordered.len();
        let symbols: Vec<RankedSymbol> = ordered
            .into_iter()
            .take(limit)
            .filter_map(|node| graph.ranked(node))
            .collect();

        let mut output = CodeMapOutput {
            path: input.path.unwrap_or_else(|| ".".to_owned()),
            shown: symbols.len(),
            truncated: total > symbols.len(),
            symbols,
            total,
            counts_floor: true,
            estimated_tokens: 0,
            graph: graph.summary,
        };
        output.estimated_tokens = estimate_tokens(output.model_text().len());
        Ok(output)
    }

    /// The task-shaped bundle.
    pub async fn code_context(
        &self,
        input: CodeContextInput,
        token: &CancellationToken,
    ) -> Result<CodeContextOutput, CodeGraphError> {
        if input.task.trim().is_empty() {
            return Err(CodeGraphError::invalid("task must not be empty"));
        }
        let limit = self.limits.resolve_limit(input.limit);
        let graph = self.graph_for(input.path.as_deref(), token).await?;

        let retrieval = graph.retrieve(&input.task, limit);
        let results: Vec<RankedSymbol> = retrieval
            .results
            .iter()
            .filter_map(|scored| graph.ranked(scored.node))
            .collect();

        let mut output = CodeContextOutput {
            task: input.task,
            path: input.path.unwrap_or_else(|| ".".to_owned()),
            shape: retrieval.shape.name().to_owned(),
            shape_reason: retrieval.shape.reason().to_owned(),
            confidence: retrieval.confidence.name().to_owned(),
            margin_percent: retrieval.margin_percent,
            shown: results.len(),
            truncated: retrieval.total_matched > results.len(),
            results,
            total_matched: retrieval.total_matched,
            counts_floor: true,
            estimated_tokens: 0,
            graph: graph.summary,
        };
        output.estimated_tokens = estimate_tokens(output.model_text().len());
        Ok(output)
    }

    /// Callers or callees of one symbol.
    pub async fn code_refs(
        &self,
        input: CodeRefsInput,
        token: &CancellationToken,
    ) -> Result<Result<CodeRefsOutput, SelectorRefusal>, CodeGraphError> {
        let limit = self.limits.resolve_limit(input.limit);
        let graph = self.graph_for(input.path.as_deref(), token).await?;

        let seeds = match graph.select(&input.symbol, &self.limits) {
            Ok(seeds) => seeds,
            Err(refusal) => return Ok(Err(refusal)),
        };

        let mut references = graph.references(&seeds, input.direction);
        let total = references.len();
        references.truncate(limit);

        let mut output = CodeRefsOutput {
            symbol: input.symbol,
            direction: input.direction.name(),
            unit: input.direction.unit(),
            matched: seeds.iter().filter_map(|&n| graph.symbol_ref(n)).collect(),
            shown: references.len(),
            truncated: total > references.len(),
            references,
            total,
            counts_floor: true,
            estimated_tokens: 0,
            graph: graph.summary,
        };
        output.estimated_tokens = estimate_tokens(output.model_text().len());
        Ok(Ok(output))
    }

    /// Blast radius of a change to one symbol.
    pub async fn code_impact(
        &self,
        input: CodeImpactInput,
        token: &CancellationToken,
    ) -> Result<Result<CodeImpactOutput, SelectorRefusal>, CodeGraphError> {
        let limit = self.limits.resolve_limit(input.limit);
        let depth = input
            .depth
            .unwrap_or(DEFAULT_IMPACT_DEPTH)
            .clamp(1, MAX_IMPACT_DEPTH);
        let graph = self.graph_for(input.path.as_deref(), token).await?;

        let seeds = match graph.select(&input.symbol, &self.limits) {
            Ok(seeds) => seeds,
            Err(refusal) => return Ok(Err(refusal)),
        };

        // One past the limit, so the truncation flag reflects whether more exist rather than
        // whether the walk happened to stop exactly at the boundary.
        let mut reached = graph.impact(&seeds, depth, limit.saturating_add(1));
        let total = reached.len();
        reached.truncate(limit);
        let tests_reaching = reached
            .iter()
            .filter(|row| row.test_scope)
            .cloned()
            .collect();

        let mut output = CodeImpactOutput {
            symbol: input.symbol,
            depth,
            matched: seeds.iter().filter_map(|&n| graph.symbol_ref(n)).collect(),
            shown: reached.len(),
            truncated: total > reached.len(),
            reached,
            tests_reaching,
            total,
            counts_floor: true,
            estimated_tokens: 0,
            graph: graph.summary,
        };
        output.estimated_tokens = estimate_tokens(output.model_text().len());
        Ok(Ok(output))
    }

    /// One symbol's body plus its one-hop neighbourhood.
    pub async fn code_expand(
        &self,
        input: CodeExpandInput,
        token: &CancellationToken,
    ) -> Result<Result<CodeExpandOutput, SelectorRefusal>, CodeGraphError> {
        let graph = self.graph_for(input.path.as_deref(), token).await?;
        let seeds = match graph.select(&input.symbol, &self.limits) {
            Ok(seeds) => seeds,
            Err(refusal) => return Ok(Err(refusal)),
        };

        // The highest-ranked match. Every match is disclosed through `callers`/`callees` counts on
        // the map, so choosing one body here narrows the answer without hiding the ambiguity.
        let node = *seeds
            .iter()
            .max_by(|&&left, &&right| {
                let left_rank = graph
                    .ranking
                    .scores
                    .get(left as usize)
                    .copied()
                    .unwrap_or(0.0);
                let right_rank = graph
                    .ranking
                    .scores
                    .get(right as usize)
                    .copied()
                    .unwrap_or(0.0);
                left_rank
                    .partial_cmp(&right_rank)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| right.cmp(&left))
            })
            .ok_or(CodeGraphError::Internal("selection returned no node"))?;

        let definition = graph
            .facts
            .definition(node)
            .ok_or(CodeGraphError::Internal("selected node has no definition"))?;
        let path = graph.facts.path_of(node).unwrap_or_default().to_owned();

        let resource = self
            .files
            .inspect_path(&path, FileResourceAccess::Read)
            .await
            .map_err(CodeGraphError::from)?;
        let file_source = std::fs::read_to_string(&resource.path)
            .map_err(|_| CodeGraphError::invalid("the defining file could not be read"))?;

        let span = definition.span;
        let body = file_source
            .get(span.start..span.end.min(file_source.len()))
            .unwrap_or_default();

        let mut truncated = false;
        let mut source = body.to_owned();
        let mut served_whole_file = None;

        // Ripwire's honest counterexample. A ranked bundle assembled from a small file can cost
        // more than the file, and selling a summary at a premium over the thing it summarizes is
        // strictly worse for the caller. Serve the file and say so.
        if file_source.len() <= self.limits.max_expand_bytes && body.len() * 2 > file_source.len() {
            source = file_source.clone();
            served_whole_file = Some(format!(
                "the symbol spans {} of {} bytes, so the whole file is served: it costs no more \
                 and omits nothing",
                body.len(),
                file_source.len()
            ));
        } else if source.len() > self.limits.max_expand_bytes {
            source.truncate(floor_char_boundary(&source, self.limits.max_expand_bytes));
            truncated = true;
        }

        let callers: Vec<SymbolRef> = graph
            .callers_of(node)
            .into_iter()
            .filter_map(|caller| graph.symbol_ref(caller))
            .collect();
        let callees: Vec<SymbolRef> = graph
            .callees_of(node)
            .iter()
            .filter_map(|&callee| graph.symbol_ref(callee))
            .collect();

        let mut output = CodeExpandOutput {
            symbol: input.symbol,
            path,
            kind: definition.kind.name().to_owned(),
            line_start: definition.lines.start,
            line_end: definition.lines.end,
            source,
            callers,
            callees,
            served_whole_file,
            truncated,
            counts_floor: true,
            estimated_tokens: 0,
            graph: graph.summary,
        };
        output.estimated_tokens = estimate_tokens(output.model_text().len());
        Ok(Ok(output))
    }
}

/// Largest index at or below `limit` that is a char boundary.
fn floor_char_boundary(text: &str, limit: usize) -> usize {
    if limit >= text.len() {
        return text.len();
    }
    let mut index = limit;
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}
