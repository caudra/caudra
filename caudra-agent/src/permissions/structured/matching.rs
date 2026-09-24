use super::{
    BROWSE_DIRECT, BROWSE_RECURSION_ATTRIBUTE, BROWSE_RECURSIVE, CONFINED_READ_ATTRIBUTE,
    CONFINED_READ_AUTHORITY, FILESYSTEM_BROWSE_CONTRACTS, FILESYSTEM_READ_CONTRACTS,
    PermissionCapabilityFamily, PermissionRequest, PermissionResource, PermissionResourceAccess,
    PermissionResourceConstraint, PermissionResourceKind, PermissionResourceSelector,
    PermissionSubject, PolicyRule, RemotePermissionIdentity, ResourceCoverage, ResourceStanding,
    StructuredPermissionDecision, StructuredPermissionEffect, StructuredPermissionRule,
    WORKCELL_OWNER, argument_constraint_matches, attribute_kind, canonical_json_sha256,
    remote_resource_identity, safe_summary,
};
use super::{
    COMMAND_OBSERVATION_ATTRIBUTE, NORMALIZED_COMMAND_ATTRIBUTE, PermissionExecutorKind,
    WORKDIR_ATTRIBUTE,
};
use super::{
    COMMAND_OBSERVATION_BINDING_ATTRIBUTE, POSSIBLE_WORKDIRS_ATTRIBUTE, prepared_command_binding,
    resources::PreparedWorkdirs,
};
use crate::permissions::{
    pattern_matching::CompiledPattern,
    pattern_recognition::{CommandObservation, InvocationOutcome, ObservationProvenance},
    policy::SHELL_EXECUTION_CONTRACT,
};
use caudra_config::ToolKey;
use caudra_storage::permission_state::{
    filesystem_browse_recursion, validate_command_templates, validate_filesystem_browse,
};
use serde_json::Value;
use std::cmp::Reverse;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use url::Url;

pub fn resource_constraint_matches(
    constraint: &PermissionResourceConstraint,
    resource: &PermissionResource,
) -> bool {
    constraint_covers_resource(constraint, resource, None, RuleIntent::Grant, None)
}

/// What a rule does with a resource it names. Protection raises the bar for
/// granting only: a refusal that had to clear the same bar would fail open on
/// exactly the resources protection exists for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RuleIntent {
    Grant,
    Restrain,
}

impl RuleIntent {
    pub(super) fn of(effect: &StructuredPermissionEffect) -> Self {
        match effect {
            StructuredPermissionEffect::Allow => Self::Grant,
            StructuredPermissionEffect::Ask | StructuredPermissionEffect::Deny => Self::Restrain,
        }
    }
}

/// Whether the constraint and the resource name the same operation. Without a
/// family this is exact equality on kind and access; a family instead accepts
/// any member pair, which is what lets one subtree grant serve read, list, and
/// search without ever reaching a write.
pub(super) fn operation_matches(
    constraint: &PermissionResourceConstraint,
    resource: &PermissionResource,
    family: Option<PermissionCapabilityFamily>,
) -> bool {
    match family {
        Some(PermissionCapabilityFamily::FilesystemRead) => {
            is_filesystem_read_kind(&constraint.kind)
                && is_filesystem_read_kind(&resource.kind)
                && is_filesystem_read_access(constraint.access.as_ref())
                && (is_filesystem_read_access(resource.access.as_ref())
                    || resource.access == Some(PermissionResourceAccess::List))
        }
        Some(PermissionCapabilityFamily::FilesystemBrowse) => {
            constraint.kind == PermissionResourceKind::Directory
                && resource.kind == PermissionResourceKind::Directory
                && constraint.access == Some(PermissionResourceAccess::List)
                && resource.access == Some(PermissionResourceAccess::List)
                && constraint
                    .attributes
                    .get(BROWSE_RECURSION_ATTRIBUTE)
                    .and_then(filesystem_browse_recursion)
                    .zip(resource.attributes.get(BROWSE_RECURSION_ATTRIBUTE))
                    .is_some_and(|(expected, actual)| {
                        actual == BROWSE_DIRECT
                            || (expected == BROWSE_RECURSIVE && actual == BROWSE_RECURSIVE)
                    })
        }
        // Widens which subject a rule reaches, never which operation, so the
        // constraint still has to name the operation exactly.
        Some(PermissionCapabilityFamily::McpServer) | None => {
            (constraint.kind == resource.kind
                || matches!(
                    (&constraint.kind, &resource.kind),
                    (
                        PermissionResourceKind::RemoteDirectory { identity: directory },
                        PermissionResourceKind::RemoteFile { identity: descendant }
                            | PermissionResourceKind::RemoteDirectory { identity: descendant }
                    ) if directory == descendant
                ))
                && !constraint
                    .access
                    .as_ref()
                    .is_some_and(|access| resource.access.as_ref() != Some(access))
        }
    }
}

pub(super) fn is_filesystem_read_kind(kind: &PermissionResourceKind) -> bool {
    matches!(
        kind,
        PermissionResourceKind::File | PermissionResourceKind::Directory
    )
}

/// An absent access means "any access" on a constraint, which would reach
/// `Write`, so only an explicit read-shaped access joins the family.
pub(super) fn is_filesystem_read_access(access: Option<&PermissionResourceAccess>) -> bool {
    matches!(
        access,
        Some(PermissionResourceAccess::Read) | Some(PermissionResourceAccess::Search)
    )
}

pub(super) fn constraint_covers_resource(
    constraint: &PermissionResourceConstraint,
    resource: &PermissionResource,
    family: Option<PermissionCapabilityFamily>,
    intent: RuleIntent,
    request: Option<&PermissionRequest>,
) -> bool {
    if !operation_matches(constraint, resource, family) {
        return false;
    }
    if constraint
        .protected
        .is_some_and(|protected| resource.protected != protected)
    {
        return false;
    }
    if resource.protected
        && intent == RuleIntent::Grant
        && !protected_coverage_allowed(constraint, resource)
    {
        return false;
    }
    let matches_selector = match &constraint.selector {
        PermissionResourceSelector::CommandTemplate { definition } => request
            .and_then(|request| trusted_command_observation(request, resource))
            .is_some_and(|observation| {
                CompiledPattern::compile(definition)
                    .ok()
                    .and_then(|compiled| compiled.matches(&observation).ok())
                    .is_some_and(|report| report.is_match())
            }),
        selector => selector_matches(selector, &resource.value, &resource.kind),
    };
    if !matches_selector {
        return false;
    }
    constraint.attributes.iter().all(|(name, selector)| {
        if family == Some(PermissionCapabilityFamily::FilesystemBrowse)
            && name == BROWSE_RECURSION_ATTRIBUTE
        {
            return true;
        }
        resource
            .attributes
            .get(name)
            .is_some_and(|value| selector_matches(selector, value, &attribute_kind(name)))
    })
}

pub(in crate::permissions) fn trusted_command_observation(
    request: &PermissionRequest,
    resource: &PermissionResource,
) -> Option<CommandObservation> {
    if !matches!(&request.tool, ToolKey::Native(name) if name.as_ref() == "shell")
        || request.executor != PermissionExecutorKind::Native
        || !matches!(&request.subject, PermissionSubject::Native { owner, contract }
            if owner == WORKCELL_OWNER && contract == SHELL_EXECUTION_CONTRACT)
        || resource.kind != PermissionResourceKind::Command
        || resource.access != Some(PermissionResourceAccess::Execute)
        || resource.protected
        || resource.requires_prompt
    {
        return None;
    }
    let observation =
        CommandObservation::from_json(resource.attributes.get(COMMAND_OBSERVATION_ATTRIBUTE)?)
            .ok()?;
    let binding = prepared_command_binding(&resource.value, &request.input);
    if resource
        .attributes
        .get(COMMAND_OBSERVATION_BINDING_ATTRIBUTE)
        != Some(&binding)
        || observation.source.input_hash != binding
        || request
            .input
            .get("command")
            .and_then(Value::as_str)
            .is_none()
    {
        return None;
    }
    if let Some(possible) = resource.attributes.get(POSSIBLE_WORKDIRS_ATTRIBUTE)
        && !matches!(PreparedWorkdirs::parse(possible), Some(PreparedWorkdirs::Known(paths))
            if paths.as_slice() == [observation.context.effective_workdir.as_str()])
    {
        return None;
    }
    if observation.source.provenance != ObservationProvenance::Native
        || observation.source.outcome != InvocationOutcome::Requested
        || resource.attributes.get(WORKDIR_ATTRIBUTE)
            != Some(&observation.context.effective_workdir)
        || normalized_filesystem_path(&observation.context.effective_workdir).is_none()
    {
        return None;
    }
    Some(observation)
}

