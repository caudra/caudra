//! `code`: one fixed tool that runs Python in an isolated interpreter. Whatever it reaches beyond
//! the language, MCP servers among it, reaches the model as reminders, so connecting,
//! reconnecting or reconfiguring a server never changes the request's tool array.

use std::borrow::Cow;
use std::sync::LazyLock;
use std::time::Duration;

use serde_json::Value;

use crate::code::module::ModuleDoc;
use crate::code::{MODULE_DOCS, gateway};
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
const MODULES: &str = "- Modules are namespaces of calls that reach beyond the interpreter; `await` each call unless its help says otherwise:";
const HELP: &str = "- `help()` lists the modules and what this session can reach now; `help(\"history\")` shows a module's calls, `help(\"history.search\")` one call's full reference, `help(\"github\")` an MCP server's tools. A call a module line says to read `help` for first raises with its reference instead of running until this agent has read it; then call it again.";
const READ_FIRST_LEAD: &str = " Before the first ";
const READ_FIRST_JOIN: &str = " or ";
const READ_FIRST_HELP: &str = ", read ";
const FAILURES: &str = "- A failed call raises `RuntimeError` and a refused one `PermissionError`; catch them to carry on. Calls that ran before a failure keep their effects.";
const SANDBOX: &str = "- The interpreter runs a Python subset: no filesystem, network, subprocesses, class inheritance or third-party packages. Its modules are `asyncio`, `base64`, `binascii`, `bisect`, `collections`, `copy`, `csv`, `dataclasses`, `datetime`, `functools`, `hashlib`, `itertools`, `json`, `math`, `os`, `pathlib`, `posixpath`, `random`, `re`, `sys`, `time`, `tomllib`, `typing` and `unicodedata`. `os.path` supports POSIX path manipulation, not host access. Clocks read the host clock in UTC, and `time.sleep` or `await asyncio.sleep` wait for real within `timeoutSec`; prefer `asyncio.sleep` beside gathered calls, and seed `random` explicitly. `match`, list slice assignment, set subset ordering, dataclass fields/factories and `__post_init__`, arithmetic/subscript/call dunders, and `re.split` capture groups work. Type checking can reject `map`, `filter`, `getattr`, `setattr`, `hasattr` and `format`; use comprehensions and f-strings.";
/// The description in reading order: each module's one-line summary, then how to load the rest.
/// Nothing here depends on what the session connects, so the tool array stays cache-stable.
pub static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    let modules = MODULE_DOCS
        .iter()
        .map(|doc| format!("  - {}{}", doc.summary, read_first_note(doc)));
    [SNIPPETS.to_owned(), MODULES.to_owned()]
        .into_iter()
        .chain(modules)
        .chain([HELP, FAILURES, SANDBOX].map(str::to_owned))
        .collect::<Vec<_>>()
        .join("\n")
});
/// ` Before the first `image.generate`, read `help("image")`.`: what a module line adds when some
/// of its calls need their reference first, which `help("<module>")` shows in full.
fn read_first_note(doc: &ModuleDoc) -> String {
    let calls: Vec<String> = doc
        .read_first()
        .map(|member| format!("`{}`", doc.qualified(member)))
        .collect();
    match calls.is_empty() {
        true => String::new(),
        false => format!(
            "{READ_FIRST_LEAD}{}{READ_FIRST_HELP}`help(\"{}\")`.",
            calls.join(READ_FIRST_JOIN),
            doc.namespace
        ),
    }
}

static PERMISSION_CONTRACT: LazyLock<String> =
    LazyLock::new(|| super::permission_contract(&CodeTool, ToolEffect::Orchestrator, &DESCRIPTION));

/// The identity the `code` tool registers under, which the calls its snippets make ask as.
pub fn permission_contract() -> &'static str {
    &PERMISSION_CONTRACT
}

const CODE_FIELD: &str = "code";
const RESET_FIELD: &str = "reset";
const TIMEOUT_FIELD: &str = "timeoutSec";
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 3600;
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
static TIMEOUT_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Integer,
    description: "Wall-clock deadline in seconds for the whole run, waits and calls included: 1 to 3600, default 120",
};
static PROPERTIES: &[Property] = &[
    (CODE_FIELD, &CODE_PARAM, true, &[]),
    (RESET_FIELD, &RESET_PARAM, false, &[]),
    (TIMEOUT_FIELD, &TIMEOUT_PARAM, false, &[]),
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
        let timeout = parse_timeout(input.get(TIMEOUT_FIELD))?;
        Ok(Box::new(CodeRun {
            code,
            reset,
            timeout,
        }))
    }
}

