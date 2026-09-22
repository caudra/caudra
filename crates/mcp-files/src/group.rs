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
use crate::model_text::ModelText;
use crate::mutation_operations::PlannedChangeType;
use crate::{
    FilesystemError, FilesystemLimits, PreparedFileEdit, PreparedFileGlob, PreparedFileGrep,
    PreparedFilePatch, PreparedFileRead, PreparedFileWrite,
    glob::GlobMatcher,
    operations::FilesystemCore,
    patch::{PatchHunk, parse_patch},
    read_operations::grep::compile_linear_regex,
    text::{check_cancelled, enforce_bytes},
    types::{
        FileApplyPatchInput, FileApplyPatchOutput, FileEditInput, FileEditOutput, FileGlobInput,
        FileGlobOutput, FileGrepInput, FileGrepOutput, FileReadInput, FileReadOutput, FileResource,
        FileResourceAccess, FileWriteInput, FileWriteOutput,
    },
};
#[cfg(feature = "index")]
use crate::{
    INDEX_MAX_PATH_BYTES, IndexExecutionConfiguration, IndexInput, IndexOutput, PreparedFileIndex,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
/// Context lines per side. Past this a search is really a file read.
const MAX_GREP_CONTEXT_LINES: usize = 100;
const PREPARED_DIRECTORY_REQUIRED: &str = "Directory listing requires a prepared directory read";
// This is a protocol compatibility bound, not a deployment tuning default.
pub(crate) const MCP_RAW_RESULT_CEILING_BYTES: usize = 64_000;
const TRANSPORT_FRAME_DELIMITER_BYTES: usize = 1;
#[cfg(all(feature = "index", feature = "mcp"))]
const INDEX_TRUNCATED: &str = "[truncated]";

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
    pub(crate) core: Arc<FilesystemCore>,
    pub(crate) workspace: Arc<crate::workspace::WorkspaceState>,
}

impl FileToolGroup {
    pub async fn new(
        root: impl AsRef<Path>,
        allow_write: bool,
        limits: Option<FilesystemLimits>,
    ) -> Result<Self, FilesystemError> {
        Ok(Self {
            core: Arc::new(FilesystemCore::create(root.as_ref(), allow_write, limits).await?),
            workspace: Arc::new(crate::workspace::WorkspaceState::default()),
        })
    }

