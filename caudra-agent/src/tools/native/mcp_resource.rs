//! `mcp_resource`: list what running MCP servers offer to be read, and read
//! it. Listing answers from the latest snapshot; reading asks the server.

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::agent::tool_dispatch::mcp_call_scope;
use crate::mcp::output::{McpBlock, McpOutput, decoded_len};
use crate::mcp::resources::ResourceContents;
use crate::mcp::{McpServerResources, McpSnapshot};
use crate::permissions::{
    MCP_SERVER_RESOURCE, PermissionResource, PermissionResourceAccess, PermissionResourceKind,
    PermissionRisk,
};
use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, PermissionIntent, PermissionScopes, Tool,
    ToolError, ToolExecResult, ToolFailure, ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{BoxFuture, DescriptionContext, MCP_RESOURCE_TOOL_NAME, ToolContext};
use crate::types::ToolOutput;

pub const DESCRIPTION: &str = "List or read the resources that running MCP servers offer: documents, records, files and other context a server exposes to be read rather than called.

- `action: \"list\"` names every resource and resource template, or only those of `server`. Pass the `cursor` a listing returns to read the next page.
- `action: \"read\"` reads `uri` from `server`. A URI spelled the way a listed template describes can be read too.
- Text comes back as written. Images and other binary contents are named with their type and size.";

const LIST_PAGE_SIZE: usize = 100;
const LIST_ACTION: &str = "list";
const READ_ACTION: &str = "read";
const SERVER_SEPARATOR: &str = ":";
const MODEL_SEPARATOR: &str = "\n\n";
const TEMPLATE_NOTE: &str = "template";
const NO_RESOURCES: &str = "No running MCP server offers resources.";
const NO_MCP: &str = "No MCP servers are running in this session.";
const READ_NEEDS_TARGET: &str = "read needs both `server` and `uri`";
const BAD_CURSOR: &str = "cursor is not one a listing returned";
const EMPTY_READ: &str = "The server returned no contents.";

static ACTION_PARAM: ParamSchema = ParamSchema::Enum {
    variants: &[LIST_ACTION, READ_ACTION],
    description: "`list` to see what can be read, `read` to read one resource",
};
static SERVER_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "The MCP server's configured name. Required to read; narrows a listing",
};
static URI_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "The resource URI to read, as listed or as a template spells it",
};
static CURSOR_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "The cursor a previous listing returned, to list the next page",
};
static PROPERTIES: &[Property] = &[
    ("action", &ACTION_PARAM, true, &[]),
    ("server", &SERVER_PARAM, false, &[]),
    ("uri", &URI_PARAM, false, &[]),
    ("cursor", &CURSOR_PARAM, false, &[]),
];
static SCHEMA: ParamSchema = ParamSchema::Object {
    properties: PROPERTIES,
    description: "",
    reject_unknown: false,
};

pub struct McpResourceTool;

impl Tool for McpResourceTool {
    fn name(&self) -> &str {
        MCP_RESOURCE_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(DESCRIPTION)
    }

    fn schema(&self) -> Value {
        to_json_schema(&SCHEMA)
    }

