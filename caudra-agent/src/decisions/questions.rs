//! The built-in question sets, embedded from `questions/*.json` and parsed
//! once. A set keeps its id when its wording changes; the content hash that
//! `QuestionSet` computes is its version.

use std::sync::LazyLock;

use caudra_decision::QuestionSet;

pub(crate) static SHELL_DURATION: LazyLock<Option<QuestionSet>> = LazyLock::new(|| {
    embedded(
        "shell_duration.v2",
        include_str!("questions/shell_duration.json"),
    )
});
pub(crate) static PERMISSION: LazyLock<Option<QuestionSet>> =
    LazyLock::new(|| embedded("permission.v1", include_str!("questions/permission.json")));
pub(crate) static SHELL_EFFECT: LazyLock<Option<QuestionSet>> = LazyLock::new(|| {
    embedded(
        "shell_effect.v1",
        include_str!("questions/shell_effect.json"),
    )
});
pub(crate) static CONTENT: LazyLock<Option<QuestionSet>> =
    LazyLock::new(|| embedded("content.v1", include_str!("questions/content.json")));
pub(crate) static GOAL: LazyLock<Option<QuestionSet>> =
    LazyLock::new(|| embedded("goal.v1", include_str!("questions/goal.json")));
pub(crate) static SUBAGENT: LazyLock<Option<QuestionSet>> =
    LazyLock::new(|| embedded("subagent.v1", include_str!("questions/subagent.json")));
pub(crate) static QUESTION_TOOL_NUDGE: LazyLock<Option<QuestionSet>> = LazyLock::new(|| {
    embedded(
        "question_tool_nudge.v1",
        include_str!("questions/question_tool_nudge.json"),
    )
});

fn embedded(id: &str, json: &str) -> Option<QuestionSet> {
    QuestionSet::new(id, serde_json::from_str(json).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use caudra_decision::QuestionType;
    use test_case::test_case;

    use super::{
        CONTENT, GOAL, LazyLock, PERMISSION, QUESTION_TOOL_NUDGE, QuestionSet, SHELL_DURATION,
        SHELL_EFFECT, SUBAGENT,
    };
    use crate::agent::GOAL_MET_QUESTION;
    use crate::agent::subagent::DIFFICULTY_QUESTION;
    use crate::decisions::content::{ADDRESSED_TO_AGENT, INJECTION};
    use crate::decisions::permission::FLAGS;
    use crate::decisions::question_tool_nudge::QUESTION;
    use crate::decisions::shell_duration::{DURATION_QUESTION, ENDLESS_QUESTION};
    use crate::decisions::shell_effect::{CHANGES_SYSTEM, WRITES};

    const SET_INVALID: &str = "an embedded question set failed to parse";

    #[test_case(&SHELL_DURATION, &[(DURATION_QUESTION, QuestionType::Score), (ENDLESS_QUESTION, QuestionType::Noul)]; "shell_duration")]
    #[test_case(&PERMISSION, &FLAGS.map(|flag| (flag, QuestionType::Noul)); "permission")]
    #[test_case(&SHELL_EFFECT, &[(WRITES, QuestionType::Noul), (CHANGES_SYSTEM, QuestionType::Noul)]; "shell_effect")]
    #[test_case(&CONTENT, &[(INJECTION, QuestionType::Noul), (ADDRESSED_TO_AGENT, QuestionType::Noul)]; "content")]
    #[test_case(&GOAL, &[(GOAL_MET_QUESTION, QuestionType::Noul)]; "goal")]
    #[test_case(&SUBAGENT, &[(DIFFICULTY_QUESTION, QuestionType::Score)]; "subagent")]
    #[test_case(&QUESTION_TOOL_NUDGE, &[(QUESTION, QuestionType::Noul)]; "question_tool_nudge")]
    fn every_embedded_question_set_is_valid(
        set: &LazyLock<Option<QuestionSet>>,
        required: &[(&str, QuestionType)],
    ) {
        set.as_ref()
            .expect(SET_INVALID)
            .validate_required(required)
            .unwrap();
    }
}
