use std::collections::{BTreeMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

use maki_config::{FILE_WRITE_TOOLS, ToolKey};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

pub use maki_storage::permission_state::{
    PermissionArgumentConstraint, PermissionExecutorKind, PermissionLifetime,
    PermissionResourceAccess, PermissionResourceConstraint, PermissionResourceKind,
    PermissionResourceSelector, PermissionRuleRecord, PermissionSubject,
    SelectedPermissionArgument, StructuredPermissionEffect, StructuredPermissionRule,
};

const NATIVE_OWNER: &str = "maki";
const MCP_CONTRACT: &str = "mcp.tools.call/v1";
const SUMMARY_MAX_CHARS: usize = 240;
const REVIEW_MAX_DEPTH: usize = 6;
const REVIEW_MAX_ITEMS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionRisk {
    Low,
    Medium,
    High,
    Critical,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionResource {
    pub kind: PermissionResourceKind,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<PermissionResourceAccess>,
    #[serde(default)]
    pub protected: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRuleOption {
    pub id: String,
    pub label: String,
    pub description: String,
    pub rule: StructuredPermissionRule,
    #[serde(default)]
    pub broad: bool,
    #[serde(default)]
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionResourcePresentation {
    pub kind: PermissionResourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<PermissionResourceAccess>,
    pub summary: String,
    pub protected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionPresentation {
    pub action: String,
    pub risk: PermissionRisk,
    pub risk_summary: String,
    pub resources: Vec<PermissionResourcePresentation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub id: String,
    #[serde(deserialize_with = "deserialize_tool_key")]
    pub tool: ToolKey,
    pub scopes: Vec<String>,
    pub subject: PermissionSubject,
    pub executor: PermissionExecutorKind,
    pub risk: PermissionRisk,
    pub resources: Vec<PermissionResource>,
    pub input: Value,
    pub input_digest: String,
    pub lifetime: PermissionLifetime,
    pub options: Vec<PermissionRuleOption>,
    pub presentation: PermissionPresentation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructuredPermissionDecision {
    Allow,
    Deny,
    NoMatch,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SelectedInputError {
    #[error("JSON pointer must be empty or begin with '/': {0:?}")]
    InvalidPointer(String),
    #[error("JSON pointer contains an invalid '~' escape: {0:?}")]
    InvalidEscape(String),
    #[error("JSON pointer uses a non-canonical array index: {0:?}")]
    InvalidArrayIndex(String),
    #[error("JSON pointer does not exist in the input: {0:?}")]
    MissingPointer(String),
    #[error("JSON pointer is selected more than once: {0:?}")]
    DuplicatePointer(String),
}

impl PermissionRequest {
    pub fn from_legacy(
        id: String,
        tool: ToolKey,
        scopes: Vec<String>,
        input: Value,
        cwd: &Path,
        force_prompt: bool,
    ) -> Self {
        let (subject, executor) = subject_and_executor(&tool);
        Self::from_legacy_with_identity(
            id,
            tool,
            scopes,
            input,
            cwd,
            force_prompt,
            subject,
            executor,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_legacy_with_identity(
        id: String,
        tool: ToolKey,
        scopes: Vec<String>,
        input: Value,
        cwd: &Path,
        force_prompt: bool,
        subject: PermissionSubject,
        executor: PermissionExecutorKind,
    ) -> Self {
        let risk = risk_for(&tool, force_prompt);
        let resources = resources_for(&tool, &scopes, &input, cwd, force_prompt);
        let input_digest = canonical_json_sha256(&input);
        let options = rule_options(&tool, &subject, &executor, &resources, &input_digest);
        let presentation = presentation_for(&tool, &risk, &resources);
        Self {
            id,
            tool,
            scopes,
            subject,
            executor,
            risk,
            resources,
            input,
            input_digest,
            lifetime: PermissionLifetime::Once,
            options,
            presentation,
        }
    }
}

pub fn canonical_json(value: &Value) -> String {
    let mut output = String::new();
    write_canonical_json(value, &mut output);
    output
}

pub fn canonical_json_sha256(value: &Value) -> String {
    let digest = Sha256::digest(canonical_json(value).as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

pub fn redacted_review_shape(value: &Value) -> Value {
    fn redact(value: &Value, depth: usize) -> Value {
        if depth == REVIEW_MAX_DEPTH {
            return Value::String("<depth-limit>".into());
        }
        match value {
            Value::Null => Value::String("<null>".into()),
            Value::Bool(_) => Value::String("<boolean>".into()),
            Value::Number(_) => Value::String("<number>".into()),
            Value::String(value) => {
                Value::String(format!("<string:{} chars>", value.chars().count()))
            }
            Value::Array(values) => {
                let mut redacted: Vec<_> = values
                    .iter()
                    .take(REVIEW_MAX_ITEMS)
                    .map(|value| redact(value, depth + 1))
                    .collect();
                if values.len() > REVIEW_MAX_ITEMS {
                    redacted.push(Value::String(format!(
                        "<{} items omitted>",
                        values.len() - REVIEW_MAX_ITEMS
                    )));
                }
                Value::Array(redacted)
            }
            Value::Object(values) => {
                let mut redacted = serde_json::Map::new();
                for (index, value) in values.values().take(REVIEW_MAX_ITEMS).enumerate() {
                    redacted.insert(format!("<field:{}>", index + 1), redact(value, depth + 1));
                }
                if values.len() > REVIEW_MAX_ITEMS {
                    redacted.insert(
                        "<omitted>".into(),
                        Value::String(format!(
                            "<{} fields omitted>",
                            values.len() - REVIEW_MAX_ITEMS
                        )),
                    );
                }
                Value::Object(redacted)
            }
        }
    }

    redact(value, 0)
}

fn write_canonical_json(value: &Value, output: &mut String) {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => output.push_str(&value.to_string()),
        Value::String(value) => {
            output.push_str(&serde_json::to_string(value).expect("strings always serialize"));
        }
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical_json(value, output);
            }
            output.push(']');
        }
        Value::Object(values) => {
            output.push('{');
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_unstable_by_key(|(key, _)| *key);
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key).expect("object keys always serialize"));
                output.push(':');
                write_canonical_json(value, output);
            }
            output.push('}');
        }
    }
}

pub fn escape_json_pointer_segment(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

pub fn json_pointer<S: AsRef<str>>(segments: &[S]) -> String {
    let mut pointer = String::new();
    for segment in segments {
        pointer.push('/');
        pointer.push_str(&escape_json_pointer_segment(segment.as_ref()));
    }
    pointer
}

pub fn selected_input_pointer<'a>(
    input: &'a Value,
    pointer: &str,
) -> Result<&'a Value, SelectedInputError> {
    let segments = decode_json_pointer(pointer)?;
    let mut selected = input;
    for segment in segments {
        selected = match selected {
            Value::Object(object) => object
                .get(&segment)
                .ok_or_else(|| SelectedInputError::MissingPointer(pointer.to_owned()))?,
            Value::Array(array) => {
                if segment == "-"
                    || (segment.len() > 1 && segment.starts_with('0'))
                    || !segment.bytes().all(|byte| byte.is_ascii_digit())
                {
                    return Err(SelectedInputError::InvalidArrayIndex(pointer.to_owned()));
                }
                let index = segment
                    .parse::<usize>()
                    .map_err(|_| SelectedInputError::InvalidArrayIndex(pointer.to_owned()))?;
                array
                    .get(index)
                    .ok_or_else(|| SelectedInputError::MissingPointer(pointer.to_owned()))?
            }
            _ => return Err(SelectedInputError::MissingPointer(pointer.to_owned())),
        };
    }
    Ok(selected)
}

pub fn selected_input<S: AsRef<str>>(
    input: &Value,
    pointers: &[S],
) -> Result<Vec<SelectedPermissionArgument>, SelectedInputError> {
    let mut seen = HashSet::with_capacity(pointers.len());
    let mut selected = Vec::with_capacity(pointers.len());
    for pointer in pointers {
        let pointer = pointer.as_ref();
        if !seen.insert(pointer) {
            return Err(SelectedInputError::DuplicatePointer(pointer.to_owned()));
        }
        let value = selected_input_pointer(input, pointer)?.clone();
        selected.push(SelectedPermissionArgument {
            pointer: pointer.to_owned(),
            digest: canonical_json_sha256(&value),
            value,
        });
    }
    Ok(selected)
}

fn decode_json_pointer(pointer: &str) -> Result<Vec<String>, SelectedInputError> {
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    let Some(pointer) = pointer.strip_prefix('/') else {
        return Err(SelectedInputError::InvalidPointer(pointer.to_owned()));
    };
    pointer
        .split('/')
        .map(|segment| {
            let mut decoded = String::with_capacity(segment.len());
            let mut chars = segment.chars();
            while let Some(character) = chars.next() {
                if character != '~' {
                    decoded.push(character);
                    continue;
                }
                match chars.next() {
                    Some('0') => decoded.push('~'),
                    Some('1') => decoded.push('/'),
                    _ => return Err(SelectedInputError::InvalidEscape(pointer.to_owned())),
                }
            }
            Ok(decoded)
        })
        .collect()
}

pub fn argument_constraint_matches(
    constraint: &PermissionArgumentConstraint,
    input: &Value,
) -> bool {
    match constraint {
        PermissionArgumentConstraint::Exact { digest } => canonical_json_sha256(input) == *digest,
        PermissionArgumentConstraint::Selected { arguments } => {
            let mut seen = HashSet::with_capacity(arguments.len());
            arguments.iter().all(|argument| {
                seen.insert(argument.pointer.as_str())
                    && canonical_json_sha256(&argument.value) == argument.digest
                    && selected_input_pointer(input, &argument.pointer).is_ok_and(|selected| {
                        canonical_json_sha256(selected) == argument.digest
                            && canonical_json(selected) == canonical_json(&argument.value)
                    })
            })
        }
        PermissionArgumentConstraint::Unconstrained => true,
    }
}

pub fn resource_constraint_matches(
    constraint: &PermissionResourceConstraint,
    resource: &PermissionResource,
) -> bool {
    if constraint.kind != resource.kind {
        return false;
    }
    if constraint
        .access
        .as_ref()
        .is_some_and(|access| resource.access.as_ref() != Some(access))
    {
        return false;
    }
    if constraint
        .protected
        .is_some_and(|protected| resource.protected != protected)
    {
        return false;
    }
    if resource.protected
        && (constraint.protected != Some(true)
            || !matches!(
                constraint.selector,
                PermissionResourceSelector::Exact { .. }
                    | PermissionResourceSelector::Digest { .. }
            )
            || constraint.attributes.len() != resource.attributes.len()
            || constraint.attributes.values().any(|selector| {
                !matches!(
                    selector,
                    PermissionResourceSelector::Exact { .. }
                        | PermissionResourceSelector::Digest { .. }
                )
            }))
    {
        return false;
    }
    if !selector_matches(&constraint.selector, &resource.value, &resource.kind) {
        return false;
    }
    constraint.attributes.iter().all(|(name, selector)| {
        resource.attributes.get(name).is_some_and(|value| {
            let kind = if name == "workdir" {
                PermissionResourceKind::Directory
            } else {
                PermissionResourceKind::Custom { name: name.clone() }
            };
            selector_matches(selector, value, &kind)
        })
    })
}

pub fn permission_rule_covers_request(
    rule: &StructuredPermissionRule,
    request: &PermissionRequest,
) -> bool {
    rule.effect == StructuredPermissionEffect::Allow
        && rule_context_matches(rule, request)
        && request.resources.iter().all(|resource| {
            rule.resources
                .iter()
                .any(|constraint| resource_constraint_matches(constraint, resource))
        })
}

pub fn permission_rule_intersects_request(
    rule: &StructuredPermissionRule,
    request: &PermissionRequest,
) -> bool {
    rule.effect == StructuredPermissionEffect::Deny
        && rule_context_matches(rule, request)
        && (rule.resources.is_empty()
            || request.resources.iter().any(|resource| {
                rule.resources
                    .iter()
                    .any(|constraint| resource_constraint_matches(constraint, resource))
            }))
}

pub fn evaluate_structured_permission_rules(
    rules: &[StructuredPermissionRule],
    request: &PermissionRequest,
) -> StructuredPermissionDecision {
    if rules
        .iter()
        .any(|rule| permission_rule_intersects_request(rule, request))
    {
        return StructuredPermissionDecision::Deny;
    }
    if rules
        .iter()
        .any(|rule| permission_rule_covers_request(rule, request))
    {
        StructuredPermissionDecision::Allow
    } else {
        StructuredPermissionDecision::NoMatch
    }
}

fn rule_context_matches(rule: &StructuredPermissionRule, request: &PermissionRequest) -> bool {
    rule.subject == request.subject
        && rule.executor == request.executor
        && argument_constraint_matches(&rule.arguments, &request.input)
}

fn selector_matches(
    selector: &PermissionResourceSelector,
    value: &str,
    kind: &PermissionResourceKind,
) -> bool {
    match selector {
        PermissionResourceSelector::Any => true,
        PermissionResourceSelector::Digest { digest } => {
            resource_value_digest(value, kind).is_some_and(|actual| actual == *digest)
        }
        PermissionResourceSelector::Exact { value: expected } => match kind {
            PermissionResourceKind::File | PermissionResourceKind::Directory => {
                normalized_filesystem_path(expected)
                    .zip(normalized_filesystem_path(value))
                    .is_some_and(|(expected, actual)| expected == actual)
            }
            PermissionResourceKind::Url => strict_http_url(expected)
                .zip(strict_http_url(value))
                .is_some_and(|(expected, actual)| expected.key == actual.key),
            _ => expected == value,
        },
        PermissionResourceSelector::Subtree { root } => match kind {
            PermissionResourceKind::File | PermissionResourceKind::Directory => {
                normalized_filesystem_path(root)
                    .zip(normalized_filesystem_path(value))
                    .is_some_and(|(root, value)| value == root || value.starts_with(root))
            }
            PermissionResourceKind::Url => http_url_is_subtree(root, value),
            _ => false,
        },
    }
}

fn resource_value_digest(value: &str, kind: &PermissionResourceKind) -> Option<String> {
    let canonical = match kind {
        PermissionResourceKind::File | PermissionResourceKind::Directory => {
            normalized_filesystem_path(value)?
                .to_string_lossy()
                .into_owned()
        }
        PermissionResourceKind::Url => strict_http_url(value)?.key,
        _ => value.to_owned(),
    };
    Some(canonical_json_sha256(&Value::String(canonical)))
}

fn normalized_filesystem_path(path: &str) -> Option<PathBuf> {
    if path.is_empty() || path.contains('\0') {
        return None;
    }
    let path = Path::new(path);
    let absolute = std::path::absolute(path).ok()?;
    Some(
        maki_storage::paths::incremental_canonicalize(&absolute)
            .unwrap_or_else(|| maki_storage::paths::normalize_path(&absolute)),
    )
}

struct StrictHttpUrl {
    url: Url,
    key: String,
}

fn strict_http_url(value: &str) -> Option<StrictHttpUrl> {
    if value.chars().any(char::is_control) || value.contains('\\') {
        return None;
    }
    let authority_and_path = value.split_once("://")?.1;
    let authority = authority_and_path
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.contains('@') {
        return None;
    }
    validate_url_percent_encoding(value)?;
    let url = Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
        || url.fragment().is_some()
    {
        return None;
    }
    let key = normalize_percent_hex(url.as_str());
    Some(StrictHttpUrl { url, key })
}

fn validate_url_percent_encoding(value: &str) -> Option<()> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            index += 1;
            continue;
        }
        let high = *bytes.get(index + 1)?;
        let low = *bytes.get(index + 2)?;
        let decoded = (hex_value(high)? << 4) | hex_value(low)?;
        if decoded == b'/'
            || decoded == b'\\'
            || decoded == b'.'
            || decoded == b'%'
            || decoded <= 0x1f
            || decoded == 0x7f
            || decoded.is_ascii_alphanumeric()
            || matches!(decoded, b'-' | b'_' | b'~')
        {
            return None;
        }
        index += 3;
    }
    Some(())
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn normalize_percent_hex(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            output.push('%');
            output.push((bytes[index + 1] as char).to_ascii_uppercase());
            output.push((bytes[index + 2] as char).to_ascii_uppercase());
            index += 3;
        } else {
            output.push(bytes[index] as char);
            index += 1;
        }
    }
    output
}

