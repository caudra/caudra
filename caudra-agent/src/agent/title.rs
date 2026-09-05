use std::sync::Arc;
use std::time::Duration;

use caudra_config::ModelPolicy;
use caudra_providers::model_registry::TitleTarget;
use caudra_providers::provider::{Provider, from_model_async};
use caudra_providers::{
    AgentError, ContentBlock, Message, Model, ModelError, ModelTier, RequestOptions, Timeouts,
};
use caudra_storage::id::SessionRef;
use caudra_storage::sessions::{normalize_title, truncate_title};
use serde_json::json;
use tracing::{debug, warn};

use super::streaming::{StreamError, stream_silent_with_retry};
use crate::cancel::CancelToken;

/// A title nobody is waiting for is worth one short attempt, not a long one.
const TITLE_TIMEOUT: Duration = Duration::from_secs(20);
/// One line, so anything past this is the model ignoring its instructions.
const TITLE_OUTPUT_TOKENS: u32 = 512;
/// A pasted file makes a poor title and an expensive request; the opening of
/// the prompt is what the title is about.
const MAX_PROMPT_BYTES: usize = 4_096;
const PROMPT_PREFIX: &str = "Generate a title for this conversation:\n";
const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

pub(crate) struct ResolvedTitleModel {
    pub provider: Arc<dyn Provider>,
    pub model: Model,
}

/// Falls back to the chat model whenever the target cannot be resolved: a
/// missing weak tier is a reason to use what is already loaded, not to skip
/// naming the session.
pub(crate) async fn resolve(
    current_provider: &Arc<dyn Provider>,
    current_model: &Model,
    timeouts: Timeouts,
    model_policy: &ModelPolicy,
) -> ResolvedTitleModel {
    let target_model = current_model.clone();
    let policy = model_policy.clone();
    // Catalog lookups and `warm_catalog` block, so they stay off the executor
    // that is currently carrying the turn this title belongs to.
    let resolved = smol::unblock(move || title_model(&target_model, &policy)).await;
    let mut model = match resolved {
        Ok(model) => model,
        Err(error) => {
            debug!(%error, "falling back to the chat model for the session title");
            current_model.clone()
        }
    };
    model.max_output_tokens = Some(
        model
            .max_output_tokens
            .unwrap_or(TITLE_OUTPUT_TOKENS)
            .min(TITLE_OUTPUT_TOKENS),
    );

    let provider = if model.provider == current_model.provider {
        current_provider.adjust_model(&mut model);
        Arc::clone(current_provider)
    } else {
        match from_model_async(&mut model, timeouts).await {
            Ok(provider) => Arc::from(provider),
            Err(error) => {
                warn!(%error, model = %model.id, "no provider for the title model, using the chat model");
                let mut model = current_model.clone();
                current_provider.adjust_model(&mut model);
                return ResolvedTitleModel {
                    provider: Arc::clone(current_provider),
                    model,
                };
            }
        }
    };
    ResolvedTitleModel { provider, model }
}

fn title_model(current_model: &Model, model_policy: &ModelPolicy) -> Result<Model, AgentError> {
    match caudra_providers::model_registry::title_target() {
        TitleTarget::Auto => {
            Model::from_tier_with_policy(&current_model.provider, ModelTier::Weak, model_policy)
                .map_err(|error| title_model_error("weak tier", error))
        }
        TitleTarget::Model(spec) => {
            if !model_policy.allows(&spec) {
                return Err(title_model_error(
                    &spec,
                    ModelError::NotAllowed(spec.clone()),
                ));
            }
            match Model::from_spec(&spec) {
                Err(ModelError::UnsupportedProvider(_)) => {
                    caudra_providers::warm_catalog();
                    Model::from_spec(&spec).map_err(|error| title_model_error(&spec, error))
                }
                result => result.map_err(|error| title_model_error(&spec, error)),
            }
        }
    }
}

fn title_model_error(spec: &str, error: ModelError) -> AgentError {
    AgentError::Config {
        message: format!("cannot resolve title model '{spec}': {error}"),
    }
}

