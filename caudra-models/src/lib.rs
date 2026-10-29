//! Signatures authenticate exact bytes; runtime treats `content_sha256` as an opaque identity.
//! Only same-version tooling uses [`Snapshot::hash_matches`], since newer fields may be ignored.

mod catalog;
#[cfg(feature = "cli")]
pub mod cli;
pub mod repo;
pub mod sign;
mod validate;
mod wire;

use std::cmp::Ordering;

use jiff::Timestamp;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub use catalog::{
    CatalogCost, CatalogCostTier, CatalogLimits, CatalogModalities, CatalogModel, CatalogProvider,
    CatalogShape, CatalogTierBoundary, import_models_dev,
};
pub use validate::{Issue, validate};
pub use wire::{
    Availability, DecisionModel, Family, FastPricing, Generation, ModelEntry, Payload, Period,
    PricePeriod, Pricing, PricingSchedule, PricingWindow, Provider, ReasoningOption, Snapshot,
    Tier,
};

pub const FORMAT: u32 = 1;
pub const MAX_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PROVIDERS: usize = 1000;
pub const MAX_CATALOG_MODELS: usize = 50_000;
pub const MAX_TOKENS: u32 = 100_000_000;
pub const ALLOWED_NPM: &[&str] = &["@ai-sdk/openai-compatible", "@ai-sdk/anthropic"];
pub const BLOCKED_CATALOG_PROVIDERS: &[&str] = &["zai", "zai-coding-plan", "github-copilot"];
pub const SUPPORTED_REQUIREMENTS: &[&str] = &[];
/// Production publishing is not set up until the operator supplies a trusted public key.
pub const TRUSTED_KEYS: &[[u8; 32]] = &[];

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("model file is {actual} bytes; maximum is {MAX_BYTES}")]
    TooLarge { actual: usize },
    #[error("invalid model-file JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("model file has {actual} providers; maximum is {MAX_PROVIDERS}")]
    TooManyProviders { actual: usize },
    #[error("model file has {actual} catalog models; maximum is {MAX_CATALOG_MODELS}")]
    TooManyCatalogModels { actual: usize },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("model files at {updated_at} have conflicting content hashes ({left} and {right})")]
pub struct SnapshotConflict {
    pub updated_at: Timestamp,
    pub left: String,
    pub right: String,
}

pub fn parse_bounded(bytes: &[u8]) -> Result<Snapshot, ParseError> {
    if bytes.len() > MAX_BYTES {
        return Err(ParseError::TooLarge {
            actual: bytes.len(),
        });
    }
    let snapshot: Snapshot = serde_json::from_slice(bytes)?;
    if snapshot.payload.providers.len() > MAX_PROVIDERS {
        return Err(ParseError::TooManyProviders {
            actual: snapshot.payload.providers.len(),
        });
    }
    let models = snapshot
        .payload
        .catalog
        .values()
        .map(|provider| provider.models.len())
        .sum();
    if models > MAX_CATALOG_MODELS {
        return Err(ParseError::TooManyCatalogModels { actual: models });
    }
    Ok(snapshot)
}

pub fn to_pretty_bytes(snapshot: &Snapshot) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(snapshot)
        .expect("model-file wire types serialize infallibly to a byte vector");
    bytes.push(b'\n');
    bytes
}

impl Payload {
    pub fn content_sha256(&self) -> String {
        let bytes = serde_json::to_vec(self)
            .expect("model-file wire types serialize infallibly to a byte vector");
        hex(&Sha256::digest(bytes))
    }
}

impl Snapshot {
    pub fn hash_matches(&self) -> bool {
        self.content_sha256 == self.payload.content_sha256()
    }

    pub fn stamp(payload: Payload, previous: Option<&Self>, now: Timestamp) -> Self {
        let content_sha256 = payload.content_sha256();
        let updated_at = previous
            .filter(|previous| previous.content_sha256 == content_sha256)
            .map_or(now, |previous| previous.updated_at);
        Self {
            schema: previous.and_then(|previous| previous.schema.clone()),
            format: FORMAT,
            updated_at,
            content_sha256,
            payload,
        }
    }

