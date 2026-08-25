use std::{path::Path, sync::Arc};

use rmcp::model::{CallToolResult, ContentBlock, Tool};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    FilesystemError, FilesystemLimits,
    catalog::catalog,
    operations::FilesystemCore,
    types::{
        FileApplyPatchInput, FileApplyPatchOutput, FileEditInput, FileEditOutput, FileGlobInput,
        FileGlobOutput, FileGrepInput, FileGrepOutput, FileReadInput, FileReadOutput,
        FileWriteInput, FileWriteOutput,
    },
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
// This is a protocol compatibility bound, not a deployment tuning default.
pub(crate) const MCP_RAW_RESULT_CEILING_BYTES: usize = 64_000;
const TRANSPORT_FRAME_DELIMITER_BYTES: usize = 1;

#[derive(Serialize)]
struct McpResponse<'a> {
    jsonrpc: &'static str,
    id: u64,
    result: &'a CallToolResult,
}

/// Cloneable composition unit for the final MCP server. Clones share the same
/// mutation lock and immutable root policy.
#[derive(Clone, Debug)]
pub struct FileToolGroup {
    core: Arc<FilesystemCore>,
}

impl FileToolGroup {
    pub async fn new(
        root: impl AsRef<Path>,
        allow_write: bool,
        limits: Option<FilesystemLimits>,
    ) -> Result<Self, FilesystemError> {
        Ok(Self {
            core: Arc::new(FilesystemCore::create(root.as_ref(), allow_write, limits).await?),
        })
    }

    pub fn root(&self) -> &Path {
        self.core.root()
    }

    pub fn allow_write(&self) -> bool {
        self.core.allow_write
    }

    pub fn limits(&self) -> &FilesystemLimits {
        &self.core.limits
    }

    pub fn catalog(&self) -> Vec<Tool> {
        catalog()
    }

    pub async fn file_read(
        &self,
        input: FileReadInput,
        token: &CancellationToken,
    ) -> Result<FileReadOutput, FilesystemError> {
        self.core.file_read(input, token).await
    }

    pub async fn file_glob(
        &self,
        input: FileGlobInput,
        token: &CancellationToken,
    ) -> Result<FileGlobOutput, FilesystemError> {
        self.core.file_glob(input, token).await
    }

    pub async fn file_grep(
        &self,
        input: FileGrepInput,
        token: &CancellationToken,
    ) -> Result<FileGrepOutput, FilesystemError> {
        self.core.file_grep(input, token).await
    }

    pub async fn file_write(
        &self,
        input: FileWriteInput,
        token: &CancellationToken,
    ) -> Result<FileWriteOutput, FilesystemError> {
        self.core.file_write(input, token).await
    }

    pub async fn file_edit(
        &self,
        input: FileEditInput,
        token: &CancellationToken,
    ) -> Result<FileEditOutput, FilesystemError> {
        self.core.file_edit(input, token).await
    }

    pub async fn file_apply_patch(
        &self,
        input: FileApplyPatchInput,
        token: &CancellationToken,
    ) -> Result<FileApplyPatchOutput, FilesystemError> {
        self.core.file_apply_patch(input, token).await
    }

    /// Returns `None` only when this group does not own the tool name, allowing
    /// a server to compose several groups without a second routing table.
    pub async fn dispatch(
        &self,
        name: &str,
        arguments: Value,
        token: CancellationToken,
    ) -> Option<Result<CallToolResult, rmcp::ErrorData>> {
        let result = match name {
            "file_read" => {
                match parse_arguments::<FileReadInput>(name, arguments).and_then(validate_read) {
                    Ok(input) => run(self.core.file_read(input, &token).await),
                    Err(error) => tool_error(error),
                }
            }
            "file_glob" => {
                match parse_arguments::<FileGlobInput>(name, arguments).and_then(validate_glob) {
                    Ok(input) => run(self.core.file_glob(input, &token).await),
                    Err(error) => tool_error(error),
                }
            }
            "file_grep" => {
                match parse_arguments::<FileGrepInput>(name, arguments).and_then(validate_grep) {
                    Ok(input) => run(self.core.file_grep(input, &token).await),
                    Err(error) => tool_error(error),
                }
            }
            "file_write" => {
                match parse_arguments::<FileWriteInput>(name, arguments).and_then(validate_write) {
                    Ok(input) => run(self.core.file_write(input, &token).await),
                    Err(error) => tool_error(error),
                }
            }
            "file_edit" => {
                match parse_arguments::<FileEditInput>(name, arguments).and_then(validate_edit) {
                    Ok(input) => run(self.core.file_edit(input, &token).await),
                    Err(error) => tool_error(error),
                }
            }
            "file_apply_patch" => {
                match parse_arguments::<FileApplyPatchInput>(name, arguments)
                    .and_then(validate_patch)
                {
                    Ok(input) => run(self.core.file_apply_patch(input, &token).await),
                    Err(error) => tool_error(error),
                }
            }
            _ => return None,
        };
        Some(result)
    }
}

