use std::path::Path;

use caudra_storage::permission_state::{
    PermissionExecutorKind, PermissionResourceAccess, PermissionResourceKind,
    PermissionResourceSelector, PermissionSubject,
};

use super::details::{MAX_REVIEW_CHARS, review_text};
use super::inspector::{offered_pattern, pattern_impact, pattern_summary};
use super::scope::{
    ApprovalImpact, ReviewDocument, SHELL_REACH, ScopeSummary, WHOLE_CALL, complete_text,
    identity_lines, option_impact, option_summary, requested_action,
};
use super::{
    AuthorityRow, CHIP_ONCE, COMPOSABLE_SHELL_OPTIONS, Panel, PermissionAnswer, PermissionDecision,
    PermissionLifetime, PermissionPrompt, PermissionRequest, PermissionRowGrant,
    PermissionRuleOption, PromptState, PromptTarget, RowChoice, StructuredPermissionEffect,
    grade_command_pattern,
};

const PATTERN_COVERAGE_UNAVAILABLE: &str =
    "Pattern coverage is unavailable or does not match this call. Change the scope or use Once.";

pub(super) struct Confirmation {
    pub answer: PermissionAnswer,
    pub review: ReviewDocument,
    pub phrase: Option<String>,
    pub complete: bool,
    pub impact: ApprovalImpact,
}

pub(super) fn authorities(
    request: &PermissionRequest,
) -> impl Iterator<Item = &PermissionRuleOption> {
    let per_command = !command_ladders(request).is_empty();
    request.options.iter().filter(move |option| {
        option.rule.effect == StructuredPermissionEffect::Allow
            && option
                .allowed_lifetimes
                .iter()
                .any(|lifetime| *lifetime != PermissionLifetime::Once)
            && option
                .group
                .as_ref()
                .is_none_or(|group| group.resource.is_none())
            && !(per_command && COMPOSABLE_SHELL_OPTIONS.contains(&option.id.as_str()))
    })
}

pub(super) fn command_ladders(request: &PermissionRequest) -> Vec<Vec<&PermissionRuleOption>> {
    let mut ladders = vec![Vec::new(); request.resources.len()];
    for option in &request.options {
        if option.rule.effect != StructuredPermissionEffect::Allow {
            continue;
        }
        if let Some(index) = option.group.as_ref().and_then(|group| group.resource)
            && let Some(ladder) = ladders.get_mut(index)
        {
            ladder.push(option);
        }
    }
    if ladders.iter().any(Vec::is_empty) {
        Vec::new()
    } else {
        for ladder in &mut ladders {
            ladder.sort_by_key(|option| offered_pattern(option).is_some());
        }
        ladders
    }
}

pub(super) fn authority_rows(request: &PermissionRequest, selected: &str) -> Vec<AuthorityRow> {
    let mut keys = Vec::new();
    let mut rows: Vec<AuthorityRow> = Vec::new();
    for option in authorities(request) {
        let key = option.group.as_ref().map(|group| group.key.as_str());
        let existing = key.and_then(|key| keys.iter().position(|found| *found == Some(key)));
        match existing.map(|index| &mut rows[index]) {
            Some(row) => {
                if option.id == selected {
                    row.chosen.clone_from(&option.id);
                }
                row.rungs.push(option.id.clone());
            }
            None => {
                keys.push(key);
                rows.push(AuthorityRow {
                    chosen: option.id.clone(),
                    rungs: vec![option.id.clone()],
                });
            }
        }
    }
    rows
}

impl PermissionPrompt {
    pub(super) fn approve(&mut self, lifetime: PermissionLifetime) -> Option<PermissionDecision> {
        if self.awaiting_review || !self.grants_lifetime(&lifetime) {
            return None;
        }
        let answer = self.allow_answer(lifetime.clone());
        let request = self.current()?;
        let confirmation = Confirmation::new(request, answer.clone(), &self.scopes);
        if confirmation.impact == ApprovalImpact::Routine && confirmation.complete {
            return Some(PermissionDecision {
                request_id: request.id.clone(),
                answer,
            });
        }
        let state = match lifetime {
            PermissionLifetime::Conversation => PromptState::ConfirmAllowSession,
            PermissionLifetime::Project => PromptState::ConfirmAllowAlwaysLocal,
            PermissionLifetime::Global => PromptState::ConfirmAllowAlwaysGlobal,
            PermissionLifetime::Once => return None,
        };
        self.start_confirmation(state, confirmation);
        None
    }

