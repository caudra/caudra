use crate::tools::PermissionScopes;
use serde_json::json;

use std::collections::BTreeMap;

use std::path::Path;

use caudra_config::ToolKey;

use serde_json::Value;

use crate::permissions::structured::{
    ComposedAnswerError, ComposedRow, MCP_CONTRACT, PermissionArgumentConstraint,
    PermissionAuthorityProfile, PermissionCaution, PermissionExecutorKind, PermissionIntent,
    PermissionLifetime, PermissionRequest, PermissionResource, PermissionResourceAccess,
    PermissionResourceConstraint, PermissionResourceKind, PermissionResourceSelector,
    PermissionRisk, PermissionRowGrant, PermissionRuleOption, PermissionSubject,
    RemotePermissionIdentity, ResourceCoverage, RuleOrigin, StructuredPermissionDecision,
    StructuredPermissionEffect, StructuredPermissionRule, URL_SUBTREE_OPTION_ID, WORKCELL_OWNER,
    filesystem_resource_flags, resource_decision,
};
pub(super) fn request(resources: Vec<PermissionResource>) -> PermissionRequest {
    PermissionRequest::from_legacy(
        "request".into(),
        ToolKey::native("legacy"),
        resources
            .iter()
            .map(|resource| resource.value.clone())
            .collect(),
        json!({"branch": "main", "nested": {"value": 1}}),
        Path::new("/tmp"),
        false,
    )
    .with_resources(resources)
}

pub(super) trait RequestTestExt {
    fn with_resources(self, resources: Vec<PermissionResource>) -> Self;
}

impl RequestTestExt for PermissionRequest {
    fn with_resources(mut self, resources: Vec<PermissionResource>) -> Self {
        self.resources = resources;
        self
    }
}

pub(super) fn custom_resource(value: &str) -> PermissionResource {
    PermissionResource {
        kind: PermissionResourceKind::Custom {
            name: "test".into(),
        },
        value: value.into(),
        access: Some(PermissionResourceAccess::Execute),
        protected: false,
        requires_prompt: false,
        attributes: BTreeMap::new(),
    }
}

pub(super) fn command_resource(value: &str, workdir: &str) -> PermissionResource {
    PermissionResource {
        kind: PermissionResourceKind::Command,
        value: value.into(),
        access: Some(PermissionResourceAccess::Execute),
        protected: false,
        requires_prompt: false,
        attributes: BTreeMap::from([("workdir".into(), workdir.into())]),
    }
}

pub(super) fn protected_command_resource(value: &str, workdir: &str) -> PermissionResource {
    PermissionResource {
        protected: true,
        requires_prompt: true,
        ..command_resource(value, workdir)
    }
}

pub(super) fn explicit_request(
    authority: PermissionAuthorityProfile,
    resources: Vec<PermissionResource>,
    input: Value,
) -> PermissionRequest {
    let intent = PermissionIntent::new(
        PermissionScopes::single("legacy-scope".into()),
        resources,
        PermissionRisk::Medium,
    )
    .with_authority(authority);
    PermissionRequest::from_intent_with_identity(
        "request".into(),
        ToolKey::native("generic_platform_tool"),
        &intent,
        input,
        Path::new("/project"),
        PermissionSubject::Native {
            owner: "first-party".into(),
            contract: "platform/v1".into(),
        },
        PermissionExecutorKind::Native,
    )
}

pub(super) fn remote_identity(
    anchor: &str,
    server: &str,
    workspace: &str,
    generation: &str,
    namespace: &str,
    principal: &str,
    project: &str,
) -> RemotePermissionIdentity {
    let authority = caudra_workspace::AuthorityIdentity::new(
        caudra_workspace::SourceTrustAnchor::new(anchor).unwrap(),
        server,
        workspace,
        generation,
        namespace,
    )
    .unwrap();
    RemotePermissionIdentity {
        principal: caudra_workspace::AuthenticatedPrincipalId::new(authority.clone(), principal)
            .unwrap(),
        project: caudra_workspace::ProjectIdentity::new(
            authority.clone(),
            caudra_workspace::ProjectKey::new(project).unwrap(),
        ),
        authority,
    }
}

pub(super) fn default_remote_identity() -> RemotePermissionIdentity {
    remote_identity(
        "https://workcell.example",
        "server",
        "workspace",
        "generation",
        "namespace",
        "principal",
        "project",
    )
}

