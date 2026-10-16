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
    CodeContextOutput, CodeRefsOutput, GraphSummary, RankedSymbol, ReferenceRow, SelectorRefusal,
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
    if summary.declined_references > 0 {
        parts.push(format!("{} declined", summary.declined_references));
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

/// A reference row: the symbol line, its hop when more than one was walked, and its call sites.
fn reference_line(output: &CodeRefsOutput, row: &ReferenceRow) -> String {
    let mut line = symbol_line(&row.symbol);
    if output.depth > 1 {
        line = format!("hop {} {line}", row.hops);
    }
    let labels = output.site_labels(row);
    if !labels.is_empty() {
        line.push_str(&format!(" at={}", labels.join(",")));
    }
    if row.sites_withheld() > 0 {
        line.push_str(&format!(" +{} more", row.sites_withheld()));
    }
    if row.name_only {
        line.push_str(" ~name");
    }
    line
}

const SYMBOL_LEGEND: &str = "name kind path:lines in=referencing out=referenced (counts are floors: name-extracted edges \
     miss dynamic dispatch, callbacks and macros; 0 means none found)";
const SITES_LEGEND: &str = "at=call-site lines, path:line in another file";
const NAME_ONLY_LEGEND: &str = "~name=linked by name alone, the receiver's type was never seen";

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
        let walked = self.depth > 1;
        let mut lines = Vec::new();
        if self.matched.len() > 1 {
            lines.push(format!(
                "`{}` names {} definitions; this is their union",
                self.symbol,
                self.matched.len()
            ));
        }
        if walked {
            let hops: Vec<String> = self
                .by_depth
                .iter()
                .enumerate()
                .map(|(hop, count)| format!("hop {}: {count}", hop + 1))
                .collect();
            lines.push(format!(
                "{} symbols reach {} within {} hops ({}); {} of them are tests",
                self.total,
                self.symbol,
                self.depth,
                hops.join(", "),
                self.tests_reaching.unwrap_or_default()
            ));
            if self.tests_reaching == Some(0) && !self.references.is_empty() {
                lines.push(
                    "no test reaches this symbol within the walked depth, so the change has no \
                     existing coverage this map can see"
                        .to_owned(),
                );
            }
        } else {
            lines.push(format!(
                "{} of {}: {} found, each row one {}",
                self.direction, self.symbol, self.total, self.unit
            ));
        }
        if self.declined > 0 {
            lines.push(format!(
                "{} more calls spelled `{}` were not linked to it: their receiver or path names a \
                 type outside this tree, or one the calling file never mentions",
                self.declined, self.symbol
            ));
        }
        lines.push(SYMBOL_LEGEND.to_owned());
        if self.references.iter().any(|row| !row.sites.is_empty()) {
            lines.push(SITES_LEGEND.to_owned());
        }
        if self.references.iter().any(|row| row.name_only) {
            lines.push(NAME_ONLY_LEGEND.to_owned());
        }
        if self.references.is_empty() {
            lines.push(format!(
                "none found; this is a floor, not a proof that no {} exists",
                self.unit
            ));
        }
        lines.extend(self.references.iter().map(|row| reference_line(self, row)));
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
    use test_case::test_case;

    use super::*;
    use crate::types::{Direction, SiteRef, SymbolRef};

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

    fn context(results: Vec<RankedSymbol>, graph: GraphSummary) -> CodeContextOutput {
        CodeContextOutput {
            task: "a task".to_owned(),
            path: ".".to_owned(),
            shape: "conceptual".to_owned(),
            shape_reason: "prose".to_owned(),
            confidence: "low".to_owned(),
            margin_percent: 0,
            shown: results.len(),
            total_matched: results.len(),
            results,
            truncated: false,
            counts_floor: true,
            estimated_tokens: 0,
            graph,
        }
    }

    fn seed() -> SymbolRef {
        SymbolRef {
            name: "alpha".to_owned(),
            kind: "function".to_owned(),
            path: "src/a.rs".to_owned(),
            line_start: 1,
            line_end: 3,
        }
    }

    fn refs(references: Vec<ReferenceRow>, depth: usize) -> CodeRefsOutput {
        CodeRefsOutput {
            symbol: "alpha".to_owned(),
            direction: Direction::Callers.name(),
            unit: Direction::Callers.unit(),
            depth,
            matched: vec![seed()],
            shown: references.len(),
            total: references.len(),
            by_depth: if depth > 1 {
                vec![references.len()]
            } else {
                Vec::new()
            },
            tests_reaching: (depth > 1).then_some(0),
            references,
            declined: 0,
            truncated: false,
            counts_floor: true,
            estimated_tokens: 0,
            graph: summary(),
        }
    }

    fn row(
        name: &str,
        sites: &[(&str, usize)],
        sites_total: usize,
        name_only: bool,
    ) -> ReferenceRow {
        ReferenceRow {
            symbol: symbol(name),
            hops: 1,
            sites: sites
                .iter()
                .map(|&(path, line)| SiteRef {
                    path: path.to_owned(),
                    line,
                })
                .collect(),
            sites_total,
            name_only,
        }
    }

    #[test]
    fn the_legend_states_the_floor_once_rather_than_every_row() {
        let output = context(vec![symbol("alpha"), symbol("beta")], summary());
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
        let output = context(
            Vec::new(),
            GraphSummary {
                pr_converged: false,
                pr_iterations: 100,
                ..summary()
            },
        );
        assert!(
            output.model_text().contains("rank NOT converged"),
            "a truncated iteration produces a document that looks converged and is not"
        );
    }

    #[test]
    fn an_empty_reference_result_is_stated_as_a_floor_not_a_proof() {
        let text = refs(Vec::new(), 1).model_text().into_owned();
        assert!(text.contains("none found"));
        assert!(
            text.contains("not a proof"),
            "zero callers must not read as proof of no callers"
        );
    }

    #[test_case(&[("src/a.rs", 7), ("src/a.rs", 9)], 0, false, "in=2 out=1 at=7,9" ; "home_sites_are_bare_lines")]
    #[test_case(&[("src/b.rs", 4)], 0, false, "at=src/b.rs:4" ; "a_foreign_site_carries_its_path")]
    #[test_case(&[("src/a.rs", 7)], 20, false, "at=7 +19 more" ; "a_capped_row_says_how_many_more")]
    #[test_case(&[("src/a.rs", 7)], 0, true, "at=7 ~name" ; "a_name_only_link_is_marked")]
    fn reference_rows_carry_their_call_sites(
        sites: &[(&str, usize)],
        sites_total: usize,
        name_only: bool,
        expected: &str,
    ) {
        let text = refs(vec![row("beta", sites, sites_total, name_only)], 1)
            .model_text()
            .into_owned();
        assert!(text.contains(expected), "{text}");
    }

    #[test]
    fn a_walk_reports_hops_and_test_coverage() {
        let text = refs(vec![row("beta", &[], 0, false)], 3)
            .model_text()
            .into_owned();
        assert!(
            text.contains("1 symbols reach alpha within 3 hops (hop 1: 1); 0 of them are tests")
        );
        assert!(text.contains("no existing coverage"));
        assert!(text.contains("hop 1 beta function"));
    }

    #[test]
    fn declined_calls_are_disclosed() {
        let mut output = refs(Vec::new(), 1);
        output.declined = 12;
        assert!(
            output
                .model_text()
                .contains("12 more calls spelled `alpha`")
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
        assert!(
            context(Vec::new(), summary())
                .model_text()
                .contains("nothing is returned rather than"),
            "an empty retrieval must not look like a failure to run"
        );
    }
}
