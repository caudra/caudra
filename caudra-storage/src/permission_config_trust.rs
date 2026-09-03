use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{StateDir, StorageError, atomic_write_permissions, exclusive_state_lock};

const TRUST_FILE: &str = "permission-config-trust.json";
const TRUST_FILE_MODE: u32 = 0o600;
const TRUST_LOCK_FILE: &str = "permission-config-trust.lock";
const TRUST_VERSION: u32 = 1;
const SHA256_HEX_LEN: usize = 64;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PermissionConfigTrust {
    version: u32,
    projects: HashMap<PathBuf, String>,
}

impl Default for PermissionConfigTrust {
    fn default() -> Self {
        Self {
            version: TRUST_VERSION,
            projects: HashMap::new(),
        }
    }
}

pub fn is_project_trusted(
    state_dir: &StateDir,
    project: &Path,
    config_digest: &str,
) -> Result<bool, StorageError> {
    validate_digest(config_digest)?;
    let trust = load(state_dir)?;
    let project = project.canonicalize()?;
    Ok(trust
        .projects
        .get(&project)
        .is_some_and(|digest| digest == config_digest))
}

pub fn trust_project(
    state_dir: &StateDir,
    project: &Path,
    config_digest: &str,
) -> Result<(), StorageError> {
    validate_digest(config_digest)?;
    let _lock = exclusive_state_lock(&state_dir.path().join(TRUST_LOCK_FILE), TRUST_FILE_MODE)?;
    let mut trust = load(state_dir)?;
    let project = project.canonicalize()?;
    trust.projects.insert(project, config_digest.to_string());
    save(state_dir, &trust)
}

pub fn revoke_project_trust(state_dir: &StateDir, project: &Path) -> Result<(), StorageError> {
    let _lock = exclusive_state_lock(&state_dir.path().join(TRUST_LOCK_FILE), TRUST_FILE_MODE)?;
    let mut trust = load(state_dir)?;
    let project = project.canonicalize()?;
    trust.projects.remove(&project);
    save(state_dir, &trust)
}

fn path(state_dir: &StateDir) -> PathBuf {
    state_dir.path().join(TRUST_FILE)
}

fn load(state_dir: &StateDir) -> Result<PermissionConfigTrust, StorageError> {
    let trust_path = path(state_dir);
    match fs::symlink_metadata(&trust_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(invalid_data(format!(
                "permission config trust path {} is not a regular file",
                trust_path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PermissionConfigTrust::default());
        }
        Err(error) => return Err(error.into()),
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32);
    let mut file = match options.open(&trust_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PermissionConfigTrust::default());
        }
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(invalid_data(format!(
            "permission config trust path {} is not a regular file",
            trust_path.display()
        )));
    }
    #[cfg(unix)]
    if metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(invalid_data(format!(
            "permission config trust file {} must be owned by the current user and inaccessible to other users",
            trust_path.display()
        )));
    }
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    let trust = serde_json::from_slice(&data)?;
    validate(&trust)?;
    Ok(trust)
}

fn save(state_dir: &StateDir, trust: &PermissionConfigTrust) -> Result<(), StorageError> {
    validate(trust)?;
    fs::create_dir_all(state_dir.path())?;
    let data = serde_json::to_vec_pretty(trust)?;
    atomic_write_permissions(&path(state_dir), &data, TRUST_FILE_MODE)
}

fn validate(trust: &PermissionConfigTrust) -> Result<(), StorageError> {
    if trust.version != TRUST_VERSION {
        return Err(invalid_data(format!(
            "unsupported permission config trust version {} (expected {TRUST_VERSION})",
            trust.version
        )));
    }
    for (project, digest) in &trust.projects {
        if !project.is_absolute() {
            return Err(invalid_data(format!(
                "permission config trust project path {} is not absolute",
                project.display()
            )));
        }
        validate_digest(digest)?;
    }
    Ok(())
}

fn validate_digest(digest: &str) -> Result<(), StorageError> {
    if digest.len() == SHA256_HEX_LEN
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(invalid_data(
            "permission config digest must be a lowercase 64-character SHA-256 hex string",
        ))
    }
}

