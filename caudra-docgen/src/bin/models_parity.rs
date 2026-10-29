//! `caudra-models-parity`: a deterministic JSON dump of what every built-in
//! provider resolves its models to, so two builds can be diffed line by line.
//!
//! The dump runs with isolated home, XDG and temp directories. Catalog lookups
//! use the current snapshot, without config or network access.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;

use caudra_agent::permissions::canonical_json_sha256;
use caudra_providers::accounts::{AccountCredential, AccountSecret, CredentialSource};
use caudra_providers::decision_pricing::{self, decision_cost};
use caudra_providers::manifest::{ManifestRegistry, ProviderManifest};
use caudra_providers::pricing::Period;
use caudra_providers::provider::ProviderKind;
use caudra_providers::{
    CacheKey, FastPricing, Message, Model, ModelEntry, ModelGeneration, ModelMarker, ModelPricing,
    ModelPurpose, ProviderData, ReasoningOption, ReasoningOptions, RequestOptions,
    StaticReasoningOption, ThinkingConfig, Timeouts, TokenUsage, catalog_providers, model_ids,
    model_meta,
};
use caudra_storage::accounts::{AccountId, AccountIdentity, AccountName};
use caudra_storage::auth::OAuthTokens;
use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use jiff::Timestamp;
use serde_json::{Map, Value, json};

const SANDBOX_FLAG: &str = "--sandbox";
const USAGE: &str = "usage: caudra-models-parity [--sandbox DIR] [--requests]";
const REQUESTS_FLAG: &str = "--requests";
const REQUESTS_ENV: &str = "CAUDRA_MODELS_PARITY_REQUESTS";
const PROBE_CREDENTIAL: &str = "parity-not-a-credential";
const PROBE_ACCOUNT: &str = "parity";
const PROBE_SYSTEM: &str = "Reply briefly.";
const PROBE_MESSAGE: &str = "Hello.";
const PROBE_CACHE_KEY: &str = "parity-cache";
const PROBE_SUFFIXES: [&str; 3] = ["", "-preview", "-2026-10-29"];
const REQUEST_PROVIDERS: [(&str, ProviderKind); 2] =
    [("openai", ProviderKind::OpenAi), ("zai", ProviderKind::Zai)];
const DEFAULT_SANDBOX: &str = "caudra-models-parity";
/// Proves a sandbox is ours to wipe before a rerun removes it.
const SANDBOX_MARKER: &str = ".caudra-models-parity";
/// Marks the isolated child process.
const CHILD_ENV: &str = "CAUDRA_MODELS_PARITY_CHILD";
const NAMESPACE_ENV: &str = "CAUDRA_NAMESPACE";
/// Fixed, so debug and release builds read the same cache directory.
const NAMESPACE: &str = "caudra-models-parity";
const SANDBOX_DIRS: [(&str, &str); 6] = [
    ("XDG_CACHE_HOME", "cache"),
    ("XDG_CONFIG_HOME", "config"),
    ("XDG_DATA_HOME", "data"),
    ("XDG_STATE_HOME", "state"),
    ("HOME", "home"),
    ("TMPDIR", "tmp"),
];
/// Passed through so a dead proxy can prove the run never dials out.
const PROXY_ENV: [&str; 8] = [
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
];
const DATES: [&str; 4] = [
    "2026-10-29T12:00:00Z",
    "2026-11-20T12:00:00Z",
    "2026-12-15T12:00:00Z",
    "2027-05-01T12:00:00Z",
];
const DAY_LEN: usize = "YYYY-MM-DD".len();
/// Below every tier, past a 272K tier, and past every tier.
const USAGES: [(&str, TokenUsage); 3] = [
    (
        "short",
        TokenUsage {
            input: 100_000,
            output: 10_000,
            cache_creation: 20_000,
            cache_read: 50_000,
        },
    ),
    (
        "prompt_300k",
        TokenUsage {
            input: 250_000,
            output: 10_000,
            cache_creation: 20_000,
            cache_read: 30_000,
        },
    ),
    (
        "prompt_1m2",
        TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            cache_creation: 100_000,
            cache_read: 100_000,
        },
    ),
];
/// Bills without the provider's wall-clock schedule, which the dump prints.
const PROBE_PROVIDER: &str = "caudra-models-parity-probe";
const UNPRICED: &str = "-";
const SAMPLED_FALLBACK_IDS: usize = 5;
const SHORT_HASH_LEN: usize = 16;

