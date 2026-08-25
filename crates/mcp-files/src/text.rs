use std::path::Path;

use file_format::FileFormat;
use sha2::{Digest, Sha256};
use tokio::{fs, io::AsyncReadExt};
use tokio_util::sync::CancellationToken;

use crate::FilesystemError;

#[derive(Clone)]
pub(crate) struct FileSnapshot {
    pub(crate) content: String,
    pub(crate) version: FileVersion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    first: u64,
    second: u64,
    size: u64,
    modified_nanos: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileVersion {
    identity: FileIdentity,
    digest: [u8; 32],
}

pub(crate) fn check_cancelled(token: &CancellationToken) -> Result<(), FilesystemError> {
    if token.is_cancelled() {
        Err(FilesystemError::Aborted)
    } else {
        Ok(())
    }
}

pub(crate) async fn read_bounded(
    path: &Path,
    maximum: usize,
    token: &CancellationToken,
) -> Result<Vec<u8>, FilesystemError> {
    check_cancelled(token)?;
    let file = fs::File::open(path)
        .await
        .map_err(|error| FilesystemError::io_path("Cannot read", path, error))?;
    let capacity = maximum
        .checked_add(1)
        .ok_or_else(|| FilesystemError::message("maximum file size is too large"))?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(capacity as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| FilesystemError::io_path("Cannot read", path, error))?;
    check_cancelled(token)?;
    if bytes.len() > maximum {
        return Err(FilesystemError::message(format!(
            "File exceeds maximum size of {maximum} bytes: {}",
            path.to_string_lossy()
        )));
    }
    Ok(bytes)
}

pub(crate) fn reject_binary(path: &Path, bytes: &[u8]) -> Result<(), FilesystemError> {
    if is_binary_content(bytes) {
        return Err(FilesystemError::message(format!(
            "Cannot operate on binary file: {}",
            path.to_string_lossy()
        )));
    }
    Ok(())
}

pub(crate) fn is_binary_content(bytes: &[u8]) -> bool {
    let format = FileFormat::from_bytes(bytes);
    let recognized_binary = !matches!(format, FileFormat::Empty | FileFormat::ArbitraryBinaryData)
        && !is_textual_format(format, bytes);

    recognized_binary || has_binary_sample(&bytes[..bytes.len().min(4_096)])
}

fn is_textual_format(format: FileFormat, bytes: &[u8]) -> bool {
    // `file-format` groups by purpose, so text-backed images, documents, and
    // models need an explicit editability policy rather than a `Kind` check.
    let media_type = format.media_type();
    if media_type.starts_with("text/")
        || media_type == "application/xml"
        || media_type.ends_with("+xml")
        || media_type.ends_with("+json")
    {
        return true;
    }

    match format {
        FileFormat::BmfontAscii
        | FileFormat::DrawingExchangeFormatAscii
        | FileFormat::Glyphs
        | FileFormat::InitialGraphicsExchangeSpecification
        | FileFormat::InterQuakeExport
        | FileFormat::Latex
        | FileFormat::MayaAscii
        | FileFormat::Model3dAscii
        | FileFormat::PemCertificate
        | FileFormat::PemCertificateSigningRequest
        | FileFormat::PemPrivateKey
        | FileFormat::PemPublicKey
        | FileFormat::PgpMessage
        | FileFormat::PgpPrivateKeyBlock
        | FileFormat::PgpPublicKeyBlock
        | FileFormat::PgpSignature
        | FileFormat::PgpSignedMessage
        | FileFormat::PolygonAscii
        | FileFormat::RichTextFormat
        | FileFormat::StandardForTheExchangeOfProductModelData
        | FileFormat::StereolithographyAscii
        | FileFormat::UniversalSceneDescriptionAscii
        | FileFormat::VirtualRealityModelingLanguage
        | FileFormat::WebassemblyText
        | FileFormat::XPixmap => true,
        FileFormat::EncapsulatedPostscript | FileFormat::Postscript => bytes.starts_with(b"%!"),
        _ => false,
    }
}

fn has_binary_sample(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let mut controls = 0usize;
    for byte in bytes {
        if *byte == 0 {
            return true;
        }
        if *byte < 9 || (*byte > 13 && *byte < 32) {
            controls += 1;
        }
    }
    controls as f64 / bytes.len() as f64 > 0.3
}

/// Node decodes malformed UTF-8 with replacement characters. Matching that is
/// important for previews: Rust's strict `String::from_utf8` would reject files
/// that the TypeScript implementation reads and subsequently rewrites.
pub(crate) fn decode_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

pub(crate) fn split_text_lines(text: &str) -> Vec<String> {
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
        .collect()
}

pub(crate) fn text_line_count(bytes: &[u8]) -> usize {
    split_text_lines(&decode_text(bytes)).len()
}

/// JavaScript's `String.length` and `slice` count UTF-16 code units, not Rust
/// scalar values. Encoding first preserves the boundary; if it cuts a surrogate
/// pair, U+FFFD is the closest value representable by Rust's UTF-8 `String`.
pub(crate) fn truncate_line(line: &str, maximum: usize) -> String {
    let utf16 = line.encode_utf16().collect::<Vec<_>>();
    if utf16.len() <= maximum {
        return line.to_owned();
    }
    format!(
        "{}... (line truncated)",
        String::from_utf16_lossy(&utf16[..maximum])
    )
}

pub(crate) fn enforce_bytes(
    label: &str,
    value: &str,
    maximum: usize,
) -> Result<(), FilesystemError> {
    if value.len() > maximum {
        return Err(FilesystemError::message(format!(
            "{label} exceeds maximum size of {maximum} bytes"
        )));
    }
    Ok(())
}

pub(crate) async fn read_text_if_exists(
    path: &Path,
    maximum: usize,
    token: &CancellationToken,
) -> Result<Option<String>, FilesystemError> {
    match read_bounded(path, maximum, token).await {
        Ok(bytes) => {
            reject_binary(path, &bytes)?;
            Ok(Some(decode_text(&bytes)))
        }
        Err(error) if error.is_not_found() => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) async fn read_text_snapshot_required(
    path: &Path,
    maximum: usize,
    token: &CancellationToken,
) -> Result<FileSnapshot, FilesystemError> {
    check_cancelled(token)?;
    let before = fs::metadata(path)
        .await
        .map_err(|error| FilesystemError::io_path("Cannot inspect", path, error))?;
    let bytes = read_bounded(path, maximum, token).await?;
    reject_binary(path, &bytes)?;
    let after = fs::metadata(path)
        .await
        .map_err(|error| FilesystemError::io_path("Cannot inspect", path, error))?;
    if file_identity(&before) != file_identity(&after) {
        return Err(FilesystemError::message(format!(
            "File changed while it was being read: {}",
            path.to_string_lossy()
        )));
    }
    Ok(FileSnapshot {
        content: decode_text(&bytes),
        version: FileVersion {
            identity: file_identity(&after),
            digest: Sha256::digest(&bytes).into(),
        },
    })
}

pub(crate) async fn validate_snapshot(
    path: &Path,
    expected: &FileVersion,
    maximum: usize,
    token: &CancellationToken,
) -> Result<(), FilesystemError> {
    let current = read_text_snapshot_required(path, maximum, token).await?;
    if current.version != *expected {
        return Err(FilesystemError::message(format!(
            "File changed before publication: {}",
            path.to_string_lossy()
        )));
    }
    Ok(())
}

pub(crate) async fn exists(path: &Path) -> bool {
    fs::metadata(path).await.is_ok()
}

pub(crate) fn js_length(value: &str) -> usize {
    value.encode_utf16().count()
}

fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    use std::time::UNIX_EPOCH;

    let (first, second) = platform_file_identity(metadata);
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |value| value.as_nanos());
    FileIdentity {
        first,
        second,
        size: metadata.len(),
        modified_nanos,
    }
}

