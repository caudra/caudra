//! The MCP adapter: argument parsing, result envelopes, and the protocol size ceiling.
//!
//! This module is a projection of the group's native methods. It adds no behaviour, no policy, and
//! no second source of truth; a native host calling `source_code_refs` and an MCP client calling `source_code_refs`
//! run the same code and receive the same record.

use rmcp::model::{CallToolResult, ContentBlock};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    bound::{Shrinkable, fit},
    error::SourceCodeGraphError,
    group::SourceCodeGraphToolGroup,
    model_text::ModelText,
    types::{SelectorRefusal, SourceCodeContextInput, SourceCodeRefsInput},
};

impl SourceCodeGraphToolGroup {
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
            "source_code_context" => match parse::<SourceCodeContextInput>(name, arguments) {
                Ok(input) => complete(self.source_code_context(input, None, &token).await),
                Err(message) => invalid(message),
            },
            "source_code_refs" => match parse::<SourceCodeRefsInput>(name, arguments) {
                Ok(input) => selectable(self.source_code_refs(input, None, &token).await),
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

fn complete<T>(result: Result<T, SourceCodeGraphError>) -> Result<CallToolResult, rmcp::ErrorData>
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
    result: Result<Result<T, SelectorRefusal>, SourceCodeGraphError>,
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
        rmcp::ErrorData::internal_error("Failed to serialize source-code-graph result", None)
    })?;
    let mut result = CallToolResult::default();
    result.content = vec![ContentBlock::text(output.model_text().into_owned())];
    result.structured_content = Some(structured);
    Ok(result)
}

fn failure(error: &SourceCodeGraphError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(error.to_string())])
}

fn invalid(message: String) -> Result<CallToolResult, rmcp::ErrorData> {
    Ok(CallToolResult::error(vec![ContentBlock::text(message)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MISSING_TASK: &str = "missing field `task`";

    #[tokio::test]
    async fn an_unknown_tool_name_is_not_claimed() {
        let directory = tempfile::TempDir::new().expect("temp dir");
        let group = SourceCodeGraphToolGroup::new(directory.path(), None)
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
        let group = SourceCodeGraphToolGroup::new(directory.path(), None)
            .await
            .expect("group");
        let result = group
            .dispatch(
                "source_code_refs",
                serde_json::json!({ "symbol": "alpha", "unknownField": 1 }),
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
        let group = SourceCodeGraphToolGroup::new(directory.path(), None)
            .await
            .expect("group");
        let result = group
            .dispatch("source_code_context", Value::Null, CancellationToken::new())
            .await
            .expect("claimed")
            .expect("returns a result");
        assert_eq!(result.is_error, Some(true));
        let text = serde_json::to_string(&result.content).expect("serialize");
        assert!(
            text.contains(MISSING_TASK),
            "null must parse as an empty object, so the error names the missing field: {text}"
        );
    }
}
