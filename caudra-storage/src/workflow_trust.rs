//! Exact-digest trust for project workflow sources. A source is trusted only
//! as the precise bytes the user approved under the language and ABI they
//! approved them for; any change to either makes it untrusted again.

use std::collections::HashMap;
use std::fmt::Write;
use std::io;
use std::path::{Component, Path};

use sha2::{Digest, Sha256};

use crate::state::{self, StateKey, project_scope};
use crate::{StateClass, StateDir, StorageError};

const TRUST: StateKey = StateKey {
    name: "workflow.trust",
    class: StateClass::Persistent,
};
const DIGEST_PREAMBLE: &str = "caudra-workflow-source/v1\n";
const SHA256_HEX_LEN: usize = 64;
const INVALID_DIGEST: &str =
    "workflow source digest must be a lowercase 64-character SHA-256 hex string";
const INVALID_SOURCE_PATH: &str =
    "workflow source path must be a non-empty relative path without parent references";

/// SHA-256 over a version-tagged preamble and the exact source bytes. Tagging
/// the language and ABI in means the same file approved for one interpreter
/// is not silently trusted for another.
pub fn workflow_source_digest(bytes: &[u8], language_version: u32, abi_version: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{DIGEST_PREAMBLE}{language_version}\n{abi_version}\n").as_bytes());
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(SHA256_HEX_LEN), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

pub fn is_workflow_trusted(
    state_dir: &StateDir,
    project_root: &Path,
    source_rel_path: &str,
    digest: &str,
) -> Result<bool, StorageError> {
    validate_digest(digest)?;
    let source = normalize_source_path(source_rel_path)?;
    let trust = state::get::<HashMap<String, String>>(state_dir, &scope(project_root)?, TRUST)?
        .unwrap_or_default();
    validate(&trust)?;
    Ok(trust.get(&source).is_some_and(|stored| stored == digest))
}

pub fn trust_workflow(
    state_dir: &StateDir,
    project_root: &Path,
    source_rel_path: &str,
    digest: &str,
) -> Result<(), StorageError> {
    validate_digest(digest)?;
    let source = normalize_source_path(source_rel_path)?;
    let scope = scope(project_root)?;
    state::try_update(
        state_dir,
        &scope,
        TRUST,
        |trust: &mut HashMap<String, String>| {
            validate(trust)?;
            trust.insert(source, digest.to_owned());
            Ok(())
        },
    )?
}

pub fn revoke_workflow_trust(
    state_dir: &StateDir,
    project_root: &Path,
    source_rel_path: &str,
) -> Result<(), StorageError> {
    let source = normalize_source_path(source_rel_path)?;
    let scope = scope(project_root)?;
    state::try_update(
        state_dir,
        &scope,
        TRUST,
        |trust: &mut HashMap<String, String>| {
            validate(trust)?;
            trust.remove(&source);
            Ok(())
        },
    )?
}

pub fn is_remote_workflow_trusted(
    state_dir: &StateDir,
    asset: &caudra_workspace::ProjectAssetTrustKey,
    digest: &str,
) -> Result<bool, StorageError> {
    validate_digest(digest)?;
    let scope = state::remote_asset_scope(asset);
    Ok(state::get::<String>(state_dir, &scope, TRUST)?.is_some_and(|stored| stored == digest))
}

pub fn trust_remote_workflow(
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

pub fn revoke_remote_workflow_trust(
    state_dir: &StateDir,
    asset: &caudra_workspace::ProjectAssetTrustKey,
) -> Result<(), StorageError> {
    state::delete(state_dir, &state::remote_asset_scope(asset), TRUST)?;
    Ok(())
}

fn scope(project_root: &Path) -> Result<String, StorageError> {
    Ok(project_scope(&project_root.canonicalize()?))
}

/// The key a source is trusted under: its normal components joined by `/`.
/// Anything that could escape the project or name it two ways is refused.
fn normalize_source_path(source_rel_path: &str) -> Result<String, StorageError> {
    let mut normalized = String::with_capacity(source_rel_path.len());
    for component in Path::new(source_rel_path).components() {
        match component {
            Component::Normal(part) => {
                let Some(part) = part.to_str() else {
                    return Err(invalid_data(INVALID_SOURCE_PATH));
                };
                if !normalized.is_empty() {
                    normalized.push('/');
                }
                normalized.push_str(part);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(invalid_data(INVALID_SOURCE_PATH));
            }
        }
    }
    if normalized.is_empty() {
        return Err(invalid_data(INVALID_SOURCE_PATH));
    }
    Ok(normalized)
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
        Err(invalid_data(INVALID_DIGEST))
    }
}

fn invalid_data(message: &'static str) -> StorageError {
    StorageError::Io(io::Error::new(io::ErrorKind::InvalidData, message))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use test_case::test_case;

    use super::*;

    const SOURCE: &[u8] = b"let meta = #{ name: \"review\" };";
    const OTHER_SOURCE: &[u8] = b"let meta = #{ name: \"review\" }; ";
    const SOURCE_PATH: &str = ".caudra/workflows/review.rhai";
    const LANGUAGE_VERSION: u32 = 1;
    const ABI_VERSION: u32 = 1;
    const BYTES_ARE_EXACT: &str = "a changed byte must make the source untrusted";
    const VERSIONS_ARE_EXACT: &str = "a changed interpreter version must make the source untrusted";
    const PROJECTS_ARE_SEPARATE: &str = "trust in one project must not reach another";
    const PATHS_NORMALIZE: &str = "the same file spelled differently must share one trust entry";

    fn setup() -> (tempfile::TempDir, StateDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        (temp, state_dir, project)
    }

    #[test]
    fn digest_is_stable_and_hex() {
        let digest = workflow_source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION);

        assert_eq!(
            digest,
            workflow_source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION)
        );
        assert!(validate_digest(&digest).is_ok());
    }

    #[test]
    fn a_byte_change_flips_trust() {
        let (_temp, state_dir, project) = setup();
        let digest = workflow_source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION);
        let other = workflow_source_digest(OTHER_SOURCE, LANGUAGE_VERSION, ABI_VERSION);

        trust_workflow(&state_dir, &project, SOURCE_PATH, &digest).unwrap();

        assert!(is_workflow_trusted(&state_dir, &project, SOURCE_PATH, &digest).unwrap());
        assert!(
            !is_workflow_trusted(&state_dir, &project, SOURCE_PATH, &other).unwrap(),
            "{BYTES_ARE_EXACT}"
        );
        revoke_workflow_trust(&state_dir, &project, SOURCE_PATH).unwrap();
        assert!(!is_workflow_trusted(&state_dir, &project, SOURCE_PATH, &digest).unwrap());
    }

    #[test_case(LANGUAGE_VERSION + 1, ABI_VERSION; "language")]
    #[test_case(LANGUAGE_VERSION, ABI_VERSION + 1; "abi")]
    fn a_version_change_flips_trust(language_version: u32, abi_version: u32) {
        let (_temp, state_dir, project) = setup();
        let digest = workflow_source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION);
        let other = workflow_source_digest(SOURCE, language_version, abi_version);
        trust_workflow(&state_dir, &project, SOURCE_PATH, &digest).unwrap();

        assert_ne!(digest, other, "{VERSIONS_ARE_EXACT}");
        assert!(
            !is_workflow_trusted(&state_dir, &project, SOURCE_PATH, &other).unwrap(),
            "{VERSIONS_ARE_EXACT}"
        );
    }

    #[test]
    fn trust_is_scoped_to_the_canonical_project_root() {
        let (temp, state_dir, project) = setup();
        let other_project = temp.path().join("other");
        fs::create_dir(&other_project).unwrap();
        let digest = workflow_source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION);

        trust_workflow(&state_dir, &project.join("."), SOURCE_PATH, &digest).unwrap();

        assert!(is_workflow_trusted(&state_dir, &project, SOURCE_PATH, &digest).unwrap());
        assert!(
            !is_workflow_trusted(&state_dir, &other_project, SOURCE_PATH, &digest).unwrap(),
            "{PROJECTS_ARE_SEPARATE}"
        );
    }

    #[test_case("./.caudra/workflows/review.rhai"; "leading_dot")]
    #[test_case(".caudra//workflows/review.rhai"; "double_separator")]
    #[test_case(".caudra/./workflows/review.rhai/"; "inner_dot_and_trailing_separator")]
    fn equivalent_paths_share_one_entry(spelling: &str) {
        let (_temp, state_dir, project) = setup();
        let digest = workflow_source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION);

        trust_workflow(&state_dir, &project, spelling, &digest).unwrap();

        assert_eq!(normalize_source_path(spelling).unwrap(), SOURCE_PATH);
        assert!(
            is_workflow_trusted(&state_dir, &project, SOURCE_PATH, &digest).unwrap(),
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
        let digest = workflow_source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION);

        let error = trust_workflow(&state_dir, &project, spelling, &digest).unwrap_err();

        assert!(matches!(
            error,
            StorageError::Io(error) if error.kind() == io::ErrorKind::InvalidData
        ));
        assert!(is_workflow_trusted(&state_dir, &project, spelling, &digest).is_err());
    }

    #[test]
    fn invalid_digests_are_refused() {
        let (_temp, state_dir, project) = setup();

        let error = trust_workflow(&state_dir, &project, SOURCE_PATH, "not-a-digest").unwrap_err();

        assert!(matches!(
            error,
            StorageError::Io(error) if error.kind() == io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn ephemeral_access_uses_the_persistent_root() {
        let temp = tempfile::tempdir().unwrap();
        let persistent = StateDir::from_path(temp.path().join("persistent"));
        let state_dir = StateDir::split(temp.path().join("volatile"), persistent.path().into());
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        let digest = workflow_source_digest(SOURCE, LANGUAGE_VERSION, ABI_VERSION);

        trust_workflow(&state_dir, &project, SOURCE_PATH, &digest).unwrap();

        assert!(is_workflow_trusted(&persistent, &project, SOURCE_PATH, &digest).unwrap());
        assert!(!state_dir.path().exists());
    }
}
