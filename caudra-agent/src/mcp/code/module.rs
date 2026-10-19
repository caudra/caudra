//! MCP as a host module: every server a namespace whose async methods are its tools, beside the
//! `mcp` namespace whose helpers find tools and read resources.
//!
//! A tool call is planned the way a direct MCP call was checked: its binding, the agent's
//! `disabled_tools`, and the permission intent its recognised server shapes. Whether it is
//! read-only decides plan and read-only modes, which the gateway applies.

use std::collections::BTreeMap;
use std::future;

use caudra_config::ToolKey;
use serde_json::{Map, Value, json};

use super::catalog::unavailable;
use super::naming::{CLASS_PREFIX, Names};
use super::search::{DEFAULT_LIMIT, MAX_LIMIT, search};
use super::signature::stubs;
use super::view::ServerView;
use crate::code::args::{bind, count, required_text, text};
use crate::code::gateway::Permission;
use crate::code::module::{
    CallFailure, CallRun, HostCallPlan, HostModule, MemberDoc, ModuleDoc, Prepared,
};
use crate::code::names::{
    MCP_MODULE, READ_RESOURCE_HELPER, RESOURCES_HELPER, SEARCH_HELPER, SERVERS_HELPER,
};
use crate::code::output::CodeCall;
use crate::code::runner::{
    ScriptCall, ScriptError, ScriptErrorKind, ScriptInvocation, ScriptName, ScriptReply,
};
use crate::mcp::call::CallScope;
use crate::mcp::output::{McpBlock, McpOutput, decoded_len};
use crate::mcp::resources::{ResourceContents, ResourceLists};
use crate::mcp::{McpSession, SEPARATOR};
use crate::permissions::{
    MCP_SERVER_RESOURCE, PermissionExecutorKind, PermissionResource, PermissionResourceAccess,
    PermissionResourceKind, PermissionRisk, mcp_permission_intent,
};
use crate::tools::registry::{PermissionIntent, PermissionScopes, TrustedToolSource};
use crate::tools::{CODE_TOOL_NAME, ToolContext};

pub static DOC: ModuleDoc = ModuleDoc {
    namespace: MCP_MODULE,
    summary: "`mcp`: each server the `# MCP servers` reminder lists is a namespace of its own (`await github.list_issues(owner=\"o\")`); `mcp.search(\"query\")` finds the tools it leaves out, `mcp.servers()`, `mcp.resources`, `mcp.read_resource`.",
    page: "mcp: the MCP servers this session connects. Each server is a namespace whose tools are async methods: `issues = await github.list_issues(owner=\"o\", repo=\"r\")`, or pass the arguments as one dict. Run independent calls together with `await asyncio.gather(...)`.\n\
A tool returns its structured content, else its text, parsed when it is JSON. Images and other media come back as a dict naming their type and size. A tool that reports an error raises `RuntimeError` with its text.\n\
`help(\"github\")` lists a server's tools with their signatures, and `help(\"github.list_issues\")` shows one.",
    members: &[
        MemberDoc {
            name: SEARCH_HELPER,
            signature: "search(query: str, server: str | None = None, limit: int = 10, offset: int = 0) -> list[dict[str, Any]]",
            awaited: false,
            read_first: false,
            page: "Finds tools across the connected servers by name and description, best first, as `[{\"name\", \"signature\", \"doc\"}]` with the exact signature to call. Answers at once, so never `await` it.",
        },
        MemberDoc {
            name: SERVERS_HELPER,
            signature: "servers() -> list[dict[str, Any]]",
            awaited: false,
            read_first: false,
            page: "Lists every configured server as `{\"name\", \"server\", \"title\", \"status\", \"tools\", \"resources\"}`, where `name` is its namespace and `status` says whether it is connected. Answers at once, so never `await` it.",
        },
        MemberDoc {
            name: RESOURCES_HELPER,
            signature: "resources(server: str) -> list[dict[str, Any]]",
            awaited: true,
            read_first: false,
            page: "Lists what a server offers to be read, resources and templates, by namespace or configured name.",
        },
        MemberDoc {
            name: READ_RESOURCE_HELPER,
            signature: "read_resource(server: str, uri: str) -> Any",
            awaited: true,
            read_first: false,
            page: "Reads one resource. It asks first, scoped to the server and URI, and is allowed while planning.",
        },
    ],
};
const SEARCH_CALL: &str = "mcp.search";
const RESOURCES_CALL: &str = "mcp.resources";
const READ_RESOURCE_CALL: &str = "mcp.read_resource";
const PROPERTIES: &str = "properties";
const REQUIRED: &str = "required";
const ADDITIONAL_PROPERTIES: &str = "additionalProperties";
const QUERY_PARAM: &str = "query";
const SERVER_PARAM: &str = "server";
const LIMIT_PARAM: &str = "limit";
const OFFSET_PARAM: &str = "offset";
const URI_PARAM: &str = "uri";
const SEARCH_PARAMS: &[&str] = &[QUERY_PARAM, SERVER_PARAM, LIMIT_PARAM, OFFSET_PARAM];
const RESOURCES_PARAMS: &[&str] = &[SERVER_PARAM];
const READ_RESOURCE_PARAMS: &[&str] = &[SERVER_PARAM, URI_PARAM];
const RESOURCE_SCOPE_SEPARATOR: &str = ":";
const CONNECTED: &str = "connected";
const IMAGE_TYPE: &str = "image";
const AUDIO_TYPE: &str = "audio";
const RESOURCE_TYPE: &str = "resource";
const RESOURCE_LINK_TYPE: &str = "resource_link";
/// What the type checker says after the stub class of a namespace whose tool it cannot find.
const NO_ATTRIBUTE: &str = " has no attribute `";
const QUOTE: char = '`';

