//! `todo_write`: the model's plan for multi-step work, as a replace-all list.
//!
//! The tool itself is nearly stateless. It validates the list and hands back
//! [`ToolOutput::TodoList`]; the UI keeps the latest list per session and
//! renders the panel from it. Replace-all is what makes that safe: the newest
//! output is the whole truth, so a restored session needs no replay.

use std::borrow::Cow;

use serde_json::Value;

use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolExecResult, ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{DescriptionContext, ToolAudience, ToolContext};
use crate::types::{TodoItem, TodoPriority, TodoStatus, ToolOutput};

pub const DESCRIPTION: &str = "Create or update a structured todo list to track tasks.

**Use after EACH completed step!**

- Send the complete list each time (replace-all semantics).
- Use ONLY for multi-step work (3+ steps).
- Skip for trivial tasks.";

/// Injected into the `tool_usage` prompt slot whenever this tool is offered.
pub const TOOL_USAGE: &str = "- Use todo_write for multi-step tasks (3+ steps); update **after EACH step** (done + next in_progress), never batched at the end.";

const STATUSES: &[&str] = &["pending", "in_progress", "completed", "cancelled"];
const PRIORITIES: &[&str] = &["high", "medium", "low"];

static CONTENT_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Task description",
};
static STATUS_PARAM: ParamSchema = ParamSchema::Enum {
    variants: STATUSES,
    description: "",
};
static PRIORITY_PARAM: ParamSchema = ParamSchema::Enum {
    variants: PRIORITIES,
    description: "",
};
static ITEM_PROPERTIES: &[Property] = &[
    ("content", &CONTENT_PARAM, true, &[]),
    ("status", &STATUS_PARAM, true, &[]),
    ("priority", &PRIORITY_PARAM, false, &[]),
];
static ITEM_SCHEMA: ParamSchema = ParamSchema::Object {
    properties: ITEM_PROPERTIES,
    description: "",
    reject_unknown: false,
};
static TODOS_PARAM: ParamSchema = ParamSchema::Array {
    items: &ITEM_SCHEMA,
    description: "The updated todo list",
};
static PROPERTIES: &[Property] = &[("todos", &TODOS_PARAM, true, &[])];
static SCHEMA: ParamSchema = ParamSchema::Object {
    properties: PROPERTIES,
    description: "",
    reject_unknown: false,
};

pub struct TodoWrite;

impl Tool for TodoWrite {
    fn name(&self) -> &str {
        crate::tools::TODOWRITE_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(DESCRIPTION)
    }

    fn schema(&self) -> Value {
        to_json_schema(&SCHEMA)
    }

    fn audience(&self) -> ToolAudience {
        ToolAudience::MAIN | ToolAudience::RESEARCH_SUB | ToolAudience::GENERAL_SUB
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let input = validate(&SCHEMA, input.clone())?;
        let todos = input
            .get("todos")
            .and_then(Value::as_array)
            .ok_or_else(|| ParseError::custom("todos is required"))?
            .iter()
            .map(parse_item)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Box::new(TodoCall { todos }))
    }
}

/// `validate` has already checked the shape, so a failure here means a status
/// or priority the enum does not cover.
fn parse_item(raw: &Value) -> Result<TodoItem, ParseError> {
    let field = |name: &str| raw.get(name).and_then(Value::as_str);
    let status = field("status").ok_or_else(|| ParseError::custom("status is required"))?;
    Ok(TodoItem {
        content: field("content")
            .ok_or_else(|| ParseError::custom("content is required"))?
            .to_owned(),
        status: parse_status(status)
            .ok_or_else(|| ParseError::custom(format!("unknown status: {status}")))?,
        priority: field("priority")
            .map(|p| {
                parse_priority(p)
                    .ok_or_else(|| ParseError::custom(format!("unknown priority: {p}")))
            })
            .transpose()?
            .unwrap_or_default(),
    })
}

fn parse_status(raw: &str) -> Option<TodoStatus> {
    match raw {
        "pending" => Some(TodoStatus::Pending),
        "in_progress" => Some(TodoStatus::InProgress),
        "completed" => Some(TodoStatus::Completed),
        "cancelled" => Some(TodoStatus::Cancelled),
        _ => None,
    }
}

