use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::marker::PhantomData;

use serde::de::{Error, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::engine::DecisionError;
use crate::question_set::bounded_json;

pub const MAX_QUESTIONS: usize = 64;
pub const MAX_CHOICE_OPTIONS: usize = 255;
pub const MIN_SCORE_LEVELS: usize = 2;
pub const MAX_SCORE_LEVELS: usize = 10;
pub const MAX_TOTAL_OPTIONS: usize = 512;
pub const MAX_STATE_CHARS: usize = 50_000;
pub const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const NOUL_CRITERIA_KEYS: [&str; 2] = ["false", "true"];
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
}

impl Question {
    pub fn validate(&self) -> Result<(), DecisionError> {
        if !is_content(&self.instructions) {
            return Err(DecisionError::Rejected(
                "instructions must be a string, object, or array",
            ));
        }
        match self.kind {
            QuestionType::Noul => {
                if let Some(criteria) = &self.criteria
                    && !criteria.as_object().is_some_and(|criteria| {
                        criteria.iter().all(|(key, description)| {
                            NOUL_CRITERIA_KEYS.contains(&key.as_str()) && is_content(description)
                        })
                    })
                {
                    return Err(DecisionError::Rejected(
                        "noul criteria must map true or false to a string, object, or array",
                    ));
                }
            }
            QuestionType::Choice => {
                let options = self.choice_options()?;
                if options.is_empty() || options.len() > MAX_CHOICE_OPTIONS {
                    return Err(DecisionError::Rejected(
                        "choice option count is outside the supported limits",
                    ));
                }
                if !options
                    .values()
                    .all(|description| description.is_null() || is_content(description))
                {
                    return Err(DecisionError::Rejected(
                        "choice descriptions must be a string, object, array, or null",
                    ));
                }
            }
            QuestionType::Score => {
                let levels = self.score_levels()?;
                if !(MIN_SCORE_LEVELS..=MAX_SCORE_LEVELS).contains(&levels.len())
                    || !levels.iter().all(is_content)
                {
                    return Err(DecisionError::Rejected(
                        "score requires string, object, or array levels within the supported limits",
                    ));
                }
            }
        }
        Ok(())
    }

    fn choice_options(&self) -> Result<&Map<String, Value>, DecisionError> {
        self.criteria
            .as_ref()
            .and_then(Value::as_object)
            .ok_or(DecisionError::Rejected("choice criteria must be an object"))
    }

    fn score_levels(&self) -> Result<&[Value], DecisionError> {
        self.criteria
            .as_ref()
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .ok_or(DecisionError::Rejected("score criteria must be an array"))
    }

    fn option_count(&self) -> usize {
        match self.kind {
            QuestionType::Noul => 0,
            QuestionType::Choice => self.choice_options().map_or(0, Map::len),
            QuestionType::Score => self.score_levels().map_or(0, <[Value]>::len),
        }
    }
}