/// The servers one run reaches, named as they were when the run started.
pub struct McpModule {
    session: McpSession,
    servers: Vec<ServerView>,
    names: Names,
}

impl McpModule {
    pub fn new(session: McpSession) -> Self {
        let servers = session.servers();
        let names = Names::new(&servers);
        Self {
            session,
            servers,
            names,
        }
    }

    fn view(&self, server: &str) -> Option<&ServerView> {
        self.servers.iter().find(|view| view.name == server)
    }

    /// The configured server a snippet names by namespace or by configured name.
    fn server_named(&self, requested: &str) -> Result<String, ScriptError> {
        let names = &self.names;
        names
            .server(requested)
            .or_else(|| names.namespace(requested).and(Some(requested)))
            .map(str::to_owned)
            .ok_or_else(|| {
                let suggestion = names
                    .suggest_namespace(requested)
                    .map(|namespace| format!(" Did you mean `{namespace}`?"))
                    .unwrap_or_default();
                ScriptError::new(
                    ScriptErrorKind::Value,
                    format!("No MCP server `{requested}`.{suggestion}"),
                )
            })
    }

    fn search(&self, call: &ScriptCall) -> ScriptReply {
        let arguments = bind(SEARCH_CALL, SEARCH_PARAMS, call)?;
        let query = text(&arguments, SEARCH_CALL, QUERY_PARAM)?.unwrap_or_default();
        let server = text(&arguments, SEARCH_CALL, SERVER_PARAM)?;
        let limit = count(&arguments, SEARCH_CALL, LIMIT_PARAM)?.unwrap_or(DEFAULT_LIMIT);
        let offset = count(&arguments, SEARCH_CALL, OFFSET_PARAM)?.unwrap_or_default();
        let hits = search(
            &self.servers,
            &self.names,
            &query,
            server.as_deref(),
            limit,
            offset,
        )
        .map_err(|message| ScriptError::new(ScriptErrorKind::Value, message))?;
        Ok(hits
            .into_iter()
            .map(|hit| {
                json!({
                    "name": format!("{}.{}", hit.namespace, hit.method),
                    "signature": format!("{}.{}", hit.namespace, hit.signature),
                    "doc": hit.doc,
                })
            })
            .collect())
    }

