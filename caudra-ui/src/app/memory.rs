//! `/memory`: the inspector's side of the conversation with the project's
//! memory. Every read and every change goes to the store from a task of its
//! own and lands on a later tick, so the loop never waits on the journal. The
//! notes belong to the `memory` tool and the summaries to the summarizer.

use std::path::PathBuf;
use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

use caudra_agent::memory::search::Hit;
use caudra_agent::memory::snapshot::MemorySnapshot;
use caudra_agent::memory::store::{MemoryStore, note_name};
use caudra_storage::StateDir;
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_storage::memory_journal::EntryOrigin;
use caudra_storage::projects::project_document_dirs;
use caudra_workspace::MemoryRef;
use tracing::warn;

use super::App;
use crate::agent::BtwPrompt;
use crate::components::document_view::COPIED_SELECTION;
use crate::components::memory_inspector::MemoryAction;
use crate::components::{Action, escape_terminal_controls};
use crate::repaint::{Cadence, Dirty};

const UNRESOLVED: &str = "Cannot resolve memory directory";
const NOTE_GONE: &str = "Note is already gone";
const DELETED: &str = "Deleted ";
const DELETE_FAILED: &str = "Delete failed: ";
const FORGOT: &str = "Forgot ";
const FORGET_FAILED: &str = "Forget failed: ";
#[cfg(test)]
const REPLY_WAIT: Duration = Duration::from_secs(30);
#[cfg(test)]
const REPLY_MISSING: &str = "the memory store must answer a request in flight";

/// Where the memory the inspector opened on lives. A remote session's is the
/// document store's copy and never a directory on this machine.
#[derive(Clone)]
enum MemoryPlace {
    Documents(Arc<LocalDocumentStore>),
    Project(StateDir, PathBuf),
}

impl MemoryPlace {
    fn open(&self) -> Result<Arc<MemoryStore>, String> {
        match self {
            Self::Documents(documents) => MemoryStore::for_documents(documents),
            Self::Project(state, cwd) => MemoryStore::for_cwd(state, cwd),
        }
        .map_err(|error| error.to_string())
    }
}

/// Where a note the outline names opens.
#[derive(Debug, PartialEq, Eq)]
enum NoteAt {
    Stored(MemoryRef),
    File(PathBuf),
}

/// What the store answered.
enum MemoryReply {
    Snapshot(Result<MemorySnapshot, String>),
    Body(u64, Result<Option<String>, String>),
    Hits(String, Result<Vec<Hit>, String>),
    /// A delete or a forget went to the journal; the words report how.
    Changed(String),
}

/// The channel answers come back through, drained each tick.
pub(crate) struct MemoryReads {
    tx: flume::Sender<MemoryReply>,
    rx: flume::Receiver<MemoryReply>,
    place: Option<MemoryPlace>,
    /// The journal changed since the inspector's last read.
    stale: bool,
}

impl Default for MemoryReads {
    fn default() -> Self {
        let (tx, rx) = flume::unbounded();
        Self {
            tx,
            rx,
            place: None,
            stale: false,
        }
    }
}

impl MemoryReads {
    /// Whether a request is in flight: its task holds a sender until it has answered.
    fn awaited(&self) -> bool {
        self.rx.sender_count() > 1
    }
}

impl App {
    pub(super) fn open_memory_inspector(&mut self) -> Vec<Action> {
        self.memory_reads.place = match &self.workspace_session {
            Some(_) => self
                .remote_document_store()
                .cloned()
                .map(MemoryPlace::Documents),
            None => Some(MemoryPlace::Project(
                self.storage.clone(),
                PathBuf::from(&self.state.session.cwd),
            )),
        };
        self.memory_reads.stale = false;
        let prompt = self.bound_prompt();
        self.memory_inspector.open(
            prompt.as_deref().map(|prompt| prompt.system.as_str()),
            self.state.session.id.to_string(),
            self.summarize_memory,
        );
        self.read_memory();
        Vec::new()
    }

