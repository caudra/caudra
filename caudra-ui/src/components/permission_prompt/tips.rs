//! What the prompt's controls mean, for the tooltip of the one under a
//! resting pointer. Every sentence restates the permissions docs or what the
//! screen shows elsewhere, so a reader without a mouse loses nothing.

use caudra_agent::permissions::{
    COMMAND_TEMPLATE_PREFIX, PermissionLifetime, PermissionRuleOption,
};
use ratatui::layout::{Position, Rect};
use unicode_width::UnicodeWidthStr;

use super::choices::Choice;
use super::customize::{Effect, ScopeItem, item_option, scope_item_label};
use super::decision::{covered, grant_option};
use super::details::review_text;
use super::notes::RowStatus;
use super::step_through::{PageItem, REMEMBER_AS_LISTED, REVIEW_CHOICES, ReviewChoice};
use super::view::row_command;
use super::{Panel, PermissionPrompt, PromptState, PromptTarget};
use crate::components::counted;
use crate::components::input::CLICK_OR_PRESS;
use crate::components::permission_scope::view::ScopeView;
use crate::components::tooltip::{self, Tip, TipArea, TipKey};

const ONCE: &str = "Allows this call only. Nothing is remembered.";
const ALL_ONCE: &str = "Runs them once. Every command is on this time only.";
const NOTHING_TO_REMEMBER: &str = "Allows this call once. Nothing here can be remembered.";
const CONVERSATION: &str = "Remembers the scope in ‹ › until this conversation ends. It survives resume and covers subtasks.";
const PROJECT: &str = "Saves the scope in ‹ › for this project folder, in later conversations too.";
const REVOKE: &str = "/permissions lists and revokes it.";
const OWN_RULES: &str = "Each new command becomes its own rule.";
const SCOPE_KEYS_FIRST: &str = "← → change the scope first.";
const DENY: &str = "Denies this call. The field that opens takes guidance for the agent; leave it empty to deny without it.";
const ROW_REMEMBERED: &str = "Needs an answer. A remembering answer stores it as ";
const ROW_ONCE: &str = "Needs an answer. On this time only, it runs without being remembered.";
const ROW_FIXED: &str = "Needs an answer. It can't be remembered on its own.";
const ROW_FOCUSED: &str = "← → change its scope.";
const ROW_CLICK: &str = "Click or press Tab to focus it, then ← → change its scope.";
const ASKS: &str = "An ask rule covers it";
const ASKS_TAIL: &str = ", so Caudra asks every time and can't remember it.";
const ALLOWED: &str = "Already allowed";
const ALLOWED_TAIL: &str = ". It runs without asking.";
pub(super) const COLLAPSED: &str =
    "Details lists each allowed command and the rule that covers it.\nPress ? to open Details.";
const BATCH_NEW: &str = "an answer; the others are already allowed or always ask.";
const MORE_REQUEST: &str = "more request";
const COMMAND: &str = "command";
const PRESS: &str = "Press ";
const VISITED: &str = "Visited.";
const TAB_CLICK: &str = "Click to open its page. Tab and Shift-Tab move between pages.";
const REVIEW_TAB: &str =
    "Every command with its scope and how long. Only Review applies the draft.\nClick to open it.";
const PAGE_ONCE: &str = "Runs it now without remembering it.";
const PAGE_RUNG: &str = "Remembers this scope for as long as Remember for says.";
const OWN_PATTERN: &str = "A prefix of your own ending in ` *` or `/*`, with at most eight tokens, that matches this command.";
const REMEMBER_FOR: &str =
    "How long Caudra remembers this command. Only lifetimes the chosen scope allows are offered.";
const REVIEW_REMEMBER: &str = "Stores each command with the scope and lifetime listed above.";
const REVIEW_ONCE: &str = "Runs every command once. Nothing is remembered.";
const REVIEW_MORE: &str =
    "Opens Customize with scopes for the whole line, such as these commands exactly.";
