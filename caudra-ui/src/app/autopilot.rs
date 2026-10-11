//! The App's half of autopilot: the session's handle, `/autopilot`, the spend
//! it counts, the implement handoff, the resume check-in and the stops.

use std::mem;
use std::sync::Arc;
use std::time::{Duration, Instant};

use caudra_agent::AgentMode;
use caudra_agent::automation::clock::SystemClock;
use caudra_agent::autopilot::{ActionName, AutopilotDecision, AutopilotHandle, DecisionKind};
use caudra_agent::permissions::AutopilotNote;
use caudra_agent::types::AutopilotEvent;
use caudra_automation::event::StartedBy;
use caudra_config::{AutopilotConfig, Feature};
use caudra_providers::model_registry;
use caudra_providers::{
    AutopilotMessageKind, AutopilotOrigin, Billing, Message, ModelPurpose, TokenUsage,
};
use caudra_storage::autopilot::{
    AutopilotEnd, AutopilotStopper, StoredAutopilot, StoredAutopilotDecision,
};
use caudra_storage::sessions::SessionDatabase;
use caudra_storage::usage_ledger::LedgerPurpose;
use ratatui::Frame;
use ratatui::layout::Rect;
use tracing::warn;

use super::{
    App, CONFIRMATION_DURATION, FLASH_CANCEL, IMPLEMENT_CAPTURE_FAILED, MODEL_ARG, Notification,
    Status, is_goal_clear,
};
use crate::components::autopilot_panel::{
    PanelAction, PanelContent, budget_left, decision_text, end_notice, end_reason, goal_line,
    state_label,
};
use crate::components::messages::markdown_code_span;
use crate::components::status_bar::AutopilotChip;
use crate::components::{Action, DisplayMessage, Overlay, format_elapsed};

const GOAL_COMMAND: &str = "/goal";
const AUTOPILOT_COMMAND: &str = "/autopilot";
const STOP_ARGS: [&str; 2] = ["off", "stop"];
const MS_PER_MINUTE: u64 = 60_000;
/// How far the stored engagement clock may trail the live one, so time
/// passing alone does not rewrite an engaged session every frame.
const ELAPSED_SAVE_STEP_MS: u64 = MS_PER_MINUTE;
pub(crate) const NOT_ENGAGED_MSG: &str = "Autopilot is not engaged";
pub(crate) const PLAN_LEFT_TO_YOU: &str = "Autopilot left the plan to you: queued input goes first";
pub(crate) const ENGAGED_ROW: &str = "engaged";
pub(crate) const ENDED_ROW: &str = "ended";
const RETRYING_ROW: &str = "retrying";
const SUMMARY_LINE_BREAK: &str = "\n";
/// Autopilot retries the main agent's runs alone.
const MAIN_CHAT: usize = 0;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum AutopilotCommand<'a> {
    Engage,
    Goal(&'a str),
    Stop,
    Model,
}

pub(super) fn parse_autopilot_command(args: &str) -> AutopilotCommand<'_> {
    let args = args.trim();
    if args.is_empty() {
        AutopilotCommand::Engage
    } else if args.eq_ignore_ascii_case(MODEL_ARG) {
        AutopilotCommand::Model
    } else if STOP_ARGS.iter().any(|stop| args.eq_ignore_ascii_case(stop)) {
        AutopilotCommand::Stop
    } else {
        AutopilotCommand::Goal(args)
    }
}

/// The prompt a launch with `--autopilot` submits: a `/goal <condition>`
/// prompt becomes `/autopilot <condition>`, so autopilot owns that goal.
pub(crate) fn launch_prompt(prompt: String) -> String {
    let condition = prompt
        .trim()
        .strip_prefix(GOAL_COMMAND)
        .filter(|args| args.starts_with(char::is_whitespace))
        .map(str::trim)
        .filter(|condition| {
            !condition.eq_ignore_ascii_case(MODEL_ARG) && !is_goal_clear(condition)
        });
    match condition {
        Some(condition) if !condition.is_empty() => format!("{AUTOPILOT_COMMAND} {condition}"),
        _ => prompt,
    }
}

