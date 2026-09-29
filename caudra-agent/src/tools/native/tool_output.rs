//! `tool_output`: paging over output too large to inline.
//!
//! When a tool's result exceeds the session's line budget it is spilled to the
//! `ToolOutputStore` and replaced by a truncation notice carrying an opaque
//! id. This tool is how the model gets the rest back, either as a page of
//! lines or as regex matches: one contract rather than two near-identical ones
//! competing for the same call. It is session-scoped, so an id from another
//! session reads as if it does not exist.

use std::borrow::Cow;
use std::sync::Arc;

use serde_json::Value;

use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolError, ToolExecResult,
    ToolFailure, ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{DescriptionContext, ToolAudience, ToolContext};
use crate::types::ToolOutput;
use caudra_providers::{estimate_tokens, token_label};
use caudra_storage::id::CaudraId;
use caudra_storage::tool_outputs::{
    ToolOutputError, ToolOutputGrepResult, ToolOutputId, ToolOutputReadResult, ToolOutputStore,
};

pub const DESCRIPTION: &str = "Page or search managed tool output owned by the current session. \
     Omit `pattern` to read lines from `offset`, or supply it to return regex matches with \
     context.";

const SESSION_REQUIRED: &str = "tool output retrieval requires a session";
const STORE_UNAVAILABLE: &str = "tool output store is unavailable";
const BYTE_OFFSET_WITH_PATTERN: &str = "byte_offset only applies without pattern";

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
    description: "Output handle from a truncation notice or task result. New handles describe the producing tool or command, with numeric suffixes for collisions. Existing IDs remain valid. Pass the handle unchanged.",
};
static OFFSET_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Integer,
    description: "Starting line, 1-indexed (default: 1).",
};
static BYTE_OFFSET_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Integer,
    description: "Starting byte within the first line (default: 0; use continuation hints). \
                  Reading only.",
};
static LIMIT_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Integer,
    description: "Lines to return when reading, or matches when searching. Reading defaults to \
                  200 and caps at 2000. Searching defaults to 100 and caps at 200.",
};
static PATTERN_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Regex to search for. Omit to read lines instead.",
};
static CONTEXT_BEFORE_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Integer,
    description: "Context lines before each match (default: 0; capped at 5). Searching only.",
};
static CONTEXT_AFTER_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Integer,
    description: "Context lines after each match (default: 0; capped at 5). Searching only.",
};
static PROPERTIES: &[Property] = &[
    ("output_id", &OUTPUT_ID_PARAM, true, &[]),
    ("pattern", &PATTERN_PARAM, false, &[]),
    ("offset", &OFFSET_PARAM, false, &[]),
    ("limit", &LIMIT_PARAM, false, &[]),
    ("byte_offset", &BYTE_OFFSET_PARAM, false, &[]),
    ("context_before", &CONTEXT_BEFORE_PARAM, false, &[]),
    ("context_after", &CONTEXT_AFTER_PARAM, false, &[]),
];
static SCHEMA: ParamSchema = ParamSchema::Object {
    properties: PROPERTIES,
    description: "",
    reject_unknown: false,
};

pub struct ToolOutputTool;

impl Tool for ToolOutputTool {
    fn name(&self) -> &str {
        crate::tools::TOOL_OUTPUT_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(DESCRIPTION)
    }

    fn schema(&self) -> Value {
        to_json_schema(&SCHEMA)
    }

    fn audience(&self) -> ToolAudience {
        ToolAudience::all()
    }

    fn tool_kind(&self) -> Option<&str> {
        Some("read")
    }