    /// Construct a host-managed filesystem where `base_cwd` only anchors relative-path resolution.
    ///
    /// This is the native-hosting constructor and it is deliberately fail-open: the calling host is
    /// the policy layer. Both root confinement and protected-path denial are disabled, so absolute
    /// paths and `..` traversal resolve anywhere the process can reach, and credential-bearing
    /// entries such as `.env`, `.ssh`, `.netrc`, `*.key`, and `id_rsa` are reachable for read and,
    /// when `allow_write` is set, for mutation. Broad traversal reports those entries too, so an
    /// authorizing host can enumerate exactly what a call would touch.
    ///
    /// Hosts must authorize the [`FileResource`] values a prepared operation exposes before
    /// committing it. Pass `allow_write = false` for inspection-only hosting.
    pub async fn new_unconfined(
        base_cwd: impl AsRef<Path>,
        allow_write: bool,
        limits: Option<FilesystemLimits>,
    ) -> Result<Self, FilesystemError> {
        Ok(Self {
            core: Arc::new(
                FilesystemCore::create_unconfined(base_cwd.as_ref(), allow_write, limits).await?,
            ),
            workspace: Arc::new(crate::workspace::WorkspaceState::default()),
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

    pub async fn resolve_directory(
        &self,
        requested: &str,
    ) -> Result<crate::ResolvedDirectory, FilesystemError> {
        let (path, relative_path) = self.core.resolve_directory(requested).await?;
        Ok(crate::ResolvedDirectory {
            path,
            relative_path,
        })
    }

    #[cfg(feature = "mcp")]
    pub fn catalog(&self) -> Vec<Tool> {
        catalog(self.core.allow_write)
    }

    pub async fn file_read(
        &self,
        input: FileReadInput,
        token: &CancellationToken,
    ) -> Result<FileReadOutput, FilesystemError> {
        self.execute_prepared_read(self.prepare_read(input, token).await?, token)
            .await
    }

    pub async fn prepare_read(
        &self,
        input: FileReadInput,
        token: &CancellationToken,
    ) -> Result<PreparedFileRead, FilesystemError> {
        check_cancelled(token)?;
        let input = validate_read(input)?;
        let offset = input.offset.unwrap_or(1);
        let limit = input.limit.unwrap_or(self.core.limits.max_read_lines);
        if limit > self.core.limits.max_read_lines {
            return Err(FilesystemError::message(format!(
                "limit must be an integer between 0 and {}",
                self.core.limits.max_read_lines
            )));
        }
        let path = self.core.policy.resolve(&input.file_path).await?;
        let metadata = inspect_existing(&path, &input.file_path).await?;
        let access = if metadata.is_dir() {
            FileResourceAccess::Traverse
        } else if metadata.is_file() {
            FileResourceAccess::Read
        } else {
            return Err(FilesystemError::message(format!(
                "Path is not a regular file: {}",
                input.file_path
            )));
        };
        let relative_path = self.core.policy.relative(&path)?;
        Ok(PreparedFileRead {
            core: self.core.clone(),
            resources: [FileResource {
                requested_path: input.file_path,
                path,
                root_relative_path: relative_path.clone(),
                access,
            }],
            relative_paths: [relative_path],
            offset,
            limit,
        })
    }

    pub async fn execute_prepared_read(
        &self,
        prepared: PreparedFileRead,
        token: &CancellationToken,
    ) -> Result<FileReadOutput, FilesystemError> {
        ensure_prepared_core(&self.core, &prepared.core)?;
        check_cancelled(token)?;
        let PreparedFileRead {
            resources: [resource],
            relative_paths: [relative_path],
            offset,
            limit,
            ..
        } = prepared;
        self.core.revalidate_resource(&resource).await?;
        self.core
            .file_read_prepared(resource, relative_path, offset, limit, token)
            .await
    }

    pub async fn execute_prepared_directory_read(
        &self,
        prepared: PreparedFileRead,
        token: &CancellationToken,
    ) -> Result<FileReadOutput, FilesystemError> {
        if prepared.resource().access != FileResourceAccess::Traverse {
            return Err(FilesystemError::message(PREPARED_DIRECTORY_REQUIRED));
        }
        self.execute_prepared_read(prepared, token).await
    }

    pub async fn file_glob(
        &self,
        input: FileGlobInput,
        token: &CancellationToken,
    ) -> Result<FileGlobOutput, FilesystemError> {
        self.execute_prepared_glob(self.prepare_glob(input, token).await?, token)
            .await
    }

    pub async fn prepare_glob(
        &self,
        input: FileGlobInput,
        token: &CancellationToken,
    ) -> Result<PreparedFileGlob, FilesystemError> {
        check_cancelled(token)?;
        let input = validate_glob(input)?;
        let matcher = GlobMatcher::new(&input.pattern, &self.core.limits)?;
        let requested_path = input.path.as_deref().unwrap_or(".");
        let path = self.core.policy.resolve(requested_path).await?;
        let metadata = inspect_existing(&path, requested_path).await?;
        if !metadata.is_dir() {
            return Err(FilesystemError::message(format!(
                "glob path must be a directory: {requested_path}"
            )));
        }
        let relative_path = self.core.policy.relative(&path)?;
        Ok(PreparedFileGlob {
            core: self.core.clone(),
            resources: [FileResource {
                requested_path: requested_path.to_owned(),
                path,
                root_relative_path: relative_path.clone(),
                access: FileResourceAccess::Traverse,
            }],
            relative_paths: [relative_path],
            pattern: input.pattern,
            matcher,
        })
    }

    pub async fn execute_prepared_glob(
        &self,
        prepared: PreparedFileGlob,
        token: &CancellationToken,
    ) -> Result<FileGlobOutput, FilesystemError> {
        ensure_prepared_core(&self.core, &prepared.core)?;
        check_cancelled(token)?;
        let PreparedFileGlob {
            resources: [resource],
            relative_paths: [relative_path],
            pattern,
            matcher,
            ..
        } = prepared;
        self.core.revalidate_resource(&resource).await?;
        self.core
            .file_glob_prepared(resource.path, relative_path, pattern, matcher, token)
            .await
    }

    pub async fn file_grep(
        &self,
        input: FileGrepInput,
        token: &CancellationToken,
    ) -> Result<FileGrepOutput, FilesystemError> {
        self.execute_prepared_grep(self.prepare_grep(input, token).await?, token)
            .await
    }

    pub async fn prepare_grep(
        &self,
        input: FileGrepInput,
        token: &CancellationToken,
    ) -> Result<PreparedFileGrep, FilesystemError> {
        check_cancelled(token)?;
        let input = validate_grep(input)?;
        let regex = compile_linear_regex(&input.pattern, self.core.limits.max_regex_length)?;
        let include_matcher = input
            .include
            .as_deref()
            .map(|pattern| GlobMatcher::new(pattern, &self.core.limits))
            .transpose()?;
        let requested_path = input.path.as_deref().unwrap_or(".");
        let path = self.core.policy.resolve(requested_path).await?;
        let metadata = inspect_existing(&path, requested_path).await?;
        let access = if metadata.is_file() {
            FileResourceAccess::Read
        } else if metadata.is_dir() {
            FileResourceAccess::Traverse
        } else {
            return Err(FilesystemError::message(format!(
                "Path is not a regular file or directory: {requested_path}"
            )));
        };
        let relative_path = self.core.policy.relative(&path)?;
        Ok(PreparedFileGrep {
            core: self.core.clone(),
            resources: [FileResource {
                requested_path: requested_path.to_owned(),
                path,
                root_relative_path: relative_path.clone(),
                access,
            }],
            relative_paths: [relative_path],
            context_before: input.before(),
            context_after: input.after(),
            head_limit: input.head_limit,
            pattern: input.pattern,
            include: input.include,
            regex,
            include_matcher,
        })
    }

    pub async fn execute_prepared_grep(
        &self,
        prepared: PreparedFileGrep,
        token: &CancellationToken,
    ) -> Result<FileGrepOutput, FilesystemError> {
        ensure_prepared_core(&self.core, &prepared.core)?;
        check_cancelled(token)?;
        self.core.revalidate_resource(prepared.resource()).await?;
        self.core.file_grep_prepared(prepared, token).await
    }

    pub async fn file_write(
        &self,
        input: FileWriteInput,
        token: &CancellationToken,
    ) -> Result<FileWriteOutput, FilesystemError> {
        self.execute_prepared_write(self.prepare_write(input, token).await?, token)
            .await
    }

    pub async fn prepare_write(
        &self,
        input: FileWriteInput,
        token: &CancellationToken,
    ) -> Result<PreparedFileWrite, FilesystemError> {
        self.prepare_write_bounded(input, usize::MAX, token).await
    }

    pub async fn prepare_write_bounded(
        &self,
        input: FileWriteInput,
        maximum_retained_bytes: usize,
        token: &CancellationToken,
    ) -> Result<PreparedFileWrite, FilesystemError> {
        let input = validate_write(input)?;
        let _guard = self.core.mutation.lock().await;
        let path = self.core.policy.resolve(&input.file_path).await?;
        let relative_path = self.core.policy.relative(&path)?;
        let requested_path = input.file_path.clone();
        let (content, expected, preview) = self
            .core
            .prepare_write_bounded(
                &path,
                relative_path.clone(),
                input,
                maximum_retained_bytes,
                token,
            )
            .await?;
        let access = if expected.is_some() {
            FileResourceAccess::ReadWrite
        } else {
            FileResourceAccess::Write
        };
        Ok(PreparedFileWrite {
            core: self.core.clone(),
            resources: [FileResource {
                requested_path,
                path,
                root_relative_path: relative_path.clone(),
                access,
            }],
            relative_paths: [relative_path],
            content,
            expected,
            preview,
        })
    }

    pub async fn execute_prepared_write(
        &self,
        prepared: PreparedFileWrite,
        token: &CancellationToken,
    ) -> Result<FileWriteOutput, FilesystemError> {
        ensure_prepared_core(&self.core, &prepared.core)?;
        check_cancelled(token)?;
        let PreparedFileWrite {
            resources: [resource],
            content,
            expected,
            mut preview,
            ..
        } = prepared;
        let _guard = self.core.mutation.lock().await;
        self.core.require_write()?;
        self.core
            .commit_write(
                &resource.requested_path,
                &resource.path,
                &content,
                token,
                expected.is_none(),
                expected.as_ref(),
            )
            .await?;
        preview.applied = true;
        Ok(preview)
    }

    pub async fn file_edit(
        &self,
        input: FileEditInput,
        token: &CancellationToken,
    ) -> Result<FileEditOutput, FilesystemError> {
        self.execute_prepared_edit(self.prepare_edit(input, token).await?, token)
            .await
    }

    pub async fn prepare_edit(
        &self,
        input: FileEditInput,
        token: &CancellationToken,
    ) -> Result<PreparedFileEdit, FilesystemError> {
        self.prepare_edit_bounded(input, usize::MAX, token).await
    }

    pub async fn prepare_edit_bounded(
        &self,
        input: FileEditInput,
        maximum_retained_bytes: usize,
        token: &CancellationToken,
    ) -> Result<PreparedFileEdit, FilesystemError> {
        let input = validate_edit(input)?;
        let _guard = self.core.mutation.lock().await;
        let path = self.core.policy.resolve(&input.file_path).await?;
        let relative_path = self.core.policy.relative(&path)?;
        let requested_path = input.file_path.clone();
        let (content, expected, preview) = self
            .core
            .prepare_edit_bounded(
                &path,
                relative_path.clone(),
                input,
                maximum_retained_bytes,
                token,
            )
            .await?;
        Ok(PreparedFileEdit {
            core: self.core.clone(),
            resources: [FileResource {
                requested_path,
                path,
                root_relative_path: relative_path.clone(),
                access: FileResourceAccess::ReadWrite,
            }],
            relative_paths: [relative_path],
            content,
            expected,
            preview,
        })
    }

    pub async fn execute_prepared_edit(
        &self,
        prepared: PreparedFileEdit,
        token: &CancellationToken,
    ) -> Result<FileEditOutput, FilesystemError> {
        ensure_prepared_core(&self.core, &prepared.core)?;
        check_cancelled(token)?;
        let PreparedFileEdit {
            resources: [resource],
            content,
            expected,
            mut preview,
            ..
        } = prepared;
        let _guard = self.core.mutation.lock().await;
        self.core.require_write()?;
        self.core
            .commit_write(
                &resource.requested_path,
                &resource.path,
                &content,
                token,
                false,
                Some(&expected),
            )
            .await?;
        preview.applied = true;
        Ok(preview)
    }

    pub async fn file_apply_patch(
        &self,
        input: FileApplyPatchInput,
        token: &CancellationToken,
    ) -> Result<FileApplyPatchOutput, FilesystemError> {
        self.execute_prepared_patch(self.prepare_apply_patch(input, token).await?, token)
            .await
    }

    #[cfg(feature = "index")]
    pub async fn index(
        &self,
        input: IndexInput,
        token: &CancellationToken,
    ) -> Result<IndexOutput, FilesystemError> {
        self.execute_prepared_index(self.prepare_index(input, token).await?, token)
            .await
    }

    #[cfg(feature = "index")]
    pub async fn index_with_configuration(
        &self,
        input: IndexInput,
        configuration: IndexExecutionConfiguration,
        token: &CancellationToken,
    ) -> Result<IndexOutput, FilesystemError> {
        self.execute_prepared_index(
            self.prepare_index_with_configuration(input, configuration, token)
                .await?,
            token,
        )
        .await
    }

    #[cfg(feature = "index")]
    pub async fn prepare_index(
        &self,
        input: IndexInput,
        token: &CancellationToken,
    ) -> Result<PreparedFileIndex, FilesystemError> {
        self.prepare_index_with_configuration(input, IndexExecutionConfiguration::default(), token)
            .await
    }

    #[cfg(feature = "index")]
    pub async fn prepare_index_with_configuration(
        &self,
        input: IndexInput,
        configuration: IndexExecutionConfiguration,
        token: &CancellationToken,
    ) -> Result<PreparedFileIndex, FilesystemError> {
        check_cancelled(token)?;
        let input = validate_index(input)?;
        let limits = configuration.limits.validate()?;
        let path = self.core.policy.resolve(&input.path).await?;
        let metadata = inspect_existing(&path, &input.path).await?;
        let access = if metadata.is_dir() {
            FileResourceAccess::Traverse
        } else if metadata.is_file() {
            FileResourceAccess::Read
        } else {
            return Err(FilesystemError::message(format!(
                "Path is not a regular file or directory: {}",
                input.path
            )));
        };
        let relative_path = self.core.policy.relative(&path)?;
        Ok(PreparedFileIndex {
            core: self.core.clone(),
            resources: [FileResource {
                requested_path: input.path,
                path,
                root_relative_path: relative_path.clone(),
                access,
            }],
            relative_paths: [relative_path],
            limits,
        })
    }

    #[cfg(feature = "index")]
    pub async fn execute_prepared_index(
        &self,
        prepared: PreparedFileIndex,
        token: &CancellationToken,
    ) -> Result<IndexOutput, FilesystemError> {
        ensure_prepared_core(&self.core, &prepared.core)?;
        check_cancelled(token)?;
        let PreparedFileIndex {
            resources: [resource],
            limits,
            ..
        } = prepared;
        self.core
            .index_authorized_with_limits(resource, limits, token)
            .await
    }

    #[cfg(feature = "index")]
    /// Indexes a resource returned by [`Self::inspect_index`], rejecting path or type changes.
    pub async fn index_authorized_with_configuration(
        &self,
        resource: FileResource,
        configuration: IndexExecutionConfiguration,
        token: &CancellationToken,
    ) -> Result<IndexOutput, FilesystemError> {
        self.core
            .index_authorized(resource, configuration, token)
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

    #[cfg(feature = "index")]
    pub async fn inspect_index(&self, input: &IndexInput) -> Result<FileResource, FilesystemError> {
        validate_index(input.clone())?;
        let path = self.core.policy.resolve(&input.path).await?;
        let metadata = tokio::fs::metadata(&path).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                FilesystemError::not_found(format!("Path not found: {}", input.path))
            } else {
                FilesystemError::io_path("Cannot inspect", &path, error)
            }
        })?;
        let access = if metadata.is_dir() {
            FileResourceAccess::Traverse
        } else if metadata.is_file() {
            FileResourceAccess::Read
        } else {
            return Err(FilesystemError::message(format!(
                "Path is not a regular file or directory: {}",
                input.path
            )));
        };
        let root_relative_path = self.core.policy.relative(&path)?;
        Ok(FileResource {
            requested_path: input.path.clone(),
            path,
            root_relative_path,
            access,
        })
    }

    /// Fully plan a patch for host authorization without mutating the filesystem.
    /// Planning does not require write access; publication occurs only through
    /// `execute_prepared_patch`, which does.
    pub async fn prepare_apply_patch(
        &self,
        input: FileApplyPatchInput,
        token: &CancellationToken,
    ) -> Result<PreparedFilePatch, FilesystemError> {
        self.prepare_apply_patch_bounded(input, self.core.limits.max_patch_plan_bytes, token)
            .await
    }

    pub async fn prepare_apply_patch_bounded(
        &self,
        input: FileApplyPatchInput,
        maximum_retained_bytes: usize,
        token: &CancellationToken,
    ) -> Result<PreparedFilePatch, FilesystemError> {
        let input = validate_patch(input)?;
        let _guard = self.core.mutation.lock().await;
        let (changes, preview) = self
            .core
            .prepare_patch_bounded(&input.patch_text, maximum_retained_bytes, token)
            .await?;
        let mut resources = Vec::with_capacity(changes.len());
        let mut relative_paths = Vec::with_capacity(changes.len());
        for change in &changes {
            resources.push(FileResource {
                requested_path: change.requested_path.clone(),
                path: change.file_path.clone(),
                root_relative_path: self.core.policy.relative(&change.file_path)?,
                access: match change.change_type {
                    PlannedChangeType::Add => FileResourceAccess::Write,
                    PlannedChangeType::Update => FileResourceAccess::ReadWrite,
                    PlannedChangeType::Delete | PlannedChangeType::Move => {
                        FileResourceAccess::Delete
                    }
                },
            });
            relative_paths.push(self.core.policy.relative(&change.file_path)?);
            if let Some(path) = &change.move_path {
                resources.push(FileResource {
                    requested_path: change
                        .requested_move_path
                        .clone()
                        .expect("move has requested target"),
                    path: path.clone(),
                    root_relative_path: self.core.policy.relative(path)?,
                    access: FileResourceAccess::Write,
                });
                relative_paths.push(self.core.policy.relative(path)?);
            }
        }
        let prepared = PreparedFilePatch {
            core: self.core.clone(),
            changes,
            preview,
            resources,
            relative_paths,
        };
        if prepared.retained_bytes() > maximum_retained_bytes {
            return Err(FilesystemError::message(format!(
                "Patch plan exceeds maximum retained size of {maximum_retained_bytes} bytes"
            )));
        }
        Ok(prepared)
    }

    /// Publish an authorized prepared patch exactly once, without replanning.
    pub async fn execute_prepared_patch(
        &self,
        prepared: PreparedFilePatch,
        token: &CancellationToken,
    ) -> Result<FileApplyPatchOutput, FilesystemError> {
        ensure_prepared_core(&self.core, &prepared.core)?;
        check_cancelled(token)?;
        let _guard = self.core.mutation.lock().await;
        self.core
            .publish_prepared_patch(&prepared.changes, token)
            .await?;
        let mut output = prepared.preview;
        output.applied = true;
        Ok(output)
    }

    /// Resolves a caller-supplied path through the same confinement policy the
    /// tools use, so a host that moves bytes over a side channel authorizes the
    /// identical resource set. Resolution alone is not permission: `Write`
    /// callers must still check [`Self::allow_write`].
    pub async fn inspect_path(
        &self,
        requested_path: &str,
        access: FileResourceAccess,
    ) -> Result<FileResource, FilesystemError> {
        let path = self.core.policy.resolve(requested_path).await?;
        let root_relative_path = self.core.policy.relative(&path)?;
        Ok(FileResource {
            requested_path: requested_path.to_owned(),
            path,
            root_relative_path,
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
                Ok(input) => run_search(self.file_glob(input, &token).await),
                Err(error) => tool_error(error),
            },
            "file_grep" => match parse_arguments::<FileGrepInput>(name, arguments) {
                Ok(input) => run_search(self.file_grep(input, &token).await),
                Err(error) => tool_error(error),
            },
            // Resolution must match enumeration: a read-only catalog omits these
            // names, so this group does not own them and must not answer for them.
            "file_write" if self.core.allow_write => {
                match parse_arguments::<FileWriteInput>(name, arguments) {
                    Ok(input) => run(self.file_write(input, &token).await),
                    Err(error) => tool_error(error),
                }
            }
            "file_edit" if self.core.allow_write => {
                match parse_arguments::<FileEditInput>(name, arguments) {
                    Ok(input) => run(self.file_edit(input, &token).await),
                    Err(error) => tool_error(error),
                }
            }
            "file_apply_patch" if self.core.allow_write => {
                match parse_arguments::<FileApplyPatchInput>(name, arguments) {
                    Ok(input) => run(self.file_apply_patch(input, &token).await),
                    Err(error) => tool_error(error),
                }
            }
            #[cfg(feature = "index")]
            "file_index" => match parse_arguments::<IndexInput>(name, arguments) {
                Ok(input) => run_index(self.index(input, &token).await),
                Err(error) => tool_error(error),
            },
            _ => return None,
        };
        Some(result)
    }
}

fn ensure_prepared_core(
    expected: &Arc<FilesystemCore>,
    prepared: &Arc<FilesystemCore>,
) -> Result<(), FilesystemError> {
    if Arc::ptr_eq(expected, prepared) {
        Ok(())
    } else {
        Err(FilesystemError::message(
            "Prepared operation belongs to a different file tool group",
        ))
    }
}

async fn inspect_existing(
    path: &Path,
    requested_path: &str,
) -> Result<std::fs::Metadata, FilesystemError> {
    tokio::fs::metadata(path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            FilesystemError::not_found(format!("Path not found: {requested_path}"))
        } else {
            FilesystemError::io_path("Cannot inspect", path, error)
        }
    })
}