struct Options {
    sandbox: PathBuf,
    requests: bool,
}

fn parse_options(mut args: impl Iterator<Item = String>) -> Result<Options> {
    let mut sandbox = None;
    let mut requests = false;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            SANDBOX_FLAG => {
                sandbox = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| eyre!("{flag} needs a value\n{USAGE}"))?,
                ));
            }
            REQUESTS_FLAG => requests = true,
            _ => bail!("unknown flag {flag}\n{USAGE}"),
        }
    }
    Ok(Options {
        sandbox: sandbox.unwrap_or_else(|| env::temp_dir().join(DEFAULT_SANDBOX)),
        requests,
    })
}

fn reset_sandbox(sandbox: &Path) -> Result<()> {
    if sandbox.exists() {
        let ours = sandbox.join(SANDBOX_MARKER).exists();
        let empty = fs::read_dir(sandbox)?.next().is_none();
        if !ours && !empty {
            bail!(
                "{} exists and is not a parity sandbox; pass another {SANDBOX_FLAG}",
                sandbox.display()
            );
        }
        fs::remove_dir_all(sandbox)?;
    }
    fs::create_dir_all(sandbox)?;
    fs::write(sandbox.join(SANDBOX_MARKER), "")?;
    Ok(())
}

fn run_sandboxed() -> Result<()> {
    let options = parse_options(env::args().skip(1))?;
    reset_sandbox(&options.sandbox)?;
    let mut child = Command::new(env::current_exe()?);
    child
        .env_clear()
        .env(CHILD_ENV, "1")
        .env(NAMESPACE_ENV, NAMESPACE);
    if options.requests {
        child.env(REQUESTS_ENV, "1");
    }
    for (var, dir) in SANDBOX_DIRS {
        let path = options.sandbox.join(dir);
        fs::create_dir_all(&path)?;
        child.env(var, path);
    }
    for var in PROXY_ENV {
        if let Some(value) = env::var_os(var) {
            child.env(var, value);
        }
    }
    let status = child.status().wrap_err("starting the sandboxed dump")?;
    if !status.success() {
        bail!("the sandboxed dump failed: {status}");
    }
    Ok(())
}

fn dump() -> Result<String> {
    if env::var_os(REQUESTS_ENV).is_some() {
        return Ok(format!("{}\n", serde_json::to_string_pretty(&requests()?)?));
    }
    let catalog_providers = catalog_providers();
    if catalog_providers.is_empty() {
        bail!("the snapshot catalog is empty");
    }
    let dates = DATES
        .iter()
        .map(|date| Ok((&date[..DAY_LEN], date.parse::<Timestamp>()?)))
        .collect::<Result<Vec<_>>>()?;
    let document = json!({
        "dates": DATES,
        "usages": USAGES.iter().map(|(name, usage)| json!({
            "name": name,
            "input": usage.input,
            "output": usage.output,
            "cache_creation": usage.cache_creation,
            "cache_read": usage.cache_read,
        })).collect::<Vec<_>>(),
        "providers": ManifestRegistry::builtins()
            .iter()
            .map(|manifest| provider(manifest, &dates))
            .collect::<Vec<_>>(),
        "decision_models": decision_models(),
        "catalog": catalog(catalog_providers, &dates),
    });
    Ok(format!("{}\n", serde_json::to_string_pretty(&document)?))
}

