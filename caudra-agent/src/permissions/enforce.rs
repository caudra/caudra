use super::decisions::{advisories, label_answer};
use super::diagnostics::{answer_scope_kind, bounded_log_value, resource_kind_name};
use super::diagnostics::{prompt_reason, prompt_reason_message};
use super::manager::{ObservedPatterns, PERMISSION_POLL_INTERVAL};
use super::policy::TRUSTED_UNSCOPED_TOOLS;
use super::structured::trusted_shell_request;
use super::{
    AutoNote, AutopilotNote, DECISION_SOURCE_AUTO, DECISION_SOURCE_RULE,
    DECISION_SOURCE_USER_ABORT, DECISION_SOURCE_YOLO, DEFAULT_DENY_GUIDANCE,
    NORMALIZED_COMMAND_ATTRIBUTE, PERMISSION_DENIED_PREFIX, PERMISSION_LOG_TARGET,
    PROMPT_LOG_MAX_RESOURCES, PendingDecision, PendingPermission, PendingRegistration,
    PermissionAnswer, PermissionExecutorKind, PermissionLifetime, PermissionManager,
    PermissionMode, PermissionPolicyError, PermissionPresentation, PermissionRequest,
    PermissionResource, PermissionResourceAccess, PermissionResourceKind, PermissionSubject,
    PolicyRule, PromptReason, ResourceCoverage, RuleOrigin, ShellOpacity,
    StructuredPermissionDecision, StructuredPermissionEffect, answer_log_fields, command_pattern,
    normalize_scope_path, permission_rule_intersects_request, permission_rules_resource_standing,
    prompt_forcing_reason, remove_pending, subject_kind_and_contract, uncovered_resource_summary,
    update_presentation_coverage,
};
use crate::CancelToken;
use crate::autopilot::{
    AutopilotHandle, DecisionKind, DecisionOrigin, DecisionOutcome, DecisionRequest, DecisionSlot,
    DelegateAction, DelegateOutcome, DelegateOutput, WireError, mode_label,
};
use crate::decisions::{DecisionFeature, NoteAnchor, NoteSink};
use crate::tools::native::plan::{self, PlanAccess, PlanTarget};
use crate::tools::{PermissionIntent, PermissionScopes, ToolContext};
use crate::types::Answerer;
use crate::{AgentEvent, EventSender};
use caudra_config::{DefaultEffect, FILE_WRITE_TOOLS, ToolKey};
use serde_json::Value;
use std::borrow::Cow;
use std::fmt::Write;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;
use thiserror::Error;
use tracing::{info, warn};

pub(super) const CURRENT_POLICY_DENIES_REQUEST: &str =
    "current permission policy denies this request";
pub(super) const CURRENT_DEFAULT_DENIES_REQUEST: &str =
    "current permission default denies this request";
const ACTIVE_PLAN_INTENT_MISMATCH: &str =
    "plan permission intent does not match the active target and requested action";
const USER_GUIDANCE_LABEL: &str = "User guidance:";
const AUTOPILOT_GUIDANCE_LABEL: &str = "Autopilot guidance:";
/// What the model is told when autopilot escalates and no human can answer.
const UNATTENDED_ESCALATION: &str = "Autopilot left this for a human, and none is present.";
const TAINTED_RESERVATION: &str = "tainted";
const MAX_ITEM_INPUT_BYTES: usize = 2 * 1024;
const ITEM_TOOL_LABEL: &str = "tool:";
const ITEM_INPUT_LABEL: &str = "input:";
const ITEM_RESOURCES_LABEL: &str = "resources:";
const ITEM_BULLET: &str = "-";
const ITEM_PROTECTED: &str = "protected";
const ITEM_MORE: &str = "more";
const ITEM_REASON_LABEL: &str = "prompt reason:";
const ITEM_AUTO_LABEL: &str = "auto note:";
const ITEM_ADVISORIES_LABEL: &str = "engine advisories:";
const ITEM_AGENT_LABEL: &str = "requesting agent:";
const ITEM_MODE_LABEL: &str = "permission mode:";
const ITEM_RESERVED_LABEL: &str = "reserved:";
const ITEM_NOT_RESERVED: &str = "no";
const ITEM_TRUNCATED: &str = " [truncated]";
const ITEM_SEPARATOR: &str = ", ";
const HUMAN_GUIDANCE_SEPARATOR: &str = ": ";
const MAIN_AGENT: &str = "the main agent or one of its subagents";
const WORKFLOW_AGENT: &str = "workflow run";
const WORKFLOW_CALL: &str = "agent call";
const WORKFLOW_PHASE: &str = "phase";
const BACKGROUND_TASK_AGENT: &str = "background task";

pub(super) fn active_plan_access(
    intent: &PermissionIntent,
    input: &Value,
    target: &PlanTarget,
) -> Result<PlanAccess, &'static str> {
    let action = input.get("action").and_then(Value::as_str);
    let access = [PlanAccess::Read, PlanAccess::Write]
        .into_iter()
        .find(|access| action == Some(access.operation()))
        .ok_or(ACTIVE_PLAN_INTENT_MISMATCH)?;
    let operation = access.operation();
    let [resource] = intent.resources.as_slice() else {
        return Err(ACTIVE_PLAN_INTENT_MISMATCH);
    };
    let (kind, value, scope_target) = match target {
        PlanTarget::Local(path) => {
            let path = path.to_str().ok_or(ACTIVE_PLAN_INTENT_MISMATCH)?;
            (PermissionResourceKind::File, Cow::Borrowed(path), path)
        }
        PlanTarget::Remote(reference) => (
            PermissionResourceKind::Custom {
                name: "local_document".into(),
            },
            Cow::Owned(format!("plan:{}", reference.as_str())),
            reference.as_str(),
        ),
    };
    if resource.kind != kind
        || resource.value != value
        || resource.access != Some(access.resource_access())
        || resource.attributes.get("operation").map(String::as_str) != Some(operation)
        || intent.scopes.scopes != [format!("plan:{operation}:{scope_target}")]
    {
        return Err(ACTIVE_PLAN_INTENT_MISMATCH);
    }
    Ok(access)
}

pub(super) fn exact_local_plan_write(
    tool: &ToolKey,
    resources: &[PermissionResource],
    plan_path: Option<&Path>,
) -> bool {
    plan_path.is_some_and(|plan_path| {
        matches!(tool, ToolKey::Native(name) if FILE_WRITE_TOOLS.contains(&name.as_ref()))
            && !resources.is_empty()
            && resources.iter().all(|resource| {
                resource.access == Some(PermissionResourceAccess::Write)
                    && normalize_scope_path(&resource.value)
                        == normalize_scope_path(&plan_path.display().to_string())
            })
    })
}

/// An intent that names no resource is normally a tool that under-declared what
/// it touches, and denying it is what keeps a broad tool-level rule from
/// standing in for the path scoping the tool should have supplied.
///
/// The tools in [`TRUSTED_UNSCOPED_TOOLS`] are the exception, because for them
/// the empty list is the true answer rather than a missing one: a `code` snippet
/// has no filesystem, network or subprocess of its own, and it, `batch`, `task`
/// and `workflow` reach the manager again for every inner call. Denying those
/// leaves the tool unusable and teaches the model nothing, so the request goes
/// on to be evaluated as the unscoped call it is. Evaluation still runs, so a
/// deny rule aimed at one of these tools keeps blocking it.
fn is_unscoped_tool(tool: &ToolKey) -> bool {
    matches!(tool, ToolKey::Native(name) if TRUSTED_UNSCOPED_TOOLS.contains(&name.as_ref()))
}

#[derive(Debug, Error)]
pub struct PermissionError {
    pub(super) tool: String,
    pub(super) scope: String,
    pub(super) guidance: Option<Guidance>,
}

/// What the model is told to do instead, and who said it.
#[derive(Debug)]
pub(super) struct Guidance {
    pub(super) text: String,
    pub(super) by: Answerer,
}

impl Guidance {
    fn label(&self) -> &'static str {
        match self.by {
            Answerer::User => USER_GUIDANCE_LABEL,
            Answerer::Autopilot => AUTOPILOT_GUIDANCE_LABEL,
        }
    }
}

impl std::fmt::Display for PermissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} `{}` ({}).",
            PERMISSION_DENIED_PREFIX, self.tool, self.scope
        )?;
        match &self.guidance {
            Some(guidance) => write!(f, " {} {}", guidance.label(), guidance.text),
            None => write!(f, " {DEFAULT_DENY_GUIDANCE}"),
        }
    }
}

impl PermissionError {
    pub(super) fn new(tool: &str, scope: &str) -> Self {
        Self {
            tool: tool.to_string(),
            scope: scope.to_string(),
            guidance: None,
        }
    }

    pub(super) fn with_guidance(tool: &str, scope: &str, guidance: String) -> Self {
        Self::guided_by(tool, scope, guidance, Answerer::User)
    }

    pub(super) fn guided_by(tool: &str, scope: &str, text: String, by: Answerer) -> Self {
        Self {
            tool: tool.to_string(),
            scope: scope.to_string(),
            guidance: Some(Guidance { text, by }),
        }
    }
}

pub(super) struct RequestCoverage {
    pub(super) covered: Vec<Option<ResourceCoverage>>,
    pub(super) must_prompt: bool,
    /// The first ask that forced the prompt, for explaining it. `must_prompt`
    /// alone decides, so a missing explanation can never weaken an ask.
    pub(super) asking: Option<ResourceCoverage>,
    pub(super) resolved: bool,
}

#[derive(Clone)]
pub(super) struct EvaluationContext {
    pub(super) revision: u64,
    pub(super) plan_scoped: bool,
    pub(super) builtin_allows: bool,
    pub(super) force_prompt: bool,
    pub(super) forced: bool,
    pub(super) exact_plan: Option<PlanAccess>,
}

/// How Auto mode may settle a request the policy leaves to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AutoEligibility {
    /// Auto decides as it always has, consulting an engine only for caution.
    Direct,
    /// Only an enforcing engine that read the whole line may approve it.
    NeedsEngine,
    Never(AutoNote),
}

pub(super) struct CurrentPolicy {
    pub(super) coverage: RequestCoverage,
    pub(super) automatic: bool,
    /// `None` outside Auto mode, or when the policy settles the request alone.
    pub(super) auto: Option<AutoEligibility>,
    source: &'static str,
    reason: PromptReason,
}

impl CurrentPolicy {
    /// Says why the request needs an answer. Screening is not repeated while a
    /// prompt waits, so a request Auto may still settle keeps the note its
    /// screening left.
    fn explain(&self, presentation: &mut PermissionPresentation) {
        presentation.risk_summary = prompt_reason_message(&self.reason).into();
        presentation.reason = self.reason.clone();
        match self.auto {
            None => presentation.auto = None,
            Some(AutoEligibility::Never(note)) => presentation.auto = Some(note),
            Some(AutoEligibility::Direct | AutoEligibility::NeedsEngine) => {}
        }
    }
}

/// What woke a waiting prompt: the waiter's own sources, or autopilot's call
/// ending, `None` when it was cut off.
enum Woken {
    Waiter(Option<PendingDecision>),
    Delegate(Option<DelegateOutcome>),
}

/// Autopilot's part in one prompt: what the delegate is told about it, and
/// the decision's place in the engagement.
struct PromptDelegate {
    /// Why autopilot may not allow the request, `None` when it may.
    reservation: Option<&'static str>,
    item: String,
    slot: DecisionSlot,
}

impl PromptDelegate {
    /// `None` when `cancel` fired first.
    async fn decide(&self, cancel: &CancelToken) -> Option<DelegateOutcome> {
        let reserved = self.reservation.is_some();
        let request = DecisionRequest {
            kind: DecisionKind::Permission,
            reserved,
            item: &self.item,
        };
        let outcome = self.slot.decide(&request, cancel).await?;
        Some(screened(outcome, reserved))
    }

    /// Ends the decision with what settled the prompt, `None` when the run
    /// stopped first.
    fn settle(&self, decision: Option<&PendingDecision>) {
        match decision {
            Some(PendingDecision::Explicit(answer, Answerer::User)) => {
                self.slot.human_answered(&human_answer(answer));
            }
            Some(PendingDecision::MatchedRule | PendingDecision::PolicyDenied(_)) => {
                self.slot.end(DecisionOutcome::Superseded);
            }
            Some(PendingDecision::Explicit(_, Answerer::Autopilot)) | None => {}
        }
    }
}

/// A human's answer to a prompt as the log words it: how it answered and,
/// for a denial, what it said.
fn human_answer(answer: &PermissionAnswer) -> String {
    let (name, ..) = answer_log_fields(answer);
    match answer.guidance() {
        Some(guidance) => format!("{name}{HUMAN_GUIDANCE_SEPARATOR}{guidance}"),
        None => name.to_owned(),
    }
}

impl PendingRegistration<'_> {
    /// Re-explains the waiting prompt under `current`. The autopilot note is
    /// the pending entry's own, set only by [`Self::note_autopilot`], so a
    /// refresh carries it over rather than writing back a copy that predates
    /// it.
    fn refresh(
        &self,
        request: &mut PermissionRequest,
        current: &CurrentPolicy,
        revision: u64,
    ) -> Option<(bool, bool)> {
        let mut updated = request.clone();
        updated.add_pattern_candidates(
            &self.manager.pattern_candidates(),
            &current.coverage.covered,
        );
        update_presentation_coverage(&mut updated.presentation, &current.coverage.covered);
        current.explain(&mut updated.presentation);
        let mut pending = self.manager.pending();
        if revision != self.manager.broker.revision.load(Ordering::Acquire) {
            return None;
        }
        let candidate = pending
            .get_mut(&self.manager.id)
            .and_then(|requests| requests.get_mut(self.request_id))
            .filter(|candidate| candidate.sender.same_channel(&self.sender));
        match candidate {
            Some(candidate) if !candidate.answering && !candidate.abandoned => {
                updated
                    .presentation
                    .autopilot
                    .clone_from(&candidate.request.presentation.autopilot);
                let changed = request.presentation != updated.presentation
                    || request.options != updated.options;
                candidate.request = updated.clone();
                *request = updated;
                if current.automatic {
                    remove_pending(&mut pending, self.manager.id, self.request_id);
                    Some((true, false))
                } else {
                    Some((false, changed))
                }
            }
            _ => Some((false, false)),
        }
    }
}

impl PermissionManager {
    /// The builtin allowlist of fully literal, side-effect-free commands. It is
    /// a default rather than a rule, so it is consulted only where no rule
    /// speaks: ranking it against configured policy would let `echo hi` outrank
    /// a configured `echo *` ask purely for being the more exact text.
    pub(super) fn builtin_command_allow(resource: &PermissionResource) -> Option<ResourceCoverage> {
        (resource.kind == PermissionResourceKind::Command && !resource.protected)
            .then(|| command_pattern::builtin_allow_pattern(&resource.value))
            .flatten()
            .map(|pattern| ResourceCoverage {
                origin: RuleOrigin::Builtin,
                authority: pattern.to_owned(),
                asks: false,
            })
    }

    /// The builtin ask family a command falls in, named by its pattern.
    pub(super) fn builtin_command_ask(resource: &PermissionResource) -> Option<ResourceCoverage> {
        (resource.kind == PermissionResourceKind::Command)
            .then(|| {
                command_pattern::BUILTIN_ASK_PATTERNS
                    .iter()
                    .find(|pattern| {
                        command_pattern::matches(pattern, &resource.value)
                            || resource
                                .attributes
                                .get(NORMALIZED_COMMAND_ATTRIBUTE)
                                .is_some_and(|normalized| {
                                    command_pattern::matches(pattern, normalized)
                                })
                    })
            })
            .flatten()
            .map(|pattern| ResourceCoverage {
                origin: RuleOrigin::Builtin,
                authority: (*pattern).to_owned(),
                asks: true,
            })
    }

    pub(super) fn request_coverage(
        &self,
        request: &PermissionRequest,
        structured_rules: &[PolicyRule],
        builtin_allows: bool,
    ) -> RequestCoverage {
        let mut must_prompt = false;
        let mut asking = None;
        let mut resolved = true;
        let covered = request
            .resources
            .iter()
            .map(|resource| {
                let standing =
                    permission_rules_resource_standing(structured_rules, request, resource);
                match standing.decision {
                    StructuredPermissionDecision::Allow => standing.coverage,
                    // An ask withholds authority without erasing it. The
                    // resource stays covered so a later grant can sweep it,
                    // and the prompt names the ask that outranked the allow.
                    StructuredPermissionDecision::Ask => {
                        must_prompt = true;
                        let named = standing.asking.clone();
                        asking = asking.take().or(standing.asking);
                        standing.coverage.map(|allowed| ResourceCoverage {
                            asks: true,
                            ..named.unwrap_or(allowed)
                        })
                    }
                    StructuredPermissionDecision::Deny => {
                        resolved = false;
                        None
                    }
                    StructuredPermissionDecision::NoMatch => {
                        let covered = builtin_allows
                            .then(|| Self::builtin_command_allow(resource))
                            .flatten();
                        let builtin_ask = covered
                            .is_none()
                            .then(|| Self::builtin_command_ask(resource))
                            .flatten();
                        if builtin_ask.is_some() {
                            must_prompt = true;
                            asking = asking.take().or(builtin_ask);
                        } else if covered.is_none() {
                            resolved = false;
                        }
                        covered
                    }
                }
            })
            .collect();
        RequestCoverage {
            covered,
            must_prompt,
            asking,
            resolved,
        }
    }

    pub(super) fn current_policy(
        &self,
        request: &PermissionRequest,
        context: &EvaluationContext,
    ) -> Result<CurrentPolicy, PermissionPolicyError> {
        let revision = self
            .context_revision
            .read()
            .unwrap_or_else(|error| error.into_inner());
        if *revision != context.revision {
            return Err(PermissionPolicyError(
                "reviewed permission context changed".into(),
            ));
        }
        let _mutation = self
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let rules =
            self.applicable_rules_within(request, context.plan_scoped, context.builtin_allows)?;
        self.evaluate_policy_rules(request, context, &rules)
    }

