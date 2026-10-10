//! Tells an agent to act once a request has outlasted several compactions with
//! no work started: the sign of a model that keeps investigating while each
//! summary throws away what it gathered.
//!
//! Read from the transcript rather than counted in a field, because the agent
//! is rebuilt for every run while history outlives it, and a restored session
//! has to arrive at the same count.

use caudra_providers::{HistoryItemKind, Message, ToolNameAliases, UserOrigin};
use serde_json::Value;

use super::compaction::CONTINUE_AFTER_COMPACT;
use super::history::History;
use crate::AgentMode;
use crate::prompt::{
    COMPACTIONS_SLOT, WORK_NUDGE_BUILD_PROMPT, WORK_NUDGE_PLAN_PROMPT, WORK_NUDGE_REPORT_PROMPT,
};
use crate::tools::native::batch;
use crate::tools::profile_policy::PLAN_TOOL_NAME;
use crate::tools::{
    BATCH_TOOL_NAME, FILE_APPLY_PATCH_TOOL_NAME, FILE_EDIT_TOOL_NAME, FILE_WRITE_TOOL_NAME,
    QUESTION_TOOL_NAME, TASK_TOOL_NAME, WORKFLOW_TOOL_NAME,
};

const ACTION_FIELD: &str = "action";
const MODE_FIELD: &str = "mode";
const PLAN_WRITE_ACTION: &str = "write";
const WORKFLOW_START_ACTION: &str = "start";
const PLAN_TASK_MODE: &str = "plan";

/// The reminder to follow a compaction with, once `threshold` of them have
/// passed since the latest request without work starting. Zero disables it.
pub(super) fn reminder(
    history: &History,
    mode: &AgentMode,
    aliases: Option<&ToolNameAliases>,
    threshold: u32,
) -> Option<Message> {
    if threshold == 0 {
        return None;
    }
    let compactions = stalled_compactions(history, mode, aliases);
    (compactions >= threshold).then(|| render(mode, compactions))
}

/// Compactions since the latest request, or zero once anything since then
/// started work. Counts the continuation each one leaves rather than its
/// summary: the summary sits above the turns compaction keeps verbatim, which
/// can include the request itself.
fn stalled_compactions(
    history: &History,
    mode: &AgentMode,
    aliases: Option<&ToolNameAliases>,
) -> u32 {
    let mut compactions = 0;
    for item in history.transcript().rev() {
        match &item.kind {
            HistoryItemKind::User {
                origin: UserOrigin::Turn,
                ..
            } => break,
            HistoryItemKind::User {
                origin: UserOrigin::Synthetic,
                text,
                ..
            } if text.starts_with(CONTINUE_AFTER_COMPACT) => compactions += 1,
            HistoryItemKind::ToolCall { name, input, .. }
                if starts_work(mode, canonical(aliases, name), input, aliases) =>
            {
                return 0;
            }
            _ => {}
        }
    }
    compactions
}

/// Whether a call counts as acting in `mode`. An attempt is enough: a failed
/// edit still shows the agent stopped only reading.
fn starts_work(
    mode: &AgentMode,
    name: &str,
    input: &Value,
    aliases: Option<&ToolNameAliases>,
) -> bool {
    let field = |key: &str| input.get(key).and_then(Value::as_str);
    match name {
        BATCH_TOOL_NAME => batch::child_calls(input)
            .iter()
            .any(|(child, params)| starts_work(mode, canonical(aliases, child), params, aliases)),
        FILE_WRITE_TOOL_NAME | FILE_EDIT_TOOL_NAME | FILE_APPLY_PATCH_TOOL_NAME => {
            *mode == AgentMode::Build
        }
        TASK_TOOL_NAME => *mode == AgentMode::Build && field(MODE_FIELD) != Some(PLAN_TASK_MODE),
        WORKFLOW_TOOL_NAME => {
            *mode == AgentMode::Build && field(ACTION_FIELD) == Some(WORKFLOW_START_ACTION)
        }
        PLAN_TOOL_NAME => mode.is_planning() && field(ACTION_FIELD) == Some(PLAN_WRITE_ACTION),
        QUESTION_TOOL_NAME => mode.is_planning(),
        _ => false,
    }
}

/// A batch child keeps the name the model wrote, which under a provider that
/// renames tools is the wire name rather than the registry's.
fn canonical<'a>(aliases: Option<&'a ToolNameAliases>, name: &'a str) -> &'a str {
    aliases
        .and_then(|aliases| aliases.get(name))
        .map_or(name, String::as_str)
}

