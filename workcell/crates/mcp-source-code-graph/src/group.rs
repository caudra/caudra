//! The source-code tool group.

use std::{
    mem::size_of,
    sync::{Arc, Mutex},
};

use tokio::sync::mpsc;

use tokio_util::sync::CancellationToken;
use workcell_mcp_files::{FileResource, FileResourceAccess, FileToolGroup};
use workcell_source_code_graph::{FactsCache, NodeId};

use crate::{
    crawl::{crawl, crawl_filesystem_limits},
    engine::{SourceCodeGraph, estimate_tokens},
    error::SourceCodeGraphError,
    limits::SourceCodeGraphLimits,
    model_text::ModelText,
    progress::{GraphPhase, GraphProgress, GraphProgressSink, report},
    types::{
        Direction, RankedSymbol, SelectorRefusal, SourceCodeContextInput, SourceCodeContextOutput,
        SourceCodeRefsInput, SourceCodeRefsOutput,
    },
};

/// Deepest reference walk, regardless of what the caller asks for.
///
/// Beyond a handful of hops a backwards reachability set stops describing a blast radius and starts
/// describing the repository: in a connected call graph almost everything reaches almost everything
/// given enough hops, and the answer is true, useless, and expensive.
pub(crate) const MAX_REFS_DEPTH: usize = 8;
const DEFAULT_REFS_DEPTH: usize = 1;
const CALLEES_DEPTH_REFUSED: &str =
    "depth walks callers only; callees are one body's own calls, so ask for them with depth 1";

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
pub struct PreparedSourceCodeContext {
    scope: PreparedScope,
    task: String,
    limit: usize,
}

