//! Rebuilds display messages from stored sessions. Tool outputs get syntax
//! highlighted, missing outputs fall back to plain text from `ToolResult`.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::MouseEvent;

use crate::app::tasks::{TaskOutcome, TaskStatus};
use crate::components::card_decisions::{self, CardDecision, DecisionState, DecisionSubject};
use crate::components::commit_popup::CommitIndex;
use crate::components::messages::{MessageActionTarget, MessageDetails, MessagesPanel};
use crate::components::producer::Producer;
use crate::components::prompt_progress::PromptProgress;
use crate::components::tool_display::append_annotation;
use crate::components::tooltip::Tip;
use crate::components::workflow_card::CardHit;
use crate::components::{
    DisplayMessage, DisplayRole, DisplaySource, RetryInfo, ToolRole, ToolStatus, workflow_card,
};
use crate::inline_image::ImageRef;
use crate::markdown::truncate_output;

use crate::selection::Selection;
use caudra_agent::background::TaskSnapshot;
use caudra_agent::decisions::{DecisionFeature, DecisionNote, PendingDecision, ScreeningVerdict};
use caudra_agent::permissions::PermissionRequest;
use caudra_agent::restart::is_resume_nudge;
use caudra_agent::tools::native::plan::{self, PlanTarget, PlanWriteResult};
use caudra_agent::tools::native::question::asked_questions;
use caudra_agent::tools::{
    BATCH_TOOL_NAME, FILE_WRITE_TOOL_NAME, ToolEffect, ToolInvocation, ToolRegistry,
    WORKFLOW_TOOL_NAME,
};
use caudra_agent::types::{Answer, AnswerSheet, QuestionEvent, TodoItem, WorkflowRunCard};
use caudra_agent::{
    AgentEvent, BatchToolEntry, BufferSnapshot, COMPACTION_ANCHOR, CallStage, CommitRef,
    EMPTY_RESPONSE_RULE, ImageSource, Mention, QueueConsumedItem, SubagentProgress, TaskCard,
    ToolDoneEvent, ToolOutput, ToolStartEvent,
};
use caudra_config::{ToolOutputLines, UiConfig};
use caudra_lua::WinView;
use caudra_providers::calibration;
use caudra_providers::{
    AutomationEventOrigin, AutopilotMessageKind, CaudraId, HistoryItem, HistoryItemKind,
    ReasoningSource, SteeringKind, SteeringOrigin, TokenScale, UserOrigin,
};
use caudra_storage::engine_decisions::{StoredDecisionAnchor, StoredEngineDecision};
use caudra_storage::usage_ledger::LedgerPurpose;
use caudra_storage::view::ViewMode;
use caudra_workflow::RunSnapshot;
use jiff::Timestamp;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::repaint::{Cadence, Dirty};

pub(crate) const DONE_TEXT: &str = "Done!";
pub(crate) const ERROR_TEXT: &str = "Error";
pub(crate) const CANCELLED_TEXT: &str = "Cancelled";
pub(crate) const INTERRUPTED_TEXT: &str = "Interrupted";
/// The seam a summary replaced turns at. Progress belongs to the status bar's
/// spinner, so the transcript card marks the border instead of announcing work.
const COMPACTION_BORDER_TEXT: &str =
    "Context compacted - the turns above were replaced by the summary below.";
/// What a batch appends to its own id to run a child under, from
/// `batch::child_tool_use_id`.
const BATCH_CHILD_ID_SEPARATOR: char = ':';
/// What parts an implement message's header from the plan it carries.
const IMPLEMENT_HEADER_END: &str = "\n\n";

pub enum ChatEventResult {
    Continue,
    PlanWritten(PlanWriteResult),
    Done,
    QueueItemConsumed {
        id: caudra_agent::QueueItemId,
        text: String,
        images: Vec<ImageSource>,
    },
    QueueBatchConsumed {
        items: Vec<QueueConsumedItem>,
    },
    Error(String),
    PermissionRequest(Box<PermissionRequest>),
    PermissionRequestUpdated(Box<PermissionRequest>),
    PermissionRequestResolved {
        request_id: String,
    },
    AuthRequired,
    AuthRestored,
    Question(Box<QuestionEvent>),
}

pub struct Chat {
    pub name: String,
    pub cost: Option<f64>,
    pub subscription_cost: Option<f64>,
    pub context_size: u32,
    pub context_window: u32,
    pub model_id: Option<String>,
    /// What a task's own requests carry: the reasoning level its subagent
    /// resolved against its own model, and whether it runs fast. The footer
    /// draws both for whichever chat is on screen, so neither can be read off
    /// the session.
    pub thinking: Option<String>,
    pub fast: bool,
    pending_turn_usage: Option<String>,
    messages_panel: MessagesPanel,
    todos: Vec<TodoItem>,
    /// The list a still-streaming `todo_write` has written so far, under its
    /// call id. It stands in for [`Self::todos`] until that call finishes,
    /// and is dropped unread when it fails or its run ends without it.
    live_todos: Option<(String, Vec<TodoItem>)>,
    /// The ending and the index of the bubble announcing it, so a later, better
    /// informed outcome can fix that bubble instead of appending a second one.
    finish: Option<(TaskOutcome, usize)>,
    /// `None` for the main chat, the subagent's `tool_use_id` otherwise. That
    /// is the handle `caudra.task` addresses a task by, see `app::tasks`.
    task_id: Option<Arc<str>>,
    parent_tool_use_id: Option<Arc<str>>,
    /// The provider backoff this chat's own stream is waiting out. It is per
    /// chat because a subagent retrying says nothing about the main chat, and
    /// the status bar draws whichever chat is on screen.
    retry: Option<RetryInfo>,
    /// Whether harness-injected messages get a row at all. They always start
    /// folded, so this is the switch for readers who want the transcript to
    /// hold nothing but the conversation.
    show_reminders: bool,
    /// Whether what the decision engine did gets a row. The rows are stored
    /// either way, so turning this back on shows them again on restore.
    show_decisions: bool,
    /// The row the current stall is reporting on, and whether the injection
    /// about to arrive belongs to it. A stall answers itself several times
    /// over; the transcript should still read as one event.
    stall_row: Option<usize>,
    stall_pending: bool,
}

impl Chat {
    /// `cwd` is the session's working directory, which a running command's
    /// `workdir` argument is resolved against.
    pub fn new(
        name: String,
        cwd: &Path,
        ui_config: UiConfig,
        lua_event_handle: caudra_lua::EventHandle,
    ) -> Self {
        let mut chat = Self {
            name,
            cost: None,
            subscription_cost: None,
            context_size: 0,
            context_window: 0,
            model_id: None,
            thinking: None,
            fast: false,
            pending_turn_usage: None,
            show_reminders: ui_config.show_reminders,
            show_decisions: ui_config.show_decisions,
            stall_row: None,
            stall_pending: false,
            messages_panel: MessagesPanel::new(ui_config, lua_event_handle),
            todos: Vec::new(),
            live_todos: None,
            finish: None,
            task_id: None,
            parent_tool_use_id: None,
            retry: None,
        };
        chat.set_cwd(cwd);
        chat
    }

    pub(crate) fn subagent(
        task_id: &str,
        name: String,
        cwd: &Path,
        ui_config: UiConfig,
        lua_event_handle: caudra_lua::EventHandle,
    ) -> Self {
        Self {
            task_id: Some(Arc::from(task_id)),
            ..Self::new(name, cwd, ui_config, lua_event_handle)
        }
    }

    pub(crate) fn set_cwd(&mut self, cwd: &Path) {
        self.messages_panel.set_cwd(cwd);
    }

    pub(crate) fn task_id(&self) -> Option<&Arc<str>> {
        self.task_id.as_ref()
    }

    /// A chat opened from a still-streaming delegation is keyed on the id its
    /// subagent is predicted to take. When the real one differs, the chat
    /// follows it rather than being duplicated beside it.
    pub(crate) fn set_task_id(&mut self, id: impl Into<Arc<str>>) {
        self.task_id = Some(id.into());
    }

    /// Whether decisions from here on get a row; rows already drawn stay.
    /// What a card already carries shows or hides with the switch.
    pub(crate) fn set_show_decisions(&mut self, shown: bool) {
        self.show_decisions = shown;
        self.messages_panel.set_show_decisions(shown);
    }

    /// Grows the instruction this task is being given, before the subagent
    /// that will receive it exists.
    pub(crate) fn prompt_delta(&mut self, text: &str) {
        self.messages_panel.prompt_delta(text);
    }

    pub(crate) fn parent_tool_use_id(&self) -> Option<&Arc<str>> {
        self.parent_tool_use_id.as_ref()
    }

    pub(crate) fn set_parent_tool_use_id(&mut self, id: impl Into<Arc<str>>) {
        self.parent_tool_use_id = Some(id.into());
    }

    pub(crate) fn task_status(&self) -> TaskStatus {
        self.finish.map(|(outcome, _)| outcome).into()
    }

    pub(crate) fn task_outcome(&self) -> Option<TaskOutcome> {
        self.finish.map(|(outcome, _)| outcome)
    }

    pub(crate) fn resume(&mut self) {
        self.flush();
        if let Some((_, bubble)) = self.finish.take() {
            self.messages_panel.remove(bubble);
        }
    }

    pub fn set_pending_turn_usage(&mut self, usage: String) {
        self.pending_turn_usage = Some(usage);
    }

    pub(crate) fn set_restore_channel(&mut self, event_tx: Option<caudra_agent::EventSender>) {
        self.messages_panel.set_restore_channel(event_tx);
    }

    /// The events the app routes itself, which never reach [`Self::handle_event`].
    pub(crate) fn routed_before(event: &AgentEvent) -> bool {
        matches!(
            event,
            AgentEvent::StreamReset
                | AgentEvent::AccountRouted { .. }
                | AgentEvent::AccountExhausted(_)
                | AgentEvent::ModelRerouted(_)
                | AgentEvent::SettingsChanged { .. }
                | AgentEvent::TaskAdmitted(_)
                | AgentEvent::Retry { .. }
                | AgentEvent::SubagentProgress { .. }
                | AgentEvent::Unrecorded { .. }
        )
    }

    pub fn handle_event(&mut self, event: AgentEvent, plan_path: Option<&Path>) -> ChatEventResult {
        match event {
            AgentEvent::ThinkingDelta { text } => {
                self.messages_panel.clear_prompt_progress();
                self.messages_panel.thinking_delta(&text);
            }
            AgentEvent::ThinkingBoundary => self.messages_panel.thinking_boundary(),
            AgentEvent::TextDelta { text } => {
                self.messages_panel.clear_prompt_progress();
                self.messages_panel.text_delta(&text);
            }
            AgentEvent::ToolPending { id, name } => self.messages_panel.tool_pending(id, &name),
            AgentEvent::ToolInputDelta {
                id,
                preview,
                size,
                body,
                roster,
                todos,
                patch,
                complete,
                ..
            } => {
                if let Some(items) = &todos {
                    self.live_todos = Some((id.clone(), items.clone()));
                }
                self.messages_panel.tool_input_todos(&id, todos);
                self.messages_panel.tool_input_patch(&id, patch);
                self.messages_panel.tool_input_roster(&id, roster);
                self.messages_panel.tool_input_preview(&id, preview, size);
                self.messages_panel.tool_input_body(&id, body);
                if complete {
                    self.messages_panel.leave_stage(&id, CallStage::Drafting);
                }
            }
            AgentEvent::ToolStart(e) => self.messages_panel.tool_start(*e),
            AgentEvent::ToolOutput { id, content } => self.tool_output(&id, &content),
            AgentEvent::BatchProgress(e) => {
                let plan = e.entry.plan_write_result();
                if self.messages_panel.batch_progress(&e.id, e.index, e.entry)
                    && let Some(plan) = plan
                {
                    return ChatEventResult::PlanWritten(plan);
                }
            }
            AgentEvent::CodeProgress(e) => {
                let e = *e;
                self.messages_panel.code_progress(&e.id, e.index, e.call);
            }
            AgentEvent::ToolDone(mut e) => {
                if self.live_todos.as_ref().is_some_and(|(id, _)| *id == e.id) {
                    self.live_todos = None;
                }
                if let ToolOutput::TodoList(items) = &e.output {
                    self.todos.clone_from(items);
                }
                let native_plan = &*e.tool == plan::NAME;
                let plan_write = plan_path.filter(|pp| !native_plan && e.wrote_to(pp));
                if native_plan
                    && e.annotation
                        .as_deref()
                        .is_some_and(|annotation| annotation.starts_with(plan::WRITE_RESULT_PREFIX))
                {
                    e.annotation = e.output.annotation(self.messages_panel.token_scale());
                }
                let is_full_write = &*e.tool == FILE_WRITE_TOOL_NAME;
                let tool_id = e.id.clone();
                self.messages_panel.tool_done(*e);
                if let Some(pp) = plan_write {
                    let content = if is_full_write {
                        std::fs::read_to_string(pp).unwrap_or_default()
                    } else {
                        String::new()
                    };
                    // The plan card below draws the same file, and a write's
                    // card draws it too, so the write falls back to its header
                    // rather than the plan being read twice. An edit keeps its
                    // body: the plan card it produces has none.
                    if !content.is_empty() {
                        self.messages_panel.close_tool_card(&tool_id);
                    }
                    self.messages_panel
                        .push(DisplayMessage::plan(content, pp.display().to_string()));
                }
            }
            // A compaction summary is edited after its stream closes: the
            // requirements section is appended once extraction finishes. The
            // agent streams that append too, so this is a no-op on the happy
            // path and the guarantee that the card equals what was stored on
            // any other.
            AgentEvent::TurnComplete(turn) => {
                if turn.purpose == LedgerPurpose::Compaction
                    && let Some(text) = turn.message.first_text_content()
                {
                    self.messages_panel.adopt_final_text(text);
                }
            }
            AgentEvent::ModelUsage { .. }
            | AgentEvent::CacheWarmEpisode(_)
            | AgentEvent::CacheHitSamples(_)
            | AgentEvent::TokenCalibration(_)
            | AgentEvent::Learned(_)
            | AgentEvent::LearnerActivity(_) => {}
            AgentEvent::GoalEvaluating { .. }
            | AgentEvent::GoalSkipped { .. }
            | AgentEvent::GoalEvaluation { .. }
            | AgentEvent::GoalFinished { .. }
            | AgentEvent::GoalDeferred { .. }
            | AgentEvent::GoalLoopCap { .. }
            | AgentEvent::GoalStalled { .. }
            | AgentEvent::GoalWaiting { .. }
            | AgentEvent::GoalTurnLimit { .. }
            | AgentEvent::GoalEvaluationFailed { .. }
            | AgentEvent::GoalClearedAfterError { .. } => {}
            AgentEvent::ToolResultsSubmitted { .. } => {
                if let Some(usage) = self.pending_turn_usage.take() {
                    self.messages_panel.set_turn_usage_on_last_tool(usage);
                }
            }
            AgentEvent::MessageStarted {
                model, thinking, ..
            } => self
                .messages_panel
                .message_started(Producer { model, thinking }),
            AgentEvent::Compacting => {
                self.messages_panel.flush();
                self.messages_panel.set_producer(None);
                self.messages_panel.push(DisplayMessage::new(
                    DisplayRole::Notice,
                    COMPACTION_BORDER_TEXT.into(),
                ));
            }
            AgentEvent::CompactionDone => {
                self.messages_panel.flush();
            }
            AgentEvent::QueueItemConsumed { id, text, images } => {
                return ChatEventResult::QueueItemConsumed { id, text, images };
            }
            AgentEvent::QueueBatchConsumed { items } => {
                return ChatEventResult::QueueBatchConsumed { items };
            }
            AgentEvent::QueueDrained
            | AgentEvent::UserExchangeAdmitted { .. }
            | AgentEvent::TaskGuidanceQueued { .. }
            | AgentEvent::SessionTitle { .. }
            | AgentEvent::MemoryChanged
            | AgentEvent::QuestionClosed
            | AgentEvent::SubagentFinished { .. }
            | AgentEvent::Autopilot(_) => {}
            AgentEvent::Decision(note) => match self.show_decisions {
                true => self.place_decision(*note),
                false => self
                    .messages_panel
                    .settle_decision(&(&note.anchor).into(), &note.feature),
            },
            AgentEvent::DecisionPending(pending) => {
                if self.show_decisions {
                    self.wait_on_decision(*pending);
                }
            }
            AgentEvent::DecisionSettled(pending) => self.end_decision_wait(*pending),
            AgentEvent::StreamReset
            | AgentEvent::AccountRouted { .. }
            | AgentEvent::AccountExhausted(_)
            | AgentEvent::ModelRerouted(_)
            | AgentEvent::SettingsChanged { .. }
            | AgentEvent::TaskAdmitted(_)
            | AgentEvent::Retry { .. }
            | AgentEvent::SubagentProgress { .. }
            | AgentEvent::Unrecorded { .. } => {
                unreachable!("handled before handle_event")
            }
            AgentEvent::Done { .. } => {
                self.live_todos = None;
                self.messages_panel.flush();
                return ChatEventResult::Done;
            }
            AgentEvent::Error { message } => {
                self.live_todos = None;
                self.messages_panel.flush();
                return ChatEventResult::Error(message);
            }
            AgentEvent::PermissionRequest(request) => {
                return ChatEventResult::PermissionRequest(request);
            }
            AgentEvent::PermissionRequestUpdated(request) => {
                return ChatEventResult::PermissionRequestUpdated(request);
            }
            AgentEvent::PermissionRequestResolved { request_id, .. } => {
                return ChatEventResult::PermissionRequestResolved { request_id };
            }
            AgentEvent::AuthRequired => {
                return ChatEventResult::AuthRequired;
            }
            AgentEvent::AuthRestored => {
                return ChatEventResult::AuthRestored;
            }
            AgentEvent::Question(event) => {
                return ChatEventResult::Question(event);
            }
            AgentEvent::ToolSnapshot {
                id,
                snapshot,
                theme_gen,
            } => {
                self.messages_panel.tool_snapshot(&id, snapshot, theme_gen);
            }
            AgentEvent::ToolHeaderSnapshot {
                id,
                snapshot,
                theme_gen,
            } => {
                self.messages_panel
                    .tool_header_snapshot(&id, snapshot, theme_gen);
            }
            // The continuation itself arrives as `Injected` and carries the
            // prompt the model was actually sent, which is strictly more than a
            // fixed line could say. A repeat only updates the row it already
            // wrote: one stall is one row, however many attempts it takes.
            AgentEvent::Nudge { attempt, .. } => {
                if attempt == 1 {
                    self.stall_row = None;
                }
                self.stall_pending = true;
            }
            AgentEvent::Injected {
                text,
                peer_event: Some(origin),
                ..
            } => {
                self.messages_panel.flush();
                self.messages_panel
                    .push(DisplayMessage::peer(&text, origin));
            }
            AgentEvent::Injected {
                text,
                automation_event: Some(origin),
                peer_event: None,
                ..
            } => {
                self.messages_panel.flush();
                self.messages_panel
                    .push(DisplayMessage::automation(text, origin));
            }
            AgentEvent::Injected {
                text,
                autopilot_event: Some(origin),
                peer_event: None,
                ..
            } => {
                self.messages_panel.flush();
                self.messages_panel
                    .push(DisplayMessage::autopilot(live_autopilot_text(
                        text,
                        origin.kind,
                    )));
            }
            AgentEvent::Injected {
                text,
                task_event: Some(origin),
                peer_event: None,
                ..
            } => {
                self.messages_panel.flush();
                self.messages_panel
                    .push(DisplayMessage::injected(text, Some(origin)));
            }
            AgentEvent::Injected {
                text,
                task_event: None,
                peer_event: None,
                ..
            } => {
                self.messages_panel.flush();
                if self.show_reminders || is_resume_nudge(&text) {
                    let row = DisplayMessage::injected(text, None);
                    match self.stall_pending.then_some(self.stall_row).flatten() {
                        Some(index) => self.messages_panel.replace(index, row),
                        None => {
                            let index = self.messages_panel.push(row);
                            if self.stall_pending {
                                self.stall_row = Some(index);
                            }
                        }
                    }
                }
                self.stall_pending = false;
            }
            AgentEvent::SubagentHistory { .. } | AgentEvent::Workflow(_) => {}
            AgentEvent::LiveToolBuf { id, body } => {
                self.messages_panel.register_live_buf(id, body);
            }
            AgentEvent::ToolAnnotation { id, annotation } => {
                self.messages_panel.tool_annotation(&id, annotation);
            }
            AgentEvent::PromptProgress {
                processed,
                total,
                cache,
            } => {
                self.messages_panel
                    .set_prompt_progress((processed < total).then_some(PromptProgress {
                        processed,
                        total,
                        cache,
                    }));
            }
        }
        ChatEventResult::Continue
    }

    pub fn scroll(&mut self, delta: i32) {
        self.messages_panel.scroll(delta);
    }

    pub(crate) fn plan_written(&mut self, id: &str, plan: &PlanWriteResult) {
        self.messages_panel.close_tool_card(id);
        self.messages_panel.push(plan_message(plan));
    }

    /// A decision goes on the row it belongs to, settling the wait it
    /// answers: a call's on the call's card, a turn's on the row the
    /// transcript ended on when it was asked. With no such row, as for a call
    /// whose card this chat does not hold, it is a row of its own that
    /// follows the transcript.
    fn place_decision(&mut self, note: DecisionNote) {
        let subject = DecisionSubject::from(&note.anchor);
        if subject == DecisionSubject::Turn {
            self.messages_panel.flush();
        }
        if self.messages_panel.attach_decision(
            CardDecision::settled(
                subject,
                note.feature.clone(),
                note.outcome,
                &note.summary,
                &note.details,
            )
            .screened(note.screening),
        ) {
            return;
        }
        self.messages_panel.flush();
        self.messages_panel.push(DisplayMessage::decision(
            note.feature.label(),
            note.outcome,
            None,
            &note.summary,
            &note.details,
        ));
    }

    /// A wait shows as a chip on the row its answer will go on, and nowhere
    /// when there is no such row.
    fn wait_on_decision(&mut self, pending: PendingDecision) {
        let subject = DecisionSubject::from(&pending.anchor);
        if subject == DecisionSubject::Turn {
            self.messages_panel.flush();
        }
        self.messages_panel.attach_decision(
            CardDecision::pending(subject, pending.feature).in_phase(pending.phase),
        );
    }

    /// The engine answered with nothing to do. A call's own wait goes off its
    /// card; any other stays on its row as a quiet chip that keeps what came
    /// closest, or, when the feature had nothing to ask, why.
    fn end_decision_wait(&mut self, pending: PendingDecision) {
        let subject = DecisionSubject::from(&pending.anchor);
        if !card_decisions::keeps_quiet(&subject, &pending.feature) {
            self.messages_panel
                .settle_decision(&subject, &pending.feature);
            return;
        }
        if subject == DecisionSubject::Turn {
            self.messages_panel.flush();
        }
        self.messages_panel.quiet_decision(
            &subject,
            &pending.feature,
            pending.cause,
            &pending.details,
        );
    }

    /// Opens or closes the details of the decisions whose chips are under the
    /// pointer, on a text row or a card's header.
    pub fn toggle_decisions_at(&mut self, column: u16, row: u16, area: Rect) -> bool {
        self.messages_panel.toggle_decisions_at(column, row, area)
    }

    /// Offers the wheel to an armed scroll card under the pointer first,
    /// returning what is left for the transcript.
    pub fn scroll_card_at(&mut self, column: u16, row: u16, delta: i32) -> i32 {
        self.messages_panel.scroll_card_at(column, row, delta)
    }

    /// Arms the window under the pointer, consuming the press when one is
    /// there so the card is not folded by the same click.
    pub fn arm_card_at(&mut self, column: u16, row: u16) -> bool {
        self.messages_panel.arm_card_at(column, row)
    }

    /// Whether a release completes an arming press, so the same click does not
    /// also fold the card.
    pub fn armed_card_at(&self, column: u16, row: u16) -> bool {
        self.messages_panel.armed_card_at(column, row)
    }

    pub fn disarm_card(&mut self) {
        self.messages_panel.disarm_card();
    }

    #[cfg(test)]
    pub(crate) fn card_window_key_at(&self, column: u16, row: u16) -> Option<&str> {
        self.messages_panel.card_window_key_at(column, row)
    }

    #[cfg(test)]
    pub(crate) fn armed_card_key(&self) -> Option<&str> {
        self.messages_panel.armed_card_key()
    }

    pub fn set_scroll_top(&mut self, top: u32) {
        self.messages_panel.set_scroll_top(top);
    }

    pub fn handle_card_scrollbar(&mut self, event: &MouseEvent) -> bool {
        self.messages_panel.handle_card_scrollbar(event)
    }

