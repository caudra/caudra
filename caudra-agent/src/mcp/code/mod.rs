//! Code mode: MCP servers reached from Python run by the `mcp` tool.
//!
//! The model never sees an MCP tool definition. One fixed `mcp` tool takes a snippet, and the
//! servers appear inside it as namespaces whose async methods are the servers' tools. What exists
//! is announced in a `# MCP servers` reminder rather than in the request prefix, so a server
//! connecting, a tool list changing, or a tool being used never invalidates the prompt cache.
//!
//! Everything here is protocol-neutral about the script language: the gateway answers name
//! lookups and calls, and the runner is whatever executes the snippet.

pub mod catalog;
pub mod decide;
pub mod gateway;
pub mod naming;
pub mod output;
pub mod runner;
pub mod search;
pub mod signature;
pub mod view;
