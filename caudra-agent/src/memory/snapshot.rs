//! What the memory inspector shows, loaded off the UI thread: every entry
//! with what became of it, every node with its lease, the tree, and the live
//! view folded from it.

use std::collections::HashMap;

use caudra_storage::memory_journal::{EntryKind, EntryMeta, StoredNode};

use super::store::{MemoryError, MemoryState, MemoryStore};
use super::tree::{Merge, Part, Tree, VIEW};

/// What became of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryStatus {
    /// The newest entry of its name.
    Current,
    /// A later note of the same name, at this entry, replaced it.
    Rewritten(u64),
    /// A later entry deleted the note.
    Deleted(u64),
    /// Its text was purged.
    Forgotten,
}

/// Where a node stands in the mechanism.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// A view line that merges with `sibling` into `parent` when the view
    /// needs room, after `ahead` other merges.
    Merges {
        sibling: Part,
        parent: Part,
        ahead: usize,
    },
    /// A view line whose pair waits for `parent` to be summarized.
    AwaitsSummary { parent: Part },
    /// A view line whose sibling is still several smaller lines.
    AwaitsSibling { sibling: Part },
    /// A view line whose sibling needs `count` more entries.
    AwaitsEntries { count: u64 },
    /// Below the view, inside the view line `line`.
    Inside { line: Part },
}

pub struct MemorySnapshot {
    pub entries: Vec<EntryMeta>,
    /// What became of each entry, by its number.
    pub statuses: Vec<EntryStatus>,
    pub tree: Tree,
    /// The live view: the lines the fold keeps now, oldest first.
    pub view: Vec<Part>,
    /// Every pair of sibling lines in the view, most due first.
    pub merges: Vec<Merge>,
    nodes: HashMap<Part, StoredNode>,
}

impl MemorySnapshot {
    /// The journal as it stands: reconciling is the caller's choice.
    pub fn load(store: &MemoryStore) -> Result<Self, MemoryError> {
        Ok(Self::fold(store.load()?, VIEW))
    }

    /// `state` folded at `budget` bytes of lines instead of [`VIEW`].
    pub fn fold(state: MemoryState, budget: usize) -> Self {
        let MemoryState {
            entries,
            nodes,
            tree,
        } = state;
        let view = tree.fold(budget);
        Self {
            statuses: statuses(&entries),
            merges: tree.merge_queue(&view),
            nodes: nodes
                .into_iter()
                .map(|node| {
                    let part = Part {
                        level: node.level,
                        index: node.index,
                    };
                    (part, node)
                })
                .collect(),
            entries,
            tree,
            view,
        }
    }

    /// A node's stored row: who wrote it and when, or who holds its lease.
    pub fn node(&self, part: &Part) -> Option<&StoredNode> {
        self.nodes.get(part)
    }

    /// Whether a summarizer holds the node at `now_ms`.
    pub fn leased(&self, part: &Part, now_ms: i64) -> bool {
        self.nodes.get(part).is_some_and(|node| held(node, now_ms))
    }

    /// Every node a summarizer holds at `now_ms`.
    pub fn leases(&self, now_ms: i64) -> impl Iterator<Item = &Part> {
        self.nodes
            .iter()
            .filter(move |(_, node)| held(node, now_ms))
            .map(|(part, _)| part)
    }

    /// Bytes the live view's lines take. Over [`VIEW`] while the parents a
    /// merge needs wait to be summarized.
    pub fn size(&self) -> usize {
        self.tree.size(&self.view)
    }

    /// Where `part` stands: a view line and what it waits for, or a node
    /// inside one. `None` for a node above the view.
    pub fn placement(&self, part: &Part) -> Option<Placement> {
        if !self.view.contains(part) {
            return self
                .view
                .iter()
                .find(|line| line.level > part.level && line.covers(part.start()))
                .map(|line| Placement::Inside { line: line.clone() });
        }
        let sibling = part.sibling();
        let parent = part.parent();
        if self.view.contains(&sibling) {
            let ahead = self
                .merges
                .iter()
                .filter(|merge| merge.ready)
                .position(|merge| merge.parent == parent);
            return Some(match ahead {
                Some(ahead) => Placement::Merges {
                    sibling,
                    parent,
                    ahead,
                },
                None => Placement::AwaitsSummary { parent },
            });
        }
        let entries = self.tree.entries();
        Some(match sibling.formed(entries) {
            true => Placement::AwaitsSibling { sibling },
            false => Placement::AwaitsEntries {
                count: sibling.end() - entries,
            },
        })
    }
}

fn held(node: &StoredNode, now_ms: i64) -> bool {
    node.text.is_none()
        && node
            .lease_expires_ms
            .is_some_and(|expires| expires > now_ms)
}

