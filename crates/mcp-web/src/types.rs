use serde::{Deserialize, Serialize};

use crate::WebsearchBackend;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WebfetchFormat {
    #[default]
    Markdown,
    Text,
    Html,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WebfetchPdfMode {
    #[default]
    Extract,
    Attachment,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebsearchInput {
    pub query: String,
    pub country: Option<String>,
    pub categories: Option<String>,
    pub language: Option<String>,
    pub pageno: Option<u64>,
    pub time_range: Option<TimeRange>,
    pub safesearch: Option<u8>,
    pub limit: Option<u64>,
    #[serde(rename = "timeoutSec")]
    pub timeout_sec: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimeRange {
    Day,
    Month,
    Year,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WebfetchInput {
    pub url: String,
    #[serde(default)]
    pub format: WebfetchFormat,
    #[serde(default)]
    pub pdf_mode: WebfetchPdfMode,
    pub timeout: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebsearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_data_url: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebsearchOutput {
    pub kind: &'static str,
    pub backend: Option<WebsearchBackend>,
    pub query: String,
    pub results_found: usize,
    pub results: Vec<WebsearchResult>,
    /// Model-facing projection used for the MCP content block, not duplicated in structured output.
    #[serde(skip_serializing)]
    pub(crate) formatted_results: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebfetchPdfAttachment {
    #[serde(rename = "type")]
    pub attachment_type: &'static str,
    pub mime: &'static str,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    pub size_bytes: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebfetchOutput {
    pub kind: &'static str,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    pub format: WebfetchFormat,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pdf_mode: Option<WebfetchPdfMode>,
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_input: Option<String>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pdf_attachment: Option<WebfetchPdfAttachment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extraction_method: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extraction_low_signal: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_data_url: Option<String>,
}
