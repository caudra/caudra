use std::collections::BTreeSet;
use std::path::Path;

use caudra_agent::permissions::{
    COMMAND_OBSERVATION_ATTRIBUTE, COMMAND_OBSERVATION_BINDING_ATTRIBUTE, PermissionAnswer,
    PermissionArgumentConstraint, PermissionCapabilityFamily, PermissionExecutorKind,
    PermissionLifetime, PermissionRequest, PermissionResource, PermissionResourceAccess,
    PermissionResourceConstraint, PermissionResourceKind, PermissionResourceSelector,
    PermissionRowGrant, PermissionRuleOption, PermissionSubject, RemotePermissionIdentity,
    StructuredPermissionEffect, canonical_json_sha256, resource_constraint_matches,
    review::review_for_rule,
};
use serde::Deserialize;
use serde_json::json;

use super::details::{INCOMPLETE_REDACTION, MAX_REVIEW_CHARS, TRUNCATED, review_text};
use super::inspector::{offered_pattern, pattern_impact, pattern_summary, unknown_role_caution};

pub(super) const NO_POLICY_REASON: &str = "No rule covers this request";
pub(super) const SHELL_REACH: &str =
    "Commands may read or change files elsewhere. The starting directory is not a sandbox.";
pub(super) const WHOLE_CALL: &str =
    "The complete call is submitted unchanged; shell operators control execution.";
pub(super) const MISSING_SCOPE: &str = "Scope unavailable; inspect Details. Approval disabled.";
const GENERIC_REASONS: &[&str] = &[
    "Limited read or lookup operation",
    "External read or network operation",
    "May modify data, execute code, or invoke an external authority",
    "Complex or protected operation requiring exact review",
    "Legacy operation with unknown effects",
];
const POSSIBLE_WORKDIRS_ATTRIBUTE: &str = "possible_workdirs";
const NORMALIZED_COMMAND_ATTRIBUTE: &str = "normalized_command";
pub(super) const INCOMPLETE_REVIEW: &str =
    "Authority is unavailable or truncated. Go back; approval is disabled.";
const PROTECTED_REVIEW: &str = "Includes protected resources. Review the scope before allowing.";
const REMOTE_BINDING_UNAVAILABLE: &str =
    "[omitted: remote scope has no verified binding to this request]";
const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";
const REMOTE_SCOPE_SEPARATOR: char = '\u{1f}';
const NATIVE_SHELL_OWNER: &str = "workcell";
const NATIVE_SHELL_CONTRACT: &str = "shell.execution.v1";

#[derive(PartialEq, Eq)]
pub(super) struct ReviewField {
    pub label: String,
    pub value: String,
}

impl ReviewField {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        let value: String = value.into();
        Self {
            label: label.into(),
            value: review_text(&value),
        }
    }
}

pub(super) struct AuthorityReview {
    pub title: String,
    pub fields: Vec<ReviewField>,
    pub row: Option<usize>,
}

pub(super) struct ReviewDocument {
    pub action: String,
    pub shell: bool,
    pub authorities: Vec<AuthorityReview>,
    pub context: Vec<ReviewField>,
    pub lifetime: PermissionLifetime,
    pub warnings: Vec<String>,
    pub exact_call_workdir: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PreparedDirectories {
    Known { symbolic_paths: Vec<String> },
    Unknown,
}

impl ReviewDocument {
    pub fn new(request: &PermissionRequest, answer: &PermissionAnswer) -> Self {
        let mut document = Self {
            action: requested_action(request),
            shell: request
                .resources
                .iter()
                .any(|resource| resource.kind == PermissionResourceKind::Command),
            authorities: Vec::new(),
            context: vec![ReviewField::new("Tool", request.tool.to_string())],
            lifetime: PermissionLifetime::Once,
            warnings: Vec::new(),
            exact_call_workdir: exact_call_workdir(request, answer).map(review_text),
        };
        match answer {
            PermissionAnswer::AllowOption {
                option_id,
                lifetime,
            } => {
                document.lifetime = lifetime.clone();
                document.add_option(request, option_id, None);
            }
            PermissionAnswer::AllowComposed { rows, lifetime } => {
                document.lifetime = lifetime.clone();
                for (index, grant) in rows.iter().enumerate() {
                    match grant {
                        Some(PermissionRowGrant::Offered(id)) => {
                            document.add_option(request, id, Some(index))
                        }
                        Some(PermissionRowGrant::Pattern { definition, .. }) => {
                            let summary = pattern_summary(definition);
                            document.authorities.push(AuthorityReview {
                                title: summary.label,
                                fields: summary
                                    .lines
                                    .into_iter()
                                    .map(|line| ReviewField::new("", line))
                                    .collect(),
                                row: Some(index),
                            });
                            document.warn(SHELL_REACH);
                            if let Some(caution) = unknown_role_caution(definition) {
                                document.warn(caution);
                            }
                        }
                        Some(PermissionRowGrant::Written(pattern)) => {
                            let mut fields = vec![ReviewField::new("Prefix", pattern)];
                            if let Some(directory) = request
                                .resources
                                .get(index)
                                .and_then(|resource| resource.attributes.get("workdir"))
                            {
                                fields.push(ReviewField::new("Starting directory", directory));
                            }
                            document.authorities.push(AuthorityReview {
                                title: format!("Command {} · prefix", index + 1),
                                fields,
                                row: Some(index),
                            });
                            document.warn(SHELL_REACH);
                        }
                        None => document.authorities.push(AuthorityReview {
                            title: format!("Command {} · once", index + 1),
                            fields: vec![ReviewField::new(
                                "Retained",
                                "None; runs with this call only.",
                            )],
                            row: Some(index),
                        }),
                    }
                }
            }
            PermissionAnswer::DenyAlwaysLocal | PermissionAnswer::DenyAlwaysGlobal => {
                document.lifetime = if matches!(answer, PermissionAnswer::DenyAlwaysLocal) {
                    PermissionLifetime::Project
                } else {
                    PermissionLifetime::Global
                };
                document.authorities.push(AuthorityReview {
                    title: "Deny this exact call".into(),
                    fields: vec![ReviewField::new(
                        "Effect",
                        "Block this input and context; other calls are unchanged.",
                    )],
                    row: None,
                });
            }
            _ => {}
        }
        if document.lifetime == PermissionLifetime::Project {
            document.context.push(ReviewField::new(
                "Project",
                request
                    .presentation
                    .project
                    .as_deref()
                    .and_then(Path::to_str)
                    .unwrap_or("Unavailable"),
            ));
        }
        if document.lifetime == PermissionLifetime::Global {
            document.warn("Applies across all projects and future conversations.");
        }
        if request.resources.iter().any(|resource| resource.protected) {
            document.warn(PROTECTED_REVIEW);
        }
        let bound_directories = bound_directories(request, answer);
        for resource in &request.resources {
            if let Some(directory) = resource.attributes.get("workdir")
                && !bound_directories.contains(directory)
            {
                push_field(
                    &mut document.context,
                    ReviewField::new("Run from", directory),
                );
            }
        }
        document
            .context
            .extend(identity_fields(&request.subject, &request.executor));
        document
    }

