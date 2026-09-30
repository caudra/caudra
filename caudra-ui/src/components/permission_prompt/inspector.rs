use std::collections::BTreeSet;

use caudra_agent::permissions::{
    ComposedAnswerError, PermissionRequest, PermissionResourceSelector,
    pattern_matching::{CompiledPattern, PatternCompileError},
};
use caudra_storage::permission_patterns::{
    ArgumentDomain, ArgumentRole, ObservedTuple, OptionLikePolicy, PatternDefinition, PatternToken,
    PatternValidationError, SlotCombinations, SlotId,
};

use super::details::review_text;
use super::scope::{ApprovalImpact, SHELL_REACH, ScopeSummary, complete_text};
use super::{
    FooterRow, HINT_ENTER, HINT_ESC, KEY_ALLOW_ONCE, KeyCode, KeyEvent, KeyModifiers, Line, Panel,
    PermissionAnswer, PermissionDecision, PermissionPrompt, PermissionRowGrant,
    PermissionRuleOption, PromptTarget, Span, TextField, command_ladders,
};
use crate::components::permission_scope::controls::{DOMAIN_COUNT, domain_for_mode, domain_index};
pub(super) use crate::components::permission_scope::pattern::PatternControl as InspectorControl;
use crate::components::permission_scope::pattern::PatternPanel;
use crate::theme::Theme;

const EXCLUSIONS: &str = "Excludes extra arguments, redirects, expansions and payloads. Other commands need their own coverage.";
const MATCH_UNAVAILABLE: &str = "Current-row match unavailable. Change the scope or use Once.";
const MATCHED: &str =
    "Current row matches the policy preview; the whole call is rechecked on approval.";
const MATCH_MISMATCH: &str = "Current row does not match this scope. Change the scope or use Once.";
const SCOPE_UNAVAILABLE: &str = "Policy cannot use this edited scope for the current request.";
const UNKNOWN_ROLE_CAUTION: &str = "Unknown-role slots: values can change program operation. Widen with care; not read-only or sandboxed.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct EditedPattern {
    pub option_id: String,
    pub definition: Box<PatternDefinition>,
}

#[derive(Clone, PartialEq, Eq)]
enum EditField {
    Name,
    SlotName,
    Constraint,
}

#[derive(Debug, PartialEq, Eq)]
enum CurrentMatch {
    Matched,
    KnownMismatch,
    Unavailable,
}

pub(super) struct PatternInspector {
    row: usize,
    option_id: String,
    proposal: Box<PatternDefinition>,
    draft: Box<PatternDefinition>,
    slot: usize,
    editing: Option<EditField>,
    show_values: bool,
    error: Option<String>,
    current_match: CurrentMatch,
    current_bindings: Option<ObservedTuple>,
}

fn pattern_preview(
    request: &PermissionRequest,
    row: usize,
    option_id: &str,
    definition: &PatternDefinition,
) -> Result<(), ComposedAnswerError> {
    let lifetime = request
        .options
        .iter()
        .find(|option| option.id == option_id)
        .and_then(|option| option.allowed_lifetimes.first())
        .ok_or_else(|| ComposedAnswerError::NotOffered(option_id.into()))?;
    let mut rows = vec![None; request.resources.len()];
    let grant = rows.get_mut(row).ok_or(ComposedAnswerError::Uncovered)?;
    *grant = Some(PermissionRowGrant::Pattern {
        option_id: option_id.into(),
        definition: Box::new(definition.clone()),
    });
    request.composed_rules(&rows, lifetime).map(|_| ())
}

fn current_bindings(
    request: &PermissionRequest,
    row: usize,
    option_id: &str,
    proposal: &PatternDefinition,
) -> Option<ObservedTuple> {
    pattern_preview(request, row, option_id, proposal).ok()?;
    let mut probe = proposal.clone();
    if let SlotCombinations::ObservedTuples { tuples } = &proposal.combinations {
        return tuples
            .iter()
            .find(|tuple| {
                probe.combinations = SlotCombinations::ObservedTuples {
                    tuples: BTreeSet::from([(*tuple).clone()]),
                };
                pattern_preview(request, row, option_id, &probe).is_ok()
            })
            .cloned();
    }
    let mut bindings = ObservedTuple::new();
    for (index, slot) in proposal.slots.iter().enumerate() {
        let values = match &slot.domain {
            ArgumentDomain::ObservedSet { values } => values.clone(),
            ArgumentDomain::Exact { value } => BTreeSet::from([value.clone()]),
            _ => continue,
        };
        for value in values {
            probe.slots[index].domain = ArgumentDomain::Exact {
                value: value.clone(),
            };
            if pattern_preview(request, row, option_id, &probe).is_ok() {
                bindings.insert(slot.id, value);
                break;
            }
        }
        probe.slots[index].domain = slot.domain.clone();
    }
    Some(bindings)
}

fn matching_observed_tuples(
    definition: &PatternDefinition,
    tuples: &BTreeSet<ObservedTuple>,
) -> Result<BTreeSet<ObservedTuple>, PatternCompileError> {
    let mut probe = definition.clone();
    probe.combinations = SlotCombinations::Independent;
    CompiledPattern::compile(&probe)?;
    Ok(tuples
        .iter()
        .filter(|tuple| {
            probe.combinations = SlotCombinations::ObservedTuples {
                tuples: BTreeSet::from([(*tuple).clone()]),
            };
            CompiledPattern::compile(&probe).is_ok()
        })
        .cloned()
        .collect())
}

pub(super) fn offered_pattern(option: &PermissionRuleOption) -> Option<&PatternDefinition> {
    let [resource] = option.rule.resources.as_slice() else {
        return None;
    };
    match &resource.selector {
        PermissionResourceSelector::CommandTemplate { definition } => Some(definition),
        _ => None,
    }
}

pub(super) fn pattern_impact(definition: &PatternDefinition) -> ApprovalImpact {
    if definition.slots.iter().any(|slot| {
        matches!(
            slot.domain,
            ArgumentDomain::Glob { .. }
                | ArgumentDomain::Regex { .. }
                | ArgumentDomain::AnyLiteralArgument
        ) || slot.option_like == OptionLikePolicy::AllowForProvenData
    }) || (definition.slots.len() > 1
        && definition.combinations == SlotCombinations::Independent)
    {
        ApprovalImpact::Review
    } else {
        ApprovalImpact::Routine
    }
}

pub(super) fn unknown_role_caution(definition: &PatternDefinition) -> Option<&'static str> {
    definition
        .argv
        .iter()
        .any(|token| {
            matches!(
                token,
                PatternToken::Slot {
                    role: ArgumentRole::Unknown,
                    ..
                }
            )
        })
        .then_some(UNKNOWN_ROLE_CAUTION)
}

fn slot_name(definition: &PatternDefinition, id: SlotId) -> String {
    let index = definition
        .slots
        .iter()
        .position(|slot| slot.id == id)
        .unwrap_or_default();
    format!("<pattern{}>", index + 1)
}

fn literal(value: &str) -> String {
    shell_words::quote(&review_text(value)).into_owned()
}

fn template_words(definition: &PatternDefinition) -> Vec<(String, bool)> {
    definition
        .argv
        .iter()
        .map(|token| match token {
            PatternToken::Exact { value, .. } => (literal(value), false),
            PatternToken::Slot { id, .. } => (slot_name(definition, *id), true),
        })
        .collect()
}

fn template_text(definition: &PatternDefinition) -> String {
    template_words(definition)
        .into_iter()
        .map(|(word, _)| word)
        .collect::<Vec<_>>()
        .join(" ")
}

fn mode_name(domain: &ArgumentDomain) -> &'static str {
    match domain {
        ArgumentDomain::ObservedSet { .. } => "Observed values",
        ArgumentDomain::Exact { .. } => "Exact literal",
        ArgumentDomain::Glob { .. } => "Glob",
        ArgumentDomain::Regex { .. } => "Regex",
        ArgumentDomain::AnyLiteralArgument => "Any literal argument",
    }
}

