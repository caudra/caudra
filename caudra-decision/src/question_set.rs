use std::collections::BTreeMap;
use std::io::{self, Write};

use serde::Serialize;
use serde::ser::{SerializeMap, SerializeSeq, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::engine::DecisionError;
use crate::wire::{MAX_REQUEST_BYTES, QuestionType, Questions, validate_questions};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct QuestionSet {
    id: String,
    version: String,
    questions: Questions,
}

impl QuestionSet {
    pub fn new(id: impl Into<String>, questions: Questions) -> Result<Self, DecisionError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(DecisionError::Rejected("question set id must not be empty"));
        }
        validate_questions(&questions)?;
        let version = content_hash(&questions)?;
        Ok(Self {
            id,
            version,
            questions,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn questions(&self) -> &Questions {
        &self.questions
    }

    pub fn validate_required(
        &self,
        required: &[(&str, QuestionType)],
    ) -> Result<(), DecisionError> {
        for (id, kind) in required {
            if !self
                .questions
                .get(*id)
                .is_some_and(|question| question.kind == *kind)
            {
                return Err(DecisionError::Rejected(
                    "required question is missing or has the wrong type",
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn content_hash(value: &impl Serialize) -> Result<String, DecisionError> {
    let bytes = bounded_json(value, MAX_REQUEST_BYTES)?;
    let value = serde_json::from_slice(&bytes)
        .map_err(|_| DecisionError::Rejected("request cannot be serialized"))?;
    let bytes = bounded_json(&Canonical(&value), MAX_REQUEST_BYTES)?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub(crate) fn bounded_json(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, DecisionError> {
    let mut writer = BoundedWriter {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value).map_err(|_| {
        DecisionError::Rejected("JSON exceeds its byte limit or cannot be serialized")
    })?;
    Ok(writer.bytes)
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("JSON byte limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Canonical<'a>(&'a Value);

impl Serialize for Canonical<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Value::Object(values) => {
                let sorted: BTreeMap<_, _> = values.iter().collect();
                let mut map = serializer.serialize_map(Some(sorted.len()))?;
                for (key, value) in sorted {
                    map.serialize_entry(key, &Canonical(value))?;
                }
                map.end()
            }
            Value::Array(values) => {
                let mut seq = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    seq.serialize_element(&Canonical(value))?;
                }
                seq.end()
            }
            value => value.serialize(serializer),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::{QuestionSet, bounded_json, content_hash};
    use crate::wire::{QuestionType, tests::request};

    #[test]
    fn hashes_canonical_content_not_object_order() {
        let first = json!({"b":{"y":2,"x":1}, "a":[3,4]});
        let second = json!({"a":[3,4], "b":{"x":1,"y":2}});
        assert_eq!(
            content_hash(&first).unwrap(),
            content_hash(&second).unwrap()
        );
        assert_ne!(
            content_hash(&first).unwrap(),
            content_hash(&json!({"a":[4,3], "b":{"x":1,"y":2}})).unwrap()
        );
    }

    #[test]
    fn versions_change_with_wording_not_set_name() {
        let questions = request().questions;
        let first = QuestionSet::new("permission", questions.clone()).unwrap();
        let second = QuestionSet::new("renamed", questions.clone()).unwrap();
        assert_eq!(first.version(), second.version());
        let mut changed = questions;
        changed.get_mut("writes").unwrap().instructions = json!("Does this write?");
        assert_ne!(
            first.version(),
            QuestionSet::new("permission", changed).unwrap().version()
        );
    }

    #[test_case("writes", QuestionType::Noul, true; "present")]
    #[test_case("writes", QuestionType::Choice, false; "wrong_type")]
    #[test_case("missing", QuestionType::Noul, false; "missing")]
    fn validates_required_questions(id: &str, kind: QuestionType, valid: bool) {
        let set = QuestionSet::new("permission", request().questions).unwrap();
        assert_eq!(set.validate_required(&[(id, kind)]).is_ok(), valid);
    }

    #[test]
    fn bounds_json_before_allocating_entire_payload() {
        let value = json!("abcdef");
        let length = serde_json::to_vec(&value).unwrap().len();
        assert!(bounded_json(&value, length).is_ok());
        assert!(bounded_json(&value, length - 1).is_err());
    }
}
