use std::collections::{HashMap, HashSet};
use std::iter;
use std::sync::Arc;

use caudra_providers::{
    HistoryItem, HistoryItemKind, HistoryProjectionError, active_history_items,
    resolve_history_head,
};
use caudra_storage::id::SessionRef;
use caudra_storage::sessions::{PermissionMode, SessionError, StoredSubagentTaskSpec};
use caudra_storage::tool_outputs::ToolOutputId;
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_storage::{StateDir, StorageError};
use serde_json::Value;
use thiserror::Error;

use crate::{
    History, StoredSession, active_task_history_versions_with_outputs, recover_session_activity,
};

const TASK_METADATA_OPEN: &str = "<task_metadata>";
const TASK_METADATA_CLOSE: &str = "</task_metadata>";
const TASK_ID_PREFIX: &str = "task_id: ";

#[derive(Debug, Error)]
pub enum SessionForkError {
    #[error("fork target session {0} must differ from its source")]
    SameSession(SessionRef),
    #[error("fork target session {0} already exists")]
    TargetExists(SessionRef),
    #[error("check whether fork target session {target} exists: {source}")]
    TargetLookup {
        target: SessionRef,
        source: Box<SessionError>,
    },
    #[error("resolve the history to fork: {0}")]
    History(Box<HistoryProjectionError>),
    #[error("save fork session {target}: {source}")]
    Save {
        target: SessionRef,
        source: Box<SessionError>,
    },
}

impl From<HistoryProjectionError> for SessionForkError {
    fn from(error: HistoryProjectionError) -> Self {
        Self::History(Box::new(error))
    }
}

/// Where a fork lands and what it keeps of its source's authority.
pub struct ForkTarget<'a> {
    pub id: &'a SessionRef,
    pub cwd: &'a str,
    pub workspace_binding: Option<&'a StoredWorkspaceBinding>,
    /// Conversation approval rules never carry over; the mode does only when
    /// the caller passes one.
    pub permission_mode: Option<PermissionMode>,
}

