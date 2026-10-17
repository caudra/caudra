//! Recorded model answers, for tests that drive the binary end to end against real tools but no
//! live model. `CAUDRA_PROVIDER_RECORD=<file>` sends every request to the real provider and keeps
//! each answer in the file. `CAUDRA_PROVIDER_REPLAY=<file>` answers from such a file and never
//! builds a real provider, so a replay needs neither credentials nor a network.

use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use flume::Sender;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

use crate::model::{Model, ModelInfo, TokenUsage};
use crate::provider::{BoxFuture, Provider, WireRequest};
use crate::{
    AgentError, CacheKey, Message, ProviderEvent, ProviderUsage, ReasoningTransport,
    RequestOptions, StopReason, StreamResponse,
};

const RECORD_ENV: &str = "CAUDRA_PROVIDER_RECORD";
const REPLAY_ENV: &str = "CAUDRA_PROVIDER_REPLAY";
const BOTH_SET: &str = "set CAUDRA_PROVIDER_RECORD or CAUDRA_PROVIDER_REPLAY, not both";
const WITH_TOOLS: &str = " with tools";

#[derive(Default, Serialize, Deserialize)]
struct Cassette {
    /// The model the answers came from, which a replay has to name as well.
    model: String,
    answers: Vec<Answer>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Answer {
    /// Requests from different callers can interleave, so each lane replays in its own order.
    lane: String,
    message: Message,
    usage: TokenUsage,
    stop_reason: Option<StopReason>,
}

impl Answer {
    fn response(self) -> StreamResponse {
        StreamResponse {
            message: self.message,
            usage: self.usage,
            stop_reason: self.stop_reason,
            ..StreamResponse::default()
        }
    }
}

enum Tape {
    Record(Recorder),
    Replay(Reel),
}

struct Recorder {
    path: PathBuf,
    cassette: Mutex<Cassette>,
}

struct Reel {
    path: PathBuf,
    lanes: Mutex<HashMap<String, VecDeque<Answer>>>,
}

static TAPE: OnceLock<Result<Option<Tape>, String>> = OnceLock::new();

fn tape() -> Result<Option<&'static Tape>, AgentError> {
    TAPE.get_or_init(load)
        .as_ref()
        .map(Option::as_ref)
        .map_err(|message| AgentError::Config {
            message: message.clone(),
        })
}

fn load() -> Result<Option<Tape>, String> {
    match (env::var_os(RECORD_ENV), env::var_os(REPLAY_ENV)) {
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err(BOTH_SET.to_owned()),
        (Some(path), None) => {
            warn!(path = ?path, "recording every model answer");
            Ok(Some(Tape::Record(Recorder {
                path: path.into(),
                cassette: Mutex::default(),
            })))
        }
        (None, Some(path)) => {
            let path = PathBuf::from(path);
            warn!(path = %path.display(), "replaying recorded model answers");
            let cassette: Cassette = fs::read_to_string(&path)
                .map_err(|error| error.to_string())
                .and_then(|text| serde_json::from_str(&text).map_err(|error| error.to_string()))
                .map_err(|error| format!("cannot replay {}: {error}", path.display()))?;
            Ok(Some(Tape::Replay(Reel::new(path, cassette.answers))))
        }
    }
}

/// What `provider_for_slug` hands out: `build`'s provider, recording it when a cassette is being
/// recorded, or in a replay a provider that answers from the cassette instead.
pub(crate) fn intercept(
    build: impl FnOnce() -> Result<Box<dyn Provider>, AgentError>,
) -> Result<Box<dyn Provider>, AgentError> {
    Ok(match tape()? {
        None => build()?,
        Some(Tape::Record(recorder)) => Box::new(Recording {
            inner: build()?,
            recorder,
        }),
        Some(Tape::Replay(reel)) => Box::new(Replay(reel)),
    })
}

fn lane(model: &Model, tools: &Value) -> String {
    let with_tools = tools.as_array().is_some_and(|tools| !tools.is_empty());
    format!(
        "{}{}",
        model.spec(),
        if with_tools { WITH_TOOLS } else { "" }
    )
}

impl Recorder {
    fn keep(&self, model: &Model, answer: Answer) -> Result<(), AgentError> {
        let mut cassette = self.cassette.lock().unwrap_or_else(PoisonError::into_inner);
        if cassette.model.is_empty() {
            cassette.model = model.spec();
        }
        cassette.answers.push(answer);
        fs::write(&self.path, serde_json::to_string_pretty(&*cassette)? + "\n")?;
        Ok(())
    }
}

