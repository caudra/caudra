use crate::files;
use crate::providers::{
    BUILTIN_IGNORED_FIELDS, ModelDef, ModelFields, OverrideFields, PROVIDERS_VERSION,
    PURPOSE_FIELDS, ProviderDef, ProviderOverride,
};

use super::{Document, Header, RECORDS_USAGE, Table, code_list, global_location, preamble};

pub const PROVIDER: &str = "my-provider";
pub const PURPOSES: &str = "my-provider.purposes";
pub const MODEL_DEFAULTS: &str = "my-provider.model_defaults";
pub const MODELS: &str = "my-provider.models";
pub const UPSTREAM: &str = "aperture.overrides.llmserver";
const REWRITES: &str = "`caudra auth login` and `caudra auth logout` rewrite this file and drop \
     its comments, so keep a copy of the ones you want.";
const PURPOSES_ABOUT: &str = "Which models of my-provider are small and which are flagships. Each \
     key takes one prefix or a list.";
const MODEL_DEFAULTS_ABOUT: &str = "Model keys for every model of my-provider, including the ones \
     only discovery finds. A [[my-provider.models]] entry wins key by key.";
const MODELS_ABOUT: &str = "One model my-provider serves, so repeat the table for each model. A key \
     it leaves out comes from [my-provider.model_defaults], then from discovery, and then from the \
     default shown.";
const UPSTREAM_ABOUT: &str = "Aperture routes the models of upstream providers as \
     `aperture/UPSTREAM/MODEL`. Each [aperture.overrides.UPSTREAM] table overrides the models of \
     one upstream, such as llmserver here.";

/// Every `providers.toml` key, on an example custom provider and an Aperture
/// upstream.
pub fn document() -> Document {
    let file = &files::PROVIDERS;
    Document {
        preamble: preamble(
            file,
            [
                RECORDS_USAGE.to_owned(),
                global_location(file),
                REWRITES.to_owned(),
            ],
        ),
        version: PROVIDERS_VERSION,
        tables: vec![
            Table::of(Header::Record(PROVIDER.into()), ProviderDef::FIELDS).about(provider_about()),
            Table::of(Header::Record(PURPOSES.into()), PURPOSE_FIELDS).about(PURPOSES_ABOUT),
            Table::of(Header::Record(MODEL_DEFAULTS.into()), ModelFields::FIELDS)
                .about(MODEL_DEFAULTS_ABOUT),
            Table::of(
                Header::RecordArray(MODELS.into()),
                ModelDef::FIELDS.iter().chain(ModelFields::FIELDS),
            )
            .about(MODELS_ABOUT),
            Table::of(
                Header::Record(UPSTREAM.into()),
                OverrideFields::FIELDS
                    .iter()
                    .chain(ProviderOverride::FIELDS),
            )
            .about(UPSTREAM_ABOUT),
        ],
    }
}

fn provider_about() -> String {
    format!(
        "Each top-level table is one provider, named by its slug. A new slug, such as my-provider \
         here, adds a custom provider whose models are `my-provider/MODEL`. A built-in slug, such \
         as [anthropic], changes that provider and ignores {}, except that opencode reads \
         `enable_free_models`. In an environment variable name, `<SLUG>` is the slug in capitals \
         with `_` for `-`.",
        code_list(&BUILTIN_IGNORED_FIELDS)
    )
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use caudra_storage::thinking::ReasoningOptions;
    use serde::Serialize;

    use super::{MODEL_DEFAULTS, MODELS, PROVIDER, PURPOSES, UPSTREAM, document};
    use crate::example::Render;
    use crate::providers::{
        ModelDef, ModelFields, ModelPurpose, OverrideFields, Protocol, ProviderDef,
        ProviderOverride, ProvidersConfig, PurposeModels,
    };

    const TEXT: &str = "text";
    const NUMBER: u32 = 1;
    const PRICE: f64 = 1.0;

    fn parse(render: Render) -> ProvidersConfig {
        toml::from_str(&document().render(render)).unwrap()
    }

    fn names(path: &str) -> BTreeSet<String> {
        let document = document();
        let table = document.table(path).unwrap();
        table
            .entries
            .iter()
            .map(|entry| entry.name.into())
            .collect()
    }

    fn keys(value: impl Serialize) -> BTreeSet<String> {
        let value = serde_json::to_value(value).unwrap();
        value.as_object().unwrap().keys().cloned().collect()
    }

    fn model_fields() -> ModelFields {
        ModelFields {
            context_window: Some(NUMBER),
            max_output_tokens: Some(NUMBER),
            supports_tool_examples: Some(true),
            supports_thinking: Some(true),
            requires_thinking: Some(true),
            supports_vision: Some(true),
            supports_cache_breakpoints: Some(true),
            reasoning_options: Some(ReasoningOptions::default()),
            pricing_input: Some(PRICE),
            pricing_output: Some(PRICE),
            pricing_cache_write: Some(PRICE),
            pricing_cache_read: Some(PRICE),
            pricing_fast_input: Some(PRICE),
            pricing_fast_output: Some(PRICE),
        }
    }

    fn override_fields() -> OverrideFields {
        OverrideFields {
            context_window: Some(NUMBER),
            max_output_tokens: Some(NUMBER),
            supports_thinking: Some(true),
            supports_vision: Some(true),
            base: Some(TEXT.into()),
            path_prefix: Some(TEXT.into()),
        }
    }

    #[test]
    fn the_reference_declares_no_provider() {
        assert!(parse(Render::Reference).providers.is_empty());
    }

    #[test]
    fn every_stated_default_is_the_built_in_one() {
        assert_eq!(
            serde_json::to_value(parse(Render::Live { defaults: true })).unwrap(),
            serde_json::to_value(parse(Render::Live { defaults: false })).unwrap()
        );
    }

    #[test]
    fn every_key_the_schema_writes_is_described() {
        let model = ModelDef {
            id: TEXT.into(),
            fields: model_fields(),
        };
        let prefixes: PurposeModels = toml::Value::from(TEXT).try_into().unwrap();
        let provider = ProviderDef {
            display_name: Some(TEXT.into()),
            protocol: Some(Protocol::Openai),
            base_url: Some(TEXT.into()),
            plan: Some(TEXT.into()),
            api_key_env: Some(TEXT.into()),
            api_key: Some(TEXT.into()),
            default_model: Some(TEXT.into()),
            discover_models: true,
            enable_free_models: Some(true),
            overrides: HashMap::from([(TEXT.into(), ProviderOverride::default())]),
            model_defaults: Some(model_fields()),
            purposes: HashMap::from([(ModelPurpose::Fast, prefixes)]),
            models: vec![model.clone()],
        };
        let upstream = ProviderOverride {
            default: override_fields(),
            models: HashMap::from([(TEXT.into(), override_fields())]),
        };
        let classes = ModelPurpose::CLASSES.map(|purpose| purpose.to_string());
        assert_eq!(names(PROVIDER), keys(provider));
        assert_eq!(names(PURPOSES), BTreeSet::from(classes));
        assert_eq!(names(MODEL_DEFAULTS), keys(model_fields()));
        assert_eq!(names(MODELS), keys(model));
        assert_eq!(names(UPSTREAM), keys(upstream));
    }
}
