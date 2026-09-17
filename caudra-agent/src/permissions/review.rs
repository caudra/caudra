use super::{
    COMMAND_OBSERVATION_BINDING_ATTRIBUTE, POSSIBLE_WORKDIRS_ATTRIBUTE, resources::PreparedWorkdirs,
};
use caudra_storage::permission_patterns::{
    ArgumentDomain, PatternDefinition, PatternToken, SlotCombinations,
};
use caudra_storage::permission_state::{
    BROWSE_RECURSION_ATTRIBUTE, PermissionReview, PermissionReviewResource, PermissionReviewSource,
    REVIEW_MAX_ATTRIBUTES, REVIEW_MAX_INPUT_BYTES, REVIEW_MAX_INPUT_DEPTH, REVIEW_MAX_INPUT_NODES,
    REVIEW_MAX_JSON_BYTES, REVIEW_MAX_RESOURCES, REVIEW_MAX_STRING_BYTES,
    filesystem_browse_recursion,
};
use serde_json::{Map, Value, json};
use std::path::{Component, Path, PathBuf};
use url::Url;

use crate::permissions::editor::SelectorValue;

use super::arguments::decode_json_pointer;
use super::matching::{
    filesystem_subtree_digest, normalized_filesystem_path, resource_value_digest, url_origin_digest,
};
use super::resources::remote_resource_identity;
use super::{
    COMMAND_OBSERVATION_ATTRIBUTE, CONFINED_READ_ATTRIBUTE, NORMALIZED_COMMAND_ATTRIBUTE,
    PermissionArgumentConstraint, PermissionCapabilityFamily, PermissionRequest,
    PermissionResourceKind, PermissionResourceSelector, PermissionSubject,
    StructuredPermissionRule, argument_constraint_matches, attribute_kind, canonical_json_sha256,
    selected_input_pointer, strict_http_url, url_subtree_digest, url_subtree_roots,
};

pub const COMMAND_TEMPLATE_EXECUTION_NOTICE: &str =
    "Execution grant; callee behavior is unknown, not certified read-only or path-confined";
const REDACTED: &str = "[redacted]";
const OMITTED: &str = "[omitted: review limit]";
const UNKNOWN_INPUT: &str = "[omitted: unrecognized tool input]";
const UNKNOWN_FIELD: &str = "[omitted: unrecognized field]";
const BULK_CONTENT: &str = "[omitted: content payload]";
const MISSING_SCOPE: &str = "[scope unavailable: no verified preimage]";
const INCOMPLETE: &str = "; some scope descriptions unavailable or omitted";
const MISSING_INPUT: &str = "; input scope unavailable: no verified input";
const INCOMPLETE_COMMAND: &str =
    "[omitted: ambiguous credential expansion; hidden text may execute actions]";
const FILESYSTEM_SUBTREE_DOMAIN: &str = "filesystem_subtree";
const URL_ORIGIN_DOMAIN: &str = "url_origin";
const MAX_CANDIDATE_PREIMAGE_BYTES: usize = 32 * 1024;
const MAX_CANDIDATE_PREIMAGE_DEPTH: usize = 64;
const SELECTED_ENTRY_NODES: usize = 3;
const ATTACHED_VALUE_FLAGS: &[&str] = &["-u", "-H", "-d"];
const KNOWN_RESOURCE_ATTRIBUTES: &[&str] = &[
    POSSIBLE_WORKDIRS_ATTRIBUTE,
    BROWSE_RECURSION_ATTRIBUTE,
    NORMALIZED_COMMAND_ATTRIBUTE,
    CONFINED_READ_ATTRIBUTE,
    "display_path",
];
const CREDENTIAL_FIELDS: &[&str] = &[
    "headers",
    "header",
    "authorization",
    "auth",
    "token",
    "password",
    "passwd",
    "secret",
    "credentials",
    "api_key",
    "apiKey",
    "access_token",
    "refresh_token",
    "cookie",
    "cookies",
    "private_key",
    "signature",
    "key",
    "sig",
    "access_key",
    "session_id",
];
const KNOWN_TOOLS: &[&str] = &[
    "bash",
    "shell",
    "file_read",
    "file_write",
    "file_edit",
    "file_apply_patch",
    "file_glob",
    "file_grep",
    "file_index",
    "index",
    "webfetch",
    "websearch",
    "python_execution",
    "code_map",
    "code_context",
    "code_refs",
    "code_impact",
    "code_expand",
    "tool_output",
    "tool_output_read",
    "tool_output_grep",
    "view_image",
    "execution_environment",
];
const KNOWN_FIELDS: &[&str] = &[
    "filePath",
    "file_path",
    "path",
    "paths",
    "root",
    "directory",
    "workdir",
    "cwd",
    "pattern",
    "glob",
    "include",
    "exclude",
    "query",
    "url",
    "command",
    "timeout",
    "timeoutSec",
    "timeout_ms",
    "timeout_sec",
    "offset",
    "limit",
    "max_lines",
    "max_results",
    "format",
    "pdfMode",
    "pdf_mode",
    "symbol",
    "direction",
    "depth",
    "task",
    "output_id",
    "byte_offset",
    "context_before",
    "context_after",
    "recursive",
    "case_sensitive",
    "replace_all",
    "encoding",
    "line",
    "start_line",
    "end_line",
];
const BULK_FIELDS: &[&str] = &[
    "code",
    "content",
    "contents",
    "patch",
    "patchText",
    "patch_text",
    "prompt",
    "data",
    "old_string",
    "new_string",
    "oldText",
    "newText",
    "old_text",
    "new_text",
    "edits",
];

pub(in crate::permissions) fn editor_attribute_kind(name: &str) -> PermissionResourceKind {
    attribute_kind(name)
}

pub(in crate::permissions) fn normalize_editor_selector_value(
    kind: &PermissionResourceKind,
    value: &SelectorValue,
) -> Result<SelectorValue, String> {
    let invalid = || "Selector is incompatible with this resource kind or value".to_owned();
    Ok(match value {
        SelectorValue::Exact(value)
            if matches!(
                kind,
                PermissionResourceKind::File | PermissionResourceKind::Directory
            ) =>
        {
            SelectorValue::Exact(
                normalized_filesystem_path(value)
                    .ok_or_else(invalid)?
                    .to_string_lossy()
                    .into_owned(),
            )
        }
        SelectorValue::Exact(value) if *kind == PermissionResourceKind::Url => {
            SelectorValue::Exact(strict_http_url(value).ok_or_else(invalid)?.key)
        }
        SelectorValue::FilesystemSubtree(root) => SelectorValue::FilesystemSubtree(
            normalized_filesystem_path(root)
                .ok_or_else(invalid)?
                .to_string_lossy()
                .into_owned(),
        ),
        SelectorValue::UrlOrigin(origin) => SelectorValue::UrlOrigin(
            strict_http_url(origin)
                .ok_or_else(invalid)?
                .url
                .origin()
                .ascii_serialization(),
        ),
        SelectorValue::UrlSubtree(root) => SelectorValue::UrlSubtree(
            url_subtree_roots(&strict_http_url(root).ok_or_else(invalid)?)
                .and_then(|roots| roots.into_iter().next())
                .ok_or_else(invalid)?,
        ),
        _ => value.clone(),
    })
}

