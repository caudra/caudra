use std::borrow::Cow;

use caudra_storage::background::{BackgroundCursor, JobOwner, MAX_HISTORY_PAGE};
use serde_json::{Value, json};

use crate::{
    AgentEvent, TaskCard, ToolOutput,
    background::{MAX_REQUEST_BYTES, guidance_text},
    tools::{
        DescriptionContext, ToolAudience, ToolContext,
        registry::{
            ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolEffect, ToolError,
            ToolExecResult, ToolFailure, ToolInvocation,
        },
    },
};

pub const DESCRIPTION: &str = "Inspect or control jobs visible to this owner. Actions: list, status, cancel. List returns resident jobs and a bounded history page; pass next as before to read older history. Use status when details are needed, not as a polling loop.";
const PROMOTION_GUIDANCE: &str =
    "The background action promotes a running foreground task without restarting it.";
const GUIDE_GUIDANCE: &str = "The guide action sends a running agent task a message, such as updated requirements or a reply to its report, which it reads at its next step; interrupt stops its current turn and its jobs first. Guidance it never reads comes back with its result.";
const PROMOTION_DENIED: &str = "task promotion requires a session that supports background tasks";
const GUIDANCE_DENIED: &str = "guidance requires a session that runs agent tasks";
const GUIDANCE_RECEIPT: &str = "Accepted, not yet read: the task reads it at its next step. Guidance it never reads comes back with its result.";
const MESSAGE_REQUIRED: &str = "guide requires a nonempty message";
const MESSAGE_TOO_LARGE: &str = "guidance message is too large";
const GUIDE_FIELDS_REQUIRE_GUIDE: &str = "message and interrupt require guide";
const INTERRUPT_NOT_BOOLEAN: &str = "interrupt must be a boolean";
const GUIDE: &str = "guide";
const REPLACE: &str = "replace";
const GUIDANCE_QUEUED: &str = "queued";
const GUIDANCE_REPLACING: &str = "replacing";
/// The actions that only report on jobs, so their cards fold like any lookup's.
const READ_ACTIONS: &[&str] = &["list", "status"];

pub fn configure_execution(definition: &mut Value, promote: bool) {
    definition["description"] = if promote {
        json!(main_description())
    } else {
        json!(DESCRIPTION)
    };
    definition["input_schema"] = schema(promote);
}

fn main_description() -> String {
    format!("{DESCRIPTION} {PROMOTION_GUIDANCE} {GUIDE_GUIDANCE}")
}

fn schema(promote: bool) -> Value {
    let mut schema = json!({"type":"object","additionalProperties":false,"properties":{
        "action":{"type":"string","enum":["list", "status", "cancel"]},
        "task_id":{"type":"string","description":"Required except for list."},
        "before":{"type":"object","additionalProperties":false,"properties":{"sequence":{"type":"integer","minimum":0},"invocation_id":{"type":"string"}},"required":["sequence","invocation_id"],"description":"History cursor returned as next by list."},
        "limit":{"type":"integer","minimum":1,"maximum":MAX_HISTORY_PAGE}
    },"required":["action"]});
    if promote {
        let properties = &mut schema["properties"];
        properties["action"]["enum"] = json!(["list", "status", "cancel", "background", GUIDE]);
        properties["message"] = json!({"type":"string","minLength":1,"maxLength":MAX_REQUEST_BYTES,"description":"Required for guide."});
        properties["interrupt"] = json!({"type":"boolean","default":false,"description":"guide only: stop the task's current turn and its jobs, then run its next turn on this message."});
    }
    schema
}

pub struct TaskControl;
struct ControlCall {
    action: String,
    task_id: Option<String>,
    before: Option<BackgroundCursor>,
    limit: usize,
    message: Option<String>,
    interrupt: bool,
}

