use super::{
    PermissionPresentation, PermissionResource, PermissionResourceKind,
    PermissionResourcePresentation, PermissionRisk, ResourceCoverage, strict_http_url,
};
use caudra_config::ToolKey;

pub(super) const SUMMARY_MAX_CHARS: usize = 240;

pub(super) const LISTED_COMMANDS_MAX: usize = 3;

pub(super) fn presentation_for(
    tool: &ToolKey,
    risk: &PermissionRisk,
    resources: &[PermissionResource],
) -> PermissionPresentation {
    let action = match tool {
        ToolKey::McpTool { server, tool } => {
            format!(
                "Call MCP tool {} from {}",
                safe_summary(tool),
                safe_summary(server)
            )
        }
        ToolKey::McpServer { server } => {
            format!("Call an MCP tool from {}", safe_summary(server))
        }
        ToolKey::Native(name) => format!("Run native tool {}", safe_summary(name)),
        ToolKey::Wildcard => "Run an unknown legacy tool".into(),
    };
    let risk_summary = match risk {
        PermissionRisk::Low => "Limited read or lookup operation",
        PermissionRisk::Medium => "External read or network operation",
        PermissionRisk::High => "May modify data, execute code, or invoke an external authority",
        PermissionRisk::Critical => "Complex or protected operation requiring exact review",
        PermissionRisk::Unknown => "Legacy operation with unknown effects",
    }
    .into();
    PermissionPresentation {
        action,
        risk: risk.clone(),
        risk_summary,
        project: None,
        resources: resources
            .iter()
            .map(|resource| {
                let mut summary = if matches!(
                    resource.kind,
                    PermissionResourceKind::RemoteFile { .. }
                        | PermissionResourceKind::RemoteDirectory { .. }
                        | PermissionResourceKind::RemoteResource { .. }
                ) {
                    resource
                        .attributes
                        .iter()
                        .find(|(name, _)| name.starts_with("display_"))
                        .map(|(_, value)| value)
                        .map_or_else(|| "remote resource".into(), |path| safe_summary(path))
                } else if resource.kind == PermissionResourceKind::Url {
                    redacted_url_summary(&resource.value)
                } else {
                    safe_summary(&resource.value)
                };
                if let Some(workdir) = resource.attributes.get("workdir") {
                    summary.push_str(" in ");
                    summary.push_str(&safe_summary(workdir));
                }
                PermissionResourcePresentation {
                    kind: resource.kind.clone(),
                    access: resource.access.clone(),
                    summary,
                    protected: resource.protected,
                    coverage: None,
                }
            })
            .collect(),
    }
}

pub fn update_presentation_coverage(
    presentation: &mut PermissionPresentation,
    coverage: &[Option<ResourceCoverage>],
) -> bool {
    if presentation.resources.len() != coverage.len() {
        return false;
    }
    for (resource, coverage) in presentation.resources.iter_mut().zip(coverage) {
        resource.coverage.clone_from(coverage);
    }
    true
}

pub(super) fn redacted_url_summary(value: &str) -> String {
    let Some(strict) = strict_http_url(value) else {
        return safe_summary(value);
    };
    let mut url = strict.url;
    if url.query().is_some() {
        let query = url
            .query_pairs()
            .map(|(key, _)| format!("{key}=<redacted>"))
            .collect::<Vec<_>>()
            .join("&");
        url.set_query(Some(&query));
    }
    safe_summary(url.as_str())
}

pub(super) fn safe_summary(value: &str) -> String {
    let mut output = String::new();
    let mut truncated = false;
    for (index, character) in value.chars().enumerate() {
        if index == SUMMARY_MAX_CHARS {
            truncated = true;
            break;
        }
        if character.is_control() {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    if truncated {
        output.push_str("...");
    }
    output
}

/// Renders reviewed commands as a backtick-quoted list, capped for readability.
pub(super) fn listed_commands<'a>(commands: impl ExactSizeIterator<Item = &'a str>) -> String {
    let total = commands.len();
    let listed = commands
        .take(LISTED_COMMANDS_MAX)
        .map(|command| format!("`{}`", safe_summary(command)))
        .collect::<Vec<_>>()
        .join(", ");
    if total > LISTED_COMMANDS_MAX {
        format!("{listed}, +{} more", total - LISTED_COMMANDS_MAX)
    } else {
        listed
    }
}

#[cfg(test)]
mod tests {
    use super::presentation_for;
    use caudra_config::ToolKey;
    use serde_json::json;
    use std::path::PathBuf;
    use test_case::test_case;

    use crate::permissions::structured::tests::NARROW_ALLOW;
    use crate::permissions::structured::{
        PermissionPresentation, PermissionResourceAccess, PermissionResourceKind,
        PermissionResourcePresentation, PermissionRisk, ResourceCoverage, RuleOrigin,
        listed_commands, update_presentation_coverage,
    };

    #[test_case(None; "legacy_missing_project")]
    #[test_case(Some("/reviewed/project"); "project_round_trip")]
    fn presentation_project_is_optional_display_context(project: Option<&str>) {
        let mut presentation =
            presentation_for(&ToolKey::native("bash"), &PermissionRisk::High, &[]);
        assert!(presentation.project.is_none());
        presentation.project = project.map(PathBuf::from);
        let serialized = serde_json::to_value(&presentation).unwrap();
        assert_eq!(
            serialized.get("project"),
            project.map(|path| json!(path)).as_ref()
        );
        let restored: PermissionPresentation = serde_json::from_value(serialized).unwrap();
        assert_eq!(restored, presentation);
        assert!(update_presentation_coverage(&mut presentation, &[]));
        assert_eq!(restored.project, presentation.project);
    }
    #[test]
    fn listed_commands_cap_the_enumeration_and_escape_control_characters() {
        let commands = ["one", "two", "three", "four\nfive"];

        assert_eq!(
            listed_commands(commands.iter().copied()),
            "`one`, `two`, `three`, +1 more"
        );
        assert_eq!(
            listed_commands(commands[3..].iter().copied()),
            r"`four\nfive`"
        );
    }

    #[test]
    fn presentation_coverage_defaults_absent_and_updates_atomically() {
        let resource = PermissionResourcePresentation {
            kind: PermissionResourceKind::Command,
            access: Some(PermissionResourceAccess::Execute),
            summary: "git status".into(),
            protected: false,
            coverage: None,
        };
        let serialized = serde_json::to_value(&resource).unwrap();
        assert!(serialized.get("coverage").is_none());
        let restored: PermissionResourcePresentation = serde_json::from_value(serialized).unwrap();
        assert!(!restored.covered());

        let mut presentation = PermissionPresentation {
            action: "Run commands".into(),
            risk: PermissionRisk::High,
            risk_summary: "Shell execution".into(),
            resources: vec![resource.clone(), resource],
            project: None,
        };
        let granted = ResourceCoverage {
            origin: RuleOrigin::Project,
            authority: NARROW_ALLOW.into(),
        };
        assert!(update_presentation_coverage(
            &mut presentation,
            &[Some(granted), None]
        ));
        assert!(presentation.resources[0].covered());
        assert!(!presentation.resources[1].covered());

        let unchanged = presentation.clone();
        assert!(!update_presentation_coverage(&mut presentation, &[None]));
        assert_eq!(presentation, unchanged);
        assert_eq!(
            serde_json::to_value(&presentation.resources[0]).unwrap()["coverage"],
            json!({"origin": "project", "authority": NARROW_ALLOW})
        );
    }
}
