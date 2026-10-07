use std::fmt::Write as _;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Length of a lowercase hex SHA-256 digest.
pub const HEX_DIGEST_LEN: usize = 64;

/// JSON with object keys sorted recursively and no whitespace, so equal values hash equally.
pub fn canonical_json(value: &Value) -> String {
    fn canonicalise(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut sorted: Vec<(&String, &Value)> = map.iter().collect();
                sorted.sort_unstable_by(|left, right| left.0.cmp(right.0));
                Value::Object(
                    sorted
                        .into_iter()
                        .map(|(key, value)| (key.clone(), canonicalise(value)))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(canonicalise).collect()),
            scalar => scalar.clone(),
        }
    }
    canonicalise(value).to_string()
}

/// Lowercase hex SHA-256 of `parts`, hashed in order as one message.
pub fn sha256_hex(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    let mut hex = String::with_capacity(HEX_DIGEST_LEN);
    for byte in hasher.finalize() {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hex
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const CANONICAL: &str = r#"{"a":{"x":"s","y":null},"b":[{"a":[3,2],"z":1}]}"#;

    #[test_case(json!({ "b": [{ "z": 1, "a": [3, 2] }], "a": { "y": null, "x": "s" } }); "reversed_keys")]
    #[test_case(json!({ "a": { "x": "s", "y": null }, "b": [{ "a": [3, 2], "z": 1 }] }); "sorted_keys")]
    fn canonical_json_sorts_keys_at_every_depth_and_keeps_array_order(value: Value) {
        assert_eq!(canonical_json(&value), CANONICAL);
    }

    #[test_case(&[b"abc"] => ABC_SHA256; "one_part")]
    #[test_case(&[b"a", b"", b"bc"] => ABC_SHA256; "split_parts")]
    #[test_case(&[] => EMPTY_SHA256; "no_parts")]
    fn sha256_hex_matches_known_vectors(parts: &[&[u8]]) -> String {
        sha256_hex(parts)
    }
}
