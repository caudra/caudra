use super::{
    COMMAND_OBSERVATION_BINDING_ATTRIBUTE, POSSIBLE_WORKDIRS_ATTRIBUTE, resources::PreparedWorkdirs,
};
use caudra_storage::permission_patterns::{
    ArgumentDomain, PatternDefinition, PatternToken, SlotCombinations, SlotId,
};
use caudra_storage::permission_state::{
    BROWSE_RECURSION_ATTRIBUTE, PermissionReview, PermissionReviewResource, PermissionReviewSource,
    REVIEW_MAX_ATTRIBUTES, REVIEW_MAX_INPUT_BYTES, REVIEW_MAX_INPUT_DEPTH, REVIEW_MAX_INPUT_NODES,
    REVIEW_MAX_JSON_BYTES, REVIEW_MAX_RESOURCES, REVIEW_MAX_STRING_BYTES,
    filesystem_browse_recursion,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
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
const FALLBACK_SLOT_NAME: &str = "value";
const MAX_SLOT_NAME_BYTES: usize = 24;
const SLOT_NAME_PUNCTUATION: &[char] = &['-', '_', '.'];
const MAX_LISTED_VALUES: usize = 4;
const MAX_VALUE_CHARS: usize = 40;
const SHORTENED: char = '…';
const ONE_OTHER: &str = "1 other";
const OTHERS: &str = "others";
const NOTHING: &str = "nothing";
const SEEN_COMBINATIONS_ONLY: &str = "Only in combinations seen before.";
const UNKNOWN_INPUT: &str = "[omitted: unrecognized tool input]";
const UNKNOWN_FIELD: &str = "[omitted: unrecognized field]";
const BULK_CONTENT: &str = "[omitted: content payload]";
const MISSING_SCOPE: &str = "[scope unavailable: no verified preimage]";
const SCOPE_SEPARATOR: &str = ": ";
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
const CURL_CLIENTS: &[&str] = &["curl", "curlie"];
const CURL_CREDENTIAL_FLAGS: &[&str] = &["-H", "-u", "--user", "--header"];
const CURL_CONTENT_FLAGS: &[&str] = &["-d", "--data", "--data-raw", "--data-binary", "--json"];
const AUTH_SCHEMES: &[&str] = &["Bearer", "Basic"];
const CREDENTIAL_SHAPE: &[char] = &[':', '=', '@', '{', '['];
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
    let value = match selector {
        PermissionResourceSelector::CommandTemplate { definition } => {
            return Some(command_template_label(definition));
        }
        PermissionResourceSelector::Any => return Some("Any resource".into()),
        PermissionResourceSelector::Exact { value }
        | PermissionResourceSelector::Prefix { value } => value.clone(),
        PermissionResourceSelector::Subtree { root } => root.clone(),
        PermissionResourceSelector::CommandPattern { pattern } => pattern.clone(),
        PermissionResourceSelector::RemoteResource { scope, .. }
        | PermissionResourceSelector::RemoteSubtree { scope, .. } => {
            serde_json::to_string(scope).ok()?
        }
        PermissionResourceSelector::Digest { digest } => candidates
            .iter()
            .filter_map(|candidate| match kind {
                PermissionResourceKind::File | PermissionResourceKind::Directory => {
                    absolute_preimage(candidate).map(|_| candidate.clone())
                }
                PermissionResourceKind::Url => strict_http_url(candidate).map(|url| url.key),
                _ => Some(candidate.clone()),
            })
            .find(|value| canonical_json_sha256(&json!(value)) == *digest)?,
        PermissionResourceSelector::FilesystemSubtreeDigest { digest } => candidates
            .iter()
            .filter_map(|candidate| absolute_preimage(candidate))
            .find_map(|path| {
                path.ancestors()
                    .take(MAX_CANDIDATE_PREIMAGE_DEPTH)
                    .filter_map(|root| root.to_str())
                    .find(|root| {
                        canonical_json_sha256(&json!([FILESYSTEM_SUBTREE_DOMAIN, root])) == *digest
                    })
                    .map(str::to_owned)
            })?,
        PermissionResourceSelector::UrlSubtreeDigest { digest } => candidates
            .iter()
            .filter_map(|value| strict_http_url(value))
            .filter(|url| url_within_depth(&url.url, MAX_CANDIDATE_PREIMAGE_DEPTH))
            .filter_map(|url| url_subtree_roots(&url))
            .flatten()
            .find(|root| url_subtree_digest(root) == *digest)?,
        PermissionResourceSelector::UrlOriginDigest { digest } => candidates
            .iter()
            .filter_map(|value| strict_http_url(value))
            .map(|url| url.url.origin().ascii_serialization())
            .find(|origin| canonical_json_sha256(&json!([URL_ORIGIN_DOMAIN, origin])) == *digest)?,
    };
    let value = display_text(&value)?;
    let label = format!("{}{SCOPE_SEPARATOR}{value}", labelled_scope(selector)?);
    (label.len() <= REVIEW_MAX_STRING_BYTES).then_some(label)
}

