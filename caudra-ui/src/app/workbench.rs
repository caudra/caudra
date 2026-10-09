//! Caudra's own text in the workbench: the plan, the memory notes, and the
//! prompt draft.
//!
//! The plan and the notes live in the state directory rather than the project,
//! so they open under a name that says what they are, and the agent's writes
//! reach their tabs through its tool results, because the project's watch
//! never sees them. A remote workspace keeps them in the local document store
//! instead, by reference rather than by path, so their tabs hold documents the
//! store hands over and a save hands back. The prompt draft is no file at all:
//! the workbench holds it as a document and hands it back to the composer it
//! came from when saved.

use std::path::{Path, PathBuf};

use caudra_agent::ToolDoneEvent;
use caudra_agent::tools::MEMORY_TOOL_NAME;
use caudra_agent::tools::native::plan::{PlanTarget, PlanWriteResult};
use caudra_storage::local_documents::{DocumentRevision, LocalDocument, LocalDocumentError};
use caudra_storage::projects::project_document_dirs;
use caudra_workbench::{DocumentKey, TabLabel};
use caudra_workspace::{LocalDocumentRef, MemoryRef, PlanRef};

use super::tasks::MAIN_TASK_ID;
use super::{App, KeyFocus};
use crate::components::paste_editor::PasteEditorTarget;

const PLAN_TITLE: &str = "Plan";
const MEMORY_TITLE: &str = "Memory";
const PROMPT_TITLE: &str = "Prompt";
const PROMPT_KEY_PREFIX: &str = "prompt:";
const PLAN_KEY_PREFIX: &str = "plan:";
const MEMORY_KEY_PREFIX: &str = "memory:";
pub(super) const FLASH_NO_PLAN: &str = "No plan file";
const PLAN_NOT_WRITTEN: &str = "The plan has not been written yet";
const PLAN_UNSAVED: &str = "The plan has unsaved edits: save them before implementing";
const LOCAL_SOURCE_OPEN: &str = "Close the local policy source first";
const DRAFT_OTHER_COMPOSER: &str = "This draft is for another chat: switch back to it to apply it";
const STORE_UNAVAILABLE: &str = "Plans and notes are unavailable in this workspace";
const STORED_CHANGED: &str = "Changed since it was opened: Ctrl+R takes the newer copy";
/// How much of a plan's reference its status row shows: enough to tell two
/// plans apart, the way a short commit hash does, and little enough to leave
/// the row room to say it is a plan.
const SHORT_REFERENCE: usize = 13;

/// A plan or note from the local document store, open in a workbench tab.
pub(super) struct StoredDocument {
    reference: LocalDocumentRef,
    /// A plan is filed under the session that made it, which may not be the
    /// one in front by the time its tab is saved.
    session: String,
    /// What the store held when the tab last took its text. A save over
    /// anything newer would undo a change the reader never saw.
    revision: DocumentRevision,
}

impl StoredDocument {
    /// Whether `done` may have changed the document. The `memory` tool names
    /// a note by name rather than by reference, so any call of it counts.
    fn written_by(&self, done: &ToolDoneEvent) -> bool {
        done.wrote_document(&self.reference)
            || (matches!(self.reference, LocalDocumentRef::Memory(_))
                && &*done.tool == MEMORY_TOOL_NAME)
    }
}

impl App {
    /// Whether the permission-source editor holds the workbench. Opening
    /// anything else would re-root it away from the policy file it was lent
    /// for, so the caller refuses and this says why.
    pub(super) fn workbench_lent(&mut self) -> bool {
        let lent = self.parked_workbench.is_some();
        if lent {
            self.flash(LOCAL_SOURCE_OPEN.into());
        }
        lent
    }

    /// `Ctrl+O`, `Ctrl+X o` and the plan form all land here. A plan still
    /// being drafted may not be written yet, which an external editor showed
    /// as an empty buffer: locally there is no file for the workbench to open,
    /// and a remote workspace's store holds an empty document.
    pub(super) fn open_plan(&mut self) {
        if let Some(reference) = self.state.plan.reference().cloned() {
            self.open_stored_plan(reference);
            return;
        }
        let Some(plan) = self.state.plan.path().map(Path::to_path_buf) else {
            self.flash(FLASH_NO_PLAN.into());
            return;
        };
        if !plan.is_file() {
            self.flash(PLAN_NOT_WRITTEN.into());
            return;
        }
        let name = plan.file_name().unwrap_or_default().to_string_lossy();
        let label = TabLabel {
            title: PLAN_TITLE.to_owned(),
            status: format!("{PLAN_TITLE} · {name}"),
        };
        self.open_caudra_file(&plan, label);
    }

    /// A note from `/memory` or a memory card, under its name within the notes
    /// directory rather than the path to it.
    pub(super) fn open_memory_note(&mut self, note: &Path) {
        self.memory_inspector.close();
        let [_, notes] = project_document_dirs(&self.storage, Path::new(&self.state.session.cwd));
        let name = note
            .strip_prefix(&notes)
            .ok()
            .or_else(|| note.file_name().map(Path::new))
            .unwrap_or(note)
            .to_string_lossy()
            .into_owned();
        let label = TabLabel {
            status: format!("{MEMORY_TITLE} · {name}"),
            title: name,
        };
        self.open_caudra_file(note, label);
    }

    fn open_caudra_file(&mut self, path: &Path, label: TabLabel) {
        if self.workbench_lent() {
            return;
        }
        self.sync_workbench_theme();
        let cwd = PathBuf::from(&self.state.session.cwd);
        if let Err(error) = self.workbench.open_labelled(&cwd, path, label) {
            self.flash(error.to_string());
        }
    }

    /// A remote workspace's plan, which the store keeps by reference.
    fn open_stored_plan(&mut self, reference: PlanRef) {
        let session = self.state.session.id.to_string();
        let id = reference.as_str();
        let status = format!("{PLAN_TITLE} · {}", id.get(..SHORT_REFERENCE).unwrap_or(id));
        let Some(document) = self.read_or_flash(&LocalDocumentRef::Plan(reference), &session)
        else {
            return;
        };
        if document.content.trim().is_empty() {
            self.flash(PLAN_NOT_WRITTEN.into());
            return;
        }
        let label = TabLabel {
            title: PLAN_TITLE.to_owned(),
            status,
        };
        self.open_stored(document, session, label);
    }

    /// A remote workspace's note, which the store keeps under its name.
    pub(super) fn open_stored_note(&mut self, reference: MemoryRef) {
        self.memory_inspector.close();
        let session = self.state.session.id.to_string();
        let Some(document) = self.read_or_flash(&LocalDocumentRef::Memory(reference), &session)
        else {
            return;
        };
        let name = document.name.clone().unwrap_or_default();
        let label = TabLabel {
            status: format!("{MEMORY_TITLE} · {name}"),
            title: name,
        };
        self.open_stored(document, session, label);
    }

