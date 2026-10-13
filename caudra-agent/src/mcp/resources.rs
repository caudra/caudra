//! What a server offers to be read rather than called: its resources, the
//! templates that name more of them, and the completions it suggests for a
//! prompt argument or a template variable.

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use caudra_providers::{ContentBlock, ImageSource, Message, Role};
use serde::Deserialize;
use serde_json::{Value, json};

use super::call::CallScope;
use super::error::McpError;
use super::output::{IMAGE_KIND, RESOURCE_KIND, decoded_len, media_label};
use super::protocol::{
    COMPLETION_COMPLETE_METHOD, EmbeddedResource, METHOD_NOT_FOUND, PromptMessage, PromptRole,
    RESOURCES_LIST_METHOD, RESOURCES_READ_METHOD, RESOURCES_TEMPLATES_LIST_METHOD,
};
use super::transport::{McpTransport, invalid_response, list_paged, send};
use crate::tools::image_bytes;

/// The most values a completion offers, which is also the most the
/// specification lets a server send.
pub const MAX_COMPLETION_VALUES: usize = 100;
/// The noun `McpError::Unsupported` names for a server without resources.
pub const RESOURCES_FEATURE: &str = "resources";
/// The noun `McpError::Unsupported` names for a server without completions.
pub const COMPLETIONS_FEATURE: &str = "completions";
const RESOURCES_FIELD: &str = "resources";
const TEMPLATES_FIELD: &str = "resourceTemplates";
const PROMPT_REF: &str = "ref/prompt";
const RESOURCE_REF: &str = "ref/resource";
const VARIABLE_OPEN: char = '{';
const VARIABLE_CLOSE: char = '}';
const IMAGE_MIME_PREFIX: &str = "image/";
const BLOB_KIND: &str = "blob";

/// One resource a server listed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resource {
    pub uri: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

impl Resource {
    /// What a reader calls it: its title, else its name.
    pub fn label(&self) -> &str {
        self.title
            .as_deref()
            .filter(|title| !title.is_empty())
            .unwrap_or(&self.name)
    }
}

/// A family of resources a server can read, named by an RFC 6570 template.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceTemplate {
    pub uri_template: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
}

impl ResourceTemplate {
    pub fn label(&self) -> &str {
        self.title
            .as_deref()
            .filter(|title| !title.is_empty())
            .unwrap_or(&self.name)
    }
}

/// A running server's resources and templates, as last listed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceLists {
    pub resources: Vec<Resource>,
    pub templates: Vec<ResourceTemplate>,
}

impl ResourceLists {
    /// Whether `uri` names something the server offers: listed as it is, or
    /// spelled the way one of its templates spells a resource.
    pub fn offers(&self, uri: &str) -> bool {
        self.resources.iter().any(|resource| resource.uri == uri)
            || self
                .templates
                .iter()
                .any(|template| UriTemplate::parse(&template.uri_template).matches(uri))
    }

    pub fn is_empty(&self) -> bool {
        self.resources.is_empty() && self.templates.is_empty()
    }
}

/// One item of a `resources/read` result: text, or a base64 blob.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceContents {
    pub uri: String,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub blob: Option<String>,
}

impl ResourceContents {
    pub fn is_image(&self) -> bool {
        self.mime_type
            .as_deref()
            .is_some_and(|mime_type| mime_type.starts_with(IMAGE_MIME_PREFIX))
    }

    /// `[blob: application/pdf, 1024 bytes]`: what a blob is, without its
    /// payload.
    pub fn blob_label(&self) -> Option<String> {
        self.blob
            .as_deref()
            .map(|blob| media_label(BLOB_KIND, &self.mime_type, decoded_len(blob)))
    }

    /// The blob as an image a model accepts, shrunk the way an image file is.
    pub fn image(&self) -> Result<ImageSource, String> {
        let bytes = BASE64
            .decode(self.blob.as_deref().unwrap_or_default())
            .map_err(|error| error.to_string())?;
        image_bytes::prepare_bytes(&self.uri, &bytes)
            .map(|prepared| prepared.source)
            .map_err(|error| error.message)
    }
}

