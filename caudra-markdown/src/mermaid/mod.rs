//! Mermaid flowchart and state diagram rendering.
//!
//! Terminals cannot run mermaid, so diagrams are laid out here and drawn with
//! box-drawing characters. `flowchart`/`graph` is understood directly, and
//! `stateDiagram`/`stateDiagram-v2` is translated into the same graph so it
//! shares the layout and the drawing. Every other diagram family, and any
//! syntax outside the supported grammar, returns an error so the caller falls
//! back to a plain code block.

pub mod layout;
pub mod paint;
pub mod parse;
mod state;

pub use paint::{CONTINUATION, Canvas, Cell, Role};
pub use parse::{Direction, Edge, EdgeStyle, Graph, Node, ParseError, Shape, Subgraph, parse};

/// Lays out and draws a diagram, or returns why the source was refused so
/// the caller can fall back to rendering it as a code block.
pub fn render(source: &str) -> Result<Canvas, ParseError> {
    let graph = parse(source)?;
    let placed = layout::layout(&graph);
    Ok(paint::paint(&graph, &placed))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;

    const STATECHARTS: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../caudra-pstack/testdata/statechart"
    );
    const CHART_EXTENSION: &str = "mmd";
    const DECLARATION: &str = "state \"";
    const TRANSITION: &str = " --> ";
    const TRANSITION_LABEL: &str = " : ";

    fn charts(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("statechart fixtures are readable") {
            let path = entry.expect("directory entry").path();
            match path.is_dir() {
                true => charts(&path, found),
                false if path.extension().is_some_and(|ext| ext == CHART_EXTENSION) => {
                    found.push(path)
                }
                false => {}
            }
        }
    }

    /// Every line of every `state "Label" as id`, and every transition label
    /// as the one line it is drawn on.
    fn expected_text(source: &str) -> Vec<String> {
        let states = source
            .split(DECLARATION)
            .skip(1)
            .filter_map(|rest| rest.split_once(parse::QUOTE))
            .flat_map(|(label, _)| parse::split_label(label));
        let transitions = source
            .lines()
            .filter_map(|line| line.split_once(TRANSITION))
            .filter_map(|(_, target)| target.split_once(TRANSITION_LABEL))
            .map(|(_, label)| parse::split_label(label).join(" "));
        states.chain(transitions).collect()
    }

    #[test]
    fn every_pstack_statechart_draws_its_states_and_transitions() {
        let mut found = Vec::new();
        charts(Path::new(STATECHARTS), &mut found);
        found.sort();
        assert!(!found.is_empty(), "no charts under {STATECHARTS}");
        for path in found {
            let source = fs::read_to_string(&path).expect("chart is readable");
            let art = render(&source)
                .unwrap_or_else(|error| panic!("{} falls back: {error}", path.display()))
                .to_text();
            for text in expected_text(&source) {
                assert!(
                    art.contains(&text),
                    "{} lost {text:?}:\n{art}",
                    path.display()
                );
            }
        }
    }
}
