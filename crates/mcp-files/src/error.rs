use std::{io, path::Path};

#[derive(Debug, thiserror::Error)]
pub enum FilesystemError {
    #[error("{0}")]
    RootEscape(String),
    #[error("{0}")]
    ProtectedPath(String),
    #[error("{0}")]
    Operation(String),
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
    pub(crate) fn message(message: impl Into<String>) -> Self {
        Self::Operation(message.into())
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
        matches!(
            self,
            Self::Io { source, .. } if source.kind() == io::ErrorKind::NotFound
        )
    }
}
