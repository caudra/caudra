use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use caudra_providers::{HistoryItem, HistoryItemKind, Message};
pub use caudra_storage::sessions::{
    StoredMode as SubagentTaskMode, StoredSubagentTaskSpec as SubagentTaskSpec,
};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SubagentHistoryError {
    #[error("unknown subagent task ID `{task_id}`; omit task_id to start a fresh subagent")]
    Unknown { task_id: String },
    #[error(
        "subagent task `{task_id}` is already running; wait for it to finish before continuing it"
    )]
    AlreadyActive { task_id: String },
    #[error("subagent task `{task_id}` already has completed history")]
    AlreadyCompleted { task_id: String },
    #[error(
        "subagent task `{task_id}` uses profile `{stored}`, not requested profile `{requested}`; omit profile to keep the stored one"
    )]
    ProfileMismatch {
        task_id: String,
        stored: String,
        requested: String,
    },
    #[error(
        "subagent task `{task_id}` uses `{stored}` mode, not requested `{requested}` mode; omit mode to keep the stored one"
    )]
    ModeMismatch {
        task_id: String,
        stored: SubagentTaskMode,
        requested: SubagentTaskMode,
    },
    #[error(
        "subagent task `{task_id}` has a locked task profile and mode; continue it with task = true"
    )]
    TaskSpecRequired { task_id: String },
    #[error("subagent task `{task_id}` is a generic session; continue it without task = true")]
    GenericSessionRequired { task_id: String },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubagentTaskSpecCandidate {
    pub profile_name: Option<String>,
    pub mode: Option<SubagentTaskMode>,
}

impl SubagentTaskSpecCandidate {
    fn resolve(
        self,
        task_id: &str,
        stored: Option<&SubagentTaskSpec>,
    ) -> Result<SubagentTaskSpec, SubagentHistoryError> {
        let Some(stored) = stored else {
            let defaults = SubagentTaskSpec::default();
            return Ok(SubagentTaskSpec {
                profile_name: self.profile_name.unwrap_or(defaults.profile_name),
                mode: self.mode.unwrap_or(defaults.mode),
                ..SubagentTaskSpec::default()
            });
        };
        if let Some(requested) = self.profile_name
            && requested != stored.profile_name
        {
            return Err(SubagentHistoryError::ProfileMismatch {
                task_id: task_id.to_owned(),
                stored: stored.profile_name.clone(),
                requested,
            });
        }
        if let Some(requested) = self.mode
            && requested != stored.mode
        {
            return Err(SubagentHistoryError::ModeMismatch {
                task_id: task_id.to_owned(),
                stored: stored.mode,
                requested,
            });
        }
        Ok(stored.clone())
    }
}

#[derive(Clone, Debug)]
pub struct SubagentHistoryRecord {
    messages: Arc<Vec<Message>>,
    spec: Option<SubagentTaskSpec>,
    version_id: Option<String>,
}

impl SubagentHistoryRecord {
    pub fn new(messages: impl Into<Arc<Vec<Message>>>, spec: Option<SubagentTaskSpec>) -> Self {
        Self {
            messages: messages.into(),
            spec,
            version_id: None,
        }
    }

    pub fn messages(&self) -> &Arc<Vec<Message>> {
        &self.messages
    }

    pub fn spec(&self) -> Option<&SubagentTaskSpec> {
        self.spec.as_ref()
    }

    pub fn version_id(&self) -> Option<&str> {
        self.version_id.as_deref()
    }
}

#[derive(Clone, Debug, Default)]
pub struct SubagentHistorySnapshot {
    revision: u64,
    records: Arc<HashMap<String, Arc<SubagentHistoryRecord>>>,
    legacy_histories: OnceLock<HashMap<String, Arc<Vec<Message>>>>,
}

impl SubagentHistorySnapshot {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn histories(&self) -> &HashMap<String, Arc<Vec<Message>>> {
        self.legacy_histories.get_or_init(|| {
            self.records
                .iter()
                .map(|(task_id, record)| (task_id.clone(), Arc::clone(record.messages())))
                .collect()
        })
    }

    pub fn records(&self) -> &HashMap<String, Arc<SubagentHistoryRecord>> {
        &self.records
    }
}

#[derive(Debug, Default)]
struct State {
    revision: u64,
    records: Arc<HashMap<String, Arc<SubagentHistoryRecord>>>,
    active: HashSet<String>,
}

