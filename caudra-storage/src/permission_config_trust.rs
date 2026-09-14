use std::io;
use std::path::Path;

use crate::state::{self, StateKey, project_scope};
use crate::{StateClass, StateDir, StorageError};

const TRUST: StateKey = StateKey {
    name: "permission.config_trust",
    class: StateClass::Persistent,
};
const SHA256_HEX_LEN: usize = 64;

pub fn is_project_trusted(
    state_dir: &StateDir,
    project: &Path,
    config_digest: &str,
) -> Result<bool, StorageError> {
    validate_digest(config_digest)?;
    let scope = scope(project)?;
    let Some(stored) = state::get::<String>(state_dir, &scope, TRUST)? else {
        return Ok(false);
    };
    validate_digest(&stored)?;
    Ok(stored == config_digest)
}

pub fn trust_project(
    state_dir: &StateDir,
    project: &Path,
    config_digest: &str,
) -> Result<(), StorageError> {
    validate_digest(config_digest)?;
    let scope = scope(project)?;
    state::try_update(state_dir, &scope, TRUST, |stored: &mut String| {
        if !stored.is_empty() {
            validate_digest(stored)?;
        }
        config_digest.clone_into(stored);
        Ok(())
    })?
}

pub fn revoke_project_trust(state_dir: &StateDir, project: &Path) -> Result<(), StorageError> {
    state::delete(state_dir, &scope(project)?, TRUST)?;
    Ok(())
}

pub fn is_remote_asset_trusted(
    state_dir: &StateDir,
    asset: &caudra_workspace::ProjectAssetTrustKey,
    digest: &str,
) -> Result<bool, StorageError> {
    validate_digest(digest)?;
    let scope = state::remote_asset_scope(asset);
    Ok(state::get::<String>(state_dir, &scope, TRUST)?.is_some_and(|stored| stored == digest))
}

pub fn trust_remote_asset(
    state_dir: &StateDir,
    asset: &caudra_workspace::ProjectAssetTrustKey,
    digest: &str,
) -> Result<(), StorageError> {
    validate_digest(digest)?;
    state::set(
        state_dir,
        &state::remote_asset_scope(asset),
        TRUST,
        &digest.to_owned(),
    )
}

pub fn revoke_remote_asset_trust(
    state_dir: &StateDir,
    asset: &caudra_workspace::ProjectAssetTrustKey,
) -> Result<(), StorageError> {
    state::delete(state_dir, &state::remote_asset_scope(asset), TRUST)?;
    Ok(())
}

fn scope(project: &Path) -> Result<String, StorageError> {
    Ok(project_scope(&project.canonicalize()?))
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
    use std::fs;

    use super::*;

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER_DIGEST: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

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
                matches!(error, StorageError::Io(error) if error.kind() == io::ErrorKind::InvalidData)
            );
        }
    }

    #[test]
    fn invalid_stored_digest_is_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        let scope = scope(&project).unwrap();
        state::set(&state_dir, &scope, TRUST, &"invalid").unwrap();

        assert!(trust_project(&state_dir, &project, DIGEST).is_err());
        assert_eq!(
            state::get::<String>(&state_dir, &scope, TRUST).unwrap(),
            Some("invalid".into())
        );
    }

    #[test]
    fn ephemeral_access_uses_the_persistent_root() {
        let temp = tempfile::tempdir().unwrap();
        let persistent = StateDir::from_path(temp.path().join("persistent"));
        let state_dir = StateDir::split(temp.path().join("volatile"), persistent.path().into());
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();

        trust_project(&state_dir, &project, DIGEST).unwrap();

        assert!(is_project_trusted(&persistent, &project, DIGEST).unwrap());
        assert!(!state_dir.path().join("sessions.sqlite3").exists());
    }
}