#[cfg(feature = "mcp")]
fn parse_arguments<T: DeserializeOwned>(name: &str, value: Value) -> Result<T, String> {
    serde_json::from_value(value)
        .map_err(|error| format!("Invalid arguments for tool {name}: {error}"))
}

#[cfg(feature = "mcp")]
fn run<T: Serialize + ModelText>(
    result: Result<T, FilesystemError>,
) -> Result<CallToolResult, rmcp::ErrorData> {
    match result {
        Ok(output) => success(output),
        Err(error) => tool_error(error),
    }
}

/// A search result whose returned rows can be shortened to fit the protocol
/// result ceiling.
///
/// `max_search_results` is a coarse count bound; a single grep row can carry a
/// full `max_line_length` line, so the byte ceiling is the binding constraint
/// for wide results and must be enforced rather than assumed.
#[cfg(feature = "mcp")]
pub(crate) trait BoundedSearchResult: Serialize + ModelText + Clone {
    fn returned_len(&self) -> usize;
    fn retain_prefix(&mut self, retained: usize);
}

#[cfg(feature = "mcp")]
impl BoundedSearchResult for FileGlobOutput {
    fn returned_len(&self) -> usize {
        self.files.len()
    }

    fn retain_prefix(&mut self, retained: usize) {
        self.files.truncate(retained);
        self.count = self.files.len();
        self.truncated = true;
    }
}

