use super::{PermissionAnswer, PermissionManager, PermissionRequest};
use crate::CancelToken;
use crate::{AgentEvent, EventSender};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::warn;

#[derive(Default)]
pub(super) struct PermissionBroker {
    // Claim/register under this lock; evaluate policy, commit storage, and send only after release.
    pub(super) pending: Mutex<HashMap<u64, HashMap<String, PendingPermission>>>,
    pub(super) revision: AtomicU64,
}

impl PermissionBroker {
    pub(super) fn notify_policy_changed(&self, source_request_id: &str) {
        let senders: Vec<_> = {
            let pending = self
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            self.revision.fetch_add(1, Ordering::Release);
            pending
                .values()
                .flat_map(|requests| requests.values())
                .map(|request| request.changed.clone())
                .collect()
        };
        for sender in senders {
            let _ = sender.try_send(source_request_id.to_owned());
        }
    }
}

pub(super) struct PendingRegistration<'a> {
    pub(super) manager: &'a PermissionManager,
    pub(super) request_id: &'a str,
    pub(super) sender: flume::Sender<PendingDecision>,
    pub(super) event_tx: &'a EventSender,
}

impl Drop for PendingRegistration<'_> {
    fn drop(&mut self) {
        let mut pending = self.manager.pending();
        let candidate = pending
            .get_mut(&self.manager.id)
            .and_then(|requests| requests.get_mut(self.request_id))
            .filter(|request| request.sender.same_channel(&self.sender));
        let present = candidate.is_some();
        if candidate.is_some_and(|request| {
            request.abandoned = true;
            !request.answering
        }) {
            remove_pending(&mut pending, self.manager.id, self.request_id);
        }
        drop(pending);
        if present {
            let _ = self.event_tx.send(AgentEvent::PermissionRequestResolved {
                request_id: self.request_id.to_owned(),
                source_request_id: String::new(),
            });
        }
    }
}

pub(super) struct PendingPermission {
    pub(super) request: PermissionRequest,
    pub(super) project: Option<PathBuf>,
    pub(super) context_revision: u64,
    pub(super) answering: bool,
    pub(super) abandoned: bool,
    pub(super) cancel: CancelToken,
    pub(super) changed: flume::Sender<String>,
    pub(super) sender: flume::Sender<PendingDecision>,
}

pub(super) enum PendingDecision {
    Explicit(PermissionAnswer),
    MatchedRule,
    PolicyDenied(String),
}

pub(super) fn remove_pending(
    pending: &mut HashMap<u64, HashMap<String, PendingPermission>>,
    manager_id: u64,
    request_id: &str,
) -> Option<PendingPermission> {
    let requests = pending.get_mut(&manager_id)?;
    let removed = requests.remove(request_id);
    if requests.is_empty() {
        pending.remove(&manager_id);
    }
    removed
}

impl PermissionManager {
    pub(super) fn pending(
        &self,
    ) -> MutexGuard<'_, HashMap<u64, HashMap<String, PendingPermission>>> {
        self.broker.pending.lock().unwrap_or_else(|error| {
            warn!("permission request mutex was poisoned, recovering");
            error.into_inner()
        })
    }

    pub(super) fn notify_policy_changed(&self, source_request_id: &str) {
        self.broker.notify_policy_changed(source_request_id);
    }

    pub fn answer(&self, request_id: &str, answer: PermissionAnswer) -> bool {
        let context_revision = self
            .context_revision
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let (request, project, sender) = {
            let mut pending = self.pending();
            let Some(candidate) = pending
                .get_mut(&self.id)
                .and_then(|requests| requests.get_mut(request_id))
            else {
                return false;
            };
            if candidate.answering
                || candidate.abandoned
                || candidate.cancel.is_cancelled()
                || candidate.context_revision != *context_revision
            {
                return false;
            }
            candidate.answering = true;
            (
                candidate.request.clone(),
                candidate.project.clone(),
                candidate.sender.clone(),
            )
        };
        if let Err(error) = self.commit_structured_decision(&request, &answer, project.as_deref()) {
            self.release_failed_answer(request_id, &sender);
            warn!(%error, request_id, "permission decision was not committed");
            return false;
        }
        let answered = remove_pending(&mut self.pending(), self.id, request_id);
        drop(context_revision);
        if let Some(answered) = answered {
            let _ = answered.sender.try_send(PendingDecision::Explicit(answer));
        }
        self.notify_policy_changed(request_id);
        true
    }

    fn release_failed_answer(&self, request_id: &str, sender: &flume::Sender<PendingDecision>) {
        let mut pending = self.pending();
        let candidate = pending
            .get_mut(&self.id)
            .and_then(|requests| requests.get_mut(request_id))
            .filter(|candidate| candidate.sender.same_channel(sender));
        let retry = match candidate {
            Some(candidate) if candidate.abandoned => {
                remove_pending(&mut pending, self.id, request_id);
                None
            }
            Some(candidate) => {
                candidate.answering = false;
                Some(candidate.changed.clone())
            }
            None => None,
        };
        drop(pending);
        if let Some(retry) = retry {
            let _ = retry.try_send(request_id.to_owned());
        }
    }

    pub fn pending_count(&self) -> usize {
        self.pending().get(&self.id).map_or(0, HashMap::len)
    }

    pub fn pending_request(&self, request_id: &str) -> Option<PermissionRequest> {
        self.pending()
            .get(&self.id)
            .and_then(|requests| requests.get(request_id))
            .map(|pending| pending.request.clone())
    }
}

