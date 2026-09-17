use caudra_agent::permissions::pattern_recognition::PatternCandidate;
use caudra_storage::permission_patterns::{ArgumentRole, PatternDefinition, PatternToken};
use caudra_storage::permission_state::{
    PermissionArgumentConstraint, PermissionCapabilityFamily, PermissionLifetime,
    PermissionResourceAccess, PermissionResourceKind, PermissionResourceSelector, PermissionReview,
    PermissionReviewSource, PermissionRuleRecord, StructuredPermissionEffect,
    StructuredPermissionRule,
};
use std::path::PathBuf;
use std::sync::Arc;

use crate::components::escape_terminal_controls;

pub(crate) const OPAQUE: &str = "Opaque · preimage unavailable";
pub(crate) const ANY_RESOURCES: &str = "UNRESTRICTED resources";
pub(crate) const ALL_RESOURCES: &str = "Allow still requires coverage of every actual resource.";
pub(crate) const UNCONSTRAINED_INPUT: &str = "UNCONSTRAINED input: may vary";
pub(crate) const PROTECTED_RISK: &str = "Protected resources may match";
pub(crate) const UNKNOWN_ROLE_RISK: &str = "UNKNOWN role: operations unproven";
pub(crate) const SOURCE_RISK: &str = "Review source is unverified";
const PROPOSAL_RISK: &str = "Proposal: not authority; verify ID";
const POLICY_RISK: &str = "Less DENY/ASK may grant more";
const DIGEST_LABEL_CHARS: usize = 12;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScopeActivity {
    Stored,
    OtherProject,
    Revoked,
    Proposed,
    Live,
}

