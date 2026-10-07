//! A firing opened as a trace: the event it handled, what it did in call
//! order with where each call stands now, how it failed, and what it changed.
//! A dry run opens the same way, its actions read by how each was answered.

use std::collections::{HashMap, HashSet};

use caudra_automation::host::ActionKind;
use caudra_automation::snapshot::{
    ActionBody, ActionRow, ActionStatus, DryRunDetail, FiringDetail, FiringSummary, StateOutcome,
    WaitReason,
};
use caudra_automation::untrusted::UNTRUSTED_TAG;
use caudra_workflow::RunSnapshot;
use ratatui::style::Style;
use ratatui::text::Span;
use serde_json::Value;

use super::dry_run::{self, DryRun};
use super::firings::{CONSUMED_BADGE, REPEATS_PREFIX, TRIGGER_INDEX, glyph, summary, timing};
use super::text::{
    EXPIRES_WAIT, action_text, clock, moment, outcome_text, span, trigger_text, wait_text,
};
use super::{Body, FoldScope, Item, LOADING, Loading};
use crate::components::escape_terminal_controls;
use crate::components::workflow_card::status_span;
use crate::theme;

const FIRING_LABEL: &str = "Firing ";
pub(super) const DRY_RUN_TITLE: &str = "Dry run of ";
const EVENT_HEADING: &str = "Event";
pub(super) const UNTRUSTED_LEGEND: &str =
    "Values under $untrusted came from outside the session, and are shown escaped";
const EVENT_CUT: &str = "The event was too large to store; this is a preview of its text";
const ACTIONS_HEADING: &str = "Actions";
const NO_ACTIONS: &str = "This firing performed no actions";
const LINE_PREFIX: &str = "L";
const NO_LINE: &str = "L?";
const REQUEST_HEADING: &str = "Request";
const RESULT_HEADING: &str = "Result";
const NO_RESULT: &str = "No result yet";
const REQUEST_CUT: &str = "The request was too large to store; this is a preview";
const RESULT_CUT: &str = "The result was too large to store; this is a preview";
const ERROR_HEADING: &str = "Error";
pub(super) const AT_LINE: &str = " at line ";
pub(super) const COLUMN_SEPARATOR: &str = ":";
pub(super) const SOURCE_RULE: &str = " \u{2502} ";
const STATE_HEADING: &str = "State change";
const PATCH_CUT: &str = "The patch was too large to store; this is a preview";
pub(super) const STATE_CONFLICT: &str = "Lost to a newer revision, so the change was not committed";
pub(super) const RUNNING: &str = "running";
pub(super) const DONE: &str = "done";
pub(super) const FAILED: &str = "failed";
pub(super) const REFUSED: &str = "refused";
pub(super) const QUEUED: &str = "queued";
pub(super) const DELIVERED: &str = "delivered ";
pub(super) const DEDUPLICATED: &str = "deduplicated into ";
pub(super) const DROPPED: &str = "dropped";
pub(super) const EXPIRED: &str = "expired";
pub(super) const INTERRUPTED: &str = "interrupted";
pub(super) const STARTED: &str = "started";
const COST_PREFIX: &str = "$";
const DETAIL_SEPARATOR: &str = ": ";
pub(super) const SEPARATOR: &str = " \u{b7} ";
const GAP: &str = " ";
pub(super) const MARKDOWN_TITLE: &str = "# Firing ";
pub(super) const MARKDOWN_DRY_RUN_TITLE: &str = "# Dry run of ";
const MARKDOWN_NOTE: &str = "- ";
const MARKDOWN_AUTOMATION: &str = "- Automation: ";
const MARKDOWN_TRIGGER: &str = "- Trigger: ";
const MARKDOWN_STATUS: &str = "- Status: ";
const MARKDOWN_QUEUED: &str = "- Queued: ";
const MARKDOWN_TIMING: &str = "- Time: ";
const MARKDOWN_CONSUMED: &str = "- Owns the message it handled";
const MARKDOWN_REPEATS: &str = "- Repeats: ";
const MARKDOWN_SUMMARY: &str = "- Summary: ";
const MARKDOWN_EVENT: &str = "## Event";
const MARKDOWN_ACTIONS: &str = "## Actions";
const MARKDOWN_ERROR: &str = "## Error";
const MARKDOWN_STATE: &str = "## State change";
const JSON_FENCE: &str = "```json";
const FENCE: &str = "```";