    pub fn is_newer_than(&self, other: &Self) -> Result<Ordering, SnapshotConflict> {
        let ordering = self.updated_at.cmp(&other.updated_at);
        if ordering == Ordering::Equal && self.content_sha256 != other.content_sha256 {
            return Err(SnapshotConflict {
                updated_at: self.updated_at,
                left: self.content_sha256.clone(),
                right: other.content_sha256.clone(),
            });
        }
        Ok(ordering)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cmp::Ordering;

    use jiff::Timestamp;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use test_case::test_case;

    use super::{
        CatalogModel, MAX_BYTES, MAX_CATALOG_MODELS, MAX_PROVIDERS, ParseError, Payload, Snapshot,
        hex, parse_bounded, to_pretty_bytes, validate,
    };

    pub(crate) const NOW: &str = "2026-10-29T12:00:00Z";
    const LATER: &str = "2026-10-30T12:00:00Z";
    const EMPTY_HASH: &str = "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";
    const EMPTY_PRETTY: &str = concat!(
        "{\n",
        "  \"$schema\": \"schema.json\",\n",
        "  \"format\": 1,\n",
        "  \"updated_at\": \"2026-10-29T12:00:00Z\",\n",
        "  \"content_sha256\": \"44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a\"\n",
        "}\n",
    );
    const SIZE_MESSAGE: &str = "model file is 16777217 bytes; maximum is 16777216";
    const FIXTURE: &str = r#"{
        "providers": [{
            "slug": "example", "display_name": "Example", "family": "gpt",
            "supports_thinking": true, "accepts_arbitrary_models": false,
            "fallback_max_output": 4096, "fallback_context_window": 32000,
            "serves_fast_mode": true, "catalog_slug": "catalog",
            "pricing_schedule": {"windows": [{"start": 22, "end": 2}], "multiplier": 0.5},
            "generations": [{"label": "Current", "members": ["model-a"]}],
            "models": [{
                "prefixes": ["model-a", "model-alias"], "default": true, "family": "gpt",
                "pricing": {"input": 1.0, "output": 2.0, "cache_write": 1.0, "cache_read": 0.1,
                    "fast": {"input": 2.0, "output": 4.0, "cache_write": 2.0, "cache_read": 0.2},
                    "tiers": [{"above": 200000, "input": 2.0, "output": 3.0, "cache_write": 2.0, "cache_read": 0.2,
                        "fast": {"input": 4.0, "output": 6.0, "cache_write": 4.0, "cache_read": 0.4}}]},
                "max_output_tokens": 4096, "context_window": 32000,
                "reasoning_options": [{"type": "toggle"}, {"type": "effort", "values": ["low", "high"]},
                    {"type": "budget_tokens", "min": 1024, "max": 4096}]
            }],
            "price_periods": [{"prefix": "model-a", "during": {"from": "2026-01-01", "until": "2027-01-01"},
                "pricing": {"input": 0.5, "output": 1.0, "cache_write": 0.5, "cache_read": 0.05}}]
        }],
        "decision_models": [{"protocol": "example", "prefixes": ["decision"], "source": "fixture",
            "pricing": {"input": 0.1, "output": 0.2, "cache_write": 0.0, "cache_read": 0.0}}],
        "catalog": {"catalog": {
            "name": "Example", "env": ["EXAMPLE_API_KEY"], "npm": "@ai-sdk/openai-compatible",
            "api": "https://example.com/v1", "models": {"model": {
                "limit": {"context": 32000, "input": 28000, "output": 4000},
                "cost": {"input": 1.0, "output": 2.0, "cache_write": 1.0, "cache_read": 0.1,
                    "tiers": [{"input": 2.0, "tier": {"size": 200000}}]},
                "provider": {"shape": "openai"}, "attachment": true, "reasoning": true,
                "modalities": {"input": ["text", "image"], "output": ["text"]},
                "reasoning_options": [{"type": "toggle"}], "release_date": "2026-01-01",
                "last_updated": "2026-10-29", "status": "active", "tool_call": true
            }}
        }}
    }"#;

    pub(crate) fn fixture() -> Snapshot {
        Snapshot::stamp(
            serde_json::from_str(FIXTURE).unwrap(),
            None,
            NOW.parse().unwrap(),
        )
    }

    #[test]
    fn fixture_round_trips() {
        let snapshot = fixture();
        let bytes = to_pretty_bytes(&snapshot);
        let parsed = parse_bounded(&bytes).unwrap();
        assert_eq!(parsed, snapshot);
        assert_eq!(to_pretty_bytes(&parsed), bytes);
        assert_eq!(validate(&parsed), Ok(()));
        assert!(parsed.hash_matches());
    }

    #[test]
    fn pretty_bytes_have_stable_order_indentation_and_newline() {
        let mut snapshot = Snapshot::stamp(Payload::default(), None, NOW.parse().unwrap());
        snapshot.schema = Some("schema.json".to_owned());
        assert_eq!(snapshot.content_sha256, EMPTY_HASH);
        assert_eq!(to_pretty_bytes(&snapshot), EMPTY_PRETTY.as_bytes());
    }

    #[test]
    fn catalog_map_order_does_not_change_bytes_or_hash() {
        let mut first = fixture();
        let provider = first.payload.catalog["catalog"].clone();
        first
            .payload
            .catalog
            .insert("a".to_owned(), provider.clone());
        first
            .payload
            .catalog
            .insert("z".to_owned(), provider.clone());
        let mut second = fixture();
        second
            .payload
            .catalog
            .insert("z".to_owned(), provider.clone());
        second.payload.catalog.insert("a".to_owned(), provider);
        assert_eq!(to_pretty_bytes(&first), to_pretty_bytes(&second));
        assert_eq!(
            first.payload.content_sha256(),
            second.payload.content_sha256()
        );
    }

