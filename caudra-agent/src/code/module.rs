//! A host module: names a snippet can reach beyond the language, and the calls behind them.
//!
//! A module only describes a call. The gateway alone holds it to plan and read-only modes, asks
//! permission, opens the run's change record, runs it, and settles its row, so no module can let a
//! call skip any of that.

use serde_json::Value;

use super::gateway::Permission;
use super::output::CodeCall;
use super::runner::{ScriptCall, ScriptError, ScriptInvocation, ScriptName};
use crate::mcp::output::{McpInteraction, McpOutput};
use crate::tools::registry::BoxFuture;
use crate::tools::{ToolContext, ToolFailure};

/// Makes a permitted call under the call's own context: what its row shows and what the snippet
/// receives, or why it failed.
pub type CallRun = Box<
    dyn FnOnce(ToolContext) -> BoxFuture<'static, Result<(McpOutput, Value), CallFailure>> + Send,
>;

pub trait HostModule: Send + Sync {
    /// What a global name resolves to when the module defines it. `Undefined` with a message
    /// explains a name the module recognises as a slip; `None` leaves the name to others.
    fn global(&self, name: &str) -> Option<ScriptName>;
    /// What a member of one of the module's namespaces resolves to.
    fn member(&self, namespace: &str, name: &str) -> ScriptName;
    /// Type stubs for everything the module defines, one declaration per line.
    fn stubs(&self) -> String;
    /// What a call to one of the module's names becomes.
    fn prepare(&self, ctx: &ToolContext, call: ScriptCall) -> Prepared;
    /// What the module can add to a type checker's rejection of the snippet.
    fn rejection_hints(&self, _diagnostic: &str) -> Vec<String> {
        Vec::new()
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