fn requests() -> Result<Value> {
    let mut dump = Map::new();
    for (slug, kind) in REQUEST_PROVIDERS {
        let manifest = ManifestRegistry::get(slug).ok_or_else(|| eyre!("missing {slug}"))?;
        let mut ids: Vec<String> = manifest
            .models
            .iter()
            .flat_map(|entry| entry.prefixes)
            .flat_map(|id| PROBE_SUFFIXES.map(|suffix| format!("{id}{suffix}")))
            .chain(model_ids(manifest.catalog_slug.unwrap_or(slug)))
            .collect();
        if kind == ProviderKind::OpenAi {
            ids.push("gpt-future-codex".into());
        }
        ids.sort();
        ids.dedup();
        for oauth in [false, true] {
            if oauth && kind != ProviderKind::OpenAi {
                continue;
            }
            let secret = if oauth {
                AccountSecret::OAuth(OAuthTokens {
                    access: PROBE_CREDENTIAL.into(),
                    refresh: PROBE_CREDENTIAL.into(),
                    expires: u64::MAX,
                    account_id: None,
                })
            } else {
                AccountSecret::ApiKey {
                    key: PROBE_CREDENTIAL.into(),
                    host: None,
                }
            };
            let account = AccountCredential {
                id: AccountId::new(slug, AccountName::new(PROBE_ACCOUNT)?),
                secret,
                source: CredentialSource::Config,
                identity: AccountIdentity::default(),
            };
            let provider = kind.create(&account, Timeouts::default())?;
            let mut models = Map::new();
            for id in &ids {
                let mut model = Model::from_spec(&format!("{slug}/{id}"))?;
                provider.adjust_model(&mut model);
                let mut turns = Vec::new();
                for thinking in [
                    ThinkingConfig::Off,
                    ThinkingConfig::Adaptive,
                    ThinkingConfig::Effort("high".into()),
                ] {
                    let wire = provider.wire_request(
                        &model,
                        &[Message::user(PROBE_MESSAGE.into())],
                        PROBE_SYSTEM,
                        &Value::Null,
                        &RequestOptions {
                            thinking,
                            ..Default::default()
                        },
                        Some(&CacheKey::task(None, PROBE_CACHE_KEY)),
                    )?;
                    turns.push(json!({ "url": wire.url, "body": wire.body }));
                }
                models.insert(id.clone(), json!({
                    "context_window": model.context_window,
                    "window_excludes_output": model.window_excludes_output,
                    "supports_thinking": model.supports_thinking(),
                    "reasoning_transport": format!("{:?}", provider.reasoning_transport(&model)),
                    "turns": turns,
                }));
            }
            let listing = if oauth {
                Some(
                    smol::block_on(provider.list_models())?
                        .into_iter()
                        .map(|model| model.id)
                        .collect::<Vec<_>>(),
                )
            } else {
                None
            };
            let auth = if oauth { "oauth" } else { "api_key" };
            dump.insert(
                format!("{slug}/{auth}"),
                json!({ "models": models, "listing": listing }),
            );
        }
    }
    Ok(Value::Object(dump))
}

fn provider(manifest: &ProviderManifest, dates: &[(&str, Timestamp)]) -> Value {
    json!({
        "slug": manifest.slug,
        "display_name": manifest.display_name,
        "family": format!("{:?}", manifest.family),
        "supports_thinking": manifest.supports_thinking,
        "accepts_arbitrary_models": manifest.accepts_arbitrary_models,
        "fallback_max_output": manifest.fallback_max_output,
        "fallback_context_window": manifest.fallback_context_window,
        "serves_fast_mode": manifest.serves_fast_mode,
        "pricing_schedule": manifest.pricing_schedule.map(ToString::to_string),
        "catalog_slug": manifest.catalog_slug,
        "generations": manifest.generations.iter().map(|generation| json!({
            "label": generation.label,
            "members": generation.members,
        })).collect::<Vec<_>>(),
        "price_periods": manifest.price_periods.iter().map(|row| json!({
            "prefix": row.prefix,
            "during": period(&row.during),
            "pricing": pricing(&row.pricing),
        })).collect::<Vec<_>>(),
        "availability": manifest.availability.iter().map(|row| json!({
            "prefix": row.prefix,
            "during": period(&row.during),
        })).collect::<Vec<_>>(),
        "entries": manifest.models.iter().map(entry).collect::<Vec<_>>(),
        "listing": dates
            .iter()
            .map(|(day, at)| ((*day).to_owned(), listing(manifest, *at)))
            .collect::<Map<_, _>>(),
        "models": manifest
            .models
            .iter()
            .flat_map(|entry| entry.prefixes)
            .map(|prefix| ((*prefix).to_owned(), resolved(manifest.slug, prefix, dates)))
            .collect::<Map<_, _>>(),
        "catalog_fallback": catalog_fallback(manifest, dates),
    })
}