/// `None` whenever the model fails, stalls, or answers with nothing usable, so
/// the heuristic title the session already carries stands.
pub(crate) async fn generate(
    provider: &dyn Provider,
    model: &Model,
    prompt: &str,
    cancel: &CancelToken,
    session_id: Option<&SessionRef>,
) -> Option<String> {
    let messages = [Message::user(format!(
        "{PROMPT_PREFIX}{}",
        clamp(prompt, MAX_PROMPT_BYTES)
    ))];
    let tools = json!([]);
    let request = stream_silent_with_retry(
        provider,
        model,
        &messages,
        crate::prompt::TITLE_SYSTEM,
        &tools,
        cancel,
        RequestOptions::default(),
        session_id,
    );
    let response = futures_lite::future::race(request, async {
        smol::Timer::after(TITLE_TIMEOUT).await;
        Err(StreamError::Other(AgentError::Timeout {
            secs: TITLE_TIMEOUT.as_secs(),
        }))
    })
    .await
    .map_err(AgentError::from);

    match response {
        Ok(response) => clean(&response_text(&response.message)),
        Err(error) => {
            warn!(%error, model = %model.id, "session title generation failed");
            None
        }
    }
}

fn response_text(message: &Message) -> String {
    let mut text = String::new();
    for block in &message.content {
        if let ContentBlock::Text { text: chunk } = block {
            text.push_str(chunk);
        }
    }
    text
}

fn clean(raw: &str) -> Option<String> {
    let stripped = strip_thinking(raw);
    let line = stripped
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let title = truncate_title(&normalize_title(line));
    (!title.is_empty()).then_some(title)
}

/// Reasoning models emit the block inline when the API does not carry it
/// separately. An unterminated block means the answer never arrived, so
/// everything after it goes too.
fn strip_thinking(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(open) = rest.find(THINK_OPEN) {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + THINK_OPEN.len()..];
        let Some(close) = after_open.find(THINK_CLOSE) else {
            return out;
        };
        rest = &after_open[close + THINK_CLOSE.len()..];
    }
    out.push_str(rest);
    out
}

fn clamp(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    &text[..text.floor_char_boundary(max_bytes)]
}

#[cfg(test)]
mod tests {
    use super::{MAX_PROMPT_BYTES, clamp, clean};
    use test_case::test_case;

    const LONG_TITLE: &str = "Refactoring the authentication middleware so that refresh tokens rotate on every request and stale sessions expire";
    const LONG_TITLE_TRUNCATED: &str = "Refactoring the authentication middleware so that refresh tokens rotate on every request and stale…";

    #[test_case("Auth refresh token support", Some("Auth refresh token support") ; "plain_line")]
    #[test_case("  Auth refresh token support  ", Some("Auth refresh token support") ; "trims_padding")]
    #[test_case("<think>weighing options</think>\nAuth refresh token support", Some("Auth refresh token support") ; "strips_thinking_block")]
    #[test_case("<think>never finished the thought", None ; "unterminated_thinking_yields_nothing")]
    #[test_case("\n\nAuth refresh token support\nSecond line ignored", Some("Auth refresh token support") ; "first_non_empty_line_wins")]
    #[test_case("Auth refresh\ntoken support", Some("Auth refresh") ; "never_spans_lines")]
    #[test_case("   \n\t\n", None ; "blank_output_yields_nothing")]
    #[test_case("", None ; "empty_output_yields_nothing")]
    #[test_case(LONG_TITLE, Some(LONG_TITLE_TRUNCATED) ; "truncates_at_word_boundary")]
    fn clean_extracts_a_single_title_line(raw: &str, expected: Option<&str>) {
        assert_eq!(clean(raw).as_deref(), expected);
    }

    #[test]
    fn clamp_cuts_an_oversized_prompt_on_a_char_boundary() {
        let prompt = "é".repeat(MAX_PROMPT_BYTES);

        let clamped = clamp(&prompt, MAX_PROMPT_BYTES);

        assert!(clamped.len() <= MAX_PROMPT_BYTES);
        assert!(prompt.starts_with(clamped));
    }
}
