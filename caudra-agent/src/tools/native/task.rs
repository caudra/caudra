//! `task`: run an autonomous subagent and hand its result back to the caller.
//!
//! The run itself, the `output_schema` contract, and the process-wide cap on
//! concurrent subagents live in [`crate::agent::task_runner`], shared with
//! the workflow engine. This tool owns the model-facing contract: the input
//! schema and how an outcome reads back as a tool result.

use std::borrow::Cow;

use serde_json::Value;

use crate::agent::subagent::{self, TaskIdentity};
pub use crate::agent::task_runner::set_max_concurrent;
use crate::agent::task_runner::{TaskOutcome, TaskRequest, run_task};
use crate::subagent_history::SubagentTaskMode;
use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolExecResult, ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{DescriptionContext, ToolAudience, ToolContext};
use crate::types::ToolOutput;

pub const DESCRIPTION: &str = "Launch an autonomous subagent to perform tasks independently. Best combined with batch.

Modes:
- `plan` (default): Strictly read-only. For exploration, review, and implementation planning.
- `build`: Can modify files and run commands. For implementation work.

Available system prompt profiles:
{task_system_prompt_profiles}

Notes:
1. Launch multiple tasks concurrently when possible.
2. The agent's result is not visible to the user. Summarize it in your response.
3. A fresh call gives the subagent no context beyond your prompt, so make the prompt self-contained and state exactly what to report back.
4. Every result, success or failure, carries a task_id. Pass it back to continue that subagent with its previous messages and tool outputs, sending only the new work. Omit mode and profile when continuing; they stay locked to the original run. Omit prompt too to resume an interrupted subagent that needs no new instruction.
5. Tell it to return concise summaries with file:line refs, not full file contents.
";

const TASK_METADATA_FORMAT: &str = "<task_metadata>\ntask_id: {task_id}\n</task_metadata>";
const TASK_ID_PLACEHOLDER: &str = "{task_id}";
const DESCRIPTION_REQUIRED_ERROR: &str = "description is required";
const PROMPT_REQUIRED_ERROR: &str = "prompt is required unless task_id is set";

const MODES: &[&str] = &["plan", "build"];

static DESCRIPTION_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Short (3-5 words) description of the task",
};
static PROMPT_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Detailed task prompt for the agent. Required for a new task; omit it to resume a task_id with no new work.",
};
static TASK_ID_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Set this only to resume. Continues the subagent from an earlier task_id with its existing history instead of starting fresh.",
};
static MODE_PARAM: ParamSchema = ParamSchema::Enum {
    variants: MODES,
    description: "Subagent mode. Defaults to \"plan\" for a new task; omitted continuations retain their stored mode.",
};
static PROFILE_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "System prompt profile. Defaults to the parent profile for a new task; use \"builtin\" explicitly for Caudra's built-in prompt. Omitted continuations retain their stored profile.",
};
static OUTPUT_SCHEMA_PARAM: ParamSchema = ParamSchema::Any {
    description: "JSON Schema (object) the subagent's final result must match. When set, the result is returned as a validated JSON string.",
};
static PROPERTIES: &[Property] = &[
    ("description", &DESCRIPTION_PARAM, true, &[]),
    ("prompt", &PROMPT_PARAM, false, &[]),
    ("task_id", &TASK_ID_PARAM, false, &[]),
    ("mode", &MODE_PARAM, false, &[]),
    ("profile", &PROFILE_PARAM, false, &[]),
    ("output_schema", &OUTPUT_SCHEMA_PARAM, false, &[]),
];
static SCHEMA: ParamSchema = ParamSchema::Object {
    properties: PROPERTIES,
    description: "",
    reject_unknown: true,
};

pub struct TaskTool;

impl Tool for TaskTool {
    fn name(&self) -> &str {
        crate::tools::TASK_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(DESCRIPTION)
    }

    fn schema(&self) -> Value {
        to_json_schema(&SCHEMA)
    }

    fn audience(&self) -> ToolAudience {
        ToolAudience::MAIN
    }

