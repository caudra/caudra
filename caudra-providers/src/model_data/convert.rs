use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

use caudra_config::decisions::DecisionProtocol;
use caudra_models::{self as wire, SUPPORTED_REQUIREMENTS};
use caudra_storage::thinking::{ReasoningOption, ReasoningOptions};
use tracing::warn;

use crate::decision_pricing::DecisionModelEntry;
use crate::manifest::{Availability, ProviderManifest};
use crate::model::{
    FastPricing, ModelEntry, ModelFamily, ModelGeneration, ModelPricing, PricingTier,
    StaticReasoningOption,
};
use crate::pricing::{Period, PricePeriod, PricingSchedule, PricingWindow};
use crate::providers::catalog::{CatalogData, schema};

const INVALID_WINDOW: &str = "pricing window must cover distinct hours within a UTC day";
const INVALID_SCHEDULE: &str =
    "pricing schedule needs windows and a finite multiplier greater than one";
const INVALID_PREFIXES: &str =
    "model entries need at least one prefix, and prefixes cannot be empty";

pub(super) fn catalog(providers: BTreeMap<String, wire::CatalogProvider>) -> CatalogData {
    CatalogData::from_index(
        providers
            .into_iter()
            .map(|(slug, provider)| {
                let provider = schema::CatalogProvider {
                    name: provider.name,
                    env: provider.env,
                    npm: provider.npm,
                    api: provider.api,
                    models: provider
                        .models
                        .into_iter()
                        .map(|(id, model)| (id, catalog_model(model)))
                        .collect(),
                };
                (slug, provider)
            })
            .collect(),
    )
}

fn catalog_model(model: wire::CatalogModel) -> schema::CatalogModel {
    let reasoning_options = model.reasoning_options.map(|options| {
        ReasoningOptions::new(
            options
                .into_iter()
                .filter_map(|option| match option {
                    wire::ReasoningOption::Toggle => Some(ReasoningOption::Toggle),
                    wire::ReasoningOption::Effort { values } => {
                        Some(ReasoningOption::Effort { values })
                    }
                    wire::ReasoningOption::BudgetTokens { min, max } => {
                        Some(ReasoningOption::BudgetTokens { min, max })
                    }
                    wire::ReasoningOption::Unknown => None,
                })
                .collect(),
        )
    });
    schema::CatalogModel {
        limit: model.limit,
        cost: model.cost,
        provider: model.provider,
        attachment: model.attachment,
        reasoning: model.reasoning,
        modalities: model.modalities,
        reasoning_options,
    }
}

fn strings(values: Vec<String>) -> &'static [&'static str] {
    Box::leak(
        values
            .into_iter()
            .map(|value| &*value.leak())
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    )
}

fn family(value: wire::Family) -> ModelFamily {
    match value {
        wire::Family::Claude => ModelFamily::Claude,
        wire::Family::Generic | wire::Family::Unknown(_) => ModelFamily::Generic,
        wire::Family::Gemini => ModelFamily::Gemini,
        wire::Family::Glm => ModelFamily::Glm,
        wire::Family::Gpt => ModelFamily::Gpt,
        wire::Family::Synthetic => ModelFamily::Synthetic,
    }
}

fn fast(value: wire::FastPricing) -> FastPricing {
    FastPricing {
        input: value.input,
        output: value.output,
        cache_write: value.cache_write,
        cache_read: value.cache_read,
    }
}