    /// Opens a document from the store in a tab of its own, or raises the one
    /// already open. Unsaved edits there stay, and so does the revision they
    /// were made against, so their save still finds whatever moved since.
    fn open_stored(&mut self, document: LocalDocument, session: String, label: TabLabel) {
        if self.workbench_lent() {
            return;
        }
        self.sync_workbench_theme();
        if !self.raise_workbench() {
            return;
        }
        let key = stored_key(&document.reference);
        if !self.workbench.has_unsaved_document(&key) {
            let stored = StoredDocument {
                reference: document.reference,
                session,
                revision: document.revision,
            };
            self.stored_documents.insert(key.clone(), stored);
        }
        self.workbench.open_document(key, label, &document.content);
    }

    /// Opens the workbench the way `Ctrl+X w` opens it, stored layout and all,
    /// unless it is up already, and reports whether it is. A document never
    /// joins that layout, so a workbench opened for one alone would store an
    /// empty layout over the reader's tabs.
    fn raise_workbench(&mut self) -> bool {
        if !self.workbench.is_open() {
            self.toggle_workbench();
        }
        self.workbench.is_open()
    }

    /// `Ctrl+X e`: the composer's text in a workbench tab of its own, with
    /// room to write and a rendered view to read it back in. A draft left
    /// unsaved there is raised as it stands rather than written over.
    pub(super) fn open_prompt_draft(&mut self) {
        if self.workbench_lent() {
            return;
        }
        let Some(target) = self.active_input_target() else {
            return;
        };
        self.sync_workbench_theme();
        if !self.raise_workbench() {
            return;
        }
        let label = TabLabel {
            title: PROMPT_TITLE.to_owned(),
            status: format!("{PROMPT_TITLE} · {}", self.chats[self.active_chat].name),
        };
        let text = self.active_input_text();
        self.workbench
            .open_document(prompt_draft_key(&target), label, &text);
    }

    /// Where a saved document goes: a plan or note back to the store it came
    /// from, and a draft to its composer. `close` is `Ctrl+X Enter`, which
    /// goes back to the composer as well.
    pub(super) fn save_document(&mut self, key: &DocumentKey, text: String, close: bool) {
        let kept = match self.stored_documents.contains_key(key) {
            true => self.save_stored(key, &text),
            false => self.save_prompt_draft(key, text),
        };
        if !kept {
            return;
        }
        self.workbench.document_saved(key);
        if close {
            self.close_permission_source();
            self.key_focus = KeyFocus::Composer;
        }
    }

    /// A saved draft lands only in the composer it was drafted from, the rule
    /// the paste editor keeps, so it never writes over another chat's prompt.
    fn save_prompt_draft(&mut self, key: &DocumentKey, text: String) -> bool {
        let drafted_here = self.drafted_here(key);
        match drafted_here {
            true => self.apply_prompt_draft(text),
            false => self.flash(DRAFT_OTHER_COMPOSER.into()),
        }
        drafted_here
    }

    /// A save lands only over the revision its text came from. Anything newer
    /// in the store is a change the reader has not seen, so the tab keeps its
    /// edits and flies the conflict instead.
    fn save_stored(&mut self, key: &DocumentKey, text: &str) -> bool {
        let Some(document) = self.stored_documents.get(key) else {
            return false;
        };
        let saved = self.remote_document_store().map(|store| {
            store.replace(
                store.project_key(),
                Some(&document.session),
                &document.reference,
                &document.revision,
                text,
            )
        });
        match saved {
            Some(Ok(revision)) => {
                self.record_revision(key, revision);
                return true;
            }
            Some(Err(LocalDocumentError::StaleRevision { .. })) => {
                self.flash(STORED_CHANGED.into());
                self.refresh_stored(key);
            }
            Some(Err(error)) => self.flash(error.to_string()),
            None => self.flash(STORE_UNAVAILABLE.into()),
        }
        false
    }

    /// `Ctrl+R` over a document: its edits go for the copy its keeper holds,
    /// the store's for a plan or note and the composer's for a draft.
    pub(super) fn revert_document(&mut self, key: &DocumentKey) {
        let Some(document) = self.stored_documents.get(key) else {
            self.revert_prompt_draft(key);
            return;
        };
        match self.read_stored(&document.reference, &document.session) {
            Ok(current) => {
                self.workbench.revert_document(key, &current.content);
                self.record_revision(key, current.revision);
            }
            Err(error) => self.flash(error),
        }
    }

    fn revert_prompt_draft(&mut self, key: &DocumentKey) {
        if !self.drafted_here(key) {
            self.flash(DRAFT_OTHER_COMPOSER.into());
            return;
        }
        let text = self.active_input_text();
        self.workbench.revert_document(key, &text);
    }

    fn drafted_here(&self, key: &DocumentKey) -> bool {
        self.active_input_target()
            .is_some_and(|target| prompt_draft_key(&target) == *key)
    }

    /// Implementing reads the plan from where it is kept, so edits still in
    /// the workbench would be dropped without a word. Puts them back in front
    /// of the reader instead.
    pub(super) fn plan_unsaved(&mut self) -> bool {
        let workbench = self.bound_workbench();
        let unsaved = match self.state.plan.document_ref() {
            Some(reference) => workbench.has_unsaved_document(&stored_key(&reference)),
            None => self
                .state
                .plan
                .path()
                .is_some_and(|plan| workbench.has_unsaved(plan)),
        };
        if unsaved {
            self.open_plan();
            self.flash(PLAN_UNSAVED.into());
        }
        unsaved
    }

    /// The plan and the memory notes live where the workbench's watch cannot
    /// see them change, so the call that wrote them is the only one that can
    /// say so.
    pub(super) fn reload_written(&mut self, done: &ToolDoneEvent) {
        self.refresh_stored_documents(done);
        if done.remote_written_paths {
            return;
        }
        let cwd = Path::new(&self.state.session.cwd);
        let written: Vec<PathBuf> = done.written_paths().map(|path| cwd.join(path)).collect();
        self.bound_workbench_mut()
            .reload_paths(written.iter().map(PathBuf::as_path));
    }

    /// The `plan` tool's writes skip `reload_written`, so this is what brings
    /// an open plan tab up to date. A remote tab reads the store back, because
    /// its next save is checked against the revision kept there.
    pub(super) fn reload_committed_plan(&mut self, plan: &PlanWriteResult) {
        match plan.target() {
            PlanTarget::Local(path) => {
                self.bound_workbench_mut()
                    .replace_file(path, plan.content());
            }
            PlanTarget::Remote(reference) => {
                self.refresh_stored(&stored_key(&LocalDocumentRef::Plan(reference.clone())));
            }
        }
    }

    /// A remote workspace's plan and notes change through the store, so the
    /// call that wrote one is what brings its tab up to date. Documents whose
    /// tabs have closed since are forgotten on the way.
    fn refresh_stored_documents(&mut self, done: &ToolDoneEvent) {
        if done.is_error || self.stored_documents.is_empty() {
            return;
        }
        let workbench = self.parked_workbench.as_ref().unwrap_or(&self.workbench);
        self.stored_documents
            .retain(|key, _| workbench.has_document(key));
        let written: Vec<DocumentKey> = self
            .stored_documents
            .iter()
            .filter(|(_, document)| document.written_by(done))
            .map(|(key, _)| key.clone())
            .collect();
        for key in written {
            self.refresh_stored(&key);
        }
    }

