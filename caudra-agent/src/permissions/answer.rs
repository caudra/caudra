use super::COMMAND_TEMPLATE_PREFIX;
use super::policy::validate_compiled_templates;
use super::{
    PermissionLifetime, PermissionManager, PermissionPolicyError, PermissionRequest,
    PermissionRowGrant, PermissionRuleRecord, StructuredPermissionEffect, StructuredPermissionRule,
    permission_rule_covers_request, permission_rule_intersects_request, review::review_for_rule,
};
use caudra_storage::id::CaudraId;
use caudra_storage::now_epoch;
use caudra_storage::permission_state::mutation::{
    PermissionMutation, PermissionOwner, prepare_mutation,
};
use std::path::Path;

pub const DEFAULT_DENY_GUIDANCE: &str =
    "Do not retry. Try a different approach or ask the user for guidance.";

/// Values for the `source` attribute on `caudra.tool_decision` events.
pub const DECISION_SOURCE_RULE: &str = "rule";

pub const DECISION_SOURCE_YOLO: &str = "yolo";
pub const DECISION_SOURCE_AUTO: &str = "auto";

pub const DECISION_SOURCE_USER_ONCE: &str = "user_once";

pub const DECISION_SOURCE_USER_SESSION: &str = "user_session";

pub const DECISION_SOURCE_USER_ALWAYS: &str = "user_always";

pub const DECISION_SOURCE_USER_ABORT: &str = "user_abort";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionAnswer {
    AllowOnce,
    AllowSession,
    AllowAlwaysLocal,
    AllowAlwaysGlobal,
    AllowOption {
        option_id: String,
        lifetime: PermissionLifetime,
    },
    /// One choice per resource of the request, composed into a single rule.
    /// A row left `None` is granted for this call only.
    AllowComposed {
        rows: Vec<Option<PermissionRowGrant>>,
        lifetime: PermissionLifetime,
    },
    Deny,
    DenyWithGuidance(String),
    DenyAlwaysLocal,
    DenyAlwaysGlobal,
}

impl PermissionAnswer {
    pub fn decision_source(&self) -> &'static str {
        match self {
            Self::AllowOnce
            | Self::Deny
            | Self::DenyWithGuidance(_)
            | Self::AllowOption {
                lifetime: PermissionLifetime::Once,
                ..
            }
            | Self::AllowComposed {
                lifetime: PermissionLifetime::Once,
                ..
            } => DECISION_SOURCE_USER_ONCE,
            Self::AllowSession
            | Self::AllowOption {
                lifetime: PermissionLifetime::Conversation,
                ..
            }
            | Self::AllowComposed {
                lifetime: PermissionLifetime::Conversation,
                ..
            } => DECISION_SOURCE_USER_SESSION,
            Self::AllowAlwaysLocal
            | Self::AllowAlwaysGlobal
            | Self::DenyAlwaysLocal
            | Self::DenyAlwaysGlobal
            | Self::AllowOption { .. }
            | Self::AllowComposed { .. } => DECISION_SOURCE_USER_ALWAYS,
        }
    }

    pub fn is_allow(&self) -> bool {
        matches!(
            self,
            Self::AllowOnce
                | Self::AllowSession
                | Self::AllowAlwaysLocal
                | Self::AllowAlwaysGlobal
                | Self::AllowOption { .. }
                | Self::AllowComposed { .. }
        )
    }

    pub fn encode(&self) -> String {
        match self {
            Self::AllowOnce => "allow".to_string(),
            Self::AllowSession => "allow_session".to_string(),
            Self::AllowAlwaysLocal => "allow_always_local".to_string(),
            Self::AllowAlwaysGlobal => "allow_always_global".to_string(),
            Self::AllowOption {
                option_id,
                lifetime,
            } => format!("allow_option:{}:{option_id}", lifetime_name(lifetime)),
            Self::AllowComposed { rows, lifetime } => format!(
                "allow_composed:{}:{}",
                lifetime_name(lifetime),
                serde_json::to_string(rows).unwrap_or_default()
            ),
            Self::Deny => "deny".to_string(),
            Self::DenyWithGuidance(g) => format!("deny:{g}"),
            Self::DenyAlwaysLocal => "deny_always_local".to_string(),
            Self::DenyAlwaysGlobal => "deny_always_global".to_string(),
        }
    }

    pub fn decode(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(Self::AllowOnce),
            "allow_session" => Some(Self::AllowSession),
            "allow_always_local" => Some(Self::AllowAlwaysLocal),
            "allow_always_global" => Some(Self::AllowAlwaysGlobal),
            "deny" => Some(Self::Deny),
            "deny_always_local" => Some(Self::DenyAlwaysLocal),
            "deny_always_global" => Some(Self::DenyAlwaysGlobal),
            _ if s.starts_with("allow_option:") => {
                let rest = s.strip_prefix("allow_option:")?;
                let (lifetime, option_id) = rest.split_once(':')?;
                Some(Self::AllowOption {
                    option_id: option_id.to_owned(),
                    lifetime: parse_lifetime(lifetime)?,
                })
            }
            _ if s.starts_with("allow_composed:") => {
                let rest = s.strip_prefix("allow_composed:")?;
                let (lifetime, rows) = rest.split_once(':')?;
                Some(Self::AllowComposed {
                    rows: serde_json::from_str(rows).ok()?,
                    lifetime: parse_lifetime(lifetime)?,
                })
            }
            _ if s.starts_with("deny:") => {
                let guidance = s.strip_prefix("deny:").unwrap();
                if guidance.is_empty() {
                    Some(Self::Deny)
                } else {
                    Some(Self::DenyWithGuidance(guidance.to_string()))
                }
            }
            _ => None,
        }
    }

    pub fn guidance(&self) -> Option<&str> {
        match self {
            Self::DenyWithGuidance(g) => Some(g),
            _ => None,
        }
    }
}