fn statuses(entries: &[EntryMeta]) -> Vec<EntryStatus> {
    let mut later: HashMap<&str, (u64, EntryKind)> = HashMap::new();
    let mut statuses: Vec<EntryStatus> = entries
        .iter()
        .rev()
        .map(|meta| {
            let status = match (meta.forgotten, later.get(meta.name.as_str())) {
                (true, _) => EntryStatus::Forgotten,
                (false, None) => EntryStatus::Current,
                (false, Some(&(seq, EntryKind::Note))) => EntryStatus::Rewritten(seq),
                (false, Some(&(seq, EntryKind::Delete))) => EntryStatus::Deleted(seq),
            };
            later.insert(&meta.name, (meta.seq, meta.kind));
            status
        })
        .collect();
    statuses.reverse();
    statuses
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use caudra_storage::StateDir;
    use caudra_storage::memory_journal::EntryOrigin;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::memory::store::Source;
    use crate::memory::tree::NODE;

    const SCOPE: &str = "projects/test";
    const SESSION: &str = "session";
    const OWNER: &str = "owner";
    const MODEL: &str = "fast";
    const SHORT: &str = "x";
    /// Room for three short lines, so a fourth forces a merge.
    const BUDGET: usize = 60;
    const LEASE_MS: i64 = 1_000;

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
        fn write(&self, name: &str, body: &str) {
            self.store
                .write(name, body, &EntryOrigin::Session(SESSION.to_owned()))
                .unwrap();
        }

        fn notes(&self, count: usize, body: &str) {
            for index in 0..count {
                self.write(&format!("{index}.md"), body);
            }
        }

        fn snapshot(&self, budget: usize) -> MemorySnapshot {
            MemorySnapshot::fold(self.store.load().unwrap(), budget)
        }
    }

    fn long() -> String {
        "y".repeat(NODE)
    }

    #[test]
    fn statuses_follow_later_entries_of_the_same_name() {
        let fixture = fixture();
        fixture.write("a.md", SHORT);
        fixture.write("b.md", SHORT);
        fixture.write("a.md", "rewritten");
        fixture
            .store
            .delete("b.md", &EntryOrigin::External)
            .unwrap();
        fixture.write("c.md", SHORT);
        fixture.store.forget("c.md").unwrap();

        let snapshot = fixture.snapshot(VIEW);

        assert_eq!(
            snapshot.statuses,
            [
                EntryStatus::Rewritten(2),
                EntryStatus::Deleted(3),
                EntryStatus::Current,
                EntryStatus::Current,
                EntryStatus::Forgotten,
            ]
        );
    }

    #[test]
    fn a_built_pair_merges_first_in_line() {
        let fixture = fixture();
        fixture.notes(2, SHORT);

        let snapshot = fixture.snapshot(VIEW);
        let parent = Part { level: 1, index: 0 };

        assert_eq!(
            snapshot.placement(&Part::leaf(0)),
            Some(Placement::Merges {
                sibling: Part::leaf(1),
                parent,
                ahead: 0
            })
        );
    }

    #[test]
    fn a_pair_waits_for_its_parent_to_be_summarized() {
        let fixture = fixture();
        fixture.notes(2, &long());

        let snapshot = fixture.snapshot(VIEW);

        assert_eq!(
            snapshot.placement(&Part::leaf(1)),
            Some(Placement::AwaitsSummary {
                parent: Part { level: 1, index: 0 }
            })
        );
    }

    #[test]
    fn a_line_without_its_sibling_waits_for_entries() {
        let fixture = fixture();
        fixture.notes(3, SHORT);

        let snapshot = fixture.snapshot(VIEW);

        assert_eq!(
            snapshot.placement(&Part::leaf(2)),
            Some(Placement::AwaitsEntries { count: 1 })
        );
    }

    #[test]
    fn nodes_below_a_merged_line_are_inside_it() {
        let fixture = fixture();
        fixture.notes(4, SHORT);

        let snapshot = fixture.snapshot(BUDGET);
        let line = snapshot
            .view
            .iter()
            .find(|part| part.level > 0)
            .cloned()
            .unwrap();

        for child in line.children().unwrap() {
            assert_eq!(
                snapshot.placement(&child),
                Some(Placement::Inside { line: line.clone() })
            );
        }
        assert_eq!(snapshot.placement(&line.parent()), None);
    }

    #[test_case(0, true ; "while_the_lease_runs")]
    #[test_case(LEASE_MS, false ; "once_it_expires")]
    fn leases_show_while_they_run(elapsed_ms: i64, expected: bool) {
        let fixture = fixture();
        fixture.notes(1, &long());
        let leaf = Part::leaf(0);
        assert!(
            fixture
                .store
                .journal()
                .claim(SCOPE, leaf.level, leaf.index, OWNER, 0, LEASE_MS)
                .unwrap()
        );

        let snapshot = fixture.snapshot(VIEW);

        assert_eq!(snapshot.leased(&leaf, elapsed_ms), expected);
        assert_eq!(
            snapshot.leases(elapsed_ms).collect::<Vec<_>>(),
            expected.then_some(&leaf).into_iter().collect::<Vec<_>>()
        );
        assert!(!snapshot.tree.is_built(&leaf));
    }

    #[test]
    fn a_stored_line_names_its_model() {
        let fixture = fixture();
        fixture.notes(1, &long());
        let leaf = Part::leaf(0);
        let journal = fixture.store.journal();
        journal
            .claim(SCOPE, leaf.level, leaf.index, OWNER, 0, LEASE_MS)
            .unwrap();
        journal
            .store_node(SCOPE, leaf.level, leaf.index, OWNER, SHORT, MODEL, 0)
            .unwrap();
        drop(journal);

        let snapshot = fixture.snapshot(VIEW);

        assert_eq!(
            snapshot.node(&leaf).and_then(|node| node.model.as_deref()),
            Some(MODEL)
        );
        assert_eq!(snapshot.tree.line(&leaf), SHORT);
        assert!(snapshot.size() > SHORT.len());
    }
}
