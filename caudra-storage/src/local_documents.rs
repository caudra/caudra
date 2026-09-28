use std::collections::HashSet;
use std::fmt;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use caudra_workspace::{LocalDocumentRef, MemoryRef, PlanRef, ProjectKey, SessionWorkspaceBinding};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::projects::{DocumentProjectScope, LocalProjectAliases};
use crate::{StateDir, StorageError, atomic_write_permissions};

const PLANS_DIR: &str = "plans";
const PLAN_SESSIONS_DIR: &str = "sessions";
pub(crate) const MEMORIES_DIR: &str = "memories";
const MARKDOWN_EXTENSION: &str = "md";
const PLAN_REF_PREFIX: &str = "plan-";
const MEMORY_REF_PREFIX: &str = "memory-";
const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
const MAX_MEMORY_DOCUMENTS: usize = 1024;
const OWNER_ONLY_DIR_MODE: u32 = 0o700;
const OWNER_ONLY_FILE_MODE: u32 = 0o600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentRevision(String);

impl DocumentRevision {
    pub fn new(value: impl Into<String>) -> Result<Self, LocalDocumentError> {
        let value = value.into();
        if value.len() != Sha256::output_size() * 2
            || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(LocalDocumentError::InvalidReference);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDocument {
    pub reference: LocalDocumentRef,
    pub name: Option<String>,
    pub content: String,
    pub revision: DocumentRevision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchEdit {
    pub old: String,
    pub new: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LocalDocumentError {
    #[error("local document belongs to a different project")]
    WrongProject,
    #[error("a session identity is required for a plan document")]
    SessionRequired,
    #[error("local document does not belong to this project or session")]
    WrongOwner,
    #[error("invalid local document reference")]
    InvalidReference,
    #[error("invalid memory name")]
    InvalidMemoryName,
    #[error("local document is too large")]
    TooLarge,
    #[error("local document path is a symbolic link")]
    Symlink,
    #[error("local document changed since revision {expected}")]
    StaleRevision { expected: String },
    #[error("patch text was not found exactly once")]
    PatchConflict,
    #[error("local document storage failed: {0}")]
    Storage(#[from] StorageError),
    #[error("local document I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone)]
pub struct LocalDocumentStore {
    state_dir: StateDir,
    aliases: DocumentProjectScope,
}

impl fmt::Debug for LocalDocumentStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalDocumentStore")
            .field("project_key", self.aliases.project_key())
            .finish_non_exhaustive()
    }
}

impl LocalDocumentStore {
    pub fn local_with_legacy(
        state_dir: StateDir,
        legacy_cwd: &Path,
        project_key: ProjectKey,
    ) -> Self {
        Self {
            state_dir,
            aliases: DocumentProjectScope::Local(LocalProjectAliases::new(legacy_cwd, project_key)),
        }
    }

    pub fn remote(state_dir: StateDir, binding: &SessionWorkspaceBinding) -> Self {
        Self {
            state_dir,
            aliases: DocumentProjectScope::remote(binding),
        }
    }

    pub fn validate_binding(
        &self,
        binding: &SessionWorkspaceBinding,
    ) -> Result<(), LocalDocumentError> {
        if self.aliases.matches_remote(binding) {
            Ok(())
        } else {
            Err(LocalDocumentError::WrongOwner)
        }
    }

    pub fn project_key(&self) -> &ProjectKey {
        self.aliases.project_key()
    }

    pub fn create_plan(
        &self,
        project: &ProjectKey,
        session_id: &str,
    ) -> Result<PlanRef, LocalDocumentError> {
        self.validate_project(project)?;
        let dir = self.plan_write_dir(session_id)?;
        for _ in 0..10 {
            let reference = PlanRef::new(format!("{PLAN_REF_PREFIX}{}", random_id()))
                .map_err(|_| LocalDocumentError::InvalidReference)?;
            let path = document_path(&dir, reference.as_str())?;
            if !path.exists() {
                write_secure(&path, "")?;
                return Ok(reference);
            }
        }
        Err(StorageError::SlugCollision.into())
    }

    pub fn adopt_legacy_plan(
        &self,
        project: &ProjectKey,
        session_id: &str,
        legacy_path: &Path,
    ) -> Result<PlanRef, LocalDocumentError> {
        self.validate_project(project)?;
        if !matches!(self.aliases, DocumentProjectScope::Local(_)) {
            return Err(LocalDocumentError::WrongOwner);
        }
        let owned = self.aliases.read_subdirs().into_iter().any(|subdir| {
            let root = self
                .state_dir
                .persistent_path()
                .join(subdir)
                .join(PLANS_DIR);
            legacy_path.strip_prefix(&root).is_ok_and(|relative| {
                !relative.as_os_str().is_empty()
                    && relative
                        .components()
                        .all(|component| matches!(component, Component::Normal(_)))
            })
        });
        if !owned || !legacy_path.is_file() {
            return Err(LocalDocumentError::WrongOwner);
        }
        let content = read_secure(legacy_path)?;
        let reference = self.create_plan(project, session_id)?;
        self.write(
            project,
            Some(session_id),
            &LocalDocumentRef::Plan(reference.clone()),
            &content,
        )?;
        Ok(reference)
    }

    pub fn read(
        &self,
        project: &ProjectKey,
        session_id: Option<&str>,
        reference: &LocalDocumentRef,
    ) -> Result<LocalDocument, LocalDocumentError> {
        self.validate_project(project)?;
        let (path, name) = self.resolve(project, session_id, reference)?;
        let content = read_secure(&path)?;
        Ok(LocalDocument {
            reference: reference.clone(),
            name,
            revision: revision(&content),
            content,
        })
    }

    pub fn write(
        &self,
        project: &ProjectKey,
        session_id: Option<&str>,
        reference: &LocalDocumentRef,
        content: &str,
    ) -> Result<DocumentRevision, LocalDocumentError> {
        self.validate_project(project)?;
        validate_size(content)?;
        let (path, _) = self.resolve(project, session_id, reference)?;
        write_secure(&path, content)?;
        Ok(revision(content))
    }

    pub fn apply_patch(
        &self,
        project: &ProjectKey,
        session_id: Option<&str>,
        reference: &LocalDocumentRef,
        expected_revision: &DocumentRevision,
        edits: &[PatchEdit],
    ) -> Result<DocumentRevision, LocalDocumentError> {
        self.rewrite(
            project,
            session_id,
            reference,
            expected_revision,
            |mut content| {
                for edit in edits {
                    if edit.old.is_empty() || content.match_indices(&edit.old).count() != 1 {
                        return Err(LocalDocumentError::PatchConflict);
                    }
                    content = content.replacen(&edit.old, &edit.new, 1);
                }
                Ok(content)
            },
        )
    }

    /// Writes `content` over the whole document, provided nothing else wrote
    /// it since `expected_revision`, so a reader's save never undoes a change
    /// they did not see.
    pub fn replace(
        &self,
        project: &ProjectKey,
        session_id: Option<&str>,
        reference: &LocalDocumentRef,
        expected_revision: &DocumentRevision,
        content: &str,
    ) -> Result<DocumentRevision, LocalDocumentError> {
        self.rewrite(project, session_id, reference, expected_revision, |_| {
            Ok(content.to_owned())
        })
    }

    fn rewrite(
        &self,
        project: &ProjectKey,
        session_id: Option<&str>,
        reference: &LocalDocumentRef,
        expected_revision: &DocumentRevision,
        edit: impl FnOnce(String) -> Result<String, LocalDocumentError>,
    ) -> Result<DocumentRevision, LocalDocumentError> {
        self.validate_project(project)?;
        let (path, _) = self.resolve(project, session_id, reference)?;
        let current = read_secure(&path)?;
        if revision(&current) != *expected_revision {
            return Err(LocalDocumentError::StaleRevision {
                expected: expected_revision.as_str().to_owned(),
            });
        }
        let content = edit(current)?;
        validate_size(&content)?;
        write_secure(&path, &content)?;
        Ok(revision(&content))
    }

    pub fn list_memories(
        &self,
        project: &ProjectKey,
    ) -> Result<Vec<LocalDocument>, LocalDocumentError> {
        self.validate_project(project)?;
        let mut seen = HashSet::new();
        let mut documents = Vec::new();
        for root in self.memory_read_dirs() {
            if !root.exists() {
                continue;
            }
            secure_directory(&root)?;
            for (name, path) in memory_files(&root)? {
                if documents.len() >= MAX_MEMORY_DOCUMENTS || !seen.insert(name.clone()) {
                    continue;
                }
                let content = read_secure(&path)?;
                let reference = self.memory_ref(&name)?;
                documents.push(LocalDocument {
                    reference: LocalDocumentRef::Memory(reference),
                    name: Some(name),
                    revision: revision(&content),
                    content,
                });
            }
        }
        documents.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(documents)
    }

    pub fn write_memory(
        &self,
        project: &ProjectKey,
        name: &str,
        content: &str,
    ) -> Result<MemoryRef, LocalDocumentError> {
        self.validate_project(project)?;
        validate_size(content)?;
        let relative = memory_relative(name)?;
        let root = self.memory_write_dir()?;
        let path = secure_join(&root, &relative)?;
        ensure_secure_parent(&root, &path)?;
        write_secure(&path, content)?;
        self.memory_ref(&normalized_name(&relative))
    }

    pub fn delete_memory(
        &self,
        project: &ProjectKey,
        name: &str,
    ) -> Result<MemoryRef, LocalDocumentError> {
        self.validate_project(project)?;
        let relative = memory_relative(name)?;
        let normalized = normalized_name(&relative);
        let reference = self.memory_ref(&normalized)?;
        let path = self
            .memory_read_dirs()
            .into_iter()
            .map(|root| secure_join(&root, &relative).map(|path| (root, path)))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .find(|(_, path)| path.exists())
            .map(|(_, path)| path)
            .ok_or(LocalDocumentError::WrongOwner)?;
        ensure_no_symlink_ancestors(&path)?;
        fs::remove_file(path)?;
        Ok(reference)
    }

    pub fn delete_memory_ref(
        &self,
        project: &ProjectKey,
        reference: &MemoryRef,
    ) -> Result<(), LocalDocumentError> {
        self.validate_project(project)?;
        let (path, _) =
            self.resolve(project, None, &LocalDocumentRef::Memory(reference.clone()))?;
        ensure_no_symlink_ancestors(&path)?;
        fs::remove_file(path)?;
        Ok(())
    }

    fn resolve(
        &self,
        project: &ProjectKey,
        session_id: Option<&str>,
        reference: &LocalDocumentRef,
    ) -> Result<(PathBuf, Option<String>), LocalDocumentError> {
        match reference {
            LocalDocumentRef::Plan(reference) => {
                let session_id = session_id.ok_or(LocalDocumentError::SessionRequired)?;
                validate_ref(reference.as_str(), PLAN_REF_PREFIX)?;
                self.plan_read_dirs(session_id)
                    .into_iter()
                    .map(|dir| document_path(&dir, reference.as_str()))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .find(|path| path.is_file())
                    .map(|path| (path, None))
                    .ok_or(LocalDocumentError::WrongOwner)
            }
            LocalDocumentRef::Memory(reference) => {
                validate_ref(reference.as_str(), MEMORY_REF_PREFIX)?;
                self.list_memories(project)?
                    .into_iter()
                    .find(|document| {
                        matches!(
                            &document.reference,
                            LocalDocumentRef::Memory(candidate) if candidate == reference
                        )
                    })
                    .and_then(|document| {
                        let name = document.name?;
                        let relative = memory_relative(&name).ok()?;
                        self.memory_read_dirs()
                            .into_iter()
                            .find_map(|root| {
                                secure_join(&root, &relative)
                                    .ok()
                                    .filter(|path| path.is_file())
                            })
                            .map(|path| (path, Some(name)))
                    })
                    .ok_or(LocalDocumentError::WrongOwner)
            }
        }
    }

    fn validate_project(&self, project: &ProjectKey) -> Result<(), LocalDocumentError> {
        if project == self.aliases.project_key() {
            Ok(())
        } else {
            Err(LocalDocumentError::WrongProject)
        }
    }

    fn plan_read_dirs(&self, session_id: &str) -> Vec<PathBuf> {
        let session = session_directory(session_id);
        self.aliases
            .read_subdirs()
            .into_iter()
            .map(|subdir| {
                self.state_dir
                    .persistent_path()
                    .join(subdir)
                    .join(PLANS_DIR)
                    .join(PLAN_SESSIONS_DIR)
                    .join(&session)
            })
            .collect()
    }

    fn plan_write_dir(&self, session_id: &str) -> Result<PathBuf, LocalDocumentError> {
        let project = self.aliases.ensure_write_subdir(&self.state_dir)?;
        secure_directory(
            &project
                .join(PLANS_DIR)
                .join(PLAN_SESSIONS_DIR)
                .join(session_directory(session_id)),
        )
    }

    fn memory_read_dirs(&self) -> Vec<PathBuf> {
        self.aliases
            .read_subdirs()
            .into_iter()
            .map(|subdir| {
                self.state_dir
                    .persistent_path()
                    .join(subdir)
                    .join(MEMORIES_DIR)
            })
            .collect()
    }

    fn memory_write_dir(&self) -> Result<PathBuf, LocalDocumentError> {
        let project = self.aliases.ensure_write_subdir(&self.state_dir)?;
        secure_directory(&project.join(MEMORIES_DIR))
    }

    fn memory_ref(&self, name: &str) -> Result<MemoryRef, LocalDocumentError> {
        let mut hasher = Sha256::new();
        hasher.update(b"caudra-memory-ref\0");
        hasher.update(self.aliases.reference_namespace());
        hasher.update(b"\0");
        hasher.update(name.as_bytes());
        MemoryRef::new(format!(
            "{MEMORY_REF_PREFIX}{}",
            hex_digest(hasher.finalize().as_slice())
        ))
        .map_err(|_| LocalDocumentError::InvalidReference)
    }
}

fn random_id() -> String {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).expect("operating system randomness unavailable");
    hex_digest(&bytes)
}

fn session_directory(session_id: &str) -> String {
    let digest = Sha256::digest(session_id.as_bytes());
    format!("session-{}", hex_digest(digest.as_slice()))
}

fn revision(content: &str) -> DocumentRevision {
    DocumentRevision(hex_digest(Sha256::digest(content.as_bytes()).as_slice()))
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn validate_ref(reference: &str, prefix: &str) -> Result<(), LocalDocumentError> {
    let id = reference
        .strip_prefix(prefix)
        .ok_or(LocalDocumentError::InvalidReference)?;
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(LocalDocumentError::InvalidReference);
    }
    Ok(())
}

fn document_path(dir: &Path, reference: &str) -> Result<PathBuf, LocalDocumentError> {
    validate_ref(reference, PLAN_REF_PREFIX)?;
    Ok(dir.join(reference).with_extension(MARKDOWN_EXTENSION))
}

fn validate_size(content: &str) -> Result<(), LocalDocumentError> {
    if content.len() > MAX_DOCUMENT_BYTES {
        Err(LocalDocumentError::TooLarge)
    } else {
        Ok(())
    }
}

fn read_secure(path: &Path) -> Result<String, LocalDocumentError> {
    ensure_no_symlink_ancestors(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(OWNER_ONLY_FILE_MODE))?;
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_DOCUMENT_BYTES as u64 {
        return Err(LocalDocumentError::TooLarge);
    }
    let content = fs::read_to_string(path)?;
    validate_size(&content)?;
    Ok(content)
}

fn write_secure(path: &Path, content: &str) -> Result<(), LocalDocumentError> {
    validate_size(content)?;
    if path.exists() {
        ensure_not_symlink(path)?;
    }
    let parent = path.parent().ok_or(LocalDocumentError::WrongOwner)?;
    secure_directory(parent)?;
    atomic_write_permissions(path, content.as_bytes(), OWNER_ONLY_FILE_MODE)?;
    Ok(())
}

fn secure_directory(path: &Path) -> Result<PathBuf, LocalDocumentError> {
    ensure_no_symlink_ancestors(path)?;
    if path.exists() {
        ensure_not_symlink(path)?;
    } else {
        fs::create_dir_all(path)?;
    }
    ensure_no_symlink_ancestors(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(OWNER_ONLY_DIR_MODE))?;
    Ok(path.to_path_buf())
}

fn ensure_no_symlink_ancestors(path: &Path) -> Result<(), LocalDocumentError> {
    let ancestors = path.ancestors().collect::<Vec<_>>();
    for ancestor in ancestors.into_iter().rev().filter(|path| path.exists()) {
        ensure_not_symlink(ancestor)?;
    }
    Ok(())
}

fn ensure_not_symlink(path: &Path) -> Result<(), LocalDocumentError> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        Err(LocalDocumentError::Symlink)
    } else {
        Ok(())
    }
}

fn memory_relative(name: &str) -> Result<PathBuf, LocalDocumentError> {
    if name.is_empty() || name.contains('\0') || Path::new(name).is_absolute() {
        return Err(LocalDocumentError::InvalidMemoryName);
    }
    let mut relative = PathBuf::new();
    for component in Path::new(name).components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(LocalDocumentError::InvalidMemoryName);
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(LocalDocumentError::InvalidMemoryName);
    }
    Ok(relative)
}

