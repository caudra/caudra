use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::Ordering};

use caudra_config::ToolKey;
use caudra_storage::StateClass;
use caudra_storage::id::CaudraId;
use caudra_storage::now_epoch;
use caudra_storage::permission_patterns::{
    ArgumentRole, MAX_ARGUMENT_BYTES, MAX_ARGV_BYTES, MAX_PATTERN_ARGV, MAX_PATTERN_LABEL_BYTES,
    OptionLikePolicy, PATTERN_SCHEMA_VERSION, PatternContext, PatternDefinition, PatternToken,
    SlotCombinations,
};
use caudra_storage::permission_state::mutation::{
    PermissionCommitReceipt, PermissionMutation, PermissionMutationError, PermissionOwner,
    PermissionRecordIdentity, PermissionSnapshot, PreparedPermissionMutation, prepare_mutation,
};
use caudra_storage::permission_state::{
    PERMISSION_LABEL_MAX_BYTES, PermissionState, validate_conversation_record,
};
use caudra_storage::permission_state::{
    PermissionReview, PermissionReviewSource, read_repair_record,
};
use caudra_storage::sessions::SessionDatabase;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use crate::tools::native::plan::{PlanAccess, PlanTarget};
use crate::tools::registry::{PermissionIntent, TrustedToolSource};

use super::enforce::{
    EvaluationContext, active_plan_access, contain_authority_to_the_plan, exact_local_plan_write,
};
use super::pattern_matching::CompiledPattern;
use super::policy::{
    SHELL_EXECUTION_CONTRACT, WORKCELL_TOOL_OWNER, builtin_structured_rules,
    validate_compiled_templates,
};
use super::{
    PermissionArgumentConstraint, PermissionCapabilityFamily, PermissionLifetime,
    PermissionManager, PermissionRequest, PermissionResourceAccess, PermissionResourceConstraint,
    PermissionResourceKind, PermissionResourceSelector, PermissionRuleRecord, PermissionSubject,
    PolicyRule, RuleOrigin, StructuredPermissionDecision, StructuredPermissionEffect,
    StructuredPermissionRule, argument_constraint_matches, canonical_json_sha256,
    evaluate_structured_permission_rules, review, selected_input_digest,
};

const MAX_EDIT_BYTES: usize = 256 * 1024;
const MAX_EDIT_TARGETS: usize = 64;
const MAX_EDIT_ATTRIBUTES: usize = 64;
const MAX_EDIT_CANDIDATES: usize = 128;
const UNCONFIGURED: &str = "Choose a constraint explicitly";
const UNSUPPORTED: &str = "The current registered authority does not support this control";
const MISSING_SOURCE: &str = "The original constraint is unavailable";
const UNREVIEWABLE: &str =
    "Supply verified preimages or explicit replacements before granting unknown authority";
const COUPLED_INPUT: &str =
    "Targets changed; replace input constraints or explicitly preserve their existing pin";
const ANALYSIS_REQUIRED: &str =
    "Structural template edits require fresh host analysis of a concrete source";
const EXAMPLE_ANALYSIS_REQUIRED: &str = "This authority has no nonexecuting host example analyzer";
const EXAMPLE_REQUEST_ID: &str = "permission-editor-example";
const TEMPLATE_REQUEST_BOUND: &str = "Template source and metadata exceed the editing bound";
const TEMPLATE_SOURCE_REQUIRED: &str =
    "A concrete command and absolute working directory are required";
const TEMPLATE_NAME_INVALID: &str =
    "Template names must be bounded, nonempty text without control characters";
const TEMPLATE_CONTEXT_MISMATCH: &str =
    "Host template analysis does not belong to the current project";

