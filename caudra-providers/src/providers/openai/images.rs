//! Image generation over the ChatGPT subscription (Codex) backend.
//!
//! This is not a public OpenAI API. It attaches the hosted `image_generation`
//! tool to a single-turn Responses request on the same endpoint the coding plan
//! already uses for chat, so generations bill against the user's ChatGPT plan
//! rather than API credits. The endpoint is undocumented and may change or be
//! restricted without notice.
//!
//! Deliberately separate from `responses.rs`: that module converts agent
//! conversations and only ever emits `{"type": "function"}` tools. This request
//! carries no history and forces a hosted tool, so it owns its own body.

use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use caudra_config::ImageModel;
use caudra_storage::StateDir;
use caudra_storage::auth::try_load_tokens;
use futures_lite::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use isahc::Request;
use serde_json::{Value, json};
use tracing::debug;

use super::auth::{
    CODING_PLAN_BASE_URL, PROVIDER, build_coding_plan_resolved, refresh_from_storage,
};
use crate::AgentError;
use crate::providers::{
    ResolvedAuth, SseErrorDetail, SseErrorPayload, Timeouts, http_client, next_sse_line, user_agent,
};
use crate::types::ImageSource;

const RESPONSES_PATH: &str = "/responses";
/// The subscription backend routes image generation through the chat model
/// rather than exposing an image endpoint of its own. This is the mainline
/// model that drives the turn; the image model rides on the hosted tool.
const MAINLINE_MODEL: &str = "gpt-5.5";
const OUTPUT_FORMAT: &str = "png";
const OUTPUT_ITEM_DONE: &str = "response.output_item.done";
const IMAGE_CALL_ITEM: &str = "image_generation_call";
const DONE_SENTINEL: &str = "[DONE]";
const INSTRUCTIONS: &str = "You are an image generation assistant. Always satisfy the request by \
     invoking the image_generation tool exactly once. Do not respond with text only.";
const NOT_AUTHENTICATED: &str = "image generation requires a ChatGPT subscription — \
     run `caudra auth login openai` and choose the subscription option";
const NO_RESULT: &str = "the ChatGPT backend returned no image; the prompt may have been refused";
const DECODE_FAILED: &str = "the ChatGPT backend returned an undecodable image";
/// Upstream errors that carry no type map to 400 by default; a malformed
/// success payload is the backend's fault, not the caller's.
const BAD_GATEWAY: u16 = 502;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageQuality {
    Low,
    Medium,
    High,
    XHigh,
    Max,
    #[default]
    Auto,
}

impl ImageQuality {
    pub const ALL: [Self; 6] = [
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
        Self::Auto,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Auto => "auto",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|q| q.as_str() == value)
    }
}

pub struct ImageRequest<'a> {
    pub prompt: &'a str,
    pub model: ImageModel,
    pub quality: ImageQuality,
    pub size: Option<&'a str>,
    pub references: &'a [ImageSource],
}

/// Generate one PNG. Returns the decoded image bytes.
pub async fn generate(dir: &StateDir, req: &ImageRequest<'_>) -> Result<Vec<u8>, AgentError> {
    let storage = dir.clone();
    let auth = smol::unblock(move || resolve_auth(&storage)).await?;
    let timeouts = Timeouts::default();
    let client = http_client(timeouts);
    let body = serde_json::to_vec(&build_body(req))?;

    let request = auth
        .configure_request(
            Request::builder()
                .method("POST")
                .uri(format!("{CODING_PLAN_BASE_URL}{RESPONSES_PATH}"))
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .header("originator", "caudra")
                .header("user-agent", user_agent()),
        )
        .body(body)?;

    debug!(
        model = MAINLINE_MODEL,
        image_model = req.model.model_id(),
        references = req.references.len(),
        quality = req.quality.as_str(),
        size = req.size.unwrap_or("auto"),
        "requesting image generation"
    );

    let response = client.send_async(request).await?;
    if response.status().as_u16() != 200 {
        return Err(AgentError::from_response(response).await);
    }
    parse_sse(BufReader::new(response.into_body()), timeouts.stream).await
}