    /// A server's tools, or one of them, as `help()` shows them: each signature with its doc.
    fn server_help(&self, namespace: &str, method: Option<&str>) -> Option<String> {
        let server = self.names.server(namespace)?;
        let view = self.view(server)?;
        if let Some(reason) = unavailable(&view.state) {
            return Some(format!(
                "MCP server `{namespace}` is not connected ({reason})"
            ));
        }
        let hits = search(
            &self.servers,
            &self.names,
            method.unwrap_or_default(),
            Some(namespace),
            MAX_LIMIT,
            0,
        )
        .ok()?;
        let lines: Vec<String> = hits
            .into_iter()
            .filter(|hit| method.is_none_or(|method| hit.method == method))
            .map(|hit| format!("await {}.{}\n    {}", hit.namespace, hit.signature, hit.doc))
            .collect();
        match (method, lines.is_empty()) {
            (Some(method), true) => Some(no_tool_message(&self.names, namespace, method)),
            _ => Some(format!(
                "MCP server `{namespace}` ({server}):\n{}",
                lines.join("\n")
            )),
        }
    }

    fn server_list(&self) -> Value {
        self.names
            .namespaces()
            .filter_map(|(namespace, server)| {
                let view = self.view(server)?;
                Some(json!({
                    "name": namespace,
                    "server": server,
                    "title": view.title,
                    "status": unavailable(&view.state).unwrap_or_else(|| CONNECTED.to_owned()),
                    "tools": view.tools.len(),
                    "resources": view.resources,
                }))
            })
            .collect()
    }

    fn resources(&self, call: &ScriptCall) -> ScriptReply {
        let arguments = bind(RESOURCES_CALL, RESOURCES_PARAMS, call)?;
        let requested = required_text(&arguments, RESOURCES_CALL, SERVER_PARAM)?;
        let server = self.server_named(&requested)?;
        let snapshot = self.session.reader().load_full();
        Ok(snapshot
            .resources_of(&server)
            .map(|offer| resource_entries(&offer.lists))
            .unwrap_or_default())
    }

    /// Reading a server's resources asks as the `code` tool, scoped to the server and URI, and
    /// stays in reach while planning and under a read-only policy. The built-in allow that lets
    /// the `code` tool itself run unasked never covers a read.
    fn read_resource(
        &self,
        ctx: &ToolContext,
        call: &ScriptCall,
    ) -> Result<HostCallPlan, ScriptError> {
        let arguments = bind(READ_RESOURCE_CALL, READ_RESOURCE_PARAMS, call)?;
        let requested = required_text(&arguments, READ_RESOURCE_CALL, SERVER_PARAM)?;
        let uri = required_text(&arguments, READ_RESOURCE_CALL, URI_PARAM)?;
        let server = self.server_named(&requested)?;
        let input = json!({ URI_PARAM: uri });
        let row = CodeCall {
            server: server.clone(),
            tool: READ_RESOURCE_HELPER.to_owned(),
            called_as: format!(
                "{READ_RESOURCE_CALL}({}, {})",
                Value::from(requested),
                Value::from(uri.as_str())
            ),
            input,
            ..CodeCall::default()
        };
        let identity = ctx
            .registry
            .get(CODE_TOOL_NAME)
            .and_then(|entry| TrustedToolSource::from_registered(&entry, None))
            .map(|source| (source.subject().clone(), source.executor().clone()));
        let intent = PermissionIntent::new(
            PermissionScopes::single(format!("{server}{RESOURCE_SCOPE_SEPARATOR}{uri}")),
            vec![PermissionResource {
                kind: PermissionResourceKind::Custom {
                    name: MCP_SERVER_RESOURCE.to_owned(),
                },
                value: server.clone(),
                access: Some(PermissionResourceAccess::Read),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            }],
            PermissionRisk::Low,
        );
        let session = self.session.clone();
        let run: CallRun = Box::new(move |child: ToolContext| {
            Box::pin(async move {
                let scope = CallScope::new(
                    session.elicitor_for(&child),
                    child.live_sink.clone(),
                    child.cancel.clone(),
                );
                match session.read_resource(&server, &uri, &scope).await {
                    Ok(contents) => {
                        let value = contents_value(&contents);
                        let output = McpOutput {
                            blocks: contents.into_iter().map(read_block).collect(),
                            kind: session.server_kind(&server),
                            interactions: scope.take_interactions(),
                            ..McpOutput::default()
                        };
                        Ok((output, value))
                    }
                    Err(error) => Err(CallFailure::new(error.failure(), error.to_string())
                        .with_interactions(scope.take_interactions())),
                }
            })
        });
        Ok(HostCallPlan {
            row,
            permission: Permission::ask(ToolKey::native(CODE_TOOL_NAME), intent, identity),
            read_only: true,
            run,
        })
    }

