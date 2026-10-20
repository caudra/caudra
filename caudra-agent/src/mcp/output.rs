//! What an MCP tool answered, kept block by block so a card can draw JSON as
//! JSON and an image as the image it was, while the model goes on reading the
//! same joined text it always has.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use caudra_providers::TokenScale;

use super::elicitation::ElicitAction;
use super::kind::McpServerKind;
use super::protocol::{CallToolContent, CallToolResult};
use super::{SEPARATOR, WIRE_SEPARATOR};
use crate::types::{Answerer, CARD_ANNOTATION_SEPARATOR, LINE_NOUN, counted};

/// How an MCP call's start summary opens, ahead of its qualified name.
pub const MCP_SUMMARY_PREFIX: &str = "mcp: ";
const JSON_LABEL: &str = "JSON";
pub(super) const IMAGE_KIND: &str = "image";
const AUDIO_KIND: &str = "audio";
pub(super) const RESOURCE_KIND: &str = "resource";
const RESOURCE_LINK_KIND: &str = "resource_link";
const LINK_NOUN: &str = "link";
const MODEL_SEPARATOR: &str = "\n";
const DISPLAY_SEPARATOR: &str = "\n\n";
const BASE64_PADDING: char = '=';

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpOutput {
    pub blocks: Vec<McpBlock>,
    /// `structuredContent` when a text block mirrors it, so the generic card
    /// draws the text once while a server's own card can still read the value.
    /// Without text it is the [`McpBlock::Structured`] block instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<McpServerKind>,
    /// The tool's display title, when the server gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The server answered with `isError`: a failure the model reads as one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interactions: Vec<McpInteraction>,
    /// Why `structuredContent` did not match the tool's `outputSchema`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_mismatch: Option<String>,
    /// The code-mode helper that answered, so the card draws the answer as that helper's. Its
    /// call row keeps it; the card is told when it is drawn.
    #[serde(skip)]
    pub helper: Option<McpHelper>,
}

/// A code-mode helper whose answer is not a server's, marked apart from the server name so a
/// server called `decisions` or `history` never reads as one. Each module's card reads only its
/// own helpers. Stored as the bare helper name, as rows always were.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpHelper {
    Decision(DecisionHelper),
    History(HistoryHelper),
}

/// A `decisions` call, its answer the JSON text of its one block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionHelper {
    Decide,
    Rerank,
}

/// A `history` call, its answer the value the snippet received, kept as
/// [`McpOutput::structured`] beside its one summary line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryHelper {
    Search,
    Read,
    Sessions,
}

/// One thing the server asked the user during the call, and what came of it.
/// Values of fields that look like credentials are never kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpInteraction {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub action: ElicitAction,
    #[serde(default, skip_serializing_if = "Answerer::is_user")]
    pub by: Answerer,
    /// `(label, value)` for each answered field, in form order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<(String, String)>,
}

/// One block of a result. Media and blobs keep only their size: the card
/// labels them and the model never read their payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpBlock {
    Text {
        text: String,
    },
    Image {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        bytes: usize,
    },
    Audio {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        bytes: usize,
    },
    Resource {
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bytes: Option<usize>,
    },
    ResourceLink {
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
    /// `structuredContent`, kept only when no text block mirrors it.
    Structured {
        value: Value,
    },
    /// What is left of a result too large to keep whole: its display text,
    /// cut, with the marker that says where the rest went.
    Excerpt {
        text: String,
    },
}

impl McpOutput {
    pub fn from_result(result: CallToolResult) -> Self {
        let mut blocks: Vec<McpBlock> = result
            .content
            .into_iter()
            .map(McpBlock::from_content)
            .collect();
        let mut structured = result.structured_content;
        if !blocks.iter().any(McpBlock::has_text)
            && let Some(value) = structured.take()
        {
            blocks.push(McpBlock::Structured { value });
        }
        Self {
            blocks,
            structured,
            is_error: result.is_error,
            ..Self::default()
        }
    }

    /// The call's `structuredContent`, wherever it was kept.
    pub fn structured_content(&self) -> Option<&Value> {
        self.structured.as_ref().or_else(|| {
            self.blocks.iter().find_map(|block| match block {
                McpBlock::Structured { value } => Some(value),
                _ => None,
            })
        })
    }

