use std::cmp::Ordering;
use std::collections::HashMap;
#[cfg(feature = "mcp")]
use std::sync::Arc;

#[cfg(feature = "mcp")]
use rmcp::model::{MetaObject, Tool, ToolAnnotations};
use schemars::{JsonSchema, SchemaGenerator, generate::SchemaSettings};
use serde_json::{Map, Value, json};

use crate::search::provider::ProviderCatalogContract;
use crate::{
    WebfetchOutput, WebsearchBackend, WebsearchExecutionConfiguration, WebsearchMode,
    WebsearchOutput,
};
use workcell_tool_contract::{ToolAnnotations as NeutralAnnotations, ToolContract, ToolSpec};

const BACKEND_PROPERTY: &str = "backend";
const DESCRIPTION_KEY: &str = "description";
const MINIMUM_KEY: &str = "minimum";
const MAXIMUM_KEY: &str = "maximum";
const MAX_LENGTH_KEY: &str = "maxLength";
const MIN_LENGTH_KEY: &str = "minLength";

const WEBFETCH_DESCRIPTION: &str = r#"Fetch content from a URL and return model-facing text.

- The url parameter must be http or https.
- The format parameter can be markdown, text, or html. It defaults to markdown.
- The pdfMode parameter can be extract or attachment. It defaults to extract.
- The timeout parameter is optional, in seconds, defaults to 30, and is capped at 60.
- HTML pages are simplified for markdown/text output and script/style content is removed.
- PDF responses are parsed to extracted text by default. Use pdfMode='attachment' to preserve the PDF as a data:application/pdf attachment without text extraction. When the host limits attachments, a PDF over the limit is returned as extracted text that says why.
- Model-facing output is bounded and may be truncated; truncated output ends with a line naming the limit that cut it. Structured metadata preserves URL, content type, format, status, and source icon metadata when available.
- Use websearch first when you need to discover candidate URLs."#;

/// Return fresh tools in compatibility order: `websearch`, then `webfetch`.
#[must_use]
#[cfg(feature = "mcp")]
pub fn catalog(current_year: i32, configuration: &WebsearchExecutionConfiguration) -> Vec<Tool> {
    specs(current_year, configuration)
        .iter()
        .map(to_mcp_tool)
        .collect()
}

#[must_use]
pub fn specs(current_year: i32, configuration: &WebsearchExecutionConfiguration) -> Vec<ToolSpec> {
    vec![
        websearch_spec(current_year, configuration),
        spec(
            "webfetch",
            None,
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
            "web.fetch.v1",
            output_schema::<WebfetchOutput>(),
        ),
    ]
}

fn websearch_spec(current_year: i32, configuration: &WebsearchExecutionConfiguration) -> ToolSpec {
    let contracts = configuration
        .providers()
        .iter()
        .map(|provider| (provider.backend(), provider.catalog_contract(current_year)))
        .collect::<Vec<_>>();
    let (description, properties) = match contracts.as_slice() {
        [] => {
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
        }
        [_, _, ..] if configuration.mode() == WebsearchMode::All => {
            all_backends_contract(current_year, &contracts)
        }
        [(primary_backend, primary), rest @ ..] => {
            let mut description = primary.description.clone();
            if !rest.is_empty() {
                let fallbacks = rest
                    .iter()
                    .map(|(backend, _)| backend.as_str())
                    .collect::<Vec<_>>()
                    .join(", then ");
                description.push_str(&format!(
                    "\n\nIf {} fails, this search falls back to {fallbacks}.",
                    primary_backend.as_str()
                ));
            }
            (description, primary.properties.clone())
        }
    };
    spec(
        "websearch",
        None,
        description,
        schema(json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["query"],
            "properties": properties
        })),
        "web.search.v1",
        "web.search.v1",
        output_schema::<WebsearchOutput>(),
    )
}

