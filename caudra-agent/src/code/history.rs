//! `history` as a host module: searching and reading the conversations stored on this machine.
//!
//! Every call reads one project's history, or every project's, and asks as the `code` tool to read
//! that project's `session_history`. The built-in rule lets the session's own project through
//! unasked, which is what the call's `current_project` mark claims; only this module sets it, and
//! only after comparing the target with the project the session's permissions are bound to at the
//! time of the call. Which project a call reaches takes filesystem and database reads, so its plan
//! resolves off the executor.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use caudra_config::ToolKey;
use caudra_storage::StateDir;
use caudra_storage::history_search::{
    HistoryEntry, HistoryHit, HistoryKind, HistoryQuery, HistoryRef, HistoryScope, HistorySort,
    HistoryStream, NeighbourPreview, SessionHit, Snippet, WorkspaceIdentity,
};
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::SessionError;
use serde_json::{Map, Value, json};

use super::args::{bind, count, required_text, text};
use super::gateway::Permission;
use super::module::{
    CallFailure, CallRun, HostCallPlan, HostModule, MemberDoc, ModuleDoc, Prepared,
};
use super::names::HISTORY_MODULE;
use super::output::CodeCall;
use super::runner::{ScriptCall, ScriptError, ScriptErrorKind};
use crate::decisions::{NoteSink, counted};
use crate::history::{
    self, HistoryProject, HistoryResults, HistorySearch, HistoryStore, Ranking, iso_time,
};
use crate::mcp::output::{HistoryHelper, McpBlock, McpHelper, McpOutput};
use crate::memory::relevance::Judge;
use crate::permissions::{
    ALL_PROJECTS_HISTORY, CURRENT_PROJECT_ATTRIBUTE, CURRENT_PROJECT_VALUE, PermissionExecutorKind,
    PermissionResource, PermissionResourceAccess, PermissionResourceKind, PermissionRisk,
    PermissionSubject, SESSION_HISTORY_RESOURCE,
};
use crate::tools::registry::{BoxFuture, PermissionIntent, PermissionScopes};
use crate::tools::{CODE_TOOL_NAME, ToolContext, ToolFailure, expand_tilde, native};

const SEARCH_METHOD: &str = "search";
const READ_METHOD: &str = "read";
const SESSIONS_METHOD: &str = "sessions";
const SEARCH_HELPER: &str = "history.search";
const READ_HELPER: &str = "history.read";
const SESSIONS_HELPER: &str = "history.sessions";
const QUERY_PARAM: &str = "query";
const SCOPE_PARAM: &str = "scope";
const KINDS_PARAM: &str = "kinds";
const TOOLS_PARAM: &str = "tools";
const SINCE_PARAM: &str = "since";
const UNTIL_PARAM: &str = "until";
const SORT_PARAM: &str = "sort";
const RERANK_PARAM: &str = "rerank";
const LIMIT_PARAM: &str = "limit";
const OFFSET_PARAM: &str = "offset";
const REF_PARAM: &str = "ref";
const AROUND_PARAM: &str = "around";
const TITLE_PARAM: &str = "title";
const SEARCH_PARAMS: &[&str] = &[
    QUERY_PARAM,
    SCOPE_PARAM,
    KINDS_PARAM,
    TOOLS_PARAM,
    SINCE_PARAM,
    UNTIL_PARAM,
    SORT_PARAM,
    RERANK_PARAM,
    LIMIT_PARAM,
    OFFSET_PARAM,
];
const READ_PARAMS: &[&str] = &[REF_PARAM, OFFSET_PARAM, LIMIT_PARAM, AROUND_PARAM];
const SESSIONS_PARAMS: &[&str] = &[SCOPE_PARAM, TITLE_PARAM, LIMIT_PARAM, OFFSET_PARAM];
const SESSION_SCOPE: &str = "session";
const PROJECT_SCOPE: &str = "project";
const ALL_SCOPE: &str = "all";
const DEFAULT_READ_BYTES: usize = 8_000;
const MAX_READ_BYTES: usize = 64 * 1024;
const MAX_AROUND: usize = 20;
const DEFAULT_SESSIONS: usize = 20;
const MAX_SESSIONS: usize = 100;
const SECONDS_TO_MILLIS: u64 = 1_000;
const PERMISSION_SCOPE_PREFIX: &str = "history:";
const HIGHLIGHT_OPEN: char = '«';
const HIGHLIGHT_CLOSE: char = '»';
const RERANKED: &str = "reranked";
const KEYWORD: &str = "keyword";
const SESSION_ONE: &str = "session";
const SESSION_MANY: &str = "sessions";
const SEARCHES: &str = "Searches";
const LISTS_SESSIONS: &str = "Lists the sessions in";
const READS: &str = "Reads from";
const FROM: &str = "in";
const HISTORY_OF: &str = "the history of ";
const ALL_PROJECTS_WORDS: &str = "all projects";
/// Raised by every call of a session whose conversation is not stored.
pub const UNSAVED: &str = "history is unavailable: this session is not saved";
pub static DOC: ModuleDoc = ModuleDoc {
    namespace: HISTORY_MODULE,
    summary: "`history`: full-text search over the conversations stored on this machine, compacted parts and subagents included; `history.search`, `history.read`, `history.sessions`.",
    page: "history: the conversations stored on this machine, compacted parts and subagents included, searched by word with BM25 ranking.\n\
Scopes: `\"session\"` (this conversation), `\"project\"` (this checkout and its linked worktrees, or this remote workspace), `\"all\"`, or a project path. The current project needs no approval; another project or `\"all\"` asks first.\n\
Example: `hits = (await history.search(\"flaky snapshot test\", kinds=[\"user\", \"summary\"]))[\"hits\"]`, then `await history.read(hits[0][\"ref\"], around=2)`.",
    members: &[
        MemberDoc {
            name: SEARCH_METHOD,
            signature: r#"search(query: str, scope: str = "project", kinds: list[Literal["user", "assistant", "summary", "reasoning", "tool_call", "tool_result"]] | None = None, tools: list[str] | None = None, since: str | None = None, until: str | None = None, sort: Literal["relevance", "newest"] = "relevance", rerank: bool | None = None, limit: int | None = None, offset: int = 0) -> dict[str, Any]"#,
            awaited: true,
            read_first: true,
            page: "Returns `{\"hits\": [{\"ref\", \"session\", \"title\", \"project\", \"when\", \"kind\", \"tool\", \"agent\", \"snippet\", \"score\"}], \"ranking\", \"judged_unrelated\", \"more\", \"unindexed_sessions\", \"note\"}`. Matched words in `snippet` are marked «like this».\n\
- `kinds` keeps only those items; `tools` keeps only calls of, and results from, the named tools.\n\
- `since` and `until` take ISO dates or times.\n\
- `sort=\"relevance\"` ranks by keyword score, reranked when the session's decision engine is set to rerank history (`rerank=False` opts out); `sort=\"newest\"` takes the newest matches and keeps time order.\n\
- `limit` defaults to 10 reranked or 20 by keyword, at most 50; page on with `offset` while `more` is true.\n\
- `note` says why ranking fell back to keyword order, and `unindexed_sessions` counts sessions not indexed yet.",
        },
        MemberDoc {
            name: READ_METHOD,
            signature: "read(ref: str, offset: int = 0, limit: int = 8000, around: int = 0) -> dict[str, Any]",
            awaited: true,
            read_first: true,
            page: "Returns one item's full text from byte `offset`, at most `limit` bytes (64 KiB), with `around` neighbours each side (at most 20) as previews. `ref` comes from a `history.search` hit.",
        },
        MemberDoc {
            name: SESSIONS_METHOD,
            signature: "sessions(scope: str = \"project\", title: str | None = None, limit: int = 20, offset: int = 0) -> list[dict[str, Any]]",
            awaited: true,
            read_first: false,
            page: "Lists sessions newest first, at most 100 per call, optionally only those whose title contains `title`.",
        },
    ],
};

