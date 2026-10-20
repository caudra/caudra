//! `/learn`: the inspector's side of the conversation with the learning
//! store. Like `/memory`, every read and change runs on a task of its own and
//! lands on a later tick, so the loop never waits on the database or on git.

use std::path::PathBuf;
use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

use caudra_agent::history::HistoryProject;
use caudra_agent::learning::LearnerHandle;
use caudra_agent::learning::apply::{self, Dropped, Reverted};
use caudra_agent::memory::store::{MemoryStore, now_ms};
use caudra_config::LearningTarget;
use caudra_storage::StateDir;
use caudra_storage::learning::{LearningStore, StoredEdit};
use caudra_storage::memory_journal::EntryOrigin;
use caudra_storage::sessions::SessionError;
use tracing::warn;

use super::App;
use crate::components::Action;
use crate::components::learn_inspector::{LearnAction, LearnRun, LearnSnapshot};
use crate::repaint::{Cadence, Dirty};

const RUN_LIMIT: usize = 50;
pub(crate) const OFF_HERE: &str = "Learning is off in this session";
pub(crate) const RUN_STARTED: &str = "Learning run requested";
pub(crate) const REVERTED: &str = "Reverted ";
pub(crate) const LEFT_ALONE: &str = " changed since the run wrote it, so it was left alone";
const REVERT_FAILED: &str = "Revert failed: ";
const PAUSED: &str = "Learning paused for this project";
const RESUMED: &str = "Learning resumed for this project";
const PAUSE_FAILED: &str = "Could not pause learning: ";
#[cfg(test)]
const REPLY_WAIT: Duration = Duration::from_secs(30);
#[cfg(test)]
const REPLY_MISSING: &str = "the learning store must answer a request in flight";

enum LearnReply {
    Snapshot(Result<LearnSnapshot, String>),
    /// A revert or a pause went to the store; the words report how.
    Changed(String),
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
        }
    }

    pub(super) fn handle_learn_action(&mut self, action: LearnAction) -> Vec<Action> {
        match action {
            LearnAction::Consumed => {}
            LearnAction::Close => self.learn_inspector.close(),
            LearnAction::Flash(message) => self.flash(message.into()),
            LearnAction::RunNow => {
                let started = self.learner_handle().is_some_and(|handle| handle.run_now());
                self.flash(if started { RUN_STARTED } else { OFF_HERE }.into());
            }
            LearnAction::Revert(edit) => {
                let session = self.state.session.id.to_string();
                self.ask_learning(move |place, store| {
                    LearnReply::Changed(revert(place, &store, &edit, session))
                });
            }
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

    use caudra_agent::history::HistoryProject;
    use caudra_agent::learning::pump::LEARNED;
    use caudra_agent::learning::targets::{InstructionFile, bullets};
    use caudra_config::LearningTarget;
    use caudra_storage::StateDir;
    use caudra_storage::learning::{FinishedRun, LearningEdit, LearningStore};
    use crossterm::event::{KeyCode, KeyEvent};

    use super::{LearnAction, OFF_HERE, REVERTED};
    use crate::app::tests::test_app;
    use crate::components::learn_inspector::ALREADY_REVERTED;

    const ORIGINAL: &str = "# Rules\n";
    const LEARNED_BULLET: &str = "Run make check before committing";
    const OWNER: &str = "test-learner";
    const LEASE_MS: i64 = 60_000;
    const NO_EVIDENCE: &str = "[]";

    fn seeded_run(state: &StateDir, project: &str, path: &str, after: String) -> i64 {
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
        let run = seeded_run(&state, &project, &agents.display().to_string(), after);
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
}