/// Unlike `openai_auth::resolve`, this refreshes rather than handing back an
/// expired access token: a one-shot request has no retry loop behind it.
fn resolve_auth(dir: &StateDir) -> Result<ResolvedAuth, AgentError> {
    let tokens = try_load_tokens(dir, PROVIDER)?.ok_or_else(|| AgentError::Config {
        message: NOT_AUTHENTICATED.into(),
    })?;
    let tokens = if tokens.is_expired() {
        refresh_from_storage(dir, &[])?
    } else {
        tokens
    };
    Ok(build_coding_plan_resolved(&tokens))
}

fn build_body(req: &ImageRequest<'_>) -> Value {
    let mut content = vec![json!({"type": "input_text", "text": req.prompt})];
    content.extend(
        req.references
            .iter()
            .map(|image| json!({"type": "input_image", "image_url": image.to_data_url()})),
    );

    let mut tool = json!({
        "type": "image_generation",
        "model": req.model.model_id(),
        "output_format": OUTPUT_FORMAT,
        "quality": req.quality.as_str(),
    });
    if let Some(size) = req.size {
        tool["size"] = json!(size);
    }

    json!({
        "model": MAINLINE_MODEL,
        "instructions": INSTRUCTIONS,
        "input": [{"role": "user", "content": content}],
        "tools": [tool],
        "tool_choice": {"type": "image_generation"},
        "stream": true,
        "store": false,
    })
}