pub(super) fn remote_request_with(identity: RemotePermissionIdentity) -> PermissionRequest {
    remote_request_resource(
        identity.clone(),
        PermissionResourceKind::RemoteFile { identity },
        "root\u{1f}opaque-file",
        false,
    )
}

/// A mutating remote call is protected and must prompt, the way the Workcell
/// adapter marks every intent its authority reports as mutating.
pub(super) fn remote_request_resource(
    identity: RemotePermissionIdentity,
    kind: PermissionResourceKind,
    value: &str,
    mutating: bool,
) -> PermissionRequest {
    let intent = PermissionIntent::new(
        PermissionScopes::single("remote intent".into()),
        vec![PermissionResource {
            kind,
            value: value.into(),
            access: Some(PermissionResourceAccess::Read),
            protected: mutating,
            requires_prompt: mutating,
            attributes: BTreeMap::from([(
                "display_path".into(),
                "/path/that/must/not/be/probed".into(),
            )]),
        }],
        PermissionRisk::Low,
    )
    .with_authority(PermissionAuthorityProfile::RemoteResource);
    PermissionRequest::from_intent_with_identity(
        "remote-request".into(),
        ToolKey::native("file_read"),
        &intent,
        json!({"filePath":"display only"}),
        Path::new("/local/project"),
        PermissionSubject::RemoteWorkcell {
            identity,
            tool: "file_read".into(),
            contract: "file.read.v1@v1/v1".into(),
        },
        PermissionExecutorKind::RemoteWorkcell,
    )
}

pub(super) fn remote_request() -> PermissionRequest {
    remote_request_with(default_remote_identity())
}

pub(super) fn exact_constraint(resource: &PermissionResource) -> PermissionResourceConstraint {
    PermissionResourceConstraint {
        kind: resource.kind.clone(),
        selector: PermissionResourceSelector::Exact {
            value: resource.value.clone(),
        },
        access: resource.access.clone(),
        protected: Some(resource.protected),
        attributes: BTreeMap::new(),
    }
}

pub(super) fn rule(
    request: &PermissionRequest,
    effect: StructuredPermissionEffect,
    resources: Vec<PermissionResourceConstraint>,
) -> StructuredPermissionRule {
    StructuredPermissionRule {
        subject: request.subject.clone(),
        executor: request.executor.clone(),
        resources,
        arguments: PermissionArgumentConstraint::Exact {
            digest: request.input_digest.clone(),
        },
        lifetime: request.lifetime.clone(),
        effect,
        family: None,
    }
}

pub(super) const ALLOW: StructuredPermissionEffect = StructuredPermissionEffect::Allow;

pub(super) const ASK: StructuredPermissionEffect = StructuredPermissionEffect::Ask;

pub(super) const DENY: StructuredPermissionEffect = StructuredPermissionEffect::Deny;

pub(super) fn decision_over(
    effects: &[StructuredPermissionEffect],
) -> StructuredPermissionDecision {
    let resource = command_resource("cargo test", "/project");
    let request = request(vec![resource.clone()]);
    let rules: Vec<_> = effects
        .iter()
        .map(|effect| rule(&request, effect.clone(), vec![exact_constraint(&resource)]))
        .collect();

    resource_decision(&rules, &request, &resource)
}

pub(super) const BROAD_ASK: &str = "git *";

pub(super) const NARROW_ALLOW: &str = "git status *";

pub(super) const WILDCARD_ONLY: &str = "*";

pub(super) const SUBTREE_DIGEST: &str = "subtree";

pub(super) const NARROW_COMMAND: &str = "git status --short";

pub(super) const BROAD_COMMAND: &str = "git commit -m message";

pub(super) fn pattern_constraint(pattern: &str) -> PermissionResourceConstraint {
    PermissionResourceConstraint {
        kind: PermissionResourceKind::Command,
        selector: PermissionResourceSelector::CommandPattern {
            pattern: pattern.into(),
        },
        access: None,
        protected: None,
        attributes: BTreeMap::new(),
    }
}

pub(super) fn any_command_constraint() -> PermissionResourceConstraint {
    PermissionResourceConstraint {
        selector: PermissionResourceSelector::Any,
        ..pattern_constraint(BROAD_ASK)
    }
}

/// Answers for one command, and proves the answer is the rule set's rather
/// than the emission order's by requiring the reversed set to agree.
pub(super) fn order_independent_command_decision(
    command: &str,
    rules: impl Fn(&PermissionRequest) -> Vec<StructuredPermissionRule>,
) -> StructuredPermissionDecision {
    let resource = command_resource(command, "/project");
    let request = request(vec![resource.clone()]);
    let mut reversed = rules(&request);
    reversed.reverse();

    let decision = resource_decision(&rules(&request), &request, &resource);
    assert_eq!(
        resource_decision(&reversed, &request, &resource),
        decision,
        "reversing the rules changed the decision"
    );
    decision
}