/// A decision's transcript row, its subject quoted so the markdown row
/// draws it as written.
pub(crate) fn decision_row(kind: &str, action: &str, subject: &str, reason: &str) -> String {
    let subject = if subject.is_empty() {
        String::new()
    } else {
        markdown_code_span(subject)
    };
    decision_text(kind, action, &subject, reason)
}

fn retrying_row(attempt: u32, limit: u32, delay_ms: u64, error: &str) -> String {
    let delay = format_elapsed(Duration::from_millis(delay_ms).as_secs());
    format!("{RETRYING_ROW} {attempt}/{limit} in {delay}: {error}")
}

/// What a card's note says autopilot left to the user, if anything.
fn left_to_you(note: Option<&AutopilotNote>) -> Option<&str> {
    match note? {
        AutopilotNote::Escalated { reason } => Some(reason),
        AutopilotNote::Deciding => None,
    }
}

impl App {
    /// The handle the session's agents and root permission manager get,
    /// `None` while the experiment is off.
    pub(crate) fn attached_autopilot(&self) -> Option<AutopilotHandle> {
        self.features
            .enabled(Feature::Autopilot)
            .then(|| self.state.autopilot.clone())
    }

    /// Opens the session's own engagement under `limits`, still engaged if it
    /// was stored engaged, and hands it to the root permission manager the
    /// session's agents read it from. An engaged session with a goal owes the
    /// goal one check-in once it settles.
    pub(crate) fn restore_autopilot(&mut self, limits: AutopilotConfig) {
        let restored = (self.features.enabled(Feature::Autopilot)
            && self.state.session.meta.autopilot.is_some())
        .then(|| {
            AutopilotHandle::from_meta(
                &self.state.session.meta,
                self.stored_autopilot_decisions(),
                Arc::new(SystemClock::new()),
            )
        });
        self.state.autopilot = restored.unwrap_or_default();
        self.state.autopilot.set_limits(limits);
        self.permissions.set_autopilot(self.attached_autopilot());
        self.autopilot_checkin_due =
            self.state.autopilot.is_engaged() && self.state.goal.snapshot().is_some();
    }

    /// The stored decisions' transcript rows, each with the history length it
    /// was made at, so a restore places it among the turns. None while the
    /// experiment is off.
    pub(super) fn autopilot_decision_rows(&self) -> Vec<(usize, DisplayMessage)> {
        if !self.features.enabled(Feature::Autopilot) {
            return Vec::new();
        }
        self.stored_autopilot_decisions()
            .into_iter()
            .map(|decision| {
                let row = decision_row(
                    &decision.kind,
                    &decision.action,
                    &decision.subject,
                    &decision.reason,
                );
                (decision.history_len, DisplayMessage::autopilot(row))
            })
            .collect()
    }

    fn stored_autopilot_decisions(&self) -> Vec<StoredAutopilotDecision> {
        let session_id = self.state.session.id;
        SessionDatabase::open(&self.storage)
            .and_then(|database| database.load_autopilot_decisions(session_id))
            .unwrap_or_else(|error| {
                warn!(%error, %session_id, "autopilot decisions unreadable; resuming without them");
                Vec::new()
            })
    }

    /// What `SessionMeta.autopilot` keeps. With the experiment off the handle
    /// was never restored, so the stored engagement stays as it was.
    pub(super) fn stored_autopilot(&self) -> Option<Box<StoredAutopilot>> {
        let saved = self.state.session.meta.autopilot.as_deref();
        if !self.features.enabled(Feature::Autopilot) {
            return saved.cloned().map(Box::new);
        }
        let mut stored = self.state.autopilot.stored()?;
        if let Some(saved) = saved
            && stored.elapsed_ms.abs_diff(saved.elapsed_ms) < ELAPSED_SAVE_STEP_MS
        {
            stored.elapsed_ms = saved.elapsed_ms;
        }
        Some(Box::new(stored))
    }

    pub(super) fn execute_autopilot(&mut self, args: &str) -> Vec<Action> {
        match parse_autopilot_command(args) {
            AutopilotCommand::Engage if self.state.autopilot.is_engaged() => {
                self.clear_control_hovers();
                self.autopilot_panel.open();
                Vec::new()
            }
            AutopilotCommand::Engage => {
                self.engage_autopilot(false);
                Vec::new()
            }
            AutopilotCommand::Goal(condition) => {
                let actions = self.submit_goal(condition);
                let owns_goal = self.state.goal.active_condition().as_deref() == Some(condition);
                self.engage_autopilot(owns_goal);
                actions
            }
            AutopilotCommand::Stop => {
                self.stop_autopilot_by_command();
                Vec::new()
            }
            AutopilotCommand::Model => self.open_autopilot_model_picker(),
        }
    }

