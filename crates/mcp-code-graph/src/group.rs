//! The code-graph tool group.

use std::{
    mem::size_of,
    sync::{Arc, Mutex},
};

use tokio::sync::mpsc;

use tokio_util::sync::CancellationToken;
use workcell_code_graph::FactsCache;
use workcell_mcp_files::{FileResource, FileResourceAccess, FileToolGroup};

use crate::{
    crawl::{crawl, crawl_filesystem_limits},
    engine::{CodeGraph, estimate_tokens},
    error::CodeGraphError,
    limits::CodeGraphLimits,
    model_text::ModelText,
    progress::{GraphPhase, GraphProgress, GraphProgressSink, report},
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

#[derive(Debug)]
struct PreparedScope {
    resource: FileResource,
    display_path: String,
}

impl PreparedScope {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.resource.retained_bytes())
            .saturating_add(self.display_path.capacity())
    }
}

#[derive(Debug)]
pub struct PreparedCodeMap {
    scope: PreparedScope,
    limit: usize,
}

#[derive(Debug)]
pub struct PreparedCodeContext {
    scope: PreparedScope,
    task: String,
    limit: usize,
}

#[derive(Debug)]
pub struct PreparedCodeRefs {
    scope: PreparedScope,
    symbol: String,
    direction: crate::types::Direction,
    limit: usize,
}

#[derive(Debug)]
pub struct PreparedCodeImpact {
    scope: PreparedScope,
    symbol: String,
    depth: usize,
    limit: usize,
}

#[derive(Debug)]
pub struct PreparedCodeExpand {
    scope: PreparedScope,
    symbol: String,
}

macro_rules! scope_accessors {
    ($prepared:ty) => {
        impl $prepared {
            #[must_use]
            pub fn scope(&self) -> &FileResource {
                &self.scope.resource
            }

            #[must_use]
            pub fn path(&self) -> &str {
                &self.scope.display_path
            }
        }
    };
}

scope_accessors!(PreparedCodeMap);
scope_accessors!(PreparedCodeContext);
scope_accessors!(PreparedCodeRefs);
scope_accessors!(PreparedCodeImpact);
scope_accessors!(PreparedCodeExpand);

impl PreparedCodeMap {
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>().saturating_add(self.scope.retained_bytes())
    }

    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }
}

impl PreparedCodeContext {
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.scope.retained_bytes())
            .saturating_add(self.task.capacity())
    }

    #[must_use]
    pub fn task(&self) -> &str {
        &self.task
    }

    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }
}

impl PreparedCodeRefs {
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.scope.retained_bytes())
            .saturating_add(self.symbol.capacity())
    }

    #[must_use]
    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    #[must_use]
    pub const fn direction(&self) -> crate::types::Direction {
        self.direction
    }

    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }
}

impl PreparedCodeImpact {
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.scope.retained_bytes())
            .saturating_add(self.symbol.capacity())
    }

    #[must_use]
    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    #[must_use]
    pub const fn depth(&self) -> usize {
        self.depth
    }

    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }
}

