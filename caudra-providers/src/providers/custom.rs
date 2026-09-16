use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use flume::Sender;
use serde_json::Value;

use caudra_config::providers::{
    ModelPurpose, Protocol, ProviderDef, ProvidersConfig, resolve_api_key_env, resolve_base_url,
    resolve_protocol,
};

use super::ResolvedAuth;
use super::openai::responses;
use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::manifest::ManifestRegistry;
use crate::model::{
    Billing, FastPricing, Model, ModelFacts, ModelFamily, ModelPricing, ThinkingSupport,
};
use crate::provider::{BoxFuture, Provider, ProviderKind};
use crate::providers::Timeouts;
use crate::{
    AgentError, CacheKey, Message, ProviderEvent, RequestOptions, StreamResponse, ThinkingConfig,
};

static CUSTOM_OPENAI_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    // Custom providers resolve their own base URL (including any override) from
    // config, so the compat-layer fallback slug is unused here.
    slug: "",
    api_key_env: "",
    base_url: "",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "custom",
};

fn protocol_kind(protocol: Protocol) -> ProviderKind {
    match protocol {
        Protocol::Openai | Protocol::OpenaiResponses => ProviderKind::OpenAi,
        Protocol::Anthropic => ProviderKind::Anthropic,
        Protocol::Google => ProviderKind::Google,
    }
}

/// Builtins win their slug in `from_spec`/`create`, so every custom path skips
/// them. Key off the manifest (every builtin), not `builtin_provider`, which
/// omits the `opencode` slugs and would let them shadow the builtin.
fn is_builtin_slug(slug: &str) -> bool {
    ManifestRegistry::get(slug).is_some()
}

pub fn base_kind(slug: &str) -> Option<ProviderKind> {
    let config = ProvidersConfig::load();
    Some(protocol_kind(config.get(slug)?.protocol?))
}

fn resolve_custom_auth(slug: &str) -> Result<ResolvedAuth, AgentError> {
    let config = ProvidersConfig::load();
    let def = config.get(slug).ok_or_else(|| AgentError::Config {
        message: format!("unknown custom provider '{slug}'"),
    })?;

    let resolved_env = resolve_api_key_env(slug, Some(def));
    let env_var = def.api_key_env.as_deref().unwrap_or(&resolved_env);
    let pool = super::KeyPool::resolve(slug, env_var)?;

    let base_url = resolve_base_url(slug, Some(def));
    let mut auth = ResolvedAuth::bearer(pool.current());
    auth.base_url = base_url;
    Ok(auth)
}

pub fn create(slug: &str, timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    let kind = base_kind(slug).ok_or_else(|| AgentError::Config {
        message: format!("unknown custom provider '{slug}'"),
    })?;
    let resolved = resolve_custom_auth(slug)?;
    let auth = Arc::new(Mutex::new(resolved));

    let config = ProvidersConfig::load();
    let protocol = resolve_protocol(slug, config.get(slug)).unwrap_or(Protocol::Openai);

    match kind {
        ProviderKind::Anthropic => Ok(Box::new(super::anthropic::Anthropic::with_auth(
            auth, timeouts,
        ))),
        ProviderKind::OpenAi => Ok(Box::new(CustomOpenAiProvider {
            compat: OpenAiCompatProvider::new(&CUSTOM_OPENAI_CONFIG, timeouts),
            auth,
            protocol,
        })),
        ProviderKind::Google => Ok(Box::new(super::google::Google::with_auth(auth, timeouts))),
        _ => Err(AgentError::Config {
            message: format!(
                "unsupported protocol for custom provider '{slug}', only openai/anthropic/google are supported"
            ),
        }),
    }
}

pub fn lookup_model(slug: &str, model_id: &str) -> Option<Model> {
    if is_builtin_slug(slug) {
        return None;
    }
    let config = ProvidersConfig::load();
    let def = config.get(slug)?;
    let kind = protocol_kind(def.protocol?);
    Some(model_from_def(def, kind, slug, model_id))
}

