//! The values the slash palette offers for a slot whose source only the app
//! can read: the session's tasks, runs, automations, and the model's
//! thinking levels.

use caudra_docs::Library;

use crate::app::App;
use crate::components::command::SourceValue;
use crate::components::command::grammar::Source;
use crate::components::command::words::{ADAPTIVE_ARG, OFF_ARG};

const THINKING_OFF: &str = "Turn thinking off";
const THINKING_ADAPTIVE: &str = "Let the model decide how hard to think";
const THINKING_EFFORT: &str = "Effort level";
const SHELL_PREFIX: &str = "shell: ";
const ARMED: &str = "armed";
const DISARMED: &str = "disarmed";

impl App {
    /// Follows the composer's text with the slash palette, then hands it
    /// what the slot under the caret offers when the app is the one to say.
    /// The command modal's argument prompt holds the palette while it is
    /// open, so the composer's text waits until it closes.
    pub(super) fn sync_command_palette(&mut self, text: &str) {
        if let Some((line, caret)) = self.command_modal.argument_line() {
            return self.sync_command_palette_at(&line, caret);
        }
        let caret = self.active_input_box().buffer.palette_caret();
        self.sync_command_palette_at(text, caret);
    }

    /// The palette synced with `text`, its caret `caret` chars in.
    fn sync_command_palette_at(&mut self, text: &str, caret: usize) {
        let task_focused = !self.is_main_chat();
        let local = match self.workspace_session {
            Some(_) => None,
            None => std::env::current_dir().ok(),
        };
        self.command_palette.set_working_directory(local.as_deref());
        let Some(source) = self.command_palette.sync(text, caret, task_focused) else {
            return;
        };
        if let Some(values) = self.command_source_values(source) {
            self.command_palette.provide(source, values);
        }
    }

    /// `None` for a source the event loop answers instead.
    fn command_source_values(&mut self, source: Source) -> Option<Vec<SourceValue>> {
        Some(match source {
            Source::ThinkingLevels => self.thinking_values(),
            Source::Tasks => self.task_values(),
            Source::TasksOrShells => {
                let mut values = self.task_values();
                values.extend(self.shell_values());
                values
            }
            Source::WorkflowNames => self.workflow_names(),
            Source::WorkflowRuns => self
                .workflow
                .runs()
                .iter()
                .map(|run| SourceValue::new(&run.display_name, &run.workflow_name))
                .collect(),
            Source::Automations => self.automation_values(),
            Source::DocsAddresses => docs_addresses(self.docs_modal.library()),
            Source::PeerReviewed
            | Source::PeerTopics
            | Source::PeerJoinedGroups
            | Source::Directories
            | Source::Files
            | Source::McpPrompt
            | Source::Lua => return None,
        })
    }

    fn thinking_values(&self) -> Vec<SourceValue> {
        let model = &self.state.model;
        let off = (!model.requires_thinking()).then(|| SourceValue::new(OFF_ARG, THINKING_OFF));
        off.into_iter()
            .chain([SourceValue::new(ADAPTIVE_ARG, THINKING_ADAPTIVE)])
            .chain(
                model
                    .reasoning_options()
                    .effort_ladder()
                    .into_iter()
                    .map(|level| SourceValue::new(level, THINKING_EFFORT)),
            )
            .collect()
    }

    fn task_values(&self) -> Vec<SourceValue> {
        self.task_states()
            .map(|task| SourceValue::new(task.id.as_ref(), task.name))
            .collect()
    }

    fn shell_values(&self) -> Vec<SourceValue> {
        let Some(runtime) = self.background.as_ref() else {
            return Vec::new();
        };
        runtime
            .shells()
            .snapshot()
            .executions
            .iter()
            .map(|view| {
                SourceValue::new(
                    &view.record.execution_id,
                    format!("{SHELL_PREFIX}{}", view.record.command),
                )
            })
            .collect()
    }

    fn automation_values(&self) -> Vec<SourceValue> {
        let Some(handle) = self.automation_handle() else {
            return Vec::new();
        };
        handle
            .state()
            .automations
            .iter()
            .map(|automation| {
                let armed = match automation.armed {
                    Some(_) => ARMED,
                    None => DISARMED,
                };
                SourceValue::new(&automation.name, armed)
            })
            .collect()
    }
}

/// Every page by its slug, then every section as `page#anchor`.
fn docs_addresses(library: &Library) -> Vec<SourceValue> {
    let pages = library.pages();
    let sections = pages.iter().flat_map(|page| {
        page.headings.iter().map(move |heading| {
            SourceValue::new(format!("{}#{}", page.slug, heading.anchor), &heading.title)
        })
    });
    pages
        .iter()
        .map(|page| SourceValue::new(page.slug, &page.title))
        .chain(sections)
        .collect()
}
