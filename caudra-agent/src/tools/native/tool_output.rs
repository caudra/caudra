//! `tool_output_read` / `tool_output_grep`: paging over output too large to
//! inline.
//!
//! When a tool's result exceeds the session's line budget it is spilled to the
//! `ToolOutputStore` and replaced by a truncation notice carrying an opaque
//! id. These two tools are how the model gets the rest back. Both are
//! session-scoped: an id from another session reads as if it does not exist.

use std::borrow::Cow;
use std::sync::Arc;

use serde_json::Value;

use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolExecResult, ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{DescriptionContext, ToolAudience, ToolContext};
use crate::types::ToolOutput;
use caudra_providers::{estimate_tokens, token_label};
use caudra_storage::id::CaudraId;
use caudra_storage::tool_outputs::{
    ToolOutputGrepResult, ToolOutputId, ToolOutputReadResult, ToolOutputStore,
};

pub const READ_DESCRIPTION: &str =
    "Read a page of managed tool output owned by the current session.";
pub const GREP_DESCRIPTION: &str =
    "Search managed tool output owned by the current session using a regex.";

const SESSION_REQUIRED: &str = "tool output retrieval requires a session";
const STORE_UNAVAILABLE: &str = "tool output store is unavailable";

const DEFAULT_READ_OFFSET: usize = 1;
const DEFAULT_READ_BYTE_OFFSET: usize = 0;
const DEFAULT_READ_LIMIT: usize = 200;
const DEFAULT_GREP_OFFSET: usize = 1;
const DEFAULT_GREP_LIMIT: usize = 100;
const DEFAULT_CONTEXT: usize = 0;
const MAX_READ_LIMIT: usize = 2000;
const MAX_GREP_LIMIT: usize = 200;
const MAX_CONTEXT: usize = 5;

/// The page this tool returns is itself subject to the session's output
/// budget. Formatting shrinks until it fits so a read can never be truncated
/// into a second, unreadable notice.
const MAX_OUTPUT_BYTES: usize = 50 * 1024;
const MAX_OUTPUT_LINES: usize = 2000;

static OUTPUT_ID_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Opaque ID from a tool-output truncation notice.",
};
static READ_OFFSET_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Integer,
    description: "Starting line, 1-indexed (default: 1).",
};
static BYTE_OFFSET_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Integer,
    description: "Starting byte within the first line (default: 0; use continuation hints).",
};
static READ_LIMIT_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Integer,
    description: "Maximum lines to return (default: 200; capped at 2000).",
};
static READ_PROPERTIES: &[Property] = &[
    ("output_id", &OUTPUT_ID_PARAM, true, &[]),
    ("offset", &READ_OFFSET_PARAM, false, &[]),
    ("byte_offset", &BYTE_OFFSET_PARAM, false, &[]),
    ("limit", &READ_LIMIT_PARAM, false, &[]),
];
static READ_SCHEMA: ParamSchema = ParamSchema::Object {
    properties: READ_PROPERTIES,
    description: "",
    reject_unknown: false,
};

static PATTERN_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Regex pattern.",
};
static GREP_OFFSET_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Integer,
    description: "Starting line, 1-indexed (default: 1).",
};
static GREP_LIMIT_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Integer,
    description: "Maximum matches to return (default: 100; capped at 200).",
};
static CONTEXT_BEFORE_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Integer,
    description: "Context lines before each match (default: 0; capped at 5).",
};
static CONTEXT_AFTER_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Integer,
    description: "Context lines after each match (default: 0; capped at 5).",
};
static GREP_PROPERTIES: &[Property] = &[
    ("output_id", &OUTPUT_ID_PARAM, true, &[]),
    ("pattern", &PATTERN_PARAM, true, &[]),
    ("offset", &GREP_OFFSET_PARAM, false, &[]),
    ("limit", &GREP_LIMIT_PARAM, false, &[]),
    ("context_before", &CONTEXT_BEFORE_PARAM, false, &[]),
    ("context_after", &CONTEXT_AFTER_PARAM, false, &[]),
];
static GREP_SCHEMA: ParamSchema = ParamSchema::Object {
    properties: GREP_PROPERTIES,
    description: "",
    reject_unknown: false,
};

pub struct ToolOutputRead;

impl Tool for ToolOutputRead {
    fn name(&self) -> &str {
        crate::tools::TOOL_OUTPUT_READ_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(READ_DESCRIPTION)
    }

