use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use crate::app::tasks::TaskOutcome;
use crate::chat::{Chat, DONE_TEXT, history_to_display};
use crate::components::rewind_picker::RewindEntry;
use crate::components::{Action, DisplaySource, ForkDraft, ForkedSession, LoadedSession};
use crate::input_document::InputDraft;
use maki_agent::agent::estimate_message_tokens;
use maki_agent::snapshots::{
    ConflictPolicy, RestoreReport, RestoreStatus, RestoreTarget, SnapshotError, SnapshotStore,
};
use maki_agent::{GoalStatus, GoalVerdict};
use maki_providers::{
    HistoryItem, HistoryItemKind, ImageSource, Model, TokenUsage, active_history_items,
    merge_history_items, project_messages,
};
use maki_storage::id::MakiId;
use maki_storage::sessions::{
    PendingConversationRevert, PendingRestoreKind, PendingRestoreOperation, PendingRestorePhase,
    SessionMeta, StoredGoalResult, StoredGoalVerdict, StoredImage, StoredPasteRange,
    StoredQueuedDraft, StoredSubagent,
};

use crate::AppSession;
use crate::storage_writer::StorageWriter;

use super::session_state::{SessionState, rules_to_stored, stored_to_rules};
use super::{App, Mode, PendingInput, PlanState, RestoreMode, Status};

/// The shortest gap between two writes that carry only UI state.
const SOFT_SAVE_DELAY: Duration = Duration::from_millis(1000);
pub(crate) const REVERT_BUSY_MSG: &str = "Wait for the session to become idle before reverting";

struct RevertTarget {
    head: Option<MakiId>,
    draft: Option<(String, Vec<ImageSource>)>,
}

/// What `App::checkpoint` last handed to the writer: which session, how far
/// along it was, and when. The id is part of it because a session swapped into
/// the tab starts its revisions back at zero and would otherwise look older
/// than the stamp left by the one it replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Sent {
    pub id: MakiId,
    pub revision: u64,
    pub content_revision: u64,
    pub at: Instant,
}

/// The one content check: `App::checkpoint` saves a session only when this
/// holds, and the shutdown report reuses it to say which tabs were saved, so
/// the report and the disk can never disagree.
pub(crate) fn session_has_content(session: &AppSession) -> bool {
    !session.messages().is_empty()
        || session.meta.input_draft.is_some()
        || !session.meta.input_draft_images.is_empty()
        || !session.meta.queued_messages.is_empty()
        || !session.meta.unsent_subagent_messages.is_empty()
        || session.meta.active_goal.is_some()
        || session.meta.goal_result.is_some()
        || session.meta.pending_revert.is_some()
        || session.meta.mode != Some(maki_storage::sessions::StoredMode::Build)
}

impl App {
    pub(crate) fn has_content(&self) -> bool {
        session_has_content(&self.state.session)
    }

    /// The event loop runs this once per frame per session. It syncs whatever
    /// the session mirrors from live state, then writes only if a mutator
    /// really changed something. No dirty flags and no per-event save calls,
    /// so there is nothing left to forget.
    pub(crate) fn checkpoint(&mut self) {
        self.checkpoint_with(SOFT_SAVE_DELAY);
    }

    /// A checkpoint for the paths that get no later frame, so a draft typed a
    /// keystroke ago still reaches disk: shutdown, and swapping the session out
    /// from under the tab.
    pub(crate) fn checkpoint_now(&mut self) {
        self.checkpoint_with(Duration::ZERO);
    }

    pub(super) fn checkpoint_with(&mut self, soft_delay: Duration) {
        let snapshot = self.shared_history.as_ref().map(|h| h.load_full());
        let mut meta = self.build_meta();
        if let Some(snapshot) = snapshot.as_deref() {
            let known: HashSet<_> = self
                .state
                .session
                .messages()
                .iter()
                .map(|item| item.id)
                .collect();
            let added = snapshot
                .messages
                .iter()
                .any(|item| !known.contains(&item.id));
            let sanitizer_only = added
                && crate::active_session_history(&self.state.session).is_ok_and(|active| {
                    is_sanitizer_only_unavailable_extension(&active, &snapshot.messages)
                });
            let actual_work_added = added && !sanitizer_only;
            let commits_file_revert = actual_work_added
                && meta
                    .pending_revert
                    .as_ref()
                    .and_then(|pending| pending.file_status.as_ref())
                    .is_some_and(file_restore_succeeded);
            if commits_file_revert && let Err(error) = self.discard_workspace_unrevert() {
                self.status_bar
                    .flash(format!("Failed to commit reverted workspace: {error}"));
                return;
            }
            let mut merged = self.state.session.messages().to_vec();
            match merge_history_items(&mut merged, &snapshot.messages) {
                Ok(()) => {
                    meta.history_head = snapshot.messages.last().map(|item| item.id);
                    if actual_work_added {
                        meta.pending_revert = None;
                    }
                    if merged.as_slice() != self.state.session.messages() {
                        let session = self.state.session_mut();
                        session.merge_history(snapshot, merged);
                        session.update_title_if_default();
                    }
                    if added {
                        let (messages, _) = history_to_display(
                            &snapshot.messages,
                            self.state.session.tool_outputs(),
                            &self.ui_config.tool_output_lines,
                        );
                        self.main_chat().bind_sources(&messages);
                    }
                }
                Err(error) => {
                    tracing::error!(%error, "refusing to checkpoint invalid history graph");
                    self.status_bar
                        .flash(format!("Failed to save session history: {error}"));
                }
            }
        }
        AppSession::checkpoint(&mut self.state.session, None, meta, self.state.token_usage);

        if !self.has_content() {
            // A draft typed and then deleted is already on disk, and a file with
            // nothing in it is a session the picker still offers to resume. Idle
            // only: submitting empties the draft a frame before the agent mirrors
            // the prompt back, and that gap is not an abandoned session.
            let id = self.state.session.id;
            if self.status == Status::Idle && self.last_sent.take_if(|last| last.id == id).is_some()
            {
                self.storage_writer.delete(id, |_| {});
            }
            return;
        }
        let session = &self.state.session;
        let sent = Sent {
            id: session.id,
            revision: session.revision(),
            content_revision: session.content_revision(),
            at: Instant::now(),
        };
        if let Some(last) = &self.last_sent
            && last.id == sent.id
        {
            if last.revision == sent.revision {
                return;
            }
            // Only UI state moved: a keystroke in the draft, the queue or a
            // session rule. Each one costs a meta record plus an fsync, so they
            // land at most once per `soft_delay`, which bounds what a crash
            // takes with it. Anything the agent produced skips the wait.
            if last.content_revision == sent.content_revision && last.at.elapsed() < soft_delay {
                return;
            }
        }

        self.storage_writer.send(Arc::clone(&self.state.session));
        self.last_sent = Some(sent);
    }