pub(in crate::permissions) fn compile_editor_selector(
    kind: &PermissionResourceKind,
    value: &SelectorValue,
) -> Result<PermissionResourceSelector, String> {
    let invalid = || "Selector is incompatible with this resource kind or value".to_owned();
    Ok(match value {
        SelectorValue::Exact(value) => PermissionResourceSelector::Digest {
            digest: resource_value_digest(value, kind).ok_or_else(invalid)?,
        },
        SelectorValue::FilesystemSubtree(root)
            if matches!(
                kind,
                PermissionResourceKind::File | PermissionResourceKind::Directory
            ) =>
        {
            PermissionResourceSelector::FilesystemSubtreeDigest {
                digest: filesystem_subtree_digest(root).ok_or_else(invalid)?,
            }
        }
        SelectorValue::UrlOrigin(origin) if *kind == PermissionResourceKind::Url => {
            PermissionResourceSelector::UrlOriginDigest {
                digest: url_origin_digest(origin).ok_or_else(invalid)?,
            }
        }
        SelectorValue::UrlSubtree(root) if *kind == PermissionResourceKind::Url => {
            let strict = strict_http_url(root).ok_or_else(invalid)?;
            let roots = url_subtree_roots(&strict).ok_or_else(invalid)?;
            let root = roots.first().ok_or_else(invalid)?;
            PermissionResourceSelector::UrlSubtreeDigest {
                digest: url_subtree_digest(root),
            }
        }
        SelectorValue::CommandPattern(pattern) if *kind == PermissionResourceKind::Command => {
            PermissionResourceSelector::CommandPattern {
                pattern: pattern.clone(),
            }
        }
        SelectorValue::CommandTemplate { definition, .. }
            if *kind == PermissionResourceKind::Command =>
        {
            PermissionResourceSelector::CommandTemplate {
                definition: definition.clone(),
            }
        }
        SelectorValue::RemoteExact(scope) => PermissionResourceSelector::RemoteResource {
            identity: remote_resource_identity(kind).ok_or_else(invalid)?.clone(),
            scope: scope.clone(),
        },
        SelectorValue::RemoteSubtree(scope) => PermissionResourceSelector::RemoteSubtree {
            identity: remote_resource_identity(kind).ok_or_else(invalid)?.clone(),
            scope: scope.clone(),
        },
        SelectorValue::Any => PermissionResourceSelector::Any,
        _ => return Err(invalid()),
    })
}

pub fn review_for_rule(
    request: &PermissionRequest,
    rule: &StructuredPermissionRule,
) -> PermissionReview {
    let candidates = request
        .resources
        .iter()
        .flat_map(|resource| {
            [resource.value.clone()].into_iter().chain(
                resource
                    .attributes
                    .iter()
                    .filter(|(name, _)| {
                        !matches!(
                            name.as_str(),
                            COMMAND_OBSERVATION_ATTRIBUTE | COMMAND_OBSERVATION_BINDING_ATTRIBUTE
                        )
                    })
                    .map(|(_, value)| value.clone()),
            )
        })
        .collect::<Vec<_>>();
    review_from_candidates(
        rule,
        &request.tool.to_string(),
        Some(&request.input),
        &candidates,
        PermissionReviewSource::Approved,
    )
}

pub fn review_from_candidates(
    rule: &StructuredPermissionRule,
    tool: &str,
    input: Option<&Value>,
    candidates: &[String],
    source: PermissionReviewSource,
) -> PermissionReview {
    let available = source != PermissionReviewSource::Unavailable;
    let candidates = if available { candidates } else { &[] };
    let mut review = PermissionReview {
        tool: display_text(tool)
            .filter(|tool| !tool.is_empty())
            .unwrap_or_else(|| OMITTED.into()),
        authority: authority(rule),
        input: if available {
            reviewed_input(rule, tool, input)
        } else {
            None
        },
        resources: rule
            .resources
            .iter()
            .take(REVIEW_MAX_RESOURCES)
            .enumerate()
            .map(|(index, resource)| PermissionReviewResource {
                index,
                value: selector_label(&resource.selector, &resource.kind, candidates),
                attributes: resource
                    .attributes
                    .iter()
                    .take(REVIEW_MAX_ATTRIBUTES)
                    .filter_map(|(key, selector)| {
                        if !known_field(key) && !KNOWN_RESOURCE_ATTRIBUTES.contains(&key.as_str()) {
                            return None;
                        }
                        let name = display_text(key)?;
                        let label = if key == POSSIBLE_WORKDIRS_ATTRIBUTE {
                            possible_workdirs_label(selector, candidates)
                        } else {
                            selector_label(selector, &attribute_kind(key), candidates)
                        };
                        let value = label
                            .map(|value| {
                                if secret_key(key) {
                                    REDACTED.into()
                                } else {
                                    value
                                }
                            })
                            .unwrap_or_else(|| MISSING_SCOPE.into());
                        Some((name, value))
                    })
                    .collect(),
            })
            .collect(),
        source,
    };
    if review.input.is_none()
        && !matches!(rule.arguments, PermissionArgumentConstraint::Unconstrained)
    {
        review.authority.push_str(MISSING_INPUT);
    }
    let incomplete = review.resources.len() != rule.resources.len()
        || review.resources.iter().any(|resource| {
            resource.value.is_none()
                || resource.attributes.len() != rule.resources[resource.index].attributes.len()
                || resource
                    .attributes
                    .values()
                    .any(|value| value == MISSING_SCOPE)
        });
    if incomplete {
        review.authority.push_str(INCOMPLETE);
    }
    if serde_json::to_vec(&review).map_or(true, |bytes| bytes.len() > REVIEW_MAX_JSON_BYTES) {
        if review.input.is_some() {
            review.input = Some(Value::String(OMITTED.into()));
        }
        for resource in &mut review.resources {
            resource.value = None;
            resource.attributes.clear();
        }
        if !incomplete {
            review.authority.push_str(INCOMPLETE);
        }
    }
    review
}

pub fn visit_review_candidate_preimages(
    value: &str,
    url_depth: Option<usize>,
    mut visit: impl FnMut(&str, &str),
) -> bool {
    if value.len() > MAX_CANDIDATE_PREIMAGE_BYTES {
        return true;
    }
    visit(&canonical_json_sha256(&json!(value)), value);
    if absolute_preimage(value).is_some() {
        visit(
            &canonical_json_sha256(&json!([FILESYSTEM_SUBTREE_DOMAIN, value])),
            value,
        );
    }
    let Some(depth) = url_depth.filter(|_| value.contains("://")) else {
        return false;
    };
    let Some(url) = strict_http_url(value) else {
        return false;
    };
    if url.key != value {
        visit(&canonical_json_sha256(&json!(&url.key)), &url.key);
    }
    let origin = url.url.origin().ascii_serialization();
    visit(
        &canonical_json_sha256(&json!([URL_ORIGIN_DOMAIN, &origin])),
        &origin,
    );
    if !url_within_depth(&url.url, depth.min(MAX_CANDIDATE_PREIMAGE_DEPTH)) {
        return true;
    }
    for root in url_subtree_roots(&url).into_iter().flatten() {
        visit(&url_subtree_digest(&root), &root);
    }
    false
}

fn url_within_depth(url: &Url, depth: usize) -> bool {
    url.path_segments().is_some_and(|segments| {
        segments
            .filter(|segment| !segment.is_empty())
            .take(depth + 1)
            .count()
            <= depth
    })
}

fn authority(rule: &StructuredPermissionRule) -> String {
    let family = match rule.family {
        Some(PermissionCapabilityFamily::FilesystemBrowse) => "Filesystem names-only browse family",
        Some(PermissionCapabilityFamily::FilesystemRead) => "Filesystem read family",
        Some(PermissionCapabilityFamily::McpServer) => {
            "MCP server family (all tools on the bound server)"
        }
        None => "Bound tool",
    };
    let input = match rule.arguments {
        PermissionArgumentConstraint::Exact { .. } => {
            "exact input (sanitized display; omitted fields remain constrained)"
        }
        PermissionArgumentConstraint::Selected { .. }
        | PermissionArgumentConstraint::SelectedDigest { .. } => {
            "selected input fields only (other input unconstrained)"
        }
        PermissionArgumentConstraint::Unconstrained => "input unconstrained",
    };
    if rule.resources.is_empty() {
        format!("{family}; {input}; resources unconstrained")
    } else {
        format!(
            "{family}; {input}; {} resource constraints (scope labels below)",
            rule.resources.len()
        )
    }
}

