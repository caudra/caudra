use super::decisions::{advisories, label_answer};
use super::diagnostics::{answer_scope_kind, bounded_log_value};
use super::diagnostics::{prompt_reason, prompt_reason_message};
use super::manager::PERMISSION_POLL_INTERVAL;
use super::policy::TRUSTED_UNSCOPED_TOOLS;
use super::structured::trusted_shell_request;
use super::{
    AutoNote, DECISION_SOURCE_AUTO, DECISION_SOURCE_RULE, DECISION_SOURCE_USER_ABORT,
    DECISION_SOURCE_YOLO, DEFAULT_DENY_GUIDANCE, NORMALIZED_COMMAND_ATTRIBUTE,
    PERMISSION_DENIED_PREFIX, PERMISSION_LOG_TARGET, PROMPT_LOG_MAX_RESOURCES, PendingDecision,
    PendingPermission, PendingRegistration, PermissionAnswer, PermissionExecutorKind,
    PermissionLifetime, PermissionManager, PermissionMode, PermissionPolicyError,
    PermissionPresentation, PermissionRequest, PermissionResource, PermissionResourceAccess,
    PermissionResourceKind, PermissionSubject, PolicyRule, PromptReason, ResourceCoverage,
    RuleOrigin, ShellOpacity, StructuredPermissionDecision, StructuredPermissionEffect,
    answer_log_fields, command_pattern, normalize_scope_path, permission_rule_intersects_request,
    permission_rules_resource_standing, prompt_forcing_reason, remove_pending,
    subject_kind_and_contract, uncovered_resource_summary, update_presentation_coverage,
};
use crate::CancelToken;
use crate::tools::native::plan::{self, PlanAccess, PlanTarget};
use crate::tools::{PermissionIntent, PermissionScopes, ToolContext};
use crate::{AgentEvent, EventSender};
use caudra_config::{DefaultEffect, FILE_WRITE_TOOLS, ToolKey};
use serde_json::Value;
use std::borrow::Cow;
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
/// the empty list is the true answer rather than a missing one: `python_execution`
/// runs isolated with no filesystem, network or subprocess, and `batch`, `task`
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
    pub(super) guidance: Option<String>,
}