fn render(mode: &AgentMode, compactions: u32) -> Message {
    let template = match mode {
        AgentMode::Build => WORK_NUDGE_BUILD_PROMPT,
        AgentMode::Plan(_) | AgentMode::RemotePlan(_) => WORK_NUDGE_PLAN_PROMPT,
        AgentMode::ReadOnly => WORK_NUDGE_REPORT_PROMPT,
    };
    Message::synthetic(template.replace(COMPACTIONS_SLOT, &compactions.to_string()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use caudra_providers::{ContentBlock, Role};
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::tools::{FILE_READ_TOOL_NAME, MEMORY_TOOL_NAME};

    const THRESHOLD: u32 = 2;
    const THRESHOLD_REACHED: &str = "two compactions without work reach the threshold";
    const REQUEST: &str = "implement the feature";
    const RESULT: &str = "ok";
    const PLAN_PATH: &str = "/tmp/plan.md";
    const POST_INSTRUCTIONS: &str = "Re-read plan.md";
    const WIRE_EDIT: &str = "mcp_File_edit";
    const BUILD_HEADING: &str = "# Start the work";
    const PLAN_HEADING: &str = "# Write the plan";
    const REPORT_HEADING: &str = "# Report what you have";
    const UNFILLED_SLOT: &str = "the count slot must be filled";
    const WRONG_WORDING: &str = "each mode gets its own wording";
    const NOT_SYNTHETIC: &str = "the reminder must read as Caudra's, not the user's";

    enum Step {
        Request,
        Compacted,
        Synthetic(&'static str),
        Call(&'static str, Value),
    }

    use Step::{Call, Compacted, Request, Synthetic};

    fn messages(steps: Vec<Step>) -> Vec<Message> {
        steps
            .into_iter()
            .enumerate()
            .flat_map(|(index, step)| match step {
                Request => vec![Message::user(REQUEST.into())],
                Compacted => vec![Message::synthetic(CONTINUE_AFTER_COMPACT.into())],
                Synthetic(text) => vec![Message::synthetic(text.into())],
                Call(name, input) => {
                    let id = format!("call_{index}");
                    vec![
                        Message {
                            role: Role::Assistant,
                            content: vec![ContentBlock::tool_use(&id, name, input)],
                            ..Default::default()
                        },
                        Message {
                            role: Role::User,
                            content: vec![ContentBlock::ToolResult {
                                tool_use_id: id,
                                content: RESULT.into(),
                                is_error: false,
                                output_ref: None,
                            }],
                            ..Default::default()
                        },
                    ]
                }
            })
            .collect()
    }

    fn plan() -> AgentMode {
        AgentMode::Plan(PathBuf::from(PLAN_PATH))
    }

    fn batch_of(tools: &[&str]) -> Value {
        json!({ "tool_calls": tools.iter().map(|tool| json!({ "tool": tool, "parameters": {} })).collect::<Vec<_>>() })
    }

    #[test_case(vec![Request, Compacted, Compacted], AgentMode::Build => 2 ; "counts_every_compaction_since_the_request")]
    #[test_case(vec![Compacted, Request, Compacted], AgentMode::Build => 1 ; "stops_at_the_latest_request")]
    #[test_case(vec![Request, Synthetic(POST_INSTRUCTIONS), Compacted], AgentMode::Build => 1 ; "other_synthetic_turns_are_not_compactions")]
    #[test_case(vec![Request, Call(FILE_READ_TOOL_NAME, json!({})), Compacted, Compacted], AgentMode::Build => 2 ; "reading_is_not_work")]
    #[test_case(vec![Request, Call(MEMORY_TOOL_NAME, json!({})), Compacted, Compacted], AgentMode::Build => 2 ; "saving_memory_is_not_work")]
    #[test_case(vec![Request, Call(FILE_EDIT_TOOL_NAME, json!({})), Compacted, Compacted], AgentMode::Build => 0 ; "an_edit_is_work")]
    #[test_case(vec![Request, Compacted, Call(FILE_APPLY_PATCH_TOOL_NAME, json!({})), Compacted], AgentMode::Build => 0 ; "work_anywhere_since_the_request_counts")]
    #[test_case(vec![Call(FILE_WRITE_TOOL_NAME, json!({})), Request, Compacted, Compacted], AgentMode::Build => 2 ; "work_before_the_request_does_not_count")]
    #[test_case(vec![Request, Call(BATCH_TOOL_NAME, batch_of(&[FILE_READ_TOOL_NAME, FILE_WRITE_TOOL_NAME])), Compacted, Compacted], AgentMode::Build => 0 ; "an_edit_inside_a_batch_is_work")]
    #[test_case(vec![Request, Call(BATCH_TOOL_NAME, batch_of(&[FILE_READ_TOOL_NAME])), Compacted, Compacted], AgentMode::Build => 2 ; "a_batch_of_reads_is_not_work")]
    #[test_case(vec![Request, Call(TASK_TOOL_NAME, json!({ "prompt": REQUEST })), Compacted, Compacted], AgentMode::Build => 0 ; "a_task_that_inherits_build_is_work")]
    #[test_case(vec![Request, Call(TASK_TOOL_NAME, json!({ "prompt": REQUEST, "mode": "plan" })), Compacted, Compacted], AgentMode::Build => 2 ; "a_planning_task_is_not_work")]
    #[test_case(vec![Request, Call(WORKFLOW_TOOL_NAME, json!({ "action": "start", "name": REQUEST })), Compacted, Compacted], AgentMode::Build => 0 ; "starting_a_workflow_is_work")]
    #[test_case(vec![Request, Call(WORKFLOW_TOOL_NAME, json!({ "action": "status" })), Compacted, Compacted], AgentMode::Build => 2 ; "checking_a_workflow_is_not_work")]
    #[test_case(vec![Request, Call(PLAN_TOOL_NAME, json!({ "action": "write", "content": REQUEST })), Compacted, Compacted], plan() => 0 ; "writing_the_plan_is_work")]
    #[test_case(vec![Request, Call(PLAN_TOOL_NAME, json!({ "action": "read" })), Compacted, Compacted], plan() => 2 ; "reading_the_plan_is_not_work")]
    #[test_case(vec![Request, Call(QUESTION_TOOL_NAME, json!({})), Compacted, Compacted], plan() => 0 ; "asking_while_planning_is_work")]
    #[test_case(vec![Request, Call(QUESTION_TOOL_NAME, json!({})), Compacted, Compacted], AgentMode::Build => 2 ; "asking_while_building_is_not_work")]
    #[test_case(vec![Request, Call(FILE_EDIT_TOOL_NAME, json!({})), Compacted, Compacted], plan() => 2 ; "an_edit_while_planning_is_not_work")]
    #[test_case(vec![Request, Call(TASK_TOOL_NAME, json!({ "prompt": REQUEST })), Compacted, Compacted], AgentMode::ReadOnly => 2 ; "a_read_only_agent_counts_compactions_alone")]
    fn stalled_compactions_since_the_request(steps: Vec<Step>, mode: AgentMode) -> u32 {
        stalled_compactions(&History::new(messages(steps)), &mode, None)
    }

    #[test]
    fn a_continuation_with_post_instructions_still_counts() {
        let continuation = format!("{CONTINUE_AFTER_COMPACT}\n\n{POST_INSTRUCTIONS}");
        let mut history = History::new(messages(vec![Request]));
        history.push(Message::synthetic(continuation));
        assert_eq!(stalled_compactions(&history, &AgentMode::Build, None), 1);
    }

    #[test_case(None => 2 ; "an_unknown_wire_name_is_not_work")]
    #[test_case(Some(FILE_EDIT_TOOL_NAME) => 0 ; "a_wire_name_resolves_to_its_tool")]
    fn batch_children_resolve_wire_names(canonical: Option<&str>) -> u32 {
        let aliases: Option<ToolNameAliases> = canonical
            .map(|canonical| Arc::new(HashMap::from([(WIRE_EDIT.into(), canonical.into())])));
        let history = History::new(messages(vec![
            Request,
            Call(BATCH_TOOL_NAME, batch_of(&[WIRE_EDIT])),
            Compacted,
            Compacted,
        ]));
        stalled_compactions(&history, &AgentMode::Build, aliases.as_ref())
    }

    #[test_case(vec![Request, Compacted] => 2 ; "compactions_before_the_seam_still_count")]
    #[test_case(vec![Request, Call(FILE_EDIT_TOOL_NAME, json!({})), Compacted] => 0 ; "work_before_the_seam_still_counts")]
    fn the_count_reads_across_compaction_seams(archived: Vec<Step>) -> u32 {
        let mut history = History::new(messages(archived));
        let superseded = history.item_at_message_boundary(history.len());
        history.replace_superseding(
            vec![Message::synthetic(CONTINUE_AFTER_COMPACT.into())],
            superseded,
        );
        stalled_compactions(&history, &AgentMode::Build, None)
    }

    #[test_case(AgentMode::Build, BUILD_HEADING ; "build_mode")]
    #[test_case(plan(), PLAN_HEADING ; "plan_mode")]
    #[test_case(AgentMode::ReadOnly, REPORT_HEADING ; "read_only_mode")]
    fn the_reminder_fits_the_mode(mode: AgentMode, heading: &str) {
        let history = History::new(messages(vec![Request, Compacted, Compacted]));
        let message = reminder(&history, &mode, None, THRESHOLD).expect(THRESHOLD_REACHED);
        let text = message.first_text_content().unwrap();
        assert!(text.contains(heading), "{WRONG_WORDING}");
        assert!(text.contains(&format!(": {THRESHOLD}.")), "{UNFILLED_SLOT}");
        assert!(!text.contains(COMPACTIONS_SLOT), "{UNFILLED_SLOT}");
        assert_eq!(message.display_text.as_deref(), Some(""), "{NOT_SYNTHETIC}");
    }

    #[test_case(1, THRESHOLD => false ; "below_the_threshold")]
    #[test_case(2, THRESHOLD => true ; "at_the_threshold")]
    #[test_case(3, THRESHOLD => true ; "past_the_threshold")]
    #[test_case(5, 0 => false ; "disabled")]
    fn the_reminder_waits_for_the_threshold(compactions: usize, threshold: u32) -> bool {
        let steps = std::iter::once(Request)
            .chain(std::iter::repeat_with(|| Compacted).take(compactions))
            .collect();
        let history = History::new(messages(steps));
        reminder(&history, &AgentMode::Build, None, threshold).is_some()
    }
}