fn selector_label(
    selector: &PermissionResourceSelector,
    kind: &PermissionResourceKind,
    candidates: &[String],
) -> Option<String> {
    if matches!(kind, PermissionResourceKind::Custom { name } if name == BROWSE_RECURSION_ATTRIBUTE)
    {
        return filesystem_browse_recursion(selector).map(str::to_owned);
    }
    let (scope, value) = match selector {
        PermissionResourceSelector::CommandTemplate { definition } => {
            return Some(command_template_label(definition));
        }
        PermissionResourceSelector::Any => return Some("Any resource".into()),
        PermissionResourceSelector::Exact { value } => ("Exact", value.clone()),
        PermissionResourceSelector::Subtree { root } => ("Subtree", root.clone()),
        PermissionResourceSelector::Prefix { value } => ("Prefix", value.clone()),
        PermissionResourceSelector::CommandPattern { pattern } => {
            ("Command pattern", pattern.clone())
        }
        PermissionResourceSelector::RemoteResource { scope, .. } => {
            ("Remote exact scope", serde_json::to_string(scope).ok()?)
        }
        PermissionResourceSelector::RemoteSubtree { scope, .. } => {
            ("Remote subtree", serde_json::to_string(scope).ok()?)
        }
        PermissionResourceSelector::Digest { digest } => {
            let value = candidates
                .iter()
                .filter_map(|candidate| match kind {
                    PermissionResourceKind::File | PermissionResourceKind::Directory => {
                        absolute_preimage(candidate).map(|_| candidate.clone())
                    }
                    PermissionResourceKind::Url => strict_http_url(candidate).map(|url| url.key),
                    _ => Some(candidate.clone()),
                })
                .find(|value| canonical_json_sha256(&json!(value)) == *digest)?;
            ("Exact", value)
        }
        PermissionResourceSelector::FilesystemSubtreeDigest { digest } => {
            let root = candidates
                .iter()
                .filter_map(|candidate| absolute_preimage(candidate))
                .find_map(|path| {
                    path.ancestors()
                        .take(MAX_CANDIDATE_PREIMAGE_DEPTH)
                        .filter_map(|root| root.to_str())
                        .find(|root| {
                            canonical_json_sha256(&json!([FILESYSTEM_SUBTREE_DOMAIN, root]))
                                == *digest
                        })
                        .map(str::to_owned)
                })?;
            ("Filesystem subtree", root)
        }
        PermissionResourceSelector::UrlSubtreeDigest { digest } => {
            let root = candidates
                .iter()
                .filter_map(|value| strict_http_url(value))
                .filter(|url| url_within_depth(&url.url, MAX_CANDIDATE_PREIMAGE_DEPTH))
                .filter_map(|url| url_subtree_roots(&url))
                .flatten()
                .find(|root| url_subtree_digest(root) == *digest)?;
            ("URL subtree", root)
        }
        PermissionResourceSelector::UrlOriginDigest { digest } => {
            let origin = candidates
                .iter()
                .filter_map(|value| strict_http_url(value))
                .map(|url| url.url.origin().ascii_serialization())
                .find(|origin| {
                    canonical_json_sha256(&json!([URL_ORIGIN_DOMAIN, origin])) == *digest
                })?;
            ("URL origin", origin)
        }
    };
    let value = display_text(&value)?;
    let label = format!("{scope}: {value}");
    (label.len() <= REVIEW_MAX_STRING_BYTES).then_some(label)
}

fn possible_workdirs_label(
    selector: &PermissionResourceSelector,
    candidates: &[String],
) -> Option<String> {
    let value = candidates.iter().find(|candidate| match selector {
        PermissionResourceSelector::Digest { digest } => {
            canonical_json_sha256(&json!(candidate)) == *digest
        }
        PermissionResourceSelector::Exact { value } => value == *candidate,
        _ => false,
    })?;
    let label = match PreparedWorkdirs::parse(value)? {
        PreparedWorkdirs::Known(paths) => format!(
            "Possible working directories: {}",
            paths
                .iter()
                .map(|path| display_text(path).map(|path| format!("`{path}`")))
                .collect::<Option<Vec<_>>>()?
                .join(", ")
        ),
        PreparedWorkdirs::Unknown => {
            "Working directory is unknown; only the exact prepared call is covered".into()
        }
    };
    (label.len() <= REVIEW_MAX_STRING_BYTES).then_some(label)
}

pub fn command_template_label(definition: &PatternDefinition) -> String {
    let literal = |value: &str| {
        serde_json::to_string(&display_text(value).unwrap_or_else(|| OMITTED.into()))
            .unwrap_or_else(|_| OMITTED.into())
    };
    let mut label = format!("Command template ({COMMAND_TEMPLATE_EXECUTION_NOTICE}): ");
    for token in &definition.argv {
        let word = match token {
            PatternToken::Exact { value, .. } => literal(value),
            PatternToken::Slot { id, .. } => {
                let name = definition
                    .slots
                    .iter()
                    .find(|slot| slot.id == *id)
                    .and_then(|slot| display_text(&slot.label))
                    .unwrap_or_else(|| OMITTED.into());
                format!("<{name}>")
            }
        };
        if label.len() + word.len() + 1 > REVIEW_MAX_STRING_BYTES {
            return OMITTED.into();
        }
        label.push_str(&word);
        label.push(' ');
    }
    for slot in &definition.slots {
        let domain = match &slot.domain {
            ArgumentDomain::ObservedSet { values } => format!(
                "values [{}]",
                values
                    .iter()
                    .map(|value| literal(value))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ArgumentDomain::Exact { value } => format!("exact {}", literal(value)),
            ArgumentDomain::Glob { pattern } => format!("glob {}", literal(pattern)),
            ArgumentDomain::Regex { pattern } => format!("regex {}", literal(pattern)),
            ArgumentDomain::AnyLiteralArgument => "any one literal argument".into(),
        };
        let detail = format!(
            "; {}: {domain}",
            display_text(&slot.label).unwrap_or_else(|| OMITTED.into())
        );
        if label.len() + detail.len() > REVIEW_MAX_STRING_BYTES {
            return OMITTED.into();
        }
        label.push_str(&detail);
    }
    let combinations = match &definition.combinations {
        SlotCombinations::ObservedTuples { tuples } => {
            format!("; only {} observed combinations", tuples.len())
        }
        SlotCombinations::Independent => "; independent slot combinations".into(),
    };
    if label.len() + combinations.len() > REVIEW_MAX_STRING_BYTES {
        return OMITTED.into();
    }
    label.push_str(&combinations);
    label
}

fn absolute_preimage(value: &str) -> Option<&Path> {
    let path = Path::new(value);
    (path.is_absolute()
        && !value.contains('\0')
        && !path
            .components()
            .any(|component| component == Component::ParentDir)
        && path.components().collect::<PathBuf>().to_str() == Some(value))
    .then_some(path)
}

fn reviewed_input(
    rule: &StructuredPermissionRule,
    tool: &str,
    input: Option<&Value>,
) -> Option<Value> {
    if matches!(rule.arguments, PermissionArgumentConstraint::Unconstrained) {
        return None;
    }
    let input = input.filter(|input| argument_constraint_matches(&rule.arguments, input))?;
    if !matches!(&rule.subject, PermissionSubject::Native { owner, .. } if owner == "caudra" || owner == "workcell")
        || !KNOWN_TOOLS.contains(&tool)
    {
        return Some(Value::String(UNKNOWN_INPUT.into()));
    }
    let mut nodes = REVIEW_MAX_INPUT_NODES;
    let display = match &rule.arguments {
        PermissionArgumentConstraint::Exact { .. } => sanitize_input(input, None, 0, &mut nodes),
        PermissionArgumentConstraint::SelectedDigest { pointers, .. } => {
            selected_display(input, pointers.iter().map(String::as_str), &mut nodes)
        }
        PermissionArgumentConstraint::Selected { arguments } => selected_display(
            input,
            arguments.iter().map(|argument| argument.pointer.as_str()),
            &mut nodes,
        ),
        PermissionArgumentConstraint::Unconstrained => return None,
    };
    Some(
        if serde_json::to_vec(&display).map_or(true, |bytes| bytes.len() > REVIEW_MAX_INPUT_BYTES) {
            Value::String(OMITTED.into())
        } else {
            display
        },
    )
}

fn selected_display<'a>(
    input: &Value,
    pointers: impl Iterator<Item = &'a str>,
    nodes: &mut usize,
) -> Value {
    *nodes -= 2;
    let mut result = Vec::new();
    for pointer in pointers {
        if *nodes <= SELECTED_ENTRY_NODES + 1 || pointer.len() > REVIEW_MAX_STRING_BYTES {
            result.push(Value::String(OMITTED.into()));
            break;
        }
        *nodes -= SELECTED_ENTRY_NODES;
        let selected = selected_input_pointer(input, pointer).ok();
        let (pointer, mask) = pointer_display(input, pointer);
        let mut entry = Map::from_iter([
            ("pointer".into(), Value::String(pointer)),
            ("present".into(), Value::Bool(selected.is_some())),
        ]);
        if let Some(value) = selected {
            let value = if let Some(mask) = mask {
                *nodes -= 1;
                Value::String(mask.into())
            } else {
                sanitize_input(value, None, 2, nodes)
            };
            entry.insert("value".into(), value);
        }
        result.push(Value::Object(entry));
    }
    Value::Array(result)
}

