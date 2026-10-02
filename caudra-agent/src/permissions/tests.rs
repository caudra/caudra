use crate::CancelToken;

use std::collections::BTreeMap;

use caudra_workspace::{
    AuthenticatedPrincipalId, AuthorityIdentity, ProjectIdentity, ProjectKey, ResourceId,
    ResourceRevision, SourceTrustAnchor, WorkspacePath,
};

use crate::{AgentEvent, EventSender};

use std::sync::Arc;

use caudra_config::{DefaultEffect, Effect, PermissionRule, PermissionsConfig, ToolKey};

use std::path::{Path, PathBuf};

use caudra_storage::StateDir;

use crate::permissions::{
    CONFINED_READ_ATTRIBUTE, CONFINED_READ_VALUE, ComposedRow, OPACITY_ATTRIBUTE, PermissionAnswer,
    PermissionAuthorityProfile, PermissionError, PermissionExecutorKind, PermissionLifetime,
    PermissionManager, PermissionRequest, PermissionResource, PermissionResourceAccess,
    PermissionResourceKind, PermissionRisk, PermissionRowGrant, PermissionSubject, PolicyRule,
    RequestCoverage, ResourceCoverage, RuleOrigin, ShellOpacity, StructuredPermissionDecision,
    StructuredPermissionRule, filesystem_permission_resource, permission_rule_intersects_request,
    permission_rules_resource_standing,
};
use futures_lite::future;
pub(super) const PERMISSION_RULES_STATE_KEY: &str = "permission.rules";

pub(super) const SHELL_WORKDIR: &str = "/tmp";

pub(super) const LEGACY_REQUEST_ID: &str = "legacy-request";

/// The decision engine experiment is on; the tests that need it off build
/// their config without it.
pub(super) fn make_config(rules: Vec<PermissionRule>) -> PermissionsConfig {
    PermissionsConfig {
        rules,
        decision_engine: true,
        ..Default::default()
    }
}

pub(super) fn allow_rule(scope: &str) -> PermissionRule {
    PermissionRule {
        tool: ToolKey::native("bash"),
        scope: Some(scope.into()),
        effect: Effect::Allow,
    }
}

pub(super) fn deny_rule(scope: &str) -> PermissionRule {
    PermissionRule {
        tool: ToolKey::native("bash"),
        scope: Some(scope.into()),
        effect: Effect::Deny,
    }
}

pub(super) fn remote_permission_asset(
    revision: &str,
    digest: &str,
    allow_scope: &str,
) -> crate::remote_project_context::RemotePermissionAsset {
    let authority = AuthorityIdentity::new(
        SourceTrustAnchor::new("https://workcell.example").unwrap(),
        "server",
        "workspace",
        "generation",
        "namespace",
    )
    .unwrap();
    crate::remote_project_context::RemotePermissionAsset {
        source: crate::remote_project_context::RemoteAssetIdentity {
            principal: AuthenticatedPrincipalId::new(authority.clone(), "principal").unwrap(),
            project: ProjectIdentity::new(authority.clone(), ProjectKey::new("project").unwrap()),
            authority,
            path: WorkspacePath::new(".caudra/permissions.toml").unwrap(),
            resource_id: ResourceId::new("permissions").unwrap(),
            revision: ResourceRevision::new(revision).unwrap(),
        },
        digest: digest.into(),
        declarations: crate::remote_project_context::RemotePermissionDeclarations {
            restrictive_rules: vec![deny_rule("remote-denied")],
            allow_rules: vec![allow_rule(allow_scope)],
            ..Default::default()
        },
    }
}

/// What the one evaluator says about each resource, over the configured
/// policy compiled against the request.
pub(super) fn decisions(
    manager: &PermissionManager,
    request: &PermissionRequest,
) -> Vec<StructuredPermissionDecision> {
    let rules = manager.configured_structured_rules(request, true).unwrap();
    request
        .resources
        .iter()
        .map(|resource| permission_rules_resource_standing(&rules, request, resource).decision)
        .collect()
}

