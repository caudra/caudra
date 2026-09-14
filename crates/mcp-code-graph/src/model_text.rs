//! Model-facing renderings.
//!
//! A tool result carries two forms. The structured record is what a program consumes; the content
//! block is what a model reads. Serializing the record into the block would make every result carry
//! its payload twice, so each output renders the one view a reader needs and nothing is repeated
//! between them.
//!
//! The text is deliberately dense: one legend line, then one line per row. A model reading a
//! hundred symbols should spend its budget on names and paths, not on repeated field labels.
//!
//! Every count rendered here is a floor, and the legend says so once rather than every row saying
//! it. That is the only place the honesty vocabulary is compressed — in the structured record it
//! stays a field per result.

use std::borrow::Cow;

use crate::types::{
    CodeContextOutput, CodeExpandOutput, CodeImpactOutput, CodeMapOutput, CodeRefsOutput,
    GraphSummary, RankedSymbol, ReachedSymbol, SelectorRefusal,
};

/// Renders the content block for a tool result.
pub trait ModelText {
    fn model_text(&self) -> Cow<'_, str>;
}

/// One trailing line describing the run that produced the rows above it.
fn footer(summary: &GraphSummary) -> String {
    let mut parts = vec![format!(
        "{} files, {} symbols, {} edges",
        summary.files_indexed, summary.symbols, summary.edges
    )];
    if summary.unresolved_references > 0 {
        parts.push(format!("{} unresolved", summary.unresolved_references));
    }
    if summary.ambiguous_references > 0 {
        parts.push(format!("{} ambiguous", summary.ambiguous_references));
    }
    if summary.files_skipped > 0 {
        parts.push(format!("{} skipped", summary.files_skipped));
    }
    if !summary.pr_converged {
        // A rank vector caught mid-descent orders the same way a converged one does and is not the
        // same claim. Saying so costs one clause.
        parts.push(format!(
            "rank NOT converged after {} iterations",
            summary.pr_iterations
        ));
    }
    if !summary.scan_complete {
        parts.push("scan stopped early".to_owned());
    }
    if summary.files_ignored > 0 {
        parts.push(format!("{} ignored", summary.files_ignored));
    }
    if !summary.pruned_repositories.is_empty() {
        // The reader needs the names, not a count: the next call is `path=<one of them>`, and a
        // count alone would leave a map that omits a whole subtree looking complete.
        parts.push(format!(
            "stopped at nested repositories {} (pass one as `path` to map it)",
            summary.pruned_repositories.join(", ")
        ));
    }
    if !summary.truncated_by.is_empty() {
        parts.push(format!("truncated by {}", summary.truncated_by.join(", ")));
    }
    format!("[{}]", parts.join("; "))
}

/// `name  kind  path:start-end  callers/calls`, one per line.
fn symbol_line(symbol: &RankedSymbol) -> String {
    let test = if symbol.test_scope { " [test]" } else { "" };
    format!(
        "{} {} {}:{}-{} in={} out={}{}",
        symbol.name,
        symbol.kind,
        symbol.path,
        symbol.line_start,
        symbol.line_end,
        symbol.callers,
        symbol.calls,
        test
    )
}

/// Hop count leads the line so a reader can see the shape of the blast radius by scanning one
/// column rather than parsing each row.
fn reached_line(row: &ReachedSymbol) -> String {
    let test = if row.test_scope { " [test]" } else { "" };
    format!(
        "hop {} {} {} {}:{}-{}{}",
        row.hops,
        row.symbol.name,
        row.symbol.kind,
        row.symbol.path,
        row.symbol.line_start,
        row.symbol.line_end,
        test
    )
}

const SYMBOL_LEGEND: &str = "name kind path:lines in=referencing out=referenced (counts are floors: name-extracted edges \
     miss dynamic dispatch, callbacks and macros; 0 means none found)";

impl ModelText for CodeMapOutput {
    fn model_text(&self) -> Cow<'_, str> {
        let mut lines = vec![SYMBOL_LEGEND.to_owned()];
        lines.extend(self.symbols.iter().map(symbol_line));
        if self.truncated {
            lines.push(format!(
                "[truncated: showing {} of {} ranked symbols]",
                self.shown, self.total
            ));
        }
        lines.push(footer(&self.graph));
        Cow::Owned(lines.join("\n"))
    }
}

