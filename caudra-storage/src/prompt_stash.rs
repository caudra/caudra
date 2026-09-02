//! Unsent prompt drafts parked for later, shared across every session and
//! project. Each mutation takes the lock, re-reads, and rewrites atomically,
//! so two Caudra processes stashing at once cannot clobber each other.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::id::CaudraId;
use crate::sessions::{StoredImage, StoredPasteRange};
use crate::{StateDir, atomic_write_permissions, exclusive_state_lock, now_epoch};

pub const STASH_FILE: &str = "prompt-stash.json";
pub const MAX_ENTRIES: usize = 50;

const STASH_VERSION: u32 = 1;
const STASH_MODE: u32 = 0o600;
const STASH_LOCK_FILE: &str = "prompt-stash.lock";

#[derive(Debug, thiserror::Error)]
pub enum PromptStashError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("prompt stash version {found} is not supported (expected {expected})")]
    Version { found: u32, expected: u32 },
    #[error("prompt stash file disappeared")]
    Vanished,
}

impl From<crate::StorageError> for PromptStashError {
    fn from(error: crate::StorageError) -> Self {
        match error {
            crate::StorageError::Io(error) => Self::Io(error),
            crate::StorageError::Json(error) => Self::Json(error),
            other => Self::Io(std::io::Error::other(other.to_string())),
        }
    }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StashFile {
    version: u32,
    entries: Vec<StashEntry>,
}

/// Oldest first on disk, so the newest entry is the last one and `pop` is a
/// truncation rather than a shift.
pub struct PromptStash {
    path: PathBuf,
    existed: bool,
    file: StashFile,
}

impl PromptStash {
    pub fn open(state_dir: &StateDir) -> Result<Self, PromptStashError> {
        let path = state_dir.path().join(STASH_FILE);
        let (existed, file) = match load_file(&path)? {
            Some(file) => (true, file),
            None => (
                false,
                StashFile {
                    version: STASH_VERSION,
                    entries: Vec::new(),
                },
            ),
        };
        Ok(Self {
            path,
            existed,
            file,
        })
    }

    /// Another process may have stashed since this handle was opened, so
    /// anything that shows or consumes entries re-reads first.
    pub fn refresh(&mut self) -> Result<(), PromptStashError> {
        match load_file(&self.path)? {
            Some(file) => {
                self.file = file;
                self.existed = true;
                Ok(())
            }
            None if !self.existed => Ok(()),
            None => Err(PromptStashError::Vanished),
        }
    }

    pub fn entries(&self) -> &[StashEntry] {
        &self.file.entries
    }

    pub fn len(&self) -> usize {
        self.file.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.file.entries.is_empty()
    }

    pub fn push(&mut self, draft: StashDraft) -> Result<(), PromptStashError> {
        let _lock = self.lock()?;
        self.refresh()?;
        let mut next = self.file.clone();
        next.entries.push(StashEntry {
            id: CaudraId::generate().to_string(),
            text: draft.text,
            paste_ranges: draft.paste_ranges,
            images: draft.images,
            cwd: draft.cwd,
            created_at: now_epoch(),
        });
        let overflow = next.entries.len().saturating_sub(MAX_ENTRIES);
        next.entries.drain(..overflow);
        self.write(next)
    }

    pub fn pop(&mut self) -> Result<Option<StashEntry>, PromptStashError> {
        let _lock = self.lock()?;
        self.refresh()?;
        let mut next = self.file.clone();
        let Some(entry) = next.entries.pop() else {
            return Ok(None);
        };
        self.write(next)?;
        Ok(Some(entry))
    }

    pub fn remove(&mut self, id: &str) -> Result<Option<StashEntry>, PromptStashError> {
        let _lock = self.lock()?;
        self.refresh()?;
        let Some(index) = self.file.entries.iter().position(|entry| entry.id == id) else {
            return Ok(None);
        };
        let mut next = self.file.clone();
        let entry = next.entries.remove(index);
        self.write(next)?;
        Ok(Some(entry))
    }

    fn lock(&self) -> Result<fs::File, PromptStashError> {
        let lock_path = self
            .path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(STASH_LOCK_FILE);
        Ok(exclusive_state_lock(&lock_path, STASH_MODE)?)
    }

    fn write(&mut self, next: StashFile) -> Result<(), PromptStashError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let data = serde_json::to_vec_pretty(&next)?;
        atomic_write_permissions(&self.path, &data, STASH_MODE)?;
        self.file = next;
        self.existed = true;
        Ok(())
    }
}

fn load_file(path: &Path) -> Result<Option<StashFile>, PromptStashError> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let file: StashFile = serde_json::from_slice(&data)?;
    if file.version != STASH_VERSION {
        return Err(PromptStashError::Version {
            found: file.version,
            expected: STASH_VERSION,
        });
    }
    Ok(Some(file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

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
    fn missing_file_opens_empty() {
        let (_tmp, dir) = tmp_dir();
        assert!(PromptStash::open(&dir).unwrap().is_empty());
    }

    #[test_case(b"not json" as &[u8] ; "corrupt")]
    #[test_case(br#"{"version":99,"entries":[]}"# ; "future_version")]
    fn unreadable_file_is_reported_not_overwritten(content: &[u8]) {
        let (_tmp, dir) = tmp_dir();
        let path = dir.path().join(STASH_FILE);
        fs::write(&path, content).unwrap();

        assert!(PromptStash::open(&dir).is_err());
        assert_eq!(fs::read(&path).unwrap(), content);
    }

    #[cfg(unix)]
    #[test]
    fn stash_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let (_tmp, dir) = tmp_dir();
        let mut stash = PromptStash::open(&dir).unwrap();
        stash.push(draft("secret")).unwrap();

        let mode = fs::metadata(dir.path().join(STASH_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, STASH_MODE);
    }
}