fn parse_arguments<T: DeserializeOwned>(name: &str, value: Value) -> Result<T, String> {
    serde_json::from_value(value)
        .map_err(|error| format!("Invalid arguments for tool {name}: {error}"))
}

fn run<T: Serialize>(
    result: Result<T, FilesystemError>,
) -> Result<CallToolResult, rmcp::ErrorData> {
    match result {
        Ok(output) => success(output),
        Err(error) => tool_error(error),
    }
}

fn success(output: impl Serialize) -> Result<CallToolResult, rmcp::ErrorData> {
    build_success_result(&output).map_err(|error| {
        rmcp::ErrorData::internal_error(
            "Failed to serialize filesystem tool result",
            Some(Value::String(error.to_string())),
        )
    })
}

fn build_success_result(output: &impl Serialize) -> Result<CallToolResult, serde_json::Error> {
    let structured = serde_json::to_value(output)?;
    let text = serde_json::to_string_pretty(&structured)?;
    let mut result = CallToolResult::default();
    result.content = vec![ContentBlock::text(text)];
    result.structured_content = Some(structured);
    Ok(result)
}

pub(crate) fn mcp_response_size(output: &impl Serialize) -> Result<usize, serde_json::Error> {
    let result = build_success_result(output)?;
    let envelope = McpResponse {
        jsonrpc: "2.0",
        // Numeric request IDs are common. The largest accepted safe integer is
        // conservative relative to normal incrementing request IDs.
        id: MAX_SAFE_INTEGER,
        result: &result,
    };
    Ok(serde_json::to_vec(&envelope)?
        .len()
        .saturating_add(TRANSPORT_FRAME_DELIMITER_BYTES))
}

fn tool_error(error: impl ToString) -> Result<CallToolResult, rmcp::ErrorData> {
    Ok(CallToolResult::error(vec![ContentBlock::text(
        error.to_string(),
    )]))
}

fn nonempty(value: &str, name: &str) -> Result<(), String> {
    if value.is_empty() {
        Err(format!("Invalid arguments: {name} must not be empty"))
    } else {
        Ok(())
    }
}

fn validate_read(input: FileReadInput) -> Result<FileReadInput, String> {
    nonempty(&input.file_path, "filePath")?;
    if input.offset == Some(0) {
        return Err("Invalid arguments: offset must be at least 1".to_owned());
    }
    if input
        .offset
        .is_some_and(|value| value as u64 > MAX_SAFE_INTEGER)
        || input
            .limit
            .is_some_and(|value| value as u64 > MAX_SAFE_INTEGER)
    {
        return Err("Invalid arguments: numeric values must be safe integers".to_owned());
    }
    Ok(input)
}

fn validate_glob(input: FileGlobInput) -> Result<FileGlobInput, String> {
    nonempty(&input.pattern, "pattern")?;
    if input.path.as_deref() == Some("") {
        return Err("Invalid arguments: path must not be empty".to_owned());
    }
    Ok(input)
}

fn validate_grep(input: FileGrepInput) -> Result<FileGrepInput, String> {
    nonempty(&input.pattern, "pattern")?;
    if input.path.as_deref() == Some("") {
        return Err("Invalid arguments: path must not be empty".to_owned());
    }
    if input.include.as_deref() == Some("") {
        return Err("Invalid arguments: include must not be empty".to_owned());
    }
    Ok(input)
}

fn validate_write(input: FileWriteInput) -> Result<FileWriteInput, String> {
    nonempty(&input.file_path, "filePath")?;
    Ok(input)
}

fn validate_edit(input: FileEditInput) -> Result<FileEditInput, String> {
    nonempty(&input.file_path, "filePath")?;
    nonempty(&input.old_string, "oldString")?;
    Ok(input)
}

fn validate_patch(input: FileApplyPatchInput) -> Result<FileApplyPatchInput, String> {
    nonempty(&input.patch_text, "patchText")?;
    Ok(input)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{build_success_result, mcp_response_size};

    #[test]
    fn response_size_counts_both_result_forms_and_envelope() {
        let output = json!({ "value": "x".repeat(1_000) });
        let structured_size = serde_json::to_vec(&output)
            .expect("structured output")
            .len();
        let result_size =
            serde_json::to_vec(&build_success_result(&output).expect("call tool result"))
                .expect("serialized call tool result")
                .len();
        let response_size = mcp_response_size(&output).expect("response size");

        assert!(result_size > structured_size * 2);
        assert!(response_size > result_size);
    }
}
