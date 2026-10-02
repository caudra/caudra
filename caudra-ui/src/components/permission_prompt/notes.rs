use std::collections::BTreeSet;
use std::path::Path;

use caudra_agent::permissions::{
    CONFINED_READ_AUTHORITY, PermissionCaution, PermissionRequest, PermissionResourceKind,
    PermissionSubject, PromptReason, ResourceCoverage, RuleOrigin,
};

use super::PermissionPrompt;
use super::details::{mask_secrets, redact_url_query, review_lines, review_text};
use super::scope::requested_action;

pub(super) const PLAN_NOTE: &str = "While planning, approvals last for this conversation.";
const PROTECTED_NOTE: &str = "Touches protected files such as keys and settings";
const OUTSIDE_REPOSITORY: &str = "Outside this repository";
const OUTSIDE_HOME: &str = "Outside your home directory";
const OUTSIDE_PROJECT: &str = "Outside this project";
const REMOTE_NOTE: &str = "Runs on a remote workspace";
const WORKDIR_PREFIX: &str = "in ";
const HOME_PREFIX: &str = "~";
const READ_ONLY: &str = "read-only";
const BUILT_IN: &str = "built-in";
const ORIGIN_SEPARATOR: &str = " · ";
const MAX_ARGUMENT_SUMMARY_CHARS: usize = 160;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tone {
    Warning,
    Danger,
    Muted,
}

/// One line under the action: a `⚠` caution or a muted reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Note {
    pub tone: Tone,
    pub text: String,
}

impl Note {
    fn warning(text: impl Into<String>) -> Self {
        Self {
            tone: Tone::Warning,
            text: text.into(),
        }
    }

    fn muted(text: impl Into<String>) -> Self {
        Self {
            tone: Tone::Muted,
            text: text.into(),
        }
    }
}

/// Where a command row stands before anything is chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RowStatus {
    New,
    Asks,
    Allowed,
}

impl RowStatus {
    pub(super) fn word(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Asks => "asks",
            Self::Allowed => "allowed",
        }
    }
}

pub(super) fn row_status(request: &PermissionRequest, row: usize) -> RowStatus {
    match row_coverage(request, row) {
        None => RowStatus::New,
        Some(coverage) if coverage.asks => RowStatus::Asks,
        Some(_) => RowStatus::Allowed,
    }
}

pub(super) fn row_coverage(request: &PermissionRequest, row: usize) -> Option<&ResourceCoverage> {
    request.presentation.resources.get(row)?.coverage.as_ref()
}

pub(crate) fn origin_word(origin: RuleOrigin) -> &'static str {
    match origin {
        RuleOrigin::Builtin => BUILT_IN,
        RuleOrigin::Config => "config",
        RuleOrigin::Plugin => "plugin",
        RuleOrigin::Conversation => "this conversation",
        RuleOrigin::Project => "project",
        RuleOrigin::Global => "all projects",
    }
}

/// What already settles a row, named the way a person would: `read-only`,
/// `built-in`, or the rule and where it came from, as in `rg * · project`.
pub(super) fn coverage_phrase(coverage: &ResourceCoverage) -> String {
    if coverage.authority == CONFINED_READ_AUTHORITY {
        return READ_ONLY.into();
    }
    if coverage.origin == RuleOrigin::Builtin && !coverage.asks {
        return BUILT_IN.into();
    }
    format!(
        "{}{ORIGIN_SEPARATOR}{}",
        review_text(&coverage.authority),
        origin_word(coverage.origin)
    )
}

pub(super) fn ask_rule_phrase(origin: RuleOrigin, pattern: &str) -> String {
    let pattern = review_text(pattern);
    match origin {
        RuleOrigin::Builtin => format!("Caudra asks before ‹{pattern}› by default"),
        RuleOrigin::Config => format!("Your config asks before ‹{pattern}›"),
        RuleOrigin::Plugin => format!("A plugin asks before ‹{pattern}›"),
        RuleOrigin::Conversation | RuleOrigin::Project | RuleOrigin::Global => {
            format!("A rule you added asks before ‹{pattern}›")
        }
    }
}

/// Why Caudra is asking, as Details says it.
pub(super) fn reason_sentence(reason: &PromptReason) -> String {
    match reason {
        PromptReason::Uncovered => "No rule allows this here yet.".into(),
        PromptReason::AskRule { origin, pattern } => {
            format!("{}.", ask_rule_phrase(*origin, pattern))
        }
        PromptReason::Protected => {
            "It touches protected files, which always need your approval.".into()
        }
        PromptReason::RequiresReview => {
            "Caudra couldn't check every part of it, so it needs your approval.".into()
        }
        PromptReason::Forced => "This tool always asks for approval.".into(),
        PromptReason::Plan => PLAN_NOTE.into(),
    }
}

