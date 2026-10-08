use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::engine::DecisionError;
use crate::question_set::{bounded_json, rendered_text};
use crate::wire::{
    Answer, ChoiceAnswer, DecisionRequest, DecisionResponse, MAX_REQUEST_BYTES, MAX_STATE_CHARS,
    NoulAnswer, QuestionType, ScoreAnswer, Usage,
};

const MIN_CHOICES: usize = 2;
const PREDICATE_CRITERIA: &str = "\n\nCriteria (false and true outcomes):\n";
const INVALID_SCHEMA: &str = "response does not match the OpenAI Decisions JSON schema";
const INVALID_NAMES: &str = "answer names do not match the requested questions";
const DUPLICATE_VALUE: &str = "answer contains duplicate probability values";

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    input: String,
    questions: Vec<Question<'a>>,
}

#[derive(Serialize)]
struct Question<'a> {
    name: &'a str,
    instructions: String,
    #[serde(flatten)]
    kind: QuestionKind<'a>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum QuestionKind<'a> {
    Predicate,
    Choice { choices: Vec<Choice<'a>> },
    Score { levels: Vec<Level> },
}

#[derive(Serialize)]
struct Choice<'a> {
    value: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

#[derive(Serialize)]
struct Level {
    label: String,
}

pub(crate) fn encode(request: &DecisionRequest) -> Result<Vec<u8>, DecisionError> {
    bounded_json(request, MAX_REQUEST_BYTES)?;
    let input = rendered_text(&request.state)?;
    if input.chars().count() > MAX_STATE_CHARS {
        return Err(DecisionError::Rejected("input exceeds the character limit"));
    }
    let mut questions = Vec::with_capacity(request.questions.len());
    for (name, question) in &request.questions {
        let mut instructions = rendered_text(&question.instructions)?;
        let kind = match question.kind {
            QuestionType::Noul => {
                if let Some(criteria) = &question.criteria {
                    instructions.push_str(PREDICATE_CRITERIA);
                    instructions.push_str(&rendered_text(criteria)?);
                }
                QuestionKind::Predicate
            }
            QuestionType::Choice => {
                let options = question.choice_options()?;
                if options.len() < MIN_CHOICES {
                    return Err(DecisionError::Rejected(
                        "OpenAI choice questions require at least two options",
                    ));
                }
                let choices = options
                    .iter()
                    .map(|(value, description)| {
                        Ok(Choice {
                            value,
                            description: (!description.is_null())
                                .then(|| rendered_text(description))
                                .transpose()?,
                        })
                    })
                    .collect::<Result<_, DecisionError>>()?;
                QuestionKind::Choice { choices }
            }
            QuestionType::Score => QuestionKind::Score {
                levels: question
                    .score_levels()?
                    .iter()
                    .map(|level| rendered_text(level).map(|label| Level { label }))
                    .collect::<Result<_, _>>()?,
            },
        };
        questions.push(Question {
            name,
            instructions,
            kind,
        });
    }
    bounded_json(
        &Request {
            model: &request.model,
            input,
            questions,
        },
        MAX_REQUEST_BYTES,
    )
}

#[derive(Deserialize)]
struct Response {
    model: String,
    answers: Vec<NamedAnswer>,
    usage: Usage,
}

#[derive(Deserialize)]
struct NamedAnswer {
    name: String,
    #[serde(flatten)]
    answer: ResponseAnswer,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ResponseAnswer {
    Predicate {
        probability: f64,
    },
    Choice {
        choice: String,
        confidence: f64,
        probabilities: Vec<ChoiceProbability>,
    },
    Score {
        score: f64,
        confidence: f64,
        probabilities: Vec<ScoreProbability>,
    },
    Refusal,
}

#[derive(Deserialize)]
struct ChoiceProbability {
    value: String,
    probability: f64,
}

#[derive(Deserialize)]
struct ScoreProbability {
    value: usize,
    label: String,
    probability: f64,
}

pub(crate) fn decode(
    bytes: &[u8],
    request: &DecisionRequest,
) -> Result<DecisionResponse, DecisionError> {
    let response: Response =
        serde_json::from_slice(bytes).map_err(|_| DecisionError::Invalid(INVALID_SCHEMA))?;
    let mut named = BTreeMap::new();
    for answer in response.answers {
        if named.insert(answer.name, answer.answer).is_some() {
            return Err(DecisionError::Invalid(INVALID_NAMES));
        }
    }
    if !named.keys().eq(request.questions.keys()) {
        return Err(DecisionError::Invalid(INVALID_NAMES));
    }
    if named
        .values()
        .any(|answer| matches!(answer, ResponseAnswer::Refusal))
    {
        return Err(DecisionError::Refused);
    }
    let mut answers = BTreeMap::new();
    for (name, answer) in named {
        let question = &request.questions[&name];
        let answer = match (answer, &question.kind) {
            (ResponseAnswer::Predicate { probability }, QuestionType::Noul) => {
                Answer::Noul(NoulAnswer { noul: probability })
            }
            (
                ResponseAnswer::Choice {
                    choice,
                    confidence,
                    probabilities,
                },
                QuestionType::Choice,
            ) => {
                let mut values = BTreeMap::new();
                for probability in probabilities {
                    if values
                        .insert(probability.value, probability.probability)
                        .is_some()
                    {
                        return Err(DecisionError::Invalid(DUPLICATE_VALUE));
                    }
                }
                Answer::Choice(ChoiceAnswer {
                    choice,
                    confidence,
                    probabilities: values,
                })
            }
            (
                ResponseAnswer::Score {
                    score,
                    confidence,
                    probabilities,
                },
                QuestionType::Score,
            ) => {
                let levels = question.score_levels()?;
                let mut legend = BTreeMap::new();
                let mut values = BTreeMap::new();
                for probability in probabilities {
                    let level = levels.get(probability.value).ok_or(DecisionError::Invalid(
                        "score index is outside the requested levels",
                    ))?;
                    if probability.label != rendered_text(level)? {
                        return Err(DecisionError::Invalid(
                            "score label does not match the requested level",
                        ));
                    }
                    let index = probability.value.to_string();
                    if values
                        .insert(index.clone(), probability.probability)
                        .is_some()
                    {
                        return Err(DecisionError::Invalid(DUPLICATE_VALUE));
                    }
                    legend.insert(index, probability.label);
                }
                Answer::Score(ScoreAnswer {
                    score,
                    legend,
                    probabilities: values,
                    confidence,
                })
            }
            _ => {
                return Err(DecisionError::Invalid(
                    "answer type does not match the question",
                ));
            }
        };
        answers.insert(name, answer);
    }
    Ok(DecisionResponse {
        model: response.model,
        answers,
        usage: response.usage,
        cache_hit: false,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::{INVALID_NAMES, PREDICATE_CRITERIA, decode, encode};
    use crate::engine::DecisionError;
    use crate::question_set::{bounded_json, rendered_text};
    use crate::wire::{DecisionRequest, DecisionResponse, MAX_REQUEST_BYTES, MAX_STATE_CHARS};

    const REQUEST_MODEL: &str = "gpt-6-luna-decisions";
    const RESPONSE_MODEL: &str = "gpt-6-luna-decisions-version";

    pub(crate) fn request() -> DecisionRequest {
        serde_json::from_value(json!({
            "model": REQUEST_MODEL,
            "state": {"ticket": "The screen is broken.", "metadata": {"z": 2, "a": 1}},
            "questions": {
                "damaged": {
                    "type": "noul",
                    "instructions": {"task": "Does the customer report damage?", "context": ["Use the ticket"]},
                    "criteria": {"true": ["Physical damage"], "false": {"reason": "No damage"}}
                },
                "queue": {
                    "type": "choice",
                    "instructions": ["Select a queue"],
                    "criteria": {"shipping": null, "billing": {"handles": ["Refunds", "Payments"]}}
                },
                "urgency": {
                    "type": "score",
                    "instructions": "Rate urgency",
                    "criteria": ["low", {"priority": "medium"}, ["high"]]
                }
            }
        }))
        .unwrap()
    }

    pub(crate) fn encoded() -> Value {
        json!({
            "model": REQUEST_MODEL,
            "input": "{\n  \"ticket\": \"The screen is broken.\",\n  \"metadata\": {\n    \"z\": 2,\n    \"a\": 1\n  }\n}",
            "questions": [
                {
                    "type": "predicate", "name": "damaged",
                    "instructions": concat!(
                        "{\n  \"task\": \"Does the customer report damage?\",\n  \"context\": [\n    \"Use the ticket\"\n  ]\n}",
                        "\n\nCriteria (false and true outcomes):\n",
                        "{\n  \"true\": [\n    \"Physical damage\"\n  ],\n  \"false\": {\n    \"reason\": \"No damage\"\n  }\n}"
                    )
                },
                {
                    "type": "choice", "name": "queue", "instructions": "[\n  \"Select a queue\"\n]",
                    "choices": [
                        {"value": "shipping"},
                        {"value": "billing", "description": "{\n  \"handles\": [\n    \"Refunds\",\n    \"Payments\"\n  ]\n}"}
                    ]
                },
                {
                    "type": "score", "name": "urgency", "instructions": "Rate urgency",
                    "levels": [{"label": "low"}, {"label": "{\n  \"priority\": \"medium\"\n}"}, {"label": "[\n  \"high\"\n]"}]
                }
            ]
        })
    }

    pub(crate) fn response() -> Value {
        json!({
            "model": RESPONSE_MODEL,
            "answers": [
                {"type": "predicate", "name": "damaged", "probability": 0.95},
                {
                    "type": "choice", "name": "queue", "choice": "shipping", "confidence": 0.81,
                    "probabilities": [{"value": "billing", "probability": 0.19}, {"value": "shipping", "probability": 0.81}]
                },
                {
                    "type": "score", "name": "urgency", "score": 1.3, "confidence": 0.62,
                    "probabilities": [
                        {"value": 0, "label": "low", "probability": 0.08},
                        {"value": 1, "label": "{\n  \"priority\": \"medium\"\n}", "probability": 0.54},
                        {"value": 2, "label": "[\n  \"high\"\n]", "probability": 0.38}
                    ]
                }
            ],
            "usage": {
                "input_tokens": 96, "output_tokens": 7, "total_tokens": 103,
                "input_tokens_details": {"cache_write_tokens": 0, "cached_tokens": 64},
                "output_tokens_details": {"reasoning_tokens": 3}
            },
            "cache_hit": true
        })
    }

    pub(crate) fn normalized() -> DecisionResponse {
        serde_json::from_value(json!({
            "model": RESPONSE_MODEL,
            "answers": {
                "damaged": {"type": "noul", "noul": 0.95},
                "queue": {"type": "choice", "choice": "shipping", "confidence": 0.81, "probabilities": {"billing": 0.19, "shipping": 0.81}},
                "urgency": {
                    "type": "score", "score": 1.3, "confidence": 0.62,
                    "probabilities": {"0": 0.08, "1": 0.54, "2": 0.38},
                    "legend": {"0": "low", "1": "{\n  \"priority\": \"medium\"\n}", "2": "[\n  \"high\"\n]"}
                }
            },
            "usage": {"input_tokens": 96, "output_tokens": 7}
        }))
        .unwrap()
    }

    fn validated(value: &Value) -> Result<DecisionResponse, DecisionError> {
        let request = request();
        let response = decode(&serde_json::to_vec(value).unwrap(), &request)?;
        response.validate_for(&request)?;
        Ok(response)
    }

    #[test]
    fn encodes_native_text_questions_fixture() {
        let request = request();
        request.validate().unwrap();
        let bytes = encode(&request).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), encoded());
        assert_eq!(bytes, encode(&request).unwrap());
    }

    #[test_case(&["zeta-review", "alpha-build", "none"]; "ranked_skill_keys")]
    #[test_case(&["candidate_f9", "candidate_02", "candidate_a1", "none"]; "opaque_candidate_keys")]
    fn preserves_choice_ranking_and_none_last(keys: &[&str]) {
        let mut request = request();
        request.questions.get_mut("queue").unwrap().criteria = Some(Value::Object(
            keys.iter()
                .map(|key| ((*key).to_owned(), Value::Null))
                .collect(),
        ));
        let encoded: Value = serde_json::from_slice(&encode(&request).unwrap()).unwrap();
        let values: Vec<_> = encoded["questions"][1]["choices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|choice| choice["value"].as_str().unwrap())
            .collect();
        assert_eq!(values, keys);
    }

    #[test_case("/state", "/input"; "state")]
    #[test_case("/questions/urgency/instructions", "/questions/2/instructions"; "instructions")]
    #[test_case("/questions/queue/criteria/billing", "/questions/1/choices/1/description"; "choice_description")]
    #[test_case("/questions/urgency/criteria/1", "/questions/2/levels/1/label"; "score_label")]
    fn preserves_nested_object_order(source: &str, target: &str) {
        let mut request = serde_json::to_value(request()).unwrap();
        *request.pointer_mut(source).unwrap() = json!({"z": [{"right": 1, "left": 2}], "a": 3});
        let request = serde_json::from_value(request).unwrap();
        let encoded: Value = serde_json::from_slice(&encode(&request).unwrap()).unwrap();
        assert_eq!(
            encoded.pointer(target).unwrap().as_str().unwrap(),
            "{\n  \"z\": [\n    {\n      \"right\": 1,\n      \"left\": 2\n    }\n  ],\n  \"a\": 3\n}"
        );
    }

    #[test_case(json!("Keep\nthis text unchanged"), "Keep\nthis text unchanged"; "raw_string")]
    #[test_case(json!([{"type": "input_image", "image_url": "not an image request"}]), "[\n  {\n    \"type\": \"input_image\",\n    \"image_url\": \"not an image request\"\n  }\n]"; "arrays_are_text_not_messages")]
    fn state_is_always_text(state: Value, expected: &str) {
        let mut request = request();
        request.state = state;
        let encoded: Value = serde_json::from_slice(&encode(&request).unwrap()).unwrap();
        assert_eq!(encoded["input"], expected);
    }

    #[test]
    fn rendered_input_obeys_character_limit() {
        let mut request = request();
        request.state = json!(vec![0; MAX_STATE_CHARS / 3]);
        request.validate().unwrap();
        assert!(matches!(encode(&request), Err(DecisionError::Rejected(_))));
    }

    #[test]
    fn unicode_input_limit_counts_characters_not_bytes() {
        let mut request = request();
        let text = "界".repeat(MAX_STATE_CHARS);
        request.state = json!(text);
        request.validate().unwrap();
        let encoded: Value = serde_json::from_slice(&encode(&request).unwrap()).unwrap();
        assert_eq!(encoded["input"], text);
    }

    #[test_case(None; "no_criteria")]
    #[test_case(Some(json!({"true": "yes"})); "true_only")]
    #[test_case(Some(json!({"false": ["no"]})); "false_only")]
    fn predicate_preserves_optional_criteria(criteria: Option<Value>) {
        let mut request = request();
        let question = request.questions.get_mut("damaged").unwrap();
        question.instructions = json!("Check damage");
        question.criteria = criteria.clone();
        let encoded: Value = serde_json::from_slice(&encode(&request).unwrap()).unwrap();
        let expected = criteria.map_or_else(
            || "Check damage".to_owned(),
            |criteria| {
                format!(
                    "Check damage{PREDICATE_CRITERIA}{}",
                    rendered_text(&criteria).unwrap()
                )
            },
        );
        assert_eq!(encoded["questions"][0]["type"], "predicate");
        assert_eq!(encoded["questions"][0]["instructions"], expected);
    }

    #[test]
    fn boolean_spelling_choice_values_remain_strings() {
        let mut request = request();
        request.questions.get_mut("queue").unwrap().criteria =
            Some(json!({"true": ["yes"], "false": "no"}));
        let encoded: Value = serde_json::from_slice(&encode(&request).unwrap()).unwrap();
        assert_eq!(
            encoded["questions"][1]["choices"],
            json!([
                {"value": "true", "description": "[\n  \"yes\"\n]"},
                {"value": "false", "description": "no"}
            ])
        );
        let mut response = response();
        response["answers"][1]["choice"] = json!("true");
        response["answers"][1]["probabilities"] = json!([
            {"value": "false", "probability": 0.19}, {"value": "true", "probability": 0.81}
        ]);
        let response = decode(&serde_json::to_vec(&response).unwrap(), &request).unwrap();
        response.validate_for(&request).unwrap();
    }

    #[test]
    fn rejects_singleton_choice_without_changing_typesafe_contract() {
        let mut request = request();
        request.questions.get_mut("queue").unwrap().criteria = Some(json!({"only": null}));
        request.validate().unwrap();
        assert!(matches!(encode(&request), Err(DecisionError::Rejected(_))));
    }

    #[test_case(false; "oversized_source")]
    #[test_case(true; "oversized_rendered_request")]
    fn bounds_request_before_and_after_rendering(expansion: bool) {
        let mut request = request();
        let content = if expansion {
            json!(["\\".repeat(MAX_REQUEST_BYTES / 3)])
        } else {
            json!("x".repeat(MAX_REQUEST_BYTES))
        };
        request.questions.get_mut("damaged").unwrap().instructions = content;
        if expansion {
            bounded_json(&request, MAX_REQUEST_BYTES).unwrap();
        }
        assert!(matches!(encode(&request), Err(DecisionError::Rejected(_))));
    }

    #[test_case(false; "wire_order")]
    #[test_case(true; "reordered_answers_and_probabilities")]
    fn normalizes_fixture_and_preserves_usage_without_remote_cache_hit(reverse: bool) {
        let mut response = response();
        if reverse {
            for answer in response["answers"].as_array_mut().unwrap() {
                if let Some(probabilities) = answer.get_mut("probabilities") {
                    probabilities.as_array_mut().unwrap().reverse();
                }
            }
            response["answers"].as_array_mut().unwrap().reverse();
        }
        assert_eq!(validated(&response).unwrap(), normalized());
    }

    #[test_case("/answers/0/name", json!(null); "null_name")]
    #[test_case("/answers/0/name", json!("Damaged"); "case_sensitive_name")]
    #[test_case("/answers/0/name", json!("unknown"); "unknown_name")]
    #[test_case("/answers/0/name", json!("queue"); "duplicate_name")]
    #[test_case("/answers/0/type", json!("noul"); "typesafe_answer_not_accepted")]
    #[test_case("/answers/1", json!({"type": "predicate", "name": "queue", "probability": 0.9}); "wrong_kind")]
    #[test_case("/answers/0/probability", json!(-0.1); "negative_predicate")]
    #[test_case("/answers/0/probability", json!(1.1); "predicate_above_one")]
    #[test_case("/answers/0/probability", json!(null); "null_predicate")]
    #[test_case("/answers/1/choice", json!(true); "boolean_choice")]
    #[test_case("/answers/1/choice", json!(1); "numeric_choice")]
    #[test_case("/answers/1/choice", json!("other"); "unrequested_choice")]
    #[test_case("/answers/1/probabilities/0/value", json!(false); "boolean_probability_value")]
    #[test_case("/answers/1/probabilities/0/value", json!(0); "numeric_probability_value")]
    #[test_case("/answers/1/probabilities/0/value", json!("shipping"); "duplicate_choice_probability")]
    #[test_case("/answers/1/probabilities/0/value", json!("other"); "unknown_choice_probability")]
    #[test_case("/answers/1/probabilities/0/probability", json!(0.5); "unnormalized_choice")]
    #[test_case("/answers/1/probabilities/0/probability", json!(-0.1); "negative_choice_probability")]
    #[test_case("/answers/1/probabilities", json!([]); "missing_choice_probabilities")]
    #[test_case("/answers/1/confidence", json!(1.1); "choice_confidence")]
    #[test_case("/answers/2/confidence", json!(-0.1); "score_confidence")]
    #[test_case("/answers/2/score", json!(0.5); "wrong_expected_score")]
    #[test_case("/answers/2/score", json!(3); "out_of_range_score")]
    #[test_case("/answers/2/probabilities/0/label", json!("wrong"); "mismatched_score_label")]
    #[test_case("/answers/2/probabilities/0/value", json!(3); "out_of_range_score_index")]
    #[test_case("/answers/2/probabilities/0/value", json!(-1); "negative_score_index")]
    #[test_case("/answers/2/probabilities/0/value", json!(0.5); "fractional_score_index")]
    #[test_case("/answers/2/probabilities/0/value", json!("0"); "string_score_index")]
    #[test_case("/answers/2/probabilities/0/value", json!(false); "boolean_score_index")]
    #[test_case("/answers/2/probabilities/1", json!({"value": 0, "label": "low", "probability": 0.54}); "duplicate_score_index")]
    #[test_case("/answers/2/probabilities", json!([]); "missing_score_probabilities")]
    #[test_case("/answers/2/probabilities/0/probability", json!(1.0); "unnormalized_score")]
    #[test_case("/usage/input_tokens", json!(-1); "negative_usage")]
    #[test_case("/usage/output_tokens", json!(null); "missing_usage")]
    fn rejects_malformed_answers(pointer: &str, replacement: Value) {
        let mut response = response();
        *response.pointer_mut(pointer).unwrap() = replacement;
        assert!(matches!(
            validated(&response),
            Err(DecisionError::Invalid(_))
        ));
    }

    #[test_case(false; "missing_answer")]
    #[test_case(true; "extra_duplicate_answer")]
    fn requires_exact_answer_names(extra: bool) {
        let mut response = response();
        let answers = response["answers"].as_array_mut().unwrap();
        if extra {
            answers.push(answers[0].clone());
        } else {
            answers.pop();
        }
        assert_eq!(
            validated(&response),
            Err(DecisionError::Invalid(INVALID_NAMES))
        );
    }

    #[test_case(0; "predicate_refused")]
    #[test_case(1; "choice_refused")]
    #[test_case(2; "score_refused")]
    fn refusal_fails_the_entire_request(index: usize) {
        let mut response = response();
        let name = response["answers"][index]["name"].clone();
        response["answers"][index] = json!({"type": "refusal", "name": name});
        assert_eq!(validated(&response), Err(DecisionError::Refused));
    }

    #[test_case("{\"answers\": [], \"answers\": []}"; "duplicate_top_level_field")]
    #[test_case("{\"model\":\"m\",\"answers\":[{\"name\":\"damaged\",\"name\":\"damaged\",\"type\":\"predicate\",\"probability\":0.1}],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}"; "duplicate_name_field")]
    #[test_case("{\"model\":\"m\",\"answers\":[{\"name\":\"damaged\",\"type\":\"predicate\",\"probability\":1e999}],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}"; "nonfinite_probability")]
    fn rejects_invalid_json_schema(body: &str) {
        assert!(matches!(
            decode(body.as_bytes(), &request()),
            Err(DecisionError::Invalid(_))
        ));
    }
}
