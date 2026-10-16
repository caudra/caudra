//! Searching, reading and listing stored history. Everything here only reads,
//! so it runs on a read-only connection.
//!
//! A search first takes the items matching every term, then fills any
//! shortfall from items matching any term, each tier ordered by the query's
//! sort. Fork copies share their item ids, so each item is kept once, from the
//! most recently updated session.

use std::collections::{HashMap, HashSet};

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde_json::Value;

use super::extract::{classify, full_text, index_body, item_id};
use super::snippet::{Highlighter, window};
use super::{
    HistoryEntry, HistoryHit, HistoryKind, HistoryPage, HistoryQuery, HistoryRef, HistoryScope,
    HistorySort, HistoryStream, NeighbourPreview, SessionHit, Snippet, id_value, project_label,
    source,
};
use crate::id::CaudraId;
use crate::sessions::{
    SessionDatabase, SessionError, decompress_payload, from_i64, from_i64_usize,
    history_uuid_timestamp, id_from_row, to_i64,
};

/// BM25 is scaled by these, so what people said and what compaction kept
/// outranks the machinery around it.
const KIND_WEIGHTS: [(HistoryKind, f64); 6] = [
    (HistoryKind::User, 1.0),
    (HistoryKind::Summary, 1.0),
    (HistoryKind::Assistant, 0.85),
    (HistoryKind::Reasoning, 0.6),
    (HistoryKind::ToolCall, 0.5),
    (HistoryKind::ToolResult, 0.4),
];
const MAX_QUERY_TERMS: usize = 16;
/// Distinct matches counted past the page before a scan stops.
const MATCH_COUNT_CAP: usize = 1000;
const NEIGHBOUR_PREVIEW_BYTES: usize = 200;
const PHRASE_QUOTE: char = '"';
const ALL_TERMS: &str = " AND ";
const ANY_TERM: &str = " OR ";
const HIT_COLUMNS: &str = "d.session_id, d.stream, d.ordinal, d.item_id, d.kind, d.tool, \
     d.at_ms, s.title, s.cwd, s.workspace_source, s.workspace_project";
const SESSION_COLUMNS: &str = "s.id, s.title, s.cwd, s.workspace_source, s.workspace_project, \
     s.updated_at, s.history_item_count, s.subagent_item_count";
const RELEVANCE_ORDER: &str = "score DESC, s.updated_at DESC, s.id DESC, d.doc";
const NEWEST_ORDER: &str = "d.at_ms DESC, s.updated_at DESC, s.id DESC, d.doc";
const PAYLOAD_FIELD: &str = "history payload";
const KIND_FIELD: &str = "history_search_docs.kind";
const AT_FIELD: &str = "history_search_docs.at_ms";
const ORDINAL_FIELD: &str = "history_search_docs.ordinal";
const UPDATED_FIELD: &str = "sessions.updated_at";
const COUNT_FIELD: &str = "sessions item count";
const PAGE_FIELD: &str = "history page";

/// A match before its snippet is built.
struct Candidate {
    reference: HistoryRef,
    ordinal: usize,
    kind: HistoryKind,
    tool: Option<String>,
    at_ms: u64,
    session_title: String,
    project: String,
    score: f64,
}

/// Takes matches in order, keeping each item once and each session under its
/// cap, until the page and the match count are both full.
struct Collector {
    wanted: usize,
    per_session_cap: Option<usize>,
    seen: HashSet<CaudraId>,
    per_session: HashMap<CaudraId, usize>,
    accepted: Vec<Candidate>,
}

impl Collector {
    fn new(query: &HistoryQuery) -> Self {
        Self {
            wanted: query.offset.saturating_add(query.limit).saturating_add(1),
            per_session_cap: query
                .per_session_cap
                .filter(|_| query.scope.spans_sessions()),
            seen: HashSet::new(),
            per_session: HashMap::new(),
            accepted: Vec::new(),
        }
    }

    fn full(&self) -> bool {
        self.accepted.len() >= self.wanted
    }

    fn satisfied(&self) -> bool {
        self.full() && self.seen.len() >= MATCH_COUNT_CAP
    }