/// Reports whether a constraint is specific enough to cover a protected resource.
///
/// Protected resources normally demand an exact or digest selector with every
/// attribute pinned the same way. Protected commands additionally accept the
/// blanket `Any` selector, which reaches a saved rule only through a typed
/// broad shell confirmation, so redirects and heredocs stop prompting once the
/// user grants arbitrary command execution. Command patterns stay excluded
/// because the reviewed text of a protected command describes more than the
/// pattern does.
///
/// A remote resource is pinned by the scope its authority issued, and its
/// attributes only describe it for display, so an exact remote selector clears
/// the bar on its own. A remote subtree never does. Every mutating remote call
/// is protected, so without this its own exact option could not be accepted.
pub(super) fn protected_coverage_allowed(
    constraint: &PermissionResourceConstraint,
    resource: &PermissionResource,
) -> bool {
    if resource.kind == PermissionResourceKind::Command
        && matches!(constraint.selector, PermissionResourceSelector::Any)
    {
        return true;
    }
    if remote_resource_identity(&resource.kind).is_some()
        && constraint.protected == Some(true)
        && matches!(
            constraint.selector,
            PermissionResourceSelector::RemoteResource { .. }
        )
    {
        return true;
    }
    constraint.protected == Some(true)
        && matches!(
            constraint.selector,
            PermissionResourceSelector::Exact { .. } | PermissionResourceSelector::Digest { .. }
        )
        && constraint.attributes.len()
            == resource
                .attributes
                .keys()
                .filter(|name| {
                    !matches!(
                        name.as_str(),
                        COMMAND_OBSERVATION_ATTRIBUTE
                            | COMMAND_OBSERVATION_BINDING_ATTRIBUTE
                            | NORMALIZED_COMMAND_ATTRIBUTE
                    )
                })
                .count()
        && constraint.attributes.values().all(|selector| {
            matches!(
                selector,
                PermissionResourceSelector::Exact { .. }
                    | PermissionResourceSelector::Digest { .. }
            )
        })
}

/// How narrowly a selector names a resource. A rule that names one resource
/// outranks one that names a region, which outranks one that names everything,
/// so a specific grant is not swallowed by a broad ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum SelectorWidth {
    Blanket,
    /// How much literal text the selector pins, as a token count and their total
    /// byte length, so `git status *` outranks `git *`. A subtree pins a region
    /// by structure rather than by text and so pins none.
    Region(usize, usize),
    Exact,
}

pub(super) fn selector_width(selector: &PermissionResourceSelector) -> SelectorWidth {
    match selector {
        PermissionResourceSelector::Any | PermissionResourceSelector::CommandTemplate { .. } => {
            SelectorWidth::Blanket
        }
        // The grammar knows to leave the trailing wildcard out of the count,
        // which a plain tokenization would include.
        PermissionResourceSelector::CommandPattern { pattern } => {
            match crate::permissions::command_pattern::specificity(pattern) {
                Some((tokens, bytes)) => pinned_text_width(tokens, bytes),
                None => SelectorWidth::Blanket,
            }
        }
        PermissionResourceSelector::Prefix { value } => pinned_text_width(
            value.split_whitespace().count(),
            value.split_whitespace().map(str::len).sum(),
        ),
        PermissionResourceSelector::Subtree { .. }
        | PermissionResourceSelector::FilesystemSubtreeDigest { .. }
        | PermissionResourceSelector::UrlSubtreeDigest { .. }
        | PermissionResourceSelector::UrlOriginDigest { .. }
        | PermissionResourceSelector::RemoteSubtree { .. } => SelectorWidth::Region(0, 0),
        PermissionResourceSelector::Exact { .. }
        | PermissionResourceSelector::Digest { .. }
        | PermissionResourceSelector::RemoteResource { .. } => SelectorWidth::Exact,
    }
}

/// A selector pinning no text reaches everything its kind has, so it ranks with
/// the blanket selector rather than above it. Prefixes and command patterns are
/// measured the same way, because a configured scope becomes one or the other
/// purely by its spelling and the two must rank against each other honestly.
pub(super) fn pinned_text_width(tokens: usize, bytes: usize) -> SelectorWidth {
    if tokens == 0 {
        SelectorWidth::Blanket
    } else {
        SelectorWidth::Region(tokens, bytes)
    }
}

/// Where a rule stands relative to the others once it has matched. Ordered
/// lexicographically: a denial is absolute and outranks any width, then the rule
/// that names the least wins, and an ask breaks a tie against an allow.
pub(super) type RuleStanding = (bool, SelectorWidth, StructuredPermissionDecision);

/// Reports how strongly a rule speaks to a resource, or `None` when it does not
/// speak to it at all. Keeping the traversal separate from the effect is what
/// lets one definition of matching answer for allow, deny, and ask alike.
///
/// A rule that names no resource is unrestricted, which is how the picker
/// presents it and how deny already reads it, so it speaks to every resource at
/// the widest rank.
pub(super) fn rule_standing(
    rule: &StructuredPermissionRule,
    request: &PermissionRequest,
    resource: &PermissionResource,
) -> Option<RuleStanding> {
    rule_reach(rule, request, resource).map(|(standing, _)| standing)
}

/// `rule_standing` with the constraint that carried the standing, so a caller
/// that has to name the authority names the one that actually decided. A rule
/// naming no resource is unrestricted and so carries no constraint.
pub(super) fn rule_reach<'a>(
    rule: &'a StructuredPermissionRule,
    request: &PermissionRequest,
    resource: &PermissionResource,
) -> Option<(RuleStanding, Option<&'a PermissionResourceConstraint>)> {
    if !rule_context_matches(rule, request) {
        return None;
    }
    let decision = StructuredPermissionDecision::of(&rule.effect);
    let standing = |width| {
        (
            decision == StructuredPermissionDecision::Deny,
            width,
            decision,
        )
    };
    if rule.resources.is_empty() {
        return Some((standing(SelectorWidth::Blanket), None));
    }
    rule.resources
        .iter()
        .filter(|constraint| {
            constraint_covers_resource(
                constraint,
                resource,
                rule.family,
                RuleIntent::of(&rule.effect),
                Some(request),
            )
        })
        .max_by_key(|constraint| selector_width(&constraint.selector))
        .map(|constraint| {
            (
                standing(selector_width(&constraint.selector)),
                Some(constraint),
            )
        })
}

/// What the rule set says about one resource, and the allow that covers it.
///
/// Ranking is what keeps a config that asks about `git *` from swallowing its
/// own `git status` allow. Ties are broken entirely by the standing, so the
/// answer does not depend on rule order. The credited allow is ranked the same
/// way, so the authority a prompt names is the one that would decide.
pub fn permission_rules_resource_standing(
    rules: &[PolicyRule],
    request: &PermissionRequest,
    resource: &PermissionResource,
) -> ResourceStanding {
    let decision = resource_decision(rules.iter().map(|policy| &policy.rule), request, resource);
    // `min_by_key` over the reversed width keeps the first rule of equal reach,
    // so a grant the user made outranks configured policy that says the same.
    let coverage = rules
        .iter()
        .filter(|policy| policy.rule.effect == StructuredPermissionEffect::Allow)
        .filter_map(|policy| {
            rule_reach(&policy.rule, request, resource)
                .map(|((_, width, _), constraint)| (policy, width, constraint))
        })
        .min_by_key(|(_, width, _)| Reverse(*width))
        .map(|(policy, _, constraint)| ResourceCoverage {
            origin: policy.origin,
            authority: constraint.map_or_else(
                || blanket_authority(&resource.kind),
                |constraint| constraint_authority(constraint, &resource.kind),
            ),
        });
    ResourceStanding { decision, coverage }
}

pub(super) fn resource_decision<'a>(
    rules: impl IntoIterator<Item = &'a StructuredPermissionRule>,
    request: &PermissionRequest,
    resource: &PermissionResource,
) -> StructuredPermissionDecision {
    rules
        .into_iter()
        .filter_map(|rule| rule_standing(rule, request, resource))
        .max()
        .map_or(StructuredPermissionDecision::NoMatch, |(_, _, decision)| {
            decision
        })
}

/// How a covering constraint names a resource, for a prompt that has to say why
/// the resource is already allowed.
///
/// An attribute narrows a constraint past whatever its selector says, so the
/// selector alone would describe the confined-read rule as reaching any command
/// when it only reaches a command the shell tool already judged.
pub(super) fn constraint_authority(
    constraint: &PermissionResourceConstraint,
    kind: &PermissionResourceKind,
) -> String {
    if constraint.attributes.contains_key(CONFINED_READ_ATTRIBUTE) {
        return CONFINED_READ_AUTHORITY.into();
    }
    selector_authority(&constraint.selector, kind)
}