fn is_content(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Array(_) | Value::Object(_))
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
pub struct NoulAnswer {
    pub noul: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ChoiceAnswer {
    pub choice: String,
    #[serde(deserialize_with = "unique_map")]
    pub probabilities: BTreeMap<String, f64>,
    pub confidence: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ScoreAnswer {
    pub score: f64,
    #[serde(deserialize_with = "unique_map")]
    pub legend: BTreeMap<String, String>,
    #[serde(deserialize_with = "unique_map")]
    pub probabilities: BTreeMap<String, f64>,
    pub confidence: f64,
}

impl ScoreAnswer {
    /// The probability that the answer lies at `level` or above.
    pub fn at_least(&self, level: usize) -> f64 {
        self.mass(|index| index >= level)
    }

    /// The probability that the answer lies at `level` or below.
    pub fn at_most(&self, level: usize) -> f64 {
        self.mass(|index| index <= level)
    }

    fn mass(&self, includes: impl Fn(usize) -> bool) -> f64 {
        self.probabilities
            .iter()
            .filter(|(key, _)| key.parse().is_ok_and(&includes))
            .map(|(_, probability)| probability)
            .sum()
    }
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

    fn validate(&self, question: &Question) -> Result<(), DecisionError> {
        if self.kind() != question.kind {
            return Err(DecisionError::Invalid(
                "answer type does not match the question",
            ));
        }
        match self {
            Self::Noul(answer) => probability(answer.noul),
            Self::Choice(answer) => {
                probability(answer.confidence)?;
                let keys = question.choice_options()?.keys().cloned().collect();
                distribution(&answer.probabilities, &keys)?;
                if !keys.contains(&answer.choice) {
                    return Err(DecisionError::Invalid(
                        "choice is not one of the requested options",
                    ));
                }
                Ok(())
            }
            Self::Score(answer) => {
                probability(answer.confidence)?;
                let levels = question.score_levels()?.len();
                let keys: BTreeSet<_> = (0..levels).map(|index| index.to_string()).collect();
                if !answer.legend.keys().eq(keys.iter()) {
                    return Err(DecisionError::Invalid(
                        "score legend does not match the requested levels",
                    ));
                }
                distribution(&answer.probabilities, &keys)?;
                let maximum = levels.saturating_sub(1) as f64;
                if !answer.score.is_finite() || !(0.0..=maximum).contains(&answer.score) {
                    return Err(DecisionError::Invalid(
                        "score is outside the requested levels",
                    ));
                }
                let expected: f64 = (0..levels)
                    .map(|index| index as f64 * answer.probabilities[&index.to_string()])
                    .sum();
                let tolerance = ROUNDING_ERROR * (1.0 + levels as f64 * maximum) + FLOAT_TOLERANCE;
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
    pub model: String,
    #[serde(deserialize_with = "unique_map")]
    pub answers: BTreeMap<String, Answer>,
    pub usage: Usage,
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
        Answer, DecisionRequest, DecisionResponse, FLOAT_TOLERANCE, MAX_CHOICE_OPTIONS,
        MAX_QUESTIONS, MAX_SCORE_LEVELS, MAX_STATE_CHARS, Question, QuestionType, ScoreAnswer,
    };
    use crate::DecisionError;

    const REQUEST_MODEL: &str = "jev-latest";
    const RESPONSE_MODEL: &str = "jev-1.13.0";
    const QUESTION_ID: &str = "writes";

    pub(crate) fn request() -> DecisionRequest {
        serde_json::from_value(json!({
            "model": REQUEST_MODEL,
            "state": {"command": "git status"},
            "questions": {QUESTION_ID: {"type": "noul", "instructions": "Does this modify files?"}}
        }))
        .unwrap()
    }

    pub(crate) fn response() -> DecisionResponse {
        serde_json::from_value(json!({
            "model": RESPONSE_MODEL,
            "answers": {QUESTION_ID: {"type": "noul", "noul": 0.1}},
            "usage": {"input_tokens": 17, "output_tokens": 20}
        }))
        .unwrap()
    }

    fn with_question(question: Value) -> DecisionRequest {
        let mut request = request();
        request.questions.insert(
            QUESTION_ID.into(),
            serde_json::from_value(question).unwrap(),
        );
        request
    }

    #[test_case(json!({"type":"noul", "instructions":{"task":"check"}, "criteria":{"true":["yes"], "false":{"text":"no"}}}); "structured_noul")]
    #[test_case(json!({"type":"choice", "instructions":["pick"], "criteria":{"a":{"description":"first"}, "b":["second"], "c":null}}); "structured_choice")]
    #[test_case(json!({"type":"score", "instructions":"rate", "criteria":["low", {"level":"high"}]} ); "structured_score")]
    fn question_round_trip(value: Value) {
        let question: Question = serde_json::from_value(value.clone()).unwrap();
        question.validate().unwrap();
        assert_eq!(serde_json::to_value(question).unwrap(), value);
    }

    #[test_case(json!({"type":"noul", "instructions":"check", "criteria":"yes"}); "noul_string_criteria")]
    #[test_case(json!({"type":"noul", "instructions":"check", "criteria":{"yes":"yes"}}); "noul_unknown_key")]
    #[test_case(json!({"type":"noul", "instructions":"check", "criteria":{"TRUE":"yes"}}); "noul_uppercase_key")]
    #[test_case(json!({"type":"noul", "instructions":"check", "criteria":{"true":null}}); "noul_null_description")]
    #[test_case(json!({"type":"noul", "instructions":null}); "null_instructions")]
    #[test_case(json!({"type":"choice", "instructions":"pick", "criteria":{}}); "empty_choice")]
    #[test_case(json!({"type":"choice", "instructions":"pick", "criteria":["a", "b"]}); "choice_array")]
    #[test_case(json!({"type":"choice", "instructions":"pick", "criteria":{"a":1}}); "numeric_choice_description")]
    #[test_case(json!({"type":"score", "instructions":"rate", "criteria":{"a":"low"}}); "score_object")]
    #[test_case(json!({"type":"score", "instructions":"rate", "criteria":["only"]}); "single_score_level")]
    #[test_case(json!({"type":"score", "instructions":"rate", "criteria":["low", null]}); "null_score_level")]
    #[test_case(json!({"type":"score", "instructions":"rate", "criteria":["low", 1]}); "numeric_score_level")]
    fn rejects_invalid_question(value: Value) {
        let question: Question = serde_json::from_value(value).unwrap();
        assert!(matches!(
            question.validate(),
            Err(DecisionError::Rejected(_))
        ));
    }

    #[test]
    fn questions_have_no_fields_beyond_the_official_schema() {
        let labelled =
            json!({"type":"noul", "instructions":"check", "labels":{"true":"yes","false":"no"}});
        assert!(serde_json::from_value::<Question>(labelled).is_err());
    }

    #[test_case(json!({"type":"noul", "noul":0.95}), None; "official_noul")]
    #[test_case(json!({"type":"choice", "choice":"billing", "probabilities":{"billing":0.88,"technical":0.12,"sales":0.0}, "confidence":0.81}), Some(json!({"billing":"Payments, invoicing, refunds","technical":"Bugs, outages, integrations","sales":null})); "official_choice")]
    #[test_case(json!({"type":"score", "score":1.05, "legend":{"0":"Calm","1":"Frustrated","2":"Very angry"}, "probabilities":{"0":0.0,"1":0.95,"2":0.05}, "confidence":0.92}), Some(json!(["Calm","Frustrated","Very angry"])); "official_score")]
    #[test_case(json!({"type":"score", "score":0.9921, "legend":{"0":"Calm","1":"level: Very angry\nsignals:\n  - caps"}, "probabilities":{"0":0.0079,"1":0.9921}, "confidence":0.9842}), Some(json!(["Calm",{"level":"Very angry","signals":["caps"]}])); "structured_score_legend")]
    fn answer_round_trip(answer: Value, criteria: Option<Value>) {
        let kind = answer["type"].clone();
        let request =
            with_question(json!({"type":kind, "instructions":"check", "criteria":criteria}));
        let fixture = json!({
            "model": RESPONSE_MODEL,
            "answers": {QUESTION_ID: answer},
            "usage": {"input_tokens":17, "output_tokens":20}
        });
        let mut served = fixture.clone();
        served["latency_ms"] = json!(94.2);
        let response: DecisionResponse = serde_json::from_value(served).unwrap();
        response.validate_for(&request).unwrap();
        assert_eq!(serde_json::to_value(response).unwrap(), fixture);
    }

    #[test_case(-0.1; "negative")]
    #[test_case(1.1; "above_one")]
    #[test_case(f64::NAN; "nan")]
    #[test_case(f64::INFINITY; "infinity")]
    fn rejects_invalid_probability(value: f64) {
        let mut response = response();
        let Answer::Noul(answer) = response.answers.get_mut(QUESTION_ID).unwrap() else {
            unreachable!()
        };
        answer.noul = value;
        assert!(matches!(
            response.validate_for(&request()),
            Err(DecisionError::Invalid(_))
        ));
    }

    #[test_case("/answers", json!({}); "missing_answer")]
    #[test_case("/answers/extra", json!({"type":"noul","noul":0.1}); "unexpected_answer")]
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

    #[test_case(json!({"a":0.2,"b":0.2}), "a", 0.5; "not_normalized")]
    #[test_case(json!({"a":1.0}), "a", 0.5; "missing_probability")]
    #[test_case(json!({"a":0.5,"b":0.5}), "c", 0.5; "unknown_choice")]
    #[test_case(json!({"a":-0.1,"b":1.1}), "a", 0.5; "invalid_range")]
    #[test_case(json!({"a":0.5,"b":0.5}), "a", 1.1; "invalid_confidence")]
    fn rejects_invalid_choice(probabilities: Value, choice: &str, confidence: f64) {
        let request = with_question(
            json!({"type":"choice","instructions":"pick","criteria":{"a":null,"b":null}}),
        );
        let mut response = response();
        response.answers.insert(QUESTION_ID.into(), serde_json::from_value(json!({"type":"choice","choice":choice,"confidence":confidence,"probabilities":probabilities})).unwrap());
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
    #[test_case("legend", json!({"0":"low","2":"high"}); "wrong_legend_levels")]
    #[test_case("probabilities", json!({"0":1.0}); "missing_level")]
    #[test_case("confidence", json!(1.1); "invalid_confidence")]
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

    #[test_case(json!({"0":0.1,"1":0.2,"2":0.3,"3":0.4}), 2, 0.7, 0.6; "split")]
    #[test_case(json!({"0":0.0,"1":1.0,"2":0.0,"3":0.0}), 1, 1.0, 1.0; "single_peak")]
    #[test_case(json!({"0":0.25,"1":0.25,"2":0.25,"3":0.25}), 0, 1.0, 0.25; "lowest_level")]
    #[test_case(json!({"0":0.25,"1":0.25,"2":0.25,"3":0.25}), 3, 0.25, 1.0; "highest_level")]
    fn score_bounds_sum_probabilities(
        probabilities: Value,
        level: usize,
        at_least: f64,
        at_most: f64,
    ) {
        let answer: ScoreAnswer = serde_json::from_value(json!({
            "score": 0.0, "legend": {}, "probabilities": probabilities, "confidence": 1.0
        }))
        .unwrap();
        assert!((answer.at_least(level) - at_least).abs() < FLOAT_TOLERANCE);
        assert!((answer.at_most(level) - at_most).abs() < FLOAT_TOLERANCE);
    }

    #[test_case(QuestionType::Choice, MAX_CHOICE_OPTIONS; "choice_limit")]
    #[test_case(QuestionType::Score, MAX_SCORE_LEVELS; "score_limit")]
    fn enforces_option_limits(kind: QuestionType, limit: usize) {
        let question = |count: usize| {
            let options = (0..count).map(|index| format!("option {index}"));
            Question {
                kind: kind.clone(),
                instructions: json!("pick"),
                criteria: Some(match kind {
                    QuestionType::Choice => options.map(|option| (option, Value::Null)).collect(),
                    _ => options.map(Value::String).collect(),
                }),
            }
        };
        question(limit).validate().unwrap();
        assert!(question(limit + 1).validate().is_err());
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

    #[test_case(r#"{"model":"m","answers":{},"usage":{"input_tokens":-1,"output_tokens":0}}"#; "negative_usage")]
    #[test_case(r#"{"model":"m","answers":{},"usage":{}}"#; "missing_usage_fields")]
    #[test_case(r#"{"answers":{"writes":{"type":"noul","noul":0.1}},"usage":{"input_tokens":0,"output_tokens":0}}"#; "missing_model")]
    #[test_case(r#"{"model":"m","answers":{"writes":{"type":"choice","choice":"a","probabilities":{"a":1.0}}},"usage":{"input_tokens":0,"output_tokens":0}}"#; "choice_without_confidence")]
    #[test_case(r#"{"model":"m","answers":{"writes":{"type":"score","score":0.0,"legend":{"0":"a","1":"b"},"probabilities":{"0":1.0,"1":0.0}}},"usage":{"input_tokens":0,"output_tokens":0}}"#; "score_without_confidence")]
    #[test_case(r#"{"model":"m","answers":{"writes":{"type":"score","score":0.0,"legend":{"0":{"level":"a"},"1":"b"},"probabilities":{"0":1.0,"1":0.0},"confidence":1.0}},"usage":{"input_tokens":0,"output_tokens":0}}"#; "unrendered_legend")]
    #[test_case(r#"{"model":"m","answers":{"writes":{"type":"noul","noul":0.1},"writes":{"type":"noul","noul":0.9}},"usage":{"input_tokens":0,"output_tokens":0}}"#; "duplicate_answer")]
    #[test_case(r#"{"model":"m","answers":{"writes":{"type":"choice","choice":"a","probabilities":{"a":0.1,"a":1.0},"confidence":1.0}},"usage":{"input_tokens":0,"output_tokens":0}}"#; "duplicate_probability")]
    fn rejects_invalid_schema(fixture: &str) {
        assert!(serde_json::from_str::<DecisionResponse>(fixture).is_err());
    }
}
