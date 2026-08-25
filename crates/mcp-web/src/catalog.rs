use std::sync::Arc;

use rmcp::model::{JsonObject, MetaObject, Tool, ToolAnnotations};
use serde_json::{Value, json};

use crate::WebsearchExecutionConfiguration;

const PRESENTATION_KEY: &str = "ai.workcell/presentation-profile";

const WEBFETCH_DESCRIPTION: &str = r#"Fetch content from a URL and return model-facing text.

- The url parameter must be http or https.
- The format parameter can be markdown, text, or html. It defaults to markdown.
- The pdfMode parameter can be extract or attachment. It defaults to extract.
- The timeout parameter is optional, in seconds, defaults to 30, and is capped at 60.
- HTML pages are simplified for markdown/text output and script/style content is removed.
- PDF responses are parsed to extracted text by default. Use pdfMode='attachment' to preserve the PDF as a data:application/pdf attachment without text extraction.
- Model-facing output is bounded and may be truncated; structured metadata preserves URL, content type, format, status, and source icon metadata when available.
- Use websearch first when you need to discover candidate URLs."#;

/// Return fresh tools in compatibility order: `websearch`, then `webfetch`.
#[must_use]
pub fn catalog(current_year: i32, configuration: &WebsearchExecutionConfiguration) -> Vec<Tool> {
    vec![
        websearch_tool(current_year, configuration),
        tool(
            "webfetch",
            WEBFETCH_DESCRIPTION.to_owned(),
            schema(json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["url"],
                "properties": {
                    "url": { "type": "string", "description": "HTTP(S) URL to fetch." },
                    "format": { "type": "string", "enum": ["markdown", "text", "html"], "description": "Output format." },
                    "pdfMode": { "type": "string", "enum": ["extract", "attachment"], "description": "PDF handling mode. Defaults to extract." },
                    "timeout": { "type": "integer", "minimum": 1, "maximum": 60, "description": "Timeout in seconds. Defaults to 30, max 60." }
                }
            })),
            "web.source.v1",
        ),
    ]
}

fn websearch_tool(current_year: i32, configuration: &WebsearchExecutionConfiguration) -> Tool {
    let (description, properties) = configuration.provider().map_or_else(
        || {
            let properties = json!({
                "query": { "type": "string", "description": "The intended search query to include with the configuration diagnostic." }
            });
            (
                "Websearch is unavailable because its MCP process environment is missing or invalid. Call this tool with the intended query to receive safe, actionable configuration guidance."
                    .to_owned(),
                properties
                    .as_object()
                    .expect("unavailable properties object")
                    .clone(),
            )
        },
        |provider| {
            let contract = provider.catalog_contract(current_year);
            (contract.description, contract.properties)
        },
    );
    tool(
        "websearch",
        description,
        schema(json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["query"],
            "properties": properties
        })),
        "web.search.v1",
    )
}

fn tool(
    name: &'static str,
    description: String,
    input_schema: Arc<JsonObject>,
    profile: &str,
) -> Tool {
    let mut meta = JsonObject::new();
    meta.insert(
        PRESENTATION_KEY.to_owned(),
        Value::String(profile.to_owned()),
    );
    Tool::new(name, description, input_schema)
        .with_annotations(ToolAnnotations::from_raw(
            None,
            Some(true),
            None,
            None,
            Some(true),
        ))
        .with_meta(MetaObject(meta))
}

fn schema(value: Value) -> Arc<JsonObject> {
    Arc::new(value.as_object().expect("schema is an object").clone())
}