    fn warn(&mut self, warning: &str) {
        if !self.warnings.iter().any(|found| found == warning) {
            self.warnings.push(warning.into());
        }
    }

    fn add_option(&mut self, request: &PermissionRequest, id: &str, row: Option<usize>) {
        let Some(option) = request.options.iter().find(|option| option.id == id) else {
            self.warn(INCOMPLETE_REVIEW);
            return;
        };
        let rule = &option.rule;
        if let Some(definition) = offered_pattern(option) {
            let summary = pattern_summary(definition);
            self.authorities.push(AuthorityReview {
                title: summary.label,
                fields: summary
                    .lines
                    .into_iter()
                    .map(|line| ReviewField::new("", line))
                    .collect(),
                row,
            });
            self.warn(SHELL_REACH);
            if let Some(caution) = unknown_role_caution(definition) {
                self.warn(caution);
            }
            return;
        }
        let review = review_for_rule(request, rule);
        let mut fields = vec![ReviewField::new(
            "Arguments",
            match rule.arguments {
                PermissionArgumentConstraint::Exact { .. } => "Only this exact input and context.",
                PermissionArgumentConstraint::Unconstrained => "Other tool arguments may vary.",
                _ => "Selected fields fixed; other arguments may vary. See Details.",
            },
        )];
        if subject_remote_identity(&rule.subject).is_some() {
            fields.insert(
                0,
                ReviewField::new("Authority SHA-256", canonical_json_sha256(&json!(rule))),
            );
        }
        if rule.subject != request.subject || rule.executor != request.executor {
            fields.extend(identity_fields(&rule.subject, &rule.executor));
            if let PermissionSubject::Native { owner, contract } = &rule.subject {
                fields.push(ReviewField::new("Authority owner", owner));
                fields.push(ReviewField::new("Authority contract", contract));
            }
            fields.push(ReviewField::new(
                "Authority executor",
                match rule.executor {
                    PermissionExecutorKind::Native => "Native",
                    PermissionExecutorKind::Lua => "Plugin",
                    PermissionExecutorKind::Mcp => "MCP server",
                    PermissionExecutorKind::RemoteWorkcell => "Remote Workcell",
                    PermissionExecutorKind::UnknownLegacy => "Unverified legacy executor",
                },
            ));
        }
        if let Some(family) = &rule.family {
            fields.push(ReviewField::new(
                "Capability",
                match family {
                    PermissionCapabilityFamily::FilesystemBrowse => {
                        "Trusted names-only browsing; not contents, writes or shell execution."
                    }
                    PermissionCapabilityFamily::FilesystemRead => {
                        "Trusted reading, listing and search; not writes or shell execution."
                    }
                    PermissionCapabilityFamily::McpServer => "All tools on the bound MCP server.",
                },
            ));
        }
        let mut groups = Vec::new();
        for resource in &rule.resources {
            if !groups.iter().any(|other: &&PermissionResourceConstraint| {
                same_resource_scope(other, resource, request)
            }) {
                groups.push(resource);
            }
        }
        if groups.len() > 1 {
            fields.push(ReviewField::new(
                "Alternatives",
                "Each numbered group is separate; all guards within that group apply.",
            ));
        }
        let guard_names = groups
            .iter()
            .flat_map(|resource| resource.attributes.keys())
            .collect::<BTreeSet<_>>();
        for (resource, shown) in rule.resources.iter().zip(&review.resources) {
            let group = groups
                .iter()
                .position(|other| same_resource_scope(other, resource, request))
                .unwrap_or_default();
            let label = |name: &str| {
                if groups.len() > 1 {
                    format!("{} · {name}", group + 1)
                } else {
                    name.into()
                }
            };
            let exact = exact_value(&resource.selector, request);
            let value = match &resource.selector {
                _ if exact
                    .as_deref()
                    .is_some_and(|value| Some(value) == requested_command(request))
                    && resource.kind == PermissionResourceKind::Command =>
                {
                    "Exact command shown above".into()
                }
                PermissionResourceSelector::RemoteResource { scope, .. } => {
                    format!(
                        "Exact scope key: /{}",
                        scope
                            .iter()
                            .map(|part| binding_component(part))
                            .collect::<Vec<_>>()
                            .join("/")
                    )
                }
                PermissionResourceSelector::RemoteSubtree { scope, .. } => {
                    format!(
                        "Scope key and descendants: /{}",
                        scope
                            .iter()
                            .map(|part| binding_component(part))
                            .collect::<Vec<_>>()
                            .join("/")
                    )
                }
                _ if resource.kind == PermissionResourceKind::Command => command_selector_value(
                    &resource.selector,
                    request,
                    shown.value.as_deref().unwrap_or("Unavailable"),
                ),
                _ => shown.value.clone().unwrap_or_else(|| "Unavailable".into()),
            };
            push_field(
                &mut fields,
                ReviewField::new(label(access_label(resource.access.as_ref())), value),
            );
            if groups.len() > 1 {
                push_field(
                    &mut fields,
                    ReviewField::new(
                        label("Protection"),
                        match resource.protected {
                            Some(true) => "Protected only",
                            Some(false) => "Unprotected only",
                            None => "Not restricted by this guard",
                        },
                    ),
                );
                for name in &guard_names {
                    if !resource.attributes.contains_key(*name) {
                        let name = match name.as_str() {
                            "workdir" => "Starting directory",
                            NORMALIZED_COMMAND_ATTRIBUTE => "Preparation",
                            POSSIBLE_WORKDIRS_ATTRIBUTE => "Directories",
                            name => name,
                        };
                        push_field(&mut fields, ReviewField::new(label(name), "Unrestricted"));
                    }
                }
            }
            if let PermissionResourceSelector::RemoteResource { identity, .. }
            | PermissionResourceSelector::RemoteSubtree { identity, .. } = &resource.selector
            {
                let kind = match &resource.kind {
                    PermissionResourceKind::RemoteFile { .. } => "Remote file".into(),
                    PermissionResourceKind::RemoteDirectory { .. } => "Remote directory".into(),
                    PermissionResourceKind::RemoteResource { resource_kind, .. } => {
                        format!("Remote resource kind: {}", binding_component(resource_kind))
                    }
                    _ => REMOTE_BINDING_UNAVAILABLE.into(),
                };
                push_field(&mut fields, ReviewField::new(label("Kind"), kind));
                let identity_matches = remote_identity_valid(identity)
                    && subject_remote_identity(&request.subject) == Some(identity)
                    && subject_remote_identity(&rule.subject) == Some(identity);
                let targets = request
                    .resources
                    .iter()
                    .filter(|target| {
                        identity_matches && resource_constraint_matches(resource, target)
                    })
                    .collect::<Vec<_>>();
                if targets.is_empty() {
                    push_field(
                        &mut fields,
                        ReviewField::new(label("Binding"), REMOTE_BINDING_UNAVAILABLE),
                    );
                }
                for target in targets {
                    let key = target
                        .value
                        .split(REMOTE_SCOPE_SEPARATOR)
                        .map(binding_component)
                        .collect::<Vec<_>>()
                        .join("/");
                    push_field(
                        &mut fields,
                        ReviewField::new(label("Target key"), format!("/{key}")),
                    );
                    if let Some(path) = target.attributes.get("display_path") {
                        push_field(
                            &mut fields,
                            ReviewField::new(label("Display path only"), path),
                        );
                    }
                }
            }
            let workdir = resource
                .attributes
                .get("workdir")
                .and_then(|selector| exact_value(selector, request));
            if resource.attributes.contains_key("workdir") {
                let previous = groups[..group].iter().position(|previous| {
                    previous
                        .attributes
                        .get("workdir")
                        .zip(resource.attributes.get("workdir"))
                        .is_some_and(|(left, right)| selectors_equal(left, right, request))
                });
                let directory = previous.map_or_else(
                    || {
                        shown
                            .attributes
                            .get("workdir")
                            .cloned()
                            .unwrap_or_else(|| "Unavailable".into())
                    },
                    |index| format!("Same as group {}", index + 1),
                );
                push_field(
                    &mut fields,
                    ReviewField::new(label("Starting directory"), directory),
                );
            }
            for (name, selector) in &resource.attributes {
                if name == "workdir" {
                    continue;
                }
                if name == NORMALIZED_COMMAND_ATTRIBUTE
                    && exact_value(selector, request).is_some()
                    && selectors_equal(selector, &resource.selector, request)
                {
                    push_field(
                        &mut fields,
                        ReviewField::new(
                            label("Preparation"),
                            "Normalized command fixed to the command above.",
                        ),
                    );
                    continue;
                }
                let shown_value = shown
                    .attributes
                    .get(name)
                    .map(String::as_str)
                    .unwrap_or("Unavailable");
                let value = if name == NORMALIZED_COMMAND_ATTRIBUTE {
                    if matches!(selector, PermissionResourceSelector::Any) {
                        "Any value; attribute must be present".into()
                    } else {
                        command_selector_value(selector, request, shown_value)
                    }
                } else {
                    shown_value.to_owned()
                };
                if name == POSSIBLE_WORKDIRS_ATTRIBUTE {
                    let directories = exact_value(selector, request)
                        .and_then(|raw| serde_json::from_str::<PreparedDirectories>(&raw).ok());
                    let same = matches!(&directories, Some(PreparedDirectories::Known { symbolic_paths }) if symbolic_paths.len() == 1 && workdir.as_ref() == symbolic_paths.first());
                    push_field(
                        &mut fields,
                        ReviewField::new(
                            label("Directories"),
                            if same {
                                "Only the starting directory above"
                            } else {
                                value.as_str()
                            },
                        ),
                    );
                } else {
                    let name = match name.as_str() {
                        "workdir" => "Starting directory",
                        NORMALIZED_COMMAND_ATTRIBUTE => "Preparation",
                        name => name,
                    };
                    push_field(&mut fields, ReviewField::new(label(name), value));
                }
            }
            if resource.kind == PermissionResourceKind::Command
                && !resource.attributes.contains_key("workdir")
            {
                push_field(
                    &mut fields,
                    ReviewField::new(label("Starting directory"), "Unrestricted"),
                );
            }
            if resource.protected == Some(true) {
                self.warn(PROTECTED_REVIEW);
            }
            if resource.kind == PermissionResourceKind::Command
                && !exact_selector(&resource.selector)
            {
                self.warn(SHELL_REACH);
            }
        }
        if rule.resources.is_empty() {
            fields.push(ReviewField::new(
                "Resources",
                "No restriction beyond the input above.",
            ));
        }
        let title = if matches!(rule.resources.as_slice(), [resource] if matches!(resource.selector, PermissionResourceSelector::CommandPattern { .. }))
        {
            "Command prefix".into()
        } else {
            authority_label(option)
        };
        self.authorities
            .push(AuthorityReview { title, fields, row });
    }