    fn schema(&self) -> Value {
        to_json_schema(&READ_SCHEMA)
    }

    fn audience(&self) -> ToolAudience {
        ToolAudience::all()
    }

    fn tool_kind(&self) -> Option<&str> {
        Some("read")
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let input = validate(&READ_SCHEMA, input.clone())?;
        Ok(Box::new(ReadCall {
            output_id: required_str(&input, "output_id")?,
            offset: usize_field(&input, "offset").unwrap_or(DEFAULT_READ_OFFSET),
            byte_offset: usize_field(&input, "byte_offset").unwrap_or(DEFAULT_READ_BYTE_OFFSET),
            limit: usize_field(&input, "limit")
                .unwrap_or(DEFAULT_READ_LIMIT)
                .min(MAX_READ_LIMIT),
        }))
    }
}

struct ReadCall {
    output_id: String,
    offset: usize,
    byte_offset: usize,
    limit: usize,
}

impl ToolInvocation for ReadCall {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(self.output_id.clone()))
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            match self.run(ctx).await {
                Ok(text) => ToolExecResult::from(Ok(ToolOutput::Plain(text.into()))),
                Err(error) => ToolExecResult::from(Err(format!("error: {error}"))),
            }
        })
    }
}

impl ReadCall {
    async fn run(&self, ctx: &ToolContext) -> Result<String, String> {
        let (session, store) = access(ctx)?;
        let id = parse_id(&self.output_id)?;
        let (offset, limit) = (
            positive(self.offset, "offset")?,
            positive(self.limit, "limit")?,
        );
        let byte_offset = self.byte_offset;
        let result =
            smol::unblock(move || store.read_at(session, id, offset, limit, byte_offset)).await;
        let result = result.map_err(|e| e.to_string())?;
        Ok(format_read(&self.output_id, &result, self.limit))
    }
}

pub struct ToolOutputGrep;

impl Tool for ToolOutputGrep {
    fn name(&self) -> &str {
        crate::tools::TOOL_OUTPUT_GREP_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(GREP_DESCRIPTION)
    }

    fn schema(&self) -> Value {
        to_json_schema(&GREP_SCHEMA)
    }

    fn audience(&self) -> ToolAudience {
        ToolAudience::all()
    }

    fn tool_kind(&self) -> Option<&str> {
        Some("search")
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let input = validate(&GREP_SCHEMA, input.clone())?;
        Ok(Box::new(GrepCall {
            output_id: required_str(&input, "output_id")?,
            pattern: required_str(&input, "pattern")?,
            offset: usize_field(&input, "offset").unwrap_or(DEFAULT_GREP_OFFSET),
            limit: usize_field(&input, "limit")
                .unwrap_or(DEFAULT_GREP_LIMIT)
                .min(MAX_GREP_LIMIT),
            context_before: usize_field(&input, "context_before")
                .unwrap_or(DEFAULT_CONTEXT)
                .min(MAX_CONTEXT),
            context_after: usize_field(&input, "context_after")
                .unwrap_or(DEFAULT_CONTEXT)
                .min(MAX_CONTEXT),
        }))
    }
}

struct GrepCall {
    output_id: String,
    pattern: String,
    offset: usize,
    limit: usize,
    context_before: usize,
    context_after: usize,
}

impl ToolInvocation for GrepCall {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(self.pattern.clone()))
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            match self.run(ctx).await {
                Ok(text) => ToolExecResult::from(Ok(ToolOutput::Plain(text.into()))),
                Err(error) => ToolExecResult::from(Err(format!("error: {error}"))),
            }
        })
    }
}

impl GrepCall {
    async fn run(&self, ctx: &ToolContext) -> Result<String, String> {
        let (session, store) = access(ctx)?;
        let id = parse_id(&self.output_id)?;
        let (offset, limit) = (
            positive(self.offset, "offset")?,
            positive(self.limit, "limit")?,
        );
        let (pattern, before, after) = (
            self.pattern.clone(),
            self.context_before,
            self.context_after,
        );
        let result =
            smol::unblock(move || store.grep(session, id, &pattern, offset, limit, before, after))
                .await;
        let result = result.map_err(|e| e.to_string())?;
        Ok(format_grep(self, &result))
    }
}

fn access(ctx: &ToolContext) -> Result<(CaudraId, Arc<ToolOutputStore>), String> {
    let session = ctx.session_id.as_ref().ok_or(SESSION_REQUIRED)?;
    let store = ctx.tool_output_store.as_ref().ok_or(STORE_UNAVAILABLE)?;
    Ok((session.id(), Arc::clone(store)))
}