    /// `pattern` picks the mode. A `byte_offset` alongside it is refused
    /// rather than dropped: it means the model expected a page and would
    /// otherwise read the match list as one.
    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let input = validate(&SCHEMA, input.clone())?;
        let output_id = required_str(&input, "output_id")?;
        let offset = usize_field(&input, "offset");
        let limit = usize_field(&input, "limit");
        let Some(pattern) = optional_str(&input, "pattern") else {
            return Ok(Box::new(ReadCall {
                output_id,
                offset: offset.unwrap_or(DEFAULT_READ_OFFSET),
                byte_offset: usize_field(&input, "byte_offset").unwrap_or(DEFAULT_READ_BYTE_OFFSET),
                limit: limit.unwrap_or(DEFAULT_READ_LIMIT).min(MAX_READ_LIMIT),
            }));
        };
        if input.get("byte_offset").is_some() {
            return Err(ParseError::custom(BYTE_OFFSET_WITH_PATTERN.to_owned()));
        }
        Ok(Box::new(GrepCall {
            output_id,
            pattern,
            offset: offset.unwrap_or(DEFAULT_GREP_OFFSET),
            limit: limit.unwrap_or(DEFAULT_GREP_LIMIT).min(MAX_GREP_LIMIT),
            context_before: usize_field(&input, "context_before")
                .unwrap_or(DEFAULT_CONTEXT)
                .min(MAX_CONTEXT),
            context_after: usize_field(&input, "context_after")
                .unwrap_or(DEFAULT_CONTEXT)
                .min(MAX_CONTEXT),
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
        Box::pin(async move { page_result(self.run(ctx).await) })
    }
}

impl ReadCall {
    async fn run(&self, ctx: &ToolContext) -> Result<String, ToolError> {
        let (session, store) = access(ctx)?;
        let id = parse_id(&self.output_id)?;
        let (offset, limit) = (
            positive(self.offset, "offset")?,
            positive(self.limit, "limit")?,
        );
        let byte_offset = self.byte_offset;
        let result =
            smol::unblock(move || store.read_at(session, id, offset, limit, byte_offset)).await;
        let result = result.map_err(store_error)?;
        Ok(format_read(&self.output_id, &result, self.limit))
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
        Box::pin(async move { page_result(self.run(ctx).await) })
    }
}

impl GrepCall {
    async fn run(&self, ctx: &ToolContext) -> Result<String, ToolError> {
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
        let result = result.map_err(store_error)?;
        Ok(format_grep(self, &result))
    }
}

fn page_result(page: Result<String, ToolError>) -> ToolExecResult {
    match page {
        Ok(text) => ToolExecResult::from(Ok(ToolOutput::Plain(text.into()))),
        Err(error) => ToolExecResult::failed(error.failure, format!("error: {error}")),
    }
}

fn access(ctx: &ToolContext) -> Result<(CaudraId, Arc<ToolOutputStore>), String> {
    let session = ctx.session_id.as_ref().ok_or(SESSION_REQUIRED)?;
    let store = ctx.tool_output_store.as_ref().ok_or(STORE_UNAVAILABLE)?;
    Ok((session.id(), Arc::clone(store)))
}

fn parse_id(raw: &str) -> Result<ToolOutputId, ToolError> {
    raw.parse::<ToolOutputId>().map_err(|e| {
        ToolError::new(
            ToolFailure::InvalidInput,
            format!("invalid tool output ID: {e}"),
        )
    })
}

fn positive(value: usize, name: &str) -> Result<usize, ToolError> {
    (value > 0).then_some(value).ok_or_else(|| {
        ToolError::new(
            ToolFailure::InvalidInput,
            format!("{name} must be at least 1"),
        )
    })
}

fn store_error(error: ToolOutputError) -> ToolError {
    let failure = match error {
        ToolOutputError::NotFound { .. } => ToolFailure::NotFound,
        ToolOutputError::InvalidOffset
        | ToolOutputError::InvalidByteOffset { .. }
        | ToolOutputError::InvalidLimit
        | ToolOutputError::PatternTooLong { .. }
        | ToolOutputError::InvalidPattern(_) => ToolFailure::InvalidInput,
        _ => ToolFailure::Other,
    };
    ToolError::new(failure, error.to_string())
}

fn required_str(input: &Value, key: &str) -> Result<String, ParseError> {
    optional_str(input, key).ok_or_else(|| ParseError::custom(format!("{key} is required")))
}

fn optional_str(input: &Value, key: &str) -> Option<String> {
    input.get(key)?.as_str().map(str::to_owned)
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
    let tool = crate::tools::TOOL_OUTPUT_TOOL_NAME;
    if byte_offset > 0 {
        format!(
            "Next call: {tool}(output_id={output_id:?}, offset={offset}, byte_offset={byte_offset}, limit={limit})"
        )
    } else {
        format!("Next call: {tool}(output_id={output_id:?}, offset={offset}, limit={limit})")
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
            "Next call: {}(output_id={:?}, pattern={:?}, offset={next_offset}, limit={}, context_before={}, context_after={})",
            crate::tools::TOOL_OUTPUT_TOOL_NAME,
            call.output_id,
            call.pattern,
            call.limit,
            call.context_before,
            call.context_after
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
    use test_case::test_case;

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
        let error = run(&ToolOutputTool, json!({ "output_id": f.output_id }), &f.ctx).unwrap_err();
        assert!(error.contains(SESSION_REQUIRED), "got: {error}");
    }

    #[test]
    fn read_requires_a_store() {
        let mut f = fixture("output");
        f.ctx.tool_output_store = None;
        let error = run(&ToolOutputTool, json!({ "output_id": f.output_id }), &f.ctx).unwrap_err();
        assert!(error.contains(STORE_UNAVAILABLE), "got: {error}");
    }

    #[test]
    fn invalid_ids_are_rejected() {
        let f = fixture("output");
        let error = run(
            &ToolOutputTool,
            json!({ "output_id": "../not-an-output-id", "pattern": "output" }),
            &f.ctx,
        )
        .unwrap_err();
        assert!(error.contains("invalid tool output ID"), "got: {error}");
    }

    #[test]
    fn another_sessions_output_does_not_exist() {
        let mut f = fixture("private output");
        f.ctx.session_id = Some(OTHER_SESSION.parse().unwrap());
        let error = run(&ToolOutputTool, json!({ "output_id": f.output_id }), &f.ctx).unwrap_err();
        assert!(error.contains("does not exist for session"), "got: {error}");
    }

    #[test_case(Some(OTHER_SESSION), None, None, ToolFailure::NotFound ; "another_sessions_output")]
    #[test_case(Some(SESSION), Some("../not-an-output-id"), None, ToolFailure::InvalidInput ; "malformed_id")]
    #[test_case(Some(SESSION), None, Some("("), ToolFailure::InvalidInput ; "unparsable_pattern")]
    #[test_case(None, None, None, ToolFailure::Other ; "no_session")]
    fn a_refused_call_states_why(
        session: Option<&str>,
        output_id: Option<&str>,
        pattern: Option<&str>,
        expected: ToolFailure,
    ) {
        let mut f = fixture("output");
        f.ctx.session_id = session.map(|session| session.parse().unwrap());
        let mut input = json!({ "output_id": output_id.unwrap_or(&f.output_id) });
        if let Some(pattern) = pattern {
            input["pattern"] = json!(pattern);
        }
        let invocation = ToolOutputTool.parse(&input).unwrap();
        let result = smol::block_on(invocation.execute(&f.ctx));
        assert!(result.is_error);
        assert_eq!(result.failure, Some(expected));
    }

    #[test]
    fn read_paginates_with_a_hint_that_resumes_exactly() {
        let f = fixture("one\ntwo\nthree\nfour\n");
        let first = run(
            &ToolOutputTool,
            json!({ "output_id": &f.output_id, "limit": 2 }),
            &f.ctx,
        )
        .unwrap();
        assert!(first.contains("lines 1-2 of 4"), "{first}");
        assert!(first.contains("one\ntwo"), "{first}");
        assert!(
            first.contains(&format!(
                "Next call: {}(output_id={:?}, offset=3, limit=2)",
                crate::tools::TOOL_OUTPUT_TOOL_NAME,
                f.output_id
            )),
            "{first}"
        );

        let second = run(
            &ToolOutputTool,
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
            &ToolOutputTool,
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
            &ToolOutputTool,
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
            &ToolOutputTool,
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
            &ToolOutputTool,
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
            &ToolOutputTool,
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
            &ToolOutputTool,
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
            &ToolOutputTool,
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
            &ToolOutputTool,
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
            &ToolOutputTool,
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
    fn the_tool_is_visible_to_every_audience() {
        assert_eq!(ToolOutputTool.audience(), ToolAudience::all());
    }

    /// `pattern` is the mode switch, so it cannot be required: a schema that
    /// demanded it would leave no way to ask for a page.
    #[test]
    fn only_the_output_id_is_required() {
        assert_eq!(ToolOutputTool.schema()["required"], json!(["output_id"]));
    }

    #[test]
    fn a_pattern_switches_the_same_call_from_paging_to_searching() {
        let f = fixture("alpha\nbeta\ngamma\n");
        let page = run(
            &ToolOutputTool,
            json!({ "output_id": &f.output_id }),
            &f.ctx,
        )
        .unwrap();
        assert!(page.contains("lines 1-3 of 3"), "{page}");

        let matches = run(
            &ToolOutputTool,
            json!({ "output_id": &f.output_id, "pattern": "beta" }),
            &f.ctx,
        )
        .unwrap();
        assert!(matches.contains("2: beta"), "{matches}");
        assert!(!matches.contains("1: alpha"), "{matches}");
    }

    /// Dropping it would answer a page request with a match list.
    #[test]
    fn a_byte_offset_with_a_pattern_is_refused_rather_than_ignored() {
        let f = fixture("alpha\nbeta\n");
        let error = run(
            &ToolOutputTool,
            json!({ "output_id": &f.output_id, "pattern": "beta", "byte_offset": 4 }),
            &f.ctx,
        )
        .unwrap_err();
        assert!(error.contains(BYTE_OFFSET_WITH_PATTERN), "got: {error}");
    }
}
