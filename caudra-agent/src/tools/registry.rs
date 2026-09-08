//! Single source of truth for native, Lua, and MCP tools. One registry, one lookup path, no
//! parallel lists that can drift.

use std::any::Any;
use std::borrow::Cow;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};

use arc_swap::ArcSwap;
use bitflags::bitflags;
use caudra_storage::tool_outputs::ToolOutputRef;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::permissions::{PermissionAuthorityProfile, PermissionResource, PermissionRisk};
use crate::template::Vars;
use crate::{BufferSnapshot, ToolInput, ToolOutput, ToolOutputLimits};

use super::deferral::DeferredTool;
use super::{DescriptionContext, ToolContext};

const EXAMPLES_HEADER: &str = "Examples:";
const EXAMPLE_CODE_KEY: &str = "code";
const EXAMPLE_FENCE_OPEN: &str = "\n```\n";
const EXAMPLE_FENCE_CLOSE: &str = "\n```";

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ToolAudience: u8 {
        const MAIN         = 0b0000_0001;
        const RESEARCH_SUB = 0b0000_0010;
        const GENERAL_SUB  = 0b0000_0100;
        const INTERPRETER  = 0b0000_1000;
        const WORKFLOW     = 0b0001_0000;
    }
}

impl Default for ToolAudience {
    fn default() -> Self {
        Self::all()
    }
}

pub const AUDIENCE_NAMES: &[(ToolAudience, &str)] = &[
    (ToolAudience::MAIN, "main"),
    (ToolAudience::RESEARCH_SUB, "research_sub"),
    (ToolAudience::GENERAL_SUB, "general_sub"),
    (ToolAudience::INTERPRETER, "interpreter"),
    (ToolAudience::WORKFLOW, "workflow"),
];

impl ToolAudience {
    pub fn name(self) -> Option<&'static str> {
        AUDIENCE_NAMES
            .iter()
            .find(|(flag, _)| *flag == self)
            .map(|(_, name)| *name)
    }

    pub fn parse_name(name: &str) -> Option<Self> {
        AUDIENCE_NAMES
            .iter()
            .find(|(_, n)| *n == name)
            .map(|(flag, _)| *flag)
    }
}

#[derive(Clone, Debug)]
pub enum ToolSource {
    Native {
        owner: Arc<str>,
        contract: Arc<str>,
        trusted: bool,
    },
    Mcp {
        server: Arc<str>,
    },
    Lua {
        plugin: Arc<str>,
        contract: Arc<str>,
        bundled: bool,
    },
}

impl ToolSource {
    pub fn as_log_field(&self) -> Cow<'static, str> {
        match self {
            Self::Native { owner, .. } => Cow::Owned(format!("native:{owner}")),
            Self::Mcp { server } => Cow::Owned(format!("mcp:{server}")),
            Self::Lua { plugin, .. } => Cow::Owned(format!("lua:{plugin}")),
        }
    }

    pub fn is_trusted(&self) -> bool {
        match self {
            Self::Native { trusted, .. } => *trusted,
            Self::Lua { bundled, .. } => *bundled,
            Self::Mcp { .. } => false,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    ReadOnly,
    Isolated,
    Orchestrator,
    Mutating,
    #[default]
    Unknown,
}

impl ToolEffect {
    pub fn is_safe_in_read_only(self) -> bool {
        matches!(self, Self::ReadOnly | Self::Isolated | Self::Orchestrator)
    }

    /// Whether a finished call can be hidden behind its header without losing
    /// the record of what happened. Only a call that reported something and
    /// changed nothing qualifies, so anything unclassified stays visible.
    pub fn is_collapsible(self) -> bool {
        self == Self::ReadOnly
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Isolated => "isolated",
            Self::Orchestrator => "orchestrator",
            Self::Mutating => "mutating",
            Self::Unknown => "unknown",
        }
    }
}

pub type ParseError = super::schema::ToolInputError;

pub struct ToolExecResult {
    pub output: Result<ToolOutput, String>,
    pub is_error: bool,
    pub annotation: Option<String>,
    pub written_path: Option<String>,
    pub written_paths: Vec<String>,
    pub model_suffix: Option<String>,
    pub model_output: Option<String>,
    pub output_limits: Option<ToolOutputLimits>,
    pub output_ref: Option<ToolOutputRef>,
    pub model_output_from_ref: bool,
}

impl From<Result<ToolOutput, String>> for ToolExecResult {
    fn from(output: Result<ToolOutput, String>) -> Self {
        let is_error = output.is_err();
        Self {
            output,
            is_error,
            annotation: None,
            written_path: None,
            written_paths: Vec::new(),
            model_suffix: None,
            model_output: None,
            output_limits: None,
            output_ref: None,
            model_output_from_ref: false,
        }
    }
}

impl ToolExecResult {
    pub fn with_written_path(mut self, path: Option<String>) -> Self {
        if self.output.is_ok() && !self.is_error {
            self.written_path = path;
        }
        self
    }

    pub fn with_written_paths(mut self, paths: Vec<String>) -> Self {
        if self.output.is_ok() && !self.is_error {
            self.written_path = paths.first().cloned();
            self.written_paths = paths;
        }
        self
    }

    pub fn with_model_output(mut self, model_output: Option<String>) -> Self {
        self.model_output = model_output;
        self
    }