    /// Everything the session mirrors from live state, built field by field so
    /// a new `SessionMeta` field forces a decision here. Every frame calls it,
    /// so it stays cheap: an idle UI has an empty draft, queue and rule list,
    /// and an empty `Vec` does not allocate.
    fn build_meta(&self) -> SessionMeta {
        let state = &self.state;
        let draft = self.input_box.draft();
        SessionMeta {
            history_head: state.session.meta.history_head,
            pending_revert: state.session.meta.pending_revert.clone(),
            mode: Some(state.mode.into()),
            plan_path: state.plan.path().map(|p| p.to_string_lossy().into_owned()),
            plan_written: state.plan.is_ready(),
            session_rules: rules_to_stored(&self.permissions.session_rules_snapshot()),
            context_size: state.context_size,
            input_draft: (!draft.is_empty()).then_some(draft.text),
            input_draft_images: self
                .input_box
                .pending_images()
                .iter()
                .map(|image| StoredImage {
                    media_type: image.media_type.mime().into(),
                    data: image.data.to_string(),
                })
                .collect(),
            input_draft_pastes: draft
                .paste_ranges
                .into_iter()
                .map(|range| StoredPasteRange {
                    start: range.start,
                    end: range.end,
                })
                .collect(),
            queued_messages: if self.recoverable_queue.is_empty() {
                self.queue.text_messages()
            } else {
                self.recoverable_queue.clone()
            },
            queued_messages_together: if self.recoverable_queue.is_empty() {
                self.queue.delivery() == maki_agent::QueueDelivery::TogetherNextTurn
            } else {
                self.recoverable_queue_together
            },
            unsent_subagent_messages: self
                .unsent_subagent_steers
                .iter()
                .map(|(task_id, items)| {
                    (
                        task_id.clone(),
                        items
                            .iter()
                            .map(|item| StoredQueuedDraft {
                                text: item.draft.text.clone(),
                                paste_ranges: item
                                    .draft
                                    .paste_ranges
                                    .iter()
                                    .map(|range| StoredPasteRange {
                                        start: range.start,
                                        end: range.end,
                                    })
                                    .collect(),
                            })
                            .collect(),
                    )
                })
                .collect(),
            thinking: Some(state.thinking.into()),
            fast: state.fast,
            workflow: state.workflow,
            active_goal: state.goal.active_condition(),
            goal_result: match state.goal.status() {
                Some(GoalStatus::Finished(goal)) => Some(Box::new(StoredGoalResult {
                    condition: goal.condition.to_string(),
                    verdict: match goal.verdict {
                        GoalVerdict::Met => StoredGoalVerdict::Met,
                        GoalVerdict::Impossible | GoalVerdict::NotMet => {
                            StoredGoalVerdict::Impossible
                        }
                    },
                    reason: goal.reason.to_string(),
                    evaluations: goal.evaluations,
                    duration_ms: goal.duration.as_millis().min(u128::from(u64::MAX)) as u64,
                    usage: goal.usage.billed(goal.cost),
                })),
                Some(GoalStatus::Active(_)) | None => None,
            },
            yolo: self.permissions.persisted_yolo(),
        }
    }

    pub(super) fn sync_subagents(&mut self) {
        let histories = self.state.session.subagent_messages();
        let subagents = self
            .chats
            .iter()
            .skip(1)
            .filter_map(|chat| {
                let task_id = chat.task_id()?;
                if !histories.contains_key(task_id.as_ref())
                    && !self.chat_index.contains_key(task_id.as_ref())
                {
                    return None;
                }
                Some(StoredSubagent {
                    tool_use_id: task_id.to_string(),
                    name: chat.name.clone(),
                    model: chat.model_id.clone(),
                })
            })
            .collect();
        self.state.session_mut().set_subagents(subagents);
    }

    pub(super) fn save_input_history(&self) {
        if let Err(e) = self.input_box.history().save(&self.storage) {
            tracing::warn!(error = %e, "input history save failed");
        }
    }

    pub(super) fn reset_ui_chrome(&mut self) {
        self.cancel_queue_edit();
        self.chats.clear();
        let mut main = Chat::new(
            "Main".into(),
            self.ui_config.clone(),
            self.lua_event_handle.clone(),
        );
        main.set_restore_channel(self.restore_event_tx.clone());
        self.chats.push(main);
        self.active_chat = 0;
        self.chat_index.clear();
        self.subagent_answers.clear();
        self.subagent_steers.clear();
        self.pending_subagent_steers.clear();
        self.unsent_subagent_steers.clear();
        self.parent_task_ids.clear();
        self.subagent_input_box.discard();
        self.subagent_input_task = None;
        self.subagent_drafts.clear();
        self.status = super::Status::Idle;
        self.clear_exit_request();
        self.queue.clear();
        self.task_queue_selection = None;
        self.task_queue_viewport = 0;
        self.queue_hits.clear();
        self.queue_mouse_down = None;
        self.recoverable_queue.clear();
        self.recoverable_queue_together = false;
        self.close_all_overlays();
        self.pending_input = PendingInput::None;
        self.cancelling_run = None;
        self.status_bar.clear_flash();
        self.last_esc = None;
        self.last_exit = None;
        self.restoring = Arc::new(AtomicBool::new(false));
        self.plan_form.reset();
    }