fn pointer_display(input: &Value, pointer: &str) -> (String, Option<&'static str>) {
    let Ok(segments) = decode_json_pointer(pointer) else {
        return (UNKNOWN_FIELD.into(), Some(UNKNOWN_FIELD));
    };
    let mut current = Some(input);
    let mut known = true;
    for segment in &segments {
        if let Some(Value::Array(values)) = current {
            let index = segment
                .parse::<usize>()
                .ok()
                .filter(|index| index.to_string() == *segment);
            known &= index.is_some();
            current = index.and_then(|index| values.get(index));
        } else {
            known &= known_field(segment);
            current = current
                .and_then(|value| value.as_object())
                .and_then(|object| object.get(segment));
        }
    }
    let mask = if segments.iter().any(|segment| secret_key(segment)) {
        Some(REDACTED)
    } else if segments
        .iter()
        .any(|segment| BULK_FIELDS.contains(&segment.as_str()))
    {
        Some(BULK_CONTENT)
    } else if !known {
        Some(UNKNOWN_FIELD)
    } else {
        None
    };
    (
        if known {
            pointer.into()
        } else {
            UNKNOWN_FIELD.into()
        },
        mask,
    )
}

fn known_field(key: &str) -> bool {
    KNOWN_FIELDS.contains(&key)
        || BULK_FIELDS.contains(&key)
        || CREDENTIAL_FIELDS
            .iter()
            .any(|field| key.eq_ignore_ascii_case(field))
}

fn sanitize_input(value: &Value, key: Option<&str>, depth: usize, nodes: &mut usize) -> Value {
    if *nodes == 0 || depth == REVIEW_MAX_INPUT_DEPTH {
        *nodes = nodes.saturating_sub(1);
        return Value::String(OMITTED.into());
    }
    *nodes -= 1;
    if let Some(key) = key {
        if secret_key(key) {
            return Value::String(REDACTED.into());
        }
        if BULK_FIELDS.contains(&key) {
            return Value::String(BULK_CONTENT.into());
        }
        if !KNOWN_FIELDS.contains(&key) {
            return Value::String(UNKNOWN_FIELD.into());
        }
    }
    match value {
        Value::Object(values) => {
            if *nodes <= 1 {
                return Value::String(OMITTED.into());
            }
            *nodes -= 2;
            let mut result = Map::new();
            for (key, value) in values {
                if *nodes <= 1 || key.len() > REVIEW_MAX_STRING_BYTES {
                    result.insert(OMITTED.into(), Value::Bool(true));
                    break;
                }
                if !known_field(key) {
                    result.insert(UNKNOWN_FIELD.into(), Value::Bool(true));
                    continue;
                }
                result.insert(
                    key.clone(),
                    sanitize_input(value, Some(key), depth + 1, nodes),
                );
            }
            Value::Object(result)
        }
        Value::Array(values) => {
            if *nodes == 0 {
                return Value::String(OMITTED.into());
            }
            *nodes -= 1;
            let mut result = Vec::new();
            for value in values {
                if *nodes <= 1 {
                    result.push(Value::String(OMITTED.into()));
                    break;
                }
                result.push(sanitize_input(value, key, depth + 1, nodes));
            }
            Value::Array(result)
        }
        Value::String(value) => {
            Value::String(display_text(value).unwrap_or_else(|| OMITTED.into()))
        }
        _ => value.clone(),
    }
}

fn secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace(['-', '_'], "");
    [
        "auth",
        "token",
        "password",
        "passwd",
        "secret",
        "credential",
        "apikey",
        "privatekey",
        "cookie",
        "header",
        "signature",
    ]
    .iter()
    .any(|part| key.contains(part))
        || matches!(key.as_str(), "key" | "sig" | "accesskey" | "sessionid")
}

fn display_text(value: &str) -> Option<String> {
    if value.len() > REVIEW_MAX_STRING_BYTES {
        return None;
    }
    let text = redact_text(value);
    let text = text
        .chars()
        .flat_map(|character| {
            if character.is_control() {
                character.escape_default().collect::<Vec<_>>()
            } else {
                vec![character]
            }
        })
        .collect::<String>();
    (text.len() <= REVIEW_MAX_STRING_BYTES).then_some(text)
}

fn redact_url(value: &str) -> String {
    let lowercase = value.to_ascii_lowercase();
    let start = lowercase
        .find("https://")
        .into_iter()
        .chain(lowercase.find("http://"))
        .min();
    let Some(start) = start else {
        return value.into();
    };
    let end = value[start..]
        .find(['\'', '"', '`', ' ', '\n'])
        .map_or(value.len(), |end| start + end);
    let Ok(mut url) = Url::parse(&value[start..end]) else {
        return format!("{}{REDACTED}{}", &value[..start], redact_url(&value[end..]));
    };
    let mut changed = false;
    if !url.username().is_empty() || url.password().is_some() {
        let _ = url.set_username("redacted");
        let _ = url.set_password(None);
        changed = true;
    }
    let pairs = url
        .query_pairs()
        .map(|(key, value)| {
            if secret_key(&key) {
                changed = true;
                (key.into_owned(), REDACTED.to_owned())
            } else {
                (key.into_owned(), value.into_owned())
            }
        })
        .collect::<Vec<_>>();
    if changed && url.query().is_some() {
        url.query_pairs_mut().clear().extend_pairs(pairs);
    }
    if url.fragment().is_some() {
        url.set_fragment(Some("redacted"));
        changed = true;
    }
    let suffix = redact_url(&value[end..]);
    if changed {
        format!("{}{}{}", &value[..start], url, suffix)
    } else {
        format!("{}{}", &value[..end], suffix)
    }
}