/// Coverage the way `enforce` computes it: the configured, builtin, and
/// plugin policy compiled against the request, plus whatever stored rules
/// the case is about.
pub(super) fn coverage_with(
    manager: &PermissionManager,
    request: &PermissionRequest,
    builtin_allows: bool,
    stored: &[PolicyRule],
) -> RequestCoverage {
    let mut rules = stored.to_vec();
    rules.extend(
        manager
            .configured_structured_rules(request, builtin_allows)
            .unwrap(),
    );
    manager.request_coverage(request, &rules, builtin_allows)
}

/// Which resources carry authority, for cases about coverage rather than
/// about the authority that granted it.
pub(super) fn covered_flags(coverage: &RequestCoverage) -> Vec<bool> {
    coverage.covered.iter().map(Option::is_some).collect()
}

/// A stored grant, which is what a persisted record compiles to.
pub(super) fn stored_policy(rule: StructuredPermissionRule) -> PolicyRule {
    PolicyRule {
        origin: RuleOrigin::Project,
        rule,
    }
}

/// A call the rules settle on their own: every resource carries authority
/// and nothing withholds it, so `enforce` returns without prompting.
pub(super) fn allows_without_prompt(
    manager: &PermissionManager,
    request: &PermissionRequest,
) -> bool {
    let coverage = coverage_with(manager, request, true, &[]);
    coverage.covered.iter().all(Option::is_some) && !coverage.must_prompt
}

/// A restrictive rule reaching the request, which is the one answer that
/// outranks every grant, yolo, and the default effect.
pub(super) fn denied_by_rule(manager: &PermissionManager, request: &PermissionRequest) -> bool {
    manager
        .applicable_rules_within(request, false, true)
        .unwrap()
        .iter()
        .any(|policy| permission_rule_intersects_request(&policy.rule, request))
}

/// The default effect answers what no rule spoke to, so a deny default only
/// blocks the resources the rules left uncovered.
pub(super) fn denied_by_default(manager: &PermissionManager, request: &PermissionRequest) -> bool {
    matches!(manager.default_effect(&request.tool), DefaultEffect::Deny)
        && !coverage_with(manager, request, true, &[])
            .covered
            .iter()
            .all(Option::is_some)
}

pub(super) fn allowed_by_default(manager: &PermissionManager, request: &PermissionRequest) -> bool {
    matches!(manager.default_effect(&request.tool), DefaultEffect::Allow)
        && !coverage_with(manager, request, true, &[]).must_prompt
}

pub(super) fn legacy_request(
    manager: &PermissionManager,
    tool: ToolKey,
    scopes: &[&str],
) -> PermissionRequest {
    PermissionRequest::from_legacy(
        LEGACY_REQUEST_ID.into(),
        tool,
        scopes.iter().map(|scope| (*scope).to_owned()).collect(),
        serde_json::Value::Null,
        &manager.project_cwd(),
        false,
    )
}

pub(super) fn shell_policy_rule(scope: &str, effect: Effect) -> PermissionRule {
    PermissionRule {
        tool: ToolKey::native("shell"),
        scope: Some(scope.into()),
        effect,
    }
}

pub(super) fn shell_intent(commands: &[&str]) -> crate::tools::PermissionIntent {
    let workdir = SHELL_WORKDIR;
    crate::tools::PermissionIntent::new(
        crate::tools::PermissionScopes {
            scopes: commands.iter().map(|command| (*command).into()).collect(),
            force_prompt: false,
            plan_scoped: false,
        },
        commands
            .iter()
            .map(|command| PermissionResource {
                kind: PermissionResourceKind::Command,
                value: (*command).into(),
                access: Some(PermissionResourceAccess::Execute),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::from([("workdir".into(), workdir.into())]),
            })
            .collect(),
        PermissionRisk::High,
    )
    .with_authority(PermissionAuthorityProfile::Shell)
}