    fn offer(&mut self, candidate: Candidate) {
        if !self.seen.insert(candidate.reference.item) || self.full() {
            return;
        }
        let taken = self
            .per_session
            .entry(candidate.reference.session)
            .or_default();
        if self.per_session_cap.is_some_and(|cap| *taken >= cap) {
            return;
        }
        *taken += 1;
        self.accepted.push(candidate);
    }
}

impl SessionDatabase {
    pub fn search_history(&self, query: &HistoryQuery) -> Result<HistoryPage, SessionError> {
        let connection = self.connection();
        let terms = terms(&query.text);
        if terms.is_empty() && !query.text.trim().is_empty() {
            return Ok(HistoryPage::default());
        }
        let mut collector = Collector::new(query);
        if terms.is_empty() {
            scan(connection, query, None, &mut collector)?;
        } else {
            scan(
                connection,
                query,
                Some(&terms.join(ALL_TERMS)),
                &mut collector,
            )?;
            if terms.len() > 1 && !collector.full() {
                scan(
                    connection,
                    query,
                    Some(&terms.join(ANY_TERM)),
                    &mut collector,
                )?;
            }
        }
        let matched = collector.seen.len().min(MATCH_COUNT_CAP);
        let more = collector.accepted.len() > query.offset.saturating_add(query.limit);
        let highlighter = (!terms.is_empty()).then(Highlighter::new).transpose()?;
        let expression = terms.join(ANY_TERM);
        let hits = collector
            .accepted
            .into_iter()
            .skip(query.offset)
            .take(query.limit)
            .map(|candidate| {
                let snippet = snippet(connection, &candidate, highlighter.as_ref(), &expression)?;
                Ok(HistoryHit {
                    reference: candidate.reference,
                    session_title: candidate.session_title,
                    project: candidate.project,
                    kind: candidate.kind,
                    tool: candidate.tool,
                    at_ms: candidate.at_ms,
                    score: candidate.score,
                    snippet,
                })
            })
            .collect::<Result<_, SessionError>>()?;
        Ok(HistoryPage {
            hits,
            more,
            matched,
        })
    }

    /// One item's text from byte `offset`, at most `limit` bytes of it, with
    /// up to `around` neighbours on each side. `None` when the reference names
    /// no indexed item.
    pub fn read_history(
        &self,
        reference: &HistoryRef,
        offset: usize,
        limit: usize,
        around: usize,
    ) -> Result<Option<HistoryEntry>, SessionError> {
        let connection = self.connection();
        let found = connection
            .query_row(
                "SELECT d.ordinal, d.kind, d.tool, d.at_ms, s.title, s.cwd, \
                     s.workspace_source, s.workspace_project \
                 FROM history_search_docs d JOIN sessions s ON s.id = d.session_id \
                 WHERE d.session_id = ?1 AND d.stream = ?2 AND d.item_id = ?3",
                params![
                    reference.session.as_bytes().as_slice(),
                    reference.stream.key(),
                    reference.item.as_bytes().as_slice()
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        project_label(&row.get::<_, String>(6)?, row.get(5)?, row.get(7)?),
                    ))
                },
            )
            .optional()?;
        let Some((ordinal, kind, tool, at_ms, session_title, project)) = found else {
            return Ok(None);
        };
        let ordinal = from_i64_usize(ordinal, ORDINAL_FIELD)?;
        let Some(payload) = payload_at(connection, reference, ordinal)? else {
            return Ok(None);
        };
        let text = full_text(&payload).unwrap_or_default();
        let start = text.floor_char_boundary(offset.min(text.len()));
        let mut end = text.floor_char_boundary(start.saturating_add(limit));
        if end == start && start < text.len() && limit > 0 {
            end = text.ceil_char_boundary(start + 1);
        }
        let (before, after) = neighbours(connection, reference, ordinal, around)?;
        Ok(Some(HistoryEntry {
            reference: reference.clone(),
            session_title,
            project,
            kind: kind_from_code(kind)?,
            tool,
            at_ms: from_i64(at_ms, AT_FIELD)?,
            text: text[start..end].to_owned(),
            offset: start,
            total_bytes: text.len(),
            next_offset: (end < text.len()).then_some(end),
            before,
            after,
        }))
    }

    /// Sessions in `scope`, most recently updated first, optionally only those
    /// whose title contains `title` (ASCII case folded).
    pub fn history_sessions(
        &self,
        scope: &HistoryScope,
        title: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SessionHit>, SessionError> {
        let (filter, mut values) = scope.filter();
        let title = title.map_or(SqlValue::Null, |title| SqlValue::Text(title.to_owned()));
        values.extend([
            title.clone(),
            title,
            SqlValue::Integer(to_i64(limit, PAGE_FIELD)?),
            SqlValue::Integer(to_i64(offset, PAGE_FIELD)?),
        ]);
        let mut statement = self.connection().prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions s WHERE {filter} \
                 AND (? IS NULL OR instr(lower(s.title), lower(?)) > 0) \
             ORDER BY s.updated_at DESC, s.id DESC LIMIT ? OFFSET ?"
        ))?;
        let mut rows = statement.query(params_from_iter(values))?;
        let mut sessions = Vec::new();
        while let Some(row) = rows.next()? {
            sessions.push(session_hit(row)?);
        }
        Ok(sessions)
    }

    /// Session `id`, when it is stored and falls in `scope`.
    pub fn history_session(
        &self,
        scope: &HistoryScope,
        id: CaudraId,
    ) -> Result<Option<SessionHit>, SessionError> {
        let (filter, mut values) = scope.filter();
        values.push(id_value(id));
        let mut statement = self.connection().prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions s WHERE {filter} AND s.id = ?"
        ))?;
        let mut rows = statement.query(params_from_iter(values))?;
        rows.next()?.map(session_hit).transpose()
    }
}

