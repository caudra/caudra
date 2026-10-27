//! `/learn`: the inspector's side of the conversation with the learning
//! store. Like `/memory`, every read and change runs on a task of its own and
//! lands on a later tick, so the loop never waits on the database or on git.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

use caudra_agent::AgentEvent;
use caudra_agent::history::HistoryProject;
use caudra_agent::learning::apply::{self, Dropped, Reverted};
use caudra_agent::learning::pump::FAILED;
use caudra_agent::learning::{LearnerActivity, LearnerHandle, LearnerStep};
use caudra_agent::memory::store::{MemoryStore, now_ms};
use caudra_config::LearningTarget;
use caudra_providers::Message;
use caudra_storage::StateDir;
use caudra_storage::learning::{LearningStore, StoredEdit};
use caudra_storage::memory_journal::EntryOrigin;
use caudra_storage::sessions::SessionError;
use tracing::warn;

use super::App;
use crate::app::tasks::TaskOutcome;
use crate::chat::{Chat, history_to_display};
use crate::components::Action;
use crate::components::learn_inspector::{LearnAction, LearnRun, LearnSnapshot};
use crate::repaint::{Cadence, Dirty};

const RUN_LIMIT: usize = 50;
pub(crate) const OFF_HERE: &str = "Learning is off in this session";
pub(crate) const RUN_STARTED: &str = "Learning run requested";
const RUN_UNSAVED: &str = "Learning needs this session saved first: ";
pub(crate) const REVERTED: &str = "Reverted ";
pub(crate) const LEFT_ALONE: &str = " changed since the run wrote it, so it was left alone";
const REVERT_FAILED: &str = "Revert failed: ";
const PAUSED: &str = "Learning paused for this project";
const RESUMED: &str = "Learning resumed for this project";
const PAUSE_FAILED: &str = "Could not pause learning: ";
const LEARNER_CHAT: &str = "learner";
pub(crate) const RUN_ENDED: &str = "Learner run ended: ";
pub(crate) const NO_TRANSCRIPT: &str = "This run kept no transcript";
const TRANSCRIPT_FAILED: &str = "Could not read the transcript: ";
#[cfg(test)]
const REPLY_WAIT: Duration = Duration::from_secs(30);
#[cfg(test)]
const REPLY_MISSING: &str = "the learning store must answer a request in flight";

enum LearnReply {
    Snapshot(Result<LearnSnapshot, String>),
    /// A revert or a pause went to the store; the words report how.
    Changed(String),
    /// A run's kept conversation, as JSON messages.
    Transcript(i64, Result<Option<String>, String>),
}

/// The channel answers come back through, drained each tick.
pub(crate) struct LearnReads {
    tx: flume::Sender<LearnReply>,
    rx: flume::Receiver<LearnReply>,
}

impl Default for LearnReads {
    fn default() -> Self {
        let (tx, rx) = flume::unbounded();
        Self { tx, rx }
    }
}

impl LearnReads {
    fn awaited(&self) -> bool {
        self.rx.sender_count() > 1
    }
}

/// What a request needs to find the project's runs and undo what they wrote.
struct LearnPlace {
    state: StateDir,
    cwd: PathBuf,
    /// The learner's own key, when one runs: it already resolved the project.
    project: Option<String>,
    /// Memory on this machine: a remote session's notes are not ours to revert.
    local: bool,
}

impl LearnPlace {
    fn project(&self) -> String {
        self.project
            .clone()
            .unwrap_or_else(|| HistoryProject::local(&self.cwd).label)
    }

    fn memory(&self) -> Option<Arc<MemoryStore>> {
        if !self.local {
            return None;
        }
        MemoryStore::for_cwd(&self.state, &self.cwd)
            .inspect_err(|error| warn!(%error, "memory not opened to revert a learned note"))
            .ok()
    }
}

impl App {
    fn learner_handle(&self) -> Option<Arc<LearnerHandle>> {
        self.learner.as_ref()?.load_full()
    }

    pub(super) fn open_learn_inspector(&mut self) -> Vec<Action> {
        self.learn_inspector.open(self.learner_handle().is_some());
        self.read_learning();
        Vec::new()
    }

    fn learn_place(&self) -> LearnPlace {
        LearnPlace {
            state: self.storage.clone(),
            cwd: PathBuf::from(&self.state.session.cwd),
            project: self
                .learner_handle()
                .map(|handle| handle.project().to_owned()),
            local: self.workspace_session.is_none(),
        }
    }

