//! Built-in tools held out of the request array until the model asks for them.
//!
//! The mechanism is the one MCP already uses for oversized servers: what is
//! not declared is listed by name inside a single `tool_search` catalog, and a
//! search moves it into the array from the next request on. Here the deferred
//! set is the fixed list in [`caudra_config::DEFERRED_BUILTIN_TOOLS`] rather
//! than a per-server threshold, because the reason is different: a built-in is
//! deferred when most sessions never call it, not when there are too many of
//! them.
//!
//! Deferring costs a prompt-cache prefix each time something loads, so tools
//! that are one mode of work carry a group and load together. The code graph
//! is five tools and one decision.
//!
//! That prefix is also why deferral is per model rather than global. A Fast
//! model gains from the shorter array and rebuilds its prefix cheaply; a
//! Balanced or Best one would spend a large prefix to load what it was going
//! to reach for anyway. [`BuiltinDeferral`] is that decision, taken once per
//! definitions build and carried as a value, because reading it is a
//! `providers.toml` parse ([`Model::class_of`]) and the tool report asks per
//! registry entry.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};

use caudra_config::{AgentConfig, DeferBuiltinTools};
use caudra_providers::{ContentBlock, Message, Model, ModelPurpose};
use serde_json::{Value, json};
use tracing::{info, warn};

pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";

const NAME_HIT_SCORE: usize = 2;
const DESCRIPTION_HIT_SCORE: usize = 1;
pub(crate) const SEARCH_EMPTY_QUERY: &str = "query must not be empty";
const SEARCH_NO_MATCH: &str = "No deferred tools matched";
const BUILTIN_HEADING: &str = "Available to load:";

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
            (DeferBuiltinTools::Auto, Some(ModelPurpose::Balanced | ModelPurpose::Best)) => {
                Self::EagerByClass
            }
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

/// The deferred catalog for one session. Cloning shares the loaded set, the
/// way [`crate::mcp::McpSession`] does, so a tool loaded inside a batch child
/// is loaded for the agent that dispatched it.
#[derive(Clone, Default)]
pub struct DeferralSession {
    deferred: Arc<Vec<DeferredTool>>,
    loaded: Arc<Mutex<HashSet<Arc<str>>>>,
}

/// What one search moved into the request array.
#[derive(Debug)]
pub struct SearchOutcome {
    pub loaded: Vec<Arc<str>>,
    pub message: String,
}

impl DeferralSession {
    /// `history_names` seeds the loaded set the way MCP reseeds from wire
    /// names: a restored session keeps the tools it had already searched for,
    /// instead of making the model find them again.
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
    /// outranks a description hit, matching MCP's ordering.
    pub fn search(&self, query: &str) -> Result<SearchOutcome, String> {
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

        let mut guard = self.lock_loaded();
        let loaded: Vec<Arc<str>> = matches
            .first()
            .map(|(_, _, hit)| self.load_group(&mut guard, hit))
            .unwrap_or_default();
        drop(guard);

        info!(query = %q, loaded = loaded.len(), "built-in tool search");
        Ok(SearchOutcome {
            message: describe(&loaded, query),
            loaded,
        })
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

/// The one entry that stands in for everything deferred, whatever it came
/// from. Built-ins and MCP each contribute a section, and `run_tool_search`
/// already answers for both, so the model never has to know which is which.
///
/// A `tool_search` the caller declared itself wins: a plugin or server owning
/// that name is a real conflict, unlike the two internal sources that share
/// this entry by design.
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
fn summary(definition: &Value) -> &str {
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
mod tests {
    use super::*;
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
    const FAST_SPEC: &str = "anthropic/claude-haiku-4-5";
    const DEFERRABLE: &str = "code_map";

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

    /// Only a Fast model, and a model nobody classified, pays for the shorter
    /// array. Everything else would spend a cache prefix loading what it was
    /// going to reach for.
    #[test_case(DeferBuiltinTools::Auto, Some(ModelPurpose::Fast), BuiltinDeferral::Lazy ; "auto_defers_for_fast")]
    #[test_case(DeferBuiltinTools::Auto, None, BuiltinDeferral::Lazy ; "auto_defers_for_an_unclassified_model")]
    #[test_case(DeferBuiltinTools::Auto, Some(ModelPurpose::Balanced), BuiltinDeferral::EagerByClass ; "auto_declares_for_balanced")]
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
}
