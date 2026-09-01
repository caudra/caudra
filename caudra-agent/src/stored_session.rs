use std::collections::HashMap;

use caudra_providers::{HistoryItem, Message, TokenUsage, expand_message};
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::{Session, SessionError, TitleSource};
use caudra_storage::{StateDir, StorageError};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::ToolOutput;

pub type StoredSession = Session<HistoryItem, TokenUsage, ToolOutput>;
type CompatibleSession = Session<PersistedHistoryEntry, TokenUsage, ToolOutput>;

#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum PersistedHistoryEntry {
    Item(HistoryItem),
    Legacy(Message),
}

impl TitleSource for PersistedHistoryEntry {
    fn first_user_text(&self) -> Option<&str> {
        match self {
            Self::Item(item) => item.first_user_text(),
            Self::Legacy(message) => message.first_user_text(),
        }
    }
}

pub fn load_stored_session(
    id: CaudraId,
    storage: &StateDir,
) -> Result<StoredSession, SessionError> {
    let session = CompatibleSession::load_compatible(id, storage)?;
    convert_session(session, storage)
}

pub fn latest_stored_session(
    cwd: &str,
    storage: &StateDir,
) -> Result<Option<StoredSession>, SessionError> {
    CompatibleSession::latest_compatible(cwd, storage)?
        .map(|session| convert_session(session, storage))
        .transpose()
}

fn convert_session(
    session: CompatibleSession,
    storage: &StateDir,
) -> Result<StoredSession, SessionError> {
    let write_version = session.persisted_write_version();
    let migrated = session
        .messages()
        .iter()
        .chain(
            session
                .subagent_messages()
                .values()
                .flat_map(|entries| entries.iter()),
        )
        .any(|entry| matches!(entry, PersistedHistoryEntry::Legacy(_)));
    let messages = restore_history(session.messages().iter().cloned());
    let subagent_messages: HashMap<_, _> = session
        .subagent_messages()
        .iter()
        .map(|(task_id, entries)| (task_id.clone(), restore_history(entries.iter().cloned())))
        .collect();
    let mut value = serde_json::to_value(session).map_err(StorageError::from)?;
    value["messages"] = serde_json::to_value(messages).map_err(StorageError::from)?;
    value["subagent_messages"] =
        serde_json::to_value(subagent_messages).map_err(StorageError::from)?;
    let mut session: StoredSession = serde_json::from_value(value).map_err(StorageError::from)?;
    session.set_persisted_write_version(write_version);
    if migrated && let Err(error) = session.save(storage) {
        warn!(
            %error,
            session_id = %session.id,
            "failed to persist migrated session; continuing with readable in-memory session"
        );
    }
    Ok(session)
}