const SUGGESTED: &str = "Suggested from similar commands. i inspects it.";
const INCOMPLETE: &str = "Caudra can't show everything this scope allows, so it can't be chosen.";
const EFFECT_ALLOW: &str = "Allows the request with the scope and lifetime below.";
const EFFECT_DENY: &str = "Refuses it once, for this project, or for all projects. A remembered Deny is a rule /permissions lists.";
const LIFETIME_ONCE: &str = "Only this call.";
const LIFETIME_CONVERSATION: &str =
    "Until this conversation ends. It survives resume and covers subtasks.";
const LIFETIME_PROJECT: &str = "This project folder, in later conversations too.";
const LIFETIME_GLOBAL: &str = "Every project.";
const CUSTOMIZE_RUNG: &str = "Remembers this scope for the command.";
const WHOLE: &str = "Answers for the whole request, not one command.";
const CLICK_CHOOSE: &str = "Click to choose it.";
const CLICK_TYPE: &str = "Click to type it.";

/// A command row that needs an answer, as the main view draws it.
pub(super) struct NewRow {
    pub focused: bool,
    pub remembered: bool,
}

/// A batch row: its command when the column cut it, what its status means,
/// and how to change its scope. `scope` is what the row shows after the
/// command.
pub(super) fn row_tip(
    cut_command: Option<String>,
    status: RowStatus,
    new: Option<NewRow>,
    scope: &str,
) -> String {
    let mut lines: Vec<String> = cut_command.into_iter().collect();
    match (status, new) {
        (RowStatus::New, Some(row)) => {
            lines.push(if row.remembered {
                format!("{ROW_REMEMBERED}{scope}.")
            } else {
                ROW_ONCE.into()
            });
            lines.push(if row.focused { ROW_FOCUSED } else { ROW_CLICK }.into());
        }
        (RowStatus::New, None) => lines.push(ROW_FIXED.into()),
        (settled, _) => lines.push(settled_line(settled, scope)),
    }
    lines.join("\n")
}

/// A Review row: whatever its columns cut, and what a settled status means.
pub(super) fn review_row_tip(
    cut: bool,
    command: &str,
    status: RowStatus,
    scope: &str,
    how_long: &str,
) -> Option<String> {
    if status == RowStatus::New {
        return cut.then(|| match how_long {
            "" => format!("{command}\n‹{scope}›"),
            how_long => format!("{command}\n‹{scope}› · {how_long}"),
        });
    }
    let mut lines: Vec<String> = cut.then(|| command.to_owned()).into_iter().collect();
    lines.push(settled_line(status, how_long));
    Some(lines.join("\n"))
}

/// What already settles a row, named by `coverage` when it is known.
fn settled_line(status: RowStatus, coverage: &str) -> String {
    let (lead, tail, by) = match status {
        RowStatus::Asks => (ASKS, ASKS_TAIL, ("(", ")")),
        RowStatus::Allowed | RowStatus::New => (ALLOWED, ALLOWED_TAIL, ("by ", "")),
    };
    match coverage {
        "" => format!("{lead}{tail}"),
        coverage => format!("{lead} {}{coverage}{}{tail}", by.0, by.1),
    }
}

/// A footer key whose description was dropped for width.
pub(super) fn key_tip(label: &str, description: &str, clickable: bool) -> String {
    let mut characters = description.chars();
    let sentence: String = characters
        .next()
        .map(|first| first.to_uppercase().chain(characters).collect())
        .unwrap_or_default();
    let how = if clickable { CLICK_OR_PRESS } else { PRESS };
    format!("{sentence}.\n{how}{label}.")
}

/// What a scope's badges stand for: where it came from, and how it is
/// confirmed before it is stored.
fn option_lines(option: &PermissionRuleOption) -> Vec<String> {
    let mut lines = Vec::new();
    match (option.id.starts_with(COMMAND_TEMPLATE_PREFIX), option.seen) {
        (true, Some(seen)) => lines.push(format!(
            "Suggested from {seen} similar commands. i inspects it."
        )),
        (true, None) => lines.push(SUGGESTED.into()),
        (false, Some(seen)) => lines.push(format!("Seen {seen}× before.")),
        (false, None) => {}
    }
    if let Some(phrase) = &option.confirmation {
        lines.push(format!(
            "Confirm with Enter again, or type {} for a project or global rule.",
            review_text(phrase)
        ));
    }
    lines
}