/// One schema for several backends the model chooses between: the union of their
/// parameters with the loosest bounds, each parameter naming the backends that
/// accept it when not all do, and a `backend` selector.
fn all_backends_contract(
    current_year: i32,
    contracts: &[(WebsearchBackend, ProviderCatalogContract)],
) -> (String, Map<String, Value>) {
    let names = contracts
        .iter()
        .map(|(backend, _)| backend.as_str())
        .collect::<Vec<_>>();
    let order = names.join(", ");
    let year_sentence = format!("The current year is {current_year}.");
    let summaries = contracts
        .iter()
        .map(|(backend, contract)| {
            let summary = contract.description.lines().next().unwrap_or_default();
            let summary = summary.replace(&year_sentence, "");
            format!("- {}: {}", backend.as_str(), summary.trim_end())
        })
        .collect::<Vec<_>>()
        .join("\n");
    let description = format!(
        "Search the web with one of several configured backends. Pass backend to pick one, or omit it to try them in order: {order}.\n\n{summaries}\n\n{year_sentence}"
    );

    let mut properties = Map::new();
    let mut acceptors = HashMap::<&str, Vec<&str>>::new();
    for (backend, contract) in contracts {
        for (name, schema) in &contract.properties {
            match properties.get_mut(name) {
                Some(existing) => merge_property(existing, schema),
                None => {
                    properties.insert(name.clone(), schema.clone());
                }
            }
            acceptors
                .entry(name.as_str())
                .or_default()
                .push(backend.as_str());
        }
    }
    for (name, accepted) in acceptors {
        if accepted.len() == contracts.len() {
            continue;
        }
        if let Some(Value::Object(schema)) = properties.get_mut(name) {
            let accepted = accepted.join(", ");
            let note = match schema.get(DESCRIPTION_KEY).and_then(Value::as_str) {
                Some(description) => format!("{description} Accepted by: {accepted}."),
                None => format!("Accepted by: {accepted}."),
            };
            schema.insert(DESCRIPTION_KEY.to_owned(), Value::String(note));
        }
    }
    properties.insert(
        BACKEND_PROPERTY.to_owned(),
        json!({
            "type": "string",
            "enum": names,
            "description": format!("Websearch backend to use. Omit it to try the backends in order: {order}.")
        }),
    );
    (description, properties)
}

/// Widen `existing` so every value either backend accepts stays valid. A bound
/// only one side declares is dropped, since the other side leaves it open.
fn merge_property(existing: &mut Value, other: &Value) {
    let (Value::Object(existing), Value::Object(other)) = (existing, other) else {
        return;
    };
    for (key, loosest) in [
        (MINIMUM_KEY, Ordering::Less),
        (MAXIMUM_KEY, Ordering::Greater),
        (MAX_LENGTH_KEY, Ordering::Greater),
    ] {
        let merged = match (existing.get(key), other.get(key)) {
            (Some(current), Some(candidate)) => {
                let looser = match (current.as_f64(), candidate.as_f64()) {
                    (Some(current), Some(candidate)) => candidate.total_cmp(&current) == loosest,
                    _ => false,
                };
                Some(if looser { candidate } else { current }.clone())
            }
            _ => None,
        };
        match merged {
            Some(value) => existing.insert(key.to_owned(), value),
            None => existing.remove(key),
        };
    }
    if existing.get(MIN_LENGTH_KEY) != other.get(MIN_LENGTH_KEY) {
        existing.remove(MIN_LENGTH_KEY);
    }
    if !existing.contains_key(DESCRIPTION_KEY)
        && let Some(description) = other.get(DESCRIPTION_KEY)
    {
        existing.insert(DESCRIPTION_KEY.to_owned(), description.clone());
    }
}

fn spec(
    name: &'static str,
    title: Option<&'static str>,
    description: String,
    input_schema: Map<String, Value>,
    profile: &'static str,
    contract_id: &'static str,
    output_schema: Map<String, Value>,
) -> ToolSpec {
    ToolSpec::new(
        name,
        title,
        description,
        input_schema,
        NeutralAnnotations {
            read_only_hint: Some(true),
            destructive_hint: None,
            idempotent_hint: None,
            open_world_hint: Some(true),
        },
        profile,
        ToolContract::new(contract_id, "v1", "v1"),
    )
    .with_output_schema(output_schema)
}

#[cfg(feature = "mcp")]
fn to_mcp_tool(spec: &ToolSpec) -> Tool {
    let tool = Tool::new(
        spec.name,
        spec.description.clone(),
        Arc::new(spec.input_schema.clone()),
    );
    let tool = match spec.title {
        Some(title) => tool.with_title(title),
        None => tool,
    };
    tool.with_raw_output_schema(Arc::new(
        spec.output_schema.clone().expect("web output schema"),
    ))
    .with_annotations(ToolAnnotations::from_raw(
        None,
        spec.annotations.read_only_hint,
        spec.annotations.destructive_hint,
        spec.annotations.idempotent_hint,
        spec.annotations.open_world_hint,
    ))
    .with_meta(MetaObject(spec.extension_metadata()))
}

fn output_schema<T: JsonSchema>() -> Map<String, Value> {
    Value::from(SchemaGenerator::new(SchemaSettings::draft07()).into_root_schema_for::<T>())
        .as_object()
        .expect("output schema is an object")
        .clone()
}

fn schema(value: Value) -> Map<String, Value> {
    value.as_object().expect("schema is an object").clone()
}
