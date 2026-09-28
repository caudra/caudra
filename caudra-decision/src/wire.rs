use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::marker::PhantomData;

use serde::de::{Error, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::engine::DecisionError;
use crate::question_set::bounded_json;

pub const MAX_QUESTIONS: usize = 64;
pub const MAX_CHOICE_OPTIONS: usize = 100;
pub const MAX_SCORE_LEVELS: usize = 10;
pub const MAX_TOTAL_OPTIONS: usize = 512;
pub const MAX_STATE_CHARS: usize = 50_000;
pub const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const ROUNDING_ERROR: f64 = 0.000_05;
const FLOAT_TOLERANCE: f64 = 0.000_001;

pub type Questions = BTreeMap<String, Question>;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum QuestionType {
    Noul,
    Choice,
    Score,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    #[serde(rename = "type")]
    pub kind: QuestionType,
    pub instructions: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criteria: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
}

impl Question {
    pub fn validate(&self) -> Result<(), DecisionError> {
        if !matches!(
            self.instructions,
            Value::String(_) | Value::Array(_) | Value::Object(_)
        ) {
            return Err(DecisionError::Rejected(
                "instructions must be a string, object, or array",
            ));
        }
        if let Some(labels) = &self.labels {
            let valid = self.kind == QuestionType::Noul
                && labels.len() == 2
                && labels
                    .get("false")
                    .zip(labels.get("true"))
                    .is_some_and(|(no, yes)| {
                        !no.trim().is_empty() && !yes.trim().is_empty() && no.trim() != yes.trim()
                    });
            if !valid {
                return Err(DecisionError::Rejected(
                    "noul labels must name false and true with distinct nonempty strings",
                ));
            }
        }
        match self.kind {
            QuestionType::Noul => {
                if let Some(criteria) = &self.criteria
                    && !criteria.as_object().is_some_and(|criteria| {
                        criteria.keys().all(|key| {
                            key.eq_ignore_ascii_case("false") || key.eq_ignore_ascii_case("true")
                        })
                    })
                {
                    return Err(DecisionError::Rejected(
                        "noul criteria must be an object keyed by false or true",
                    ));
                }
            }
            QuestionType::Choice => {
                let keys = self.choice_keys()?;
                if keys.is_empty() || keys.len() > MAX_CHOICE_OPTIONS {
                    return Err(DecisionError::Rejected(
                        "choice option count is outside the supported limits",
                    ));
                }
            }
            QuestionType::Score => {
                let Some(levels) = self.criteria.as_ref().and_then(Value::as_array) else {
                    return Err(DecisionError::Rejected("score criteria must be an array"));
                };
                if levels.is_empty()
                    || levels.len() > MAX_SCORE_LEVELS
                    || levels.iter().any(Value::is_null)
                {
                    return Err(DecisionError::Rejected(
                        "score requires nonnull levels within the supported limits",
                    ));
                }
            }
        }
        Ok(())
    }

    fn choice_keys(&self) -> Result<BTreeSet<String>, DecisionError> {
        match self.criteria.as_ref() {
            Some(Value::Object(options)) => Ok(options.keys().cloned().collect()),
            Some(Value::Array(options)) => {
                let keys = options
                    .iter()
                    .map(scalar_key)
                    .collect::<Option<BTreeSet<_>>>()
                    .ok_or(DecisionError::Rejected(
                        "choice labels must be scalar values",
                    ))?;
                if keys.len() != options.len() {
                    return Err(DecisionError::Rejected("choice labels must be unique"));
                }
                Ok(keys)
            }
            _ => Err(DecisionError::Rejected(
                "choice criteria must be an object or array",
            )),
        }
    }

    fn option_count(&self) -> usize {
        match &self.criteria {
            Some(Value::Object(options)) if self.kind == QuestionType::Choice => options.len(),
            Some(Value::Array(options)) => options.len(),
            _ => 0,
        }
    }
}

fn scalar_key(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Null | Value::Bool(_) | Value::Number(_) => Some(value.to_string()),
        _ => None,
    }
}

