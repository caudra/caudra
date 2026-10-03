use caudra_config::decisions::FeatureMode;
use caudra_decision::Answer;
use caudra_storage::{decision_log::DecisionLabel, now_epoch};
use serde_json::{Value, json};

use super::{
    DecisionContext, DecisionFeature, Decisions, PermissionAction, PermissionDecision,
    PermissionFlag, questions,
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
        let action = if plan && *mode == FeatureMode::Advise {
            self.config().thresholds.shell_writes.and_then(|threshold| {
                let response = evaluation.result.as_ref().ok()?;
                let Answer::Noul(answer) = response.answers.get(WRITES)? else {
                    return None;
                };
                (answer.noul >= threshold).then(|| {
                    PermissionAction::Advice(vec![PermissionFlag {
                        flag: WRITES.into(),
                        probability: answer.noul,
                    }])
                })
            })
        } else {
            None
        };
        Some(PermissionDecision { action, evaluation })
    }
}

pub(super) fn effect_state(command: &str) -> Value {
    json!({"command": command})
}
