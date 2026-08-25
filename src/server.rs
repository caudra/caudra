use std::{
    borrow::Cow,
    collections::HashSet,
    fmt,
    path::Path,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, DiscoverResult,
        ErrorCode, ExtensionCapabilities, Implementation, InitializeRequestParams,
        InitializeResult, ListToolsResult, PaginatedRequestParams, ProgressToken, ProtocolVersion,
        RequestMetaObject, ServerCapabilities, ServerInfo, Tool,
    },
    service::{Peer, RequestContext},
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workcell_mcp_files::FileToolGroup;
use workcell_mcp_shell::{ShellPermissionPolicy, ShellToolGroup};
use workcell_mcp_web::{WebToolGroup, WebsearchExecutionConfiguration};

use crate::{
    cli::ToolGroup,
    execution_environment::{
        ExecutionEnvironmentDisclosure, TOOL_NAME as EXECUTION_ENVIRONMENT_TOOL,
        ToolGroupDisclosure, tool as execution_environment_tool,
    },
};

const MODERN_PROTOCOLS: &[ProtocolVersion] = &[ProtocolVersion::V_2026_07_28];
const DUAL_ERA_PROTOCOLS: &[ProtocolVersion] =
    &[ProtocolVersion::V_2026_07_28, ProtocolVersion::V_2025_11_25];

pub(crate) fn protocol_versions(modern_only: bool) -> &'static [ProtocolVersion] {
    if modern_only {
        MODERN_PROTOCOLS
    } else {
        DUAL_ERA_PROTOCOLS
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ServerBehavior {
    pub expose_execution_environment: bool,
    pub modern_only: bool,
}

#[derive(Clone)]
pub struct WorkcellServer {
    files: Option<FileToolGroup>,
    web: Option<WebToolGroup>,
    shell: Option<ShellToolGroup>,
    execution_environment: Option<ExecutionEnvironmentDisclosure>,
    modern_only: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerBuildError {
    Filesystem,
    DuplicateToolName,
}

impl fmt::Display for ServerBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Filesystem => "filesystem or shell tools could not be initialized",
            Self::DuplicateToolName => "tool catalog contains a duplicate name",
        })
    }
}

impl std::error::Error for ServerBuildError {}

impl WorkcellServer {
    pub async fn configured(
        root: Option<&Path>,
        allow_write: bool,
        web_configuration: WebsearchExecutionConfiguration,
        web_icons: bool,
        groups: &[ToolGroup],
        behavior: ServerBehavior,
        shell_policy: ShellPermissionPolicy,
    ) -> Result<Self, ServerBuildError> {
        let files = if groups.contains(&ToolGroup::Files) {
            Some(
                FileToolGroup::new(root.ok_or(ServerBuildError::Filesystem)?, allow_write, None)
                    .await
                    .map_err(|_| ServerBuildError::Filesystem)?,
            )
        } else {
            None
        };
        let web = groups
            .contains(&ToolGroup::Web)
            .then(|| WebToolGroup::production_with_source_icons(web_configuration, web_icons));
        let shell = if groups.contains(&ToolGroup::Shell) {
            Some(
                ShellToolGroup::with_policy(
                    root.ok_or(ServerBuildError::Filesystem)?,
                    shell_policy,
                )
                .await
                .map_err(|_| ServerBuildError::Filesystem)?,
            )
        } else {
            None
        };
        compose_catalog([
            files.as_ref().map_or_else(Vec::new, FileToolGroup::catalog),
            web.as_ref()
                .map_or_else(Vec::new, |group| group.catalog(current_utc_year())),
            shell
                .as_ref()
                .map_or_else(Vec::new, ShellToolGroup::catalog),
            if behavior.expose_execution_environment {
                vec![execution_environment_tool()]
            } else {
                Vec::new()
            },
        ])?;
        let execution_environment = if behavior.expose_execution_environment {
            Some(ExecutionEnvironmentDisclosure::collect(root).await)
        } else {
            None
        };
        Ok(Self {
            files,
            web,
            shell,
            execution_environment,
            modern_only: behavior.modern_only,
        })
    }

    #[must_use]
    pub fn catalog(&self) -> Vec<Tool> {
        compose_catalog([
            self.files
                .as_ref()
                .map_or_else(Vec::new, FileToolGroup::catalog),
            self.web
                .as_ref()
                .map_or_else(Vec::new, |group| group.catalog(current_utc_year())),
            self.shell
                .as_ref()
                .map_or_else(Vec::new, ShellToolGroup::catalog),
            self.execution_environment
                .as_ref()
                .map_or_else(Vec::new, |_| vec![execution_environment_tool()]),
        ])
        .expect("validated tool groups cannot develop duplicate names")
    }