impl From<EmbeddedResource> for ResourceContents {
    fn from(resource: EmbeddedResource) -> Self {
        Self {
            uri: resource.uri,
            mime_type: resource.mime_type,
            text: resource.text,
            blob: resource.blob,
        }
    }
}

/// A `prompts/get` message as the conversation keeps it: text as text, an
/// embedded text resource under its URI, and an image as an image block when
/// the model can see one. Only a user message carries images, so an
/// assistant's image, like any other blob, is named by its type and size.
pub fn prompt_message(message: PromptMessage, vision: bool) -> Message {
    let content = message.content;
    let user = message.role == PromptRole::User;
    let mut images = Vec::new();
    let contents = match content.kind.as_deref() {
        _ if !content.text.is_empty() => None,
        Some(IMAGE_KIND) => Some(ResourceContents {
            mime_type: content.mime_type,
            blob: content.data,
            ..ResourceContents::default()
        }),
        Some(RESOURCE_KIND) => content.resource.map(ResourceContents::from),
        _ => None,
    };
    let text = match contents {
        None => match content.uri {
            Some(uri) if content.text.is_empty() => uri,
            _ => content.text,
        },
        Some(ResourceContents {
            uri,
            text: Some(text),
            ..
        }) => format!("{uri}\n{text}"),
        Some(resource) => {
            let label = resource.blob_label().unwrap_or_default();
            if !(user && vision && resource.is_image()) {
                label
            } else {
                match resource.image() {
                    Ok(source) => {
                        images.push(source);
                        label
                    }
                    Err(error) => format!("{label} not inlined: {error}"),
                }
            }
        }
    };
    match (user, images.is_empty()) {
        (false, _) => Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text }],
            ..Message::default()
        },
        (true, true) => Message::user(text),
        (true, false) => Message::user_with_images(text, images),
    }
}

#[derive(Deserialize)]
struct ReadResourceResult {
    #[serde(default)]
    contents: Vec<ResourceContents>,
}

/// What a completion is asked about.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CompletionRef {
    /// A prompt, by the name its server knows it by.
    Prompt(String),
    /// A resource template, by its URI template.
    Resource(String),
}

/// The values a server suggested, at most [`MAX_COMPLETION_VALUES`] of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Completion {
    #[serde(default)]
    pub values: Vec<String>,
    #[serde(default)]
    pub total: Option<u64>,
    #[serde(default)]
    pub has_more: bool,
}

#[derive(Deserialize)]
struct CompleteResult {
    #[serde(default)]
    completion: Completion,
}

pub async fn list_resources(transport: &dyn McpTransport) -> Result<Vec<Resource>, McpError> {
    list_paged(transport, RESOURCES_LIST_METHOD, RESOURCES_FIELD).await
}

/// A server may offer resources and no templates at all, and some say so by
/// not knowing the method.
pub async fn list_templates(
    transport: &dyn McpTransport,
) -> Result<Vec<ResourceTemplate>, McpError> {
    match list_paged(transport, RESOURCES_TEMPLATES_LIST_METHOD, TEMPLATES_FIELD).await {
        Err(McpError::RpcError { code, .. }) if code == METHOD_NOT_FOUND => Ok(Vec::new()),
        result => result,
    }
}

/// Both lists of a server that declared `resources`.
pub async fn list_all(transport: &dyn McpTransport) -> Result<ResourceLists, McpError> {
    Ok(ResourceLists {
        resources: list_resources(transport).await?,
        templates: list_templates(transport).await?,
    })
}

/// Reads one resource. A server that needs the user's input first asks
/// through `scope`, so the read can be answered before it completes.
pub async fn read_resource(
    transport: &dyn McpTransport,
    uri: &str,
    scope: &CallScope,
) -> Result<Vec<ResourceContents>, McpError> {
    let params = || Some(json!({ "uri": uri }));
    let result = send(transport, RESOURCES_READ_METHOD, params, scope, true).await?;
    let parsed: ReadResourceResult =
        serde_json::from_value(result).map_err(|e| invalid_response(transport.server_name(), e))?;
    Ok(parsed.contents)
}