fn session_hit(row: &Row<'_>) -> Result<SessionHit, SessionError> {
    Ok(SessionHit {
        id: id_from_row(row, 0)?,
        title: row.get(1)?,
        project: project_label(&row.get::<_, String>(3)?, row.get(2)?, row.get(4)?),
        updated_at: from_i64(row.get(5)?, UPDATED_FIELD)?,
        history_items: from_i64(row.get(6)?, COUNT_FIELD)?,
        subagent_items: from_i64(row.get(7)?, COUNT_FIELD)?,
    })
}

/// Free text as quoted FTS5 strings: each word, or each `"quoted phrase"`
/// whole. Pieces with no letter or digit are dropped, because the tokenizer
/// would find nothing in them.
fn terms(text: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for (index, part) in text.split(PHRASE_QUOTE).enumerate() {
        let pieces: Vec<&str> = if index % 2 == 1 {
            vec![part.trim()]
        } else {
            part.split_whitespace().collect()
        };
        for piece in pieces {
            let quoted = format!("{PHRASE_QUOTE}{piece}{PHRASE_QUOTE}");
            if piece.chars().any(char::is_alphanumeric) && !terms.contains(&quoted) {
                terms.push(quoted);
            }
        }
    }
    terms.truncate(MAX_QUERY_TERMS);
    terms
}

/// The filters on `d` (documents) and `s` (sessions) every search applies.
fn filters(query: &HistoryQuery) -> Result<(String, Vec<SqlValue>), SessionError> {
    let (scope, mut values) = query.scope.filter();
    let mut clauses = vec![scope];
    if let Some(kinds) = &query.kinds {
        clauses.push(in_list("d.kind", kinds.len()));
        values.extend(kinds.iter().map(|kind| SqlValue::Integer(kind.code())));
    }
    if let Some(tools) = &query.tools {
        clauses.push(in_list("d.tool", tools.len()));
        values.extend(tools.iter().cloned().map(SqlValue::Text));
    }
    if let Some(since) = query.since_ms {
        clauses.push("d.at_ms >= ?".to_owned());
        values.push(SqlValue::Integer(to_i64(since, AT_FIELD)?));
    }
    if let Some(until) = query.until_ms {
        clauses.push("d.at_ms < ?".to_owned());
        values.push(SqlValue::Integer(to_i64(until, AT_FIELD)?));
    }
    Ok((clauses.join(" AND "), values))
}