#[cfg(feature = "mcp")]
impl BoundedSearchResult for FileGrepOutput {
    fn returned_len(&self) -> usize {
        self.rows.len()
    }

    fn retain_prefix(&mut self, retained: usize) {
        self.rows.truncate(retained);
        self.matches = self.rows.len();
        self.truncated = true;
    }
}

#[cfg(feature = "mcp")]
fn run_search<T: BoundedSearchResult>(
    result: Result<T, FilesystemError>,
) -> Result<CallToolResult, rmcp::ErrorData> {
    match result {
        Ok(output) => {
            let output = fit_search_result(output).map_err(|error| {
                rmcp::ErrorData::internal_error(
                    "Failed to serialize filesystem tool result",
                    Some(Value::String(error.to_string())),
                )
            })?;
            success(output)
        }
        Err(error) => tool_error(error),
    }
}

#[cfg(feature = "mcp")]
fn fit_search_result<T: BoundedSearchResult>(output: T) -> Result<T, serde_json::Error> {
    if mcp_response_size(&output)? <= MCP_RAW_RESULT_CEILING_BYTES {
        return Ok(output);
    }
    // Binary search the largest retained prefix that fits. Dropping rows keeps
    // the result usable and honestly marked instead of failing the call.
    let mut lower = 0usize;
    let mut upper = output.returned_len();
    while lower < upper {
        let candidate = lower + (upper - lower).div_ceil(2);
        let mut probe = output.clone();
        probe.retain_prefix(candidate);
        if mcp_response_size(&probe)? <= MCP_RAW_RESULT_CEILING_BYTES {
            lower = candidate;
        } else {
            upper = candidate - 1;
        }
    }
    let mut fitted = output;
    fitted.retain_prefix(lower);
    Ok(fitted)
}

