//! A host module: names a snippet can reach beyond the language, and the calls behind them.
//!
//! A module only describes a call. The gateway alone holds it to plan and read-only modes, asks
//! permission, opens the run's change record, runs it, and settles its row, so no module can let a
//! call skip any of that.

use serde_json::Value;

use super::gateway::Permission;
use super::output::CodeCall;
use super::runner::{ScriptCall, ScriptError, ScriptErrorKind, ScriptInvocation, ScriptName};
use crate::mcp::output::{McpInteraction, McpOutput};
use crate::tools::registry::BoxFuture;
use crate::tools::{ToolContext, ToolFailure};

/// Names the class a module's namespace is typed as, apart from any MCP server's.
const MODULE_CLASS_PREFIX: &str = "_Module_";

/// Makes a permitted call under the call's own context: what its row shows and what the snippet
/// receives, or why it failed.
pub type CallRun = Box<
    dyn FnOnce(ToolContext) -> BoxFuture<'static, Result<(McpOutput, Value), CallFailure>> + Send,
>;

/// What a module says about itself in two layers: `summary` sits in the `code` tool's description
/// for every session, and the pages answer `help()` on demand.
pub struct ModuleDoc {
    pub namespace: &'static str,
    /// One line, always loaded, so it must be the same for every session.
    pub summary: &'static str,
    /// What `help("<namespace>")` returns before the member signatures.
    pub page: &'static str,
    pub members: &'static [MemberDoc],
}

pub struct MemberDoc {
    pub name: &'static str,
    /// The Python signature, `name(params) -> result`, without the namespace.
    pub signature: &'static str,
    /// Whether a call returns something to `await`, as every call that reaches beyond what the
    /// run already holds does.
    pub awaited: bool,
    /// What `help("<namespace>.<name>")` returns after the signature.
    pub page: &'static str,
}

impl MemberDoc {
    fn stub(&self) -> String {
        let signature = match self.signature.split_once("()") {
            Some((name, rest)) => format!("{name}(self){rest}"),
            None => self.signature.replacen('(', "(self, ", 1),
        };
        match self.awaited {
            true => format!("    async def {signature}: ..."),
            false => format!("    def {signature}: ..."),
        }
    }

    fn written(&self, namespace: &str) -> String {
        match self.awaited {
            true => format!("await {namespace}.{}", self.signature),
            false => format!("{namespace}.{}", self.signature),
        }
    }
}

impl ModuleDoc {
    /// The module's namespace as a typed object, one line per declaration.
    pub fn stubs(&self) -> String {
        let class = format!("{MODULE_CLASS_PREFIX}{}", self.namespace);
        let mut stubs = format!("class {class}:\n");
        for member in self.members {
            stubs.push_str(&member.stub());
            stubs.push('\n');
        }
        stubs.push_str(&format!("{}: {class}\n", self.namespace));
        stubs
    }

    pub fn member(&self, name: &str) -> Option<&MemberDoc> {
        self.members.iter().find(|member| member.name == name)
    }

    pub fn member_names(&self) -> impl Iterator<Item = &'static str> {
        self.members.iter().map(|member| member.name)
    }

    /// The module page followed by every member's signature.
    pub fn module_help(&self) -> String {
        let mut help = format!("{}\n", self.page);
        for member in self.members {
            help.push('\n');
            help.push_str(&member.written(self.namespace));
        }
        help
    }

    pub fn member_help(&self, member: &MemberDoc) -> String {
        format!("{}\n\n{}", member.written(self.namespace), member.page)
    }

    /// What a snippet reading a member the module lacks is told.
    pub fn no_member(&self, name: &str) -> String {
        format!(
            "'{}' has no attribute '{name}'; it has {}",
            self.namespace,
            self.member_names().collect::<Vec<_>>().join(", ")
        )
    }
}