    pub(crate) fn restore_display(&mut self) {
        let restoring = Arc::new(AtomicBool::new(true));
        self.restoring = restoring.clone();

        let active_history = match crate::active_session_history(&self.state.session) {
            Ok(history) => history,
            Err(error) => {
                self.status_bar
                    .flash(format!("Failed to read session history: {error}"));
                Vec::new()
            }
        };
        let (display_msgs, restore_items) = history_to_display(
            &active_history,
            self.state.session.tool_outputs(),
            &self.ui_config.tool_output_lines,
        );
        self.main_chat().load_messages(display_msgs);
        let cost = self.state.cost;
        let context_size = self.state.context_size;
        let main = self.main_chat();
        main.cost = cost;
        main.context_size = context_size;
        let draft = InputDraft {
            text: self
                .state
                .session
                .meta
                .input_draft
                .clone()
                .unwrap_or_default(),
            paste_ranges: self
                .state
                .session
                .meta
                .input_draft_pastes
                .iter()
                .map(|range| range.start..range.end)
                .collect(),
        };
        self.input_box.set_draft(draft);
        for image in &self.state.session.meta.input_draft_images {
            let Some(media_type) = maki_providers::ImageMediaType::from_mime(&image.media_type)
            else {
                tracing::warn!(media_type = %image.media_type, "skipping stored draft image");
                continue;
            };
            self.input_box
                .attach_image(ImageSource::new(media_type, Arc::from(image.data.as_str())));
        }
        self.input_box.move_to_end();
        self.unsent_subagent_steers = self
            .state
            .session
            .meta
            .unsent_subagent_messages
            .iter()
            .map(|(task_id, items)| {
                (
                    task_id.clone(),
                    items
                        .iter()
                        .map(|item| super::PendingSteer {
                            id: maki_agent::QueueItemId::new(),
                            text: item.text.clone(),
                            draft: InputDraft {
                                text: item.text.clone(),
                                paste_ranges: item
                                    .paste_ranges
                                    .iter()
                                    .map(|range| range.start..range.end)
                                    .collect(),
                            },
                        })
                        .collect(),
                )
            })
            .collect();

        self.fire_restore_items(restore_items);

        // Read, not taken: the live chats below are the source `sync_subagents`
        // mirrors back, so emptying the session here would only make the next
        // checkpoint write the same list again.
        for sa in self.state.session.subagents().to_vec() {
            // A subagent reaches disk when it spawns but its transcript only
            // when it ends, so one without an entry here never got to finish:
            // leftovers from a kill mid-turn. It has nothing to show, and
            // restoring it would park a task no agent backs at the top of the
            // picker, running forever. `sync_subagents` below drops it for good.
            let Some(messages) = self.state.session.subagent_messages().get(&sa.tool_use_id) else {
                continue;
            };
            let (display, items) = history_to_display(
                messages,
                self.state.session.tool_outputs(),
                &self.ui_config.tool_output_lines,
            );
            self.chat_index
                .insert(sa.tool_use_id.clone(), self.chats.len());
            let mut chat = Chat::subagent(
                &sa.tool_use_id,
                sa.name,
                self.ui_config.clone(),
                self.lua_event_handle.clone(),
            );
            chat.set_restore_channel(self.restore_event_tx.clone());
            chat.model_id = sa.model;
            chat.load_messages(display);
            // The session file keeps the transcript but never how it ended,
            // so a reload admits that instead of guessing.
            chat.mark_finished(TaskOutcome::Unknown, DONE_TEXT);
            self.fire_restore_items(items);
            self.chats.push(chat);
        }

        self.sync_subagents();

        let eh = &self.lua_event_handle;
        if eh.is_disconnected() {
            self.restoring
                .store(false, std::sync::atomic::Ordering::Relaxed);
        } else {
            eh.send_restore_complete(restoring);
        }
    }

    fn fire_restore_items(&self, items: Vec<maki_lua::RestoreItem>) {
        let Some(tx) = &self.restore_event_tx else {
            return;
        };
        let eh = &self.lua_event_handle;
        let theme_gen = crate::theme::generation();
        for mut item in items {
            item.theme_gen = Some(theme_gen);
            eh.request_restore(item, tx.clone());
        }
    }

    /// Shared by every restore path: `focus_session` picks between resuming in
    /// place and spawning a fresh runtime by tab state alone, so the same key
    /// press has to land on the same permissions. The stored value replaces
    /// whatever the previous session was running with, and a session that
    /// stored nothing falls back to `--yolo` / `always_yolo`.
    fn apply_stored_yolo(&self, meta: &SessionMeta) {
        self.permissions.set_session_yolo(meta.yolo);
    }

    /// Resume at process start: the agent was already spawned with this
    /// history, so no respawn follows and the restored queue must be
    /// flushed here.
    pub(crate) fn restore_resumed_session(&mut self) {
        self.permissions
            .load_session_rules(stored_to_rules(&self.state.session.meta.session_rules));
        self.apply_stored_yolo(&self.state.session.meta);
        self.restore_display();
        if !self.state.session.meta.queued_messages.is_empty()
            && let Err(error) = self.snapshot_history_head()
        {
            self.status_bar
                .flash(format!("Failed to snapshot workspace: {error}"));
            return;
        }
        self.flush_restored_queue();
        for w in self.state.warnings.drain(..) {
            self.status_bar.flash(w);
        }
    }

    /// The one funnel for handing a history over. When the UI installs one the
    /// agent did not give it (rewind, load, new session), the mirror handle
    /// goes away in the same breath, so no later checkpoint can bring the
    /// agent's stale copy back. Only `respawn_agent` hands a live mirror in.
    fn install_local_history(&mut self) -> LoadedSession {
        self.shared_history = None;
        let messages = match crate::active_session_history(&self.state.session) {
            Ok(history) => history,
            Err(error) => {
                self.status_bar
                    .flash(format!("Failed to read session history: {error}"));
                Vec::new()
            }
        };
        LoadedSession {
            messages,
            model_spec: self.state.session.model.clone(),
        }
    }

    pub(super) fn reset_session(&mut self) -> Vec<Action> {
        if self.cancelling_run.is_some() {
            self.status_bar.flash(REVERT_BUSY_MSG.into());
            return Vec::new();
        }
        self.checkpoint_now();
        let replacement = AppSession::new(&self.state.session.model, &self.state.session.cwd);
        let replacement_store = match Self::snapshot_store_for(
            &self.storage,
            replacement.id,
            std::path::Path::new(&replacement.cwd),
        ) {
            Ok(store) => store,
            Err(error) => {
                self.status_bar
                    .flash(format!("Failed to initialize workspace snapshots: {error}"));
                return Vec::new();
            }
        };
        self.reset_ui_chrome();
        self.state.token_usage = TokenUsage::default();
        self.state.cost = None;
        self.state.context_size = 0;
        self.state.goal.reset();
        self.goal_deferred = false;
        self.state.plan = PlanState::None;
        self.permissions.set_session_yolo(None);
        if self.state.mode == Mode::Plan {
            self.enter_plan();
        }
        // Fire before the swap. A handler cleaning up after the session
        // that just ended needs its id, and the stamp always reads
        // whichever session is current.
        self.fire_session_autocmd("SessionReset", serde_json::json!({}));
        self.state.session = Arc::new(replacement);
        self.snapshot_store = replacement_store;
        maki_otel::emit::session_started(
            maki_otel::emit::START_FRESH,
            Some(&self.state.session.id.to_string()),
        );
        self.install_local_history();
        vec![Action::NewSession]
    }

