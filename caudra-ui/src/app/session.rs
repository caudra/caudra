use std::collections::{HashMap, HashSet};
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use crate::app::tasks::TaskOutcome;
use crate::chat::{CANCELLED_TEXT, Chat, DONE_TEXT, ERROR_TEXT, history_to_display};
use crate::components::rewind_picker::RewindEntry;
use crate::components::session_picker::{OtherCheckout, SessionPickerAction, SessionRow};
use crate::components::session_relocation::SessionRelocationAction;
use crate::components::worktree_picker::{WorktreeAction, WorktreeOverview, WorktreeView};
use crate::components::{
    Action, DisplayMessage, DisplaySource, ForkDraft, ForkedSession, LoadedSession,
};
use crate::input_document::InputDraft;
use crate::repaint::{Dirty, Watch};
use caudra_agent::HistorySnapshot;
use caudra_agent::agent::estimate_message_tokens;
use caudra_agent::peers::PeerSession;
use caudra_agent::permissions::PermissionManager;
use caudra_agent::{GoalHandle, GoalStatus};
use caudra_config::Feature;
use caudra_providers::{
    HistoryItem, HistoryItemKind, HistoryProjectionError, ImageSource, Model, TokenUsage,
    active_history_items, merge_history_items, project_messages, validate_history_items,
};
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::{
    PendingConversationRevert, SessionDatabase, SessionLease, SessionLocation, SessionMeta,
    StoredActiveGoal, StoredGoalResult, StoredImage, StoredMode, StoredPasteRange,
    StoredPlanTarget, StoredPromptAdmission, StoredQueuedDraft, StoredQueuedPrompt, StoredSubagent,
    StoredSubagentOutcome, StoredSubagentTaskSpec,
};
use caudra_storage::tool_outputs::{ToolOutputId, ToolOutputRef, ToolOutputStore};
use caudra_storage::worktrees::{self, CheckoutSessions};

use crate::AppSession;

use super::file_revert::{self, FileRevert, SETTLE_FAILED};
use super::mode::PLAN_COPY_FAILED;
use super::permission_editor::{
    ConversationPermissions, PERMISSION_WORKER_BUSY, attach_session_permissions,
};
use super::session_state::SessionState;
use super::{App, Mode, PendingInput, PlanState, RestoreMode, Status};

/// The shortest gap between two writes that carry only UI state.
const SOFT_SAVE_DELAY: Duration = Duration::from_millis(1000);
/// Below this many restored subagents a resume never pays for a thread.
const PARALLEL_RESTORE_MIN_CHATS: usize = 4;
const RENAME_USAGE: &str = "Usage: /rename <title>";
pub(crate) const REVERT_BUSY_MSG: &str = "Wait for the session to become idle before reverting";

struct PreparedSessionReset {
    session: AppSession,
    lease: Arc<SessionLease>,
    permissions: Arc<PermissionManager>,
    conversation_permissions: ConversationPermissions,
}

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
pub(super) struct RevertTarget {
    pub(super) head: Option<CaudraId>,
    /// The item a file revert is measured from: the prompt itself for a user
    /// prompt, else the kept head.
    pub(super) boundary: Option<CaudraId>,
    draft: Option<(String, Vec<ImageSource>)>,
}