    pub fn with_annotation(mut self, annotation: Option<String>) -> Self {
        self.annotation = annotation;
        self
    }

    pub fn with_error(mut self, is_error: bool) -> Self {
        self.is_error = is_error;
        self
    }
}

pub type ExecFuture<'a> = Pin<Box<dyn Future<Output = ToolExecResult> + Send + 'a>>;

#[derive(Debug, Clone)]
pub enum HeaderResult {
    Plain(String),
    Styled(BufferSnapshot),
}

impl HeaderResult {
    pub fn plain(text: String) -> Self {
        Self::Plain(text)
    }

    pub fn text(&self) -> String {
        match self {
            Self::Plain(t) => t.clone(),
            Self::Styled(snap) => snap.first_line_text(),
        }
    }

    pub fn snapshot(self) -> Option<BufferSnapshot> {
        match self {
            Self::Plain(_) => None,
            Self::Styled(snap) => Some(snap),
        }
    }

    pub fn into_snapshot(self) -> BufferSnapshot {
        match self {
            Self::Plain(text) => BufferSnapshot::plain_text(text),
            Self::Styled(snap) => snap,
        }
    }
}

pub enum HeaderFuture {
    Ready(HeaderResult),
    Pending {
        fallback: String,
        fut: Pin<Box<dyn Future<Output = HeaderResult> + Send>>,
    },
}

impl HeaderFuture {
    pub fn into_ready(self) -> HeaderResult {
        match self {
            Self::Ready(r) => r,
            Self::Pending { fallback, .. } => HeaderResult::plain(fallback),
        }
    }
}

impl Future for HeaderFuture {
    type Output = HeaderResult;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<HeaderResult> {
        match self.get_mut() {
            Self::Ready(r) => Poll::Ready(std::mem::replace(r, HeaderResult::plain(String::new()))),
            Self::Pending { fut, .. } => fut.as_mut().poll(cx),
        }
    }
}

/// How a call may proceed while a plan is being written.
///
/// Plan mode otherwise judges a call by its registered effect, which is fixed
/// before the input is parsed. These variants let a call that can only be
/// judged from its parsed input say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanModeAccess {
    /// Nothing declared: the registered effect and mutation targets decide.
    Standard,
    /// Refused while planning, whatever the permissions would allow.
    Refused,
    /// Inspected and found to have no effects, so planning may run it under the
    /// usual permission checks.
    ReadOnly,
    /// Effects are possible but unproven. Planning may run it only under an
    /// authority the plan itself asked for.
    Prompted,
}

#[derive(Debug, Clone, Default)]
pub struct PermissionScopes {
    pub scopes: Vec<String>,
    /// The call is unreviewable, so no rule may cover it and the user answers
    /// every time.
    pub force_prompt: bool,
    /// The call happens while a plan is being built. It is unreviewable in the
    /// same way, except that an authority granted inside this conversation may
    /// still cover it — and no wider authority may be minted from it.
    pub plan_scoped: bool,
}

impl PermissionScopes {
    pub fn single(scope: String) -> Self {
        Self {
            scopes: vec![scope],
            ..Self::default()
        }
    }

