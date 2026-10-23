//! Ports of the scripts pstack's skills run. Each is a CLI as upstream ships it: [`Script::run`]
//! takes the arguments after the program name and returns the exit code and output the script
//! prints, so a playbook's command line maps onto a call one to one.

pub mod check_plan;
pub mod log;
pub mod orch;
pub mod statechart;
pub mod watch_pr;
pub mod worktree_audit;

use crate::host::{Host, Output};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Script {
    Log,
    CheckPlan,
    WorktreeAudit,
    WatchPr,
    Orch,
}

impl Script {
    pub const ALL: [Self; 5] = [
        Self::Log,
        Self::CheckPlan,
        Self::WorktreeAudit,
        Self::WatchPr,
        Self::Orch,
    ];

    /// The member a `code` snippet calls it by.
    pub fn name(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::CheckPlan => "check_plan",
            Self::WorktreeAudit => "worktree_audit",
            Self::WatchPr => "watch_pr",
            Self::Orch => "orch",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|script| script.name() == name)
    }

    /// The upstream file the port follows, relative to pstack's `skills/`.
    pub fn upstream(self) -> &'static str {
        match self {
            Self::Log => "show-me-your-work/scripts/log.sh",
            Self::CheckPlan => "poteto-mode/scripts/check-plan.mjs",
            Self::WorktreeAudit => "poteto-mode/scripts/worktree-audit.sh",
            Self::WatchPr => "poteto-mode/scripts/watch-pr/watch-pr",
            Self::Orch => "poteto-mode/scripts/orch/orch.ts",
        }
    }

    /// Whether a run of `argv` can change files, which plan mode refuses and permissions ask about.
    /// `orch` reads its command line, so its help and its read-only commands write nothing; `env`
    /// answers the variables its options fall back to.
    pub fn writes(self, argv: &[String], env: &dyn Fn(&str) -> Option<String>) -> bool {
        match self {
            Self::Log => true,
            Self::Orch => orch::store_use(argv, env).is_some_and(|used| used.writes),
            Self::CheckPlan | Self::WorktreeAudit | Self::WatchPr => false,
        }
    }

    pub fn run(self, host: &dyn Host, argv: &[String]) -> Output {
        match self {
            Self::Log => log::run(host, argv),
            Self::CheckPlan => check_plan::run(host, argv),
            Self::WorktreeAudit => worktree_audit::run(host, argv),
            Self::WatchPr => watch_pr::run(host, argv),
            Self::Orch => orch::run(host, argv),
        }
    }
}