    fn read_learning(&mut self) {
        self.ask_learning(|place, store| {
            LearnReply::Snapshot(load(&store, place.project()).map_err(|error| error.to_string()))
        });
    }

    fn ask_learning(
        &mut self,
        work: impl FnOnce(&LearnPlace, LearningStore) -> LearnReply + Send + 'static,
    ) {
        let place = self.learn_place();
        let replies = self.learn_reads.tx.clone();
        smol::spawn(async move {
            let reply = smol::unblock(move || match LearningStore::open(&place.state) {
                Ok(store) => work(&place, store),
                Err(error) => LearnReply::Snapshot(Err(error.to_string())),
            })
            .await;
            let _ = replies.send(reply);
        })
        .detach();
    }

    pub(super) fn poll_learning(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        while let Ok(reply) = self.learn_reads.rx.try_recv() {
            self.land_learning(reply);
            dirty = Dirty::YES;
        }
        dirty
    }

    pub(super) fn learning_cadence(&self) -> Cadence {
        Cadence::when(self.learn_reads.awaited(), Cadence::PENDING)
    }

    /// A run finished, which an open inspector reads again.
    pub(super) fn learning_changed(&mut self) {
        if self.learn_inspector.is_open() {
            self.read_learning();
        }
    }

    fn land_learning(&mut self, reply: LearnReply) {
        match reply {
            LearnReply::Snapshot(snapshot) => {
                let live = self.learner_handle().is_some();
                self.learn_inspector.fill(snapshot, live);
            }
            LearnReply::Changed(report) => {
                self.flash(report);
                self.learning_changed();
            }
            LearnReply::Transcript(run_id, read) => match read.and_then(|kept| {
                kept.map(|json| {
                    serde_json::from_str::<Vec<Message>>(&json).map_err(|error| error.to_string())
                })
                .transpose()
            }) {
                Ok(Some(messages)) => {
                    let chat = self.stored_learner_chat(&messages);
                    self.learn_transcripts.keep(run_id, chat);
                    self.learn_transcripts.show(run_id);
                }
                Ok(None) => self.flash(NO_TRANSCRIPT.into()),
                Err(error) => self.flash(format!("{TRANSCRIPT_FAILED}{error}")),
            },
        }
    }

    fn learner_chat(&self) -> Chat {
        Chat::new(
            LEARNER_CHAT.to_owned(),
            Path::new(&self.state.session.cwd),
            self.ui_config.clone(),
            self.lua_event_handle.clone(),
        )
    }

    fn stored_learner_chat(&self, messages: &[Message]) -> Chat {
        let (display, _) = history_to_display(
            &crate::history_items(messages),
            &HashMap::new(),
            &self.ui_config.tool_output_lines,
            self.ui_config.show_reminders,
        );
        let mut chat = self.learner_chat();
        chat.load_messages(display);
        chat
    }

    /// A learner run said what it is doing: the chat watching it follows,
    /// and `/learn` reads again when a run starts or ends.
    pub(super) fn learner_activity(&mut self, activity: LearnerActivity) {
        let LearnerActivity { run_id, step } = activity;
        match step {
            LearnerStep::Started { prompt } => {
                let mut chat = self.learner_chat();
                chat.show_user_message(prompt);
                self.learn_transcripts.begin(run_id, chat);
                self.learning_changed();
            }
            LearnerStep::Event(event) => {
                if let Some(chat) = self.learn_transcripts.live_mut(run_id) {
                    match *event {
                        AgentEvent::StreamReset => chat.stream_reset(),
                        event if Chat::routed_before(&event) => {}
                        event => {
                            chat.handle_event(event, None);
                        }
                    }
                }
            }
            LearnerStep::Finished { outcome } => {
                if let Some(chat) = self.learn_transcripts.live_mut(run_id) {
                    chat.flush();
                    let ending = match outcome.as_str() {
                        FAILED => TaskOutcome::Error,
                        _ => TaskOutcome::Done,
                    };
                    chat.mark_finished(ending, &format!("{RUN_ENDED}{outcome}"));
                }
                self.learning_changed();
            }
        }
    }

