//! Where a project's own state lives.
//!
//! Every value here is a compatibility surface: the directory name is derived
//! from a hash of the project root, so any drift silently orphans notes and
//! plans a user already wrote.

use std::fmt;
use std::path::{Path, PathBuf};

use caudra_workspace::{ProjectKey, SessionWorkspaceBinding};

use crate::workspace_binding::opaque_hash;
use crate::{StateClass, StateDir, StorageError};

pub(crate) const GIT_MARKER: &str = ".git";
pub(crate) const PROJECTS_DIR: &str = "projects";

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const DOCUMENT_SCOPE_DOMAIN: &str = "remote-local-documents.v1";
const REMOTE_SCRATCH_DOMAIN: &str = "remote-scratch-directory.v1";

#[derive(Clone)]
pub(crate) enum DocumentProjectScope {
    Local(LocalProjectAliases),
    Remote {
        project: ProjectKey,
        subdir: PathBuf,
    },
}

impl DocumentProjectScope {
    pub(crate) fn remote(binding: &SessionWorkspaceBinding) -> Self {
        Self::Remote {
            project: binding.project().key().clone(),
            subdir: Path::new(PROJECTS_DIR).join(format!(
                "remote-docs-{}",
                opaque_hash(DOCUMENT_SCOPE_DOMAIN, &remote_identity(binding))
            )),
        }
    }

    pub(crate) fn project_key(&self) -> &ProjectKey {
        match self {
            Self::Local(aliases) => aliases.project_key(),
            Self::Remote { project, .. } => project,
        }
    }

    pub(crate) fn read_subdirs(&self) -> Vec<&Path> {
        match self {
            Self::Local(aliases) => aliases.read_subdirs(),
            Self::Remote { subdir, .. } => vec![subdir],
        }
    }

    pub(crate) fn ensure_write_subdir(&self, state: &StateDir) -> Result<PathBuf, StorageError> {
        match self {
            Self::Local(aliases) => aliases.ensure_write_subdir(state),
            Self::Remote { subdir, .. } => state
                .for_class(StateClass::Persistent)
                .ensure_subdir(subdir),
        }
    }

    pub(crate) fn matches_remote(&self, binding: &SessionWorkspaceBinding) -> bool {
        match (self, Self::remote(binding)) {
            (
                Self::Remote { subdir, .. },
                Self::Remote {
                    subdir: expected, ..
                },
            ) => *subdir == expected,
            _ => false,
        }
    }

    pub(crate) fn reference_namespace(&self) -> &[u8] {
        match self {
            Self::Local(aliases) => aliases.project_key().as_str().as_bytes(),
            Self::Remote { subdir, .. } => subdir.as_os_str().as_encoded_bytes(),
        }
    }
}

/// Length-prefixed so no two different field splits hash alike. Every field is
/// vouched for by the remote authority, which is what makes a value derived
/// from it safe to key a directory on the remote host with.
fn remote_identity(binding: &SessionWorkspaceBinding) -> Vec<u8> {
    let authority = binding.authority();
    let mut identity = Vec::new();
    for field in [
        authority.trust_anchor().as_str(),
        authority.server_id(),
        authority.workspace_id(),
        authority.workspace_generation(),
        authority.resource_namespace_version(),
        binding.principal().subject(),
        binding.project().key().as_str(),
    ] {
        identity.extend_from_slice(&(field.len() as u64).to_be_bytes());
        identity.extend_from_slice(field.as_bytes());
    }
    identity
}

/// The directory name a remote project's scratch work goes under, keyed by the
/// remote identity rather than by any local path, because no local path is
/// meaningful on the host the tools actually run on.
///
/// The hash is base58, so the whole name is alphanumeric behind a fixed
/// prefix: nothing in it can be read as an option, a path separator, or a
/// shell metacharacter by the remote command that creates it.
pub fn remote_scratch_id(binding: &SessionWorkspaceBinding) -> String {
    format!(
        "remote-{}",
        opaque_hash(REMOTE_SCRATCH_DOMAIN, &remote_identity(binding))
    )
}