fn in_list(column: &str, len: usize) -> String {
    if len == 0 {
        return "0".to_owned();
    }
    format!("{column} IN ({})", vec!["?"; len].join(", "))
}

fn weighted_score() -> String {
    let cases: Vec<String> = KIND_WEIGHTS
        .iter()
        .map(|(kind, weight)| format!("WHEN {} THEN {weight:?}", kind.code()))
        .collect();
    format!(
        "-bm25(history_search) * CASE d.kind {} ELSE 1.0 END",
        cases.join(" ")
    )
}

/// Offers the matches of `expression` to `collector` in the query's order,
/// or the newest items passing the filters when there is no expression.
fn scan(
    connection: &Connection,
    query: &HistoryQuery,
    expression: Option<&str>,
    collector: &mut Collector,
) -> Result<(), SessionError> {
    let (filter, mut values) = filters(query)?;
    let order = match query.sort {
        HistorySort::Relevance if expression.is_some() => RELEVANCE_ORDER,
        _ => NEWEST_ORDER,
    };
    let sql = match expression {
        Some(expression) => {
            values.insert(0, SqlValue::Text(expression.to_owned()));
            format!(
                "SELECT {HIT_COLUMNS}, {} AS score FROM history_search \
                 JOIN history_search_docs d ON d.doc = history_search.rowid \
                 JOIN sessions s ON s.id = d.session_id \
                 WHERE history_search MATCH ? AND {filter} ORDER BY {order}",
                weighted_score()
            )
        }
        None => format!(
            "SELECT {HIT_COLUMNS}, 0.0 AS score FROM history_search_docs d \
             JOIN sessions s ON s.id = d.session_id WHERE {filter} ORDER BY {order}"
        ),
    };
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query(params_from_iter(values))?;
    while !collector.satisfied() {
        let Some(row) = rows.next()? else { break };
        collector.offer(candidate(row)?);
    }
    Ok(())
}

fn candidate(row: &Row<'_>) -> Result<Candidate, SessionError> {
    Ok(Candidate {
        reference: HistoryRef {
            session: id_from_row(row, 0)?,
            stream: HistoryStream::from_key(row.get(1)?),
            item: id_from_row(row, 3)?,
        },
        ordinal: from_i64_usize(row.get(2)?, ORDINAL_FIELD)?,
        kind: kind_from_code(row.get(4)?)?,
        tool: row.get(5)?,
        at_ms: from_i64(row.get(6)?, AT_FIELD)?,
        session_title: row.get(7)?,
        project: project_label(&row.get::<_, String>(9)?, row.get(8)?, row.get(10)?),
        score: row.get(11)?,
    })
}

fn kind_from_code(code: i64) -> Result<HistoryKind, SessionError> {
    HistoryKind::from_code(code).ok_or_else(|| SessionError::CorruptDatabaseValue {
        field: KIND_FIELD,
        reason: format!("unknown kind code {code}"),
    })
}

/// The payload at `ordinal` of the reference's stream, when it is still the
/// item the reference names.
fn payload_at(
    connection: &Connection,
    reference: &HistoryRef,
    ordinal: usize,
) -> Result<Option<Value>, SessionError> {
    let stored: Option<Vec<u8>> = connection
        .query_row(
            &format!(
                "SELECT payload FROM {} AND ordinal = ?3",
                source(reference.stream.key())
            ),
            params![
                reference.session.as_bytes().as_slice(),
                reference.stream.key(),
                to_i64(ordinal, ORDINAL_FIELD)?
            ],
            |row| row.get(0),
        )
        .optional()?;
    let Some(stored) = stored else {
        return Ok(None);
    };
    let payload: Option<Value> =
        serde_json::from_str(&decompress_payload(&stored, PAYLOAD_FIELD)?).ok();
    Ok(payload.filter(|payload| item_id(payload) == Some(reference.item)))
}