    /// A call to a server's tool, checked as a direct MCP call was: bound to the tool the index
    /// holds now, kept from a tool the agent's configuration disables, and asked about under the
    /// intent its recognised server shapes.
    fn call_tool(&self, namespace: &str, call: ScriptCall) -> Result<HostCallPlan, ScriptError> {
        let names = &self.names;
        let method = call.name.clone();
        let server = names.server(namespace).unwrap_or_default().to_owned();
        let Some(view) = self.view(&server) else {
            return Err(undefined_namespace(namespace));
        };
        if let Some(reason) = unavailable(&view.state) {
            return Err(ScriptError::new(
                ScriptErrorKind::Runtime,
                format!("MCP server `{namespace}` is not connected ({reason})"),
            ));
        }
        let Some(tool) = names
            .raw_tool(namespace, &method)
            .and_then(|raw| view.tools.iter().find(|tool| tool.name == raw))
        else {
            return Err(ScriptError::new(
                ScriptErrorKind::Attribute,
                no_tool_message(names, namespace, &method),
            ));
        };
        let input = Value::Object(tool_arguments(&method, call, &tool.input_schema)?);
        let qualified = format!("{server}{SEPARATOR}{}", tool.name);
        let runtime = |message: String| ScriptError::new(ScriptErrorKind::Runtime, message);
        let binding = self
            .session
            .bind_tool(&qualified)
            .map_err(|error| runtime(error.to_string()))?;
        if self.session.is_disabled(&qualified) {
            return Err(ScriptError::new(
                ScriptErrorKind::Permission,
                format!("`{qualified}` is disabled for this agent"),
            ));
        }
        let key = ToolKey::parse(&qualified).map_err(|error| runtime(error.to_string()))?;
        let intent = mcp_permission_intent(&key, &input, binding.call_facts(&input));
        let identity = Some((binding.subject().clone(), PermissionExecutorKind::Mcp));
        let read_only = binding.read_only();
        let row = CodeCall {
            server,
            tool: tool.name.clone(),
            called_as: format!("{namespace}{SEPARATOR}{method}"),
            title: tool.title.clone(),
            input: input.clone(),
            ..CodeCall::default()
        };
        let session = self.session.clone();
        let run: CallRun = Box::new(move |child: ToolContext| {
            Box::pin(async move {
                let scope = CallScope::new(
                    session.elicitor_for(&child),
                    child.live_sink.clone(),
                    child.cancel.clone(),
                );
                match binding.call(&input, &scope).await {
                    Ok(output) if output.is_error => {
                        let error = binding.reported_error(&output);
                        Err(CallFailure::new(error.failure(), error.to_string())
                            .with_output(output))
                    }
                    Ok(output) => {
                        let value = output_value(&output);
                        Ok((output, value))
                    }
                    Err(error) => Err(CallFailure::new(error.failure(), error.to_string())
                        .with_interactions(scope.take_interactions())),
                }
            })
        });
        Ok(HostCallPlan {
            row,
            permission: Permission::ask(key, intent, identity),
            read_only,
            run,
        })
    }
}