pub fn validate_questions(questions: &Questions) -> Result<(), DecisionError> {
    if questions.is_empty() || questions.len() > MAX_QUESTIONS {
        return Err(DecisionError::Rejected(
            "question count is outside the supported limits",
        ));
    }
    let mut total_options = 0;
    for (id, question) in questions {
        if id.trim().is_empty() {
            return Err(DecisionError::Rejected("question ids must not be empty"));
        }
        question.validate()?;
        total_options += question.option_count();
    }
    if total_options > MAX_TOTAL_OPTIONS {
        return Err(DecisionError::Rejected(
            "total answer option count exceeds the supported limit",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRequest {
    pub model: String,
    pub state: Value,
    #[serde(deserialize_with = "unique_map")]
    pub questions: Questions,
}

impl DecisionRequest {
    pub fn validate(&self) -> Result<(), DecisionError> {
        if self.model.trim().is_empty() {
            return Err(DecisionError::Rejected("model must not be empty"));
        }
        let state_len = match &self.state {
            Value::String(state) => state.chars().count(),
            Value::Array(_) | Value::Object(_) => {
                let bytes = bounded_json(&self.state, MAX_REQUEST_BYTES)?;
                String::from_utf8(bytes)
                    .map_err(|_| DecisionError::Rejected("state is not UTF-8"))?
                    .chars()
                    .count()
            }
            _ => {
                return Err(DecisionError::Rejected(
                    "state must be a string, object, or array",
                ));
            }
        };
        if state_len > MAX_STATE_CHARS {
            return Err(DecisionError::Rejected("state exceeds the character limit"));
        }
        validate_questions(&self.questions)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AnswerMetadata {
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<Action>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Action {
    pub act_probability: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NoulAnswer {
    pub noul: f64,
    #[serde(flatten)]
    pub metadata: AnswerMetadata,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ChoiceAnswer {
    pub choice: Value,
    #[serde(deserialize_with = "unique_map")]
    pub probabilities: BTreeMap<String, f64>,
    #[serde(flatten)]
    pub metadata: AnswerMetadata,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ScoreAnswer {
    pub score: f64,
    #[serde(deserialize_with = "unique_map")]
    pub legend: BTreeMap<String, Value>,
    #[serde(deserialize_with = "unique_map")]
    pub probabilities: BTreeMap<String, f64>,
    #[serde(flatten)]
    pub metadata: AnswerMetadata,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul(NoulAnswer),
    Choice(ChoiceAnswer),
    Score(ScoreAnswer),
}

impl Answer {
    pub fn kind(&self) -> QuestionType {
        match self {
            Self::Noul(_) => QuestionType::Noul,
            Self::Choice(_) => QuestionType::Choice,
            Self::Score(_) => QuestionType::Score,
        }
    }

    pub fn metadata(&self) -> &AnswerMetadata {
        match self {
            Self::Noul(answer) => &answer.metadata,
            Self::Choice(answer) => &answer.metadata,
            Self::Score(answer) => &answer.metadata,
        }
    }

    fn validate(&self, question: &Question) -> Result<(), DecisionError> {
        if self.kind() != question.kind {
            return Err(DecisionError::Invalid(
                "answer type does not match the question",
            ));
        }
        let metadata = self.metadata();
        probability(metadata.confidence)?;
        if let Some(confidence) = metadata.answer_confidence {
            probability(confidence)?;
        }
        if let Some(action) = &metadata.action {
            probability(action.act_probability)?;
        }
        match self {
            Self::Noul(answer) => probability(answer.noul),
            Self::Choice(answer) => {
                let keys = question.choice_keys()?;
                distribution(&answer.probabilities, &keys)?;
                if !scalar_key(&answer.choice).is_some_and(|key| keys.contains(&key)) {
                    return Err(DecisionError::Invalid(
                        "choice is not one of the requested options",
                    ));
                }
                Ok(())
            }
            Self::Score(answer) => {
                let levels = question
                    .criteria
                    .as_ref()
                    .and_then(Value::as_array)
                    .ok_or(DecisionError::Rejected("score criteria must be an array"))?;
                let legend: BTreeMap<_, _> = levels
                    .iter()
                    .enumerate()
                    .map(|(index, level)| (index.to_string(), level.clone()))
                    .collect();
                if answer.legend != legend {
                    return Err(DecisionError::Invalid(
                        "score legend does not match the requested levels",
                    ));
                }
                distribution(&answer.probabilities, &legend.keys().cloned().collect())?;
                let maximum = levels.len().saturating_sub(1) as f64;
                if !answer.score.is_finite() || !(0.0..=maximum).contains(&answer.score) {
                    return Err(DecisionError::Invalid(
                        "score is outside the requested levels",
                    ));
                }
                let expected: f64 = (0..levels.len())
                    .map(|index| index as f64 * answer.probabilities[&index.to_string()])
                    .sum();
                let tolerance =
                    ROUNDING_ERROR * (1.0 + levels.len() as f64 * maximum) + FLOAT_TOLERANCE;
                if (answer.score - expected).abs() > tolerance {
                    return Err(DecisionError::Invalid(
                        "score does not match its probability distribution",
                    ));
                }
                Ok(())
            }
        }
    }
}

fn probability(value: f64) -> Result<(), DecisionError> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(DecisionError::Invalid(
            "probabilities and confidence must be finite values between zero and one",
        ))
    }
}

fn distribution(
    values: &BTreeMap<String, f64>,
    keys: &BTreeSet<String>,
) -> Result<(), DecisionError> {
    if !values.keys().eq(keys.iter()) {
        return Err(DecisionError::Invalid(
            "probability keys do not match the requested options",
        ));
    }
    for &value in values.values() {
        probability(value)?;
    }
    let tolerance = values.len() as f64 * ROUNDING_ERROR + FLOAT_TOLERANCE;
    if (values.values().sum::<f64>() - 1.0).abs() > tolerance {
        return Err(DecisionError::Invalid("probabilities must sum to one"));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DecisionResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(deserialize_with = "unique_map")]
    pub answers: BTreeMap<String, Answer>,
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<Value>,
    #[serde(skip)]
    pub cache_hit: bool,
}

impl DecisionResponse {
    pub fn validate_for(&self, request: &DecisionRequest) -> Result<(), DecisionError> {
        request.validate()?;
        if !self.answers.keys().eq(request.questions.keys()) {
            return Err(DecisionError::Invalid(
                "answer ids do not match the requested questions",
            ));
        }
        for (id, answer) in &self.answers {
            answer.validate(&request.questions[id])?;
        }
        Ok(())
    }
}

fn unique_map<'de, D: Deserializer<'de>, V: Deserialize<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, V>, D::Error> {
    struct UniqueMap<V>(PhantomData<V>);

    impl<'de, V: Deserialize<'de>> Visitor<'de> for UniqueMap<V> {
        type Value = BTreeMap<String, V>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object with unique keys")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, V>()? {
                if values.insert(key, value).is_some() {
                    return Err(A::Error::custom("duplicate object key"));
                }
            }
            Ok(values)
        }
    }

    deserializer.deserialize_map(UniqueMap(PhantomData))
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::{
        Answer, DecisionRequest, DecisionResponse, MAX_CHOICE_OPTIONS, MAX_QUESTIONS,
        MAX_SCORE_LEVELS, MAX_STATE_CHARS, Question,
    };
    use crate::DecisionError;

    pub(crate) fn request() -> DecisionRequest {
        serde_json::from_value(json!({
            "model": "english",
            "state": {"command": "git status"},
            "questions": {"writes": {"type": "noul", "instructions": "Does this modify files?"}}
        }))
        .unwrap()
    }

    pub(crate) fn response() -> DecisionResponse {
        serde_json::from_value(json!({
            "model": "laya-rl-agent",
            "answers": {"writes": {"type": "noul", "noul": 0.1, "confidence": 0.9}},
            "usage": {"input_tokens": 17, "output_tokens": 0}
        }))
        .unwrap()
    }

    #[test_case(json!({"type":"noul", "instructions":{"task":"check"}, "criteria":{"true":["yes"], "false":{"text":"no"}}}); "structured_noul")]
    #[test_case(json!({"type":"choice", "instructions":["pick"], "criteria":{"a":{"description":"first"}, "b":["second"]}}); "structured_choice")]
    #[test_case(json!({"type":"choice", "instructions":"pick", "criteria":["first", "second"]}); "choice_array")]
    #[test_case(json!({"type":"choice", "instructions":"pick", "criteria":[0, true, null]}); "scalar_choice_array")]
    #[test_case(json!({"type":"score", "instructions":"rate", "criteria":["low", {"level":"high"}]} ); "structured_score")]
    fn question_round_trip(value: Value) {
        let question: Question = serde_json::from_value(value.clone()).unwrap();
        question.validate().unwrap();
        assert_eq!(serde_json::to_value(question).unwrap(), value);
    }

    #[test_case(json!({"type":"noul", "instructions":"check", "criteria":"yes"}); "noul_string_criteria")]
    #[test_case(json!({"type":"noul", "instructions":"check", "criteria":{"yes":"yes"}}); "noul_unknown_key")]
    #[test_case(json!({"type":"noul", "instructions":null}); "null_instructions")]
    #[test_case(json!({"type":"noul", "instructions":"check", "labels":{"true":"yes","false":" yes "}}); "duplicate_labels")]
    #[test_case(json!({"type":"choice", "instructions":"pick", "criteria":[]}); "empty_choice")]
    #[test_case(json!({"type":"choice", "instructions":"pick", "criteria":["a","a"]}); "duplicate_choice")]
    #[test_case(json!({"type":"choice", "instructions":"pick", "criteria":[["nested"]]}); "nested_choice")]
    #[test_case(json!({"type":"score", "instructions":"rate", "criteria":{"a":"low"}}); "score_object")]
    #[test_case(json!({"type":"score", "instructions":"rate", "criteria":[null]}); "null_score_level")]
    fn rejects_invalid_question(value: Value) {
        let question: Question = serde_json::from_value(value).unwrap();
        assert!(matches!(
            question.validate(),
            Err(DecisionError::Rejected(_))
        ));
    }

    #[test_case("noul", json!({"type":"noul", "noul":0.7312, "confidence":0.7312}), None; "jev_noul")]
    #[test_case("choice", json!({"type":"choice", "choice":"a", "probabilities":{"a":0.7,"b":0.3}, "confidence":0.1187}), Some(json!({"a":"first","b":"second"})); "jev_choice")]
    #[test_case("score", json!({"type":"score", "score":0.3, "legend":{"0":"low","1":"high"}, "probabilities":{"0":0.7,"1":0.3}, "confidence":0.1187}), Some(json!(["low","high"])); "jev_score")]
    fn answer_round_trip(kind: &str, answer: Value, criteria: Option<Value>) {
        let mut request = request();
        request.questions.insert(
            "writes".into(),
            serde_json::from_value(json!({"type":kind,"instructions":"check","criteria":criteria}))
                .unwrap(),
        );
        let mut fixture =
            json!({"answers":{"writes":answer}, "usage":{"input_tokens":17,"output_tokens":0}});
        let response: DecisionResponse = serde_json::from_value(fixture.clone()).unwrap();
        response.validate_for(&request).unwrap();
        assert_eq!(serde_json::to_value(response).unwrap(), fixture);
        fixture["routing"] = json!({"model":"english", "reason":"default"});
        fixture["answers"]["writes"]["answer_confidence"] = json!(0.7312);
        fixture["answers"]["writes"]["action"] = json!({"act_probability":0.95});
        let laya: DecisionResponse = serde_json::from_value(fixture.clone()).unwrap();
        laya.validate_for(&request).unwrap();
        assert_eq!(serde_json::to_value(laya).unwrap(), fixture);
    }

    #[test_case(-0.1; "negative")]
    #[test_case(1.1; "above_one")]
    #[test_case(f64::NAN; "nan")]
    #[test_case(f64::INFINITY; "infinity")]
    fn rejects_invalid_probability(value: f64) {
        let mut response = response();
        let Answer::Noul(answer) = response.answers.get_mut("writes").unwrap() else {
            unreachable!()
        };
        answer.noul = value;
        assert!(matches!(
            response.validate_for(&request()),
            Err(DecisionError::Invalid(_))
        ));
    }

    #[test_case("/answers/writes/confidence", json!(1.1); "confidence")]
    #[test_case("/answers/writes/answer_confidence", json!(-0.1); "answer_confidence")]
    #[test_case("/answers/writes/action", json!({"act_probability":-0.1}); "action")]
    #[test_case("/answers", json!({}); "missing_answer")]
    #[test_case("/answers/extra", json!({"type":"noul","noul":0.1,"confidence":0.9}); "unexpected_answer")]
    #[test_case("/answers/writes", json!({"type":"choice","choice":"a","probabilities":{"a":1.0},"confidence":1.0}); "wrong_answer_type")]
    fn rejects_invalid_response(pointer: &str, value: Value) {
        let mut fixture = serde_json::to_value(response()).unwrap();
        let (parent, key) = pointer.rsplit_once('/').unwrap();
        fixture.pointer_mut(parent).unwrap()[key] = value;
        let response: DecisionResponse = serde_json::from_value(fixture).unwrap();
        assert!(matches!(
            response.validate_for(&request()),
            Err(DecisionError::Invalid(_))
        ));
    }

    #[test_case(json!({"a":0.2,"b":0.2}), "a"; "not_normalized")]
    #[test_case(json!({"a":1.0}), "a"; "missing_probability")]
    #[test_case(json!({"a":0.5,"b":0.5}), "c"; "unknown_choice")]
    #[test_case(json!({"a":-0.1,"b":1.1}), "a"; "invalid_range")]
    fn rejects_invalid_choice(probabilities: Value, choice: &str) {
        let mut request = request();
        request.questions.insert(
            "writes".into(),
            serde_json::from_value(
                json!({"type":"choice","instructions":"pick","criteria":["a","b"]}),
            )
            .unwrap(),
        );
        let mut response = response();
        response.answers.insert("writes".into(), serde_json::from_value(json!({"type":"choice","choice":choice,"confidence":0.5,"probabilities":probabilities})).unwrap());
        assert!(matches!(
            response.validate_for(&request),
            Err(DecisionError::Invalid(_))
        ));
    }

    #[test]
    fn accepts_rounded_distributions() {
        let keys = ["a", "b", "c"].map(String::from).into_iter().collect();
        let probabilities = ["a", "b", "c"]
            .map(|key| (key.into(), 0.3333))
            .into_iter()
            .collect();
        super::distribution(&probabilities, &keys).unwrap();
    }

    #[test_case("score", json!(-0.1); "negative_score")]
    #[test_case("score", json!(1.1); "above_maximum")]
    #[test_case("score", json!(0.8); "inconsistent_expected_score")]
    #[test_case("legend", json!({"0":"high","1":"low"}); "wrong_legend")]
    #[test_case("probabilities", json!({"0":1.0}); "missing_level")]
    fn rejects_invalid_score(field: &str, value: Value) {
        let question: Question = serde_json::from_value(
            json!({"type":"score","instructions":"rate","criteria":["low","high"]}),
        )
        .unwrap();
        let mut fixture = json!({"type":"score","score":0.3,"legend":{"0":"low","1":"high"},"probabilities":{"0":0.7,"1":0.3},"confidence":0.1187});
        fixture[field] = value;
        let answer: Answer = serde_json::from_value(fixture).unwrap();
        assert!(matches!(
            answer.validate(&question),
            Err(DecisionError::Invalid(_))
        ));
    }

    #[test_case("choice", MAX_CHOICE_OPTIONS; "choice_limit")]
    #[test_case("score", MAX_SCORE_LEVELS; "score_limit")]
    fn enforces_option_limits(kind: &str, limit: usize) {
        let mut question: Question = serde_json::from_value(
            json!({"type":kind,"instructions":"pick","criteria":(0..limit).collect::<Vec<_>>()}),
        )
        .unwrap();
        question.validate().unwrap();
        question
            .criteria
            .as_mut()
            .unwrap()
            .as_array_mut()
            .unwrap()
            .push(json!(limit));
        assert!(question.validate().is_err());
    }

    #[test]
    fn enforces_state_and_question_limits() {
        let mut request = request();
        request.state = json!("é".repeat(MAX_STATE_CHARS));
        request.validate().unwrap();
        request.state = json!("é".repeat(MAX_STATE_CHARS + 1));
        assert!(request.validate().is_err());
        request.state = json!("ok");
        let question = request.questions.values().next().unwrap().clone();
        request.questions = (0..MAX_QUESTIONS)
            .map(|index| (index.to_string(), question.clone()))
            .collect();
        request.validate().unwrap();
        request.questions.insert("extra".into(), question);
        assert!(request.validate().is_err());
    }

    #[test_case(r#"{"answers":{},"usage":{"input_tokens":-1,"output_tokens":0}}"#; "negative_usage")]
    #[test_case(r#"{"answers":{},"usage":{}}"#; "missing_usage_fields")]
    #[test_case(r#"{"answers":{"writes":{"type":"noul","noul":0.1}} ,"usage":{"input_tokens":0,"output_tokens":0}}"#; "missing_confidence")]
    #[test_case(r#"{"answers":{"writes":{"type":"noul","noul":0.1,"confidence":0.9},"writes":{"type":"noul","noul":0.9,"confidence":0.9}},"usage":{"input_tokens":0,"output_tokens":0}}"#; "duplicate_answer")]
    #[test_case(r#"{"answers":{"writes":{"type":"choice","choice":"a","probabilities":{"a":0.1,"a":1.0},"confidence":1.0}},"usage":{"input_tokens":0,"output_tokens":0}}"#; "duplicate_probability")]
    fn rejects_invalid_schema(fixture: &str) {
        assert!(serde_json::from_str::<DecisionResponse>(fixture).is_err());
    }
}