    pub fn handle_scrollbar(&mut self, event: &MouseEvent) -> bool {
        self.messages_panel.handle_scrollbar(event)
    }

    pub fn half_page(&self) -> i32 {
        self.messages_panel.half_page()
    }

    pub fn win_view(&self) -> WinView {
        self.messages_panel.win_view()
    }

    pub fn auto_scroll(&self) -> bool {
        self.messages_panel.auto_scroll()
    }

    pub fn scroll_to_top(&mut self) {
        self.messages_panel.scroll_to_top();
    }

    pub fn enable_auto_scroll(&mut self) {
        self.messages_panel.enable_auto_scroll();
    }

    pub fn scroll_to_segment(&mut self, segment_index: usize) {
        self.messages_panel.scroll_to_segment(segment_index);
    }

    pub fn scroll_to_message(&mut self, msg_index: usize) {
        self.messages_panel.scroll_to_message(msg_index);
    }

    pub fn restore_scroll(&mut self, scroll_top: u32, auto_scroll: bool) {
        self.messages_panel.restore_scroll(scroll_top, auto_scroll);
    }

    pub fn set_highlight_segment(&mut self, idx: Option<usize>) {
        self.messages_panel.set_highlight_segment(idx);
    }

    pub fn message_for_item(&self, item: CaudraId) -> Option<(usize, bool)> {
        self.messages_panel.message_for_item(item)
    }

    pub fn message_for_tool(&self, tool_id: &str) -> Option<usize> {
        self.messages_panel.message_for_tool(tool_id)
    }

    pub fn land_on_message(&mut self, msg_index: usize) {
        self.messages_panel.land_on_message(msg_index);
    }

    pub fn landing(&self) -> Option<usize> {
        self.messages_panel.landing()
    }

    pub fn clear_landing(&mut self) -> bool {
        self.messages_panel.clear_landing()
    }

    pub fn set_accent(&mut self, color: Color) {
        self.messages_panel.set_accent(color);
    }

    pub fn set_view(&mut self, view: ViewMode) {
        self.messages_panel.set_view(view);
    }

    pub fn tick(&mut self) -> Dirty {
        self.messages_panel.tick()
    }

    pub fn cadence(&self) -> Cadence {
        Cadence::any([
            self.messages_panel.cadence(),
            Cadence::when(self.retry.is_some(), Cadence::SPINNER),
        ])
    }

    /// `message_actions` draws the `⋮` handles, which only a transcript the
    /// app routes clicks to can answer.
    pub fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        has_selection: bool,
        message_actions: bool,
    ) {
        self.messages_panel
            .view(frame, area, has_selection, message_actions);
    }

    pub fn scroll_top(&self) -> u32 {
        self.messages_panel.scroll_top()
    }

    pub fn segment_heights(&self) -> Vec<u16> {
        self.messages_panel.segment_heights()
    }

    pub fn segment_search_texts(&self) -> Vec<&str> {
        self.messages_panel.segment_search_texts()
    }

    pub fn last_reply_source(&self) -> Option<String> {
        self.messages_panel.last_reply_source()
    }

    pub fn extract_selection_text(&self, sel: &Selection, msg_area: Rect) -> String {
        self.messages_panel.extract_selection_text(sel, msg_area)
    }

    pub fn handle_click(&mut self, row: u16, area: Rect) {
        self.messages_panel.handle_click(row, area);
    }

    pub(crate) fn update_hover(
        &mut self,
        row: u16,
        col: u16,
        area: Rect,
        known_task_target: bool,
        cwd: &Path,
    ) {
        self.messages_panel
            .update_hover(row, col, area, known_task_target, cwd);
    }

    pub(crate) fn update_hover_remote(
        &mut self,
        row: u16,
        col: u16,
        area: Rect,
        known_task_target: bool,
    ) {
        self.messages_panel
            .update_hover_remote(row, col, area, known_task_target);
    }

    pub(crate) fn clear_hover(&mut self) {
        self.messages_panel.clear_hover();
    }

    pub(crate) fn hovered_hint(&self) -> Option<Cow<'_, str>> {
        self.messages_panel.hovered_hint()
    }

    pub(crate) fn hover_tip(&self) -> Option<Tip> {
        self.messages_panel.hover_tip()
    }

    pub(crate) fn terminal_links(&self) -> &[crate::markdown::TerminalLink] {
        self.messages_panel.terminal_links()
    }

    pub(crate) fn link_at(&self, row: u16, col: u16, area: Rect) -> Option<Arc<str>> {
        self.messages_panel.link_at(row, col, area)
    }

    pub(crate) fn mention_at(&self, row: u16, col: u16, area: Rect, cwd: &Path) -> Option<Mention> {
        self.messages_panel.mention_at(row, col, area, cwd)
    }

    pub(crate) fn mention_at_remote(&self, row: u16, col: u16, area: Rect) -> Option<Mention> {
        self.messages_panel.mention_at_remote(row, col, area)
    }

    pub(crate) fn image_at(&self, row: u16, col: u16, area: Rect) -> Option<ImageRef> {
        self.messages_panel.image_at(row, col, area)
    }

    pub(crate) fn commit_at(&self, row: u16, col: u16, area: Rect) -> Option<CommitRef> {
        self.messages_panel.commit_at(row, col, area)
    }

    pub(crate) fn set_commit_index(&mut self, index: CommitIndex) {
        self.messages_panel.set_commit_index(index);
    }

    pub(crate) fn message_action_at(&self, row: u16, col: u16) -> Option<MessageActionTarget> {
        self.messages_panel.message_action_at(row, col)
    }

    pub(crate) fn message_target_at_row(
        &self,
        row: u16,
        area: Rect,
    ) -> Option<MessageActionTarget> {
        self.messages_panel.message_target_at_row(row, area)
    }

    pub(crate) fn message_details(&self, target: MessageActionTarget) -> MessageDetails {
        self.messages_panel.message_details(target)
    }

    pub(crate) fn pan_hovered_diagram(&mut self, delta: i32) -> bool {
        self.messages_panel.pan_hovered_diagram(delta)
    }

    pub(crate) fn pan_visible_diagram(&mut self, delta: i32) -> bool {
        self.messages_panel.pan_visible_diagram(delta)
    }

    #[cfg(test)]
    pub(crate) fn panned_diagram_count(&self) -> usize {
        self.messages_panel.panned_diagram_count()
    }

    pub fn tool_id_at(&self, row: u16, area: Rect) -> Option<&str> {
        self.messages_panel.tool_id_at(row, area)
    }

    pub fn dispatched_id_at(&self, row: u16, area: Rect) -> Option<String> {
        self.messages_panel.dispatched_id_at(row, area)
    }

    #[cfg(debug_assertions)]
    pub fn grab_provenance_at(&self, row: u16, area: Rect) -> Option<String> {
        self.messages_panel.grab_provenance_at(row, area)
    }

    pub fn last_assistant_source(&self) -> Option<DisplaySource> {
        self.messages_panel.last_assistant_source()
    }

    pub(crate) fn review_target(
        &self,
        source: DisplaySource,
    ) -> Option<crate::components::messages::ReviewTarget> {
        self.messages_panel.review_target(source)
    }

    pub fn tool_snapshot(
        &mut self,
        tool_id: &str,
        snapshot: BufferSnapshot,
        theme_gen: Option<u64>,
    ) {
        self.messages_panel
            .tool_snapshot(tool_id, snapshot, theme_gen);
    }

    pub fn tool_header_snapshot(
        &mut self,
        tool_id: &str,
        snapshot: BufferSnapshot,
        theme_gen: Option<u64>,
    ) {
        self.messages_panel
            .tool_header_snapshot(tool_id, snapshot, theme_gen);
    }

    pub(crate) fn set_retry(&mut self, retry: RetryInfo) {
        self.retry = Some(retry);
    }

    pub(crate) fn retry(&self) -> Option<&RetryInfo> {
        self.retry.as_ref()
    }

    pub(crate) fn clear_retry(&mut self) {
        self.retry = None;
    }

    pub fn stream_reset(&mut self) {
        self.retry = None;
        self.live_todos = None;
        self.messages_panel.stream_reset();
    }

    pub fn flush(&mut self) {
        self.messages_panel.flush();
    }

    pub fn cancel_in_progress(&mut self) {
        self.live_todos = None;
        self.messages_panel.cancel_in_progress();
    }

    pub fn fail_in_progress_with_message(&mut self, message: String) {
        self.live_todos = None;
        self.messages_panel.fail_in_progress_with_message(message);
    }

    pub fn fail_in_progress_except(&mut self, message: String, excluded: &HashSet<String>) {
        if self
            .live_todos
            .as_ref()
            .is_some_and(|(id, _)| !excluded.contains(id))
        {
            self.live_todos = None;
        }
        self.messages_panel
            .fail_in_progress_except(message, excluded);
    }

    pub fn push(&mut self, msg: DisplayMessage) {
        self.messages_panel.push(msg);
    }

    pub fn remove_notice(&mut self, text: &str) {
        self.messages_panel.remove_notice(text);
    }

    /// The row autopilot's decision `seq` drew, for a caller that scrolls to
    /// it.
    pub fn autopilot_decision_row(&self, seq: u64) -> Option<usize> {
        self.messages_panel.autopilot_decision_row(seq)
    }

    /// The row goal evaluation `seq` drew, for a caller that scrolls to it.
    pub fn goal_evaluation_row(&self, seq: u64) -> Option<usize> {
        self.messages_panel.goal_evaluation_row(seq)
    }

    pub fn rewrite_autopilot_decision(&mut self, seq: u64, text: String) {
        self.messages_panel.rewrite_autopilot_decision(seq, text);
    }

    /// Ends the transcript with the bubble [`TaskOutcome::role`] picks. A chat
    /// only ever grows one ending, but a caller who knows more than the one who
    /// got here first rewrites it in place. See [`TaskOutcome::refines`].
    pub(crate) fn mark_finished(&mut self, outcome: TaskOutcome, text: &str) {
        self.retry = None;
        if let Some((previous, bubble)) = self.finish {
            if outcome.refines(previous) {
                self.finish = Some((outcome, bubble));
                self.messages_panel
                    .replace(bubble, DisplayMessage::new(outcome.role(), text.into()));
            }
            return;
        }
        self.messages_panel.flush();
        let bubble = self
            .messages_panel
            .push(DisplayMessage::new(outcome.role(), text.into()));
        self.finish = Some((outcome, bubble));
    }

    pub fn is_finished(&self) -> bool {
        self.finish.is_some()
    }

    pub fn update_tool_summary(&mut self, tool_id: &str, summary: &str) {
        self.messages_panel.update_tool_summary(tool_id, summary);
    }

    /// A permission request is raised under the id of the call it is for, so
    /// the request's own id names the card or batch child it holds.
    pub(crate) fn await_approval(&mut self, tool_id: &str) {
        self.messages_panel.await_approval(tool_id);
    }

    pub(crate) fn approval_settled(&mut self, tool_id: &str) {
        self.messages_panel
            .leave_stage(tool_id, CallStage::AwaitingApproval);
    }

    /// The scale results arriving from now on are counted in.
    pub fn set_token_scale(&mut self, scale: TokenScale) {
        self.messages_panel.set_token_scale(scale);
    }

    pub fn set_tool_model(&mut self, tool_id: &str, model: &str, thinking: Option<&str>) {
        self.messages_panel.set_tool_model(tool_id, model, thinking);
    }

    #[cfg(test)]
    pub fn tool_model(&self, tool_id: &str) -> Option<&str> {
        self.messages_panel.tool_model(tool_id)
    }

    pub fn set_tool_turn_usage(&mut self, tool_id: &str, usage: String) {
        self.messages_panel.set_tool_turn_usage(tool_id, usage);
    }

    /// A dispatched child's id is the batch's own with an index appended, so a
    /// report either names a card with a header of its own or one row of a
    /// roster. The suffix is dropped only when it resolves to a real child, so
    /// a tool whose id merely contains a colon still reaches its own header.
    pub fn set_tool_progress(&mut self, tool_id: &str, report: SubagentProgress) {
        if let Some((batch_id, index)) = batch_child_id(tool_id)
            && self
                .messages_panel
                .set_batch_child_progress(batch_id, index, report.clone())
        {
            return;
        }
        self.messages_panel.set_tool_progress(tool_id, report);
    }

    pub(crate) fn task_card_update(&mut self, card: TaskCard) -> bool {
        self.messages_panel.task_card_update(card)
    }

    pub(crate) fn has_task_card(&self, card: &TaskCard) -> bool {
        self.messages_panel.has_task_card(card)
    }

    pub(crate) fn reconcile_task_cards(&mut self, runtime: &TaskSnapshot) -> bool {
        self.messages_panel.reconcile_task_cards(runtime)
    }

    pub(crate) fn task_hit_at(&self, row: u16, area: Rect) -> Option<String> {
        self.messages_panel.task_hit_at(row, area)
    }

    /// The firing whose delivery a click at `row` landed on.
    pub(crate) fn automation_hit_at(&self, row: u16, area: Rect) -> Option<AutomationEventOrigin> {
        self.messages_panel.automation_hit_at(row, area)
    }

    pub(crate) fn task_prompt(&self, call_id: &str) -> Option<String> {
        self.messages_panel.task_prompt(call_id)
    }

    /// Streaming output, addressed the same way a report is: a batch runs its
    /// children under ids of its own, and one of those names a roster row
    /// rather than a card.
    pub fn tool_output(&mut self, tool_id: &str, content: &str) {
        if let Some((batch_id, index)) = batch_child_id(tool_id)
            && self
                .messages_panel
                .set_batch_child_output(batch_id, index, content)
        {
            return;
        }
        self.messages_panel.tool_output(tool_id, content);
    }

    pub fn load_messages(&mut self, msgs: Vec<DisplayMessage>) {
        self.live_todos = None;
        self.todos = msgs
            .iter()
            .rev()
            .find_map(|msg| match msg.tool_output.as_deref() {
                Some(ToolOutput::TodoList(items)) => Some(items.clone()),
                _ => None,
            })
            .unwrap_or_default();
        self.messages_panel.load_messages(msgs);
    }

    /// The list the todo panel shows: the one being written while a call
    /// streams it, the last one a call stored otherwise.
    pub(crate) fn todos(&self) -> &[TodoItem] {
        self.live_todos
            .as_ref()
            .map_or(&self.todos, |(_, items)| items)
    }

    /// Whether `event` can change what [`Self::todos`] returns.
    pub(crate) fn touches_todos(event: &AgentEvent) -> bool {
        matches!(
            event,
            AgentEvent::ToolInputDelta { todos: Some(_), .. }
                | AgentEvent::ToolDone(_)
                | AgentEvent::Done { .. }
                | AgentEvent::Error { .. }
        )
    }

    pub fn set_producer(&mut self, producer: Option<Producer>) {
        self.messages_panel.set_producer(producer);
    }

    pub fn bind_sources(&mut self, messages: &[DisplayMessage]) {
        self.messages_panel.bind_sources(messages);
    }

    pub fn push_user_message(&mut self, text: impl Into<String>) {
        self.messages_panel
            .push(DisplayMessage::new(DisplayRole::User, text.into()));
    }

    /// Flush, push, and re-pin scroll in one shot to avoid
    /// the one-frame hop where the bubble briefly lands in the wrong row.
    pub fn show_user_message(&mut self, text: impl Into<String>) {
        self.show_user_prompt(text.into(), Arc::default());
    }

    /// [`Self::show_user_message`] for a prompt that carried pictures, which
    /// its bubble draws under the text.
    pub fn show_user_prompt(&mut self, text: String, images: Arc<[ImageSource]>) {
        self.flush();
        let mut message = DisplayMessage::new(DisplayRole::User, text);
        message.images = images;
        self.messages_panel.push(message);
        self.enable_auto_scroll();
    }

    /// The bubbles of queued prompts the agent picked up together, each with
    /// the pictures it was sent with.
    pub fn show_consumed_prompts(&mut self, items: &[QueueConsumedItem]) {
        self.flush();
        for item in items {
            let text = format_with_images(&item.text, item.images.len());
            let mut message = DisplayMessage::new(DisplayRole::User, text);
            message.images = item.images.as_slice().into();
            self.messages_panel.push(message);
        }
        self.enable_auto_scroll();
    }

    pub fn shell_tool_start(&mut self, event: ToolStartEvent) {
        self.messages_panel.tool_start(event);
    }

    pub fn shell_tool_output(&mut self, id: &str, content: &str) {
        self.messages_panel.tool_output(id, content);
    }

    pub fn shell_tool_done(&mut self, event: ToolDoneEvent) {
        self.messages_panel.tool_done(event);
    }

    /// Draws the card of a run a slash command launched. Transient like a
    /// `!` shell block: the runtime keeps the run, the transcript keeps the
    /// conversation.
    pub fn workflow_card_start(&mut self, run: &RunSnapshot) {
        let card = WorkflowRunCard::from(run);
        self.messages_panel.tool_start(ToolStartEvent {
            id: workflow_card::card_id(&run.run_id),
            tool: WORKFLOW_TOOL_NAME.into(),
            effect: ToolEffect::Orchestrator,
            summary: workflow_card::summary(&card),
            render_header: None,
            annotation: Some(workflow_card::annotation(&card)),
            input: None,
            raw_input: None,
            output: Some(ToolOutput::WorkflowRun(Box::new(card))),
        });
    }

    pub fn workflow_card_update(&mut self, run: &RunSnapshot) -> bool {
        self.messages_panel.workflow_card_update(run)
    }

    /// The run, or the scratch file, whose card a click at `row` landed on.
    pub(crate) fn workflow_hit_at(&self, row: u16, area: Rect) -> Option<CardHit> {
        self.messages_panel.workflow_hit_at(row, area)
    }

    /// The memory note whose card row a click at `row` landed on.
    pub(crate) fn memory_hit_at(&self, row: u16, area: Rect) -> Option<PathBuf> {
        self.messages_panel.memory_hit_at(row, area)
    }

    pub fn message_count(&self) -> usize {
        self.messages_panel.message_count()
    }

    #[cfg(test)]
    pub fn message_at(&self, index: usize) -> Option<&DisplayMessage> {
        self.messages_panel.message_at(index)
    }

    #[cfg(test)]
    pub fn in_progress_count(&self) -> usize {
        self.messages_panel.in_progress_count()
    }

    #[cfg(test)]
    pub fn last_message_text(&self) -> &str {
        self.messages_panel.last_message_text()
    }

    #[cfg(test)]
    pub fn last_message_is_plan(&self) -> bool {
        self.messages_panel.last_message_is_plan()
    }

    #[cfg(test)]
    pub fn last_message_role(&self) -> Option<&DisplayRole> {
        self.messages_panel.last_message_role()
    }

    #[cfg(test)]
    pub fn streaming_text_is_empty(&self) -> bool {
        self.messages_panel.streaming_text_is_empty()
    }

    #[cfg(test)]
    pub fn streaming_thinking_is_empty(&self) -> bool {
        self.messages_panel.streaming_thinking_is_empty()
    }

    #[cfg(test)]
    pub fn settle_splash(&mut self) {
        self.messages_panel.settle_splash();
    }

    #[cfg(test)]
    pub fn tool_turn_usage(&self, tool_id: &str) -> Option<&str> {
        self.messages_panel.tool_turn_usage(tool_id)
    }
}

/// The batch and roster index an id names, if it is shaped like a dispatched
/// child's. Only a shape: the caller confirms the child exists before dropping
/// the suffix, so a tool whose own id merely contains a colon still reaches
/// its own header.
pub(crate) fn batch_child_id(tool_id: &str) -> Option<(&str, usize)> {
    let (parent, index) = tool_id.rsplit_once(BATCH_CHILD_ID_SEPARATOR)?;
    Some((parent, index.parse().ok()?))
}

/// What a plan card names its plan by, whether written or handed off.
pub(crate) fn plan_source(target: &PlanTarget) -> String {
    match target {
        PlanTarget::Local(path) => path.display().to_string(),
        PlanTarget::Remote(_) => plan::SESSION_PLAN_LABEL.to_owned(),
    }
}

fn plan_message(plan: &PlanWriteResult) -> DisplayMessage {
    DisplayMessage::plan(plan.content().to_owned(), plan_source(plan.target()))
}

/// A live implement message draws its header alone, under the plan card the
/// handoff already drew, as a human's handoff does. Restored, it is one row
/// with the plan in it, again as a human's is.
fn live_autopilot_text(text: String, kind: AutopilotMessageKind) -> String {
    match (kind, text.split_once(IMPLEMENT_HEADER_END)) {
        (AutopilotMessageKind::Implement, Some((header, _))) => header.to_owned(),
        _ => text,
    }
}

fn is_stall_prompt(steering: &Option<SteeringOrigin>) -> bool {
    steering.as_ref().is_some_and(|origin| {
        origin.kind == SteeringKind::Recovery && origin.rule == EMPTY_RESPONSE_RULE
    })
}

pub fn history_to_display(
    items: &[HistoryItem],
    tool_outputs: &HashMap<String, Arc<ToolOutput>>,
    tool_output_lines: &ToolOutputLines,
    show_reminders: bool,
) -> (Vec<DisplayMessage>, Vec<caudra_lua::RestoreItem>) {
    interleaved_history_to_display(
        items,
        tool_outputs,
        tool_output_lines,
        show_reminders,
        Vec::new(),
        Vec::new(),
    )
}

/// Stored decisions split the way a live chat draws them: rows and turns'
/// decisions for [`interleaved_history_to_display`] to place, and the
/// decisions that go on a call's card.
pub(crate) struct RestoredDecisions {
    pub(crate) rows: Vec<(usize, DisplayMessage)>,
    pub(crate) turns: Vec<(usize, CardDecision)>,
    pub(crate) cards: Vec<CardDecision>,
}

/// A turn's decision goes on the row drawn last before the item it was made
/// at. A call's goes on the call's card. One whose feature this build does
/// not know cannot say what its chip is, so it keeps a row, a turn's before
/// that item and a call's right after the item that made the call, titled by
/// its stored name. A decision whose call `items` no longer holds is dropped.
pub(crate) fn decision_inserts<'a>(
    items: &[HistoryItem],
    stored: impl IntoIterator<Item = &'a StoredEngineDecision>,
) -> RestoredDecisions {
    let mut after_calls: HashMap<&str, usize> = HashMap::new();
    for (index, item) in items.iter().enumerate() {
        if let HistoryItemKind::ToolCall { call_id, .. } = &item.kind {
            after_calls.entry(call_id.as_str()).or_insert(index + 1);
        }
    }
    let mut restored = RestoredDecisions {
        rows: Vec::new(),
        turns: Vec::new(),
        cards: Vec::new(),
    };
    for row in stored {
        let feature = DecisionFeature::from_name(&row.feature);
        let settled = |subject, feature| {
            CardDecision::settled(
                subject,
                feature,
                row.outcome.into(),
                &row.summary,
                &row.details,
            )
            .screened(row.screening.as_deref().and_then(ScreeningVerdict::parse))
        };
        match (&row.anchor, feature) {
            (StoredDecisionAnchor::History(len), Some(feature)) => {
                restored
                    .turns
                    .push((*len, settled(DecisionSubject::Turn, feature)));
            }
            (StoredDecisionAnchor::History(len), None) => {
                restored.rows.push((*len, stored_decision_row(row)));
            }
            (StoredDecisionAnchor::Tool(id), Some(feature)) => {
                if after_call(&after_calls, id).is_some() {
                    restored
                        .cards
                        .push(settled(DecisionSubject::Call(id.clone()), feature));
                }
            }
            (StoredDecisionAnchor::Tool(id), None) => {
                if let Some(position) = after_call(&after_calls, id) {
                    restored.rows.push((position, stored_decision_row(row)));
                }
            }
        }
    }
    restored
}

/// Puts each restored decision on the first card drawing its call, a batch
/// child's on its batch's card.
pub(crate) fn attach_card_decisions(display: &mut [DisplayMessage], cards: Vec<CardDecision>) {
    if cards.is_empty() {
        return;
    }
    let mut first_cards = HashMap::new();
    for (index, message) in display.iter().enumerate() {
        if let Some(call_id) = message.role.tool_id() {
            first_cards.entry(call_id.to_owned()).or_insert(index);
        }
    }
    for decision in cards {
        let DecisionSubject::Call(call_id) = &decision.subject else {
            continue;
        };
        let mut call_id = call_id.as_str();
        let card = loop {
            if let Some(&card) = first_cards.get(call_id) {
                break Some(card);
            }
            match batch_child_id(call_id) {
                Some((parent, _)) => call_id = parent,
                None => break None,
            }
        };
        if let Some(card) = card {
            display[card].decisions.push(decision);
        }
    }
}