fn http_url_is_subtree(root: &str, value: &str) -> bool {
    let Some(root) = strict_http_url(root) else {
        return false;
    };
    let Some(value) = strict_http_url(value) else {
        return false;
    };
    if root.url.query().is_some()
        || root.url.scheme() != value.url.scheme()
        || root.url.host_str() != value.url.host_str()
        || root.url.port_or_known_default() != value.url.port_or_known_default()
    {
        return false;
    }
    let root_path = normalize_percent_hex(root.url.path());
    let value_path = normalize_percent_hex(value.url.path());
    if root_path == "/" {
        return true;
    }
    let root_path = root_path.trim_end_matches('/');
    value_path == root_path
        || value_path
            .strip_prefix(root_path)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn subject_and_executor(tool: &ToolKey) -> (PermissionSubject, PermissionExecutorKind) {
    match tool {
        ToolKey::Native(name) => (
            PermissionSubject::Native {
                owner: NATIVE_OWNER.into(),
                contract: name.to_string(),
            },
            PermissionExecutorKind::Native,
        ),
        ToolKey::McpTool { server, tool } => (
            PermissionSubject::Mcp {
                server: server.to_string(),
                authority: server.to_string(),
                tool: tool.to_string(),
                contract: MCP_CONTRACT.into(),
            },
            PermissionExecutorKind::Mcp,
        ),
        ToolKey::McpServer { server } => (
            PermissionSubject::Mcp {
                server: server.to_string(),
                authority: server.to_string(),
                tool: "*".into(),
                contract: MCP_CONTRACT.into(),
            },
            PermissionExecutorKind::Mcp,
        ),
        ToolKey::Wildcard => (
            PermissionSubject::UnknownLegacy {
                identity: tool.to_string(),
            },
            PermissionExecutorKind::UnknownLegacy,
        ),
    }
}

fn risk_for(tool: &ToolKey, force_prompt: bool) -> PermissionRisk {
    if force_prompt {
        return PermissionRisk::Critical;
    }
    match tool {
        ToolKey::McpTool { .. } | ToolKey::McpServer { .. } => PermissionRisk::High,
        ToolKey::Native(name) if FILE_WRITE_TOOLS.contains(&name.as_ref()) => PermissionRisk::High,
        ToolKey::Native(name) if name.as_ref() == "bash" => PermissionRisk::High,
        ToolKey::Native(name) if name.as_ref() == "webfetch" => PermissionRisk::Medium,
        ToolKey::Native(name) if name.as_ref() == "websearch" => PermissionRisk::Low,
        ToolKey::Native(_) | ToolKey::Wildcard => PermissionRisk::Unknown,
    }
}

fn resources_for(
    tool: &ToolKey,
    scopes: &[String],
    input: &Value,
    cwd: &Path,
    force_prompt: bool,
) -> Vec<PermissionResource> {
    match tool {
        ToolKey::Native(name) if FILE_WRITE_TOOLS.contains(&name.as_ref()) => scopes
            .iter()
            .map(|scope| PermissionResource {
                kind: PermissionResourceKind::File,
                value: scope.clone(),
                access: Some(PermissionResourceAccess::Write),
                protected: protected_file(scope, cwd),
                attributes: BTreeMap::new(),
            })
            .collect(),
        ToolKey::Native(name) if name.as_ref() == "bash" => {
            let workdir = input
                .get("workdir")
                .and_then(Value::as_str)
                .map(String::from);
            scopes
                .iter()
                .map(|scope| PermissionResource {
                    kind: PermissionResourceKind::Command,
                    value: scope.clone(),
                    access: Some(PermissionResourceAccess::Execute),
                    protected: force_prompt,
                    attributes: workdir
                        .iter()
                        .map(|workdir| ("workdir".into(), workdir.clone()))
                        .collect(),
                })
                .collect()
        }
        ToolKey::Native(name) if name.as_ref() == "webfetch" => scopes
            .iter()
            .map(|scope| PermissionResource {
                kind: PermissionResourceKind::Url,
                value: scope.clone(),
                access: Some(PermissionResourceAccess::Read),
                protected: false,
                attributes: BTreeMap::new(),
            })
            .collect(),
        ToolKey::Native(name) if name.as_ref() == "websearch" => scopes
            .iter()
            .map(|scope| PermissionResource {
                kind: PermissionResourceKind::Query,
                value: scope.clone(),
                access: Some(PermissionResourceAccess::Search),
                protected: false,
                attributes: BTreeMap::new(),
            })
            .collect(),
        ToolKey::McpTool { .. } | ToolKey::McpServer { .. } => vec![PermissionResource {
            kind: PermissionResourceKind::Custom {
                name: "mcp_tool".into(),
            },
            value: tool.to_string(),
            access: Some(PermissionResourceAccess::Execute),
            protected: false,
            attributes: BTreeMap::new(),
        }],
        _ => scopes
            .iter()
            .map(|scope| PermissionResource {
                kind: PermissionResourceKind::Custom {
                    name: tool.to_string(),
                },
                value: scope.clone(),
                access: Some(PermissionResourceAccess::Execute),
                protected: force_prompt,
                attributes: BTreeMap::new(),
            })
            .collect(),
    }
}

fn protected_file(value: &str, cwd: &Path) -> bool {
    let Some(value) = normalized_filesystem_path(value) else {
        return true;
    };
    let outside_project = normalized_filesystem_path(&cwd.to_string_lossy())
        .is_none_or(|cwd| value != cwd && !value.starts_with(cwd));
    outside_project
        || value.components().any(|component| {
            let component = component.as_os_str().to_string_lossy();
            matches!(component.as_ref(), ".git" | ".ssh" | ".aws")
                || component == ".env"
                || component.starts_with(".env.")
        })
}

fn exact_resource_constraints(
    resources: &[PermissionResource],
) -> Vec<PermissionResourceConstraint> {
    resources
        .iter()
        .map(|resource| PermissionResourceConstraint {
            kind: resource.kind.clone(),
            selector: PermissionResourceSelector::Digest {
                digest: resource_value_digest(&resource.value, &resource.kind).unwrap_or_else(
                    || canonical_json_sha256(&Value::String(resource.value.clone())),
                ),
            },
            access: resource.access.clone(),
            protected: Some(resource.protected),
            attributes: resource
                .attributes
                .iter()
                .map(|(name, value)| {
                    let kind = if name == "workdir" {
                        PermissionResourceKind::Directory
                    } else {
                        PermissionResourceKind::Custom { name: name.clone() }
                    };
                    (
                        name.clone(),
                        PermissionResourceSelector::Digest {
                            digest: resource_value_digest(value, &kind).unwrap_or_else(|| {
                                canonical_json_sha256(&Value::String(value.clone()))
                            }),
                        },
                    )
                })
                .collect(),
        })
        .collect()
}

fn rule_options(
    tool: &ToolKey,
    subject: &PermissionSubject,
    executor: &PermissionExecutorKind,
    resources: &[PermissionResource],
    input_digest: &str,
) -> Vec<PermissionRuleOption> {
    let exact_arguments = PermissionArgumentConstraint::Exact {
        digest: input_digest.into(),
    };
    let resource_constraints = exact_resource_constraints(resources);
    let option = |id: &str,
                  label: &str,
                  description: &str,
                  effect: StructuredPermissionEffect,
                  lifetime: PermissionLifetime,
                  arguments: PermissionArgumentConstraint,
                  broad: bool,
                  is_default: bool| PermissionRuleOption {
        id: id.into(),
        label: label.into(),
        description: description.into(),
        rule: StructuredPermissionRule {
            subject: subject.clone(),
            executor: executor.clone(),
            resources: resource_constraints.clone(),
            arguments,
            lifetime,
            effect,
        },
        broad,
        is_default,
    };
    let mut options = vec![
        option(
            "allow_once",
            "Allow once",
            "Allow only this exact call once.",
            StructuredPermissionEffect::Allow,
            PermissionLifetime::Once,
            exact_arguments.clone(),
            false,
            true,
        ),
        option(
            "allow_conversation",
            "Allow for conversation",
            "Remember only this exact call for the conversation.",
            StructuredPermissionEffect::Allow,
            PermissionLifetime::Conversation,
            exact_arguments.clone(),
            false,
            false,
        ),
        option(
            "allow_project",
            "Allow for project",
            "Remember only this exact call for this project.",
            StructuredPermissionEffect::Allow,
            PermissionLifetime::Project,
            exact_arguments.clone(),
            false,
            false,
        ),
        option(
            "allow_global",
            "Allow globally",
            "Remember only this exact call globally.",
            StructuredPermissionEffect::Allow,
            PermissionLifetime::Global,
            exact_arguments.clone(),
            false,
            false,
        ),
        option(
            "deny_once",
            "Deny",
            "Deny this exact call.",
            StructuredPermissionEffect::Deny,
            PermissionLifetime::Once,
            exact_arguments,
            false,
            false,
        ),
        option(
            "deny_project",
            "Deny for project",
            "Deny only this exact call for this project.",
            StructuredPermissionEffect::Deny,
            PermissionLifetime::Project,
            PermissionArgumentConstraint::Exact {
                digest: input_digest.into(),
            },
            false,
            false,
        ),
        option(
            "deny_global",
            "Deny globally",
            "Deny only this exact call globally.",
            StructuredPermissionEffect::Deny,
            PermissionLifetime::Global,
            PermissionArgumentConstraint::Exact {
                digest: input_digest.into(),
            },
            false,
            false,
        ),
    ];
    if tool.is_mcp() {
        options.push(option(
            "allow_whole_mcp_tool_conversation",
            "Allow whole MCP tool for conversation (broad)",
            "Broad: allow this MCP tool with any arguments for the conversation.",
            StructuredPermissionEffect::Allow,
            PermissionLifetime::Conversation,
            PermissionArgumentConstraint::Unconstrained,
            true,
            false,
        ));
    }
    options
}

fn presentation_for(
    tool: &ToolKey,
    risk: &PermissionRisk,
    resources: &[PermissionResource],
) -> PermissionPresentation {
    let action = match tool {
        ToolKey::McpTool { server, tool } => {
            format!(
                "Call MCP tool {} from {}",
                safe_summary(tool),
                safe_summary(server)
            )
        }
        ToolKey::McpServer { server } => {
            format!("Call an MCP tool from {}", safe_summary(server))
        }
        ToolKey::Native(name) => format!("Run native tool {}", safe_summary(name)),
        ToolKey::Wildcard => "Run an unknown legacy tool".into(),
    };
    let risk_summary = match risk {
        PermissionRisk::Low => "Limited read or lookup operation",
        PermissionRisk::Medium => "External read or network operation",
        PermissionRisk::High => "May modify data, execute code, or invoke an external authority",
        PermissionRisk::Critical => "Complex or protected operation requiring exact review",
        PermissionRisk::Unknown => "Legacy operation with unknown effects",
    }
    .into();
    PermissionPresentation {
        action,
        risk: risk.clone(),
        risk_summary,
        resources: resources
            .iter()
            .map(|resource| {
                let mut summary = safe_summary(&resource.value);
                if let Some(workdir) = resource.attributes.get("workdir") {
                    summary.push_str(" in ");
                    summary.push_str(&safe_summary(workdir));
                }
                PermissionResourcePresentation {
                    kind: resource.kind.clone(),
                    access: resource.access.clone(),
                    summary,
                    protected: resource.protected,
                }
            })
            .collect(),
    }
}

fn safe_summary(value: &str) -> String {
    let mut output = String::new();
    let mut truncated = false;
    for (index, character) in value.chars().enumerate() {
        if index == SUMMARY_MAX_CHARS {
            truncated = true;
            break;
        }
        if character.is_control() {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    if truncated {
        output.push_str("...");
    }
    output
}

fn deserialize_tool_key<'de, D>(deserializer: D) -> Result<ToolKey, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    ToolKey::parse(&value).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    fn request(resources: Vec<PermissionResource>) -> PermissionRequest {
        PermissionRequest::from_legacy(
            "request".into(),
            ToolKey::native("legacy"),
            resources
                .iter()
                .map(|resource| resource.value.clone())
                .collect(),
            json!({"branch": "main", "nested": {"value": 1}}),
            Path::new("/tmp"),
            false,
        )
        .with_resources(resources)
    }

    trait RequestTestExt {
        fn with_resources(self, resources: Vec<PermissionResource>) -> Self;
    }

    impl RequestTestExt for PermissionRequest {
        fn with_resources(mut self, resources: Vec<PermissionResource>) -> Self {
            self.resources = resources;
            self
        }
    }

    fn custom_resource(value: &str) -> PermissionResource {
        PermissionResource {
            kind: PermissionResourceKind::Custom {
                name: "test".into(),
            },
            value: value.into(),
            access: Some(PermissionResourceAccess::Execute),
            protected: false,
            attributes: BTreeMap::new(),
        }
    }

    fn exact_constraint(resource: &PermissionResource) -> PermissionResourceConstraint {
        PermissionResourceConstraint {
            kind: resource.kind.clone(),
            selector: PermissionResourceSelector::Exact {
                value: resource.value.clone(),
            },
            access: resource.access.clone(),
            protected: Some(resource.protected),
            attributes: BTreeMap::new(),
        }
    }

    fn rule(
        request: &PermissionRequest,
        effect: StructuredPermissionEffect,
        resources: Vec<PermissionResourceConstraint>,
    ) -> StructuredPermissionRule {
        StructuredPermissionRule {
            subject: request.subject.clone(),
            executor: request.executor.clone(),
            resources,
            arguments: PermissionArgumentConstraint::Exact {
                digest: request.input_digest.clone(),
            },
            lifetime: request.lifetime.clone(),
            effect,
        }
    }

    #[test]
    fn canonical_digest_is_stable_across_object_key_order() {
        let left = json!({"z": 1, "a": {"d": 4, "b": 2}, "items": [3, 2, 1]});
        let right = json!({"items": [3, 2, 1], "a": {"b": 2, "d": 4}, "z": 1});
        assert_eq!(canonical_json(&left), canonical_json(&right));
        assert_eq!(canonical_json_sha256(&left), canonical_json_sha256(&right));
        assert_eq!(
            canonical_json(&left),
            r#"{"a":{"b":2,"d":4},"items":[3,2,1],"z":1}"#
        );
    }

    #[test]
    fn persisted_review_shape_never_contains_argument_values() {
        let input = json!({
            "command": "deploy --token secret",
            "force": true,
            "retries": 3,
            "targets": ["production"]
        });

        let review = redacted_review_shape(&input);
        assert_eq!(
            review,
            json!({
                "<field:1>": "<string:21 chars>",
                "<field:2>": "<boolean>",
                "<field:3>": "<number>",
                "<field:4>": ["<string:10 chars>"]
            })
        );
        let persisted = serde_json::to_string(&review).unwrap();
        assert!(!persisted.contains("secret"));
        assert!(!persisted.contains("production"));
    }

    #[test]
    fn selected_input_uses_safe_pointer_boundaries() {
        let input = json!({"a/b": {"~key": ["zero", "one"]}, "a": {"b": "other"}});
        let pointer = json_pointer(&["a/b", "~key", "1"]);
        assert_eq!(pointer, "/a~1b/~0key/1");
        assert_eq!(selected_input_pointer(&input, &pointer).unwrap(), "one");
        assert!(matches!(
            selected_input_pointer(&input, "/a~1b/~0key/01"),
            Err(SelectedInputError::InvalidArrayIndex(_))
        ));
        assert!(matches!(
            selected_input_pointer(&input, "/a~2b"),
            Err(SelectedInputError::InvalidEscape(_))
        ));
        assert!(matches!(
            selected_input(&input, &[pointer.as_str(), pointer.as_str()]),
            Err(SelectedInputError::DuplicatePointer(_))
        ));
    }

    #[test]
    fn selected_and_exact_argument_constraints_match_canonically() {
        let input = json!({"ignored": 1, "selected": {"b": 2, "a": 1}});
        let selected = selected_input(&input, &["/selected"]).unwrap();
        let selected_constraint = PermissionArgumentConstraint::Selected {
            arguments: selected,
        };
        assert!(argument_constraint_matches(
            &selected_constraint,
            &json!({"selected": {"a": 1, "b": 2}, "ignored": 99})
        ));
        assert!(!argument_constraint_matches(
            &selected_constraint,
            &json!({"selected": {"a": 1, "b": 3}, "ignored": 1})
        ));
        let exact = PermissionArgumentConstraint::Exact {
            digest: canonical_json_sha256(&input),
        };
        assert!(argument_constraint_matches(&exact, &input));
        assert!(!argument_constraint_matches(
            &exact,
            &json!({"ignored": 2, "selected": {"b": 2, "a": 1}})
        ));
    }

    #[test]
    fn filesystem_subtree_uses_component_boundaries() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        let constraint = PermissionResourceConstraint {
            kind: PermissionResourceKind::File,
            selector: PermissionResourceSelector::Subtree {
                root: root.to_string_lossy().into(),
            },
            access: Some(PermissionResourceAccess::Write),
            protected: Some(false),
            attributes: BTreeMap::new(),
        };
        let resource = |path: PathBuf| PermissionResource {
            kind: PermissionResourceKind::File,
            value: path.to_string_lossy().into(),
            access: Some(PermissionResourceAccess::Write),
            protected: false,
            attributes: BTreeMap::new(),
        };
        assert!(resource_constraint_matches(
            &constraint,
            &resource(root.join("src/lib.rs"))
        ));
        assert!(!resource_constraint_matches(
            &constraint,
            &resource(temp.path().join("project-copy/src/lib.rs"))
        ));
    }

    #[test]
    #[cfg(unix)]
    fn filesystem_subtree_resolves_symlinks() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("real");
        let link = temp.path().join("link");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&root, &link).unwrap();
        let constraint = PermissionResourceConstraint {
            kind: PermissionResourceKind::File,
            selector: PermissionResourceSelector::Subtree {
                root: root.to_string_lossy().into(),
            },
            access: None,
            protected: Some(false),
            attributes: BTreeMap::new(),
        };
        let resource = PermissionResource {
            kind: PermissionResourceKind::File,
            value: link.join("new.txt").to_string_lossy().into(),
            access: None,
            protected: false,
            attributes: BTreeMap::new(),
        };
        assert!(resource_constraint_matches(&constraint, &resource));
    }

    #[test]
    fn url_subtree_requires_strict_origin_and_path_boundary() {
        let constraint = PermissionResourceConstraint {
            kind: PermissionResourceKind::Url,
            selector: PermissionResourceSelector::Subtree {
                root: "https://example.com/api".into(),
            },
            access: Some(PermissionResourceAccess::Read),
            protected: Some(false),
            attributes: BTreeMap::new(),
        };
        let resource = |value: &str| PermissionResource {
            kind: PermissionResourceKind::Url,
            value: value.into(),
            access: Some(PermissionResourceAccess::Read),
            protected: false,
            attributes: BTreeMap::new(),
        };
        assert!(resource_constraint_matches(
            &constraint,
            &resource("https://example.com/api/v1?q=ok")
        ));
        assert!(!resource_constraint_matches(
            &constraint,
            &resource("https://example.com/apiv1")
        ));
        assert!(!resource_constraint_matches(
            &constraint,
            &resource("https://user@example.com/api/v1")
        ));
        assert!(!resource_constraint_matches(
            &constraint,
            &resource("https://example.com/api/%2e%2e/admin")
        ));
        assert!(!resource_constraint_matches(
            &constraint,
            &resource("http://example.com/api/v1")
        ));
    }

    #[test]
    fn one_allow_rule_must_cover_the_complete_request() {
        let first = custom_resource("first");
        let second = custom_resource("second");
        let request = request(vec![first.clone(), second.clone()]);
        let partial_rules = vec![
            rule(
                &request,
                StructuredPermissionEffect::Allow,
                vec![exact_constraint(&first)],
            ),
            rule(
                &request,
                StructuredPermissionEffect::Allow,
                vec![exact_constraint(&second)],
            ),
        ];
        assert_eq!(
            evaluate_structured_permission_rules(&partial_rules, &request),
            StructuredPermissionDecision::NoMatch
        );
        let complete = rule(
            &request,
            StructuredPermissionEffect::Allow,
            vec![exact_constraint(&first), exact_constraint(&second)],
        );
        assert_eq!(
            evaluate_structured_permission_rules(&[complete], &request),
            StructuredPermissionDecision::Allow
        );
    }

    #[test]
    fn deny_intersection_blocks_if_any_resource_matches() {
        let first = custom_resource("first");
        let second = custom_resource("second");
        let request = request(vec![first, second.clone()]);
        let allow = rule(
            &request,
            StructuredPermissionEffect::Allow,
            request.resources.iter().map(exact_constraint).collect(),
        );
        let deny = rule(
            &request,
            StructuredPermissionEffect::Deny,
            vec![exact_constraint(&second)],
        );
        assert_eq!(
            evaluate_structured_permission_rules(&[allow, deny], &request),
            StructuredPermissionDecision::Deny
        );
    }

    #[test]
    fn context_matching_is_strict_for_subject_and_executor_not_lifetime() {
        let resource = custom_resource("resource");
        let request = request(vec![resource.clone()]);
        let base = rule(
            &request,
            StructuredPermissionEffect::Allow,
            vec![exact_constraint(&resource)],
        );
        let mut wrong_subject = base.clone();
        wrong_subject.subject = PermissionSubject::UnknownLegacy {
            identity: "other".into(),
        };
        let mut wrong_executor = base.clone();
        wrong_executor.executor = PermissionExecutorKind::Mcp;
        let mut wrong_lifetime = base;
        wrong_lifetime.lifetime = PermissionLifetime::Conversation;
        assert!(!permission_rule_covers_request(&wrong_subject, &request));
        assert!(!permission_rule_covers_request(&wrong_executor, &request));
        assert!(permission_rule_covers_request(&wrong_lifetime, &request));
    }

    #[test]
    fn protected_resources_require_an_explicit_exact_selector() {
        let mut resource = custom_resource("secret");
        resource.protected = true;
        let broad = PermissionResourceConstraint {
            kind: resource.kind.clone(),
            selector: PermissionResourceSelector::Any,
            access: resource.access.clone(),
            protected: Some(true),
            attributes: BTreeMap::new(),
        };
        assert!(!resource_constraint_matches(&broad, &resource));
        assert!(resource_constraint_matches(
            &exact_constraint(&resource),
            &resource
        ));
    }

    #[test]
    fn mcp_options_keep_broad_choice_non_default() {
        let request = PermissionRequest::from_legacy(
            "mcp".into(),
            ToolKey::McpTool {
                server: Arc::from("server"),
                tool: Arc::from("lookup"),
            },
            vec![canonical_json(&json!({"query": "value"}))],
            json!({"query": "value"}),
            Path::new("/tmp"),
            false,
        );
        let broad = request.options.iter().find(|option| option.broad).unwrap();
        assert!(!broad.is_default);
        assert!(matches!(
            broad.rule.arguments,
            PermissionArgumentConstraint::Unconstrained
        ));
        assert!(
            request
                .options
                .iter()
                .filter(|option| option.rule.effect == StructuredPermissionEffect::Allow)
                .filter(|option| !option.broad)
                .all(|option| matches!(
                    option.rule.arguments,
                    PermissionArgumentConstraint::Exact { .. }
                ))
        );
        for lifetime in [
            PermissionLifetime::Once,
            PermissionLifetime::Conversation,
            PermissionLifetime::Project,
            PermissionLifetime::Global,
        ] {
            assert!(request.options.iter().any(|option| {
                option.rule.effect == StructuredPermissionEffect::Allow
                    && !option.broad
                    && option.rule.lifetime == lifetime
            }));
        }
        let serialized = serde_json::to_string(&request).unwrap();
        assert_eq!(
            serde_json::from_str::<PermissionRequest>(&serialized).unwrap(),
            request
        );
    }
}