/// The stored history one run reaches. Without a store the session is not saved, and every call
/// raises.
pub struct HistoryModule {
    store: Option<HistoryStore>,
}

impl HistoryModule {
    pub fn new(store: Option<HistoryStore>) -> Self {
        Self { store }
    }

    /// The process's history, unless the session is ephemeral or has no stored conversation.
    pub fn for_session(ctx: &ToolContext) -> Self {
        let store = ctx
            .session_id
            .as_ref()
            .and_then(|_| StateDir::resolve().ok())
            .filter(|state| !state.is_ephemeral())
            .map(HistoryStore::new);
        Self::new(store)
    }
}

impl HostModule for HistoryModule {
    fn doc(&self) -> &'static ModuleDoc {
        &DOC
    }

    fn unavailable(&self) -> Option<String> {
        self.store.is_none().then(|| UNSAVED.to_owned())
    }

    fn prepare(&self, ctx: &ToolContext, call: ScriptCall) -> Prepared {
        let Some(store) = self.store.clone() else {
            return Prepared::Plan(Err(runtime(UNSAVED.to_owned())));
        };
        let caller = Caller::of(ctx);
        let planned = match call.name.as_str() {
            SEARCH_METHOD => SearchCall::parse(&call).map(|search| search.plan(store, caller)),
            READ_METHOD => ReadCall::parse(&call).map(|read| read.plan(store, caller)),
            SESSIONS_METHOD => {
                SessionsCall::parse(&call).map(|sessions| sessions.plan(store, caller))
            }
            name => Err(ScriptError::new(
                ScriptErrorKind::Attribute,
                format!("'{HISTORY_MODULE}' has no attribute '{name}'"),
            )),
        };
        match planned {
            Ok(plan) => Prepared::Later(plan),
            Err(error) => Prepared::Plan(Err(error)),
        }
    }
}

/// What a call needs of the run that made it, taken when it is made.
struct Caller {
    session: Option<CaudraId>,
    here: Here,
    judge: Option<Judge>,
    notes: Option<NoteSink>,
}

impl Caller {
    fn of(ctx: &ToolContext) -> Self {
        Self {
            session: ctx.session_id.as_ref().map(|session| session.id()),
            here: Here::of(ctx),
            judge: judge(ctx),
            notes: ctx.note_sink(),
        }
    }

    /// Which history `requested` names, and whether it is the session's own project.
    fn reach(&self, requested: &Requested) -> Result<Reach, ScriptError> {
        let own = || {
            let project = self.here.project();
            Reach {
                label: project.label,
                scope: HistoryScope::Project(project.scope),
                current: true,
            }
        };
        Ok(match requested {
            Requested::Session => {
                let session = self.session.ok_or_else(|| runtime(UNSAVED.to_owned()))?;
                Reach {
                    scope: HistoryScope::Session(session),
                    ..own()
                }
            }
            Requested::Project => own(),
            Requested::All => Reach {
                scope: HistoryScope::All,
                label: ALL_PROJECTS_HISTORY.to_owned(),
                current: false,
            },
            Requested::Path(path) => {
                let project = HistoryProject::local(&self.here.resolve(path)?);
                Reach {
                    current: project.label == self.here.project().label,
                    label: project.label,
                    scope: HistoryScope::Project(project.scope),
                }
            }
        })
    }
}

/// The project the session works in, as its permissions see it now.
#[derive(Clone)]
enum Here {
    Local(PathBuf),
    Remote(WorkspaceIdentity),
}

impl Here {
    fn of(ctx: &ToolContext) -> Self {
        ctx.workspace_session
            .as_ref()
            .and_then(history::remote_identity)
            .map_or_else(|| Self::Local(ctx.permissions.project_cwd()), Self::Remote)
    }

