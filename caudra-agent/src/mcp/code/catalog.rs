//! The `# MCP servers` reminder: what the `code` tool can reach, within a token budget.
//!
//! Every server is announced; tool lines share what the budget leaves, round-robin across
//! servers and cheapest first, so one large server cannot crowd out the rest.

use std::collections::{HashMap, VecDeque};

use caudra_providers::{Message, estimate_tokens};

use super::naming::Names;
use super::signature::{one_line, signature, summary};
use super::view::{ServerState, ServerView};
use crate::agent::{last_announced, standing_notice};
use crate::mcp::McpSession;
use crate::prompt::{
    CATALOG_SLOT, MCP_SERVERS_MARKER, MCP_SERVERS_PROMPT, MCP_SERVERS_WITHDRAWN_PROMPT,
};

pub const CATALOG_BUDGET_TOKENS: usize = 2_000;

const USAGE: &str = "Call these with the `code` tool: `await <server>.<tool>(...)`.";
const NO_SERVERS: &str = "No MCP server is enabled.";
const COMPLETE: &str = "This list is complete: do not guess other names.";
const PARTIAL: &str = "Some tools are not listed: find them with `search(\"query\")`, which returns exact signatures.";
const CONNECTED: &str = "connected";
const CONNECTING: &str = "connecting";
const AWAITING_TRUST: &str = "awaiting trust";
const NEEDS_LOGIN: &str = "needs login";
const FAILED: &str = "failed";
const RESOURCES: &str = "resources";
const TOOL: &str = "tool";
const TOOLS: &str = "tools";
const MAX_FAILURE_CHARS: usize = 120;
/// Appended when the session has a decision engine, which the helpers need.
const DECISIONS_SECTION: &str = "\n\n## Decision helpers\n\
Use these in a `code` snippet on what it gathered, to keep only what matters before it reaches you or to branch on a judgement without another round trip. They change nothing, and their answers are probabilities.\n\
- `await decide(input, questions)`: answers every question about `input` in one request. Questions follow the OpenAI Decisions shape: `type`, `name`, `instructions`, as a `predicate`, a `choice` with `choices`, or a `score` with 2 to 10 `levels`, low to high. Read `r[\"by_name\"][name]`.\n\
- `await rerank(query, candidates, threshold=None, top_k=None)`: `[{\"index\", \"score\"}]`, best first, for up to 256 strings or small dicts. `score` is the probability of relevance.\n\
For good answers:\n\
- Make `input` a dict under 1,500 bytes of only the judged fields, and name them in the questions. `instructions` can be `{\"question\": ..., \"focus\": \"what counts and what does not\"}`.\n\
- Describe each choice or level by what sets it apart.\n\
- Write `query` as the need, not keywords, and pass short labels as candidates. `index` maps back to the full item.\n\
- Act on about 0.85 or more; below 0.6 is a guess.\n\
- `decide()` judges `input` as one item: call it once per item. A local engine queues overlapping calls, and each waits only `timeout_ms` (800 by default), so raise it to `gather` many.\n\
Examples: `await decide({\"body\": b}, [{\"type\": \"score\", \"name\": \"severity\", \"instructions\": \"How severe is the bug `body` reports?\", \"levels\": [\"cosmetic\", {\"label\": \"blocking\", \"description\": \"no workaround\"}]}])`, `await rerank(\"issues reporting a crash at startup\", titles, top_k=10)`.";

/// What keeps a server's tools out of reach, or `None` when it is connected.
pub fn unavailable(state: &ServerState) -> Option<String> {
    match state {
        ServerState::Connected => None,
        ServerState::Connecting => Some(CONNECTING.to_owned()),
        ServerState::AwaitingTrust => Some(AWAITING_TRUST.to_owned()),
        ServerState::NeedsLogin => Some(NEEDS_LOGIN.to_owned()),
        ServerState::Failed(message) => Some(format!(
            "{FAILED}: {}",
            one_line(message, MAX_FAILURE_CHARS)
        )),
    }
}

