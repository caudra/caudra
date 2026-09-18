use super::COMMAND_OBSERVATION_BINDING_ATTRIBUTE;
use super::{
    COMMAND_OBSERVATION_ATTRIBUTE, DIRECTORY_READ_TOOLS, FILE_READ_TOOLS, FILE_SEARCH_TOOLS,
    MCP_CONTRACT, NATIVE_OWNER, NORMALIZED_COMMAND_ATTRIBUTE, PermissionAuthorityProfile,
    PermissionExecutorKind, PermissionResource, PermissionResourceAccess,
    PermissionResourceConstraint, PermissionResourceKind, PermissionResourceSelector,
    PermissionRisk, PermissionSubject, RemotePermissionIdentity, WORKDIR_ATTRIBUTE,
    canonical_json_sha256, normalized_filesystem_path, remote_scope, resource_value_digest,
    strict_http_url,
};
use caudra_config::{FILE_WRITE_TOOLS, ToolKey};
use caudra_storage::permission_state::REVIEW_MAX_STRING_BYTES;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub(super) const GIT_METADATA_DIR: &str = ".git";
const MAX_POSSIBLE_WORKDIRS: usize = 32;
pub(super) const INERT_GIT_METADATA: &[&str] = &[
    "COMMIT_EDITMSG",
    "FETCH_HEAD",
    "HEAD",
    "MERGE_HEAD",
    "MERGE_MSG",
    "ORIG_HEAD",
    "index",
    "logs",
    "objects",
    "packed-refs",
    "refs",
];

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    content = "symbolic_paths",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(super) enum PreparedWorkdirs {
    Known(Vec<String>),
    Unknown,
}

impl PreparedWorkdirs {
    pub(super) fn parse(value: &str) -> Option<Self> {
        if value.len() > REVIEW_MAX_STRING_BYTES {
            return None;
        }
        let parsed: Self = serde_json::from_str(value).ok()?;
        if let Self::Known(paths) = &parsed
            && (paths.is_empty()
                || paths.len() > MAX_POSSIBLE_WORKDIRS
                || paths
                    .iter()
                    .any(|path| normalized_filesystem_path(path).is_none()))
        {
            return None;
        }
        Some(parsed)
    }
}

pub(super) fn subject_and_executor(tool: &ToolKey) -> (PermissionSubject, PermissionExecutorKind) {
    match tool {
        ToolKey::Native(name) => (
            PermissionSubject::Native {
                owner: NATIVE_OWNER.into(),
                contract: name.to_string(),
            },
            PermissionExecutorKind::Native,
        ),
        ToolKey::McpTool { server, tool } => (
            PermissionSubject::Mcp {
                server: server.to_string(),
                authority: server.to_string(),
                tool: tool.to_string(),
                contract: MCP_CONTRACT.into(),
            },
            PermissionExecutorKind::Mcp,
        ),
        ToolKey::McpServer { server } => (
            PermissionSubject::Mcp {
                server: server.to_string(),
                authority: server.to_string(),
                tool: "*".into(),
                contract: MCP_CONTRACT.into(),
            },
            PermissionExecutorKind::Mcp,
        ),
        ToolKey::Wildcard => (
            PermissionSubject::UnknownLegacy {
                identity: tool.to_string(),
            },
            PermissionExecutorKind::UnknownLegacy,
        ),
    }
}

pub(super) fn risk_for(tool: &ToolKey, force_prompt: bool) -> PermissionRisk {
    if force_prompt {
        return PermissionRisk::Critical;
    }
    match tool {
        ToolKey::McpTool { .. } | ToolKey::McpServer { .. } => PermissionRisk::High,
        ToolKey::Native(name) if FILE_WRITE_TOOLS.contains(&name.as_ref()) => PermissionRisk::High,
        ToolKey::Native(name) if name.as_ref() == "bash" => PermissionRisk::High,
        ToolKey::Native(name) if name.as_ref() == "webfetch" => PermissionRisk::Medium,
        ToolKey::Native(name) if name.as_ref() == "websearch" => PermissionRisk::Low,
        ToolKey::Native(name)
            if FILE_READ_TOOLS.contains(&name.as_ref())
                || DIRECTORY_READ_TOOLS.contains(&name.as_ref())
                || FILE_SEARCH_TOOLS.contains(&name.as_ref()) =>
        {
            PermissionRisk::Low
        }
        ToolKey::Native(_) | ToolKey::Wildcard => PermissionRisk::Unknown,
    }
}