pub(super) fn selector_authority(
    selector: &PermissionResourceSelector,
    kind: &PermissionResourceKind,
) -> String {
    match selector {
        PermissionResourceSelector::CommandTemplate { definition } => {
            super::review::command_template_label(definition)
        }
        PermissionResourceSelector::CommandPattern { pattern } => safe_summary(pattern),
        PermissionResourceSelector::Prefix { value } => safe_summary(value),
        PermissionResourceSelector::Subtree { root } => safe_summary(root),
        PermissionResourceSelector::Exact { .. } | PermissionResourceSelector::Digest { .. } => {
            format!("this {}", kind_noun(kind))
        }
        PermissionResourceSelector::FilesystemSubtreeDigest { .. }
        | PermissionResourceSelector::UrlSubtreeDigest { .. }
        | PermissionResourceSelector::UrlOriginDigest { .. }
        | PermissionResourceSelector::RemoteSubtree { .. } => {
            format!("this {} tree", kind_noun(kind))
        }
        PermissionResourceSelector::RemoteResource { .. } => format!("this {}", kind_noun(kind)),
        PermissionResourceSelector::Any => blanket_authority(kind),
    }
}

pub(super) fn blanket_authority(kind: &PermissionResourceKind) -> String {
    format!("any {}", kind_noun(kind))
}

pub(super) fn kind_noun(kind: &PermissionResourceKind) -> String {
    match kind {
        PermissionResourceKind::Custom { name } => safe_summary(name),
        PermissionResourceKind::RemoteFile { .. } => "remote file".into(),
        PermissionResourceKind::RemoteDirectory { .. } => "remote directory".into(),
        PermissionResourceKind::RemoteResource { resource_kind, .. } => {
            format!("remote {}", safe_summary(resource_kind))
        }
        other => format!("{other:?}").to_lowercase(),
    }
}

pub fn permission_rule_covers_request(
    rule: &StructuredPermissionRule,
    request: &PermissionRequest,
) -> bool {
    rule.effect == StructuredPermissionEffect::Allow
        && rule_context_matches(rule, request)
        && request.resources.iter().all(|resource| {
            rule.resources.iter().any(|constraint| {
                constraint_covers_resource(
                    constraint,
                    resource,
                    rule.family,
                    RuleIntent::of(&rule.effect),
                    Some(request),
                )
            })
        })
}

pub fn permission_rule_covers_resource(
    rule: &StructuredPermissionRule,
    request: &PermissionRequest,
    resource: &PermissionResource,
) -> bool {
    rule.effect == StructuredPermissionEffect::Allow
        && rule_standing(rule, request, resource).is_some()
}

pub fn permission_rules_cover_request(
    rules: &[StructuredPermissionRule],
    request: &PermissionRequest,
) -> bool {
    rules.iter().any(|rule| {
        rule.effect == StructuredPermissionEffect::Allow && rule_context_matches(rule, request)
    }) && request.resources.iter().all(|resource| {
        rules
            .iter()
            .any(|rule| permission_rule_covers_resource(rule, request, resource))
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
                rule.resources.iter().any(|constraint| {
                    constraint_covers_resource(
                        constraint,
                        resource,
                        rule.family,
                        RuleIntent::of(&rule.effect),
                        Some(request),
                    )
                })
            }))
}

/// What the rule set says about a whole request.
///
/// Deny and ask propagate from any one resource, because narrowing anywhere
/// narrows the call. Allow does not: it needs every resource covered, since a
/// call is only authorized when nothing it touches is left unspoken for.
pub fn evaluate_structured_permission_rules(
    rules: &[StructuredPermissionRule],
    request: &PermissionRequest,
) -> StructuredPermissionDecision {
    let narrowed = request
        .resources
        .iter()
        .map(|resource| resource_decision(rules, request, resource))
        .fold(
            StructuredPermissionDecision::NoMatch,
            |decision, resource| decision.merge(resource),
        );
    match narrowed {
        StructuredPermissionDecision::Deny | StructuredPermissionDecision::Ask => narrowed,
        StructuredPermissionDecision::Allow | StructuredPermissionDecision::NoMatch => {
            if permission_rules_cover_request(rules, request) {
                StructuredPermissionDecision::Allow
            } else {
                StructuredPermissionDecision::NoMatch
            }
        }
    }
}

pub(super) fn rule_context_matches(
    rule: &StructuredPermissionRule,
    request: &PermissionRequest,
) -> bool {
    subject_matches(rule, request)
        && rule.executor == request.executor
        && (rule.family != Some(PermissionCapabilityFamily::FilesystemBrowse)
            || validate_filesystem_browse(&rule.subject, &rule.executor, &rule.resources).is_ok())
        && validate_command_templates(rule).is_ok()
        && argument_constraint_matches(&rule.arguments, &request.input)
}

/// Authority is keyed to the subject, which for a first-party tool is a single
/// contract. A family rule is instead keyed to the trust domain, so the contract
/// that happened to ask first stops being part of the key. Both sides must be
/// members, which pins the owner too, since membership names one owner.
pub(super) fn subject_matches(
    rule: &StructuredPermissionRule,
    request: &PermissionRequest,
) -> bool {
    if rule.subject == request.subject
        && rule.family != Some(PermissionCapabilityFamily::FilesystemBrowse)
    {
        return true;
    }
    match rule.family {
        Some(PermissionCapabilityFamily::FilesystemBrowse) => {
            is_filesystem_browse_subject(&rule.subject)
                && is_filesystem_browse_subject(&request.subject)
        }
        Some(PermissionCapabilityFamily::FilesystemRead) => {
            is_filesystem_read_subject(&rule.subject)
                && is_filesystem_read_subject(&request.subject)
        }
        Some(PermissionCapabilityFamily::McpServer) => mcp_server(&rule.subject)
            .zip(mcp_server(&request.subject))
            .is_some_and(|(rule, request)| rule == request),
        None => false,
    }
}

/// A server-wide rule is keyed to the server, so the tool that happened to ask
/// first stops being part of the key. An empty name is never a key: it is what
/// a record written before servers were recorded deserializes to, and matching
/// on it would let one such rule reach every server.
pub(super) fn mcp_server(subject: &PermissionSubject) -> Option<&str> {
    match subject {
        PermissionSubject::Mcp { server, .. } if !server.is_empty() => Some(server),
        PermissionSubject::Mcp { .. }
        | PermissionSubject::RemoteWorkcell { .. }
        | PermissionSubject::RemoteNative { .. }
        | PermissionSubject::Native { .. }
        | PermissionSubject::Lua { .. }
        | PermissionSubject::UnknownLegacy { .. } => None,
    }
}

pub(super) fn is_filesystem_read_subject(subject: &PermissionSubject) -> bool {
    match subject {
        PermissionSubject::Native { owner, contract } => {
            owner == WORKCELL_OWNER && FILESYSTEM_READ_CONTRACTS.contains(&contract.as_str())
        }
        PermissionSubject::Lua { .. }
        | PermissionSubject::Mcp { .. }
        | PermissionSubject::RemoteWorkcell { .. }
        | PermissionSubject::RemoteNative { .. }
        | PermissionSubject::UnknownLegacy { .. } => false,
    }
}

pub(super) fn is_filesystem_browse_subject(subject: &PermissionSubject) -> bool {
    matches!(subject, PermissionSubject::Native { owner, contract }
        if owner == WORKCELL_OWNER && FILESYSTEM_BROWSE_CONTRACTS.contains(&contract.as_str()))
}

