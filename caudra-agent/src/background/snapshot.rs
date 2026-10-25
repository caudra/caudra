use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use caudra_storage::background::TaskRecord;

use super::{BackgroundTasks, State};
use crate::TaskCard;

/// What a frontend shows of the runtime at one revision: summaries only, so
/// nothing in it serializes an outcome or copies a report.
#[derive(Debug)]
pub struct TaskSnapshot {
    revision: u64,
    cards: Vec<TaskCard>,
    by_task: HashMap<String, usize>,
    by_invocation: HashMap<String, usize>,
    resident: HashMap<String, TaskCard>,
}

impl TaskSnapshot {
    fn build(state: &State) -> Self {
        let mut latest = BTreeMap::new();
        for record in state.recent.iter() {
            latest
                .entry(record.task_id.as_str())
                .or_insert_with(|| (record.sequence, record));
        }
        for record in state.records.values() {
            let entry = latest
                .entry(record.task_id.as_str())
                .or_insert_with(|| (record.sequence, record));
            if record.sequence >= entry.0 {
                *entry = (record.sequence, record);
            }
        }
        let mut cards = latest
            .into_values()
            .map(|(sequence, record)| (sequence, TaskCard::summary(record)))
            .collect::<Vec<_>>();
        cards.sort_by_key(|(sequence, card)| {
            (
                !matches!(card.state.as_str(), "queued" | "running" | "cancelling"),
                Reverse(*sequence),
            )
        });
        let cards = cards.into_iter().map(|(_, card)| card).collect::<Vec<_>>();
        let index = |key: fn(&TaskCard) -> &String| {
            cards
                .iter()
                .enumerate()
                .map(|(index, card)| (key(card).clone(), index))
                .collect()
        };
        Self {
            revision: state.revision(),
            by_task: index(|card| &card.task_id),
            by_invocation: index(|card| &card.invocation_id),
            resident: state
                .records
                .values()
                .map(|record: &TaskRecord| {
                    (record.invocation_id.clone(), TaskCard::summary(record))
                })
                .collect(),
            cards,
        }
    }

    /// Unique across runtimes, so a revision read from one never matches another.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The latest card of every task, running work first.
    pub fn cards(&self) -> &[TaskCard] {
        &self.cards
    }

    pub fn card(&self, task_id: &str) -> Option<&TaskCard> {
        self.by_task.get(task_id).map(|&index| &self.cards[index])
    }

    pub fn position(&self, task_id: &str) -> Option<usize> {
        self.by_task.get(task_id).copied()
    }

    /// The listed card of an invocation, which is its task's latest.
    pub fn latest_invocation(&self, invocation_id: &str) -> Option<&TaskCard> {
        self.by_invocation
            .get(invocation_id)
            .map(|&index| &self.cards[index])
    }

    /// A resident invocation of `task_id`, superseded or not.
    pub fn resident(&self, task_id: &str, invocation_id: &str) -> Option<&TaskCard> {
        self.resident
            .get(invocation_id)
            .filter(|card| card.task_id == task_id)
    }

    pub fn has_resident(&self) -> bool {
        !self.resident.is_empty()
    }
}

impl State {
    pub(super) fn revision(&self) -> u64 {
        self.records.stamp().max(self.recent.stamp())
    }
}

impl BackgroundTasks {
    /// Rebuilt only after the records changed, so reading it while nothing
    /// happens costs one lock and a comparison.
    pub fn snapshot(&self) -> Arc<TaskSnapshot> {
        let mut state = self.lock();
        if let Some(snapshot) = &state.snapshot
            && snapshot.revision == state.revision()
        {
            return Arc::clone(snapshot);
        }
        let snapshot = Arc::new(TaskSnapshot::build(&state));
        state.snapshot = Some(Arc::clone(&snapshot));
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use caudra_storage::StateDir;
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::*;
    use crate::StoredSession;

    const TASK: &str = "quiet-amber-heron";
    const OTHER_TASK: &str = "loud-amber-heron";
    const FIRST: &str = "first-invocation";
    const NEXT: &str = "next-invocation";
    const RUNNING: &str = "running";

    fn record(invocation: &str, sequence: u64) -> TaskRecord {
        TaskRecord {
            payload: Default::default(),
            owner: Default::default(),
            created_at: 1,
            updated_at: 1,
            sequence,
            task_id: TASK.into(),
            invocation_id: invocation.into(),
            root_call_id: invocation.into(),
            generation: 1,
            state: RUNNING.into(),
            background: true,
            receipt_accepted: false,
            mode: "build".into(),
            request: json!({"call_id": invocation}),
            outcome: Some(json!({"result": "a long report"})),
            output_ref: None,
            spec: Value::Null,
            events: Vec::new(),
        }
    }

    async fn runtime() -> (TempDir, BackgroundTasks) {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        let mut session = StoredSession::new("test-model", temp.path().to_str().unwrap());
        session.save(&dir).unwrap();
        let tasks = BackgroundTasks::spawn(dir, session.id).await.unwrap();
        (temp, tasks)
    }

    fn insert(tasks: &BackgroundTasks, record: TaskRecord) {
        tasks
            .lock()
            .records
            .insert(record.invocation_id.clone(), record);
    }

    #[test]
    fn an_unchanged_runtime_hands_back_the_same_snapshot() {
        smol::block_on(async {
            let (_temp, tasks) = runtime().await;
            let empty = tasks.snapshot();
            assert!(Arc::ptr_eq(&empty, &tasks.snapshot()));
            assert!(!empty.has_resident());

            insert(&tasks, record(FIRST, 1));
            let changed = tasks.snapshot();
            assert_ne!(changed.revision(), empty.revision());
            assert!(Arc::ptr_eq(&changed, &tasks.snapshot()));
            assert_eq!(changed.cards(), tasks.list().as_slice());
        });
    }

    #[test]
    fn a_snapshot_holds_summaries_of_every_resident_invocation() {
        smol::block_on(async {
            let (_temp, tasks) = runtime().await;
            insert(&tasks, record(FIRST, 1));
            insert(&tasks, record(NEXT, 2));
            let snapshot = tasks.snapshot();

            assert_eq!(snapshot.cards().len(), 1);
            assert_eq!(snapshot.card(TASK).unwrap().invocation_id, NEXT);
            assert!(snapshot.latest_invocation(NEXT).is_some());
            assert!(snapshot.latest_invocation(FIRST).is_none());
            let superseded = snapshot.resident(TASK, FIRST).unwrap();
            assert_eq!(superseded.result, None);
            assert!(superseded.reports.is_empty());
            assert!(
                tasks
                    .status_invocation(TASK, FIRST)
                    .unwrap()
                    .result
                    .is_some()
            );
            assert!(snapshot.resident(OTHER_TASK, FIRST).is_none());
        });
    }
}
