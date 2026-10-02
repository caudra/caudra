use std::path::Path;

use caudra_agent::permissions::{
    COMMAND_EXACT_PREFIX, ComposedRow, PermissionAnswer, PermissionCaution, PermissionExecutorKind,
    PermissionLifetime, PermissionRequest, PermissionResourceAccess, PermissionResourceKind,
    PermissionRowGrant, PermissionRuleOption, PermissionSubject, ShellOpacity,
    StructuredPermissionEffect, grade_command_pattern,
};

use super::choices::{Choice, grant_label};
use super::inspector::{offered_pattern, pattern_widened, unknown_role_caution};
use super::scope::option_summary;
use super::{PermissionPrompt, RowChoice};

const PROTECTED_REACH: &str = " That includes protected files such as keys and settings.";
const HOME_REACH: &str = " That includes the secrets in your home directory.";

/// How storing a grant is confirmed: not at all, by a second fresh Enter
/// under a red warning, or by typing a phrase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Confirm {
    None,
    Keypress,
    Phrase(String),
}

impl Confirm {
    fn strength(&self) -> u8 {
        match self {
            Self::None => 0,
            Self::Keypress => 1,
            Self::Phrase(_) => 2,
        }
    }
}

/// An answer held back until it is confirmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Pending {
    pub answer: PermissionAnswer,
    /// The main-view choice that asked, so an update can tell whether that
    /// choice would still grant the same.
    pub choice: Option<Choice>,
    /// What the grant reaches, in one sentence.
    pub warning: String,
    /// Phrases still to be typed, in row order. None left means a second
    /// Enter confirms.
    pub phrases: Vec<String>,
}

fn allows(option: &PermissionRuleOption) -> bool {
    option.rule.effect == StructuredPermissionEffect::Allow
}

fn remembered(option: &PermissionRuleOption) -> bool {
    option
        .allowed_lifetimes
        .iter()
        .any(|lifetime| *lifetime != PermissionLifetime::Once)
}

/// Every rung one command row offers, narrowest first, as the request emits
/// them.
pub(super) fn row_ladder(
    request: &PermissionRequest,
    row: usize,
) -> impl Iterator<Item = &PermissionRuleOption> {
    request.options.iter().filter(move |option| {
        allows(option) && option.group.as_ref().and_then(|group| group.resource) == Some(row)
    })
}

/// The rungs of a row whose whole reach can be shown, which are the only ones
/// a keypress can choose.
pub(super) fn shown_row_ladder(
    request: &PermissionRequest,
    row: usize,
) -> Vec<&PermissionRuleOption> {
    row_ladder(request, row)
        .filter(|option| option_summary(request, option).complete)
        .collect()
}

/// Whether every resource can be answered on its own row.
pub(super) fn per_row(request: &PermissionRequest) -> bool {
    !request.resources.is_empty()
        && (0..request.resources.len()).all(|row| row_ladder(request, row).next().is_some())
}

pub(super) fn covered(request: &PermissionRequest, row: usize) -> bool {
    request
        .presentation
        .resources
        .get(row)
        .is_some_and(|resource| resource.covered())
}

/// Whether a grant chosen for a row is still one the request offers there.
pub(super) fn offers(request: &PermissionRequest, row: usize, grant: &PermissionRowGrant) -> bool {
    grant_option(request, row, grant).is_some()
}

/// The offered rung a row's grant rides on: the rung itself, the template an
/// edited pattern came from, or the exact rung a written pattern replaces.
pub(super) fn grant_option<'a>(
    request: &'a PermissionRequest,
    row: usize,
    grant: &PermissionRowGrant,
) -> Option<&'a PermissionRuleOption> {
    let id = match grant {
        PermissionRowGrant::Offered(id) => id.clone(),
        PermissionRowGrant::Pattern { option_id, .. } => option_id.clone(),
        PermissionRowGrant::Written(_) => format!("{COMMAND_EXACT_PREFIX}{row}"),
    };
    row_ladder(request, row).find(|option| option.id == id)
}

