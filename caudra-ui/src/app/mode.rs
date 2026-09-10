use std::path::{Path, PathBuf};

use crate::agent::QueuedMessage;
use crate::components::Status;
use crate::components::status_bar::ModeLabel;
use crate::theme;
use caudra_agent::mentions;
use caudra_agent::{AgentInput, AgentMode, Mention};
use caudra_storage::StateDir;
use caudra_storage::plans;
use ratatui::style::{Color, Modifier, Style};

use super::App;

const BASH_LABEL: &str = "[BASH]";
const BASH_SHORT_LABEL: &str = "[$]";
const BUILD_LABEL: &str = "[BUILD]";
const BUILD_SHORT_LABEL: &str = "[B]";
const PLAN_LABEL: &str = "[PLAN]";
const PLAN_SHORT_LABEL: &str = "[P]";
const TO_PLAN_LABEL: &str = "[BUILD\u{2192}PLAN]";
const TO_PLAN_SHORT_LABEL: &str = "[B\u{2192}P]";
const TO_BUILD_LABEL: &str = "[PLAN\u{2192}BUILD]";
const TO_BUILD_SHORT_LABEL: &str = "[P\u{2192}B]";

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

    /// The one place the mode is committed to the agent, so it is also where a
    /// pending toggle stops being pending.
    pub(crate) fn build_agent_input(&mut self, msg: &QueuedMessage) -> AgentInput {
        self.state.applied_mode = self.state.mode;
        AgentInput {
            message: msg.text.clone(),
            mode: self.agent_mode(),
            images: msg.images.clone(),
            mentions: msg.mentions.clone(),
            preamble: Vec::new(),
            thinking: self.state.thinking.clone(),
            fast: self.state.fast,
            prompt: None,
            resume: false,
        }
    }

    /// A toggle does not reach the agent until the next message carries it, so
    /// a mode that has been switched but not yet handed over reads as a
    /// transition rather than as an accomplished fact.
    pub(super) fn mode_label(&self) -> ModeLabel {
        let (full, short) = if self.is_bash_input() {
            (BASH_LABEL, BASH_SHORT_LABEL)
        } else {
            match (self.state.applied_mode, self.state.mode) {
                (Mode::Build, Mode::Build) => (BUILD_LABEL, BUILD_SHORT_LABEL),
                (Mode::Plan, Mode::Plan) => (PLAN_LABEL, PLAN_SHORT_LABEL),
                (Mode::Build, Mode::Plan) => (TO_PLAN_LABEL, TO_PLAN_SHORT_LABEL),
                (Mode::Plan, Mode::Build) => (TO_BUILD_LABEL, TO_BUILD_SHORT_LABEL),
            }
        };
        ModeLabel {
            full: full.into(),
            short: short.into(),
            style: Style::new()
                .fg(self.effective_mode_color())
                .add_modifier(Modifier::BOLD),
        }
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