pub(super) fn legacy_authority_profile(tool: &ToolKey) -> PermissionAuthorityProfile {
    match tool {
        ToolKey::Native(name) if name.as_ref() == "webfetch" => PermissionAuthorityProfile::Url,
        ToolKey::Native(name) if name.as_ref() == "websearch" => PermissionAuthorityProfile::Query,
        ToolKey::Native(name) if name.as_ref() == "bash" => PermissionAuthorityProfile::Shell,
        ToolKey::Native(name) if FILE_SEARCH_TOOLS.contains(&name.as_ref()) => {
            PermissionAuthorityProfile::Filesystem {
                input_pointers: vec!["/pattern".into()],
            }
        }
        ToolKey::Native(name)
            if FILE_WRITE_TOOLS.contains(&name.as_ref())
                || FILE_READ_TOOLS.contains(&name.as_ref())
                || DIRECTORY_READ_TOOLS.contains(&name.as_ref()) =>
        {
            PermissionAuthorityProfile::Filesystem {
                input_pointers: Vec::new(),
            }
        }
        _ => PermissionAuthorityProfile::ExactOnly,
    }
}

pub(super) fn resources_for(
    tool: &ToolKey,
    scopes: &[String],
    input: &Value,
    cwd: &Path,
    force_prompt: bool,
) -> Vec<PermissionResource> {
    match tool {
        ToolKey::Native(name) if FILE_WRITE_TOOLS.contains(&name.as_ref()) => scopes
            .iter()
            .map(|scope| {
                filesystem_resource(
                    PermissionResourceKind::File,
                    scope,
                    PermissionResourceAccess::Write,
                    cwd,
                )
            })
            .collect(),
        ToolKey::Native(name) if name.as_ref() == "bash" => {
            let default_workdir = input
                .get("workdir")
                .and_then(Value::as_str)
                .and_then(normalized_filesystem_path)
                .unwrap_or_else(|| cwd.to_path_buf())
                .to_string_lossy()
                .into_owned();
            scopes
                .iter()
                .map(|scope| {
                    let (command, workdir) = crate::permissions::bash_scope_parts(scope)
                        .map(|(command, workdir)| (command.to_owned(), workdir.to_owned()))
                        .unwrap_or_else(|| (scope.clone(), default_workdir.clone()));
                    PermissionResource {
                        kind: PermissionResourceKind::Command,
                        value: command,
                        access: Some(PermissionResourceAccess::Execute),
                        protected: force_prompt,
                        requires_prompt: force_prompt,
                        attributes: BTreeMap::from([("workdir".into(), workdir)]),
                    }
                })
                .collect()
        }
        ToolKey::Native(name) if name.as_ref() == "webfetch" => scopes
            .iter()
            .map(|scope| PermissionResource {
                kind: PermissionResourceKind::Url,
                value: strict_http_url(scope)
                    .map(|strict| strict.key)
                    .unwrap_or_else(|| scope.clone()),
                access: Some(PermissionResourceAccess::Read),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            })
            .collect(),
        ToolKey::Native(name) if name.as_ref() == "websearch" => scopes
            .iter()
            .map(|scope| PermissionResource {
                kind: PermissionResourceKind::Query,
                value: scope.clone(),
                access: Some(PermissionResourceAccess::Search),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            })
            .collect(),
        ToolKey::Native(name) if FILE_READ_TOOLS.contains(&name.as_ref()) => scopes
            .iter()
            .map(|scope| {
                let directory = name.as_ref() == "file_index" && Path::new(scope).is_dir();
                filesystem_resource(
                    if directory {
                        PermissionResourceKind::Directory
                    } else {
                        PermissionResourceKind::File
                    },
                    scope,
                    PermissionResourceAccess::Read,
                    cwd,
                )
            })
            .collect(),
        ToolKey::Native(name) if DIRECTORY_READ_TOOLS.contains(&name.as_ref()) => scopes
            .iter()
            .map(|scope| {
                filesystem_resource(
                    PermissionResourceKind::Directory,
                    scope,
                    PermissionResourceAccess::Read,
                    cwd,
                )
            })
            .collect(),
        ToolKey::Native(name) if FILE_SEARCH_TOOLS.contains(&name.as_ref()) => scopes
            .iter()
            .map(|scope| {
                let root = scope.strip_suffix("/**").unwrap_or(scope);
                filesystem_resource(
                    PermissionResourceKind::Directory,
                    root,
                    PermissionResourceAccess::Search,
                    cwd,
                )
            })
            .collect(),
        ToolKey::Native(name) if name.as_ref() == "memory" => {
            let write = matches!(
                input.get("command").and_then(Value::as_str),
                Some("write" | "delete")
            );
            scopes
                .iter()
                .map(|scope| {
                    let subtree = scope.strip_suffix("/**");
                    PermissionResource {
                        kind: if subtree.is_some() {
                            PermissionResourceKind::Directory
                        } else {
                            PermissionResourceKind::File
                        },
                        value: subtree.unwrap_or(scope).to_owned(),
                        access: Some(if write {
                            PermissionResourceAccess::Write
                        } else {
                            PermissionResourceAccess::Read
                        }),
                        protected: false,
                        requires_prompt: false,
                        attributes: BTreeMap::new(),
                    }
                })
                .collect()
        }
        ToolKey::Native(name) if name.as_ref() == "skill" => scopes
            .iter()
            .map(|scope| {
                let subtree = scope.strip_suffix("/**");
                PermissionResource {
                    kind: if subtree.is_some() {
                        PermissionResourceKind::Directory
                    } else {
                        PermissionResourceKind::File
                    },
                    value: subtree.unwrap_or(scope).to_owned(),
                    access: Some(PermissionResourceAccess::Read),
                    protected: false,
                    requires_prompt: false,
                    attributes: BTreeMap::new(),
                }
            })
            .collect(),
        ToolKey::McpTool { .. } | ToolKey::McpServer { .. } => vec![PermissionResource {
            kind: PermissionResourceKind::Custom {
                name: "mcp_tool".into(),
            },
            value: tool.to_string(),
            access: Some(PermissionResourceAccess::Execute),
            protected: false,
            requires_prompt: false,
            attributes: BTreeMap::new(),
        }],
        _ => scopes
            .iter()
            .map(|scope| PermissionResource {
                kind: PermissionResourceKind::Custom {
                    name: tool.to_string(),
                },
                value: scope.clone(),
                access: Some(PermissionResourceAccess::Execute),
                protected: force_prompt,
                requires_prompt: force_prompt,
                attributes: BTreeMap::new(),
            })
            .collect(),
    }
}

