//! The App's half of autopilot: the session's handle, `/autopilot`, the spend
//! it counts, the implement handoff, the resume check-in and the stops.

use std::iter;
use std::mem;
use std::sync::Arc;
use std::time::{Duration, Instant};

use caudra_agent::AgentMode;
use caudra_agent::automation::clock::SystemClock;
use caudra_agent::autopilot::{
    ActionName, AutopilotDecision, AutopilotHandle, DecisionKind, EngagementSpend, StoodDown,
};
use caudra_agent::permissions::AutopilotNote;
use caudra_agent::types::AutopilotEvent;
use caudra_automation::event::StartedBy;
use caudra_config::{AutopilotConfig, Feature};
use caudra_providers::model_registry;
use caudra_providers::{
    AutopilotMessageKind, AutopilotOrigin, Billing, Message, ModelPurpose, TokenUsage,
};
use caudra_storage::StateDir;
use caudra_storage::autopilot::{
    AutopilotEnd, AutopilotStopper, DecisionOutcome, StoredAutopilot, StoredAutopilotDecision,
    StoredAutopilotEngagement,
};
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::{SessionDatabase, SessionError};
use caudra_storage::usage_ledger::LedgerPurpose;
use ratatui::Frame;
use ratatui::layout::Rect;
use tracing::warn;

use super::goal::LogPlace;
use super::tasks::MAIN_TASK_ID;
use super::{
    App, CONFIRMATION_DURATION, FLASH_CANCEL, IMPLEMENT_CAPTURE_FAILED, LOG_ARG, MODEL_ARG,
    Notification, Status, is_goal_clear,
};
use crate::components::audit_log::{COPIED_ENTRY, PanelTab, ROW_NOT_LOADED};
use crate::components::autopilot_panel::{
    AutopilotLog, NO_ENTRIES, PanelAction, PanelCommand, PanelContent, REASON_SEPARATOR,
    budget_left, decision_details, decision_title, end_notice, end_reason, goal_line,
    spend_summary, state_label,
};
use crate::components::messages::markdown_code_span;
use crate::components::status_bar::AutopilotChip;
use crate::components::{Action, DisplayMessage, Overlay, format_elapsed};
use crate::storage_writer::AutopilotWrite;

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
const LINE_BREAK: &str = "\n";
const AUTOPILOT_LOG: &str = "autopilot";
/// Autopilot retries the main agent's runs alone.
const MAIN_CHAT: usize = 0;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum AutopilotCommand<'a> {
    Engage,
    Goal(&'a str),
    Stop,
    Model,
    Log,
}

/// Where a restored log row goes among others at the same history length:
/// an engagement's start ahead of its decisions, and its end after them.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum LogRow {
    Engaged,
    Decision,
    Ended,
}

pub(super) fn parse_autopilot_command(args: &str) -> AutopilotCommand<'_> {
    let args = args.trim();
    if args.is_empty() {
        AutopilotCommand::Engage
    } else if args.eq_ignore_ascii_case(MODEL_ARG) {
        AutopilotCommand::Model
    } else if args.eq_ignore_ascii_case(LOG_ARG) {
        AutopilotCommand::Log
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
            ![MODEL_ARG, LOG_ARG]
                .iter()
                .any(|arg| condition.eq_ignore_ascii_case(arg))
                && !is_goal_clear(condition)
        });
    match condition {
        Some(condition) if !condition.is_empty() => format!("{AUTOPILOT_COMMAND} {condition}"),
        _ => prompt,
    }
}

/// A decision's transcript row, found again by its seq.
fn decision_message(decision: &StoredAutopilotDecision) -> DisplayMessage {
    DisplayMessage::autopilot_decision(decision.seq, decision_body(decision))
}

/// A decision row's title, its subject quoted so the markdown title draws it
/// as written, then the details it folds open to, one per line.
pub(super) fn decision_body(decision: &StoredAutopilotDecision) -> String {
    let subject = if decision.subject.is_empty() {
        String::new()
    } else {
        markdown_code_span(&decision.subject)
    };
    iter::once(decision_title(decision, &subject))
        .chain(decision_details(decision))
        .collect::<Vec<_>>()
        .join(LINE_BREAK)
}

