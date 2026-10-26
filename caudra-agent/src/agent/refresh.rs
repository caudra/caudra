use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use caudra_config::ModelPolicy;
use caudra_providers::provider::Provider;
use caudra_providers::{Message, Model, RequestOptions};
use serde_json::Value;

use super::run::{mode_switch_notice_with_tools, relocation_notice};
use super::{History, LoadedInstructions, favorites, standing_notice};
use crate::mcp::McpSession;
use crate::mcp::code::catalog;
use crate::memory::store::MemoryStore;
use crate::prompt;
use crate::remote_project_context::RemoteProjectContext;
use crate::template::Vars;
use crate::tools::native::plan::PlanTarget;
use crate::tools::{CODE_TOOL_NAME, ToolFilter};
use crate::{AgentError, AgentMode, autopilot};

pub struct ActiveContext<'a> {
    pub provider: &'a Arc<dyn Provider>,
    pub model: &'a Model,
    pub system: &'a str,
    pub tools: &'a Value,
    pub tool_filter: &'a ToolFilter,
    pub mode: &'a AgentMode,
    pub plan: Option<&'a PlanTarget>,
    pub options: &'a RequestOptions,
}

pub struct RefreshedContext {
    pub system: String,
    pub environment: Option<String>,
    pub instructions: Option<String>,
    pub memory: Option<String>,
    pub configuration: Option<String>,
    pub tools: Value,
    pub tool_filter: ToolFilter,
    pub loaded_instructions: LoadedInstructions,
    pub remote_project_context: Option<Arc<RemoteProjectContext>>,
    pub task_environment: Vars,
    pub memory_store: Option<Arc<MemoryStore>>,
}

pub trait ContextRefresh: Send + Sync {
    fn refresh<'a>(
        &'a mut self,
        history: &'a History,
        active: ActiveContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<RefreshedContext, AgentError>> + Send + 'a>>;
}

pub struct StandingState<'a> {
    pub environment: Option<&'a str>,
    pub instructions: Option<&'a str>,
    pub memory: Option<&'a str>,
    pub configuration: Option<&'a str>,
    pub mode_notice: Option<&'a str>,
    pub mode: &'a AgentMode,
    pub autopilot: Option<bool>,
    pub favorites: bool,
    pub model_policy: &'a ModelPolicy,
    pub mcp: Option<&'a McpSession>,
    pub decisions: bool,
    pub tools: &'a ToolFilter,
}

impl StandingState<'_> {
    pub async fn collect(&self, history: &[Message]) -> Vec<Message> {
        self.collect_with(history, |name| self.tools.matches(name))
            .await
    }

    pub(crate) async fn collect_with(
        &self,
        history: &[Message],
        available: impl Fn(&str) -> bool,
    ) -> Vec<Message> {
        let listed = if self.favorites {
            favorite_models(self.model_policy).await
        } else {
            None
        };
        self.collect_resolved(history, available, listed)
    }

    pub fn collect_with_favorites(
        &self,
        history: &[Message],
        listed: Option<String>,
    ) -> Vec<Message> {
        self.collect_resolved(history, |name| self.tools.matches(name), listed)
    }

    pub(crate) fn collect_resolved(
        &self,
        history: &[Message],
        available: impl Fn(&str) -> bool,
        listed: Option<String>,
    ) -> Vec<Message> {
        let mut notices = Vec::from_iter(standing_notice(
            history,
            prompt::ENVIRONMENT_MARKER,
            self.environment,
        ));
        notices.extend(relocation_notice(history, self.environment));
        for (marker, text) in [
            (prompt::INSTRUCTIONS_CHANGED_MARKER, self.instructions),
            (prompt::MEMORY_UPDATED_MARKER, self.memory),
            (prompt::CONFIGURATION_CHANGED_MARKER, self.configuration),
        ] {
            notices.extend(standing_notice(history, marker, text));
        }
        notices.extend(autopilot::standing_notice(
            history,
            self.autopilot.unwrap_or(false),
        ));
        notices.extend(standing_notice(
            history,
            prompt::TASK_MODE_MARKER,
            self.mode_notice,
        ));
        notices.extend(favorites::notice(
            history,
            listed.filter(|_| self.favorites),
        ));
        let code = available(CODE_TOOL_NAME);
        notices.extend(catalog::notice(
            history,
            self.mcp.filter(|_| code),
            code && self.decisions,
        ));
        notices.extend(mode_switch_notice_with_tools(history, self.mode, available));
        notices
    }
}

pub async fn favorite_models(policy: &ModelPolicy) -> Option<String> {
    let policy = policy.clone();
    smol::unblock(move || favorites::list(&policy)).await
}