/// A batch child's call is drawn on its batch's card, so it falls back to the
/// position after the batch.
fn after_call(after_calls: &HashMap<&str, usize>, mut call_id: &str) -> Option<usize> {
    loop {
        if let Some(position) = after_calls.get(call_id) {
            return Some(*position);
        }
        call_id = batch_child_id(call_id)?.0;
    }
}

fn stored_decision_row(row: &StoredEngineDecision) -> DisplayMessage {
    let feature = DecisionFeature::from_name(&row.feature)
        .map_or(row.feature.as_str(), |feature| feature.label());
    DisplayMessage::decision(
        feature,
        row.outcome.into(),
        None,
        &row.summary,
        &row.details,
    )
}

/// [`history_to_display`] with rows kept outside the history, each placed
/// after the first `n` items where `n` is the position it comes with. Rows
/// sharing a position keep their order, and a position past the end lands
/// after everything. Each of a turn's `decisions` goes on the last row that
/// can carry it once its position is reached, after the rows placed there.
pub fn interleaved_history_to_display(
    items: &[HistoryItem],
    tool_outputs: &HashMap<String, Arc<ToolOutput>>,
    tool_output_lines: &ToolOutputLines,
    show_reminders: bool,
    mut inserts: Vec<(usize, DisplayMessage)>,
    mut decisions: Vec<(usize, CardDecision)>,
) -> (Vec<DisplayMessage>, Vec<caudra_lua::RestoreItem>) {
    inserts.sort_by_key(|(position, _)| *position);
    let mut inserts = inserts.into_iter().peekable();
    decisions.sort_by_key(|(position, _)| *position);
    let mut decisions = decisions.into_iter().peekable();
    let results = build_tool_results_map(items);
    let mut group_text = build_user_text_map(items);
    let mut group_images: HashMap<CaudraId, Vec<ImageSource>> = HashMap::new();
    for item in items {
        if let HistoryItemKind::User { images, .. } = &item.kind
            && !images.is_empty()
        {
            group_images
                .entry(item.group_id)
                .or_default()
                .extend(images.iter().cloned());
        }
    }
    let mut display = Vec::new();
    let mut restore_items: Vec<caudra_lua::RestoreItem> = Vec::new();
    let mut stall_row: Option<usize> = None;
    let mut last_producer: Option<Producer> = None;
    for (index, item) in items.iter().enumerate() {
        while let Some((_, row)) = inserts.next_if(|(position, _)| *position <= index) {
            display.push(row);
            last_producer = None;
        }
        while let Some((_, decision)) = decisions.next_if(|(position, _)| *position <= index) {
            host_turn_decision(&mut display, decision);
        }
        let first_row = display.len();
        match &item.kind {
            HistoryItemKind::User {
                text,
                display_text,
                peer_event: Some(origin),
                ..
            } => display.push(DisplayMessage::peer(
                display_text.as_deref().unwrap_or(text),
                (**origin).clone(),
            )),
            HistoryItemKind::User {
                text,
                display_text,
                automation_event: Some(origin),
                ..
            } => display.push(DisplayMessage::automation(
                display_text.clone().unwrap_or_else(|| text.clone()),
                origin.clone(),
            )),
            HistoryItemKind::User {
                text,
                display_text,
                autopilot_event: Some(_),
                ..
            } => display.push(DisplayMessage::autopilot(
                display_text.clone().unwrap_or_else(|| text.clone()),
            )),
            // An injected item is its own row rather than a candidate for the
            // turn's bubble, so it must not consume the group: a reminder and
            // the message it trails can share one.
            HistoryItemKind::User {
                origin: UserOrigin::Observation | UserOrigin::Synthetic,
                text,
                steering,
                task_event,
                ..
            } => {
                if task_event.is_some()
                    || is_resume_nudge(text)
                    || (show_reminders && text != COMPACTION_ANCHOR)
                {
                    let row = DisplayMessage::injected(text.clone(), task_event.clone());
                    // A stall left one pair per attempt behind. Restoring it as
                    // one row keeps the record without replaying the repetition
                    // that a live run already collapsed.
                    let replaceable = is_stall_prompt(steering)
                        .then_some(stall_row)
                        .flatten()
                        .filter(|index| index + 1 == display.len());
                    match replaceable {
                        Some(index) => display[index] = row,
                        None => {
                            display.push(row);
                            stall_row =
                                is_stall_prompt(steering).then(|| display.len().saturating_sub(1));
                        }
                    }
                }
            }
            HistoryItemKind::User {
                origin: UserOrigin::Mention,
                ..
            } => {}
            HistoryItemKind::User { .. } => {
                if let Some(UserGroupText::Visible(id, text)) = group_text.remove(&item.group_id) {
                    let images: Arc<[ImageSource]> = group_images
                        .remove(&item.group_id)
                        .map_or_else(Arc::default, Arc::from);
                    let mut message = DisplayMessage::new(
                        DisplayRole::User,
                        format_with_images(text, images.len()),
                    );
                    message.source = Some(DisplaySource::User(id));
                    message.images = images;
                    display.push(message);
                }
            }
            HistoryItemKind::AssistantText {
                text,
                is_compaction_summary,
                ..
            } if !text.is_empty() => {
                if *is_compaction_summary {
                    display.push(DisplayMessage::new(
                        DisplayRole::Notice,
                        COMPACTION_BORDER_TEXT.into(),
                    ));
                }
                let mut message = DisplayMessage::new(DisplayRole::Assistant, text.clone());
                message.source = Some(DisplaySource::AssistantText(item.id));
                display.push(message);
            }
            HistoryItemKind::Reasoning {
                text,
                redacted: false,
                duration_ms,
                responses,
                ..
            } if !text.is_empty() || responses.is_some() => {
                let mut message = DisplayMessage::new(DisplayRole::Thinking, text.clone());
                message.source = Some(DisplaySource::Reasoning(item.id));
                message.thinking_duration = duration_ms.map(Duration::from_millis);
                display.push(message);
            }
            HistoryItemKind::ToolCall {
                call_id,
                name,
                input,
                source,
                ..
            } => {
                let static_name = name.as_str();
                let reg = ToolRegistry::global();
                let entry = reg.get(name);
                let tool_call: Option<Box<dyn ToolInvocation>> =
                    entry.as_ref().and_then(|entry| entry.try_parse(input));
                let effect = entry.as_ref().map_or(ToolEffect::Unknown, |entry| {
                    tool_call
                        .as_deref()
                        .map_or(entry.effect, |call| entry.effect_for(call))
                });
                let summary = reg.resolve_header(name, input);
                let result = results.get(call_id.as_str());
                let (status, result_text) = result
                    .map(|result| {
                        let status = if result.is_error {
                            ToolStatus::Error
                        } else {
                            ToolStatus::Success
                        };
                        (status, Some(result.content))
                    })
                    .unwrap_or((ToolStatus::Success, None));
                let reconstructed = tool_outputs
                    .get(call_id.as_str())
                    .cloned()
                    .map(|output| stamped_batch_effects(output, reg))
                    .map(|output| restored_answers(output, input));
                let saved_plan = (static_name == plan::NAME && status == ToolStatus::Success)
                    .then(|| {
                        reconstructed
                            .as_ref()
                            .and_then(|output| output.plan_write_result())
                    })
                    .flatten();
                let (text, truncated_lines, tool_output, mut annotation) = build_loaded_tool(
                    static_name,
                    &summary,
                    reconstructed.clone(),
                    result_text,
                    tool_output_lines,
                    source.as_ref(),
                );
                if let Some(tool_annotation) = tool_call
                    .as_deref()
                    .and_then(|call| call.start_annotation())
                {
                    append_annotation(&mut annotation, &tool_annotation);
                }
                let output = reconstructed
                    .as_ref()
                    .map(|output| output.as_text())
                    .or_else(|| result_text.map(str::to_owned))
                    .unwrap_or_default();
                let state = reconstructed
                    .as_ref()
                    .and_then(|output| output.state().cloned());
                let rust_rendered = reconstructed.as_ref().is_some_and(|output| {
                    output.structured_display_text().is_some()
                        || (static_name == BATCH_TOOL_NAME
                            && matches!(output.as_ref(), ToolOutput::Batch { .. }))
                });
                if !rust_rendered && saved_plan.is_none() {
                    restore_items.push(caudra_lua::RestoreItem {
                        tool: Arc::from(static_name),
                        tool_use_id: call_id.clone(),
                        output,
                        input: input.clone(),
                        is_error: status == ToolStatus::Error,
                        tool_output_lines: *tool_output_lines,
                        theme_gen: None,
                        clicks: Vec::new(),
                        state,
                        lua_provenance: reconstructed
                            .as_ref()
                            .and_then(|output| output.lua_provenance().cloned()),
                    });
                }
                display.push(DisplayMessage {
                    decisions: Vec::new(),
                    role: DisplayRole::Tool(Box::new(ToolRole {
                        id: call_id.clone(),
                        status,
                        name: static_name.into(),
                        effect,
                    })),
                    text,
                    source: Some(DisplaySource::ToolCall {
                        id: item.id,
                        result_id: result.map(|result| result.id),
                    }),
                    tool_input: tool_call
                        .as_deref()
                        .and_then(|call| call.start_input())
                        .map(Arc::new),
                    tool_raw_input: Some(Arc::new(input.clone())),
                    tool_output,
                    tool_preview_pending: false,
                    tool_stage: None,
                    live_output: None,
                    live_body: None,
                    annotation,
                    progress: None,
                    plan_path: None,
                    created: None,
                    producer: None,
                    turn_usage: None,
                    truncated_lines,
                    render_snapshot: None,
                    render_header: None,
                    snapshot_theme_gen: 0,
                    body_open: saved_plan.as_ref().map(|_| false),
                    thinking_duration: None,
                    tool_started: None,
                    images: Arc::default(),
                });
                if let Some(plan) = saved_plan {
                    display.push(plan_message(&plan));
                }
            }
            HistoryItemKind::AssistantText { .. }
            | HistoryItemKind::Reasoning { .. }
            | HistoryItemKind::ToolResult { .. } => {}
        }
        let producer = item_producer(item);
        if is_compaction_summary(item) {
            last_producer = None;
        } else if let Some(producer) = &producer
            && display.len() > first_row
        {
            if let Some(note) = last_producer
                .as_ref()
                .and_then(|last| last.change_note(producer))
            {
                display.insert(first_row, DisplayMessage::new(DisplayRole::Notice, note));
            }
            last_producer = Some(producer.clone());
        }
        let created = item_created(item);
        for row in &mut display[first_row..] {
            row.created = created;
            if !matches!(row.role, DisplayRole::Notice) {
                row.producer.clone_from(&producer);
            }
        }
    }
    display.extend(inserts.map(|(_, row)| row));
    for (_, decision) in decisions {
        host_turn_decision(&mut display, decision);
    }
    (display, restore_items)
}

/// Puts a restored turn's `decision` on the last row that can carry it, or,
/// in a transcript with none, on a row of its own.
fn host_turn_decision(display: &mut Vec<DisplayMessage>, decision: CardDecision) {
    if let Some(host) = display.iter().rposition(|row| row.role.hosts_decisions()) {
        display[host].decisions.push(decision);
        return;
    }
    if let DecisionState::Settled {
        outcome,
        text,
        took,
        ..
    } = decision.state
    {
        display.push(DisplayMessage::new(
            DisplayRole::Decision(outcome, took),
            text,
        ));
    }
}

/// A compaction copy is new, but it stands for something said when its
/// original was.
fn item_created(item: &HistoryItem) -> Option<Timestamp> {
    let millis = item.stands_for.unwrap_or(item.id).created_ms()?;
    Timestamp::from_millisecond(i64::try_from(millis).ok()?).ok()
}

fn item_producer(item: &HistoryItem) -> Option<Producer> {
    match &item.kind {
        HistoryItemKind::AssistantText { source, .. }
        | HistoryItemKind::Reasoning { source, .. }
        | HistoryItemKind::ToolCall { source, .. } => source.as_ref().map(Producer::from),
        HistoryItemKind::User { .. } | HistoryItemKind::ToolResult { .. } => None,
    }
}

fn is_compaction_summary(item: &HistoryItem) -> bool {
    matches!(
        item.kind,
        HistoryItemKind::AssistantText {
            is_compaction_summary: true,
            ..
        }
    )
}

/// The text a prompt's bubble shows, live and restored alike, so the two can
/// be matched to bind the live bubble to its history.
pub(crate) fn format_with_images(text: &str, image_count: usize) -> String {
    match image_count {
        0 => text.to_owned(),
        1 => format!("{text} [1 image]"),
        n => format!("{text} [{n} images]"),
    }
}

enum UserGroupText<'a> {
    Hidden,
    Pending,
    Visible(CaudraId, &'a str),
}

fn build_user_text_map(items: &[HistoryItem]) -> HashMap<CaudraId, UserGroupText<'_>> {
    let mut groups = HashMap::new();
    for item in items {
        let HistoryItemKind::User {
            text,
            display_text,
            origin,
            ..
        } = &item.kind
        else {
            continue;
        };
        let group = groups.entry(item.group_id).or_insert_with(|| {
            if *origin != UserOrigin::Turn {
                return UserGroupText::Hidden;
            }
            match display_text.as_deref() {
                Some("") => UserGroupText::Hidden,
                Some(text) => UserGroupText::Visible(item.id, text),
                None => UserGroupText::Pending,
            }
        });
        if matches!(group, UserGroupText::Pending) && !text.trim().is_empty() {
            *group = UserGroupText::Visible(item.id, text);
        }
    }
    groups
}

/// Rebuilds a Lua restore request from the data retained by a tool row.
pub(crate) fn restore_item_for(
    msg: &DisplayMessage,
    tool_output_lines: caudra_config::ToolOutputLines,
    theme_gen: u64,
) -> Option<caudra_lua::RestoreItem> {
    let DisplayRole::Tool(role) = &msg.role else {
        return None;
    };
    let input = msg.tool_raw_input.as_deref()?;
    let stored = msg.tool_output.as_deref()?;
    if stored.structured_display_text().is_some() {
        return None;
    }
    let output = stored.as_text();
    let state = stored.state().cloned();
    Some(caudra_lua::RestoreItem {
        tool: role.name.clone(),
        tool_use_id: role.id.clone(),
        output,
        input: input.clone(),
        is_error: role.status == ToolStatus::Error,
        tool_output_lines,
        theme_gen: Some(theme_gen),
        clicks: Vec::new(),
        state,
        lua_provenance: stored.lua_provenance().cloned(),
    })
}

/// Fills in the effects of a restored batch's children from the registry,
/// the way the call above resolves the parent's. A child stamps its own as it
/// starts, so this only ever answers for a session written before that was
/// recorded; the disclosure rule reads the effect, and without this every such
/// child would read as unclassified and open. A tool no longer registered
/// stays unclassified, which is what its own card does with it.
fn stamped_batch_effects(output: Arc<ToolOutput>, reg: &ToolRegistry) -> Arc<ToolOutput> {
    let ToolOutput::Batch { entries, text } = output.as_ref() else {
        return output;
    };
    if !entries.iter().any(|e| e.effect == ToolEffect::Unknown) {
        return output;
    }
    let entries = entries
        .iter()
        .map(|entry| BatchToolEntry {
            effect: reg
                .get(&entry.tool)
                .map_or(entry.effect, |tool| tool.effect),
            ..entry.clone()
        })
        .collect();
    Arc::new(ToolOutput::Batch {
        entries,
        text: text.clone(),
    })
}

/// Fills a restored answer back in from the questions it was given, the way the
/// call above fills a restored batch child in from the registry.
///
/// An answer is persisted as the picks alone, because the tool call's input
/// holds the form already and storing it twice would only let the two disagree.
/// The card draws the form, so it is put back together here, where the input
/// and the output are both in hand. A session written before the card drew
/// anything but the picks restores through this same path, which is what makes
/// it look like one written after.
fn restored_answers(output: Arc<ToolOutput>, input: &serde_json::Value) -> Arc<ToolOutput> {
    let ToolOutput::Answers(sheet) = output.as_ref() else {
        return output;
    };
    if sheet
        .answers
        .iter()
        .all(|answer| !answer.question.is_empty())
    {
        return output;
    }
    let questions = asked_questions(input);
    // An input that no longer lines up with its answers is one the card cannot
    // redraw; the picks it does have are worth more than a form built from the
    // wrong questions.
    if questions.len() != sheet.answers.len() {
        return output;
    }
    let answers = sheet
        .answers
        .iter()
        .zip(questions)
        .map(|(answer, asked)| Answer {
            question: asked.question,
            options: asked.options,
            ..answer.clone()
        })
        .collect();
    Arc::new(ToolOutput::Answers(AnswerSheet {
        answers,
        note: sheet.note.clone(),
    }))
}

/// Mirrors the live `tool_done` path so restored sessions
/// look the same as streamed ones.
///
/// Counted in the current scale of the model that made the call, which is the
/// one that read its result.
fn build_loaded_tool(
    tool: &str,
    summary: &str,
    reconstructed: Option<Arc<ToolOutput>>,
    result_text: Option<&str>,
    tool_output_lines: &ToolOutputLines,
    caller: Option<&ReasoningSource>,
) -> (String, usize, Option<Arc<ToolOutput>>, Option<String>) {
    let scale = caller.map_or(TokenScale::UNIT, |caller| {
        calibration::scale_of(&caller.provider, &caller.model)
    });
    match reconstructed {
        Some(output) => {
            let annotation = output.annotation(scale);
            (summary.to_owned(), 0, Some(output), annotation)
        }
        None => {
            let result = result_text.unwrap_or("");
            let annotation = if !result.is_empty() {
                ToolOutput::Plain(result.into()).annotation(scale)
            } else {
                None
            };
            if result.is_empty() {
                (summary.to_owned(), 0, None, annotation)
            } else {
                let tr = truncate_output(result, tool_output_lines.get(tool));
                (
                    format!("{}\n{}", summary, tr.kept),
                    tr.skipped,
                    None,
                    annotation,
                )
            }
        }
    }
}

struct ToolResultRef<'a> {
    id: CaudraId,
    content: &'a str,
    is_error: bool,
}