pub(super) fn lifetime_name(lifetime: &PermissionLifetime) -> &'static str {
    match lifetime {
        PermissionLifetime::Once => "once",
        PermissionLifetime::Conversation => "conversation",
        PermissionLifetime::Project => "project",
        PermissionLifetime::Global => "global",
    }
}

pub(super) fn parse_lifetime(value: &str) -> Option<PermissionLifetime> {
    match value {
        "once" => Some(PermissionLifetime::Once),
        "conversation" => Some(PermissionLifetime::Conversation),
        "project" => Some(PermissionLifetime::Project),
        "global" => Some(PermissionLifetime::Global),
        _ => None,
    }
}

impl PermissionManager {
    /// Files what the answer decided and reports the allows other pending
    /// prompts can be swept with. A composed answer files one rule per command,
    /// so each is listed and revoked on its own.
    pub(super) fn commit_structured_decision(
        &self,
        request: &PermissionRequest,
        answer: &PermissionAnswer,
        approved_project: Option<&Path>,
    ) -> Result<(), PermissionPolicyError> {
        let selects_template = match answer {
            PermissionAnswer::AllowOption { option_id, .. } => {
                option_id.starts_with(COMMAND_TEMPLATE_PREFIX)
            }
            PermissionAnswer::AllowComposed { rows, .. } => {
                rows.iter().flatten().any(|grant| match grant {
                    PermissionRowGrant::Pattern { .. } => true,
                    PermissionRowGrant::Offered(id) => id.starts_with(COMMAND_TEMPLATE_PREFIX),
                    PermissionRowGrant::Written(_) => false,
                })
            }
            _ => false,
        };
        let mut refreshed;
        let request = if selects_template {
            let mut current = request.clone();
            current.add_pattern_candidates(&self.pattern_candidates(), &[]);
            refreshed = request.clone();
            refreshed.options.retain(|offered| {
                !offered.id.starts_with(COMMAND_TEMPLATE_PREFIX)
                    || current
                        .options
                        .iter()
                        .any(|option| option.id == offered.id && option.rule == offered.rule)
            });
            &refreshed
        } else {
            request
        };
        let (option_id, lifetime) = match answer {
            PermissionAnswer::AllowOnce => ("allow_exact", PermissionLifetime::Once),
            PermissionAnswer::AllowSession => ("allow_exact", PermissionLifetime::Conversation),
            PermissionAnswer::AllowAlwaysLocal => ("allow_exact", PermissionLifetime::Project),
            PermissionAnswer::AllowAlwaysGlobal => ("allow_exact", PermissionLifetime::Global),
            PermissionAnswer::AllowOption {
                option_id,
                lifetime,
            } => (option_id.as_str(), lifetime.clone()),
            PermissionAnswer::AllowComposed { rows, lifetime } => {
                let rules = request
                    .composed_rules(rows, lifetime)
                    .map_err(|error| PermissionPolicyError(error.to_string()))?;
                return self.store_reusable_rules(request, rules, approved_project);
            }
            PermissionAnswer::DenyAlwaysLocal => ("deny_exact", PermissionLifetime::Project),
            PermissionAnswer::DenyAlwaysGlobal => ("deny_exact", PermissionLifetime::Global),
            PermissionAnswer::Deny | PermissionAnswer::DenyWithGuidance(_) => {
                return Ok(());
            }
        };
        let option = request
            .options
            .iter()
            .find(|option| option.id == option_id)
            .ok_or_else(|| {
                PermissionPolicyError(format!("request did not offer {option_id:?} authority"))
            })?;
        if !option.allowed_lifetimes.contains(&lifetime) {
            return Err(PermissionPolicyError(format!(
                "authority {option_id:?} does not allow {lifetime:?} lifetime"
            )));
        }
        let mut rule = option.rule.clone();
        rule.lifetime = lifetime;
        let covers = match rule.effect {
            StructuredPermissionEffect::Allow => permission_rule_covers_request(&rule, request),
            StructuredPermissionEffect::Deny | StructuredPermissionEffect::Ask => {
                permission_rule_intersects_request(&rule, request)
            }
        };
        if !covers {
            return Err(PermissionPolicyError(format!(
                "authority {option_id:?} does not cover the pending request"
            )));
        }
        self.store_reusable_rules(request, vec![rule], approved_project)
    }