impl ScopeActivity {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Stored => "STORED · context unchecked",
            Self::OtherProject => "OTHER PROJECT",
            Self::Revoked => "REVOKED",
            Self::Proposed => "PROPOSED · not active",
            Self::Live => "THIS REQUEST",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ScopeSource {
    Record(Arc<PermissionRuleRecord>),
    Candidate(Arc<PatternCandidate>),
    Live {
        rule: Box<StructuredPermissionRule>,
        review: PermissionReview,
        project: Option<PathBuf>,
    },
    Pattern {
        definition: Box<PatternDefinition>,
        lifetime: PermissionLifetime,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ScopeModel {
    pub(crate) source: ScopeSource,
    pub(crate) activity: ScopeActivity,
}

impl ScopeModel {
    pub(crate) fn record(record: Arc<PermissionRuleRecord>) -> Self {
        let activity = if record.revoked_at.is_some() {
            ScopeActivity::Revoked
        } else {
            ScopeActivity::Stored
        };
        Self {
            source: ScopeSource::Record(record),
            activity,
        }
    }

    pub(crate) fn candidate(candidate: Arc<PatternCandidate>) -> Self {
        Self {
            source: ScopeSource::Candidate(candidate),
            activity: ScopeActivity::Proposed,
        }
    }

    pub(crate) fn rule(&self) -> Option<&StructuredPermissionRule> {
        match &self.source {
            ScopeSource::Record(record) => Some(&record.rule),
            ScopeSource::Live { rule, .. } => Some(rule),
            ScopeSource::Candidate(_) | ScopeSource::Pattern { .. } => None,
        }
    }

    pub(crate) fn review(&self) -> Option<&PermissionReview> {
        match &self.source {
            ScopeSource::Record(record) => record.review.as_ref(),
            ScopeSource::Live { review, .. } => Some(review),
            ScopeSource::Candidate(_) | ScopeSource::Pattern { .. } => None,
        }
    }

    pub(crate) fn pattern(&self, target: usize) -> Option<&PatternDefinition> {
        match &self.source {
            ScopeSource::Candidate(candidate) => Some(&candidate.definition),
            ScopeSource::Pattern { definition, .. } => Some(definition),
            _ => self.rule()?.resources.get(target).and_then(|resource| {
                if let PermissionResourceSelector::CommandTemplate { definition } =
                    &resource.selector
                {
                    Some(definition.as_ref())
                } else {
                    None
                }
            }),
        }
    }

    pub(crate) fn badges(&self) -> Vec<String> {
        let (effect, lifetime, kind, origin) = if let Some(rule) = self.rule() {
            (
                effect_name(&rule.effect),
                lifetime_name(&rule.lifetime),
                rule_kind(rule),
                "RULE",
            )
        } else {
            let lifetime = match &self.source {
                ScopeSource::Pattern { lifetime, .. } => lifetime_name(lifetime),
                _ => "UNSET",
            };
            ("ALLOW", lifetime, "COMMAND TEMPLATE".into(), "PROPOSAL")
        };
        vec![
            effect.into(),
            lifetime.into(),
            kind,
            origin.into(),
            self.activity.label().into(),
        ]
    }

    pub(crate) fn target_text(&self, index: usize) -> String {
        let Some(resource) = self.rule().and_then(|rule| rule.resources.get(index)) else {
            return String::new();
        };
        selector_value(
            &resource.selector,
            matches!(self.source, ScopeSource::Live { .. }),
        )
    }

    pub(crate) fn warnings(&self) -> Vec<&'static str> {
        let mut warnings = Vec::new();
        if let Some(rule) = self.rule() {
            if rule.resources.is_empty() {
                warnings.push(ANY_RESOURCES);
            }
            if matches!(rule.arguments, PermissionArgumentConstraint::Unconstrained) {
                warnings.push(UNCONSTRAINED_INPUT);
            }
            if rule.effect != StructuredPermissionEffect::Allow {
                warnings.push(POLICY_RISK);
            }
            if rule
                .resources
                .iter()
                .any(|resource| resource.protected != Some(false))
            {
                warnings.push(PROTECTED_RISK);
            }
        }
        if matches!(self.source, ScopeSource::Candidate(_)) {
            warnings.push(PROPOSAL_RISK);
        } else if self
            .review()
            .is_some_and(|review| review.source != PermissionReviewSource::Approved)
        {
            warnings.push(SOURCE_RISK);
        }
        let targets = self.rule().map_or(1, |rule| rule.resources.len());
        if (0..targets)
            .filter_map(|index| self.pattern(index))
            .any(|pattern| {
                pattern.argv.iter().any(|token| {
                    matches!(
                        token,
                        PatternToken::Exact {
                            role: ArgumentRole::Unknown,
                            ..
                        } | PatternToken::Slot {
                            role: ArgumentRole::Unknown,
                            ..
                        }
                    )
                })
            })
        {
            warnings.push(UNKNOWN_ROLE_RISK);
        }
        warnings
    }
}

pub(crate) fn safe(value: &str) -> String {
    escape_terminal_controls(value)
}

pub(crate) fn literal(value: &str) -> String {
    format!("{value:?}")
}

pub(crate) fn effect_name(effect: &StructuredPermissionEffect) -> &'static str {
    match effect {
        StructuredPermissionEffect::Allow => "ALLOW",
        StructuredPermissionEffect::Deny => "DENY",
        StructuredPermissionEffect::Ask => "ASK",
    }
}

pub(crate) fn lifetime_name(lifetime: &PermissionLifetime) -> &'static str {
    match lifetime {
        PermissionLifetime::Once => "ONCE",
        PermissionLifetime::Conversation => "CONVERSATION",
        PermissionLifetime::Project => "PROJECT",
        PermissionLifetime::Global => "GLOBAL",
    }
}

pub(crate) fn selector_mode(selector: &PermissionResourceSelector) -> &'static str {
    match selector {
        PermissionResourceSelector::Exact { .. } | PermissionResourceSelector::Digest { .. } => {
            "Exact"
        }
        PermissionResourceSelector::FilesystemSubtreeDigest { .. }
        | PermissionResourceSelector::Subtree { .. } => "Subtree",
        PermissionResourceSelector::UrlSubtreeDigest { .. } => "URL subtree",
        PermissionResourceSelector::UrlOriginDigest { .. } => "URL origin",
        PermissionResourceSelector::CommandPattern { .. } => "Token prefix",
        PermissionResourceSelector::CommandTemplate { .. } => "Template",
        PermissionResourceSelector::RemoteResource { .. } => "Remote exact",
        PermissionResourceSelector::RemoteSubtree { .. } => "Remote subtree",
        PermissionResourceSelector::Prefix { .. } => "Raw prefix",
        PermissionResourceSelector::Any => "Any",
    }
}

