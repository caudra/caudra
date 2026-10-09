//! A project's memory as one value: the notes on disk, and the journal and
//! summary tree the state database keeps over them.
//!
//! The files hold each note's current text, so editors, the workbench and
//! remote sessions keep working on them. The journal holds every version in
//! the order they were made. [`MemoryStore::reconcile`] brings changes made
//! outside the memory tool into the journal, and on a scope's first use it is
//! the import of the notes already there.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use caudra_storage::local_documents::{LocalDocumentError, LocalDocumentStore, MemoryFileStat};
use caudra_storage::memory_journal::{
    EntryKind, EntryMeta, EntryOrigin, ForgetReport, JournalChange, MAX_ENTRY_BODY_BYTES,
    MemoryJournal, MemoryJournalError, StoredNode, body_hash,
};
use caudra_storage::projects::project_subdir;
use caudra_storage::{StateDir, StorageError};
use jiff::Timestamp;
use thiserror::Error;
use tracing::{info, warn};

use super::search::{self, Document, Hit};
use super::tree::{self, Block, Leaf, LeafKind, Part, Tree, VIEW};

const MEMORIES_DIR: &str = "memories";
const NAME_SEPARATOR: &str = "/";
const HIDDEN_PREFIX: char = '.';
const MARKDOWN_SUFFIX: &str = ".md";
const FRONTMATTER_FENCE: &str = "---";
const FRONTMATTER_CLOSE: &str = "\n---";
pub const NAME_REQUIRED: &str = "path is required";
pub const NAME_MUST_BE_RELATIVE: &str = "path must be relative";
pub const NAME_TRAVERSAL: &str = "path traversal outside memories directory is not allowed";
pub const NAME_HIDDEN: &str = "path parts must not start with a dot";
pub const NAME_NOT_MARKDOWN: &str = "a remote session's notes must end in .md";

/// Every open store, by persistent state root and scope.
type Registry = HashMap<(PathBuf, String), Arc<MemoryStore>>;

