//! Repository-scale symbol map, retrieval, and impact tools over one confined source tree.
//!
//! Five questions, one graph:
//!
//! | Tool | Answers |
//! | --- | --- |
//! | `code_map` | orient: what are the important symbols here |
//! | `code_context` | what should I read before making this change |
//! | `code_refs` | what references this, or what does this reference |
//! | `code_impact` | what breaks if I change this, and what tests cover it |
//! | `code_expand` | show me this symbol and what sits next to it |
//!
//! # What the graph is not
//!
//! Call edges are recovered from source text by name. Dynamic dispatch, callbacks, function
//! pointers, trait objects, reflection, and macro-generated call sites contribute no edge at all.
//! Every count is therefore a **floor**, every result says so in a field rather than only in prose,
//! and a zero means "none found", never "none exists".
//!
//! Nothing here sandboxes anything. Confinement comes from the filesystem group this crate is
//! constructed over, which is the only path resolver in the process.

pub mod bound;
pub mod catalog;
pub mod crawl;
#[cfg(feature = "mcp")]
pub mod dispatch;
pub mod engine;
pub mod error;
pub mod group;
pub mod limits;
pub mod model_text;
pub mod progress;
pub mod types;

pub use bound::{RAW_RESULT_CEILING_BYTES, Shrinkable, fit};
#[cfg(feature = "mcp")]
pub use catalog::catalog;
pub use catalog::specs;
pub use crawl::{CrawlSkip, CrawlTruncation, Crawled, crawl, crawl_filesystem_limits};
pub use engine::{CodeGraph, estimate_tokens};
pub use error::CodeGraphError;
pub use group::CodeGraphToolGroup;
pub use limits::CodeGraphLimits;
pub use model_text::ModelText;
pub use progress::{
    GraphPhase, GraphProgress, GraphProgressSink, PROGRESS_FILE_INTERVAL, PhaseNotifier,
};
pub use types::{
    CodeContextInput, CodeContextOutput, CodeExpandInput, CodeExpandOutput, CodeImpactInput,
    CodeImpactOutput, CodeMapInput, CodeMapOutput, CodeRefsInput, CodeRefsOutput, Direction,
    GraphSummary, RankedSymbol, ReachedSymbol, SelectorRefusal, SymbolRef,
};
pub use workcell_tool_contract::{ToolAnnotations, ToolSpec};
