//! MCP servers reached from Python run by the `code` tool.
//!
//! The model never sees an MCP tool definition. Inside a snippet the servers appear as namespaces
//! whose async methods are the servers' tools. What exists is announced in a `# MCP servers`
//! reminder rather than in the request prefix, so a server connecting, a tool list changing, or a
//! tool being used never invalidates the prompt cache.

pub mod catalog;
pub mod module;
pub mod naming;
pub mod search;
pub mod signature;
pub mod view;