/// The kind a review label names a selector's value by.
fn labelled_scope(selector: &PermissionResourceSelector) -> Option<&'static str> {
    Some(match selector {
        PermissionResourceSelector::Exact { .. } | PermissionResourceSelector::Digest { .. } => {
            "Exact"
        }
        PermissionResourceSelector::Subtree { .. } => "Subtree",
        PermissionResourceSelector::Prefix { .. } => "Prefix",
        PermissionResourceSelector::CommandPattern { .. } => "Command pattern",
        PermissionResourceSelector::RemoteResource { .. } => "Remote exact scope",
        PermissionResourceSelector::RemoteSubtree { .. } => "Remote subtree",
        PermissionResourceSelector::FilesystemSubtreeDigest { .. } => "Filesystem subtree",
        PermissionResourceSelector::UrlSubtreeDigest { .. } => "URL subtree",
        PermissionResourceSelector::UrlOriginDigest { .. } => "URL origin",
        PermissionResourceSelector::Any | PermissionResourceSelector::CommandTemplate { .. } => {
            return None;
        }
    })
}

/// The value a review label recovered for `selector`, without the kind the
/// label names it by. `None` when the review holds no value for it, which is
/// also how a redacted or missing value reads.
pub fn recovered_value<'a>(
    selector: &PermissionResourceSelector,
    label: &'a str,
) -> Option<&'a str> {
    label
        .strip_prefix(labelled_scope(selector)?)?
        .strip_prefix(SCOPE_SEPARATOR)
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
            PatternToken::Slot { id, .. } => slot_label(definition, *id),
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
        let detail = format!("; {}: {domain}", slot_label(definition, slot.id));
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

/// A template as a scope reads in a sentence: its literal words, quoted only
/// where the shell would need it, with each slot named in angle brackets, as
/// in `cargo test -p <crate>`.
pub fn command_template_phrase(definition: &PatternDefinition) -> String {
    definition
        .argv
        .iter()
        .map(|token| match token {
            PatternToken::Exact { value, .. } => {
                display_text(value).map(|value| shell_words::quote(&value).into_owned())
            }
            PatternToken::Slot { id, .. } => Some(slot_label(definition, *id)),
        })
        .collect::<Option<Vec<_>>>()
        .map(|words| words.join(" "))
        .filter(|phrase| phrase.len() <= REVIEW_MAX_STRING_BYTES)
        .unwrap_or_else(|| OMITTED.into())
}

/// A slot the way a template names it, `<package>`: its label as one plain
/// word, else its position, numbered when another slot reads the same.
pub fn slot_label(definition: &PatternDefinition, id: SlotId) -> String {
    let word = |position: usize, label: &str| {
        let word = label
            .trim_start_matches('<')
            .trim_end_matches('>')
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("-");
        let plain = !word.is_empty()
            && word.len() <= MAX_SLOT_NAME_BYTES
            && word.chars().all(|character| {
                character.is_ascii_alphanumeric() || SLOT_NAME_PUNCTUATION.contains(&character)
            });
        if plain {
            word
        } else {
            format!("{FALLBACK_SLOT_NAME}{}", position + 1)
        }
    };
    let words: Vec<_> = definition
        .slots
        .iter()
        .enumerate()
        .map(|(position, slot)| word(position, &slot.label))
        .collect();
    let Some(position) = definition.slots.iter().position(|slot| slot.id == id) else {
        return format!("<{FALLBACK_SLOT_NAME}>");
    };
    let own = &words[position];
    if words.iter().filter(|word| *word == own).count() > 1 {
        format!("<{own}{}>", position + 1)
    } else {
        format!("<{own}>")
    }
}