/// The `# MCP servers` block a turn should carry, `None` when the transcript already states it.
/// `session` is what the agent reaches through the `code` tool, `None` when it cannot reach the
/// tool; with nothing to list, a list still in force is withdrawn. `decisions` adds the decision
/// helpers, which only a session with a decision engine has, and states the block even when no
/// server is listed.
pub fn notice(
    history: &[Message],
    session: Option<&McpSession>,
    decisions: bool,
) -> Option<Message> {
    let catalog = session
        .filter(|session| session.has_enabled_server())
        .map(|session| {
            let servers = session.servers();
            render(&servers, &Names::new(&servers))
        })
        .filter(|catalog| !catalog.is_empty());
    let listed = (catalog.is_some() || decisions).then(|| {
        let mut body = catalog.unwrap_or_else(|| NO_SERVERS.to_owned());
        if decisions {
            body.push_str(DECISIONS_SECTION);
        }
        MCP_SERVERS_PROMPT.replace(CATALOG_SLOT, &body)
    });
    let withdrawn =
        || last_announced(history, &[MCP_SERVERS_MARKER]).map(|_| MCP_SERVERS_WITHDRAWN_PROMPT);
    standing_notice(
        history,
        MCP_SERVERS_MARKER,
        listed.as_deref().or_else(withdrawn),
    )
}

/// The reminder body, or an empty string when no server is configured.
pub fn render(servers: &[ServerView], names: &Names) -> String {
    let by_name: HashMap<&str, &ServerView> = servers
        .iter()
        .map(|server| (server.name.as_str(), server))
        .collect();
    let mut sections: Vec<Section> = names
        .namespaces()
        .filter_map(|(namespace, server)| {
            Some(Section::new(
                namespace,
                by_name.get(server).copied()?,
                names,
            ))
        })
        .collect();
    if sections.is_empty() {
        return String::new();
    }
    let reserved = tokens(&intro(PARTIAL))
        + sections
            .iter()
            .map(|section| tokens(&section.preamble(Some(section.total()))))
            .sum::<usize>();
    fill(
        &mut sections,
        CATALOG_BUDGET_TOKENS.saturating_sub(reserved),
    );
    let complete = sections
        .iter()
        .all(|section| section.listed() == section.total());
    let mut catalog = intro(if complete { COMPLETE } else { PARTIAL });
    for section in &mut sections {
        let shown = (section.listed() < section.total()).then(|| section.listed());
        catalog.push_str(&section.preamble(shown));
        section.chosen.sort_unstable_by_key(|line| line.method);
        for line in &section.chosen {
            catalog.push_str(&line.text);
        }
    }
    catalog.truncate(catalog.trim_end().len());
    catalog
}

struct Line<'a> {
    method: &'a str,
    /// Ends with a newline, so its cost is what it adds to the catalog.
    text: String,
    tokens: usize,
}

struct Section<'a> {
    namespace: &'a str,
    server: &'a ServerView,
    /// Cheapest first.
    pending: VecDeque<Line<'a>>,
    chosen: Vec<Line<'a>>,
}

