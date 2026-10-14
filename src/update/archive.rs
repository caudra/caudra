//! The release archive `caudra update` fetches before the installer runs, so
//! the download shows its progress and a corrupt archive stops the update
//! before the current version is backed up.

use std::env::consts::{ARCH, OS};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use caudra_storage::version::Release;
use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use tempfile::{Builder, TempDir};

use super::{HASH_BUFFER_BYTES, UpdateError, asset_url, expected_digest, open_asset};
use crate::progress::{BAR_CHARS, BAR_REFRESH};

pub(super) const ARCHIVE_ENV: &str = "CAUDRA_INSTALL_ARCHIVE";
const ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const ARCHIVE_DIR_PREFIX: &str = "caudra-update-";
const ARCHIVE_MISMATCH: &str = "release archive checksum mismatch";
const ARCHIVE_TOO_LARGE: &str = "release archive exceeds the download limit";
const DOWNLOAD_TEMPLATE: &str = "  [{bar:30}] {bytes}/{total_bytes}  {bytes_per_sec}  ETA {eta}";
const DOWNLOAD_SPINNER_TEMPLATE: &str = "  {spinner} {bytes}  {bytes_per_sec}";

pub(super) struct Archive {
    path: PathBuf,
    _dir: TempDir,
}

impl Archive {
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

/// `Ok(None)` leaves the download to the installer: this platform publishes
/// no archive the installer would pick, or the transfer failed and the
/// installer gets its own attempt.
pub(super) fn download(
    release: &Release,
    checksums: &[u8],
) -> Result<Option<Archive>, UpdateError> {
    let Some(name) = archive_name(OS, ARCH, &release.tag) else {
        return Ok(None);
    };
    let expected = expected_digest(checksums, &name)?;
    match fetch(&asset_url(release, &name), &name, &expected) {
        Ok(archive) => Ok(Some(archive)),
        Err(error @ UpdateError::Download(_)) => {
            eprintln!("warning: {error}; the installer will download it");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Mirrors the target `install.sh` derives from `uname`.
fn archive_name(os: &str, arch: &str, tag: &str) -> Option<String> {
    let target = match os {
        "linux" => format!("{arch}-unknown-linux-musl"),
        "macos" => format!("{arch}-apple-darwin"),
        _ => return None,
    };
    Some(format!("caudra-{tag}-{target}.tar.gz"))
}

fn fetch(url: &str, name: &str, expected: &[u8; 32]) -> Result<Archive, UpdateError> {
    let response = open_asset(url, None).map_err(UpdateError::Download)?;
    let dir = Builder::new()
        .prefix(ARCHIVE_DIR_PREFIX)
        .tempdir()
        .map_err(UpdateError::Download)?;
    // The installer refuses symlinked path components, and macOS keeps its
    // temporary directory behind the /var symlink.
    let path = fs::canonicalize(dir.path())
        .map_err(UpdateError::Download)?
        .join(name);
    let file = File::create_new(&path).map_err(UpdateError::Download)?;
    let bar = download_bar(response.body().len());
    let copied = copy_verified(response.into_body(), file, expected, ARCHIVE_BYTES, &bar);
    bar.finish_and_clear();
    copied?;
    Ok(Archive { path, _dir: dir })
}

fn download_bar(len: Option<u64>) -> ProgressBar {
    let (bar, template) = match len {
        Some(len) => (ProgressBar::new(len), DOWNLOAD_TEMPLATE),
        None => (ProgressBar::new_spinner(), DOWNLOAD_SPINNER_TEMPLATE),
    };
    bar.set_style(
        ProgressStyle::with_template(template)
            .expect("static template")
            .progress_chars(BAR_CHARS),
    );
    bar.enable_steady_tick(BAR_REFRESH);
    bar
}

fn copy_verified(
    mut reader: impl Read,
    mut writer: impl Write,
    expected: &[u8; 32],
    limit: u64,
    bar: &ProgressBar,
) -> Result<(), UpdateError> {
    let mut hash = Sha256::new();
    let mut buffer = [0; HASH_BUFFER_BYTES];
    let mut total = 0;
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(UpdateError::Download(error)),
        };
        total += count as u64;
        if total > limit {
            return Err(UpdateError::Integrity(ARCHIVE_TOO_LARGE));
        }
        hash.update(&buffer[..count]);
        writer
            .write_all(&buffer[..count])
            .map_err(UpdateError::Download)?;
        bar.inc(count as u64);
    }
    writer.flush().map_err(UpdateError::Download)?;
    if hash.finalize().as_slice() != expected {
        return Err(UpdateError::Integrity(ARCHIVE_MISMATCH));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use indicatif::ProgressBar;
    use sha2::{Digest, Sha256};
    use test_case::test_case;

    use super::{ARCHIVE_MISMATCH, ARCHIVE_TOO_LARGE, archive_name, copy_verified};

    const TAG: &str = "v0.2.0-preview.9";
    const PAYLOAD: &[u8] = b"release archive bytes";

    #[test_case("linux", "x86_64", Some("caudra-v0.2.0-preview.9-x86_64-unknown-linux-musl.tar.gz"); "linux_x86_64")]
    #[test_case("linux", "aarch64", Some("caudra-v0.2.0-preview.9-aarch64-unknown-linux-musl.tar.gz"); "linux_aarch64")]
    #[test_case("macos", "aarch64", Some("caudra-v0.2.0-preview.9-aarch64-apple-darwin.tar.gz"); "macos_aarch64")]
    #[test_case("macos", "x86_64", Some("caudra-v0.2.0-preview.9-x86_64-apple-darwin.tar.gz"); "macos_x86_64")]
    #[test_case("freebsd", "x86_64", None; "unpublished_os")]
    fn archive_name_matches_the_installer(os: &str, arch: &str, expected: Option<&str>) {
        assert_eq!(archive_name(os, arch, TAG).as_deref(), expected);
    }

    #[test]
    fn copy_verified_writes_a_matching_archive() {
        let expected: [u8; 32] = Sha256::digest(PAYLOAD).into();
        let bar = ProgressBar::hidden();
        let mut written = Vec::new();
        copy_verified(
            Cursor::new(PAYLOAD),
            &mut written,
            &expected,
            PAYLOAD.len() as u64,
            &bar,
        )
        .unwrap();
        assert_eq!(written, PAYLOAD);
        assert_eq!(bar.position(), PAYLOAD.len() as u64);
    }

    #[test_case([0; 32], PAYLOAD.len() as u64, ARCHIVE_MISMATCH; "wrong_digest")]
    #[test_case(Sha256::digest(PAYLOAD).into(), PAYLOAD.len() as u64 - 1, ARCHIVE_TOO_LARGE; "over_limit")]
    fn copy_verified_fails_closed(expected: [u8; 32], limit: u64, error: &str) {
        let result = copy_verified(
            Cursor::new(PAYLOAD),
            Vec::new(),
            &expected,
            limit,
            &ProgressBar::hidden(),
        );
        assert_eq!(result.unwrap_err().to_string(), error);
    }
}
