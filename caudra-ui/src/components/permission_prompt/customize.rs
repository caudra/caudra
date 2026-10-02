use caudra_agent::permissions::{
    COMMAND_EXACT_PREFIX, COMMAND_TEMPLATE_PREFIX, ComposedRow, PermissionAnswer,
    PermissionLifetime, PermissionRequest, PermissionRowGrant, PermissionRuleOption,
    StructuredPermissionEffect, grade_command_pattern,
};

use super::choices::grant_label;
use super::decision::{default_grant, grant_option, row_ladder, step};
use super::details::review_text;
use super::scope::option_summary;
use super::{Panel, PermissionDecision, PermissionPrompt, PromptState, front_request};

const DENY_EXACT: &str = "deny_exact";
const EXACT_COMMANDS: &str = "allow_exact_commands";
const EXACT_CALL: &str = "allow_exact";
pub(super) const OWN_PATTERN_ITEM: &str = "your own pattern…";
const EXACT_SCRIPT: &str = "this exact script";
const SUGGESTED: &str = "suggested";
const BROAD: &str = "⚠ broad";
const INCOMPLETE: &str = "can't show everything";

/// Whether Customize allows or refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Effect {
    Allow,
    Deny,
}

/// The field `←` `→` change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Field {
    Effect,
    Remember,
    Scope,
}

pub(super) const FIELDS: [Field; 3] = [Field::Effect, Field::Remember, Field::Scope];

/// One scope Customize lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ScopeItem {
    /// A rung of the command's own ladder, or a pattern written or edited
    /// for it.
    Row(PermissionRowGrant),
    /// Writing a pattern of one's own for the command.
    OwnPattern,
    /// An option that answers for the whole request.
    Whole(String),
}

/// Every answer the main view leaves out: the effect, how long, and the
/// whole list of scopes.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Customize {
    pub effect: Effect,
    pub lifetime: PermissionLifetime,
    pub chosen: ScopeItem,
    pub highlight: usize,
    pub field: Field,
    /// The command whose own ladder is listed, or `None` when only scopes
    /// for the whole request are.
    pub row: Option<usize>,
    /// Where Esc returns to.
    pub back: Panel,
    /// Whether the rule the chosen scope would store is shown in full.
    pub advanced: bool,
}

fn allows(option: &PermissionRuleOption) -> bool {
    option.rule.effect == StructuredPermissionEffect::Allow
}

fn durable(option: &PermissionRuleOption) -> bool {
    option
        .allowed_lifetimes
        .iter()
        .any(|lifetime| *lifetime != PermissionLifetime::Once)
}

fn whole(option: &PermissionRuleOption) -> bool {
    option
        .group
        .as_ref()
        .is_none_or(|group| group.resource.is_none())
}

fn option<'a>(request: &'a PermissionRequest, id: &str) -> Option<&'a PermissionRuleOption> {
    request.options.iter().find(|option| option.id == id)
}

/// The scopes Customize lists for `customize`, narrowest first: the
/// command's ladder and a pattern of one's own, then the broad grants; or the
/// scopes that answer for the whole request.
fn scope_items(request: &PermissionRequest, customize: &Customize, shell: bool) -> Vec<ScopeItem> {
    if customize.effect == Effect::Deny {
        return option(request, DENY_EXACT)
            .map(|option| ScopeItem::Whole(option.id.clone()))
            .into_iter()
            .collect();
    }
    let broad = request
        .options
        .iter()
        .filter(|option| allows(option) && durable(option) && whole(option))
        .filter(|option| option.confirmation.is_some());
    let Some(row) = customize.row else {
        if !shell {
            return request
                .options
                .iter()
                .filter(|option| allows(option) && durable(option) && whole(option))
                .map(|option| ScopeItem::Whole(option.id.clone()))
                .collect();
        }
        let exact = option(request, EXACT_COMMANDS)
            .or_else(|| option(request, EXACT_CALL))
            .filter(|option| durable(option));
        return exact
            .into_iter()
            .chain(broad)
            .map(|option| ScopeItem::Whole(option.id.clone()))
            .collect();
    };
    let current = match &customize.chosen {
        ScopeItem::Row(grant) => Some(grant),
        _ => None,
    };
    let mut items: Vec<ScopeItem> = row_ladder(request, row)
        .map(|option| {
            ScopeItem::Row(match current {
                Some(grant @ PermissionRowGrant::Pattern { option_id, .. })
                    if *option_id == option.id =>
                {
                    grant.clone()
                }
                _ => PermissionRowGrant::Offered(option.id.clone()),
            })
        })
        .collect();
    if let Some(grant @ PermissionRowGrant::Written(_)) = current {
        items.push(ScopeItem::Row(grant.clone()));
    }
    items.push(ScopeItem::OwnPattern);
    items.extend(broad.map(|option| ScopeItem::Whole(option.id.clone())));
    items
}