/// What a template's slots stand for, one sentence each, as in
/// `<package> is caudra-agent or caudra-ui.` `None` for a template that fixes
/// every word.
pub fn command_template_values(definition: &PatternDefinition) -> Option<String> {
    if definition.slots.is_empty() {
        return None;
    }
    let mut sentences: Vec<_> = definition
        .slots
        .iter()
        .map(|slot| {
            let name = slot_label(definition, slot.id);
            match &slot.domain {
                ArgumentDomain::ObservedSet { values } => {
                    format!("{name} is {}.", alternatives(values))
                }
                ArgumentDomain::Exact { value } => format!("{name} is {}.", value_word(value)),
                ArgumentDomain::Glob { pattern } => {
                    format!("{name} matches the wildcard {}.", value_word(pattern))
                }
                ArgumentDomain::Regex { pattern } => {
                    format!("{name} matches the expression {}.", value_word(pattern))
                }
                ArgumentDomain::AnyLiteralArgument => format!("{name} is any one argument."),
            }
        })
        .collect();
    if definition.slots.len() > 1
        && matches!(
            definition.combinations,
            SlotCombinations::ObservedTuples { .. }
        )
    {
        sentences.push(SEEN_COMBINATIONS_ONLY.into());
    }
    Some(sentences.join(" "))
}

/// `a, b, c, d or 3 others`.
fn alternatives(values: &BTreeSet<String>) -> String {
    let mut words: Vec<_> = values
        .iter()
        .take(MAX_LISTED_VALUES)
        .map(|value| value_word(value))
        .collect();
    match values.len().saturating_sub(MAX_LISTED_VALUES) {
        0 => {}
        1 => words.push(ONE_OTHER.into()),
        others => words.push(format!("{others} {OTHERS}")),
    }
    match words.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => NOTHING.into(),
    }
}