    /// Where a remote workspace keeps its plans and notes, provided it still
    /// belongs to the workspace in front.
    pub(super) fn remote_document_store(&self) -> Option<&Arc<LocalDocumentStore>> {
        let workspace = self.workspace_session.as_ref()?;
        let store = self.local_documents.as_ref()?;
        store.validate_binding(workspace.binding()).ok()?;
        Some(store)
    }

    /// The memory the inspector opened on, while it is still this session's:
    /// once the workspace in front belongs to someone else, what the outline
    /// lists names nothing that may be read or changed.
    fn memory_place(&self) -> Option<MemoryPlace> {
        match (&self.memory_reads.place, &self.workspace_session) {
            (Some(MemoryPlace::Documents(documents)), Some(workspace))
                if documents.validate_binding(workspace.binding()).is_ok() =>
            {
                self.memory_reads.place.clone()
            }
            (Some(MemoryPlace::Project(..)), None) => self.memory_reads.place.clone(),
            _ => None,
        }
    }

    fn bound_prompt(&self) -> Option<Arc<BtwPrompt>> {
        self.btw_prompt.as_ref().map(|prompt| prompt.load_full())
    }

    /// Reads the files first, so an edit made outside Caudra shows, then the
    /// entries, the nodes and the fold.
    fn read_memory(&mut self) {
        self.ask_memory(|store| {
            if let Err(error) = store.reconcile() {
                warn!(scope = store.scope(), %error, "memory notes not reconciled for /memory");
            }
            MemoryReply::Snapshot(MemorySnapshot::load(store).map_err(|error| error.to_string()))
        });
    }

    fn ask_memory(&mut self, work: impl FnOnce(&MemoryStore) -> MemoryReply + Send + 'static) {
        let Some(place) = self.memory_place() else {
            self.flash(UNRESOLVED.into());
            self.land_memory(MemoryReply::Snapshot(Err(UNRESOLVED.into())));
            return;
        };
        let replies = self.memory_reads.tx.clone();
        smol::spawn(async move {
            let reply = smol::unblock(move || match place.open() {
                Ok(store) => work(&store),
                Err(error) => MemoryReply::Snapshot(Err(error)),
            })
            .await;
            let _ = replies.send(reply);
        })
        .detach();
    }

    /// Lands the store's answers, and reads the journal again once it changed
    /// or while the view waits on a summarizer.
    pub(super) fn poll_memory(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        while let Ok(reply) = self.memory_reads.rx.try_recv() {
            self.land_memory(reply);
            dirty = Dirty::YES;
        }
        let due = self.memory_reads.stale || self.memory_inspector.reload_due();
        if self.memory_inspector.is_open() && due && !self.memory_reads.awaited() {
            self.memory_reads.stale = false;
            self.read_memory();
        }
        dirty
    }

    /// A request in flight is looked at on the pending frame, so its answer
    /// lands without waiting for a key.
    pub(super) fn memory_cadence(&self) -> Cadence {
        Cadence::when(self.memory_reads.awaited(), Cadence::PENDING)
    }

    /// The summarizer wrote a line, which an open inspector reads again.
    pub(super) fn memory_changed(&mut self) {
        self.memory_reads.stale |= self.memory_inspector.is_open();
    }

    fn land_memory(&mut self, reply: MemoryReply) {
        let action = match reply {
            MemoryReply::Snapshot(snapshot) => {
                let prompt = self.bound_prompt();
                self.memory_inspector.fill(
                    snapshot,
                    prompt.as_deref().map(|prompt| prompt.system.as_str()),
                )
            }
            MemoryReply::Body(seq, body) => {
                self.memory_inspector.fill_body(seq, body);
                MemoryAction::Consumed
            }
            MemoryReply::Hits(query, hits) => self.memory_inspector.fill_hits(&query, hits),
            MemoryReply::Changed(report) => {
                self.flash(escape_terminal_controls(&report));
                self.memory_reads.stale = true;
                MemoryAction::Consumed
            }
        };
        self.handle_memory_action(action);
    }