pub(super) const MCP_SERVER: &str = "deepwiki";

pub(super) const OTHER_SERVER: &str = "othersrv";

pub(super) const MINTED_TOOL: &str = "search";

pub(super) const OTHER_TOOL: &str = "fetch";

pub(super) fn mcp_subject(server: &str, tool: &str) -> PermissionSubject {
    PermissionSubject::Mcp {
        server: server.into(),
        authority: server.into(),
        tool: tool.into(),
        contract: MCP_CONTRACT.into(),
    }
}

pub(super) const EXPECT_STRICT_URL: &str = "the value is a strict HTTP(S) URL";

pub(super) const EXPECT_URL_ROOTS: &str = "a strict HTTP(S) URL has subtree roots";

pub(super) const EXPECT_SUBTREE_OPTION: &str = "a webfetch request offers a URL subtree";

pub(super) fn webfetch_request(url: &str) -> PermissionRequest {
    PermissionRequest::from_legacy(
        url.into(),
        ToolKey::native("webfetch"),
        vec![url.into()],
        json!({"url": url}),
        Path::new("/project"),
        false,
    )
}

/// Every rung of the URL ladder, narrowest first, as `(id, shown reach)`.
pub(super) fn url_ladder(request: &PermissionRequest) -> Vec<(&str, &str)> {
    request
        .options
        .iter()
        .filter_map(|option| {
            let group = option.group.as_ref()?;
            (group.key == URL_SUBTREE_OPTION_ID)
                .then_some((option.id.as_str(), group.value.as_str()))
        })
        .collect()
}

pub(super) fn two_command_request() -> PermissionRequest {
    explicit_request(
        PermissionAuthorityProfile::Shell,
        vec![
            command_resource("git status --short", "/project"),
            command_resource("cargo test", "/project"),
        ],
        json!({"command": "multiple"}),
    )
}

pub(super) fn composed(
    request: &PermissionRequest,
    rows: Vec<Option<PermissionRowGrant>>,
) -> Result<Vec<StructuredPermissionRule>, ComposedAnswerError> {
    request.composed_rules(&remembered(rows))
}

/// Every granted row remembered for this conversation.
pub(super) fn remembered(rows: Vec<Option<PermissionRowGrant>>) -> Vec<Option<ComposedRow>> {
    ComposedRow::uniform(rows, &PermissionLifetime::Conversation)
}

pub(super) const SHARED_PATTERN_COMMANDS: [&str; 2] =
    ["git status --short", "git status --porcelain"];

pub(super) const BUILTIN_ALLOW_AUTHORITY: &str = "echo *";

/// Two commands one pattern reaches, which is how a pipeline that greps
/// twice arrives at the prompt.
pub(super) fn twin_command_request() -> PermissionRequest {
    explicit_request(
        PermissionAuthorityProfile::Shell,
        SHARED_PATTERN_COMMANDS
            .iter()
            .map(|command| command_resource(command, "/project"))
            .collect(),
        json!({"command": "twice"}),
    )
}

pub(super) fn offered(id: &str) -> Option<PermissionRowGrant> {
    Some(PermissionRowGrant::Offered(id.into()))
}

pub(super) fn covered_at(
    request: &mut PermissionRequest,
    index: usize,
    origin: RuleOrigin,
    authority: &str,
) {
    request.presentation.resources[index].coverage = Some(ResourceCoverage {
        origin,
        authority: authority.into(),
        asks: false,
    });
}

pub(super) const EXACT_RESOURCES_OPTION: &str = "allow_exact_resources";

pub(super) const SUBTREE_OPTION: &str = "allow_filesystem_subtree";

/// The first rung above the resource's own directory. For a path one level
/// inside the project that rung is the project root itself.
pub(super) const PROJECT_RUNG: &str = "allow_filesystem_subtree_1";

pub(super) const PROJECT_ROOT_MARK: &str = "(project root)";

pub(super) const PROTECTED_PATH: &str = "/project/.env";

pub(super) const FIRST_READ_OFFSET: u32 = 1;

pub(super) const LATER_READ_OFFSET: u32 = 500;

