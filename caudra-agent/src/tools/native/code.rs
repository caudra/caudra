//! `code`: one fixed tool that runs Python in an isolated interpreter. Whatever it reaches beyond
//! the language, MCP servers among it, reaches the model as reminders, so connecting,
//! reconnecting or reconfiguring a server never changes the request's tool array.

use std::borrow::Cow;
use std::sync::LazyLock;

use serde_json::Value;

use crate::code::gateway;
use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolEffect, ToolExecResult,
    ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{CODE_TOOL_NAME, DescriptionContext, ToolAudience, ToolContext};
use crate::types::{ToolInput, ToolOutput};

const SNIPPETS: &str = "Run Python in an isolated interpreter: compute, reshape data, and reach what the session connects it to.

- A snippet is module code, not a function body: its last expression is the result, returned with anything printed. Do not use `return` at top level. Top-level `await` works. Filter and summarise in the snippet, and return only what you need.
- Variables persist between runs; `reset: true` starts empty.";
const HISTORY: &str = "- `history` searches the conversations stored on this machine, compacted parts and subagents included. `await history.search(\"query\", scope=\"project\", kinds=None, tools=None, since=None, until=None, sort=\"relevance\", rerank=None, limit=None, offset=0)` returns `{\"hits\": [{\"ref\", \"session\", \"title\", \"project\", \"when\", \"kind\", \"tool\", \"agent\", \"snippet\", \"score\"}], \"ranking\", \"judged_unrelated\", \"more\", \"unindexed_sessions\", \"note\"}`; `scope` is `\"session\"` (this conversation), `\"project\"`, `\"all\"` or a project path; `kinds` are `user`, `assistant`, `summary`, `reasoning`, `tool_call` and `tool_result`; `since` and `until` take ISO dates; `sort` is `\"relevance\"` or `\"newest\"`. `await history.read(ref, offset=0, limit=8000, around=0)` returns an item's full text from byte `offset`, with `around` neighbours each side. `await history.sessions(scope=\"project\", title=None, limit=20, offset=0)` lists sessions. Reading another project's history asks first.";
const MCP_SERVERS: &str = "- MCP servers are namespaces when the `# MCP servers` reminder lists them with their tools' signatures. Call a tool as an async method of its server: `issues = await github.list_issues(owner=\"o\", repo=\"r\")`. Run independent calls together: `import asyncio`, then `await asyncio.gather(...)`.
- A tool returns its structured content, else its text, parsed when it is JSON. Images and other media come back as a dict naming their type and size.
- `search(\"query\", server=None, limit=10, offset=0)` finds tools the reminder leaves out and returns their exact signatures. `servers()` lists every server and whether it is connected. Both answer at once, so never `await` them.
- `await resources(\"server\")` lists what a server offers to be read; `await read_resource(\"server\", uri)` reads one.
- `await decide(...)` and `await rerank(...)` exist when the reminder describes them.";
const FAILURES: &str = "- A failed call raises `RuntimeError` and a refused one `PermissionError`; catch them to carry on. Calls that ran before a failure keep their effects.";
const SANDBOX: &str = "- The interpreter runs a Python subset: no filesystem, network, subprocesses, class inheritance or third-party packages. Its modules are `asyncio`, `base64`, `binascii`, `bisect`, `collections`, `copy`, `csv`, `dataclasses`, `datetime`, `functools`, `hashlib`, `itertools`, `json`, `math`, `os`, `pathlib`, `posixpath`, `random`, `re`, `sys`, `time`, `tomllib`, `typing` and `unicodedata`. `os.path` supports POSIX path manipulation, not host access. `match`, list slice assignment, set subset ordering, dataclass fields/factories and `__post_init__`, arithmetic/subscript/call dunders, and `re.split` capture groups work. Type checking can reject `map`, `filter`, `getattr`, `setattr`, `hasattr` and `format`; use comprehensions and f-strings.";
/// The description in reading order. A host module documents itself with one section of its own,
/// after `SNIPPETS`. Nothing here depends on what the session connects, so the tool array stays
/// cache-stable.
const SECTIONS: &[&str] = &[SNIPPETS, HISTORY, MCP_SERVERS, FAILURES, SANDBOX];

pub static DESCRIPTION: LazyLock<String> = LazyLock::new(|| SECTIONS.join("\n"));
static PERMISSION_CONTRACT: LazyLock<String> =
    LazyLock::new(|| super::permission_contract(&CodeTool, ToolEffect::Orchestrator, &DESCRIPTION));

/// The identity the `code` tool registers under, which the calls its snippets make ask as.
pub fn permission_contract() -> &'static str {
    &PERMISSION_CONTRACT
}

const CODE_FIELD: &str = "code";
const RESET_FIELD: &str = "reset";
const PYTHON_LANGUAGE: &str = "python";
const ONE_LINE: &str = "1 line";
const LINES: &str = "lines";

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

pub struct CodeTool;

impl Tool for CodeTool {
    fn name(&self) -> &str {
        CODE_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(DESCRIPTION.as_str())
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
        Ok(Box::new(CodeRun { code, reset }))
    }
}

struct CodeRun {
    code: String,
    reset: bool,
}

impl ToolInvocation for CodeRun {
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
            let output = gateway::run(ctx, self.code, self.reset).await;
            let model_text = output.model_text();
            let failure = output.outcome.failure();
            let result = ToolExecResult::from(Ok(ToolOutput::Code(output)))
                .with_model_output(Some(model_text));
            match failure {
                Some(failure) => result.with_failure(failure),
                None => result,
            }
        })
    }
}