fn parse_timeout(value: Option<&Value>) -> Result<Duration, ParseError> {
    let Some(value) = value else {
        return Ok(Duration::from_secs(DEFAULT_TIMEOUT_SECS));
    };
    value
        .as_u64()
        .filter(|secs| (1..=MAX_TIMEOUT_SECS).contains(secs))
        .map(Duration::from_secs)
        .ok_or_else(|| {
            ParseError::custom(format!(
                "{TIMEOUT_FIELD} must be a whole number of seconds from 1 to {MAX_TIMEOUT_SECS}, got {value}"
            ))
        })
}

struct CodeRun {
    code: String,
    reset: bool,
    timeout: Duration,
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
            let output = gateway::run(ctx, self.code, self.reset, self.timeout).await;
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use caudra_providers::estimate_tokens;
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const SNIPPET: &str = "1";
    /// What the always-loaded description may cost; the rest of every module loads with `help()`.
    const DESCRIPTION_TOKENS: usize = 1_000;
    const SUMMARY_CHARS: usize = 260;

    #[test]
    fn the_description_lists_every_module_within_its_budget() {
        for doc in MODULE_DOCS {
            assert!(DESCRIPTION.contains(doc.summary), "{}", doc.namespace);
        }
        let tokens = estimate_tokens(&DESCRIPTION) as usize;
        assert!(tokens <= DESCRIPTION_TOKENS, "{tokens}");
    }

    /// Every module documents both layers, and no two share a namespace.
    #[test]
    fn every_module_documents_itself_in_both_layers() {
        let mut namespaces = HashSet::new();
        for doc in MODULE_DOCS {
            assert!(namespaces.insert(doc.namespace), "{}", doc.namespace);
            assert!(doc.summary.len() <= SUMMARY_CHARS, "{}", doc.namespace);
            assert!(doc.summary.starts_with(&format!("`{}`", doc.namespace)));
            assert!(
                !doc.page.is_empty() && !doc.members.is_empty(),
                "{}",
                doc.namespace
            );
            for member in doc.members {
                assert!(
                    member.signature.starts_with(&format!("{}(", member.name))
                        && !member.page.is_empty(),
                    "{}.{}",
                    doc.namespace,
                    member.name
                );
            }
        }
    }

    /// Exactly the calls whose reference must be read first are named in the description, which
    /// says to read their module's help.
    #[test]
    fn the_description_names_the_calls_to_read_help_for_first() {
        let marked: Vec<String> = MODULE_DOCS
            .iter()
            .flat_map(|doc| doc.read_first().map(|member| doc.qualified(member)))
            .collect();

        assert_eq!(
            marked,
            [
                "history.search",
                "history.read",
                "image.generate",
                "workflow.start"
            ]
        );
        for line in [
            "Before the first `history.search` or `history.read`, read `help(\"history\")`.",
            "Before the first `image.generate`, read `help(\"image\")`.",
            "Before the first `workflow.start`, read `help(\"workflow\")`.",
        ] {
            assert!(DESCRIPTION.contains(line), "{line}");
        }
        assert_eq!(DESCRIPTION.matches(READ_FIRST_LEAD).count(), 3);
    }

    #[test_case(json!(null), Some(DEFAULT_TIMEOUT_SECS) ; "null_means_omitted")]
    #[test_case(json!(1), Some(1) ; "smallest")]
    #[test_case(json!(MAX_TIMEOUT_SECS), Some(MAX_TIMEOUT_SECS) ; "largest")]
    #[test_case(json!(0), None ; "zero_refused")]
    #[test_case(json!(MAX_TIMEOUT_SECS + 1), None ; "past_the_cap_refused")]
    #[test_case(json!(-5), None ; "negative_refused")]
    fn timeout_sec_is_checked_like_shell(timeout: Value, expected: Option<u64>) {
        let parsed = parse_timeout(
            validate(
                &SCHEMA,
                json!({CODE_FIELD: SNIPPET, TIMEOUT_FIELD: timeout}),
            )
            .unwrap()
            .get(TIMEOUT_FIELD),
        );
        assert_eq!(parsed.ok().map(|timeout| timeout.as_secs()), expected);
    }
}