/// A path the way a person reads it: `~`-abbreviated under home.
pub(crate) fn tilde(path: &str) -> String {
    let path = review_text(path);
    caudra_storage::paths::home()
        .and_then(|home| {
            Path::new(&path).strip_prefix(&home).ok().map(|relative| {
                match relative.as_os_str().is_empty() {
                    true => HOME_PREFIX.to_owned(),
                    false => format!("{HOME_PREFIX}/{}", relative.display()),
                }
            })
        })
        .unwrap_or(path)
}

fn outside_project(request: &PermissionRequest, path: &str) -> bool {
    request
        .presentation
        .project
        .as_deref()
        .is_none_or(|project| !Path::new(path).starts_with(project))
}

/// The literal request a prompt asks about: the command line with its own
/// line breaks, the URL, the paths, or the tool and its arguments.
pub(super) fn action_lines(request: &PermissionRequest) -> Vec<String> {
    let shell = request
        .resources
        .iter()
        .any(|resource| resource.kind == PermissionResourceKind::Command);
    if shell {
        return review_lines(&requested_action(request));
    }
    let paths: Vec<String> = request
        .resources
        .iter()
        .filter_map(|resource| match resource.kind {
            PermissionResourceKind::File | PermissionResourceKind::Directory => {
                Some(tilde(&resource.value))
            }
            PermissionResourceKind::Url => Some(review_text(&redact_url_query(&resource.value))),
            PermissionResourceKind::Query => Some(review_text(&resource.value)),
            _ => None,
        })
        .collect();
    if !paths.is_empty() {
        return paths;
    }
    if request.tool.is_mcp() {
        let arguments = mask_secrets(&request.input).to_string();
        let arguments: String = arguments.chars().take(MAX_ARGUMENT_SUMMARY_CHARS).collect();
        return vec![
            review_text(&request.tool.to_string()),
            review_text(&arguments),
        ];
    }
    review_lines(&request.presentation.action)
}

impl PermissionPrompt {
    /// Muted context under the action, only where it says something new.
    pub(super) fn context(&self) -> Vec<Note> {
        let Some(request) = self.current() else {
            return Vec::new();
        };
        let project = request.presentation.project.as_deref();
        let mut context = Vec::new();
        let workdirs: BTreeSet<&str> = request
            .resources
            .iter()
            .filter(|resource| resource.kind == PermissionResourceKind::Command)
            .filter_map(|resource| resource.attributes.get("workdir").map(String::as_str))
            .filter(|workdir| project.is_none_or(|project| Path::new(workdir) != project))
            .collect();
        for workdir in workdirs {
            context.push(Note::muted(format!("{WORKDIR_PREFIX}{}", tilde(workdir))));
        }
        if request.resources.iter().any(|resource| {
            matches!(
                resource.kind,
                PermissionResourceKind::File | PermissionResourceKind::Directory
            ) && outside_project(request, &resource.value)
        }) {
            context.push(Note::warning(OUTSIDE_PROJECT));
        }
        if matches!(
            request.subject,
            PermissionSubject::RemoteWorkcell { .. } | PermissionSubject::RemoteNative { .. }
        ) {
            context.push(Note::muted(REMOTE_NOTE));
        }
        context
    }

    /// Cautions first, then the reasons worth saying: plan mode, an ask
    /// rule, and why Auto left this to you.
    pub(super) fn notes(&self) -> Vec<Note> {
        let Some(request) = self.current() else {
            return Vec::new();
        };
        let mut notes = Vec::new();
        if let Some(opacity) = self.opacity() {
            notes.push(Note::warning(opacity.caution()));
        } else if request.resources.iter().any(|resource| resource.protected) {
            notes.push(Note::warning(PROTECTED_NOTE));
        }
        if !self.per_row()
            && let Some(caution) = self.chosen_authority().and_then(|option| option.caution)
        {
            notes.push(match caution {
                PermissionCaution::Warn => Note::warning(OUTSIDE_REPOSITORY),
                PermissionCaution::Danger => Note {
                    tone: Tone::Danger,
                    text: OUTSIDE_HOME.into(),
                },
            });
        }
        notes.extend(
            request
                .presentation
                .advisories
                .iter()
                .filter_map(|advisory| advisory.summary())
                .map(Note::warning),
        );
        match &request.presentation.reason {
            PromptReason::Plan => notes.push(Note::muted(PLAN_NOTE)),
            PromptReason::AskRule { origin, pattern } => {
                notes.push(Note::muted(ask_rule_phrase(*origin, pattern)))
            }
            _ => {}
        }
        if let Some(auto) = request.presentation.auto {
            notes.push(Note::muted(auto.phrase()));
        }
        notes
    }
}