fn period(period: &Period) -> Value {
    json!({
        "from": period.from.map(|day| day.to_string()),
        "until": period.until.map(|day| day.to_string()),
    })
}

fn rates(input: f64, output: f64, cache_write: f64, cache_read: f64) -> Value {
    json!({
        "input": input,
        "output": output,
        "cache_write": cache_write,
        "cache_read": cache_read,
    })
}

fn pricing(pricing: &ModelPricing) -> Value {
    let fast =
        |fast: &FastPricing| rates(fast.input, fast.output, fast.cache_write, fast.cache_read);
    let mut value = rates(
        pricing.input,
        pricing.output,
        pricing.cache_write,
        pricing.cache_read,
    );
    value["fast"] = pricing.fast.as_ref().map_or(Value::Null, fast);
    value["tiers"] = pricing
        .tiers
        .iter()
        .map(|tier| {
            let mut value = rates(tier.input, tier.output, tier.cache_write, tier.cache_read);
            value["above"] = tier.above.into();
            value["fast"] = tier.fast.as_ref().map_or(Value::Null, fast);
            value
        })
        .collect();
    value
}

fn static_reasoning(options: &[StaticReasoningOption]) -> ReasoningOptions {
    ReasoningOptions::new(
        options
            .iter()
            .map(|option| match option {
                StaticReasoningOption::Toggle => ReasoningOption::Toggle,
                StaticReasoningOption::Effort(values) => ReasoningOption::Effort {
                    values: values.iter().map(|value| (*value).to_owned()).collect(),
                },
                StaticReasoningOption::BudgetTokens { min, max } => ReasoningOption::BudgetTokens {
                    min: *min,
                    max: *max,
                },
            })
            .collect(),
    )
}

fn entry(entry: &ModelEntry) -> Value {
    json!({
        "prefixes": entry.prefixes,
        "small": entry.small,
        "default": entry.default,
        "vision": entry.vision,
        "family": format!("{:?}", entry.family),
        "pricing": pricing(&entry.pricing),
        "max_output_tokens": entry.max_output_tokens,
        "context_window": entry.context_window,
        "reasoning_options": entry.reasoning_options.map(static_reasoning),
    })
}

fn first_prefix(entry: &ModelEntry) -> Option<&'static str> {
    entry.prefixes.first().copied()
}

/// `ManifestRegistry::prefixes_for_purpose` at `at`: the anchor's line first,
/// then the lane default, each group in table order.
fn candidates(
    listed: &[&'static ModelEntry],
    purpose: ModelPurpose,
    line: Option<&ModelGeneration>,
) -> Vec<&'static str> {
    let mut entries: Vec<_> = listed
        .iter()
        .filter(|entry| entry.class() == purpose)
        .collect();
    entries.sort_by_key(|entry| {
        (
            !line.is_some_and(|line| line.contains(entry)),
            !entry.default,
        )
    });
    entries
        .iter()
        .flat_map(|entry| entry.prefixes)
        .copied()
        .collect()
}

