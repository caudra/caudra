use super::enforce::{AutoEligibility, CurrentPolicy, EvaluationContext};
use super::structured::{WORKDIR_ATTRIBUTE, kind_noun};
use super::{
    AutoNote, EngineFlag, PendingRegistration, PermissionAdvisory, PermissionAnswer,
    PermissionManager, PermissionPolicyError, PermissionRequest, PermissionResource,
    PermissionResourceKind,
};
use crate::decisions::{
    DecisionContext, DecisionFeature, DecisionReceipt, Decisions, PermissionAction,
    PermissionDecision, PermissionPurpose,
};
use crate::{AgentEvent, CancelToken};
use caudra_config::decisions::FeatureMode;
use caudra_storage::decision_log::{DecisionEffect, DecisionLabel};
use caudra_storage::now_epoch;
use futures_lite::future;
use serde_json::{Value, json};
use smol::Task;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tracing::warn;

const SCREEN_CANCELLED: &str = "permission screening cancelled";
const LABEL_SOURCE: &str = "user";
const EFFECT_UPDATE_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) type AdvisoryTask = Task<(Decisions, Option<PermissionDecision>)>;
pub(super) type ShellAdvisoryTask = AdvisoryTask;
pub(super) type PermissionReceipt = (Decisions, DecisionReceipt);

pub(super) struct AutoScreening {
    pub(super) policy: CurrentPolicy,
    pub(super) approved: bool,
    /// Why screening left a still-eligible request to the user.
    pub(super) note: Option<AutoNote>,
    pub(super) decision: Option<(Decisions, PermissionDecision)>,
    pub(super) escalation: Option<EscalationReceipt>,
}

pub(super) struct EscalationReceipt {
    service: Decisions,
    receipt: DecisionReceipt,
    revision: u64,
}

fn record_effect(
    service: &Decisions,
    receipt: &DecisionReceipt,
    effect: DecisionEffect,
) -> Task<()> {
    let service = service.clone();
    let receipt = receipt.clone();
    smol::spawn(async move {
        future::race(
            async {
                if let Err(error) = service.record_effect(&receipt, effect).await {
                    warn!(%error, "permission decision effect was not recorded");
                }
            },
            async {
                smol::Timer::after(EFFECT_UPDATE_TIMEOUT).await;
                warn!("permission decision effect recording timed out");
            },
        )
        .await;
    })
}

fn state(request: &PermissionRequest) -> Value {
    decision_state(
        &request.tool.to_string(),
        &request.input,
        &request.resources,
    )
}

/// Command resources repeat `input.command` segment by segment, so only the
/// other resources reach the engine, each with its kind as a plain noun.
pub(crate) fn decision_state(tool: &str, input: &Value, resources: &[PermissionResource]) -> Value {
    let mut state = json!({"tool": tool, "input": input});
    if let Some(workdir) = resources
        .iter()
        .find_map(|resource| resource.attributes.get(WORKDIR_ATTRIBUTE))
    {
        state["workdir"] = json!(workdir);
    }
    let resources: Vec<_> = resources
        .iter()
        .filter(|resource| resource.kind != PermissionResourceKind::Command)
        .map(|resource| {
            let mut entry = json!({"kind": kind_noun(&resource.kind), "value": resource.value});
            if let Some(access) = &resource.access {
                entry["access"] = json!(access);
            }
            entry
        })
        .collect();
    if !resources.is_empty() {
        state["resources"] = json!(resources);
    }
    state
}

fn context(manager: &PermissionManager, request: &PermissionRequest) -> DecisionContext {
    manager.decision_context(json!({"request_id": request.id}))
}

pub(super) fn advisories(decision: &PermissionDecision) -> Vec<PermissionAdvisory> {
    let Some(PermissionAction::Advice(flags) | PermissionAction::Escalate(flags)) =
        &decision.action
    else {
        return Vec::new();
    };
    EngineFlag::ALL
        .into_iter()
        .filter_map(|flag| {
            flags
                .iter()
                .find(|candidate| {
                    candidate.flag == flag.name()
                        && candidate.probability.is_finite()
                        && (0.0..=1.0).contains(&candidate.probability)
                })
                .map(|candidate| PermissionAdvisory {
                    flag,
                    probability: candidate.probability,
                })
        })
        .collect()
}

impl PermissionManager {
    pub async fn run_passive_decision<T>(&self, future: impl Future<Output = T>) -> Option<T> {
        let revision = self.passive_decision_revision()?;
        let changed = self.broker.passive_changed.listen();
        if !self.passive_decision_is_current(revision) {
            return None;
        }
        let outcome = future::race(
            async {
                changed.await;
                None
            },
            async { Some(future.await) },
        )
        .await;
        outcome.filter(|_| self.passive_decision_is_current(revision))
    }

    pub(crate) fn run_permission_decision<T, F: Future<Output = T>>(
        &self,
        future: F,
    ) -> impl Future<Output = Option<T>> + use<T, F> {
        let revision = self.broker.revision.load(Ordering::Acquire);
        let changed = self.broker.changed.listen();
        let eligible = self.permission_decision_revision().is_some();
        let broker = Arc::clone(&self.broker);
        async move {
            if !eligible || broker.revision.load(Ordering::Acquire) != revision {
                return None;
            }
            let outcome = future::race(
                async {
                    changed.await;
                    None
                },
                async { Some(future.await) },
            )
            .await;
            outcome.filter(|_| broker.revision.load(Ordering::Acquire) == revision)
        }
    }

    pub(super) async fn screen_auto_candidate(
        &self,
        request: &PermissionRequest,
        evaluation: &EvaluationContext,
        cancel: &CancelToken,
    ) -> Result<AutoScreening, PermissionPolicyError> {
        let changed = self.broker.changed.listen();
        let revision = self.broker.revision.load(Ordering::Acquire);
        let policy = self.current_policy(request, evaluation)?;
        let Some(eligibility @ (AutoEligibility::Direct | AutoEligibility::NeedsEngine)) =
            policy.auto
        else {
            return Ok(AutoScreening {
                policy,
                approved: false,
                note: None,
                decision: None,
                escalation: None,
            });
        };
        let decisions = self.decisions();
        let restricted = decisions
            .as_ref()
            .is_some_and(|service| service.config().auto_screening_restricted);
        let service = decisions.filter(|service| service.enabled(&DecisionFeature::AutoScreening));
        let enforcing = service.as_ref().is_some_and(|service| {
            service.config().features.auto_screening == FeatureMode::Enforce
        });
        let advisory = service.is_some();
        let mut decision = None;
        if let Some(service) = service.filter(|_| !restricted) {
            let input = state(request);
            let context = context(self, request);
            decision = cancel
                .race(future::race(
                    async {
                        changed.await;
                        None
                    },
                    service.permission(PermissionPurpose::AutoScreening, &input, &context),
                ))
                .await
                .map_err(|_| PermissionPolicyError(SCREEN_CANCELLED.into()))?
                .map(|result| (service, result));
        }
        let screened = if restricted {
            Err(AutoNote::Restricted)
        } else if enforcing {
            match decision.as_ref().map(|(_, decision)| decision) {
                Some(decision) if decision.evaluation.result.is_err() => {
                    Err(AutoNote::EngineUnavailable)
                }
                Some(decision) if decision.action.is_some() => Err(AutoNote::EngineFlagged),
                Some(_) => Ok(()),
                None => Err(AutoNote::EngineUnavailable),
            }
        } else if eligibility == AutoEligibility::Direct {
            Ok(())
        } else if advisory {
            Err(AutoNote::EngineAdvisory)
        } else {
            Err(AutoNote::EngineNeeded)
        };
        let policy = self.current_policy(request, evaluation)?;
        let unchanged = self.broker.revision.load(Ordering::Acquire) == revision;
        let approved = screened.is_ok()
            && !cancel.is_cancelled()
            && policy.auto == Some(eligibility)
            && unchanged;
        if !unchanged {
            decision = None;
        }
        let escalation = decision.as_ref().and_then(|(service, decision)| {
            matches!(decision.action, Some(PermissionAction::Escalate(_)))
                .then(|| decision.evaluation.receipt.as_ref())
                .flatten()
                .map(|receipt| EscalationReceipt {
                    service: service.clone(),
                    receipt: receipt.clone(),
                    revision,
                })
        });
        Ok(AutoScreening {
            policy,
            approved,
            note: screened.err(),
            decision,
            escalation,
        })
    }

