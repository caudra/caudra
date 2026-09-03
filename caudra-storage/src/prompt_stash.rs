//! Unsent prompt drafts parked for later, shared across every session and
//! project. A volatile state row: an ephemeral run keeps its own stash and
//! never touches the persistent one. Each mutation is one write transaction
//! that re-reads first, so two Caudra processes stashing at once cannot
//! clobber each other.

use serde::{Deserialize, Serialize};

use crate::id::CaudraId;
use crate::sessions::{StoredImage, StoredPasteRange};
use crate::state::{SCOPE_GLOBAL, StateKey, StateStore};
use crate::{StateClass, StateDir, StorageError, now_epoch};

pub const MAX_ENTRIES: usize = 50;

const STASH: StateKey = StateKey {
    name: "input.stash",
    class: StateClass::Volatile,
};
#[derive(Debug, thiserror::Error)]
pub enum PromptStashError {
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// The composer contents a stash entry is built from. Mirrors the draft fields
/// a session already persists, so a stashed prompt restores its paste tokens
/// and images the same way resuming a session does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StashDraft {
    pub text: String,
    pub paste_ranges: Vec<StoredPasteRange>,
    pub images: Vec<StoredImage>,
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashEntry {
    pub id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paste_ranges: Vec<StoredPasteRange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<StoredImage>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd: String,
    pub created_at: u64,
}

/// Oldest first, so the newest entry is the last one and `pop` is a
/// truncation rather than a shift.
pub struct PromptStash {
    store: StateStore,
    entries: Vec<StashEntry>,
}

impl PromptStash {
    pub fn open(state_dir: &StateDir) -> Result<Self, PromptStashError> {
        let store = StateStore::open(state_dir, STASH.class)?;
        let entries = store.get(SCOPE_GLOBAL, STASH)?.unwrap_or_default();
        Ok(Self { store, entries })
    }

    /// Another process may have stashed since this handle was opened, so
    /// anything that shows or consumes entries re-reads first.
    pub fn refresh(&mut self) -> Result<(), PromptStashError> {
        self.entries = self.store.get(SCOPE_GLOBAL, STASH)?.unwrap_or_default();
        Ok(())
    }

