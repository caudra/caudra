use serde_json::Value;

use crate::agent::tool_dispatch::{self, Emit};

use super::ToolContext;

pub const IMAGE_NOT_VISIBLE_NOTE: &str =
    "image pixels are not visible from here; call the view_image tool directly";

pub async fn dispatch(ctx: &ToolContext, name: &str, input: &Value) -> Result<String, String> {
    ctx.deadline.check()?;
    let done = tool_dispatch::run(
        &ctx.registry,
        ctx.mcp.as_ref(),
        String::new(),
        name,
        input,
        ctx,
        Emit::Silent,
    )
    .await;
    flatten(&done)
}

/// The one place a `ToolDoneEvent` becomes text a caller can read.
/// Both the interpreter and `caudra.agent.call_tool` come through here,
/// so the two can never drift apart.
pub fn flatten(done: &crate::ToolDoneEvent) -> Result<String, String> {
    let text = match (&done.model_output, &done.output) {
        // The pixels are dropped here; say so instead of implying they were seen.
        (_, crate::ToolOutput::Image { text, .. }) if !done.is_error => {
            format!("{text} ({IMAGE_NOT_VISIBLE_NOTE})")
        }
        (Some(model_output), _) => model_output.clone(),
        (None, output) => output.as_text(),
    };
    if done.is_error { Err(text) } else { Ok(text) }
}

pub fn build_tool_input(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value, String> {
    if let Some(first) = args.first()
        && first.is_object()
    {
        return Ok(first.clone());
    }

    if !kwargs.is_empty() {
        let mut obj = serde_json::Map::new();
        for (k, v) in kwargs {
            obj.insert(k.clone(), v.clone());
        }
        return Ok(Value::Object(obj));
    }

    if args.is_empty() {
        return Ok(serde_json::json!({}));
    }

    Err("pass arguments as keyword arguments (e.g. read(path='/file'))".into())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const EXPECTED_ERR: &str = "pass arguments as keyword arguments (e.g. read(path='/file'))";

    #[test_case(&[], &[("path".into(), json!("/foo"))],                              json!({"path": "/foo"})          ; "kwargs")]
    #[test_case(&[json!({"path": "/foo"})], &[],                                     json!({"path": "/foo"})          ; "dict_passthrough")]
    #[test_case(&[], &[],                                                            json!({})                        ; "no_args")]
    #[test_case(&[json!({"a": 1}), json!({"b": 2})], &[],                           json!({"a": 1})                  ; "first_object_ignores_rest")]
    #[test_case(&[json!({"a": 1})], &[("b".into(), json!(2))],                      json!({"a": 1})                  ; "first_object_ignores_kwargs")]
    #[test_case(&[], &[("a".into(), json!(1)), ("b".into(), json!(2))],              json!({"a": 1, "b": 2})         ; "multiple_kwargs_all_included")]
    fn build_tool_input_cases(args: &[Value], kwargs: &[(String, Value)], expected: Value) {
        assert_eq!(build_tool_input(args, kwargs).unwrap(), expected);
    }

    #[test_case(&[json!("hello")], &[]          ; "positional_string")]
    #[test_case(&[json!(1), json!(2)], &[]      ; "multiple_positional_non_objects")]
    fn build_tool_input_rejects_positional_non_objects(args: &[Value], kwargs: &[(String, Value)]) {
        assert_eq!(build_tool_input(args, kwargs).unwrap_err(), EXPECTED_ERR);
    }

    #[test_case(false ; "success")]
    #[test_case(true ; "error")]
    fn flatten_excludes_model_suffix(is_error: bool) {
        let done = crate::ToolDoneEvent {
            id: "t1".into(),
            tool: Arc::from("test"),
            output: crate::ToolOutput::Plain("visible output".into()),
            is_error,
            annotation: None,
            written_path: None,
            written_paths: Vec::new(),
            remote_written_paths: false,
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
            documents: Vec::new(),
            accounting: crate::ToolAccounting::default(),
        }
        .with_model_suffix(Some("model-only context".into()));

        let result = flatten(&done);
        if is_error {
            assert_eq!(result.unwrap_err(), "visible output");
        } else {
            assert_eq!(result.unwrap(), "visible output");
        }
    }

    #[test]
    fn flatten_returns_bounded_presentation_after_dispatch_limiting() {
        let mut done = crate::ToolDoneEvent {
            id: "t1".into(),
            tool: Arc::from("test"),
            output: crate::ToolOutput::Plain("x".repeat(2_000).into()),
            is_error: false,
            annotation: None,
            written_path: None,
            written_paths: Vec::new(),
            remote_written_paths: false,
            output_ref: None,
            output_limits: None,
            model_suffix: Some("model-only context".into()),
            model_output: None,
            model_output_from_ref: false,
            documents: Vec::new(),
            accounting: crate::ToolAccounting::default(),
        };
        let mut ctx = crate::tools::test_support::stub_ctx(&crate::AgentMode::Build);
        ctx.config.max_output_lines = 10;
        ctx.config.max_output_bytes = 220;
        smol::block_on(crate::tool_output::limit(&mut done, &ctx));

        let flattened = flatten(&done).unwrap();
        assert!(flattened.len() <= ctx.config.max_output_bytes);
        assert!(!flattened.contains("model-only context"));
        assert!(flattened.contains("Full output was unavailable"));
    }

    #[test]
    fn flatten_prefers_exact_model_output() {
        let mut done = crate::ToolDoneEvent {
            id: "t1".into(),
            tool: Arc::from("test"),
            output: crate::ToolOutput::Plain("presentation".into()),
            is_error: false,
            annotation: None,
            written_path: None,
            written_paths: Vec::new(),
            remote_written_paths: false,
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: Some("exact model output".into()),
            model_output_from_ref: false,
            documents: Vec::new(),
            accounting: crate::ToolAccounting::default(),
        };

        assert_eq!(flatten(&done).unwrap(), "exact model output");
        done.is_error = true;
        assert_eq!(flatten(&done).unwrap_err(), "exact model output");
    }
}