#[derive(Clone, Debug, Default)]
pub struct SubagentHistoryStore {
    state: Arc<Mutex<State>>,
}

impl SubagentHistoryStore {
    pub fn seeded(histories: HashMap<String, Arc<Vec<Message>>>) -> Self {
        Self::seeded_with_specs(histories, HashMap::new())
    }

    pub fn seeded_with_specs(
        histories: HashMap<String, Arc<Vec<Message>>>,
        mut specs: HashMap<String, SubagentTaskSpec>,
    ) -> Self {
        let records = histories
            .into_iter()
            .map(|(task_id, messages)| {
                let spec = specs.remove(&task_id);
                (
                    task_id,
                    Arc::new(SubagentHistoryRecord::new(messages, spec)),
                )
            })
            .collect();
        Self {
            state: Arc::new(Mutex::new(State {
                records: Arc::new(records),
                ..State::default()
            })),
        }
    }

    pub fn seeded_records(records: HashMap<String, SubagentHistoryRecord>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                records: Arc::new(
                    records
                        .into_iter()
                        .map(|(task_id, record)| (task_id, Arc::new(record)))
                        .collect(),
                ),
                ..State::default()
            })),
        }
    }

    pub fn reserve(
        &self,
        task_id: impl Into<String>,
    ) -> Result<SubagentHistoryLease, SubagentHistoryError> {
        self.reserve_inner(task_id.into(), Some(SubagentTaskSpec::generic()))
    }

    pub fn reserve_with_spec(
        &self,
        task_id: impl Into<String>,
        spec: SubagentTaskSpec,
    ) -> Result<SubagentHistoryLease, SubagentHistoryError> {
        self.reserve_inner(task_id.into(), Some(spec))
    }

    fn reserve_inner(
        &self,
        task_id: String,
        spec: Option<SubagentTaskSpec>,
    ) -> Result<SubagentHistoryLease, SubagentHistoryError> {
        let mut state = self.lock();
        if state.active.contains(&task_id) {
            return Err(SubagentHistoryError::AlreadyActive { task_id });
        }
        if state.records.contains_key(&task_id) {
            return Err(SubagentHistoryError::AlreadyCompleted { task_id });
        }
        state.active.insert(task_id.clone());
        drop(state);
        Ok(SubagentHistoryLease::new(self.clone(), task_id, None, spec))
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
        let record =
            state
                .records
                .get(task_id)
                .cloned()
                .ok_or_else(|| SubagentHistoryError::Unknown {
                    task_id: task_id.to_owned(),
                })?;
        if record.spec().is_some_and(|spec| !spec.is_generic()) {
            return Err(SubagentHistoryError::TaskSpecRequired {
                task_id: task_id.to_owned(),
            });
        }
        state.active.insert(task_id.to_owned());
        drop(state);
        Ok(SubagentHistoryLease::new(
            self.clone(),
            task_id.to_owned(),
            Some(Arc::clone(record.messages())),
            Some(SubagentTaskSpec::generic()),
        ))
    }

    pub fn continue_task_with(
        &self,
        task_id: &str,
        candidate: SubagentTaskSpecCandidate,
    ) -> Result<SubagentHistoryLease, SubagentHistoryError> {
        self.continue_task_with_defaults(task_id, candidate, SubagentTaskSpec::default())
    }

    pub fn continue_task_with_defaults(
        &self,
        task_id: &str,
        candidate: SubagentTaskSpecCandidate,
        defaults: SubagentTaskSpec,
    ) -> Result<SubagentHistoryLease, SubagentHistoryError> {
        let mut state = self.lock();
        if state.active.contains(task_id) {
            return Err(SubagentHistoryError::AlreadyActive {
                task_id: task_id.to_owned(),
            });
        }
        let record =
            state
                .records
                .get(task_id)
                .cloned()
                .ok_or_else(|| SubagentHistoryError::Unknown {
                    task_id: task_id.to_owned(),
                })?;
        let spec = match record.spec() {
            Some(stored) if stored.is_generic() => {
                return Err(SubagentHistoryError::GenericSessionRequired {
                    task_id: task_id.to_owned(),
                });
            }
            Some(stored) => candidate.resolve(task_id, Some(stored))?,
            None => SubagentTaskSpec {
                profile_name: candidate.profile_name.unwrap_or(defaults.profile_name),
                mode: candidate.mode.unwrap_or(defaults.mode),
                ..SubagentTaskSpec::default()
            },
        };
        state.active.insert(task_id.to_owned());
        drop(state);
        Ok(SubagentHistoryLease::new(
            self.clone(),
            task_id.to_owned(),
            Some(Arc::clone(record.messages())),
            Some(spec),
        ))
    }

    pub fn snapshot(&self) -> SubagentHistorySnapshot {
        let state = self.lock();
        SubagentHistorySnapshot {
            revision: state.revision,
            records: Arc::clone(&state.records),
            legacy_histories: OnceLock::new(),
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

    fn complete(
        &self,
        task_id: String,
        history: Arc<Vec<Message>>,
        spec: Option<SubagentTaskSpec>,
        version_id: Option<String>,
    ) {
        let mut state = self.lock();
        state.active.remove(&task_id);
        let mut record = SubagentHistoryRecord::new(history, spec);
        record.version_id = version_id;
        Arc::make_mut(&mut state.records).insert(task_id, Arc::new(record));
        state.revision += 1;
    }

    fn release(&self, task_id: &str) {
        self.lock().active.remove(task_id);
    }
}

pub fn active_task_history_versions(history: &[HistoryItem]) -> HashMap<String, String> {
    active_task_history_versions_with_batch_state(history, |_| None)
}

pub fn active_task_history_versions_with_batch_state<'a>(
    history: &[HistoryItem],
    mut batch_state: impl FnMut(&str) -> Option<&'a serde_json::Value>,
) -> HashMap<String, String> {
    let mut versions = HashMap::new();
    for item in history {
        match &item.kind {
            HistoryItemKind::ToolCall {
                call_id,
                name,
                input,
                ..
            } if name == "task" => {
                let task_id = input
                    .get("task_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(call_id);
                versions.insert(task_id.to_owned(), call_id.clone());
            }
            HistoryItemKind::ToolCall { call_id, name, .. } if name == "batch" => {
                if let Some(state) = batch_state(call_id) {
                    versions.extend(batch_task_history_versions(state));
                }
            }
            _ => {}
        }
    }
    versions
}

pub fn batch_task_history_versions(state: &serde_json::Value) -> HashMap<String, String> {
    fn collect(value: &serde_json::Value, versions: &mut HashMap<String, String>) {
        match value {
            serde_json::Value::Array(values) => {
                for value in values {
                    collect(value, versions);
                }
            }
            serde_json::Value::Object(values) => {
                if values.get("tool").and_then(serde_json::Value::as_str) == Some("task") {
                    let task_id = values
                        .get("output")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|output| {
                            output.split("<task_metadata>").skip(1).find_map(|block| {
                                block.split("</task_metadata>").next().and_then(|metadata| {
                                    metadata
                                        .lines()
                                        .find_map(|line| line.trim().strip_prefix("task_id: "))
                                })
                            })
                        });
                    let invocation_id = values
                        .get("invocation_id")
                        .and_then(serde_json::Value::as_str);
                    if let (Some(task_id), Some(invocation_id)) = (task_id, invocation_id) {
                        versions.insert(task_id.to_owned(), invocation_id.to_owned());
                    }
                    return;
                }
                for value in values.values() {
                    collect(value, versions);
                }
            }
            _ => {}
        }
    }

    let mut versions = HashMap::new();
    collect(state, &mut versions);
    versions
}