fn parse_id(raw: &str) -> Result<ToolOutputId, String> {
    raw.parse::<ToolOutputId>()
        .map_err(|e| format!("invalid tool output ID: {e}"))
}

fn positive(value: usize, name: &str) -> Result<usize, String> {
    (value > 0)
        .then_some(value)
        .ok_or_else(|| format!("{name} must be at least 1"))
}

fn required_str(input: &Value, key: &str) -> Result<String, ParseError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ParseError::custom(format!("{key} is required")))
}

/// Absent and malformed both fall back to the default: validation has already
/// rejected the shapes the model could still fix.
fn usize_field(input: &Value, key: &str) -> Option<usize> {
    input.get(key)?.as_u64().map(|v| v as usize)
}

fn fits(output: &str) -> bool {
    output.len() <= MAX_OUTPUT_BYTES && line_count(output) <= MAX_OUTPUT_LINES
}

fn line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.matches('\n').count() + 1
    }
}

fn read_hint(output_id: &str, offset: usize, byte_offset: usize, limit: usize) -> String {
    if byte_offset > 0 {
        format!(
            "Next call: tool_output_read(output_id={output_id:?}, offset={offset}, byte_offset={byte_offset}, limit={limit})"
        )
    } else {
        format!(
            "Next call: tool_output_read(output_id={output_id:?}, offset={offset}, limit={limit})"
        )
    }
}

/// Drops trailing lines until the rendered page fits the session budget,
/// recomputing the continuation hint each time so it always points at the
/// first line actually withheld.
fn format_read(output_id: &str, result: &ToolOutputReadResult, limit: usize) -> String {
    let mut lines: Vec<&str> = if result.returned_lines > 0 {
        result
            .text
            .split('\n')
            .take(result.returned_lines)
            .collect()
    } else {
        Vec::new()
    };

    loop {
        let (next_offset, next_byte_offset) = if lines.len() < result.returned_lines {
            (Some(result.offset + lines.len()), 0)
        } else {
            (result.next_offset, result.next_byte_offset)
        };

        let metadata = if lines.is_empty() {
            format!(
                "Tool output {output_id}: no lines returned from offset {}; {} total lines ({} bytes)",
                result.offset, result.total_lines, result.total_bytes
            )
        } else {
            format!(
                "Tool output {output_id}: lines {}-{} of {} ({} shown, {} bytes stored)",
                result.offset,
                result.offset + lines.len() - 1,
                result.total_lines,
                token_label(estimate_tokens(&lines.join("\n"))),
                result.total_bytes
            )
        };

        let mut parts = vec![metadata];
        if !lines.is_empty() {
            parts.push(String::new());
            parts.extend(lines.iter().map(|l| (*l).to_owned()));
        }
        if let Some(next_offset) = next_offset {
            parts.push(String::new());
            parts.push(read_hint(output_id, next_offset, next_byte_offset, limit));
        }

        let output = parts.join("\n");
        if fits(&output) || lines.is_empty() {
            return output;
        }
        lines.pop();
    }
}

