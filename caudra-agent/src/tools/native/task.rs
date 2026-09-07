//! `task`: run an autonomous subagent and hand its result back to the caller.
//!
//! The subagent machinery lives in [`crate::agent::subagent`]; this tool owns
//! the parts specific to delegation: the optional `output_schema` contract,
//! the nudges that get a silent subagent to actually report, and the
//! process-wide cap on how many subagents run at once.
//!
//! Structured output is a session-local tool. Its handler validates against
//! the caller's schema and captures the value, so invalid input becomes an
//! inline tool error the subagent can fix within the same run instead of a
//! failure the parent has to retry.

use std::borrow::Cow;
use std::sync::{Arc, LazyLock, Mutex};

use arc_swap::ArcSwap;
use async_lock::Semaphore;
use jsonschema::Validator;
use serde_json::Value;

use crate::agent::subagent::{self, STRUCTURED_OUTPUT_TOOL, Subagent};
use crate::subagent_history::SubagentTaskMode;
use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolExecResult, ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{
    DescriptionContext, LocalToolFn, LocalTools, ToolAudience, ToolContext, ToolEffect,
};
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
4. Every result, success or failure, carries a task_id. Pass it back to continue that subagent with its previous messages and tool outputs, sending only the new work. Omit mode and profile when continuing; they stay locked to the original run.
5. Tell it to return concise summaries with file:line refs, not full file contents.
";

const STRUCTURED_OUTPUT_DESCRIPTION: &str =
    "Report your final result. Call it exactly once when your task is complete.";
const STRUCTURED_OUTPUT_ACK: &str = "Output recorded.";
const STRUCTURED_OUTPUT_PROMPT_SUFFIX: &str =
    "\n\nWhen finished, call the structured_output tool with your final result.";
const MAX_NUDGES: usize = 2;
const MAX_SCHEMA_ERRORS: usize = 3;
const SCHEMA_COMPILE_ERROR: &str = "invalid output_schema";
const SCHEMA_ROOT_ERROR: &str = "output_schema must have type object";
const STRUCTURED_MISSING_ERROR: &str = "subagent finished without calling structured_output";
const STRUCTURED_INVALID_ERROR: &str = "subagent result does not match output_schema";
const SUMMARY_MISSING_ERROR: &str = "subagent finished without providing a summary";
const TASK_METADATA_FORMAT: &str = "<task_metadata>\ntask_id: {task_id}\n</task_metadata>";
const TASK_ID_PLACEHOLDER: &str = "{task_id}";
const NUDGE_MISSING: &str = "You did not call the structured_output tool. Call it now with your final result matching its input schema.";
const NUDGE_SUMMARY: &str = "You finished your work but did not provide a summary. Reply with a concise summary of what you did and found.";
const INVALID_INPUT_PREFIX: &str =
    "Input does not match the required schema. Fix the errors and call structured_output again:\n";
const INTERRUPTED_PREFIX: &str = "sub-agent interrupted (";
const INTERRUPTED_SUFFIX: &str = "). Partial output:\n";
const ERROR_PREFIX: &str = "sub-agent error: ";

const MODES: &[&str] = &["plan", "build"];

/// Process-wide cap on concurrently running subagents. Sized once from config
/// before any tool runs; `caudra_config::DEFAULT_TASK_MAX_CONCURRENT` until
/// then, so a test or embedder that never configures still gets a bound.
static PERMITS: LazyLock<ArcSwap<Semaphore>> = LazyLock::new(|| {
    ArcSwap::from_pointee(Semaphore::new(caudra_config::DEFAULT_TASK_MAX_CONCURRENT))
});

pub fn set_max_concurrent(limit: usize) {
    PERMITS.store(Arc::new(Semaphore::new(limit.max(1))));
}

static DESCRIPTION_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Short (3-5 words) description of the task",
};
static PROMPT_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Detailed task prompt for the agent",
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
    ("prompt", &PROMPT_PARAM, true, &[]),
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
        Ok(Box::new(TaskCall {
            description: field("description")
                .ok_or_else(|| ParseError::custom("description is required"))?,
            prompt: field("prompt").ok_or_else(|| ParseError::custom("prompt is required"))?,
            task_id: field("task_id"),
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
    prompt: String,
    task_id: Option<String>,
    profile: Option<String>,
    mode: Option<SubagentTaskMode>,
    output_schema: Option<Value>,
}

/// What the subagent reported through `structured_output`, plus the most
/// recent validation failure. Shared with the local tool's handler, which runs
/// on the subagent's turn while this call awaits it.
#[derive(Default)]
struct Captured {
    value: Option<Value>,
    last_errors: Option<String>,
}

impl ToolInvocation for TaskCall {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(self.description.clone()))
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            // Compile early: a bad schema costs zero tokens.
            let captured = Arc::new(Mutex::new(Captured::default()));
            let local_tools = match self.output_schema.as_ref() {
                None => None,
                Some(schema) => match structured_output_tool(schema, &captured) {
                    Ok(tools) => Some(tools),
                    Err(message) => return error(message),
                },
            };
            let _permit = PERMITS.load().acquire_arc().await;
            self.run(ctx, local_tools, &captured).await
        })
    }
}

