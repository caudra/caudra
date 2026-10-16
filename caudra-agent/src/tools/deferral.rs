//! Built-in tools held out of the request array until the model asks for them.
//!
//! What is not declared is listed by name inside a single `tool_search`
//! catalog, and a search moves it into the array from the next request on.
//! The deferred set is the fixed list in
//! [`caudra_config::DEFERRED_BUILTIN_TOOLS`]: a built-in is deferred when most
//! sessions never call it. MCP tools never pass through here; the `code` tool
//! reaches them without declaring any.
//!
//! Deferring costs a prompt-cache prefix each time something loads, so tools
//! that are one mode of work carry a group and load together. The code graph
//! is five tools and one decision.
//!
//! That prefix is also why deferral is per model rather than global. A small
//! model gains from the shorter array and rebuilds its prefix cheaply; a known
//! non-small one would spend a large prefix to load what it was going to reach
//! for anyway. [`BuiltinDeferral`] is that decision, taken once per definitions
//! build and carried as a value, because reading it is a `providers.toml` parse
//! ([`Model::class_of`]) and the tool report asks per registry entry.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};

use caudra_config::{AgentConfig, DeferBuiltinTools};
use caudra_providers::{ContentBlock, Message, Model, ModelPurpose};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::CancelToken;
use crate::decisions::{
    DecisionContext, DecisionFeature, Decisions, MAX_RERANK_BATCH, NoteDraft, NoteOutcome,
    Relevance, Rerank, Reranked, candidate_text, placement, score_line,
};

pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";

const NAME_HIT_SCORE: usize = 2;
const DESCRIPTION_HIT_SCORE: usize = 1;
pub(crate) const SEARCH_EMPTY_QUERY: &str = "query must not be empty";
const SEARCH_NO_MATCH: &str = "No deferred tools matched";
const BUILTIN_HEADING: &str = "Available to load:";
const MIN_RANKED_CANDIDATES: usize = 2;
const MAX_SEARCH_NAME_BYTES: usize = 160;
const MAX_SEARCH_SUMMARY_BYTES: usize = 320;
const MAX_SEARCH_QUERY_BYTES: usize = 512;
const QUERY_KEY: &str = "query";
const NAME_KEY: &str = "name";
const SUMMARY_KEY: &str = "summary";
const KEPT_LEXICAL_ORDER: &str = "kept the keyword order";
const TOOL_LIST_SEPARATOR: &str = ", ";
const RELEVANCE: Relevance = Relevance {
    statement: "The tool is suited to the query. This only orders catalog loading; it grants no execution permission.",
    relevant: "The tool's name or summary shows it does what the query asks for.",
    unrelated: "The tool only shares words with the query, or does something else.",
};

/// The lexical head of a tool search reordered by relevance: candidates at
/// or above the relevance floor first, most relevant first, then the rest in
/// lexical order. Nothing is dropped, so a search loads only what its
/// keywords matched. A shadow ranking is only reported, never applied.
pub(crate) struct ToolSearchRanking {
    names: Vec<String>,
    order: Vec<usize>,
    relevant: usize,
    outcome: NoteOutcome,
    reranked: Reranked,
}

impl ToolSearchRanking {
    /// The candidate judged most relevant, when any reached the floor.
    pub(crate) fn top(&self) -> Option<usize> {
        self.order.first().copied().filter(|_| self.relevant > 0)
    }

    pub(crate) fn applied(&self) -> bool {
        self.outcome == NoteOutcome::Acted
    }

    /// When the ranked order loads something other than the `lexical` one
    /// would have, records `Rerouted` and returns the row: what `ranked`
    /// loaded instead, or in shadow would have, and every match's score and
    /// position.
    pub(crate) fn settle(self, ranked: &[Arc<str>], lexical: &[Arc<str>]) -> Option<NoteDraft> {
        if !rerouted(ranked, lexical) {
            return None;
        }
        self.reranked.record_rerouted(&self.order, self.order.len());
        let mut summary = format!("loaded {}", tool_list(ranked));
        if !lexical.is_empty() {
            summary.push_str(&format!(" instead of {}", tool_list(lexical)));
        }
        let scores = self.reranked.scores();
        let details = self
            .order
            .iter()
            .enumerate()
            .map(|(to, from)| score_line(scores[*from], &self.names[*from], &placement(*from, to)))
            .collect();
        Some(NoteDraft::new(
            self.reranked.decisions(),
            DecisionFeature::ToolSearch,
            self.outcome,
            &summary,
            details,
        ))
    }
}

/// One relevance question per candidate over the first [`MAX_RERANK_BATCH`]
/// lexical matches. Without a ranking the lexical order stands: too few
/// candidates, the feature off, or no usable answer, with the row for a
/// fresh engine failure.
pub(crate) async fn rank_tool_search<'a>(
    query: &str,
    candidates: impl Iterator<Item = (&'a str, &'a Value)>,
    decisions: Option<&Decisions>,
    context: &DecisionContext,
) -> Result<ToolSearchRanking, Option<NoteDraft>> {
    let feature = DecisionFeature::ToolSearch;
    let decisions = decisions
        .filter(|service| service.enabled(&feature))
        .ok_or(None)?;
    let floor = feature
        .rerank_threshold(&decisions.config().thresholds)
        .ok_or(None)?;
    let (names, candidates): (Vec<String>, Vec<Value>) = candidates
        .take(MAX_RERANK_BATCH)
        .map(|(name, definition)| {
            let candidate = json!({
                NAME_KEY: candidate_text(name, MAX_SEARCH_NAME_BYTES),
                SUMMARY_KEY: candidate_text(
                    definition["description"].as_str().unwrap_or_default(),
                    MAX_SEARCH_SUMMARY_BYTES,
                ),
            });
            (name.to_owned(), candidate)
        })
        .unzip();
    if candidates.len() < MIN_RANKED_CANDIDATES {
        return Err(None);
    }
    let (reranked, outcome) = decisions
        .rerank(
            Rerank {
                feature: feature.clone(),
                relevance: &RELEVANCE,
                query: &json!({QUERY_KEY: candidate_text(query, MAX_SEARCH_QUERY_BYTES)}),
                candidates: &candidates,
            },
            context,
            None,
            &CancelToken::none(),
        )
        .await
        .scored()
        .map_err(|failure| {
            failure.map(|failure| {
                NoteDraft::fell_back(decisions, feature, &failure, KEPT_LEXICAL_ORDER)
            })
        })?;
    let (mut order, below) = reranked.split_at_floor(floor);
    let relevant = order.len();
    order.extend(below);
    Ok(ToolSearchRanking {
        names,
        order,
        relevant,
        outcome,
        reranked,
    })
}

/// Whether a search loaded something other than the `lexical` order would
/// have.
fn rerouted(loaded: &[Arc<str>], lexical: &[Arc<str>]) -> bool {
    !loaded.is_empty()
        && (loaded.len() != lexical.len() || loaded.iter().any(|name| !lexical.contains(name)))
}

fn tool_list(names: &[Arc<str>]) -> String {
    let names: Vec<String> = names
        .iter()
        .map(|name| candidate_text(name, MAX_SEARCH_NAME_BYTES))
        .collect();
    names.join(TOOL_LIST_SEPARATOR)
}

/// Every tool the transcript shows being called, for reseeding a resumed
/// session: a tool that was loaded and used stays declared across a restart.
/// A pure name scan, so names that are not deferred are inert.
pub fn loaded_tool_names(history: &[Message]) -> impl Iterator<Item = Arc<str>> + '_ {
    history
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolUse { name, .. } => Some(Arc::from(name.as_str())),
            _ => None,
        })
}

/// Whether this run holds the deferrable built-ins back, and what decided it.
/// The eager variants are distinct so a report can say which, since one is the
/// user's setting and the other is a fact about the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinDeferral {
    Lazy,
    EagerByClass,
    EagerByConfig,
}

impl BuiltinDeferral {
    pub fn resolve(config: &AgentConfig, model: &Model) -> Self {
        Self::from_class(
            config.defer_builtin_tools,
            Model::class_of(&model.provider, &model.id),
        )
    }

