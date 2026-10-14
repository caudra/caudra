//! The App's half of the goal log: draining what the goal handle journals into
//! the session's log, the `/goal` panel, and the rows a resume restores.

use caudra_storage::StateDir;
use caudra_storage::goal_log::{GoalEnd, GoalLogWrite};
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::SessionDatabase;

use super::App;
use crate::components::audit_log::{COPIED_ENTRY, PanelTab, ROW_NOT_LOADED};
use crate::components::goal_panel::{
    GoalLog, NO_GOAL_LOG, PanelAction, PanelCommand, cleared_after_error_row, evaluation_row,
};
use crate::components::{Action, DisplayMessage, DisplayRole, Overlay};
use crate::repaint::Dirty;

const GOAL_LOG: &str = "goal";
/// The goal log only describes the main agent's goal.
const MAIN_CHAT: usize = 0;

/// Where a restored log row goes: after the history items the session held
/// when it was made, ordered by when it was made among rows placed alike.
pub(super) type LogPlace = (usize, u64);

/// The session's whole goal log, for the panel's Log tab.
fn load_goal_log(storage: &StateDir, session_id: CaudraId) -> Result<GoalLog, String> {
    let database = SessionDatabase::open(storage).map_err(|error| error.to_string())?;
    let goals = database
        .load_goals(session_id)
        .map_err(|error| error.to_string())?;
    let evaluations = database
        .load_goal_evaluations(session_id)
        .map_err(|error| error.to_string())?;
    Ok(GoalLog::new(goals, evaluations))
}

impl App {
    /// Hands what the goal handle journaled to the writer, after the
    /// session's row so the log has one to reference. A goal's start or end
    /// is placed at the history the session holds now, and never before a
    /// check drained with it, which the agent placed itself. An open panel's
    /// Log takes each write too.
    pub(super) fn flush_goal_log(&mut self) {
        if !self.state.goal.has_unlogged() {
            return;
        }
        self.checkpoint_now();
        let mut writes = self.state.goal.take_log();
        GoalLogWrite::stamp_batch(&mut writes, self.autopilot_history_len());
        let session_id = self.state.session.id;
        for write in writes {
            self.goal_panel.note(&write);
            self.storage_writer.record_goal_log(session_id, write);
        }
    }

    /// Drains a goal change no event announced, such as one an automation
    /// or a resume made.
    pub(super) fn poll_goal_log(&mut self) -> Dirty {
        if !self.state.goal.has_unlogged() {
            return Dirty::NO;
        }
        self.flush_goal_log();
        Dirty::from(self.goal_panel.is_open())
    }

    /// Opens the panel on `tab`, reading the session's log off the UI thread.
    /// The journal is drained and what the writer has yet to write is taken
    /// first, so the read misses nothing the transcript already shows.
    pub(super) fn open_goal_panel(&mut self, tab: PanelTab) {
        self.clear_control_hovers();
        self.flush_goal_log();
        let storage = self.storage.clone();
        let session_id = self.state.session.id;
        let unwritten = self.storage_writer.unwritten_goal_log(session_id);
        self.goal_panel.open(
            tab,
            smol::unblock(move || load_goal_log(&storage, session_id)),
            unwritten,
            self.ui_config.clock_format,
        );
    }

    /// The Log, whether a goal is active or not. A session that never set a
    /// goal has nothing logged, which needs no read to tell.
    pub(super) fn open_goal_log(&mut self) {
        if self.state.goal.stored().log.is_none() {
            self.flash(NO_GOAL_LOG.into());
            return;
        }
        self.open_goal_panel(PanelTab::Log);
    }

    /// The panel closes before it runs a command or jumps, so what that opens
    /// or scrolls is not drawn under it.
    pub(super) fn on_goal_panel(&mut self, action: PanelAction) -> Vec<Action> {
        match action {
            PanelAction::Copy(text) => self.copy_labelled(&text, COPIED_ENTRY),
            PanelAction::Flash(message) => self.flash(message.into()),
            PanelAction::Jump(seq) => self.jump_to_goal_evaluation(seq),
            PanelAction::Command(PanelCommand::Limit(limit)) => {
                self.state.goal.set_continuation_limit(limit);
            }
            PanelAction::Command(PanelCommand::Run(cmdline)) => {
                self.goal_panel.close();
                return self.run_footer_command(cmdline);
            }
        }
        Vec::new()
    }

    /// Scrolls the main transcript to check `seq`'s row. A check that drew no
    /// row, or one the transcript does not hold, leaves the panel up, saying
    /// so.
    fn jump_to_goal_evaluation(&mut self, seq: u64) {
        let Some(row) = self.chats[MAIN_CHAT].goal_evaluation_row(seq) else {
            self.flash(ROW_NOT_LOADED.into());
            return;
        };
        self.goal_panel.close();
        self.scroll_main_to(row);
    }

    /// The transcript rows the stored log drew live, each with where a
    /// restore places it among the turns: every check whose verdict or
    /// failure stood, and every goal the run cleared after an error.
    pub(super) fn goal_log_rows(&self) -> Vec<(LogPlace, DisplayMessage)> {
        let evaluations = self
            .read_session_log(GOAL_LOG, SessionDatabase::load_goal_evaluations)
            .into_iter()
            .filter_map(|evaluation| {
                let (tone, text) = evaluation_row(&evaluation)?;
                Some((
                    (evaluation.history_len, evaluation.at_ms),
                    DisplayMessage::goal_evaluation(evaluation.seq, tone, text),
                ))
            });
        let cleared = self
            .read_session_log(GOAL_LOG, SessionDatabase::load_goals)
            .into_iter()
            .filter_map(|goal| {
                let Some(GoalEnd::Error { message }) = &goal.end else {
                    return None;
                };
                let place = (goal.ended_history_len?, goal.ended_at_ms?);
                let row = cleared_after_error_row(&goal.condition, message);
                Some((place, DisplayMessage::new(DisplayRole::Error, row)))
            });
        evaluations.chain(cleared).collect()
    }
}