impl<'a> Section<'a> {
    fn new(namespace: &'a str, server: &'a ServerView, names: &'a Names) -> Self {
        let mut pending: Vec<Line> = if server.state == ServerState::Connected {
            names
                .tools(server)
                .into_iter()
                .filter(|(_, tool)| !tool.search_only)
                .map(|(method, tool)| {
                    let signature = signature(method, tool);
                    let summary = summary(tool);
                    let text = if summary.is_empty() {
                        format!("- {}\n", signature.text)
                    } else {
                        format!("- {}: {summary}\n", signature.text)
                    };
                    Line {
                        method,
                        tokens: tokens(&text),
                        text,
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        pending.sort_unstable_by(|left, right| {
            (left.tokens, left.method).cmp(&(right.tokens, right.method))
        });
        Self {
            namespace,
            server,
            pending: pending.into(),
            chosen: Vec::new(),
        }
    }

    fn total(&self) -> usize {
        self.server.tools.len()
    }

    fn listed(&self) -> usize {
        self.chosen.len()
    }

    /// The header and instructions. `shown` is how many tools are listed when not all of them are.
    fn preamble(&self, shown: Option<usize>) -> String {
        let title = self
            .server
            .title
            .as_deref()
            .filter(|title| *title != self.namespace)
            .map(|title| format!(": {title}"))
            .unwrap_or_default();
        let status = unavailable(&self.server.state).unwrap_or_else(|| {
            let total = self.total();
            let noun = if total == 1 { TOOL } else { TOOLS };
            let count = match shown {
                Some(shown) => format!("showing {shown} of {total} {noun}"),
                None => format!("{total} {noun}"),
            };
            let resources = if self.server.resources {
                format!(", {RESOURCES}")
            } else {
                String::new()
            };
            format!("{CONNECTED}, {count}{resources}")
        });
        let mut preamble = format!("## {}{title} ({status})\n", self.namespace);
        if let Some(instructions) = self
            .server
            .instructions
            .as_deref()
            .map(str::trim)
            .filter(|instructions| !instructions.is_empty())
        {
            preamble.push_str(instructions);
            preamble.push('\n');
        }
        preamble
    }
}

fn intro(completeness: &str) -> String {
    format!("{USAGE} {completeness}\n\n")
}

/// Hands out `budget` one line per server per round, until each server's next line no longer fits.
fn fill(sections: &mut [Section], mut budget: usize) {
    let mut open = true;
    while open {
        open = false;
        for section in sections.iter_mut() {
            let Some(next) = section.pending.front() else {
                continue;
            };
            if next.tokens > budget {
                section.pending.clear();
                continue;
            }
            budget -= next.tokens;
            section.chosen.extend(section.pending.pop_front());
            open = true;
        }
    }
}

fn tokens(text: &str) -> usize {
    estimate_tokens(text) as usize
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::mcp::code::naming::tests::{server, tool};
    use crate::mcp::code::view::ToolView;

    const INSTRUCTIONS: &str = "Prefer list_issues over search.";
    const STUB_TOOL: &str = "stub.list_issues";
    const OTHER_STUB_TOOL: &str = "stub.close_issue";
    const LISTED: &str = "a list the transcript lacks must be announced";
    const REPEATED: &str = "a list the transcript already states must not repeat";
    const WITHDRAWN: &str = "an announced list must be withdrawn once nothing is left";
    const LONG_DESCRIPTION: &str = "Does one specific thing to a resource in the remote service.";

    fn described(name: &str) -> ToolView {
        ToolView {
            description: LONG_DESCRIPTION.to_owned(),
            input_schema: json!({"properties": {"owner": {"type": "string"}, "repo": {"type": "string"}}}),
            ..tool(name)
        }
    }

    fn many_tools(prefix: &str, count: usize) -> Vec<ToolView> {
        (0..count)
            .map(|index| described(&format!("{prefix}_{index}")))
            .collect()
    }

    fn catalog(servers: &[ServerView]) -> String {
        render(servers, &Names::new(servers))
    }

    #[test]
    fn no_servers_render_nothing() {
        assert_eq!(catalog(&[]), "");
    }

    #[test]
    fn a_small_catalog_is_complete() {
        let github = ServerView {
            title: Some("GitHub".to_owned()),
            instructions: Some(INSTRUCTIONS.to_owned()),
            resources: true,
            ..server("github", vec![described("list_issues")])
        };
        let broken = ServerView {
            state: ServerState::Failed("connection\nrefused".to_owned()),
            ..server("broken", Vec::new())
        };
        let linear = ServerView {
            state: ServerState::NeedsLogin,
            ..server("linear", Vec::new())
        };
        let expected = format!(
            "{USAGE} {COMPLETE}\n\n\
             ## broken (failed: connection refused)\n\
             ## github: GitHub (connected, 1 tool, resources)\n\
             {INSTRUCTIONS}\n\
             - list_issues(*, owner: str | None = None, repo: str | None = None) -> Any: {LONG_DESCRIPTION}\n\
             ## linear (needs login)"
        );
        assert_eq!(catalog(&[linear, github, broken]), expected);
    }

    #[test]
    fn the_catalog_ignores_input_order() {
        let mut servers = vec![
            server("alpha", many_tools("a", 40)),
            server("beta", many_tools("b", 40)),
            server("gamma", many_tools("c", 3)),
        ];
        let forward = catalog(&servers);
        servers.reverse();
        servers.iter_mut().for_each(|server| server.tools.reverse());
        assert_eq!(catalog(&servers), forward);
    }

    #[test]
    fn a_catalog_over_budget_lists_what_fits_and_says_so() {
        let servers = [
            ServerView {
                instructions: Some(INSTRUCTIONS.to_owned()),
                ..server("alpha", many_tools("a", 100))
            },
            server("beta", many_tools("b", 100)),
        ];
        let catalog = catalog(&servers);
        assert!(catalog.contains(PARTIAL), "{catalog}");
        assert!(catalog.contains(INSTRUCTIONS), "{catalog}");
        assert!(
            catalog.contains("## alpha (connected, showing "),
            "{catalog}"
        );
        assert!(
            catalog.contains("## beta (connected, showing "),
            "{catalog}"
        );
        assert!(tokens(&catalog) <= CATALOG_BUDGET_TOKENS, "{catalog}");
        let alpha = catalog.matches("- a_").count();
        let beta = catalog.matches("- b_").count();
        assert!(alpha > 0 && alpha.abs_diff(beta) <= 1, "{alpha} vs {beta}");
    }

    #[test]
    fn headers_survive_when_no_tool_fits() {
        let instructions = "word ".repeat(CATALOG_BUDGET_TOKENS * 2);
        let servers = [ServerView {
            instructions: Some(instructions),
            ..server("alpha", many_tools("a", 2))
        }];
        let catalog = catalog(&servers);
        assert!(
            catalog.contains("## alpha (connected, showing 0 of 2 tools)"),
            "{catalog}"
        );
        assert!(!catalog.contains("- a_"), "{catalog}");
    }

    #[test_case(true, PARTIAL, "showing 1 of 2 tools" ; "search_only_is_never_listed")]
    #[test_case(false, COMPLETE, "2 tools" ; "listed_tools_are_complete")]
    fn search_only_tools_stay_out_of_the_list(search_only: bool, sentence: &str, count: &str) {
        let hidden = ToolView {
            search_only,
            ..described("hidden")
        };
        let catalog = catalog(&[server("alpha", vec![described("shown"), hidden])]);
        assert!(catalog.contains(sentence), "{catalog}");
        assert!(
            catalog.contains(&format!("## alpha (connected, {count})")),
            "{catalog}"
        );
        assert_eq!(catalog.contains("- hidden("), !search_only, "{catalog}");
    }

    #[test]
    fn listed_tools_print_in_method_order() {
        let tools = vec![
            described("zeta"),
            described("alpha_with_a_much_longer_name"),
        ];
        let catalog = catalog(&[server("srv", tools)]);
        let alpha = catalog
            .find("- alpha_with_a_much_longer_name(")
            .expect("alpha listed");
        let zeta = catalog.find("- zeta(").expect("zeta listed");
        assert!(alpha < zeta, "{catalog}");
    }

    #[test]
    fn nothing_is_said_without_a_server() {
        assert!(notice(&[], None, false).is_none());
    }

    #[test]
    fn a_list_is_stated_once_and_restated_when_it_changes() {
        let session = crate::mcp::stub_session(&[(STUB_TOOL, LONG_DESCRIPTION)]);
        let listed = notice(&[], Some(&session), false).expect(LISTED);
        let text = listed.user_text().unwrap();
        assert!(text.contains(MCP_SERVERS_MARKER), "{text}");
        assert!(text.contains("- list_issues("), "{text}");
        let history = [listed];
        assert!(
            notice(&history, Some(&session), false).is_none(),
            "{REPEATED}"
        );

        let changed = crate::mcp::stub_session(&[(OTHER_STUB_TOOL, LONG_DESCRIPTION)]);
        let restated = notice(&history, Some(&changed), false).expect(LISTED);
        assert!(restated.user_text().unwrap().contains("- close_issue("));
    }

    #[test_case(true ; "with_an_engine")]
    #[test_case(false ; "without_an_engine")]
    fn decision_helpers_are_listed_only_with_an_engine(decisions: bool) {
        let session = crate::mcp::stub_session(&[(STUB_TOOL, LONG_DESCRIPTION)]);
        let listed = notice(&[], Some(&session), decisions).expect(LISTED);
        let text = listed.user_text().unwrap();
        assert_eq!(text.contains(DECISIONS_SECTION), decisions, "{text}");
    }

    #[test]
    fn decision_helpers_are_stated_without_a_server() {
        let expected =
            MCP_SERVERS_PROMPT.replace(CATALOG_SLOT, &format!("{NO_SERVERS}{DECISIONS_SECTION}"));
        let listed = notice(&[], None, true).expect(LISTED);
        assert_eq!(listed.user_text(), Some(expected.as_str()));
        let history = [listed];
        assert!(notice(&history, None, true).is_none(), "{REPEATED}");
    }

    #[test]
    fn decision_helpers_stay_within_their_budget() {
        const DECISIONS_SECTION_TOKENS: usize = 420;
        const OVER_BUDGET: &str = "the decision helpers outgrew their share of the reminder";
        let tokens = estimate_tokens(DECISIONS_SECTION) as usize;
        assert!(
            tokens <= DECISIONS_SECTION_TOKENS,
            "{OVER_BUDGET}: {tokens}"
        );
    }

    #[test]
    fn an_announced_list_is_withdrawn_once() {
        let session = crate::mcp::stub_session(&[(STUB_TOOL, LONG_DESCRIPTION)]);
        let history = [notice(&[], Some(&session), false).expect(LISTED)];
        let withdrawal = notice(&history, None, false).expect(WITHDRAWN);
        assert_eq!(withdrawal.user_text(), Some(MCP_SERVERS_WITHDRAWN_PROMPT));

        let history = [history[0].clone(), withdrawal];
        assert!(notice(&history, None, false).is_none(), "{REPEATED}");
    }
}
