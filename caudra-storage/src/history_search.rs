//! Full-text search over stored session history, shared by the agent and any
//! frontend. A contentless FTS5 index holds one row per searchable history
//! item, `history_search_docs` says where each row came from, and
//! `history_search_cursors` records how far each stream is indexed. The index
//! keeps no copy of the text: snippets and reads decompress the original
//! payloads. Indexing is lazy, through [`SessionDatabase::catch_up_history`],
//! so the migration that adds these tables costs nothing.

mod extract;
mod index;
mod query;
mod snippet;
mod timeline;

pub(crate) use extract::strip_reminders;
pub use query::highlight_history;
pub use timeline::TimelineItem;

use std::fmt;
use std::ops::Range;
use std::path::{MAIN_SEPARATOR, PathBuf};
use std::str::FromStr;

use rusqlite::InterruptHandle;
use rusqlite::types::Value as SqlValue;
use thiserror::Error;

use crate::id::CaudraId;
use crate::sessions::{AFTER_SEPARATOR, SessionDatabase, SessionError};
use crate::workspace_binding::StoredWorkspaceBinding;

/// Bumped whenever extraction or tokenization changes, which rebuilds the
/// whole index on the next catch-up.
pub const INDEX_VERSION: u32 = 1;
pub const DEFAULT_LIMIT: usize = 20;
/// Shared by the index and the scratch table snippets are matched in.
const TOKENIZER: &str = "porter unicode61 remove_diacritics 2";
const MAIN_STREAM: &str = "";
const REF_SEPARATOR: char = '/';

/// The index as a fresh database creates it and as the schema 29 step adds
/// it. FTS5 rows carry no foreign key, so the trigger removes a row's terms
/// when its document goes, including through a session's cascade.
pub(crate) const TABLES: &str = r#"
CREATE TABLE history_search_docs (
    doc        INTEGER PRIMARY KEY,
    session_id BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    stream     TEXT NOT NULL,
    ordinal    INTEGER NOT NULL,
    item_id    BLOB NOT NULL,
    kind       INTEGER NOT NULL,
    tool       TEXT,
    at_ms      INTEGER NOT NULL,
    UNIQUE(session_id, stream, item_id)
) STRICT;

CREATE TABLE history_search_cursors (
    session_id BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    stream     TEXT NOT NULL,
    indexed    INTEGER NOT NULL,
    last_item  BLOB,
    last_bytes INTEGER,
    PRIMARY KEY(session_id, stream)
) STRICT, WITHOUT ROWID;

CREATE VIRTUAL TABLE history_search USING fts5(
    body, content='', contentless_delete=1, tokenize='porter unicode61 remove_diacritics 2'
);

CREATE TRIGGER history_search_docs_gone AFTER DELETE ON history_search_docs
BEGIN
    DELETE FROM history_search WHERE rowid = OLD.doc;
END;
"#;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HistoryParseError {
    #[error(
        "unknown history kind `{0}`; expected user, assistant, summary, reasoning, tool_call or tool_result"
    )]
    UnknownKind(String),
    #[error("unknown history sort `{0}`; expected relevance or newest")]
    UnknownSort(String),
    #[error(
        "invalid history reference `{0}`; expected `<session>/<item>` or `<session>/<agent>/<item>`"
    )]
    InvalidRef(String),
}

/// What an indexed item is. The stored code and the name are both stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HistoryKind {
    User,
    Assistant,
    /// A compaction summary.
    Summary,
    Reasoning,
    ToolCall,
    ToolResult,
}

impl HistoryKind {
    pub const ALL: [Self; 6] = [
        Self::User,
        Self::Assistant,
        Self::Summary,
        Self::Reasoning,
        Self::ToolCall,
        Self::ToolResult,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Summary => "summary",
            Self::Reasoning => "reasoning",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
        }
    }

    fn code(self) -> i64 {
        match self {
            Self::User => 0,
            Self::Assistant => 1,
            Self::Summary => 2,
            Self::Reasoning => 3,
            Self::ToolCall => 4,
            Self::ToolResult => 5,
        }
    }

    fn from_code(code: i64) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.code() == code)
    }
}

impl fmt::Display for HistoryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for HistoryKind {
    type Err = HistoryParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == value)
            .ok_or_else(|| HistoryParseError::UnknownKind(value.to_owned()))
    }
}

/// The conversation an item belongs to within its session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HistoryStream {
    Main,
    Subagent(String),
}

impl HistoryStream {
    fn key(&self) -> &str {
        match self {
            Self::Main => MAIN_STREAM,
            Self::Subagent(id) => id,
        }
    }

