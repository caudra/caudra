//! What an engagement lacks, told once when it starts: the content screening
//! that keeps injected text from earning a grant, and a bound delegate model
//! that can be used. Neither stops autopilot from engaging.

use std::fmt;
use std::sync::Arc;

use caudra_config::ModelPolicy;
use caudra_providers::model_registry::{self, Binding};
use caudra_providers::provider::provider_for_slug;
use caudra_providers::{Model, ModelPurpose, Timeouts};
use serde::Serialize;

use super::AutopilotHandle;
use crate::decisions::{DecisionFeature, Decisions};
use crate::types::AutopilotEvent;

const INJECTION_RISK: &str = "autopilot may approve calls after a fetched page or a tool's output tries to instruct the agent";

/// Something an engagement runs without, with what the user can do about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutopilotCaveat {
    /// No decision endpoint is active, so nothing screens content.
    NoDecisionEndpoint,
    /// An endpoint is active, and content screening is off.
    ContentScreeningOff,
    /// The bound Autopilot model cannot be used, and autopilot never replaces it.
    DelegateUnavailable { model: String, reason: String },
}

impl fmt::Display for AutopilotCaveat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDecisionEndpoint => write!(
                f,
                "No decision endpoint is active, so {INJECTION_RISK}. Add one with /login or select one with `caudra decisions use <name>`, then turn on content screening in /decisions."
            ),
            Self::ContentScreeningOff => write!(
                f,
                "Content screening is off, so {INJECTION_RISK}. Turn it on in /decisions."
            ),
            Self::DelegateUnavailable { model, reason } => write!(
                f,
                "Autopilot's model {model} cannot be used, so everything it would decide is left to you: {reason}. Pick another with /autopilot model."
            ),
        }
    }
}

/// What the engagement running now lacks, as its events. Nothing once it ended
/// or another took its place while the delegate's model was checked.
pub async fn autopilot_caveats(
    autopilot: &AutopilotHandle,
    decisions: Option<Decisions>,
    chat_model: Arc<Model>,
    policy: Arc<ModelPolicy>,
) -> Vec<AutopilotEvent> {
    let Some(engagement) = running_engagement(autopilot) else {
        return Vec::new();
    };
    let screening = screening_caveat(decisions.as_ref());
    let delegate = delegate_caveat(chat_model, policy).await;
    if running_engagement(autopilot) != Some(engagement) {
        return Vec::new();
    }
    screening
        .into_iter()
        .chain(delegate)
        .map(|caveat| AutopilotEvent::Caveat { engagement, caveat })
        .collect()
}

pub fn running_engagement(autopilot: &AutopilotHandle) -> Option<u32> {
    autopilot.engagement().filter(|_| autopilot.is_engaged())
}

/// Whether content screening guards this session's grants.
fn screening_caveat(decisions: Option<&Decisions>) -> Option<AutopilotCaveat> {
    match decisions.filter(|decisions| decisions.endpoint().is_some()) {
        None => Some(AutopilotCaveat::NoDecisionEndpoint),
        Some(decisions) if !decisions.enabled(&DecisionFeature::ContentScreening) => {
            Some(AutopilotCaveat::ContentScreeningOff)
        }
        Some(_) => None,
    }
}

/// Whether the Autopilot job's bound model resolves and its provider signs in,
/// without a model call. An unbound job falls back to the chat model, so it
/// always has one.
async fn delegate_caveat(
    chat_model: Arc<Model>,
    policy: Arc<ModelPolicy>,
) -> Option<AutopilotCaveat> {
    let binding = model_registry::binding(ModelPurpose::Autopilot)?;
    smol::unblock(move || bound_caveat(&binding, &chat_model, &policy)).await
}