pub(super) fn filesystem_resource(
    kind: PermissionResourceKind,
    value: &str,
    access: PermissionResourceAccess,
    cwd: &Path,
) -> PermissionResource {
    let (protected, requires_prompt) = filesystem_resource_flags(value, &access, cwd);
    PermissionResource {
        kind,
        value: value.to_owned(),
        access: Some(access),
        protected,
        requires_prompt,
        attributes: BTreeMap::new(),
    }
}

pub fn filesystem_permission_resource(
    kind: PermissionResourceKind,
    path: &Path,
    access: PermissionResourceAccess,
    cwd: &Path,
) -> PermissionResource {
    filesystem_resource(kind, &path.to_string_lossy(), access, cwd)
}

pub(super) fn filesystem_resource_flags(
    value: &str,
    access: &PermissionResourceAccess,
    cwd: &Path,
) -> (bool, bool) {
    let Some(value) = normalized_filesystem_path(value) else {
        return (true, true);
    };
    let project = normalized_filesystem_path(&cwd.to_string_lossy());
    if matches!(
        access,
        PermissionResourceAccess::Read
            | PermissionResourceAccess::Search
            | PermissionResourceAccess::List
    ) && project
        .as_deref()
        .is_some_and(|project| is_inert_git_metadata(&value, project))
    {
        return (false, false);
    }
    let outside_project = project
        .as_deref()
        .is_none_or(|project| value != project && !value.starts_with(project));
    let protected = value.components().any(|component| {
        let component = component.as_os_str().to_string_lossy();
        matches!(component.as_ref(), GIT_METADATA_DIR | ".ssh" | ".aws")
            || component == ".env"
            || component.starts_with(".env.")
    });
    (protected, protected || outside_project)
}