    /// Brings a document's tab up to the store's copy: a clean tab takes it,
    /// and one with unsaved edits keeps them and flies the conflict. A note
    /// the agent deleted has no copy left, and its tab stays as it was.
    fn refresh_stored(&mut self, key: &DocumentKey) {
        let Some(document) = self.stored_documents.get(key) else {
            return;
        };
        let current = match self.read_stored(&document.reference, &document.session) {
            Ok(current) if current.revision != document.revision => current,
            Ok(_) => return,
            Err(error) => {
                tracing::debug!(%error, "stored document unreadable after a write");
                return;
            }
        };
        if self
            .bound_workbench_mut()
            .replace_document(key, &current.content)
        {
            self.record_revision(key, current.revision);
        }
    }

    fn record_revision(&mut self, key: &DocumentKey, revision: DocumentRevision) {
        if let Some(document) = self.stored_documents.get_mut(key) {
            document.revision = revision;
        }
    }

    fn read_stored(
        &self,
        reference: &LocalDocumentRef,
        session: &str,
    ) -> Result<LocalDocument, String> {
        let store = self
            .remote_document_store()
            .ok_or_else(|| STORE_UNAVAILABLE.to_owned())?;
        store
            .read(store.project_key(), Some(session), reference)
            .map_err(|error| error.to_string())
    }

    fn read_or_flash(
        &mut self,
        reference: &LocalDocumentRef,
        session: &str,
    ) -> Option<LocalDocument> {
        match self.read_stored(reference, session) {
            Ok(document) => Some(document),
            Err(error) => {
                self.flash(error);
                None
            }
        }
    }
}

/// Names a plan or note from the store, so its one tab can be found again.
fn stored_key(reference: &LocalDocumentRef) -> DocumentKey {
    let (prefix, id) = match reference {
        LocalDocumentRef::Plan(plan) => (PLAN_KEY_PREFIX, plan.as_str()),
        LocalDocumentRef::Memory(note) => (MEMORY_KEY_PREFIX, note.as_str()),
    };
    DocumentKey(format!("{prefix}{id}"))
}