pub(super) fn shell_request(commands: &[&str], subject: PermissionSubject) -> PermissionRequest {
    let workdir = SHELL_WORKDIR;
    let intent = shell_intent(commands);
    PermissionRequest::from_intent_with_identity(
        "shell-request".into(),
        ToolKey::native("shell"),
        &intent,
        serde_json::json!({"command": commands.join(" && "), "workdir": workdir}),
        Path::new(workdir),
        subject,
        PermissionExecutorKind::Native,
    )
}

/// Runs the real evaluator over a shell call with no response channel, so a
/// call it cannot settle on the rules alone reports the refusal instead of
/// waiting on a prompt nobody will answer.
pub(super) async fn enforce_shell_without_prompt(
    manager: &PermissionManager,
    commands: &[&str],
    force_prompt: bool,
) -> Result<(), PermissionError> {
    let mut intent = shell_intent(commands);
    intent.scopes.force_prompt = force_prompt;
    let (event_tx, _event_rx) = flume::unbounded();
    manager
        .enforce_with_intent(
            &ToolKey::native("shell"),
            &intent,
            &serde_json::json!({"command": commands.join(" && "), "workdir": SHELL_WORKDIR}),
            &crate::EventSender::new(event_tx, 0),
            None,
            "shell-request",
            &CancelToken::none(),
            None,
            Some((workcell_shell_subject(), PermissionExecutorKind::Native)),
            true,
        )
        .await
}

pub(super) fn workcell_shell_subject() -> PermissionSubject {
    PermissionSubject::Native {
        owner: "workcell".into(),
        contract: "shell.execution.v1".into(),
    }
}

pub(super) fn mgr_with(config: PermissionsConfig, cwd: PathBuf) -> PermissionManager {
    PermissionManager::new_nonpersistent(config, cwd, Arc::default())
}

pub(super) fn default_mgr() -> PermissionManager {
    mgr_with(make_config(Vec::new()), PathBuf::from("/tmp"))
}

pub(super) const CONFINED_COMMAND: &str = "git status --short";

pub(super) fn mark_confined(request: &mut PermissionRequest) {
    for resource in &mut request.resources {
        resource.attributes.insert(
            CONFINED_READ_ATTRIBUTE.into(),
            CONFINED_READ_VALUE.to_owned(),
        );
    }
}

pub(super) const COVERAGE_COMMAND: &str = "git status --short";

pub(super) const COVERAGE_PATTERN: &str = "git status *";

pub(super) const ECHO_COMMAND: &str = "echo hi";

pub(super) const BUILTIN_ECHO_PATTERN: &str = "echo *";

pub(super) const THIS_COMMAND_AUTHORITY: &str = "this command";

pub(super) const BROAD_GIT_PATTERN: &str = "git *";

pub(super) fn coverage_of(
    manager: &PermissionManager,
    command: &str,
    stored: &[PolicyRule],
) -> Option<ResourceCoverage> {
    let request = shell_request(&[command], workcell_shell_subject());
    coverage_with(manager, &request, true, stored)
        .covered
        .swap_remove(0)
}

pub(super) fn conversation_grant(command: &str) -> PolicyRule {
    let request = shell_request(&[command], workcell_shell_subject());
    PolicyRule {
        origin: RuleOrigin::Conversation,
        rule: request
            .option_rule("allow_exact", PermissionLifetime::Conversation)
            .unwrap(),
    }
}

pub(super) const GIT_HEAD: &str = ".git/HEAD";

pub(super) const GIT_CONFIG: &str = ".git/config";

pub(super) fn project_read_request(cwd: &Path, relative: &str) -> PermissionRequest {
    let path = cwd.join(relative);
    let intent = crate::tools::PermissionIntent::new(
        crate::tools::PermissionScopes::single(path.to_string_lossy().into_owned()),
        vec![filesystem_permission_resource(
            PermissionResourceKind::File,
            &path,
            PermissionResourceAccess::Read,
            cwd,
        )],
        PermissionRisk::Low,
    );
    PermissionRequest::from_intent(
        "read".into(),
        ToolKey::native("file_read"),
        &intent,
        serde_json::json!({"filePath": path.to_string_lossy()}),
        cwd,
    )
}

