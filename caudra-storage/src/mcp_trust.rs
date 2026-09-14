use std::collections::HashMap;
use std::io;
use std::path::Path;

use crate::state::{self, StateKey, project_scope};
use crate::{StateClass, StateDir, StorageError};

const TRUST: StateKey = StateKey {
    name: "mcp.trust",
    class: StateClass::Persistent,
};
const SHA256_HEX_LEN: usize = 64;

pub fn is_project_trusted(
    state_dir: &StateDir,
    project: &Path,
    server: &str,
    config_digest: &str,
) -> Result<bool, StorageError> {
    validate_digest(config_digest)?;
    let trust = state::get::<HashMap<String, String>>(state_dir, &scope(project)?, TRUST)?
        .unwrap_or_default();
    validate(&trust)?;
    Ok(trust
        .get(server)
        .is_some_and(|digest| digest == config_digest))
}

pub fn trust_project(
    state_dir: &StateDir,
    project: &Path,
    server: &str,
    config_digest: &str,
) -> Result<(), StorageError> {
    validate_digest(config_digest)?;
    let scope = scope(project)?;
    state::try_update(
        state_dir,
        &scope,
        TRUST,
        |trust: &mut HashMap<String, String>| {
            validate(trust)?;
            trust.insert(server.to_string(), config_digest.to_string());
            Ok(())
        },
    )?
}

pub fn revoke_project_trust(
    state_dir: &StateDir,
    project: &Path,
    server: &str,
) -> Result<(), StorageError> {
    let scope = scope(project)?;
    state::try_update(
        state_dir,
        &scope,
        TRUST,
        |trust: &mut HashMap<String, String>| {
            validate(trust)?;
            trust.remove(server);
            Ok(())
        },
    )?
}

fn scope(project: &Path) -> Result<String, StorageError> {
    Ok(project_scope(&project.canonicalize()?))
}

fn validate(trust: &HashMap<String, String>) -> Result<(), StorageError> {
    trust
        .values()
        .try_for_each(|digest| validate_digest(digest))
}

fn validate_digest(digest: &str) -> Result<(), StorageError> {
    if digest.len() == SHA256_HEX_LEN
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(StorageError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "MCP config digest must be a lowercase 64-character SHA-256 hex string",
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER_DIGEST: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    #[test]
    fn trust_is_exact_to_project_server_and_digest() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let project = temp.path().join("project");
        let other_project = temp.path().join("other");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&other_project).unwrap();

        trust_project(&state_dir, &project, "server", DIGEST).unwrap();

        assert!(is_project_trusted(&state_dir, &project, "server", DIGEST).unwrap());
        assert!(!is_project_trusted(&state_dir, &project, "other", DIGEST).unwrap());
        assert!(!is_project_trusted(&state_dir, &other_project, "server", DIGEST).unwrap());
        assert!(!is_project_trusted(&state_dir, &project, "server", OTHER_DIGEST).unwrap());

        revoke_project_trust(&state_dir, &project, "server").unwrap();
        assert!(!is_project_trusted(&state_dir, &project, "server", DIGEST).unwrap());
    }

    #[test]
    fn independent_server_updates_do_not_clobber_each_other() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();

        trust_project(&state_dir, &project, "first", DIGEST).unwrap();
        trust_project(&state_dir, &project, "second", OTHER_DIGEST).unwrap();

        assert!(is_project_trusted(&state_dir, &project, "first", DIGEST).unwrap());
        assert!(is_project_trusted(&state_dir, &project, "second", OTHER_DIGEST).unwrap());
    }

    #[test]
    fn invalid_stored_digest_is_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        let scope = scope(&project).unwrap();
        state::set(
            &state_dir,
            &scope,
            TRUST,
            &HashMap::from([("bad".to_string(), "invalid".to_string())]),
        )
        .unwrap();

        assert!(trust_project(&state_dir, &project, "server", DIGEST).is_err());
        let stored = state::get::<HashMap<String, String>>(&state_dir, &scope, TRUST)
            .unwrap()
            .unwrap();
        assert_eq!(stored.get("bad").map(String::as_str), Some("invalid"));
        assert!(!stored.contains_key("server"));
    }

    #[test]
    fn ephemeral_access_uses_the_persistent_root() {
        let temp = tempfile::tempdir().unwrap();
        let persistent = StateDir::from_path(temp.path().join("persistent"));
        let state_dir = StateDir::split(temp.path().join("volatile"), persistent.path().into());
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();

        trust_project(&state_dir, &project, "server", DIGEST).unwrap();

        assert!(is_project_trusted(&persistent, &project, "server", DIGEST).unwrap());
        assert!(!state_dir.path().join("caudra.sqlite").exists());
    }
}