impl Tool for TaskControl {
    fn name(&self) -> &str {
        "task_control"
    }
    fn description(&self, _: &DescriptionContext) -> Cow<'_, str> {
        Cow::Owned(main_description())
    }
    fn audience(&self) -> ToolAudience {
        ToolAudience::MAIN | ToolAudience::RESEARCH_SUB | ToolAudience::GENERAL_SUB
    }
    fn schema(&self) -> Value {
        schema(true)
    }
    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let object = input
            .as_object()
            .ok_or_else(|| ParseError::custom("task control input must be an object"))?;
        if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "action" | "task_id" | "before" | "limit" | "message" | "interrupt"
            )
        }) {
            return Err(ParseError::custom("unknown task control field"));
        }
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| ParseError::custom("action is required"))?;
        if !matches!(action, "list" | "status" | "cancel" | "background" | GUIDE) {
            return Err(ParseError::custom("unknown task action"));
        }
        if action != GUIDE && (input.get("message").is_some() || input.get("interrupt").is_some()) {
            return Err(ParseError::custom(GUIDE_FIELDS_REQUIRE_GUIDE));
        }
        let message = input
            .get("message")
            .and_then(Value::as_str)
            .filter(|message| !message.trim().is_empty());
        if action == GUIDE && message.is_none() {
            return Err(ParseError::custom(MESSAGE_REQUIRED));
        }
        if message.is_some_and(|message| message.len() > MAX_REQUEST_BYTES) {
            return Err(ParseError::custom(MESSAGE_TOO_LARGE));
        }
        let interrupt = match input.get("interrupt") {
            None => false,
            Some(value) => value
                .as_bool()
                .ok_or_else(|| ParseError::custom(INTERRUPT_NOT_BOOLEAN))?,
        };
        let task_id = input
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if action != "list" && task_id.is_none() {
            return Err(ParseError::custom("task_id is required"));
        }
        let before = input
            .get("before")
            .map(|value| serde_json::from_value::<BackgroundCursor>(value.clone()))
            .transpose()
            .map_err(|error| ParseError::custom(error.to_string()))?;
        let limit = match input.get("limit") {
            None => MAX_HISTORY_PAGE,
            Some(value) => value
                .as_u64()
                .filter(|limit| (1..=MAX_HISTORY_PAGE as u64).contains(limit))
                .ok_or_else(|| ParseError::custom("invalid history page limit"))?
                as usize,
        };
        if action != "list" && (before.is_some() || input.get("limit").is_some()) {
            return Err(ParseError::custom("history cursor and limit require list"));
        }
        Ok(Box::new(ControlCall {
            action: action.into(),
            task_id,
            before,
            limit,
            message: message.map(str::to_owned),
            interrupt,
        }))
    }
}