fn lifetime_meaning(lifetime: &PermissionLifetime) -> &'static str {
    match lifetime {
        PermissionLifetime::Once => LIFETIME_ONCE,
        PermissionLifetime::Conversation => LIFETIME_CONVERSATION,
        PermissionLifetime::Project => LIFETIME_PROJECT,
        PermissionLifetime::Global => LIFETIME_GLOBAL,
    }
}

fn effect_tip(effect: Effect) -> String {
    let meaning = match effect {
        Effect::Allow => EFFECT_ALLOW,
        Effect::Deny => EFFECT_DENY,
    };
    format!("{meaning}\n{CLICK_OR_PRESS}← → with Effect focused.")
}

impl PermissionPrompt {
    /// The tooltip for whatever the last draw put under `pointer`.
    pub(crate) fn hover_tip(&self, pointer: Position) -> Option<Tip> {
        if !self.area.contains(pointer) {
            return None;
        }
        tooltip::area_tip(&self.tip_areas, Some(pointer)).or_else(|| {
            let hit = self.hits.iter().find(|hit| hit.area.contains(pointer))?;
            let text = self.tip_for(&hit.target)?;
            Some(Tip::at(TipKey::Row(hit.area), hit.area, text))
        })
    }

    fn tip_for(&self, target: &PromptTarget) -> Option<String> {
        match target {
            PromptTarget::Choice(choice) => self.choice_tip(*choice),
            PromptTarget::Item(index) => match self.panel {
                Panel::StepThrough => self.step_item_tip(*index),
                Panel::Customize => self.scope_item_tip(*index),
                Panel::Main | Panel::Details => None,
            },
            PromptTarget::Tab(row) => self.tab_tip(*row),
            PromptTarget::Review => Some(REVIEW_TAB.into()),
            PromptTarget::Effect(effect) => Some(effect_tip(*effect)),
            PromptTarget::Remember(lifetime) => Some(self.remember_tip(lifetime)),
            PromptTarget::Inspector(control) => self.inspector_panel()?.tip(control),
            PromptTarget::VisualScope(control) => ScopeView::tip(control),
            PromptTarget::Row(_) | PromptTarget::Hint(_) => None,
        }
    }

    /// A main-view answer. While a confirmation or a field is open a click
    /// on one does something else, so it offers no tip then.
    fn choice_tip(&self, choice: Choice) -> Option<String> {
        if self.pending.is_some() || self.state != PromptState::Normal {
            return None;
        }
        let choices = self.choices();
        let number = choices.iter().position(|found| *found == choice)? + 1;
        let own_rules = self.batch() && self.per_row();
        let mut lines = match choice {
            Choice::Once if choices.len() > 2 => vec![ONCE],
            Choice::Once if self.batch() => vec![ALL_ONCE],
            Choice::Once => vec![NOTHING_TO_REMEMBER],
            Choice::Conversation => vec![CONVERSATION],
            Choice::Project => vec![PROJECT, REVOKE],
            Choice::Deny => vec![DENY],
        };
        if choice.lifetime().is_some() {
            if own_rules {
                lines.push(OWN_RULES);
            }
            if self.scope_movable() {
                lines.push(SCOPE_KEYS_FIRST);
            }
        }
        let mut text = lines.join("\n");
        text.push_str(&format!(
            "\n{CLICK_OR_PRESS}{number} or {}.",
            choice.letter()
        ));
        Some(text)
    }

    /// A page's rung, or a Review answer.
    fn step_item_tip(&self, index: usize) -> Option<String> {
        let (request, step) = (self.current()?, self.step.as_ref()?);
        let number = index + 1;
        let Some(row) = step.page else {
            return Some(self.review_choice_tip(*REVIEW_CHOICES.get(index)?, number));
        };
        let choose = format!("{CLICK_OR_PRESS}{number} to choose it and go on.");
        let lines = match step.items(request, row).into_iter().nth(index)? {
            PageItem::OwnPattern => {
                vec![
                    OWN_PATTERN.into(),
                    format!("{CLICK_OR_PRESS}{number} to type it."),
                ]
            }
            PageItem::Grant(None) => vec![PAGE_ONCE.into(), choose],
            PageItem::Grant(Some(grant)) => {
                let mut lines = vec![PAGE_RUNG.to_owned()];
                lines.extend(
                    grant_option(request, row, &grant)
                        .map(option_lines)
                        .unwrap_or_default(),
                );
                lines.push(choose);
                lines
            }
        };
        Some(lines.join("\n"))
    }

