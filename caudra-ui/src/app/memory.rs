//! `/memory`: browsing the project's persistent notes.
//!
//! The notes themselves belong to the `memory` tool; this is only the picker's
//! side of the conversation. Every list is rebuilt from disk rather than
//! cached here, because the model writes notes mid-session and the user edits
//! them in an external editor.

use std::path::{Path, PathBuf};

use caudra_agent::tools::native::memory;
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_workspace::MemoryRef;

use super::App;
use crate::components::Action;
use crate::components::memory_picker::MemoryPickerAction;
use crate::repaint::Dirty;

const UNRESOLVED: &str = "Cannot resolve memory directory";
const EMPTY: &str = "No memories yet";
const NOTE_GONE: &str = "Note is already gone";
const REMOTE_EDITOR: &str =
    "Use local_document_read/write with the memory reference to view or edit this note";

impl App {
    pub(super) fn memory_browse(&mut self) -> Vec<Action> {
        let Some((entries, unreadable)) = self.memory_notes() else {
            self.flash(UNRESOLVED.into());
            return Vec::new();
        };
        if entries.is_empty() {
            self.flash(EMPTY.into());
            return Vec::new();
        }
        if unreadable > 0 {
            self.flash(format!("{unreadable} unreadable memory file(s)"));
        }
        self.memory_picker.open(entries);
        Vec::new()
    }

    fn remote_memory_store(&self) -> Option<&LocalDocumentStore> {
        let workspace = self.workspace_session.as_ref()?;
        let store = self.local_documents.as_deref()?;
        store.validate_binding(workspace.binding()).ok()?;
        Some(store)
    }

    fn memory_notes(&self) -> Option<(Vec<memory::BrowseEntry>, usize)> {
        if self.workspace_session.is_some() {
            let entries = memory::browse_store(self.remote_memory_store()?).ok()?;
            return Some((entries.into_iter().map(|(_, entry)| entry).collect(), 0));
        }
        memory::browse(Path::new(&self.state.session.cwd))
            .map(|(_, entries, unreadable)| (entries, unreadable))
    }

    /// Re-reads the notes directory behind an open picker. The list closes
    /// when the last note goes, so an empty picker never lingers.
    fn memory_refresh(&mut self) {
        let Some((entries, _)) = self.memory_notes() else {
            self.memory_picker.close();
            return;
        };
        if entries.is_empty() {
            self.memory_picker.close();
            self.flash(EMPTY.into());
        } else {
            self.memory_picker.open(entries);
        }
    }

    /// The editor runs synchronously in the event loop, so a tick that sees
    /// the stale flag is already past it and can trust the disk.
    pub(super) fn refresh_memory_picker_if_stale(&mut self) -> Dirty {
        if !self.memory_picker.is_open() || !self.memory_picker.take_stale() {
            return Dirty::NO;
        }
        self.memory_refresh();
        Dirty::YES
    }

    pub(super) fn handle_memory_picker_action(
        &mut self,
        action: MemoryPickerAction,
    ) -> Vec<Action> {
        match action {
            MemoryPickerAction::Consumed | MemoryPickerAction::Closed => Vec::new(),
            MemoryPickerAction::Open(_) if self.workspace_session.is_some() => {
                self.flash(REMOTE_EDITOR.into());
                Vec::new()
            }
            MemoryPickerAction::Delete(name) if self.workspace_session.is_some() => {
                let result = self
                    .remote_memory_store()
                    .ok_or_else(|| UNRESOLVED.to_owned())
                    .and_then(|store| delete_remote_memory(store, &name));
                match result {
                    Ok(()) => self.flash(format!("Deleted {name}")),
                    Err(error) => self.flash(format!("Delete failed: {error}")),
                }
                self.memory_refresh();
                Vec::new()
            }
            MemoryPickerAction::Open(name) => match self.memory_note_path(&name) {
                Some(path) => vec![Action::OpenEditor(path)],
                None => {
                    self.flash(NOTE_GONE.into());
                    Vec::new()
                }
            },
            MemoryPickerAction::Delete(name) => {
                match self.memory_note_path(&name).map(std::fs::remove_file) {
                    Some(Ok(())) => self.flash(format!("Deleted {name}")),
                    Some(Err(error)) => self.flash(format!("Delete failed: {error}")),
                    None => self.flash(NOTE_GONE.into()),
                }
                self.memory_refresh();
                Vec::new()
            }
        }
    }

    /// Resolved through the tool's own path guard, so a name that came back
    /// from the picker still cannot escape the notes directory.
    fn memory_note_path(&self, name: &str) -> Option<PathBuf> {
        if self.workspace_session.is_some() {
            return None;
        }
        let (dir, ..) = memory::browse(Path::new(&self.state.session.cwd))?;
        memory::paths::safe_resolve(&dir, name).ok()
    }
}

fn delete_remote_memory(store: &LocalDocumentStore, selection: &str) -> Result<(), String> {
    let reference: MemoryRef = memory::browse_store(store)?
        .into_iter()
        .find(|(_, entry)| entry.name == selection)
        .map(|(reference, _)| reference)
        .ok_or_else(|| NOTE_GONE.to_owned())?;
    store
        .delete_memory_ref(store.project_key(), &reference)
        .map_err(|error| error.to_string())
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

    use super::{LocalDocumentStore, MemoryPickerAction, memory};
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
    fn remote_memory_picker_never_uses_local_paths(change_principal: bool) {
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
        let selection = memory::browse_store(&store).expect("browse")[0]
            .1
            .name
            .clone();
        let mut app = test_app();
        app.state.session_mut().cwd = root.path().to_string_lossy().into_owned();
        app.workspace_session = Some(owner);
        app.local_documents = Some(Arc::clone(&store));
        let (entries, _) = app.memory_notes().expect("remote browse");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].name.contains(reference.as_str()));
        assert!(app.memory_note_path(NOTE).is_none());
        assert!(
            app.handle_memory_picker_action(MemoryPickerAction::Open(selection.clone()))
                .is_empty()
        );
        if change_principal {
            let other = workspace("other");
            let other_store = Arc::new(LocalDocumentStore::remote(state, other.binding()));
            other_store
                .write_memory(other_store.project_key(), NOTE, CANARY)
                .expect("other note");
            app.workspace_session = Some(other);
            assert!(app.memory_notes().is_none());
            app.local_documents = Some(other_store);
        }
        app.handle_memory_picker_action(MemoryPickerAction::Delete(selection));
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
        assert_eq!(
            fs::read_to_string(legacy.join(NOTE)).expect("local canary"),
            CANARY
        );
        app.local_documents = None;
        assert!(app.memory_notes().is_none());
        app.handle_memory_picker_action(MemoryPickerAction::Delete(NOTE.to_owned()));
        assert_eq!(
            fs::read_to_string(legacy.join(NOTE)).expect("local canary"),
            CANARY
        );
    }
}
