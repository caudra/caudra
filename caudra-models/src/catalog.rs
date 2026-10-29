use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::validate::{URL_MESSAGE, allowed_api};
use crate::{ALLOWED_NPM, BLOCKED_CATALOG_PROVIDERS, ReasoningOption};

#[derive(Debug, PartialEq)]
pub struct Import {
    pub catalog: BTreeMap<String, CatalogProvider>,
    pub dropped_apis: Vec<DroppedApi>,
}

/// An upstream base URL the import left out, such as models.dev's
/// `${NEON_AI_GATEWAY_BASE_URL}/v1` template or a public `http://` host.
#[derive(Debug, PartialEq, Eq)]
pub struct DroppedApi {
    pub provider: String,
    pub api: String,
}

impl fmt::Display for DroppedApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: dropped api {:?}: {URL_MESSAGE}",
            self.provider, self.api
        )
    }
}

pub fn import_models_dev(
    api_json: &[u8],
    builtin_catalog_slugs: &BTreeSet<String>,
) -> Result<Import, serde_json::Error> {
    let providers: BTreeMap<String, Value> = serde_json::from_slice(api_json)?;
    let mut dropped_apis = Vec::new();
    let catalog = providers
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
            if let Some(api) = provider.api.take_if(|api| !allowed_api(api)) {
                dropped_apis.push(DroppedApi {
                    provider: slug.clone(),
                    api,
                });
            }
            Ok((slug, provider))
        })
        .collect::<Result<_, serde_json::Error>>()?;
    Ok(Import {
        catalog,
        dropped_apis,
    })
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

    use super::{CatalogProvider, DroppedApi, import_models_dev};
    use crate::validate::URL_MESSAGE;

    const DROPPED: &str = "gateway";
    const KEPT: &str = "kept";
    const KEPT_API: &str = "https://kept.example.com/v1";

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
        .unwrap()
        .catalog;
        assert_eq!(
            imported.keys().map(String::as_str).collect::<Vec<_>>(),
            ["allowed", "builtin", "github-copilot"]
        );
        assert_eq!(imported["builtin"].npm, "custom-sdk");
        assert_eq!(imported["allowed"].models.len(), 2);
    }

    #[test_case("${NEON_AI_GATEWAY_BASE_URL}/v1"; "unresolved_template")]
    #[test_case("http://gateway.example.com/v1"; "public_http")]
    #[test_case("gateway.example.com/v1"; "no_scheme")]
    fn import_drops_an_unusable_api_and_reports_it(api: &str) {
        let input = json!({
            DROPPED: {"name": "Gateway", "npm": "@ai-sdk/openai-compatible", "api": api, "models": {"model": {}}},
            KEPT: {"name": "Kept", "npm": "@ai-sdk/openai-compatible", "api": KEPT_API},
        });
        let imported =
            import_models_dev(&serde_json::to_vec(&input).unwrap(), &BTreeSet::new()).unwrap();
        assert_eq!(imported.catalog[DROPPED].api, None);
        assert!(imported.catalog[DROPPED].models.contains_key("model"));
        assert_eq!(imported.catalog[KEPT].api.as_deref(), Some(KEPT_API));
        assert_eq!(
            imported.dropped_apis,
            [DroppedApi {
                provider: DROPPED.to_owned(),
                api: api.to_owned()
            }]
        );
        assert!(imported.dropped_apis[0].to_string().ends_with(URL_MESSAGE));
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