fn domain_text(domain: &ArgumentDomain) -> String {
    match domain {
        ArgumentDomain::ObservedSet { values } => format!(
            "{} allowed: {}",
            values.len(),
            values
                .iter()
                .map(|value| literal(value))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ArgumentDomain::Exact { value } => literal(value),
        ArgumentDomain::Glob { pattern } | ArgumentDomain::Regex { pattern } => literal(pattern),
        ArgumentDomain::AnyLiteralArgument => "One argument; option guard stays fixed".into(),
    }
}

fn combinations_text(definition: &PatternDefinition) -> String {
    match &definition.combinations {
        SlotCombinations::ObservedTuples { tuples } => {
            format!("{} observed tuples only", tuples.len())
        }
        SlotCombinations::Independent => {
            let count =
                definition
                    .slots
                    .iter()
                    .try_fold(1usize, |count, slot| match &slot.domain {
                        ArgumentDomain::ObservedSet { values } => count.checked_mul(values.len()),
                        ArgumentDomain::Exact { .. } => Some(count),
                        _ => None,
                    });
            count.map_or_else(
                || "Independent; new combinations allowed".into(),
                |count| format!("Independent; up to {count} combinations"),
            )
        }
    }
}

fn compile_error(definition: &PatternDefinition, error: PatternCompileError) -> String {
    let message = match error {
        PatternCompileError::Expression { slot, reason } => {
            format!("{}: {reason}", slot_name(definition, slot))
        }
        PatternCompileError::InvalidTuple(slot) => format!(
            "An observed tuple is outside {}'s domain or fixed option guard",
            slot_name(definition, slot)
        ),
        PatternCompileError::Definition(PatternValidationError::InvalidSlot(slot)) => format!(
            "{} is not a valid variable position",
            slot_name(definition, slot)
        ),
        PatternCompileError::Definition(PatternValidationError::InvalidDomain(slot)) => format!(
            "{} has no valid allowed values",
            slot_name(definition, slot)
        ),
        other => other.to_string(),
    };
    review_text(&message)
}

pub(super) fn pattern_summary(definition: &PatternDefinition) -> ScopeSummary {
    let mut lines = vec![
        format!("Pattern name: {}", review_text(&definition.name)),
        format!(
            "Starting directory: {}",
            review_text(&definition.context.effective_workdir)
        ),
    ];
    for slot in &definition.slots {
        lines.push(format!(
            "{} ({}): {} — {}",
            slot_name(definition, slot.id),
            review_text(&slot.label),
            mode_name(&slot.domain),
            domain_text(&slot.domain)
        ));
        lines.push(option_guard(&slot.option_like).into());
    }
    lines.push(format!("Combinations: {}", combinations_text(definition)));
    lines.push(EXCLUSIONS.into());
    lines.push(SHELL_REACH.into());
    let validation = CompiledPattern::compile(definition);
    let complete = validation.is_ok() && lines.iter().all(|line| complete_text(line));
    if let Err(error) = validation {
        lines.push(format!("Invalid: {}", compile_error(definition, error)));
    }
    ScopeSummary {
        label: format!("Pattern: {}", template_text(definition)),
        lines,
        complete,
    }
}

fn option_guard(policy: &OptionLikePolicy) -> &'static str {
    match policy {
        OptionLikePolicy::Reject => "Option-looking values: rejected (fixed guard).",
        OptionLikePolicy::AllowForProvenData => {
            "Option-looking values: allowed only at proven data positions (fixed guard)."
        }
    }
}

impl PatternInspector {
    fn usable(&self) -> bool {
        self.error.is_none() && self.current_match == CurrentMatch::Matched
    }

    pub(super) fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    fn observed_values(&self) -> BTreeSet<String> {
        let Some(slot) = self.proposal.slots.get(self.slot) else {
            return BTreeSet::new();
        };
        match &self.proposal.combinations {
            SlotCombinations::ObservedTuples { tuples } => tuples
                .iter()
                .filter_map(|tuple| tuple.get(&slot.id).cloned())
                .collect(),
            SlotCombinations::Independent => match &slot.domain {
                ArgumentDomain::ObservedSet { values } => values.clone(),
                _ => BTreeSet::new(),
            },
        }
    }

    fn definition(&self, field: &TextField) -> Box<PatternDefinition> {
        let mut definition = self.draft.clone();
        if let Some(editing) = &self.editing {
            let text = field.text();
            match editing {
                EditField::Name => definition.name = text,
                EditField::SlotName => {
                    if let Some(slot) = definition.slots.get_mut(self.slot) {
                        slot.label = text;
                    }
                }
                EditField::Constraint => {
                    if let Some(slot) = definition.slots.get_mut(self.slot) {
                        match &mut slot.domain {
                            ArgumentDomain::Exact { value } => *value = text,
                            ArgumentDomain::Glob { pattern }
                            | ArgumentDomain::Regex { pattern } => *pattern = text,
                            _ => {}
                        }
                    }
                }
            }
        }
        if matches!(
            definition.combinations,
            SlotCombinations::ObservedTuples { .. }
        ) && let SlotCombinations::ObservedTuples { tuples } = &self.proposal.combinations
            && let Ok(tuples) = matching_observed_tuples(&definition, tuples)
        {
            definition.combinations = SlotCombinations::ObservedTuples { tuples };
        }
        definition
    }
}

impl PermissionPrompt {
    pub(super) fn suggested_pattern(&self) -> Option<&PermissionRuleOption> {
        let row = self.command_row()?;
        let ladders = command_ladders(self.current()?);
        let offered = ladders.get(row)?;
        self.scopes[row]
            .rung
            .checked_sub(1)
            .and_then(|rung| offered.get(rung).copied())
            .filter(|option| offered_pattern(option).is_some())
            .or_else(|| {
                offered
                    .iter()
                    .copied()
                    .find(|option| offered_pattern(option).is_some())
            })
    }

    pub(super) fn open_inspector(&mut self) {
        let Some(row) = self.command_row() else {
            return;
        };
        let Some(option) = self.suggested_pattern() else {
            return;
        };
        let Some(proposal) = offered_pattern(option) else {
            return;
        };
        let draft = self.scopes[row]
            .pattern
            .as_ref()
            .filter(|edited| edited.option_id == option.id)
            .map_or_else(
                || Box::new(proposal.clone()),
                |edited| edited.definition.clone(),
            );
        self.inspector = Some(PatternInspector {
            row,
            option_id: option.id.clone(),
            proposal: Box::new(proposal.clone()),
            draft,
            slot: 0,
            editing: None,
            show_values: false,
            error: None,
            current_match: CurrentMatch::Unavailable,
            current_bindings: self
                .current()
                .and_then(|request| current_bindings(request, row, &option.id, proposal)),
        });
        self.panel = Panel::Scopes;
        self.field.clear();
        self.scroll.reset();
        self.refresh_inspector(None);
    }

    pub(super) fn refresh_inspector(&mut self, focus: Option<InspectorControl>) {
        let Some(inspector) = &self.inspector else {
            return;
        };
        let definition = inspector.definition(&self.field);
        let mut error = CompiledPattern::compile(&definition)
            .err()
            .map(|error| compile_error(&definition, error));
        let current_match = if error.is_none()
            && let Some(request) = self.current()
        {
            match pattern_preview(request, inspector.row, &inspector.option_id, &definition) {
                Ok(()) => CurrentMatch::Matched,
                Err(ComposedAnswerError::Uncovered) if inspector.current_bindings.is_some() => {
                    CurrentMatch::KnownMismatch
                }
                Err(ComposedAnswerError::Uncovered) => CurrentMatch::Unavailable,
                Err(_) => {
                    error = Some(SCOPE_UNAVAILABLE.into());
                    CurrentMatch::Unavailable
                }
            }
        } else {
            CurrentMatch::Unavailable
        };
        if let Some(inspector) = &mut self.inspector {
            inspector.error = error;
            inspector.current_match = current_match;
        }
        self.invalidate_controls();
        self.focus = focus.map(PromptTarget::Inspector);
        self.pending_reveal = self.focus.clone();
    }

    fn set_domain(&mut self, mode: usize) {
        let Some(inspector) = &mut self.inspector else {
            return;
        };
        let values = inspector.observed_values();
        let Some(slot) = inspector.draft.slots.get_mut(inspector.slot) else {
            return;
        };
        let exact = inspector
            .current_bindings
            .as_ref()
            .and_then(|bindings| bindings.get(&slot.id))
            .cloned();
        slot.domain = domain_for_mode(mode, values, exact);
        inspector.show_values = false;
        self.refresh_inspector(Some(InspectorControl::Mode));
    }