/// The rung a row starts on: the one the request marks as its default, or
/// none for a row something else already settles.
pub(super) fn default_grant(request: &PermissionRequest, row: usize) -> Option<PermissionRowGrant> {
    if covered(request, row) {
        return None;
    }
    shown_row_ladder(request, row)
        .into_iter()
        .find(|option| option.is_default)
        .map(|option| PermissionRowGrant::Offered(option.id.clone()))
}

/// Where a row can stand, narrowest first: this time only when `once`, each
/// shown rung, and a written pattern once one is chosen. An edited template
/// stands in for the rung it came from.
pub(super) fn row_positions(
    request: &PermissionRequest,
    row: usize,
    current: &Option<PermissionRowGrant>,
    once: bool,
) -> Vec<Option<PermissionRowGrant>> {
    let mut positions = Vec::new();
    if once {
        positions.push(None);
    }
    for option in shown_row_ladder(request, row) {
        positions.push(Some(match current {
            Some(grant @ PermissionRowGrant::Pattern { option_id, .. })
                if *option_id == option.id =>
            {
                grant.clone()
            }
            _ => PermissionRowGrant::Offered(option.id.clone()),
        }));
    }
    if let Some(grant @ PermissionRowGrant::Written(_)) = current {
        positions.push(Some(grant.clone()));
    }
    positions
}

/// The ladder `←` `→` walk on a request answered as a whole: the group of the
/// option the request starts on, narrowest first, or that option alone.
/// Blanket grants are left to Customize.
pub(super) fn main_ladder(request: &PermissionRequest) -> Vec<&PermissionRuleOption> {
    let whole = |option: &PermissionRuleOption| {
        allows(option)
            && remembered(option)
            && option
                .group
                .as_ref()
                .is_none_or(|group| group.resource.is_none())
    };
    let Some(default) = request
        .options
        .iter()
        .find(|option| option.is_default && whole(option))
    else {
        return Vec::new();
    };
    let key = default.group.as_ref().map(|group| group.key.as_str());
    request
        .options
        .iter()
        .filter(|option| {
            whole(option)
                && match key {
                    Some(key) => option.group.as_ref().is_some_and(|group| group.key == key),
                    None => option.id == default.id,
                }
                && option_summary(request, option).complete
        })
        .collect()
}

/// One step along `positions` from `current`, or `None` at that end.
pub(super) fn step<T: PartialEq + Clone>(positions: &[T], current: &T, forward: bool) -> Option<T> {
    let next = match (
        positions.iter().position(|position| position == current),
        forward,
    ) {
        (Some(index), true) => index + 1,
        (Some(index), false) => index.checked_sub(1)?,
        (None, true) => 0,
        (None, false) => return None,
    };
    positions.get(next).cloned()
}

/// How storing `option` for `lifetime` is confirmed. A confirmation the option
/// asks for is a second Enter for this conversation and its phrase beyond it.
pub(super) fn confirm_for(
    request: &PermissionRequest,
    option: &PermissionRuleOption,
    lifetime: &PermissionLifetime,
) -> Confirm {
    if let Some(phrase) = &option.confirmation {
        return phrase_or_keypress(phrase, lifetime);
    }
    if covers_protected(option) || writes_outside_project(request, option) {
        Confirm::Keypress
    } else {
        Confirm::None
    }
}

fn phrase_or_keypress(phrase: &str, lifetime: &PermissionLifetime) -> Confirm {
    if *lifetime == PermissionLifetime::Conversation {
        Confirm::Keypress
    } else {
        Confirm::Phrase(phrase.to_owned())
    }
}

/// How storing one row's grant is confirmed. A written pattern is graded on
/// its own, and an edited template that reaches past what was offered needs
/// a second look.
fn row_confirm(
    request: &PermissionRequest,
    row: usize,
    grant: &PermissionRowGrant,
    lifetime: &PermissionLifetime,
) -> Option<(Confirm, String)> {
    let option = grant_option(request, row, grant)?;
    let confirm = match grant {
        PermissionRowGrant::Written(pattern) => {
            let command = &request.resources.get(row)?.value;
            match grade_command_pattern(pattern, command).ok()?.confirmation {
                Some(phrase) => phrase_or_keypress(phrase, lifetime),
                None => Confirm::None,
            }
        }
        PermissionRowGrant::Pattern { definition, .. } if pattern_widened(definition) => {
            Confirm::Keypress.max_by(confirm_for(request, option, lifetime))
        }
        _ => confirm_for(request, option, lifetime),
    };
    let label = grant_label(request, row, grant);
    Some((confirm, reach_warning(request, option, &label)))
}