    pub(super) fn record_escalation(
        &self,
        escalation: Option<EscalationReceipt>,
        context: &EvaluationContext,
    ) -> Option<Task<()>> {
        let escalation = escalation?;
        let revision = self
            .context_revision
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let _mutation = self
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *revision != context.revision
            || escalation.revision != self.broker.revision.load(Ordering::Acquire)
            || self.is_yolo()
        {
            return None;
        }
        Some(record_effect(
            &escalation.service,
            &escalation.receipt,
            DecisionEffect::Escalated,
        ))
    }

    pub(super) fn permission_advice(&self, request: &PermissionRequest) -> Option<AdvisoryTask> {
        if self.is_yolo() {
            return None;
        }
        let service = self
            .decisions()
            .filter(|service| service.enabled(&DecisionFeature::PermissionAdvice))?;
        let input = state(request);
        let context = context(self, request);
        let evaluation_service = service.clone();
        let decision = self.run_permission_decision(async move {
            evaluation_service
                .permission(PermissionPurpose::Advice, &input, &context)
                .await
        });
        Some(smol::spawn(
            async move { (service, decision.await.flatten()) },
        ))
    }

    pub(super) fn shell_effect_advice(
        &self,
        request: &PermissionRequest,
        plan: bool,
    ) -> Option<ShellAdvisoryTask> {
        if self.is_yolo()
            || !request
                .resources
                .iter()
                .any(|resource| resource.kind == PermissionResourceKind::Command)
        {
            return None;
        }
        let service = self
            .decisions()
            .filter(|service| service.enabled(&DecisionFeature::ShellEffect))?;
        let command = request.input.get("command")?.as_str()?.to_owned();
        let context = context(self, request);
        let evaluation_service = service.clone();
        let decision = self.run_permission_decision(async move {
            evaluation_service
                .shell_effect(&command, false, plan, &context)
                .await
        });
        Some(smol::spawn(
            async move { (service, decision.await.flatten()) },
        ))
    }
}

impl PendingRegistration<'_> {
    pub(super) fn annotate(
        &self,
        request: &mut PermissionRequest,
        decision: &PermissionDecision,
        revision: u64,
        service: &Decisions,
    ) -> Option<Task<()>> {
        let _mutation = self
            .manager
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.manager.is_yolo()
            || revision != self.manager.broker.revision.load(Ordering::Acquire)
        {
            return None;
        }
        let mut advisories = advisories(decision);
        for previous in &request.presentation.advisories {
            if let Some(current) = advisories
                .iter_mut()
                .find(|current| current.flag == previous.flag)
            {
                current.probability = current.probability.max(previous.probability);
            } else {
                advisories.push(previous.clone());
            }
        }
        if advisories.is_empty() || request.presentation.advisories == advisories {
            return None;
        }
        let mut pending = self.manager.pending();
        let candidate = pending
            .get_mut(&self.manager.id)
            .and_then(|requests| requests.get_mut(self.request_id))
            .filter(|candidate| {
                !candidate.answering
                    && !candidate.abandoned
                    && candidate.sender.same_channel(&self.sender)
            })?;
        request.presentation.advisories = advisories;
        candidate.request.presentation.advisories = request.presentation.advisories.clone();
        self.event_tx
            .send(AgentEvent::PermissionRequestUpdated(Box::new(
                request.clone(),
            )))
            .ok()?;
        decision
            .evaluation
            .receipt
            .as_ref()
            .map(|receipt| record_effect(service, receipt, DecisionEffect::Advised))
    }
}