    pub(super) fn store_reusable_rules(
        &self,
        request: &PermissionRequest,
        rules: Vec<StructuredPermissionRule>,
        approved_project: Option<&Path>,
    ) -> Result<(), PermissionPolicyError> {
        for rule in &rules {
            validate_compiled_templates(rule)?;
        }
        let rules: Vec<_> = rules
            .into_iter()
            .filter(|rule| rule.lifetime != PermissionLifetime::Once)
            .collect();
        let Some(first) = rules.first() else {
            return Ok(());
        };
        if rules.iter().any(|rule| rule.lifetime != first.lifetime) {
            return Err(PermissionPolicyError("mixed permission lifetimes".into()));
        }
        let _mutation = self
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if first.lifetime == PermissionLifetime::Conversation {
            let records = rules
                .iter()
                .map(|rule| {
                    PermissionRuleRecord::conversation_with_review(
                        rule.clone(),
                        Some(review_for_rule(request, rule)),
                    )
                    .map_err(|error| PermissionPolicyError(error.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            if let Some(publication) = self.publication() {
                let snapshot = publication
                    .snapshot()
                    .map_err(|error| PermissionPolicyError(error.to_string()))?;
                if snapshot.records != *self.structured_conversation_rules() {
                    return Err(PermissionPolicyError(
                        "conversation permission state changed".into(),
                    ));
                }
                let prepared = prepare_mutation(
                    vec![snapshot.clone()],
                    PermissionMutation::Create {
                        destination: snapshot.revision.owner,
                        records: records.into_boxed_slice(),
                    },
                )
                .map_err(|error| PermissionPolicyError(error.to_string()))?;
                self.commit_prepared_permission_mutation(&prepared)
                    .map_err(|error| PermissionPolicyError(error.to_string()))?;
            } else {
                self.structured_conversation_rules().extend(records);
            }
        } else {
            let project = if first.lifetime == PermissionLifetime::Project {
                Some(approved_project.map(Path::to_path_buf).ok_or_else(|| {
                    PermissionPolicyError("canonical project is unavailable".into())
                })?)
            } else {
                None
            };
            let records = rules
                .iter()
                .map(|rule| PermissionRuleRecord {
                    id: CaudraId::generate().to_string(),
                    project: project.clone(),
                    rule: rule.clone(),
                    review: Some(review_for_rule(request, rule)),
                    label: None,
                    replaces: None,
                    created_at: now_epoch(),
                    revoked_at: None,
                })
                .collect();
            let policy = self
                .policy
                .as_ref()
                .ok_or_else(|| PermissionPolicyError("persistent storage is disabled".into()))?;
            let mut policy = policy
                .policy
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let snapshot = policy
                .state()?
                .snapshot()
                .map_err(|error| PermissionPolicyError(error.to_string()))?;
            drop(policy);
            let prepared = prepare_mutation(
                vec![snapshot],
                PermissionMutation::Create {
                    destination: PermissionOwner::Persistent,
                    records,
                },
            )
            .map_err(|error| PermissionPolicyError(error.to_string()))?;
            self.commit_prepared_permission_mutation(&prepared)
                .map_err(|error| PermissionPolicyError(error.to_string()))?;
        }
        self.notify_policy_changed(&request.id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::PermissionRequest;

    use crate::AgentEvent;
    use crate::permissions::tests::{COMPOSED_REQUEST_ID, default_mgr, enforce_without_prompt};
    use crate::permissions::{PermissionAnswer, PermissionLifetime, PermissionRowGrant};
    use caudra_config::ToolKey;
    use std::sync::Arc;
    #[test]
    fn permission_answer_roundtrip() {
        for a in [
            PermissionAnswer::AllowOnce,
            PermissionAnswer::AllowSession,
            PermissionAnswer::AllowAlwaysLocal,
            PermissionAnswer::AllowOption {
                option_id: "allow_url_origin".into(),
                lifetime: PermissionLifetime::Project,
            },
            PermissionAnswer::AllowComposed {
                rows: vec![
                    Some(PermissionRowGrant::Offered("command_exact_0".into())),
                    Some(PermissionRowGrant::Written("git status *".into())),
                    None,
                ],
                lifetime: PermissionLifetime::Conversation,
            },
            PermissionAnswer::Deny,
            PermissionAnswer::DenyWithGuidance("hint".into()),
        ] {
            assert_eq!(PermissionAnswer::decode(&a.encode()), Some(a));
        }
    }

    /// The point of per-row scopes: one answer, one rule, and only the rows
    /// that asked to be remembered end up in it. The row that asked for nothing
    /// still runs, because the call proceeds on the answer and not on the rule.
    #[test]
    fn a_composed_answer_remembers_only_the_rows_that_asked_for_it() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let commands = ["git status --short", "cargo test"];
            let input = serde_json::json!({"command": commands.join(" && ")});
            let (event_tx, event_rx) = flume::unbounded::<crate::Envelope>();
            let (_legacy_tx, legacy_rx) = flume::unbounded();
            let legacy_rx = Arc::new(async_lock::Mutex::new(legacy_rx));
            let task = smol::spawn({
                let manager = Arc::clone(&manager);
                let input = input.clone();
                async move {
                    manager
                        .enforce(
                            &ToolKey::native("bash"),
                            &crate::tools::PermissionScopes {
                                scopes: commands.iter().map(|c| (*c).to_owned()).collect(),
                                force_prompt: false,
                                plan_scoped: false,
                            },
                            &input,
                            &crate::EventSender::new(event_tx, 0),
                            Some(&legacy_rx),
                            COMPOSED_REQUEST_ID,
                            &crate::CancelToken::none(),
                            None,
                        )
                        .await
                }
            });
            assert!(matches!(
                event_rx.recv_async().await.unwrap().event,
                AgentEvent::PermissionRequest(_)
            ));
            assert!(manager.answer(
                COMPOSED_REQUEST_ID,
                PermissionAnswer::AllowComposed {
                    rows: vec![
                        Some(PermissionRowGrant::Offered("command_pattern_0".into())),
                        None,
                    ],
                    lifetime: PermissionLifetime::Conversation,
                }
            ));
            assert!(task.await.is_ok());

            assert!(
                enforce_without_prompt(
                    &manager,
                    "git status --porcelain",
                    serde_json::json!({"command": "git status --porcelain"})
                )
                .await
                .is_ok()
            );
            assert!(
                enforce_without_prompt(
                    &manager,
                    "cargo test",
                    serde_json::json!({"command": "cargo test"})
                )
                .await
                .is_err()
            );
        });
    }
    #[test_case::test_case(PermissionLifetime::Conversation; "conversation_is_atomic")]
    #[test_case::test_case(PermissionLifetime::Project; "project_is_atomic")]
    fn invalid_second_grant_leaves_no_partial_authority(lifetime: PermissionLifetime) {
        let temp = tempfile::tempdir().unwrap();
        let manager = crate::permissions::tests::persistent_manager(
            crate::permissions::StateDir::from_path(temp.path().join("state")),
            temp.path(),
        );
        let request = PermissionRequest::from_legacy(
            "atomic".into(),
            ToolKey::native("bash"),
            vec!["cargo build".into()],
            serde_json::json!({"command": "cargo build"}),
            temp.path(),
            false,
        );
        let first = request.option_rule("allow_exact", lifetime).unwrap();
        let mut invalid = first.clone();
        invalid.arguments = crate::permissions::PermissionArgumentConstraint::Exact {
            digest: "invalid".into(),
        };
        assert!(
            manager
                .store_reusable_rules(&request, vec![first, invalid], Some(temp.path()))
                .is_err()
        );
        assert!(manager.structured_rule_inventory().unwrap().is_empty());
    }
}