fn snippet(
    connection: &Connection,
    candidate: &Candidate,
    highlighter: Option<&Highlighter>,
    expression: &str,
) -> Result<Snippet, SessionError> {
    let Some(body) = payload_at(connection, &candidate.reference, candidate.ordinal)?
        .as_ref()
        .and_then(index_body)
    else {
        return Ok(Snippet::default());
    };
    let ranges = match highlighter {
        Some(highlighter) => highlighter.ranges(&body, expression)?,
        None => Vec::new(),
    };
    Ok(window(&body, &ranges))
}

fn neighbours(
    connection: &Connection,
    reference: &HistoryRef,
    ordinal: usize,
    around: usize,
) -> Result<(Vec<NeighbourPreview>, Vec<NeighbourPreview>), SessionError> {
    if around == 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    let session = reference.session.as_bytes().as_slice();
    let stream = reference.stream.key();
    let mut statement = connection.prepare(&format!(
        "SELECT ordinal, payload FROM {} AND ordinal >= ?3 AND ordinal <= ?4 ORDER BY ordinal",
        source(stream)
    ))?;
    let mut rows = statement.query(params![
        session,
        stream,
        to_i64(ordinal.saturating_sub(around), ORDINAL_FIELD)?,
        to_i64(ordinal.saturating_add(around), ORDINAL_FIELD)?
    ])?;
    let (mut before, mut after) = (Vec::new(), Vec::new());
    while let Some(row) = rows.next()? {
        let neighbour = from_i64_usize(row.get(0)?, ORDINAL_FIELD)?;
        if neighbour == ordinal {
            continue;
        }
        let stored: Vec<u8> = row.get(1)?;
        let Ok(payload) =
            serde_json::from_str::<Value>(&decompress_payload(&stored, PAYLOAD_FIELD)?)
        else {
            continue;
        };
        let (Some(item), Some((kind, call_tool)), Some(text)) =
            (item_id(&payload), classify(&payload), full_text(&payload))
        else {
            continue;
        };
        let tool = match call_tool {
            Some(tool) => Some(tool.to_owned()),
            None => indexed_tool(connection, reference, item)?,
        };
        let end = text.floor_char_boundary(NEIGHBOUR_PREVIEW_BYTES);
        let preview = NeighbourPreview {
            reference: HistoryRef {
                session: reference.session,
                stream: reference.stream.clone(),
                item,
            },
            kind,
            tool,
            at_ms: history_uuid_timestamp(item).unwrap_or_default(),
            preview: text[..end].to_owned(),
            truncated: end < text.len(),
        };
        if neighbour < ordinal {
            before.push(preview);
        } else {
            after.push(preview);
        }
    }
    Ok((before, after))
}