    fn examples(&self) -> Option<Value> {
        Some(serde_json::json!([{
            "description": "Find auth middleware",
            "prompt": "Search the codebase for authentication middleware. Return file paths and a summary of how auth is implemented.",
        }]))
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let input = validate(&SCHEMA, input.clone())?;
        let field = |name: &str| input.get(name).and_then(Value::as_str).map(str::to_owned);
        let task_id = field("task_id");
        let prompt = field("prompt");
        if prompt.is_none() && task_id.is_none() {
            return Err(ParseError::custom(PROMPT_REQUIRED_ERROR));
        }
        Ok(Box::new(TaskCall {
            description: field("description")
                .ok_or_else(|| ParseError::custom(DESCRIPTION_REQUIRED_ERROR))?,
            prompt,
            task_id,
            profile: field("profile"),
            mode: field("mode").map(|mode| parse_mode(&mode)).transpose()?,
            output_schema: input.get("output_schema").cloned(),
        }))
    }
}

fn parse_mode(raw: &str) -> Result<SubagentTaskMode, ParseError> {
    match raw {
        "plan" => Ok(SubagentTaskMode::Plan),
        "build" => Ok(SubagentTaskMode::Build),
        other => Err(ParseError::custom(format!("unknown task mode: {other}"))),
    }
}

struct TaskCall {
    description: String,
    /// `None` continues an existing `task_id` with nothing new to say.
    prompt: Option<String>,
    task_id: Option<String>,
    profile: Option<String>,
    mode: Option<SubagentTaskMode>,
    output_schema: Option<Value>,
}

impl ToolInvocation for TaskCall {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(self.description.clone()))
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            let request = TaskRequest {
                prompt: self.prompt,
                label: self.description,
                task: TaskIdentity::continue_or_derive(self.task_id),
                mode: self.mode,
                profile: self.profile,
                output_schema: self.output_schema,
                call_id: ctx
                    .tool_use_id
                    .clone()
                    .unwrap_or_else(subagent::generated_session_id),
                provenance: None,
            };
            render(run_task(ctx, request).await)
        })
    }
}

/// The task ID is what makes a continuation possible, so it rides along with
/// failures too: an interrupted subagent is still resumable.
fn render(outcome: TaskOutcome) -> ToolExecResult {
    let result = match (outcome.error, outcome.output) {
        (Some(message), _) => error(message),
        (None, Value::String(text)) => markdown(text),
        (None, structured) => markdown(structured.to_string()),
    };
    match outcome.task_id {
        Some(task_id) => with_task_id(&task_id, result),
        None => result,
    }
}

fn with_task_id(task_id: &str, mut result: ToolExecResult) -> ToolExecResult {
    result.model_suffix = Some(TASK_METADATA_FORMAT.replace(TASK_ID_PLACEHOLDER, task_id));
    result
}

fn markdown(text: String) -> ToolExecResult {
    ToolExecResult::from(Ok(ToolOutput::Markdown(text.into())))
}

