use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use caudra_config::{AgentConfig, CompactionBuffer, ProfileToolPolicy};
use caudra_providers::{
    ContentBlock, Message, Model, Role, adapt_images_for_model, estimate_tokens_cached,
};
use serde_json::Value;

use crate::AgentMode;
use crate::agent::{compaction_reserve, estimate_message_tokens};
use crate::mcp::{McpRequestSnapshot, McpToolStatus};
use crate::prompt::profile::{BUILTIN_PROFILE_NAME, PromptProfileCatalog, TaskProfileBindings};
use crate::tools::TOOL_SEARCH_TOOL_NAME;
use crate::tools::native::{memory, skill};
use crate::tools::{
    BuiltinDeferral, DeferredTool, DescriptionContext, MEMORY_TOOL_NAME, SKILL_TOOL_NAME,
    TASK_TOOL_NAME, ToolAudience, ToolFilter, ToolRegistry, ToolState, builtin_report,
};

const MEMORY_READ_COMMAND: &str = "read";
const BILLED_TO_PROFILES: &str = "profiles";
const BILLED_TO_MEMORY: &str = "memory";
const BILLED_TO_SKILLS: &str = "skills";
const REQUEST_UNAVAILABLE: &str = "unavailable in this request";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ContextKey {
    Main,
    Task(Arc<str>),
}

impl ContextKey {
    pub fn task(task_id: impl Into<Arc<str>>) -> Self {
        Self::Task(task_id.into())
    }
}

#[derive(Clone, Default)]
pub struct ContextStore {
    snapshots: Arc<RwLock<HashMap<ContextKey, Arc<ContextSnapshot>>>>,
}

impl ContextStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn publisher(&self, key: ContextKey) -> ContextPublisher {
        ContextPublisher {
            store: self.clone(),
            key,
        }
    }

    pub fn latest(&self, key: &ContextKey) -> Option<Arc<ContextSnapshot>> {
        self.snapshots
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .cloned()
    }
}

#[derive(Clone)]
pub struct ContextPublisher {
    store: ContextStore,
    key: ContextKey,
}

impl ContextPublisher {
    pub fn for_task(&self, task_id: impl Into<Arc<str>>) -> Self {
        self.store.publisher(ContextKey::task(task_id))
    }

