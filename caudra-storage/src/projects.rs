//! Where a project's own state lives.
//!
//! Every state directory here is a compatibility surface: the name is derived
//! from a hash of the project root, so any drift silently orphans notes and
//! plans a user already wrote. The scratch names are not. They point at a temp
//! root nothing outlives, which is what lets them be spelled for a reader
//! rather than for permanence.
//!
//! A verified linked worktree keeps its state with its main checkout, so every
//! checkout of a repository shares one set of notes and plans. Scratch stays
//! per checkout, so agents working side by side never share temp files.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use caudra_workspace::{ProjectKey, SessionWorkspaceBinding};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::checkout::{self, Checkout};
use crate::local_documents::MEMORIES_DIR;
use crate::paths::ensure_private_dir;
use crate::plans::PLANS_DIR;
use crate::words::derived_phrase;
use crate::workspace_binding::{local_project_key, opaque_hash};
use crate::{StateClass, StateDir, StorageError, atomic_write, sync_parent_dir};

pub(crate) const GIT_MARKER: &str = ".git";
pub(crate) const PROJECTS_DIR: &str = "projects";

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const DOCUMENT_SCOPE_DOMAIN: &str = "remote-local-documents.v1";
const REMOTE_SCRATCH_DOMAIN: &str = "remote-scratch-directory.v1";
const PROJECT_SCRATCH_DOMAIN: &str = "project-scratch-directory.v1";
const KEYED_DIRECTORY_DOMAIN: &str = "local-project-directory";
const KEYED_DIRECTORY_PREFIX: &str = "key-";
/// Left in a worktree's own state directory once it is imported, naming where
/// to. Importing twice would bring back notes deleted in between.
const ADOPTED_MARKER: &str = "adopted-into";
const IMPORTED_FROM: &str = ".from-";
/// How many `<name>.from-<branch>-<n>` spellings an import tries before it
/// leaves a file only where it was.
const MAX_IMPORT_RENAMES: usize = 10;

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
/// A phrase rather than a digest, because this path is quoted back to a model
/// on every mention and a base58 digest costs several times what three words
/// do. It carries no name the remote host supplied: that value is declared by
/// the far side, reaches the creating command unquoted, and for a sandbox is an
/// instance id no reader recognizes anyway. Lowercase words and hyphens leave
/// nothing in the name that command can read as an option, a path separator or
/// a shell metacharacter.
pub fn remote_scratch_id(binding: &SessionWorkspaceBinding) -> String {
    derived_phrase(REMOTE_SCRATCH_DOMAIN, &remote_identity(binding))
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

fn root_basename(root: &Path) -> &str {
    root.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("root")
}

/// Readable prefix plus a hash: two checkouts of the same repo need different
/// directories, and the user still wants to recognize theirs on disk.
///
/// This names the project's *state*, which holds memories and plans, so the
/// hash is pinned by [`fnv1a_64`] and cannot be restyled. Scratch parted ways
/// with it and is named by [`project_scratch_name`].
pub fn project_id(root: &Path) -> String {
    named_id(root_basename(root), root)
}

fn named_id(name: &str, root: &Path) -> String {
    format!("{name}-{}", fnv1a_64(&root.to_string_lossy()))
}

/// Names the store of change records kept for the directory `cwd` resolves
/// to: the SHA-256 of its canonical path, in lowercase hex. The store is found
/// by this name alone, so the digest is as pinned as [`fnv1a_64`].
pub fn workspace_key(cwd: &Path) -> io::Result<String> {
    let root = fs::canonicalize(cwd)?;
    if !root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            root.display().to_string(),
        ));
    }
    let mut hasher = Sha256::new();
    #[cfg(unix)]
    hasher.update(root.as_os_str().as_bytes());
    #[cfg(windows)]
    for unit in root.as_os_str().encode_wide() {
        hasher.update(unit.to_le_bytes());
    }
    #[cfg(not(any(unix, windows)))]
    hasher.update(root.to_string_lossy().as_bytes());
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// The same readable prefix, then a phrase in place of the hash.
///
/// Scratch is the one project directory a model is told about and asked to
/// approve, so its name is paid for in tokens every time it comes up, and
/// sixteen hex characters buy nothing a reader can hold on to. Derived rather
/// than drawn at random: a project must land on one directory on every run, or
/// each launch strands the last one in the temp root.
pub fn project_scratch_name(root: &Path) -> String {
    format!(
        "{}-{}",
        root_basename(root),
        derived_phrase(PROJECT_SCRATCH_DOMAIN, root.to_string_lossy().as_bytes())
    )
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

/// The root `cwd`'s project state is keyed on: a verified linked worktree
/// shares its main checkout's, and anything else keeps its [`project_root`].
pub fn state_root(cwd: &Path) -> PathBuf {
    linked_checkout(cwd).map_or_else(|| project_root(cwd), |checkout| checkout.main_root)
}

/// The state-directory-relative home for everything scoped to `cwd`'s project.
pub fn project_subdir(cwd: &Path) -> PathBuf {
    Path::new(PROJECTS_DIR).join(state_id(cwd))
}

fn state_id(cwd: &Path) -> String {
    linked_checkout(cwd).map_or_else(
        || project_id(&project_root(cwd)),
        |checkout| shared_id(&checkout),
    )
}

fn linked_checkout(cwd: &Path) -> Option<Checkout> {
    checkout::discover(cwd).filter(Checkout::is_linked)
}

/// The main checkout's id. A bare repository is named for the repository plus
/// `.git`, which names nothing a reader looks for, so its id drops the suffix.
fn shared_id(checkout: &Checkout) -> String {
    let name = root_basename(&checkout.main_root);
    let name = match name.strip_suffix(GIT_MARKER) {
        Some(repository) if checkout.is_bare() => repository,
        _ => name,
    };
    named_id(name, &checkout.main_root)
}

/// A project's own corner of the scratch root, keyed on its checkout rather
/// than on the root its state is shared under, so two checkouts writing the
/// same filename do not collide, linked worktrees of one repository included.
///
/// The scratch root holds nothing but these, so they sit directly inside it
/// rather than under a `projects` level. Keying on the repository rather than
/// `cwd` means a session started in a subdirectory shares the repository's
/// scratch, which is the same reason [`project_root`] exists.
pub fn project_scratch_dir(cwd: &Path) -> Result<PathBuf, std::io::Error> {
    let root = crate::paths::scratch_root()?;
    ensure_private_dir(&root.join(project_scratch_name(&project_root(cwd))))
}

#[derive(Clone, PartialEq, Eq)]
pub struct LocalProjectAliases {
    project_key: ProjectKey,
    legacy_id: LegacyLocalProjectId,
    keyed_subdir: PathBuf,
    legacy_subdir: PathBuf,
    /// A linked worktree's own directories from before it shared its main
    /// checkout's: still read, never written.
    fallback_subdirs: Vec<PathBuf>,
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

    fn subdir(&self) -> PathBuf {
        Path::new(PROJECTS_DIR).join(&self.0)
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
    /// A verified linked worktree writes where its main checkout does, keyed
    /// as the main checkout's own local binding would be, and still reads what
    /// it kept before. `project_key` stays the key documents are checked
    /// against either way.
    pub fn new(cwd: &Path, project_key: ProjectKey) -> Self {
        let own_keyed = keyed_project_subdir(&project_key);
        let own_legacy = LegacyLocalProjectId::from_root(&project_root(cwd));
        let (legacy_id, keyed_subdir, mut fallback_subdirs) = match linked_checkout(cwd) {
            Some(checkout) => (
                LegacyLocalProjectId(shared_id(&checkout)),
                keyed_project_subdir(&local_project_key(&checkout.main_root.to_string_lossy())),
                vec![own_keyed, own_legacy.subdir()],
            ),
            None => (own_legacy, own_keyed, Vec::new()),
        };
        let legacy_subdir = legacy_id.subdir();
        fallback_subdirs.retain(|subdir| *subdir != keyed_subdir && *subdir != legacy_subdir);
        Self {
            project_key,
            legacy_id,
            keyed_subdir,
            legacy_subdir,
            fallback_subdirs,
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
        let mut subdirs = self.write_subdirs();
        subdirs.extend(self.fallback_subdirs.iter().map(PathBuf::as_path));
        subdirs
    }

    /// The existing directory writes go to. Fallbacks are never one.
    pub fn resolve_existing(&self, state_dir: &StateDir) -> Option<PathBuf> {
        self.write_subdirs()
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

    fn write_subdirs(&self) -> Vec<&Path> {
        if self.keyed_subdir == self.legacy_subdir {
            vec![&self.keyed_subdir]
        } else {
            vec![&self.keyed_subdir, &self.legacy_subdir]
        }
    }
}

fn keyed_project_subdir(project_key: &ProjectKey) -> PathBuf {
    Path::new(PROJECTS_DIR).join(format!(
        "{KEYED_DIRECTORY_PREFIX}{}",
        opaque_hash(KEYED_DIRECTORY_DOMAIN, project_key.as_str().as_bytes())
    ))
}

/// What a linked worktree's one-time import brought into its repository's
/// shared state.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AdoptReport {
    pub copied: usize,
    /// Copied as `<name>.from-<branch>` beside a different file of their name.
    pub renamed: usize,
    /// Already there with the same content.
    pub identical: usize,
}

/// Copies the notes and plans a verified linked worktree kept on its own,
/// before it shared its main checkout's state, into that shared directory.
///
/// Nothing is overwritten or removed, so plan paths old sessions recorded stay
/// valid: a name already taken by different content is kept beside it as
/// `<name>.from-<branch>`. A marker left in each imported directory makes this
/// happen once.
pub fn adopt_checkout_state(state_dir: &StateDir, cwd: &Path) -> Result<AdoptReport, StorageError> {
    let Some(checkout) = linked_checkout(cwd) else {
        return Ok(AdoptReport::default());
    };
    let shared = Path::new(PROJECTS_DIR).join(shared_id(&checkout));
    let own_root = project_root(cwd);
    let mut import = Import {
        persistent: state_dir.persistent_path(),
        label: import_label(
            checkout
                .branch
                .as_deref()
                .or(checkout.linked.as_deref())
                .unwrap_or_default(),
        ),
        report: AdoptReport::default(),
    };
    let mut imported = Vec::new();
    for own in [
        LegacyLocalProjectId::from_root(&own_root).subdir(),
        keyed_project_subdir(&local_project_key(&own_root.to_string_lossy())),
    ] {
        let source = import.persistent.join(&own);
        if own == shared || !source.is_dir() || source.join(ADOPTED_MARKER).exists() {
            continue;
        }
        for kind in [MEMORIES_DIR, PLANS_DIR] {
            import.tree(&source.join(kind), &shared.join(kind))?;
        }
        atomic_write(
            &source.join(ADOPTED_MARKER),
            shared.to_string_lossy().as_bytes(),
        )?;
        imported.push(source);
    }
    if !imported.is_empty() {
        tracing::info!(
            cwd = %cwd.display(),
            shared = %import.persistent.join(&shared).display(),
            sources = ?imported,
            copied = import.report.copied,
            renamed = import.report.renamed,
            identical = import.report.identical,
            "imported a linked worktree's own notes and plans into its repository's shared state"
        );
    }
    Ok(import.report)
}

struct Import<'a> {
    persistent: &'a Path,
    /// Names where a renamed copy came from: `<name>.from-<label>`.
    label: String,
    report: AdoptReport,
}

impl Import<'_> {
    /// Every regular file under `source` to the same place under `target`,
    /// which is relative to the persistent root. Links are skipped, not
    /// followed.
    fn tree(&mut self, source: &Path, target: &Path) -> Result<(), StorageError> {
        if !fs::symlink_metadata(source).is_ok_and(|metadata| metadata.is_dir()) {
            return Ok(());
        }
        let mut pending = vec![PathBuf::new()];
        while let Some(relative) = pending.pop() {
            for entry in fs::read_dir(source.join(&relative))? {
                let entry = entry?;
                let file_type = entry.file_type()?;
                if file_type.is_dir() {
                    pending.push(relative.join(entry.file_name()));
                } else if file_type.is_file() {
                    self.file(&entry.path(), &target.join(&relative), &entry.file_name())?;
                }
            }
        }
        Ok(())
    }

    /// Takes `name` without clobbering it, and when it holds something else
    /// moves on to the next `.from-<label>` spelling.
    fn file(&mut self, source: &Path, target: &Path, name: &OsStr) -> Result<(), StorageError> {
        let content = fs::read(source)?;
        let directory = ensure_private_tree(self.persistent, target)?;
        for attempt in 0..=MAX_IMPORT_RENAMES {
            let path = directory.join(import_name(name, &self.label, attempt));
            match write_new(&directory, &path, &content) {
                Ok(()) if attempt == 0 => {
                    self.report.copied += 1;
                    return Ok(());
                }
                Ok(()) => {
                    self.report.renamed += 1;
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    if fs::read(&path).is_ok_and(|existing| existing == content) {
                        self.report.identical += 1;
                        return Ok(());
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        tracing::warn!(
            source = %source.display(),
            directory = %directory.display(),
            attempts = MAX_IMPORT_RENAMES,
            "every import name is taken; the file stays only where it was"
        );
        Ok(())
    }
}

/// A branch such as `feature/login` spelled for a file name: `feature-login`.
fn import_label(branch: &str) -> String {
    branch
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

/// `name` itself first, then `<stem>.from-<label>.<ext>`, then numbered.
fn import_name(name: &OsStr, label: &str, attempt: usize) -> OsString {
    if attempt == 0 {
        return name.to_os_string();
    }
    let path = Path::new(name);
    let mut renamed = path.file_stem().unwrap_or(name).to_os_string();
    renamed.push(IMPORTED_FROM);
    renamed.push(label);
    if attempt > 1 {
        renamed.push(format!("-{attempt}"));
    }
    if let Some(extension) = path.extension() {
        renamed.push(".");
        renamed.push(extension);
    }
    renamed
}

/// Owner-only, one directory at a time, refusing any that is a link.
fn ensure_private_tree(base: &Path, relative: &Path) -> io::Result<PathBuf> {
    relative
        .components()
        .try_fold(base.to_path_buf(), |directory, component| {
            ensure_private_dir(&directory.join(component))
        })
}

/// Publishes `content` at `path` atomically, failing with `AlreadyExists`
/// rather than replacing what is there.
fn write_new(directory: &Path, path: &Path, content: &[u8]) -> io::Result<()> {
    let mut staged = NamedTempFile::new_in(directory)?;
    staged.write_all(content)?;
    staged.as_file().sync_data()?;
    staged.persist_noclobber(path)?;
    sync_parent_dir(path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkout::fixture::{
        ADMIN, BRANCH, Links, MAIN_BRANCH, canonical_tempdir, git_dir, linked_pair,
        linked_worktree, main_checkout,
    };
    use test_case::test_case;

    const BARE_REPOSITORY: &str = "app.git";
    const BARE_NAME: &str = "app";
    const PROJECT_KEY: &str = "authority-project";
    const NEW_NOTE: &str = "new.md";
    const NESTED_NOTE: &str = "nested/deep.md";
    const SAME_NOTE: &str = "same.md";
    const CLASHING_NOTE: &str = "clash.md";
    const RENAMED_NOTE: &str = "clash.from-feature-login.md";
    const PLAN: &str = "quiet-river.md";
    const NEW_CONTENT: &str = "only in the worktree";
    const SAME_CONTENT: &str = "written in both";
    const OWN_CONTENT: &str = "the worktree's version";
    const SHARED_CONTENT: &str = "the main checkout's version";

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

    /// Pinned, not computed: the name of an existing store of change records.
    #[cfg(unix)]
    #[test]
    fn a_workspace_key_is_the_digest_of_the_canonical_root() {
        const ROOT_DIGEST: &str =
            "8a5edab282632443219e051e4ade2d1d5bbc671c781051bf1437897cbdfea0f1";
        assert_eq!(workspace_key(Path::new("/")).unwrap(), ROOT_DIGEST);
    }

    #[test]
    fn workspace_keys_are_canonical_and_distinguish_roots() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let other = temp.path().join("other");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&other).unwrap();

        assert_eq!(
            workspace_key(&root).unwrap(),
            workspace_key(&root.join(".")).unwrap()
        );
        assert_ne!(
            workspace_key(&root).unwrap(),
            workspace_key(&other).unwrap()
        );
    }

    #[test]
    fn a_file_has_no_workspace_key() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join(NEW_NOTE);
        std::fs::write(&file, NEW_CONTENT).unwrap();

        assert_eq!(
            workspace_key(&file).unwrap_err().kind(),
            io::ErrorKind::NotADirectory
        );
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

    /// Scratch keeps the readable prefix and drops the hash, so it names the
    /// checkout without costing what sixteen hex characters cost to quote.
    #[test]
    fn a_scratch_name_is_the_directory_name_and_a_phrase() {
        let root = Path::new("/home/user/app");
        let name = project_scratch_name(root);

        assert!(name.starts_with("app-"), "{name}");
        assert_eq!(name.split('-').count(), 4, "{name}");
        assert_ne!(name, project_id(root));
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

    fn own_state(state_dir: &StateDir, checkout: &Path) -> PathBuf {
        state_dir
            .persistent_path()
            .join(PROJECTS_DIR)
            .join(project_id(checkout))
    }

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    #[test]
    fn a_linked_worktree_shares_its_main_checkouts_state_directory() {
        let (_temp, base) = canonical_tempdir();
        let (main, worktree) = linked_pair(&base);
        let nested = worktree.join("src");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(state_root(&nested), main);
        assert_eq!(project_subdir(&nested), project_subdir(&main));
        assert_eq!(
            project_subdir(&main),
            Path::new(PROJECTS_DIR).join(project_id(&main))
        );
    }

    #[test]
    fn scratch_stays_with_each_checkout() {
        let (_temp, base) = canonical_tempdir();
        let (main, worktree) = linked_pair(&base);

        let scratch = [&main, &worktree].map(|cwd| project_scratch_dir(cwd).unwrap());

        assert_ne!(scratch[0], scratch[1]);
        for dir in scratch {
            fs::remove_dir(dir).unwrap();
        }
    }

    #[test]
    fn a_bare_repository_names_shared_state_without_its_git_suffix() {
        let (_temp, base) = canonical_tempdir();
        let common = base.join(BARE_REPOSITORY);
        git_dir(&common, MAIN_BRANCH);
        let worktree = base.join(ADMIN);
        linked_worktree(&common, ADMIN, &worktree, BRANCH, Links::Absolute);

        assert_eq!(
            project_subdir(&worktree),
            Path::new(PROJECTS_DIR).join(format!(
                "{BARE_NAME}-{}",
                fnv1a_64(&common.to_string_lossy())
            ))
        );
    }

    /// Only a bare repository drops the suffix: a checkout that happens to be
    /// named like one keeps the id its state already lives under.
    #[test]
    fn a_checkout_named_like_a_bare_repository_keeps_its_id() {
        let (_temp, base) = canonical_tempdir();
        let root = base.join(BARE_REPOSITORY);
        main_checkout(&root);

        assert!(project_id(&root).starts_with(BARE_REPOSITORY));
        assert_eq!(
            project_subdir(&root),
            Path::new(PROJECTS_DIR).join(project_id(&root))
        );
    }

    #[test]
    fn a_linked_worktree_writes_with_its_main_checkout_and_still_reads_its_own_state() {
        let (_temp, base) = canonical_tempdir();
        let (main, worktree) = linked_pair(&base);
        let state_dir = StateDir::from_path(base.join("state"));
        let key = ProjectKey::new(PROJECT_KEY).unwrap();
        let aliases = LocalProjectAliases::new(&worktree, key.clone());
        let main_aliases =
            LocalProjectAliases::new(&main, local_project_key(&main.to_string_lossy()));
        let own_keyed = keyed_project_subdir(&key);
        let own_legacy = Path::new(PROJECTS_DIR).join(project_id(&worktree));
        fs::create_dir_all(own_state(&state_dir, &worktree)).unwrap();

        assert_eq!(aliases.project_key(), &key);
        assert_eq!(aliases.legacy_subdir(), project_subdir(&worktree));
        assert_eq!(
            aliases.read_subdirs(),
            vec![
                main_aliases.keyed_subdir(),
                main_aliases.legacy_subdir(),
                own_keyed.as_path(),
                own_legacy.as_path(),
            ]
        );
        assert_eq!(
            aliases.ensure_write_subdir(&state_dir).unwrap(),
            state_dir
                .persistent_path()
                .join(main_aliases.keyed_subdir())
        );
    }

    #[test]
    fn adoption_copies_keeps_both_on_a_clash_and_leaves_the_source() {
        let (_temp, base) = canonical_tempdir();
        let (main, worktree) = linked_pair(&base);
        let state_dir = StateDir::from_path(base.join("state"));
        let own = own_state(&state_dir, &worktree);
        let shared = state_dir.persistent_path().join(project_subdir(&main));
        let memories = Path::new(MEMORIES_DIR);
        let plan = Path::new(PLANS_DIR).join(PLAN);
        for (name, content) in [
            (memories.join(NEW_NOTE), NEW_CONTENT),
            (memories.join(NESTED_NOTE), NEW_CONTENT),
            (memories.join(SAME_NOTE), SAME_CONTENT),
            (memories.join(CLASHING_NOTE), OWN_CONTENT),
            (plan.clone(), NEW_CONTENT),
        ] {
            write(&own.join(name), content);
        }
        write(&shared.join(memories).join(SAME_NOTE), SAME_CONTENT);
        write(&shared.join(memories).join(CLASHING_NOTE), SHARED_CONTENT);

        let report = adopt_checkout_state(&state_dir, &worktree).unwrap();

        assert_eq!(
            report,
            AdoptReport {
                copied: 3,
                renamed: 1,
                identical: 1,
            }
        );
        for name in [memories.join(NEW_NOTE), memories.join(NESTED_NOTE), plan] {
            assert_eq!(read(&shared.join(name)), NEW_CONTENT);
        }
        assert_eq!(
            read(&shared.join(memories).join(CLASHING_NOTE)),
            SHARED_CONTENT
        );
        assert_eq!(read(&shared.join(memories).join(RENAMED_NOTE)), OWN_CONTENT);
        assert_eq!(read(&own.join(memories).join(CLASHING_NOTE)), OWN_CONTENT);
        assert_eq!(
            read(&own.join(ADOPTED_MARKER)),
            project_subdir(&main).to_string_lossy()
        );
    }

    #[test]
    fn adoption_happens_once_so_deleted_notes_stay_deleted() {
        let (_temp, base) = canonical_tempdir();
        let (main, worktree) = linked_pair(&base);
        let state_dir = StateDir::from_path(base.join("state"));
        let note = Path::new(MEMORIES_DIR).join(NEW_NOTE);
        write(&own_state(&state_dir, &worktree).join(&note), NEW_CONTENT);
        let shared = state_dir
            .persistent_path()
            .join(project_subdir(&main))
            .join(&note);

        assert_eq!(
            adopt_checkout_state(&state_dir, &worktree).unwrap().copied,
            1
        );
        fs::remove_file(&shared).unwrap();

        assert_eq!(
            adopt_checkout_state(&state_dir, &worktree).unwrap(),
            AdoptReport::default()
        );
        assert!(!shared.exists());
    }

    #[test]
    fn adoption_leaves_a_main_checkout_alone() {
        let (_temp, base) = canonical_tempdir();
        let (main, _) = linked_pair(&base);
        let state_dir = StateDir::from_path(base.join("state"));
        let own = own_state(&state_dir, &main);
        write(&own.join(MEMORIES_DIR).join(NEW_NOTE), NEW_CONTENT);

        assert_eq!(
            adopt_checkout_state(&state_dir, &main).unwrap(),
            AdoptReport::default()
        );
        assert!(!own.join(ADOPTED_MARKER).exists());
    }
}