/// Copies the active branch of `source`, the subagent histories it reaches,
/// and the tool outputs they reference into a new stored session, in one
/// save that leaves no fork behind when any part fails. The caller holds
/// `target.id`'s lease. Returns the fork's history, rebased onto fresh item
/// ids.
pub fn fork_stored_session(
    storage: &StateDir,
    source: &StoredSession,
    target: ForkTarget<'_>,
) -> Result<Vec<HistoryItem>, SessionForkError> {
    if target.id.id() == source.id {
        return Err(SessionForkError::SameSession(target.id.clone()));
    }
    ensure_fork_target_available(storage, target.id)?;
    let head = resolve_history_head(
        source.messages(),
        source.meta.history_head,
        source.meta.pending_revert.is_some(),
    );
    let active = active_history_items(source.messages(), head)?;
    let tasks = ReachableTasks::of(&active, source);
    let history = rebase_history(active)?;
    let subagent_histories = tasks
        .histories(source)
        .map(|(task_id, items)| {
            rebase_history(items.as_ref().clone()).map(|history| (task_id.clone(), history))
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    save_fork(storage, source, &target, &history, subagent_histories)?;
    Ok(history)
}

fn ensure_fork_target_available(
    storage: &StateDir,
    target: &SessionRef,
) -> Result<(), SessionForkError> {
    match crate::load_stored_session(target.id(), storage) {
        Ok(_) => Err(SessionForkError::TargetExists(target.clone())),
        Err(SessionError::Storage(StorageError::NotFound(_))) => Ok(()),
        Err(source) => Err(SessionForkError::TargetLookup {
            target: target.clone(),
            source: Box::new(source),
        }),
    }
}

fn rebase_history(items: Vec<HistoryItem>) -> Result<Vec<HistoryItem>, HistoryProjectionError> {
    let messages = History::restored(items)?.into_vec();
    Ok(History::new(messages).into_items())
}

/// Every output `histories` reference, each once.
pub fn history_output_ids<'a>(
    histories: impl IntoIterator<Item = &'a [HistoryItem]>,
) -> Vec<ToolOutputId> {
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for item in histories.into_iter().flatten() {
        let output_refs = match &item.kind {
            HistoryItemKind::ToolResult {
                output_ref: Some(output_ref),
                ..
            } => std::slice::from_ref(output_ref),
            HistoryItemKind::AssistantText {
                retained_output_refs,
                ..
            }
            | HistoryItemKind::User {
                retained_output_refs,
                ..
            } => retained_output_refs,
            _ => &[],
        };
        for output_ref in output_refs {
            if seen.insert(&output_ref.id) {
                ids.push(output_ref.id.clone());
            }
        }
    }
    ids
}

fn save_fork(
    storage: &StateDir,
    source: &StoredSession,
    target: &ForkTarget<'_>,
    history: &[HistoryItem],
    subagent_histories: HashMap<String, Vec<HistoryItem>>,
) -> Result<(), SessionForkError> {
    let mut fork = target.workspace_binding.map_or_else(
        || StoredSession::new(&source.model, target.cwd),
        |binding| StoredSession::new_with_workspace(&source.model, target.cwd, binding.clone()),
    );
    fork.id = target.id.id();
    fork.meta.system_prompt_profile = source.meta.system_prompt_profile.clone();
    fork.meta.permission_mode = target.permission_mode.clone();
    fork.meta.thinking = source.meta.thinking.clone();
    fork.meta.fast = source.meta.fast;
    fork.replace_messages(history.to_vec());
    fork.set_title(format!("{} (fork)", source.title));
    fork.copy_saved_outputs_from(
        source.id,
        history_output_ids(
            iter::once(history).chain(subagent_histories.values().map(Vec::as_slice)),
        ),
    );
    let mut stored_tool_ids = all_tool_call_ids(history);
    for history in subagent_histories.values() {
        stored_tool_ids.extend(all_tool_call_ids(history));
    }
    let copied_task_ids: HashSet<String> = subagent_histories.keys().cloned().collect();
    let versions = active_task_history_versions_with_outputs(history, |call_id| {
        source.tool_outputs().get(call_id).map(Arc::as_ref)
    });
    for (task_id, history) in subagent_histories {
        if let Some(version_id) = versions
            .get(&task_id)
            .filter(|version| *version != &task_id)
        {
            fork.set_subagent_history(
                version_id.clone(),
                history.clone(),
                Some(StoredSubagentTaskSpec::version()),
            );
        }
        let spec = source.subagent_task_specs().get(&task_id).cloned();
        fork.set_subagent_history(task_id, history, spec);
    }
    fork.set_subagents(
        source
            .subagents()
            .iter()
            .filter(|subagent| copied_task_ids.contains(&subagent.tool_use_id))
            .cloned()
            .collect(),
    );
    for tool_id in stored_tool_ids {
        if let Some(output) = source.tool_outputs().get(&tool_id) {
            fork.insert_tool_output(tool_id, output.as_ref().clone());
        }
    }
    fork.meta.activity = None;
    fork.meta.activity = Some(recover_session_activity(&fork));
    fork.save(storage).map_err(|source| SessionForkError::Save {
        target: target.id.clone(),
        source: Box::new(source),
    })
}

/// The subagent tasks a history still reaches, and the version stream each
/// one selected.
pub(crate) struct ReachableTasks {
    pub(crate) ids: HashSet<String>,
    pub(crate) versions: HashMap<String, String>,
}

impl ReachableTasks {
    pub(crate) fn of(history: &[HistoryItem], session: &StoredSession) -> Self {
        let mut ids = reachable_subagent_ids(history, session);
        let mut versions = active_task_history_versions_with_outputs(history, |call_id| {
            session.tool_outputs().get(call_id).map(Arc::as_ref)
        });
        ids.extend(versions.keys().cloned());
        for subagent in session.subagents() {
            if ids.contains(&subagent.tool_use_id)
                && !versions.contains_key(&subagent.tool_use_id)
                && let Some(version_id) = &subagent.parent_tool_use_id
                && session.subagent_messages().contains_key(version_id)
            {
                versions.insert(subagent.tool_use_id.clone(), version_id.clone());
            }
        }
        Self { ids, versions }
    }

    /// Each reachable task's selected history. A stream that only serves as
    /// another task's selected version is not a task of its own.
    pub(crate) fn histories<'a>(
        &'a self,
        session: &'a StoredSession,
    ) -> impl Iterator<Item = (&'a String, &'a Arc<Vec<HistoryItem>>)> {
        let version_ids: HashSet<&str> = self
            .versions
            .iter()
            .filter_map(|(task_id, version_id)| {
                (task_id != version_id && session.subagent_messages().contains_key(version_id))
                    .then_some(version_id.as_str())
            })
            .collect();
        self.ids
            .iter()
            .filter(move |task_id| !version_ids.contains(task_id.as_str()))
            .filter_map(|task_id| {
                let version_id = self.versions.get(task_id).unwrap_or(task_id);
                session
                    .subagent_messages()
                    .get(version_id)
                    .map(|items| (task_id, items))
            })
    }
}