    /// Split from [`Self::resolve`] so the rule is decided without reading
    /// `providers.toml`, which is what a class lookup costs.
    fn from_class(setting: DeferBuiltinTools, class: Option<ModelPurpose>) -> Self {
        match (setting, class) {
            (DeferBuiltinTools::Never, _) => Self::EagerByConfig,
            (DeferBuiltinTools::Always, _) => Self::Lazy,
            (DeferBuiltinTools::Auto, Some(ModelPurpose::Best)) => Self::EagerByClass,
            (DeferBuiltinTools::Auto, _) => Self::Lazy,
        }
    }

    pub fn is_lazy(self) -> bool {
        matches!(self, Self::Lazy)
    }
}

/// Which built-ins this run withholds. Naming a tool in `allowed_tools` is a
/// request for it upfront, so an explicit allow list opts that tool out of
/// deferral rather than fighting it.
pub fn deferred_names(allowed_tools: &[String], deferral: BuiltinDeferral) -> Vec<&'static str> {
    caudra_config::DEFERRED_BUILTIN_TOOLS
        .iter()
        .map(|deferred| deferred.name)
        .filter(|name| is_deferred(name, allowed_tools, deferral))
        .collect()
}

pub fn is_deferred(name: &str, allowed_tools: &[String], deferral: BuiltinDeferral) -> bool {
    deferral.is_lazy()
        && caudra_config::is_deferred_builtin(name)
        && !allowed_tools.iter().any(|allowed| allowed == name)
}

/// One deferred definition and the text a query is matched against.
#[derive(Clone, Debug)]
pub struct DeferredTool {
    pub name: Arc<str>,
    /// Tools sharing a group load together, so one search buys the whole
    /// mode of work and one cache miss instead of one per tool.
    pub group: Option<&'static str>,
    pub definition: Value,
    haystack: String,
}

impl DeferredTool {
    pub fn new(name: &str, group: Option<&'static str>, definition: Value) -> Self {
        let haystack = haystack(&definition);
        Self {
            name: Arc::from(name),
            group,
            definition,
            haystack,
        }
    }

    /// Members of the same group, plus the tool itself. An ungrouped tool
    /// loads alone.
    fn loads_with(&self, other: &Self) -> bool {
        self.name == other.name || self.group.is_some_and(|group| other.group == Some(group))
    }
}

/// The deferred catalog for one session. Cloning shares the loaded set, so a
/// tool loaded inside a batch child is loaded for the agent that dispatched it.
#[derive(Clone, Default)]
pub struct DeferralSession {
    deferred: Arc<Vec<DeferredTool>>,
    loaded: Arc<Mutex<HashSet<Arc<str>>>>,
}

/// What one search moved into the request array, and the row for what the
/// decision engine did to it.
#[derive(Debug)]
pub struct SearchOutcome {
    pub loaded: Vec<Arc<str>>,
    pub message: String,
    pub note: Option<NoteDraft>,
}

impl SearchOutcome {
    #[cfg(test)]
    pub(crate) fn rerouted_from(&self, baseline: &[Arc<str>]) -> bool {
        rerouted(&self.loaded, baseline)
    }
}

pub struct PreparedDeferredSearch<'a> {
    session: &'a DeferralSession,
    query: &'a str,
    ranking: Result<ToolSearchRanking, Option<NoteDraft>>,
}

impl PreparedDeferredSearch<'_> {
    pub fn commit(self) -> Result<SearchOutcome, String> {
        let matches = self.session.search_matches(self.query)?;
        Ok(match self.ranking {
            Ok(ranking) => self
                .session
                .load_search_match(self.query, &matches, Some(ranking)),
            Err(note) => SearchOutcome {
                note,
                ..self.session.load_search_match(self.query, &matches, None)
            },
        })
    }
}

impl DeferralSession {
    /// `history_names` seeds the loaded set: a restored session keeps the
    /// tools it had already searched for, instead of making the model find
    /// them again.
    pub fn new(deferred: Vec<DeferredTool>, history_names: impl Iterator<Item = Arc<str>>) -> Self {
        let known: HashSet<&str> = deferred.iter().map(|tool| tool.name.as_ref()).collect();
        let loaded = history_names
            .filter(|name| known.contains(name.as_ref()))
            .collect();
        Self {
            deferred: Arc::new(deferred),
            loaded: Arc::new(Mutex::new(loaded)),
        }
    }

    /// A view over the same catalog with no loads, for a subagent: what the
    /// parent searched for says nothing about what the child needs.
    pub fn fresh(&self) -> Self {
        Self {
            deferred: Arc::clone(&self.deferred),
            loaded: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.deferred.is_empty()
    }

    /// Every deferred definition, loaded or not, so a report can size what a
    /// load would cost before the model spends it.
    pub fn definitions(&self) -> &[DeferredTool] {
        &self.deferred
    }

    pub fn loaded_names(&self) -> Vec<Arc<str>> {
        self.lock_loaded().iter().cloned().collect()
    }

    pub fn accounting_definitions(&self, declared: &Value) -> Value {
        let mut definitions = declared.as_array().cloned().unwrap_or_default();
        definitions.extend(self.deferred.iter().map(|tool| tool.definition.clone()));
        Value::Array(definitions)
    }

    pub fn filtered(&self, eligible: impl Fn(&str) -> bool) -> Self {
        Self {
            deferred: Arc::new(
                self.deferred
                    .iter()
                    .filter(|tool| eligible(&tool.name))
                    .cloned()
                    .collect(),
            ),
            loaded: Arc::clone(&self.loaded),
        }
    }

    /// Snapshot for one request, taken while the loaded set is stable.
    pub fn request_snapshot(&self) -> DeferralSnapshot {
        DeferralSnapshot {
            deferred: Arc::clone(&self.deferred),
            loaded: self.lock_loaded().clone(),
        }
    }

    /// Called when the model invokes a deferred tool by its catalog name
    /// instead of searching: a lucky guess keeps its definition from the next
    /// request on. Returns what that load added, empty when nothing changed.
    pub fn mark_loaded(&self, name: &str) -> Vec<Arc<str>> {
        let Some(hit) = self.deferred.iter().find(|tool| tool.name.as_ref() == name) else {
            return Vec::new();
        };
        self.load_group(&mut self.lock_loaded(), hit)
    }

    /// Ranks the deferred tools against `query` and loads every member of the
    /// best match's group. Exact names win over keyword hits, and a name hit
    /// outranks a description hit.
    pub fn search(&self, query: &str) -> Result<SearchOutcome, String> {
        let matches = self.search_matches(query)?;
        Ok(self.load_search_match(query, &matches, None))
    }

    pub fn has_exact_match(&self, query: &str) -> bool {
        self.deferred
            .iter()
            .any(|tool| tool.name.eq_ignore_ascii_case(query.trim()))
    }

    pub async fn prepare_search_with_decisions<'a>(
        &'a self,
        query: &'a str,
        decisions: Option<&Decisions>,
        context: &DecisionContext,
    ) -> Result<PreparedDeferredSearch<'a>, String> {
        let matches = self.search_matches(query)?;
        let ranking = if self.has_exact_match(query) {
            Err(None)
        } else {
            rank_tool_search(
                query,
                matches
                    .iter()
                    .map(|(_, _, tool)| (tool.name.as_ref(), &tool.definition)),
                decisions,
                context,
            )
            .await
        };
        Ok(PreparedDeferredSearch {
            session: self,
            query,
            ranking,
        })
    }

    fn search_matches(&self, query: &str) -> Result<Vec<(bool, usize, &DeferredTool)>, String> {
        let q = query.trim().to_lowercase();
        let tokens: Vec<&str> = q
            .split(|c: char| !c.is_alphanumeric())
            .filter(|token| !token.is_empty())
            .collect();
        if tokens.is_empty() {
            return Err(SEARCH_EMPTY_QUERY.into());
        }

        let mut matches: Vec<(bool, usize, &DeferredTool)> = self
            .deferred
            .iter()
            .filter_map(|tool| {
                let name = tool.name.to_lowercase();
                let score: usize = tokens
                    .iter()
                    .map(|token| {
                        if name.contains(token) {
                            NAME_HIT_SCORE
                        } else if tool.haystack.contains(token) {
                            DESCRIPTION_HIT_SCORE
                        } else {
                            0
                        }
                    })
                    .sum();
                let exact = name == q;
                (exact || score > 0).then_some((exact, score, tool))
            })
            .collect();
        matches.sort_by(|a, b| {
            (b.0, b.1)
                .cmp(&(a.0, a.1))
                .then_with(|| a.2.name.cmp(&b.2.name))
        });
        Ok(matches)
    }

    fn load_search_match(
        &self,
        query: &str,
        matches: &[(bool, usize, &DeferredTool)],
        ranking: Option<ToolSearchRanking>,
    ) -> SearchOutcome {
        let mut guard = self.lock_loaded();
        let unloaded_group = |index: usize, loaded: &HashSet<Arc<str>>| -> Vec<Arc<str>> {
            matches
                .get(index)
                .into_iter()
                .flat_map(|(_, _, hit)| {
                    self.deferred
                        .iter()
                        .filter(move |tool| hit.loads_with(tool))
                })
                .filter(|tool| !loaded.contains(&tool.name))
                .map(|tool| Arc::clone(&tool.name))
                .collect()
        };
        let top = ranking
            .as_ref()
            .and_then(ToolSearchRanking::top)
            .unwrap_or_default();
        let compared = ranking
            .as_ref()
            .map(|_| (unloaded_group(top, &guard), unloaded_group(0, &guard)));
        let chosen = match ranking.as_ref().is_some_and(ToolSearchRanking::applied) {
            true => top,
            false => 0,
        };
        let loaded: Vec<Arc<str>> = matches
            .get(chosen)
            .map(|(_, _, hit)| self.load_group(&mut guard, hit))
            .unwrap_or_default();
        drop(guard);

        info!(loaded = loaded.len(), "built-in tool search");
        let note = ranking
            .zip(compared)
            .and_then(|(ranking, (ranked, lexical))| ranking.settle(&ranked, &lexical));
        SearchOutcome {
            message: describe(&loaded, query),
            loaded,
            note,
        }
    }

    /// Loading is per group, so the returned list is what the array gains,
    /// never a tool that was already declared.
    fn load_group(&self, loaded: &mut HashSet<Arc<str>>, hit: &DeferredTool) -> Vec<Arc<str>> {
        self.deferred
            .iter()
            .filter(|tool| hit.loads_with(tool))
            .filter(|tool| loaded.insert(Arc::clone(&tool.name)))
            .map(|tool| Arc::clone(&tool.name))
            .collect()
    }

    fn lock_loaded(&self) -> MutexGuard<'_, HashSet<Arc<str>>> {
        self.loaded.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One request's view: which deferred definitions are declared in full and
