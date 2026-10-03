use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use crate::ToolOutput;
use crate::tools::native::batch::child_tool_use_id;
use caudra_providers::{HistoryItem, HistoryItemKind, Message};
use caudra_storage::DescriptiveIdCandidates;
pub use caudra_storage::sessions::{
    StoredMode as SubagentTaskMode, StoredSubagentTaskSpec as SubagentTaskSpec,
};
use thiserror::Error;

const TASK_ID_EXHAUSTED: &str = "task identity allocation exhausted after 4096 candidates; use a different description or start a new session";

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
    selected_versions: HashMap<String, String>,
}

#[derive(Clone, Debug, Default)]
pub struct SubagentHistoryStore {
    state: Arc<Mutex<State>>,
}

impl SubagentHistoryStore {
    pub(crate) fn reserve_unconfigured(
        &self,
        task_id: &str,
    ) -> Result<SubagentHistoryLease, String> {
        self.reserve_inner(task_id.to_owned(), None)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn reserve_generated(
        &self,
        label: &str,
        mut occupied: impl FnMut(&str) -> Result<bool, String>,
    ) -> Result<SubagentHistoryLease, String> {
        for task_id in DescriptiveIdCandidates::new(label, "task") {
            let mut state = self.lock();
            if task_id == "main"
                || state.active.contains(&task_id)
                || state.records.contains_key(&task_id)
                || state.selected_versions.contains_key(&task_id)
            {
                continue;
            }
            state.active.insert(task_id.clone());
            drop(state);
            let lease = SubagentHistoryLease::new(self.clone(), task_id, None, None);
            if !occupied(lease.task_id())? {
                return Ok(lease);
            }
        }
        Err(TASK_ID_EXHAUSTED.into())
    }

    pub fn seeded(histories: HashMap<String, Arc<Vec<Message>>>) -> Self {
        Self::seeded_with_specs(histories, HashMap::new())
    }

    pub fn seeded_with_specs(
        histories: HashMap<String, Arc<Vec<Message>>>,
        specs: HashMap<String, SubagentTaskSpec>,
    ) -> Self {
        Self::seeded_with_versions(histories, specs, HashMap::new())
    }

    pub fn seeded_with_versions(
        histories: HashMap<String, Arc<Vec<Message>>>,
        mut specs: HashMap<String, SubagentTaskSpec>,
        mut selected_versions: HashMap<String, String>,
    ) -> Self {
        let records = histories
            .into_iter()
            .map(|(task_id, messages)| {
                let spec = specs.remove(&task_id);
                let mut record = SubagentHistoryRecord::new(messages, spec);
                record.version_id = selected_versions.remove(&task_id);
                (task_id, Arc::new(record))
            })
            .collect();
        Self {
            state: Arc::new(Mutex::new(State {
                records: Arc::new(records),
                selected_versions,
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
        if state.records.contains_key(&task_id) || state.selected_versions.contains_key(&task_id) {
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

    pub(crate) fn selected_version(&self, task_id: &str) -> Option<String> {
        let state = self.lock();
        state.selected_versions.get(task_id).cloned().or_else(|| {
            state
                .records
                .get(task_id)
                .and_then(|record| record.version_id().map(str::to_owned))
        })
    }

    #[cfg(test)]
    pub(crate) fn active_count(&self) -> usize {
        self.lock().active.len()
    }

    pub(crate) fn restore_version(
        &self,
        task_id: String,
        history: Arc<Vec<Message>>,
        spec: SubagentTaskSpec,
        version_id: String,
    ) -> Result<(), SubagentHistoryError> {
        let mut state = self.lock();
        if state.active.contains(&task_id) {
            return Err(SubagentHistoryError::AlreadyActive { task_id });
        }
        let mut record = SubagentHistoryRecord::new(history, Some(spec));
        record.version_id = Some(version_id);
        state.selected_versions.remove(&task_id);
        Arc::make_mut(&mut state.records).insert(task_id, Arc::new(record));
        state.revision += 1;
        Ok(())
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
        state.selected_versions.remove(&task_id);
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
    selected_history_versions(history, |call_id| {
        batch_state(call_id).map(batch_task_history_versions)
    })
}

pub fn active_task_history_versions_with_outputs<'a>(
    history: &[HistoryItem],
    mut output: impl FnMut(&str) -> Option<&'a ToolOutput>,
) -> HashMap<String, String> {
    selected_history_versions(history, |call_id| {
        output(call_id).map(|output| task_output_history_versions(output, Some(call_id)))
    })
}

fn task_output_history_versions(
    output: &ToolOutput,
    call_id: Option<&str>,
) -> HashMap<String, String> {
    match output {
        ToolOutput::Tasks(cards) => cards
            .iter()
            .filter(|card| !card.call_id.is_empty())
            .map(|card| (card.task_id.clone(), card.call_id.clone()))
            .collect(),
        ToolOutput::Batch { entries, .. } => entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| matches!(entry.tool.as_str(), "task" | "batch"))
            .flat_map(|(index, entry)| {
                let child_call = call_id.map(|id| child_tool_use_id(Some(id), index));
                let mut selected = entry
                    .output
                    .as_ref()
                    .map(|output| task_output_history_versions(output, child_call.as_deref()))
                    .unwrap_or_default();
                if entry.tool == "task"
                    && selected.is_empty()
                    && let Some(child_call) = child_call
                {
                    let task_id = entry
                        .model_suffix
                        .as_deref()
                        .and_then(metadata_task_id)
                        .or_else(|| {
                            entry
                                .raw_input
                                .as_ref()
                                .and_then(|input| input.get("task_id"))
                                .and_then(serde_json::Value::as_str)
                        })
                        .unwrap_or(&child_call);
                    selected.insert(task_id.to_owned(), child_call);
                }
                selected
            })
            .collect(),
        _ => output
            .state()
            .map(batch_task_history_versions)
            .unwrap_or_default(),
    }
}

fn metadata_task_id(output: &str) -> Option<&str> {
    output
        .rsplit_once("<task_metadata>")?
        .1
        .split_once("</task_metadata>")?
        .0
        .lines()
        .find_map(|line| line.trim().strip_prefix("task_id: "))
}

fn selected_history_versions(
    history: &[HistoryItem],
    mut output_versions: impl FnMut(&str) -> Option<HashMap<String, String>>,
) -> HashMap<String, String> {
    let results: HashMap<_, _> = history
        .iter()
        .filter_map(|item| match &item.kind {
            HistoryItemKind::ToolResult {
                call_id, content, ..
            } => Some((call_id.as_str(), content.as_str())),
            _ => None,
        })
        .collect();
    let mut versions = HashMap::new();
    for item in history {
        match &item.kind {
            HistoryItemKind::ToolCall {
                call_id,
                name,
                input,
                ..
            } if name == "task" => {
                let typed = output_versions(call_id).unwrap_or_default();
                if !typed.is_empty() {
                    versions.extend(typed);
                    continue;
                }
                let task_id = results
                    .get(call_id.as_str())
                    .and_then(|output| metadata_task_id(output))
                    .or_else(|| input.get("task_id").and_then(serde_json::Value::as_str))
                    .unwrap_or(call_id);
                versions.insert(task_id.to_owned(), call_id.clone());
            }
            HistoryItemKind::ToolCall { call_id, name, .. } if name == "batch" => {
                if let Some(selected) = output_versions(call_id) {
                    versions.extend(selected);
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
                if values
                    .get("tool")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|tool| !matches!(tool, "task" | "batch"))
                {
                    return;
                }
                if let Some(cards) = values.get("Tasks") {
                    if let Ok(output) =
                        serde_json::from_value::<ToolOutput>(serde_json::json!({"Tasks": cards}))
                    {
                        versions.extend(task_output_history_versions(&output, None));
                    }
                    return;
                }
                if values.get("tool").and_then(serde_json::Value::as_str) == Some("task") {
                    if let Some(output) = values.get("output")
                        && let Ok(output) = serde_json::from_value::<ToolOutput>(output.clone())
                    {
                        let typed = task_output_history_versions(&output, None);
                        if !typed.is_empty() {
                            versions.extend(typed);
                            return;
                        }
                    }
                    let task_id = values
                        .get("output")
                        .and_then(serde_json::Value::as_str)
                        .and_then(metadata_task_id);
                    let invocation_id = values
                        .get("call_id")
                        .or_else(|| values.get("invocation_id"))
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
    pub(crate) fn with_spec(mut self, spec: SubagentTaskSpec) -> Self {
        self.spec = Some(spec);
        self
    }

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
    use std::sync::{Barrier, mpsc};
    use std::thread;
    use test_case::test_case;

    const TASK_ID: &str = "task-1";
    const UNKNOWN_ID: &str = "missing";
    const FIRST_PROMPT: &str = "first prompt";
    const SECOND_PROMPT: &str = "second prompt";
    const PROFILE: &str = "review";
    const OTHER_PROFILE: &str = "custom";
    const PHRASE: &str = "happy-cute-tick";
    const LAUNCH: &str = "launch-call";
    const CONTINUATION: &str = "continuation-call";
    const RUNTIME: &str = "runtime-invocation-not-history";
    const LABEL: &str = "Implement active footer chips";
    const LABEL_ID: &str = "implement-active-footer-chips";
    const SECOND_ID: &str = "implement-active-footer-chips-2";
    const THIRD_ID: &str = "implement-active-footer-chips-3";
    const LOOKUP_ERROR: &str = "persisted identity lookup failed";

    #[test_case(false; "active")]
    #[test_case(true; "completed")]
    fn generated_reservation_skips_live_and_completed_collisions(completed: bool) {
        let store = SubagentHistoryStore::default();
        let occupied = store.reserve(LABEL_ID).unwrap();
        let active = if completed {
            occupied.complete(history(FIRST_PROMPT));
            None
        } else {
            Some(occupied)
        };
        let lease = store.reserve_generated(LABEL, |_| Ok(false)).unwrap();
        assert_eq!(lease.task_id(), SECOND_ID);
        assert!(store.is_active(SECOND_ID));
        drop(lease);
        assert!(!store.is_active(SECOND_ID));
        drop(active);
    }

    #[test_case(false; "live")]
    #[test_case(true; "durable")]
    fn generated_reservation_is_bounded_and_skips_durable_ids(durable: bool) {
        let store = SubagentHistoryStore::default();
        let occupied: Vec<_> = if durable {
            Vec::new()
        } else {
            DescriptiveIdCandidates::new(LABEL, "task")
                .map(|id| store.reserve(id).unwrap())
                .collect()
        };
        let mut attempts = 0;
        let error = store
            .reserve_generated(LABEL, |_| {
                attempts += 1;
                Ok(durable)
            })
            .unwrap_err();
        assert_eq!(error, TASK_ID_EXHAUSTED);
        assert_eq!(
            attempts,
            if durable {
                DescriptiveIdCandidates::new(LABEL, "task").count()
            } else {
                0
            }
        );
        drop(occupied);
        assert_eq!(store.active_count(), 0);
        let lease = store
            .reserve_generated(LABEL, |id| Ok(id == LABEL_ID))
            .unwrap();
        assert_eq!(lease.task_id(), SECOND_ID);
    }

    #[test_case("MAIN", "main-2"; "reserved_main")]
    #[test_case("!!!", "task"; "fallback")]
    #[test_case(LABEL, LABEL_ID; "description")]
    fn generated_reservation_normalizes_labels(label: &str, expected: &str) {
        let store = SubagentHistoryStore::default();
        let lease = store.reserve_generated(label, |_| Ok(false)).unwrap();
        assert_eq!(lease.task_id(), expected);
    }

    #[test]
    fn generated_reservation_skips_selected_versions_and_reuses_first_gap() {
        let store = SubagentHistoryStore::seeded_with_versions(
            HashMap::new(),
            HashMap::new(),
            HashMap::from([(LABEL_ID.into(), RUNTIME.into())]),
        );
        let third = store.reserve(THIRD_ID).unwrap();
        let second = store.reserve_generated(LABEL, |_| Ok(false)).unwrap();
        assert_eq!(second.task_id(), SECOND_ID);
        drop(second);
        let second = store
            .reserve_generated("IMPLEMENT-active/footer/chips", |_| Ok(false))
            .unwrap();
        assert_eq!(second.task_id(), SECOND_ID);
        drop((second, third));
    }

    #[test]
    fn generated_reservation_propagates_lookup_failure_without_leaking_lease() {
        let store = SubagentHistoryStore::default();
        let error = store
            .reserve_generated(LABEL, |_| Err(LOOKUP_ERROR.into()))
            .unwrap_err();
        assert_eq!(error, LOOKUP_ERROR);
        assert_eq!(store.active_count(), 0);
        let lease = store.reserve_generated(LABEL, |_| Ok(false)).unwrap();
        assert_eq!(lease.task_id(), LABEL_ID);
    }

    #[test_case(false; "collision")]
    #[test_case(true; "error")]
    fn delayed_lookup_releases_provisional_reservation(fail: bool) {
        let store = SubagentHistoryStore::default();
        let (candidate_tx, candidate_rx) = mpsc::channel();
        let (lookup_tx, lookup_rx) = mpsc::channel();
        let lookup_store = store.clone();
        let job = thread::spawn(move || {
            lookup_store.reserve_generated(LABEL, |id| {
                assert!(lookup_store.state.try_lock().unwrap().active.contains(id));
                candidate_tx.send(id.to_owned()).unwrap();
                lookup_rx.recv().unwrap()
            })
        });

        assert_eq!(candidate_rx.recv().unwrap(), LABEL_ID);
        assert!(store.is_active(LABEL_ID));
        assert!(store.snapshot().records().is_empty());
        let unrelated = store.reserve(TASK_ID).unwrap();
        drop(unrelated);
        if fail {
            lookup_tx.send(Err(LOOKUP_ERROR.into())).unwrap();
            assert_eq!(job.join().unwrap().unwrap_err(), LOOKUP_ERROR);
        } else {
            lookup_tx.send(Ok(true)).unwrap();
            assert_eq!(candidate_rx.recv().unwrap(), SECOND_ID);
            assert!(!store.is_active(LABEL_ID));
            assert!(store.is_active(SECOND_ID));
            lookup_tx.send(Ok(false)).unwrap();
            let lease = job.join().unwrap().unwrap();
            assert_eq!(lease.task_id(), SECOND_ID);
            drop(lease);
        }
        assert_eq!(store.active_count(), 0);
        let lease = store.reserve_generated(LABEL, |_| Ok(false)).unwrap();
        assert_eq!(lease.task_id(), LABEL_ID);
    }

    #[test]
    fn concurrent_allocators_reserve_distinct_candidates_atomically() {
        let store = SubagentHistoryStore::default();
        let barrier = Arc::new(Barrier::new(2));
        let jobs: Vec<_> = (0..2)
            .map(|_| {
                let store = store.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    store
                        .reserve_generated(LABEL, |id| {
                            assert!(store.is_active(id));
                            barrier.wait();
                            Ok(false)
                        })
                        .unwrap()
                })
            })
            .collect();
        let leases: Vec<_> = jobs.into_iter().map(|job| job.join().unwrap()).collect();
        assert_ne!(leases[0].task_id(), leases[1].task_id());
        assert_eq!(
            leases
                .iter()
                .map(|lease| lease.task_id())
                .collect::<HashSet<_>>(),
            HashSet::from([LABEL_ID, SECOND_ID])
        );
        assert_eq!(store.active_count(), 2);
        drop(leases);
        assert_eq!(store.active_count(), 0);
    }

    fn task_call(call_id: &str, name: &str, task_id: Option<&str>) -> HistoryItem {
        let id = caudra_storage::id::CaudraId::generate();
        HistoryItem {
            id,
            parent_id: None,
            supersedes: None,
            stands_for: None,
            group_id: id,
            kind: HistoryItemKind::ToolCall {
                call_id: call_id.into(),
                name: name.into(),
                input: serde_json::json!({"task_id": task_id}),
                thought_signature: None,
                source: None,
            },
        }
    }

    fn card(task_id: &str, call_id: &str) -> ToolOutput {
        serde_json::from_value(serde_json::json!({"Tasks": [{
            "task_id": task_id, "call_id": call_id, "invocation_id": RUNTIME,
            "root_call_id": LAUNCH, "label": FIRST_PROMPT, "state": "succeeded", "mode": "build",
            "background": true, "generation": 1, "created_at": 0, "updated_at": 0,
            "result": null, "result_preview": null, "result_truncated": false, "reports": [], "reports_truncated": false
        }]})).unwrap()
    }

    #[test_case("task"; "direct")]
    #[test_case("batch"; "batch")]
    fn typed_launch_versions_survive_reload_and_rewind(name: &str) {
        let wrap = |output: ToolOutput| {
            if name == "batch" {
                serde_json::from_value(serde_json::json!({"Batch": {"entries": [{"tool": "task", "summary": FIRST_PROMPT, "status": "Success", "output": output}], "text": ""}})).unwrap()
            } else {
                output
            }
        };
        let first = wrap(card(PHRASE, LAUNCH));
        let next = wrap(card(PHRASE, CONTINUATION));
        let history = vec![
            task_call(LAUNCH, name, None),
            task_call(CONTINUATION, name, Some(PHRASE)),
        ];
        let encoded = serde_json::to_string(&history).unwrap();
        let restored: Vec<HistoryItem> = serde_json::from_str(&encoded).unwrap();
        let outputs = |id: &str| Some(if id == LAUNCH { &first } else { &next });
        assert_eq!(
            active_task_history_versions_with_outputs(&restored, outputs)[PHRASE],
            CONTINUATION
        );
        assert_eq!(
            active_task_history_versions_with_outputs(&restored[..1], outputs)[PHRASE],
            LAUNCH
        );
    }

    #[test]
    fn foreground_batch_metadata_uses_the_stable_child_launch_after_reload() {
        let output: ToolOutput = serde_json::from_value(serde_json::json!({"Batch": {
            "entries": [{"tool": "task", "summary": FIRST_PROMPT, "status": "Success",
                "output": {"Markdown": {"text": FIRST_PROMPT}},
                "model_suffix": format!("<task_metadata>\ntask_id: {PHRASE}\n</task_metadata>")
            }], "text": ""
        }}))
        .unwrap();
        let output: ToolOutput =
            serde_json::from_str(&serde_json::to_string(&output).unwrap()).unwrap();
        let history = [
            task_call(LAUNCH, "batch", None),
            task_call(CONTINUATION, "batch", None),
        ];
        let selected = active_task_history_versions_with_outputs(&history, |_| Some(&output));
        assert_eq!(selected[PHRASE], child_tool_use_id(Some(CONTINUATION), 0));
        let rewound = active_task_history_versions_with_outputs(&history[..1], |_| Some(&output));
        assert_eq!(rewound[PHRASE], child_tool_use_id(Some(LAUNCH), 0));
    }

    #[test_case(PHRASE; "phrase")]
    #[test_case(TASK_ID; "legacy")]
    fn direct_result_metadata_selects_actual_id_without_an_output_snapshot(task_id: &str) {
        let call = task_call(LAUNCH, "task", None);
        let mut result = call.clone();
        result.kind = HistoryItemKind::ToolResult {
            call_id: LAUNCH.into(),
            content: format!("<task_metadata>\ntask_id: {task_id}\n</task_metadata>"),
            is_error: false,
            output_ref: None,
            images: Vec::new(),
            refused_calls: Vec::new(),
        };
        let history = [call, result, task_call(CONTINUATION, "task", Some(task_id))];
        assert_eq!(
            active_task_history_versions(&history)[task_id],
            CONTINUATION
        );
        assert_eq!(active_task_history_versions(&history[..2])[task_id], LAUNCH);
    }

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
            stands_for: None,
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
            stands_for: None,
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