pub(crate) fn reachable_subagent_ids(
    history: &[HistoryItem],
    session: &StoredSession,
) -> HashSet<String> {
    let mut reachable = history_task_ids(history);
    let mut active_calls = crate::history_tool_call_ids(history);
    expand_reachable_subagents(&mut reachable, &mut active_calls, session);
    let legacy_fallback = reachable.is_empty()
        || history.iter().any(|item| {
            matches!(
                &item.kind,
                HistoryItemKind::AssistantText {
                    retained_subagent_ids,
                    is_compaction_summary: true,
                    ..
                } if retained_subagent_ids.is_empty()
            )
        });
    reachable.extend(
        session
            .subagent_task_specs()
            .iter()
            .filter(|(task_id, spec)| {
                if !spec.is_generic() {
                    return false;
                }
                let descriptor = session
                    .subagents()
                    .iter()
                    .find(|subagent| subagent.tool_use_id == **task_id);
                descriptor.is_none()
                    || active_calls.contains(*task_id)
                    || descriptor
                        .and_then(|subagent| subagent.root_tool_use_id.as_ref())
                        .is_some_and(|root| active_calls.contains(root))
            })
            .map(|(task_id, _)| task_id.clone()),
    );
    if legacy_fallback {
        reachable.extend(
            session
                .subagent_messages()
                .keys()
                .filter(|task_id| !session.subagent_task_specs().contains_key(*task_id))
                .filter(|task_id| {
                    !session.subagents().iter().any(|subagent| {
                        subagent.tool_use_id.as_str() != task_id.as_str()
                            && subagent.parent_tool_use_id.as_ref() == Some(*task_id)
                    })
                })
                .cloned(),
        );
    }
    expand_reachable_subagents(&mut reachable, &mut active_calls, session);
    reachable
}

fn expand_reachable_subagents(
    reachable: &mut HashSet<String>,
    active_calls: &mut HashSet<String>,
    session: &StoredSession,
) {
    let mut visited = HashSet::new();
    loop {
        for subagent in session.subagents() {
            if subagent
                .parent_tool_use_id
                .as_ref()
                .is_some_and(|parent| reachable.contains(parent) || active_calls.contains(parent))
                || subagent
                    .root_tool_use_id
                    .as_ref()
                    .is_some_and(|root| active_calls.contains(root))
            {
                reachable.insert(subagent.tool_use_id.clone());
            }
        }
        let Some(task_id) = reachable
            .iter()
            .find(|task_id| !visited.contains(*task_id))
            .cloned()
        else {
            break;
        };
        visited.insert(task_id.clone());
        if let Some(state) = session
            .tool_outputs()
            .get(&task_id)
            .and_then(|output| output.state())
        {
            collect_task_metadata_from_value(state, reachable);
        }
        if let Some(nested) = session.subagent_messages().get(&task_id) {
            reachable.extend(history_task_ids(nested));
            active_calls.extend(crate::history_tool_call_ids(nested));
        }
    }
}

fn history_task_ids(items: &[HistoryItem]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for item in items {
        match &item.kind {
            HistoryItemKind::ToolCall { call_id, name, .. }
                if crate::tools::is_container_tool(name) =>
            {
                ids.insert(call_id.clone());
            }
            HistoryItemKind::AssistantText {
                retained_subagent_ids,
                ..
            } => ids.extend(retained_subagent_ids.iter().cloned()),
            _ => {}
        }
    }
    ids
}

