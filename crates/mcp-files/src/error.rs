use std::{io, path::Path};

#[derive(Debug, thiserror::Error)]
pub enum FilesystemError {
    #[error("{0}")]
    RootEscape(String),
    #[error("{0}")]
    ProtectedPath(String),
    #[error("{0}")]
    Operation(String),
    /// The path does not exist. Distinct from `Operation` because a caller that
    /// has to read prose to learn a file is simply absent cannot branch on it.
    #[error("{0}")]
    NotFound(String),
    /// A prepared change found its target changed since preparation and
    /// published nothing, so the caller can read the file again and retry.
    #[error("{0}")]
    Stale(String),
    #[error("Operation aborted")]
    Aborted,
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
}

impl FilesystemError {
    /// The symbolic token a remote caller branches on. The human message is
    /// diagnostic text a client may decline to surface, so the cause has to
    /// survive independently of it.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::RootEscape(_) => "path_outside_root",
            Self::ProtectedPath(_) => "protected_path",
            Self::Operation(_) => "invalid_operation",
            Self::NotFound(_) => "not_found",
            Self::Stale(_) => "stale_resource",
            Self::Aborted => "cancelled",
            Self::Io { source, .. } if source.kind() == io::ErrorKind::NotFound => "not_found",
            Self::Io { source, .. } if source.kind() == io::ErrorKind::PermissionDenied => {
                "filesystem_permission_denied"
            }
            Self::Io { .. } => "filesystem_io",
        }
    }

    pub(crate) fn message(message: impl Into<String>) -> Self {
        Self::Operation(message.into())
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    pub(crate) fn io_path(action: &str, path: &Path, source: io::Error) -> Self {
        Self::io(format!("{action} {}", path.to_string_lossy()), source)
    }

    pub(crate) fn is_not_found(&self) -> bool {
        match self {
            Self::NotFound(_) => true,
            Self::Io { source, .. } => source.kind() == io::ErrorKind::NotFound,
            _ => false,
        }
    }
}