    fn project(&self) -> HistoryProject {
        match self {
            Self::Local(cwd) => HistoryProject::local(cwd),
            Self::Remote(identity) => HistoryProject::workspace(identity.clone()),
        }
    }

    /// A project path as a snippet wrote it: `~` expanded, relative to a local project.
    fn resolve(&self, path: &str) -> Result<PathBuf, ScriptError> {
        let expanded = PathBuf::from(expand_tilde(path).map_err(value_error)?);
        match (expanded.is_absolute(), self) {
            (true, _) => Ok(expanded),
            (false, Self::Local(cwd)) => Ok(cwd.join(expanded)),
            (false, Self::Remote(_)) => Err(value_error(format!(
                "scope '{path}' must be an absolute project path in a remote session"
            ))),
        }
    }
}

enum Requested {
    Session,
    Project,
    All,
    Path(String),
}

impl Requested {
    fn parse(scope: Option<String>) -> Self {
        let Some(scope) = scope else {
            return Self::Project;
        };
        match scope.as_str() {
            PROJECT_SCOPE => Self::Project,
            SESSION_SCOPE => Self::Session,
            ALL_SCOPE => Self::All,
            _ => Self::Path(scope),
        }
    }
}

struct Reach {
    scope: HistoryScope,
    label: String,
    current: bool,
}

impl Reach {
    /// Asks as the `code` tool to read this project's history, the prompt saying what the call
    /// does with it. Only the session's own project carries the mark the built-in rule lets
    /// through.
    fn permission(&self, does: &str) -> Permission {
        let attributes = match self.current {
            true => BTreeMap::from([(
                CURRENT_PROJECT_ATTRIBUTE.to_owned(),
                CURRENT_PROJECT_VALUE.to_owned(),
            )]),
            false => BTreeMap::new(),
        };
        let intent = PermissionIntent::new(
            PermissionScopes::single(format!("{PERMISSION_SCOPE_PREFIX}{}", self.label)),
            vec![PermissionResource {
                kind: PermissionResourceKind::Custom {
                    name: SESSION_HISTORY_RESOURCE.to_owned(),
                },
                value: self.label.clone(),
                access: Some(PermissionResourceAccess::Read),
                protected: false,
                requires_prompt: false,
                attributes,
            }],
            PermissionRisk::Low,
        );
        let identity = (
            PermissionSubject::Native {
                owner: native::OWNER.into(),
                contract: native::code::permission_contract().into(),
            },
            PermissionExecutorKind::Native,
        );
        Permission::ask(ToolKey::native(CODE_TOOL_NAME), intent, Some(identity))
            .with_context(format!("{does} {}", history_of(&self.label)))
    }
}

/// Whose history a prompt names: `the history of /work/app` or `the history of all projects`.
fn history_of(label: &str) -> String {
    match label {
        ALL_PROJECTS_HISTORY => format!("{HISTORY_OF}{ALL_PROJECTS_WORDS}"),
        project => format!("{HISTORY_OF}{project}"),
    }
}

struct SearchCall {
    arguments: Map<String, Value>,
    requested: Requested,
    query: HistoryQuery,
    rerank: bool,
    limit: Option<usize>,
}

