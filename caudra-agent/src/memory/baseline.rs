//! The memory view a session's system prompt carries, pinned.
//!
//! Notes change while a session runs, in this session and in others. Taking
//! the view again on every turn would re-cache the whole conversation each
//! time anyone saved a note, so the view stays put and what other sessions
//! wrote since arrives as a standing reminder. The view is taken again only
//! where the prompt is cold anyway: at runtime start, on a change of
//! directory, and when a new compaction summary heads the conversation.

use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use caudra_providers::{CaudraId, HistoryItem};
use caudra_storage::StateDir;
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_storage::memory_journal::{EntryMeta, EntryOrigin};
use tracing::warn;

use super::store::{MemoryStore, leaf_kind};
use super::tree::{Block, Part};
use crate::agent::{History, last_announced};
use crate::prompt::{
    ENTRIES_SLOT, MEMORY_REFRESHED_PROMPT, MEMORY_UPDATED_MARKER, MEMORY_UPDATED_PROMPT,
};

const MAX_ROWS: usize = 20;
const MAX_ROWS_BYTES: usize = 4 * 1024;

#[derive(Clone, Default)]
pub struct MemoryBaseline {
    store: Option<Arc<MemoryStore>>,
    view: Option<String>,
    /// How many entries the view accounts for.
    through: u64,
    key: Option<CaudraId>,
}

impl MemoryBaseline {
    /// Takes the store's view as it is now. Reads the disk and the database.
    pub fn adopt(store: Option<Arc<MemoryStore>>, key: Option<CaudraId>) -> Self {
        let block = store.as_deref().and_then(|store| {
            if let Err(error) = store.reconcile() {
                warn!(scope = store.scope(), %error, "memory notes not reconciled");
            }
            store
                .view()
                .inspect_err(
                    |error| warn!(scope = store.scope(), %error, "memory view unavailable"),
                )
                .ok()
        });
        Self {
            through: block.as_ref().map_or(0, Block::through),
            view: block.as_ref().map(Block::render),
            store,
            key,
        }
    }

    pub fn view(&self) -> Option<&str> {
        self.view.as_deref()
    }

    pub fn store(&self) -> Option<&Arc<MemoryStore>> {
        self.store.as_ref()
    }

    /// The `# Memory updated` reminder this turn carries: the entries other
    /// sessions wrote since the view was taken, the withdrawal of an earlier
    /// reminder once the view covers it, or `None`. Takes the view again
    /// first when `key` moved. `announced` is the transcript's last memory
    /// reminder. Reads the disk and the database.
    pub fn drift(
        &mut self,
        key: Option<CaudraId>,
        session: Option<&str>,
        announced: Option<&str>,
    ) -> Option<String> {
        if key != self.key {
            *self = Self::adopt(self.store.take(), key);
        } else if let Some(store) = &self.store
            && let Err(error) = store.reconcile()
        {
            warn!(scope = store.scope(), %error, "memory notes not reconciled");
        }
        let store = self.store.as_deref()?;
        let foreign: Vec<EntryMeta> = store
            .journal()
            .entries(store.scope(), self.through)
            .inspect_err(|error| warn!(scope = store.scope(), %error, "memory entries unreadable"))
            .ok()?
            .into_iter()
            .filter(|meta| {
                !matches!(&meta.origin, EntryOrigin::Session(id) if Some(id.as_str()) == session)
            })
            .collect();
        if foreign.is_empty() {
            return announced
                .is_some_and(|text| text != MEMORY_REFRESHED_PROMPT)
                .then(|| MEMORY_REFRESHED_PROMPT.to_owned());
        }
        Some(MEMORY_UPDATED_PROMPT.replace(ENTRIES_SLOT, &rows(&foreign)))
    }

    /// Opens a runtime's memory and takes its view, off the executor.
    pub async fn open(
        documents: Option<Arc<LocalDocumentStore>>,
        state: Option<StateDir>,
        cwd: PathBuf,
        history: &History,
    ) -> Self {
        let key = compaction_key(history.active_items());
        smol::unblock(move || {
            Self::adopt(open_store(documents.as_ref(), state.as_ref(), &cwd), key)
        })
        .await
    }

    /// [`Self::drift`] for the turn about to run on `history`, off the
    /// executor. A turn dropped while it runs keeps the baseline it had.
    pub async fn refresh(&mut self, history: &History, session: Option<&str>) -> Option<String> {
        let key = compaction_key(history.active_items());
        let announced =
            last_announced(history.as_slice(), &[MEMORY_UPDATED_MARKER]).map(str::to_owned);
        let session = session.map(str::to_owned);
        let mut next = self.clone();
        let (next, notice) = smol::unblock(move || {
            let notice = next.drift(key, session.as_deref(), announced.as_deref());
            (next, notice)
        })
        .await;
        *self = next;
        notice
    }
}

/// A runtime's memory: a remote session's, kept by its document store, or
/// else the project's at `cwd`, under the runtime's own state directory so a
/// test runtime never opens the user's. `None`, logged when it cannot be
/// opened, and for a local runtime without a state directory.
pub fn open_store(
    documents: Option<&Arc<LocalDocumentStore>>,
    state: Option<&StateDir>,
    cwd: &Path,
) -> Option<Arc<MemoryStore>> {
    match (documents, state) {
        (Some(documents), _) => MemoryStore::for_documents(documents),
        (None, Some(state)) => MemoryStore::for_cwd(state, cwd),
        (None, None) => return None,
    }
    .inspect_err(|error| warn!(%error, "project memory unavailable"))
    .ok()
}