    fn tool_kind(&self) -> Option<&str> {
        Some(READ_ACTION)
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let input = validate(&SCHEMA, input.clone())?;
        let call: ResourceCall =
            serde_json::from_value(input).map_err(|error| ParseError::custom(error.to_string()))?;
        if let ResourceCall::Read { server, uri } = &call
            && (server.trim().is_empty() || uri.trim().is_empty())
        {
            return Err(ParseError::custom(READ_NEEDS_TARGET));
        }
        Ok(Box::new(call))
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum ResourceCall {
    List {
        #[serde(default)]
        server: Option<String>,
        #[serde(default)]
        cursor: Option<String>,
    },
    Read {
        #[serde(default)]
        server: String,
        #[serde(default)]
        uri: String,
    },
}

impl ResourceCall {
    /// The servers this call reads from, which is what a grant names: every
    /// URI of a server the user trusts with reads is covered alike.
    fn servers(&self, snapshot: &McpSnapshot) -> Vec<String> {
        match self {
            Self::List {
                server: Some(server),
                ..
            }
            | Self::Read { server, .. } => vec![server.clone()],
            Self::List { server: None, .. } => snapshot
                .resources
                .iter()
                .map(|offer| offer.server.clone())
                .collect(),
        }
    }

    fn scope(&self, servers: &[String]) -> String {
        match self {
            Self::List { .. } => format!("{LIST_ACTION} {}", servers.join(", ")),
            Self::Read { server, uri } => format!("{server}{SERVER_SEPARATOR}{uri}"),
        }
    }
}

impl ToolInvocation for ResourceCall {
    fn start_header(&self) -> HeaderFuture {
        let header = match self {
            Self::List { server: None, .. } => LIST_ACTION.to_owned(),
            Self::List {
                server: Some(server),
                ..
            } => format!("{LIST_ACTION} {server}"),
            Self::Read { server, uri } => format!("{server}{SERVER_SEPARATOR}{uri}"),
        };
        HeaderFuture::Ready(HeaderResult::plain(header))
    }

    fn preflight<'a>(
        &'a self,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<Option<PermissionIntent>, ToolError>> {
        Box::pin(async move {
            let snapshot = ctx
                .mcp
                .as_ref()
                .map(|mcp| mcp.reader().load_full())
                .unwrap_or_default();
            Ok(Some(read_intent(self, &self.servers(&snapshot))))
        })
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            let Some(mcp) = ctx.mcp.as_ref() else {
                return ToolExecResult::failed(ToolFailure::NotFound, NO_MCP);
            };
            match *self {
                Self::List { server, cursor } => {
                    list(&mcp.reader().load(), server.as_deref(), cursor.as_deref())
                }
                Self::Read { server, uri } => {
                    match mcp.read_resource(&server, &uri, &mcp_call_scope(ctx)).await {
                        Ok(contents) => read_output(contents),
                        Err(error) => ToolExecResult::failed(error.failure(), error.to_string()),
                    }
                }
            }
        })
    }
}

fn read_intent(call: &ResourceCall, servers: &[String]) -> PermissionIntent {
    PermissionIntent::new(
        PermissionScopes::single(call.scope(servers)),
        servers
            .iter()
            .map(|server| PermissionResource {
                kind: PermissionResourceKind::Custom {
                    name: MCP_SERVER_RESOURCE.to_owned(),
                },
                value: server.clone(),
                access: Some(PermissionResourceAccess::Read),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            })
            .collect(),
        PermissionRisk::Low,
    )
}