/// Reports whether a path is the project's own inert git bookkeeping.
///
/// Reading these reveals no secret and changes no state, so they are exempt
/// from the `.git` guard that otherwise forces a prompt on every path holding a
/// credential-bearing component. `config` and `hooks` are deliberately absent:
/// remote URLs in `config` embed tokens, and hooks are executable. Anything
/// unrecognized stays protected, so a new git file is guarded until reviewed.
pub(super) fn is_inert_git_metadata(path: &Path, project: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(project) else {
        return false;
    };
    let mut components = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned());
    components.next().as_deref() == Some(GIT_METADATA_DIR)
        && components
            .next()
            .is_some_and(|entry| INERT_GIT_METADATA.contains(&entry.as_str()))
}

/// The digest a selector pins a value with, falling back to hashing the value
/// as text when the kind has no canonical form of its own.
pub(super) fn pinned_digest(value: &str, kind: &PermissionResourceKind) -> String {
    resource_value_digest(value, kind)
        .unwrap_or_else(|| canonical_json_sha256(&Value::String(value.to_owned())))
}

/// The kind an attribute's value is matched as. One definition, so building a
/// constraint and testing one agree on what `workdir` means.
pub(super) fn attribute_kind(name: &str) -> PermissionResourceKind {
    if name == WORKDIR_ATTRIBUTE {
        PermissionResourceKind::Directory
    } else if name == NORMALIZED_COMMAND_ATTRIBUTE {
        PermissionResourceKind::Command
    } else {
        PermissionResourceKind::Custom {
            name: name.to_owned(),
        }
    }
}

/// The constraint that pins one resource to itself, attributes included.
///
/// Widening a single resource means taking this and replacing its selector, so
/// option generation and answer validation both start here.
pub(super) fn resource_constraint(resource: &PermissionResource) -> PermissionResourceConstraint {
    if let Some(identity) = remote_resource_identity(&resource.kind)
        && let Some(scope) = remote_scope(&resource.value)
    {
        return PermissionResourceConstraint {
            kind: resource.kind.clone(),
            selector: PermissionResourceSelector::RemoteResource {
                identity: identity.clone(),
                scope,
            },
            access: resource.access.clone(),
            protected: Some(resource.protected),
            attributes: BTreeMap::new(),
        };
    }
    PermissionResourceConstraint {
        kind: resource.kind.clone(),
        selector: PermissionResourceSelector::Digest {
            digest: pinned_digest(&resource.value, &resource.kind),
        },
        access: resource.access.clone(),
        protected: Some(resource.protected),
        attributes: resource
            .attributes
            .iter()
            .filter(|(name, _)| {
                !matches!(
                    name.as_str(),
                    NORMALIZED_COMMAND_ATTRIBUTE
                        | COMMAND_OBSERVATION_ATTRIBUTE
                        | COMMAND_OBSERVATION_BINDING_ATTRIBUTE
                )
            })
            .map(|(name, value)| {
                (
                    name.clone(),
                    PermissionResourceSelector::Digest {
                        digest: pinned_digest(value, &attribute_kind(name)),
                    },
                )
            })
            .collect(),
    }
}