    pub fn entries(&self) -> &[StashEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn push(&mut self, draft: StashDraft) -> Result<(), PromptStashError> {
        self.mutate(|entries| {
            entries.push(StashEntry {
                id: CaudraId::generate().to_string(),
                text: draft.text,
                paste_ranges: draft.paste_ranges,
                images: draft.images,
                cwd: draft.cwd,
                created_at: now_epoch(),
            });
            let overflow = entries.len().saturating_sub(MAX_ENTRIES);
            entries.drain(..overflow);
        })
    }

    pub fn pop(&mut self) -> Result<Option<StashEntry>, PromptStashError> {
        self.mutate(Vec::pop)
    }

    pub fn remove(&mut self, id: &str) -> Result<Option<StashEntry>, PromptStashError> {
        self.mutate(|entries| {
            let index = entries.iter().position(|entry| entry.id == id)?;
            Some(entries.remove(index))
        })
    }

    fn mutate<R>(
        &mut self,
        update: impl FnOnce(&mut Vec<StashEntry>) -> R,
    ) -> Result<R, PromptStashError> {
        let (result, entries) =
            self.store
                .update(SCOPE_GLOBAL, STASH, |entries: &mut Vec<StashEntry>| {
                    (update(entries), entries.clone())
                })?;
        self.entries = entries;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CWD: &str = "/tmp/project";

    fn tmp_dir() -> (tempfile::TempDir, StateDir) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        (tmp, dir)
    }

    fn draft(text: &str) -> StashDraft {
        StashDraft {
            text: text.into(),
            cwd: CWD.into(),
            ..Default::default()
        }
    }

    #[test]
    fn push_and_pop_are_last_in_first_out() {
        let (_tmp, dir) = tmp_dir();
        let mut stash = PromptStash::open(&dir).unwrap();
        stash.push(draft("first")).unwrap();
        stash.push(draft("second")).unwrap();

        let mut reopened = PromptStash::open(&dir).unwrap();
        assert_eq!(reopened.len(), 2);
        assert_eq!(reopened.entries()[0].text, "first");
        assert_eq!(reopened.pop().unwrap().unwrap().text, "second");
        assert_eq!(reopened.pop().unwrap().unwrap().text, "first");
        assert!(reopened.pop().unwrap().is_none());
    }

    #[test]
    fn paste_ranges_and_images_round_trip() {
        let (_tmp, dir) = tmp_dir();
        let mut stash = PromptStash::open(&dir).unwrap();
        stash
            .push(StashDraft {
                text: "look at PASTED here".into(),
                paste_ranges: vec![StoredPasteRange { start: 8, end: 14 }],
                images: vec![StoredImage {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                }],
                cwd: CWD.into(),
            })
            .unwrap();

        let entry = PromptStash::open(&dir).unwrap().pop().unwrap().unwrap();
        assert_eq!(
            entry.paste_ranges,
            vec![StoredPasteRange { start: 8, end: 14 }]
        );
        assert_eq!(entry.images.len(), 1);
        assert_eq!(entry.images[0].media_type, "image/png");
        assert_eq!(entry.cwd, CWD);
    }

    #[test]
    fn oldest_entries_drop_past_the_cap() {
        let (_tmp, dir) = tmp_dir();
        let mut stash = PromptStash::open(&dir).unwrap();
        for index in 0..MAX_ENTRIES + 5 {
            stash.push(draft(&format!("entry{index}"))).unwrap();
        }
        assert_eq!(stash.len(), MAX_ENTRIES);
        assert_eq!(stash.entries()[0].text, "entry5");
        assert_eq!(
            stash.entries()[MAX_ENTRIES - 1].text,
            format!("entry{}", MAX_ENTRIES + 4)
        );
    }

    #[test]
    fn remove_targets_the_matching_id() {
        let (_tmp, dir) = tmp_dir();
        let mut stash = PromptStash::open(&dir).unwrap();
        stash.push(draft("keep")).unwrap();
        stash.push(draft("drop")).unwrap();
        let id = stash.entries()[1].id.clone();

        assert_eq!(stash.remove(&id).unwrap().unwrap().text, "drop");
        assert!(stash.remove(&id).unwrap().is_none());
        assert_eq!(stash.len(), 1);
        assert_eq!(stash.entries()[0].text, "keep");
    }

    #[test]
    fn a_second_handle_sees_writes_from_the_first() {
        let (_tmp, dir) = tmp_dir();
        let mut writer = PromptStash::open(&dir).unwrap();
        let mut reader = PromptStash::open(&dir).unwrap();
        writer.push(draft("from writer")).unwrap();

        assert!(reader.is_empty());
        reader.refresh().unwrap();
        assert_eq!(reader.entries()[0].text, "from writer");
    }

    #[test]
    fn concurrent_pushes_do_not_clobber_each_other() {
        let (_tmp, dir) = tmp_dir();
        let mut first = PromptStash::open(&dir).unwrap();
        let mut second = PromptStash::open(&dir).unwrap();
        first.push(draft("first")).unwrap();
        second.push(draft("second")).unwrap();

        let stash = PromptStash::open(&dir).unwrap();
        assert_eq!(stash.len(), 2);
        assert_eq!(stash.entries()[0].text, "first");
        assert_eq!(stash.entries()[1].text, "second");
    }

    #[test]
    fn missing_state_opens_empty() {
        let (_tmp, dir) = tmp_dir();
        assert!(PromptStash::open(&dir).unwrap().is_empty());
    }

    #[test]
    fn ephemeral_stash_never_reaches_the_persistent_root() {
        let tmp = tempfile::tempdir().unwrap();
        let persistent = StateDir::from_path(tmp.path().join("persistent"));
        let ephemeral = StateDir::split(tmp.path().join("volatile"), persistent.path().into());
        PromptStash::open(&ephemeral)
            .unwrap()
            .push(draft("secret"))
            .unwrap();

        assert!(PromptStash::open(&persistent).unwrap().is_empty());
        assert_eq!(PromptStash::open(&ephemeral).unwrap().len(), 1);
    }
}
