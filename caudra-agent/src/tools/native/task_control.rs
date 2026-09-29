use std::borrow::Cow;

use caudra_config::{ExecutionMode, effective_task_execution};
use caudra_storage::background::JobOwner;
use serde_json::{Value, json};

use crate::{
    ToolOutput,
    tools::{
        DescriptionContext, ToolAudience, ToolContext,
        registry::{
            ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolError, ToolExecResult,
            ToolFailure, ToolInvocation,
        },
    },
};

pub const DESCRIPTION: &str = "Inspect or control jobs visible to this owner. Actions: list, status, cancel. Use status when details are needed, not as a polling loop.";
const PROMOTION_GUIDANCE: &str =
    "The background action promotes a running foreground task without restarting it.";
const PROMOTION_DENIED: &str =
    "task promotion requires agent.task_execution = auto and a supported session";

pub fn configure_execution(definition: &mut Value, task_mode: Option<&ExecutionMode>) {
    let promote = task_mode == Some(&ExecutionMode::Auto);
    definition["description"] = if promote {
        json!(format!("{DESCRIPTION} {PROMOTION_GUIDANCE}"))
    } else {
        json!(DESCRIPTION)
    };
    definition["input_schema"] = schema(promote);
}

fn schema(promote: bool) -> Value {
    let actions = if promote {
        json!(["list", "status", "cancel", "background"])
    } else {
        json!(["list", "status", "cancel"])
    };
    json!({"type":"object","additionalProperties":false,"properties":{
        "action":{"type":"string","enum":actions},
        "task_id":{"type":"string","description":"Required except for list."}
    },"required":["action"]})
}

pub struct TaskControl;
struct ControlCall {
    action: String,
    task_id: Option<String>,
}

impl Tool for TaskControl {
    fn name(&self) -> &str {
        "task_control"
    }
    fn description(&self, _: &DescriptionContext) -> Cow<'_, str> {
        Cow::Owned(format!("{DESCRIPTION} {PROMOTION_GUIDANCE}"))
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
        if object.keys().any(|key| key != "action" && key != "task_id") {
            return Err(ParseError::custom("unknown task control field"));
        }
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| ParseError::custom("action is required"))?;
        if !matches!(action, "list" | "status" | "cancel" | "background") {
            return Err(ParseError::custom("unknown task action"));
        }
        let task_id = input
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if action != "list" && task_id.is_none() {
            return Err(ParseError::custom("task_id is required"));
        }
        Ok(Box::new(ControlCall {
            action: action.into(),
            task_id,
        }))
    }
}

impl ToolInvocation for ControlCall {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(self.action.clone()))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            let result = async {
                if self.action == "background"
                    && (effective_task_execution(&ctx.config, ctx.background.is_some())
                        != Some(ExecutionMode::Auto)
                        || ctx
                            .jobs
                            .as_ref()
                            .is_some_and(|jobs| *jobs.owner() != JobOwner::Main))
                {
                    return Err(ToolError::new(ToolFailure::Denied, PROMOTION_DENIED));
                }
                let id = self.task_id.as_deref().unwrap_or_default();
                if self.action != "background"
                    && let Some(jobs) = &ctx.jobs
                {
                    let cards = match self.action.as_str() {
                        "list" => jobs.list(),
                        "status" => vec![jobs.status(id).map_err(invisible)?],
                        _ => {
                            jobs.status(id).map_err(invisible)?;
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
                    "status" => vec![tasks.status(id).map_err(invisible)?],
                    action => {
                        tasks.status(id).map_err(invisible)?;
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
                Ok(output) => ToolExecResult::from(Ok(output)),
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
    use super::{DESCRIPTION, PROMOTION_DENIED, TaskControl, configure_execution};
    use crate::AgentMode;
    use crate::tools::registry::{Tool, ToolFailure};
    use crate::tools::test_support::stub_ctx;
    use caudra_config::ExecutionMode;
    use serde_json::json;
    use test_case::test_case;

    #[test_case(None; "shell_only")]
    #[test_case(Some(ExecutionMode::Sync); "sync")]
    #[test_case(Some(ExecutionMode::Auto); "auto")]
    #[test_case(Some(ExecutionMode::Async); "async_mode")]
    fn promotion_only_published_in_auto(mode: Option<ExecutionMode>) {
        let mut definition = json!({"name":"task_control"});
        configure_execution(&mut definition, mode.as_ref());
        let promote = mode == Some(ExecutionMode::Auto);
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

    #[test_case(ExecutionMode::Sync)]
    #[test_case(ExecutionMode::Async)]
    fn stale_promotion_is_denied(mode: ExecutionMode) {
        smol::block_on(async {
            let mut ctx = stub_ctx(&AgentMode::Build);
            ctx.config.task_execution = mode;
            let call = TaskControl
                .parse(&json!({"action":"background", "task_id":"unknown"}))
                .unwrap();
            let result = call.execute(&ctx).await;
            assert_eq!(result.failure, Some(ToolFailure::Denied));
            assert_eq!(result.output.err().as_deref(), Some(PROMOTION_DENIED));
        });
    }
}