pub(super) fn plugin_edit_rule(scope: &str, effect: Effect) -> PermissionRule {
    PermissionRule {
        tool: ToolKey::native("edit"),
        scope: Some(scope.into()),
        effect,
    }
}

pub(super) const COMPLEX_COMMAND: &str = "echo $(whoami)";

pub(super) const ALLOWED_COMMANDS: [&str; 2] = ["cargo test", "git push"];

pub(super) fn pending_tool_enforcement(
    manager: Arc<PermissionManager>,
    request_id: &str,
    tool: &str,
    scope: String,
    input: serde_json::Value,
) -> (
    smol::Task<Result<(), PermissionError>>,
    flume::Receiver<crate::Envelope>,
) {
    pending_scope_enforcement(
        manager,
        request_id,
        tool,
        crate::tools::PermissionScopes::single(scope),
        input,
    )
}

pub(super) fn pending_scope_enforcement(
    manager: Arc<PermissionManager>,
    request_id: &str,
    tool: &str,
    scopes: crate::tools::PermissionScopes,
    input: serde_json::Value,
) -> (
    smol::Task<Result<(), PermissionError>>,
    flume::Receiver<crate::Envelope>,
) {
    let (event_tx, event_rx) = flume::unbounded();
    let event_tx = crate::EventSender::new(event_tx, 0);
    let request_id = request_id.to_owned();
    let tool = ToolKey::native(tool);
    let task = smol::spawn(async move {
        let (_legacy_tx, legacy_rx) = flume::unbounded();
        let legacy_rx = async_lock::Mutex::new(legacy_rx);
        manager
            .enforce(
                &tool,
                &scopes,
                &input,
                &event_tx,
                Some(&legacy_rx),
                &request_id,
                &CancelToken::none(),
                None,
            )
            .await
    });
    (task, event_rx)
}

pub(super) const BROAD_SHELL_OPTION: &str = "allow_any_command";

pub(super) fn broad_shell_grant() -> PermissionAnswer {
    PermissionAnswer::AllowOption {
        option_id: BROAD_SHELL_OPTION.into(),
        lifetime: PermissionLifetime::Conversation,
    }
}

pub(super) fn seeded_mgr(yolo: bool) -> PermissionManager {
    mgr_with(
        PermissionsConfig {
            yolo,
            ..make_config(Vec::new())
        },
        PathBuf::from("/tmp"),
    )
}

pub(super) const CARGO_TEST_COMMAND: &str = "cargo test";

pub(super) const EMPTY_MCP_SCOPE: &str = "{}";

pub(super) const PLAN_PATH: &str =
    "/home/user/.local/state/caudra/projects/app-0123456789abcdef/plans/test.md";

/// Enforces against the plan being built with no response channel, so the
/// plan-write escape hatch is the only thing that can let the call through.
pub(super) async fn enforce_plan_write_without_prompt(
    manager: &PermissionManager,
    tool: &str,
    scopes: &[&str],
) -> Result<(), PermissionError> {
    let (event_tx, _event_rx) = flume::unbounded();
    manager
        .enforce(
            &ToolKey::native(tool),
            &crate::tools::PermissionScopes {
                scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
                force_prompt: false,
                plan_scoped: false,
            },
            &serde_json::Value::Null,
            &crate::EventSender::new(event_tx, 0),
            None,
            "plan-request",
            &CancelToken::none(),
            Some(Path::new(PLAN_PATH)),
        )
        .await
}

pub(super) const PLUGIN_EDIT_PATH: &str = "/x/f";

pub(super) fn persistent_manager(state_dir: StateDir, project: &Path) -> Arc<PermissionManager> {
    Arc::new(PermissionManager::new_persistent_in(
        make_config(Vec::new()),
        project.to_path_buf(),
        Arc::default(),
        state_dir,
    ))
}

pub(super) async fn answer_enforcement(
    manager: Arc<PermissionManager>,
    scope: &str,
    input: serde_json::Value,
    answer: PermissionAnswer,
) -> Result<(), PermissionError> {
    answer_tool_enforcement(manager, "bash", scope, input, answer).await
}