    pub fn force_prompt(scope: String) -> Self {
        Self {
            scopes: vec![scope],
            force_prompt: true,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct PermissionIntent {
    pub scopes: PermissionScopes,
    pub resources: Vec<PermissionResource>,
    pub risk: PermissionRisk,
    pub authority: PermissionAuthorityProfile,
}

impl PermissionIntent {
    pub fn new(
        scopes: PermissionScopes,
        resources: Vec<PermissionResource>,
        risk: PermissionRisk,
    ) -> Self {
        Self {
            scopes,
            resources,
            risk,
            authority: PermissionAuthorityProfile::default(),
        }
    }

    pub fn with_authority(mut self, authority: PermissionAuthorityProfile) -> Self {
        self.authority = authority;
        self
    }
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Holds the parsed input so start-event and `execute` share one parse pass.
/// Permission and mutation metadata belongs here because only the parsed call
/// knows which authorities and files it will touch.
pub trait ToolInvocation: Send + Sync {
    fn start_header(&self) -> HeaderFuture;
    fn start_annotation(&self) -> Option<String> {
        None
    }
    fn start_output(&self, _ctx: &ToolContext) -> Option<ToolOutput> {
        None
    }
    /// Display-only echo of the call (a shell command, a code block). Takes
    /// no context so a restored session can rebuild it from the stored input
    /// alone, without standing up an agent to ask.
    fn start_input(&self) -> Option<ToolInput> {
        None
    }
    fn mutable_path(&self) -> Option<&Path> {
        None
    }
    /// Files this call writes. Dispatch takes an exclusive guard on each for
    /// the whole of `execute`, so an implementor must not re-enter dispatch.
    fn mutation_targets(&self, _ctx: &ToolContext) -> Vec<PathBuf> {
        self.mutable_path()
            .map(Path::to_path_buf)
            .into_iter()
            .collect()
    }
    /// Files this call reads whole, named before it runs. Dispatch takes a
    /// shared guard on each, so a concurrent write cannot land between the read
    /// and the mtime the tool records for it. Paths a call only discovers while
    /// running, like `file_grep` matches, cannot be declared here. Never
    /// overlaps `mutation_targets`, which already covers read-modify-write.
    fn read_targets(&self, _ctx: &ToolContext) -> Vec<PathBuf> {
        Vec::new()
    }
    /// How this call may proceed while a plan is being written. Consulted after
    /// `preflight`, so an invocation that can only judge itself once its input
    /// is parsed still gets to answer.
    fn plan_mode_access(&self) -> PlanModeAccess {
        PlanModeAccess::Standard
    }
    /// Effect of this one call. A tool whose commands do not share an effect
    /// registers its worst case and narrows here, so plan and read-only gating
    /// judges the call instead of the registration.
    fn call_effect(&self, registered: ToolEffect) -> ToolEffect {
        registered
    }
    /// Performs non-effectful native inspection before plan and boundary
    /// checks. Legacy tools leave this unset and retain the existing
    /// permission lifecycle.
    fn preflight<'a>(
        &'a self,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<Option<PermissionIntent>, String>> {
        Box::pin(std::future::ready(Ok(None)))
    }
    fn permission_intent<'a>(
        &'a self,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Option<PermissionIntent>> {
        Box::pin(std::future::ready(None))
    }
    fn permission_scopes(&self) -> BoxFuture<'_, Option<PermissionScopes>> {
        Box::pin(std::future::ready(None))
    }
    fn permission_input(&self) -> Option<&Value> {
        None
    }
    /// Runs after permission enforcement and `ToolStart`. Some call paths skip
    /// it, so `execute` must never rely on it having run.
    fn start<'a>(&'a self, _ctx: &'a ToolContext) -> BoxFuture<'a, ()> {
        Box::pin(std::future::ready(()))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a>;
}

pub trait Tool: Any + Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self, ctx: &DescriptionContext) -> Cow<'_, str>;
    fn schema(&self) -> Value;
    fn examples(&self) -> Option<Value> {
        None
    }
    fn audience(&self) -> ToolAudience {
        ToolAudience::default()
    }
    fn tool_kind(&self) -> Option<&str> {
        None
    }
    /// Whether some call shape is read-only despite a mutating registration.
    /// A read-only audience still gets the definition; dispatch refuses the
    /// calls that are not read-only.
    fn has_read_only_calls(&self) -> bool {
        false
    }
    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError>;
}

#[derive(Clone)]
pub struct RegisteredTool {
    pub tool: Arc<dyn Tool>,
    pub source: ToolSource,
    pub effect: ToolEffect,
}

impl RegisteredTool {
    pub fn name(&self) -> &str {
        self.tool.name()
    }

    /// Parse without naming `ParseError`, handy for crates outside `caudra-agent`.
    pub fn try_parse(&self, input: &serde_json::Value) -> Option<Box<dyn ToolInvocation>> {
        self.tool.parse(input).ok()
    }

    pub(crate) fn downcast_ref<T: Tool>(&self) -> Option<&T> {
        let tool: &dyn Any = self.tool.as_ref();
        tool.downcast_ref()
    }

    pub fn is_safe_in_read_only(&self) -> bool {
        self.is_safe_in_read_only_with(self.effect)
    }

    pub fn is_safe_in_read_only_with(&self, effect: ToolEffect) -> bool {
        effect.is_safe_in_read_only() && self.is_audited_host()
    }

    /// Effect of one call. A narrower effect counts only from a host we
    /// audited; an MCP server or an external plugin is held to what it
    /// registered, so it cannot talk its way past a mode gate.
    pub fn effect_for(&self, invocation: &dyn ToolInvocation) -> ToolEffect {
        if self.is_audited_host() {
            invocation.call_effect(self.effect)
        } else {
            self.effect
        }
    }

    pub fn is_visible_in_read_only(&self) -> bool {
        (self.effect.is_safe_in_read_only() || self.tool.has_read_only_calls())
            && self.is_audited_host()
    }

    fn is_audited_host(&self) -> bool {
        matches!(
            self.source,
            ToolSource::Native { trusted: true, .. } | ToolSource::Lua { bundled: true, .. }
        )
    }
}