static STORES: LazyLock<Mutex<Registry>> = LazyLock::new(Mutex::default);

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("memory state directory is unavailable: {0}")]
    State(#[from] StorageError),
    #[error(transparent)]
    Journal(#[from] MemoryJournalError),
    #[error(transparent)]
    Documents(#[from] LocalDocumentError),
    #[error("memory note access failed: {0}")]
    Io(#[from] io::Error),
    #[error("{reason}")]
    InvalidName { name: String, reason: &'static str },
    #[error("memory note {name} is {bytes} bytes, maximum is {MAX_ENTRY_BODY_BYTES}")]
    TooLarge { name: String, bytes: u64 },
}

/// Where a scope's note files live.
pub enum Source {
    /// A directory on this machine.
    Local(PathBuf),
    /// A remote session's notes, which the document store keeps on this
    /// machine.
    Documents(Arc<LocalDocumentStore>),
}

impl Source {
    /// Every note's name, size and modification time, or `None` when the
    /// notes directory does not exist.
    fn list(&self) -> Result<Option<Vec<MemoryFileStat>>, MemoryError> {
        match self {
            Self::Local(dir) => local_listing(dir),
            Self::Documents(store) => Ok(Some(store.memory_stats(store.project_key())?)),
        }
    }

    fn read(&self, name: &str) -> Result<String, MemoryError> {
        match self {
            Self::Local(dir) => Ok(fs::read_to_string(dir.join(name))?),
            Self::Documents(store) => Ok(store.read_memory(store.project_key(), name)?),
        }
    }

    fn admits(&self, name: &str) -> Result<(), MemoryError> {
        match self {
            Self::Documents(_) if !name.ends_with(MARKDOWN_SUFFIX) => {
                Err(MemoryError::InvalidName {
                    name: name.to_owned(),
                    reason: NAME_NOT_MARKDOWN,
                })
            }
            _ => Ok(()),
        }
    }

    fn write(&self, name: &str, content: &str) -> io::Result<()> {
        match self {
            Self::Local(dir) => {
                let path = dir.join(name);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(path, content)
            }
            Self::Documents(store) => store
                .write_memory(store.project_key(), name, content)
                .map(drop)
                .map_err(document_io),
        }
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        match self {
            Self::Local(dir) => fs::remove_file(dir.join(name)),
            Self::Documents(store) => store
                .delete_memory(store.project_key(), name)
                .map(drop)
                .map_err(document_io),
        }
    }
}

/// The journal and the tree as they stood at one moment.
pub struct MemoryState {
    pub entries: Vec<EntryMeta>,
    /// Every node row, built or only leased.
    pub nodes: Vec<StoredNode>,
    pub tree: Tree,
}

pub struct MemoryStore {
    scope: String,
    source: Source,
    journal: Mutex<MemoryJournal>,
    /// Each file's size and modification time when reconcile last looked, so
    /// a turn that changed nothing on disk only lists the directory.
    seen: Mutex<HashMap<String, (u64, i64)>>,
}

impl MemoryStore {
    pub fn open(state: &StateDir, scope: String, source: Source) -> Result<Self, MemoryError> {
        Ok(Self {
            scope,
            source,
            journal: Mutex::new(MemoryJournal::open(state)?),
            seen: Mutex::default(),
        })
    }

    /// The memory of the project at `cwd` kept under `state`, shared by
    /// everything in this process.
    pub fn for_cwd(state: &StateDir, cwd: &Path) -> Result<Arc<Self>, MemoryError> {
        let subdir = project_subdir(cwd);
        shared(state, joined(&subdir), || {
            Source::Local(notes_dir(state, &subdir))
        })
    }

    /// A remote session's memory, shared by everything in this process.
    pub fn for_documents(documents: &Arc<LocalDocumentStore>) -> Result<Arc<Self>, MemoryError> {
        shared(documents.state_dir(), documents.memory_scope()?, || {
            Source::Documents(Arc::clone(documents))
        })
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    pub fn source(&self) -> &Source {
        &self.source
    }

    /// The journal, for the reads and leases no method here wraps. Never
    /// held across an await.
    pub fn journal(&self) -> MutexGuard<'_, MemoryJournal> {
        lock(&self.journal)
    }

    /// Records the changes made to the files since the last look: new and
    /// edited notes, oldest first, and notes whose file is gone. When the
    /// notes directory itself is gone, so is the journal. Returns the entries
    /// appended.
    pub fn reconcile(&self) -> Result<Vec<u64>, MemoryError> {
        let mut seen = lock(&self.seen);
        let Some(listing) = self.source.list()? else {
            seen.clear();
            self.purge()?;
            return Ok(Vec::new());
        };
        if listing.len() == seen.len()
            && listing
                .iter()
                .all(|stat| seen.get(&stat.name) == Some(&stamp(stat)))
        {
            return Ok(Vec::new());
        }
        let journal = lock(&self.journal);
        let live = journal.current_hashes(&self.scope)?;
        let origin = match journal.len(&self.scope)? {
            0 => EntryOrigin::Import,
            _ => EntryOrigin::External,
        };
        let mut found: Vec<(i64, String, Option<String>)> = Vec::new();
        for stat in listing
            .iter()
            .filter(|stat| seen.get(&stat.name) != Some(&stamp(stat)))
        {
            match self.changed_body(stat, &live) {
                Ok(Some(body)) => found.push((stat.modified_ms, stat.name.clone(), Some(body))),
                Ok(None) => {}
                Err(error) => {
                    warn!(scope = %self.scope, note = %stat.name, %error, "memory note not recorded")
                }
            }
        }
        let present: HashSet<&str> = listing.iter().map(|stat| stat.name.as_str()).collect();
        let now = now_ms();
        found.extend(
            live.keys()
                .filter(|name| !present.contains(name.as_str()))
                .map(|name| (now, name.clone(), None)),
        );
        found.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        let changes: Vec<JournalChange> = found
            .into_iter()
            .map(|(created_ms, name, body)| match body {
                Some(body) => JournalChange::Note {
                    name,
                    body,
                    created_ms,
                },
                None => JournalChange::Delete { name, created_ms },
            })
            .collect();
        let appended = match changes.is_empty() {
            true => Vec::new(),
            false => journal.reconcile(&self.scope, &changes, &origin)?,
        };
        *seen = listing
            .iter()
            .map(|stat| (stat.name.clone(), stamp(stat)))
            .collect();
        Ok(appended)
    }

    /// Writes the note and records it as one step, which racing processes
    /// see whole. `None` when the note already said exactly this.
    pub fn write(
        &self,
        name: &str,
        content: &str,
        origin: &EntryOrigin,
    ) -> Result<Option<u64>, MemoryError> {
        let name = note_name(name)?;
        self.source.admits(&name)?;
        let change = JournalChange::Note {
            name: name.clone(),
            body: journal_body(content).to_owned(),
            created_ms: now_ms(),
        };
        Ok(self.journal().append(&self.scope, change, origin, || {
            self.source.write(&name, content)
        })?)
    }

    /// Deletes the note's file and records that. `None` when the journal
    /// held no live version of it.
    pub fn delete(&self, name: &str, origin: &EntryOrigin) -> Result<Option<u64>, MemoryError> {
        let name = note_name(name)?;
        let change = JournalChange::Delete {
            name: name.clone(),
            created_ms: now_ms(),
        };
        Ok(self
            .journal()
            .append(&self.scope, change, origin, || self.source.remove(&name))?)
    }

    /// Purges every version of the note, its file, and every summary that
    /// could have read it. Only the user does this, to take a secret out.
    pub fn forget(&self, name: &str) -> Result<ForgetReport, MemoryError> {
        let name = note_name(name)?;
        Ok(self
            .journal()
            .forget(&self.scope, &name, || match self.source.remove(&name) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                result => result,
            })?)
    }

    /// Entries, nodes and the tree, read in one transaction.
    pub fn load(&self) -> Result<MemoryState, MemoryError> {
        let (entries, mut bodies, nodes) = self.journal().read(|journal| {
            let entries = journal.entries(&self.scope, 0)?;
            let short: Vec<u64> = entries
                .iter()
                .filter(|meta| kept_whole(meta))
                .map(|meta| meta.seq)
                .collect();
            let bodies: HashMap<u64, String> = journal
                .bodies(&self.scope, &short)?
                .into_iter()
                .map(|entry| (entry.meta.seq, entry.body))
                .collect();
            Ok((entries, bodies, journal.nodes(&self.scope)?))
        })?;
        let leaves = entries
            .iter()
            .map(|meta| leaf(meta, bodies.remove(&meta.seq)))
            .collect();
        let stored = nodes.iter().filter_map(|node| {
            let part = Part {
                level: node.level,
                index: node.index,
            };
            Some((part, node.text.clone()?))
        });
        let tree = Tree::new(leaves, stored);
        Ok(MemoryState {
            entries,
            nodes,
            tree,
        })
    }

    /// The view as the system prompt carries it.
    pub fn view(&self) -> Result<Block, MemoryError> {
        let tree = self.load()?.tree;
        Ok(tree.block(&tree.fold(VIEW), VIEW))
    }

    /// The live notes that best match `query`, best first.
    pub fn search(&self, query: &str) -> Result<Vec<Hit>, MemoryError> {
        let current = self.journal().current(&self.scope)?;
        let documents = current.iter().map(|entry| Document {
            seq: entry.meta.seq,
            name: &entry.meta.name,
            heading: &entry.meta.heading,
            body: &entry.body,
        });
        Ok(search::search(&search::terms(query), documents))
    }

    fn changed_body(
        &self,
        stat: &MemoryFileStat,
        live: &HashMap<String, [u8; 32]>,
    ) -> Result<Option<String>, MemoryError> {
        if stat.size > MAX_ENTRY_BODY_BYTES as u64 {
            return Err(MemoryError::TooLarge {
                name: stat.name.clone(),
                bytes: stat.size,
            });
        }
        let content = self.source.read(&stat.name)?;
        let body = journal_body(&content);
        Ok((live.get(&stat.name) != Some(&body_hash(body))).then(|| body.to_owned()))
    }

    fn purge(&self) -> Result<(), MemoryError> {
        let journal = self.journal();
        if journal.len(&self.scope)? > 0 {
            let entries = journal.purge(&self.scope)?;
            info!(scope = %self.scope, entries, "memory notes directory is gone, so its journal was purged");
        }
        Ok(())
    }
}

/// The journal's name for the note at `path` in the notes directory: its
/// parts joined by `/`. Refuses anything that could leave the directory, and
/// hidden parts, which reconcile skips.
pub fn note_name(path: &str) -> Result<String, MemoryError> {
    let invalid = |reason| {
        Err(MemoryError::InvalidName {
            name: path.to_owned(),
            reason,
        })
    };
    let bytes = path.as_bytes();
    let drive_letter = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if path.contains('\0') || path.starts_with(['/', '\\']) || drive_letter {
        return invalid(NAME_MUST_BE_RELATIVE);
    }
    let mut parts = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_string_lossy();
                if part.starts_with(HIDDEN_PREFIX) {
                    return invalid(NAME_HIDDEN);
                }
                parts.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir => return invalid(NAME_TRAVERSAL),
            Component::RootDir | Component::Prefix(_) => return invalid(NAME_MUST_BE_RELATIVE),
        }
    }
    if parts.is_empty() {
        return invalid(NAME_REQUIRED);
    }
    Ok(parts.join(NAME_SEPARATOR))
}

/// A note as the journal keeps it: without the YAML frontmatter earlier
/// releases kept its tags in. A fence that never closes is text.
pub fn journal_body(content: &str) -> &str {
    let Some(rest) = content
        .trim_start()
        .strip_prefix(FRONTMATTER_FENCE)
        .and_then(|rest| {
            rest.strip_prefix('\n')
                .or_else(|| rest.strip_prefix("\r\n"))
        })
    else {
        return content;
    };
    match rest.find(FRONTMATTER_CLOSE) {
        Some(end) => rest[end + FRONTMATTER_CLOSE.len()..].trim_start_matches(['\n', '\r']),
        None => content,
    }
}

pub fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

/// The directory the notes of the project at `cwd` live in on this machine.
pub fn local_dir(cwd: &Path) -> Result<PathBuf, MemoryError> {
    Ok(notes_dir(&StateDir::resolve()?, &project_subdir(cwd)))
}

/// The name comes from [`project_subdir`], which plans share: any drift
/// silently orphans notes a user already wrote.
fn notes_dir(state: &StateDir, subdir: &Path) -> PathBuf {
    state.persistent_path().join(subdir).join(MEMORIES_DIR)
}

fn shared(
    state: &StateDir,
    scope: String,
    source: impl FnOnce() -> Source,
) -> Result<Arc<MemoryStore>, MemoryError> {
    let key = (state.persistent_path().to_path_buf(), scope);
    let mut stores = lock(&STORES);
    if let Some(store) = stores.get(&key) {
        return Ok(Arc::clone(store));
    }
    let store = Arc::new(MemoryStore::open(state, key.1.clone(), source())?);
    stores.insert(key, Arc::clone(&store));
    Ok(store)
}

/// Every regular file under `dir` that is not hidden, symlinks followed, as
/// the old tool accepted any name. `None` when `dir` does not exist.
pub(crate) fn local_listing(dir: &Path) -> Result<Option<Vec<MemoryFileStat>>, MemoryError> {
    let mut files = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let entries = match fs::read_dir(dir.join(&relative)) {
            Ok(entries) => entries,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound && relative.as_os_str().is_empty() =>
            {
                return Ok(None);
            }
            Err(error) if relative.as_os_str().is_empty() => return Err(error.into()),
            Err(error) => {
                warn!(directory = %relative.display(), %error, "memory notes directory skipped");
                continue;
            }
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with(HIDDEN_PREFIX) {
                continue;
            }
            let path = relative.join(&name);
            let Ok(metadata) = fs::metadata(entry.path()) else {
                continue;
            };
            if metadata.is_dir() && !entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
                pending.push(path);
            } else if metadata.is_file()
                && let Some(name) = path.to_str().map(|_| joined(&path))
            {
                files.push(MemoryFileStat {
                    name,
                    size: metadata.len(),
                    modified_ms: metadata
                        .modified()
                        .ok()
                        .and_then(|modified| Timestamp::try_from(modified).ok())
                        .map_or(0, |modified| modified.as_millisecond()),
                });
            }
        }
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Some(files))
}

