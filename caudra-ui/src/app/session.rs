use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use crate::app::tasks::TaskOutcome;
use crate::chat::{CANCELLED_TEXT, Chat, DONE_TEXT, ERROR_TEXT, history_to_display};
use crate::components::rewind_picker::RewindEntry;
use crate::components::session_picker::{SessionPickerAction, SessionRow};
use crate::components::session_relocation::SessionRelocationAction;
use crate::components::{Action, DisplaySource, ForkDraft, ForkedSession, LoadedSession};
use crate::input_document::InputDraft;
use crate::repaint::{Dirty, Watch};
use caudra_agent::HistorySnapshot;
use caudra_agent::agent::estimate_message_tokens;
use caudra_agent::snapshots::{
    ConflictPolicy, RestoreReport, RestoreStatus, RestoreTarget, SnapshotError, SnapshotStore,
};
use caudra_agent::workspace_baseline::WorkspaceBaseline;
use caudra_agent::{GoalHandle, GoalStatus};
use caudra_providers::{
    HistoryItem, HistoryItemKind, ImageSource, Model, TokenUsage, active_history_items,
    merge_history_items, project_messages,
};
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::{
    PendingConversationRevert, PendingRestoreKind, PendingRestoreOperation, PendingRestorePhase,
    SessionDatabase, SessionLease, SessionLocation, SessionMeta, StoredActiveGoal,
    StoredGoalResult, StoredImage, StoredPasteRange, StoredPlanTarget, StoredPromptAdmission,
    StoredQueuedDraft, StoredQueuedPrompt, StoredSubagent, StoredSubagentOutcome,
};
use caudra_storage::tool_outputs::{ToolOutputId, ToolOutputRef, ToolOutputStore};
use ratatui::layout::Rect;

use crate::AppSession;
use crate::storage_writer::StorageWriter;

use super::session_state::SessionState;
use super::{App, Mode, PendingInput, PlanState, RestoreMode, Status};

/// The shortest gap between two writes that carry only UI state.
const SOFT_SAVE_DELAY: Duration = Duration::from_millis(1000);
const RENAME_USAGE: &str = "Usage: /rename <title>";
pub(crate) const REVERT_BUSY_MSG: &str = "Wait for the session to become idle before reverting";

/// Saturates rather than wraps: a goal left open for longer than `u64`
/// milliseconds is not a number worth panicking over.
fn as_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn plan_target(plan: &PlanState) -> Option<StoredPlanTarget> {
    plan.path().map_or_else(
        || {
            plan.reference()
                .cloned()
                .map(|reference| StoredPlanTarget::PlanRef { reference })
        },
        |path| {
            Some(StoredPlanTarget::LocalPath {
                path: path.to_string_lossy().into_owned(),
            })
        },
    )
}

fn stored_subagent_outcome(outcome: Option<TaskOutcome>) -> StoredSubagentOutcome {
    match outcome {
        None | Some(TaskOutcome::Unknown) => StoredSubagentOutcome::Unknown,
        Some(TaskOutcome::Done) => StoredSubagentOutcome::Done,
        Some(TaskOutcome::Killed) => StoredSubagentOutcome::Killed,
        Some(TaskOutcome::Error) => StoredSubagentOutcome::Error,
    }
}

fn restored_subagent_outcome(outcome: StoredSubagentOutcome) -> (TaskOutcome, &'static str) {
    match outcome {
        StoredSubagentOutcome::Unknown => (TaskOutcome::Unknown, DONE_TEXT),
        StoredSubagentOutcome::Done => (TaskOutcome::Done, DONE_TEXT),
        StoredSubagentOutcome::Killed => (TaskOutcome::Killed, CANCELLED_TEXT),
        StoredSubagentOutcome::Error => (TaskOutcome::Error, ERROR_TEXT),
    }
}

#[derive(Clone)]
struct RevertTarget {
    head: Option<CaudraId>,
    draft: Option<(String, Vec<ImageSource>)>,
}

pub(super) struct RemoteRestoreConfirmation {
    conversation_source: Option<CaudraId>,
    target: RevertTarget,
    pending: PendingConversationRevert,
    mode: RestoreMode,
    pub(super) prepared: caudra_workspace::PreparedSnapshotOperation,
}

/// What `App::checkpoint` last handed to the writer: which session, how far
/// along it was, and when. The id is part of it because a session swapped into
/// the tab starts its revisions back at zero and would otherwise look older
/// than the stamp left by the one it replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Sent {
    pub id: CaudraId,
    pub revision: u64,
    pub content_revision: u64,
    pub at: Instant,
}