pub(super) fn remote_resource_identity(
    kind: &PermissionResourceKind,
) -> Option<&RemotePermissionIdentity> {
    match kind {
        PermissionResourceKind::RemoteFile { identity }
        | PermissionResourceKind::RemoteDirectory { identity }
        | PermissionResourceKind::RemoteResource { identity, .. } => Some(identity),
        _ => None,
    }
}

pub(super) fn reusable_remote_resource_constraint(
    resource: &PermissionResource,
) -> PermissionResourceConstraint {
    let mut constraint = resource_constraint(resource);
    if matches!(
        resource.kind,
        PermissionResourceKind::RemoteDirectory { .. }
    ) && let PermissionResourceSelector::RemoteResource { identity, scope } =
        &constraint.selector
    {
        constraint.selector = PermissionResourceSelector::RemoteSubtree {
            identity: identity.clone(),
            scope: scope.clone(),
        };
    }
    constraint
}

pub(super) fn exact_resource_constraints(
    resources: &[PermissionResource],
) -> Vec<PermissionResourceConstraint> {
    resources.iter().map(resource_constraint).collect()
}

#[cfg(test)]
mod tests {

    use serde_json::json;

    use test_case::test_case;

    use crate::permissions::structured::tests::{
        EXACT_RESOURCES_OPTION, FIRST_READ_OFFSET, LATER_READ_OFFSET, PROJECT_ROOT_MARK,
        SUBTREE_OPTION, command_resource, custom_resource, exact_constraint, explicit_request,
        filesystem_request, option_ids, protected_command_resource,
    };
    use crate::permissions::structured::{
        PermissionArgumentConstraint, PermissionAuthorityProfile, PermissionLifetime,
        PermissionResource, PermissionResourceAccess, PermissionResourceConstraint,
        PermissionResourceKind, PermissionResourceSelector, PermissionSubject,
        exact_resource_constraints, permission_rule_covers_request, resource_constraint_matches,
    };
    use std::collections::BTreeMap;
    #[test]
    fn native_contract_identity_is_strict_for_explicit_intents() {
        let request = explicit_request(
            PermissionAuthorityProfile::ExactOnly,
            vec![custom_resource("opaque")],
            json!({"value": "opaque"}),
        );
        let rule = request
            .option_rule("allow_exact", PermissionLifetime::Conversation)
            .unwrap();
        let mut other_contract = request.clone();
        other_contract.subject = PermissionSubject::Native {
            owner: "first-party".into(),
            contract: "platform/v2".into(),
        };

        assert!(permission_rule_covers_request(&rule, &request));
        assert!(!permission_rule_covers_request(&rule, &other_contract));
    }

    #[test]
    fn protected_resources_require_an_explicit_exact_selector() {
        let mut resource = custom_resource("secret");
        resource.protected = true;
        let broad = PermissionResourceConstraint {
            kind: resource.kind.clone(),
            selector: PermissionResourceSelector::Any,
            access: resource.access.clone(),
            protected: Some(true),
            attributes: BTreeMap::new(),
        };
        assert!(!resource_constraint_matches(&broad, &resource));
        assert!(resource_constraint_matches(
            &exact_constraint(&resource),
            &resource
        ));
    }

    #[test]
    fn protected_command_does_not_treat_a_command_pattern_as_exact() {
        let mut resource = command_resource("git diff --stat", "/project");
        resource.protected = true;
        let mut pattern = exact_resource_constraints(std::slice::from_ref(&resource))
            .pop()
            .unwrap();
        pattern.selector = PermissionResourceSelector::CommandPattern {
            pattern: "git diff *".into(),
        };

        assert!(!resource_constraint_matches(&pattern, &resource));
    }

    #[test]
    fn protected_commands_offer_only_exact_and_blanket_authorities() {
        let request = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![protected_command_resource(
                "git status > /tmp/status",
                "/project",
            )],
            json!({"command": "git status > /tmp/status"}),
        );