    fn stop_autopilot_by_command(&mut self) {
        if !self.end_autopilot(AutopilotEnd::Stopped {
            by: AutopilotStopper::Command,
        }) {
            self.flash(NOT_ENGAGED_MSG.into());
        }
    }

    fn open_autopilot_model_picker(&mut self) -> Vec<Action> {
        self.model_picker.open_purpose(
            &self.state.model,
            &self.model_policy,
            ModelPurpose::Autopilot,
        );
        vec![Action::RefreshModels]
    }

    /// The panel closes before acting, so the picker it opens is not drawn
    /// under it.
    pub(super) fn on_autopilot_panel(&mut self, action: PanelAction) -> Vec<Action> {
        self.autopilot_panel.close();
        match action {
            PanelAction::Stop => {
                self.stop_autopilot_by_command();
                Vec::new()
            }
            PanelAction::Model => self.open_autopilot_model_picker(),
        }
    }

    pub(super) fn draw_autopilot_panel(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        let Some(stored) = self.state.autopilot.stored() else {
            self.autopilot_panel.close();
            return Rect::default();
        };
        let limits = self.state.autopilot.limits();
        let goal = self.state.goal.active_condition();
        let model = model_registry::binding(ModelPurpose::Autopilot);
        let escalations: Vec<String> = self
            .autopilot_escalations()
            .map(|(kind, reason)| format!("{}: {reason}", kind.name()))
            .collect();
        let decisions = self.state.autopilot.recent_decisions();
        let content = PanelContent {
            stored: &stored,
            limits: &limits,
            goal: goal.as_deref(),
            model: model.as_ref(),
            deciding: self.autopilot_is_deciding(),
            escalations: &escalations,
            decisions: &decisions,
        };
        self.autopilot_panel.view(frame, area, &content)
    }

    /// The footer chip, present while autopilot is engaged with the
    /// experiment on.
    pub(super) fn autopilot_chip(&self) -> Option<AutopilotChip> {
        if !self.features.enabled(Feature::Autopilot) {
            return None;
        }
        let stored = self
            .state
            .autopilot
            .stored()
            .filter(|stored| stored.engaged)?;
        let limits = self.state.autopilot.limits();
        let escalated = self.autopilot_escalations().next().is_some();
        let summary = [
            state_label(&stored, self.autopilot_is_deciding(), escalated),
            goal_line(
                self.state.goal.active_condition().as_deref(),
                stored.owns_goal,
            ),
            budget_left(&stored, &limits),
        ]
        .join(SUMMARY_LINE_BREAK);
        Some(AutopilotChip {
            used: stored.continuations,
            limit: limits.max_continuations,
            escalated,
            summary,
        })
    }

    /// Whether autopilot is making a decision the user could be waiting on.
    fn autopilot_is_deciding(&self) -> bool {
        !self.autopilot_deciding.is_empty()
            || self.permission_prompt.autopilot_note() == Some(&AutopilotNote::Deciding)
    }

    /// The cards autopilot left to the user, with why.
    fn autopilot_escalations(&self) -> impl Iterator<Item = (DecisionKind, &str)> {
        let plan = self
            .plan_form_active()
            .then(|| self.plan_form.autopilot_note())
            .flatten();
        [
            (
                DecisionKind::Permission,
                self.permission_prompt.autopilot_note(),
            ),
            (DecisionKind::Question, self.question_form.autopilot_note()),
            (DecisionKind::Plan, plan),
        ]
        .into_iter()
        .filter_map(|(kind, note)| left_to_you(note).map(|reason| (kind, reason)))
    }

    /// The first card autopilot left to the user, as what the terminal says.
    pub(super) fn autopilot_wait(&self) -> Option<Notification> {
        self.autopilot_escalations()
            .next()
            .map(|(wait, _)| Notification::AutopilotNeedsYou { wait })
    }

