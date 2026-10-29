//! Reminds the main agent to save a memory note right after something worth
//! keeping happened: a command that failed now passes, a long search ended in
//! an edit, the user corrected it, or the context nears compaction. Reminders
//! thin out as the project's notes grow, and stop for good once the agent
//! saves notes without them.
//!
//! The turn is read back from the transcript rather than counted in fields,
//! because the agent is rebuilt for every run while history outlives it.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use caudra_providers::{HistoryItemKind, Message, ToolNameAliases, UserOrigin};
use caudra_storage::id::CaudraId;
use caudra_storage::memory_nudges::NudgeStanding;
use serde_json::Value;

use super::history::{History, user_request};
use crate::decisions::counted;
use crate::prompt::{MEMORY_NUDGE_EVENT_SLOT, MEMORY_NUDGE_NOTES_SLOT, MEMORY_NUDGE_PROMPT};
use crate::tools::native::batch;
use crate::tools::{
    BATCH_TOOL_NAME, FILE_APPLY_PATCH_TOOL_NAME, FILE_EDIT_TOOL_NAME, FILE_GLOB_TOOL_NAME,
    FILE_GREP_TOOL_NAME, FILE_INDEX_TOOL_NAME, FILE_READ_TOOL_NAME, FILE_WRITE_TOOL_NAME,
    MEMORY_TOOL_NAME, SHELL_TOOL_NAME,
};

const SOURCE_CODE_CONTEXT_TOOL_NAME: &str = "source_code_context";
const SOURCE_CODE_REFS_TOOL_NAME: &str = "source_code_refs";
const COMMAND_FIELD: &str = "command";
const MEMORY_WRITES: [&str; 2] = ["write", "delete"];
/// Reads and searches before a turn's first edit that make it a discovery.
const DISCOVERY_CALLS: usize = 8;
/// A note written this many responses or fewer after a nudge was asked for.
pub(super) const ATTRIBUTION_RESPONSES: usize = 5;
/// Nudges a project gets a day, across its sessions.
pub(super) const MAX_NUDGES_PER_DAY: u32 = 3;
const NO_NOTES: &str = "no memory notes yet";
const NOTE: &str = "memory note";
const NOTES: &str = "memory notes";
const RECOVERY_EVENT: &str = "A command that failed earlier in this turn now passes.";
const CORRECTION_EVENT: &str = "The user corrected you or stated a rule for this project.";
const DISCOVERY_EVENT: &str = "You read and searched a lot before this edit.";
const NEAR_COMPACTION_EVENT: &str =
    "The context will be compacted soon, and a summary will replace the details of this turn.";
/// How often a project is nudged, by how many live notes it has. Past the
/// last tier it never is.
const TIERS: [Tier; 3] = [
    Tier {
        notes_below: 1,
        min_responses: 10,
        per_session: 3,
        discovery: true,
    },
    Tier {
        notes_below: 5,
        min_responses: 20,
        per_session: 2,
        discovery: true,
    },
    Tier {
        notes_below: 15,
        min_responses: 20,
        per_session: 1,
        discovery: false,
    },
];
/// Past this many responses the count since the last nudge stops mattering.
const RESPONSES_CAP: usize = 20;
const _: () = {
    let mut tier = 0;
    while tier < TIERS.len() {
        assert!(TIERS[tier].min_responses <= RESPONSES_CAP);
        tier += 1;
    }
    assert!(ATTRIBUTION_RESPONSES < RESPONSES_CAP);
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Trigger {
    Recovery,
    Correction,
    Discovery,
    NearCompaction,
}

impl Trigger {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Recovery => "recovery",
            Self::Correction => "correction",
            Self::Discovery => "discovery",
            Self::NearCompaction => "near_compaction",
        }
    }

    fn event(self) -> &'static str {
        match self {
            Self::Recovery => RECOVERY_EVENT,
            Self::Correction => CORRECTION_EVENT,
            Self::Discovery => DISCOVERY_EVENT,
            Self::NearCompaction => NEAR_COMPACTION_EVENT,
        }
    }
}

struct Tier {
    notes_below: u64,
    min_responses: usize,
    per_session: u32,
    /// Whether a discovery is reason enough, which it stops being once the
    /// project has a handful of notes to follow.
    discovery: bool,
}