/// Where how long falls back to when the scope no longer allows it: this
/// conversation when it can, else once.
fn fallback_lifetime(lifetimes: &[PermissionLifetime]) -> PermissionLifetime {
    if lifetimes.contains(&PermissionLifetime::Conversation) {
        PermissionLifetime::Conversation
    } else {
        PermissionLifetime::Once
    }
}

/// The option a listed scope stands on.
fn item_option<'a>(
    request: &'a PermissionRequest,
    row: Option<usize>,
    item: &ScopeItem,
) -> Option<&'a PermissionRuleOption> {
    match item {
        ScopeItem::Row(grant) => grant_option(request, row?, grant),
        ScopeItem::OwnPattern => option(request, &format!("{COMMAND_EXACT_PREFIX}{}", row?)),
        ScopeItem::Whole(id) => option(request, id),
    }
}

/// Whether a scope can be chosen: every part of what it allows can be shown.
fn selectable(request: &PermissionRequest, row: Option<usize>, item: &ScopeItem) -> bool {
    match item {
        ScopeItem::OwnPattern => true,
        ScopeItem::Row(PermissionRowGrant::Written(_) | PermissionRowGrant::Pattern { .. }) => {
            item_option(request, row, item).is_some()
        }
        _ => item_option(request, row, item)
            .is_some_and(|option| option_summary(request, option).complete),
    }
}

impl Customize {
    /// The lifetimes the chosen effect and scope allow, `Once` first.
    fn lifetimes(&self, request: &PermissionRequest, project: bool) -> Vec<PermissionLifetime> {
        let durable = match self.effect {
            Effect::Deny => option(request, DENY_EXACT)
                .map(|option| option.allowed_lifetimes.clone())
                .unwrap_or_default(),
            Effect::Allow => item_option(request, self.row, &self.chosen)
                .map(|option| option.allowed_lifetimes.clone())
                .unwrap_or_default(),
        };
        let mut lifetimes = vec![PermissionLifetime::Once];
        lifetimes.extend(
            [
                PermissionLifetime::Conversation,
                PermissionLifetime::Project,
                PermissionLifetime::Global,
            ]
            .into_iter()
            .filter(|lifetime| {
                durable.contains(lifetime) && (project || *lifetime != PermissionLifetime::Project)
            }),
        );
        lifetimes
    }

    /// Customize fitted to the current request: a scope it no longer offers
    /// falls back to the command's default, and a lifetime the scope no
    /// longer allows shortens.
    pub(super) fn clamped(mut self, prompt: &PermissionPrompt) -> Self {
        let Some(request) = prompt.current() else {
            return self;
        };
        if self.row.is_some_and(|row| !prompt.is_new_row(row)) {
            self.row = None;
        }
        let items = scope_items(request, &self, prompt.shell());
        if !items.contains(&self.chosen)
            || !selectable(request, self.row, &self.chosen)
            || self.chosen == ScopeItem::OwnPattern
        {
            self.chosen = self
                .row
                .and_then(|row| default_grant(request, row))
                .map(ScopeItem::Row)
                .filter(|item| items.contains(item))
                .or_else(|| {
                    items
                        .iter()
                        .find(|item| {
                            **item != ScopeItem::OwnPattern && selectable(request, self.row, item)
                        })
                        .cloned()
                })
                .unwrap_or(ScopeItem::OwnPattern);
        }
        let lifetimes = self.lifetimes(request, prompt.project_available());
        if !lifetimes.contains(&self.lifetime) {
            self.lifetime = fallback_lifetime(&lifetimes);
        }
        self.highlight = self.highlight.min(items.len().saturating_sub(1));
        self
    }

    /// What applying Customize answers, or `None` while it names nothing
    /// that can be stored.
    pub(super) fn answer(&self, prompt: &PermissionPrompt) -> Option<PermissionAnswer> {
        let request = prompt.current()?;
        let lifetime = self.lifetime.clone();
        match (self.effect, lifetime) {
            (Effect::Deny, PermissionLifetime::Once) => Some(PermissionAnswer::Deny),
            (Effect::Deny, PermissionLifetime::Project) => Some(PermissionAnswer::DenyAlwaysLocal),
            (Effect::Deny, PermissionLifetime::Global) => Some(PermissionAnswer::DenyAlwaysGlobal),
            (Effect::Deny, PermissionLifetime::Conversation) => None,
            (Effect::Allow, PermissionLifetime::Once) => Some(PermissionAnswer::AllowOnce),
            (Effect::Allow, lifetime) => match &self.chosen {
                ScopeItem::Row(grant) => {
                    let mut rows = vec![None; request.resources.len()];
                    *rows.get_mut(self.row?)? = Some(ComposedRow {
                        grant: grant.clone(),
                        lifetime,
                    });
                    Some(PermissionAnswer::AllowComposed { rows })
                }
                ScopeItem::Whole(id) => Some(PermissionAnswer::AllowOption {
                    option_id: id.clone(),
                    lifetime,
                }),
                ScopeItem::OwnPattern => None,
            },
        }
    }
}