impl Confirm {
    fn max_by(self, other: Self) -> Self {
        if other.strength() > self.strength() {
            other
        } else {
            self
        }
    }
}

fn covers_protected(option: &PermissionRuleOption) -> bool {
    option
        .rule
        .resources
        .iter()
        .any(|resource| resource.protected == Some(true))
}

fn writes_outside_project(request: &PermissionRequest, option: &PermissionRuleOption) -> bool {
    let project = request.presentation.project.as_deref();
    option
        .rule
        .resources
        .iter()
        .any(|resource| resource.access == Some(PermissionResourceAccess::Write))
        && request.resources.iter().any(|resource| {
            resource.access == Some(PermissionResourceAccess::Write)
                && project.is_none_or(|project| !Path::new(&resource.value).starts_with(project))
        })
}

/// What the agent could do once the grant is stored, in a sentence or two.
fn reach_warning(
    request: &PermissionRequest,
    option: &PermissionRuleOption,
    scope: &str,
) -> String {
    let mut warning = format!(
        "The agent could {} {scope} without asking.",
        reach_verb(request)
    );
    if option.caution == Some(PermissionCaution::Danger) {
        warning.push_str(HOME_REACH);
    }
    if covers_protected(option) {
        warning.push_str(PROTECTED_REACH);
    }
    if let Some(caution) = offered_pattern(option).and_then(unknown_role_caution) {
        warning.push(' ');
        warning.push_str(caution);
    }
    warning
}

fn reach_verb(request: &PermissionRequest) -> &'static str {
    if request.tool.is_mcp() {
        return "call this tool with";
    }
    let Some(resource) = request.resources.first() else {
        return "use";
    };
    match (&resource.kind, &resource.access) {
        (PermissionResourceKind::Command | PermissionResourceKind::Query, _) => "run",
        (PermissionResourceKind::Url, _) => "fetch",
        (_, Some(PermissionResourceAccess::Write)) => "change",
        (_, Some(PermissionResourceAccess::List)) => "list",
        (_, Some(PermissionResourceAccess::Read | PermissionResourceAccess::Search)) => "read",
        _ => "use",
    }
}

impl PermissionPrompt {
    pub(super) fn per_row(&self) -> bool {
        self.current().is_some_and(per_row)
    }

    /// Whether the request runs shell commands.
    pub(super) fn shell(&self) -> bool {
        self.current().is_some_and(|request| {
            request
                .resources
                .iter()
                .any(|resource| resource.kind == PermissionResourceKind::Command)
        })
    }

    /// The gravest reason Caudra could not read through the command line, if
    /// any. Such a line can only run once or be refused.
    pub(super) fn opacity(&self) -> Option<ShellOpacity> {
        self.current()?
            .resources
            .iter()
            .filter_map(ShellOpacity::of)
            .max()
    }

    /// The rows the main view lists: each command, without the whole line an
    /// unreadable command stands for.
    pub(super) fn listed_rows(&self) -> Vec<usize> {
        self.current().map_or_else(Vec::new, |request| {
            request
                .resources
                .iter()
                .enumerate()
                .filter(|(_, resource)| {
                    resource.kind == PermissionResourceKind::Command
                        && ShellOpacity::of(resource).is_none()
                })
                .map(|(row, _)| row)
                .collect()
        })
    }

    /// Whether the request lists more than one command.
    pub(super) fn batch(&self) -> bool {
        self.listed_rows().len() > 1
    }

    pub(super) fn is_new_row(&self, row: usize) -> bool {
        self.current()
            .is_some_and(|request| per_row(request) && !covered(request, row))
    }