#[derive(Debug, Error)]
pub enum PermissionEditError {
    #[error("permission edit conflicts with current state; inspect and review again")]
    Conflict,
    #[error("permission editor unavailable: {0}")]
    Unavailable(String),
    #[error("permission draft has invalid fields")]
    Invalid(Vec<EditFieldError>),
    #[error("the normalized preview has not been confirmed")]
    Unconfirmed,
    #[error("permission persistence failed: {0}")]
    Storage(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditFieldError {
    pub field: EditField,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum EditField {
    Identity,
    Lifetime,
    Project,
    Label,
    Resources,
    Resource(usize),
    Attribute { resource: usize, name: String },
    Arguments,
    Rule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgumentMode {
    Exact,
    Selected,
    Unconstrained,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SelectorMode {
    Exact,
    FilesystemSubtree,
    UrlSubtree,
    UrlOrigin,
    CommandPattern,
    CommandTemplate,
    RemoteExact,
    RemoteSubtree,
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceCapability {
    pub kind: PermissionResourceKind,
    pub selectors: Vec<SelectorMode>,
    pub access: Vec<PermissionResourceAccess>,
    pub wildcard_access: bool,
    pub wildcard_protection: bool,
    pub attributes: BTreeMap<String, Vec<SelectorMode>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditableAuthorityDescriptor {
    pub key: String,
    pub source: TrustedToolSource,
    pub resources: Vec<ResourceCapability>,
    pub arguments: Vec<ArgumentMode>,
    pub families: Vec<PermissionCapabilityFamily>,
    pub unrestricted_resources: bool,
    pub unavailable: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorityCatalog {
    pub revision: String,
    pub authorities: Vec<EditableAuthorityDescriptor>,
}

pub trait PermissionAuthorityProvider: Send + Sync {
    fn acquire(
        &self,
        project: &Path,
    ) -> Result<Box<dyn PermissionAuthorityLease + '_>, PermissionEditError>;
}

pub trait PermissionAuthorityLease {
    fn catalog(&self) -> &AuthorityCatalog;

    fn analyze_template(
        &self,
        _authority: &EditableAuthorityDescriptor,
        _source: &TemplateSource,
    ) -> Result<TemplateAnalysis, PermissionEditError> {
        Err(PermissionEditError::Unavailable(ANALYSIS_REQUIRED.into()))
    }

    fn analyze_example(
        &self,
        _authority: &EditableAuthorityDescriptor,
        _input: &Value,
    ) -> Result<PermissionExampleAnalysis, PermissionEditError> {
        Err(PermissionEditError::Unavailable(
            EXAMPLE_ANALYSIS_REQUIRED.into(),
        ))
    }

    fn active_plan_target(
        &self,
        _authority: &EditableAuthorityDescriptor,
    ) -> Result<Option<PlanTarget>, PermissionEditError> {
        Ok(None)
    }
}

pub struct PermissionExampleAnalysis {
    pub tool: ToolKey,
    pub intent: PermissionIntent,
    pub plan_path: Option<PathBuf>,
}

struct UnavailableAuthorityProvider;

struct UnavailableAuthorityLease(AuthorityCatalog);

impl PermissionAuthorityProvider for UnavailableAuthorityProvider {
    fn acquire(
        &self,
        _project: &Path,
    ) -> Result<Box<dyn PermissionAuthorityLease + '_>, PermissionEditError> {
        Ok(Box::new(UnavailableAuthorityLease(AuthorityCatalog {
            revision: String::new(),
            authorities: Vec::new(),
        })))
    }
}

impl PermissionAuthorityLease for UnavailableAuthorityLease {
    fn catalog(&self) -> &AuthorityCatalog {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TemplateSource {
    pub command: String,
    pub workdir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateAnalysis {
    pub context: PatternContext,
    pub argv: Vec<String>,
    pub roles: Vec<ArgumentRole>,
    pub option_like_data: BTreeSet<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum IdentityDraft {
    Preserve,
    Registered {
        key: String,
        family: Option<PermissionCapabilityFamily>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ProjectDraft {
    Unconfigured,
    None,
    Current,
    Explicit(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum GuardDraft<T> {
    Unconfigured,
    Any,
    Equals(T),
}

impl<T: Clone> GuardDraft<T> {
    fn from_guard(value: &Option<T>) -> Self {
        value
            .as_ref()
            .map_or(Self::Any, |value| Self::Equals(value.clone()))
    }

    fn normalize(&self) -> Result<Option<T>, String> {
        match self {
            Self::Unconfigured => Err(UNCONFIGURED.into()),
            Self::Any => Ok(None),
            Self::Equals(value) => Ok(Some(value.clone())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SelectorValue {
    Exact(String),
    FilesystemSubtree(String),
    UrlSubtree(String),
    UrlOrigin(String),
    CommandPattern(String),
    CommandTemplate {
        definition: Box<PatternDefinition>,
        source: Option<TemplateSource>,
    },
    RemoteExact(Vec<String>),
    RemoteSubtree(Vec<String>),
    Any,
}

impl SelectorValue {
    pub fn mode(&self) -> SelectorMode {
        match self {
            Self::Exact(_) => SelectorMode::Exact,
            Self::FilesystemSubtree(_) => SelectorMode::FilesystemSubtree,
            Self::UrlSubtree(_) => SelectorMode::UrlSubtree,
            Self::UrlOrigin(_) => SelectorMode::UrlOrigin,
            Self::CommandPattern(_) => SelectorMode::CommandPattern,
            Self::CommandTemplate { .. } => SelectorMode::CommandTemplate,
            Self::RemoteExact(_) => SelectorMode::RemoteExact,
            Self::RemoteSubtree(_) => SelectorMode::RemoteSubtree,
            Self::Any => SelectorMode::Any,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SelectorDraft {
    Unconfigured,
    Preserve,
    Replace(SelectorValue),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceDraft {
    pub original_index: Option<usize>,
    pub kind: PermissionResourceKind,
    pub selector: SelectorDraft,
    pub access: GuardDraft<PermissionResourceAccess>,
    pub protected: GuardDraft<bool>,
    pub attributes: BTreeMap<String, SelectorDraft>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ResourcesDraft {
    Unconfigured,
    Unrestricted,
    Constrained(Vec<ResourceDraft>),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum ArgumentsDraft {
    Unconfigured,
    Preserve,
    PreserveCoupled,
    Exact(Value),
    Selected { input: Value, pointers: Vec<String> },
    Unconstrained,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PermissionRuleDraft {
    pub identity: IdentityDraft,
    pub effect: StructuredPermissionEffect,
    pub lifetime: PermissionLifetime,
    pub project: ProjectDraft,
    pub resources: ResourcesDraft,
    pub arguments: ArgumentsDraft,
    pub label: Option<String>,
}

impl PermissionRuleDraft {
    pub fn blank() -> Self {
        Self {
            identity: IdentityDraft::Preserve,
            effect: StructuredPermissionEffect::Ask,
            lifetime: PermissionLifetime::Conversation,
            project: ProjectDraft::None,
            resources: ResourcesDraft::Unconfigured,
            arguments: ArgumentsDraft::Unconfigured,
            label: None,
        }
    }

    pub fn from_record(record: &PermissionRuleRecord, label: Option<String>) -> Self {
        Self {
            identity: IdentityDraft::Preserve,
            effect: record.rule.effect.clone(),
            lifetime: record.rule.lifetime.clone(),
            project: record
                .project
                .clone()
                .map_or(ProjectDraft::None, ProjectDraft::Explicit),
            resources: if record.rule.resources.is_empty() {
                ResourcesDraft::Unrestricted
            } else {
                ResourcesDraft::Constrained(
                    record
                        .rule
                        .resources
                        .iter()
                        .enumerate()
                        .map(|(index, resource)| ResourceDraft {
                            original_index: Some(index),
                            kind: resource.kind.clone(),
                            selector: SelectorDraft::Preserve,
                            access: GuardDraft::from_guard(&resource.access),
                            protected: GuardDraft::from_guard(&resource.protected),
                            attributes: resource
                                .attributes
                                .keys()
                                .map(|name| (name.clone(), SelectorDraft::Preserve))
                                .collect(),
                        })
                        .collect(),
                )
            },
            arguments: ArgumentsDraft::Preserve,
            label,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PermissionEditEvidence {
    pub input: Option<Value>,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum VerifiedValue {
    Selector(SelectorValue),
    Input(Value),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VerifiedField {
    pub field: EditField,
    pub value: VerifiedValue,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NormalizedPermissionDraft {
    pub rule: StructuredPermissionRule,
    pub project: Option<PathBuf>,
    pub label: Option<String>,
    pub review: PermissionReview,
    pub verified: Vec<VerifiedField>,
    pub opaque: Vec<EditField>,
}

pub trait PermissionPublication: Send + Sync {
    fn snapshot(&self) -> Result<PermissionSnapshot, PermissionEditError>;
    fn commit(
        &self,
        prepared: &PreparedPermissionMutation,
    ) -> Result<PermissionCommitReceipt, PermissionEditError>;
    fn receipt(
        &self,
        operation_id: CaudraId,
    ) -> Result<Option<PermissionCommitReceipt>, PermissionEditError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PermissionEditOperation {
    Create,
    Replace(String),
    Duplicate(String),
    Copy(String),
    Revoke(String),
    ActivateDiscovery,
}

impl PermissionEditOperation {
    fn source_id(&self) -> Option<&str> {
        match self {
            Self::Replace(id) | Self::Duplicate(id) | Self::Copy(id) | Self::Revoke(id) => Some(id),
            Self::Create | Self::ActivateDiscovery => None,
        }
    }

    fn creates(&self) -> bool {
        matches!(
            self,
            Self::Create | Self::Duplicate(_) | Self::Copy(_) | Self::ActivateDiscovery
        )
    }
}

#[derive(Clone)]
pub struct PermissionEditSession {
    manager_id: u64,
    context_revision: u64,
    policy_revision: u64,
    project: PathBuf,
    catalog: AuthorityCatalog,
    snapshots: Vec<PermissionSnapshot>,
    source: Option<(PermissionRecordIdentity, PermissionRuleRecord)>,
    operation: PermissionEditOperation,
    evidence: PermissionEditEvidence,
}

impl PermissionEditSession {
    pub fn original(&self) -> Option<&PermissionRuleRecord> {
        self.source.as_ref().map(|(_, record)| record)
    }

    pub fn catalog(&self) -> &AuthorityCatalog {
        &self.catalog
    }

    pub fn draft(&self) -> PermissionRuleDraft {
        self.original()
            .map_or_else(PermissionRuleDraft::blank, |record| {
                PermissionRuleDraft::from_record(record, record.label.clone())
            })
    }

    pub fn source_identity(&self) -> Option<&PermissionRecordIdentity> {
        self.source.as_ref().map(|(identity, _)| identity)
    }
}

pub struct PermissionEditPreview {
    session: PermissionEditSession,
    draft: PermissionRuleDraft,
    normalized: Option<NormalizedPermissionDraft>,
    changes: Vec<SemanticChange>,
    authority_change: AuthorityChange,
    requirements: BTreeSet<ConfirmationRequirement>,
    prepared: PreparedPermissionMutation,
    seal: String,
}

pub struct PermissionEditConfirmation {
    seal: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectivePolicyPreview {
    AllowedByPolicy,
    Prompt,
    Denied(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionMatchPreview {
    pub matches_rule: bool,
    pub effective_policy: EffectivePolicyPreview,
    pub dispatch_gates_rechecked_at_execution: bool,
}

impl PermissionEditPreview {
    pub fn normalized(&self) -> Option<&NormalizedPermissionDraft> {
        self.normalized.as_ref()
    }
    pub fn changes(&self) -> &[SemanticChange] {
        &self.changes
    }
    pub fn authority_change(&self) -> &AuthorityChange {
        &self.authority_change
    }
    pub fn requirements(&self) -> &BTreeSet<ConfirmationRequirement> {
        &self.requirements
    }
    pub fn operation_id(&self) -> CaudraId {
        self.prepared.operation_id()
    }

    pub fn confirm(
        &self,
        acknowledged: &BTreeSet<ConfirmationRequirement>,
    ) -> Result<PermissionEditConfirmation, PermissionEditError> {
        if acknowledged != &self.requirements {
            return Err(PermissionEditError::Unconfirmed);
        }
        Ok(PermissionEditConfirmation {
            seal: self.seal.clone(),
        })
    }
}

impl From<PermissionMutationError> for PermissionEditError {
    fn from(error: PermissionMutationError) -> Self {
        match error {
            PermissionMutationError::Conflict { .. } => Self::Conflict,
            PermissionMutationError::DifferentDatabase => Self::Unavailable(error.to_string()),
            _ => Self::Storage(error.to_string()),
        }
    }
}

impl PermissionManager {
    pub fn set_permission_authority_provider(
        &self,
        provider: Arc<dyn PermissionAuthorityProvider>,
    ) {
        let mut context = self
            .context_revision
            .write()
            .unwrap_or_else(|error| error.into_inner());
        *self
            .editor_provider
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(provider);
        *context += 1;
        self.notify_policy_changed("");
    }

    pub fn attach_permission_publication(
        &self,
        publication: Arc<dyn PermissionPublication>,
    ) -> Result<(), PermissionEditError> {
        let mut context = self
            .context_revision
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let _mutation = self
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let snapshot = publication.snapshot()?;
        if !matches!(snapshot.revision.owner, PermissionOwner::Conversation(_))
            || !snapshot.revision.row_present
        {
            return Err(PermissionEditError::Unavailable(
                "A durable conversation owner is required".into(),
            ));
        }
        let current = self.structured_conversation_rules();
        if !current.is_empty() && *current != snapshot.records {
            return Err(PermissionEditError::Conflict);
        }
        drop(current);
        self.publish_conversation_snapshot(snapshot)?;
        *self
            .publication
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(publication);
        *context += 1;
        self.notify_policy_changed("");
        Ok(())
    }

    fn editor_provider(&self) -> Result<Arc<dyn PermissionAuthorityProvider>, PermissionEditError> {
        Ok(self
            .editor_provider
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .unwrap_or_else(|| Arc::new(UnavailableAuthorityProvider)))
    }

    pub(super) fn publication(&self) -> Option<Arc<dyn PermissionPublication>> {
        self.publication
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub(super) fn publish_conversation_snapshot(
        &self,
        snapshot: PermissionSnapshot,
    ) -> Result<(), PermissionEditError> {
        if !snapshot.revision.row_present
            || !matches!(snapshot.revision.owner, PermissionOwner::Conversation(_))
        {
            return Err(PermissionEditError::Conflict);
        }
        let current = self
            .conversation_snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if current.as_ref().is_some_and(|current| {
            current.store_id != snapshot.store_id
                || current.revision.owner != snapshot.revision.owner
                || current.revision.lineage != snapshot.revision.lineage
        }) {
            return Err(PermissionEditError::Conflict);
        }
        drop(current);
        for record in &snapshot.records {
            validate_conversation_record(record)
                .map_err(|error| invalid(EditField::Rule, error.to_string()))?;
            validate_compiled_templates(&record.rule)
                .map_err(|error| invalid(EditField::Rule, error.to_string()))?;
        }
        *self.structured_conversation_rules() = snapshot.records.clone();
        *self
            .conversation_snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(snapshot);
        *self
            .conversation_policy_error
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        Ok(())
    }

    pub(super) fn durable_permission_snapshots(
        &self,
    ) -> Result<Vec<PermissionSnapshot>, PermissionEditError> {
        let mut snapshots = Vec::new();
        if let Some(publication) = self.publication() {
            let snapshot = publication.snapshot()?;
            if !matches!(snapshot.revision.owner, PermissionOwner::Conversation(_))
                || !snapshot.revision.row_present
            {
                return Err(PermissionEditError::Conflict);
            }
            snapshots.push(snapshot);
        }
        if let Some(policy) = &self.policy {
            let mut state = policy
                .policy
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            snapshots.push(
                state
                    .state()
                    .map_err(|error| PermissionEditError::Storage(error.to_string()))?
                    .snapshot()?,
            );
            drop(state);
            if let [conversation, persistent] = snapshots.as_slice()
                && conversation.store_id == persistent.store_id
            {
                let coherent = SessionDatabase::open_read_only(
                    &policy.state_dir.for_class(StateClass::Persistent),
                )
                .map_err(|error| PermissionEditError::Storage(error.to_string()))?
                .permission_snapshots(&[
                    conversation.revision.owner.clone(),
                    PermissionOwner::Persistent,
                ])?;
                if snapshots.iter().zip(&coherent).any(|(previous, current)| {
                    previous.store_id != current.store_id
                        || previous.revision.lineage != current.revision.lineage
                        || current.revision.generation < previous.revision.generation
                        || (previous.revision.row_present && !current.revision.row_present)
                }) {
                    return Err(PermissionEditError::Conflict);
                }
                return Ok(coherent);
            }
        }
        Ok(snapshots)
    }

    fn editor_snapshots(&self) -> Result<Vec<PermissionSnapshot>, PermissionEditError> {
        let snapshots = self.durable_permission_snapshots()?;
        if snapshots.iter().any(|snapshot| {
            matches!(snapshot.revision.owner, PermissionOwner::Conversation(_))
                && snapshot.records != *self.structured_conversation_rules()
        }) {
            return Err(PermissionEditError::Conflict);
        }
        if snapshots.is_empty() {
            return Err(PermissionEditError::Unavailable(
                "No durable permission storage is attached".into(),
            ));
        }
        Ok(snapshots)
    }

    pub fn begin_permission_edit(
        &self,
        operation: PermissionEditOperation,
        evidence: PermissionEditEvidence,
    ) -> Result<PermissionEditSession, PermissionEditError> {
        self.poll_permission_changes()
            .map_err(|error| PermissionEditError::Storage(error.to_string()))?;
        let context = self
            .context_revision
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let _mutation = self
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let project = self.project_cwd();
        let provider = self.editor_provider()?;
        let lease = provider.acquire(&project)?;
        let snapshots = self.editor_snapshots()?;
        if let Some(id) = operation.source_id()
            && snapshots
                .iter()
                .flat_map(|snapshot| &snapshot.records)
                .filter(|record| record.id == id && record.is_active())
                .count()
                != 1
        {
            return Err(PermissionEditError::Conflict);
        }
        let source = operation
            .source_id()
            .map(|id| {
                snapshots
                    .iter()
                    .find_map(|snapshot| {
                        snapshot
                            .records
                            .iter()
                            .find(|record| record.id == id && record.is_active())
                            .map(|record| {
                                (
                                    PermissionRecordIdentity {
                                        owner: snapshot.revision.owner.clone(),
                                        record_id: record.id.clone(),
                                    },
                                    record.clone(),
                                )
                            })
                    })
                    .ok_or(PermissionEditError::Conflict)
            })
            .transpose()?;
        Ok(PermissionEditSession {
            manager_id: self.id,
            context_revision: *context,
            policy_revision: self.broker.revision.load(Ordering::Acquire),
            project,
            catalog: lease.catalog().clone(),
            snapshots,
            source,
            operation,
            evidence,
        })
    }

    pub fn preview_permission_edit(
        &self,
        session: &PermissionEditSession,
        draft: &PermissionRuleDraft,
    ) -> Result<PermissionEditPreview, PermissionEditError> {
        let context = self
            .context_revision
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let _mutation = self
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.check_edit_context(session, *context)?;
        if self.editor_snapshots()? != session.snapshots {
            return Err(PermissionEditError::Conflict);
        }
        let provider = self.editor_provider()?;
        let lease = provider.acquire(&session.project)?;
        if lease.catalog() != &session.catalog {
            return Err(PermissionEditError::Conflict);
        }
        let source = session.source_identity();
        let original = session.original();
        let (normalized, changes, authority_change, mut requirements, mutation, owners) =
            if matches!(session.operation, PermissionEditOperation::Revoke(_)) {
                let original = original.ok_or(PermissionEditError::Conflict)?;
                let identity = source.ok_or(PermissionEditError::Conflict)?.clone();
                let mut requirements = BTreeSet::new();
                if original.rule.effect != StructuredPermissionEffect::Allow {
                    requirements.insert(ConfirmationRequirement::RestrictivePolicyRelaxation);
                    requirements.insert(ConfirmationRequirement::MayReleasePendingRequests);
                }
                let unknown = original.rule.resources.iter().any(|resource| {
                    verified_selector_value(
                        &resource.selector,
                        &resource.kind,
                        &session.evidence.values,
                    )
                    .is_none()
                        || resource.attributes.iter().any(|(name, selector)| {
                            verified_selector_value(
                                selector,
                                &review::editor_attribute_kind(name),
                                &session.evidence.values,
                            )
                            .is_none()
                        })
                }) || (original.rule.arguments
                    != PermissionArgumentConstraint::Unconstrained
                    && !session.evidence.input.as_ref().is_some_and(|input| {
                        argument_constraint_matches(&original.rule.arguments, input)
                    }));
                if unknown {
                    requirements.insert(ConfirmationRequirement::UnknownRevocationScope);
                }
                (
                    None,
                    vec![SemanticChange::Revoked],
                    if original.rule.effect == StructuredPermissionEffect::Allow {
                        AuthorityChange::Restriction
                    } else {
                        AuthorityChange::Expansion
                    },
                    requirements,
                    PermissionMutation::Revoke {
                        source: identity.clone(),
                    },
                    vec![identity.owner],
                )
            } else {
                let normalized = normalize_permission_draft(
                    draft,
                    original,
                    &session.evidence,
                    &session.project,
                    lease.as_ref(),
                    session.operation.creates(),
                )?;
                if session.operation.creates() && !normalized.opaque.is_empty() {
                    return Err(invalid(EditField::Rule, UNREVIEWABLE));
                }
                let before = if session.operation.creates() {
                    None
                } else {
                    original
                };
                let change = classify_authority_change(
                    before.map(|record| &record.rule),
                    &normalized.rule,
                    before.and_then(|record| record.project.as_ref()),
                    normalized.project.as_ref(),
                );
                let changes = semantic_changes(
                    before,
                    before.and_then(|record| record.label.as_deref()),
                    &normalized,
                );
                let requirements = confirmation_requirements(before, &normalized, &change);
                let destination = match normalized.rule.lifetime {
                    PermissionLifetime::Conversation => session
                        .snapshots
                        .iter()
                        .find(|snapshot| {
                            matches!(snapshot.revision.owner, PermissionOwner::Conversation(_))
                        })
                        .map(|snapshot| snapshot.revision.owner.clone())
                        .ok_or_else(|| {
                            PermissionEditError::Unavailable(
                                "Conversation edits require an acknowledged session writer".into(),
                            )
                        })?,
                    PermissionLifetime::Project | PermissionLifetime::Global => {
                        PermissionOwner::Persistent
                    }
                    PermissionLifetime::Once => {
                        return Err(invalid(EditField::Lifetime, "Once is not a saved lifetime"));
                    }
                };
                let record = PermissionRuleRecord {
                    id: CaudraId::generate().to_string(),
                    project: normalized.project.clone(),
                    rule: normalized.rule.clone(),
                    review: Some(normalized.review.clone()),
                    label: normalized.label.clone(),
                    replaces: None,
                    created_at: now_epoch(),
                    revoked_at: None,
                };
                let (mutation, owners) =
                    if matches!(session.operation, PermissionEditOperation::Replace(_)) {
                        let source = source.ok_or(PermissionEditError::Conflict)?.clone();
                        let owners = if source.owner == destination {
                            vec![destination.clone()]
                        } else {
                            vec![source.owner.clone(), destination.clone()]
                        };
                        (
                            PermissionMutation::Replace {
                                source,
                                destination,
                                replacement: Box::new(record),
                            },
                            owners,
                        )
                    } else {
                        (
                            PermissionMutation::Create {
                                destination: destination.clone(),
                                records: Box::new([record]),
                            },
                            vec![destination],
                        )
                    };
                (
                    Some(normalized),
                    changes,
                    change,
                    requirements,
                    mutation,
                    owners,
                )
            };
        if matches!(draft.arguments, ArgumentsDraft::PreserveCoupled) {
            requirements.insert(ConfirmationRequirement::PreservedInputPin);
        }
        if matches!(session.operation, PermissionEditOperation::Copy(_)) {
            requirements.insert(ConfirmationRequirement::CopyLeavesSource);
        }
        let mut expected = owners
            .iter()
            .map(|owner| {
                session
                    .snapshots
                    .iter()
                    .find(|snapshot| &snapshot.revision.owner == owner)
                    .cloned()
                    .ok_or_else(|| {
                        PermissionEditError::Unavailable(
                            "The destination store is unavailable".into(),
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if matches!(
            session.operation,
            PermissionEditOperation::Duplicate(_) | PermissionEditOperation::Copy(_)
        ) && let Some(source) = session.source_identity()
            && !expected
                .iter()
                .any(|snapshot| snapshot.revision.owner == source.owner)
        {
            let snapshot = session
                .snapshots
                .iter()
                .find(|snapshot| snapshot.revision.owner == source.owner)
                .ok_or(PermissionEditError::Conflict)?;
            if expected
                .iter()
                .all(|destination| destination.store_id == snapshot.store_id)
            {
                expected.push(snapshot.clone());
            } else if matches!(session.operation, PermissionEditOperation::Duplicate(_)) {
                return Err(PermissionEditError::Unavailable(
                    "Separate databases require explicit Copy; the source will remain active"
                        .into(),
                ));
            }
        }
        let prepared = prepare_mutation(expected, mutation)?;
        let seal = canonical_json_sha256(
            &serde_json::to_value((
                &prepared,
                draft,
                &normalized,
                &requirements,
                session.manager_id,
                session.context_revision,
                session.policy_revision,
                &session.catalog.revision,
                &session.snapshots,
            ))
            .map_err(|error| invalid(EditField::Rule, error.to_string()))?,
        );
        Ok(PermissionEditPreview {
            session: session.clone(),
            draft: draft.clone(),
            normalized,
            changes,
            authority_change,
            requirements,
            prepared,
            seal,
        })
    }

    fn check_edit_context(
        &self,
        session: &PermissionEditSession,
        context: u64,
    ) -> Result<(), PermissionEditError> {
        if session.manager_id != self.id
            || session.context_revision != context
            || session.policy_revision != self.broker.revision.load(Ordering::Acquire)
            || session.project != self.project_cwd()
        {
            return Err(PermissionEditError::Conflict);
        }
        Ok(())
    }

    pub fn seed_permission_template(
        &self,
        session: &PermissionEditSession,
        authority_key: &str,
        source: &TemplateSource,
        name: &str,
    ) -> Result<PatternDefinition, PermissionEditError> {
        if name.trim().is_empty()
            || name.len() > MAX_PATTERN_LABEL_BYTES
            || name.chars().any(char::is_control)
        {
            return Err(invalid(EditField::Rule, TEMPLATE_NAME_INVALID));
        }
        validate_template_request(authority_key, source, &name)?;
        self.with_template_analysis(session, authority_key, source, |analysis| {
            Ok(PatternDefinition {
                version: PATTERN_SCHEMA_VERSION,
                name: name.into(),
                context: analysis.context.clone(),
                argv: analysis
                    .argv
                    .iter()
                    .zip(&analysis.roles)
                    .map(|(value, role)| PatternToken::Exact {
                        value: value.clone(),
                        role: role.clone(),
                    })
                    .collect(),
                slots: Vec::new(),
                combinations: SlotCombinations::Independent,
            })
        })
    }

    pub fn analyze_permission_template(
        &self,
        session: &PermissionEditSession,
        authority_key: &str,
        source: &TemplateSource,
        proposed: &PatternDefinition,
    ) -> Result<PatternDefinition, PermissionEditError> {
        validate_template_request(authority_key, source, proposed)?;
        self.with_template_analysis(session, authority_key, source, |analysis| {
            if proposed.argv.len() != analysis.roles.len() {
                return Err(invalid(EditField::Rule, ANALYSIS_REQUIRED));
            }
            let mut definition = proposed.clone();
            definition.context = analysis.context.clone();
            for (token, role) in definition.argv.iter_mut().zip(&analysis.roles) {
                match token {
                    PatternToken::Exact { role: target, .. }
                    | PatternToken::Slot { role: target, .. } => *target = role.clone(),
                }
            }
            Ok(definition)
        })
    }

    fn with_template_analysis(
        &self,
        session: &PermissionEditSession,
        authority_key: &str,
        source: &TemplateSource,
        build: impl FnOnce(&TemplateAnalysis) -> Result<PatternDefinition, PermissionEditError>,
    ) -> Result<PatternDefinition, PermissionEditError> {
        let context = self
            .context_revision
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let _mutation = self
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _plugins = self
            .plugin_rules
            .edit_revision
            .read()
            .unwrap_or_else(|error| error.into_inner());
        self.check_edit_context(session, *context)?;
        let provider = self.editor_provider()?;
        let lease = provider.acquire(&session.project)?;
        if lease.catalog() != &session.catalog || self.editor_snapshots()? != session.snapshots {
            return Err(PermissionEditError::Conflict);
        }
        let authority = lease
            .catalog()
            .authorities
            .iter()
            .find(|authority| authority.key == authority_key)
            .ok_or_else(|| invalid(EditField::Identity, UNSUPPORTED))?;
        validate_analysis_authority(authority)?;
        let analysis = lease.analyze_template(authority, source)?;
        if analysis.argv.is_empty()
            || analysis.argv.len() > MAX_PATTERN_ARGV
            || analysis.roles.len() != analysis.argv.len()
            || analysis
                .argv
                .iter()
                .any(|value| value.len() > MAX_ARGUMENT_BYTES)
            || analysis.argv.iter().map(String::len).sum::<usize>() > MAX_ARGV_BYTES
        {
            return Err(invalid(EditField::Rule, ANALYSIS_REQUIRED));
        }
        if Path::new(&analysis.context.path_binding) != session.project {
            return Err(invalid(EditField::Rule, TEMPLATE_CONTEXT_MISMATCH));
        }
        analysis
            .context
            .validate()
            .map_err(|error| invalid(EditField::Rule, error.to_string()))?;
        let definition = build(&analysis)?;
        validate_template_analysis(&definition, source, &analysis)
            .map_err(|error| invalid(EditField::Rule, error))?;
        CompiledPattern::compile(&definition)
            .map_err(|error| invalid(EditField::Rule, error.to_string()))?;
        self.check_edit_context(session, *context)?;
        if self.editor_snapshots()? != session.snapshots {
            return Err(PermissionEditError::Conflict);
        }
        Ok(definition)
    }

    pub fn preview_pending_permission_match(
        &self,
        preview: &PermissionEditPreview,
        request_id: &str,
    ) -> Result<PermissionMatchPreview, PermissionEditError> {
        let context = self
            .context_revision
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let _mutation = self
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.check_edit_context(&preview.session, *context)?;
        let provider = self.editor_provider()?;
        let lease = provider.acquire(&preview.session.project)?;
        if lease.catalog() != &preview.session.catalog
            || self.editor_snapshots()? != preview.session.snapshots
        {
            return Err(PermissionEditError::Conflict);
        }
        let (request, evaluation) = {
            let pending = self.pending();
            let pending = pending
                .get(&self.id)
                .and_then(|requests| requests.get(request_id))
                .ok_or(PermissionEditError::Conflict)?;
            if pending.cancel.is_cancelled() || pending.abandoned {
                return Err(PermissionEditError::Conflict);
            }
            (
                pending.request.clone(),
                pending.evaluation.clone().ok_or_else(|| {
                    PermissionEditError::Unavailable(
                        "The request has no verified evaluation context".into(),
                    )
                })?,
            )
        };
        if evaluation.revision != *context {
            return Err(PermissionEditError::Conflict);
        }
        self.evaluate_permission_edit_example(preview, &request, &evaluation)
    }

    pub fn preview_permission_example(
        &self,
        preview: &PermissionEditPreview,
        authority_key: &str,
        input: &Value,
    ) -> Result<PermissionMatchPreview, PermissionEditError> {
        if serde_json::to_vec(input).map_or(true, |bytes| bytes.len() > MAX_EDIT_BYTES) {
            return Err(invalid(
                EditField::Arguments,
                "Example exceeds the editing bound",
            ));
        }
        let context = self
            .context_revision
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let _mutation = self
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.check_edit_context(&preview.session, *context)?;
        let provider = self.editor_provider()?;
        let lease = provider.acquire(&preview.session.project)?;
        if lease.catalog() != &preview.session.catalog
            || self.editor_snapshots()? != preview.session.snapshots
        {
            return Err(PermissionEditError::Conflict);
        }
        let authority = lease
            .catalog()
            .authorities
            .iter()
            .find(|authority| authority.key == authority_key)
            .ok_or_else(|| invalid(EditField::Identity, UNSUPPORTED))?;
        if let Some(reason) = &authority.unavailable {
            return Err(PermissionEditError::Unavailable(reason.clone()));
        }
        let analysis = lease.analyze_example(authority, input)?;
        let scopes = &analysis.intent.scopes;
        let mut request = PermissionRequest::from_intent_with_identity(
            EXAMPLE_REQUEST_ID.into(),
            analysis.tool.clone(),
            &analysis.intent,
            input.clone(),
            &preview.session.project,
            authority.source.subject().clone(),
            authority.source.executor().clone(),
        );
        if request.resources.is_empty() {
            return Err(invalid(
                EditField::Resources,
                "Host analysis returned no resources",
            ));
        }
        let exact_plan = lease
            .active_plan_target(authority)?
            .map(|target| active_plan_access(&analysis.intent, input, &target))
            .transpose()
            .map_err(|error| invalid(EditField::Resources, error))?
            .or_else(|| {
                exact_local_plan_write(
                    &analysis.tool,
                    &request.resources,
                    analysis.plan_path.as_deref(),
                )
                .then_some(PlanAccess::Write)
            });
        let evaluation = EvaluationContext {
            revision: *context,
            plan_scoped: scopes.plan_scoped,
            builtin_allows: authority.source.builtin_allows(),
            force_prompt: scopes.force_prompt
                || scopes.plan_scoped
                || (exact_plan.is_none()
                    && request
                        .resources
                        .iter()
                        .any(|resource| resource.requires_prompt)),
            forced: scopes.force_prompt,
            exact_plan,
        };
        if scopes.plan_scoped {
            contain_authority_to_the_plan(&mut request);
        }
        let mut result = self.evaluate_permission_edit_example(preview, &request, &evaluation)?;
        if exact_plan.is_none()
            && let Some(reason) = request
                .resources
                .iter()
                .filter(|resource| {
                    resource.access == Some(PermissionResourceAccess::Write)
                        && matches!(
                            resource.kind,
                            PermissionResourceKind::File | PermissionResourceKind::Directory
                        )
                })
                .find_map(|resource| self.boundary_block_reason(Path::new(&resource.value)))
        {
            result.effective_policy = EffectivePolicyPreview::Denied(reason);
        }
        Ok(result)
    }

    fn evaluate_permission_edit_example(
        &self,
        preview: &PermissionEditPreview,
        request: &PermissionRequest,
        evaluation: &EvaluationContext,
    ) -> Result<PermissionMatchPreview, PermissionEditError> {
        let retires = matches!(
            preview.session.operation,
            PermissionEditOperation::Replace(_) | PermissionEditOperation::Revoke(_)
        );
        let old_id = retires
            .then(|| preview.session.operation.source_id())
            .flatten();
        let mut rules = builtin_structured_rules();
        let records = self
            .stored_permission_records()
            .map_err(|error| PermissionEditError::Storage(error.to_string()))?;
        rules.extend(
            records
                .into_iter()
                .filter(|record| record.is_active() && Some(record.id.as_str()) != old_id)
                .map(|record| PolicyRule {
                    origin: lifetime_origin(&record.rule.lifetime),
                    rule: record.rule,
                }),
        );
        if let Some(normalized) = &preview.normalized
            && (normalized.project.is_none()
                || normalized.project.as_ref() == Some(&preview.session.project))
        {
            rules.push(PolicyRule {
                origin: lifetime_origin(&normalized.rule.lifetime),
                rule: normalized.rule.clone(),
            });
        }
        rules.extend(
            self.configured_structured_rules(request, evaluation.builtin_allows)
                .map_err(|error| PermissionEditError::Storage(error.to_string()))?,
        );
        if evaluation.plan_scoped {
            rules.retain(|policy| {
                policy.rule.effect != StructuredPermissionEffect::Allow
                    || matches!(
                        policy.rule.lifetime,
                        PermissionLifetime::Once | PermissionLifetime::Conversation
                    )
            });
        }
        let effective_policy = match self.evaluate_policy_rules(request, evaluation, &rules) {
            Ok(policy) if policy.automatic => EffectivePolicyPreview::AllowedByPolicy,
            Ok(_) => EffectivePolicyPreview::Prompt,
            Err(error) => EffectivePolicyPreview::Denied(error.to_string()),
        };
        Ok(PermissionMatchPreview {
            matches_rule: preview.normalized.as_ref().is_some_and(|normalized| {
                (normalized.project.is_none()
                    || normalized.project.as_ref() == Some(&preview.session.project))
                    && draft_matches_request(&normalized.rule, request)
            }),
            effective_policy,
            dispatch_gates_rechecked_at_execution: true,
        })
    }

    pub fn permission_edit_receipt(
        &self,
        preview: &PermissionEditPreview,
    ) -> Result<Option<PermissionCommitReceipt>, PermissionEditError> {
        if preview.session.manager_id != self.id {
            return Err(PermissionEditError::Conflict);
        }
        if preview.prepared.persistent_only() {
            let _mutation = self
                .broker
                .mutation_gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let policy = self.policy.as_ref().ok_or_else(|| {
                PermissionEditError::Unavailable("Persistent storage is unavailable".into())
            })?;
            let mut policy = policy
                .policy
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let state = policy
                .state()
                .map_err(|error| PermissionEditError::Storage(error.to_string()))?;
            state
                .mutation_receipt(preview.operation_id())?
                .map(|receipt| validate_persistent_receipt(state, &preview.prepared, receipt))
                .transpose()
        } else {
            self.publication()
                .ok_or_else(|| {
                    PermissionEditError::Unavailable("Conversation storage is unavailable".into())
                })?
                .receipt(preview.operation_id())
        }
    }

    pub fn commit_permission_edit(
        &self,
        preview: &PermissionEditPreview,
        confirmation: &PermissionEditConfirmation,
        current_draft: &PermissionRuleDraft,
    ) -> Result<PermissionCommitReceipt, PermissionEditError> {
        if preview.seal != confirmation.seal || current_draft != &preview.draft {
            return Err(PermissionEditError::Unconfirmed);
        }
        let context = self
            .context_revision
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let _mutation = self
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _plugins = self
            .plugin_rules
            .edit_revision
            .read()
            .unwrap_or_else(|error| error.into_inner());
        self.check_edit_context(&preview.session, *context)?;
        let provider = self.editor_provider()?;
        let lease = provider.acquire(&preview.session.project)?;
        if lease.catalog() != &preview.session.catalog {
            return Err(PermissionEditError::Conflict);
        }
        if let Some(expected) = &preview.normalized {
            let actual = normalize_permission_draft(
                &preview.draft,
                preview.session.original(),
                &preview.session.evidence,
                &preview.session.project,
                lease.as_ref(),
                preview.session.operation.creates(),
            )?;
            if &actual != expected {
                return Err(PermissionEditError::Conflict);
            }
        }
        let snapshots = self.editor_snapshots()?;
        if snapshots != preview.session.snapshots {
            return Err(PermissionEditError::Conflict);
        }
        let receipt = self.commit_prepared_permission_mutation(&preview.prepared)?;
        self.notify_policy_changed("");
        Ok(receipt)
    }

    pub(super) fn commit_prepared_permission_mutation(
        &self,
        prepared: &PreparedPermissionMutation,
    ) -> Result<PermissionCommitReceipt, PermissionEditError> {
        let receipt = if prepared.persistent_only() {
            let policy = self.policy.as_ref().ok_or_else(|| {
                PermissionEditError::Unavailable("Persistent storage is unavailable".into())
            })?;
            let mut policy = policy
                .policy
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let state = policy
                .state()
                .map_err(|error| PermissionEditError::Storage(error.to_string()))?;
            commit_persistent_permission_mutation(state, prepared)?
        } else {
            let publication = self.publication().ok_or_else(|| {
                PermissionEditError::Unavailable(
                    "No durable conversation publisher is installed".into(),
                )
            })?;
            let receipt = match publication.commit(prepared) {
                Ok(receipt) => receipt,
                Err(error) => publication.receipt(prepared.operation_id())?.ok_or(error)?,
            };
            prepared.committed_snapshots(&receipt)?;
            self.publish_conversation_snapshot(publication.snapshot()?)?;
            receipt
        };
        Ok(receipt)
    }
}

fn commit_persistent_permission_mutation(
    state: &mut PermissionState,
    prepared: &PreparedPermissionMutation,
) -> Result<PermissionCommitReceipt, PermissionEditError> {
    let receipt = match state.commit_mutation(prepared) {
        Ok(receipt) => receipt,
        Err(error) => {
            if state.mutation_receipt(prepared.operation_id())?.is_none() {
                return Err(error.into());
            }
            state.commit_mutation(prepared)?
        }
    };
    validate_persistent_receipt(state, prepared, receipt)
}

fn validate_persistent_receipt(
    state: &PermissionState,
    prepared: &PreparedPermissionMutation,
    receipt: PermissionCommitReceipt,
) -> Result<PermissionCommitReceipt, PermissionEditError> {
    let committed = prepared.committed_snapshots(&receipt)?;
    let [expected] = committed.as_slice() else {
        return Err(PermissionEditError::Conflict);
    };
    let current = state.snapshot()?;
    if current.store_id != expected.store_id
        || !current.revision.row_present
        || current.revision.generation < expected.revision.generation
        || (current.revision.generation == expected.revision.generation && &current != expected)
    {
        return Err(PermissionEditError::Conflict);
    }
    Ok(receipt)
}

pub fn draft_matches_request(rule: &StructuredPermissionRule, request: &PermissionRequest) -> bool {
    evaluate_structured_permission_rules(std::slice::from_ref(rule), request)
        != StructuredPermissionDecision::NoMatch
}

fn lifetime_origin(lifetime: &PermissionLifetime) -> RuleOrigin {
    match lifetime {
        PermissionLifetime::Global => RuleOrigin::Global,
        PermissionLifetime::Project => RuleOrigin::Project,
        PermissionLifetime::Conversation | PermissionLifetime::Once => RuleOrigin::Conversation,
    }
}

fn validate_analysis_authority(
    authority: &EditableAuthorityDescriptor,
) -> Result<(), PermissionEditError> {
    if let Some(reason) = &authority.unavailable {
        return Err(PermissionEditError::Unavailable(reason.clone()));
    }
    if !authority.source.builtin_allows()
        || !matches!(authority.source.subject(), PermissionSubject::Native { owner, contract } if owner == WORKCELL_TOOL_OWNER && contract == SHELL_EXECUTION_CONTRACT)
    {
        return Err(PermissionEditError::Unavailable(ANALYSIS_REQUIRED.into()));
    }
    if !authority.resources.iter().any(|resource| {
        resource.kind == PermissionResourceKind::Command
            && resource.selectors.contains(&SelectorMode::CommandTemplate)
            && resource.access.contains(&PermissionResourceAccess::Execute)
    }) {
        return Err(invalid(EditField::Identity, UNSUPPORTED));
    }
    Ok(())
}

fn validate_template_request(
    authority_key: &str,
    source: &TemplateSource,
    metadata: &impl Serialize,
) -> Result<(), PermissionEditError> {
    if source.command.trim().is_empty() || !source.workdir.is_absolute() {
        return Err(invalid(EditField::Rule, TEMPLATE_SOURCE_REQUIRED));
    }
    if serde_json::to_vec(&(authority_key, source, metadata))
        .map_or(true, |bytes| bytes.len() > MAX_EDIT_BYTES)
    {
        return Err(invalid(EditField::Rule, TEMPLATE_REQUEST_BOUND));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorityChange {
    Equivalent,
    Restriction,
    Expansion,
    MixedOrUnknown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SemanticChange {
    Identity,
    Effect {
        before: StructuredPermissionEffect,
        after: StructuredPermissionEffect,
    },
    Lifetime {
        before: PermissionLifetime,
        after: PermissionLifetime,
    },
    Project {
        before: Option<PathBuf>,
        after: Option<PathBuf>,
    },
    Arguments {
        before: PermissionArgumentConstraint,
        after: PermissionArgumentConstraint,
    },
    Resource {
        index: usize,
        before: Option<Box<PermissionResourceConstraint>>,
        after: Option<Box<PermissionResourceConstraint>>,
    },
    Label {
        before: Option<String>,
        after: Option<String>,
    },
    Created,
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum ConfirmationRequirement {
    ArbitraryExecution,
    UnrestrictedInput,
    UnrestrictedResources,
    WildcardGuards,
    CapabilityFamily,
    GlobalReach,
    ProjectMove,
    IndependentCombinations,
    RestrictivePolicyRelaxation,
    UnknownInclusion,
    UnknownRevocationScope,
    PreservedInputPin,
    MayReleasePendingRequests,
    CopyLeavesSource,
}

pub fn normalize_permission_draft(
    draft: &PermissionRuleDraft,
    original: Option<&PermissionRuleRecord>,
    evidence: &PermissionEditEvidence,
    project: &Path,
    lease: &dyn PermissionAuthorityLease,
    fresh_activation: bool,
) -> Result<NormalizedPermissionDraft, PermissionEditError> {
    let mut errors = Vec::new();
    let mut error = |field, message: &str| {
        errors.push(EditFieldError {
            field,
            message: message.into(),
        })
    };
    if serde_json::to_vec(&(draft, evidence)).map_or(true, |bytes| bytes.len() > MAX_EDIT_BYTES)
        || evidence.values.len() > MAX_EDIT_CANDIDATES
    {
        error(
            EditField::Rule,
            "Draft or candidate evidence exceeds the editing bound",
        );
    }
    if draft.label.as_ref().is_some_and(|label| {
        label.trim().is_empty()
            || label.len() > PERMISSION_LABEL_MAX_BYTES
            || label.chars().any(char::is_control)
    }) {
        error(
            EditField::Label,
            "Labels must be bounded, nonempty text without control characters",
        );
    }
    let binding = match (&draft.lifetime, &draft.project) {
        (PermissionLifetime::Conversation | PermissionLifetime::Global, ProjectDraft::None) => None,
        (PermissionLifetime::Project, ProjectDraft::Current) => Some(project.to_path_buf()),
        (PermissionLifetime::Project, ProjectDraft::Explicit(path)) if path.is_absolute() => {
            Some(path.clone())
        }
        _ => {
            error(
                EditField::Project,
                "Choose a project only for Project lifetime; Once is not a saved lifetime",
            );
            None
        }
    };
    if !errors.is_empty() {
        return Err(PermissionEditError::Invalid(errors));
    }
    if !fresh_activation && let Some(original) = original {
        let unchanged = PermissionRuleDraft::from_record(original, draft.label.clone());
        if &unchanged == draft {
            let mut validated = original.clone();
            validated.label = draft.label.clone();
            read_repair_record(
                &serde_json::to_value(validated)
                    .map_err(|error| invalid(EditField::Rule, error.to_string()))?,
                original.rule.lifetime != PermissionLifetime::Conversation,
            )
            .map_err(|error| invalid(EditField::Label, error.to_string()))?;
            validate_compiled_templates(&original.rule)
                .map_err(|error| invalid(EditField::Rule, error.to_string()))?;
            let mut verified = Vec::new();
            let mut opaque = Vec::new();
            for (index, resource) in original.rule.resources.iter().enumerate() {
                let fields = [(
                    EditField::Resource(index),
                    resource.kind.clone(),
                    &resource.selector,
                )]
                .into_iter()
                .chain(resource.attributes.iter().map(|(name, selector)| {
                    (
                        EditField::Attribute {
                            resource: index,
                            name: name.clone(),
                        },
                        review::editor_attribute_kind(name),
                        selector,
                    )
                }));
                for (field, kind, selector) in fields {
                    if let Some(value) = verified_selector_value(selector, &kind, &evidence.values)
                    {
                        verified.push(VerifiedField {
                            field,
                            value: VerifiedValue::Selector(value),
                        });
                    } else {
                        opaque.push(field);
                    }
                }
            }
            if let Some(input) = evidence
                .input
                .as_ref()
                .filter(|input| argument_constraint_matches(&original.rule.arguments, input))
            {
                verified.push(VerifiedField {
                    field: EditField::Arguments,
                    value: VerifiedValue::Input(input.clone()),
                });
            } else if original.rule.arguments != PermissionArgumentConstraint::Unconstrained {
                opaque.push(EditField::Arguments);
            }
            let review = original.review.clone().unwrap_or_else(|| {
                review::review_from_candidates(
                    &original.rule,
                    "unavailable",
                    None,
                    &[],
                    PermissionReviewSource::Unavailable,
                )
            });
            return Ok(NormalizedPermissionDraft {
                rule: original.rule.clone(),
                project: original.project.clone(),
                label: draft.label.clone(),
                review,
                verified,
                opaque,
            });
        }
    }
    let descriptor = match &draft.identity {
        IdentityDraft::Registered { key, .. } => lease
            .catalog()
            .authorities
            .iter()
            .find(|authority| &authority.key == key),
        IdentityDraft::Preserve => original.and_then(|original| {
            lease.catalog().authorities.iter().find(|authority| {
                authority.source.subject() == &original.rule.subject
                    && authority.source.executor() == &original.rule.executor
            })
        }),
    }
    .ok_or_else(|| {
        PermissionEditError::Unavailable("Current registered tool identity is unavailable".into())
    })?;
    if let Some(reason) = &descriptor.unavailable {
        return Err(PermissionEditError::Unavailable(reason.clone()));
    }
    let family = match &draft.identity {
        IdentityDraft::Registered { family, .. } => *family,
        IdentityDraft::Preserve => original.and_then(|original| original.rule.family),
    };
    if family.is_some_and(|family| !descriptor.families.contains(&family)) {
        return Err(invalid(EditField::Identity, UNSUPPORTED));
    }
    let mut verified = Vec::new();
    let mut opaque = Vec::new();
    let resources = match &draft.resources {
        ResourcesDraft::Unconfigured => return Err(invalid(EditField::Resources, UNCONFIGURED)),
        ResourcesDraft::Unrestricted if descriptor.unrestricted_resources => Vec::new(),
        ResourcesDraft::Unrestricted => return Err(invalid(EditField::Resources, UNSUPPORTED)),
        ResourcesDraft::Constrained(resources) => {
            if resources.is_empty() || resources.len() > MAX_EDIT_TARGETS {
                return Err(invalid(
                    EditField::Resources,
                    "Choose targets or explicitly choose unrestricted resources",
                ));
            }
            let mut normalized = Vec::with_capacity(resources.len());
            for (index, resource) in resources.iter().enumerate() {
                let old = resource.original_index.and_then(|index| {
                    original.and_then(|original| original.rule.resources.get(index))
                });
                let capability = descriptor
                    .resources
                    .iter()
                    .find(|capability| capability.kind == resource.kind)
                    .ok_or_else(|| invalid(EditField::Resource(index), UNSUPPORTED))?;
                let access = resource
                    .access
                    .normalize()
                    .map_err(|message| invalid(EditField::Resource(index), message))?;
                let protected = resource
                    .protected
                    .normalize()
                    .map_err(|message| invalid(EditField::Resource(index), message))?;
                if access
                    .as_ref()
                    .map_or(!capability.wildcard_access, |access| {
                        !capability.access.contains(access)
                    })
                    || (protected.is_none() && !capability.wildcard_protection)
                {
                    return Err(invalid(EditField::Resource(index), UNSUPPORTED));
                }
                let selector = normalize_selector(
                    &resource.selector,
                    &resource.kind,
                    old.map(|resource| &resource.selector),
                    &capability.selectors,
                    evidence,
                    EditField::Resource(index),
                    &mut verified,
                    &mut opaque,
                    descriptor,
                    lease,
                    fresh_activation,
                )?;
                if resource.attributes.len() > MAX_EDIT_ATTRIBUTES {
                    return Err(invalid(
                        EditField::Resource(index),
                        "Too many attribute constraints",
                    ));
                }
                let mut attributes = BTreeMap::new();
                for (name, selector) in &resource.attributes {
                    let field = EditField::Attribute {
                        resource: index,
                        name: name.clone(),
                    };
                    let old = old.and_then(|resource| resource.attributes.get(name));
                    let modes = capability
                        .attributes
                        .get(name)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]);
                    let value = normalize_selector(
                        selector,
                        &review::editor_attribute_kind(name),
                        old,
                        modes,
                        evidence,
                        field,
                        &mut verified,
                        &mut opaque,
                        descriptor,
                        lease,
                        fresh_activation,
                    )?;
                    attributes.insert(name.clone(), value);
                }
                normalized.push(PermissionResourceConstraint {
                    kind: resource.kind.clone(),
                    selector,
                    access,
                    protected,
                    attributes,
                });
            }
            normalized
        }
    };
    if matches!(draft.arguments, ArgumentsDraft::Preserve)
        && original.is_some_and(|old| {
            let mut before = old.rule.clone();
            let mut after = before.clone();
            after.resources.clone_from(&resources);
            strip_names(&mut before);
            strip_names(&mut after);
            before.resources != after.resources
                && old.rule.arguments != PermissionArgumentConstraint::Unconstrained
        })
    {
        return Err(invalid(EditField::Arguments, COUPLED_INPUT));
    }
    let arguments = normalize_arguments(
        &draft.arguments,
        original.map(|record| &record.rule.arguments),
        evidence,
        descriptor,
        &mut verified,
        &mut opaque,
    )?;
    let rule = StructuredPermissionRule {
        subject: descriptor.source.subject().clone(),
        executor: descriptor.source.executor().clone(),
        resources,
        arguments,
        lifetime: draft.lifetime.clone(),
        effect: draft.effect.clone(),
        family,
    };
    validate_compiled_templates(&rule)
        .map_err(|error| invalid(EditField::Rule, error.to_string()))?;
    let persistent = !matches!(rule.lifetime, PermissionLifetime::Conversation);
    let mut validation = PermissionRuleRecord::conversation({
        let mut rule = rule.clone();
        rule.lifetime = PermissionLifetime::Conversation;
        rule
    })
    .map_err(|error| invalid(EditField::Rule, error.to_string()))?;
    validation.rule = rule.clone();
    validation.project = binding.clone();
    validation.label = draft.label.clone();
    read_repair_record(
        &serde_json::to_value(&validation)
            .map_err(|error| invalid(EditField::Rule, error.to_string()))?,
        persistent,
    )
    .map_err(|error| invalid(EditField::Rule, error.to_string()))?;
    let change = classify_authority_change(
        original.map(|record| &record.rule),
        &rule,
        original.and_then(|record| record.project.as_ref()),
        binding.as_ref(),
    );
    if !opaque.is_empty()
        && !matches!(
            change,
            AuthorityChange::Equivalent | AuthorityChange::Restriction
        )
    {
        return Err(invalid(EditField::Rule, UNREVIEWABLE));
    }
    let mut candidates = evidence.values.clone();
    for proof in &verified {
        if let VerifiedValue::Selector(
            SelectorValue::Exact(value)
            | SelectorValue::FilesystemSubtree(value)
            | SelectorValue::UrlSubtree(value)
            | SelectorValue::UrlOrigin(value),
        ) = &proof.value
        {
            candidates.push(value.clone());
        }
    }
    let input = verified.iter().find_map(|proof| match &proof.value {
        VerifiedValue::Input(input) => Some(input),
        _ => None,
    });
    let review = review::review_from_candidates(
        &rule,
        &descriptor.key,
        input,
        &candidates,
        PermissionReviewSource::Approved,
    );
    Ok(NormalizedPermissionDraft {
        rule,
        project: binding,
        label: draft.label.clone(),
        review,
        verified,
        opaque,
    })
}

fn invalid(field: EditField, message: impl Into<String>) -> PermissionEditError {
    PermissionEditError::Invalid(vec![EditFieldError {
        field,
        message: message.into(),
    }])
}

#[allow(clippy::too_many_arguments)]
fn normalize_selector(
    draft: &SelectorDraft,
    kind: &PermissionResourceKind,
    old: Option<&PermissionResourceSelector>,
    modes: &[SelectorMode],
    evidence: &PermissionEditEvidence,
    field: EditField,
    verified: &mut Vec<VerifiedField>,
    opaque: &mut Vec<EditField>,
    authority: &EditableAuthorityDescriptor,
    lease: &dyn PermissionAuthorityLease,
    fresh_activation: bool,
) -> Result<PermissionResourceSelector, PermissionEditError> {
    match draft {
        SelectorDraft::Unconfigured => Err(invalid(field, UNCONFIGURED)),
        SelectorDraft::Preserve => {
            let old = old.ok_or_else(|| invalid(field.clone(), MISSING_SOURCE))?;
            if fresh_activation && matches!(old, PermissionResourceSelector::CommandTemplate { .. })
            {
                return Err(invalid(field, ANALYSIS_REQUIRED));
            }
            if let Some(value) = verified_selector_value(old, kind, &evidence.values) {
                verified.push(VerifiedField {
                    field,
                    value: VerifiedValue::Selector(value),
                });
            } else {
                opaque.push(field);
            }
            Ok(old.clone())
        }
        SelectorDraft::Replace(value) => {
            if !modes.contains(&value.mode()) {
                return Err(invalid(field, UNSUPPORTED));
            }
            if let SelectorValue::CommandTemplate { definition, source } = value {
                let unchanged_structure = old.is_some_and(|old| match old {
                    PermissionResourceSelector::CommandTemplate { definition: old } => {
                        same_template_structure(old, definition)
                    }
                    _ => false,
                });
                if fresh_activation || !unchanged_structure {
                    validate_analysis_authority(authority)?;
                    let source = source
                        .as_ref()
                        .ok_or_else(|| invalid(field.clone(), ANALYSIS_REQUIRED))?;
                    let analysis = lease.analyze_template(authority, source)?;
                    validate_template_analysis(definition, source, &analysis)
                        .map_err(|message| invalid(field.clone(), message))?;
                }
                CompiledPattern::compile(definition)
                    .map_err(|error| invalid(field.clone(), error.to_string()))?;
            }
            let value = review::normalize_editor_selector_value(kind, value)
                .map_err(|message| invalid(field.clone(), message))?;
            let selector = review::compile_editor_selector(kind, &value)
                .map_err(|message| invalid(field.clone(), message))?;
            verified.push(VerifiedField {
                field,
                value: VerifiedValue::Selector(value),
            });
            Ok(selector)
        }
    }
}

pub fn verified_selector_value(
    selector: &PermissionResourceSelector,
    kind: &PermissionResourceKind,
    candidates: &[String],
) -> Option<SelectorValue> {
    let plain = match selector {
        PermissionResourceSelector::CommandPattern { pattern } => {
            Some(SelectorValue::CommandPattern(pattern.clone()))
        }
        PermissionResourceSelector::CommandTemplate { definition } => {
            Some(SelectorValue::CommandTemplate {
                definition: definition.clone(),
                source: None,
            })
        }
        PermissionResourceSelector::RemoteResource { .. }
        | PermissionResourceSelector::RemoteSubtree { .. } => {
            return candidates_for_remote(selector, kind);
        }
        PermissionResourceSelector::Any => Some(SelectorValue::Any),
        _ => None,
    };
    if plain.is_some() {
        return plain;
    }
    candidates
        .iter()
        .take(MAX_EDIT_CANDIDATES)
        .filter(|candidate| candidate.len() <= MAX_EDIT_BYTES)
        .find_map(|candidate| {
            let value = match selector {
                PermissionResourceSelector::Digest { .. } => {
                    SelectorValue::Exact(candidate.clone())
                }
                PermissionResourceSelector::FilesystemSubtreeDigest { .. } => {
                    SelectorValue::FilesystemSubtree(candidate.clone())
                }
                PermissionResourceSelector::UrlSubtreeDigest { .. } => {
                    SelectorValue::UrlSubtree(candidate.clone())
                }
                PermissionResourceSelector::UrlOriginDigest { .. } => {
                    SelectorValue::UrlOrigin(candidate.clone())
                }
                _ => return None,
            };
            let value = review::normalize_editor_selector_value(kind, &value).ok()?;
            review::compile_editor_selector(kind, &value)
                .is_ok_and(|compiled| compiled == *selector)
                .then_some(value)
        })
}

fn candidates_for_remote(
    selector: &PermissionResourceSelector,
    kind: &PermissionResourceKind,
) -> Option<SelectorValue> {
    let value = match selector {
        PermissionResourceSelector::RemoteResource { scope, .. } => {
            SelectorValue::RemoteExact(scope.clone())
        }
        PermissionResourceSelector::RemoteSubtree { scope, .. } => {
            SelectorValue::RemoteSubtree(scope.clone())
        }
        _ => return None,
    };
    review::compile_editor_selector(kind, &value)
        .is_ok_and(|compiled| compiled == *selector)
        .then_some(value)
}

fn normalize_arguments(
    draft: &ArgumentsDraft,
    old: Option<&PermissionArgumentConstraint>,
    evidence: &PermissionEditEvidence,
    authority: &EditableAuthorityDescriptor,
    verified: &mut Vec<VerifiedField>,
    opaque: &mut Vec<EditField>,
) -> Result<PermissionArgumentConstraint, PermissionEditError> {
    let (constraint, input, mode) = match draft {
        ArgumentsDraft::Unconfigured => return Err(invalid(EditField::Arguments, UNCONFIGURED)),
        ArgumentsDraft::Preserve | ArgumentsDraft::PreserveCoupled => {
            let old = old.ok_or_else(|| invalid(EditField::Arguments, MISSING_SOURCE))?;
            let input = evidence
                .input
                .as_ref()
                .filter(|input| argument_constraint_matches(old, input));
            if input.is_none() && *old != PermissionArgumentConstraint::Unconstrained {
                opaque.push(EditField::Arguments);
            }
            (old.clone(), input, None)
        }
        ArgumentsDraft::Exact(input) => (
            PermissionArgumentConstraint::Exact {
                digest: canonical_json_sha256(input),
            },
            Some(input),
            Some(ArgumentMode::Exact),
        ),
        ArgumentsDraft::Selected { input, pointers } => {
            if pointers.is_empty() {
                return Err(invalid(
                    EditField::Arguments,
                    "Select at least one nonempty JSON pointer",
                ));
            }
            let digest = selected_input_digest(input, pointers)
                .map_err(|error| invalid(EditField::Arguments, error.to_string()))?;
            (
                PermissionArgumentConstraint::SelectedDigest {
                    pointers: pointers.clone(),
                    digest,
                },
                Some(input),
                Some(ArgumentMode::Selected),
            )
        }
        ArgumentsDraft::Unconstrained => (
            PermissionArgumentConstraint::Unconstrained,
            None,
            Some(ArgumentMode::Unconstrained),
        ),
    };
    if mode.is_some_and(|mode| !authority.arguments.contains(&mode)) {
        return Err(invalid(EditField::Arguments, UNSUPPORTED));
    }
    if let Some(input) = input {
        verified.push(VerifiedField {
            field: EditField::Arguments,
            value: VerifiedValue::Input(input.clone()),
        });
    }
    Ok(constraint)
}

fn same_template_structure(before: &PatternDefinition, after: &PatternDefinition) -> bool {
    before.version == after.version
        && before.context == after.context
        && before.argv == after.argv
        && before.slots.len() == after.slots.len()
        && before.slots.iter().all(|old| {
            after
                .slots
                .iter()
                .any(|new| old.id == new.id && old.option_like == new.option_like)
        })
}

fn validate_template_analysis(
    definition: &PatternDefinition,
    source: &TemplateSource,
    analysis: &TemplateAnalysis,
) -> Result<(), String> {
    if source.command.is_empty()
        || !source.workdir.is_absolute()
        || definition.context != analysis.context
        || Path::new(&analysis.context.effective_workdir) != source.workdir
        || analysis.argv.len() != definition.argv.len()
        || analysis.roles.len() != analysis.argv.len()
    {
        return Err(
            "Fresh analysis does not establish the template's exact context and structure".into(),
        );
    }
    let mut repeated = BTreeMap::new();
    for (index, token) in definition.argv.iter().enumerate() {
        let role = &analysis.roles[index];
        if matches!(role, ArgumentRole::Sensitive | ArgumentRole::Payload) {
            return Err("Sensitive and payload roles cannot become template authority".into());
        }
        match token {
            PatternToken::Exact {
                value,
                role: expected,
            } if value == &analysis.argv[index] && expected == role => {}
            PatternToken::Slot { id, role: expected }
                if expected == role
                    && matches!(role, ArgumentRole::Data | ArgumentRole::Unknown) =>
            {
                if repeated
                    .insert(*id, &analysis.argv[index])
                    .is_some_and(|old| old != &analysis.argv[index])
                {
                    return Err("Repeated slots must have equal concrete source arguments".into());
                }
                if definition.slots.iter().any(|slot| {
                    slot.id == *id && slot.option_like == OptionLikePolicy::AllowForProvenData
                }) && (role != &ArgumentRole::Data
                    || !analysis.option_like_data.contains(&index))
                {
                    return Err(
                        "Fresh host analysis does not establish option-like data eligibility"
                            .into(),
                    );
                }
            }
            _ => return Err("Fresh analysis disagrees with a literal or host-derived role".into()),
        }
    }
    Ok(())
}

pub fn classify_authority_change(
    before: Option<&StructuredPermissionRule>,
    after: &StructuredPermissionRule,
    before_project: Option<&PathBuf>,
    after_project: Option<&PathBuf>,
) -> AuthorityChange {
    let Some(before) = before else {
        return if after.effect == StructuredPermissionEffect::Allow {
            AuthorityChange::Expansion
        } else {
            AuthorityChange::Restriction
        };
    };
    let mut left = before.clone();
    let mut right = after.clone();
    strip_names(&mut left);
    strip_names(&mut right);
    if left == right && before_project == after_project {
        return AuthorityChange::Equivalent;
    }
    if left.subject != right.subject
        || left.executor != right.executor
        || left.family != right.family
        || left.lifetime != right.lifetime
        || before_project != after_project
    {
        return AuthorityChange::MixedOrUnknown;
    }
    let before_effect = left.effect.clone();
    left.effect = right.effect.clone();
    if left == right {
        return if effect_rank(&before_effect) < effect_rank(&right.effect) {
            AuthorityChange::Expansion
        } else {
            AuthorityChange::Restriction
        };
    }
    if before.effect != after.effect || before.arguments != after.arguments {
        return AuthorityChange::MixedOrUnknown;
    }
    let subset = |narrow: &[PermissionResourceConstraint],
                  wide: &[PermissionResourceConstraint]| {
        !narrow.is_empty()
            && (wide.is_empty() || narrow.iter().all(|resource| wide.contains(resource)))
    };
    let narrower = subset(&right.resources, &left.resources);
    let wider = subset(&left.resources, &right.resources);
    if narrower && wider {
        return AuthorityChange::Equivalent;
    }
    match (
        narrower,
        wider,
        before.effect == StructuredPermissionEffect::Allow,
    ) {
        (true, false, true) | (false, true, false) => AuthorityChange::Restriction,
        (false, true, true) | (true, false, false) => AuthorityChange::Expansion,
        _ => AuthorityChange::MixedOrUnknown,
    }
}

fn strip_names(rule: &mut StructuredPermissionRule) {
    for resource in &mut rule.resources {
        if let PermissionResourceSelector::CommandTemplate { definition } = &mut resource.selector {
            definition.name.clear();
            definition.slots.sort_by_key(|slot| slot.id);
            for slot in &mut definition.slots {
                slot.label.clear();
            }
        }
    }
}

fn effect_rank(effect: &StructuredPermissionEffect) -> u8 {
    match effect {
        StructuredPermissionEffect::Deny => 0,
        StructuredPermissionEffect::Ask => 1,
        StructuredPermissionEffect::Allow => 2,
    }
}

pub fn semantic_changes(
    before: Option<&PermissionRuleRecord>,
    before_label: Option<&str>,
    after: &NormalizedPermissionDraft,
) -> Vec<SemanticChange> {
    let Some(before) = before else {
        return vec![SemanticChange::Created];
    };
    let mut changes = Vec::new();
    let left = &before.rule;
    let right = &after.rule;
    if left.subject != right.subject
        || left.executor != right.executor
        || left.family != right.family
    {
        changes.push(SemanticChange::Identity);
    }
    if left.effect != right.effect {
        changes.push(SemanticChange::Effect {
            before: left.effect.clone(),
            after: right.effect.clone(),
        });
    }
    if left.lifetime != right.lifetime {
        changes.push(SemanticChange::Lifetime {
            before: left.lifetime.clone(),
            after: right.lifetime.clone(),
        });
    }
    if before.project != after.project {
        changes.push(SemanticChange::Project {
            before: before.project.clone(),
            after: after.project.clone(),
        });
    }
    if left.arguments != right.arguments {
        changes.push(SemanticChange::Arguments {
            before: left.arguments.clone(),
            after: right.arguments.clone(),
        });
    }
    for index in 0..left.resources.len().max(right.resources.len()) {
        if left.resources.get(index) != right.resources.get(index) {
            changes.push(SemanticChange::Resource {
                index,
                before: left.resources.get(index).cloned().map(Box::new),
                after: right.resources.get(index).cloned().map(Box::new),
            });
        }
    }
    if before_label != after.label.as_deref() {
        changes.push(SemanticChange::Label {
            before: before_label.map(str::to_owned),
            after: after.label.clone(),
        });
    }
    changes
}

pub fn confirmation_requirements(
    before: Option<&PermissionRuleRecord>,
    after: &NormalizedPermissionDraft,
    change: &AuthorityChange,
) -> BTreeSet<ConfirmationRequirement> {
    let mut requirements = BTreeSet::new();
    if *change == AuthorityChange::Equivalent {
        return requirements;
    }
    let rule = &after.rule;
    if rule
        .resources
        .iter()
        .any(|resource| resource.kind == PermissionResourceKind::Command)
    {
        requirements.insert(ConfirmationRequirement::ArbitraryExecution);
    }
    if rule.arguments == PermissionArgumentConstraint::Unconstrained {
        requirements.insert(ConfirmationRequirement::UnrestrictedInput);
    }
    if rule.resources.is_empty()
        || rule
            .resources
            .iter()
            .any(|resource| resource.selector == PermissionResourceSelector::Any)
    {
        requirements.insert(ConfirmationRequirement::UnrestrictedResources);
    }
    if rule
        .resources
        .iter()
        .any(|resource| resource.access.is_none() || resource.protected.is_none())
    {
        requirements.insert(ConfirmationRequirement::WildcardGuards);
    }
    if rule.family.is_some() {
        requirements.insert(ConfirmationRequirement::CapabilityFamily);
    }
    if rule.lifetime == PermissionLifetime::Global {
        requirements.insert(ConfirmationRequirement::GlobalReach);
    }
    if before.is_some_and(|before| before.project != after.project) {
        requirements.insert(ConfirmationRequirement::ProjectMove);
    }
    if rule.resources.iter().any(|resource| matches!(&resource.selector, PermissionResourceSelector::CommandTemplate { definition } if definition.combinations == SlotCombinations::Independent)) { requirements.insert(ConfirmationRequirement::IndependentCombinations); }
    if before.is_some_and(|before| before.rule.effect != StructuredPermissionEffect::Allow)
        && !matches!(change, AuthorityChange::Restriction)
    {
        requirements.insert(ConfirmationRequirement::RestrictivePolicyRelaxation);
    }
    if *change == AuthorityChange::MixedOrUnknown {
        requirements.insert(ConfirmationRequirement::UnknownInclusion);
    }
    if matches!(
        change,
        AuthorityChange::Expansion | AuthorityChange::MixedOrUnknown
    ) {
        requirements.insert(ConfirmationRequirement::MayReleasePendingRequests);
    }
    requirements
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;
    use std::slice::from_ref;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex, RwLock, RwLockReadGuard};
    use std::thread;

    use caudra_config::{DefaultEffect, Effect, PermissionRule, PermissionsConfig, ToolKey};
    use caudra_storage::StateDir;
    use caudra_storage::id::CaudraId;
    use caudra_storage::permission_patterns::{
        ArgumentDomain, ArgumentRole, MAX_ARGUMENT_BYTES, MAX_ARGV_BYTES, MAX_PATTERN_ARGV,
        MAX_PATTERN_LABEL_BYTES, OptionLikePolicy, PATTERN_SCHEMA_VERSION, PatternContext,
        PatternDefinition, PatternSlot, PatternToken, SlotCombinations, SlotId,
    };
    use caudra_storage::permission_state::mutation::{
        PermissionCommitReceipt, PermissionMutation, PermissionOwner, PermissionRecordIdentity,
        PermissionSnapshot, PreparedPermissionMutation, prepare_mutation,
    };
    use caudra_storage::permission_state::{
        BROWSE_RECURSION_ATTRIBUTE, BROWSE_RECURSIVE, PermissionState,
    };
    use caudra_storage::sessions::{Session, SessionDatabase, TitleSource};
    use caudra_storage::state::SCOPE_GLOBAL;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, ProjectIdentity, ProjectKey, SourceTrustAnchor,
    };
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;
    use test_case::test_case;

    use crate::CancelToken;
    use crate::permissions::broker::PendingPermission;
    use crate::permissions::enforce::tests::{active_plan_fixture, active_plan_read};
    use crate::permissions::enforce::{
        CURRENT_DEFAULT_DENIES_REQUEST, CURRENT_POLICY_DENIES_REQUEST, EvaluationContext,
    };
    use crate::permissions::tests::{
        PERMISSION_RULES_STATE_KEY, SHELL_WORKDIR, shell_request, workcell_shell_subject,
    };
    use crate::permissions::{
        COMMAND_EXACT_PREFIX, ComposedRow, PermissionAnswer, PermissionArgumentConstraint,
        PermissionCapabilityFamily, PermissionLifetime, PermissionManager, PermissionPolicyError,
        PermissionProjectFilter, PermissionResourceAccess, PermissionResourceKind,
        PermissionResourceSelector, PermissionRowGrant, PermissionRuleRecord, PermissionSubject,
        RemotePermissionIdentity, StructuredPermissionEffect, VerifiedLocalSourceLocator,
        argument_constraint_matches, canonical_json_sha256, hex_encode,
    };
    use crate::tools::DescriptionContext;
    use crate::tools::native::plan::{self, PlanAccess, PlanTarget, PlanTool};
    use crate::tools::registry::{
        ParseError, PermissionIntent, RegisteredTool, Tool, ToolEffect, ToolInvocation, ToolSource,
        TrustedToolSource,
    };

    use super::{
        ANALYSIS_REQUIRED, ArgumentMode, ArgumentsDraft, AuthorityCatalog, AuthorityChange,
        COUPLED_INPUT, ConfirmationRequirement, EditField, EditableAuthorityDescriptor,
        EffectivePolicyPreview, GuardDraft, IdentityDraft, MAX_EDIT_BYTES,
        NormalizedPermissionDraft, PermissionAuthorityLease, PermissionAuthorityProvider,
        PermissionEditError, PermissionEditEvidence, PermissionEditOperation,
        PermissionExampleAnalysis, PermissionPublication, PermissionRuleDraft, ProjectDraft,
        ResourceCapability, ResourceDraft, ResourcesDraft, SelectorDraft, SelectorMode,
        SelectorValue, TemplateAnalysis, TemplateSource, UNREVIEWABLE, classify_authority_change,
        commit_persistent_permission_mutation, confirmation_requirements,
        normalize_permission_draft, validate_persistent_receipt, verified_selector_value,
    };

    const TOOL: &str = "editor-shell";
    const PROJECT: &str = "/project";
    const COMMAND: &str = "git show alpha";
    const COMMAND_PATTERN: &str = "git show *";
    const OTHER_COMMAND: &str = "git log --oneline";
    const CONVERSATION_AND_PROJECT: [PermissionLifetime; 2] = [
        PermissionLifetime::Conversation,
        PermissionLifetime::Project,
    ];
    const RELATIVE_PROJECT: &str = "relative/project";
    const LABEL: &str = "Edited label";
    const OTHER_LABEL: &str = "Concurrent label";
    const FAILURE: &str = "injected durable write failure";
    const LOST_ACK: &str = "injected lost acknowledgment";
    const NEVER: &str = "opening or previewing an editor must not call tool code";
    const WORKDIR: &str = "workdir";
    const POINTER: &str = "/value";
    const INVALID_PERSISTENT_SNAPSHOT: &str = "invalid persistent snapshot";
    const PERSISTENT_DIRECTORY: &str = "state";
    const VOLATILE_DIRECTORY: &str = "volatile";

    enum TemplateSessionChange {
        Project,
        Authority,
        Manager,
        PersistentState,
    }

    struct NeverInvoked;

    impl Tool for NeverInvoked {
        fn name(&self) -> &str {
            TOOL
        }
        fn description(&self, _context: &DescriptionContext) -> Cow<'_, str> {
            panic!("{NEVER}")
        }
        fn schema(&self) -> Value {
            panic!("{NEVER}")
        }
        fn parse(&self, _input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            panic!("{NEVER}")
        }
    }

    struct Host {
        catalog: AuthorityCatalog,
        analysis: Option<TemplateAnalysis>,
        plan_example: Option<(PermissionIntent, Option<PlanTarget>)>,
    }

    struct TestProvider {
        host: RwLock<Host>,
        analyses: AtomicUsize,
    }

    struct TestLease<'a> {
        host: RwLockReadGuard<'a, Host>,
        analyses: &'a AtomicUsize,
    }

    impl PermissionAuthorityProvider for TestProvider {
        fn acquire(
            &self,
            _project: &Path,
        ) -> Result<Box<dyn PermissionAuthorityLease + '_>, PermissionEditError> {
            Ok(Box::new(TestLease {
                host: self.host.read().unwrap(),
                analyses: &self.analyses,
            }))
        }
    }

    impl PermissionAuthorityLease for TestLease<'_> {
        fn catalog(&self) -> &AuthorityCatalog {
            &self.host.catalog
        }
        fn analyze_template(
            &self,
            _authority: &EditableAuthorityDescriptor,
            source: &TemplateSource,
        ) -> Result<TemplateAnalysis, PermissionEditError> {
            self.analyses.fetch_add(1, Ordering::Relaxed);
            assert_eq!(source.command, COMMAND);
            self.host
                .analysis
                .clone()
                .ok_or_else(|| PermissionEditError::Unavailable(ANALYSIS_REQUIRED.into()))
        }

        fn analyze_example(
            &self,
            authority: &EditableAuthorityDescriptor,
            _input: &Value,
        ) -> Result<PermissionExampleAnalysis, PermissionEditError> {
            assert_eq!(authority.key, plan::NAME);
            let (intent, _) = self.host.plan_example.as_ref().unwrap();
            Ok(PermissionExampleAnalysis {
                tool: ToolKey::native(plan::NAME),
                intent: intent.clone(),
                plan_path: None,
            })
        }

        fn active_plan_target(
            &self,
            authority: &EditableAuthorityDescriptor,
        ) -> Result<Option<PlanTarget>, PermissionEditError> {
            assert_eq!(authority.key, plan::NAME);
            Ok(self.host.plan_example.as_ref().unwrap().1.clone())
        }
    }

    fn provider() -> Arc<TestProvider> {
        let registered = RegisteredTool {
            tool: Arc::new(NeverInvoked),
            source: ToolSource::Native {
                owner: "workcell".into(),
                contract: "shell.execution.v1".into(),
                trusted: true,
            },
            effect: ToolEffect::Unknown,
        };
        let resources = [
            PermissionResourceKind::Command,
            PermissionResourceKind::File,
            PermissionResourceKind::Directory,
            PermissionResourceKind::Url,
            PermissionResourceKind::Query,
        ]
        .into_iter()
        .map(|kind| ResourceCapability {
            kind,
            selectors: vec![
                SelectorMode::Exact,
                SelectorMode::FilesystemSubtree,
                SelectorMode::UrlSubtree,
                SelectorMode::UrlOrigin,
                SelectorMode::CommandPattern,
                SelectorMode::CommandTemplate,
                SelectorMode::Any,
            ],
            access: vec![
                PermissionResourceAccess::Read,
                PermissionResourceAccess::List,
                PermissionResourceAccess::Write,
                PermissionResourceAccess::Execute,
                PermissionResourceAccess::Search,
                PermissionResourceAccess::Connect,
            ],
            wildcard_access: true,
            wildcard_protection: true,
            attributes: BTreeMap::from([(
                WORKDIR.into(),
                vec![
                    SelectorMode::Exact,
                    SelectorMode::FilesystemSubtree,
                    SelectorMode::Any,
                ],
            )]),
        })
        .collect();
        Arc::new(TestProvider {
            host: RwLock::new(Host {
                catalog: AuthorityCatalog {
                    revision: TOOL.into(),
                    authorities: vec![EditableAuthorityDescriptor {
                        key: TOOL.into(),
                        source: TrustedToolSource::from_registered(&registered, None).unwrap(),
                        resources,
                        arguments: vec![
                            ArgumentMode::Exact,
                            ArgumentMode::Selected,
                            ArgumentMode::Unconstrained,
                        ],
                        families: Vec::new(),
                        unrestricted_resources: true,
                        unavailable: None,
                    }],
                },
                analysis: None,
                plan_example: None,
            }),
            analyses: AtomicUsize::new(0),
        })
    }

    fn draft() -> PermissionRuleDraft {
        PermissionRuleDraft {
            identity: IdentityDraft::Registered {
                key: TOOL.into(),
                family: None,
            },
            effect: StructuredPermissionEffect::Allow,
            lifetime: PermissionLifetime::Conversation,
            project: ProjectDraft::None,
            resources: ResourcesDraft::Constrained(vec![ResourceDraft {
                original_index: None,
                kind: PermissionResourceKind::Command,
                selector: SelectorDraft::Replace(SelectorValue::CommandPattern(
                    COMMAND_PATTERN.into(),
                )),
                access: GuardDraft::Equals(PermissionResourceAccess::Execute),
                protected: GuardDraft::Equals(false),
                attributes: BTreeMap::from([(
                    WORKDIR.into(),
                    SelectorDraft::Replace(SelectorValue::Exact(PROJECT.into())),
                )]),
            }]),
            arguments: ArgumentsDraft::Unconstrained,
            label: None,
        }
    }

    fn normalize(
        draft: &PermissionRuleDraft,
        old: Option<&PermissionRuleRecord>,
        evidence: &PermissionEditEvidence,
        provider: &TestProvider,
    ) -> Result<NormalizedPermissionDraft, PermissionEditError> {
        normalize_permission_draft(
            draft,
            old,
            evidence,
            Path::new(PROJECT),
            provider.acquire(Path::new(PROJECT))?.as_ref(),
            false,
        )
    }

    fn record(provider: &TestProvider) -> PermissionRuleRecord {
        PermissionRuleRecord::conversation(
            normalize(&draft(), None, &PermissionEditEvidence::default(), provider)
                .unwrap()
                .rule,
        )
        .unwrap()
    }

    fn field_error(
        result: Result<NormalizedPermissionDraft, PermissionEditError>,
        field: EditField,
        message: Option<&str>,
    ) {
        let Err(PermissionEditError::Invalid(errors)) = result else {
            panic!("expected an invalid field");
        };
        assert!(
            errors.iter().any(|error| error.field == field
                && message.is_none_or(|message| error.message == message))
        );
    }

    fn target(draft: &mut PermissionRuleDraft) -> &mut ResourceDraft {
        let ResourcesDraft::Constrained(resources) = &mut draft.resources else {
            panic!("expected targets");
        };
        &mut resources[0]
    }

    #[test_case(StructuredPermissionEffect::Allow; "allow")]
    #[test_case(StructuredPermissionEffect::Deny; "deny")]
    #[test_case(StructuredPermissionEffect::Ask; "ask")]
    fn round_trip_lifetimes_and_opaque_constraints(effect: StructuredPermissionEffect) {
        let provider = provider();
        for lifetime in [
            PermissionLifetime::Conversation,
            PermissionLifetime::Project,
            PermissionLifetime::Global,
        ] {
            let mut draft = draft();
            draft.effect = effect.clone();
            draft.lifetime = lifetime.clone();
            draft.project = if lifetime == PermissionLifetime::Project {
                ProjectDraft::Current
            } else {
                ProjectDraft::None
            };
            let normalized =
                normalize(&draft, None, &PermissionEditEvidence::default(), &provider).unwrap();
            let mut original = record(&provider);
            original.rule = normalized.rule;
            original.project = normalized.project;
            original.review = Some(normalized.review);
            let mut unchanged = PermissionRuleDraft::from_record(&original, None);
            unchanged.label = Some(LABEL.into());
            let result = normalize(
                &unchanged,
                Some(&original),
                &PermissionEditEvidence::default(),
                &provider,
            )
            .unwrap();
            assert_eq!(result.rule, original.rule);
            assert_eq!(result.project, original.project);
            assert_eq!(result.review, original.review.clone().unwrap());
            assert!(!result.opaque.is_empty());
            assert!(
                confirmation_requirements(Some(&original), &result, &AuthorityChange::Equivalent)
                    .is_empty()
            );
        }
    }

    #[test_case(ResourcesDraft::Unconfigured; "blank_targets")]
    #[test_case(ResourcesDraft::Constrained(Vec::new()); "removed_last_target")]
    fn no_accidental_unrestricted_targets(resources: ResourcesDraft) {
        let mut draft = draft();
        draft.resources = resources;
        field_error(
            normalize(
                &draft,
                None,
                &PermissionEditEvidence::default(),
                &provider(),
            ),
            EditField::Resources,
            None,
        );
        draft.resources = ResourcesDraft::Unrestricted;
        let normalized = normalize(
            &draft,
            None,
            &PermissionEditEvidence::default(),
            &provider(),
        )
        .unwrap();
        assert!(
            confirmation_requirements(None, &normalized, &AuthorityChange::Expansion)
                .contains(&ConfirmationRequirement::UnrestrictedResources)
        );
    }

    #[test_case(true; "access")]
    #[test_case(false; "protection")]
    fn missing_guards_are_not_wildcards(access: bool) {
        let mut draft = draft();
        if access {
            target(&mut draft).access = GuardDraft::Unconfigured;
        } else {
            target(&mut draft).protected = GuardDraft::Unconfigured;
        }
        field_error(
            normalize(
                &draft,
                None,
                &PermissionEditEvidence::default(),
                &provider(),
            ),
            EditField::Resource(0),
            None,
        );
    }

    #[test_case(PermissionResourceKind::File, SelectorValue::Exact("/project/file".into()), "/project/file", "/project/other"; "file_exact")]
    #[test_case(PermissionResourceKind::Directory, SelectorValue::FilesystemSubtree("/project".into()), "/project", "/project/child"; "subtree_not_descendant_preimage")]
    #[test_case(PermissionResourceKind::Url, SelectorValue::Exact("https://example.test/a".into()), "https://example.test/a", "https://example.test/b"; "url_exact")]
    #[test_case(PermissionResourceKind::Url, SelectorValue::UrlSubtree("https://example.test/a".into()), "https://example.test/a", "https://example.test/a/b"; "url_subtree")]
    #[test_case(PermissionResourceKind::Url, SelectorValue::UrlOrigin("https://example.test".into()), "https://example.test", "https://other.test"; "url_origin")]
    #[test_case(PermissionResourceKind::Command, SelectorValue::Exact(COMMAND.into()), COMMAND, "git show [redacted]"; "command_redaction")]
    fn selector_preimages_require_actual_kind_specific_digest(
        kind: PermissionResourceKind,
        value: SelectorValue,
        correct: &str,
        wrong: &str,
    ) {
        let selector = super::review::compile_editor_selector(&kind, &value).unwrap();
        assert!(verified_selector_value(&selector, &kind, &[wrong.into()]).is_none());
        assert!(verified_selector_value(&selector, &kind, &[correct.into()]).is_some());
        assert!(verified_selector_value(&selector, &kind, &[]).is_none());
    }

    #[test_case(json!({}), json!({"value": null}); "missing_versus_null")]
    #[test_case(json!({"value": "1"}), json!({"value": 1}); "string_versus_number")]
    #[test_case(json!({"value": false}), json!({"value": 0}); "bool_versus_number")]
    fn selected_inputs_preserve_presence_and_json_types(input: Value, near_miss: Value) {
        let mut draft = draft();
        draft.arguments = ArgumentsDraft::Selected {
            input: input.clone(),
            pointers: vec![POINTER.into()],
        };
        let normalized = normalize(
            &draft,
            None,
            &PermissionEditEvidence::default(),
            &provider(),
        )
        .unwrap();
        assert!(argument_constraint_matches(
            &normalized.rule.arguments,
            &input
        ));
        assert!(!argument_constraint_matches(
            &normalized.rule.arguments,
            &near_miss
        ));
    }

    #[test_case(true; "exact")]
    #[test_case(false; "selected")]
    fn argument_digest_cannot_be_recovered_from_a_review(exact: bool) {
        let provider = provider();
        let mut draft = draft();
        let input = json!({"value": "secret"});
        draft.arguments = if exact {
            ArgumentsDraft::Exact(input.clone())
        } else {
            ArgumentsDraft::Selected {
                input: input.clone(),
                pointers: vec![POINTER.into()],
            }
        };
        let normalized =
            normalize(&draft, None, &PermissionEditEvidence::default(), &provider).unwrap();
        let mut original = record(&provider);
        original.rule.arguments = normalized.rule.arguments;
        original.review = Some(normalized.review);
        let draft = PermissionRuleDraft::from_record(&original, None);
        let unknown = normalize(
            &draft,
            Some(&original),
            &PermissionEditEvidence::default(),
            &provider,
        )
        .unwrap();
        assert!(unknown.opaque.contains(&EditField::Arguments));
        let proved = normalize(
            &draft,
            Some(&original),
            &PermissionEditEvidence {
                input: Some(input),
                values: Vec::new(),
            },
            &provider,
        )
        .unwrap();
        assert!(!proved.opaque.contains(&EditField::Arguments));
    }

    #[test_case(StructuredPermissionEffect::Deny; "deny_narrowing_relaxes_authority")]
    #[test_case(StructuredPermissionEffect::Ask; "ask_narrowing_relaxes_authority")]
    fn restrictive_predicate_removal_is_an_expansion(effect: StructuredPermissionEffect) {
        let provider = provider();
        let mut before = record(&provider);
        before.rule.effect = effect;
        let mut other = before.rule.resources[0].clone();
        other.selector = PermissionResourceSelector::CommandPattern {
            pattern: "git status *".into(),
        };
        before.rule.resources.push(other);
        let mut after = before.rule.clone();
        after.resources.pop();
        assert_eq!(
            classify_authority_change(Some(&before.rule), &after, None, None),
            AuthorityChange::Expansion
        );
        let mut normalized = normalize(
            &draft(),
            None,
            &PermissionEditEvidence::default(),
            &provider,
        )
        .unwrap();
        normalized.rule = after;
        assert!(
            confirmation_requirements(Some(&before), &normalized, &AuthorityChange::Expansion)
                .contains(&ConfirmationRequirement::RestrictivePolicyRelaxation)
        );
    }

    #[test_case(false; "unknown_global_expansion_blocked")]
    #[test_case(true; "verified_global_expansion_reviewable")]
    fn opaque_constraints_cannot_be_silently_widened(prove: bool) {
        let provider = provider();
        let original = record(&provider);
        let mut draft = PermissionRuleDraft::from_record(&original, None);
        draft.lifetime = PermissionLifetime::Global;
        let evidence = PermissionEditEvidence {
            input: None,
            values: if prove {
                vec![PROJECT.into()]
            } else {
                Vec::new()
            },
        };
        let result = normalize(&draft, Some(&original), &evidence, &provider);
        if prove {
            assert!(result.is_ok());
        } else {
            field_error(result, EditField::Rule, Some(UNREVIEWABLE));
        }
    }

    #[test_case(ArgumentsDraft::Preserve; "implicit_old_input_pin_rejected")]
    #[test_case(ArgumentsDraft::PreserveCoupled; "explicit_old_input_pin_retained")]
    fn target_edits_require_an_explicit_input_coupling_decision(arguments: ArgumentsDraft) {
        let provider = provider();
        let mut original = record(&provider);
        let input = json!({"value": COMMAND});
        original.rule.arguments = PermissionArgumentConstraint::Exact {
            digest: canonical_json_sha256(&input),
        };
        let mut draft = PermissionRuleDraft::from_record(&original, None);
        target(&mut draft).selector = SelectorDraft::Replace(SelectorValue::Exact(COMMAND.into()));
        draft.arguments = arguments.clone();
        let evidence = PermissionEditEvidence {
            input: Some(input),
            values: vec![PROJECT.into()],
        };
        let result = normalize(&draft, Some(&original), &evidence, &provider);
        if matches!(arguments, ArgumentsDraft::Preserve) {
            field_error(result, EditField::Arguments, Some(COUPLED_INPUT));
        } else {
            assert_eq!(result.unwrap().rule.arguments, original.rule.arguments);
        }
    }

    fn definition() -> PatternDefinition {
        PatternDefinition {
            version: PATTERN_SCHEMA_VERSION,
            name: LABEL.into(),
            context: PatternContext {
                tool_identity: TOOL.into(),
                executable_identity: "verified-git".into(),
                effective_workdir: PROJECT.into(),
                path_binding: "local-project".into(),
                analysis_version: "host-analysis-v1".into(),
            },
            argv: vec![
                PatternToken::Exact {
                    value: "git".into(),
                    role: ArgumentRole::Executable,
                },
                PatternToken::Exact {
                    value: "show".into(),
                    role: ArgumentRole::Operation,
                },
                PatternToken::Slot {
                    id: SlotId(1),
                    role: ArgumentRole::Data,
                },
            ],
            slots: vec![PatternSlot {
                id: SlotId(1),
                label: "revision".into(),
                domain: ArgumentDomain::ObservedSet {
                    values: BTreeSet::from(["alpha".into(), "beta".into()]),
                },
                option_like: OptionLikePolicy::Reject,
            }],
            combinations: SlotCombinations::Independent,
        }
    }

    fn install_analysis(provider: &TestProvider) {
        provider.host.write().unwrap().analysis = Some(TemplateAnalysis {
            context: definition().context,
            argv: vec!["git".into(), "show".into(), "alpha".into()],
            roles: vec![
                ArgumentRole::Executable,
                ArgumentRole::Operation,
                ArgumentRole::Data,
            ],
            option_like_data: BTreeSet::new(),
        });
    }

    fn install_current_analysis(
        manager: &PermissionManager,
        provider: &TestProvider,
    ) -> TemplateSource {
        install_analysis(provider);
        let source = TemplateSource {
            command: COMMAND.into(),
            workdir: manager.project_cwd(),
        };
        let mut host = provider.host.write().unwrap();
        let analysis = host.analysis.as_mut().unwrap();
        analysis.context.effective_workdir = source.workdir.to_string_lossy().into_owned();
        analysis.context.path_binding = manager.project_cwd().to_string_lossy().into_owned();
        source
    }

    #[test_case(LABEL; "user_template_name")]
    #[test_case(OTHER_LABEL; "another_user_template_name")]
    fn first_template_can_be_seeded_and_saved_without_history(name: &str) {
        let (_temp, manager, provider, publication) = manager();
        let source = install_current_analysis(&manager, &provider);
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        let seeded = manager
            .seed_permission_template(&session, TOOL, &source, name)
            .unwrap();
        let analysis = provider.host.read().unwrap().analysis.clone().unwrap();
        assert_eq!(seeded.name, name);
        assert_eq!(seeded.version, PATTERN_SCHEMA_VERSION);
        assert_eq!(seeded.context, analysis.context);
        assert_eq!(
            seeded.argv,
            analysis
                .argv
                .into_iter()
                .zip(analysis.roles)
                .map(|(value, role)| PatternToken::Exact { value, role })
                .collect::<Vec<_>>()
        );
        assert!(seeded.slots.is_empty());
        assert_eq!(seeded.combinations, SlotCombinations::Independent);
        assert!(publication.snapshot().unwrap().records.is_empty());
        assert!(manager.pattern_proposal_inventory().2.is_empty());
        let mut draft = draft();
        target(&mut draft).attributes.insert(
            WORKDIR.into(),
            SelectorDraft::Replace(SelectorValue::Exact(
                seeded.context.effective_workdir.clone(),
            )),
        );
        target(&mut draft).selector = SelectorDraft::Replace(SelectorValue::CommandTemplate {
            definition: Box::new(seeded.clone()),
            source: Some(source),
        });
        let preview = manager.preview_permission_edit(&session, &draft).unwrap();
        let confirmation = preview.confirm(preview.requirements()).unwrap();
        manager
            .commit_permission_edit(&preview, &confirmation, &draft)
            .unwrap();
        assert_eq!(
            publication.snapshot().unwrap().records[0].rule.resources[0].selector,
            PermissionResourceSelector::CommandTemplate {
                definition: Box::new(seeded)
            }
        );
        assert!(manager.pattern_proposal_inventory().2.is_empty());
    }

    #[test_case(""; "empty_name")]
    #[test_case(" \n "; "blank_name")]
    #[test_case("untrusted\u{1b}[31m"; "control_in_name")]
    #[test_case(&"x".repeat(MAX_PATTERN_LABEL_BYTES + 1); "oversized_name")]
    fn seed_rejects_invalid_metadata_before_host_analysis(name: &str) {
        let (_temp, manager, provider, _publication) = manager();
        let source = install_current_analysis(&manager, &provider);
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        assert!(matches!(
            manager.seed_permission_template(&session, TOOL, &source, name),
            Err(PermissionEditError::Invalid(_))
        ));
        assert_eq!(provider.analyses.load(Ordering::Relaxed), 0);
    }

    #[test_case("", true; "no_command")]
    #[test_case(COMMAND, false; "relative_workdir")]
    #[test_case(&"x".repeat(MAX_EDIT_BYTES + 1), true; "oversized_source")]
    fn seed_rejects_invalid_source_before_host_analysis(command: &str, absolute: bool) {
        let (_temp, manager, provider, _publication) = manager();
        let mut source = install_current_analysis(&manager, &provider);
        source.command = command.into();
        if !absolute {
            source.workdir = "relative".into();
        }
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        assert!(matches!(
            manager.seed_permission_template(&session, TOOL, &source, LABEL),
            Err(PermissionEditError::Invalid(_))
        ));
        assert_eq!(provider.analyses.load(Ordering::Relaxed), 0);
    }

    #[test_case(false; "no_template_capability")]
    #[test_case(true; "disabled_authority")]
    fn seed_respects_registered_authority_availability(disabled: bool) {
        let (_temp, manager, provider, _publication) = manager();
        let source = install_current_analysis(&manager, &provider);
        {
            let mut host = provider.host.write().unwrap();
            let authority = &mut host.catalog.authorities[0];
            if disabled {
                authority.unavailable = Some(FAILURE.into());
            } else {
                authority.resources.clear();
            }
        }
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        assert!(
            manager
                .seed_permission_template(&session, TOOL, &source, LABEL)
                .is_err()
        );
        assert!(matches!(
            manager.seed_permission_template(&session, OTHER_LABEL, &source, LABEL),
            Err(PermissionEditError::Invalid(_))
        ));
        assert_eq!(provider.analyses.load(Ordering::Relaxed), 0);
    }

    #[test_case(|analysis| analysis.argv.clear(); "empty_argv")]
    #[test_case(|analysis| { analysis.roles.pop(); }; "mismatched_roles")]
    #[test_case(|analysis| analysis.roles[2] = ArgumentRole::Sensitive; "sensitive_argument")]
    #[test_case(|analysis| analysis.roles[2] = ArgumentRole::Payload; "payload_argument")]
    #[test_case(|analysis| analysis.context.path_binding = PROJECT.into(); "wrong_project_binding")]
    #[test_case(|analysis| analysis.context.effective_workdir = PROJECT.into(); "wrong_workdir")]
    #[test_case(|analysis| { analysis.argv.resize(MAX_PATTERN_ARGV + 1, LABEL.into()); analysis.roles.resize(MAX_PATTERN_ARGV + 1, ArgumentRole::Data); }; "too_many_arguments")]
    #[test_case(|analysis| analysis.argv[2] = "x".repeat(MAX_ARGUMENT_BYTES + 1); "oversized_argument")]
    #[test_case(|analysis| { analysis.argv = vec!["x".repeat(MAX_ARGUMENT_BYTES); MAX_ARGV_BYTES / MAX_ARGUMENT_BYTES + 1]; analysis.roles.resize(analysis.argv.len(), ArgumentRole::Data); }; "oversized_argv")]
    fn seed_rejects_inconsistent_or_unreviewable_host_facts(change: fn(&mut TemplateAnalysis)) {
        let (_temp, manager, provider, _publication) = manager();
        let source = install_current_analysis(&manager, &provider);
        change(provider.host.write().unwrap().analysis.as_mut().unwrap());
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        assert!(matches!(
            manager.seed_permission_template(&session, TOOL, &source, LABEL),
            Err(PermissionEditError::Invalid(_))
        ));
        assert_eq!(provider.analyses.load(Ordering::Relaxed), 1);
        assert!(manager.pattern_proposal_inventory().2.is_empty());
    }

    #[test_case(TemplateSessionChange::Project; "project_revision")]
    #[test_case(TemplateSessionChange::Authority; "registry_revision")]
    #[test_case(TemplateSessionChange::Manager; "other_session_manager")]
    #[test_case(TemplateSessionChange::PersistentState; "external_policy_snapshot")]
    fn seed_is_bound_to_the_current_edit_session(change: TemplateSessionChange) {
        let (temp, permissions, provider, publication) = manager();
        let source = install_current_analysis(&permissions, &provider);
        let session = permissions
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        let result = match change {
            TemplateSessionChange::Manager => {
                let (_other_temp, other, _provider, _publication) = manager();
                other.seed_permission_template(&session, TOOL, &source, LABEL)
            }
            change => {
                match change {
                    TemplateSessionChange::Project => permissions.set_project(temp.path()),
                    TemplateSessionChange::Authority => provider
                        .host
                        .write()
                        .unwrap()
                        .catalog
                        .revision
                        .push_str(OTHER_LABEL),
                    TemplateSessionChange::PersistentState => publication
                        .database
                        .lock()
                        .unwrap()
                        .state_set(
                            SCOPE_GLOBAL,
                            PERMISSION_RULES_STATE_KEY,
                            &Vec::<PermissionRuleRecord>::new(),
                        )
                        .unwrap(),
                    TemplateSessionChange::Manager => unreachable!(),
                }
                permissions.seed_permission_template(&session, TOOL, &source, LABEL)
            }
        };
        assert!(matches!(result, Err(PermissionEditError::Conflict)));
        assert_eq!(provider.analyses.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn seeded_context_is_reanalyzed_before_authority_preview() {
        let (_temp, manager, provider, _publication) = manager();
        let source = install_current_analysis(&manager, &provider);
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        let seeded = manager
            .seed_permission_template(&session, TOOL, &source, LABEL)
            .unwrap();
        let mut draft = draft();
        target(&mut draft).attributes.insert(
            WORKDIR.into(),
            SelectorDraft::Replace(SelectorValue::Exact(
                seeded.context.effective_workdir.clone(),
            )),
        );
        target(&mut draft).selector = SelectorDraft::Replace(SelectorValue::CommandTemplate {
            definition: Box::new(seeded),
            source: Some(source),
        });
        provider
            .host
            .write()
            .unwrap()
            .analysis
            .as_mut()
            .unwrap()
            .context
            .analysis_version = OTHER_LABEL.into();
        assert!(matches!(
            manager.preview_permission_edit(&session, &draft),
            Err(PermissionEditError::Invalid(_))
        ));
    }

    #[test_case(ArgumentDomain::ObservedSet { values: BTreeSet::from(["alpha".into(), "beta".into()]) }; "allowed_values")]
    #[test_case(ArgumentDomain::Exact { value: "alpha".into() }; "exact")]
    #[test_case(ArgumentDomain::Glob { pattern: "a*".into() }; "glob")]
    #[test_case(ArgumentDomain::Regex { pattern: "a.+".into() }; "regex")]
    #[test_case(ArgumentDomain::AnyLiteralArgument; "one_literal")]
    fn template_domain_matrix_uses_fresh_host_facts(domain: ArgumentDomain) {
        let provider = provider();
        install_analysis(&provider);
        let mut definition = definition();
        definition.slots[0].domain = domain;
        let mut draft = draft();
        target(&mut draft).selector = SelectorDraft::Replace(SelectorValue::CommandTemplate {
            definition: Box::new(definition),
            source: Some(TemplateSource {
                command: COMMAND.into(),
                workdir: PROJECT.into(),
            }),
        });
        assert!(normalize(&draft, None, &PermissionEditEvidence::default(), &provider).is_ok());
        assert_eq!(provider.analyses.load(Ordering::Relaxed), 1);
    }

    #[test_case(false; "structural_role_forgery")]
    #[test_case(true; "unproved_option_like_eligibility")]
    fn caller_cannot_assert_host_roles_or_option_eligibility(option_like: bool) {
        let provider = provider();
        install_analysis(&provider);
        let mut definition = definition();
        if option_like {
            definition.slots[0].option_like = OptionLikePolicy::AllowForProvenData;
        } else {
            definition.argv[1] = PatternToken::Exact {
                value: "show".into(),
                role: ArgumentRole::Data,
            };
        }
        let mut draft = draft();
        target(&mut draft).selector = SelectorDraft::Replace(SelectorValue::CommandTemplate {
            definition: Box::new(definition),
            source: Some(TemplateSource {
                command: COMMAND.into(),
                workdir: PROJECT.into(),
            }),
        });
        field_error(
            normalize(&draft, None, &PermissionEditEvidence::default(), &provider),
            EditField::Resource(0),
            None,
        );
    }

    #[test_case(false; "invalid_regex")]
    #[test_case(true; "invalid_glob")]
    fn expression_compile_errors_are_field_errors(glob: bool) {
        let provider = provider();
        install_analysis(&provider);
        let mut definition = definition();
        definition.slots[0].domain = if glob {
            ArgumentDomain::Glob {
                pattern: "[".into(),
            }
        } else {
            ArgumentDomain::Regex {
                pattern: "(".into(),
            }
        };
        let mut draft = draft();
        target(&mut draft).selector = SelectorDraft::Replace(SelectorValue::CommandTemplate {
            definition: Box::new(definition),
            source: Some(TemplateSource {
                command: COMMAND.into(),
                workdir: PROJECT.into(),
            }),
        });
        field_error(
            normalize(&draft, None, &PermissionEditEvidence::default(), &provider),
            EditField::Resource(0),
            None,
        );
    }

    #[derive(Clone, Serialize, Deserialize)]
    struct Message(String);

    impl TitleSource for Message {
        fn first_user_text(&self) -> Option<&str> {
            Some(&self.0)
        }
    }

    struct DatabasePublication {
        database: Mutex<SessionDatabase>,
        owner: PermissionOwner,
        fail: AtomicBool,
        lose_ack: AtomicBool,
        barriers: Mutex<Option<(Arc<Barrier>, Arc<Barrier>)>>,
        snapshot_barriers: Mutex<Option<(Arc<Barrier>, Arc<Barrier>)>>,
    }

    impl PermissionPublication for DatabasePublication {
        fn snapshot(&self) -> Result<PermissionSnapshot, PermissionEditError> {
            let snapshot = self
                .database
                .lock()
                .unwrap()
                .permission_snapshot(self.owner.clone())?;
            if let Some((entered, release)) = self.snapshot_barriers.lock().unwrap().take() {
                entered.wait();
                release.wait();
            }
            Ok(snapshot)
        }
        fn commit(
            &self,
            prepared: &PreparedPermissionMutation,
        ) -> Result<PermissionCommitReceipt, PermissionEditError> {
            if let Some((entered, release)) = self.barriers.lock().unwrap().clone() {
                entered.wait();
                release.wait();
            }
            if self.fail.swap(false, Ordering::Relaxed) {
                return Err(PermissionEditError::Storage(FAILURE.into()));
            }
            let receipt = self
                .database
                .lock()
                .unwrap()
                .commit_permission_mutation(prepared)?;
            if self.lose_ack.swap(false, Ordering::Relaxed) {
                return Err(PermissionEditError::Storage(LOST_ACK.into()));
            }
            Ok(receipt)
        }
        fn receipt(
            &self,
            id: CaudraId,
        ) -> Result<Option<PermissionCommitReceipt>, PermissionEditError> {
            Ok(self.database.lock().unwrap().permission_receipt(id)?)
        }
    }

    fn manager() -> (
        TempDir,
        Arc<PermissionManager>,
        Arc<TestProvider>,
        Arc<DatabasePublication>,
    ) {
        manager_with_storage(false)
    }

    fn manager_with_storage(
        split: bool,
    ) -> (
        TempDir,
        Arc<PermissionManager>,
        Arc<TestProvider>,
        Arc<DatabasePublication>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let directory = if split {
            StateDir::split(
                temp.path().join(VOLATILE_DIRECTORY),
                temp.path().join(PERSISTENT_DIRECTORY),
            )
        } else {
            StateDir::from_path(temp.path().join(PERSISTENT_DIRECTORY))
        };
        let mut database = SessionDatabase::open(&directory).unwrap();
        let session = Session::<Message, Value, Value>::new(TOOL, temp.path().to_str().unwrap());
        database.save(&session, None).unwrap();
        let publication = Arc::new(DatabasePublication {
            database: Mutex::new(database),
            owner: PermissionOwner::Conversation(session.id),
            fail: AtomicBool::new(false),
            lose_ack: AtomicBool::new(false),
            barriers: Mutex::new(None),
            snapshot_barriers: Mutex::new(None),
        });
        let manager = Arc::new(PermissionManager::new_persistent_in(
            PermissionsConfig::default(),
            temp.path().to_path_buf(),
            Arc::default(),
            directory,
        ));
        let provider = provider();
        manager
            .attach_permission_publication(publication.clone())
            .unwrap();
        manager.set_permission_authority_provider(provider.clone());
        (temp, manager, provider, publication)
    }

    fn create(manager: &PermissionManager) -> PermissionRuleRecord {
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        let preview = manager.preview_permission_edit(&session, &draft()).unwrap();
        let confirmation = preview.confirm(preview.requirements()).unwrap();
        manager
            .commit_permission_edit(&preview, &confirmation, &preview.draft)
            .unwrap();
        manager
            .structured_conversation_rules_snapshot()
            .into_iter()
            .find(PermissionRuleRecord::is_active)
            .unwrap()
    }

    fn copy_source(
        manager: &PermissionManager,
        provider: &TestProvider,
        persistent: bool,
    ) -> PermissionRuleRecord {
        if !persistent {
            return create(manager);
        }
        let mut rule = record(provider).rule;
        rule.lifetime = PermissionLifetime::Global;
        PermissionState::open(&manager.policy.as_ref().unwrap().state_dir)
            .unwrap()
            .insert(None, rule)
            .unwrap()
    }

    #[test_case(false; "storage_failure_publishes_nothing")]
    #[test_case(true; "lost_ack_recovers_receipt")]
    fn durable_acknowledgment_precedes_publication(lost_ack: bool) {
        let (_temp, manager, _provider, publication) = manager();
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        let preview = manager.preview_permission_edit(&session, &draft()).unwrap();
        let confirmation = preview.confirm(preview.requirements()).unwrap();
        publication.lose_ack.store(lost_ack, Ordering::Relaxed);
        publication.fail.store(!lost_ack, Ordering::Relaxed);
        let result = manager.commit_permission_edit(&preview, &confirmation, &preview.draft);
        assert_eq!(result.is_ok(), lost_ack);
        assert_eq!(
            manager.structured_conversation_rules_snapshot().len(),
            usize::from(lost_ack)
        );
        assert_eq!(
            manager.permission_edit_receipt(&preview).unwrap().is_some(),
            lost_ack
        );
        if !lost_ack {
            assert!(
                manager
                    .commit_permission_edit(&preview, &confirmation, &preview.draft)
                    .is_ok()
            );
        }
        assert_eq!(publication.snapshot().unwrap().records.len(), 1);
    }

    #[test_case(PermissionLifetime::Project; "project_receipt")]
    #[test_case(PermissionLifetime::Global; "global_receipt")]
    fn persistent_commit_receipt_requires_a_matching_fresh_snapshot(lifetime: PermissionLifetime) {
        let (_temp, manager, _provider, _publication) = manager();
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        let mut draft = draft();
        draft.project = if lifetime == PermissionLifetime::Project {
            ProjectDraft::Current
        } else {
            ProjectDraft::None
        };
        draft.lifetime = lifetime;
        let preview = manager.preview_permission_edit(&session, &draft).unwrap();
        let confirmation = preview.confirm(preview.requirements()).unwrap();
        let receipt = manager
            .commit_permission_edit(&preview, &confirmation, &draft)
            .unwrap();
        assert_eq!(
            manager.permission_edit_receipt(&preview).unwrap(),
            Some(receipt)
        );
        assert_eq!(
            PermissionState::open(&manager.policy.as_ref().unwrap().state_dir)
                .unwrap()
                .snapshot()
                .unwrap()
                .records,
            preview.prepared.targets()[0].records
        );
    }

    #[test_case(false; "corrupt_snapshot_after_commit")]
    #[test_case(true; "missing_snapshot_after_commit")]
    fn persistent_receipt_never_bypasses_a_failed_fresh_snapshot(missing: bool) {
        let (_temp, manager, _provider, publication) = manager();
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        let mut draft = draft();
        draft.lifetime = PermissionLifetime::Global;
        let preview = manager.preview_permission_edit(&session, &draft).unwrap();
        let mut state = PermissionState::open(&manager.policy.as_ref().unwrap().state_dir).unwrap();
        let database = publication.database.lock().unwrap();
        let receipt = database
            .commit_permission_mutation(&preview.prepared)
            .unwrap();
        if missing {
            database
                .state_delete(SCOPE_GLOBAL, PERMISSION_RULES_STATE_KEY)
                .unwrap();
        } else {
            database
                .state_set(
                    SCOPE_GLOBAL,
                    PERMISSION_RULES_STATE_KEY,
                    &INVALID_PERSISTENT_SNAPSHOT,
                )
                .unwrap();
        }
        assert_eq!(
            state.mutation_receipt(preview.operation_id()).unwrap(),
            Some(receipt.clone())
        );
        assert!(commit_persistent_permission_mutation(&mut state, &preview.prepared).is_err());
        assert!(validate_persistent_receipt(&state, &preview.prepared, receipt.clone()).is_err());
        assert!(state.records().is_empty());
        let mut current = preview.prepared.targets()[0].records.clone();
        current[0].revoked_at = Some(current[0].created_at + 1);
        database
            .state_set(SCOPE_GLOBAL, PERMISSION_RULES_STATE_KEY, &current)
            .unwrap();
        assert_eq!(
            commit_persistent_permission_mutation(&mut state, &preview.prepared).unwrap(),
            receipt
        );
        assert_eq!(state.records(), current);
        assert!(state.records().iter().all(|record| !record.is_active()));
        drop(database);
        assert_eq!(
            manager.permission_edit_receipt(&preview).unwrap(),
            Some(receipt)
        );
    }

    #[test_case(false; "concurrent_label_edit")]
    #[test_case(true; "revoke_wins_over_save")]
    fn full_source_cas_never_resurrects_a_stale_edit(revoke: bool) {
        let (_temp, manager, _provider, _publication) = manager();
        let original = create(&manager);
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Replace(original.id.clone()),
                PermissionEditEvidence::default(),
            )
            .unwrap();
        let mut first = session.draft();
        first.label = Some(LABEL.into());
        let first = manager.preview_permission_edit(&session, &first).unwrap();
        let first_confirmation = first.confirm(first.requirements()).unwrap();
        if revoke {
            manager.revoke_structured_rule(&original.id).unwrap();
        } else {
            let mut second = session.draft();
            second.label = Some(OTHER_LABEL.into());
            let second = manager.preview_permission_edit(&session, &second).unwrap();
            let confirmation = second.confirm(second.requirements()).unwrap();
            manager
                .commit_permission_edit(&second, &confirmation, &second.draft)
                .unwrap();
        }
        assert!(matches!(
            manager.commit_permission_edit(&first, &first_confirmation, &first.draft),
            Err(PermissionEditError::Conflict)
        ));
        let records = manager.structured_conversation_rules_snapshot();
        assert!(
            !records
                .iter()
                .find(|record| record.id == original.id)
                .unwrap()
                .is_active()
        );
        assert!(
            !records
                .iter()
                .any(|record| record.label.as_deref() == Some(LABEL))
        );
    }

    #[test_case(false; "context_change")]
    #[test_case(true; "registry_revision_change")]
    fn confirmation_is_bound_to_context_and_provider_revision(registry: bool) {
        let (temp, manager, provider, _publication) = manager();
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        let preview = manager.preview_permission_edit(&session, &draft()).unwrap();
        let confirmation = preview.confirm(preview.requirements()).unwrap();
        if registry {
            provider
                .host
                .write()
                .unwrap()
                .catalog
                .revision
                .push_str(OTHER_LABEL);
        } else {
            manager.set_project(temp.path());
        }
        assert!(matches!(
            manager.commit_permission_edit(&preview, &confirmation, &preview.draft),
            Err(PermissionEditError::Conflict)
        ));
        assert!(manager.structured_conversation_rules_snapshot().is_empty());
    }

    #[test]
    fn replacing_the_provider_invalidates_confirmation_even_with_an_equal_catalog() {
        let (_temp, manager, provider, publication) = manager();
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        let preview = manager.preview_permission_edit(&session, &draft()).unwrap();
        let confirmation = preview.confirm(preview.requirements()).unwrap();
        manager.set_permission_authority_provider(provider);
        assert!(matches!(
            manager.commit_permission_edit(&preview, &confirmation, &preview.draft),
            Err(PermissionEditError::Conflict)
        ));
        assert!(publication.snapshot().unwrap().records.is_empty());
    }

    #[test_case(PermissionLifetime::Project; "conversation_to_project")]
    #[test_case(PermissionLifetime::Global; "conversation_to_global")]
    fn same_database_move_is_one_reviewed_mutation(lifetime: PermissionLifetime) {
        let (_temp, manager, _provider, publication) = manager();
        let original = create(&manager);
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Replace(original.id.clone()),
                PermissionEditEvidence {
                    input: None,
                    values: vec![PROJECT.into()],
                },
            )
            .unwrap();
        let mut draft = session.draft();
        draft.lifetime = lifetime.clone();
        draft.project = if lifetime == PermissionLifetime::Project {
            ProjectDraft::Current
        } else {
            ProjectDraft::None
        };
        let preview = manager.preview_permission_edit(&session, &draft).unwrap();
        let confirmation = preview.confirm(preview.requirements()).unwrap();
        manager
            .commit_permission_edit(&preview, &confirmation, &preview.draft)
            .unwrap();
        assert!(
            publication
                .snapshot()
                .unwrap()
                .records
                .iter()
                .all(|record| !record.is_active())
        );
        let persistent = publication
            .database
            .lock()
            .unwrap()
            .permission_snapshot(PermissionOwner::Persistent)
            .unwrap();
        assert_eq!(
            persistent
                .records
                .iter()
                .filter(|record| record.is_active())
                .count(),
            1
        );
        assert_eq!(persistent.records.last().unwrap().rule.lifetime, lifetime);
        assert_eq!(
            persistent
                .records
                .last()
                .unwrap()
                .replaces
                .as_ref()
                .unwrap()
                .record_id,
            original.id
        );
    }

    #[test_case(PermissionEditOperation::Duplicate(String::new()); "duplicate")]
    #[test_case(PermissionEditOperation::Copy(String::new()); "copy")]
    fn copying_never_chains_a_revocation(operation: PermissionEditOperation) {
        let (_temp, manager, _provider, _publication) = manager();
        let original = create(&manager);
        let operation = if matches!(operation, PermissionEditOperation::Copy(_)) {
            PermissionEditOperation::Copy(original.id.clone())
        } else {
            PermissionEditOperation::Duplicate(original.id.clone())
        };
        let session = manager
            .begin_permission_edit(
                operation,
                PermissionEditEvidence {
                    input: None,
                    values: vec![PROJECT.into()],
                },
            )
            .unwrap();
        let preview = manager
            .preview_permission_edit(&session, &session.draft())
            .unwrap();
        let confirmation = preview.confirm(preview.requirements()).unwrap();
        manager
            .commit_permission_edit(&preview, &confirmation, &preview.draft)
            .unwrap();
        let records = manager.structured_conversation_rules_snapshot();
        assert_eq!(
            records.iter().filter(|record| record.is_active()).count(),
            2
        );
        assert_eq!(
            records.iter().find(|record| record.id == original.id),
            Some(&original)
        );
    }

    #[test_case(false, false, false; "duplicate_conversation_source_unchanged")]
    #[test_case(false, true, false; "copy_conversation_source_unchanged")]
    #[test_case(true, false, false; "duplicate_persistent_source_unchanged")]
    #[test_case(true, true, false; "copy_persistent_source_unchanged")]
    #[test_case(false, false, true; "duplicate_conversation_source_revoked")]
    #[test_case(false, true, true; "copy_conversation_source_revoked")]
    #[test_case(true, false, true; "duplicate_persistent_source_revoked")]
    #[test_case(true, true, true; "copy_persistent_source_revoked")]
    fn same_database_copy_checks_source_in_the_writer_transaction(
        persistent_source: bool,
        copy: bool,
        revoke: bool,
    ) {
        let (_temp, manager, provider, publication) = manager();
        let original = copy_source(&manager, &provider, persistent_source);
        let session = manager
            .begin_permission_edit(
                if copy {
                    PermissionEditOperation::Copy(original.id.clone())
                } else {
                    PermissionEditOperation::Duplicate(original.id.clone())
                },
                PermissionEditEvidence {
                    input: None,
                    values: vec![PROJECT.into()],
                },
            )
            .unwrap();
        let mut draft = session.draft();
        draft.lifetime = if persistent_source {
            PermissionLifetime::Conversation
        } else {
            PermissionLifetime::Global
        };
        let preview = manager.preview_permission_edit(&session, &draft).unwrap();
        let confirmation = preview.confirm(preview.requirements()).unwrap();
        let source = session.source_identity().unwrap();
        let source_snapshot = session
            .snapshots
            .iter()
            .find(|snapshot| snapshot.revision.owner == source.owner)
            .unwrap();
        let destination_snapshot = session
            .snapshots
            .iter()
            .find(|snapshot| snapshot.revision.owner != source.owner)
            .unwrap();
        assert!(preview.prepared.expected().contains(source_snapshot));
        assert!(!preview.prepared.persistent_only());
        assert!(
            preview
                .prepared
                .targets()
                .iter()
                .all(|snapshot| snapshot.revision.owner != source.owner)
        );
        let external = SessionDatabase::open(&manager.policy.as_ref().unwrap().state_dir).unwrap();
        let revocation = prepare_mutation(
            vec![source_snapshot.clone()],
            PermissionMutation::Revoke {
                source: source.clone(),
            },
        )
        .unwrap();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        *publication.barriers.lock().unwrap() = Some((entered.clone(), release.clone()));
        let result = thread::scope(|scope| {
            let worker =
                scope.spawn(|| manager.commit_permission_edit(&preview, &confirmation, &draft));
            entered.wait();
            let revoked = revoke.then(|| external.commit_permission_mutation(&revocation));
            release.wait();
            if let Some(revoked) = revoked {
                revoked.unwrap();
            }
            worker.join().unwrap()
        });
        let current_source = external.permission_snapshot(source.owner.clone()).unwrap();
        let current_destination = external
            .permission_snapshot(destination_snapshot.revision.owner.clone())
            .unwrap();
        if revoke {
            assert!(matches!(result, Err(PermissionEditError::Conflict)));
            assert_eq!(&current_destination, destination_snapshot);
            assert!(
                !current_source
                    .records
                    .iter()
                    .find(|record| record.id == original.id)
                    .unwrap()
                    .is_active()
            );
            assert!(
                external
                    .permission_receipt(preview.operation_id())
                    .unwrap()
                    .is_none()
            );
        } else {
            let receipt = result.unwrap();
            assert_eq!(&current_source, source_snapshot);
            assert_eq!(receipt.revisions, vec![current_destination.revision]);
            assert_eq!(
                current_destination.records,
                preview.prepared.targets()[0].records
            );
        }
    }

    #[test_case(false; "conversation_to_persistent")]
    #[test_case(true; "persistent_to_conversation")]
    fn split_database_copy_is_explicit_and_never_changes_the_source(persistent_source: bool) {
        let (_temp, manager, provider, _publication) = manager_with_storage(true);
        let original = copy_source(&manager, &provider, persistent_source);
        let evidence = PermissionEditEvidence {
            input: None,
            values: vec![PROJECT.into()],
        };
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Duplicate(original.id.clone()),
                evidence.clone(),
            )
            .unwrap();
        let mut draft = session.draft();
        draft.lifetime = if persistent_source {
            PermissionLifetime::Conversation
        } else {
            PermissionLifetime::Global
        };
        assert!(matches!(
            manager.preview_permission_edit(&session, &draft),
            Err(PermissionEditError::Unavailable(_))
        ));
        let session = manager
            .begin_permission_edit(PermissionEditOperation::Copy(original.id.clone()), evidence)
            .unwrap();
        let preview = manager.preview_permission_edit(&session, &draft).unwrap();
        let source = session.source_identity().unwrap();
        let source_snapshot = session
            .snapshots
            .iter()
            .find(|snapshot| snapshot.revision.owner == source.owner)
            .unwrap();
        let destination_snapshot = session
            .snapshots
            .iter()
            .find(|snapshot| snapshot.revision.owner != source.owner)
            .unwrap();
        assert_eq!(preview.prepared.expected(), from_ref(destination_snapshot));
        assert!(
            preview
                .requirements()
                .contains(&ConfirmationRequirement::CopyLeavesSource)
        );
        let confirmation = preview.confirm(preview.requirements()).unwrap();
        manager
            .commit_permission_edit(&preview, &confirmation, &draft)
            .unwrap();
        let current = manager.durable_permission_snapshots().unwrap();
        assert_eq!(
            current
                .iter()
                .find(|snapshot| snapshot.revision.owner == source.owner),
            Some(source_snapshot)
        );
        assert_eq!(
            current
                .iter()
                .find(|snapshot| snapshot.revision.owner == destination_snapshot.revision.owner)
                .unwrap()
                .records,
            preview.prepared.targets()[0].records
        );
    }

    /// Saves one command for the project and another for this conversation,
    /// and reports whether the answer committed and which lifetimes it left.
    fn save_mixed_lifetimes(
        manager: &PermissionManager,
        publication: &DatabasePublication,
        project: &Path,
        fail_conversation: bool,
    ) -> (bool, Vec<PermissionLifetime>) {
        let request = shell_request(&[COMMAND, OTHER_COMMAND], workcell_shell_subject());
        let row = |index: usize, lifetime| {
            Some(ComposedRow {
                grant: PermissionRowGrant::Offered(format!("{COMMAND_EXACT_PREFIX}{index}")),
                lifetime,
            })
        };
        let answer = PermissionAnswer::AllowComposed {
            rows: vec![
                row(0, PermissionLifetime::Project),
                row(1, PermissionLifetime::Conversation),
            ],
        };
        publication.fail.store(fail_conversation, Ordering::Relaxed);
        let committed = manager
            .commit_structured_decision(&request, &answer, Some(project), false)
            .is_ok();
        let mut lifetimes: Vec<_> = manager
            .structured_rule_inventory()
            .unwrap()
            .into_iter()
            .map(|record| record.rule.lifetime)
            .collect();
        lifetimes.sort();
        (committed, lifetimes)
    }

    /// One database commits both owners in one transaction, so a failure
    /// saves neither.
    #[test_case(false; "commits")]
    #[test_case(true; "a_failure_saves_neither")]
    fn mixed_lifetimes_commit_in_one_transaction(fail: bool) {
        let (_temp, manager, _provider, publication) = manager_with_storage(false);
        let (committed, lifetimes) =
            save_mixed_lifetimes(&manager, &publication, &manager.project_cwd(), fail);
        assert_eq!(committed, !fail);
        assert_eq!(
            lifetimes,
            if fail {
                &[][..]
            } else {
                &CONVERSATION_AND_PROJECT[..]
            }
        );
    }

    #[test]
    fn mixed_lifetimes_save_project_rules_first() {
        let (_temp, manager, _provider, publication) = manager_with_storage(true);
        assert_eq!(
            save_mixed_lifetimes(&manager, &publication, &manager.project_cwd(), false),
            (true, CONVERSATION_AND_PROJECT.to_vec())
        );
    }

    #[test]
    fn failed_conversation_save_revokes_new_project_rules() {
        let (_temp, manager, _provider, publication) = manager_with_storage(true);
        assert_eq!(
            save_mixed_lifetimes(&manager, &publication, &manager.project_cwd(), true),
            (false, Vec::new())
        );
    }

    /// Storage refuses a project rule whose project is not absolute, so the
    /// project save fails where a conversation-first order would already
    /// have committed the conversation row.
    #[test]
    fn failed_project_save_leaves_conversation_untouched() {
        let (_temp, manager, _provider, publication) = manager_with_storage(true);
        let conversation = publication.snapshot().unwrap();
        assert_eq!(
            save_mixed_lifetimes(&manager, &publication, Path::new(RELATIVE_PROJECT), false),
            (false, Vec::new())
        );
        assert_eq!(publication.snapshot().unwrap(), conversation);
    }

    #[test_case(StructuredPermissionEffect::Deny, false; "deny_persistent_to_conversation")]
    #[test_case(StructuredPermissionEffect::Deny, true; "deny_conversation_to_persistent")]
    #[test_case(StructuredPermissionEffect::Ask, false; "ask_persistent_to_conversation")]
    #[test_case(StructuredPermissionEffect::Ask, true; "ask_conversation_to_persistent")]
    fn evaluation_keeps_restrictive_rules_during_external_owner_moves(
        effect: StructuredPermissionEffect,
        to_persistent: bool,
    ) {
        let (_temp, manager, provider, publication) = manager();
        manager.configured.write().unwrap().default = DefaultEffect::Allow;
        let request = shell_request(&[COMMAND], workcell_shell_subject());
        let context = EvaluationContext {
            revision: *manager.context_revision.read().unwrap(),
            plan_scoped: false,
            builtin_allows: false,
            force_prompt: false,
            forced: false,
            exact_plan: None,
        };
        assert!(
            manager
                .current_policy(&request, &context)
                .unwrap()
                .automatic
        );
        let mut draft = draft();
        draft.effect = effect.clone();
        target(&mut draft).attributes.insert(
            WORKDIR.into(),
            SelectorDraft::Replace(SelectorValue::Exact(SHELL_WORKDIR.into())),
        );
        let mut original = PermissionRuleRecord::conversation(
            normalize(&draft, None, &PermissionEditEvidence::default(), &provider)
                .unwrap()
                .rule,
        )
        .unwrap();
        let (source_owner, destination) = if to_persistent {
            (publication.owner.clone(), PermissionOwner::Persistent)
        } else {
            original.rule.lifetime = PermissionLifetime::Global;
            (PermissionOwner::Persistent, publication.owner.clone())
        };
        let external = SessionDatabase::open(&manager.policy.as_ref().unwrap().state_dir).unwrap();
        external
            .commit_permission_mutation(
                &prepare_mutation(
                    vec![external.permission_snapshot(source_owner.clone()).unwrap()],
                    PermissionMutation::Create {
                        destination: source_owner.clone(),
                        records: Box::new([original.clone()]),
                    },
                )
                .unwrap(),
            )
            .unwrap();
        manager.refresh_permission_state().unwrap();
        let mut replacement = original.clone();
        replacement.id = CaudraId::generate().to_string();
        replacement.rule.lifetime = if to_persistent {
            PermissionLifetime::Global
        } else {
            PermissionLifetime::Conversation
        };
        let movement = prepare_mutation(
            external
                .permission_snapshots(&[source_owner.clone(), destination.clone()])
                .unwrap(),
            PermissionMutation::Replace {
                source: PermissionRecordIdentity {
                    owner: source_owner,
                    record_id: original.id,
                },
                destination,
                replacement: Box::new(replacement),
            },
        )
        .unwrap();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        *publication.snapshot_barriers.lock().unwrap() = Some((entered.clone(), release.clone()));
        let result = thread::scope(|scope| {
            let evaluator = scope.spawn(|| manager.current_policy(&request, &context));
            entered.wait();
            let moved = external.commit_permission_mutation(&movement);
            release.wait();
            moved.unwrap();
            evaluator.join().unwrap()
        });
        if effect == StructuredPermissionEffect::Deny {
            assert_eq!(result.err().unwrap().0, CURRENT_POLICY_DENIES_REQUEST);
        } else {
            let policy = result.unwrap();
            assert!(!policy.automatic);
            assert!(policy.coverage.must_prompt);
        }
        assert_eq!(
            manager.conversation_permission_snapshot().unwrap(),
            external
                .permission_snapshot(publication.owner.clone())
                .unwrap()
        );
    }

    #[test]
    fn durable_wait_holds_no_pending_lock_and_cannot_publish_early() {
        let (_temp, manager, provider, publication) = manager();
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        let preview = manager.preview_permission_edit(&session, &draft()).unwrap();
        let confirmation = preview.confirm(preview.requirements()).unwrap();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        *publication.barriers.lock().unwrap() = Some((entered.clone(), release.clone()));
        thread::scope(|scope| {
            let worker = scope
                .spawn(|| manager.commit_permission_edit(&preview, &confirmation, &preview.draft));
            entered.wait();
            assert!(manager.broker.pending.try_lock().is_ok());
            assert!(manager.structured_conversation_rules().is_empty());
            assert!(publication.snapshot().unwrap().records.is_empty());
            let context_fenced = manager.context_revision.try_write().is_err();
            let authority_fenced = provider.host.try_write().is_err();
            let plugins_fenced = manager.plugin_rules.edit_revision.try_write().is_err();
            let mutation_fenced = manager.broker.mutation_gate.try_lock().is_err();
            release.wait();
            worker.join().unwrap().unwrap();
            assert!(context_fenced);
            assert!(authority_fenced);
            assert!(plugins_fenced);
            assert!(mutation_fenced);
        });
        assert_eq!(manager.structured_conversation_rules_snapshot().len(), 1);
    }

    #[test]
    fn all_project_inventory_keeps_inactive_project_rules_visible() {
        let (temp, manager, _provider, _publication) = manager();
        let other = temp.path().join(OTHER_LABEL);
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        let mut draft = draft();
        draft.lifetime = PermissionLifetime::Project;
        draft.project = ProjectDraft::Explicit(other);
        let preview = manager.preview_permission_edit(&session, &draft).unwrap();
        let confirmation = preview.confirm(preview.requirements()).unwrap();
        manager
            .commit_permission_edit(&preview, &confirmation, &preview.draft)
            .unwrap();
        assert!(manager.structured_rule_inventory().unwrap().is_empty());
        assert_eq!(
            manager
                .structured_rule_inventory_filtered(&PermissionProjectFilter::All, false)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn unsupported_family_is_not_inferred_from_a_tool_name() {
        let mut draft = draft();
        draft.identity = IdentityDraft::Registered {
            key: TOOL.into(),
            family: Some(PermissionCapabilityFamily::FilesystemRead),
        };
        field_error(
            normalize(
                &draft,
                None,
                &PermissionEditEvidence::default(),
                &provider(),
            ),
            EditField::Identity,
            None,
        );
    }

    #[test_case(PermissionCapabilityFamily::FilesystemRead; "filesystem_read")]
    #[test_case(PermissionCapabilityFamily::FilesystemBrowse; "names_only_browse")]
    #[test_case(PermissionCapabilityFamily::McpServer; "mcp_server_family")]
    fn family_matrix_uses_registered_contracts_and_storage_validation(
        family: PermissionCapabilityFamily,
    ) {
        let provider = provider();
        let mut draft = draft();
        draft.identity = IdentityDraft::Registered {
            key: TOOL.into(),
            family: Some(family),
        };
        let mut host = provider.host.write().unwrap();
        let authority = &mut host.catalog.authorities[0];
        authority.families = vec![family];
        if family == PermissionCapabilityFamily::McpServer {
            authority.source = TrustedToolSource::from_mcp_binding(PermissionSubject::Mcp {
                server: TOOL.into(),
                authority: TOOL.into(),
                tool: TOOL.into(),
                contract: TOOL.into(),
            })
            .unwrap();
            draft.resources = ResourcesDraft::Unrestricted;
        } else {
            let contract = if family == PermissionCapabilityFamily::FilesystemBrowse {
                "file.glob.v1"
            } else {
                "file.read.v1"
            };
            let tool = RegisteredTool {
                tool: Arc::new(NeverInvoked),
                source: ToolSource::Native {
                    owner: "workcell".into(),
                    contract: contract.into(),
                    trusted: true,
                },
                effect: ToolEffect::ReadOnly,
            };
            authority.source = TrustedToolSource::from_registered(&tool, None).unwrap();
            let resource = target(&mut draft);
            resource.kind = PermissionResourceKind::Directory;
            resource.selector =
                SelectorDraft::Replace(SelectorValue::FilesystemSubtree(PROJECT.into()));
            resource.attributes.clear();
            resource.access =
                GuardDraft::Equals(if family == PermissionCapabilityFamily::FilesystemBrowse {
                    PermissionResourceAccess::List
                } else {
                    PermissionResourceAccess::Read
                });
            if family == PermissionCapabilityFamily::FilesystemBrowse {
                let name = BROWSE_RECURSION_ATTRIBUTE;
                resource.attributes.insert(
                    name.into(),
                    SelectorDraft::Replace(SelectorValue::Exact(BROWSE_RECURSIVE.into())),
                );
                authority
                    .resources
                    .iter_mut()
                    .find(|resource| resource.kind == PermissionResourceKind::Directory)
                    .unwrap()
                    .attributes
                    .insert(name.into(), vec![SelectorMode::Exact]);
            }
        }
        drop(host);
        let result =
            normalize(&draft, None, &PermissionEditEvidence::default(), &provider).unwrap();
        assert_eq!(result.rule.family, Some(family));
        if family != PermissionCapabilityFamily::McpServer {
            target(&mut draft).access = GuardDraft::Equals(PermissionResourceAccess::Write);
            field_error(
                normalize(&draft, None, &PermissionEditEvidence::default(), &provider),
                EditField::Rule,
                None,
            );
        }
    }

    #[test_case(false; "remote_exact")]
    #[test_case(true; "remote_subtree")]
    fn remote_scope_edits_keep_bound_authority_and_principal(subtree: bool) {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("https://example.test").unwrap(),
            TOOL,
            TOOL,
            TOOL,
            TOOL,
        )
        .unwrap();
        let identity = RemotePermissionIdentity {
            principal: AuthenticatedPrincipalId::new(authority.clone(), TOOL).unwrap(),
            project: ProjectIdentity::new(authority.clone(), ProjectKey::new(TOOL).unwrap()),
            authority,
        };
        let kind = PermissionResourceKind::RemoteDirectory {
            identity: identity.clone(),
        };
        let value = if subtree {
            SelectorValue::RemoteSubtree(vec![PROJECT.into()])
        } else {
            SelectorValue::RemoteExact(vec![PROJECT.into()])
        };
        let selector = super::review::compile_editor_selector(&kind, &value).unwrap();
        assert_eq!(verified_selector_value(&selector, &kind, &[]), Some(value));
        let mut other = identity;
        other.principal =
            AuthenticatedPrincipalId::new(other.authority.clone(), OTHER_LABEL).unwrap();
        assert!(
            verified_selector_value(
                &selector,
                &PermissionResourceKind::RemoteDirectory { identity: other },
                &[]
            )
            .is_none()
        );
    }

    #[test_case(""; "empty_pointer")]
    #[test_case("value"; "not_a_pointer")]
    #[test_case("/bad~2escape"; "invalid_escape")]
    fn selected_argument_pointer_validation_is_shared(pointer: &str) {
        let mut draft = draft();
        draft.arguments = ArgumentsDraft::Selected {
            input: json!({}),
            pointers: vec![pointer.into()],
        };
        field_error(
            normalize(
                &draft,
                None,
                &PermissionEditEvidence::default(),
                &provider(),
            ),
            EditField::Arguments,
            None,
        );
    }

    #[test_case(false; "repeated_slot_equality")]
    #[test_case(true; "listed_tuple_membership")]
    fn template_relationships_are_validated_not_inferred(tuples: bool) {
        let provider = provider();
        install_analysis(&provider);
        let mut definition = definition();
        if tuples {
            definition.combinations = SlotCombinations::ObservedTuples {
                tuples: BTreeSet::from([BTreeMap::from([(SlotId(1), "unlisted".into())])]),
            };
        } else {
            definition.argv.push(PatternToken::Slot {
                id: SlotId(1),
                role: ArgumentRole::Data,
            });
            let mut host = provider.host.write().unwrap();
            let analysis = host.analysis.as_mut().unwrap();
            analysis.argv.push("beta".into());
            analysis.roles.push(ArgumentRole::Data);
        }
        let mut draft = draft();
        target(&mut draft).selector = SelectorDraft::Replace(SelectorValue::CommandTemplate {
            definition: Box::new(definition),
            source: Some(TemplateSource {
                command: COMMAND.into(),
                workdir: PROJECT.into(),
            }),
        });
        field_error(
            normalize(&draft, None, &PermissionEditEvidence::default(), &provider),
            EditField::Resource(0),
            None,
        );
    }

    #[test_case(false; "pattern_name")]
    #[test_case(true; "slot_label")]
    fn template_labels_do_not_require_reanalysis_or_input_rebinding(slot: bool) {
        let provider = provider();
        install_analysis(&provider);
        let mut template_draft = draft();
        template_draft.arguments = ArgumentsDraft::Exact(json!({"command": COMMAND}));
        target(&mut template_draft).selector =
            SelectorDraft::Replace(SelectorValue::CommandTemplate {
                definition: Box::new(definition()),
                source: Some(TemplateSource {
                    command: COMMAND.into(),
                    workdir: PROJECT.into(),
                }),
            });
        let normalized = normalize(
            &template_draft,
            None,
            &PermissionEditEvidence::default(),
            &provider,
        )
        .unwrap();
        let original = PermissionRuleRecord::conversation(normalized.rule).unwrap();
        let mut definition = definition();
        if slot {
            definition.slots[0].label = OTHER_LABEL.into();
        } else {
            definition.name = OTHER_LABEL.into();
        }
        let mut edit = PermissionRuleDraft::from_record(&original, None);
        target(&mut edit).selector = SelectorDraft::Replace(SelectorValue::CommandTemplate {
            definition: Box::new(definition),
            source: None,
        });
        let result = normalize(
            &edit,
            Some(&original),
            &PermissionEditEvidence::default(),
            &provider,
        )
        .unwrap();
        assert_eq!(
            classify_authority_change(Some(&original.rule), &result.rule, None, None),
            AuthorityChange::Equivalent
        );
        assert_eq!(result.rule.arguments, original.rule.arguments);
        assert_eq!(provider.analyses.load(Ordering::Relaxed), 1);
    }

    #[test_case("\u{1b}[31m"; "terminal_control")]
    #[test_case("a\u{202e}b"; "bidi_control")]
    #[test_case("   "; "blank_label")]
    fn labels_cannot_embed_display_controls(label: &str) {
        let provider = provider();
        let original = record(&provider);
        let mut edit = PermissionRuleDraft::from_record(&original, None);
        edit.label = Some(label.into());
        field_error(
            normalize(
                &edit,
                Some(&original),
                &PermissionEditEvidence::default(),
                &provider,
            ),
            EditField::Label,
            None,
        );
    }

    #[test_case(false; "unchanged_local_source")]
    #[test_case(true; "changed_local_source")]
    fn source_locator_verifies_loaded_bytes_not_a_review_label(changed: bool) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().canonicalize().unwrap().join(TOOL);
        std::fs::write(&path, LABEL).unwrap();
        let digest = hex_encode(&Sha256::digest(LABEL.as_bytes()));
        let locator = VerifiedLocalSourceLocator::from_loaded_file(&path, &digest).unwrap();
        if changed {
            std::fs::write(&path, OTHER_LABEL).unwrap();
        }
        assert_eq!(locator.verify_current().is_ok(), !changed);
    }

    #[test_case(DefaultEffect::Prompt, None, true, false, PlanAccess::Write; "verified_plan_is_allowed")]
    #[test_case(DefaultEffect::Prompt, None, false, false, PlanAccess::Write; "name_without_verified_target_does_not_approve")]
    #[test_case(DefaultEffect::Prompt, Some(Effect::Deny), true, false, PlanAccess::Write; "deny_blocks_verified_plan")]
    #[test_case(DefaultEffect::Prompt, Some(Effect::Ask), true, false, PlanAccess::Write; "ask_prompts_verified_plan")]
    #[test_case(DefaultEffect::Deny, None, true, false, PlanAccess::Write; "default_deny_blocks_verified_plan")]
    #[test_case(DefaultEffect::Allow, None, true, true, PlanAccess::Write; "malformed_intent_is_not_shown_allowed")]
    #[test_case(DefaultEffect::Prompt, None, true, false, PlanAccess::Read; "verified_read_is_allowed")]
    #[test_case(DefaultEffect::Deny, None, true, false, PlanAccess::Read; "default_deny_allows_verified_read")]
    #[test_case(DefaultEffect::Prompt, None, false, false, PlanAccess::Read; "read_without_verified_target_prompts")]
    #[test_case(DefaultEffect::Allow, Some(Effect::Deny), true, false, PlanAccess::Read; "deny_blocks_verified_read")]
    #[test_case(DefaultEffect::Deny, Some(Effect::Ask), true, false, PlanAccess::Read; "ask_prompts_verified_read")]
    #[test_case(DefaultEffect::Allow, None, true, true, PlanAccess::Read; "malformed_read_is_not_shown_allowed")]
    fn active_plan_example_preview_preserves_policy(
        default: DefaultEffect,
        effect: Option<Effect>,
        verified: bool,
        malformed: bool,
        access: PlanAccess,
    ) {
        smol::block_on(async {
            for remote in [false, true] {
                let (_temp, manager, provider, _publication) = manager();
                let (_root, ctx, mut intent, mut input) = active_plan_fixture(remote).await;
                if access == PlanAccess::Read {
                    (intent, input) = active_plan_read(&ctx).await;
                }
                manager.set_project_with_config(
                    ctx.host_cwd.as_ref().unwrap(),
                    PermissionsConfig {
                        default,
                        rules: effect
                            .into_iter()
                            .map(|effect| PermissionRule {
                                tool: ToolKey::native(plan::NAME),
                                scope: Some(intent.resources[0].value.clone()),
                                effect,
                            })
                            .collect(),
                        ..Default::default()
                    },
                );
                if malformed {
                    intent.resources[0].access = Some(match access {
                        PlanAccess::Read => PermissionResourceAccess::Write,
                        PlanAccess::Write => PermissionResourceAccess::Read,
                    });
                }
                let registered = RegisteredTool {
                    tool: Arc::new(PlanTool),
                    source: ToolSource::Native {
                        owner: "caudra".into(),
                        contract: "plan/v1".into(),
                        trusted: true,
                    },
                    effect: ToolEffect::Mutating,
                };
                {
                    let mut host = provider.host.write().unwrap();
                    host.catalog.authorities.push(EditableAuthorityDescriptor {
                        key: plan::NAME.into(),
                        source: TrustedToolSource::from_registered(&registered, None).unwrap(),
                        resources: Vec::new(),
                        arguments: vec![ArgumentMode::Exact],
                        families: Vec::new(),
                        unrestricted_resources: false,
                        unavailable: None,
                    });
                    host.plan_example = Some((
                        intent,
                        verified.then(|| plan::verified_target(&ctx).unwrap()),
                    ));
                }
                let session = manager
                    .begin_permission_edit(
                        PermissionEditOperation::Create,
                        PermissionEditEvidence::default(),
                    )
                    .unwrap();
                let preview = manager.preview_permission_edit(&session, &draft()).unwrap();
                let result = manager.preview_permission_example(&preview, plan::NAME, &input);
                if malformed {
                    assert!(matches!(result, Err(PermissionEditError::Invalid(_))));
                    continue;
                }
                let result = result.unwrap();
                assert!(!result.matches_rule);
                let verified_read = verified && access == PlanAccess::Read;
                let expected = match effect {
                    Some(Effect::Deny) => EffectivePolicyPreview::Denied(
                        PermissionPolicyError(CURRENT_POLICY_DENIES_REQUEST.into()).to_string(),
                    ),
                    Some(Effect::Ask) => EffectivePolicyPreview::Prompt,
                    _ if default == DefaultEffect::Deny && !verified_read => {
                        EffectivePolicyPreview::Denied(
                            PermissionPolicyError(CURRENT_DEFAULT_DENIES_REQUEST.into())
                                .to_string(),
                        )
                    }
                    _ if !verified => EffectivePolicyPreview::Prompt,
                    _ => EffectivePolicyPreview::AllowedByPolicy,
                };
                assert_eq!(result.effective_policy, expected);
            }
        });
    }

    #[test_case(false, false, None; "allow_matches_and_allows")]
    #[test_case(true, false, None; "forced_prompt_stays_forced")]
    #[test_case(false, true, None; "plan_blocks_persistent_allow")]
    #[test_case(false, false, Some(Effect::Ask); "other_ask_remains_effective")]
    #[test_case(false, false, Some(Effect::Deny); "other_deny_remains_effective")]
    fn matches_rule_is_separate_from_effective_pending_policy(
        forced: bool,
        plan: bool,
        restrictive: Option<Effect>,
    ) {
        let (_temp, manager, _provider, _publication) = manager();
        if let Some(effect) = restrictive {
            manager.plugin_rules.replace(
                TOOL,
                vec![PermissionRule {
                    tool: ToolKey::native("shell"),
                    scope: Some(COMMAND_PATTERN.into()),
                    effect,
                }],
            );
        }
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        let mut draft = draft();
        draft.lifetime = PermissionLifetime::Global;
        target(&mut draft).attributes.insert(
            WORKDIR.into(),
            SelectorDraft::Replace(SelectorValue::Exact(SHELL_WORKDIR.into())),
        );
        let preview = manager.preview_permission_edit(&session, &draft).unwrap();
        let request = shell_request(&[COMMAND], workcell_shell_subject());
        let id = request.id.clone();
        let (sender, _answer) = flume::bounded(1);
        let (changed, _changes) = flume::bounded(1);
        let revision = *manager.context_revision.read().unwrap();
        manager.pending().entry(manager.id).or_default().insert(
            id.clone(),
            PendingPermission {
                request,
                evaluation: Some(EvaluationContext {
                    revision,
                    plan_scoped: plan,
                    builtin_allows: false,
                    force_prompt: forced,
                    forced,
                    exact_plan: None,
                }),
                project: None,
                context_revision: revision,
                answering: false,
                abandoned: false,
                cancel: CancelToken::none(),
                changed,
                sender,
            },
        );
        let result = manager
            .preview_pending_permission_match(&preview, &id)
            .unwrap();
        assert!(result.matches_rule);
        if restrictive == Some(Effect::Deny) {
            assert!(matches!(
                result.effective_policy,
                EffectivePolicyPreview::Denied(_)
            ));
        } else if forced || plan || restrictive.is_some() {
            assert_eq!(result.effective_policy, EffectivePolicyPreview::Prompt);
        } else {
            assert_eq!(
                result.effective_policy,
                EffectivePolicyPreview::AllowedByPolicy
            );
        }
        assert!(manager.structured_rule_inventory().unwrap().is_empty());
    }

    #[test]
    fn external_generation_refresh_notifies_sleeping_requests() {
        let (_temp, manager, provider, _publication) = manager();
        manager.refresh_permission_state().unwrap();
        let request = shell_request(&[COMMAND], workcell_shell_subject());
        let (sender, _answer) = flume::bounded(1);
        let (changed, changes) = flume::bounded(1);
        let revision = *manager.context_revision.read().unwrap();
        manager.pending().entry(manager.id).or_default().insert(
            request.id.clone(),
            PendingPermission {
                request,
                evaluation: None,
                project: None,
                context_revision: revision,
                answering: false,
                abandoned: false,
                cancel: CancelToken::none(),
                changed,
                sender,
            },
        );
        let mut rule = record(&provider).rule;
        rule.lifetime = PermissionLifetime::Global;
        PermissionState::open(&manager.policy.as_ref().unwrap().state_dir)
            .unwrap()
            .insert(None, rule)
            .unwrap();
        assert!(changes.try_recv().is_err());
        assert!(manager.refresh_permission_state().unwrap());
        assert!(changes.try_recv().is_ok());
        assert!(!manager.refresh_permission_state().unwrap());
    }

    #[test]
    fn commit_rejects_changed_fields_without_writing() {
        let (_temp, manager, _provider, publication) = manager();
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        let mut draft = draft();
        let preview = manager.preview_permission_edit(&session, &draft).unwrap();
        let confirmation = preview.confirm(preview.requirements()).unwrap();
        draft.effect = StructuredPermissionEffect::Deny;
        assert!(matches!(
            manager.commit_permission_edit(&preview, &confirmation, &draft),
            Err(PermissionEditError::Unconfirmed)
        ));
        assert!(publication.snapshot().unwrap().records.is_empty());
    }
}
