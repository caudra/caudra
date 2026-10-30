//! Moving the conversation on screen, the main one or a task's, to another
//! model or reasoning level. A running conversation takes the change at its
//! next request, and one waiting out a backoff or a spent quota resends at
//! once. A stopped task keeps the change for its next continuation.

use caudra_agent::SettingsChange;
use caudra_providers::{Model, ThinkingConfig};
use caudra_storage::thinking::StoredThinking;

use super::{App, THINKING_UNSUPPORTED_MSG};
use crate::agent::AgentCommand;
use crate::chat::Chat;
use crate::components::{Action, Status};

const TASK_GONE: &str = "That task is no longer in this session";
const NEXT_REQUEST: &str = "from its next request";
const NEXT_CONTINUATION: &str = "when it continues";
const NOW_ON: &str = "now on";

impl App {
    /// The session's model picker on the main chat, the task's own on a task.
    pub(super) fn open_model_picker_here(&mut self) -> Vec<Action> {
        self.open_model_picker_for(self.active_subagent_id().map(str::to_owned))
    }

    /// `None` is the session's picker.
    pub(super) fn open_model_picker_for(&mut self, task_id: Option<String>) -> Vec<Action> {
        let Some(task_id) = task_id else {
            self.model_picker.open(
                &self.state.model,
                &self.model_policy,
                self.active_decision_endpoint(),
            );
            return vec![Action::RefreshModels];
        };
        let Some(&idx) = self.chat_index.get(&task_id) else {
            self.flash(TASK_GONE.into());
            return Vec::new();
        };
        let chat = &self.chats[idx];
        let model = self.task_model(chat);
        let name = chat.name.clone();
        self.model_picker
            .open_task(&model, &self.model_policy, task_id, &name);
        vec![Action::RefreshModels]
    }

    /// The levels of the model the chat on screen runs on.
    pub(super) fn open_thinking_picker_here(&mut self) {
        let Some(task_id) = self.active_subagent_id().map(str::to_owned) else {
            if self.state.model.supports_thinking() {
                self.thinking_picker
                    .open(&self.state.model, &self.state.thinking);
            } else {
                self.flash(THINKING_UNSUPPORTED_MSG.into());
            }
            return;
        };
        let chat = &self.chats[self.active_chat];
        let model = self.task_model(chat);
        if !model.supports_thinking() {
            self.flash(THINKING_UNSUPPORTED_MSG.into());
            return;
        }
        let current = task_thinking(chat);
        self.thinking_picker.open_task(&model, &current, task_id);
    }

    /// `/thinking` on a task: a level for it, or the picker when none is given.
    pub(super) fn set_task_thinking(&mut self, task_id: String, input: &str) {
        let input = input.trim();
        if input.is_empty() {
            self.open_thinking_picker_here();
            return;
        }
        let Some(&idx) = self.chat_index.get(&task_id) else {
            self.flash(TASK_GONE.into());
            return;
        };
        let current = task_thinking(&self.chats[idx]);
        match ThinkingConfig::parse(input, &current) {
            Ok(thinking) => self.switch_task(&task_id, SettingsChange::thinking(thinking)),
            Err(error) => self.flash(error.to_owned()),
        }
    }

    /// The running task takes `change` at its next request; a stopped one
    /// keeps it for its next continuation.
    pub(crate) fn switch_task(&mut self, task_id: &str, change: SettingsChange) {
        let Some(&idx) = self.chat_index.get(task_id) else {
            self.flash(TASK_GONE.into());
            return;
        };
        let summary = self.describe(&self.chats[idx], &change);
        let running = !self.chats[idx].is_finished();
        if running && let Some(control) = self.subagent_controls.get(task_id) {
            control.live.request(change);
            self.flash(format!("{summary} {NEXT_REQUEST}"));
            return;
        }
        match self.retarget_stopped_task(idx, task_id, change) {
            Ok(()) => self.flash(format!("{summary} {NEXT_CONTINUATION}")),
            Err(error) => self.flash(error),
        }
    }

