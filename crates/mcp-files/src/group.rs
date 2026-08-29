use std::{path::Path, sync::Arc};

#[cfg(feature = "mcp")]
use rmcp::model::{CallToolResult, ContentBlock, Tool};
use serde::Serialize;
#[cfg(feature = "mcp")]
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[cfg(feature = "mcp")]
use crate::catalog::catalog;
use crate::mutation_operations::{PlannedChange, PlannedChangeType};
use crate::{
    FilesystemError, FilesystemLimits,
    operations::FilesystemCore,
    patch::{PatchHunk, parse_patch},
    text::enforce_bytes,
    types::{
        FileApplyPatchInput, FileApplyPatchOutput, FileEditInput, FileEditOutput, FileGlobInput,
        FileGlobOutput, FileGrepInput, FileGrepOutput, FileReadInput, FileReadOutput, FileResource,
        FileResourceAccess, FileWriteInput, FileWriteOutput,
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
    result: &'a SerializedToolResult<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SerializedToolResult<'a> {
    result_type: &'static str,
    content: [SerializedTextContent<'a>; 1],
    structured_content: &'a Value,
}

#[derive(Serialize)]
struct SerializedTextContent<'a> {
    #[serde(rename = "type")]
    content_type: &'static str,
    text: &'a str,
}

/// Cloneable filesystem executor. Clones share the same mutation lock and path policy.
#[derive(Clone, Debug)]
pub struct FileToolGroup {
    core: Arc<FilesystemCore>,
}

pub struct PreparedFilePatch {
    core: Arc<FilesystemCore>,
    changes: Vec<PlannedChange>,
    preview: FileApplyPatchOutput,
    resources: Vec<FileResource>,
}

impl PreparedFilePatch {
    #[must_use]
    pub fn preview(&self) -> &FileApplyPatchOutput {
        &self.preview
    }

    #[must_use]
    pub fn resources(&self) -> &[FileResource] {
        &self.resources
    }
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

    /// Construct a host-managed filesystem rooted only for relative-path resolution.
    /// Absolute paths and relative traversal may resolve outside `base_cwd`.
    pub async fn new_unconfined(
        base_cwd: impl AsRef<Path>,
        limits: Option<FilesystemLimits>,
    ) -> Result<Self, FilesystemError> {
        Ok(Self {
            core: Arc::new(FilesystemCore::create_unconfined(base_cwd.as_ref(), limits).await?),
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

    #[cfg(feature = "mcp")]
    pub fn catalog(&self) -> Vec<Tool> {
        catalog()
    }

    pub async fn file_read(
        &self,
        input: FileReadInput,
        token: &CancellationToken,
    ) -> Result<FileReadOutput, FilesystemError> {
        self.core.file_read(validate_read(input)?, token).await
    }

    pub async fn file_glob(
        &self,
        input: FileGlobInput,
        token: &CancellationToken,
    ) -> Result<FileGlobOutput, FilesystemError> {
        self.core.file_glob(validate_glob(input)?, token).await
    }

    pub async fn file_grep(
        &self,
        input: FileGrepInput,
        token: &CancellationToken,
    ) -> Result<FileGrepOutput, FilesystemError> {
        self.core.file_grep(validate_grep(input)?, token).await
    }

    pub async fn file_write(
        &self,
        input: FileWriteInput,
        token: &CancellationToken,
    ) -> Result<FileWriteOutput, FilesystemError> {
        self.core.file_write(validate_write(input)?, token).await
    }

    pub async fn file_edit(
        &self,
        input: FileEditInput,
        token: &CancellationToken,
    ) -> Result<FileEditOutput, FilesystemError> {
        self.core.file_edit(validate_edit(input)?, token).await
    }

    pub async fn file_apply_patch(
        &self,
        input: FileApplyPatchInput,
        token: &CancellationToken,
    ) -> Result<FileApplyPatchOutput, FilesystemError> {
        self.core
            .file_apply_patch(validate_patch(input)?, token)
            .await
    }

    pub async fn inspect_read(
        &self,
        input: &FileReadInput,
    ) -> Result<FileResource, FilesystemError> {
        validate_read(input.clone())?;
        self.inspect_path(&input.file_path, FileResourceAccess::Read)
            .await
    }

    pub async fn inspect_glob(
        &self,
        input: &FileGlobInput,
    ) -> Result<FileResource, FilesystemError> {
        validate_glob(input.clone())?;
        self.inspect_path(
            input.path.as_deref().unwrap_or("."),
            FileResourceAccess::Traverse,
        )
        .await
    }

    pub async fn inspect_grep(
        &self,
        input: &FileGrepInput,
    ) -> Result<FileResource, FilesystemError> {
        validate_grep(input.clone())?;
        self.inspect_path(
            input.path.as_deref().unwrap_or("."),
            FileResourceAccess::Traverse,
        )
        .await
    }

    pub async fn inspect_write(
        &self,
        input: &FileWriteInput,
    ) -> Result<FileResource, FilesystemError> {
        validate_write(input.clone())?;
        enforce_bytes("content", &input.content, self.core.limits.max_write_bytes)?;
        self.inspect_path(&input.file_path, FileResourceAccess::Write)
            .await
    }

    pub async fn inspect_edit(
        &self,
        input: &FileEditInput,
    ) -> Result<FileResource, FilesystemError> {
        validate_edit(input.clone())?;
        self.inspect_path(&input.file_path, FileResourceAccess::ReadWrite)
            .await
    }

    pub async fn inspect_apply_patch(
        &self,
        input: &FileApplyPatchInput,
    ) -> Result<Vec<FileResource>, FilesystemError> {
        validate_patch(input.clone())?;
        enforce_bytes(
            "patchText",
            &input.patch_text,
            self.core.limits.max_patch_bytes,
        )?;
        let hunks = parse_patch(&input.patch_text)?;
        if hunks.len() > self.core.limits.max_patch_files {
            return Err(FilesystemError::message(format!(
                "Patch exceeds maximum of {} file sections",
                self.core.limits.max_patch_files
            )));
        }
        let mut resources = Vec::new();
        for hunk in hunks {
            match hunk {
                PatchHunk::Add { path, .. } => {
                    resources.push(self.inspect_path(&path, FileResourceAccess::Write).await?);
                }
                PatchHunk::Delete { path } => {
                    resources.push(self.inspect_path(&path, FileResourceAccess::Delete).await?);
                }
                PatchHunk::Update {
                    path, move_path, ..
                } => {
                    resources.push(
                        self.inspect_path(
                            &path,
                            if move_path.is_some() {
                                FileResourceAccess::Delete
                            } else {
                                FileResourceAccess::ReadWrite
                            },
                        )
                        .await?,
                    );
                    if let Some(move_path) = move_path {
                        resources.push(
                            self.inspect_path(&move_path, FileResourceAccess::Write)
                                .await?,
                        );
                    }
                }
            }
        }
        Ok(resources)
    }

    /// Fully plan a patch for host authorization without mutating the filesystem.
    /// `dry_run` is ignored because publication only occurs through `execute_prepared_patch`.
    pub async fn prepare_apply_patch(
        &self,
        input: FileApplyPatchInput,
        token: &CancellationToken,
    ) -> Result<PreparedFilePatch, FilesystemError> {
        let input = validate_patch(input)?;
        let _guard = self.core.mutation.lock().await;
        let (changes, preview) = self.core.prepare_patch(&input.patch_text, token).await?;
        let resources = changes
            .iter()
            .flat_map(|change| {
                let source = FileResource {
                    requested_path: change.file_path.to_string_lossy().into_owned(),
                    path: change.file_path.clone(),
                    access: match change.change_type {
                        PlannedChangeType::Add => FileResourceAccess::Write,
                        PlannedChangeType::Update => FileResourceAccess::ReadWrite,
                        PlannedChangeType::Delete | PlannedChangeType::Move => {
                            FileResourceAccess::Delete
                        }
                    },
                };
                let destination = change.move_path.as_ref().map(|path| FileResource {
                    requested_path: path.to_string_lossy().into_owned(),
                    path: path.clone(),
                    access: FileResourceAccess::Write,
                });
                std::iter::once(source).chain(destination)
            })
            .collect();
        Ok(PreparedFilePatch {
            core: self.core.clone(),
            changes,
            preview,
            resources,
        })
    }

    /// Publish an authorized prepared patch exactly once, without replanning.
    pub async fn execute_prepared_patch(
        &self,
        prepared: PreparedFilePatch,
        token: &CancellationToken,
    ) -> Result<FileApplyPatchOutput, FilesystemError> {
        if !Arc::ptr_eq(&self.core, &prepared.core) {
            return Err(FilesystemError::message(
                "Prepared patch belongs to a different file tool group",
            ));
        }
        let _guard = self.core.mutation.lock().await;
        self.core
            .publish_prepared_patch(&prepared.changes, token)
            .await?;
        let mut output = prepared.preview;
        output.applied = true;
        Ok(output)
    }

    async fn inspect_path(
        &self,
        requested_path: &str,
        access: FileResourceAccess,
    ) -> Result<FileResource, FilesystemError> {
        Ok(FileResource {
            requested_path: requested_path.to_owned(),
            path: self.core.policy.resolve(requested_path).await?,
            access,
        })
    }

    /// Returns `None` only when this group does not own the tool name, allowing
    /// a server to compose several groups without a second routing table.
    #[cfg(feature = "mcp")]
    pub async fn dispatch(
        &self,
        name: &str,
        arguments: Value,
        token: CancellationToken,
    ) -> Option<Result<CallToolResult, rmcp::ErrorData>> {
        let result = match name {
            "file_read" => match parse_arguments::<FileReadInput>(name, arguments) {
                Ok(input) => run(self.file_read(input, &token).await),
                Err(error) => tool_error(error),
            },
            "file_glob" => match parse_arguments::<FileGlobInput>(name, arguments) {
                Ok(input) => run(self.file_glob(input, &token).await),
                Err(error) => tool_error(error),
            },
            "file_grep" => match parse_arguments::<FileGrepInput>(name, arguments) {
                Ok(input) => run(self.file_grep(input, &token).await),
                Err(error) => tool_error(error),
            },
            "file_write" => match parse_arguments::<FileWriteInput>(name, arguments) {
                Ok(input) => run(self.file_write(input, &token).await),
                Err(error) => tool_error(error),
            },
            "file_edit" => match parse_arguments::<FileEditInput>(name, arguments) {
                Ok(input) => run(self.file_edit(input, &token).await),
                Err(error) => tool_error(error),
            },
            "file_apply_patch" => match parse_arguments::<FileApplyPatchInput>(name, arguments) {
                Ok(input) => run(self.file_apply_patch(input, &token).await),
                Err(error) => tool_error(error),
            },
            _ => return None,
        };
        Some(result)
    }
}

#[cfg(feature = "mcp")]
fn parse_arguments<T: DeserializeOwned>(name: &str, value: Value) -> Result<T, String> {
    serde_json::from_value(value)
        .map_err(|error| format!("Invalid arguments for tool {name}: {error}"))
}

#[cfg(feature = "mcp")]
fn run<T: Serialize>(
    result: Result<T, FilesystemError>,
) -> Result<CallToolResult, rmcp::ErrorData> {
    match result {
        Ok(output) => success(output),
        Err(error) => tool_error(error),
    }
}

#[cfg(feature = "mcp")]
fn success(output: impl Serialize) -> Result<CallToolResult, rmcp::ErrorData> {
    build_success_result(&output).map_err(|error| {
        rmcp::ErrorData::internal_error(
            "Failed to serialize filesystem tool result",
            Some(Value::String(error.to_string())),
        )
    })
}

#[cfg(feature = "mcp")]
fn build_success_result(output: &impl Serialize) -> Result<CallToolResult, serde_json::Error> {
    let structured = serde_json::to_value(output)?;
    let text = serde_json::to_string_pretty(&structured)?;
    let mut result = CallToolResult::default();
    result.content = vec![ContentBlock::text(text)];
    result.structured_content = Some(structured);
    Ok(result)
}

pub(crate) fn mcp_response_size(output: &impl Serialize) -> Result<usize, serde_json::Error> {
    let structured = serde_json::to_value(output)?;
    let text = serde_json::to_string_pretty(&structured)?;
    let result = SerializedToolResult {
        result_type: "complete",
        content: [SerializedTextContent {
            content_type: "text",
            text: &text,
        }],
        structured_content: &structured,
    };
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

#[cfg(feature = "mcp")]
fn tool_error(error: impl ToString) -> Result<CallToolResult, rmcp::ErrorData> {
    Ok(CallToolResult::error(vec![ContentBlock::text(
        error.to_string(),
    )]))
}

fn nonempty(value: &str, name: &str) -> Result<(), FilesystemError> {
    if value.is_empty() {
        Err(FilesystemError::message(format!(
            "Invalid arguments: {name} must not be empty"
        )))
    } else {
        Ok(())
    }
}

fn validate_read(input: FileReadInput) -> Result<FileReadInput, FilesystemError> {
    nonempty(&input.file_path, "filePath")?;
    if input.offset == Some(0) {
        return Err(FilesystemError::message(
            "Invalid arguments: offset must be at least 1",
        ));
    }
    if input
        .offset
        .is_some_and(|value| value as u64 > MAX_SAFE_INTEGER)
        || input
            .limit
            .is_some_and(|value| value as u64 > MAX_SAFE_INTEGER)
    {
        return Err(FilesystemError::message(
            "Invalid arguments: numeric values must be safe integers",
        ));
    }
    Ok(input)
}

fn validate_glob(input: FileGlobInput) -> Result<FileGlobInput, FilesystemError> {
    nonempty(&input.pattern, "pattern")?;
    if input.path.as_deref() == Some("") {
        return Err(FilesystemError::message(
            "Invalid arguments: path must not be empty",
        ));
    }
    Ok(input)
}

fn validate_grep(input: FileGrepInput) -> Result<FileGrepInput, FilesystemError> {
    nonempty(&input.pattern, "pattern")?;
    if input.path.as_deref() == Some("") {
        return Err(FilesystemError::message(
            "Invalid arguments: path must not be empty",
        ));
    }
    if input.include.as_deref() == Some("") {
        return Err(FilesystemError::message(
            "Invalid arguments: include must not be empty",
        ));
    }
    Ok(input)
}

fn validate_write(input: FileWriteInput) -> Result<FileWriteInput, FilesystemError> {
    nonempty(&input.file_path, "filePath")?;
    Ok(input)
}

fn validate_edit(input: FileEditInput) -> Result<FileEditInput, FilesystemError> {
    nonempty(&input.file_path, "filePath")?;
    nonempty(&input.old_string, "oldString")?;
    Ok(input)
}

fn validate_patch(input: FileApplyPatchInput) -> Result<FileApplyPatchInput, FilesystemError> {
    nonempty(&input.patch_text, "patchText")?;
    Ok(input)
}

#[cfg(all(test, feature = "mcp"))]
mod tests {
    use serde_json::json;

    use super::{
        SerializedTextContent, SerializedToolResult, build_success_result, mcp_response_size,
    };

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
        let structured = serde_json::to_value(&output).unwrap();
        let text = serde_json::to_string_pretty(&structured).unwrap();
        let neutral = SerializedToolResult {
            result_type: "complete",
            content: [SerializedTextContent {
                content_type: "text",
                text: &text,
            }],
            structured_content: &structured,
        };
        assert_eq!(
            serde_json::to_value(neutral).unwrap(),
            serde_json::to_value(build_success_result(&output).unwrap()).unwrap()
        );
        let response_size = mcp_response_size(&output).expect("response size");

        assert!(result_size > structured_size * 2);
        assert!(response_size > result_size);
    }
}
