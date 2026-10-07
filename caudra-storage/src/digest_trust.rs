//! Exact-digest trust for the project sources Caudra runs. A source is trusted
//! only as the precise bytes the user approved under the language and ABI they
//! approved them for; any change to either makes it untrusted again. Each
//! domain digests under its own preamble and keeps its grants under its own
//! key, so approving a file as a workflow never trusts it as an automation.

use std::collections::HashMap;
use std::fmt::Write;
use std::io;
use std::path::{Component, Path};

use caudra_workspace::ProjectAssetTrustKey;
use sha2::{Digest, Sha256};

use crate::checkout::{self, TrustSource};
use crate::state::{self, StateKey, StateStore, project_scope};
use crate::{StateClass, StateDir, StorageError};

const WORKFLOW_TRUST: StateKey = StateKey {
    name: "workflow.trust",
    class: StateClass::Persistent,
};
const AUTOMATION_TRUST: StateKey = StateKey {
    name: "automation.trust",
    class: StateClass::Persistent,
};
const WORKFLOW_PREAMBLE: &str = "caudra-workflow-source/v1\n";
const AUTOMATION_PREAMBLE: &str = "caudra-automation-source/v1\n";
const SHA256_HEX_LEN: usize = 64;
const WORKFLOW_INVALID_DIGEST: &str =
    "workflow source digest must be a lowercase 64-character SHA-256 hex string";
const AUTOMATION_INVALID_DIGEST: &str =
    "automation source digest must be a lowercase 64-character SHA-256 hex string";
const WORKFLOW_INVALID_SOURCE_PATH: &str =
    "workflow source path must be a non-empty relative path without parent references";
const AUTOMATION_INVALID_SOURCE_PATH: &str =
    "automation source path must be a non-empty relative path without parent references";

/// What a trusted source runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrustDomain {
    Workflow,
    Automation,
}

impl TrustDomain {
    const fn key(self) -> StateKey {
        match self {
            Self::Workflow => WORKFLOW_TRUST,
            Self::Automation => AUTOMATION_TRUST,
        }
    }