    pub(super) fn open_rewind_picker(&mut self) -> Vec<Action> {
        let history = match crate::active_session_history(&self.state.session) {
            Ok(history) => history,
            Err(error) => {
                self.status_bar
                    .flash(format!("Failed to read session history: {error}"));
                return vec![];
            }
        };
        match self.rewind_picker.open(&history) {
            Ok(()) => vec![],
            Err(msg) => {
                self.status_bar.flash(msg);
                vec![]
            }
        }
    }

    pub(crate) fn rewind_to(&mut self, entry: RewindEntry) -> Vec<Action> {
        let active_history = match crate::active_session_history(&self.state.session) {
            Ok(history) => history,
            Err(error) => {
                self.status_bar
                    .flash(format!("Failed to read session history: {error}"));
                return Vec::new();
            }
        };
        let Some(selected) = active_history.get(entry.turn_index) else {
            self.status_bar
                .flash("Selected turn is no longer available".into());
            return Vec::new();
        };
        self.revert_to(selected.id, RestoreMode::Conversation)
    }

    pub fn revert_to(&mut self, item_id: MakiId, mode: RestoreMode) -> Vec<Action> {
        self.revert_to_with_policy(item_id, mode, ConflictPolicy::Abort)
    }

    pub fn revert_at(&mut self, source: DisplaySource, mode: RestoreMode) -> Vec<Action> {
        self.revert_to(source_target_id(source), mode)
    }

    pub fn revert_to_with_policy(
        &mut self,
        item_id: MakiId,
        mode: RestoreMode,
        policy: ConflictPolicy,
    ) -> Vec<Action> {
        if self.status == Status::Streaming
            || self.awaiting_input()
            || self.cancelling_run.is_some()
            || self
                .state
                .session
                .meta
                .pending_revert
                .as_ref()
                .is_some_and(|pending| pending.restore_operation.is_some())
        {
            self.status_bar.flash(REVERT_BUSY_MSG.into());
            return Vec::new();
        }
        self.checkpoint_now();
        let conversation_source = crate::session_history_head(&self.state.session);
        let target = match resolve_revert_target(
            self.state.session.messages(),
            conversation_source,
            item_id,
        ) {
            Ok(target) => target,
            Err(error) => {
                self.status_bar.flash(error);
                return Vec::new();
            }
        };
        let previous = self.state.session.meta.pending_revert.clone();
        if previous.is_none()
            && mode.restores_files()
            && let Err(error) = self.discard_workspace_unrevert()
        {
            self.status_bar
                .flash(format!("Failed to begin workspace revert: {error}"));
            return Vec::new();
        }
        let workspace_source = previous.as_ref().map_or(conversation_source, |pending| {
            pending_workspace_head(pending)
        });
        let original_head = previous
            .as_ref()
            .map_or(conversation_source, |pending| pending.original_head);
        let original_workspace_head = previous
            .as_ref()
            .map(pending_original_workspace_head)
            .unwrap_or(workspace_source);
        let previous_file_status = previous
            .as_ref()
            .and_then(|pending| pending.file_status.clone());
        let file_status = if mode == RestoreMode::Conversation {
            previous_file_status.filter(file_restore_succeeded)
        } else {
            previous_file_status
        };
        let mut pending = PendingConversationRevert {
            original_head,
            target_head: target.head,
            original_workspace_head: Some(original_workspace_head.into()),
            workspace_head: Some(workspace_source.into()),
            file_status,
            restore_operation: None,
        };

        if !mode.restores_files() {
            let actual_head = if mode.restores_conversation() {
                target.head
            } else {
                conversation_source
            };
            self.state
                .session_mut()
                .set_conversation_state(actual_head, Some(pending));
            if mode.restores_conversation() {
                return self.finish_conversation_restore(conversation_source, target);
            }
            self.checkpoint_now();
            return Vec::new();
        }

        let source_chain = match checkpoint_chain(self.state.session.messages(), workspace_source) {
            Ok(chain) => chain,
            Err(error) => {
                self.status_bar.flash(error);
                return Vec::new();
            }
        };
        let target_chain = match checkpoint_chain(self.state.session.messages(), target.head) {
            Ok(chain) => chain,
            Err(error) => {
                self.status_bar.flash(error);
                return Vec::new();
            }
        };
        let operation_id = MakiId::generate();
        pending.restore_operation = Some(PendingRestoreOperation {
            id: operation_id,
            kind: PendingRestoreKind::Revert,
            phase: PendingRestorePhase::Intent,
            target_workspace_head: target.head.into(),
            conversation_target: mode.restores_conversation().then_some(target.head.into()),
            overwrite: policy == ConflictPolicy::Overwrite,
        });
        let before_intent = Arc::clone(&self.state.session);
        self.state
            .session_mut()
            .set_conversation_state(conversation_source, Some(pending));
        if let Err(error) = self.save_session_barrier() {
            self.state.session = before_intent;
            self.status_bar
                .flash(format!("Failed to save workspace restore intent: {error}"));
            return Vec::new();
        }

        let cwd = std::path::PathBuf::from(&self.state.session.cwd);
        let result = self.snapshot_store.restore_transaction_with_policy(
            &cwd,
            &source_chain,
            &target_chain,
            policy,
            operation_id,
        );
        let report = match result {
            Ok(report) => report,
            Err(error) => {
                let message = error.to_string();
                self.finish_failed_restore(operation_id, error, false);
                self.status_bar
                    .flash(format!("Failed to restore workspace: {message}"));
                return Vec::new();
            }
        };

        let intent_state = Arc::clone(&self.state.session);
        let Some(mut applied) = self.state.session.meta.pending_revert.clone() else {
            self.status_bar
                .flash("Workspace restore intent disappeared".into());
            return Vec::new();
        };
        applied.workspace_head = Some(target.head.into());
        applied.file_status = Some(restore_status_value(&Ok(report)));
        let Some(operation) = applied.restore_operation.as_mut() else {
            self.status_bar
                .flash("Workspace restore operation disappeared".into());
            return Vec::new();
        };
        operation.phase = PendingRestorePhase::FilesApplied;
        let conversation_target = if mode.restores_conversation() {
            target.head
        } else {
            conversation_source
        };
        self.state
            .session_mut()
            .set_conversation_state(conversation_target, Some(applied));
        if let Err(error) = self.save_session_barrier() {
            self.state.session = intent_state;
            self.status_bar
                .flash(format!("Failed to commit workspace restore: {error}"));
            return Vec::new();
        }
        if let Err(error) = self
            .snapshot_store
            .acknowledge_operation(&cwd, operation_id)
        {
            self.status_bar
                .flash(format!("Failed to finish workspace restore: {error}"));
            return Vec::new();
        }
        let applied_state = Arc::clone(&self.state.session);
        let Some(mut completed) = self.state.session.meta.pending_revert.clone() else {
            self.status_bar
                .flash("Applied workspace restore state disappeared".into());
            return Vec::new();
        };
        completed.restore_operation = None;
        self.state
            .session_mut()
            .set_conversation_state(conversation_target, Some(completed));
        if let Err(error) = self.save_session_barrier() {
            self.state.session = applied_state;
            self.status_bar
                .flash(format!("Failed to finalize workspace restore: {error}"));
            return Vec::new();
        }

        if !mode.restores_conversation() {
            return Vec::new();
        }
        self.finish_conversation_restore(conversation_source, target)
    }

