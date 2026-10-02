use serde_json::{Value, json};

use crate::{
    background::TaskReporter,
    tools::{LocalToolFn, ToolEffect, ToolError, ToolFailure, typed_local_tool},
};

pub(crate) const NAME: &str = "report_to_parent";
pub(crate) const CONTRACT: &str = "Work independently within the assigned scope. Use report_to_parent sparingly for an important finding, correction, or blocker. Give the report a short title for its compact card and put the full finding in message. Reports are one-way: no parent reply is expected. Continue useful independent work after reporting. If you cannot proceed without information or authority, use blocked: true to finish with the precise blocker and what is needed. Do not wait or poll for a reply. Reports do not replace the successful final result or its schema; blocked: true ends with a non-success outcome. Do not assume you received later main-conversation instructions.";
const MAX_TITLE_CHARS: usize = 80;
const INVALID_TITLE: &str = "title must be a nonempty single-line string of at most 80 characters without control characters";

fn title(input: &Value) -> Result<Option<&str>, &'static str> {
    match input.get("title") {
        None => Ok(None),
        Some(Value::String(title))
            if !title.trim().is_empty()
                && title.chars().count() <= MAX_TITLE_CHARS
                && !title.chars().any(char::is_control) =>
        {
            Ok(Some(title.trim()))
        }
        _ => Err(INVALID_TITLE),
    }
}

pub(crate) fn header(input: &Value) -> String {
    let label = title(input)
        .ok()
        .flatten()
        .or_else(|| {
            input
                .get("message")?
                .as_str()?
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
        })
        .unwrap_or("Report to parent");
    match label.char_indices().nth(MAX_TITLE_CHARS) {
        Some((end, _)) => format!("{}…", &label[..end]),
        None => label.to_owned(),
    }
}

pub(crate) fn tool(reporter: TaskReporter) -> (Value, LocalToolFn) {
    let definition = json!({
        "name": NAME,
        "description": CONTRACT,
        "input_schema": {
            "type":"object", "additionalProperties":false,
            "properties": {
                "title":{"type":"string","minLength":1,"maxLength":MAX_TITLE_CHARS,"description":"Short, single-line report title (3-8 words) for the compact card. Keep details in message."},
                "message":{"type":"string","description":"Important finding or precise blocker; not ordinary progress."},
                "blocked":{"type":"boolean","description":"End this invocation with a non-success blocked outcome. No reply is awaited."}
            },
            "required":["message"]
        }
    });
    let handler = typed_local_tool(ToolEffect::ReadOnly, move |input, ctx| {
        let reporter = reporter.clone();
        Box::pin(async move {
            let object = input
                .as_object()
                .ok_or_else(|| invalid("report input must be an object"))?;
            if object
                .keys()
                .any(|key| key != "title" && key != "message" && key != "blocked")
            {
                return Err(invalid("unknown report field"));
            }
            title(&input).map_err(invalid)?;
            let message = input
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("message is required"))?
                .to_owned();
            let blocked = match input.get("blocked") {
                None => false,
                Some(Value::Bool(blocked)) => *blocked,
                _ => return Err(invalid("blocked must be a boolean")),
            };
            reporter
                .report(
                    ctx.tool_use_id
                        .ok_or("report requires tool call identity")?,
                    message,
                    blocked,
                )
                .await
                .map_err(ToolError::from)
        })
    });
    (definition, handler.required_output())
}

fn invalid(message: &'static str) -> ToolError {
    ToolError::new(ToolFailure::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::{INVALID_TITLE, MAX_TITLE_CHARS, header, title};
    use serde_json::{Value, json};
    use test_case::test_case;

    const TITLE: &str = "Writer lock identified";
    const MESSAGE: &str = "The writer holds the transaction.\nThe reader can retry safely.";

    #[test_case(json!({}), Ok(None); "legacy_input")]
    #[test_case(json!({"title": TITLE}), Ok(Some(TITLE)); "short_title")]
    #[test_case(json!({"title": format!(" {TITLE} ")}), Ok(Some(TITLE)); "trimmed_title")]
    #[test_case(json!({"title": " "}), Err(INVALID_TITLE); "empty_title")]
    #[test_case(json!({"title": null}), Err(INVALID_TITLE); "null_title")]
    #[test_case(json!({"title": 1}), Err(INVALID_TITLE); "non_string_title")]
    #[test_case(json!({"title": "first\nsecond"}), Err(INVALID_TITLE); "multiline_title")]
    #[test_case(json!({"title": "first\u{1b}second"}), Err(INVALID_TITLE); "terminal_controls")]
    #[test_case(json!({"title": "界".repeat(MAX_TITLE_CHARS + 1)}), Err(INVALID_TITLE); "overlong_title")]
    fn validates_optional_title(input: Value, expected: Result<Option<&str>, &str>) {
        assert_eq!(title(&input), expected);
    }

    #[test_case(json!({"title": TITLE, "message": MESSAGE}), TITLE; "prefers_title")]
    #[test_case(json!({"message": MESSAGE}), "The writer holds the transaction."; "legacy_message")]
    #[test_case(json!({"message": format!("\n  \n{MESSAGE}")}), "The writer holds the transaction."; "skips_empty_lines")]
    #[test_case(json!({}), "Report to parent"; "missing_input")]
    fn report_header(input: Value, expected: &str) {
        assert_eq!(header(&input), expected);
    }

    #[test_case(true; "title")]
    #[test_case(false; "legacy_message")]
    fn report_header_bounds_unicode(by_title: bool) {
        let label = "界".repeat(MAX_TITLE_CHARS);
        let input = if by_title {
            json!({"title": label, "message": MESSAGE})
        } else {
            json!({"message": format!("{label}界")})
        };
        assert_eq!(
            header(&input),
            if by_title {
                label
            } else {
                format!("{label}…")
            }
        );
        assert!(title(&input).is_ok());
    }
}