    pub(crate) fn take_autopilot_notice(&mut self) -> Option<Notification> {
        self.autopilot_notice.take()
    }

    /// The first touch on a permission card autopilot is deciding takes it
    /// from autopilot and does nothing else: the card may change once
    /// autopilot lets go, and the touch was made without reading it.
    pub(super) fn claim_deciding_permission(&mut self) -> bool {
        let Some(request_id) = self.permission_prompt.take_deciding() else {
            return false;
        };
        let manager = self
            .sandbox_live
            .transfer
            .as_ref()
            .map(|worker| &worker.permissions)
            .filter(|manager| manager.pending_request(&request_id).is_some())
            .unwrap_or(&self.permissions);
        manager.claim(&request_id);
        true
    }

    /// The first touch on a question form autopilot is deciding takes it,
    /// and keeps its own effect: the form does not change under it.
    pub(super) fn claim_question_form(&mut self) {
        if self.question_form.autopilot_note() == Some(&AutopilotNote::Deciding)
            && self.state.autopilot.claim_question()
        {
            self.question_form.set_autopilot_note(None);
            self.autopilot_deciding
                .retain(|(_, kind)| *kind != DecisionKind::Question);
        }
    }

    /// As [`Self::claim_question_form`], for the plan card.
    pub(super) fn claim_plan_card(&mut self) {
        if self.plan_form.autopilot_note() == Some(&AutopilotNote::Deciding)
            && self.state.autopilot.claim_plan()
        {
            self.plan_form.set_autopilot_note(None);
            self.autopilot_deciding
                .retain(|(_, kind)| *kind != DecisionKind::Plan);
        }
    }

    /// The plan card takes Esc while it is up, so hiding it while an engaged
    /// autopilot's run streams counts as the first Esc of Esc Esc.
    pub(super) fn arm_autopilot_stop(&mut self) {
        if self.status == Status::Streaming && self.state.autopilot.is_engaged() {
            self.last_esc = Some(Instant::now());
            self.status_bar
                .flash_for(FLASH_CANCEL.into(), CONFIRMATION_DURATION);
        }
    }

    /// A decision the run's end cut off is never made, so its card stops
    /// saying autopilot is deciding it.
    pub(super) fn autopilot_run_ended(&mut self) {
        for (_, kind) in mem::take(&mut self.autopilot_deciding) {
            match kind {
                DecisionKind::Question => self.question_form.set_autopilot_note(None),
                DecisionKind::Plan => self.plan_form.set_autopilot_note(None),
                _ => {}
            }
        }
    }

    /// Engages with a fresh budget, owning the active goal when autopilot set
    /// it. Already engaged, it only takes ownership of such a goal.
    pub(crate) fn engage_autopilot(&mut self, owns_goal: bool) {
        if let Some(engaged) = self.state.autopilot.engage(owns_goal) {
            self.note_autopilot_event(engaged);
        }
    }

    /// Whether there was an engagement to end.
    fn end_autopilot(&mut self, reason: AutopilotEnd) -> bool {
        let ended = self.state.autopilot.end(reason);
        let was_engaged = ended.is_some();
        if let Some(ended) = ended {
            self.note_autopilot_event(ended);
        }
        was_engaged
    }

    /// Esc Esc, Ctrl-C while streaming and the question form's Cancel: the
    /// user stopped the run, so automations hold and autopilot disengages.
    pub(super) fn user_stopped(&mut self, by: AutopilotStopper) {
        self.pause_automations();
        self.end_autopilot(AutopilotEnd::Stopped { by });
    }

    /// Spend outside the main agent's own turns, which the goal and an
    /// engaged autopilot both count. The delegate's spend is counted where it
    /// is made.
    pub(super) fn record_external_spend(
        &mut self,
        usage: TokenUsage,
        cost: Option<f64>,
        billing: Billing,
        purpose: LedgerPurpose,
    ) {
        self.state.goal.record_external_usage(usage, cost, billing);
        if purpose == LedgerPurpose::Autopilot {
            return;
        }
        let autopilot = &self.state.autopilot;
        if let Some(ended) = autopilot.record_spend(cost, billing, &autopilot.limits()) {
            self.note_autopilot_event(ended);
        }
    }