/// What takes the view again: the compaction summary heading the
/// conversation. Appends and rewinds within it keep the view.
pub fn compaction_key(items: &[HistoryItem]) -> Option<CaudraId> {
    items
        .first()
        .filter(|item| item.supersedes.is_some())
        .map(|item| item.id)
}

/// One row per entry, newest kept when they do not all fit. Rows carry no
/// summaries, which change as the compactor works, so the reminder stays the
/// same until another entry arrives.
fn rows(entries: &[EntryMeta]) -> String {
    let mut rows: Vec<String> = Vec::new();
    let mut bytes = 0;
    for meta in entries.iter().rev().take(MAX_ROWS) {
        let mut row = format!(
            "- {} {} {}",
            Part::leaf(meta.seq),
            leaf_kind(meta).as_str(),
            meta.name
        );
        if !meta.heading.is_empty() {
            let _ = write!(row, ": {}", meta.heading);
        }
        bytes += row.len() + 1;
        if bytes > MAX_ROWS_BYTES {
            break;
        }
        rows.push(row);
    }
    let omitted = entries.len() - rows.len();
    if omitted > 0 {
        rows.push(format!(
            "- and {omitted} earlier ones: `memory` with `command=\"view\"` shows the current view"
        ));
    }
    rows.reverse();
    rows.join("\n")
}

#[cfg(test)]
mod tests {
    use caudra_providers::Message;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::memory::store::Source;

    const SCOPE: &str = "projects/test";
    const OWN: &str = "own-session";
    const OTHER: &str = "other-session";
    const BODY: &str = "# Heading\nbody\n";
    const FIRST: &str = "first";
    const SUMMARY: &str = "summary";

    struct Fixture {
        _state: TempDir,
        store: Arc<MemoryStore>,
    }

    fn fixture() -> Fixture {
        let state = TempDir::new().unwrap();
        let store = MemoryStore::open(
            &StateDir::from_path(state.path().to_path_buf()),
            SCOPE.to_owned(),
            Source::Local(state.path().join("memories")),
        )
        .unwrap();
        Fixture {
            _state: state,
            store: Arc::new(store),
        }
    }

    impl Fixture {
        fn write(&self, name: &str, session: &str) {
            self.store
                .write(name, BODY, &EntryOrigin::Session(session.to_owned()))
                .unwrap();
        }
    }

    fn compacted() -> Option<CaudraId> {
        let mut history = History::new(vec![Message::user(FIRST.into())]);
        let seam = history.item_at_message_boundary(1);
        history.replace_superseding(vec![Message::user(SUMMARY.into())], seam);
        compaction_key(history.active_items())
    }

    #[test]
    fn the_view_stays_while_sessions_write_notes() {
        let fixture = fixture();
        fixture.write("old.md", OTHER);
        let mut baseline = MemoryBaseline::adopt(Some(Arc::clone(&fixture.store)), None);
        let view = baseline.view().unwrap().to_owned();

        fixture.write("mine.md", OWN);
        fixture.write("theirs.md", OTHER);
        let notice = baseline.drift(None, Some(OWN), None).unwrap();

        assert_eq!(baseline.view(), Some(view.as_str()));
        assert!(notice.contains("- 2+1 note theirs.md: Heading"));
        assert!(!notice.contains("mine.md"));
        assert_eq!(baseline.drift(None, Some(OWN), Some(&notice)), Some(notice));
    }

    #[test]
    fn own_notes_alone_need_no_reminder() {
        let fixture = fixture();
        let mut baseline = MemoryBaseline::adopt(Some(Arc::clone(&fixture.store)), None);

        fixture.write("mine.md", OWN);

        assert_eq!(baseline.drift(None, Some(OWN), None), None);
    }

    #[test]
    fn a_new_compaction_takes_the_view_again_and_withdraws_the_reminder() {
        let fixture = fixture();
        let mut baseline = MemoryBaseline::adopt(Some(Arc::clone(&fixture.store)), None);
        fixture.write("theirs.md", OTHER);
        let notice = baseline.drift(None, Some(OWN), None).unwrap();

        let key = compacted();
        let withdrawal = baseline.drift(key, Some(OWN), Some(&notice));

        assert!(key.is_some());
        assert_eq!(withdrawal.as_deref(), Some(MEMORY_REFRESHED_PROMPT));
        assert!(baseline.view().unwrap().contains("0+1|note theirs.md"));
        assert_eq!(
            baseline.drift(key, Some(OWN), Some(MEMORY_REFRESHED_PROMPT)),
            None
        );
    }

    #[test]
    fn without_a_store_there_is_no_view_or_reminder() {
        let mut baseline = MemoryBaseline::adopt(None, None);

        assert_eq!(baseline.view(), None);
        assert_eq!(baseline.drift(compacted(), None, None), None);
    }

    #[test_case(3, 3, false ; "all_listed")]
    #[test_case(MAX_ROWS + 5, MAX_ROWS, true ; "newest_listed")]
    fn rows_are_capped(entries: usize, listed: usize, omitted: bool) {
        let fixture = fixture();
        let mut baseline = MemoryBaseline::adopt(Some(Arc::clone(&fixture.store)), None);
        for index in 0..entries {
            fixture.write(&format!("note-{index}.md"), OTHER);
        }

        let notice = baseline.drift(None, Some(OWN), None).unwrap();

        let rows: Vec<&str> = notice
            .lines()
            .filter(|line| line.starts_with("- "))
            .collect();
        assert_eq!(rows.len(), listed + usize::from(omitted));
        assert!(notice.contains(&format!("note-{}.md", entries - 1)));
        assert_eq!(notice.contains("earlier ones"), omitted);
    }
}