/// One value as a sentence quotes it: redacted, escaped, shortened past a
/// glance, and quoted where the shell would need it.
fn value_word(value: &str) -> String {
    let Some(text) = display_text(value) else {
        return OMITTED.into();
    };
    let text = if text.chars().count() > MAX_VALUE_CHARS {
        text.chars()
            .take(MAX_VALUE_CHARS - 1)
            .chain([SHORTENED])
            .collect()
    } else {
        text
    };
    shell_words::quote(&text).into_owned()
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

pub(crate) fn secret_key(key: &str) -> bool {
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

pub(crate) fn redact_text(value: &str) -> String {
    if value.contains("-----BEGIN") && value.contains("PRIVATE KEY-----") {
        return REDACTED.into();
    }
    let urls = redact_url(value);
    let value = urls.as_str();
    let words = review_words(value);
    let curl = curl_words(value, &words);
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
        let content_flag = CURL_CONTENT_FLAGS.contains(&name);
        let curl_flag = content_flag || CURL_CREDENTIAL_FLAGS.contains(&name);
        let credential = AUTH_SCHEMES.contains(&name)
            || (secret_key(name) && (separator.is_some() || name.starts_with("--") || spaced));
        let mut secret_start = None;
        let mut secret_end = end;
        let mut incomplete = !closed;
        let mut next = index + 1;
        if credential || curl_flag {
            secret_start = attached
                .or_else(|| separator.map(|offset| offset + 1))
                .map(|offset| {
                    start + (word.len() - word.trim_start_matches(['\'', '"']).len()) + offset
                })
                .filter(|&position| !value[position..end].trim_matches(['\'', '"']).is_empty());
            if secret_start.is_none() {
                next += usize::from(spaced);
                if words
                    .get(next)
                    .is_some_and(|&(start, end, _)| AUTH_SCHEMES.contains(&&value[start..end]))
                {
                    next += 1;
                }
                if let Some(&(start, end, closed)) = words.get(next)
                    && continues_command(value, &words, next)
                {
                    secret_start = Some(start);
                    secret_end = end;
                    incomplete |= !closed;
                    next += 1;
                }
            }
        }
        let Some(secret_start) = secret_start.filter(|&position| {
            credential || curl[index] || value[position..secret_end].contains(CREDENTIAL_SHAPE)
        }) else {
            result.push_str(&value[copied..start]);
            result.push_str(&redact_url(word));
            copied = end;
            index += 1;
            continue;
        };
        let secret = &value[secret_start..secret_end];
        incomplete |= secret.contains("$(") || secret.contains('`') || secret.ends_with('$');
        result.push_str(&value[copied..secret_start]);
        result.push_str(if content_flag { BULK_CONTENT } else { REDACTED });
        if incomplete {
            result.push_str(INCOMPLETE_COMMAND);
            return result;
        }
        copied = secret_end;
        index = next;
    }
    result.push_str(&value[copied..]);
    result
}

/// Whether each word belongs to a simple command that runs a curl-compatible
/// client, whose `-u`, `-H`, and `-d` style flags always carry credentials or
/// payloads. Elsewhere they often mean something else, such as `find -delete`
/// or `git push -u origin`, so their values are redacted only when shaped like
/// a credential or payload.
fn curl_words(value: &str, words: &[(usize, usize, bool)]) -> Vec<bool> {
    let mut curl = false;
    let mut previous_end = 0;
    words
        .iter()
        .map(|&(start, end, _)| {
            let word = &value[start..end];
            let separator = match word {
                ";" | "|" | "(" | ")" => true,
                "&" => !value[..start].ends_with(['<', '>']) && !value[end..].starts_with('>'),
                _ => value[previous_end..start].contains('\n'),
            };
            if separator {
                curl = false;
            }
            previous_end = end;
            let program = word.trim_matches(['\'', '"', '`', '\\']);
            let program = program.rsplit_once('/').map_or(program, |(_, name)| name);
            curl |= CURL_CLIENTS.contains(&program);
            curl
        })
        .collect()
}

fn continues_command(value: &str, words: &[(usize, usize, bool)], index: usize) -> bool {
    let start = words[index].0;
    !value[words[index - 1].1..start].contains('\n') && !value[start..].starts_with(shell_operator)
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
    use std::collections::{BTreeMap, BTreeSet};
    use std::iter;
    use std::path::Path;

    use caudra_config::ToolKey;
    use caudra_storage::permission_patterns::{
        ArgumentDomain, ArgumentRole, OptionLikePolicy, PATTERN_SCHEMA_VERSION, PatternContext,
        PatternDefinition, PatternSlot, PatternToken, SlotCombinations, SlotId,
    };
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
        UNKNOWN_INPUT, command_template_phrase, command_template_values, display_text,
        recovered_value, redact_text, review_for_rule, review_from_candidates, selector_label,
        slot_label, url_subtree_digest, visit_review_candidate_preimages,
    };
    use crate::permissions::{
        ComposedRow, PermissionRowGrant, canonical_json_sha256, selected_input_digest,
    };

    const ROOT: &str = "/project";
    const PATH: &str = "/project/src/main.rs";
    const OTHER_PATH: &str = "/other/src/main.rs";
    const COMMAND: &str = "deploy --token top-secret --target production";
    const CHOSEN: &str = "git status --short";
    const UNCHOSEN: &str = "cargo test";
    const URL: &str = "https://example.test/api/item?token=top-secret&page=2";
    const URL_ROOT: &str = "https://example.test/api";
    const URL_ORIGIN: &str = "https://example.test";
    const PATTERN: &str = "**/*.{rs,toml}";
    const TEMPLATE_NAME: &str = "template";
    const TEMPLATE_PROGRAM: &str = "cargo";
    const CRATES: &[&str] = &["caudra-agent", "caudra-ui", "caudra-workcell"];
    const LONG_LABEL: &str = "abcdefghijklmnopqrstuvwxyz";
    const LONG_VALUE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

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

    #[test_case("find target/decision-smoke -delete"; "find_delete")]
    #[test_case("git clean -dfx && git branch -d feature/login"; "git_delete_flags")]
    #[test_case("git push -u origin main"; "upstream")]
    #[test_case("pip install --user requests; systemctl --user restart caudra"; "user_scope")]
    #[test_case("gh pr list --json number,title"; "json_fields")]
    #[test_case("npm ls --json | jq ."; "operator_is_not_a_value")]
    #[test_case("grep -Hn needle src/main.rs && kill -HUP 1234"; "attached_letters")]
    #[test_case("curl -s https://example.test\nfind . -delete"; "newline_ends_curl")]
    #[test_case("curl -s https://example.test && git push -u origin main"; "operator_ends_curl")]
    #[test_case("deploy --token\nrm -rf target"; "value_never_crosses_lines")]
    fn non_credential_flags_are_preserved(command: &str) {
        assert_eq!(redact_text(command), command);
    }

    #[test_case("curl -u top-secret https://example.test"; "user")]
    #[test_case("curl -dtop-secret https://example.test"; "attached_data")]
    #[test_case("sudo /usr/bin/curl --data top-secret https://example.test"; "wrapped_path")]
    #[test_case("echo ok | curl -H top-secret https://example.test"; "pipeline")]
    #[test_case("curl https://example.test 2>&1 --user top-secret"; "redirection_is_not_a_separator")]
    fn curl_flags_are_always_redacted(command: &str) {
        let text = redact_text(command);
        assert!(!text.contains("top-secret"));
        assert!(text.contains("https://example.test"));
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
            .composed_rules(&ComposedRow::uniform(
                vec![Some(grant), None],
                &PermissionLifetime::Conversation,
            ))
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

    #[test_case(PermissionResourceSelector::Digest { digest: canonical_json_sha256(&json!(PATH)) }, PermissionResourceKind::File, PATH, PATH; "pinned_path")]
    #[test_case(PermissionResourceSelector::CommandPattern { pattern: CHOSEN.into() }, PermissionResourceKind::Command, CHOSEN, CHOSEN; "command_pattern")]
    #[test_case(PermissionResourceSelector::FilesystemSubtreeDigest { digest: filesystem_subtree_digest(ROOT).unwrap() }, PermissionResourceKind::Directory, PATH, ROOT; "filesystem_subtree")]
    #[test_case(PermissionResourceSelector::UrlSubtreeDigest { digest: url_subtree_digest(URL_ROOT) }, PermissionResourceKind::Url, URL, URL_ROOT; "url_subtree")]
    #[test_case(PermissionResourceSelector::UrlOriginDigest { digest: url_origin_digest(URL).unwrap() }, PermissionResourceKind::Url, URL, URL_ORIGIN; "url_origin")]
    fn recovered_values_drop_only_the_kind_the_label_names(
        selector: PermissionResourceSelector,
        kind: PermissionResourceKind,
        candidate: &str,
        expected: &str,
    ) {
        let label = selector_label(&selector, &kind, &[candidate.into()]).unwrap();
        assert_eq!(recovered_value(&selector, &label), Some(expected));
        assert_eq!(recovered_value(&selector, MISSING_SCOPE), None);
        assert_eq!(recovered_value(&selector, REDACTED), None);
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
            .composed_rules(&ComposedRow::uniform(
                vec![
                    Some(PermissionRowGrant::Offered("command_exact_0".into())),
                    None,
                ],
                &PermissionLifetime::Conversation,
            ))
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

    fn template(
        labels: &[&str],
        domain: &ArgumentDomain,
        combinations: SlotCombinations,
    ) -> PatternDefinition {
        let slot_id = |position: usize| SlotId(u16::try_from(position + 1).unwrap());
        PatternDefinition {
            version: PATTERN_SCHEMA_VERSION,
            name: TEMPLATE_NAME.into(),
            context: PatternContext {
                tool_identity: TEMPLATE_NAME.into(),
                executable_identity: TEMPLATE_PROGRAM.into(),
                effective_workdir: ROOT.into(),
                path_binding: ROOT.into(),
                analysis_version: TEMPLATE_NAME.into(),
            },
            argv: iter::once(PatternToken::Exact {
                value: TEMPLATE_PROGRAM.into(),
                role: ArgumentRole::Executable,
            })
            .chain((0..labels.len()).map(|position| PatternToken::Slot {
                id: slot_id(position),
                role: ArgumentRole::Unknown,
            }))
            .collect(),
            slots: labels
                .iter()
                .enumerate()
                .map(|(position, label)| PatternSlot {
                    id: slot_id(position),
                    label: (*label).into(),
                    domain: domain.clone(),
                    option_like: OptionLikePolicy::Reject,
                })
                .collect(),
            combinations,
        }
    }

    fn observed(values: &[&str]) -> ArgumentDomain {
        ArgumentDomain::ObservedSet {
            values: values.iter().map(|value| (*value).into()).collect(),
        }
    }

    #[test_case(&["<package>"], &["<package>"]; "label_names_the_slot")]
    #[test_case(&["<value1>", "<value2>"], &["<value1>", "<value2>"]; "numbered_labels_stay")]
    #[test_case(&["crate name"], &["<crate-name>"]; "spaces_become_dashes")]
    #[test_case(&["<pattern1> (crate)"], &["<value1>"]; "punctuation_falls_back_to_the_position")]
    #[test_case(&[LONG_LABEL], &["<value1>"]; "long_label_falls_back_to_the_position")]
    #[test_case(&["\u{1b}[31m"], &["<value1>"]; "control_characters_fall_back")]
    #[test_case(&["<crate>", "<crate>"], &["<crate1>", "<crate2>"]; "duplicates_are_numbered")]
    fn slots_read_as_one_plain_word(labels: &[&str], expected: &[&str]) {
        let definition = template(labels, &observed(CRATES), SlotCombinations::Independent);
        let names: Vec<_> = definition
            .slots
            .iter()
            .map(|slot| slot_label(&definition, slot.id))
            .collect();
        assert_eq!(names, expected);
        assert_eq!(
            command_template_phrase(&definition),
            format!("{TEMPLATE_PROGRAM} {}", expected.join(" "))
        );
    }

    #[test_case(&observed(&[CRATES[0]]), "<value> is caudra-agent."; "one_value")]
    #[test_case(&observed(&CRATES[..2]), "<value> is caudra-agent or caudra-ui."; "two_values")]
    #[test_case(&observed(CRATES), "<value> is caudra-agent, caudra-ui or caudra-workcell."; "three_values")]
    #[test_case(&observed(&["a", "b", "c", "d", "e"]), "<value> is a, b, c, d or 1 other."; "one_past_the_cap")]
    #[test_case(&observed(&["a", "b", "c", "d", "e", "f"]), "<value> is a, b, c, d or 2 others."; "several_past_the_cap")]
    #[test_case(&observed(&["two words", "\u{1b}[31m"]), "<value> is '\\u{1b}[31m' or 'two words'."; "values_are_escaped_and_quoted")]
    #[test_case(&observed(&[LONG_VALUE]), "<value> is aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa…."; "long_values_are_shortened")]
    #[test_case(&ArgumentDomain::Exact { value: CRATES[1].into() }, "<value> is caudra-ui."; "exact")]
    #[test_case(&ArgumentDomain::Glob { pattern: "src/*.rs".into() }, "<value> matches the wildcard 'src/*.rs'."; "wildcard")]
    #[test_case(&ArgumentDomain::Regex { pattern: "^caudra-.*$".into() }, "<value> matches the expression '^caudra-.*$'."; "expression")]
    #[test_case(&ArgumentDomain::AnyLiteralArgument, "<value> is any one argument."; "any")]
    fn a_template_says_what_its_slot_stands_for(domain: &ArgumentDomain, expected: &str) {
        let definition = template(&["<value>"], domain, SlotCombinations::Independent);
        assert_eq!(command_template_values(&definition).unwrap(), expected);
    }

    #[test_case(true, "<value1> is a or b. <value2> is a or b. Only in combinations seen before."; "observed_tuples")]
    #[test_case(false, "<value1> is a or b. <value2> is a or b."; "independent")]
    fn several_slots_say_whether_they_combine_freely(tuples: bool, expected: &str) {
        let combinations = match tuples {
            true => SlotCombinations::ObservedTuples {
                tuples: BTreeSet::new(),
            },
            false => SlotCombinations::Independent,
        };
        let definition = template(
            &["<value1>", "<value2>"],
            &observed(&["a", "b"]),
            combinations,
        );
        assert_eq!(command_template_values(&definition).unwrap(), expected);
    }

    #[test]
    fn a_template_without_slots_has_no_values() {
        let definition = template(&[], &observed(CRATES), SlotCombinations::Independent);
        assert_eq!(command_template_values(&definition), None);
    }
}
