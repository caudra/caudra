//! The Shell modal's side of the app. The modal keeps no shell state: every
//! open and refresh hands it the tracker's snapshot and the background
//! runtime's cards, and a stop names exactly the execution its row showed.

use caudra_storage::background::JobKind;

use crate::app::App;
use crate::app::tasks::ControlModal;
use crate::components::Action;
use crate::components::shell_modal::{ShellInputs, ShellModalAction, ShellStop};
use crate::repaint::Dirty;

const SHELLS_UNAVAILABLE: &str = "Shell commands are unavailable in this session";

impl App {
    fn shell_inputs(&self) -> ShellInputs {
        let runtime = self.background.as_ref();
        ShellInputs {
            snapshot: runtime.map(|runtime| runtime.shells().snapshot()),
            cards: self.task_history_cards(),
        }
    }

    /// The tracker publishes a fresh snapshot on every change, so a quiet
    /// tick costs a pointer compare. An open modal also follows the
    /// background cards and the output its details page shows.
    pub(super) fn poll_shells(&mut self) -> Dirty {
        let latest = self
            .background
            .as_ref()
            .map(|runtime| runtime.shells().snapshot());
        let mut dirty = self.shell_snapshot.poll(latest);
        if self.shell_modal.is_open() {
            let inputs = self.shell_inputs();
            dirty |= Dirty::from(self.shell_modal.refresh(inputs)) | self.shell_modal.poll_live();
        }
        dirty
    }

    /// Running commands are listed first, so opening lands on one.
    pub(super) fn shells_browse(&mut self) -> Vec<Action> {
        self.load_task_history(false);
        if self.task_picker.is_open() {
            let action = self.task_picker.cancel();
            let _ = self.handle_task_picker_action(action);
        }
        let inputs = self.shell_inputs();
        self.shell_modal.open(inputs);
        Vec::new()
    }

    /// Opens the details of a shell command a status command or a task card
    /// names. False, with nothing opened, when `id` names no shell command.
    pub(super) fn show_shell(&mut self, id: &str) -> bool {
        let known = self.background.as_ref().is_some_and(|runtime| {
            self.task_history_cards()
                .iter()
                .any(|task| task.task_id == id && task.kind == JobKind::Shell)
                || runtime
                    .shells()
                    .snapshot()
                    .executions
                    .iter()
                    .any(|view| view.record.execution_id == id)
        });
        if !known {
            return false;
        }
        let _ = self.shells_browse();
        if let Some(action) = self.shell_modal.show(id) {
            let _ = self.handle_shell_modal_action(action);
        }
        true
    }

    pub(super) fn handle_shell_modal_action(&mut self, action: ShellModalAction) -> Vec<Action> {
        match action {
            ShellModalAction::History { older } => self.load_task_history(older),
            ShellModalAction::Consumed => {}
            ShellModalAction::Stop(ShellStop::Foreground(id)) => {
                let stopped = self
                    .background
                    .as_ref()
                    .ok_or_else(|| SHELLS_UNAVAILABLE.to_owned())
                    .and_then(|runtime| runtime.shells().cancel(&id));
                if let Err(error) = stopped {
                    self.flash(error);
                }
            }
            ShellModalAction::Stop(ShellStop::Background(task)) => {
                self.start_task_control(*task, false, ControlModal::Shells);
            }
            ShellModalAction::LoadOutput(id) => {
                if let Some(runtime) = &self.background {
                    runtime.shells().load_output(&id);
                }
            }
        }
        Vec::new()
    }
}
