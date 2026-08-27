use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use maki_providers::Message;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SubagentHistoryError {
    #[error("unknown subagent task ID `{task_id}`")]
    Unknown { task_id: String },
    #[error("subagent task `{task_id}` is already active")]
    AlreadyActive { task_id: String },
    #[error("subagent task `{task_id}` already has completed history")]
    AlreadyCompleted { task_id: String },
}

#[derive(Clone, Debug, Default)]
pub struct SubagentHistorySnapshot {
    revision: u64,
    histories: Arc<HashMap<String, Arc<Vec<Message>>>>,
}

impl SubagentHistorySnapshot {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn histories(&self) -> &HashMap<String, Arc<Vec<Message>>> {
        &self.histories
    }
}

#[derive(Debug, Default)]
struct State {
    revision: u64,
    histories: Arc<HashMap<String, Arc<Vec<Message>>>>,
    active: HashSet<String>,
}

#[derive(Clone, Debug, Default)]
pub struct SubagentHistoryStore {
    state: Arc<Mutex<State>>,
}

impl SubagentHistoryStore {
    pub fn seeded(histories: HashMap<String, Arc<Vec<Message>>>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                histories: Arc::new(histories),
                ..State::default()
            })),
        }
    }

    pub fn reserve(
        &self,
        task_id: impl Into<String>,
    ) -> Result<SubagentHistoryLease, SubagentHistoryError> {
        let task_id = task_id.into();
        let mut state = self.lock();
        if state.active.contains(&task_id) {
            return Err(SubagentHistoryError::AlreadyActive { task_id });
        }
        if state.histories.contains_key(&task_id) {
            return Err(SubagentHistoryError::AlreadyCompleted { task_id });
        }
        state.active.insert(task_id.clone());
        drop(state);
        Ok(SubagentHistoryLease::new(self.clone(), task_id, None))
    }

    pub fn continue_task(
        &self,
        task_id: &str,
    ) -> Result<SubagentHistoryLease, SubagentHistoryError> {
        let mut state = self.lock();
        if state.active.contains(task_id) {
            return Err(SubagentHistoryError::AlreadyActive {
                task_id: task_id.to_owned(),
            });
        }
        let history =
            state
                .histories
                .get(task_id)
                .cloned()
                .ok_or_else(|| SubagentHistoryError::Unknown {
                    task_id: task_id.to_owned(),
                })?;
        state.active.insert(task_id.to_owned());
        drop(state);
        Ok(SubagentHistoryLease::new(
            self.clone(),
            task_id.to_owned(),
            Some(history),
        ))
    }

    pub fn snapshot(&self) -> SubagentHistorySnapshot {
        let state = self.lock();
        SubagentHistorySnapshot {
            revision: state.revision,
            histories: Arc::clone(&state.histories),
        }
    }

    pub fn revision(&self) -> u64 {
        self.lock().revision
    }

    pub fn is_active(&self, task_id: &str) -> bool {
        self.lock().active.contains(task_id)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn complete(&self, task_id: String, history: Arc<Vec<Message>>) {
        let mut state = self.lock();
        state.active.remove(&task_id);
        Arc::make_mut(&mut state.histories).insert(task_id, history);
        state.revision += 1;
    }

    fn release(&self, task_id: &str) {
        self.lock().active.remove(task_id);
    }
}

#[derive(Debug)]
pub struct SubagentHistoryLease {
    store: SubagentHistoryStore,
    task_id: String,
    history: Option<Arc<Vec<Message>>>,
    completed: bool,
}

impl SubagentHistoryLease {
    fn new(
        store: SubagentHistoryStore,
        task_id: String,
        history: Option<Arc<Vec<Message>>>,
    ) -> Self {
        Self {
            store,
            task_id,
            history,
            completed: false,
        }
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn history(&self) -> Option<&Arc<Vec<Message>>> {
        self.history.as_ref()
    }

    pub fn complete(mut self, history: impl Into<Arc<Vec<Message>>>) {
        self.store.complete(self.task_id.clone(), history.into());
        self.completed = true;
    }
}

impl Drop for SubagentHistoryLease {
    fn drop(&mut self) {
        if !self.completed {
            self.store.release(&self.task_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASK_ID: &str = "task-1";
    const UNKNOWN_ID: &str = "missing";
    const FIRST_PROMPT: &str = "first prompt";
    const SECOND_PROMPT: &str = "second prompt";

    fn history(prompt: &str) -> Arc<Vec<Message>> {
        Arc::new(vec![Message::user(prompt.into())])
    }

    #[test]
    fn completion_publishes_arc_history_and_advances_revision() {
        let store = SubagentHistoryStore::default();
        let lease = store.reserve(TASK_ID).unwrap();
        let messages = history(FIRST_PROMPT);
        lease.complete(Arc::clone(&messages));

        let snapshot = store.snapshot();
        assert_eq!(snapshot.revision(), 1);
        assert!(Arc::ptr_eq(&snapshot.histories()[TASK_ID], &messages));
        assert!(!store.is_active(TASK_ID));
    }

    #[test]
    fn continuation_rejects_unknown_and_active_tasks() {
        let store = SubagentHistoryStore::default();
        assert_eq!(
            store.continue_task(UNKNOWN_ID).unwrap_err(),
            SubagentHistoryError::Unknown {
                task_id: UNKNOWN_ID.into()
            }
        );

        let lease = store.reserve(TASK_ID).unwrap();
        assert_eq!(
            store.continue_task(TASK_ID).unwrap_err(),
            SubagentHistoryError::AlreadyActive {
                task_id: TASK_ID.into()
            }
        );
        drop(lease);
    }

    #[test]
    fn dropped_incomplete_lease_releases_reservation() {
        let store = SubagentHistoryStore::default();
        drop(store.reserve(TASK_ID).unwrap());
        assert!(!store.is_active(TASK_ID));

        store
            .reserve(TASK_ID)
            .unwrap()
            .complete(history(FIRST_PROMPT));
        let continuation = store.continue_task(TASK_ID).unwrap();
        assert_eq!(
            continuation.history().unwrap()[0].user_text(),
            Some(FIRST_PROMPT)
        );
        drop(continuation);
        assert!(store.continue_task(TASK_ID).is_ok());
    }

    #[test]
    fn snapshots_remain_stable_across_continuation_updates() {
        let store = SubagentHistoryStore::default();
        store
            .reserve(TASK_ID)
            .unwrap()
            .complete(history(FIRST_PROMPT));
        let first = store.snapshot();

        store
            .continue_task(TASK_ID)
            .unwrap()
            .complete(history(SECOND_PROMPT));
        let second = store.snapshot();

        assert_eq!(first.revision(), 1);
        assert_eq!(second.revision(), 2);
        assert_eq!(
            first.histories()[TASK_ID][0].user_text(),
            Some(FIRST_PROMPT)
        );
        assert_eq!(
            second.histories()[TASK_ID][0].user_text(),
            Some(SECOND_PROMPT)
        );
    }
}