impl Reel {
    fn new(path: PathBuf, answers: Vec<Answer>) -> Self {
        let mut lanes: HashMap<String, VecDeque<Answer>> = HashMap::new();
        for answer in answers {
            lanes
                .entry(answer.lane.clone())
                .or_default()
                .push_back(answer);
        }
        Self {
            path,
            lanes: Mutex::new(lanes),
        }
    }

    fn next(&self, lane: &str) -> Result<Answer, AgentError> {
        self.lanes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(lane)
            .and_then(VecDeque::pop_front)
            .ok_or_else(|| AgentError::Config {
                message: format!(
                    "{} has no answer left for {lane}: the requests changed since it was recorded, so record it again",
                    self.path.display()
                ),
            })
    }
}

struct Recording {
    inner: Box<dyn Provider>,
    recorder: &'static Recorder,
}

impl Provider for Recording {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        cache_key: Option<&'a CacheKey>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let response = self
                .inner
                .stream_message(model, messages, system, tools, event_tx, opts, cache_key)
                .await?;
            let answer = Answer {
                lane: lane(model, tools),
                message: response.message.clone(),
                usage: response.usage,
                stop_reason: response.stop_reason,
            };
            self.recorder.keep(model, answer)?;
            Ok(response)
        })
    }

    fn wire_request(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        opts: &RequestOptions,
        cache_key: Option<&CacheKey>,
    ) -> Result<WireRequest, AgentError> {
        self.inner
            .wire_request(model, messages, system, tools, opts, cache_key)
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        self.inner.list_models()
    }

    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        self.inner.fetch_usage()
    }

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        self.inner.refresh_auth()
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        self.inner.reload_auth()
    }

    fn reload_auth_if_changed(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
        self.inner.reload_auth_if_changed()
    }

    fn adjust_model(&self, model: &mut Model) {
        self.inner.adjust_model(model);
    }

    fn reasoning_transport(&self, model: &Model) -> ReasoningTransport {
        self.inner.reasoning_transport(model)
    }

    fn prompt_cache_ttl(&self, model: &Model) -> Option<Duration> {
        self.inner.prompt_cache_ttl(model)
    }
}

struct Replay(&'static Reel);

impl Provider for Replay {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        _messages: &'a [Message],
        _system: &'a str,
        tools: &'a Value,
        _event_tx: &'a Sender<ProviderEvent>,
        _opts: RequestOptions,
        _cache_key: Option<&'a CacheKey>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        let answer = self.0.next(&lane(model, tools));
        Box::pin(async move { answer.map(Answer::response) })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentBlock, Role};
    use serde_json::json;
    use test_case::test_case;

    const SPEC: &str = "openai/gpt-6-luna";
    const RECORDED: &str = "recorded";

    fn answer(lane: String) -> Answer {
        Answer {
            lane,
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: RECORDED.into(),
                }],
                ..Message::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::EndTurn),
        }
    }

    #[test_case(json!([{"name": "mcp"}]), true ; "a_request_offering_tools")]
    #[test_case(json!([]), false ; "a_request_offering_none")]
    #[test_case(Value::Null, false ; "a_request_without_a_tools_list")]
    fn a_lane_tells_tool_requests_from_side_requests(tools: Value, with_tools: bool) {
        let model = Model::from_spec(SPEC).unwrap();
        assert_eq!(lane(&model, &tools).ends_with(WITH_TOOLS), with_tools);
    }

    /// A side request answered between two turns of the main conversation takes the side
    /// lane's answer, so the main lane keeps its order.
    #[test]
    fn each_lane_replays_in_its_own_order() {
        let model = Model::from_spec(SPEC).unwrap();
        let tools = json!([{"name": "mcp"}]);
        let main = lane(&model, &tools);
        let side = lane(&model, &Value::Null);
        let reel = Reel::new(
            PathBuf::from(SPEC),
            vec![answer(main.clone()), answer(side.clone())],
        );

        assert_eq!(reel.next(&side).unwrap().lane, side);
        assert_eq!(reel.next(&main).unwrap().lane, main);
        assert!(reel.next(&main).is_err());
    }
}
