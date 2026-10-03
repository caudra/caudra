#[cfg(test)]
use caudra_config::DefaultEffect;
#[cfg(test)]
use caudra_storage::StateDir;

mod command_arity;
#[allow(dead_code)]
pub(crate) mod command_pattern;
pub mod executables;
pub mod pattern_matching;
pub mod pattern_recognition;
mod sed_script;
mod structured;
pub use caudra_storage::permission_state::{
    PermissionReview, PermissionReviewResource, PermissionReviewSource,
};
pub use caudra_storage::sessions::PermissionMode;
pub use command_pattern::{PatternFault, PatternGrade, grade_command_pattern};
pub use sed_script::{sed_only_prints, sed_written_files};
use structured::NORMALIZED_COMMAND_ATTRIBUTE;
pub(crate) use structured::hex_encode;
pub use structured::review;
pub use structured::{
    AutoNote, COMMAND_EXACT_PREFIX, COMMAND_GROUP_PREFIX, COMMAND_OBSERVATION_ATTRIBUTE,
    COMMAND_OBSERVATION_BINDING_ATTRIBUTE, COMMAND_PATTERN_PREFIX, COMMAND_TEMPLATE_PREFIX,
    COMPOSABLE_SHELL_OPTIONS, CONFINED_READ_AUTHORITY, ComposedAnswerError, ComposedRow,
    EngineFlag, OPACITY_ATTRIBUTE, PermissionAdvisory, PermissionArgumentConstraint,
    PermissionAuthorityProfile, PermissionCapabilityFamily, PermissionCaution,
    PermissionExecutorKind, PermissionLifetime, PermissionOptionGroup, PermissionPresentation,
    PermissionRequest, PermissionResource, PermissionResourceAccess, PermissionResourceConstraint,
    PermissionResourceKind, PermissionResourcePresentation, PermissionResourceSelector,
    PermissionRisk, PermissionRowGrant, PermissionRuleOption, PermissionRuleRecord,
    PermissionSubject, PolicyRule, PromptReason, RemotePermissionIdentity, ResourceCoverage,
    ResourceStanding, RuleOrigin, ScriptLanguage, SelectedInputError, SelectedPermissionArgument,
    ShellOpacity, StructuredPermissionDecision, StructuredPermissionEffect,
    StructuredPermissionRule, UnknownShellOpacity, argument_constraint_matches, canonical_json,
    canonical_json_sha256, escape_json_pointer_segment, evaluate_structured_permission_rules,
    filesystem_permission_resource, json_pointer, permission_rule_covers_request,
    permission_rule_covers_resource, permission_rule_intersects_request,
    permission_rules_cover_request, permission_rules_resource_standing, prepared_command_binding,
    resource_constraint_matches, selected_input, selected_input_digest, selected_input_pointer,
    update_presentation_coverage,
};

pub mod editor;
mod manager;
pub use manager::{PermissionManager, PermissionProjectFilter, RevokedRuleScope};
mod policy;
pub use policy::{
    ActivePolicyRule, CONFINED_READ_ATTRIBUTE, CONFINED_READ_VALUE, PERMISSION_DENIED_PREFIX,
    PermissionPolicyError, PluginRuleStore, VerifiedLocalSourceLocator,
};
use policy::{
    ConfiguredPolicy, SharedPermissionState, builtin_rules, configured_policy, shared_policy,
};
#[cfg(test)]
use policy::{builtin_structured_rules, configured_selector, is_shell_tool};
mod broker;
use broker::{
    PendingDecision, PendingPermission, PendingRegistration, PermissionBroker, remove_pending,
};
mod decisions;
#[cfg(test)]
pub(crate) use decisions::decision_state;
mod enforce;
pub use enforce::PermissionError;
#[cfg(test)]
use enforce::RequestCoverage;
mod answer;
use answer::lifetime_name;
pub use answer::{
    DECISION_SOURCE_AUTO, DECISION_SOURCE_RULE, DECISION_SOURCE_USER_ABORT,
    DECISION_SOURCE_USER_ALWAYS, DECISION_SOURCE_USER_ONCE, DECISION_SOURCE_USER_SESSION,
    DECISION_SOURCE_YOLO, DEFAULT_DENY_GUIDANCE, PermissionAnswer,
};
mod diagnostics;
use diagnostics::{
    PERMISSION_LOG_TARGET, PROMPT_LOG_MAX_RESOURCES, answer_log_fields, prompt_forcing_reason,
    subject_kind_and_contract, uncovered_resource_summary,
};
#[cfg(test)]
use diagnostics::{
    PROMPT_LOG_MAX_VALUE_CHARS, PROMPT_REASON_ASK_RULE, PROMPT_REASON_FORCED,
    PROMPT_REASON_PROTECTED, PROMPT_REASON_UNCOVERED,
};
mod paths;
pub mod rebind;
pub use paths::{
    BOUNDARY_UNVERIFIABLE_PREFIX, normalize_scope_path, physical_boundary_check, scope_matches,
    shell_permission_scope,
};
use paths::{SUBTREE_SCOPE_SUFFIX, bash_scope_parts, normalize_configured_selector};

#[cfg(test)]
mod tests;
