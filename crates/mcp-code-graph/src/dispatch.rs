//! The MCP adapter: argument parsing, result envelopes, and the protocol size ceiling.
//!
//! This module is a projection of the group's native methods. It adds no behaviour, no policy, and
//! no second source of truth; a native host calling `code_map` and an MCP client calling `code_map`
//! run the same code and receive the same record.

use rmcp::model::{CallToolResult, ContentBlock};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    bound::{Shrinkable, fit},
    error::CodeGraphError,
    group::CodeGraphToolGroup,
    model_text::ModelText,
    types::{
        CodeContextInput, CodeExpandInput, CodeImpactInput, CodeMapInput, CodeRefsInput,
        SelectorRefusal,
    },
};

impl CodeGraphToolGroup {
    /// Routes one MCP tool call, or returns `None` when the name is not ours.
    ///
    /// `None` rather than an error, so a composing server can offer other groups under the same
    /// dispatcher without this one claiming names it does not serve.
    pub async fn dispatch(
        &self,
        name: &str,
        arguments: Value,
        token: CancellationToken,
    ) -> Option<Result<CallToolResult, rmcp::ErrorData>> {
        Some(match name {
            "code_map" => match parse::<CodeMapInput>(name, arguments) {
                Ok(input) => complete(self.code_map(input, None, &token).await),
                Err(message) => invalid(message),
            },
            "code_context" => match parse::<CodeContextInput>(name, arguments) {
                Ok(input) => complete(self.code_context(input, None, &token).await),
                Err(message) => invalid(message),
            },
            "code_refs" => match parse::<CodeRefsInput>(name, arguments) {
                Ok(input) => selectable(self.code_refs(input, None, &token).await),
                Err(message) => invalid(message),
            },
            "code_impact" => match parse::<CodeImpactInput>(name, arguments) {
                Ok(input) => selectable(self.code_impact(input, None, &token).await),
                Err(message) => invalid(message),
            },
            "code_expand" => match parse::<CodeExpandInput>(name, arguments) {
                Ok(input) => selectable(self.code_expand(input, None, &token).await),
                Err(message) => invalid(message),
            },
            _ => return None,
        })
    }
}

fn parse<T: DeserializeOwned>(name: &str, arguments: Value) -> Result<T, String> {
    let arguments = if arguments.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        arguments
    };
    serde_json::from_value(arguments).map_err(|error| format!("Invalid {name} arguments: {error}"))
}

fn complete<T>(result: Result<T, CodeGraphError>) -> Result<CallToolResult, rmcp::ErrorData>
where
    T: Serialize + ModelText + Clone + Shrinkable,
{
    match result {
        Ok(output) => emit(fit(output)),
        Err(error) => Ok(failure(&error)),
    }
}

/// A tool whose selector may not resolve.
///
/// A refusal is a successful call carrying a refusal record, not a protocol error. The caller asked
/// a well-formed question about a symbol that does not exist, and the useful answer is the
/// did-you-mean list, which an error envelope has nowhere to put.
fn selectable<T>(
    result: Result<Result<T, SelectorRefusal>, CodeGraphError>,
) -> Result<CallToolResult, rmcp::ErrorData>
where
    T: Serialize + ModelText + Clone + Shrinkable,
{
    match result {
        Ok(Ok(output)) => emit(fit(output)),
        Ok(Err(refusal)) => emit(refusal),
        Err(error) => Ok(failure(&error)),
    }
}

fn emit(output: impl Serialize + ModelText) -> Result<CallToolResult, rmcp::ErrorData> {
    let structured = serde_json::to_value(&output).map_err(|_| {
        rmcp::ErrorData::internal_error("Failed to serialize code-graph result", None)
    })?;
    let mut result = CallToolResult::default();
    result.content = vec![ContentBlock::text(output.model_text().into_owned())];
    result.structured_content = Some(structured);
    Ok(result)
}

fn failure(error: &CodeGraphError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(error.to_string())])
}

fn invalid(message: String) -> Result<CallToolResult, rmcp::ErrorData> {
    Ok(CallToolResult::error(vec![ContentBlock::text(message)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_unknown_tool_name_is_not_claimed() {
        let directory = tempfile::TempDir::new().expect("temp dir");
        let group = CodeGraphToolGroup::new(directory.path(), None)
            .await
            .expect("group");
        assert!(
            group
                .dispatch("file_read", Value::Null, CancellationToken::new())
                .await
                .is_none(),
            "a composing server must be able to offer other groups"
        );
    }

    #[tokio::test]
    async fn malformed_arguments_are_rejected_without_running_a_crawl() {
        let directory = tempfile::TempDir::new().expect("temp dir");
        let group = CodeGraphToolGroup::new(directory.path(), None)
            .await
            .expect("group");
        let result = group
            .dispatch(
                "code_map",
                serde_json::json!({ "unknownField": 1 }),
                CancellationToken::new(),
            )
            .await
            .expect("claimed")
            .expect("returns a result");
        assert_eq!(result.is_error, Some(true));
    }

    #[tokio::test]
    async fn absent_arguments_are_treated_as_an_empty_object() {
        let directory = tempfile::TempDir::new().expect("temp dir");
        std::fs::write(directory.path().join("a.rs"), "fn alpha() {}").expect("write");
        let group = CodeGraphToolGroup::new(directory.path(), None)
            .await
            .expect("group");
        let result = group
            .dispatch("code_map", Value::Null, CancellationToken::new())
            .await
            .expect("claimed")
            .expect("returns a result");
        assert_ne!(result.is_error, Some(true));
        assert!(result.structured_content.is_some());
    }
}
