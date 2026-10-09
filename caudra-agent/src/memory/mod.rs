//! Project memory: an append-only journal of notes, a tree of one-line
//! summaries a Fast model builds over it in the background, and the
//! fixed-size view of that tree the system prompt carries.

pub mod baseline;
pub mod compactor;
pub mod search;
pub mod snapshot;
pub mod store;
pub mod tree;