/// The params of a `completion/complete` asking for `argument`, typed so far
/// as `value`, with the arguments already settled as its context.
pub fn completion_params(
    reference: &CompletionRef,
    argument: &str,
    value: &str,
    context: &BTreeMap<String, String>,
) -> Value {
    let reference = match reference {
        CompletionRef::Prompt(name) => json!({ "type": PROMPT_REF, "name": name }),
        CompletionRef::Resource(uri) => json!({ "type": RESOURCE_REF, "uri": uri }),
    };
    let mut params = json!({
        "ref": reference,
        "argument": { "name": argument, "value": value },
    });
    if !context.is_empty() {
        params["context"] = json!({ "arguments": context });
    }
    params
}

pub async fn complete(
    transport: &dyn McpTransport,
    reference: &CompletionRef,
    argument: &str,
    value: &str,
    context: &BTreeMap<String, String>,
) -> Result<Completion, McpError> {
    let params = || Some(completion_params(reference, argument, value, context));
    let result = send(
        transport,
        COMPLETION_COMPLETE_METHOD,
        params,
        &CallScope::none(),
        true,
    )
    .await?;
    let mut parsed: CompleteResult =
        serde_json::from_value(result).map_err(|e| invalid_response(transport.server_name(), e))?;
    parsed.completion.values.truncate(MAX_COMPLETION_VALUES);
    Ok(parsed.completion)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part<'a> {
    Literal(&'a str),
    Variable(&'a str),
}

/// An RFC 6570 level 1 URI template: literal text and `{name}` variables. A
/// variable stands for any non-empty run of characters, so a template matches
/// the URIs its server would build from it whatever they hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UriTemplate<'a> {
    parts: Vec<Part<'a>>,
}

/// How far typed text has got through a template: the variables it settled,
/// and the one it is in the middle of, if any.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TemplateProgress {
    pub settled: BTreeMap<String, String>,
    /// The variable being typed and what it holds so far.
    pub current: Option<(String, String)>,
    /// The typed text that comes before `current`'s value.
    pub prefix: String,
    /// The literal that follows `current`, up to the next variable.
    pub follows: String,
    /// Whether another variable comes after `current`.
    pub more: bool,
}

impl<'a> UriTemplate<'a> {
    /// A template with an unclosed or empty variable matches nothing.
    pub fn parse(template: &'a str) -> Self {
        let mut parts = Vec::new();
        let mut rest = template;
        while !rest.is_empty() {
            let Some(open) = rest.find(VARIABLE_OPEN) else {
                parts.push(Part::Literal(rest));
                break;
            };
            if open > 0 {
                parts.push(Part::Literal(&rest[..open]));
            }
            let after = &rest[open + VARIABLE_OPEN.len_utf8()..];
            match after.find(VARIABLE_CLOSE) {
                Some(close) if close > 0 => {
                    parts.push(Part::Variable(&after[..close]));
                    rest = &after[close + VARIABLE_CLOSE.len_utf8()..];
                }
                _ => return Self { parts: Vec::new() },
            }
        }
        Self { parts }
    }

    pub fn variables(&self) -> impl Iterator<Item = &'a str> + '_ {
        self.parts.iter().filter_map(|part| match part {
            Part::Variable(name) => Some(*name),
            Part::Literal(_) => None,
        })
    }

    /// The text before the first variable, which every URI it names opens with.
    pub fn stem(&self) -> String {
        self.parts
            .iter()
            .map_while(|part| match part {
                Part::Literal(text) => Some(*text),
                Part::Variable(_) => None,
            })
            .collect()
    }

    pub fn matches(&self, uri: &str) -> bool {
        !self.parts.is_empty() && match_parts(&self.parts, uri)
    }

