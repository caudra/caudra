use caudra_providers::{HistoryItem, TokenUsage};
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::{Session, SessionError, mark_opened};

use crate::ToolOutput;

pub type StoredSession = Session<HistoryItem, TokenUsage, ToolOutput>;

pub fn load_stored_session(
    id: CaudraId,
    storage: &StateDir,
) -> Result<StoredSession, SessionError> {
    StoredSession::load(id, storage)
}

/// A load that is the user opening the session: resume, `--continue`, the
/// picker, an ACP `session/load`. Records the activity retention keys on.
pub fn open_stored_session(
    id: CaudraId,
    storage: &StateDir,
) -> Result<StoredSession, SessionError> {
    let session = load_stored_session(id, storage)?;
    mark_opened(id, storage)?;
    Ok(session)
}

pub fn latest_stored_session(
    cwd: &str,
    storage: &StateDir,
) -> Result<Option<StoredSession>, SessionError> {
    StoredSession::latest(cwd, storage)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::{
        IndexDirectoryEntry, IndexDirectoryEntryKind, IndexLine, IndexLineSemantic, IndexOutput,
        IndexSourceRange, ShellFilterInfo, ShellOutput,
    };
    use caudra_storage::sessions::SessionDatabase;

    const CWD: &str = "/repo";
    const MODEL: &str = "anthropic/test";
    const LOAD_IS_NOT_ACTIVITY: &str = "a recovery scan or retitle must not count as opening";
    const OPEN_IS_ACTIVITY: &str = "opening a session must record last_opened_at";

    #[test]
    fn only_opening_a_session_counts_as_activity() {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let mut session = StoredSession::new(MODEL, CWD);
        let id = session.id;
        session.save(&storage).unwrap();
        let last_opened_at = || {
            SessionDatabase::open(&storage)
                .unwrap()
                .session_facts(None)
                .unwrap()[0]
                .last_opened_at
        };

        load_stored_session(id, &storage).unwrap();
        assert_eq!(last_opened_at(), None, "{LOAD_IS_NOT_ACTIVITY}");

        open_stored_session(id, &storage).unwrap();
        assert!(last_opened_at().is_some(), "{OPEN_IS_ACTIVITY}");
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
            stdout_redraws_collapsed: 190,
            stderr_redraws_collapsed: 0,
            filter: Some(ShellFilterInfo {
                stages: vec!["make".into(), "progress".into()],
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
