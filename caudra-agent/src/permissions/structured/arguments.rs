use super::{PermissionArgumentConstraint, SelectedPermissionArgument};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt::Write;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SelectedInputError {
    #[error("JSON pointer must be empty or begin with '/': {0:?}")]
    InvalidPointer(String),
    #[error("JSON pointer contains an invalid '~' escape: {0:?}")]
    InvalidEscape(String),
    #[error("JSON pointer uses a non-canonical array index: {0:?}")]
    InvalidArrayIndex(String),
    #[error("JSON pointer does not exist in the input: {0:?}")]
    MissingPointer(String),
    #[error("JSON pointer is selected more than once: {0:?}")]
    DuplicatePointer(String),
}

pub fn canonical_json(value: &Value) -> String {
    let mut output = String::new();
    write_canonical_json(value, &mut output);
    output
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

pub fn canonical_json_sha256(value: &Value) -> String {
    hex_encode(&Sha256::digest(canonical_json(value).as_bytes()))
}

pub(super) fn write_canonical_json(value: &Value, output: &mut String) {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => output.push_str(&value.to_string()),
        Value::String(value) => {
            output.push_str(&serde_json::to_string(value).expect("strings always serialize"));
        }
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical_json(value, output);
            }
            output.push(']');
        }
        Value::Object(values) => {
            output.push('{');
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_unstable_by_key(|(key, _)| *key);
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key).expect("object keys always serialize"));
                output.push(':');
                write_canonical_json(value, output);
            }
            output.push('}');
        }
    }
}

pub fn escape_json_pointer_segment(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

pub fn json_pointer<S: AsRef<str>>(segments: &[S]) -> String {
    let mut pointer = String::new();
    for segment in segments {
        pointer.push('/');
        pointer.push_str(&escape_json_pointer_segment(segment.as_ref()));
    }
    pointer
}

pub fn selected_input_pointer<'a>(
    input: &'a Value,
    pointer: &str,
) -> Result<&'a Value, SelectedInputError> {
    let segments = decode_json_pointer(pointer)?;
    let mut selected = input;
    for segment in segments {
        selected = match selected {
            Value::Object(object) => object
                .get(&segment)
                .ok_or_else(|| SelectedInputError::MissingPointer(pointer.to_owned()))?,
            Value::Array(array) => {
                if segment == "-"
                    || (segment.len() > 1 && segment.starts_with('0'))
                    || !segment.bytes().all(|byte| byte.is_ascii_digit())
                {
                    return Err(SelectedInputError::InvalidArrayIndex(pointer.to_owned()));
                }
                let index = segment
                    .parse::<usize>()
                    .map_err(|_| SelectedInputError::InvalidArrayIndex(pointer.to_owned()))?;
                array
                    .get(index)
                    .ok_or_else(|| SelectedInputError::MissingPointer(pointer.to_owned()))?
            }
            _ => return Err(SelectedInputError::MissingPointer(pointer.to_owned())),
        };
    }
    Ok(selected)
}

pub fn selected_input<S: AsRef<str>>(
    input: &Value,
    pointers: &[S],
) -> Result<Vec<SelectedPermissionArgument>, SelectedInputError> {
    let mut seen = HashSet::with_capacity(pointers.len());
    let mut selected = Vec::with_capacity(pointers.len());
    for pointer in pointers {
        let pointer = pointer.as_ref();
        if !seen.insert(pointer) {
            return Err(SelectedInputError::DuplicatePointer(pointer.to_owned()));
        }
        let value = selected_input_pointer(input, pointer)?.clone();
        selected.push(SelectedPermissionArgument {
            pointer: pointer.to_owned(),
            digest: canonical_json_sha256(&value),
            value,
        });
    }
    Ok(selected)
}

pub fn selected_input_digest<S: AsRef<str>>(
    input: &Value,
    pointers: &[S],
) -> Result<String, SelectedInputError> {
    let mut seen = HashSet::with_capacity(pointers.len());
    let mut projection = Vec::with_capacity(pointers.len());
    for pointer in pointers {
        let pointer = pointer.as_ref();
        if pointer.is_empty() || !seen.insert(pointer) {
            return Err(if pointer.is_empty() {
                SelectedInputError::InvalidPointer(pointer.to_owned())
            } else {
                SelectedInputError::DuplicatePointer(pointer.to_owned())
            });
        }
        decode_json_pointer(pointer)?;
        let selected = selected_input_pointer(input, pointer);
        projection.push(serde_json::json!({
            "pointer": pointer,
            "present": selected.is_ok(),
            "value": selected.ok(),
        }));
    }
    Ok(canonical_json_sha256(&Value::Array(projection)))
}