    fn finish_conversation_restore(
        &mut self,
        previous_head: Option<MakiId>,
        target: RevertTarget,
    ) -> Vec<Action> {
        self.update_context_for_head(previous_head);
        self.reset_ui_chrome();
        self.restore_display();
        self.input_box.discard();
        if let Some((text, images)) = target.draft {
            self.input_box.set_input(text);
            for image in images {
                self.input_box.attach_image(image);
            }
            self.input_box.buffer.move_to_end();
        }

        let loaded = self.install_local_history();
        self.checkpoint_now();
        vec![Action::LoadSession(Box::new(loaded))]
    }

    pub fn unrevert(&mut self) -> Vec<Action> {
        if self.status == Status::Streaming
            || self.awaiting_input()
            || self.cancelling_run.is_some()
        {
            self.status_bar.flash(REVERT_BUSY_MSG.into());
            return Vec::new();
        }
        let Some(mut pending) = self.state.session.meta.pending_revert.clone() else {
            return Vec::new();
        };
        if pending.restore_operation.is_some() {
            self.status_bar.flash(REVERT_BUSY_MSG.into());
            return Vec::new();
        }

        if let Err(error) = checkpoint_chain(self.state.session.messages(), pending.original_head) {
            self.status_bar.flash(error);
            return Vec::new();
        }
        if let Some(file_status) = &pending.file_status {
            let Ok(status) = serde_json::from_value::<RestoreStatus>(file_status.clone()) else {
                self.status_bar.flash(
                    "Workspace revert status is invalid; conversation was not changed".into(),
                );
                return Vec::new();
            };
            if !status.worktree_is_reverted() {
                self.status_bar.flash(
                    "Workspace revert did not complete; conversation was not changed".into(),
                );
                return Vec::new();
            }
            self.checkpoint_now();
            let current_head = crate::session_history_head(&self.state.session);
            let operation_id = MakiId::generate();
            pending.restore_operation = Some(PendingRestoreOperation {
                id: operation_id,
                kind: PendingRestoreKind::Unrevert,
                phase: PendingRestorePhase::Intent,
                target_workspace_head: pending_original_workspace_head(&pending).into(),
                conversation_target: Some(pending.original_head.into()),
                overwrite: false,
            });
            let before_intent = Arc::clone(&self.state.session);
            self.state
                .session_mut()
                .set_conversation_state(current_head, Some(pending));
            if let Err(error) = self.save_session_barrier() {
                self.state.session = before_intent;
                self.status_bar
                    .flash(format!("Failed to save workspace restore intent: {error}"));
                return Vec::new();
            }
            let cwd = std::path::PathBuf::from(&self.state.session.cwd);
            let result = self.snapshot_store.unrevert_transaction_with_policy(
                &cwd,
                ConflictPolicy::Abort,
                operation_id,
            );
            if let Err(error) = result {
                let message = error.to_string();
                self.finish_failed_restore(operation_id, error, true);
                self.status_bar
                    .flash(format!("Failed to restore workspace: {message}"));
                return Vec::new();
            }

            let intent_state = Arc::clone(&self.state.session);
            let Some(mut applied) = self.state.session.meta.pending_revert.clone() else {
                self.status_bar
                    .flash("Workspace unrevert intent disappeared".into());
                return Vec::new();
            };
            applied.workspace_head = applied.original_workspace_head.clone();
            let Some(operation) = applied.restore_operation.as_mut() else {
                self.status_bar
                    .flash("Workspace unrevert operation disappeared".into());
                return Vec::new();
            };
            operation.phase = PendingRestorePhase::FilesApplied;
            self.state
                .session_mut()
                .set_conversation_state(applied.original_head, Some(applied));
            self.update_context_for_head(current_head);
            self.sync_live_meta();
            if let Err(error) = self.save_session_barrier() {
                self.state.session = intent_state;
                self.status_bar
                    .flash(format!("Failed to commit workspace restore: {error}"));
                return Vec::new();
            }
            if let Err(error) = self
                .snapshot_store
                .acknowledge_operation(&cwd, operation_id)
            {
                self.status_bar
                    .flash(format!("Failed to finish workspace restore: {error}"));
                return Vec::new();
            }
            let applied_state = Arc::clone(&self.state.session);
            let original_head = applied_state
                .meta
                .pending_revert
                .as_ref()
                .map(|pending| pending.original_head)
                .unwrap_or(current_head);
            self.state
                .session_mut()
                .set_conversation_state(original_head, None);
            if let Err(error) = self.save_session_barrier() {
                self.state.session = applied_state;
                self.status_bar
                    .flash(format!("Failed to finalize workspace restore: {error}"));
                return Vec::new();
            }
        } else {
            let current_head = crate::session_history_head(&self.state.session);
            self.state
                .session_mut()
                .set_conversation_state(pending.original_head, None);
            self.update_context_for_head(current_head);
        }

        self.reset_ui_chrome();
        self.restore_display();
        self.input_box.discard();

        let loaded = self.install_local_history();
        self.checkpoint_now();
        vec![Action::LoadSession(Box::new(loaded))]
    }

    fn sync_live_meta(&mut self) {
        let meta = self.build_meta();
        AppSession::checkpoint(&mut self.state.session, None, meta, self.state.token_usage);
    }

    fn save_session_barrier(&mut self) -> Result<(), String> {
        self.storage_writer
            .save_sync(Arc::clone(&self.state.session))
            .map_err(|error| error.to_string())?;
        let session = &self.state.session;
        self.last_sent = Some(Sent {
            id: session.id,
            revision: session.revision(),
            content_revision: session.content_revision(),
            at: Instant::now(),
        });
        Ok(())
    }