    fn from_key(key: String) -> Self {
        if key == MAIN_STREAM {
            Self::Main
        } else {
            Self::Subagent(key)
        }
    }
}

/// One stored item, as an opaque string: `<session>/<item>` for the main
/// conversation and `<session>/<agent>/<item>` for a subagent's.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HistoryRef {
    pub session: CaudraId,
    pub stream: HistoryStream,
    pub item: CaudraId,
}

impl fmt::Display for HistoryRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.stream {
            HistoryStream::Main => write!(f, "{}{REF_SEPARATOR}{}", self.session, self.item),
            HistoryStream::Subagent(agent) => write!(
                f,
                "{}{REF_SEPARATOR}{agent}{REF_SEPARATOR}{}",
                self.session, self.item
            ),
        }
    }
}

impl FromStr for HistoryRef {
    type Err = HistoryParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || HistoryParseError::InvalidRef(value.to_owned());
        let (session, rest) = value.split_once(REF_SEPARATOR).ok_or_else(invalid)?;
        let (stream, item) = match rest.rsplit_once(REF_SEPARATOR) {
            None => (HistoryStream::Main, rest),
            Some(("", _)) => return Err(invalid()),
            Some((agent, item)) => (HistoryStream::Subagent(agent.to_owned()), item),
        };
        Ok(Self {
            session: session.parse().map_err(|_| invalid())?,
            stream,
            item: item.parse().map_err(|_| invalid())?,
        })
    }
}

/// The remote workspace a session ran in, matched without its cursor so every
/// directory of the project counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceIdentity {
    pub source: String,
    pub authority: String,
    pub principal: String,
    pub project: String,
}