    pub(super) fn open_confirmation(&mut self, state: PromptState) {
        if self.awaiting_review {
            return;
        }
        let answer = match state {
            PromptState::ConfirmDenyAlwaysLocal if self.project_available() => {
                PermissionAnswer::DenyAlwaysLocal
            }
            PromptState::ConfirmDenyAlwaysGlobal => PermissionAnswer::DenyAlwaysGlobal,
            _ => return,
        };
        let Some(request) = self.current() else {
            return;
        };
        let confirmation = Confirmation::new(request, answer, &[]);
        self.start_confirmation(state, confirmation);
    }

    fn start_confirmation(&mut self, state: PromptState, confirmation: Confirmation) {
        self.confirmation = Some(confirmation);
        self.input_freshness.barrier();
        self.inspector = None;
        self.state = state;
        self.panel = Panel::Main;
        self.field.clear();
        self.scroll.reset();
        self.invalidate_controls();
    }

    pub(super) fn select_default_path(&mut self) {
        if let Some(request) = self.current()
            && ordinary_local(request)
            && request.resources.iter().all(|resource| {
                matches!(
                    resource.access,
                    Some(PermissionResourceAccess::Read | PermissionResourceAccess::Search)
                )
            })
            && let Some(option) = request
                .options
                .iter()
                .find(|option| option.id == "allow_exact_resources")
        {
            self.selected_option = option.id.clone();
        }
    }

    pub(super) fn open_scope_editor(&mut self) {
        self.panel = Panel::Scopes;
        if self.lifetime == PermissionLifetime::Once {
            self.lifetime = if self.current().is_some_and(ordinary_local)
                && self.grants_lifetime(&PermissionLifetime::Project)
            {
                PermissionLifetime::Project
            } else if self.grants_lifetime(&PermissionLifetime::Conversation) {
                PermissionLifetime::Conversation
            } else {
                PermissionLifetime::Once
            };
        }
        self.scroll.reset();
        self.invalidate_controls();
        self.focus = Some(PromptTarget::Scope);
    }

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

    pub(super) fn grants_lifetime(&self, lifetime: &PermissionLifetime) -> bool {
        let Some(request) = self.current() else {
            return false;
        };
        if *lifetime == PermissionLifetime::Project && !self.project_available() {
            return false;
        }
        if self.command_row().is_none() {
            return self
                .selected_authority()
                .is_some_and(|option| option.allowed_lifetimes.contains(lifetime));
        }
        let ladders = command_ladders(request);
        let mut granted = false;
        for (offered, choice) in ladders.iter().zip(&self.scopes) {
            let option = match choice.grant(offered) {
                Some(PermissionRowGrant::Offered(id)) => offered.iter().find(|rung| rung.id == id),
                Some(PermissionRowGrant::Written(_)) => offered.first(),
                Some(PermissionRowGrant::Pattern { .. }) => choice
                    .pattern
                    .as_ref()
                    .and_then(|edited| offered.iter().find(|option| option.id == edited.option_id)),
                None => continue,
            };
            if !option.is_some_and(|option| option.allowed_lifetimes.contains(lifetime)) {
                return false;
            }
            granted = true;
        }
        granted
    }

    pub(super) fn selected_authority(&self) -> Option<&PermissionRuleOption> {
        self.current()?.options.iter().find(|option| {
            option.id == self.selected_option
                && option.rule.effect == StructuredPermissionEffect::Allow
        })
    }

    pub(super) fn command_row(&self) -> Option<usize> {
        command_ladders(self.current()?)
            .iter()
            .position(|ladder| self.row_key(ladder[0]) == self.selected_option)
    }

    pub(super) fn row_key(&self, option: &PermissionRuleOption) -> String {
        option
            .group
            .as_ref()
            .filter(|group| group.resource.is_some())
            .map_or_else(|| option.id.clone(), |group| group.key.clone())
    }

    pub(super) fn covered_count(&self) -> usize {
        self.current().map_or(0, |request| {
            request
                .presentation
                .resources
                .iter()
                .filter(|resource| resource.covered())
                .count()
        })
    }