/// Lock-free reads via `ArcSwap`, writes swap in a new snapshot atomically.
pub struct ToolRegistry {
    tools: ArcSwap<Vec<RegisteredTool>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("tool '{name}' is already registered (existing source: {existing})")]
    NameConflict { name: String, existing: String },
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: ArcSwap::from_pointee(Vec::new()),
        }
    }

    /// The process-wide registry shared by native, Lua, and MCP tools.
    pub fn global() -> &'static Self {
        Self::global_arc()
    }

    pub fn global_arc() -> &'static Arc<Self> {
        static GLOBAL: LazyLock<Arc<ToolRegistry>> =
            LazyLock::new(|| Arc::new(ToolRegistry::new()));
        &GLOBAL
    }

    pub fn get(&self, name: &str) -> Option<RegisteredTool> {
        self.tools.load().iter().find(|t| t.name() == name).cloned()
    }

    pub fn has(&self, name: &str) -> bool {
        self.tools.load().iter().any(|t| t.name() == name)
    }

    pub fn register(&self, tool: Arc<dyn Tool>, source: ToolSource) -> Result<(), RegistryError> {
        self.register_audited(tool, source, ToolEffect::Unknown)
    }

    pub fn register_audited(
        &self,
        tool: Arc<dyn Tool>,
        source: ToolSource,
        effect: ToolEffect,
    ) -> Result<(), RegistryError> {
        let name = tool.name().to_owned();
        let mut conflict = None;
        self.tools.rcu(|current| {
            conflict = None;
            if let Some(existing) = current.iter().find(|t| t.name() == name) {
                conflict = Some(existing.source.as_log_field().into_owned());
                return Vec::clone(current);
            }
            let mut next = Vec::with_capacity(current.len() + 1);
            next.extend(current.iter().cloned());
            next.push(RegisteredTool {
                tool: Arc::clone(&tool),
                source: source.clone(),
                effect,
            });
            next
        });
        if let Some(existing) = conflict {
            return Err(RegistryError::NameConflict { name, existing });
        }
        Ok(())
    }

    /// All-or-nothing: a name clash rolls back the whole batch so an MCP server
    /// never ends up half-registered.
    pub fn register_many(
        &self,
        entries: impl IntoIterator<Item = (Arc<dyn Tool>, ToolSource)>,
    ) -> Result<(), RegistryError> {
        self.register_many_audited(
            entries
                .into_iter()
                .map(|(tool, source)| (tool, source, ToolEffect::Unknown)),
        )
    }

    pub fn register_many_audited(
        &self,
        entries: impl IntoIterator<Item = (Arc<dyn Tool>, ToolSource, ToolEffect)>,
    ) -> Result<(), RegistryError> {
        let entries: Vec<_> = entries.into_iter().collect();
        let mut conflict = None;
        self.tools.rcu(|current| {
            conflict = None;
            let mut next = Vec::clone(current);
            for (tool, source, effect) in &entries {
                let name = tool.name();
                if let Some(existing) = next.iter().find(|t| t.name() == name) {
                    conflict = Some(RegistryError::NameConflict {
                        name: name.to_owned(),
                        existing: existing.source.as_log_field().into_owned(),
                    });
                    return Vec::clone(current);
                }
                next.push(RegisteredTool {
                    tool: Arc::clone(tool),
                    source: source.clone(),
                    effect: *effect,
                });
            }
            next
        });
        if let Some(e) = conflict {
            return Err(e);
        }
        Ok(())
    }

    pub fn clear_mcp_server(&self, server: &str) {
        self.tools.rcu(|current| {
            current
                .iter()
                .filter(
                    |t| !matches!(&t.source, ToolSource::Mcp { server: s } if s.as_ref() == server),
                )
                .cloned()
                .collect::<Vec<_>>()
        });
    }

    pub fn replace_plugin(
        &self,
        plugin: &str,
        new_entries: Vec<(Arc<dyn Tool>, ToolSource)>,
    ) -> Result<(), RegistryError> {
        self.replace_plugin_audited(
            plugin,
            new_entries
                .into_iter()
                .map(|(tool, source)| (tool, source, ToolEffect::Unknown))
                .collect(),
        )
    }

    pub fn replace_plugin_audited(
        &self,
        plugin: &str,
        new_entries: Vec<(Arc<dyn Tool>, ToolSource, ToolEffect)>,
    ) -> Result<(), RegistryError> {
        let mut conflict = None;
        self.tools.rcu(|current| {
            conflict = None;
            let mut next: Vec<RegisteredTool> = current
                .iter()
                .filter(
                    |t| !matches!(&t.source, ToolSource::Lua { plugin: p, .. } if p.as_ref() == plugin),
                )
                .cloned()
                .collect();
            for (tool, source, effect) in &new_entries {
                let name = tool.name();
                if let Some(existing) = next.iter().find(|t| t.name() == name) {
                    conflict = Some(RegistryError::NameConflict {
                        name: name.to_owned(),
                        existing: existing.source.as_log_field().into_owned(),
                    });
                    return Vec::clone(current);
                }
                next.push(RegisteredTool {
                    tool: Arc::clone(tool),
                    source: source.clone(),
                    effect: *effect,
                });
            }
            next
        });
        if let Some(e) = conflict {
            return Err(e);
        }
        Ok(())
    }

    pub fn clear_lua(&self) {
        self.tools.rcu(|current| {
            current
                .iter()
                .filter(|t| !matches!(t.source, ToolSource::Lua { .. }))
                .cloned()
                .collect::<Vec<_>>()
        });
    }

    pub fn clear_plugin(&self, plugin: &str) {
        self.tools.rcu(|current| {
            current
                .iter()
                .filter(
                    |t| !matches!(&t.source, ToolSource::Lua { plugin: p, .. } if p.as_ref() == plugin),
                )
                .cloned()
                .collect::<Vec<_>>()
        });
    }

    /// Human-friendly summary of an invocation; the raw tool name when
    /// there is nothing better.
    pub fn resolve_header(&self, name: &str, input: &Value) -> String {
        self.get(name)
            .and_then(|e| e.try_parse(input))
            .map(|inv| inv.start_header().into_ready().text())
            .unwrap_or_else(|| name.to_owned())
    }

    pub fn names(&self) -> Vec<Arc<str>> {
        self.tools
            .load()
            .iter()
            .map(|t| Arc::from(t.name()))
            .collect()
    }

    /// Rebuilt each request so tools registered mid-session (MCP handshake) show
    /// up on the very next turn.
    pub fn definitions(
        &self,
        vars: &Vars,
        ctx: &DescriptionContext,
        supports_examples: bool,
    ) -> Value {
        self.definitions_split(vars, ctx, supports_examples, &[])
            .declared
    }

    /// The same definitions, split into what the request declares and what is
    /// held back for `tool_search`. `deferred` names tools to withhold;
    /// anything not registered, filtered out, or wrong for the audience is
    /// absent from both halves, so a disabled tool cannot be searched into
    /// existence.
    pub fn definitions_split(
        &self,
        vars: &Vars,
        ctx: &DescriptionContext,
        supports_examples: bool,
        deferred: &[&str],
    ) -> ToolDefinitions {
        let snapshot = self.tools.load();
        let mut out = Vec::with_capacity(snapshot.len());
        let mut held = Vec::new();
        for entry in snapshot.iter() {
            if !entry.tool.audience().contains(ctx.audience) {
                continue;
            }
            if ctx.policy().is_read_only() && !entry.is_visible_in_read_only() {
                continue;
            }
            if !ctx.filter.matches(entry.name()) {
                continue;
            }
            let description = vars.apply(&entry.tool.description(ctx)).into_owned();
            let mut def = json!({
                "name": entry.name(),
                "description": description,
                "input_schema": entry.tool.schema(),
            });
            if let Some(examples) = entry.tool.examples() {
                if supports_examples {
                    def["input_examples"] = examples;
                } else if let Some(text) = format_examples_as_text(&examples) {
                    let merged =
                        format!("{}\n\n{}", def["description"].as_str().unwrap_or(""), text);
                    def["description"] = Value::String(merged);
                }
            }
            match deferred.contains(&entry.name()) {
                true => held.push(DeferredTool::new(entry.name(), group_of(entry.name()), def)),
                false => out.push(def),
            }
        }
        ToolDefinitions {
            declared: Value::Array(out),
            deferred: held,
        }
    }

    pub fn iter(&self) -> RegistrySnapshot {
        RegistrySnapshot(self.tools.load_full())
    }
}

