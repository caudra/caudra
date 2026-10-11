use std::path::PathBuf;
use std::process::ExitStatus;

use thiserror::Error;

use crate::bounded_process::ProcessError;

#[derive(Debug, Error)]
pub enum HostError {
    #[error("{0}: {1}")]
    Run(&'static str, #[source] ProcessError),
    #[error("{host} refused the request ({code}): {message}")]
    Refused {
        host: &'static str,
        code: String,
        message: String,
    },
    #[error("this {0} predates the request, update {0}: {1}")]
    Unsupported(&'static str, String),
    #[error("{host} exited with {status}: {stderr}")]
    Failed {
        host: &'static str,
        status: ExitStatus,
        stderr: String,
    },
    #[error("{0} answered with unreadable output: {1}")]
    Output(&'static str, #[source] serde_json::Error),
    #[error("{host} does not manage the repository at {}", path.display())]
    UnknownRepository { host: &'static str, path: PathBuf },
}

impl HostError {
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Refused { code, .. } => Some(code),
            _ => None,
        }
    }

    pub(crate) fn missing_field(host: &'static str, field: &'static str) -> Self {
        Self::Output(host, serde::de::Error::missing_field(field))
    }
}