/// The firing whose trace is open, and the action bodies it asked for.
pub(super) struct Trace {
    pub(super) fire_id: String,
    pub(super) detail: Loading<Box<FiringDetail>>,
    pub(super) open_action: Option<u64>,
    pub(super) bodies: HashMap<u64, Loading<Box<ActionBody>>>,
}

impl Trace {
    pub(super) fn new(fire_id: String) -> Self {
        Self {
            fire_id,
            detail: Loading::Requested,
            open_action: None,
            bodies: HashMap::new(),
        }
    }

    pub(super) fn loaded(&self) -> Option<&FiringDetail> {
        match &self.detail {
            Loading::Loaded(detail) => Some(detail),
            Loading::Requested | Loading::Failed(_) => None,
        }
    }

    pub(super) fn action(&self, seq: u64) -> Option<&ActionRow> {
        action_of(self.loaded()?, seq)
    }
}

/// A trace open in place of the Firings list: a firing's, with the request
/// and result a reader opened, or a dry run's.
#[derive(Clone, Copy)]
pub(super) enum Shown<'a> {
    Firing(&'a Trace),
    DryRun(&'a DryRun),
}

impl<'a> Shown<'a> {
    /// What it shows, once that has landed.
    pub(super) fn loaded(self) -> Option<&'a FiringDetail> {
        match self {
            Self::Firing(trace) => trace.loaded(),
            Self::DryRun(dry_run) => dry_run.loaded().map(|replay| &replay.trace),
        }
    }
}

pub(super) fn action_of(detail: &FiringDetail, seq: u64) -> Option<&ActionRow> {
    detail.actions.iter().find(|action| action.seq == seq)
}

/// The run a `start_workflow` action started, once it answered.
pub(super) fn started_run(action: &ActionRow) -> Option<&str> {
    match (action.kind, action.status) {
        (ActionKind::StartWorkflow, ActionStatus::Done) => action.target.as_deref(),
        _ => None,
    }
}

/// `run_id` in this session's workflow mirror, which holds no other session's runs and no
/// dry run's.
pub(super) fn mirrored<'a>(runs: &'a [RunSnapshot], run_id: &str) -> Option<&'a RunSnapshot> {
    runs.iter().find(|run| run.run_id == run_id)
}

/// `runs` is the session's workflow mirror, read as the trace draws so a started run shows
/// where it stands now.
pub(super) fn lines(
    body: &mut Body,
    shown: Shown<'_>,
    folded: &HashMap<FoldScope, HashSet<usize>>,
    now: i64,
    runs: &[RunSnapshot],
) {
    let t = theme::current();
    let (detail, dry_run): (&FiringDetail, _) = match shown {
        Shown::Firing(trace) => {
            body.text(format!("{FIRING_LABEL}{}", trace.fire_id), t.bold);
            let Some(detail) = body.landed(Some(&trace.detail)) else {
                return;
            };
            header(body, &detail.firing, timing(&detail.firing, now));
            (detail, None)
        }
        Shown::DryRun(dry_run) => {
            body.text(format!("{DRY_RUN_TITLE}{}", dry_run.fire_id), t.bold);
            let Some(replay) = body.landed(Some(&dry_run.result)) else {
                return;
            };
            header(body, &replay.trace.firing, None);
            for (note, style) in dry_run::notes(dry_run, replay, now) {
                body.text(note, style);
            }
            (&replay.trace, Some(dry_run))
        }
    };
    body.heading(EVENT_HEADING);
    if holds_untrusted(&detail.event) {
        body.text(UNTRUSTED_LEGEND, t.tool_warning);
    }
    body.tree(
        FoldScope::Event,
        &detail.event,
        folded.get(&FoldScope::Event),
    );
    if detail.event_cut {
        body.text(EVENT_CUT, t.tool_warning);
    }
    body.heading(format!("{ACTIONS_HEADING} ({})", detail.actions.len()));
    if detail.actions.is_empty() {
        body.text(NO_ACTIONS, t.tool_dim);
    }
    for (index, action) in detail.actions.iter().enumerate() {
        body.item(
            Item::Action(action.seq),
            action_row(action, states(action, index, dry_run, now, runs)),
        );
        if let Shown::Firing(trace) = shown
            && trace.open_action == Some(action.seq)
        {
            action_body(body, trace.bodies.get(&action.seq), action.seq, folded);
        }
    }
    if let Some(error) = &detail.firing.error {
        body.heading(ERROR_HEADING);
        let mut text = format!("{}{DETAIL_SEPARATOR}{}", error.kind, error.message);
        if let Some(line) = error.line {
            text.push_str(&format!("{AT_LINE}{line}"));
            if let Some(column) = error.column {
                text.push_str(&format!("{COLUMN_SEPARATOR}{column}"));
            }
        }
        body.text(escape_terminal_controls(&text), t.tool_error);
        if let Some(source) = &detail.error_source {
            let number = error.line.map(|line| line.to_string()).unwrap_or_default();
            body.line(vec![
                Span::styled(format!("{number}{SOURCE_RULE}"), t.tool_dim),
                Span::styled(escape_terminal_controls(source), t.tool),
            ]);
        }
    }
    if let Some(patch) = &detail.state_patch {
        body.heading(STATE_HEADING);
        if detail.firing.state_outcome == Some(StateOutcome::Conflict) {
            body.text(STATE_CONFLICT, t.tool_warning);
        }
        body.tree(FoldScope::Patch, patch, folded.get(&FoldScope::Patch));
        if detail.patch_cut {
            body.text(PATCH_CUT, t.tool_warning);
        }
    }
}

