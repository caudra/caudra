use caudra_config::decisions::FeatureMode;
use caudra_decision::Answer;
use serde_json::json;

use super::{DecisionOutcome, DecisionState, redact_decision_text};

pub(crate) const QUESTION: &str = "question_tool_needed";
pub(crate) const REMINDER: &str = "<system-reminder>\n# Ask through the question tool\n\nYour last reply asks the user for a choice, clarification, or approval. Use the `question` tool now to collect the outstanding answer instead of leaving the question only in prose. Preserve the choices and any recommendation already given. Do not invent new questions, repeat questions already answered, or proceed as though approval was granted. Follow the user's latest instructions if they have changed the request.\n</system-reminder>";
const USER_REQUEST_BYTES: usize = 320;
const USER_REQUEST_HEAD_BYTES: usize = 80;
const ASSISTANT_REPLY_BYTES: usize = 1_120;
const ASSISTANT_REPLY_HEAD_BYTES: usize = 160;
const OMISSION: &str = "\n[... omitted ...]\n";

pub(crate) fn state(user_request: &str, assistant_reply: &str) -> Option<DecisionState> {
    if user_request.trim().is_empty() || assistant_reply.trim().is_empty() {
        return None;
    }
    let user_request = excerpt(
        &redact_decision_text(user_request),
        USER_REQUEST_BYTES,
        USER_REQUEST_HEAD_BYTES,
    );
    let assistant_reply = excerpt(
        &redact_decision_text(assistant_reply),
        ASSISTANT_REPLY_BYTES,
        ASSISTANT_REPLY_HEAD_BYTES,
    );
    DecisionState::new(&json!({
        "user_request": user_request,
        "assistant_reply": assistant_reply,
    }))
    .ok()
}

pub(crate) fn should_nudge(mode: &FeatureMode, outcome: &DecisionOutcome, threshold: f64) -> bool {
    *mode == FeatureMode::Advise
        && threshold.is_finite()
        && (0.0..=1.0).contains(&threshold)
        && probability(outcome).is_some_and(|probability| probability >= threshold)
}

pub(super) fn probability(outcome: &DecisionOutcome) -> Option<f64> {
    let response = outcome.result.as_ref().ok()?;
    if response.answers.len() != 1 {
        return None;
    }
    let Answer::Noul(answer) = response.answers.get(QUESTION)? else {
        return None;
    };
    (answer.noul.is_finite() && (0.0..=1.0).contains(&answer.noul)).then_some(answer.noul)
}

fn excerpt(text: &str, budget: usize, head_budget: usize) -> String {
    if fitting_bytes(text.chars(), budget) == text.len() {
        return text.to_owned();
    }
    let head = fitting_bytes(text.chars(), head_budget);
    let used: usize = text[..head]
        .chars()
        .chain(OMISSION.chars())
        .map(json_bytes)
        .sum();
    let tail = fitting_bytes(text.chars().rev(), budget.saturating_sub(used));
    format!("{}{OMISSION}{}", &text[..head], &text[text.len() - tail..])
}

fn fitting_bytes(chars: impl Iterator<Item = char>, mut budget: usize) -> usize {
    let mut bytes = 0;
    for character in chars {
        let cost = json_bytes(character);
        if cost > budget {
            break;
        }
        budget -= cost;
        bytes += character.len_utf8();
    }
    bytes
}

fn json_bytes(character: char) -> usize {
    match character {
        '"' | '\\' | '\u{0008}' | '\u{000c}' | '\n' | '\r' | '\t' => 2,
        '\u{0000}'..='\u{001f}' => 6,
        _ => character.len_utf8(),
    }
}

#[cfg(test)]
mod tests {
    use caudra_config::decisions::{DecisionThresholds, FeatureMode};
    use caudra_decision::{
        Answer, DecisionError, DecisionResponse, NoulAnswer, ScoreAnswer, Usage,
    };
    use serde_json::json;
    use test_case::test_case;