impl HostModule for McpModule {
    fn doc(&self) -> &'static ModuleDoc {
        &DOC
    }

    fn dynamic_help(&self, topic: &str) -> Option<String> {
        match topic.split_once('.') {
            Some((namespace, method)) => self.server_help(namespace, Some(method)),
            None => self.server_help(topic, None),
        }
    }

    fn index_lines(&self) -> Vec<String> {
        self.names
            .namespaces()
            .filter_map(|(namespace, server)| {
                let view = self.view(server)?;
                let state = unavailable(&view.state).unwrap_or_else(|| CONNECTED.to_owned());
                Some(format!(
                    "{namespace}: MCP server, {} tools ({state})",
                    view.tools.len()
                ))
            })
            .collect()
    }

    fn global(&self, name: &str) -> Option<ScriptName> {
        if name == MCP_MODULE || self.names.server(name).is_some() {
            return Some(ScriptName::Namespace);
        }
        self.names.suggest_namespace(name).map(|namespace| {
            ScriptName::Undefined(Some(format!(
                "name '{name}' is not defined. Did you mean the MCP server `{namespace}`?"
            )))
        })
    }

    fn member(&self, namespace: &str, name: &str) -> ScriptName {
        if namespace == MCP_MODULE {
            return match DOC.member(name) {
                Some(_) => ScriptName::Function,
                None => ScriptName::Undefined(Some(DOC.no_member(name))),
            };
        }
        let names = &self.names;
        let reachable = names
            .server(namespace)
            .and_then(|server| self.view(server))
            .is_some_and(|view| unavailable(&view.state).is_none());
        // A server that is not connected answers every name, so the call raises why.
        if !reachable || names.raw_tool(namespace, name).is_some() {
            return ScriptName::Function;
        }
        ScriptName::Undefined(Some(no_tool_message(names, namespace, name)))
    }

    fn stubs(&self) -> String {
        stubs(&self.names, &self.servers) + DOC.stubs().as_str()
    }

    fn prepare(&self, ctx: &ToolContext, call: ScriptCall) -> Prepared {
        match call.namespace.as_deref() {
            Some(MCP_MODULE) => {}
            Some(namespace) => {
                let namespace = namespace.to_owned();
                return Prepared::Plan(self.call_tool(&namespace, call).map(Box::new));
            }
            None => {
                return Prepared::Answer(ScriptInvocation::Ready(Err(undefined_namespace(
                    &call.name,
                ))));
            }
        }
        let answer = match call.name.as_str() {
            SEARCH_HELPER => ScriptInvocation::Ready(self.search(&call)),
            SERVERS_HELPER => ScriptInvocation::Ready(Ok(self.server_list())),
            RESOURCES_HELPER => {
                ScriptInvocation::Deferred(Box::pin(future::ready(self.resources(&call))))
            }
            READ_RESOURCE_HELPER => {
                return Prepared::Plan(self.read_resource(ctx, &call).map(Box::new));
            }
            name => ScriptInvocation::Ready(Err(ScriptError::new(
                ScriptErrorKind::Attribute,
                DOC.no_member(name),
            ))),
        };
        Prepared::Answer(answer)
    }

    /// Type checking rejects a call to a tool a server lacks before the host can answer for it,
    /// so the rejection gets the "did you mean" the call would have raised at runtime.
    fn rejection_hints(&self, diagnostic: &str) -> Vec<String> {
        diagnostic
            .split(CLASS_PREFIX)
            .skip(1)
            .filter_map(|rest| {
                let (namespace, rest) = rest.split_once(QUOTE)?;
                let (method, _) = rest.strip_prefix(NO_ATTRIBUTE)?.split_once(QUOTE)?;
                self.names.server(namespace)?;
                Some(no_tool_message(&self.names, namespace, method))
            })
            .collect()
    }
}

fn undefined_namespace(namespace: &str) -> ScriptError {
    ScriptError::new(
        ScriptErrorKind::Name,
        format!("name '{namespace}' is not defined"),
    )
}

fn no_tool_message(names: &Names, namespace: &str, method: &str) -> String {
    let suggestion = names
        .suggest_tool(namespace, method)
        .map(|tool| format!(" Did you mean `{tool}`?"))
        .unwrap_or_default();
    format!(
        "MCP server `{namespace}` has no tool `{method}`.{suggestion} Find tools with `mcp.search(\"...\", server=\"{namespace}\")`."
    )
}