fn all_tool_call_ids(items: &[HistoryItem]) -> HashSet<String> {
    items
        .iter()
        .filter_map(|item| match &item.kind {
            HistoryItemKind::ToolCall { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect()
}

fn collect_task_metadata_from_value(value: &Value, ids: &mut HashSet<String>) {
    match value {
        Value::String(text) => collect_task_metadata(text, ids),
        Value::Array(values) => {
            for value in values {
                collect_task_metadata_from_value(value, ids);
            }
        }
        Value::Object(values) => {
            if let Some(tool) = values.get("tool").and_then(Value::as_str) {
                if tool == crate::tools::TASK_TOOL_NAME {
                    if let Some(invocation_id) = values.get("invocation_id").and_then(Value::as_str)
                    {
                        ids.insert(invocation_id.to_owned());
                    }
                    if let Some(output) = values.get("output").and_then(Value::as_str) {
                        collect_task_metadata(output, ids);
                    }
                }
                return;
            }
            for value in values.values() {
                collect_task_metadata_from_value(value, ids);
            }
        }
        _ => {}
    }
}

fn collect_task_metadata(content: &str, ids: &mut HashSet<String>) {
    for block in content.split(TASK_METADATA_OPEN).skip(1) {
        let Some(metadata) = block.split(TASK_METADATA_CLOSE).next() else {
            continue;
        };
        if let Some(task_id) = metadata
            .lines()
            .find_map(|line| line.trim().strip_prefix(TASK_ID_PREFIX))
        {
            ids.insert(task_id.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use caudra_providers::{ContentBlock, Message, Role, TaskEventOrigin};
    use caudra_storage::background::TaskRecord;
    use caudra_storage::permission_state::PermissionRuleRecord;
    use caudra_storage::sessions::{
        StoredMode, StoredPlanTarget, StoredSubagent, StoredSubagentOutcome,
    };
    use caudra_storage::thinking::StoredThinking;
    use caudra_storage::tool_outputs::{ToolOutputError, ToolOutputRef, ToolOutputStore};
    use std::path::Path;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::permissions::{PermissionLifetime, PermissionRequest};
    use crate::tools::TASK_TOOL_NAME;
    use crate::{SubagentTaskSpec, TaskCard, TextOutput, ToolOutput};

    const MODEL_SPEC: &str = "provider/model";
    const CWD: &str = "/repo";
    const FORK_CWD: &str = "/fork-target";
    const FORK_HISTORY: &str = "history retained without source authority";
    const SOURCE_PLAN: &str = "/source/plan.md";
    const FORK_LEVEL: &str = "high";
    const PHRASE_TASK: &str = "happy-cute-tick";
    const TASK_CALL: &str = "task-launch-call";
    const TASK_INVOCATION: &str = "task-runtime-invocation";
    const TASK_EVENT: &str = "task-result-event";
    const TASK_SUCCEEDED: &str = "succeeded";
    const LARGE_TASK_RESULT_BYTES: usize = 64 * 1024;
    const COMPLETE_ARTIFACT: &str = "complete artifact";
    const NESTED_ARTIFACT: &str = "nested artifact";
    const OUTPUT_ID: &str = "calm-blue-wren";
    const PROFILE: &str = "review";
    const SANDBOX_ANCHOR: &str = "https://sandbox.test";

    fn storage(temp: &TempDir) -> StateDir {
        StateDir::from_path(temp.path().join("state"))
    }

    fn history_messages(items: Vec<HistoryItem>) -> Vec<Message> {
        History::restored(items).unwrap().into_vec()
    }

    fn tool_call(id: &str, name: &str) -> Vec<HistoryItem> {
        History::new(vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(id, name, serde_json::json!({}))],
            ..Default::default()
        }])
        .into_items()
    }

    fn target(id: &SessionRef) -> ForkTarget<'_> {
        ForkTarget {
            id,
            cwd: FORK_CWD,
            workspace_binding: None,
            permission_mode: None,
        }
    }

    fn stored_structured_rule() -> PermissionRuleRecord {
        let request = PermissionRequest::from_legacy(
            "stored-structured".into(),
            caudra_config::ToolKey::native("bash"),
            vec!["cargo test".into()],
            serde_json::json!({"command": "cargo test"}),
            Path::new(CWD),
            false,
        );
        PermissionRuleRecord::conversation(
            request
                .option_rule("allow_exact", PermissionLifetime::Conversation)
                .unwrap(),
        )
        .unwrap()
    }

    fn task_record() -> TaskRecord {
        TaskRecord {
            payload: Default::default(),
            owner: Default::default(),
            created_at: 0,
            updated_at: 0,
            sequence: 1,
            task_id: PHRASE_TASK.into(),
            invocation_id: TASK_INVOCATION.into(),
            root_call_id: TASK_CALL.into(),
            generation: 1,
            state: TASK_SUCCEEDED.into(),
            background: true,
            receipt_accepted: true,
            mode: "build".into(),
            request: serde_json::json!({"call_id": TASK_CALL, "label": PHRASE_TASK}),
            outcome: Some(serde_json::json!({
                "task_id": PHRASE_TASK, "mode": "build", "success": true, "cancelled": false,
                "output": Value::Null, "error": null, "tokens_used": 0, "duration_ms": 0
            })),
            output_ref: None,
            spec: Value::Null,
            events: Vec::new(),
        }
    }

    fn generic_subagent(id: &str, root: &str) -> StoredSubagent {
        StoredSubagent {
            tool_use_id: id.into(),
            parent_tool_use_id: Some(id.into()),
            root_tool_use_id: Some(root.into()),
            name: id.into(),
            model: None,
            thinking: None,
            fast: false,
            outcome: StoredSubagentOutcome::Unknown,
        }
    }

    #[test]
    fn batch_state_accepts_task_metadata_only_from_task_children() {
        let metadata =
            |task_id: &str| format!("<task_metadata>\ntask_id: {task_id}\n</task_metadata>");
        let mut ids = HashSet::new();

        collect_task_metadata_from_value(
            &serde_json::json!([
                { "tool": "task", "output": metadata("real-task") },
                { "tool": "bash", "output": metadata("forged-task") },
            ]),
            &mut ids,
        );

        assert!(ids.contains("real-task"));
        assert!(!ids.contains("forged-task"));
    }

    #[test]
    fn reachability_follows_nested_generic_session_parents() {
        let main = tool_call("generic-root", "custom_session_tool");
        let mut session = StoredSession::new(MODEL_SPEC, CWD);
        session.replace_messages(main.clone());
        session.set_subagent_history(
            "generic-root".into(),
            tool_call("generic-nested", "custom_session_tool"),
            Some(SubagentTaskSpec::generic()),
        );
        session.set_subagent_history(
            "generic-nested".into(),
            Vec::new(),
            Some(SubagentTaskSpec::generic()),
        );
        session.set_subagents(vec![
            generic_subagent("generic-root", "generic-root"),
            generic_subagent("generic-nested", "generic-root"),
        ]);

        let reachable = reachable_subagent_ids(&main, &session);

        assert!(reachable.contains("generic-root"));
        assert!(reachable.contains("generic-nested"));
    }

    #[test]
    fn reachability_excludes_other_branches_and_includes_nested_tasks() {
        let mut session = StoredSession::new(MODEL_SPEC, CWD);
        let main = tool_call("task-live", TASK_TOOL_NAME);
        session.set_subagent_history(
            "task-live".into(),
            tool_call("task-nested", TASK_TOOL_NAME),
            Some(StoredSubagentTaskSpec::default()),
        );
        for task in ["task-nested", "task-other-branch"] {
            session.set_subagent_history(
                task.into(),
                Vec::new(),
                Some(StoredSubagentTaskSpec::default()),
            );
        }
        let expected = HashSet::from(["task-live".into(), "task-nested".into()]);

        assert_eq!(reachable_subagent_ids(&main, &session), expected);

        let compacted = History::new(vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "summary".into(),
            }],
            retained_subagent_ids: vec!["task-live".into()],
            is_compaction_summary: true,
            ..Default::default()
        }])
        .into_items();
        assert_eq!(reachable_subagent_ids(&compacted, &session), expected);
    }

    #[test_case(1; "single_observation")]
    #[test_case(3; "repeated_observations_and_refs")]
    fn retained_output_refs_are_listed_once_across_history_kinds(copies: usize) {
        let reference = ToolOutputRef {
            id: OUTPUT_ID.parse().unwrap(),
            byte_count: COMPLETE_ARTIFACT.len(),
            line_count: 1,
        };
        let mut observation = Message::observation(FORK_HISTORY.into());
        observation.retained_output_refs = vec![reference.clone(); copies];
        let mut messages = vec![observation; copies];
        messages.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: COMPLETE_ARTIFACT.into(),
            }],
            retained_output_refs: vec![reference.clone()],
            ..Default::default()
        });
        let items = History::new(messages).into_items();

        assert_eq!(history_output_ids([items.as_slice()]), [reference.id]);
    }

    #[test]
    fn fork_rebases_item_identity_without_changing_provider_content() {
        let items = History::new(vec![
            Message::user("hello".into()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "first".into(),
                    },
                    ContentBlock::Text {
                        text: "second".into(),
                    },
                ],
                ..Default::default()
            },
        ])
        .into_items();
        let original_ids: Vec<_> = items.iter().map(|item| item.id).collect();
        let expected = serde_json::to_value(history_messages(items.clone())).unwrap();

        let forked = rebase_history(items).unwrap();

        assert!(forked.iter().all(|item| !original_ids.contains(&item.id)));
        assert_eq!(
            serde_json::to_value(history_messages(forked)).unwrap(),
            expected
        );
    }

    #[test]
    fn fork_copies_main_and_subagent_output_refs_to_target_session() {
        let temp = TempDir::new().unwrap();
        let storage = storage(&temp);
        let store = ToolOutputStore::new(storage.clone());
        let mut source = StoredSession::new(MODEL_SPEC, CWD);
        source.save(&storage).unwrap();
        let target_id = SessionRef::generate();
        let output_ref = store.put(source.id, COMPLETE_ARTIFACT).unwrap();
        let retained_ref = store.put(source.id, NESTED_ARTIFACT).unwrap();
        let history = rebase_history(
            History::new(vec![
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::tool_use(
                        "call-1",
                        "bash",
                        serde_json::json!({}),
                    )],
                    ..Default::default()
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call-1".into(),
                        content: "preview".into(),
                        is_error: false,
                        output_ref: Some(output_ref.clone()),
                    }],
                    ..Default::default()
                },
            ])
            .into_items(),
        )
        .unwrap();
        let compacted = rebase_history(
            History::new(vec![Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: format!("retained output ID: {}", retained_ref.id),
                }],
                retained_output_refs: vec![retained_ref.clone()],
                ..Default::default()
            }])
            .into_items(),
        )
        .unwrap();

        save_fork(
            &storage,
            &source,
            &target(&target_id),
            &history,
            HashMap::from([(PHRASE_TASK.into(), compacted)]),
        )
        .unwrap();

        assert_eq!(
            store
                .load_text(target_id.id(), output_ref.id.clone())
                .unwrap(),
            COMPLETE_ARTIFACT
        );
        assert_eq!(
            store.load_text(target_id.id(), retained_ref.id).unwrap(),
            NESTED_ARTIFACT
        );
        assert!(history.iter().any(|item| matches!(
            &item.kind,
            HistoryItemKind::ToolResult {
                output_ref: Some(reference),
                ..
            } if reference == &output_ref
        )));
    }

    #[test]
    fn fork_copies_task_notice_output_without_a_retrieval_call() {
        let temp = TempDir::new().unwrap();
        let storage = storage(&temp);
        let store = ToolOutputStore::new(storage.clone());
        let mut source = StoredSession::new(MODEL_SPEC, CWD);
        source.save(&storage).unwrap();
        let target_id = SessionRef::generate();
        let complete = "x".repeat(LARGE_TASK_RESULT_BYTES);
        let reference = store.put(source.id, &complete).unwrap();
        let origin = TaskEventOrigin {
            task_id: PHRASE_TASK.into(),
            invocation_id: TASK_INVOCATION.into(),
            event_id: TASK_EVENT.into(),
        };
        let mut notice = Message::task_observation(
            format!("Task {PHRASE_TASK}: success.\n\n[truncated]"),
            origin.clone(),
        );
        notice.retained_output_refs.push(reference.clone());
        let items = History::new(vec![notice]).into_items();
        let restored = serde_json::from_value(serde_json::to_value(items).unwrap()).unwrap();
        let forked = rebase_history(restored).unwrap();

        save_fork(
            &storage,
            &source,
            &target(&target_id),
            &forked,
            HashMap::new(),
        )
        .unwrap();

        assert_eq!(
            store
                .load_text(target_id.id(), reference.id.clone())
                .unwrap(),
            complete
        );
        let messages = history_messages(forked);
        assert_eq!(messages[0].task_event.as_ref(), Some(&origin));
        assert_eq!(messages[0].retained_output_refs, [reference]);
    }

    #[test]
    fn fork_missing_a_referenced_output_leaves_no_fork() {
        let temp = TempDir::new().unwrap();
        let storage = storage(&temp);
        let store = ToolOutputStore::new(storage.clone());
        let mut source = StoredSession::new(MODEL_SPEC, CWD);
        source.save(&storage).unwrap();
        let target_id = SessionRef::generate();
        let kept = store.put(source.id, COMPLETE_ARTIFACT).unwrap();
        let gone = store.put(source.id, NESTED_ARTIFACT).unwrap();
        store.delete(source.id, &gone.id).unwrap();
        let mut message = Message::user(FORK_HISTORY.into());
        message.retained_output_refs = vec![kept.clone(), gone];
        let history = History::new(vec![message]).into_items();

        let result = save_fork(
            &storage,
            &source,
            &target(&target_id),
            &history,
            HashMap::new(),
        );

        assert!(matches!(result, Err(SessionForkError::Save { .. })));
        assert!(ensure_fork_target_available(&storage, &target_id).is_ok());
        assert!(matches!(
            store.load_text(target_id.id(), kept.id),
            Err(ToolOutputError::NotFound { .. })
        ));
    }

    #[test_case(false; "direct")]
    #[test_case(true; "batch")]
    fn fork_preserves_selected_task_version_stream(batch: bool) {
        const ROOT_CALL: &str = "root-launch";
        const SELECTED: &str = "selected earlier history";
        const LATEST: &str = "latest must not replace selected";
        let temp = TempDir::new().unwrap();
        let storage = storage(&temp);
        let mut source = StoredSession::new(MODEL_SPEC, CWD);
        let target_id = SessionRef::generate();
        let output = ToolOutput::Tasks(vec![TaskCard::from(&task_record())]);
        let output = if batch {
            serde_json::from_value(serde_json::json!({"Batch": {
                "entries": [{"tool": "task", "summary": PHRASE_TASK, "status": "Success", "output": output}],
                "text": ""
            }})).unwrap()
        } else {
            output
        };
        source.insert_tool_output(ROOT_CALL.into(), output);
        source.set_subagent_history(
            PHRASE_TASK.into(),
            History::new(vec![Message::user(LATEST.into())]).into_items(),
            Some(StoredSubagentTaskSpec::default()),
        );
        let selected = History::new(vec![Message::user(SELECTED.into())]).into_items();
        source.set_subagent_history(
            TASK_CALL.into(),
            selected.clone(),
            Some(StoredSubagentTaskSpec::version()),
        );
        let history = tool_call(ROOT_CALL, if batch { "batch" } else { TASK_TOOL_NAME });

        save_fork(
            &storage,
            &source,
            &target(&target_id),
            &history,
            HashMap::from([(PHRASE_TASK.into(), selected.clone())]),
        )
        .unwrap();

        let restored = crate::load_stored_session(target_id.id(), &storage).unwrap();
        let versions = active_task_history_versions_with_outputs(restored.messages(), |call_id| {
            restored.tool_outputs().get(call_id).map(Arc::as_ref)
        });
        assert_eq!(versions[PHRASE_TASK], TASK_CALL);
        assert_eq!(restored.subagent_messages()[TASK_CALL].as_ref(), &selected);
        assert!(restored.subagent_task_specs()[TASK_CALL].is_version());
        assert_eq!(
            restored.subagent_messages()[PHRASE_TASK].as_ref(),
            &selected
        );
    }

    #[test]
    fn saved_fork_restores_rebased_subagent_history_and_rejects_collision() {
        let temp = TempDir::new().unwrap();
        let storage = storage(&temp);
        let mut source = StoredSession::new(MODEL_SPEC, CWD);
        source.meta.system_prompt_profile = Some(PROFILE.into());
        source.meta.structured_permission_rules = vec![stored_structured_rule()];
        source.meta.permission_mode = Some(PermissionMode::Yolo);
        source.meta.plan_target = Some(StoredPlanTarget::PlanRef {
            reference: caudra_workspace::PlanRef::new(format!("plan-{}", "a".repeat(32))).unwrap(),
        });
        let target_id = SessionRef::generate();
        let history = History::new(vec![
            Message::user("main".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    "batch-call",
                    "batch",
                    serde_json::json!({}),
                )],
                ..Default::default()
            },
        ])
        .into_items();
        let subagent = History::new(vec![Message::user("nested".into())]).into_items();
        source.insert_tool_output(
            "batch-call".into(),
            ToolOutput::Plain(TextOutput {
                text: "batch output".into(),
                instructions: None,
                state: Some(serde_json::json!([])),
                lua_provenance: None,
            }),
        );

        save_fork(
            &storage,
            &source,
            &target(&target_id),
            &history,
            HashMap::from([("task-1".into(), subagent.clone())]),
        )
        .unwrap();

        let loaded = crate::load_stored_session(target_id.id(), &storage).unwrap();
        assert_eq!(loaded.messages(), history);
        assert_eq!(loaded.subagent_messages()["task-1"].as_ref(), &subagent);
        assert!(loaded.tool_outputs().contains_key("batch-call"));
        assert!(loaded.meta.structured_permission_rules.is_empty());
        assert_eq!(loaded.meta.permission_mode, None);
        assert_eq!(loaded.cwd, FORK_CWD);
        assert!(loaded.meta.plan_target.is_none());
        assert!(loaded.meta.pending_revert.is_none());
        assert_eq!(loaded.meta.system_prompt_profile.as_deref(), Some(PROFILE));
        assert!(matches!(
            ensure_fork_target_available(&storage, &target_id),
            Err(SessionForkError::TargetExists(_))
        ));
        assert!(ensure_fork_target_available(&storage, &SessionRef::generate()).is_ok());
    }

    #[test_case(false; "fork_to_local")]
    #[test_case(true; "fork_to_explicit_sandbox")]
    fn fork_rebinds_without_source_grants_or_plan(remote_target: bool) {
        let directory = TempDir::new().unwrap();
        let storage = storage(&directory);
        let local = StoredWorkspaceBinding::local_from_cwd("/source");
        let remote = serde_json::from_str::<StoredWorkspaceBinding>(
            &serde_json::to_string(&local)
                .unwrap()
                .replace(local.trust_anchor().as_str(), SANDBOX_ANCHOR),
        )
        .unwrap();
        let mut source = StoredSession::new_with_workspace(
            MODEL_SPEC,
            "/source",
            remote
                .clone()
                .with_sandbox_record(SessionRef::generate().id())
                .unwrap(),
        );
        source.meta.structured_permission_rules = vec![stored_structured_rule()];
        source.meta.permission_mode = Some(PermissionMode::Yolo);
        source.meta.mode = Some(StoredMode::Plan);
        source.meta.thinking = Some(StoredThinking::Effort {
            level: FORK_LEVEL.into(),
        });
        source.meta.fast = true;
        source.meta.plan_path = Some(SOURCE_PLAN.into());
        source.meta.plan_target = Some(StoredPlanTarget::LocalPath {
            path: SOURCE_PLAN.into(),
        });
        source
            .replace_messages(History::new(vec![Message::user(FORK_HISTORY.into())]).into_items());
        let binding = remote_target.then(|| {
            remote
                .with_sandbox_record(SessionRef::generate().id())
                .unwrap()
        });
        let target_id = SessionRef::generate();
        let cwd = if remote_target {
            "."
        } else {
            directory.path().to_str().unwrap()
        };

        let history = fork_stored_session(
            &storage,
            &source,
            ForkTarget {
                id: &target_id,
                cwd,
                workspace_binding: binding.as_ref(),
                permission_mode: None,
            },
        )
        .unwrap();

        let fork = crate::open_stored_session(target_id.id(), &storage).unwrap();
        StoredWorkspaceBinding::validate_resume_identity(
            fork.workspace_binding(),
            binding.as_ref(),
        )
        .unwrap();
        assert_ne!(fork.workspace_binding(), source.workspace_binding());
        assert!(fork.meta.structured_permission_rules.is_empty());
        assert_eq!(fork.meta.permission_mode, None);
        assert_eq!(fork.meta.plan_target, None);
        assert_eq!(fork.meta.plan_path, None);
        assert_ne!(fork.meta.mode, Some(StoredMode::Plan));
        assert_eq!(fork.meta.thinking, source.meta.thinking);
        assert!(fork.meta.fast);
        assert_eq!(fork.messages(), history);
        assert_eq!(
            serde_json::to_value(history_messages(history)).unwrap(),
            serde_json::to_value([Message::user(FORK_HISTORY.into())]).unwrap()
        );
    }

    #[test_case(None; "mode_left_to_the_front_end")]
    #[test_case(Some(PermissionMode::Auto); "mode_carried")]
    fn fork_copies_the_active_branch_and_leaves_the_source_alone(mode: Option<PermissionMode>) {
        let temp = TempDir::new().unwrap();
        let storage = storage(&temp);
        let mut source = StoredSession::new(MODEL_SPEC, CWD);
        source.meta.structured_permission_rules = vec![stored_structured_rule()];
        source
            .replace_messages(History::new(vec![Message::user(FORK_HISTORY.into())]).into_items());
        source.save(&storage).unwrap();
        let target_id = SessionRef::generate();

        fork_stored_session(
            &storage,
            &source,
            ForkTarget {
                permission_mode: mode.clone(),
                ..target(&target_id)
            },
        )
        .unwrap();

        let fork = crate::open_stored_session(target_id.id(), &storage).unwrap();
        let reloaded = crate::open_stored_session(source.id, &storage).unwrap();
        assert_ne!(fork.id, source.id);
        assert_eq!(fork.meta.permission_mode, mode);
        assert!(fork.meta.structured_permission_rules.is_empty());
        assert_eq!(
            serde_json::to_value(history_messages(fork.messages().to_vec())).unwrap(),
            serde_json::to_value(history_messages(source.messages().to_vec())).unwrap()
        );
        assert_eq!(reloaded.messages(), source.messages());
        assert_eq!(reloaded.title, source.title);
        assert_eq!(reloaded.meta.structured_permission_rules.len(), 1);
    }

    #[test]
    fn a_session_cannot_be_forked_onto_itself() {
        let temp = TempDir::new().unwrap();
        let source = StoredSession::new(MODEL_SPEC, CWD);
        let same = SessionRef::from(source.id);

        let result = fork_stored_session(&storage(&temp), &source, target(&same));

        assert!(matches!(result, Err(SessionForkError::SameSession(_))));
    }
}