    fn move_inspector_slot(&mut self, forward: bool) {
        let Some(inspector) = &mut self.inspector else {
            return;
        };
        let len = inspector.draft.slots.len();
        if len == 0 {
            return;
        }
        inspector.slot = if forward {
            (inspector.slot + 1) % len
        } else {
            (inspector.slot + len - 1) % len
        };
        inspector.show_values = false;
        self.refresh_inspector(Some(InspectorControl::Slot));
    }

    fn cycle_domain(&mut self, forward: bool) {
        let Some(slot) = self
            .inspector
            .as_ref()
            .and_then(|inspector| inspector.draft.slots.get(inspector.slot))
        else {
            return;
        };
        let index = domain_index(&slot.domain);
        self.set_domain(if forward {
            (index + 1) % DOMAIN_COUNT
        } else {
            (index + DOMAIN_COUNT - 1) % DOMAIN_COUNT
        });
    }

    pub(super) fn activate_inspector(&mut self, control: InspectorControl) {
        self.apply_inspector_edit();
        let Some(inspector) = &mut self.inspector else {
            return;
        };
        let edit = match control {
            InspectorControl::Name => Some((EditField::Name, inspector.draft.name.clone())),
            InspectorControl::SlotName => inspector
                .draft
                .slots
                .get(inspector.slot)
                .map(|slot| (EditField::SlotName, slot.label.clone())),
            InspectorControl::Constraint => {
                inspector
                    .draft
                    .slots
                    .get(inspector.slot)
                    .and_then(|slot| match &slot.domain {
                        ArgumentDomain::Exact { value } => {
                            Some((EditField::Constraint, value.clone()))
                        }
                        ArgumentDomain::Glob { pattern } | ArgumentDomain::Regex { pattern } => {
                            Some((EditField::Constraint, pattern.clone()))
                        }
                        _ => None,
                    })
            }
            _ => None,
        };
        if let Some((field, value)) = edit {
            inspector.editing = Some(field);
            self.field.set_text(&value);
            self.refresh_inspector(Some(control));
            return;
        }
        match control {
            InspectorControl::SelectSlot(id) => {
                if let Some(slot) = inspector.draft.slots.iter().position(|slot| slot.id == id) {
                    inspector.slot = slot;
                    inspector.show_values = false;
                }
            }
            InspectorControl::Slot => {
                self.move_inspector_slot(true);
                return;
            }
            InspectorControl::Mode => {
                self.cycle_domain(true);
                return;
            }
            InspectorControl::Combinations => {
                inspector.draft.combinations = match &inspector.draft.combinations {
                    SlotCombinations::ObservedTuples { .. } => SlotCombinations::Independent,
                    SlotCombinations::Independent => inspector.proposal.combinations.clone(),
                };
            }
            InspectorControl::Constraint | InspectorControl::Observations => {
                inspector.show_values = !inspector.show_values
            }
            InspectorControl::ObservedValue(index) => {
                let value = inspector.observed_values().into_iter().nth(index);
                if let Some(value) = value
                    && let Some(slot) = inspector.draft.slots.get_mut(inspector.slot)
                    && let ArgumentDomain::ObservedSet { values } = &mut slot.domain
                    && !values.remove(&value)
                {
                    values.insert(value);
                }
            }
            _ => {}
        }
        self.refresh_inspector(Some(control));
    }