    #[must_use]
    pub const fn modern_only(&self) -> bool {
        self.modern_only
    }

    fn validate_request_context(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<ProtocolVersion, ErrorData> {
        let requested = context.protocol_version().ok_or_else(|| {
            ErrorData::invalid_params("request protocol version is required", None)
        })?;
        let supported = self.supported_protocol_versions();
        if !supported.contains(&requested) {
            return Err(ErrorData::unsupported_protocol_version(
                requested, &supported,
            ));
        }
        if requested == ProtocolVersion::V_2026_07_28 {
            let missing = context
                .meta
                .missing_required_keys(&ProtocolVersion::V_2026_07_28);
            if !missing.is_empty() {
                return Err(ErrorData::invalid_params(
                    format!(
                        "request _meta is missing or has malformed required fields: {}",
                        missing.join(", ")
                    ),
                    None,
                ));
            }
        }
        Ok(requested)
    }

    pub async fn dispatch(
        &self,
        name: &str,
        arguments: Value,
        cancellation: CancellationToken,
    ) -> Result<CallToolResult, ErrorData> {
        self.dispatch_with_context(name, arguments, cancellation, None)
            .await
    }

    async fn dispatch_with_context(
        &self,
        name: &str,
        arguments: Value,
        cancellation: CancellationToken,
        progress: Option<ToolProgressContext>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(files) = &self.files
            && let Some(result) = files
                .dispatch(name, arguments.clone(), cancellation.clone())
                .await
        {
            return result;
        }
        if let Some(web) = &self.web
            && let Some(result) = web
                .dispatch(name, arguments.clone(), cancellation.clone())
                .await
        {
            return result;
        }
        if name == EXECUTION_ENVIRONMENT_TOOL
            && let Some(execution_environment) = &self.execution_environment
        {
            return Ok(execution_environment
                .call_tool(arguments, self.tool_group_disclosure(), cancellation)
                .await);
        }
        if let Some(shell) = &self.shell
            && let Some(result) = shell
                .dispatch(
                    name,
                    arguments,
                    cancellation,
                    progress.map(|progress| (progress.peer, progress.token)),
                )
                .await
        {
            return result;
        }
        Err(ErrorData::new(
            ErrorCode::METHOD_NOT_FOUND,
            "Unknown tool",
            None,
        ))
    }

    fn tool_group_disclosure(&self) -> ToolGroupDisclosure {
        ToolGroupDisclosure {
            files: self.files.is_some(),
            web: self.web.is_some(),
            shell: self.shell.is_some(),
        }
    }

    fn canonical_tool_name(&self, requested: &str) -> Option<String> {
        self.catalog()
            .iter()
            .find(|tool| tool.name.as_ref() == requested)
            .map(|tool| tool.name.to_string())
    }
}

impl ServerHandler for WorkcellServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("workcell-mcp", env!("CARGO_PKG_VERSION")),
        )
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.catalog()
            .iter()
            .find(|tool| tool.name.as_ref() == name)
            .cloned()
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(protocol_versions(self.modern_only))
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        if request.protocol_version == ProtocolVersion::V_2026_07_28 {
            return Err(ErrorData::invalid_request(
                "initialize is not valid for MCP 2026-07-28; use server/discover or per-request metadata",
                Some(serde_json::json!({"supported": self.supported_protocol_versions()})),
            ));
        }
        if self.modern_only || request.protocol_version != ProtocolVersion::V_2025_11_25 {
            return Err(ErrorData::unsupported_protocol_version(
                request.protocol_version,
                &self.supported_protocol_versions(),
            ));
        }
        context.peer.set_peer_info(request);
        let mut info = self.get_info();
        info.protocol_version = ProtocolVersion::V_2025_11_25;
        Ok(info)
    }

    async fn discover(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, ErrorData> {
        let mut request = InitializeRequestParams::default();
        request.protocol_version = ProtocolVersion::V_2026_07_28;
        request.capabilities = context.client_capabilities().unwrap_or_default();
        let mut info = self.get_info();
        info.protocol_version = ProtocolVersion::V_2026_07_28;
        if let Some(execution_environment) = self
            .execution_environment
            .as_ref()
            .filter(|_| requests_execution_environment(&request, &context.meta))
        {
            let mut extensions = ExtensionCapabilities::new();
            extensions.insert(
                crate::execution_environment::EXTENSION_ID.into(),
                execution_environment.discovery_descriptor(self.tool_group_disclosure()),
            );
            info.capabilities.extensions = Some(extensions);
        }
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            info,
        ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let protocol_version = self.validate_request_context(&context)?;
        let result = ListToolsResult::with_all_items(self.catalog());
        if protocol_version == ProtocolVersion::V_2026_07_28 {
            Ok(result.with_ttl_ms(0).with_cache_scope(CacheScope::Private))
        } else {
            Ok(result)
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.validate_request_context(&context)?;
        let Some(tool_name) = self.canonical_tool_name(request.name.as_ref()) else {
            return Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                "Unknown tool",
                None,
            ));
        };
        let request_id = format!("tool_{}", Uuid::new_v4());
        let started = Instant::now();
        tracing::debug!(
            operation = "mcp.tool.started",
            request_id,
            tool = tool_name.as_str(),
            "tool call started"
        );
        let progress = context
            .meta
            .get_progress_token()
            .map(|token| ToolProgressContext {
                peer: context.peer.clone(),
                token,
            });
        let result = self
            .dispatch_with_context(
                &tool_name,
                Value::Object(request.arguments.unwrap_or_default()),
                context.ct,
                progress,
            )
            .await;
        let outcome = match &result {
            Ok(value) if value.is_error == Some(true) => "tool_error",
            Ok(_) => "completed",
            Err(_) => "protocol_error",
        };
        tracing::debug!(
            operation = "mcp.tool.completed",
            request_id,
            tool = tool_name.as_str(),
            duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            outcome,
            "tool call completed"
        );
        result.map(Into::into)
    }
}