#[cfg(feature = "mcp")]
pub fn fit_glob_output(output: FileGlobOutput) -> Result<FileGlobOutput, serde_json::Error> {
    fit_search_result(output)
}

#[cfg(feature = "mcp")]
pub fn fit_grep_output(output: FileGrepOutput) -> Result<FileGrepOutput, serde_json::Error> {
    fit_search_result(output)
}

#[cfg(feature = "mcp")]
fn success(output: impl Serialize + ModelText) -> Result<CallToolResult, rmcp::ErrorData> {
    build_success_result(&output).map_err(|error| {
        rmcp::ErrorData::internal_error(
            "Failed to serialize filesystem tool result",
            Some(Value::String(error.to_string())),
        )
    })
}

#[cfg(all(feature = "index", feature = "mcp"))]
fn run_index(
    result: Result<IndexOutput, FilesystemError>,
) -> Result<CallToolResult, rmcp::ErrorData> {
    match result {
        Ok(output) => {
            let output = fit_index_output(output).map_err(|_| {
                rmcp::ErrorData::internal_error("Failed to serialize index result", None)
            })?;
            let structured = serde_json::to_value(&output).map_err(|_| {
                rmcp::ErrorData::internal_error("Failed to serialize index result", None)
            })?;
            let mut result = CallToolResult::default();
            result.content = vec![ContentBlock::text(output.model_text().to_owned())];
            result.structured_content = Some(structured);
            Ok(result)
        }
        Err(error) => tool_error(error),
    }
}