fn normalized_name(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn secure_join(root: &Path, relative: &Path) -> Result<PathBuf, LocalDocumentError> {
    let path = root.join(relative);
    if path.starts_with(root) && path != root {
        Ok(path)
    } else {
        Err(LocalDocumentError::InvalidMemoryName)
    }
}

fn ensure_secure_parent(root: &Path, path: &Path) -> Result<(), LocalDocumentError> {
    let parent = path.parent().ok_or(LocalDocumentError::InvalidMemoryName)?;
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| LocalDocumentError::InvalidMemoryName)?;
    let mut current = root.to_path_buf();
    secure_directory(&current)?;
    for component in relative.components() {
        current.push(component);
        secure_directory(&current)?;
    }
    Ok(())
}

fn memory_files(root: &Path) -> Result<Vec<(String, PathBuf)>, LocalDocumentError> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        secure_directory(&directory)?;
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                return Err(LocalDocumentError::Symlink);
            }
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file()
                && path.extension().and_then(|extension| extension.to_str())
                    == Some(MARKDOWN_EXTENSION)
            {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| LocalDocumentError::WrongOwner)?;
                files.push((normalized_name(relative), path));
            }
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::{
        LocalDocumentError, LocalDocumentRef, LocalDocumentStore, LocalProjectAliases,
        MEMORIES_DIR, PatchEdit, ProjectKey, StateDir, fs,
    };
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, ProjectIdentity, SessionBindingId,
        SessionWorkspaceBinding, SourceTrustAnchor,
    };
    use test_case::test_case;

    const PROJECT: &str = "remote-project";
    const OTHER_PROJECT: &str = "other-project";
    const SESSION: &str = "session-a";
    const OTHER_SESSION: &str = "session-b";
    const NOTE: &str = "note.md";
    const CANARY: &str = "local legacy secret";
    const REMOTE_CONTENT: &str = "remote note";
    const REPLACED: &str = "a reader's edit";
    const NEWER: &str = "written in between";
    const WRONG_TEXT: &str = "the document holds the wrong text after a replace";
    const STALE_REPLACED: &str = "a replace went over a revision it never read";

    fn binding(fields: [&str; 7], session: &str) -> SessionWorkspaceBinding {
        let [
            anchor,
            server,
            workspace,
            generation,
            namespace,
            subject,
            project,
        ] = fields;
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new(anchor).expect("anchor"),
            server,
            workspace,
            generation,
            namespace,
        )
        .expect("authority");
        SessionWorkspaceBinding::new(
            SessionBindingId::new(session).expect("binding id"),
            authority.clone(),
            AuthenticatedPrincipalId::new(authority.clone(), subject).expect("principal"),
            ProjectIdentity::new(authority, ProjectKey::new(project).expect("project")),
        )
        .expect("consistent binding")
    }

    #[test_case(0; "source_anchor")]
    #[test_case(1; "server")]
    #[test_case(2; "workspace")]
    #[test_case(3; "generation")]
    #[test_case(4; "namespace_version")]
    #[test_case(5; "principal")]
    #[test_case(6; "project")]
    fn remote_documents_isolate_every_durable_identity_field(changed: usize) {
        let root = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(root.path().join("state"));
        let fields = [
            "origin",
            "server",
            "workspace",
            "generation",
            "namespace",
            "principal",
            PROJECT,
        ];
        let owner = binding(fields, SESSION);
        let store = LocalDocumentStore::remote(state.clone(), &owner);
        let project = owner.project().key();
        let memory = store
            .write_memory(project, NOTE, REMOTE_CONTENT)
            .expect("write memory");
        let plan = LocalDocumentRef::Plan(store.create_plan(project, SESSION).expect("plan"));
        let mut other_fields = fields;
        other_fields[changed] = "different";
        let other = binding(other_fields, SESSION);
        let foreign = LocalDocumentStore::remote(state.clone(), &other);
        let other_project = other.project().key();
        assert!(store.validate_binding(&other).is_err());
        assert!(
            foreign
                .list_memories(other_project)
                .expect("list")
                .is_empty()
        );
        let foreign_memory = foreign
            .write_memory(other_project, NOTE, CANARY)
            .expect("foreign memory");
        assert_ne!(memory, foreign_memory);
        let reference = LocalDocumentRef::Memory(memory.clone());
        for document in [&reference, &plan] {
            assert!(matches!(
                foreign.read(other_project, Some(SESSION), document),
                Err(LocalDocumentError::WrongOwner)
            ));
            assert!(matches!(
                foreign.write(other_project, Some(SESSION), document, REMOTE_CONTENT),
                Err(LocalDocumentError::WrongOwner)
            ));
        }
        assert!(matches!(
            foreign.delete_memory_ref(other_project, &memory),
            Err(LocalDocumentError::WrongOwner)
        ));
        assert_eq!(
            foreign.list_memories(other_project).expect("list")[0].content,
            CANARY
        );
        let reconnect = binding(fields, OTHER_SESSION);
        let reopened = LocalDocumentStore::remote(state, &reconnect);
        reopened.validate_binding(&owner).expect("durable scope");
        assert_eq!(
            reopened
                .read(project, None, &reference)
                .expect("reopen")
                .content,
            REMOTE_CONTENT
        );
        reopened
            .delete_memory_ref(project, &memory)
            .expect("owner delete");
        assert!(store.list_memories(project).expect("list").is_empty());
    }

    #[test_case((); "remote_never_reads_or_adopts_local_legacy")]
    fn remote_legacy_canary_is_inaccessible(_: ()) {
        let (root, local, project) = store();
        let legacy = local
            .state_dir
            .persistent_path()
            .join(LocalProjectAliases::new(root.path(), project.clone()).legacy_subdir());
        fs::create_dir_all(legacy.join(MEMORIES_DIR)).expect("legacy dir");
        fs::write(legacy.join(MEMORIES_DIR).join(NOTE), CANARY).expect("canary");
        fs::create_dir_all(legacy.join("plans")).expect("plans dir");
        let legacy_plan = legacy.join("plans/legacy.md");
        fs::write(&legacy_plan, CANARY).expect("plan canary");
        let owner = binding(
            [
                "origin",
                "server",
                "workspace",
                "generation",
                "namespace",
                "principal",
                PROJECT,
            ],
            SESSION,
        );
        let remote = LocalDocumentStore::remote(local.state_dir.clone(), &owner);
        assert!(local.validate_binding(&owner).is_err());
        let local_note = &local.list_memories(&project).expect("local list")[0];
        assert_eq!(local_note.content, CANARY);
        assert!(
            remote
                .list_memories(&project)
                .expect("remote list")
                .is_empty()
        );
        assert!(remote.read(&project, None, &local_note.reference).is_err());
        assert!(remote.delete_memory(&project, NOTE).is_err());
        assert!(
            remote
                .adopt_legacy_plan(&project, SESSION, &legacy_plan)
                .is_err()
        );
        local
            .adopt_legacy_plan(&project, SESSION, &legacy_plan)
            .expect("explicit local migration");
        remote
            .write_memory(&project, NOTE, REMOTE_CONTENT)
            .expect("remote write");
        assert_eq!(
            fs::read_to_string(legacy.join(MEMORIES_DIR).join(NOTE)).expect("canary retained"),
            CANARY
        );
    }

    fn store() -> (tempfile::TempDir, LocalDocumentStore, ProjectKey) {
        let root = tempfile::tempdir().expect("tempdir");
        let project = ProjectKey::new(PROJECT).expect("project key");
        let store = LocalDocumentStore::local_with_legacy(
            StateDir::from_path(root.path().join("state")),
            root.path(),
            project.clone(),
        );
        (root, store, project)
    }

    #[test]
    fn plan_round_trip_patch_and_owner_checks() {
        let (_root, store, project) = store();
        let reference = store.create_plan(&project, SESSION).expect("create plan");
        let document = LocalDocumentRef::Plan(reference.clone());
        store
            .write(&project, Some(SESSION), &document, "one\ntwo\n")
            .expect("write plan");
        let first = store
            .read(&project, Some(SESSION), &document)
            .expect("read plan");
        let revision = store
            .apply_patch(
                &project,
                Some(SESSION),
                &document,
                &first.revision,
                &[PatchEdit {
                    old: "two".into(),
                    new: "three".into(),
                }],
            )
            .expect("patch plan");
        let updated = store
            .read(&project, Some(SESSION), &document)
            .expect("read patched plan");

        assert_eq!(updated.content, "one\nthree\n");
        assert_eq!(updated.revision, revision);
        assert!(matches!(
            store.read(&project, Some(OTHER_SESSION), &document),
            Err(LocalDocumentError::WrongOwner)
        ));
        let other = ProjectKey::new(OTHER_PROJECT).expect("project key");
        assert!(matches!(
            store.read(&other, Some(SESSION), &document),
            Err(LocalDocumentError::WrongProject)
        ));
    }

    #[test]
    fn patch_rejects_a_stale_revision() {
        let (_root, store, project) = store();
        let reference =
            LocalDocumentRef::Plan(store.create_plan(&project, SESSION).expect("create plan"));
        let stale = store
            .read(&project, Some(SESSION), &reference)
            .expect("initial read")
            .revision;
        store
            .write(&project, Some(SESSION), &reference, "new")
            .expect("write plan");

        assert!(matches!(
            store.apply_patch(
                &project,
                Some(SESSION),
                &reference,
                &stale,
                &[PatchEdit {
                    old: "new".into(),
                    new: "newer".into(),
                }]
            ),
            Err(LocalDocumentError::StaleRevision { .. })
        ));
    }

    /// A reader's save names the revision their text came from, and one that
    /// is no longer current would undo whatever replaced it.
    #[test_case(false ; "a current revision is written over")]
    #[test_case(true ; "a stale revision leaves the newer text")]
    fn replace_writes_only_over_the_revision_it_names(stale: bool) {
        let (_root, store, project) = store();
        let reference =
            LocalDocumentRef::Plan(store.create_plan(&project, SESSION).expect("create plan"));
        let read = store
            .read(&project, Some(SESSION), &reference)
            .expect("initial read")
            .revision;
        if stale {
            store
                .write(&project, Some(SESSION), &reference, NEWER)
                .expect("write in between");
        }

        let replaced = store.replace(&project, Some(SESSION), &reference, &read, REPLACED);

        let current = store
            .read(&project, Some(SESSION), &reference)
            .expect("read back");
        match stale {
            true => {
                assert!(
                    matches!(replaced, Err(LocalDocumentError::StaleRevision { .. })),
                    "{STALE_REPLACED}"
                );
                assert_eq!(current.content, NEWER, "{WRONG_TEXT}");
            }
            false => {
                assert_eq!(replaced.ok(), Some(current.revision), "{WRONG_TEXT}");
                assert_eq!(current.content, REPLACED, "{WRONG_TEXT}");
            }
        }
    }

    #[test]
    fn memories_are_project_isolated_and_dual_read_the_legacy_alias() {
        let (root, store, project) = store();
        let legacy = store
            .state_dir
            .persistent_path()
            .join(LocalProjectAliases::new(root.path(), project.clone()).legacy_subdir())
            .join(MEMORIES_DIR);
        fs::create_dir_all(&legacy).expect("legacy memories");
        fs::write(legacy.join("legacy.md"), "kept").expect("legacy note");

        let documents = store.list_memories(&project).expect("list memories");
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].content, "kept");
        assert_eq!(documents[0].name.as_deref(), Some("legacy.md"));
        assert_eq!(
            store
                .read(&project, None, &documents[0].reference)
                .expect("read legacy memory")
                .content,
            "kept"
        );
        let other = ProjectKey::new(OTHER_PROJECT).expect("project key");
        assert!(matches!(
            store.list_memories(&other),
            Err(LocalDocumentError::WrongProject)
        ));
        drop(root);
    }

    #[test]
    fn traversal_and_symlinks_are_rejected() {
        let (root, store, project) = store();
        assert!(matches!(
            store.write_memory(&project, "../escape.md", "no"),
            Err(LocalDocumentError::InvalidMemoryName)
        ));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let memory_dir = store.memory_write_dir().expect("memory dir");
            symlink("outside", memory_dir.join("linked.md")).expect("symlink");
            assert!(matches!(
                store.list_memories(&project),
                Err(LocalDocumentError::Symlink)
            ));

            fs::remove_file(memory_dir.join("linked.md")).expect("remove symlink");
            let outside = root.path().join("outside");
            fs::create_dir(&outside).expect("outside dir");
            fs::write(outside.join("victim.md"), "keep").expect("outside file");
            symlink(&outside, memory_dir.join("linked")).expect("directory symlink");
            assert!(matches!(
                store.delete_memory(&project, "linked/victim.md"),
                Err(LocalDocumentError::Symlink)
            ));
            assert!(outside.join("victim.md").exists());
        }
    }
}