/// Names the draft of one composer, so each chat keeps a draft of its own.
fn prompt_draft_key(target: &PasteEditorTarget) -> DocumentKey {
    let chat = match target {
        PasteEditorTarget::Main => MAIN_TASK_ID,
        PasteEditorTarget::Subagent(id) => id,
    };
    DocumentKey(format!("{PROMPT_KEY_PREFIX}{chat}"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use caudra_agent::tools::native::plan::{self, PlanTarget, PlanWriteResult};
    use caudra_agent::tools::{
        BATCH_TOOL_NAME, FILE_WRITE_TOOL_NAME, MEMORY_TOOL_NAME, ToolEffect,
    };
    use caudra_agent::{
        AgentEvent, AgentMode, BatchProgressEvent, BatchToolEntry, BatchToolStatus, TextOutput,
        ToolAccounting, ToolDoneEvent, ToolOutput, ToolStartEvent,
    };
    use caudra_storage::StateDir;
    use caudra_storage::local_documents::LocalDocumentStore;
    use caudra_storage::plans::PlanFile;
    use caudra_workbench::{Layout as WorkbenchLayout, Workbench, keys as workbench_keys};
    use caudra_workspace::{LocalDocumentRef, MemoryRef};
    use crossterm::event::KeyCode;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        DRAFT_OTHER_COMPOSER, FLASH_NO_PLAN, LOCAL_SOURCE_OPEN, PLAN_NOT_WRITTEN, PLAN_UNSAVED,
        SHORT_REFERENCE, STORED_CHANGED, stored_key,
    };
    use crate::app::mode::PLAN_COPY_FAILED;
    use crate::app::permission_editor::tests::workbench_workspace;
    use crate::app::tests::{
        agent_msg, focused_task_composer, press_chord, private_tempdir, rendered, sent_input,
        test_app,
    };
    use crate::app::{App, KeyFocus, Mode, Msg, PlanState, PlanTrigger};
    use crate::components::keybindings::{Bind, key, leader};
    use crate::components::memory_inspector::MemoryAction;
    use crate::components::workbench::styles as workbench_styles;
    use crate::components::{Action, Status, key as press};

    const PLAN_FILE: &str = "calm-otter.md";
    const PLAN_STATUS: &str = "Plan · calm-otter.md";
    const PLAN_STATUS_PREFIX: &str = "Plan · ";
    const PLAN_TEXT: &str = "# Plan\n";
    const REWRITTEN_PLAN: &str = "# Rewritten plan";
    const EARLIER_PLAN: &str = "# Earlier committed plan";
    const BATCH_PLAN_ID: &str = "batch-plan";
    const NOTE_FILE: &str = "arch.md";
    const NOTE_STATUS: &str = "Memory · arch.md";
    const EDIT: char = 'X';
    const PASTED: &str = "pasted";
    const TEMP_DIR: &str = "a temporary directory";
    const WRITTEN: &str = "a file on disk";
    const STORED: &str = "a document in the store";
    const PLAN_NOT_OPENED: &str = "the plan did not open in the workbench";
    const NOT_LABELLED: &str = "the tab does not say what it is";
    const WRONG_FLASH: &str = "the status bar does not say why";
    const FORM_TOOK_KEYS: &str = "the plan form took keys meant for the workbench over it";
    const IMPLEMENTED_UNSAVED: &str = "implementing went ahead with edits still unsaved";
    const STALE_PLAN: &str = "the open plan missed the agent's rewrite";
    const SOURCE_REROOTED: &str = "opening the plan took the workbench from the policy source";
    const NOTE_NOT_OPENED: &str = "the note did not open in the workbench";
    const INSPECTOR_LEFT_UP: &str = "the inspector stayed up over the note it opened";
    const EMPHASIS_SOURCE: &str = "Keep **calm**";
    const EMPHASIS_RENDERED: &str = "Keep calm";
    const NOT_RENDERED: &str = "the plan is not painted the way the transcript paints Markdown";
    const TRANSCRIPT_TOGGLED: &str = "the chord reached the transcript behind the workbench";
    const DRAFT: &str = "first thought";
    const NEWER: &str = "second thought";
    const MAIN_DRAFT_STATUS: &str = "Prompt · Main";
    const DRAFT_NOT_OPENED: &str = "the edit chord did not open the prompt in the workbench";
    const NOT_APPLIED: &str = "the saved draft did not reach the composer it came from";
    const WRONG_COMPOSER: &str = "the draft reached a composer it was not drafted from";
    const LEFT_EARLY: &str = "saving the draft took the workbench down";
    const DRAFT_LOST: &str = "a draft the composer refused lost its unsaved edits";
    const STALE_DRAFT: &str = "the draft came back showing the wrong text";
    const NOT_RETURNED: &str = "sending the draft did not hand the keyboard back to the composer";
    const STORED_FILE: &str = "kept.rs";
    const LAYOUT_LOST: &str = "opening the draft lost the tabs the workbench had stored";
    const NOT_STORED: &str = "the save did not reach the store the document came from";
    const OVERWRITTEN: &str = "a save undid a change the reader had not seen";
    const EDITS_LOST: &str = "the tab lost edits that were never saved";
    const STALE_NOTE: &str = "the open note missed the memory tool's rewrite";
    const PLAN_NOT_ADOPTED: &str = "the new session did not take the plan over as its own";
    const BLOCKED_STATE: &str = "not-a-directory";
    const REMOTE_SESSION: &str = "a session in a remote workspace";
    const SEND: Bind = Bind::from_workbench(workbench_keys::SEND_TO_COMPOSER);
    const REVERT: Bind = Bind::from_workbench(workbench_keys::REVERT);

    #[derive(Clone, Copy)]
    enum Draft {
        Absent,
        Unwritten,
        Written,
    }

    #[derive(Clone, Copy)]
    enum Composer {
        Main,
        Task,
    }

    /// `Ctrl+S` keeps the workbench up, and `Ctrl+X Enter` leaves it.
    #[derive(Clone, Copy)]
    enum Handback {
        Save,
        Send,
    }

    /// A project whose plan is kept beside it rather than in it, the way the
    /// state directory keeps it. The directories go when `_dirs` does, so a
    /// test binds it for as long as it reads the files.
    struct Planned {
        plan: PathBuf,
        app: App,
        _dirs: [TempDir; 2],
    }

    fn planned(draft: Draft) -> Planned {
        let project = private_tempdir();
        let state = private_tempdir();
        let plan = state.path().join(PLAN_FILE);
        let mut app = test_app();
        app.state.session_mut().cwd = project.path().to_string_lossy().into_owned();
        app.state.mode = Mode::Plan;
        app.state.applied_mode = Mode::Plan;
        app.state.plan = match draft {
            Draft::Absent => PlanState::None,
            Draft::Unwritten | Draft::Written => PlanState::Drafting(plan.clone()),
        };
        if let Draft::Written = draft {
            PlanFile::new(plan.clone())
                .unwrap()
                .write(PLAN_TEXT)
                .expect(WRITTEN);
            app.transition_plan(PlanTrigger::WriteDone);
        }
        Planned {
            plan,
            app,
            _dirs: [project, state],
        }
    }

    fn open_plan(app: &mut App) {
        app.update(Msg::Key(key::OPEN_EDITOR.to_key_event()));
    }

    fn save(app: &mut App) {
        app.update(Msg::Key(key::SAVE.to_key_event()));
    }

    /// A composer holding [`DRAFT`], in the main chat or in a task's.
    fn composing(composer: Composer) -> App {
        let mut app = match composer {
            Composer::Main => test_app(),
            Composer::Task => focused_task_composer().0,
        };
        app.active_input_box_mut().buffer.insert_text(DRAFT);
        app
    }

    fn edit_draft(app: &mut App) {
        press_chord(app, leader::EDIT_INPUT);
        app.update(Msg::Key(press(KeyCode::Char(EDIT))));
    }

    fn edited() -> String {
        format!("{EDIT}{DRAFT}")
    }

    fn hand_back(app: &mut App, handback: Handback) {
        match handback {
            Handback::Save => save(app),
            Handback::Send => {
                press_chord(app, SEND);
            }
        }
    }

    fn focus_chat(app: &mut App, chat: usize) {
        app.active_chat = chat;
        app.sync_subagent_input_target();
    }

    fn done(tool: &str, annotation: Option<String>, written_paths: Vec<String>) -> Msg {
        agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
            id: "rewrite".into(),
            tool: tool.into(),
            output: ToolOutput::Plain("wrote plan".into()),
            is_error: false,
            annotation,
            written_path: None,
            written_paths,
            remote_written_paths: false,
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
            documents: Vec::new(),
            accounting: ToolAccounting::default(),
        })))
    }

    fn wrote(path: &Path) -> Msg {
        let written = vec![path.to_string_lossy().into_owned()];
        done(FILE_WRITE_TOOL_NAME, None, written)
    }

    /// A session in a remote workspace, whose plan and notes live in a store
    /// of their own. The store's directory goes when `_state` does.
    struct Remote {
        app: App,
        store: Arc<LocalDocumentStore>,
        _state: TempDir,
    }

    impl Remote {
        fn session(&self) -> String {
            self.app.state.session.id.to_string()
        }

        fn text(&self, document: &LocalDocumentRef) -> String {
            self.store
                .read(self.store.project_key(), Some(&self.session()), document)
                .expect(STORED)
                .content
        }

        /// Writes `document` the way another writer would, behind its tab.
        fn write(&self, document: &LocalDocumentRef, text: &str) {
            self.store
                .write(
                    self.store.project_key(),
                    Some(&self.session()),
                    document,
                    text,
                )
                .expect(STORED);
        }

        fn note(&self, text: &str) -> MemoryRef {
            self.store
                .write_memory(self.store.project_key(), NOTE_FILE, text)
                .expect(STORED)
        }
    }

    fn remote() -> Remote {
        let state = private_tempdir();
        let workspace = workbench_workspace();
        let store = Arc::new(LocalDocumentStore::remote(
            StateDir::from_path(state.path().to_path_buf()),
            workspace.binding(),
        ));
        let mut app = test_app();
        app.workspace_session = Some(workspace);
        app.local_documents = Some(Arc::clone(&store));
        Remote {
            app,
            store,
            _state: state,
        }
    }

    /// A remote session in plan mode whose plan holds `text`, and is ready
    /// once there is any.
    fn remote_plan(text: &str) -> (Remote, LocalDocumentRef) {
        let mut remote = remote();
        let plan = remote
            .store
            .create_plan(remote.store.project_key(), &remote.session())
            .expect(STORED);
        let document = LocalDocumentRef::Plan(plan.clone());
        remote.write(&document, text);
        remote.app.state.mode = Mode::Plan;
        remote.app.state.applied_mode = Mode::Plan;
        remote.app.state.plan = PlanState::RemoteDrafting(plan);
        if !text.is_empty() {
            remote.app.transition_plan(PlanTrigger::WriteDone);
        }
        (remote, document)
    }

    fn type_edit(app: &mut App) {
        app.update(Msg::Key(press(KeyCode::Char(EDIT))));
    }

    fn revert(app: &mut App) {
        app.update(Msg::Key(REVERT.to_key_event()));
    }

    #[test_case(Draft::Absent, Some(FLASH_NO_PLAN) ; "no plan")]
    #[test_case(Draft::Unwritten, Some(PLAN_NOT_WRITTEN) ; "a plan not written yet")]
    #[test_case(Draft::Written, None ; "a written plan")]
    fn ctrl_o_opens_the_plan_in_the_workbench(draft: Draft, refusal: Option<&str>) {
        let Planned {
            plan,
            mut app,
            _dirs,
        } = planned(draft);

        open_plan(&mut app);

        assert_eq!(app.status_bar.flash_text(), refusal, "{WRONG_FLASH}");
        assert_eq!(
            app.workbench.is_open(),
            refusal.is_none(),
            "{PLAN_NOT_OPENED}"
        );
        if refusal.is_none() {
            assert_eq!(app.workbench.layout().tabs, [plan], "{PLAN_NOT_OPENED}");
            assert!(rendered(&mut app).contains(PLAN_STATUS), "{NOT_LABELLED}");
        }
    }

    #[test]
    fn the_plan_chord_opens_it_too() {
        let Planned {
            plan,
            mut app,
            _dirs,
        } = planned(Draft::Written);

        press_chord(&mut app, leader::PLAN_EDITOR);

        assert_eq!(app.workbench.layout().tabs, [plan], "{PLAN_NOT_OPENED}");
    }

    /// The form stays up behind the workbench, and used to answer first: Enter
    /// dismissed it, and every other key vanished into it.
    #[test]
    fn keys_over_the_plan_form_reach_the_workbench() {
        let Planned {
            plan,
            mut app,
            _dirs,
        } = planned(Draft::Written);
        open_plan(&mut app);

        app.update(Msg::Key(press(KeyCode::Char(EDIT))));
        app.update(Msg::Key(press(KeyCode::Enter)));
        app.update(Msg::Paste(PASTED.into()));
        save(&mut app);

        let saved = fs::read_to_string(&plan).expect(WRITTEN);
        assert_eq!(
            saved,
            format!("{EDIT}\n{PASTED}{PLAN_TEXT}"),
            "{FORM_TOOK_KEYS}"
        );
        assert!(app.state.plan.is_ready(), "{FORM_TOOK_KEYS}");
        assert_eq!(app.state.mode, Mode::Plan, "{FORM_TOOK_KEYS}");
        assert!(app.plan_form.is_visible(), "{FORM_TOOK_KEYS}");
    }

    #[test]
    fn implementing_waits_for_the_plan_to_be_saved() {
        let Planned {
            plan,
            mut app,
            _dirs,
        } = planned(Draft::Written);
        open_plan(&mut app);
        app.update(Msg::Key(press(KeyCode::Char(EDIT))));
        app.workbench.close();

        app.update(Msg::Key(press(KeyCode::Down)));
        app.update(Msg::Key(press(KeyCode::Down)));
        let actions = app.update(Msg::Key(press(KeyCode::Enter)));

        assert!(actions.is_empty(), "{IMPLEMENTED_UNSAVED}");
        assert_eq!(
            app.status_bar.flash_text(),
            Some(PLAN_UNSAVED),
            "{WRONG_FLASH}"
        );
        assert_eq!(app.state.mode, Mode::Plan, "{IMPLEMENTED_UNSAVED}");
        assert!(app.plan_form.is_visible(), "{IMPLEMENTED_UNSAVED}");
        assert!(app.workbench.has_unsaved(&plan), "{IMPLEMENTED_UNSAVED}");
        assert!(app.workbench.is_open(), "{PLAN_NOT_OPENED}");
    }

    #[test]
    fn an_agent_rewrite_reaches_the_open_plan() {
        let Planned {
            plan,
            mut app,
            _dirs,
        } = planned(Draft::Written);
        open_plan(&mut app);
        app.status = Status::Streaming;
        app.run_id = 1;

        fs::write(&plan, REWRITTEN_PLAN).expect(WRITTEN);
        app.update(wrote(&plan));

        assert!(rendered(&mut app).contains(REWRITTEN_PLAN), "{STALE_PLAN}");
    }

    /// Inside the workbench the chord belongs to the rendered view, so the
    /// transcript behind it keeps the view it had.
    #[test]
    fn the_view_chord_paints_the_plan_the_way_the_transcript_would() {
        let Planned {
            plan,
            mut app,
            _dirs,
        } = planned(Draft::Written);
        fs::write(&plan, EMPHASIS_SOURCE).expect(WRITTEN);
        open_plan(&mut app);
        let transcript = app.view;

        press_chord(&mut app, leader::VIEW_TOGGLE);

        let frame = rendered(&mut app);
        assert!(frame.contains(EMPHASIS_RENDERED), "{NOT_RENDERED}");
        assert!(!frame.contains(EMPHASIS_SOURCE), "{NOT_RENDERED}");
        assert_eq!(app.view, transcript, "{TRANSCRIPT_TOGGLED}");
    }

    #[test]
    fn the_plan_waits_for_the_policy_source_to_close() {
        let Planned { mut app, _dirs, .. } = planned(Draft::Written);
        app.parked_workbench = Some(Workbench::new(workbench_styles()));

        open_plan(&mut app);

        assert_eq!(
            app.status_bar.flash_text(),
            Some(LOCAL_SOURCE_OPEN),
            "{WRONG_FLASH}"
        );
        assert!(!app.workbench.is_open(), "{SOURCE_REROOTED}");
    }

    #[test]
    fn a_memory_note_opens_under_its_name_and_takes_the_inspector_down() {
        let project = TempDir::new().expect(TEMP_DIR);
        let notes = TempDir::new().expect(TEMP_DIR);
        let note = notes.path().join(NOTE_FILE);
        fs::write(&note, PLAN_TEXT).expect(WRITTEN);
        let mut app = test_app();
        app.state.session_mut().cwd = project.path().to_string_lossy().into_owned();
        app.memory_inspector.open(None, String::new(), true);

        app.open_memory_note(&note);

        assert!(!app.memory_inspector.is_open(), "{INSPECTOR_LEFT_UP}");
        assert_eq!(app.workbench.layout().tabs, [note], "{NOTE_NOT_OPENED}");
        assert!(rendered(&mut app).contains(NOTE_STATUS), "{NOT_LABELLED}");
    }

    #[test]
    fn the_edit_chord_opens_the_prompt_in_the_workbench() {
        let mut app = composing(Composer::Main);

        press_chord(&mut app, leader::EDIT_INPUT);

        assert!(app.workbench.is_open(), "{DRAFT_NOT_OPENED}");
        let frame = rendered(&mut app);
        assert!(frame.contains(DRAFT), "{DRAFT_NOT_OPENED}");
        assert!(frame.contains(MAIN_DRAFT_STATUS), "{NOT_LABELLED}");
    }

    /// The reader came from the transcript, so going back has to hand the
    /// keyboard to the composer rather than wherever it was before.
    #[test_case(Composer::Main, Handback::Save ; "saved in the main chat")]
    #[test_case(Composer::Task, Handback::Save ; "saved in a task")]
    #[test_case(Composer::Main, Handback::Send ; "sent in the main chat")]
    #[test_case(Composer::Task, Handback::Send ; "sent in a task")]
    fn a_saved_draft_reaches_its_composer(composer: Composer, handback: Handback) {
        let mut app = composing(composer);
        app.key_focus = KeyFocus::Transcript;
        edit_draft(&mut app);

        hand_back(&mut app, handback);

        assert_eq!(app.active_input_text(), edited(), "{NOT_APPLIED}");
        match handback {
            Handback::Save => assert!(app.workbench.is_open(), "{LEFT_EARLY}"),
            Handback::Send => {
                assert!(!app.workbench.is_open(), "{NOT_RETURNED}");
                assert_eq!(app.key_focus, KeyFocus::Composer, "{NOT_RETURNED}");
            }
        }
    }

    #[test]
    fn a_draft_waits_for_the_chat_it_came_from() {
        let mut app = composing(Composer::Task);
        let task = app.active_chat;
        edit_draft(&mut app);
        focus_chat(&mut app, 0);

        hand_back(&mut app, Handback::Send);

        assert_eq!(
            app.status_bar.flash_text(),
            Some(DRAFT_OTHER_COMPOSER),
            "{WRONG_FLASH}"
        );
        assert!(app.input_box.is_empty(), "{WRONG_COMPOSER}");
        assert!(app.workbench.is_open(), "{LEFT_EARLY}");

        focus_chat(&mut app, task);
        hand_back(&mut app, Handback::Save);

        assert_eq!(app.active_input_text(), edited(), "{DRAFT_LOST}");
    }

    /// Leaving the workbench forgets a clean draft, which the composer holds
    /// anyway, and keeps one with edits the composer has never seen.
    #[test_case(false ; "a clean draft follows the composer")]
    #[test_case(true ; "an unsaved draft keeps what was typed")]
    fn reopening_the_draft_shows_the_newest_text(unsaved: bool) {
        let mut app = composing(Composer::Main);
        match unsaved {
            true => edit_draft(&mut app),
            false => {
                press_chord(&mut app, leader::EDIT_INPUT);
            }
        }
        app.update(Msg::Key(press(KeyCode::Esc)));
        app.input_box.set_input(NEWER.into());

        press_chord(&mut app, leader::EDIT_INPUT);

        let (shown, hidden) = match unsaved {
            true => (edited(), NEWER),
            false => (NEWER.to_owned(), DRAFT),
        };
        let frame = rendered(&mut app);
        assert!(frame.contains(&shown), "{STALE_DRAFT}");
        assert!(!frame.contains(hidden), "{STALE_DRAFT}");
    }

    /// The draft is left out of the stored layout, so a workbench opened for
    /// it alone would go on to store a layout with no tabs in it.
    #[test]
    fn the_draft_opens_beside_the_stored_tabs() {
        let mut app = composing(Composer::Main);
        let cwd = PathBuf::from(&app.state.session.cwd);
        let kept = cwd.join(STORED_FILE);
        fs::write(&kept, PLAN_TEXT).expect(WRITTEN);
        let stored = WorkbenchLayout {
            tabs: vec![kept.clone()],
            ..WorkbenchLayout::default()
        };
        caudra_storage::workbench::persist(&app.storage, &cwd, &stored);

        press_chord(&mut app, leader::EDIT_INPUT);

        assert!(rendered(&mut app).contains(DRAFT), "{DRAFT_NOT_OPENED}");
        assert_eq!(app.workbench.layout().tabs, [kept], "{LAYOUT_LOST}");
    }

    /// A remote workspace's plan has no path, so it opens from the store by
    /// reference. The store files an empty plan up front, and empty is how
    /// one the agent has not written yet looks there.
    #[test_case("", Some(PLAN_NOT_WRITTEN) ; "a plan not written yet")]
    #[test_case(PLAN_TEXT, None ; "a written plan")]
    fn ctrl_o_opens_a_remote_plan_from_its_store(text: &str, refusal: Option<&str>) {
        let (mut remote, _) = remote_plan(text);
        let app = &mut remote.app;

        open_plan(app);

        assert_eq!(app.status_bar.flash_text(), refusal, "{WRONG_FLASH}");
        assert_eq!(
            app.workbench.is_open(),
            refusal.is_none(),
            "{PLAN_NOT_OPENED}"
        );
        if refusal.is_none() {
            let status =
                app.state.plan.reference().map(|plan| {
                    format!("{PLAN_STATUS_PREFIX}{}", &plan.as_str()[..SHORT_REFERENCE])
                });
            let frame = rendered(app);
            assert!(frame.contains(PLAN_TEXT.trim_end()), "{PLAN_NOT_OPENED}");
            assert!(
                status.is_some_and(|status| frame.contains(&status)),
                "{NOT_LABELLED}"
            );
        }
    }

    #[test]
    fn a_saved_remote_plan_goes_back_to_its_store() {
        let (mut remote, plan) = remote_plan(PLAN_TEXT);
        open_plan(&mut remote.app);
        type_edit(&mut remote.app);

        save(&mut remote.app);

        assert_eq!(
            remote.text(&plan),
            format!("{EDIT}{PLAN_TEXT}"),
            "{NOT_STORED}"
        );
        assert!(
            !remote
                .app
                .workbench
                .has_unsaved_document(&stored_key(&plan)),
            "{NOT_STORED}"
        );
    }

    /// Saving over a write the reader never saw would undo it, so the save
    /// waits until `Ctrl+R` has taken the newer copy into the tab.
    #[test]
    fn a_save_over_a_newer_copy_waits_for_the_reader_to_take_it() {
        let (mut remote, plan) = remote_plan(PLAN_TEXT);
        open_plan(&mut remote.app);
        type_edit(&mut remote.app);
        remote.write(&plan, REWRITTEN_PLAN);

        save(&mut remote.app);

        assert_eq!(
            remote.app.status_bar.flash_text(),
            Some(STORED_CHANGED),
            "{WRONG_FLASH}"
        );
        assert_eq!(remote.text(&plan), REWRITTEN_PLAN, "{OVERWRITTEN}");
        assert!(
            remote
                .app
                .workbench
                .has_unsaved_document(&stored_key(&plan)),
            "{EDITS_LOST}"
        );

        revert(&mut remote.app);
        remote.app.update(Msg::Key(press(KeyCode::Home)));
        type_edit(&mut remote.app);
        save(&mut remote.app);

        assert_eq!(
            remote.text(&plan),
            format!("{EDIT}{REWRITTEN_PLAN}"),
            "{NOT_STORED}"
        );
    }

    /// The write result names the plan and nothing more, so the tab takes the
    /// store's copy, and a later save is held to what the store holds: a clean
    /// tab saves over the rewrite it took, and an unsaved one is refused.
    #[test_case(false ; "a clean tab takes the rewrite")]
    #[test_case(true ; "an unsaved tab keeps its edits")]
    fn an_agent_rewrite_reaches_the_open_remote_plan(unsaved: bool) {
        let (mut remote, plan) = remote_plan(PLAN_TEXT);
        open_plan(&mut remote.app);
        if unsaved {
            type_edit(&mut remote.app);
        }
        remote.app.status = Status::Streaming;
        remote.app.run_id = 1;

        remote.write(&plan, REWRITTEN_PLAN);
        let LocalDocumentRef::Plan(reference) = &plan else {
            unreachable!()
        };
        let written =
            PlanWriteResult::new(PlanTarget::Remote(reference.clone()), REWRITTEN_PLAN.into());
        remote.app.update(done(
            plan::NAME,
            Some(written.annotation().unwrap()),
            Vec::new(),
        ));

        let app = &mut remote.app;
        assert_eq!(
            rendered(app).contains(REWRITTEN_PLAN),
            !unsaved,
            "{STALE_PLAN}"
        );
        assert_eq!(
            app.workbench.has_unsaved_document(&stored_key(&plan)),
            unsaved,
            "{EDITS_LOST}"
        );

        if unsaved {
            save(app);
            assert_eq!(
                app.status_bar.flash_text(),
                Some(STORED_CHANGED),
                "{WRONG_FLASH}"
            );
            assert_eq!(remote.text(&plan), REWRITTEN_PLAN, "{OVERWRITTEN}");
        } else {
            app.update(Msg::Key(press(KeyCode::Home)));
            type_edit(app);
            save(app);
            assert_eq!(
                remote.text(&plan),
                format!("{EDIT}{REWRITTEN_PLAN}"),
                "{NOT_STORED}"
            );
        }
    }

    #[test_case(false; "clean")]
    #[test_case(true; "unsaved")]
    fn native_local_plan_refresh_uses_committed_content_without_file_read(unsaved: bool) {
        let Planned {
            plan: path,
            mut app,
            _dirs,
        } = planned(Draft::Written);
        open_plan(&mut app);
        if unsaved {
            type_edit(&mut app);
        }
        let written = PlanWriteResult::new(PlanTarget::Local(path.clone()), REWRITTEN_PLAN.into());
        fs::remove_file(&path).unwrap();
        app.status = Status::Streaming;
        app.run_id = 1;
        let mut completed = ToolDoneEvent::error(BATCH_PLAN_ID.into(), "");
        completed.tool = plan::NAME.into();
        completed.is_error = false;
        completed.output = ToolOutput::Markdown(TextOutput {
            state: Some(written.annotation().unwrap().into()),
            ..REWRITTEN_PLAN.into()
        });
        app.update(agent_msg(AgentEvent::ToolDone(Box::new(completed))));
        assert_eq!(rendered(&mut app).contains(REWRITTEN_PLAN), !unsaved);
        assert_eq!(app.workbench.has_unsaved(&path), unsaved);
        assert!(!path.exists());
    }

    /// Implements the open plan in place, then delivers the Build run's write
    /// of it. The tab takes the write, while the plan stays in drafting, no
    /// form opens, and no plan card takes the place of the call's own card.
    fn assert_a_build_write_only_refreshes(app: &mut App, target: PlanTarget) {
        assert!(!app.implement_plan(false).is_empty());
        app.state.applied_mode = Mode::Build;
        let plan = app.state.plan.clone();
        let written = PlanWriteResult::new(target, REWRITTEN_PLAN.into());

        app.update(done(
            plan::NAME,
            Some(written.annotation().unwrap()),
            Vec::new(),
        ));

        assert!(rendered(app).contains(REWRITTEN_PLAN), "{STALE_PLAN}");
        assert_eq!(app.state.plan, plan);
        assert!(!app.state.plan.is_ready());
        assert!(!app.plan_form.is_visible());
        assert!(!app.main_chat().last_message_is_plan());
    }

    #[test]
    fn a_build_write_only_refreshes_the_open_local_plan() {
        let Planned {
            plan,
            mut app,
            _dirs,
        } = planned(Draft::Written);
        open_plan(&mut app);
        assert_a_build_write_only_refreshes(&mut app, PlanTarget::Local(plan));
    }

    #[test]
    fn a_build_write_only_refreshes_the_open_remote_plan() {
        let (mut remote, plan) = remote_plan(PLAN_TEXT);
        open_plan(&mut remote.app);
        remote.write(&plan, REWRITTEN_PLAN);
        let LocalDocumentRef::Plan(reference) = plan else {
            unreachable!()
        };
        assert_a_build_write_only_refreshes(&mut remote.app, PlanTarget::Remote(reference));
    }

    #[test_case(false; "clean")]
    #[test_case(true; "unsaved")]
    fn batch_plan_refresh_follows_completion_order_not_roster_order(unsaved: bool) {
        let Planned {
            plan: path,
            mut app,
            _dirs,
        } = planned(Draft::Written);
        open_plan(&mut app);
        if unsaved {
            type_edit(&mut app);
        }
        let entry = |content: &str| BatchToolEntry {
            tool: plan::NAME.into(),
            effect: ToolEffect::Mutating,
            summary: String::new(),
            status: BatchToolStatus::Success,
            input: None,
            raw_input: None,
            output: Some(ToolOutput::Markdown(TextOutput {
                state: Some(
                    PlanWriteResult::new(PlanTarget::Local(path.clone()), content.into())
                        .annotation()
                        .unwrap()
                        .into(),
                ),
                ..content.into()
            })),
            annotation: None,
            model_suffix: None,
            refused: false,
        };
        let earlier = entry(EARLIER_PLAN);
        let later = entry(REWRITTEN_PLAN);
        let output = ToolOutput::Batch {
            entries: vec![later.clone(), earlier.clone()],
            text: String::new(),
        };
        let mut roster = output.clone();
        if let ToolOutput::Batch { entries, .. } = &mut roster {
            for child in entries {
                child.status = BatchToolStatus::Running;
                child.output = None;
            }
        }
        fs::remove_file(&path).unwrap();
        app.status = Status::Streaming;
        app.run_id = 1;
        app.update(agent_msg(AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: BATCH_PLAN_ID.into(),
            tool: BATCH_TOOL_NAME.into(),
            effect: ToolEffect::Orchestrator,
            summary: String::new(),
            annotation: None,
            input: None,
            raw_input: None,
            output: Some(roster),
            render_header: None,
        }))));
        for (index, entry) in [(1, earlier.clone()), (0, later.clone()), (1, earlier)] {
            app.update(agent_msg(AgentEvent::BatchProgress(Box::new(
                BatchProgressEvent {
                    id: BATCH_PLAN_ID.into(),
                    index,
                    entry,
                },
            ))));
        }
        let mut completed = ToolDoneEvent::error(BATCH_PLAN_ID.into(), "");
        completed.tool = BATCH_TOOL_NAME.into();
        completed.is_error = false;
        completed.output = output;
        app.update(agent_msg(AgentEvent::ToolDone(Box::new(completed))));
        assert_eq!(rendered(&mut app).contains(REWRITTEN_PLAN), !unsaved);
        assert_eq!(app.workbench.has_unsaved(&path), unsaved);
        assert!(!path.exists());
        let saved = &app.state.session.tool_outputs()[BATCH_PLAN_ID];
        let restored: ToolOutput =
            serde_json::from_str(&serde_json::to_string(saved).unwrap()).unwrap();
        let ToolOutput::Batch { entries, .. } = restored else {
            unreachable!()
        };
        assert_eq!(entries[0].plan_write_result(), later.plan_write_result());
    }

    #[test]
    fn implementing_waits_for_the_remote_plan_to_be_saved() {
        let (mut remote, plan) = remote_plan(PLAN_TEXT);
        open_plan(&mut remote.app);
        type_edit(&mut remote.app);
        remote.app.workbench.close();

        let app = &mut remote.app;
        app.update(Msg::Key(press(KeyCode::Down)));
        app.update(Msg::Key(press(KeyCode::Down)));
        let actions = app.update(Msg::Key(press(KeyCode::Enter)));

        assert!(actions.is_empty(), "{IMPLEMENTED_UNSAVED}");
        assert_eq!(
            app.status_bar.flash_text(),
            Some(PLAN_UNSAVED),
            "{WRONG_FLASH}"
        );
        assert_eq!(app.state.mode, Mode::Plan, "{IMPLEMENTED_UNSAVED}");
        assert!(app.workbench.is_open(), "{PLAN_NOT_OPENED}");
        assert!(
            app.workbench.has_unsaved_document(&stored_key(&plan)),
            "{EDITS_LOST}"
        );
    }

    #[test_case(false ; "implement")]
    #[test_case(true ; "clear_and_implement")]
    fn remote_plan_handoff_carries_content_across_sessions(clear: bool) {
        let (mut remote, plan) = remote_plan(PLAN_TEXT);
        let owner = remote.session();
        let bound = remote.app.state.plan.clone();
        remote.write(&plan, REWRITTEN_PLAN);
        let actions = remote.app.implement_plan(clear);
        assert_eq!(
            matches!(actions.first(), Some(Action::ClearAndImplement(_))),
            clear
        );
        let actions = if clear {
            let Action::ClearAndImplement(handoff) = actions.into_iter().next().unwrap() else {
                panic!("expected captured plan handoff");
            };
            assert!(remote.app.plan_form.is_visible());
            remote.app.reset_session_for_plan().unwrap();
            remote.app.adopt_plan(&handoff);
            remote.app.finish_plan_handoff(*handoff)
        } else {
            actions
        };
        let input = sent_input(&actions);
        assert_eq!(input.mode, AgentMode::Build);
        assert!(input.message.ends_with(REWRITTEN_PLAN));
        assert!(input.message.contains(plan::SESSION_PLAN_LABEL));
        if let LocalDocumentRef::Plan(reference) = &plan {
            assert!(!input.message.contains(reference.as_str()));
        }
        assert!(!input.message.contains(&owner));
        if clear {
            assert_ne!(remote.session(), owner);
            assert!(
                remote
                    .store
                    .read(remote.store.project_key(), Some(&remote.session()), &plan,)
                    .is_err()
            );
            let PlanState::RemoteDrafting(copy) = &remote.app.state.plan else {
                panic!("{PLAN_NOT_ADOPTED}");
            };
            let copy = LocalDocumentRef::Plan(copy.clone());
            assert_ne!(copy, plan, "{PLAN_NOT_ADOPTED}");
            assert_eq!(remote.text(&copy), REWRITTEN_PLAN, "{PLAN_NOT_ADOPTED}");
        } else {
            let mut kept = bound;
            kept.mark_drafting();
            assert_eq!(remote.app.state.plan, kept);
        }
        assert_eq!(input.plan, remote.app.state.plan.target());
    }

    /// Without a copy the new session has no plan, but the work still starts:
    /// its message holds the plan.
    #[test_case(false ; "store_unavailable")]
    #[test_case(true ; "copy_refused")]
    fn a_failed_remote_plan_copy_still_starts_the_implementation(refused: bool) {
        let (mut remote, _) = remote_plan(PLAN_TEXT);
        let Some(Action::ClearAndImplement(handoff)) =
            remote.app.implement_plan(true).into_iter().next()
        else {
            panic!("expected captured plan handoff");
        };
        remote.app.reset_session_for_plan().unwrap();
        let temp = private_tempdir();
        let blocked = temp.path().join(BLOCKED_STATE);
        fs::write(&blocked, PLAN_TEXT).expect(WRITTEN);
        remote.app.local_documents = refused.then(|| {
            let workspace = remote.app.workspace_session.as_ref().expect(REMOTE_SESSION);
            Arc::new(LocalDocumentStore::remote(
                StateDir::from_path(blocked),
                workspace.binding(),
            ))
        });

        remote.app.adopt_plan(&handoff);
        let actions = remote.app.finish_plan_handoff(*handoff);

        let input = sent_input(&actions);
        assert!(input.message.ends_with(PLAN_TEXT));
        assert_eq!(input.plan, None);
        assert_eq!(remote.app.state.plan, PlanState::None);
        assert_eq!(remote.app.state.mode, Mode::Build);
        assert_eq!(remote.app.status, Status::Streaming);
        assert!(
            remote
                .app
                .status_bar
                .flash_text()
                .is_some_and(|text| text.starts_with(PLAN_COPY_FAILED)),
            "{WRONG_FLASH}"
        );
    }

    #[test_case(false ; "implement")]
    #[test_case(true ; "clear_and_implement")]
    fn unavailable_remote_plan_handoff_preserves_plan(clear: bool) {
        let (mut remote, _) = remote_plan(PLAN_TEXT);
        let plan = remote.app.state.plan.clone();
        let session = remote.session();
        remote.app.local_documents = None;
        assert!(remote.app.implement_plan(clear).is_empty());
        assert_eq!(remote.app.state.plan, plan);
        assert_eq!(remote.app.state.mode, Mode::Plan);
        assert_eq!(remote.session(), session);
        assert!(remote.app.plan_form.is_visible());
        assert!(remote.app.status_bar.flash_text().is_some());
    }

    /// A remote note has no file to open, so `/memory` opens the store's copy
    /// under the note's name, and a save puts it back there.
    #[test]
    fn a_remote_note_opens_from_the_inspector_and_saves_to_its_store() {
        let mut remote = remote();
        let note = LocalDocumentRef::Memory(remote.note(PLAN_TEXT));
        remote.app.open_memory_inspector();
        remote.app.await_memory_reply();

        remote
            .app
            .handle_memory_action(MemoryAction::Open(NOTE_FILE.to_owned()));

        assert!(
            !remote.app.memory_inspector.is_open(),
            "{INSPECTOR_LEFT_UP}"
        );
        assert!(
            rendered(&mut remote.app).contains(NOTE_STATUS),
            "{NOT_LABELLED}"
        );
        type_edit(&mut remote.app);
        save(&mut remote.app);
        assert_eq!(
            remote.text(&note),
            format!("{EDIT}{PLAN_TEXT}"),
            "{NOT_STORED}"
        );
    }

    /// The `memory` tool names a note by its name, not its reference, so any
    /// call of it is a reason to look at the notes that are open.
    #[test]
    fn the_memory_tool_rewriting_an_open_note_reaches_its_tab() {
        let mut remote = remote();
        let note = remote.note(PLAN_TEXT);
        remote.app.open_stored_note(note);
        remote.app.status = Status::Streaming;
        remote.app.run_id = 1;

        remote.note(REWRITTEN_PLAN);
        remote.app.update(done(MEMORY_TOOL_NAME, None, Vec::new()));

        assert!(
            rendered(&mut remote.app).contains(REWRITTEN_PLAN),
            "{STALE_NOTE}"
        );
    }
}