fn joined(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(NAME_SEPARATOR)
}

fn stamp(stat: &MemoryFileStat) -> (u64, i64) {
    (stat.size, stat.modified_ms)
}

/// Whether the entry may be its own line, so the tree needs its body.
fn kept_whole(meta: &EntryMeta) -> bool {
    meta.kind == EntryKind::Note
        && !meta.forgotten
        && usize::try_from(meta.body_bytes).is_ok_and(|bytes| tree::fits(&meta.name, bytes))
}

pub fn leaf_kind(meta: &EntryMeta) -> LeafKind {
    match (meta.forgotten, meta.kind) {
        (true, _) => LeafKind::Forgotten,
        (false, EntryKind::Note) => LeafKind::Note,
        (false, EntryKind::Delete) => LeafKind::Delete,
    }
}

fn leaf(meta: &EntryMeta, body: Option<String>) -> Leaf {
    Leaf {
        kind: leaf_kind(meta),
        name: meta.name.clone(),
        heading: meta.heading.clone(),
        body,
    }
}

/// The document store names a missing note `WrongOwner`; forgetting a note
/// whose file is already gone has to tell that apart.
fn document_io(error: LocalDocumentError) -> io::Error {
    let kind = match error {
        LocalDocumentError::WrongOwner => io::ErrorKind::NotFound,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, error)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::time::{Duration, SystemTime};

    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const SCOPE: &str = "projects/test";
    const SESSION: &str = "session-a";
    const NOTE: &str = "a.md";
    const OTHER: &str = "b.md";
    const BODY: &str = "# Alpha\nfirst body\n";
    const EDITED: &str = "# Alpha\nedited body\n";
    const TAGGED: &str = "---\ntags: [gotchas]\n---\n# Alpha\nfirst body\n";

    struct Fixture {
        _state: TempDir,
        dir: PathBuf,
        store: MemoryStore,
    }

    fn fixture() -> Fixture {
        let state = TempDir::new().unwrap();
        let dir = state.path().join(MEMORIES_DIR);
        let store = MemoryStore::open(
            &StateDir::from_path(state.path().to_path_buf()),
            SCOPE.to_owned(),
            Source::Local(dir.clone()),
        )
        .unwrap();
        Fixture {
            _state: state,
            dir,
            store,
        }
    }

    impl Fixture {
        fn put(&self, name: &str, content: &str, age_secs: u64) {
            let path = self.dir.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(SystemTime::now() - Duration::from_secs(age_secs))
                .unwrap();
        }

        fn entries(&self) -> Vec<EntryMeta> {
            self.store.journal().entries(SCOPE, 0).unwrap()
        }

        fn names(&self) -> Vec<(EntryKind, String)> {
            self.entries()
                .into_iter()
                .map(|meta| (meta.kind, meta.name))
                .collect()
        }
    }

    fn session() -> EntryOrigin {
        EntryOrigin::Session(SESSION.to_owned())
    }

    #[test]
    fn import_takes_every_note_oldest_first_without_frontmatter() {
        let fixture = fixture();
        fixture.put(OTHER, BODY, 10);
        fixture.put(NOTE, TAGGED, 20);
        fixture.put("nested/c", BODY, 5);
        fixture.put(".hidden.md", BODY, 30);

        assert_eq!(fixture.store.reconcile().unwrap(), [0, 1, 2]);

        let entries = fixture.entries();
        let names: Vec<&str> = entries.iter().map(|meta| meta.name.as_str()).collect();
        assert_eq!(names, [NOTE, OTHER, "nested/c"]);
        assert!(
            entries
                .iter()
                .all(|meta| meta.origin == EntryOrigin::Import)
        );
        let first = fixture.store.journal().entry(SCOPE, 0).unwrap().unwrap();
        assert_eq!(first.body, BODY);
        assert_eq!(first.meta.heading, "Alpha");
    }

    #[test]
    fn reconcile_records_edits_and_deletions_as_external() {
        let fixture = fixture();
        fixture.put(NOTE, BODY, 20);
        fixture.put(OTHER, BODY, 20);
        fixture.store.reconcile().unwrap();

        fixture.put(NOTE, EDITED, 0);
        fs::remove_file(fixture.dir.join(OTHER)).unwrap();

        assert_eq!(fixture.store.reconcile().unwrap(), [2, 3]);
        assert_eq!(
            fixture.names()[2..],
            [
                (EntryKind::Note, NOTE.to_owned()),
                (EntryKind::Delete, OTHER.to_owned())
            ]
        );
        assert!(
            fixture.entries()[2..]
                .iter()
                .all(|meta| meta.origin == EntryOrigin::External)
        );
        assert!(fixture.store.reconcile().unwrap().is_empty());
    }

    #[test]
    fn reconcile_after_a_tool_write_appends_nothing() {
        let fixture = fixture();
        assert_eq!(
            fixture.store.write(NOTE, TAGGED, &session()).unwrap(),
            Some(0)
        );

        assert!(fixture.store.reconcile().unwrap().is_empty());
        assert_eq!(fs::read_to_string(fixture.dir.join(NOTE)).unwrap(), TAGGED);
        assert_eq!(fixture.entries()[0].origin, session());
    }

    #[test]
    fn rewriting_the_same_text_appends_nothing() {
        let fixture = fixture();
        fixture.store.write(NOTE, BODY, &session()).unwrap();

        assert_eq!(fixture.store.write(NOTE, BODY, &session()).unwrap(), None);
        assert_eq!(
            fixture.store.write(NOTE, EDITED, &session()).unwrap(),
            Some(1)
        );
    }

    #[test]
    fn delete_removes_the_file_and_records_it() {
        let fixture = fixture();
        fixture.store.write(NOTE, BODY, &session()).unwrap();

        assert_eq!(fixture.store.delete(NOTE, &session()).unwrap(), Some(1));
        assert!(!fixture.dir.join(NOTE).exists());
        assert!(fixture.store.delete(NOTE, &session()).is_err());
    }

    #[test]
    fn changes_outside_the_notes_directory_are_refused() {
        let fixture = fixture();
        let escape = "../escaped.md";

        assert!(matches!(
            fixture.store.write(escape, BODY, &session()),
            Err(MemoryError::InvalidName {
                reason: NAME_TRAVERSAL,
                ..
            })
        ));
        assert!(fixture.store.delete(escape, &session()).is_err());
        assert!(fixture.store.forget(escape).is_err());
        assert!(!fixture.dir.join(escape).exists());
    }

    #[test]
    fn a_missing_directory_purges_the_journal() {
        let fixture = fixture();
        fixture.store.write(NOTE, BODY, &session()).unwrap();

        fs::remove_dir_all(&fixture.dir).unwrap();
        fixture.store.reconcile().unwrap();

        assert_eq!(fixture.store.journal().len(SCOPE).unwrap(), 0);
    }

    #[test]
    fn forget_removes_the_file_and_blanks_every_version() {
        let fixture = fixture();
        fixture.store.write(NOTE, BODY, &session()).unwrap();
        fixture.store.write(NOTE, EDITED, &session()).unwrap();

        let report = fixture.store.forget(NOTE).unwrap();

        assert_eq!(report.entries, 2);
        assert!(!fixture.dir.join(NOTE).exists());
        assert!(fixture.store.reconcile().unwrap().is_empty());
        let tree = fixture.store.load().unwrap().tree;
        assert_eq!(tree.line(&Part::leaf(0)), format!("forgotten {NOTE}"));
    }

    #[test]
    fn short_notes_are_their_own_lines() {
        let fixture = fixture();
        fixture.store.write(NOTE, BODY, &session()).unwrap();
        fixture
            .store
            .write(OTHER, &"x".repeat(tree::NODE), &session())
            .unwrap();

        let view = fixture.store.view().unwrap();

        assert_eq!(
            view.lines[0].text,
            format!("note {NOTE} {}", BODY.replace('\n', " "))
        );
        assert!(view.lines[1].pending());
        assert_eq!(view.through(), 2);
    }

    #[test_case("a.md", Ok("a.md") ; "plain")]
    #[test_case("./dir/./a.md", Ok("dir/a.md") ; "current_dir_parts_dropped")]
    #[test_case("", Err(NAME_REQUIRED) ; "empty")]
    #[test_case(".", Err(NAME_REQUIRED) ; "only_current_dir")]
    #[test_case("/etc/passwd", Err(NAME_MUST_BE_RELATIVE) ; "absolute")]
    #[test_case("C:notes.md", Err(NAME_MUST_BE_RELATIVE) ; "drive_letter")]
    #[test_case("../a.md", Err(NAME_TRAVERSAL) ; "parent")]
    #[test_case("dir/.a.md", Err(NAME_HIDDEN) ; "hidden")]
    fn note_names(path: &str, expected: Result<&str, &str>) {
        let name = note_name(path).map_err(|error| match error {
            MemoryError::InvalidName { reason, .. } => reason,
            other => panic!("unexpected error {other}"),
        });
        assert_eq!(name, expected.map(str::to_owned));
    }

    #[test_case(BODY, BODY ; "no_frontmatter")]
    #[test_case(TAGGED, BODY ; "frontmatter")]
    #[test_case("---\nnever closed\n", "---\nnever closed\n" ; "unclosed_fence")]
    #[test_case("---\r\ntags: [a]\r\n---\r\nbody", "body" ; "crlf")]
    fn journal_bodies(content: &str, expected: &str) {
        assert_eq!(journal_body(content), expected);
    }
}