    pub(super) fn row_keys(&self) -> Vec<String> {
        let Some(request) = self.current() else {
            return Vec::new();
        };
        command_ladders(request)
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                self.expanded_covered
                    || !request
                        .presentation
                        .resources
                        .get(*index)
                        .is_some_and(|resource| resource.covered())
            })
            .map(|(_, ladder)| self.row_key(ladder[0]))
            .chain(
                authority_rows(request, &self.selected_option)
                    .into_iter()
                    .map(|row| row.chosen),
            )
            .collect()
    }

    pub(super) fn move_selection(&mut self, reverse: bool) {
        let keys = self.row_keys();
        if keys.is_empty() {
            return;
        }
        let current = keys
            .iter()
            .position(|key| *key == self.selected_option)
            .unwrap_or_default();
        let next = if reverse {
            (current + keys.len() - 1) % keys.len()
        } else {
            (current + 1) % keys.len()
        };
        self.select_authority(keys[next].clone());
    }

    pub(super) fn widen(&mut self, forward: bool) {
        if let Some(row) = self.command_row() {
            let Some(request) = self.current() else {
                return;
            };
            let offered = command_ladders(request)[row].len();
            let choice = &mut self.scopes[row];
            choice.rung = if forward {
                (choice.rung + 1).min(choice.ladder_len(offered) - 1)
            } else {
                choice.rung.saturating_sub(1)
            };
            self.scope_changed();
            return;
        }
        let Some(request) = self.current() else {
            return;
        };
        let rows = authority_rows(request, &self.selected_option);
        if let Some(row) = rows
            .iter()
            .find(|row| row.rungs.contains(&self.selected_option))
        {
            let rung = row
                .rungs
                .iter()
                .position(|rung| *rung == self.selected_option)
                .unwrap_or_default();
            let next = if forward {
                (rung + 1).min(row.rungs.len() - 1)
            } else {
                rung.saturating_sub(1)
            };
            self.select_authority(row.rungs[next].clone());
        }
    }

    pub(super) fn can_widen(&self) -> bool {
        let Some(request) = self.current() else {
            return false;
        };
        if let Some(row) = self.command_row() {
            return self.scopes[row].ladder_len(command_ladders(request)[row].len()) > 1;
        }
        authority_rows(request, &self.selected_option)
            .iter()
            .any(|row| row.rungs.contains(&self.selected_option) && row.rungs.len() > 1)
    }

    pub(super) fn select_authority(&mut self, id: String) {
        if self.confirmation.is_some() {
            return;
        }
        self.selected_option = id;
        self.scope_changed();
    }

    pub(super) fn scope_changed(&mut self) {
        if !self.grants_lifetime(&self.lifetime) {
            self.lifetime = PermissionLifetime::Once;
        }
        let focus = self.focus.clone();
        self.invalidate_controls();
        self.focus = focus;
        self.pending_reveal = Some(PromptTarget::Authority(self.selected_option.clone()));
    }

    pub(super) fn allow_answer(&self, lifetime: PermissionLifetime) -> PermissionAnswer {
        if let Some(request) = self.current().filter(|_| self.command_row().is_some()) {
            PermissionAnswer::AllowComposed {
                rows: self.row_grants(request),
                lifetime,
            }
        } else {
            PermissionAnswer::AllowOption {
                option_id: self.selected_option.clone(),
                lifetime,
            }
        }
    }

    pub(super) fn row_grants(
        &self,
        request: &PermissionRequest,
    ) -> Vec<Option<PermissionRowGrant>> {
        command_ladders(request)
            .iter()
            .zip(&self.scopes)
            .map(|(offered, choice)| choice.grant(offered))
            .collect()
    }

    pub(super) fn row_summary(&self, row: usize) -> ScopeSummary {
        let Some(request) = self.current() else {
            return empty_summary();
        };
        let ladders = command_ladders(request);
        let grant = ladders
            .get(row)
            .zip(self.scopes.get(row))
            .and_then(|(offered, choice)| choice.grant(offered));
        match grant {
            Some(PermissionRowGrant::Offered(id)) => request
                .options
                .iter()
                .find(|option| option.id == id)
                .map_or_else(empty_summary, |option| option_summary(request, option)),
            Some(PermissionRowGrant::Written(pattern)) => ScopeSummary {
                label: format!("Custom prefix: {}", review_text(&pattern)),
                lines: vec![
                    "Only matching commands are remembered; other rows still run this time.".into(),
                    SHELL_REACH.into(),
                ],
                complete: complete_text(&review_text(&pattern)),
            },
            Some(PermissionRowGrant::Pattern { definition, .. }) => pattern_summary(&definition),
            None => ScopeSummary {
                label: CHIP_ONCE.into(),
                lines: vec!["Runs with this call; no new future authority for this row.".into()],
                complete: true,
            },
        }
    }

    pub(super) fn selected_summary(&self) -> ScopeSummary {
        if let Some(row) = self.command_row() {
            return self.row_summary(row);
        }
        self.current()
            .zip(self.selected_authority())
            .map_or_else(empty_summary, |(request, option)| {
                option_summary(request, option)
            })
    }

    pub(super) fn confirmation_phrase(&self) -> Option<&str> {
        self.confirmation.as_ref()?.phrase.as_deref()
    }
}