/// The producer snapshot `App::checkpoint` last merged, and the session
/// revision the merge left behind.
///
/// The merge walks the whole history graph, and the event loop checkpoints
/// every frame, so an idle session would pay that walk ten times a second
/// forever. `ArcSwap` publishes a fresh `Arc` per mutation, which makes
/// pointer identity an exact "the producer added nothing" test, and holding
/// the `Arc` is what stops a later snapshot from landing on the same address.
/// The revision covers the other side: a rewind or a compaction moves the
/// session's own messages without the producer publishing anything.
pub(super) struct MergedHistory {
    snapshot: Arc<HistorySnapshot>,
    content_revision: u64,
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
        || !session.meta.structured_permission_rules.is_empty()
        || session
            .meta
            .system_prompt_profile
            .as_deref()
            .is_some_and(|profile| profile != caudra_agent::prompt::profile::BUILTIN_PROFILE_NAME)
        // Plan is the mode a session opens in, so only leaving it is a choice
        // worth keeping an otherwise empty session for.
        || session.meta.mode == Some(caudra_storage::sessions::StoredMode::Build)
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
        if let Some(snapshot) = snapshot.filter(|snapshot| self.history_moved(snapshot)) {
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
            if commits_file_revert {
                let committed = if self.workspace_baseline.is_remote() {
                    self.workspace_baseline
                        .pending_remote_restore()
                        .map_err(|error| error.to_string())
                        .and_then(|restore| {
                            restore.map_or(Ok(()), |restore| {
                                smol::block_on(
                                    self.workspace_baseline
                                        .acknowledge_remote_restore(&restore.restore_id),
                                )
                                .map(drop)
                                .map_err(|error| error.to_string())
                            })
                        })
                } else {
                    self.discard_workspace_unrevert()
                        .map_err(|error| error.to_string())
                };
                if let Err(error) = committed {
                    self.status_bar
                        .flash(format!("Failed to commit reverted workspace: {error}"));
                    return;
                }
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
                        session.merge_history(&snapshot, merged);
                        session.update_title_if_default();
                    }
                    if added {
                        let (messages, _) = history_to_display(
                            &snapshot.messages,
                            self.state.session.tool_outputs(),
                            &self.ui_config.tool_output_lines,
                            self.ui_config.show_reminders,
                        );
                        self.main_chat().bind_sources(&messages);
                    }
                    // Only a merge that reached a consistent graph may be
                    // remembered. A failed one has to be retried, and its
                    // flash re-raised, on the next frame.
                    self.merged_history = Some(MergedHistory {
                        snapshot,
                        content_revision: self.state.session.content_revision(),
                    });
                }
                Err(error) => {
                    tracing::error!(%error, "refusing to checkpoint invalid history graph");
                    self.status_bar
                        .flash(format!("Failed to save session history: {error}"));
                }
            }
        }
        AppSession::checkpoint(&mut self.state.session, None, meta, self.state.token_usage);
        self.workspace_baseline
            .set_current_head(self.history_head());

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

    /// Drops the merge memo so the next checkpoint walks the graph again.
    /// Called wherever a different history is installed, which is the one
    /// thing pointer identity cannot notice on its own.
    pub(crate) fn forget_merged_history(&mut self) {
        self.merged_history = None;
    }

    /// Whether either side of the history merge moved since the last one.
    ///
    /// Both comparisons are `u64`-cheap, which is the point: the merge they
    /// guard allocates a set of every message id, deep-clones the whole
    /// message vector and deep-compares it, and an idle session runs this ten
    /// times a second.
    pub(super) fn history_moved(&self, snapshot: &Arc<HistorySnapshot>) -> bool {
        self.merged_history.as_ref().is_none_or(|merged| {
            !Arc::ptr_eq(&merged.snapshot, snapshot)
                || merged.content_revision != self.state.session.content_revision()
        })
    }

    /// Everything the session mirrors from live state, built field by field so
    /// a new `SessionMeta` field forces a decision here. Every frame calls it,
    /// so it stays cheap: an idle UI has an empty draft, queue and rule list,
    /// and an empty `Vec` does not allocate.
    fn build_meta(&self) -> SessionMeta {
        let state = &self.state;
        let draft = self.input_box.draft();
        let queued_prompts = if self.recoverable_queue.is_empty() {
            self.queue.pending_prompts()
        } else {
            self.recoverable_queue.clone()
        };
        SessionMeta {
            system_prompt_profile: if state.system_prompt_profile_override {
                state.session.meta.system_prompt_profile.clone()
            } else {
                Some(state.system_prompt_profile_name.clone())
            },
            history_head: state.session.meta.history_head,
            pending_revert: state.session.meta.pending_revert.clone(),
            mode: Some(state.mode.into()),
            plan_path: state.plan.path().map(|p| p.to_string_lossy().into_owned()),
            plan_target: plan_target(&state.plan),
            plan_written: state.plan.is_ready(),
            structured_permission_rules: self.permissions.structured_conversation_rules_snapshot(),
            context_size: state.context_size,
            turns: state.turns,
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
            queued_messages: queued_prompts
                .iter()
                .map(|prompt| StoredQueuedPrompt {
                    text: prompt.text.clone(),
                    images: prompt
                        .images
                        .iter()
                        .map(|image| StoredImage {
                            media_type: image.media_type.mime().into(),
                            data: image.data.to_string(),
                        })
                        .collect(),
                    paste_ranges: prompt
                        .paste_ranges
                        .iter()
                        .map(|range| StoredPasteRange {
                            start: range.start,
                            end: range.end,
                        })
                        .collect(),
                })
                .collect(),
            queued_message_admissions: queued_prompts
                .iter()
                .map(|prompt| match prompt.admission {
                    caudra_agent::PromptAdmission::Queue => StoredPromptAdmission::Queue,
                    caudra_agent::PromptAdmission::Steer => StoredPromptAdmission::Steer,
                    caudra_agent::PromptAdmission::Interrupt => StoredPromptAdmission::Interrupt,
                })
                .collect(),
            queued_messages_together: if self.recoverable_queue.is_empty() {
                self.queue.delivery() == caudra_agent::QueueDelivery::TogetherNextTurn
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
            thinking: Some(state.thinking.clone().into()),
            fast: state.fast,
            active_goal: state.goal.snapshot().map(|goal| {
                Box::new(StoredActiveGoal {
                    condition: goal.condition.to_string(),
                    evaluations: goal.evaluations,
                    elapsed_ms: as_millis(goal.elapsed()),
                    usage: goal.usage.spent(goal.cost, goal.subscription_cost),
                    last_verdict: goal.last_verdict.map(Into::into),
                    last_reason: goal.last_reason.map(|reason| reason.to_string()),
                })
            }),
            goal_result: match state.goal.status() {
                Some(GoalStatus::Finished(goal)) => Some(Box::new(StoredGoalResult {
                    condition: goal.condition.to_string(),
                    verdict: goal.verdict.into(),
                    reason: goal.reason.to_string(),
                    evaluations: goal.evaluations,
                    duration_ms: as_millis(goal.duration),
                    usage: goal.usage.spent(goal.cost, goal.subscription_cost),
                })),
                Some(GoalStatus::Active(_)) | None => None,
            },
            goal_continuation_limit: Some(state.goal.continuation_limit()),
            yolo: self.permissions.persisted_yolo(),
            snapshots_unavailable: self.snapshots_unavailable.clone(),
        }
    }

    pub(super) fn sync_subagents(&mut self) {
        let histories = self.state.session.subagent_messages();
        let roots: HashMap<&str, Option<String>> = self
            .state
            .session
            .subagents()
            .iter()
            .map(|subagent| {
                (
                    subagent.tool_use_id.as_str(),
                    subagent.root_tool_use_id.clone(),
                )
            })
            .collect();
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
                    parent_tool_use_id: chat.parent_tool_use_id().map(ToString::to_string),
                    root_tool_use_id: roots.get(task_id.as_ref()).cloned().flatten(),
                    name: chat.name.clone(),
                    model: chat.model_id.clone(),
                    outcome: stored_subagent_outcome(chat.task_outcome()),
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
        self.review.discard();
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
        self.queue_hover = None;
        self.todo_panel.reset();
        self.admission_hits.clear();
        self.admission_mouse_down = None;
        self.admission_hover = None;
        self.task_hint_hit = Rect::ZERO;
        self.task_hint_mouse_down = false;
        self.task_hint_hover = false;
        self.key_focus = super::KeyFocus::Composer;
        self.recoverable_queue.clear();
        self.recoverable_queue_together = false;
        self.close_all_overlays();
        self.pending_input = PendingInput::None;
        self.cancelling_run = None;
        self.replacement_item = None;
        self.status_bar.clear_flash();
        self.last_esc = None;
        self.last_exit = None;
        self.restoring = Arc::new(AtomicBool::new(false));
        self.plan_form.reset();
    }

    pub(crate) fn restore_display(&mut self) {
        let restoring = Arc::new(AtomicBool::new(true));
        self.restoring = restoring.clone();

        // The transcript, not the request: a compaction leaves the turns it
        // summarized in the store, and this is the reader's only way back to
        // them. The subagent and tool-output reachability below reads the same
        // path on purpose, so a card drawn above the border keeps its output.
        let active_history = match crate::transcript_session_history(&self.state.session) {
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
            self.ui_config.show_reminders,
        );
        let mut reachable_subagents = reachable_subagent_ids(
            &active_history,
            self.state.session.subagent_messages(),
            self.state.session.tool_outputs(),
            self.state.session.subagents(),
        );
        let legacy_fallback = reachable_subagents.is_empty();
        let mut active_calls = caudra_agent::history_tool_call_ids(&active_history);
        for task_id in &reachable_subagents {
            if let Some(history) = self.state.session.subagent_messages().get(task_id) {
                active_calls.extend(caudra_agent::history_tool_call_ids(history));
            }
        }
        reachable_subagents.extend(
            self.state
                .session
                .subagent_task_specs()
                .iter()
                .filter(|(task_id, spec)| {
                    if !spec.is_generic() {
                        return false;
                    }
                    let descriptor = self
                        .state
                        .session
                        .subagents()
                        .iter()
                        .find(|subagent| subagent.tool_use_id == **task_id);
                    descriptor.is_none()
                        || active_calls.contains(*task_id)
                        || descriptor
                            .and_then(|subagent| subagent.root_tool_use_id.as_ref())
                            .is_some_and(|root| active_calls.contains(root))
                })
                .map(|(task_id, _)| task_id.clone()),
        );
        if legacy_fallback {
            reachable_subagents.extend(
                self.state
                    .session
                    .subagent_messages()
                    .keys()
                    .filter(|task_id| {
                        !self
                            .state
                            .session
                            .subagent_task_specs()
                            .contains_key(*task_id)
                    })
                    .filter(|task_id| {
                        !self.state.session.subagents().iter().any(|subagent| {
                            subagent.tool_use_id.as_str() != task_id.as_str()
                                && subagent.parent_tool_use_id.as_ref() == Some(*task_id)
                        })
                    })
                    .cloned(),
            );
        }
        let mut subagent_versions = caudra_agent::active_task_history_versions_with_batch_state(
            &active_history,
            |call_id| {
                self.state
                    .session
                    .tool_outputs()
                    .get(call_id)
                    .and_then(|output| output.state())
            },
        );
        for subagent in self.state.session.subagents() {
            if reachable_subagents.contains(&subagent.tool_use_id)
                && !subagent_versions.contains_key(&subagent.tool_use_id)
                && let Some(version_id) = &subagent.parent_tool_use_id
                && self
                    .state
                    .session
                    .subagent_messages()
                    .contains_key(version_id)
            {
                subagent_versions.insert(subagent.tool_use_id.clone(), version_id.clone());
            }
        }
        // `todo_write` replaces the whole list every call, so the last one in
        // the transcript is the current plan.
        if let Some(items) =
            display_msgs
                .iter()
                .rev()
                .find_map(|msg| match msg.tool_output.as_deref() {
                    Some(caudra_agent::types::ToolOutput::TodoList(items)) => Some(items.clone()),
                    _ => None,
                })
        {
            self.todo_panel.set_items(items);
        }
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
            let Some(media_type) = caudra_providers::ImageMediaType::from_mime(&image.media_type)
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
                            id: caudra_agent::QueueItemId::new(),
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
        for sa in self
            .state
            .session
            .subagents()
            .iter()
            .filter(|subagent| reachable_subagents.contains(&subagent.tool_use_id))
            .cloned()
            .collect::<Vec<_>>()
        {
            // A subagent reaches disk when it spawns but its transcript only
            // when it ends, so one without an entry here never got to finish:
            // leftovers from a kill mid-turn. It has nothing to show, and
            // restoring it would park a task no agent backs at the top of the
            // picker, running forever. `sync_subagents` below drops it for good.
            let version_id = subagent_versions
                .get(&sa.tool_use_id)
                .unwrap_or(&sa.tool_use_id);
            let Some(messages) = self
                .state
                .session
                .subagent_messages()
                .get(version_id)
                .or_else(|| self.state.session.subagent_messages().get(&sa.tool_use_id))
            else {
                continue;
            };
            let (display, items) = history_to_display(
                messages,
                self.state.session.tool_outputs(),
                &self.ui_config.tool_output_lines,
                self.ui_config.show_reminders,
            );
            self.chat_index
                .insert(sa.tool_use_id.clone(), self.chats.len());
            let mut chat = Chat::subagent(
                &sa.tool_use_id,
                sa.name,
                self.ui_config.clone(),
                self.lua_event_handle.clone(),
            );
            if let Some(parent_tool_use_id) = sa.parent_tool_use_id {
                chat.set_parent_tool_use_id(parent_tool_use_id);
            }
            chat.set_restore_channel(self.restore_event_tx.clone());
            chat.model_id = sa.model;
            chat.load_messages(display);
            let (outcome, text) = restored_subagent_outcome(sa.outcome);
            chat.mark_finished(outcome, text);
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

    fn fire_restore_items(&self, items: Vec<caudra_lua::RestoreItem>) {
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
        self.permissions.load_structured_conversation_rules(
            self.state.session.meta.structured_permission_rules.clone(),
        );
        self.apply_stored_yolo(&self.state.session.meta);
        self.restore_display();
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
        self.merged_history = None;
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

    pub(crate) fn reset_session(&mut self) -> Vec<Action> {
        if self.cancelling_run.is_some() {
            self.status_bar.flash(REVERT_BUSY_MSG.into());
            return Vec::new();
        }
        let replacement = self.state.session.workspace_binding().map_or_else(
            || AppSession::new(&self.state.session.model, &self.state.session.cwd),
            |binding| {
                AppSession::new_with_workspace(
                    &self.state.session.model,
                    &self.state.session.cwd,
                    binding.clone(),
                )
            },
        );
        let lease = match SessionLease::acquire(&self.storage, replacement.id) {
            Ok(lease) => Arc::new(lease),
            Err(error) => {
                self.status_bar
                    .flash(format!("Failed to reserve new session: {error}"));
                return Vec::new();
            }
        };
        let replacement_store = if self.workspace_baseline.is_remote() {
            Arc::new(SnapshotStore::new(
                self.storage
                    .path()
                    .join("remote-snapshot-metadata")
                    .join(replacement.id.to_string()),
            ))
        } else {
            match Self::snapshot_store_for(
                &self.storage,
                replacement.id,
                std::path::Path::new(&replacement.cwd),
                self.snapshots_config.into(),
            ) {
                Ok(store) => store,
                Err(error) => {
                    self.status_bar
                        .flash(format!("Failed to initialize workspace snapshots: {error}"));
                    return Vec::new();
                }
            }
        };
        if let Err(error) = self.retire_current_session() {
            self.status_bar
                .flash(format!("Failed to retire current session: {error}"));
            return Vec::new();
        }
        self.reset_ui_chrome();
        self.state.token_usage = TokenUsage::default();
        self.state.cost = None;
        self.state.context_size = 0;
        self.state.goal = GoalHandle::default();
        self.goal_deferred = false;
        self.state.plan = PlanState::None;
        self.permissions
            .load_structured_conversation_rules(Vec::new());
        self.permissions.set_session_yolo(None);
        if self.state.mode == Mode::Plan {
            self.enter_plan();
        }
        // Fire before the swap. A handler cleaning up after the session
        // that just ended needs its id, and the stamp always reads
        // whichever session is current.
        self.fire_session_autocmd("SessionReset", serde_json::json!({}));
        let replacement_cwd = PathBuf::from(&replacement.cwd);
        self.state.session = Arc::new(replacement);
        if let (Some(workspace), Some(binding)) = (
            self.workspace_session.clone(),
            self.state.session.workspace_binding().cloned(),
        ) {
            self.workspace_baseline.rebind_workspace_session(
                self.storage.clone(),
                self.state.session.id,
                workspace,
                binding,
            );
            self.snapshot_store = replacement_store;
        } else {
            self.rebind_workspace_baseline(replacement_store, replacement_cwd);
        }
        caudra_otel::emit::session_started(
            caudra_otel::emit::START_FRESH,
            Some(&self.state.session.id.to_string()),
        );
        self.install_local_history();
        vec![Action::NewSession(lease)]
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

    pub fn revert_to(&mut self, item_id: CaudraId, mode: RestoreMode) -> Vec<Action> {
        self.revert_to_with_policy(item_id, mode, ConflictPolicy::Abort)
    }

    pub fn revert_at(&mut self, source: DisplaySource, mode: RestoreMode) -> Vec<Action> {
        self.revert_to(source_target_id(source), mode)
    }

    pub fn revert_to_with_policy(
        &mut self,
        item_id: CaudraId,
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
        // Say why the files cannot move, then do the half that can. A raw
        // not-found out of the store names a missing manifest rather than the
        // reason there is no manifest to miss.
        let mode = match self.file_revert_blocker().filter(|_| mode.restores_files()) {
            None => mode,
            Some(blocker) => {
                self.status_bar.flash(blocker);
                if !mode.restores_conversation() {
                    return Vec::new();
                }
                RestoreMode::Conversation
            }
        };
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
        if !self.workspace_baseline.is_remote()
            && previous.is_none()
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

        if self.workspace_baseline.is_remote() && mode.restores_files() {
            return self.revert_remote_workspace(
                conversation_source,
                target,
                pending,
                mode,
                policy,
            );
        }

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
        let operation_id = CaudraId::generate();
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

    fn revert_remote_workspace(
        &mut self,
        conversation_source: Option<CaudraId>,
        target: RevertTarget,
        mut pending: PendingConversationRevert,
        mode: RestoreMode,
        policy: ConflictPolicy,
    ) -> Vec<Action> {
        if policy == ConflictPolicy::Overwrite {
            self.status_bar
                .flash("Remote snapshot conflicts cannot be overwritten automatically".into());
            return Vec::new();
        }
        let prepared = match self.remote_restore_confirmation.take() {
            Some(confirmation)
                if confirmation.conversation_source == conversation_source
                    && confirmation.target.head == target.head
                    && confirmation.mode == mode =>
            {
                pending = confirmation.pending;
                confirmation.prepared
            }
            previous => {
                if let Some(previous) = previous {
                    let _ = smol::block_on(
                        self.workspace_baseline
                            .release_remote_prepared(&previous.prepared),
                    );
                }
                let prepared = match smol::block_on(
                    self.workspace_baseline.prepare_remote_restore(target.head),
                ) {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        self.status_bar
                            .flash(format!("Remote workspace restore preview failed: {error}"));
                        return Vec::new();
                    }
                };
                let change_count = match &prepared.preview {
                    caudra_workspace::SnapshotOperationPreview::Restore(preview) => {
                        preview.changes.len()
                    }
                    _ => {
                        self.status_bar
                            .flash("Remote workspace restore preview was invalid".into());
                        return Vec::new();
                    }
                };
                self.remote_restore_confirmation = Some(RemoteRestoreConfirmation {
                    conversation_source,
                    target,
                    pending,
                    mode,
                    prepared,
                });
                self.status_bar.flash(format!(
                    "Remote restore preview: {change_count} change{}. Repeat rewind to confirm",
                    if change_count == 1 { "" } else { "s" }
                ));
                return Vec::new();
            }
        };
        let change_count = match &prepared.preview {
            caudra_workspace::SnapshotOperationPreview::Restore(preview) => preview.changes.len(),
            _ => {
                self.status_bar
                    .flash("Remote workspace restore preview was invalid".into());
                return Vec::new();
            }
        };
        let operation_id = CaudraId::generate();
        pending.restore_operation = Some(PendingRestoreOperation {
            id: operation_id,
            kind: PendingRestoreKind::Revert,
            phase: PendingRestorePhase::Intent,
            target_workspace_head: target.head.into(),
            conversation_target: mode.restores_conversation().then_some(target.head.into()),
            overwrite: false,
        });
        let before_intent = Arc::clone(&self.state.session);
        self.state
            .session_mut()
            .set_conversation_state(conversation_source, Some(pending));
        if let Err(error) = self.save_session_barrier() {
            self.state.session = before_intent;
            let _ = smol::block_on(self.workspace_baseline.release_remote_prepared(&prepared));
            self.status_bar.flash(format!(
                "Failed to save remote workspace restore intent: {error}"
            ));
            return Vec::new();
        }
        self.status_bar.flash(format!(
            "Restoring {change_count} remote workspace change{}",
            if change_count == 1 { "" } else { "s" }
        ));
        let status = match smol::block_on(
            self.workspace_baseline
                .execute_remote_restore(prepared, target.head),
        ) {
            Ok(status) => status,
            Err(error) => {
                self.status_bar.flash(format!(
                    "Remote workspace restore requires recovery before more changes: {error}"
                ));
                return Vec::new();
            }
        };
        let definitive = matches!(
            status.state,
            caudra_workspace::SnapshotRestoreState::Completed
                | caudra_workspace::SnapshotRestoreState::Acknowledged
        ) && !status.reconciliation_required;
        if !definitive {
            if let Some(mut pending) = self.state.session.meta.pending_revert.clone() {
                pending.file_status = serde_json::to_value(&status).ok();
                self.state
                    .session_mut()
                    .set_conversation_state(conversation_source, Some(pending));
                let _ = self.save_session_barrier();
            }
            let state = match status.state {
                caudra_workspace::SnapshotRestoreState::Partial => "partial",
                caudra_workspace::SnapshotRestoreState::Indeterminate => "indeterminate",
                _ => "still in progress",
            };
            self.status_bar.flash(format!(
                "Remote workspace restore is {state}; recovery is required before more changes"
            ));
            return Vec::new();
        }

        let Some(mut applied) = self.state.session.meta.pending_revert.clone() else {
            self.status_bar
                .flash("Remote workspace restore intent disappeared".into());
            return Vec::new();
        };
        applied.workspace_head = Some(target.head.into());
        applied.file_status = serde_json::to_value(&status).ok();
        if let Some(operation) = applied.restore_operation.as_mut() {
            operation.phase = PendingRestorePhase::FilesApplied;
        }
        let conversation_target = if mode.restores_conversation() {
            target.head
        } else {
            conversation_source
        };
        self.state
            .session_mut()
            .set_conversation_state(conversation_target, Some(applied));
        if let Err(error) = self.save_session_barrier() {
            self.status_bar.flash(format!(
                "Remote workspace restore completed, but transcript recovery is required: {error}"
            ));
            return Vec::new();
        }
        let Some(mut completed) = self.state.session.meta.pending_revert.clone() else {
            return Vec::new();
        };
        completed.restore_operation = None;
        self.state
            .session_mut()
            .set_conversation_state(conversation_target, Some(completed));
        if let Err(error) = self.save_session_barrier() {
            self.status_bar.flash(format!(
                "Remote workspace restore acknowledgement needs recovery: {error}"
            ));
            return Vec::new();
        }
        if mode.restores_conversation() {
            self.finish_conversation_restore(conversation_source, target)
        } else {
            Vec::new()
        }
    }

    fn finish_conversation_restore(
        &mut self,
        previous_head: Option<CaudraId>,
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
        if self.workspace_baseline.is_remote() {
            return self.unrevert_remote_workspace(pending);
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
            let operation_id = CaudraId::generate();
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

    fn unrevert_remote_workspace(&mut self, mut pending: PendingConversationRevert) -> Vec<Action> {
        let Some(file_status) = pending.file_status.as_ref() else {
            let current_head = crate::session_history_head(&self.state.session);
            self.state
                .session_mut()
                .set_conversation_state(pending.original_head, None);
            self.update_context_for_head(current_head);
            return self.finish_remote_unrevert_display();
        };
        let status = match serde_json::from_value::<caudra_workspace::SnapshotRestoreStatus>(
            file_status.clone(),
        ) {
            Ok(status)
                if matches!(
                    status.state,
                    caudra_workspace::SnapshotRestoreState::Completed
                        | caudra_workspace::SnapshotRestoreState::Acknowledged
                ) && !status.reconciliation_required =>
            {
                status
            }
            _ => {
                self.status_bar
                    .flash("Remote workspace restore is not complete; recovery is required".into());
                return Vec::new();
            }
        };
        let prepared = match smol::block_on(
            self.workspace_baseline
                .prepare_remote_unrevert(&status.restore_id),
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.status_bar
                    .flash(format!("Remote workspace unrevert preview failed: {error}"));
                return Vec::new();
            }
        };
        self.checkpoint_now();
        let current_head = crate::session_history_head(&self.state.session);
        pending.restore_operation = Some(PendingRestoreOperation {
            id: CaudraId::generate(),
            kind: PendingRestoreKind::Unrevert,
            phase: PendingRestorePhase::Intent,
            target_workspace_head: pending_original_workspace_head(&pending).into(),
            conversation_target: Some(pending.original_head.into()),
            overwrite: false,
        });
        let before_intent = Arc::clone(&self.state.session);
        self.state
            .session_mut()
            .set_conversation_state(current_head, Some(pending.clone()));
        if let Err(error) = self.save_session_barrier() {
            self.state.session = before_intent;
            let _ = smol::block_on(self.workspace_baseline.release_remote_prepared(&prepared));
            self.status_bar.flash(format!(
                "Failed to save remote workspace unrevert intent: {error}"
            ));
            return Vec::new();
        }
        let restore = match smol::block_on(self.workspace_baseline.execute_remote_unrevert(
            prepared,
            pending_original_workspace_head(&pending),
            status.restore_id,
        )) {
            Ok(status) => status,
            Err(error) => {
                self.status_bar.flash(format!(
                    "Remote workspace unrevert requires recovery before more changes: {error}"
                ));
                return Vec::new();
            }
        };
        if !matches!(
            restore.state,
            caudra_workspace::SnapshotRestoreState::Completed
                | caudra_workspace::SnapshotRestoreState::Acknowledged
        ) || restore.reconciliation_required
        {
            if let Some(mut pending) = self.state.session.meta.pending_revert.clone() {
                pending.file_status = serde_json::to_value(&restore).ok();
                self.state
                    .session_mut()
                    .set_conversation_state(current_head, Some(pending));
                let _ = self.save_session_barrier();
            }
            self.status_bar.flash(
                "Remote workspace unrevert is partial or indeterminate; recovery is required"
                    .into(),
            );
            return Vec::new();
        }
        self.state
            .session_mut()
            .set_conversation_state(pending.original_head, None);
        self.update_context_for_head(current_head);
        if let Err(error) = self.save_session_barrier() {
            self.state.session = before_intent;
            self.status_bar.flash(format!(
                "Remote workspace unrevert completed, but transcript recovery is required: {error}"
            ));
            return Vec::new();
        }
        if let Err(error) = smol::block_on(
            self.workspace_baseline
                .acknowledge_remote_restore(&restore.restore_id),
        ) {
            self.status_bar.flash(format!(
                "Remote workspace unrevert completed, but acknowledgement is required: {error}"
            ));
            return Vec::new();
        }
        self.finish_remote_unrevert_display()
    }

    fn finish_remote_unrevert_display(&mut self) -> Vec<Action> {
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
        operation_id: CaudraId,
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
        let mut child = self.state.session.workspace_binding().map_or_else(
            || AppSession::new(&self.state.session.model, &self.state.session.cwd),
            |binding| {
                AppSession::new_with_workspace(
                    &self.state.session.model,
                    &self.state.session.cwd,
                    binding.clone(),
                )
            },
        );
        let lease = Arc::new(
            SessionLease::acquire(&self.storage, child.id)
                .map_err(|error| format!("Failed to reserve fork session: {error}"))?,
        );
        child.meta = SessionMeta {
            system_prompt_profile: if self.state.system_prompt_profile_override {
                self.state.session.meta.system_prompt_profile.clone()
            } else {
                Some(self.state.system_prompt_profile_name.clone())
            },
            mode: Some(self.state.mode.into()),
            plan_path: self
                .state
                .plan
                .path()
                .map(|path| path.to_string_lossy().into_owned()),
            plan_target: plan_target(&self.state.plan),
            plan_written: self.state.plan.is_ready(),
            thinking: Some(self.state.thinking.clone().into()),
            fast: self.state.fast,
            ..SessionMeta::default()
        };
        if matches!(
            child.meta.plan_target,
            Some(StoredPlanTarget::PlanRef { .. })
        ) {
            child.meta.plan_target = None;
            child.meta.plan_path = None;
            child.meta.plan_written = false;
        }
        child.replace_messages(ancestor.clone());
        child.set_title(self.next_fork_title()?);

        let mut output_ids = HashSet::new();
        let mut output_refs = Vec::new();
        collect_tool_output_refs(&ancestor, &mut output_ids, &mut output_refs);
        let mut stored_tool_ids = all_tool_call_ids(&ancestor);
        let mut reachable = reachable_subagent_ids(
            &ancestor,
            self.state.session.subagent_messages(),
            self.state.session.tool_outputs(),
            self.state.session.subagents(),
        );
        let mut versions =
            caudra_agent::active_task_history_versions_with_batch_state(&ancestor, |call_id| {
                self.state
                    .session
                    .tool_outputs()
                    .get(call_id)
                    .and_then(|output| output.state())
            });
        reachable.extend(
            versions
                .iter()
                .filter(|(_, version_id)| {
                    self.state
                        .session
                        .subagent_messages()
                        .contains_key(*version_id)
                })
                .map(|(task_id, _)| task_id.clone()),
        );
        let legacy_fallback = reachable.is_empty()
            || ancestor.iter().any(|item| {
                matches!(
                    &item.kind,
                    HistoryItemKind::AssistantText {
                        retained_subagent_ids,
                        is_compaction_summary: true,
                        ..
                    } if retained_subagent_ids.is_empty()
                )
            });
        let mut active_calls = caudra_agent::history_tool_call_ids(&ancestor);
        for task_id in &reachable {
            if let Some(history) = self.state.session.subagent_messages().get(task_id) {
                active_calls.extend(caudra_agent::history_tool_call_ids(history));
            }
        }
        reachable.extend(
            self.state
                .session
                .subagent_task_specs()
                .iter()
                .filter(|(task_id, spec)| {
                    if !spec.is_generic() {
                        return false;
                    }
                    let descriptor = self
                        .state
                        .session
                        .subagents()
                        .iter()
                        .find(|subagent| subagent.tool_use_id == **task_id);
                    descriptor.is_none()
                        || active_calls.contains(*task_id)
                        || descriptor
                            .and_then(|subagent| subagent.root_tool_use_id.as_ref())
                            .is_some_and(|root| active_calls.contains(root))
                })
                .map(|(task_id, _)| task_id.clone()),
        );
        if legacy_fallback {
            reachable.extend(
                self.state
                    .session
                    .subagent_messages()
                    .keys()
                    .filter(|task_id| {
                        !self
                            .state
                            .session
                            .subagent_task_specs()
                            .contains_key(*task_id)
                    })
                    .cloned(),
            );
        }
        for subagent in self.state.session.subagents() {
            if reachable.contains(&subagent.tool_use_id)
                && !versions.contains_key(&subagent.tool_use_id)
                && let Some(version_id) = &subagent.parent_tool_use_id
                && self
                    .state
                    .session
                    .subagent_messages()
                    .contains_key(version_id)
            {
                versions.insert(subagent.tool_use_id.clone(), version_id.clone());
            }
        }
        let version_ids: HashSet<&str> = versions
            .iter()
            .filter_map(|(task_id, version_id)| {
                (task_id != version_id
                    && self
                        .state
                        .session
                        .subagent_messages()
                        .contains_key(version_id))
                .then_some(version_id.as_str())
            })
            .collect();
        for task_id in reachable
            .iter()
            .filter(|task_id| !version_ids.contains(task_id.as_str()))
        {
            let version_id = versions.get(task_id).unwrap_or(task_id);
            let Some(history) = self
                .state
                .session
                .subagent_messages()
                .get(version_id)
                .or_else(|| self.state.session.subagent_messages().get(task_id))
            else {
                continue;
            };
            collect_tool_output_refs(history, &mut output_ids, &mut output_refs);
            stored_tool_ids.extend(all_tool_call_ids(history));
            child.set_subagent_history(
                task_id.clone(),
                history.to_vec(),
                self.state
                    .session
                    .subagent_task_specs()
                    .get(task_id)
                    .cloned(),
            );
        }
        for tool_id in &stored_tool_ids {
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

        if !output_refs.is_empty() {
            ToolOutputStore::new(self.storage.clone())
                .copy_session_outputs(self.state.session.id, child.id, &output_refs)
                .map_err(|error| {
                    format!("Failed to copy managed tool outputs for fork: {error}")
                })?;
        }

        if !self.workspace_baseline.is_remote() {
            let child_snapshots = Self::snapshot_store_for(
                &self.storage,
                child.id,
                std::path::Path::new(&child.cwd),
                self.snapshots_config.into(),
            )
            .map_err(|error| format!("Failed to initialize workspace snapshots: {error}"))?;
            self.snapshot_store
                .copy_ancestry_to(
                    &child_snapshots,
                    &ancestor.iter().map(|item| item.id).collect::<Vec<_>>(),
                )
                .map_err(|error| format!("Failed to copy workspace snapshots: {error}"))?;
        }

        Ok(ForkedSession {
            session: child,
            lease,
            draft: target
                .draft
                .map(|(text, images)| ForkDraft { text, images }),
        })
    }

    fn next_fork_title(&self) -> Result<String, String> {
        let (base, own_number) = split_fork_title(&self.state.session.title);
        let sessions = if self.workspace_baseline.is_remote() {
            let binding = self
                .state
                .session
                .workspace_binding()
                .ok_or_else(|| "Remote session workspace identity is unavailable".to_owned())?;
            SessionDatabase::open_state(&self.storage)
                .and_then(|database| database.list_for_workspace(binding))
        } else {
            AppSession::list(&self.state.session.cwd, &self.storage)
        }
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

    fn update_context_for_head(&mut self, previous_head: Option<CaudraId>) {
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
        if self.workspace_session.is_none() {
            caudra_storage::workspace_binding::StoredWorkspaceBinding::validate_resume(
                session.workspace_binding(),
                None,
            )
            .map_err(|error| error.to_string())?;
        }
        let remote_target = self
            .workspace_session
            .clone()
            .map(|workspace| {
                let binding =
                    caudra_storage::workspace_binding::StoredWorkspaceBinding::new_with_cursor(
                        workspace.binding().clone(),
                        workspace.cursor().clone(),
                        session
                            .workspace_binding()
                            .and_then(|binding| binding.cursor_label().map(str::to_owned)),
                    )
                    .map_err(|error| {
                        format!("Remote workspace snapshot identity is invalid: {error}")
                    })?;
                if session
                    .workspace_binding()
                    .is_none_or(|stored| !stored.exact_scope_eq(&binding))
                {
                    return Err("Session belongs to a different remote workspace cursor".to_owned());
                }
                Ok((workspace, binding))
            })
            .transpose()?;
        let snapshot_store = if let Some((workspace, binding)) = &remote_target {
            session
                .replace_workspace_cursor(binding.clone())
                .map_err(|error| error.to_string())?;
            let baseline = WorkspaceBaseline::new_workspace_session(
                self.storage.clone(),
                session.id,
                workspace.clone(),
                binding.clone(),
                self.snapshots_config.enabled,
            );
            let recovered = smol::block_on(baseline.reconcile_remote_restore())
                .map_err(|error| format!("Remote workspace snapshot recovery failed: {error}"))?;
            crate::event_loop::reconcile_remote_session_restore(
                &mut session,
                &self.storage_writer,
                &baseline,
                recovered,
            )?;
            Arc::new(SnapshotStore::new(
                self.storage
                    .path()
                    .join("remote-snapshot-metadata")
                    .join(session.id.to_string()),
            ))
        } else {
            let store = Self::snapshot_store_for(
                &self.storage,
                session.id,
                std::path::Path::new(&session.cwd),
                self.snapshots_config.into(),
            )
            .map_err(|error| format!("Failed to initialize workspace snapshots: {error}"))?;
            recover_pending_workspace_restore(&mut session, &store, &self.storage_writer)?;
            store
        };
        self.retire_current_session()
            .map_err(|error| format!("Failed to retire current session: {error}"))?;
        self.permissions
            .load_structured_conversation_rules(session.meta.structured_permission_rules.clone());
        self.apply_stored_yolo(&session.meta);
        self.state =
            SessionState::from_session(session, fallback_model, &self.storage, &self.model_policy);
        if let Some((workspace, binding)) = remote_target {
            self.workspace_baseline.rebind_workspace_session(
                self.storage.clone(),
                self.state.session.id,
                workspace,
                binding,
            );
            self.snapshot_store = snapshot_store;
        } else {
            let cwd = PathBuf::from(&self.state.session.cwd);
            self.rebind_workspace_baseline(snapshot_store, cwd);
        }
        for w in self.state.warnings.drain(..) {
            self.status_bar.flash(w);
        }
        self.reset_ui_chrome();
        self.restore_display();

        self.request_pattern_suggestions();
        Ok(self.install_local_history())
    }

    fn retire_current_session(&mut self) -> Result<(), caudra_storage::sessions::SessionError> {
        self.release_remote_restore_confirmation();
        self.checkpoint_now();
        if self.has_content() {
            self.storage_writer
                .save_sync(Arc::clone(&self.state.session))
        } else {
            self.storage_writer.delete_sync(self.state.session.id)
        }
    }

    #[cfg(test)]
    pub(crate) fn load_session(&mut self, session_id: CaudraId) -> Vec<Action> {
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

fn source_target_id(source: DisplaySource) -> CaudraId {
    match source {
        DisplaySource::User(id)
        | DisplaySource::AssistantText(id)
        | DisplaySource::Reasoning(id)
        | DisplaySource::ToolResult(id) => id,
        DisplaySource::ToolCall { id, result_id } => result_id.unwrap_or(id),
    }
}

fn tool_call_ids(items: &[HistoryItem]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for item in items {
        match &item.kind {
            HistoryItemKind::ToolCall { call_id, name, .. }
                if caudra_agent::tools::is_container_tool(name) =>
            {
                ids.insert(call_id.clone());
            }
            HistoryItemKind::AssistantText {
                retained_subagent_ids,
                ..
            } => ids.extend(retained_subagent_ids.iter().cloned()),
            _ => {}
        }
    }
    ids
}

fn all_tool_call_ids(items: &[HistoryItem]) -> HashSet<String> {
    items
        .iter()
        .filter_map(|item| match &item.kind {
            HistoryItemKind::ToolCall { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect()
}

pub(crate) fn reachable_subagent_ids(
    items: &[HistoryItem],
    histories: &HashMap<String, Arc<Vec<HistoryItem>>>,
    tool_outputs: &HashMap<String, Arc<caudra_agent::ToolOutput>>,
    subagents: &[StoredSubagent],
) -> HashSet<String> {
    let mut reachable = tool_call_ids(items);
    let mut active_calls = caudra_agent::history_tool_call_ids(items);
    let mut visited = HashSet::new();
    loop {
        for subagent in subagents {
            if subagent
                .parent_tool_use_id
                .as_ref()
                .is_some_and(|parent| reachable.contains(parent) || active_calls.contains(parent))
                || subagent
                    .root_tool_use_id
                    .as_ref()
                    .is_some_and(|root| active_calls.contains(root))
            {
                reachable.insert(subagent.tool_use_id.clone());
            }
        }
        let Some(task_id) = reachable
            .iter()
            .find(|task_id| !visited.contains(*task_id))
            .cloned()
        else {
            break;
        };
        visited.insert(task_id.clone());
        if let Some(state) = tool_outputs.get(&task_id).and_then(|output| output.state()) {
            collect_task_metadata_from_value(state, &mut reachable);
        }
        if let Some(history) = histories.get(&task_id) {
            reachable.extend(tool_call_ids(history));
            active_calls.extend(caudra_agent::history_tool_call_ids(history));
        }
    }
    reachable
}

fn collect_task_metadata_from_value(value: &serde_json::Value, ids: &mut HashSet<String>) {
    match value {
        serde_json::Value::String(text) => collect_task_metadata(text, ids),
        serde_json::Value::Array(values) => {
            for value in values {
                collect_task_metadata_from_value(value, ids);
            }
        }
        serde_json::Value::Object(values) => {
            if let Some(tool) = values.get("tool").and_then(serde_json::Value::as_str) {
                if tool == "task" {
                    if let Some(invocation_id) = values
                        .get("invocation_id")
                        .and_then(serde_json::Value::as_str)
                    {
                        ids.insert(invocation_id.to_owned());
                    }
                    if let Some(output) = values.get("output").and_then(serde_json::Value::as_str) {
                        collect_task_metadata(output, ids);
                    }
                }
                return;
            }
            for value in values.values() {
                collect_task_metadata_from_value(value, ids);
            }
        }
        _ => {}
    }
}

fn collect_task_metadata(content: &str, ids: &mut HashSet<String>) {
    for block in content.split("<task_metadata>").skip(1) {
        let Some(metadata) = block.split("</task_metadata>").next() else {
            continue;
        };
        if let Some(task_id) = metadata
            .lines()
            .find_map(|line| line.trim().strip_prefix("task_id: "))
        {
            ids.insert(task_id.to_owned());
        }
    }
}

fn collect_tool_output_refs(
    items: &[HistoryItem],
    ids: &mut HashSet<ToolOutputId>,
    references: &mut Vec<ToolOutputRef>,
) {
    for item in items {
        match &item.kind {
            HistoryItemKind::ToolResult {
                output_ref: Some(output_ref),
                ..
            } => push_tool_output_ref(output_ref, ids, references),
            HistoryItemKind::AssistantText {
                retained_output_refs,
                ..
            } => {
                for output_ref in retained_output_refs {
                    push_tool_output_ref(output_ref, ids, references);
                }
            }
            _ => {}
        }
    }
}

fn push_tool_output_ref(
    output_ref: &ToolOutputRef,
    ids: &mut HashSet<ToolOutputId>,
    references: &mut Vec<ToolOutputRef>,
) {
    if ids.insert(output_ref.id) {
        references.push(output_ref.clone());
    }
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
    current_head: Option<CaudraId>,
    item_id: CaudraId,
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
    result_id: CaudraId,
) -> Result<CaudraId, String> {
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

fn complete_group_head(items: &[HistoryItem], item_id: CaudraId) -> Result<CaudraId, String> {
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

fn checkpoint_chain(
    items: &[HistoryItem],
    head: Option<CaudraId>,
) -> Result<Vec<CaudraId>, String> {
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
                    ..
                } if content == caudra_agent::UNAVAILABLE_RESULT
                    && images.is_empty()
                    && call_ids.remove(call_id.as_str())
            )
    }) && call_ids.is_empty()
}

fn pending_workspace_head(pending: &PendingConversationRevert) -> Option<CaudraId> {
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

fn pending_original_workspace_head(pending: &PendingConversationRevert) -> Option<CaudraId> {
    pending
        .original_workspace_head
        .as_ref()
        .map(|head| head.head)
        .unwrap_or(pending.original_head)
}

fn history_tokens(items: &[HistoryItem], head: Option<CaudraId>) -> u32 {
    let Ok(history) = active_history_items(items, head) else {
        return 0;
    };
    let messages = project_messages(&history)
        .or_else(|_| caudra_agent::History::restored(history).map(caudra_agent::History::into_vec));
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
        || serde_json::from_value::<caudra_workspace::SnapshotRestoreStatus>(status.clone())
            .is_ok_and(|status| {
                matches!(
                    status.state,
                    caudra_workspace::SnapshotRestoreState::Completed
                        | caudra_workspace::SnapshotRestoreState::Acknowledged
                ) && !status.reconciliation_required
            })
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

/// The picker merges two sources: sessions this process has open (which the
/// event loop publishes, and only it can see) and everything else on disk.
/// Live rows win, because a session mid-turn has state the store has not
/// caught up with yet.
impl App {
    pub(super) fn sessions_browse(&mut self) -> Vec<Action> {
        let rows = self.session_rows();
        // Both halves are read here, so both watches start from what the
        // picker is already showing rather than reporting their own birth as
        // an arrival and rebuilding the list on the very next tick.
        self.stored_session_generation = self.storage_writer.generation();
        self.live_session_watch = Watch::seeded(self.live_sessions.load_full());
        self.session_picker.open(rows, now_secs());
        Vec::new()
    }

    /// Both halves are polled, because each moves on its own: renaming or
    /// deleting a session the event loop has not got open touches only the
    /// store, and the writer thread is what finishes it, so the picker used
    /// to keep showing a row that no longer existed.
    pub(crate) fn refresh_session_picker(&mut self) -> Dirty {
        if !self.session_picker.is_open() {
            return Dirty::NO;
        }
        let generation = self.storage_writer.generation();
        // `|` never short-circuits: the live watch must poll either way, or it
        // reports the arrival it missed on some later, unrelated tick.
        let changed = self.live_session_watch.poll(self.live_sessions.load_full())
            | Dirty::from(generation != self.stored_session_generation);
        self.stored_session_generation = generation;
        if changed == Dirty::NO {
            return Dirty::NO;
        }
        let rows = self.session_rows();
        self.session_picker.refresh(rows, now_secs());
        Dirty::YES
    }

    pub(super) fn handle_session_picker_action(
        &mut self,
        action: SessionPickerAction,
    ) -> Vec<Action> {
        match action {
            SessionPickerAction::Consumed | SessionPickerAction::Closed => Vec::new(),
            SessionPickerAction::Focus(id) => vec![Action::FocusSession(id)],
            SessionPickerAction::Delete(id) => vec![Action::DeleteSession(id)],
            SessionPickerAction::Rename { id, title } => {
                vec![Action::SetSessionTitle { id, title }]
            }
            SessionPickerAction::Generate(id) => vec![Action::GenerateSessionTitle(id)],
            SessionPickerAction::New => vec![Action::RequestNewSession],
            SessionPickerAction::MoveCurrent => vec![Action::OpenSessionRelocation {
                bulk: false,
                destination: None,
            }],
            SessionPickerAction::MigrateDirectory => vec![Action::OpenSessionRelocation {
                bulk: true,
                destination: None,
            }],
        }
    }

    pub(crate) fn open_session_relocation(
        &mut self,
        locations: Vec<SessionLocation>,
        bulk: bool,
        destination: Option<String>,
        other_open_count: usize,
    ) {
        self.session_relocation_picker.open(
            self.state.session.id,
            self.state.session.cwd.clone(),
            locations,
            bulk,
            destination,
            other_open_count,
        );
    }

    pub(super) fn handle_session_relocation_action(
        &mut self,
        action: SessionRelocationAction,
    ) -> Vec<Action> {
        match action {
            SessionRelocationAction::Consumed | SessionRelocationAction::Closed => Vec::new(),
            SessionRelocationAction::Confirm(request, donor) => {
                vec![Action::RelocateSessions { request, donor }]
            }
        }
    }

    pub(super) fn rename_session(&mut self, args: &str) -> Vec<Action> {
        let title = args.trim();
        if title.is_empty() {
            self.flash(RENAME_USAGE.into());
            return Vec::new();
        }
        vec![Action::SetSessionTitle {
            id: self.state.session.id,
            title: title.to_owned(),
        }]
    }

    fn session_rows(&self) -> Vec<SessionRow> {
        let live = self.live_sessions.load();
        let seen: HashSet<CaudraId> = live.iter().map(|row| row.id).collect();
        let stored = if self.workspace_baseline.is_remote() {
            let Some(binding) = self.state.session.workspace_binding() else {
                tracing::warn!("failed to list remote sessions without a workspace identity");
                return live.iter().cloned().collect();
            };
            SessionDatabase::open_state(&self.storage)
                .and_then(|database| database.list_for_workspace_identity(binding))
        } else {
            AppSession::list(&self.state.session.cwd, &self.storage)
        }
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "failed to list stored sessions");
            Vec::new()
        });
        live.iter()
            .cloned()
            .chain(
                stored
                    .into_iter()
                    .filter(|summary| !seen.contains(&summary.id))
                    .map(|summary| SessionRow {
                        id: summary.id,
                        title: summary.title,
                        updated_at: summary.updated_at,
                        activity: None,
                        focused: false,
                    }),
            )
            .collect()
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