impl RevertTarget {
    /// A target that keeps `head` and everything before it.
    fn kept(head: CaudraId) -> Self {
        Self {
            head: Some(head),
            boundary: Some(head),
            draft: None,
        }
    }
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

/// The producer snapshot `App::checkpoint` last merged, and the session's
/// messages revision the merge left behind.
///
/// The merge walks the whole history graph, and the event loop checkpoints
/// every frame, so an idle session would pay that walk ten times a second
/// forever. `ArcSwap` publishes a fresh `Arc` per mutation, which makes
/// pointer identity an exact "the producer added nothing" test, and holding
/// the `Arc` is what stops a later snapshot from landing on the same address.
/// The revision covers the other side: a rewind or a compaction moves the
/// session's own messages without the producer publishing anything. It is the
/// messages revision because a tool output or a usage record changes nothing
/// the merge reads, and in a long session each one used to cost a full walk.
pub(super) struct MergedHistory {
    snapshot: Arc<HistorySnapshot>,
    messages_revision: u64,
}

/// How many items `snapshot` shares with `merged` when it only appended to it:
/// the same run, strictly longer, and the same ids up to the old end. The ids
/// matter within one run too, since dropping a run marker and then appending
/// keeps the epoch while replacing the item the next one is parented on.
fn appended_len(merged: &HistorySnapshot, snapshot: &HistorySnapshot) -> Option<usize> {
    let known = merged.messages.len();
    let appended = merged.epoch == snapshot.epoch
        && known > 0
        && snapshot.messages.len() > known
        && merged
            .messages
            .iter()
            .zip(snapshot.messages.iter())
            .all(|(old, new)| old.id == new.id);
    appended.then_some(known)
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
        // A name, a subscription, or a group membership is how peers and scripts
        // reach the session, so it is worth keeping before the first message. An
        // inbound policy is not.
        || session.meta.peer_controls.as_ref().is_some_and(|controls| {
            controls.handle.is_some()
                || !controls.topics.is_empty()
                || controls.broadcasts
                || !controls.groups.is_empty()
        })
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
            let appended = self.appended_since_merge(&snapshot);
            let added = appended.is_some() || {
                let known: HashSet<_> = self
                    .state
                    .session
                    .messages()
                    .iter()
                    .map(|item| item.id)
                    .collect();
                snapshot
                    .messages
                    .iter()
                    .any(|item| !known.contains(&item.id))
            };
            let sanitizer_only = added
                && match appended {
                    Some(known) => is_sanitizer_only_unavailable_extension(
                        &snapshot.messages[..known],
                        &snapshot.messages,
                    ),
                    None => {
                        crate::active_session_history(&self.state.session).is_ok_and(|active| {
                            is_sanitizer_only_unavailable_extension(&active, &snapshot.messages)
                        })
                    }
                };
            let actual_work_added = added && !sanitizer_only;
            let settles_file_revert = actual_work_added
                && meta
                    .pending_revert
                    .as_ref()
                    .is_some_and(file_revert::files_pending);
            if settles_file_revert && let Err(error) = self.acknowledge_reverts() {
                self.status_bar.flash(format!("{SETTLE_FAILED}: {error}"));
                return;
            }
            match self.merge_snapshot(&snapshot, appended) {
                Ok(()) => {
                    meta.history_head = snapshot.messages.last().map(|item| item.id);
                    if actual_work_added {
                        meta.pending_revert = None;
                    }
                    if added {
                        self.bind_main_chat_sources_to(&snapshot.messages);
                    }
                    // Only a merge that reached a consistent graph may be
                    // remembered. A failed one has to be retried, and its
                    // flash re-raised, on the next frame.
                    self.merged_history = Some(MergedHistory {
                        snapshot,
                        messages_revision: self.state.session.messages_revision(),
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

        if !self.has_content() {
            // A published session is fenced against its own row, so the row
            // outlives whatever emptied the session.
            if self.conversation_permissions.is_published() {
                return;
            }
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

    /// A merge binds only the rows drawn by then, and the reply a run ends on
    /// is flushed when `Done` arrives, which can be after the merge that
    /// carried its item. No later merge adds anything, so the end of a run
    /// binds again against the history already merged. A history that is not
    /// merged yet is left to the checkpoint that merges it.
    pub(super) fn bind_main_chat_sources(&mut self) {
        let Some(snapshot) = self
            .merged_history
            .as_ref()
            .map(|merged| Arc::clone(&merged.snapshot))
        else {
            return;
        };
        self.bind_main_chat_sources_to(&snapshot.messages);
    }

    fn bind_main_chat_sources_to(&mut self, items: &[HistoryItem]) {
        let (messages, _) = history_to_display(
            items,
            self.state.session.tool_outputs(),
            &self.ui_config.tool_output_lines,
            self.ui_config.show_reminders,
        );
        self.main_chat().bind_sources(&messages);
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
                || merged.messages_revision != self.state.session.messages_revision()
        })
    }

    /// The fast path's starting point: how many items of `snapshot` the store
    /// already holds exactly as the last merge left them. That needs the
    /// producer to have only appended since, and the session to have kept its
    /// messages and its head, so the stored active chain is still the old
    /// snapshot.
    pub(super) fn appended_since_merge(&self, snapshot: &HistorySnapshot) -> Option<usize> {
        let session = &self.state.session;
        let merged = self.merged_history.as_ref().filter(|merged| {
            merged.messages_revision == session.messages_revision()
                && crate::session_history_head(session)
                    == merged.snapshot.messages.last().map(|item| item.id)
        })?;
        appended_len(&merged.snapshot, snapshot)
    }

    /// Brings the producer's snapshot into the stored graph. A pure append
    /// joins as it is, sparing the clone, the graph walk and the three deep
    /// compares of everything already stored, which grow with the session
    /// rather than with the context window. Anything else takes the full merge.
    fn merge_snapshot(
        &mut self,
        snapshot: &HistorySnapshot,
        appended: Option<usize>,
    ) -> Result<(), HistoryProjectionError> {
        if let Some(known) = appended {
            validate_history_items(&snapshot.messages)?;
            let session = self.state.session_mut();
            session.append_history(snapshot, known);
            session.update_title_if_default();
            return Ok(());
        }
        let mut merged = self.state.session.messages().to_vec();
        merge_history_items(&mut merged, &snapshot.messages)?;
        if merged.as_slice() != self.state.session.messages() {
            let session = self.state.session_mut();
            session.merge_history(snapshot, merged);
            session.update_title_if_default();
        }
        Ok(())
    }

    /// Everything the session mirrors from live state, built field by field so
    /// a new `SessionMeta` field forces a decision here. Every frame calls it,
    /// so it stays cheap: an idle UI has an empty draft, queue and rule list,
    /// and an empty `Vec` does not allocate.
    fn build_meta(&self) -> SessionMeta {
        let state = &self.state;
        let peer = self
            .features
            .enabled(Feature::CrossSessionMessaging)
            .then(|| PeerSession::lookup(state.session.id))
            .flatten();
        let draft = self.input_box.draft();
        let queued_prompts = if self.recoverable_queue.is_empty() {
            self.queue.pending_prompts()
        } else {
            self.recoverable_queue.clone()
        };
        let mut meta = SessionMeta {
            system_prompt_profile: if state.system_prompt_profile_override {
                state.session.meta.system_prompt_profile.clone()
            } else {
                Some(state.system_prompt_profile_name.clone())
            },
            history_head: state.session.meta.history_head,
            pending_revert: state.session.meta.pending_revert.clone(),
            mode: Some(state.mode.into()),
            execution_mode: Some(if self.execution_agent_mode().is_planning() {
                StoredMode::Plan
            } else {
                StoredMode::Build
            }),
            plan_path: state.plan.path().map(|p| p.to_string_lossy().into_owned()),
            plan_target: plan_target(&state.plan),
            plan_written: state.plan.is_ready(),
            structured_permission_rules: state.session.meta.structured_permission_rules.clone(),
            permission_generation: state.session.meta.permission_generation,
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
                    mode: prompt.mode,
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
            automatic_wakes_suppressed: self.automatic_wakes_suppressed
                || peer.as_ref().is_some_and(PeerSession::wakes_suppressed),
            permission_mode: self.permissions.persisted_mode(),
            peer_controls: peer
                .map(|peer| peer.controls())
                .or_else(|| state.session.meta.peer_controls.clone()),
            unrecorded: state.session.meta.unrecorded.clone(),
            record_coverage: state.session.meta.record_coverage.clone(),
        };
        if let Some(snapshot) = self.conversation_permissions.published() {
            let snapshot = snapshot.load();
            if let Err(error) = snapshot.apply_to_meta(state.session.id, &mut meta) {
                tracing::error!(%error, session_id = %state.session.id, "permission snapshot owner mismatch");
            }
        } else {
            meta.structured_permission_rules =
                self.permissions.structured_conversation_rules_snapshot();
        }
        meta
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
                    thinking: chat.thinking.clone(),
                    fast: chat.fast,
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
        self.execution_mode = None;
        self.release_background_claims();
        self.task_interactions = super::tasks::TaskInteractions::default();
        self.background_saved_revision = None;
        self.automatic_wakes_suppressed = true;
        self.cancel_queue_edit();
        self.review.discard();
        self.chats.clear();
        let mut main = Chat::new(
            "Main".into(),
            Path::new(&self.state.session.cwd),
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
        self.chord_hint_hit = None;
        self.chord_hint_down = None;
        self.chord_hint_hover = None;
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
        let mut subagent_versions =
            caudra_agent::active_task_history_versions_with_outputs(&active_history, |call_id| {
                self.state
                    .session
                    .tool_outputs()
                    .get(call_id)
                    .map(AsRef::as_ref)
            });
        for subagent in self.state.session.subagents() {
            if reachable_subagents.contains(&subagent.tool_use_id)
                && !subagent_versions.contains_key(&subagent.tool_use_id)
                && let Some(version_id) = &subagent.parent_tool_use_id
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
        // A subagent reaches disk when it spawns but its transcript only when
        // it ends, so one without an entry here never got to finish: leftovers
        // from a kill mid-turn. It has nothing to show, and restoring it would
        // park a task no agent backs at the top of the picker, running forever.
        // `sync_subagents` below drops it for good.
        let restorable: Vec<_> = self
            .state
            .session
            .subagents()
            .iter()
            .filter(|subagent| reachable_subagents.contains(&subagent.tool_use_id))
            .filter_map(|subagent| {
                let version_id = subagent_versions
                    .get(&subagent.tool_use_id)
                    .unwrap_or(&subagent.tool_use_id);
                let messages = self.state.session.subagent_messages().get(version_id)?;
                Some((subagent.clone(), Arc::clone(messages)))
            })
            .collect();
        let rendered = render_subagent_displays(
            &restorable,
            self.state.session.tool_outputs(),
            &self.ui_config.tool_output_lines,
            self.ui_config.show_reminders,
        );
        for ((sa, _), (display, items)) in restorable.into_iter().zip(rendered) {
            self.chat_index
                .insert(sa.tool_use_id.clone(), self.chats.len());
            let mut chat = Chat::subagent(
                &sa.tool_use_id,
                sa.name,
                Path::new(&self.state.session.cwd),
                self.ui_config.clone(),
                self.lua_event_handle.clone(),
            );
            if let Some(parent_tool_use_id) = sa.parent_tool_use_id {
                chat.set_parent_tool_use_id(parent_tool_use_id);
            }
            chat.set_restore_channel(self.restore_event_tx.clone());
            chat.model_id = sa.model;
            chat.thinking = sa.thinking;
            chat.fast = sa.fast;
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

    pub(super) fn fire_restore_items(&self, items: Vec<caudra_lua::RestoreItem>) {
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

    fn apply_stored_permission_mode(&self, meta: &SessionMeta) {
        self.permissions
            .set_session_mode(meta.permission_mode.clone());
    }

    /// Resume at process start: the agent was already spawned with this
    /// history, so no respawn follows and the restored queue must be
    /// flushed here.
    pub(crate) fn restore_resumed_session(&mut self) {
        self.automatic_wakes_suppressed |= self.state.session.meta.automatic_wakes_suppressed;
        if !self.conversation_permissions.is_published() {
            self.permissions.load_structured_conversation_rules(
                self.state.session.meta.structured_permission_rules.clone(),
            );
        }
        self.apply_stored_permission_mode(&self.state.session.meta);
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
        let result = self
            .prepare_session_reset()
            .and_then(|prepared| self.commit_session_reset(prepared, self.state.mode));
        match result {
            Ok(actions) => actions,
            Err(error) => {
                self.flash(error);
                Vec::new()
            }
        }
    }

    /// The plan goes with the work, so the session left behind is saved
    /// without it, and keeps it only if it is not left behind after all.
    pub(crate) fn reset_session_for_plan(&mut self) -> Result<Vec<Action>, String> {
        self.check_run_admission()?;
        let mut prepared = self.prepare_session_reset()?;
        if matches!(
            prepared.conversation_permissions,
            ConversationPermissions::Pending
        ) {
            match attach_session_permissions(
                &self.storage,
                &self.storage_writer,
                &mut prepared.session,
                &prepared.permissions,
            ) {
                Ok(snapshot) => {
                    prepared.conversation_permissions = ConversationPermissions::Published(snapshot)
                }
                Err(error) => {
                    self.discard_unstarted_session(prepared.session.id);
                    return Err(error);
                }
            }
        }
        let plan = mem::take(&mut self.state.plan);
        self.commit_session_reset(prepared, Mode::Build)
            .inspect_err(|_| self.state.plan = plan)
    }

    pub(crate) fn discard_unstarted_session(&self, id: CaudraId) {
        if let Err(error) = self.storage_writer.delete_sync(id) {
            tracing::warn!(%error, session_id = %id, "failed to discard unstarted session");
        }
    }

    fn prepare_session_reset(&self) -> Result<PreparedSessionReset, String> {
        if self.permission_mutation_pending() {
            return Err(PERMISSION_WORKER_BUSY.into());
        }
        if self.cancelling_run.is_some() {
            return Err(REVERT_BUSY_MSG.into());
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
        let lease = SessionLease::acquire(&self.storage, replacement.id)
            .map_err(|error| format!("Failed to reserve new session: {error}"))?;
        let permissions = Arc::new(self.permissions.fork_session(replacement.id));
        Ok(PreparedSessionReset {
            session: replacement,
            lease: Arc::new(lease),
            permissions,
            conversation_permissions: self.conversation_permissions.deferred(),
        })
    }

    fn commit_session_reset(
        &mut self,
        prepared: PreparedSessionReset,
        mode: Mode,
    ) -> Result<Vec<Action>, String> {
        if let Err(error) = self.retire_current_session() {
            if prepared.conversation_permissions.is_published() {
                self.discard_unstarted_session(prepared.session.id);
            }
            return Err(format!("Failed to retire current session: {error}"));
        }
        self.suspend_permission_editor();
        self.permissions = prepared.permissions;
        self.conversation_permissions = prepared.conversation_permissions;
        self.reset_ui_chrome();
        self.state.token_usage = TokenUsage::default();
        self.state.cost = None;
        self.state.context_size = 0;
        self.state.goal = GoalHandle::default();
        self.goal_deferred = false;
        self.state.plan = PlanState::None;
        self.state.mode = mode;
        self.state.applied_mode = mode;
        self.permissions.set_session_mode(None);
        // Fire before the swap. A handler cleaning up after the session
        // that just ended needs its id, and the stamp always reads
        // whichever session is current.
        self.fire_session_autocmd("SessionReset", serde_json::json!({}));
        self.state.session = Arc::new(prepared.session);
        self.automatic_wakes_suppressed = self.state.session.meta.automatic_wakes_suppressed;
        // After the swap: a remote plan document is filed under the session id
        // that will own it, and the retiring session must not be handed one.
        if self.state.mode == Mode::Plan {
            self.enter_plan();
        }
        self.bind_change_recorder();
        self.refresh_record_index();
        caudra_otel::emit::session_started(
            caudra_otel::emit::START_FRESH,
            Some(&self.state.session.id.to_string()),
        );
        self.install_local_history();
        Ok(vec![Action::NewSession(prepared.lease)])
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

    /// Reverts to `item_id`. A file revert previews first and runs when the
    /// same action is repeated; a conflict aborts both halves.
    pub fn revert_to(&mut self, item_id: CaudraId, mode: RestoreMode) -> Vec<Action> {
        if self.is_reverting_blocked() {
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
        let mut blocked = None;
        if mode.restores_files() {
            match self.revert_files(conversation_source, &target, mode) {
                FileRevert::Reverted if mode.restores_conversation() => {
                    return self.finish_conversation_restore(conversation_source, target);
                }
                FileRevert::Reverted | FileRevert::Held => return Vec::new(),
                FileRevert::Blocked(reason) if mode.restores_conversation() => {
                    blocked = Some(reason);
                }
                FileRevert::Blocked(reason) => {
                    self.status_bar.flash(reason);
                    return Vec::new();
                }
            }
        }
        self.release_revert_confirmation();
        let previous = self.state.session.meta.pending_revert.clone();
        let pending = PendingConversationRevert {
            original_head: previous
                .as_ref()
                .map_or(conversation_source, |pending| pending.original_head),
            target_head: target.head,
            file_status: previous.and_then(|pending| pending.file_status),
        };
        self.state
            .session_mut()
            .set_conversation_state(target.head, Some(pending));
        let actions = self.finish_conversation_restore(conversation_source, target);
        if let Some(reason) = blocked {
            self.status_bar.flash(reason);
        }
        actions
    }

    fn is_reverting_blocked(&self) -> bool {
        self.status == Status::Streaming || self.awaiting_input() || self.cancelling_run.is_some()
    }

    pub fn unrevert(&mut self) -> Vec<Action> {
        if self.is_reverting_blocked() {
            self.status_bar.flash(REVERT_BUSY_MSG.into());
            return Vec::new();
        }
        let Some(pending) = self.state.session.meta.pending_revert.clone() else {
            return Vec::new();
        };
        self.release_revert_confirmation();
        if let Err(error) =
            active_history_items(self.state.session.messages(), pending.original_head)
        {
            self.status_bar
                .flash(format!("Failed to read session history: {error}"));
            return Vec::new();
        }
        if file_revert::files_pending(&pending) && !self.unrevert_files() {
            return Vec::new();
        }
        let current_head = crate::session_history_head(&self.state.session);
        self.state
            .session_mut()
            .set_conversation_state(pending.original_head, None);
        self.update_context_for_head(current_head);
        if let Err(error) = self.save_session_barrier() {
            self.status_bar
                .flash(format!("Failed to save the unreverted session: {error}"));
        }
        self.refresh_record_index();
        self.reset_ui_chrome();
        self.restore_display();
        self.input_box.discard();
        let loaded = self.install_local_history();
        self.checkpoint_now();
        vec![Action::LoadSession(Box::new(loaded))]
    }

    pub fn revert_at(&mut self, source: DisplaySource, mode: RestoreMode) -> Vec<Action> {
        self.revert_to(source_target_id(source), mode)
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

    pub(super) fn save_session_barrier(&mut self) -> Result<(), String> {
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

    pub fn fork_at(&self, source: DisplaySource) -> Result<ForkedSession, String> {
        if self.cancelling_run.is_some() {
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
            thinking: Some(self.state.thinking.clone().into()),
            fast: self.state.fast,
            unrecorded: self.state.session.meta.unrecorded.clone(),
            record_coverage: self.state.session.meta.record_coverage.clone(),
            ..SessionMeta::default()
        };
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
            caudra_agent::active_task_history_versions_with_outputs(&ancestor, |call_id| {
                self.state
                    .session
                    .tool_outputs()
                    .get(call_id)
                    .map(AsRef::as_ref)
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
            let Some(history) = self.state.session.subagent_messages().get(version_id) else {
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
            if version_id != task_id {
                child.set_subagent_history(
                    version_id.clone(),
                    history.to_vec(),
                    Some(StoredSubagentTaskSpec::version()),
                );
            }
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

        let (plan, warning) = match self.copy_plan(child.id) {
            Ok(plan) => (plan, None),
            Err(error) => (
                PlanState::None,
                Some(format!("{PLAN_COPY_FAILED}: {error}")),
            ),
        };
        child.meta.plan_path = plan.path().map(|path| path.to_string_lossy().into_owned());
        child.meta.plan_target = plan_target(&plan);
        child.meta.plan_written = plan.is_ready();

        self.hold_records_for(child.id);

        Ok(ForkedSession {
            session: child,
            lease,
            draft: target
                .draft
                .map(|(text, images)| ForkDraft { text, images }),
            warning,
        })
    }

    fn next_fork_title(&self) -> Result<String, String> {
        let (base, own_number) = split_fork_title(&self.state.session.title);
        let sessions = if self.workspace_session.is_some() {
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
        if self.permission_mutation_pending() {
            return Err(PERMISSION_WORKER_BUSY.into());
        }
        self.features
            .require_source(session.workspace_binding())
            .map_err(|disabled| disabled.to_string())?;
        if self.workspace_session.is_none() {
            caudra_storage::workspace_binding::StoredWorkspaceBinding::validate_resume(
                session.workspace_binding(),
                None,
            )
            .map_err(|error| error.to_string())?;
        }
        let remote_binding = self
            .workspace_session
            .as_ref()
            .map(|workspace| {
                let binding =
                    caudra_storage::workspace_binding::StoredWorkspaceBinding::new_with_cursor(
                        workspace.binding().clone(),
                        workspace.cursor().clone(),
                        session
                            .workspace_binding()
                            .and_then(|binding| binding.cursor_label().map(str::to_owned)),
                    )
                    .map_err(|error| format!("Remote workspace identity is invalid: {error}"))?;
                if session
                    .workspace_binding()
                    .is_none_or(|stored| !stored.exact_scope_eq(&binding))
                {
                    return Err("Session belongs to a different remote workspace cursor".to_owned());
                }
                Ok(binding)
            })
            .transpose()?;
        if let Some(binding) = remote_binding {
            session
                .replace_workspace_cursor(binding)
                .map_err(|error| error.to_string())?;
        }
        let permissions = Arc::new(self.permissions.fork_session(session.id));
        // A loaded session that holds nothing owns no row worth publishing
        // against, so it waits for its first run like a fresh one.
        let conversation_permissions = match self.conversation_permissions.deferred() {
            ConversationPermissions::Pending if session_has_content(&session) => {
                ConversationPermissions::Published(attach_session_permissions(
                    &self.storage,
                    &self.storage_writer,
                    &mut session,
                    &permissions,
                )?)
            }
            deferred => {
                permissions.load_structured_conversation_rules(
                    session.meta.structured_permission_rules.clone(),
                );
                deferred
            }
        };
        let automatic_wakes_suppressed = session.meta.automatic_wakes_suppressed
            || (session.id == self.state.session.id && self.automatic_wakes_suppressed);
        self.retire_current_session()
            .map_err(|error| format!("Failed to retire current session: {error}"))?;
        self.suspend_permission_editor();
        self.permissions = permissions;
        self.conversation_permissions = conversation_permissions;
        self.apply_stored_permission_mode(&session.meta);
        self.state =
            SessionState::from_session(session, fallback_model, &self.storage, &self.model_policy);
        let coverage = self.state.session.meta.record_coverage.clone();
        let first = self.bind_change_recorder();
        let covered_anew = self.state.session.meta.record_coverage != coverage;
        self.reconcile_loaded_session(first, covered_anew);
        self.refresh_record_index();
        self.reconcile_plan_target();
        for w in self.state.warnings.drain(..) {
            self.status_bar.flash(w);
        }
        self.reset_ui_chrome();
        self.automatic_wakes_suppressed = automatic_wakes_suppressed;
        self.restore_display();

        self.request_pattern_suggestions();
        Ok(self.install_local_history())
    }

    fn retire_current_session(&mut self) -> Result<(), caudra_storage::sessions::SessionError> {
        self.release_revert_confirmation();
        self.close_stream_modal();
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

pub(super) fn source_target_id(source: DisplaySource) -> CaudraId {
    match source {
        DisplaySource::User(id)
        | DisplaySource::AssistantText(id)
        | DisplaySource::Reasoning(id)
        | DisplaySource::ToolResult(id) => id,
        DisplaySource::ToolCall { id, result_id } => result_id.unwrap_or(id),
    }
}

/// Renders each restored subagent's transcript on its own core. The work is
/// per-chat, independent, and reads nothing but shared state, and a long-lived
/// session carries enough subagents that doing it serially is the largest
/// remaining cost of a resume.
fn render_subagent_displays(
    restorable: &[(StoredSubagent, Arc<Vec<HistoryItem>>)],
    tool_outputs: &HashMap<String, Arc<caudra_agent::ToolOutput>>,
    tool_output_lines: &caudra_config::ToolOutputLines,
    show_reminders: bool,
) -> Vec<(Vec<DisplayMessage>, Vec<caudra_lua::RestoreItem>)> {
    let render = |messages: &Arc<Vec<HistoryItem>>| {
        history_to_display(messages, tool_outputs, tool_output_lines, show_reminders)
    };
    let workers = restore_workers(restorable.len());
    if workers == 1 {
        return restorable.iter().map(|(_, items)| render(items)).collect();
    }
    let chunk = restorable.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let handles: Vec<_> = restorable
            .chunks(chunk)
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|(_, items)| render(items))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
            })
            .collect::<Vec<_>>()
    })
}

fn restore_workers(chats: usize) -> usize {
    if chats < PARALLEL_RESTORE_MIN_CHATS {
        return 1;
    }
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .min(chats.div_ceil(PARALLEL_RESTORE_MIN_CHATS))
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
    reachable.extend(
        caudra_agent::active_task_history_versions_with_outputs(items, |call_id| {
            tool_outputs.get(call_id).map(AsRef::as_ref)
        })
        .into_keys(),
    );
    let mut active_calls = caudra_agent::history_tool_call_ids(items);
    // A worklist rather than a rescan: choosing the next id by walking
    // `reachable` for an unvisited one costs a pass over the whole set per
    // step, so a transcript with thousands of tool calls spends most of a
    // resume here. An id joins `pending` exactly when it joins `reachable`,
    // which visits each one once and needs no visited set.
    let mut pending: Vec<String> = reachable.iter().cloned().collect();
    loop {
        for subagent in subagents {
            let linked =
                subagent.parent_tool_use_id.as_ref().is_some_and(|parent| {
                    reachable.contains(parent) || active_calls.contains(parent)
                }) || subagent
                    .root_tool_use_id
                    .as_ref()
                    .is_some_and(|root| active_calls.contains(root));
            if linked && reachable.insert(subagent.tool_use_id.clone()) {
                pending.push(subagent.tool_use_id.clone());
            }
        }
        let Some(task_id) = pending.pop() else {
            break;
        };
        if let Some(history) = histories.get(&task_id) {
            let mut discovered = tool_call_ids(history);
            discovered.extend(
                caudra_agent::active_task_history_versions_with_outputs(history, |call_id| {
                    tool_outputs.get(call_id).map(AsRef::as_ref)
                })
                .into_keys(),
            );
            for id in discovered {
                if reachable.insert(id.clone()) {
                    pending.push(id);
                }
            }
            active_calls.extend(caudra_agent::history_tool_call_ids(history));
        }
    }
    reachable
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
            HistoryItemKind::User {
                retained_output_refs,
                ..
            }
            | HistoryItemKind::AssistantText {
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
    if ids.insert(output_ref.id.clone()) {
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

pub(super) fn resolve_revert_target(
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
                boundary: Some(first.id),
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
            Ok(RevertTarget::kept(match result {
                Some(result) => complete_tool_result_group_head(items, result.id)?,
                None => complete_group_head(items, selected.id)?,
            }))
        }
        HistoryItemKind::AssistantText { .. } | HistoryItemKind::Reasoning { .. } => {
            Ok(RevertTarget::kept(selected.id))
        }
        HistoryItemKind::ToolResult { .. } => Ok(RevertTarget::kept(
            complete_tool_result_group_head(items, selected.id)?,
        )),
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

fn is_sanitizer_only_unavailable_extension(
    original: &[HistoryItem],
    candidate: &[HistoryItem],
) -> bool {
    let Some(last) = original.last() else {
        return false;
    };
    if candidate.len() <= original.len() {
        return false;
    }

    let call_group_id = last.group_id;
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
        // Last because it deep-compares the whole chain, and the additions
        // rule out nearly every append on their own.
        && candidate.starts_with(original)
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

/// The picker merges two sources: sessions this process has open (which the
/// event loop publishes, and only it can see) and everything else on disk.
/// Live rows win, because a session mid-turn has state the store has not
/// caught up with yet.
impl App {
    pub(super) fn sessions_browse(&mut self) -> Vec<Action> {
        self.move_back_from_removed_worktrees();
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
            SessionPickerAction::Copy(text) => {
                self.copy_to_clipboard(&text);
                Vec::new()
            }
            SessionPickerAction::Focus(id) => vec![Action::FocusSession(id)],
            SessionPickerAction::FocusElsewhere { id, cwd } => {
                vec![Action::OpenSessionElsewhere { id, cwd }]
            }
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
            SessionRelocationAction::Copy(text) => {
                self.copy_to_clipboard(&text);
                Vec::new()
            }
            SessionRelocationAction::Confirm(request, donor) => {
                vec![Action::RelocateSessions { request, donor }]
            }
        }
    }

    pub(crate) fn open_worktrees(&mut self, overview: WorktreeOverview, view: WorktreeView) {
        self.worktree_picker.open(overview, view);
    }

    pub(crate) fn show_worktree_removal(&mut self, root: PathBuf, dirty: bool) {
        self.worktree_picker.show_removal(root, dirty);
    }

    pub(super) fn handle_worktree_action(&mut self, action: WorktreeAction) -> Vec<Action> {
        match action {
            WorktreeAction::Consumed | WorktreeAction::Closed => Vec::new(),
            WorktreeAction::Copy(text) => {
                self.copy_to_clipboard(&text);
                Vec::new()
            }
            WorktreeAction::Refresh => vec![Action::OpenWorktrees(WorktreeView::List)],
            WorktreeAction::Open(root) => vec![Action::OpenWorktree(root)],
            WorktreeAction::InspectRemoval(root) => vec![Action::InspectWorktreeRemoval(root)],
            WorktreeAction::Run(request) => vec![Action::RunWorktree(request)],
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

    /// Catches a worktree removed while Caudra runs, so its sessions are
    /// listed where they now live.
    pub(crate) fn move_back_from_removed_worktrees(&mut self) {
        if self.workspace_session.is_some() {
            return;
        }
        match worktrees::reconcile(&self.storage, Path::new(&self.state.session.cwd)) {
            Ok(moved) if moved.is_empty() => {}
            Ok(moved) => self.flash(
                moved
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; "),
            ),
            Err(error) => {
                tracing::warn!(%error, "failed to move sessions back from removed worktrees");
            }
        }
    }

    fn session_rows(&self) -> Vec<SessionRow> {
        let live = self.live_sessions.load();
        let seen: HashSet<CaudraId> = live.iter().map(|row| row.id).collect();
        let stored = if self.workspace_session.is_some() {
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
        let elsewhere = if self.workspace_session.is_some() {
            Vec::new()
        } else {
            self.other_checkout_rows(&seen)
        };
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
                        checkout: None,
                    }),
            )
            .chain(elsewhere)
            .collect()
    }

    fn other_checkout_rows(&self, seen: &HashSet<CaudraId>) -> Vec<SessionRow> {
        worktrees::sibling_sessions(&self.storage, Path::new(&self.state.session.cwd))
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "failed to list sessions of other checkouts");
                Vec::new()
            })
            .into_iter()
            .flat_map(
                |CheckoutSessions {
                     root,
                     branch,
                     sessions,
                     ..
                 }| {
                    sessions.into_iter().map(move |session| SessionRow {
                        id: session.id,
                        title: session.title,
                        updated_at: session.updated_at,
                        activity: None,
                        focused: false,
                        checkout: Some(OtherCheckout {
                            root: root.clone(),
                            branch: branch.clone(),
                            cwd: PathBuf::from(session.cwd),
                        }),
                    })
                },
            )
            .filter(|row| !seen.contains(&row.id))
            .collect()
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::{appended_len, collect_tool_output_refs};
    use caudra_agent::HistorySnapshot;
    use caudra_providers::{ContentBlock, HistoryItem, Message, Role, expand_message};
    use caudra_storage::tool_outputs::ToolOutputRef;
    use std::collections::HashSet;
    use std::sync::Arc;
    use test_case::test_case;

    const OUTPUT_ID: &str = "calm-blue-wren";
    const PROSE_ONLY_ID: &str = "bright-small-heron";
    const OUTCOME: &str = "Complete retained task outcome";
    const MERGED_LEN: usize = 2;
    const APPENDED_LEN_MSG: &str =
        "only a same-run extension of the merged ids may skip the full merge";

    /// `turns` more prompts chained onto `items`, the way a producer appends.
    fn with_turns(items: &[HistoryItem], turns: usize) -> Vec<HistoryItem> {
        let mut items = items.to_vec();
        for turn in 0..turns {
            let parent = items.last().map(|item| item.id);
            items.extend(expand_message(&Message::user(turn.to_string()), parent));
        }
        items
    }

    fn same_run(merged: &HistorySnapshot, messages: Vec<HistoryItem>) -> HistorySnapshot {
        HistorySnapshot {
            epoch: merged.epoch,
            messages: Arc::new(messages),
        }
    }

    #[test_case(MERGED_LEN, |merged| same_run(merged, with_turns(&merged.messages, 1)), Some(MERGED_LEN) ; "a_pure_append")]
    #[test_case(MERGED_LEN, |merged| HistorySnapshot::new(with_turns(&merged.messages, 1)), None ; "another_run")]
    #[test_case(MERGED_LEN, |merged| same_run(merged, merged.messages[..1].to_vec()), None ; "a_shorter_run")]
    #[test_case(MERGED_LEN, |merged| same_run(merged, merged.messages.to_vec()), None ; "the_same_length")]
    #[test_case(0, |merged| same_run(merged, with_turns(&[], 1)), None ; "nothing_merged_yet")]
    #[test_case(MERGED_LEN, |merged| same_run(merged, with_turns(&merged.messages[..1], 2)), None ; "a_replaced_boundary")]
    fn only_an_append_to_the_merged_run_skips_the_full_merge(
        merged_len: usize,
        candidate: fn(&HistorySnapshot) -> HistorySnapshot,
        expected: Option<usize>,
    ) {
        let merged = HistorySnapshot::new(with_turns(&[], merged_len));
        assert_eq!(
            appended_len(&merged, &candidate(&merged)),
            expected,
            "{APPENDED_LEN_MSG}"
        );
    }

    #[test_case(1; "single_observation")]
    #[test_case(3; "repeated_observations_and_refs")]
    fn retained_observation_refs_are_counted_once_across_history_kinds(copies: usize) {
        let reference = ToolOutputRef {
            id: OUTPUT_ID.parse().unwrap(),
            byte_count: OUTCOME.len(),
            line_count: 1,
        };
        let mut observation = Message::observation(PROSE_ONLY_ID.into());
        observation.retained_output_refs = vec![reference.clone(); copies];
        let mut messages = vec![observation; copies];
        messages.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: OUTCOME.into(),
            }],
            retained_output_refs: vec![reference.clone()],
            ..Default::default()
        });
        let items = crate::history_items(&messages);
        let mut ids = HashSet::new();
        let mut references = Vec::new();
        collect_tool_output_refs(&items, &mut ids, &mut references);
        assert_eq!(
            references
                .iter()
                .map(|reference| reference.byte_count)
                .sum::<usize>(),
            OUTCOME.len()
        );
        assert_eq!(ids.len(), 1);
        assert_eq!(references, [reference]);
    }
}
