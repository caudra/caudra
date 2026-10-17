use crate::decisions::DecisionEndpoint;
use crate::files;
use crate::providers::{
    AccountDef, BUILTIN_IGNORED_FIELDS, BUILTIN_MODEL_FIELDS, GlobalSwitchingDef, ModelDef,
    ModelFields, OverrideFields, PROVIDERS_VERSION, PURPOSE_FIELDS, ProviderDef, ProviderOverride,
    SWITCHING_KEY, SwitchingDef,
};

use super::{Document, Header, RECORDS_USAGE, Table, code_list, global_location, preamble};

pub const PROVIDER: &str = "my-provider";
pub const PURPOSES: &str = "my-provider.purposes";
pub const MODEL_DEFAULTS: &str = "my-provider.model_defaults";
pub const MODELS: &str = "my-provider.models";
pub const SWITCHING: &str = "my-provider.switching";
pub const ACCOUNTS: &str = "my-provider.accounts";
pub const UPSTREAM: &str = "aperture.overrides.llmserver";
pub const DECISION_ENDPOINT: &str = "decisions.my-endpoint";
pub const PROVIDER_ORDER: &str = SWITCHING_KEY;
const REWRITES: &str = "`caudra auth login` and `caudra auth logout` replace only the table they \
     change, and keep the rest of the file and its comments.";
const PURPOSES_ABOUT: &str = "Which models of my-provider are small and which are flagships. Each \
     key takes one prefix or a list.";
const MODEL_DEFAULTS_ABOUT: &str = "Model keys for every model of my-provider, including the ones \
     only discovery finds. A [[my-provider.models]] entry wins key by key.";
const MODELS_ABOUT: &str = "One model my-provider serves, so repeat the table for each model. A key \
     it leaves out comes from [my-provider.model_defaults], then from discovery, and then from the \
     default shown.";
const SWITCHING_ABOUT: &str = "When Auto moves a session from one account of my-provider to \
     another. It applies once the provider has two or more accounts and its selection is Auto.";
const ACCOUNTS_ABOUT: &str = "One account to sign in to my-provider with, so repeat the table for \
     each account. The first entry has the highest priority. Accounts saved by `caudra auth login` \
     that no entry lists come after these. A slug whose table holds only accounts and switching, \
     such as a built-in one, stays the provider it was.";
const UPSTREAM_ABOUT: &str = "Aperture routes the models of upstream providers as \
     `aperture/UPSTREAM/MODEL`. Each [aperture.overrides.UPSTREAM] table overrides the models of \
     one upstream, such as llmserver here.";
const DECISION_ENDPOINT_ABOUT: &str = "One decision endpoint, named my-endpoint here. Configuring \
     it is the consent to send decision context to it, and `/decision-model` or `caudra decisions \
     use` makes it the active one.";
const PROVIDER_ORDER_ABOUT: &str = "Lets Auto move a conversation to another provider's \
     subscription when its own provider's subscriptions run low. It applies only to a provider \
     whose selection is Auto, and every move loses the prompt cache.";

/// Every `providers.toml` key, on an example custom provider, an Aperture
/// upstream, and a decision endpoint.
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
            Table::of(Header::Record(SWITCHING.into()), SwitchingDef::FIELDS)
                .about(SWITCHING_ABOUT),
            Table::of(Header::RecordArray(ACCOUNTS.into()), AccountDef::FIELDS)
                .about(ACCOUNTS_ABOUT),
            Table::of(
                Header::Record(UPSTREAM.into()),
                OverrideFields::FIELDS
                    .iter()
                    .chain(ProviderOverride::FIELDS),
            )
            .about(UPSTREAM_ABOUT),
            Table::of(
                Header::Record(DECISION_ENDPOINT.into()),
                DecisionEndpoint::FIELDS,
            )
            .about(DECISION_ENDPOINT_ABOUT),
            Table::of(
                Header::Record(PROVIDER_ORDER.into()),
                GlobalSwitchingDef::FIELDS,
            )
            .about(PROVIDER_ORDER_ABOUT),
        ],
    }
}

