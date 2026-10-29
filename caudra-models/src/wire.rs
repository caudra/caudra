use std::collections::BTreeMap;

use jiff::{Timestamp, civil::Date};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::catalog::CatalogProvider;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Snapshot {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub format: u32,
    #[cfg_attr(feature = "schema", schemars(with = "String"))]
    pub updated_at: Timestamp,
    pub content_sha256: String,
    #[serde(flatten)]
    pub payload: Payload,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Payload {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<Provider>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decision_models: Vec<DecisionModel>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub catalog: BTreeMap<String, CatalogProvider>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Provider {
    pub slug: String,
    pub display_name: String,
    pub family: Family,
    #[serde(default)]
    pub supports_thinking: bool,
    #[serde(default)]
    pub accepts_arbitrary_models: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_max_output: Option<u32>,
    pub fallback_context_window: u32,
    #[serde(default)]
    pub serves_fast_mode: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_schedule: Option<PricingSchedule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_from: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub generations: Vec<Generation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub price_periods: Vec<PricePeriod>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub availability: Vec<Availability>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelEntry {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prefixes: Vec<String>,
    #[serde(default)]
    pub small: bool,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub vision: bool,
    pub family: Family,
    pub pricing: Pricing,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    pub context_window: u32,
    #[serde(default, skip_serializing_if = "is_false")]
    pub window_excludes_output: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_options: Option<Vec<ReasoningOption>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Period {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>"))]
    pub from: Option<Date>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>"))]
    pub until: Option<Date>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Pricing {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast: Option<FastPricing>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<Tier>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FastPricing {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Tier {
    pub above: u32,
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast: Option<FastPricing>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PricingSchedule {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<PricingWindow>,
    pub multiplier: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PricingWindow {
    pub start: u8,
    pub end: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Generation {
    pub label: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PricePeriod {
    pub prefix: String,
    pub during: Period,
    pub pricing: Pricing,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Availability {
    pub prefix: String,
    pub during: Period,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionModel {
    pub protocol: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prefixes: Vec<String>,
    pub pricing: Pricing,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Family {
    Claude,
    Generic,
    Gemini,
    Glm,
    Gpt,
    Synthetic,
    Unknown(String),
}

impl Serialize for Family {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Claude => "claude",
            Self::Generic => "generic",
            Self::Gemini => "gemini",
            Self::Glm => "glm",
            Self::Gpt => "gpt",
            Self::Synthetic => "synthetic",
            Self::Unknown(value) => value,
        })
    }
}

impl<'de> Deserialize<'de> for Family {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "claude" => Self::Claude,
            "generic" => Self::Generic,
            "gemini" => Self::Gemini,
            "glm" => Self::Glm,
            "gpt" => Self::Gpt,
            "synthetic" => Self::Synthetic,
            value => Self::Unknown(value.to_owned()),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningOption {
    Toggle,
    Effort {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        values: Vec<String>,
    },
    BudgetTokens {
        #[serde(skip_serializing_if = "Option::is_none")]
        min: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max: Option<u32>,
    },
    Unknown,
}

impl<'de> Deserialize<'de> for ReasoningOption {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Wire {
            Toggle,
            Effort {
                #[serde(default)]
                values: Vec<String>,
            },
            BudgetTokens {
                #[serde(default, deserialize_with = "deserialize_bound")]
                min: Option<u32>,
                #[serde(default, deserialize_with = "deserialize_bound")]
                max: Option<u32>,
            },
            #[serde(other)]
            Unknown,
        }

        Ok(match Wire::deserialize(deserializer)? {
            Wire::Toggle => Self::Toggle,
            Wire::Effort { values } => Self::Effort { values },
            Wire::BudgetTokens { min, max } => Self::BudgetTokens { min, max },
            Wire::Unknown => Self::Unknown,
        })
    }
}

fn deserialize_bound<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    let raw = Option::<i64>::deserialize(deserializer)?;
    Ok(raw.and_then(|value| u32::try_from(value).ok()))
}

fn is_false(value: &bool) -> bool {
    !value
}

impl Provider {
    pub fn unsupported_reason(&self, supported: &[&str]) -> Option<String> {
        unsupported_reason(&self.family, &self.requires, supported)
    }
}

impl ModelEntry {
    pub fn unsupported_reason(&self, supported: &[&str]) -> Option<String> {
        unsupported_reason(&self.family, &self.requires, supported)
    }
}

fn unsupported_reason(family: &Family, requires: &[String], supported: &[&str]) -> Option<String> {
    if let Family::Unknown(family) = family {
        return Some(format!("family {family:?} needs a newer Caudra"));
    }
    requires
        .iter()
        .find(|requirement| !supported.contains(&requirement.as_str()))
        .map(|requirement| format!("requirement {requirement:?} needs a newer Caudra"))
}

#[cfg(feature = "schema")]
mod schema {
    use std::borrow::Cow;

    use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};

    use super::{Family, ReasoningOption};

    impl JsonSchema for Family {
        fn schema_name() -> Cow<'static, str> {
            "Family".into()
        }

        fn json_schema(_: &mut SchemaGenerator) -> Schema {
            json_schema!({
                "type": "string",
                "description": "Known families are claude, generic, gemini, glm, gpt, synthetic. Other strings require a newer Caudra.",
                "examples": ["claude", "generic", "gemini", "glm", "gpt", "synthetic"]
            })
        }
    }

    impl JsonSchema for ReasoningOption {
        fn schema_name() -> Cow<'static, str> {
            "ReasoningOption".into()
        }

        fn json_schema(_: &mut SchemaGenerator) -> Schema {
            json_schema!({
                "type": "object",
                "required": ["type"],
                "oneOf": [
                    {"properties": {"type": {"const": "toggle"}}},
                    {"properties": {"type": {"const": "effort"}, "values": {"type": "array", "items": {"type": "string"}}}},
                    {"properties": {"type": {"const": "budget_tokens"}, "min": {"type": ["integer", "null"]}, "max": {"type": ["integer", "null"]}}},
                    {"properties": {"type": {"type": "string", "not": {"enum": ["toggle", "effort", "budget_tokens"]}}}}
                ]
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "schema")]
    use schemars::schema_for;
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::{Family, ModelEntry, ReasoningOption};
    use crate::{parse_bounded, tests::fixture};

    const FAMILY_MESSAGE: &str = "family \"future\" needs a newer Caudra";
    const REQUIREMENT_MESSAGE: &str = "requirement \"future\" needs a newer Caudra";

    #[test_case("claude", Family::Claude; "claude")]
    #[test_case("generic", Family::Generic; "generic")]
    #[test_case("gemini", Family::Gemini; "gemini")]
    #[test_case("glm", Family::Glm; "glm")]
    #[test_case("gpt", Family::Gpt; "gpt")]
    #[test_case("synthetic", Family::Synthetic; "synthetic")]
    #[test_case("future", Family::Unknown("future".to_owned()); "unknown")]
    fn families_round_trip(text: &str, expected: Family) {
        let value = json!(text);
        assert_eq!(
            serde_json::from_value::<Family>(value.clone()).unwrap(),
            expected
        );
        assert_eq!(serde_json::to_value(expected).unwrap(), value);
    }

    #[test_case(json!({"type": "toggle"}), ReasoningOption::Toggle; "toggle")]
    #[test_case(json!({"type": "effort", "values": ["low"]}), ReasoningOption::Effort { values: vec!["low".to_owned()] }; "effort")]
    #[test_case(json!({"type": "budget_tokens", "min": 1, "max": 2}), ReasoningOption::BudgetTokens { min: Some(1), max: Some(2) }; "budget")]
    #[test_case(json!({"type": "budget_tokens", "min": -1, "max": -10}), ReasoningOption::BudgetTokens { min: None, max: None }; "negative_budget")]
    #[test_case(json!({"type": "budget_tokens"}), ReasoningOption::BudgetTokens { min: None, max: None }; "absent_budget")]
    #[test_case(json!({"type": "future", "future_field": {"foo": [1]}}), ReasoningOption::Unknown; "unknown")]
    fn reasoning_options_parse(value: Value, expected: ReasoningOption) {
        let option: ReasoningOption = serde_json::from_value(value).unwrap();
        assert_eq!(option, expected);
        assert_eq!(
            serde_json::from_value::<ReasoningOption>(serde_json::to_value(option).unwrap())
                .unwrap(),
            expected
        );
    }

    #[test]
    fn unknown_enums_do_not_reject_snapshot() {
        let mut value = serde_json::to_value(fixture()).unwrap();
        value["providers"][0]["family"] = json!("future");
        value["providers"][0]["models"][0]["reasoning_options"] = json!([{"type": "future"}]);
        let snapshot = parse_bounded(&serde_json::to_vec(&value).unwrap()).unwrap();
        let provider = &snapshot.payload.providers[0];
        assert_eq!(provider.family, Family::Unknown("future".to_owned()));
        assert_eq!(
            provider.models[0].reasoning_options,
            Some(vec![ReasoningOption::Unknown])
        );
    }

    #[test_case(true; "provider")]
    #[test_case(false; "model")]
    fn unsupported_classification_checks_family_then_requirements(provider: bool) {
        let mut snapshot = fixture();
        let entry = &mut snapshot.payload.providers[0];
        if provider {
            assert_eq!(entry.unsupported_reason(&[]), None);
            entry.requires = vec!["future".to_owned()];
            assert_eq!(
                entry.unsupported_reason(&[]).as_deref(),
                Some(REQUIREMENT_MESSAGE)
            );
            assert_eq!(entry.unsupported_reason(&["future"]), None);
            entry.family = Family::Unknown("future".to_owned());
            assert_eq!(
                entry.unsupported_reason(&["future"]).as_deref(),
                Some(FAMILY_MESSAGE)
            );
        } else {
            let entry = &mut entry.models[0];
            assert_eq!(entry.unsupported_reason(&[]), None);
            entry.requires = vec!["future".to_owned()];
            assert_eq!(
                entry.unsupported_reason(&[]).as_deref(),
                Some(REQUIREMENT_MESSAGE)
            );
            assert_eq!(entry.unsupported_reason(&["future"]), None);
            entry.family = Family::Unknown("future".to_owned());
            assert_eq!(
                entry.unsupported_reason(&["future"]).as_deref(),
                Some(FAMILY_MESSAGE)
            );
        }
    }

    #[test_case(false; "default_false_is_omitted")]
    #[test_case(true; "input_budget_is_explicit")]
    fn input_budget_flag_round_trips(excludes_output: bool) {
        let mut model = fixture().payload.providers.remove(0).models.remove(0);
        model.window_excludes_output = excludes_output;
        let value = serde_json::to_value(&model).unwrap();
        assert_eq!(
            value.get("window_excludes_output").is_some(),
            excludes_output
        );
        assert_eq!(serde_json::from_value::<ModelEntry>(value).unwrap(), model);
    }

    #[test]
    fn optional_fields_are_omitted_but_explicit_empty_reasoning_is_preserved() {
        let mut model = fixture().payload.providers.remove(0).models.remove(0);
        model.reasoning_options = None;
        let absent = serde_json::to_value(&model).unwrap();
        for field in ["note", "source", "requires", "reasoning_options"] {
            assert!(absent.get(field).is_none());
        }
        model.reasoning_options = Some(Vec::new());
        assert_eq!(
            serde_json::to_value(&model).unwrap()["reasoning_options"],
            json!([])
        );
    }

    #[cfg(feature = "schema")]
    #[test]
    fn generated_model_schema_describes_open_enums() {
        let schema = serde_json::to_value(schema_for!(ModelEntry)).unwrap();
        let family = &schema["$defs"]["Family"];
        assert_eq!(family["type"], json!("string"));
        assert!(family.get("enum").is_none());
        assert_eq!(
            family["examples"],
            json!(["claude", "generic", "gemini", "glm", "gpt", "synthetic"])
        );
        let reasoning = &schema["$defs"]["ReasoningOption"];
        assert_eq!(reasoning["required"], json!(["type"]));
        assert_eq!(
            reasoning["oneOf"][3]["properties"]["type"],
            json!({
                "type": "string", "not": {"enum": ["toggle", "effort", "budget_tokens"]}
            })
        );
    }
}
