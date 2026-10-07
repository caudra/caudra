use std::path::Path;

use tokio::fs;
use tokio_util::sync::CancellationToken;

use crate::{
    FilesystemError,
    operations::FilesystemCore,
    text::{check_cancelled, is_binary_content, read_bounded, text_line_count},
};

#[derive(Default)]
pub(super) struct ListingMetadata {
    pub(super) size_bytes: Option<u64>,
    pub(super) line_count: Option<usize>,
}

pub(super) async fn file_listing_metadata(
    core: &FilesystemCore,
    path: &Path,
    token: &CancellationToken,
) -> Result<ListingMetadata, FilesystemError> {
    check_cancelled(token)?;
    let Ok(metadata) = fs::metadata(path).await else {
        return Ok(ListingMetadata::default());
    };
    if !metadata.is_file() {
        return Ok(ListingMetadata::default());
    }
    let size_bytes = metadata.len();
    if size_bytes > core.limits.max_file_bytes as u64 {
        return Ok(ListingMetadata {
            size_bytes: Some(size_bytes),
            line_count: None,
        });
    }
    let bytes = match read_bounded(path, core.limits.max_file_bytes, token).await {
        Ok(bytes) => bytes,
        Err(FilesystemError::Aborted) => return Err(FilesystemError::Aborted),
        Err(_) => {
            return Ok(ListingMetadata {
                size_bytes: Some(size_bytes),
                line_count: None,
            });
        }
    };
    let line_count = if is_binary_content(&bytes) {
        None
    } else {
        Some(text_line_count(&bytes))
    };
    Ok(ListingMetadata {
        size_bytes: Some(size_bytes),
        line_count,
    })
}
