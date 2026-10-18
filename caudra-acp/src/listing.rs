use std::path::Path;

use agent_client_protocol_schema::v1::{
    Error as AcpError, ListSessionsRequest, ListSessionsResponse, Meta, SessionId, SessionInfo,
};
use caudra_storage::StateDir;
use caudra_storage::id::{CaudraId, SessionRef};
use caudra_storage::sessions::{SessionDatabase, SessionListing, SessionPreview};
use jiff::Timestamp;
use serde_json::json;
use tracing::warn;

use crate::errors::{internal, invalid_params};
use crate::translate::META_NAMESPACE;

const PAGE_SIZE: usize = 100;
const RELATIVE_CWD: &str = "cwd must be an absolute path";
const INVALID_CURSOR: &str = "cursor was not issued by session/list";
const UNTITLED: &str = "Untitled session";
const CURSOR_SEPARATOR: char = '.';

/// The local sessions a client may load, newest first, a page at a time.
/// Sessions bound to a remote workspace have no host cwd to report, so only
/// local ones with an absolute cwd are listed, as the TUI's picker does.
pub(crate) fn list_sessions(
    storage: &StateDir,
    request: &ListSessionsRequest,
) -> Result<ListSessionsResponse, AcpError> {
    let cwd = match request.cwd.as_deref() {
        Some(cwd) if !cwd.is_absolute() => return Err(invalid_params(RELATIVE_CWD)),
        cwd => cwd.map(Path::to_string_lossy),
    };
    let after = request.cursor.as_deref().map(decode_cursor).transpose()?;
    let database = SessionDatabase::open(storage).map_err(|error| internal(&error))?;
    let mut page = database
        .local_session_page(cwd.as_deref(), after, PAGE_SIZE + 1)
        .map_err(|error| internal(&error))?;
    let next_cursor = if page.len() > PAGE_SIZE {
        page.truncate(PAGE_SIZE);
        page.last().map(encode_cursor)
    } else {
        None
    };
    let sessions = page
        .into_iter()
        .filter(|row| Path::new(&row.cwd).is_absolute())
        .map(|row| session_info(&database, row))
        .collect();
    Ok(ListSessionsResponse::new(sessions).next_cursor(next_cursor))
}

/// A session as listed. A history that cannot be read previews nothing
/// rather than failing the page.
fn session_info(database: &SessionDatabase, row: SessionListing) -> SessionInfo {
    let preview = database.session_preview(row.id).unwrap_or_else(|error| {
        warn!(session = %row.id, %error, "session/list could not preview a session");
        SessionPreview::default()
    });
    let title = if row.title.trim().is_empty() {
        UNTITLED.to_owned()
    } else {
        row.title
    };
    let updated_at = rfc3339(row.updated_at);
    let meta = Meta::from_iter([(
        META_NAMESPACE.to_owned(),
        json!({
            "model": row.model,
            "firstPrompt": preview.first_prompt,
            "lastPrompt": preview.last_prompt,
        }),
    )]);
    SessionInfo::new(
        SessionId::from(SessionRef::from(row.id).to_string()),
        row.cwd,
    )
    .title(title)
    .updated_at(updated_at)
    .meta(meta)
}

/// A stored Unix-seconds timestamp as ACP spells one.
pub(crate) fn rfc3339(seconds: u64) -> Option<String> {
    i64::try_from(seconds)
        .ok()
        .and_then(|seconds| Timestamp::from_second(seconds).ok())
        .map(|timestamp| timestamp.to_string())
}

fn encode_cursor(row: &SessionListing) -> String {
    format!("{}{CURSOR_SEPARATOR}{}", row.updated_at, row.id)
}