pub(super) async fn answer_tool_enforcement(
    manager: Arc<PermissionManager>,
    tool: &str,
    scope: &str,
    input: serde_json::Value,
    answer: PermissionAnswer,
) -> Result<(), PermissionError> {
    let scopes = crate::tools::PermissionScopes::single(scope.to_owned());
    let (event_tx, event_rx) = flume::unbounded::<crate::Envelope>();
    let event_tx = crate::EventSender::new(event_tx, 0);
    let (_legacy_tx, legacy_rx) = flume::unbounded();
    let legacy_rx = Arc::new(async_lock::Mutex::new(legacy_rx));
    let tool = ToolKey::native(tool);
    let task = smol::spawn({
        let manager = Arc::clone(&manager);
        let legacy_rx = Arc::clone(&legacy_rx);
        async move {
            manager
                .enforce(
                    &tool,
                    &scopes,
                    &input,
                    &event_tx,
                    Some(&legacy_rx),
                    "durable-request",
                    &CancelToken::none(),
                    None,
                )
                .await
        }
    });
    let event = event_rx.recv_async().await.unwrap().event;
    assert!(matches!(event, AgentEvent::PermissionRequest(_)));
    assert!(manager.answer("durable-request", answer));
    task.await
}

pub(super) async fn enforce_without_prompt(
    manager: &PermissionManager,
    scope: &str,
    input: serde_json::Value,
) -> Result<(), PermissionError> {
    enforce_tool_without_prompt(manager, "bash", scope, input).await
}

pub(super) async fn enforce_tool_without_prompt(
    manager: &PermissionManager,
    tool: &str,
    scope: &str,
    input: serde_json::Value,
) -> Result<(), PermissionError> {
    let (event_tx, _) = flume::unbounded::<crate::Envelope>();
    manager
        .enforce(
            &ToolKey::native(tool),
            &crate::tools::PermissionScopes::single(scope.to_owned()),
            &input,
            &crate::EventSender::new(event_tx, 0),
            None,
            "restart-request",
            &CancelToken::none(),
            None,
        )
        .await
}

pub(super) const COMPOSED_REQUEST_ID: &str = "composed-request";

/// A composed answer whose rows last as listed; what each row grants does not
/// matter to the questions asked of it.
pub(super) fn composed_answer(lifetimes: &[Option<PermissionLifetime>]) -> PermissionAnswer {
    PermissionAnswer::AllowComposed {
        rows: lifetimes
            .iter()
            .map(|lifetime| {
                lifetime.clone().map(|lifetime| ComposedRow {
                    grant: PermissionRowGrant::Written(String::new()),
                    lifetime,
                })
            })
            .collect(),
    }
}

pub(super) const COMPOSED_PROMPT_MISSING: &str = "batched commands did not raise a prompt";

pub(super) fn opaque_intent(
    command: &str,
    workdir: &Path,
    plan_scoped: bool,
) -> crate::tools::PermissionIntent {
    let scopes = crate::tools::PermissionScopes {
        scopes: vec![command.to_owned()],
        force_prompt: false,
        plan_scoped,
    };
    crate::tools::PermissionIntent::new(
        scopes,
        vec![PermissionResource {
            kind: PermissionResourceKind::Command,
            value: command.into(),
            access: Some(PermissionResourceAccess::Execute),
            protected: true,
            requires_prompt: true,
            attributes: BTreeMap::from([(
                "workdir".into(),
                workdir.to_string_lossy().into_owned(),
            )]),
        }],
        PermissionRisk::Critical,
    )
    .with_authority(PermissionAuthorityProfile::Shell)
}

pub(super) async fn enforce_opaque_command_without_prompt(
    manager: &PermissionManager,
    workdir: &Path,
    command: &str,
) -> Result<(), PermissionError> {
    enforce_opaque_command_scoped(manager, workdir, command, false).await
}

