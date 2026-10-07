use std::collections::HashSet;
#[cfg(unix)]
use std::ffi::CString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use caudra_config::config_file;
use caudra_storage::version::{self, Release, UpdateChannel, VersionError};
use caudra_storage::{StateDir, StorageError};
use isahc::Request;
use isahc::config::{Configurable, RedirectPolicy};
use isahc::http::Uri;
use sha2::{Digest, Sha256};
use tempfile::Builder;

const RELEASE_DOWNLOAD_URL: &str = "https://github.com/caudra/caudra/releases/download";
const CHECKSUM_FILE: &str = "sha256sums.txt";
const INSTALLER_BYTES: u64 = 1024 * 1024;
const CHECKSUM_BYTES: u64 = 64 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REDIRECT_LIMIT: u32 = 5;
const CHECKSUM_INVALID: &str = "invalid release checksum manifest";
const CHECKSUM_MISSING: &str = "release checksum manifest must contain exactly one installer entry";
const CHECKSUM_MISMATCH: &str = "release installer checksum mismatch";
const WINDOWS_ROLLBACK: &str = "in-process rollback is not supported on Windows; close all Caudra processes and run the versioned PowerShell installer for the previous release";
const BACKUP_FILENAME: &str = "caudra_backup";
const INSTALL_DIR_ENV: &str = "CAUDRA_INSTALL_DIR";
const BACKUP_BINARY: &str = "binary";
const BACKUP_LICENSES: &str = "licenses";
const LEGACY_MARKER: &str = "no-license-bundle";
const LEGACY_CONTENT: &str = "The previous installation did not contain a license bundle.\n";
const LICENSE_MANIFEST: &str = "manifest.json";
const LICENSE_NOTICE: &str = "NOTICE";
const LICENSE_ATTRIBUTION: &str = "ATTRIBUTION.txt";
const RESTORE_PREFIX: &str = ".caudra-rollback.";
const LEGACY_WARNING: &str =
    "Warning: this backup predates retained license bundles; no matching notices are available.";
const RESTORE_SCRIPT: &str = r#"
set -eu
source_binary=$1
source_licenses=$2
dest_binary=$3
dest_licenses=$4

check_path() {
    path=$1
    while [ "$path" != / ] && [ "$path" != . ]; do
        [ ! -L "$path" ] || { echo "refusing symlink: $path" >&2; exit 1; }
        path=$(dirname -- "$path")
    done
}
check_path "$dest_binary"
check_path "$dest_licenses"
[ -f "$dest_binary" ] || exit 1
[ ! -e "$dest_licenses" ] || [ -d "$dest_licenses" ] || exit 1
mkdir -p -- "$(dirname -- "$dest_licenses")"
binary_stage=$(mktemp -d "$(dirname -- "$dest_binary")/.caudra-rollback.XXXXXX")
license_stage=$(mktemp -d "$(dirname -- "$dest_licenses")/.caudra-rollback.XXXXXX")
licenses_saved=false
licenses_installed=false
committed=false
recover() {
    status=$?
    trap - EXIT HUP INT TERM
    if [ "$committed" = false ]; then
        if [ "$licenses_installed" = true ]; then
            mv -- "$dest_licenses" "$license_stage/failed" || exit 1
        fi
        if [ "$licenses_saved" = true ]; then
            mv -- "$license_stage/previous" "$dest_licenses" || exit 1
        fi
        echo "Rollback failed; recovery files, if any: $binary_stage and $license_stage" >&2
    fi
    exit "$status"
}
trap recover EXIT
trap 'exit 1' HUP INT TERM
cp -p -- "$dest_binary" "$binary_stage/next"
cat -- "$source_binary" > "$binary_stage/next"
if [ -n "$source_licenses" ]; then
    cp -R -- "$source_licenses" "$license_stage/next"
fi
if [ -e "$dest_licenses" ]; then
    mv -- "$dest_licenses" "$license_stage/previous"
    licenses_saved=true
fi
if [ -n "$source_licenses" ]; then
    mv -- "$license_stage/next" "$dest_licenses"
    licenses_installed=true
