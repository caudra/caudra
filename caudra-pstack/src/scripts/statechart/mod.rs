//! A port of the statechart skill's `statechart.mjs`: it explores a TLA+ spec with tla-checker in
//! place of TLC and draws what the reader sees as Mermaid charts.

pub mod chart;
pub mod mapping;
pub mod mermaid;
pub mod model;
mod ordered;
mod regions;
mod story;
#[cfg(test)]
mod testing;
pub mod text;