    #[test_case(""; "snapshot")]
    #[test_case("/providers/0"; "provider")]
    #[test_case("/providers/0/models/0"; "model")]
    #[test_case("/providers/0/models/0/pricing"; "pricing")]
    #[test_case("/catalog/catalog/models/model"; "catalog_model")]
    fn unknown_fields_are_ignored(path: &str) {
        let snapshot = fixture();
        let mut value = serde_json::to_value(&snapshot).unwrap();
        value.pointer_mut(path).unwrap()["future_field"] = json!({"nested": [true]});
        assert_eq!(
            parse_bounded(&serde_json::to_vec(&value).unwrap()).unwrap(),
            snapshot
        );
    }

    #[test]
    fn future_payload_hash_is_opaque_to_semantic_validation() {
        let mut payload = serde_json::to_value(&fixture().payload).unwrap();
        payload["future_field"] = json!(true);
        let future_hash = hex(&Sha256::digest(serde_json::to_vec(&payload).unwrap()));
        let mut snapshot = serde_json::to_value(fixture()).unwrap();
        snapshot["future_field"] = json!(true);
        snapshot["content_sha256"] = json!(future_hash);
        let parsed = parse_bounded(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert_eq!(validate(&parsed), Ok(()));
        assert!(!parsed.hash_matches());
    }

    #[test_case(false; "unchanged")]
    #[test_case(true; "changed")]
    fn stamp_preserves_time_only_for_unchanged_payload(change: bool) {
        let previous = fixture();
        let mut payload = previous.payload.clone();
        if change {
            payload.providers[0].display_name.push('!');
        }
        let now: Timestamp = LATER.parse().unwrap();
        let snapshot = Snapshot::stamp(payload, Some(&previous), now);
        assert_eq!(
            snapshot.updated_at,
            if change { now } else { previous.updated_at }
        );
        assert_eq!(snapshot.content_sha256 == previous.content_sha256, !change);
    }

    #[test_case(NOW, NOW, Ordering::Equal; "equal")]
    #[test_case(LATER, NOW, Ordering::Greater; "newer")]
    #[test_case(NOW, LATER, Ordering::Less; "older")]
    fn snapshot_selection(left: &str, right: &str, expected: Ordering) {
        let mut a = fixture();
        let mut b = fixture();
        a.updated_at = left.parse().unwrap();
        b.updated_at = right.parse().unwrap();
        assert_eq!(a.is_newer_than(&b).unwrap(), expected);
    }

    #[test]
    fn equal_timestamp_with_different_hash_is_a_conflict() {
        let a = fixture();
        let mut b = fixture();
        b.content_sha256 = EMPTY_HASH.to_owned();
        let error = a.is_newer_than(&b).unwrap_err();
        assert_eq!(error.updated_at, a.updated_at);
        assert_eq!(error.left, a.content_sha256);
        assert_eq!(error.right, b.content_sha256);
    }

    #[test]
    fn size_limit_precedes_json_parsing() {
        let error = parse_bounded(&vec![b'x'; MAX_BYTES + 1]).unwrap_err();
        assert!(matches!(error, ParseError::TooLarge { .. }));
        assert_eq!(error.to_string(), SIZE_MESSAGE);
    }

    #[test]
    fn exact_size_limit_is_allowed() {
        let mut bytes = to_pretty_bytes(&fixture());
        bytes.resize(MAX_BYTES, b' ');
        assert!(parse_bounded(&bytes).is_ok());
    }

    #[test_case(false; "provider_count")]
    #[test_case(true; "catalog_model_count")]
    fn parsed_count_limits(catalog: bool) {
        let mut snapshot = fixture();
        if catalog {
            let models = &mut snapshot.payload.catalog.get_mut("catalog").unwrap().models;
            let model = CatalogModel::default();
            models.clear();
            models.extend((0..=MAX_CATALOG_MODELS).map(|i| (i.to_string(), model.clone())));
        } else {
            snapshot
                .payload
                .providers
                .resize(MAX_PROVIDERS + 1, snapshot.payload.providers[0].clone());
        }
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        let error = parse_bounded(&bytes).unwrap_err();
        assert!(matches!(
            (catalog, error),
            (true, ParseError::TooManyCatalogModels { .. })
                | (false, ParseError::TooManyProviders { .. })
        ));
    }

    #[test]
    fn json_depth_is_bounded() {
        let mut value = Value::Null;
        for _ in 0..128 {
            value = json!([value]);
        }
        let mut snapshot = serde_json::to_value(fixture()).unwrap();
        snapshot["future_field"] = value;
        assert!(matches!(
            parse_bounded(&serde_json::to_vec(&snapshot).unwrap()),
            Err(ParseError::Json(_))
        ));
    }
}
