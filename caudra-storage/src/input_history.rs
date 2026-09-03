//! Prompts the user sent, for up-arrow recall. A volatile state row: an
//! ephemeral run starts from the persistent history but writes its own.

use std::collections::VecDeque;

use crate::state::{self, SCOPE_GLOBAL, StateKey};
use crate::{StateClass, StateDir, StorageError};

const HISTORY: StateKey = StateKey {
    name: "input.history",
    class: StateClass::Volatile,
};
pub const MAX_ENTRIES: usize = 100;

#[derive(Debug)]
pub struct InputHistory {
    entries: VecDeque<String>,
    max_entries: usize,
    storage: Option<StateDir>,
}

impl Default for InputHistory {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            max_entries: MAX_ENTRIES,
            storage: None,
        }
    }
}

impl InputHistory {
    pub fn load(dir: &StateDir, max_entries: usize) -> Self {
        let items = stored_entries(dir).unwrap_or_default();
        let mut history = Self {
            entries: VecDeque::with_capacity(max_entries),
            max_entries,
            storage: Some(dir.clone()),
        };
        for entry in items {
            let _ = history.push_inner(entry);
        }
        history
    }

    pub fn save(&self, dir: &StateDir) -> Result<(), StorageError> {
        state::set(dir, SCOPE_GLOBAL, HISTORY, &self.entries)
    }

    pub fn push(&mut self, entry: String) {
        let trimmed = entry.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        if self.push_inner(trimmed)
            && let Some(storage) = &self.storage
            && let Err(error) = self.save(storage)
        {
            tracing::warn!(%error, "input history save failed");
        }
    }

    fn push_inner(&mut self, entry: String) -> bool {
        if self.entries.back().is_some_and(|last| *last == entry) {
            return false;
        }
        if self.entries.len() == self.max_entries {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
        true
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&str> {
        self.entries.get(index).map(String::as_str)
    }
}

/// The volatile row, then the persistent one during an ephemeral run, so
/// recall works from the first prompt without writing anything back.
fn stored_entries(dir: &StateDir) -> Result<Vec<String>, StorageError> {
    if let Some(entries) = state::get::<Vec<String>>(dir, SCOPE_GLOBAL, HISTORY)? {
        return Ok(entries);
    }
    let persistent = dir.for_class(StateClass::Persistent);
    Ok(state::get(&persistent, SCOPE_GLOBAL, HISTORY)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> (tempfile::TempDir, StateDir) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        (tmp, dir)
    }

    #[test]
    fn push_persists_immediately() {
        let (_tmp, dir) = tmp_dir();
        let mut history = InputHistory::load(&dir, MAX_ENTRIES);
        history.push("a".into());
        history.push("b".into());
        history.push("c".into());
        let loaded = InputHistory::load(&dir, MAX_ENTRIES);
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded.get(0), Some("a"));
        assert_eq!(loaded.get(2), Some("c"));
    }

    #[test]
    fn truncates_to_max_entries() {
        let mut history = InputHistory::default();
        for i in 0..150 {
            history.push(format!("entry{i}"));
        }
        assert_eq!(history.len(), MAX_ENTRIES);
        assert_eq!(history.get(0), Some("entry50"));
        assert_eq!(history.get(MAX_ENTRIES - 1), Some("entry149"));
    }

    #[test]
    fn rejects_consecutive_duplicates() {
        let mut history = InputHistory::default();
        history.push("a".into());
        history.push("a".into());
        history.push("b".into());
        history.push("b".into());
        history.push("a".into());
        assert_eq!(history.len(), 3);
        assert_eq!(history.get(0), Some("a"));
        assert_eq!(history.get(1), Some("b"));
        assert_eq!(history.get(2), Some("a"));
    }

    #[test]
    fn push_trims_and_rejects_blank() {
        let mut history = InputHistory::default();
        history.push("".into());
        history.push("   ".into());
        history.push("\n".into());
        assert!(history.is_empty());

        history.push("  hello  ".into());
        assert_eq!(history.get(0), Some("hello"));
    }

    #[test]
    fn ephemeral_run_reads_persistent_history_and_writes_its_own() {
        let tmp = tempfile::tempdir().unwrap();
        let persistent = StateDir::from_path(tmp.path().join("persistent"));
        let mut history = InputHistory::load(&persistent, MAX_ENTRIES);
        history.push("kept".into());
        let ephemeral = StateDir::split(tmp.path().join("volatile"), persistent.path().into());

        let mut seeded = InputHistory::load(&ephemeral, MAX_ENTRIES);
        assert_eq!(seeded.get(0), Some("kept"));
        seeded.push("secret".into());

        assert_eq!(InputHistory::load(&ephemeral, MAX_ENTRIES).len(), 2);
        assert_eq!(InputHistory::load(&persistent, MAX_ENTRIES).len(), 1);
    }
}