impl SearchCall {
    fn parse(call: &ScriptCall) -> Result<Self, ScriptError> {
        let arguments = bind(SEARCH_HELPER, SEARCH_PARAMS, call)?;
        let words = required_text(&arguments, SEARCH_HELPER, QUERY_PARAM)?;
        let mut query = HistoryQuery::new(words, HistoryScope::All);
        query.kinds = strings(&arguments, SEARCH_HELPER, KINDS_PARAM)?
            .map(|kinds| {
                kinds
                    .iter()
                    .map(|kind| kind.parse::<HistoryKind>().map_err(value_error))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        query.tools = strings(&arguments, SEARCH_HELPER, TOOLS_PARAM)?;
        query.since_ms = time(&arguments, SEARCH_HELPER, SINCE_PARAM)?;
        query.until_ms = time(&arguments, SEARCH_HELPER, UNTIL_PARAM)?;
        query.sort = text(&arguments, SEARCH_HELPER, SORT_PARAM)?
            .map(|sort| sort.parse::<HistorySort>().map_err(value_error))
            .transpose()?
            .unwrap_or_default();
        query.offset = count(&arguments, SEARCH_HELPER, OFFSET_PARAM)?.unwrap_or_default();
        Ok(Self {
            requested: Requested::parse(text(&arguments, SEARCH_HELPER, SCOPE_PARAM)?),
            rerank: flag(&arguments, SEARCH_HELPER, RERANK_PARAM)?.unwrap_or(true),
            limit: count(&arguments, SEARCH_HELPER, LIMIT_PARAM)?,
            arguments,
            query,
        })
    }

    fn plan(self, store: HistoryStore, caller: Caller) -> PlanFuture {
        Box::pin(async move {
            let Self {
                arguments,
                requested,
                mut query,
                rerank,
                limit,
            } = self;
            let (reach, caller) = resolve(caller, requested).await?;
            let words = Value::from(query.text.as_str());
            let first_arguments = match arguments.get(SCOPE_PARAM).and_then(Value::as_str) {
                Some(scope) => format!("{words}, {SCOPE_PARAM}={}", Value::from(scope)),
                None => words.to_string(),
            };
            let row = row(
                SEARCH_METHOD,
                HistoryHelper::Search,
                first_arguments,
                arguments,
            );
            let permission = reach.permission(SEARCHES);
            query.scope = reach.scope;
            let run: CallRun = Box::new(move |_| {
                Box::pin(async move {
                    let search = HistorySearch {
                        query,
                        rerank,
                        limit,
                    };
                    let mut results = history::search(&store, search, caller.judge.as_ref())
                        .await
                        .map_err(unreadable)?;
                    if let (Some(notes), Some(decision)) = (&caller.notes, results.decision.take())
                    {
                        notes.emit(decision);
                    }
                    Ok(answered(results.summary(), results_value(&results)))
                })
            });
            Ok(plan(row, permission, run))
        })
    }
}

struct ReadCall {
    arguments: Map<String, Value>,
    reference: HistoryRef,
    offset: usize,
    limit: usize,
    around: usize,
}

impl ReadCall {
    fn parse(call: &ScriptCall) -> Result<Self, ScriptError> {
        let arguments = bind(READ_HELPER, READ_PARAMS, call)?;
        let reference = required_text(&arguments, READ_HELPER, REF_PARAM)?
            .parse::<HistoryRef>()
            .map_err(value_error)?;
        Ok(Self {
            reference,
            offset: count(&arguments, READ_HELPER, OFFSET_PARAM)?.unwrap_or_default(),
            limit: count(&arguments, READ_HELPER, LIMIT_PARAM)?
                .unwrap_or(DEFAULT_READ_BYTES)
                .min(MAX_READ_BYTES),
            around: count(&arguments, READ_HELPER, AROUND_PARAM)?
                .unwrap_or_default()
                .min(MAX_AROUND),
            arguments,
        })
    }

    fn plan(self, store: HistoryStore, caller: Caller) -> PlanFuture {
        Box::pin(async move {
            let Self {
                arguments,
                reference,
                offset,
                limit,
                around,
            } = self;
            let session = reference.session;
            let here = caller.here.clone();
            let own = caller.session == Some(session);
            let (reach, title) = store
                .read(move |database| {
                    let Some(found) = database.history_session(&HistoryScope::All, session)? else {
                        return Ok(None);
                    };
                    let project = here.project();
                    let current = own
                        || database
                            .history_session(&HistoryScope::Project(project.scope), session)?
                            .is_some();
                    let label = match current {
                        true => project.label,
                        false => session_project(&found),
                    };
                    let reach = Reach {
                        scope: HistoryScope::Session(session),
                        label,
                        current,
                    };
                    Ok(Some((reach, found.title)))
                })
                .await
                .map_err(|error| runtime(unreadable_message(&error)))?
                .ok_or_else(|| value_error(format!("no stored session holds `{reference}`")))?;
            let row = row(
                READ_METHOD,
                HistoryHelper::Read,
                Value::from(reference.to_string()).to_string(),
                arguments,
            );
            let permission =
                reach.permission(&format!("{READS} {} {FROM}", Value::from(title.as_str())));
            let run: CallRun = Box::new(move |_| {
                Box::pin(async move {
                    if let Err(error) = store.catch_up(reach.scope).await {
                        tracing::warn!(%error, "history read could not catch the index up");
                    }
                    let lookup = reference.clone();
                    let entry = store
                        .read(move |database| database.read_history(&lookup, offset, limit, around))
                        .await
                        .map_err(unreadable)?
                        .ok_or_else(|| {
                            CallFailure::new(
                                ToolFailure::NotFound,
                                format!("no searchable history item `{reference}`"),
                            )
                        })?;
                    let end = entry.offset + entry.text.len();
                    let text = format!(
                        "{} in {}, bytes {}..{end} of {}",
                        entry.kind,
                        Value::from(entry.session_title.as_str()),
                        entry.offset,
                        entry.total_bytes
                    );
                    Ok(answered(text, entry_value(&entry)))
                })
            });
            Ok(plan(row, permission, run))
        })
    }
}

struct SessionsCall {
    arguments: Map<String, Value>,
    requested: Requested,
    title: Option<String>,
    limit: usize,
    offset: usize,
}

impl SessionsCall {
    fn parse(call: &ScriptCall) -> Result<Self, ScriptError> {
        let arguments = bind(SESSIONS_HELPER, SESSIONS_PARAMS, call)?;
        Ok(Self {
            requested: Requested::parse(text(&arguments, SESSIONS_HELPER, SCOPE_PARAM)?),
            title: text(&arguments, SESSIONS_HELPER, TITLE_PARAM)?,
            limit: count(&arguments, SESSIONS_HELPER, LIMIT_PARAM)?
                .unwrap_or(DEFAULT_SESSIONS)
                .min(MAX_SESSIONS),
            offset: count(&arguments, SESSIONS_HELPER, OFFSET_PARAM)?.unwrap_or_default(),
            arguments,
        })
    }

    fn plan(self, store: HistoryStore, caller: Caller) -> PlanFuture {
        Box::pin(async move {
            let Self {
                arguments,
                requested,
                title,
                limit,
                offset,
            } = self;
            let scope_text = arguments
                .get(SCOPE_PARAM)
                .and_then(Value::as_str)
                .unwrap_or(PROJECT_SCOPE)
                .to_owned();
            let (reach, _) = resolve(caller, requested).await?;
            let row = row(
                SESSIONS_METHOD,
                HistoryHelper::Sessions,
                Value::from(scope_text).to_string(),
                arguments,
            );
            let permission = reach.permission(LISTS_SESSIONS);
            let scope = reach.scope;
            let run: CallRun = Box::new(move |_| {
                Box::pin(async move {
                    let sessions = store
                        .read(move |database| {
                            database.history_sessions(&scope, title.as_deref(), limit, offset)
                        })
                        .await
                        .map_err(unreadable)?;
                    let text = counted(sessions.len(), SESSION_ONE, SESSION_MANY);
                    Ok(answered(text, sessions.iter().map(session_value).collect()))
                })
            });
            Ok(plan(row, permission, run))
        })
    }
}

type PlanFuture = BoxFuture<'static, Result<Box<HostCallPlan>, ScriptError>>;

/// Works out which history a call reaches off the executor, as it takes filesystem reads.
async fn resolve(caller: Caller, requested: Requested) -> Result<(Reach, Caller), ScriptError> {
    smol::unblock(move || caller.reach(&requested).map(|reach| (reach, caller))).await
}

fn judge(ctx: &ToolContext) -> Option<Judge> {
    Some(Judge {
        decisions: ctx.permissions.decisions()?,
        context: ctx
            .permissions
            .decision_context(json!({"tool_use_id": ctx.tool_use_id})),
        permissions: Some(Arc::clone(&ctx.permissions)),
        cancel: ctx.cancel.clone(),
    })
}

fn row(
    method: &str,
    helper: HistoryHelper,
    first_argument: String,
    arguments: Map<String, Value>,
) -> CodeCall {
    CodeCall {
        server: HISTORY_MODULE.to_owned(),
        tool: method.to_owned(),
        called_as: format!("{HISTORY_MODULE}.{method}({first_argument})"),
        input: Value::Object(arguments),
        helper: Some(McpHelper::History(helper)),
        ..CodeCall::default()
    }
}

fn plan(row: CodeCall, permission: Permission, run: CallRun) -> Box<HostCallPlan> {
    Box::new(HostCallPlan {
        row,
        permission,
        read_only: true,
        run,
    })
}

/// The row's summary line, with the value the snippet receives kept for its card to draw.
fn answered(summary: String, value: Value) -> (McpOutput, Value) {
    let output = McpOutput {
        blocks: vec![McpBlock::Text { text: summary }],
        structured: Some(value.clone()),
        ..McpOutput::default()
    };
    (output, value)
}

/// What prompts name another project's session by: its main checkout when it is a local one
/// still on disk, else what it was stored under.
fn session_project(session: &SessionHit) -> String {
    let path = Path::new(&session.project);
    match path.is_absolute() {
        true => HistoryProject::local(path).label,
        false => session.project.clone(),
    }
}

fn results_value(results: &HistoryResults) -> Value {
    json!({
        "hits": results.hits.iter().map(hit_value).collect::<Vec<_>>(),
        "ranking": match results.ranking {
            Ranking::Reranked { .. } => RERANKED,
            Ranking::Keyword { .. } => KEYWORD,
        },
        "judged_unrelated": results.judged_unrelated(),
        "more": results.more,
        "unindexed_sessions": results.unindexed_sessions,
        "note": results.note,
    })
}

fn hit_value(hit: &HistoryHit) -> Value {
    json!({
        "ref": hit.reference.to_string(),
        "session": hit.reference.session.to_string(),
        "title": hit.session_title,
        "project": hit.project,
        "when": iso_time(hit.at_ms),
        "kind": hit.kind.as_str(),
        "tool": hit.tool,
        "agent": agent(&hit.reference),
        "snippet": highlighted(&hit.snippet),
        "score": hit.score,
    })
}

fn entry_value(entry: &HistoryEntry) -> Value {
    json!({
        "ref": entry.reference.to_string(),
        "session": entry.reference.session.to_string(),
        "title": entry.session_title,
        "project": entry.project,
        "when": iso_time(entry.at_ms),
        "kind": entry.kind.as_str(),
        "tool": entry.tool,
        "agent": agent(&entry.reference),
        "text": entry.text,
        "offset": entry.offset,
        "total_bytes": entry.total_bytes,
        "next_offset": entry.next_offset,
        "before": entry.before.iter().map(neighbour_value).collect::<Vec<_>>(),
        "after": entry.after.iter().map(neighbour_value).collect::<Vec<_>>(),
    })
}

fn neighbour_value(neighbour: &NeighbourPreview) -> Value {
    json!({
        "ref": neighbour.reference.to_string(),
        "when": iso_time(neighbour.at_ms),
        "kind": neighbour.kind.as_str(),
        "tool": neighbour.tool,
        "preview": neighbour.preview,
        "truncated": neighbour.truncated,
    })
}

fn session_value(session: &SessionHit) -> Value {
    json!({
        "session": session.id.to_string(),
        "title": session.title,
        "project": session.project,
        "updated": iso_time(session.updated_at.saturating_mul(SECONDS_TO_MILLIS)),
        "items": session.history_items,
        "subagent_items": session.subagent_items,
    })
}

fn agent(reference: &HistoryRef) -> Option<&str> {
    match &reference.stream {
        HistoryStream::Main => None,
        HistoryStream::Subagent(agent) => Some(agent),
    }
}

/// The snippet with each match wrapped in `«…»`.
fn highlighted(snippet: &Snippet) -> String {
    let mut text = String::with_capacity(snippet.text.len() + snippet.highlights.len() * 4);
    let mut at = 0;
    for range in &snippet.highlights {
        let Some(matched) = snippet
            .text
            .get(range.clone())
            .filter(|_| range.start >= at)
        else {
            continue;
        };
        text.push_str(&snippet.text[at..range.start]);
        text.push(HIGHLIGHT_OPEN);
        text.push_str(matched);
        text.push(HIGHLIGHT_CLOSE);
        at = range.end;
    }
    text.push_str(&snippet.text[at..]);
    text
}

/// A list of strings, or `None`.
fn strings(
    arguments: &Map<String, Value>,
    helper: &str,
    param: &str,
) -> Result<Option<Vec<String>>, ScriptError> {
    let type_error = || {
        ScriptError::new(
            ScriptErrorKind::Type,
            format!("{helper}() argument '{param}' must be a list of str"),
        )
    };
    match arguments.get(param) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned).ok_or_else(type_error))
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(type_error()),
    }
}