fn listing(manifest: &ProviderManifest, at: Timestamp) -> Value {
    let listed: Vec<_> = manifest.listed_models_at(at).collect();
    let lanes = ModelPurpose::CLASSES
        .iter()
        .map(|&purpose| {
            let lane = json!({
                "default": listed
                    .iter()
                    .find(|entry| entry.default && entry.class() == purpose)
                    .and_then(|entry| first_prefix(entry)),
                "candidates": candidates(&listed, purpose, None),
                "candidates_by_line": manifest
                    .generations
                    .iter()
                    .map(|line| (line.label.to_owned(), json!(candidates(&listed, purpose, Some(line)))))
                    .collect::<Map<_, _>>(),
            });
            (purpose.as_str().to_owned(), lane)
        })
        .collect::<Map<_, _>>();
    json!({
        "listed": listed.iter().filter_map(|entry| first_prefix(entry)).collect::<Vec<_>>(),
        "lanes": lanes,
    })
}

fn resolved(slug: &str, model_id: &str, dates: &[(&str, Timestamp)]) -> Value {
    match Model::from_spec(&format!("{slug}/{model_id}")) {
        Ok(model) => model_json(&model, dates),
        Err(error) => json!({ "error": error.to_string() }),
    }
}

fn model_json(model: &Model, dates: &[(&str, Timestamp)]) -> Value {
    let (provider, id) = (&*model.provider, model.id.as_str());
    json!({
        "id": id,
        "provider": provider,
        "family": format!("{:?}", model.family),
        "pricing": pricing(&model.pricing),
        "dated_rates": model.dated_rates.map(|dated| json!({
            "during": period(&dated.period.during),
            "pricing": pricing(&dated.period.pricing),
            "otherwise": pricing(dated.otherwise),
        })),
        "max_output_tokens": model.max_output_tokens,
        "context_window": model.context_window,
        "window_excludes_output": model.window_excludes_output,
        "reasoning_options": model.reasoning_options(),
        "supports_thinking": model.supports_thinking(),
        "requires_thinking": model.requires_thinking(),
        "supports_vision": model.supports_vision(),
        "supports_fast": model.supports_fast(),
        "supports_tool_examples": model.supports_tool_examples(),
        "supports_cache_breakpoints": model.supports_cache_breakpoints(),
        "prefers_apply_patch": model.prefers_apply_patch(),
        "discovered_free": model.discovered_free,
        "is_free": model.is_free(),
        "billing": format!("{:?}", model.billing),
        "marker": Model::marker_of(provider, id).map(ModelMarker::label),
        "class": Model::class_of(provider, id).map(ModelPurpose::as_str),
        "cost": dates
            .iter()
            .map(|(day, at)| ((*day).to_owned(), Value::from(costs(model, *at))))
            .collect::<Map<_, _>>(),
    })
}

