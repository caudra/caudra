//! Mermaid flowchart rendering.
//!
//! Terminals cannot run mermaid, so flowcharts are laid out here and drawn
//! with box-drawing characters. Only `flowchart`/`graph` is understood;
//! every other diagram family, and any syntax outside the supported grammar,
//! returns an error so the caller falls back to a plain code block.

pub mod layout;
pub mod paint;
pub mod parse;

pub use paint::{CONTINUATION, Canvas, Cell, Role};
pub use parse::{Direction, Edge, EdgeStyle, Graph, Node, ParseError, Shape, Subgraph, parse};

/// Lays out and draws a flowchart, or returns why the source was refused so
/// the caller can fall back to rendering it as a code block.
pub fn render(source: &str) -> Result<Canvas, ParseError> {
    let graph = parse(source)?;
    let placed = layout::layout(&graph);
    Ok(paint::paint(&graph, &placed))
}
