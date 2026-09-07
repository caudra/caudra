//! A symbol graph over a source tree: extraction, resolution, ranking, and retrieval.
//!
//! The pipeline has four stages and no back edges. Each stage's output is plain data, so any stage
//! can be exercised on a hand-built input rather than a real repository.
//!
//! ```text
//! extract  ->  resolve  ->  rank  ->  report
//! ```
//!
//! Nothing here touches the filesystem or a protocol. A caller supplies already-read, already-
//! authorized sources; path resolution and confinement belong to the filesystem crate, and this one
//! must never grow a second resolver.
//!
//! # What the graph is not
//!
//! The call graph is extracted from source text by name. Dynamic dispatch, callbacks, function
//! pointers, macro-generated call sites, and declarations that parse without a call expression
//! contribute no edge. A count of callers is therefore a **floor**, never a total, and every
//! surface that reports one says so. A zero means "none found", never "none exists".

pub mod extract;
pub mod ingest;
pub mod model;
pub mod resolve;

pub use extract::{ExtractLimits, FileFacts};
pub use ingest::{IngestLimits, IngestTruncation, Ingested, SourceInput, ingest};
pub use model::{
    ByteSpan, Definition, Facts, FileId, LineSpan, Metrics, NodeId, Reference, SkipReason,
    SkippedFile, SourceFile,
};
pub use resolve::{Edge, Graph, ResolutionDiagnostics, ResolutionTier, resolve};
pub use workcell_source_languages::{Language, LanguageFamily, ReferenceKind, SymbolKind};
