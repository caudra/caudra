use std::collections::BTreeMap;

pub use caudra_script::canonical_json;
use caudra_script::sha256_hex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::host::{AgentRequest, AgentResult, DecisionRequest, DecisionResult};

pub const MAX_JOURNAL_ENTRIES: usize = 16_384;
pub const MAX_JOURNAL_BYTES: usize = 64 * 1024 * 1024;

const HASH_VERSION_TAG: &str = "caudra-workflow/journal/v1\n";
const KIND_SEPARATOR: &[u8] = b"\n";
const SCRATCH_NAME_FIELD: &str = "name";
const SCRATCH_CONTENT_FIELD: &str = "content";

/// Position of a result-bearing host call within a run. Keys are dense from [`CallKey::FIRST`],
/// so an identical script issues identical keys on every replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CallKey(pub u64);

impl CallKey {
    pub const FIRST: Self = Self(1);

    pub fn offset(self, count: u64) -> Option<Self> {
        self.0.checked_add(count).map(Self)
    }
}

impl std::fmt::Display for CallKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallKind {
    Agent,
    Parallel,
    ScratchFile,
    Decision,
}

impl CallKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Parallel => "parallel",
            Self::ScratchFile => "scratch_file",
            Self::Decision => "decision",
        }
    }
}

impl std::fmt::Display for CallKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lowercase hex SHA-256 of a versioned, canonicalised request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestHash(String);

impl RequestHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RequestHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The JSON the engine hashes for `agent` and `parallel` calls.
pub fn agent_request_value(request: &AgentRequest) -> Value {
    serde_json::to_value(request).expect("AgentRequest is plain JSON data")
}

pub fn decision_request_value(request: &DecisionRequest) -> Value {
    serde_json::to_value(request).expect("DecisionRequest is plain JSON data")
}

/// The JSON the engine hashes for `write_scratch_file` calls.
pub fn scratch_request_value(name: &str, content: &str) -> Value {
    json!({ SCRATCH_NAME_FIELD: name, SCRATCH_CONTENT_FIELD: content })
}

pub fn hash_request(kind: CallKind, request: &Value) -> RequestHash {
    RequestHash(sha256_hex(&[
        HASH_VERSION_TAG.as_bytes(),
        kind.as_str().as_bytes(),
        KIND_SEPARATOR,
        canonical_json(request).as_bytes(),
    ]))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSignature {
    pub kind: CallKind,
    pub hash: RequestHash,
}

impl std::fmt::Display for CallSignature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.kind, self.hash)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub kind: CallKind,
    pub hash: RequestHash,
    pub result: Value,
}

impl JournalEntry {
    pub fn new(kind: CallKind, hash: RequestHash, result: Value) -> Self {
        Self { kind, hash, result }
    }

    /// What a host commits after `WorkflowHost::agent` (`CallKind::Agent`) or for one item of
    /// `WorkflowHost::parallel` (`CallKind::Parallel`).
    pub fn agent(kind: CallKind, request: &AgentRequest, result: &AgentResult) -> Self {
        Self::new(
            kind,
            hash_request(kind, &agent_request_value(request)),
            serde_json::to_value(result).expect("AgentResult is plain JSON data"),
        )
    }

    /// What a host commits after `WorkflowHost::write_scratch_file` returned `path`.
    pub fn scratch_file(name: &str, content: &str, path: &str) -> Self {
        Self::new(
            CallKind::ScratchFile,
            hash_request(CallKind::ScratchFile, &scratch_request_value(name, content)),
            Value::String(path.to_owned()),
        )
    }

    pub fn decision(request: &DecisionRequest, result: &DecisionResult) -> Self {
        Self::new(
            CallKind::Decision,
            hash_request(CallKind::Decision, &decision_request_value(request)),
            serde_json::to_value(result).expect("DecisionResult is plain JSON data"),
        )
    }

    fn signature(&self) -> CallSignature {
        CallSignature {
            kind: self.kind,
            hash: self.hash.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JournalError {
    #[error(
        "journal call {key} diverged: journaled {expected}, script issued {found}; the workflow \
         is nondeterministic or was edited mid-run"
    )]
    Divergence {
        key: CallKey,
        expected: CallSignature,
        found: CallSignature,
    },
    #[error("journal already holds call {key}")]
    Duplicate { key: CallKey },
    #[error("journal is full ({MAX_JOURNAL_ENTRIES} entries)")]
    Full,
    #[error("journal would exceed {MAX_JOURNAL_BYTES} bytes")]
    TooLarge,
}

/// Committed host-call results from earlier attempts of the same run, keyed by [`CallKey`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Journal {
    entries: BTreeMap<CallKey, JournalEntry>,
    bytes: usize,
}