    pub fn bound(&mut self) -> bool {
        let mut remaining = MAX_REVIEW_CHARS;
        let mut complete = true;
        let mut bound = |text: &mut String| {
            *text = review_text(text);
            complete &= complete_text(text);
            let count = text.chars().count();
            if count > remaining {
                *text = text.chars().take(remaining).collect::<String>() + TRUNCATED;
                complete = false;
            }
            remaining = remaining.saturating_sub(count);
        };
        bound(&mut self.action);
        for field in &mut self.context {
            bound(&mut field.label);
            bound(&mut field.value);
        }
        for authority in &mut self.authorities {
            bound(&mut authority.title);
            for field in &mut authority.fields {
                bound(&mut field.label);
                bound(&mut field.value);
            }
        }
        for warning in &mut self.warnings {
            bound(warning);
        }
        if !complete {
            self.exact_call_workdir = None;
        }
        complete
    }

    #[cfg(test)]
    pub fn text(&self) -> String {
        let mut lines = vec![self.action.clone()];
        for authority in &self.authorities {
            lines.push(authority.title.clone());
            lines.extend(
                authority
                    .fields
                    .iter()
                    .map(|field| format!("{}: {}", field.label, field.value)),
            );
        }
        lines.extend(
            self.context
                .iter()
                .map(|field| format!("{}: {}", field.label, field.value)),
        );
        lines.extend(self.warnings.iter().cloned());
        lines.join("\n")
    }
}

fn exact_call_workdir<'a>(
    request: &'a PermissionRequest,
    answer: &PermissionAnswer,
) -> Option<&'a str> {
    let PermissionAnswer::AllowOption { option_id, .. } = answer else {
        return None;
    };
    let rule = &request
        .options
        .iter()
        .find(|option| option.id == *option_id)?
        .rule;
    if request.executor != PermissionExecutorKind::Native
        || !matches!(&request.subject, PermissionSubject::Native { owner, contract }
            if owner == NATIVE_SHELL_OWNER && contract == NATIVE_SHELL_CONTRACT)
        || rule.subject != request.subject
        || rule.executor != request.executor
        || rule.effect != StructuredPermissionEffect::Allow
        || rule.family.is_some()
        || !matches!(&rule.arguments, PermissionArgumentConstraint::Exact { digest }
            if *digest == request.input_digest && *digest == canonical_json_sha256(&request.input))
    {
        return None;
    }
    let command = request.input.get("command")?.as_str()?;
    let workdir = request.resources.first()?.attributes.get("workdir")?;
    if !Path::new(workdir).is_absolute()
        || !request.resources.iter().all(|resource| {
            resource.kind == PermissionResourceKind::Command
                && resource.access == Some(PermissionResourceAccess::Execute)
                && resource.value == command
                && resource.attributes.get("workdir") == Some(workdir)
                && rule
                    .resources
                    .iter()
                    .any(|constraint| exact_resource_bound(constraint, resource))
        })
        || !rule.resources.iter().all(|constraint| {
            request
                .resources
                .iter()
                .any(|resource| exact_resource_bound(constraint, resource))
        })
    {
        return None;
    }
    Some(workdir)
}

