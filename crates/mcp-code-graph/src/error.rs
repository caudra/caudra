//! Structured, redacted failures.
//!
//! Variants carry a category and a bounded message, never an underlying I/O or parser error. An
//! attached source error embeds the path it failed on, and a tool error is rendered to a caller and
//! written to a log.

use workcell_mcp_files::FilesystemError;

/// Why a code-graph call could not be answered.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CodeGraphError {
    /// Confinement or a protected path refused the request.
    #[error("{0}")]
    Denied(String),
    /// The request was malformed.
    #[error("{0}")]
    Invalid(String),
    /// Cancelled by the caller.
    #[error("Operation aborted")]
    Aborted,
    /// An invariant this crate owns did not hold.
    #[error("{0}")]
    Internal(&'static str),
}

impl CodeGraphError {
    /// A stable category name, for a host that branches on the class rather than the text.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Denied(_) => "denied",
            Self::Invalid(_) => "invalid",
            Self::Aborted => "aborted",
            Self::Internal(_) => "internal",
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

impl From<FilesystemError> for CodeGraphError {
    fn from(error: FilesystemError) -> Self {
        match error {
            FilesystemError::Aborted => Self::Aborted,
            FilesystemError::RootEscape(message) | FilesystemError::ProtectedPath(message) => {
                Self::Denied(message)
            }
            FilesystemError::Operation(message) | FilesystemError::NotFound(message) => {
                Self::Invalid(message)
            }
            // The context string is the action and path the filesystem crate already chose to
            // surface; the io::Error itself is dropped rather than forwarded.
            FilesystemError::Io { context, .. } => Self::Invalid(context),
        }
    }
}