pub(super) fn selector_matches(
    selector: &PermissionResourceSelector,
    value: &str,
    kind: &PermissionResourceKind,
) -> bool {
    match selector {
        PermissionResourceSelector::CommandTemplate { .. } => false,
        PermissionResourceSelector::Any => true,
        PermissionResourceSelector::Digest { digest } => {
            resource_value_digest(value, kind).is_some_and(|actual| actual == *digest)
        }
        PermissionResourceSelector::FilesystemSubtreeDigest { digest } => {
            matches!(
                kind,
                PermissionResourceKind::File | PermissionResourceKind::Directory
            ) && filesystem_ancestor_digests(value).is_some_and(|digests| digests.contains(digest))
        }
        PermissionResourceSelector::UrlSubtreeDigest { digest } => {
            matches!(kind, PermissionResourceKind::Url)
                && url_subtree_digests(value).is_some_and(|digests| digests.contains(digest))
        }
        PermissionResourceSelector::UrlOriginDigest { digest } => {
            matches!(kind, PermissionResourceKind::Url)
                && url_origin_digest(value).is_some_and(|actual| actual == *digest)
        }
        PermissionResourceSelector::CommandPattern { pattern } => {
            matches!(kind, PermissionResourceKind::Command)
                && crate::permissions::command_pattern::matches(pattern, value)
        }
        PermissionResourceSelector::RemoteResource { identity, scope } => {
            remote_resource_matches(kind, value, identity, scope, false)
        }
        PermissionResourceSelector::RemoteSubtree { identity, scope } => {
            remote_resource_matches(kind, value, identity, scope, true)
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
        // Raw text matched raw, on every kind, exactly as a configured scope
        // ending in a bare `*` always was. Normalizing either side would change
        // which existing configs match, and a deny is among them.
        PermissionResourceSelector::Prefix { value: prefix } => value.starts_with(prefix),
    }
}

pub(super) fn resource_value_digest(value: &str, kind: &PermissionResourceKind) -> Option<String> {
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

pub(super) fn remote_resource_matches(
    kind: &PermissionResourceKind,
    value: &str,
    identity: &RemotePermissionIdentity,
    scope: &[String],
    descendants: bool,
) -> bool {
    let kind_matches = match kind {
        PermissionResourceKind::RemoteFile { identity: actual }
        | PermissionResourceKind::RemoteDirectory { identity: actual }
        | PermissionResourceKind::RemoteResource {
            identity: actual, ..
        } => actual == identity,
        _ => false,
    };
    let Some(actual_scope) = remote_scope(value) else {
        return false;
    };
    kind_matches
        && if descendants {
            actual_scope.starts_with(scope)
        } else {
            actual_scope == scope
        }
}

pub(super) fn remote_scope(value: &str) -> Option<Vec<String>> {
    let scope = value.split('\u{1f}').map(str::to_owned).collect::<Vec<_>>();
    (!scope.is_empty()
        && scope.iter().all(|part| !part.is_empty())
        && scope.iter().collect::<HashSet<_>>().len() == scope.len())
    .then_some(scope)
}

pub(super) fn scoped_digest(domain: &str, value: &str) -> String {
    canonical_json_sha256(&serde_json::json!([domain, value]))
}

pub(super) fn filesystem_subtree_digest(value: &str) -> Option<String> {
    let value = normalized_filesystem_path(value)?;
    Some(scoped_digest(
        "filesystem_subtree",
        &value.to_string_lossy(),
    ))
}

pub(super) fn filesystem_ancestor_digests(value: &str) -> Option<HashSet<String>> {
    let value = normalized_filesystem_path(value)?;
    Some(
        value
            .ancestors()
            .map(|ancestor| scoped_digest("filesystem_subtree", &ancestor.to_string_lossy()))
            .collect(),
    )
}

pub(super) fn url_subtree_digest(root: &str) -> String {
    scoped_digest("url_subtree", root)
}

pub(super) fn url_subtree_digests(value: &str) -> Option<HashSet<String>> {
    Some(
        url_subtree_roots(&strict_http_url(value)?)?
            .iter()
            .map(|root| url_subtree_digest(root))
            .collect(),
    )
}

pub(super) fn url_origin_digest(value: &str) -> Option<String> {
    let strict = strict_http_url(value)?;
    Some(scoped_digest(
        "url_origin",
        &strict.url.origin().ascii_serialization(),
    ))
}

/// Every prefix of a URL's path as a subtree root, deepest first and ending at
/// the origin's own root. One construction, so a rule minted for a root and the
/// match that has to accept it can never disagree on how the root is spelled.
pub(super) fn url_subtree_roots(strict: &StrictHttpUrl) -> Option<Vec<String>> {
    let segments: Vec<_> = strict
        .url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect();
    let mut base = strict.url.clone();
    base.set_query(None);
    base.set_fragment(None);
    Some(
        (0..=segments.len())
            .rev()
            .map(|depth| {
                let mut root = base.clone();
                root.set_path(&format!("/{}", segments[..depth].join("/")));
                normalize_percent_hex(root.as_str())
            })
            .collect(),
    )
}

pub(super) fn normalized_filesystem_path(path: &str) -> Option<PathBuf> {
    if path.is_empty() || path.contains('\0') {
        return None;
    }
    let path = Path::new(path);
    let absolute = std::path::absolute(path).ok()?;
    Some(
        caudra_storage::paths::incremental_canonicalize(&absolute)
            .unwrap_or_else(|| caudra_storage::paths::normalize_path(&absolute)),
    )
}

pub(super) struct StrictHttpUrl {
    pub(super) url: Url,
    pub(super) key: String,
}

pub(super) fn strict_http_url(value: &str) -> Option<StrictHttpUrl> {
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
    let mut url = Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
    {
        return None;
    }
    if url.scheme() == "http" {
        url.set_scheme("https").ok()?;
    }
    url.set_fragment(None);
    let key = normalize_percent_hex(url.as_str());
    Some(StrictHttpUrl { url, key })
}

pub(super) fn validate_url_percent_encoding(value: &str) -> Option<()> {
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

pub(super) fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

pub(super) fn normalize_percent_hex(value: &str) -> String {
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

pub(super) fn http_url_is_subtree(root: &str, value: &str) -> bool {
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

#[cfg(test)]
mod tests {

    use crate::tools::PermissionScopes;
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use crate::permissions::structured::tests::{
        ALLOW, ASK, BROAD_ASK, BROAD_COMMAND, DENY, EXACT_RESOURCES_OPTION, EXPECT_STRICT_URL,
        EXPECT_SUBTREE_OPTION, EXPECT_URL_ROOTS, GREP_CONTRACT, MCP_SERVER, MINTED_TOOL,
        NARROW_ALLOW, NARROW_COMMAND, OTHER_SERVER, OTHER_TOOL, PROJECT_ROOT_MARK, READ_CONTRACT,
        SOURCE_DIR, SOURCE_FILE, SUBTREE_DIGEST, SUBTREE_OPTION, WILDCARD_ONLY, WRITE_CONTRACT,
        any_command_constraint, command_resource, custom_resource, decision_over,
        default_remote_identity, exact_constraint, explicit_request, flags_in_project,
        ladder_values, mcp_subject, order_independent_command_decision, pattern_constraint,
        protected_command_resource, read_subtree_rule, remote_identity, remote_request,
        remote_request_resource, remote_request_with, request, rule, url_ladder, webfetch_request,
        workcell_request,
    };
    use crate::permissions::structured::{
        PermissionArgumentConstraint, PermissionAuthorityProfile, PermissionCapabilityFamily,
        PermissionExecutorKind, PermissionIntent, PermissionLifetime, PermissionRequest,
        PermissionResource, PermissionResourceAccess, PermissionResourceConstraint,
        PermissionResourceKind, PermissionResourceSelector, PermissionRisk, PermissionSubject,
        SelectorWidth, StructuredPermissionDecision, StructuredPermissionEffect,
        URL_ORIGIN_OPTION_ID, WORKCELL_OWNER, argument_constraint_matches, canonical_json,
        canonical_json_sha256, evaluate_structured_permission_rules, filesystem_resource_flags,
        permission_rule_covers_request, permission_rule_covers_resource,
        permission_rules_cover_request, resource_constraint_matches, resource_decision,
        selected_input_digest, selector_matches, selector_width, strict_http_url,
        url_subtree_roots,
    };
    use caudra_config::ToolKey;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    const REMOTE_PARENT_SCOPE: &str = "root";
    const REMOTE_FILE_SCOPE: &str = "root\u{1f}opaque-file";

    #[test_case(false; "ordinary")]
    #[test_case(true; "mutating")]
    fn a_remote_call_is_approvable_exactly_and_only_an_ordinary_one_by_reuse(mutating: bool) {
        let identity = default_remote_identity();
        let request = remote_request_resource(
            identity.clone(),
            PermissionResourceKind::RemoteFile {
                identity: identity.clone(),
            },
            REMOTE_FILE_SCOPE,
            mutating,
        );
        for lifetime in [PermissionLifetime::Once, PermissionLifetime::Conversation] {
            let exact = request.option_rule("allow_exact", lifetime).unwrap();
            assert!(permission_rule_covers_request(&exact, &request));
        }
        let mut subtree = request
            .option_rule("allow_exact", PermissionLifetime::Once)
            .unwrap()
            .resources[0]
            .clone();
        subtree.selector = PermissionResourceSelector::RemoteSubtree {
            identity,
            scope: vec![REMOTE_PARENT_SCOPE.into()],
        };
        assert_eq!(
            resource_constraint_matches(&subtree, &request.resources[0]),
            !mutating
        );
        assert_eq!(
            request
                .options
                .iter()
                .any(|option| option.id == "allow_remote_resources"),
            !mutating
        );
    }

    #[test]
    fn remote_grants_are_isolated_across_every_subject_identity_dimension() {
        let request = remote_request();
        let rule = request
            .options
            .iter()
            .find(|option| option.id == "allow_remote_resources")
            .expect("remote reusable option")
            .rule
            .clone();
        assert!(permission_rule_covers_request(&rule, &request));

        let identities = [
            (
                "origin",
                remote_identity(
                    "https://clone.example",
                    "server",
                    "workspace",
                    "generation",
                    "namespace",
                    "principal",
                    "project",
                ),
            ),
            (
                "server",
                remote_identity(
                    "https://workcell.example",
                    "other-server",
                    "workspace",
                    "generation",
                    "namespace",
                    "principal",
                    "project",
                ),
            ),
            (
                "workspace",
                remote_identity(
                    "https://workcell.example",
                    "server",
                    "other-workspace",
                    "generation",
                    "namespace",
                    "principal",
                    "project",
                ),
            ),
            (
                "generation",
                remote_identity(
                    "https://workcell.example",
                    "server",
                    "workspace",
                    "other-generation",
                    "namespace",
                    "principal",
                    "project",
                ),
            ),
            (
                "namespace",
                remote_identity(
                    "https://workcell.example",
                    "server",
                    "workspace",
                    "generation",
                    "other-namespace",
                    "principal",
                    "project",
                ),
            ),
            (
                "principal",
                remote_identity(
                    "https://workcell.example",
                    "server",
                    "workspace",
                    "generation",
                    "namespace",
                    "other-principal",
                    "project",
                ),
            ),
            (
                "project",
                remote_identity(
                    "https://workcell.example",
                    "server",
                    "workspace",
                    "generation",
                    "namespace",
                    "principal",
                    "other-project",
                ),
            ),
        ];
        for (dimension, identity) in identities {
            let other = remote_request_with(identity);
            assert!(
                !permission_rule_covers_request(&rule, &other),
                "grant crossed {dimension}"
            );
        }
        for dimension in ["tool", "contract"] {
            let mut other = request.clone();
            let PermissionSubject::RemoteWorkcell { tool, contract, .. } = &mut other.subject
            else {
                unreachable!("remote request has remote subject")
            };
            if dimension == "tool" {
                *tool = "other".into();
            } else {
                *contract = "other".into();
            }
            assert!(!permission_rule_covers_request(&rule, &other));
        }
    }

    #[test]
    fn remote_resource_selectors_use_opaque_identity_without_local_path_rules() {
        let request = remote_request();
        let option = request
            .options
            .iter()
            .find(|option| option.id == "allow_remote_resources")
            .expect("remote reusable option");
        let selector = &option.rule.resources[0].selector;
        assert!(matches!(
            selector,
            PermissionResourceSelector::RemoteResource {
                identity,
                scope,
            } if identity == &default_remote_identity()
                && scope == &["root".to_owned(), "opaque-file".to_owned()]
        ));
        assert_eq!(
            request.presentation.resources[0].summary,
            "/path/that/must/not/be/probed"
        );

        let mut other_scope = request.clone();
        other_scope.resources[0].value = "root\u{1f}other".into();
        assert!(!permission_rule_covers_request(&option.rule, &other_scope));
        let mut other_identity = request.clone();
        other_identity.resources[0].kind = PermissionResourceKind::RemoteFile {
            identity: remote_identity(
                "https://clone.example",
                "server",
                "workspace",
                "generation",
                "namespace",
                "principal",
                "project",
            ),
        };
        assert!(!permission_rule_covers_request(
            &option.rule,
            &other_identity
        ));
    }

    #[test]
    fn explicit_authority_profiles_do_not_depend_on_tool_names() {
        let filesystem = explicit_request(
            PermissionAuthorityProfile::Filesystem {
                input_pointers: vec!["/pattern".into()],
            },
            vec![PermissionResource {
                kind: PermissionResourceKind::Directory,
                value: "/project/src".into(),
                access: Some(PermissionResourceAccess::Search),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            }],
            json!({"pattern": "needle", "limit": 10}),
        );
        let url = explicit_request(
            PermissionAuthorityProfile::Url,
            vec![PermissionResource {
                kind: PermissionResourceKind::Url,
                value: "https://example.com/docs/page".into(),
                access: Some(PermissionResourceAccess::Read),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            }],
            json!({"url": "https://example.com/docs/page"}),
        );
        let query = explicit_request(
            PermissionAuthorityProfile::Query,
            vec![PermissionResource {
                kind: PermissionResourceKind::Query,
                value: "rust permissions".into(),
                access: Some(PermissionResourceAccess::Search),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            }],
            json!({"query": "rust permissions"}),
        );
        let shell = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![PermissionResource {
                kind: PermissionResourceKind::Command,
                value: "cargo test".into(),
                access: Some(PermissionResourceAccess::Execute),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::from([("workdir".into(), "/project".into())]),
            }],
            json!({"command": "cargo test", "timeoutSec": 30}),
        );
        let exact = explicit_request(
            PermissionAuthorityProfile::ExactOnly,
            vec![custom_resource("opaque")],
            json!({"value": "opaque"}),
        );

        for (request, option) in [
            (&filesystem, "allow_exact_resources"),
            (&url, "allow_exact_url"),
            (&query, "allow_exact_query"),
            (&shell, "allow_exact_commands"),
        ] {
            assert!(
                request
                    .options
                    .iter()
                    .any(|candidate| candidate.id == option)
            );
            assert_eq!(request.risk, PermissionRisk::Medium);
        }
        assert_eq!(
            exact
                .options
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            ["allow_exact", "deny_exact"]
        );
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
    fn bash_resource_uses_the_framed_execution_workdir() {
        let workdir = "/tmp/actual";
        let scope = format!(
            "cargo test # caudra-workdir[{}]={workdir} # caudra-frame[{}]",
            workdir.len(),
            workdir.len()
        );
        let request = PermissionRequest::from_legacy(
            "request".into(),
            ToolKey::native("bash"),
            vec![scope],
            json!({"command": "cd /tmp/actual && cargo test"}),
            Path::new("/tmp/project"),
            false,
        );

        assert_eq!(request.resources[0].value, "cargo test");
        assert_eq!(
            request.resources[0]
                .attributes
                .get("workdir")
                .map(String::as_str),
            Some(workdir)
        );
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
            requires_prompt: false,
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
            requires_prompt: false,
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
            requires_prompt: false,
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
        assert!(resource_constraint_matches(
            &constraint,
            &resource("http://example.com/api/v1")
        ));
    }

    #[test]
    fn separate_allow_rules_union_to_cover_a_multi_command_request() {
        let first = command_resource("cargo test", "/project");
        let second = command_resource("git status", "/project");
        let request = request(vec![first.clone(), second.clone()]);
        let allow_rules = vec![
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

        assert!(permission_rule_covers_resource(
            &allow_rules[0],
            &request,
            &first
        ));
        assert!(permission_rule_covers_resource(
            &allow_rules[1],
            &request,
            &second
        ));
        assert!(permission_rules_cover_request(&allow_rules, &request));
        assert_eq!(
            evaluate_structured_permission_rules(&allow_rules, &request),
            StructuredPermissionDecision::Allow
        );
        assert!(!permission_rule_covers_request(&allow_rules[0], &request));
    }

    /// The precedence is the whole contract of a rule set: authority only ever
    /// narrows, so the strictest rule that matches decides, whatever order the
    /// set is stored in.
    #[test_case(&[] => StructuredPermissionDecision::NoMatch ; "silence")]
    #[test_case(&[ALLOW] => StructuredPermissionDecision::Allow ; "a_lone_allow")]
    #[test_case(&[ASK] => StructuredPermissionDecision::Ask ; "a_lone_ask")]
    #[test_case(&[DENY] => StructuredPermissionDecision::Deny ; "a_lone_deny")]
    #[test_case(&[ALLOW, ASK] => StructuredPermissionDecision::Ask ; "ask_outranks_allow")]
    #[test_case(&[ASK, ALLOW] => StructuredPermissionDecision::Ask ; "ask_outranks_allow_reversed")]
    #[test_case(&[ALLOW, DENY] => StructuredPermissionDecision::Deny ; "deny_outranks_allow")]
    #[test_case(&[DENY, ALLOW] => StructuredPermissionDecision::Deny ; "deny_outranks_allow_reversed")]
    #[test_case(&[ASK, DENY] => StructuredPermissionDecision::Deny ; "deny_outranks_ask")]
    #[test_case(&[ALLOW, ASK, DENY] => StructuredPermissionDecision::Deny ; "deny_outranks_everything")]
    fn a_rule_set_is_decided_by_its_strictest_match(
        effects: &[StructuredPermissionEffect],
    ) -> StructuredPermissionDecision {
        decision_over(effects)
    }

    /// A rule naming no resource is unrestricted, so it reaches resources no
    /// constraint mentions. Deny already read it that way; allow and ask now
    /// agree, which is what lets one traversal serve all three.
    #[test_case(ALLOW => StructuredPermissionDecision::Allow ; "unrestricted_allow")]
    #[test_case(ASK => StructuredPermissionDecision::Ask ; "unrestricted_ask")]
    #[test_case(DENY => StructuredPermissionDecision::Deny ; "unrestricted_deny")]
    fn an_unconstrained_rule_reaches_every_resource(
        effect: StructuredPermissionEffect,
    ) -> StructuredPermissionDecision {
        let resource = command_resource("cargo test", "/project");
        let request = request(vec![resource.clone()]);
        let unrestricted = rule(&request, effect, Vec::new());

        resource_decision(&[unrestricted], &request, &resource)
    }

    /// Narrowing one resource narrows the call, but authorizing one does not
    /// authorize the call.
    #[test]
    fn a_request_is_denied_by_one_resource_and_allowed_only_by_all() {
        let allowed = command_resource("cargo test", "/project");
        let other = command_resource("git status", "/project");
        let request = request(vec![allowed.clone(), other.clone()]);
        let allow_one = rule(&request, ALLOW, vec![exact_constraint(&allowed)]);
        let allow_other = rule(&request, ALLOW, vec![exact_constraint(&other)]);
        let deny_one = rule(&request, DENY, vec![exact_constraint(&allowed)]);
        let ask_one = rule(&request, ASK, vec![exact_constraint(&allowed)]);

        assert_eq!(
            evaluate_structured_permission_rules(std::slice::from_ref(&allow_one), &request),
            StructuredPermissionDecision::NoMatch
        );
        assert_eq!(
            evaluate_structured_permission_rules(
                &[allow_one.clone(), allow_other.clone()],
                &request
            ),
            StructuredPermissionDecision::Allow
        );
        assert_eq!(
            evaluate_structured_permission_rules(
                &[allow_one.clone(), allow_other.clone(), ask_one],
                &request
            ),
            StructuredPermissionDecision::Ask
        );
        assert_eq!(
            evaluate_structured_permission_rules(&[allow_one, allow_other, deny_one], &request),
            StructuredPermissionDecision::Deny
        );
    }

    /// A config that asks about a family and allows one member of it means the
    /// allow to win, so the rules have to be ranked rather than folded on the
    /// effect alone.
    #[test_case(NARROW_COMMAND => StructuredPermissionDecision::Allow ; "the narrower allow wins where it applies")]
    #[test_case(BROAD_COMMAND => StructuredPermissionDecision::Ask ; "the broader ask still covers everything else")]
    fn a_narrower_command_pattern_outranks_a_broader_one(
        command: &str,
    ) -> StructuredPermissionDecision {
        order_independent_command_decision(command, |request| {
            vec![
                rule(request, ASK, vec![pattern_constraint(BROAD_ASK)]),
                rule(request, ALLOW, vec![pattern_constraint(NARROW_ALLOW)]),
            ]
        })
    }

    /// Equal width leaves nothing to rank on, so the safer effect decides.
    #[test_case(pattern_constraint(NARROW_ALLOW), pattern_constraint(NARROW_ALLOW) ; "two rules naming the same pattern")]
    #[test_case(pattern_constraint(WILDCARD_ONLY), any_command_constraint() ; "a bare wildcard names no more than a blanket selector")]
    fn equally_wide_rules_break_the_tie_toward_asking(
        allow: PermissionResourceConstraint,
        ask: PermissionResourceConstraint,
    ) {
        assert_eq!(
            order_independent_command_decision(NARROW_COMMAND, |request| vec![
                rule(request, ALLOW, vec![allow.clone()]),
                rule(request, ASK, vec![ask.clone()]),
            ]),
            StructuredPermissionDecision::Ask
        );
    }

    /// Width ranks what a rule set permits, never what it refuses, so a denial
    /// cannot be out-specified.
    #[test]
    fn a_broad_deny_outranks_an_exact_allow() {
        assert_eq!(
            order_independent_command_decision(NARROW_COMMAND, |request| vec![
                rule(
                    request,
                    ALLOW,
                    vec![exact_constraint(&command_resource(
                        NARROW_COMMAND,
                        "/project"
                    ))]
                ),
                rule(request, DENY, vec![pattern_constraint(BROAD_ASK)]),
            ]),
            StructuredPermissionDecision::Deny
        );
    }

    /// A grant naming one exact resource is the narrowest statement there is, so
    /// saving "always allow this" has to survive a broad ask.
    #[test_case(pattern_constraint(BROAD_ASK) => StructuredPermissionDecision::Allow ; "outranks a pattern")]
    #[test_case(any_command_constraint() => StructuredPermissionDecision::Allow ; "outranks a blanket selector")]
    #[test_case(exact_constraint(&command_resource(NARROW_COMMAND, "/project")) => StructuredPermissionDecision::Ask ; "ties with another exact and yields")]
    fn an_exact_grant_outranks_a_wider_ask(
        ask: PermissionResourceConstraint,
    ) -> StructuredPermissionDecision {
        order_independent_command_decision(NARROW_COMMAND, |request| {
            vec![
                rule(request, ASK, vec![ask.clone()]),
                rule(
                    request,
                    ALLOW,
                    vec![exact_constraint(&command_resource(
                        NARROW_COMMAND,
                        "/project",
                    ))],
                ),
            ]
        })
    }

    /// Protection raises the bar for granting, so a rule naming a pattern cannot
    /// grant a protected command. It must not raise the same bar for refusing, or
    /// a deny would fail open on exactly the commands protection exists for.
    #[test_case(ALLOW => StructuredPermissionDecision::NoMatch ; "a pattern cannot grant it")]
    #[test_case(ASK => StructuredPermissionDecision::Ask ; "a pattern can still ask about it")]
    #[test_case(DENY => StructuredPermissionDecision::Deny ; "a pattern can still refuse it")]
    fn protection_gates_grants_and_not_refusals(
        effect: StructuredPermissionEffect,
    ) -> StructuredPermissionDecision {
        let resource = protected_command_resource(NARROW_COMMAND, "/project");
        let request = request(vec![resource.clone()]);
        let rules = vec![rule(&request, effect, vec![pattern_constraint(BROAD_ASK)])];

        resource_decision(&rules, &request, &resource)
    }

    /// The ranking table. The variant order supplies the comparison; this pins
    /// what each selector is worth, which is where a selector could silently
    /// rank as wider or narrower than it reaches.
    #[test_case(PermissionResourceSelector::Any => SelectorWidth::Blanket ; "a blanket selector names everything")]
    #[test_case(PermissionResourceSelector::CommandPattern { pattern: WILDCARD_ONLY.into() } => SelectorWidth::Blanket ; "a pattern with no literal names everything too")]
    #[test_case(PermissionResourceSelector::CommandPattern { pattern: BROAD_ASK.into() } => SelectorWidth::Region(1, 3) ; "a pattern names its literals")]
    #[test_case(PermissionResourceSelector::FilesystemSubtreeDigest { digest: SUBTREE_DIGEST.into() } => SelectorWidth::Region(0, 0) ; "a subtree names a region")]
    #[test_case(PermissionResourceSelector::Exact { value: NARROW_COMMAND.into() } => SelectorWidth::Exact ; "an exact selector names one resource")]
    #[test_case(PermissionResourceSelector::Prefix { value: "git status".into() } => SelectorWidth::Region(2, 9) ; "a prefix is measured like the pattern it competes with")]
    #[test_case(PermissionResourceSelector::Prefix { value: String::new() } => SelectorWidth::Blanket ; "an empty prefix names everything")]
    fn selector_width_reflects_how_much_a_selector_names(
        selector: PermissionResourceSelector,
    ) -> SelectorWidth {
        selector_width(&selector)
    }

    /// The one configured form the structured model had no equivalent for: a
    /// scope ending in a bare `*`. It is raw text matched raw, on any kind, so
    /// that the deny rules already written against it keep matching.
    #[test_case(PermissionResourceKind::Command, "git status --short" => true ; "reaches a command it prefixes")]
    #[test_case(PermissionResourceKind::Command, "git stash" => false ; "stops where the prefix stops")]
    #[test_case(PermissionResourceKind::Command, "sudo git status" => false ; "must start the value, not merely appear in it")]
    #[test_case(PermissionResourceKind::File, "git status --short" => true ; "is not tied to one kind")]
    fn a_prefix_selector_reaches_what_it_starts(kind: PermissionResourceKind, value: &str) -> bool {
        selector_matches(
            &PermissionResourceSelector::Prefix {
                value: "git stat".into(),
            },
            value,
            &kind,
        )
    }

    /// A configured scope becomes a prefix or a command pattern purely by its
    /// spelling, so the two have to rank against each other rather than by which
    /// kind of selector they became.
    #[test]
    fn a_prefix_outranks_a_command_pattern_that_pins_less() {
        let prefix = PermissionResourceConstraint {
            selector: PermissionResourceSelector::Prefix {
                value: "git status".into(),
            },
            ..pattern_constraint(BROAD_ASK)
        };

        assert_eq!(
            order_independent_command_decision(NARROW_COMMAND, |request| vec![
                rule(request, ASK, vec![pattern_constraint(BROAD_ASK)]),
                rule(request, ALLOW, vec![prefix.clone()]),
            ]),
            StructuredPermissionDecision::Allow
        );
    }

    /// A server-wide rule is keyed to the server, so the tool that happened to
    /// ask first stops being part of the key. It must not reach another server,
    /// and without the family it must not reach another tool either.
    #[test_case(Some(PermissionCapabilityFamily::McpServer), MCP_SERVER, OTHER_TOOL => true ; "reaches_a_sibling_tool")]
    #[test_case(Some(PermissionCapabilityFamily::McpServer), MCP_SERVER, MINTED_TOOL => true ; "still_reaches_its_own_tool")]
    #[test_case(Some(PermissionCapabilityFamily::McpServer), OTHER_SERVER, MINTED_TOOL => false ; "never_crosses_to_another_server")]
    #[test_case(None, MCP_SERVER, OTHER_TOOL => false ; "without_the_family_one_tool_stays_one_tool")]
    fn an_mcp_server_family_widens_to_the_server_and_no_further(
        family: Option<PermissionCapabilityFamily>,
        request_server: &str,
        request_tool: &str,
    ) -> bool {
        let resource = command_resource("query", "/project");
        let mut minted = request(vec![resource.clone()]);
        minted.subject = mcp_subject(MCP_SERVER, MINTED_TOOL);
        let mut rule = rule(&minted, ALLOW, vec![exact_constraint(&resource)]);
        rule.family = family;

        let mut incoming = request(vec![resource.clone()]);
        incoming.subject = mcp_subject(request_server, request_tool);

        resource_decision(&[rule], &incoming, &resource) == StructuredPermissionDecision::Allow
    }

    /// A record written before servers were recorded deserializes with an empty
    /// server, which must not become a key that reaches every server.
    #[test]
    fn an_unnamed_mcp_server_never_matches() {
        let resource = command_resource("query", "/project");
        let mut minted = request(vec![resource.clone()]);
        minted.subject = mcp_subject("", MINTED_TOOL);
        let mut rule = rule(&minted, ALLOW, vec![exact_constraint(&resource)]);
        rule.family = Some(PermissionCapabilityFamily::McpServer);

        let mut incoming = request(vec![resource.clone()]);
        incoming.subject = mcp_subject("", OTHER_TOOL);

        assert_eq!(
            resource_decision(&[rule], &incoming, &resource),
            StructuredPermissionDecision::NoMatch
        );
    }

    /// Widening the subject must not widen the operation, or a rule minted from
    /// a read would reach a write on a sibling tool.
    #[test]
    fn an_mcp_server_family_does_not_widen_the_operation() {
        let read = PermissionResource {
            kind: PermissionResourceKind::File,
            value: "/project/notes.md".into(),
            access: Some(PermissionResourceAccess::Read),
            protected: false,
            requires_prompt: false,
            attributes: BTreeMap::new(),
        };
        let write = PermissionResource {
            access: Some(PermissionResourceAccess::Write),
            ..read.clone()
        };
        let mut minted = request(vec![read.clone()]);
        minted.subject = mcp_subject(MCP_SERVER, MINTED_TOOL);
        let mut rule = rule(&minted, ALLOW, vec![exact_constraint(&read)]);
        rule.family = Some(PermissionCapabilityFamily::McpServer);

        let mut incoming = request(vec![write.clone()]);
        incoming.subject = mcp_subject(MCP_SERVER, OTHER_TOOL);

        assert_eq!(
            resource_decision(std::slice::from_ref(&rule), &incoming, &write),
            StructuredPermissionDecision::NoMatch
        );
        let mut same_tool_read = request(vec![read.clone()]);
        same_tool_read.subject = mcp_subject(MCP_SERVER, OTHER_TOOL);
        assert_eq!(
            resource_decision(&[rule], &same_tool_read, &read),
            StructuredPermissionDecision::Allow
        );
    }

    #[test]
    fn missing_resource_keeps_union_coverage_at_no_match() {
        let first = command_resource("cargo test", "/project");
        let second = command_resource("git status", "/project");
        let request = request(vec![first.clone(), second]);
        let allow = rule(
            &request,
            StructuredPermissionEffect::Allow,
            vec![exact_constraint(&first)],
        );

        assert!(!permission_rules_cover_request(
            std::slice::from_ref(&allow),
            &request
        ));
        assert_eq!(
            evaluate_structured_permission_rules(&[allow], &request),
            StructuredPermissionDecision::NoMatch
        );
    }

    #[test]
    fn exact_input_rule_cannot_contribute_to_a_different_input() {
        let first = command_resource("cargo test", "/project");
        let second = command_resource("git status", "/project");
        let request = request(vec![first.clone(), second.clone()]);
        let mut wrong_input = rule(
            &request,
            StructuredPermissionEffect::Allow,
            vec![exact_constraint(&first)],
        );
        wrong_input.arguments = PermissionArgumentConstraint::Exact {
            digest: canonical_json_sha256(&json!({"branch": "other"})),
        };
        let second_allow = rule(
            &request,
            StructuredPermissionEffect::Allow,
            vec![exact_constraint(&second)],
        );

        assert!(!permission_rule_covers_resource(
            &wrong_input,
            &request,
            &first
        ));
        assert_eq!(
            evaluate_structured_permission_rules(&[wrong_input, second_allow], &request),
            StructuredPermissionDecision::NoMatch
        );
    }

    #[test]
    fn deny_intersection_blocks_if_any_resource_matches() {
        let first = custom_resource("first");
        let second = custom_resource("second");
        let request = request(vec![first.clone(), second.clone()]);
        let first_allow = rule(
            &request,
            StructuredPermissionEffect::Allow,
            vec![exact_constraint(&first)],
        );
        let second_allow = rule(
            &request,
            StructuredPermissionEffect::Allow,
            vec![exact_constraint(&second)],
        );
        let deny = rule(
            &request,
            StructuredPermissionEffect::Deny,
            vec![exact_constraint(&second)],
        );
        assert_eq!(
            evaluate_structured_permission_rules(&[first_allow, second_allow, deny], &request),
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
    fn command_pattern_matches_quoted_command_tokens_only_for_commands() {
        let selector = PermissionResourceSelector::CommandPattern {
            pattern: "git diff *".into(),
        };

        assert!(selector_matches(
            &selector,
            r#"git "diff" -- "src/file name.rs""#,
            &PermissionResourceKind::Command
        ));
        assert!(!selector_matches(
            &selector,
            r#"git "diff" -- "src/file name.rs""#,
            &PermissionResourceKind::Query
        ));
    }

    #[test]
    fn url_subtree_roots_climb_from_the_path_to_the_origin() {
        let strict =
            strict_http_url("https://example.com/a/b/c?q=1#frag").expect(EXPECT_STRICT_URL);
        assert_eq!(
            url_subtree_roots(&strict).expect(EXPECT_URL_ROOTS),
            [
                "https://example.com/a/b/c",
                "https://example.com/a/b",
                "https://example.com/a",
                "https://example.com/",
            ]
        );
    }

    /// The root a rule is minted from and the roots a match is tested against
    /// have to be spelled the same way, or a grant fails to cover the very URL
    /// it was granted for. An empty path segment is where the two spellings used
    /// to diverge.
    #[test]
    fn a_repeated_slash_mints_a_rule_that_matches_its_own_url() {
        let request = webfetch_request("https://example.com/a//b");
        let subtree = request
            .option_rule("allow_url_subtree", PermissionLifetime::Conversation)
            .expect(EXPECT_SUBTREE_OPTION);
        assert!(permission_rule_covers_request(&subtree, &request));
    }

    #[test]
    fn a_url_with_no_path_offers_the_origin_alone() {
        assert_eq!(
            url_ladder(&webfetch_request("https://example.com/")),
            [(URL_ORIGIN_OPTION_ID, "https://example.com/**")]
        );
    }

    #[test]
    fn blanket_workdir_authority_reaches_every_reviewed_workdir() {
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![
                command_resource("git status", "/project"),
                command_resource("cargo test", "/other"),
            ],
            json!({"command": "multiple"}),
        );
        let option = request
            .options
            .iter()
            .find(|option| option.id == "allow_commands_in_workdir")
            .expect("workdir authority option");

        assert_eq!(option.label, "Any command in these workdirs");
        assert_eq!(
            option.description,
            "Allow arbitrary commands starting in `/other`, `/project`."
        );
        assert!(permission_rule_covers_request(&option.rule, &request));
    }

    #[test]
    fn broad_shell_authority_stops_at_its_workdir() {
        let reviewed = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![command_resource("git status", "/project")],
            json!({"command": "git status"}),
        );
        let rule = reviewed
            .options
            .iter()
            .find(|option| option.id == "allow_commands_in_workdir")
            .expect("workdir authority option")
            .rule
            .clone();
        let elsewhere = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![protected_command_resource(
                "git status > /tmp/out",
                "/other",
            )],
            json!({"command": "git status > /tmp/out"}),
        );

        assert!(!permission_rule_covers_request(&rule, &elsewhere));
    }

    /// Inert git bookkeeping is the repository describing itself: reading it
    /// leaks nothing and mutates nothing, so it must not force a prompt. The
    /// guard has to survive for `config` and `hooks`, which carry remote
    /// credentials and executable content, and for every write.
    #[test_case("/project/.git/HEAD", PermissionResourceAccess::Read => (false, false) ; "head_read")]
    #[test_case("/project/.git/refs/heads/main", PermissionResourceAccess::Read => (false, false) ; "refs_read")]
    #[test_case("/project/.git/logs/HEAD", PermissionResourceAccess::Read => (false, false) ; "reflog_read")]
    #[test_case("/project/.git/index", PermissionResourceAccess::Read => (false, false) ; "index_read")]
    #[test_case("/project/.git/objects/ab/cdef", PermissionResourceAccess::Search => (false, false) ; "objects_search")]
    #[test_case("/project/.git/config", PermissionResourceAccess::Read => (true, true) ; "config_stays_guarded")]
    #[test_case("/project/.git/hooks/pre-commit", PermissionResourceAccess::Read => (true, true) ; "hooks_stay_guarded")]
    #[test_case("/project/.git/refs/../config", PermissionResourceAccess::Read => (true, true) ; "traversal_into_config_stays_guarded")]
    #[test_case("/project/.git", PermissionResourceAccess::Read => (true, true) ; "the_directory_itself_stays_guarded")]
    #[test_case("/project/vendor/dep/.git/HEAD", PermissionResourceAccess::Read => (true, true) ; "a_nested_checkout_is_not_the_projects_own_git")]
    #[test_case("/project/.git/HEAD", PermissionResourceAccess::Write => (true, true) ; "writes_stay_guarded")]
    #[test_case("/elsewhere/.git/HEAD", PermissionResourceAccess::Read => (true, true) ; "outside_the_project_stays_guarded")]
    #[test_case("/project/.ssh/id_rsa", PermissionResourceAccess::Read => (true, true) ; "ssh_is_untouched")]
    #[test_case("/project/.env.local", PermissionResourceAccess::Read => (true, true) ; "dotenv_is_untouched")]
    #[test_case("/project/src/main.rs", PermissionResourceAccess::Read => (false, false) ; "ordinary_project_file")]
    #[test_case("/elsewhere/notes.md", PermissionResourceAccess::Read => (false, true) ; "ordinary_file_outside_the_project")]
    fn filesystem_flags_exempt_only_inert_git_reads(
        path: &str,
        access: PermissionResourceAccess,
    ) -> (bool, bool) {
        flags_in_project(path, access)
    }

    /// The exemption is anchored to the working directory, not to the enclosing
    /// repository, so running from a subdirectory must not silently widen it.
    #[test]
    fn the_git_exemption_does_not_reach_above_the_working_directory() {
        assert_eq!(
            filesystem_resource_flags(
                "/project/.git/HEAD",
                &PermissionResourceAccess::Read,
                Path::new("/project/src"),
            ),
            (true, true)
        );
    }

    /// Two resources in sibling directories share no rung until their common
    /// ancestor, so that is where the ladder starts climbing.
    #[test]
    fn a_split_request_starts_climbing_at_the_common_ancestor() {
        let intent = PermissionIntent::new(
            PermissionScopes::single("split".into()),
            ["/project/src/main.rs", "/project/tests/it.rs"]
                .into_iter()
                .map(|value| PermissionResource {
                    kind: PermissionResourceKind::File,
                    value: value.into(),
                    access: Some(PermissionResourceAccess::Read),
                    protected: false,
                    requires_prompt: false,
                    attributes: BTreeMap::new(),
                })
                .collect(),
            PermissionRisk::Medium,
        )
        .with_authority(PermissionAuthorityProfile::Filesystem {
            input_pointers: Vec::new(),
        });
        let request = PermissionRequest::from_intent_with_identity(
            "request".into(),
            ToolKey::native("workcell_file_tool"),
            &intent,
            json!({ "paths": ["/project/src/main.rs", "/project/tests/it.rs"] }),
            Path::new("/project"),
            PermissionSubject::Native {
                owner: WORKCELL_OWNER.into(),
                contract: READ_CONTRACT.into(),
            },
            PermissionExecutorKind::Native,
        );

        assert_eq!(
            ladder_values(&request),
            vec![
                "/project/src/**, /project/tests/**".to_string(),
                format!("/project/** {PROJECT_ROOT_MARK}"),
                "/**".to_string(),
            ]
        );
    }

    #[test]
    fn a_read_subtree_grant_covers_a_later_search_by_another_contract() {
        let rule = read_subtree_rule(SUBTREE_OPTION);

        let grep = workcell_request(
            GREP_CONTRACT,
            PermissionResourceKind::Directory,
            PermissionResourceAccess::Search,
            SOURCE_DIR,
        );

        assert_ne!(rule.subject, grep.subject);
        assert!(permission_rule_covers_request(&rule, &grep));
    }

    #[test]
    fn a_read_subtree_grant_never_covers_a_write_to_the_same_subtree() {
        let rule = read_subtree_rule(SUBTREE_OPTION);

        let write = workcell_request(
            WRITE_CONTRACT,
            PermissionResourceKind::File,
            PermissionResourceAccess::Write,
            SOURCE_FILE,
        );

        assert!(!permission_rule_covers_request(&rule, &write));
    }

    #[test]
    fn the_exact_path_grant_is_never_widened() {
        assert_eq!(read_subtree_rule(EXACT_RESOURCES_OPTION).family, None);
    }

    #[test]
    fn a_write_request_mints_no_family() {
        let rule = workcell_request(
            WRITE_CONTRACT,
            PermissionResourceKind::File,
            PermissionResourceAccess::Write,
            SOURCE_FILE,
        )
        .option_rule(SUBTREE_OPTION, PermissionLifetime::Conversation)
        .expect("a write still offers a subtree grant");

        assert_eq!(rule.family, None);
    }

    #[test]
    fn selected_digest_tracks_presence_without_storing_values() {
        let pointers = vec!["/query".to_owned(), "/country".to_owned()];
        let input = json!({"query": "secret", "limit": 10});
        let constraint = PermissionArgumentConstraint::SelectedDigest {
            digest: selected_input_digest(&input, &pointers).unwrap(),
            pointers,
        };
        assert!(argument_constraint_matches(
            &constraint,
            &json!({"query": "secret", "limit": 20})
        ));
        assert!(!argument_constraint_matches(
            &constraint,
            &json!({"query": "changed", "limit": 10})
        ));
        assert!(!argument_constraint_matches(
            &constraint,
            &json!({"query": "secret", "country": null})
        ));
        assert!(
            !serde_json::to_string(&constraint)
                .unwrap()
                .contains("secret")
        );
    }
}