/// The row an engagement's end draws: why it ended and what it spent.
pub(super) fn ended_row(end: &AutopilotEnd, engagement: &StoredAutopilotEngagement) -> String {
    format!(
        "{ENDED_ROW}: {}{REASON_SEPARATOR}{}",
        end_reason(end),
        spend_summary(engagement)
    )
}

fn retrying_row(attempt: u32, limit: u32, delay_ms: u64, error: &str) -> String {
    let delay = format_elapsed(Duration::from_millis(delay_ms).as_secs());
    format!("{RETRYING_ROW} {attempt}/{limit} in {delay}: {error}")
}

/// The session's whole log, for the panel's Log tab.
fn load_autopilot_log(storage: &StateDir, session_id: CaudraId) -> Result<AutopilotLog, String> {
    let database = SessionDatabase::open(storage).map_err(|error| error.to_string())?;
    let decisions = database
        .load_autopilot_decisions(session_id)
        .map_err(|error| error.to_string())?;
    let engagements = database
        .load_autopilot_engagements(session_id)
        .map_err(|error| error.to_string())?;
    Ok(AutopilotLog::new(decisions, engagements))
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

    /// The stored log's transcript rows, every decision and each engagement's
    /// start and end, with the history length and time each was made at, so a
    /// restore places it among the turns. None while the experiment is off.
    pub(super) fn autopilot_log_rows(&self) -> Vec<(LogPlace, DisplayMessage)> {
        if !self.features.enabled(Feature::Autopilot) {
            return Vec::new();
        }
        let decisions = self
            .stored_autopilot_decisions()
            .into_iter()
            .map(|decision| {
                let order = (
                    decision.history_len,
                    decision.engagement.unwrap_or_default(),
                    LogRow::Decision,
                );
                (order, decision.at_ms, decision_message(&decision))
            });
        let engagements = self
            .read_session_log(AUTOPILOT_LOG, SessionDatabase::load_autopilot_engagements)
            .into_iter()
            .flat_map(|engagement| {
                let number = engagement.engagement;
                let engaged = (
                    (engagement.engaged_history_len, number, LogRow::Engaged),
                    engagement.engaged_at_ms,
                    DisplayMessage::autopilot(ENGAGED_ROW.to_owned()),
                );
                let ended = engagement
                    .ended_history_len
                    .zip(engagement.end.as_ref())
                    .map(|(history_len, end)| {
                        (
                            (history_len, number, LogRow::Ended),
                            engagement.ended_at_ms.unwrap_or(engagement.engaged_at_ms),
                            DisplayMessage::autopilot(ended_row(end, &engagement)),
                        )
                    });
                iter::once(engaged).chain(ended)
            });
        let mut rows: Vec<_> = decisions.chain(engagements).collect();
        rows.sort_by(|(left, ..), (right, ..)| left.cmp(right));
        rows.into_iter()
            .map(|((history_len, ..), at_ms, row)| ((history_len, at_ms), row))
            .collect()
    }

    fn stored_autopilot_decisions(&self) -> Vec<StoredAutopilotDecision> {
        self.read_session_log(AUTOPILOT_LOG, SessionDatabase::load_autopilot_decisions)
    }

    /// A log is a record, so a session whose `log` cannot be read resumes
    /// without it.
    pub(super) fn read_session_log<T>(
        &self,
        log: &'static str,
        read: impl FnOnce(&SessionDatabase, CaudraId) -> Result<Vec<T>, SessionError>,
    ) -> Vec<T> {
        let session_id = self.state.session.id;
        SessionDatabase::open(&self.storage)
            .and_then(|database| read(&database, session_id))
            .unwrap_or_else(|error| {
                warn!(%error, %session_id, log, "log unreadable; resuming without it");
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
                self.open_autopilot_panel(PanelTab::Status);
                Vec::new()
            }
            AutopilotCommand::Log => {
                self.open_autopilot_log();
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

    /// Opens the panel on `tab`, reading the session's log off the UI thread.
    /// What the writer has yet to write is taken first, so the read misses
    /// nothing the transcript already shows.
    pub(super) fn open_autopilot_panel(&mut self, tab: PanelTab) {
        self.clear_control_hovers();
        let storage = self.storage.clone();
        let session_id = self.state.session.id;
        let unwritten = self.storage_writer.unwritten_autopilot(session_id);
        self.autopilot_panel.open(
            tab,
            smol::unblock(move || load_autopilot_log(&storage, session_id)),
            unwritten,
            self.ui_config.clock_format,
        );
    }

    /// The Log, engaged or not. A session autopilot never ran in has nothing
    /// logged, which needs no read to tell.
    fn open_autopilot_log(&mut self) {
        if self.state.autopilot.stored().is_none() {
            self.flash(NO_ENTRIES.into());
            return;
        }
        self.open_autopilot_panel(PanelTab::Log);
    }

    /// The panel closes before it stops, picks the model or jumps, so what
    /// that opens or scrolls is not drawn under it.
    pub(super) fn on_autopilot_panel(&mut self, action: PanelAction) -> Vec<Action> {
        match action {
            PanelAction::Copy(text) => self.copy_labelled(&text, COPIED_ENTRY),
            PanelAction::Flash(message) => self.flash(message.into()),
            PanelAction::Jump(seq) => self.jump_to_autopilot_decision(seq),
            PanelAction::Command(PanelCommand::Stop) => {
                self.autopilot_panel.close();
                self.stop_autopilot_by_command();
            }
            PanelAction::Command(PanelCommand::Model) => {
                self.autopilot_panel.close();
                return self.open_autopilot_model_picker();
            }
        }
        Vec::new()
    }

    /// Scrolls the main transcript to decision `seq`'s row. A row the
    /// transcript does not hold leaves the panel up, saying so.
    fn jump_to_autopilot_decision(&mut self, seq: u64) {
        let Some(row) = self.chats[MAIN_CHAT].autopilot_decision_row(seq) else {
            self.flash(ROW_NOT_LOADED.into());
            return;
        };
        self.autopilot_panel.close();
        self.scroll_main_to(row);
    }

    /// Scrolls the main transcript to row `row`, focusing it first.
    pub(super) fn scroll_main_to(&mut self, row: usize) {
        if !self.is_main_chat()
            && let Err(error) = self.focus_task(MAIN_TASK_ID)
        {
            self.flash(error);
            return;
        }
        self.main_chat().scroll_to_message(row);
    }

    pub(super) fn draw_autopilot_panel(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        let stored = self.state.autopilot.stored();
        let limits = self.state.autopilot.limits();
        let goal = self.state.goal.active_condition();
        let model = model_registry::binding(ModelPurpose::Autopilot);
        let escalations: Vec<String> = self
            .autopilot_escalations()
            .map(|(kind, reason)| format!("{}: {reason}", kind.name()))
            .collect();
        let content = PanelContent {
            stored: stored.as_ref(),
            limits: &limits,
            goal: goal.as_deref(),
            model: model.as_ref(),
            deciding: self.autopilot_is_deciding(),
            escalations: &escalations,
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
        .join(LINE_BREAK);
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
        self.question_form.touch();
        let key = self.question_form.call_id().unwrap_or_default();
        if self.question_form.autopilot_note() == Some(&AutopilotNote::Deciding)
            && self.state.autopilot.claim_question(key)
        {
            self.question_form.set_autopilot_note(None);
            let claimed = self.claimed_question_decisions();
            self.autopilot_deciding
                .retain(|(seq, _)| !claimed.contains(seq));
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
    /// A deferred question's decision outlives the run, as its job does.
    pub(super) fn autopilot_run_ended(&mut self) {
        for (seq, kind) in mem::take(&mut self.autopilot_deciding) {
            match kind {
                DecisionKind::Question if self.decides_deferred_question(seq) => {
                    self.autopilot_deciding.push((seq, kind));
                }
                DecisionKind::Question => {
                    if let Some(form) = self.decided_question(seq) {
                        form.set_autopilot_note(None);
                    }
                }
                DecisionKind::Plan => self.plan_form.set_autopilot_note(None),
                _ => {}
            }
        }
    }

    /// Engages with a fresh budget, owning the active goal when autopilot set
    /// it. Already engaged, it only takes ownership of such a goal.
    pub(crate) fn engage_autopilot(&mut self, owns_goal: bool) {
        let goal = self
            .state
            .goal
            .snapshot()
            .map(|goal| goal.condition.to_string());
        if let Some(engaged) = self.state.autopilot.engage(owns_goal, goal) {
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
            AutopilotEvent::Engaged { engagement, goal } => {
                self.autopilot_engaged(engagement, goal);
            }
            AutopilotEvent::Decided(decision) => self.log_autopilot_decision(*decision),
            AutopilotEvent::StoodDown(stood_down) => self.autopilot_stood_down(*stood_down),
            AutopilotEvent::Settled { seq, outcome } => self.autopilot_settled(seq, outcome),
            AutopilotEvent::Ended {
                reason,
                engagement,
                spend,
            } => self.autopilot_ended(reason, engagement, &spend),
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
            DecisionKind::Question => {
                if let Some(form) = self.decided_question(seq) {
                    form.set_autopilot_note(deciding);
                }
            }
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
                self.close_decided_question(decision.seq);
            }
            DecisionKind::Question => {
                if let Some(form) = self.decided_question(decision.seq) {
                    form.set_autopilot_note(left_to_you);
                }
                self.forget_question_decision(decision.seq);
            }
            DecisionKind::Plan => {
                self.autopilot_plan_escalation = left_to_you.is_some().then_some(decision.seq);
                self.plan_form.set_autopilot_note(left_to_you);
            }
            DecisionKind::Permission
            | DecisionKind::Reply
            | DecisionKind::Review
            | DecisionKind::SecondOpinion => {}
        }
    }

    /// The user answered the plan card autopilot left to them, by `action`.
    /// A card autopilot never left to them has nothing to settle.
    pub(super) fn answer_plan_escalation(&mut self, action: ActionName) {
        let Some(seq) = self.autopilot_plan_escalation.take() else {
            return;
        };
        let answer = action.name().to_owned();
        let settled = self
            .state
            .autopilot
            .settle(seq, DecisionOutcome::Answered { answer });
        self.note_autopilot_event(settled);
    }

    /// A decision autopilot gave up before it answered: its card stays with
    /// whoever holds it, and the log says who.
    fn autopilot_stood_down(&mut self, stood_down: StoodDown) {
        self.release_deciding_card(stood_down.seq);
        let decision = StoredAutopilotDecision::from(stood_down);
        self.main_chat().push(decision_message(&decision));
        self.record_autopilot(|_| AutopilotWrite::Decision(Box::new(decision)));
    }

    /// The card stops saying autopilot is deciding it.
    fn release_deciding_card(&mut self, seq: u32) {
        let Some(index) = self
            .autopilot_deciding
            .iter()
            .position(|(deciding, _)| *deciding == seq)
        else {
            return;
        };
        match self.autopilot_deciding.remove(index).1 {
            DecisionKind::Question => {
                if let Some(form) = self.decided_question(seq) {
                    form.set_autopilot_note(None);
                }
                self.forget_question_decision(seq);
            }
            DecisionKind::Plan => self.plan_form.set_autopilot_note(None),
            DecisionKind::Permission
            | DecisionKind::Reply
            | DecisionKind::Review
            | DecisionKind::SecondOpinion => {}
        }
    }

    fn push_autopilot_row(&mut self, text: String) {
        self.main_chat().push(DisplayMessage::autopilot(text));
    }

    fn log_autopilot_decision(&mut self, decision: AutopilotDecision) {
        self.settle_autopilot_card(&decision);
        let decision = StoredAutopilotDecision::from(decision);
        self.main_chat().push(decision_message(&decision));
        self.record_autopilot(|_| AutopilotWrite::Decision(Box::new(decision)));
    }

    /// What became of a logged decision. Its row is retitled while the
    /// handle still holds the decision, and the log learns it either way.
    fn autopilot_settled(&mut self, seq: u32, outcome: DecisionOutcome) {
        let settled = self
            .state
            .autopilot
            .recent_decisions()
            .into_iter()
            .find(|decision| decision.seq == seq);
        if let Some(decision) = settled {
            let decision = StoredAutopilotDecision {
                outcome: Some(outcome.clone()),
                ..decision.into()
            };
            self.main_chat()
                .rewrite_autopilot_decision(decision.seq, decision_body(&decision));
        }
        self.record_autopilot(|_| AutopilotWrite::Settle {
            seq: u64::from(seq),
            outcome,
        });
    }

    fn autopilot_engaged(&mut self, engagement: u32, goal: Option<String>) {
        self.push_autopilot_row(ENGAGED_ROW.to_owned());
        let stored = self.state.autopilot.stored();
        let engaged_at_ms = stored
            .as_ref()
            .and_then(|stored| stored.engaged_at_ms)
            .unwrap_or_else(|| self.state.autopilot.now_ms());
        let owns_goal = stored.is_some_and(|stored| stored.owns_goal);
        self.record_autopilot(|app| {
            AutopilotWrite::Engaged(StoredAutopilotEngagement {
                engagement,
                engaged_at_ms,
                engaged_history_len: app.autopilot_history_len(),
                goal,
                owns_goal,
                ended_at_ms: None,
                ended_history_len: None,
                end: None,
                continuations: 0,
                cost_usd: 0.0,
                subscription_cost_usd: 0.0,
                elapsed_ms: 0,
                retries: 0,
            })
        });
    }

    /// The row says why the engagement ended and what it spent. The notice
    /// says only why, as a notification carries fixed text alone.
    fn autopilot_ended(&mut self, end: AutopilotEnd, engagement: u32, spend: &EngagementSpend) {
        let ended_at_ms = self.state.autopilot.now_ms();
        let ended = StoredAutopilotEngagement {
            engagement,
            engaged_at_ms: ended_at_ms,
            engaged_history_len: 0,
            goal: None,
            owns_goal: false,
            ended_at_ms: Some(ended_at_ms),
            ended_history_len: None,
            end: Some(end.clone()),
            continuations: spend.continuations,
            cost_usd: spend.cost_usd,
            subscription_cost_usd: spend.subscription_cost_usd,
            elapsed_ms: spend.elapsed_ms,
            retries: spend.retries,
        };
        self.push_autopilot_row(ended_row(&end, &ended));
        self.autopilot_notice = Some(Notification::AutopilotEnded {
            reason: end_notice(&end),
        });
        self.record_autopilot(|app| {
            AutopilotWrite::Ended(StoredAutopilotEngagement {
                ended_history_len: Some(app.autopilot_history_len()),
                ..ended
            })
        });
    }

    /// The session's row goes to the writer first, so the log has one to
    /// reference, and `write` is built after it, so a history length it
    /// measures counts what that merged. An open panel's Log takes it too.
    fn record_autopilot(&mut self, write: impl FnOnce(&Self) -> AutopilotWrite) {
        self.checkpoint_now();
        let write = write(self);
        self.autopilot_panel.note(&write);
        self.storage_writer
            .record_autopilot(self.state.session.id, write);
    }

    /// History items the session holds, counted as a decision's row counts
    /// them. The snapshot autopilot last saw is newer during a run, and the
    /// merged transcript between runs, so the larger of the two is the nearer.
    pub(super) fn autopilot_history_len(&self) -> usize {
        let transcript =
            crate::transcript_session_history(&self.state.session).map_or(0, |items| items.len());
        self.state
            .autopilot
            .snapshot()
            .map_or(transcript, |snapshot| snapshot.history_len.max(transcript))
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
    #[test_case("Log", AutopilotCommand::Log; "log_any_case")]
    #[test_case(" the tests pass ", AutopilotCommand::Goal(CONDITION); "goal_trimmed")]
    fn the_command_parses_its_argument(args: &str, expected: AutopilotCommand<'static>) {
        assert_eq!(parse_autopilot_command(args), expected);
    }

    #[test_case("/goal the tests pass", "/autopilot the tests pass"; "a_goal_becomes_owned")]
    #[test_case("  /goal   the tests pass ", "/autopilot the tests pass"; "padded_goal")]
    #[test_case("/goal model", "/goal model"; "the_model_picker_stays")]
    #[test_case("/goal log", "/goal log"; "the_log_stays")]
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
        let decision = StoredAutopilotDecision {
            seq: 0,
            at_ms: 0,
            kind: DecisionKind::Permission.name().to_owned(),
            action: Some(ActionName::Allow.name().to_owned()),
            subject: subject.to_owned(),
            reason: REASON.to_owned(),
            agent: None,
            tool_use_id: None,
            history_len: 0,
            model: None,
            usage: None,
            cost: None,
            engagement: None,
            item: None,
            payload: None,
            outcome: None,
        };

        assert_eq!(
            decision_body(&decision).lines().next(),
            Some(decision_title(&decision, &quoted).as_str())
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