    pub(super) fn evaluate_policy_rules(
        &self,
        request: &PermissionRequest,
        context: &EvaluationContext,
        rules: &[PolicyRule],
    ) -> Result<CurrentPolicy, PermissionPolicyError> {
        if self.configured().remote_policy_invalid {
            return Err(PermissionPolicyError(
                "remote permission policy is unavailable".into(),
            ));
        }
        if self.remote_default_denies(&request.tool, request)
            || rules
                .iter()
                .any(|policy| permission_rule_intersects_request(&policy.rule, request))
        {
            return Err(PermissionPolicyError(CURRENT_POLICY_DENIES_REQUEST.into()));
        }
        let coverage = self.request_coverage(request, rules, context.builtin_allows);
        // A verified read of the session plan is covered the way the built-in
        // allows cover the project's plan files: asking first protects nothing.
        let exact_read = context.exact_plan == Some(PlanAccess::Read);
        let covered = exact_read || coverage.covered.iter().all(Option::is_some);
        let default = self.default_effect(&request.tool);
        let mode = self.mode();
        if mode != PermissionMode::Yolo
            && !coverage.resolved
            && !exact_read
            && !context.force_prompt
            && default == DefaultEffect::Deny
        {
            return Err(PermissionPolicyError(CURRENT_DEFAULT_DENIES_REQUEST.into()));
        }
        let automatic = mode == PermissionMode::Yolo
            || (!context.forced
                && !coverage.must_prompt
                && (covered
                    || (!context.force_prompt
                        && (context.exact_plan == Some(PlanAccess::Write)
                            || default == DefaultEffect::Allow))));
        let auto = (mode == PermissionMode::Auto && !automatic)
            .then(|| auto_eligibility(request, context, &coverage, default));
        Ok(CurrentPolicy {
            reason: prompt_reason(
                request,
                &coverage.covered,
                context.forced,
                coverage.asking.as_ref(),
                context.plan_scoped,
            ),
            coverage,
            automatic,
            auto,
            source: if mode == PermissionMode::Yolo {
                DECISION_SOURCE_YOLO
            } else if matches!(
                auto,
                Some(AutoEligibility::Direct | AutoEligibility::NeedsEngine)
            ) {
                DECISION_SOURCE_AUTO
            } else {
                DECISION_SOURCE_RULE
            },
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn enforce(
        &self,
        tool: &ToolKey,
        scopes: &crate::tools::PermissionScopes,
        input: &serde_json::Value,
        event_tx: &EventSender,
        user_response_rx: Option<&async_lock::Mutex<flume::Receiver<String>>>,
        request_id: &str,
        cancel: &CancelToken,
        plan_path: Option<&Path>,
    ) -> Result<(), PermissionError> {
        self.enforce_with_identity(
            tool,
            scopes,
            input,
            event_tx,
            user_response_rx,
            request_id,
            cancel,
            plan_path,
            None,
            true,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn enforce_with_identity(
        &self,
        tool: &ToolKey,
        scopes: &crate::tools::PermissionScopes,
        input: &serde_json::Value,
        event_tx: &EventSender,
        user_response_rx: Option<&async_lock::Mutex<flume::Receiver<String>>>,
        request_id: &str,
        cancel: &CancelToken,
        plan_path: Option<&Path>,
        identity: Option<(PermissionSubject, PermissionExecutorKind)>,
        include_builtin_allows: bool,
        action: Option<&str>,
    ) -> Result<(), PermissionError> {
        self.enforce_inner(
            tool,
            scopes,
            input,
            event_tx,
            user_response_rx,
            request_id,
            cancel,
            plan_path,
            identity,
            include_builtin_allows,
            None,
            None,
            action,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn enforce_with_intent(
        &self,
        tool: &ToolKey,
        intent: &crate::tools::PermissionIntent,
        input: &serde_json::Value,
        event_tx: &EventSender,
        user_response_rx: Option<&async_lock::Mutex<flume::Receiver<String>>>,
        request_id: &str,
        cancel: &CancelToken,
        plan_path: Option<&Path>,
        identity: Option<(PermissionSubject, PermissionExecutorKind)>,
        include_builtin_allows: bool,
    ) -> Result<(), PermissionError> {
        self.enforce_inner(
            tool,
            &intent.scopes,
            input,
            event_tx,
            user_response_rx,
            request_id,
            cancel,
            plan_path,
            identity,
            include_builtin_allows,
            Some(intent),
            None,
            None,
        )
        .await
    }

    pub async fn enforce_active_plan(
        &self,
        intent: &PermissionIntent,
        input: &Value,
        ctx: &ToolContext,
        request_id: &str,
        identity: Option<(PermissionSubject, PermissionExecutorKind)>,
    ) -> Result<(), PermissionError> {
        let refuse = |guidance: String| {
            PermissionError::with_guidance(plan::NAME, &intent.scopes.scopes.join("; "), guidance)
        };
        let target = plan::verified_target(ctx).map_err(|error| refuse(error.to_string()))?;
        let access =
            active_plan_access(intent, input, &target).map_err(|error| refuse(error.into()))?;
        if access == PlanAccess::Write {
            plan::write_target(ctx).map_err(|error| refuse(error.to_string()))?;
        }
        self.enforce_inner(
            &ToolKey::native(plan::NAME),
            &intent.scopes,
            input,
            &ctx.event_tx,
            ctx.user_response_rx.as_deref(),
            request_id,
            &ctx.cancel,
            None,
            identity,
            true,
            Some(intent),
            Some(access),
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn enforce_inner(
        &self,
        tool: &ToolKey,
        scopes: &PermissionScopes,
        input: &Value,
        event_tx: &EventSender,
        user_response_rx: Option<&async_lock::Mutex<flume::Receiver<String>>>,
        request_id: &str,
        cancel: &CancelToken,
        plan_path: Option<&Path>,
        identity: Option<(PermissionSubject, PermissionExecutorKind)>,
        include_builtin_allows: bool,
        intent: Option<&PermissionIntent>,
        verified_plan: Option<PlanAccess>,
        action: Option<&str>,
    ) -> Result<(), PermissionError> {
        // A plan-scoped call is unreviewable in the same way a forced prompt
        // is, so it is built and presented the same way. The difference is what
        // may settle it, which is decided against the rules, not here.
        info!(
            target: PERMISSION_LOG_TARGET,
            event = "permission_invocation",
            manager_id = self.id,
            run_id = event_tx.run_id(),
            request_id,
            workflow_call_key = event_tx.workflow().map(|provenance| provenance.call_key),
            policy_revision = self.broker.revision.load(Ordering::Acquire),
            "permission invocation evaluated"
        );
        let unreviewed = scopes.force_prompt || scopes.plan_scoped;
        let (cwd, canonical_project, reviewed_revision) = {
            let revision = self
                .context_revision
                .read()
                .unwrap_or_else(|error| error.into_inner());
            let project = self.project();
            (
                project.cwd.clone(),
                project.canonical_project.clone(),
                *revision,
            )
        };
        let tool_string = tool.to_string();
        let scope_display = || scopes.scopes.join("; ");
        // Every deny is built here and every approval passes through
        // `allowed`, so reporting cannot drift from what the caller gets.
        let deny_by = |source: &'static str, guidance: Option<String>, by: Answerer| {
            caudra_otel::emit::tool_decision(
                &tool_string,
                caudra_otel::emit::DECISION_REJECT,
                source,
            );
            match guidance {
                Some(g) => PermissionError::guided_by(&tool_string, &scope_display(), g, by),
                None => PermissionError::new(&tool_string, &scope_display()),
            }
        };
        let deny = |source, guidance| deny_by(source, guidance, Answerer::User);
        let allowed = |observed: ObservedPatterns<'_>, source: &'static str| {
            observed.keep();
            caudra_otel::emit::tool_decision(
                &tool_string,
                caudra_otel::emit::DECISION_ACCEPT,
                source,
            );
            Ok(())
        };
        let make_request = |tool: ToolKey, request_scopes: Vec<String>, force_prompt: bool| {
            if let Some(intent) = intent {
                let mut intent = intent.clone();
                intent.scopes.force_prompt = force_prompt;
                return match &identity {
                    Some((subject, executor)) => PermissionRequest::from_intent_with_identity(
                        request_id.to_owned(),
                        tool,
                        &intent,
                        input.clone(),
                        &cwd,
                        subject.clone(),
                        executor.clone(),
                    ),
                    None => PermissionRequest::from_intent(
                        request_id.to_owned(),
                        tool,
                        &intent,
                        input.clone(),
                        &cwd,
                    ),
                };
            }
            let request = match &identity {
                Some((subject, executor)) => PermissionRequest::from_legacy_with_identity(
                    request_id.to_owned(),
                    tool,
                    request_scopes,
                    input.clone(),
                    &cwd,
                    force_prompt,
                    subject.clone(),
                    executor.clone(),
                ),
                None => PermissionRequest::from_legacy(
                    request_id.to_owned(),
                    tool,
                    request_scopes,
                    input.clone(),
                    &cwd,
                    force_prompt,
                ),
            };
            match action {
                Some(action) => request.with_action(action),
                None => request,
            }
        };
        let initial_request = make_request(tool.clone(), scopes.scopes.clone(), unreviewed);
        let exact_plan = verified_plan.or_else(|| {
            exact_local_plan_write(tool, &initial_request.resources, plan_path)
                .then_some(PlanAccess::Write)
        });
        let force_prompt = unreviewed
            || (exact_plan.is_none()
                && initial_request
                    .resources
                    .iter()
                    .any(|resource| resource.requires_prompt));
        let full_request = if force_prompt == unreviewed {
            initial_request
        } else {
            make_request(tool.clone(), scopes.scopes.clone(), force_prompt)
        };
        if intent.is_some() && full_request.resources.is_empty() && !is_unscoped_tool(tool) {
            warn!(tool = %tool, "explicit permission intent has no resources");
            return Err(deny(
                DECISION_SOURCE_RULE,
                Some("tool permission intent did not identify any resources".into()),
            ));
        }
        let context = EvaluationContext {
            revision: reviewed_revision,
            plan_scoped: scopes.plan_scoped,
            builtin_allows: include_builtin_allows,
            force_prompt,
            forced: scopes.force_prompt,
            exact_plan,
        };
        let mut request = full_request;
        let observed = self.observe_pattern_request(&request);
        if scopes.plan_scoped {
            contain_authority_to_the_plan(&mut request);
        }
        let mut current = self
            .current_policy(&request, &context)
            .map_err(|error| deny(DECISION_SOURCE_RULE, Some(error.to_string())))?;
        if current.automatic {
            return allowed(observed, current.source);
        }
        let mut receipts = Vec::new();
        let mut escalation = None;
        let mut screening_note = None;
        if let Some(AutoEligibility::Direct | AutoEligibility::NeedsEngine) = current.auto {
            let pending = self.decisions().map(|service| {
                NoteSink::new(event_tx.clone(), NoteAnchor::Tool(request_id.to_owned()))
                    .begin(&service, DecisionFeature::AutoScreening)
            });
            let screened = self
                .screen_auto_candidate(&request, &context, cancel)
                .await
                .map_err(|error| deny(DECISION_SOURCE_RULE, Some(error.to_string())))?;
            drop(pending);
            let note = screened.decision_note(request_id, cancel);
            current = screened.policy;
            if current.automatic {
                return allowed(observed, current.source);
            }
            if screened.approved {
                if let Some(note) = note {
                    event_tx.try_send(note);
                }
                return allowed(observed, DECISION_SOURCE_AUTO);
            }
            screening_note = note;
            request.presentation.auto = screened.note;
            escalation = screened.escalation;
            if let Some((service, decision)) = screened.decision {
                request.presentation.advisories = advisories(&decision);
                if let Some(receipt) = decision.evaluation.receipt {
                    receipts.push((service, receipt));
                }
            }
        }
        request.add_pattern_candidates(&self.pattern_candidates(), &current.coverage.covered);
        current.explain(&mut request.presentation);
        if self.policy.is_some()
            && request.options.iter().any(|option| {
                option.rule.effect == StructuredPermissionEffect::Allow
                    && option
                        .allowed_lifetimes
                        .contains(&PermissionLifetime::Project)
            })
        {
            request.presentation.project = canonical_project.clone();
        }
        let coverage = current.coverage;
        update_presentation_coverage(&mut request.presentation, &coverage.covered);
        let delegate = self.prompt_delegate(&request, &context, event_tx);

        let Some(_) = user_response_rx else {
            warn!(
                tool = %tool,
                scope = %scope_display(),
                autopilot = delegate.is_some(),
                "no permission response channel"
            );
            if let Some(effect) = self.record_escalation(escalation, &context) {
                effect.detach();
            }
            if let Some(note) = screening_note {
                event_tx.try_send(note);
            }
            let Some(delegate) = delegate else {
                return Err(deny(DECISION_SOURCE_USER_ABORT, None));
            };
            delegate.slot.announce();
            let Some(outcome) = delegate.decide(cancel).await else {
                return Err(deny(DECISION_SOURCE_USER_ABORT, None));
            };
            let (answer, settled) = match delegated_answer(&outcome) {
                Ok(answer) => (answer, DecisionOutcome::Applied),
                Err(_) => (
                    PermissionAnswer::DenyWithGuidance(UNATTENDED_ESCALATION.into()),
                    DecisionOutcome::DeniedUnattended,
                ),
            };
            let source = answer.decision_source_by(Answerer::Autopilot);
            if !answer.is_allow() {
                delegate.slot.record(&outcome, Some(settled));
                return Err(deny_by(
                    source,
                    answer.guidance().map(String::from),
                    Answerer::Autopilot,
                ));
            }
            if cancel.is_cancelled() {
                delegate
                    .slot
                    .record(&outcome, Some(DecisionOutcome::Cancelled));
                return Err(deny(DECISION_SOURCE_USER_ABORT, None));
            }
            if let Err(error) = self.current_policy(&request, &context) {
                delegate
                    .slot
                    .record(&outcome, Some(DecisionOutcome::Superseded));
                return Err(deny(DECISION_SOURCE_RULE, Some(error.to_string())));
            }
            delegate.slot.record(&outcome, Some(settled));
            return allowed(observed, source);
        };

        let (delegate_stop, delegate_cancel) =
            delegate.as_ref().map(|_| CancelToken::new()).unzip();
        if delegate.is_some() {
            request.presentation.autopilot = Some(AutopilotNote::Deciding);
        }
        let (answer_tx, answer_rx) = flume::bounded(1);
        let (changed_tx, changed_rx) = flume::bounded(1);
        {
            let mut pending = self.pending();
            let requests = pending.entry(self.id).or_default();
            if requests.contains_key(request_id) {
                return Err(deny(DECISION_SOURCE_USER_ABORT, None));
            }
            requests.insert(
                request_id.to_owned(),
                PendingPermission {
                    request: request.clone(),
                    evaluation: Some(context.clone()),
                    project: canonical_project,
                    context_revision: reviewed_revision,
                    answering: false,
                    abandoned: false,
                    claimed_by_human: false,
                    cancel: cancel.clone(),
                    delegate_stop,
                    changed: changed_tx.clone(),
                    sender: answer_tx.clone(),
                },
            );
        }
        let registration = PendingRegistration {
            manager: self,
            request_id,
            sender: answer_tx,
            event_tx,
        };
        if event_tx
            .send(AgentEvent::PermissionRequest(Box::new(request.clone())))
            .is_err()
        {
            if let Some(effect) = self.record_escalation(escalation, &context) {
                effect.detach();
            }
            return Err(deny(DECISION_SOURCE_USER_ABORT, None));
        }
        if let Some(effect) = self.record_escalation(escalation, &context) {
            effect.detach();
        }
        if let Some(note) = screening_note {
            event_tx.try_send(note);
        }
        if let Some(delegate) = &delegate {
            delegate.slot.announce();
        }
        let forcing_reason = prompt_forcing_reason(
            &request,
            &coverage.covered,
            unreviewed,
            coverage.must_prompt,
        );
        let uncovered = uncovered_resource_summary(&request, &coverage.covered);
        let uncovered_count = coverage
            .covered
            .iter()
            .filter(|covered| covered.is_none())
            .count();
        let (subject_owner, subject_contract) = subject_kind_and_contract(&request.subject);
        let opacity = request.resources.iter().filter_map(ShellOpacity::of).max();
        // Captured now, because a refresh while the prompt waits may drop the
        // note that marks this as an Auto prompt.
        let auto_opacity = request.presentation.auto.and(opacity);
        info!(
            target: PERMISSION_LOG_TARGET,
            event = "permission_prompt",
            manager_id = self.id,
            run_id = event_tx.run_id(),
            policy_revision = self.broker.revision.load(Ordering::Acquire),
            request_id,
            tool = %request.tool,
            executor = ?request.executor,
            risk = ?request.risk,
            subject_owner = %bounded_log_value(subject_owner),
            subject_contract = %bounded_log_value(subject_contract),
            resource_count = request.resources.len(),
            uncovered_count,
            forcing_reason,
            opacity = opacity.map(display),
            auto_note = request.presentation.auto.map(AutoNote::name),
            autopilot_seq = delegate.as_ref().map(|delegate| delegate.slot.seq()),
            autopilot_reservation = delegate.as_ref().and_then(|delegate| delegate.reservation),
            uncovered = %uncovered,
            offered_options = %request
                .options
                .iter()
                .take(PROMPT_LOG_MAX_RESOURCES)
                .map(|option| bounded_log_value(&option.id))
                .collect::<Vec<_>>()
                .join(","),
            "permission prompt raised"
        );
        let waiting_since = Instant::now();
        let advice_revision = self.broker.revision.load(Ordering::Acquire);
        let mut advisory_task = self.permission_advice(&request);
        let mut shell_advisory_task = self.shell_effect_advice(&request, context.plan_scoped);
        let mut shell_pending = self
            .decisions()
            .filter(|_| shell_advisory_task.is_some())
            .map(|service| {
                NoteSink::new(event_tx.clone(), NoteAnchor::Tool(request_id.to_owned()))
                    .begin(&service, DecisionFeature::ShellEffect)
            });
        let wait = async {
            let mut source_request_id = String::new();
            let mut delegate_call = delegate
                .as_ref()
                .zip(delegate_cancel.as_ref())
                .map(|(delegate, stop)| Box::pin(delegate.decide(stop)));
            loop {
                while let Ok(source) = changed_rx.try_recv() {
                    source_request_id = source;
                }
                if let Ok(decision) = answer_rx.try_recv() {
                    return Ok::<_, PermissionPolicyError>(decision);
                }
                let revision = self.broker.revision.load(Ordering::Acquire);
                let current = self.current_policy(&request, &context)?;
                let Some((settled, refreshed)) =
                    registration.refresh(&mut request, &current, revision)
                else {
                    continue;
                };
                if settled {
                    let _ = event_tx.send(AgentEvent::PermissionRequestResolved {
                        request_id: request_id.to_owned(),
                        source_request_id,
                        by: None,
                    });
                    return Ok(PendingDecision::MatchedRule);
                }
                if refreshed {
                    let _ = event_tx.send(AgentEvent::PermissionRequestUpdated(Box::new(
                        request.clone(),
                    )));
                }
                let wake = futures_lite::future::race(
                    async {
                        futures_lite::future::race(
                            async { answer_rx.recv_async().await.map(Some) },
                            async {
                                changed_rx.recv_async().await.map(|source| {
                                    source_request_id = source;
                                    None
                                })
                            },
                        )
                        .await
                        .map_err(|_| PermissionPolicyError("permission channel closed".into()))
                    },
                    async {
                        smol::Timer::after(PERMISSION_POLL_INTERVAL).await;
                        self.poll_permission_changes()?;
                        Ok(None)
                    },
                );
                let wake = futures_lite::future::race(wake, async {
                    let (service, decision, label_user) = futures_lite::future::race(
                        async {
                            let Some(task) = advisory_task.as_mut() else {
                                return futures_lite::future::pending().await;
                            };
                            let (service, decision) = task.await;
                            advisory_task = None;
                            (service, decision, true)
                        },
                        async {
                            let Some(task) = shell_advisory_task.as_mut() else {
                                return futures_lite::future::pending().await;
                            };
                            let (service, decision) = task.await;
                            shell_advisory_task = None;
                            shell_pending = None;
                            (service, decision, false)
                        },
                    )
                    .await;
                    if let Some(decision) = decision {
                        let effect = if label_user {
                            registration.annotate(
                                &mut request,
                                &decision,
                                advice_revision,
                                &service,
                            )
                        } else {
                            registration.annotate_shell_effect(
                                &mut request,
                                &decision,
                                advice_revision,
                                &service,
                                context.plan_scoped,
                            )
                        };
                        if let Some(effect) = effect {
                            effect.detach();
                        }
                        if label_user && let Some(receipt) = decision.evaluation.receipt {
                            receipts.push((service, receipt));
                        }
                    }
                    Ok(None)
                });
                let woken =
                    futures_lite::future::race(async { wake.await.map(Woken::Waiter) }, async {
                        let Some(call) = delegate_call.as_mut() else {
                            return futures_lite::future::pending().await;
                        };
                        let outcome = call.await;
                        delegate_call = None;
                        Ok(Woken::Delegate(outcome))
                    })
                    .await?;
                match woken {
                    Woken::Waiter(Some(decision)) => return Ok(decision),
                    Woken::Waiter(None) => {}
                    Woken::Delegate(outcome) => {
                        if let Some(delegate) = &delegate {
                            self.conclude_delegate(
                                delegate,
                                outcome,
                                &registration,
                                &mut request,
                                &context,
                            );
                        }
                    }
                }
            }
        };
        let decision = match cancel.race(wait).await {
            Ok(Ok(decision)) => Some(decision),
            Ok(Err(error)) => Some(PendingDecision::PolicyDenied(error.to_string())),
            Err(_) => None,
        };
        if let Some(delegate) = &delegate {
            delegate.settle(decision.as_ref());
        }
        if let Some(PendingDecision::Explicit(_, Answerer::Autopilot)) = &decision {
            let _ = event_tx.send(AgentEvent::PermissionRequestResolved {
                request_id: request_id.to_owned(),
                source_request_id: request_id.to_owned(),
                by: Some(Answerer::Autopilot),
            });
        }
        if let Some(PendingDecision::Explicit(answer, by)) = &decision {
            let label_advisory = self
                .decisions()
                .filter(|service| service.config().log)
                .and_then(|_| advisory_task.take());
            if let Some(label) = label_answer(
                receipts,
                label_advisory,
                answer,
                *by,
                waiting_since.elapsed().as_millis() as u64,
            ) {
                label.detach();
            }
        }
        // Paired with `permission_prompt` by `request_id`: the two together give
        // the prompt rate, what authority the answer bought, and the wait cost.
        let (answer, option_id, lifetime, answer_source) = match &decision {
            Some(PendingDecision::Explicit(explicit, by)) => {
                let (answer, option_id, lifetime) = answer_log_fields(explicit);
                (
                    answer,
                    option_id,
                    lifetime,
                    explicit.decision_source_by(*by),
                )
            }
            Some(PendingDecision::MatchedRule) => {
                ("matched_rule", Cow::Borrowed(""), "", DECISION_SOURCE_RULE)
            }
            Some(PendingDecision::PolicyDenied(_)) => {
                ("policy_denied", Cow::Borrowed(""), "", DECISION_SOURCE_RULE)
            }
            None => (
                "abandoned",
                Cow::Borrowed(""),
                "",
                DECISION_SOURCE_USER_ABORT,
            ),
        };
        info!(
            target: PERMISSION_LOG_TARGET,
            event = "permission_decision",
            manager_id = self.id,
            run_id = event_tx.run_id(),
            policy_revision = self.broker.revision.load(Ordering::Acquire),
            request_id,
            tool = %request.tool,
            answer,
            scope_kind = match &decision {
                Some(PendingDecision::Explicit(answer, _)) => answer_scope_kind(answer, &request),
                Some(PendingDecision::MatchedRule | PendingDecision::PolicyDenied(_)) => "current_policy",
                None => "abandoned",
            },
            option_id = %option_id,
            lifetime,
            source = answer_source,
            opacity = auto_opacity.map(display),
            waited_ms = waiting_since.elapsed().as_millis() as u64,
            "permission prompt answered"
        );
        let Some(decision) = decision else {
            return Err(deny(DECISION_SOURCE_USER_ABORT, None));
        };

        let allow = match &decision {
            PendingDecision::Explicit(answer, _) => answer.is_allow(),
            PendingDecision::MatchedRule => true,
            PendingDecision::PolicyDenied(_) => false,
        };
        let mut source = match &decision {
            PendingDecision::Explicit(answer, by) => answer.decision_source_by(*by),
            PendingDecision::MatchedRule | PendingDecision::PolicyDenied(_) => DECISION_SOURCE_RULE,
        };
        if allow {
            if cancel.is_cancelled() {
                return Err(deny(DECISION_SOURCE_USER_ABORT, None));
            }
            let current = self
                .current_policy(&request, &context)
                .map_err(|error| deny(DECISION_SOURCE_RULE, Some(error.to_string())))?;
            if matches!(decision, PendingDecision::MatchedRule) {
                if !current.automatic {
                    return Err(deny(DECISION_SOURCE_RULE, None));
                }
                source = current.source;
            }
            if let PendingDecision::Explicit(answer, _) = &decision {
                let remembered = match answer {
                    PermissionAnswer::AllowSession
                    | PermissionAnswer::AllowAlwaysLocal
                    | PermissionAnswer::AllowAlwaysGlobal => true,
                    PermissionAnswer::AllowOption { lifetime, .. } => {
                        *lifetime != PermissionLifetime::Once
                    }
                    PermissionAnswer::AllowComposed { rows } => rows.iter().any(Option::is_some),
                    _ => false,
                };
                if remembered
                    && current
                        .coverage
                        .covered
                        .iter()
                        .enumerate()
                        .any(|(index, covered)| {
                            let selected = match answer {
                                PermissionAnswer::AllowComposed { rows } => {
                                    rows.get(index).is_some_and(Option::is_some)
                                }
                                _ => true,
                            };
                            selected && covered.is_none()
                        })
                {
                    return Err(deny(DECISION_SOURCE_RULE, None));
                }
            }
        }
        if allow {
            allowed(observed, source)
        } else {
            let (guidance, by) = match decision {
                PendingDecision::Explicit(answer, by) => (answer.guidance().map(String::from), by),
                PendingDecision::MatchedRule => (None, Answerer::User),
                PendingDecision::PolicyDenied(guidance) => (Some(guidance), Answerer::User),
            };
            Err(deny_by(source, guidance, by))
        }
    }
}

impl PermissionManager {
    /// Autopilot's part in a prompt, when it is engaged on this manager. A
    /// fork never carries it, so only the session that engaged it is asked.
    fn prompt_delegate(
        &self,
        request: &PermissionRequest,
        context: &EvaluationContext,
        event_tx: &EventSender,
    ) -> Option<PromptDelegate> {
        let handle = self.autopilot().filter(AutopilotHandle::is_engaged)?;
        let reservation = reserved_note(request, context)
            .map(AutoNote::name)
            .or_else(|| {
                self.decisions()
                    .is_some_and(|decisions| decisions.is_tainted())
                    .then_some(TAINTED_RESERVATION)
            });
        let agent = requesting_agent(event_tx);
        let item = decision_item(request, &self.mode(), agent.as_deref(), reservation);
        let origin = DecisionOrigin {
            agent,
            tool_use_id: Some(request.id.clone()),
            ..DecisionOrigin::new(&handle, decision_subject(request), &item)
        };
        Some(PromptDelegate {
            reservation,
            item,
            slot: DecisionSlot::new(handle, DecisionKind::Permission, origin, event_tx),
        })
    }

    /// Acts on autopilot's call for a waiting prompt. A call cut off by a
    /// claim leaves the prompt to the human, an answer a human beat is still
    /// recorded, and a request a reload superseded raises no escalation.
    fn conclude_delegate(
        &self,
        delegate: &PromptDelegate,
        outcome: Option<DelegateOutcome>,
        registration: &PendingRegistration<'_>,
        request: &mut PermissionRequest,
        context: &EvaluationContext,
    ) {
        let Some(outcome) = outcome else {
            delegate.slot.end(DecisionOutcome::TakenOver);
            registration.note_autopilot(request, None);
            return;
        };
        let answer = delegated_answer(&outcome);
        if answer.is_err() && self.superseded(context) {
            delegate.slot.end(DecisionOutcome::Superseded);
            return;
        }
        let note = match answer {
            Ok(answer) => {
                if self.answer_as(registration.request_id, answer, Answerer::Autopilot) {
                    delegate
                        .slot
                        .record(&outcome, Some(DecisionOutcome::Applied));
                    return;
                }
                None
            }
            Err(reason) => Some(AutopilotNote::Escalated { reason }),
        };
        delegate.slot.record_open(&outcome);
        registration.note_autopilot(request, note);
    }

    /// Whether a reload replaced the context the request was reviewed in.
    fn superseded(&self, context: &EvaluationContext) -> bool {
        *self
            .context_revision
            .read()
            .unwrap_or_else(|error| error.into_inner())
            != context.revision
    }
}

/// Withdraws the lifetimes a plan may not hand out: all projects for every
/// allow, and this project for a broad one.
///
/// `commit_structured_decision` validates an answer against the lifetimes the
/// request carried, so withdrawing them here is what refuses such an answer
/// from any client, not just from a prompt that hid the keys. A narrow grant
/// kept for the project also files a conversation copy, because remembered
/// rules never reach a plan-scoped call. Denials keep theirs: a plan may not
/// widen authority, but narrowing it is always the user's to make.
pub(super) fn contain_authority_to_the_plan(request: &mut PermissionRequest) {
    for option in &mut request.options {
        if option.rule.effect != StructuredPermissionEffect::Allow {
            continue;
        }
        let broad = option.confirmation.is_some();
        option.allowed_lifetimes.retain(|lifetime| match lifetime {
            PermissionLifetime::Once | PermissionLifetime::Conversation => true,
            PermissionLifetime::Project => !broad,
            PermissionLifetime::Global => false,
        });
    }
}

fn is_gated(resource: &PermissionResource) -> bool {
    resource.protected || resource.requires_prompt
}

/// The reserved class a request falls in, which neither Auto nor autopilot
/// may ever approve, in the precedence of the prompt's own reason. It reads
/// only the request and how it was raised, so it applies in every mode.
///
/// A protected or prepared resource is reserved. The one exception is a whole
/// command line the first-party shell could not review command by command,
/// for a cause an engine reading the full text may screen. A cause that always
/// asks is reserved from any source, but only the trusted shell's facts can
/// lift the reservation.
fn reserved_note(request: &PermissionRequest, context: &EvaluationContext) -> Option<AutoNote> {
    let gated: Vec<_> = request
        .resources
        .iter()
        .filter(|resource| is_gated(resource))
        .map(ShellOpacity::of)
        .collect();
    let worst = gated.iter().flatten().max().copied();
    if context.plan_scoped {
        Some(AutoNote::Planning)
    } else if context.forced {
        Some(AutoNote::Forced)
    } else if let Some(cause) = worst.filter(|cause| !cause.screenable()) {
        Some(AutoNote::AlwaysAsks(cause))
    } else if !gated.is_empty() && (gated.contains(&None) || !trusted_shell_request(request)) {
        Some(AutoNote::Protected)
    } else {
        None
    }
}

/// Whether Auto may settle a request the policy leaves to the user, or the
/// first reason it may not, in the precedence of the prompt's own reason.
/// Past the reserved classes, only an engine may approve a gated line.
fn auto_eligibility(
    request: &PermissionRequest,
    context: &EvaluationContext,
    coverage: &RequestCoverage,
    default: DefaultEffect,
) -> AutoEligibility {
    let gated = request.resources.iter().any(is_gated);
    let note = if let Some(reserved) = reserved_note(request, context) {
        reserved
    } else if coverage.must_prompt {
        AutoNote::RuleAsks
    } else if !gated && context.force_prompt {
        AutoNote::Forced
    } else if default != DefaultEffect::Prompt {
        AutoNote::ToolDefault
    } else if !gated {
        return AutoEligibility::Direct;
    } else {
        return AutoEligibility::NeedsEngine;
    };
    AutoEligibility::Never(note)
}

/// An action the request does not accept, such as a grant of a reserved
/// request, is left to the human whatever let it through.
fn screened(mut outcome: DelegateOutcome, reserved: bool) -> DelegateOutcome {
    if let Ok(output) = &outcome.answer
        && !outcome
            .kind
            .accepted_actions(reserved)
            .contains(&output.action.name())
    {
        let refused = WireError::Refused {
            kind: outcome.kind,
            action: output.action.name().name().into(),
        };
        outcome.answer = Err(refused.to_string());
    }
    outcome
}

/// The answer the delegate gave, or why it left the prompt to the human.
fn delegated_answer(outcome: &DelegateOutcome) -> Result<PermissionAnswer, String> {
    match &outcome.answer {
        Ok(DelegateOutput {
            action: DelegateAction::Allow,
            ..
        }) => Ok(PermissionAnswer::AllowOnce),
        Ok(DelegateOutput {
            action: DelegateAction::Deny { guidance },
            ..
        }) => Ok(PermissionAnswer::DenyWithGuidance(guidance.clone())),
        Ok(DelegateOutput { reason, .. }) | Err(reason) => Err(reason.clone()),
    }
}

/// Who asked, as far as the sender says. The session's own agents share its
/// stream, so they are `None`; workflow agents and background tasks are named.
fn requesting_agent(event_tx: &EventSender) -> Option<String> {
    if let Some(workflow) = event_tx.workflow() {
        let mut agent = format!(
            "{WORKFLOW_AGENT} {}{ITEM_SEPARATOR}{WORKFLOW_CALL} {}",
            workflow.run_id, workflow.call_key
        );
        if let Some(phase) = &workflow.phase {
            let _ = write!(agent, "{ITEM_SEPARATOR}{WORKFLOW_PHASE} {phase}");
        }
        return Some(agent);
    }
    event_tx
        .task()
        .map(|task| format!("{BACKGROUND_TASK_AGENT} {}", task.task_id))
}

/// What a decision row names: the tool and the first resource it asks for.
fn decision_subject(request: &PermissionRequest) -> String {
    match request.presentation.resources.split_first() {
        None => request.tool.to_string(),
        Some((first, [])) => format!("{}: {}", request.tool, first.summary),
        Some((first, rest)) => format!(
            "{}: {} +{} {ITEM_MORE}",
            request.tool,
            first.summary,
            rest.len()
        ),
    }
}

/// The prompt as the delegate reads it, bounded: what is asked and why, who
/// asked, and whether autopilot may allow it.
fn decision_item(
    request: &PermissionRequest,
    mode: &PermissionMode,
    agent: Option<&str>,
    reservation: Option<&str>,
) -> String {
    let presentation = &request.presentation;
    let input = request.input.to_string();
    let cut = input.floor_char_boundary(MAX_ITEM_INPUT_BYTES);
    let truncated = if cut < input.len() {
        ITEM_TRUNCATED
    } else {
        ""
    };
    let mut item = format!(
        "{ITEM_TOOL_LABEL} {}\n{ITEM_INPUT_LABEL} {}{truncated}\n{ITEM_RESOURCES_LABEL}\n",
        request.tool,
        &input[..cut]
    );
    for resource in presentation.resources.iter().take(PROMPT_LOG_MAX_RESOURCES) {
        let _ = write!(
            item,
            "{ITEM_BULLET} {} {}",
            resource_kind_name(&resource.kind),
            resource.summary
        );
        if let Some(access) = &resource.access {
            let _ = write!(item, " ({access:?})");
        }
        if resource.protected {
            let _ = write!(item, " {ITEM_PROTECTED}");
        }
        item.push('\n');
    }
    let hidden = presentation
        .resources
        .len()
        .saturating_sub(PROMPT_LOG_MAX_RESOURCES);
    if hidden > 0 {
        let _ = writeln!(item, "{ITEM_BULLET} +{hidden} {ITEM_MORE}");
    }
    let _ = writeln!(
        item,
        "{ITEM_REASON_LABEL} {}",
        prompt_reason_message(&presentation.reason)
    );
    if let Some(auto) = presentation.auto {
        let _ = writeln!(item, "{ITEM_AUTO_LABEL} {}", auto.phrase());
    }
    if !presentation.advisories.is_empty() {
        let advisories = presentation
            .advisories
            .iter()
            .map(|advisory| format!("{} {:.2}", advisory.flag.name(), advisory.probability))
            .collect::<Vec<_>>()
            .join(ITEM_SEPARATOR);
        let _ = writeln!(item, "{ITEM_ADVISORIES_LABEL} {advisories}");
    }
    let _ = write!(
        item,
        "{ITEM_AGENT_LABEL} {}\n{ITEM_MODE_LABEL} {}\n{ITEM_RESERVED_LABEL} {}",
        agent.unwrap_or(MAIN_AGENT),
        mode_label(mode),
        reservation.unwrap_or(ITEM_NOT_RESERVED)
    );
    item
}

#[cfg(test)]
pub(super) mod tests {
    use super::{
        ACTIVE_PLAN_INTENT_MISMATCH, AUTOPILOT_GUIDANCE_LABEL, AutoEligibility,
        BACKGROUND_TASK_AGENT, CURRENT_DEFAULT_DENIES_REQUEST, CURRENT_POLICY_DENIES_REQUEST,
        EvaluationContext, ITEM_ADVISORIES_LABEL, ITEM_AGENT_LABEL, ITEM_AUTO_LABEL, ITEM_BULLET,
        ITEM_INPUT_LABEL, ITEM_MODE_LABEL, ITEM_MORE, ITEM_NOT_RESERVED, ITEM_PROTECTED,
        ITEM_REASON_LABEL, ITEM_RESERVED_LABEL, ITEM_RESOURCES_LABEL, ITEM_SEPARATOR,
        ITEM_TOOL_LABEL, ITEM_TRUNCATED, MAIN_AGENT, MAX_ITEM_INPUT_BYTES,
        PROMPT_LOG_MAX_RESOURCES, PendingDecision, PendingPermission, PendingRegistration,
        TAINTED_RESERVATION, UNATTENDED_ESCALATION, USER_GUIDANCE_LABEL, WORKFLOW_AGENT,
        WORKFLOW_CALL, WORKFLOW_PHASE, contain_authority_to_the_plan, decision_item,
        decision_subject, mode_label, prompt_reason_message, remove_pending, reserved_note,
        screened,
    };
    use crate::Envelope;
    use crate::autopilot::{
        ActionName, AutopilotDecision, DecisionKind, DecisionOutcome, DelegateAction,
        DelegateOutcome, DelegateOutput, GatedProvider, ScriptedProvider, Step, WireError,
        engaged_handle,
    };
    use crate::decisions::Decisions;
    use crate::mcp::kind::{McpCallFacts, McpServerKind, PausedRuns};
    use crate::permissions::tests::{
        CONTROLLED_REQUEST, FIRST_COMMAND, SECOND_COMMAND, controlled_enforcement,
        opaque_line_intent, remember_command,
    };
    use crate::permissions::{
        AutopilotNote, DECISION_SOURCE_AUTOPILOT_DENY, DECISION_SOURCE_AUTOPILOT_ONCE,
        DECISION_SOURCE_RULE, DECISION_SOURCE_USER_ABORT, DECISION_SOURCE_USER_ONCE,
        DECISION_SOURCE_YOLO, EngineFlag, PermissionAdvisory, PermissionArgumentConstraint,
        PermissionError, READ_ONLY_MCP_TOOL_OPTION, mcp_permission_intent,
    };
    use crate::types::Answerer;
    use crate::types::{AutopilotEvent, TaskProvenance, WorkflowProvenance};
    use crate::{CancelToken, EventSender};
    use caudra_config::decisions::DecisionsConfig;
    use caudra_otel::logs::EVENT_TOOL_DECISION;
    use caudra_providers::provider::Provider;
    use caudra_providers::{Billing, TokenUsage};
    use caudra_storage::id::CaudraId;
    use futures_lite::future;
    use std::fmt::Debug;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    use std::collections::BTreeMap;

    use crate::tools::native;
    use crate::tools::native::memory::{self, LOCAL_MEMORY_RESOURCE};
    use crate::tools::native::plan::{self, PlanAccess, PlanTool};
    use crate::tools::registry::Tool;
    use crate::tools::test_support::stub_ctx;
    use crate::tools::{
        CODE_TOOL_NAME, MEMORY_TOOL_NAME, PermissionIntent, PermissionScopes, SHELL_TOOL_NAME,
        ToolAudience, ToolContext,
    };
    use caudra_storage::id::SessionRef;
    use caudra_storage::local_documents::LocalDocumentStore;
    use caudra_storage::plans::PlanFile;
    use caudra_storage::sessions::SessionDatabase;
    use serde_json::{Value, json};
    use std::env;
    #[cfg(unix)]
    use std::fs::Permissions;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::{Builder, TempDir, tempdir};

    use test_case::test_case;

    use crate::permissions::tests::{
        ALLOWED_COMMANDS, BROAD_GIT_PATTERN, BUILTIN_ECHO_PATTERN, COMPLEX_COMMAND,
        CONFINED_COMMAND, COVERAGE_COMMAND, COVERAGE_PATTERN, ECHO_COMMAND, EMPTY_MCP_SCOPE,
        GIT_CONFIG, GIT_HEAD, PERMISSION_RULES_STATE_KEY, PLAN_PATH, SHELL_PROMPT_MISSING,
        SHELL_WORKDIR, THIS_COMMAND_AUTHORITY, WORKDIR_OPTION, allow_rule, allows_without_prompt,
        answer_enforcement, answer_plan_command, answer_tool_enforcement, conversation_grant,
        coverage_of, coverage_with, covered_flags, decisions, default_mgr, denied_by_rule,
        deny_rule, enforce_opaque_command_without_prompt, enforce_plan_command_without_prompt,
        enforce_plan_write_without_prompt, enforce_shell_without_prompt,
        enforce_tool_without_prompt, enforce_without_prompt, legacy_request, log_coverage,
        log_request, log_resource, make_config, mark_confined, mgr_with, pending_scope_enforcement,
        pending_tool_enforcement, persistent_manager, project_read_request,
        remote_permission_asset, shell_intent, shell_policy_rule, shell_prompt, shell_request,
        workcell_shell_subject, workdir_grant,
    };
    use crate::permissions::{
        AutoNote, CONFINED_READ_ATTRIBUTE, CONFINED_READ_AUTHORITY, CONFINED_READ_VALUE,
        CURRENT_PROJECT_ATTRIBUTE, CURRENT_PROJECT_VALUE, ComposedRow, PROMPT_REASON_ASK_RULE,
        PROMPT_REASON_FORCED, PROMPT_REASON_PROTECTED, PROMPT_REASON_UNCOVERED, PermissionAnswer,
        PermissionAuthorityProfile, PermissionExecutorKind, PermissionLifetime, PermissionManager,
        PermissionMode, PermissionPolicyError, PermissionPresentation, PermissionRequest,
        PermissionResource, PermissionResourceAccess, PermissionResourceKind, PermissionRisk,
        PermissionRowGrant, PermissionRuleRecord, PermissionSubject, PromptReason,
        RemotePermissionIdentity, ResourceCoverage, RevokedRuleScope, RuleOrigin,
        SESSION_HISTORY_RESOURCE, ShellOpacity, StructuredPermissionDecision,
        StructuredPermissionEffect, builtin_structured_rules, canonical_json,
        canonical_json_sha256, filesystem_permission_resource, is_shell_tool, normalize_scope_path,
        prompt_forcing_reason,
    };
    use crate::{AgentEvent, AgentMode};
    use caudra_config::{DefaultEffect, Effect, PermissionRule, PermissionsConfig, ToolKey};
    use caudra_storage::StateDir;
    use caudra_storage::permission_state::PermissionState;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    const REPLACEMENT_COMMAND: &str = "cargo clean";
    const RUSTFMT_COMMAND: &str = "rustfmt --edition 2024 --check src/lib.rs";
    const BROAD_RUSTFMT_PATTERN: &str = "rustfmt *";
    const NARROW_RUSTFMT_PATTERN: &str = "rustfmt --edition *";
    const AUTO_BUILTIN_ASK: &str = "git push origin main";
    const BUILTIN_PUSH_ASK: &str = "git push *";
    const PLAN_DOCUMENT: &str = "active-plan.md";
    const PLAN_CONTENT: &str = "The complete plan";
    const LOCAL_MEMORY_SCOPE: &str = "local-memory:read:architecture.md";
    const HISTORY_PROJECT: &str = "/work/project";
    const IMPOSTOR_PLUGIN: &str = "impostor";
    const PRIVILEGED_LINE: &str = "sudo cargo build";
    const DENY_GUIDANCE: &str = "run the narrower test instead";
    #[cfg(unix)]
    const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
    const DELEGATE_ALLOW: &str = r#"{"action":"allow","reason":"the user asked for the build"}"#;
    const DELEGATE_DENY: &str = r#"{"action":"deny","reason":"outside the task","guidance":"run the narrower test instead"}"#;
    const DELEGATE_ESCALATE: &str = r#"{"action":"escalate","reason":"the user must choose"}"#;
    const DELEGATE_REASON: &str = "the user asked for the build";
    const ESCALATION_REASON: &str = "the user must choose";
    const HUMAN_ALLOW: &str = "allow";
    const HUMAN_DENIAL: &str = "deny_guidance: run the narrower test instead";
    const TASK_ID: &str = "task-7";
    const INVOCATION_ID: &str = "invocation-7";
    const WORKFLOW_RUN_ID: &str = "run-3";
    const WORKFLOW_EPOCH: u64 = 1;
    const WORKFLOW_CALL_KEY: u64 = 4;
    const WORKFLOW_PHASE_NAME: &str = "review";
    const HIDDEN_RESOURCES: usize = 2;
    const ADVISORY_PROBABILITY: f64 = 0.75;
    const SOURCE_FIELD: &str = "source";
    const UNUSED_SPAN: u64 = 1;

    enum PlanIntentMismatch {
        Target,
        Access,
        Operation,
        Scope,
        Kind,
        ExtraResource,
        MissingResource,
        Action,
        MissingAction,
    }

    pub(in crate::permissions) async fn active_plan_fixture(
        remote: bool,
    ) -> (TempDir, ToolContext, PermissionIntent, Value) {
        let builder = &mut Builder::new();
        #[cfg(unix)]
        builder.permissions(Permissions::from_mode(PRIVATE_DIRECTORY_MODE));
        let root = builder
            .tempdir_in(env::temp_dir().canonicalize().unwrap())
            .unwrap();
        let mut ctx = if remote {
            let (workspace, _) =
                crate::stored_session::tests::remote_workspace(CONTROLLED_REQUEST, PLAN_CONTENT);
            let session = SessionRef::generate();
            let store = Arc::new(LocalDocumentStore::remote(
                StateDir::from_path(root.path().join("state")),
                workspace.binding(),
            ));
            let reference = store
                .create_plan(workspace.binding().project().key(), session.as_str())
                .unwrap();
            let mut ctx = stub_ctx(&AgentMode::RemotePlan(reference));
            ctx.workspace_session = Some(workspace);
            ctx.local_documents = Some(store);
            ctx.session_id = Some(session);
            ctx
        } else {
            let path = root.path().join(PLAN_DOCUMENT);
            PlanFile::new(path.clone())
                .unwrap()
                .write(PLAN_CONTENT)
                .unwrap();
            stub_ctx(&AgentMode::Plan(path))
        };
        ctx.host_cwd = Some(root.path().to_path_buf());
        let input = json!({"action": "write", "content": PLAN_CONTENT});
        let intent = PlanTool
            .parse(&input)
            .unwrap()
            .preflight(&ctx)
            .await
            .unwrap()
            .unwrap();
        (root, ctx, intent, input)
    }

    pub(in crate::permissions) async fn active_plan_read(
        ctx: &ToolContext,
    ) -> (PermissionIntent, Value) {
        let input = json!({"action": PlanAccess::Read.operation()});
        let intent = PlanTool
            .parse(&input)
            .unwrap()
            .preflight(ctx)
            .await
            .unwrap()
            .unwrap();
        (intent, input)
    }

    #[test_case(DefaultEffect::Prompt, None; "default_prompt_approves_exact_plan")]
    #[test_case(DefaultEffect::Allow, None; "default_allow_approves_exact_plan")]
    #[test_case(DefaultEffect::Deny, None; "default_deny_blocks_exact_plan")]
    #[test_case(DefaultEffect::Prompt, Some(Effect::Deny); "explicit_deny_blocks_exact_plan")]
    #[test_case(DefaultEffect::Allow, Some(Effect::Deny); "deny_beats_default_allow")]
    #[test_case(DefaultEffect::Prompt, Some(Effect::Ask); "explicit_ask_prompts_exact_plan")]
    #[test_case(DefaultEffect::Allow, Some(Effect::Ask); "ask_beats_default_allow")]
    fn active_plan_approval_preserves_policy(default: DefaultEffect, effect: Option<Effect>) {
        smol::block_on(async {
            for remote in [false, true] {
                let (_root, mut ctx, intent, input) = active_plan_fixture(remote).await;
                let manager = mgr_with(
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
                    ctx.host_cwd.clone().unwrap(),
                );
                let (events, receiver) = flume::unbounded();
                ctx.event_tx = EventSender::new(events, 0);
                let (_sender, responses) = flume::unbounded();
                ctx.user_response_rx = Some(Arc::new(async_lock::Mutex::new(responses)));
                let mut enforcement = Box::pin(manager.enforce_active_plan(
                    &intent,
                    &input,
                    &ctx,
                    CONTROLLED_REQUEST,
                    None,
                ));
                if effect == Some(Effect::Ask) {
                    assert!(
                        futures_lite::future::poll_once(&mut enforcement)
                            .await
                            .is_none()
                    );
                    assert!(matches!(
                        receiver.try_recv().unwrap().event,
                        AgentEvent::PermissionRequest(_)
                    ));
                    assert!(manager.answer(CONTROLLED_REQUEST, PermissionAnswer::Deny));
                    assert!(enforcement.await.is_err());
                } else if effect == Some(Effect::Deny) || default == DefaultEffect::Deny {
                    let error = enforcement.await.unwrap_err();
                    let expected = PermissionPolicyError(if effect == Some(Effect::Deny) {
                        CURRENT_POLICY_DENIES_REQUEST.into()
                    } else {
                        CURRENT_DEFAULT_DENIES_REQUEST.into()
                    })
                    .to_string();
                    assert_eq!(error.guidance.map(|guidance| guidance.text), Some(expected));
                    assert!(receiver.is_empty());
                } else {
                    assert!(enforcement.await.is_ok());
                    assert!(receiver.is_empty());
                }
            }
        });
    }

    #[test_case(PlanIntentMismatch::Target; "other_target")]
    #[test_case(PlanIntentMismatch::Access; "missing_write_access")]
    #[test_case(PlanIntentMismatch::Operation; "wrong_operation")]
    #[test_case(PlanIntentMismatch::Scope; "wrong_scope")]
    #[test_case(PlanIntentMismatch::Kind; "wrong_resource_kind")]
    #[test_case(PlanIntentMismatch::ExtraResource; "extra_resource")]
    #[test_case(PlanIntentMismatch::MissingResource; "missing_resource")]
    #[test_case(PlanIntentMismatch::Action; "read_input_with_write_intent")]
    #[test_case(PlanIntentMismatch::MissingAction; "missing_input_action")]
    fn active_plan_rejects_mismatched_intent(field: PlanIntentMismatch) {
        smol::block_on(async {
            for remote in [false, true] {
                let (_root, ctx, mut intent, mut input) = active_plan_fixture(remote).await;
                match field {
                    PlanIntentMismatch::Target => intent.resources[0].value.push_str(".other"),
                    PlanIntentMismatch::Access => intent.resources[0].access = None,
                    PlanIntentMismatch::Operation => {
                        intent.resources[0]
                            .attributes
                            .insert("operation".into(), "read".into());
                    }
                    PlanIntentMismatch::Scope => intent.scopes.scopes.clear(),
                    PlanIntentMismatch::Kind => {
                        intent.resources[0].kind = PermissionResourceKind::Directory
                    }
                    PlanIntentMismatch::ExtraResource => {
                        intent.resources.push(intent.resources[0].clone())
                    }
                    PlanIntentMismatch::MissingResource => intent.resources.clear(),
                    PlanIntentMismatch::Action => input["action"] = json!("read"),
                    PlanIntentMismatch::MissingAction => input = json!({}),
                }
                let manager = mgr_with(
                    PermissionsConfig {
                        default: DefaultEffect::Allow,
                        ..Default::default()
                    },
                    ctx.host_cwd.clone().unwrap(),
                );
                let error = manager
                    .enforce_active_plan(&intent, &input, &ctx, CONTROLLED_REQUEST, None)
                    .await
                    .unwrap_err();
                assert_eq!(
                    error.guidance.map(|guidance| guidance.text).as_deref(),
                    Some(ACTIVE_PLAN_INTENT_MISMATCH)
                );
            }
        });
    }

    #[test_case(true; "forced_prompt")]
    #[test_case(false; "plan_containment")]
    fn active_plan_approval_preserves_prompt_flags(forced: bool) {
        smol::block_on(async {
            for remote in [false, true] {
                let (_root, ctx, mut intent, input) = active_plan_fixture(remote).await;
                intent.scopes.force_prompt = forced;
                intent.scopes.plan_scoped = !forced;
                assert!(
                    default_mgr()
                        .enforce_active_plan(&intent, &input, &ctx, CONTROLLED_REQUEST, None)
                        .await
                        .is_err()
                );
            }
        });
    }

    #[test_case(false; "build_mode")]
    #[test_case(true; "subagent_audience")]
    fn active_plan_revalidates_execution_authority(subagent: bool) {
        smol::block_on(async {
            for remote in [false, true] {
                let (_root, mut ctx, intent, input) = active_plan_fixture(remote).await;
                if subagent {
                    ctx.audience = ToolAudience::GENERAL_SUB;
                } else {
                    ctx.mode = AgentMode::Build;
                }
                assert!(
                    default_mgr()
                        .enforce_active_plan(&intent, &input, &ctx, CONTROLLED_REQUEST, None)
                        .await
                        .is_err()
                );
            }
        });
    }

    #[test_case(ToolAudience::MAIN, PlanAccess::Read, None; "main_reads")]
    #[test_case(ToolAudience::MAIN, PlanAccess::Write, None; "main_writes")]
    #[test_case(ToolAudience::GENERAL_SUB, PlanAccess::Read, None; "task_reads")]
    #[test_case(ToolAudience::GENERAL_SUB, PlanAccess::Write, Some(plan::WRITE_DENIED); "task_write_is_refused")]
    fn a_build_binding_authorizes_plan_access_by_audience(
        audience: ToolAudience,
        access: PlanAccess,
        refusal: Option<&str>,
    ) {
        smol::block_on(async {
            for remote in [false, true] {
                let (_root, mut ctx, mut intent, mut input) = active_plan_fixture(remote).await;
                if access == PlanAccess::Read {
                    (intent, input) = active_plan_read(&ctx).await;
                }
                ctx.plan = ctx.mode.plan_target();
                ctx.mode = AgentMode::Build;
                ctx.audience = audience;
                let manager = mgr_with(PermissionsConfig::default(), ctx.host_cwd.clone().unwrap());
                let guidance = manager
                    .enforce_active_plan(&intent, &input, &ctx, CONTROLLED_REQUEST, None)
                    .await
                    .err()
                    .and_then(|error| error.guidance)
                    .map(|guidance| guidance.text);
                assert_eq!(guidance.as_deref(), refusal);
            }
        });
    }

    #[test_case(false; "ordinary_intent_does_not_gain_approval")]
    #[test_case(true; "legacy_scopes_do_not_gain_approval")]
    fn active_plan_name_does_not_grant_authority(legacy: bool) {
        smol::block_on(async {
            for remote in [false, true] {
                let (_root, ctx, intent, input) = active_plan_fixture(remote).await;
                let manager = default_mgr();
                let tool = ToolKey::native(plan::NAME);
                let result = if legacy {
                    manager
                        .enforce(
                            &tool,
                            &intent.scopes,
                            &input,
                            &ctx.event_tx,
                            None,
                            CONTROLLED_REQUEST,
                            &ctx.cancel,
                            ctx.mode.plan_path(),
                        )
                        .await
                } else {
                    manager
                        .enforce_with_intent(
                            &tool,
                            &intent,
                            &input,
                            &ctx.event_tx,
                            None,
                            CONTROLLED_REQUEST,
                            &ctx.cancel,
                            ctx.mode.plan_path(),
                            None,
                            true,
                        )
                        .await
                };
                assert!(result.is_err());
            }
        });
    }

    /// Reading the session plan back never asks, whatever the default, so no
    /// response channel is needed. Only an explicit rule or a forced prompt
    /// says otherwise.
    #[test_case(DefaultEffect::Prompt, None, false; "default_prompt_reads_without_asking")]
    #[test_case(DefaultEffect::Allow, None, false; "default_allow_reads_without_asking")]
    #[test_case(DefaultEffect::Deny, None, false; "default_deny_reads_without_asking")]
    #[test_case(DefaultEffect::Allow, Some(Effect::Deny), false; "explicit_deny_refuses_the_read")]
    #[test_case(DefaultEffect::Deny, Some(Effect::Ask), false; "explicit_ask_asks_before_the_read")]
    #[test_case(DefaultEffect::Allow, None, true; "forced_prompt_asks_before_the_read")]
    fn active_plan_read_asks_only_when_told_to(
        default: DefaultEffect,
        effect: Option<Effect>,
        forced: bool,
    ) {
        smol::block_on(async {
            for remote in [false, true] {
                let (_root, mut ctx, _, _) = active_plan_fixture(remote).await;
                let (mut intent, input) = active_plan_read(&ctx).await;
                intent.scopes.force_prompt = forced;
                let manager = mgr_with(
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
                    ctx.host_cwd.clone().unwrap(),
                );
                let (events, receiver) = flume::unbounded();
                ctx.event_tx = EventSender::new(events, 0);
                let asks = forced || effect == Some(Effect::Ask);
                let _sender = asks.then(|| {
                    let (sender, responses) = flume::unbounded();
                    ctx.user_response_rx = Some(Arc::new(async_lock::Mutex::new(responses)));
                    sender
                });
                let mut enforcement = Box::pin(manager.enforce_active_plan(
                    &intent,
                    &input,
                    &ctx,
                    CONTROLLED_REQUEST,
                    None,
                ));
                if asks {
                    assert!(
                        futures_lite::future::poll_once(&mut enforcement)
                            .await
                            .is_none()
                    );
                    assert!(matches!(
                        receiver.try_recv().unwrap().event,
                        AgentEvent::PermissionRequest(_)
                    ));
                    assert!(manager.answer(CONTROLLED_REQUEST, PermissionAnswer::Deny));
                    assert!(enforcement.await.is_err());
                } else if effect == Some(Effect::Deny) {
                    assert_eq!(
                        enforcement
                            .await
                            .unwrap_err()
                            .guidance
                            .map(|guidance| guidance.text),
                        Some(
                            PermissionPolicyError(CURRENT_POLICY_DENIES_REQUEST.into()).to_string()
                        )
                    );
                    assert!(receiver.is_empty());
                } else {
                    assert!(enforcement.await.is_ok());
                    assert!(receiver.is_empty());
                }
            }
        });
    }

    /// The remote project's restrictive default ranks with a deny rule, ahead of
    /// anything that covers a request, so it refuses the plan read as well.
    #[test]
    fn a_remote_restrictive_default_refuses_the_plan_read() {
        smol::block_on(async {
            let (_root, ctx, _, _) = active_plan_fixture(true).await;
            let (intent, input) = active_plan_read(&ctx).await;
            let manager = mgr_with(PermissionsConfig::default(), ctx.host_cwd.clone().unwrap());
            manager
                .configured
                .write()
                .unwrap()
                .remote_restrictive_default = Some(DefaultEffect::Deny);
            assert_eq!(
                manager
                    .enforce_active_plan(&intent, &input, &ctx, CONTROLLED_REQUEST, None)
                    .await
                    .unwrap_err()
                    .guidance
                    .map(|guidance| guidance.text),
                Some(PermissionPolicyError(CURRENT_POLICY_DENIES_REQUEST.into()).to_string())
            );
        });
    }

    #[test_case(FIRST_COMMAND, None, false, true; "unmatched_default_ask_runs")]
    #[test_case(FIRST_COMMAND, Some(Effect::Ask), false, false; "configured_ask_prompts")]
    #[test_case(FIRST_COMMAND, Some(Effect::Deny), false, false; "configured_deny_blocks")]
    #[test_case(FIRST_COMMAND, Some(Effect::Allow), false, true; "configured_allow_runs")]
    #[test_case(AUTO_BUILTIN_ASK, None, false, false; "builtin_ask_prompts")]
    #[test_case(FIRST_COMMAND, None, true, false; "forced_prompts")]
    fn auto_only_skips_unmatched_prompts(
        command: &str,
        rule: Option<Effect>,
        forced: bool,
        allowed: bool,
    ) {
        smol::block_on(async {
            let rules = rule
                .into_iter()
                .map(|effect| shell_policy_rule(command, effect))
                .collect();
            let manager = mgr_with(make_config(rules), PathBuf::from(SHELL_WORKDIR));
            manager.set_session_mode(Some(PermissionMode::Auto));
            assert_eq!(
                enforce_shell_without_prompt(&manager, &[command], forced)
                    .await
                    .is_ok(),
                allowed
            );
        });
    }

    #[test_case(DefaultEffect::Prompt, false, true; "ask_is_candidate")]
    #[test_case(DefaultEffect::Allow, true, false; "allow_is_baseline")]
    fn auto_candidate_is_not_baseline_automatic(
        default: DefaultEffect,
        automatic: bool,
        eligible: bool,
    ) {
        let manager = mgr_with(
            PermissionsConfig {
                default,
                ..Default::default()
            },
            PathBuf::from(SHELL_WORKDIR),
        );
        manager.set_session_mode(Some(PermissionMode::Auto));
        let request = shell_request(&[FIRST_COMMAND], workcell_shell_subject());
        let context = EvaluationContext {
            revision: *manager.context_revision.read().unwrap(),
            plan_scoped: false,
            builtin_allows: true,
            force_prompt: false,
            forced: false,
            exact_plan: None,
        };
        let current = manager.current_policy(&request, &context).unwrap();
        assert_eq!(current.automatic, automatic);
        assert_eq!(current.auto, eligible.then_some(AutoEligibility::Direct));
        manager.set_session_mode(Some(PermissionMode::Ask));
        assert_eq!(
            manager
                .current_policy(&request, &context)
                .unwrap()
                .automatic,
            automatic
        );
    }

    #[test_case(true, false, false; "protected")]
    #[test_case(false, true, false; "requires_prompt")]
    #[test_case(false, false, true; "plan_scoped")]
    fn auto_preserves_resource_and_plan_prompts(
        protected: bool,
        requires_prompt: bool,
        plan_scoped: bool,
    ) {
        smol::block_on(async {
            let manager = default_mgr();
            manager.set_session_mode(Some(PermissionMode::Auto));
            let mut intent = shell_intent(&[FIRST_COMMAND]);
            intent.resources[0].protected = protected;
            intent.resources[0].requires_prompt = requires_prompt;
            intent.scopes.plan_scoped = plan_scoped;
            let (sender, receiver) = flume::unbounded();
            let events = EventSender::new(sender, 0);
            assert!(
                manager
                    .enforce_with_intent(
                        &ToolKey::native("shell"),
                        &intent,
                        &serde_json::json!({"command": FIRST_COMMAND}),
                        &events,
                        None,
                        CONTROLLED_REQUEST,
                        &CancelToken::none(),
                        None,
                        Some((workcell_shell_subject(), PermissionExecutorKind::Native)),
                        true,
                    )
                    .await
                    .is_err()
            );
            assert!(receiver.is_empty());
        });
    }

    #[test_case(PermissionMode::Ask; "ask")]
    #[test_case(PermissionMode::Auto; "auto")]
    fn default_deny_is_not_skipped(mode: PermissionMode) {
        smol::block_on(async {
            let manager = mgr_with(
                PermissionsConfig {
                    default: DefaultEffect::Deny,
                    ..Default::default()
                },
                PathBuf::from(SHELL_WORKDIR),
            );
            manager.set_session_mode(Some(mode));
            assert!(
                enforce_shell_without_prompt(&manager, &[FIRST_COMMAND], false)
                    .await
                    .is_err()
            );
        });
    }

    #[test_case(false; "unmatched_pending_call_stays_pending")]
    #[test_case(true; "forced_pending_call_stays_pending")]
    fn switching_to_auto_reevaluates_pending_calls(forced: bool) {
        smol::block_on(async {
            let manager = default_mgr();
            let scopes = crate::tools::PermissionScopes {
                scopes: vec![FIRST_COMMAND.into()],
                force_prompt: forced,
                plan_scoped: false,
            };
            let (sender, receiver) = flume::unbounded();
            let events = EventSender::new(sender, 0);
            let cancel = CancelToken::none();
            let mut enforcement =
                Box::pin(controlled_enforcement(&manager, &scopes, &events, &cancel));
            assert!(
                futures_lite::future::poll_once(&mut enforcement)
                    .await
                    .is_none()
            );
            assert!(matches!(
                receiver.try_recv().unwrap().event,
                AgentEvent::PermissionRequest(_)
            ));
            manager.toggle_auto();
            let result = futures_lite::future::poll_once(&mut enforcement).await;
            assert!(result.is_none());
            assert!(manager.answer(CONTROLLED_REQUEST, PermissionAnswer::Deny));
            assert!(enforcement.await.is_err());
        });
    }

    #[test_case(true, false; "persistent_reviewed_project")]
    #[test_case(false, false; "nonpersistent_canonical_project")]
    #[test_case(true, true; "persistent_plan_keeps_a_narrow_project_grant")]
    #[test_case(false, true; "nonpersistent_plan")]
    fn enforcement_presentation_names_only_available_reviewed_project_grants(
        persistent: bool,
        plan_scoped: bool,
    ) {
        smol::block_on(async {
            let project = tempfile::tempdir().unwrap();
            let reviewed_project = project.path().canonicalize().unwrap();
            assert_ne!(
                reviewed_project,
                std::env::current_dir().unwrap().canonicalize().unwrap()
            );
            let manager = if persistent {
                persistent_manager(
                    StateDir::from_path(project.path().join("state")),
                    project.path(),
                )
            } else {
                Arc::new(PermissionManager::new_nonpersistent(
                    PermissionsConfig::default(),
                    project.path().to_path_buf(),
                    Arc::default(),
                ))
            };
            manager.set_project(project.path());
            assert_eq!(
                manager.project().canonical_project,
                Some(reviewed_project.clone())
            );
            let scopes = crate::tools::PermissionScopes {
                scopes: vec![FIRST_COMMAND.into()],
                force_prompt: true,
                plan_scoped,
            };
            let (events, received) = flume::unbounded();
            let events = EventSender::new(events, 0);
            let cancel = CancelToken::none();
            let mut enforcement =
                Box::pin(controlled_enforcement(&manager, &scopes, &events, &cancel));
            assert!(
                futures_lite::future::poll_once(&mut enforcement)
                    .await
                    .is_none()
            );
            let AgentEvent::PermissionRequest(request) = received.try_recv().unwrap().event else {
                panic!("expected permission request");
            };
            let expected_project = persistent.then_some(reviewed_project);
            assert_eq!(request.presentation.project, expected_project);
            assert_eq!(
                manager
                    .pending_request(CONTROLLED_REQUEST)
                    .unwrap()
                    .presentation
                    .project,
                expected_project
            );
            assert!(request.options.iter().any(|option| {
                option.rule.effect == StructuredPermissionEffect::Allow
                    && option
                        .allowed_lifetimes
                        .contains(&PermissionLifetime::Project)
            }));
            let answer = if expected_project.is_some() {
                PermissionAnswer::AllowAlwaysLocal
            } else {
                PermissionAnswer::AllowOnce
            };
            assert!(manager.answer(CONTROLLED_REQUEST, answer));
            assert!(enforcement.await.is_ok());
            if expected_project.is_some() {
                let stored: Vec<_> = manager
                    .structured_rule_inventory()
                    .unwrap()
                    .into_iter()
                    .filter(|record| record.rule.lifetime == PermissionLifetime::Project)
                    .map(|record| record.project)
                    .collect();
                assert_eq!(stored, [expected_project]);
            }
        });
    }

    /// The classifier only ever kept plan mode from refusing such a line; it
    /// still cost a prompt in both modes. The builtin rule is what makes the
    /// resource covered, so nothing has to be asked.
    #[test_case(true => vec![true] ; "a confined read needs no prompt")]
    #[test_case(false => vec![false] ; "the same line unmarked still does")]
    fn a_confined_read_is_covered_by_the_builtin_rule(confined: bool) -> Vec<bool> {
        let manager = default_mgr();
        let mut request = shell_request(&[CONFINED_COMMAND], workcell_shell_subject());
        if confined {
            mark_confined(&mut request);
        }

        covered_flags(&coverage_with(
            &manager,
            &request,
            false,
            &builtin_structured_rules(),
        ))
    }

    /// A prompt has to name the authority covering a row, and this rule's
    /// selector is `Any`. Read off the selector alone it would tell the user
    /// every command is allowed, when the rule reaches only what the shell tool
    /// marked. Nothing showed the string before, because a line whose every row
    /// is confined raises no prompt to read it off.
    #[test]
    fn a_confined_read_names_the_reason_rather_than_its_selector() {
        let mut request = shell_request(&[CONFINED_COMMAND], workcell_shell_subject());
        mark_confined(&mut request);

        assert_eq!(
            coverage_with(&default_mgr(), &request, false, &builtin_structured_rules())
                .covered
                .swap_remove(0),
            Some(ResourceCoverage {
                origin: RuleOrigin::Builtin,
                authority: CONFINED_READ_AUTHORITY.into(),
                asks: false,
            })
        );
    }

    /// Coverage is only worth anything if the manager actually consults the
    /// builtin rule when it collects the applicable set, which no test that
    /// hands the rule in directly can show.
    #[test_case(true => true ; "a confined read is enforced without a responder")]
    #[test_case(false => false ; "the same line unmarked cannot be")]
    fn the_manager_applies_the_builtin_rule_it_owns(confined: bool) -> bool {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().to_path_buf();
            let manager = mgr_with(PermissionsConfig::default(), project.clone());
            let mut intent = shell_intent(&[CONFINED_COMMAND]);
            if confined {
                for resource in &mut intent.resources {
                    resource.attributes.insert(
                        CONFINED_READ_ATTRIBUTE.into(),
                        CONFINED_READ_VALUE.to_owned(),
                    );
                }
            }
            let (event_tx, _event_rx) = flume::unbounded();
            let event_tx = crate::EventSender::new(event_tx, 0);

            manager
                .enforce_with_intent(
                    &ToolKey::native("shell"),
                    &intent,
                    &serde_json::json!({}),
                    &event_tx,
                    None,
                    "confined-read",
                    &crate::CancelToken::none(),
                    None,
                    Some((workcell_shell_subject(), PermissionExecutorKind::Native)),
                    true,
                )
                .await
                .is_ok()
        })
    }

    /// The attribute is the whole justification, so it only justifies the tool
    /// that earns it. A plugin presenting the same command must not inherit it.
    #[test]
    fn a_confined_read_does_not_cross_to_another_subject() {
        let manager = default_mgr();
        let mut request = shell_request(
            &[CONFINED_COMMAND],
            PermissionSubject::Lua {
                plugin: "untrusted".into(),
                tool: "shell".into(),
                contract: "shell.execution.v1".into(),
            },
        );
        mark_confined(&mut request);

        let coverage = coverage_with(&manager, &request, false, &builtin_structured_rules());
        assert_eq!(covered_flags(&coverage), vec![false]);
    }

    fn caudra_native(contract: &str) -> PermissionSubject {
        PermissionSubject::Native {
            owner: native::OWNER.into(),
            contract: contract.into(),
        }
    }

    /// Remote notes are named by an opaque resource no path rule reaches, so
    /// the builtin rule is what lets browsing them go unasked. It covers the
    /// memory tool reading and nothing else that presents the same resource.
    #[test_case(caudra_native(memory::permission_contract()), PermissionExecutorKind::Native, PermissionResourceAccess::Read => vec![true] ; "memory_reads_are_covered")]
    #[test_case(caudra_native(memory::permission_contract()), PermissionExecutorKind::Native, PermissionResourceAccess::Write => vec![false] ; "memory_changes_are_not")]
    #[test_case(caudra_native(plan::permission_contract()), PermissionExecutorKind::Native, PermissionResourceAccess::Read => vec![false] ; "another_native_tool_is_not")]
    #[test_case(
        PermissionSubject::Lua {
            plugin: IMPOSTOR_PLUGIN.into(),
            tool: MEMORY_TOOL_NAME.into(),
            contract: memory::permission_contract().into(),
        },
        PermissionExecutorKind::Lua,
        PermissionResourceAccess::Read
        => vec![false] ; "a_plugin_named_memory_is_not"
    )]
    fn the_builtin_memory_rule_covers_only_memory_reads(
        subject: PermissionSubject,
        executor: PermissionExecutorKind,
        access: PermissionResourceAccess,
    ) -> Vec<bool> {
        let intent = PermissionIntent::new(
            PermissionScopes::single(LOCAL_MEMORY_SCOPE.into()),
            vec![PermissionResource {
                kind: PermissionResourceKind::Custom {
                    name: LOCAL_MEMORY_RESOURCE.into(),
                },
                value: LOCAL_MEMORY_SCOPE.into(),
                access: Some(access),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            }],
            PermissionRisk::Low,
        );
        let request = PermissionRequest::from_intent_with_identity(
            CONTROLLED_REQUEST.into(),
            ToolKey::native(MEMORY_TOOL_NAME),
            &intent,
            json!({}),
            Path::new(SHELL_WORKDIR),
            subject,
            executor,
        );

        covered_flags(&coverage_with(
            &default_mgr(),
            &request,
            false,
            &builtin_structured_rules(),
        ))
    }

    /// Stored session history goes unasked only as the `code` tool, and only for the project the
    /// history module marked as the session's own.
    #[test_case(caudra_native(native::code::permission_contract()), true => vec![true] ; "this_projects_history_is_covered")]
    #[test_case(caudra_native(native::code::permission_contract()), false => vec![false] ; "another_projects_history_is_not")]
    #[test_case(caudra_native(plan::permission_contract()), true => vec![false] ; "another_native_tool_is_not")]
    fn the_builtin_history_rule_covers_only_this_projects_history(
        subject: PermissionSubject,
        current_project: bool,
    ) -> Vec<bool> {
        let attributes = match current_project {
            true => BTreeMap::from([(
                CURRENT_PROJECT_ATTRIBUTE.into(),
                CURRENT_PROJECT_VALUE.into(),
            )]),
            false => BTreeMap::new(),
        };
        let intent = PermissionIntent::new(
            PermissionScopes::single(HISTORY_PROJECT.into()),
            vec![PermissionResource {
                kind: PermissionResourceKind::Custom {
                    name: SESSION_HISTORY_RESOURCE.into(),
                },
                value: HISTORY_PROJECT.into(),
                access: Some(PermissionResourceAccess::Read),
                protected: false,
                requires_prompt: false,
                attributes,
            }],
            PermissionRisk::Low,
        );
        let request = PermissionRequest::from_intent_with_identity(
            CONTROLLED_REQUEST.into(),
            ToolKey::native(CODE_TOOL_NAME),
            &intent,
            json!({}),
            Path::new(SHELL_WORKDIR),
            subject,
            PermissionExecutorKind::Native,
        );

        covered_flags(&coverage_with(
            &default_mgr(),
            &request,
            false,
            &builtin_structured_rules(),
        ))
    }

    #[test]
    fn a_local_confined_read_never_covers_a_remote_resource() {
        let manager = default_mgr();
        let asset = remote_permission_asset(
            "revision",
            "4444444444444444444444444444444444444444444444444444444444444444",
            "unused",
        );
        let identity = RemotePermissionIdentity {
            authority: asset.source.authority.clone(),
            principal: asset.source.principal.clone(),
            project: asset.source.project.clone(),
        };
        let intent = PermissionIntent::new(
            crate::tools::PermissionScopes::single("remote read".into()),
            vec![PermissionResource {
                kind: PermissionResourceKind::RemoteFile {
                    identity: identity.clone(),
                },
                value: "root\u{1f}file".into(),
                access: Some(PermissionResourceAccess::Read),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::from([(
                    CONFINED_READ_ATTRIBUTE.into(),
                    CONFINED_READ_VALUE.into(),
                )]),
            }],
            PermissionRisk::Low,
        )
        .with_authority(PermissionAuthorityProfile::RemoteResource);
        let request = PermissionRequest::from_intent_with_identity(
            "remote-confined".into(),
            ToolKey::native("file_read"),
            &intent,
            serde_json::json!({}),
            Path::new("/tmp"),
            PermissionSubject::RemoteNative {
                identity,
                owner: "caudra".into(),
                contract: "view-image/v1".into(),
            },
            PermissionExecutorKind::Native,
        );

        let coverage = coverage_with(&manager, &request, false, &builtin_structured_rules());
        assert_eq!(covered_flags(&coverage), vec![false]);
    }

    /// Belt and braces. The shell tool never marks an opaque line, and an opaque
    /// line is protected, which the rule refuses to cover on its own.
    #[test]
    fn a_protected_command_is_not_covered_even_when_marked() {
        let manager = default_mgr();
        let mut request = shell_request(&[CONFINED_COMMAND], workcell_shell_subject());
        mark_confined(&mut request);
        for resource in &mut request.resources {
            resource.protected = true;
        }

        let coverage = coverage_with(&manager, &request, false, &builtin_structured_rules());
        assert_eq!(covered_flags(&coverage), vec![false]);
    }

    /// A covered resource has to say which authority covers it: a prompt that
    /// only says "already allowed" cannot be acted on, and the lifetime alone
    /// would report configured policy as something the user granted.
    #[test]
    fn coverage_names_the_origin_and_the_authority_that_carries_it() {
        let configured = mgr_with(
            make_config(vec![shell_policy_rule(COVERAGE_PATTERN, Effect::Allow)]),
            PathBuf::from(SHELL_WORKDIR),
        );

        assert_eq!(
            coverage_of(&configured, COVERAGE_COMMAND, &[]),
            Some(ResourceCoverage {
                origin: RuleOrigin::Config,
                authority: COVERAGE_PATTERN.into(),
                asks: false,
            })
        );
        assert_eq!(
            coverage_of(&default_mgr(), ECHO_COMMAND, &[]),
            Some(ResourceCoverage {
                origin: RuleOrigin::Builtin,
                authority: BUILTIN_ECHO_PATTERN.into(),
                asks: false,
            })
        );
        assert_eq!(
            coverage_of(
                &default_mgr(),
                COVERAGE_COMMAND,
                &[conversation_grant(COVERAGE_COMMAND)]
            ),
            Some(ResourceCoverage {
                origin: RuleOrigin::Conversation,
                authority: THIS_COMMAND_AUTHORITY.into(),
                asks: false,
            })
        );
    }

    /// A deny leaves nothing covered even when an allow reaches the resource,
    /// so the prompt cannot claim an authority the call does not have.
    #[test]
    fn a_denied_resource_carries_no_coverage() {
        let manager = mgr_with(
            make_config(vec![
                shell_policy_rule(BROAD_GIT_PATTERN, Effect::Allow),
                shell_policy_rule(COVERAGE_PATTERN, Effect::Deny),
            ]),
            PathBuf::from(SHELL_WORKDIR),
        );

        assert_eq!(coverage_of(&manager, COVERAGE_COMMAND, &[]), None);
    }

    /// An ask withholds authority without erasing it, so the resource stays
    /// covered for a later grant to sweep, and names the ask that outranked
    /// the allow rather than claiming the allow.
    #[test]
    fn ask_coverage_is_reported_as_asks() {
        let manager = mgr_with(
            make_config(vec![
                shell_policy_rule(BROAD_GIT_PATTERN, Effect::Allow),
                shell_policy_rule(COVERAGE_PATTERN, Effect::Ask),
            ]),
            PathBuf::from(SHELL_WORKDIR),
        );
        let request = shell_request(&[COVERAGE_COMMAND], workcell_shell_subject());

        let coverage = coverage_with(&manager, &request, true, &[]);

        assert!(coverage.must_prompt);
        assert_eq!(
            coverage.covered[0],
            Some(ResourceCoverage {
                origin: RuleOrigin::Config,
                authority: COVERAGE_PATTERN.into(),
                asks: true,
            })
        );
    }

    #[test]
    fn command_denies_match_static_quoted_tokens() {
        let manager = mgr_with(
            PermissionsConfig {
                yolo: true,
                rules: vec![shell_policy_rule("git commit *", Effect::Deny)],
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        );
        let quoted = shell_request(&[r#"git "commit" -m message"#], workcell_shell_subject());
        assert_eq!(
            decisions(&manager, &quoted),
            vec![StructuredPermissionDecision::Deny]
        );

        // A protected command the deny does not name is no longer refused for
        // the mere existence of a deny against the tool. It is left uncovered,
        // so it prompts: no configured allow can reach a protected resource.
        let mut opaque = shell_request(&["eval command"], workcell_shell_subject());
        opaque.resources[0].protected = true;
        assert_eq!(
            decisions(&manager, &opaque),
            vec![StructuredPermissionDecision::NoMatch]
        );
        assert_eq!(
            covered_flags(&coverage_with(&manager, &opaque, true, &[])),
            [false]
        );
    }

    /// The builtin project-read allow already matched `.git/HEAD`; the resource
    /// flag vetoed it, so every session re-prompted for the repository reading
    /// its own state. `config` holds remote credentials and must keep prompting.
    #[test_case(GIT_HEAD => true ; "inert_git_metadata_needs_no_prompt")]
    #[test_case(GIT_CONFIG => false ; "git_config_still_prompts")]
    fn project_reads_of_git_metadata(relative: &str) -> bool {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().to_path_buf();
        let manager = mgr_with(PermissionsConfig::default(), cwd.clone());
        let request = project_read_request(&cwd, relative);

        let coverage = coverage_with(&manager, &request, true, &[]);
        coverage.covered.iter().all(Option::is_some) && !coverage.must_prompt
    }

    #[test]
    fn explicit_allow_overrides_the_builtin_ask_fallback() {
        let manager = mgr_with(
            make_config(vec![shell_policy_rule("rm *", Effect::Allow)]),
            PathBuf::from("/tmp"),
        );
        let request = shell_request(&["rm build.log"], workcell_shell_subject());
        let coverage = coverage_with(&manager, &request, true, &[]);

        assert_eq!(covered_flags(&coverage), vec![true]);
        assert!(!coverage.must_prompt);
    }

    #[test]
    fn empty_explicit_intent_fails_closed() {
        smol::block_on(async {
            let manager = mgr_with(
                PermissionsConfig {
                    default: DefaultEffect::Allow,
                    ..Default::default()
                },
                PathBuf::from("/tmp"),
            );
            let intent = crate::tools::PermissionIntent::new(
                crate::tools::PermissionScopes::single("missing".into()),
                Vec::new(),
                PermissionRisk::High,
            );
            let (event_tx, _event_rx) = flume::unbounded();
            let event_tx = crate::EventSender::new(event_tx, 0);
            assert!(
                manager
                    .enforce_with_intent(
                        &ToolKey::native("broken"),
                        &intent,
                        &serde_json::json!({}),
                        &event_tx,
                        None,
                        "empty-intent",
                        &crate::CancelToken::none(),
                        None,
                        None,
                        true,
                    )
                    .await
                    .is_err()
            );
        });
    }

    /// A `code` snippet runs isolated, so the resource list it declares is
    /// empty because there is nothing to declare. Failing that closed leaves a
    /// tool that cannot be called at all, which is what remote mode hit. The
    /// request is evaluated instead, so an explicit deny still stops it and the
    /// guard keeps failing closed for a tool that should have named something.
    #[test_case(false => true ; "isolated_tool_runs_without_naming_a_resource")]
    #[test_case(true => false ; "an_explicit_deny_still_stops_it")]
    fn an_unscoped_tool_may_declare_no_resources(denied: bool) -> bool {
        const ISOLATED_TOOL: &str = "code";

        smol::block_on(async {
            let rules = if denied {
                vec![caudra_config::PermissionRule {
                    tool: ToolKey::native(ISOLATED_TOOL),
                    scope: Some("*".into()),
                    effect: caudra_config::Effect::Deny,
                }]
            } else {
                Vec::new()
            };
            let manager = mgr_with(
                PermissionsConfig {
                    default: DefaultEffect::Allow,
                    rules,
                    ..Default::default()
                },
                PathBuf::from("/tmp"),
            );
            let intent = crate::tools::PermissionIntent::new(
                crate::tools::PermissionScopes::default(),
                Vec::new(),
                PermissionRisk::Low,
            );
            let (event_tx, _event_rx) = flume::unbounded();
            let event_tx = crate::EventSender::new(event_tx, 0);
            manager
                .enforce_with_intent(
                    &ToolKey::native(ISOLATED_TOOL),
                    &intent,
                    &serde_json::json!({ "code": "1 + 1" }),
                    &event_tx,
                    None,
                    "isolated-intent",
                    &crate::CancelToken::none(),
                    None,
                    None,
                    true,
                )
                .await
                .is_ok()
        })
    }

    #[test]
    fn explicit_intent_does_not_authorize_resources_from_unrelated_scopes() {
        smol::block_on(async {
            let tool = ToolKey::native("platform_tool");
            let manager = mgr_with(
                PermissionsConfig {
                    default: DefaultEffect::Deny,
                    rules: vec![PermissionRule {
                        tool: tool.clone(),
                        scope: Some("safe".into()),
                        effect: Effect::Allow,
                    }],
                    ..Default::default()
                },
                PathBuf::from("/tmp"),
            );
            let intent = crate::tools::PermissionIntent::new(
                crate::tools::PermissionScopes::single("safe".into()),
                vec![PermissionResource {
                    kind: PermissionResourceKind::Custom {
                        name: "platform".into(),
                    },
                    value: "unrelated".into(),
                    access: Some(PermissionResourceAccess::Execute),
                    protected: false,
                    requires_prompt: false,
                    attributes: BTreeMap::new(),
                }],
                PermissionRisk::High,
            );
            let (event_tx, _event_rx) = flume::unbounded();
            let event_tx = crate::EventSender::new(event_tx, 0);

            assert!(
                manager
                    .enforce_with_intent(
                        &tool,
                        &intent,
                        &serde_json::json!({}),
                        &event_tx,
                        None,
                        "scope-resource-drift",
                        &crate::CancelToken::none(),
                        None,
                        None,
                        false,
                    )
                    .await
                    .is_err()
            );
        });
    }

    #[test]
    fn structured_and_builtin_authority_combine_per_resource() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            let external = temp.path().join("external.txt");
            std::fs::create_dir(&project).unwrap();
            let project_file = project.join("project.txt");
            let tool = ToolKey::native("file_apply_patch");
            let manager = mgr_with(
                PermissionsConfig {
                    default: DefaultEffect::Deny,
                    ..Default::default()
                },
                project.clone(),
            );
            let external_resource = filesystem_permission_resource(
                PermissionResourceKind::File,
                &external,
                PermissionResourceAccess::Write,
                &project,
            );
            let external_intent = crate::tools::PermissionIntent::new(
                crate::tools::PermissionScopes::single(external.to_string_lossy().into_owned()),
                vec![external_resource.clone()],
                PermissionRisk::High,
            )
            .with_authority(PermissionAuthorityProfile::Filesystem {
                input_pointers: Vec::new(),
            });
            let seed = PermissionRequest::from_intent(
                "seed".into(),
                tool.clone(),
                &external_intent,
                serde_json::json!({}),
                &project,
            );
            let rule = seed
                .option_rule("allow_exact_resources", PermissionLifetime::Conversation)
                .unwrap();
            manager.load_structured_conversation_rules(vec![
                PermissionRuleRecord::conversation(rule).unwrap(),
            ]);

            let project_resource = filesystem_permission_resource(
                PermissionResourceKind::File,
                &project_file,
                PermissionResourceAccess::Write,
                &project,
            );
            let intent = crate::tools::PermissionIntent::new(
                crate::tools::PermissionScopes {
                    scopes: vec![
                        project_file.to_string_lossy().into_owned(),
                        external.to_string_lossy().into_owned(),
                    ],
                    force_prompt: false,
                    plan_scoped: false,
                },
                vec![project_resource, external_resource],
                PermissionRisk::High,
            )
            .with_authority(PermissionAuthorityProfile::Filesystem {
                input_pointers: Vec::new(),
            });
            let (event_tx, _event_rx) = flume::unbounded();
            let event_tx = crate::EventSender::new(event_tx, 0);

            assert!(
                manager
                    .enforce_with_intent(
                        &tool,
                        &intent,
                        &serde_json::json!({}),
                        &event_tx,
                        None,
                        "mixed-authority",
                        &crate::CancelToken::none(),
                        None,
                        None,
                        true,
                    )
                    .await
                    .is_ok()
            );
        });
    }

    #[test]
    fn project_cwd_returns_the_canonical_session_root() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let manager = mgr_with(PermissionsConfig::default(), project.clone());

        assert_eq!(manager.project_cwd(), project.canonicalize().unwrap());
    }

    #[test]
    fn explicit_intent_enforcement_uses_typed_resources_and_strict_identity() {
        smol::block_on(async {
            let manager = default_mgr();
            let intent = crate::tools::PermissionIntent::new(
                crate::tools::PermissionScopes::single("legacy-scope".into()),
                vec![PermissionResource {
                    kind: PermissionResourceKind::Custom {
                        name: "platform".into(),
                    },
                    value: "resource".into(),
                    access: Some(PermissionResourceAccess::Execute),
                    protected: false,
                    requires_prompt: false,
                    attributes: BTreeMap::new(),
                }],
                PermissionRisk::High,
            );
            let subject = PermissionSubject::Native {
                owner: "first-party".into(),
                contract: "platform/v1".into(),
            };
            let input = serde_json::json!({"value": "resource"});
            let request = PermissionRequest::from_intent_with_identity(
                "seed".into(),
                ToolKey::native("platform_tool"),
                &intent,
                input.clone(),
                Path::new("/tmp"),
                subject.clone(),
                PermissionExecutorKind::Native,
            );
            let rule = request
                .option_rule("allow_exact", PermissionLifetime::Conversation)
                .unwrap();
            manager.load_structured_conversation_rules(vec![
                PermissionRuleRecord::conversation(rule).unwrap(),
            ]);
            let (event_tx, _event_rx) = flume::unbounded();
            let event_tx = crate::EventSender::new(event_tx, 0);

            let allowed = manager
                .enforce_with_intent(
                    &ToolKey::native("platform_tool"),
                    &intent,
                    &input,
                    &event_tx,
                    None,
                    "matching",
                    &crate::CancelToken::none(),
                    None,
                    Some((subject, PermissionExecutorKind::Native)),
                    false,
                )
                .await;
            assert!(allowed.is_ok());

            let denied = manager
                .enforce_with_intent(
                    &ToolKey::native("platform_tool"),
                    &intent,
                    &input,
                    &event_tx,
                    None,
                    "different-contract",
                    &crate::CancelToken::none(),
                    None,
                    Some((
                        PermissionSubject::Native {
                            owner: "first-party".into(),
                            contract: "platform/v2".into(),
                        },
                        PermissionExecutorKind::Native,
                    )),
                    false,
                )
                .await;
            assert!(denied.is_err());
        });
    }

    #[test_case(vec!["cd /tmp", "cargo test"], vec!["cd *", "cargo *"], true ; "all_allowed")]
    #[test_case(vec!["cd /tmp", "cargo test"], vec!["cargo *"], false ; "missing_rule")]
    fn compound_check(scopes: Vec<&str>, rules: Vec<&str>, expect_allowed: bool) {
        let mgr = mgr_with(
            make_config(rules.into_iter().map(allow_rule).collect()),
            PathBuf::from(SHELL_WORKDIR),
        );
        let request = shell_request(&scopes, workcell_shell_subject());
        assert_eq!(allows_without_prompt(&mgr, &request), expect_allowed);
    }

    #[test]
    fn compound_denied_if_any_segment_denied() {
        let mgr = mgr_with(
            make_config(vec![
                allow_rule("cd *"),
                allow_rule("cargo *"),
                deny_rule("rm *"),
            ]),
            PathBuf::from(SHELL_WORKDIR),
        );
        let request = shell_request(
            &["cd /tmp", "cargo test", "rm -rf /"],
            workcell_shell_subject(),
        );
        assert!(denied_by_rule(&mgr, &request));
    }

    #[test]
    fn complex_constructs_force_prompt_even_with_allow_star() {
        smol::block_on(async {
            let mgr = mgr_with(
                make_config(vec![allow_rule("*")]),
                PathBuf::from(SHELL_WORKDIR),
            );
            let request = shell_request(&[COMPLEX_COMMAND], workcell_shell_subject());
            assert!(allows_without_prompt(&mgr, &request));
            assert!(
                enforce_shell_without_prompt(&mgr, &[COMPLEX_COMMAND], true)
                    .await
                    .is_err()
            );
        });
    }

    #[test]
    fn bash_workdir_scope_still_matches_legacy_command_deny() {
        let mgr = mgr_with(
            PermissionsConfig {
                yolo: true,
                rules: vec![deny_rule("git push --force")],
                ..PermissionsConfig::default()
            },
            PathBuf::from("/tmp"),
        );
        let workdir = "/tmp/project # caudra-workdir[1]=a";
        let scope = format!(
            "git push --force # caudra-workdir[{}]={workdir} # caudra-frame[{}]",
            workdir.len(),
            workdir.len(),
        );
        let request = legacy_request(&mgr, ToolKey::native("bash"), &[&scope]);
        assert!(denied_by_rule(&mgr, &request));
    }

    #[test_case("write", "/tmp/file.txt" => true ; "write_in_cwd")]
    #[test_case("write", "/etc/passwd" => false ; "write_outside_cwd")]
    #[test_case("task", "task:research" => true ; "task_allowed")]
    #[test_case("skill", r#"{"name":"caudra-workflow-dev"}"# => true ; "skill_allowed")]
    #[test_case("bash", "cargo test" => false ; "bash_prompts")]
    #[test_case("bash", "echo hi" => true ; "literal_echo_allowed")]
    #[test_case("shell", "echo hi" => true ; "literal_echo_allowed_for_shell")]
    #[test_case("bash", "echo $HOME" => false ; "expanding_echo_prompts")]
    fn builtin_check(tool: &str, scope: &str) -> bool {
        let manager = default_mgr();
        let key = ToolKey::native(tool);
        let request = if is_shell_tool(&key) {
            shell_request(&[scope], workcell_shell_subject())
        } else {
            legacy_request(&manager, key, &[scope])
        };
        allows_without_prompt(&manager, &request)
    }

    /// The scratch root is pre-allowed the way the project is, and the shared
    /// temp root around it is not. The allowed path sits under a project id
    /// that is not the current one, because that is the case `/cd` creates:
    /// `TMPDIR` keeps pointing at the directory startup chose while these rules
    /// are rebuilt for somewhere else, and the write has to stay covered.
    #[test_case(true => true ; "any_project_inside_the_scratch_root")]
    #[test_case(false => false ; "beside_the_scratch_root")]
    fn scratch_writes_skip_the_prompt_that_the_temp_root_still_earns(inside: bool) -> bool {
        const OTHER_PROJECT: &str = "other-project-0123456789abcdef";
        const SCRATCH_NOTE: &str = "note.md";
        const SIBLING_NOTE: &str = "caudra-scratch-sibling.md";

        let _scratch_mode = crate::scratch::ScratchGuard::local();
        let project = tempfile::tempdir().unwrap();
        let manager = mgr_with(PermissionsConfig::default(), project.path().to_path_buf());
        let path = if inside {
            caudra_storage::paths::scratch_root()
                .unwrap()
                .join(OTHER_PROJECT)
                .join(SCRATCH_NOTE)
        } else {
            std::env::temp_dir().join(SIBLING_NOTE)
        };
        let request = legacy_request(
            &manager,
            ToolKey::native("write"),
            &[&path.to_string_lossy()],
        );

        allows_without_prompt(&manager, &request)
    }

    #[test]
    fn builtin_echo_allow_requires_a_bundled_implementation() {
        let manager = default_mgr();
        let request = shell_request(&["echo hi"], workcell_shell_subject());

        assert_eq!(
            covered_flags(&coverage_with(&manager, &request, true, &[])),
            [true]
        );
        assert_eq!(
            covered_flags(&coverage_with(&manager, &request, false, &[])),
            [false]
        );
    }

    #[test]
    fn builtin_echo_allow_covers_only_the_literal_command_in_a_chain() {
        let manager = default_mgr();
        let request = shell_request(&["echo hi", "rm -rf build"], workcell_shell_subject());
        let coverage = coverage_with(&manager, &request, true, &[]);

        assert_eq!(covered_flags(&coverage), vec![true, false]);
        assert!(coverage.must_prompt);
    }

    #[test]
    fn builtin_allows_apply_only_to_bundled_implementations() {
        let manager = default_mgr();
        let request = PermissionRequest::from_legacy(
            "task".into(),
            ToolKey::native("task"),
            vec!["{}".into()],
            serde_json::Value::Null,
            Path::new("/tmp"),
            false,
        );

        assert_eq!(
            covered_flags(&coverage_with(&manager, &request, true, &[])),
            [true]
        );
        assert_eq!(
            covered_flags(&coverage_with(&manager, &request, false, &[])),
            [false]
        );
    }

    #[test]
    fn path_traversal_prompts() {
        let path = normalize_scope_path("/tmp/../etc/passwd");
        let manager = default_mgr();
        let request = legacy_request(&manager, ToolKey::native("write"), &[&path]);
        assert!(!denied_by_rule(&manager, &request));
        assert!(!allows_without_prompt(&manager, &request));
    }

    #[test]
    fn force_prompt_skips_allow_rules() {
        smol::block_on(async {
            let mgr = mgr_with(
                make_config(vec![allow_rule("cargo *"), allow_rule("git *")]),
                PathBuf::from(SHELL_WORKDIR),
            );
            let request = shell_request(&ALLOWED_COMMANDS, workcell_shell_subject());
            assert!(allows_without_prompt(&mgr, &request));
            assert!(
                enforce_shell_without_prompt(&mgr, &ALLOWED_COMMANDS, false)
                    .await
                    .is_ok()
            );
            assert!(
                enforce_shell_without_prompt(&mgr, &ALLOWED_COMMANDS, true)
                    .await
                    .is_err()
            );
        });
    }

    #[test]
    fn deny_wins_over_force_prompt() {
        let mgr = mgr_with(
            make_config(vec![deny_rule("rm *")]),
            PathBuf::from(SHELL_WORKDIR),
        );
        let mut forced = shell_intent(&["rm -rf /"]);
        forced.scopes.force_prompt = true;
        let request = PermissionRequest::from_intent_with_identity(
            "forced-request".into(),
            ToolKey::native("shell"),
            &forced,
            serde_json::json!({"command": "rm -rf /", "workdir": SHELL_WORKDIR}),
            Path::new(SHELL_WORKDIR),
            workcell_shell_subject(),
            PermissionExecutorKind::Native,
        );
        assert!(denied_by_rule(&mgr, &request));
    }

    #[test]
    fn partial_coverage_prompts_only_the_uncovered_commands() {
        let mgr = mgr_with(
            make_config(vec![allow_rule("cargo *")]),
            PathBuf::from(SHELL_WORKDIR),
        );
        let request = shell_request(&["cargo test", "git push", "ls"], workcell_shell_subject());
        let coverage = coverage_with(&mgr, &request, true, &[]);
        assert_eq!(covered_flags(&coverage), vec![true, false, false]);
        assert!(coverage.must_prompt);
    }

    #[test]
    fn mcp_prompt_preserves_input_larger_than_200_bytes() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let input = serde_json::json!({
                "query": "x".repeat(512),
                "nested": {"z": 1, "a": 2}
            });
            let scope = canonical_json(&input);
            assert!(scope.len() > 200);
            let scopes = crate::tools::PermissionScopes::single(scope.clone());
            let (event_tx, event_rx) = flume::unbounded::<crate::Envelope>();
            let event_tx = crate::EventSender::new(event_tx, 0);
            let (_answer_tx, answer_rx) = flume::unbounded();
            let answer_rx = Arc::new(async_lock::Mutex::new(answer_rx));
            let task = smol::spawn({
                let manager = Arc::clone(&manager);
                let input = input.clone();
                let scopes = scopes.clone();
                let answer_rx = Arc::clone(&answer_rx);
                async move {
                    manager
                        .enforce(
                            &ToolKey::parse("server.lookup").unwrap(),
                            &scopes,
                            &input,
                            &event_tx,
                            Some(&answer_rx),
                            "request-id",
                            &crate::CancelToken::none(),
                            None,
                        )
                        .await
                }
            });
            let event = event_rx.recv_async().await.unwrap().event;
            assert!(manager.answer("request-id", PermissionAnswer::Deny));
            let result = task.await;
            assert!(result.is_err());

            let AgentEvent::PermissionRequest(request) = event else {
                panic!("expected permission request, got {event:?}");
            };
            assert_eq!(request.input, input);
            assert_eq!(request.scopes, [scope]);
            assert_eq!(request.input_digest, canonical_json_sha256(&request.input));
            assert!(request.scopes[0].len() > 200);
            assert!(request.options.iter().any(|option| option.broad));
            assert!(
                request
                    .options
                    .iter()
                    .filter(|option| option.broad)
                    .all(|option| !option.is_default)
            );
        });
    }

