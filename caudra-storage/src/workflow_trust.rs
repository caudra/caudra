//! Exact-digest trust for project workflow sources: the workflow domain of
//! [`crate::digest_trust`].

use std::path::Path;

use caudra_workspace::ProjectAssetTrustKey;

use crate::checkout::TrustSource;
use crate::digest_trust::TrustDomain;
use crate::{StateDir, StorageError};

const DOMAIN: TrustDomain = TrustDomain::Workflow;

pub fn workflow_source_digest(bytes: &[u8], language_version: u32, abi_version: u32) -> String {
    DOMAIN.source_digest(bytes, language_version, abi_version)
}

pub fn is_workflow_trusted(
    state_dir: &StateDir,
    project_root: &Path,
    source_rel_path: &str,
    digest: &str,
) -> Result<bool, StorageError> {
    DOMAIN.is_trusted(state_dir, project_root, source_rel_path, digest)
}

pub fn workflow_trust_source(
    state_dir: &StateDir,
    project_root: &Path,
    source_rel_path: &str,
    digest: &str,
) -> Result<Option<TrustSource>, StorageError> {
    DOMAIN.trust_source(state_dir, project_root, source_rel_path, digest)
}

pub fn trust_workflow(
    state_dir: &StateDir,
    project_root: &Path,
    source_rel_path: &str,
    digest: &str,
) -> Result<(), StorageError> {
    DOMAIN.trust(state_dir, project_root, source_rel_path, digest)
}

pub fn revoke_workflow_trust(
    state_dir: &StateDir,
    project_root: &Path,
    source_rel_path: &str,
) -> Result<(), StorageError> {
    DOMAIN.revoke(state_dir, project_root, source_rel_path)
}

pub fn is_remote_workflow_trusted(
    state_dir: &StateDir,
    asset: &ProjectAssetTrustKey,
    digest: &str,
) -> Result<bool, StorageError> {
    DOMAIN.is_remote_trusted(state_dir, asset, digest)
}

pub fn trust_remote_workflow(
    state_dir: &StateDir,
    asset: &ProjectAssetTrustKey,
    digest: &str,
) -> Result<(), StorageError> {
    DOMAIN.trust_remote(state_dir, asset, digest)
}

pub fn revoke_remote_workflow_trust(
    state_dir: &StateDir,
    asset: &ProjectAssetTrustKey,
) -> Result<(), StorageError> {
    DOMAIN.revoke_remote(state_dir, asset)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    const SOURCE: &[u8] = b"let meta = #{ name: \"review\" };";
    const SOURCE_PATH: &str = ".caudra/workflows/review.rhai";
    const LANGUAGE_VERSION: u32 = 1;
    const ABI_VERSION: u32 = 1;
    const SOURCE_DIGEST: &str = "725998d0ba03af3a48a685d6d9e3e06a6ba4cf4170d7aea466c6b1565594bae2";
    const DIGEST_IS_UNCHANGED: &str = "workflow digests must stay byte-identical";
    const AUTOMATIONS_ARE_SEPARATE: &str = "a workflow grant must not trust an automation";

    #[test]
    fn workflow_trust_keeps_its_digest_and_domain() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        let digest = workflow_source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION);

        trust_workflow(&state_dir, &project, SOURCE_PATH, &digest).unwrap();

        assert_eq!(digest, SOURCE_DIGEST, "{DIGEST_IS_UNCHANGED}");
        assert_eq!(
            workflow_trust_source(&state_dir, &project, SOURCE_PATH, &digest).unwrap(),
            Some(TrustSource::Exact)
        );
        assert!(
            !TrustDomain::Automation
                .is_trusted(&state_dir, &project, SOURCE_PATH, &digest)
                .unwrap(),
            "{AUTOMATIONS_ARE_SEPARATE}"
        );
        revoke_workflow_trust(&state_dir, &project, SOURCE_PATH).unwrap();
        assert!(!is_workflow_trusted(&state_dir, &project, SOURCE_PATH, &digest).unwrap());
    }
}
