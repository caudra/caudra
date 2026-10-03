use std::fs::OpenOptions;
use std::io::{ErrorKind, Read};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use caudra_config::decisions::FeatureMode;
use caudra_decision::{Answer, DecisionError, DecisionResponse, QuestionSet, QuestionType};
use serde_json::Value;

use super::{DecisionContext, DecisionFeature, DecisionOutcome, Decisions, questions};

const QUESTION_FILE: &str = "decisions/permission.json";
const MAX_QUESTION_FILE_BYTES: u64 = 64 * 1024;
const TAINT_THRESHOLD_FACTOR: f64 = 0.75;
pub(super) const FLAGS: [&str; 5] = [
    "deletes",
    "uploads",
    "credentials",
    "permissions",
    "remote_rewrite",
];

#[derive(Clone, Debug)]
pub enum PermissionPurpose {
    Advice,
    AutoScreening,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PermissionFlag {
    pub flag: String,
    pub probability: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PermissionAction {
    Advice(Vec<PermissionFlag>),
    Escalate(Vec<PermissionFlag>),
}

pub struct PermissionDecision {
    pub action: Option<PermissionAction>,
    pub evaluation: DecisionOutcome,
}

impl Decisions {
    pub fn permission_questions() -> Result<QuestionSet, DecisionError> {
        questions::PERMISSION.clone().ok_or(DecisionError::Rejected(
            "invalid embedded permission questions",
        ))
    }

    pub async fn permission(
        &self,
        purpose: PermissionPurpose,
        state: &Value,
        context: &DecisionContext,
    ) -> Option<PermissionDecision> {
        let feature = match purpose {
            PermissionPurpose::Advice => DecisionFeature::PermissionAdvice,
            PermissionPurpose::AutoScreening => DecisionFeature::AutoScreening,
        };
        if !self.enabled(&feature) {
            return None;
        }
        let evaluation = self
            .evaluate(
                feature.clone(),
                state,
                &self.0.permission_questions,
                context,
            )
            .await?;
        Some(PermissionDecision {
            action: self.permission_action(&feature, &evaluation.result),
            evaluation,
        })
    }

    pub(super) fn permission_action(
        &self,
        feature: &DecisionFeature,
        result: &Result<DecisionResponse, DecisionError>,
    ) -> Option<PermissionAction> {
        let (escalate, threshold) = match (feature, self.mode(feature)) {
            (DecisionFeature::PermissionAdvice, FeatureMode::Advise) => {
                (false, self.config().thresholds.permission_flag)
            }
            (DecisionFeature::AutoScreening, FeatureMode::Enforce) => {
                (true, self.config().thresholds.auto_flag)
            }
            _ => return None,
        };
        let response = match result {
            Ok(response) => response,
            Err(_) => return escalate.then(|| PermissionAction::Escalate(Vec::new())),
        };
        let flags: Vec<_> = FLAGS
            .iter()
            .filter_map(|flag| {
                let Answer::Noul(answer) = response.answers.get(*flag)? else {
                    return None;
                };
                let threshold = if escalate
                    && self.is_tainted()
                    && matches!(*flag, "uploads" | "credentials")
                {
                    threshold * TAINT_THRESHOLD_FACTOR
                } else {
                    threshold
                };
                (answer.noul >= threshold).then(|| PermissionFlag {
                    flag: (*flag).into(),
                    probability: answer.noul,
                })
            })
            .collect();
        if flags.is_empty() {
            None
        } else if escalate {
            Some(PermissionAction::Escalate(flags))
        } else {
            Some(PermissionAction::Advice(flags))
        }
    }
}

pub(super) fn load_questions(global_dir: Option<&Path>) -> Result<QuestionSet, DecisionError> {
    let defaults = Decisions::permission_questions()?;
    let Some(global_dir) = global_dir else {
        return Ok(defaults);
    };
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let file = match options.open(global_dir.join(QUESTION_FILE)) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(defaults),
        Err(_) => {
            return Err(DecisionError::Rejected(
                "permission question override cannot be opened",
            ));
        }
    };
    let metadata = file
        .metadata()
        .map_err(|_| DecisionError::Rejected("permission question override cannot be inspected"))?;
    if !metadata.is_file() || metadata.len() > MAX_QUESTION_FILE_BYTES {
        return Err(DecisionError::Rejected(
            "permission question override must be a bounded regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_QUESTION_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| DecisionError::Rejected("permission question override cannot be read"))?;
    if bytes.len() as u64 > MAX_QUESTION_FILE_BYTES {
        return Err(DecisionError::Rejected(
            "permission question override exceeds the byte limit",
        ));
    }
    let questions = QuestionSet::new(
        defaults.id(),
        serde_json::from_slice(&bytes)
            .map_err(|_| DecisionError::Rejected("permission question override is invalid JSON"))?,
    )?;
    let required: Vec<_> = defaults
        .questions()
        .keys()
        .map(|id| (id.as_str(), QuestionType::Noul))
        .collect();
    questions.validate_required(&required)?;
    Ok(questions)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use caudra_config::decisions::{DecisionsConfig, FeatureMode};
    use caudra_decision::{Answer, DecisionResponse, NoulAnswer, QuestionType, Usage};
    use caudra_storage::StateDir;
    use serde_json::json;
    use test_case::test_case;

    use super::{
        DecisionFeature, Decisions, MAX_QUESTION_FILE_BYTES, PermissionAction, QUESTION_FILE,
        load_questions,
    };

    const TAINT_PROBABILITY: f64 = 0.8;

    #[test_case(FeatureMode::Enforce, true; "taint_escalates_only_exfiltration_flags")]
    #[test_case(FeatureMode::Shadow, false; "taint_does_not_change_shadow")]
    fn taint_thresholds_remain_caution_only(mode: FeatureMode, escalates: bool) {
        let root = tempfile::tempdir().unwrap();
        let mut config = DecisionsConfig::default();
        config.features.auto_screening = mode;
        config.thresholds.auto_flag = 1.0;
        let model = config.model.clone();
        let service = Decisions::new(config, &StateDir::from_path(root.path().into())).unwrap();
        let response = Ok(DecisionResponse {
            model,
            answers: ["uploads", "credentials", "deletes"]
                .into_iter()
                .map(|flag| {
                    (
                        flag.into(),
                        Answer::Noul(NoulAnswer {
                            noul: TAINT_PROBABILITY,
                        }),
                    )
                })
                .collect(),
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
            cache_hit: false,
        });
        assert!(
            service
                .permission_action(&DecisionFeature::AutoScreening, &response)
                .is_none()
        );
        service.mark_tainted();
        let action = service.permission_action(&DecisionFeature::AutoScreening, &response);
        if escalates {
            let Some(PermissionAction::Escalate(flags)) = action else {
                panic!("tainted enforce must escalate")
            };
            let flags: Vec<_> = flags.iter().map(|flag| flag.flag.as_str()).collect();
            assert_eq!(flags, ["uploads", "credentials"]);
        } else {
            assert!(action.is_none());
        }
    }

    #[test_case(false; "wording_override")]
    #[test_case(true; "missing_override_uses_defaults")]
    fn overrides_are_global_and_content_versioned(missing: bool) {
        let root = tempfile::tempdir().unwrap();
        let defaults = Decisions::permission_questions().unwrap();
        if !missing {
            let mut questions = defaults.questions().clone();
            questions.get_mut("deletes").unwrap().instructions =
                json!("Will local data be deleted?");
            let path = root.path().join(QUESTION_FILE);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, serde_json::to_vec(&questions).unwrap()).unwrap();
        }
        let loaded = load_questions(Some(root.path())).unwrap();
        assert_eq!(loaded.id(), defaults.id());
        assert_eq!(loaded.version() == defaults.version(), missing);
    }

    #[test_case(false; "required_id")]
    #[test_case(true; "required_noul_type")]
    fn invalid_permission_contract_never_falls_back_to_defaults(wrong_type: bool) {
        let root = tempfile::tempdir().unwrap();
        let mut questions = Decisions::permission_questions()
            .unwrap()
            .questions()
            .clone();
        if wrong_type {
            let question = questions.get_mut("deletes").unwrap();
            question.kind = QuestionType::Score;
            question.criteria = Some(json!(["low", "high"]));
        } else {
            questions.remove("deletes");
        }
        let path = root.path().join(QUESTION_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec(&questions).unwrap()).unwrap();
        assert!(load_questions(Some(root.path())).is_err());
    }

    #[test_case(false; "oversized")]
    #[test_case(true; "not_regular")]
    fn override_files_are_bounded_regular_files(directory: bool) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(QUESTION_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        if directory {
            fs::create_dir(path).unwrap();
        } else {
            fs::write(path, vec![b' '; MAX_QUESTION_FILE_BYTES as usize + 1]).unwrap();
        }
        assert!(load_questions(Some(root.path())).is_err());
    }
}