/// Model ids this provider declares for `purpose` in `providers.toml`, the one
/// that wins the slot first.
pub fn declared_purpose(slug: &str, purpose: ModelPurpose) -> Vec<String> {
    ProvidersConfig::load()
        .get(slug)
        .and_then(|def| def.purposes.get(&purpose))
        .map(|models| models.iter().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// Facts declared in `providers.toml`. Each lane is declaration-ordered, so its
/// first matching prefix determines whether the model is that lane's default;
/// the more specific match wins when Fast and Best overlap.
pub fn facts_for_model(slug: &str, model_id: &str) -> Option<ModelFacts> {
    let config = ProvidersConfig::load();
    let def = config.get(slug)?;
    facts_from_def(def, model_id)
}

fn facts_from_def(def: &ProviderDef, model_id: &str) -> Option<ModelFacts> {
    ModelPurpose::CLASSES
        .into_iter()
        .filter_map(|purpose| {
            def.purposes.get(&purpose).and_then(|models| {
                models
                    .iter()
                    .enumerate()
                    .find(|(_, prefix)| model_id.starts_with(prefix))
                    .map(|(index, prefix)| {
                        (
                            prefix.len(),
                            ModelFacts {
                                small: purpose == ModelPurpose::Fast,
                                default: index == 0,
                            },
                        )
                    })
            })
        })
        .max_by_key(|(prefix_len, _)| *prefix_len)
        .map(|(_, facts)| facts)
}

/// A `providers.toml` entry whose id matches no live model silently voids every
/// setting on it, which reads as caudra ignoring the config. Say so once per
/// id: `model_from_def` runs per request, and provider defaults are the fix.
fn warn_unmatched_model_id(def: &ProviderDef, slug: &str, model_id: &str) {
    if def.models.is_empty() || def.declares(model_id) {
        return;
    }
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !seen.insert(format!("{slug}/{model_id}")) {
        return;
    }
    let declared: Vec<&str> = def.models.iter().map(|m| m.id.as_str()).collect();
    tracing::warn!(
        provider = %slug,
        model = %model_id,
        declared = ?declared,
        "no providers.toml entry for this model; its settings are ignored. \
         Use [{slug}.model_defaults] for settings that apply to every model"
    );
}

/// Build a model from an already-loaded provider definition so declared settings
/// and id lookup can share one `providers.toml` read instead of loading twice.
fn model_from_def(def: &ProviderDef, kind: ProviderKind, slug: &str, model_id: &str) -> Model {
    warn_unmatched_model_id(def, slug, model_id);
    let declared = def.model_settings(model_id);
    let discovered = crate::model_registry::discovered(slug, model_id);
    let discovered = discovered.as_ref();
    let max_output_tokens = declared
        .max_output_tokens
        .or_else(|| discovered.and_then(|d| d.max_output_tokens))
        .or_else(|| kind.fallback_max_output());
    let context_window = declared
        .context_window
        .or_else(|| discovered.and_then(|d| d.context_window))
        .unwrap_or_else(|| kind.fallback_context_window());
    let supports_tool_examples_override = declared.supports_tool_examples;
    let thinking_override = ThinkingSupport::from_flags(
        declared
            .supports_thinking
            .or_else(|| ManifestRegistry::get(&kind.to_string()).map(|m| m.supports_thinking)),
        declared.requires_thinking.unwrap_or(false),
    );
    let supports_vision_override = declared.supports_vision;
    let pricing = Some(&declared)
        .filter(|m| m.has_pricing())
        .map(|m| ModelPricing {
            input: m.pricing_input.unwrap_or(0.0),
            output: m.pricing_output.unwrap_or(0.0),
            cache_write: m.pricing_cache_write.unwrap_or(0.0),
            cache_read: m.pricing_cache_read.unwrap_or(0.0),
            fast: Some(m)
                .filter(|d| d.has_fast_pricing())
                .map(|d| FastPricing {
                    input: d.pricing_fast_input.unwrap_or(0.0),
                    output: d.pricing_fast_output.unwrap_or(0.0),
                }),
            tiers: Vec::new(),
        })
        .unwrap_or_default();
    Model {
        id: model_id.to_string(),
        provider: Arc::from(slug),
        // `kind` is the wire protocol, never the weights: an OpenAI-shaped
        // endpoint serves whatever the operator loaded. Windows and thinking
        // defaults above are transport concerns and may follow it; lineage may
        // not, so capabilities come from config or stay off.
        family: ModelFamily::Generic,
        supports_tool_examples_override,
        thinking_override,
        supports_vision_override,
        pricing,
        discovered_free: false,
        max_output_tokens,
        context_window,
        window_excludes_output: false,
        reasoning_options: declared.reasoning_options.unwrap_or_default(),
        thinking_fields: None,
        billing: Billing::default(),
    }
}

/// `reasoning_effort`, not Anthropic's `thinking`: a strict OpenAI-compatible
/// server 400s on an unknown top-level key.
///
/// A model that declares no reasoning options gets neither key. These endpoints
/// reject an unrecognized level outright instead of falling back to a default,
/// and with nothing declared there is no ladder to snap the level onto, so
/// `ThinkingConfig::resolve` would pass it through verbatim. Declaring
/// `reasoning_options` in `providers.toml` is the opt-in.
fn apply_declared_effort(thinking: &ThinkingConfig, body: &mut Value, model: &Model) {
    if !model.reasoning_options().is_empty() {
        thinking.apply_reasoning_effort(body, model);
    }
}

fn build_responses_body(
    model: &Model,
    messages: &[Message],
    system: &str,
    tools: &Value,
    thinking: &ThinkingConfig,
    cache_key: Option<&CacheKey>,
) -> Value {
    let mut body = responses::build_body(model, messages, system, tools);
    responses::apply_responses_reasoning(&mut body, thinking, model);
    if let Some(max_output_tokens) = model.max_output_tokens {
        body["max_output_tokens"] = Value::from(max_output_tokens);
    }
    responses::apply_prompt_cache_key(&mut body, cache_key);
    body
}

fn build_chat_body(
    compat: &OpenAiCompatProvider,
    model: &Model,
    messages: &[Message],
    system: &str,
    tools: &Value,
    thinking: &ThinkingConfig,
    cache_key: Option<&CacheKey>,
) -> Value {
    let mut body = compat.build_body(model, messages, system, tools);
    apply_declared_effort(thinking, &mut body, model);
    responses::apply_prompt_cache_key(&mut body, cache_key);
    body
}

/// Specs declared statically in `providers.toml` (no HTTP).
pub fn declared_model_specs() -> Vec<String> {
    declared_specs_from(&ProvidersConfig::load())
}

fn declared_specs_from(config: &ProvidersConfig) -> Vec<String> {
    let mut specs = Vec::new();
    for (slug, def) in &config.providers {
        if is_builtin_slug(slug) {
            continue;
        }
        if resolve_protocol(slug, Some(def)).is_none() {
            continue;
        }
        for m in &def.models {
            specs.push(format!("{slug}/{}", m.id));
        }
    }
    specs
}

/// Skip definitions handled by [`declared_model_specs`]; only HTTP `/models`
/// goes through here, so an empty `discover_models = false` provider returns
/// nothing and never hits the network.
pub fn discover_models(timeouts: Timeouts) -> Vec<String> {
    let config = ProvidersConfig::load();
    let mut all_specs = Vec::new();
    for slug in config.providers.keys() {
        if is_builtin_slug(slug) {
            continue;
        }
        let def = config.get(slug).unwrap();
        if !def.discover_models {
            continue;
        }
        if resolve_protocol(slug, Some(def)).is_none() {
            continue;
        }
        match create(slug, timeouts) {
            Ok(provider) => {
                let slug_c = slug.clone();
                let result = smol::block_on(provider.list_models());
                match result {
                    Ok(models) => {
                        crate::model_registry::set_known_models(&slug_c, models.clone());
                        for m in models {
                            all_specs.push(format!("{slug_c}/{}", m.id));
                        }
                    }
                    Err(e) => {
                        tracing::warn!(slug, error = %e, "failed to list models for custom provider");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(slug, error = %e, "failed to create custom provider");
            }
        }
    }
    all_specs
}

struct CustomOpenAiProvider {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    protocol: Protocol,
}

impl Provider for CustomOpenAiProvider {
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
            let auth = self.auth.lock().unwrap().clone();

            if self.protocol == Protocol::OpenaiResponses {
                let body =
                    build_responses_body(model, messages, system, tools, &opts.thinking, cache_key);
                return responses::do_stream(
                    self.compat.client(),
                    model,
                    &body,
                    event_tx,
                    &auth,
                    self.compat.stream_timeout(),
                )
                .await;
            }

            let body = build_chat_body(
                &self.compat,
                model,
                messages,
                system,
                tools,
                &opts.thinking,
                cache_key,
            );
            self.compat
                .do_stream(model, &[], &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        let auth = self.auth.lock().unwrap().clone();
        Box::pin(async move { self.compat.do_list_models(&auth).await })
    }

    fn reasoning_transport(&self, _model: &Model) -> crate::ReasoningTransport {
        if self.protocol == Protocol::OpenaiResponses {
            crate::ReasoningTransport::OpenAiResponses
        } else {
            crate::ReasoningTransport::Other
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelInfo, ModelMarker};
    use crate::types::ThinkingConfig;

    const ANTHROPIC_KEY_LEAKED: &str =
        "an OpenAI-compatible body must never carry Anthropic's `thinking` key";
    const CACHE_KEY: &str = "session/task";

    fn openai_def(model_id: &str) -> ProviderDef {
        serde_json::from_str(&format!(
            r#"{{"protocol":"openai","models":[{{"id":"{model_id}"}}]}}"#
        ))
        .unwrap()
    }

    fn purpose_def() -> ProviderDef {
        serde_json::from_str(
            r#"{
                "purposes": {
                    "fast": ["small", "small-special", "tiny"],
                    "best": ["large", "medium"]
                }
            }"#,
        )
        .unwrap()
    }

    #[test_case::test_case("small-v2", true, true, Some(ModelMarker::Fast) ; "small_default")]
    #[test_case::test_case("tiny-v2", true, false, Some(ModelMarker::Small) ; "small_non_default")]
    #[test_case::test_case("large-v2", false, true, Some(ModelMarker::Best) ; "non_small_default")]
    #[test_case::test_case("medium-v2", false, false, None ; "non_small_non_default")]
    fn custom_facts_preserve_lane_and_default(
        model_id: &str,
        small: bool,
        default: bool,
        marker: Option<ModelMarker>,
    ) {
        let facts = facts_from_def(&purpose_def(), model_id).unwrap();

        assert_eq!(facts.small, small);
        assert_eq!(facts.default, default);
        assert_eq!(facts.marker(), marker);
    }

    #[test]
    fn custom_default_comes_from_the_first_matching_prefix() {
        let facts = facts_from_def(&purpose_def(), "small-special-v2").unwrap();

        assert_eq!(
            facts,
            ModelFacts {
                small: true,
                default: true,
            }
        );
    }

    /// The protocol says how to frame the request, never what the weights are:
    /// every OpenAI-compatible local server used to inherit `ModelFamily::Gpt`
    /// and with it the Codex editor, vision, and a tool-example encoding that
    /// the OpenAI translations drop on the floor.
    #[test_case::test_case("openai" ; "chat completions")]
    #[test_case::test_case("openai-responses" ; "responses")]
    fn an_openai_shaped_protocol_never_implies_gpt_weights(protocol: &str) {
        let def: ProviderDef = serde_json::from_str(&format!(
            r#"{{"protocol":"{protocol}","models":[{{"id":"qwen3.8-27b-cyberstrike"}}]}}"#
        ))
        .unwrap();

        let model = model_from_def(
            &def,
            ProviderKind::OpenAi,
            "local-openai-shaped-test",
            "qwen3.8-27b-cyberstrike",
        );

        assert_eq!(model.family, ModelFamily::Generic, "{protocol}");
        assert!(
            !model.prefers_apply_patch(),
            "{protocol}: got the Codex editor"
        );
        assert!(!model.supports_vision(), "{protocol}: claimed vision");
        assert!(
            !model.supports_tool_examples(),
            "{protocol}: examples would go to `input_examples`, which the \
             OpenAI translations drop instead of sending"
        );
    }

    /// Declaring a capability is still how you get it; only the guess is gone.
    #[test]
    fn declared_capabilities_still_win_over_the_generic_default() {
        let def: ProviderDef = serde_json::from_str(
            r#"{"protocol":"openai","models":[{"id":"m","supports_vision":true,"supports_tool_examples":true}]}"#,
        )
        .unwrap();

        let model = model_from_def(&def, ProviderKind::OpenAi, "declared-caps-test", "m");

        assert!(model.supports_vision());
        assert!(model.supports_tool_examples());
    }

    // `opencode` is a builtin whose slug is absent from the `builtin_provider`
    // inventory; the old guard leaked it into the picker, where it then resolved
    // as the builtin and silently dropped the custom model. Listing must skip
    // every builtin slug so a providers.toml entry can never shadow one.
    #[test]
    fn declared_specs_skip_builtin_named_entries_but_keep_custom() {
        let mut config = ProvidersConfig::default();
        config.upsert("opencode".to_string(), openai_def("shadow-model"));
        config.upsert("my-custom".to_string(), openai_def("real-model"));

        let specs = declared_specs_from(&config);
        assert!(
            !specs.iter().any(|s| s.starts_with("opencode/")),
            "builtin slug must be skipped in custom listing: {specs:?}"
        );
        assert!(specs.contains(&"my-custom/real-model".to_string()));

        // Resolution owns the builtin slug regardless of the providers.toml entry.
        let model = Model::from_spec("opencode/shadow-model").unwrap();
        assert_eq!(model.provider.as_ref(), "opencode");
    }

    // The exact regression this fixes: discovery parsed context_window but
    // never stored it, so custom models always got the protocol fallback.
    #[test]
    fn discovered_metadata_flows_into_custom_model_from_def() {
        let slug = "custom-discovery-metadata-test";
        let model_id = "vllm-model";
        let expected_window: u32 = 131_072;
        let expected_output: u32 = 8_192;

        crate::model_registry::set_known_models(
            slug,
            vec![ModelInfo {
                context_window: Some(expected_window),
                max_output_tokens: Some(expected_output),
                ..ModelInfo::id_only(model_id.to_string())
            }],
        );

        let def = openai_def(model_id);
        let model = model_from_def(&def, ProviderKind::OpenAi, slug, model_id);
        assert_eq!(model.context_window, expected_window);
        assert_eq!(model.max_output_tokens, Some(expected_output));
    }

    /// A strict OpenAI-compatible server 400s on `thinking`, so the effort has
    /// to travel as `reasoning_effort` or not at all. `none` only goes out
    /// when the model declared it; a model with no reasoning options must get
    /// a body with neither key.
    #[test_case::test_case(
        r#"[{"type":"effort","values":["none","low","xhigh"]}]"#,
        ThinkingConfig::Off => Some("none".to_string()) ; "declared_none_is_how_off_is_spelled"
    )]
    #[test_case::test_case(
        r#"[{"type":"effort","values":["none","low","xhigh"]}]"#,
        ThinkingConfig::Effort("xhigh".into()) => Some("xhigh".to_string()) ; "declared_level"
    )]
    #[test_case::test_case("[]", ThinkingConfig::Off => None ; "no_reasoning_options_sends_nothing")]
    #[test_case::test_case(
        "[]",
        ThinkingConfig::Effort("max".into()) => None ; "undeclared_effort_is_not_guessed"
    )]
    fn custom_openai_body_carries_effort_never_anthropic_thinking(
        reasoning_options: &str,
        thinking: ThinkingConfig,
    ) -> Option<String> {
        let def: ProviderDef = serde_json::from_str(&format!(
            r#"{{"protocol":"openai","models":[{{"id":"m","reasoning_options":{reasoning_options}}}]}}"#
        ))
        .unwrap();
        let model = model_from_def(&def, ProviderKind::OpenAi, "effort-body-test", "m");
        let provider = OpenAiCompatProvider::new(&CUSTOM_OPENAI_CONFIG, Timeouts::default());

        let mut body = provider.build_body(&model, &[], "", &Value::Null);
        apply_declared_effort(&thinking, &mut body, &model);

        assert!(body.get("thinking").is_none(), "{ANTHROPIC_KEY_LEAKED}");
        body.get("reasoning_effort")
            .map(|level| level.as_str().unwrap().to_string())
    }

    #[test]
    fn custom_chat_body_keeps_chat_completions_shape() {
        let model = model_from_def(
            &openai_def("m"),
            ProviderKind::OpenAi,
            "chat-body-test",
            "m",
        );
        let provider = OpenAiCompatProvider::new(&CUSTOM_OPENAI_CONFIG, Timeouts::default());

        let body = provider.build_body(
            &model,
            &[Message::user("hello".into())],
            "system",
            &Value::Null,
        );

        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hello");
        assert!(body.get("input").is_none());
        assert!(body.get("store").is_none());
    }

    #[test]
    fn custom_responses_body_carries_declared_options_and_full_history() {
        let def: ProviderDef = serde_json::from_str(
            r#"{"protocol":"openai-responses","models":[{"id":"m","max_output_tokens":8192,"reasoning_options":[{"type":"effort","values":["none","xhigh"]}]}]}"#,
        )
        .unwrap();
        let model = model_from_def(&def, ProviderKind::OpenAi, "responses-body-test", "m");
        let messages = [Message::user("hello".into())];

        let body = build_responses_body(
            &model,
            &messages,
            "system",
            &Value::Null,
            &ThinkingConfig::Effort("xhigh".into()),
            None,
        );

        assert_eq!(body["store"], false);
        assert_eq!(body["max_output_tokens"], 8192);
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["content"][0]["text"], "hello");
        assert!(body.get("previous_response_id").is_none());
    }

    /// A custom endpoint is OpenAI-compatible by declaration, so both wire
    /// shapes carry the conversation key an OpenAI server routes caches by.
    #[test_case::test_case(true ; "responses")]
    #[test_case::test_case(false ; "chat_completions")]
    fn custom_bodies_carry_the_conversation_cache_key(responses: bool) {
        let model = model_from_def(
            &openai_def("m"),
            ProviderKind::OpenAi,
            "cache-key-test",
            "m",
        );
        let compat = OpenAiCompatProvider::new(&CUSTOM_OPENAI_CONFIG, Timeouts::default());
        let key = CacheKey::task(None, CACHE_KEY);
        let thinking = ThinkingConfig::Off;

        let [keyed, unkeyed] = [Some(&key), None].map(|cache_key| {
            if responses {
                build_responses_body(&model, &[], "", &Value::Null, &thinking, cache_key)
            } else {
                build_chat_body(&compat, &model, &[], "", &Value::Null, &thinking, cache_key)
            }
        });

        assert_eq!(keyed[responses::PROMPT_CACHE_KEY_FIELD], CACHE_KEY);
        assert!(unkeyed.get(responses::PROMPT_CACHE_KEY_FIELD).is_none());
    }
}