impl ToolInvocation for ControlCall {
    fn start_header(&self) -> HeaderFuture {
        let verb = match self.interrupt {
            true => REPLACE,
            false => &self.action,
        };
        HeaderFuture::Ready(HeaderResult::plain(match &self.task_id {
            Some(id) => format!("{verb} {id}"),
            None => verb.to_owned(),
        }))
    }
    fn call_effect(&self, registered: ToolEffect) -> ToolEffect {
        match READ_ACTIONS.contains(&self.action.as_str()) {
            true => ToolEffect::ReadOnly,
            false => registered,
        }
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            let mut model_output = None;
            let result = async {
                if self.action == "background"
                    && (ctx.background.is_none()
                        || ctx
                            .jobs
                            .as_ref()
                            .is_some_and(|jobs| *jobs.owner() != JobOwner::Main))
                {
                    return Err(ToolError::new(ToolFailure::Denied, PROMOTION_DENIED));
                }
                if self.action == GUIDE {
                    let tasks = match &ctx.background {
                        Some(tasks)
                            if ctx
                                .jobs
                                .as_ref()
                                .is_none_or(|jobs| *jobs.owner() == JobOwner::Main) =>
                        {
                            tasks
                        }
                        _ => return Err(ToolError::new(ToolFailure::Denied, GUIDANCE_DENIED)),
                    };
                    let id = self.task_id.as_deref().unwrap_or_default();
                    let message = self.message.as_deref().unwrap_or_default();
                    tasks.status_async(id).await.map_err(invisible)?;
                    let (card, queued) = tasks.guide(id, message, self.interrupt).await?;
                    ctx.event_tx.try_send(AgentEvent::TaskGuidanceQueued {
                        task_id: card.task_id.clone(),
                        id: queued.id,
                        text: guidance_text(message),
                        interrupt: self.interrupt,
                        superseded: queued.superseded,
                    });
                    let guidance = match self.interrupt {
                        true => GUIDANCE_REPLACING,
                        false => GUIDANCE_QUEUED,
                    };
                    model_output = Some(
                        json!({"task_id":card.task_id,"state":card.state,"guidance":guidance,"note":GUIDANCE_RECEIPT})
                            .to_string(),
                    );
                    return Ok(ToolOutput::Tasks(vec![card]));
                }
                if self.action == "list" {
                    let first = self.before.is_none();
                    let (mut cards, page) = if let Some(jobs) = &ctx.jobs {
                        (
                            if first { jobs.list() } else { Vec::new() },
                            jobs.history_page(self.before, self.limit).await?,
                        )
                    } else {
                        let tasks = ctx
                            .background
                            .as_ref()
                            .ok_or("task controls require a supported session")?;
                        (
                            if first { tasks.list() } else { Vec::new() },
                            tasks.history_page(self.before, self.limit).await?,
                        )
                    };
                    if page.tasks.is_empty() && page.next.is_none() {
                        return Ok(ToolOutput::Tasks(cards));
                    }
                    for card in page.tasks {
                        if !cards
                            .iter()
                            .any(|existing| existing.invocation_id == card.invocation_id)
                        {
                            cards.push(card);
                        }
                    }
                    model_output = Some(
                        json!({"tasks":cards.iter().map(TaskCard::model_value).collect::<Vec<_>>(), "next":page.next}).to_string(),
                    );
                    return Ok(ToolOutput::Tasks(cards));
                }
                let id = self.task_id.as_deref().unwrap_or_default();
                if self.action != "background"
                    && let Some(jobs) = &ctx.jobs
                {
                    let cards = match self.action.as_str() {
                        "list" => jobs.list(),
                        "status" => vec![jobs.status_async(id).await.map_err(invisible)?],
                        _ => {
                            jobs.status_async(id).await.map_err(invisible)?;
                            vec![jobs.cancel(id).await?]
                        }
                    };
                    return Ok(ToolOutput::Tasks(cards));
                }
                let tasks = ctx
                    .background
                    .as_ref()
                    .ok_or("task controls require a supported session")?;
                let cards = match self.action.as_str() {
                    "list" => tasks.list(),
                    "status" => vec![tasks.status_async(id).await.map_err(invisible)?],
                    action => {
                        tasks.status_async(id).await.map_err(invisible)?;
                        vec![match action {
                            "cancel" => tasks.cancel(id).await?,
                            _ => tasks.promote(id).await?,
                        }]
                    }
                };
                Ok(ToolOutput::Tasks(cards))
            }
            .await;
            match result {
                Ok(output) => ToolExecResult::from(Ok(output)).with_model_output(model_output),
                Err(error) => ToolExecResult::failed(error.failure, error.message),
            }
        })
    }
}

/// A job this owner cannot see reads as one that does not exist.
fn invisible(message: String) -> ToolError {
    ToolError::new(ToolFailure::NotFound, message)
}

#[cfg(test)]
mod tests {
    use super::{
        DESCRIPTION, GUIDANCE_DENIED, GUIDE, GUIDE_FIELDS_REQUIRE_GUIDE, INTERRUPT_NOT_BOOLEAN,
        MAX_REQUEST_BYTES, MESSAGE_REQUIRED, MESSAGE_TOO_LARGE, PROMOTION_DENIED, TaskControl,
        configure_execution,
    };
    use crate::AgentMode;
    use crate::tools::ToolEffect;
    use crate::tools::registry::{Tool, ToolFailure};
    use crate::tools::schema::ToolInputErrorKind;
    use crate::tools::test_support::stub_ctx;
    use serde_json::{Value, json};
    use test_case::test_case;

    const GUIDANCE: &str = "Also cover the error path.";