/// `timing` is how long the firing took or waits. A dry run has none to
/// tell: its times are pinned to the firing it replays.
fn header(body: &mut Body, firing: &FiringSummary, timing: Option<String>) {
    let t = theme::current();
    let (glyph, glyph_style) = glyph(firing.status);
    let mut spans = vec![
        Span::styled(escape_terminal_controls(&firing.automation), t.tool_path),
        Span::styled(
            format!(
                "{SEPARATOR}{}{TRIGGER_INDEX}{}{SEPARATOR}",
                trigger_text(firing.trigger),
                firing.trigger_index
            ),
            t.tool,
        ),
        Span::styled(format!("{glyph} {}", firing.status), glyph_style),
    ];
    if let Some(timing) = timing {
        spans.push(Span::styled(format!("{SEPARATOR}{timing}"), t.tool_dim));
    }
    if firing.consumed {
        spans.push(Span::styled(CONSUMED_BADGE, t.accent));
    }
    if firing.repeats > 1 {
        spans.push(Span::styled(
            format!("{REPEATS_PREFIX}{}", firing.repeats),
            t.tool_dim,
        ));
    }
    body.line(spans);
    body.text(escape_terminal_controls(&summary(firing)), t.tool_dim);
}

fn holds_untrusted(value: &Value) -> bool {
    match value {
        Value::Object(entries) => entries
            .iter()
            .any(|(key, value)| key == UNTRUSTED_TAG || holds_untrusted(value)),
        Value::Array(items) => items.iter().any(holds_untrusted),
        _ => false,
    }
}

fn action_row(action: &ActionRow, states: Vec<(String, Style)>) -> Vec<Span<'static>> {
    let t = theme::current();
    let mut spans = vec![
        Span::styled(position(action), t.tool_dim),
        Span::raw(GAP),
        Span::styled(action_text(action.kind), t.tool),
        Span::styled(
            format!("{SEPARATOR}{}", escape_terminal_controls(&action.summary)),
            t.tool_dim,
        ),
    ];
    spans.extend(
        states
            .into_iter()
            .map(|(state, style)| Span::styled(format!("{SEPARATOR}{state}"), style)),
    );
    spans
}

fn position(action: &ActionRow) -> String {
    action
        .line
        .map_or_else(|| NO_LINE.to_owned(), |line| format!("{LINE_PREFIX}{line}"))
}

/// Where an action of a firing stands now; how a dry run's was answered,
/// after the failure the journal answered it with. Every other action of a
/// dry run is done the moment it is asked, so saying so would add nothing.
fn states(
    action: &ActionRow,
    index: usize,
    dry_run: Option<&DryRun>,
    now: i64,
    runs: &[RunSnapshot],
) -> Vec<(String, Style)> {
    let Some(dry_run) = dry_run else {
        return vec![action_state(action, now, runs)];
    };
    let mut states = Vec::new();
    if action.status != ActionStatus::Done {
        states.push(action_state(action, now, runs));
    }
    if let Some(answer) = dry_run.answer(index) {
        let (badge, style) = dry_run::answer_badge(answer);
        states.push((badge.to_owned(), style));
    }
    states
}