    fn finish_failed_restore(
        &mut self,
        operation_id: MakiId,
        error: SnapshotError,
        unrevert: bool,
    ) {
        match self.snapshot_store.journal_operation_id() {
            Ok(Some(journal_id)) if journal_id == operation_id => return,
            Ok(_) => {}
            Err(journal_error) => {
                self.status_bar.flash(format!(
                    "Failed to inspect workspace restore journal: {journal_error}"
                ));
                return;
            }
        }
        let intent_state = Arc::clone(&self.state.session);
        let Some(mut pending) = self.state.session.meta.pending_revert.clone() else {
            return;
        };
        pending.restore_operation = None;
        let result = Err(error);
        pending.file_status = Some(if unrevert {
            unrevert_failure_status_value(&result)
        } else {
            restore_status_value(&result)
        });
        let current_head = crate::session_history_head(&self.state.session);
        self.state
            .session_mut()
            .set_conversation_state(current_head, Some(pending));
        if let Err(save_error) = self.save_session_barrier() {
            self.state.session = intent_state;
            self.status_bar.flash(format!(
                "Failed to save workspace restore failure: {save_error}"
            ));
        }
    }

    pub fn fork_at(&self, source: DisplaySource) -> Result<ForkedSession, String> {
        if self.cancelling_run.is_some()
            || self
                .state
                .session
                .meta
                .pending_revert
                .as_ref()
                .is_some_and(|pending| pending.restore_operation.is_some())
        {
            return Err(REVERT_BUSY_MSG.into());
        }
        let current_head = crate::session_history_head(&self.state.session);
        let target = resolve_revert_target(
            self.state.session.messages(),
            current_head,
            source_target_id(source),
        )?;
        let ancestor = active_history_items(self.state.session.messages(), target.head)
            .map_err(|error| format!("Failed to read session history: {error}"))?;
        let mut child = AppSession::new(&self.state.session.model, &self.state.session.cwd);
        child.meta = SessionMeta {
            mode: Some(self.state.mode.into()),
            plan_path: self
                .state
                .plan
                .path()
                .map(|path| path.to_string_lossy().into_owned()),
            plan_written: self.state.plan.is_ready(),
            session_rules: rules_to_stored(&self.permissions.session_rules_snapshot()),
            thinking: Some(self.state.thinking.into()),
            fast: self.state.fast,
            workflow: self.state.workflow,
            yolo: self.permissions.persisted_yolo(),
            ..SessionMeta::default()
        };
        child.replace_messages(ancestor.clone());
        child.set_title(self.next_fork_title()?);

        let mut reachable = tool_call_ids(&ancestor);
        let mut copied_subagents = HashSet::new();
        while let Some(task_id) = reachable
            .iter()
            .find(|task_id| {
                !copied_subagents.contains(*task_id)
                    && self
                        .state
                        .session
                        .subagent_messages()
                        .contains_key(*task_id)
            })
            .cloned()
        {
            let history = self.state.session.subagent_messages()[&task_id].as_ref();
            reachable.extend(tool_call_ids(history));
            child.set_subagent_messages(task_id.clone(), history.to_vec());
            copied_subagents.insert(task_id);
        }
        for tool_id in &reachable {
            if let Some(output) = self.state.session.tool_outputs().get(tool_id) {
                child.insert_tool_output(tool_id.clone(), output.as_ref().clone());
            }
        }
        child.set_subagents(
            self.state
                .session
                .subagents()
                .iter()
                .filter(|subagent| reachable.contains(&subagent.tool_use_id))
                .cloned()
                .collect(),
        );

        let child_snapshots =
            Self::snapshot_store_for(&self.storage, child.id, std::path::Path::new(&child.cwd))
                .map_err(|error| format!("Failed to initialize workspace snapshots: {error}"))?;
        self.snapshot_store
            .copy_ancestry_to(
                &child_snapshots,
                &ancestor.iter().map(|item| item.id).collect::<Vec<_>>(),
            )
            .map_err(|error| format!("Failed to copy workspace snapshots: {error}"))?;

        Ok(ForkedSession {
            session: child,
            draft: target
                .draft
                .map(|(text, images)| ForkDraft { text, images }),
        })
    }

    fn next_fork_title(&self) -> Result<String, String> {
        let (base, own_number) = split_fork_title(&self.state.session.title);
        let sessions = AppSession::list(&self.state.session.cwd, &self.storage)
            .map_err(|error| format!("Failed to number fork: {error}"))?;
        let number = sessions
            .iter()
            .filter_map(|session| {
                let (candidate, number) = split_fork_title(&session.title);
                (candidate == base).then_some(number).flatten()
            })
            .chain(own_number)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        Ok(format!("{base} (fork #{number})"))
    }

    fn update_context_for_head(&mut self, previous_head: Option<MakiId>) {
        let previous = history_tokens(self.state.session.messages(), previous_head);
        let baseline = self.state.context_size.saturating_sub(previous);
        let current = crate::active_session_history(&self.state.session)
            .and_then(|history| project_messages(&history))
            .map(|messages| estimate_message_tokens(&messages))
            .unwrap_or_default();
        self.state.context_size = if current == 0 { 0 } else { baseline + current };
    }

    pub(crate) fn apply_loaded_session(
        &mut self,
        mut session: AppSession,
        fallback_model: &Model,
    ) -> Result<LoadedSession, String> {
        let snapshot_store = Self::snapshot_store_for(
            &self.storage,
            session.id,
            std::path::Path::new(&session.cwd),
        )
        .map_err(|error| format!("Failed to initialize workspace snapshots: {error}"))?;
        recover_pending_workspace_restore(&mut session, &snapshot_store, &self.storage_writer)?;
        self.checkpoint_now();
        self.permissions
            .load_session_rules(stored_to_rules(&session.meta.session_rules));
        self.apply_stored_yolo(&session.meta);
        self.state =
            SessionState::from_session(session, fallback_model, &self.storage, &self.model_policy);
        self.snapshot_store = snapshot_store;
        for w in self.state.warnings.drain(..) {
            self.status_bar.flash(w);
        }
        self.reset_ui_chrome();
        self.restore_display();

        Ok(self.install_local_history())
    }

    #[cfg(test)]
    pub(crate) fn load_session(&mut self, session_id: MakiId) -> Vec<Action> {
        let session = match crate::load_app_session(session_id, &self.storage) {
            Ok(s) => s,
            Err(e) => {
                self.status_bar
                    .flash(format!("Failed to load session: {e}"));
                return vec![];
            }
        };
        match self.apply_loaded_session(session, &self.state.model.clone()) {
            Ok(loaded) => vec![Action::LoadSession(Box::new(loaded))],
            Err(error) => {
                self.status_bar.flash(error);
                Vec::new()
            }
        }
    }
}