fn invalid_data(message: impl Into<String>) -> StorageError {
    StorageError::Io(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::{
        PermissionConfigTrust, TRUST_FILE_MODE, TRUST_LOCK_FILE, TRUST_VERSION, is_project_trusted,
        path, revoke_project_trust, trust_project,
    };
    use crate::{StateDir, StorageError};

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER_DIGEST: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    fn write_trust_file(state_dir: &StateDir, data: &[u8]) {
        fs::write(path(state_dir), data).unwrap();
        #[cfg(unix)]
        fs::set_permissions(path(state_dir), fs::Permissions::from_mode(TRUST_FILE_MODE)).unwrap();
    }

    #[test]
    fn trust_is_exact_to_canonical_project_and_digest() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let project = temp.path().join("project");
        let other_project = temp.path().join("other");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&other_project).unwrap();

        trust_project(&state_dir, &project.join("."), DIGEST).unwrap();

        assert!(is_project_trusted(&state_dir, &project, DIGEST).unwrap());
        assert!(!is_project_trusted(&state_dir, &project, OTHER_DIGEST).unwrap());
        assert!(!is_project_trusted(&state_dir, &other_project, DIGEST).unwrap());
        let stored = super::load(&state_dir).unwrap();
        assert!(
            stored
                .projects
                .contains_key(&project.canonicalize().unwrap())
        );

        trust_project(&state_dir, &project, OTHER_DIGEST).unwrap();
        assert!(!is_project_trusted(&state_dir, &project, DIGEST).unwrap());
        assert!(is_project_trusted(&state_dir, &project, OTHER_DIGEST).unwrap());

        revoke_project_trust(&state_dir, &project).unwrap();
        assert!(!is_project_trusted(&state_dir, &project, OTHER_DIGEST).unwrap());
    }

    #[test]
    fn invalid_digests_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();

        for digest in [
            "",
            "0123456789abcdef",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeg",
            "0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef",
        ] {
            let error = trust_project(&state_dir, &project, digest).unwrap_err();
            assert!(
                matches!(error, StorageError::Io(error) if error.kind() == std::io::ErrorKind::InvalidData)
            );
            assert!(!path(&state_dir).exists());
        }
    }

    #[test]
    fn corrupt_trust_is_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let project = temp.path().join("project");
        fs::create_dir_all(state_dir.path()).unwrap();
        fs::create_dir(&project).unwrap();
        let corrupt = b"{not valid JSON";
        write_trust_file(&state_dir, corrupt);

        assert!(trust_project(&state_dir, &project, DIGEST).is_err());
        assert!(is_project_trusted(&state_dir, &project, DIGEST).is_err());
        assert_eq!(fs::read(path(&state_dir)).unwrap(), corrupt);
    }

    #[test]
    fn invalid_stored_digest_is_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let project = temp.path().join("project");
        fs::create_dir_all(state_dir.path()).unwrap();
        fs::create_dir(&project).unwrap();
        let trust = PermissionConfigTrust {
            version: TRUST_VERSION,
            projects: HashMap::from([(project.canonicalize().unwrap(), "invalid".into())]),
        };
        let corrupt = serde_json::to_vec_pretty(&trust).unwrap();
        write_trust_file(&state_dir, &corrupt);

        assert!(trust_project(&state_dir, &project, DIGEST).is_err());
        assert_eq!(fs::read(path(&state_dir)).unwrap(), corrupt);
    }

    #[test]
    fn unsupported_version_is_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let project = temp.path().join("project");
        fs::create_dir_all(state_dir.path()).unwrap();
        fs::create_dir(&project).unwrap();
        let trust = PermissionConfigTrust {
            version: TRUST_VERSION + 1,
            projects: HashMap::from([(project.canonicalize().unwrap(), DIGEST.into())]),
        };
        let corrupt = serde_json::to_vec_pretty(&trust).unwrap();
        write_trust_file(&state_dir, &corrupt);

        assert!(trust_project(&state_dir, &project, DIGEST).is_err());
        assert_eq!(fs::read(path(&state_dir)).unwrap(), corrupt);
    }

    #[cfg(unix)]
    #[test]
    fn trust_and_lock_files_are_owner_only() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();

        trust_project(&state_dir, &project, DIGEST).unwrap();

        let trust_mode = fs::metadata(path(&state_dir)).unwrap().permissions().mode() & 0o777;
        let lock_mode = fs::metadata(state_dir.path().join(TRUST_LOCK_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(trust_mode, TRUST_FILE_MODE);
        assert_eq!(lock_mode, TRUST_FILE_MODE);
    }

    #[cfg(unix)]
    #[test]
    fn permissive_or_symlinked_trust_files_fail_closed() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let project = temp.path().join("project");
        fs::create_dir_all(state_dir.path()).unwrap();
        fs::create_dir(&project).unwrap();
        let trust = PermissionConfigTrust {
            version: TRUST_VERSION,
            projects: HashMap::from([(project.canonicalize().unwrap(), DIGEST.into())]),
        };
        fs::write(path(&state_dir), serde_json::to_vec(&trust).unwrap()).unwrap();
        fs::set_permissions(path(&state_dir), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(is_project_trusted(&state_dir, &project, DIGEST).is_err());

        fs::remove_file(path(&state_dir)).unwrap();
        let target = temp.path().join("trust-target.json");
        fs::write(&target, serde_json::to_vec(&trust).unwrap()).unwrap();
        symlink(target, path(&state_dir)).unwrap();
        assert!(is_project_trusted(&state_dir, &project, DIGEST).is_err());
    }
}