#[derive(Debug)]
pub struct PreparedSourceCodeRefs {
    scope: PreparedScope,
    symbol: String,
    direction: Direction,
    depth: usize,
    limit: usize,
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

scope_accessors!(PreparedSourceCodeContext);
scope_accessors!(PreparedSourceCodeRefs);

impl PreparedSourceCodeContext {
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

impl PreparedSourceCodeRefs {
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
    pub const fn direction(&self) -> Direction {
        self.direction
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

/// Task retrieval and reference tools over one confined root.
///
/// Holds no per-call state. The extraction cache is shared across calls and cannot change an
/// answer; see the engine's warm==cold gate.
pub struct SourceCodeGraphToolGroup {
    files: Arc<FileToolGroup>,
    limits: SourceCodeGraphLimits,
    // Shared rather than owned so the blocking build task can hold it across a await-free section
    // without borrowing the group.
    cache: Arc<Mutex<FactsCache>>,
}

impl std::fmt::Debug for SourceCodeGraphToolGroup {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never prints the root: a debug line is a log line, and logs must not carry paths.
        formatter
            .debug_struct("SourceCodeGraphToolGroup")
            .finish_non_exhaustive()
    }
}

impl SourceCodeGraphToolGroup {
    /// Builds a group confined to `root`.
    ///
    /// Confinement belongs to the filesystem group constructed here, which is the only resolver in
    /// the process. This type never opens a path it did not receive from that group.
    pub async fn new(
        root: impl AsRef<std::path::Path>,
        limits: Option<SourceCodeGraphLimits>,
    ) -> Result<Self, SourceCodeGraphError> {
        let limits = limits.unwrap_or_default();
        let files = FileToolGroup::new(root, false, Some(crawl_filesystem_limits(&limits)))
            .await
            .map_err(SourceCodeGraphError::from)?;
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
    pub fn from_files(files: Arc<FileToolGroup>, limits: SourceCodeGraphLimits) -> Self {
        let filesystem = files.limits();
        let limits = SourceCodeGraphLimits {
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
    pub const fn limits(&self) -> &SourceCodeGraphLimits {
        &self.limits
    }

    #[must_use]
    pub fn root(&self) -> &std::path::Path {
        self.files.root()
    }

    pub async fn inspect_scope(
        &self,
        path: Option<&str>,
    ) -> Result<workcell_mcp_files::FileResource, SourceCodeGraphError> {
        self.files
            .inspect_path(scope(path).unwrap_or("."), FileResourceAccess::Traverse)
            .await
            .map_err(SourceCodeGraphError::from)
    }

    pub fn prepare_source_code_context(
        &self,
        input: SourceCodeContextInput,
        scope: FileResource,
    ) -> Result<PreparedSourceCodeContext, SourceCodeGraphError> {
        if input.task.trim().is_empty() {
            return Err(SourceCodeGraphError::invalid("task must not be empty"));
        }
        Ok(PreparedSourceCodeContext {
            scope: prepare_scope(scope, input.path.as_deref())?,
            task: input.task,
            limit: self.limits.resolve_limit(input.limit),
        })
    }

    pub fn prepare_source_code_refs(
        &self,
        input: SourceCodeRefsInput,
        scope: FileResource,
    ) -> Result<PreparedSourceCodeRefs, SourceCodeGraphError> {
        if input.symbol.trim().is_empty() {
            return Err(SourceCodeGraphError::invalid("symbol must not be empty"));
        }
        let depth = input
            .depth
            .unwrap_or(DEFAULT_REFS_DEPTH)
            .clamp(1, MAX_REFS_DEPTH);
        if depth > 1 && input.direction == Direction::Callees {
            return Err(SourceCodeGraphError::invalid(CALLEES_DEPTH_REFUSED));
        }
        Ok(PreparedSourceCodeRefs {
            scope: prepare_scope(scope, input.path.as_deref())?,
            symbol: input.symbol,
            direction: input.direction,
            depth,
            limit: self.limits.resolve_limit(input.limit),
        })
    }

    /// Crawls and builds a graph for one request.
    async fn graph_for(
        &self,
        scope: &PreparedScope,
        progress: Option<&dyn GraphProgressSink>,
        token: &CancellationToken,
    ) -> Result<SourceCodeGraph, SourceCodeGraphError> {
        let authoritative_path = scope.resource.path.to_string_lossy();
        let current = self
            .files
            .inspect_path(&authoritative_path, FileResourceAccess::Traverse)
            .await
            .map_err(SourceCodeGraphError::from)?;
        if current.path != scope.resource.path {
            return Err(SourceCodeGraphError::invalid(
                "source-code-graph scope changed after authorization",
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
                .map_err(|_| SourceCodeGraphError::Internal("extraction cache is poisoned"))?;
            let notify = move |phase, files| {
                let _ = phases.send(GraphProgress { phase, files });
            };
            Ok(SourceCodeGraph::build(
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
        built.map_err(|_| SourceCodeGraphError::Internal("graph construction did not complete"))?
    }

    /// The task-shaped bundle.
    pub async fn source_code_context(
        &self,
        input: SourceCodeContextInput,
        progress: Option<&dyn GraphProgressSink>,
        token: &CancellationToken,
    ) -> Result<SourceCodeContextOutput, SourceCodeGraphError> {
        let scope = self.inspect_scope(input.path.as_deref()).await?;
        let prepared = self.prepare_source_code_context(input, scope)?;
        self.execute_prepared_source_code_context(prepared, progress, token)
            .await
    }

    pub async fn execute_prepared_source_code_context(
        &self,
        prepared: PreparedSourceCodeContext,
        progress: Option<&dyn GraphProgressSink>,
        token: &CancellationToken,
    ) -> Result<SourceCodeContextOutput, SourceCodeGraphError> {
        let graph = self.graph_for(&prepared.scope, progress, token).await?;

        let retrieval = graph.retrieve(&prepared.task, prepared.limit);
        let results: Vec<RankedSymbol> = retrieval
            .results
            .iter()
            .filter_map(|scored| graph.ranked(scored.node))
            .collect();

        let mut output = SourceCodeContextOutput {
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
    pub async fn source_code_refs(
        &self,
        input: SourceCodeRefsInput,
        progress: Option<&dyn GraphProgressSink>,
        token: &CancellationToken,
    ) -> Result<Result<SourceCodeRefsOutput, SelectorRefusal>, SourceCodeGraphError> {
        let scope = self.inspect_scope(input.path.as_deref()).await?;
        let prepared = self.prepare_source_code_refs(input, scope)?;
        self.execute_prepared_source_code_refs(prepared, progress, token)
            .await
    }

    pub async fn execute_prepared_source_code_refs(
        &self,
        prepared: PreparedSourceCodeRefs,
        progress: Option<&dyn GraphProgressSink>,
        token: &CancellationToken,
    ) -> Result<Result<SourceCodeRefsOutput, SelectorRefusal>, SourceCodeGraphError> {
        let graph = self.graph_for(&prepared.scope, progress, token).await?;

        let seeds = match graph.select(&prepared.symbol, &self.limits) {
            Ok(seeds) => seeds,
            Err(refusal) => return Ok(Err(refusal)),
        };

        let (mut references, by_depth) =
            graph.references(&seeds, prepared.direction, prepared.depth);
        let total = references.len();
        references.truncate(prepared.limit);
        let walked = prepared.depth > 1;
        let tests_reaching = walked.then(|| {
            references
                .iter()
                .filter(|row| row.symbol.test_scope)
                .count()
        });
        let declined = match prepared.direction {
            Direction::Callers => graph.declined(selected_name(&graph, &seeds)),
            Direction::Callees => 0,
        };

        let mut output = SourceCodeRefsOutput {
            symbol: prepared.symbol,
            direction: prepared.direction.name(),
            unit: prepared.direction.unit(),
            depth: prepared.depth,
            matched: seeds.iter().filter_map(|&n| graph.symbol_ref(n)).collect(),
            shown: references.len(),
            truncated: total > references.len(),
            references,
            by_depth,
            tests_reaching,
            declined,
            total,
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
) -> Result<PreparedScope, SourceCodeGraphError> {
    if resource.access != FileResourceAccess::Traverse {
        return Err(SourceCodeGraphError::invalid(
            "source-code-graph scope must authorize traversal",
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

/// The name every matched definition shares.
fn selected_name<'a>(graph: &'a SourceCodeGraph, seeds: &[NodeId]) -> &'a str {
    seeds
        .first()
        .and_then(|&seed| graph.facts.definition(seed))
        .map_or("", |definition| definition.name.as_str())
}