    /// Where `typed`, a prefix of some URI the template names, has got to.
    /// `None` once `typed` has left the template.
    pub fn progress(&self, typed: &str) -> Option<TemplateProgress> {
        let mut progress = TemplateProgress::default();
        let mut rest = typed;
        for (index, part) in self.parts.iter().enumerate() {
            match part {
                Part::Literal(text) => match rest.strip_prefix(text) {
                    Some(tail) => {
                        progress.prefix.push_str(text);
                        rest = tail;
                    }
                    None if text.starts_with(rest) => {
                        progress.prefix.push_str(rest);
                        return Some(progress);
                    }
                    None => return None,
                },
                Part::Variable(name) => {
                    let follows = match self.parts.get(index + 1) {
                        Some(Part::Literal(text)) => *text,
                        _ => "",
                    };
                    let settled_at = (!follows.is_empty())
                        .then(|| rest.find(follows))
                        .flatten()
                        .filter(|&end| end > 0);
                    match settled_at {
                        Some(end) => {
                            progress
                                .settled
                                .insert((*name).to_owned(), rest[..end].to_owned());
                            progress.prefix.push_str(&rest[..end]);
                            rest = &rest[end..];
                        }
                        None => {
                            progress.current = Some(((*name).to_owned(), rest.to_owned()));
                            progress.follows = follows.to_owned();
                            progress.more = self.parts[index + 1..]
                                .iter()
                                .any(|part| matches!(part, Part::Variable(_)));
                            return Some(progress);
                        }
                    }
                }
            }
        }
        rest.is_empty().then_some(progress)
    }
}

/// Shortest values first, so `{a}/{b}` splits at the first slash.
fn match_parts(parts: &[Part<'_>], uri: &str) -> bool {
    match parts.split_first() {
        None => uri.is_empty(),
        Some((Part::Literal(text), rest)) => uri
            .strip_prefix(text)
            .is_some_and(|tail| match_parts(rest, tail)),
        Some((Part::Variable(_), rest)) => uri
            .char_indices()
            .skip(1)
            .map(|(end, _)| end)
            .chain((!uri.is_empty()).then_some(uri.len()))
            .any(|end| match_parts(rest, &uri[end..])),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use test_case::test_case;

    use super::*;
    use crate::mcp::config::LocalExecutionPolicy;
    use crate::mcp::stdio::StdioTransport;
    use crate::mcp::transport::BoxFuture;

    const SERVER: &str = "notes";
    const PROMPT: &str = "review";
    const TEMPLATE: &str = "repo://{owner}/{name}/issues";
    const ARGUMENT: &str = "name";
    const TYPED: &str = "cau";
    const TEXT: &str = "alpha";
    const STDIO_TIMEOUT: Duration = Duration::from_secs(5);
    /// Lists `mem://a` then, behind cursor `p2`, `mem://b`; reads `mem://a`.
    const RESOURCE_SERVER: &str = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9]*\),.*/\1/p')
  test -z "$id" && continue
  case "$line" in
    *'"cursor":"p2"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"resources":[{"uri":"mem://b","name":"b"}]}}\n' "$id" ;;
    *'"method":"resources/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"resources":[{"uri":"mem://a","name":"a","title":"Alpha"}],"nextCursor":"p2"}}\n' "$id" ;;
    *'"method":"resources/templates/list"'*) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"no templates"}}\n' "$id" ;;
    *'"method":"resources/read"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"contents":[{"uri":"mem://a","mimeType":"text/plain","text":"alpha"},{"uri":"mem://a.png","mimeType":"image/png","blob":"iVBORw0K"}]}}\n' "$id" ;;
  esac