/// What one request declares, and what it holds back behind `tool_search`.
pub struct ToolDefinitions {
    pub declared: Value,
    pub deferred: Vec<DeferredTool>,
}

/// Grouping is policy, so it is read from the same list that names the
/// deferred tools rather than from the tool itself.
fn group_of(name: &str) -> Option<&'static str> {
    caudra_config::DEFERRED_BUILTIN_TOOLS
        .iter()
        .find(|deferred| deferred.name == name)
        .and_then(|deferred| deferred.group)
}

pub struct RegistrySnapshot(Arc<Vec<RegisteredTool>>);

impl RegistrySnapshot {
    pub fn iter(&self) -> std::slice::Iter<'_, RegisteredTool> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The fallback for models that take examples as prose rather than as a
/// structured field. An example is either a prewritten snippet under `code` or,
/// as every native tool writes them, a literal input object; rendering only the
/// former emitted a bare header and silently dropped every example.
fn format_examples_as_text(examples: &Value) -> Option<String> {
    let mut text = String::from(EXAMPLES_HEADER);
    for example in examples.as_array()? {
        let body = match example.get(EXAMPLE_CODE_KEY).and_then(Value::as_str) {
            Some(code) => code.to_owned(),
            None => serde_json::to_string_pretty(example).ok()?,
        };
        text.push_str(EXAMPLE_FENCE_OPEN);
        text.push_str(&body);
        text.push_str(EXAMPLE_FENCE_CLOSE);
    }
    (text.len() > EXAMPLES_HEADER.len()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::Vars;
    use test_case::test_case;

    const EXAMPLES_DROPPED: &str = "an example body must survive into the text form";
    const SNIPPET: &str = "batch { file_read }";

    struct MockTool {
        name: String,
        audience: ToolAudience,
        read_only_calls: bool,
    }

    struct MockInvocation;

    impl ToolInvocation for MockInvocation {
        fn start_header(&self) -> HeaderFuture {
            HeaderFuture::Ready(HeaderResult::plain("mock".into()))
        }
        fn execute<'a>(self: Box<Self>, _ctx: &'a super::ToolContext) -> ExecFuture<'a> {
            Box::pin(async { Ok(ToolOutput::Plain(String::new().into())).into() })
        }
    }

