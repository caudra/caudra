//! Secrets the user hands a session's shell commands. A session's own live in
//! `sessions/<id>/secrets/`, one owner-only JSON file per variable name, and go
//! with the session. Remembered ones live in `secrets/` under the persistent
//! root, the raw value only, so `[agent.shell_env] set NAME = "{file:…}"` reads
//! them as they are.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::auth::{AUTH_FILE_MODE, now_millis, read_private_credential};
use crate::id::CaudraId;
use crate::paths::ensure_private_dir;
use crate::sessions::SESSIONS_DIR;
use crate::{StateDir, StorageError, durable_rename, sync_parent_dir};

pub const SESSION_SECRETS_DIR: &str = "secrets";
pub const REMEMBERED_SECRETS_DIR: &str = "secrets";
const PARTIAL_SUFFIX: &str = ".partial";
#[cfg(unix)]
const SECRETS_DIR_MODE: u32 = 0o700;
const MAX_SECRET_FILE_BYTES: usize = 64 * 1024;
const INVALID_NAME: &str = "a secret name holds only ASCII letters, digits and _";

/// One session secret as its file holds it. No `Debug`, so it cannot reach a log.
#[derive(Serialize, Deserialize)]
pub struct StoredSecret {
    pub value: String,
    /// Where its lifetime starts, in Unix milliseconds: the last save, or later once extended.
    pub saved_at: u64,
}

pub fn session_secrets_dir(state_dir: &StateDir, session_id: CaudraId) -> PathBuf {
    state_dir
        .path()
        .join(SESSIONS_DIR)
        .join(session_id.to_string())
        .join(SESSION_SECRETS_DIR)
}

pub fn remembered_secrets_dir(state_dir: &StateDir) -> PathBuf {
    state_dir.persistent_path().join(REMEMBERED_SECRETS_DIR)
}

/// The one file a remembered secret lives in, for both the writer and the `{file:…}` reference.
pub fn remembered_path(state_dir: &StateDir, name: &str) -> Result<PathBuf, StorageError> {
    Ok(remembered_secrets_dir(state_dir).join(checked(name)?))
}

/// The session's secrets saved within `ttl`. Expired files and the partial writes a crash left
/// behind are deleted. A file that cannot be read is skipped with a warning, never fatal.
pub fn load_session_secrets(
    state_dir: &StateDir,
    session_id: CaudraId,
    ttl: Duration,
) -> Result<BTreeMap<String, StoredSecret>, StorageError> {
    let directory = session_secrets_dir(state_dir, session_id);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    let oldest = now_millis().saturating_sub(ttl.as_millis().try_into().unwrap_or(u64::MAX));
    let mut secrets = BTreeMap::new();
    for entry in entries {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.ends_with(PARTIAL_SUFFIX) {
            remove_if_present(&path)?;
            continue;
        }
        let name = name.to_owned();
        let stored = match read_secret(&path) {
            Ok(Some(stored)) => stored,
            Ok(None) => continue,
            Err(error) => {
                warn!(session = %session_id, %name, %error, "skipped an unreadable session secret");
                continue;
            }
        };
        if stored.saved_at < oldest {
            remove_if_present(&path)?;
            continue;
        }
        secrets.insert(name, stored);
    }
    Ok(secrets)
}

pub fn save_session_secret(
    state_dir: &StateDir,
    session_id: CaudraId,
    name: &str,
    secret: &StoredSecret,
) -> Result<(), StorageError> {
    write_private(
        &session_secrets_dir(state_dir, session_id),
        checked(name)?,
        &serde_json::to_vec(secret)?,
    )
}

pub fn delete_session_secret(
    state_dir: &StateDir,
    session_id: CaudraId,
    name: &str,
) -> Result<bool, StorageError> {
    remove_if_present(&session_secrets_dir(state_dir, session_id).join(checked(name)?))
}

pub fn save_remembered_secret(
    state_dir: &StateDir,
    name: &str,
    value: &str,
) -> Result<(), StorageError> {
    write_private(
        &remembered_secrets_dir(state_dir),
        checked(name)?,
        value.as_bytes(),
    )
}

pub fn load_remembered_secret(
    state_dir: &StateDir,
    name: &str,
) -> Result<Option<String>, StorageError> {
    read_private_credential(
        &remembered_path(state_dir, name)?,
        MAX_SECRET_FILE_BYTES,
        StorageError::InvalidSecret,
    )
}

pub fn delete_remembered_secret(state_dir: &StateDir, name: &str) -> Result<bool, StorageError> {
    remove_if_present(&remembered_path(state_dir, name)?)
}

/// Whether `name` has a remembered file, a regular one and not a link.
pub fn is_remembered_secret(state_dir: &StateDir, name: &str) -> bool {
    remembered_path(state_dir, name)
        .ok()
        .and_then(|path| fs::symlink_metadata(path).ok())
        .is_some_and(|metadata| metadata.is_file())
}

fn checked(name: &str) -> Result<&str, StorageError> {
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if valid {
        Ok(name)
    } else {
        Err(StorageError::InvalidSecret(INVALID_NAME.to_owned()))
    }
}

fn read_secret(path: &Path) -> Result<Option<StoredSecret>, StorageError> {
    read_private_credential(path, MAX_SECRET_FILE_BYTES, StorageError::InvalidSecret)?
        .map(|data| serde_json::from_str(&data).map_err(StorageError::from))
        .transpose()
}