    /// Exactly what the joined `text` of every content block was, which is
    /// what the model has always been sent.
    pub fn model_text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(McpBlock::model_text)
            .collect::<Vec<_>>()
            .join(MODEL_SEPARATOR)
    }

    pub fn display_text(&self) -> String {
        self.blocks
            .iter()
            .map(McpBlock::display_text)
            .collect::<Vec<_>>()
            .join(DISPLAY_SEPARATOR)
    }

    pub fn is_empty(&self) -> bool {
        self.blocks
            .iter()
            .all(|block| matches!(block, McpBlock::Text { text } if text.is_empty()))
    }

    /// `JSON · 12 lines · 1 image`: what kind of text came back, how much of
    /// it, and what else came with it.
    pub fn annotation(&self, scale: TokenScale) -> Option<String> {
        let bodies: Vec<(String, bool)> = self.blocks.iter().filter_map(McpBlock::body).collect();
        let lines: usize = bodies.iter().map(|(body, _)| body.lines().count()).sum();
        let mut parts = Vec::new();
        if lines > 0 {
            if bodies.iter().all(|(_, json)| *json) {
                parts.push(JSON_LABEL.to_owned());
            }
            parts.push(counted(lines, LINE_NOUN));
        }
        let model_text = self.model_text();
        if !model_text.is_empty() {
            parts.push(scale.label(&model_text));
        }
        for (noun, count) in [
            (
                IMAGE_KIND,
                self.count(|b| matches!(b, McpBlock::Image { .. })),
            ),
            (
                AUDIO_KIND,
                self.count(|b| matches!(b, McpBlock::Audio { .. })),
            ),
            (
                RESOURCE_KIND,
                self.count(|b| matches!(b, McpBlock::Resource { text: None, .. })),
            ),
            (
                LINK_NOUN,
                self.count(|b| matches!(b, McpBlock::ResourceLink { .. })),
            ),
        ] {
            if count > 0 {
                parts.push(counted(count, noun));
            }
        }
        (!parts.is_empty()).then(|| parts.join(CARD_ANNOTATION_SEPARATOR))
    }

    fn count(&self, kind: impl Fn(&McpBlock) -> bool) -> usize {
        self.blocks.iter().filter(|block| kind(block)).count()
    }
}

impl McpBlock {
    /// A block carrying text is read as text whatever it calls itself, so the
    /// model text cannot lose a byte to a kind this client classifies.
    fn from_content(content: CallToolContent) -> Self {
        let CallToolContent {
            kind,
            text,
            data,
            mime_type,
            resource,
            uri,
            name,
        } = content;
        if !text.is_empty() {
            return Self::Text { text };
        }
        let bytes = decoded_len(data.as_deref().unwrap_or_default());
        match kind.as_deref() {
            Some(IMAGE_KIND) => Self::Image { mime_type, bytes },
            Some(AUDIO_KIND) => Self::Audio { mime_type, bytes },
            Some(RESOURCE_KIND) => {
                let resource = resource.unwrap_or_default();
                Self::Resource {
                    uri: resource.uri,
                    mime_type: resource.mime_type,
                    text: resource.text,
                    bytes: resource.blob.as_deref().map(decoded_len),
                }
            }
            Some(RESOURCE_LINK_KIND) => Self::ResourceLink {
                uri: uri.unwrap_or_default(),
                name,
                mime_type,
            },
            _ => Self::Text { text },
        }
    }

    fn has_text(&self) -> bool {
        match self {
            Self::Text { text } => !text.is_empty(),
            Self::Resource { text, .. } => text.is_some(),
            _ => false,
        }
    }

