//! The result size ceiling, shared by every host.
//!
//! Shortening keeps a result usable and honestly marked. Failing the call instead would spend the
//! whole crawl and then return nothing.
//!
//! The ceiling is a property of the transport and the consumer rather than of any one tool, so it
//! lives here rather than in the MCP adapter: a native host embedding this crate has the same
//! consumer and needs the same bound. Size is measured against a full JSON-RPC envelope, which
//! overstates a native host's framing by a constant. That is the safe direction to be wrong in, and
//! it keeps one number rather than one per host.

use serde::Serialize;
use serde_json::Value;

use crate::{
    model_text::ModelText,
    types::{CodeContextOutput, CodeExpandOutput, CodeImpactOutput, CodeMapOutput, CodeRefsOutput},
};

/// Largest raw result any host will emit, in bytes.
///
/// Matches the filesystem group's ceiling.
pub const RAW_RESULT_CEILING_BYTES: usize = 64_000;
/// Newline framing added by the stdio transport.
const TRANSPORT_FRAME_DELIMITER_BYTES: usize = 1;
/// The largest JSON-RPC id worth budgeting for. Conservative against incrementing integer ids.
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// A result that can be shortened by dropping trailing rows.
pub trait Shrinkable {
    fn rows(&self) -> usize;
    fn retain_prefix(&mut self, retained: usize);
}

impl Shrinkable for CodeMapOutput {
    fn rows(&self) -> usize {
        self.symbols.len()
    }
    fn retain_prefix(&mut self, retained: usize) {
        self.symbols.truncate(retained);
        self.shown = self.symbols.len();
        self.truncated = self.total > self.shown;
    }
}

impl Shrinkable for CodeContextOutput {
    fn rows(&self) -> usize {
        self.results.len()
    }
    fn retain_prefix(&mut self, retained: usize) {
        self.results.truncate(retained);
        self.shown = self.results.len();
        self.truncated = self.total_matched > self.shown;
    }
}

impl Shrinkable for CodeRefsOutput {
    fn rows(&self) -> usize {
        self.references.len()
    }
    fn retain_prefix(&mut self, retained: usize) {
        self.references.truncate(retained);
        self.shown = self.references.len();
        self.truncated = self.total > self.shown;
    }
}

impl Shrinkable for CodeImpactOutput {
    fn rows(&self) -> usize {
        self.reached.len()
    }
    fn retain_prefix(&mut self, retained: usize) {
        self.reached.truncate(retained);
        self.tests_reaching.retain(|row| {
            self.reached.iter().any(|kept| {
                kept.symbol.path == row.symbol.path
                    && kept.symbol.line_start == row.symbol.line_start
            })
        });
        self.shown = self.reached.len();
        self.truncated = self.total > self.shown;
    }
}

impl Shrinkable for CodeExpandOutput {
    fn rows(&self) -> usize {
        // The body is the payload and cannot be dropped row-wise, so the neighbourhood is what
        // gives. A caller asked for this symbol; returning it without its callers is a smaller
        // answer than returning its callers without it.
        self.callers.len() + self.callees.len()
    }
    fn retain_prefix(&mut self, retained: usize) {
        let callers = retained.min(self.callers.len());
        self.callers.truncate(callers);
        self.callees.truncate(retained.saturating_sub(callers));
        self.truncated = true;
    }
}

/// Shortens `output` until the whole response envelope fits the ceiling.
///
/// Measured against the serialized envelope, not the payload, because the ceiling is a transport
/// property: a payload that fits and an envelope that does not is still a call that fails.
#[must_use]
pub fn fit<T: Serialize + ModelText + Clone + Shrinkable>(output: T) -> T {
    if response_size(&output).is_ok_and(|size| size <= RAW_RESULT_CEILING_BYTES) {
        return output;
    }
    // Binary search the largest retained prefix that fits.
    let mut lower = 0_usize;
    let mut upper = output.rows();
    while lower < upper {
        let candidate = lower + (upper - lower).div_ceil(2);
        let mut probe = output.clone();
        probe.retain_prefix(candidate);
        if response_size(&probe).is_ok_and(|size| size <= RAW_RESULT_CEILING_BYTES) {
            lower = candidate;
        } else {
            upper = candidate - 1;
        }
    }
    let mut fitted = output;
    fitted.retain_prefix(lower);
    fitted
}

#[derive(Serialize)]
struct Envelope<'a> {
    jsonrpc: &'static str,
    id: u64,
    result: Rendered<'a>,
}

#[derive(Serialize)]
struct Rendered<'a> {
    #[serde(rename = "type")]
    result_type: &'static str,
    content: [TextBlock<'a>; 1],
    #[serde(rename = "structuredContent")]
    structured_content: &'a Value,
}

#[derive(Serialize)]
struct TextBlock<'a> {
    #[serde(rename = "type")]
    content_type: &'static str,
    text: &'a str,
}

fn response_size(output: &(impl Serialize + ModelText)) -> Result<usize, serde_json::Error> {
    let structured = serde_json::to_value(output)?;
    let text = output.model_text();
    let envelope = Envelope {
        jsonrpc: "2.0",
        id: MAX_SAFE_INTEGER,
        result: Rendered {
            result_type: "complete",
            content: [TextBlock {
                content_type: "text",
                text: &text,
            }],
            structured_content: &structured,
        },
    };
    Ok(serde_json::to_vec(&envelope)?
        .len()
        .saturating_add(TRANSPORT_FRAME_DELIMITER_BYTES))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{GraphSummary, RankedSymbol};

    fn wide_map(rows: usize) -> CodeMapOutput {
        let symbols: Vec<RankedSymbol> = (0..rows)
            .map(|index| RankedSymbol {
                name: format!("a_symbol_with_a_reasonably_long_name_{index}"),
                kind: "function".to_owned(),
                path: format!("src/some/nested/directory/module_{index}.rs"),
                line_start: index,
                line_end: index + 20,
                rank: 0.001,
                callers: 3,
                calls: 2,
                test_scope: false,
            })
            .collect();
        CodeMapOutput {
            path: ".".to_owned(),
            shown: symbols.len(),
            total: symbols.len(),
            truncated: false,
            symbols,
            counts_floor: true,
            estimated_tokens: 0,
            graph: GraphSummary::default(),
        }
    }

    #[test]
    fn an_oversized_result_is_shortened_to_fit_the_envelope() {
        let output = wide_map(4_000);
        assert!(
            response_size(&output).expect("size") > RAW_RESULT_CEILING_BYTES,
            "the fixture must actually exceed the ceiling"
        );

        let fitted = fit(output);
        let size = response_size(&fitted).expect("size");
        assert!(
            size <= RAW_RESULT_CEILING_BYTES,
            "fitted response is {size} bytes"
        );
        assert!(fitted.shown > 0, "shortening must not empty the result");
        assert!(
            fitted.truncated,
            "a shortened result must say it was shortened"
        );
        assert_eq!(
            fitted.total, 4_000,
            "the total must survive: it is what tells the caller what was withheld"
        );
    }

    #[test]
    fn a_result_that_already_fits_is_returned_unchanged() {
        let output = wide_map(3);
        let fitted = fit(output.clone());
        assert_eq!(fitted.shown, output.shown);
        assert!(!fitted.truncated);
    }
}