    pub(super) fn first_new_row(&self) -> Option<usize> {
        self.listed_rows()
            .into_iter()
            .find(|row| self.is_new_row(*row))
    }

    pub(super) fn new_rows(&self) -> Vec<usize> {
        self.listed_rows()
            .into_iter()
            .filter(|row| self.is_new_row(*row))
            .collect()
    }

    pub(super) fn row_grant(&self, row: usize) -> Option<PermissionRowGrant> {
        let request = self.current()?;
        match self.rows.get(row)? {
            RowChoice::Chosen(grant) => grant.clone(),
            RowChoice::Default => default_grant(request, row),
        }
    }

    pub(super) fn row_grants(&self) -> Vec<Option<PermissionRowGrant>> {
        (0..self.rows.len())
            .map(|row| self.row_grant(row))
            .collect()
    }

    /// The rung chosen on a request answered as a whole.
    pub(super) fn chosen_authority(&self) -> Option<&PermissionRuleOption> {
        let ladder = main_ladder(self.current()?);
        match &self.authority {
            Some(id) => ladder.into_iter().find(|option| option.id == *id),
            None => ladder.into_iter().find(|option| option.is_default),
        }
    }

    /// Whether a project rule can be filed: the host named the project and
    /// the call runs here.
    pub(super) fn project_available(&self) -> bool {
        self.current().is_some_and(|request| {
            request
                .presentation
                .project
                .as_ref()
                .is_some_and(|project| project.to_str().is_some())
                && !matches!(
                    request.subject,
                    PermissionSubject::RemoteWorkcell { .. }
                        | PermissionSubject::RemoteNative { .. }
                )
                && request.executor != PermissionExecutorKind::RemoteWorkcell
        })
    }

    /// Whether the main view can remember what it shows for `lifetime`.
    pub(super) fn remembers(&self, lifetime: &PermissionLifetime) -> bool {
        let Some(request) = self.current() else {
            return false;
        };
        if *lifetime == PermissionLifetime::Once
            || (*lifetime == PermissionLifetime::Project && !self.project_available())
            || self.opacity().is_some()
        {
            return false;
        }
        if !per_row(request) {
            return self
                .chosen_authority()
                .is_some_and(|option| option.allowed_lifetimes.contains(lifetime));
        }
        let mut remembered = false;
        for (row, grant) in self.row_grants().iter().enumerate() {
            let Some(grant) = grant else {
                continue;
            };
            if !grant_option(request, row, grant)
                .is_some_and(|option| option.allowed_lifetimes.contains(lifetime))
            {
                return false;
            }
            remembered = true;
        }
        remembered
    }

    /// What remembering the main view's scope for `lifetime` answers.
    pub(super) fn allow_answer(&self, lifetime: PermissionLifetime) -> Option<PermissionAnswer> {
        if !self.remembers(&lifetime) {
            return None;
        }
        if self.per_row() {
            return Some(PermissionAnswer::AllowComposed {
                rows: ComposedRow::uniform(self.row_grants(), &lifetime),
            });
        }
        Some(PermissionAnswer::AllowOption {
            option_id: self.chosen_authority()?.id.clone(),
            lifetime,
        })
    }

    /// Moves one row along its ladder. False when it is already at that end.
    pub(super) fn widen_row(&mut self, row: usize, forward: bool) -> bool {
        let Some(request) = self.current() else {
            return false;
        };
        let current = self.row_grant(row);
        let positions = row_positions(request, row, &current, self.batch());
        let Some(next) = step(&positions, &current, forward) else {
            return false;
        };
        self.rows[row] = RowChoice::Chosen(next);
        true
    }

    /// Moves the scope of a request answered as a whole along its ladder.
    pub(super) fn widen_authority(&mut self, forward: bool) -> bool {
        let Some(request) = self.current() else {
            return false;
        };
        let ladder: Vec<String> = main_ladder(request)
            .into_iter()
            .map(|option| option.id.clone())
            .collect();
        let Some(current) = self.chosen_authority().map(|option| option.id.clone()) else {
            return false;
        };
        let Some(next) = step(&ladder, &current, forward) else {
            return false;
        };
        self.authority = Some(next);
        true
    }