/// Creates `<name>.partial` beside the target without following a link, then renames it over.
fn write_private(directory: &Path, name: &str, contents: &[u8]) -> Result<(), StorageError> {
    private_directory(directory)?;
    let target = directory.join(name);
    let partial = directory.join(format!("{name}{PARTIAL_SUFFIX}"));
    remove_if_present(&partial)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options
        .mode(AUTH_FILE_MODE)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    let written = options.open(&partial).and_then(|mut file| {
        file.write_all(contents)?;
        file.sync_all()?;
        durable_rename(&partial, &target)
    });
    if written.is_err() {
        let _ = fs::remove_file(&partial);
    }
    written?;
    sync_parent_dir(&target);
    Ok(())
}

fn private_directory(directory: &Path) -> Result<(), StorageError> {
    if let Some(parent) = directory.parent() {
        fs::create_dir_all(parent)?;
    }
    ensure_private_dir(directory)?;
    #[cfg(unix)]
    fs::set_permissions(directory, fs::Permissions::from_mode(SECRETS_DIR_MODE))?;
    Ok(())
}

fn remove_if_present(path: &Path) -> Result<bool, StorageError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const NAME: &str = "CLOUDFLARE_API_TOKEN";
    const VALUE: &str = "secret-canary-4711";
    const REPLACED: &str = "secret-canary-0815";
    const TTL: Duration = Duration::from_secs(60);
    const EXPIRED_AGE_MS: u64 = 120_000;
    #[cfg(unix)]
    const FILE_MODE: u32 = 0o600;
    #[cfg(unix)]
    const MODE_MASK: u32 = 0o777;

    fn state() -> (TempDir, StateDir, CaudraId) {
        let temp = TempDir::new().unwrap();
        let state = StateDir::from_path(temp.path().to_path_buf());
        (temp, state, CaudraId::generate())
    }

    fn stored(value: &str, saved_at: u64) -> StoredSecret {
        StoredSecret {
            value: value.to_owned(),
            saved_at,
        }
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & MODE_MASK
    }

    #[test]
    fn a_saved_secret_loads_back_and_a_replacement_wins() {
        let (_temp, state, id) = state();
        save_session_secret(&state, id, NAME, &stored(VALUE, now_millis())).unwrap();
        save_session_secret(&state, id, NAME, &stored(REPLACED, now_millis())).unwrap();

        let loaded = load_session_secrets(&state, id, TTL).unwrap();

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[NAME].value, REPLACED);
    }

    #[cfg(unix)]
    #[test]
    fn files_are_owner_only_in_an_owner_only_directory() {
        let (_temp, state, id) = state();
        save_session_secret(&state, id, NAME, &stored(VALUE, now_millis())).unwrap();
        save_remembered_secret(&state, NAME, VALUE).unwrap();

        for directory in [
            session_secrets_dir(&state, id),
            remembered_secrets_dir(&state),
        ] {
            assert_eq!(mode(&directory), SECRETS_DIR_MODE);
            assert_eq!(mode(&directory.join(NAME)), FILE_MODE);
        }
    }

    #[test]
    fn loading_deletes_expired_files_and_partial_writes() {
        let (_temp, state, id) = state();
        let expired = now_millis() - EXPIRED_AGE_MS;
        save_session_secret(&state, id, NAME, &stored(VALUE, expired)).unwrap();
        let directory = session_secrets_dir(&state, id);
        let partial = directory.join(format!("{NAME}{PARTIAL_SUFFIX}"));
        fs::write(&partial, VALUE).unwrap();

        let loaded = load_session_secrets(&state, id, TTL).unwrap();

        assert!(loaded.is_empty());
        assert_eq!(fs::read_dir(directory).unwrap().count(), 0);
    }

    #[test]
    fn a_stale_partial_write_does_not_block_the_next_save() {
        let (_temp, state, _id) = state();
        save_remembered_secret(&state, NAME, VALUE).unwrap();
        let partial = remembered_secrets_dir(&state).join(format!("{NAME}{PARTIAL_SUFFIX}"));
        fs::write(&partial, VALUE).unwrap();

        save_remembered_secret(&state, NAME, REPLACED).unwrap();

        assert!(!partial.exists());
        assert_eq!(
            load_remembered_secret(&state, NAME).unwrap().as_deref(),
            Some(REPLACED)
        );
    }

    #[test]
    fn the_remembered_file_holds_the_raw_value() {
        let (_temp, state, _id) = state();
        save_remembered_secret(&state, NAME, VALUE).unwrap();

        let path = remembered_path(&state, NAME).unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), VALUE);
        assert!(is_remembered_secret(&state, NAME));
        assert!(delete_remembered_secret(&state, NAME).unwrap());
        assert!(!is_remembered_secret(&state, NAME));
    }

    #[test_case("../escape"; "traversal")]
    #[test_case("A.partial"; "dot")]
    #[test_case(""; "empty")]
    fn a_name_that_is_not_a_variable_never_becomes_a_path(name: &str) {
        let (_temp, state, id) = state();

        let refused = save_session_secret(&state, id, name, &stored(VALUE, now_millis()));

        assert!(
            matches!(&refused, Err(StorageError::InvalidSecret(message)) if message == INVALID_NAME)
        );
        assert!(remembered_path(&state, name).is_err());
    }
}