fn exact_resource_bound(
    constraint: &PermissionResourceConstraint,
    resource: &PermissionResource,
) -> bool {
    constraint.kind == resource.kind
        && constraint.access == resource.access
        && constraint.protected == Some(resource.protected)
        && matches!(
            constraint.selector,
            PermissionResourceSelector::Exact { .. } | PermissionResourceSelector::Digest { .. }
        )
        && constraint.attributes.values().all(|selector| {
            matches!(
                selector,
                PermissionResourceSelector::Exact { .. }
                    | PermissionResourceSelector::Digest { .. }
            )
        })
        && resource.attributes.keys().all(|name| {
            matches!(
                name.as_str(),
                NORMALIZED_COMMAND_ATTRIBUTE
                    | COMMAND_OBSERVATION_ATTRIBUTE
                    | COMMAND_OBSERVATION_BINDING_ATTRIBUTE
            ) || constraint.attributes.contains_key(name)
        })
        && resource_constraint_matches(constraint, resource)
}

fn same_resource_scope(
    left: &PermissionResourceConstraint,
    right: &PermissionResourceConstraint,
    request: &PermissionRequest,
) -> bool {
    left.kind == right.kind
        && left.access == right.access
        && left.protected == right.protected
        && selectors_equal(&left.selector, &right.selector, request)
        && left.attributes.len() == right.attributes.len()
        && left.attributes.iter().all(|(name, left)| {
            right
                .attributes
                .get(name)
                .is_some_and(|right| selectors_equal(left, right, request))
        })
}