    /// What has to happen before `answer` is sent: the strongest confirmation
    /// any remembered rung asks for, or nothing.
    pub(super) fn pending_for(
        &self,
        answer: &PermissionAnswer,
        choice: Option<Choice>,
    ) -> Option<Pending> {
        let request = self.current()?;
        let confirms: Vec<(Confirm, String)> = match answer {
            PermissionAnswer::AllowOption {
                option_id,
                lifetime,
            } => request
                .options
                .iter()
                .find(|option| option.id == *option_id)
                .map(|option| {
                    (
                        confirm_for(request, option, lifetime),
                        reach_warning(request, option, &option.label),
                    )
                })
                .into_iter()
                .collect(),
            PermissionAnswer::AllowComposed { rows } => rows
                .iter()
                .enumerate()
                .filter_map(|(row, composed)| {
                    let composed = composed.as_ref()?;
                    row_confirm(request, row, &composed.grant, &composed.lifetime)
                })
                .collect(),
            _ => Vec::new(),
        };
        let strongest = confirms
            .iter()
            .map(|(confirm, _)| confirm.strength())
            .max()?;
        if strongest == 0 {
            return None;
        }
        let warning = confirms
            .iter()
            .find(|(confirm, _)| confirm.strength() == strongest)
            .map(|(_, warning)| warning.clone())?;
        let mut phrases: Vec<String> = Vec::new();
        for (confirm, _) in &confirms {
            if let Confirm::Phrase(phrase) = confirm
                && !phrases.contains(phrase)
            {
                phrases.push(phrase.clone());
            }
        }
        Some(Pending {
            answer: answer.clone(),
            choice,
            warning,
            phrases,
        })
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use caudra_agent::permissions::{
        PermissionAuthorityProfile, PermissionExecutorKind, PermissionRequest, PermissionResource,
        PermissionResourceAccess, PermissionResourceKind, PermissionRisk, PermissionSubject,
    };
    use caudra_agent::tools::{PermissionIntent, PermissionScopes};
    use caudra_config::ToolKey;
    use serde_json::json;

    pub(crate) const PROJECT: &str = "/project";
    const NATIVE_OWNER: &str = "workcell";
    const SHELL_CONTRACT: &str = "shell.execution.v1";

    pub(crate) fn command_resource(command: &str) -> PermissionResource {
        PermissionResource {
            kind: PermissionResourceKind::Command,
            value: command.into(),
            access: Some(PermissionResourceAccess::Execute),
            protected: false,
            requires_prompt: false,
            attributes: BTreeMap::from([
                ("workdir".into(), PROJECT.into()),
                ("normalized_command".into(), command.into()),
            ]),
        }
    }

    /// A shell request the way the native shell tool prepares one, one
    /// resource per command plus any extra resources given.
    pub(crate) fn shell_request(
        line: &str,
        resources: Vec<PermissionResource>,
    ) -> PermissionRequest {
        let intent = PermissionIntent::new(
            PermissionScopes::single(line.into()),
            resources,
            PermissionRisk::Low,
        )
        .with_authority(PermissionAuthorityProfile::Shell);
        let mut request = PermissionRequest::from_intent_with_identity(
            "native-shell".into(),
            ToolKey::native("shell"),
            &intent,
            json!({"command": line, "workdir": PROJECT}),
            Path::new(PROJECT),
            PermissionSubject::Native {
                owner: NATIVE_OWNER.into(),
                contract: SHELL_CONTRACT.into(),
            },
            PermissionExecutorKind::Native,
        );
        request.presentation.project = Some(PROJECT.into());
        request
    }

    pub(crate) fn native_shell_request(command: &str) -> PermissionRequest {
        shell_request(command, vec![command_resource(command)])
    }

    pub(crate) fn commands_request(commands: &[&str]) -> PermissionRequest {
        shell_request(
            &commands.join(" && "),
            commands
                .iter()
                .map(|command| command_resource(command))
                .collect(),
        )
    }
}