    pub(super) fn handle_learn_action(&mut self, action: LearnAction) -> Vec<Action> {
        match action {
            LearnAction::Consumed => {}
            LearnAction::Close => self.learn_inspector.close(),
            LearnAction::Flash(message) => self.flash(message.into()),
            LearnAction::RunNow => {
                let flash = match self.learner_handle() {
                    None => OFF_HERE.to_owned(),
                    Some(handle) => match self.publish_conversation_permissions() {
                        Err(error) => format!("{RUN_UNSAVED}{error}"),
                        Ok(()) if handle.run_now() => RUN_STARTED.to_owned(),
                        Ok(()) => OFF_HERE.to_owned(),
                    },
                };
                self.flash(flash);
            }
            LearnAction::Revert(edit) => {
                let session = self.state.session.id.to_string();
                self.ask_learning(move |place, store| {
                    LearnReply::Changed(revert(place, &store, &edit, session))
                });
            }
            LearnAction::Transcript(run_id) => match self.learn_transcripts.has(run_id) {
                true => self.learn_transcripts.show(run_id),
                false => self.ask_learning(move |_, store| {
                    LearnReply::Transcript(
                        run_id,
                        store.transcript(run_id).map_err(|error| error.to_string()),
                    )
                }),
            },
            LearnAction::SetPaused(paused) => {
                self.ask_learning(move |place, store| {
                    LearnReply::Changed(match store.set_paused(&place.project(), paused) {
                        Ok(()) if paused => PAUSED.to_owned(),
                        Ok(()) => RESUMED.to_owned(),
                        Err(error) => format!("{PAUSE_FAILED}{error}"),
                    })
                });
            }
        }
        Vec::new()
    }

    /// Blocks until the store answers a request in flight, and lands the answer.
    #[cfg(test)]
    pub(crate) fn await_learn_reply(&mut self) {
        let reply = self
            .learn_reads
            .rx
            .recv_timeout(REPLY_WAIT)
            .expect(REPLY_MISSING);
        self.land_learning(reply);
    }
}

fn load(store: &LearningStore, project: String) -> Result<LearnSnapshot, SessionError> {
    let runs = store
        .runs(&project, RUN_LIMIT)?
        .into_iter()
        .map(|run| {
            Ok(LearnRun {
                edits: store.edits(run.id)?,
                dropped: serde_json::from_str::<Vec<Dropped>>(&run.dropped).unwrap_or_default(),
                run,
            })
        })
        .collect::<Result<_, SessionError>>()?;
    Ok(LearnSnapshot {
        state: store.project(&project)?,
        project,
        runs,
    })
}