async fn parse_sse(
    reader: impl AsyncBufRead + Unpin,
    stream_timeout: Duration,
) -> Result<Vec<u8>, AgentError> {
    let mut lines = reader.lines();
    let mut deadline = Instant::now() + stream_timeout;

    while let Some(line) = next_sse_line(&mut lines, &mut deadline, stream_timeout).await? {
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data.is_empty() || data == DONE_SENTINEL {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if let Some(error) = sse_error(&event) {
            return Err(error);
        }
        if let Some(result) = image_result(&event) {
            return BASE64
                .decode(result)
                .map_err(|e| AgentError::api(BAD_GATEWAY, format!("{DECODE_FAILED}: {e}")));
        }
    }

    Err(AgentError::api(BAD_GATEWAY, NO_RESULT))
}

fn image_result(event: &Value) -> Option<&str> {
    if event.get("type")?.as_str()? != OUTPUT_ITEM_DONE {
        return None;
    }
    let item = event.get("item")?;
    if item.get("type")?.as_str()? != IMAGE_CALL_ITEM {
        return None;
    }
    item.get("result")?.as_str().filter(|r| !r.is_empty())
}

/// Failures arrive either as a bare `error` event or nested under a
/// `response.failed` payload.
fn sse_error(event: &Value) -> Option<AgentError> {
    let error = event
        .get("error")
        .or_else(|| event.get("response")?.get("error"))
        .filter(|error| !error.is_null())?;
    let detail: SseErrorDetail = serde_json::from_value(error.clone()).ok()?;
    Some(SseErrorPayload { error: detail }.into_agent_error())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures_lite::io::Cursor;
    use test_case::test_case;

    use super::*;
    use crate::types::ImageMediaType;

    const PIXEL: &str = "iVBORw0KGgo=";
    const STREAM_TIMEOUT: Duration = Duration::from_secs(5);
    const PROMPT: &str = "a red square";

    fn request<'a>(references: &'a [ImageSource], size: Option<&'a str>) -> ImageRequest<'a> {
        ImageRequest {
            prompt: PROMPT,
            model: ImageModel::Sunburst,
            quality: ImageQuality::High,
            size,
            references,
        }
    }

    fn reference() -> ImageSource {
        ImageSource::new(ImageMediaType::Png, Arc::from(PIXEL))
    }

    fn run(sse: &str) -> Result<Vec<u8>, AgentError> {
        smol::block_on(parse_sse(
            futures_lite::io::BufReader::new(Cursor::new(sse.to_owned().into_bytes())),
            STREAM_TIMEOUT,
        ))
    }

    #[test]
    fn image_result_event_yields_decoded_png_bytes() {
        let sse = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"type": "response.created"}),
            json!({
                "type": OUTPUT_ITEM_DONE,
                "item": {"type": IMAGE_CALL_ITEM, "result": PIXEL},
            })
        );

        assert_eq!(run(&sse).unwrap(), BASE64.decode(PIXEL).unwrap());
    }

    #[test]
    fn stream_without_an_image_item_reports_no_result() {
        let sse = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({
                "type": OUTPUT_ITEM_DONE,
                "item": {"type": "message", "result": PIXEL},
            })
        );

        let error = run(&sse).unwrap_err();
        assert!(error.to_string().contains(NO_RESULT), "{error}");
    }

    #[test]
    fn empty_image_result_is_not_accepted_as_an_image() {
        let sse = format!(
            "data: {}\n\n",
            json!({
                "type": OUTPUT_ITEM_DONE,
                "item": {"type": IMAGE_CALL_ITEM, "result": ""},
            })
        );

        let error = run(&sse).unwrap_err();
        assert!(error.to_string().contains(NO_RESULT), "{error}");
    }

    #[test]
    fn undecodable_result_reports_a_decode_failure() {
        let sse = format!(
            "data: {}\n\n",
            json!({
                "type": OUTPUT_ITEM_DONE,
                "item": {"type": IMAGE_CALL_ITEM, "result": "not!base64"},
            })
        );

        let error = run(&sse).unwrap_err();
        assert!(error.to_string().contains(DECODE_FAILED), "{error}");
    }

    const MODERATION_MESSAGE: &str = "request rejected by safety system";

    #[test_case(json!({"type": "error", "error": {"type": "invalid_request_error", "message": MODERATION_MESSAGE}}) ; "bare_error_event")]
    #[test_case(json!({"type": "response.failed", "response": {"error": {"message": MODERATION_MESSAGE}}}) ; "nested_response_failed")]
    fn backend_errors_surface_before_the_stream_ends(event: Value) {
        let error = run(&format!("data: {event}\n\n")).unwrap_err();
        assert!(error.to_string().contains(MODERATION_MESSAGE), "{error}");
    }

    #[test]
    fn body_forces_the_hosted_image_tool_and_carries_no_history() {
        let body = build_body(&request(&[], None));

        assert_eq!(body["model"], MAINLINE_MODEL);
        assert_eq!(body["tool_choice"], json!({"type": "image_generation"}));
        assert_eq!(body["tools"][0]["type"], "image_generation");
        assert_eq!(body["tools"][0]["model"], ImageModel::Sunburst.model_id());
        assert_eq!(body["tools"][0]["output_format"], OUTPUT_FORMAT);
        assert_eq!(body["tools"][0]["quality"], "high");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        assert_eq!(body["input"][0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(body["input"][0]["content"][0]["text"], PROMPT);
    }

    /// The image model rides on the hosted tool, not the top level: the
    /// mainline model must stay put whichever image model is configured.
    #[test_case(ImageModel::Sunburst ; "sunburst")]
    #[test_case(ImageModel::Flare ; "flare")]
    fn the_configured_image_model_reaches_the_tool_and_leaves_the_mainline_alone(model: ImageModel) {
        let mut req = request(&[], None);
        req.model = model;
        let body = build_body(&req);

        assert_eq!(body["tools"][0]["model"], model.model_id());
        assert_eq!(body["model"], MAINLINE_MODEL);
    }

    #[test]
    fn size_is_omitted_rather_than_sent_as_null_when_unset() {
        assert!(
            build_body(&request(&[], None))["tools"][0]
                .get("size")
                .is_none()
        );
        assert_eq!(
            build_body(&request(&[], Some("1024x1536")))["tools"][0]["size"],
            "1024x1536"
        );
    }

    #[test]
    fn reference_images_follow_the_prompt_as_data_urls() {
        let references = [reference(), reference()];
        let body = build_body(&request(&references, None));
        let content = body["input"][0]["content"].as_array().unwrap();

        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "input_text");
        for image in &content[1..] {
            assert_eq!(image["type"], "input_image");
            assert_eq!(image["image_url"], format!("data:image/png;base64,{PIXEL}"));
        }
    }

    #[test_case(ImageQuality::Low, "low")]
    #[test_case(ImageQuality::Medium, "medium")]
    #[test_case(ImageQuality::High, "high")]
    #[test_case(ImageQuality::XHigh, "xhigh")]
    #[test_case(ImageQuality::Max, "max")]
    #[test_case(ImageQuality::Auto, "auto")]
    fn quality_round_trips_through_its_wire_string(quality: ImageQuality, wire: &str) {
        assert_eq!(quality.as_str(), wire);
        assert_eq!(ImageQuality::from_wire(wire), Some(quality));
    }

    #[test]
    fn unknown_quality_is_rejected() {
        assert_eq!(ImageQuality::from_wire("ultra"), None);
    }
}
