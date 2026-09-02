//! `/memory`: browsing the project's persistent notes.
//!
//! The notes themselves belong to the `memory` tool; this is only the picker's
//! side of the conversation. Every list is rebuilt from disk rather than
//! cached here, because the model writes notes mid-session and the user edits
//! them in an external editor.

use std::path::PathBuf;

use caudra_agent::tools::native::memory;

use super::App;
use crate::components::Action;
use crate::components::memory_picker::MemoryPickerAction;
use crate::repaint::Dirty;

const UNRESOLVED: &str = "Cannot resolve memory directory";
const EMPTY: &str = "No memories yet";
const NOTE_GONE: &str = "Note is already gone";

impl App {
    pub(super) fn memory_browse(&mut self) -> Vec<Action> {
        let Some((_, entries, unreadable)) = self.memory_notes() else {
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

    fn memory_notes(&self) -> Option<(PathBuf, Vec<memory::BrowseEntry>, usize)> {
        memory::browse(std::path::Path::new(&self.state.session.cwd))
    }

    /// Re-reads the notes directory behind an open picker. The list closes
    /// when the last note goes, so an empty picker never lingers.
    fn memory_refresh(&mut self) {
        let Some((_, entries, _)) = self.memory_notes() else {
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
        let (dir, ..) = self.memory_notes()?;
        memory::paths::safe_resolve(&dir, name).ok()
    }
}