/// Where an action stands, in words: a queued delivery says what holds it,
/// a delivered one what its turn led to and cost, and a started run its name
/// and status in `runs`, else its id.
pub(super) fn action_state(action: &ActionRow, now: i64, runs: &[RunSnapshot]) -> (String, Style) {
    let t = theme::current();
    if let Some(run_id) = started_run(action) {
        return match mirrored(runs, run_id) {
            Some(run) => {
                let status = status_span(run.status);
                (
                    format!(
                        "{}{SEPARATOR}{}",
                        escape_terminal_controls(&run.display_name),
                        status.content
                    ),
                    status.style,
                )
            }
            None => (
                format!("{STARTED}{SEPARATOR}{}", escape_terminal_controls(run_id)),
                t.tool_success,
            ),
        };
    }
    let with_error = |word: &str| match &action.error {
        Some(error) => format!(
            "{word}{DETAIL_SEPARATOR}{}",
            escape_terminal_controls(error)
        ),
        None => word.to_owned(),
    };
    match action.status {
        ActionStatus::Running => (RUNNING.to_owned(), t.todo_in_progress),
        ActionStatus::Done => {
            let took = action
                .finished_at
                .map(|finished| {
                    format!("{GAP}{}", span(finished.saturating_sub(action.started_at)))
                })
                .unwrap_or_default();
            (format!("{DONE}{took}"), t.tool_success)
        }
        ActionStatus::Failed => (with_error(FAILED), t.tool_error),
        ActionStatus::Refused => (with_error(REFUSED), t.tool_warning),
        ActionStatus::Queued => {
            let mut text = match action.wait {
                Some(reason) => format!("{QUEUED}{DETAIL_SEPARATOR}{}", wait_text(reason, now)),
                None => QUEUED.to_owned(),
            };
            if let Some(at) = action.expires_at
                && !matches!(action.wait, Some(WaitReason::ExpiresAt { .. }))
            {
                text.push_str(&format!("{SEPARATOR}{EXPIRES_WAIT}{}", moment(at, now)));
            }
            (text, t.tool_warning)
        }
        ActionStatus::Delivered => {
            let mut text = format!(
                "{DELIVERED}{}",
                action.delivered_at.map(clock).unwrap_or_default()
            );
            if let Some(outcome) = action.turn_outcome {
                text.push_str(&format!("{SEPARATOR}{}", outcome_text(outcome)));
            }
            if let Some(cost) = action.turn_cost {
                text.push_str(&format!("{SEPARATOR}{COST_PREFIX}{cost:.4}"));
            }
            (text, t.tool_success)
        }
        ActionStatus::Deduplicated => (
            format!(
                "{DEDUPLICATED}{}",
                escape_terminal_controls(action.target.as_deref().unwrap_or_default())
            ),
            t.tool_dim,
        ),
        ActionStatus::Dropped => (with_error(DROPPED), t.tool_dim),
        ActionStatus::Expired => (EXPIRED.to_owned(), t.tool_dim),
        ActionStatus::Interrupted => (INTERRUPTED.to_owned(), t.tool_warning),
    }
}

fn action_body(
    body: &mut Body,
    loaded: Option<&Loading<Box<ActionBody>>>,
    seq: u64,
    folded: &HashMap<FoldScope, HashSet<usize>>,
) {
    let t = theme::current();
    let note = |body: &mut Body, text: &str, style: Style| {
        body.under(vec![Span::styled(text.to_owned(), style)]);
    };
    let action = match loaded {
        Some(Loading::Loaded(action)) => action,
        Some(Loading::Failed(error)) => {
            note(body, &escape_terminal_controls(error), t.tool_error);
            return;
        }
        Some(Loading::Requested) | None => {
            note(body, LOADING, t.tool_dim);
            return;
        }
    };
    note(body, REQUEST_HEADING, t.keybind_section);
    body.tree(
        FoldScope::Request(seq),
        &action.request,
        folded.get(&FoldScope::Request(seq)),
    );
    if action.request_cut {
        note(body, REQUEST_CUT, t.tool_warning);
    }
    note(body, RESULT_HEADING, t.keybind_section);
    match &action.result {
        Some(result) => body.tree(
            FoldScope::Result(seq),
            result,
            folded.get(&FoldScope::Result(seq)),
        ),
        None => note(body, NO_RESULT, t.tool_dim),
    }
    if action.result_cut {
        note(body, RESULT_CUT, t.tool_warning);
    }
}

