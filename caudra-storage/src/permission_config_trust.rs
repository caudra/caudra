use std::io;
use std::path::Path;

use crate::checkout::{self, TrustSource};
use crate::state::{self, StateKey, StateStore, project_scope};
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
    Ok(project_trust_source(state_dir, project, config_digest)?.is_some())
}

/// The project's own grant, or else one for the identical config at the same
/// place in another verified checkout of its repository.
pub fn project_trust_source(
    state_dir: &StateDir,
    project: &Path,
    config_digest: &str,
) -> Result<Option<TrustSource>, StorageError> {
    validate_digest(config_digest)?;
    let store = StateStore::open(state_dir, TRUST.class)?;
    let source = TrustSource::find::<StorageError>(project, |candidate| {
        let Some(stored) = store.get::<String>(&scope(candidate)?, TRUST)? else {
            return Ok(false);
        };
        validate_digest(&stored)?;
        Ok(stored == config_digest)
    })?;
    if let Some(TrustSource::Inherited(from)) = &source {
        tracing::debug!(
            project = %project.display(),
            from = %from.display(),
            "project permission config trusted through a sibling checkout"
        );
    }
    Ok(source)
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

/// Revokes the grant for `project` and for its counterpart in every sibling
/// checkout, any of which it could otherwise still inherit.
pub fn revoke_project_trust(state_dir: &StateDir, project: &Path) -> Result<(), StorageError> {
    let store = StateStore::open(state_dir, TRUST.class)?;
    for path in checkout::with_siblings(project) {
        store.delete(&scope(&path)?, TRUST)?;
    }
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

    use test_case::test_case;

    use super::*;
    use crate::checkout::fixture::{forged_worktree, linked_pair_with_state};

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER_DIGEST: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
    const NESTED: &str = "crates/app";

    #[test_case(DIGEST, true ; "identical_config")]
    #[test_case(OTHER_DIGEST, false ; "changed_config")]
    fn a_worktree_inherits_only_the_identical_config(digest: &str, inherited: bool) {
        let (_temp, state_dir, main, worktree) = linked_pair_with_state();
        trust_project(&state_dir, &main, DIGEST).unwrap();

        assert_eq!(
            project_trust_source(&state_dir, &worktree, digest).unwrap(),
            inherited.then_some(TrustSource::Inherited(main))
        );
    }

    #[test_case("", false ; "checkout_root")]
    #[test_case(NESTED, true ; "same_subdirectory")]
    fn a_subdirectory_inherits_only_from_the_same_subdirectory(granted: &str, inherited: bool) {
        let (_temp, state_dir, main, worktree) = linked_pair_with_state();
        fs::create_dir_all(main.join(NESTED)).unwrap();
        fs::create_dir_all(worktree.join(NESTED)).unwrap();
        trust_project(&state_dir, &main.join(granted), DIGEST).unwrap();

        assert_eq!(
            project_trust_source(&state_dir, &worktree.join(NESTED), DIGEST).unwrap(),
            inherited.then(|| TrustSource::Inherited(main.join(NESTED)))
        );
    }

    #[test]
    fn an_exact_grant_wins_over_an_inherited_one() {
        let (_temp, state_dir, main, worktree) = linked_pair_with_state();
        trust_project(&state_dir, &main, DIGEST).unwrap();
        trust_project(&state_dir, &worktree, DIGEST).unwrap();

        assert_eq!(
            project_trust_source(&state_dir, &worktree, DIGEST).unwrap(),
            Some(TrustSource::Exact)
        );
    }

    #[test]
    fn a_forged_worktree_inherits_nothing() {
        let (_temp, state_dir, main, _) = linked_pair_with_state();
        let forged = forged_worktree(main.parent().unwrap(), &main);
        trust_project(&state_dir, &main, DIGEST).unwrap();

        assert_eq!(
            project_trust_source(&state_dir, &forged, DIGEST).unwrap(),
            None
        );
    }

    #[test]
    fn a_removed_worktree_stops_granting() {
        let (_temp, state_dir, main, worktree) = linked_pair_with_state();
        trust_project(&state_dir, &worktree, DIGEST).unwrap();
        assert!(is_project_trusted(&state_dir, &main, DIGEST).unwrap());

        fs::remove_dir_all(&worktree).unwrap();

        assert!(!is_project_trusted(&state_dir, &main, DIGEST).unwrap());
    }

    #[test]
    fn revoking_in_a_worktree_clears_the_grant_it_inherited() {
        let (_temp, state_dir, main, worktree) = linked_pair_with_state();
        trust_project(&state_dir, &main, DIGEST).unwrap();

        revoke_project_trust(&state_dir, &worktree).unwrap();

        assert!(!is_project_trusted(&state_dir, &main, DIGEST).unwrap());
        assert!(!is_project_trusted(&state_dir, &worktree, DIGEST).unwrap());
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
        assert!(!state_dir.path().join("caudra.db").exists());
    }
}
