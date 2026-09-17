use std::sync::Arc;
use std::time::Duration;

use caudra_config::ModelPolicy;
use caudra_providers::provider::Provider;
use caudra_providers::{
    AgentError, ContentBlock, MIN_THINKING_BUDGET, Message, Model, ModelPurpose, RequestOptions,
    Timeouts, TokenUsage,
};
use caudra_storage::sessions::{normalize_title, truncate_title};
use serde_json::json;
use tracing::warn;

use super::side_model::{self, SideModel};
use super::streaming::{StreamError, stream_silent_with_retry};
use crate::cancel::CancelToken;

/// A title nobody is waiting for is worth one short attempt, not a long one.
const TITLE_TIMEOUT: Duration = Duration::from_secs(20);
/// One line is all the answer needs, but a model that reasons unconditionally
/// draws its thinking budget from this same pool, and providers floor that
/// budget at [`MIN_THINKING_BUDGET`]. Since the budget derives from half the
/// output window, anything tighter than twice the floor produces a budget the
/// provider rejects outright.
const TITLE_OUTPUT_TOKENS: u32 = MIN_THINKING_BUDGET * 2;
/// A pasted file makes a poor title and an expensive request; the opening of
/// the prompt is what the title is about.
const MAX_PROMPT_BYTES: usize = 4_096;
const PROMPT_PREFIX: &str = "Generate a title for this conversation:\n";
const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

/// Resolves the title model and asks it for a name. The one entry point:
/// the agent names a new session with it, and the session picker renames an
/// existing one.
pub async fn for_prompt(
    provider: &Arc<dyn Provider>,
    model: &Model,
    timeouts: Timeouts,
    model_policy: &ModelPolicy,
    prompt: &str,
    cancel: &CancelToken,
) -> Result<(SideModel, TitleOutcome), AgentError> {
    let resolved = side_model::resolve(
        ModelPurpose::Title,
        provider,
        model,
        timeouts,
        model_policy,
        TITLE_OUTPUT_TOKENS,
    )
    .await;
    let outcome = generate(&*resolved.provider, &resolved.model, prompt, cancel).await?;
    Ok((resolved, outcome))
}

/// What a title attempt cost and what it produced. `title` is `None` when the
/// model answered with nothing usable, so the heuristic title the session
/// already carries stands. The spend is reported either way: an unusable
/// answer was still billed.
pub struct TitleOutcome {
    pub title: Option<String>,
    pub usage: TokenUsage,
}

/// Errors only when the request itself failed or stalled, which is the one
/// case with no spend to attribute.
///
/// No cache key: the title has its own system prompt, so it shares no prefix
/// with the session and must not claim the session's cache slot.
async fn generate(
    provider: &dyn Provider,
    model: &Model,
    prompt: &str,
    cancel: &CancelToken,
) -> Result<TitleOutcome, AgentError> {
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
        None,
        cancel,
        RequestOptions::default(),
        None,
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
        Ok(response) => Ok(TitleOutcome {
            title: clean(&response_text(&response.message)),
            usage: response.usage,
        }),
        Err(error) => {
            warn!(%error, model = %model.id, "session title generation failed");
            Err(error)
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