/// FNV-1a over the raw bytes, lowercase hex. The Lua original split the state
/// into 32-bit halves because `bit32` has no 64-bit ops; the arithmetic was
/// plain FNV-1a and this is the same function.
fn fnv1a_64(data: &str) -> String {
    let hash = data.bytes().fold(FNV_OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    });
    format!("{hash:016x}")
}

/// Readable prefix plus a hash: two checkouts of the same repo need different
/// directories, and the user still wants to recognize theirs on disk.
pub fn project_id(root: &Path) -> String {
    let base = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("root");
    format!("{base}-{}", fnv1a_64(&root.to_string_lossy()))
}

/// The project root is the enclosing repository, so state follows the checkout
/// rather than whichever subdirectory the session started in.
pub fn project_root(cwd: &Path) -> PathBuf {
    let mut dir = cwd;
    loop {
        if dir.join(GIT_MARKER).exists() {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return cwd.to_path_buf(),
        }
    }
}

/// The state-directory-relative home for everything scoped to `cwd`'s project.
pub fn project_subdir(cwd: &Path) -> PathBuf {
    Path::new(PROJECTS_DIR).join(project_id(&project_root(cwd)))
}

/// A project's own corner of the scratch root, keyed the way its state is, so
/// two checkouts writing the same filename do not collide.
///
/// The scratch root holds nothing but these, so they sit directly inside it
/// rather than under a `projects` level. Keying on the repository rather than
/// `cwd` means a session started in a subdirectory shares the repository's
/// scratch, which is the same reason [`project_root`] exists.
pub fn project_scratch_dir(cwd: &Path) -> Result<PathBuf, std::io::Error> {
    let root = crate::paths::scratch_root()?;
    crate::paths::ensure_private_dir(&root.join(project_id(&project_root(cwd))))
}

#[derive(Clone, PartialEq, Eq)]
pub struct LocalProjectAliases {
    project_key: ProjectKey,
    legacy_id: LegacyLocalProjectId,
    keyed_subdir: PathBuf,
    legacy_subdir: PathBuf,
}