/// Reverted as this session's edit, and recorded only when the file or note
/// still held what the run wrote.
fn revert(place: &LearnPlace, store: &LearningStore, edit: &StoredEdit, session: String) -> String {
    let memory = (edit.edit.target == LearningTarget::Memory.as_str())
        .then(|| place.memory())
        .flatten();
    let path = &edit.edit.path;
    match apply::revert(
        &edit.edit,
        memory.as_deref(),
        &EntryOrigin::Session(session),
    ) {
        Ok(Reverted::Reverted) => match store.mark_reverted(edit.run_id, edit.seq, now_ms()) {
            Ok(_) => format!("{REVERTED}{path}"),
            Err(error) => format!("{REVERT_FAILED}{error}"),
        },
        Ok(Reverted::Changed) => format!("{path}{LEFT_ALONE}"),
        Err(error) => format!("{REVERT_FAILED}{error}"),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use arc_swap::ArcSwapOption;
    use caudra_agent::history::HistoryProject;
    use caudra_agent::learning::pump::LEARNED;
    use caudra_agent::learning::targets::{InstructionFile, bullets};
    use caudra_agent::learning::{LearnerActivity, LearnerHandle, LearnerStep};
    use caudra_agent::permissions::PermissionManager;
    use caudra_agent::{AgentEvent, StoredSession};
    use caudra_config::{LearningTarget, PermissionsConfig};
    use caudra_providers::Message;
    use caudra_storage::StateDir;
    use caudra_storage::learning::{FinishedRun, LearningEdit, LearningStore};
    use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEventKind};
    use test_case::test_case;

    use super::{
        LearnAction, LearnSnapshot, NO_TRANSCRIPT, OFF_HERE, REVERTED, RUN_ENDED, RUN_STARTED,
    };
    use crate::app::App;
    use crate::app::permission_editor::ConversationPermissions;
    use crate::app::tests::{mouse_event, rendered, test_app};
    use crate::components::Overlay;
    use crate::components::learn_inspector::ALREADY_REVERTED;
    use crate::components::learn_transcript::LIVE_RUNS;

    const ORIGINAL: &str = "# Rules\n";
    const LEARNED_BULLET: &str = "Run make check before committing";
    const OWNER: &str = "test-learner";
    const LEASE_MS: i64 = 60_000;
    const NO_EVIDENCE: &str = "[]";
    const PROMPT: &str = "## Digest";
    const REPLY: &str = "Reading the history first.";
    const LIVE_RUN: i64 = 7;
    const CLOSE_HINT: &str = "Esc close";
    const BACK_HINT: &str = "Esc back";
    const LABEL_DRAWN: &str = "the label is on screen";

    fn seeded_run(
        state: &StateDir,
        project: &str,
        path: &str,
        after: String,
        transcript: Option<String>,
    ) -> i64 {
        let store = LearningStore::open(state).expect("learning store");
        let begun = store
            .begin(project, OWNER, 1, LEASE_MS, 0)
            .expect("begin")
            .expect("lease");
        let finished = FinishedRun {
            outcome: LEARNED.to_owned(),
            model: String::new(),
            tokens: 0,
            summary: String::new(),
            dropped: NO_EVIDENCE.to_owned(),
            through_ms: Some(2),
            edits: vec![LearningEdit {
                target: LearningTarget::Project.as_str().to_owned(),
                path: path.to_owned(),
                before: Some(ORIGINAL.to_owned()),
                after: Some(after),
                evidence: NO_EVIDENCE.to_owned(),
            }],
            transcript,
        };
        store
            .finish(project, OWNER, begun.id, 2, &finished)
            .expect("finish");
        begun.id
    }

    #[test]
    fn revert_restores_the_file_once_and_records_it() {
        let root = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(root.path().join("state"));
        let agents = root.path().join("AGENTS.md");
        fs::write(&agents, ORIGINAL).expect("instructions");
        let file = InstructionFile::read(LearningTarget::Project, agents.clone()).expect("read");
        let after = file.rewritten(&[LEARNED_BULLET.to_owned()]);
        fs::write(&agents, &after).expect("learned block");
        let project = HistoryProject::local(root.path()).label;
        let run = seeded_run(&state, &project, &agents.display().to_string(), after, None);
        let mut app = test_app();
        app.storage = state.clone();
        app.state.session_mut().cwd = root.path().to_string_lossy().into_owned();

        app.open_learn_inspector();
        app.await_learn_reply();
        let revert = app
            .learn_inspector
            .handle_key(KeyEvent::from(KeyCode::Char('u')));
        assert!(matches!(revert, LearnAction::Revert(_)), "{revert:?}");
        app.handle_learn_action(revert);
        app.await_learn_reply();
        let flash = app.status_bar.flash_text().unwrap_or_default().to_owned();
        assert!(flash.starts_with(REVERTED), "{flash}");
        let reverted = fs::read_to_string(&agents).expect("reverted");
        assert!(bullets(&reverted).is_empty(), "{reverted}");
        assert_eq!(reverted.trim_end(), ORIGINAL.trim_end());
        let edits = LearningStore::open(&state)
            .expect("store")
            .edits(run)
            .expect("edits");
        assert!(edits[0].reverted_ms.is_some());

        app.await_learn_reply();
        assert_eq!(
            app.learn_inspector
                .handle_key(KeyEvent::from(KeyCode::Char('u'))),
            LearnAction::Flash(ALREADY_REVERTED)
        );
    }

    #[test]
    fn run_now_without_a_learner_says_learning_is_off() {
        let mut app = test_app();
        app.handle_learn_action(LearnAction::RunNow);
        assert_eq!(app.status_bar.flash_text(), Some(OFF_HERE));
    }

    #[test]
    fn run_now_in_a_fresh_tab_saves_the_session_first() {
        let mut app = test_app();
        app.permissions = Arc::new(PermissionManager::new_persistent_in(
            PermissionsConfig::default(),
            app.permissions.project_cwd(),
            Arc::default(),
            app.storage.clone(),
        ));
        app.conversation_permissions = ConversationPermissions::Pending;
        let (handle, probe) = LearnerHandle::detached(OWNER);
        app.learner = Some(Arc::new(ArcSwapOption::from_pointee(handle)));

        app.handle_learn_action(LearnAction::RunNow);

        assert_eq!(app.status_bar.flash_text(), Some(RUN_STARTED));
        assert!(probe.asked_to_run_now());
        assert!(StoredSession::load(app.state.session.id, &app.storage).is_ok());
    }

    fn step(app: &mut App, run_id: i64, step: LearnerStep) {
        app.learner_activity(LearnerActivity { run_id, step });
    }

    #[test]
    fn a_running_learner_is_watched_live() {
        let mut app = test_app();
        step(
            &mut app,
            LIVE_RUN,
            LearnerStep::Started {
                prompt: PROMPT.to_owned(),
            },
        );
        step(
            &mut app,
            LIVE_RUN,
            LearnerStep::Event(Box::new(AgentEvent::TextDelta {
                text: REPLY.to_owned(),
            })),
        );

        app.handle_learn_action(LearnAction::Transcript(LIVE_RUN));
        assert_eq!(app.learn_transcripts.shown(), Some(LIVE_RUN));
        let chat = app.learn_transcripts.chat(LIVE_RUN).expect("live chat");
        assert!(!chat.streaming_text_is_empty());

        step(
            &mut app,
            LIVE_RUN,
            LearnerStep::Finished {
                outcome: LEARNED.to_owned(),
            },
        );
        let chat = app.learn_transcripts.chat(LIVE_RUN).expect("live chat");
        assert!(chat.is_finished());
        assert_eq!(chat.last_message_text(), format!("{RUN_ENDED}{LEARNED}"));

        app.learn_transcripts
            .handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(!app.learn_transcripts.is_open());
    }

    #[test_case(AgentEvent::StreamReset ; "stream_reset")]
    #[test_case(AgentEvent::Retry { attempt: 1, message: String::new(), delay_ms: 0 } ; "retry")]
    fn events_the_app_routes_itself_are_safe_in_a_live_run(event: AgentEvent) {
        let mut app = test_app();
        step(
            &mut app,
            LIVE_RUN,
            LearnerStep::Started {
                prompt: PROMPT.to_owned(),
            },
        );
        step(&mut app, LIVE_RUN, LearnerStep::Event(Box::new(event)));
        let chat = app.learn_transcripts.chat(LIVE_RUN).expect("live chat");
        assert_eq!(chat.last_message_text(), PROMPT);
    }

    #[test]
    fn only_the_latest_runs_stay_live() {
        let mut app = test_app();
        let runs: Vec<i64> = (0..=LIVE_RUNS as i64).collect();
        for run in &runs {
            step(
                &mut app,
                *run,
                LearnerStep::Started {
                    prompt: PROMPT.to_owned(),
                },
            );
        }
        assert!(!app.learn_transcripts.has(runs[0]));
        assert!(runs[1..].iter().all(|run| app.learn_transcripts.has(*run)));
    }

    #[test_case(Some(vec![Message::user(PROMPT.into())]), true ; "kept")]
    #[test_case(None, false ; "not_kept")]
    fn a_finished_run_opens_from_the_store(transcript: Option<Vec<Message>>, opens: bool) {
        let root = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(root.path().join("state"));
        let project = HistoryProject::local(root.path()).label;
        let json = transcript.map(|messages| serde_json::to_string(&messages).expect("json"));
        let run = seeded_run(&state, &project, ORIGINAL, ORIGINAL.to_owned(), json);
        let mut app = test_app();
        app.storage = state;
        app.state.session_mut().cwd = root.path().to_string_lossy().into_owned();

        app.handle_learn_action(LearnAction::Transcript(run));
        app.await_learn_reply();

        assert_eq!(app.learn_transcripts.is_open(), opens);
        match opens {
            true => assert_eq!(
                app.learn_transcripts
                    .chat(run)
                    .expect("stored chat")
                    .last_message_text(),
                PROMPT
            ),
            false => assert_eq!(app.status_bar.flash_text(), Some(NO_TRANSCRIPT)),
        }
    }

    /// Presses and releases on the first cell of `label` as drawn.
    fn click_label(app: &mut App, label: &str) {
        let screen = rendered(app);
        let (row, line) = screen
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains(label))
            .expect(LABEL_DRAWN);
        let column = line[..line.find(label).unwrap_or_default()].chars().count();
        let (column, row) = (column as u16, row as u16);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            app.update(mouse_event(kind, column, row));
        }
    }

    #[test]
    fn the_inspector_footer_answers_a_click() {
        let mut app = test_app();
        app.learn_inspector.open(false);
        app.learn_inspector
            .fill(Ok(LearnSnapshot::default()), false);

        click_label(&mut app, CLOSE_HINT);
        assert!(!app.learn_inspector.is_open());
    }

    #[test]
    fn the_transcript_footer_answers_a_click() {
        let mut app = test_app();
        step(
            &mut app,
            LIVE_RUN,
            LearnerStep::Started {
                prompt: PROMPT.to_owned(),
            },
        );
        app.handle_learn_action(LearnAction::Transcript(LIVE_RUN));

        click_label(&mut app, BACK_HINT);
        assert!(!app.learn_transcripts.is_open());
    }
}