pub(super) async fn enforce_plan_command_without_prompt(
    manager: &PermissionManager,
    workdir: &Path,
    command: &str,
) -> Result<(), PermissionError> {
    enforce_opaque_command_scoped(manager, workdir, command, true).await
}

pub(super) async fn enforce_opaque_command_scoped(
    manager: &PermissionManager,
    workdir: &Path,
    command: &str,
    plan_scoped: bool,
) -> Result<(), PermissionError> {
    let (event_tx, _event_rx) = flume::unbounded::<crate::Envelope>();
    manager
        .enforce_with_intent(
            &ToolKey::native("bash"),
            &opaque_intent(command, workdir, plan_scoped),
            &serde_json::json!({"command": command}),
            &crate::EventSender::new(event_tx, 0),
            None,
            "opaque-request",
            &CancelToken::none(),
            None,
            None,
            true,
        )
        .await
}

/// Answers one plan-scoped command prompt, reporting whether the answer was
/// accepted and what the request offered. A refused answer leaves the call
/// waiting, so it is always followed by one that ends it.
pub(super) async fn answer_plan_command(
    manager: Arc<PermissionManager>,
    workdir: &Path,
    command: &str,
    answer: PermissionAnswer,
) -> (bool, Result<(), PermissionError>, Box<PermissionRequest>) {
    let intent = opaque_intent(command, workdir, true);
    let input = serde_json::json!({ "command": command });
    let (event_tx, event_rx) = flume::unbounded::<crate::Envelope>();
    let (_legacy_tx, legacy_rx) = flume::unbounded();
    let legacy_rx = Arc::new(async_lock::Mutex::new(legacy_rx));
    let task = smol::spawn({
        let manager = Arc::clone(&manager);
        let input = input.clone();
        async move {
            manager
                .enforce_with_intent(
                    &ToolKey::native("bash"),
                    &intent,
                    &input,
                    &crate::EventSender::new(event_tx, 0),
                    Some(&legacy_rx),
                    PLAN_REQUEST_ID,
                    &CancelToken::none(),
                    None,
                    None,
                    true,
                )
                .await
        }
    });
    let AgentEvent::PermissionRequest(request) = event_rx.recv_async().await.unwrap().event else {
        panic!("{PLAN_PROMPT_MISSING}");
    };
    let accepted = manager.answer(PLAN_REQUEST_ID, answer);
    if !accepted {
        manager.answer(PLAN_REQUEST_ID, PermissionAnswer::Deny);
    }
    (accepted, task.await, request)
}

pub(super) const PLAN_REQUEST_ID: &str = "plan-request";

pub(super) const PLAN_PROMPT_MISSING: &str = "plan-scoped call did not raise a prompt";

pub(super) const WORKDIR_OPTION: &str = "allow_commands_in_workdir";

pub(super) fn workdir_grant(lifetime: PermissionLifetime) -> PermissionAnswer {
    PermissionAnswer::AllowOption {
        option_id: WORKDIR_OPTION.into(),
        lifetime,
    }
}

pub(super) fn log_request(resources: Vec<PermissionResource>) -> PermissionRequest {
    let mut request = PermissionRequest::from_legacy(
        "log".into(),
        ToolKey::native("bash"),
        vec!["cargo test".into()],
        serde_json::json!({"command": "cargo test"}),
        Path::new("/tmp"),
        false,
    );
    request.resources = resources;
    request
}

pub(super) fn log_coverage() -> Option<ResourceCoverage> {
    Some(ResourceCoverage {
        origin: RuleOrigin::Project,
        authority: "this command".into(),
        asks: false,
    })
}

pub(super) fn log_resource(
    value: &str,
    protected: bool,
    requires_prompt: bool,
) -> PermissionResource {
    PermissionResource {
        kind: PermissionResourceKind::Command,
        value: value.into(),
        access: Some(PermissionResourceAccess::Execute),
        protected,
        requires_prompt,
        attributes: BTreeMap::new(),
    }
}

pub(super) const CONTROLLED_REQUEST: &str = "controlled-permission";

pub(super) const FIRST_COMMAND: &str = "cargo build";