fn flag(
    arguments: &Map<String, Value>,
    helper: &str,
    param: &str,
) -> Result<Option<bool>, ScriptError> {
    match arguments.get(param) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(flag)) => Ok(Some(*flag)),
        Some(_) => Err(ScriptError::new(
            ScriptErrorKind::Type,
            format!("{helper}() argument '{param}' must be a bool"),
        )),
    }
}

/// An ISO date or datetime as Unix milliseconds. One without an offset is UTC.
fn time(
    arguments: &Map<String, Value>,
    helper: &str,
    param: &str,
) -> Result<Option<u64>, ScriptError> {
    let Some(written) = text(arguments, helper, param)? else {
        return Ok(None);
    };
    history::parse_iso_time(&written)
        .map(Some)
        .ok_or_else(|| {
            value_error(format!(
                "{helper}() argument '{param}' must be an ISO date or datetime, such as 2026-10-16 or 2026-10-16T09:30:00Z, not {}",
                Value::from(written)
            ))
        })
}

fn runtime(message: String) -> ScriptError {
    ScriptError::new(ScriptErrorKind::Runtime, message)
}

fn value_error(message: impl ToString) -> ScriptError {
    ScriptError::new(ScriptErrorKind::Value, message.to_string())
}

fn unreadable_message(error: &SessionError) -> String {
    format!("history is unreadable: {error}")
}