    pub(super) fn handle_memory_action(&mut self, action: MemoryAction) -> Vec<Action> {
        match action {
            MemoryAction::Consumed => {}
            MemoryAction::Close => self.memory_inspector.close(),
            MemoryAction::Copy { text, label } => self.copy_labelled(&text, label),
            MemoryAction::Cut { text, query } => {
                self.copy_labelled(&text, COPIED_SELECTION);
                if let Some(query) = query {
                    self.search_memory(query);
                }
            }
            MemoryAction::Flash(message) => self.flash(message.into()),
            MemoryAction::LoadBody(seq) => {
                self.ask_memory(move |store| MemoryReply::Body(seq, read_body(store, seq)));
            }
            MemoryAction::Search(query) => self.search_memory(query),
            MemoryAction::Open(name) => self.open_memory_entry(&name),
            MemoryAction::Delete(name) => {
                self.ask_memory(move |store| MemoryReply::Changed(delete_note(store, &name)));
            }
            MemoryAction::Forget(name) => {
                self.ask_memory(move |store| MemoryReply::Changed(forget_note(store, &name)));
            }
        }
        Vec::new()
    }

    fn search_memory(&mut self, query: String) {
        self.ask_memory(move |store| {
            let hits = store.search(&query).map_err(|error| error.to_string());
            MemoryReply::Hits(query, hits)
        });
    }

    fn open_memory_entry(&mut self, name: &str) {
        match self.locate_note(name) {
            Ok(NoteAt::Stored(reference)) => self.open_stored_note(reference),
            Ok(NoteAt::File(path)) => self.open_memory_note(&path),
            Err(error) => self.flash(error),
        }
    }

    /// The note's newest version: the file on this machine, or the copy a
    /// remote session's store keeps.
    fn locate_note(&self, name: &str) -> Result<NoteAt, String> {
        match self.memory_place() {
            Some(MemoryPlace::Documents(documents)) => documents
                .memory_reference(documents.project_key(), name)
                .map(NoteAt::Stored)
                .map_err(|error| error.to_string()),
            Some(MemoryPlace::Project(state, cwd)) => {
                let [_, notes] = project_document_dirs(&state, &cwd);
                note_name(name)
                    .ok()
                    .map(|name| notes.join(name))
                    .filter(|path| path.is_file())
                    .map(NoteAt::File)
                    .ok_or_else(|| NOTE_GONE.to_owned())
            }
            None => Err(UNRESOLVED.to_owned()),
        }
    }

    /// Blocks until the store answers a request in flight, and lands the answer.
    #[cfg(test)]
    pub(crate) fn await_memory_reply(&mut self) {
        let reply = self
            .memory_reads
            .rx
            .recv_timeout(REPLY_WAIT)
            .expect(REPLY_MISSING);
        self.land_memory(reply);
    }
}

fn read_body(store: &MemoryStore, seq: u64) -> Result<Option<String>, String> {
    let entry = store.journal().entry(store.scope(), seq);
    entry
        .map(|entry| entry.map(|entry| entry.body))
        .map_err(|error| error.to_string())
}

/// Deleted as an edit made outside Caudra: the user, not a session, did it.
fn delete_note(store: &MemoryStore, name: &str) -> String {
    match store.delete(name, &EntryOrigin::External) {
        Ok(Some(_)) => format!("{DELETED}{name}"),
        Ok(None) => NOTE_GONE.to_owned(),
        Err(error) => format!("{DELETE_FAILED}{error}"),
    }
}

fn forget_note(store: &MemoryStore, name: &str) -> String {
    match store.forget(name) {
        Ok(report) => format!(
            "{FORGOT}{name}: {} {} and {} {} purged",
            report.entries,
            plural(report.entries, "entry", "entries"),
            report.nodes,
            plural(report.nodes, "line", "lines"),
        ),
        Err(error) => format!("{FORGET_FAILED}{error}"),
    }
}

fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    match count {
        1 => one,
        _ => many,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use caudra_storage::StateDir;
    use caudra_storage::projects::project_subdir;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, CwdHandle, ProjectIdentity, ProjectKey,
        ResourceId, ResourceScope, SessionBindingId, SessionWorkspaceBinding, SourceTrustAnchor,
        WorkspaceCapabilities, WorkspaceCursor, WorkspaceHandle, WorkspaceServices,
        WorkspaceSession,
    };
    use test_case::test_case;

    use super::{DELETED, LocalDocumentStore, MemoryAction, NoteAt, UNRESOLVED};
    use crate::app::tests::test_app;

    const NOTE: &str = "note.md";
    const CANARY: &str = "local legacy secret";
    const REMOTE_CONTENT: &str = "remote note";

    fn workspace(principal: &str) -> WorkspaceSession {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("origin").expect("anchor"),
            "server",
            "workspace",
            "generation",
            "namespace",
        )
        .expect("authority");
        let binding = SessionWorkspaceBinding::new(
            SessionBindingId::new("binding").expect("binding id"),
            authority.clone(),
            AuthenticatedPrincipalId::new(authority.clone(), principal).expect("principal"),
            ProjectIdentity::new(
                authority.clone(),
                ProjectKey::new("project").expect("project"),
            ),
        )
        .expect("binding");
        let cursor = WorkspaceCursor::new(
            &binding,
            ResourceScope::root(ResourceId::new("root").expect("root")),
            1,
            CwdHandle::new("cwd").expect("cwd"),
        );
        let handle = WorkspaceHandle::new(
            authority,
            WorkspaceCapabilities::new([]),
            WorkspaceServices::default(),
        )
        .expect("handle");
        WorkspaceSession::new(handle, binding, cursor).expect("workspace")
    }

    #[test_case(false; "scoped_delete")]
    #[test_case(true; "stale_selection_after_principal_change")]
    fn remote_memory_inspector_never_uses_local_paths(change_principal: bool) {
        let root = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(root.path().join("state"));
        let legacy = state
            .persistent_path()
            .join(project_subdir(root.path()))
            .join("memories");
        fs::create_dir_all(&legacy).expect("legacy dir");
        fs::write(legacy.join(NOTE), CANARY).expect("canary");
        let owner = workspace("owner");
        let store = Arc::new(LocalDocumentStore::remote(state.clone(), owner.binding()));
        let reference = store
            .write_memory(store.project_key(), NOTE, REMOTE_CONTENT)
            .expect("note");
        let mut app = test_app();
        app.state.session_mut().cwd = root.path().to_string_lossy().into_owned();
        app.storage = state.clone();
        app.workspace_session = Some(owner);
        app.local_documents = Some(Arc::clone(&store));

        app.open_memory_inspector();
        app.await_memory_reply();

        assert_eq!(app.locate_note(NOTE), Ok(NoteAt::Stored(reference)));
        if change_principal {
            let other = workspace("other");
            let other_store = Arc::new(LocalDocumentStore::remote(state, other.binding()));
            other_store
                .write_memory(other_store.project_key(), NOTE, CANARY)
                .expect("other note");
            app.workspace_session = Some(other);
            app.local_documents = Some(other_store);
            assert_eq!(app.locate_note(NOTE), Err(UNRESOLVED.to_owned()));
        }
        app.handle_memory_action(MemoryAction::Delete(NOTE.to_owned()));
        match change_principal {
            true => assert_eq!(app.status_bar.flash_text(), Some(UNRESOLVED)),
            false => {
                app.await_memory_reply();
                let flash = app.status_bar.flash_text().unwrap_or_default();
                assert!(flash.starts_with(DELETED), "{flash}");
            }
        }
        assert_eq!(
            store
                .list_memories(store.project_key())
                .expect("owner list")
                .len(),
            usize::from(change_principal)
        );
        if change_principal {
            let other = app.local_documents.as_ref().expect("store");
            assert_eq!(
                other
                    .list_memories(other.project_key())
                    .expect("other list")[0]
                    .content,
                CANARY
            );
        }
        app.local_documents = None;
        app.open_memory_inspector();
        assert_eq!(app.status_bar.flash_text(), Some(UNRESOLVED));
        app.handle_memory_action(MemoryAction::Delete(NOTE.to_owned()));
        assert_eq!(
            fs::read_to_string(legacy.join(NOTE)).expect("local canary"),
            CANARY
        );
    }
}