    /// Counts a turn autopilot starts without a human. A budget that runs out
    /// ends autopilot, never the turn.
    pub(super) fn count_autopilot_continuation(&mut self) {
        let autopilot = &self.state.autopilot;
        if let Err(ended) = autopilot.try_continue(&autopilot.limits()) {
            self.note_autopilot_event(ended);
        }
    }

    /// An event from the agent path, sent under `run_id`.
    pub(super) fn on_autopilot_event(&mut self, event: AutopilotEvent, run_id: u64) {
        match event {
            AutopilotEvent::ImplementPlan { seq } => self.autopilot_implement = Some((run_id, seq)),
            event => self.note_autopilot_event(event),
        }
    }

    /// Where every engagement change and decision lands, from the agent or
    /// from the App itself.
    fn note_autopilot_event(&mut self, event: AutopilotEvent) {
        match event {
            AutopilotEvent::Engaged => self.push_autopilot_row(ENGAGED_ROW.to_owned()),
            AutopilotEvent::Decided(decision) => self.log_autopilot_decision(*decision),
            AutopilotEvent::Ended { reason } => {
                self.push_autopilot_row(format!("{ENDED_ROW}: {}", end_reason(&reason)));
                self.autopilot_notice = Some(Notification::AutopilotEnded {
                    reason: end_notice(&reason),
                });
            }
            AutopilotEvent::Retrying {
                attempt,
                limit,
                delay_ms,
                error,
            } => {
                self.wait_out_retry(MAIN_CHAT, attempt, error.clone(), delay_ms);
                self.push_autopilot_row(retrying_row(attempt, limit, delay_ms, &error));
            }
            AutopilotEvent::Deciding { seq, kind, .. } => self.autopilot_decides(seq, kind),
            AutopilotEvent::ImplementPlan { .. } => {}
        }
    }

    /// A decision autopilot started. A permission card carries its own note.
    fn autopilot_decides(&mut self, seq: u32, kind: DecisionKind) {
        let deciding = Some(AutopilotNote::Deciding);
        match kind {
            DecisionKind::Permission => return,
            DecisionKind::Question => self.question_form.set_autopilot_note(deciding),
            DecisionKind::Plan => self.plan_form.set_autopilot_note(deciding),
            DecisionKind::Reply | DecisionKind::Review | DecisionKind::SecondOpinion => {}
        }
        self.autopilot_deciding.push((seq, kind));
    }

    /// The card a decision was about, unless the user took it first: an
    /// answered form closes, and an escalation stays up saying why.
    fn settle_autopilot_card(&mut self, decision: &AutopilotDecision) {
        let Some(index) = self
            .autopilot_deciding
            .iter()
            .position(|(seq, _)| *seq == decision.seq)
        else {
            return;
        };
        self.autopilot_deciding.remove(index);
        let left_to_you =
            (decision.action == ActionName::Escalate).then(|| AutopilotNote::Escalated {
                reason: decision.reason.clone(),
            });
        match decision.kind {
            DecisionKind::Question if left_to_you.is_none() => {
                self.discard_question_btw();
                self.question_form.close();
                self.question_subagent = None;
                self.task_interactions.question = None;
            }
            DecisionKind::Question => self.question_form.set_autopilot_note(left_to_you),
            DecisionKind::Plan => self.plan_form.set_autopilot_note(left_to_you),
            DecisionKind::Permission
            | DecisionKind::Reply
            | DecisionKind::Review
            | DecisionKind::SecondOpinion => {}
        }
    }

    fn push_autopilot_row(&mut self, text: String) {
        self.main_chat().push(DisplayMessage::autopilot(text));
    }

    /// The session's row goes to the writer first, so the decision has one
    /// to reference.
    fn log_autopilot_decision(&mut self, decision: AutopilotDecision) {
        self.settle_autopilot_card(&decision);
        self.push_autopilot_row(decision_row(
            decision.kind.name(),
            decision.action.name(),
            &decision.subject,
            &decision.reason,
        ));
        self.checkpoint_now();
        self.storage_writer
            .record_autopilot_decision(self.state.session.id, decision.into());
    }