    #[test_case(false; "shell_only")]
    #[test_case(true; "tasks_supported")]
    fn promotion_only_published_with_background_tasks(promote: bool) {
        let mut definition = json!({"name":"task_control"});
        configure_execution(&mut definition, promote);
        assert_eq!(definition.to_string().contains("background"), promote);
        assert_eq!(definition.to_string().contains("foreground"), promote);
        assert!(
            definition["description"]
                .as_str()
                .unwrap()
                .contains(DESCRIPTION)
        );
        for action in ["list", "status", "cancel"] {
            assert!(
                definition["input_schema"]["properties"]["action"]["enum"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(action))
            );
        }
    }

    #[test]
    fn promotion_without_background_tasks_is_denied() {
        smol::block_on(async {
            let ctx = stub_ctx(&AgentMode::Build);
            let call = TaskControl
                .parse(&json!({"action":"background", "task_id":"unknown"}))
                .unwrap();
            let result = call.execute(&ctx).await;
            assert_eq!(result.failure, Some(ToolFailure::Denied));
            assert_eq!(result.output.err().as_deref(), Some(PROMOTION_DENIED));
        });
    }

    /// Listing and checking fold like a file read; a cancel or a promotion
    /// keeps the effect the tool was registered with.
    #[test_case(json!({"action":"list"}) => ToolEffect::ReadOnly; "list")]
    #[test_case(json!({"action":"status", "task_id":"shell-1"}) => ToolEffect::ReadOnly; "status")]
    #[test_case(json!({"action":"cancel", "task_id":"shell-1"}) => ToolEffect::Orchestrator; "cancel")]
    #[test_case(json!({"action":"background", "task_id":"shell-1"}) => ToolEffect::Orchestrator; "background")]
    #[test_case(json!({"action":"guide", "task_id":"task-1", "message":GUIDANCE}) => ToolEffect::Orchestrator; "guide")]
    fn only_reads_take_a_read_effect(input: Value) -> ToolEffect {
        TaskControl
            .parse(&input)
            .unwrap()
            .call_effect(ToolEffect::Orchestrator)
    }

    #[test_case(false; "shell_only")]
    #[test_case(true; "tasks_supported")]
    fn guide_only_published_with_background_tasks(promote: bool) {
        let mut definition = json!({"name":"task_control"});
        configure_execution(&mut definition, promote);
        let schema = &definition["input_schema"]["properties"];
        assert_eq!(
            schema["action"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!(GUIDE)),
            promote
        );
        assert_eq!(schema.get("message").is_some(), promote);
        assert_eq!(schema.get("interrupt").is_some(), promote);
    }

    #[test_case(json!({"action":"guide", "task_id":"task-1"}), MESSAGE_REQUIRED; "missing_message")]
    #[test_case(json!({"action":"guide", "task_id":"task-1", "message":" \n"}), MESSAGE_REQUIRED; "blank_message")]
    #[test_case(json!({"action":"guide", "task_id":"task-1", "message":"x".repeat(MAX_REQUEST_BYTES + 1)}), MESSAGE_TOO_LARGE; "oversized_message")]
    #[test_case(json!({"action":"guide", "task_id":"task-1", "message":GUIDANCE, "interrupt":"yes"}), INTERRUPT_NOT_BOOLEAN; "interrupt_not_boolean")]
    #[test_case(json!({"action":"cancel", "task_id":"task-1", "message":GUIDANCE}), GUIDE_FIELDS_REQUIRE_GUIDE; "message_on_cancel")]
    #[test_case(json!({"action":"status", "task_id":"task-1", "interrupt":true}), GUIDE_FIELDS_REQUIRE_GUIDE; "interrupt_on_status")]
    fn malformed_guidance_is_refused(input: Value, expected: &str) {
        let error = TaskControl.parse(&input).err().unwrap();
        assert!(matches!(&error.kind, ToolInputErrorKind::Custom { detail } if detail == expected));
    }

    #[test]
    fn guidance_without_background_tasks_is_denied() {
        smol::block_on(async {
            let ctx = stub_ctx(&AgentMode::Build);
            let call = TaskControl
                .parse(&json!({"action":"guide", "task_id":"task-1", "message":GUIDANCE}))
                .unwrap();
            let result = call.execute(&ctx).await;
            assert_eq!(result.failure, Some(ToolFailure::Denied));
            assert_eq!(result.output.err().as_deref(), Some(GUIDANCE_DENIED));
        });
    }
}