    use super::{
        ASSISTANT_REPLY_BYTES, OMISSION, QUESTION, REMINDER, USER_REQUEST_BYTES, probability,
        should_nudge, state,
    };
    use crate::decisions::state::MAX_STATE_BYTES;
    use crate::decisions::{DecisionFeature, DecisionOutcome, stats_thresholds};

    const REQUEST: &str = "Implement the recording controls.";
    const REPLY: &str = "I recommend 2x. Which zoom level should I use?";
    const SECRET: &str = "never-send-this-value";
    const ERROR: &str = "invalid test response";
    const THRESHOLD: f64 = 0.85;

    fn outcome(answer: f64) -> DecisionOutcome {
        DecisionOutcome {
            result: Ok(DecisionResponse {
                model: "test".into(),
                answers: [(QUESTION.into(), Answer::Noul(NoulAnswer { noul: answer }))].into(),
                usage: Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                },
                cache_hit: false,
            }),
            latency_ms: 0,
            receipt: None,
        }
    }

    #[test_case(FeatureMode::Off, false; "off")]
    #[test_case(FeatureMode::Shadow, false; "shadow")]
    #[test_case(FeatureMode::Advise, true; "advise")]
    #[test_case(FeatureMode::Enforce, false; "unsupported_enforce")]
    fn only_advise_acts(mode: FeatureMode, expected: bool) {
        assert_eq!(should_nudge(&mode, &outcome(1.0), THRESHOLD), expected);
    }

    #[test_case(0.0, false; "negative")]
    #[test_case(0.5212, false; "known_quiz_miss")]
    #[test_case(THRESHOLD - f64::EPSILON, false; "below_boundary")]
    #[test_case(THRESHOLD, true; "boundary")]
    #[test_case(1.0, true; "positive")]
    #[test_case(-0.1, false; "below_range")]
    #[test_case(1.1, false; "above_range")]
    #[test_case(f64::NAN, false; "nan")]
    #[test_case(f64::INFINITY, false; "infinity")]
    #[test_case(f64::NEG_INFINITY, false; "negative_infinity")]
    fn scores_fail_open(answer: f64, expected: bool) {
        assert_eq!(
            should_nudge(&FeatureMode::Advise, &outcome(answer), THRESHOLD),
            expected
        );
    }

    #[test_case(-0.1; "below_range")]
    #[test_case(1.1; "above_range")]
    #[test_case(f64::NAN; "nan")]
    #[test_case(f64::INFINITY; "infinity")]
    fn invalid_thresholds_fail_open(threshold: f64) {
        assert!(!should_nudge(
            &FeatureMode::Advise,
            &outcome(1.0),
            threshold
        ));
    }

    #[test_case(DecisionError::Timeout; "timeout")]
    #[test_case(DecisionError::Unreachable; "unreachable")]
    #[test_case(DecisionError::Invalid(ERROR); "invalid")]
    #[test_case(DecisionError::Rejected(ERROR); "rejected")]
    #[test_case(DecisionError::Http { status: 503 }; "http")]
    fn errors_fail_open(error: DecisionError) {
        let mut outcome = outcome(1.0);
        outcome.result = Err(error);
        assert_eq!(probability(&outcome), None);
        assert!(!should_nudge(&FeatureMode::Advise, &outcome, THRESHOLD));
    }

    #[test_case("missing"; "missing")]
    #[test_case("wrong_id"; "wrong_id")]
    #[test_case("wrong_type"; "wrong_type")]
    #[test_case("extra"; "extra")]
    fn malformed_answers_fail_open(kind: &str) {
        let mut outcome = outcome(1.0);
        let answers = &mut outcome.result.as_mut().unwrap().answers;
        match kind {
            "missing" => answers.clear(),
            "wrong_id" => {
                let answer = answers.remove(QUESTION).unwrap();
                answers.insert("other".into(), answer);
            }
            "wrong_type" => {
                answers.insert(
                    QUESTION.into(),
                    Answer::Score(ScoreAnswer {
                        score: 1.0,
                        confidence: 1.0,
                        legend: [("0".into(), "no".into()), ("1".into(), "yes".into())].into(),
                        probabilities: [("0".into(), 0.0), ("1".into(), 1.0)].into(),
                    }),
                );
            }
            _ => {
                answers.insert("extra".into(), Answer::Noul(NoulAnswer { noul: 1.0 }));
            }
        }
        assert_eq!(probability(&outcome), None);
        assert!(!should_nudge(&FeatureMode::Advise, &outcome, THRESHOLD));
    }

    #[test_case("", REPLY; "no_request")]
    #[test_case(REQUEST, " \n\t"; "no_reply")]
    fn empty_context_is_not_sent(request: &str, reply: &str) {
        assert!(state(request, reply).is_none());
    }

    #[test_case(REPLY; "explicit_question")]
    #[test_case("Please confirm whether I should implement the fix."; "implicit_request")]
    #[test_case("Could you share the patch?\nDraft requested by the user."; "draft_data")]
    #[test_case("Why bump the version? Because the API changed."; "rhetorical_data")]
    #[test_case("Tests could not run because credentials are missing."; "blocker_data")]
    fn small_context_is_preserved_without_a_question_mark_prefilter(reply: &str) {
        assert_eq!(
            state(REQUEST, reply).unwrap().value(),
            &json!({
                "user_request": REQUEST,
                "assistant_reply": reply,
            })
        );
    }

    #[test_case("a"; "ascii")]
    #[test_case("界𐐀é"; "utf8")]
    #[test_case("\"\\\n\r\t"; "json_escapes")]
    #[test_case("\u{0000}\u{0001}\u{0008}\u{000c}\u{001f}"; "control_escapes")]
    fn large_context_keeps_both_ends_within_serialized_cap(filler: &str) {
        let text = format!("opening {} {REPLY}", filler.repeat(MAX_STATE_BYTES));
        let state = state(&text, &text).unwrap();
        assert!(state.value().to_string().len() <= MAX_STATE_BYTES);
        for (field, budget) in [
            ("user_request", USER_REQUEST_BYTES),
            ("assistant_reply", ASSISTANT_REPLY_BYTES),
        ] {
            let excerpt = state.value()[field].as_str().unwrap();
            assert!(excerpt.starts_with("opening "));
            assert!(excerpt.ends_with(REPLY));
            assert!(excerpt.contains(OMISSION));
            assert!(serde_json::to_string(excerpt).unwrap().len() <= budget + 2);
        }
    }

    #[test]
    fn redaction_happens_before_clipping_either_end() {
        let secret = SECRET.repeat(MAX_STATE_BYTES);
        let text = format!("https://user:{secret}@example.test\n{REPLY}\nTOKEN={secret}");
        let state = state(&text, &text).unwrap();
        let encoded = state.value().to_string();
        assert!(!encoded.contains(SECRET));
        assert!(encoded.contains("[redacted]"));
        assert!(
            state.value()["assistant_reply"]
                .as_str()
                .unwrap()
                .contains(REPLY)
        );
        assert!(encoded.len() <= MAX_STATE_BYTES);
    }

    #[test]
    fn omitted_middle_questions_are_a_recall_limitation() {
        let padding = "status ".repeat(MAX_STATE_BYTES);
        let reply = format!("{padding}{REPLY}{padding}");
        let state = state(REQUEST, &reply).unwrap();
        let excerpt = state.value()["assistant_reply"].as_str().unwrap();
        assert!(!excerpt.contains(REPLY));
        assert!(excerpt.contains(OMISSION));
    }

    #[test]
    fn feature_and_statistics_use_the_typed_question() {
        let feature = DecisionFeature::QuestionToolNudge;
        let config = DecisionThresholds {
            question_tool_nudge: 0.93,
            ..Default::default()
        };
        assert_eq!(feature.name(), "question_tool_nudge");
        assert_eq!(feature.config_key(), Some(feature.name()));
        assert_eq!(
            stats_thresholds(&config, feature.name()).noul_by_question[QUESTION],
            config.question_tool_nudge
        );
        assert!(REMINDER.starts_with("<system-reminder>\n"));
        assert!(REMINDER.ends_with("\n</system-reminder>"));
    }
}