    fn retarget_stopped_task(
        &mut self,
        idx: usize,
        task_id: &str,
        change: SettingsChange,
    ) -> Result<(), String> {
        let store = self.subagent_history.as_ref().ok_or(TASK_GONE)?;
        let spec = store
            .retarget(
                task_id,
                change.model_spec(),
                change.thinking.clone().map(StoredThinking::from),
            )
            .map_err(|error| error.to_string())?;
        let messages = self
            .state
            .session
            .subagent_messages()
            .get(task_id)
            .map(|messages| messages.as_ref().clone())
            .unwrap_or_default();
        self.state
            .session_mut()
            .set_subagent_history(task_id.to_owned(), messages, Some(spec));
        let model = match change.route {
            Some((_, model)) => model,
            None => self.task_model(&self.chats[idx]),
        };
        let chat = &mut self.chats[idx];
        let thinking = change.thinking.unwrap_or_else(|| task_thinking(chat));
        chat.model_id = Some(model.spec());
        chat.thinking = thinking.label_for(&model);
        self.sync_subagents();
        Ok(())
    }

    /// Forwards a model the user picked to the main run, unless the run is
    /// on a route the pick does not replace, such as a Plan job's model, or
    /// the pick rides on a mode toggle that waits for the run to settle.
    pub(crate) fn forward_main_model(&self, replaced: &str, change: SettingsChange) {
        let on_replaced = self
            .effective_model_slot
            .as_ref()
            .is_some_and(|slot| slot.load().model.spec() == replaced);
        if on_replaced && self.state.applied_mode == self.state.mode {
            self.forward_main(change);
        }
    }

    pub(super) fn forward_main(&self, change: SettingsChange) {
        if self.status == Status::Streaming
            && let Some(control) = &self.main_control
        {
            control.live.request(change);
        }
    }

    /// The model a pick moved to, or the level, is what requests carry now.
    pub(super) fn settings_changed(
        &mut self,
        chat_idx: usize,
        main_agent: bool,
        model: String,
        thinking: Option<String>,
    ) -> Vec<Action> {
        let settled = describe_settled(&model, thinking.as_deref());
        if main_agent {
            self.flash(format!("{NOW_ON} {settled}"));
            return vec![Action::AdoptLiveSwitch(model)];
        }
        if let Some(parent) = self.chats[chat_idx].parent_tool_use_id().cloned() {
            self.chats[0].set_tool_model(&parent, &model, thinking.as_deref());
        }
        let chat = &mut self.chats[chat_idx];
        let name = chat.name.clone();
        chat.model_id = Some(model);
        chat.thinking = thinking;
        self.sync_subagents();
        self.flash(format!("{name} {NOW_ON} {settled}"));
        Vec::new()
    }

    /// Cuts the backoff of the chat on screen short.
    pub(super) fn retry_here(&mut self) {
        self.active_chat().clear_retry();
        self.retry_chat(self.active_subagent_id().map(str::to_owned));
    }

    pub(super) fn retry_chat(&self, task_id: Option<String>) {
        match task_id {
            Some(task_id) => {
                if let Some(control) = self.subagent_controls.get(&task_id) {
                    control.retry_now.notify();
                }
            }
            None => {
                if let Some(cmd_tx) = &self.cmd_tx {
                    let _ = cmd_tx.try_send(AgentCommand::RetryNow);
                }
            }
        }
    }

    /// The model a task chat names, else the session's.
    fn task_model(&self, chat: &Chat) -> Model {
        chat.model_id
            .as_deref()
            .and_then(|spec| Model::from_spec(spec).ok())
            .unwrap_or_else(|| self.state.model.clone())
    }

    fn describe(&self, chat: &Chat, change: &SettingsChange) -> String {
        let model = change
            .route
            .as_ref()
            .map_or_else(|| self.task_model(chat), |(_, model)| model.clone());
        let level = change
            .thinking
            .as_ref()
            .and_then(|thinking| thinking.label_for(&model));
        let target = match &change.route {
            Some(_) => describe_settled(&model.spec(), level.as_deref()),
            None => level.unwrap_or_default(),
        };
        format!("{}: {target}", chat.name)
    }
}

/// The level a task chat names, `off` when it names none.
fn task_thinking(chat: &Chat) -> ThinkingConfig {
    chat.thinking
        .as_deref()
        .and_then(|label| ThinkingConfig::parse(label, &ThinkingConfig::Off).ok())
        .unwrap_or(ThinkingConfig::Off)
}

fn describe_settled(model: &str, thinking: Option<&str>) -> String {
    match thinking {
        Some(level) => format!("{model} · {level}"),
        None => model.to_owned(),
    }
}
