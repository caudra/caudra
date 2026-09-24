//! Task chats that open before the call that fills them runs.
//!
//! A `task` call spends its whole stream writing the brief for a subagent that
//! does not exist yet, so the transcript used to show `Delegate … (queued)`
//! and nothing behind it until the call ran. The agent decodes that brief as
//! it arrives; this opens a chat for it straight away, keyed on the id the
//! subagent is predicted to publish under, and grows the instruction inside it.
//!
//! The chat is a prediction and is treated as one. It stays out of
//! `chat_index`, so nothing routes a subagent's events to it, and out of the
//! session snapshot, so a call that never runs leaves nothing behind.
//! [`super::App::resolve_or_create_chat`] adopts it by `parent_tool_use_id`,
//! which is exact even when the reserved `task_id` turns out to differ, and
//! everything still unadopted when the call ends is discarded.

use std::path::Path;

use caudra_agent::Delegation;

use crate::app::App;
use crate::chat::Chat;

/// What a task's chat is called before its description has arrived.
const UNNAMED_TASK: &str = "Task";

impl App {
    /// One decoded fragment of a delegating call. Only the main chat can
    /// delegate, so a subagent's own stream never reaches here.
    pub(super) fn delegation_delta(&mut self, delegation: Delegation) {
        let Delegation {
            parent_tool_use_id,
            name,
            prompt,
            task_id,
        } = delegation;
        // A continuation already has a chat with a transcript in it. Its
        // instruction lands there whole when the call runs, so the prediction
        // opened for it is dropped rather than shown twice.
        if let Some(task_id) = task_id
            && self.chat_for_task(&task_id).is_some()
        {
            self.discard_pending_delegation(&parent_tool_use_id);
            return;
        }
        let idx = self.pending_delegation_chat(&parent_tool_use_id, name.as_deref());
        if let Some(name) = name {
            self.chats[idx].name = name;
        }
        if let Some(prompt) = prompt {
            self.chats[idx].prompt_delta(&prompt);
        }
    }

    /// The chat for a delegation still being written, opening one on the first
    /// fragment that has something to put in it.
    fn pending_delegation_chat(&mut self, parent_tool_use_id: &str, name: Option<&str>) -> usize {
        if let Some(idx) = self.chat_for_parent(parent_tool_use_id) {
            return idx;
        }
        let mut chat = Chat::subagent(
            parent_tool_use_id,
            name.unwrap_or(UNNAMED_TASK).to_owned(),
            Path::new(&self.state.session.cwd),
            self.ui_config.clone(),
            self.lua_event_handle.clone(),
        );
        chat.set_parent_tool_use_id(parent_tool_use_id.to_owned());
        chat.set_restore_channel(self.restore_event_tx.clone());
        chat.set_view(self.view);
        self.chats.push(chat);
        self.pending_delegations
            .insert(parent_tool_use_id.to_owned());
        self.chats.len() - 1
    }

    /// Hands a predicted chat over to the subagent that really opened. `Some`
    /// means the instruction is already on screen and the caller must not push
    /// `SubagentInfo`'s own copy of it.
    pub(super) fn adopt_pending_delegation(
        &mut self,
        parent_tool_use_id: &str,
        task_id: &str,
    ) -> Option<usize> {
        if !self.pending_delegations.remove(parent_tool_use_id) {
            return None;
        }
        let idx = self.chat_for_parent(parent_tool_use_id)?;
        // The real id names another chat only when a continuation raced its
        // own prediction. That chat is the one with the history, so the
        // prediction goes rather than shadowing it.
        if self
            .chat_for_task(task_id)
            .is_some_and(|existing| existing != idx)
        {
            self.remove_chat(idx);
            return None;
        }
        self.chats[idx].flush();
        self.chats[idx].set_task_id(task_id.to_owned());
        Some(idx)
    }

    /// Drops a chat opened for a call that never ran: a stream the provider
    /// reset, a turn the user cancelled, arguments that failed to parse, or a
    /// child `batch` refused.
    pub(super) fn discard_pending_delegation(&mut self, parent_tool_use_id: &str) {
        if !self.pending_delegations.remove(parent_tool_use_id) {
            return;
        }
        if let Some(idx) = self.chat_for_parent(parent_tool_use_id) {
            self.remove_chat(idx);
        }
    }

    /// Every prediction made under `tool_use_id`, the call itself and the
    /// `batch` children whose ids extend it.
    pub(super) fn discard_pending_delegations_under(&mut self, tool_use_id: &str) {
        let prefix = format!("{tool_use_id}:");
        let stale: Vec<String> = self
            .pending_delegations
            .iter()
            .filter(|id| id.as_str() == tool_use_id || id.starts_with(&prefix))
            .cloned()
            .collect();
        for id in stale {
            self.discard_pending_delegation(&id);
        }
    }

    /// A reset attempt takes its predictions with it: the text it wrote is
    /// dropped from the view, and the retry writes fresh calls under fresh
    /// ids. Only the main chat delegates, so a subagent's reset spares them.
    pub(super) fn discard_stream_delegations(&mut self, chat_idx: usize) {
        if chat_idx == 0 {
            self.discard_all_pending_delegations();
        }
    }

    pub(super) fn discard_all_pending_delegations(&mut self) {
        for id in std::mem::take(&mut self.pending_delegations) {
            if let Some(idx) = self.chat_for_parent(&id) {
                self.remove_chat(idx);
            }
        }
    }

    fn chat_for_parent(&self, parent_tool_use_id: &str) -> Option<usize> {
        self.chats.iter().position(|chat| {
            chat.parent_tool_use_id()
                .is_some_and(|id| &**id == parent_tool_use_id)
        })
    }

    fn chat_for_task(&self, task_id: &str) -> Option<usize> {
        self.chats
            .iter()
            .position(|chat| chat.task_id().is_some_and(|id| &**id == task_id))
    }

    /// The only place a chat leaves the list mid-session. Every index the app
    /// holds is a position in it, so they all move together or one of them
    /// starts pointing at the wrong transcript.
    fn remove_chat(&mut self, idx: usize) {
        if idx == 0 || idx >= self.chats.len() {
            return;
        }
        let removed = self.chats.remove(idx);
        if let Some(task_id) = removed.task_id() {
            self.subagent_drafts.remove(&**task_id);
            if self.subagent_input_task.as_deref() == Some(&**task_id) {
                self.subagent_input_task = None;
                self.subagent_input_box.set_draft(Default::default());
            }
        }
        self.chat_index.retain(|_, at| *at != idx);
        for at in self.chat_index.values_mut() {
            *at -= usize::from(*at > idx);
        }
        if self.active_chat >= idx {
            self.active_chat = self.active_chat.saturating_sub(1);
        }
    }
}