/// The firing as Markdown, with its event, actions, error and state change
/// once its trace has loaded.
pub(super) fn markdown(
    firing: &FiringSummary,
    detail: Option<&FiringDetail>,
    now: i64,
    runs: &[RunSnapshot],
) -> String {
    let mut out = markdown_head(format!("{MARKDOWN_TITLE}{}", firing.fire_id), firing);
    out.push(format!("{MARKDOWN_QUEUED}{}", clock(firing.queued_at)));
    if let Some(timing) = timing(firing, now) {
        out.push(format!("{MARKDOWN_TIMING}{timing}"));
    }
    if firing.consumed {
        out.push(MARKDOWN_CONSUMED.to_owned());
    }
    if firing.repeats > 1 {
        out.push(format!("{MARKDOWN_REPEATS}{}", firing.repeats));
    }
    out.push(format!("{MARKDOWN_SUMMARY}{}", summary(firing)));
    if let Some(detail) = detail {
        markdown_sections(&mut out, detail, None, now, runs);
    }
    out.join("\n")
}

/// A dry run that landed as Markdown, with its notes, and how each of its
/// actions was answered.
pub(super) fn dry_run_markdown(
    dry_run: &DryRun,
    replay: &DryRunDetail,
    now: i64,
    runs: &[RunSnapshot],
) -> String {
    let firing = &replay.trace.firing;
    let mut out = markdown_head(
        format!("{MARKDOWN_DRY_RUN_TITLE}{}", dry_run.fire_id),
        firing,
    );
    out.push(format!("{MARKDOWN_SUMMARY}{}", summary(firing)));
    out.extend(
        dry_run::notes(dry_run, replay, now)
            .into_iter()
            .map(|(note, _)| format!("{MARKDOWN_NOTE}{note}")),
    );
    markdown_sections(&mut out, &replay.trace, Some(dry_run), now, runs);
    out.join("\n")
}

/// `title`, then the automation, the trigger and how it ended.
fn markdown_head(title: String, firing: &FiringSummary) -> Vec<String> {
    vec![
        title,
        String::new(),
        format!("{MARKDOWN_AUTOMATION}{}", firing.automation),
        format!(
            "{MARKDOWN_TRIGGER}{}{TRIGGER_INDEX}{}",
            trigger_text(firing.trigger),
            firing.trigger_index
        ),
        format!("{MARKDOWN_STATUS}{}", firing.status),
    ]
}

/// The event, the actions, the error and the state change, as [`lines`]
/// draws them.
fn markdown_sections(
    out: &mut Vec<String>,
    detail: &FiringDetail,
    dry_run: Option<&DryRun>,
    now: i64,
    runs: &[RunSnapshot],
) {
    out.extend([String::new(), MARKDOWN_EVENT.to_owned(), String::new()]);
    out.extend(json_block(&detail.event));
    out.extend([String::new(), MARKDOWN_ACTIONS.to_owned(), String::new()]);
    for (index, action) in detail.actions.iter().enumerate() {
        let states: Vec<String> = states(action, index, dry_run, now, runs)
            .into_iter()
            .map(|(state, _)| state)
            .collect();
        out.push(format!(
            "{}. `{}` {}{DETAIL_SEPARATOR}{} ({})",
            index + 1,
            position(action),
            action_text(action.kind),
            action.summary,
            states.join(SEPARATOR)
        ));
    }
    if let Some(error) = &detail.firing.error {
        out.extend([String::new(), MARKDOWN_ERROR.to_owned(), String::new()]);
        let mut text = format!("{}{DETAIL_SEPARATOR}{}", error.kind, error.message);
        if let Some(line) = error.line {
            text.push_str(&format!("{AT_LINE}{line}"));
        }
        out.push(text);
        if let Some(source) = &detail.error_source {
            out.extend([
                String::new(),
                FENCE.to_owned(),
                source.clone(),
                FENCE.to_owned(),
            ]);
        }
    }
    if let Some(patch) = &detail.state_patch {
        out.extend([String::new(), MARKDOWN_STATE.to_owned(), String::new()]);
        out.extend(json_block(patch));
    }
}

fn json_block(value: &Value) -> [String; 3] {
    [
        JSON_FENCE.to_owned(),
        serde_json::to_string_pretty(value).unwrap_or_default(),
        FENCE.to_owned(),
    ]
}
