//! Integration tests against real MCP servers, pinned in Docker images under
//! `tests/mcp-servers`. Every test is ignored; `make test-mcp-servers` builds the images, starts
//! Home Assistant, and runs them.

mod everything;
mod executor;
mod home_assistant;
mod support;