    fn review_choice_tip(&self, choice: ReviewChoice, number: usize) -> String {
        match choice {
            ReviewChoice::Remember => {
                let meaning = if self.review_sentence(choice) == REMEMBER_AS_LISTED {
                    REVIEW_REMEMBER
                } else {
                    REVIEW_ONCE
                };
                format!("{meaning}\n{CLICK_OR_PRESS}{number} or y.")
            }
            ReviewChoice::Deny => format!("{DENY}\n{CLICK_OR_PRESS}{number} or n."),
            ReviewChoice::More => format!("{REVIEW_MORE}\n{CLICK_OR_PRESS}{number}."),
        }
    }

    /// A scope Customize lists.
    fn scope_item_tip(&self, index: usize) -> Option<String> {
        let (request, customize) = (self.current()?, self.customize.as_ref()?);
        let item = self.customize_items().into_iter().nth(index)?;
        let (_, _, selectable) = scope_item_label(request, customize, &item, self.batch());
        let (meaning, click) = match item {
            ScopeItem::OwnPattern => (OWN_PATTERN, CLICK_TYPE),
            ScopeItem::Row(_) => (CUSTOMIZE_RUNG, CLICK_CHOOSE),
            ScopeItem::Whole(_) => (WHOLE, CLICK_CHOOSE),
        };
        let mut lines = vec![meaning.to_owned()];
        if item != ScopeItem::OwnPattern {
            lines.extend(
                item_option(request, customize.row, &item)
                    .map(option_lines)
                    .unwrap_or_default(),
            );
        }
        lines.push(if selectable { click } else { INCOMPLETE }.into());
        Some(lines.join("\n"))
    }

    /// A page's tab: the command its label shortens.
    fn tab_tip(&self, row: usize) -> Option<String> {
        let (request, step) = (self.current()?, self.step.as_ref()?);
        let mut lines = vec![row_command(request, row)];
        if step.visited.get(row).copied().unwrap_or_default() {
            lines.push(VISITED.into());
        }
        lines.push(TAB_CLICK.into());
        Some(lines.join("\n"))
    }

    /// Customize's lifetime option, or the page's `Remember for`, whose
    /// target is the lifetime a click moves to.
    fn remember_tip(&self, lifetime: &PermissionLifetime) -> String {
        if self.panel == Panel::StepThrough {
            return format!("{REMEMBER_FOR}\n{CLICK_OR_PRESS}← → to change it.");
        }
        let mut lines = vec![lifetime_meaning(lifetime)];
        if matches!(
            lifetime,
            PermissionLifetime::Project | PermissionLifetime::Global
        ) {
            lines.push(REVOKE);
        }
        format!(
            "{}\n{CLICK_OR_PRESS}← → with Remember focused.",
            lines.join("\n")
        )
    }

    /// The title's right edge: who asks, the queue, and a batch's count,
    /// one line each.
    pub(super) fn place_tip(&self, area: Rect) -> Option<TipArea> {
        let place = self.place();
        if place.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        if let Some(requester) = self.requester_name() {
            lines.push(format!("Asked by the subagent {}.", review_text(requester)));
        }
        let pending = self.pending_count();
        if pending > 1 {
            let more = pending - 1;
            let verb = if more == 1 { "waits" } else { "wait" };
            lines.push(format!(
                "1 of {pending}: {} {verb} behind this one.",
                counted(more, MORE_REQUEST)
            ));
        }
        if let Some(request) = self.current()
            && self.batch()
        {
            let listed = self.listed_rows();
            let new = listed.iter().filter(|row| !covered(request, **row)).count();
            lines.push(format!(
                "{new} of {} new: {} {} {BATCH_NEW}",
                listed.len(),
                counted(new, COMMAND),
                if new == 1 { "needs" } else { "need" }
            ));
        }
        let width = u16::try_from(place.width() + 2)
            .unwrap_or(u16::MAX)
            .min(area.width.saturating_sub(2));
        Some(TipArea {
            area: Rect::new(area.right().saturating_sub(1 + width), area.y, width, 1),
            text: lines.join("\n"),
        })
    }
}