fn redact_text(value: &str) -> String {
    if value.contains("-----BEGIN") && value.contains("PRIVATE KEY-----") {
        return REDACTED.into();
    }
    let urls = redact_url(value);
    let value = urls.as_str();
    let words = review_words(value);
    let mut result = String::new();
    let mut copied = 0;
    let mut index = 0;
    while let Some(&(start, end, closed)) = words.get(index) {
        let word = &value[start..end];
        let inner = word.trim_matches(['\'', '"']);
        let attached = ATTACHED_VALUE_FLAGS
            .iter()
            .find(|flag| inner.starts_with(**flag) && inner.len() > flag.len())
            .map(|flag| flag.len());
        let separator = inner.find(['=', ':']);
        let name = attached.map_or_else(
            || separator.map_or(inner, |offset| &inner[..offset]),
            |offset| &inner[..offset],
        );
        let spaced = words
            .get(index + 1)
            .is_some_and(|&(start, end, _)| &value[start..end] == "=");
        let credential_flag = matches!(
            name,
            "-H" | "-u" | "--user" | "--header" | "Bearer" | "Basic"
        );
        let content_flag = matches!(
            name,
            "-d" | "--data" | "--data-raw" | "--data-binary" | "--json"
        );
        if !credential_flag
            && !content_flag
            && (!secret_key(name) || (separator.is_none() && !name.starts_with("--") && !spaced))
        {
            result.push_str(&value[copied..start]);
            result.push_str(&redact_url(word));
            copied = end;
            index += 1;
            continue;
        }
        let mut secret_start =
            attached
                .or_else(|| separator.map(|offset| offset + 1))
                .map(|offset| {
                    start + (word.len() - word.trim_start_matches(['\'', '"']).len()) + offset
                });
        let mut secret_end = end;
        let mut incomplete = !closed;
        if secret_start
            .is_none_or(|position| value[position..end].trim_matches(['\'', '"']).is_empty())
        {
            index += 1 + usize::from(spaced);
            if words
                .get(index)
                .is_some_and(|&(start, end, _)| matches!(&value[start..end], "Bearer" | "Basic"))
            {
                index += 1;
            }
            if let Some(&(start, end, closed)) = words.get(index) {
                secret_start = Some(start);
                secret_end = end;
                incomplete |= !closed;
            }
        }
        let start = secret_start.unwrap_or(end);
        let secret = &value[start..secret_end];
        incomplete |= secret.contains("$(") || secret.contains('`') || secret.ends_with('$');
        result.push_str(&value[copied..start]);
        result.push_str(if content_flag { BULK_CONTENT } else { REDACTED });
        if incomplete {
            result.push_str(INCOMPLETE_COMMAND);
            return result;
        }
        copied = secret_end;
        index += 1;
    }
    result.push_str(&value[copied..]);
    result
}

fn shell_operator(character: char) -> bool {
    matches!(character, ';' | '&' | '|' | '<' | '>' | '(' | ')')
}

