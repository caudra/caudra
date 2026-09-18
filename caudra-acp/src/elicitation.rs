//! Maps the `question` tool onto ACP form elicitation (`elicitation/create`).
//! The tool keeps its Lua-advertised schema; only execution is rerouted here,
//! so Zed and friends render native forms instead of a spinner nobody can answer.

use std::collections::BTreeMap;

use agent_client_protocol_schema::v1::{
    ClientCapabilities, CreateElicitationRequest, ElicitationContentValue, ElicitationFormMode,
    ElicitationPropertySchema, ElicitationSchema, ElicitationSessionScope, EnumOption,
    MultiSelectPropertySchema, PermissionOption, PermissionOptionId, PermissionOptionKind,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, SessionId,
    StringPropertySchema, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
};
use serde::Deserialize;
use serde_json::Value;

/// Mirrors the Lua question tool's dismiss output so the model sees the same
/// text regardless of frontend.
pub const DISMISSED: &str = "(question dismissed by user)";
const NO_ANSWER: &str = "(no answer)";
/// Marks a permission request as a question rather than an approval. This is a
/// client convention rather than ACP, so only clients that announce themselves
/// as speaking it ever receive one.
const QUESTION_TOOL_CALL_PREFIX: &str = "interaction_";
const ANSWER_OPTION_PREFIX: &str = "answer_";
const UNSUPPORTED_FORM: &str =
    "this client renders one multiple-choice question at a time; ask the rest in prose";

#[derive(Deserialize)]
struct Question {
    question: String,
    #[serde(default)]
    header: String,
    #[serde(default)]
    options: Vec<QuestionOption>,
    #[serde(default, rename = "multiSelect", alias = "multiple")]
    multi_select: bool,
}

#[derive(Deserialize)]
struct QuestionOption {
    label: String,
    #[serde(default)]
    description: String,
}

pub fn supports_form(caps: &ClientCapabilities) -> bool {
    caps.elicitation.as_ref().is_some_and(|e| e.form.is_some())
}

fn parse_questions(input: &Value) -> Result<Vec<Question>, String> {
    let questions = input.get("questions").cloned().unwrap_or(Value::Null);
    serde_json::from_value(questions).map_err(|e| format!("invalid questions: {e}"))
}

fn option_title(opt: &QuestionOption) -> String {
    if opt.description.is_empty() {
        opt.label.clone()
    } else {
        format!("{} - {}", opt.label, opt.description)
    }
}

fn enum_options(options: &[QuestionOption]) -> Vec<EnumOption> {
    options
        .iter()
        .map(|opt| EnumOption::new(opt.label.clone(), option_title(opt)))
        .collect()
}

fn property(q: &Question) -> ElicitationPropertySchema {
    let title = q.question.clone();
    if q.options.is_empty() {
        ElicitationPropertySchema::String(StringPropertySchema::new().title(title))
    } else if q.multi_select {
        ElicitationPropertySchema::Array(
            MultiSelectPropertySchema::titled(enum_options(&q.options)).title(title),
        )
    } else {
        ElicitationPropertySchema::String(
            StringPropertySchema::new()
                .title(title)
                .one_of(enum_options(&q.options)),
        )
    }
}

/// Property keys are positional (`q1`, `q2`, ...) so answers map back to
/// questions even when headers repeat or are missing.
fn key(index: usize) -> String {
    format!("q{}", index + 1)
}

pub fn form_request(
    session_id: &str,
    tool_call_id: Option<String>,
    input: &Value,
) -> Result<CreateElicitationRequest, String> {
    let questions = parse_questions(input)?;
    if questions.is_empty() {
        return Err("at least one question is required".to_owned());
    }

    let mut schema = ElicitationSchema::new();
    schema.properties = questions
        .iter()
        .enumerate()
        .map(|(i, q)| (key(i), property(q)))
        .collect();

    let scope = ElicitationSessionScope::new(SessionId::from(session_id.to_owned()))
        .tool_call_id(tool_call_id.map(ToolCallId::from));
    let message = match questions.as_slice() {
        [only] => only.question.clone(),
        many => format!("{} questions", many.len()),
    };
    Ok(CreateElicitationRequest::new(
        ElicitationFormMode::new(scope, schema),
        message,
    ))
}