/// What the transcript says about the turn, as of its newest response.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Reading {
    /// What the newest response showed, if anything.
    pub trigger: Option<Trigger>,
    /// The newest response wrote or deleted a note.
    pub wrote: bool,
    /// A nudge already went in since the user's message.
    pub nudged_this_turn: bool,
    /// Responses since the last nudge, the newest included, up to
    /// [`RESPONSES_CAP`].
    pub responses_since_nudge: usize,
}

/// Whether a nudge for `trigger` may go in, given where the project and the
/// session stand.
pub(super) fn allowed(
    trigger: Trigger,
    standing: &NudgeStanding,
    responses_since_nudge: usize,
) -> bool {
    if standing.graduated || standing.session_wrote || standing.nudges_since >= MAX_NUDGES_PER_DAY {
        return false;
    }
    TIERS
        .iter()
        .find(|tier| standing.live_notes < tier.notes_below)
        .is_some_and(|tier| {
            standing.session_nudges < tier.per_session
                && responses_since_nudge >= tier.min_responses
                && (tier.discovery || trigger != Trigger::Discovery)
        })
}

pub(super) fn reminder(trigger: Trigger, live_notes: u64) -> Message {
    let notes = match live_notes {
        0 => NO_NOTES.to_owned(),
        count => counted(usize::try_from(count).unwrap_or(usize::MAX), NOTE, NOTES),
    };
    Message::synthetic(
        MEMORY_NUDGE_PROMPT
            .trim_end()
            .replace(MEMORY_NUDGE_EVENT_SLOT, trigger.event())
            .replace(MEMORY_NUDGE_NOTES_SLOT, &notes),
    )
}

/// The part of every nudge ahead of its event, which marks one in the
/// transcript.
fn marker() -> &'static str {
    MEMORY_NUDGE_PROMPT
        .split(MEMORY_NUDGE_EVENT_SLOT)
        .next()
        .unwrap_or(MEMORY_NUDGE_PROMPT)
}

fn is_nudge(kind: &HistoryItemKind) -> bool {
    matches!(
        kind,
        HistoryItemKind::User { text, origin: UserOrigin::Synthetic, .. } if text.starts_with(marker())
    )
}

/// The reply the newest user message answers, `None` before any reply.
pub(super) fn previous_reply(history: &History) -> Option<&str> {
    let mut items = history.transcript().rev();
    items.find(|item| user_request(&item.kind).is_some())?;
    for item in items {
        match &item.kind {
            HistoryItemKind::AssistantText {
                text,
                is_compaction_summary: false,
                ..
            } if !text.trim().is_empty() => return Some(text),
            kind if user_request(kind).is_some() => return None,
            _ => {}
        }
    }
    None
}

struct Call<'a> {
    group: CaudraId,
    name: Cow<'a, str>,
    input: Cow<'a, Value>,
    /// Unknown for a `batch` child, whose result is its parent's.
    failed: Option<bool>,
}

impl Call<'_> {
    fn field(&self, key: &str) -> Option<&str> {
        self.input.get(key).and_then(Value::as_str)
    }

    fn edits(&self) -> bool {
        matches!(
            self.name.as_ref(),
            FILE_WRITE_TOOL_NAME | FILE_EDIT_TOOL_NAME | FILE_APPLY_PATCH_TOOL_NAME
        )
    }

    fn reads(&self) -> bool {
        matches!(
            self.name.as_ref(),
            FILE_READ_TOOL_NAME
                | FILE_GREP_TOOL_NAME
                | FILE_GLOB_TOOL_NAME
                | FILE_INDEX_TOOL_NAME
                | SOURCE_CODE_CONTEXT_TOOL_NAME
                | SOURCE_CODE_REFS_TOOL_NAME
        )
    }

    fn writes_memory(&self) -> bool {
        self.name == MEMORY_TOOL_NAME
            && self.failed != Some(true)
            && self
                .field(COMMAND_FIELD)
                .is_some_and(|command| MEMORY_WRITES.contains(&command))
    }

    /// The shell command, trimmed, so a rerun matches its first attempt.
    fn shell_command(&self) -> Option<&str> {
        (self.name == SHELL_TOOL_NAME)
            .then(|| self.field(COMMAND_FIELD))
            .flatten()
            .map(str::trim)
    }
}

