//! A port of the statechart skill's `statechart.mjs`: it explores a TLA+ spec with tla-checker in
//! place of TLC and draws what the reader sees as Mermaid charts. [`cli`] is its command line.

pub mod chart;
mod check;
pub mod cli;
pub mod from_chart;
pub mod mapping;
pub mod mermaid;
pub mod model;
mod ordered;
mod publish;
mod regions;
pub mod second;
mod story;
#[cfg(test)]
mod testing;
pub mod text;
pub mod uncited;
