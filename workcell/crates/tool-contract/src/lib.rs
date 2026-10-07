#![forbid(unsafe_code)]

use std::fmt::Write;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub const CONTRACT_METADATA_KEY: &str = "ai.workcell/contract";
pub const MANIFEST_VERSION: &str = "v2";
pub const PRESENTATION_METADATA_KEY: &str = "ai.workcell/presentation-profile";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolContract {
    pub id: &'static str,
    pub version: &'static str,
    pub result_version: &'static str,
}

impl ToolContract {
    #[must_use]
    pub const fn new(
        id: &'static str,
        version: &'static str,
        result_version: &'static str,
    ) -> Self {
        Self {
            id,
            version,
            result_version,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub title: Option<&'static str>,
    pub description: String,
    pub input_schema: Map<String, Value>,
    pub output_schema: Option<Map<String, Value>>,
    pub annotations: ToolAnnotations,
    pub presentation: &'static str,
    pub contract_id: &'static str,
    pub contract_version: &'static str,
    pub result_version: &'static str,
}

impl ToolSpec {
    #[must_use]
    pub fn new(
        name: &'static str,
        title: Option<&'static str>,
        description: impl Into<String>,
        input_schema: Map<String, Value>,
        annotations: ToolAnnotations,
        presentation: &'static str,
        contract: ToolContract,
    ) -> Self {
        Self {
            name,
            title,
            description: description.into(),
            input_schema,
            output_schema: None,
            annotations,
            presentation,
            contract_id: contract.id,
            contract_version: contract.version,
            result_version: contract.result_version,
        }
    }

    #[must_use]
    pub fn with_output_schema(mut self, output_schema: Map<String, Value>) -> Self {
        self.output_schema = Some(output_schema);
        self
    }

    #[must_use]
    pub fn extension_metadata(&self) -> Map<String, Value> {
        Map::from_iter([
            (
                PRESENTATION_METADATA_KEY.to_owned(),
                Value::String(self.presentation.to_owned()),
            ),
            (
                CONTRACT_METADATA_KEY.to_owned(),
                serde_json::json!({
                    "id": self.contract_id,
                    "version": self.contract_version,
                    "resultVersion": self.result_version,
                }),
            ),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CatalogRevision(String);

impl CatalogRevision {
    pub fn for_serializable(value: &impl Serialize) -> Result<Self, serde_json::Error> {
        let encoded = serde_json::to_vec(&canonicalize(serde_json::to_value(value)?))?;
        let digest = Sha256::digest(encoded);
        let mut revision = String::with_capacity(71);
        revision.push_str("sha256:");
        for byte in digest {
            write!(revision, "{byte:02x}").expect("writing to a string cannot fail");
        }
        Ok(Self(revision))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnedToolSpec {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub description: String,
    pub input_schema: Map<String, Value>,
    pub output_schema: Option<Map<String, Value>>,
    pub annotations: ToolAnnotations,
    pub presentation: String,
    pub contract_id: String,
    pub contract_version: String,
    pub result_version: String,
}

impl From<&ToolSpec> for OwnedToolSpec {
    fn from(spec: &ToolSpec) -> Self {
        Self {
            name: spec.name.to_owned(),
            title: spec.title.map(str::to_owned),
            description: spec.description.clone(),
            input_schema: canonicalize_map(spec.input_schema.clone()),
            output_schema: spec.output_schema.clone().map(canonicalize_map),
            annotations: spec.annotations,
            presentation: spec.presentation.to_owned(),
            contract_id: spec.contract_id.to_owned(),
            contract_version: spec.contract_version.to_owned(),
            result_version: spec.result_version.to_owned(),
        }
    }
}

fn canonicalize(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize).collect()),
        Value::Object(values) => {
            let mut entries = values.into_iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, canonicalize(value)))
                    .collect(),
            )
        }
        value => value,
    }
}

fn canonicalize_map(value: Map<String, Value>) -> Map<String, Value> {
    let Value::Object(value) = canonicalize(Value::Object(value)) else {
        unreachable!("object canonicalization preserves the value kind")
    };
    value
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolManifest {
    pub version: String,
    pub revision: CatalogRevision,
    pub tools: Vec<OwnedToolSpec>,
}

impl ToolManifest {
    pub fn new(specs: &[ToolSpec]) -> Result<Self, serde_json::Error> {
        let version = MANIFEST_VERSION.to_owned();
        let tools = specs.iter().map(OwnedToolSpec::from).collect::<Vec<_>>();
        let revision = CatalogRevision::for_serializable(&(&version, &tools))?;
        Ok(Self {
            version,
            revision,
            tools,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn spec(description: &str) -> ToolSpec {
        ToolSpec::new(
            "example",
            Some("Example"),
            description,
            Map::new(),
            ToolAnnotations::default(),
            "example.result.v1",
            ToolContract::new("example.v1", "v1", "v1"),
        )
        .with_output_schema(Map::new())
    }

    #[test]
    fn manifests_are_owned_serializable_and_content_addressed() {
        let first = ToolManifest::new(&[spec("first")]).unwrap();
        let same = ToolManifest::new(&[spec("first")]).unwrap();
        let changed = ToolManifest::new(&[spec("changed")]).unwrap();

        assert_eq!(first, same);
        assert_ne!(first.revision, changed.revision);
        assert_eq!(first.tools[0].contract_version, "v1");
        assert_eq!(first.tools[0].result_version, "v1");
        assert_eq!(
            serde_json::to_value(&first).unwrap()["version"],
            json!(MANIFEST_VERSION)
        );
    }

    #[test]
    fn revisions_are_recursive_key_order_independent_and_array_order_sensitive() {
        let first = json!({"outer":{"b":2,"a":1},"items":[{"d":4,"c":3}, 5]});
        let reordered = json!({"items":[{"c":3,"d":4}, 5],"outer":{"a":1,"b":2}});
        let changed_order = json!({"outer":{"a":1,"b":2},"items":[5,{"c":3,"d":4}]});

        assert_eq!(
            CatalogRevision::for_serializable(&first).unwrap(),
            CatalogRevision::for_serializable(&reordered).unwrap()
        );
        assert_ne!(
            CatalogRevision::for_serializable(&first).unwrap(),
            CatalogRevision::for_serializable(&changed_order).unwrap()
        );
    }

    #[test]
    fn canonical_revision_has_a_fixed_value() {
        let revision = CatalogRevision::for_serializable(&json!({
            "z": [3, {"b": true, "a": null}],
            "a": "workcell"
        }))
        .unwrap();

        assert_eq!(
            revision.as_str(),
            "sha256:ac3d60f41ec121492c66df5d58e1b6261885776c699b2bed9cceb9f0f057e43a"
        );
    }

    #[test]
    fn extension_metadata_has_one_uniform_contract_shape() {
        assert_eq!(
            spec("example").extension_metadata(),
            Map::from_iter([
                (
                    PRESENTATION_METADATA_KEY.to_owned(),
                    json!("example.result.v1"),
                ),
                (
                    CONTRACT_METADATA_KEY.to_owned(),
                    json!({"id":"example.v1","version":"v1","resultVersion":"v1"}),
                ),
            ])
        );
    }
}