fn format_grep(call: &GrepCall, result: &ToolOutputGrepResult) -> String {
    if result.rows.is_empty() {
        return "No matches.".to_owned();
    }
    let mut parts: Vec<String> = result
        .rows
        .iter()
        .map(|row| {
            let indicator = if row.is_match { ':' } else { '-' };
            format!("{}{indicator} {}", row.line_number, row.text)
        })
        .collect();
    if let Some(next_offset) = result.next_offset {
        parts.push(String::new());
        parts.push(format!(
            "Next call: tool_output_grep(output_id={:?}, pattern={:?}, offset={next_offset}, limit={}, context_before={}, context_after={})",
            call.output_id, call.pattern, call.limit, call.context_before, call.context_after
        ));
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;
    use caudra_storage::StateDir;
    use caudra_storage::id::SessionRef;
    use serde_json::json;

    const SESSION: &str = "CNK1hV6GWoysH3KQMm5wv";
    const OTHER_SESSION: &str = "CNK1hV6GWoysH3KQMm5ww";
    const NO_MATCHES: &str = "No matches.";

    struct Fixture {
        _temp: tempfile::TempDir,
        ctx: ToolContext,
        output_id: String,
    }

    fn fixture(text: &str) -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(ToolOutputStore::new(StateDir::from_path(
            temp.path().to_path_buf(),
        )));
        let session: SessionRef = SESSION.parse().unwrap();
        let output = store.put(session.id(), text).unwrap();
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.session_id = Some(session);
        ctx.tool_output_store = Some(store);
        Fixture {
            _temp: temp,
            ctx,
            output_id: output.id.to_string(),
        }
    }

    fn run(tool: &dyn Tool, input: Value, ctx: &ToolContext) -> Result<String, String> {
        let invocation = tool.parse(&input).map_err(|e| e.to_string())?;
        let result = smol::block_on(invocation.execute(ctx));
        match result.output {
            Ok(output) => Ok(output.as_text()),
            Err(error) => Err(error),
        }
    }

    #[test]
    fn read_requires_a_session() {
        let mut f = fixture("output");
        f.ctx.session_id = None;
        let error = run(&ToolOutputRead, json!({ "output_id": f.output_id }), &f.ctx).unwrap_err();
        assert!(error.contains(SESSION_REQUIRED), "got: {error}");
    }

    #[test]
    fn read_requires_a_store() {
        let mut f = fixture("output");
        f.ctx.tool_output_store = None;
        let error = run(&ToolOutputRead, json!({ "output_id": f.output_id }), &f.ctx).unwrap_err();
        assert!(error.contains(STORE_UNAVAILABLE), "got: {error}");
    }

    #[test]
    fn invalid_ids_are_rejected() {
        let f = fixture("output");
        let error = run(
            &ToolOutputGrep,
            json!({ "output_id": "not-an-output-id", "pattern": "output" }),
            &f.ctx,
        )
        .unwrap_err();
        assert!(error.contains("invalid tool output ID"), "got: {error}");
    }

    #[test]
    fn another_sessions_output_does_not_exist() {
        let mut f = fixture("private output");
        f.ctx.session_id = Some(OTHER_SESSION.parse().unwrap());
        let error = run(&ToolOutputRead, json!({ "output_id": f.output_id }), &f.ctx).unwrap_err();
        assert!(error.contains("does not exist for session"), "got: {error}");
    }

    #[test]
    fn read_paginates_with_a_hint_that_resumes_exactly() {
        let f = fixture("one\ntwo\nthree\nfour\n");
        let first = run(
            &ToolOutputRead,
            json!({ "output_id": &f.output_id, "limit": 2 }),
            &f.ctx,
        )
        .unwrap();
        assert!(first.contains("lines 1-2 of 4"), "{first}");
        assert!(first.contains("one\ntwo"), "{first}");
        assert!(
            first.contains(&format!(
                "Next call: tool_output_read(output_id={:?}, offset=3, limit=2)",
                f.output_id
            )),
            "{first}"
        );

        let second = run(
            &ToolOutputRead,
            json!({ "output_id": &f.output_id, "offset": 3, "limit": 2 }),
            &f.ctx,
        )
        .unwrap();
        assert!(second.contains("lines 3-4 of 4"), "{second}");
        assert!(second.contains("three\nfour"), "{second}");
    }

    /// The size suffix was previously asserted only against the Lua mirror in
    /// `plugins/tool_output`, so the native footer could change shape with the
    /// suite still green. Pins both halves: what this page costs, and how much
    /// output remains behind the ID.
    #[test]
    fn a_page_reports_its_own_token_cost_and_the_stored_byte_total() {
        const TEXT: &str = "one\ntwo\nthree\nfour\n";
        let f = fixture(TEXT);
        let page = run(
            &ToolOutputRead,
            json!({ "output_id": &f.output_id, "limit": 2 }),
            &f.ctx,
        )
        .unwrap();
        assert!(
            page.contains(&format!(
                "lines 1-2 of 4 ({} shown, {} bytes stored)",
                token_label(estimate_tokens("one\ntwo")),
                TEXT.len()
            )),
            "{page}"
        );
    }

    #[test]
    fn a_long_line_resumes_by_byte_offset() {
        let long = "x".repeat(MAX_OUTPUT_BYTES * 2);
        let f = fixture(&long);
        let page = run(
            &ToolOutputRead,
            json!({ "output_id": &f.output_id, "limit": 1 }),
            &f.ctx,
        )
        .unwrap();
        assert!(page.contains("byte_offset="), "{page}");
        assert!(fits(&page), "page must respect the session budget");
    }

    #[test]
    fn formatting_never_exceeds_the_session_budget() {
        let body: String = (0..5000).map(|i| format!("line {i}\n")).collect();
        let f = fixture(&body);
        let page = run(
            &ToolOutputRead,
            json!({ "output_id": &f.output_id, "limit": MAX_READ_LIMIT }),
            &f.ctx,
        )
        .unwrap();
        assert!(fits(&page), "{} bytes", page.len());
        assert!(page.contains("Next call:"), "{page}");
    }

    #[test]
    fn grep_marks_matches_and_context_differently() {
        let f = fixture("alpha\nbeta\ngamma\nbeta\ndelta\n");
        let out = run(
            &ToolOutputGrep,
            json!({
                "output_id": &f.output_id,
                "pattern": "beta",
                "limit": 1,
                "context_before": 1,
            }),
            &f.ctx,
        )
        .unwrap();
        assert!(out.contains("1- alpha"), "context uses '-': {out}");
        assert!(out.contains("2: beta"), "matches use ':': {out}");
    }

    /// The hint has to be callable verbatim, so resume from it rather than
    /// asserting an offset the store is free to choose.
    #[test]
    fn the_grep_hint_resumes_at_the_next_match() {
        let f = fixture("alpha\nbeta\ngamma\nbeta\ndelta\n");
        let first = run(
            &ToolOutputGrep,
            json!({ "output_id": &f.output_id, "pattern": "beta", "limit": 1 }),
            &f.ctx,
        )
        .unwrap();
        assert!(first.contains("2: beta"), "{first}");

        let next_offset: usize = first
            .rsplit_once("offset=")
            .and_then(|(_, rest)| rest.split(',').next())
            .expect("hint carries the next offset")
            .parse()
            .expect("next offset is a number");

        let second = run(
            &ToolOutputGrep,
            json!({
                "output_id": &f.output_id,
                "pattern": "beta",
                "offset": next_offset,
                "limit": 1,
            }),
            &f.ctx,
        )
        .unwrap();
        assert!(second.contains("4: beta"), "{second}");
        assert!(!second.contains("2: beta"), "resumed page repeats a match");
    }

    #[test]
    fn grep_without_matches_says_so() {
        let f = fixture("alpha\nbeta\n");
        let out = run(
            &ToolOutputGrep,
            json!({ "output_id": &f.output_id, "pattern": "zeta" }),
            &f.ctx,
        )
        .unwrap();
        assert_eq!(out, NO_MATCHES);
    }

    /// An over-large limit is clamped rather than rejected: the model asking
    /// for more than a page can hold is not an error it can act on.
    #[test]
    fn an_oversized_read_limit_is_clamped_and_echoed_in_the_hint() {
        let body: String = (0..MAX_READ_LIMIT + 500)
            .map(|i| format!("l{i}\n"))
            .collect();
        let f = fixture(&body);
        let page = run(
            &ToolOutputRead,
            json!({ "output_id": &f.output_id, "limit": 99_999 }),
            &f.ctx,
        )
        .unwrap();
        assert!(
            page.contains(&format!("limit={MAX_READ_LIMIT}")),
            "hint must echo the clamped limit: {page}"
        );
    }

    #[test]
    fn an_oversized_grep_limit_is_clamped_and_echoed_in_the_hint() {
        let body: String = (0..MAX_GREP_LIMIT + 50)
            .map(|i| format!("hit {i}\n"))
            .collect();
        let f = fixture(&body);
        let page = run(
            &ToolOutputGrep,
            json!({
                "output_id": &f.output_id,
                "pattern": "hit",
                "limit": 99_999,
                "context_after": 99,
            }),
            &f.ctx,
        )
        .unwrap();
        assert!(page.contains(&format!("limit={MAX_GREP_LIMIT}")), "{page}");
        assert!(
            page.contains(&format!("context_after={MAX_CONTEXT}")),
            "{page}"
        );
    }

    #[test]
    fn both_tools_are_visible_to_every_audience() {
        assert_eq!(ToolOutputRead.audience(), ToolAudience::all());
        assert_eq!(ToolOutputGrep.audience(), ToolAudience::all());
    }

    #[test]
    fn schemas_require_their_identifying_fields() {
        assert_eq!(ToolOutputRead.schema()["required"], json!(["output_id"]));
        assert_eq!(
            ToolOutputGrep.schema()["required"],
            json!(["output_id", "pattern"])
        );
    }
}
