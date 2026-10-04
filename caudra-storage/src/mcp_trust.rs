use std::collections::HashMap;
use std::io;
use std::path::Path;

use crate::checkout::{self, TrustSource};
use crate::state::{self, StateKey, StateStore, project_scope};
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
    Ok(project_trust_source(state_dir, project, server, config_digest)?.is_some())
}

/// The project's own grant for `server`, or else one for the identical server
/// config at the same place in another verified checkout of its repository.
pub fn project_trust_source(
    state_dir: &StateDir,
    project: &Path,
    server: &str,
    config_digest: &str,
) -> Result<Option<TrustSource>, StorageError> {
    validate_digest(config_digest)?;
    let store = StateStore::open(state_dir, TRUST.class)?;
    let source = TrustSource::find::<StorageError>(project, |candidate| {
        let trust = store
            .get::<HashMap<String, String>>(&scope(candidate)?, TRUST)?
            .unwrap_or_default();
        validate(&trust)?;
        Ok(trust
            .get(server)
            .is_some_and(|digest| digest == config_digest))
    })?;
    if let Some(TrustSource::Inherited(from)) = &source {
        tracing::debug!(
            project = %project.display(),
            from = %from.display(),
            server,
            "MCP server trusted through a sibling checkout"
        );
    }
    Ok(source)
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

/// Revokes the grant for `server` in `project` and in its counterpart in every
/// sibling checkout, any of which it could otherwise still inherit.
pub fn revoke_project_trust(
    state_dir: &StateDir,
    project: &Path,
    server: &str,
) -> Result<(), StorageError> {
    let mut store = StateStore::open(state_dir, TRUST.class)?;
    for path in checkout::with_siblings(project) {
        store.try_update(
            &scope(&path)?,
            TRUST,
            |trust: &mut HashMap<String, String>| {
                validate(trust)?;
                trust.remove(server);
                Ok::<_, StorageError>(())
            },
        )??;
    }
    Ok(())
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

    use test_case::test_case;

    use super::*;
    use crate::checkout::fixture::linked_pair_with_state;

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER_DIGEST: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
    const SERVER: &str = "github";
    const OTHER_SERVER: &str = "linear";

    #[test_case(SERVER, DIGEST, true ; "same_server_and_config")]
    #[test_case(OTHER_SERVER, DIGEST, false ; "other_server")]
    #[test_case(SERVER, OTHER_DIGEST, false ; "changed_config")]
    fn a_worktree_inherits_only_the_same_server_config(
        server: &str,
        digest: &str,
        inherited: bool,
    ) {
        let (_temp, state_dir, main, worktree) = linked_pair_with_state();
        trust_project(&state_dir, &main, SERVER, DIGEST).unwrap();

        assert_eq!(
            project_trust_source(&state_dir, &worktree, server, digest).unwrap(),
            inherited.then_some(TrustSource::Inherited(main))
        );
    }

    #[test]
    fn revoking_in_a_worktree_clears_only_that_server_in_every_checkout() {
        let (_temp, state_dir, main, worktree) = linked_pair_with_state();
        trust_project(&state_dir, &main, SERVER, DIGEST).unwrap();
        trust_project(&state_dir, &main, OTHER_SERVER, DIGEST).unwrap();

        revoke_project_trust(&state_dir, &worktree, SERVER).unwrap();

        assert!(!is_project_trusted(&state_dir, &main, SERVER, DIGEST).unwrap());
        assert!(is_project_trusted(&state_dir, &worktree, OTHER_SERVER, DIGEST).unwrap());
    }

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
        assert!(!state_dir.path().join("caudra.db").exists());
    }
}