pub(super) const SECOND_COMMAND: &str = "npm test";

pub(super) const MISSING_UPDATE: &str = "expected partial coverage update";

pub(super) async fn controlled_enforcement(
    manager: &PermissionManager,
    scopes: &crate::tools::PermissionScopes,
    event_tx: &EventSender,
    cancel: &CancelToken,
) -> Result<(), PermissionError> {
    let (_legacy_tx, legacy_rx) = flume::unbounded();
    let legacy_rx = async_lock::Mutex::new(legacy_rx);
    manager
        .enforce(
            &ToolKey::native("bash"),
            scopes,
            &serde_json::json!({"command": scopes.scopes.join(" && ")}),
            event_tx,
            Some(&legacy_rx),
            CONTROLLED_REQUEST,
            cancel,
            None,
        )
        .await
}

pub(super) const SHELL_PROMPT_MISSING: &str = "the shell call ran without a prompt";

const PROMPT_NOT_FIRST: &str = "a prompted shell call raises its prompt before any other event";

/// A line the first-party shell could not review command by command, the way
/// it presents one: the command it did read, then the whole line, protected
/// and naming why.
pub(super) fn opaque_line_intent(
    line: &str,
    opacity: ShellOpacity,
) -> crate::tools::PermissionIntent {
    let mut intent = shell_intent(&[FIRST_COMMAND]);
    intent.scopes.scopes.push(line.into());
    intent.resources.push(PermissionResource {
        kind: PermissionResourceKind::Command,
        value: line.into(),
        access: Some(PermissionResourceAccess::Execute),
        protected: true,
        requires_prompt: true,
        attributes: BTreeMap::from([
            ("workdir".into(), SHELL_WORKDIR.into()),
            (OPACITY_ATTRIBUTE.into(), opacity.to_string()),
        ]),
    });
    intent
}

/// Runs a first-party shell call that can be answered. A prompt it raises is
/// denied and returned; `None` means the call ran without one.
pub(super) async fn shell_prompt(
    manager: &PermissionManager,
    intent: &crate::tools::PermissionIntent,
    command: &str,
) -> Option<Box<PermissionRequest>> {
    let (event_tx, events) = flume::unbounded();
    let event_tx = EventSender::new(event_tx, 0);
    let (_legacy_tx, legacy_rx) = flume::unbounded();
    let legacy_rx = async_lock::Mutex::new(legacy_rx);
    let input = serde_json::json!({"command": command, "workdir": SHELL_WORKDIR});
    let tool = ToolKey::native("shell");
    let cancel = CancelToken::none();
    let mut enforcement = Box::pin(manager.enforce_with_intent(
        &tool,
        intent,
        &input,
        &event_tx,
        Some(&legacy_rx),
        CONTROLLED_REQUEST,
        &cancel,
        None,
        Some((workcell_shell_subject(), PermissionExecutorKind::Native)),
        true,
    ));
    let prompt = future::race(
        async {
            enforcement.as_mut().await.unwrap();
            None
        },
        async {
            let AgentEvent::PermissionRequest(request) = events.recv_async().await.unwrap().event
            else {
                panic!("{PROMPT_NOT_FIRST}");
            };
            Some(request)
        },
    )
    .await;
    if prompt.is_some() {
        assert!(manager.answer(CONTROLLED_REQUEST, PermissionAnswer::Deny));
        assert!(enforcement.await.is_err());
    }
    prompt
}

pub(super) fn remember_command(manager: &PermissionManager, command: &str) {
    let request = PermissionRequest::from_legacy(
        "grant".into(),
        ToolKey::native("bash"),
        vec![command.into()],
        serde_json::json!({"command": command}),
        Path::new(SHELL_WORKDIR),
        false,
    );
    manager
        .commit_structured_decision(
            &request,
            &PermissionAnswer::AllowOption {
                option_id: "allow_exact_commands".into(),
                lifetime: PermissionLifetime::Conversation,
            },
            None,
            false,
        )
        .unwrap();
    manager.notify_policy_changed("grant");
}