#[cfg(test)]
mod tests {
    use caudra_agent::permissions::{
        CONFINED_READ_AUTHORITY, PermissionLifetime, PermissionRequest, RuleOrigin,
    };
    use crossterm::event::KeyCode;
    use ratatui::layout::{Position, Rect};
    use test_case::test_case;

    use super::super::choices::Choice;
    use super::super::customize::{Effect, item_option};
    use super::super::decision::tests::commands_request;
    use super::super::inspector::tests::suggested_prompt;
    use super::super::view::tests::{
        BATCH, ROOMY_HEIGHT, ROOMY_WIDTH, SINGLE_COMMAND, batch_request, cover, heredoc_request,
        key, prompt_for, render, shell_prompt,
    };
    use super::super::{Panel, PermissionPrompt, PromptTarget};
    use super::*;

    const NARROW: u16 = 60;
    const TIGHT: u16 = 40;
    const NO_TIP: &str = "this control must raise a tooltip";
    const WRONG_TIP: &str = "the tooltip does not say what the control under the pointer means";
    const SPURIOUS: &str = "something drawn whole and plain must not raise a tooltip";
    const MISSING: &str = "the control under test was not drawn";
    const BROAD_PHRASE: &str = "ALLOW BROAD SHELL ACCESS";
    const CUT: char = '…';

    fn centre(area: Rect) -> Position {
        Position::new(area.x + area.width / 2, area.y + area.height / 2)
    }

    fn tip_on(prompt: &PermissionPrompt, target: &PromptTarget) -> Option<String> {
        let hit = prompt
            .hits
            .iter()
            .find(|hit| hit.target == *target)
            .expect(MISSING);
        prompt.hover_tip(centre(hit.area)).map(|tip| tip.text)
    }

    fn area_texts(prompt: &PermissionPrompt) -> Vec<&str> {
        prompt
            .tip_areas
            .iter()
            .map(|tip| tip.text.as_str())
            .collect()
    }

    fn drawn(mut prompt: PermissionPrompt, width: u16) -> PermissionPrompt {
        render(&mut prompt, width, ROOMY_HEIGHT);
        prompt
    }

    fn pressed(mut prompt: PermissionPrompt, width: u16, code: KeyCode) -> PermissionPrompt {
        prompt.handle_key(key(code));
        drawn(prompt, width)
    }

    fn opened(mut prompt: PermissionPrompt, width: u16, target: PromptTarget) -> PermissionPrompt {
        prompt.activate(target);
        drawn(prompt, width)
    }

    #[test_case(Choice::Once, ONCE, 1 ; "once")]
    #[test_case(Choice::Conversation, CONVERSATION, 2 ; "conversation")]
    #[test_case(Choice::Project, REVOKE, 3 ; "project")]
    #[test_case(Choice::Deny, DENY, 4 ; "deny")]
    fn an_answer_names_its_keys_and_what_it_remembers(
        choice: Choice,
        meaning: &str,
        number: usize,
    ) {
        let prompt = drawn(shell_prompt(SINGLE_COMMAND), ROOMY_WIDTH);
        let text = tip_on(&prompt, &PromptTarget::Choice(choice)).expect(NO_TIP);
        assert!(text.contains(meaning), "{WRONG_TIP}: {text}");
        let keys = format!("{CLICK_OR_PRESS}{number} or {}.", choice.letter());
        assert!(text.ends_with(&keys), "{WRONG_TIP}: {text}");
    }