pub(super) fn label_answer(
    mut receipts: Vec<PermissionReceipt>,
    advisory: Option<AdvisoryTask>,
    answer: &PermissionAnswer,
    waited_ms: u64,
) -> Option<Task<()>> {
    if receipts.is_empty() && advisory.is_none() {
        return None;
    }
    let label = DecisionLabel {
        expected: json!({"user_approves": answer.is_allow()}),
        source: LABEL_SOURCE.into(),
        timestamp: now_epoch(),
        meta: json!({"answer": answer.decision_source(), "waited_ms": waited_ms}),
    };
    Some(smol::spawn(async move {
        if let Some(advisory) = advisory {
            let (service, decision) = advisory.await;
            if let Some(receipt) = decision.and_then(|decision| decision.evaluation.receipt) {
                receipts.push((service, receipt));
            }
        }
        for (service, receipt) in receipts {
            if let Err(error) = service.attach_label(&receipt, &label).await {
                warn!(%error, "permission decision label was not recorded");
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::{advisories, label_answer, state};
    use crate::decisions::{
        DecisionContext, DecisionOutcome, DecisionReceipt, Decisions, PermissionAction,
        PermissionDecision, PermissionFlag, PermissionPurpose,
    };
    use crate::permissions::enforce::EvaluationContext;
    use crate::permissions::tests::{
        CONTROLLED_REQUEST, FIRST_COMMAND, SHELL_WORKDIR, controlled_enforcement, default_mgr,
        enforce_shell_without_prompt, make_config, mgr_with, opaque_line_intent, shell_policy_rule,
        shell_prompt, shell_request, workcell_shell_subject,
    };
    use crate::permissions::{
        AutoNote, EngineFlag, PendingRegistration, PermissionAnswer, PermissionManager,
        PermissionMode, PermissionResourceAccess, PermissionResourceKind, ScriptLanguage,
        ShellOpacity, filesystem_permission_resource,
    };
    use crate::tools::PermissionScopes;
    use crate::{AgentEvent, CancelToken, EventSender};
    use async_trait::async_trait;
    use caudra_config::decisions::{DecisionsConfig, FeatureMode};
    use caudra_config::{Effect, PermissionsConfig, ToolKey};
    use caudra_decision::{
        Answer, DecisionEngine, DecisionError, DecisionRequest, DecisionResponse, NoulAnswer, Usage,
    };
    use caudra_storage::StateDir;
    use caudra_storage::decision_log::{DecisionEffect, DecisionLog};
    use caudra_storage::id::CaudraId;
    use futures_lite::future;
    use serde_json::{Value, json};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;
    use tempfile::TempDir;
    use test_case::test_case;

    const BASE_URL: &str = "http://127.0.0.1:1";
    const WRITTEN_FILE: &str = "build.log";
    const FILE_NOUN: &str = "file";
    const WRITE_ACCESS: &str = "write";
    const TIMEOUT_MS: u64 = 5_000;
    const SHORT_TIMEOUT_MS: u64 = 1;
    const UNKNOWN_FLAG: &str = "untrusted text";
    const PROMPT_MISSING: &str = "expected a permission prompt";
    const UPDATE_MISSING: &str = "expected advisory update";
    const PREMATURE_ADMISSION: &str = "advisory must not settle the prompt";
    const OVERSIZED_STATE_BYTES: usize = 16_384;
    const SCREENABLE_LINE: &str = "cargo build > build.log";
    const SCRIPT_STEPS: usize = 24;
    const INLINE_PYTHON: ShellOpacity = ShellOpacity::InlineScript {
        language: ScriptLanguage::Python,
    };

    #[derive(Clone)]
    enum Behavior {
        Probability(f64),
        Error,
        Invalid,
        Pending,
    }

    /// Reports the state of every call it starts, so a case can read exactly
    /// what the engine was given.
    struct FakeEngine {
        behavior: Behavior,
        calls: Arc<AtomicUsize>,
        started: flume::Sender<Value>,
        release: Option<flume::Receiver<()>>,
    }

    #[async_trait]
    impl DecisionEngine for FakeEngine {
        async fn decide(
            &self,
            request: &DecisionRequest,
            _deadline: Instant,
        ) -> Result<DecisionResponse, DecisionError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let _ = self.started.try_send(request.state.clone());
            if let Some(release) = &self.release {
                let _ = release.recv_async().await;
            }
            let probability = match self.behavior {
                Behavior::Probability(probability) => probability,
                Behavior::Error => return Err(DecisionError::Unreachable),
                Behavior::Invalid => 0.0,
                Behavior::Pending => return future::pending().await,
            };
            Ok(DecisionResponse {
                model: request.model.clone(),
                answers: request
                    .questions
                    .keys()
                    .filter(|_| !matches!(self.behavior, Behavior::Invalid))
                    .map(|id| (id.clone(), Answer::Noul(NoulAnswer { noul: probability })))
                    .collect(),
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                cache_hit: false,
            })
        }
    }

    struct Service {
        decisions: Decisions,
        calls: Arc<AtomicUsize>,
        started: flume::Receiver<Value>,
        release: flume::Sender<()>,
        _state: TempDir,
    }

    fn service(
        behavior: Behavior,
        screening: FeatureMode,
        advice: FeatureMode,
        delayed: bool,
    ) -> Service {
        let mut config = DecisionsConfig {
            base_url: Some(BASE_URL.parse().unwrap()),
            timeout_ms: TIMEOUT_MS,
            ..Default::default()
        };
        if matches!(behavior, Behavior::Pending) && !delayed {
            config.timeout_ms = SHORT_TIMEOUT_MS;
        }
        config.features.auto_screening = screening;
        config.features.permission_advice = advice;
        configured_service(behavior, config, delayed)
    }

    fn configured_service(behavior: Behavior, config: DecisionsConfig, delayed: bool) -> Service {
        let state = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let (started, received) = flume::bounded(1);
        let (release, released) = flume::bounded(1);
        let engine = FakeEngine {
            behavior,
            calls: Arc::clone(&calls),
            started,
            release: delayed.then_some(released),
        };
        let decisions = Decisions::with_engine(
            config,
            &StateDir::from_path(state.path().to_path_buf()),
            engine,
        )
        .unwrap();
        Service {
            decisions,
            calls,
            started: received,
            release,
            _state: state,
        }
    }

    #[test_case(Behavior::Probability(0.0), FeatureMode::Enforce, true; "clear_runs")]
    #[test_case(Behavior::Probability(1.0), FeatureMode::Enforce, false; "flagged_prompts")]
    #[test_case(Behavior::Error, FeatureMode::Enforce, false; "failed_prompts")]
    #[test_case(Behavior::Invalid, FeatureMode::Enforce, false; "invalid_prompts")]
    #[test_case(Behavior::Pending, FeatureMode::Enforce, false; "timeout_prompts")]
    #[test_case(Behavior::Probability(1.0), FeatureMode::Shadow, true; "shadow_does_not_escalate")]
    #[test_case(Behavior::Error, FeatureMode::Shadow, true; "shadow_failure_preserves_baseline")]
    fn auto_screening_is_caution_only(behavior: Behavior, screening: FeatureMode, allowed: bool) {
        smol::block_on(async {
            let may_time_out_before_dispatch = matches!(behavior, Behavior::Pending);
            let service = service(behavior, screening, FeatureMode::Off, false);
            let manager = default_mgr();
            manager.set_session_mode(Some(PermissionMode::Auto));
            manager.set_decisions(Some(service.decisions));
            assert_eq!(
                enforce_shell_without_prompt(&manager, &[FIRST_COMMAND], false)
                    .await
                    .is_ok(),
                allowed
            );
            let calls = service.calls.load(Ordering::Relaxed);
            assert!(calls == 1 || (may_time_out_before_dispatch && calls == 0));
        });
    }

    /// A line the shell could not review runs in Auto only on an enforcing
    /// engine's clear answer; anything less asks, and says what was missing.
    #[test_case(Some(Behavior::Probability(0.0)), FeatureMode::Enforce, false, None; "enforcing_engine_clears_the_line")]
    #[test_case(Some(Behavior::Probability(1.0)), FeatureMode::Enforce, false, Some(AutoNote::EngineFlagged); "flagged")]
    #[test_case(Some(Behavior::Probability(0.0)), FeatureMode::Shadow, false, Some(AutoNote::EngineAdvisory); "advice_mode")]
    #[test_case(None, FeatureMode::Off, false, Some(AutoNote::EngineNeeded); "no_engine")]
    #[test_case(Some(Behavior::Probability(0.0)), FeatureMode::Enforce, true, Some(AutoNote::Restricted); "restricted")]
    #[test_case(Some(Behavior::Error), FeatureMode::Enforce, false, Some(AutoNote::EngineUnavailable); "engine_error")]
    fn auto_sends_screenable_lines_to_an_enforcing_engine(
        engine: Option<Behavior>,
        screening: FeatureMode,
        restricted: bool,
        note: Option<AutoNote>,
    ) {
        smol::block_on(async {
            let service = engine.map(|behavior| {
                let mut config = DecisionsConfig {
                    base_url: Some(BASE_URL.parse().unwrap()),
                    timeout_ms: TIMEOUT_MS,
                    auto_screening_restricted: restricted,
                    ..Default::default()
                };
                config.features.auto_screening = screening;
                configured_service(behavior, config, false)
            });
            let manager = default_mgr();
            manager.set_session_mode(Some(PermissionMode::Auto));
            manager.set_decisions(service.as_ref().map(|service| service.decisions.clone()));
            let intent = opaque_line_intent(SCREENABLE_LINE, ShellOpacity::Redirect);
            let prompt = shell_prompt(&manager, &intent, SCREENABLE_LINE).await;
            assert_eq!(
                prompt.map(|request| request.presentation.auto),
                note.map(Some)
            );
            if let Some(service) = service {
                assert_eq!(
                    service.calls.load(Ordering::Relaxed),
                    usize::from(!restricted)
                );
            }
        });
    }

    #[test_case("sudo cargo build", ShellOpacity::Privilege; "privilege")]
    #[test_case("eval cargo build", ShellOpacity::Indirect; "indirect")]
    #[test_case("cargo build &&", ShellOpacity::Unparsed; "unparsed")]
    fn always_asking_lines_never_reach_the_engine(line: &str, cause: ShellOpacity) {
        smol::block_on(async {
            let service = service(
                Behavior::Probability(0.0),
                FeatureMode::Enforce,
                FeatureMode::Off,
                false,
            );
            let manager = default_mgr();
            manager.set_session_mode(Some(PermissionMode::Auto));
            manager.set_decisions(Some(service.decisions));
            let prompt = shell_prompt(&manager, &opaque_line_intent(line, cause), line)
                .await
                .expect(PROMPT_MISSING);
            assert_eq!(prompt.presentation.auto, Some(AutoNote::AlwaysAsks(cause)));
            assert_eq!(service.calls.load(Ordering::Relaxed), 0);
        });
    }

    /// The engine judges the script itself, so it has to read all of it, not
    /// the shortened form a prompt displays.
    #[test]
    fn a_long_script_reaches_the_engine_untruncated() {
        smol::block_on(async {
            let steps: Vec<_> = (0..SCRIPT_STEPS)
                .map(|step| format!("print('step {step}')"))
                .collect();
            let script = format!("python3 - <<'PY'\n{}\nPY", steps.join("\n"));
            let service = service(
                Behavior::Probability(0.0),
                FeatureMode::Enforce,
                FeatureMode::Off,
                false,
            );
            let manager = default_mgr();
            manager.set_session_mode(Some(PermissionMode::Auto));
            manager.set_decisions(Some(service.decisions));
            let intent = opaque_line_intent(&script, INLINE_PYTHON);
            assert!(shell_prompt(&manager, &intent, &script).await.is_none());
            let state = service.started.try_recv().unwrap();
            assert_eq!(state["input"]["command"], script);
        });
    }

    #[test_case(PermissionMode::Ask, Effect::Ask, false; "ask_stays_ask")]
    #[test_case(PermissionMode::Auto, Effect::Ask, false; "auto_preserves_ask")]
    #[test_case(PermissionMode::Auto, Effect::Deny, false; "auto_preserves_deny")]
    #[test_case(PermissionMode::Auto, Effect::Allow, true; "baseline_allow_skips_engine")]
    #[test_case(PermissionMode::Yolo, Effect::Ask, true; "yolo_never_screens")]
    #[test_case(PermissionMode::Yolo, Effect::Deny, false; "yolo_keeps_denies")]
    fn engine_never_weakens_deterministic_rules(
        mode: PermissionMode,
        effect: Effect,
        allowed: bool,
    ) {
        smol::block_on(async {
            let service = service(
                Behavior::Pending,
                FeatureMode::Enforce,
                FeatureMode::Advise,
                true,
            );
            let manager = mgr_with(
                make_config(vec![shell_policy_rule(FIRST_COMMAND, effect)]),
                PathBuf::from(SHELL_WORKDIR),
            );
            manager.set_session_mode(Some(mode));
            manager.set_decisions(Some(service.decisions));
            assert_eq!(
                enforce_shell_without_prompt(&manager, &[FIRST_COMMAND], false)
                    .await
                    .is_ok(),
                allowed
            );
            assert_eq!(service.calls.load(Ordering::Relaxed), 0);
        });
    }

    #[test_case(Some(PermissionMode::Auto); "stored_auto")]
    #[test_case(None; "seeded_auto")]
    fn auto_without_the_experiment_prompts_like_ask(stored: Option<PermissionMode>) {
        smol::block_on(async {
            let service = service(
                Behavior::Probability(0.0),
                FeatureMode::Enforce,
                FeatureMode::Off,
                false,
            );
            let manager = mgr_with(PermissionsConfig::default(), PathBuf::from(SHELL_WORKDIR));
            manager.set_seed_mode(PermissionMode::Auto);
            manager.set_session_mode(stored.clone());
            manager.set_decisions(Some(service.decisions));
            assert!(manager.decisions().is_none());
            assert!(
                enforce_shell_without_prompt(&manager, &[FIRST_COMMAND], false)
                    .await
                    .is_err()
            );
            assert_eq!(service.calls.load(Ordering::Relaxed), 0);
            assert_eq!(manager.persisted_mode(), stored);
        });
    }

    #[test_case(false, true; "off_without_endpoint_preserves_auto")]
    #[test_case(true, false; "restricted_without_endpoint_prompts")]
    fn project_restriction_is_sticky_without_engine(restricted: bool, allowed: bool) {
        smol::block_on(async {
            let state = tempfile::tempdir().unwrap();
            let decisions = Decisions::new(
                DecisionsConfig {
                    auto_screening_restricted: restricted,
                    ..Default::default()
                },
                &StateDir::from_path(state.path().to_path_buf()),
            )
            .unwrap();
            let manager = default_mgr();
            let fork = manager.fork();
            manager.set_decisions(Some(decisions));
            assert!(fork.decisions().is_some());
            fork.set_session_mode(Some(PermissionMode::Auto));
            assert_eq!(
                enforce_shell_without_prompt(&fork, &[FIRST_COMMAND], false)
                    .await
                    .is_ok(),
                allowed
            );
            manager.set_decisions(None);
            assert!(fork.decisions().is_none());
        });
    }

    #[test_case(PermissionMode::Yolo, true; "yolo_immediately_bypasses_pending_engine")]
    #[test_case(PermissionMode::Ask, false; "ask_withdraws_auto")]
    #[test_case(PermissionMode::Auto, false; "revision_invalidates_screen")]
    fn changed_mode_or_revision_cannot_reuse_screening(mode: PermissionMode, allowed: bool) {
        smol::block_on(async {
            let service = service(
                Behavior::Probability(0.0),
                FeatureMode::Enforce,
                FeatureMode::Off,
                true,
            );
            let manager = default_mgr();
            manager.set_session_mode(Some(PermissionMode::Auto));
            manager.set_decisions(Some(service.decisions));
            let scopes = PermissionScopes::single(FIRST_COMMAND.into());
            let (sender, events) = flume::unbounded();
            let sender = EventSender::new(sender, 0);
            let cancel = CancelToken::none();
            let mut enforcement =
                Box::pin(controlled_enforcement(&manager, &scopes, &sender, &cancel));
            assert!(future::poll_once(&mut enforcement).await.is_none());
            assert_eq!(service.calls.load(Ordering::Relaxed), 1);
            assert!(events.is_empty());
            manager.set_session_mode(Some(mode));
            let result = future::poll_once(&mut enforcement).await;
            if allowed {
                assert!(result.unwrap().is_ok());
                assert!(events.is_empty());
            } else {
                assert!(result.is_none());
                assert!(matches!(
                    events.try_recv().unwrap().event,
                    AgentEvent::PermissionRequest(_)
                ));
                assert!(future::poll_once(&mut enforcement).await.is_none());
                assert!(manager.answer(CONTROLLED_REQUEST, PermissionAnswer::Deny));
                assert!(enforcement.await.is_err());
            }
        });
    }

    #[test_case(false; "context_changed")]
    #[test_case(true; "cancelled")]
    fn screening_aborts_on_context_change_or_cancellation(cancelled: bool) {
        smol::block_on(async {
            let service = service(
                Behavior::Probability(0.0),
                FeatureMode::Enforce,
                FeatureMode::Off,
                true,
            );
            let manager = default_mgr();
            manager.set_session_mode(Some(PermissionMode::Auto));
            manager.set_decisions(Some(service.decisions));
            let scopes = PermissionScopes::single(FIRST_COMMAND.into());
            let (sender, _events) = flume::unbounded();
            let sender = EventSender::new(sender, 0);
            let (trigger, cancel) = CancelToken::new();
            let mut enforcement =
                Box::pin(controlled_enforcement(&manager, &scopes, &sender, &cancel));
            assert!(future::poll_once(&mut enforcement).await.is_none());
            if cancelled {
                trigger.cancel();
            } else {
                manager.set_project(&PathBuf::from(SHELL_WORKDIR));
            }
            assert!(enforcement.await.is_err());
        });
    }

    #[test]
    fn escalated_auto_prompt_cannot_settle_itself() {
        smol::block_on(async {
            let service = service(
                Behavior::Probability(1.0),
                FeatureMode::Enforce,
                FeatureMode::Off,
                false,
            );
            let manager = default_mgr();
            manager.set_session_mode(Some(PermissionMode::Auto));
            manager.set_decisions(Some(service.decisions));
            let scopes = PermissionScopes::single(FIRST_COMMAND.into());
            let (sender, events) = flume::unbounded();
            let sender = EventSender::new(sender, 0);
            let cancel = CancelToken::none();
            let mut enforcement =
                Box::pin(controlled_enforcement(&manager, &scopes, &sender, &cancel));
            let prompt = future::race(
                async {
                    enforcement.as_mut().await.unwrap();
                    panic!("{PREMATURE_ADMISSION}");
                },
                events.recv_async(),
            )
            .await
            .unwrap();
            let AgentEvent::PermissionRequest(request) = prompt.event else {
                panic!("{PROMPT_MISSING}");
            };
            assert!(!request.presentation.advisories.is_empty());
            manager.set_session_mode(Some(PermissionMode::Auto));
            assert!(future::poll_once(&mut enforcement).await.is_none());
            assert_eq!(service.calls.load(Ordering::Relaxed), 1);
            assert!(manager.answer(CONTROLLED_REQUEST, PermissionAnswer::AllowOnce));
            assert!(enforcement.await.is_ok());
        });
    }

    #[test]
    fn late_advice_only_changes_presentation() {
        smol::block_on(async {
            let service = service(
                Behavior::Probability(1.0),
                FeatureMode::Off,
                FeatureMode::Advise,
                true,
            );
            let manager = default_mgr();
            manager.set_decisions(Some(service.decisions));
            let scopes = PermissionScopes::single(FIRST_COMMAND.into());
            let (sender, events) = flume::unbounded();
            let sender = EventSender::new(sender, 0);
            let cancel = CancelToken::none();
            let mut enforcement =
                Box::pin(controlled_enforcement(&manager, &scopes, &sender, &cancel));
            assert!(future::poll_once(&mut enforcement).await.is_none());
            let AgentEvent::PermissionRequest(original) = events.try_recv().unwrap().event else {
                panic!("{PROMPT_MISSING}");
            };
            assert!(original.presentation.advisories.is_empty());
            service.started.recv_async().await.unwrap();
            service.release.send(()).unwrap();
            let updated = future::race(
                async {
                    enforcement.as_mut().await.unwrap();
                    panic!("{PREMATURE_ADMISSION}");
                },
                events.recv_async(),
            )
            .await
            .unwrap();
            let AgentEvent::PermissionRequestUpdated(mut updated) = updated.event else {
                panic!("{UPDATE_MISSING}");
            };
            assert_eq!(
                updated.presentation.advisories.len(),
                EngineFlag::ALL
                    .iter()
                    .filter(|flag| **flag != EngineFlag::WritesProjectFiles)
                    .count()
            );
            assert_eq!(
                manager.pending_request(CONTROLLED_REQUEST).unwrap(),
                *updated
            );
            updated.presentation.advisories.clear();
            assert_eq!(updated, original);
            assert!(manager.answer(CONTROLLED_REQUEST, PermissionAnswer::Deny));
            assert!(enforcement.await.is_err());
        });
    }

    #[test_case(false; "answer_does_not_wait")]
    #[test_case(true; "yolo_does_not_wait")]
    fn pending_advice_never_delays_answer_or_yolo(yolo: bool) {
        smol::block_on(async {
            let service = service(
                Behavior::Pending,
                FeatureMode::Off,
                FeatureMode::Advise,
                true,
            );
            let manager = default_mgr();
            manager.set_decisions(Some(service.decisions));
            let scopes = PermissionScopes::single(FIRST_COMMAND.into());
            let (sender, events) = flume::unbounded();
            let sender = EventSender::new(sender, 0);
            let cancel = CancelToken::none();
            let mut enforcement =
                Box::pin(controlled_enforcement(&manager, &scopes, &sender, &cancel));
            assert!(future::poll_once(&mut enforcement).await.is_none());
            let _ = events.try_recv().unwrap();
            service.started.recv_async().await.unwrap();
            if yolo {
                manager.toggle_yolo();
            } else {
                assert!(manager.answer(CONTROLLED_REQUEST, PermissionAnswer::AllowOnce));
            }
            assert!(future::poll_once(&mut enforcement).await.unwrap().is_ok());
            assert!(
                events
                    .try_iter()
                    .all(|event| !matches!(event.event, AgentEvent::PermissionRequestUpdated(_)))
            );
        });
    }

    #[test]
    fn only_fixed_finite_advisory_flags_are_presented() {
        let decision = PermissionDecision {
            action: Some(PermissionAction::Advice(vec![
                PermissionFlag {
                    flag: UNKNOWN_FLAG.into(),
                    probability: 1.0,
                },
                PermissionFlag {
                    flag: EngineFlag::Deletes.name().into(),
                    probability: f64::NAN,
                },
                PermissionFlag {
                    flag: EngineFlag::Uploads.name().into(),
                    probability: 1.0,
                },
            ])),
            evaluation: DecisionOutcome {
                result: Err(DecisionError::Unreachable),
                latency_ms: 0,
                receipt: None,
            },
        };
        let warnings = advisories(&decision);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].flag, EngineFlag::Uploads);
    }

    #[test]
    fn oversized_screening_state_denies_headless_without_raw_fallback() {
        smol::block_on(async {
            let service = service(
                Behavior::Probability(0.0),
                FeatureMode::Enforce,
                FeatureMode::Off,
                false,
            );
            let manager = default_mgr();
            manager.set_session_mode(Some(PermissionMode::Auto));
            manager.set_decisions(Some(service.decisions));
            let scopes = PermissionScopes::single(FIRST_COMMAND.into());
            let (sender, _events) = flume::unbounded();
            let sender = EventSender::new(sender, 0);
            assert!(manager.enforce(
                &ToolKey::native("bash"), &scopes,
                &json!({"command": FIRST_COMMAND, "payload": "x".repeat(OVERSIZED_STATE_BYTES)}),
                &sender, None, CONTROLLED_REQUEST, &CancelToken::none(), None,
            ).await.is_err());
            assert_eq!(service.calls.load(Ordering::Relaxed), 0);
        });
    }

    #[test]
    fn state_drops_command_resources_and_flattens_kind() {
        let mut request = shell_request(&[FIRST_COMMAND], workcell_shell_subject());
        let written = filesystem_permission_resource(
            PermissionResourceKind::File,
            Path::new(WRITTEN_FILE),
            PermissionResourceAccess::Write,
            Path::new(SHELL_WORKDIR),
        );
        request.resources.push(written.clone());
        assert_eq!(
            state(&request),
            json!({
                "tool": request.tool.to_string(),
                "input": request.input,
                "workdir": SHELL_WORKDIR,
                "resources": [{"kind": FILE_NOUN, "value": written.value, "access": WRITE_ACCESS}],
            })
        );
    }

    #[test]
    fn session_roots_isolate_decisions_and_inherited_forks_share_them() {
        let service = service(
            Behavior::Probability(0.0),
            FeatureMode::Off,
            FeatureMode::Off,
            false,
        );
        let manager = default_mgr();
        manager.set_decisions(Some(service.decisions));
        let inherited = manager.fork();
        manager.decisions().unwrap().mark_tainted();
        let session = CaudraId::generate();
        let other_root = manager.fork_session(session);
        let next_root = other_root.fork_session(CaudraId::generate());
        assert_eq!(other_root.decisions().unwrap().session(), Some(session));
        assert_eq!(
            inherited.decisions().unwrap().session(),
            manager.decisions().unwrap().session()
        );
        assert!(inherited.decisions().unwrap().is_tainted());
        assert!(!other_root.decisions().unwrap().is_tainted());
        other_root.decisions().unwrap().mark_tainted();
        assert!(!next_root.decisions().unwrap().is_tainted());
        manager.set_decisions(None);
        assert!(inherited.decisions().is_none());
        assert!(other_root.decisions().unwrap().is_tainted());
        other_root.set_decisions(None);
        assert!(next_root.decisions().is_some());
    }

    #[test_case(|manager| manager.set_decisions(None); "decision_service")]
    #[test_case(|manager| manager.set_project(Path::new(SHELL_WORKDIR)); "project")]
    #[test_case(|manager| manager.set_project_with_config(Path::new(SHELL_WORKDIR), PermissionsConfig::default()); "project_config")]
    fn passive_decision_stops_immediately_on_revision_change(revoke: fn(&PermissionManager)) {
        smol::block_on(async {
            let manager = default_mgr();
            let mut pending = Box::pin(manager.run_passive_decision(future::pending::<()>()));
            assert!(future::poll_once(&mut pending).await.is_none());
            revoke(&manager);
            assert_eq!(future::poll_once(&mut pending).await, Some(None));
        });
    }

    #[test_case(|manager| manager.notify_policy_changed(CONTROLLED_REQUEST); "policy_change")]
    #[test_case(|manager| manager.set_session_mode(Some(PermissionMode::Ask)); "ask")]
    #[test_case(|manager| manager.set_session_mode(Some(PermissionMode::Auto)); "auto")]
    #[test_case(|manager| manager.set_session_mode(Some(PermissionMode::Yolo)); "yolo")]
    #[test_case(|manager| { manager.toggle_yolo(); manager.toggle_yolo(); }; "yolo_round_trip")]
    fn passive_decision_outlives_policy_changes(change: fn(&PermissionManager)) {
        smol::block_on(async {
            let manager = default_mgr();
            let (finish, finished) = flume::bounded(1);
            let mut running = Box::pin(
                manager.run_passive_decision(async { finished.recv_async().await.is_ok() }),
            );
            assert!(future::poll_once(&mut running).await.is_none());
            change(&manager);
            assert!(future::poll_once(&mut running).await.is_none());
            finish.send(()).unwrap();
            assert_eq!(future::poll_once(&mut running).await, Some(Some(true)));
        });
    }

    #[test_case(PermissionMode::Ask; "ask")]
    #[test_case(PermissionMode::Auto; "auto")]
    #[test_case(PermissionMode::Yolo; "yolo")]
    fn passive_decision_starts_in_all_modes(mode: PermissionMode) {
        smol::block_on(async {
            let manager = default_mgr();
            manager.set_session_mode(Some(mode));
            let called = AtomicUsize::new(0);
            let outcome = manager
                .run_passive_decision(async {
                    called.fetch_add(1, Ordering::Relaxed);
                    true
                })
                .await;
            assert_eq!(outcome, Some(true));
            assert_eq!(called.load(Ordering::Relaxed), 1);
        });
    }

    #[test_case(|manager| manager.set_decisions(None); "service")]
    #[test_case(|manager| manager.set_project(Path::new(SHELL_WORKDIR)); "project")]
    fn passive_decision_rechecks_revision_when_work_finishes(change: fn(&PermissionManager)) {
        smol::block_on(async {
            let manager = default_mgr();
            assert_eq!(
                manager
                    .run_passive_decision(async {
                        change(&manager);
                        true
                    })
                    .await,
                None
            );
        });
    }

    #[test_case(|manager| manager.notify_policy_changed(CONTROLLED_REQUEST); "policy")]
    #[test_case(|manager| manager.set_session_mode(Some(PermissionMode::Ask)); "ask")]
    #[test_case(|manager| manager.set_session_mode(Some(PermissionMode::Auto)); "auto")]
    #[test_case(|manager| manager.set_session_mode(Some(PermissionMode::Yolo)); "yolo")]
    #[test_case(|manager| { manager.toggle_yolo(); manager.toggle_yolo(); }; "yolo_round_trip")]
    #[test_case(|manager| manager.set_decisions(None); "service")]
    #[test_case(|manager| manager.set_project(Path::new(SHELL_WORKDIR)); "project")]
    fn permission_decision_stops_on_policy_changes(change: fn(&PermissionManager)) {
        smol::block_on(async {
            let manager = default_mgr();
            let mut pending = Box::pin(manager.run_permission_decision(future::pending::<()>()));
            assert!(future::poll_once(&mut pending).await.is_none());
            change(&manager);
            assert_eq!(future::poll_once(&mut pending).await, Some(None));
        });
    }

    #[test_case(false, Some(true); "ask")]
    #[test_case(true, None; "yolo")]
    fn permission_decision_never_starts_in_yolo(yolo: bool, expected: Option<bool>) {
        smol::block_on(async {
            let manager = default_mgr();
            manager.set_session_mode(Some(PermissionMode::from(yolo)));
            let called = AtomicUsize::new(0);
            let outcome = manager
                .run_permission_decision(async {
                    called.fetch_add(1, Ordering::Relaxed);
                    true
                })
                .await;
            assert_eq!(outcome, expected);
            assert_eq!(called.load(Ordering::Relaxed), usize::from(!yolo));
        });
    }

    #[test_case(|manager| manager.set_session_mode(Some(PermissionMode::Yolo)); "yolo")]
    #[test_case(|manager| { manager.toggle_yolo(); manager.toggle_yolo(); }; "yolo_round_trip")]
    #[test_case(|manager| manager.set_decisions(None); "service")]
    fn permission_decision_rechecks_revision_when_work_finishes(change: fn(&PermissionManager)) {
        smol::block_on(async {
            let manager = default_mgr();
            assert_eq!(
                manager
                    .run_permission_decision(async {
                        change(&manager);
                        true
                    })
                    .await,
                None
            );
        });
    }

    fn advisory_service(behavior: Behavior, delayed: bool) -> Service {
        let mut config = DecisionsConfig {
            base_url: Some(BASE_URL.parse().unwrap()),
            timeout_ms: TIMEOUT_MS,
            ..Default::default()
        };
        config.features.permission_advice = FeatureMode::Advise;
        config.features.shell_effect = FeatureMode::Advise;
        configured_service(behavior, config, delayed)
    }

    #[test_case(None, false; "permission_yolo")]
    #[test_case(Some(false), false; "shell_yolo")]
    #[test_case(Some(true), false; "read_only_shell_yolo")]
    #[test_case(None, true; "permission_yolo_round_trip")]
    #[test_case(Some(false), true; "shell_yolo_round_trip")]
    #[test_case(Some(true), true; "read_only_shell_yolo_round_trip")]
    fn advisory_guard_rejects_mode_changes_before_first_poll(
        shell: Option<bool>,
        round_trip: bool,
    ) {
        smol::block_on(async {
            let service = advisory_service(Behavior::Probability(1.0), false);
            let manager = default_mgr();
            manager.set_decisions(Some(service.decisions.clone()));
            let request = shell_request(&[FIRST_COMMAND], workcell_shell_subject());
            let input = state(&request);
            let context = manager.decision_context(Value::Null);
            let evaluation_service = service.decisions.clone();
            let guarded = manager.run_permission_decision(async move {
                if let Some(read_only) = shell {
                    evaluation_service
                        .shell_effect(FIRST_COMMAND, read_only, true, &context)
                        .await
                } else {
                    evaluation_service
                        .permission(PermissionPurpose::Advice, &input, &context)
                        .await
                }
            });
            manager.toggle_yolo();
            if round_trip {
                manager.toggle_yolo();
            }
            assert!(smol::spawn(guarded).await.is_none());
            assert_eq!(service.calls.load(Ordering::Relaxed), 0);
        });
    }

    #[test_case(false; "permission")]
    #[test_case(true; "shell")]
    fn spawned_advice_is_cancelled_when_entering_yolo(shell: bool) {
        smol::block_on(async {
            let service = advisory_service(Behavior::Pending, true);
            let manager = default_mgr();
            manager.set_decisions(Some(service.decisions.clone()));
            let request = shell_request(&[FIRST_COMMAND], workcell_shell_subject());
            let task = if shell {
                manager.shell_effect_advice(&request, true)
            } else {
                manager.permission_advice(&request)
            }
            .unwrap();
            service.started.recv_async().await.unwrap();
            manager.toggle_yolo();
            assert!(task.await.1.is_none());
            assert_eq!(service.calls.load(Ordering::Relaxed), 1);
        });
    }

    #[test_case(true, Some(0.8), false, true; "calibrated_plan_warning")]
    #[test_case(false, Some(0.8), false, false; "build_no_warning")]
    #[test_case(true, None, false, false; "uncalibrated_no_warning")]
    #[test_case(true, Some(0.8), true, false; "yolo_no_call")]
    fn shell_effect_is_plan_advice_only(
        plan: bool,
        threshold: Option<f64>,
        yolo: bool,
        warns: bool,
    ) {
        smol::block_on(async {
            let mut config = DecisionsConfig {
                base_url: Some(BASE_URL.parse().unwrap()),
                timeout_ms: TIMEOUT_MS,
                ..Default::default()
            };
            config.features.shell_effect = FeatureMode::Advise;
            config.thresholds.shell_writes = threshold;
            let service = configured_service(Behavior::Probability(1.0), config, false);
            let manager = default_mgr();
            manager.set_decisions(Some(service.decisions));
            if yolo {
                manager.toggle_yolo();
            }
            let request = shell_request(&[FIRST_COMMAND], workcell_shell_subject());
            let outcome = match manager.shell_effect_advice(&request, plan) {
                Some(task) => task.await.1,
                None => None,
            };
            assert_eq!(
                outcome
                    .as_ref()
                    .is_some_and(|decision| !advisories(decision).is_empty()),
                warns
            );
            assert!(!outcome.as_ref().is_some_and(|decision| matches!(
                decision.action,
                Some(PermissionAction::Escalate(_))
            )));
            assert_eq!(service.calls.load(Ordering::Relaxed), usize::from(!yolo));
            assert_eq!(
                enforce_shell_without_prompt(&manager, &[FIRST_COMMAND], false)
                    .await
                    .is_ok(),
                yolo
            );
        });
    }

    #[test]
    fn shell_effect_and_permission_flags_merge_without_changing_authority() {
        smol::block_on(async {
            let mut config = DecisionsConfig {
                base_url: Some(BASE_URL.parse().unwrap()),
                timeout_ms: TIMEOUT_MS,
                ..Default::default()
            };
            config.features.permission_advice = FeatureMode::Advise;
            config.features.shell_effect = FeatureMode::Advise;
            config.thresholds.shell_writes = Some(0.8);
            let service = configured_service(Behavior::Probability(1.0), config, false);
            let manager = default_mgr();
            manager.set_decisions(Some(service.decisions));
            let mut scopes = PermissionScopes::single(FIRST_COMMAND.into());
            scopes.plan_scoped = true;
            let (sender, events) = flume::unbounded();
            let sender = EventSender::new(sender, 0);
            let cancel = CancelToken::none();
            let mut enforcement =
                Box::pin(controlled_enforcement(&manager, &scopes, &sender, &cancel));
            assert!(future::poll_once(&mut enforcement).await.is_none());
            let AgentEvent::PermissionRequest(original) = events.try_recv().unwrap().event else {
                panic!("{PROMPT_MISSING}");
            };
            let mut updated = future::race(
                async {
                    enforcement.as_mut().await.unwrap();
                    panic!("{PREMATURE_ADMISSION}");
                },
                async {
                    loop {
                        if let AgentEvent::PermissionRequestUpdated(request) =
                            events.recv_async().await.unwrap().event
                            && request.presentation.advisories.len() == EngineFlag::ALL.len()
                        {
                            break request;
                        }
                    }
                },
            )
            .await;
            assert!(
                updated
                    .presentation
                    .advisories
                    .iter()
                    .any(|advisory| advisory.flag == EngineFlag::WritesProjectFiles)
            );
            updated.presentation.advisories.clear();
            assert_eq!(updated, original);
            assert!(manager.answer(CONTROLLED_REQUEST, PermissionAnswer::AllowOnce));
            assert!(enforcement.await.is_ok());
            assert_eq!(service.calls.load(Ordering::Relaxed), 2);
        });
    }

    fn logged_service() -> Service {
        let mut config = DecisionsConfig {
            base_url: Some(BASE_URL.parse().unwrap()),
            timeout_ms: TIMEOUT_MS,
            log: true,
            ..Default::default()
        };
        config.features.permission_advice = FeatureMode::Advise;
        config.features.auto_screening = FeatureMode::Enforce;
        configured_service(Behavior::Probability(1.0), config, false)
    }

    async fn labeled_record(service: &Service, receipt: DecisionReceipt) -> Value {
        label_answer(
            vec![(service.decisions.clone(), receipt)],
            None,
            &PermissionAnswer::Deny,
            0,
        )
        .unwrap()
        .await;
        let log =
            DecisionLog::open_existing(&StateDir::from_path(service._state.path().to_path_buf()))
                .unwrap()
                .unwrap();
        let mut exported = Vec::new();
        assert_eq!(log.export_jsonl(&mut exported, None).unwrap(), 1);
        serde_json::from_slice(&exported).unwrap()
    }

    #[test_case(false, true, false, DecisionEffect::Advised; "delivered_warning")]
    #[test_case(true, true, false, DecisionEffect::None; "stale_warning")]
    #[test_case(false, false, false, DecisionEffect::None; "undelivered_warning")]
    #[test_case(false, true, true, DecisionEffect::Escalated; "never_downgrade_escalation")]
    fn warning_effect_requires_actual_current_delivery(
        stale: bool,
        deliver: bool,
        escalated: bool,
        expected: DecisionEffect,
    ) {
        smol::block_on(async {
            let service = logged_service();
            let manager = default_mgr();
            let scopes = PermissionScopes::single(FIRST_COMMAND.into());
            let (sender, events) = flume::unbounded();
            let sender = EventSender::new(sender, 0);
            let cancel = CancelToken::none();
            let mut enforcement =
                Box::pin(controlled_enforcement(&manager, &scopes, &sender, &cancel));
            assert!(future::poll_once(&mut enforcement).await.is_none());
            let AgentEvent::PermissionRequest(mut request) = events.try_recv().unwrap().event
            else {
                panic!("{PROMPT_MISSING}");
            };
            let events = deliver.then_some(events);
            let decision = service
                .decisions
                .permission(
                    PermissionPurpose::Advice,
                    &state(&request),
                    &DecisionContext::default(),
                )
                .await
                .unwrap();
            let receipt = decision.evaluation.receipt.clone().unwrap();
            if escalated {
                service
                    .decisions
                    .record_effect(&receipt, DecisionEffect::Escalated)
                    .await
                    .unwrap();
            }
            let revision = manager.broker.revision.load(Ordering::Acquire);
            if stale {
                manager.set_session_mode(Some(PermissionMode::Ask));
            }
            let answer_sender = manager.pending()[&manager.id][CONTROLLED_REQUEST]
                .sender
                .clone();
            let registration = PendingRegistration {
                manager: &manager,
                request_id: CONTROLLED_REQUEST,
                sender: answer_sender,
                event_tx: &sender,
            };
            let effect =
                registration.annotate(&mut request, &decision, revision, &service.decisions);
            assert_eq!(effect.is_some(), !stale && deliver);
            if let Some(effect) = effect {
                effect.await;
            }
            if let Some(events) = events {
                assert_eq!(
                    events
                        .try_iter()
                        .filter(|event| matches!(
                            event.event,
                            AgentEvent::PermissionRequestUpdated(_)
                        ))
                        .count(),
                    usize::from(!stale)
                );
            }
            assert!(manager.answer(CONTROLLED_REQUEST, PermissionAnswer::Deny));
            assert!(enforcement.await.is_err());
            let record = labeled_record(&service, receipt).await;
            assert_eq!(
                record["caudra"]["effect"],
                serde_json::to_value(expected).unwrap()
            );
        });
    }

    #[test_case(false, DecisionEffect::Escalated; "selected_escalation")]
    #[test_case(true, DecisionEffect::None; "stale_escalation")]
    fn escalation_effect_obeys_the_revision_fence(stale: bool, expected: DecisionEffect) {
        smol::block_on(async {
            let service = logged_service();
            let manager = default_mgr();
            manager.set_session_mode(Some(PermissionMode::Auto));
            manager.set_decisions(Some(service.decisions.clone()));
            let request = shell_request(&[FIRST_COMMAND], workcell_shell_subject());
            let context = EvaluationContext {
                revision: *manager.context_revision.read().unwrap(),
                plan_scoped: false,
                builtin_allows: true,
                force_prompt: false,
                forced: false,
                exact_plan: None,
            };
            let screened = manager
                .screen_auto_candidate(&request, &context, &CancelToken::none())
                .await
                .unwrap();
            let receipt = screened
                .decision
                .as_ref()
                .unwrap()
                .1
                .evaluation
                .receipt
                .clone()
                .unwrap();
            assert!(!screened.approved);
            if stale {
                manager.set_session_mode(Some(PermissionMode::Ask));
            }
            let effect = manager.record_escalation(screened.escalation, &context);
            assert_eq!(effect.is_some(), !stale);
            if let Some(effect) = effect {
                effect.await;
            }
            let record = labeled_record(&service, receipt).await;
            assert_eq!(
                record["caudra"]["effect"],
                serde_json::to_value(expected).unwrap()
            );
        });
    }

    #[test_case(PermissionAnswer::AllowOnce, true; "approval")]
    #[test_case(PermissionAnswer::Deny, false; "denial")]
    fn explicit_answers_attach_only_user_approval_labels(answer: PermissionAnswer, approves: bool) {
        smol::block_on(async {
            let mut config = DecisionsConfig {
                base_url: Some(BASE_URL.parse().unwrap()),
                timeout_ms: TIMEOUT_MS,
                log: true,
                ..Default::default()
            };
            config.features.permission_advice = FeatureMode::Advise;
            let service = configured_service(Behavior::Probability(1.0), config, false);
            let decision = service
                .decisions
                .permission(
                    PermissionPurpose::Advice,
                    &json!({"command": FIRST_COMMAND}),
                    &DecisionContext::default(),
                )
                .await
                .unwrap();
            let receipt = decision.evaluation.receipt.unwrap();
            label_answer(vec![(service.decisions, receipt)], None, &answer, 0)
                .unwrap()
                .await;
            let log = DecisionLog::open_existing(&StateDir::from_path(
                service._state.path().to_path_buf(),
            ))
            .unwrap()
            .unwrap();
            let mut exported = Vec::new();
            assert_eq!(log.export_jsonl(&mut exported, None).unwrap(), 1);
            let row: Value = serde_json::from_slice(&exported).unwrap();
            assert_eq!(row["expected"], json!({"user_approves": approves}));
        });
    }
}