pub(crate) fn selector_value(selector: &PermissionResourceSelector, redact: bool) -> String {
    match selector {
        PermissionResourceSelector::Digest { digest }
        | PermissionResourceSelector::FilesystemSubtreeDigest { digest }
        | PermissionResourceSelector::UrlSubtreeDigest { digest }
        | PermissionResourceSelector::UrlOriginDigest { digest } => format!(
            "{OPAQUE} #{}",
            safe(&digest.chars().take(DIGEST_LABEL_CHARS).collect::<String>())
        ),
        PermissionResourceSelector::Exact { value }
        | PermissionResourceSelector::Prefix { value } => {
            if redact {
                "Opaque source · see redacted evidence".into()
            } else {
                literal(value)
            }
        }
        PermissionResourceSelector::Subtree { root } => {
            if redact {
                "Opaque root · see redacted evidence".into()
            } else {
                literal(root)
            }
        }
        PermissionResourceSelector::CommandPattern { pattern } => {
            if redact {
                "Opaque token prefix · see evidence".into()
            } else {
                safe(pattern)
            }
        }
        PermissionResourceSelector::CommandTemplate { definition } => safe(&definition.name),
        PermissionResourceSelector::RemoteResource { scope, .. }
        | PermissionResourceSelector::RemoteSubtree { scope, .. } => scope
            .iter()
            .map(|part| literal(part))
            .collect::<Vec<_>>()
            .join(" › "),
        PermissionResourceSelector::Any => "UNRESTRICTED".into(),
    }
}

pub(crate) fn resource_kind(kind: &PermissionResourceKind) -> String {
    match kind {
        PermissionResourceKind::File => "File".into(),
        PermissionResourceKind::Directory => "Directory".into(),
        PermissionResourceKind::Url => "URL".into(),
        PermissionResourceKind::Command => "Command".into(),
        PermissionResourceKind::Query => "Query".into(),
        PermissionResourceKind::RemoteFile { .. } => "Remote file".into(),
        PermissionResourceKind::RemoteDirectory { .. } => "Remote dir".into(),
        PermissionResourceKind::RemoteResource { resource_kind, .. } => {
            format!("Remote {}", safe(resource_kind))
        }
        PermissionResourceKind::Custom { name } => safe(name),
    }
}

pub(crate) fn access_name(access: Option<&PermissionResourceAccess>) -> &'static str {
    match access {
        Some(PermissionResourceAccess::Read) => "read",
        Some(PermissionResourceAccess::List) => "list",
        Some(PermissionResourceAccess::Write) => "write",
        Some(PermissionResourceAccess::Execute) => "execute",
        Some(PermissionResourceAccess::Search) => "search",
        Some(PermissionResourceAccess::Connect) => "connect",
        None => "ANY access",
    }
}

pub(crate) fn rule_kind(rule: &StructuredPermissionRule) -> String {
    if let Some(family) = rule.family {
        return match family {
            PermissionCapabilityFamily::FilesystemRead => "FILESYSTEM READ FAMILY",
            PermissionCapabilityFamily::FilesystemBrowse => "NAMES-ONLY BROWSE",
            PermissionCapabilityFamily::McpServer => "MCP SERVER FAMILY · all tools",
        }
        .into();
    }
    let mut modes = Vec::new();
    for resource in &rule.resources {
        let mode = selector_mode(&resource.selector);
        if !modes.contains(&mode) {
            modes.push(mode);
        }
    }
    if modes.is_empty() {
        "ANY RESOURCE".into()
    } else {
        modes.join(" + ").to_uppercase()
    }
}