fn pricing(value: wire::Pricing) -> ModelPricing {
    ModelPricing {
        input: value.input,
        output: value.output,
        cache_write: value.cache_write,
        cache_read: value.cache_read,
        fast: value.fast.map(fast),
        tiers: Cow::Borrowed(Box::leak(
            value
                .tiers
                .into_iter()
                .map(|tier| PricingTier {
                    above: tier.above,
                    input: tier.input,
                    output: tier.output,
                    cache_write: tier.cache_write,
                    cache_read: tier.cache_read,
                    fast: tier.fast.map(fast),
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        )),
    }
}

fn period(value: wire::Period) -> Period {
    Period {
        from: value.from,
        until: value.until,
    }
}

fn reasoning(value: wire::ReasoningOption) -> Option<StaticReasoningOption> {
    match value {
        wire::ReasoningOption::Toggle => Some(StaticReasoningOption::Toggle),
        wire::ReasoningOption::Effort { values } => {
            Some(StaticReasoningOption::Effort(strings(values)))
        }
        wire::ReasoningOption::BudgetTokens { min, max } => {
            Some(StaticReasoningOption::BudgetTokens { min, max })
        }
        wire::ReasoningOption::Unknown => None,
    }
}

fn model(value: wire::ModelEntry) -> ModelEntry {
    let unsupported = value
        .unsupported_reason(SUPPORTED_REQUIREMENTS)
        .map(|reason| &*reason.leak());
    ModelEntry {
        prefixes: strings(value.prefixes),
        small: value.small,
        default: value.default,
        vision: value.vision,
        family: family(value.family),
        pricing: pricing(value.pricing),
        max_output_tokens: value.max_output_tokens,
        context_window: value.context_window,
        window_excludes_output: value.window_excludes_output,
        api: value.api,
        plan_context_window: value.plan_context_window,
        supports_thinking: value.supports_thinking,
        unsupported,
        reasoning_options: value.reasoning_options.map(|options| {
            &*Box::leak(
                options
                    .into_iter()
                    .filter_map(reasoning)
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            )
        }),
    }
}

fn schedule(value: wire::PricingSchedule) -> Result<&'static PricingSchedule, String> {
    let windows = value
        .windows
        .into_iter()
        .map(|window| {
            PricingWindow::try_hours(window.start, window.end)
                .ok_or_else(|| INVALID_WINDOW.to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let schedule =
        PricingSchedule::try_new(Box::leak(windows.into_boxed_slice()), value.multiplier)
            .ok_or_else(|| INVALID_SCHEDULE.to_owned())?;
    Ok(Box::leak(Box::new(schedule)))
}

fn provider(value: wire::Provider) -> Result<ProviderManifest, String> {
    if let Some(reason) = value.unsupported_reason(SUPPORTED_REQUIREMENTS) {
        return Err(reason);
    }
    if value
        .models
        .iter()
        .any(|model| model.prefixes.is_empty() || model.prefixes.iter().any(String::is_empty))
    {
        return Err(INVALID_PREFIXES.to_owned());
    }
    let pricing_schedule = value.pricing_schedule.map(schedule).transpose()?;
    Ok(ProviderManifest {
        slug: value.slug.leak(),
        display_name: value.display_name.leak(),
        family: family(value.family),
        supports_thinking: value.supports_thinking,
        accepts_arbitrary_models: value.accepts_arbitrary_models,
        fallback_max_output: value.fallback_max_output,
        fallback_context_window: value.fallback_context_window,
        serves_fast_mode: value.serves_fast_mode,
        catalog_slug: value.catalog_slug.map(|slug| &*slug.leak()),
        pricing_schedule,
        models: Box::leak(
            value
                .models
                .into_iter()
                .map(model)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        ),
        generations: Box::leak(
            value
                .generations
                .into_iter()
                .map(|generation| ModelGeneration {
                    label: generation.label.leak(),
                    members: strings(generation.members),
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        ),
        price_periods: Box::leak(
            value
                .price_periods
                .into_iter()
                .map(|row| PricePeriod {
                    prefix: row.prefix.leak(),
                    during: period(row.during),
                    pricing: pricing(row.pricing),
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        ),
        availability: Box::leak(
            value
                .availability
                .into_iter()
                .map(|row| Availability {
                    prefix: row.prefix.leak(),
                    during: period(row.during),
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        ),
    })
}

pub(super) fn manifests(
    providers: Vec<wire::Provider>,
) -> (&'static [ProviderManifest], HashMap<String, &'static str>) {
    let mut manifests = Vec::new();
    let mut unsupported = HashMap::new();
    for value in providers {
        let slug = value.slug.clone();
        match provider(value) {
            Ok(manifest) => manifests.push(manifest),
            Err(reason) => {
                warn!(provider = slug, %reason, "Skipping unsupported model-file provider");
                unsupported.insert(slug, &*reason.leak());
            }
        }
    }
    (Box::leak(manifests.into_boxed_slice()), unsupported)
}

pub(super) fn decisions(models: Vec<wire::DecisionModel>) -> &'static [DecisionModelEntry] {
    Box::leak(
        models
            .into_iter()
            .filter_map(|value| {
                let protocol = match value.protocol.as_str() {
                    "typesafe" => DecisionProtocol::TypeSafe,
                    "openai" => DecisionProtocol::OpenAI,
                    protocol => {
                        warn!(
                            protocol,
                            "Skipping unsupported model-file decision protocol"
                        );
                        return None;
                    }
                };
                Some(DecisionModelEntry {
                    protocol,
                    prefixes: strings(value.prefixes),
                    pricing: pricing(value.pricing),
                    source: value.source.leak(),
                })
            })
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    )
}
