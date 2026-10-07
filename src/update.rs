#[cfg(unix)]
use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use caudra_storage::version::{self, VersionError};
use caudra_storage::{StateDir, StorageError};
use tempfile::Builder;

const INSTALL_SCRIPT_URL: &str = "https://caudra.ai/install.sh";
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
        url: &'static str,
        #[source]
        source: isahc::Error,
    },

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

fn fetch_script() -> Result<String, UpdateError> {
    use isahc::ReadResponseExt;
    isahc::get(INSTALL_SCRIPT_URL)
        .and_then(|mut r| r.text().map_err(Into::into))
        .map_err(|source| UpdateError::Fetch {
            url: INSTALL_SCRIPT_URL,
            source,
        })
        .or_else(|e| {
            version::curl_fetch(INSTALL_SCRIPT_URL)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .map_err(|_| e)
        })
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

fn execute_script(script: &str, install_dir: &Path) -> Result<(), UpdateError> {
    let mut tmp = tempfile::NamedTempFile::new().map_err(UpdateError::WriteScript)?;
    tmp.write_all(script.as_bytes())
        .map_err(UpdateError::WriteScript)?;
    tmp.flush().map_err(UpdateError::WriteScript)?;

    let status = std::process::Command::new("sh")
        .arg(tmp.path())
        .env(INSTALL_DIR_ENV, install_dir)
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
    eprint!(
        "Install to {} and run this script? [y/N] ",
        install_dir.display()
    );
    let _ = std::io::stderr().flush();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).is_ok() && input.trim().eq_ignore_ascii_case("y")
}

pub fn update(skip_confirm: bool, no_color: bool) -> Result<(), UpdateError> {
    let latest = version::fetch_latest()?;
    if !version::is_newer(&latest, version::CURRENT) {
        println!("Already up to date (v{})", version::CURRENT);
        return Ok(());
    }

    println!("Current version: v{}", version::CURRENT);
    println!("Latest version:  v{latest}");
    println!();

    let exe_path = current_exe_resolved()?;
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

    let script = fetch_script()?;

    if no_color {
        println!("{script}");
    } else {
        println!("{}", caudra_ui::highlight_ansi("bash", &script));
    }

    if !skip_confirm && !prompt_yes(&install_dir) {
        println!("Aborted.");
        return Ok(());
    }

    let backup_path = backup_binary(&exe_path, &storage)?;

    execute_script(&script, &install_dir)?;

    println!();
    println!("Updated successfully.");
    println!("Previous version saved to: {}", backup_path.display());
    println!("To restore: caudra rollback");

    Ok(())
}

pub fn rollback() -> Result<(), UpdateError> {
    let exe_path = current_exe_resolved()?;
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

    use tempfile::{TempDir, tempdir};
    use test_case::test_case;

    #[cfg(unix)]
    use super::RESTORE_SCRIPT;
    use super::{
        BACKUP_BINARY, BACKUP_FILENAME, BACKUP_LICENSES, LEGACY_CONTENT, LEGACY_MARKER,
        LICENSE_ATTRIBUTION, LICENSE_MANIFEST, LICENSE_NOTICE, RESTORE_PREFIX, license_path,
        needs_sudo, replace_installation, restore_backup, save_snapshot, snapshot_licenses,
    };

    const OLD_BINARY: &str = "previous executable";
    const NEW_BINARY: &str = "current executable";
    const OLD_NOTICE: &str = "previous notices";
    const NEW_NOTICE: &str = "current notices";
    const MANIFEST: &str = "{\"schema_version\": 1}";
    const EXTRA_FILE: &str = "user-file.txt";
    const EXTRA_CONTENT: &str = "retain this unrelated file";
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
