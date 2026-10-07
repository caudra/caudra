//! Server-only reviewed file transfer over authenticated MCP methods and the `/files` byte route.
//! The embedding facade does not link this transport; filesystem primitives remain in `mcp-files`.

#[cfg(unix)]
pub(crate) mod endpoints;
#[cfg(unix)]
mod journal;
#[cfg(unix)]
pub(crate) mod reviewed;
#[cfg(unix)]
mod reviewed_http;

use std::path::Path;

use workcell_mcp_files::{FileToolGroup, FilesystemError};

pub const ENDPOINT_PATH: &str = "/files";
#[cfg(unix)]
const OCTET_STREAM: &str = "application/octet-stream";

#[derive(Clone, Debug)]
pub(crate) struct TransferGroup {
    files: FileToolGroup,
    max_transfer_bytes: usize,
    #[cfg(unix)]
    pub(crate) reviewed: Option<reviewed::ReviewedTransfers>,
}

impl TransferGroup {
    pub async fn new(
        root: impl AsRef<Path>,
        allow_write: bool,
        max_transfer_bytes: usize,
    ) -> Result<Self, FilesystemError> {
        Ok(Self {
            files: FileToolGroup::new(root, allow_write, None).await?,
            max_transfer_bytes,
            #[cfg(unix)]
            reviewed: None,
        })
    }

    pub fn share_files(&mut self, files: FileToolGroup) {
        self.files = files;
    }

    pub fn files(&self) -> &FileToolGroup {
        &self.files
    }

    pub fn enabled(&self) -> bool {
        #[cfg(unix)]
        {
            self.reviewed.is_some()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    pub fn max_transfer_bytes(&self) -> usize {
        self.max_transfer_bytes
    }
}
