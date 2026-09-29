use std::path::Path;

use caudra_agent::{
    TaskCard,
    background::{BackgroundTasks, TaskHistoryPage},
};
use caudra_providers::Message;
use caudra_storage::{
    background::{BackgroundCursor, JobKind, MAX_HISTORY_PAGE},
    id::CaudraId,
};

use crate::chat::{Chat, history_to_display};
use crate::components::DisplayMessage;
use crate::repaint::Dirty;

use super::App;
use super::tasks::{MAIN_TASK_ID, TaskOutcome};

type HistoryLoad<T> = smol::Task<Result<T, String>>;

struct ArchivedChat {
    card: TaskCard,
    messages: Vec<DisplayMessage>,
    restore: Vec<caudra_lua::RestoreItem>,
}

#[derive(Default)]
pub(super) struct TaskHistory {
    owner: Option<(CaudraId, u64)>,
    cards: Vec<TaskCard>,
    next: Option<BackgroundCursor>,
    page: Option<HistoryLoad<TaskHistoryPage>>,
    detail: Option<TaskCard>,
    detail_request: Option<(String, HistoryLoad<TaskCard>)>,
    detail_requested: Option<String>,
    lookup: Option<(Option<String>, HistoryLoad<TaskCard>)>,
    transcript: Option<(String, String, HistoryLoad<ArchivedChat>)>,
    cached_chat: Option<String>,
}

impl TaskHistory {
    pub(super) fn cancel_transcript(&mut self) {
        self.transcript = None;
    }

    fn bind(&mut self, runtime: &BackgroundTasks) {
        let owner = (runtime.session_id(), runtime.generation());
        if self.owner != Some(owner) {
            let cached_chat = self
                .owner
                .filter(|(session, _)| *session == owner.0)
                .and_then(|_| self.cached_chat.take());
            *self = Self {
                owner: Some(owner),
                cached_chat,
                ..Default::default()
            };
        }
    }

    pub(super) fn cards(&self, runtime: &BackgroundTasks) -> Vec<TaskCard> {
        let mut cards = runtime.list();
        if self.owner != Some((runtime.session_id(), runtime.generation())) {
            return cards;
        }
        for card in &self.cards {
            if !cards.iter().any(|current| current.task_id == card.task_id) {
                cards.push(card.clone());
            }
        }
        if let Some(detail) = &self.detail {
            if let Some(card) = cards.iter_mut().find(|card| card.task_id == detail.task_id) {
                if card.invocation_id == detail.invocation_id
                    && card.state == detail.state
                    && card.updated_at <= detail.updated_at
                {
                    *card = detail.clone();
                }
            } else {
                cards.push(detail.clone());
            }
        }
        cards
    }
}

impl App {
    pub(super) fn load_archived_chat(&mut self, id: &str) -> bool {
        let Some(runtime) = self.background.clone() else {
            return false;
        };
        let Some(card) = self
            .task_history_cards()
            .into_iter()
            .find(|card| card.task_id == id && card.kind == JobKind::Agent && !card.active())
        else {
            return false;
        };
        self.task_history.bind(&runtime);
        let focus = self.chats[self.active_chat]
            .task_id()
            .map_or(MAIN_TASK_ID, |id| id.as_ref())
            .to_owned();
        let outputs = self.state.session.tool_outputs().clone();
        let lines = self.ui_config.tool_output_lines;
        let reminders = self.ui_config.show_reminders;
        let invocation = card.invocation_id.clone();
        self.task_history.transcript = Some((
            id.to_owned(),
            focus,
            smol::spawn(async move {
                let record = runtime.record_invocation(&invocation).await?;
                smol::unblock(move || {
                    let history: Vec<Message> = if record.history.is_null() {
                        Vec::new()
                    } else {
                        serde_json::from_value(record.history).map_err(|error| error.to_string())?
                    };
                    let (messages, restore) = history_to_display(
                        &crate::history_items(&history),
                        &outputs,
                        &lines,
                        reminders,
                    );
                    Ok(ArchivedChat {
                        card,
                        messages,
                        restore,
                    })
                })
                .await
            }),
        ));
        true
    }

    pub(super) fn load_task_status(&mut self, id: &str) {
        let Some(runtime) = self.background.clone() else {
            return;
        };
        self.task_history.bind(&runtime);
        let id = id.to_owned();
        self.task_history.lookup = Some((
            self.task_picker.selected_id(),
            smol::spawn(async move { runtime.status_async(&id).await }),
        ));
    }

    pub(super) fn task_history_cards(&self) -> Vec<TaskCard> {
        self.background
            .as_ref()
            .map(|runtime| self.task_history.cards(runtime))
            .unwrap_or_default()
    }

    pub(super) fn load_task_history(&mut self, older: bool) {
        let Some(runtime) = self.background.clone() else {
            return;
        };
        self.task_history.bind(&runtime);
        if self.task_history.page.is_some() {
            return;
        }
        let before = if older {
            let Some(cursor) = self.task_history.next.clone() else {
                return;
            };
            Some(cursor)
        } else {
            None
        };
        self.task_history.page = Some(smol::spawn(async move {
            runtime.history_page(before, MAX_HISTORY_PAGE).await
        }));
    }