    #[test]
    fn a_line_nothing_can_remember_says_so_on_its_yes() {
        let prompt = drawn(prompt_for(heredoc_request()), ROOMY_WIDTH);
        assert_eq!(prompt.choices().len(), 2, "{MISSING}");
        let text = tip_on(&prompt, &PromptTarget::Choice(Choice::Once)).expect(NO_TIP);
        assert!(text.starts_with(NOTHING_TO_REMEMBER), "{WRONG_TIP}: {text}");
    }

    #[test]
    fn an_answer_offers_no_tip_while_guidance_is_typed() {
        let prompt = pressed(
            drawn(shell_prompt(SINGLE_COMMAND), ROOMY_WIDTH),
            ROOMY_WIDTH,
            KeyCode::Char('n'),
        );
        assert_eq!(
            tip_on(&prompt, &PromptTarget::Choice(Choice::Once)),
            None,
            "{SPURIOUS}"
        );
    }

    #[test]
    fn batch_rows_say_what_their_status_means() {
        let prompt = drawn(prompt_for(batch_request()), ROOMY_WIDTH);
        let texts = area_texts(&prompt);
        for expected in [ROW_FOCUSED, ROW_CLICK, ASKS, ALLOWED] {
            assert!(
                texts.iter().any(|text| text.contains(expected)),
                "{WRONG_TIP}: {expected} in {texts:?}"
            );
        }
        let asks = prompt
            .tip_areas
            .iter()
            .find(|tip| tip.text.contains(ASKS))
            .expect(MISSING);
        let shown = prompt.hover_tip(centre(asks.area)).expect(NO_TIP);
        assert_eq!(shown.text, asks.text, "{WRONG_TIP}");
    }

    #[test_case(ROOMY_WIDTH, false ; "roomy")]
    #[test_case(NARROW, true ; "narrow")]
    fn a_row_names_its_command_only_when_the_column_cut_it(width: u16, cut: bool) {
        let prompt = drawn(prompt_for(batch_request()), width);
        let named = area_texts(&prompt)
            .iter()
            .any(|text| text.lines().next() == Some(BATCH[1]));
        assert_eq!(named, cut, "{WRONG_TIP}");
    }

    #[test]
    fn folded_allowed_rows_point_to_details() {
        let commands = ["make", "ls", "pwd", "whoami", "date"];
        let mut request = commands_request(&commands);
        for row in 1..commands.len() {
            cover(
                &mut request,
                row,
                RuleOrigin::Builtin,
                CONFINED_READ_AUTHORITY,
                false,
            );
        }
        let prompt = drawn(prompt_for(request), ROOMY_WIDTH);
        assert!(area_texts(&prompt).contains(&COLLAPSED), "{NO_TIP}");
    }

    #[test]
    fn the_title_explains_the_queue_and_the_batch_count() {
        let mut prompt = prompt_for(batch_request());
        let mut second: PermissionRequest = batch_request();
        second.id = "second".into();
        prompt.enqueue(Box::new(second), None);
        let prompt = drawn(prompt, ROOMY_WIDTH);
        let top_right = Position::new(prompt.area.right() - 4, prompt.area.y);
        let text = prompt.hover_tip(top_right).expect(NO_TIP).text;
        assert!(
            text.contains("1 of 2: 1 more request waits"),
            "{WRONG_TIP}: {text}"
        );
        assert!(
            text.contains("3 of 6 new: 3 commands"),
            "{WRONG_TIP}: {text}"
        );
    }

    #[test_case(TIGHT, true ; "tight")]
    #[test_case(ROOMY_WIDTH, false ; "roomy")]
    fn a_footer_key_explains_itself_only_when_its_description_was_dropped(
        width: u16,
        compact: bool,
    ) {
        let prompt = drawn(shell_prompt(SINGLE_COMMAND), width);
        let customize = key_tip("e", "customize", true);
        assert_eq!(
            area_texts(&prompt).contains(&customize.as_str()),
            compact,
            "{WRONG_TIP}"
        );
    }

