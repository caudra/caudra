//! Phase reporting for the one long call this crate makes.
//!
//! A code-graph call crawls, parses, and ranks a whole tree before it can answer anything, which on
//! a large repository is seconds of work with nothing to show. This reports which of the three
//! phases is running and how many files it has seen.
//!
//! Unlike the shell group's progress sink, `publish` returns nothing and cannot fail the call.
//! Shell progress carries output, so a dropped chunk is a hole in the result and has to be an
//! error. This carries a phase label: a host that misses one loses a frame of animation, and
//! failing a completed crawl over that would be absurd.

use async_trait::async_trait;

/// How often the crawl reports its file count.
///
/// Per-file reporting would put an await between every two `stat` calls to describe work that is
/// already fast; this is often enough to look continuous and rare enough to be free.
pub const PROGRESS_FILE_INTERVAL: usize = 64;

/// Which part of a code-graph call is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphPhase {
    /// Listing and reading source files.
    Crawl,
    /// Extracting symbols and references from the sources that were read.
    Parse,
    /// Ranking the graph and answering the query.
    Rank,
}

impl GraphPhase {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Crawl => "crawl",
            Self::Parse => "parse",
            Self::Rank => "rank",
        }
    }
}

/// One phase report. `files` is the count seen so far, and is a running total during `Crawl` and
/// the final total afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphProgress {
    pub phase: GraphPhase,
    pub files: usize,
}

#[async_trait]
pub trait GraphProgressSink: Send + Sync {
    async fn publish(&self, progress: GraphProgress);
}

/// The sync counterpart of [`GraphProgressSink`].
///
/// Ingest, resolution and ranking run on a blocking thread and cannot await, so the phases they
/// cross are reported through this and forwarded to the async sink by whoever spawned them.
pub type PhaseNotifier = dyn Fn(GraphPhase, usize) + Send + Sync;

/// Publishes when a sink is present, and is a no-op when it is not.
pub(crate) async fn report(sink: Option<&dyn GraphProgressSink>, phase: GraphPhase, files: usize) {
    if let Some(sink) = sink {
        sink.publish(GraphProgress { phase, files }).await;
    }
}