fn build_tool_results_map(items: &[HistoryItem]) -> HashMap<&str, ToolResultRef<'_>> {
    let mut map = HashMap::new();
    for item in items {
        if let HistoryItemKind::ToolResult {
            call_id,
            content,
            is_error,
            ..
        } = &item.kind
        {
            map.insert(
                call_id.as_str(),
                ToolResultRef {
                    id: item.id,
                    content,
                    is_error: *is_error,
                },
            );
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_agent::decisions::{NoteAnchor, NoteOutcome};
    use caudra_agent::peers::{
        AssignedWork, PeerSummary, PublishReceipt, QueuedWork, RecipientReceipt, SendReceipt,
    };
    use caudra_agent::restart::{StopCause, nudge};
    use caudra_agent::tools::native::peers::{LIST_NAME, PUBLISH_NAME, SEND_NAME, WORK_NAME};
    use caudra_agent::tools::{BATCH_TOOL_NAME, SHELL_TOOL_NAME, TOOL_OUTPUT_TOOL_NAME};
    use caudra_agent::types::{Answerer, TodoPriority, TodoStatus};
    use caudra_agent::{
        AgentEvent, BatchProgressEvent, BatchToolEntry, BatchToolStatus, IndexLine,
        IndexLineSemantic, IndexOutput, IndexSourceRange, PeerOutput, SharedBuf, TextOutput,
        ToolDoneEvent, ToolOutput, ToolStartEvent, TurnCompleteEvent,
    };
    use caudra_config::decisions::FeatureMode;
    use caudra_config::{InboundPolicy, UiConfig};
    use caudra_providers::{
        AutopilotOrigin, Billing, ContentBlock, ImageMediaType, Message, Model, PeerAudience,
        PeerMessageOrigin, ReasoningTransport, Role, StandingReminderKind, TaskEventOrigin,
        estimate_tokens_cached, project_messages, token_label,
    };
    use caudra_storage::engine_decisions::StoredDecisionOutcome;
    use caudra_workspace::PlanRef;
    use ratatui::{Terminal, backend::TestBackend};
    use std::hint::black_box;
    use std::time::Instant;
    use test_case::test_case;

    use crate::animation::test_clock::FrozenClock;
    use crate::components::card_decisions::DecisionState;
    use crate::components::{DECISION_SEPARATOR, SHADOW_MARKER};

    fn tool_start(id: &str, tool: &str) -> AgentEvent {
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: id.into(),
            effect: ToolEffect::Unknown,
            tool: tool.into(),
            summary: String::new(),
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
            render_header: None,
        }))
    }

    fn tool_done(id: &str, tool: &str, output: ToolOutput) -> AgentEvent {
        tool_done_with_written_path(id, tool, output, None)
    }

    fn tool_done_with_written_path(
        id: &str,
        tool: &str,
        output: ToolOutput,
        written_path: Option<String>,
    ) -> AgentEvent {
        AgentEvent::ToolDone(Box::new(ToolDoneEvent {
            id: id.into(),
            tool: tool.into(),
            output,
            is_error: false,
            annotation: None,
            written_path,
            written_paths: Vec::new(),
            remote_written_paths: false,
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
            documents: Vec::new(),
            accounting: caudra_agent::ToolAccounting::default(),
        }))
    }

    fn write_output(path: &str) -> (ToolOutput, Option<String>) {
        (
            ToolOutput::Plain(format!("wrote 42 bytes to {path}").into()),
            Some(path.to_owned()),
        )
    }

    fn edit_output(path: &str) -> ToolOutput {
        ToolOutput::Diff {
            path: path.into(),
            before: String::new(),
            after: String::new(),
            summary: String::new(),
        }
    }

    fn empty_outputs() -> HashMap<String, Arc<ToolOutput>> {
        HashMap::new()
    }

    fn display_messages(
        messages: &[Message],
        outputs: &HashMap<String, Arc<ToolOutput>>,
    ) -> (Vec<DisplayMessage>, Vec<caudra_lua::RestoreItem>) {
        history_to_display(
            &crate::history_items(messages),
            outputs,
            &ToolOutputLines::default(),
            true,
        )
    }

    const MAIN_NAME: &str = "Main";
    const COMMITTED_PLAN_CONTENT: &str = "# Committed plan\n\nUse this exact content.";
    const COMMITTED_PLAN_PATH: &str = "/unreadable/committed-plan.md";
    const COMMITTED_PLAN_REFERENCE: &str = "plan-committed";
    const SUBAGENT_NAME: &str = "research";
    const TASK_ID: &str = "toolu_01";
    const USER_TEXT: &str = "one more thing";
    const REPLY_TEXT: &str = "on it";
    const INJECTED_TEXT: &str = "# Environment\n\ncwd: /tmp";
    const RESUMED_SUMMARY: &str = "main turn";
    const SYNTHETIC_TEXT: &str = "# Goal check-in";
    const BACKGROUND_REMINDER: &str =
        "<system-reminder>\n# Background work\n\nNo active background work.\n</system-reminder>";
    const MENTION_BODY: &str = "<file path=\"a.rs\">fn main() {}</file>";
    const INDEX_SKELETON: &str = "fns:\n  pub run() [2]";
    const INDEX_ANNOTATION: &str = "2 lines";
    const PEER_TARGET: &str = "calm-blue-wren";
    const PEER_MESSAGE: &str = "kind-amber-fox";
    const PEER_REASON: &str = "Needs local review.";
    const PEER_TOPIC: &str = "ci.failures";
    const PEER_GROUP: &str = "ci-triage";
    const PEER_WORK: &str = "steady-warm-heron";
    const PEER_MODEL_JSON: &str = "{\"session_id\":\"private-session-id\"}";
    const SESSION_CWD: &str = "/project";
    const TASK_OBSERVATION: &str = "neat-wanted-cowbird completed: All checks passed.";
    const PRIVATE_INVOCATION: &str = "private-invocation";
    const PRIVATE_EVENT: &str = "private-event";
    const DELIVERY_TASK: &str = "neat-wanted-cowbird";
    const DELIVERY_SUCCESS: &str = "All checks passed.";
    const WAIT_NUDGE_TEXT: &str =
        "<system-reminder>\n# Long-running shell\n\nStill running.\n</system-reminder>";
    const DELIVERY_BLOCKER: &str = "Permission is needed before continuing.";
    const DELIVERY_PREVIEW: &str =
        "First useful line.\nResult truncated; tool_output calm-blue-wren";
    const DELIVERY_UNICODE: &str = "調査の結果は正常です。界界界界界界界界界界界界界界界界界界界界界界界界界界界界界界界界界界界界";
    const AUTOMATION_NAME: &str = "ci-watch";
    const AUTOMATION_FIRE_ID: &str = "fire-quiet-otter";
    const AUTOMATION_SEQ: u32 = 2;
    const AUTOMATION_TEXT: &str = "The nightly build failed on main.";
    const AUTOMATION_FRAMED: &str =
        "<automation name=\"ci-watch\">The nightly build failed on main.</automation>";
    const TODO_TEXT: &str = "Verify the task-local todo list";
    const TODO_CALL: &str = "todo-update";
    const BLANK_USER_TEXT: &str = " \t\n";
    const FIRST_IMAGE: &str = "Zmlyc3Q=";
    const SECOND_IMAGE: &str = "c2Vjb25k";
    const THIRD_IMAGE: &str = "dGhpcmQ=";
    const NESTED_CALL_CHILD: &str = "t2:0:1";
    const INVALID_CALL_CHILD: &str = "t2:not-an-index";
    const LOAD_SCALING_BASE: usize = 1_000;

    fn chat() -> Chat {
        Chat::new(
            MAIN_NAME.into(),
            Path::new(SESSION_CWD),
            UiConfig::default(),
            caudra_lua::EventHandle::disconnected_for_test(),
        )
    }

    fn subagent_chat() -> Chat {
        Chat::subagent(
            TASK_ID,
            SUBAGENT_NAME.into(),
            Path::new(SESSION_CWD),
            UiConfig::default(),
            caudra_lua::EventHandle::disconnected_for_test(),
        )
    }

    fn end(chat: &mut Chat, outcome: TaskOutcome) {
        let text = match outcome {
            TaskOutcome::Killed => CANCELLED_TEXT,
            TaskOutcome::Error => ERROR_TEXT,
            TaskOutcome::Unknown | TaskOutcome::Done => DONE_TEXT,
        };
        chat.mark_finished(outcome, text);
    }

    fn text_delta(chat: &mut Chat, text: &str) {
        chat.handle_event(AgentEvent::TextDelta { text: text.into() }, None);
    }

    #[test_case(false; "main")]
    #[test_case(true; "task")]
    fn todo_snapshots_match_live_and_restored_replace_all_updates(task: bool) {
        let mut live = if task { subagent_chat() } else { chat() };
        let mut restored = if task { subagent_chat() } else { chat() };
        let items = vec![TodoItem {
            content: TODO_TEXT.into(),
            status: TodoStatus::Pending,
            priority: TodoPriority::default(),
        }];
        let mut history = Vec::new();
        for items in [items.clone(), Vec::new(), items] {
            let output = ToolOutput::TodoList(items.clone());
            live.handle_event(tool_done(TODO_CALL, "todo_write", output.clone()), None);
            let messages = tool_use_pair("todo_write", serde_json::json!({}), "ok", false);
            let outputs = HashMap::from([("t1".into(), Arc::new(output))]);
            history.extend(display_messages(&messages, &outputs).0);
            history.push(DisplayMessage::new(
                DisplayRole::Assistant,
                REPLY_TEXT.into(),
            ));
            restored.load_messages(history.clone());
            assert_eq!(live.todos(), items);
            assert_eq!(restored.todos(), live.todos());
        }
        restored.load_messages(vec![DisplayMessage::new(
            DisplayRole::User,
            USER_TEXT.into(),
        )]);
        assert!(restored.todos().is_empty());
    }

    #[test]
    fn tool_lifecycle() {
        let mut chat = chat();
        chat.handle_event(tool_start("t1", "shell"), None);
        assert_eq!(chat.in_progress_count(), 1);

        chat.handle_event(
            tool_done("t1", "shell", ToolOutput::Plain("ok".into())),
            None,
        );
        assert_eq!(chat.in_progress_count(), 0);
    }

    const CHILD_STREAM_MSG: &str =
        "a batch child's streamed output belongs to the roster row, not to a header";
    const HEADER_STREAM_MSG: &str = "an id that merely ends in a number still names its own card";
    const CHILD_TAIL: &str = "building";
    const LIVE_PHASE: &str = "ranking 12 files";
    const LIVE_PARENT_ID: &str = "batch:source";
    const LIVE_CHILD_ID: &str = "batch:source:0";

    fn batch_start(id: &str, children: usize) -> AgentEvent {
        let AgentEvent::ToolStart(mut ev) = tool_start(id, BATCH_TOOL_NAME) else {
            unreachable!("tool_start builds a ToolStart");
        };
        ev.output = Some(ToolOutput::Batch {
            entries: (0..children)
                .map(|_| BatchToolEntry {
                    model_suffix: None,
                    tool: SHELL_TOOL_NAME.into(),
                    effect: ToolEffect::Unknown,
                    summary: String::new(),
                    status: BatchToolStatus::Running,
                    input: None,
                    raw_input: None,
                    output: None,
                    annotation: None,
                    refused: false,
                    accounting: Default::default(),
                })
                .collect(),
            text: String::new(),
        });
        AgentEvent::ToolStart(ev)
    }

    fn streamed(id: &str, content: &str) -> AgentEvent {
        AgentEvent::ToolOutput {
            id: id.into(),
            content: content.into(),
        }
    }

    /// The reported bug. A batch runs each child under its own id with an
    /// index appended, so a dispatched shell's output was addressed to a
    /// header that does not exist and the panel dropped it.
    #[test]
    fn a_batch_childs_streamed_output_reaches_its_roster_row() {
        let mut chat = chat();
        chat.handle_event(batch_start("t1", 2), None);
        chat.handle_event(streamed("t1:1", CHILD_TAIL), None);

        assert_eq!(
            chat.messages_panel.batch_child_stream("t1", 1),
            Some(CHILD_TAIL),
            "{CHILD_STREAM_MSG}"
        );
    }

    /// The suffix is only dropped when it resolves to a real child, so a tool
    /// whose own id ends in a colon and a number keeps its own body.
    #[test]
    fn an_id_that_is_not_a_batch_child_still_streams_to_its_own_card() {
        let mut chat = chat();
        chat.handle_event(tool_start("t:1", SHELL_TOOL_NAME), None);
        chat.handle_event(streamed("t:1", CHILD_TAIL), None);

        assert_eq!(
            chat.messages_panel.batch_child_stream("t", 1),
            None,
            "{HEADER_STREAM_MSG}"
        );
    }

    #[test_case(false, false ; "local_top_level")]
    #[test_case(false, true ; "remote_top_level")]
    #[test_case(true, false ; "local_batch_child")]
    #[test_case(true, true ; "remote_batch_child")]
    fn generic_live_events_reach_their_execution_slot(child: bool, remote: bool) {
        let mut chat = chat();
        let id = if child { LIVE_CHILD_ID } else { LIVE_PARENT_ID };
        chat.handle_event(
            if child {
                batch_start(LIVE_PARENT_ID, 1)
            } else {
                tool_start(id, SHELL_TOOL_NAME)
            },
            None,
        );
        if remote {
            let body = Arc::new(SharedBuf::new());
            body.set_lines(
                BufferSnapshot::plain_text(CHILD_TAIL.into())
                    .lines
                    .as_ref()
                    .clone(),
            );
            chat.handle_event(
                AgentEvent::LiveToolBuf {
                    id: id.into(),
                    body,
                },
                None,
            );
            let _ = chat.tick();
        } else {
            chat.handle_event(streamed(id, CHILD_TAIL), None);
        }
        chat.handle_event(
            AgentEvent::ToolAnnotation {
                id: id.into(),
                annotation: LIVE_PHASE.into(),
            },
            None,
        );
        let msg = chat.messages_panel.message_at(0).unwrap();
        if child {
            assert_eq!(
                chat.messages_panel.batch_child_stream(LIVE_PARENT_ID, 0),
                Some(CHILD_TAIL)
            );
            let Some(ToolOutput::Batch { entries, .. }) = msg.tool_output.as_deref() else {
                panic!("{CHILD_STREAM_MSG}");
            };
            assert_eq!(entries[0].annotation.as_deref(), Some(LIVE_PHASE));
            assert!(msg.annotation.is_none());
        } else {
            assert_eq!(msg.live_output.as_deref(), Some(CHILD_TAIL));
            assert_eq!(msg.annotation.as_deref(), Some(LIVE_PHASE));
        }
    }

    /// The plan card renders the same file the write card does, so exactly one
    /// of them draws it.
    const PLAN_DUPLICATION_MSG: &str = "a plan write should fall back to its header";
    const PLAN_DIFF_MSG: &str = "a plan edit is a diff the plan card does not carry";

    #[test]
    fn plan_write_renders_file_content() {
        let mut chat = chat();
        let dir = tempfile::tempdir().unwrap();
        let plan_path = dir.path().join("plan.md");
        std::fs::write(&plan_path, "# My Plan\n\n- Step 1").unwrap();
        let plan_str = plan_path.to_str().unwrap();

        chat.handle_event(tool_start("w1", "file_write"), Some(plan_path.as_path()));
        let (output, wp) = write_output(plan_str);
        chat.handle_event(
            tool_done_with_written_path("w1", "file_write", output, wp),
            Some(plan_path.as_path()),
        );

        assert!(chat.last_message_is_plan());
        let last = chat.last_message_text();
        assert!(last.contains("# My Plan"));
        assert!(
            chat.messages_panel.card_closed("w1"),
            "{PLAN_DUPLICATION_MSG}"
        );
    }

    #[test]
    fn plan_write_ignores_different_path() {
        let mut chat = chat();
        let plan_path = Path::new("/plans/123.md");
        chat.handle_event(tool_start("w1", "file_write"), Some(plan_path));
        let (output, wp) = write_output("src/main.rs");
        chat.handle_event(
            tool_done_with_written_path("w1", "file_write", output, wp),
            Some(plan_path),
        );
        assert!(!chat.last_message_is_plan());
    }

    #[test]
    fn plan_edit_shows_path_only() {
        let mut chat = chat();
        let dir = tempfile::tempdir().unwrap();
        let plan_path = dir.path().join("plan.md");
        std::fs::write(&plan_path, "# My Plan\n\n- Step 1").unwrap();
        let plan_str = plan_path.to_str().unwrap();

        chat.handle_event(tool_start("e1", "file_edit"), Some(plan_path.as_path()));
        chat.handle_event(
            tool_done("e1", "file_edit", edit_output(plan_str)),
            Some(plan_path.as_path()),
        );

        assert!(chat.last_message_is_plan());
        assert!(chat.last_message_text().is_empty());
        assert!(!chat.messages_panel.card_closed("e1"), "{PLAN_DIFF_MSG}");
    }

    fn committed_plan(remote: bool) -> PlanWriteResult {
        let target = if remote {
            PlanTarget::Remote(PlanRef::new(COMMITTED_PLAN_REFERENCE).unwrap())
        } else {
            PlanTarget::Local(COMMITTED_PLAN_PATH.into())
        };
        PlanWriteResult::new(target, COMMITTED_PLAN_CONTENT.into())
    }

    #[test_case(false ; "local")]
    #[test_case(true ; "remote")]
    fn native_plan_write_draws_one_committed_card_without_reading_storage(remote: bool) {
        let mut chat = chat();
        let result = committed_plan(remote);
        chat.handle_event(tool_start("w1", plan::NAME), None);
        let mut done = ToolDoneEvent::error("w1".into(), COMMITTED_PLAN_CONTENT);
        done.tool = plan::NAME.into();
        done.is_error = false;
        done.output = ToolOutput::Markdown(COMMITTED_PLAN_CONTENT.into());
        done.annotation = Some(result.annotation().unwrap());
        chat.handle_event(
            AgentEvent::ToolDone(Box::new(done)),
            Some(Path::new(COMMITTED_PLAN_PATH)),
        );
        chat.plan_written("w1", &result);
        assert_eq!(chat.message_count(), 2);
        assert!(
            chat.messages_panel.card_closed("w1"),
            "{PLAN_DUPLICATION_MSG}"
        );
        assert!(chat.last_message_is_plan());
        assert_eq!(chat.last_message_text(), COMMITTED_PLAN_CONTENT);
        assert_eq!(
            chat.message_at(1).unwrap().plan_path.as_deref(),
            Some(if remote {
                plan::SESSION_PLAN_LABEL
            } else {
                COMMITTED_PLAN_PATH
            })
        );
        assert!(
            !chat
                .message_at(0)
                .unwrap()
                .annotation
                .as_deref()
                .unwrap()
                .contains(plan::WRITE_RESULT_PREFIX)
        );
    }

    #[test_case(false, false ; "local_success")]
    #[test_case(true, false ; "remote_success")]
    #[test_case(false, true ; "local_failure")]
    #[test_case(true, true ; "remote_failure")]
    fn persisted_plan_write_restores_one_captured_card(remote: bool, error: bool) {
        let result = committed_plan(remote);
        let output = ToolOutput::Markdown(TextOutput {
            state: Some(result.annotation().unwrap().into()),
            ..COMMITTED_PLAN_CONTENT.into()
        });
        let stored: ToolOutput =
            serde_json::from_str(&serde_json::to_string(&output).unwrap()).unwrap();
        let messages = tool_use_pair(
            plan::NAME,
            serde_json::json!({"action": "write", "content": COMMITTED_PLAN_CONTENT}),
            "Active plan saved.",
            error,
        );
        let outputs = HashMap::from([("t1".into(), Arc::new(stored))]);
        let (display, restores) = display_messages(&messages, &outputs);
        assert_eq!(display.len(), if error { 1 } else { 2 });
        if !error {
            assert_eq!(display[0].body_open, Some(false));
            assert_eq!(display[1].text, COMMITTED_PLAN_CONTENT);
            assert_eq!(display[1].plan_path, plan_message(&result).plan_path);
            assert!(restores.is_empty());
        }
    }

    #[test_case(false; "local")]
    #[test_case(true; "remote")]
    fn batch_plan_completion_and_replay_keep_one_native_body(remote: bool) {
        let mut live = chat();
        live.handle_event(batch_start("t1", 2), None);
        let plan = committed_plan(remote);
        let entry = BatchToolEntry {
            tool: plan::NAME.into(),
            effect: ToolEffect::Mutating,
            summary: String::new(),
            status: BatchToolStatus::Success,
            input: None,
            raw_input: None,
            output: Some(ToolOutput::Markdown(TextOutput {
                state: Some(plan.annotation().unwrap().into()),
                ..COMMITTED_PLAN_CONTENT.into()
            })),
            annotation: None,
            model_suffix: None,
            refused: false,
            accounting: Default::default(),
        };
        for index in [1, 0] {
            let event = || {
                AgentEvent::BatchProgress(Box::new(BatchProgressEvent {
                    id: "t1".into(),
                    index,
                    entry: entry.clone(),
                }))
            };
            assert!(
                matches!(live.handle_event(event(), None), ChatEventResult::PlanWritten(written) if written == plan)
            );
            assert!(matches!(
                live.handle_event(event(), None),
                ChatEventResult::Continue
            ));
        }
        let output = ToolOutput::Batch {
            entries: vec![entry.clone(), entry],
            text: String::new(),
        };
        live.handle_event(tool_done("t1", BATCH_TOOL_NAME, output.clone()), None);
        assert_eq!(live.message_count(), 1);
        let restored: ToolOutput =
            serde_json::from_str(&serde_json::to_string(&output).unwrap()).unwrap();
        let messages = tool_use_pair(BATCH_TOOL_NAME, serde_json::json!({}), "saved", false);
        let outputs = HashMap::from([("t1".into(), Arc::new(restored))]);
        let (display, restores) = display_messages(&messages, &outputs);
        assert_eq!(display.len(), 1);
        assert!(restores.is_empty());
        let ToolOutput::Batch { entries, .. } = display[0].tool_output.as_deref().unwrap() else {
            panic!("expected batch output");
        };
        assert_eq!(entries.len(), 2);
        for entry in entries {
            assert_eq!(entry.plan_write_result(), Some(plan.clone()));
        }
    }

    #[test]
    fn history_skips_empty_text() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: String::new(),
            }],
            ..Default::default()
        }];
        assert!(display_messages(&msgs, &empty_outputs()).0.is_empty());
    }

    #[test]
    fn history_sources_keep_atomic_ids_on_merged_tools() {
        let messages = vec![
            Message::user("do it".into()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::thinking("inspect".into(), None),
                    ContentBlock::Text {
                        text: "running".into(),
                    },
                    ContentBlock::tool_use("t1", "shell", serde_json::json!({"command": "true"})),
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "ok".into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            },
        ];
        let items = crate::history_items(&messages);

        let display =
            history_to_display(&items, &empty_outputs(), &ToolOutputLines::default(), true).0;

        assert_eq!(display[0].source, Some(DisplaySource::User(items[0].id)));
        assert_eq!(
            display[1].source,
            Some(DisplaySource::Reasoning(items[1].id))
        );
        assert_eq!(
            display[2].source,
            Some(DisplaySource::AssistantText(items[2].id))
        );
        assert_eq!(
            display[3].source,
            Some(DisplaySource::ToolCall {
                id: items[3].id,
                result_id: Some(items[4].id),
            })
        );
    }

    fn tool_use_pair(
        tool: &str,
        input: serde_json::Value,
        result: &str,
        is_error: bool,
    ) -> Vec<Message> {
        vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("t1", tool, input)],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: result.into(),
                    is_error,
                    output_ref: None,
                }],
                ..Default::default()
            },
        ]
    }

    #[test_case(false, ToolStatus::Success ; "success")]
    #[test_case(true,  ToolStatus::Error   ; "error")]
    fn history_tool_result_status(is_error: bool, expected: ToolStatus) {
        let msgs = tool_use_pair(
            "shell",
            serde_json::json!({"command": "ls"}),
            "output",
            is_error,
        );
        let display = display_messages(&msgs, &empty_outputs()).0;
        assert_eq!(display.len(), 1);
        assert!(matches!(&display[0].role, DisplayRole::Tool(t) if t.status == expected));
    }

    #[test]
    fn restored_native_index_keeps_rust_renderer_without_lua_restore() {
        let messages = tool_use_pair(
            "file_index",
            serde_json::json!({"path": "src/lib.rs"}),
            INDEX_SKELETON,
            false,
        );
        let output = ToolOutput::Index(IndexOutput::File {
            path: "/project/src/lib.rs".into(),
            relative_path: "src/lib.rs".into(),
            language: "rust".into(),
            skeleton: INDEX_SKELETON.into(),
            lines: vec![
                IndexLine {
                    output_line: 1,
                    text: "fns:".into(),
                    semantic: IndexLineSemantic::Section,
                    body: None,
                    source_range: None,
                },
                IndexLine {
                    output_line: 2,
                    text: "  pub run() [2]".into(),
                    semantic: IndexLineSemantic::Item,
                    body: Some("  pub run()".into()),
                    source_range: Some(IndexSourceRange {
                        start_line: 2,
                        end_line: 2,
                    }),
                },
            ],
            source_line_count: 2,
            parse_error: false,
            truncated: false,
            instructions: None,
            state: Some(serde_json::json!({"kind": "file"})),
        });
        let annotation = output.annotation(TokenScale::UNIT);
        let outputs = HashMap::from([("t1".into(), Arc::new(output))]);

        let (display, restores) = display_messages(&messages, &outputs);

        assert!(restores.is_empty());
        assert!(matches!(
            display[0].tool_output.as_deref(),
            Some(ToolOutput::Index(_))
        ));
        assert_eq!(display[0].annotation, annotation);
        assert!(
            annotation.unwrap_or_default().starts_with(INDEX_ANNOTATION),
            "a restored index still says how many lines it holds"
        );
    }

    fn restored_discovery() -> PeerOutput {
        PeerOutput::Sessions {
            sessions: vec![PeerSummary {
                target: PEER_TARGET.into(),
                title: MAIN_NAME.into(),
                handle: None,
                cwd: SESSION_CWD.into(),
                busy: false,
                blocked: false,
                inbound: InboundPolicy::Hold,
                topics: vec![PEER_TOPIC.into()],
                broadcasts: true,
                groups: vec![PEER_GROUP.into()],
            }],
        }
    }

    fn restored_receipt() -> PeerOutput {
        PeerOutput::Sent {
            target: PEER_TARGET.into(),
            receipt: SendReceipt {
                status: "held".into(),
                message_id: PEER_MESSAGE.into(),
                reason: Some(PEER_REASON.into()),
            },
        }
    }

    fn restored_publication() -> PeerOutput {
        PeerOutput::Published {
            receipt: PublishReceipt {
                message_id: PEER_MESSAGE.into(),
                audience: PeerAudience::Topic {
                    topic: PEER_TOPIC.into(),
                },
                recipients: vec![RecipientReceipt {
                    target: PEER_TARGET.into(),
                    title: MAIN_NAME.into(),
                    handle: None,
                    status: "held".into(),
                    reason: Some(PEER_REASON.into()),
                }],
                skipped: 0,
                queued: vec![QueuedWork {
                    group: PEER_GROUP.into(),
                    work: PEER_WORK.into(),
                }],
            },
        }
    }

    fn restored_report() -> PeerOutput {
        PeerOutput::Reported {
            work: AssignedWork {
                work: PEER_WORK.into(),
                group: PEER_GROUP.into(),
                state: "completed".into(),
                attempt: 1,
                max_attempts: 3,
                topic: Some(PEER_TOPIC.into()),
                publisher: MAIN_NAME.into(),
                reason: None,
                result: Some(PEER_REASON.into()),
            },
        }
    }

    #[test_case(LIST_NAME, serde_json::json!({}), restored_discovery(), &[PEER_TARGET, SESSION_CWD, MAIN_NAME, PEER_TOPIC, PEER_GROUP]; "discovery")]
    #[test_case(SEND_NAME, serde_json::json!({"target": PEER_TARGET, "text": PEER_REASON}), restored_receipt(), &[PEER_TARGET, PEER_MESSAGE, PEER_REASON]; "receipt")]
    #[test_case(PUBLISH_NAME, serde_json::json!({"topic": PEER_TOPIC, "text": PEER_REASON}), restored_publication(), &[PEER_TARGET, PEER_MESSAGE, PEER_REASON, PEER_TOPIC, MAIN_NAME, PEER_GROUP, PEER_WORK]; "publication")]
    #[test_case(WORK_NAME, serde_json::json!({"action": "complete", "work": PEER_WORK}), restored_report(), &[PEER_WORK, PEER_GROUP, PEER_TOPIC, MAIN_NAME, PEER_REASON]; "work_report")]
    fn restored_peers_keep_typed_cards_and_display_text_without_lua(
        tool: &str,
        input: serde_json::Value,
        peer: PeerOutput,
        expected: &[&str],
    ) {
        let annotation = peer.annotation();
        let serialized = serde_json::to_string(&ToolOutput::Peers(peer)).unwrap();
        let restored: ToolOutput = serde_json::from_str(&serialized).unwrap();
        let outputs = HashMap::from([("t1".into(), Arc::new(restored))]);
        let messages = tool_use_pair(tool, input, PEER_MODEL_JSON, false);
        let (display, restores) = display_messages(&messages, &outputs);
        assert!(restores.is_empty());
        assert!(matches!(
            display[0].tool_output.as_deref(),
            Some(ToolOutput::Peers(_))
        ));
        assert_eq!(display[0].annotation.as_deref(), Some(annotation.as_str()));
        assert!(!display[0].text.contains(PEER_MODEL_JSON));
        assert!(restore_item_for(&display[0], ToolOutputLines::DEFAULT, 0).is_none());
        let copied = display[0]
            .tool_output
            .as_deref()
            .unwrap()
            .structured_display_text()
            .unwrap();
        assert!(!copied.contains(PEER_MODEL_JSON));
        assert!(!copied.contains("session_id"));
        for expected in expected {
            assert!(copied.contains(expected), "{copied}");
        }
    }

    #[test]
    fn index_without_stored_output_falls_back_to_plain_text_and_a_lua_restore() {
        let messages = tool_use_pair(
            "file_index",
            serde_json::json!({"path": "src/lib.rs"}),
            INDEX_SKELETON,
            false,
        );

        let (display, restores) = display_messages(&messages, &empty_outputs());

        assert!(display[0].tool_output.is_none());
        assert!(display[0].text.contains(INDEX_SKELETON));
        assert_eq!(restores.len(), 1);
        assert_eq!(restores[0].output, INDEX_SKELETON);
    }

    #[test]
    fn history_mixed_conversation() {
        let msgs = vec![
            Message::user("do something".into()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "Sure, let me help.".into(),
                    },
                    ContentBlock::tool_use(
                        "t1",
                        "shell",
                        serde_json::json!({"command": "echo hi"}),
                    ),
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "hi".into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "Done!".into(),
                }],
                ..Default::default()
            },
        ];
        let display = display_messages(&msgs, &empty_outputs()).0;
        assert_eq!(display.len(), 4);
        assert_eq!(display[0].role, DisplayRole::User);
        assert_eq!(display[1].role, DisplayRole::Assistant);
        assert!(matches!(display[2].role, DisplayRole::Tool(_)));
        assert_eq!(display[3].role, DisplayRole::Assistant);
        assert_eq!(display[3].text, "Done!");
    }

    #[test]
    fn history_stored_output_variants_pass_through() {
        let variants: Vec<(&str, serde_json::Value, ToolOutput)> = vec![
            (
                "file_edit",
                serde_json::json!({"path": "a", "old_string": "x", "new_string": "y"}),
                ToolOutput::Diff {
                    path: "a".into(),
                    before: "x\n".into(),
                    after: "y\n".into(),
                    summary: "edited a".into(),
                },
            ),
            (
                "file_read",
                serde_json::json!({"path": "/src/main.rs"}),
                ToolOutput::ReadCode {
                    path: "/src/main.rs".into(),
                    start_line: 1,
                    lines: vec!["fn main() {}".into()],
                    total_lines: 1,
                    instructions: None,
                },
            ),
            (
                "file_grep",
                serde_json::json!({"pattern": "TODO"}),
                ToolOutput::GrepResult {
                    entries: vec![],
                    capped: None,
                },
            ),
            (
                "todo_write",
                serde_json::json!({"todos": []}),
                ToolOutput::Plain("Todos cleared".into()),
            ),
        ];
        for (tool_name, input_json, output) in variants {
            let discriminant = std::mem::discriminant(&output);
            let msgs = tool_use_pair(tool_name, input_json, "ok", false);
            let outputs = HashMap::from([("t1".into(), Arc::new(output))]);
            let display = display_messages(&msgs, &outputs).0;
            assert_eq!(
                std::mem::discriminant(display[0].tool_output.as_deref().unwrap()),
                discriminant,
                "stored {tool_name} output should pass through"
            );
        }
    }

    #[test]
    fn history_stored_write_has_annotation() {
        let write_output = ToolOutput::WriteCode {
            path: "/src/main.rs".into(),
            byte_count: 12,
            lines: vec!["fn main() {}".into()],
        };
        let msgs = tool_use_pair(
            "file_write",
            serde_json::json!({"path": "/src/main.rs", "content": "fn main() {}"}),
            "wrote 12 bytes",
            false,
        );
        let outputs = HashMap::from([("t1".into(), Arc::new(write_output))]);
        let display = display_messages(&msgs, &outputs).0;
        assert!(display[0].annotation.is_some());
    }

    const RETRIEVED_OUTPUT_ID: &str = "output-shell-1";
    const RETRIEVED_TEXT: &str = "Output ID: output-shell-1\n1: retrieved line";
    const RETRIEVED_PATTERN: &str = "retrieved";
    const RETRIEVED_ANNOTATION_MSG: &str =
        "live and restored cards derive the loaded annotation from persisted output state";

    #[test_case(false, false ; "read")]
    #[test_case(false, true ; "search")]
    #[test_case(true, true ; "batch_search")]
    fn retrieval_annotations_survive_session_replay(batched: bool, search: bool) {
        let output = ToolOutput::Plain(TextOutput {
            state: Some(serde_json::json!({ "kind": TOOL_OUTPUT_TOOL_NAME })),
            ..RETRIEVED_TEXT.into()
        });
        let mut input = serde_json::json!({ "output_id": RETRIEVED_OUTPUT_ID });
        let summary = if search {
            input["pattern"] = RETRIEVED_PATTERN.into();
            format!("{RETRIEVED_PATTERN} in {RETRIEVED_OUTPUT_ID}")
        } else {
            RETRIEVED_OUTPUT_ID.into()
        };
        let (tool, input, output) = if batched {
            let batch_input = serde_json::json!({
                "tool_calls": [{ "tool": TOOL_OUTPUT_TOOL_NAME, "parameters": input }]
            });
            let output = ToolOutput::Batch {
                entries: vec![BatchToolEntry {
                    tool: TOOL_OUTPUT_TOOL_NAME.into(),
                    effect: ToolEffect::Unknown,
                    summary,
                    status: BatchToolStatus::Success,
                    input: None,
                    raw_input: Some(input),
                    output: Some(output),
                    annotation: None,
                    model_suffix: None,
                    refused: false,
                    accounting: Default::default(),
                }],
                text: RETRIEVED_TEXT.into(),
            };
            (BATCH_TOOL_NAME, batch_input, output)
        } else {
            (TOOL_OUTPUT_TOOL_NAME, input, output)
        };
        let stored: ToolOutput =
            serde_json::from_value(serde_json::to_value(&output).unwrap()).unwrap();
        let mut live = chat();
        live.handle_event(tool_start("t1", tool), None);
        live.handle_event(tool_done("t1", tool, output), None);
        let messages = tool_use_pair(tool, input, RETRIEVED_TEXT, false);
        let outputs = HashMap::from([("t1".into(), Arc::new(stored))]);
        let mut replayed = chat();
        replayed.load_messages(display_messages(&messages, &outputs).0);
        let annotation = format!(
            "(2 lines · {} loaded)",
            token_label(estimate_tokens_cached(RETRIEVED_TEXT))
        );
        let area = Rect::new(0, 0, 200, 20);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        for mut card in [live, replayed] {
            card.set_view(ViewMode::Expanded);
            terminal
                .draw(|frame| card.view(frame, area, false, false))
                .unwrap();
            let shown: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(shown.contains("Retrieved output"), "{shown}");
            assert_eq!(
                shown.matches(&annotation).count(),
                1,
                "{RETRIEVED_ANNOTATION_MSG}: {shown}"
            );
        }
    }

    #[test]
    fn history_bash_output_truncated() {
        let long_output = (0..200).map(|i| format!("line {i}")).collect::<Vec<_>>();
        let joined = long_output.join("\n");
        let msgs = tool_use_pair(
            "shell",
            serde_json::json!({"command": "cmd"}),
            &joined,
            false,
        );
        let display = display_messages(&msgs, &empty_outputs()).0;
        let line_count = display[0].text.lines().count();
        assert!(
            line_count < long_output.len(),
            "output should be truncated, got {line_count} lines for {} input lines",
            long_output.len()
        );
    }
    #[test]
    fn history_no_stored_output_falls_back_to_plain_text() {
        let msgs = tool_use_pair(
            "file_read",
            serde_json::json!({"path": "/src/main.rs"}),
            "1: fn main() {}",
            false,
        );
        let display = display_messages(&msgs, &empty_outputs()).0;
        assert!(display[0].tool_output.is_none());
        assert!(display[0].text.contains("fn main"));
    }

    /// Restoring a session re-parses the stored call, so the command echo above
    /// a tool's output survives a reload. It used to be dropped outright.
    #[test]
    fn a_restored_tool_call_keeps_its_input_echo() {
        use caudra_agent::tools::registry::{
            ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolExecResult,
            ToolInvocation,
        };
        use caudra_agent::tools::{DescriptionContext, ToolContext};
        use serde_json::{Value, json};

        const TOOL: &str = "restore_echo_probe";
        const COMMAND: &str = "echo restored";

        struct Probe;
        struct Call(String);

        impl Tool for Probe {
            fn name(&self) -> &str {
                TOOL
            }
            fn description(&self, _ctx: &DescriptionContext) -> std::borrow::Cow<'_, str> {
                std::borrow::Cow::Borrowed("")
            }
            fn schema(&self) -> Value {
                json!({ "type": "object" })
            }
            fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
                Ok(Box::new(Call(
                    input["command"].as_str().unwrap_or_default().to_owned(),
                )))
            }
        }

        impl ToolInvocation for Call {
            fn start_header(&self) -> HeaderFuture {
                HeaderFuture::Ready(HeaderResult::plain(String::new()))
            }
            fn start_input(&self) -> Option<caudra_agent::ToolInput> {
                Some(caudra_agent::ToolInput::Code {
                    language: "bash".into(),
                    code: self.0.clone(),
                })
            }
            fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
                Box::pin(async move {
                    ToolExecResult::from(Ok(ToolOutput::Plain(String::new().into())))
                })
            }
        }

        caudra_agent::tools::ToolRegistry::global()
            .register(
                Arc::new(Probe),
                caudra_agent::tools::registry::ToolSource::Native {
                    owner: "test".into(),
                    contract: TOOL.into(),
                    trusted: true,
                },
            )
            .expect("probe registers once");

        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "call-1",
                TOOL,
                json!({ "command": COMMAND }),
            )],
            ..Default::default()
        }];
        let display = display_messages(&messages, &HashMap::new()).0;
        let tool = display
            .iter()
            .find(|msg| matches!(msg.role, DisplayRole::Tool(_)))
            .expect("the call is displayed");
        let Some(caudra_agent::ToolInput::Code { code, .. }) = tool.tool_input.as_deref() else {
            panic!("a restored call keeps its code echo");
        };
        assert_eq!(code, COMMAND);
    }

    #[test]
    fn history_to_display_thinking_blocks() {
        let msgs = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::thinking("reasoning".into(), None),
                ContentBlock::Text {
                    text: "answer".into(),
                },
                ContentBlock::RedactedThinking { data: "x".into() },
            ],
            ..Default::default()
        }];
        let display = display_messages(&msgs, &HashMap::new()).0;
        assert_eq!(display.len(), 2);
        assert_eq!(display[0].role, DisplayRole::Thinking);
        assert_eq!(display[0].text, "reasoning");
        assert_eq!(display[1].role, DisplayRole::Assistant);
    }

    /// Live, the border rides on the Compacting event. A restored session
    /// replays history instead, so without this the summary butts straight
    /// against the reply it replaced everything after.
    #[test]
    fn history_to_display_marks_the_compaction_border() {
        let mut summary = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "## Objective".into(),
            }],
            ..Default::default()
        };
        summary.is_compaction_summary = true;
        let msgs = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "earlier reply".into(),
                }],
                ..Default::default()
            },
            Message::synthetic(COMPACTION_ANCHOR.into()),
            summary,
        ];

        let display = display_messages(&msgs, &HashMap::new()).0;

        assert_eq!(display.len(), 3);
        assert_eq!(display[0].role, DisplayRole::Assistant);
        assert_eq!(display[1].role, DisplayRole::Notice);
        assert_eq!(display[1].text, COMPACTION_BORDER_TEXT);
        assert_eq!(display[2].role, DisplayRole::Assistant);
        assert_eq!(display[2].text, "## Objective");
    }

    /// Live, each reminder arrives as its own `Injected` event. A restored
    /// session replays history instead, so without this the reader loses every
    /// reminder the moment they reopen the session that received it.
    ///
    /// A run writes its reminders after the message that triggered them, which
    /// is the order the reader watched them land in, so the replay is history's
    /// own order with the mention body dropped.
    #[test]
    fn history_to_display_replays_injected_messages_in_history_order() {
        let msgs = vec![
            Message::user(USER_TEXT.into()),
            Message::observation(INJECTED_TEXT.into()),
            Message::mention(MENTION_BODY.into()),
            Message::synthetic(SYNTHETIC_TEXT.into()),
        ];
        let items = crate::history_items(&msgs);

        let display =
            history_to_display(&items, &empty_outputs(), &ToolOutputLines::default(), true).0;

        let rows: Vec<(&DisplayRole, &str)> = display
            .iter()
            .map(|message| (&message.role, message.text.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                (&DisplayRole::User, USER_TEXT),
                (&DisplayRole::Injected, INJECTED_TEXT),
                (&DisplayRole::Injected, SYNTHETIC_TEXT),
            ]
        );
    }

    fn grouped_user_items(specs: &[(UserOrigin, &str, Option<&str>)]) -> Vec<HistoryItem> {
        let messages: Vec<_> = specs
            .iter()
            .map(|_| Message::user(USER_TEXT.into()))
            .collect();
        let mut items = crate::history_items(&messages);
        let group_id = items[0].group_id;
        for (item, &(origin, text, display_text)) in items.iter_mut().zip(specs) {
            item.group_id = group_id;
            let HistoryItemKind::User {
                origin: saved_origin,
                text: saved_text,
                display_text: saved_display,
                ..
            } = &mut item.kind
            else {
                unreachable!();
            };
            *saved_origin = origin;
            *saved_text = text.into();
            *saved_display = display_text.map(str::to_owned);
        }
        items
    }

    #[test_case(&[(UserOrigin::Turn, USER_TEXT, None), (UserOrigin::Turn, REPLY_TEXT, None)], Some((0, USER_TEXT)); "first_nonblank_text_wins")]
    #[test_case(&[(UserOrigin::Turn, USER_TEXT, Some(REPLY_TEXT)), (UserOrigin::Turn, INJECTED_TEXT, None)], Some((0, REPLY_TEXT)); "explicit_display_wins")]
    #[test_case(&[(UserOrigin::Turn, USER_TEXT, Some(BLANK_USER_TEXT)), (UserOrigin::Turn, REPLY_TEXT, None)], Some((0, BLANK_USER_TEXT)); "whitespace_display_is_visible")]
    #[test_case(&[(UserOrigin::Turn, USER_TEXT, Some("")), (UserOrigin::Turn, REPLY_TEXT, None)], None; "empty_display_hides_entire_group")]
    #[test_case(&[(UserOrigin::Turn, BLANK_USER_TEXT, None), (UserOrigin::Mention, USER_TEXT, Some(REPLY_TEXT)), (UserOrigin::Turn, INJECTED_TEXT, None)], Some((1, USER_TEXT)); "fallback_uses_raw_text_and_source_of_first_nonblank_user")]
    #[test_case(&[(UserOrigin::Turn, "", None), (UserOrigin::Turn, BLANK_USER_TEXT, None)], None; "blank_group_has_no_bubble")]
    #[test_case(&[(UserOrigin::Observation, INJECTED_TEXT, None), (UserOrigin::Turn, USER_TEXT, None)], None; "first_observation_blocks_later_turn")]
    #[test_case(&[(UserOrigin::Synthetic, SYNTHETIC_TEXT, Some(USER_TEXT)), (UserOrigin::Turn, USER_TEXT, None)], None; "first_synthetic_blocks_later_turn")]
    #[test_case(&[(UserOrigin::Mention, MENTION_BODY, None), (UserOrigin::Turn, USER_TEXT, None)], None; "first_mention_blocks_later_turn")]
    fn restored_user_groups_preserve_text_and_source(
        specs: &[(UserOrigin, &str, Option<&str>)],
        expected: Option<(usize, &str)>,
    ) {
        let mut items = grouped_user_items(specs);
        let expected =
            expected.map(|(index, text)| (Some(DisplaySource::User(items[index].id)), text));
        let unrelated = crate::history_items(&[Message::user(REPLY_TEXT.into())]).remove(0);
        let unrelated_source = Some(DisplaySource::User(unrelated.id));
        let mut assistant = crate::history_items(&[Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: REPLY_TEXT.into(),
            }],
            ..Default::default()
        }])
        .remove(0);
        assistant.group_id = items[0].group_id;
        items.insert(1, unrelated);
        items.insert(0, assistant);

        let display =
            history_to_display(&items, &empty_outputs(), &ToolOutputLines::default(), true).0;
        let users: Vec<_> = display
            .iter()
            .filter(|message| message.role == DisplayRole::User)
            .map(|message| (message.source, message.text.as_str()))
            .collect();
        let expected: Vec<_> = expected
            .into_iter()
            .chain([(unrelated_source, REPLY_TEXT)])
            .collect();
        assert_eq!(users, expected);
    }

    #[test_case(false; "hidden_reminders")]
    #[test_case(true; "visible_reminders")]
    fn restored_user_groups_keep_noncontiguous_images_in_history_order(show_reminders: bool) {
        let mut items = grouped_user_items(&[
            (UserOrigin::Turn, USER_TEXT, None),
            (UserOrigin::Mention, MENTION_BODY, None),
            (UserOrigin::Observation, INJECTED_TEXT, None),
        ]);
        let images: Vec<_> = [FIRST_IMAGE, SECOND_IMAGE, THIRD_IMAGE]
            .into_iter()
            .map(|data| ImageSource::new(ImageMediaType::Png, data.into()))
            .collect();
        for (item, image) in items.iter_mut().zip(&images) {
            let HistoryItemKind::User { images, .. } = &mut item.kind else {
                unreachable!();
            };
            images.push(image.clone());
        }
        let source = Some(DisplaySource::User(items[0].id));
        let unrelated_image = images[0].clone();
        let unrelated = crate::history_items(&[Message::user_with_images(
            REPLY_TEXT.into(),
            vec![unrelated_image.clone()],
        )])
        .remove(0);
        items.insert(1, unrelated);

        let display = history_to_display(
            &items,
            &empty_outputs(),
            &ToolOutputLines::default(),
            show_reminders,
        )
        .0;
        assert_eq!(display[0].source, source);
        assert_eq!(display[0].images.as_ref(), images);
        assert_eq!(display[0].text, format!("{USER_TEXT} [3 images]"));
        assert_eq!(display[1].images.as_ref(), [unrelated_image]);
        assert_eq!(display[1].text, format!("{REPLY_TEXT} [1 image]"));
        assert_eq!(display.len(), 2 + usize::from(show_reminders));
    }

    #[test_case(false; "hidden_reminders")]
    #[test_case(true; "visible_reminders")]
    fn task_observation_is_not_a_host_continuation_reminder(show_reminders: bool) {
        let messages = [
            Message::task_observation(
                TASK_OBSERVATION.into(),
                TaskEventOrigin {
                    task_id: TASK_ID.into(),
                    invocation_id: PRIVATE_INVOCATION.into(),
                    event_id: PRIVATE_EVENT.into(),
                },
            ),
            Message::observation(INJECTED_TEXT.into()),
        ];
        let history = crate::history_items(&messages);
        let display = history_to_display(
            &history,
            &empty_outputs(),
            &ToolOutputLines::default(),
            show_reminders,
        )
        .0;
        assert!(
            matches!(&display[0].role, DisplayRole::TaskDelivery(origin) if origin.task_id == TASK_ID)
        );
        assert_eq!(display[0].text, TASK_OBSERVATION);
        assert_eq!(display.len(), 1 + usize::from(show_reminders));
        if show_reminders {
            assert_eq!(display[1].role, DisplayRole::Injected);
            assert_eq!(display[1].text, INJECTED_TEXT);
        }
    }

    #[test_case(false; "hidden")]
    #[test_case(true; "visible")]
    fn background_reminder_live_and_reloaded_visibility_preserves_model_history(
        show_reminders: bool,
    ) {
        let mut message = Message::observation(BACKGROUND_REMINDER.into());
        message.standing_reminder = Some(StandingReminderKind::BackgroundWork);
        let history = crate::history_items(&[message]);
        let encoded = serde_json::to_value(&history).unwrap();
        let restored: Vec<HistoryItem> = serde_json::from_value(encoded).unwrap();
        assert_eq!(restored, history);
        assert!(matches!(
            restored[0].kind,
            HistoryItemKind::User {
                standing_reminder: Some(StandingReminderKind::BackgroundWork),
                task_event: None,
                ..
            }
        ));
        let model_history = project_messages(&restored).unwrap();
        assert_eq!(
            model_history[0].first_text_content(),
            Some(BACKGROUND_REMINDER)
        );
        assert_eq!(
            model_history[0].standing_reminder,
            Some(StandingReminderKind::BackgroundWork)
        );
        let (display, restore) = history_to_display(
            &restored,
            &empty_outputs(),
            &ToolOutputLines::default(),
            show_reminders,
        );
        assert!(restore.is_empty());
        assert_eq!(display.len(), usize::from(show_reminders));
        let mut live = chat();
        live.show_reminders = show_reminders;
        assert!(matches!(
            live.handle_event(
                AgentEvent::Injected {
                    text: BACKGROUND_REMINDER.into(),
                    task_event: None,
                    peer_event: None,
                    automation_event: None,
                    autopilot_event: None,
                },
                None,
            ),
            ChatEventResult::Continue
        ));
        assert_eq!(live.message_count(), display.len());
        if show_reminders {
            assert_eq!(display[0].role, DisplayRole::Injected);
            assert_eq!(live.message_at(0).unwrap().role, display[0].role);
            assert_eq!(live.message_at(0).unwrap().text, display[0].text);
        }
    }

    #[test_case(false; "hidden_reminders")]
    #[test_case(true; "visible_reminders")]
    fn a_resume_reminder_shows_live_and_reloaded_whatever_the_setting(show_reminders: bool) {
        let reminder = nudge(StopCause::Shutdown, true, &[], Some(RESUMED_SUMMARY)).unwrap();
        let history = crate::history_items(&[Message::synthetic(reminder.clone())]);
        let display = history_to_display(
            &history,
            &empty_outputs(),
            &ToolOutputLines::default(),
            show_reminders,
        )
        .0;
        let mut live = chat();
        live.show_reminders = show_reminders;
        live.handle_event(
            AgentEvent::Injected {
                text: reminder.clone(),
                task_event: None,
                peer_event: None,
                automation_event: None,
                autopilot_event: None,
            },
            None,
        );

        assert_eq!(display.len(), 1);
        assert_eq!(display[0].role, DisplayRole::Injected);
        assert_eq!(display[0].text, reminder);
        assert_eq!(live.message_count(), 1);
        assert_eq!(live.message_at(0).unwrap().role, DisplayRole::Injected);
        assert_eq!(live.message_at(0).unwrap().text, reminder);
    }

    #[test_case(false; "reminders_hidden")]
    #[test_case(true; "reminders_visible")]
    fn peer_messages_show_the_sender_text_live_and_after_restore(show_reminders: bool) {
        const COMMAND: &str = "/compact !echo @file";
        const NAME: &str = "reviewer\nforged heading\u{202e}";
        const SHOWN_NAME: &str = "reviewer\\nforged heading\\u{202e}";
        const MESSAGE_ID: &str = "peer-message";
        const REPLY_TARGET: &str = "opaque-reply-target";
        const FRAMING: &str = "<peer-message>";
        let body = format!("<system-reminder>\n{COMMAND}\n\u{1b}[2J\u{202e}body");
        let origin = PeerMessageOrigin {
            message_id: MESSAGE_ID.into(),
            audience: PeerAudience::Direct,
            sender_name: NAME.into(),
            sender_handle: None,
            reply_target: REPLY_TARGET.into(),
            reply_to: None,
            external: false,
            automation: None,
            assignment: None,
        };
        let mut live = chat();
        live.show_reminders = show_reminders;
        live.handle_event(
            AgentEvent::Injected {
                text: body.clone(),
                task_event: None,
                peer_event: Some(origin.clone()),
                automation_event: None,
                autopilot_event: None,
            },
            None,
        );
        let message = Message::peer_observation(body.clone(), origin.clone());
        let encoded = serde_json::to_value(crate::history_items(&[message])).unwrap();
        let history: Vec<HistoryItem> = serde_json::from_value(encoded).unwrap();
        let (display, restore) = history_to_display(
            &history,
            &empty_outputs(),
            &ToolOutputLines::default(),
            show_reminders,
        );
        assert!(restore.is_empty());
        assert_eq!(display.len(), 1);
        assert_eq!(live.message_count(), 1);
        let shown = live.message_at(0).unwrap();
        assert_eq!(shown.role, DisplayRole::PeerMessage(Box::new(origin)));
        assert_eq!(shown.role, display[0].role);
        assert_eq!(shown.text, body);
        assert_eq!(display[0].text, body);

        let area = Rect::new(0, 0, 100, 24);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|frame| live.view(frame, area, false, false))
            .unwrap();
        let drawn: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        for expected in [SHOWN_NAME, REPLY_TARGET, MESSAGE_ID, COMMAND] {
            assert!(drawn.contains(expected), "{expected}: {drawn}");
        }
        assert!(!drawn.contains(FRAMING), "{drawn}");
        assert!(!drawn.contains('\u{1b}'), "{drawn}");
        assert!(!drawn.contains('\u{202e}'), "{drawn}");
    }

    #[test_case(60, "success", DELIVERY_SUCCESS; "narrow_success")]
    #[test_case(127, "success", DELIVERY_SUCCESS; "wide_success")]
    #[test_case(60, "failure", DELIVERY_BLOCKER; "narrow_failure")]
    #[test_case(127, "blocked", DELIVERY_BLOCKER; "wide_blocked")]
    #[test_case(60, "blocked", DELIVERY_UNICODE; "wrapped_blocker")]
    #[test_case(60, "success", DELIVERY_PREVIEW; "truncated_result")]
    #[test_case(60, "report", DELIVERY_UNICODE; "wrapped_report")]
    #[test_case(28, "blocked", DELIVERY_BLOCKER; "wrapped_status")]
    fn task_delivery_live_and_reloaded_buffers_and_targets_agree(
        width: u16,
        state: &str,
        body: &str,
    ) {
        let origin = TaskEventOrigin {
            task_id: DELIVERY_TASK.into(),
            invocation_id: PRIVATE_INVOCATION.into(),
            event_id: PRIVATE_EVENT.into(),
        };
        let text = format!("Task {DELIVERY_TASK}: {state}.\n\n{body}");
        let mut live = chat();
        live.show_reminders = false;
        live.handle_event(
            AgentEvent::Injected {
                text: text.clone(),
                task_event: Some(origin.clone()),
                peer_event: None,
                automation_event: None,
                autopilot_event: None,
            },
            None,
        );
        let area = Rect::new(0, 0, width, 40);
        let mut terminal = Terminal::new(TestBackend::new(width, area.height)).unwrap();
        terminal
            .draw(|frame| live.view(frame, area, false, false))
            .unwrap();
        let before = terminal.backend().buffer().clone();
        let shown: String = before.content.iter().map(|cell| cell.symbol()).collect();
        assert_eq!(shown.matches(DELIVERY_TASK).count(), 1, "{shown}");
        assert_eq!(shown.matches(state).count(), 1, "{shown}");
        assert!(!shown.contains(PRIVATE_INVOCATION), "{shown}");
        assert!(!shown.contains(PRIVATE_EVENT), "{shown}");
        assert!(!shown.contains("\"output\":"), "{shown}");
        let expected_style = match state {
            "success" => crate::theme::current().tool_success,
            "failure" => crate::theme::current().tool_error,
            "blocked" => crate::theme::current().tool_warning,
            _ => crate::theme::current().tool_dim,
        };
        let (state_column, state_row) = (0..area.height)
            .find_map(|row| {
                let line: String = (0..width)
                    .map(|column| before[(column, row)].symbol())
                    .collect();
                line.find(state)
                    .map(|column| (line[..column].chars().count() as u16, row))
            })
            .unwrap();
        assert_eq!(
            before[(state_column, state_row)].fg,
            expected_style.fg.unwrap()
        );
        if body == DELIVERY_SUCCESS {
            assert!(shown.contains(DELIVERY_SUCCESS), "{shown}");
            assert!(!shown.contains("tool_output"), "{shown}");
            assert!(!shown.contains("Full outcome"), "{shown}");
        }
        if body == DELIVERY_UNICODE {
            assert_eq!(
                shown.matches('界').count(),
                body.matches('界').count(),
                "{shown}"
            );
        }
        if body == DELIVERY_PREVIEW {
            assert_eq!(shown.matches("calm-blue-wren").count(), 1, "{shown}");
        }
        let row = (0..area.height)
            .find(|&row| {
                let text: String = (0..width)
                    .map(|column| before[(column, row)].symbol())
                    .collect();
                text.contains(DELIVERY_TASK)
            })
            .unwrap();
        assert_eq!(live.task_hit_at(row, area).as_deref(), Some(DELIVERY_TASK));
        let encoded =
            serde_json::to_value(Message::task_observation(text.clone(), origin.clone())).unwrap();
        let restored: Message = serde_json::from_value(encoded).unwrap();
        let history = crate::history_items(&[restored]);
        let (display, _) = history_to_display(
            &history,
            &empty_outputs(),
            &ToolOutputLines::default(),
            false,
        );
        assert_eq!(display.len(), 1);
        assert_eq!(display[0].role, DisplayRole::TaskDelivery(Box::new(origin)));
        assert_eq!(display[0].text, text);
        live.load_messages(display);
        terminal
            .draw(|frame| live.view(frame, area, false, false))
            .unwrap();
        assert_eq!(terminal.backend().buffer(), &before);
        assert_eq!(live.task_hit_at(row, area).as_deref(), Some(DELIVERY_TASK));
    }

    #[test_case(true; "typed_origin_wins_over_heading")]
    #[test_case(false; "text_alone_is_not_a_delivery")]
    fn task_delivery_navigation_never_infers_identity_from_text(typed: bool) {
        let text = format!("Task {DELIVERY_TASK}: success.\n\n{DELIVERY_SUCCESS}");
        let origin = typed.then(|| TaskEventOrigin {
            task_id: TASK_ID.into(),
            invocation_id: PRIVATE_INVOCATION.into(),
            event_id: PRIVATE_EVENT.into(),
        });
        let mut chat = chat();
        chat.show_reminders = true;
        chat.handle_event(
            AgentEvent::Injected {
                text,
                task_event: origin,
                peer_event: None,
                automation_event: None,
                autopilot_event: None,
            },
            None,
        );
        let area = Rect::new(0, 0, 127, 40);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|frame| chat.view(frame, area, false, false))
            .unwrap();
        let row = (0..area.height)
            .find(|&row| {
                let text: String = (0..area.width)
                    .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                    .collect();
                text.contains(DELIVERY_TASK)
            })
            .unwrap();
        assert_eq!(
            chat.task_hit_at(row, area).as_deref(),
            typed.then_some(TASK_ID)
        );
    }

    #[test_case(40 ; "narrow")]
    #[test_case(127 ; "wide")]
    fn automation_delivery_live_and_reloaded_rows_agree_and_name_the_firing(width: u16) {
        let origin = AutomationEventOrigin {
            automation: AUTOMATION_NAME.into(),
            fire_id: AUTOMATION_FIRE_ID.into(),
            seq: AUTOMATION_SEQ,
        };
        let mut live = chat();
        live.show_reminders = false;
        live.handle_event(
            AgentEvent::Injected {
                text: AUTOMATION_TEXT.into(),
                task_event: None,
                peer_event: None,
                automation_event: Some(origin.clone()),
                autopilot_event: None,
            },
            None,
        );
        let area = Rect::new(0, 0, width, 40);
        let mut terminal = Terminal::new(TestBackend::new(width, area.height)).unwrap();
        terminal
            .draw(|frame| live.view(frame, area, false, false))
            .unwrap();
        let before = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..area.height)
            .map(|row| {
                (0..width)
                    .map(|column| before[(column, row)].symbol())
                    .collect()
            })
            .collect();
        let shown = rows.concat();
        assert_eq!(shown.matches(AUTOMATION_NAME).count(), 1, "{shown}");
        assert!(!shown.contains(AUTOMATION_FIRE_ID), "{shown}");
        let heading = rows
            .iter()
            .position(|line| line.contains(AUTOMATION_NAME))
            .unwrap() as u16;
        let last_word = AUTOMATION_TEXT.rsplit(' ').next().unwrap();
        let body = rows
            .iter()
            .rposition(|line| line.contains(last_word))
            .unwrap() as u16;
        assert!(body > heading, "{shown}");
        for row in [heading, body] {
            assert_eq!(live.automation_hit_at(row, area), Some(origin.clone()));
        }
        assert_eq!(live.task_hit_at(heading, area), None);

        let message = Message {
            display_text: Some(AUTOMATION_TEXT.into()),
            ..Message::automation_observation(AUTOMATION_FRAMED.into(), origin.clone())
        };
        let encoded = serde_json::to_value(message).unwrap();
        let restored: Message = serde_json::from_value(encoded).unwrap();
        let history = crate::history_items(&[restored]);
        let (display, _) = history_to_display(
            &history,
            &empty_outputs(),
            &ToolOutputLines::default(),
            false,
        );
        assert_eq!(display.len(), 1);
        assert_eq!(
            display[0].role,
            DisplayRole::AutomationDelivery(Box::new(origin.clone()))
        );
        assert_eq!(display[0].text, AUTOMATION_TEXT);
        live.load_messages(display);
        terminal
            .draw(|frame| live.view(frame, area, false, false))
            .unwrap();
        assert_eq!(terminal.backend().buffer(), &before);
        assert_eq!(live.automation_hit_at(heading, area), Some(origin));
    }

    /// The exact shape of a fresh session: the environment and the mode are
    /// announced under the first message, so the transcript opens on what was
    /// typed rather than on two reminders.
    #[test]
    fn a_restored_first_turn_keeps_its_reminders_below_it() {
        let msgs = vec![
            Message::user(USER_TEXT.into()),
            Message::observation(INJECTED_TEXT.into()),
            Message::observation(SYNTHETIC_TEXT.into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: REPLY_TEXT.into(),
                }],
                ..Default::default()
            },
        ];
        let items = crate::history_items(&msgs);

        let display =
            history_to_display(&items, &empty_outputs(), &ToolOutputLines::default(), true).0;

        assert_eq!(display[0].role, DisplayRole::User);
        assert_eq!(display[0].text, USER_TEXT);
        assert_eq!(display[1].role, DisplayRole::Injected);
        assert_eq!(display[2].role, DisplayRole::Injected);
        assert_eq!(display[3].role, DisplayRole::Assistant);
    }

    fn id_time(id: CaudraId) -> Option<Timestamp> {
        Timestamp::from_millisecond(i64::try_from(id.created_ms()?).ok()?).ok()
    }

    #[test_case(false ; "an_item_is_as_old_as_its_id")]
    #[test_case(true ; "a_compaction_copy_is_as_old_as_its_original")]
    fn a_restored_message_keeps_its_time(copied: bool) {
        const ORIGINAL_MS: u64 = 1_700_000_000_000;
        const TIME_BYTES: usize = 6;
        let mut bytes = *CaudraId::generate().as_bytes();
        bytes[..TIME_BYTES].copy_from_slice(&ORIGINAL_MS.to_be_bytes()[2..]);
        let original = CaudraId::from_bytes(bytes);
        let mut items = crate::history_items(&[Message::user(USER_TEXT.into())]);
        if copied {
            items[0].stands_for = Some(original);
        }

        let display =
            history_to_display(&items, &empty_outputs(), &ToolOutputLines::default(), true).0;

        let expected = if copied {
            id_time(original)
        } else {
            id_time(items[0].id)
        };
        assert_eq!(display[0].created, expected);
        assert!(expected.is_some());
    }

    /// An arrival is written above the message because it landed before it, and
    /// the replay says so. Live it draws below, because the row is emitted when
    /// the run claims the notice rather than when the notice arrived; the replay
    /// is the order that actually happened.
    #[test]
    fn a_restored_arrival_stays_above_the_turn_that_followed_it() {
        let msgs = vec![
            Message::observation(INJECTED_TEXT.into()),
            Message::user(USER_TEXT.into()),
            Message::observation(SYNTHETIC_TEXT.into()),
        ];
        let items = crate::history_items(&msgs);

        let display =
            history_to_display(&items, &empty_outputs(), &ToolOutputLines::default(), true).0;

        let rows: Vec<(&DisplayRole, &str)> = display
            .iter()
            .map(|message| (&message.role, message.text.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                (&DisplayRole::Injected, INJECTED_TEXT),
                (&DisplayRole::User, USER_TEXT),
                (&DisplayRole::Injected, SYNTHETIC_TEXT),
            ]
        );
    }

    /// A continuation belongs to no turn of its own, so it stays where it
    /// happened rather than joining the turn above it.
    #[test]
    fn an_injected_message_with_no_turn_of_its_own_keeps_its_place() {
        let msgs = vec![
            Message::user(USER_TEXT.into()),
            Message::synthetic(SYNTHETIC_TEXT.into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: REPLY_TEXT.into(),
                }],
                ..Default::default()
            },
        ];
        let items = crate::history_items(&msgs);

        let display =
            history_to_display(&items, &empty_outputs(), &ToolOutputLines::default(), true).0;

        assert_eq!(display[0].role, DisplayRole::User);
        assert_eq!(display[1].role, DisplayRole::Injected);
        assert_eq!(display[1].text, SYNTHETIC_TEXT);
        assert_eq!(display[2].role, DisplayRole::Assistant);
    }

    fn delivery_origin() -> TaskEventOrigin {
        TaskEventOrigin {
            task_id: DELIVERY_TASK.into(),
            invocation_id: PRIVATE_INVOCATION.into(),
            event_id: PRIVATE_EVENT.into(),
        }
    }

    #[test_case(WAIT_NUDGE_TEXT, DisplayRole::Injected ; "a_wait_nudge_folds_like_a_reminder")]
    #[test_case(DELIVERY_SUCCESS, DisplayRole::TaskDelivery(Box::new(delivery_origin())) ; "a_result_keeps_its_task_card")]
    fn a_shell_observation_is_drawn_by_who_wrote_it(text: &str, role: DisplayRole) {
        let msgs = vec![Message::task_observation(text.into(), delivery_origin())];
        let display = history_to_display(
            &crate::history_items(&msgs),
            &empty_outputs(),
            &ToolOutputLines::default(),
            false,
        )
        .0;
        assert_eq!(display.len(), 1);
        assert_eq!(display[0].role, role);
    }

    #[test]
    fn history_to_display_drops_injected_messages_when_reminders_are_off() {
        let msgs = vec![
            Message::observation(INJECTED_TEXT.into()),
            Message::synthetic(SYNTHETIC_TEXT.into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: REPLY_TEXT.into(),
                }],
                ..Default::default()
            },
        ];
        let items = crate::history_items(&msgs);

        let display =
            history_to_display(&items, &empty_outputs(), &ToolOutputLines::default(), false).0;

        assert_eq!(display.len(), 1);
        assert_eq!(display[0].role, DisplayRole::Assistant);
        assert_eq!(display[0].text, REPLY_TEXT);
    }

    #[test]
    fn history_to_display_keeps_opaque_reasoning_as_a_title_only_block() {
        let mut reasoning = ContentBlock::thinking(String::new(), None);
        let ContentBlock::Thinking { responses, .. } = &mut reasoning else {
            unreachable!();
        };
        *responses = Some(caudra_providers::ResponsesReasoning {
            item_id: "rs_1".into(),
            encrypted_content: Some("ciphertext".into()),
        });
        let display = display_messages(
            &[Message {
                role: Role::Assistant,
                content: vec![reasoning],
                ..Default::default()
            }],
            &HashMap::new(),
        )
        .0;

        assert_eq!(display.len(), 1);
        assert_eq!(display[0].role, DisplayRole::Thinking);
        assert!(display[0].text.is_empty());
    }

    const RESTORE_OUTPUT: &str = "rendered output";

    fn tool_msg_with_input(tool: &str) -> DisplayMessage {
        let mut msg = DisplayMessage::new(DisplayRole::User, String::new());
        msg.role = DisplayRole::Tool(Box::new(ToolRole {
            id: "t1".into(),
            effect: ToolEffect::Unknown,
            status: ToolStatus::Success,
            name: tool.into(),
        }));
        msg.tool_raw_input = Some(Arc::new(serde_json::json!({ "q": tool })));
        msg.tool_output = Some(Arc::new(ToolOutput::Plain(RESTORE_OUTPUT.into())));
        msg
    }

    const RESTORE_THEME_GEN: u64 = 7;

    #[test]
    fn restore_item_for_round_trips_fields() {
        let msg = tool_msg_with_input("shell");
        let item = restore_item_for(&msg, ToolOutputLines::default(), RESTORE_THEME_GEN)
            .expect("tool message with input and output must produce a RestoreItem");
        assert_eq!(&*item.tool, "shell");
        assert_eq!(item.tool_use_id, "t1");
        assert!(!item.is_error);
        assert_eq!(item.output, RESTORE_OUTPUT);
        assert_eq!(item.theme_gen, Some(RESTORE_THEME_GEN));
        assert_eq!(item.input, serde_json::json!({ "q": "shell" }));
    }

    #[test]
    fn restore_item_for_skips_structured_outputs_rust_renders() {
        let mut msg = tool_msg_with_input("file_edit");
        msg.tool_output = Some(Arc::new(edit_output("/src/main.rs")));
        assert!(restore_item_for(&msg, ToolOutputLines::default(), RESTORE_THEME_GEN).is_none());
    }

    #[test]
    fn history_structured_output_produces_no_restore_item() {
        let msgs = tool_use_pair(
            "file_edit",
            serde_json::json!({"path": "a", "old_string": "x", "new_string": "y"}),
            "edited a",
            false,
        );
        let outputs = HashMap::from([("t1".to_owned(), Arc::new(edit_output("a")))]);
        let (_, items) = display_messages(&msgs, &outputs);
        assert!(items.is_empty(), "Rust owns Diff rendering on restore");

        let (_, items) = display_messages(&msgs, &empty_outputs());
        assert_eq!(items.len(), 1, "text-only history still restores via Lua");
    }

    const ANSWER_HEADER: &str = "Transfer channel";
    const ANSWER_QUESTION: &str = "How should bytes move?";
    const ANSWER_PICKED: &str = "Signed URLs";
    const ANSWER_DECLINED: &str = "Base64 in JSON-RPC";
    const ANSWER_DESCRIPTION: &str = "Mint short-lived URLs instead of returning bytes";
    const FORM_RESTORED: &str = "the form is filled back in from the tool call input";
    const ANSWER_NOTE: &str = "Keep the bucket private";

    fn question_input() -> serde_json::Value {
        serde_json::json!({ "questions": [{
            "question": ANSWER_QUESTION,
            "header": ANSWER_HEADER,
            "options": [
                { "label": ANSWER_PICKED, "description": ANSWER_DESCRIPTION },
                { "label": ANSWER_DECLINED, "description": "" },
            ],
        }]})
    }

    fn bare_answers(count: usize) -> HashMap<String, Arc<ToolOutput>> {
        noted_answers(count, "")
    }

    fn noted_answers(count: usize, note: &str) -> HashMap<String, Arc<ToolOutput>> {
        let answers = (0..count)
            .map(|_| Answer {
                header: ANSWER_HEADER.into(),
                labels: vec![ANSWER_PICKED.into()],
                question: String::new(),
                options: Vec::new(),
                by: Answerer::User,
            })
            .collect();
        let sheet = AnswerSheet {
            answers,
            note: note.into(),
        };
        HashMap::from([("t1".to_owned(), Arc::new(ToolOutput::Answers(sheet)))])
    }

    fn restored_sheet(
        input: serde_json::Value,
        outputs: &HashMap<String, Arc<ToolOutput>>,
    ) -> AnswerSheet {
        let msgs = tool_use_pair("question", input, "Transfer channel: Signed URLs", false);
        let (display, _) = display_messages(&msgs, outputs);
        let Some(ToolOutput::Answers(sheet)) = display[0].tool_output.as_deref().cloned() else {
            panic!("a restored question keeps its structured answers");
        };
        sheet
    }

    fn restored_answer(
        input: serde_json::Value,
        outputs: &HashMap<String, Arc<ToolOutput>>,
    ) -> Answer {
        restored_sheet(input, outputs).answers[0].clone()
    }

    #[test]
    fn a_restored_answer_keeps_the_users_note() {
        let sheet = restored_sheet(question_input(), &noted_answers(1, ANSWER_NOTE));
        assert_eq!(sheet.note, ANSWER_NOTE);
        assert_eq!(
            sheet.answers[0].question, ANSWER_QUESTION,
            "{FORM_RESTORED}"
        );
    }

    /// An answer is persisted as the picks alone, so the card's form comes back
    /// from the questions the call was made with.
    #[test]
    fn a_restored_answer_is_filled_in_from_the_questions_it_answered() {
        let answer = restored_answer(question_input(), &bare_answers(1));
        assert_eq!(answer.question, ANSWER_QUESTION, "{FORM_RESTORED}");
        assert_eq!(answer.options.len(), 2, "{FORM_RESTORED}");
        assert_eq!(answer.options[1].label, ANSWER_DECLINED, "{FORM_RESTORED}");
        assert_eq!(answer.labels, [ANSWER_PICKED]);
    }

    /// An input that no longer lines up with its answers cannot be redrawn as a
    /// form, and the picks are worth more than the wrong questions.
    #[test]
    fn an_input_that_does_not_match_its_answers_leaves_the_picks_alone() {
        let answer = restored_answer(question_input(), &bare_answers(2));
        assert!(answer.question.is_empty());
        assert!(answer.options.is_empty());
        assert_eq!(answer.labels, [ANSWER_PICKED]);
    }

    #[test]
    fn an_answer_restored_without_an_input_keeps_its_picks() {
        let answer = restored_answer(serde_json::json!({}), &bare_answers(1));
        assert!(answer.question.is_empty());
        assert_eq!(answer.labels, [ANSWER_PICKED]);
    }

    #[test]
    fn restore_item_for_returns_none_when_data_missing() {
        let tol = ToolOutputLines::default();

        let plain = DisplayMessage::new(DisplayRole::Assistant, "hi".into());
        assert!(restore_item_for(&plain, tol, RESTORE_THEME_GEN).is_none());

        let mut no_input = tool_msg_with_input("shell");
        no_input.tool_raw_input = None;
        assert!(restore_item_for(&no_input, tol, RESTORE_THEME_GEN).is_none());

        let mut no_output = tool_msg_with_input("shell");
        no_output.tool_output = None;
        assert!(restore_item_for(&no_output, tol, RESTORE_THEME_GEN).is_none());
    }

    #[test]
    fn compaction_done_flushes_streaming_buffers() {
        let mut chat = chat();

        chat.handle_event(AgentEvent::Compacting, None);
        assert_eq!(chat.message_count(), 1);
        assert_eq!(chat.last_message_text(), COMPACTION_BORDER_TEXT);
        assert_eq!(chat.last_message_role(), Some(&DisplayRole::Notice));

        chat.handle_event(
            AgentEvent::TextDelta {
                text: "summary".into(),
            },
            None,
        );
        chat.handle_event(
            AgentEvent::ThinkingDelta {
                text: "thinking".into(),
            },
            None,
        );
        assert!(!chat.streaming_text_is_empty());
        assert!(!chat.streaming_thinking_is_empty());

        chat.handle_event(AgentEvent::CompactionDone, None);
        assert!(chat.streaming_text_is_empty());
        assert!(chat.streaming_thinking_is_empty());
        assert_eq!(chat.message_count(), 3);

        chat.handle_event(AgentEvent::TextDelta { text: "new".into() }, None);
        chat.flush();
        assert_eq!(chat.message_count(), 4);
        assert_eq!(chat.last_message_text(), "new");
    }

    fn turn_complete(text: &str, purpose: LedgerPurpose) -> AgentEvent {
        AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
            interrupted: false,
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: text.into() }],
                ..Default::default()
            },
            usage: Default::default(),
            model: String::new(),
            provider: String::new(),
            purpose,
            cost: None,
            billing: Billing::Api,
            account: None,
            context_size: None,
            context_window: 0,
        }))
    }

    /// The requirements section is appended once extraction finishes, which is
    /// after the summary's own stream closed. The card has to end up holding
    /// what was stored, not only what streamed.
    #[test]
    fn a_compaction_turn_adopts_the_message_the_agent_kept() {
        const STREAMED: &str = "did the work";
        const APPENDED: &str = "\n\n# User requirements\n- one";

        let mut chat = chat();
        chat.handle_event(AgentEvent::Compacting, None);
        text_delta(&mut chat, STREAMED);

        let kept = format!("{STREAMED}{APPENDED}");
        chat.handle_event(turn_complete(&kept, LedgerPurpose::Compaction), None);
        chat.handle_event(AgentEvent::CompactionDone, None);

        assert_eq!(chat.last_message_text(), kept);
    }

    /// A buffer the final message does not extend is replaced outright: a delta
    /// cannot take characters back, so pushing a tail would leave the card
    /// holding text no one wrote.
    #[test]
    fn a_diverged_buffer_is_replaced_rather_than_extended() {
        const KEPT: &str = "what was really kept";

        let mut chat = chat();
        chat.handle_event(AgentEvent::Compacting, None);
        text_delta(&mut chat, "something else entirely");

        chat.handle_event(turn_complete(KEPT, LedgerPurpose::Compaction), None);
        chat.handle_event(AgentEvent::CompactionDone, None);

        assert_eq!(chat.last_message_text(), KEPT);
    }

    /// Only compaction edits a message after its stream: an ordinary turn is
    /// already whole, and adopting it would risk redrawing a settled card.
    #[test]
    fn an_ordinary_turn_leaves_the_streamed_text_alone() {
        let mut chat = chat();
        text_delta(&mut chat, REPLY_TEXT);

        chat.handle_event(turn_complete("something else", LedgerPurpose::Chat), None);
        chat.flush();

        assert_eq!(chat.last_message_text(), REPLY_TEXT);
    }

    /// `Nudge` only says a continuation is coming; the continuation itself
    /// follows as `Injected` and carries the prompt that was really sent. A
    /// bubble on the announcement would double every retry.
    #[test]
    fn a_nudge_shows_the_continuation_rather_than_an_announcement() {
        let mut chat = chat();
        text_delta(&mut chat, REPLY_TEXT);

        chat.handle_event(
            AgentEvent::Nudge {
                attempt: 1,
                limit: 3,
            },
            None,
        );
        chat.handle_event(
            AgentEvent::Injected {
                text: INJECTED_TEXT.into(),
                task_event: None,
                peer_event: None,
                automation_event: None,
                autopilot_event: None,
            },
            None,
        );

        assert_eq!(chat.message_count(), 2);
        assert_eq!(
            chat.message_at(0).map(|m| &m.role),
            Some(&DisplayRole::Assistant)
        );
        assert_eq!(chat.last_message_role(), Some(&DisplayRole::Injected));
        assert_eq!(chat.last_message_text(), INJECTED_TEXT);
    }

    /// The transcript keeps growing after the ending, since the subagent chat
    /// stays on screen while the parent turn talks on. A fix aimed at the last
    /// message would eat whatever landed in between.
    #[test]
    fn a_correction_rewrites_the_recorded_bubble_not_the_last_one() {
        let mut chat = chat();
        end(&mut chat, TaskOutcome::Unknown);
        let ending = chat.message_count() - 1;

        chat.show_user_message(USER_TEXT);
        text_delta(&mut chat, REPLY_TEXT);
        chat.flush();
        let before = chat.message_count();

        end(&mut chat, TaskOutcome::Error);

        assert_eq!(chat.message_count(), before);
        let bubble = chat
            .message_at(ending)
            .expect("recorded ending still there");
        assert_eq!(bubble.text, ERROR_TEXT);
        assert_eq!(bubble.role, DisplayRole::Error);
        assert_eq!(
            chat.message_at(ending + 1).map(|m| &m.role),
            Some(&DisplayRole::User)
        );
        assert_eq!(
            chat.message_at(ending + 1).map(|m| m.text.as_str()),
            Some(USER_TEXT)
        );
        assert_eq!(chat.last_message_text(), REPLY_TEXT);
        assert_eq!(chat.last_message_role(), Some(&DisplayRole::Assistant));
    }

    /// The stream is flushed before the ending is pushed, so a reply still in
    /// the buffer keeps its place ahead of it and the recorded index points at
    /// the ending, not at the flushed text.
    #[test]
    fn a_pending_stream_lands_before_the_ending_bubble() {
        let mut chat = chat();
        text_delta(&mut chat, REPLY_TEXT);
        assert!(!chat.streaming_text_is_empty());

        end(&mut chat, TaskOutcome::Unknown);

        assert!(chat.streaming_text_is_empty());
        assert_eq!(chat.message_count(), 2);
        assert_eq!(
            chat.message_at(0).map(|m| m.text.as_str()),
            Some(REPLY_TEXT)
        );
        assert_eq!(chat.last_message_text(), DONE_TEXT);

        end(&mut chat, TaskOutcome::Error);

        assert_eq!(chat.message_count(), 2);
        assert_eq!(
            chat.message_at(0).map(|m| m.text.as_str()),
            Some(REPLY_TEXT)
        );
        assert_eq!(
            chat.message_at(0).map(|m| &m.role),
            Some(&DisplayRole::Assistant)
        );
        assert_eq!(chat.last_message_text(), ERROR_TEXT);
        assert_eq!(chat.last_message_role(), Some(&DisplayRole::Error));
    }

    /// Every order two endings can arrive in. Only the placeholder gives way,
    /// a verdict is never walked back, and no order grows a second bubble.
    #[test_case(TaskOutcome::Unknown, TaskOutcome::Done, DONE_TEXT, DisplayRole::Done, TaskStatus::Done   ; "placeholder_settles_as_done")]
    #[test_case(TaskOutcome::Unknown, TaskOutcome::Killed, CANCELLED_TEXT, DisplayRole::Error, TaskStatus::Error ; "placeholder_corrected_to_killed")]
    #[test_case(TaskOutcome::Unknown, TaskOutcome::Error, ERROR_TEXT, DisplayRole::Error, TaskStatus::Error ; "placeholder_corrected_to_error")]
    #[test_case(TaskOutcome::Done, TaskOutcome::Error, DONE_TEXT, DisplayRole::Done, TaskStatus::Done     ; "verdict_survives_late_error")]
    #[test_case(TaskOutcome::Error, TaskOutcome::Done, ERROR_TEXT, DisplayRole::Error, TaskStatus::Error  ; "verdict_survives_late_done")]
    #[test_case(TaskOutcome::Done, TaskOutcome::Done, DONE_TEXT, DisplayRole::Done, TaskStatus::Done      ; "repeated_verdict")]
    #[test_case(TaskOutcome::Unknown, TaskOutcome::Unknown, DONE_TEXT, DisplayRole::Done, TaskStatus::Done ; "repeated_placeholder")]
    fn a_second_ending_never_adds_a_bubble(
        first: TaskOutcome,
        second: TaskOutcome,
        text: &str,
        role: DisplayRole,
        status: TaskStatus,
    ) {
        let mut chat = chat();
        chat.show_user_message(USER_TEXT);
        end(&mut chat, first);
        let count = chat.message_count();

        end(&mut chat, second);

        assert_eq!(chat.message_count(), count);
        assert_eq!(chat.last_message_text(), text);
        assert_eq!(chat.last_message_role(), Some(&role));
        assert_eq!(chat.task_status(), status);
    }

    /// The shape `caudra.task.list()` serializes: the main chat is not a task,
    /// and a subagent is one from the moment it starts, working until an
    /// ending lands on it.
    #[test]
    fn only_a_subagent_reports_a_task_and_its_status() {
        let main = chat();
        assert!(main.task_id().is_none());
        assert!(!main.is_finished());

        let mut sub = subagent_chat();
        assert_eq!(sub.task_id().map(|id| &**id), Some(TASK_ID));
        assert_eq!(sub.task_status(), TaskStatus::Working);
        assert!(!sub.is_finished());

        end(&mut sub, TaskOutcome::Error);

        assert!(sub.is_finished());
        assert_eq!(sub.task_status(), TaskStatus::Error);
        assert_eq!(sub.task_id().map(|id| &**id), Some(TASK_ID));
    }

    const AUTOPILOT_TEXT: &str = "Autopilot replied: keep going";
    const EARLY_ROW: &str = "permission allow: `cargo test`";
    const LATE_ROW: &str = "ended: goal met";
    const AUTOPILOT_SEQ: u32 = 4;

    fn autopilot_origin() -> AutopilotOrigin {
        AutopilotOrigin {
            seq: AUTOPILOT_SEQ,
            kind: AutopilotMessageKind::Reply,
        }
    }

    /// A row made at history length `n` lands after the first `n` messages,
    /// one past the end follows everything, and what autopilot wrote into
    /// the conversation reads as its own row.
    #[test]
    fn autopilot_rows_interleave_with_the_history_they_were_made_at() {
        let msgs = vec![
            Message::user(USER_TEXT.into()),
            Message::autopilot_observation(AUTOPILOT_TEXT.into(), autopilot_origin()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: REPLY_TEXT.into(),
                }],
                ..Default::default()
            },
        ];
        let items = crate::history_items(&msgs);
        let rows = vec![
            (msgs.len(), DisplayMessage::autopilot(LATE_ROW.into())),
            (1, DisplayMessage::autopilot(EARLY_ROW.into())),
        ];

        let (display, _) = interleaved_history_to_display(
            &items,
            &empty_outputs(),
            &ToolOutputLines::default(),
            false,
            rows,
            Vec::new(),
        );

        let shown: Vec<(&DisplayRole, &str)> = display
            .iter()
            .map(|message| (&message.role, message.text.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                (&DisplayRole::User, USER_TEXT),
                (&DisplayRole::Autopilot, EARLY_ROW),
                (&DisplayRole::Autopilot, AUTOPILOT_TEXT),
                (&DisplayRole::Assistant, REPLY_TEXT),
                (&DisplayRole::Autopilot, LATE_ROW),
            ]
        );
    }

    #[test]
    fn a_live_autopilot_message_is_autopilots_row() {
        let mut chat = chat();
        chat.show_reminders = false;

        chat.handle_event(
            AgentEvent::Injected {
                text: AUTOPILOT_TEXT.into(),
                task_event: None,
                peer_event: None,
                automation_event: None,
                autopilot_event: Some(autopilot_origin()),
            },
            None,
        );

        let last = chat.message_at(chat.message_count() - 1).unwrap();
        assert_eq!(
            (&last.role, last.text.as_str()),
            (&DisplayRole::Autopilot, AUTOPILOT_TEXT)
        );
    }

    const DECISION_SUMMARY: &str = "3 notes ranked";
    const DECISION_DETAIL: &str = "0.97  release-notes  kept 1st";
    const FIRST_CALL: &str = "t1";
    const SECOND_CALL: &str = "t2";
    const SECOND_CALL_CHILD: &str = "t2:0";
    const UNKNOWN_CALL: &str = "t9";
    const RETIRED_FEATURE: &str = "retired_feature";
    const UNKNOWN_SCREENING: &str = "retired_screening";
    const TURN_ROW: &str = "ranked before the turn";
    const FIRST_CALL_ROW: &str = "ranked in the first call";
    const SECOND_CALL_ROW: &str = "ranked in the second call";
    const CHILD_ROW: &str = "ranked in a batch child";
    const UNKNOWN_CALL_ROW: &str = "ranked in a call that is gone";
    const COMPACTION_SUMMARY: &str = "## Objective";
    const PENDING_STATE: &str = "pending";
    const QUIET_STATE: &str = "quiet";
    const TURN_SUBJECT: &str = "turn";
    const NEAR_MISS: &str = "0.41  release-notes";
    const TURN_WAIT: Duration = Duration::from_millis(1_234);

    fn decision(outcome: NoteOutcome, anchor: NoteAnchor) -> AgentEvent {
        turn_decision(DecisionFeature::MemorySearch, outcome, anchor)
    }

    fn turn_decision(
        feature: DecisionFeature,
        outcome: NoteOutcome,
        anchor: NoteAnchor,
    ) -> AgentEvent {
        DecisionNote::new(
            feature,
            FeatureMode::Advise,
            outcome,
            DECISION_SUMMARY,
            vec![DECISION_DETAIL.into()],
            anchor,
        )
        .into()
    }

    fn pending(anchor: NoteAnchor) -> AgentEvent {
        pending_on(DecisionFeature::MemorySearch, anchor)
    }

    fn pending_on(feature: DecisionFeature, anchor: NoteAnchor) -> AgentEvent {
        AgentEvent::DecisionPending(Box::new(PendingDecision {
            feature,
            anchor,
            cause: None,
            details: Vec::new(),
            phase: None,
        }))
    }

    fn settled(anchor: NoteAnchor) -> AgentEvent {
        settled_on(DecisionFeature::MemorySearch, anchor, Vec::new())
    }

    fn settled_on(
        feature: DecisionFeature,
        anchor: NoteAnchor,
        details: Vec<String>,
    ) -> AgentEvent {
        AgentEvent::DecisionSettled(Box::new(PendingDecision {
            feature,
            anchor,
            cause: None,
            details,
            phase: None,
        }))
    }

    /// `base` followed by `t1=acted` for each decision on the row, a turn's
    /// as `turn=acted`.
    fn decided_label(base: &str, decisions: &[CardDecision]) -> String {
        let mut label = base.to_owned();
        for decision in decisions {
            let state = match &decision.state {
                DecisionState::Pending { .. } => PENDING_STATE,
                DecisionState::Settled { outcome, .. } => outcome.as_str(),
                DecisionState::Quiet { .. } => QUIET_STATE,
            };
            let subject = match &decision.subject {
                DecisionSubject::Call(call_id) => call_id.as_str(),
                DecisionSubject::Turn => TURN_SUBJECT,
            };
            label.push_str(&format!(" {subject}={state}"));
        }
        label
    }

    /// Each message as a reader tells it apart: a card by its call, a
    /// decision row by its outcome, anything else by its text, each followed
    /// by the decisions on it.
    fn live_placement(chat: &Chat) -> Vec<String> {
        (0..chat.message_count())
            .filter_map(|index| chat.message_at(index))
            .map(|message| {
                let base = match &message.role {
                    DisplayRole::Tool(tool) => tool.id.as_str(),
                    DisplayRole::Decision(outcome, _) => outcome.as_str(),
                    _ => message.text.as_str(),
                };
                decided_label(base, &message.decisions)
            })
            .collect()
    }

    fn turn_label(base: &str, state: &str) -> String {
        format!("{base} {TURN_SUBJECT}={state}")
    }

    fn prompt(chat: &mut Chat) {
        chat.messages_panel
            .push(DisplayMessage::new(DisplayRole::User, USER_TEXT.into()));
    }

    /// Two calls run side by side, so a later card is already drawn when the
    /// first call's decisions arrive. Each still lands on its own card, in
    /// the order it arrived, and none draws a row.
    #[test]
    fn a_calls_decision_lands_on_its_card_beside_a_parallel_call() {
        let mut chat = chat();
        chat.handle_event(tool_start(FIRST_CALL, SHELL_TOOL_NAME), None);
        chat.handle_event(tool_start(SECOND_CALL, SHELL_TOOL_NAME), None);

        for (outcome, call) in [
            (NoteOutcome::Shadow, SECOND_CALL),
            (NoteOutcome::Acted, FIRST_CALL),
            (NoteOutcome::FellBack, FIRST_CALL),
        ] {
            chat.handle_event(decision(outcome, NoteAnchor::Tool(call.into())), None);
        }

        assert_eq!(
            live_placement(&chat),
            [
                format!("{FIRST_CALL} {FIRST_CALL}=acted {FIRST_CALL}=fell_back"),
                format!("{SECOND_CALL} {SECOND_CALL}=shadow"),
            ]
        );
    }

    #[test]
    fn a_batch_childs_decision_lands_on_its_batch() {
        let mut chat = chat();
        chat.handle_event(batch_start(SECOND_CALL, 1), None);
        chat.handle_event(tool_start(FIRST_CALL, SHELL_TOOL_NAME), None);

        chat.handle_event(
            decision(
                NoteOutcome::Acted,
                NoteAnchor::Tool(SECOND_CALL_CHILD.into()),
            ),
            None,
        );

        assert_eq!(
            live_placement(&chat),
            [
                format!("{SECOND_CALL} {SECOND_CALL_CHILD}=acted"),
                FIRST_CALL.to_owned(),
            ]
        );
    }

    /// A recall started by a todo update waits on that card. Answered with
    /// nothing to do, it stays as a quiet chip there, as a call's own wait
    /// would not.
    #[test_case(DecisionFeature::MemoryRecall, Some(QUIET_STATE) ; "a_recall_stays_quiet")]
    #[test_case(DecisionFeature::SourceCodeContext, None ; "a_calls_own_wait_goes")]
    fn a_quiet_answer_on_a_card_stays_only_when_it_was_not_the_calls_own(
        feature: DecisionFeature,
        state: Option<&str>,
    ) {
        let mut chat = chat();
        chat.handle_event(tool_start(FIRST_CALL, SHELL_TOOL_NAME), None);
        let anchor = NoteAnchor::Tool(FIRST_CALL.into());
        chat.handle_event(pending_on(feature.clone(), anchor.clone()), None);

        chat.handle_event(settled_on(feature, anchor, vec![NEAR_MISS.into()]), None);

        let expected = match state {
            Some(state) => format!("{FIRST_CALL} {FIRST_CALL}={state}"),
            None => FIRST_CALL.to_owned(),
        };
        assert_eq!(live_placement(&chat), [expected]);
    }

    #[test]
    fn a_decision_for_a_card_this_chat_lacks_is_a_row_that_follows_the_transcript() {
        let mut chat = chat();
        chat.handle_event(tool_start(FIRST_CALL, SHELL_TOOL_NAME), None);
        chat.handle_event(tool_start(SECOND_CALL, SHELL_TOOL_NAME), None);

        chat.handle_event(
            decision(NoteOutcome::Acted, NoteAnchor::Tool(UNKNOWN_CALL.into())),
            None,
        );

        assert_eq!(
            live_placement(&chat),
            [FIRST_CALL, SECOND_CALL, NoteOutcome::Acted.as_str()]
        );
    }

    /// A turn's decision goes on the row the transcript ends on, a card's
    /// header as readily as a prompt, past reasoning and decision rows that
    /// cannot carry one.
    #[test]
    fn a_turns_decision_lands_on_the_last_row_that_can_carry_it() {
        let mut chat = chat();
        prompt(&mut chat);
        chat.handle_event(tool_start(FIRST_CALL, SHELL_TOOL_NAME), None);
        chat.handle_event(
            decision(NoteOutcome::Shadow, NoteAnchor::Tool(UNKNOWN_CALL.into())),
            None,
        );
        chat.messages_panel.push(DisplayMessage::new(
            DisplayRole::Thinking,
            REPLY_TEXT.into(),
        ));

        chat.handle_event(decision(NoteOutcome::Acted, NoteAnchor::History(0)), None);

        assert_eq!(
            live_placement(&chat),
            [
                USER_TEXT.to_owned(),
                turn_label(FIRST_CALL, NoteOutcome::Acted.as_str()),
                NoteOutcome::Shadow.as_str().to_owned(),
                REPLY_TEXT.to_owned(),
            ]
        );
    }

    #[test]
    fn a_turns_decision_with_no_row_to_carry_it_is_an_untimed_row() {
        let mut chat = chat();

        chat.handle_event(decision(NoteOutcome::Acted, NoteAnchor::History(0)), None);

        let row = chat.message_at(0).unwrap();
        assert_eq!(
            (&row.role, row.decisions.is_empty()),
            (&DisplayRole::Decision(NoteOutcome::Acted, None), true)
        );
    }

    /// The card says the engine is working until it answers: with a note,
    /// which takes the wait's place, with nothing to show, or by the call
    /// ending first.
    #[test_case(Some(decision(NoteOutcome::Acted, NoteAnchor::Tool(FIRST_CALL.into()))), format!("{FIRST_CALL} {FIRST_CALL}=acted") ; "a_note_takes_its_place")]
    #[test_case(Some(settled(NoteAnchor::Tool(FIRST_CALL.into()))), FIRST_CALL.to_owned() ; "a_quiet_answer_clears_it")]
    #[test_case(Some(tool_done(FIRST_CALL, SHELL_TOOL_NAME, ToolOutput::Plain(String::new().into()))), FIRST_CALL.to_owned() ; "the_call_ending_clears_it")]
    #[test_case(None, format!("{FIRST_CALL} {FIRST_CALL}={PENDING_STATE}") ; "until_then_it_waits")]
    fn a_wait_shows_on_the_card_until_the_engine_answers(answer: Option<AgentEvent>, card: String) {
        let mut chat = chat();
        chat.handle_event(tool_start(FIRST_CALL, SHELL_TOOL_NAME), None);
        chat.handle_event(pending(NoteAnchor::Tool(FIRST_CALL.into())), None);

        if let Some(answer) = answer {
            chat.handle_event(answer, None);
        }

        assert_eq!(live_placement(&chat), [card]);
    }

    #[test]
    fn a_calls_wait_without_its_card_draws_nothing() {
        let mut chat = chat();
        chat.handle_event(tool_start(FIRST_CALL, SHELL_TOOL_NAME), None);

        chat.handle_event(pending(NoteAnchor::Tool(UNKNOWN_CALL.into())), None);

        assert_eq!(live_placement(&chat), [FIRST_CALL]);
    }

    /// A turn's wait is a chip on the row it was asked about until the
    /// engine answers, however many rows follow: a note takes its place and a
    /// quiet answer leaves a quiet chip.
    #[test_case(None, PENDING_STATE ; "until_then_it_waits")]
    #[test_case(Some(decision(NoteOutcome::Acted, NoteAnchor::History(0))), NoteOutcome::Acted.as_str() ; "a_note_takes_its_place")]
    #[test_case(Some(settled(NoteAnchor::History(0))), QUIET_STATE ; "a_quiet_answer_keeps_a_quiet_chip")]
    fn a_turns_wait_is_a_chip_on_its_host_until_the_engine_answers(
        answer: Option<AgentEvent>,
        state: &str,
    ) {
        let mut chat = chat();
        prompt(&mut chat);
        chat.handle_event(pending(NoteAnchor::History(0)), None);
        chat.handle_event(tool_start(FIRST_CALL, SHELL_TOOL_NAME), None);

        if let Some(answer) = answer {
            chat.handle_event(answer, None);
        }

        assert_eq!(
            live_placement(&chat),
            [turn_label(USER_TEXT, state), FIRST_CALL.to_owned()]
        );
    }

    #[test]
    fn a_turns_note_is_timed_by_the_wait_it_replaces() {
        let _clock = FrozenClock::at(TURN_WAIT);
        let mut chat = chat();
        prompt(&mut chat);
        chat.handle_event(pending(NoteAnchor::History(0)), None);

        chat.handle_event(decision(NoteOutcome::Acted, NoteAnchor::History(0)), None);

        let host = chat.message_at(0).unwrap();
        assert!(matches!(
            host.decisions.as_slice(),
            [CardDecision {
                state: DecisionState::Settled {
                    took: Some(TURN_WAIT),
                    ..
                },
                ..
            }]
        ));
    }

    #[test]
    fn a_quiet_answer_keeps_how_long_it_took_and_what_came_closest() {
        let _clock = FrozenClock::at(TURN_WAIT);
        let mut chat = chat();
        prompt(&mut chat);
        chat.handle_event(pending(NoteAnchor::History(0)), None);

        chat.handle_event(
            settled_on(
                DecisionFeature::MemorySearch,
                NoteAnchor::History(0),
                vec![NEAR_MISS.into()],
            ),
            None,
        );

        let host = chat.message_at(0).unwrap();
        let [
            CardDecision {
                state: DecisionState::Quiet { text, took },
                ..
            },
        ] = host.decisions.as_slice()
        else {
            panic!("{:?}", host.decisions);
        };
        assert_eq!(
            (text.lines().skip(1).collect::<Vec<_>>(), *took),
            (vec![NEAR_MISS], Some(TURN_WAIT))
        );
    }

    /// The reminders a first turn carries come after its prompt, and the chip
    /// still goes on the prompt.
    #[test]
    fn a_turn_with_nothing_to_ask_leaves_an_untimed_quiet_chip_saying_why() {
        let mut chat = chat();
        prompt(&mut chat);
        chat.messages_panel
            .push(DisplayMessage::injected(INJECTED_TEXT.into(), None));

        chat.handle_event(
            settled_on(
                DecisionFeature::MemoryRecall,
                NoteAnchor::History(0),
                vec![NEAR_MISS.into()],
            ),
            None,
        );

        let host = chat.message_at(0).unwrap();
        let [
            CardDecision {
                state: DecisionState::Quiet { text, took: None },
                ..
            },
        ] = host.decisions.as_slice()
        else {
            panic!("{:?}", host.decisions);
        };
        assert_eq!(text.lines().skip(1).collect::<Vec<_>>(), [NEAR_MISS]);
    }

    /// Recall and skill matching ask the engine side by side, so each answer
    /// settles the wait of its own feature, whichever comes first, on the row
    /// both were asked about.
    #[test]
    fn concurrent_turn_waits_are_matched_by_feature() {
        let mut chat = chat();
        prompt(&mut chat);
        for feature in [
            DecisionFeature::MemoryRecall,
            DecisionFeature::SkillSuggestions,
        ] {
            chat.handle_event(pending_on(feature, NoteAnchor::History(0)), None);
        }
        chat.handle_event(tool_start(FIRST_CALL, SHELL_TOOL_NAME), None);

        chat.handle_event(
            turn_decision(
                DecisionFeature::SkillSuggestions,
                NoteOutcome::Shadow,
                NoteAnchor::History(0),
            ),
            None,
        );
        let pending_and_shadow = format!(
            "{} {TURN_SUBJECT}={}",
            turn_label(USER_TEXT, PENDING_STATE),
            NoteOutcome::Shadow.as_str()
        );
        assert_eq!(
            live_placement(&chat),
            [pending_and_shadow, FIRST_CALL.to_owned()]
        );

        chat.handle_event(
            settled_on(
                DecisionFeature::MemoryRecall,
                NoteAnchor::History(0),
                Vec::new(),
            ),
            None,
        );
        let quiet_and_shadow = format!(
            "{} {TURN_SUBJECT}={}",
            turn_label(USER_TEXT, QUIET_STATE),
            NoteOutcome::Shadow.as_str()
        );
        assert_eq!(
            live_placement(&chat),
            [quiet_and_shadow, FIRST_CALL.to_owned()]
        );
    }

    /// A turn's wait that lands on a card stays there when the card's call
    /// ends, since it is the turn's to answer, not the call's.
    #[test]
    fn a_turns_wait_on_a_card_outlives_the_call() {
        let mut chat = chat();
        chat.handle_event(tool_start(FIRST_CALL, SHELL_TOOL_NAME), None);
        chat.handle_event(pending(NoteAnchor::History(0)), None);

        chat.handle_event(
            tool_done(
                FIRST_CALL,
                SHELL_TOOL_NAME,
                ToolOutput::Plain(String::new().into()),
            ),
            None,
        );

        assert_eq!(
            live_placement(&chat),
            [turn_label(FIRST_CALL, PENDING_STATE)]
        );
    }

    #[test]
    fn a_live_row_titles_the_feature_and_lists_its_details() {
        let mut chat = chat();

        chat.handle_event(decision(NoteOutcome::Shadow, NoteAnchor::History(0)), None);

        let row = chat.message_at(0).unwrap();
        let title = format!(
            "{SHADOW_MARKER}{}{DECISION_SEPARATOR}{DECISION_SUMMARY}",
            DecisionFeature::MemorySearch.label()
        );
        assert_eq!(row.text, format!("{title}\n{DECISION_DETAIL}"));
    }

    #[test]
    fn hidden_decisions_draw_no_row() {
        let mut chat = Chat::new(
            MAIN_NAME.into(),
            Path::new(SESSION_CWD),
            UiConfig {
                show_decisions: false,
                ..UiConfig::default()
            },
            caudra_lua::EventHandle::disconnected_for_test(),
        );
        chat.handle_event(tool_start(FIRST_CALL, SHELL_TOOL_NAME), None);

        for anchor in [NoteAnchor::History(0), NoteAnchor::Tool(FIRST_CALL.into())] {
            chat.handle_event(pending(anchor.clone()), None);
            assert_eq!(live_placement(&chat), [FIRST_CALL]);
            chat.handle_event(decision(NoteOutcome::Acted, anchor), None);
        }

        assert_eq!(live_placement(&chat), [FIRST_CALL]);
    }

    fn restored_call_card(call_id: &str) -> DisplayMessage {
        DisplayMessage::new(
            DisplayRole::Tool(Box::new(ToolRole {
                id: call_id.into(),
                status: ToolStatus::Success,
                name: SHELL_TOOL_NAME.into(),
                effect: ToolEffect::Unknown,
            })),
            String::new(),
        )
    }

    fn restored_card_decision(subject: DecisionSubject, summary: &str) -> CardDecision {
        CardDecision::settled(
            subject,
            DecisionFeature::MemorySearch,
            NoteOutcome::Acted,
            summary,
            &[],
        )
    }

    #[test_case(&[FIRST_CALL, FIRST_CALL], Some(FIRST_CALL), Some(0); "first_duplicate_card_wins")]
    #[test_case(&[SECOND_CALL, SECOND_CALL_CHILD], Some(SECOND_CALL_CHILD), Some(1); "exact_child_beats_earlier_parent")]
    #[test_case(&[SECOND_CALL, SECOND_CALL_CHILD, NESTED_CALL_CHILD], Some(NESTED_CALL_CHILD), Some(2); "exact_nested_child_beats_ancestors")]
    #[test_case(&[SECOND_CALL, SECOND_CALL_CHILD], Some(NESTED_CALL_CHILD), Some(1); "nearest_ancestor_wins")]
    #[test_case(&[SECOND_CALL], Some(NESTED_CALL_CHILD), Some(0); "nested_child_falls_back_to_root")]
    #[test_case(&[SECOND_CALL], Some(INVALID_CALL_CHILD), None; "invalid_suffix_does_not_fall_back")]
    #[test_case(&[SECOND_CALL, INVALID_CALL_CHILD], Some(INVALID_CALL_CHILD), Some(1); "exact_nonnumeric_id_is_valid")]
    #[test_case(&[FIRST_CALL], Some(UNKNOWN_CALL), None; "missing_call_is_dropped")]
    #[test_case(&[], Some(FIRST_CALL), None; "no_tool_cards")]
    #[test_case(&[FIRST_CALL], None, None; "turn_decisions_are_not_attached")]
    fn restored_card_decisions_preserve_target_and_order(
        call_ids: &[&str],
        subject: Option<&str>,
        target: Option<usize>,
    ) {
        let mut display: Vec<_> = call_ids.iter().map(|id| restored_call_card(id)).collect();
        display.insert(0, DisplayMessage::new(DisplayRole::User, USER_TEXT.into()));
        let existing = restored_card_decision(DecisionSubject::Turn, TURN_ROW);
        for message in &mut display {
            message.decisions.push(existing.clone());
        }
        let subject = subject.map_or(DecisionSubject::Turn, |id| DecisionSubject::Call(id.into()));
        let cards = vec![
            restored_card_decision(subject.clone(), FIRST_CALL_ROW),
            restored_card_decision(subject, SECOND_CALL_ROW),
        ];

        attach_card_decisions(&mut display, cards.clone());

        for (index, message) in display.iter().enumerate() {
            let mut expected = vec![existing.clone()];
            if target.is_some_and(|target| index == target + 1) {
                expected.extend(cards.clone());
            }
            assert_eq!(message.decisions, expected);
        }
    }

    fn reference_visible_user_text(
        items: &[HistoryItem],
        group_id: CaudraId,
    ) -> Option<(CaudraId, &str)> {
        let mut users = items.iter().filter(|item| item.group_id == group_id);
        let first = users
            .clone()
            .find(|item| matches!(item.kind, HistoryItemKind::User { .. }))?;
        let HistoryItemKind::User {
            display_text,
            origin: UserOrigin::Turn,
            ..
        } = &first.kind
        else {
            return None;
        };
        if let Some(display_text) = display_text {
            return (!display_text.is_empty()).then_some((first.id, display_text.as_str()));
        }
        users.find_map(|item| match &item.kind {
            HistoryItemKind::User { text, .. } if !text.trim().is_empty() => {
                Some((item.id, text.as_str()))
            }
            _ => None,
        })
    }

    fn reference_attach_card_decisions(display: &mut [DisplayMessage], cards: Vec<CardDecision>) {
        for decision in cards {
            let DecisionSubject::Call(call_id) = &decision.subject else {
                continue;
            };
            let mut call_id = call_id.as_str();
            let card = loop {
                if let Some(card) = display
                    .iter()
                    .position(|message| message.role.tool_id() == Some(call_id))
                {
                    break Some(card);
                }
                match batch_child_id(call_id) {
                    Some((parent, _)) => call_id = parent,
                    None => break None,
                }
            };
            if let Some(card) = card {
                display[card].decisions.push(decision);
            }
        }
    }

    #[test]
    #[ignore = "manual before/after session-load scaling measurements"]
    fn session_load_indexes_scaling() {
        for size in [
            LOAD_SCALING_BASE,
            LOAD_SCALING_BASE * 2,
            LOAD_SCALING_BASE * 4,
        ] {
            let messages = vec![Message::user(USER_TEXT.into()); size * 2];
            let mut items = crate::history_items(&messages);
            let groups: Vec<_> = items[..size].iter().map(|item| item.group_id).collect();
            for (index, item) in items.iter_mut().enumerate() {
                item.group_id = groups[index % size];
                if index < size
                    && let HistoryItemKind::User { text, .. } = &mut item.kind
                {
                    *text = BLANK_USER_TEXT.into();
                }
            }
            let started = Instant::now();
            let before: Vec<_> = groups
                .iter()
                .map(|&group| reference_visible_user_text(black_box(&items), group))
                .collect();
            let user_before = started.elapsed();
            let started = Instant::now();
            let mut index = build_user_text_map(black_box(&items));
            let after: Vec<_> = groups
                .iter()
                .map(|group| match index.remove(group) {
                    Some(UserGroupText::Visible(id, text)) => Some((id, text)),
                    _ => None,
                })
                .collect();
            let user_after = started.elapsed();
            assert_eq!(before, after);

            let call_ids: Vec<_> = (0..size)
                .map(|index| format!("{FIRST_CALL}-{index}"))
                .collect();
            let mut before: Vec<_> = call_ids.iter().map(|id| restored_call_card(id)).collect();
            before.extend(call_ids.iter().map(|id| restored_call_card(id)));
            let mut after = before.clone();
            let cards: Vec<_> = call_ids
                .iter()
                .flat_map(|id| {
                    [
                        restored_card_decision(
                            DecisionSubject::Call(format!("{id}:0:1")),
                            CHILD_ROW,
                        ),
                        restored_card_decision(DecisionSubject::Call(id.clone()), FIRST_CALL_ROW),
                    ]
                })
                .collect();
            let after_cards = cards.clone();
            let started = Instant::now();
            reference_attach_card_decisions(black_box(&mut before), cards);
            let cards_before = started.elapsed();
            let started = Instant::now();
            attach_card_decisions(black_box(&mut after), after_cards);
            let cards_after = started.elapsed();
            for (before, after) in before.iter().zip(&after) {
                assert_eq!(before.decisions, after.decisions);
            }
            println!(
                "groups={size} user_before={user_before:?} user_after={user_after:?} cards_before={cards_before:?} cards_after={cards_after:?}"
            );
        }
    }

    fn stored(feature: &str, summary: &str, anchor: StoredDecisionAnchor) -> StoredEngineDecision {
        StoredEngineDecision {
            seq: 0,
            at_ms: 0,
            feature: feature.into(),
            mode: FeatureMode::Advise.as_str().into(),
            outcome: StoredDecisionOutcome::Acted,
            summary: summary.into(),
            details: Vec::new(),
            agent: None,
            anchor,
            screening: None,
        }
    }

    fn call(id: &str) -> [Message; 2] {
        [
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    id,
                    SHELL_TOOL_NAME,
                    serde_json::json!({}),
                )],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: id.into(),
                    content: REPLY_TEXT.into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            },
        ]
    }

    /// A compaction keeps the first call's pair after its summary, so the
    /// transcript holds that call twice and its decision stays on the first
    /// card. A turn's decision counts items across the seam and goes on the
    /// last row drawn before its position, untimed, a batch child's
    /// decision goes on its batch's card, a decision whose call is gone is
    /// dropped, and one this build cannot name keeps a row under its card.
    #[test]
    fn stored_rows_restore_among_the_history_across_a_compaction() {
        let mut summary = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: COMPACTION_SUMMARY.into(),
            }],
            ..Default::default()
        };
        summary.is_compaction_summary = true;
        let mut msgs = vec![Message::user(USER_TEXT.into())];
        msgs.extend(call(FIRST_CALL));
        let seam = msgs.len();
        msgs.push(Message::synthetic(COMPACTION_ANCHOR.into()));
        msgs.push(summary);
        msgs.extend(call(FIRST_CALL));
        msgs.extend(call(SECOND_CALL));
        let items = crate::history_items(&msgs);
        let feature = DecisionFeature::MemorySearch.name();
        let rows = [
            stored(feature, TURN_ROW, StoredDecisionAnchor::History(seam)),
            stored(
                feature,
                UNKNOWN_CALL_ROW,
                StoredDecisionAnchor::Tool(UNKNOWN_CALL.into()),
            ),
            stored(
                feature,
                SECOND_CALL_ROW,
                StoredDecisionAnchor::Tool(SECOND_CALL.into()),
            ),
            stored(
                feature,
                CHILD_ROW,
                StoredDecisionAnchor::Tool(SECOND_CALL_CHILD.into()),
            ),
            stored(
                feature,
                FIRST_CALL_ROW,
                StoredDecisionAnchor::Tool(FIRST_CALL.into()),
            ),
            stored(
                RETIRED_FEATURE,
                DECISION_SUMMARY,
                StoredDecisionAnchor::Tool(SECOND_CALL.into()),
            ),
        ];

        let restored = decision_inserts(&items, &rows);
        let (mut display, _) = interleaved_history_to_display(
            &items,
            &empty_outputs(),
            &ToolOutputLines::default(),
            false,
            restored.rows,
            restored.turns,
        );
        attach_card_decisions(&mut display, restored.cards);

        let summary = |text: &str| text.rsplit(DECISION_SEPARATOR).next().unwrap().to_owned();
        let shown: Vec<(String, Vec<String>)> = display
            .iter()
            .map(|message| {
                let decisions = message
                    .decisions
                    .iter()
                    .map(|decision| match &decision.state {
                        DecisionState::Settled { text, .. } => summary(text),
                        DecisionState::Pending { .. } | DecisionState::Quiet { .. } => {
                            String::new()
                        }
                    })
                    .collect();
                let shown = match &message.role {
                    DisplayRole::Tool(tool) => tool.id.clone(),
                    DisplayRole::Decision(..) => summary(&message.text),
                    _ => message.text.clone(),
                };
                (shown, decisions)
            })
            .collect();
        let row = |text: &str| (text.to_owned(), Vec::new());
        assert_eq!(
            shown,
            [
                row(USER_TEXT),
                (
                    FIRST_CALL.into(),
                    vec![TURN_ROW.into(), FIRST_CALL_ROW.into()]
                ),
                row(COMPACTION_BORDER_TEXT),
                row(COMPACTION_SUMMARY),
                row(FIRST_CALL),
                (
                    SECOND_CALL.into(),
                    vec![SECOND_CALL_ROW.into(), CHILD_ROW.into()]
                ),
                row(DECISION_SUMMARY),
            ]
        );
    }

    /// Each restored turn decision as the rows it is drawn among: a host
    /// lists its chips' summaries, a row of its own is its summary in
    /// brackets.
    fn restored_turn(position: usize, feature: &str) -> Vec<String> {
        let msgs = [
            Message::user(USER_TEXT.into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: REPLY_TEXT.into(),
                }],
                ..Default::default()
            },
        ];
        let items = crate::history_items(&msgs);
        let rows = [stored(
            feature,
            TURN_ROW,
            StoredDecisionAnchor::History(position),
        )];
        let restored = decision_inserts(&items, &rows);
        let (display, _) = interleaved_history_to_display(
            &items,
            &empty_outputs(),
            &ToolOutputLines::default(),
            false,
            restored.rows,
            restored.turns,
        );
        display
            .iter()
            .map(|message| {
                let mut shown = match &message.role {
                    DisplayRole::Decision(..) => format!("[{TURN_ROW}]"),
                    _ => message.text.clone(),
                };
                for decision in &message.decisions {
                    let DecisionState::Settled { text, took, .. } = &decision.state else {
                        panic!("{decision:?}");
                    };
                    assert_eq!(*took, None);
                    shown.push_str(&format!(
                        " {}",
                        text.rsplit(DECISION_SEPARATOR).next().unwrap()
                    ));
                }
                shown
            })
            .collect()
    }

    #[test_case(1, &[format!("{USER_TEXT} {TURN_ROW}"), REPLY_TEXT.to_owned()] ; "before_the_reply_on_the_prompt")]
    #[test_case(2, &[USER_TEXT.to_owned(), format!("{REPLY_TEXT} {TURN_ROW}")] ; "after_the_reply_on_the_reply")]
    #[test_case(0, &[format!("[{TURN_ROW}]"), USER_TEXT.to_owned(), REPLY_TEXT.to_owned()] ; "before_everything_a_row_of_its_own")]
    fn a_restored_turns_decision_goes_on_the_last_row_before_its_position(
        position: usize,
        shown: &[String],
    ) {
        assert_eq!(
            restored_turn(position, DecisionFeature::MemoryRecall.name()),
            shown
        );
    }

    #[test_case(Some(ScreeningVerdict::TimedOut.as_str()), Some(ScreeningVerdict::TimedOut) ; "a_failure_keeps_its_cause")]
    #[test_case(Some(ScreeningVerdict::Flagged.as_str()), Some(ScreeningVerdict::Flagged) ; "a_flag_stays_a_flag")]
    #[test_case(None, None ; "a_row_stored_before_screening_said_stays_neutral")]
    #[test_case(Some(UNKNOWN_SCREENING), None ; "a_cause_this_build_cannot_name_stays_neutral")]
    fn a_restored_screening_keeps_what_it_concluded(
        stored_screening: Option<&str>,
        expected: Option<ScreeningVerdict>,
    ) {
        let row = StoredEngineDecision {
            screening: stored_screening.map(str::to_owned),
            ..stored(
                DecisionFeature::ContentScreening.name(),
                DECISION_SUMMARY,
                StoredDecisionAnchor::History(0),
            )
        };

        let restored = decision_inserts(&[], [&row]);

        let [(_, decision)] = restored.turns.as_slice() else {
            panic!("one restored turn decision");
        };
        assert!(
            matches!(decision.state, DecisionState::Settled { screening, .. } if screening == expected),
            "{decision:?}"
        );
    }

    #[test]
    fn a_restored_turns_decision_this_build_cannot_name_keeps_a_row() {
        assert_eq!(
            restored_turn(1, RETIRED_FEATURE),
            [
                USER_TEXT.to_owned(),
                format!("[{TURN_ROW}]"),
                REPLY_TEXT.to_owned()
            ]
        );
    }

    #[test_case(DecisionFeature::MemorySearch.name(), DecisionFeature::MemorySearch.label() ; "a_known_feature_by_its_label")]
    #[test_case(RETIRED_FEATURE, RETIRED_FEATURE ; "an_unknown_feature_by_its_stored_name")]
    fn a_stored_row_is_titled_by_its_feature(feature: &str, title: &str) {
        let row = stored(feature, DECISION_SUMMARY, StoredDecisionAnchor::History(0));

        assert_eq!(
            stored_decision_row(&row).text,
            format!("{title}{DECISION_SEPARATOR}{DECISION_SUMMARY}")
        );
    }

    const CALIBRATED_MODEL_SPEC: &str = "anthropic/claude-sonnet-4-6";
    /// A model id no other test learns about, since the registry is
    /// process-wide.
    const CALIBRATED_MODEL_ID: &str = "restored-card-calibration";
    const CALIBRATED_ESTIMATE: u32 = 1_000;
    const CALIBRATED_REPORTED: u32 = 1_500;
    const RESTORED_SCALE_MSG: &str = "a restored card counts at its caller model's current ratio";

    #[test]
    fn a_restored_card_counts_at_its_callers_current_ratio() {
        let mut model = Model::from_spec(CALIBRATED_MODEL_SPEC).unwrap();
        model.id = CALIBRATED_MODEL_ID.into();
        calibration::observe(&model, CALIBRATED_ESTIMATE, CALIBRATED_REPORTED).unwrap();
        let caller = ReasoningSource::new(&model, ReasoningTransport::Other);
        let output = Arc::new(ToolOutput::Plain("the quick brown fox\n".repeat(50).into()));

        let (_, _, _, annotation) = build_loaded_tool(
            SHELL_TOOL_NAME,
            "",
            Some(Arc::clone(&output)),
            None,
            &ToolOutputLines::default(),
            Some(&caller),
        );

        assert_eq!(
            annotation,
            output.annotation(calibration::scale(&model)),
            "{RESTORED_SCALE_MSG}"
        );
        assert_ne!(
            annotation,
            output.annotation(TokenScale::UNIT),
            "{RESTORED_SCALE_MSG}"
        );
    }

    const STAMP_PROVIDER: &str = "anthropic";
    const STAMP_OPUS: &str = "claude-opus-5-5";
    const STAMP_SONNET: &str = "claude-sonnet-5";
    const STAMP_HIGH: &str = "high";
    const STAMP_LOW: &str = "low";
    const THINKING_NOTE: &str = "Thinking: high → low";
    const MODEL_NOTE: &str = "Model: anthropic/claude-opus-5-5 → anthropic/claude-sonnet-5";
    const ROUTE_ROW: &str = "Moved to another provider";

    fn stamp(model: &str, thinking: &str) -> ReasoningSource {
        ReasoningSource {
            provider: STAMP_PROVIDER.into(),
            model: model.into(),
            transport: ReasoningTransport::Other,
            thinking: Some(thinking.into()),
        }
    }

    fn stamped_reply(block: ContentBlock, source: Option<ReasoningSource>) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![block],
            reasoning_source: source,
            ..Default::default()
        }
    }

    fn reply_text() -> ContentBlock {
        ContentBlock::Text {
            text: REPLY_TEXT.into(),
        }
    }

    fn notes(display: &[DisplayMessage]) -> Vec<&str> {
        display
            .iter()
            .filter(|row| row.role == DisplayRole::Notice)
            .map(|row| row.text.as_str())
            .collect()
    }

    /// Each reply says who wrote it, and a change between neighbours is
    /// noted where it took effect, derived from the stamps alone.
    #[test]
    fn restored_replies_carry_their_producer_and_note_each_change() {
        let messages = vec![
            Message::user(USER_TEXT.into()),
            stamped_reply(reply_text(), Some(stamp(STAMP_OPUS, STAMP_HIGH))),
            Message::user(USER_TEXT.into()),
            stamped_reply(reply_text(), Some(stamp(STAMP_OPUS, STAMP_LOW))),
            Message::user(USER_TEXT.into()),
            stamped_reply(
                ContentBlock::tool_use(
                    "t1",
                    SHELL_TOOL_NAME,
                    serde_json::json!({"command": "true"}),
                ),
                Some(stamp(STAMP_SONNET, STAMP_LOW)),
            ),
        ];

        let display = display_messages(&messages, &empty_outputs()).0;

        assert_eq!(notes(&display), [THINKING_NOTE, MODEL_NOTE]);
        let last = display.last().unwrap();
        assert!(matches!(last.role, DisplayRole::Tool(_)));
        assert_eq!(
            last.producer,
            Some(Producer::from(&stamp(STAMP_SONNET, STAMP_LOW)))
        );
        let note_at = display
            .iter()
            .position(|row| row.text == MODEL_NOTE)
            .unwrap();
        assert!(matches!(display[note_at + 1].role, DisplayRole::Tool(_)));
        assert!(
            display
                .iter()
                .filter(|row| row.role == DisplayRole::User)
                .all(|row| row.producer.is_none())
        );
    }

    /// A route row already says how the conversation moved, a compaction
    /// summary is another model's, and a reply saved before the stamp says
    /// nothing either way.
    #[test_case(Some(stamp(STAMP_OPUS, STAMP_HIGH)), true, false ; "after_a_route_row")]
    #[test_case(None, false, false ; "unstamped_neighbour")]
    #[test_case(Some(stamp(STAMP_OPUS, STAMP_HIGH)), false, true ; "after_a_compaction_summary")]
    fn some_changes_go_unnoted(first: Option<ReasoningSource>, route_row: bool, compacted: bool) {
        let mut summary = stamped_reply(reply_text(), Some(stamp(STAMP_OPUS, STAMP_HIGH)));
        summary.is_compaction_summary = true;
        let mut messages = vec![
            Message::user(USER_TEXT.into()),
            stamped_reply(reply_text(), first),
        ];
        if compacted {
            messages.push(Message::synthetic(COMPACTION_ANCHOR.into()));
            messages.push(summary);
        }
        messages.push(Message::user(USER_TEXT.into()));
        messages.push(stamped_reply(
            reply_text(),
            Some(stamp(STAMP_SONNET, STAMP_LOW)),
        ));
        let items = crate::history_items(&messages);
        let inserts = route_row
            .then(|| {
                (
                    items.len() - 1,
                    DisplayMessage::new(DisplayRole::Notice, ROUTE_ROW.into()),
                )
            })
            .into_iter()
            .collect();

        let display = interleaved_history_to_display(
            &items,
            &empty_outputs(),
            &ToolOutputLines::default(),
            true,
            inserts,
            Vec::new(),
        )
        .0;

        assert!(
            !notes(&display)
                .iter()
                .any(|note| note.starts_with(MODEL_NOTE)),
            "{:?}",
            notes(&display)
        );
    }

    /// Live, the note lands above the first row a change wrote, and every row
    /// is stamped with its request's settings. A request that wrote nothing,
    /// as one that failed, leaves no note, as a restore has nothing of it.
    #[test]
    fn a_live_change_is_noted_and_stamped_as_a_restore_would() {
        let mut chat = chat();
        let started = |source: &ReasoningSource| AgentEvent::MessageStarted {
            id: CaudraId::generate(),
            model: source.spec(),
            thinking: source.thinking.clone(),
        };
        chat.handle_event(started(&stamp(STAMP_OPUS, STAMP_HIGH)), None);
        chat.handle_event(
            AgentEvent::TextDelta {
                text: REPLY_TEXT.into(),
            },
            None,
        );
        chat.handle_event(started(&stamp(STAMP_SONNET, STAMP_HIGH)), None);
        chat.handle_event(started(&stamp(STAMP_OPUS, STAMP_LOW)), None);
        chat.handle_event(
            AgentEvent::TextDelta {
                text: REPLY_TEXT.into(),
            },
            None,
        );
        chat.flush();

        let rows: Vec<&DisplayMessage> = (0..chat.message_count())
            .filter_map(|index| chat.message_at(index))
            .collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0].producer,
            Some(Producer::from(&stamp(STAMP_OPUS, STAMP_HIGH)))
        );
        assert_eq!(rows[1].role, DisplayRole::Notice);
        assert_eq!(rows[1].text, THINKING_NOTE);
        assert_eq!(
            rows[2].producer,
            Some(Producer::from(&stamp(STAMP_OPUS, STAMP_LOW)))
        );
    }
}