pub(super) fn decode_json_pointer(pointer: &str) -> Result<Vec<String>, SelectedInputError> {
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    let Some(pointer) = pointer.strip_prefix('/') else {
        return Err(SelectedInputError::InvalidPointer(pointer.to_owned()));
    };
    pointer
        .split('/')
        .map(|segment| {
            let mut decoded = String::with_capacity(segment.len());
            let mut chars = segment.chars();
            while let Some(character) = chars.next() {
                if character != '~' {
                    decoded.push(character);
                    continue;
                }
                match chars.next() {
                    Some('0') => decoded.push('~'),
                    Some('1') => decoded.push('/'),
                    _ => return Err(SelectedInputError::InvalidEscape(pointer.to_owned())),
                }
            }
            Ok(decoded)
        })
        .collect()
}

pub fn argument_constraint_matches(
    constraint: &PermissionArgumentConstraint,
    input: &Value,
) -> bool {
    match constraint {
        PermissionArgumentConstraint::Exact { digest } => canonical_json_sha256(input) == *digest,
        PermissionArgumentConstraint::Selected { arguments } => {
            let mut seen = HashSet::with_capacity(arguments.len());
            arguments.iter().all(|argument| {
                seen.insert(argument.pointer.as_str())
                    && canonical_json_sha256(&argument.value) == argument.digest
                    && selected_input_pointer(input, &argument.pointer).is_ok_and(|selected| {
                        canonical_json_sha256(selected) == argument.digest
                            && canonical_json(selected) == canonical_json(&argument.value)
                    })
            })
        }
        PermissionArgumentConstraint::SelectedDigest { pointers, digest } => {
            selected_input_digest(input, pointers).is_ok_and(|actual| actual == *digest)
        }
        PermissionArgumentConstraint::Unconstrained => true,
    }
}

#[cfg(test)]
mod tests {

    use serde_json::json;

    use crate::permissions::structured::{
        PermissionArgumentConstraint, SelectedInputError, argument_constraint_matches,
        canonical_json_sha256, json_pointer, selected_input, selected_input_pointer,
    };
    #[test]
    fn selected_input_uses_safe_pointer_boundaries() {
        let input = json!({"a/b": {"~key": ["zero", "one"]}, "a": {"b": "other"}});
        let pointer = json_pointer(&["a/b", "~key", "1"]);
        assert_eq!(pointer, "/a~1b/~0key/1");
        assert_eq!(selected_input_pointer(&input, &pointer).unwrap(), "one");
        assert!(matches!(
            selected_input_pointer(&input, "/a~1b/~0key/01"),
            Err(SelectedInputError::InvalidArrayIndex(_))
        ));
        assert!(matches!(
            selected_input_pointer(&input, "/a~2b"),
            Err(SelectedInputError::InvalidEscape(_))
        ));
        assert!(matches!(
            selected_input(&input, &[pointer.as_str(), pointer.as_str()]),
            Err(SelectedInputError::DuplicatePointer(_))
        ));
    }

    #[test]
    fn selected_and_exact_argument_constraints_match_canonically() {
        let input = json!({"ignored": 1, "selected": {"b": 2, "a": 1}});
        let selected = selected_input(&input, &["/selected"]).unwrap();
        let selected_constraint = PermissionArgumentConstraint::Selected {
            arguments: selected,
        };
        assert!(argument_constraint_matches(
            &selected_constraint,
            &json!({"selected": {"a": 1, "b": 2}, "ignored": 99})
        ));
        assert!(!argument_constraint_matches(
            &selected_constraint,
            &json!({"selected": {"a": 1, "b": 3}, "ignored": 1})
        ));
        let exact = PermissionArgumentConstraint::Exact {
            digest: canonical_json_sha256(&input),
        };
        assert!(argument_constraint_matches(&exact, &input));
        assert!(!argument_constraint_matches(
            &exact,
            &json!({"ignored": 2, "selected": {"b": 2, "a": 1}})
        ));
    }
}