fn indexed_tool(
    connection: &Connection,
    reference: &HistoryRef,
    item: CaudraId,
) -> Result<Option<String>, SessionError> {
    Ok(connection
        .query_row(
            "SELECT tool FROM history_search_docs \
             WHERE session_id = ?1 AND stream = ?2 AND item_id = ?3",
            params![
                reference.session.as_bytes().as_slice(),
                reference.stream.key(),
                item.as_bytes().as_slice()
            ],
            |row| row.get(0),
        )
        .optional()?
        .flatten())
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::super::fixtures::{
        BASE_MS, OTHER_PROJECT, PROJECT, SHELL, assistant, catch_up, database, session_with,
        tool_call, user, user_at,
    };
    use super::super::{ProjectScope, WorkspaceIdentity};
    use super::*;
    use crate::StateDir;

    const RETRY_LOOP: &str = "the retry loop sleeps too long";
    const LOOP_RETRY: &str = "loop forever, then retry";
    const ONLY_RETRY: &str = "retry once";
    const UNRELATED: &str = "nothing to see here";
    const TITLE: &str = "Fix the Flaky Test";
    const HOUR_MS: u64 = 3_600_000;
    const PAGE_BYTES: usize = 10;

    fn texts(page: &HistoryPage) -> Vec<String> {
        page.hits
            .iter()
            .map(|hit| hit.snippet.text.clone())
            .collect()
    }

    fn query(text: &str) -> HistoryQuery {
        HistoryQuery::new(text, HistoryScope::All)
    }

    fn seeded(sessions: Vec<(&str, Vec<Value>)>) -> (tempfile::TempDir, SessionDatabase) {
        let (temp, mut database) = database();
        for (cwd, items) in sessions {
            database
                .save(&session_with(cwd, TITLE, items), None)
                .unwrap();
        }
        catch_up(&database, &HistoryScope::All);
        (temp, database)
    }

    fn retry_corpus() -> (tempfile::TempDir, SessionDatabase) {
        seeded(vec![(
            PROJECT,
            vec![
                user(ONLY_RETRY),
                user(RETRY_LOOP),
                user(LOOP_RETRY),
                user(UNRELATED),
            ],
        )])
    }

    fn sorted(mut texts: Vec<String>) -> Vec<String> {
        texts.sort();
        texts
    }

    #[test]
    fn all_terms_come_before_any_term() {
        let (_temp, database) = retry_corpus();
        let page = database.search_history(&query("retry loop")).unwrap();
        let found = texts(&page);
        assert_eq!(
            sorted(found[..2].to_vec()),
            sorted(vec![LOOP_RETRY.to_owned(), RETRY_LOOP.to_owned()])
        );
        assert_eq!(found[2..], [ONLY_RETRY]);
        assert_eq!(page.matched, 3);
    }

    #[test_case("\"retry loop\"", &[RETRY_LOOP]; "phrase")]
    #[test_case("retries", &[LOOP_RETRY, ONLY_RETRY, RETRY_LOOP]; "stemmed")]
    #[test_case("RETRY", &[LOOP_RETRY, ONLY_RETRY, RETRY_LOOP]; "case_folded")]
    fn matching_terms(text: &str, expected: &[&str]) {
        let (_temp, database) = retry_corpus();
        let page = database.search_history(&query(text)).unwrap();
        assert_eq!(sorted(texts(&page)), expected);
        assert_eq!(page.matched, expected.len());
    }

    #[test_case("\""; "lone_quote")]
    #[test_case("*"; "star")]
    #[test_case("retry*"; "prefix_star")]
    #[test_case("-retry"; "minus")]
    #[test_case("body:retry"; "column_filter")]
    #[test_case("NEAR(retry loop)"; "near")]
    #[test_case("retry AND OR NOT"; "operators")]
    #[test_case("\"unbalanced retry"; "unbalanced_phrase")]
    #[test_case("^retry + ( ) {}"; "punctuation")]
    #[test_case("'; DROP TABLE sessions; --"; "injection")]
    fn query_syntax_never_errors(text: &str) {
        let (_temp, database) = seeded(vec![(PROJECT, vec![user(RETRY_LOOP)])]);
        database.search_history(&query(text)).unwrap();
    }

    #[test]
    fn filters_narrow_kind_tool_and_time() {
        let (_temp, database) = seeded(vec![(
            PROJECT,
            vec![
                user_at(BASE_MS, "deploy early"),
                user_at(BASE_MS + 2 * HOUR_MS, "deploy late"),
                assistant("deploy answer"),
                tool_call("c1", SHELL, "deploy.sh"),
                tool_call("c2", "file_read", "deploy.md"),
            ],
        )]);
        let mut kinds = query("deploy");
        kinds.kinds = Some(vec![HistoryKind::Assistant]);
        assert_eq!(
            texts(&database.search_history(&kinds).unwrap()),
            ["deploy answer"]
        );

        let mut tools = query("deploy");
        tools.tools = Some(vec![SHELL.to_owned()]);
        let page = database.search_history(&tools).unwrap();
        assert_eq!(page.hits.len(), 1);
        assert_eq!(page.hits[0].tool.as_deref(), Some(SHELL));

        let mut window = query("deploy");
        window.kinds = Some(vec![HistoryKind::User]);
        window.since_ms = Some(BASE_MS + HOUR_MS);
        assert_eq!(
            texts(&database.search_history(&window).unwrap()),
            ["deploy late"]
        );
        window.since_ms = None;
        window.until_ms = Some(BASE_MS + HOUR_MS);
        assert_eq!(
            texts(&database.search_history(&window).unwrap()),
            ["deploy early"]
        );
    }

    #[test]
    fn scope_selects_sessions() {
        let (_temp, mut database) = database();
        let here = session_with(PROJECT, TITLE, vec![user("zebra here")]);
        let nested = session_with(
            &format!("{PROJECT}/crate"),
            TITLE,
            vec![user("zebra nested")],
        );
        let sibling = session_with(
            &format!("{PROJECT}-sibling"),
            TITLE,
            vec![user("zebra sibling")],
        );
        let other = session_with(OTHER_PROJECT, TITLE, vec![user("zebra other")]);
        for session in [&here, &nested, &sibling, &other] {
            database.save(session, None).unwrap();
        }
        catch_up(&database, &HistoryScope::All);
        let count = |scope: HistoryScope| {
            database
                .search_history(&HistoryQuery::new("zebra", scope))
                .unwrap()
                .hits
                .len()
        };

        assert_eq!(count(HistoryScope::Session(here.id)), 1);
        assert_eq!(
            count(HistoryScope::Project(ProjectScope::Local(vec![
                PROJECT.into()
            ]))),
            2
        );
        assert_eq!(
            count(HistoryScope::Project(ProjectScope::Local(vec![
                PROJECT.into(),
                OTHER_PROJECT.into()
            ]))),
            3
        );
        assert_eq!(
            count(HistoryScope::Project(ProjectScope::Workspace(
                WorkspaceIdentity {
                    source: "remote".into(),
                    authority: String::new(),
                    principal: String::new(),
                    project: String::new(),
                }
            ))),
            0
        );
        assert_eq!(count(HistoryScope::All), 4);
    }

    #[test]
    fn per_session_cap_spreads_hits_across_sessions() {
        let (_temp, database) = seeded(vec![
            (
                PROJECT,
                vec![user("zebra 1"), user("zebra 2"), user("zebra 3")],
            ),
            (PROJECT, vec![user("zebra 4")]),
        ]);
        let mut capped = query("zebra");
        capped.per_session_cap = Some(1);
        assert_eq!(database.search_history(&capped).unwrap().hits.len(), 2);

        let session = database.search_history(&capped).unwrap().hits[1]
            .reference
            .session;
        let mut within = capped.clone();
        within.scope = HistoryScope::Session(session);
        assert!(!database.search_history(&within).unwrap().hits.is_empty());
    }

    #[test]
    fn newest_orders_by_time_and_pages() {
        let (_temp, database) = seeded(vec![(
            PROJECT,
            vec![
                user_at(BASE_MS + HOUR_MS, "zebra middle"),
                user_at(BASE_MS + 2 * HOUR_MS, "zebra zebra zebra newest"),
                user_at(BASE_MS, "zebra oldest"),
            ],
        )]);
        let mut newest = query("zebra");
        newest.sort = HistorySort::Newest;
        newest.limit = 2;
        let page = database.search_history(&newest).unwrap();
        assert_eq!(texts(&page), ["zebra zebra zebra newest", "zebra middle"]);
        assert!(page.more);

        newest.offset = 2;
        let page = database.search_history(&newest).unwrap();
        assert_eq!(texts(&page), ["zebra oldest"]);
        assert!(!page.more);
    }

    #[test]
    fn blank_text_lists_the_newest_items() {
        let (_temp, database) = seeded(vec![(
            PROJECT,
            vec![
                user_at(BASE_MS, "first"),
                user_at(BASE_MS + HOUR_MS, "second"),
            ],
        )]);
        let page = database.search_history(&query("")).unwrap();
        assert_eq!(texts(&page), ["second", "first"]);
    }

    #[test]
    fn fork_copies_collapse_to_the_newest_session() {
        let (_temp, mut database) = database();
        let shared = user("zebra shared");
        let original = session_with(PROJECT, TITLE, vec![shared.clone()]);
        let mut fork = session_with(PROJECT, TITLE, vec![shared]);
        fork.updated_at = original.updated_at + 1;
        database.save(&original, None).unwrap();
        database.save(&fork, None).unwrap();
        catch_up(&database, &HistoryScope::All);

        let page = database.search_history(&query("zebra")).unwrap();

        assert_eq!(page.hits.len(), 1);
        assert_eq!(page.matched, 1);
        assert_eq!(page.hits[0].reference.session, fork.id);
    }

    #[test]
    fn snippets_highlight_terms() {
        let (_temp, database) = seeded(vec![(PROJECT, vec![user(RETRY_LOOP)])]);
        let page = database.search_history(&query("sleeping retries")).unwrap();
        let snippet = &page.hits[0].snippet;
        let marked: Vec<&str> = snippet
            .highlights
            .iter()
            .map(|range| &snippet.text[range.clone()])
            .collect();
        assert_eq!(marked, ["retry", "sleeps"]);
        assert_eq!(page.hits[0].session_title, TITLE);
        assert_eq!(page.hits[0].project, PROJECT);
        assert_eq!(page.hits[0].at_ms, BASE_MS);
        assert!(page.hits[0].score > 0.0);
    }

    #[test]
    fn read_pages_text_and_shows_neighbours() {
        let (_temp, database) = seeded(vec![(
            PROJECT,
            vec![user("before"), assistant("é zebra answer"), user("after")],
        )]);
        let reference = database.search_history(&query("zebra")).unwrap().hits[0]
            .reference
            .clone();

        let entry = database
            .read_history(&reference, 1, PAGE_BYTES, 1)
            .unwrap()
            .unwrap();

        assert_eq!(entry.kind, HistoryKind::Assistant);
        assert_eq!(entry.offset, 0);
        assert_eq!(entry.text, "é zebra a");
        assert_eq!(entry.total_bytes, "é zebra answer".len());
        assert_eq!(entry.next_offset, Some(entry.text.len()));
        assert_eq!(entry.before[0].preview, "before");
        assert_eq!(entry.after[0].preview, "after");

        let missing = HistoryRef {
            item: CaudraId::generate(),
            ..reference
        };
        assert_eq!(
            database.read_history(&missing, 0, PAGE_BYTES, 0).unwrap(),
            None
        );
    }

    #[test]
    fn sessions_list_by_scope_and_title() {
        let (_temp, database) = seeded(vec![
            (PROJECT, vec![user("one")]),
            (OTHER_PROJECT, vec![user("two")]),
        ]);
        let all = database
            .history_sessions(&HistoryScope::All, Some("flaky"), 10, 0)
            .unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].history_items, 1);

        let scoped = database
            .history_sessions(
                &HistoryScope::Project(ProjectScope::Local(vec![PROJECT.into()])),
                None,
                10,
                0,
            )
            .unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].project, PROJECT);
        assert!(
            database
                .history_sessions(&HistoryScope::All, Some("absent"), 10, 0)
                .unwrap()
                .is_empty()
        );
    }

    #[test_case(PROJECT, true; "inside")]
    #[test_case(OTHER_PROJECT, false; "outside")]
    fn a_session_is_found_only_within_its_scope(cwd: &str, inside: bool) {
        let (_temp, mut database) = database();
        let session = session_with(cwd, TITLE, vec![user("one")]);
        database.save(&session, None).unwrap();
        let scope = HistoryScope::Project(ProjectScope::Local(vec![PROJECT.into()]));

        let found = database.history_session(&scope, session.id).unwrap();

        assert_eq!(found.is_some(), inside);
        let anywhere = database
            .history_session(&HistoryScope::All, session.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            (anywhere.title.as_str(), anywhere.project.as_str()),
            (TITLE, cwd)
        );
        assert_eq!(
            database
                .history_session(&HistoryScope::All, CaudraId::generate())
                .unwrap(),
            None
        );
    }

    #[test]
    fn search_runs_on_a_read_only_connection() {
        let (temp, database) = seeded(vec![(PROJECT, vec![user(RETRY_LOOP)])]);
        drop(database);
        let state = StateDir::from_path(temp.path().to_path_buf());

        let reader = SessionDatabase::open_read_only(&state).unwrap();

        assert_eq!(
            reader.search_history(&query("retry")).unwrap().hits.len(),
            1
        );
    }
}