impl From<&StoredWorkspaceBinding> for WorkspaceIdentity {
    fn from(binding: &StoredWorkspaceBinding) -> Self {
        Self {
            source: binding.trust_anchor().as_str().to_owned(),
            authority: binding.authority_storage_key(),
            principal: binding.principal_id().to_owned(),
            project: binding.project_key().as_str().to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectScope {
    /// Local sessions working in any of these directories or below them, such
    /// as a main checkout and its linked worktrees.
    Local(Vec<PathBuf>),
    Workspace(WorkspaceIdentity),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryScope {
    /// One session with its subagents and everything compaction or revert
    /// left behind.
    Session(CaudraId),
    Project(ProjectScope),
    All,
}

impl HistoryScope {
    fn spans_sessions(&self) -> bool {
        !matches!(self, Self::Session(_))
    }

    /// A condition on the `sessions` table aliased `s`, with its parameters.
    fn filter(&self) -> (String, Vec<SqlValue>) {
        match self {
            Self::All => ("1".to_owned(), Vec::new()),
            Self::Session(id) => ("s.id = ?".to_owned(), vec![id_value(*id)]),
            Self::Project(ProjectScope::Workspace(identity)) => (
                "(s.workspace_source = ? AND s.workspace_authority = ? \
                 AND s.workspace_principal = ? AND s.workspace_project = ?)"
                    .to_owned(),
                [
                    &identity.source,
                    &identity.authority,
                    &identity.principal,
                    &identity.project,
                ]
                .into_iter()
                .map(|value| SqlValue::Text(value.clone()))
                .collect(),
            ),
            Self::Project(ProjectScope::Local(roots)) if roots.is_empty() => {
                ("0".to_owned(), Vec::new())
            }
            Self::Project(ProjectScope::Local(roots)) => {
                let local = StoredWorkspaceBinding::local_from_cwd("");
                let mut values = vec![SqlValue::Text(local.trust_anchor().as_str().to_owned())];
                let mut roots_sql = Vec::with_capacity(roots.len());
                for root in roots {
                    let root = root.to_string_lossy();
                    let root = root.trim_end_matches(MAIN_SEPARATOR);
                    roots_sql.push("s.cwd = ? OR (s.cwd >= ? AND s.cwd < ?)");
                    values.extend([
                        SqlValue::Text(root.to_owned()),
                        SqlValue::Text(format!("{root}{MAIN_SEPARATOR}")),
                        SqlValue::Text(format!("{root}{AFTER_SEPARATOR}")),
                    ]);
                }
                (
                    format!(
                        "(s.workspace_source IN ('', ?) AND ({}))",
                        roots_sql.join(" OR ")
                    ),
                    values,
                )
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HistorySort {
    #[default]
    Relevance,
    Newest,
}

impl HistorySort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Relevance => "relevance",
            Self::Newest => "newest",
        }
    }
}

impl FromStr for HistorySort {
    type Err = HistoryParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        [Self::Relevance, Self::Newest]
            .into_iter()
            .find(|sort| sort.as_str() == value)
            .ok_or_else(|| HistoryParseError::UnknownSort(value.to_owned()))
    }
}

/// A keyword search. Free text becomes quoted terms, with `"double-quoted
/// phrases"` kept whole, so no query reaches FTS5 as raw syntax. Blank text
/// lists the newest items that pass the filters.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryQuery {
    pub text: String,
    pub scope: HistoryScope,
    pub kinds: Option<Vec<HistoryKind>>,
    pub tools: Option<Vec<String>>,
    /// Unix milliseconds, inclusive.
    pub since_ms: Option<u64>,
    /// Unix milliseconds, exclusive.
    pub until_ms: Option<u64>,
    pub sort: HistorySort,
    pub limit: usize,
    pub offset: usize,
    /// At most this many hits from one session, when the scope spans several.
    pub per_session_cap: Option<usize>,
}

impl HistoryQuery {
    pub fn new(text: impl Into<String>, scope: HistoryScope) -> Self {
        Self {
            text: text.into(),
            scope,
            kinds: None,
            tools: None,
            since_ms: None,
            until_ms: None,
            sort: HistorySort::default(),
            limit: DEFAULT_LIMIT,
            offset: 0,
            per_session_cap: None,
        }
    }
}

/// Text with the byte ranges of the query's terms in it, each on a char
/// boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snippet {
    pub text: String,
    pub highlights: Vec<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryHit {
    pub reference: HistoryRef,
    pub session_title: String,
    /// The local working directory, or the remote workspace's project.
    pub project: String,
    pub kind: HistoryKind,
    pub tool: Option<String>,
    /// When the item was made, Unix milliseconds.
    pub at_ms: u64,
    /// Higher is better. Zero when listing without terms.
    pub score: f64,
    pub snippet: Snippet,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistoryPage {
    pub hits: Vec<HistoryHit>,
    /// More hits follow this page.
    pub more: bool,
    /// Distinct matching items considered, capped by the scan.
    pub matched: usize,
}

/// A neighbour of a read item, in the same stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeighbourPreview {
    pub reference: HistoryRef,
    pub kind: HistoryKind,
    pub tool: Option<String>,
    pub at_ms: u64,
    pub preview: String,
    pub truncated: bool,
}

/// One item's text, paged by byte offset on char boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub reference: HistoryRef,
    pub session_title: String,
    pub project: String,
    pub kind: HistoryKind,
    pub tool: Option<String>,
    pub at_ms: u64,
    pub text: String,
    pub offset: usize,
    pub total_bytes: usize,
    /// Where the next page starts, when there is one.
    pub next_offset: Option<usize>,
    pub before: Vec<NeighbourPreview>,
    pub after: Vec<NeighbourPreview>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHit {
    pub id: CaudraId,
    pub title: String,
    pub project: String,
    /// Unix seconds.
    pub updated_at: u64,
    pub history_items: u64,
    pub subagent_items: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CatchUp {
    pub indexed_items: usize,
    /// Sessions in scope with items still unindexed when the deadline came.
    pub pending_sessions: usize,
}

/// Stops the query running on the connection it came from, which then fails
/// with an error [`SessionError::is_interrupted`] recognises.
pub struct HistoryInterrupt(InterruptHandle);

impl HistoryInterrupt {
    pub fn interrupt(&self) {
        self.0.interrupt();
    }
}

impl SessionDatabase {
    pub fn history_interrupt(&self) -> HistoryInterrupt {
        HistoryInterrupt(self.connection().get_interrupt_handle())
    }
}

impl SessionError {
    pub fn is_interrupted(&self) -> bool {
        matches!(self, Self::Sqlite(error)
            if error.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted))
    }
}

/// The rows of `stream`, with the session bound as `?1` and the stream as
/// `?2` either way.
fn source(stream: &str) -> &'static str {
    if stream == MAIN_STREAM {
        "main_history_items WHERE session_id = ?1 AND ?2 = ''"
    } else {
        "subagent_history_items WHERE session_id = ?1 AND subagent_id = ?2"
    }
}

fn id_value(id: CaudraId) -> SqlValue {
    SqlValue::Blob(id.as_bytes().to_vec())
}

/// The label a hit names its project by: the directory of a local session,
/// the project of a remote one.
fn project_label(source: &str, cwd: String, project: String) -> String {
    let local = StoredWorkspaceBinding::local_from_cwd("");
    if source.is_empty() || source == local.trust_anchor().as_str() || project.is_empty() {
        cwd
    } else {
        project
    }
}

#[cfg(test)]
mod fixtures {
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::{CatchUp, HistoryScope};
    use crate::StateDir;
    use crate::id::CaudraId;
    use crate::sessions::{Session, SessionDatabase};

    pub(super) type TestSession = Session<Value, Value, Value>;

    pub(super) const PROJECT: &str = "/work/project";
    pub(super) const OTHER_PROJECT: &str = "/work/other";
    pub(super) const MODEL: &str = "test/model";
    pub(super) const BASE_MS: u64 = 1_767_225_600_000;
    pub(super) const SHELL: &str = "shell";
    const CATCH_UP_BUDGET: Duration = Duration::from_secs(60);
    const TIMESTAMP_BYTES: usize = 6;
    const MILLIS_BYTES: usize = 8;

    pub(super) fn database() -> (TempDir, SessionDatabase) {
        let temp = TempDir::new().unwrap();
        let database =
            SessionDatabase::open_state(&StateDir::from_path(temp.path().to_path_buf())).unwrap();
        (temp, database)
    }

    /// A fresh UUIDv7 stamped `at_ms`.
    pub(super) fn id_at(at_ms: u64) -> CaudraId {
        let mut bytes = *CaudraId::generate().as_bytes();
        bytes[..TIMESTAMP_BYTES]
            .copy_from_slice(&at_ms.to_be_bytes()[MILLIS_BYTES - TIMESTAMP_BYTES..]);
        CaudraId::from_bytes(bytes)
    }

    fn item(at_ms: u64, mut fields: Value) -> Value {
        fields["id"] = json!(id_at(at_ms).to_string());
        fields
    }

    pub(super) fn user_at(at_ms: u64, text: &str) -> Value {
        item(
            at_ms,
            json!({"type": "user", "text": text, "origin": "turn"}),
        )
    }

    pub(super) fn user(text: &str) -> Value {
        user_at(BASE_MS, text)
    }

    pub(super) fn assistant(text: &str) -> Value {
        item(
            BASE_MS,
            json!({"type": "assistant_text", "text": text, "state": "complete"}),
        )
    }

    pub(super) fn tool_call(call: &str, name: &str, command: &str) -> Value {
        item(
            BASE_MS,
            json!({"type": "tool_call", "call_id": call, "name": name, "input": {"command": command}}),
        )
    }

    pub(super) fn tool_result(call: &str, content: &str) -> Value {
        item(
            BASE_MS,
            json!({"type": "tool_result", "call_id": call, "content": content, "is_error": false}),
        )
    }

    pub(super) fn session_with(cwd: &str, title: &str, items: Vec<Value>) -> TestSession {
        let mut session = TestSession::new(MODEL, cwd);
        session.set_title(title.to_owned());
        session.replace_messages(items);
        session
    }

    pub(super) fn catch_up(database: &SessionDatabase, scope: &HistoryScope) -> CatchUp {
        database
            .catch_up_history(scope, Instant::now() + CATCH_UP_BUDGET)
            .unwrap()
    }

    pub(super) fn count(database: &SessionDatabase, sql: &str) -> i64 {
        database
            .connection()
            .query_row(sql, [], |row| row.get(0))
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const AGENT: &str = "toolu_01/nested";

    #[test_case(HistoryStream::Main; "main")]
    #[test_case(HistoryStream::Subagent(AGENT.to_owned()); "subagent")]
    fn reference_round_trips_as_a_string(stream: HistoryStream) {
        let reference = HistoryRef {
            session: CaudraId::generate(),
            stream,
            item: CaudraId::generate(),
        };
        assert_eq!(reference.to_string().parse::<HistoryRef>(), Ok(reference));
    }

    #[test_case(""; "empty")]
    #[test_case("session"; "no_item")]
    #[test_case("abc/def"; "not_ids")]
    fn malformed_reference_is_refused(text: &str) {
        assert_eq!(
            text.parse::<HistoryRef>(),
            Err(HistoryParseError::InvalidRef(text.to_owned()))
        );
    }

    #[test]
    fn reference_with_an_empty_agent_is_refused() {
        let text = format!("{}//{}", CaudraId::generate(), CaudraId::generate());
        assert_eq!(
            text.parse::<HistoryRef>(),
            Err(HistoryParseError::InvalidRef(text))
        );
    }

    #[test]
    fn kind_names_and_codes_round_trip() {
        for kind in HistoryKind::ALL {
            assert_eq!(kind.as_str().parse(), Ok(kind));
            assert_eq!(HistoryKind::from_code(kind.code()), Some(kind));
        }
        assert!(matches!(
            "tool".parse::<HistoryKind>(),
            Err(HistoryParseError::UnknownKind(_))
        ));
    }

    #[test]
    fn index_and_snippets_share_one_tokenizer() {
        assert!(TABLES.contains(TOKENIZER));
    }
}