fi
mv -- "$binary_stage/next" "$dest_binary"
committed=true
rmdir -- "$binary_stage" || :
echo "Displaced licenses retained at $license_stage"
"#;

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("failed to fetch {url}: {source}")]
    Fetch {
        url: String,
        #[source]
        source: io::Error,
    },

    #[error("{0}")]
    Integrity(&'static str),

    #[error("this installation is managed by {0}; update it through that package manager")]
    Managed(&'static str),

    #[error("{0}")]
    Unsupported(&'static str),

    #[error("failed to determine current binary path: {0}")]
    CurrentExe(std::io::Error),

    #[error("failed to back up binary and licenses to {path}: {source}")]
    Backup {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write install script: {0}")]
    WriteScript(std::io::Error),

    #[error("failed to execute install script: {0}")]
    #[cfg(not(windows))]
    ExecScript(std::io::Error),

    #[error("install script failed with exit code {0:?}")]
    InstallFailed(Option<i32>),

    #[error("no backup found at {0}")]
    NoBackup(PathBuf),

    #[error("failed to restore backup from {path}: {source}")]
    Restore {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot access data directory: {0}")]
    Storage(#[from] StorageError),

    #[error("failed to check latest version: {0}")]
    VersionCheck(#[from] VersionError),
}

fn fetch_asset(url: &str, limit: u64) -> Result<Vec<u8>, UpdateError> {
    let fetch = || -> io::Result<Vec<u8>> {
        let client = isahc::HttpClient::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(FETCH_TIMEOUT)
            .redirect_policy(RedirectPolicy::None)
            .build()
            .map_err(io::Error::other)?;
        let started = Instant::now();
        let mut next = url.to_owned();
        for redirects in 0..=REDIRECT_LIMIT {
            if !asset_url_allowed(&next) {
                return Err(io::Error::other(
                    "release asset redirect is not an allowed HTTPS origin",
                ));
            }
            let timeout = FETCH_TIMEOUT
                .checked_sub(started.elapsed())
                .filter(|duration| !duration.is_zero())
                .ok_or_else(|| io::Error::other("release asset download timed out"))?;
            let request = Request::get(&next)
                .timeout(timeout)
                .body(())
                .map_err(io::Error::other)?;
            let response = match client.send(request) {
                Ok(response) => response,
                Err(error) if redirects == 0 => {
                    return version::curl_fetch(url).map_err(|_| io::Error::other(error));
                }
                Err(error) => return Err(io::Error::other(error)),
            };
            if response.status().is_redirection() {
                next = response
                    .headers()
                    .get("Location")
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| {
                        io::Error::other("release asset redirect has no valid location")
                    })?
                    .to_owned();
                continue;
            }
            if !response.status().is_success() {
                return Err(io::Error::other(format!("HTTP {}", response.status())));
            }
            let mut bytes = Vec::new();
            response
                .into_body()
                .take(limit + 1)
                .read_to_end(&mut bytes)?;
            return Ok(bytes);
        }
        Err(io::Error::other("release asset redirect limit exceeded"))
    };
    let bytes = fetch().map_err(|source| UpdateError::Fetch {
        url: url.to_owned(),
        source,
    })?;
    if bytes.len() as u64 > limit {
        return Err(UpdateError::Integrity(
            "release asset exceeds download limit",
        ));
    }
    Ok(bytes)
}

fn asset_url_allowed(url: &str) -> bool {
    url.parse::<Uri>().is_ok_and(|uri| {
        uri.scheme_str() == Some("https")
            && matches!(uri.port_u16(), None | Some(443))
            && matches!(
                uri.host(),
                Some(
                    "github.com"
                        | "release-assets.githubusercontent.com"
                        | "objects.githubusercontent.com"
                )
            )
    })
}

fn verify_checksum(manifest: &[u8], name: &str, bytes: &[u8]) -> Result<(), UpdateError> {
    let text =
        std::str::from_utf8(manifest).map_err(|_| UpdateError::Integrity(CHECKSUM_INVALID))?;
    let mut expected = None;
    let mut names = HashSet::new();
    for line in text.lines() {
        let (hash, filename) = line
            .split_once("  ")
            .ok_or(UpdateError::Integrity(CHECKSUM_INVALID))?;
        if hash.len() != 64
            || !hash.bytes().all(|b| b.is_ascii_hexdigit())
            || !filename
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            || !filename
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
        {
            return Err(UpdateError::Integrity(CHECKSUM_INVALID));
        }
        if filename == name && expected.replace(hash).is_some() {
            return Err(UpdateError::Integrity(CHECKSUM_MISSING));
        }
        if !names.insert(filename) {
            return Err(UpdateError::Integrity(CHECKSUM_INVALID));
        }
    }
    let expected = expected.ok_or(UpdateError::Integrity(CHECKSUM_MISSING))?;
    if !Sha256::digest(bytes)
        .iter()
        .enumerate()
        .all(|(index, byte)| {
            u8::from_str_radix(&expected[index * 2..index * 2 + 2], 16) == Ok(*byte)
        })
    {
        return Err(UpdateError::Integrity(CHECKSUM_MISMATCH));
    }
    Ok(())
}

fn fetch_script(release: &Release) -> Result<String, UpdateError> {
    let name = if cfg!(windows) {
        "install.ps1"
    } else {
        "install.sh"
    };
    let base = format!("{RELEASE_DOWNLOAD_URL}/{}", release.tag);
    let manifest = fetch_asset(&format!("{base}/{CHECKSUM_FILE}"), CHECKSUM_BYTES)?;
    let bytes = fetch_asset(&format!("{base}/{name}"), INSTALLER_BYTES)?;
    verify_checksum(&manifest, name, &bytes)?;
    String::from_utf8(bytes).map_err(|_| UpdateError::Integrity("release installer is not UTF-8"))
}

fn backup_binary(exe_path: &Path, storage: &StateDir) -> Result<PathBuf, UpdateError> {
    let backup_path = storage.path().join(BACKUP_FILENAME);
    save_snapshot(exe_path, &backup_path).map_err(|source| UpdateError::Backup {
        path: backup_path.clone(),
        source,
    })?;
    Ok(backup_path)
}

fn license_path(exe_path: &Path) -> io::Result<PathBuf> {
    let prefix = exe_path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::other("binary path has no installation prefix"))?;
    Ok(prefix.join("share/licenses/caudra"))
}

fn check_path(path: &Path) -> io::Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.is_symlink() => {
                return Err(io::Error::other(format!(
                    "refusing symlink at {}",
                    ancestor.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn check_tree(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            check_tree(&entry?.path())?;
        }
    } else if !metadata.is_file() {
        return Err(io::Error::other(format!(
            "expected a regular file or directory at {}",
            path.display()
        )));
    }
    Ok(())
}

fn check_bundle(path: &Path) -> io::Result<()> {
    check_path(path)?;
    check_tree(path)?;
    for required in [LICENSE_MANIFEST, LICENSE_NOTICE, LICENSE_ATTRIBUTION] {
        let required = path.join(required);
        let metadata = fs::symlink_metadata(&required)?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(io::Error::other(format!(
                "missing or empty license bundle file: {}",
                required.display()
            )));
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    fs::create_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let destination = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &destination)?;
        } else if entry.file_type()?.is_file() {
            fs::copy(entry.path(), destination)?;
        } else {
            return Err(io::Error::other("license tree contains a non-regular file"));
        }
    }
    Ok(())
}

fn snapshot_licenses(backup: &Path) -> io::Result<Option<PathBuf>> {
    check_path(backup)?;
    check_tree(backup)?;
    if !fs::metadata(backup.join(BACKUP_BINARY))?.is_file() {
        return Err(io::Error::other("backup binary is not a regular file"));
    }
    let licenses = backup.join(BACKUP_LICENSES);
    let legacy = backup.join(LEGACY_MARKER);
    for entry in fs::read_dir(backup)? {
        let name = entry?.file_name();
        if name != BACKUP_BINARY && name != BACKUP_LICENSES && name != LEGACY_MARKER {
            return Err(io::Error::other("backup contains unrecognized files"));
        }
    }
    if licenses.try_exists()? && !legacy.try_exists()? {
        check_bundle(&licenses)?;
        Ok(Some(licenses))
    } else if !licenses.try_exists()? && fs::read_to_string(legacy)? == LEGACY_CONTENT {
        Ok(None)
    } else {
        Err(io::Error::other("backup has inconsistent license metadata"))
    }
}

fn save_snapshot(exe_path: &Path, backup: &Path) -> io::Result<()> {
    check_path(exe_path)?;
    check_path(backup)?;
    if backup.try_exists()? && fs::metadata(backup)?.is_dir() {
        snapshot_licenses(backup)?;
    } else if backup.try_exists()? && !fs::metadata(backup)?.is_file() {
        return Err(io::Error::other("backup is not a regular file or snapshot"));
    }
    let licenses = license_path(exe_path)?;
    check_path(&licenses)?;
    let has_licenses = licenses.try_exists()?;
    if has_licenses {
        check_bundle(&licenses)?;
    }
    let parent = backup
        .parent()
        .ok_or_else(|| io::Error::other("backup path has no parent"))?;
    let staging = Builder::new()
        .prefix(".caudra-backup-")
        .tempdir_in(parent)?;
    let next = staging.path().join("next");
    fs::create_dir(&next)?;
    fs::copy(exe_path, next.join(BACKUP_BINARY))?;
    if has_licenses {
        copy_tree(&licenses, &next.join(BACKUP_LICENSES))?;
    } else {
        fs::write(next.join(LEGACY_MARKER), LEGACY_CONTENT)?;
        eprintln!("{LEGACY_WARNING}");
    }
    let previous = staging.path().join("previous");
    if backup.try_exists()? {
        fs::rename(backup, &previous)?;
    }
    if let Err(error) = fs::rename(&next, backup) {
        if previous.try_exists()?
            && let Err(recovery) = fs::rename(&previous, backup)
        {
            let retained = staging.keep();
            return Err(io::Error::other(format!(
                "{error}; backup recovery failed: {recovery}; previous backup retained at {}",
                retained.display()
            )));
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(any(not(windows), test))]
fn installer_command(script: &Path, install_dir: &Path, tag: &str) -> Command {
    let mut command = Command::new("sh");
    command
        .arg(script)
        .arg(tag)
        .env(INSTALL_DIR_ENV, install_dir);
    command
}

#[cfg(not(windows))]
fn execute_script(script: &str, install_dir: &Path, tag: &str) -> Result<(), UpdateError> {
    let mut tmp = tempfile::NamedTempFile::new().map_err(UpdateError::WriteScript)?;
    tmp.write_all(script.as_bytes())
        .map_err(UpdateError::WriteScript)?;
    tmp.flush().map_err(UpdateError::WriteScript)?;

    let status = installer_command(tmp.path(), install_dir, tag)
        .status()
        .map_err(UpdateError::ExecScript)?;

    if !status.success() {
        return Err(UpdateError::InstallFailed(status.code()));
    }
    Ok(())
}

fn current_exe_resolved() -> Result<PathBuf, UpdateError> {
    std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(UpdateError::CurrentExe)
}

#[cfg(unix)]
fn needs_sudo(path: &Path) -> bool {
    let Some(dir) = path.ancestors().skip(1).find(|dir| dir.exists()) else {
        return false;
    };
    let Ok(cpath) = CString::new(dir.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    unsafe { libc::access(cpath.as_ptr(), libc::W_OK) != 0 }
}

#[cfg(not(unix))]
fn needs_sudo(_path: &Path) -> bool {
    false
}

fn restore_backup(backup_path: &Path, exe_path: &Path) -> Result<(), UpdateError> {
    let err = |e| UpdateError::Restore {
        path: backup_path.to_path_buf(),
        source: e,
    };

    check_path(backup_path).map_err(err)?;
    check_path(exe_path).map_err(err)?;
    let licenses = license_path(exe_path).map_err(err)?;
    check_path(&licenses).map_err(err)?;
    if licenses.try_exists().map_err(err)? {
        check_bundle(&licenses).map_err(err)?;
    }
    let (binary, saved_licenses) = if fs::metadata(backup_path).map_err(err)?.is_dir() {
        (
            backup_path.join(BACKUP_BINARY),
            snapshot_licenses(backup_path).map_err(err)?,
        )
    } else {
        if licenses.try_exists().map_err(err)? {
            return Err(err(io::Error::other(
                "old binary-only backup has no license record; refusing to replace an installation with a license bundle",
            )));
        }
        check_tree(backup_path).map_err(err)?;
        (backup_path.to_path_buf(), None)
    };
    if saved_licenses.is_none() {
        eprintln!("{LEGACY_WARNING}");
    }
    let sudo = needs_sudo(exe_path) || needs_sudo(&licenses);
    if !sudo {
        return restore_files(&binary, saved_licenses.as_deref(), exe_path, &licenses).map_err(err);
    }
    println!("Restoring to {} (requires sudo)...", exe_path.display());
    let status = Command::new("sudo")
        .arg("sh")
        .args(["-c", RESTORE_SCRIPT, "caudra-restore"])
        .arg(binary)
        .arg(saved_licenses.as_deref().unwrap_or_else(|| Path::new("")))
        .arg(exe_path)
        .arg(licenses)
        .status()
        .map_err(err)?;
    if !status.success() {
        return Err(err(io::Error::other(format!(
            "restore failed with exit code {:?}",
            status.code()
        ))));
    }
    Ok(())
}

fn restore_files(
    binary: &Path,
    saved_licenses: Option<&Path>,
    exe_path: &Path,
    licenses: &Path,
) -> io::Result<()> {
    let binary_parent = exe_path
        .parent()
        .ok_or_else(|| io::Error::other("binary path has no parent"))?;
    let license_parent = licenses
        .parent()
        .ok_or_else(|| io::Error::other("license path has no parent"))?;
    fs::create_dir_all(license_parent)?;
    let binary_stage = Builder::new()
        .prefix(RESTORE_PREFIX)
        .tempdir_in(binary_parent)?;
    let license_stage = Builder::new()
        .prefix(RESTORE_PREFIX)
        .tempdir_in(license_parent)?;
    fs::copy(binary, binary_stage.path().join("next"))?;
    if let Some(saved) = saved_licenses {
        copy_tree(saved, &license_stage.path().join("next"))?;
    }
    let license_stage = license_stage.keep();
    println!("Displaced licenses retained at {}", license_stage.display());
    replace_installation(binary_stage.path(), &license_stage, exe_path, licenses)
}

fn replace_installation(
    binary_stage: &Path,
    license_stage: &Path,
    exe_path: &Path,
    licenses: &Path,
) -> io::Result<()> {
    let previous = license_stage.join("previous");
    if licenses.try_exists()? {
        fs::rename(licenses, &previous)?;
    }
    let next = license_stage.join("next");
    let mut installed = false;
    let result = (|| {
        if next.try_exists()? {
            fs::rename(&next, licenses)?;
            installed = true;
        }
        fs::rename(binary_stage.join("next"), exe_path)
    })();
    if let Err(error) = result {
        let recovery = (|| {
            if installed {
                fs::rename(licenses, license_stage.join("failed"))?;
            }
            if previous.try_exists()? {
                fs::rename(previous, licenses)?;
            }
            Ok::<(), io::Error>(())
        })();
        return match recovery {
            Ok(()) => Err(error),
            Err(recovery) => Err(io::Error::other(format!(
                "{error}; license recovery failed: {recovery}; retained files at {}",
                license_stage.display()
            ))),
        };
    }
    Ok(())
}

fn prompt_yes(install_dir: &Path) -> bool {
    let action = if cfg!(windows) {
        "Prepare an external update for"
    } else {
        "Install to"
    };
    eprint!(
        "{action} {} using this script? [y/N] ",
        install_dir.display()
    );
    let _ = std::io::stderr().flush();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).is_ok() && input.trim().eq_ignore_ascii_case("y")
}

fn configured_channel() -> UpdateChannel {
    let load = || {
        let dir = config_file::resolve_config_dir()?;
        config_file::load_global_config(&config_file::global_config_path(&dir))
            .map(|config| config.settings.ui.update_channel.unwrap_or_default())
    };
    load().unwrap_or_else(|error| {
        eprintln!(
            "warning: cannot read update channel: {error}; using auto (override with --channel)"
        );
        UpdateChannel::Auto
    })
}

fn package_manager(exe: &Path) -> Option<&'static str> {
    if exe.starts_with("/nix/store") {
        return Some("Nix");
    }
    if exe.components().any(|part| part.as_os_str() == "Cellar") {
        return Some("Homebrew");
    }
    let cargo_record = exe.parent()?.parent()?.join(".crates2.json");
    let record: serde_json::Value = serde_json::from_slice(&fs::read(cargo_record).ok()?).ok()?;
    record
        .get("installs")?
        .as_object()?
        .values()
        .any(|entry| {
            entry
                .get("bins")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|bins| {
                    bins.iter()
                        .any(|bin| matches!(bin.as_str(), Some("caudra" | "caudra.exe")))
                })
        })
        .then_some("Cargo")
}

#[cfg(windows)]
fn prepare_windows_update(script: &str, install_dir: &Path, tag: &str) -> Result<(), UpdateError> {
    let mut file = Builder::new()
        .prefix("caudra-update-")
        .suffix(".ps1")
        .tempfile()
        .map_err(UpdateError::WriteScript)?;
    file.write_all(script.as_bytes())
        .map_err(UpdateError::WriteScript)?;
    let (_, path) = file
        .keep()
        .map_err(|error| UpdateError::WriteScript(error.error))?;
    let quote = |value: &str| format!("'{}'", value.replace('\'', "''"));
    println!("Not installed yet. Close all Caudra processes, then run in PowerShell:");
    println!(
        "$env:{INSTALL_DIR_ENV} = {}",
        quote(&install_dir.to_string_lossy())
    );
    println!("{}", windows_update_command(&path, tag));
    println!(
        "Remove the saved installer after it completes: {}",
        path.display()
    );
    Ok(())
}

#[cfg(any(windows, test))]
fn windows_update_command(path: &Path, tag: &str) -> String {
    let script = path.to_string_lossy().replace('\'', "''");
    format!("powershell.exe -NoProfile -ExecutionPolicy Bypass -File '{script}' '{tag}'")
}

pub fn update(
    channel: Option<UpdateChannel>,
    skip_confirm: bool,
    no_color: bool,
) -> Result<(), UpdateError> {
    let exe_path = current_exe_resolved()?;
    if let Some(manager) = package_manager(&exe_path) {
        return Err(UpdateError::Managed(manager));
    }
    let channel = channel.unwrap_or_else(configured_channel);
    let release = version::fetch_release(channel)?;
    if !version::is_newer(&release.version, version::CURRENT) {
        println!(
            "No newer eligible release (installed v{}, selected {}).",
            version::CURRENT,
            release.tag
        );
        return Ok(());
    }

    println!("Current version: v{}", version::CURRENT);
    println!("Selected release: {}", release.tag);
    println!();

    let install_dir = match std::env::var_os(INSTALL_DIR_ENV).filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => exe_path
            .parent()
            .ok_or_else(|| {
                UpdateError::CurrentExe(std::io::Error::other(
                    "binary path has no parent directory",
                ))
            })?
            .to_path_buf(),
    };
    let storage = StateDir::resolve()?;

    let script = fetch_script(&release)?;

    if no_color {
        println!("{script}");
    } else {
        let language = if cfg!(windows) { "powershell" } else { "bash" };
        println!("{}", caudra_ui::highlight_ansi(language, &script));
    }

    if !skip_confirm && !prompt_yes(&install_dir) {
        println!("Aborted.");
        return Ok(());
    }

    let backup_path = backup_binary(&exe_path, &storage)?;

    #[cfg(windows)]
    {
        prepare_windows_update(&script, &install_dir, &release.tag)?;
        println!("Previous version saved to: {}", backup_path.display());
    }

    #[cfg(not(windows))]
    {
        execute_script(&script, &install_dir, &release.tag)?;
        println!();
        println!("Updated successfully.");
        println!("Previous version saved to: {}", backup_path.display());
        println!("To restore: caudra rollback");
    }

    Ok(())
}

pub fn rollback() -> Result<(), UpdateError> {
    if cfg!(windows) {
        return Err(UpdateError::Unsupported(WINDOWS_ROLLBACK));
    }
    let exe_path = current_exe_resolved()?;
    if let Some(manager) = package_manager(&exe_path) {
        return Err(UpdateError::Managed(manager));
    }
    let storage = StateDir::resolve()?;
    let backup_path = storage.path().join(BACKUP_FILENAME);

    if !backup_path.exists() {
        return Err(UpdateError::NoBackup(backup_path));
    }

    restore_backup(&backup_path, &exe_path)?;

    println!("Restored previous version.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    #[cfg(unix)]
    use std::process::Command;

    use sha2::{Digest, Sha256};
    use tempfile::{TempDir, tempdir};
    use test_case::test_case;

    #[cfg(unix)]
    use super::RESTORE_SCRIPT;
    use super::{
        BACKUP_BINARY, BACKUP_FILENAME, BACKUP_LICENSES, CHECKSUM_INVALID, CHECKSUM_MISMATCH,
        CHECKSUM_MISSING, INSTALL_DIR_ENV, LEGACY_CONTENT, LEGACY_MARKER, LICENSE_ATTRIBUTION,
        LICENSE_MANIFEST, LICENSE_NOTICE, RESTORE_PREFIX, asset_url_allowed, installer_command,
        license_path, needs_sudo, package_manager, replace_installation, restore_backup,
        save_snapshot, snapshot_licenses, verify_checksum, windows_update_command,
    };

    const OLD_BINARY: &str = "previous executable";
    const NEW_BINARY: &str = "current executable";
    const OLD_NOTICE: &str = "previous notices";
    const NEW_NOTICE: &str = "current notices";
    const MANIFEST: &str = "{\"schema_version\": 1}";
    const EXTRA_FILE: &str = "user-file.txt";
    const EXTRA_CONTENT: &str = "retain this unrelated file";
    const INSTALLER_NAME: &str = "install.sh";
    const INSTALLER_CONTENT: &[u8] = b"set -eu\n";
    const RELEASE_TAG: &str = "v0.2.0-preview.1";
    #[cfg(unix)]
    const FAIL_BINARY_RENAME: &str = r#"
mv() {
    [ "$3" != "$FAIL_DESTINATION" ] || return 73
    command mv "$@"
}
"#;
    #[cfg(unix)]
    const RENAME_FAILURE_STATUS: i32 = 73;
    #[cfg(unix)]
    const INSTALLED_MODE: u32 = 0o750;
    #[cfg(unix)]
    const SNAPSHOT_MODE: u32 = 0o600;
    #[cfg(unix)]
    const PERMISSION_MASK: u32 = 0o7777;

    #[test_case("https://github.com/caudra/caudra/releases/download/v0.2.0/install.sh", true; "canonical")]
    #[test_case("https://release-assets.githubusercontent.com/asset", true; "github_cdn")]
    #[test_case("http://github.com/asset", false; "no_downgrade")]
    #[test_case("https://github.com.attacker.example/asset", false; "not_github")]
    #[test_case("https://github.com:444/asset", false; "nonstandard_port")]
    fn asset_redirects_stay_on_https_github(url: &str, allowed: bool) {
        assert_eq!(asset_url_allowed(url), allowed);
    }

    #[test_case("temp/install.ps1", "temp/install.ps1"; "ordinary_path")]
    #[test_case("user's directory/install.ps1", "user''s directory/install.ps1"; "quoted_path")]
    fn windows_handoff_uses_process_scoped_execution_policy(path: &str, escaped: &str) {
        assert_eq!(
            windows_update_command(Path::new(path), RELEASE_TAG),
            format!(
                "powershell.exe -NoProfile -ExecutionPolicy Bypass -File '{escaped}' '{RELEASE_TAG}'"
            )
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_rollback_refuses_before_touching_files() {
        assert_eq!(
            super::rollback().unwrap_err().to_string(),
            super::WINDOWS_ROLLBACK
        );
    }

    #[test_case(false; "one_matching_entry")]
    #[test_case(true; "uppercase_digest")]
    fn checksum_verifies_exact_installer(uppercase: bool) {
        let mut digest: String = Sha256::digest(INSTALLER_CONTENT)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if uppercase {
            digest.make_ascii_uppercase();
        }
        let manifest = format!("{digest}  {INSTALLER_NAME}\n");
        verify_checksum(manifest.as_bytes(), INSTALLER_NAME, INSTALLER_CONTENT).unwrap();
    }

    #[test_case("missing", CHECKSUM_MISSING; "missing_entry")]
    #[test_case("duplicate", CHECKSUM_MISSING; "duplicate_entry")]
    #[test_case("mismatch", CHECKSUM_MISMATCH; "corrupt_installer")]
    #[test_case("malformed", CHECKSUM_INVALID; "malformed_manifest")]
    fn installer_checksum_fails_closed(case: &str, expected: &str) {
        let digest: String = Sha256::digest(INSTALLER_CONTENT)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let line = format!("{digest}  {INSTALLER_NAME}\n");
        let manifest = match case {
            "missing" => format!("{digest}  another-file\n"),
            "duplicate" => line.repeat(2),
            "mismatch" => format!("{}  {INSTALLER_NAME}\n", "0".repeat(64)),
            _ => "invalid\n".to_owned(),
        };
        assert_eq!(
            verify_checksum(manifest.as_bytes(), INSTALLER_NAME, INSTALLER_CONTENT)
                .unwrap_err()
                .to_string(),
            expected
        );
    }

    #[test_case("prefix with spaces"; "spaces")]
    #[test_case("prefix's directory"; "quote")]
    fn installer_receives_exact_tag_and_destination(destination: &str) {
        let script = Path::new("downloaded installer.sh");
        let command = installer_command(script, Path::new(destination), RELEASE_TAG);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [script.as_os_str(), RELEASE_TAG.as_ref()]
        );
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == INSTALL_DIR_ENV && value == Some(destination.as_ref()))
        );
    }

    #[test_case("/nix/store/hash-caudra/bin/caudra", Some("Nix"); "nix")]
    #[test_case("/opt/homebrew/Cellar/caudra/0.2/bin/caudra", Some("Homebrew"); "homebrew")]
    #[test_case("/usr/local/bin/caudra", None; "unmanaged")]
    fn managed_installations_are_not_overwritten(exe: &str, expected: Option<&str>) {
        assert_eq!(package_manager(Path::new(exe)), expected);
    }

    #[test_case(true; "cargo_owned")]
    #[test_case(false; "another_cargo_binary")]
    fn cargo_installation_record_is_respected(owned: bool) {
        let root = tempdir().unwrap();
        let bin = if owned { "caudra" } else { "another-cli" };
        fs::write(
            root.path().join(".crates2.json"),
            format!(r#"{{"installs":{{"package":{{"bins":["{bin}"]}}}}}}"#),
        )
        .unwrap();
        assert_eq!(
            package_manager(&root.path().join("bin/caudra")),
            owned.then_some("Cargo")
        );
    }

    fn installation(bundle: bool) -> (TempDir, PathBuf, PathBuf, PathBuf) {
        let root = tempdir().unwrap();
        let prefix = root
            .path()
            .canonicalize()
            .unwrap()
            .join("prefix with spaces");
        let exe = prefix.join("bin/caudra");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, OLD_BINARY).unwrap();
        let licenses = license_path(&exe).unwrap();
        if bundle {
            write_bundle(&licenses, OLD_NOTICE);
        }
        let backup = root.path().canonicalize().unwrap().join(BACKUP_FILENAME);
        (root, exe, licenses, backup)
    }

    fn write_bundle(path: &Path, notice: &str) {
        fs::create_dir_all(path.join("third-party/nested")).unwrap();
        fs::write(path.join(LICENSE_MANIFEST), MANIFEST).unwrap();
        fs::write(path.join(LICENSE_NOTICE), notice).unwrap();
        fs::write(path.join(LICENSE_ATTRIBUTION), notice).unwrap();
        fs::write(path.join("third-party/nested/LICENSE"), notice).unwrap();
    }

    fn retained_licenses(licenses: &Path) -> PathBuf {
        fs::read_dir(licenses.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(RESTORE_PREFIX)
            })
            .unwrap()
            .join("previous")
    }

    #[test_case(true; "complete_bundle")]
    #[test_case(false; "explicit_legacy_install")]
    fn snapshot_and_restore_preserve_the_pair(bundle: bool) {
        let (_root, exe, licenses, backup) = installation(bundle);
        save_snapshot(&exe, &backup).unwrap();
        assert_eq!(
            fs::read_to_string(backup.join(BACKUP_BINARY)).unwrap(),
            OLD_BINARY
        );
        assert_eq!(snapshot_licenses(&backup).unwrap().is_some(), bundle);
        if !bundle {
            assert_eq!(
                fs::read_to_string(backup.join(LEGACY_MARKER)).unwrap(),
                LEGACY_CONTENT
            );
        }
        fs::write(&exe, NEW_BINARY).unwrap();
        write_bundle(&licenses, NEW_NOTICE);
        fs::write(licenses.join(EXTRA_FILE), EXTRA_CONTENT).unwrap();

        restore_backup(&backup, &exe).unwrap();

        assert_eq!(fs::read_to_string(&exe).unwrap(), OLD_BINARY);
        assert_eq!(licenses.exists(), bundle);
        if bundle {
            assert_eq!(
                fs::read_to_string(licenses.join(LICENSE_NOTICE)).unwrap(),
                OLD_NOTICE
            );
            assert_eq!(
                fs::read_to_string(licenses.join("third-party/nested/LICENSE")).unwrap(),
                OLD_NOTICE
            );
            assert!(!licenses.join(EXTRA_FILE).exists());
        }
        let retained = retained_licenses(&licenses);
        assert_eq!(
            fs::read_to_string(retained.join(LICENSE_NOTICE)).unwrap(),
            NEW_NOTICE
        );
        assert_eq!(
            fs::read_to_string(retained.join(EXTRA_FILE)).unwrap(),
            EXTRA_CONTENT
        );
    }

    #[test]
    fn legacy_rollback_does_not_require_a_license_parent_or_sudo() {
        let (_root, exe, licenses, backup) = installation(false);
        assert!(!needs_sudo(&licenses));
        fs::write(&backup, OLD_BINARY).unwrap();
        fs::write(&exe, NEW_BINARY).unwrap();
        restore_backup(&backup, &exe).unwrap();
        assert_eq!(fs::read_to_string(exe).unwrap(), OLD_BINARY);
        assert!(!licenses.exists());
    }

    #[test]
    fn unpaired_old_backup_cannot_replace_licensed_installation() {
        let (_root, exe, licenses, backup) = installation(true);
        fs::write(&backup, NEW_BINARY).unwrap();
        assert!(restore_backup(&backup, &exe).is_err());
        assert_eq!(fs::read_to_string(exe).unwrap(), OLD_BINARY);
        assert_eq!(
            fs::read_to_string(licenses.join(LICENSE_NOTICE)).unwrap(),
            OLD_NOTICE
        );
    }

    #[test_case(true; "paired_backup")]
    #[test_case(false; "old_binary_only_backup")]
    fn successive_backups_replace_the_complete_snapshot(paired: bool) {
        let (_root, exe, licenses, backup) = installation(true);
        if paired {
            save_snapshot(&exe, &backup).unwrap();
        } else {
            fs::write(&backup, OLD_BINARY).unwrap();
        }
        fs::write(&exe, NEW_BINARY).unwrap();
        write_bundle(&licenses, NEW_NOTICE);
        save_snapshot(&exe, &backup).unwrap();
        assert_eq!(
            fs::read_to_string(backup.join(BACKUP_BINARY)).unwrap(),
            NEW_BINARY
        );
        assert_eq!(
            fs::read_to_string(backup.join(BACKUP_LICENSES).join(LICENSE_NOTICE)).unwrap(),
            NEW_NOTICE
        );
    }

    #[test_case(LICENSE_MANIFEST; "missing_manifest")]
    #[test_case(LICENSE_NOTICE; "missing_notice")]
    #[test_case(LICENSE_ATTRIBUTION; "missing_attribution")]
    fn incomplete_bundle_does_not_replace_previous_backup(missing: &str) {
        let (_root, exe, licenses, backup) = installation(true);
        save_snapshot(&exe, &backup).unwrap();
        fs::write(&exe, NEW_BINARY).unwrap();
        fs::remove_file(licenses.join(missing)).unwrap();
        assert!(save_snapshot(&exe, &backup).is_err());
        assert_eq!(
            fs::read_to_string(backup.join(BACKUP_BINARY)).unwrap(),
            OLD_BINARY
        );
    }

    #[test]
    fn unrelated_backup_directory_is_not_overwritten() {
        let (_root, exe, _licenses, backup) = installation(false);
        fs::create_dir(&backup).unwrap();
        fs::write(backup.join(EXTRA_FILE), EXTRA_CONTENT).unwrap();
        assert!(save_snapshot(&exe, &backup).is_err());
        assert_eq!(
            fs::read_to_string(backup.join(EXTRA_FILE)).unwrap(),
            EXTRA_CONTENT
        );
    }

    #[test_case(true; "replacement_bundle")]
    #[test_case(false; "legacy_removal")]
    fn binary_install_failure_restores_current_bundle(bundle: bool) {
        let (root, exe, licenses, _backup) = installation(true);
        let binary_stage = root.path().join("binary-stage");
        let license_stage = root.path().join("license-stage");
        fs::create_dir(&binary_stage).unwrap();
        fs::create_dir(&license_stage).unwrap();
        if bundle {
            write_bundle(&license_stage.join("next"), NEW_NOTICE);
        }
        assert!(replace_installation(&binary_stage, &license_stage, &exe, &licenses).is_err());
        assert_eq!(fs::read_to_string(exe).unwrap(), OLD_BINARY);
        assert_eq!(
            fs::read_to_string(licenses.join(LICENSE_NOTICE)).unwrap(),
            OLD_NOTICE
        );
    }

    #[cfg(unix)]
    #[test_case("backup"; "backup_symlink")]
    #[test_case("licenses"; "license_directory_symlink")]
    #[test_case("nested"; "nested_license_symlink")]
    #[test_case("ancestor"; "license_parent_symlink")]
    fn symlinks_are_rejected_without_touching_the_target(location: &str) {
        let (root, exe, licenses, backup) = installation(true);
        let target = root.path().join(EXTRA_FILE);
        fs::write(&target, EXTRA_CONTENT).unwrap();
        let link = match location {
            "backup" => backup.clone(),
            "licenses" => {
                fs::remove_dir_all(&licenses).unwrap();
                licenses.clone()
            }
            "ancestor" => {
                let parent = licenses.parent().unwrap();
                fs::remove_dir_all(parent).unwrap();
                parent.to_path_buf()
            }
            _ => licenses.join(EXTRA_FILE),
        };
        symlink(&target, link).unwrap();
        assert!(save_snapshot(&exe, &backup).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), EXTRA_CONTENT);
    }

    #[test_case("missing"; "missing_snapshot_bundle")]
    #[test_case("extra"; "unrecognized_snapshot_file")]
    #[test_case("conflicting"; "conflicting_legacy_marker")]
    fn invalid_snapshot_never_changes_current_installation(damage: &str) {
        let (_root, exe, licenses, backup) = installation(true);
        save_snapshot(&exe, &backup).unwrap();
        match damage {
            "missing" => fs::remove_dir_all(backup.join(BACKUP_LICENSES)).unwrap(),
            "extra" => fs::write(backup.join(EXTRA_FILE), EXTRA_CONTENT).unwrap(),
            _ => fs::write(backup.join(LEGACY_MARKER), LEGACY_CONTENT).unwrap(),
        }
        fs::write(&exe, NEW_BINARY).unwrap();
        write_bundle(&licenses, NEW_NOTICE);
        assert!(restore_backup(&backup, &exe).is_err());
        assert_eq!(fs::read_to_string(exe).unwrap(), NEW_BINARY);
        assert_eq!(
            fs::read_to_string(licenses.join(LICENSE_NOTICE)).unwrap(),
            NEW_NOTICE
        );
    }

    #[cfg(unix)]
    #[test_case(true; "snapshot_symlink")]
    #[test_case(false; "destination_symlink")]
    fn rollback_rejects_license_symlinks(snapshot: bool) {
        let (root, exe, licenses, backup) = installation(true);
        save_snapshot(&exe, &backup).unwrap();
        let target = root.path().join(EXTRA_FILE);
        fs::write(&target, EXTRA_CONTENT).unwrap();
        let link = if snapshot {
            backup.join(BACKUP_LICENSES).join(EXTRA_FILE)
        } else {
            licenses.join(EXTRA_FILE)
        };
        symlink(&target, link).unwrap();
        assert!(restore_backup(&backup, &exe).is_err());
        assert_eq!(fs::read_to_string(exe).unwrap(), OLD_BINARY);
        assert_eq!(fs::read_to_string(target).unwrap(), EXTRA_CONTENT);
    }

    #[cfg(unix)]
    #[test_case(true; "bundle_rename_failure")]
    #[test_case(false; "legacy_rename_failure")]
    fn privileged_script_recovers_when_binary_rename_fails(bundle: bool) {
        let (_root, exe, licenses, backup) = installation(bundle);
        save_snapshot(&exe, &backup).unwrap();
        fs::write(&exe, NEW_BINARY).unwrap();
        write_bundle(&licenses, NEW_NOTICE);
        let saved = snapshot_licenses(&backup).unwrap();
        let script = format!("{FAIL_BINARY_RENAME}\n{RESTORE_SCRIPT}");
        let output = Command::new("sh")
            .args(["-c", &script, "caudra-restore-test"])
            .arg(backup.join(BACKUP_BINARY))
            .arg(saved.as_deref().unwrap_or_else(|| Path::new("")))
            .arg(&exe)
            .arg(&licenses)
            .env("FAIL_DESTINATION", &exe)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(RENAME_FAILURE_STATUS));
        assert_eq!(fs::read_to_string(exe).unwrap(), NEW_BINARY);
        assert_eq!(
            fs::read_to_string(licenses.join(LICENSE_NOTICE)).unwrap(),
            NEW_NOTICE
        );
    }

    #[cfg(unix)]
    #[test_case(true; "sudo_script_bundle")]
    #[test_case(false; "sudo_script_legacy")]
    fn privileged_script_restores_the_same_pair_without_sudo(bundle: bool) {
        let (_root, exe, licenses, backup) = installation(bundle);
        fs::set_permissions(&exe, fs::Permissions::from_mode(INSTALLED_MODE)).unwrap();
        save_snapshot(&exe, &backup).unwrap();
        fs::set_permissions(
            backup.join(BACKUP_BINARY),
            fs::Permissions::from_mode(SNAPSHOT_MODE),
        )
        .unwrap();
        fs::write(&exe, NEW_BINARY).unwrap();
        let installed_metadata = fs::metadata(&exe).unwrap();
        write_bundle(&licenses, NEW_NOTICE);
        let saved = snapshot_licenses(&backup).unwrap();
        let status = Command::new("sh")
            .args(["-c", RESTORE_SCRIPT, "caudra-restore-test"])
            .arg(backup.join(BACKUP_BINARY))
            .arg(saved.as_deref().unwrap_or_else(|| Path::new("")))
            .arg(&exe)
            .arg(&licenses)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(fs::read_to_string(&exe).unwrap(), OLD_BINARY);
        let restored_metadata = fs::metadata(&exe).unwrap();
        assert_eq!(restored_metadata.mode() & PERMISSION_MASK, INSTALLED_MODE);
        assert_eq!(restored_metadata.uid(), installed_metadata.uid());
        assert_eq!(restored_metadata.gid(), installed_metadata.gid());
        assert_eq!(licenses.exists(), bundle);
        if bundle {
            assert_eq!(
                fs::read_to_string(licenses.join(LICENSE_NOTICE)).unwrap(),
                OLD_NOTICE
            );
        }
        assert_eq!(
            fs::read_to_string(retained_licenses(&licenses).join(LICENSE_NOTICE)).unwrap(),
            NEW_NOTICE
        );
    }
}