        assert_eq!(
            request
                .options
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            [
                "allow_exact",
                "deny_exact",
                "allow_commands_in_workdir",
                "allow_any_command",
            ]
        );
    }

    #[test_case("allow_commands_in_workdir", true; "workdir_authority")]
    #[test_case("allow_any_command", true; "global_authority")]
    #[test_case("allow_command_patterns", false; "pattern_authority")]
    #[test_case("allow_exact_commands", false; "exact_command_authority")]
    fn broad_shell_authority_reaches_protected_commands(option_id: &str, covers: bool) {
        let reviewed = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![command_resource("git status --short", "/project")],
            json!({"command": "git status --short"}),
        );
        let rule = reviewed
            .options
            .iter()
            .find(|option| option.id == option_id)
            .expect("shell authority option")
            .rule
            .clone();
        let protected = explicit_request(
            PermissionAuthorityProfile::Shell,
            vec![protected_command_resource(
                "git status > /tmp/out",
                "/project",
            )],
            json!({"command": "git status > /tmp/out"}),
        );

        assert_eq!(permission_rule_covers_request(&rule, &protected), covers);
    }

    #[test_case(PermissionResourceKind::Command, true; "protected_command")]
    #[test_case(PermissionResourceKind::File, false; "protected_file")]
    #[test_case(PermissionResourceKind::Directory, false; "protected_directory")]
    fn blanket_selectors_reach_protected_commands_only(
        kind: PermissionResourceKind,
        matches: bool,
    ) {
        let constraint = PermissionResourceConstraint {
            kind: kind.clone(),
            selector: PermissionResourceSelector::Any,
            access: Some(PermissionResourceAccess::Execute),
            protected: None,
            attributes: BTreeMap::new(),
        };
        let resource = PermissionResource {
            kind,
            value: "/etc/shadow".into(),
            access: Some(PermissionResourceAccess::Execute),
            protected: true,
            requires_prompt: true,
            attributes: BTreeMap::new(),
        };

        assert_eq!(resource_constraint_matches(&constraint, &resource), matches);
    }

    #[test]
    fn a_protected_path_is_offered_a_reusable_exact_path_grant() {
        let request = filesystem_request(true, FIRST_READ_OFFSET);

        let option = request
            .options
            .iter()
            .find(|option| option.id == EXACT_RESOURCES_OPTION)
            .expect("protected path must still earn an exact-path option");

        assert!(matches!(
            option.rule.arguments,
            PermissionArgumentConstraint::Unconstrained
        ));
        assert_eq!(option.rule.resources[0].protected, Some(true));
        assert!(
            option
                .allowed_lifetimes
                .contains(&PermissionLifetime::Conversation)
        );
    }

    #[test]
    fn a_protected_path_is_never_offered_a_subtree_grant() {
        let request = filesystem_request(true, FIRST_READ_OFFSET);
        let ids = option_ids(&request);

        assert!(
            !ids.iter().any(|id| id.starts_with(SUBTREE_OPTION)),
            "{ids:?}"
        );
    }

    #[test]
    fn an_unprotected_path_still_earns_every_filesystem_grant() {
        let request = filesystem_request(false, FIRST_READ_OFFSET);
        let ids = option_ids(&request);

        assert!(ids.contains(&EXACT_RESOURCES_OPTION), "{ids:?}");
        assert!(ids.contains(&SUBTREE_OPTION), "{ids:?}");
        assert!(
            request.options.iter().any(|option| option
                .group
                .as_ref()
                .is_some_and(|group| group.value.contains(PROJECT_ROOT_MARK))),
            "{ids:?}"
        );
    }

    #[test]
    fn a_reusable_protected_grant_covers_the_same_path_under_a_different_input() {
        let granted = filesystem_request(true, FIRST_READ_OFFSET);
        let rule = granted
            .option_rule(EXACT_RESOURCES_OPTION, PermissionLifetime::Conversation)
            .expect("protected path must still earn an exact-path option");

        let repeat = filesystem_request(true, LATER_READ_OFFSET);

        assert_ne!(granted.input_digest, repeat.input_digest);
        assert!(permission_rule_covers_request(&rule, &repeat));
    }
}