/// A question carried by `session/request_permission`, for clients that ask in
/// chat instead of rendering a form. Only one multiple-choice question fits:
/// there is nowhere to put a second prompt and no free-text field, so anything
/// richer is refused and the model asks in prose rather than silently losing
/// half the form.
pub fn question_permission_request(
    session_id: &str,
    tool_call_id: Option<String>,
    input: &Value,
) -> Result<RequestPermissionRequest, String> {
    let questions = parse_questions(input)?;
    let [question] = questions.as_slice() else {
        return Err(UNSUPPORTED_FORM.to_owned());
    };
    if question.options.is_empty() {
        return Err(UNSUPPORTED_FORM.to_owned());
    }

    let options = question
        .options
        .iter()
        .enumerate()
        .map(|(i, opt)| {
            PermissionOption::new(
                PermissionOptionId::from(answer_option_id(i)),
                option_title(opt),
                // Answering is proceeding. The kind is never read for a
                // question, but the field is required.
                PermissionOptionKind::AllowOnce,
            )
        })
        .collect();

    let call_id = format!(
        "{QUESTION_TOOL_CALL_PREFIX}{}",
        tool_call_id.unwrap_or_default()
    );
    let tool_call = ToolCallUpdate::new(
        ToolCallId::from(call_id),
        ToolCallUpdateFields::new().title(question.question.clone()),
    );
    Ok(RequestPermissionRequest::new(
        SessionId::from(session_id.to_owned()),
        tool_call,
        options,
    ))
}

fn answer_option_id(index: usize) -> String {
    format!("{ANSWER_OPTION_PREFIX}{index}")
}

/// Turns the selected option back into the same `header: label` line the form
/// path feeds the model, so the transport never shows through to the agent.
pub fn format_permission_answer(input: &Value, raw_result: &str) -> String {
    let Ok(response) = serde_json::from_str::<RequestPermissionResponse>(raw_result) else {
        return DISMISSED.to_owned();
    };
    let RequestPermissionOutcome::Selected(selected) = &response.outcome else {
        return DISMISSED.to_owned();
    };
    let index = selected
        .option_id
        .0
        .strip_prefix(ANSWER_OPTION_PREFIX)
        .and_then(|i| i.parse::<usize>().ok());
    let questions = parse_questions(input).unwrap_or_default();

    let Some((question, chosen)) = questions
        .first()
        .zip(index)
        .and_then(|(q, i)| q.options.get(i).map(|opt| (q, opt)))
    else {
        return DISMISSED.to_owned();
    };
    let label = if question.header.is_empty() {
        "Q1"
    } else {
        &question.header
    };
    format!("{label}: {}", chosen.label)
}