    #[test]
    fn step_through_tabs_items_and_remember_explain_themselves() {
        let prompt = pressed(
            drawn(prompt_for(batch_request()), ROOMY_WIDTH),
            ROOMY_WIDTH,
            KeyCode::Char('e'),
        );
        assert_eq!(prompt.panel, Panel::StepThrough, "{MISSING}");
        let tab = tip_on(&prompt, &PromptTarget::Tab(1)).expect(NO_TIP);
        assert!(tab.starts_with(BATCH[1]), "{WRONG_TIP}: {tab}");
        let review = tip_on(&prompt, &PromptTarget::Review).expect(NO_TIP);
        assert_eq!(review, REVIEW_TAB, "{WRONG_TIP}");
        let items: Vec<String> = prompt
            .hits
            .iter()
            .filter(|hit| matches!(hit.target, PromptTarget::Item(_)))
            .filter_map(|hit| prompt.hover_tip(centre(hit.area)).map(|tip| tip.text))
            .collect();
        for expected in [PAGE_ONCE, PAGE_RUNG, OWN_PATTERN] {
            assert!(
                items.iter().any(|text| text.contains(expected)),
                "{WRONG_TIP}: {expected} in {items:?}"
            );
        }
        let remember = prompt
            .hits
            .iter()
            .find(|hit| matches!(hit.target, PromptTarget::Remember(_)))
            .expect(MISSING);
        let text = prompt.hover_tip(centre(remember.area)).expect(NO_TIP).text;
        assert!(text.starts_with(REMEMBER_FOR), "{WRONG_TIP}: {text}");
    }

    #[test_case(ROOMY_WIDTH, false ; "roomy")]
    #[test_case(NARROW, true ; "narrow")]
    fn review_rows_restore_what_their_columns_cut(width: u16, cut: bool) {
        let prompt = pressed(
            drawn(prompt_for(batch_request()), width),
            width,
            KeyCode::Char('e'),
        );
        let prompt = opened(prompt, width, PromptTarget::Review);
        let texts = area_texts(&prompt);
        assert!(texts.iter().any(|text| text.contains(ASKS)), "{NO_TIP}");
        let named = texts
            .iter()
            .any(|text| text.lines().next() == Some(BATCH[1]));
        assert_eq!(named, cut, "{WRONG_TIP}: {texts:?}");
        let more = tip_on(&prompt, &PromptTarget::Item(2)).expect(NO_TIP);
        assert_eq!(
            more,
            format!("{REVIEW_MORE}\n{CLICK_OR_PRESS}3."),
            "{WRONG_TIP}"
        );
    }

    #[test_case(Effect::Allow, EFFECT_ALLOW ; "allow")]
    #[test_case(Effect::Deny, EFFECT_DENY ; "deny")]
    fn customize_effects_explain_themselves(effect: Effect, meaning: &str) {
        let prompt = pressed(
            drawn(shell_prompt(SINGLE_COMMAND), ROOMY_WIDTH),
            ROOMY_WIDTH,
            KeyCode::Char('e'),
        );
        assert_eq!(prompt.panel, Panel::Customize, "{MISSING}");
        let text = tip_on(&prompt, &PromptTarget::Effect(effect)).expect(NO_TIP);
        assert!(text.starts_with(meaning), "{WRONG_TIP}: {text}");
    }

    #[test_case(PermissionLifetime::Conversation, LIFETIME_CONVERSATION, false ; "conversation")]
    #[test_case(PermissionLifetime::Project, LIFETIME_PROJECT, true ; "project")]
    fn customize_lifetimes_explain_how_long(
        lifetime: PermissionLifetime,
        meaning: &str,
        revocable: bool,
    ) {
        let prompt = pressed(
            drawn(shell_prompt(SINGLE_COMMAND), ROOMY_WIDTH),
            ROOMY_WIDTH,
            KeyCode::Char('e'),
        );
        let text = tip_on(&prompt, &PromptTarget::Remember(lifetime)).expect(NO_TIP);
        assert!(text.starts_with(meaning), "{WRONG_TIP}: {text}");
        assert_eq!(text.contains(REVOKE), revocable, "{WRONG_TIP}: {text}");
    }