fn empty_summary() -> ScopeSummary {
    ScopeSummary {
        label: "This call only".into(),
        lines: vec!["No reusable authority is available.".into()],
        complete: false,
    }
}

fn ordinary_local(request: &PermissionRequest) -> bool {
    request.executor == PermissionExecutorKind::Native
        && matches!(request.subject, PermissionSubject::Native { .. })
        && !request.resources.is_empty()
        && request.resources.iter().all(|resource| {
            !resource.protected
                && !resource.requires_prompt
                && matches!(
                    resource.kind,
                    PermissionResourceKind::File
                        | PermissionResourceKind::Directory
                        | PermissionResourceKind::Command
                )
                && !resource
                    .value
                    .split(['/', '\\'])
                    .any(|part| part.starts_with('.') && part != ".")
                && (!matches!(
                    resource.kind,
                    PermissionResourceKind::File | PermissionResourceKind::Directory
                ) || (Path::new(&resource.value).parent().is_some()
                    && caudra_storage::paths::home()
                        .is_none_or(|home| Path::new(&resource.value) != home)))
        })
}

impl Confirmation {
    fn new(request: &PermissionRequest, answer: PermissionAnswer, scopes: &[RowChoice]) -> Self {
        let mut summary = Vec::new();
        let mut phrases = Vec::new();
        let mut complete = true;
        let mut impact = ApprovalImpact::Routine;
        let mut add_option = |option: Option<&PermissionRuleOption>| {
            if let Some(option) = option {
                let shown = option_summary(request, option);
                complete &= shown.complete;
                if option_impact(request, option) == ApprovalImpact::Review {
                    impact = ApprovalImpact::Review;
                }
                summary.push(format!("Authority: {}", shown.label));
                summary.extend(shown.lines);
                phrases.extend(option.confirmation.clone());
            } else {
                complete = false;
            }
        };
        let lifetime = match &answer {
            PermissionAnswer::AllowOption {
                option_id,
                lifetime,
            } => {
                add_option(
                    request
                        .options
                        .iter()
                        .find(|option| option.id == *option_id),
                );
                lifetime
            }
            PermissionAnswer::AllowComposed { rows, lifetime } => {
                for row in rows {
                    if let Some(PermissionRowGrant::Offered(id)) = row {
                        add_option(request.options.iter().find(|option| option.id == *id));
                    }
                }
                for (index, row) in rows.iter().enumerate() {
                    match row {
                        Some(PermissionRowGrant::Pattern {
                            option_id,
                            definition,
                        }) => {
                            let shown = pattern_summary(definition);
                            complete &= shown.complete;
                            if pattern_impact(definition) == ApprovalImpact::Review {
                                impact = ApprovalImpact::Review;
                            }
                            let source = scopes
                                .get(index)
                                .and_then(|choice| choice.pattern.as_ref())
                                .filter(|edited| {
                                    edited.option_id == *option_id
                                        && edited.definition == *definition
                                })
                                .and_then(|edited| {
                                    request
                                        .options
                                        .iter()
                                        .find(|option| option.id == edited.option_id)
                                })
                                .filter(|option| {
                                    option.group.as_ref().and_then(|group| group.resource)
                                        == Some(index)
                                        && offered_pattern(option).is_some()
                                });
                            if let Some(source) = source {
                                let mut edited = source.clone();
                                edited.rule.resources[0].selector =
                                    PermissionResourceSelector::CommandTemplate {
                                        definition: definition.clone(),
                                    };
                                if option_impact(request, &edited) == ApprovalImpact::Review {
                                    impact = ApprovalImpact::Review;
                                }
                                phrases.extend(source.confirmation.clone());
                            } else {
                                complete = false;
                            }
                            summary.push(format!("Command {}: {}", index + 1, shown.label));
                            summary.extend(shown.lines);
                        }
                        Some(PermissionRowGrant::Written(pattern)) => {
                            impact = ApprovalImpact::Review;
                            summary.push(format!(
                                "Command {} prefix: {}",
                                index + 1,
                                review_text(pattern)
                            ));
                            if let Some(resource) = request.resources.get(index) {
                                if let Some(workdir) = resource.attributes.get("workdir") {
                                    summary.push(format!(
                                        "Starting directory: {}",
                                        review_text(workdir)
                                    ));
                                }
                                match grade_command_pattern(pattern, &resource.value) {
                                    Ok(grade) => {
                                        phrases.extend(grade.confirmation.map(str::to_owned))
                                    }
                                    Err(_) => complete = false,
                                }
                            } else {
                                complete = false;
                            }
                            summary.push(SHELL_REACH.into());
                        }
                        None => summary
                            .push(format!("Command {}: run now; do not remember.", index + 1)),
                        _ => {}
                    }
                }
                complete &= rows.len() == request.resources.len();
                if rows.iter().any(|grant| match grant {
                    Some(PermissionRowGrant::Pattern { .. }) => true,
                    Some(PermissionRowGrant::Offered(id)) => request
                        .options
                        .iter()
                        .find(|option| option.id == *id)
                        .is_some_and(|option| offered_pattern(option).is_some()),
                    _ => false,
                }) && request.composed_rules(rows, lifetime).is_err()
                {
                    complete = false;
                    summary.push(PATTERN_COVERAGE_UNAVAILABLE.into());
                }
                summary.push(WHOLE_CALL.into());
                lifetime
            }
            PermissionAnswer::DenyAlwaysLocal => {
                impact = ApprovalImpact::Review;
                summary.push("Deny this exact call for this project.".into());
                &PermissionLifetime::Project
            }
            PermissionAnswer::DenyAlwaysGlobal => {
                impact = ApprovalImpact::Review;
                summary.push("Deny this exact call across projects.".into());
                &PermissionLifetime::Global
            }
            _ => &PermissionLifetime::Once,
        };
        if *lifetime == PermissionLifetime::Global {
            impact = ApprovalImpact::Review;
        }
        summary.insert(0, lifetime_description(lifetime).into());
        summary.push(format!("Requested: {}", requested_action(request)));
        if *lifetime == PermissionLifetime::Project {
            summary.insert(1, project_binding(request));
            complete &= request
                .presentation
                .project
                .as_ref()
                .is_some_and(|project| project.to_str().is_some());
        }
        summary.extend(identity_lines(request));
        phrases.sort();
        phrases.dedup();
        let phrase = (!phrases.is_empty()).then(|| phrases.join("; "));
        if let Some(phrase) = &phrase {
            summary.push(format!("Type to confirm: {}", review_text(phrase)));
        }
        let mut remaining = MAX_REVIEW_CHARS;
        for line in &mut summary {
            *line = review_text(line);
            complete &= complete_text(line);
            let length = line.chars().count();
            if length > remaining {
                *line = line.chars().take(remaining).collect();
                complete = false;
            }
            remaining = remaining.saturating_sub(length);
        }
        let mut review = ReviewDocument::new(request, &answer);
        complete &= review.bound();
        Self {
            answer,
            review,
            phrase,
            complete,
            impact,
        }
    }
}