    pub fn publish(&self, snapshot: ContextSnapshot) {
        self.store
            .snapshots
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(self.key.clone(), Arc::new(snapshot));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextReadiness {
    PreparedNextRequest,
    CapturedCurrentRequest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextModel {
    pub spec: String,
    pub provider_display_name: String,
}

impl From<&Model> for ContextModel {
    fn from(model: &Model) -> Self {
        Self {
            spec: model.spec(),
            provider_display_name: model.provider_display_name().to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextReserve {
    Disabled,
    Enabled(u32),
}

impl ContextReserve {
    pub fn tokens(self) -> u32 {
        match self {
            Self::Disabled => 0,
            Self::Enabled(tokens) => tokens,
        }
    }

    pub fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextWindow {
    pub tokens: u32,
    pub reserve: ContextReserve,
}

impl ContextWindow {
    pub fn new(model: &Model, auto_compact: bool, buffer: Option<CompactionBuffer>) -> Self {
        Self {
            tokens: model.context_window,
            reserve: if auto_compact {
                ContextReserve::Enabled(compaction_reserve(model, buffer))
            } else {
                ContextReserve::Disabled
            },
        }
    }

    /// Where auto-compaction fires. Equal to the window itself when it is off.
    pub fn threshold(&self) -> u32 {
        self.tokens.saturating_sub(self.reserve.tokens())
    }

    /// [`threshold`](Self::threshold), but only where it is a limit of its own.
    pub fn compaction_border(&self) -> Option<u32> {
        self.reserve.is_enabled().then(|| self.threshold())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextUsage {
    pub system_prompt: u32,
    pub system_tools: u32,
    pub mcp_tools: u32,
    pub profiles: u32,
    pub memory: u32,
    pub skills: u32,
    pub messages: u32,
}

impl ContextUsage {
    pub fn used(&self) -> u32 {
        [
            self.system_prompt,
            self.system_tools,
            self.mcp_tools,
            self.profiles,
            self.memory,
            self.skills,
            self.messages,
        ]
        .into_iter()
        .fold(0, u32::saturating_add)
    }

    pub fn free(&self, window: &ContextWindow) -> u32 {
        window.threshold().saturating_sub(self.used())
    }

    pub fn over_window(&self, window: &ContextWindow) -> u32 {
        self.used().saturating_sub(window.tokens)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextProfileSource {
    Builtin,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextProfile {
    pub name: String,
    pub description: Option<String>,
    pub source: ContextProfileSource,
    pub active: bool,
    pub available_for_tasks: bool,
    pub task_unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextProfileInventory {
    pub profiles: Vec<ContextProfile>,
    pub request_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMemoryFile {
    pub name: String,
    pub tags: Vec<String>,
    pub on_load_tokens: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextMemoryInventory {
    pub directory: Option<PathBuf>,
    pub files: Vec<ContextMemoryFile>,
    pub unreadable_files: usize,
    pub definition_tokens: u32,
    pub prompt_tokens: u32,
    pub loaded_tokens: u32,
}

impl ContextMemoryInventory {
    pub fn on_load_tokens(&self) -> u32 {
        self.files
            .iter()
            .map(|file| file.on_load_tokens)
            .fold(0, u32::saturating_add)
    }

    pub fn request_tokens(&self) -> u32 {
        [
            self.definition_tokens,
            self.prompt_tokens,
            self.loaded_tokens,
        ]
        .into_iter()
        .fold(0, u32::saturating_add)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSkill {
    pub name: String,
    pub description: String,
    pub loaded_tokens: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextSkillInventory {
    pub skills: Vec<ContextSkill>,
    pub definition_tokens: u32,
    pub loaded_tokens: u32,
}

impl ContextSkillInventory {
    pub fn request_tokens(&self) -> u32 {
        self.definition_tokens.saturating_add(self.loaded_tokens)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContextMcpStatus {
    LoadedOrEager,
    AvailableOnDemand,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMcpTool {
    pub qualified_name: String,
    pub wire_name: String,
    pub server: String,
    pub status: ContextMcpStatus,
    pub reason: Option<&'static str>,
    pub request_tokens: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextMcpInventory {
    pub tools: Vec<ContextMcpTool>,
    pub unattributed_tokens: u32,
}

impl ContextMcpInventory {
    pub fn from_statuses(statuses: Vec<McpToolStatus>) -> Self {
        let mut tools = statuses
            .into_iter()
            .map(|status| ContextMcpTool {
                qualified_name: status.qualified_name,
                wire_name: status.wire_name,
                server: status.server,
                reason: status.reason,
                status: if status.disabled {
                    ContextMcpStatus::Disabled
                } else if status.deferred {
                    ContextMcpStatus::AvailableOnDemand
                } else {
                    ContextMcpStatus::LoadedOrEager
                },
                request_tokens: 0,
            })
            .collect::<Vec<_>>();
        tools.sort_by(|left, right| {
            left.status
                .cmp(&right.status)
                .then_with(|| left.server.cmp(&right.server))
                .then_with(|| left.qualified_name.cmp(&right.qualified_name))
        });
        Self {
            tools,
            ..Self::default()
        }
    }

    pub fn request_tokens(&self) -> u32 {
        self.tools
            .iter()
            .map(|tool| tool.request_tokens)
            .chain([self.unattributed_tokens])
            .fold(0, u32::saturating_add)
    }
}

/// Whether a built-in reaches the model in this request. `Deferred` is not a
/// weaker `Declared`: the definition is absent from the array, so its tokens
/// are what a load would cost rather than what the request is paying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContextBuiltinState {
    Declared,
    Deferred,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBuiltinTool {
    pub name: String,
    pub source: String,
    pub state: ContextBuiltinState,
    /// The rule that decided the state, when one had to.
    pub reason: Option<&'static str>,
    pub tokens: u32,
    /// The category row already paying for this definition. `task`, `memory`
    /// and `skill` are charged to the feature they serve, so their tokens are
    /// reported here and left out of the built-in total.
    pub billed_to: Option<&'static str>,
}

/// `catalog_tokens` is the one `tool_search` entry. It stands in for the
/// deferred built-ins and the deferred MCP tools together, so it belongs to
/// neither inventory alone and is counted here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextBuiltinInventory {
    pub tools: Vec<ContextBuiltinTool>,
    pub catalog_tokens: u32,
    /// What the array costs beyond the definitions it holds, so the section
    /// reconciles with `ContextUsage::system_tools` exactly.
    pub unattributed_tokens: u32,
}

impl ContextBuiltinInventory {
    fn tokens_for(&self, state: ContextBuiltinState) -> u32 {
        self.tools
            .iter()
            .filter(|tool| tool.state == state && tool.billed_to.is_none())
            .map(|tool| tool.tokens)
            .fold(0, u32::saturating_add)
    }

    /// What the declared built-ins cost this request, catalog included.
    pub fn request_tokens(&self) -> u32 {
        self.tokens_for(ContextBuiltinState::Declared)
            .saturating_add(self.catalog_tokens)
            .saturating_add(self.unattributed_tokens)
    }

    /// What loading every deferred built-in would add.
    pub fn deferred_tokens(&self) -> u32 {
        self.tokens_for(ContextBuiltinState::Deferred)
    }

    pub fn count(&self, state: ContextBuiltinState) -> usize {
        self.tools.iter().filter(|tool| tool.state == state).count()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextInventory {
    pub profiles: ContextProfileInventory,
    pub memory: ContextMemoryInventory,
    pub skills: ContextSkillInventory,
    pub builtins: ContextBuiltinInventory,
    pub mcp: ContextMcpInventory,
}

/// Everything the built-in half of an inventory needs: the tools that exist,
/// the rules that decide their state, and the deferred definitions the array
/// does not carry.
pub struct BuiltinToolsInput<'a> {
    pub registry: &'a ToolRegistry,
    pub filter: &'a ToolFilter,
    pub config: &'a AgentConfig,
    pub model: &'a Model,
    pub session_plan: bool,
    pub audience: ToolAudience,
    pub deferral: BuiltinDeferral,
    pub deferred: &'a [DeferredTool],
}

impl BuiltinToolsInput<'_> {
    pub fn inventory(&self, profile: &ProfileToolPolicy) -> ContextBuiltinInventory {
        let deferred_tokens: HashMap<&str, u32> = self
            .deferred
            .iter()
            .map(|tool| (tool.name.as_ref(), value_tokens(&tool.definition)))
            .collect();
        let tools = self
            .registry
            .iter()
            .iter()
            .map(|entry| {
                let name = entry.name();
                let report = builtin_report(
                    name,
                    self.filter,
                    &[],
                    self.config,
                    self.model,
                    self.deferral,
                );
                let report = crate::tools::report::profile_report(
                    entry,
                    profile,
                    report,
                    &DescriptionContext {
                        filter: self.filter,
                        audience: self.audience,
                        workflows_available: true,
                    },
                    self.session_plan,
                );
                let (state, reason) = match report.state {
                    ToolState::On => (ContextBuiltinState::Declared, report.reason),
                    ToolState::Lazy if deferred_tokens.contains_key(name) => {
                        (ContextBuiltinState::Deferred, report.reason)
                    }
                    ToolState::Lazy => (ContextBuiltinState::Disabled, Some(REQUEST_UNAVAILABLE)),
                    ToolState::Off => (ContextBuiltinState::Disabled, report.reason),
                };
                ContextBuiltinTool {
                    name: name.to_owned(),
                    source: entry.source.as_log_field().into_owned(),
                    state,
                    reason,
                    tokens: deferred_tokens.get(name).copied().unwrap_or_default(),
                    billed_to: None,
                }
            })
            .collect();
        ContextBuiltinInventory {
            tools,
            ..ContextBuiltinInventory::default()
        }
    }
}

impl ContextInventory {
    pub fn collect(
        cwd: &Path,
        registry: &ToolRegistry,
        profiles: &PromptProfileCatalog,
        task_profiles: &TaskProfileBindings,
        active_profile: Option<&str>,
        builtins: Option<&BuiltinToolsInput<'_>>,
        mcp: Option<&McpRequestSnapshot>,
    ) -> Self {
        let available = task_profiles
            .available()
            .map(|profile| profile.name().to_owned())
            .collect::<HashSet<_>>();
        let disabled = task_profiles
            .disabled()
            .map(|(name, reason)| (name.to_owned(), reason.to_owned()))
            .collect::<BTreeMap<_, _>>();
        let mut profile_entries = vec![ContextProfile {
            name: BUILTIN_PROFILE_NAME.to_owned(),
            description: None,
            source: ContextProfileSource::Builtin,
            active: active_profile == Some(BUILTIN_PROFILE_NAME),
            available_for_tasks: true,
            task_unavailable_reason: None,
        }];
        profile_entries.extend(profiles.profiles().map(|profile| ContextProfile {
            name: profile.name().to_owned(),
            description: profile.description().map(str::to_owned),
            source: ContextProfileSource::User,
            active: active_profile == Some(profile.name()),
            available_for_tasks: available.contains(profile.name()),
            task_unavailable_reason: disabled.get(profile.name()).cloned(),
        }));

        let memory = memory::inventory(cwd).map_or_else(ContextMemoryInventory::default, |found| {
            ContextMemoryInventory {
                directory: Some(found.directory),
                files: found
                    .notes
                    .into_iter()
                    .map(|note| ContextMemoryFile {
                        name: note.name,
                        tags: note.tags,
                        on_load_tokens: note.on_load_tokens,
                    })
                    .collect(),
                unreadable_files: found.unreadable_files,
                ..ContextMemoryInventory::default()
            }
        });
        let skills = skill::inventory(registry)
            .into_iter()
            .map(|entry| ContextSkill {
                name: entry.name,
                description: entry.description,
                loaded_tokens: 0,
            })
            .collect();
        let mcp = mcp
            .map(McpRequestSnapshot::tool_inventory)
            .map(ContextMcpInventory::from_statuses)
            .unwrap_or_default();

        Self {
            profiles: ContextProfileInventory {
                profiles: profile_entries,
                request_tokens: 0,
            },
            memory,
            skills: ContextSkillInventory {
                skills,
                ..ContextSkillInventory::default()
            },
            builtins: builtins
                .map(|builtins| {
                    builtins.inventory(
                        &profiles
                            .resolve(active_profile)
                            .ok()
                            .flatten()
                            .map(|profile| profile.tools().clone())
                            .unwrap_or_default(),
                    )
                })
                .unwrap_or_default(),
            mcp,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSnapshot {
    pub readiness: ContextReadiness,
    pub mode: AgentMode,
    pub audience: ToolAudience,
    pub model: ContextModel,
    pub window: ContextWindow,
    pub usage: ContextUsage,
    /// The same request as [`usage`](Self::usage), but anchored on the count the
    /// provider last charged. `None` until a response has been billed.
    pub measured: Option<u32>,
    pub inventory: ContextInventory,
}

impl ContextSnapshot {
    /// What to compare against the window. Prefers the provider's own count,
    /// which is the one auto-compaction decides on; the estimate stands in only
    /// until the first response arrives, and remains available beside it as the
    /// per-category breakdown.
    pub fn used(&self) -> u32 {
        self.measured.unwrap_or_else(|| self.usage.used())
    }
}

pub struct ContextCapture<'a> {
    pub readiness: ContextReadiness,
    pub mode: &'a AgentMode,
    pub audience: ToolAudience,
    pub model: &'a Model,
    pub auto_compact: bool,
    pub compaction_buffer: Option<CompactionBuffer>,
    pub system: &'a str,
    pub base_tools: &'a Value,
    pub full_tools: &'a Value,
    pub projected_messages: &'a [Message],
    pub measured: Option<u32>,
    pub inventory: ContextInventory,
}

impl ContextSnapshot {
    pub fn capture(capture: ContextCapture<'_>) -> Self {
        let mut source_definitions = capture.base_tools.as_array().cloned().unwrap_or_default();
        source_definitions.extend(
            capture
                .inventory
                .builtins
                .tools
                .iter()
                .filter(|tool| !tool.source.starts_with("mcp:"))
                .map(|tool| serde_json::json!({"name": tool.name})),
        );
        let accounting = account_request(
            capture.model,
            capture.system,
            &Value::Array(source_definitions),
            capture.full_tools,
            capture.projected_messages,
        );
        let mut inventory = capture.inventory;
        inventory.apply_accounting(&accounting);
        Self {
            readiness: capture.readiness,
            mode: capture.mode.clone(),
            audience: capture.audience,
            model: ContextModel::from(capture.model),
            window: ContextWindow::new(
                capture.model,
                capture.auto_compact,
                capture.compaction_buffer,
            ),
            usage: accounting.usage,
            measured: capture.measured,
            inventory,
        }
    }
}

pub fn estimate_context_usage(
    model: &Model,
    system: &str,
    base_tools: &Value,
    full_tools: &Value,
    projected_messages: &[Message],
) -> ContextUsage {
    account_request(model, system, base_tools, full_tools, projected_messages).usage
}

#[derive(Default)]
struct RequestAccounting {
    usage: ContextUsage,
    memory_definition_tokens: u32,
    memory_prompt_tokens: u32,
    memory_result_tokens: u32,
    skill_definition_tokens: u32,
    skill_result_tokens: u32,
    skill_results: BTreeMap<String, u32>,
    builtin_definitions: Vec<BuiltinDefinition>,
    system_unattributed: u32,
    mcp_definitions: Vec<(String, u32)>,
}

#[derive(Default)]
struct ToolAccounting {
    system: u32,
    mcp: u32,
    profiles: u32,
    memory: u32,
    skills: u32,
    builtin_definitions: Vec<BuiltinDefinition>,
    system_unattributed: u32,
    mcp_definitions: Vec<(String, u32)>,
}

/// One built-in definition the request array carries, and the category its
/// tokens were charged to when that was not the generic tool overhead.
struct BuiltinDefinition {
    name: String,
    tokens: u32,
    billed_to: Option<&'static str>,
    catalog: bool,
}

#[derive(Clone, Copy)]
enum ToolCategory {
    System,
    Mcp,
    Profiles,
    Memory,
    Skills,
}

impl ToolCategory {
    /// The category row that already pays for this definition, so the
    /// built-in section can name it rather than repeat the charge.
    fn billed_to(self) -> Option<&'static str> {
        match self {
            Self::System | Self::Mcp => None,
            Self::Profiles => Some(BILLED_TO_PROFILES),
            Self::Memory => Some(BILLED_TO_MEMORY),
            Self::Skills => Some(BILLED_TO_SKILLS),
        }
    }
}

struct ToolContribution {
    name: String,
    category: ToolCategory,
    tokens: u32,
}

#[derive(Default)]
struct MessageAccounting {
    messages: u32,
    memory: u32,
    skills: u32,
    skill_results: BTreeMap<String, u32>,
}

enum LoadedBody<'a> {
    Memory,
    Skill(&'a str),
}

fn account_request(
    model: &Model,
    system: &str,
    base_tools: &Value,
    full_tools: &Value,
    projected_messages: &[Message],
) -> RequestAccounting {
    let (system_prompt, memory_prompt_tokens) = account_system(system);
    let tools = account_tools(base_tools, full_tools);
    let adapted_messages = adapt_images_for_model(model, projected_messages);
    let messages = account_messages(adapted_messages.as_ref());
    RequestAccounting {
        usage: ContextUsage {
            system_prompt,
            system_tools: tools.system,
            mcp_tools: tools.mcp,
            profiles: tools.profiles,
            memory: tools
                .memory
                .saturating_add(memory_prompt_tokens)
                .saturating_add(messages.memory),
            skills: tools.skills.saturating_add(messages.skills),
            messages: messages.messages,
        },
        memory_definition_tokens: tools.memory,
        memory_prompt_tokens,
        memory_result_tokens: messages.memory,
        skill_definition_tokens: tools.skills,
        skill_result_tokens: messages.skills,
        skill_results: messages.skill_results,
        builtin_definitions: tools.builtin_definitions,
        system_unattributed: tools.system_unattributed,
        mcp_definitions: tools.mcp_definitions,
    }
}

fn account_system(system: &str) -> (u32, u32) {
    let total = estimate_tokens_cached(system);
    let Some(memory_prompt) = memory::prompt_tag_line_range(system) else {
        return (total, 0);
    };
    let mut without_memory =
        String::with_capacity(system.len().saturating_sub(memory_prompt.len()));
    without_memory.push_str(&system[..memory_prompt.start]);
    without_memory.push_str(&system[memory_prompt.end..]);
    let memory = total.saturating_sub(estimate_tokens_cached(&without_memory));
    (total.saturating_sub(memory), memory)
}

fn account_tools(base_tools: &Value, full_tools: &Value) -> ToolAccounting {
    let full_tokens = value_tokens(full_tools);
    let (Some(base), Some(full)) = (base_tools.as_array(), full_tools.as_array()) else {
        return ToolAccounting {
            system: full_tokens,
            ..ToolAccounting::default()
        };
    };
    let own_names: HashSet<_> = base.iter().filter_map(tool_name).collect();
    let mut contributions = full
        .iter()
        .map(|definition| {
            let name = tool_name(definition).unwrap_or_default();
            let category = match name {
                TASK_TOOL_NAME => ToolCategory::Profiles,
                MEMORY_TOOL_NAME => ToolCategory::Memory,
                SKILL_TOOL_NAME => ToolCategory::Skills,
                _ if own_names.contains(name) => ToolCategory::System,
                TOOL_SEARCH_TOOL_NAME => ToolCategory::System,
                _ => ToolCategory::Mcp,
            };
            ToolContribution {
                name: name.to_owned(),
                category,
                tokens: value_tokens(definition),
            }
        })
        .collect::<Vec<_>>();
    normalize_tool_contributions(&mut contributions, full_tokens);

    let mut accounting = ToolAccounting::default();
    for contribution in contributions {
        if let Some(billed_to) = contribution.category.billed_to() {
            accounting.builtin_definitions.push(BuiltinDefinition {
                name: contribution.name.clone(),
                tokens: contribution.tokens,
                billed_to: Some(billed_to),
                catalog: false,
            });
        }
        match contribution.category {
            ToolCategory::System => {
                add_tokens(&mut accounting.system, contribution.tokens);
                accounting.builtin_definitions.push(BuiltinDefinition {
                    catalog: contribution.name == TOOL_SEARCH_TOOL_NAME
                        && !own_names.contains(contribution.name.as_str()),
                    name: contribution.name,
                    tokens: contribution.tokens,
                    billed_to: None,
                });
            }
            ToolCategory::Mcp => {
                add_tokens(&mut accounting.mcp, contribution.tokens);
                accounting
                    .mcp_definitions
                    .push((contribution.name, contribution.tokens));
            }
            ToolCategory::Profiles => add_tokens(&mut accounting.profiles, contribution.tokens),
            ToolCategory::Memory => add_tokens(&mut accounting.memory, contribution.tokens),
            ToolCategory::Skills => add_tokens(&mut accounting.skills, contribution.tokens),
        }
    }
    let definitions = accounting
        .system
        .saturating_add(accounting.mcp)
        .saturating_add(accounting.profiles)
        .saturating_add(accounting.memory)
        .saturating_add(accounting.skills);
    accounting.system_unattributed = full_tokens.saturating_sub(definitions);
    add_tokens(&mut accounting.system, accounting.system_unattributed);
    accounting
}

fn account_messages(messages: &[Message]) -> MessageAccounting {
    let mut accounting = MessageAccounting {
        messages: estimate_message_tokens(messages),
        ..MessageAccounting::default()
    };
    let mut preceding_loads = HashMap::<&str, LoadedBody<'_>>::new();
    for message in messages {
        if matches!(message.role, Role::User) {
            for block in &message.content {
                let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } = block
                else {
                    continue;
                };
                let Some(load) = preceding_loads.remove(tool_use_id.as_str()) else {
                    continue;
                };
                if *is_error {
                    continue;
                }
                let body_tokens = estimate_tokens_cached(content).min(accounting.messages);
                accounting.messages -= body_tokens;
                match load {
                    LoadedBody::Memory => add_tokens(&mut accounting.memory, body_tokens),
                    LoadedBody::Skill(name) => {
                        add_tokens(&mut accounting.skills, body_tokens);
                        let loaded = accounting.skill_results.entry(name.to_owned()).or_default();
                        add_tokens(loaded, body_tokens);
                    }
                }
            }
        }

        preceding_loads = if matches!(message.role, Role::Assistant) {
            message
                .content
                .iter()
                .filter_map(|block| {
                    let ContentBlock::ToolUse {
                        id, name, input, ..
                    } = block
                    else {
                        return None;
                    };
                    loaded_body(name, input).map(|load| (id.as_str(), load))
                })
                .collect()
        } else {
            HashMap::new()
        };
    }
    accounting
}

fn loaded_body<'a>(tool_name: &str, input: &'a Value) -> Option<LoadedBody<'a>> {
    match tool_name {
        MEMORY_TOOL_NAME
            if input.get("command").and_then(Value::as_str) == Some(MEMORY_READ_COMMAND) =>
        {
            Some(LoadedBody::Memory)
        }
        SKILL_TOOL_NAME => input
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(LoadedBody::Skill),
        _ => None,
    }
}

impl ContextInventory {
    fn apply_accounting(&mut self, accounting: &RequestAccounting) {
        self.profiles.request_tokens = accounting.usage.profiles;
        self.memory.definition_tokens = accounting.memory_definition_tokens;
        self.memory.prompt_tokens = accounting.memory_prompt_tokens;
        self.memory.loaded_tokens = accounting.memory_result_tokens;
        self.skills.definition_tokens = accounting.skill_definition_tokens;
        self.skills.loaded_tokens = accounting.skill_result_tokens;
        for skill in &mut self.skills.skills {
            skill.loaded_tokens = accounting
                .skill_results
                .iter()
                .filter(|(loaded, _)| {
                    **loaded == skill.name
                        || skill::split_address(loaded)
                            .is_some_and(|(parent, _)| parent == skill.name)
                })
                .fold(0, |total, (_, tokens)| total.saturating_add(*tokens));
        }
        self.builtins.apply_request_tokens(
            &accounting.builtin_definitions,
            accounting.system_unattributed,
        );
        self.mcp.apply_request_tokens(&accounting.mcp_definitions);
    }
}

impl ContextMcpInventory {
    fn apply_request_tokens(&mut self, definitions: &[(String, u32)]) {
        self.unattributed_tokens = 0;
        for tool in &mut self.tools {
            tool.request_tokens = 0;
        }

        for (name, tokens) in definitions {
            if let Some(tool) = self.tools.iter_mut().find(|tool| tool.wire_name == *name) {
                add_tokens(&mut tool.request_tokens, *tokens);
            } else {
                add_tokens(&mut self.unattributed_tokens, *tokens);
            }
        }
    }
}

impl ContextBuiltinInventory {
    /// The registry says which tools exist and why; only the request knows
    /// what they cost. A name the array actually carries is declared with its
    /// measured tokens, whatever the filter predicted.
    fn apply_request_tokens(&mut self, definitions: &[BuiltinDefinition], unattributed: u32) {
        self.catalog_tokens = 0;
        self.unattributed_tokens = unattributed;

        for tool in &mut self.tools {
            if tool.state == ContextBuiltinState::Declared {
                tool.state = ContextBuiltinState::Disabled;
                tool.reason = Some(REQUEST_UNAVAILABLE);
                tool.tokens = 0;
                tool.billed_to = None;
            }
        }

        for definition in definitions {
            let BuiltinDefinition {
                name,
                tokens,
                billed_to,
                catalog,
            } = definition;
            match self.tools.iter_mut().find(|tool| tool.name == *name) {
                Some(tool) => {
                    let reason = tool.reason;
                    let deferred = tool.state == ContextBuiltinState::Deferred;
                    tool.state = ContextBuiltinState::Declared;
                    tool.reason = match reason {
                        Some(crate::tools::profile_policy::PROFILE_LOADING) if deferred => {
                            Some(crate::tools::report::REASON_PROFILE_LOADED)
                        }
                        Some(
                            crate::tools::profile_policy::PROFILE_LOADING
                            | crate::tools::profile_policy::REQUIRED_INFRASTRUCTURE,
                        ) => reason,
                        _ => None,
                    };
                    tool.tokens = *tokens;
                    tool.billed_to = *billed_to;
                }
                None if *catalog => add_tokens(&mut self.catalog_tokens, *tokens),
                None => self.tools.push(ContextBuiltinTool {
                    name: name.clone(),
                    source: String::new(),
                    state: ContextBuiltinState::Declared,
                    reason: None,
                    tokens: *tokens,
                    billed_to: *billed_to,
                }),
            }
        }
        self.tools.sort_by(|left, right| {
            left.state
                .cmp(&right.state)
                .then_with(|| left.name.cmp(&right.name))
        });
    }
}

/// Reconciles per-definition estimates with what the array actually costs.
///
/// Measured apart, definitions cost more than the array holding them, because
/// the tokenizer merges across the `},{` boundaries. The difference is an
/// artifact of how every row was measured, not a debt any one row owes, so it
/// comes off all of them in proportion; charging it to whichever row happens to
/// sit last would grow with the tool count until that row read zero.
fn normalize_tool_contributions(contributions: &mut [ToolContribution], budget: u32) {
    let total = contributions
        .iter()
        .map(|contribution| contribution.tokens)
        .fold(0, u32::saturating_add);
    if total <= budget {
        return;
    }
    let excess = u64::from(total - budget);
    let total = u64::from(total);

    let mut placed = 0;
    let mut remainders = Vec::with_capacity(contributions.len());
    for (index, contribution) in contributions.iter_mut().enumerate() {
        let scaled = u64::from(contribution.tokens) * excess;
        let share = (scaled / total) as u32;
        contribution.tokens -= share;
        placed += u64::from(share);
        remainders.push((scaled % total, index));
    }

    // Integer division leaves fewer tokens over than there are rows, and a row
    // only has a remainder if it kept a token to give: the largest ones have
    // the strongest claim to what is left.
    remainders.sort_unstable_by_key(|&(remainder, _)| Reverse(remainder));
    let mut leftover = excess - placed;
    for (_, index) in remainders {
        if leftover == 0 {
            break;
        }
        let tokens = &mut contributions[index].tokens;
        *tokens = tokens.saturating_sub(1);
        leftover -= 1;
    }
}

fn tool_name(tool: &Value) -> Option<&str> {
    tool.get("name").and_then(Value::as_str)
}

fn value_tokens(value: &Value) -> u32 {
    estimate_tokens_cached(&value.to_string())
}

fn add_tokens(total: &mut u32, tokens: u32) {
    *total = total.saturating_add(tokens);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use caudra_providers::{
        ImageMediaType, ImageSource, MessageKind, ReasoningSource, ReasoningTransport,
        ResponsesReasoning, Role,
    };
    use caudra_storage::id::CaudraId;
    use caudra_storage::tool_outputs::ToolOutputRef;
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const MODEL_SPEC: &str = "anthropic/claude-sonnet-4-6";
    const SYSTEM_TEXT: &str = "system policy";
    const MEMORY_PROMPT: &str =
        "\n\nMemory tags (`memory` with `command=\"read\"` and `tags=[...]`): project\n";
    const USER_TEXT: &str = "finish the requested change";
    const MEMORY_RESULT: &str = "remember this project convention";
    const SKILL_RESULT: &str = "follow this deployment workflow";
    const PRUNED_RESULT: &str = "[Old tool result pruned. Full output ID: out_123.]";
    const FULL_RESULT: &str = "the complete result that was replaced in projection";
    const EARLY_RESULT: &str = "result before its tool use";
    const TASK_A: &str = "task-a";
    const TASK_B: &str = "task-b";
    const LOADED_MCP: &str = "issues__fetch";
    const DEFERRED_MCP: &str = "search__query";
    const DISABLED_MCP: &str = "admin__delete";
    const TEST_WINDOW: u32 = 200_000;
    const LONG_METADATA_REPETITIONS: usize = 2_048;
    const LARGE_IMAGE_PAYLOAD_BYTES: usize = 256 * 1_024;
    const SMALL_CALL_COUNT: usize = 192;
    const LOAD_CALL_ID: &str = "load-call";
    const MISSING_INVENTORY_DIR: &str = "/nonexistent/caudra-context-inventory";
    const SKILL_NAME: &str = "deploy";
    const CROWDED_ROWS: u32 = 60;
    const CROWDED_ROW_TOKENS: u32 = 20;
    const CROWDED_EXCESS: u32 = 30;
    const TASK_CONTROL_NAME: &str = "task_control";
    const CONFIG_DISABLED_REASON: &str = "disabled in configuration";
    const CUSTOM_SEARCH_SOURCE: &str = "plugin:custom-search";
    const SEARCH_DESCRIPTION: &str = "Search available information";

    fn model(context_window: u32, window_excludes_output: bool) -> Model {
        let mut model = Model::from_spec(MODEL_SPEC).unwrap();
        model.context_window = context_window;
        model.window_excludes_output = window_excludes_output;
        model
    }

    fn definition(name: &str, description: &str) -> Value {
        json!({
            "name": name,
            "description": description,
            "input_schema": { "type": "object" }
        })
    }

    /// What measuring the definitions apart overcounts the array by, and so the
    /// most any one row can be reconciled down.
    fn reconciliation_slack(tools: &Value) -> u32 {
        let measured = tools
            .as_array()
            .unwrap()
            .iter()
            .map(value_tokens)
            .fold(0, u32::saturating_add);
        measured.saturating_sub(value_tokens(tools))
    }

    /// A row is never charged more than it measures alone, and never discounted
    /// by more than the whole array was overcounted by.
    fn assert_reconciled(charged: u32, definition: &Value, slack: u32) {
        let measured = value_tokens(definition);
        let name = tool_name(definition).unwrap_or_default();
        assert!(
            charged <= measured && measured - charged <= slack,
            "{name} was charged {charged} against the {measured} it measures alone, \
             beyond the {slack} of reconciliation"
        );
    }

    fn mcp_status(
        qualified_name: &str,
        wire_name: &str,
        disabled: bool,
        deferred: bool,
    ) -> McpToolStatus {
        McpToolStatus {
            qualified_name: qualified_name.to_owned(),
            wire_name: wire_name.to_owned(),
            server: qualified_name.split_once('.').unwrap().0.to_owned(),
            disabled,
            deferred,
            reason: None,
        }
    }

    fn messages() -> Vec<Message> {
        vec![
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::tool_use(
                        "memory-use",
                        MEMORY_TOOL_NAME,
                        json!({ "command": MEMORY_READ_COMMAND, "path": "project.md" }),
                    ),
                    ContentBlock::tool_use(
                        "skill-use",
                        SKILL_TOOL_NAME,
                        json!({ "name": SKILL_NAME }),
                    ),
                ],
                ..Message::default()
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "memory-use".into(),
                        content: MEMORY_RESULT.into(),
                        is_error: false,
                        output_ref: None,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "skill-use".into(),
                        content: SKILL_RESULT.into(),
                        is_error: false,
                        output_ref: None,
                    },
                    ContentBlock::Text {
                        text: USER_TEXT.into(),
                    },
                ],
                ..Message::default()
            },
        ]
    }

    fn loading_exchange(
        tool_name: &str,
        input: Value,
        result: &str,
        is_error: bool,
        stale: bool,
    ) -> Vec<Message> {
        let mut messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(LOAD_CALL_ID, tool_name, input)],
            ..Message::default()
        }];
        if stale {
            messages.push(Message::user("intervening turn".into()));
        }
        messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: LOAD_CALL_ID.into(),
                content: result.into(),
                is_error,
                output_ref: None,
            }],
            ..Message::default()
        });
        messages
    }

    #[test_case(None ; "no_active_profile")]
    #[test_case(Some(BUILTIN_PROFILE_NAME) ; "explicit_builtin_profile")]
    fn profile_inventory_marks_only_an_explicit_active_profile(active_profile: Option<&str>) {
        let model = model(TEST_WINDOW, false);
        let profiles = PromptProfileCatalog::default();
        let task_profiles = profiles.bind_for_tasks(
            &model,
            &model,
            &caudra_providers::ThinkingConfig::default(),
            &caudra_config::ModelPolicy::default(),
            caudra_providers::Timeouts::default(),
        );
        let inventory = ContextInventory::collect(
            Path::new(MISSING_INVENTORY_DIR),
            &ToolRegistry::new(),
            &profiles,
            &task_profiles,
            active_profile,
            None,
            None,
        );
        let active = inventory
            .profiles
            .profiles
            .iter()
            .filter(|profile| profile.active)
            .map(|profile| profile.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(active, active_profile.into_iter().collect::<Vec<_>>());
    }

    #[test_case(ContextBuiltinState::Declared, false, ContextBuiltinState::Disabled, Some(REQUEST_UNAVAILABLE); "capability_removed_control")]
    #[test_case(ContextBuiltinState::Deferred, false, ContextBuiltinState::Deferred, Some(CONFIG_DISABLED_REASON); "deferred_definition_kept")]
    #[test_case(ContextBuiltinState::Disabled, false, ContextBuiltinState::Disabled, Some(CONFIG_DISABLED_REASON); "disabled_reason_kept")]
    #[test_case(ContextBuiltinState::Declared, true, ContextBuiltinState::Declared, None; "declared_definition")]
    #[test_case(ContextBuiltinState::Deferred, true, ContextBuiltinState::Declared, None; "loaded_deferred_definition")]
    #[test_case(ContextBuiltinState::Disabled, true, ContextBuiltinState::Declared, None; "request_overrides_prediction")]
    fn builtin_availability_follows_actual_request(
        initial: ContextBuiltinState,
        present: bool,
        expected: ContextBuiltinState,
        reason: Option<&'static str>,
    ) {
        let mut inventory = ContextBuiltinInventory {
            tools: vec![ContextBuiltinTool {
                name: TASK_CONTROL_NAME.into(),
                source: String::new(),
                state: initial,
                reason: Some(CONFIG_DISABLED_REASON),
                tokens: CROWDED_ROW_TOKENS,
                billed_to: None,
            }],
            ..ContextBuiltinInventory::default()
        };
        let definitions = if present {
            vec![BuiltinDefinition {
                name: TASK_CONTROL_NAME.into(),
                tokens: CROWDED_EXCESS,
                billed_to: None,
                catalog: false,
            }]
        } else {
            Vec::new()
        };

        inventory.apply_request_tokens(&definitions, 0);

        let tool = &inventory.tools[0];
        assert_eq!(tool.state, expected);
        assert_eq!(tool.reason, reason);
        assert_eq!(
            tool.tokens,
            if present {
                CROWDED_EXCESS
            } else if initial == ContextBuiltinState::Declared {
                0
            } else {
                CROWDED_ROW_TOKENS
            }
        );
    }

    #[test_case(Some(ContextBuiltinState::Declared), true; "registered_eager")]
    #[test_case(Some(ContextBuiltinState::Deferred), true; "registered_lazy")]
    #[test_case(None, true; "local_binding")]
    #[test_case(None, false; "native_catalog")]
    fn tool_search_tokens_keep_binding_identity(initial: Option<ContextBuiltinState>, bound: bool) {
        let full = json!([definition(TOOL_SEARCH_TOOL_NAME, SEARCH_DESCRIPTION)]);
        let sources = if bound { full.clone() } else { json!([]) };
        let accounting = account_tools(&sources, &full);
        let mut inventory = ContextBuiltinInventory {
            tools: initial
                .into_iter()
                .map(|state| ContextBuiltinTool {
                    name: TOOL_SEARCH_TOOL_NAME.into(),
                    source: CUSTOM_SEARCH_SOURCE.into(),
                    state,
                    reason: None,
                    tokens: 0,
                    billed_to: None,
                })
                .collect(),
            ..ContextBuiltinInventory::default()
        };
        inventory.apply_request_tokens(
            &accounting.builtin_definitions,
            accounting.system_unattributed,
        );
        assert_eq!(inventory.request_tokens(), accounting.system);
        if bound {
            assert_eq!(inventory.catalog_tokens, 0);
            assert_eq!(inventory.tools.len(), 1);
            let tool = &inventory.tools[0];
            assert_eq!(tool.name, TOOL_SEARCH_TOOL_NAME);
            assert_eq!(tool.state, ContextBuiltinState::Declared);
            assert_eq!(tool.tokens, value_tokens(&full[0]));
            assert_eq!(
                tool.source,
                if initial.is_some() {
                    CUSTOM_SEARCH_SOURCE
                } else {
                    ""
                },
            );
        } else {
            assert!(inventory.tools.is_empty());
            assert!(inventory.catalog_tokens > 0);
            assert_eq!(inventory.catalog_tokens, value_tokens(&full[0]));
        }
    }

    #[test]
    fn request_accounting_is_exclusive_and_tracks_inventory_contributions() {
        let model = model(TEST_WINDOW, false);
        let base_tools = json!([
            definition("file_read", "Read a file"),
            definition(TASK_TOOL_NAME, "Profiles: builtin and review"),
            definition(MEMORY_TOOL_NAME, "Read memory on demand"),
            definition(SKILL_TOOL_NAME, "Skills: deploy")
        ]);
        let mut full_tools = base_tools.clone();
        full_tools.as_array_mut().unwrap().extend([
            definition(LOADED_MCP, "Fetch an issue"),
            definition(TOOL_SEARCH_TOOL_NAME, "Deferred tools:\nsearch: query"),
        ]);
        let inventory = ContextInventory {
            memory: ContextMemoryInventory {
                files: vec![ContextMemoryFile {
                    name: "project.md".into(),
                    tags: vec!["project".into()],
                    on_load_tokens: u32::MAX,
                }],
                ..ContextMemoryInventory::default()
            },
            skills: ContextSkillInventory {
                skills: vec![ContextSkill {
                    name: "deploy".into(),
                    description: "Deploy safely".into(),
                    loaded_tokens: 0,
                }],
                ..ContextSkillInventory::default()
            },
            mcp: ContextMcpInventory::from_statuses(vec![
                mcp_status("issues.fetch", LOADED_MCP, false, false),
                mcp_status("search.query", DEFERRED_MCP, false, true),
                mcp_status("admin.delete", DISABLED_MCP, true, false),
            ]),
            ..ContextInventory::default()
        };
        let system = format!("{SYSTEM_TEXT}{MEMORY_PROMPT}");
        let messages = messages();
        let snapshot = ContextSnapshot::capture(ContextCapture {
            readiness: ContextReadiness::CapturedCurrentRequest,
            mode: &AgentMode::Build,
            audience: ToolAudience::MAIN,
            model: &model,
            auto_compact: true,
            compaction_buffer: None,
            system: &system,
            base_tools: &base_tools,
            full_tools: &full_tools,
            projected_messages: &messages,
            measured: None,
            inventory,
        });

        let message_tokens = estimate_message_tokens(&messages);
        let expected = estimate_tokens_cached(&system)
            .saturating_add(value_tokens(&full_tools))
            .saturating_add(message_tokens);
        assert_eq!(snapshot.usage.used(), expected);
        assert_eq!(
            snapshot.usage.system_prompt,
            estimate_tokens_cached(SYSTEM_TEXT)
        );
        assert_eq!(
            snapshot
                .usage
                .system_prompt
                .saturating_add(snapshot.inventory.memory.prompt_tokens),
            estimate_tokens_cached(&system)
        );
        assert_eq!(
            snapshot
                .usage
                .system_tools
                .saturating_add(snapshot.usage.mcp_tools)
                .saturating_add(snapshot.usage.profiles)
                .saturating_add(snapshot.inventory.memory.definition_tokens)
                .saturating_add(snapshot.inventory.skills.definition_tokens),
            value_tokens(&full_tools)
        );
        let slack = reconciliation_slack(&full_tools);
        assert_reconciled(snapshot.usage.profiles, &base_tools[1], slack);
        assert_reconciled(
            snapshot.inventory.memory.definition_tokens,
            &base_tools[2],
            slack,
        );
        assert_reconciled(
            snapshot.inventory.skills.definition_tokens,
            &base_tools[3],
            slack,
        );
        assert_reconciled(snapshot.usage.mcp_tools, &full_tools[4], slack);
        assert_reconciled(
            snapshot.inventory.builtins.catalog_tokens,
            &full_tools[5],
            slack,
        );
        assert!(
            snapshot.inventory.builtins.catalog_tokens > 0,
            "the shared catalog is ours, not a server's"
        );
        assert_eq!(
            snapshot.inventory.builtins.request_tokens(),
            snapshot.usage.system_tools
        );
        let declared = snapshot
            .inventory
            .builtins
            .tools
            .iter()
            .find(|tool| tool.name == "file_read")
            .expect("declared built-in is inventoried");
        assert_eq!(declared.state, ContextBuiltinState::Declared);
        assert!(declared.tokens > 0);
        // `task` costs real tokens the profiles row already pays for, so the
        // built-in row reports them without adding them a second time.
        let billed = snapshot
            .inventory
            .builtins
            .tools
            .iter()
            .find(|tool| tool.name == TASK_TOOL_NAME)
            .expect("a built-in billed elsewhere is still inventoried");
        assert_eq!(billed.billed_to, Some(BILLED_TO_PROFILES));
        assert_eq!(billed.tokens, snapshot.usage.profiles);
        assert_eq!(
            snapshot
                .usage
                .messages
                .saturating_add(snapshot.inventory.memory.loaded_tokens)
                .saturating_add(snapshot.inventory.skills.loaded_tokens),
            message_tokens
        );
        assert!(snapshot.usage.system_prompt > 0);
        assert!(snapshot.usage.system_tools > 0);
        assert!(snapshot.usage.profiles > 0);
        assert_eq!(
            snapshot.inventory.memory.request_tokens(),
            snapshot.usage.memory
        );
        assert_eq!(
            snapshot.inventory.skills.request_tokens(),
            snapshot.usage.skills
        );
        assert_eq!(
            snapshot.inventory.memory.loaded_tokens,
            estimate_tokens_cached(MEMORY_RESULT)
        );
        assert_eq!(
            snapshot.inventory.skills.skills[0].loaded_tokens,
            estimate_tokens_cached(SKILL_RESULT)
        );
        assert_eq!(
            snapshot.inventory.mcp.request_tokens(),
            snapshot.usage.mcp_tools
        );
        assert_eq!(snapshot.inventory.mcp.unattributed_tokens, 0);
        assert!(snapshot.inventory.builtins.catalog_tokens > 0);
        let loaded = snapshot
            .inventory
            .mcp
            .tools
            .iter()
            .find(|tool| tool.wire_name == LOADED_MCP)
            .unwrap();
        let deferred = snapshot
            .inventory
            .mcp
            .tools
            .iter()
            .find(|tool| tool.wire_name == DEFERRED_MCP)
            .unwrap();
        let disabled = snapshot
            .inventory
            .mcp
            .tools
            .iter()
            .find(|tool| tool.wire_name == DISABLED_MCP)
            .unwrap();
        assert!(loaded.request_tokens > 0);
        assert_eq!(deferred.request_tokens, 0);
        assert_eq!(disabled.request_tokens, 0);
    }

    #[test_case("file_read"; "native")]
    #[test_case("plugin_lookup"; "plugin")]
    #[test_case("local_lookup"; "local")]
    fn lazy_own_definitions_keep_source_attribution(name: &str) {
        let own_definition = definition(name, "Deferred lookup");
        let sources = json!([own_definition]);
        let full = json!([own_definition, definition(LOADED_MCP, "MCP lookup")]);
        let accounted = account_tools(&sources, &full);
        assert_eq!(accounted.builtin_definitions[0].name, name);
        assert_eq!(accounted.mcp_definitions.len(), 1);
        assert_eq!(accounted.mcp_definitions[0].0, LOADED_MCP);
        assert_eq!(accounted.system + accounted.mcp, value_tokens(&full));
    }

    /// The reconciliation grows with the tool count, so a request carrying
    /// enough of them used to drain whichever row the loop reached first down
    /// to nothing and start on the next.
    #[test]
    fn reconciliation_comes_off_every_row_instead_of_emptying_the_last() {
        let budget = CROWDED_ROWS * CROWDED_ROW_TOKENS - CROWDED_EXCESS;
        let mut contributions = (0..CROWDED_ROWS)
            .map(|index| ToolContribution {
                name: index.to_string(),
                category: ToolCategory::System,
                tokens: CROWDED_ROW_TOKENS,
            })
            .collect::<Vec<_>>();

        normalize_tool_contributions(&mut contributions, budget);

        let total = contributions
            .iter()
            .map(|contribution| contribution.tokens)
            .fold(0, u32::saturating_add);
        assert_eq!(total, budget);
        let smallest = contributions
            .iter()
            .map(|contribution| contribution.tokens)
            .min()
            .unwrap();
        assert_eq!(smallest, CROWDED_ROW_TOKENS - 1);
    }

    #[test_case(MEMORY_TOOL_NAME, FULL_RESULT, PRUNED_RESULT ; "memory_result")]
    #[test_case(SKILL_TOOL_NAME, FULL_RESULT, PRUNED_RESULT ; "skill_result")]
    fn projected_tool_results_use_the_projected_content(
        tool_name: &str,
        full: &str,
        projected: &str,
    ) {
        let model = model(TEST_WINDOW, false);
        let input = if tool_name == MEMORY_TOOL_NAME {
            json!({ "command": MEMORY_READ_COMMAND, "path": "project.md" })
        } else {
            json!({ "name": SKILL_NAME })
        };
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("call", tool_name, input.clone())],
                ..Message::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call".into(),
                    content: projected.into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Message::default()
            },
        ];
        let usage = estimate_context_usage(&model, "", &json!([]), &json!([]), &messages);
        let special = if tool_name == MEMORY_TOOL_NAME {
            usage.memory
        } else {
            usage.skills
        };
        assert_eq!(special, estimate_tokens_cached(projected));
        assert_ne!(special, estimate_tokens_cached(full));
        assert_eq!(
            usage.messages.saturating_add(special),
            estimate_message_tokens(&messages)
        );
        assert_eq!(
            usage.messages,
            estimate_message_tokens(&messages).saturating_sub(special)
        );
    }

    #[test]
    fn only_results_with_a_preceding_special_tool_use_are_reclassified() {
        let model = model(TEST_WINDOW, false);
        let messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call".into(),
                    content: EARLY_RESULT.into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Message::default()
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    "call",
                    MEMORY_TOOL_NAME,
                    json!({ "command": "read" }),
                )],
                ..Message::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call".into(),
                    content: MEMORY_RESULT.into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Message::default()
            },
        ];
        let usage = estimate_context_usage(&model, "", &json!([]), &json!([]), &messages);
        assert_eq!(usage.memory, estimate_tokens_cached(MEMORY_RESULT));
        assert!(usage.messages >= estimate_tokens_cached(EARLY_RESULT));
        assert_eq!(
            usage.messages.saturating_add(usage.memory),
            estimate_message_tokens(&messages)
        );
    }

    #[test_case(MEMORY_READ_COMMAND, false, false, true ; "successful_immediate_read")]
    #[test_case("list", false, false, false ; "list_is_not_a_load")]
    #[test_case("write", false, false, false ; "write_is_not_a_load")]
    #[test_case("delete", false, false, false ; "delete_is_not_a_load")]
    #[test_case(MEMORY_READ_COMMAND, true, false, false ; "read_error")]
    #[test_case(MEMORY_READ_COMMAND, false, true, false ; "stale_read_id")]
    #[test_case("", false, false, false ; "missing_read_command")]
    fn memory_results_only_reclassify_successful_immediate_reads(
        command: &str,
        is_error: bool,
        stale: bool,
        loaded: bool,
    ) {
        let messages = loading_exchange(
            MEMORY_TOOL_NAME,
            json!({ "command": command, "path": "project.md" }),
            MEMORY_RESULT,
            is_error,
            stale,
        );
        let total = estimate_message_tokens(&messages);
        let body_tokens = if loaded {
            estimate_tokens_cached(MEMORY_RESULT)
        } else {
            0
        };
        let usage = estimate_context_usage(
            &model(TEST_WINDOW, false),
            "",
            &json!([]),
            &json!([]),
            &messages,
        );

        assert_eq!(usage.memory, body_tokens);
        assert_eq!(usage.skills, 0);
        assert_eq!(usage.messages, total.saturating_sub(body_tokens));
        assert_eq!(
            usage
                .messages
                .saturating_add(usage.memory)
                .saturating_add(usage.skills),
            total
        );
    }

    #[test_case(MEMORY_TOOL_NAME, json!({ "command": MEMORY_READ_COMMAND }) ; "memory")]
    #[test_case(SKILL_TOOL_NAME, json!({ "name": SKILL_NAME }) ; "skill")]
    fn failed_result_consumes_the_pair_before_a_duplicate_success(tool_name: &str, input: Value) {
        let messages = [
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(LOAD_CALL_ID, tool_name, input)],
                ..Message::default()
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: LOAD_CALL_ID.into(),
                        content: "failed".into(),
                        is_error: true,
                        output_ref: None,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: LOAD_CALL_ID.into(),
                        content: "stale success".into(),
                        is_error: false,
                        output_ref: None,
                    },
                ],
                ..Message::default()
            },
        ];
        let accounting = account_messages(&messages);

        assert_eq!(accounting.memory, 0);
        assert_eq!(accounting.skills, 0);
        assert!(accounting.skill_results.is_empty());
        assert_eq!(accounting.messages, estimate_message_tokens(&messages));
    }

    #[test_case(true, false, false, true ; "successful_immediate_invocation")]
    #[test_case(true, true, false, false ; "skill_error")]
    #[test_case(true, false, true, false ; "stale_skill_id")]
    #[test_case(false, false, false, false ; "missing_skill_name")]
    fn skill_results_only_reclassify_successful_immediate_invocations(
        has_name: bool,
        is_error: bool,
        stale: bool,
        loaded: bool,
    ) {
        let input = if has_name {
            json!({ "name": SKILL_NAME })
        } else {
            json!({})
        };
        let messages = loading_exchange(SKILL_TOOL_NAME, input, SKILL_RESULT, is_error, stale);
        let total = estimate_message_tokens(&messages);
        let body_tokens = if loaded {
            estimate_tokens_cached(SKILL_RESULT)
        } else {
            0
        };
        let accounting = account_messages(&messages);

        assert_eq!(accounting.memory, 0);
        assert_eq!(accounting.skills, body_tokens);
        assert_eq!(accounting.messages, total.saturating_sub(body_tokens));
        assert_eq!(
            accounting.skill_results.get(SKILL_NAME).copied(),
            loaded.then_some(body_tokens)
        );
        assert_eq!(
            accounting
                .messages
                .saturating_add(accounting.memory)
                .saturating_add(accounting.skills),
            total
        );
    }

    #[test]
    fn page_and_search_loads_are_charged_to_their_skill() {
        const DOCS_SKILL: &str = "caudra-docs";
        let mut inventory = ContextInventory {
            skills: ContextSkillInventory {
                skills: [DOCS_SKILL, SKILL_NAME]
                    .map(|name| ContextSkill {
                        name: name.into(),
                        description: String::new(),
                        loaded_tokens: 0,
                    })
                    .into(),
                ..ContextSkillInventory::default()
            },
            ..ContextInventory::default()
        };
        let accounting = RequestAccounting {
            skill_results: BTreeMap::from([
                (DOCS_SKILL.to_owned(), 1),
                (format!("{DOCS_SKILL}/tools#shell"), 2),
                (format!("{DOCS_SKILL}?shell timeout"), 4),
                (format!("{DOCS_SKILL}-other"), 8),
                (SKILL_NAME.to_owned(), 16),
            ]),
            ..RequestAccounting::default()
        };
        inventory.apply_accounting(&accounting);
        let loaded: Vec<u32> = inventory
            .skills
            .skills
            .iter()
            .map(|skill| skill.loaded_tokens)
            .collect();
        assert_eq!(loaded, [1 + 2 + 4, 16]);
    }

    #[test]
    fn message_accounting_includes_replay_metadata_and_framing() {
        let long = |label: &str| {
            format!(
                "{label} {}",
                "provider replay metadata ".repeat(LONG_METADATA_REPETITIONS)
            )
        };
        let thinking = long("thinking");
        let thinking_signature = long("thinking-signature");
        let responses_item_id = long("responses-item");
        let encrypted_content = long("encrypted-reasoning");
        let redacted_thinking = long("redacted-thinking");
        let tool_use_id = long("tool-use-id");
        let tool_name = long("tool-name");
        let tool_thought_signature = long("tool-thought-signature");
        let tool_input = json!({ "payload": long("tool-input") });
        let tool_result = long("tool-result");
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: thinking.clone(),
                        signature: Some(thinking_signature.clone()),
                        duration_ms: None,
                        interrupted: false,
                        responses: Some(ResponsesReasoning {
                            item_id: responses_item_id.clone(),
                            encrypted_content: Some(encrypted_content.clone()),
                        }),
                    },
                    ContentBlock::RedactedThinking {
                        data: redacted_thinking.clone(),
                    },
                    ContentBlock::ToolUse {
                        id: tool_use_id.clone(),
                        name: tool_name.clone(),
                        input: tool_input.clone(),
                        thought_signature: Some(tool_thought_signature.clone()),
                    },
                ],
                ..Message::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: tool_result.clone(),
                    is_error: true,
                    output_ref: None,
                }],
                ..Message::default()
            },
        ];
        let provider_fields = [
            thinking.as_str(),
            thinking_signature.as_str(),
            responses_item_id.as_str(),
            encrypted_content.as_str(),
            redacted_thinking.as_str(),
            tool_use_id.as_str(),
            tool_name.as_str(),
            tool_thought_signature.as_str(),
            tool_use_id.as_str(),
            tool_result.as_str(),
        ]
        .into_iter()
        .map(estimate_tokens_cached)
        .chain([estimate_tokens_cached(&tool_input.to_string())])
        .fold(0, u32::saturating_add);
        let total = estimate_message_tokens(&messages);
        let usage = estimate_context_usage(
            &model(TEST_WINDOW, false),
            "",
            &json!([]),
            &json!([]),
            &messages,
        );

        assert!(total > provider_fields);
        assert_eq!(usage.messages, total);
        assert_eq!(usage.memory, 0);
        assert_eq!(usage.skills, 0);

        let ContentBlock::ToolResult { is_error, .. } = &mut messages[1].content[0] else {
            unreachable!();
        };
        *is_error = false;
        assert!(total > estimate_message_tokens(&messages));
    }

    #[test]
    fn host_only_display_reasoning_and_retention_metadata_is_excluded() {
        let mut message = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "provider-visible reasoning".into(),
                    signature: None,
                    duration_ms: None,
                    interrupted: false,
                    responses: None,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "result-id".into(),
                    content: "provider-visible result".into(),
                    is_error: false,
                    output_ref: None,
                },
            ],
            ..Message::default()
        };
        let baseline = estimate_message_tokens(&[message.clone()]);
        let host_metadata = "host-only metadata ".repeat(LONG_METADATA_REPETITIONS);
        let output_ref = ToolOutputRef {
            id: CaudraId::generate().to_string().parse().unwrap(),
            byte_count: host_metadata.len(),
            line_count: LONG_METADATA_REPETITIONS,
        };

        message.display_text = Some(host_metadata.clone());
        message.kind = MessageKind::Observation;
        message.reasoning_source = Some(ReasoningSource {
            provider: host_metadata.clone(),
            model: host_metadata.clone(),
            transport: ReasoningTransport::Other,
        });
        message.tool_result_image_owners = vec![host_metadata.clone()];
        message.retained_output_refs = vec![output_ref.clone()];
        message.retained_subagent_ids = vec![host_metadata];
        message.is_compaction_summary = true;
        let ContentBlock::Thinking {
            duration_ms,
            interrupted,
            ..
        } = &mut message.content[0]
        else {
            unreachable!();
        };
        *duration_ms = Some(u64::MAX);
        *interrupted = true;
        let ContentBlock::ToolResult {
            output_ref: result_ref,
            ..
        } = &mut message.content[1]
        else {
            unreachable!();
        };
        *result_ref = Some(output_ref);

        assert_eq!(estimate_message_tokens(&[message.clone()]), baseline);
        assert_eq!(
            estimate_context_usage(
                &model(TEST_WINDOW, false),
                "",
                &json!([]),
                &json!([]),
                &[message],
            )
            .messages,
            baseline
        );
    }

    #[test]
    fn image_payload_size_does_not_change_the_token_estimate() {
        let mut model = model(TEST_WINDOW, false);
        model.supports_vision_override = Some(true);
        let image_message = |data: String| Message {
            role: Role::User,
            content: vec![ContentBlock::Image {
                source: ImageSource::new(ImageMediaType::Png, Arc::from(data)),
            }],
            ..Message::default()
        };
        let short = [image_message("!!!!".into())];
        let large = [image_message("!".repeat(LARGE_IMAGE_PAYLOAD_BYTES))];

        assert_eq!(
            estimate_message_tokens(&short),
            estimate_message_tokens(&large)
        );
        assert_eq!(
            estimate_context_usage(&model, "", &json!([]), &json!([]), &short).messages,
            estimate_context_usage(&model, "", &json!([]), &json!([]), &large).messages
        );
    }

    #[test]
    fn many_small_calls_keep_framing_and_categories_exclusive() {
        let mut calls = Vec::with_capacity(SMALL_CALL_COUNT);
        let mut results = Vec::with_capacity(SMALL_CALL_COUNT);
        let mut payload_tokens = 0u32;
        let mut memory_tokens = 0u32;
        let mut skill_tokens = 0u32;
        for index in 0..SMALL_CALL_COUNT {
            let id = format!("small-call-{index}");
            let (name, input) = match index % 3 {
                0 => (
                    MEMORY_TOOL_NAME,
                    json!({ "command": MEMORY_READ_COMMAND, "path": format!("{index}.md") }),
                ),
                1 => (SKILL_TOOL_NAME, json!({ "name": SKILL_NAME })),
                _ => ("shell", json!({ "command": "true" })),
            };
            let result = format!("r{index}");
            add_tokens(
                &mut payload_tokens,
                estimate_tokens_cached(&input.to_string()),
            );
            add_tokens(&mut payload_tokens, estimate_tokens_cached(&result));
            match name {
                MEMORY_TOOL_NAME => {
                    add_tokens(&mut memory_tokens, estimate_tokens_cached(&result));
                }
                SKILL_TOOL_NAME => {
                    add_tokens(&mut skill_tokens, estimate_tokens_cached(&result));
                }
                _ => {}
            }
            calls.push(ContentBlock::tool_use(id.clone(), name, input));
            results.push(ContentBlock::ToolResult {
                tool_use_id: id,
                content: result,
                is_error: false,
                output_ref: None,
            });
        }
        let messages = [
            Message {
                role: Role::Assistant,
                content: calls,
                ..Message::default()
            },
            Message {
                role: Role::User,
                content: results,
                ..Message::default()
            },
        ];
        let total = estimate_message_tokens(&messages);
        let accounting = account_messages(&messages);

        assert!(total > payload_tokens);
        assert_eq!(accounting.memory, memory_tokens);
        assert_eq!(accounting.skills, skill_tokens);
        assert_eq!(
            accounting.skill_results.get(SKILL_NAME),
            Some(&skill_tokens)
        );
        assert_eq!(
            accounting
                .messages
                .saturating_add(accounting.memory)
                .saturating_add(accounting.skills),
            total
        );
    }

    #[test]
    fn available_memory_and_skills_do_not_count_until_loaded() {
        let model = model(TEST_WINDOW, false);
        let inventory = ContextInventory {
            memory: ContextMemoryInventory {
                files: vec![ContextMemoryFile {
                    name: "large.md".into(),
                    tags: Vec::new(),
                    on_load_tokens: u32::MAX,
                }],
                ..ContextMemoryInventory::default()
            },
            skills: ContextSkillInventory {
                skills: vec![ContextSkill {
                    name: "large".into(),
                    description: String::new(),
                    loaded_tokens: 0,
                }],
                ..ContextSkillInventory::default()
            },
            ..ContextInventory::default()
        };
        let empty_tools = json!([]);
        let snapshot = ContextSnapshot::capture(ContextCapture {
            readiness: ContextReadiness::PreparedNextRequest,
            mode: &AgentMode::Build,
            audience: ToolAudience::MAIN,
            model: &model,
            auto_compact: false,
            compaction_buffer: None,
            system: "",
            base_tools: &empty_tools,
            full_tools: &empty_tools,
            projected_messages: &[],
            measured: None,
            inventory,
        });
        assert_eq!(snapshot.usage.memory, 0);
        assert_eq!(snapshot.usage.skills, 0);
        assert_eq!(snapshot.inventory.memory.on_load_tokens(), u32::MAX);
    }

    #[test_case(false, false, None, 200_000, ContextReserve::Disabled ; "disabled")]
    #[test_case(true, false, None, 200_000, ContextReserve::Enabled(40_000) ; "default_total_window")]
    #[test_case(true, true, None, 200_000, ContextReserve::Enabled(20_000) ; "default_input_budget")]
    #[test_case(true, true, Some(CompactionBuffer::Tokens(12_345)), 200_000, ContextReserve::Enabled(12_345) ; "explicit_tokens")]
    #[test_case(true, false, Some(CompactionBuffer::Percent(25)), 200_000, ContextReserve::Enabled(50_000) ; "explicit_percent")]
    #[test_case(true, false, None, 0, ContextReserve::Enabled(0) ; "zero_window")]
    fn reserve_matches_compaction_policy(
        auto_compact: bool,
        window_excludes_output: bool,
        buffer: Option<CompactionBuffer>,
        context_window: u32,
        expected: ContextReserve,
    ) {
        let window = ContextWindow::new(
            &model(context_window, window_excludes_output),
            auto_compact,
            buffer,
        );
        assert_eq!(window.reserve, expected);
    }

    #[test_case(60, 100, ContextReserve::Enabled(20), 80, 20, 0 ; "within_threshold")]
    #[test_case(90, 100, ContextReserve::Enabled(20), 80, 0, 0 ; "inside_window_beyond_threshold")]
    #[test_case(120, 100, ContextReserve::Enabled(20), 80, 0, 20 ; "over_window")]
    #[test_case(10, 0, ContextReserve::Enabled(0), 0, 0, 10 ; "zero_window")]
    fn usage_helpers_saturate(
        used: u32,
        window_tokens: u32,
        reserve: ContextReserve,
        threshold: u32,
        free: u32,
        over_window: u32,
    ) {
        let usage = ContextUsage {
            system_prompt: used,
            ..ContextUsage::default()
        };
        let window = ContextWindow {
            tokens: window_tokens,
            reserve,
        };
        assert_eq!(window.threshold(), threshold);
        assert_eq!(usage.free(&window), free);
        assert_eq!(usage.over_window(&window), over_window);
    }

    #[test]
    fn used_tokens_saturate_instead_of_wrapping() {
        let usage = ContextUsage {
            system_prompt: u32::MAX,
            messages: 1,
            ..ContextUsage::default()
        };
        assert_eq!(usage.used(), u32::MAX);
    }

    fn stored_snapshot(spec: &str, readiness: ContextReadiness) -> ContextSnapshot {
        ContextSnapshot {
            readiness,
            mode: AgentMode::Build,
            audience: ToolAudience::MAIN,
            model: ContextModel {
                spec: spec.to_owned(),
                provider_display_name: "Test".into(),
            },
            window: ContextWindow {
                tokens: TEST_WINDOW,
                reserve: ContextReserve::Disabled,
            },
            usage: ContextUsage::default(),
            measured: None,
            inventory: ContextInventory::default(),
        }
    }

    #[test]
    fn store_keeps_main_and_task_snapshots_isolated_and_latest() {
        let store = ContextStore::default();
        let main = store.publisher(ContextKey::Main);
        let task_a = main.for_task(TASK_A);
        let task_b = main.for_task(TASK_B);
        main.publish(stored_snapshot(
            "main/prepared",
            ContextReadiness::PreparedNextRequest,
        ));
        task_a.publish(stored_snapshot(
            "task/a",
            ContextReadiness::CapturedCurrentRequest,
        ));
        task_b.publish(stored_snapshot(
            "task/b",
            ContextReadiness::CapturedCurrentRequest,
        ));
        main.publish(stored_snapshot(
            "main/current",
            ContextReadiness::CapturedCurrentRequest,
        ));

        assert_eq!(
            store.latest(&ContextKey::Main).unwrap().model.spec,
            "main/current"
        );
        assert_eq!(
            store.latest(&ContextKey::task(TASK_A)).unwrap().model.spec,
            "task/a"
        );
        assert_eq!(
            store.latest(&ContextKey::task(TASK_B)).unwrap().model.spec,
            "task/b"
        );
    }

    #[test]
    fn publishers_can_update_distinct_tasks_from_threads() {
        let store = ContextStore::default();
        let root = store.publisher(ContextKey::Main);
        let threads = [(TASK_A, "thread/a"), (TASK_B, "thread/b")]
            .into_iter()
            .map(|(task, spec)| {
                let publisher = root.for_task(task);
                thread::spawn(move || {
                    publisher.publish(stored_snapshot(
                        spec,
                        ContextReadiness::CapturedCurrentRequest,
                    ));
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap();
        }

        assert_eq!(
            store.latest(&ContextKey::task(TASK_A)).unwrap().model.spec,
            "thread/a"
        );
        assert_eq!(
            store.latest(&ContextKey::task(TASK_B)).unwrap().model.spec,
            "thread/b"
        );
        assert!(store.latest(&ContextKey::Main).is_none());
    }
}