    fn model_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } | Self::Excerpt { text } => Some(text),
            Self::Structured { .. } => None,
            _ => Some(""),
        }
    }

    /// The text a reader is shown for a block that is text, and whether it is
    /// JSON.
    fn body(&self) -> Option<(String, bool)> {
        let text = match self {
            Self::Text { text }
            | Self::Resource {
                text: Some(text), ..
            } => text,
            Self::Excerpt { text } => return Some((text.clone(), false)),
            Self::Structured { value } => {
                return Some((serde_json::to_string_pretty(value).ok()?, true));
            }
            _ => return None,
        };
        Some(match pretty_json(text) {
            Some(pretty) => (pretty, true),
            None => (text.clone(), false),
        })
    }

    fn display_text(&self) -> String {
        let body = self.body().map(|(body, _)| body);
        match self {
            Self::Image { mime_type, bytes } => media_label(IMAGE_KIND, mime_type, *bytes),
            Self::Audio { mime_type, bytes } => media_label(AUDIO_KIND, mime_type, *bytes),
            Self::Resource { uri, .. } => match body {
                Some(body) => format!("{uri}\n{body}"),
                None => uri.clone(),
            },
            Self::ResourceLink { uri, name, .. } => match name {
                Some(name) => format!("{name} {uri}"),
                None => uri.clone(),
            },
            _ => body.unwrap_or_default(),
        }
    }
}

pub(crate) fn media_label(kind: &str, mime_type: &Option<String>, bytes: usize) -> String {
    match mime_type {
        Some(mime_type) => format!("[{kind}: {mime_type}, {bytes} bytes]"),
        None => format!("[{kind}: {bytes} bytes]"),
    }
}

/// Bytes a base64 payload decodes to, without decoding it.
pub(crate) fn decoded_len(data: &str) -> usize {
    data.trim_end_matches(BASE64_PADDING).len() * 3 / 4
}

/// `text` re-indented, when the whole of it is a JSON object or array. A
/// scalar, a fragment or prose that merely contains JSON stays as it was.
pub fn pretty_json(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if !trimmed.starts_with(['{', '[']) {
        return None;
    }
    let value: Value = serde_json::from_str(trimmed).ok()?;
    serde_json::to_string_pretty(&value).ok()
}

