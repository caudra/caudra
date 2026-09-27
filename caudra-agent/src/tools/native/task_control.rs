use std::borrow::Cow;

use serde_json::{Value, json};

use crate::{
    ToolOutput,
    tools::{
        DescriptionContext, ToolAudience, ToolContext,
        registry::{
            ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolExecResult,
            ToolInvocation,
        },
    },
};

pub const DESCRIPTION: &str = "Inspect or control tasks owned by this session. Actions: list, status, cancel, background. background promotes a running foreground task without restarting it. Reports and outcomes are delivered automatically where background execution is enabled; do not repeatedly poll. An active or cancelling task cannot be resumed with task_id.";

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
        Cow::Borrowed(DESCRIPTION)
    }
    fn audience(&self) -> ToolAudience {
        ToolAudience::MAIN
    }
    fn schema(&self) -> Value {
        json!({"type":"object","additionalProperties":false,"properties":{
            "action":{"type":"string","enum":["list","status","cancel","background"]},
            "task_id":{"type":"string","description":"Required except for list."}
        },"required":["action"]})
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
                let tasks = ctx
                    .background
                    .as_ref()
                    .ok_or("background task controls are unavailable in this frontend")?;
                let id = self.task_id.as_deref().unwrap_or_default();
                let cards = match self.action.as_str() {
                    "list" => tasks.list(),
                    "status" => vec![tasks.status(id)?],
                    "cancel" => vec![tasks.cancel(id).await?],
                    _ => vec![tasks.promote(id).await?],
                };
                Ok(ToolOutput::Tasks(cards))
            }
            .await;
            ToolExecResult::from(result)
        })
    }
}
