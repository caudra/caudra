use caudra_config::decisions::FeatureMode;
use caudra_decision::Answer;
use caudra_storage::{decision_log::DecisionLabel, now_epoch};
use serde_json::{Value, json};

use super::{
    DecisionContext, DecisionFeature, DecisionOutcome, Decisions, PermissionAction,
    PermissionDecision, PermissionFlag, questions,
};

pub(super) const WRITES: &str = "writes_project_files";
#[cfg(test)]
pub(super) const CHANGES_SYSTEM: &str = "changes_system_state";
const LABEL_SOURCE: &str = "deterministic";

impl Decisions {
    pub async fn shell_effect(
        &self,
        command: &str,
        read_only: bool,
        plan: bool,
        context: &DecisionContext,
    ) -> Option<PermissionDecision> {
        let mode = self.mode(&DecisionFeature::ShellEffect);
        if !self.enabled(&DecisionFeature::ShellEffect)
            || (read_only && *mode != FeatureMode::Shadow)
        {
            return None;
        }
        let evaluation = self
            .evaluate(
                DecisionFeature::ShellEffect,
                &effect_state(command),
                questions::SHELL_EFFECT.as_ref()?,
                context,
            )
            .await?;
        if read_only && let Some(receipt) = &evaluation.receipt {
            let _ = self
                .attach_label(
                    receipt,
                    &DecisionLabel {
                        expected: json!({WRITES: false}),
                        source: LABEL_SOURCE.into(),
                        timestamp: now_epoch(),
                        meta: json!({"deterministic_read_only": true}),
                    },
                )
                .await;
        }
        let action = (plan && *mode == FeatureMode::Advise)
            .then(|| self.writes_flag(&evaluation))
            .flatten()
            .map(|flag| PermissionAction::Advice(vec![flag]));
        Some(PermissionDecision { action, evaluation })
    }

    /// The write warning an answer raises against `shell_writes`, whatever
    /// the mode: what advise shows during plan review and shadow reports.
    pub(crate) fn writes_flag(&self, evaluation: &DecisionOutcome) -> Option<PermissionFlag> {
        let threshold = self.config().thresholds.shell_writes?;
        let Answer::Noul(answer) = evaluation.result.as_ref().ok()?.answers.get(WRITES)? else {
            return None;
        };
        (answer.noul >= threshold).then(|| PermissionFlag {
            flag: WRITES.into(),
            probability: answer.noul,
        })
    }
}

pub(super) fn effect_state(command: &str) -> Value {
    json!({"command": command})
}