/// How a listed scope reads, its badges, and whether it can be chosen.
pub(super) fn scope_item_label(
    request: &PermissionRequest,
    customize: &Customize,
    item: &ScopeItem,
    script: bool,
) -> (String, Vec<String>, bool) {
    let label = match item {
        ScopeItem::OwnPattern => return (OWN_PATTERN_ITEM.into(), Vec::new(), true),
        ScopeItem::Row(grant) => customize
            .row
            .map(|row| grant_label(request, row, grant))
            .unwrap_or_default(),
        ScopeItem::Whole(id) if id == DENY_EXACT && script => EXACT_SCRIPT.into(),
        ScopeItem::Whole(id) => option(request, id)
            .map(|option| review_text(&option.label))
            .unwrap_or_default(),
    };
    let mut badges = Vec::new();
    if let Some(option) = item_option(request, customize.row, item) {
        if option.id.starts_with(COMMAND_TEMPLATE_PREFIX) {
            badges.push(SUGGESTED.into());
        }
        if let Some(seen) = option.seen {
            badges.push(format!("seen {seen}×"));
        }
        if option.confirmation.is_some() {
            badges.push(BROAD.into());
        }
    }
    let selectable = selectable(request, customize.row, item);
    if !selectable {
        badges.push(INCOMPLETE.into());
    }
    (label, badges, selectable)
}

impl PermissionPrompt {
    pub(super) fn customize_items(&self) -> Vec<ScopeItem> {
        match (self.current(), &self.customize) {
            (Some(request), Some(customize)) => scope_items(request, customize, self.shell()),
            _ => Vec::new(),
        }
    }

    pub(super) fn customize_lifetimes(&self) -> Vec<PermissionLifetime> {
        match (self.current(), &self.customize) {
            (Some(request), Some(customize)) => {
                customize.lifetimes(request, self.project_available())
            }
            _ => Vec::new(),
        }
    }

    /// Opens Customize in place of the choices: for the one command's own
    /// ladder, or, from Review or on a line no row can remember, for the
    /// whole script.
    pub(super) fn open_customize(&mut self, from_review: bool) {
        let listed = self.listed_rows();
        let row = match listed.as_slice() {
            [row] if !from_review && self.per_row() && self.is_new_row(*row) => Some(*row),
            _ => None,
        };
        let chosen = match row {
            Some(row) => self.row_grant(row).map(ScopeItem::Row),
            None => self
                .chosen_authority()
                .map(|option| ScopeItem::Whole(option.id.clone())),
        };
        let customize = Customize {
            effect: Effect::Allow,
            lifetime: PermissionLifetime::Conversation,
            chosen: chosen.unwrap_or(ScopeItem::OwnPattern),
            highlight: 0,
            field: Field::Scope,
            row,
            back: if from_review {
                Panel::StepThrough
            } else {
                Panel::Main
            },
            advanced: false,
        }
        .clamped(self);
        self.customize = Some(customize);
        self.panel = Panel::Customize;
        self.highlight_chosen_scope();
        self.scroll.reset();
        self.invalidate_controls();
    }

    pub(super) fn highlight_chosen_scope(&mut self) {
        let items = self.customize_items();
        if let Some(customize) = self.customize.as_mut()
            && let Some(index) = items.iter().position(|item| *item == customize.chosen)
        {
            customize.highlight = index;
        }
        self.reveal = true;
    }

    /// Leaves Customize for wherever it was opened, answering nothing.
    pub(super) fn close_customize(&mut self) {
        let back = self
            .customize
            .take()
            .map_or(Panel::Main, |customize| customize.back);
        self.panel = if back == Panel::StepThrough && self.step.is_some() {
            Panel::StepThrough
        } else {
            Panel::Main
        };
        self.state = PromptState::Normal;
        self.inspector = None;
        self.field.clear();
        self.scroll.reset();
        self.invalidate_controls();
    }

    /// Moves the scope highlight to the next scope that can be chosen, and
    /// chooses it unless it is the pattern of one's own.
    pub(super) fn move_scope(&mut self, forward: bool) {
        let Some(request) = self.current() else {
            return;
        };
        let items = self.customize_items();
        let Some(customize) = self.customize.as_ref() else {
            return;
        };
        let mut index = customize.highlight;
        let next = loop {
            index = match (forward, index) {
                (true, index) if index + 1 < items.len() => index + 1,
                (false, index) if index > 0 => index - 1,
                _ => return,
            };
            if selectable(request, customize.row, &items[index]) {
                break index;
            }
        };
        self.highlight_scope(next);
    }