done"#;

    /// Answers every request with `reply`, and keeps each method and params.
    struct Recording {
        name: Arc<str>,
        reply: Value,
        sent: Mutex<Vec<(String, Value)>>,
    }

    impl Recording {
        fn new(reply: Value) -> Self {
            Self {
                name: Arc::from(SERVER),
                reply,
                sent: Mutex::default(),
            }
        }
    }

    impl McpTransport for Recording {
        fn send_request<'a>(
            &'a self,
            method: &'a str,
            params: Option<Value>,
            _scope: &'a CallScope,
        ) -> BoxFuture<'a, Result<Value, McpError>> {
            self.sent
                .lock()
                .unwrap()
                .push((method.to_owned(), params.unwrap_or_default()));
            let reply = self.reply.clone();
            Box::pin(async move { Ok(reply) })
        }
        fn send_notification<'a>(
            &'a self,
            _method: &'a str,
            _params: Option<Value>,
        ) -> BoxFuture<'a, Result<(), McpError>> {
            Box::pin(async { Ok(()) })
        }
        fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
        fn server_name(&self) -> &Arc<str> {
            &self.name
        }
        fn transport_kind(&self) -> &'static str {
            SERVER
        }
    }

    fn resource_server() -> StdioTransport {
        StdioTransport::spawn(
            SERVER,
            "/bin/sh",
            &["-c".into(), RESOURCE_SERVER.into()],
            &HashMap::new(),
            STDIO_TIMEOUT,
            LocalExecutionPolicy::Embedded,
            true,
        )
        .unwrap()
    }

    #[test]
    fn a_stdio_server_is_listed_page_by_page_and_read_whole() {
        let transport = resource_server();
        smol::block_on(async {
            let lists = list_all(&transport).await.unwrap();
            let uris: Vec<&str> = lists.resources.iter().map(|r| r.uri.as_str()).collect();
            assert_eq!(uris, ["mem://a", "mem://b"]);
            assert_eq!(lists.resources[0].label(), "Alpha");
            assert!(lists.templates.is_empty(), "an unknown method lists none");

            let contents = read_resource(&transport, "mem://a", &CallScope::none())
                .await
                .unwrap();
            assert_eq!(contents[0].text.as_deref(), Some(TEXT));
            assert_eq!(contents[0].mime_type.as_deref(), Some("text/plain"));
            assert_eq!(contents[1].blob.as_deref(), Some("iVBORw0K"));
            assert_eq!(contents[1].mime_type.as_deref(), Some("image/png"));
            transport.shutdown().await;
        });
    }

    #[test_case(CompletionRef::Prompt(PROMPT.into()), &[], json!({"ref": {"type": "ref/prompt", "name": PROMPT}, "argument": {"name": ARGUMENT, "value": TYPED}}) ; "prompt_without_context")]
    #[test_case(CompletionRef::Resource(TEMPLATE.into()), &[("owner", "caudra")], json!({"ref": {"type": "ref/resource", "uri": TEMPLATE}, "argument": {"name": ARGUMENT, "value": TYPED}, "context": {"arguments": {"owner": "caudra"}}}) ; "template_with_settled_variables")]
    fn a_completion_request_names_its_reference_argument_and_context(
        reference: CompletionRef,
        context: &[(&str, &str)],
        expected: Value,
    ) {
        let transport = Recording::new(json!({"completion": {"values": []}}));
        let context = context
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        smol::block_on(complete(&transport, &reference, ARGUMENT, TYPED, &context)).unwrap();
        let sent = transport.sent.lock().unwrap();
        assert_eq!(sent[0].0, COMPLETION_COMPLETE_METHOD);
        assert_eq!(sent[0].1, expected);
    }

    #[test]
    fn a_completion_keeps_at_most_the_values_the_specification_allows() {
        let values: Vec<String> = (0..MAX_COMPLETION_VALUES + 5)
            .map(|n| n.to_string())
            .collect();
        let transport = Recording::new(
            json!({"completion": {"values": values, "total": 105, "hasMore": true}}),
        );
        let completion = smol::block_on(complete(
            &transport,
            &CompletionRef::Prompt(PROMPT.into()),
            ARGUMENT,
            TYPED,
            &BTreeMap::new(),
        ))
        .unwrap();
        assert_eq!(completion.values.len(), MAX_COMPLETION_VALUES);
        assert!(completion.has_more);
    }

    #[test_case(TEMPLATE, "repo://caudra/caudra/issues", true ; "both_variables")]
    #[test_case(TEMPLATE, "repo://caudra/issues", false ; "a_missing_variable")]
    #[test_case(TEMPLATE, "repo:///caudra/issues", false ; "an_empty_variable")]
    #[test_case(TEMPLATE, "repo://a/b/issues/extra", false ; "trailing_text")]
    #[test_case("file:///{path}", "file:///home/me/notes.md", true ; "a_variable_holding_slashes")]
    #[test_case("mem://fixed", "mem://fixed", true ; "no_variables")]
    #[test_case("mem://{open", "mem://open", false ; "an_unclosed_variable")]
    fn a_template_matches_the_uris_it_names(template: &str, uri: &str, expected: bool) {
        assert_eq!(UriTemplate::parse(template).matches(uri), expected);
    }

    #[test]
    fn a_template_lists_its_variables_and_its_stem() {
        let template = UriTemplate::parse(TEMPLATE);
        assert_eq!(template.variables().collect::<Vec<_>>(), ["owner", "name"]);
        assert_eq!(template.stem(), "repo://");
    }

    #[test_case("repo:/", None, &[], "repo:/" ; "inside_the_stem")]
    #[test_case("repo://ca", Some(("owner", "ca")), &[], "repo://" ; "first_variable")]
    #[test_case("repo://caudra/ca", Some(("name", "ca")), &[("owner", "caudra")], "repo://caudra/" ; "second_variable")]
    fn typed_text_reports_how_far_it_got_through_a_template(
        typed: &str,
        current: Option<(&str, &str)>,
        settled: &[(&str, &str)],
        prefix: &str,
    ) {
        let progress = UriTemplate::parse(TEMPLATE).progress(typed).unwrap();
        assert_eq!(
            progress
                .current
                .as_ref()
                .map(|(name, value)| (name.as_str(), value.as_str())),
            current
        );
        let settled: BTreeMap<String, String> = settled
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        assert_eq!(progress.settled, settled);
        assert_eq!(progress.prefix, prefix);
    }

    #[test]
    fn text_that_left_the_template_has_no_progress() {
        assert!(UriTemplate::parse(TEMPLATE).progress("file://x").is_none());
    }

    #[test]
    fn the_first_variable_says_what_follows_it() {
        let progress = UriTemplate::parse(TEMPLATE).progress("repo://ca").unwrap();
        assert_eq!(progress.follows, "/");
        assert!(progress.more);
    }

    #[test]
    fn resource_lists_offer_listed_and_templated_uris() {
        let lists = ResourceLists {
            resources: vec![Resource {
                uri: "mem://a".into(),
                ..Resource::default()
            }],
            templates: vec![ResourceTemplate {
                uri_template: TEMPLATE.into(),
                ..ResourceTemplate::default()
            }],
        };
        assert!(lists.offers("mem://a"));
        assert!(lists.offers("repo://a/b/issues"));
        assert!(!lists.offers("mem://b"));
    }

    const PNG_MIME: &str = "image/png";
    const PDF_MIME: &str = "application/pdf";
    const DOC_URI: &str = "mem://doc";
    const PNG_SIDE: u32 = 2;

    fn png() -> Vec<u8> {
        image_bytes::test_support::png(PNG_SIDE, PNG_SIDE)
    }

    fn prompt(role: &str, content: Value) -> PromptMessage {
        serde_json::from_value(json!({ "role": role, "content": content })).unwrap()
    }

    fn images(message: &Message) -> usize {
        message
            .content
            .iter()
            .filter(|block| matches!(block, ContentBlock::Image { .. }))
            .count()
    }

    fn text(message: &Message) -> String {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// A user's image reaches a model that can see it as an image block, and
    /// is named in its place everywhere else.
    #[test_case("user", true => 1 ; "user_with_vision")]
    #[test_case("user", false => 0 ; "user_without_vision")]
    #[test_case("assistant", true => 0 ; "assistant")]
    fn a_prompt_image_becomes_an_image_block(role: &str, vision: bool) -> usize {
        let png = png();
        let message = prompt_message(
            prompt(
                role,
                json!({"type": "image", "mimeType": PNG_MIME, "data": BASE64.encode(&png)}),
            ),
            vision,
        );
        assert_eq!(
            text(&message),
            format!("[{BLOB_KIND}: {PNG_MIME}, {} bytes]", png.len())
        );
        images(&message)
    }

    #[test_case(json!({"type": "text", "text": TEXT}), TEXT ; "text")]
    #[test_case(json!({"type": "resource", "resource": {"uri": DOC_URI, "text": TEXT}}), "mem://doc\nalpha" ; "an_embedded_text_resource")]
    #[test_case(json!({"type": "resource", "resource": {"uri": DOC_URI, "mimeType": PDF_MIME, "blob": "AAAA"}}), "[blob: application/pdf, 3 bytes]" ; "an_embedded_blob")]
    #[test_case(json!({"type": "resource_link", "uri": DOC_URI}), DOC_URI ; "a_resource_link")]
    fn prompt_content_keeps_what_it_carries(content: Value, expected: &str) {
        let message = prompt_message(prompt("user", content), true);
        assert_eq!(text(&message), expected);
    }
}