fn review_words(value: &str) -> Vec<(usize, usize, bool)> {
    let mut words = Vec::new();
    let mut chars = value.char_indices().peekable();
    while let Some((start, first)) = chars.next() {
        if first.is_whitespace() {
            continue;
        }
        let mut quote = matches!(first, '\'' | '"').then_some(first);
        let mut escaped = first == '\\';
        let mut end = start + first.len_utf8();
        if !shell_operator(first) {
            while let Some(&(offset, character)) = chars.peek() {
                if !escaped
                    && quote.is_none()
                    && (character.is_whitespace() || shell_operator(character))
                {
                    break;
                }
                chars.next();
                end = offset + character.len_utf8();
                if escaped {
                    escaped = false;
                } else if character == '\\' && quote != Some('\'') {
                    escaped = true;
                } else if Some(character) == quote {
                    quote = None;
                } else if quote.is_none() && matches!(character, '\'' | '"') {
                    quote = Some(character);
                }
            }
        }
        words.push((start, end, quote.is_none() && !escaped));
    }
    words
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use caudra_config::ToolKey;
    use caudra_storage::permission_state::{PermissionLifetime, PermissionRuleRecord};
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::super::{filesystem_subtree_digest, resource_value_digest, url_origin_digest};
    use super::{
        BULK_CONTENT, FILESYSTEM_SUBTREE_DOMAIN, INCOMPLETE, MAX_CANDIDATE_PREIMAGE_BYTES,
        MAX_CANDIDATE_PREIMAGE_DEPTH, MISSING_INPUT, MISSING_SCOPE, OMITTED,
        PermissionArgumentConstraint, PermissionCapabilityFamily, PermissionRequest,
        PermissionResourceKind, PermissionResourceSelector, PermissionReviewSource,
        PermissionSubject, REDACTED, REVIEW_MAX_INPUT_DEPTH, REVIEW_MAX_INPUT_NODES,
        REVIEW_MAX_RESOURCES, REVIEW_MAX_STRING_BYTES, StructuredPermissionRule, UNKNOWN_FIELD,
        UNKNOWN_INPUT, display_text, review_for_rule, review_from_candidates, selector_label,
        url_subtree_digest, visit_review_candidate_preimages,
    };
    use crate::permissions::{PermissionRowGrant, canonical_json_sha256, selected_input_digest};

    const ROOT: &str = "/project";
    const PATH: &str = "/project/src/main.rs";
    const OTHER_PATH: &str = "/other/src/main.rs";
    const COMMAND: &str = "deploy --token top-secret --target production";
    const CHOSEN: &str = "git status --short";
    const UNCHOSEN: &str = "cargo test";
    const URL: &str = "https://example.test/api/item?token=top-secret&page=2";
    const URL_ROOT: &str = "https://example.test/api";
    const PATTERN: &str = "**/*.{rs,toml}";

    #[test_case(PATH; "raw_and_filesystem_domains_once")]
    fn candidate_digest_visit_is_domain_separated(value: &str) {
        let mut digests = Vec::new();
        assert!(!visit_review_candidate_preimages(
            value,
            None,
            |digest, preimage| digests.push((digest.to_owned(), preimage.to_owned()))
        ));
        assert_eq!(
            digests,
            vec![
                (canonical_json_sha256(&json!(value)), value.into()),
                (
                    canonical_json_sha256(&json!([FILESYSTEM_SUBTREE_DOMAIN, value])),
                    value.into()
                )
            ]
        );
    }

    #[test_case(true; "url_depth_does_not_expand_subtree")]
    #[test_case(false; "filesystem_depth_does_not_expand_subtree")]
    fn candidate_and_render_ancestor_expansion_are_bounded(url: bool) {
        let root = if url { URL_ROOT } else { ROOT };
        let value = format!(
            "{root}{}",
            "/segment".repeat(MAX_CANDIDATE_PREIMAGE_DEPTH + 1)
        );
        let selector = if url {
            PermissionResourceSelector::UrlSubtreeDigest {
                digest: url_subtree_digest(root),
            }
        } else {
            PermissionResourceSelector::FilesystemSubtreeDigest {
                digest: canonical_json_sha256(&json!([FILESYSTEM_SUBTREE_DOMAIN, root])),
            }
        };
        let kind = if url {
            PermissionResourceKind::Url
        } else {
            PermissionResourceKind::File
        };
        if url {
            let mut digests = Vec::new();
            assert!(visit_review_candidate_preimages(
                &value,
                Some(MAX_CANDIDATE_PREIMAGE_DEPTH),
                |digest, _| digests.push(digest.to_owned())
            ));
            assert_eq!(digests.len(), 2);
            assert!(!digests.contains(&url_subtree_digest(root)));
        }
        assert!(selector_label(&selector, &kind, &[value]).is_none());
    }

    #[test_case(MAX_CANDIDATE_PREIMAGE_BYTES + 1; "oversize_preimage")]
    fn candidate_byte_limit_precedes_hashing(bytes: usize) {
        let mut visits = 0;
        assert!(visit_review_candidate_preimages(
            &"x".repeat(bytes),
            Some(MAX_CANDIDATE_PREIMAGE_DEPTH),
            |_, _| visits += 1
        ));
        assert_eq!(visits, 0);
    }

    fn request(tool: &str, input: Value, resources: &[&str]) -> PermissionRequest {
        PermissionRequest::from_legacy(
            "review".into(),
            ToolKey::native(tool),
            resources.iter().map(|value| (*value).into()).collect(),
            input,
            Path::new(ROOT),
            false,
        )
    }

    fn exact_rule(request: &PermissionRequest) -> StructuredPermissionRule {
        request
            .option_rule("allow_exact", PermissionLifetime::Conversation)
            .unwrap()
    }

    #[test_case("file_glob"; "glob")]
    #[test_case("file_grep"; "grep")]
    fn exact_input_preserves_named_paths_patterns_and_paging(tool: &str) {
        let input = json!({"path": ROOT, "pattern": PATTERN, "offset": 10, "limit": 20, "headers": {"Authorization": "top-secret"}, "password": "top-secret", "payload": "top-secret", "content": "large file contents"});
        let request = request(tool, input, &[ROOT]);
        let rule = exact_rule(&request);
        let review = review_for_rule(&request, &rule);
        assert_eq!(review.source, PermissionReviewSource::Approved);
        let input = review.input.as_ref().unwrap();
        assert_eq!(input["path"], ROOT);
        assert_eq!(input["pattern"], PATTERN);
        assert_eq!(input["offset"], 10);
        assert_eq!(input["limit"], 20);
        assert_eq!(input["headers"], REDACTED);
        assert_eq!(input["password"], REDACTED);
        assert!(input.get("payload").is_none());
        assert_eq!(input[UNKNOWN_FIELD], true);
        assert_eq!(input["content"], BULK_CONTENT);
        assert!(
            !serde_json::to_string(&review)
                .unwrap()
                .contains("top-secret")
        );
        PermissionRuleRecord::conversation_with_review(rule, Some(review)).unwrap();
    }

    #[test_case(COMMAND; "flag")]
    #[test_case("TOKEN=top-secret deploy --target production"; "environment")]
    #[test_case("deploy --token='top-secret' --target production"; "quoted_assignment")]
    #[test_case("deploy --token \"top-secret value\" --target production"; "quoted_value")]
    #[test_case("deploy -H 'Authorization: Bearer top-secret' --target production"; "header")]
    #[test_case("deploy -u user:top-secret --target production"; "basic_auth")]
    #[test_case("deploy --user=alice:top-secret --target production"; "long_user_assignment")]
    #[test_case("deploy -ualice:top-secret --target production"; "short_attached_user")]
    #[test_case("deploy -u=alice:top-secret --target production"; "short_attached_assignment")]
    #[test_case("deploy -H'Authorization: Bearer top-secret' --target production"; "short_attached_header")]
    #[test_case("deploy HTTP://alice:top-secret@example.test/api --target production"; "uppercase_http_credentials")]
    #[test_case("deploy HtTpS://example.test/?token=top-secret&page=2 --target production"; "mixedcase_https_query")]
    #[test_case("deploy --authorization Bearer top-secret --target production"; "authorization_scheme")]
    #[test_case("deploy -H Authorization: Bearer top-secret --target production"; "unquoted_header")]
    #[test_case("deploy --json '{\"name\":\"example\",\"password\":\"top-secret\"}' --target production"; "inline_payload")]
    #[test_case("deploy http://example.test/?page=2&to%6ben=top-secret https://example.test --target production"; "encoded_query_key")]
    #[test_case("deploy https://user:top-secret@example.test/api?token=top-secret --target production"; "url")]
    fn command_redaction_preserves_action(command: &str) {
        let display = display_text(command).unwrap();
        assert!(display.contains("deploy"));
        assert!(display.contains("production"));
        assert!(!display.contains("top-secret"));
    }

    #[test_case("deploy --token=$(echo top-secret)"; "expansion")]
    #[test_case("deploy --token='top-secret"; "unclosed_quote")]
    fn ambiguous_credential_boundary_omits_remainder(command: &str) {
        let display = display_text(command).unwrap();
        assert!(display.contains("deploy"));
        assert!(!display.contains("top-secret"));
        assert!(display.contains(super::INCOMPLETE_COMMAND));
    }

    #[test_case(PermissionReviewSource::Approved; "approved")]
    #[test_case(PermissionReviewSource::Recovered; "recovered")]
    fn url_query_redacts_only_credentials(source: PermissionReviewSource) {
        let request = request(
            "webfetch",
            json!({"url": URL, "format": "markdown"}),
            &[URL],
        );
        let rule = exact_rule(&request);
        let review = review_from_candidates(
            &rule,
            "webfetch",
            Some(&request.input),
            &[URL.into()],
            source,
        );
        let display = serde_json::to_string(&review).unwrap();
        assert!(!display.contains("top-secret"));
        assert!(display.contains("page=2"));
        assert!(display.contains("/api/item"));
        assert!(display.contains("markdown"));
    }

    #[test_case(false; "exact_command")]
    #[test_case(true; "command_pattern")]
    fn composed_rule_review_contains_only_chosen_authority(pattern: bool) {
        let request = request(
            "bash",
            json!({"command": format!("{CHOSEN} && {UNCHOSEN}")}),
            &[CHOSEN, UNCHOSEN],
        );
        let grant = if pattern {
            PermissionRowGrant::Written("git status *".into())
        } else {
            PermissionRowGrant::Offered("command_exact_0".into())
        };
        let rules = request
            .composed_rules(&[Some(grant), None], &PermissionLifetime::Conversation)
            .unwrap();
        assert_eq!(rules.len(), 1);
        let review = review_for_rule(&request, &rules[0]);
        assert!(review.input.is_none());
        let text = serde_json::to_string(&review).unwrap();
        assert!(text.contains("git status"));
        assert!(!text.contains(UNCHOSEN));
        assert!(review.resources[0].attributes["workdir"].contains(ROOT));
        PermissionRuleRecord::conversation_with_review(rules[0].clone(), Some(review)).unwrap();
    }

    #[test_case(false; "selected_digest")]
    #[test_case(true; "unconstrained")]
    fn selected_or_unconstrained_input_never_depicts_exact_trigger(unconstrained: bool) {
        let request = request(
            "file_glob",
            json!({"path": ROOT, "pattern": PATTERN, "limit": 7}),
            &[ROOT],
        );
        let mut rule = exact_rule(&request);
        rule.arguments = if unconstrained {
            PermissionArgumentConstraint::Unconstrained
        } else {
            PermissionArgumentConstraint::SelectedDigest {
                pointers: vec!["/pattern".into()],
                digest: selected_input_digest(&request.input, &["/pattern"]).unwrap(),
            }
        };
        let review = review_for_rule(&request, &rule);
        assert!(!review.authority.contains("exact input"));
        assert_eq!(
            review.input,
            if unconstrained {
                None
            } else {
                Some(json!([{ "pointer": "/pattern", "present": true, "value": PATTERN }]))
            }
        );
        PermissionRuleRecord::conversation_with_review(rule, Some(review)).unwrap();
    }

    #[test_case(false; "wrong_candidates")]
    #[test_case(true; "unavailable_ignores_candidates")]
    fn missing_preimages_are_explicit(unavailable: bool) {
        let request = request("file_read", json!({"filePath": PATH}), &[PATH]);
        let rule = exact_rule(&request);
        let review = review_from_candidates(
            &rule,
            "file_read",
            None,
            &[if unavailable {
                PATH.into()
            } else {
                OTHER_PATH.into()
            }],
            if unavailable {
                PermissionReviewSource::Unavailable
            } else {
                PermissionReviewSource::Recovered
            },
        );
        assert_eq!(review.resources[0].value, None);
        assert!(review.authority.contains(INCOMPLETE));
        assert!(review.authority.contains(MISSING_INPUT));
    }

    #[test_case("filesystem"; "filesystem_ancestor_hash")]
    #[test_case("url_subtree"; "url_ancestor_hash")]
    #[test_case("url_origin"; "url_origin_hash")]
    fn scope_labels_name_verified_root_not_triggering_descendant(kind: &str) {
        let request = request("file_read", json!({"filePath": PATH}), &[PATH]);
        let mut rule = exact_rule(&request);
        rule.arguments = PermissionArgumentConstraint::Unconstrained;
        let (selector, resource_kind, candidate, expected) = match kind {
            "filesystem" => (
                PermissionResourceSelector::FilesystemSubtreeDigest {
                    digest: filesystem_subtree_digest(ROOT).unwrap(),
                },
                PermissionResourceKind::Directory,
                PATH,
                ROOT,
            ),
            "url_subtree" => (
                PermissionResourceSelector::UrlSubtreeDigest {
                    digest: url_subtree_digest(URL_ROOT),
                },
                PermissionResourceKind::Url,
                URL,
                URL_ROOT,
            ),
            "url_origin" => (
                PermissionResourceSelector::UrlOriginDigest {
                    digest: url_origin_digest(URL).unwrap(),
                },
                PermissionResourceKind::Url,
                URL,
                "https://example.test",
            ),
            _ => unreachable!(),
        };
        rule.resources[0].selector = selector;
        rule.resources[0].kind = resource_kind;
        rule.resources[0].attributes = BTreeMap::from([(
            "workdir".into(),
            PermissionResourceSelector::Digest {
                digest: resource_value_digest(ROOT, &PermissionResourceKind::Directory).unwrap(),
            },
        )]);
        let review = review_from_candidates(
            &rule,
            "file_read",
            None,
            &[candidate.into()],
            PermissionReviewSource::Recovered,
        );
        assert!(
            review.resources[0]
                .value
                .as_ref()
                .unwrap()
                .ends_with(expected)
        );
        assert!(
            !review.resources[0]
                .value
                .as_ref()
                .unwrap()
                .contains(candidate)
        );
        assert_eq!(review.resources[0].attributes["workdir"], MISSING_SCOPE);
    }

    #[test_case("mcp"; "untrusted_mcp")]
    #[test_case("unknown"; "unknown_native_tool")]
    fn arbitrary_payloads_are_not_persisted(kind: &str) {
        let request = request(
            "file_read",
            json!({"command": COMMAND, "path": PATH}),
            &[PATH],
        );
        let mut rule = exact_rule(&request);
        let tool = if kind == "mcp" {
            rule.subject = PermissionSubject::Mcp {
                server: "server".into(),
                authority: "server".into(),
                tool: "file_read".into(),
                contract: "mcp".into(),
            };
            "file_read"
        } else {
            "unknown"
        };
        let review = review_from_candidates(
            &rule,
            tool,
            Some(&request.input),
            &[],
            PermissionReviewSource::Recovered,
        );
        assert_eq!(review.input, Some(json!(UNKNOWN_INPUT)));
    }

    #[test_case("string"; "oversize_scope")]
    #[test_case("depth"; "input_depth")]
    #[test_case("nodes"; "input_nodes")]
    #[test_case("resources"; "resource_count")]
    fn generated_reviews_fit_storage_without_truncated_exact_paths(case: &str) {
        let mut request = request("file_read", json!({"filePath": PATH}), &[PATH]);
        if case == "string" {
            request.resources[0].value = format!("/{PATH}{}", "x".repeat(REVIEW_MAX_STRING_BYTES));
        }
        if case == "depth" {
            for _ in 0..=REVIEW_MAX_INPUT_DEPTH {
                request.input = json!({"path": request.input});
            }
        }
        if case == "nodes" {
            request.input = json!({"paths": vec![PATH; REVIEW_MAX_INPUT_NODES]});
        }
        let mut rule = exact_rule(&request);
        rule.arguments = PermissionArgumentConstraint::Exact {
            digest: canonical_json_sha256(&request.input),
        };
        if case == "string" {
            rule.resources[0].selector = PermissionResourceSelector::Digest {
                digest: resource_value_digest(
                    &request.resources[0].value,
                    &PermissionResourceKind::File,
                )
                .unwrap(),
            };
        }
        if case == "resources" {
            rule.resources = vec![rule.resources[0].clone(); REVIEW_MAX_RESOURCES + 1];
        }
        let review = review_for_rule(&request, &rule);
        if case == "string" {
            assert_eq!(review.resources[0].value, None);
        }
        if case == "depth" || case == "nodes" {
            assert!(serde_json::to_string(&review).unwrap().contains(OMITTED));
        }
        PermissionRuleRecord::conversation_with_review(rule, Some(review)).unwrap();
    }

    #[test_case(PermissionReviewSource::Recovered; "recovered")]
    #[test_case(PermissionReviewSource::Unavailable; "unavailable")]
    fn unavailable_family_authority_is_still_explicit(source: PermissionReviewSource) {
        let request = request("file_read", json!({"filePath": PATH}), &[PATH]);
        let mut rule = exact_rule(&request);
        rule.family = Some(PermissionCapabilityFamily::FilesystemRead);
        let review = review_from_candidates(&rule, "file_read", None, &[], source);
        assert!(review.authority.contains("Filesystem read family"));
        assert_eq!(review.resources[0].value, None);
    }

    #[test_case(json!({"headers": {"path": "sensitive-value"}}), "/headers/path", "/headers/path", REDACTED; "secret_ancestor")]
    #[test_case(json!({"headers": [{"path": "sensitive-value"}]}), "/headers/0/path", "/headers/0/path", REDACTED; "secret_array_ancestor")]
    #[test_case(json!({"content": {"path": "sensitive-value"}}), "/content/path", "/content/path", BULK_CONTENT; "payload_ancestor")]
    #[test_case(json!({"credential/value": {"path": "sensitive-value"}}), "/credential~1value/path", UNKNOWN_FIELD, REDACTED; "decoded_ancestor")]
    #[test_case(json!({"opaque-key-value": {"path": "sensitive-value"}}), "/opaque-key-value/path", UNKNOWN_FIELD, UNKNOWN_FIELD; "unknown_ancestor")]
    #[test_case(json!({"path": {"opaque-key-value": "sensitive-value"}}), "/path/opaque-key-value", UNKNOWN_FIELD, UNKNOWN_FIELD; "unknown_leaf")]
    fn selected_review_classifies_every_decoded_pointer_segment(
        input: Value,
        pointer: &str,
        shown_pointer: &str,
        mask: &str,
    ) {
        let request = request("file_read", input, &[PATH]);
        let mut rule = exact_rule(&request);
        rule.arguments = PermissionArgumentConstraint::SelectedDigest {
            pointers: vec![pointer.into()],
            digest: selected_input_digest(&request.input, &[pointer]).unwrap(),
        };
        let review = review_for_rule(&request, &rule);
        assert_eq!(
            review.input,
            Some(json!([{"pointer": shown_pointer, "present": true, "value": mask}]))
        );
        let text = serde_json::to_string(&review).unwrap();
        assert!(!text.contains("sensitive-value"));
        assert!(!text.contains("opaque-key-value"));
        assert!(!text.contains("credential~1value"));
        PermissionRuleRecord::conversation_with_review(rule, Some(review)).unwrap();
    }

    #[test_case(false; "root_keys")]
    #[test_case(true; "nested_keys")]
    fn unknown_keys_never_enter_the_review(nested: bool) {
        let input = json!({"opaque-key-value": "sensitive-value", "X-Auth-s3cr3t": "sensitive-value", "filePath": PATH});
        let input = if nested {
            json!({"paths": [input]})
        } else {
            input
        };
        let request = request("file_read", input, &[PATH]);
        let mut rule = exact_rule(&request);
        rule.resources[0].attributes.insert(
            "opaque-key-value".into(),
            PermissionResourceSelector::Digest {
                digest: canonical_json_sha256(&json!(ROOT)),
            },
        );
        let review = review_for_rule(&request, &rule);
        let text = serde_json::to_string(&review).unwrap();
        assert!(!text.contains("opaque-key-value"));
        assert!(!text.contains("s3cr3t"));
        assert!(!text.contains("sensitive-value"));
        assert!(text.contains("filePath"));
        assert!(text.contains(PATH));
        assert!(text.contains(UNKNOWN_FIELD));
        assert!(review.resources[0].attributes.is_empty());
        PermissionRuleRecord::conversation_with_review(rule, Some(review)).unwrap();
    }

    #[test_case(PermissionReviewSource::Approved; "approved")]
    #[test_case(PermissionReviewSource::Recovered; "recovered")]
    fn selected_presence_is_explicit_without_changing_authorization(
        source: PermissionReviewSource,
    ) {
        let request = request("file_read", json!({"filePath": null, "limit": 20}), &[PATH]);
        let mut rule = exact_rule(&request);
        let pointers = ["/filePath", "/offset", "/limit"];
        rule.arguments = PermissionArgumentConstraint::SelectedDigest {
            pointers: pointers.iter().map(|pointer| (*pointer).into()).collect(),
            digest: selected_input_digest(&request.input, &pointers).unwrap(),
        };
        let original = serde_json::to_vec(&rule).unwrap();
        let review = review_from_candidates(
            &rule,
            "file_read",
            Some(&request.input),
            &[PATH.into()],
            source,
        );
        assert_eq!(
            review.input,
            Some(json!([
                {"pointer": "/filePath", "present": true, "value": null},
                {"pointer": "/offset", "present": false},
                {"pointer": "/limit", "present": true, "value": 20},
            ]))
        );
        let record = PermissionRuleRecord::conversation_with_review(rule, Some(review)).unwrap();
        assert_eq!(serde_json::to_vec(&record.rule).unwrap(), original);
        assert!(super::argument_constraint_matches(
            &record.rule.arguments,
            &request.input
        ));
        let mut changed = request.input.clone();
        changed["offset"] = Value::Null;
        assert!(!super::argument_constraint_matches(
            &record.rule.arguments,
            &changed
        ));
        let unverified = review_from_candidates(
            &record.rule,
            "file_read",
            Some(&changed),
            &[OTHER_PATH.into()],
            PermissionReviewSource::Recovered,
        );
        assert!(unverified.input.is_none());
        assert_eq!(unverified.resources[0].value, None);
    }

    #[cfg(unix)]
    #[test_case(false; "exact")]
    #[test_case(true; "subtree")]
    fn historical_preimages_ignore_replaced_directories_and_reject_current_aliases(subtree: bool) {
        use std::fs;
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let old = temp.path().join("maki");
        let new = temp.path().join("caudra");
        fs::create_dir(&old).unwrap();
        fs::create_dir(&new).unwrap();
        let old_file = old.join("src.rs");
        let old_value = old_file.to_str().unwrap();
        let old_root = old.to_str().unwrap();
        let request = request("file_read", json!({"filePath": old_value}), &[old_value]);
        let mut rule = exact_rule(&request);
        rule.arguments = PermissionArgumentConstraint::Unconstrained;
        rule.resources[0].selector = if subtree {
            PermissionResourceSelector::FilesystemSubtreeDigest {
                digest: canonical_json_sha256(&json!(["filesystem_subtree", old_root])),
            }
        } else {
            PermissionResourceSelector::Digest {
                digest: canonical_json_sha256(&json!(old_value)),
            }
        };
        rule.resources[0].attributes.insert(
            "workdir".into(),
            PermissionResourceSelector::Digest {
                digest: canonical_json_sha256(&json!(old_root)),
            },
        );
        let original = serde_json::to_vec(&rule).unwrap();
        let candidates = vec![old_value.into(), old_root.into()];
        let before = review_from_candidates(
            &rule,
            "file_read",
            None,
            &candidates,
            PermissionReviewSource::Recovered,
        );
        fs::rename(&old, temp.path().join("retired")).unwrap();
        symlink(&new, &old).unwrap();
        let after = review_from_candidates(
            &rule,
            "file_read",
            None,
            &candidates,
            PermissionReviewSource::Recovered,
        );
        assert_eq!(after, before);
        let expected = if subtree {
            format!("Filesystem subtree: {old_root}")
        } else {
            format!("Exact: {old_value}")
        };
        assert_eq!(after.resources[0].value.as_deref(), Some(expected.as_str()));
        assert_eq!(
            after.resources[0].attributes["workdir"],
            format!("Exact: {old_root}")
        );
        assert_eq!(serde_json::to_vec(&rule).unwrap(), original);
        let approved = review_for_rule(&request, &rule);
        assert_eq!(approved.resources[0].value, after.resources[0].value);

        rule.resources[0].selector = if subtree {
            PermissionResourceSelector::FilesystemSubtreeDigest {
                digest: canonical_json_sha256(&json!([
                    "filesystem_subtree",
                    new.to_str().unwrap()
                ])),
            }
        } else {
            PermissionResourceSelector::Digest {
                digest: canonical_json_sha256(&json!(new.join("src.rs").to_str().unwrap())),
            }
        };
        rule.resources[0].attributes.insert(
            "workdir".into(),
            PermissionResourceSelector::Digest {
                digest: canonical_json_sha256(&json!(new.to_str().unwrap())),
            },
        );
        let alias = review_from_candidates(
            &rule,
            "file_read",
            None,
            &candidates,
            PermissionReviewSource::Recovered,
        );
        assert_eq!(alias.resources[0].value, None);
        assert_eq!(alias.resources[0].attributes["workdir"], MISSING_SCOPE);
    }

    #[test_case("project/src.rs"; "relative")]
    #[test_case("/project/../other/src.rs"; "parent_component")]
    #[test_case("/project/./src.rs"; "current_component")]
    #[test_case("/project//src.rs"; "repeated_separator")]
    #[test_case("/project/src.rs/"; "trailing_separator")]
    fn filesystem_candidates_are_not_reinterpreted(candidate: &str) {
        let request = request("file_read", json!({"filePath": PATH}), &[PATH]);
        let mut rule = exact_rule(&request);
        rule.resources[0].selector = PermissionResourceSelector::Digest {
            digest: canonical_json_sha256(&json!(candidate)),
        };
        let review = review_from_candidates(
            &rule,
            "file_read",
            None,
            &[candidate.into()],
            PermissionReviewSource::Recovered,
        );
        assert_eq!(review.resources[0].value, None);
        rule.resources[0].selector = PermissionResourceSelector::FilesystemSubtreeDigest {
            digest: canonical_json_sha256(&json!(["filesystem_subtree", ROOT])),
        };
        let review = review_from_candidates(
            &rule,
            "file_read",
            None,
            &[candidate.into()],
            PermissionReviewSource::Recovered,
        );
        assert_eq!(review.resources[0].value, None);
    }

    #[test_case(COMMAND; "separate_secret_value")]
    #[test_case("deploy --user=alice:top-secret --target production"; "attached_secret_value")]
    fn chosen_command_redaction_does_not_mutate_granted_authority(command: &str) {
        let request = request(
            "bash",
            json!({"command": format!("{command} && {UNCHOSEN}")}),
            &[command, UNCHOSEN],
        );
        let rules = request
            .composed_rules(
                &[
                    Some(PermissionRowGrant::Offered("command_exact_0".into())),
                    None,
                ],
                &PermissionLifetime::Conversation,
            )
            .unwrap();
        let original = serde_json::to_vec(&rules).unwrap();
        let review = review_for_rule(&request, &rules[0]);
        let text = serde_json::to_string(&review).unwrap();
        assert!(text.contains("deploy"));
        assert!(text.contains("production"));
        assert!(!text.contains("top-secret"));
        assert!(!text.contains(UNCHOSEN));
        assert!(review.input.is_none());
        assert_eq!(serde_json::to_vec(&rules).unwrap(), original);
        assert!(crate::permissions::resource_constraint_matches(
            &rules[0].resources[0],
            &request.resources[0]
        ));
        let mut changed = request.resources[0].clone();
        changed.value = command.replace("top-secret", "different-value");
        assert!(!crate::permissions::resource_constraint_matches(
            &rules[0].resources[0],
            &changed
        ));
        PermissionRuleRecord::conversation_with_review(rules[0].clone(), Some(review)).unwrap();
    }
}
