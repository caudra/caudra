//! `mcp`: one fixed tool through which the model reaches every MCP server, by writing Python that
//! calls them. The servers and their tools reach the model as the `# MCP servers` reminder, so
//! connecting, reconnecting or reconfiguring a server never changes the request's tool array.

use std::borrow::Cow;

use serde_json::Value;

use crate::mcp::code::gateway;
use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolExecResult, ToolFailure,
    ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{DescriptionContext, MCP_TOOL_NAME, ToolAudience, ToolContext};
use crate::types::{ToolInput, ToolOutput};

pub const DESCRIPTION: &str = "Run Python that calls the MCP servers this session connects to. The `# MCP servers` reminder lists each server and the signatures of its tools.

- Call a tool as an async method of its server: `issues = await github.list_issues(owner=\"o\", repo=\"r\")`. Run independent calls together: `import asyncio`, then `await asyncio.gather(...)`.
- A tool returns its structured content, else its text, parsed when it is JSON. Images and other media come back as a dict naming their type and size.
- `search(\"query\", server=None, limit=10, offset=0)` finds tools the reminder leaves out and returns their exact signatures. `servers()` lists every server and whether it is connected. Both answer at once, so never `await` them.
- `await resources(\"server\")` lists what a server offers to be read; `await read_resource(\"server\", uri)` reads one.
- A failed call raises `RuntimeError` and a refused one `PermissionError`; catch them to carry on. Calls that ran before a failure keep their effects.
- A snippet is module code, not a function body: its last expression is the result, returned with anything printed. Do not use `return` at top level. Filter and summarise in the snippet, and return only what you need.
- Variables persist between runs; `reset: true` starts empty.
- The interpreter is a sandboxed Python subset: no filesystem, network, subprocesses, class inheritance or third-party packages. Its modules are `asyncio`, `base64`, `binascii`, `collections`, `copy`, `dataclasses`, `datetime`, `functools`, `itertools`, `json`, `math`, `os`, `pathlib`, `random`, `re`, `sys`, `time`, `typing` and `unicodedata`. Type checking can reject `map`, `filter`, `getattr`, `setattr`, `hasattr` and `format`; use comprehensions and f-strings.";

const CODE_FIELD: &str = "code";
const RESET_FIELD: &str = "reset";
const PYTHON_LANGUAGE: &str = "python";
const ONE_LINE: &str = "1 line";
const LINES: &str = "lines";
const NO_SERVERS: &str = "No MCP servers are configured in this session.";

static CODE_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Python to run. Top-level `await` works",
};
static RESET_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Bool,
    description: "Drop the variables earlier runs kept before running",
};
static PROPERTIES: &[Property] = &[
    (CODE_FIELD, &CODE_PARAM, true, &[]),
    (RESET_FIELD, &RESET_PARAM, false, &[]),
];
static SCHEMA: ParamSchema = ParamSchema::Object {
    properties: PROPERTIES,
    description: "",
    reject_unknown: true,
};

pub struct McpTool;

impl Tool for McpTool {
    fn name(&self) -> &str {
        MCP_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(DESCRIPTION)
    }

    fn schema(&self) -> Value {
        to_json_schema(&SCHEMA)
    }

    /// No interpreter audience: a snippet already running in the code worker cannot start another.
    fn audience(&self) -> ToolAudience {
        ToolAudience::MAIN | ToolAudience::RESEARCH_SUB | ToolAudience::GENERAL_SUB
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let input = validate(&SCHEMA, input.clone())?;
        let code = input
            .get(CODE_FIELD)
            .and_then(Value::as_str)
            .filter(|code| !code.trim().is_empty())
            .ok_or_else(|| ParseError::custom(format!("{CODE_FIELD} is required")))?
            .to_owned();
        let reset = input
            .get(RESET_FIELD)
            .and_then(Value::as_bool)
            .unwrap_or_default();
        Ok(Box::new(McpRun { code, reset }))
    }
}

struct McpRun {
    code: String,
    reset: bool,
}

impl ToolInvocation for McpRun {
    fn start_header(&self) -> HeaderFuture {
        let header = match self.code.lines().count() {
            1 => ONE_LINE.to_owned(),
            count => format!("{count} {LINES}"),
        };
        HeaderFuture::Ready(HeaderResult::plain(header))
    }

    fn start_input(&self) -> Option<ToolInput> {
        Some(ToolInput::Code {
            language: PYTHON_LANGUAGE.to_owned(),
            code: self.code.clone(),
        })
    }

    /// Registered as an orchestrator, so plan and read-only modes keep it: each call it makes is
    /// judged on its own, and the gateway records the run once a call could change files.
    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            let Some(session) = ctx.mcp.as_ref() else {
                return ToolExecResult::failed(ToolFailure::NotFound, NO_SERVERS);
            };
            let output = gateway::run(session, ctx, self.code, self.reset).await;
            let model_text = output.model_text();
            let failure = output.outcome.failure();
            let result = ToolExecResult::from(Ok(ToolOutput::McpCode(output)))
                .with_model_output(Some(model_text));
            match failure {
                Some(failure) => result.with_failure(failure),
                None => result,
            }
        })
    }
}