pub(super) fn read(history: &History, aliases: Option<&ToolNameAliases>) -> Reading {
    let mut responses = HashSet::new();
    let mut in_turn = true;
    let mut nudged_this_turn = false;
    for item in history.transcript().rev() {
        if is_nudge(&item.kind) {
            nudged_this_turn = in_turn;
            break;
        }
        in_turn &= user_request(&item.kind).is_none();
        if matches!(
            item.kind,
            HistoryItemKind::ToolCall { .. } | HistoryItemKind::AssistantText { .. }
        ) && responses.insert(item.group_id)
            && responses.len() >= RESPONSES_CAP
        {
            break;
        }
    }
    let calls = calls(history, aliases);
    let newest = calls.last().map(|call| call.group);
    let (latest, earlier): (Vec<_>, Vec<_>) =
        calls.iter().partition(|call| Some(call.group) == newest);
    let recovered = latest
        .iter()
        .filter(|call| call.failed == Some(false))
        .filter_map(|call| call.shell_command())
        .any(|command| {
            earlier
                .iter()
                .any(|call| call.failed == Some(true) && call.shell_command() == Some(command))
        });
    let discovered = latest.iter().any(|call| call.edits())
        && !earlier.iter().any(|call| call.edits())
        && earlier.iter().filter(|call| call.reads()).count() >= DISCOVERY_CALLS;
    Reading {
        trigger: if recovered {
            Some(Trigger::Recovery)
        } else {
            discovered.then_some(Trigger::Discovery)
        },
        wrote: latest.iter().any(|call| call.writes_memory()),
        nudged_this_turn,
        responses_since_nudge: responses.len(),
    }
}

/// The calls since the user's newest message, oldest first, with `batch`
/// children in place of their parent.
fn calls<'a>(history: &'a History, aliases: Option<&ToolNameAliases>) -> Vec<Call<'a>> {
    let mut items: Vec<_> = history
        .transcript()
        .rev()
        .take_while(|item| user_request(&item.kind).is_none())
        .collect();
    items.reverse();
    let failed: HashMap<&str, bool> = items
        .iter()
        .filter_map(|item| match &item.kind {
            HistoryItemKind::ToolResult {
                call_id, is_error, ..
            } => Some((call_id.as_str(), *is_error)),
            _ => None,
        })
        .collect();
    let mut calls = Vec::new();
    for item in items {
        let HistoryItemKind::ToolCall {
            call_id,
            name,
            input,
            ..
        } = &item.kind
        else {
            continue;
        };
        if name == BATCH_TOOL_NAME {
            calls.extend(
                batch::child_calls(input)
                    .into_iter()
                    .map(|(name, input)| Call {
                        group: item.group_id,
                        name: Cow::Owned(canonical(aliases, &name).to_owned()),
                        input: Cow::Owned(input),
                        failed: None,
                    }),
            );
        } else {
            calls.push(Call {
                group: item.group_id,
                name: Cow::Borrowed(name),
                input: Cow::Borrowed(input),
                failed: failed.get(call_id.as_str()).copied(),
            });
        }
    }
    calls
}

/// A batch child keeps the name the model wrote, which under a provider that
/// renames tools is the wire name rather than the registry's.
fn canonical<'a>(aliases: Option<&'a ToolNameAliases>, name: &'a str) -> &'a str {
    aliases
        .and_then(|aliases| aliases.get(name))
        .map_or(name, String::as_str)
}

#[cfg(test)]
mod tests {
    use caudra_providers::{ContentBlock, Role};
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const REQUEST: &str = "fix the build";
    const REPLY: &str = "The build passes now.";
    const FAILING: &str = "cargo test";
    const OTHER: &str = "cargo check";
    const NOTE_PATH: &str = "build.md";