impl TaskCall {
    async fn run(
        self: Box<Self>,
        ctx: &ToolContext,
        local_tools: Option<(Value, LocalTools)>,
        captured: &Mutex<Captured>,
    ) -> ToolExecResult {
        let validating = local_tools.is_some();
        let (local_definitions, local_tools) = match local_tools {
            Some((definition, tools)) => (vec![definition], tools),
            None => (Vec::new(), LocalTools::default()),
        };
        let mut session = match subagent::open_task(
            ctx,
            subagent::TaskOptions {
                name: self.description.clone(),
                task_id: self.task_id.clone(),
                profile: self.profile.clone(),
                mode: self.mode,
                local_definitions,
                local_tools,
            },
        )
        .await
        {
            Ok(session) => session,
            Err(message) => return error(message),
        };
        let task_id = session.id().to_owned();

        let outcome = self.converse(&mut session, validating, captured).await;
        session.close();
        with_task_id(&task_id, outcome)
    }

    /// Prompts, then nudges a subagent that finished without reporting. A
    /// nudge only makes sense while the run is healthy, so any error ends the
    /// loop immediately.
    async fn converse(
        &self,
        session: &mut Subagent,
        validating: bool,
        captured: &Mutex<Captured>,
    ) -> ToolExecResult {
        let mut message = self.prompt.clone();
        if validating {
            message.push_str(STRUCTURED_OUTPUT_PROMPT_SUFFIX);
        }
        let mut result = session.prompt(message).await;
        for _ in 0..MAX_NUDGES {
            let Ok(reply) = &result else { break };
            let Some(nudge) = nudge_for(validating, lock(captured).value.is_some(), &reply.text)
            else {
                break;
            };
            result = session.prompt(nudge.to_owned()).await;
        }

        let text = match result {
            Ok(result) => result.text,
            Err(failure) => return error(failure_message(failure)),
        };
        report(validating, std::mem::take(&mut *lock(captured)), text)
    }
}

/// What to say to a subagent that finished without reporting, or `None` when
/// it already has. Splitting the run's only decision out of the loop is what
/// makes the nudge policy testable without an agent behind it.
fn nudge_for(validating: bool, reported: bool, text: &str) -> Option<&'static str> {
    match validating {
        true if !reported => Some(NUDGE_MISSING),
        false if text.is_empty() => Some(NUDGE_SUMMARY),
        _ => None,
    }
}

/// A result alongside the error means the run was cut short after streaming
/// some text, and half a transcript beats a bare error.
fn failure_message(failure: subagent::PromptFailure) -> String {
    match failure.partial {
        Some(partial) => format!(
            "{INTERRUPTED_PREFIX}{}{INTERRUPTED_SUFFIX}{partial}",
            failure.error
        ),
        None => format!("{ERROR_PREFIX}{}", failure.error),
    }
}

/// The subagent's verdict, once it has stopped talking. A schema contract is
/// answered by `captured` alone; without one the transcript is the answer.
fn report(validating: bool, captured: Captured, text: String) -> ToolExecResult {
    match (validating, captured.value) {
        (true, Some(value)) => markdown(value.to_string()),
        (true, None) => error(match captured.last_errors {
            Some(errors) => format!("{STRUCTURED_INVALID_ERROR}:\n{errors}"),
            None => STRUCTURED_MISSING_ERROR.to_owned(),
        }),
        (false, _) if text.is_empty() => error(SUMMARY_MISSING_ERROR.to_owned()),
        (false, _) => markdown(text),
    }
}

/// Builds the session-local `structured_output` tool from the caller's schema.
/// The handler is the only writer of `captured`, and it runs on the subagent's
/// turn, which is why the state is shared rather than returned.
fn structured_output_tool(
    schema: &Value,
    captured: &Arc<Mutex<Captured>>,
) -> Result<(Value, LocalTools), String> {
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(SCHEMA_ROOT_ERROR.to_owned());
    }
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| format!("{SCHEMA_COMPILE_ERROR}: {error}"))?;
    let definition = serde_json::json!({
        "name": STRUCTURED_OUTPUT_TOOL,
        "description": STRUCTURED_OUTPUT_DESCRIPTION,
        "input_schema": schema,
    });
    let captured = Arc::clone(captured);
    let handler: LocalToolFn =
        crate::tools::audited_local_tool(ToolEffect::ReadOnly, move |input, _ctx| {
            let result = record(&validator, &captured, input);
            Box::pin(async move { result })
        });
    Ok((
        definition,
        Arc::new([(STRUCTURED_OUTPUT_TOOL.to_owned(), handler)].into()),
    ))
}