fn decode_cursor(cursor: &str) -> Result<(u64, CaudraId), AcpError> {
    cursor
        .split_once(CURSOR_SEPARATOR)
        .and_then(|(updated_at, id)| Some((updated_at.parse().ok()?, id.parse().ok()?)))
        .ok_or_else(|| invalid_params(INVALID_CURSOR))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use caudra_agent::StoredSession;
    use caudra_providers::{Message, expand_message};
    use serde_json::Value;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::test_support::OFFLINE_SPEC;

    const PROJECT: &str = "/project";
    const ELSEWHERE: &str = "/elsewhere";
    const TITLE: &str = "rename foo to bar";
    const UPDATED_AT: u64 = 1_700_000_000;
    const UPDATED_AT_RFC3339: &str = "2023-11-14T22:13:20Z";
    const FIRST_PROMPT: &str = "find the flaky test";
    const LAST_PROMPT: &str = "now fix it";

    fn save(storage: &StateDir, cwd: &str, updated_at: u64, title: &str) -> SessionId {
        save_with(storage, cwd, updated_at, title, &[])
    }

    fn save_with(
        storage: &StateDir,
        cwd: &str,
        updated_at: u64,
        title: &str,
        prompts: &[&str],
    ) -> SessionId {
        let mut session = StoredSession::new(OFFLINE_SPEC, cwd);
        session.set_title(title.to_owned());
        let mut parent = None;
        for prompt in prompts {
            for item in expand_message(&Message::user((*prompt).to_owned()), parent) {
                parent = Some(item.id);
                session.push_message(item);
            }
        }
        session.updated_at = updated_at;
        SessionDatabase::open(storage)
            .unwrap()
            .save(&session, None)
            .unwrap();
        SessionId::from(SessionRef::from(session.id).to_string())
    }

    fn listed(storage: &StateDir, request: ListSessionsRequest) -> ListSessionsResponse {
        list_sessions(storage, &request).unwrap()
    }

    #[test]
    fn sessions_list_newest_first_in_the_requested_cwd() {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let older = save(&storage, PROJECT, UPDATED_AT, TITLE);
        let newer = save(&storage, PROJECT, UPDATED_AT + 1, TITLE);
        let elsewhere = save(&storage, ELSEWHERE, UPDATED_AT + 2, TITLE);

        let filtered = listed(
            &storage,
            ListSessionsRequest::new().cwd(PathBuf::from(PROJECT)),
        );
        let ids: Vec<_> = filtered
            .sessions
            .iter()
            .map(|info| &info.session_id)
            .collect();
        assert_eq!(ids, [&newer, &older]);
        assert_eq!(filtered.next_cursor, None);

        let everything = listed(&storage, ListSessionsRequest::new());
        assert_eq!(everything.sessions[0].session_id, elsewhere);
        assert_eq!(everything.sessions.len(), 3);
    }

    #[test]
    fn a_listed_session_reports_its_title_time_and_model() {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        save_with(
            &storage,
            PROJECT,
            UPDATED_AT,
            " ",
            &[FIRST_PROMPT, LAST_PROMPT],
        );

        let response = listed(&storage, ListSessionsRequest::new());
        let wire = serde_json::to_value(&response.sessions[0]).unwrap();

        assert_eq!(wire["cwd"], PROJECT);
        assert_eq!(wire["title"], UNTITLED);
        assert_eq!(wire["updatedAt"], UPDATED_AT_RFC3339);
        let caudra = &wire["_meta"][META_NAMESPACE];
        assert_eq!(caudra["model"], OFFLINE_SPEC);
        assert_eq!(caudra["firstPrompt"], FIRST_PROMPT);
        assert_eq!(caudra["lastPrompt"], LAST_PROMPT);
    }

    #[test]
    fn a_session_without_prompts_previews_null() {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        save(&storage, PROJECT, UPDATED_AT, TITLE);

        let response = listed(&storage, ListSessionsRequest::new());
        let wire = serde_json::to_value(&response.sessions[0]).unwrap();

        assert_eq!(wire["_meta"][META_NAMESPACE]["firstPrompt"], Value::Null);
        assert_eq!(wire["_meta"][META_NAMESPACE]["lastPrompt"], Value::Null);
    }

    #[test]
    fn pages_continue_from_the_cursor_without_repeats() {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let mut saved: Vec<_> = (0..=PAGE_SIZE as u64)
            .map(|offset| save(&storage, PROJECT, UPDATED_AT + offset, TITLE))
            .collect();
        saved.reverse();

        let first = listed(&storage, ListSessionsRequest::new());
        let second = listed(
            &storage,
            ListSessionsRequest::new().cursor(first.next_cursor.clone()),
        );

        assert_eq!(first.sessions.len(), PAGE_SIZE);
        assert!(first.next_cursor.is_some());
        assert_eq!(second.next_cursor, None);
        let paged: Vec<_> = first
            .sessions
            .iter()
            .chain(&second.sessions)
            .map(|info| info.session_id.clone())
            .collect();
        assert_eq!(paged, saved);
    }

    #[test_case(ListSessionsRequest::new().cwd(PathBuf::from("relative/project")), RELATIVE_CWD ; "relative_cwd")]
    #[test_case(ListSessionsRequest::new().cursor("not-a-cursor".to_owned()), INVALID_CURSOR ; "foreign_cursor")]
    fn a_malformed_request_is_invalid_params(request: ListSessionsRequest, detail: &str) {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());

        let error = list_sessions(&storage, &request).unwrap_err();

        assert_eq!(error.code, AcpError::invalid_params().code);
        assert_eq!(error.data, Some(Value::from(detail)));
    }
}