    /// Starts the plan's implementation as `implement_plan` does, once the run
    /// that chose it is done. The text is an autopilot observation, not a
    /// message, so the turn reads as no human's. Input queued meanwhile goes
    /// first, and the plan stays for the user. A stop or a plan the user
    /// already took leaves nothing to do.
    pub(super) fn implement_for_autopilot(&mut self, seq: u32) -> Vec<Action> {
        if self.status != Status::Idle
            || !self.state.autopilot.is_engaged()
            || !self.state.plan.is_ready()
            || self.plan_unsaved()
        {
            return Vec::new();
        }
        if !self.queue.is_empty() {
            self.flash(PLAN_LEFT_TO_YOU.into());
            return Vec::new();
        }
        let snapshot = match self.capture_plan() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.flash(format!("{IMPLEMENT_CAPTURE_FAILED}: {error}"));
                return Vec::new();
            }
        };
        if let Err(error) = self.admit_run() {
            self.flash(error);
            return Vec::new();
        }
        self.count_autopilot_continuation();
        let header = self.implement_header(&snapshot.target);
        let mut input = self.continuation_input();
        input.mode = AgentMode::Build;
        input.preamble.push(Message::autopilot_observation(
            format!("{header}\n\n{}", snapshot.content),
            AutopilotOrigin {
                seq,
                kind: AutopilotMessageKind::Implement,
            },
        ));
        let actions = self.start_admitted_run(input, String::new(), StartedBy::Autopilot);
        self.leave_plan();
        self.main_chat()
            .push(DisplayMessage::plan(snapshot.content, snapshot.source));
        actions
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const CONDITION: &str = "the tests pass";
    const SUBJECT: &str = "shell: cargo test";
    const REASON: &str = "the tests need running";
    const ERROR: &str = "overloaded";
    const ATTEMPT: u32 = 2;
    const RETRY_LIMIT: u32 = 5;
    const DELAY_MS: u64 = 3_000;

    #[test_case("", AutopilotCommand::Engage; "bare")]
    #[test_case("  ", AutopilotCommand::Engage; "blank")]
    #[test_case("off", AutopilotCommand::Stop; "off")]
    #[test_case("STOP", AutopilotCommand::Stop; "stop_any_case")]
    #[test_case("model", AutopilotCommand::Model; "model")]
    #[test_case(" the tests pass ", AutopilotCommand::Goal(CONDITION); "goal_trimmed")]
    fn the_command_parses_its_argument(args: &str, expected: AutopilotCommand<'static>) {
        assert_eq!(parse_autopilot_command(args), expected);
    }

    #[test_case("/goal the tests pass", "/autopilot the tests pass"; "a_goal_becomes_owned")]
    #[test_case("  /goal   the tests pass ", "/autopilot the tests pass"; "padded_goal")]
    #[test_case("/goal model", "/goal model"; "the_model_picker_stays")]
    #[test_case("/goal clear", "/goal clear"; "a_clear_stays")]
    #[test_case("/goal", "/goal"; "no_condition")]
    #[test_case("/goals are fun", "/goals are fun"; "another_word")]
    #[test_case("fix the tests", "fix the tests"; "a_plain_prompt")]
    fn a_launch_goal_prompt_is_given_to_autopilot(prompt: &str, expected: &str) {
        assert_eq!(launch_prompt(prompt.to_owned()), expected);
    }

    #[test_case(SUBJECT, markdown_code_span(SUBJECT) ; "a_subject_is_quoted")]
    #[test_case("", String::new() ; "no_subject_stays_empty")]
    fn a_decision_row_quotes_its_subject(subject: &str, quoted: String) {
        let kind = DecisionKind::Permission.name();
        let action = ActionName::Allow.name();

        assert_eq!(
            decision_row(kind, action, subject, REASON),
            decision_text(kind, action, &quoted, REASON)
        );
    }

    #[test]
    fn a_retry_row_counts_attempts_and_the_wait() {
        assert_eq!(
            retrying_row(ATTEMPT, RETRY_LIMIT, DELAY_MS, ERROR),
            format!(
                "{RETRYING_ROW} {ATTEMPT}/{RETRY_LIMIT} in {}: {ERROR}",
                format_elapsed(Duration::from_millis(DELAY_MS).as_secs())
            )
        );
    }
}
