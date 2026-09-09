use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::agent::QueuedMessage;
use crate::components::Status;
use crate::theme;
use caudra_agent::mentions;
use caudra_agent::{AgentInput, AgentMode, Mention};
use caudra_storage::StateDir;
use caudra_storage::plans;
use ratatui::style::{Color, Modifier, Style};

use super::App;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Build,
    Plan,
}

pub(crate) enum PlanTrigger {
    WriteDone,
    InteractivePrompt,
}

impl Mode {
    pub(crate) fn color(&self) -> Color {
        match self {
            Self::Build => theme::current().mode_build,
            Self::Plan => theme::current().mode_plan,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum PlanState {
    #[default]
    None,
    Drafting(PathBuf),
    Ready(PathBuf),
}

impl PlanState {
    pub(crate) fn path(&self) -> Option<&Path> {
        match self {
            Self::None => Option::None,
            Self::Drafting(p) | Self::Ready(p) => Some(p),
        }
    }

    pub(crate) fn mark_ready(&mut self) {
        if let Self::Drafting(p) = self {
            *self = Self::Ready(std::mem::take(p));
        }
    }

    pub(crate) fn mark_drafting(&mut self) {
        if let Self::Ready(p) = self {
            *self = Self::Drafting(std::mem::take(p));
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    pub(crate) fn allocate_path(&mut self, storage: &StateDir, cwd: &Path) {
        if matches!(self, Self::None) {
            *self = Self::Drafting(
                plans::new_plan_path(storage, cwd)
                    .unwrap_or_else(|_| PathBuf::from("plans/plan.md")),
            );
        }
    }
}

impl App {
    pub(crate) fn transition_plan(&mut self, trigger: PlanTrigger) {
        if self.state.mode != Mode::Plan {
            return;
        }
        match trigger {
            PlanTrigger::WriteDone => {
                if self.state.plan.is_ready() {
                    return;
                }
                self.state.plan.mark_ready();
                self.plan_form.on_plan_ready();
            }
            PlanTrigger::InteractivePrompt => {
                if self.state.plan.is_ready() {
                    self.state.plan.mark_drafting();
                    self.plan_form.on_plan_drafting();
                }
            }
        }
    }

    pub(super) fn enter_plan(&mut self) {
        let cwd = PathBuf::from(&self.state.session.cwd);
        self.state.plan.allocate_path(&self.storage, &cwd);
        self.state.mode = Mode::Plan;
    }

    pub(super) fn toggle_mode(&mut self) -> Vec<super::Action> {
        match self.state.mode {
            Mode::Build => self.enter_plan(),
            Mode::Plan => self.state.mode = Mode::Build,
        };
        vec![]
    }

    pub(super) fn agent_mode(&self) -> AgentMode {
        match self.state.mode {
            Mode::Plan => match self.state.plan.path() {
                Some(p) => AgentMode::Plan(p.to_path_buf()),
                None => {
                    debug_assert!(false, "Plan mode without path - invariant violated");
                    AgentMode::Build
                }
            },
            Mode::Build => AgentMode::Build,
        }
    }

    /// Mentions in text the composer did not hand us already resolved: a queue
    /// entry restored from a previous session, where the paths were checked
    /// against a working directory that may since have changed.
    pub(crate) fn scan_mentions(&self, text: &str) -> Vec<Mention> {
        let root = Path::new(&self.state.session.cwd);
        mentions::scan(text, |path| root.join(path).exists())
            .into_iter()
            .map(|(_, mention)| mention)
            .collect()
    }

    pub(crate) fn build_agent_input(&self, msg: &QueuedMessage) -> AgentInput {
        AgentInput {
            message: msg.text.clone(),
            mode: self.agent_mode(),
            images: msg.images.clone(),
            mentions: msg.mentions.clone(),
            preamble: Vec::new(),
            thinking: self.state.thinking.clone(),
            fast: self.state.fast,
            workflow: self.state.workflow,
            prompt: None,
        }
    }

    pub(super) fn mode_label(&self) -> (Cow<'static, str>, Style) {
        let label: Cow<'static, str> = if self.is_bash_input() {
            "[BASH]".into()
        } else {
            match self.state.mode {
                Mode::Build => "[BUILD]".into(),
                Mode::Plan => "[PLAN]".into(),
            }
        };
        let style = Style::new()
            .fg(self.effective_mode_color())
            .add_modifier(Modifier::BOLD);
        (label, style)
    }

    pub(crate) fn is_bash_input(&self) -> bool {
        self.input_box.buffer.starts_with_shell_prefix()
    }

    pub(super) fn effective_mode_color(&self) -> Color {
        if self.is_bash_input() {
            theme::current().mode_bash
        } else {
            self.state.mode.color()
        }
    }

    pub(super) fn separator_style(&self) -> Style {
        if self.status == Status::Streaming {
            theme::current().input_border
        } else {
            Style::new().fg(self.effective_mode_color())
        }
    }
}