#[cfg(unix)]
fn platform_file_identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;

    (metadata.dev(), metadata.ino())
}

#[cfg(windows)]
fn platform_file_identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    use std::os::windows::fs::MetadataExt;

    (
        metadata.volume_serial_number().map_or(0, u64::from),
        metadata.file_index().unwrap_or(0),
    )
}

#[cfg(not(any(unix, windows)))]
fn platform_file_identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    (metadata.len(), 0)
}

#[cfg(test)]
mod tests {
    use super::is_binary_content;

    #[test]
    fn detects_known_binary_signatures_and_unknown_binary_samples() {
        for bytes in [
            b"%PDF-1.7\n1 0 obj\n".as_slice(),
            b"PK\x03\x04archive".as_slice(),
            b"\x89PNG\r\n\x1a\n".as_slice(),
            b"\x7fELF\x02\x01\x01\0".as_slice(),
            b"unknown\0binary".as_slice(),
            b"\xC5\xD0\xD3\xC6binary eps".as_slice(),
        ] {
            assert!(
                is_binary_content(bytes),
                "expected binary content: {bytes:?}"
            );
        }
    }

    #[test]
    fn keeps_reviewed_textual_formats_editable() {
        for bytes in [
            b"plain text\n".as_slice(),
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>".as_slice(),
            b"<?xml version=\"1.0\"?><root/>".as_slice(),
            b"-----BEGIN CERTIFICATE-----\nYWJj\n-----END CERTIFICATE-----\n".as_slice(),
            b"{\\rtf1 plain rtf}".as_slice(),
            b"%!PS-Adobe-3.0\nshowpage\n".as_slice(),
            b"ply\nformat ascii 1.0\nend_header\n".as_slice(),
            b"hello \xFF world\n".as_slice(),
            b"".as_slice(),
        ] {
            assert!(
                !is_binary_content(bytes),
                "expected text content: {bytes:?}"
            );
        }
    }

    #[test]
    fn detects_signatures_beyond_the_legacy_sample_window() {
        let mut iso = vec![b' '; 32_774];
        iso[32_769..32_774].copy_from_slice(b"CD001");

        assert!(is_binary_content(&iso));
    }
}
