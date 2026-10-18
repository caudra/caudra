//! The `search()` helper: connected servers' tools ranked against a query.

use std::cmp::Reverse;
use std::collections::HashMap;

use super::catalog::unavailable;
use super::naming::Names;
use super::signature::{doc, signature};
use super::view::{ServerView, ToolView};

/// What the stub declares as `search()`'s default `limit`.
pub const DEFAULT_LIMIT: usize = 10;
pub const MAX_LIMIT: usize = 50;

const MIN_LIMIT: usize = 1;
const EXACT_SCORE: usize = usize::MAX;
const NAME_WEIGHT: usize = 3;
const TITLE_WEIGHT: usize = 2;
const DESCRIPTION_WEIGHT: usize = 1;
const SERVER_WEIGHT: usize = 1;
const QUALIFIER: char = '.';
const TERM_SEPARATORS: [char; 3] = ['.', '_', '-'];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub namespace: String,
    pub method: String,
    pub signature: String,
    pub doc: String,
}

struct Candidate<'a> {
    namespace: &'a str,
    method: &'a str,
    server: &'a ServerView,
    tool: &'a ToolView,
}

/// Ranks connected servers' tools against `query`, optionally within one server given by
/// namespace or configured name. A blank query lists every tool in scope.
pub fn search(
    servers: &[ServerView],
    names: &Names,
    query: &str,
    server: Option<&str>,
    limit: usize,
    offset: usize,
) -> Result<Vec<SearchHit>, String> {
    let by_name: HashMap<&str, &ServerView> = servers
        .iter()
        .map(|server| (server.name.as_str(), server))
        .collect();
    let scope = server
        .map(|requested| resolve_scope(&by_name, names, requested))
        .transpose()?;
    let query = query.trim();
    let terms = terms(query);
    let mut scored: Vec<(usize, Candidate)> = names
        .namespaces()
        .filter(|(_, server)| scope.is_none_or(|scope| scope == *server))
        .filter_map(|(namespace, server)| Some((namespace, by_name.get(server).copied()?)))
        .filter(|(_, server)| unavailable(&server.state).is_none())
        .flat_map(|(namespace, server)| {
            names
                .tools(server)
                .into_iter()
                .map(move |(method, tool)| Candidate {
                    namespace,
                    method,
                    server,
                    tool,
                })
        })
        .filter_map(|candidate| {
            let score = score(query, &terms, &candidate);
            (query.is_empty() || score > 0).then_some((score, candidate))
        })
        .collect();
    scored.sort_by_key(|(score, _)| Reverse(*score));
    Ok(scored
        .into_iter()
        .skip(offset)
        .take(limit.clamp(MIN_LIMIT, MAX_LIMIT))
        .map(|(_, candidate)| hit(&candidate))
        .collect())
}

/// The configured name of the server `requested` names, or why it cannot be searched.
fn resolve_scope<'a>(
    by_name: &HashMap<&str, &ServerView>,
    names: &'a Names,
    requested: &str,
) -> Result<&'a str, String> {
    let server = names
        .server(requested)
        .or_else(|| names.server(names.namespace(requested)?));
    let Some(server) = server else {
        let known: Vec<&str> = names.namespaces().map(|(namespace, _)| namespace).collect();
        let suggestion = names
            .suggest_namespace(requested)
            .map(|namespace| format!(" Did you mean `{namespace}`?"))
            .unwrap_or_default();
        return Err(format!(
            "No MCP server `{requested}`.{suggestion} Servers: {}.",
            known.join(", ")
        ));
    };
    match by_name
        .get(server)
        .and_then(|view| unavailable(&view.state))
    {
        Some(reason) => Err(format!(
            "MCP server `{requested}` is not connected ({reason}), so it has no tools to search."
        )),
        None => Ok(server),
    }
}

fn terms(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split(|c: char| c.is_whitespace() || TERM_SEPARATORS.contains(&c))
        .filter(|term| !term.is_empty())
        .map(str::to_owned)
        .collect()
}

fn score(query: &str, terms: &[String], candidate: &Candidate) -> usize {
    if query.is_empty() {
        return 0;
    }
    let Candidate {
        namespace,
        method,
        server,
        tool,
    } = *candidate;
    let qualified = |owner: &str, name: &str| {
        query.split_once(QUALIFIER).is_some_and(|(left, right)| {
            left.eq_ignore_ascii_case(owner) && right.eq_ignore_ascii_case(name)
        })
    };
    if qualified(namespace, method)
        || qualified(&server.name, &tool.name)
        || query.eq_ignore_ascii_case(method)
        || query.eq_ignore_ascii_case(&tool.name)
    {
        return EXACT_SCORE;
    }
    let names = [method.to_lowercase(), tool.name.to_lowercase()];
    let title = tool.title.as_deref().unwrap_or_default().to_lowercase();
    let description = tool.description.to_lowercase();
    let owners = [namespace.to_lowercase(), server.name.to_lowercase()];
    terms
        .iter()
        .map(|term| {
            let term = term.as_str();
            weight(NAME_WEIGHT, names.iter().any(|name| name.contains(term)))
                + weight(TITLE_WEIGHT, title.contains(term))
                + weight(DESCRIPTION_WEIGHT, description.contains(term))
                + weight(
                    SERVER_WEIGHT,
                    owners.iter().any(|owner| owner.contains(term)),
                )
        })
        .sum()
}