fn parse_priority(raw: &str) -> Option<TodoPriority> {
    match raw {
        "high" => Some(TodoPriority::High),
        "medium" => Some(TodoPriority::Medium),
        "low" => Some(TodoPriority::Low),
        _ => None,
    }
}

struct TodoCall {
    todos: Vec<TodoItem>,
}

impl ToolInvocation for TodoCall {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(format!("{} todos", self.todos.len())))
    }

    fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move { ToolExecResult::from(Ok(ToolOutput::TodoList(self.todos))) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;
    use serde_json::json;
    use test_case::test_case;

    const EMPTY_DISPLAY: &str = "No todos.";

    fn parse(input: Value) -> Result<Vec<TodoItem>, ParseError> {
        let todos = input
            .get("todos")
            .and_then(Value::as_array)
            .expect("todos array")
            .iter()
            .map(parse_item)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(todos)
    }

    fn item(content: &str, status: &str) -> Value {
        json!({ "content": content, "status": status })
    }

    #[test]
    fn a_list_round_trips_into_typed_items() {
        let todos = parse(json!({ "todos": [
            { "content": "first", "status": "in_progress", "priority": "high" },
            { "content": "second", "status": "pending" },
        ]}))
        .unwrap();
        assert_eq!(todos[0].content, "first");
        assert_eq!(todos[0].status, TodoStatus::InProgress);
        assert_eq!(todos[0].priority, TodoPriority::High);
        assert_eq!(todos[1].priority, TodoPriority::Medium, "default priority");
    }

    #[test_case("pending", TodoStatus::Pending)]
    #[test_case("in_progress", TodoStatus::InProgress)]
    #[test_case("completed", TodoStatus::Completed)]
    #[test_case("cancelled", TodoStatus::Cancelled)]
    fn every_declared_status_parses(raw: &str, expected: TodoStatus) {
        assert_eq!(parse_status(raw), Some(expected));
    }

    #[test_case("high", TodoPriority::High)]
    #[test_case("medium", TodoPriority::Medium)]
    #[test_case("low", TodoPriority::Low)]
    fn every_declared_priority_parses(raw: &str, expected: TodoPriority) {
        assert_eq!(parse_priority(raw), Some(expected));
    }

    /// The schema's enum list and the parser are separate; a variant in one and
    /// not the other would only surface as a runtime parse error.
    #[test]
    fn the_schema_enums_match_what_the_parser_accepts() {
        assert!(STATUSES.iter().all(|s| parse_status(s).is_some()));
        assert!(PRIORITIES.iter().all(|p| parse_priority(p).is_some()));
    }

    #[test]
    fn an_unknown_status_is_rejected() {
        let error = parse(json!({ "todos": [item("x", "blocked")] })).unwrap_err();
        assert!(error.to_string().contains("blocked"), "{error}");
    }

    #[test]
    fn the_schema_rejects_a_missing_status() {
        let Err(error) = TodoWrite.parse(&json!({ "todos": [{ "content": "x" }] })) else {
            panic!("a todo without a status must not parse");
        };
        assert!(error.to_string().contains("status"), "{error}");
    }

    #[test]
    fn the_header_counts_the_list() {
        let call = TodoWrite
            .parse(&json!({ "todos": [item("a", "pending")] }))
            .unwrap();
        let HeaderResult::Plain(text) = smol::block_on(call.start_header()) else {
            panic!("todo headers are plain text");
        };
        assert_eq!(text, "1 todos");
    }

    #[test]
    fn an_empty_list_is_accepted_so_the_model_can_clear_it() {
        let call = TodoWrite.parse(&json!({ "todos": [] })).unwrap();
        let out = smol::block_on(call.execute(&stub_ctx(&AgentMode::Build)));
        assert_eq!(out.output.unwrap().as_display_text(), EMPTY_DISPLAY);
    }

    /// The model gets a constant acknowledgement, not the list it just sent.
    #[test]
    fn the_model_sees_only_an_acknowledgement() {
        let call = TodoWrite
            .parse(&json!({ "todos": [item("secret plan", "pending")] }))
            .unwrap();
        let out = smol::block_on(call.execute(&stub_ctx(&AgentMode::Build)));
        assert!(!out.output.unwrap().as_text().contains("secret plan"));
    }
}