    #[test]
    fn a_broad_scope_names_the_phrase_that_confirms_it() {
        let prompt = pressed(
            drawn(shell_prompt(SINGLE_COMMAND), ROOMY_WIDTH),
            ROOMY_WIDTH,
            KeyCode::Char('e'),
        );
        let request = prompt.current().expect(MISSING);
        let customize = prompt.customize.as_ref().expect(MISSING);
        let broad = prompt
            .customize_items()
            .iter()
            .position(|item| {
                item_option(request, customize.row, item)
                    .is_some_and(|option| option.confirmation.as_deref() == Some(BROAD_PHRASE))
            })
            .expect(MISSING);
        let text = tip_on(&prompt, &PromptTarget::Item(broad)).expect(NO_TIP);
        assert!(text.contains(BROAD_PHRASE), "{WRONG_TIP}: {text}");
    }

    #[test]
    fn inspector_controls_explain_themselves() {
        let prompt = pressed(
            drawn(suggested_prompt(), ROOMY_WIDTH),
            ROOMY_WIDTH,
            KeyCode::Char('e'),
        );
        let prompt = pressed(prompt, ROOMY_WIDTH, KeyCode::Char('i'));
        assert!(prompt.inspector.is_some(), "{MISSING}");
        let explained = prompt
            .hits
            .iter()
            .filter(|hit| matches!(hit.target, PromptTarget::Inspector(_)))
            .filter(|hit| prompt.hover_tip(centre(hit.area)).is_some())
            .count();
        assert!(explained > 0, "{NO_TIP}");
        assert!(
            !prompt.tip_areas.is_empty(),
            "the fixed chips must explain their roles"
        );
    }

    /// Every tip the prompt can raise right now.
    fn every_tip(prompt: &PermissionPrompt) -> Vec<String> {
        prompt
            .hits
            .iter()
            .map(|hit| hit.area)
            .chain(prompt.tip_areas.iter().map(|tip| tip.area))
            .filter_map(|area| prompt.hover_tip(centre(area)).map(|tip| tip.text))
            .collect()
    }

    #[test]
    fn every_tip_fits_its_box_without_being_cut() {
        let batch = drawn(prompt_for(batch_request()), ROOMY_WIDTH);
        let page = pressed(
            drawn(prompt_for(batch_request()), NARROW),
            NARROW,
            KeyCode::Char('e'),
        );
        let review = opened(
            pressed(
                drawn(prompt_for(batch_request()), NARROW),
                NARROW,
                KeyCode::Char('e'),
            ),
            NARROW,
            PromptTarget::Review,
        );
        let customize = pressed(
            drawn(shell_prompt(SINGLE_COMMAND), ROOMY_WIDTH),
            ROOMY_WIDTH,
            KeyCode::Char('e'),
        );
        let inspector = pressed(
            pressed(
                drawn(suggested_prompt(), ROOMY_WIDTH),
                ROOMY_WIDTH,
                KeyCode::Char('e'),
            ),
            ROOMY_WIDTH,
            KeyCode::Char('i'),
        );
        let frame = Rect::new(0, 0, ROOMY_WIDTH, ROOMY_HEIGHT);
        let anchor = Rect::new(ROOMY_WIDTH / 2, ROOMY_HEIGHT / 2, 1, 1);
        for prompt in [batch, page, review, customize, inspector] {
            for text in every_tip(&prompt) {
                let placed = tooltip::place(anchor, frame, &text).expect(NO_TIP);
                let last = placed.lines.last().expect(NO_TIP);
                assert!(!last.ends_with(CUT), "a tip too long for its box: {text}");
            }
        }
    }

    #[test]
    fn nothing_outside_the_prompt_or_in_a_cramped_one_raises_a_tip() {
        let prompt = drawn(shell_prompt(SINGLE_COMMAND), ROOMY_WIDTH);
        let below = Position::new(prompt.area.x, prompt.area.bottom());
        assert_eq!(prompt.hover_tip(below), None, "{SPURIOUS}");
        let mut cramped = shell_prompt(SINGLE_COMMAND);
        render(&mut cramped, TIGHT / 2, ROOMY_HEIGHT);
        let inside = centre(cramped.area);
        assert_eq!(cramped.hover_tip(inside), None, "{SPURIOUS}");
    }
}