pub(crate) fn recover_pending_workspace_restore(
    session: &mut AppSession,
    snapshot_store: &SnapshotStore,
    storage_writer: &StorageWriter,
) -> Result<(), String> {
    let cwd = std::path::PathBuf::from(&session.cwd);
    let Some(mut pending) = session.meta.pending_revert.clone() else {
        if snapshot_store
            .journal_state()
            .map_err(|error| format!("Failed to inspect workspace restore journal: {error}"))?
            .is_none()
        {
            return Ok(());
        }
        if let Some(report) = snapshot_store
            .recover(&cwd)
            .map_err(|error| format!("Failed to recover workspace restore: {error}"))?
            && let Some(operation_id) = report.operation_id
        {
            return Err(format!(
                "Workspace restore journal {operation_id} has no matching session operation"
            ));
        }
        return Ok(());
    };
    let Some(operation) = pending.restore_operation.clone() else {
        if snapshot_store
            .journal_state()
            .map_err(|error| format!("Failed to inspect workspace restore journal: {error}"))?
            .is_none()
        {
            return Ok(());
        }
        if let Some(report) = snapshot_store
            .recover(&cwd)
            .map_err(|error| format!("Failed to recover workspace restore: {error}"))?
            && let Some(operation_id) = report.operation_id
        {
            return Err(format!(
                "Workspace restore journal {operation_id} has no matching session operation"
            ));
        }
        return Ok(());
    };

    if operation.phase == PendingRestorePhase::Intent {
        let recovered = snapshot_store
            .recover(&cwd)
            .map_err(|error| format!("Failed to recover workspace restore: {error}"))?;
        let report = if let Some(report) = recovered {
            validate_restore_report(&operation, &report)?;
            report
        } else {
            let policy = if operation.overwrite {
                ConflictPolicy::Overwrite
            } else {
                ConflictPolicy::Abort
            };
            let result = match operation.kind {
                PendingRestoreKind::Revert => {
                    let source_chain =
                        checkpoint_chain(session.messages(), pending_workspace_head(&pending))?;
                    let target_chain =
                        checkpoint_chain(session.messages(), operation.target_workspace_head.head)?;
                    snapshot_store.restore_transaction_with_policy(
                        &cwd,
                        &source_chain,
                        &target_chain,
                        policy,
                        operation.id,
                    )
                }
                PendingRestoreKind::Unrevert => {
                    snapshot_store.unrevert_transaction_with_policy(&cwd, policy, operation.id)
                }
            };
            result.map_err(|error| format!("Failed to recover workspace restore: {error}"))?
        };
        validate_restore_report(&operation, &report)?;
        pending.workspace_head = Some(operation.target_workspace_head.clone());
        if operation.kind == PendingRestoreKind::Revert {
            pending.file_status = Some(restore_status_value(&Ok(report)));
        }
        let Some(applied_operation) = pending.restore_operation.as_mut() else {
            return Err("Workspace restore operation disappeared during recovery".into());
        };
        applied_operation.phase = PendingRestorePhase::FilesApplied;
        let conversation_head = operation
            .conversation_target
            .as_ref()
            .map_or_else(|| crate::session_history_head(session), |head| head.head);
        session.set_conversation_state(conversation_head, Some(pending));
        storage_writer
            .save_sync(Arc::new(session.clone()))
            .map_err(|error| format!("Failed to commit recovered workspace restore: {error}"))?;
    } else if let Some(report) = snapshot_store
        .recover(&cwd)
        .map_err(|error| format!("Failed to recover workspace restore: {error}"))?
    {
        validate_restore_report(&operation, &report)?;
    }

    snapshot_store
        .acknowledge_operation(&cwd, operation.id)
        .map_err(|error| format!("Failed to acknowledge recovered workspace restore: {error}"))?;
    if operation.kind == PendingRestoreKind::Unrevert {
        let conversation_head = operation
            .conversation_target
            .as_ref()
            .map_or(session.meta.history_head, |head| head.head);
        session.set_conversation_state(conversation_head, None);
    } else {
        let Some(mut completed) = session.meta.pending_revert.clone() else {
            return Err("Recovered workspace restore state disappeared".into());
        };
        completed.restore_operation = None;
        let current_head = crate::session_history_head(session);
        session.set_conversation_state(current_head, Some(completed));
    }
    storage_writer
        .save_sync(Arc::new(session.clone()))
        .map_err(|error| format!("Failed to finalize recovered workspace restore: {error}"))?;
    Ok(())
}

fn validate_restore_report(
    operation: &PendingRestoreOperation,
    report: &RestoreReport,
) -> Result<(), String> {
    if report.operation_id != Some(operation.id) {
        return Err(format!(
            "Workspace restore journal belongs to {:?}, expected {}",
            report.operation_id, operation.id
        ));
    }
    let target_matches = match operation.kind {
        PendingRestoreKind::Revert => matches!(report.target, RestoreTarget::Snapshot(_)),
        PendingRestoreKind::Unrevert => report.target == RestoreTarget::Unrevert,
    };
    if !target_matches {
        return Err(format!(
            "Workspace restore journal has the wrong target for operation {}",
            operation.id
        ));
    }
    Ok(())
}

fn source_target_id(source: DisplaySource) -> MakiId {
    match source {
        DisplaySource::User(id)
        | DisplaySource::AssistantText(id)
        | DisplaySource::Reasoning(id)
        | DisplaySource::ToolResult(id) => id,
        DisplaySource::ToolCall { id, result_id } => result_id.unwrap_or(id),
    }
}