#[cfg(test)]
mod tests {

    use test_case::test_case;

    use crate::permissions::tests::{
        COMPOSED_PROMPT_MISSING, CONTROLLED_REQUEST, FIRST_COMMAND, MISSING_UPDATE, SECOND_COMMAND,
        broad_shell_grant, controlled_enforcement, default_mgr, make_config, mgr_with,
        pending_scope_enforcement, pending_tool_enforcement, persistent_manager, remember_command,
    };
    use crate::permissions::{
        PermissionAnswer, PermissionLifetime, PermissionManager, PermissionRequest,
        PermissionRowGrant, PermissionRuleRecord,
    };
    use crate::{AgentEvent, EventSender};
    use caudra_config::{Effect, PermissionRule, PermissionsConfig, ToolKey};
    use caudra_storage::StateDir;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    #[test]
    fn duplicate_request_id_does_not_replace_the_pending_request() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let scopes = crate::tools::PermissionScopes::single("cargo test".into());
            let input = serde_json::json!({"command": "cargo test"});
            let (event_tx, event_rx) = flume::unbounded::<crate::Envelope>();
            let event_tx = crate::EventSender::new(event_tx, 0);
            let (_answer_tx, answer_rx) = flume::unbounded();
            let answer_rx = Arc::new(async_lock::Mutex::new(answer_rx));
            let start = |manager: Arc<PermissionManager>| {
                let scopes = scopes.clone();
                let input = input.clone();
                let event_tx = event_tx.clone();
                let answer_rx = Arc::clone(&answer_rx);
                smol::spawn(async move {
                    manager
                        .enforce(
                            &ToolKey::native("bash"),
                            &scopes,
                            &input,
                            &event_tx,
                            Some(&answer_rx),
                            "duplicate-id",
                            &crate::CancelToken::none(),
                            None,
                        )
                        .await
                })
            };

