//! Continual learning: a background pump that reads what happened in a
//! project's conversations since it last looked, has a learner child agent
//! pick out what is worth keeping, and applies the checked result to the
//! project's instruction files, the user's own, and project memory. It never
//! takes a turn of the conversation it runs beside.

pub mod apply;
pub mod digest;
pub mod pump;
pub mod runner;
pub mod targets;

use serde::{Deserialize, Serialize};

pub use pump::{Learner, LearnerDeps, LearnerHandle};

/// What one run changed, for the frontends to mention.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearnedNotice {
    pub run_id: i64,
    pub summary: String,
    pub notes: usize,
    pub project: usize,
    pub local: usize,
    pub global: usize,
}

impl LearnedNotice {
    /// `Learned 2 notes · AGENTS.md`: what changed, in a few words.
    pub fn headline(&self) -> String {
        let mut parts = Vec::new();
        if self.notes > 0 {
            let noun = if self.notes == 1 { "note" } else { "notes" };
            parts.push(format!("{} memory {noun}", self.notes));
        }
        for (count, label) in [
            (self.project, "project instructions"),
            (self.local, "AGENTS.local.md"),
            (self.global, "your AGENTS.md"),
        ] {
            if count > 0 {
                parts.push(label.to_owned());
            }
        }
        format!("Learned: {}", parts.join(" · "))
    }

    pub fn changed(&self) -> bool {
        self.notes + self.project + self.local + self.global > 0
    }
}