fn selectors_equal(
    left: &PermissionResourceSelector,
    right: &PermissionResourceSelector,
    request: &PermissionRequest,
) -> bool {
    left == right
        || match (exact_value(left, request), exact_value(right, request)) {
            (Some(left), Some(right)) => left == right,
            _ => false,
        }
}

fn command_selector_value(
    selector: &PermissionResourceSelector,
    request: &PermissionRequest,
    fallback: &str,
) -> String {
    if let Some(value) = exact_value(selector, request) {
        return format!("Exact: {}", review_text(&value));
    }
    match selector {
        PermissionResourceSelector::Prefix { value } => format!("Prefix: {}", review_text(value)),
        PermissionResourceSelector::CommandPattern { pattern } => {
            format!("Command pattern: {}", review_text(pattern))
        }
        _ => fallback.into(),
    }
}

fn bound_directories(request: &PermissionRequest, answer: &PermissionAnswer) -> Vec<String> {
    let mut directories = Vec::new();
    let mut add_option = |id: &str| {
        if let Some(option) = request.options.iter().find(|option| option.id == id) {
            if let Some(definition) = offered_pattern(option) {
                directories.push(definition.context.effective_workdir.clone());
            }
            directories.extend(
                option
                    .rule
                    .resources
                    .iter()
                    .filter_map(|resource| resource.attributes.get("workdir"))
                    .filter_map(|selector| exact_value(selector, request)),
            );
        }
    };
    match answer {
        PermissionAnswer::AllowOption { option_id, .. } => add_option(option_id),
        PermissionAnswer::AllowComposed { rows, .. } => {
            for row in rows {
                if let Some(PermissionRowGrant::Offered(id)) = row {
                    add_option(id);
                }
            }
            for (index, row) in rows.iter().enumerate() {
                match row {
                    Some(PermissionRowGrant::Pattern { definition, .. }) => {
                        directories.push(definition.context.effective_workdir.clone())
                    }
                    Some(PermissionRowGrant::Written(_)) => {
                        if let Some(directory) = request
                            .resources
                            .get(index)
                            .and_then(|resource| resource.attributes.get("workdir"))
                        {
                            directories.push(directory.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    directories
}

fn push_field(fields: &mut Vec<ReviewField>, field: ReviewField) {
    if !fields.contains(&field) {
        fields.push(field);
    }
}

fn exact_value(
    selector: &PermissionResourceSelector,
    request: &PermissionRequest,
) -> Option<String> {
    match selector {
        PermissionResourceSelector::Exact { value } => Some(value.clone()),
        PermissionResourceSelector::Digest { digest } => request
            .resources
            .iter()
            .flat_map(|resource| {
                [&resource.value]
                    .into_iter()
                    .chain(resource.attributes.values())
            })
            .find(|value| canonical_json_sha256(&json!(value)) == *digest)
            .cloned(),
        _ => None,
    }
}

fn identity_fields(
    subject: &PermissionSubject,
    executor: &PermissionExecutorKind,
) -> Vec<ReviewField> {
    match subject {
        PermissionSubject::RemoteWorkcell { identity, .. }
        | PermissionSubject::RemoteNative { identity, .. } => {
            let mut fields = vec![ReviewField::new(
                "Binding IDs",
                "Percent-escaped authority keys; display paths are not authority.",
            )];
            for (label, value) in [
                ("Trust anchor", identity.authority.trust_anchor().as_str()),
                ("Server", identity.authority.server_id()),
                ("Workspace", identity.authority.workspace_id()),
                ("Generation", identity.authority.workspace_generation()),
                ("Namespace", identity.authority.resource_namespace_version()),
                ("Principal", identity.principal.subject()),
                ("Remote project", identity.project.key().as_str()),
            ] {
                fields.push(ReviewField::new(label, binding_component(value)));
            }
            if !remote_identity_valid(identity) {
                fields.push(ReviewField::new("Binding", REMOTE_BINDING_UNAVAILABLE));
            }
            fields
        }
        PermissionSubject::Mcp { server, .. } => vec![ReviewField::new("MCP server", server)],
        PermissionSubject::Lua { plugin, .. } => vec![ReviewField::new("Plugin", plugin)],
        PermissionSubject::UnknownLegacy { .. } => {
            vec![ReviewField::new("Executor", "Unverified legacy tool")]
        }
        PermissionSubject::Native { .. } if *executor != PermissionExecutorKind::Native => {
            vec![ReviewField::new(
                "Execution",
                "Non-local; no local project default.",
            )]
        }
        PermissionSubject::Native { .. } => Vec::new(),
    }
}

fn subject_remote_identity(subject: &PermissionSubject) -> Option<&RemotePermissionIdentity> {
    match subject {
        PermissionSubject::RemoteWorkcell { identity, .. }
        | PermissionSubject::RemoteNative { identity, .. } => Some(identity),
        _ => None,
    }
}

fn remote_identity_valid(identity: &RemotePermissionIdentity) -> bool {
    identity.authority.legacy_local_authority_id().is_none()
        && identity.principal.authority() == &identity.authority
        && identity.project.authority() == &identity.authority
}

fn binding_component(value: &str) -> String {
    let mut escaped = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.') {
            escaped.push(char::from(byte));
        } else {
            escaped.push('%');
            escaped.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
            escaped.push(char::from(HEX_DIGITS[usize::from(byte & 0xf)]));
        }
    }
    escaped
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ApprovalImpact {
    Routine,
    Review,
}

pub(super) struct ScopeSummary {
    pub label: String,
    pub lines: Vec<String>,
    pub complete: bool,
}

pub(super) fn option_impact(
    request: &PermissionRequest,
    option: &PermissionRuleOption,
) -> ApprovalImpact {
    let rule = &option.rule;
    let exact_input = matches!(
        &rule.arguments,
        PermissionArgumentConstraint::Exact { digest } if *digest == request.input_digest
    );
    if option.confirmation.is_some()
        || rule.family.is_some()
        || rule.subject != request.subject
        || rule.executor != request.executor
        || request.resources.iter().any(|resource| {
            resource.protected
                || resource.access == Some(PermissionResourceAccess::Write)
                || (matches!(
                    resource.kind,
                    PermissionResourceKind::File | PermissionResourceKind::Directory
                ) && (Path::new(&resource.value).parent().is_none()
                    || caudra_storage::paths::home()
                        .is_some_and(|home| Path::new(&resource.value) == home)))
        })
        || (rule.resources.is_empty() && !exact_input)
        || rule.resources.iter().any(|resource| {
            resource.protected == Some(true)
                || resource.access == Some(PermissionResourceAccess::Write)
                || match &resource.selector {
                    PermissionResourceSelector::CommandTemplate { definition } => {
                        pattern_impact(definition) == ApprovalImpact::Review
                    }
                    selector => !exact_selector(selector),
                }
                || resource
                    .attributes
                    .values()
                    .any(|selector| !exact_selector(selector))
                || (resource.kind == PermissionResourceKind::Command
                    && !exact_input
                    && !resource.attributes.contains_key("workdir"))
        })
    {
        ApprovalImpact::Review
    } else {
        ApprovalImpact::Routine
    }
}

fn exact_selector(selector: &PermissionResourceSelector) -> bool {
    matches!(
        selector,
        PermissionResourceSelector::Exact { .. }
            | PermissionResourceSelector::Digest { .. }
            | PermissionResourceSelector::RemoteResource { .. }
    )
}

pub(super) fn option_summary(
    request: &PermissionRequest,
    option: &PermissionRuleOption,
) -> ScopeSummary {
    if let Some(definition) = offered_pattern(option) {
        return pattern_summary(definition);
    }
    let rule = &option.rule;
    let review = review_for_rule(request, rule);
    let mut summary = ScopeSummary {
        label: authority_label(option),
        lines: Vec::new(),
        complete: review.resources.len() == rule.resources.len()
            && (matches!(rule.arguments, PermissionArgumentConstraint::Unconstrained)
                || review.input.is_some()),
    };
    if let [resource] = rule.resources.as_slice()
        && !exact_selector(&resource.selector)
        && matches!(
            resource.kind,
            PermissionResourceKind::File
                | PermissionResourceKind::Directory
                | PermissionResourceKind::Url
                | PermissionResourceKind::Query
        )
        && let Some(value) = review
            .resources
            .first()
            .and_then(|shown| shown.value.as_deref())
    {
        summary.label = format!(
            "{}: {}",
            access_label(resource.access.as_ref()),
            review_text(value)
        );
    }
    summary.lines.push(match rule.family {
        Some(PermissionCapabilityFamily::FilesystemBrowse) => {
            "Trusted names-only directory browsing; not file contents, writes or shell execution."
                .into()
        }
        Some(PermissionCapabilityFamily::FilesystemRead) => {
            "Trusted file reading, listing and search; not writes or shell execution.".into()
        }
        Some(PermissionCapabilityFamily::McpServer) => "All tools on the bound MCP server.".into(),
        None => format!("Tool: {}", review_text(&request.tool.to_string())),
    });
    summary.lines.push(match rule.arguments {
        PermissionArgumentConstraint::Exact { .. } => "Only this exact input and context.".into(),
        PermissionArgumentConstraint::Selected { .. }
        | PermissionArgumentConstraint::SelectedDigest { .. } => {
            "Selected input fields stay fixed; other arguments may vary. See Details.".into()
        }
        PermissionArgumentConstraint::Unconstrained => "Other tool arguments may vary.".into(),
    });
    for (resource, shown) in rule.resources.iter().zip(&review.resources) {
        let scope = match &resource.selector {
            PermissionResourceSelector::RemoteResource { .. } => {
                Some("Exact authority-issued resource".to_owned())
            }
            PermissionResourceSelector::RemoteSubtree { .. } => {
                Some("Authority-issued resource and descendants".to_owned())
            }
            _ => shown.value.clone(),
        };
        if let Some(scope) = scope {
            summary.lines.push(format!(
                "{}: {}",
                access_label(resource.access.as_ref()),
                review_text(&scope)
            ));
        } else {
            summary.complete = false;
        }
        if resource.protected == Some(true) {
            summary.lines.push("Includes protected resources.".into());
        }
        if resource.attributes.contains_key("workdir") {
            if let Some(workdir) = shown.attributes.get("workdir") {
                summary
                    .lines
                    .push(format!("Starting directory: {}", review_text(workdir)));
            } else {
                summary.complete = false;
            }
        } else if resource.kind == PermissionResourceKind::Command {
            summary
                .lines
                .push("Starting directory: unrestricted.".into());
        }
        if let Some(workdirs) = shown.attributes.get(POSSIBLE_WORKDIRS_ATTRIBUTE) {
            summary.lines.push(review_text(workdirs));
        }
        summary.complete &= shown.attributes.len() == resource.attributes.len()
            && !shown
                .attributes
                .values()
                .any(|value| value.contains("unavailable"));
    }
    if rule.resources.is_empty() {
        summary
            .lines
            .push("No resource restriction beyond the input above.".into());
    }
    if rule.resources.iter().any(|resource| {
        resource.kind == PermissionResourceKind::Command && !exact_selector(&resource.selector)
    }) {
        summary.lines.push(SHELL_REACH.into());
    }
    summary.complete &= summary.lines.iter().all(|line| complete_text(line));
    if !summary.complete {
        summary.lines.push(MISSING_SCOPE.into());
    }
    summary
}

fn authority_label(option: &PermissionRuleOption) -> String {
    let rule = &option.rule;
    if rule.family == Some(PermissionCapabilityFamily::FilesystemBrowse) {
        return option.label.clone();
    }
    if rule.family == Some(PermissionCapabilityFamily::FilesystemRead) {
        return "Read, list and search selected roots".into();
    }
    if rule.family == Some(PermissionCapabilityFamily::McpServer) {
        return "All tools on this MCP server".into();
    }
    if matches!(rule.arguments, PermissionArgumentConstraint::Exact { .. }) {
        return "This exact call".into();
    }
    if let [resource] = rule.resources.as_slice()
        && let PermissionResourceSelector::CommandPattern { pattern } = &resource.selector
    {
        return format!("Command prefix: {}", review_text(pattern));
    }
    if rule
        .resources
        .iter()
        .all(|resource| exact_selector(&resource.selector))
        && let Some(resource) = rule.resources.first()
    {
        return match resource.kind {
            PermissionResourceKind::Command => "Exact commands in their workdirs",
            PermissionResourceKind::File | PermissionResourceKind::Directory => "These exact paths",
            PermissionResourceKind::Url => "This exact URL",
            PermissionResourceKind::Query => "This exact query",
            PermissionResourceKind::RemoteFile { .. }
            | PermissionResourceKind::RemoteDirectory { .. }
            | PermissionResourceKind::RemoteResource { .. } => "These exact remote resources",
            PermissionResourceKind::Custom { .. } => "These exact resources",
        }
        .into();
    }
    if rule
        .resources
        .iter()
        .any(|resource| resource.kind == PermissionResourceKind::Command)
    {
        return if rule
            .resources
            .iter()
            .all(|resource| resource.attributes.contains_key("workdir"))
        {
            "Commands in selected starting directories"
        } else {
            "Any shell command"
        }
        .into();
    }
    "Selected resource scope".into()
}

pub(super) fn access_label(access: Option<&PermissionResourceAccess>) -> &'static str {
    match access {
        Some(PermissionResourceAccess::Read) => "Read",
        Some(PermissionResourceAccess::List) => "List names",
        Some(PermissionResourceAccess::Write) => "Change",
        Some(PermissionResourceAccess::Execute) => "Execute",
        Some(PermissionResourceAccess::Search) => "Search",
        Some(PermissionResourceAccess::Connect) => "Connect",
        None => "Resource",
    }
}

pub(super) fn policy_reason(request: &PermissionRequest) -> String {
    let reason = request.presentation.risk_summary.trim();
    review_text(if reason.is_empty() || GENERIC_REASONS.contains(&reason) {
        NO_POLICY_REASON
    } else {
        reason
    })
}

pub(super) fn requested_action(request: &PermissionRequest) -> String {
    let shell = request
        .resources
        .iter()
        .any(|resource| resource.kind == PermissionResourceKind::Command);
    if shell && let Some(command) = requested_command(request) {
        return review_text(command);
    }
    if shell {
        return match request.resources.as_slice() {
            [resource] => review_text(&resource.value),
            _ => "Complete shell source unavailable; inspect Details.".into(),
        };
    }
    review_text(&request.presentation.action)
}

fn requested_command(request: &PermissionRequest) -> Option<&str> {
    ["command", "command_text"]
        .iter()
        .find_map(|key| request.input.get(key).and_then(|value| value.as_str()))
        .or_else(|| match request.resources.as_slice() {
            [resource] if resource.kind == PermissionResourceKind::Command => {
                Some(resource.value.as_str())
            }
            _ => None,
        })
}

pub(super) fn identity_lines(request: &PermissionRequest) -> Vec<String> {
    identity_fields(&request.subject, &request.executor)
        .into_iter()
        .map(|field| format!("{}: {}", field.label, field.value))
        .collect()
}

pub(super) fn complete_text(text: &str) -> bool {
    !text.contains(TRUNCATED) && !text.contains(INCOMPLETE_REDACTION) && !text.contains("[omitted:")
}

#[cfg(test)]
mod tests {
    use caudra_agent::permissions::{PermissionResourceAccess, PermissionResourceSelector};
    use test_case::test_case;

    use super::super::view::tests::open_prompt;
    use super::{ApprovalImpact, GENERIC_REASONS, NO_POLICY_REASON, option_impact, policy_reason};

    #[test_case(false; "narrow_flag")]
    #[test_case(true; "broad_flag")]
    fn exact_command_impact_uses_the_rule_not_the_option_flag(broad: bool) {
        let prompt = open_prompt();
        let request = prompt.current().unwrap();
        let mut option = request
            .options
            .iter()
            .find(|option| option.id == "command_exact_0")
            .unwrap()
            .clone();
        option.broad = broad;
        assert_eq!(option_impact(request, &option), ApprovalImpact::Routine);
        option.rule.resources[0].selector = PermissionResourceSelector::Any;
        assert_eq!(option_impact(request, &option), ApprovalImpact::Review);
    }

    #[test_case("write"; "write")]
    #[test_case("protected"; "protected")]
    #[test_case("phrase"; "required_phrase")]
    #[test_case("prefix"; "custom_prefix")]
    #[test_case("unbound"; "missing_workdir")]
    fn semantic_expansion_requires_review_even_with_narrow_flag(expansion: &str) {
        let prompt = open_prompt();
        let mut request = prompt.current().unwrap().clone();
        let mut option = request
            .options
            .iter()
            .find(|option| option.id == "command_exact_0")
            .unwrap()
            .clone();
        option.broad = false;
        match expansion {
            "write" => option.rule.resources[0].access = Some(PermissionResourceAccess::Write),
            "protected" => request.resources[0].protected = true,
            "phrase" => option.confirmation = Some("REVIEW".into()),
            "prefix" => {
                option.rule.resources[0].selector = PermissionResourceSelector::CommandPattern {
                    pattern: "cargo *".into(),
                }
            }
            "unbound" => option.rule.resources[0].attributes.clear(),
            _ => unreachable!(),
        }
        assert_eq!(option_impact(&request, &option), ApprovalImpact::Review);
    }

    #[test]
    fn generic_risk_text_never_becomes_an_inferred_policy_reason() {
        const SPECIFIC: &str = "The home-relative operand is unresolved";
        let prompt = open_prompt();
        let mut request = prompt.current().unwrap().clone();
        for generic in GENERIC_REASONS {
            request.presentation.risk_summary = (*generic).into();
            assert_eq!(policy_reason(&request), NO_POLICY_REASON);
        }
        request.presentation.risk_summary = SPECIFIC.into();
        assert_eq!(policy_reason(&request), SPECIFIC);
    }
}
