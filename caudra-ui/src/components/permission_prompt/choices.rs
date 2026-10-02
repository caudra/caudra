use caudra_agent::permissions::review::command_template_phrase;
use caudra_agent::permissions::{
    PermissionAnswer, PermissionLifetime, PermissionRequest, PermissionResourceAccess,
    PermissionResourceKind, PermissionRowGrant,
};

use super::decision::grant_option;
use super::details::review_text;
use super::{PermissionDecision, PermissionPrompt, PromptState};

const YES: &str = "Yes";
const YES_ONCE: &str = "Yes, run it once";
const YES_ALL_ONCE: &str = "Yes, run them once";
pub(super) const NO: &str = "No, and tell the agent what to do instead";
pub(super) const ONCE_ONLY: &str = "this time only";

/// One numbered answer of the main view. The letters keep their meaning
/// whatever number a choice ends up with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Choice {
    Once,
    Conversation,
    Project,
    Deny,
}

impl Choice {
    pub(super) fn from_letter(letter: char) -> Option<Self> {
        match letter {
            'y' => Some(Self::Once),
            's' => Some(Self::Conversation),
            'a' => Some(Self::Project),
            'n' => Some(Self::Deny),
            _ => None,
        }
    }

    pub(super) fn lifetime(self) -> Option<PermissionLifetime> {
        match self {
            Self::Conversation => Some(PermissionLifetime::Conversation),
            Self::Project => Some(PermissionLifetime::Project),
            Self::Once | Self::Deny => None,
        }
    }
}

/// How a row's grant reads in a sentence.
pub(super) fn grant_label(
    request: &PermissionRequest,
    row: usize,
    grant: &PermissionRowGrant,
) -> String {
    review_text(&match grant {
        PermissionRowGrant::Offered(_) => grant_option(request, row, grant)
            .map(|option| option.label.clone())
            .unwrap_or_default(),
        PermissionRowGrant::Pattern { definition, .. } => command_template_phrase(definition),
        PermissionRowGrant::Written(pattern) => pattern.clone(),
    })
}

/// How a file grant says what it lets the agent do, so the sentence reads
/// `allow reading ‹this file›`.
fn access_verb(request: &PermissionRequest) -> Option<&'static str> {
    let resource = request.resources.first()?;
    if !matches!(
        resource.kind,
        PermissionResourceKind::File | PermissionResourceKind::Directory
    ) {
        return None;
    }
    Some(match resource.access.as_ref()? {
        PermissionResourceAccess::Write => "editing",
        PermissionResourceAccess::List => "listing",
        PermissionResourceAccess::Search => "searching",
        _ => "reading",
    })
}

impl PermissionPrompt {
    /// The choices on offer, in order. Remembering is offered only for a
    /// lifetime every remembered rung allows.
    pub(super) fn choices(&self) -> Vec<Choice> {
        let mut choices = vec![Choice::Once];
        choices.extend(
            [Choice::Conversation, Choice::Project]
                .into_iter()
                .filter(|choice| {
                    choice
                        .lifetime()
                        .is_some_and(|lifetime| self.remembers(&lifetime))
                }),
        );
        choices.push(Choice::Deny);
        choices
    }

    pub(super) fn choice_answer(&self, choice: Choice) -> Option<PermissionAnswer> {
        match choice {
            Choice::Once => Some(PermissionAnswer::AllowOnce),
            Choice::Deny => Some(PermissionAnswer::Deny),
            remembered => self.allow_answer(remembered.lifetime()?),
        }
    }

    /// The sentence a choice reads as.
    pub(super) fn choice_sentence(&self, choice: Choice) -> String {
        let remembering = self.choices().len() > 2;
        match choice {
            Choice::Once if remembering => YES.into(),
            Choice::Once if self.batch() => YES_ALL_ONCE.into(),
            Choice::Once => YES_ONCE.into(),
            Choice::Conversation => {
                format!(
                    "Yes, and allow {} for this conversation",
                    self.scope_phrase()
                )
            }
            Choice::Project => {
                format!(
                    "Yes, and always allow {} in this project",
                    self.scope_phrase()
                )
            }
            Choice::Deny => NO.into(),
        }
    }

    /// ‹scope› as choices 2 and 3 name it: one rung in marks, with the verb
    /// for a file, or how many commands a batch remembers.
    pub(super) fn scope_phrase(&self) -> String {
        let Some(request) = self.current() else {
            return String::new();
        };
        if self.per_row() {
            let remembered: Vec<String> = self
                .row_grants()
                .iter()
                .enumerate()
                .filter_map(|(row, grant)| Some(grant_label(request, row, grant.as_ref()?)))
                .collect();
            return match remembered.as_slice() {
                [one] => format!("‹{one}›"),
                many => format!("these {} commands", many.len()),
            };
        }
        let label = self
            .chosen_authority()
            .map(|option| review_text(&option.label))
            .unwrap_or_default();
        match access_verb(request) {
            Some(verb) => format!("{verb} ‹{label}›"),
            None => format!("‹{label}›"),
        }
    }

    /// Acts on a choice the same way whether it came from its number, its
    /// letter, Enter, or a click. No asks what to do instead first.
    pub(super) fn choose(&mut self, choice: Choice) -> Option<PermissionDecision> {
        if !self.choices().contains(&choice) {
            return None;
        }
        self.highlight = choice;
        if choice == Choice::Deny {
            self.open_guidance();
            return None;
        }
        if self.awaiting_review {
            return None;
        }
        let answer = self.choice_answer(choice)?;
        self.decide_or_confirm(answer, Some(choice))
    }

    pub(super) fn open_guidance(&mut self) {
        self.state = PromptState::Guidance;
        self.field.clear();
        self.reveal = true;
        self.invalidate_controls();
    }

    /// Sends `answer`, or holds it until the grant it stores is confirmed.
    /// Rows the request would refuse to store are never sent.
    pub(super) fn decide_or_confirm(
        &mut self,
        answer: PermissionAnswer,
        choice: Option<Choice>,
    ) -> Option<PermissionDecision> {
        if let PermissionAnswer::AllowComposed { rows } = &answer
            && self
                .current()
                .is_none_or(|request| request.composed_rules(rows).is_err())
        {
            return None;
        }
        if let Some(pending) = self.pending_for(&answer, choice) {
            self.pending = Some(pending);
            self.field.clear();
            self.input_freshness.barrier();
            self.reveal = true;
            self.invalidate_controls();
            return None;
        }
        self.decision(answer)
    }

    pub(super) fn move_highlight(&mut self, forward: bool) {
        let choices = self.choices();
        let Some(index) = choices.iter().position(|choice| *choice == self.highlight) else {
            self.highlight = choices[0];
            return;
        };
        let next = if forward {
            (index + 1).min(choices.len() - 1)
        } else {
            index.saturating_sub(1)
        };
        self.highlight = choices[next];
        self.reveal = true;
    }
}