    #[derive(Clone)]
    enum Step {
        Request,
        Reply(&'static str),
        Nudge,
        /// One response: each call's tool, input, and whether its result is
        /// an error.
        Response(Vec<(&'static str, Value, bool)>),
    }

    use Step::{Nudge, Reply, Request, Response};

    fn shell(command: &str, failed: bool) -> (&'static str, Value, bool) {
        (SHELL_TOOL_NAME, json!({ COMMAND_FIELD: command }), failed)
    }

    fn tool(name: &'static str) -> (&'static str, Value, bool) {
        (name, json!({}), false)
    }

    fn memory(command: &str) -> (&'static str, Value, bool) {
        (
            MEMORY_TOOL_NAME,
            json!({ COMMAND_FIELD: command, "path": NOTE_PATH }),
            false,
        )
    }

    fn reads(count: usize) -> Vec<Step> {
        (0..count)
            .map(|_| Response(vec![tool(FILE_READ_TOOL_NAME)]))
            .collect()
    }

    fn history(steps: Vec<Step>) -> History {
        let mut messages = Vec::new();
        for (index, step) in steps.into_iter().enumerate() {
            match step {
                Request => messages.push(Message::user(REQUEST.into())),
                Reply(text) => messages.push(Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text { text: text.into() }],
                    ..Default::default()
                }),
                Nudge => messages.push(reminder(Trigger::Recovery, 0)),
                Response(calls) => {
                    let ids: Vec<_> = (0..calls.len())
                        .map(|call| format!("call_{index}_{call}"))
                        .collect();
                    messages.push(Message {
                        role: Role::Assistant,
                        content: calls
                            .iter()
                            .zip(&ids)
                            .map(|((name, input, _), id)| {
                                ContentBlock::tool_use(id, *name, input.clone())
                            })
                            .collect(),
                        ..Default::default()
                    });
                    messages.push(Message {
                        role: Role::User,
                        content: calls
                            .iter()
                            .zip(ids)
                            .map(|((_, _, failed), id)| ContentBlock::ToolResult {
                                tool_use_id: id,
                                content: String::new(),
                                is_error: *failed,
                                output_ref: None,
                            })
                            .collect(),
                        ..Default::default()
                    });
                }
            }
        }
        History::new(messages)
    }

    fn reading(steps: Vec<Step>) -> Reading {
        read(&history(steps), None)
    }

    fn standing(live_notes: u64, session_nudges: u32) -> NudgeStanding {
        NudgeStanding {
            live_notes,
            session_nudges,
            ..NudgeStanding::default()
        }
    }

    #[test_case(vec![Request, Response(vec![shell(FAILING, true)]), Response(vec![tool(FILE_EDIT_TOOL_NAME)]), Response(vec![shell(FAILING, false)])] => Some(Trigger::Recovery) ; "a_failed_command_passes_after_a_fix")]
    #[test_case(vec![Request, Response(vec![shell(FAILING, true)]), Response(vec![shell(OTHER, false)])] => None ; "another_command_passing_is_no_recovery")]
    #[test_case(vec![Request, Response(vec![shell(FAILING, true)]), Response(vec![shell(&format!("  {FAILING}\n"), false)])] => Some(Trigger::Recovery) ; "whitespace_does_not_change_the_command")]
    #[test_case(vec![Response(vec![shell(FAILING, true)]), Request, Response(vec![shell(FAILING, false)])] => None ; "a_failure_before_the_request_does_not_count")]
    #[test_case(vec![Request, Response(vec![shell(FAILING, true)]), Response(vec![shell(FAILING, true)])] => None ; "failing_again_is_no_recovery")]
    #[test_case([vec![Request], reads(DISCOVERY_CALLS), vec![Response(vec![tool(FILE_EDIT_TOOL_NAME)])]].concat() => Some(Trigger::Discovery) ; "a_long_search_ends_in_an_edit")]
    #[test_case([vec![Request], reads(DISCOVERY_CALLS - 1), vec![Response(vec![tool(FILE_EDIT_TOOL_NAME)])]].concat() => None ; "a_short_search_is_no_discovery")]
    #[test_case([vec![Request, Response(vec![tool(FILE_WRITE_TOOL_NAME)])], reads(DISCOVERY_CALLS), vec![Response(vec![tool(FILE_EDIT_TOOL_NAME)])]].concat() => None ; "only_the_first_edit_counts")]
    #[test_case([vec![Request], reads(DISCOVERY_CALLS), vec![Response(vec![(BATCH_TOOL_NAME, json!({ "tool_calls": [{ "tool": FILE_EDIT_TOOL_NAME, "parameters": {} }] }), false)])]].concat() => Some(Trigger::Discovery) ; "an_edit_inside_a_batch")]
    fn the_newest_response_shows_a_trigger(steps: Vec<Step>) -> Option<Trigger> {
        reading(steps).trigger
    }

    #[test_case(vec![Request, Response(vec![memory("write")])] => true ; "a_write")]
    #[test_case(vec![Request, Response(vec![memory("delete")])] => true ; "a_delete")]
    #[test_case(vec![Request, Response(vec![memory("search")])] => false ; "a_search")]
    #[test_case(vec![Request, Response(vec![memory("write")]), Response(vec![tool(FILE_READ_TOOL_NAME)])] => false ; "an_earlier_response")]
    fn the_newest_response_wrote_a_note(steps: Vec<Step>) -> bool {
        reading(steps).wrote
    }