impl ModelText for CodeContextOutput {
    fn model_text(&self) -> Cow<'_, str> {
        let mut lines = vec![
            format!(
                "read as {} ({}); confidence {} at {}% separation",
                self.shape, self.shape_reason, self.confidence, self.margin_percent
            ),
            SYMBOL_LEGEND.to_owned(),
        ];
        if self.results.is_empty() {
            lines.push(
                "no symbol matched this task; nothing is returned rather than the repository's \
                 busiest symbols, which would answer a question that was not asked"
                    .to_owned(),
            );
        }
        lines.extend(self.results.iter().map(symbol_line));
        if self.truncated {
            lines.push(format!(
                "[truncated: showing {} of {} matching symbols]",
                self.shown, self.total_matched
            ));
        }
        lines.push(footer(&self.graph));
        Cow::Owned(lines.join("\n"))
    }
}

impl ModelText for CodeRefsOutput {
    fn model_text(&self) -> Cow<'_, str> {
        let mut lines = Vec::new();
        if self.matched.len() > 1 {
            lines.push(format!(
                "`{}` names {} definitions; this is their union",
                self.symbol,
                self.matched.len()
            ));
        }
        lines.push(format!(
            "{} of {}: {} found, each row one {}",
            self.direction, self.symbol, self.total, self.unit
        ));
        lines.push(SYMBOL_LEGEND.to_owned());
        if self.references.is_empty() {
            lines.push(format!(
                "none found; this is a floor, not a proof that no {} exists",
                self.unit
            ));
        }
        lines.extend(self.references.iter().map(symbol_line));
        if self.truncated {
            lines.push(format!(
                "[truncated: showing {} of {}]",
                self.shown, self.total
            ));
        }
        lines.push(footer(&self.graph));
        Cow::Owned(lines.join("\n"))
    }
}

impl ModelText for CodeImpactOutput {
    fn model_text(&self) -> Cow<'_, str> {
        let mut lines = vec![format!(
            "{} symbols reach {} within {} hops; {} of them are tests",
            self.total,
            self.symbol,
            self.depth,
            self.tests_reaching.len()
        )];
        if self.tests_reaching.is_empty() && !self.reached.is_empty() {
            lines.push(
                "no test reaches this symbol within the walked depth, so the change has no \
                 existing coverage this map can see"
                    .to_owned(),
            );
        }
        lines.push(
            "hop N name kind path:lines (reach is a floor: dynamic dispatch and macros contribute \
             no edge)"
                .to_owned(),
        );
        lines.extend(self.reached.iter().map(reached_line));
        if self.truncated {
            lines.push(format!(
                "[truncated: showing {} of {}]",
                self.shown, self.total
            ));
        }
        lines.push(footer(&self.graph));
        Cow::Owned(lines.join("\n"))
    }
}