    /// Puts the highlight on scope `index`, choosing it when it is one that
    /// can be chosen as it stands.
    pub(super) fn highlight_scope(&mut self, index: usize) {
        let Some(request) = front_request(&self.requests) else {
            return;
        };
        let items = self.customize_items();
        let Some(item) = items.get(index).cloned() else {
            return;
        };
        let project = self.project_available();
        let Some(customize) = self.customize.as_mut() else {
            return;
        };
        if !selectable(request, customize.row, &item) {
            return;
        }
        customize.highlight = index;
        customize.field = Field::Scope;
        if item != ScopeItem::OwnPattern {
            customize.chosen = item;
            let lifetimes = customize.lifetimes(request, project);
            if !lifetimes.contains(&customize.lifetime) {
                customize.lifetime = fallback_lifetime(&lifetimes);
            }
        }
        self.reveal = true;
        self.invalidate_controls();
    }

    pub(super) fn set_effect(&mut self, effect: Effect) {
        let Some(customize) = self.customize.as_mut() else {
            return;
        };
        if customize.effect == effect {
            return;
        }
        customize.effect = effect;
        customize.chosen = ScopeItem::OwnPattern;
        customize.highlight = 0;
        customize.advanced = false;
        if let Some(customize) = self.customize.take() {
            self.customize = Some(customize.clamped(self));
        }
        self.highlight_chosen_scope();
        self.invalidate_controls();
    }

    pub(super) fn set_lifetime(&mut self, lifetime: PermissionLifetime) {
        if !self.customize_lifetimes().contains(&lifetime) {
            return;
        }
        if let Some(customize) = self.customize.as_mut() {
            customize.lifetime = lifetime;
        }
        self.invalidate_controls();
    }

    /// `←` `→` on the focused field: the effect, or how long.
    pub(super) fn step_customize(&mut self, forward: bool) {
        let Some(customize) = self.customize.as_ref() else {
            return;
        };
        match customize.field {
            Field::Effect => self.set_effect(match customize.effect {
                Effect::Allow => Effect::Deny,
                Effect::Deny => Effect::Allow,
            }),
            Field::Remember | Field::Scope => {
                let lifetimes = self.customize_lifetimes();
                if let Some(next) = step(&lifetimes, &customize.lifetime, forward) {
                    self.set_lifetime(next);
                }
            }
        }
    }

    pub(super) fn cycle_customize_field(&mut self, backwards: bool) {
        if let Some(customize) = self.customize.as_mut() {
            let index = FIELDS
                .iter()
                .position(|field| *field == customize.field)
                .unwrap_or_default();
            let next = if backwards {
                (index + FIELDS.len() - 1) % FIELDS.len()
            } else {
                (index + 1) % FIELDS.len()
            };
            customize.field = FIELDS[next];
        }
        self.invalidate_controls();
    }

    /// Enter in Customize: write a pattern of one's own, or apply.
    pub(super) fn apply_customize(&mut self) -> Option<PermissionDecision> {
        let customize = self.customize.as_ref()?;
        if self.customize_items().get(customize.highlight) == Some(&ScopeItem::OwnPattern)
            && customize.effect == Effect::Allow
        {
            let seed = match &customize.chosen {
                ScopeItem::Row(PermissionRowGrant::Written(pattern)) => pattern.clone(),
                _ => String::new(),
            };
            self.state = PromptState::PatternEditing;
            self.field.set_text(&seed);
            self.invalidate_controls();
            return None;
        }
        if self.awaiting_review {
            return None;
        }
        let answer = customize.answer(self)?;
        self.decide_or_confirm(answer, None)
    }

    /// Accepts a pattern written in Customize when it matches the command.
    pub(super) fn commit_customize_pattern(&mut self) -> bool {
        let pattern = self.field.text().trim().to_owned();
        let Some(request) = self.current() else {
            return false;
        };
        let Some(row) = self.customize.as_ref().and_then(|customize| customize.row) else {
            return false;
        };
        let Some(resource) = request.resources.get(row) else {
            return false;
        };
        if grade_command_pattern(&pattern, &resource.value).is_err() {
            return false;
        }
        if let Some(customize) = self.customize.as_mut() {
            customize.chosen = ScopeItem::Row(PermissionRowGrant::Written(pattern));
        }
        self.state = PromptState::Normal;
        self.field.clear();
        self.highlight_chosen_scope();
        self.invalidate_controls();
        true
    }
}