fn unreadable(error: SessionError) -> CallFailure {
    CallFailure::new(ToolFailure::Other, unreadable_message(&error))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use caudra_config::PermissionsConfig;
    use caudra_storage::id::SessionRef;
    use caudra_storage::paths::canonicalize_clean;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::AgentMode;
    use crate::code::gateway::Gateway;
    use crate::code::output::CodeCallStatus;
    use crate::code::repeats::python_repr;
    use crate::code::runner::ScriptName;
    use crate::code::runner::{
        ScriptHost, ScriptInvocation, ScriptOutput, ScriptReply, ScriptStatus,
    };
    use crate::history::tests::{seed, seeded_text, stamped};
    use crate::permissions::{PERMISSION_DENIED_PREFIX, PermissionManager, PermissionMode};
    use crate::tools::registry::PermissionIntent;
    use crate::tools::test_support::stub_ctx_with_permissions;

    const QUERY: &str = "retry";
    const BASE_MS: u64 = 1_767_225_600_000;
    const PLAN_FILE: &str = "/tmp/caudra-plan.md";
    const SPLIT_METHOD: (&str, &str) = ("sea", "rch");
    const KEYWORD_SUMMARY: &str = "keyword order: 1 of 1";
    const HISTORY_READS_LABEL: &str = "reads history of ";

    #[derive(Debug, PartialEq)]
    enum Verdict {
        Ran,
        Asked,
    }

    #[derive(Clone, Copy)]
    enum Target {
        Project,
        Session,
        OtherProject,
        AllProjects,
    }

    struct Fixture {
        project: TempDir,
        other: TempDir,
        state: TempDir,
        permissions: Arc<PermissionManager>,
        ctx: ToolContext,
    }

    /// A saved session working in `project`, with one matching message there and one in `other`,
    /// whose permissions ask about anything no rule allows and have no one to ask, and whose agent
    /// has read the module's reference.
    fn fixture(mode: AgentMode) -> Fixture {
        let (project, other, state) = (temp(), temp(), temp());
        let item = |index| vec![(stamped(BASE_MS, index), seeded_text(QUERY, index))];
        let session = seed(state.path(), &path_text(&project), &item(0));
        seed(state.path(), &path_text(&other), &item(1));
        let permissions = Arc::new(PermissionManager::new_nonpersistent(
            PermissionsConfig::default(),
            project.path().to_path_buf(),
            Arc::default(),
        ));
        permissions.set_session_mode(Some(PermissionMode::Ask));
        let mut ctx = stub_ctx_with_permissions(&mode, Arc::clone(&permissions));
        ctx.session_id = Some(SessionRef::from_id(session));
        ctx.code
            .mark_read(DOC.read_first().map(|member| DOC.qualified(member)));
        Fixture {
            project,
            other,
            state,
            permissions,
            ctx,
        }
    }

    fn temp() -> TempDir {
        TempDir::new().unwrap()
    }

    fn path_text(dir: &TempDir) -> String {
        canonicalize_clean(dir.path()).display().to_string()
    }

    impl Fixture {
        fn gateway(&self) -> Gateway {
            let store = HistoryStore::new(StateDir::from_path(self.state.path().into()));
            Gateway::new(
                self.ctx.clone(),
                vec![Arc::new(HistoryModule::new(Some(store)))],
            )
        }

        fn scope(&self, target: Target) -> Value {
            match target {
                Target::Project => Value::from(PROJECT_SCOPE),
                Target::Session => Value::from(SESSION_SCOPE),
                Target::OtherProject => Value::from(path_text(&self.other)),
                Target::AllProjects => Value::from(ALL_SCOPE),
            }
        }
    }

    fn search(method: &str, scope: Value) -> ScriptCall {
        ScriptCall {
            namespace: Some(HISTORY_MODULE.to_owned()),
            name: method.to_owned(),
            args: vec![Value::from(QUERY)],
            kwargs: Map::from_iter([(SCOPE_PARAM.to_owned(), scope)]),
        }
    }

    fn invoke(host: &dyn ScriptHost, call: ScriptCall) -> ScriptReply {
        match host.call(call) {
            ScriptInvocation::Ready(reply) => reply,
            ScriptInvocation::Deferred(future) => smol::block_on(future),
        }
    }

    fn verdict(reply: &ScriptReply) -> Verdict {
        match reply {
            Ok(_) => Verdict::Ran,
            Err(error)
                if error.kind == ScriptErrorKind::Permission
                    && error.message.starts_with(PERMISSION_DENIED_PREFIX) =>
            {
                Verdict::Asked
            }
            reply => panic!("{reply:?}"),
        }
    }

    /// The session's own project and conversation are read unasked, in plan mode too; another
    /// project, or every project, asks, and with no one to answer the call is denied.
    #[test_case(AgentMode::Build, Target::Project, Verdict::Ran ; "this_project_is_silent")]
    #[test_case(AgentMode::Build, Target::Session, Verdict::Ran ; "this_conversation_is_silent")]
    #[test_case(AgentMode::Plan(PathBuf::from(PLAN_FILE)), Target::Project, Verdict::Ran ; "plan_mode_reads_this_project")]
    #[test_case(AgentMode::ReadOnly, Target::Project, Verdict::Ran ; "a_read_only_policy_reads_this_project")]
    #[test_case(AgentMode::Build, Target::OtherProject, Verdict::Asked ; "another_project_asks")]
    #[test_case(AgentMode::Build, Target::AllProjects, Verdict::Asked ; "all_projects_ask")]
    fn a_read_asks_unless_it_is_this_projects_history(
        mode: AgentMode,
        target: Target,
        expected: Verdict,
    ) {
        let fixture = fixture(mode);
        let gateway = fixture.gateway();

        let reply = invoke(&gateway, search(SEARCH_METHOD, fixture.scope(target)));

        assert_eq!(verdict(&reply), expected, "{reply:?}");
        let calls = gateway.finish();
        let status = match expected {
            Verdict::Ran => CodeCallStatus::Success,
            Verdict::Asked => CodeCallStatus::Refused,
        };
        assert_eq!(calls[0].status, status);
        assert_eq!(calls[0].server, HISTORY_MODULE);
    }

    /// A prompt names the call and whose history it reads, and remembers that history rather
    /// than the one call.
    #[test_case(Target::OtherProject ; "another_project")]
    #[test_case(Target::AllProjects ; "all_projects")]
    fn a_prompt_names_the_call_and_whose_history_it_reads(target: Target) {
        let mut fixture = fixture(AgentMode::Build);
        let (events_tx, events) = flume::unbounded();
        let (_answers, answers_rx) = flume::unbounded::<String>();
        fixture.ctx.event_tx = crate::EventSender::new(events_tx, 0);
        fixture.ctx.user_response_rx = Some(Arc::new(async_lock::Mutex::new(answers_rx)));
        let whose = match target {
            Target::AllProjects => ALL_PROJECTS_WORDS.to_owned(),
            _ => path_text(&fixture.other),
        };
        let gateway = fixture.gateway();
        let scope = fixture.scope(target);
        let call = std::thread::spawn(move || invoke(&gateway, search(SEARCH_METHOD, scope)));

        let request = loop {
            if let crate::AgentEvent::PermissionRequest(request) = events.recv().unwrap().event {
                break request;
            }
        };
        assert!(
            fixture
                .permissions
                .answer(&request.id, crate::permissions::PermissionAnswer::Deny)
        );

        let lines: Vec<&str> = request.presentation.action.lines().collect();
        assert_eq!(
            lines[0],
            format!(
                "{SEARCH_HELPER}({QUERY_PARAM}={}, {SCOPE_PARAM}={})",
                Value::from(QUERY),
                fixture.scope(target)
            )
        );
        assert_eq!(lines[1], format!("{SEARCHES} {HISTORY_OF}{whose}"));
        let remembered = request
            .options
            .iter()
            .find(|option| option.is_default)
            .unwrap();
        assert_eq!(remembered.label, format!("{HISTORY_READS_LABEL}{whose}"));
        assert_eq!(verdict(&call.join().unwrap()), Verdict::Asked);
    }

    /// The `code` tool itself runs unasked under the built-in allow, and that allow is never what
    /// lets a snippet read another project's history.
    #[test]
    fn the_unscoped_code_allow_never_covers_another_projects_history() {
        let fixture = fixture(AgentMode::Build);
        let (events, _received) = flume::unbounded();
        let code_itself = smol::block_on(fixture.permissions.enforce_with_intent(
            &ToolKey::native(CODE_TOOL_NAME),
            &PermissionIntent::new(PermissionScopes::default(), Vec::new(), PermissionRisk::Low),
            &json!({}),
            &crate::EventSender::new(events, 0),
            None,
            CODE_TOOL_NAME,
            &crate::CancelToken::none(),
            None,
            None,
            false,
        ));

        let reply = invoke(
            &fixture.gateway(),
            search(SEARCH_METHOD, fixture.scope(Target::OtherProject)),
        );

        assert!(code_itself.is_ok());
        assert_eq!(verdict(&reply), Verdict::Asked);
    }

    /// `getattr(history, "sea" + "rch")` and `eval` reach the host as the same lookup and call a
    /// written `history.search` does, so they meet the same enforcement. Checked at the host
    /// boundary every interpreter path goes through.
    #[test_case(Target::Project, Verdict::Ran ; "this_project")]
    #[test_case(Target::OtherProject, Verdict::Asked ; "another_project")]
    fn a_computed_name_meets_the_same_enforcement(target: Target, expected: Verdict) {
        let fixture = fixture(AgentMode::Build);
        let gateway = fixture.gateway();
        let method = format!("{}{}", SPLIT_METHOD.0, SPLIT_METHOD.1);

        let resolved = gateway.lookup(Some(HISTORY_MODULE), &method);
        let reply = invoke(&gateway, search(&method, fixture.scope(target)));

        assert_eq!(resolved, ScriptName::Function);
        assert_eq!(verdict(&reply), expected, "{reply:?}");
    }

    #[test]
    fn an_unsaved_session_raises() {
        let fixture = fixture(AgentMode::Build);
        let gateway = Gateway::new(
            fixture.ctx.clone(),
            vec![Arc::new(HistoryModule::new(None))],
        );

        let error = invoke(&gateway, search(SEARCH_METHOD, Value::Null)).unwrap_err();

        assert_eq!(error.kind, ScriptErrorKind::Runtime);
        assert_eq!(error.message, UNSAVED);
        assert!(gateway.finish().is_empty());
    }

    /// A folded row names the history it searched when the snippet chose one, so a search of
    /// every project reads apart from one of this checkout.
    #[test_case(Value::Null, format!("history.search(\"{QUERY}\")") ; "default_scope")]
    #[test_case(
        Value::from(PROJECT_SCOPE),
        format!("history.search(\"{QUERY}\", scope=\"{PROJECT_SCOPE}\")") ;
        "chosen_scope"
    )]
    fn a_search_row_names_the_scope_it_was_given(scope: Value, expected: String) {
        let fixture = fixture(AgentMode::Build);
        let gateway = fixture.gateway();

        invoke(&gateway, search(SEARCH_METHOD, scope)).unwrap();

        assert_eq!(gateway.finish()[0].called_as, expected);
    }

    #[derive(Clone, Copy, Debug)]
    enum Printed {
        Answer,
        Hits,
        Words,
        AnswerBesideAnError,
    }

    /// A print of what the search handed the snippet, whole or the hits it kept, is a repeat of
    /// the call; words of the snippet's own, or output beside an error, are not. Ending on the
    /// answer repeats it too.
    #[test_case(Printed::Answer => (vec![0], Some(0)) ; "the_answer")]
    #[test_case(Printed::Hits => (vec![0], Some(0)) ; "its_hits")]
    #[test_case(Printed::Words => (Vec::new(), Some(0)) ; "own_words")]
    #[test_case(Printed::AnswerBesideAnError => (Vec::new(), Some(0)) ; "beside_an_error")]
    fn a_print_of_a_calls_answer_repeats_it(printed: Printed) -> (Vec<usize>, Option<usize>) {
        let fixture = fixture(AgentMode::Build);
        let gateway = fixture.gateway();
        let answer = invoke(&gateway, search(SEARCH_METHOD, Value::Null)).unwrap();
        let repr = |value: &Value| python_repr(value).unwrap();
        let (stdout, stderr) = match printed {
            Printed::Answer => (repr(&answer), String::new()),
            Printed::Hits => (repr(&answer["hits"]), String::new()),
            Printed::Words => (QUERY.to_owned(), String::new()),
            Printed::AnswerBesideAnError => (repr(&answer), QUERY.to_owned()),
        };

        gateway.repeats(&ScriptOutput {
            stdout: format!("{stdout}\n"),
            stderr,
            result: answer,
            ..ScriptOutput::refused(ScriptStatus::Completed, String::new())
        })
    }

    #[test]
    fn a_search_row_says_how_its_hits_were_ranked_and_keeps_them_for_its_card() {
        let fixture = fixture(AgentMode::Build);
        let gateway = fixture.gateway();

        let value = invoke(&gateway, search(SEARCH_METHOD, Value::Null)).unwrap();

        let calls = gateway.finish();
        assert_eq!(calls[0].called_as, format!("history.search(\"{QUERY}\")"));
        assert_eq!(
            calls[0].helper,
            Some(McpHelper::History(HistoryHelper::Search))
        );
        let output = calls[0].output.as_ref().unwrap();
        assert_eq!(output.display_text(), KEYWORD_SUMMARY);
        assert_eq!(output.structured.as_ref(), Some(&value));
        assert_eq!(value["ranking"], KEYWORD);
        assert_eq!(value["hits"][0]["snippet"], format!("«{QUERY}» 0"));
        assert_eq!(value["hits"][0]["project"], path_text(&fixture.project));
    }

    #[test_case(SEARCH_METHOD, json!({"query": 1}), ScriptErrorKind::Type ; "query_not_a_string")]
    #[test_case(SEARCH_METHOD, json!({"query": QUERY, "kinds": ["tool"]}), ScriptErrorKind::Value ; "unknown_kind")]
    #[test_case(SEARCH_METHOD, json!({"query": QUERY, "since": "last week"}), ScriptErrorKind::Value ; "not_a_date")]
    #[test_case(SEARCH_METHOD, json!({"query": QUERY, "sort": "oldest"}), ScriptErrorKind::Value ; "unknown_sort")]
    #[test_case(SEARCH_METHOD, json!({"query": QUERY, "rerank": "yes"}), ScriptErrorKind::Type ; "rerank_not_a_bool")]
    #[test_case(READ_METHOD, json!({"ref": "nowhere"}), ScriptErrorKind::Value ; "malformed_ref")]
    fn bad_arguments_raise_before_a_row(method: &str, kwargs: Value, kind: ScriptErrorKind) {
        let fixture = fixture(AgentMode::Build);
        let gateway = fixture.gateway();
        let Value::Object(kwargs) = kwargs else {
            panic!("arguments are an object");
        };
        let call = ScriptCall {
            namespace: Some(HISTORY_MODULE.to_owned()),
            name: method.to_owned(),
            args: Vec::new(),
            kwargs,
        };

        assert_eq!(invoke(&gateway, call).unwrap_err().kind, kind);
        assert!(gateway.finish().is_empty());
    }

    #[test_case("2026-10-16", 1_792_108_800_000 ; "a_date_is_utc_midnight")]
    #[test_case("2026-10-16T09:30:00", 1_792_143_000_000 ; "a_datetime_without_offset_is_utc")]
    #[test_case("2026-10-16T09:30:00+02:00", 1_792_135_800_000 ; "an_offset_is_honoured")]
    fn times_are_read_as_iso(written: &str, expected: u64) {
        let arguments = Map::from_iter([(SINCE_PARAM.to_owned(), Value::from(written))]);

        assert_eq!(
            time(&arguments, SEARCH_HELPER, SINCE_PARAM).unwrap(),
            Some(expected)
        );
    }
}