impl std::fmt::Display for PermissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} `{}` ({}).",
            PERMISSION_DENIED_PREFIX, self.tool, self.scope
        )?;
        if let Some(g) = &self.guidance {
            write!(f, " User guidance: {}", g)
        } else {
            write!(f, " {}", DEFAULT_DENY_GUIDANCE)
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
        Self {
            tool: tool.to_string(),
            scope: scope.to_string(),
            guidance: Some(guidance),
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

impl PendingRegistration<'_> {
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
        let deny = |source: &'static str, guidance: Option<String>| {
            caudra_otel::emit::tool_decision(
                &tool_string,
                caudra_otel::emit::DECISION_REJECT,
                source,
            );
            match guidance {
                Some(g) => PermissionError::with_guidance(&tool_string, &scope_display(), g),
                None => PermissionError::new(&tool_string, &scope_display()),
            }
        };
        let allowed = |source: &'static str| {
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
            match &identity {
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
        self.observe_pattern_request(&request);
        if scopes.plan_scoped {
            contain_authority_to_the_plan(&mut request);
        }
        let mut current = self
            .current_policy(&request, &context)
            .map_err(|error| deny(DECISION_SOURCE_RULE, Some(error.to_string())))?;
        if current.automatic {
            return allowed(current.source);
        }
        let mut receipts = Vec::new();
        let mut escalation = None;
        if let Some(AutoEligibility::Direct | AutoEligibility::NeedsEngine) = current.auto {
            let screened = self
                .screen_auto_candidate(&request, &context, cancel)
                .await
                .map_err(|error| deny(DECISION_SOURCE_RULE, Some(error.to_string())))?;
            current = screened.policy;
            if current.automatic {
                return allowed(current.source);
            }
            if screened.approved {
                return allowed(DECISION_SOURCE_AUTO);
            }
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

        let Some(_) = user_response_rx else {
            warn!(tool = %tool, scope = %scope_display(), "no permission response channel");
            if let Some(effect) = self.record_escalation(escalation, &context) {
                effect.detach();
            }
            return Err(deny(DECISION_SOURCE_USER_ABORT, None));
        };

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
                    cancel: cancel.clone(),
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
        let wait = async {
            let mut source_request_id = String::new();
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
                            (service, decision, false)
                        },
                    )
                    .await;
                    if let Some(decision) = decision {
                        if let Some(effect) = registration.annotate(
                            &mut request,
                            &decision,
                            advice_revision,
                            &service,
                        ) {
                            effect.detach();
                        }
                        if label_user && let Some(receipt) = decision.evaluation.receipt {
                            receipts.push((service, receipt));
                        }
                    }
                    Ok(None)
                })
                .await?;
                if let Some(decision) = wake {
                    return Ok(decision);
                }
            }
        };
        let decision = match cancel.race(wait).await {
            Ok(Ok(decision)) => Some(decision),
            Ok(Err(error)) => Some(PendingDecision::PolicyDenied(error.to_string())),
            Err(_) => None,
        };
        if let Some(PendingDecision::Explicit(answer)) = &decision {
            let label_advisory = self
                .decisions()
                .filter(|service| service.config().log)
                .and_then(|_| advisory_task.take());
            if let Some(label) = label_answer(
                receipts,
                label_advisory,
                answer,
                waiting_since.elapsed().as_millis() as u64,
            ) {
                label.detach();
            }
        }
        // Paired with `permission_prompt` by `request_id`: the two together give
        // the prompt rate, what authority the answer bought, and the wait cost.
        let (answer, option_id, lifetime, answer_source) = match &decision {
            Some(PendingDecision::Explicit(explicit)) => {
                let (answer, option_id, lifetime) = answer_log_fields(explicit);
                (answer, option_id, lifetime, explicit.decision_source())
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
                Some(PendingDecision::Explicit(answer)) => answer_scope_kind(answer, &request),
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
            PendingDecision::Explicit(answer) => answer.is_allow(),
            PendingDecision::MatchedRule => true,
            PendingDecision::PolicyDenied(_) => false,
        };
        let mut source = match &decision {
            PendingDecision::Explicit(answer) => answer.decision_source(),
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
            if let PendingDecision::Explicit(answer) = &decision {
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
            allowed(source)
        } else {
            let guidance = match decision {
                PendingDecision::Explicit(answer) => answer.guidance().map(String::from),
                PendingDecision::MatchedRule => None,
                PendingDecision::PolicyDenied(guidance) => Some(guidance),
            };
            Err(deny(source, guidance))
        }
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

/// Whether Auto may settle a request the policy leaves to the user, or the
/// first reason it may not, in the precedence of the prompt's own reason.
///
/// A protected or prepared resource normally means Auto never applies. The one
/// exception is a whole command line the first-party shell could not review
/// command by command, for a cause an engine reading the full text may screen.
/// A cause that always asks restricts from any source, but only the trusted
/// shell's facts can open the engine path.
fn auto_eligibility(
    request: &PermissionRequest,
    context: &EvaluationContext,
    coverage: &RequestCoverage,
    default: DefaultEffect,
) -> AutoEligibility {
    let gated: Vec<_> = request
        .resources
        .iter()
        .filter(|resource| resource.protected || resource.requires_prompt)
        .map(ShellOpacity::of)
        .collect();
    let worst = gated.iter().flatten().max().copied();
    let note = if context.plan_scoped {
        AutoNote::Planning
    } else if context.forced {
        AutoNote::Forced
    } else if let Some(cause) = worst.filter(|cause| !cause.screenable()) {
        AutoNote::AlwaysAsks(cause)
    } else if !gated.is_empty() && (gated.contains(&None) || !trusted_shell_request(request)) {
        AutoNote::Protected
    } else if coverage.must_prompt {
        AutoNote::RuleAsks
    } else if gated.is_empty() && context.force_prompt {
        AutoNote::Forced
    } else if default != DefaultEffect::Prompt {
        AutoNote::ToolDefault
    } else if gated.is_empty() {
        return AutoEligibility::Direct;
    } else {
        return AutoEligibility::NeedsEngine;
    };
    AutoEligibility::Never(note)
}

#[cfg(test)]
pub(super) mod tests {
    use super::{
        ACTIVE_PLAN_INTENT_MISMATCH, AutoEligibility, CURRENT_DEFAULT_DENIES_REQUEST,
        CURRENT_POLICY_DENIES_REQUEST, EvaluationContext, PendingDecision, PendingPermission,
        PendingRegistration, contain_authority_to_the_plan, remove_pending,
    };
    use crate::permissions::tests::{
        CONTROLLED_REQUEST, FIRST_COMMAND, SECOND_COMMAND, controlled_enforcement, remember_command,
    };
    use crate::{CancelToken, EventSender};
    use std::sync::atomic::Ordering;

    use std::collections::BTreeMap;

    use crate::tools::native;
    use crate::tools::native::memory::{self, LOCAL_MEMORY_RESOURCE};
    use crate::tools::native::plan::{self, PlanAccess, PlanTool};
    use crate::tools::registry::Tool;
    use crate::tools::test_support::stub_ctx;
    use crate::tools::{
        MEMORY_TOOL_NAME, PermissionIntent, PermissionScopes, ToolAudience, ToolContext,
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
    use tempfile::{Builder, TempDir};

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
        ComposedRow, PROMPT_REASON_ASK_RULE, PROMPT_REASON_FORCED, PROMPT_REASON_PROTECTED,
        PROMPT_REASON_UNCOVERED, PermissionAnswer, PermissionAuthorityProfile,
        PermissionExecutorKind, PermissionLifetime, PermissionManager, PermissionMode,
        PermissionPolicyError, PermissionPresentation, PermissionRequest, PermissionResource,
        PermissionResourceAccess, PermissionResourceKind, PermissionRisk, PermissionRowGrant,
        PermissionRuleRecord, PermissionSubject, PromptReason, RemotePermissionIdentity,
        ResourceCoverage, RevokedRuleScope, RuleOrigin, StructuredPermissionDecision,
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
    const IMPOSTOR_PLUGIN: &str = "impostor";
    #[cfg(unix)]
    const PRIVATE_DIRECTORY_MODE: u32 = 0o700;

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
        let mut builder = Builder::new();
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
                    assert_eq!(error.guidance, Some(expected));
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
                assert_eq!(error.guidance.as_deref(), Some(ACTIVE_PLAN_INTENT_MISMATCH));
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
                    .and_then(|error| error.guidance);
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
                        enforcement.await.unwrap_err().guidance,
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
                    .guidance,
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
                decision_engine: true,
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

    /// `python_execution` runs isolated, so the resource list it declares is
    /// empty because there is nothing to declare. Failing that closed leaves a
    /// tool that cannot be called at all, which is what remote mode hit. The
    /// request is evaluated instead, so an explicit deny still stops it and the
    /// guard keeps failing closed for a tool that should have named something.
    #[test_case(false => true ; "isolated_tool_runs_without_naming_a_resource")]
    #[test_case(true => false ; "an_explicit_deny_still_stops_it")]
    fn an_unscoped_tool_may_declare_no_resources(denied: bool) -> bool {
        const ISOLATED_TOOL: &str = "python_execution";

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
            let rules = rule
                .map(|effect| shell_policy_rule(FIRST_COMMAND, effect))
                .into_iter()
                .collect();
            let manager = mgr_with(make_config(rules), PathBuf::from(SHELL_WORKDIR));
            manager.set_session_mode(Some(PermissionMode::Auto));
            let mut intent = shell_intent(&[FIRST_COMMAND]);
            intent.scopes.force_prompt = forced;
            intent.scopes.plan_scoped = plan_scoped;
            intent.resources[0].protected = protected;
            let prompt = shell_prompt(&manager, &intent, FIRST_COMMAND)
                .await
                .expect(SHELL_PROMPT_MISSING);
            assert_eq!(prompt.presentation.auto, Some(note));
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
                cancel: CancelToken::none(),
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
                .try_send(PendingDecision::Explicit(PermissionAnswer::AllowOnce))
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
                cancel: CancelToken::none(),
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
            Ok(PendingDecision::Explicit(PermissionAnswer::Deny))
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(PendingDecision::Explicit(PermissionAnswer::AllowOnce))
        ));
    }
}