pub(super) fn lifetime_description(lifetime: &PermissionLifetime) -> &'static str {
    match lifetime {
        PermissionLifetime::Once => "Keep for: this call only; nothing remembered.",
        PermissionLifetime::Conversation => "Keep for: this conversation only.",
        PermissionLifetime::Project => "Keep for: this project, across conversations.",
        PermissionLifetime::Global => "Keep for: all projects, across conversations.",
    }
}

pub(super) fn project_binding(request: &PermissionRequest) -> String {
    request
        .presentation
        .project
        .as_deref()
        .and_then(Path::to_str)
        .map_or_else(
            || "Project: unavailable; project persistence is disabled.".into(),
            |project| format!("Project: {}", review_text(project)),
        )
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use caudra_agent::permissions::{
        PermissionAnswer, PermissionAuthorityProfile, PermissionExecutorKind, PermissionLifetime,
        PermissionRequest, PermissionResource, PermissionResourceAccess, PermissionResourceKind,
        PermissionResourceSelector, PermissionRisk, PermissionRowGrant, PermissionSubject,
    };
    use caudra_agent::tools::{PermissionIntent, PermissionScopes};
    use caudra_config::ToolKey;
    use crossterm::event::{KeyCode, KeyEventKind};
    use serde_json::json;
    use test_case::test_case;

    use super::super::view::tests::{ROOMY_HEIGHT, ROOMY_WIDTH, key, open_prompt, render};
    use super::super::{Panel, PermissionPrompt};
    use super::{ApprovalImpact, Confirmation};

    const FILE: &str = "/project/src/main.rs";
    const PROJECT: &str = "/host/project-binding";
    const SHELL_PHRASE: &str = "ALLOW BROAD SHELL ACCESS";
    const WRITE_PHRASE: &str = "ALLOW FILE CHANGES";
    const FIRST_PHRASE: &str = "FIRST PHRASE";
    const SECOND_PHRASE: &str = "SECOND PHRASE";
    const NATIVE_COMMAND: &str = "cargo test";
    const NATIVE_WORKDIR: &str = "/project";
    const NATIVE_OWNER: &str = "workcell";
    const SHELL_CONTRACT: &str = "shell.execution.v1";
    const POSSIBLE_WORKDIRS: &str = "possible_workdirs";
    const POSSIBLE_WORKDIRS_LABEL: &str = "Possible working directories: `/project`";

    pub(crate) fn native_shell_request(command: &str) -> PermissionRequest {
        let resource = PermissionResource {
            kind: PermissionResourceKind::Command,
            value: command.into(),
            access: Some(PermissionResourceAccess::Execute),
            protected: false,
            requires_prompt: false,
            attributes: BTreeMap::from([
                ("workdir".into(), NATIVE_WORKDIR.into()),
                ("normalized_command".into(), command.into()),
                (
                    POSSIBLE_WORKDIRS.into(),
                    json!({"kind": "known", "symbolic_paths": [NATIVE_WORKDIR]}).to_string(),
                ),
            ]),
        };
        let intent = PermissionIntent::new(
            PermissionScopes::single(command.into()),
            vec![resource],
            PermissionRisk::Low,
        )
        .with_authority(PermissionAuthorityProfile::Shell);
        let mut request = PermissionRequest::from_intent_with_identity(
            "native-shell".into(),
            ToolKey::native("shell"),
            &intent,
            json!({"command": command, "workdir": NATIVE_WORKDIR}),
            Path::new(NATIVE_WORKDIR),
            PermissionSubject::Native {
                owner: NATIVE_OWNER.into(),
                contract: SHELL_CONTRACT.into(),
            },
            PermissionExecutorKind::Native,
        );
        request.presentation.project = Some(PROJECT.into());
        request
    }

    fn file_prompt(tool: &str) -> PermissionPrompt {
        let mut request = PermissionRequest::from_legacy(
            "file".into(),
            ToolKey::native(tool),
            vec![FILE.into()],
            json!({"filePath": FILE, "limit": 10}),
            Path::new("/project"),
            false,
        );
        request.presentation.project = Some(PROJECT.into());
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(Box::new(request), None);
        prompt
    }

    #[test_case('s', PermissionLifetime::Conversation; "conversation")]
    #[test_case('a', PermissionLifetime::Project; "project")]
    fn ordinary_exact_command_is_approved_directly(shortcut: char, lifetime: PermissionLifetime) {
        let mut prompt = open_prompt();
        assert!(prompt.handle_key(key(KeyCode::Char(shortcut))).is_none());
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        let mut released = key(KeyCode::Char(shortcut));
        released.kind = KeyEventKind::Release;
        prompt.handle_key(released);
        let expected = prompt.allow_answer(lifetime);
        assert_eq!(
            prompt
                .handle_key(key(KeyCode::Char(shortcut)))
                .unwrap()
                .answer,
            expected
        );
        assert!(prompt.confirmation.is_none());
    }

    #[test_case(false, 'y', PermissionLifetime::Once; "digest_once")]
    #[test_case(false, 's', PermissionLifetime::Conversation; "digest_conversation")]
    #[test_case(false, 'a', PermissionLifetime::Project; "digest_project")]
    #[test_case(true, 'y', PermissionLifetime::Once; "literal_once")]
    #[test_case(true, 's', PermissionLifetime::Conversation; "literal_conversation")]
    #[test_case(true, 'a', PermissionLifetime::Project; "literal_project")]
    fn native_exact_command_with_prepared_attributes_is_direct(
        literal: bool,
        shortcut: char,
        lifetime: PermissionLifetime,
    ) {
        let mut request = native_shell_request(NATIVE_COMMAND);
        let option = request
            .options
            .iter_mut()
            .find(|option| option.id == "command_exact_0")
            .unwrap();
        assert!(matches!(
            option.rule.resources[0].attributes[POSSIBLE_WORKDIRS],
            PermissionResourceSelector::Digest { .. }
        ));
        if literal {
            for (name, selector) in &mut option.rule.resources[0].attributes {
                *selector = PermissionResourceSelector::Exact {
                    value: request.resources[0].attributes[name].clone(),
                };
            }
        }
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(Box::new(request), None);
        let summary = prompt.row_summary(0);
        assert!(summary.complete, "{}", summary.lines.join("\n"));
        assert!(
            summary
                .lines
                .iter()
                .any(|line| line == POSSIBLE_WORKDIRS_LABEL)
        );
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        let expected = if lifetime == PermissionLifetime::Once {
            PermissionAnswer::AllowOnce
        } else {
            prompt.allow_answer(lifetime)
        };
        assert_eq!(
            prompt
                .handle_key(key(KeyCode::Char(shortcut)))
                .unwrap()
                .answer,
            expected
        );
        assert!(prompt.confirmation.is_none());
    }

    #[test_case("allow_exact"; "exact_call")]
    #[test_case("allow_exact_resources"; "exact_path_despite_broad_flag")]
    fn ordinary_read_scope_does_not_need_confirmation(option_id: &str) {
        let mut prompt = file_prompt("file_read");
        prompt.select_authority(option_id.into());
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert_eq!(
            prompt.handle_key(key(KeyCode::Char('a'))).unwrap().answer,
            PermissionAnswer::AllowOption {
                option_id: option_id.into(),
                lifetime: PermissionLifetime::Project
            }
        );
        assert!(prompt.confirmation.is_none());
    }

    #[test]
    fn scope_editor_retains_project_preference_but_never_approves_on_use() {
        let mut prompt = file_prompt("file_read");
        assert_eq!(prompt.lifetime, PermissionLifetime::Once);
        prompt.open_scope_editor();
        assert_eq!(prompt.lifetime, PermissionLifetime::Project);
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(prompt.handle_key(key(KeyCode::Char('p'))).is_none());
        assert!(prompt.panel == Panel::Main);
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        assert_eq!(
            prompt.handle_key(key(KeyCode::Char('y'))).unwrap().answer,
            PermissionAnswer::AllowOnce
        );
    }

    #[test_case("allow_commands_in_workdir"; "starting_directory")]
    #[test_case("allow_any_command"; "unbounded")]
    fn broad_shell_freezes_human_authority_and_required_phrase(option_id: &str) {
        let mut prompt = open_prompt();
        prompt.select_authority(option_id.into());
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(prompt.handle_key(key(KeyCode::Char('s'))).is_none());
        let confirmation = prompt.confirmation.as_ref().unwrap();
        let frozen = confirmation.answer.clone();
        let summary = confirmation.review.text();
        assert!(confirmation.complete, "{summary}");
        assert_eq!(prompt.confirmation_phrase(), Some(SHELL_PHRASE));
        for forbidden in [
            option_id,
            "input_digest",
            "subject",
            "attributes",
            "\"match\"",
            "rule family",
        ] {
            assert!(!summary.contains(forbidden), "{summary}");
        }
        assert!(summary.contains(super::super::scope::SHELL_REACH));
        prompt.select_authority("allow_exact".into());
        assert_eq!(prompt.confirmation.as_ref().unwrap().answer, frozen);
        prompt.selected_option = "allow_exact".into();
        prompt.scopes[0].rung = 0;
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        render(&mut prompt, 80, 18);
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        prompt.handle_paste("NOT THE REQUIRED PHRASE");
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        prompt.field.clear();
        prompt.handle_paste(SHELL_PHRASE);
        assert_eq!(
            prompt.handle_key(key(KeyCode::Enter)).unwrap().answer,
            frozen
        );
    }

    #[test]
    fn new_write_authority_keeps_its_required_phrase_and_project_binding() {
        let mut prompt = file_prompt("file_write");
        prompt.select_authority("allow_exact_resources".into());
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(prompt.handle_key(key(KeyCode::Char('a'))).is_none());
        assert_eq!(prompt.confirmation_phrase(), Some(WRITE_PHRASE));
        let summary = prompt.confirmation.as_ref().unwrap().review.text();
        assert!(summary.contains(FILE));
        assert!(summary.contains(PROJECT));
        assert!(!summary.contains("allow_exact_resources"));
        render(&mut prompt, 40, 10);
        assert!(prompt.handle_key(key(KeyCode::Char('y'))).is_none());
        prompt.field.clear();
        prompt.handle_paste(WRITE_PHRASE);
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_some());
    }

    #[test]
    fn global_exact_call_requires_a_short_fresh_confirmation() {
        let mut prompt = open_prompt();
        prompt.open_scope_editor();
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(prompt.handle_key(key(KeyCode::Char('A'))).is_none());
        assert_eq!(
            prompt.confirmation.as_ref().unwrap().impact,
            ApprovalImpact::Review
        );
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        render(&mut prompt, 80, 18);
        let mut released = key(KeyCode::Enter);
        released.kind = KeyEventKind::Release;
        prompt.handle_key(released);
        assert_eq!(
            prompt.handle_key(key(KeyCode::Enter)).unwrap().answer,
            prompt.allow_answer(PermissionLifetime::Global)
        );
    }

    #[test]
    fn only_selected_composed_grants_contribute_phrases() {
        let mut request = PermissionRequest::from_legacy(
            "compound".into(),
            ToolKey::native("bash"),
            vec!["git status".into(), "cargo test".into()],
            json!({"command": "git status && cargo test"}),
            Path::new("/project"),
            false,
        );
        request.presentation.project = Some(PROJECT.into());
        for (id, phrase) in [
            ("command_exact_0", FIRST_PHRASE),
            ("command_exact_1", SECOND_PHRASE),
        ] {
            request
                .options
                .iter_mut()
                .find(|option| option.id == id)
                .unwrap()
                .confirmation = Some(phrase.into());
        }
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(Box::new(request), None);
        prompt.scopes[1].rung = 0;
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        prompt.handle_key(key(KeyCode::Char('a')));
        assert_eq!(prompt.confirmation_phrase(), Some(FIRST_PHRASE));
        let confirmation = prompt.confirmation.as_ref().unwrap();
        assert_eq!(
            confirmation.answer,
            PermissionAnswer::AllowComposed {
                rows: vec![
                    Some(PermissionRowGrant::Offered("command_exact_0".into())),
                    None
                ],
                lifetime: PermissionLifetime::Project
            }
        );
        assert!(confirmation.review.authorities.iter().any(|authority| {
            authority.row == Some(1)
                && authority
                    .fields
                    .iter()
                    .any(|field| field.value == "None; runs with this call only.")
        }));
    }

    #[test]
    fn unavailable_preimage_never_borrows_an_option_label() {
        let mut prompt = file_prompt("file_read");
        let request = &mut prompt.requests.front_mut().unwrap().request;
        request.resources[0].value = "/different/path".into();
        let option = request
            .options
            .iter_mut()
            .find(|option| option.id == "allow_exact_resources")
            .unwrap();
        option.label = "safe exact file".into();
        let frozen = Confirmation::new(
            request,
            PermissionAnswer::AllowOption {
                option_id: "allow_exact_resources".into(),
                lifetime: PermissionLifetime::Project,
            },
            &[],
        );
        assert!(!frozen.complete);
        assert!(!frozen.review.text().contains("safe exact file"));
    }

    #[test_case(false; "missing_host_binding")]
    #[test_case(true; "nonlocal_executor")]
    fn project_is_not_inferred_from_execution_workdir(remote: bool) {
        let mut prompt = open_prompt();
        let request = &mut prompt.requests.front_mut().unwrap().request;
        if remote {
            request.executor = super::PermissionExecutorKind::RemoteWorkcell;
        } else {
            request.presentation.project = None;
        }
        prompt.open_scope_editor();
        assert_ne!(prompt.lifetime, PermissionLifetime::Project);
        assert!(!prompt.grants_lifetime(&PermissionLifetime::Project));
        prompt.handle_key(key(KeyCode::Char('p')));
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(prompt.handle_key(key(KeyCode::Char('a'))).is_none());
    }
}
