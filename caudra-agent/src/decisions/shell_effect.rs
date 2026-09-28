use caudra_config::decisions::FeatureMode;
use caudra_decision::{Answer, QuestionSet};
use caudra_storage::{decision_log::DecisionLabel, now_epoch};
use serde_json::json;

use super::{
    DecisionContext, DecisionFeature, Decisions, PermissionAction, PermissionDecision,
    PermissionFlag,
};

const QUESTION_SET: &str = "shell_effect.v1";
const WRITES: &str = "writes_project_files";
const QUESTIONS: &str = r#"{
    "writes_project_files": {"type":"noul","instructions":"Does executing this shell command modify files in the project tree?"},
    "changes_system_state": {"type":"noul","instructions":"Does executing this shell command change system state beyond the project tree?"}
}"#;
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
        let questions =
            QuestionSet::new(QUESTION_SET, serde_json::from_str(QUESTIONS).ok()?).ok()?;
        let evaluation = self
            .evaluate(
                DecisionFeature::ShellEffect,
                &json!({"command": command}),
                &questions,
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