    const fn preamble(self) -> &'static str {
        match self {
            Self::Workflow => WORKFLOW_PREAMBLE,
            Self::Automation => AUTOMATION_PREAMBLE,
        }
    }

    const fn invalid_digest(self) -> &'static str {
        match self {
            Self::Workflow => WORKFLOW_INVALID_DIGEST,
            Self::Automation => AUTOMATION_INVALID_DIGEST,
        }
    }

    const fn invalid_source_path(self) -> &'static str {
        match self {
            Self::Workflow => WORKFLOW_INVALID_SOURCE_PATH,
            Self::Automation => AUTOMATION_INVALID_SOURCE_PATH,
        }
    }

    /// SHA-256 over the domain's version-tagged preamble and the exact source
    /// bytes. Tagging the language and ABI in means the same file approved
    /// for one interpreter is not silently trusted for another.
    pub fn source_digest(self, bytes: &[u8], language_version: u32, abi_version: u32) -> String {
        let mut hasher = Sha256::new();
        hasher.update(format!("{}{language_version}\n{abi_version}\n", self.preamble()).as_bytes());
        hasher.update(bytes);
        hasher
            .finalize()
            .iter()
            .fold(String::with_capacity(SHA256_HEX_LEN), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            })
    }

    pub fn is_trusted(
        self,
        state_dir: &StateDir,
        project_root: &Path,
        source_rel_path: &str,
        digest: &str,
    ) -> Result<bool, StorageError> {
        Ok(self
            .trust_source(state_dir, project_root, source_rel_path, digest)?
            .is_some())
    }

    /// The project's own grant for the source, or else one for the identical
    /// source at the same path in another verified checkout of its repository.
    pub fn trust_source(
        self,
        state_dir: &StateDir,
        project_root: &Path,
        source_rel_path: &str,
        digest: &str,
    ) -> Result<Option<TrustSource>, StorageError> {
        self.validate_digest(digest)?;
        let source = self.normalize_source_path(source_rel_path)?;
        let key = self.key();
        let store = StateStore::open(state_dir, key.class)?;
        let found = TrustSource::find::<StorageError>(project_root, |candidate| {
            let trust = store
                .get::<HashMap<String, String>>(&scope(candidate)?, key)?
                .unwrap_or_default();
            self.validate(&trust)?;
            Ok(trust.get(&source).is_some_and(|stored| stored == digest))
        })?;
        if let Some(TrustSource::Inherited(from)) = &found {
            tracing::debug!(
                domain = ?self,
                project = %project_root.display(),
                from = %from.display(),
                source,
                "source trusted through a sibling checkout"
            );
        }
        Ok(found)
    }

    pub fn trust(
        self,
        state_dir: &StateDir,
        project_root: &Path,
        source_rel_path: &str,
        digest: &str,
    ) -> Result<(), StorageError> {
        self.validate_digest(digest)?;
        let source = self.normalize_source_path(source_rel_path)?;
        let scope = scope(project_root)?;
        state::try_update(
            state_dir,
            &scope,
            self.key(),
            |trust: &mut HashMap<String, String>| {
                self.validate(trust)?;
                trust.insert(source, digest.to_owned());
                Ok(())
            },
        )?
    }

    /// Revokes the source's grant in `project_root` and in its counterpart in
    /// every sibling checkout, any of which it could otherwise still inherit.
    pub fn revoke(
        self,
        state_dir: &StateDir,
        project_root: &Path,
        source_rel_path: &str,
    ) -> Result<(), StorageError> {
        let source = self.normalize_source_path(source_rel_path)?;
        let key = self.key();
        let mut store = StateStore::open(state_dir, key.class)?;
        for path in checkout::with_siblings(project_root) {
            store.try_update(
                &scope(&path)?,
                key,
                |trust: &mut HashMap<String, String>| {
                    self.validate(trust)?;
                    trust.remove(&source);
                    Ok::<_, StorageError>(())
                },
            )??;
        }
        Ok(())
    }

    pub fn is_remote_trusted(
        self,
        state_dir: &StateDir,
        asset: &ProjectAssetTrustKey,
        digest: &str,
    ) -> Result<bool, StorageError> {
        self.validate_digest(digest)?;
        let scope = state::remote_asset_scope(asset);
        Ok(state::get::<String>(state_dir, &scope, self.key())?
            .is_some_and(|stored| stored == digest))
    }

    pub fn trust_remote(
        self,
        state_dir: &StateDir,
        asset: &ProjectAssetTrustKey,
        digest: &str,
    ) -> Result<(), StorageError> {
        self.validate_digest(digest)?;
        state::set(
            state_dir,
            &state::remote_asset_scope(asset),
            self.key(),
            &digest.to_owned(),
        )
    }

    pub fn revoke_remote(
        self,
        state_dir: &StateDir,
        asset: &ProjectAssetTrustKey,
    ) -> Result<(), StorageError> {
        state::delete(state_dir, &state::remote_asset_scope(asset), self.key())?;
        Ok(())
    }

    /// The key a source is trusted under: its normal components joined by
    /// `/`. Anything that could escape the project or name it two ways is
    /// refused.
    fn normalize_source_path(self, source_rel_path: &str) -> Result<String, StorageError> {
        let mut normalized = String::with_capacity(source_rel_path.len());
        for component in Path::new(source_rel_path).components() {
            match component {
                Component::Normal(part) => {
                    let Some(part) = part.to_str() else {
                        return Err(invalid_data(self.invalid_source_path()));
                    };
                    if !normalized.is_empty() {
                        normalized.push('/');
                    }
                    normalized.push_str(part);
                }
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(invalid_data(self.invalid_source_path()));
                }
            }
        }
        if normalized.is_empty() {
            return Err(invalid_data(self.invalid_source_path()));
        }
        Ok(normalized)
    }

    fn validate(self, trust: &HashMap<String, String>) -> Result<(), StorageError> {
        trust
            .values()
            .try_for_each(|digest| self.validate_digest(digest))
    }

    fn validate_digest(self, digest: &str) -> Result<(), StorageError> {
        if digest.len() == SHA256_HEX_LEN
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            Ok(())
        } else {
            Err(invalid_data(self.invalid_digest()))
        }
    }
}