struct ToolProgressContext {
    peer: Peer<RoleServer>,
    token: ProgressToken,
}

fn requests_execution_environment(
    request: &InitializeRequestParams,
    meta: &RequestMetaObject,
) -> bool {
    request
        .capabilities
        .extensions
        .as_ref()
        .and_then(|extensions| extensions.get(crate::execution_environment::EXTENSION_ID))
        .or_else(|| {
            meta.0
                .0
                .get(crate::execution_environment::EXTENSION_ID)?
                .as_object()
        })
        .and_then(|settings| settings.get("versions"))
        .and_then(Value::as_array)
        .is_some_and(|versions| versions.iter().any(|version| version == "v1"))
}

fn compose_catalog(
    groups: impl IntoIterator<Item = Vec<Tool>>,
) -> Result<Vec<Tool>, ServerBuildError> {
    let mut names = HashSet::new();
    let mut catalog = Vec::new();
    for tool in groups.into_iter().flatten() {
        if !names.insert(tool.name.to_string()) {
            return Err(ServerBuildError::DuplicateToolName);
        }
        catalog.push(tool);
    }
    Ok(catalog)
}

fn current_utc_year() -> i32 {
    let days_since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() / 86_400);
    year_from_unix_days(i64::try_from(days_since_epoch).unwrap_or(i64::MAX))
}

fn year_from_unix_days(days: i64) -> i32 {
    let shifted = days.saturating_add(719_468);
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    if month_prime >= 10 {
        year += 1;
    }
    i32::try_from(year).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use workcell_mcp_files::catalog as file_catalog;
    use workcell_mcp_web::{WebsearchExecutionConfiguration, catalog as web_catalog};

    use super::*;

    #[test]
    fn composed_catalog_is_exact_and_ordered() {
        let names = compose_catalog([
            file_catalog(),
            web_catalog(2026, &WebsearchExecutionConfiguration::unconfigured()),
            workcell_mcp_shell::catalog(),
            vec![execution_environment_tool()],
        ])
        .unwrap()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "file_read",
                "file_glob",
                "file_grep",
                "file_write",
                "file_edit",
                "file_apply_patch",
                "websearch",
                "webfetch",
                "shell",
                "execution_environment",
            ]
        );
    }

    #[test]
    fn calendar_conversion_covers_year_boundaries() {
        assert_eq!(year_from_unix_days(0), 1970);
        assert_eq!(year_from_unix_days(19_723), 2024);
    }

    #[tokio::test]
    async fn execution_environment_tool_follows_disclosure_switch() {
        let disabled = WorkcellServer::configured(
            None,
            false,
            WebsearchExecutionConfiguration::unconfigured(),
            false,
            &[],
            ServerBehavior {
                expose_execution_environment: false,
                modern_only: false,
            },
            ShellPermissionPolicy::restricted(),
        )
        .await
        .unwrap();
        assert!(disabled.catalog().is_empty());
        assert!(
            disabled
                .dispatch(
                    EXECUTION_ENVIRONMENT_TOOL,
                    serde_json::json!({}),
                    CancellationToken::new(),
                )
                .await
                .is_err()
        );

        let enabled = WorkcellServer::configured(
            None,
            false,
            WebsearchExecutionConfiguration::unconfigured(),
            false,
            &[],
            ServerBehavior {
                expose_execution_environment: true,
                modern_only: false,
            },
            ShellPermissionPolicy::restricted(),
        )
        .await
        .unwrap();
        assert_eq!(enabled.catalog()[0].name, EXECUTION_ENVIRONMENT_TOOL);
    }
}