pub fn history_tool_call_ids(history: &[HistoryItem]) -> HashSet<String> {
    history
        .iter()
        .flat_map(|item| match &item.kind {
            HistoryItemKind::ToolCall { call_id, .. } => std::slice::from_ref(call_id),
            HistoryItemKind::AssistantText {
                retained_subagent_ids,
                ..
            } => retained_subagent_ids.as_slice(),
            _ => &[],
        })
        .cloned()
        .collect()
}

#[derive(Debug)]
pub struct SubagentHistoryLease {
    store: SubagentHistoryStore,
    task_id: String,
    history: Option<Arc<Vec<Message>>>,
    spec: Option<SubagentTaskSpec>,
    completed: bool,
}

impl SubagentHistoryLease {
    fn new(
        store: SubagentHistoryStore,
        task_id: String,
        history: Option<Arc<Vec<Message>>>,
        spec: Option<SubagentTaskSpec>,
    ) -> Self {
        Self {
            store,
            task_id,
            history,
            spec,
            completed: false,
        }
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn history(&self) -> Option<&Arc<Vec<Message>>> {
        self.history.as_ref()
    }

    pub fn spec(&self) -> Option<&SubagentTaskSpec> {
        self.spec.as_ref()
    }

    pub fn complete(mut self, history: impl Into<Arc<Vec<Message>>>) {
        self.store.complete(
            self.task_id.clone(),
            history.into(),
            self.spec.clone(),
            None,
        );
        self.completed = true;
    }

    pub fn complete_version(mut self, history: impl Into<Arc<Vec<Message>>>, version_id: String) {
        self.store.complete(
            self.task_id.clone(),
            history.into(),
            self.spec.clone(),
            Some(version_id),
        );
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
    const PROFILE: &str = "review";
    const OTHER_PROFILE: &str = "custom";

    fn history(prompt: &str) -> Arc<Vec<Message>> {
        Arc::new(vec![Message::user(prompt.into())])
    }

    fn spec(profile_name: &str, mode: SubagentTaskMode) -> SubagentTaskSpec {
        SubagentTaskSpec {
            profile_name: profile_name.into(),
            mode,
            ..SubagentTaskSpec::default()
        }
    }

    #[test]
    fn completion_publishes_arc_history_and_advances_revision() {
        let store = SubagentHistoryStore::default();
        let task_spec = spec(PROFILE, SubagentTaskMode::Plan);
        let lease = store.reserve_with_spec(TASK_ID, task_spec.clone()).unwrap();
        let messages = history(FIRST_PROMPT);
        lease.complete(Arc::clone(&messages));

        let snapshot = store.snapshot();
        assert_eq!(snapshot.revision(), 1);
        assert!(Arc::ptr_eq(&snapshot.histories()[TASK_ID], &messages));
        assert_eq!(snapshot.records()[TASK_ID].spec(), Some(&task_spec));
        assert!(!store.is_active(TASK_ID));
    }

    #[test]
    fn completion_records_the_invocation_version() {
        let store = SubagentHistoryStore::default();
        let lease = store
            .reserve_with_spec(TASK_ID, spec(PROFILE, SubagentTaskMode::Plan))
            .unwrap();
        lease.complete_version(history(FIRST_PROMPT), "continuation-call".into());

        assert_eq!(
            store.snapshot().records()[TASK_ID].version_id(),
            Some("continuation-call")
        );
    }

    #[test]
    fn active_history_selects_latest_continuation_version() {
        let item = |call_id: &str, task_id: Option<&str>| HistoryItem {
            id: caudra_storage::id::CaudraId::generate(),
            parent_id: None,
            supersedes: None,
            group_id: caudra_storage::id::CaudraId::generate(),
            kind: HistoryItemKind::ToolCall {
                call_id: call_id.into(),
                name: "task".into(),
                input: task_id.map_or_else(
                    || serde_json::json!({}),
                    |task_id| serde_json::json!({ "task_id": task_id }),
                ),
                thought_signature: None,
                source: None,
            },
        };
        let versions = active_task_history_versions(&[
            item(TASK_ID, None),
            item("continuation-call", Some(TASK_ID)),
        ]);

        assert_eq!(versions[TASK_ID], "continuation-call");
    }

    #[test]
    fn active_history_selects_batched_continuation_version() {
        let batch_call = HistoryItem {
            id: caudra_storage::id::CaudraId::generate(),
            parent_id: None,
            supersedes: None,
            group_id: caudra_storage::id::CaudraId::generate(),
            kind: HistoryItemKind::ToolCall {
                call_id: "batch-call".into(),
                name: "batch".into(),
                input: serde_json::json!({}),
                thought_signature: None,
                source: None,
            },
        };
        let state = serde_json::json!([{
            "tool": "task",
            "output": "<task_metadata>\ntask_id: task-1\n</task_metadata>",
            "invocation_id": "batch-child-call",
        }]);

        let versions = active_task_history_versions_with_batch_state(&[batch_call], |call_id| {
            (call_id == "batch-call").then_some(&state)
        });

        assert_eq!(versions[TASK_ID], "batch-child-call");
    }

    #[test]
    fn generic_sessions_keep_their_kind_and_cannot_bypass_locked_task_specs() {
        let generic = SubagentHistoryStore::default();
        generic
            .reserve(TASK_ID)
            .unwrap()
            .complete(history(FIRST_PROMPT));
        assert!(
            generic.snapshot().records()[TASK_ID]
                .spec()
                .is_some_and(SubagentTaskSpec::is_generic)
        );
        assert!(generic.continue_task(TASK_ID).is_ok());

        let task = SubagentHistoryStore::default();
        task.reserve_with_spec(TASK_ID, spec(PROFILE, SubagentTaskMode::Plan))
            .unwrap()
            .complete(history(FIRST_PROMPT));
        assert_eq!(
            task.continue_task(TASK_ID).unwrap_err(),
            SubagentHistoryError::TaskSpecRequired {
                task_id: TASK_ID.into()
            }
        );
    }

    #[test]
    fn completed_generic_session_cannot_be_reclassified_as_a_task() {
        let store = SubagentHistoryStore::default();
        store
            .reserve(TASK_ID)
            .unwrap()
            .complete(history(FIRST_PROMPT));

        assert_eq!(
            store
                .continue_task_with(TASK_ID, SubagentTaskSpecCandidate::default())
                .unwrap_err(),
            SubagentHistoryError::GenericSessionRequired {
                task_id: TASK_ID.into()
            }
        );
    }

    #[test]
    fn continuation_uses_stored_values_for_omitted_fields() {
        let task_spec = spec(PROFILE, SubagentTaskMode::Plan);
        let store = SubagentHistoryStore::default();
        store
            .reserve_with_spec(TASK_ID, task_spec.clone())
            .unwrap()
            .complete(history(FIRST_PROMPT));

        let lease = store
            .continue_task_with(
                TASK_ID,
                SubagentTaskSpecCandidate {
                    profile_name: None,
                    mode: Some(SubagentTaskMode::Plan),
                },
            )
            .unwrap();

        assert_eq!(lease.spec(), Some(&task_spec));
    }

    #[test]
    fn continuation_mismatches_do_not_claim_active_task() {
        let store = SubagentHistoryStore::default();
        store
            .reserve_with_spec(TASK_ID, spec(PROFILE, SubagentTaskMode::Plan))
            .unwrap()
            .complete(history(FIRST_PROMPT));

        assert_eq!(
            store
                .continue_task_with(
                    TASK_ID,
                    SubagentTaskSpecCandidate {
                        profile_name: Some(OTHER_PROFILE.into()),
                        mode: None,
                    },
                )
                .unwrap_err(),
            SubagentHistoryError::ProfileMismatch {
                task_id: TASK_ID.into(),
                stored: PROFILE.into(),
                requested: OTHER_PROFILE.into(),
            }
        );
        assert!(!store.is_active(TASK_ID));

        assert_eq!(
            store
                .continue_task_with(
                    TASK_ID,
                    SubagentTaskSpecCandidate {
                        profile_name: None,
                        mode: Some(SubagentTaskMode::Build),
                    },
                )
                .unwrap_err(),
            SubagentHistoryError::ModeMismatch {
                task_id: TASK_ID.into(),
                stored: SubagentTaskMode::Plan,
                requested: SubagentTaskMode::Build,
            }
        );
        assert!(!store.is_active(TASK_ID));
    }

    #[test]
    fn legacy_history_binds_spec_only_when_continuation_completes() {
        let store =
            SubagentHistoryStore::seeded(HashMap::from([(TASK_ID.into(), history(FIRST_PROMPT))]));
        let candidate = SubagentTaskSpecCandidate {
            profile_name: Some(PROFILE.into()),
            mode: Some(SubagentTaskMode::Plan),
        };

        let lease = store
            .continue_task_with(TASK_ID, candidate.clone())
            .unwrap();
        assert_eq!(lease.spec(), Some(&spec(PROFILE, SubagentTaskMode::Plan)));
        assert_eq!(store.snapshot().records()[TASK_ID].spec(), None);
        drop(lease);
        assert_eq!(store.snapshot().records()[TASK_ID].spec(), None);

        store
            .continue_task_with(TASK_ID, candidate)
            .unwrap()
            .complete(history(SECOND_PROMPT));

        assert_eq!(
            store.snapshot().records()[TASK_ID].spec(),
            Some(&spec(PROFILE, SubagentTaskMode::Plan))
        );
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