#[cfg(all(feature = "index", feature = "mcp"))]
pub fn fit_index_output(output: IndexOutput) -> Result<IndexOutput, serde_json::Error> {
    if index_response_size(&output)? <= MCP_RAW_RESULT_CEILING_BYTES {
        return Ok(output);
    }

    let retained = index_payload_len(&output);
    let mut template = output;
    if index_response_size(&index_prefix(&template, 0))? > MCP_RAW_RESULT_CEILING_BYTES {
        truncate_index_paths(&mut template);
    }
    if index_response_size(&index_prefix(&template, 0))? > MCP_RAW_RESULT_CEILING_BYTES
        && let IndexOutput::File { language, .. } = &mut template
    {
        *language = INDEX_TRUNCATED.to_owned();
    }

    let mut lower = 0usize;
    let mut upper = retained;
    while lower < upper {
        let candidate = lower + (upper - lower).div_ceil(2);
        if index_response_size(&index_prefix(&template, candidate))? <= MCP_RAW_RESULT_CEILING_BYTES
        {
            lower = candidate;
        } else {
            upper = candidate - 1;
        }
    }
    let fitted = index_prefix(&template, lower);
    debug_assert!(index_response_size(&fitted)? <= MCP_RAW_RESULT_CEILING_BYTES);
    Ok(fitted)
}

#[cfg(all(feature = "index", feature = "mcp"))]
fn index_response_size(output: &IndexOutput) -> Result<usize, serde_json::Error> {
    let structured = serde_json::to_value(output)?;
    mcp_response_size_from_parts(&structured, output.model_text())
}

#[cfg(all(feature = "index", feature = "mcp"))]
fn index_payload_len(output: &IndexOutput) -> usize {
    match output {
        IndexOutput::File { lines, .. } => lines.len().saturating_sub(usize::from(
            lines
                .last()
                .is_some_and(|line| line.text == INDEX_TRUNCATED),
        )),
        IndexOutput::Directory { entries, .. } => entries.len(),
    }
}

#[cfg(all(feature = "index", feature = "mcp"))]
fn index_prefix(output: &IndexOutput, retained: usize) -> IndexOutput {
    let mut output = output.clone();
    match &mut output {
        IndexOutput::File {
            skeleton,
            lines,
            truncated,
            ..
        } => {
            let available = lines.len().saturating_sub(usize::from(
                lines
                    .last()
                    .is_some_and(|line| line.text == INDEX_TRUNCATED),
            ));
            lines.truncate(retained.min(available));
            lines.push(crate::IndexOutputLine {
                output_line: lines.len() + 1,
                text: INDEX_TRUNCATED.to_owned(),
                semantic: crate::IndexLineSemantic::Dimmed,
                body: None,
                source_range: None,
            });
            for (index, line) in lines.iter_mut().enumerate() {
                line.output_line = index + 1;
            }
            *skeleton = lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            *truncated = true;
        }
        IndexOutput::Directory {
            entries,
            truncated,
            listing,
            ..
        } => {
            entries.truncate(retained.min(entries.len()));
            let mut lines = entries
                .iter()
                .map(|entry| match entry.kind {
                    crate::IndexDirectoryEntryKind::Directory => format!("{}/", entry.name),
                    crate::IndexDirectoryEntryKind::File => entry.name.clone(),
                })
                .collect::<Vec<_>>();
            lines.push(INDEX_TRUNCATED.to_owned());
            *listing = lines.join("\n");
            *truncated = true;
        }
    }
    output
}