/// what the catalog still advertises.
pub struct DeferralSnapshot {
    deferred: Arc<Vec<DeferredTool>>,
    loaded: HashSet<Arc<str>>,
}

impl DeferralSnapshot {
    /// Appends the loaded definitions, then one catalog entry naming whatever
    /// is left. Appending keeps the base array a prefix of the result, which
    /// is what token accounting attributes against.
    pub fn extend_tools(&self, tools: &mut Value) {
        let section = self.extend_declared(tools);
        push_catalog(tools, section.as_slice());
    }

    /// Appends the loaded definitions and returns this source's slice of the
    /// catalog. Declaring and cataloguing are separate because the request
    /// carries one `tool_search` covering every source, and whichever source
    /// ran first used to claim the name and silence the rest.
    pub fn extend_declared(&self, tools: &mut Value) -> Option<String> {
        let Some(array) = tools.as_array_mut() else {
            debug_assert!(false, "tools must be a JSON array");
            return None;
        };
        let declared: HashSet<String> = array
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
            .collect();
        let (loaded, pending): (Vec<&DeferredTool>, Vec<&DeferredTool>) = self
            .deferred
            .iter()
            .filter(|tool| !declared.contains(tool.name.as_ref()))
            .partition(|tool| self.loaded.contains(&tool.name));

        array.extend(loaded.iter().map(|tool| tool.definition.clone()));
        (!pending.is_empty()).then(|| format!("{BUILTIN_HEADING}{}", catalog_listing(&pending)))
    }

    pub fn pending_names(&self) -> Vec<&str> {
        self.deferred
            .iter()
            .filter(|tool| !self.loaded.contains(&tool.name))
            .map(|tool| tool.name.as_ref())
            .collect()
    }
}

/// The one entry that stands in for every deferred built-in.
///
/// A `tool_search` the caller declared itself wins: a plugin owning that name
/// is a real conflict.
pub fn push_catalog(tools: &mut Value, sections: &[String]) {
    let Some(array) = tools.as_array_mut() else {
        debug_assert!(false, "tools must be a JSON array");
        return;
    };
    if sections.is_empty() {
        return;
    }
    if array
        .iter()
        .any(|tool| tool["name"] == TOOL_SEARCH_TOOL_NAME)
    {
        warn!(
            sections = sections.len(),
            "a tool named {TOOL_SEARCH_TOOL_NAME} already exists; deferred tools stay hidden"
        );
        return;
    }
    array.push(catalog_definition(&sections.join("\n")));
}

pub fn push_unbound_catalog(tools: &mut Value, sections: &[String], bound: bool) {
    if bound {
        if !sections.is_empty() {
            warn!(
                "deferred tools have no discovery route: tool_search is owned by a registered or local binding"
            );
        }
    } else {
        push_catalog(tools, sections);
    }
}

fn catalog_definition(listing: &str) -> Value {
    json!({
        "name": TOOL_SEARCH_TOOL_NAME,
        "description": format!(
            "Load a tool that is not declared yet; it becomes callable from your \
             next message on. Keywords match tool names and descriptions, and an \
             exact name always wins. Related tools load together, and every load \
             resets the prompt cache, so search once for the work you are about \
             to do rather than tool by tool.\n{listing}"
        ),
        "input_schema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords or an exact tool name from the list"
                }
            },
            "required": ["query"]
        }
    })
}

fn describe(loaded: &[Arc<str>], query: &str) -> String {
    if loaded.is_empty() {
        return format!("{SEARCH_NO_MATCH} '{query}'. Try other keywords or an exact tool name.");
    }
    let plural = if loaded.len() == 1 { "tool" } else { "tools" };
    let mut out = format!(
        "Loaded {} {plural}, callable from your next message:",
        loaded.len()
    );
    for name in loaded {
        out.push_str(&format!("\n- `{name}`"));
    }
    out
}

/// One line per load: a group names its members, an ungrouped tool carries
/// the sentence its own description opens with. A group is a mode of work
/// rather than a capability, and summarising its members separately would
/// invite loading them one at a time, which is what the group exists to stop.
fn catalog_listing(pending: &[&DeferredTool]) -> String {
    let mut listing = String::new();
    let mut listed: Vec<&str> = Vec::new();
    for tool in pending {
        match tool.group {
            Some(group) if listed.contains(&group) => continue,
            Some(group) => {
                listed.push(group);
                let members: Vec<&str> = pending
                    .iter()
                    .filter(|other| other.group == Some(group))
                    .map(|other| other.name.as_ref())
                    .collect();
                listing.push_str(&format!("\n- {group}: {}", members.join(", ")));
            }
            None => listing.push_str(&format!("\n- {}: {}", tool.name, summary(&tool.definition))),
        }
    }
    listing
}

/// The first sentence of a description, derived rather than authored so the
/// catalog cannot drift from the tool it advertises. A description with no
/// full stop is short enough to use whole.
pub(crate) fn summary(definition: &Value) -> &str {
    let description = definition["description"]
        .as_str()
        .unwrap_or_default()
        .trim_start();
    let end = description
        .match_indices('.')
        .find(|(index, _)| {
            description[index + 1..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace)
        })
        .map_or(description.len(), |(index, _)| index + 1);
    description[..end].trim_end()
}