    pub(super) fn handle_inspector_key(&mut self, key: KeyEvent) -> Option<PermissionDecision> {
        if self.inspector.as_ref()?.is_editing() {
            let control = match self.inspector.as_ref()?.editing.as_ref()? {
                EditField::Name => InspectorControl::Name,
                EditField::SlotName => InspectorControl::SlotName,
                EditField::Constraint => InspectorControl::Constraint,
            };
            match key.code {
                KeyCode::Esc => {
                    self.inspector.as_mut()?.editing = None;
                    self.field.clear();
                }
                KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab => {
                    self.apply_inspector_edit();
                }
                _ => {
                    self.edit_field(key);
                }
            }
            self.refresh_inspector(Some(control));
            if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
                self.move_focus(
                    key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT),
                );
            }
            return None;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match key.code {
            KeyCode::Esc => {
                self.inspector = None;
                self.field.clear();
                self.panel = Panel::Scopes;
                self.scroll.reset();
                self.invalidate_controls();
            }
            KeyCode::Tab | KeyCode::BackTab => self.move_focus(
                key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT),
            ),
            KeyCode::Enter => {
                if let Some(target) = self.focus.clone() {
                    return self.activate(target);
                }
            }
            KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => {
                let forward = matches!(key.code, KeyCode::Down | KeyCode::Right);
                match self.focus.as_ref() {
                    Some(PromptTarget::Inspector(InspectorControl::Slot)) => {
                        self.move_inspector_slot(forward)
                    }
                    Some(PromptTarget::Inspector(InspectorControl::Mode)) => {
                        self.cycle_domain(forward)
                    }
                    Some(PromptTarget::Inspector(InspectorControl::Combinations)) => {
                        self.activate_inspector(InspectorControl::Combinations)
                    }
                    _ if matches!(key.code, KeyCode::Up | KeyCode::Down) => {
                        self.move_focus(!forward)
                    }
                    _ => {}
                }
            }
            KeyCode::Char('1'..='5') => {
                if let KeyCode::Char(mode) = key.code {
                    self.set_domain(mode as usize - '1' as usize);
                }
            }
            KeyCode::Char('N') => self.activate_inspector(InspectorControl::Name),
            KeyCode::Char('n') => self.activate_inspector(InspectorControl::SlotName),
            KeyCode::Char('e') => self.activate_inspector(InspectorControl::Constraint),
            KeyCode::Char('o') => self.activate_inspector(InspectorControl::Observations),
            KeyCode::Char('c') => self.activate_inspector(InspectorControl::Combinations),
            KeyCode::F(2) => {
                self.panel = Panel::Details;
                self.scroll.reset();
                self.invalidate_controls();
            }
            KeyCode::Char('p') if !self.awaiting_review => self.use_inspected_pattern(),
            KeyCode::Char('y') if !self.awaiting_review => {
                return Some(PermissionDecision {
                    request_id: self.request_id()?.into(),
                    answer: PermissionAnswer::AllowOnce,
                });
            }
            _ => {
                self.scroll.handle_key(key);
            }
        }
        None
    }

    fn use_inspected_pattern(&mut self) {
        let Some(inspector) = &self.inspector else {
            return;
        };
        if !inspector.usable() {
            return;
        }
        let Some(request) = self.current() else {
            return;
        };
        let definition = inspector.definition(&self.field);
        if pattern_preview(request, inspector.row, &inspector.option_id, &definition).is_err() {
            self.refresh_inspector(None);
            return;
        }
        let ladders = command_ladders(request);
        let Some(rung) = ladders.get(inspector.row).and_then(|offered| {
            offered
                .iter()
                .position(|option| option.id == inspector.option_id)
        }) else {
            return;
        };
        let Some(inspector) = self.inspector.take() else {
            return;
        };
        let choice = &mut self.scopes[inspector.row];
        choice.pattern = Some(EditedPattern {
            option_id: inspector.option_id,
            definition,
        });
        choice.rung = rung + 1;
        self.panel = Panel::Main;
        self.scroll.reset();
        self.scope_changed();
    }

    fn apply_inspector_edit(&mut self) {
        if let Some(inspector) = &self.inspector
            && inspector.is_editing()
        {
            let definition = inspector.definition(&self.field);
            if let Some(inspector) = &mut self.inspector {
                inspector.draft = definition;
                inspector.editing = None;
            }
            self.field.clear();
        }
    }

    pub(super) fn inspector_footer(&self) -> Vec<FooterRow> {
        let Some(inspector) = &self.inspector else {
            return Vec::new();
        };
        if inspector.is_editing() {
            return vec![
                FooterRow::InspectorStatus,
                FooterRow::ConfirmationInput,
                FooterRow::Hints(vec![(HINT_ENTER, "Apply edit"), (HINT_ESC, "Cancel edit")]),
            ];
        }
        let mut actions = Vec::new();
        if inspector.usable() {
            actions.push(("p", "Use scope"));
        }
        actions.extend([
            (KEY_ALLOW_ONCE, "Once"),
            ("F2", "Details"),
            (HINT_ESC, "Back"),
        ]);
        let mut rows = vec![FooterRow::InspectorStatus];
        if !inspector.draft.slots.is_empty() {
            rows.push(FooterRow::Hints(vec![
                ("1", "Values"),
                ("2", "Exact"),
                ("3", "Glob"),
                ("4", "Regex"),
                ("5", "Any"),
            ]));
        }
        rows.push(FooterRow::Hints(actions));
        rows
    }

    pub(super) fn inspector_status_line(&self, t: &Theme) -> Line<'static> {
        let Some(inspector) = &self.inspector else {
            return Line::default();
        };
        match &inspector.error {
            Some(error) => Line::from(Span::styled(format!("Invalid: {error}"), t.error)),
            None if inspector.current_match == CurrentMatch::KnownMismatch => {
                Line::from(Span::styled(MATCH_MISMATCH, t.error))
            }
            None if inspector.current_match == CurrentMatch::Unavailable => {
                Line::styled(MATCH_UNAVAILABLE, t.tool_warning)
            }
            None => Line::from(Span::styled(MATCHED, t.tool_dim)),
        }
    }

    pub(super) fn inspector_panel(&self) -> Option<PatternPanel> {
        let inspector = self.inspector.as_ref()?;
        let definition = inspector.definition(&self.field);
        let caution = unknown_role_caution(&definition).map(str::to_owned);
        let evidence = self
            .current()
            .and_then(|request| {
                request
                    .options
                    .iter()
                    .find(|option| option.id == inspector.option_id)
            })
            .map_or_else(String::new, |option| review_text(&option.description));
        Some(PatternPanel {
            definition,
            slot: inspector.slot,
            supplied: inspector.observed_values(),
            show_values: inspector.show_values,
            caution,
            evidence,
        })
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use caudra_agent::permissions::{
        COMMAND_OBSERVATION_ATTRIBUTE, COMMAND_OBSERVATION_BINDING_ATTRIBUTE, PermissionLifetime,
        PermissionRequest, PermissionRowGrant,
        pattern_recognition::{
            CommandObservation, InvocationOutcome, OBSERVATION_SCHEMA_VERSION,
            ObservationProvenance, ObservationSource, ObservationVerification, ShellEffectStatus,
        },
        prepared_command_binding,
    };
    use caudra_storage::permission_patterns::{
        ArgumentRole, PATTERN_SCHEMA_VERSION, PatternContext, PatternSlot,
    };
    use caudra_storage::permission_state::validate_command_templates;
    use crossterm::event::{KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
    use serde_json::json;
    use test_case::test_case;

    use super::super::decision::tests::native_shell_request;
    use super::super::details::{MAX_REVIEW_CHARS, TRUNCATED};
    use super::super::scope::ReviewDocument;
    use super::super::view::tests::{key, render};
    use super::super::{Panel, PromptMouse};
    use super::{
        ArgumentDomain, CompiledPattern, CurrentMatch, InspectorControl, MATCH_MISMATCH,
        MATCH_UNAVAILABLE, OptionLikePolicy, PatternDefinition, PatternToken, PermissionAnswer,
        PermissionPrompt, PermissionResourceSelector, PromptTarget, SlotCombinations, SlotId,
        UNKNOWN_ROLE_CAUTION, offered_pattern, template_text, unknown_role_caution,
    };
    use super::{KeyCode, KeyModifiers};
    use crate::theme;

    const OPTION_ID: &str = "command_template_0";
    const EVIDENCE: &str = "Native observations: 12 observations across 3 sessions. Outcomes: 12 requested (execution outcome not recorded).";
    const IMPORTED_EVIDENCE: &str = "Imported history (unverified): 12 observations across 3 sessions. Outcomes: 12 unknown outcomes. Historical execution context and tool identity are unverified. Analysis assumes standard Bash startup.";
    const EVIDENCE_LABEL: &str = "Proposal evidence: ";
    const UI_VALUE: &str = "caudra-ui";
    const AGENT_VALUE: &str = "caudra-agent";
    const SLOT: SlotId = SlotId(9);
    const SECOND_SLOT: SlotId = SlotId(17);
    const TEMPLATE_PHRASE: &str = "REVIEW TEMPLATE";
    const EXPECTED_TEMPLATE: &str = "cargo check -p <pattern1> --tests";
    const EXPECTED_TUPLE: &str = "\"caudra-agent\" │ \"arm\"";
    const INVALID_EXPRESSION: &str = "[";
    const EDITED_NAME: &str = "Reviewed checks";
    const DISCARDED_NAME: &str = "Discard this edit";
    const UNOBSERVED_VALUE: &str = "not-observed";
    const ESCAPED_VALUE: &str = "src/[a];b";
    const OTHER_PATH_VALUE: &str = "src/other";
    const ESCAPED_EXPRESSION: &str = r"src/\[a\];b";
    const POSSIBLE_WORKDIRS: &str = "possible_workdirs";
    const WIDE: u16 = 140;
    const TALL: u16 = 48;

    fn definition() -> PatternDefinition {
        PatternDefinition {
            version: PATTERN_SCHEMA_VERSION,
            name: "Cargo checks".into(),
            context: PatternContext {
                tool_identity: "fixture-shell".into(),
                executable_identity: "fixture-cargo".into(),
                effective_workdir: "/project".into(),
                path_binding: "opaque-fixture-binding".into(),
                analysis_version: "fixture-analysis".into(),
            },
            argv: vec![
                PatternToken::Exact {
                    value: "cargo".into(),
                    role: ArgumentRole::Executable,
                },
                PatternToken::Exact {
                    value: "check".into(),
                    role: ArgumentRole::Operation,
                },
                PatternToken::Exact {
                    value: "-p".into(),
                    role: ArgumentRole::Flag,
                },
                PatternToken::Slot {
                    id: SLOT,
                    role: ArgumentRole::Data,
                },
                PatternToken::Exact {
                    value: "--tests".into(),
                    role: ArgumentRole::Flag,
                },
            ],
            slots: vec![PatternSlot {
                id: SLOT,
                label: "<pattern1>".into(),
                domain: ArgumentDomain::ObservedSet {
                    values: BTreeSet::from([AGENT_VALUE.into(), UI_VALUE.into()]),
                },
                option_like: OptionLikePolicy::Reject,
            }],
            combinations: SlotCombinations::ObservedTuples {
                tuples: BTreeSet::from([
                    BTreeMap::from([(SLOT, AGENT_VALUE.into())]),
                    BTreeMap::from([(SLOT, UI_VALUE.into())]),
                ]),
            },
        }
    }

    fn offered_request(id: &str, definition: PatternDefinition) -> Box<PermissionRequest> {
        let mut observation = preview(&definition, UI_VALUE);
        if let SlotCombinations::ObservedTuples { tuples } = &definition.combinations
            && let Some(bindings) = tuples
                .iter()
                .find(|tuple| tuple.get(&SLOT).is_some_and(|value| value == UI_VALUE))
                .or_else(|| tuples.first())
        {
            for (index, token) in definition.argv.iter().enumerate() {
                if let PatternToken::Slot { id, .. } = token {
                    observation.argv[index].clone_from(&bindings[id]);
                }
            }
        }
        let command = observation
            .argv
            .iter()
            .map(|word| shell_words::quote(word).into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let mut request = native_shell_request(&command);
        request.id = id.into();
        let binding = prepared_command_binding(&request.resources[0].value, &request.input);
        observation.source.input_hash.clone_from(&binding);
        request.resources[0].attributes.insert(
            COMMAND_OBSERVATION_ATTRIBUTE.into(),
            serde_json::to_string(&observation).unwrap(),
        );
        request.resources[0]
            .attributes
            .insert(COMMAND_OBSERVATION_BINDING_ATTRIBUTE.into(), binding);
        let mut option = request
            .options
            .iter()
            .find(|option| option.id == "command_exact_0")
            .unwrap()
            .clone();
        option.id = OPTION_ID.into();
        option.label = "Suggested Cargo checks".into();
        option.description = EVIDENCE.into();
        option.is_default = false;
        option.rule.resources[0].selector = PermissionResourceSelector::CommandTemplate {
            definition: Box::new(definition),
        };
        option.rule.resources[0]
            .attributes
            .remove(POSSIBLE_WORKDIRS);
        validate_command_templates(&option.rule).unwrap();
        request.options.insert(0, option);
        Box::new(request)
    }

    pub(crate) fn suggested_prompt() -> PermissionPrompt {
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(offered_request("pattern", definition()), None);
        prompt
    }

    fn inspect(prompt: &mut PermissionPrompt) {
        prompt.handle_key(key(KeyCode::Char('r')));
        prompt.handle_key(key(KeyCode::Char('i')));
        assert!(prompt.inspector.is_some());
        render(prompt, WIDE, TALL);
    }

    fn edit(prompt: &mut PermissionPrompt, shortcut: char, text: &str) {
        prompt.handle_key(key(KeyCode::Char(shortcut)));
        prompt.field.clear();
        assert!(prompt.handle_paste(text));
        prompt.handle_key(key(KeyCode::Enter));
        render(prompt, WIDE, TALL);
    }

    fn take_scope(prompt: &mut PermissionPrompt) -> Box<PatternDefinition> {
        render(prompt, WIDE, TALL);
        assert!(prompt.handle_key(key(KeyCode::Char('p'))).is_none());
        assert!(prompt.inspector.is_none());
        let Some(PermissionRowGrant::Pattern { definition, .. }) = prompt
            .row_grants(prompt.current().unwrap())
            .into_iter()
            .next()
            .unwrap()
        else {
            panic!("edited pattern was not selected");
        };
        definition
    }

    fn click(prompt: &mut PermissionPrompt, target: PromptTarget) {
        let area = prompt
            .row_hits
            .iter()
            .find(|hit| hit.target == target)
            .unwrap()
            .area;
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            assert!(matches!(
                prompt.handle_mouse(MouseEvent {
                    kind,
                    column: area.x,
                    row: area.y,
                    modifiers: KeyModifiers::NONE,
                }),
                PromptMouse::Consumed
            ));
        }
    }

    fn preview(definition: &PatternDefinition, argument: &str) -> CommandObservation {
        CommandObservation {
            version: OBSERVATION_SCHEMA_VERSION,
            argv: definition
                .argv
                .iter()
                .map(|token| match token {
                    PatternToken::Exact { value, .. } => value.clone(),
                    PatternToken::Slot { .. } => argument.into(),
                })
                .collect(),
            roles: definition
                .argv
                .iter()
                .map(|token| match token {
                    PatternToken::Exact { role, .. } | PatternToken::Slot { role, .. } => {
                        role.clone()
                    }
                })
                .collect(),
            context: definition.context.clone(),
            verification: ObservationVerification {
                complete_command: true,
                static_argv: true,
                context_verified: true,
                sensitivity_checked: true,
                shell_effects: ShellEffectStatus::Absent,
            },
            source: ObservationSource {
                source_identity: "fixture".into(),
                observation_id: "preview".into(),
                input_hash: "0".repeat(64),
                session_id: "preview".into(),
                timestamp_ms: 1,
                provenance: ObservationProvenance::Native,
                outcome: InvocationOutcome::Requested,
            },
        }
    }

    #[test]
    fn suggestions_never_replace_the_exact_default_or_once_answer() {
        let mut prompt = suggested_prompt();
        assert_eq!(
            prompt.row_grants(prompt.current().unwrap()),
            vec![Some(PermissionRowGrant::Offered("command_exact_0".into()))]
        );
        render(&mut prompt, 80, 18);
        assert_eq!(
            prompt.handle_key(key(KeyCode::Char('y'))).unwrap().answer,
            PermissionAnswer::AllowOnce
        );
        assert!(prompt.scopes[0].pattern.is_none());
    }

    #[test_case('1', ""; "observed_set")]
    #[test_case('2', UI_VALUE; "exact_literal")]
    #[test_case('3', "caudra-*"; "glob")]
    #[test_case('4', "caudra-(agent|ui)"; "regex")]
    #[test_case('5', ""; "any_literal_argument")]
    fn every_domain_mode_preserves_the_offered_structure(mode: char, constraint: &str) {
        let mut prompt = suggested_prompt();
        let original = prompt.current().unwrap().clone();
        inspect(&mut prompt);
        prompt.handle_key(key(KeyCode::Char(mode)));
        if !constraint.is_empty() {
            edit(&mut prompt, 'e', constraint);
        }
        let definition = take_scope(&mut prompt);
        assert_eq!(definition.context, self::definition().context);
        assert_eq!(definition.argv, self::definition().argv);
        assert_eq!(definition.slots[0].id, SLOT);
        assert_eq!(definition.slots[0].option_like, OptionLikePolicy::Reject);
        assert_eq!(prompt.current().unwrap(), &original);
        match (mode, &definition.slots[0].domain) {
            ('1', ArgumentDomain::ObservedSet { values }) => assert_eq!(values.len(), 2),
            ('2', ArgumentDomain::Exact { value }) => assert_eq!(value, UI_VALUE),
            ('3', ArgumentDomain::Glob { pattern }) | ('4', ArgumentDomain::Regex { pattern }) => {
                assert_eq!(pattern, constraint)
            }
            ('5', ArgumentDomain::AnyLiteralArgument) => {}
            _ => panic!("wrong argument domain"),
        }
        render(&mut prompt, WIDE, TALL);
        let answer = prompt.allow_answer(PermissionLifetime::Conversation);
        let decision = prompt.handle_key(key(KeyCode::Char('s')));
        if matches!(mode, '3' | '4' | '5') {
            assert!(decision.is_none());
            assert!(prompt.confirmation_phrase().is_none());
            let frozen = prompt.confirmation.as_ref().unwrap();
            assert!(frozen.complete, "{}", frozen.review.text());
            let summary = frozen.review.text();
            assert!(summary.contains(EXPECTED_TEMPLATE));
            assert!(summary.contains("/project"));
            assert!(!summary.contains("opaque-fixture-binding"));
            assert!(!summary.contains(OPTION_ID));
            assert!(!summary.contains(&definition.fingerprint().unwrap()));
            render(&mut prompt, WIDE, TALL);
            assert_eq!(
                prompt.handle_key(key(KeyCode::Enter)).unwrap().answer,
                answer
            );
        } else {
            assert_eq!(decision.unwrap().answer, answer);
        }
    }

    #[test]
    fn unknown_role_caution_uses_variable_token_roles_not_display_text() {
        let mut definition = definition();
        definition.name = UNKNOWN_ROLE_CAUTION.into();
        definition.slots[0].label = UNKNOWN_ROLE_CAUTION.into();
        definition.argv[1] = PatternToken::Exact {
            value: UNKNOWN_ROLE_CAUTION.into(),
            role: ArgumentRole::Unknown,
        };
        assert_eq!(unknown_role_caution(&definition), None);
        definition.argv[3] = PatternToken::Slot {
            id: SLOT,
            role: ArgumentRole::Unknown,
        };
        assert_eq!(
            unknown_role_caution(&definition),
            Some(UNKNOWN_ROLE_CAUTION)
        );
    }

    fn unknown_joint_definition() -> PatternDefinition {
        let mut definition = joint_definition();
        for token in &mut definition.argv {
            if let PatternToken::Slot { role, .. } = token {
                *role = ArgumentRole::Unknown;
            }
        }
        definition
    }

    #[test_case('1', ""; "observed_set")]
    #[test_case('2', UI_VALUE; "exact_literal")]
    #[test_case('3', "caudra-*"; "glob")]
    #[test_case('4', "caudra-(agent|ui)"; "regex")]
    #[test_case('5', ""; "any_literal_argument")]
    fn unknown_slots_warn_once_without_changing_roles_or_default_tuples(
        mode: char,
        constraint: &str,
    ) {
        let original = unknown_joint_definition();
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(offered_request("unknown", original.clone()), None);
        inspect(&mut prompt);
        assert_eq!(prompt.inspector.as_ref().unwrap().draft.as_ref(), &original);
        prompt.handle_key(key(KeyCode::Char(mode)));
        if !constraint.is_empty() {
            edit(&mut prompt, 'e', constraint);
        }
        let panel = prompt.inspector_panel().unwrap();
        assert_eq!(panel.caution.as_deref(), Some(UNKNOWN_ROLE_CAUTION));
        prompt.scroll.scroll_to(0);
        assert!(render(&mut prompt, WIDE, TALL).contains(UNKNOWN_ROLE_CAUTION));
        let selected = take_scope(&mut prompt);
        assert_eq!(selected.argv, original.argv);
        assert!(matches!(
            selected.combinations,
            SlotCombinations::ObservedTuples { .. }
        ));
        render(&mut prompt, WIDE, TALL);
        let decision = prompt.handle_key(key(KeyCode::Char('s')));
        if matches!(mode, '3' | '4' | '5') {
            assert!(decision.is_none());
            let frozen = prompt.confirmation.as_ref().unwrap();
            assert!(frozen.complete, "{}", frozen.review.text());
            assert_eq!(
                frozen.review.text().matches(UNKNOWN_ROLE_CAUTION).count(),
                1
            );
            assert!(render(&mut prompt, WIDE, TALL).contains(UNKNOWN_ROLE_CAUTION));
        } else {
            assert!(decision.is_some());
        }
    }

    #[test_case(false; "offered_pattern")]
    #[test_case(true; "edited_pattern")]
    fn unknown_role_caution_is_in_the_bounded_frozen_review(edited: bool) {
        let definition = unknown_joint_definition();
        let request = offered_request("unknown", definition.clone());
        let answer = if edited {
            PermissionAnswer::AllowComposed {
                rows: vec![Some(PermissionRowGrant::Pattern {
                    option_id: OPTION_ID.into(),
                    definition: Box::new(definition),
                })],
                lifetime: PermissionLifetime::Conversation,
            }
        } else {
            PermissionAnswer::AllowOption {
                option_id: OPTION_ID.into(),
                lifetime: PermissionLifetime::Conversation,
            }
        };
        let mut review = ReviewDocument::new(&request, &answer);
        assert!(review.bound());
        assert_eq!(review.text().matches(UNKNOWN_ROLE_CAUTION).count(), 1);
        review.action = "x".repeat(MAX_REVIEW_CHARS);
        assert!(!review.bound());
        assert!(
            review
                .warnings
                .iter()
                .any(|warning| warning.contains(TRUNCATED))
        );
    }

    #[test]
    fn names_change_display_not_fingerprint_or_neutral_slot_tokens() {
        let mut prompt = suggested_prompt();
        let fingerprint = definition().fingerprint().unwrap();
        inspect(&mut prompt);
        edit(&mut prompt, 'N', "Build checks");
        edit(&mut prompt, 'n', "crate");
        let definition = take_scope(&mut prompt);
        assert_eq!(definition.name, "Build checks");
        assert_eq!(definition.slots[0].label, "crate");
        assert_eq!(definition.fingerprint().unwrap(), fingerprint);
        assert_eq!(template_text(&definition), EXPECTED_TEMPLATE);
        assert_eq!(
            offered_pattern(&prompt.current().unwrap().options[0])
                .unwrap()
                .name,
            "Cargo checks"
        );
    }

    #[test_case('3'; "invalid_glob")]
    #[test_case('4'; "invalid_regex")]
    fn invalid_expressions_disable_use_but_not_once(mode: char) {
        let mut prompt = suggested_prompt();
        inspect(&mut prompt);
        prompt.handle_key(key(KeyCode::Char(mode)));
        prompt.handle_key(key(KeyCode::Char('e')));
        prompt.field.clear();
        prompt.handle_paste(INVALID_EXPRESSION);
        assert!(prompt.inspector.as_ref().unwrap().error.is_some());
        prompt.handle_key(key(KeyCode::Enter));
        let screen = render(&mut prompt, 40, 10);
        assert!(screen.contains("Invalid:"));
        assert!(
            !prompt
                .row_hits
                .iter()
                .any(|hit| hit.target == PromptTarget::Hint(key(KeyCode::Char('p'))))
        );
        assert!(prompt.handle_key(key(KeyCode::Char('p'))).is_none());
        assert!(prompt.inspector.is_some());
        assert_eq!(
            prompt.handle_key(key(KeyCode::Char('y'))).unwrap().answer,
            PermissionAnswer::AllowOnce
        );
        assert!(prompt.scopes[0].pattern.is_none());
    }

    #[test_case('2', "caudra-ui", "caudra-ui", "caudra-ui-more"; "exact_whole_argument")]
    #[test_case('3', "caudra-*", "caudra-ui", "other/caudra-ui"; "glob_whole_argument")]
    #[test_case('4', "caudra-ui|caudra-agent", "caudra-ui", "x-caudra-ui-y"; "regex_alternation_is_whole_argument")]
    fn editor_uses_core_whole_argument_matching(
        mode: char,
        constraint: &str,
        positive: &str,
        negative: &str,
    ) {
        let mut prompt = suggested_prompt();
        inspect(&mut prompt);
        prompt.handle_key(key(KeyCode::Char('c')));
        prompt.handle_key(key(KeyCode::Char(mode)));
        edit(&mut prompt, 'e', constraint);
        let definition = take_scope(&mut prompt);
        let compiled = CompiledPattern::compile(&definition).unwrap();
        assert!(
            compiled
                .matches(&preview(&definition, positive))
                .unwrap()
                .is_match()
        );
        assert!(
            !compiled
                .matches(&preview(&definition, negative))
                .unwrap()
                .is_match()
        );
    }

    fn joint_definition() -> PatternDefinition {
        let mut definition = definition();
        definition.argv.insert(
            4,
            PatternToken::Exact {
                value: "--target".into(),
                role: ArgumentRole::Flag,
            },
        );
        definition.argv.insert(
            5,
            PatternToken::Slot {
                id: SECOND_SLOT,
                role: ArgumentRole::Data,
            },
        );
        definition.slots.push(PatternSlot {
            id: SECOND_SLOT,
            label: "<pattern2>".into(),
            domain: ArgumentDomain::ObservedSet {
                values: BTreeSet::from(["arm".into(), "x86".into()]),
            },
            option_like: OptionLikePolicy::Reject,
        });
        definition.combinations = SlotCombinations::ObservedTuples {
            tuples: BTreeSet::from([
                BTreeMap::from([(SLOT, AGENT_VALUE.into()), (SECOND_SLOT, "arm".into())]),
                BTreeMap::from([(SLOT, UI_VALUE.into()), (SECOND_SLOT, "x86".into())]),
            ]),
        };
        definition
    }

    #[test]
    fn observed_values_only_narrow_joint_tuples_until_independence_is_explicit() {
        let definition = joint_definition();
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(offered_request("joint", definition.clone()), None);
        inspect(&mut prompt);
        prompt.handle_key(key(KeyCode::Char('o')));
        let screen = render(&mut prompt, WIDE, TALL);
        assert!(screen.contains(EXPECTED_TUPLE));
        prompt.activate_inspector(InspectorControl::ObservedValue(0));
        let edited = prompt.inspector.as_ref().unwrap().definition(&prompt.field);
        assert!(
            matches!(&edited.combinations, SlotCombinations::ObservedTuples { tuples } if tuples.len() == 1)
        );
        prompt.activate_inspector(InspectorControl::ObservedValue(0));
        prompt.handle_key(key(KeyCode::Char('c')));
        let screen = render(&mut prompt, WIDE, TALL);
        assert!(screen.contains("INDEPENDENT"));
        assert_eq!(
            prompt.inspector.as_ref().unwrap().draft.combinations,
            SlotCombinations::Independent
        );
        prompt.handle_key(key(KeyCode::Char('c')));
        assert_eq!(
            prompt
                .inspector
                .as_ref()
                .unwrap()
                .definition(&prompt.field)
                .combinations,
            definition.combinations
        );
        prompt.handle_key(key(KeyCode::Char('c')));
        let edited = take_scope(&mut prompt);
        assert_eq!(edited.combinations, SlotCombinations::Independent);
        render(&mut prompt, WIDE, TALL);
        assert!(prompt.handle_key(key(KeyCode::Char('s'))).is_none());
        assert!(prompt.confirmation.as_ref().unwrap().complete);
        assert!(prompt.confirmation_phrase().is_none());
        let answer = prompt.allow_answer(PermissionLifetime::Conversation);
        render(&mut prompt, WIDE, TALL);
        assert_eq!(
            prompt.handle_key(key(KeyCode::Enter)).unwrap().answer,
            answer
        );
    }

    #[test]
    fn absent_trusted_observation_is_not_presented_as_a_match() {
        let mut prompt = suggested_prompt();
        prompt.requests.front_mut().unwrap().request.resources[0]
            .attributes
            .remove(COMMAND_OBSERVATION_ATTRIBUTE);
        inspect(&mut prompt);
        assert_eq!(
            prompt.inspector.as_ref().unwrap().current_match,
            CurrentMatch::Unavailable
        );
        assert!(render(&mut prompt, WIDE, TALL).contains(MATCH_UNAVAILABLE));
        prompt.handle_key(key(KeyCode::Char('2')));
        assert_eq!(
            prompt.inspector.as_ref().unwrap().draft.slots[0].domain,
            ArgumentDomain::Exact {
                value: AGENT_VALUE.into()
            }
        );
        render(&mut prompt, WIDE, TALL);
        assert!(prompt.handle_key(key(KeyCode::Char('p'))).is_none());
        assert!(prompt.inspector.is_some());
        assert!(prompt.scopes[0].pattern.is_none());
    }

    #[test_case(false; "observation_no_longer_available")]
    #[test_case(true; "scope_no_longer_offered")]
    fn use_scope_rechecks_current_request_instead_of_cached_admission(remove_option: bool) {
        let mut prompt = suggested_prompt();
        inspect(&mut prompt);
        assert!(prompt.inspector.as_ref().unwrap().usable());
        let request = &mut prompt.requests.front_mut().unwrap().request;
        if remove_option {
            request.options.retain(|option| option.id != OPTION_ID);
        } else {
            request.resources[0]
                .attributes
                .remove(COMMAND_OBSERVATION_ATTRIBUTE);
        }
        assert!(prompt.handle_key(key(KeyCode::Char('p'))).is_none());
        assert!(!prompt.inspector.as_ref().unwrap().usable());
        assert!(prompt.scopes[0].pattern.is_none());
    }

    #[test_case(false; "joint_values")]
    #[test_case(true; "independent_values")]
    fn exact_mode_starts_with_the_backend_matched_value_not_lexical_first(independent: bool) {
        let mut definition = definition();
        if independent {
            definition.combinations = SlotCombinations::Independent;
        }
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(offered_request("current", definition), None);
        inspect(&mut prompt);
        prompt.handle_key(key(KeyCode::Char('2')));
        let inspector = prompt.inspector.as_ref().unwrap();
        assert_eq!(
            inspector.draft.slots[0].domain,
            ArgumentDomain::Exact {
                value: UI_VALUE.into()
            }
        );
        assert_eq!(inspector.current_match, CurrentMatch::Matched);
        let chosen = take_scope(&mut prompt);
        assert_eq!(
            chosen.slots[0].domain,
            ArgumentDomain::Exact {
                value: UI_VALUE.into()
            }
        );
    }

    #[test_case('3'; "glob")]
    #[test_case('4'; "regex")]
    fn expression_narrowing_preserves_only_matching_correlated_tuples(mode: char) {
        let original = joint_definition();
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(offered_request("narrow", original.clone()), None);
        inspect(&mut prompt);
        prompt.handle_key(key(KeyCode::Char(mode)));
        edit(&mut prompt, 'e', UI_VALUE);
        let inspector = prompt.inspector.as_ref().unwrap();
        assert!(inspector.error.is_none(), "{:?}", inspector.error);
        assert_eq!(inspector.current_match, CurrentMatch::Matched);
        assert_eq!(*inspector.proposal, original);
        let narrowed = take_scope(&mut prompt);
        assert_eq!(
            narrowed.combinations,
            SlotCombinations::ObservedTuples {
                tuples: BTreeSet::from([BTreeMap::from([
                    (SLOT, UI_VALUE.into()),
                    (SECOND_SLOT, "x86".into())
                ])])
            }
        );
    }

    #[test_case('3'; "glob")]
    #[test_case('4'; "regex")]
    fn narrowing_to_no_observed_tuples_disables_use_scope(mode: char) {
        let mut prompt = suggested_prompt();
        inspect(&mut prompt);
        prompt.handle_key(key(KeyCode::Char(mode)));
        edit(&mut prompt, 'e', UNOBSERVED_VALUE);
        let inspector = prompt.inspector.as_ref().unwrap();
        assert!(
            matches!(inspector.definition(&prompt.field).combinations, SlotCombinations::ObservedTuples { tuples } if tuples.is_empty())
        );
        assert!(inspector.error.is_some());
        assert!(prompt.handle_key(key(KeyCode::Char('p'))).is_none());
        assert!(prompt.inspector.is_some());
        assert_eq!(
            prompt.handle_key(key(KeyCode::Char('y'))).unwrap().answer,
            PermissionAnswer::AllowOnce
        );
    }

    #[test_case('3'; "glob")]
    #[test_case('4'; "regex")]
    fn expression_narrowing_treats_escaped_metacharacters_and_separators_as_argument_data(
        mode: char,
    ) {
        let mut definition = definition();
        let values = BTreeSet::from([ESCAPED_VALUE.into(), OTHER_PATH_VALUE.into()]);
        definition.slots[0].domain = ArgumentDomain::ObservedSet {
            values: values.clone(),
        };
        definition.combinations = SlotCombinations::ObservedTuples {
            tuples: values
                .into_iter()
                .map(|value| BTreeMap::from([(SLOT, value)]))
                .collect(),
        };
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(offered_request("escaped", definition.clone()), None);
        inspect(&mut prompt);
        prompt.handle_key(key(KeyCode::Char(mode)));
        edit(&mut prompt, 'e', ESCAPED_EXPRESSION);
        assert_eq!(
            prompt.inspector.as_ref().unwrap().current_match,
            CurrentMatch::Matched
        );
        let narrowed = take_scope(&mut prompt);
        assert_eq!(narrowed.argv, definition.argv);
        assert_eq!(
            narrowed.combinations,
            SlotCombinations::ObservedTuples {
                tuples: BTreeSet::from([BTreeMap::from([(SLOT, ESCAPED_VALUE.into())])])
            }
        );
    }

    #[test]
    fn a_known_mismatch_cannot_be_selected_or_approved_as_a_remembered_scope() {
        let mut prompt = suggested_prompt();
        inspect(&mut prompt);
        prompt.handle_key(key(KeyCode::Char('2')));
        edit(&mut prompt, 'e', AGENT_VALUE);
        let inspector = prompt.inspector.as_ref().unwrap();
        assert!(inspector.error.is_none());
        assert_eq!(inspector.current_match, CurrentMatch::KnownMismatch);
        assert!(render(&mut prompt, WIDE, TALL).contains(MATCH_MISMATCH));
        assert!(
            !prompt
                .row_hits
                .iter()
                .any(|hit| hit.target == PromptTarget::Hint(key(KeyCode::Char('p'))))
        );
        assert!(prompt.handle_key(key(KeyCode::Char('p'))).is_none());
        assert!(prompt.inspector.is_some());
        edit(&mut prompt, 'e', UI_VALUE);
        take_scope(&mut prompt);
        let stored = &mut prompt.scopes[0].pattern.as_mut().unwrap().definition;
        stored.slots[0].domain = ArgumentDomain::Exact {
            value: AGENT_VALUE.into(),
        };
        stored.combinations = SlotCombinations::ObservedTuples {
            tuples: BTreeSet::from([BTreeMap::from([(SLOT, AGENT_VALUE.into())])]),
        };
        render(&mut prompt, WIDE, TALL);
        assert!(prompt.handle_key(key(KeyCode::Char('s'))).is_none());
        assert!(!prompt.confirmation.as_ref().unwrap().complete);
        render(&mut prompt, WIDE, TALL);
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
    }

    #[test]
    fn input_json_cannot_create_a_pattern_proposal() {
        let mut prompt = suggested_prompt();
        let request = &mut prompt.requests.front_mut().unwrap().request;
        request
            .options
            .retain(|option| offered_pattern(option).is_none());
        request.input = json!({"pattern": definition()});
        prompt.open_scope_editor();
        prompt.open_inspector();
        assert!(prompt.inspector.is_none());
    }

    #[test_case(false; "refresh")]
    #[test_case(true; "queue_advance")]
    fn new_request_discards_the_draft_and_stale_click(advance: bool) {
        let mut prompt = suggested_prompt();
        inspect(&mut prompt);
        edit(&mut prompt, 'n', "not persisted");
        let area = prompt
            .row_hits
            .iter()
            .find(|hit| hit.target == PromptTarget::Hint(key(KeyCode::Char('p'))))
            .unwrap()
            .area;
        let mouse = |kind| MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        prompt.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left)));
        if advance {
            prompt.enqueue(offered_request("next", definition()), None);
            prompt.resolve("pattern");
        } else {
            prompt.update(offered_request("pattern", definition()));
        }
        assert!(prompt.inspector.is_none());
        assert!(prompt.scopes[0].pattern.is_none());
        assert!(prompt.handle_key(key(KeyCode::Char('s'))).is_none());
        render(&mut prompt, WIDE, TALL);
        assert!(!matches!(
            prompt.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left))),
            PromptMouse::Decided(_)
        ));
    }

    #[test]
    fn closing_an_inspector_does_not_select_its_edited_scope() {
        let mut prompt = suggested_prompt();
        let before = prompt.row_grants(prompt.current().unwrap());
        inspect(&mut prompt);
        edit(&mut prompt, 'N', "Discard me");
        prompt.handle_key(key(KeyCode::Char('5')));
        prompt.handle_key(key(KeyCode::Esc));
        assert!(prompt.inspector.is_none());
        assert!(prompt.panel == Panel::Scopes);
        assert_eq!(prompt.row_grants(prompt.current().unwrap()), before);
    }

    #[test_case(40, 10; "narrow_short")]
    #[test_case(80, 28; "normal")]
    #[test_case(140, 28; "wide")]
    fn tab_reveals_inspector_controls_and_mouse_changes_the_same_mode(width: u16, height: u16) {
        let mut prompt = suggested_prompt();
        inspect(&mut prompt);
        render(&mut prompt, width, height);
        for target in [
            InspectorControl::Name,
            InspectorControl::Slot,
            InspectorControl::SlotName,
            InspectorControl::Mode,
        ] {
            prompt.handle_key(key(KeyCode::Tab));
            render(&mut prompt, width, height);
            assert_eq!(prompt.focus, Some(PromptTarget::Inspector(target.clone())));
            assert!(
                prompt
                    .row_hits
                    .iter()
                    .any(|hit| hit.target == PromptTarget::Inspector(target.clone()))
            );
        }
        click(&mut prompt, PromptTarget::Inspector(InspectorControl::Mode));
        assert!(matches!(
            prompt.inspector.as_ref().unwrap().draft.slots[0].domain,
            ArgumentDomain::Exact { .. }
        ));
        render(&mut prompt, width, height);
        prompt.handle_key(key(KeyCode::Left));
        assert!(matches!(
            prompt.inspector.as_ref().unwrap().draft.slots[0].domain,
            ArgumentDomain::ObservedSet { .. }
        ));
        let mut event = key(KeyCode::Char('p'));
        event.kind = KeyEventKind::Repeat;
        assert!(prompt.handle_key(event).is_none());
        assert!(prompt.inspector.is_some());
    }

    #[test_case(40, 10; "narrow_short")]
    #[test_case(80, 28; "normal")]
    #[test_case(140, 28; "wide")]
    fn edits_cancel_and_details_preserve_keyboard_mouse_parity(width: u16, height: u16) {
        let mut answers = Vec::new();
        for mouse in [false, true] {
            let mut prompt = suggested_prompt();
            inspect(&mut prompt);
            render(&mut prompt, width, height);
            let action = |prompt: &mut PermissionPrompt, code| {
                render(prompt, width, height);
                if mouse {
                    click(prompt, PromptTarget::Hint(key(code)));
                } else {
                    assert!(prompt.handle_key(key(code)).is_none());
                }
            };
            if mouse {
                prompt.handle_key(key(KeyCode::Tab));
                render(&mut prompt, width, height);
                click(&mut prompt, PromptTarget::Inspector(InspectorControl::Name));
            } else {
                prompt.handle_key(key(KeyCode::Char('N')));
            }
            prompt.field.clear();
            assert!(prompt.handle_paste(EDITED_NAME));
            let mut repeat = key(KeyCode::Enter);
            repeat.kind = KeyEventKind::Repeat;
            assert!(prompt.handle_key(repeat).is_none());
            assert!(prompt.inspector.as_ref().unwrap().is_editing());
            repeat.kind = KeyEventKind::Release;
            prompt.handle_key(repeat);
            action(&mut prompt, KeyCode::Enter);
            assert_eq!(prompt.inspector.as_ref().unwrap().draft.name, EDITED_NAME);

            prompt.handle_key(key(KeyCode::Char('N')));
            prompt.field.clear();
            assert!(prompt.handle_paste(DISCARDED_NAME));
            action(&mut prompt, KeyCode::Esc);
            assert_eq!(prompt.inspector.as_ref().unwrap().draft.name, EDITED_NAME);
            assert!(!prompt.inspector.as_ref().unwrap().is_editing());

            action(&mut prompt, KeyCode::F(2));
            assert!(prompt.panel == Panel::Details);
            assert!(!prompt.handle_paste(DISCARDED_NAME));
            action(&mut prompt, KeyCode::Esc);
            assert!(prompt.panel != Panel::Details);
            assert_eq!(prompt.inspector.as_ref().unwrap().draft.name, EDITED_NAME);
            assert!(prompt.scopes[0].pattern.is_none());
            action(&mut prompt, KeyCode::Char('p'));
            assert!(prompt.inspector.is_none());
            assert!(prompt.confirmation.is_none());
            answers.push(prompt.allow_answer(PermissionLifetime::Conversation));
        }
        assert_eq!(answers[0], answers[1]);
    }

    #[test]
    fn edited_patterns_retain_offered_lifetimes_and_required_phrases() {
        let mut prompt = suggested_prompt();
        let source = prompt
            .requests
            .front_mut()
            .unwrap()
            .request
            .options
            .iter_mut()
            .find(|option| option.id == OPTION_ID)
            .unwrap();
        source.allowed_lifetimes = vec![PermissionLifetime::Conversation];
        source.confirmation = Some(TEMPLATE_PHRASE.into());
        inspect(&mut prompt);
        take_scope(&mut prompt);
        assert!(!prompt.grants_lifetime(&PermissionLifetime::Project));
        render(&mut prompt, WIDE, TALL);
        prompt.handle_key(key(KeyCode::Char('s')));
        assert_eq!(prompt.confirmation_phrase(), Some(TEMPLATE_PHRASE));
        let frozen = prompt.confirmation.as_ref().unwrap().answer.clone();
        prompt.scopes[0].pattern.as_mut().unwrap().definition.name = "later name".into();
        render(&mut prompt, WIDE, TALL);
        prompt.handle_paste(TEMPLATE_PHRASE);
        assert_eq!(
            prompt.handle_key(key(KeyCode::Enter)).unwrap().answer,
            frozen
        );
    }

    #[test_case(EVIDENCE; "native_requests")]
    #[test_case(IMPORTED_EVIDENCE; "imported_history")]
    fn inspector_preserves_complete_evidence_without_sentence_truncation(evidence: &str) {
        let mut request = offered_request("pattern", definition());
        request
            .options
            .iter_mut()
            .find(|option| option.id == OPTION_ID)
            .unwrap()
            .description = evidence.into();
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(request, None);
        inspect(&mut prompt);
        prompt.activate_inspector(InspectorControl::Observations);
        let panel = prompt.inspector_panel().unwrap();
        let displayed = panel
            .details(&theme::current())
            .iter()
            .map(|line| line.to_string())
            .find(|line| line.starts_with(EVIDENCE_LABEL))
            .unwrap();
        assert_eq!(displayed, format!("{EVIDENCE_LABEL}{evidence}"));
        assert!(!displayed.contains(OPTION_ID));
        assert!(!displayed.contains(&definition().fingerprint().unwrap()));
    }

    #[test]
    fn display_names_and_option_guards_are_human_without_raw_context_ids() {
        let mut prompt = suggested_prompt();
        inspect(&mut prompt);
        let body = prompt
            .inspector_panel()
            .unwrap()
            .details(&theme::current())
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("Option-looking values: rejected (fixed guard)."));
        for hidden in [
            OPTION_ID,
            "opaque-fixture-binding",
            "fixture-analysis",
            "SlotId",
            "fixture-shell",
        ] {
            assert!(!body.contains(hidden));
        }
    }
}
