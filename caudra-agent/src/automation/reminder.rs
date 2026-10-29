use std::collections::VecDeque;
use std::mem;

use caudra_automation::snapshot::{AutomationState, PauseSource};
use caudra_providers::Message;

use super::handle::AutomationHandle;
use super::manager::spelled;
use crate::agent::last_announced;
use crate::background_reminder::quote;
use crate::prompt::{AUTOMATIONS_MARKER, RELOCATED_MARKER};
use crate::{AgentEvent, EventSender, History};

pub(super) const MAX_ROWS: usize = 16;
const MAX_TEXT_BYTES: usize = 240;
const MAX_DIRECTORY_BYTES: usize = 1024;
const MAX_CHANGE_BYTES: usize = 768;
pub(super) const MAX_REMINDER_BYTES: usize = 16 * 1024;
const DIRECTORY_PREFIX: &str = "Working directory ";
const PROJECT_ROW: &str = " in project scope.";
const CLOSE: &str = "</system-reminder>";
const RELOCATED: &str = "The session move disarmed project automations.";

#[derive(Default)]
pub(super) struct Reminder {
    changes: VecDeque<String>,
    omitted: usize,
    had_context: bool,
    pub(super) tool_request: bool,
    directory: String,
}

impl Reminder {
    pub(super) fn new(directory: &str) -> Self {
        Self {
            directory: format!(
                "{DIRECTORY_PREFIX}{}",
                quote(directory, MAX_DIRECTORY_BYTES)
            ),
            ..Self::default()
        }
    }

    pub(super) fn armed(&mut self) {
        self.had_context = true;
    }

    pub(super) fn note(&mut self, mut text: String) {
        if self.tool_request {
            return;
        }
        self.had_context = true;
        if self.changes.len() == MAX_ROWS {
            self.changes.pop_front();
            self.omitted = self.omitted.saturating_add(1);
        }
        text.truncate(text.floor_char_boundary(MAX_CHANGE_BYTES));
        self.changes.push_back(text);
    }

    fn render(
        &mut self,
        state: &AutomationState,
        previous: Option<&str>,
        force: bool,
        relocated: bool,
    ) -> Option<String> {
        let moved = relocated
            || previous.is_some_and(|previous| {
                previous
                    .lines()
                    .any(|line| line.starts_with(DIRECTORY_PREFIX) && line != self.directory)
            });
        if moved && (self.had_context || previous.is_some()) {
            self.note(RELOCATED.to_owned());
            if let Some(previous) = previous {
                for line in previous.lines() {
                    if let Some((name, _)) = line
                        .strip_prefix("- ")
                        .and_then(|row| row.split_once(PROJECT_ROW))
                    {
                        self.note(format!(
                            "{name} disarmed because the session moved to another directory."
                        ));
                    }
                }
            }
        }
        if self.changes.is_empty() && !(force && (self.had_context || previous.is_some())) {
            return None;
        }
        let mut text = format!(
            "<system-reminder>\n{AUTOMATIONS_MARKER}\n\n{}\n",
            self.directory
        );
        if let Some(pause) = &state.session.controls.pause {
            text.push_str(&format!(
                "Automations are paused. {}\n",
                pause_cause(&pause.source, &pause.reason)
            ));
        }
        let armed: Vec<_> = state
            .automations
            .iter()
            .filter(|automation| automation.armed.is_some())
            .collect();
        if armed.is_empty() {
            text.push_str("No automations are armed.\n");
        } else {
            text.push_str("Currently armed automations\n");
            for automation in armed.iter().take(MAX_ROWS) {
                let mut kinds: Vec<_> = automation
                    .triggers
                    .iter()
                    .map(|trigger| spelled(trigger.kind))
                    .collect();
                kinds.sort();
                kinds.dedup();
                text.push_str(&format!(
                    "- {} in {} scope. Triggers {}.\n",
                    quoted(&automation.name),
                    spelled(automation.scope),
                    kinds.join(", ")
                ));
            }
            if armed.len() > MAX_ROWS {
                text.push_str(&format!(
                    "{} armed automations omitted.\n",
                    armed.len() - MAX_ROWS
                ));
            }
        }
        if !self.changes.is_empty() {
            text.push_str("Changes since the previous reminder\n");
            for change in self.changes.drain(..) {
                if text.len() + change.len() + MAX_TEXT_BYTES <= MAX_REMINDER_BYTES {
                    text.push_str(&format!("- {change}\n"));
                } else {
                    self.omitted = self.omitted.saturating_add(1);
                }
            }
            let omitted = mem::take(&mut self.omitted);
            if omitted > 0 {
                text.push_str(&format!("{omitted} changes omitted.\n"));
            }
        }
        text.push_str(CLOSE);
        Some(text)
    }
}

pub(super) fn quoted(text: &str) -> String {
    quote(text, MAX_TEXT_BYTES)
}

pub(super) fn pause_cause(source: &PauseSource, reason: &str) -> String {
    let source = match source {
        PauseSource::User => "The user paused them".to_owned(),
        PauseSource::Inspector => "The user paused them through /automations".to_owned(),
        PauseSource::Sdk => "The SDK client paused them".to_owned(),
        PauseSource::Script { automation } => {
            format!("{} called pause_automations()", quoted(automation))
        }
    };
    format!("{source} with reason {}.", quoted(reason))
}

impl AutomationHandle {
    pub(crate) fn refresh_reminder(
        &self,
        history: &mut History,
        event_tx: &EventSender,
        force: bool,
    ) {
        let previous = last_announced(history.as_slice(), &[AUTOMATIONS_MARKER]);
        let text = {
            let mut reminder = self.shared().reminder();
            let state = self.state();
            let relocated =
                last_announced(history.as_slice(), &[AUTOMATIONS_MARKER, RELOCATED_MARKER])
                    .is_some_and(|latest| Some(latest) != previous);
            reminder.render(&state, previous, force, relocated)
        };
        if let Some(text) = text {
            history.push(Message::observation(text.clone()));
            let _ = event_tx.send(AgentEvent::Injected {
                text,
                task_event: None,
                peer_event: None,
                automation_event: None,
                autopilot_event: None,
            });
        }
    }
}