/// A tool call's arguments: keywords, or one positional dict, checked against what the tool's
/// schema requires and allows.
fn tool_arguments(
    method: &str,
    call: ScriptCall,
    schema: &Value,
) -> Result<Map<String, Value>, ScriptError> {
    let type_error = |message: String| ScriptError::new(ScriptErrorKind::Type, message);
    let ScriptCall { args, kwargs, .. } = call;
    let arguments = match <[Value; 1]>::try_from(args) {
        Ok([Value::Object(arguments)]) if kwargs.is_empty() => arguments,
        Err(args) if args.is_empty() => kwargs,
        _ => {
            return Err(type_error(format!(
                "{method}() takes keyword arguments, or a single dict of them"
            )));
        }
    };
    let properties = schema.get(PROPERTIES).and_then(Value::as_object);
    let missing: Vec<&str> = schema
        .get(REQUIRED)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|name| !arguments.contains_key(*name))
        .collect();
    if !missing.is_empty() {
        return Err(type_error(format!(
            "{method}() missing required arguments: {}",
            quoted(missing.into_iter())
        )));
    }
    let open = schema
        .get(ADDITIONAL_PROPERTIES)
        .is_some_and(|allowed| allowed.as_bool() != Some(false));
    if let Some(properties) = properties.filter(|_| !open) {
        let unknown: Vec<&str> = arguments
            .keys()
            .map(String::as_str)
            .filter(|name| !properties.contains_key(*name))
            .collect();
        if !unknown.is_empty() {
            let known = properties.keys().map(String::as_str);
            return Err(type_error(format!(
                "{method}() got unexpected arguments: {}. It takes: {}",
                quoted(unknown.into_iter()),
                quoted(known)
            )));
        }
    }
    Ok(arguments)
}

fn quoted<'a>(names: impl Iterator<Item = &'a str>) -> String {
    names
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a snippet receives: `structuredContent`, else the content, a JSON text parsed.
fn output_value(output: &McpOutput) -> Value {
    if let Some(structured) = output.structured_content() {
        return structured.clone();
    }
    let mut values: Vec<Value> = output.blocks.iter().map(block_value).collect();
    match values.len() {
        0 => Value::Null,
        1 => values.remove(0),
        _ => Value::Array(values),
    }
}

fn block_value(block: &McpBlock) -> Value {
    match block {
        McpBlock::Text { text } | McpBlock::Excerpt { text } => json_text(text),
        McpBlock::Structured { value } => value.clone(),
        McpBlock::Image { mime_type, bytes } => media(IMAGE_TYPE, mime_type, *bytes),
        McpBlock::Audio { mime_type, bytes } => media(AUDIO_TYPE, mime_type, *bytes),
        McpBlock::Resource {
            uri,
            mime_type,
            text,
            bytes,
        } => json!({
            "type": RESOURCE_TYPE,
            "uri": uri,
            "mime_type": mime_type,
            "text": text,
            "bytes": bytes,
        }),
        McpBlock::ResourceLink {
            uri,
            name,
            mime_type,
        } => json!({
            "type": RESOURCE_LINK_TYPE,
            "uri": uri,
            "name": name,
            "mime_type": mime_type,
        }),
    }
}

/// Text that is a JSON object or array, parsed; anything else stays text.
fn json_text(text: &str) -> Value {
    let trimmed = text.trim();
    trimmed
        .starts_with(['{', '['])
        .then(|| serde_json::from_str(trimmed).ok())
        .flatten()
        .unwrap_or_else(|| Value::String(text.to_owned()))
}

/// Media the model never reads, named by kind and size.
fn media(kind: &str, mime_type: &Option<String>, bytes: usize) -> Value {
    json!({ "type": kind, "mime_type": mime_type, "bytes": bytes })
}

fn resource_entries(lists: &ResourceLists) -> Value {
    let resources = lists.resources.iter().map(|resource| {
        json!({
            "uri": resource.uri,
            "name": resource.label(),
            "description": resource.description,
            "mime_type": resource.mime_type,
            "template": false,
        })
    });
    let templates = lists.templates.iter().map(|template| {
        json!({
            "uri": template.uri_template,
            "name": template.label(),
            "description": template.description,
            "mime_type": template.mime_type,
            "template": true,
        })
    });
    resources.chain(templates).collect()
}

fn contents_value(contents: &[ResourceContents]) -> Value {
    contents
        .iter()
        .map(|content| match &content.blob {
            Some(blob) => json!({
                "uri": content.uri,
                "mime_type": content.mime_type,
                "bytes": decoded_len(blob),
            }),
            None => json!({
                "uri": content.uri,
                "mime_type": content.mime_type,
                "text": content.text.clone().unwrap_or_default(),
            }),
        })
        .collect()
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
