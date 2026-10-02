use super::COMMAND_TEMPLATE_PREFIX;
use super::policy::validate_compiled_templates;
use super::{
    ComposedRow, PermissionLifetime, PermissionManager, PermissionPolicyError, PermissionRequest,
    PermissionRowGrant, PermissionRuleRecord, StructuredPermissionEffect, StructuredPermissionRule,
    permission_rule_covers_request, permission_rule_intersects_request, review::review_for_rule,
};
use caudra_storage::id::CaudraId;
use caudra_storage::now_epoch;
use caudra_storage::permission_state::mutation::{
    PermissionMutation, PermissionOwner, PermissionRecordIdentity, PermissionSnapshot,
    prepare_mutations,
};
use std::fmt::Display;
use std::path::Path;
use tracing::warn;

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
    /// One choice per resource of the request, each remembered as its own rule
    /// for its own lifetime. A row left `None` is granted for this call only.
    AllowComposed {
        rows: Vec<Option<ComposedRow>>,
    },
    Deny,
    DenyWithGuidance(String),
    DenyAlwaysLocal,
    DenyAlwaysGlobal,
}

impl PermissionAnswer {
    pub fn decision_source(&self) -> &'static str {
        let lifetime = match self {
            Self::AllowOnce | Self::Deny | Self::DenyWithGuidance(_) => PermissionLifetime::Once,
            Self::AllowSession => PermissionLifetime::Conversation,
            Self::AllowAlwaysLocal | Self::DenyAlwaysLocal => PermissionLifetime::Project,
            Self::AllowAlwaysGlobal | Self::DenyAlwaysGlobal => PermissionLifetime::Global,
            Self::AllowOption { lifetime, .. } => lifetime.clone(),
            Self::AllowComposed { rows } => ComposedRow::longest(rows),
        };
        match lifetime {
            PermissionLifetime::Once => DECISION_SOURCE_USER_ONCE,
            PermissionLifetime::Conversation => DECISION_SOURCE_USER_SESSION,
            PermissionLifetime::Project | PermissionLifetime::Global => DECISION_SOURCE_USER_ALWAYS,
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
            Self::AllowComposed { rows } => format!(
                "allow_composed:{}",
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
            _ if s.starts_with("allow_composed:") => Some(Self::AllowComposed {
                rows: serde_json::from_str(s.strip_prefix("allow_composed:")?).ok()?,
            }),
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
            PermissionAnswer::AllowComposed { rows } => {
                rows.iter().flatten().any(|row| match &row.grant {
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
            PermissionAnswer::AllowComposed { rows } => {
                let rules = request
                    .composed_rules(rows)
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

    /// Files each rule under its lifetime's owner: conversation rules with the
    /// conversation, project and global rules in persistent storage.
    pub(super) fn store_reusable_rules(
        &self,
        request: &PermissionRequest,
        rules: Vec<StructuredPermissionRule>,
        approved_project: Option<&Path>,
    ) -> Result<(), PermissionPolicyError> {
        for rule in &rules {
            validate_compiled_templates(rule)?;
        }
        let (conversation, persistent): (Vec<_>, Vec<_>) = rules
            .into_iter()
            .filter(|rule| rule.lifetime != PermissionLifetime::Once)
            .partition(|rule| rule.lifetime == PermissionLifetime::Conversation);
        if conversation.is_empty() && persistent.is_empty() {
            return Ok(());
        }
        let _mutation = self
            .broker
            .mutation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let conversation = conversation
            .into_iter()
            .map(|rule| {
                let review = review_for_rule(request, &rule);
                PermissionRuleRecord::conversation_with_review(rule, Some(review))
                    .map_err(policy_error)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let persistent = persistent
            .into_iter()
            .map(|rule| {
                let project = match rule.lifetime {
                    PermissionLifetime::Project => {
                        Some(approved_project.map(Path::to_path_buf).ok_or_else(|| {
                            PermissionPolicyError("canonical project is unavailable".into())
                        })?)
                    }
                    _ => None,
                };
                Ok(PermissionRuleRecord {
                    id: CaudraId::generate().to_string(),
                    project,
                    review: Some(review_for_rule(request, &rule)),
                    rule,
                    label: None,
                    replaces: None,
                    created_at: now_epoch(),
                    revoked_at: None,
                })
            })
            .collect::<Result<Vec<_>, PermissionPolicyError>>()?;
        self.file_records(conversation, persistent)?;
        self.notify_policy_changed(&request.id);
        Ok(())
    }

    /// Commits both owners' new records so that an answer never half-applies.
    ///
    /// When both owners live in one database, which is every normal run, the
    /// records land in one transaction. Split databases commit the persistent
    /// records first, because other processes contend there, and take them back
    /// if the conversation records then fail. Without a conversation publisher
    /// the conversation records stay in memory.
    fn file_records(
        &self,
        conversation: Vec<PermissionRuleRecord>,
        persistent: Vec<PermissionRuleRecord>,
    ) -> Result<(), PermissionPolicyError> {
        let conversation_snapshot = self
            .publication()
            .filter(|_| !conversation.is_empty())
            .map(|publication| {
                let snapshot = publication.snapshot().map_err(policy_error)?;
                if snapshot.records != *self.structured_conversation_rules() {
                    return Err(PermissionPolicyError(
                        "conversation permission state changed".into(),
                    ));
                }
                Ok(snapshot)
            })
            .transpose()?;
        let persistent_snapshot = (!persistent.is_empty())
            .then(|| self.persistent_snapshot())
            .transpose()?;
        let create = |snapshot: &PermissionSnapshot, records: Vec<PermissionRuleRecord>| {
            PermissionMutation::Create {
                destination: snapshot.revision.owner.clone(),
                records: records.into_boxed_slice(),
            }
        };
        if let (Some(conversation_snapshot), Some(persistent_snapshot)) =
            (&conversation_snapshot, &persistent_snapshot)
            && conversation_snapshot.store_id == persistent_snapshot.store_id
        {
            let creates = vec![
                create(persistent_snapshot, persistent),
                create(conversation_snapshot, conversation),
            ];
            return self.commit_mutations(
                vec![persistent_snapshot.clone(), conversation_snapshot.clone()],
                creates,
            );
        }
        let filed: Vec<_> = persistent.iter().map(|record| record.id.clone()).collect();
        if let Some(snapshot) = persistent_snapshot {
            let creates = vec![create(&snapshot, persistent)];
            self.commit_mutations(vec![snapshot], creates)?;
        }
        let Some(snapshot) = conversation_snapshot else {
            self.structured_conversation_rules().extend(conversation);
            return Ok(());
        };
        let creates = vec![create(&snapshot, conversation)];
        self.commit_mutations(vec![snapshot], creates)
            .inspect_err(|_| self.withdraw_persistent(&filed))
    }

    /// Takes back persistent records whose conversation companions failed to
    /// commit. A failure here leaves them listed in `/permissions`.
    fn withdraw_persistent(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let revokes = ids
            .iter()
            .map(|id| PermissionMutation::Revoke {
                source: PermissionRecordIdentity {
                    owner: PermissionOwner::Persistent,
                    record_id: id.clone(),
                },
            })
            .collect();
        if let Err(error) = self
            .persistent_snapshot()
            .and_then(|snapshot| self.commit_mutations(vec![snapshot], revokes))
        {
            warn!(
                %error,
                records = ?ids,
                "persistent permission rules outlived their failed answer"
            );
        }
    }

    fn persistent_snapshot(&self) -> Result<PermissionSnapshot, PermissionPolicyError> {
        let policy = self
            .policy
            .as_ref()
            .ok_or_else(|| PermissionPolicyError("persistent storage is disabled".into()))?;
        let mut policy = policy
            .policy
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        policy.state()?.snapshot().map_err(policy_error)
    }

    fn commit_mutations(
        &self,
        expected: Vec<PermissionSnapshot>,
        mutations: Vec<PermissionMutation>,
    ) -> Result<(), PermissionPolicyError> {
        let prepared = prepare_mutations(expected, mutations).map_err(policy_error)?;
        self.commit_prepared_permission_mutation(&prepared)
            .map_err(policy_error)?;
        Ok(())
    }
}

fn policy_error(error: impl Display) -> PermissionPolicyError {
    PermissionPolicyError(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        DECISION_SOURCE_USER_ALWAYS, DECISION_SOURCE_USER_ONCE, DECISION_SOURCE_USER_SESSION,
        PermissionRequest,
    };

    use crate::AgentEvent;
    use crate::permissions::tests::{
        COMPOSED_REQUEST_ID, composed_answer, default_mgr, enforce_without_prompt,
    };
    use crate::permissions::{
        ComposedRow, PermissionAnswer, PermissionLifetime, PermissionRowGrant,
    };
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
                    Some(ComposedRow {
                        grant: PermissionRowGrant::Offered("command_exact_0".into()),
                        lifetime: PermissionLifetime::Conversation,
                    }),
                    Some(ComposedRow {
                        grant: PermissionRowGrant::Written("git status *".into()),
                        lifetime: PermissionLifetime::Project,
                    }),
                    None,
                ],
            },
            PermissionAnswer::Deny,
            PermissionAnswer::DenyWithGuidance("hint".into()),
        ] {
            assert_eq!(PermissionAnswer::decode(&a.encode()), Some(a));
        }
    }

    #[test_case::test_case(&[None, None], DECISION_SOURCE_USER_ONCE; "nothing_remembered")]
    #[test_case::test_case(&[Some(PermissionLifetime::Conversation), None], DECISION_SOURCE_USER_SESSION; "conversation")]
    #[test_case::test_case(&[Some(PermissionLifetime::Conversation), Some(PermissionLifetime::Project)], DECISION_SOURCE_USER_ALWAYS; "the_longest_row_decides")]
    fn a_composed_answer_is_sourced_from_its_longest_lived_row(
        lifetimes: &[Option<PermissionLifetime>],
        expected: &str,
    ) {
        assert_eq!(composed_answer(lifetimes).decision_source(), expected);
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
                        Some(ComposedRow {
                            grant: PermissionRowGrant::Offered("command_pattern_0".into()),
                            lifetime: PermissionLifetime::Conversation,
                        }),
                        None,
                    ],
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