/// `("server", "tool")` for an MCP tool name, qualified (`server.tool`) or
/// in the wire form history keeps (`server__tool`). A server name holds
/// neither separator, so whichever comes first is the boundary.
pub fn mcp_tool_parts(name: &str) -> Option<(&str, &str)> {
    let boundary = [SEPARATOR, WIRE_SEPARATOR]
        .into_iter()
        .filter_map(|separator| name.find(separator).map(|at| (at, separator.len())))
        .min()?;
    let (server, tool) = (&name[..boundary.0], &name[boundary.0 + boundary.1..]);
    (!server.is_empty() && !tool.is_empty()).then_some((server, tool))
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_providers::{estimate_tokens_cached, token_label};
    use serde_json::json;
    use test_case::test_case;

    const PRETTY_OBJECT: &str = "{\n  \"a\": 1,\n  \"b\": [\n    true\n  ]\n}";

    fn result(raw: Value) -> CallToolResult {
        serde_json::from_value(raw).unwrap()
    }

    #[test_case(json!({"content": [{"type": "text", "text": "one"}, {"type": "text", "text": "two"}]}) ; "two_texts")]
    #[test_case(json!({"content": [{"type": "text", "text": "a"}, {"type": "image", "data": "aGk=", "mimeType": "image/png"}, {"text": "b"}]}) ; "image_between_texts")]
    #[test_case(json!({"content": [{"type": "resource", "resource": {"uri": "u", "text": "t"}}]}) ; "embedded_resource")]
    #[test_case(json!({"content": [{"type": "image", "text": "stray", "data": "aGk="}]}) ; "typed_block_with_text")]
    #[test_case(json!({"content": [{"type": "mystery", "text": "x"}]}) ; "unknown_kind")]
    #[test_case(json!({"content": [], "structuredContent": {"k": 1}}) ; "structured_only")]
    #[test_case(json!({"content": []}) ; "empty")]
    fn model_text_is_the_legacy_joined_text(raw: Value) {
        let legacy = result(raw.clone()).joined_text();
        assert_eq!(McpOutput::from_result(result(raw)).model_text(), legacy);
    }

    #[test_case(json!({"content": [{"type": "text", "text": "{}"}], "structuredContent": {"k": 1}}), false ; "mirrored_by_text")]
    #[test_case(json!({"content": [], "structuredContent": {"k": 1}}), true ; "no_text")]
    fn structured_content_is_kept_only_without_text(raw: Value, kept: bool) {
        let output = McpOutput::from_result(result(raw));
        assert_eq!(
            output
                .blocks
                .iter()
                .any(|block| matches!(block, McpBlock::Structured { .. })),
            kept
        );
    }

    #[test]
    fn media_keeps_its_size_and_not_its_payload() {
        let output = McpOutput::from_result(result(
            json!({"content": [{"type": "image", "data": "aGk=", "mimeType": "image/png"}]}),
        ));
        assert_eq!(
            output.blocks,
            vec![McpBlock::Image {
                mime_type: Some("image/png".into()),
                bytes: 2
            }]
        );
    }

    #[test_case("{\"a\":1,\"b\":[true]}", Some(PRETTY_OBJECT) ; "object")]
    #[test_case("  [1]  ", Some("[\n  1\n]") ; "array")]
    #[test_case("42", None ; "scalar")]
    #[test_case("\"text\"", None ; "string")]
    #[test_case("see {\"a\": 1}", None ; "prose")]
    #[test_case("{\"a\": ", None ; "truncated")]
    fn pretty_json_accepts_whole_objects_and_arrays(text: &str, expected: Option<&str>) {
        assert_eq!(pretty_json(text).as_deref(), expected);
    }

    #[test]
    fn pretty_json_keeps_key_order() {
        assert_eq!(
            pretty_json("{\"z\":1,\"a\":2}").as_deref(),
            Some("{\n  \"z\": 1,\n  \"a\": 2\n}")
        );
    }

    #[test_case(json!({"content": [{"type": "text", "text": "{\"a\":1,\"b\":[true]}"}]}), Some("JSON · 6 lines · {tokens}") ; "json")]
    #[test_case(json!({"content": [{"type": "text", "text": "one\ntwo"}]}), Some("2 lines · {tokens}") ; "text")]
    #[test_case(json!({"content": [{"type": "text", "text": "{}"}, {"type": "text", "text": "prose"}]}), Some("2 lines · {tokens}") ; "mixed")]
    #[test_case(json!({"content": [{"type": "text", "text": "x"}, {"type": "image", "data": ""}, {"type": "resource_link", "uri": "u"}]}), Some("1 line · {tokens} · 1 image · 1 link") ; "media")]
    #[test_case(json!({"content": [], "structuredContent": {"k": 1}}), Some("JSON · 3 lines") ; "structured")]
    #[test_case(json!({"content": []}), None ; "empty")]
    fn annotation_names_the_kind_size_and_tokens(raw: Value, expected: Option<&str>) {
        let output = McpOutput::from_result(result(raw));
        let tokens = token_label(estimate_tokens_cached(&output.model_text()));
        assert_eq!(
            output.annotation(TokenScale::UNIT),
            expected.map(|expected| expected.replace("{tokens}", &tokens))
        );
    }

    #[test]
    fn display_text_pretty_prints_json_blocks() {
        let output = McpOutput::from_result(result(json!({"content": [
            {"type": "text", "text": "{\"a\":1,\"b\":[true]}"},
            {"type": "text", "text": "done"}
        ]})));
        assert_eq!(output.display_text(), format!("{PRETTY_OBJECT}\n\ndone"));
    }

    #[test_case("srv.tool", Some(("srv", "tool")) ; "qualified")]
    #[test_case("srv.a.b", Some(("srv", "a.b")) ; "dotted_tool")]
    #[test_case("shell", None ; "builtin")]
    #[test_case(".tool", None ; "no_server")]
    #[test_case("srv__tool", Some(("srv", "tool")) ; "wire")]
    #[test_case("srv.my__tool", Some(("srv", "my__tool")) ; "qualified_tool_with_wire_separator")]
    #[test_case("srv__a.b", Some(("srv", "a.b")) ; "wire_tool_with_dot")]
    fn mcp_tool_parts_splits_at_the_first_boundary(name: &str, expected: Option<(&str, &str)>) {
        assert_eq!(mcp_tool_parts(name), expected);
    }
}