impl ModelText for CodeExpandOutput {
    fn model_text(&self) -> Cow<'_, str> {
        let mut lines = vec![format!(
            "{} {} {}:{}-{}",
            self.symbol, self.kind, self.path, self.line_start, self.line_end
        )];
        if let Some(reason) = &self.served_whole_file {
            lines.push(format!("[whole file: {reason}]"));
        }
        lines.push(self.source.clone());
        if self.truncated {
            lines.push("[truncated: the body exceeded the expansion ceiling]".to_owned());
        }
        if !self.callers.is_empty() {
            lines.push(format!(
                "callers ({}, a floor): {}",
                self.callers.len(),
                self.callers
                    .iter()
                    .map(|symbol| format!("{}@{}:{}", symbol.name, symbol.path, symbol.line_start))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.callees.is_empty() {
            lines.push(format!(
                "calls ({}, a floor): {}",
                self.callees.len(),
                self.callees
                    .iter()
                    .map(|symbol| format!("{}@{}:{}", symbol.name, symbol.path, symbol.line_start))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        lines.push(footer(&self.graph));
        Cow::Owned(lines.join("\n"))
    }
}

impl ModelText for SelectorRefusal {
    fn model_text(&self) -> Cow<'_, str> {
        let mut text = self.reason.clone();
        if !self.did_you_mean.is_empty() {
            text.push_str(&format!("\ndid you mean: {}", self.did_you_mean.join(", ")));
        }
        text.push_str(&format!(
            "\n[{} symbols indexed; this is a refusal, not a count of zero]",
            self.symbols_known
        ));
        Cow::Owned(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SymbolRef;

    fn summary() -> GraphSummary {
        GraphSummary {
            files_indexed: 2,
            symbols: 4,
            edges: 3,
            pr_converged: true,
            scan_complete: true,
            ..GraphSummary::default()
        }
    }

    fn symbol(name: &str) -> RankedSymbol {
        RankedSymbol {
            name: name.to_owned(),
            kind: "function".to_owned(),
            path: "src/a.rs".to_owned(),
            line_start: 1,
            line_end: 3,
            rank: 0.5,
            callers: 2,
            calls: 1,
            test_scope: false,
        }
    }

    #[test]
    fn the_legend_states_the_floor_once_rather_than_every_row() {
        let output = CodeMapOutput {
            path: ".".to_owned(),
            symbols: vec![symbol("alpha"), symbol("beta")],
            shown: 2,
            total: 2,
            truncated: false,
            counts_floor: true,
            estimated_tokens: 0,
            graph: summary(),
        };
        let text = output.model_text();
        assert_eq!(
            text.matches("floors").count(),
            1,
            "the caveat belongs in the legend, not on every line"
        );
        assert!(text.contains("alpha function src/a.rs:1-3 in=2 out=1"));
    }

    #[test]
    fn an_unconverged_ranking_says_so_in_the_footer() {
        let output = CodeMapOutput {
            path: ".".to_owned(),
            symbols: Vec::new(),
            shown: 0,
            total: 0,
            truncated: false,
            counts_floor: true,
            estimated_tokens: 0,
            graph: GraphSummary {
                pr_converged: false,
                pr_iterations: 100,
                ..summary()
            },
        };
        assert!(
            output.model_text().contains("rank NOT converged"),
            "a truncated iteration produces a document that looks converged and is not"
        );
    }

    #[test]
    fn an_empty_reference_result_is_stated_as_a_floor_not_a_proof() {
        let output = CodeRefsOutput {
            symbol: "alpha".to_owned(),
            direction: "callers",
            unit: "referencing symbol",
            matched: vec![SymbolRef {
                name: "alpha".to_owned(),
                kind: "function".to_owned(),
                path: "src/a.rs".to_owned(),
                line_start: 1,
                line_end: 3,
            }],
            references: Vec::new(),
            shown: 0,
            total: 0,
            truncated: false,
            counts_floor: true,
            estimated_tokens: 0,
            graph: summary(),
        };
        let text = output.model_text();
        assert!(text.contains("none found"));
        assert!(
            text.contains("not a proof"),
            "zero callers must not read as proof of no callers"
        );
    }

    #[test]
    fn a_refusal_is_distinguishable_from_a_zero_result() {
        let refusal = SelectorRefusal::new("alhpa".to_owned(), vec!["alpha".to_owned()], 42);
        let text = refusal.model_text();
        assert!(text.contains("did you mean: alpha"));
        assert!(text.contains("refusal, not a count of zero"));
    }

    #[test]
    fn an_empty_context_result_explains_why_it_is_empty() {
        let output = CodeContextOutput {
            task: "something unrelated".to_owned(),
            path: ".".to_owned(),
            shape: "conceptual".to_owned(),
            shape_reason: "prose".to_owned(),
            confidence: "low".to_owned(),
            margin_percent: 0,
            results: Vec::new(),
            shown: 0,
            total_matched: 0,
            truncated: false,
            counts_floor: true,
            estimated_tokens: 0,
            graph: summary(),
        };
        assert!(
            output
                .model_text()
                .contains("nothing is returned rather than"),
            "an empty retrieval must not look like a failure to run"
        );
    }
}