    /// A composed answer files one rule per command, so a sibling waiting on
    /// the second of them is only swept if the whole set is consulted.
    #[test]
    fn a_sibling_is_swept_by_any_rule_the_answer_filed() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let first_command = "cargo build";
            let second_command = "npm test";
            let (first, first_events) = pending_scope_enforcement(
                Arc::clone(&manager),
                "first",
                "bash",
                crate::tools::PermissionScopes {
                    scopes: vec![first_command.into(), second_command.into()],
                    force_prompt: false,
                    plan_scoped: false,
                },
                serde_json::json!({"command": format!("{first_command} && {second_command}")}),
            );
            let (second, second_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "second",
                "bash",
                second_command.into(),
                serde_json::json!({"command": second_command}),
            );
            first_events.recv_async().await.unwrap();
            second_events.recv_async().await.unwrap();
            assert_eq!(manager.pending_count(), 2);

            assert!(manager.answer(
                "first",
                PermissionAnswer::AllowComposed {
                    rows: ComposedRow::uniform(
                        vec![
                            Some(PermissionRowGrant::Offered("command_exact_0".into())),
                            Some(PermissionRowGrant::Offered("command_exact_1".into())),
                        ],
                        &PermissionLifetime::Conversation,
                    ),
                }
            ));
            assert!(matches!(
                second_events.recv_async().await.unwrap().event,
                AgentEvent::PermissionRequestResolved { request_id, .. } if request_id == "second"
            ));
            assert!(first.await.is_ok());
            assert!(second.await.is_ok());
        });
    }

    /// Once an answer is saved, only the rows it remembered have to be covered:
    /// a row left to this call runs without a rule of its own.
    #[test]
    fn post_save_check_reads_remembered_rows() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let (call, events) = pending_scope_enforcement(
                Arc::clone(&manager),
                "mixed",
                "bash",
                crate::tools::PermissionScopes {
                    scopes: vec!["cargo build".into(), "npm test".into()],
                    force_prompt: false,
                    plan_scoped: false,
                },
                serde_json::json!({"command": "cargo build && npm test"}),
            );
            events.recv_async().await.unwrap();

            assert!(manager.answer(
                "mixed",
                PermissionAnswer::AllowComposed {
                    rows: vec![
                        Some(ComposedRow {
                            grant: PermissionRowGrant::Offered("command_exact_0".into()),
                            lifetime: PermissionLifetime::Conversation,
                        }),
                        None,
                    ],
                }
            ));
            assert!(call.await.is_ok());
        });
    }

    #[test]
    fn deny_rule_with_none_scope_blocks_everything() {
        let mgr = mgr_with(
            make_config(vec![PermissionRule {
                tool: ToolKey::native("bash"),
                scope: None,
                effect: Effect::Deny,
            }]),
            PathBuf::from(SHELL_WORKDIR),
        );
        let request = shell_request(&["anything"], workcell_shell_subject());
        assert!(denied_by_rule(&mgr, &request));
    }

    #[test]
    fn wildcard_deny_blocks_all_tools() {
        let mgr = mgr_with(
            make_config(vec![PermissionRule {
                tool: ToolKey::Wildcard,
                scope: None,
                effect: Effect::Deny,
            }]),
            PathBuf::from("/tmp"),
        );
        // Any deny wins: Wildcard deny blocks everything including builtins
        let command = shell_request(&["ls"], workcell_shell_subject());
        let write = legacy_request(&mgr, ToolKey::native("write"), &["/tmp/x"]);
        assert!(denied_by_rule(&mgr, &command));
        assert!(denied_by_rule(&mgr, &write));
    }

    #[test]
    fn mcp_server_wildcard_matches_all_server_tools() {
        let mgr = mgr_with(
            make_config(vec![PermissionRule {
                tool: ToolKey::McpServer {
                    server: "deepwiki".into(),
                },
                scope: None,
                effect: Effect::Allow,
            }]),
            PathBuf::from("/tmp"),
        );
        for tool in ["search", "web_search"] {
            let request = legacy_request(
                &mgr,
                ToolKey::McpTool {
                    server: "deepwiki".into(),
                    tool: tool.into(),
                },
                &[EMPTY_MCP_SCOPE],
            );
            assert!(allows_without_prompt(&mgr, &request));
        }
    }

    #[test]
    fn mcp_server_wildcard_does_not_match_other_server() {
        let mgr = mgr_with(
            make_config(vec![PermissionRule {
                tool: ToolKey::McpServer {
                    server: "deepwiki".into(),
                },
                scope: None,
                effect: Effect::Allow,
            }]),
            PathBuf::from("/tmp"),
        );
        let request = legacy_request(
            &mgr,
            ToolKey::McpTool {
                server: "github".into(),
                tool: "search".into(),
            },
            &[EMPTY_MCP_SCOPE],
        );
        assert!(!allows_without_prompt(&mgr, &request));
    }

    #[test_case("write", true ; "write_tool_allowed")]
    #[test_case("edit", true ; "edit_tool_allowed")]
    #[test_case("bash", false ; "non_write_tool_prompts")]
    fn plan_path_auto_allows_file_write_tools_only(tool: &str, expect_allowed: bool) {
        smol::block_on(async {
            let mgr = default_mgr();
            assert_eq!(
                enforce_plan_write_without_prompt(&mgr, tool, &[PLAN_PATH])
                    .await
                    .is_ok(),
                expect_allowed,
            );
        });
    }

    #[test]
    fn plan_path_multi_scope_all_must_match() {
        smol::block_on(async {
            let mgr = default_mgr();
            assert!(
                enforce_plan_write_without_prompt(&mgr, "write", &[PLAN_PATH, PLAN_PATH])
                    .await
                    .is_ok()
            );
            assert!(
                enforce_plan_write_without_prompt(&mgr, "write", &[PLAN_PATH, "/etc/passwd"])
                    .await
                    .is_err()
            );
        });
    }

    /// The point of plan containment: one approval while planning is enough to
    /// keep exploring, so the model can run the scripts the plan needs.
    #[test]
    fn a_conversation_grant_covers_later_plan_scoped_commands() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let manager =
                persistent_manager(StateDir::from_path(temp.path().join("state")), &project);
            assert!(
                enforce_plan_command_without_prompt(&manager, &project, "cargo check > /tmp/out")
                    .await
                    .is_err()
            );

            let (accepted, granted, _) = answer_plan_command(
                Arc::clone(&manager),
                &project,
                "cargo check",
                workdir_grant(PermissionLifetime::Conversation),
            )
            .await;
            assert!(accepted);
            assert!(granted.is_ok());

            assert!(
                enforce_plan_command_without_prompt(&manager, &project, "python3 explore.py")
                    .await
                    .is_ok()
            );
        });
    }

    /// Containment cuts the other way too: authority the plan never asked for
    /// does not apply to it, however durable that authority is.
    #[test]
    fn a_project_grant_does_not_cover_a_plan_scoped_command() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let manager =
                persistent_manager(StateDir::from_path(temp.path().join("state")), &project);
            let command = "cargo check > /tmp/out";

            answer_enforcement(
                Arc::clone(&manager),
                "cargo check",
                serde_json::json!({"command": "cargo check"}),
                workdir_grant(PermissionLifetime::Project),
            )
            .await
            .unwrap();

            assert!(
                enforce_opaque_command_without_prompt(&manager, &project, command)
                    .await
                    .is_ok()
            );
            assert!(
                enforce_plan_command_without_prompt(&manager, &project, command)
                    .await
                    .is_err()
            );
        });
    }

    #[test]
    fn yolo_runs_plan_scoped_commands_without_storing_rules() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let manager =
                persistent_manager(StateDir::from_path(temp.path().join("state")), &project);
            manager.set_seed_mode(PermissionMode::Yolo);

            assert!(
                enforce_plan_command_without_prompt(&manager, &project, "python3 - <<'PY'")
                    .await
                    .is_ok()
            );
            assert!(manager.structured_rule_inventory().unwrap().is_empty());
        });
    }

    /// A narrow grant kept for the project while planning also files a
    /// conversation copy, which is what lets the rest of this plan use it.
    #[test]
    fn a_narrow_project_grant_while_planning_covers_the_rest_of_the_plan() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let manager =
                persistent_manager(StateDir::from_path(temp.path().join("state")), &project);
            let command = "cargo check";

            let (accepted, granted, _) = answer_plan_command(
                Arc::clone(&manager),
                &project,
                command,
                PermissionAnswer::AllowAlwaysLocal,
            )
            .await;
            assert!(accepted);
            assert!(granted.is_ok());

            let lifetimes: Vec<_> = manager
                .structured_rule_inventory()
                .unwrap()
                .into_iter()
                .map(|record| record.rule.lifetime)
                .collect();
            assert_eq!(
                lifetimes,
                [
                    PermissionLifetime::Conversation,
                    PermissionLifetime::Project
                ]
            );
            assert!(
                enforce_plan_command_without_prompt(&manager, &project, command)
                    .await
                    .is_ok()
            );
        });
    }

    #[test_case(workdir_grant(PermissionLifetime::Project) ; "a broad rung for the project")]
    #[test_case(workdir_grant(PermissionLifetime::Global) ; "a broad rung for all projects")]
    #[test_case(PermissionAnswer::AllowAlwaysGlobal ; "a narrow rung for all projects")]
    fn a_plan_scoped_prompt_refuses_what_a_plan_may_not_keep(answer: PermissionAnswer) {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let manager =
                persistent_manager(StateDir::from_path(temp.path().join("state")), &project);

            let (accepted, _, request) =
                answer_plan_command(Arc::clone(&manager), &project, "cargo check", answer).await;

            assert!(!accepted);
            assert!(manager.structured_rule_inventory().unwrap().is_empty());
            let offered = request
                .options
                .iter()
                .find(|option| option.id == WORKDIR_OPTION)
                .expect("the workdir authority is offered");
            assert_eq!(
                offered.allowed_lifetimes,
                vec![PermissionLifetime::Conversation]
            );
        });
    }

    /// A written pattern rides the exact rung, which a plan may keep for the
    /// project, so a pattern broad enough to need confirming is held to the
    /// conversation on its own.
    #[test_case(BROAD_RUSTFMT_PATTERN, PermissionLifetime::Project, true, false ; "a broad pattern for the project while planning")]
    #[test_case(BROAD_RUSTFMT_PATTERN, PermissionLifetime::Conversation, true, true ; "a broad pattern for the conversation while planning")]
    #[test_case(BROAD_RUSTFMT_PATTERN, PermissionLifetime::Project, false, true ; "a broad pattern for the project outside a plan")]
    #[test_case(NARROW_RUSTFMT_PATTERN, PermissionLifetime::Project, true, true ; "a narrow pattern for the project while planning")]
    fn a_plan_holds_a_broad_written_pattern_to_the_conversation(
        pattern: &str,
        lifetime: PermissionLifetime,
        plan_scoped: bool,
        accepted: bool,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let manager =
            persistent_manager(StateDir::from_path(temp.path().join("state")), temp.path());
        let mut request = shell_request(&[RUSTFMT_COMMAND], workcell_shell_subject());
        if plan_scoped {
            contain_authority_to_the_plan(&mut request);
        }
        let answer = PermissionAnswer::AllowComposed {
            rows: vec![Some(ComposedRow {
                grant: PermissionRowGrant::Written(pattern.into()),
                lifetime,
            })],
        };

        assert_eq!(
            manager
                .commit_structured_decision(&request, &answer, Some(temp.path()), plan_scoped)
                .is_ok(),
            accepted
        );
    }

    /// Containment narrows. A deny is narrowing, so the plan still obeys it
    /// even though the conversation grant would otherwise have covered it.
    #[test]
    fn a_project_deny_outranks_a_plan_scoped_conversation_grant() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let manager =
                persistent_manager(StateDir::from_path(temp.path().join("state")), &project);
            let denied = "cargo check > /tmp/out";

            let (denied_accepted, refused, _) = answer_plan_command(
                Arc::clone(&manager),
                &project,
                denied,
                PermissionAnswer::DenyAlwaysLocal,
            )
            .await;
            assert!(denied_accepted);
            assert!(refused.is_err());
            let (accepted, _, _) = answer_plan_command(
                Arc::clone(&manager),
                &project,
                "cargo check",
                workdir_grant(PermissionLifetime::Conversation),
            )
            .await;
            assert!(accepted);

            // The grant is live for the workdir, and still cannot reach what
            // the project denied.
            assert!(
                enforce_plan_command_without_prompt(&manager, &project, "python3 explore.py")
                    .await
                    .is_ok()
            );
            assert!(
                enforce_plan_command_without_prompt(&manager, &project, denied)
                    .await
                    .is_err()
            );
        });
    }

    #[test]
    fn broad_shell_authority_silences_opaque_command_prompts() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let manager =
                persistent_manager(StateDir::from_path(temp.path().join("state")), &project);
            let opaque = "cargo check > /tmp/out";

            assert!(
                enforce_opaque_command_without_prompt(&manager, &project, opaque)
                    .await
                    .is_err()
            );

            answer_enforcement(
                Arc::clone(&manager),
                "cargo check",
                serde_json::json!({"command": "cargo check"}),
                PermissionAnswer::AllowOption {
                    option_id: "allow_commands_in_workdir".into(),
                    lifetime: PermissionLifetime::Conversation,
                },
            )
            .await
            .unwrap();

            assert!(
                enforce_opaque_command_without_prompt(&manager, &project, opaque)
                    .await
                    .is_ok()
            );
        });
    }

    #[test]
    fn exact_global_rule_matches_after_restart_without_storing_raw_input() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let state_dir = StateDir::from_path(temp.path().join("state"));
            let secret = "cargo test --token super-secret-value";
            let input = serde_json::json!({"command": secret});
            let manager = persistent_manager(state_dir.clone(), &project);

            answer_enforcement(
                Arc::clone(&manager),
                secret,
                input.clone(),
                PermissionAnswer::AllowAlwaysGlobal,
            )
            .await
            .unwrap();

            let state = PermissionState::open(&state_dir).unwrap();
            let serialized = serde_json::to_string(state.records()).unwrap();
            assert!(!serialized.contains(secret));
            assert!(!serialized.contains("super-secret-value"));
            drop(manager);

            let restarted = persistent_manager(state_dir, &project);
            enforce_without_prompt(&restarted, secret, input.clone())
                .await
                .unwrap();
            assert!(
                enforce_without_prompt(
                    &restarted,
                    "cargo test --token changed",
                    serde_json::json!({"command": "cargo test --token changed"}),
                )
                .await
                .is_err()
            );
        });
    }

    #[test]
    fn url_subtree_project_rule_survives_restart_with_sanitized_review() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let state_dir = StateDir::from_path(temp.path().join("state"));
            let manager = persistent_manager(state_dir.clone(), &project);
            let approved = "https://example.com/docs/page?token=secret";

            answer_tool_enforcement(
                Arc::clone(&manager),
                "webfetch",
                approved,
                serde_json::json!({"url": approved}),
                PermissionAnswer::AllowOption {
                    option_id: "allow_url_subtree".into(),
                    lifetime: PermissionLifetime::Project,
                },
            )
            .await
            .unwrap();
            let state = PermissionState::open(&state_dir).unwrap();
            let stored = serde_json::to_string(state.records()).unwrap();
            assert!(stored.contains("example.com"));
            assert!(
                !serde_json::to_string(&state.records()[0].rule)
                    .unwrap()
                    .contains("example.com")
            );
            assert!(!stored.contains("secret"));
            drop(manager);

            let restarted = persistent_manager(state_dir, &project);
            let descendant = "https://example.com/docs/page/child?other=value";
            enforce_tool_without_prompt(
                &restarted,
                "webfetch",
                descendant,
                serde_json::json!({"url": descendant, "timeout": 10}),
            )
            .await
            .unwrap();
            assert!(
                enforce_tool_without_prompt(
                    &restarted,
                    "webfetch",
                    "https://example.com/docs/sibling",
                    serde_json::json!({"url": "https://example.com/docs/sibling"}),
                )
                .await
                .is_err()
            );
        });
    }

    #[test]
    fn revocation_is_durable_and_visible_to_live_managers() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let state_dir = StateDir::from_path(temp.path().join("state"));
            let input = serde_json::json!({"command": "cargo check"});
            let first = persistent_manager(state_dir.clone(), &project);
            let second = persistent_manager(state_dir.clone(), &project);
            answer_enforcement(
                Arc::clone(&first),
                "cargo check",
                input.clone(),
                PermissionAnswer::AllowAlwaysLocal,
            )
            .await
            .unwrap();
            let id = second.structured_rule_inventory().unwrap()[0].id.clone();

            assert_eq!(
                second.revoke_structured_rule(&id).unwrap(),
                Some(RevokedRuleScope::Project)
            );
            assert!(first.structured_rule_inventory().unwrap().is_empty());
            drop(first);
            drop(second);

            let restarted = persistent_manager(state_dir, &project);
            assert!(restarted.structured_rule_inventory().unwrap().is_empty());
            assert!(
                enforce_without_prompt(&restarted, "cargo check", input)
                    .await
                    .is_err()
            );
        });
    }

    /// A second process writes straight to the store, so the manager's cached
    /// records are the only thing that could answer. Both directions have to
    /// reach it without a restart.
    #[test]
    fn live_managers_follow_rules_written_by_another_process() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let state_dir = StateDir::from_path(temp.path().join("state"));
            let input = serde_json::json!({"command": "cargo check"});
            let manager = persistent_manager(state_dir.clone(), &project);
            answer_enforcement(
                Arc::clone(&manager),
                "cargo check",
                input.clone(),
                PermissionAnswer::AllowAlwaysLocal,
            )
            .await
            .unwrap();
            let granted = manager.structured_rule_inventory().unwrap().remove(0);
            let mut elsewhere = PermissionState::open(&state_dir).unwrap();

            assert!(elsewhere.revoke(&granted.id).unwrap());
            assert!(
                enforce_without_prompt(&manager, "cargo check", input.clone())
                    .await
                    .is_err()
            );

            elsewhere
                .insert(granted.project.clone(), granted.rule.clone())
                .unwrap();
            assert!(
                enforce_without_prompt(&manager, "cargo check", input)
                    .await
                    .is_ok()
            );
        });
    }

    #[test]
    fn corrupt_store_fails_closed_without_overwrite() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            let state_path = temp.path().join("state");
            std::fs::create_dir(&project).unwrap();
            let state_dir = StateDir::from_path(state_path);
            let database = SessionDatabase::open_state(&state_dir).unwrap();
            database
                .global_state_set(PERMISSION_RULES_STATE_KEY, &"corrupt")
                .unwrap();
            let manager = persistent_manager(state_dir, &project);

            assert!(
                enforce_without_prompt(
                    &manager,
                    "cargo test",
                    serde_json::json!({"command": "cargo test"}),
                )
                .await
                .is_err()
            );
            assert_eq!(
                database
                    .global_state_get::<String>(PERMISSION_RULES_STATE_KEY)
                    .unwrap(),
                Some("corrupt".into())
            );
        });
    }

    #[test_case(true, true, true, PROMPT_REASON_FORCED; "force_prompt outranks every other cause")]
    #[test_case(false, true, true, PROMPT_REASON_PROTECTED; "protected outranks an ask rule")]
    #[test_case(false, false, true, PROMPT_REASON_ASK_RULE; "ask rule outranks bare uncovered")]
    #[test_case(false, false, false, PROMPT_REASON_UNCOVERED; "uncovered is the fallback")]
    fn prompt_forcing_reason_reports_the_highest_precedence_cause(
        forced: bool,
        protected: bool,
        ask_rule: bool,
        expected: &str,
    ) {
        let request = log_request(vec![log_resource("cargo test", protected, false)]);

        let reason = prompt_forcing_reason(&request, &[None], forced, ask_rule);

        assert_eq!(reason, expected);
    }

    #[test]
    fn prompt_forcing_reason_ignores_covered_resources() {
        let request = log_request(vec![
            log_resource("git status", true, true),
            log_resource("cargo test", false, false),
        ]);

        let reason = prompt_forcing_reason(&request, &[log_coverage(), None], false, false);

        assert_eq!(reason, PROMPT_REASON_UNCOVERED);
    }

    #[test]
    fn prompt_forcing_reason_prefers_protected_over_requires_prompt() {
        let request = log_request(vec![
            log_resource("cargo test", false, true),
            log_resource("git push", true, false),
        ]);

        let reason = prompt_forcing_reason(&request, &[None, None], false, false);

        assert_eq!(reason, PROMPT_REASON_PROTECTED);
    }

    /// An ask is named by the rule that decided it, never by text a client
    /// shows, so a covering allow cannot stand in for the ask that withheld it.
    #[test_case(&[], AUTO_BUILTIN_ASK, PromptReason::AskRule { origin: RuleOrigin::Builtin, pattern: BUILTIN_PUSH_ASK.into() }; "a_builtin_ask_names_its_family")]
    #[test_case(&[(BROAD_GIT_PATTERN, Effect::Allow), (COVERAGE_PATTERN, Effect::Ask)], COVERAGE_COMMAND, PromptReason::AskRule { origin: RuleOrigin::Config, pattern: COVERAGE_PATTERN.into() }; "a_configured_ask_names_its_rule_not_the_covering_allow")]
    #[test_case(&[], FIRST_COMMAND, PromptReason::Uncovered; "an_uncovered_command")]
    fn prompt_reason_is_typed(rules: &[(&str, Effect)], command: &str, expected: PromptReason) {
        smol::block_on(async {
            let rules = rules
                .iter()
                .map(|&(scope, effect)| shell_policy_rule(scope, effect))
                .collect();
            let manager = mgr_with(make_config(rules), PathBuf::from(SHELL_WORKDIR));
            let prompt = shell_prompt(&manager, &shell_intent(&[command]), command)
                .await
                .expect(SHELL_PROMPT_MISSING);
            assert_eq!(prompt.presentation.reason, expected);
            assert_eq!(prompt.presentation.auto, None);
            let wire = serde_json::to_value(&prompt.presentation).unwrap();
            let restored: PermissionPresentation = serde_json::from_value(wire).unwrap();
            assert_eq!(restored.reason, expected);
        });
    }

    fn matrix_manager(rule: Option<Effect>) -> PermissionManager {
        let rules = rule
            .map(|effect| shell_policy_rule(FIRST_COMMAND, effect))
            .into_iter()
            .collect();
        mgr_with(make_config(rules), PathBuf::from(SHELL_WORKDIR))
    }

    fn matrix_intent(forced: bool, plan_scoped: bool, protected: bool) -> PermissionIntent {
        let mut intent = shell_intent(&[FIRST_COMMAND]);
        intent.scopes.force_prompt = forced;
        intent.scopes.plan_scoped = plan_scoped;
        intent.resources[0].protected = protected;
        intent
    }

    #[test_case(Some(Effect::Ask), false, false, false, AutoNote::RuleAsks; "an_ask_rule")]
    #[test_case(None, true, false, false, AutoNote::Forced; "a_forced_prompt")]
    #[test_case(None, false, true, false, AutoNote::Planning; "plan_mode")]
    #[test_case(None, false, false, true, AutoNote::Protected; "a_protected_command")]
    fn auto_names_why_it_did_not_decide(
        rule: Option<Effect>,
        forced: bool,
        plan_scoped: bool,
        protected: bool,
        note: AutoNote,
    ) {
        smol::block_on(async {
            let manager = matrix_manager(rule);
            manager.set_session_mode(Some(PermissionMode::Auto));
            let intent = matrix_intent(forced, plan_scoped, protected);
            let prompt = shell_prompt(&manager, &intent, FIRST_COMMAND)
                .await
                .expect(SHELL_PROMPT_MISSING);
            assert_eq!(prompt.presentation.auto, Some(note));
        });
    }

    /// Autopilot reads the reserved classes outside Auto too, and they are
    /// exactly the notes Auto gives for them.
    #[test_case(Some(Effect::Ask), matrix_intent(false, false, false), AutoNote::RuleAsks, false; "an_ask_rule")]
    #[test_case(None, matrix_intent(true, false, false), AutoNote::Forced, true; "a_forced_prompt")]
    #[test_case(None, matrix_intent(false, true, false), AutoNote::Planning, true; "plan_mode")]
    #[test_case(None, matrix_intent(false, false, true), AutoNote::Protected, true; "a_protected_command")]
    #[test_case(None, opaque_line_intent(PRIVILEGED_LINE, ShellOpacity::Privilege), AutoNote::AlwaysAsks(ShellOpacity::Privilege), true; "a_line_that_always_asks")]
    fn reserved_note_agrees_with_auto(
        rule: Option<Effect>,
        intent: PermissionIntent,
        note: AutoNote,
        reserved: bool,
    ) {
        smol::block_on(async {
            let manager = matrix_manager(rule);
            let command = intent.scopes.scopes.last().unwrap().clone();
            let asked = shell_prompt(&manager, &intent, &command)
                .await
                .expect(SHELL_PROMPT_MISSING);
            manager.set_session_mode(Some(PermissionMode::Auto));
            let auto = shell_prompt(&manager, &intent, &command)
                .await
                .expect(SHELL_PROMPT_MISSING);
            let context = EvaluationContext {
                revision: *manager.context_revision.read().unwrap(),
                plan_scoped: intent.scopes.plan_scoped,
                builtin_allows: true,
                force_prompt: intent.scopes.force_prompt || intent.scopes.plan_scoped,
                forced: intent.scopes.force_prompt,
                exact_plan: None,
            };

            assert_eq!(asked.presentation.auto, None);
            assert_eq!(auto.presentation.auto, Some(note));
            assert_eq!(reserved_note(&asked, &context), reserved.then_some(note));
        });
    }

    #[test_case(Answerer::User, USER_GUIDANCE_LABEL; "the_user")]
    #[test_case(Answerer::Autopilot, AUTOPILOT_GUIDANCE_LABEL; "autopilot")]
    fn deny_guidance_tells_the_model_who_gave_it(by: Answerer, label: &str) {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let (call, events) = pending_tool_enforcement(
                Arc::clone(&manager),
                CONTROLLED_REQUEST,
                "bash",
                FIRST_COMMAND.into(),
                json!({"command": FIRST_COMMAND}),
            );
            events.recv_async().await.unwrap();

            assert!(manager.answer_as(
                CONTROLLED_REQUEST,
                PermissionAnswer::DenyWithGuidance(DENY_GUIDANCE.into()),
                by,
            ));

            let refusal = call.await.unwrap_err().to_string();
            assert!(
                refusal.ends_with(&format!(" {label} {DENY_GUIDANCE}")),
                "{refusal}"
            );
        });
    }
    #[test_case(false; "stale_partial_refresh_preserves_replacement")]
    #[test_case(true; "stale_settlement_preserves_replacement")]
    fn stale_waiter_cannot_mutate_a_reused_request_id(automatic: bool) {
        let manager = default_mgr();
        let make_request = |commands: Vec<String>| {
            PermissionRequest::from_legacy(
                CONTROLLED_REQUEST.into(),
                ToolKey::native("bash"),
                commands.clone(),
                serde_json::json!({"command": commands.join(" && ")}),
                &manager.project_cwd(),
                false,
            )
        };
        let mut request = make_request(vec![FIRST_COMMAND.into(), SECOND_COMMAND.into()]);
        let (sender, receiver) = flume::bounded(1);
        let (changed, _changes) = flume::bounded(1);
        let (events, received) = flume::unbounded();
        let events = EventSender::new(events, 0);
        let context_revision = *manager.context_revision.read().unwrap();
        manager.pending().entry(manager.id).or_default().insert(
            CONTROLLED_REQUEST.into(),
            PendingPermission {
                request: request.clone(),
                evaluation: None,
                project: None,
                context_revision,
                answering: false,
                abandoned: false,
                claimed_by_human: false,
                cancel: CancelToken::none(),
                delegate_stop: None,
                changed,
                sender: sender.clone(),
            },
        );
        let registration = PendingRegistration {
            manager: &manager,
            request_id: CONTROLLED_REQUEST,
            sender,
            event_tx: &events,
        };
        remember_command(&manager, FIRST_COMMAND);
        if automatic {
            remember_command(&manager, SECOND_COMMAND);
        }
        let current = manager
            .current_policy(
                &request,
                &EvaluationContext {
                    revision: context_revision,
                    plan_scoped: false,
                    builtin_allows: true,
                    force_prompt: false,
                    forced: false,
                    exact_plan: None,
                },
            )
            .unwrap();
        assert_eq!(current.automatic, automatic);
        let revision = manager.broker.revision.load(Ordering::Acquire);
        let answered =
            remove_pending(&mut manager.pending(), manager.id, CONTROLLED_REQUEST).unwrap();
        assert!(
            answered
                .sender
                .try_send(PendingDecision::Explicit(
                    PermissionAnswer::AllowOnce,
                    Answerer::User
                ))
                .is_ok()
        );
        let replacement = make_request(vec![REPLACEMENT_COMMAND.into()]);
        let (new_sender, new_receiver) = flume::bounded(1);
        let (changed, _new_changes) = flume::bounded(1);
        manager.pending().entry(manager.id).or_default().insert(
            CONTROLLED_REQUEST.into(),
            PendingPermission {
                request: replacement.clone(),
                evaluation: None,
                project: None,
                context_revision,
                answering: false,
                abandoned: false,
                claimed_by_human: false,
                cancel: CancelToken::none(),
                delegate_stop: None,
                changed,
                sender: new_sender.clone(),
            },
        );
        assert_eq!(manager.broker.revision.load(Ordering::Acquire), revision);
        let presentation = request.presentation.clone();
        assert_eq!(
            registration.refresh(&mut request, &current, revision),
            Some((false, false))
        );
        assert_eq!(request.presentation, presentation);
        assert_eq!(
            manager.pending_request(CONTROLLED_REQUEST),
            Some(replacement)
        );
        assert!(
            manager.pending()[&manager.id][CONTROLLED_REQUEST]
                .sender
                .same_channel(&new_sender)
        );
        drop(registration);
        assert!(received.is_empty());
        assert!(manager.answer(CONTROLLED_REQUEST, PermissionAnswer::Deny));
        assert!(matches!(
            new_receiver.try_recv(),
            Ok(PendingDecision::Explicit(
                PermissionAnswer::Deny,
                Answerer::User
            ))
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(PendingDecision::Explicit(
                PermissionAnswer::AllowOnce,
                Answerer::User
            ))
        ));
    }

    /// The `source` of every tool decision reported on this thread while it
    /// records.
    #[derive(Clone, Default)]
    struct DecisionSources(Arc<Mutex<Vec<String>>>);

    impl DecisionSources {
        fn during<T>(&self, run: impl FnOnce() -> T) -> T {
            tracing::subscriber::with_default(self.clone(), run)
        }

        fn recorded(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Subscriber for DecisionSources {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.target() == EVENT_TOOL_DECISION
        }

        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(UNUSED_SPAN)
        }

        fn record(&self, _: &Id, _: &Record<'_>) {}

        fn record_follows_from(&self, _: &Id, _: &Id) {}

        fn event(&self, event: &Event<'_>) {
            event.record(&mut self.clone());
        }

        fn enter(&self, _: &Id) {}

        fn exit(&self, _: &Id) {}
    }

    impl Visit for DecisionSources {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == SOURCE_FIELD {
                self.0.lock().unwrap().push(value.to_owned());
            }
        }

        fn record_debug(&mut self, _: &Field, _: &dyn Debug) {}
    }

    /// A manager whose prompts autopilot decides with `provider`, behind an
    /// ask rule, so the call always prompts and is never reserved.
    fn autopilot_manager(provider: Arc<dyn Provider>) -> PermissionManager {
        let manager = matrix_manager(Some(Effect::Ask));
        manager.set_autopilot(Some(engaged_handle(provider)));
        manager
    }

    /// A provider that holds every call until it is dropped, the end of each
    /// call it starts, and the release, kept so a held call never fails.
    fn held_provider() -> (
        Arc<GatedProvider>,
        flume::Receiver<flume::Receiver<()>>,
        flume::Sender<&'static str>,
    ) {
        let (started_tx, started) = flume::unbounded();
        let (release, release_rx) = flume::unbounded();
        let provider = GatedProvider {
            started: started_tx,
            release: release_rx,
        };
        (Arc::new(provider), started, release)
    }

    /// Raises `intent` as a first-party shell call, with a human to answer it
    /// unless `unattended`.
    async fn delegated_call(
        manager: &PermissionManager,
        intent: &PermissionIntent,
        event_tx: &EventSender,
        cancel: &CancelToken,
        unattended: bool,
    ) -> Result<(), PermissionError> {
        let (_legacy_tx, legacy_rx) = flume::unbounded();
        let legacy_rx = async_lock::Mutex::new(legacy_rx);
        manager
            .enforce_with_intent(
                &ToolKey::native(SHELL_TOOL_NAME),
                intent,
                &json!({"command": intent.scopes.scopes.last(), "workdir": SHELL_WORKDIR}),
                event_tx,
                (!unattended).then_some(&legacy_rx),
                CONTROLLED_REQUEST,
                cancel,
                None,
                Some((workcell_shell_subject(), PermissionExecutorKind::Native)),
                true,
            )
            .await
    }

    /// Waits for an event `wanted` accepts, keeping every event it passes.
    async fn wait_for(
        events: &flume::Receiver<Envelope>,
        seen: &mut Vec<AgentEvent>,
        wanted: impl Fn(&AgentEvent) -> bool,
    ) {
        loop {
            let event = events.recv_async().await.unwrap().event;
            let found = wanted(&event);
            seen.push(event);
            if found {
                return;
            }
        }
    }

    fn events_of(events: &flume::Receiver<Envelope>) -> Vec<AgentEvent> {
        events.drain().map(|envelope| envelope.event).collect()
    }

    fn decided(events: &[AgentEvent]) -> Vec<&AutopilotDecision> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Autopilot(AutopilotEvent::Decided(decision)) => Some(&**decision),
                _ => None,
            })
            .collect()
    }

    fn escalated(events: &[AgentEvent]) -> bool {
        events.iter().any(|event| {
            matches!(event, AgentEvent::PermissionRequestUpdated(request)
                if matches!(request.presentation.autopilot, Some(AutopilotNote::Escalated { .. })))
        })
    }

    fn resolved_by(events: &[AgentEvent]) -> Vec<Option<Answerer>> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::PermissionRequestResolved { by, .. } => Some(*by),
                _ => None,
            })
            .collect()
    }

    /// One row the audit log keeps of a decision.
    #[derive(Debug, PartialEq)]
    enum Row {
        Decided(Option<DecisionOutcome>),
        StoodDown(DecisionOutcome),
        Settled(DecisionOutcome),
    }

    fn rows(events: &[AgentEvent]) -> Vec<Row> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Autopilot(AutopilotEvent::Decided(decision)) => {
                    Some(Row::Decided(decision.outcome.clone()))
                }
                AgentEvent::Autopilot(AutopilotEvent::StoodDown(stood_down)) => {
                    Some(Row::StoodDown(stood_down.outcome.clone()))
                }
                AgentEvent::Autopilot(AutopilotEvent::Settled { outcome, .. }) => {
                    Some(Row::Settled(outcome.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn evaluation_context(manager: &PermissionManager) -> EvaluationContext {
        EvaluationContext {
            revision: *manager.context_revision.read().unwrap(),
            plan_scoped: false,
            builtin_allows: true,
            force_prompt: false,
            forced: false,
            exact_plan: None,
        }
    }

    fn delegate_outcome(action: DelegateAction) -> DelegateOutcome {
        DelegateOutcome {
            kind: DecisionKind::Permission,
            answer: Ok(DelegateOutput {
                action,
                reason: DELEGATE_REASON.into(),
            }),
            usage: TokenUsage::default(),
            cost: None,
            billing: Billing::default(),
            model: None,
        }
    }

    /// Autopilot is asked about every prompt, but a reserved one is never its
    /// to allow in any mode: unattended, its allow becomes a denial.
    #[test_case(PermissionMode::Ask, Some(Effect::Ask), matrix_intent(false, false, false), None; "ask_an_ask_rule")]
    #[test_case(PermissionMode::Ask, None, matrix_intent(false, true, false), Some(AutoNote::Planning); "ask_plan_mode")]
    #[test_case(PermissionMode::Ask, None, matrix_intent(true, false, false), Some(AutoNote::Forced); "ask_a_forced_prompt")]
    #[test_case(PermissionMode::Ask, None, matrix_intent(false, false, true), Some(AutoNote::Protected); "ask_a_protected_command")]
    #[test_case(PermissionMode::Ask, None, opaque_line_intent(PRIVILEGED_LINE, ShellOpacity::Privilege), Some(AutoNote::AlwaysAsks(ShellOpacity::Privilege)); "ask_a_line_that_always_asks")]
    #[test_case(PermissionMode::Auto, Some(Effect::Ask), matrix_intent(false, false, false), None; "auto_an_ask_rule")]
    #[test_case(PermissionMode::Auto, None, matrix_intent(false, true, false), Some(AutoNote::Planning); "auto_plan_mode")]
    #[test_case(PermissionMode::Auto, None, matrix_intent(true, false, false), Some(AutoNote::Forced); "auto_a_forced_prompt")]
    #[test_case(PermissionMode::Auto, None, matrix_intent(false, false, true), Some(AutoNote::Protected); "auto_a_protected_command")]
    #[test_case(PermissionMode::Auto, None, opaque_line_intent(PRIVILEGED_LINE, ShellOpacity::Privilege), Some(AutoNote::AlwaysAsks(ShellOpacity::Privilege)); "auto_a_line_that_always_asks")]
    fn autopilot_never_allows_a_reserved_request(
        mode: PermissionMode,
        rule: Option<Effect>,
        intent: PermissionIntent,
        reserved: Option<AutoNote>,
    ) {
        let provider = ScriptedProvider::new(vec![Step::Answer(DELEGATE_ALLOW)]);
        let manager = matrix_manager(rule);
        manager.set_session_mode(Some(mode));
        manager.set_autopilot(Some(engaged_handle(provider.clone())));
        let (event_tx, events) = flume::unbounded();
        let event_tx = EventSender::new(event_tx, 0);

        let result = smol::block_on(delegated_call(
            &manager,
            &intent,
            &event_tx,
            &CancelToken::none(),
            true,
        ));

        let reservation = reserved.map_or(ITEM_NOT_RESERVED, AutoNote::name);
        let [prompt] = provider.prompts().try_into().unwrap();
        assert!(
            prompt.contains(&format!("{ITEM_RESERVED_LABEL} {reservation}")),
            "{prompt}"
        );
        let events = events_of(&events);
        let decisions = decided(&events);
        let [decision] = decisions.as_slice() else {
            panic!("{decisions:?}");
        };
        if reserved.is_none() {
            result.unwrap();
            assert_eq!(decision.action, ActionName::Allow);
            return;
        }
        let refused = WireError::Refused {
            kind: DecisionKind::Permission,
            action: ActionName::Allow.name().into(),
        };
        let guidance = result.unwrap_err().guidance.unwrap();
        assert_eq!(
            (guidance.text.as_str(), guidance.by),
            (UNATTENDED_ESCALATION, Answerer::Autopilot)
        );
        assert_eq!(
            (decision.action, decision.reason.clone()),
            (ActionName::Escalate, refused.to_string())
        );
    }

    /// Taint reserves a request no class reserves, yet autopilot still reads it.
    #[test]
    fn a_tainted_session_keeps_autopilot_from_allowing() {
        let state = tempdir().unwrap();
        let provider = ScriptedProvider::new(vec![Step::Answer(DELEGATE_ALLOW)]);
        let manager = autopilot_manager(provider.clone());
        let decisions = Decisions::new(
            DecisionsConfig::default(),
            &StateDir::from_path(state.path().into()),
        )
        .unwrap();
        manager.set_decisions(Some(decisions));
        manager.decisions().unwrap().mark_tainted();
        let (event_tx, _events) = flume::unbounded();

        let refusal = smol::block_on(delegated_call(
            &manager,
            &matrix_intent(false, false, false),
            &EventSender::new(event_tx, 0),
            &CancelToken::none(),
            true,
        ))
        .unwrap_err();

        let [prompt] = provider.prompts().try_into().unwrap();
        assert!(
            prompt.contains(&format!("{ITEM_RESERVED_LABEL} {TAINTED_RESERVATION}")),
            "{prompt}"
        );
        assert_eq!(refusal.guidance.unwrap().text, UNATTENDED_ESCALATION);
    }

    /// With no human to ask, autopilot's answer stands, and an escalation is a
    /// denial that says why.
    #[test_case(DELEGATE_ALLOW, None, DECISION_SOURCE_AUTOPILOT_ONCE, DecisionOutcome::Applied; "an_allow_runs_the_call")]
    #[test_case(DELEGATE_DENY, Some(DENY_GUIDANCE), DECISION_SOURCE_AUTOPILOT_DENY, DecisionOutcome::Applied; "a_denial_carries_its_guidance")]
    #[test_case(DELEGATE_ESCALATE, Some(UNATTENDED_ESCALATION), DECISION_SOURCE_AUTOPILOT_DENY, DecisionOutcome::DeniedUnattended; "an_escalation_finds_no_human")]
    fn an_unattended_prompt_takes_the_autopilot_answer(
        answer: &'static str,
        guidance: Option<&str>,
        source: &str,
        outcome: DecisionOutcome,
    ) {
        let manager = autopilot_manager(ScriptedProvider::new(vec![Step::Answer(answer)]));
        let (event_tx, events) = flume::unbounded();
        let event_tx = EventSender::new(event_tx, 0);
        let sources = DecisionSources::default();

        let result = sources.during(|| {
            smol::block_on(delegated_call(
                &manager,
                &matrix_intent(false, false, false),
                &event_tx,
                &CancelToken::none(),
                true,
            ))
        });

        match guidance {
            None => result.unwrap(),
            Some(text) => {
                let guidance = result.unwrap_err().guidance.unwrap();
                assert_eq!(
                    (guidance.text.as_str(), guidance.by),
                    (text, Answerer::Autopilot)
                );
            }
        }
        assert_eq!(sources.recorded(), [source]);
        let events = events_of(&events);
        assert!(matches!(
            events.first(),
            Some(AgentEvent::Autopilot(AutopilotEvent::Deciding {
                kind: DecisionKind::Permission,
                ..
            }))
        ));
        assert_eq!(rows(&events), [Row::Decided(Some(outcome))]);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::PermissionRequest(_)))
        );
    }

    /// Autopilot answers a waiting prompt for this one call: the prompt
    /// resolves as autopilot's, and no rule is kept.
    #[test_case(DELEGATE_ALLOW, ActionName::Allow, None, DECISION_SOURCE_AUTOPILOT_ONCE; "an_allow")]
    #[test_case(DELEGATE_DENY, ActionName::Deny, Some(DENY_GUIDANCE), DECISION_SOURCE_AUTOPILOT_DENY; "a_denial")]
    fn autopilot_answers_a_waiting_prompt(
        answer: &'static str,
        action: ActionName,
        guidance: Option<&str>,
        source: &str,
    ) {
        let manager = autopilot_manager(ScriptedProvider::new(vec![Step::Answer(answer)]));
        let (event_tx, events) = flume::unbounded();
        let event_tx = EventSender::new(event_tx, 0);
        let sources = DecisionSources::default();

        let result = sources.during(|| {
            smol::block_on(delegated_call(
                &manager,
                &matrix_intent(false, false, false),
                &event_tx,
                &CancelToken::none(),
                false,
            ))
        });

        match guidance {
            None => result.unwrap(),
            Some(text) => {
                let refusal = result.unwrap_err().to_string();
                assert!(
                    refusal.ends_with(&format!(" {AUTOPILOT_GUIDANCE_LABEL} {text}")),
                    "{refusal}"
                );
            }
        }
        assert_eq!(sources.recorded(), [source]);
        assert!(manager.structured_conversation_rules_snapshot().is_empty());
        assert_eq!(manager.pending_count(), 0);
        let events = events_of(&events);
        let raised = events
            .iter()
            .find_map(|event| match event {
                AgentEvent::PermissionRequest(request) => Some(request),
                _ => None,
            })
            .unwrap();
        assert_eq!(raised.presentation.autopilot, Some(AutopilotNote::Deciding));
        let decisions = decided(&events);
        let [decision] = decisions.as_slice() else {
            panic!("{decisions:?}");
        };
        assert_eq!(
            (decision.action, decision.tool_use_id.as_deref()),
            (action, Some(CONTROLLED_REQUEST))
        );
        assert_eq!(
            rows(&events),
            [Row::Decided(Some(DecisionOutcome::Applied))]
        );
        assert_eq!(resolved_by(&events), [Some(Answerer::Autopilot)]);
    }

    /// An escalation leaves the prompt pending with autopilot's reason, and
    /// the human's answer is the user's.
    #[test]
    fn an_escalation_leaves_the_prompt_to_the_human() {
        let manager =
            autopilot_manager(ScriptedProvider::new(vec![Step::Answer(DELEGATE_ESCALATE)]));
        let (event_tx, events) = flume::unbounded();
        let event_tx = EventSender::new(event_tx, 0);
        let sources = DecisionSources::default();
        let escalation = Some(AutopilotNote::Escalated {
            reason: ESCALATION_REASON.into(),
        });
        let mut seen = Vec::new();

        let (result, ()) = sources.during(|| {
            smol::block_on(future::zip(
                delegated_call(
                    &manager,
                    &matrix_intent(false, false, false),
                    &event_tx,
                    &CancelToken::none(),
                    false,
                ),
                async {
                    wait_for(&events, &mut seen, |event| {
                        matches!(event, AgentEvent::PermissionRequestUpdated(request)
                            if request.presentation.autopilot == escalation)
                    })
                    .await;
                    let pending = manager.pending_request(CONTROLLED_REQUEST).unwrap();
                    assert_eq!(pending.presentation.autopilot, escalation);
                    assert!(manager.answer(CONTROLLED_REQUEST, PermissionAnswer::AllowOnce));
                },
            ))
        });

        result.unwrap();
        assert_eq!(sources.recorded(), [DECISION_SOURCE_USER_ONCE]);
        seen.extend(events_of(&events));
        let decisions: Vec<_> = decided(&seen)
            .iter()
            .map(|decision| (decision.action, decision.reason.as_str()))
            .collect();
        assert_eq!(decisions, [(ActionName::Escalate, ESCALATION_REASON)]);
        assert_eq!(
            rows(&seen),
            [
                Row::Decided(None),
                Row::Settled(DecisionOutcome::Answered {
                    answer: HUMAN_ALLOW.into()
                })
            ]
        );
        assert!(resolved_by(&seen).is_empty());
    }

    /// A human taking the prompt stops autopilot's call and clears its note,
    /// and autopilot may no longer answer.
    #[test]
    fn a_claim_stands_the_delegate_down() {
        let (provider, started, _release) = held_provider();
        let manager = autopilot_manager(provider);
        let (event_tx, events) = flume::unbounded();
        let event_tx = EventSender::new(event_tx, 0);
        let mut seen = Vec::new();

        let (result, ()) = smol::block_on(future::zip(
            delegated_call(
                &manager,
                &matrix_intent(false, false, false),
                &event_tx,
                &CancelToken::none(),
                false,
            ),
            async {
                let ended = started.recv_async().await.unwrap();
                assert!(manager.claim(CONTROLLED_REQUEST));
                assert!(ended.recv_async().await.is_err());
                wait_for(&events, &mut seen, |event| {
                    matches!(event, AgentEvent::PermissionRequestUpdated(request)
                        if request.presentation.autopilot.is_none())
                })
                .await;
                assert!(!manager.answer_as(
                    CONTROLLED_REQUEST,
                    PermissionAnswer::AllowOnce,
                    Answerer::Autopilot
                ));
                assert!(manager.answer(CONTROLLED_REQUEST, PermissionAnswer::Deny));
            },
        ));

        assert!(result.unwrap_err().guidance.is_none());
        seen.extend(events_of(&events));
        assert_eq!(rows(&seen), [Row::StoodDown(DecisionOutcome::TakenOver)]);
    }

    enum PromptEnd {
        Settled,
        Cancelled,
        Reloaded,
    }

    /// Whatever ends a waiting prompt ends autopilot's call for it, which
    /// stands down with no escalation, and the prompt never resolves as
    /// autopilot's.
    #[test_case(PromptEnd::Settled, true, DECISION_SOURCE_YOLO, DecisionOutcome::Superseded; "a_matching_rule")]
    #[test_case(PromptEnd::Cancelled, false, DECISION_SOURCE_USER_ABORT, DecisionOutcome::Cancelled; "a_cancelled_run")]
    #[test_case(PromptEnd::Reloaded, false, DECISION_SOURCE_RULE, DecisionOutcome::Superseded; "a_reload")]
    fn the_delegate_stops_when_the_prompt_ends(
        end: PromptEnd,
        allowed: bool,
        source: &str,
        outcome: DecisionOutcome,
    ) {
        let (provider, started, _release) = held_provider();
        let manager = autopilot_manager(provider);
        let (event_tx, events) = flume::unbounded();
        let event_tx = EventSender::new(event_tx, 0);
        let (trigger, cancel) = CancelToken::new();
        let mut trigger = Some(trigger);
        let sources = DecisionSources::default();

        let (result, ended) = sources.during(|| {
            smol::block_on(future::zip(
                delegated_call(
                    &manager,
                    &matrix_intent(false, false, false),
                    &event_tx,
                    &cancel,
                    false,
                ),
                async {
                    let ended = started.recv_async().await.unwrap();
                    match end {
                        PromptEnd::Settled => manager.set_session_mode(Some(PermissionMode::Yolo)),
                        PromptEnd::Cancelled => trigger.take().unwrap().cancel(),
                        PromptEnd::Reloaded => manager.set_project(Path::new(SHELL_WORKDIR)),
                    }
                    ended
                },
            ))
        });

        assert_eq!(result.is_ok(), allowed);
        assert_eq!(sources.recorded(), [source]);
        assert!(matches!(
            ended.try_recv(),
            Err(flume::TryRecvError::Disconnected)
        ));
        let events = events_of(&events);
        assert_eq!(rows(&events), [Row::StoodDown(outcome)]);
        assert!(!escalated(&events));
        assert_eq!(resolved_by(&events), [None]);
    }

    enum Requester {
        Session,
        Task,
        Workflow,
    }

    impl Requester {
        fn sender(&self, tx: flume::Sender<Envelope>) -> EventSender {
            let sender = EventSender::new(tx, 0);
            match self {
                Self::Session => sender,
                Self::Task => sender.with_task(TaskProvenance {
                    session_id: CaudraId::generate(),
                    task_id: TASK_ID.into(),
                    invocation_id: INVOCATION_ID.into(),
                }),
                Self::Workflow => sender.with_workflow(WorkflowProvenance {
                    run_id: WORKFLOW_RUN_ID.into(),
                    epoch: WORKFLOW_EPOCH,
                    call_key: WORKFLOW_CALL_KEY,
                    phase: Some(WORKFLOW_PHASE_NAME.into()),
                }),
            }
        }

        fn agent(&self) -> Option<String> {
            match self {
                Self::Session => None,
                Self::Task => Some(format!("{BACKGROUND_TASK_AGENT} {TASK_ID}")),
                Self::Workflow => Some(format!(
                    "{WORKFLOW_AGENT} {WORKFLOW_RUN_ID}{ITEM_SEPARATOR}{WORKFLOW_CALL} \
                     {WORKFLOW_CALL_KEY}{ITEM_SEPARATOR}{WORKFLOW_PHASE} {WORKFLOW_PHASE_NAME}"
                )),
            }
        }
    }

    /// Every agent sharing the session's manager reaches its autopilot, and
    /// the item and the row name who asked.
    #[test_case(Requester::Session; "the_session")]
    #[test_case(Requester::Task; "a_background_task")]
    #[test_case(Requester::Workflow; "a_workflow_agent")]
    fn every_agent_sharing_the_manager_reaches_autopilot(requester: Requester) {
        let provider = ScriptedProvider::new(vec![Step::Answer(DELEGATE_ALLOW)]);
        let manager = autopilot_manager(provider.clone());
        let (event_tx, events) = flume::unbounded();

        smol::block_on(delegated_call(
            &manager,
            &matrix_intent(false, false, false),
            &requester.sender(event_tx),
            &CancelToken::none(),
            true,
        ))
        .unwrap();

        let agent = requester.agent();
        let [prompt] = provider.prompts().try_into().unwrap();
        let named = agent.as_deref().unwrap_or(MAIN_AGENT);
        assert!(
            prompt.contains(&format!("{ITEM_AGENT_LABEL} {named}")),
            "{prompt}"
        );
        let events = events_of(&events);
        let agents: Vec<_> = decided(&events)
            .iter()
            .map(|decision| decision.agent.clone())
            .collect();
        assert_eq!(agents, [agent]);
    }

    /// A fork carries no autopilot, so its prompts are never delegated.
    #[test]
    fn a_fork_never_asks_autopilot() {
        let provider = ScriptedProvider::new(Vec::new());
        let fork = autopilot_manager(provider.clone()).fork();
        let (event_tx, events) = flume::unbounded();

        let refusal = smol::block_on(delegated_call(
            &fork,
            &matrix_intent(false, false, false),
            &EventSender::new(event_tx, 0),
            &CancelToken::none(),
            true,
        ))
        .unwrap_err();

        assert!(refusal.guidance.is_none());
        assert!(provider.prompts().is_empty());
        assert!(events.is_empty());
    }

    enum Interference {
        HumanAnswering,
        Reload,
    }

    /// A call that ends after the prompt moved on is still recorded, unless a
    /// reload superseded its escalation, and it never escalates the prompt.
    #[test_case(DelegateAction::Allow, Interference::HumanAnswering, true; "an_allow_a_human_beat")]
    #[test_case(DelegateAction::Escalate, Interference::HumanAnswering, true; "an_escalation_a_human_beat")]
    #[test_case(DelegateAction::Allow, Interference::Reload, true; "an_allow_a_reload_refused")]
    #[test_case(DelegateAction::Escalate, Interference::Reload, false; "an_escalation_a_reload_superseded")]
    fn a_late_call_never_escalates_the_prompt_it_lost(
        action: DelegateAction,
        interference: Interference,
        recorded: bool,
    ) {
        let manager = autopilot_manager(ScriptedProvider::new(Vec::new()));
        let mut request = shell_request(&[FIRST_COMMAND], workcell_shell_subject());
        let request_id = request.id.clone();
        let context = evaluation_context(&manager);
        let (event_tx, events) = flume::unbounded();
        let event_tx = EventSender::new(event_tx, 0);
        let delegate = manager
            .prompt_delegate(&request, &context, &event_tx)
            .unwrap();
        let (sender, _decisions) = flume::bounded(1);
        let (changed, _changes) = flume::bounded(1);
        manager.pending().entry(manager.id).or_default().insert(
            request_id.clone(),
            PendingPermission {
                request: request.clone(),
                evaluation: Some(context.clone()),
                project: None,
                context_revision: context.revision,
                answering: matches!(interference, Interference::HumanAnswering),
                abandoned: false,
                claimed_by_human: false,
                cancel: CancelToken::none(),
                delegate_stop: None,
                changed,
                sender: sender.clone(),
            },
        );
        if matches!(interference, Interference::Reload) {
            manager.set_project(Path::new(SHELL_WORKDIR));
        }
        let registration = PendingRegistration {
            manager: &manager,
            request_id: &request_id,
            sender,
            event_tx: &event_tx,
        };

        manager.conclude_delegate(
            &delegate,
            Some(delegate_outcome(action)),
            &registration,
            &mut request,
            &context,
        );

        let events = events_of(&events);
        let rows = usize::from(recorded);
        assert_eq!(decided(&events).len(), rows);
        assert_eq!(manager.autopilot().unwrap().recent_decisions().len(), rows);
        assert!(!escalated(&events));
        assert_eq!(
            manager
                .pending_request(&request_id)
                .map(|pending| pending.presentation.autopilot),
            Some(None)
        );
    }

    /// What ends a prompt ends autopilot's decision for it: one still deciding
    /// stands down, and one already recorded settles, with the human's answer
    /// as the log words it.
    #[test_case(None, Some(PendingDecision::Explicit(PermissionAnswer::AllowOnce, Answerer::User)), Row::StoodDown(DecisionOutcome::TakenOver); "a_human_while_deciding")]
    #[test_case(Some(DelegateAction::Allow), Some(PendingDecision::Explicit(PermissionAnswer::DenyWithGuidance(DENY_GUIDANCE.into()), Answerer::User)), Row::Settled(DecisionOutcome::HumanFirst { answer: HUMAN_DENIAL.into() }); "a_human_first")]
    #[test_case(Some(DelegateAction::Escalate), Some(PendingDecision::Explicit(PermissionAnswer::AllowOnce, Answerer::User)), Row::Settled(DecisionOutcome::Answered { answer: HUMAN_ALLOW.into() }); "a_human_answering_an_escalation")]
    #[test_case(None, Some(PendingDecision::MatchedRule), Row::StoodDown(DecisionOutcome::Superseded); "a_rule_while_deciding")]
    #[test_case(Some(DelegateAction::Escalate), Some(PendingDecision::PolicyDenied(CURRENT_POLICY_DENIES_REQUEST.into())), Row::Settled(DecisionOutcome::Superseded); "a_policy_after_an_escalation")]
    #[test_case(None, None, Row::StoodDown(DecisionOutcome::Cancelled); "a_stopped_run_while_deciding")]
    #[test_case(Some(DelegateAction::Allow), None, Row::Settled(DecisionOutcome::Cancelled); "a_stopped_run_after_an_answer")]
    fn what_ends_a_prompt_ends_its_decision(
        recorded: Option<DelegateAction>,
        decision: Option<PendingDecision>,
        last: Row,
    ) {
        let manager = autopilot_manager(ScriptedProvider::new(Vec::new()));
        let request = shell_request(&[FIRST_COMMAND], workcell_shell_subject());
        let (event_tx, events) = flume::unbounded();
        let delegate = manager
            .prompt_delegate(
                &request,
                &evaluation_context(&manager),
                &EventSender::new(event_tx, 0),
            )
            .unwrap();
        delegate.slot.announce();
        let expected: Vec<Row> = recorded
            .as_ref()
            .map(|_| Row::Decided(None))
            .into_iter()
            .chain([last])
            .collect();
        if let Some(action) = recorded {
            delegate.slot.record_open(&delegate_outcome(action));
        }

        delegate.settle(decision.as_ref());
        drop(delegate);

        assert_eq!(rows(&events_of(&events)), expected);
    }

    /// The screen refuses an action the request does not accept, whatever let
    /// it through the wire.
    #[test_case(DelegateAction::Allow, false, false; "an_allow_of_an_open_request")]
    #[test_case(DelegateAction::Allow, true, true; "an_allow_of_a_reserved_request")]
    #[test_case(DelegateAction::Deny { guidance: DENY_GUIDANCE.into() }, true, false; "a_denial_of_a_reserved_request")]
    fn the_screen_refuses_what_the_request_does_not_accept(
        action: DelegateAction,
        reserved: bool,
        refused: bool,
    ) {
        let outcome = delegate_outcome(action.clone());
        let expected = if refused {
            Err(WireError::Refused {
                kind: DecisionKind::Permission,
                action: action.name().name().into(),
            }
            .to_string())
        } else {
            outcome.answer.clone()
        };

        assert_eq!(screened(outcome, reserved).answer, expected);
    }

    /// The delegate reads the prompt bounded: the input clipped, the resources
    /// capped, then why it asks, who asked, and whether it may allow.
    #[test]
    fn the_item_tells_the_delegate_what_it_decides() {
        let commands: Vec<String> = (0..PROMPT_LOG_MAX_RESOURCES + HIDDEN_RESOURCES)
            .map(|index| format!("{FIRST_COMMAND} {index}"))
            .collect();
        let commands: Vec<&str> = commands.iter().map(String::as_str).collect();
        let mut request = shell_request(&commands, workcell_shell_subject());
        request.input = json!({"command": FIRST_COMMAND.repeat(MAX_ITEM_INPUT_BYTES)});
        request.presentation.resources[0].protected = true;
        request.presentation.auto = Some(AutoNote::RuleAsks);
        request.presentation.advisories = vec![PermissionAdvisory {
            flag: EngineFlag::Deletes,
            probability: ADVISORY_PROBABILITY,
        }];

        let item = decision_item(
            &request,
            &PermissionMode::Auto,
            Some(TASK_ID),
            Some(TAINTED_RESERVATION),
        );

        let lines: Vec<&str> = item.lines().collect();
        assert_eq!(lines[0], format!("{ITEM_TOOL_LABEL} {}", request.tool));
        assert!(lines[1].starts_with(ITEM_INPUT_LABEL) && lines[1].ends_with(ITEM_TRUNCATED));
        assert!(
            lines[1].len()
                <= ITEM_INPUT_LABEL.len() + 1 + MAX_ITEM_INPUT_BYTES + ITEM_TRUNCATED.len()
        );
        assert_eq!(lines[2], ITEM_RESOURCES_LABEL);
        let bullet = format!("{ITEM_BULLET} ");
        let resources: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|line| line.starts_with(&bullet))
            .collect();
        assert_eq!(resources.len(), PROMPT_LOG_MAX_RESOURCES + 1);
        assert!(resources[0].ends_with(ITEM_PROTECTED));
        assert!(!resources[1].ends_with(ITEM_PROTECTED));
        assert_eq!(
            resources[PROMPT_LOG_MAX_RESOURCES],
            format!("{ITEM_BULLET} +{HIDDEN_RESOURCES} {ITEM_MORE}")
        );
        let tail = [
            format!(
                "{ITEM_REASON_LABEL} {}",
                prompt_reason_message(&request.presentation.reason)
            ),
            format!("{ITEM_AUTO_LABEL} {}", AutoNote::RuleAsks.phrase()),
            format!(
                "{ITEM_ADVISORIES_LABEL} {} {ADVISORY_PROBABILITY}",
                EngineFlag::Deletes.name()
            ),
            format!("{ITEM_AGENT_LABEL} {TASK_ID}"),
            format!("{ITEM_MODE_LABEL} {}", mode_label(&PermissionMode::Auto)),
            format!("{ITEM_RESERVED_LABEL} {TAINTED_RESERVATION}"),
        ];
        assert_eq!(lines[lines.len() - tail.len()..], tail);
        assert_eq!(
            decision_subject(&request),
            format!(
                "{}: {} +{} {ITEM_MORE}",
                request.tool,
                request.presentation.resources[0].summary,
                commands.len() - 1
            )
        );
    }

    const HOME_ASSISTANT_SERVER: &str = "home-assistant";
    const EXECUTOR_SERVER: &str = "executor";
    const TURN_ON_TOOL: &str = "HassTurnOn";
    const LOOKUP_TOOL: &str = "GetLiveContext";
    const RESUME_TOOL: &str = "resume";
    const MCP_TEST_CONTRACT: &str = "mcp-test-contract";
    const MCP_REQUEST: &str = "mcp-call";
    const MCP_FACTS_FIELD: &str = "mcp";
    const PAUSED_RUN: &str = "exec_1";
    const UNSEEN_RUN: &str = "exec_2";
    const WAITING_STATUS: &str = "waiting_for_interaction";
    const PAUSED_MESSAGE: &str = "Deploy to prod?";
    const PAUSED_ADDRESS: &str = "deploy.run";
    const LOCK_NOTE: &str = "Home Assistant lock devices always ask";
    const WAITING_NOTE: &str =
        "Executor run waiting for: Deploy to prod? · deploy.run({\"env\":\"prod\"})";
    const UNSEEN_RUN_NOTE: &str = "Executor run exec_2: not a run Caudra saw pause";
    const MCP_PROMPT_MISSING: &str = "the MCP call ran without a prompt";
    const READ_ONLY_OPTION_MISSING: &str = "a read-only tool offers any arguments";

    fn mcp_key(server: &str, tool: &str) -> ToolKey {
        ToolKey::parse(&format!("{server}.{tool}")).unwrap()
    }

    /// The intent dispatch raises for `server`'s `tool`, a recognised server
    /// having seen [`PAUSED_RUN`] pause.
    fn mcp_intent(server: &str, tool: &str, input: &Value, read_only: bool) -> PermissionIntent {
        let paused = PausedRuns::default();
        paused.record(&json!({
            "status": WAITING_STATUS,
            "executionId": PAUSED_RUN,
            "interaction": {"message": PAUSED_MESSAGE, "address": PAUSED_ADDRESS, "args": {"env": "prod"}},
        }));
        let facts = McpServerKind::from_server_name(server).map(|kind| McpCallFacts {
            kind,
            read_only,
            note: kind.always_asks(tool, input, &paused),
        });
        mcp_permission_intent(&mcp_key(server, tool), input, facts)
    }

    async fn enforce_mcp(
        manager: &PermissionManager,
        (server, tool): (&str, &str),
        input: &Value,
        read_only: bool,
        event_tx: &EventSender,
        answers: Option<&async_lock::Mutex<flume::Receiver<String>>>,
    ) -> Result<(), PermissionError> {
        let subject = PermissionSubject::Mcp {
            server: server.into(),
            authority: server.into(),
            tool: tool.into(),
            contract: MCP_TEST_CONTRACT.into(),
        };
        manager
            .enforce_with_intent(
                &mcp_key(server, tool),
                &mcp_intent(server, tool, input, read_only),
                input,
                event_tx,
                answers,
                MCP_REQUEST,
                &CancelToken::none(),
                None,
                Some((subject, PermissionExecutorKind::Mcp)),
                false,
            )
            .await
    }

    /// The prompt a call raises, refused once seen, or `None` when it ran.
    async fn mcp_prompt(
        manager: &PermissionManager,
        call: (&str, &str),
        input: &Value,
        read_only: bool,
    ) -> Option<Box<PermissionRequest>> {
        let (event_tx, events) = flume::unbounded();
        let event_tx = EventSender::new(event_tx, 0);
        let (_answer_tx, answers) = flume::unbounded();
        let answers = async_lock::Mutex::new(answers);
        let mut enforcement = Box::pin(enforce_mcp(
            manager,
            call,
            input,
            read_only,
            &event_tx,
            Some(&answers),
        ));
        let prompt = future::race(
            async {
                enforcement.as_mut().await.unwrap();
                None
            },
            async {
                loop {
                    if let AgentEvent::PermissionRequest(request) =
                        events.recv_async().await.unwrap().event
                    {
                        return Some(request);
                    }
                }
            },
        )
        .await;
        if prompt.is_some() {
            assert!(manager.answer(MCP_REQUEST, PermissionAnswer::Deny));
            assert!(enforcement.await.is_err());
        }
        prompt
    }

    fn mcp_manager((server, tool): (&str, &str), rule: Option<Effect>) -> PermissionManager {
        let rules = rule
            .map(|effect| PermissionRule {
                tool: mcp_key(server, tool),
                scope: None,
                effect,
            })
            .into_iter()
            .collect();
        mgr_with(make_config(rules), PathBuf::from(SHELL_WORKDIR))
    }

    /// A call its recognised server guards asks whatever the rules or the
    /// mode say, and says why.
    #[test_case((HOME_ASSISTANT_SERVER, TURN_ON_TOOL), json!({"domain": "lock"}), None, PermissionMode::Ask, LOCK_NOTE, None; "a_string_domain")]
    #[test_case((HOME_ASSISTANT_SERVER, TURN_ON_TOOL), json!({"domain": ["light", "lock"]}), None, PermissionMode::Ask, LOCK_NOTE, None; "an_array_domain")]
    #[test_case((HOME_ASSISTANT_SERVER, TURN_ON_TOOL), json!({"domain": ["LoCk"]}), None, PermissionMode::Ask, LOCK_NOTE, None; "a_mixed_case_domain")]
    #[test_case((HOME_ASSISTANT_SERVER, TURN_ON_TOOL), json!({"domain": "lock"}), Some(Effect::Allow), PermissionMode::Ask, LOCK_NOTE, None; "despite_an_allow_rule")]
    #[test_case((HOME_ASSISTANT_SERVER, TURN_ON_TOOL), json!({"domain": "lock"}), None, PermissionMode::Auto, LOCK_NOTE, Some(AutoNote::Forced); "in_auto")]
    #[test_case((EXECUTOR_SERVER, RESUME_TOOL), json!({"executionId": PAUSED_RUN, "action": "Accept"}), Some(Effect::Allow), PermissionMode::Ask, WAITING_NOTE, None; "an_executor_accept")]
    #[test_case((EXECUTOR_SERVER, RESUME_TOOL), json!({"executionId": UNSEEN_RUN, "action": "accept"}), Some(Effect::Allow), PermissionMode::Ask, UNSEEN_RUN_NOTE, None; "an_accept_of_an_unseen_run")]
    fn a_guarded_mcp_call_always_asks(
        call: (&str, &str),
        input: Value,
        rule: Option<Effect>,
        mode: PermissionMode,
        note: &str,
        auto: Option<AutoNote>,
    ) {
        smol::block_on(async {
            let manager = mcp_manager(call, rule);
            manager.set_session_mode(Some(mode));
            let prompt = mcp_prompt(&manager, call, &input, false)
                .await
                .expect(MCP_PROMPT_MISSING);
            assert_eq!(
                prompt.mcp.as_ref().and_then(|facts| facts.note.as_deref()),
                Some(note)
            );
            assert_eq!(
                (
                    &prompt.risk,
                    &prompt.presentation.reason,
                    prompt.presentation.auto
                ),
                (&PermissionRisk::Critical, &PromptReason::Forced, auto)
            );
        });
    }

    /// Without a guarded argument the same tools follow the rules and the
    /// mode, and Yolo runs even a guarded one.
    #[test_case((HOME_ASSISTANT_SERVER, TURN_ON_TOOL), json!({"name": "Front door"}), Some(Effect::Allow), PermissionMode::Ask; "no_domain_with_an_allow_rule")]
    #[test_case((HOME_ASSISTANT_SERVER, TURN_ON_TOOL), json!({"domain": "light"}), None, PermissionMode::Auto; "a_harmless_domain_in_auto")]
    #[test_case((HOME_ASSISTANT_SERVER, TURN_ON_TOOL), json!({"domain": "lock"}), None, PermissionMode::Yolo; "yolo_runs_a_sensitive_domain")]
    #[test_case((EXECUTOR_SERVER, RESUME_TOOL), json!({"executionId": PAUSED_RUN, "action": "decline"}), Some(Effect::Allow), PermissionMode::Ask; "an_executor_decline")]
    #[test_case((EXECUTOR_SERVER, RESUME_TOOL), json!({"executionId": PAUSED_RUN, "action": "cancel"}), Some(Effect::Allow), PermissionMode::Ask; "an_executor_cancel")]
    fn an_unguarded_mcp_call_follows_policy(
        call: (&str, &str),
        input: Value,
        rule: Option<Effect>,
        mode: PermissionMode,
    ) {
        smol::block_on(async {
            let manager = mcp_manager(call, rule);
            manager.set_session_mode(Some(mode));
            assert_eq!(mcp_prompt(&manager, call, &input, false).await, None);
        });
    }

    /// Autopilot reads a guarded call as forced, so it may not allow it.
    #[test]
    fn autopilot_cannot_allow_a_guarded_mcp_call() {
        let provider = ScriptedProvider::new(vec![Step::Answer(DELEGATE_ALLOW)]);
        let manager = default_mgr();
        manager.set_autopilot(Some(engaged_handle(provider.clone())));
        let (event_tx, events) = flume::unbounded();
        let event_tx = EventSender::new(event_tx, 0);

        let result = smol::block_on(enforce_mcp(
            &manager,
            (HOME_ASSISTANT_SERVER, TURN_ON_TOOL),
            &json!({"domain": "lock"}),
            false,
            &event_tx,
            None,
        ));

        let [prompt] = provider.prompts().try_into().unwrap();
        assert!(
            prompt.contains(&format!(
                "{ITEM_RESERVED_LABEL} {}",
                AutoNote::Forced.name()
            )),
            "{prompt}"
        );
        let guidance = result.unwrap_err().guidance.unwrap();
        assert_eq!(
            (guidance.text.as_str(), guidance.by),
            (UNATTENDED_ESCALATION, Answerer::Autopilot)
        );
        let events = events_of(&events);
        let decisions = decided(&events);
        let [decision] = decisions.as_slice() else {
            panic!("{decisions:?}");
        };
        assert_eq!(decision.action, ActionName::Escalate);
    }

    /// A read-only tool asks like any other in Ask, at Low risk, and offers
    /// to remember it with any arguments for as long as the user likes.
    #[test_case(true; "read_only")]
    #[test_case(false; "writes")]
    fn a_read_only_mcp_tool_offers_any_arguments(read_only: bool) {
        smol::block_on(async {
            let prompt = mcp_prompt(
                &default_mgr(),
                (HOME_ASSISTANT_SERVER, LOOKUP_TOOL),
                &json!({}),
                read_only,
            )
            .await
            .expect(MCP_PROMPT_MISSING);
            let option = prompt
                .options
                .iter()
                .find(|option| option.id == READ_ONLY_MCP_TOOL_OPTION);
            if !read_only {
                assert_eq!(prompt.risk, PermissionRisk::High);
                assert!(option.is_none());
                return;
            }
            let option = option.expect(READ_ONLY_OPTION_MISSING);
            assert_eq!(prompt.risk, PermissionRisk::Low);
            assert_eq!(
                (
                    option.allowed_lifetimes.as_slice(),
                    &option.rule.arguments,
                    option.broad,
                    option.is_default,
                    option.confirmation.as_deref(),
                ),
                (
                    [
                        PermissionLifetime::Conversation,
                        PermissionLifetime::Project,
                        PermissionLifetime::Global,
                    ]
                    .as_slice(),
                    &PermissionArgumentConstraint::Unconstrained,
                    false,
                    false,
                    None,
                )
            );
        });
    }

    /// The grant outlives the session and covers the tool's next call
    /// whatever its arguments, and no other tool.
    #[test_case(PermissionLifetime::Project; "project")]
    #[test_case(PermissionLifetime::Global; "global")]
    fn a_read_only_mcp_grant_persists_for_any_arguments(lifetime: PermissionLifetime) {
        smol::block_on(async {
            let temp = tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let state_dir = StateDir::from_path(temp.path().join("state"));
            let manager = persistent_manager(state_dir.clone(), &project);
            let (event_tx, events) = flume::unbounded();
            let event_tx = EventSender::new(event_tx, 0);
            let (_answer_tx, answers) = flume::unbounded();
            let answers = async_lock::Mutex::new(answers);
            let lookup = (HOME_ASSISTANT_SERVER, LOOKUP_TOOL);
            let first_input = json!({"area": "kitchen"});

            let call = enforce_mcp(
                &manager,
                lookup,
                &first_input,
                true,
                &event_tx,
                Some(&answers),
            );
            let answer = async {
                wait_for(&events, &mut Vec::new(), |event| {
                    matches!(event, AgentEvent::PermissionRequest(_))
                })
                .await;
                assert!(manager.answer(
                    MCP_REQUEST,
                    PermissionAnswer::AllowOption {
                        option_id: READ_ONLY_MCP_TOOL_OPTION.into(),
                        lifetime,
                    },
                ));
            };
            let (granted, ()) = future::zip(call, answer).await;
            granted.unwrap();
            drop(manager);

            let restarted = persistent_manager(state_dir, &project);
            let (event_tx, _events) = flume::unbounded();
            let event_tx = EventSender::new(event_tx, 0);
            enforce_mcp(
                &restarted,
                lookup,
                &json!({"area": "garage"}),
                true,
                &event_tx,
                None,
            )
            .await
            .unwrap();
            assert!(
                enforce_mcp(
                    &restarted,
                    (HOME_ASSISTANT_SERVER, TURN_ON_TOOL),
                    &json!({}),
                    false,
                    &event_tx,
                    None,
                )
                .await
                .is_err()
            );
        });
    }

    /// A request saved before calls carried MCP facts still decodes, and
    /// one that carries them round-trips.
    #[test]
    fn a_request_without_mcp_facts_decodes() {
        smol::block_on(async {
            let prompt = mcp_prompt(
                &default_mgr(),
                (HOME_ASSISTANT_SERVER, LOOKUP_TOOL),
                &json!({}),
                true,
            )
            .await
            .expect(MCP_PROMPT_MISSING);
            let wire = serde_json::to_value(&prompt).unwrap();
            assert_eq!(
                serde_json::from_value::<PermissionRequest>(wire.clone()).unwrap(),
                *prompt
            );
            let mut older = wire;
            older.as_object_mut().unwrap().remove(MCP_FACTS_FIELD);
            let decoded: PermissionRequest = serde_json::from_value(older).unwrap();
            assert_eq!(decoded.mcp, None);
            assert_eq!(
                serde_json::to_value(&decoded).unwrap().get(MCP_FACTS_FIELD),
                None
            );
        });
    }
}