fn scope(project_root: &Path) -> Result<String, StorageError> {
    Ok(project_scope(&project_root.canonicalize()?))
}

fn invalid_data(message: &'static str) -> StorageError {
    StorageError::Io(io::Error::new(io::ErrorKind::InvalidData, message))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::checkout::fixture::linked_pair_with_state;

    const SOURCE: &[u8] = b"let meta = #{ name: \"review\" };";
    const OTHER_SOURCE: &[u8] = b"let meta = #{ name: \"review\" }; ";
    const SOURCE_PATH: &str = ".caudra/workflows/review.rhai";
    const OTHER_SOURCE_PATH: &str = ".caudra/workflows/deploy.rhai";
    const LANGUAGE_VERSION: u32 = 1;
    const ABI_VERSION: u32 = 1;
    const WORKFLOW_SOURCE_DIGEST: &str =
        "725998d0ba03af3a48a685d6d9e3e06a6ba4cf4170d7aea466c6b1565594bae2";
    const AUTOMATION_SOURCE_DIGEST: &str =
        "05a982e7e55921a467692e835c49d6f956c2f016fffcdf0b9fbf511de08c782d";
    const WORKFLOW_KEY_NAME: &str = "workflow.trust";
    const AUTOMATION_KEY_NAME: &str = "automation.trust";
    const DIGESTS_ARE_PINNED: &str = "a domain's digest must never change for the same bytes";
    const BYTES_ARE_EXACT: &str = "a changed byte must make the source untrusted";
    const VERSIONS_ARE_EXACT: &str = "a changed interpreter version must make the source untrusted";
    const PROJECTS_ARE_SEPARATE: &str = "trust in one project must not reach another";
    const PATHS_NORMALIZE: &str = "the same file spelled differently must share one trust entry";
    const DOMAINS_ARE_SEPARATE: &str = "trust in one domain must not reach another";

    fn setup() -> (TempDir, StateDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        (temp, state_dir, project)
    }

    fn other(domain: TrustDomain) -> TrustDomain {
        match domain {
            TrustDomain::Workflow => TrustDomain::Automation,
            TrustDomain::Automation => TrustDomain::Workflow,
        }
    }

    #[test_case(TrustDomain::Workflow, WORKFLOW_SOURCE_DIGEST; "workflow")]
    #[test_case(TrustDomain::Automation, AUTOMATION_SOURCE_DIGEST; "automation")]
    fn digests_are_pinned(domain: TrustDomain, expected: &str) {
        let digest = domain.source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION);

        assert_eq!(digest, expected, "{DIGESTS_ARE_PINNED}");
        assert!(domain.validate_digest(&digest).is_ok());
    }

    #[test_case(TrustDomain::Workflow, WORKFLOW_KEY_NAME; "workflow")]
    #[test_case(TrustDomain::Automation, AUTOMATION_KEY_NAME; "automation")]
    fn grants_persist_under_the_domain_key(domain: TrustDomain, key_name: &'static str) {
        let (_temp, state_dir, project) = setup();
        let digest = domain.source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION);
        let key = StateKey {
            name: key_name,
            class: StateClass::Persistent,
        };

        domain
            .trust(&state_dir, &project, SOURCE_PATH, &digest)
            .unwrap();

        let stored =
            state::get::<HashMap<String, String>>(&state_dir, &scope(&project).unwrap(), key)
                .unwrap()
                .unwrap();
        assert_eq!(stored.get(SOURCE_PATH), Some(&digest));
    }

    #[test_case(TrustDomain::Workflow; "workflow")]
    #[test_case(TrustDomain::Automation; "automation")]
    fn a_grant_in_one_domain_trusts_nothing_in_another(domain: TrustDomain) {
        let (_temp, state_dir, project) = setup();
        let digest = domain.source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION);

        domain
            .trust(&state_dir, &project, SOURCE_PATH, &digest)
            .unwrap();

        assert_ne!(
            digest,
            other(domain).source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION),
            "{DOMAINS_ARE_SEPARATE}"
        );
        assert!(
            !other(domain)
                .is_trusted(&state_dir, &project, SOURCE_PATH, &digest)
                .unwrap(),
            "{DOMAINS_ARE_SEPARATE}"
        );
    }

    #[test_case(TrustDomain::Workflow; "workflow")]
    #[test_case(TrustDomain::Automation; "automation")]
    fn a_byte_change_flips_trust(domain: TrustDomain) {
        let (_temp, state_dir, project) = setup();
        let digest = domain.source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION);
        let changed = domain.source_digest(OTHER_SOURCE, LANGUAGE_VERSION, ABI_VERSION);

        domain
            .trust(&state_dir, &project, SOURCE_PATH, &digest)
            .unwrap();

        assert!(
            domain
                .is_trusted(&state_dir, &project, SOURCE_PATH, &digest)
                .unwrap()
        );
        assert!(
            !domain
                .is_trusted(&state_dir, &project, SOURCE_PATH, &changed)
                .unwrap(),
            "{BYTES_ARE_EXACT}"
        );
        domain.revoke(&state_dir, &project, SOURCE_PATH).unwrap();
        assert!(
            !domain
                .is_trusted(&state_dir, &project, SOURCE_PATH, &digest)
                .unwrap()
        );
    }

    #[test_case(TrustDomain::Workflow, LANGUAGE_VERSION + 1, ABI_VERSION; "workflow_language")]
    #[test_case(TrustDomain::Workflow, LANGUAGE_VERSION, ABI_VERSION + 1; "workflow_abi")]
    #[test_case(TrustDomain::Automation, LANGUAGE_VERSION + 1, ABI_VERSION; "automation_language")]
    #[test_case(TrustDomain::Automation, LANGUAGE_VERSION, ABI_VERSION + 1; "automation_abi")]
    fn a_version_change_flips_trust(domain: TrustDomain, language_version: u32, abi_version: u32) {
        let (_temp, state_dir, project) = setup();
        let digest = domain.source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION);
        let changed = domain.source_digest(SOURCE, language_version, abi_version);
        domain
            .trust(&state_dir, &project, SOURCE_PATH, &digest)
            .unwrap();

        assert_ne!(digest, changed, "{VERSIONS_ARE_EXACT}");
        assert!(
            !domain
                .is_trusted(&state_dir, &project, SOURCE_PATH, &changed)
                .unwrap(),
            "{VERSIONS_ARE_EXACT}"
        );
    }

    #[test]
    fn trust_is_scoped_to_the_canonical_project_root() {
        let (temp, state_dir, project) = setup();
        let other_project = temp.path().join("other");
        fs::create_dir(&other_project).unwrap();
        let domain = TrustDomain::Automation;
        let digest = domain.source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION);

        domain
            .trust(&state_dir, &project.join("."), SOURCE_PATH, &digest)
            .unwrap();

        assert!(
            domain
                .is_trusted(&state_dir, &project, SOURCE_PATH, &digest)
                .unwrap()
        );
        assert!(
            !domain
                .is_trusted(&state_dir, &other_project, SOURCE_PATH, &digest)
                .unwrap(),
            "{PROJECTS_ARE_SEPARATE}"
        );
    }

    #[test_case("./.caudra/workflows/review.rhai"; "leading_dot")]
    #[test_case(".caudra//workflows/review.rhai"; "double_separator")]
    #[test_case(".caudra/./workflows/review.rhai/"; "inner_dot_and_trailing_separator")]
    fn equivalent_paths_share_one_entry(spelling: &str) {
        let (_temp, state_dir, project) = setup();
        let domain = TrustDomain::Workflow;
        let digest = domain.source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION);

        domain
            .trust(&state_dir, &project, spelling, &digest)
            .unwrap();

        assert_eq!(domain.normalize_source_path(spelling).unwrap(), SOURCE_PATH);
        assert!(
            domain
                .is_trusted(&state_dir, &project, SOURCE_PATH, &digest)
                .unwrap(),
            "{PATHS_NORMALIZE}"
        );
    }

    #[test_case(""; "empty")]
    #[test_case("."; "only_dot")]
    #[test_case("/etc/workflow.rhai"; "absolute")]
    #[test_case("../review.rhai"; "parent")]
    #[test_case("workflows/../../review.rhai"; "inner_parent")]
    fn escaping_paths_are_refused(spelling: &str) {
        let (_temp, state_dir, project) = setup();
        let domain = TrustDomain::Automation;
        let digest = domain.source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION);

        let error = domain
            .trust(&state_dir, &project, spelling, &digest)
            .unwrap_err();

        assert!(matches!(
            error,
            StorageError::Io(error) if error.kind() == io::ErrorKind::InvalidData
                && error.to_string() == AUTOMATION_INVALID_SOURCE_PATH
        ));
        assert!(
            domain
                .is_trusted(&state_dir, &project, spelling, &digest)
                .is_err()
        );
    }

    #[test_case(SOURCE_PATH, SOURCE, true ; "identical_source")]
    #[test_case("./.caudra/workflows/review.rhai", SOURCE, true ; "equivalent_spelling")]
    #[test_case(OTHER_SOURCE_PATH, SOURCE, false ; "other_path")]
    #[test_case(SOURCE_PATH, OTHER_SOURCE, false ; "changed_bytes")]
    fn a_worktree_inherits_only_the_identical_source(path: &str, bytes: &[u8], inherited: bool) {
        let (_temp, state_dir, main, worktree) = linked_pair_with_state();
        let domain = TrustDomain::Automation;
        let digest = domain.source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION);
        domain
            .trust(&state_dir, &main, SOURCE_PATH, &digest)
            .unwrap();

        let checked = domain.source_digest(bytes, LANGUAGE_VERSION, ABI_VERSION);

        assert_eq!(
            domain
                .trust_source(&state_dir, &worktree, path, &checked)
                .unwrap(),
            inherited.then_some(TrustSource::Inherited(main))
        );
    }

    #[test]
    fn revoking_in_a_worktree_clears_the_grant_it_inherited() {
        let (_temp, state_dir, main, worktree) = linked_pair_with_state();
        let domain = TrustDomain::Workflow;
        let digest = domain.source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION);
        domain
            .trust(&state_dir, &main, SOURCE_PATH, &digest)
            .unwrap();

        domain.revoke(&state_dir, &worktree, SOURCE_PATH).unwrap();

        assert!(
            !domain
                .is_trusted(&state_dir, &main, SOURCE_PATH, &digest)
                .unwrap()
        );
        assert!(
            !domain
                .is_trusted(&state_dir, &worktree, SOURCE_PATH, &digest)
                .unwrap()
        );
    }

    #[test_case(TrustDomain::Workflow, WORKFLOW_INVALID_DIGEST; "workflow")]
    #[test_case(TrustDomain::Automation, AUTOMATION_INVALID_DIGEST; "automation")]
    fn invalid_digests_are_refused(domain: TrustDomain, message: &str) {
        let (_temp, state_dir, project) = setup();

        let error = domain
            .trust(&state_dir, &project, SOURCE_PATH, "not-a-digest")
            .unwrap_err();

        assert!(matches!(
            error,
            StorageError::Io(error) if error.kind() == io::ErrorKind::InvalidData
                && error.to_string() == message
        ));
    }

    #[test_case(TrustDomain::Workflow; "workflow")]
    #[test_case(TrustDomain::Automation; "automation")]
    fn ephemeral_access_uses_the_persistent_root(domain: TrustDomain) {
        let temp = tempfile::tempdir().unwrap();
        let persistent = StateDir::from_path(temp.path().join("persistent"));
        let state_dir = StateDir::split(temp.path().join("volatile"), persistent.path().into());
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        let digest = domain.source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION);

        domain
            .trust(&state_dir, &project, SOURCE_PATH, &digest)
            .unwrap();

        assert!(
            domain
                .is_trusted(&persistent, &project, SOURCE_PATH, &digest)
                .unwrap()
        );
        assert!(!state_dir.path().exists());
    }
}
