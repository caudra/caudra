use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{ALLOWED_NPM, BLOCKED_CATALOG_PROVIDERS, ReasoningOption};

pub fn import_models_dev(
    api_json: &[u8],
    builtin_catalog_slugs: &BTreeSet<String>,
) -> Result<BTreeMap<String, CatalogProvider>, serde_json::Error> {
    let providers: BTreeMap<String, Value> = serde_json::from_slice(api_json)?;
    providers
        .into_iter()
        .filter(|(slug, provider)| {
            builtin_catalog_slugs.contains(slug)
                || (!BLOCKED_CATALOG_PROVIDERS.contains(&slug.as_str())
                    && provider["npm"]
                        .as_str()
                        .is_some_and(|npm| ALLOWED_NPM.contains(&npm)))
        })
        .map(|(slug, provider)| {
            let mut provider: CatalogProvider = serde_json::from_value(provider)?;
            // models.dev also publishes environment templates, not usable base URLs.
            if provider
                .api
                .as_deref()
                .is_some_and(|api| api.starts_with("${"))
            {
                provider.api = None;
            }
            Ok((slug, provider))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CatalogProvider {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    pub npm: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_models",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub models: BTreeMap<String, CatalogModel>,
}

fn deserialize_models<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, CatalogModel>, D::Error> {
    let raw = BTreeMap::<String, Value>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|(id, value)| serde_json::from_value(value).ok().map(|model| (id, model)))
        .collect())
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CatalogModel {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<CatalogLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CatalogCost>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<CatalogShape>,
    #[serde(default)]
    pub attachment: bool,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modalities: Option<CatalogModalities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_options: Option<Vec<ReasoningOption>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_updated: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CatalogModalities {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CatalogLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CatalogCost {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<CatalogCostTier>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CatalogCostTier {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    pub tier: CatalogTierBoundary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CatalogTierBoundary {
    pub size: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CatalogShape {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use std::collections::BTreeSet;
    use test_case::test_case;

    use super::{CatalogProvider, import_models_dev};

    #[test]
    fn import_keeps_builtin_metadata_and_every_allowed_model() {
        let row = json!({"name": "Example", "npm": "@ai-sdk/openai-compatible", "models": {"curated": {}, "other": {}}});
        let mut unsupported = row.clone();
        unsupported["npm"] = json!("custom-sdk");
        let input = json!({
            "allowed": row, "github-copilot": row, "zai-coding-plan": row,
            "builtin": unsupported, "unsupported": unsupported
        });
        let imported = import_models_dev(
            &serde_json::to_vec(&input).unwrap(),
            &BTreeSet::from(["builtin".to_owned(), "github-copilot".to_owned()]),
        )
        .unwrap();
        assert_eq!(
            imported.keys().map(String::as_str).collect::<Vec<_>>(),
            ["allowed", "builtin", "github-copilot"]
        );
        assert_eq!(imported["builtin"].npm, "custom-sdk");
        assert_eq!(imported["allowed"].models.len(), 2);
    }

    #[test]
    fn import_keeps_models_but_not_an_unresolved_endpoint_template() {
        let input = br#"{"neon":{"name":"Neon","npm":"@ai-sdk/openai-compatible","api":"${NEON_AI_GATEWAY_BASE_URL}/v1","models":{"model":{}}}}"#;
        let imported = import_models_dev(input, &BTreeSet::new()).unwrap();
        assert_eq!(imported["neon"].api, None);
        assert!(imported["neon"].models.contains_key("model"));
    }

    #[test_case(json!({"limit": {"context": "unreadable"}}); "invalid_limit")]
    #[test_case(json!({"cost": {"input": []}}); "invalid_cost")]
    #[test_case(json!(false); "invalid_model")]
    fn unreadable_model_is_dropped_without_losing_other_models(unreadable: Value) {
        let provider: CatalogProvider = serde_json::from_value(json!({
            "name": "Example", "npm": "@ai-sdk/openai-compatible",
            "models": {"broken": unreadable, "good": {"limit": {"context": 32000}}}
        }))
        .unwrap();
        assert_eq!(provider.models.len(), 1);
        assert!(provider.models.contains_key("good"));
    }
}