impl PreparedCodeExpand {
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.scope.retained_bytes())
            .saturating_add(self.symbol.capacity())
    }

    #[must_use]
    pub fn symbol(&self) -> &str {
        &self.symbol
    }
}

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
    ///
    /// The supplied group carries its own traversal bounds, and the crawl cannot see more files
    /// than that group will enumerate. Rather than let the two disagree silently — which would make
    /// the result claim a ceiling it never reached — `limits` is clamped to what the group can
    /// actually deliver, so `truncated_by` names the bound that really fired.
    #[must_use]
    pub fn from_files(files: Arc<FileToolGroup>, limits: CodeGraphLimits) -> Self {
        let filesystem = files.limits();
        let limits = CodeGraphLimits {
            max_files: limits.max_files.min(filesystem.max_search_results),
            max_traversal_entries: limits
                .max_traversal_entries
                .min(filesystem.max_traversal_entries),
            ..limits
        };
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

    pub async fn inspect_scope(
        &self,
        path: Option<&str>,
    ) -> Result<workcell_mcp_files::FileResource, CodeGraphError> {
        self.files
            .inspect_path(scope(path).unwrap_or("."), FileResourceAccess::Traverse)
            .await
            .map_err(CodeGraphError::from)
    }

    pub fn prepare_code_map(
        &self,
        input: CodeMapInput,
        scope: FileResource,
    ) -> Result<PreparedCodeMap, CodeGraphError> {
        Ok(PreparedCodeMap {
            scope: prepare_scope(scope, input.path.as_deref())?,
            limit: self.limits.resolve_limit(input.limit),
        })
    }

    pub fn prepare_code_context(
        &self,
        input: CodeContextInput,
        scope: FileResource,
    ) -> Result<PreparedCodeContext, CodeGraphError> {
        if input.task.trim().is_empty() {
            return Err(CodeGraphError::invalid("task must not be empty"));
        }
        Ok(PreparedCodeContext {
            scope: prepare_scope(scope, input.path.as_deref())?,
            task: input.task,
            limit: self.limits.resolve_limit(input.limit),
        })
    }

    pub fn prepare_code_refs(
        &self,
        input: CodeRefsInput,
        scope: FileResource,
    ) -> Result<PreparedCodeRefs, CodeGraphError> {
        if input.symbol.trim().is_empty() {
            return Err(CodeGraphError::invalid("symbol must not be empty"));
        }
        Ok(PreparedCodeRefs {
            scope: prepare_scope(scope, input.path.as_deref())?,
            symbol: input.symbol,
            direction: input.direction,
            limit: self.limits.resolve_limit(input.limit),
        })
    }

    pub fn prepare_code_impact(
        &self,
        input: CodeImpactInput,
        scope: FileResource,
    ) -> Result<PreparedCodeImpact, CodeGraphError> {
        if input.symbol.trim().is_empty() {
            return Err(CodeGraphError::invalid("symbol must not be empty"));
        }
        Ok(PreparedCodeImpact {
            scope: prepare_scope(scope, input.path.as_deref())?,
            symbol: input.symbol,
            depth: input
                .depth
                .unwrap_or(DEFAULT_IMPACT_DEPTH)
                .clamp(1, MAX_IMPACT_DEPTH),
            limit: self.limits.resolve_limit(input.limit),
        })
    }

    pub fn prepare_code_expand(
        &self,
        input: CodeExpandInput,
        scope: FileResource,
    ) -> Result<PreparedCodeExpand, CodeGraphError> {
        if input.symbol.trim().is_empty() {
            return Err(CodeGraphError::invalid("symbol must not be empty"));
        }
        Ok(PreparedCodeExpand {
            scope: prepare_scope(scope, input.path.as_deref())?,
            symbol: input.symbol,
        })
    }

    /// Crawls and builds a graph for one request.
    async fn graph_for(
        &self,
        scope: &PreparedScope,
        progress: Option<&dyn GraphProgressSink>,
        token: &CancellationToken,
    ) -> Result<CodeGraph, CodeGraphError> {
        let authoritative_path = scope.resource.path.to_string_lossy();
        let current = self
            .files
            .inspect_path(&authoritative_path, FileResourceAccess::Traverse)
            .await
            .map_err(CodeGraphError::from)?;
        if current.path != scope.resource.path {
            return Err(CodeGraphError::invalid(
                "code-graph scope changed after authorization",
            ));
        }
        report(progress, GraphPhase::Crawl, 0).await;
        let crawled = crawl(
            &self.files,
            Some(&authoritative_path),
            &self.limits,
            progress,
            token,
        )
        .await?;
        let cache = Arc::clone(&self.cache);
        let limits = self.limits;
        // The build cannot await, so its phase changes arrive over a channel that this function
        // drains while the build runs. Unbounded because a phase report must never block the work
        // it is describing, and the sender is dropped by the build itself, which is what ends the
        // drain.
        let (phases, mut reports) = mpsc::unbounded_channel();
        // Parsing and ranking a whole tree is CPU-bound for as long as the tree is large, and the
        // extraction it starts owns threads of its own. Left on a runtime worker it would stall
        // every other tool call this process is serving for that entire time.
        let build = tokio::task::spawn_blocking(move || {
            let mut cache = cache
                .lock()
                .map_err(|_| CodeGraphError::Internal("extraction cache is poisoned"))?;
            let notify = move |phase, files| {
                let _ = phases.send(GraphProgress { phase, files });
            };
            Ok(CodeGraph::build(
                crawled,
                &limits,
                &mut cache,
                Some(&notify),
            ))
        });
        let drain = async {
            while let Some(progress_report) = reports.recv().await {
                if let Some(sink) = progress {
                    sink.publish(progress_report).await;
                }
            }
        };
        let (built, ()) = tokio::join!(build, drain);
        built.map_err(|_| CodeGraphError::Internal("graph construction did not complete"))?
    }

    /// Ranked symbols for a tree.
    pub async fn code_map(
        &self,
        input: CodeMapInput,
        progress: Option<&dyn GraphProgressSink>,
        token: &CancellationToken,
    ) -> Result<CodeMapOutput, CodeGraphError> {
        let scope = self.inspect_scope(input.path.as_deref()).await?;
        let prepared = self.prepare_code_map(input, scope)?;
        self.execute_prepared_code_map(prepared, progress, token)
            .await
    }

    pub async fn execute_prepared_code_map(
        &self,
        prepared: PreparedCodeMap,
        progress: Option<&dyn GraphProgressSink>,
        token: &CancellationToken,
    ) -> Result<CodeMapOutput, CodeGraphError> {
        let graph = self.graph_for(&prepared.scope, progress, token).await?;

        let ordered = graph.ordered();
        let total = ordered.len();
        let symbols: Vec<RankedSymbol> = ordered
            .into_iter()
            .take(prepared.limit)
            .filter_map(|node| graph.ranked(node))
            .collect();

        let mut output = CodeMapOutput {
            path: prepared.scope.display_path,
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
        progress: Option<&dyn GraphProgressSink>,
        token: &CancellationToken,
    ) -> Result<CodeContextOutput, CodeGraphError> {
        let scope = self.inspect_scope(input.path.as_deref()).await?;
        let prepared = self.prepare_code_context(input, scope)?;
        self.execute_prepared_code_context(prepared, progress, token)
            .await
    }

    pub async fn execute_prepared_code_context(
        &self,
        prepared: PreparedCodeContext,
        progress: Option<&dyn GraphProgressSink>,
        token: &CancellationToken,
    ) -> Result<CodeContextOutput, CodeGraphError> {
        let graph = self.graph_for(&prepared.scope, progress, token).await?;

        let retrieval = graph.retrieve(&prepared.task, prepared.limit);
        let results: Vec<RankedSymbol> = retrieval
            .results
            .iter()
            .filter_map(|scored| graph.ranked(scored.node))
            .collect();

        let mut output = CodeContextOutput {
            task: prepared.task,
            path: prepared.scope.display_path,
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
        progress: Option<&dyn GraphProgressSink>,
        token: &CancellationToken,
    ) -> Result<Result<CodeRefsOutput, SelectorRefusal>, CodeGraphError> {
        let scope = self.inspect_scope(input.path.as_deref()).await?;
        let prepared = self.prepare_code_refs(input, scope)?;
        self.execute_prepared_code_refs(prepared, progress, token)
            .await
    }

    pub async fn execute_prepared_code_refs(
        &self,
        prepared: PreparedCodeRefs,
        progress: Option<&dyn GraphProgressSink>,
        token: &CancellationToken,
    ) -> Result<Result<CodeRefsOutput, SelectorRefusal>, CodeGraphError> {
        let graph = self.graph_for(&prepared.scope, progress, token).await?;

        let seeds = match graph.select(&prepared.symbol, &self.limits) {
            Ok(seeds) => seeds,
            Err(refusal) => return Ok(Err(refusal)),
        };

        let mut references = graph.references(&seeds, prepared.direction);
        let total = references.len();
        references.truncate(prepared.limit);

        let mut output = CodeRefsOutput {
            symbol: prepared.symbol,
            direction: prepared.direction.name(),
            unit: prepared.direction.unit(),
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
        progress: Option<&dyn GraphProgressSink>,
        token: &CancellationToken,
    ) -> Result<Result<CodeImpactOutput, SelectorRefusal>, CodeGraphError> {
        let scope = self.inspect_scope(input.path.as_deref()).await?;
        let prepared = self.prepare_code_impact(input, scope)?;
        self.execute_prepared_code_impact(prepared, progress, token)
            .await
    }

    pub async fn execute_prepared_code_impact(
        &self,
        prepared: PreparedCodeImpact,
        progress: Option<&dyn GraphProgressSink>,
        token: &CancellationToken,
    ) -> Result<Result<CodeImpactOutput, SelectorRefusal>, CodeGraphError> {
        let graph = self.graph_for(&prepared.scope, progress, token).await?;

        let seeds = match graph.select(&prepared.symbol, &self.limits) {
            Ok(seeds) => seeds,
            Err(refusal) => return Ok(Err(refusal)),
        };

        // One past the limit, so the truncation flag reflects whether more exist rather than
        // whether the walk happened to stop exactly at the boundary.
        let mut reached = graph.impact(&seeds, prepared.depth, prepared.limit.saturating_add(1));
        let total = reached.len();
        reached.truncate(prepared.limit);
        let tests_reaching = reached
            .iter()
            .filter(|row| row.test_scope)
            .cloned()
            .collect();

        let mut output = CodeImpactOutput {
            symbol: prepared.symbol,
            depth: prepared.depth,
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
        progress: Option<&dyn GraphProgressSink>,
        token: &CancellationToken,
    ) -> Result<Result<CodeExpandOutput, SelectorRefusal>, CodeGraphError> {
        let scope = self.inspect_scope(input.path.as_deref()).await?;
        let prepared = self.prepare_code_expand(input, scope)?;
        self.execute_prepared_code_expand(prepared, progress, token)
            .await
    }

    pub async fn execute_prepared_code_expand(
        &self,
        prepared: PreparedCodeExpand,
        progress: Option<&dyn GraphProgressSink>,
        token: &CancellationToken,
    ) -> Result<Result<CodeExpandOutput, SelectorRefusal>, CodeGraphError> {
        let graph = self.graph_for(&prepared.scope, progress, token).await?;
        let seeds = match graph.select(&prepared.symbol, &self.limits) {
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
            symbol: prepared.symbol,
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

fn prepare_scope(
    resource: FileResource,
    requested_path: Option<&str>,
) -> Result<PreparedScope, CodeGraphError> {
    if resource.access != FileResourceAccess::Traverse {
        return Err(CodeGraphError::invalid(
            "code-graph scope must authorize traversal",
        ));
    }
    Ok(PreparedScope {
        resource,
        display_path: scope(requested_path).unwrap_or(".").to_owned(),
    })
}

/// Folds an empty `path` onto absent.
///
/// Absent already means the whole configured root, so an empty string has exactly one sensible
/// reading and this is it. Callers reach for `""` when they mean "no scope", and refusing it spends
/// a turn on a value whose meaning was never in doubt.
///
/// Done here rather than left to the filesystem group's own folding so the contract holds for a
/// native embedder that never goes through an MCP validator, and so it survives any future change to
/// how the crawl enumerates files.
fn scope(path: Option<&str>) -> Option<&str> {
    path.filter(|path| !path.is_empty())
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