fn bound_caveat(
    binding: &Binding,
    chat_model: &Model,
    policy: &ModelPolicy,
) -> Option<AutopilotCaveat> {
    let model =
        match Model::resolve_binding(ModelPurpose::Autopilot, Some(binding), chat_model, policy) {
            Ok(model) => model,
            Err(error) => {
                return Some(AutopilotCaveat::DelegateUnavailable {
                    model: binding.to_string(),
                    reason: error.to_string(),
                });
            }
        };
    if model.provider == chat_model.provider {
        return None;
    }
    provider_for_slug(&model.provider, Timeouts::default())
        .err()
        .map(|error| AutopilotCaveat::DelegateUnavailable {
            model: model.spec(),
            reason: error.user_message(),
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use async_trait::async_trait;
    use caudra_config::ModelPolicy;
    use caudra_config::decisions::{
        DecisionEndpoint, DecisionProtocol, DecisionsConfig, FeatureMode,
    };
    use caudra_decision::{DecisionEngine, DecisionError, DecisionRequest, DecisionResponse};
    use caudra_providers::Model;
    use caudra_providers::model_registry::Binding;
    use caudra_storage::StateDir;
    use futures_lite::future;
    use test_case::test_case;

    use super::{
        AutopilotCaveat, AutopilotEvent, AutopilotHandle, Decisions, autopilot_caveats,
        bound_caveat, screening_caveat,
    };

    const BASE_URL: &str = "http://127.0.0.1:1";
    const CHAT_SPEC: &str = "anthropic/claude-opus-5-5";
    const EXCLUDED_SPEC: &str = "openai/gpt-6-astra";
    const EXCLUDED_PATTERN: &str = "openai/*";
    const FIRST_ENGAGEMENT: u32 = 1;

    struct UnusedEngine;

    #[async_trait]
    impl DecisionEngine for UnusedEngine {
        async fn decide(
            &self,
            _request: &DecisionRequest,
            _deadline: Instant,
        ) -> Result<DecisionResponse, DecisionError> {
            future::pending().await
        }
    }

    fn decisions(content_screening: FeatureMode) -> (tempfile::TempDir, Decisions) {
        let state = tempfile::tempdir().unwrap();
        let mut config = DecisionsConfig {
            endpoint: Some(DecisionEndpoint::new(
                DecisionProtocol::TypeSafe.as_str(),
                DecisionProtocol::TypeSafe,
                BASE_URL.parse().unwrap(),
            )),
            ..Default::default()
        };
        config.features.content_screening = content_screening;
        let decisions = Decisions::with_engine(
            config,
            &StateDir::from_path(state.path().to_path_buf()),
            UnusedEngine,
        )
        .unwrap();
        (state, decisions)
    }

    fn caveats(autopilot: &AutopilotHandle) -> Vec<AutopilotEvent> {
        future::block_on(autopilot_caveats(
            autopilot,
            None,
            Arc::new(Model::from_spec(CHAT_SPEC).unwrap()),
            Arc::new(ModelPolicy::default()),
        ))
    }

    #[test]
    fn an_engagement_hears_what_it_runs_without() {
        let autopilot = AutopilotHandle::default();
        let engaged = autopilot.engage(false, None);
        assert!(engaged.is_some());

        assert_eq!(
            caveats(&autopilot),
            [AutopilotEvent::Caveat {
                engagement: FIRST_ENGAGEMENT,
                caveat: AutopilotCaveat::NoDecisionEndpoint,
            }]
        );
    }

    #[test]
    fn a_session_not_engaged_hears_nothing() {
        assert_eq!(caveats(&AutopilotHandle::default()), []);
    }

    #[test]
    fn without_an_endpoint_nothing_screens() {
        assert_eq!(
            screening_caveat(None),
            Some(AutopilotCaveat::NoDecisionEndpoint)
        );
    }

    #[test]
    fn a_decisions_service_without_an_endpoint_screens_nothing() {
        let state = tempfile::tempdir().unwrap();
        let mut config = DecisionsConfig::default();
        config.features.content_screening = FeatureMode::Enforce;
        let decisions =
            Decisions::new(config, &StateDir::from_path(state.path().to_path_buf())).unwrap();

        assert_eq!(
            screening_caveat(Some(&decisions)),
            Some(AutopilotCaveat::NoDecisionEndpoint)
        );
    }

    #[test_case(FeatureMode::Off, Some(AutopilotCaveat::ContentScreeningOff); "off")]
    #[test_case(FeatureMode::Advise, None; "advise")]
    #[test_case(FeatureMode::Enforce, None; "enforce")]
    fn an_endpoint_screens_only_with_content_screening_on(
        mode: FeatureMode,
        expected: Option<AutopilotCaveat>,
    ) {
        let (_state, decisions) = decisions(mode);
        assert_eq!(screening_caveat(Some(&decisions)), expected);
    }

    #[test]
    fn a_bound_model_the_policy_refuses_is_unavailable() {
        let policy = ModelPolicy::new(&[], &[EXCLUDED_PATTERN.to_owned()]).unwrap();
        let chat = Model::from_spec(CHAT_SPEC).unwrap();

        let caveat = bound_caveat(&Binding::Exact(EXCLUDED_SPEC.into()), &chat, &policy);

        assert!(matches!(
            caveat,
            Some(AutopilotCaveat::DelegateUnavailable { model, .. }) if model == EXCLUDED_SPEC
        ));
    }

    #[test]
    fn a_bound_model_on_the_chat_provider_is_usable() {
        let chat = Model::from_spec(CHAT_SPEC).unwrap();

        let caveat = bound_caveat(
            &Binding::Exact(CHAT_SPEC.into()),
            &chat,
            &ModelPolicy::default(),
        );

        assert_eq!(caveat, None);
    }
}