/// One page of every resource and template the running servers offer, or
/// only `server`'s. The cursor is where the page starts, so it stays valid
/// for as long as the lists do.
fn list(snapshot: &McpSnapshot, server: Option<&str>, cursor: Option<&str>) -> ToolExecResult {
    let offers: Vec<&McpServerResources> = match server {
        Some(server) => match snapshot.resources_of(server) {
            Some(offer) => vec![offer],
            None => {
                return ToolExecResult::failed(
                    ToolFailure::NotFound,
                    format!("MCP server `{server}` is not running or offers no resources"),
                );
            }
        },
        None => snapshot.resources.iter().collect(),
    };
    let start = match cursor.map(str::parse::<usize>).transpose() {
        Ok(start) => start.unwrap_or_default(),
        Err(_) => return ToolExecResult::failed(ToolFailure::InvalidInput, BAD_CURSOR),
    };
    let entries: Vec<(McpBlock, String)> = offers.iter().flat_map(|offer| entries(offer)).collect();
    if entries.is_empty() {
        return ToolExecResult::from(Ok(ToolOutput::Mcp(McpOutput::default())))
            .with_model_output(Some(NO_RESOURCES.to_owned()));
    }
    let end = entries.len().min(start.saturating_add(LIST_PAGE_SIZE));
    let page = entries.get(start..end).unwrap_or_default();
    let mut model = page
        .iter()
        .map(|(_, line)| line.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if end < entries.len() {
        model.push_str(&format!(
            "\n{} more. Pass cursor \"{end}\" to list them.",
            entries.len() - end
        ));
    }
    let output = McpOutput {
        blocks: page.iter().map(|(block, _)| block.clone()).collect(),
        ..McpOutput::default()
    };
    ToolExecResult::from(Ok(ToolOutput::Mcp(output))).with_model_output(Some(model))
}

/// Each resource and template as a card's link and the model's line.
fn entries(offer: &McpServerResources) -> Vec<(McpBlock, String)> {
    let qualified = |uri: &str| format!("{}{SERVER_SEPARATOR}{uri}", offer.server);
    let resources = offer.lists.resources.iter().map(|resource| {
        let uri = qualified(&resource.uri);
        let line = describe(&uri, resource.label(), resource.mime_type.as_deref(), None);
        let block = McpBlock::ResourceLink {
            uri,
            name: Some(resource.label().to_owned()).filter(|name| !name.is_empty()),
            mime_type: resource.mime_type.clone(),
        };
        (block, line)
    });
    let templates = offer.lists.templates.iter().map(|template| {
        let uri = qualified(&template.uri_template);
        let line = describe(
            &uri,
            template.label(),
            template.mime_type.as_deref(),
            Some(TEMPLATE_NOTE),
        );
        let block = McpBlock::ResourceLink {
            uri,
            name: Some(format!("{} ({TEMPLATE_NOTE})", template.label())),
            mime_type: template.mime_type.clone(),
        };
        (block, line)
    });
    resources.chain(templates).collect()
}

/// `server:uri (Label, text/markdown)`.
fn describe(uri: &str, label: &str, mime_type: Option<&str>, note: Option<&str>) -> String {
    let details: Vec<&str> = [note, Some(label), mime_type]
        .into_iter()
        .flatten()
        .filter(|detail| !detail.is_empty())
        .collect();
    if details.is_empty() {
        uri.to_owned()
    } else {
        format!("{uri} ({})", details.join(", "))
    }
}

/// The card keeps each content's URI, text and size; the model reads the
/// text, and only a label for a blob it could not read.
fn read_output(contents: Vec<ResourceContents>) -> ToolExecResult {
    let model = contents
        .iter()
        .filter_map(|content| content.text.clone().or_else(|| content.blob_label()))
        .collect::<Vec<_>>()
        .join(MODEL_SEPARATOR);
    let blocks = contents.into_iter().map(read_block).collect();
    let model = if model.is_empty() {
        EMPTY_READ.to_owned()
    } else {
        model
    };
    ToolExecResult::from(Ok(ToolOutput::Mcp(McpOutput {
        blocks,
        ..McpOutput::default()
    })))
    .with_model_output(Some(model))
}

fn read_block(content: ResourceContents) -> McpBlock {
    let bytes = content.blob.as_deref().map(decoded_len);
    match bytes {
        Some(bytes) if content.is_image() => McpBlock::Image {
            mime_type: content.mime_type,
            bytes,
        },
        _ => McpBlock::Resource {
            uri: content.uri,
            mime_type: content.mime_type,
            text: content.text,
            bytes,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use caudra_config::{FeatureFlags, PermissionsConfig, ToolKey};
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        BAD_CURSOR, EMPTY_READ, LIST_PAGE_SIZE, McpResourceTool, NO_RESOURCES, READ_NEEDS_TARGET,
        list, read_output,
    };
    use crate::AgentMode;
    use crate::agent::tool_dispatch::{Emit, run};
    use crate::mcp::output::McpBlock;
    use crate::mcp::resources::{Resource, ResourceContents, ResourceLists, ResourceTemplate};
    use crate::mcp::{McpSnapshot, resource_stub_session};
    use crate::permissions::{
        MCP_SERVER_READS_OPTION, PERMISSION_DENIED_PREFIX, PermissionLifetime, PermissionManager,
        PermissionMode, PermissionRequest,
    };
    use crate::tools::native::register;
    use crate::tools::registry::{Tool, ToolFailure};
    use crate::tools::test_support::{stub_ctx, stub_ctx_with_permissions};
    use crate::tools::{MCP_RESOURCE_TOOL_NAME, ToolContext};
    use crate::types::ToolOutput;

    const SERVER: &str = "stub";
    const README_URI: &str = "docs://readme";
    const README_TITLE: &str = "Read me";
    const README_TEXT: &str = "hello from the server";
    const TEMPLATE_URI: &str = "docs://pages/{page}";
    const TEMPLATE_NAME: &str = "Page";
    const MARKDOWN: &str = "text/markdown";
    const PDF: &str = "application/pdf";
    const PNG: &str = "image/png";
    const BLOB: &str = "AAAA";
    const BLOB_LABEL: &str = "[blob: application/pdf, 3 bytes]";
    const READS_LABEL: &str = "reads from stub";
    const CWD: &str = "/project";
    const REQUEST_ID: &str = "mcp-resource-request";
    const PLAN_FILE: &str = "/tmp/plan.md";

    fn lists(resources: usize) -> ResourceLists {
        let mut listed = vec![Resource {
            uri: README_URI.into(),
            name: "readme".into(),
            title: Some(README_TITLE.into()),
            mime_type: Some(MARKDOWN.into()),
            ..Resource::default()
        }];
        listed.extend((1..resources).map(|n| Resource {
            uri: format!("docs://{n}"),
            name: format!("doc {n}"),
            ..Resource::default()
        }));
        ResourceLists {
            resources: listed,
            templates: vec![ResourceTemplate {
                uri_template: TEMPLATE_URI.into(),
                name: TEMPLATE_NAME.into(),
                ..ResourceTemplate::default()
            }],
        }
    }

    fn ctx_under(
        mode: AgentMode,
        permission_mode: PermissionMode,
        resources: usize,
        read: Value,
    ) -> (TempDir, ToolContext) {
        let dir = TempDir::new().unwrap();
        let permissions = Arc::new(PermissionManager::new_nonpersistent(
            PermissionsConfig::default(),
            dir.path().to_path_buf(),
            Arc::default(),
        ));
        permissions.set_session_mode(Some(permission_mode));
        let mut ctx = stub_ctx_with_permissions(&mode, permissions);
        ctx.mcp = Some(resource_stub_session(lists(resources), read));
        register(&ctx.registry, FeatureFlags::all()).unwrap();
        (dir, ctx)
    }

    fn ctx_with(resources: usize, read: Value) -> (TempDir, ToolContext) {
        ctx_under(AgentMode::Build, PermissionMode::Yolo, resources, read)
    }

    fn text_read() -> Value {
        json!({"contents": [{"uri": README_URI, "mimeType": MARKDOWN, "text": README_TEXT}]})
    }

    async fn call(ctx: &ToolContext, input: Value) -> (String, bool, ToolOutput) {
        let done = run(
            &ctx.registry,
            ctx.mcp.as_ref(),
            MCP_RESOURCE_TOOL_NAME.into(),
            MCP_RESOURCE_TOOL_NAME,
            &input,
            ctx,
            Emit::Silent,
        )
        .await;
        let model = done
            .model_output
            .clone()
            .unwrap_or_else(|| done.output.as_text());
        (model, done.is_error, done.output)
    }

    /// Reading stays in reach while planning and under a read-only policy,
    /// and asks unless the permission mode says otherwise.
    #[test_case(AgentMode::Plan(PathBuf::from(PLAN_FILE)), PermissionMode::Yolo => true ; "plan_reads")]
    #[test_case(AgentMode::ReadOnly, PermissionMode::Yolo => true ; "read_only_policy_reads")]
    #[test_case(AgentMode::Plan(PathBuf::from(PLAN_FILE)), PermissionMode::Ask => false ; "plan_still_asks")]
    #[test_case(AgentMode::Build, PermissionMode::Ask => false ; "build_asks_by_default")]
    fn a_read_reaches_plan_and_read_only_modes(
        mode: AgentMode,
        permission_mode: PermissionMode,
    ) -> bool {
        smol::block_on(async {
            let (_dir, ctx) = ctx_under(mode, permission_mode, 1, text_read());
            assert!(ctx.tool_available(MCP_RESOURCE_TOOL_NAME));
            let (model, is_error, output) = call(
                &ctx,
                json!({"action": "read", "server": SERVER, "uri": README_URI}),
            )
            .await;
            if is_error {
                assert!(
                    output.as_text().starts_with(PERMISSION_DENIED_PREFIX),
                    "{model}"
                );
                return false;
            }
            assert_eq!(model, README_TEXT);
            assert!(matches!(output, ToolOutput::Mcp(_)));
            true
        })
    }

    #[test_case(None, false ; "without_a_server")]
    #[test_case(Some(ResourceLists::default()), false ; "a_server_that_lists_nothing")]
    #[test_case(Some(lists(1)), true ; "a_server_that_lists_something")]
    fn the_tool_is_offered_only_while_a_server_lists_resources(
        listed: Option<ResourceLists>,
        offered: bool,
    ) {
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.mcp = listed.map(|lists| resource_stub_session(lists, text_read()));
        register(&ctx.registry, FeatureFlags::all()).unwrap();
        assert_eq!(ctx.tool_available(MCP_RESOURCE_TOOL_NAME), offered);
    }

    #[test_case(json!({"contents": [{"uri": README_URI, "mimeType": PDF, "blob": BLOB}]}), BLOB_LABEL ; "a_blob_is_labelled")]
    #[test_case(json!({"contents": []}), EMPTY_READ ; "nothing_says_so")]
    fn a_read_without_text_tells_the_model_what_came(read: Value, expected: &str) {
        smol::block_on(async {
            let (_dir, ctx) = ctx_with(1, read);
            let (model, is_error, _) = call(
                &ctx,
                json!({"action": "read", "server": SERVER, "uri": README_URI}),
            )
            .await;
            assert!(!is_error);
            assert_eq!(model, expected);
        });
    }

    #[test]
    fn an_image_is_carded_as_an_image() {
        let result = read_output(vec![ResourceContents {
            uri: README_URI.into(),
            mime_type: Some(PNG.into()),
            text: None,
            blob: Some(BLOB.into()),
        }]);
        let Ok(ToolOutput::Mcp(output)) = result.output else {
            panic!("expected an MCP card");
        };
        assert!(matches!(
            output.blocks.as_slice(),
            [McpBlock::Image { bytes: 3, .. }]
        ));
    }

    #[test]
    fn a_listing_names_resources_and_templates() {
        smol::block_on(async {
            let (_dir, ctx) = ctx_with(1, text_read());
            let (model, is_error, _) = call(&ctx, json!({"action": "list"})).await;
            assert!(!is_error);
            assert_eq!(
                model,
                format!(
                    "{SERVER}:{README_URI} ({README_TITLE}, {MARKDOWN})\n\
                     {SERVER}:{TEMPLATE_URI} (template, {TEMPLATE_NAME})"
                )
            );
        });
    }

    #[test]
    fn a_long_listing_pages_by_cursor() {
        smol::block_on(async {
            let (_dir, ctx) = ctx_with(LIST_PAGE_SIZE, text_read());
            let (first, ..) = call(&ctx, json!({"action": "list", "server": SERVER})).await;
            assert_eq!(first.lines().count(), LIST_PAGE_SIZE + 1);
            assert!(first.ends_with(&format!(
                "1 more. Pass cursor \"{LIST_PAGE_SIZE}\" to list them."
            )));
            let (rest, ..) = call(
                &ctx,
                json!({"action": "list", "cursor": LIST_PAGE_SIZE.to_string()}),
            )
            .await;
            assert!(rest.starts_with(&format!("{SERVER}:{TEMPLATE_URI}")));
        });
    }

    #[test_case(json!({"action": "list", "cursor": "later"}), BAD_CURSOR ; "a_foreign_cursor")]
    fn a_bad_listing_is_refused(input: Value, expected: &str) {
        smol::block_on(async {
            let (_dir, ctx) = ctx_with(1, text_read());
            let (model, is_error, _) = call(&ctx, input).await;
            assert!(is_error);
            assert!(model.contains(expected), "{model}");
        });
    }

    #[test]
    fn a_read_needs_a_server_and_a_uri() {
        let error = McpResourceTool
            .parse(&json!({"action": "read", "server": SERVER}))
            .err()
            .expect("a read without a URI parses");
        assert!(error.to_string().contains(READ_NEEDS_TARGET));
    }

    #[test]
    fn nothing_to_list_says_so() {
        let result = list(&McpSnapshot::default(), None, None);
        assert_eq!(result.model_output.as_deref(), Some(NO_RESOURCES));
    }

    #[test]
    fn an_unknown_server_is_not_found() {
        let result = list(&McpSnapshot::default(), Some(SERVER), None);
        assert_eq!(result.failure, Some(ToolFailure::NotFound));
    }

    /// The prompt offers to trust every read from the server, for as long as
    /// the user likes, without pinning the URI.
    #[test]
    fn the_prompt_offers_reads_from_the_server() {
        smol::block_on(async {
            let (_dir, ctx) = ctx_under(
                AgentMode::Plan(PathBuf::from(PLAN_FILE)),
                PermissionMode::Ask,
                1,
                text_read(),
            );
            let input = json!({"action": "read", "server": SERVER, "uri": README_URI});
            let call = McpResourceTool.parse(&input).unwrap();
            let intent = call.preflight(&ctx).await.unwrap().unwrap();
            let request = PermissionRequest::from_intent(
                REQUEST_ID.into(),
                ToolKey::native(MCP_RESOURCE_TOOL_NAME),
                &intent,
                input,
                Path::new(CWD),
            );
            let option = request
                .options
                .iter()
                .find(|option| option.id == MCP_SERVER_READS_OPTION)
                .expect("no option to allow reads from the server");
            assert_eq!(option.label, READS_LABEL);
            assert_eq!(
                option.allowed_lifetimes,
                [
                    PermissionLifetime::Conversation,
                    PermissionLifetime::Project,
                    PermissionLifetime::Global,
                ]
            );
        });
    }
}