impl Journal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, key: CallKey, entry: JournalEntry) -> Result<(), JournalError> {
        if self.entries.contains_key(&key) {
            return Err(JournalError::Duplicate { key });
        }
        if self.entries.len() >= MAX_JOURNAL_ENTRIES {
            return Err(JournalError::Full);
        }
        let bytes = self.bytes.saturating_add(entry.result.to_string().len());
        if bytes > MAX_JOURNAL_BYTES {
            return Err(JournalError::TooLarge);
        }
        self.bytes = bytes;
        self.entries.insert(key, entry);
        Ok(())
    }

    pub fn get(&self, key: CallKey) -> Option<&JournalEntry> {
        self.entries.get(&key)
    }

    pub fn last_key(&self) -> Option<CallKey> {
        self.entries.keys().next_back().copied()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (CallKey, &JournalEntry)> {
        self.entries.iter().map(|(key, entry)| (*key, entry))
    }

    /// Whether `key` falls inside the journaled range, meaning the run is still catching up.
    pub fn covers(&self, key: CallKey) -> bool {
        self.last_key().is_some_and(|last| key <= last)
    }

    /// `Some(result)` when `key` holds a matching call, `None` when nothing was committed for it,
    /// and a divergence error when the committed call differs in kind or request.
    pub fn replay(
        &self,
        key: CallKey,
        kind: CallKind,
        hash: &RequestHash,
    ) -> Result<Option<&Value>, JournalError> {
        let Some(entry) = self.entries.get(&key) else {
            return Ok(None);
        };
        if entry.kind != kind || entry.hash != *hash {
            return Err(JournalError::Divergence {
                key,
                expected: entry.signature(),
                found: CallSignature {
                    kind,
                    hash: hash.clone(),
                },
            });
        }
        Ok(Some(&entry.result))
    }
}

#[cfg(test)]
mod tests {
    use caudra_script::HEX_DIGEST_LEN;
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    #[test]
    fn canonical_json_sorts_keys_recursively_without_whitespace() {
        let value = json!({ "b": [ { "z": 1, "a": [3, 2] } ], "a": { "y": null, "x": "s" } });
        assert_eq!(
            canonical_json(&value),
            r#"{"a":{"x":"s","y":null},"b":[{"a":[3,2],"z":1}]}"#
        );
    }

    #[test]
    fn hash_ignores_key_order_but_not_content_or_kind() {
        let left = json!({ "prompt": "p", "label": "l" });
        let right = json!({ "label": "l", "prompt": "p" });
        let changed = json!({ "label": "l", "prompt": "q" });
        assert_eq!(
            hash_request(CallKind::Agent, &left),
            hash_request(CallKind::Agent, &right)
        );
        assert_ne!(
            hash_request(CallKind::Agent, &left),
            hash_request(CallKind::Agent, &changed)
        );
        assert_ne!(
            hash_request(CallKind::Agent, &left),
            hash_request(CallKind::Parallel, &left)
        );
        assert_eq!(
            hash_request(CallKind::Agent, &left).as_str().len(),
            HEX_DIGEST_LEN
        );
    }

    fn entry(kind: CallKind, request: &Value, result: Value) -> JournalEntry {
        JournalEntry::new(kind, hash_request(kind, request), result)
    }

    #[test]
    fn replay_distinguishes_hit_miss_and_divergence() {
        let request = json!({ "prompt": "p" });
        let mut journal = Journal::new();
        journal
            .insert(CallKey(1), entry(CallKind::Agent, &request, json!("done")))
            .expect("first insert");
        let hash = hash_request(CallKind::Agent, &request);
        assert_eq!(
            journal.replay(CallKey(1), CallKind::Agent, &hash),
            Ok(Some(&json!("done")))
        );
        assert_eq!(journal.replay(CallKey(2), CallKind::Agent, &hash), Ok(None));
        let other = hash_request(CallKind::Agent, &json!({ "prompt": "q" }));
        assert!(matches!(
            journal.replay(CallKey(1), CallKind::Agent, &other),
            Err(JournalError::Divergence {
                key: CallKey(1),
                ..
            })
        ));
        assert!(matches!(
            journal.replay(CallKey(1), CallKind::Parallel, &hash),
            Err(JournalError::Divergence {
                key: CallKey(1),
                ..
            })
        ));
    }

    #[test_case(1, 1 => Err(JournalError::Duplicate { key: CallKey(1) }); "duplicate")]
    #[test_case(1, 2 => Ok(()); "distinct")]
    fn insert_rejects_duplicates(first: u64, second: u64) -> Result<(), JournalError> {
        let request = json!({});
        let mut journal = Journal::new();
        journal.insert(
            CallKey(first),
            entry(CallKind::ScratchFile, &request, json!("a")),
        )?;
        journal.insert(
            CallKey(second),
            entry(CallKind::ScratchFile, &request, json!("b")),
        )
    }

    #[test]
    fn covers_and_last_key_follow_the_highest_key() {
        let mut journal = Journal::new();
        assert!(!journal.covers(CallKey::FIRST));
        journal
            .insert(CallKey(3), entry(CallKind::Agent, &json!({}), json!(null)))
            .expect("insert");
        assert_eq!(journal.last_key(), Some(CallKey(3)));
        assert!(journal.covers(CallKey(2)));
        assert!(!journal.covers(CallKey(4)));
        assert_eq!(journal.len(), 1);
    }

    #[test]
    fn insert_enforces_byte_limit() {
        let mut journal = Journal::new();
        let oversized = Value::String("x".repeat(MAX_JOURNAL_BYTES));
        assert_eq!(
            journal.insert(CallKey(1), entry(CallKind::Agent, &json!({}), oversized)),
            Err(JournalError::TooLarge)
        );
        assert!(journal.is_empty());
    }
}
