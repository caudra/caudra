use std::{mem::size_of, sync::Arc};

use regex::Regex;

#[cfg(feature = "index")]
use crate::IndexLimits;
use crate::{
    FileApplyPatchOutput, FileEditOutput, FileResource, FileWriteOutput, glob::GlobMatcher,
    mutation_operations::PlannedChange, operations::FilesystemCore, text::FileVersion,
};

macro_rules! prepared_resource_accessors {
    () => {
        #[must_use]
        pub fn resource(&self) -> &FileResource {
            &self.resources[0]
        }

        #[must_use]
        pub fn resources(&self) -> &[FileResource] {
            &self.resources
        }

        #[must_use]
        pub fn relative_path(&self) -> &str {
            &self.relative_paths[0]
        }

        #[must_use]
        pub fn relative_paths(&self) -> &[String] {
            &self.relative_paths
        }
    };
}

/// A validated file read bound to one canonical resource and one line window.
///
/// Prepared operations are deliberately non-cloneable and execution takes ownership:
///
/// ```compile_fail
/// # use tokio_util::sync::CancellationToken;
/// # use workcell_mcp_files::{FileToolGroup, FilesystemError, PreparedFileRead};
/// # async fn execute_twice(
/// #     files: &FileToolGroup,
/// #     prepared: PreparedFileRead,
/// # ) -> Result<(), FilesystemError> {
/// let token = CancellationToken::new();
/// files.execute_prepared_read(prepared, &token).await?;
/// files.execute_prepared_read(prepared, &token).await?;
/// # Ok(())
/// # }
/// ```
pub struct PreparedFileRead {
    pub(crate) core: Arc<FilesystemCore>,
    pub(crate) resources: [FileResource; 1],
    pub(crate) relative_paths: [String; 1],
    pub(crate) offset: usize,
    pub(crate) limit: usize,
}

impl PreparedFileRead {
    prepared_resource_accessors!();

    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.resource().retained_bytes())
            .saturating_add(self.relative_paths[0].capacity())
    }
}

/// A validated file glob bound to one canonical traversal root and compiled pattern.
pub struct PreparedFileGlob {
    pub(crate) core: Arc<FilesystemCore>,
    pub(crate) resources: [FileResource; 1],
    pub(crate) relative_paths: [String; 1],
    pub(crate) pattern: String,
    pub(crate) matcher: GlobMatcher,
}

impl PreparedFileGlob {
    prepared_resource_accessors!();

    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.resource().retained_bytes())
            .saturating_add(self.relative_paths[0].capacity())
            .saturating_add(self.pattern.capacity())
            .saturating_add(self.matcher.retained_bytes())
    }
}

/// A validated file grep bound to one canonical scope and compiled filters.
pub struct PreparedFileGrep {
    pub(crate) core: Arc<FilesystemCore>,
    pub(crate) resources: [FileResource; 1],
    pub(crate) relative_paths: [String; 1],
    pub(crate) pattern: String,
    pub(crate) include: Option<String>,
    pub(crate) regex: Regex,
    pub(crate) include_matcher: Option<GlobMatcher>,
}

impl PreparedFileGrep {
    prepared_resource_accessors!();

    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        const REGEX_ENGINE_RETAINED_BYTES: usize = 10 * 1_024 * 1_024;

        size_of::<Self>()
            .saturating_add(self.resource().retained_bytes())
            .saturating_add(self.relative_paths[0].capacity())
            .saturating_add(self.pattern.capacity())
            .saturating_add(self.include.as_ref().map_or(0, String::capacity))
            .saturating_add(REGEX_ENGINE_RETAINED_BYTES)
            .saturating_add(
                self.include_matcher
                    .as_ref()
                    .map_or(0, GlobMatcher::retained_bytes),
            )
    }
}

/// A fully planned file write that has not been published.
pub struct PreparedFileWrite {
    pub(crate) core: Arc<FilesystemCore>,
    pub(crate) resources: [FileResource; 1],
    pub(crate) relative_paths: [String; 1],
    pub(crate) content: String,
    pub(crate) expected: Option<FileVersion>,
    pub(crate) preview: FileWriteOutput,
}

impl PreparedFileWrite {
    prepared_resource_accessors!();

    #[must_use]
    pub fn preview(&self) -> &FileWriteOutput {
        &self.preview
    }

    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.resource().retained_bytes())
            .saturating_add(self.relative_paths[0].capacity())
            .saturating_add(self.content.capacity())
            .saturating_add(self.preview.retained_bytes())
    }
}

/// A fully planned exact edit that has not been published.
pub struct PreparedFileEdit {
    pub(crate) core: Arc<FilesystemCore>,
    pub(crate) resources: [FileResource; 1],
    pub(crate) relative_paths: [String; 1],
    pub(crate) content: String,
    pub(crate) expected: FileVersion,
    pub(crate) preview: FileEditOutput,
}

impl PreparedFileEdit {
    prepared_resource_accessors!();

    #[must_use]
    pub fn preview(&self) -> &FileEditOutput {
        &self.preview
    }

    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.resource().retained_bytes())
            .saturating_add(self.relative_paths[0].capacity())
            .saturating_add(self.content.capacity())
            .saturating_add(self.preview.retained_bytes())
    }
}

/// A fully planned multi-file patch that has not been published.
pub struct PreparedFilePatch {
    pub(crate) core: Arc<FilesystemCore>,
    pub(crate) changes: Vec<PlannedChange>,
    pub(crate) preview: FileApplyPatchOutput,
    pub(crate) resources: Vec<FileResource>,
    pub(crate) relative_paths: Vec<String>,
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

    #[must_use]
    pub fn relative_paths(&self) -> &[String] {
        &self.relative_paths
    }

    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(
                self.changes
                    .capacity()
                    .saturating_mul(size_of::<PlannedChange>()),
            )
            .saturating_add(
                self.changes
                    .iter()
                    .map(PlannedChange::retained_bytes)
                    .fold(0, usize::saturating_add),
            )
            .saturating_add(self.preview.retained_bytes())
            .saturating_add(
                self.resources
                    .capacity()
                    .saturating_mul(size_of::<FileResource>()),
            )
            .saturating_add(
                self.resources
                    .iter()
                    .map(FileResource::retained_bytes)
                    .fold(0, usize::saturating_add),
            )
            .saturating_add(
                self.relative_paths
                    .capacity()
                    .saturating_mul(size_of::<String>()),
            )
            .saturating_add(
                self.relative_paths
                    .iter()
                    .map(String::capacity)
                    .fold(0, usize::saturating_add),
            )
    }
}

/// A validated source index request bound to one authorized canonical path and configuration.
#[cfg(feature = "index")]
pub struct PreparedFileIndex {
    pub(crate) core: Arc<FilesystemCore>,
    pub(crate) resources: [FileResource; 1],
    pub(crate) relative_paths: [String; 1],
    pub(crate) limits: IndexLimits,
}

#[cfg(feature = "index")]
impl PreparedFileIndex {
    prepared_resource_accessors!();

    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.resource().retained_bytes())
            .saturating_add(self.relative_paths[0].capacity())
    }
}