fn tool_call_ids(items: &[HistoryItem]) -> HashSet<String> {
    items
        .iter()
        .filter_map(|item| match &item.kind {
            HistoryItemKind::ToolCall { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect()
}

fn split_fork_title(title: &str) -> (&str, Option<u32>) {
    let Some((base, suffix)) = title.rsplit_once(" (fork #") else {
        return (title, None);
    };
    let Some(number) = suffix
        .strip_suffix(')')
        .and_then(|number| number.parse().ok())
    else {
        return (title, None);
    };
    (base, Some(number))
}

fn resolve_revert_target(
    items: &[HistoryItem],
    current_head: Option<MakiId>,
    item_id: MakiId,
) -> Result<RevertTarget, String> {
    let selected = items
        .iter()
        .find(|item| item.id == item_id)
        .ok_or_else(|| format!("History item {item_id} is no longer available"))?;

    match &selected.kind {
        HistoryItemKind::User { .. } => {
            let group: Vec<_> = items
                .iter()
                .filter(|item| item.group_id == selected.group_id)
                .collect();
            let first = group
                .first()
                .ok_or_else(|| format!("History group {} is empty", selected.group_id))?;
            let display_text = group.iter().find_map(|item| match &item.kind {
                HistoryItemKind::User {
                    display_text: Some(text),
                    ..
                } if !text.is_empty() => Some(text.clone()),
                _ => None,
            });
            let text = display_text.unwrap_or_else(|| {
                group
                    .iter()
                    .filter_map(|item| match &item.kind {
                        HistoryItemKind::User { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect()
            });
            let images = group
                .iter()
                .filter_map(|item| match &item.kind {
                    HistoryItemKind::User { images, .. } => Some(images.iter().cloned()),
                    _ => None,
                })
                .flatten()
                .collect();
            Ok(RevertTarget {
                head: first.parent_id,
                draft: Some((text, images)),
            })
        }
        HistoryItemKind::ToolCall { call_id, .. } => {
            let active = active_history_items(items, current_head)
                .map_err(|error| format!("Failed to read session history: {error}"))?;
            let result = active
                .iter()
                .position(|item| item.id == item_id)
                .and_then(|position| {
                    active[position + 1..].iter().find(|item| {
                        matches!(
                            &item.kind,
                            HistoryItemKind::ToolResult {
                                call_id: result_call_id,
                                ..
                            } if result_call_id == call_id
                        )
                    })
                });
            Ok(RevertTarget {
                head: Some(match result {
                    Some(result) => complete_tool_result_group_head(items, result.id)?,
                    None => complete_group_head(items, selected.id)?,
                }),
                draft: None,
            })
        }
        HistoryItemKind::AssistantText { .. } | HistoryItemKind::Reasoning { .. } => {
            Ok(RevertTarget {
                head: Some(selected.id),
                draft: None,
            })
        }
        HistoryItemKind::ToolResult { .. } => Ok(RevertTarget {
            head: Some(complete_tool_result_group_head(items, selected.id)?),
            draft: None,
        }),
    }
}

fn complete_tool_result_group_head(
    items: &[HistoryItem],
    result_id: MakiId,
) -> Result<MakiId, String> {
    let head_id = complete_group_head(items, result_id)?;
    let path = active_history_items(items, Some(head_id))
        .map_err(|error| format!("Failed to read session history: {error}"))?;
    let mut unresolved = HashSet::new();
    for item in &path {
        match &item.kind {
            HistoryItemKind::ToolCall { call_id, .. } => {
                unresolved.insert(call_id.as_str());
            }
            HistoryItemKind::ToolResult { call_id, .. } => {
                unresolved.remove(call_id.as_str());
            }
            _ => {}
        }
    }
    if !unresolved.is_empty() {
        return Err(format!(
            "Tool result group is incomplete ({} unresolved tool calls)",
            unresolved.len()
        ));
    }
    Ok(head_id)
}

fn complete_group_head(items: &[HistoryItem], item_id: MakiId) -> Result<MakiId, String> {
    let item = items
        .iter()
        .find(|item| item.id == item_id)
        .ok_or_else(|| format!("History item {item_id} is no longer available"))?;
    let mut head = item;
    while let Some(next) = items.iter().find(|candidate| {
        candidate.group_id == item.group_id && candidate.parent_id == Some(head.id)
    }) {
        head = next;
    }
    Ok(head.id)
}

fn checkpoint_chain(items: &[HistoryItem], head: Option<MakiId>) -> Result<Vec<MakiId>, String> {
    let mut chain = active_history_items(items, head)
        .map_err(|error| format!("Failed to read session history: {error}"))?
        .into_iter()
        .map(|item| item.id)
        .collect::<Vec<_>>();
    chain.reverse();
    Ok(chain)
}

fn is_sanitizer_only_unavailable_extension(
    original: &[HistoryItem],
    candidate: &[HistoryItem],
) -> bool {
    if original.is_empty() || candidate.len() <= original.len() || !candidate.starts_with(original)
    {
        return false;
    }

    let call_group_id = original.last().unwrap().group_id;
    let mut call_ids: HashSet<_> = original
        .iter()
        .rev()
        .take_while(|item| item.group_id == call_group_id)
        .filter_map(|item| match &item.kind {
            HistoryItemKind::ToolCall { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    if call_ids.is_empty() {
        return false;
    }

    let additions = &candidate[original.len()..];
    let result_group_id = additions[0].group_id;
    additions.iter().all(|item| {
        item.group_id == result_group_id
            && matches!(
                &item.kind,
                HistoryItemKind::ToolResult {
                    call_id,
                    content,
                    is_error: true,
                    images,
                } if content == maki_agent::UNAVAILABLE_RESULT
                    && images.is_empty()
                    && call_ids.remove(call_id.as_str())
            )
    }) && call_ids.is_empty()
}

fn pending_workspace_head(pending: &PendingConversationRevert) -> Option<MakiId> {
    pending
        .workspace_head
        .as_ref()
        .map(|head| head.head)
        .unwrap_or_else(|| {
            if pending
                .file_status
                .as_ref()
                .is_some_and(file_restore_succeeded)
            {
                pending.target_head
            } else {
                pending.original_head
            }
        })
}

fn pending_original_workspace_head(pending: &PendingConversationRevert) -> Option<MakiId> {
    pending
        .original_workspace_head
        .as_ref()
        .map(|head| head.head)
        .unwrap_or(pending.original_head)
}

fn history_tokens(items: &[HistoryItem], head: Option<MakiId>) -> u32 {
    let Ok(history) = active_history_items(items, head) else {
        return 0;
    };
    let messages = project_messages(&history)
        .or_else(|_| maki_agent::History::restored(history).map(maki_agent::History::into_vec));
    messages
        .map(|messages| estimate_message_tokens(&messages))
        .unwrap_or_default()
}

fn restore_status_value(result: &Result<RestoreReport, SnapshotError>) -> serde_json::Value {
    let status = RestoreStatus::from_result(result);
    serde_json::to_value(&status).unwrap_or_else(|error| {
        tracing::error!(%error, "failed to serialize workspace restore status");
        serde_json::json!({
            "status": "failed",
            "kind": "other",
            "message": error.to_string(),
        })
    })
}

fn file_restore_succeeded(status: &serde_json::Value) -> bool {
    serde_json::from_value::<RestoreStatus>(status.clone())
        .is_ok_and(|status| status.worktree_is_reverted())
}

fn unrevert_failure_status_value(
    result: &Result<RestoreReport, SnapshotError>,
) -> serde_json::Value {
    let mut status = RestoreStatus::from_result(result);
    status.mark_worktree_reverted();
    serde_json::to_value(&status).unwrap_or_else(|error| {
        tracing::error!(%error, "failed to serialize workspace unrevert status");
        serde_json::json!({
            "status": "failed",
            "kind": "other",
            "message": error.to_string(),
            "worktree_reverted": true,
        })
    })
}