    impl Tool for MockTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
            "mock tool".into()
        }
        fn schema(&self) -> Value {
            json!({"type": "object", "properties": {}, "additionalProperties": false})
        }
        fn audience(&self) -> ToolAudience {
            self.audience
        }
        fn has_read_only_calls(&self) -> bool {
            self.read_only_calls
        }
        fn parse(&self, _input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            Ok(Box::new(MockInvocation))
        }
    }

    fn mock(name: &str) -> Arc<dyn Tool> {
        mock_scoped(name, ToolAudience::all())
    }

    fn mock_scoped(name: &str, audience: ToolAudience) -> Arc<dyn Tool> {
        Arc::new(MockTool {
            name: name.to_owned(),
            audience,
            read_only_calls: false,
        })
    }

    /// A mutating registration that can still be called read-only, as `memory`
    /// is for `list` and `read`.
    fn mock_with_read_only_calls(name: &str) -> Arc<dyn Tool> {
        Arc::new(MockTool {
            name: name.to_owned(),
            audience: ToolAudience::all(),
            read_only_calls: true,
        })
    }

    fn lua_source(plugin: &str) -> ToolSource {
        ToolSource::Lua {
            plugin: plugin.into(),
            contract: "test-contract".into(),
            bundled: false,
        }
    }

    #[test]
    fn name_conflict_is_rejected() {
        let reg = ToolRegistry::new();
        reg.register(mock("dupe"), lua_source("p")).unwrap();
        let err = reg.register(mock("dupe"), lua_source("p")).unwrap_err();
        assert!(matches!(err, RegistryError::NameConflict { .. }));
    }

    #[test]
    fn native_source_logs_its_actual_owner() {
        let source = ToolSource::Native {
            owner: "caudra".into(),
            contract: "patch/v1".into(),
            trusted: true,
        };

        assert_eq!(source.as_log_field(), "native:caudra");
    }

    /// Tools added mid-session must show up in the next `definitions()` call.
    /// That is the whole reason we build schemas per-request.
    #[test]
    fn definitions_reflects_mid_session_registration() {
        let reg = ToolRegistry::new();
        reg.register(
            mock("late_server.probe"),
            ToolSource::Mcp {
                server: "late_server".into(),
            },
        )
        .unwrap();

        let filter = crate::tools::ToolFilter::All;
        let ctx = DescriptionContext {
            filter: &filter,
            audience: ToolAudience::MAIN,
            workflow: false,
        };
        let vars = Vars::new();
        let defs = reg.definitions(&vars, &ctx, false);
        let arr = defs.as_array().expect("definitions returns array");
        assert!(
            arr.iter()
                .any(|d| d["name"].as_str() == Some("late_server.probe")),
            "mid-session tool missing from definitions"
        );
    }

    /// `/reload` re-registers the same lua tool names, so anything
    /// `clear_lua` leaves behind becomes a `NameConflict` that breaks every
    /// later reload.
    #[test]
    fn clear_lua_removes_lua_keeps_mcp_and_allows_reregistration() {
        let reg = ToolRegistry::new();
        reg.register(mock("lua_a"), lua_source("p1")).unwrap();
        reg.register(mock("lua_b"), lua_source("p2")).unwrap();
        reg.register(
            mock("srv.tool"),
            ToolSource::Mcp {
                server: "srv".into(),
            },
        )
        .unwrap();

        reg.clear_lua();

        assert!(!reg.has("lua_a"));
        assert!(!reg.has("lua_b"));
        assert!(reg.has("srv.tool"));

        reg.register(mock("lua_a"), lua_source("p1")).unwrap();
        reg.register(mock("lua_b"), lua_source("p2")).unwrap();
        assert!(reg.has("lua_a"));
        assert!(reg.has("lua_b"));
    }

    #[test]
    fn clear_mcp_server_removes_only_that_server() {
        let reg = ToolRegistry::new();
        reg.register(
            mock("serverA.one"),
            ToolSource::Mcp {
                server: "serverA".into(),
            },
        )
        .unwrap();
        reg.register(
            mock("serverB.one"),
            ToolSource::Mcp {
                server: "serverB".into(),
            },
        )
        .unwrap();
        reg.register(mock("other_tool"), lua_source("other"))
            .unwrap();

        reg.clear_mcp_server("serverA");

        assert!(!reg.has("serverA.one"));
        assert!(reg.has("serverB.one"));
        assert!(reg.has("other_tool"));
    }

    #[test]
    fn clear_plugin_removes_only_that_plugin() {
        let reg = ToolRegistry::new();
        reg.register(
            mock("pluginA.foo"),
            ToolSource::Lua {
                plugin: "pluginA".into(),
                contract: "contract-a".into(),
                bundled: false,
            },
        )
        .unwrap();
        reg.register(
            mock("pluginB.bar"),
            ToolSource::Lua {
                plugin: "pluginB".into(),
                contract: "contract-b".into(),
                bundled: false,
            },
        )
        .unwrap();
        reg.register(
            mock("mcp.tool"),
            ToolSource::Mcp {
                server: "srv".into(),
            },
        )
        .unwrap();

        reg.clear_plugin("pluginA");

        assert!(!reg.has("pluginA.foo"));
        assert!(reg.has("pluginB.bar"));
        assert!(reg.has("mcp.tool"));
    }

    #[test]
    fn replace_plugin_swaps_own_tools() {
        let reg = ToolRegistry::new();
        reg.register(mock("mytool"), lua_source("myplugin"))
            .unwrap();

        reg.replace_plugin("myplugin", vec![(mock("mytool"), lua_source("myplugin"))])
            .unwrap();

        let entry = reg.get("mytool").unwrap();
        assert!(matches!(entry.source, ToolSource::Lua { .. }));

        reg.clear_plugin("myplugin");
        assert!(!reg.has("mytool"));
    }

    #[test]
    fn replace_plugin_rejects_conflict_with_other_plugin() {
        let reg = ToolRegistry::new();
        reg.register(mock("shared"), ToolSource::Mcp { server: "s".into() })
            .unwrap();

        let err = reg
            .replace_plugin(
                "myplugin",
                vec![(
                    mock("shared"),
                    ToolSource::Lua {
                        plugin: "myplugin".into(),
                        contract: "test-contract".into(),
                        bundled: false,
                    },
                )],
            )
            .unwrap_err();
        assert!(matches!(err, RegistryError::NameConflict { .. }));
    }

    #[test]
    fn audience_names_round_trip() {
        let mut union = ToolAudience::empty();
        for (flag, name) in AUDIENCE_NAMES {
            assert_eq!(flag.name(), Some(*name));
            assert_eq!(ToolAudience::parse_name(name), Some(*flag));
            union |= *flag;
        }
        assert_eq!(union, ToolAudience::all());
        assert_eq!(ToolAudience::parse_name("nope"), None);
        assert_eq!(ToolAudience::all().name(), None);
    }

    #[test]
    fn definitions_excludes_wrong_audience() {
        let reg = ToolRegistry::new();
        reg.register(
            mock_scoped("main_only_tool", ToolAudience::MAIN),
            lua_source("p"),
        )
        .unwrap();
        reg.register(mock("everywhere"), lua_source("p")).unwrap();

        let vars = Vars::new();
        let filter = crate::tools::ToolFilter::All;
        let names_for = |audience: ToolAudience| -> Vec<String> {
            let ctx = DescriptionContext {
                filter: &filter,
                audience,
                workflow: false,
            };
            reg.definitions(&vars, &ctx, false)
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d["name"].as_str().unwrap().to_owned())
                .collect()
        };

        assert_eq!(
            names_for(ToolAudience::MAIN),
            vec!["main_only_tool", "everywhere"]
        );
        assert_eq!(names_for(ToolAudience::RESEARCH_SUB), Vec::<String>::new());
        assert_eq!(names_for(ToolAudience::GENERAL_SUB), vec!["everywhere"]);
    }

    #[test]
    fn read_only_definitions_require_audited_host_trust_and_safe_effect() {
        let reg = ToolRegistry::new();
        let trusted_native = |name: &str| {
            (
                mock(name),
                ToolSource::Native {
                    owner: "caudra".into(),
                    contract: format!("{name}/v1").into(),
                    trusted: true,
                },
            )
        };

        for (name, effect) in [
            ("native_read", ToolEffect::ReadOnly),
            ("native_isolated", ToolEffect::Isolated),
            ("native_orchestrator", ToolEffect::Orchestrator),
            ("native_mutating", ToolEffect::Mutating),
        ] {
            let (tool, source) = trusted_native(name);
            reg.register_audited(tool, source, effect).unwrap();
        }
        let (tool, source) = trusted_native("native_unknown");
        reg.register(tool, source).unwrap();
        reg.register_audited(
            mock("native_untrusted"),
            ToolSource::Native {
                owner: "third-party".into(),
                contract: "native-untrusted/v1".into(),
                trusted: false,
            },
            ToolEffect::ReadOnly,
        )
        .unwrap();
        reg.register_audited(
            mock("lua_bundled"),
            ToolSource::Lua {
                plugin: "bundled".into(),
                contract: "lua-bundled/v1".into(),
                bundled: true,
            },
            ToolEffect::ReadOnly,
        )
        .unwrap();
        reg.register_audited(
            mock("lua_unbundled"),
            lua_source("external"),
            ToolEffect::ReadOnly,
        )
        .unwrap();
        reg.register_audited(
            mock("mcp_claimed_read"),
            ToolSource::Mcp {
                server: "server".into(),
            },
            ToolEffect::ReadOnly,
        )
        .unwrap();
        let (_, source) = trusted_native("native_mutating_with_reads");
        reg.register_audited(
            mock_with_read_only_calls("native_mutating_with_reads"),
            source,
            ToolEffect::Mutating,
        )
        .unwrap();
        reg.register_audited(
            mock_with_read_only_calls("untrusted_mutating_with_reads"),
            lua_source("external"),
            ToolEffect::Mutating,
        )
        .unwrap();

        let filter = crate::tools::ToolFilter::All.for_mode(&crate::AgentMode::ReadOnly);
        let ctx = DescriptionContext {
            filter: &filter,
            audience: ToolAudience::MAIN,
            workflow: false,
        };
        let names: Vec<_> = reg
            .definitions(&Vars::new(), &ctx, false)
            .as_array()
            .unwrap()
            .iter()
            .map(|definition| definition["name"].as_str().unwrap().to_owned())
            .collect();

        assert_eq!(
            names,
            [
                "native_read",
                "native_isolated",
                "native_orchestrator",
                "lua_bundled",
                "native_mutating_with_reads"
            ]
        );
    }

    #[test]
    fn research_audience_uses_read_only_definition_policy() {
        let reg = ToolRegistry::new();
        for (name, effect) in [
            ("read", ToolEffect::ReadOnly),
            ("write", ToolEffect::Mutating),
        ] {
            reg.register_audited(
                mock(name),
                ToolSource::Native {
                    owner: "caudra".into(),
                    contract: format!("{name}/v1").into(),
                    trusted: true,
                },
                effect,
            )
            .unwrap();
        }
        let ctx = DescriptionContext {
            filter: &crate::tools::ToolFilter::All,
            audience: ToolAudience::RESEARCH_SUB,
            workflow: false,
        };

        let definitions = reg.definitions(&Vars::new(), &ctx, false);
        assert_eq!(definitions[0]["name"], "read");
        assert_eq!(definitions.as_array().unwrap().len(), 1);
    }

    #[test]
    fn definitions_keep_internal_companions_with_restrictive_config() {
        use crate::tools::{FILE_READ_TOOL_NAME, SHELL_TOOL_NAME, TOOL_OUTPUT_TOOL_NAME};

        let reg = ToolRegistry::new();
        for name in [FILE_READ_TOOL_NAME, TOOL_OUTPUT_TOOL_NAME, SHELL_TOOL_NAME] {
            reg.register(mock(name), lua_source("p")).unwrap();
        }
        let config = crate::AgentConfig {
            allowed_tools: vec![FILE_READ_TOOL_NAME.into()],
            disabled_tools: vec![TOOL_OUTPUT_TOOL_NAME.into()],
            ..Default::default()
        };
        let model = caudra_providers::Model::from_spec("anthropic/claude-opus-4-8").unwrap();
        let filter = crate::tools::ToolFilter::from_config(&config, &model, &[]);
        let ctx = DescriptionContext {
            filter: &filter,
            audience: ToolAudience::MAIN,
            workflow: false,
        };

        let definitions = reg.definitions(&Vars::new(), &ctx, false);
        let names: Vec<_> = definitions
            .as_array()
            .unwrap()
            .iter()
            .map(|definition| definition["name"].as_str().unwrap())
            .collect();

        assert_eq!(names, [FILE_READ_TOOL_NAME, TOOL_OUTPUT_TOOL_NAME]);
    }

    #[test_case(Err("boom".into()), Some("/tmp/foo".into()), None          ; "clears_on_error")]
    #[test_case(Ok(ToolOutput::Plain("ok".into())), Some("/tmp/foo".into()), Some("/tmp/foo") ; "sets_on_success")]
    fn with_written_path(
        base: Result<ToolOutput, String>,
        path: Option<String>,
        expected: Option<&str>,
    ) {
        let result: ToolExecResult = base.into();
        let result = result.with_written_path(path);
        assert_eq!(result.written_path.as_deref(), expected);
    }

    #[test]
    fn plural_paths_and_model_output_are_carried_by_exec_result() {
        let result = ToolExecResult::from(Ok::<_, String>(ToolOutput::Plain("ok".into())))
            .with_written_paths(vec!["first.rs".into(), "second.rs".into()])
            .with_model_output(Some("model-only".into()));

        assert_eq!(result.written_path.as_deref(), Some("first.rs"));
        assert_eq!(result.written_paths, ["first.rs", "second.rs"]);
        assert_eq!(result.model_output.as_deref(), Some("model-only"));
    }

    /// Every native tool writes its examples as literal input objects, so
    /// rendering only a `code` key shipped the header alone. A header with
    /// nothing under it is worse than no header.
    #[test]
    fn an_input_object_example_reaches_the_text_form() {
        let text = format_examples_as_text(&json!([{ "pattern": "src/**/*.ts" }]))
            .expect(EXAMPLES_DROPPED);

        assert!(text.contains("src/**/*.ts"), "{EXAMPLES_DROPPED}");
        assert!(text.starts_with(EXAMPLES_HEADER));
    }

    #[test]
    fn a_prewritten_snippet_still_wins_over_the_input_object() {
        let text = format_examples_as_text(&json!([{ "code": SNIPPET }])).expect(EXAMPLES_DROPPED);

        assert!(text.contains(SNIPPET));
        assert!(
            !text.contains(EXAMPLE_CODE_KEY),
            "the key leaked into prose"
        );
    }

    #[test]
    fn no_examples_means_no_dangling_header() {
        assert!(format_examples_as_text(&json!([])).is_none());
        assert!(format_examples_as_text(&json!({})).is_none());
    }
}

