use serde_json::{Value, json};

use crate::{
    background::TaskReporter,
    tools::{LocalToolFn, ToolEffect, audited_local_tool},
};

pub(crate) const NAME: &str = "report_to_parent";
pub(crate) const CONTRACT: &str = "Work independently within the assigned scope. Use report_to_parent sparingly for an important finding, correction, or blocker. Reports are one-way: no parent reply is expected. Continue useful independent work after reporting. If you cannot proceed without information or authority, use blocked: true to finish with the precise blocker and what is needed. Do not wait or poll for a reply. Reports do not replace the successful final result or its schema; blocked: true ends with a non-success outcome. Do not assume you received later main-conversation instructions.";

pub(crate) fn tool(reporter: TaskReporter) -> (Value, LocalToolFn) {
    let definition = json!({
        "name": NAME,
        "description": CONTRACT,
        "input_schema": {
            "type":"object", "additionalProperties":false,
            "properties": {
                "message":{"type":"string","description":"Important finding or precise blocker; not ordinary progress."},
                "blocked":{"type":"boolean","description":"End this invocation with a non-success blocked outcome. No reply is awaited."}
            },
            "required":["message"]
        }
    });
    let handler = audited_local_tool(ToolEffect::ReadOnly, move |input, ctx| {
        let reporter = reporter.clone();
        Box::pin(async move {
            let object = input.as_object().ok_or("report input must be an object")?;
            if object
                .keys()
                .any(|key| key != "message" && key != "blocked")
            {
                return Err("unknown report field".into());
            }
            let message = input
                .get("message")
                .and_then(Value::as_str)
                .ok_or("message is required")?
                .to_owned();
            let blocked = match input.get("blocked") {
                None => false,
                Some(Value::Bool(blocked)) => *blocked,
                _ => return Err("blocked must be a boolean".into()),
            };
            reporter
                .report(
                    ctx.tool_use_id
                        .ok_or("report requires tool call identity")?,
                    message,
                    blocked,
                )
                .await
        })
    });
    (definition, handler)
}