fn weight(weight: usize, matched: bool) -> usize {
    if matched { weight } else { 0 }
}

fn hit(candidate: &Candidate) -> SearchHit {
    let signature = signature(candidate.method, candidate.tool);
    SearchHit {
        namespace: candidate.namespace.to_owned(),
        method: candidate.method.to_owned(),
        doc: doc(&signature, candidate.tool),
        signature: signature.text,
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::mcp::code::naming::tests::{server, tool};
    use crate::mcp::code::view::ServerState;

    const SERVER: &str = "git-hub";
    const NAMESPACE: &str = "git_hub";
    const ISSUES_DESCRIPTION: &str = "List issues in a repository.";

    fn described(name: &str, description: &str) -> ToolView {
        ToolView {
            description: description.to_owned(),
            ..tool(name)
        }
    }

    fn fixture() -> Vec<ServerView> {
        vec![
            server(
                SERVER,
                vec![
                    described("list_issues", ISSUES_DESCRIPTION),
                    described("issue", "Read one issue."),
                    described("search_code", "Search code across repositories."),
                ],
            ),
            server(
                "linear",
                vec![described(
                    "list_tickets",
                    "List tickets; issues live elsewhere.",
                )],
            ),
            ServerView {
                state: ServerState::NeedsLogin,
                ..server("jira", vec![described("list_issues", ISSUES_DESCRIPTION)])
            },
        ]
    }

    fn found(query: &str, server: Option<&str>) -> Vec<String> {
        let servers = fixture();
        search(&servers, &Names::new(&servers), query, server, MAX_LIMIT, 0)
            .expect("search")
            .into_iter()
            .map(|hit| format!("{}.{}", hit.namespace, hit.method))
            .collect()
    }

    #[test_case("issue", &["git_hub.issue", "git_hub.list_issues", "linear.list_tickets"] ; "exact_method_first")]
    #[test_case("git-hub.issue", &["git_hub.issue", "git_hub.list_issues", "git_hub.search_code", "linear.list_tickets"] ; "exact_permission_key")]
    #[test_case("git_hub.list_issues", &["git_hub.list_issues", "linear.list_tickets", "git_hub.issue", "git_hub.search_code"] ; "exact_qualified_method")]
    #[test_case("code", &["git_hub.search_code"] ; "description_match")]
    #[test_case("tickets", &["linear.list_tickets"] ; "unrelated_tools_are_dropped")]
    fn search_ranks_matches(query: &str, expected: &[&str]) {
        assert_eq!(found(query, None), expected);
    }

    #[test_case(NAMESPACE ; "by_namespace")]
    #[test_case(SERVER ; "by_config_name")]
    fn search_can_be_scoped_to_one_server(scope: &str) {
        assert_eq!(
            found("issue", Some(scope)),
            ["git_hub.issue", "git_hub.list_issues"]
        );
    }

    #[test]
    fn a_blank_query_lists_a_server_in_method_order() {
        assert_eq!(
            found("  ", Some(NAMESPACE)),
            [
                "git_hub.issue",
                "git_hub.list_issues",
                "git_hub.search_code"
            ]
        );
    }

    #[test]
    fn a_blank_query_skips_servers_that_are_not_connected() {
        assert!(found("", None).iter().all(|hit| !hit.starts_with("jira.")));
    }

    #[test_case("gti_hub", "Did you mean `git_hub`?" ; "unknown_with_suggestion")]
    #[test_case("nope", "Servers: git_hub, jira, linear." ; "unknown_lists_servers")]
    #[test_case("jira", "not connected (needs login)" ; "not_connected")]
    fn a_bad_scope_is_refused(scope: &str, expected: &str) {
        let servers = fixture();
        let error = search(
            &servers,
            &Names::new(&servers),
            "",
            Some(scope),
            DEFAULT_LIMIT,
            0,
        )
        .expect_err("refused");
        assert!(error.contains(expected), "{error}");
    }

    #[test_case(0, 2, &["git_hub.issue", "git_hub.list_issues"] ; "first_page")]
    #[test_case(2, 2, &["git_hub.search_code", "linear.list_tickets"] ; "second_page")]
    #[test_case(4, 2, &[] ; "past_the_end")]
    #[test_case(0, 0, &["git_hub.issue"] ; "limit_is_at_least_one")]
    fn pages_slice_the_ranking(offset: usize, limit: usize, expected: &[&str]) {
        let servers = fixture();
        let hits: Vec<String> = search(&servers, &Names::new(&servers), "", None, limit, offset)
            .expect("search")
            .iter()
            .map(|hit| format!("{}.{}", hit.namespace, hit.method))
            .collect();
        assert_eq!(hits, expected);
    }

    #[test]
    fn a_hit_carries_its_signature_and_doc() {
        let servers = fixture();
        let hits =
            search(&servers, &Names::new(&servers), "list_issues", None, 1, 0).expect("search");
        let hit = &hits[0];
        assert_eq!(hit.namespace, NAMESPACE);
        assert_eq!(hit.method, "list_issues");
        assert_eq!(hit.signature, "list_issues() -> Any");
        assert_eq!(hit.doc, ISSUES_DESCRIPTION);
    }
}