#[cfg(test)]
mod effect_tests {
    use super::ToolEffect;
    use test_case::test_case;

    const COLLAPSIBLE_MSG: &str =
        "only a call that reported something and changed nothing may hide behind its header";
    const SPELLING_MSG: &str = "the serialized effect must match the name Lua is given";

    #[test_case(ToolEffect::ReadOnly, true ; "read_only")]
    #[test_case(ToolEffect::Isolated, false ; "isolated")]
    #[test_case(ToolEffect::Orchestrator, false ; "orchestrator")]
    #[test_case(ToolEffect::Mutating, false ; "mutating")]
    #[test_case(ToolEffect::Unknown, false ; "unknown")]
    fn the_effect_decides_whether_a_card_may_close(effect: ToolEffect, expected: bool) {
        assert_eq!(effect.is_collapsible(), expected, "{COLLAPSIBLE_MSG}");
    }

    /// Lua reads the effect through `as_str`, so a serialized one that spells
    /// itself differently would put two names for the same thing on the wire.
    #[test_case(ToolEffect::ReadOnly ; "read_only")]
    #[test_case(ToolEffect::Isolated ; "isolated")]
    #[test_case(ToolEffect::Orchestrator ; "orchestrator")]
    #[test_case(ToolEffect::Mutating ; "mutating")]
    #[test_case(ToolEffect::Unknown ; "unknown")]
    fn an_effect_spells_itself_the_same_way_everywhere(effect: ToolEffect) {
        assert_eq!(
            serde_json::to_value(effect).unwrap(),
            serde_json::Value::from(effect.as_str()),
            "{SPELLING_MSG}"
        );
    }
}