fn restore_history(entries: impl IntoIterator<Item = PersistedHistoryEntry>) -> Vec<HistoryItem> {
    let mut items = Vec::new();
    for entry in entries {
        match entry {
            PersistedHistoryEntry::Item(item) => items.push(item),
            PersistedHistoryEntry::Legacy(message) => {
                let parent_id = items.last().map(|item: &HistoryItem| item.id);
                items.extend(expand_message(&message, parent_id));
            }
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::{
        History, IndexDirectoryEntry, IndexDirectoryEntryKind, IndexLine, IndexLineSemantic,
        IndexOutput, IndexSourceRange, ShellFilterInfo, ShellOutput,
    };
    use caudra_providers::{ContentBlock, Role, active_history_items, resolve_history_head};

    const CWD: &str = "/repo";
    const MODEL: &str = "anthropic/test";

    type LegacySession = Session<Message, TokenUsage, ToolOutput>;

    fn orphan_result() -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "orphan".into(),
                content: "legacy".into(),
                is_error: false,
                output_ref: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn migrated_orphan_reaches_active_path_and_restored_sanitation() {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let mut legacy = LegacySession::new(MODEL, CWD);
        let id = legacy.id;
        legacy.replace_messages(vec![Message::user("keep".into()), orphan_result()]);
        legacy.save(&storage).unwrap();

        let migrated = load_stored_session(id, &storage).unwrap();
        let head = resolve_history_head(migrated.messages(), migrated.meta.history_head, false);
        let active = active_history_items(migrated.messages(), head).unwrap();
        let restored = History::restored(active).unwrap();

        assert_eq!(restored.as_slice().len(), 1);
        assert_eq!(restored.as_slice()[0].user_text(), Some("keep"));
    }

    #[test]
    fn migration_save_failure_still_returns_converted_session() {
        let mut compatible = CompatibleSession::new(MODEL, CWD);
        compatible.replace_messages(vec![PersistedHistoryEntry::Legacy(Message::user(
            "readable".into(),
        ))]);
        let temp = TempDir::new().unwrap();
        let blocked_root = temp.path().join("not-a-directory");
        std::fs::write(&blocked_root, "blocked").unwrap();
        let storage = StateDir::from_path(blocked_root);

        let migrated = convert_session(compatible, &storage).unwrap();

        assert_eq!(migrated.messages().len(), 1);
        assert_eq!(
            History::restored(migrated.messages().to_vec())
                .unwrap()
                .as_slice()[0]
                .user_text(),
            Some("readable")
        );
    }

    fn native_file_index() -> ToolOutput {
        ToolOutput::Index(IndexOutput::File {
            path: "/repo/src/lib.rs".into(),
            relative_path: "src/lib.rs".into(),
            language: "rust".into(),
            skeleton: "fns:\n  pub run() [2]".into(),
            lines: vec![IndexLine {
                output_line: 2,
                text: "  pub run() [2]".into(),
                semantic: IndexLineSemantic::Item,
                body: Some("  pub run()".into()),
                source_range: Some(IndexSourceRange {
                    start_line: 2,
                    end_line: 2,
                }),
            }],
            source_line_count: 2,
            parse_error: false,
            truncated: false,
            instructions: None,
            state: Some(serde_json::json!({"kind": "file", "language": "rust"})),
        })
    }

    fn native_directory_index() -> ToolOutput {
        ToolOutput::Index(IndexOutput::Directory {
            path: "/repo".into(),
            relative_path: ".".into(),
            entries: vec![IndexDirectoryEntry {
                name: "src".into(),
                kind: IndexDirectoryEntryKind::Directory,
            }],
            total_count: 2,
            truncated: true,
            listing: "src/\n[truncated]".into(),
            instructions: None,
            state: Some(serde_json::json!({
                "kind": "directory",
                "listing": "src/",
                "truncated": true
            })),
        })
    }

    #[test_case(native_file_index() ; "file")]
    #[test_case(native_directory_index() ; "directory")]
    fn native_index_output_survives_persisted_session_roundtrip(output: ToolOutput) {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let expected = serde_json::to_value(&output).unwrap();
        let mut session = StoredSession::new(MODEL, CWD);
        let id = session.id;
        session.insert_tool_output("index-call".into(), output);
        session.save(&storage).unwrap();

        let loaded = load_stored_session(id, &storage).unwrap();
        let actual = loaded.tool_outputs().get("index-call").unwrap();

        assert_eq!(serde_json::to_value(actual.as_ref()).unwrap(), expected);
        assert!(matches!(actual.as_ref(), ToolOutput::Index(_)));
    }

    #[test]
    fn native_shell_output_survives_persisted_session_roundtrip() {
        let output = ToolOutput::Shell(ShellOutput {
            model_text: "filtered\n\n[shell status: exit code 0]".into(),
            relative_workdir: ".".into(),
            timeout_ms: 120_000,
            duration_ms: 10,
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            output_limit_exceeded: false,
            final_sequence: 1,
            stdout_utf8_bytes: 3,
            stderr_utf8_bytes: 0,
            stdout: "raw".into(),
            stderr: String::new(),
            stdout_capture_truncated: false,
            stderr_capture_truncated: false,
            stdout_preview_truncated: false,
            stderr_preview_truncated: false,
            filter: Some(ShellFilterInfo {
                rule: "cargo".into(),
                unfiltered_utf8_bytes: 100,
                filtered_utf8_bytes: 20,
            }),
        });
        let expected = serde_json::to_value(&output).unwrap();
        let mut session = StoredSession::new(MODEL, CWD);
        let id = session.id;
        session.insert_tool_output("shell-call".into(), output);

        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        session.save(&storage).unwrap();
        let loaded = load_stored_session(id, &storage).unwrap();
        let actual = loaded.tool_outputs().get("shell-call").unwrap();

        assert_eq!(serde_json::to_value(actual.as_ref()).unwrap(), expected);
        assert_eq!(actual.as_text(), "filtered\n\n[shell status: exit code 0]");
    }
}