pub(super) fn filesystem_request(protected: bool, offset: u32) -> PermissionRequest {
    explicit_request(
        PermissionAuthorityProfile::Filesystem {
            input_pointers: Vec::new(),
        },
        vec![PermissionResource {
            kind: PermissionResourceKind::File,
            value: PROTECTED_PATH.into(),
            access: Some(PermissionResourceAccess::Read),
            protected,
            requires_prompt: protected,
            attributes: BTreeMap::new(),
        }],
        json!({"path": PROTECTED_PATH, "offset": offset}),
    )
}

pub(super) fn option_ids(request: &PermissionRequest) -> Vec<&str> {
    request
        .options
        .iter()
        .map(|option| option.id.as_str())
        .collect()
}

pub(super) const PROJECT_ROOT: &str = "/project";

pub(super) fn flags_in_project(path: &str, access: PermissionResourceAccess) -> (bool, bool) {
    filesystem_resource_flags(path, &access, Path::new(PROJECT_ROOT))
}

pub(super) const READ_CONTRACT: &str = "file.read.v1";

pub(super) const GREP_CONTRACT: &str = "file.grep.v1";

pub(super) const WRITE_CONTRACT: &str = "file.write.v1";

pub(super) const SOURCE_FILE: &str = "/project/src/main.rs";

pub(super) const SOURCE_DIR: &str = "/project/src";

pub(super) fn workcell_request(
    contract: &str,
    kind: PermissionResourceKind,
    access: PermissionResourceAccess,
    value: &str,
) -> PermissionRequest {
    let intent = PermissionIntent::new(
        PermissionScopes::single(value.to_owned()),
        vec![PermissionResource {
            kind,
            value: value.into(),
            access: Some(access),
            protected: false,
            requires_prompt: false,
            attributes: BTreeMap::new(),
        }],
        PermissionRisk::Medium,
    )
    .with_authority(PermissionAuthorityProfile::Filesystem {
        input_pointers: Vec::new(),
    });
    PermissionRequest::from_intent_with_identity(
        "request".into(),
        ToolKey::native("workcell_file_tool"),
        &intent,
        json!({ "path": value }),
        Path::new("/project"),
        PermissionSubject::Native {
            owner: WORKCELL_OWNER.into(),
            contract: contract.into(),
        },
        PermissionExecutorKind::Native,
    )
}

pub(super) fn read_subtree_rule(option: &str) -> StructuredPermissionRule {
    workcell_request(
        READ_CONTRACT,
        PermissionResourceKind::File,
        PermissionResourceAccess::Read,
        SOURCE_FILE,
    )
    .option_rule(option, PermissionLifetime::Conversation)
    .expect("a first-party read must offer a subtree grant")
}

/// The subtree rungs of the filesystem ladder in the order they were offered,
/// without the exact paths it starts on.
pub(super) fn subtree_ladder(request: &PermissionRequest) -> Vec<&PermissionRuleOption> {
    request
        .options
        .iter()
        .filter(|option| {
            option.id.starts_with(SUBTREE_OPTION)
                && option
                    .group
                    .as_ref()
                    .is_some_and(|group| group.key == SUBTREE_OPTION)
        })
        .collect()
}

pub(super) fn ladder_values(request: &PermissionRequest) -> Vec<String> {
    subtree_ladder(request)
        .iter()
        .map(|option| {
            option
                .group
                .as_ref()
                .expect("a rung is grouped")
                .value
                .clone()
        })
        .collect()
}

pub(super) fn caution_of(root: &Path, resource: &Path) -> Vec<Option<PermissionCaution>> {
    let value = resource.to_string_lossy().into_owned();
    let intent = PermissionIntent::new(
        PermissionScopes::single(value.clone()),
        vec![PermissionResource {
            kind: PermissionResourceKind::File,
            value: value.clone(),
            access: Some(PermissionResourceAccess::Read),
            protected: false,
            requires_prompt: false,
            attributes: BTreeMap::new(),
        }],
        PermissionRisk::Medium,
    )
    .with_authority(PermissionAuthorityProfile::Filesystem {
        input_pointers: Vec::new(),
    });
    let request = PermissionRequest::from_intent_with_identity(
        "request".into(),
        ToolKey::native("workcell_file_tool"),
        &intent,
        json!({ "path": value }),
        root,
        PermissionSubject::Native {
            owner: WORKCELL_OWNER.into(),
            contract: READ_CONTRACT.into(),
        },
        PermissionExecutorKind::Native,
    );
    subtree_ladder(&request)
        .iter()
        .map(|option| option.caution)
        .collect()
}