fn provider_about() -> String {
    format!(
        "Each top-level table is one provider, named by its slug. A new slug, such as my-provider \
         here, adds a custom provider whose models are `my-provider/MODEL`. A built-in slug, such \
         as [anthropic], changes that provider and ignores {}, except that opencode reads \
         `enable_free_models`. From [anthropic.model_defaults] and [[anthropic.models]], a \
         built-in reads only these model keys: {}. In an environment variable name, `<SLUG>` is \
         the slug in capitals with `_` for `-`.",
        code_list(&BUILTIN_IGNORED_FIELDS),
        code_list(&BUILTIN_MODEL_FIELDS)
    )
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::mem;

    use caudra_storage::thinking::ReasoningOptions;
    use serde::Serialize;

    use super::{
        ACCOUNTS, DECISION_ENDPOINT, MODEL_DEFAULTS, MODELS, PROVIDER, PROVIDER_ORDER, PURPOSES,
        SWITCHING, UPSTREAM, document,
    };
    use crate::decisions::{DecisionEndpoint, DecisionEndpointDef, DecisionProtocol};
    use crate::example::Render;
    use crate::providers::{
        AccountDef, GlobalSwitchingDef, ModelDef, ModelFields, ModelPurpose, OverrideFields,
        Protocol, ProviderDef, ProviderOverride, ProvidersConfig, PurposeModels, QuotaWindowName,
        SwitchStrategy, SwitchingDef,
    };

    const TEXT: &str = "text";
    const ACCOUNT_NAME: &str = "personal";
    const SWITCH_BELOW: u8 = 1;
    const PREFER_BELOW: u8 = 2;
    const NUMBER: u32 = 1;
    const PRICE: f64 = 1.0;

    fn parse(render: Render) -> ProvidersConfig {
        toml::from_str(&document().render(render)).unwrap()
    }

    /// Decision endpoints compare once resolved, since an unset `allow_http`
    /// and a stated `false` are the same endpoint.
    fn resolved(render: Render) -> (serde_json::Value, Vec<DecisionEndpoint>) {
        let mut config = parse(render);
        let endpoints = mem::take(&mut config.decisions)
            .iter()
            .map(|(name, def)| def.resolve(name).unwrap())
            .collect();
        (serde_json::to_value(config).unwrap(), endpoints)
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
            supports_pdf: Some(true),
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
        let reference = parse(Render::Reference);
        assert!(reference.providers.is_empty());
        assert!(reference.decisions.is_empty());
        assert!(reference.switching.is_none());
    }

    #[test]
    fn every_stated_default_is_the_built_in_one() {
        assert_eq!(
            resolved(Render::Live { defaults: true }),
            resolved(Render::Live { defaults: false })
        );
    }

    #[test]
    fn every_key_the_schema_writes_is_described() {
        let model = ModelDef {
            id: TEXT.into(),
            fields: model_fields(),
        };
        let prefixes: PurposeModels = toml::Value::from(TEXT).try_into().unwrap();
        let switching = SwitchingDef {
            strategy: SwitchStrategy::Paced,
            switch_below: SWITCH_BELOW,
            prefer_below: PREFER_BELOW,
            windows: Some(vec![QuotaWindowName::Weekly]),
        };
        let account = AccountDef {
            name: ACCOUNT_NAME.into(),
            api_key_env: Some(TEXT.into()),
            fallback: true,
        };
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
            switching: Some(switching.clone()),
            accounts: vec![account.clone()],
        };
        let upstream = ProviderOverride {
            default: override_fields(),
            models: HashMap::from([(TEXT.into(), override_fields())]),
        };
        let endpoint = DecisionEndpointDef {
            protocol: DecisionProtocol::OpenAI,
            base_url: TEXT.into(),
            model: Some(TEXT.into()),
            api_key_env: Some(TEXT.into()),
            allow_http: Some(true),
        };
        let classes = ModelPurpose::CLASSES.map(|purpose| purpose.to_string());
        assert_eq!(names(PROVIDER), keys(provider));
        assert_eq!(names(PURPOSES), BTreeSet::from(classes));
        assert_eq!(names(MODEL_DEFAULTS), keys(model_fields()));
        assert_eq!(names(MODELS), keys(model));
        assert_eq!(names(SWITCHING), keys(switching));
        assert_eq!(names(ACCOUNTS), keys(account));
        assert_eq!(names(UPSTREAM), keys(upstream));
        assert_eq!(names(DECISION_ENDPOINT), keys(endpoint));
        assert_eq!(
            names(PROVIDER_ORDER),
            keys(GlobalSwitchingDef {
                providers: vec![TEXT.into()],
            })
        );
    }
}