fn answer_text(value: Option<&ElicitationContentValue>) -> Option<String> {
    match value? {
        ElicitationContentValue::String(s) if !s.is_empty() => Some(s.clone()),
        ElicitationContentValue::StringArray(items) if !items.is_empty() => Some(items.join(", ")),
        ElicitationContentValue::Integer(n) => Some(n.to_string()),
        ElicitationContentValue::Number(n) => Some(n.to_string()),
        ElicitationContentValue::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Turns the client's `elicitation/create` result into the same markdown the
/// Lua tool feeds the model: one `header: labels` line per question.
pub fn format_response(input: &Value, raw_result: &str) -> String {
    let Ok(response) = serde_json::from_str::<Value>(raw_result) else {
        return DISMISSED.to_owned();
    };
    if response["action"] != "accept" {
        return DISMISSED.to_owned();
    }
    // Values parse per key: nothing in the schema is `required`, so clients
    // may null out skipped fields, and one unreadable value should cost one
    // answer, not the whole form.
    let content: BTreeMap<String, ElicitationContentValue> = response["content"]
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| Some((k.clone(), serde_json::from_value(v.clone()).ok()?)))
                .collect()
        })
        .unwrap_or_default();
    let questions = parse_questions(input).unwrap_or_default();

    questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let label = if q.header.is_empty() {
                format!("Q{}", i + 1)
            } else {
                q.header.clone()
            };
            let answer = answer_text(content.get(&key(i))).unwrap_or_else(|| NO_ANSWER.to_owned());
            format!("{label}: {answer}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::ElicitationScope;
    use test_case::test_case;

    use super::*;

    fn questions_input() -> Value {
        serde_json::json!({
            "questions": [
                {
                    "question": "Pick a framework",
                    "header": "Framework",
                    "options": [
                        { "label": "axum", "description": "tokio based" },
                        { "label": "actix" }
                    ]
                },
                {
                    "question": "Which features?",
                    "header": "Features",
                    "multiSelect": true,
                    "options": [{ "label": "auth" }, { "label": "uploads" }]
                },
                { "question": "Anything else?" }
            ]
        })
    }

    #[test]
    fn form_request_maps_questions_to_schema() {
        let req = form_request("sess_1", Some("tool_1".to_owned()), &questions_input()).unwrap();
        assert_eq!(req.message, "3 questions");

        let ElicitationScope::Session(scope) = req.scope() else {
            panic!("expected session scope");
        };
        assert_eq!(scope.session_id.0.as_ref(), "sess_1");
        assert_eq!(
            scope.tool_call_id.as_ref().map(|t| t.0.as_ref()),
            Some("tool_1")
        );

        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["mode"], "form");
        let props = &json["requestedSchema"]["properties"];
        assert_eq!(props["q1"]["type"], "string");
        assert_eq!(props["q1"]["oneOf"][0]["const"], "axum");
        assert_eq!(props["q2"]["type"], "array");
        assert_eq!(props["q3"]["type"], "string");
        assert!(props["q3"].get("oneOf").is_none());
    }

    #[test]
    fn single_question_is_the_message() {
        let input = serde_json::json!({ "questions": [{ "question": "Proceed?" }] });
        let req = form_request("sess_1", None, &input).unwrap();
        assert_eq!(req.message, "Proceed?");
    }

    #[test_case(serde_json::json!({}) ; "missing_questions")]
    #[test_case(serde_json::json!({ "questions": [] }) ; "empty_questions")]
    fn form_request_rejects_bad_input(input: Value) {
        assert!(form_request("sess_1", None, &input).is_err());
    }

    #[test]
    fn accept_response_formats_answers() {
        let raw = serde_json::json!({
            "action": "accept",
            "content": { "q1": "axum", "q2": ["auth", "uploads"] }
        })
        .to_string();
        assert_eq!(
            format_response(&questions_input(), &raw),
            "Framework: axum\nFeatures: auth, uploads\nQ3: (no answer)"
        );
    }

    #[test]
    fn nulled_out_field_costs_one_answer_not_the_form() {
        let raw = serde_json::json!({
            "action": "accept",
            "content": { "q1": "axum", "q2": null }
        })
        .to_string();
        assert_eq!(
            format_response(&questions_input(), &raw),
            "Framework: axum\nFeatures: (no answer)\nQ3: (no answer)"
        );
    }

    #[test_case(r#"{"action":"decline"}"# ; "decline")]
    #[test_case(r#"{"action":"cancel"}"# ; "cancel")]
    #[test_case("not json" ; "unparsable")]
    #[test_case("null" ; "jsonrpc_error_forwarded_as_null")]
    fn non_accept_is_dismissed(raw: &str) {
        assert_eq!(format_response(&questions_input(), raw), DISMISSED);
    }

    fn single_choice_input() -> Value {
        serde_json::json!({
            "questions": [{
                "question": "Pick a framework",
                "header": "Framework",
                "options": [
                    { "label": "axum", "description": "tokio based" },
                    { "label": "actix" }
                ]
            }]
        })
    }

    #[test]
    fn question_permission_request_marks_the_call_as_an_interaction() {
        let req =
            question_permission_request("sess_1", Some("toolu_9".to_owned()), &single_choice_input())
                .unwrap();
        let json = serde_json::to_value(&req).unwrap();

        assert_eq!(json["toolCall"]["toolCallId"], "interaction_toolu_9");
        assert_eq!(json["toolCall"]["title"], "Pick a framework");
        assert_eq!(
            json["options"],
            serde_json::json!([
                { "optionId": "answer_0", "name": "axum - tokio based", "kind": "allow_once" },
                { "optionId": "answer_1", "name": "actix", "kind": "allow_once" },
            ])
        );
    }

    #[test_case(serde_json::json!({ "questions": [{ "question": "Anything?" }] }) ; "free_text_has_no_shape")]
    #[test_case(serde_json::json!({ "questions": [
        { "question": "One", "options": [{ "label": "a" }] },
        { "question": "Two", "options": [{ "label": "b" }] }
    ] }) ; "second_question_would_be_lost")]
    fn question_permission_request_refuses_what_it_cannot_render(input: Value) {
        assert_eq!(
            question_permission_request("sess_1", None, &input).unwrap_err(),
            UNSUPPORTED_FORM
        );
    }

    #[test]
    fn selected_option_becomes_the_answer_line() {
        let raw = serde_json::json!({
            "outcome": { "outcome": "selected", "optionId": "answer_1" }
        })
        .to_string();
        assert_eq!(
            format_permission_answer(&single_choice_input(), &raw),
            "Framework: actix"
        );
    }

    #[test_case(r#"{"outcome":{"outcome":"cancelled"}}"# ; "cancelled")]
    #[test_case(r#"{"outcome":{"outcome":"selected","optionId":"answer_9"}}"# ; "index_out_of_range")]
    #[test_case(r#"{"outcome":{"outcome":"selected","optionId":"allow_once"}}"# ; "not_an_answer_id")]
    #[test_case("not json" ; "unparsable")]
    fn unusable_permission_answer_is_dismissed(raw: &str) {
        assert_eq!(
            format_permission_answer(&single_choice_input(), raw),
            DISMISSED
        );
    }

    #[test]
    fn supports_form_requires_form_capability() {
        assert!(!supports_form(&ClientCapabilities::default()));
        let caps: ClientCapabilities = serde_json::from_value(serde_json::json!({
            "elicitation": { "form": {} }
        }))
        .unwrap();
        assert!(supports_form(&caps));
        let url_only: ClientCapabilities = serde_json::from_value(serde_json::json!({
            "elicitation": { "url": {} }
        }))
        .unwrap();
        assert!(!supports_form(&url_only));
    }
}