    pub(super) fn poll_task_history(&mut self) -> Dirty {
        let Some(runtime) = self.background.clone() else {
            return Dirty::NO;
        };
        self.task_history.bind(&runtime);
        let mut dirty = Dirty::NO;
        if self
            .task_history
            .transcript
            .as_ref()
            .is_some_and(|(_, _, task)| task.is_finished())
        {
            let Some((id, focus, task)) = self.task_history.transcript.take() else {
                return dirty;
            };
            let result = smol::block_on(task);
            let current_focus = self.chats[self.active_chat]
                .task_id()
                .map_or(MAIN_TASK_ID, |id| id.as_ref());
            if current_focus == focus
                && (!self.task_picker.is_open()
                    || self.task_picker.selected_id().as_deref() == Some(&id))
            {
                match result {
                    Ok(loaded)
                        if !runtime.resident_status(&id).is_some_and(|card| {
                            card.active() || card.invocation_id != loaded.card.invocation_id
                        }) =>
                    {
                        let mut chat = Chat::subagent(
                            &id,
                            loaded.card.label.clone(),
                            Path::new(&self.state.session.cwd),
                            self.ui_config.clone(),
                            self.lua_event_handle.clone(),
                        );
                        chat.set_parent_tool_use_id(loaded.card.call_id.clone());
                        chat.set_restore_channel(self.restore_event_tx.clone());
                        chat.load_messages(loaded.messages);
                        let outcome = match loaded.card.state.as_str() {
                            "succeeded" => TaskOutcome::Done,
                            "cancelled" => TaskOutcome::Killed,
                            _ => TaskOutcome::Error,
                        };
                        chat.mark_finished(outcome, &loaded.card.state);
                        self.leave_active_chat();
                        let slot = self
                            .chats
                            .iter()
                            .position(|chat| chat.task_id().is_some_and(|task| task.as_ref() == id))
                            .or_else(|| {
                                self.task_history.cached_chat.as_ref().and_then(|cached| {
                                    self.chats.iter().position(|chat| {
                                        chat.task_id().is_some_and(|task| task.as_ref() == cached)
                                            && chat.is_finished()
                                            && runtime.resident_status(cached).is_none()
                                    })
                                })
                            });
                        self.active_chat = if let Some(slot) = slot {
                            if let Some(previous) = self.chats[slot].task_id() {
                                self.chat_index.remove(previous.as_ref());
                            }
                            self.chats[slot] = chat;
                            slot
                        } else {
                            self.chats.push(chat);
                            self.chats.len() - 1
                        };
                        self.task_history.cached_chat = Some(id);
                        self.task_history.detail = Some(loaded.card);
                        self.fire_restore_items(loaded.restore);
                    }
                    Ok(_) => {}
                    Err(error) => self.flash(error),
                }
                dirty = Dirty::YES;
            }
        }
        if self
            .task_history
            .lookup
            .as_ref()
            .is_some_and(|(_, task)| task.is_finished())
        {
            let Some((selection, task)) = self.task_history.lookup.take() else {
                return dirty;
            };
            let result = smol::block_on(task);
            if self.task_picker.is_open() && self.task_picker.selected_id() == selection {
                match result {
                    Ok(card) => {
                        let id = card.task_id.clone();
                        let shell = card.kind == JobKind::Shell;
                        self.task_history.detail = Some(card);
                        if shell {
                            self.show_shell(&id);
                        } else {
                            dirty |= self.refresh_task_picker();
                            self.task_picker.select(&id);
                        }
                    }
                    Err(error) => self.flash(error),
                }
                dirty = Dirty::YES;
            }
        }
        if self
            .task_history
            .page
            .as_ref()
            .is_some_and(smol::Task::is_finished)
        {
            let Some(page) = self.task_history.page.take() else {
                return dirty;
            };
            match smol::block_on(page) {
                Ok(page) => {
                    self.task_history.cards = page.tasks;
                    self.task_history.next = page.next;
                }
                Err(error) => self.flash(error),
            }
            dirty = Dirty::YES;
        }
        let selected = if self.task_picker.is_open() {
            self.task_picker.selected_id()
        } else if self.shell_modal.is_open() {
            self.shell_modal.selected_id()
        } else {
            None
        };
        if selected != self.task_history.detail_requested {
            self.task_history.detail_requested = None;
        }
        if self
            .task_history
            .detail_request
            .as_ref()
            .is_some_and(|(_, task)| task.is_finished())
        {
            let Some((id, task)) = self.task_history.detail_request.take() else {
                return dirty;
            };
            let result = smol::block_on(task);
            if selected.as_ref() == Some(&id) {
                match result {
                    Ok(card) => self.task_history.detail = Some(card),
                    Err(error) => self.flash(error),
                }
                dirty = Dirty::YES;
            }
        }
        if let Some(id) = selected {
            let card = self
                .task_history_cards()
                .into_iter()
                .find(|card| card.task_id == id);
            if let Some(card) = card {
                if let Some(detail) = runtime.resident_invocation_status(&id, &card.invocation_id) {
                    self.task_history.detail = Some(detail);
                    return dirty | self.refresh_task_picker();
                }
                let loaded = self.task_history.detail.as_ref().is_some_and(|detail| {
                    detail.task_id == id
                        && detail.invocation_id == card.invocation_id
                        && detail.state == card.state
                        && detail.updated_at >= card.updated_at
                });
                let loading = self
                    .task_history
                    .detail_request
                    .as_ref()
                    .is_some_and(|(pending, _)| pending == &id);
                if !loaded && !loading && self.task_history.detail_requested.as_ref() != Some(&id) {
                    let requested = id.clone();
                    self.task_history.detail_requested = Some(id.clone());
                    self.task_history.detail_request = Some((
                        id,
                        smol::spawn(async move { runtime.status_async(&requested).await }),
                    ));
                }
            }
        }
        dirty | self.refresh_task_picker()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use caudra_agent::background::BackgroundTasks;
    use caudra_providers::Message;
    use caudra_storage::{
        background::{MAX_HISTORY_PAGE, TaskRecord},
        sessions::SessionDatabase,
    };
    use futures_lite::future::yield_now;
    use serde_json::json;
    use test_case::test_case;

    use crate::app::{App, tests::test_app};

    const HISTORY_TEXT: &str = "Inspect the archived implementation.";
    const TASK_PREFIX: &str = "archived-task";
    const SUCCEEDED: &str = "succeeded";

    async fn fixture() -> App {
        let mut app = test_app();
        app.storage_writer
            .save_sync(Arc::clone(&app.state.session))
            .unwrap();
        let database = SessionDatabase::open(&app.storage).unwrap();
        for index in 0..=MAX_HISTORY_PAGE {
            let id = format!("{TASK_PREFIX}-{index}");
            let record = TaskRecord {
                payload: Default::default(),
                owner: Default::default(),
                created_at: 1,
                updated_at: 1,
                sequence: index as u64 + 1,
                task_id: id.clone(),
                invocation_id: id.clone(),
                root_call_id: id.clone(),
                generation: 1,
                state: SUCCEEDED.into(),
                background: false,
                receipt_accepted: true,
                mode: "build".into(),
                request: json!({"call_id": id, "label": id}),
                outcome: Some(json!({"success": true})),
                output_ref: None,
                history: json!([Message::user(HISTORY_TEXT.into())]),
                spec: json!({}),
                events: Vec::new(),
            };
            database
                .save_background_task(app.state.session.id, &record)
                .unwrap();
        }
        app.background = Some(
            BackgroundTasks::spawn(app.storage.clone(), app.state.session.id)
                .await
                .unwrap(),
        );
        app
    }

    async fn flush(app: &mut App) {
        while app.task_history.page.is_some()
            || app.task_history.lookup.is_some()
            || app.task_history.detail_request.is_some()
            || app.task_history.transcript.is_some()
        {
            let _ = app.poll_task_history();
            yield_now().await;
        }
    }

    #[test_case(false; "open_and_reuse_one_cold_chat")]
    #[test_case(true; "late_preview_is_discarded")]
    fn archive_pages_and_chat_loading_are_bounded_and_selection_fenced(cancel: bool) {
        smol::block_on(async {
            let mut app = fixture().await;
            let oldest = format!("{TASK_PREFIX}-0");
            app.tasks_browse();
            flush(&mut app).await;
            assert_eq!(app.task_history.cards.len(), MAX_HISTORY_PAGE);
            assert_eq!(app.chats.len(), 1);
            assert!(
                !app.task_history_cards()
                    .iter()
                    .any(|card| card.task_id == oldest)
            );
            app.load_task_status(&oldest);
            flush(&mut app).await;
            assert_eq!(
                app.task_picker.selected_id().as_deref(),
                Some(oldest.as_str())
            );
            app.preview_task(&oldest);
            if cancel {
                app.preview_task(super::MAIN_TASK_ID);
            }
            flush(&mut app).await;
            if cancel {
                assert_eq!(app.chats.len(), 1);
                assert_eq!(app.active_chat, 0);
                return;
            }
            assert_eq!(app.chats.len(), 2);
            assert_eq!(
                app.chats[app.active_chat].message_at(0).unwrap().text,
                HISTORY_TEXT
            );
            app.load_task_history(true);
            flush(&mut app).await;
            assert_eq!(app.task_history.cards.len(), 1);
            assert!(app.task_history.next.is_none());
            let latest = format!("{TASK_PREFIX}-{MAX_HISTORY_PAGE}");
            app.task_picker.select(&latest);
            app.preview_task(&latest);
            flush(&mut app).await;
            assert_eq!(app.chats.len(), 2);
            assert_eq!(
                app.chats[app.active_chat].task_id().unwrap().as_ref(),
                latest
            );
            app.background.as_ref().unwrap().shutdown().await.unwrap();
        });
    }
}