fn error(message: String) -> ToolExecResult {
    ToolExecResult {
        is_error: true,
        ..ToolExecResult::from(Ok(ToolOutput::Plain(message.into())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use test_case::test_case;

    const TASK_ID: &str = "toolu_01";
    const SUMMARY: &str = "found the middleware in src/auth.rs:12";
    const REQUIRED_FIELD: &str = "answer";
    const BOOM: &str = "boom";

    fn parse(input: Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        TaskTool.parse(&input)
    }

    fn minimal() -> Value {
        json!({ "description": "find auth", "prompt": "search the codebase" })
    }

    fn text_of(result: &ToolExecResult) -> String {
        match result.output.as_ref().expect("tool produced output") {
            ToolOutput::Plain(text) | ToolOutput::Markdown(text) => text.text.clone(),
            other => panic!("unexpected output: {other:?}"),
        }
    }

    fn outcome(task_id: Option<&str>, verdict: Result<Value, &str>) -> TaskOutcome {
        TaskOutcome {
            task_id: task_id.map(str::to_owned),
            success: verdict.is_ok(),
            cancelled: false,
            output: verdict.clone().unwrap_or(Value::Null),
            error: verdict.err().map(str::to_owned),
            tokens_used: 0,
            duration_ms: 0,
        }
    }

    fn metadata(task_id: &str) -> String {
        format!("<task_metadata>\ntask_id: {task_id}\n</task_metadata>")
    }

    #[test]
    fn a_minimal_call_parses() {
        assert!(parse(minimal()).is_ok());
    }

    #[test_case("model_spec"; "model")]
    #[test_case("system"; "system_prompt")]
    #[test_case("tools"; "tool_list")]
    #[test_case("audience"; "audience")]
    fn a_field_the_task_derives_itself_is_rejected(field: &str) {
        let mut input = minimal();
        input[field] = json!("anything");
        assert!(
            parse(input).is_err(),
            "{field} must not be caller-controlled"
        );
    }

    /// Resuming an interrupted subagent has nothing new to say, and inventing
    /// a prompt for it would land a fake instruction in the child's history.
    #[test]
    fn a_continuation_may_omit_the_prompt() {
        let input = json!({ "description": "find auth", "task_id": TASK_ID });
        assert!(parse(input).is_ok());
    }

    #[test]
    fn a_new_task_without_a_prompt_is_rejected() {
        let input = json!({ "description": "find auth" });
        let Err(error) = parse(input) else {
            panic!("a new task without a prompt was accepted");
        };
        assert!(error.to_string().contains(PROMPT_REQUIRED_ERROR));
    }

    #[test]
    fn an_unknown_mode_is_rejected() {
        let mut input = minimal();
        input["mode"] = json!("yolo");
        assert!(parse(input).is_err());
    }

    #[test_case("plan", SubagentTaskMode::Plan)]
    #[test_case("build", SubagentTaskMode::Build)]
    fn every_declared_mode_parses(raw: &str, expected: SubagentTaskMode) {
        assert_eq!(parse_mode(raw).expect("known mode"), expected);
    }

    /// A mode mismatch on a continuation reports the stored and requested
    /// modes through `Display`, so what it names has to be a value the model
    /// can pass straight back.
    #[test_case(SubagentTaskMode::Plan)]
    #[test_case(SubagentTaskMode::Build)]
    fn a_rendered_mode_is_one_the_tool_accepts(mode: SubagentTaskMode) {
        let rendered = mode.to_string();
        assert!(MODES.contains(&rendered.as_str()));
        assert_eq!(parse_mode(&rendered).expect("rendered mode"), mode);
    }

    #[test]
    fn a_structured_value_is_returned_as_compact_json() {
        let value = json!({ REQUIRED_FIELD: SUMMARY });
        let result = render(outcome(Some(TASK_ID), Ok(value.clone())));
        assert!(!result.is_error);
        assert_eq!(text_of(&result), value.to_string());
        assert_eq!(
            result.model_suffix.as_deref(),
            Some(metadata(TASK_ID).as_str())
        );
    }

    #[test]
    fn a_summary_is_returned_verbatim() {
        let result = render(outcome(Some(TASK_ID), Ok(Value::String(SUMMARY.into()))));
        assert!(!result.is_error);
        assert_eq!(text_of(&result), SUMMARY);
    }

    #[test]
    fn a_failure_still_carries_the_task_id_so_it_can_be_resumed() {
        let result = render(outcome(Some(TASK_ID), Err(BOOM)));
        assert!(result.is_error);
        assert_eq!(text_of(&result), BOOM);
        assert_eq!(
            result.model_suffix.as_deref(),
            Some(metadata(TASK_ID).as_str())
        );
    }

    /// Nothing was opened, so there is nothing to resume and no id to offer.
    #[test]
    fn a_failure_before_any_session_carries_no_metadata() {
        let result = render(outcome(None, Err(BOOM)));
        assert!(result.is_error);
        assert_eq!(result.model_suffix, None);
    }
}