impl fmt::Debug for LocalProjectAliases {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalProjectAliases")
            .field("project_key", &self.project_key)
            .field("legacy_id", &self.legacy_id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct LegacyLocalProjectId(String);

impl LegacyLocalProjectId {
    pub fn from_root(root: &Path) -> Self {
        Self(project_id(root))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for LegacyLocalProjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("LegacyLocalProjectId")
            .field(&"<opaque>")
            .finish()
    }
}

impl LocalProjectAliases {
    pub fn new(cwd: &Path, project_key: ProjectKey) -> Self {
        let legacy_id = LegacyLocalProjectId::from_root(&project_root(cwd));
        let keyed_id = format!(
            "key-{}",
            opaque_hash("local-project-directory", project_key.as_str().as_bytes())
        );
        Self {
            project_key,
            legacy_subdir: Path::new(PROJECTS_DIR).join(legacy_id.as_str()),
            legacy_id,
            keyed_subdir: Path::new(PROJECTS_DIR).join(keyed_id),
        }
    }

    pub fn project_key(&self) -> &ProjectKey {
        &self.project_key
    }

    pub fn keyed_subdir(&self) -> &Path {
        &self.keyed_subdir
    }

    pub fn legacy_id(&self) -> &LegacyLocalProjectId {
        &self.legacy_id
    }

    pub fn legacy_subdir(&self) -> &Path {
        &self.legacy_subdir
    }

    pub fn read_subdirs(&self) -> Vec<&Path> {
        if self.keyed_subdir == self.legacy_subdir {
            vec![&self.keyed_subdir]
        } else {
            vec![&self.keyed_subdir, &self.legacy_subdir]
        }
    }

    pub fn resolve_existing(&self, state_dir: &StateDir) -> Option<PathBuf> {
        self.read_subdirs()
            .into_iter()
            .map(|subdir| state_dir.persistent_path().join(subdir))
            .find(|path| path.is_dir())
    }

    pub fn ensure_write_subdir(&self, state_dir: &StateDir) -> Result<PathBuf, StorageError> {
        if let Some(existing) = self.resolve_existing(state_dir) {
            return Ok(existing);
        }
        state_dir
            .for_class(StateClass::Persistent)
            .ensure_subdir(&self.keyed_subdir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    /// Pinned, not computed: this is the on-disk directory name for an existing
    /// user's state. If it changes, their memories and plans disappear.
    #[test_case("", "cbf29ce484222325" ; "empty is the offset basis")]
    #[test_case("a", "af63dc4c8601ec8c" ; "single byte")]
    #[test_case("foobar", "85944171f73967e8" ; "known fnv vector")]
    fn fnv1a_matches_the_reference_vectors(input: &str, expected: &str) {
        assert_eq!(fnv1a_64(input), expected);
    }

    #[test]
    fn a_project_id_is_the_directory_name_and_a_hash() {
        let id = project_id(Path::new("/home/user/app"));
        assert!(id.starts_with("app-"), "{id}");
        assert_eq!(id.len(), "app-".len() + 16);
    }

    /// Two checkouts of one repo must not share a state directory.
    #[test]
    fn same_named_directories_in_different_places_differ() {
        assert_ne!(
            project_id(Path::new("/a/app")),
            project_id(Path::new("/b/app"))
        );
    }

    #[test]
    fn a_rootless_path_still_produces_an_id() {
        assert!(project_id(Path::new("/")).starts_with("root-"));
    }

    #[test]
    fn the_repository_root_wins_over_the_working_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let nested = root.join("crates/inner");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(GIT_MARKER)).unwrap();
        assert_eq!(project_root(&nested), root);
    }

    #[test]
    fn without_a_repository_the_working_directory_is_the_root() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(project_root(temp.path()), temp.path());
    }

    #[test]
    fn a_subdirectory_shares_the_repositorys_state_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let nested = root.join("crates/inner");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(GIT_MARKER)).unwrap();
        assert_eq!(project_subdir(&nested), project_subdir(&root));
    }

    #[test]
    fn a_subdirectory_shares_the_repositorys_scratch_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let nested = root.join("crates/inner");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(GIT_MARKER)).unwrap();

        let scratch = project_scratch_dir(&nested).unwrap();

        assert_eq!(scratch, project_scratch_dir(&root).unwrap());
        assert_eq!(
            scratch.parent(),
            Some(crate::paths::scratch_root().unwrap().as_path())
        );
    }

    /// Two checkouts named the same are the case the hash in `project_id`
    /// exists for, and the case a flat scratch directory got wrong.
    #[test]
    fn checkouts_sharing_a_name_get_different_scratch_directories() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("a/caudra");
        let second = temp.path().join("b/caudra");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();

        assert_ne!(
            project_scratch_dir(&first).unwrap(),
            project_scratch_dir(&second).unwrap()
        );
    }

    #[test]
    fn keyed_projects_dual_read_legacy_state_without_moving_it() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let cwd = temp.path().join("checkout");
        let aliases = LocalProjectAliases::new(&cwd, ProjectKey::new("authority-project").unwrap());
        let legacy = state_dir.persistent_path().join(aliases.legacy_subdir());
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("memory.md"), "kept").unwrap();

        assert_eq!(aliases.resolve_existing(&state_dir), Some(legacy.clone()));
        assert_eq!(
            aliases.legacy_id().as_str(),
            project_id(&project_root(&cwd))
        );
        assert_eq!(aliases.ensure_write_subdir(&state_dir).unwrap(), legacy);
        assert!(
            !state_dir
                .persistent_path()
                .join(aliases.keyed_subdir())
                .exists()
        );
    }

    #[test]
    fn new_project_key_state_is_path_safe_and_created_lazily() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let aliases = LocalProjectAliases::new(
            Path::new("/missing/project"),
            ProjectKey::new("project/with/path-syntax").unwrap(),
        );

        assert_eq!(aliases.resolve_existing(&state_dir), None);
        let created = aliases.ensure_write_subdir(&state_dir).unwrap();
        assert_eq!(
            created,
            state_dir.persistent_path().join(aliases.keyed_subdir())
        );
        assert!(created.is_dir());
    }
}