fn haystack(definition: &Value) -> String {
    let mut hay = definition["description"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    if let Some(properties) = definition["input_schema"]["properties"].as_object() {
        for key in properties.keys() {
            hay.push(' ');
            hay.push_str(&key.to_lowercase());
        }
    }
    hay
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::decisions::{
        CANDIDATE_KEY, CANDIDATE_TEXT_OMITTED, ENGINE_UNREACHABLE, MAX_CANDIDATE_INPUT_BYTES,
    };
    use crate::template::Vars;
    use crate::tools::{
        DescriptionContext, ToolAudience, ToolDefinitions, ToolEffect, ToolFilter, ToolRegistry,
        ToolSource, WORKFLOW_TOOL_NAME,
        native::{OWNER, workflow::WorkflowTool},
    };
    use async_trait::async_trait;
    use caudra_config::decisions::{DecisionsConfig, FeatureMode};
    use caudra_config::{Feature, FeatureFlags};
    use caudra_decision::{
        Answer, DecisionEngine, DecisionError, DecisionRequest, DecisionResponse, NoulAnswer, Usage,
    };
    use caudra_storage::StateDir;
    use caudra_storage::decision_log::{DecisionEffect, DecisionFilter, DecisionLog};
    use futures_lite::future;
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;
    use test_case::test_case;

    const GRAPH: &str = "code_graph";
    const MAP: &str = "code_map";
    const REFS: &str = "code_refs";
    const LONELY: &str = "execution_environment";
    const MAP_DESCRIPTION: &str = "Rank every symbol in a source tree";
    const REFS_DESCRIPTION: &str = "List the symbols that reference one";
    const LONELY_DESCRIPTION: &str = "Inspect the execution host";
    const FIRST_SENTENCE: &str = "Inspect the execution host's environment.";
    const SECOND_SENTENCE: &str = "Each call collects a fresh snapshot.";
    const TWO_SENTENCES: &str =
        "Inspect the execution host's environment. Each call collects a fresh snapshot.";
    const NOTHING_DEFERRED: &str = "a catalog with nothing left to load is dead weight";
    const BEST_SPEC: &str = "anthropic/claude-opus-4-8";
    const NON_SMALL_SPEC: &str = "anthropic/claude-sonnet-4-6";
    const FAST_SPEC: &str = "anthropic/claude-haiku-4-5";
    const DEFERRABLE: &str = "code_map";
    const SEARCH_SECRET: &str = "never-send-this-search-secret";
    const SEARCH_QUERY: &str = "inspect";
    const CANDIDATE_ID_PREFIX: &str = "c";
    const FIRST_CANDIDATE: &str = "c00";
    const SECOND_CANDIDATE: &str = "c01";
    const THIRD_CANDIDATE: &str = "c02";
    pub(crate) const NO_CANDIDATE: &str = "none";
    const UNREACHABLE_REPLY: &str = "unreachable";
    pub(crate) const PENDING_REPLY: &str = "pending";
    pub(crate) const RELEVANT: f64 = 1.0;
    const MORE_RELEVANT: f64 = 0.8;
    const LESS_RELEVANT: f64 = 0.5;
    const TEST_FLOOR: f64 = 0.1;
    const BELOW_FLOOR: f64 = 0.09;
    const TEST_BASE_URL: &str = "http://127.0.0.1:1";
    const TEST_TIMEOUT_MS: u64 = 5_000;
    const EFFECT_TIMEOUT: Duration = Duration::from_secs(5);
    const SEARCH_REDACTED: &str = "[redacted]";
    const RANKED_SUMMARY: &str = "loaded execution_environment instead of code_map, code_refs";
    const SHADOW_RANKED_SUMMARY: &str =
        "would have loaded execution_environment instead of code_map, code_refs";
    const RANKED_DETAILS: [&str; 3] = [
        "1.00  execution_environment  moved 3rd to 1st",
        "0.00  code_map  moved 1st to 2nd",
        "0.00  code_refs  moved 2nd to 3rd",
    ];
    const WORKFLOWS_ON: FeatureFlags = FeatureFlags::NONE.with(Feature::Workflows);

    type Hook = Option<Box<dyn Fn() + Send + Sync>>;

    struct SearchEngine {
        requests: Arc<Mutex<Vec<DecisionRequest>>>,
        scores: BTreeMap<String, f64>,
        hook: Hook,
    }

    #[async_trait]
    impl DecisionEngine for SearchEngine {
        async fn decide(
            &self,
            request: &DecisionRequest,
            _deadline: Instant,
        ) -> Result<DecisionResponse, DecisionError> {
            self.requests.lock().unwrap().push(request.clone());
            if self.scores.contains_key(PENDING_REPLY) {
                return future::pending().await;
            }
            if self.scores.contains_key(UNREACHABLE_REPLY) {
                return Err(DecisionError::Unreachable);
            }
            if let Some(hook) = &self.hook {
                hook();
            }
            Ok(DecisionResponse {
                model: request.model.clone(),
                answers: request
                    .questions
                    .keys()
                    .map(|id| {
                        let noul = self.scores.get(id).copied().unwrap_or_default();
                        (id.clone(), Answer::Noul(NoulAnswer { noul }))
                    })
                    .collect(),
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                cache_hit: false,
            })
        }
    }

    pub(crate) struct SearchDecisions {
        root: TempDir,
        pub(crate) decisions: Decisions,
        pub(crate) requests: Arc<Mutex<Vec<DecisionRequest>>>,
    }

    impl SearchDecisions {
        /// Scores the `favoured` candidate ID at `score` and every other
        /// candidate zero.
        pub(crate) fn new(mode: FeatureMode, favoured: &str, score: f64) -> Self {
            Self::with_hook(mode, favoured, score, None)
        }

        pub(crate) fn with_hook(mode: FeatureMode, favoured: &str, score: f64, hook: Hook) -> Self {
            Self::scored(mode, &[(favoured, score)], hook, false)
        }

        fn scored(mode: FeatureMode, scores: &[(&str, f64)], hook: Hook, log: bool) -> Self {
            let root = tempfile::tempdir().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let mut config = DecisionsConfig {
                base_url: Some(TEST_BASE_URL.parse().unwrap()),
                timeout_ms: TEST_TIMEOUT_MS,
                rerank_timeout_ms: TEST_TIMEOUT_MS,
                log,
                ..DecisionsConfig::default()
            };
            config.features.tool_search = mode;
            config.thresholds.relevance_floor = TEST_FLOOR;
            let decisions = Decisions::with_engine(
                config,
                &StateDir::from_path(root.path().into()),
                SearchEngine {
                    requests: requests.clone(),
                    scores: scores
                        .iter()
                        .map(|(id, score)| ((*id).to_owned(), *score))
                        .collect(),
                    hook,
                },
            )
            .unwrap();
            Self {
                root,
                decisions,
                requests,
            }
        }

        fn sent_candidates(&self) -> Vec<Value> {
            self.requests.lock().unwrap()[0]
                .questions
                .values()
                .map(|question| question.instructions[CANDIDATE_KEY].clone())
                .collect()
        }
    }

    fn candidate_id(index: usize) -> String {
        format!("{CANDIDATE_ID_PREFIX}{index:02}")
    }

    fn ranked_session() -> DeferralSession {
        DeferralSession::new(
            vec![
                tool(MAP, Some(GRAPH), SEARCH_QUERY),
                tool(REFS, Some(GRAPH), SEARCH_QUERY),
                tool(LONELY, None, SEARCH_QUERY),
            ],
            std::iter::empty(),
        )
    }

    fn ranked_search(session: &DeferralSession, decisions: &Decisions) -> SearchOutcome {
        smol::block_on(session.prepare_search_with_decisions(
            SEARCH_QUERY,
            Some(decisions),
            &DecisionContext::default(),
        ))
        .and_then(|prepared| prepared.commit())
        .unwrap()
    }

    #[test]
    fn discarded_preparation_keeps_builtin_catalog_unloaded() {
        let fixture = SearchDecisions::new(FeatureMode::Enforce, THIRD_CANDIDATE, RELEVANT);
        let session = ranked_session();
        let prepared = smol::block_on(session.prepare_search_with_decisions(
            SEARCH_QUERY,
            Some(&fixture.decisions),
            &DecisionContext::default(),
        ))
        .unwrap();
        assert!(prepared.ranking.is_ok());
        assert!(session.lock_loaded().is_empty());
        drop(prepared);
        assert!(session.lock_loaded().is_empty());
        assert_eq!(
            session.search(SEARCH_QUERY).unwrap().loaded,
            [Arc::<str>::from(MAP), Arc::<str>::from(REFS)]
        );
    }

    #[test]
    fn cancelled_preparation_keeps_builtin_catalog_unloaded() {
        smol::block_on(async {
            let fixture = SearchDecisions::new(FeatureMode::Enforce, PENDING_REPLY, RELEVANT);
            let session = ranked_session();
            let context = DecisionContext::default();
            let mut pending = Box::pin(session.prepare_search_with_decisions(
                SEARCH_QUERY,
                Some(&fixture.decisions),
                &context,
            ));
            assert!(future::poll_once(&mut pending).await.is_none());
            assert_eq!(fixture.requests.lock().unwrap().len(), 1);
            drop(pending);
            assert!(session.lock_loaded().is_empty());
            assert!(session.search(SEARCH_QUERY).unwrap().note.is_none());
        });
    }

    #[test_case(FIRST_CANDIDATE, false, false; "lexical_choice")]
    #[test_case(SECOND_CANDIDATE, false, false; "same_group")]
    #[test_case(THIRD_CANDIDATE, false, true; "different_group")]
    #[test_case(THIRD_CANDIDATE, true, false; "selected_group_already_loaded_at_commit")]
    #[test_case(NO_CANDIDATE, false, false; "nothing_relevant_falls_back")]
    fn builtin_commit_accounts_only_changed_new_loads(
        favoured: &str,
        preload: bool,
        rerouted: bool,
    ) {
        let fixture = SearchDecisions::new(FeatureMode::Enforce, favoured, RELEVANT);
        let session = ranked_session();
        let lexical = session.fresh();
        let prepared = smol::block_on(session.prepare_search_with_decisions(
            SEARCH_QUERY,
            Some(&fixture.decisions),
            &DecisionContext::default(),
        ))
        .unwrap();
        assert!(session.lock_loaded().is_empty());
        if preload {
            session.mark_loaded(LONELY);
            lexical.mark_loaded(LONELY);
        }
        let baseline = lexical.search(SEARCH_QUERY).unwrap();
        let outcome = prepared.commit().unwrap();
        assert_eq!(outcome.rerouted_from(&baseline.loaded), rerouted);
        if favoured == SECOND_CANDIDATE {
            assert_eq!(outcome.loaded, baseline.loaded);
        }
        if preload {
            assert!(outcome.loaded.is_empty());
        }
    }

    #[test]
    fn rerouted_is_recorded_when_another_group_loads() {
        let fixture = SearchDecisions::scored(
            FeatureMode::Enforce,
            &[(THIRD_CANDIDATE, RELEVANT)],
            None,
            true,
        );
        let outcome = ranked_search(&ranked_session(), &fixture.decisions);
        assert_eq!(outcome.loaded, [Arc::<str>::from(LONELY)]);
        let log = DecisionLog::open_read_only(&StateDir::from_path(fixture.root.path().into()))
            .unwrap()
            .unwrap();
        let deadline = Instant::now() + EFFECT_TIMEOUT;
        smol::block_on(async {
            loop {
                let effects: Vec<DecisionEffect> = log
                    .recent(&DecisionFilter::default(), 1)
                    .unwrap()
                    .into_iter()
                    .map(|row| row.record.effect)
                    .collect();
                if effects == [DecisionEffect::Rerouted] || Instant::now() >= deadline {
                    assert_eq!(effects, [DecisionEffect::Rerouted]);
                    break;
                }
                future::yield_now().await;
            }
        });
    }

    #[test_case(FeatureMode::Enforce, THIRD_CANDIDATE, RELEVANT, true; "ranked_group")]
    #[test_case(FeatureMode::Enforce, THIRD_CANDIDATE, TEST_FLOOR, true; "floor_inclusive")]
    #[test_case(FeatureMode::Enforce, THIRD_CANDIDATE, BELOW_FLOOR, false; "below_floor")]
    #[test_case(FeatureMode::Shadow, THIRD_CANDIDATE, RELEVANT, false; "shadow_preserves_lexical_group")]
    #[test_case(FeatureMode::Off, THIRD_CANDIDATE, RELEVANT, false; "off")]
    #[test_case(FeatureMode::Enforce, NO_CANDIDATE, RELEVANT, false; "nothing_relevant")]
    #[test_case(FeatureMode::Enforce, UNREACHABLE_REPLY, RELEVANT, false; "unreachable")]
    fn decision_search_falls_back_unless_enforced_and_relevant(
        mode: FeatureMode,
        favoured: &str,
        score: f64,
        ranked: bool,
    ) {
        let fixture = SearchDecisions::new(mode.clone(), favoured, score);
        let outcome = ranked_search(&ranked_session(), &fixture.decisions);
        let expected: Vec<Arc<str>> = if ranked {
            vec![LONELY.into()]
        } else {
            vec![MAP.into(), REFS.into()]
        };
        assert_eq!(outcome.loaded, expected);
        assert_eq!(
            fixture.requests.lock().unwrap().len(),
            usize::from(mode != FeatureMode::Off)
        );
    }

    #[test_case(FeatureMode::Enforce, NoteOutcome::Acted, RANKED_SUMMARY, &[LONELY]; "acted")]
    #[test_case(FeatureMode::Shadow, NoteOutcome::Shadow, SHADOW_RANKED_SUMMARY, &[MAP, REFS]; "shadow_loads_the_keyword_group")]
    fn a_rerouted_search_names_what_it_loaded_instead(
        mode: FeatureMode,
        outcome: NoteOutcome,
        summary: &str,
        loaded: &[&str],
    ) {
        let fixture = SearchDecisions::new(mode, THIRD_CANDIDATE, RELEVANT);
        let searched = ranked_search(&ranked_session(), &fixture.decisions);
        let expected: Vec<Arc<str>> = loaded.iter().map(|name| Arc::from(*name)).collect();
        assert_eq!(searched.loaded, expected);
        assert_eq!(searched.message, describe(&expected, SEARCH_QUERY));
        let note = searched.note.unwrap();
        assert_eq!(note.outcome, outcome);
        assert_eq!(note.summary, summary);
        assert_eq!(note.details, RANKED_DETAILS);
    }

    #[test_case(FIRST_CANDIDATE; "lexical_choice")]
    #[test_case(SECOND_CANDIDATE; "same_group")]
    #[test_case(NO_CANDIDATE; "nothing_relevant")]
    fn an_unchanged_search_has_no_row(favoured: &str) {
        let fixture = SearchDecisions::new(FeatureMode::Enforce, favoured, RELEVANT);
        assert!(
            ranked_search(&ranked_session(), &fixture.decisions)
                .note
                .is_none()
        );
    }

    #[test]
    fn a_failing_engine_falls_back_once_until_it_answers() {
        let fixture = SearchDecisions::new(FeatureMode::Enforce, UNREACHABLE_REPLY, RELEVANT);
        let summaries: Vec<Option<String>> = (0..2)
            .map(|_| {
                ranked_search(&ranked_session(), &fixture.decisions)
                    .note
                    .map(|note| note.summary)
            })
            .collect();
        assert_eq!(
            summaries,
            [
                Some(format!("{ENGINE_UNREACHABLE}; {KEPT_LEXICAL_ORDER}")),
                None
            ]
        );
    }

    #[test_case(&[(FIRST_CANDIDATE, LESS_RELEVANT), (THIRD_CANDIDATE, MORE_RELEVANT)], LONELY; "more_relevant_outranks_the_lexical_head")]
    #[test_case(&[(FIRST_CANDIDATE, MORE_RELEVANT), (THIRD_CANDIDATE, LESS_RELEVANT)], MAP; "lexical_head_keeps_a_higher_score")]
    fn the_most_relevant_candidate_loads(scores: &[(&str, f64)], first: &str) {
        let fixture = SearchDecisions::scored(FeatureMode::Enforce, scores, None, false);
        let outcome = ranked_search(&ranked_session(), &fixture.decisions);
        assert_eq!(outcome.loaded[0].as_ref(), first);
    }

    #[test_case(" CODE_MAP ", true; "exact_case_insensitive")]
    #[test_case("not_found", false; "no_matches")]
    fn exact_or_missing_names_do_not_call_decisions(query: &str, exact: bool) {
        let fixture = SearchDecisions::new(FeatureMode::Enforce, SECOND_CANDIDATE, RELEVANT);
        let session = ranked_session();
        assert_eq!(session.has_exact_match(query), exact);
        let outcome = smol::block_on(session.prepare_search_with_decisions(
            query,
            Some(&fixture.decisions),
            &DecisionContext::default(),
        ))
        .and_then(|prepared| prepared.commit())
        .unwrap();
        assert_eq!(outcome.loaded.is_empty(), !exact);
        assert!(fixture.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn missing_decisions_preserve_lexical_search() {
        let session = ranked_session();
        let outcome = smol::block_on(session.prepare_search_with_decisions(
            SEARCH_QUERY,
            None,
            &DecisionContext::default(),
        ))
        .and_then(|prepared| prepared.commit())
        .unwrap();
        assert_eq!(
            outcome.loaded,
            ranked_session().search(SEARCH_QUERY).unwrap().loaded
        );
    }

    #[test]
    fn shortlist_is_the_lexical_head_in_rank_order() {
        let names: Vec<String> = (0..MAX_RERANK_BATCH + 3)
            .map(|index| format!("tool_{index:03}"))
            .collect();
        let last = MAX_RERANK_BATCH - 1;
        let fixture = SearchDecisions::new(FeatureMode::Enforce, &candidate_id(last), RELEVANT);
        let session = DeferralSession::new(
            names
                .iter()
                .map(|name| tool(name, None, SEARCH_QUERY))
                .collect(),
            std::iter::empty(),
        );
        let outcome = ranked_search(&session, &fixture.decisions);
        assert_eq!(outcome.loaded, [Arc::<str>::from(names[last].as_str())]);
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let questions = &requests[0].questions;
        let expected: Vec<String> = (0..MAX_RERANK_BATCH).map(candidate_id).collect();
        assert_eq!(questions.keys().cloned().collect::<Vec<_>>(), expected);
        for (question, name) in questions.values().zip(&names) {
            assert_eq!(
                question.instructions[CANDIDATE_KEY][NAME_KEY],
                name.as_str()
            );
        }
    }

    #[test_case(FIRST_CANDIDATE, true; "redacted_name")]
    #[test_case(SECOND_CANDIDATE, false; "unredacted_name")]
    fn redaction_does_not_change_tool_identity(favoured: &str, secret_selected: bool) {
        let fixture = SearchDecisions::new(FeatureMode::Enforce, favoured, RELEVANT);
        let secret_name = format!("token={SEARCH_SECRET}");
        let description = format!("{SEARCH_QUERY} token={SEARCH_SECRET}");
        let session = DeferralSession::new(
            vec![
                tool(MAP, None, &description),
                tool(&secret_name, None, &description),
            ],
            std::iter::empty(),
        );
        let query = format!("{SEARCH_QUERY} password={SEARCH_SECRET}");
        let outcome = smol::block_on(session.prepare_search_with_decisions(
            &query,
            Some(&fixture.decisions),
            &DecisionContext::default(),
        ))
        .and_then(|prepared| prepared.commit())
        .unwrap();
        let expected = if secret_selected {
            secret_name.as_str()
        } else {
            MAP
        };
        assert_eq!(outcome.loaded, vec![Arc::<str>::from(expected)]);
        let requests = fixture.requests.lock().unwrap();
        let serialized = serde_json::to_string(&requests[0]).unwrap();
        assert!(!serialized.contains(SEARCH_SECRET));
        assert!(serialized.contains(SEARCH_REDACTED));
        assert_eq!(outcome.note.is_some(), !secret_selected);
        if let Some(note) = outcome.note {
            let text = format!("{} {}", note.summary, note.details.join("\n"));
            assert!(!text.contains(SEARCH_SECRET));
            assert!(text.contains(SEARCH_REDACTED));
        }
    }

    #[test_case("https"; "https")]
    #[test_case("ssh"; "ssh")]
    #[test_case("postgresql"; "postgresql")]
    #[test_case("mongodb+srv"; "mongodb")]
    fn credential_uris_are_scrubbed_in_generated_questions(scheme: &str) {
        let fixture = SearchDecisions::new(FeatureMode::Enforce, FIRST_CANDIDATE, RELEVANT);
        let credential_uri = format!("{scheme}://alice:{SEARCH_SECRET}@host/app");
        let public_uri = format!("{scheme}://host/app");
        let description = format!("{SEARCH_QUERY} {credential_uri} {public_uri}");
        let definitions = [
            json!({"description": description}),
            json!({"description": public_uri}),
        ];
        let result = smol::block_on(rank_tool_search(
            &description,
            [
                (credential_uri.as_str(), &definitions[0]),
                (public_uri.as_str(), &definitions[1]),
            ]
            .into_iter(),
            Some(&fixture.decisions),
            &DecisionContext::default(),
        ));
        assert_eq!(
            result.as_ref().ok().and_then(ToolSearchRanking::top),
            Some(0)
        );
        let serialized = serde_json::to_string(&fixture.requests.lock().unwrap()[0]).unwrap();
        assert!(!serialized.contains(SEARCH_SECRET));
        assert!(!serialized.contains("alice"));
        let candidates = fixture.sent_candidates();
        for field in [NAME_KEY, SUMMARY_KEY] {
            assert!(
                candidates[0][field]
                    .as_str()
                    .unwrap()
                    .contains(SEARCH_REDACTED)
            );
            assert_eq!(candidates[1][field], public_uri);
        }
    }

    #[test_case("界", false, true; "unicode_at_scan_limit")]
    #[test_case("x", true, true; "oversized_input")]
    #[test_case("\u{0000}", false, false; "escaping_past_the_state_bound_is_never_sent")]
    fn generated_candidates_are_bounded(character: &str, oversized: bool, sent: bool) {
        let fixture = SearchDecisions::new(FeatureMode::Enforce, FIRST_CANDIDATE, RELEVANT);
        let text =
            character.repeat(MAX_CANDIDATE_INPUT_BYTES / character.len() + usize::from(oversized));
        let definition = json!({"description": text});
        let result = smol::block_on(rank_tool_search(
            SEARCH_QUERY,
            std::iter::repeat_n((text.as_str(), &definition), MAX_RERANK_BATCH + 1),
            Some(&fixture.decisions),
            &DecisionContext::default(),
        ));
        assert_eq!(result.is_ok(), sent);
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), usize::from(sent));
        for request in requests.iter() {
            request.validate().unwrap();
            assert_eq!(request.questions.len(), MAX_RERANK_BATCH);
            for question in request.questions.values() {
                for (field, limit) in [
                    (NAME_KEY, MAX_SEARCH_NAME_BYTES),
                    (SUMMARY_KEY, MAX_SEARCH_SUMMARY_BYTES),
                ] {
                    let value = question.instructions[CANDIDATE_KEY][field]
                        .as_str()
                        .unwrap();
                    assert!(value.len() <= limit);
                    if oversized {
                        assert_eq!(value, CANDIDATE_TEXT_OMITTED);
                    } else {
                        assert_eq!(value, &text[..text.floor_char_boundary(limit)]);
                    }
                }
            }
        }
    }

    #[test]
    fn oversized_summaries_are_omitted_before_egress() {
        let fixture = SearchDecisions::new(FeatureMode::Enforce, FIRST_CANDIDATE, RELEVANT);
        let description = format!(
            "{SEARCH_QUERY} {} token={SEARCH_SECRET}",
            "x".repeat(MAX_CANDIDATE_INPUT_BYTES)
        );
        let session = DeferralSession::new(
            vec![
                tool(MAP, None, &description),
                tool(LONELY, None, SEARCH_QUERY),
            ],
            std::iter::empty(),
        );
        ranked_search(&session, &fixture.decisions);
        assert!(
            !serde_json::to_string(&fixture.requests.lock().unwrap()[0])
                .unwrap()
                .contains(SEARCH_SECRET)
        );
        assert_eq!(
            fixture.sent_candidates()[0][SUMMARY_KEY],
            CANDIDATE_TEXT_OMITTED
        );
    }

    fn tool(name: &str, group: Option<&'static str>, description: &str) -> DeferredTool {
        DeferredTool::new(
            name,
            group,
            json!({ "name": name, "description": description, "input_schema": {} }),
        )
    }

    fn session() -> DeferralSession {
        DeferralSession::new(
            vec![
                tool(MAP, Some(GRAPH), MAP_DESCRIPTION),
                tool(REFS, Some(GRAPH), REFS_DESCRIPTION),
                tool(LONELY, None, LONELY_DESCRIPTION),
            ],
            std::iter::empty(),
        )
    }

    fn names(tools: &Value) -> Vec<String> {
        tools
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect()
    }

    fn catalog_description(session: &DeferralSession) -> String {
        let mut tools = json!([]);
        session.request_snapshot().extend_tools(&mut tools);
        tools
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == TOOL_SEARCH_TOOL_NAME)
            .expect("the catalog is declared while anything is pending")["description"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn lonely(description: &str) -> DeferralSession {
        DeferralSession::new(vec![tool(LONELY, None, description)], std::iter::empty())
    }

    fn workflow_definitions(
        config: &AgentConfig,
        deferral: BuiltinDeferral,
        audience: ToolAudience,
    ) -> ToolDefinitions {
        let registry = ToolRegistry::new();
        registry
            .register_audited(
                Arc::new(WorkflowTool),
                ToolSource::Native {
                    owner: OWNER.into(),
                    contract: WORKFLOW_TOOL_NAME.into(),
                    trusted: true,
                },
                ToolEffect::Orchestrator,
            )
            .unwrap();
        let model = Model::from_spec(FAST_SPEC).unwrap();
        let filter = ToolFilter::from_config(config, &model, &[]);
        registry.definitions_split(
            &Vars::new(),
            &DescriptionContext {
                filter: &filter,
                audience,
                workflows_available: true,
            },
            false,
            &deferred_names(&config.allowed_tools, deferral),
        )
    }

    #[test_case(false ; "workflow_only")]
    #[test_case(true ; "another_tool_pending")]
    fn workflow_loads_independently_and_updates_the_catalog(other_pending: bool) {
        let mut definitions = workflow_definitions(
            &AgentConfig {
                features: WORKFLOWS_ON,
                ..Default::default()
            },
            BuiltinDeferral::Lazy,
            ToolAudience::MAIN,
        );
        assert!(names(&definitions.declared).is_empty());
        assert_eq!(definitions.deferred.len(), 1);
        let expected = definitions.deferred[0].definition.clone();
        if other_pending {
            definitions
                .deferred
                .push(tool(LONELY, None, LONELY_DESCRIPTION));
        }
        let session = DeferralSession::new(definitions.deferred, std::iter::empty());
        let mut tools = definitions.declared.clone();
        session.request_snapshot().extend_tools(&mut tools);
        assert_eq!(names(&tools), [TOOL_SEARCH_TOOL_NAME]);
        assert!(catalog_description(&session).contains(WORKFLOW_TOOL_NAME));

        let outcome = session.search(WORKFLOW_TOOL_NAME).unwrap();
        assert_eq!(outcome.loaded, [Arc::from(WORKFLOW_TOOL_NAME)]);
        let mut tools = definitions.declared;
        session.request_snapshot().extend_tools(&mut tools);
        assert_eq!(tools[0], expected);
        if other_pending {
            assert_eq!(names(&tools), [WORKFLOW_TOOL_NAME, TOOL_SEARCH_TOOL_NAME]);
            let description = catalog_description(&session);
            assert!(!description.contains(WORKFLOW_TOOL_NAME));
            assert!(description.contains(LONELY));
        } else {
            assert_eq!(names(&tools), [WORKFLOW_TOOL_NAME]);
        }
    }

    #[test_case(BuiltinDeferral::EagerByClass, false ; "eager_by_model")]
    #[test_case(BuiltinDeferral::EagerByConfig, false ; "eager_by_config")]
    #[test_case(BuiltinDeferral::Lazy, true ; "explicitly_allowed")]
    fn workflow_respects_eager_overrides(deferral: BuiltinDeferral, explicitly_allowed: bool) {
        let config = AgentConfig {
            allowed_tools: if explicitly_allowed {
                vec![WORKFLOW_TOOL_NAME.into()]
            } else {
                Vec::new()
            },
            features: WORKFLOWS_ON,
            ..Default::default()
        };
        let definitions = workflow_definitions(&config, deferral, ToolAudience::MAIN);
        assert!(definitions.deferred.is_empty());
        let mut tools = definitions.declared;
        DeferralSession::new(definitions.deferred, std::iter::empty())
            .request_snapshot()
            .extend_tools(&mut tools);
        assert_eq!(names(&tools), [WORKFLOW_TOOL_NAME]);
    }

    #[test_case(ToolAudience::MAIN, true, false, WORKFLOWS_ON ; "disabled")]
    #[test_case(ToolAudience::MAIN, false, true, WORKFLOWS_ON ; "filtered_out")]
    #[test_case(ToolAudience::GENERAL_SUB, false, false, WORKFLOWS_ON ; "general_subagent")]
    #[test_case(ToolAudience::RESEARCH_SUB, false, false, WORKFLOWS_ON ; "research_subagent")]
    #[test_case(ToolAudience::MAIN, false, false, FeatureFlags::NONE ; "experiment_off")]
    fn excluded_workflow_does_not_create_a_catalog(
        audience: ToolAudience,
        disabled: bool,
        filtered: bool,
        features: FeatureFlags,
    ) {
        let config = AgentConfig {
            disabled_tools: if disabled {
                vec![WORKFLOW_TOOL_NAME.into()]
            } else {
                Vec::new()
            },
            allowed_tools: if filtered {
                vec![MAP.into()]
            } else {
                Vec::new()
            },
            features,
            ..Default::default()
        };
        let definitions = workflow_definitions(&config, BuiltinDeferral::Lazy, audience);
        assert!(definitions.deferred.is_empty());
        let mut tools = definitions.declared;
        DeferralSession::new(definitions.deferred, std::iter::empty())
            .request_snapshot()
            .extend_tools(&mut tools);
        assert!(names(&tools).is_empty());
    }

    /// The catalog is what the model reads when deciding to spend a load, so
    /// it carries one sentence and stops.
    #[test]
    fn an_ungrouped_tool_is_listed_with_its_first_sentence() {
        let description = catalog_description(&lonely(TWO_SENTENCES));

        assert!(
            description.contains(&format!("- {LONELY}: {FIRST_SENTENCE}")),
            "{description}"
        );
        assert!(!description.contains(SECOND_SENTENCE), "{description}");
    }

    /// A group is one decision. Summarising each member separately would
    /// invite loading them one at a time.
    #[test]
    fn a_group_is_named_once_and_its_members_are_not_summarised() {
        let description = catalog_description(&session());

        assert!(
            description.contains(&format!("- {GRAPH}: {MAP}, {REFS}")),
            "{description}"
        );
        assert_eq!(description.matches(GRAPH).count(), 1, "{description}");
        assert!(!description.contains(MAP_DESCRIPTION), "{description}");
    }

    #[test]
    fn a_description_without_a_full_stop_is_listed_whole() {
        let description = catalog_description(&lonely(LONELY_DESCRIPTION));

        assert!(
            description.contains(&format!("- {LONELY}: {LONELY_DESCRIPTION}")),
            "{description}"
        );
    }

    #[test]
    fn nothing_is_declared_until_something_loads() {
        let mut tools = json!([{ "name": "file_read" }]);
        session().request_snapshot().extend_tools(&mut tools);

        assert_eq!(names(&tools), ["file_read", TOOL_SEARCH_TOOL_NAME]);
    }

    #[test_case(false; "unbound_loader")]
    #[test_case(true; "hidden_shadowing_binding")]
    fn hidden_binding_still_blocks_synthetic_discovery(bound: bool) {
        let mut tools = json!([]);
        let sections = session()
            .request_snapshot()
            .extend_declared(&mut tools)
            .into_iter()
            .collect::<Vec<_>>();
        assert!(!sections.is_empty());
        assert!(names(&tools).is_empty());
        push_unbound_catalog(&mut tools, &sections, bound);
        assert_eq!(
            names(&tools)
                .iter()
                .any(|name| name == TOOL_SEARCH_TOOL_NAME),
            !bound,
        );
    }

    /// The whole point of the group: five code tools are one decision and one
    /// cache miss, not five.
    #[test]
    fn a_match_loads_its_whole_group() {
        let session = session();
        let outcome = session.search("symbol references").unwrap();

        assert_eq!(outcome.loaded.len(), 2, "{:?}", outcome.loaded);
        let mut tools = json!([]);
        session.request_snapshot().extend_tools(&mut tools);
        assert_eq!(names(&tools), [MAP, REFS, TOOL_SEARCH_TOOL_NAME]);
    }

    #[test]
    fn an_ungrouped_tool_loads_alone() {
        let session = session();
        session.search(LONELY).unwrap();

        let mut tools = json!([]);
        session.request_snapshot().extend_tools(&mut tools);
        assert_eq!(names(&tools), [LONELY, TOOL_SEARCH_TOOL_NAME]);
    }

    #[test]
    fn the_catalog_disappears_once_everything_is_loaded() {
        let session = session();
        session.search(MAP).unwrap();
        session.search(LONELY).unwrap();

        let mut tools = json!([]);
        session.request_snapshot().extend_tools(&mut tools);
        assert_eq!(names(&tools), [MAP, REFS, LONELY], "{NOTHING_DEFERRED}");
    }

    /// A model that guesses the name right should not be told to search for
    /// what it just called successfully.
    #[test]
    fn calling_a_deferred_tool_directly_loads_its_group() {
        let session = session();

        assert_eq!(session.mark_loaded(MAP).len(), 2);
        assert!(session.mark_loaded(MAP).is_empty(), "already loaded");
        assert!(session.mark_loaded("file_read").is_empty(), "not deferred");
    }

    #[test]
    fn a_restored_session_keeps_what_it_already_loaded() {
        let session = DeferralSession::new(
            vec![tool(LONELY, None, "Inspect the execution host")],
            [Arc::from("file_read"), Arc::from(LONELY)].into_iter(),
        );

        let mut tools = json!([]);
        session.request_snapshot().extend_tools(&mut tools);
        assert_eq!(names(&tools), [LONELY]);
    }

    #[test]
    fn a_search_that_matches_nothing_loads_nothing() {
        let session = session();
        let outcome = session.search("quantum tunnelling").unwrap();

        assert!(outcome.loaded.is_empty());
        assert!(
            outcome.message.contains(SEARCH_NO_MATCH),
            "{}",
            outcome.message
        );
    }

    #[test_case("" ; "blank")]
    #[test_case("   " ; "whitespace")]
    #[test_case("!!" ; "punctuation only")]
    fn an_empty_query_is_an_error(query: &str) {
        assert_eq!(session().search(query).unwrap_err(), SEARCH_EMPTY_QUERY);
    }

    /// A subagent inherits the catalog but not the parent's loads.
    #[test]
    fn a_fresh_view_starts_with_nothing_loaded() {
        let parent = session();
        parent.search(MAP).unwrap();
        let child = parent.fresh();

        let mut tools = json!([]);
        child.request_snapshot().extend_tools(&mut tools);
        assert_eq!(names(&tools), [TOOL_SEARCH_TOOL_NAME]);
    }

    /// Only a small model, and a model nobody classified, pays for the shorter
    /// array. A known non-small model would spend a cache prefix loading what it
    /// was going to reach for.
    #[test_case(DeferBuiltinTools::Auto, Some(ModelPurpose::Fast), BuiltinDeferral::Lazy ; "auto_defers_for_fast")]
    #[test_case(DeferBuiltinTools::Auto, None, BuiltinDeferral::Lazy ; "auto_defers_for_an_unclassified_model")]
    #[test_case(DeferBuiltinTools::Auto, Some(ModelPurpose::Best), BuiltinDeferral::EagerByClass ; "auto_declares_for_best")]
    #[test_case(DeferBuiltinTools::Always, Some(ModelPurpose::Best), BuiltinDeferral::Lazy ; "always_outranks_the_class")]
    #[test_case(DeferBuiltinTools::Never, Some(ModelPurpose::Fast), BuiltinDeferral::EagerByConfig ; "never_outranks_the_class")]
    fn the_class_decides_deferral_unless_the_setting_does(
        setting: DeferBuiltinTools,
        class: Option<ModelPurpose>,
        expected: BuiltinDeferral,
    ) {
        assert_eq!(BuiltinDeferral::from_class(setting, class), expected);
    }

    /// The rule above is only worth anything if the lookup it wraps files real
    /// models where the curated table says it does.
    #[test_case(BEST_SPEC, BuiltinDeferral::EagerByClass ; "a_best_model_takes_them_upfront")]
    #[test_case(NON_SMALL_SPEC, BuiltinDeferral::EagerByClass ; "any_known_non_small_model_takes_them_upfront")]
    #[test_case(FAST_SPEC, BuiltinDeferral::Lazy ; "a_fast_model_defers")]
    fn resolve_reads_the_class_of_a_real_model(spec: &str, expected: BuiltinDeferral) {
        let model = Model::from_spec(spec).unwrap();

        assert_eq!(
            BuiltinDeferral::resolve(&AgentConfig::default(), &model),
            expected
        );
    }

    #[test_case(BuiltinDeferral::EagerByClass ; "by class")]
    #[test_case(BuiltinDeferral::EagerByConfig ; "by config")]
    fn an_eager_run_withholds_nothing(deferral: BuiltinDeferral) {
        assert!(deferred_names(&[], deferral).is_empty());
        assert!(!is_deferred(DEFERRABLE, &[], deferral));
    }

    #[test]
    fn a_lazy_run_withholds_every_deferrable_builtin() {
        let names = deferred_names(&[], BuiltinDeferral::Lazy);

        assert_eq!(names.len(), caudra_config::DEFERRED_BUILTIN_TOOLS.len());
        assert!(is_deferred(DEFERRABLE, &[], BuiltinDeferral::Lazy));
    }

    /// Definitions the caller already put in the array win, so a tool forced
    /// upfront by `allowed_tools` is never declared twice.
    #[test]
    fn an_already_declared_tool_is_not_appended_again() {
        let session = session();
        session.search(MAP).unwrap();

        let mut tools = json!([{ "name": MAP }]);
        session.request_snapshot().extend_tools(&mut tools);
        assert_eq!(names(&tools), [MAP, REFS, TOOL_SEARCH_TOOL_NAME]);
    }

    #[test_case(false ; "no_deferred_tools")]
    #[test_case(true ; "all_candidates_already_declared")]
    fn no_catalog_is_added_when_nothing_can_be_loaded(already_declared: bool) {
        let session = if already_declared {
            session()
        } else {
            DeferralSession::default()
        };
        let mut tools = json!([{ "name": MAP }, { "name": REFS }, { "name": LONELY }]);
        let expected = tools.clone();

        session.request_snapshot().extend_tools(&mut tools);

        assert_eq!(tools, expected, "{NOTHING_DEFERRED}");
    }
}
