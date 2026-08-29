#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

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
        contract_id: &'static str,
    ) -> Self {
        Self {
            name,
            title,
            description: description.into(),
            input_schema,
            output_schema: None,
            annotations,
            presentation,
            contract_id,
        }
    }

    #[must_use]
    pub fn with_output_schema(mut self, output_schema: Map<String, Value>) -> Self {
        self.output_schema = Some(output_schema);
        self
    }
}