#[cfg(all(feature = "index", feature = "mcp"))]
fn truncate_index_paths(output: &mut IndexOutput) {
    match output {
        IndexOutput::File {
            path,
            relative_path,
            ..
        }
        | IndexOutput::Directory {
            path,
            relative_path,
            ..
        } => {
            *path = INDEX_TRUNCATED.to_owned();
            *relative_path = INDEX_TRUNCATED.to_owned();
        }
    }
}

#[cfg(feature = "mcp")]
fn build_success_result(
    output: &(impl Serialize + ModelText),
) -> Result<CallToolResult, serde_json::Error> {
    let structured = serde_json::to_value(output)?;
    let mut result = CallToolResult::default();
    result.content = vec![ContentBlock::text(output.model_text().into_owned())];
    result.structured_content = Some(structured);
    Ok(result)
}

pub(crate) fn mcp_response_size(
    output: &(impl Serialize + ModelText),
) -> Result<usize, serde_json::Error> {
    let structured = serde_json::to_value(output)?;
    mcp_response_size_from_parts(&structured, &output.model_text())
}

fn mcp_response_size_from_parts(
    structured: &Value,
    text: &str,
) -> Result<usize, serde_json::Error> {
    let result = SerializedToolResult {
        result_type: "complete",
        content: [SerializedTextContent {
            content_type: "text",
            text,
        }],
        structured_content: structured,
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

fn validate_read(mut input: FileReadInput) -> Result<FileReadInput, FilesystemError> {
    // "An empty filePath is treated as `.` and reads the file root directory." The description has
    // always said so; rejecting it instead cost a caller a turn for believing it. `filePath` is
    // required, so this maps onto the root rather than onto absent.
    if input.file_path.is_empty() {
        input.file_path = ".".to_owned();
    }
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

fn validate_glob(mut input: FileGlobInput) -> Result<FileGlobInput, FilesystemError> {
    // `pattern` is required and has no default, so an empty one is still a caller error.
    nonempty(&input.pattern, "pattern")?;
    // "An empty path is treated as `.`." Absent already means the file root, and `.` resolves to
    // the same directory, so the two spellings are folded into one here.
    if input.path.as_deref() == Some("") {
        input.path = None;
    }
    Ok(input)
}

fn validate_grep(mut input: FileGrepInput) -> Result<FileGrepInput, FilesystemError> {
    nonempty(&input.pattern, "pattern")?;
    // "An empty path is treated as `.`, and an empty include filter is ignored." Ignoring a filter
    // is exactly what absent means, so both fold onto absent.
    if input.path.as_deref() == Some("") {
        input.path = None;
    }
    if input.include.as_deref() == Some("") {
        input.include = None;
    }
    // A caller asking for more context than this wants the file, not a search,
    // so the request is clamped rather than refused.
    for lines in [
        &mut input.context_after,
        &mut input.context_before,
        &mut input.context,
    ] {
        if let Some(lines) = lines.as_mut() {
            *lines = (*lines).min(MAX_GREP_CONTEXT_LINES);
        }
    }
    // Zero would return nothing and read as a bug in the tool rather than in the
    // call, so it folds onto absent.
    if input.head_limit == Some(0) {
        input.head_limit = None;
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

#[cfg(feature = "index")]
fn validate_index(input: IndexInput) -> Result<IndexInput, FilesystemError> {
    nonempty(&input.path, "path")?;
    enforce_bytes("path", &input.path, INDEX_MAX_PATH_BYTES)?;
    Ok(input)
}

#[cfg(all(test, feature = "mcp"))]
mod tests {
    #[cfg(feature = "index")]
    use super::{
        ContentBlock, INDEX_TRUNCATED, mcp_response_size_from_parts, run_index, validate_index,
    };
    use super::{
        MCP_RAW_RESULT_CEILING_BYTES, ModelText, SerializedTextContent, SerializedToolResult,
        build_success_result, fit_search_result, mcp_response_size,
    };
    use crate::types::{FileGrepOutput, FileGrepRow};
    #[cfg(feature = "index")]
    use crate::{IndexDirectoryEntry, IndexDirectoryEntryKind};
    #[cfg(feature = "index")]
    use crate::{IndexLineSemantic, IndexOutput, IndexOutputLine, IndexSourceRange};

    /// `max_search_results` is a count bound, but one grep row can carry a whole
    /// `max_line_length` line. Without a byte bound a legal result already
    /// exceeded the protocol ceiling, so the ceiling is enforced directly and
    /// the shortened result is marked truncated rather than failing the call.
    #[test]
    fn wide_search_results_are_fitted_to_the_protocol_ceiling() {
        let rows = (0..500)
            .map(|line| FileGrepRow {
                path: "/root/a.txt".into(),
                relative_path: "a.txt".into(),
                line,
                text: "x".repeat(2_000),
                matched: true,
            })
            .collect::<Vec<_>>();
        let output = FileGrepOutput {
            cwd: "/root".into(),
            relative_path: ".".into(),
            pattern: "x".into(),
            include: None,
            matches: rows.len(),
            files_scanned: 1,
            files_listed: 1,
            rows,
            truncated: false,
            ignored: 0,
            ignore_complete: true,
            revisions: Default::default(),
        };
        assert!(
            mcp_response_size(&output).expect("size") > MCP_RAW_RESULT_CEILING_BYTES,
            "the unbounded result must exceed the ceiling for this test to mean anything"
        );

        let fitted = fit_search_result(output).expect("fitted result");
        assert!(mcp_response_size(&fitted).expect("size") <= MCP_RAW_RESULT_CEILING_BYTES);
        assert!(fitted.truncated);
        assert!(!fitted.rows.is_empty(), "a usable prefix must survive");
        assert_eq!(fitted.matches, fitted.rows.len());
    }

    #[test]
    fn response_size_counts_both_result_forms_and_envelope() {
        let output = FileGrepOutput {
            cwd: "/root".into(),
            relative_path: ".".into(),
            pattern: "x".into(),
            include: None,
            rows: vec![FileGrepRow {
                path: "/root/a.txt".into(),
                relative_path: "a.txt".into(),
                line: 1,
                text: "x".repeat(1_000),
                matched: true,
            }],
            matches: 1,
            files_scanned: 1,
            files_listed: 1,
            truncated: false,
            ignored: 0,
            ignore_complete: true,
            revisions: Default::default(),
        };
        let structured = serde_json::to_value(&output).expect("structured output");
        let structured_size = serde_json::to_vec(&structured)
            .expect("serialized structured output")
            .len();
        let result = build_success_result(&output).expect("call tool result");
        let result_size = serde_json::to_vec(&result)
            .expect("serialized call tool result")
            .len();

        // The content block carries the rendering and the structured content
        // carries the record, so a result is not two copies of one payload.
        let text = output.model_text();
        assert!(!text.contains("relativePath"));
        assert!(result_size < structured_size * 2);

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
            serde_json::to_value(&result).unwrap()
        );

        let response_size = mcp_response_size(&output).expect("response size");
        assert!(response_size > result_size);
    }

    #[cfg(feature = "index")]
    #[test]
    fn index_path_limit_counts_utf8_bytes() {
        let character = "é";
        let exact = character.repeat(crate::INDEX_MAX_PATH_BYTES / character.len());
        assert!(validate_index(crate::IndexInput { path: exact }).is_ok());
        let oversized = character.repeat(crate::INDEX_MAX_PATH_BYTES / character.len() + 1);
        let error = validate_index(crate::IndexInput { path: oversized }).expect_err("byte bound");
        let expected = format!("{} bytes", crate::INDEX_MAX_PATH_BYTES);
        assert!(error.to_string().contains(&expected), "{error}");
    }

    #[cfg(feature = "index")]
    #[test]
    fn index_dispatch_fits_an_oversized_mcp_envelope() {
        let line = "x".repeat(1_000);
        let lines = (1..=40)
            .map(|output_line| IndexOutputLine {
                output_line,
                text: line.clone(),
                semantic: IndexLineSemantic::Plain,
                body: Some(line.clone()),
                source_range: Some(IndexSourceRange {
                    start_line: output_line,
                    end_line: output_line,
                }),
            })
            .collect::<Vec<_>>();
        let output = IndexOutput::File {
            path: "/workspace/source.rs".into(),
            relative_path: "source.rs".into(),
            language: "rust".into(),
            skeleton: vec![line; lines.len()].join("\n"),
            lines,
            source_line_count: 40,
            parse_error: false,
            truncated: false,
        };
        assert!(output.model_text().len() < 50 * 1024);

        let result = run_index(Ok(output)).expect("tool result");

        assert_ne!(result.is_error, Some(true));
        let structured = result.structured_content.as_ref().unwrap();
        let fitted: IndexOutput = serde_json::from_value(structured.clone()).unwrap();
        let ContentBlock::Text(content) = &result.content[0] else {
            panic!("expected text")
        };
        let IndexOutput::File {
            lines, truncated, ..
        } = &fitted
        else {
            panic!("expected file")
        };
        assert!(truncated);
        assert_eq!(lines.last().unwrap().text, INDEX_TRUNCATED);
        for line in &lines[..lines.len() - 1] {
            assert_eq!(line.body.as_deref(), Some(line.text.as_str()));
            assert_eq!(line.source_range.unwrap().start_line, line.output_line);
        }
        // The outline is the content block, and it is exactly the retained
        // lines, which is what lets the record omit it.
        assert_eq!(
            content.text,
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        );
        // Both published forms are charged against the protocol ceiling.
        assert!(
            mcp_response_size_from_parts(structured, &content.text).unwrap()
                <= MCP_RAW_RESULT_CEILING_BYTES
        );
    }

    #[cfg(feature = "index")]
    #[test]
    fn index_dispatch_fits_complete_directory_entries() {
        let entries = (0..1_000)
            .map(|index| IndexDirectoryEntry {
                name: format!("entry-{index}-{}", "\\".repeat(80)),
                kind: IndexDirectoryEntryKind::File,
            })
            .collect::<Vec<_>>();
        let output = IndexOutput::Directory {
            path: "/workspace".into(),
            relative_path: ".".into(),
            listing: entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            total_count: entries.len(),
            entries,
            truncated: false,
        };

        let result = run_index(Ok(output)).expect("tool result");

        assert_ne!(result.is_error, Some(true));
        let structured = result.structured_content.as_ref().expect("structured");
        let fitted: IndexOutput = serde_json::from_value(structured.clone()).expect("typed output");
        let ContentBlock::Text(content) = &result.content[0] else {
            panic!("expected text")
        };
        let IndexOutput::Directory {
            entries,
            total_count,
            truncated,
            ..
        } = &fitted
        else {
            panic!("expected directory")
        };
        assert!(truncated);
        assert_eq!(*total_count, 1_000);
        assert!(entries.len() < *total_count);
        // The listing is the content block; the record carries the entries.
        assert_eq!(content.text.lines().last(), Some(INDEX_TRUNCATED));
        assert!(
            mcp_response_size_from_parts(structured, &content.text).unwrap()
                <= MCP_RAW_RESULT_CEILING_BYTES
        );
    }
}