/// Every usage standard then fast, billed at the rates in force at `at`, on
/// one line so a changed rate is one changed line.
fn costs(model: &Model, at: Timestamp) -> String {
    let pricing = model.dated_rates.map_or(&model.pricing, |dated| {
        if dated.period.during.contains(at) {
            &dated.period.pricing
        } else {
            dated.otherwise
        }
    });
    let probe = Model {
        provider: Arc::from(PROBE_PROVIDER),
        dated_rates: None,
        pricing: pricing.clone(),
        ..model.clone()
    };
    [false, true]
        .into_iter()
        .flat_map(|fast| USAGES.iter().map(move |(_, usage)| (usage, fast)))
        .map(|(usage, fast)| {
            probe
                .billed_cost(usage, fast)
                .map_or_else(|| UNPRICED.to_owned(), |cost| cost.to_string())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

macro_rules! catalog_meta {
    ($meta:expr) => {{
        let meta = $meta;
        json!({
            "context": meta.context,
            "context_excludes_output": meta.context_excludes_output,
            "output": meta.output,
            "input_price": meta.input_price,
            "output_price": meta.output_price,
            "cache_read": meta.cache_read,
            "cache_write": meta.cache_write,
            "supports_thinking": meta.supports_thinking,
            "supports_vision": meta.supports_vision,
            "reasoning_options": meta.reasoning_options,
            "pricing_tiers": meta.pricing_tiers.iter().map(|tier| json!({
                "above": tier.above,
                "input": tier.input,
                "output": tier.output,
                "cache_write": tier.cache_write,
                "cache_read": tier.cache_read,
            })).collect::<Vec<_>>(),
        })
    }};
}

fn short_hash(value: &Value) -> String {
    canonical_json_sha256(value)[..SHORT_HASH_LEN].to_owned()
}

/// What a built-in borrows from its own catalog entry: metadata for every id
/// models.dev lists, and the whole resolved model for ids no table prefix
/// covers, which the catalog alone describes.
fn catalog_fallback(manifest: &ProviderManifest, dates: &[(&str, Timestamp)]) -> Value {
    let Some(catalog_slug) = manifest.catalog_slug else {
        return Value::Null;
    };
    let mut ids = model_ids(catalog_slug);
    ids.sort_unstable();
    let metas: Map<_, _> = ids
        .iter()
        .filter_map(|id| {
            model_meta(catalog_slug, id).map(|meta| (id.to_owned(), catalog_meta!(meta)))
        })
        .collect();
    let uncovered: Map<_, _> = ids
        .iter()
        .filter(|id| ManifestRegistry::facts_for_model(manifest.slug, id).is_none())
        .map(|id| (id.to_owned(), resolved(manifest.slug, id, dates)))
        .collect();
    json!({
        "ids": ids.len(),
        "with_meta": metas.len(),
        "meta_sha256": canonical_json_sha256(&Value::Object(metas)),
        "uncovered": uncovered
            .iter()
            .map(|(id, model)| (id.clone(), Value::from(short_hash(model))))
            .collect::<Map<_, _>>(),
        "uncovered_sample": uncovered
            .into_iter()
            .take(SAMPLED_FALLBACK_IDS)
            .collect::<Map<_, _>>(),
    })
}

fn decision_models() -> Vec<Value> {
    decision_pricing::entries()
        .iter()
        .map(|entry| {
            json!({
                "protocol": entry.protocol.as_str(),
                "prefixes": entry.prefixes,
                "pricing": pricing(&entry.pricing),
                "source": entry.source,
                "cost": entry
                    .prefixes
                    .iter()
                    .map(|prefix| {
                        let costs = USAGES
                            .iter()
                            .map(|(_, usage)| decision_cost(&entry.protocol, &[*prefix], usage))
                            .collect::<Vec<_>>();
                        ((*prefix).to_owned(), json!(costs))
                    })
                    .collect::<Map<_, _>>(),
            })
        })
        .collect()
}

fn catalog(mut providers: Vec<ProviderData>, dates: &[(&str, Timestamp)]) -> Value {
    providers.sort_by(|a, b| a.slug.cmp(&b.slug));
    let providers = providers
        .iter()
        .map(|provider| {
            let mut ids: Vec<&str> = provider.models.keys().map(String::as_str).collect();
            ids.sort_unstable();
            let metas: Map<_, _> = ids
                .iter()
                .map(|id| ((*id).to_owned(), catalog_meta!(&provider.models[*id])))
                .collect();
            let models: Map<_, _> = ids
                .iter()
                .map(|id| ((*id).to_owned(), resolved(&provider.slug, id, dates)))
                .collect();
            json!({
                "slug": provider.slug,
                "display_name": provider.display_name,
                "env_keys": provider.env_keys,
                "base_url": provider.base_url,
                "npm": provider.npm,
                "api_format": format!("{:?}", provider.api_format),
                "models": ids.len(),
                "meta_sha256": canonical_json_sha256(&Value::Object(metas)),
                "resolved_sha256": canonical_json_sha256(&Value::Object(models)),
            })
        })
        .collect::<Vec<_>>();
    json!({ "providers": providers })
}

fn main() -> ExitCode {
    if env::args().any(|arg| arg == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let result = match env::var_os(CHILD_ENV) {
        Some(_) => dump().map(|document| print!("{document}")),
        None => run_sandboxed(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("caudra-models-parity: {error:#}");
            ExitCode::FAILURE
        }
    }
}
