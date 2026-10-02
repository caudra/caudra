use super::{
    PermissionAdvisory, PermissionPresentation, PermissionResource, PermissionResourceKind,
    PermissionResourcePresentation, PermissionRisk, ResourceCoverage, RuleOrigin, ShellOpacity,
    strict_http_url,
};
use caudra_config::ToolKey;
use serde::{Deserialize, Serialize};

pub(super) const SUMMARY_MAX_CHARS: usize = 240;

pub(super) const LISTED_COMMANDS_MAX: usize = 3;

const PERCENT: f64 = 100.0;

/// A caution the decision engine can raise about a call. The wire carries the
/// snake_case name, so every client reads the same fixed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineFlag {
    Deletes,
    Uploads,
    Credentials,
    Permissions,
    RemoteRewrite,
    OffTask,
    WritesProjectFiles,
}

impl EngineFlag {
    pub const ALL: [Self; 7] = [
        Self::Deletes,
        Self::Uploads,
        Self::Credentials,
        Self::Permissions,
        Self::RemoteRewrite,
        Self::OffTask,
        Self::WritesProjectFiles,
    ];

    /// The question id the engine answers for this flag.
    pub fn name(self) -> &'static str {
        match self {
            Self::Deletes => "deletes",
            Self::Uploads => "uploads",
            Self::Credentials => "credentials",
            Self::Permissions => "permissions",
            Self::RemoteRewrite => "remote_rewrite",
            Self::OffTask => "off_task",
            Self::WritesProjectFiles => "writes_project_files",
        }
    }

    pub fn caution(self) -> &'static str {
        match self {
            Self::Deletes => "May delete files",
            Self::Uploads => "May upload or send data",
            Self::Credentials => "May read or use credentials",
            Self::Permissions => "May change file permissions",
            Self::RemoteRewrite => "May rewrite remote history",
            Self::OffTask => "Looks unrelated to the task",
            Self::WritesProjectFiles => "May change project files",
        }
    }
}

impl PermissionAdvisory {
    /// The caution with its estimate as a whole percentage, such as
    /// `May delete files (87%)`, or `None` when the estimate is not a
    /// probability.
    pub fn summary(&self) -> Option<String> {
        (self.probability.is_finite() && (0.0..=1.0).contains(&self.probability)).then(|| {
            format!(
                "{} ({:.0}%)",
                self.flag.caution(),
                (self.probability * PERCENT).round()
            )
        })
    }
}

/// Why a request needs an answer, mirroring the precedence of the prompt's
/// diagnostic reason. An ask rule is named by the typed authority that
/// decided it, never by display text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptReason {
    #[default]
    Uncovered,
    AskRule {
        origin: RuleOrigin,
        pattern: String,
    },
    Protected,
    RequiresReview,
    Forced,
    Plan,
}

/// Why Auto mode left a request to the user. Present only in Auto mode, and
/// only when Auto did not decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoNote {
    EngineFlagged,
    EngineNeeded,
    EngineAdvisory,
    EngineUnavailable,
    Restricted,
    AlwaysAsks(ShellOpacity),
    RuleAsks,
    Planning,
    Forced,
    Protected,
    ToolDefault,
}

impl AutoNote {
    /// A stable snake_case name for structured logs.
    pub fn name(self) -> &'static str {
        match self {
            Self::EngineFlagged => "engine_flagged",
            Self::EngineNeeded => "engine_needed",
            Self::EngineAdvisory => "engine_advisory",
            Self::EngineUnavailable => "engine_unavailable",
            Self::Restricted => "restricted",
            Self::AlwaysAsks(_) => "always_asks",
            Self::RuleAsks => "rule_asks",
            Self::Planning => "planning",
            Self::Forced => "forced",
            Self::Protected => "protected",
            Self::ToolDefault => "tool_default",
        }
    }

    pub fn phrase(self) -> &'static str {
        match self {
            Self::EngineFlagged => "Auto asked: the decision engine flagged this",
            Self::EngineNeeded => "Auto asked: scripts need a decision engine to screen them",
            Self::EngineAdvisory => {
                "Auto asked: the decision engine only advises, so it can't approve scripts"
            }
            Self::EngineUnavailable => "Auto asked: the decision engine couldn't check this",
            Self::Restricted => "Auto asked: this project turned off decision engine screening",
            Self::AlwaysAsks(ShellOpacity::Privilege) => {
                "Auto asked: sudo, su, and doas always need your approval"
            }
            Self::AlwaysAsks(ShellOpacity::Indirect) => {
                "Auto asked: eval and source always need your approval"
            }
            Self::AlwaysAsks(_) => "Auto asked: Caudra couldn't read this command line",
            Self::RuleAsks => "Auto asked: a rule says to ask first",
            Self::Planning => "Auto asked: plan mode needs your approval",
            Self::Forced => "Auto asked: this tool always asks for approval",
            Self::Protected => "Auto asked: protected files and commands always need your approval",
            Self::ToolDefault => "Auto asked: Auto only decides for tools that prompt by default",
        }
    }
}

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
        reason: PromptReason::default(),
        project: None,
        advisories: Vec::new(),
        auto: None,
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
        EngineFlag, PermissionAdvisory, PermissionPresentation, PermissionResourceAccess,
        PermissionResourceKind, PermissionResourcePresentation, PermissionRisk, PromptReason,
        ResourceCoverage, RuleOrigin, listed_commands, update_presentation_coverage,
    };
    use std::collections::HashSet;

    const DELETE_SUMMARY: &str = "May delete files (87%)";
    const HALF_ROUNDED_SUMMARY: &str = "May delete files (88%)";

    /// The wire carries each flag as the engine's own question id, and no two
    /// flags read the same, so a caution always says which one was raised.
    #[test]
    fn every_engine_flag_has_a_caution() {
        let mut cautions = HashSet::new();
        for flag in EngineFlag::ALL {
            assert_eq!(serde_json::to_value(flag).unwrap(), json!(flag.name()));
            assert_eq!(
                serde_json::from_value::<EngineFlag>(json!(flag.name())).unwrap(),
                flag
            );
            assert!(cautions.insert(flag.caution()), "{}", flag.name());
        }
    }

    #[test_case(0.87, Some(DELETE_SUMMARY); "a_probability")]
    #[test_case(0.875, Some(HALF_ROUNDED_SUMMARY); "a_half_rounds_up")]
    #[test_case(f64::NAN, None; "not_a_number")]
    #[test_case(1.5, None; "out_of_range")]
    fn advisory_summary_reads_as_a_percentage(probability: f64, expected: Option<&str>) {
        let advisory = PermissionAdvisory {
            flag: EngineFlag::Deletes,
            probability,
        };
        assert_eq!(advisory.summary().as_deref(), expected);
    }

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
            reason: PromptReason::default(),
            resources: vec![resource.clone(), resource],
            project: None,
            advisories: Vec::new(),
            auto: None,
        };
        let granted = ResourceCoverage {
            origin: RuleOrigin::Project,
            authority: NARROW_ALLOW.into(),
            asks: false,
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