pub trait HostModule: Send + Sync {
    fn doc(&self) -> &'static ModuleDoc;
    /// Why the module's own namespace cannot answer calls in this session, with what fixes it;
    /// `None` when it can. The gateway raises it for every call, before `prepare`.
    fn unavailable(&self) -> Option<String> {
        None
    }
    /// What `help(topic)` returns for a topic the static pages do not cover, such as an MCP
    /// server's namespace.
    fn dynamic_help(&self, _topic: &str) -> Option<String> {
        None
    }
    /// Lines `help()` lists under the module, such as the namespaces it adds.
    fn index_lines(&self) -> Vec<String> {
        Vec::new()
    }
    /// What a global name resolves to when the module defines it. `Undefined` with a message
    /// explains a name the module recognises as a slip; `None` leaves the name to others.
    fn global(&self, name: &str) -> Option<ScriptName> {
        (name == self.doc().namespace).then_some(ScriptName::Namespace)
    }
    /// What a member of one of the module's namespaces resolves to.
    fn member(&self, _namespace: &str, name: &str) -> ScriptName {
        let doc = self.doc();
        match doc.member(name) {
            Some(_) => ScriptName::Function,
            None => ScriptName::Undefined(Some(doc.no_member(name))),
        }
    }
    /// Type stubs for everything the module defines, one declaration per line.
    fn stubs(&self) -> String {
        self.doc().stubs()
    }
    /// What a call to one of the module's names becomes.
    fn prepare(&self, ctx: &ToolContext, call: ScriptCall) -> Prepared;
    /// What the module can add to a type checker's rejection of the snippet.
    fn rejection_hints(&self, _diagnostic: &str) -> Vec<String> {
        Vec::new()
    }
}

/// A module this session cannot have at all, such as `mcp` with no server enabled: its names
/// still resolve, so a snippet type-checks the same everywhere, and every call raises why.
pub struct AbsentModule {
    doc: &'static ModuleDoc,
    reason: &'static str,
}

impl AbsentModule {
    pub fn new(doc: &'static ModuleDoc, reason: &'static str) -> Self {
        Self { doc, reason }
    }
}

impl HostModule for AbsentModule {
    fn doc(&self) -> &'static ModuleDoc {
        self.doc
    }

    fn unavailable(&self) -> Option<String> {
        Some(self.reason.to_owned())
    }

    fn prepare(&self, _ctx: &ToolContext, _call: ScriptCall) -> Prepared {
        Prepared::Plan(Err(ScriptError::new(
            ScriptErrorKind::Runtime,
            self.reason.to_owned(),
        )))
    }
}

pub enum Prepared {
    /// Answered from what the module already holds: it asks nothing and leaves no row.
    Answer(ScriptInvocation),
    /// Awaited, then authorized, recorded and run by the gateway. An `Err` raises when awaited,
    /// before the call has a row.
    Plan(Result<Box<HostCallPlan>, ScriptError>),
    /// A plan that needs reads off the executor first, such as which project a call reaches,
    /// handled as `Plan` once it resolves.
    Later(BoxFuture<'static, Result<Box<HostCallPlan>, ScriptError>>),
    /// A registered tool the gateway runs through tool dispatch, which holds it to the session's
    /// mode, asks under the tool's own rules and records it, exactly as a call the model made.
    Tool(Box<ToolCallPlan>),
}

pub struct ToolCallPlan {
    /// The call's row on the run's card, as it starts.
    pub row: CodeCall,
    pub tool: &'static str,
    pub input: Value,
}

pub struct HostCallPlan {
    /// The call's row on the run's card, as it starts.
    pub row: CodeCall,
    pub permission: Permission,
    /// Whether the call cannot change anything, which plan and read-only modes require.
    pub read_only: bool,
    pub run: CallRun,
}

/// Why a call that was let through failed, as its row keeps it.
pub struct CallFailure {
    pub(super) failure: ToolFailure,
    pub(super) error: String,
    pub(super) output: Option<Box<McpOutput>>,
    pub(super) interactions: Vec<McpInteraction>,
}

impl CallFailure {
    pub fn new(failure: ToolFailure, error: String) -> Self {
        Self {
            failure,
            error,
            output: None,
            interactions: Vec::new(),
        }
    }

    pub fn with_output(self, output: McpOutput) -> Self {
        Self {
            output: Some(Box::new(output)),
            ..self
        }
    }

    pub fn with_interactions(self, interactions: Vec<McpInteraction>) -> Self {
        Self {
            interactions,
            ..self
        }
    }
}