fn record(
    validator: &Validator,
    captured: &Mutex<Captured>,
    input: Value,
) -> Result<String, String> {
    let errors: Vec<String> = validator
        .iter_errors(&input)
        .take(MAX_SCHEMA_ERRORS)
        .map(|error| error.to_string())
        .collect();
    if errors.is_empty() {
        lock(captured).value = Some(input);
        return Ok(STRUCTURED_OUTPUT_ACK.to_owned());
    }
    let errors = errors.join("\n");
    lock(captured).last_errors = Some(errors.clone());
    Err(format!("{INVALID_INPUT_PREFIX}{errors}"))
}

/// The captured state is only ever touched between awaits, so a poisoned lock
/// would mean a panic mid-update: recovering the guard keeps a failed
/// subagent from poisoning the whole tool.
fn lock(captured: &Mutex<Captured>) -> std::sync::MutexGuard<'_, Captured> {
    captured
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The task ID is what makes a continuation possible, so it rides along with
/// failures too: an interrupted subagent is still resumable.
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
    const PARTIAL: &str = "half a transcript";
    const BOOM: &str = "boom";

    fn answer_schema() -> Value {
        json!({
            "type": "object",
            "required": [REQUIRED_FIELD],
            "properties": { REQUIRED_FIELD: { "type": "string" } },
        })
    }

    /// Three required strings, so a single empty object over-runs
    /// `MAX_SCHEMA_ERRORS` and the bound is actually exercised.
    fn strict_schema() -> Value {
        json!({
            "type": "object",
            "required": ["a", "b", "c", "d"],
            "properties": {
                "a": { "type": "string" },
                "b": { "type": "string" },
                "c": { "type": "string" },
                "d": { "type": "string" },
            },
        })
    }

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

    fn tool_for(schema: &Value) -> Result<(Value, LocalTools), String> {
        structured_output_tool(schema, &Arc::new(Mutex::new(Captured::default())))
    }

    /// `LocalToolEntry` holds boxed closures and cannot be `Debug`, so the
    /// error cases unwrap by hand rather than through `expect_err`.
    fn schema_error(schema: Value) -> String {
        match tool_for(&schema) {
            Err(error) => error,
            Ok(_) => panic!("schema was accepted"),
        }
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
    fn a_schema_that_is_not_an_object_is_refused_before_any_session() {
        assert_eq!(schema_error(json!({ "type": "array" })), SCHEMA_ROOT_ERROR);
    }

    #[test]
    fn an_uncompilable_schema_is_refused_before_any_session() {
        let error = schema_error(json!({ "type": "object", "properties": 7 }));
        assert!(error.starts_with(SCHEMA_COMPILE_ERROR), "got: {error}");
    }

    #[test]
    fn the_output_tool_carries_the_callers_schema_verbatim() {
        let schema = answer_schema();
        let (definition, tools) = tool_for(&schema).expect("valid schema");
        assert_eq!(definition["name"], json!(STRUCTURED_OUTPUT_TOOL));
        assert_eq!(definition["input_schema"], schema);
        assert!(tools.contains_key(STRUCTURED_OUTPUT_TOOL));
        assert_eq!(
            tools[STRUCTURED_OUTPUT_TOOL].effect,
            ToolEffect::ReadOnly,
            "reporting a result is not an effect"
        );
    }

    #[test]
    fn a_valid_report_is_captured_and_acknowledged() {
        let validator = jsonschema::validator_for(&answer_schema()).unwrap();
        let captured = Mutex::new(Captured::default());
        let value = json!({ REQUIRED_FIELD: SUMMARY });

        assert_eq!(
            record(&validator, &captured, value.clone()),
            Ok(STRUCTURED_OUTPUT_ACK.to_owned())
        );
        assert_eq!(lock(&captured).value.as_ref(), Some(&value));
    }

    #[test]
    fn an_invalid_report_is_an_inline_error_the_subagent_can_fix() {
        let validator = jsonschema::validator_for(&answer_schema()).unwrap();
        let captured = Mutex::new(Captured::default());

        let error = record(&validator, &captured, json!({})).expect_err("missing required field");

        assert!(error.starts_with(INVALID_INPUT_PREFIX), "got: {error}");
        assert!(
            lock(&captured).value.is_none(),
            "an invalid report is not captured"
        );
        assert!(
            lock(&captured).last_errors.is_some(),
            "the failure is remembered for the final verdict"
        );
    }

    #[test]
    fn reported_schema_errors_are_bounded() {
        let validator = jsonschema::validator_for(&strict_schema()).unwrap();
        let captured = Mutex::new(Captured::default());

        record(&validator, &captured, json!({})).expect_err("four fields missing");

        let errors = lock(&captured)
            .last_errors
            .clone()
            .expect("errors recorded");
        assert_eq!(errors.lines().count(), MAX_SCHEMA_ERRORS);
    }

    #[test]
    fn a_later_valid_report_supersedes_an_earlier_failure() {
        let validator = jsonschema::validator_for(&answer_schema()).unwrap();
        let captured = Mutex::new(Captured::default());
        record(&validator, &captured, json!({})).expect_err("first attempt is invalid");

        record(&validator, &captured, json!({ REQUIRED_FIELD: SUMMARY })).expect("second attempt");

        let captured = std::mem::take(&mut *lock(&captured));
        assert!(!report(true, captured, String::new()).is_error);
    }

    #[test_case(true, false, "", Some(NUDGE_MISSING); "schema_contract_unmet")]
    #[test_case(true, true, "", None; "schema_contract_met")]
    #[test_case(false, false, "", Some(NUDGE_SUMMARY); "silent_without_a_schema")]
    #[test_case(false, false, SUMMARY, None; "summarised_without_a_schema")]
    #[test_case(true, true, SUMMARY, None; "reported_and_summarised")]
    fn a_subagent_is_nudged_only_when_it_owes_a_report(
        validating: bool,
        reported: bool,
        text: &str,
        expected: Option<&str>,
    ) {
        assert_eq!(nudge_for(validating, reported, text), expected);
    }

    #[test]
    fn a_captured_value_is_returned_as_json() {
        let captured = Captured {
            value: Some(json!({ REQUIRED_FIELD: SUMMARY })),
            last_errors: None,
        };
        let result = report(true, captured, String::new());
        assert!(!result.is_error);
        assert_eq!(
            text_of(&result),
            json!({ REQUIRED_FIELD: SUMMARY }).to_string()
        );
    }

    #[test]
    fn a_silent_subagent_under_a_schema_reports_what_it_got_wrong() {
        let errors = "answer is a required property";
        let captured = Captured {
            value: None,
            last_errors: Some(errors.into()),
        };
        let result = report(true, captured, SUMMARY.into());
        assert!(result.is_error);
        assert_eq!(
            text_of(&result),
            format!("{STRUCTURED_INVALID_ERROR}:\n{errors}")
        );
    }

    #[test]
    fn a_subagent_that_never_called_the_output_tool_says_so() {
        let result = report(true, Captured::default(), SUMMARY.into());
        assert!(result.is_error);
        assert_eq!(text_of(&result), STRUCTURED_MISSING_ERROR);
    }

    #[test]
    fn a_summary_is_returned_verbatim_without_a_schema() {
        let result = report(false, Captured::default(), SUMMARY.into());
        assert!(!result.is_error);
        assert_eq!(text_of(&result), SUMMARY);
    }

    #[test]
    fn a_subagent_that_says_nothing_at_all_is_an_error() {
        let result = report(false, Captured::default(), String::new());
        assert!(result.is_error);
        assert_eq!(text_of(&result), SUMMARY_MISSING_ERROR);
    }

    #[test]
    fn an_interrupted_run_keeps_the_partial_transcript() {
        let message = failure_message(subagent::PromptFailure {
            error: BOOM.into(),
            partial: Some(PARTIAL.into()),
        });
        assert_eq!(
            message,
            format!("{INTERRUPTED_PREFIX}{BOOM}{INTERRUPTED_SUFFIX}{PARTIAL}")
        );
    }

    #[test]
    fn a_run_that_streamed_nothing_reports_the_bare_error() {
        let message = failure_message(subagent::PromptFailure {
            error: BOOM.into(),
            partial: None,
        });
        assert_eq!(message, format!("{ERROR_PREFIX}{BOOM}"));
    }

    #[test]
    fn a_failure_still_carries_the_task_id_so_it_can_be_resumed() {
        let result = with_task_id(TASK_ID, error(BOOM.into()));
        assert!(result.is_error);
        assert_eq!(
            result.model_suffix.as_deref(),
            Some(format!("<task_metadata>\ntask_id: {TASK_ID}\n</task_metadata>").as_str())
        );
    }
}