    #[test_case(vec![Request, Response(vec![tool(FILE_READ_TOOL_NAME)])] => (false, 1) ; "no_nudge_counts_the_session")]
    #[test_case(vec![Request, Nudge, Response(vec![tool(FILE_READ_TOOL_NAME)]), Response(vec![tool(FILE_READ_TOOL_NAME)])] => (true, 2) ; "a_nudge_in_this_turn")]
    #[test_case(vec![Request, Nudge, Reply(REPLY), Request, Response(vec![tool(FILE_READ_TOOL_NAME)])] => (false, 2) ; "a_nudge_in_an_earlier_turn")]
    #[test_case([vec![Request], reads(RESPONSES_CAP + 5)].concat() => (false, RESPONSES_CAP) ; "the_count_stops_at_the_cap")]
    fn responses_count_from_the_last_nudge(steps: Vec<Step>) -> (bool, usize) {
        let reading = reading(steps);
        (reading.nudged_this_turn, reading.responses_since_nudge)
    }

    #[test_case(standing(0, 0), Trigger::Recovery, 10 => true ; "no_notes_after_ten_responses")]
    #[test_case(standing(0, 0), Trigger::Recovery, 9 => false ; "no_notes_too_soon")]
    #[test_case(standing(0, 3), Trigger::Recovery, RESPONSES_CAP => false ; "no_notes_three_a_session")]
    #[test_case(standing(4, 1), Trigger::Discovery, RESPONSES_CAP => true ; "few_notes_take_a_discovery")]
    #[test_case(standing(4, 1), Trigger::Recovery, 19 => false ; "few_notes_wait_twenty")]
    #[test_case(standing(4, 2), Trigger::Recovery, RESPONSES_CAP => false ; "few_notes_two_a_session")]
    #[test_case(standing(5, 0), Trigger::Discovery, RESPONSES_CAP => false ; "more_notes_skip_discovery")]
    #[test_case(standing(14, 0), Trigger::NearCompaction, RESPONSES_CAP => true ; "more_notes_keep_compaction")]
    #[test_case(standing(14, 1), Trigger::Correction, RESPONSES_CAP => false ; "more_notes_one_a_session")]
    #[test_case(standing(15, 0), Trigger::Correction, RESPONSES_CAP => false ; "enough_notes_never")]
    #[test_case(NudgeStanding { nudges_since: MAX_NUDGES_PER_DAY, ..standing(0, 0) }, Trigger::Recovery, RESPONSES_CAP => false ; "the_daily_cap")]
    #[test_case(NudgeStanding { session_wrote: true, ..standing(0, 0) }, Trigger::Recovery, RESPONSES_CAP => false ; "the_session_wrote")]
    #[test_case(NudgeStanding { graduated: true, ..standing(0, 0) }, Trigger::Recovery, RESPONSES_CAP => false ; "graduated")]
    fn nudges_thin_out_as_notes_grow(
        standing: NudgeStanding,
        trigger: Trigger,
        responses: usize,
    ) -> bool {
        allowed(trigger, &standing, responses)
    }

    #[test_case(0, NO_NOTES ; "no_notes")]
    #[test_case(1, "1 memory note." ; "one_note")]
    #[test_case(3, "3 memory notes." ; "several_notes")]
    fn the_reminder_names_the_event_and_the_notes(live_notes: u64, notes: &str) {
        let message = reminder(Trigger::Correction, live_notes);
        let text = message.first_text_content().unwrap();

        assert!(text.contains(CORRECTION_EVENT));
        assert!(text.contains(notes));
        assert!(!text.contains(MEMORY_NUDGE_EVENT_SLOT) && !text.contains(MEMORY_NUDGE_NOTES_SLOT));
        assert!(is_nudge(
            &history(vec![Nudge]).transcript().next().unwrap().kind
        ));
    }

    #[test_case(vec![Reply(REPLY), Request] => Some(REPLY) ; "the_reply_before_the_message")]
    #[test_case(vec![Request] => None ; "a_first_message")]
    #[test_case(vec![Reply(REPLY), Request, Request] => None ; "a_message_after_an_unanswered_one")]
    fn the_message_answers_the_previous_reply(steps: Vec<Step>) -> Option<&'static str> {
        let history = history(steps);
        previous_reply(&history).map(|reply| if reply == REPLY { REPLY } else { "" })
    }
}