            let first = start(Arc::clone(&manager));
            event_rx.recv_async().await.unwrap();
            let second = start(Arc::clone(&manager));
            assert!(second.await.is_err());
            assert_eq!(manager.pending_count(), 1);
            assert!(manager.answer("duplicate-id", PermissionAnswer::Deny));
            assert!(first.await.is_err());
            assert_eq!(manager.pending_count(), 0);
        });
    }

    #[test]
    fn reusable_exact_approval_resolves_covered_parallel_requests() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let input = serde_json::json!({"command": "cargo test"});
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "bash",
                "cargo test".into(),
                input.clone(),
            );
            let (second, second_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "second",
                "bash",
                "cargo test".into(),
                input,
            );
            first_events.recv_async().await.unwrap();
            second_events.recv_async().await.unwrap();

            assert_eq!(manager.pending_count(), 2);
            assert!(manager.answer("first", PermissionAnswer::AllowSession));
            assert!(first.await.is_ok());
            assert!(second.await.is_ok());
            assert_eq!(manager.pending_count(), 0);
            assert!(matches!(
                second_events.recv_async().await.unwrap().event,
                AgentEvent::PermissionRequestResolved {
                    request_id,
                    source_request_id
                } if request_id == "second" && source_request_id == "first"
            ));
        });
    }

    #[test]
    fn reusable_approval_does_not_resolve_configured_ask_prompts() {
        smol::block_on(async {
            let manager = Arc::new(mgr_with(
                make_config(vec![PermissionRule {
                    tool: ToolKey::native("bash"),
                    scope: Some("cargo test".into()),
                    effect: Effect::Ask,
                }]),
                PathBuf::from("/tmp"),
            ));
            let input = serde_json::json!({"command": "cargo test"});
            let seed = PermissionRequest::from_legacy(
                "seed".into(),
                ToolKey::native("bash"),
                vec!["cargo test".into()],
                input.clone(),
                Path::new("/tmp"),
                false,
            );
            let rule = seed
                .option_rule("allow_exact", PermissionLifetime::Conversation)
                .unwrap();
            manager.load_structured_conversation_rules(vec![
                PermissionRuleRecord::conversation(rule).unwrap(),
            ]);
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "bash",
                "cargo test".into(),
                input.clone(),
            );
            let (second, second_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "second",
                "bash",
                "cargo test".into(),
                input,
            );
            first_events.recv_async().await.unwrap();
            second_events.recv_async().await.unwrap();

            assert!(manager.answer("first", PermissionAnswer::AllowSession));
            assert!(first.await.is_ok());
            assert_eq!(manager.pending_count(), 1);
            assert!(
                second_events
                    .try_iter()
                    .all(|event| matches!(event.event, AgentEvent::PermissionRequestUpdated(_)))
            );
            assert!(manager.answer("second", PermissionAnswer::AllowOnce));
            assert!(second.await.is_ok());
        });
    }

    /// The batch case: several commands ask at once and one broad grant answers
    /// them all. Every other sweep test uses the same command twice, so nothing
    /// caught that an uncovered command was marked un-sweepable for good.
    #[test]
    fn a_broad_authority_sweeps_a_pending_sibling_with_a_different_command() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "bash",
                "cargo test".into(),
                serde_json::json!({"command": "cargo test"}),
            );
            let (second, second_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "second",
                "bash",
                "rm -rf build".into(),
                serde_json::json!({"command": "rm -rf build"}),
            );
            first_events.recv_async().await.unwrap();
            second_events.recv_async().await.unwrap();
            assert_eq!(manager.pending_count(), 2);

            assert!(manager.answer("first", broad_shell_grant()));
            assert!(matches!(
                second_events.recv_async().await.unwrap().event,
                AgentEvent::PermissionRequestResolved {
                    request_id,
                    source_request_id
                } if request_id == "second" && source_request_id == "first"
            ));
            assert!(first.await.is_ok());
            assert!(second.await.is_ok());
        });
    }

    /// A per-command answer deliberately says nothing about commands that were
    /// already allowed, so the sweep has to count a sibling's own prior
    /// coverage. Otherwise narrowing an answer to what was actually undecided
    /// would strand every sibling that shares an allowed command.
    #[test]
    fn a_narrow_authority_sweeps_a_sibling_whose_other_commands_were_already_allowed() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let settled = "cargo build";
            let undecided = "npm test";
            let seed = PermissionRequest::from_legacy(
                "seed".into(),
                ToolKey::native("bash"),
                vec![settled.into()],
                serde_json::json!({"command": settled}),
                Path::new("/tmp"),
                false,
            );
            manager.load_structured_conversation_rules(vec![
                PermissionRuleRecord::conversation(
                    seed.option_rule("allow_exact_commands", PermissionLifetime::Conversation)
                        .unwrap(),
                )
                .unwrap(),
            ]);
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "bash",
                undecided.into(),
                serde_json::json!({"command": undecided}),
            );
            let (second, second_events) = pending_scope_enforcement(
                Arc::clone(&manager),
                "second",
                "bash",
                crate::tools::PermissionScopes {
                    scopes: vec![settled.into(), undecided.into()],
                    force_prompt: false,
                    plan_scoped: false,
                },
                serde_json::json!({"command": format!("{settled} && {undecided}")}),
            );
            first_events.recv_async().await.unwrap();
            let AgentEvent::PermissionRequest(sibling) =
                second_events.recv_async().await.unwrap().event
            else {
                panic!("{COMPOSED_PROMPT_MISSING}");
            };
            assert!(sibling.presentation.resources[0].covered());
            assert!(!sibling.presentation.resources[1].covered());

            assert!(manager.answer(
                "first",
                PermissionAnswer::AllowComposed {
                    rows: vec![Some(PermissionRowGrant::Offered("command_exact_0".into()))],
                    lifetime: PermissionLifetime::Conversation,
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

    /// A forced prompt is the mode asking, not the rules, so no grant answers it.
    #[test]
    fn a_broad_authority_leaves_a_forced_prompt_pending() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "bash",
                "cargo test".into(),
                serde_json::json!({"command": "cargo test"}),
            );
            let (second, second_events) = pending_scope_enforcement(
                Arc::clone(&manager),
                "second",
                "bash",
                crate::tools::PermissionScopes::force_prompt("rm -rf build".into()),
                serde_json::json!({"command": "rm -rf build"}),
            );
            first_events.recv_async().await.unwrap();
            second_events.recv_async().await.unwrap();

            assert!(manager.answer("first", broad_shell_grant()));
            assert!(first.await.is_ok());
            assert_eq!(manager.pending_count(), 1);
            assert!(
                second_events
                    .try_iter()
                    .all(|event| matches!(event.event, AgentEvent::PermissionRequestUpdated(_)))
            );
            assert!(manager.answer("second", PermissionAnswer::AllowOnce));
            assert!(second.await.is_ok());
        });
    }

    #[test]
    fn allow_once_resolves_only_the_selected_parallel_request() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let input = serde_json::json!({"command": "cargo test"});
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "bash",
                "cargo test".into(),
                input.clone(),
            );
            let (second, second_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "second",
                "bash",
                "cargo test".into(),
                input,
            );
            first_events.recv_async().await.unwrap();
            second_events.recv_async().await.unwrap();

            assert!(manager.answer("first", PermissionAnswer::AllowOnce));
            assert!(first.await.is_ok());
            assert_eq!(manager.pending_count(), 1);
            assert!(
                second_events
                    .try_iter()
                    .all(|event| matches!(event.event, AgentEvent::PermissionRequestUpdated(_)))
            );
            assert!(manager.answer("second", PermissionAnswer::Deny));
            assert!(second.await.is_err());
        });
    }

    #[test]
    fn reusable_exact_approval_leaves_different_input_pending() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "bash",
                "cargo test".into(),
                serde_json::json!({"command": "cargo test"}),
            );
            let (second, second_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "second",
                "bash",
                "cargo check".into(),
                serde_json::json!({"command": "cargo check"}),
            );
            first_events.recv_async().await.unwrap();
            second_events.recv_async().await.unwrap();

            assert!(manager.answer("first", PermissionAnswer::AllowSession));
            assert!(first.await.is_ok());
            assert_eq!(manager.pending_count(), 1);
            assert!(
                second_events
                    .try_iter()
                    .all(|event| matches!(event.event, AgentEvent::PermissionRequestUpdated(_)))
            );
            assert!(manager.answer("second", PermissionAnswer::Deny));
            assert!(second.await.is_err());
        });
    }

    #[test]
    fn conversation_approval_does_not_cross_manager_forks() {
        smol::block_on(async {
            let manager = Arc::new(default_mgr());
            let other = Arc::new(manager.fork());
            let input = serde_json::json!({"command": "cargo test"});
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "bash",
                "cargo test".into(),
                input.clone(),
            );
            let (second, second_events) = pending_tool_enforcement(
                Arc::clone(&other),
                "second",
                "bash",
                "cargo test".into(),
                input,
            );
            first_events.recv_async().await.unwrap();
            second_events.recv_async().await.unwrap();

            assert!(manager.answer("first", PermissionAnswer::AllowSession));
            assert!(first.await.is_ok());
            assert_eq!(other.pending_count(), 1);
            assert!(
                second_events
                    .try_iter()
                    .all(|event| matches!(event.event, AgentEvent::PermissionRequestUpdated(_)))
            );
            assert!(other.answer("second", PermissionAnswer::Deny));
            assert!(second.await.is_err());
        });
    }

    #[test]
    fn filesystem_subtree_approval_resolves_descendant_reads_only() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            let external = temp.path().join("external");
            std::fs::create_dir(&project).unwrap();
            std::fs::create_dir(&external).unwrap();
            let manager = Arc::new(mgr_with(PermissionsConfig::default(), project));
            let first_path = external.join("first.txt").to_string_lossy().into_owned();
            let second_path = external
                .join("nested/second.txt")
                .to_string_lossy()
                .into_owned();
            let outside_path = temp
                .path()
                .join("outside.txt")
                .to_string_lossy()
                .into_owned();
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "file_read",
                first_path.clone(),
                serde_json::json!({"path": first_path}),
            );
            let (second, second_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "second",
                "file_read",
                second_path.clone(),
                serde_json::json!({"path": second_path}),
            );
            let (outside, outside_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "outside",
                "file_read",
                outside_path.clone(),
                serde_json::json!({"path": outside_path}),
            );
            first_events.recv_async().await.unwrap();
            second_events.recv_async().await.unwrap();
            outside_events.recv_async().await.unwrap();

            assert!(manager.answer(
                "first",
                PermissionAnswer::AllowOption {
                    option_id: "allow_filesystem_subtree".into(),
                    lifetime: PermissionLifetime::Conversation,
                }
            ));
            assert!(first.await.is_ok());
            assert!(second.await.is_ok());
            assert_eq!(manager.pending_count(), 1);
            assert!(manager.answer("outside", PermissionAnswer::Deny));
            assert!(outside.await.is_err());
        });
    }

    #[test]
    fn project_approval_resolves_only_pending_requests_in_the_same_project() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let first_project = temp.path().join("first");
            let second_project = temp.path().join("second");
            std::fs::create_dir(&first_project).unwrap();
            std::fs::create_dir(&second_project).unwrap();
            let manager = persistent_manager(
                StateDir::from_path(temp.path().join("state")),
                &first_project,
            );
            let same_project = Arc::new(manager.fork());
            let other_project = Arc::new(manager.fork());
            other_project.set_project(&second_project);
            let url = "https://example.com/docs";
            let input = serde_json::json!({"url": url});
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "webfetch",
                url.into(),
                input.clone(),
            );
            let (same, same_events) = pending_tool_enforcement(
                Arc::clone(&same_project),
                "same",
                "webfetch",
                url.into(),
                input.clone(),
            );
            let (other, other_events) = pending_tool_enforcement(
                Arc::clone(&other_project),
                "other",
                "webfetch",
                url.into(),
                input,
            );
            first_events.recv_async().await.unwrap();
            same_events.recv_async().await.unwrap();
            other_events.recv_async().await.unwrap();

            assert!(manager.answer("first", PermissionAnswer::AllowAlwaysLocal));
            assert!(first.await.is_ok());
            assert!(same.await.is_ok());
            assert_eq!(other_project.pending_count(), 1);
            assert!(
                other_events
                    .try_iter()
                    .all(|event| matches!(event.event, AgentEvent::PermissionRequestUpdated(_)))
            );
            assert!(other_project.answer("other", PermissionAnswer::Deny));
            assert!(other.await.is_err());
        });
    }

    #[test]
    fn global_approval_resolves_pending_requests_in_other_projects() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let first_project = temp.path().join("first");
            let second_project = temp.path().join("second");
            std::fs::create_dir(&first_project).unwrap();
            std::fs::create_dir(&second_project).unwrap();
            let manager = persistent_manager(
                StateDir::from_path(temp.path().join("state")),
                &first_project,
            );
            let other_project = Arc::new(manager.fork());
            other_project.set_project(&second_project);
            let url = "https://example.com/docs";
            let input = serde_json::json!({"url": url});
            let (first, first_events) = pending_tool_enforcement(
                Arc::clone(&manager),
                "first",
                "webfetch",
                url.into(),
                input.clone(),
            );
            let (other, other_events) = pending_tool_enforcement(
                Arc::clone(&other_project),
                "other",
                "webfetch",
                url.into(),
                input,
            );
            first_events.recv_async().await.unwrap();
            other_events.recv_async().await.unwrap();

            assert!(manager.answer("first", PermissionAnswer::AllowAlwaysGlobal));
            assert!(first.await.is_ok());
            assert!(other.await.is_ok());
            assert_eq!(other_project.pending_count(), 0);
            assert!(matches!(
                other_events.recv_async().await.unwrap().event,
                AgentEvent::PermissionRequestResolved { request_id, .. } if request_id == "other"
            ));
        });
    }

    #[test_case(false; "partial_then_complete")]
    #[test_case(true; "coalesced_changes")]
    fn pending_coverage_uses_cumulative_current_policy(coalesced: bool) {
        smol::block_on(async {
            let manager = default_mgr();
            let scopes = crate::tools::PermissionScopes {
                scopes: vec![FIRST_COMMAND.into(), SECOND_COMMAND.into()],
                force_prompt: false,
                plan_scoped: false,
            };
            let (events, received) = flume::unbounded();
            let events = EventSender::new(events, 0);
            let cancel = crate::CancelToken::none();
            let mut enforcement =
                Box::pin(controlled_enforcement(&manager, &scopes, &events, &cancel));
            assert!(
                futures_lite::future::poll_once(&mut enforcement)
                    .await
                    .is_none()
            );
            assert!(matches!(
                received.try_recv().unwrap().event,
                AgentEvent::PermissionRequest(_)
            ));
            remember_command(&manager, FIRST_COMMAND);
            if !coalesced {
                assert!(
                    futures_lite::future::poll_once(&mut enforcement)
                        .await
                        .is_none()
                );
                let AgentEvent::PermissionRequestUpdated(request) =
                    received.try_recv().unwrap().event
                else {
                    panic!("{MISSING_UPDATE}");
                };
                assert!(request.presentation.resources[0].covered());
                assert!(!request.presentation.resources[1].covered());
                assert!(
                    manager
                        .pending_request(CONTROLLED_REQUEST)
                        .unwrap()
                        .presentation
                        .resources[0]
                        .covered()
                );
            }
            remember_command(&manager, SECOND_COMMAND);
            assert!(
                futures_lite::future::poll_once(&mut enforcement)
                    .await
                    .unwrap()
                    .is_ok()
            );
            assert_eq!(manager.pending_count(), 0);
        });
    }

    #[test_case(false; "once_remote_invalidated")]
    #[test_case(true; "remembered_revoked")]
    fn approval_is_revalidated_before_execution(revoke: bool) {
        smol::block_on(async {
            let manager = default_mgr();
            let scopes = crate::tools::PermissionScopes::single(FIRST_COMMAND.into());
            let (events, received) = flume::unbounded();
            let events = EventSender::new(events, 0);
            let cancel = crate::CancelToken::none();
            let mut enforcement =
                Box::pin(controlled_enforcement(&manager, &scopes, &events, &cancel));
            assert!(
                futures_lite::future::poll_once(&mut enforcement)
                    .await
                    .is_none()
            );
            received.try_recv().unwrap();
            assert!(manager.answer(
                CONTROLLED_REQUEST,
                if revoke {
                    PermissionAnswer::AllowSession
                } else {
                    PermissionAnswer::AllowOnce
                }
            ));
            if revoke {
                let record = manager.structured_conversation_rules_snapshot().remove(0);
                manager.revoke_structured_rule(&record.id).unwrap();
            } else {
                manager.invalidate_remote_permission_asset();
            }
            assert!(
                futures_lite::future::poll_once(&mut enforcement)
                    .await
                    .unwrap()
                    .is_err()
            );
        });
    }

    #[test_case(false; "cancel_before_answer")]
    #[test_case(true; "drop_before_answer")]
    fn abandoned_request_cannot_file_authority(drop_waiter: bool) {
        smol::block_on(async {
            let manager = default_mgr();
            let scopes = crate::tools::PermissionScopes::single(FIRST_COMMAND.into());
            let (events, received) = flume::unbounded();
            let events = EventSender::new(events, 0);
            let (trigger, cancel) = crate::CancelToken::new();
            let mut enforcement =
                Box::pin(controlled_enforcement(&manager, &scopes, &events, &cancel));
            assert!(
                futures_lite::future::poll_once(&mut enforcement)
                    .await
                    .is_none()
            );
            received.try_recv().unwrap();
            if drop_waiter {
                drop(enforcement);
            } else {
                trigger.cancel();
                assert!(!manager.answer(CONTROLLED_REQUEST, PermissionAnswer::AllowSession));
                assert!(
                    futures_lite::future::poll_once(&mut enforcement)
                        .await
                        .unwrap()
                        .is_err()
                );
            }
            assert!(!manager.answer(CONTROLLED_REQUEST, PermissionAnswer::AllowSession));
            assert!(manager.structured_conversation_rules_snapshot().is_empty());
            assert_eq!(manager.pending_count(), 0);
            assert!(
                matches!(received.try_recv().unwrap().event, AgentEvent::PermissionRequestResolved { request_id, .. } if request_id == CONTROLLED_REQUEST)
            );
        });
    }

    #[test_case(false; "project_change_before_answer")]
    #[test_case(true; "project_change_after_answer")]
    fn reviewed_project_context_is_not_reinterpreted(answer_first: bool) {
        smol::block_on(async {
            let manager = default_mgr();
            let scopes = crate::tools::PermissionScopes::single(FIRST_COMMAND.into());
            let (events, received) = flume::unbounded();
            let events = EventSender::new(events, 0);
            let cancel = crate::CancelToken::none();
            let mut enforcement =
                Box::pin(controlled_enforcement(&manager, &scopes, &events, &cancel));
            assert!(
                futures_lite::future::poll_once(&mut enforcement)
                    .await
                    .is_none()
            );
            received.try_recv().unwrap();
            if answer_first {
                assert!(manager.answer(CONTROLLED_REQUEST, PermissionAnswer::AllowOnce));
            }
            let destination = tempfile::tempdir().unwrap();
            manager.set_project(destination.path());
            if !answer_first {
                assert!(!manager.answer(CONTROLLED_REQUEST, PermissionAnswer::AllowSession));
            }
            assert!(
                futures_lite::future::poll_once(&mut enforcement)
                    .await
                    .unwrap()
                    .is_err()
            );
        });
    }
    #[test_case(Effect::Ask; "owning_manager_ask")]
    #[test_case(Effect::Deny; "owning_manager_deny")]
    fn persistent_notification_uses_the_waiters_current_configuration(effect: Effect) {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let state = StateDir::from_path(temp.path().join("state"));
            let source = persistent_manager(state.clone(), temp.path());
            let waiter = PermissionManager::new_persistent_in(
                make_config(vec![PermissionRule {
                    tool: ToolKey::native("bash"),
                    scope: Some(FIRST_COMMAND.into()),
                    effect: Effect::Ask,
                }]),
                temp.path().to_path_buf(),
                Arc::default(),
                state,
            );
            let scopes = crate::tools::PermissionScopes::single(FIRST_COMMAND.into());
            let (events, received) = flume::unbounded();
            let events = EventSender::new(events, 0);
            let cancel = crate::CancelToken::none();
            let mut enforcement =
                Box::pin(controlled_enforcement(&waiter, &scopes, &events, &cancel));
            assert!(
                futures_lite::future::poll_once(&mut enforcement)
                    .await
                    .is_none()
            );
            received.try_recv().unwrap();
            let request = PermissionRequest::from_legacy(
                "source".into(),
                ToolKey::native("bash"),
                vec![FIRST_COMMAND.into()],
                serde_json::json!({"command": FIRST_COMMAND}),
                temp.path(),
                false,
            );
            source
                .commit_structured_decision(
                    &request,
                    &PermissionAnswer::AllowAlwaysLocal,
                    Some(temp.path()),
                )
                .unwrap();
            if effect == Effect::Deny {
                waiter.plugin_rules.replace(
                    "restrictive",
                    vec![PermissionRule {
                        tool: ToolKey::native("bash"),
                        scope: Some(FIRST_COMMAND.into()),
                        effect,
                    }],
                );
            }
            source.notify_policy_changed("source");
            let decision = futures_lite::future::poll_once(&mut enforcement).await;
            if effect == Effect::Deny {
                assert!(decision.unwrap().is_err());
            } else {
                assert!(decision.is_none());
                assert_eq!(waiter.pending_count(), 1);
                assert!(waiter.answer(CONTROLLED_REQUEST, PermissionAnswer::Deny));
                assert!(enforcement.await.is_err());
            }
        });
    }

    #[test_case(false; "revoked_coverage_is_not_reused")]
    #[test_case(true; "narrow_allow_outranks_broad_ask")]
    fn pending_authority_is_recomputed_not_presentation_derived(narrow: bool) {
        smol::block_on(async {
            let manager = default_mgr();
            remember_command(&manager, FIRST_COMMAND);
            if narrow {
                manager.plugin_rules.replace(
                    "ask",
                    vec![PermissionRule {
                        tool: ToolKey::native("bash"),
                        scope: Some("npm *".into()),
                        effect: Effect::Ask,
                    }],
                );
            }
            let scopes = crate::tools::PermissionScopes {
                scopes: vec![FIRST_COMMAND.into(), SECOND_COMMAND.into()],
                force_prompt: false,
                plan_scoped: false,
            };
            let (events, received) = flume::unbounded();
            let events = EventSender::new(events, 0);
            let cancel = crate::CancelToken::none();
            let mut enforcement =
                Box::pin(controlled_enforcement(&manager, &scopes, &events, &cancel));
            assert!(
                futures_lite::future::poll_once(&mut enforcement)
                    .await
                    .is_none()
            );
            received.try_recv().unwrap();
            if !narrow {
                let record = manager.structured_conversation_rules_snapshot().remove(0);
                manager.revoke_structured_rule(&record.id).unwrap();
            }
            remember_command(&manager, SECOND_COMMAND);
            let decision = futures_lite::future::poll_once(&mut enforcement).await;
            if narrow {
                assert!(decision.unwrap().is_ok());
            } else {
                assert!(decision.is_none());
                let request = manager.pending_request(CONTROLLED_REQUEST).unwrap();
                assert!(!request.presentation.resources[0].covered());
                assert!(request.presentation.resources[1].covered());
                assert!(manager.answer(CONTROLLED_REQUEST, PermissionAnswer::Deny));
                assert!(enforcement.await.is_err());
            }
        });
    }

    #[test_case(false; "remote_invalid_without_context_change")]
    #[test_case(true; "remote_default_without_context_change")]
    fn postwait_rechecks_remote_policy_even_without_a_context_notification(default_deny: bool) {
        smol::block_on(async {
            let manager = default_mgr();
            let scopes = crate::tools::PermissionScopes::single(FIRST_COMMAND.into());
            let (events, received) = flume::unbounded();
            let events = EventSender::new(events, 0);
            let cancel = crate::CancelToken::none();
            let mut enforcement =
                Box::pin(controlled_enforcement(&manager, &scopes, &events, &cancel));
            assert!(
                futures_lite::future::poll_once(&mut enforcement)
                    .await
                    .is_none()
            );
            received.try_recv().unwrap();
            assert!(manager.answer(CONTROLLED_REQUEST, PermissionAnswer::AllowOnce));
            manager.set_session_yolo(Some(true));
            {
                let mut configured = manager.configured.write().unwrap();
                if default_deny {
                    configured.remote_restrictive_default =
                        Some(crate::permissions::DefaultEffect::Deny);
                } else {
                    configured.remote_policy_invalid = true;
                }
            }
            assert!(
                futures_lite::future::poll_once(&mut enforcement)
                    .await
                    .unwrap()
                    .is_err()
            );
        });
    }

    #[test_case(false; "reused_registration_survives_old_cleanup")]
    #[test_case(true; "cancel_after_answer_prevents_execution")]
    fn settlement_and_cleanup_have_one_registration_owner(cancel_after_answer: bool) {
        smol::block_on(async {
            let manager = default_mgr();
            let scopes = crate::tools::PermissionScopes::single(FIRST_COMMAND.into());
            let (events, received) = flume::unbounded();
            let events = EventSender::new(events, 0);
            let (trigger, cancel) = crate::CancelToken::new();
            let mut first = Box::pin(controlled_enforcement(&manager, &scopes, &events, &cancel));
            assert!(futures_lite::future::poll_once(&mut first).await.is_none());
            received.try_recv().unwrap();
            assert!(manager.answer(CONTROLLED_REQUEST, PermissionAnswer::AllowOnce));
            if cancel_after_answer {
                trigger.cancel();
                assert!(
                    futures_lite::future::poll_once(&mut first)
                        .await
                        .unwrap()
                        .is_err()
                );
            } else {
                let mut second =
                    Box::pin(controlled_enforcement(&manager, &scopes, &events, &cancel));
                assert!(futures_lite::future::poll_once(&mut second).await.is_none());
                received.try_recv().unwrap();
                assert!(
                    futures_lite::future::poll_once(&mut first)
                        .await
                        .unwrap()
                        .is_ok()
                );
                assert_eq!(manager.pending_count(), 1);
                assert!(manager.answer(CONTROLLED_REQUEST, PermissionAnswer::Deny));
                assert!(second.await.is_err());
            }
        });
    }
    #[test_case(false; "claim_release_without_new_authority")]
    #[test_case(true; "claim_release_after_policy_refresh")]
    fn failed_answer_releases_the_claim_and_wakes_its_waiter(covered: bool) {
        smol::block_on(async {
            let manager = default_mgr();
            let scopes = crate::tools::PermissionScopes::single(FIRST_COMMAND.into());
            let (events, received) = flume::unbounded();
            let events = EventSender::new(events, 0);
            let cancel = crate::CancelToken::none();
            let mut enforcement =
                Box::pin(controlled_enforcement(&manager, &scopes, &events, &cancel));
            assert!(
                futures_lite::future::poll_once(&mut enforcement)
                    .await
                    .is_none()
            );
            received.try_recv().unwrap();
            if covered {
                manager
                    .pending()
                    .get_mut(&manager.id)
                    .unwrap()
                    .get_mut(CONTROLLED_REQUEST)
                    .unwrap()
                    .answering = true;
                remember_command(&manager, FIRST_COMMAND);
                assert!(
                    futures_lite::future::poll_once(&mut enforcement)
                        .await
                        .is_none()
                );
                manager
                    .pending()
                    .get_mut(&manager.id)
                    .unwrap()
                    .get_mut(CONTROLLED_REQUEST)
                    .unwrap()
                    .answering = false;
            }
            assert!(!manager.answer(
                CONTROLLED_REQUEST,
                PermissionAnswer::AllowOption {
                    option_id: "not-offered".into(),
                    lifetime: PermissionLifetime::Conversation,
                }
            ));
            let result = futures_lite::future::poll_once(&mut enforcement).await;
            if covered {
                assert!(result.unwrap().is_ok());
            } else {
                assert!(result.is_none());
                assert!(manager.structured_conversation_rules_snapshot().is_empty());
                assert!(manager.answer(CONTROLLED_REQUEST, PermissionAnswer::Deny));
                assert!(enforcement.await.is_err());
            }
        });
    }
    #[test_case(true; "receiver_outlives_registration_drop")]
    #[test_case(false; "receiver_drops_before_registration")]
    fn abandoned_failed_claim_cannot_be_answered_again(receiver_alive: bool) {
        let manager = default_mgr();
        let request = PermissionRequest::from_legacy(
            CONTROLLED_REQUEST.into(),
            ToolKey::native("bash"),
            vec![FIRST_COMMAND.into()],
            serde_json::json!({"command": FIRST_COMMAND}),
            &manager.project_cwd(),
            false,
        );
        let (sender, receiver) = flume::bounded(1);
        let mut receiver = Some(receiver);
        let (changed, _changes) = flume::bounded(1);
        let (events, received) = flume::unbounded();
        let events = EventSender::new(events, 0);
        manager.pending().entry(manager.id).or_default().insert(
            CONTROLLED_REQUEST.into(),
            super::PendingPermission {
                request: request.clone(),
                project: None,
                context_revision: *manager.context_revision.read().unwrap(),
                answering: true,
                abandoned: false,
                cancel: crate::CancelToken::none(),
                changed,
                sender: sender.clone(),
            },
        );
        let registration = super::PendingRegistration {
            manager: &manager,
            request_id: CONTROLLED_REQUEST,
            sender: sender.clone(),
            event_tx: &events,
        };
        if !receiver_alive {
            receiver.take();
        }
        drop(registration);
        assert!(matches!(
            received.try_recv().unwrap().event,
            AgentEvent::PermissionRequestResolved { .. }
        ));
        assert_eq!(sender.is_disconnected(), !receiver_alive);
        assert!(
            manager
                .commit_structured_decision(
                    &request,
                    &PermissionAnswer::AllowOption {
                        option_id: "not-offered".into(),
                        lifetime: PermissionLifetime::Conversation,
                    },
                    None
                )
                .is_err()
        );
        manager.release_failed_answer(CONTROLLED_REQUEST, &sender);
        drop(receiver);
        assert!(!manager.answer(CONTROLLED_REQUEST, PermissionAnswer::AllowSession));
        assert_eq!(manager.pending_count(), 0);
        assert!(manager.structured_conversation_rules_snapshot().is_empty());
    }
}
