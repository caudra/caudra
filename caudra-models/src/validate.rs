use std::collections::HashSet;

use thiserror::Error;
use url::{Host, Url};

use crate::{
    CatalogProvider, FORMAT, FastPricing, MAX_CATALOG_MODELS, MAX_PROVIDERS, MAX_TOKENS, Period,
    Pricing, Provider, Snapshot,
};

const FORMAT_MESSAGE: &str = "unsupported model-file format; expected 1";
const SLUG_MESSAGE: &str = "provider slug must be unique";
const PREFIX_MESSAGE: &str = "model prefix must be unique within its provider";
const EMPTY_PREFIXES_MESSAGE: &str = "model entry must have at least one prefix";
const EMPTY_PREFIX_MESSAGE: &str = "model prefix must not be empty";
const DEFAULT_MESSAGE: &str = "at most one undated default is allowed per generation and size lane";
const PERIOD_MESSAGE: &str = "period from must be before until";
const TIER_MESSAGE: &str = "tier thresholds must be strictly ascending";
const PRICE_MESSAGE: &str = "price must be finite and non-negative";
const TOKEN_MESSAGE: &str = "token limit must not exceed 100000000";
const REFERENCE_MESSAGE: &str = "prefix must name an entry prefix";
const GENERATION_MESSAGE: &str = "generation members must not be empty";
const URL_MESSAGE: &str = "catalog API must use HTTPS or HTTP to loopback";
const ENV_MESSAGE: &str = "environment name must match ^[A-Z0-9][A-Z0-9_]*$";
const PROVIDER_COUNT_MESSAGE: &str = "provider count must not exceed 1000";
const CATALOG_COUNT_MESSAGE: &str = "catalog model count must not exceed 50000";
const MODELS_FROM_MESSAGE: &str = "models_from must be expanded before publishing";

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{pointer}: {message}")]
pub struct Issue {
    pub pointer: String,
    pub message: String,
}

pub fn validate(snapshot: &Snapshot) -> Result<(), Vec<Issue>> {
    let mut issues = Vec::new();
    if snapshot.format != FORMAT {
        issue(&mut issues, "/format", FORMAT_MESSAGE);
    }
    if snapshot.payload.providers.len() > MAX_PROVIDERS {
        issue(&mut issues, "/providers", PROVIDER_COUNT_MESSAGE);
    }
    let mut slugs = HashSet::new();
    for (index, provider) in snapshot.payload.providers.iter().enumerate() {
        let path = format!("/providers/{index}");
        if !slugs.insert(&provider.slug) {
            issue(&mut issues, &format!("{path}/slug"), SLUG_MESSAGE);
        }
        validate_provider(provider, &path, &mut issues);
    }
    for (index, model) in snapshot.payload.decision_models.iter().enumerate() {
        validate_pricing(
            &model.pricing,
            &format!("/decision_models/{index}/pricing"),
            &mut issues,
        );
    }
    let count: usize = snapshot
        .payload
        .catalog
        .values()
        .map(|provider| provider.models.len())
        .sum();
    if count > MAX_CATALOG_MODELS {
        issue(&mut issues, "/catalog", CATALOG_COUNT_MESSAGE);
    }
    for (slug, provider) in &snapshot.payload.catalog {
        validate_catalog(
            provider,
            &format!("/catalog/{}", pointer_token(slug)),
            &mut issues,
        );
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues)
    }
}

fn validate_provider(provider: &Provider, path: &str, issues: &mut Vec<Issue>) {
    if provider.models_from.is_some() {
        issue(issues, &format!("{path}/models_from"), MODELS_FROM_MESSAGE);
    }
    tokens(
        provider.fallback_context_window,
        &format!("{path}/fallback_context_window"),
        issues,
    );
    if let Some(value) = provider.fallback_max_output {
        tokens(value, &format!("{path}/fallback_max_output"), issues);
    }
    if let Some(schedule) = &provider.pricing_schedule {
        price(
            schedule.multiplier,
            &format!("{path}/pricing_schedule/multiplier"),
            issues,
        );
    }
    let mut prefixes = HashSet::new();
    let mut defaults = HashSet::new();
    for (index, model) in provider.models.iter().enumerate() {
        let model_path = format!("{path}/models/{index}");
        if model.prefixes.is_empty() {
            issue(
                issues,
                &format!("{model_path}/prefixes"),
                EMPTY_PREFIXES_MESSAGE,
            );
        }
        for (index, prefix) in model.prefixes.iter().enumerate() {
            if prefix.is_empty() {
                issue(
                    issues,
                    &format!("{model_path}/prefixes/{index}"),
                    EMPTY_PREFIX_MESSAGE,
                );
            }
            if !prefixes.insert(prefix.as_str()) {
                issue(
                    issues,
                    &format!("{model_path}/prefixes/{index}"),
                    PREFIX_MESSAGE,
                );
            }
        }
        let dated = provider
            .availability
            .iter()
            .any(|availability| model.prefixes.contains(&availability.prefix));
        if model.default && !dated {
            let generation = provider
                .generations
                .iter()
                .enumerate()
                .flat_map(|(index, generation)| {
                    generation.members.iter().map(move |member| (index, member))
                })
                .filter(|(_, member)| {
                    model
                        .prefixes
                        .iter()
                        .any(|prefix| prefix.starts_with(member.as_str()))
                })
                .max_by_key(|(_, member)| member.len())
                .map(|(index, _)| index);
            if !defaults.insert((generation, model.small)) {
                issue(issues, &format!("{model_path}/default"), DEFAULT_MESSAGE);
            }
        }
        tokens(
            model.context_window,
            &format!("{model_path}/context_window"),
            issues,
        );
        if let Some(value) = model.max_output_tokens {
            tokens(value, &format!("{model_path}/max_output_tokens"), issues);
        }
        if let Some(value) = model.plan_context_window {
            tokens(value, &format!("{model_path}/plan_context_window"), issues);
        }
        validate_pricing(&model.pricing, &format!("{model_path}/pricing"), issues);
    }
    for (index, period) in provider.price_periods.iter().enumerate() {
        let period_path = format!("{path}/price_periods/{index}");
        if !prefixes.contains(period.prefix.as_str()) {
            issue(issues, &format!("{period_path}/prefix"), REFERENCE_MESSAGE);
        }
        validate_period(&period.during, &format!("{period_path}/during"), issues);
        validate_pricing(&period.pricing, &format!("{period_path}/pricing"), issues);
    }
    for (index, availability) in provider.availability.iter().enumerate() {
        let availability_path = format!("{path}/availability/{index}");
        if !prefixes.contains(availability.prefix.as_str()) {
            issue(
                issues,
                &format!("{availability_path}/prefix"),
                REFERENCE_MESSAGE,
            );
        }
        validate_period(
            &availability.during,
            &format!("{availability_path}/during"),
            issues,
        );
    }
    for (index, generation) in provider.generations.iter().enumerate() {
        if generation.members.is_empty() {
            issue(
                issues,
                &format!("{path}/generations/{index}/members"),
                GENERATION_MESSAGE,
            );
        }
    }
}

fn validate_period(period: &Period, path: &str, issues: &mut Vec<Issue>) {
    if let (Some(from), Some(until)) = (period.from, period.until)
        && from >= until
    {
        issue(issues, path, PERIOD_MESSAGE);
    }
}

fn validate_pricing(pricing: &Pricing, path: &str, issues: &mut Vec<Issue>) {
    prices(
        [
            Some(pricing.input),
            Some(pricing.output),
            Some(pricing.cache_write),
            Some(pricing.cache_read),
        ],
        path,
        issues,
    );
    if let Some(fast) = &pricing.fast {
        validate_fast(fast, &format!("{path}/fast"), issues);
    }
    let mut previous = None;
    for (index, tier) in pricing.tiers.iter().enumerate() {
        let tier_path = format!("{path}/tiers/{index}");
        if previous.is_some_and(|previous| previous >= tier.above) {
            issue(issues, &format!("{tier_path}/above"), TIER_MESSAGE);
        }
        previous = Some(tier.above);
        prices(
            [
                Some(tier.input),
                Some(tier.output),
                Some(tier.cache_write),
                Some(tier.cache_read),
            ],
            &tier_path,
            issues,
        );
        if let Some(fast) = &tier.fast {
            validate_fast(fast, &format!("{tier_path}/fast"), issues);
        }
    }
}

fn validate_fast(pricing: &FastPricing, path: &str, issues: &mut Vec<Issue>) {
    prices(
        [
            Some(pricing.input),
            Some(pricing.output),
            Some(pricing.cache_write),
            Some(pricing.cache_read),
        ],
        path,
        issues,
    );
}

fn validate_catalog(provider: &CatalogProvider, path: &str, issues: &mut Vec<Issue>) {
    if let Some(api) = &provider.api
        && !allowed_api(api)
    {
        issue(issues, &format!("{path}/api"), URL_MESSAGE);
    }
    for (index, env) in provider.env.iter().enumerate() {
        let mut bytes = env.bytes();
        let alphanumeric = |byte: u8| byte.is_ascii_uppercase() || byte.is_ascii_digit();
        if !bytes.next().is_some_and(alphanumeric)
            || !bytes.all(|byte| alphanumeric(byte) || byte == b'_')
        {
            issue(issues, &format!("{path}/env/{index}"), ENV_MESSAGE);
        }
    }
    for (id, model) in &provider.models {
        let model_path = format!("{path}/models/{}", pointer_token(id));
        if let Some(limit) = &model.limit {
            for (name, value) in [
                ("context", limit.context),
                ("input", limit.input),
                ("output", limit.output),
            ] {
                if let Some(value) = value {
                    tokens(value, &format!("{model_path}/limit/{name}"), issues);
                }
            }
        }
        if let Some(cost) = &model.cost {
            let cost_path = format!("{model_path}/cost");
            prices(
                [cost.input, cost.output, cost.cache_write, cost.cache_read],
                &cost_path,
                issues,
            );
            let mut previous = None;
            for (index, tier) in cost.tiers.iter().enumerate() {
                let tier_path = format!("{cost_path}/tiers/{index}");
                if previous.is_some_and(|previous| previous >= tier.tier.size) {
                    issue(issues, &format!("{tier_path}/tier/size"), TIER_MESSAGE);
                }
                previous = Some(tier.tier.size);
                prices(
                    [tier.input, tier.output, tier.cache_write, tier.cache_read],
                    &tier_path,
                    issues,
                );
            }
        }
    }
}

fn allowed_api(api: &str) -> bool {
    let Ok(url) = Url::parse(api) else {
        return false;
    };
    if api.starts_with("https://") && url.host().is_some() {
        return true;
    }
    api.starts_with("http://")
        && match url.host() {
            Some(Host::Domain("localhost")) => true,
            Some(Host::Ipv4(address)) => address.octets() == [127, 0, 0, 1],
            Some(Host::Ipv6(address)) => address.is_loopback(),
            _ => false,
        }
}

fn prices(values: [Option<f64>; 4], path: &str, issues: &mut Vec<Issue>) {
    for (name, value) in ["input", "output", "cache_write", "cache_read"]
        .into_iter()
        .zip(values)
    {
        if let Some(value) = value {
            price(value, &format!("{path}/{name}"), issues);
        }
    }
}

fn price(value: f64, path: &str, issues: &mut Vec<Issue>) {
    if !value.is_finite() || value < 0.0 {
        issue(issues, path, PRICE_MESSAGE);
    }
}

fn tokens(value: u32, path: &str, issues: &mut Vec<Issue>) {
    if value > MAX_TOKENS {
        issue(issues, path, TOKEN_MESSAGE);
    }
}

fn issue(issues: &mut Vec<Issue>, pointer: &str, message: &str) {
    issues.push(Issue {
        pointer: pointer.to_owned(),
        message: message.to_owned(),
    });
}

fn pointer_token(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::{
        CATALOG_COUNT_MESSAGE, DEFAULT_MESSAGE, EMPTY_PREFIX_MESSAGE, EMPTY_PREFIXES_MESSAGE,
        ENV_MESSAGE, FORMAT_MESSAGE, GENERATION_MESSAGE, Issue, MODELS_FROM_MESSAGE,
        PERIOD_MESSAGE, PREFIX_MESSAGE, PRICE_MESSAGE, PROVIDER_COUNT_MESSAGE, REFERENCE_MESSAGE,
        SLUG_MESSAGE, TIER_MESSAGE, TOKEN_MESSAGE, URL_MESSAGE, validate,
    };
    use crate::{
        Availability, CatalogModel, MAX_CATALOG_MODELS, MAX_PROVIDERS, MAX_TOKENS, Period,
        Snapshot, tests::fixture,
    };

    const PERIOD_POINTER: &str = "/providers/0/price_periods/0/during";
    const DEFAULT_POINTER: &str = "/providers/0/models/1/default";
    const PRICE_POINTER: &str = "/providers/0/models/0/pricing/input";
    const DISPLAY_MESSAGE: &str = "/format: unsupported model-file format; expected 1";

    #[test]
    fn plan_window_obeys_token_bounds() {
        let mut snapshot = fixture();
        snapshot.payload.providers[0].models[0].plan_context_window = Some(MAX_TOKENS + 1);
        assert_issue(
            &snapshot,
            "/providers/0/models/0/plan_context_window",
            TOKEN_MESSAGE,
        );
    }

    fn assert_issue(snapshot: &Snapshot, pointer: &str, message: &str) {
        assert_eq!(
            validate(snapshot).unwrap_err(),
            vec![Issue {
                pointer: pointer.to_owned(),
                message: message.to_owned(),
            }]
        );
    }

    #[test_case("/format", json!(2), "/format", FORMAT_MESSAGE; "format")]
    #[test_case("/providers/0/models_from", json!("other"), "/providers/0/models_from", MODELS_FROM_MESSAGE; "unexpanded_models_from")]
    #[test_case("/providers/0/price_periods/0/during/until", json!("2026-01-01"), PERIOD_POINTER, PERIOD_MESSAGE; "empty_period")]
    #[test_case("/providers/0/price_periods/0/during/until", json!("2025-01-01"), PERIOD_POINTER, PERIOD_MESSAGE; "reversed_period")]
    #[test_case("/providers/0/generations/0/members", json!([]), "/providers/0/generations/0/members", GENERATION_MESSAGE; "empty_generation")]
    #[test_case("/providers/0/price_periods/0/prefix", json!("missing"), "/providers/0/price_periods/0/prefix", REFERENCE_MESSAGE; "price_period_reference")]
    #[test_case("/providers/0/models/0/context_window", json!(MAX_TOKENS + 1), "/providers/0/models/0/context_window", TOKEN_MESSAGE; "model_context")]
    #[test_case("/providers/0/models/0/max_output_tokens", json!(MAX_TOKENS + 1), "/providers/0/models/0/max_output_tokens", TOKEN_MESSAGE; "model_output")]
    #[test_case("/providers/0/fallback_context_window", json!(MAX_TOKENS + 1), "/providers/0/fallback_context_window", TOKEN_MESSAGE; "fallback_context")]
    #[test_case("/providers/0/fallback_max_output", json!(MAX_TOKENS + 1), "/providers/0/fallback_max_output", TOKEN_MESSAGE; "fallback_output")]
    #[test_case("/catalog/catalog/models/model/limit/context", json!(MAX_TOKENS + 1), "/catalog/catalog/models/model/limit/context", TOKEN_MESSAGE; "catalog_context")]
    #[test_case("/catalog/catalog/models/model/limit/input", json!(MAX_TOKENS + 1), "/catalog/catalog/models/model/limit/input", TOKEN_MESSAGE; "catalog_input")]
    #[test_case("/catalog/catalog/models/model/limit/output", json!(MAX_TOKENS + 1), "/catalog/catalog/models/model/limit/output", TOKEN_MESSAGE; "catalog_output")]
    fn invalid_field_reports_exact_pointer(
        target: &str,
        value: Value,
        pointer: &str,
        message: &str,
    ) {
        let mut snapshot = serde_json::to_value(fixture()).unwrap();
        if target == "/providers/0/models_from" {
            snapshot["providers"][0]["models_from"] = value;
        } else {
            *snapshot.pointer_mut(target).unwrap() = value;
        }
        assert_issue(&serde_json::from_value(snapshot).unwrap(), pointer, message);
    }

    #[test_case("/providers/0/models/0/pricing/input"; "input")]
    #[test_case("/providers/0/models/0/pricing/output"; "output")]
    #[test_case("/providers/0/models/0/pricing/cache_write"; "cache_write")]
    #[test_case("/providers/0/models/0/pricing/cache_read"; "cache_read")]
    #[test_case("/providers/0/models/0/pricing/fast/input"; "fast")]
    #[test_case("/providers/0/models/0/pricing/tiers/0/input"; "tier")]
    #[test_case("/providers/0/models/0/pricing/tiers/0/fast/input"; "tier_fast")]
    #[test_case("/providers/0/pricing_schedule/multiplier"; "multiplier")]
    #[test_case("/providers/0/price_periods/0/pricing/input"; "period")]
    #[test_case("/decision_models/0/pricing/input"; "decision")]
    #[test_case("/catalog/catalog/models/model/cost/input"; "catalog")]
    #[test_case("/catalog/catalog/models/model/cost/tiers/0/input"; "catalog_tier")]
    fn negative_prices_are_rejected(pointer: &str) {
        let mut snapshot = serde_json::to_value(fixture()).unwrap();
        *snapshot.pointer_mut(pointer).unwrap() = json!(-1.0);
        assert_issue(
            &serde_json::from_value(snapshot).unwrap(),
            pointer,
            PRICE_MESSAGE,
        );
    }

    #[test_case(f64::NAN; "nan")]
    #[test_case(f64::INFINITY; "positive_infinity")]
    #[test_case(f64::NEG_INFINITY; "negative_infinity")]
    fn non_finite_prices_are_rejected(value: f64) {
        let mut snapshot = fixture();
        snapshot.payload.providers[0].models[0].pricing.input = value;
        assert_issue(&snapshot, PRICE_POINTER, PRICE_MESSAGE);
    }

    #[test]
    fn provider_slugs_must_be_unique() {
        let mut snapshot = fixture();
        snapshot
            .payload
            .providers
            .push(snapshot.payload.providers[0].clone());
        assert_issue(&snapshot, "/providers/1/slug", SLUG_MESSAGE);
    }

    #[test]
    fn prefixes_must_be_unique_across_entries() {
        let mut snapshot = fixture();
        let mut model = snapshot.payload.providers[0].models[0].clone();
        model.default = false;
        model.prefixes.truncate(1);
        snapshot.payload.providers[0].models.push(model);
        assert_issue(
            &snapshot,
            "/providers/0/models/1/prefixes/0",
            PREFIX_MESSAGE,
        );
    }

    #[test_case(&[], "/providers/0/models/0/prefixes", EMPTY_PREFIXES_MESSAGE; "no_prefixes")]
    #[test_case(&[""], "/providers/0/models/0/prefixes/0", EMPTY_PREFIX_MESSAGE; "empty_prefix")]
    fn model_prefixes_must_be_nonempty(prefixes: &[&str], pointer: &str, message: &str) {
        let mut snapshot = fixture();
        let provider = &mut snapshot.payload.providers[0];
        provider.models[0].prefixes = prefixes.iter().map(|prefix| (*prefix).to_owned()).collect();
        provider.price_periods.clear();
        assert_issue(&snapshot, pointer, message);
    }

    #[test_case(false; "normal_lane")]
    #[test_case(true; "small_lane")]
    fn undated_defaults_must_be_unique_per_lane(small: bool) {
        let mut snapshot = fixture();
        let provider = &mut snapshot.payload.providers[0];
        provider.models[0].small = small;
        let mut successor = provider.models[0].clone();
        successor.prefixes = vec!["successor".to_owned()];
        provider.generations[0].members.push("successor".to_owned());
        provider.models.push(successor);
        assert_issue(&snapshot, DEFAULT_POINTER, DEFAULT_MESSAGE);
    }

    #[test_case(false; "separate_lanes")]
    #[test_case(true; "retiring_successor")]
    fn valid_default_patterns_are_allowed(retiring: bool) {
        let mut snapshot = fixture();
        let provider = &mut snapshot.payload.providers[0];
        let mut successor = provider.models[0].clone();
        successor.prefixes = vec!["successor".to_owned()];
        if retiring {
            provider.availability.push(Availability {
                prefix: "model-a".to_owned(),
                note: None,
                source: None,
                during: Period {
                    from: None,
                    until: Some("2026-12-01".parse().unwrap()),
                },
            });
        } else {
            successor.small = true;
        }
        provider.models.push(successor);
        assert_eq!(validate(&snapshot), Ok(()));
    }

    #[test]
    fn distinct_generations_can_each_declare_a_default() {
        let mut snapshot = fixture();
        let provider = &mut snapshot.payload.providers[0];
        let mut entry = provider.models[0].clone();
        entry.prefixes = vec!["next-model".to_owned()];
        provider.models.push(entry);
        provider.generations.push(crate::Generation {
            label: "Next".to_owned(),
            members: vec!["next-".to_owned()],
        });
        assert_eq!(validate(&snapshot), Ok(()));
    }

    #[test_case(false; "builtin_tiers")]
    #[test_case(true; "catalog_tiers")]
    fn tier_thresholds_must_ascend(catalog: bool) {
        let mut snapshot = fixture();
        let pointer = if catalog {
            let cost = snapshot
                .payload
                .catalog
                .get_mut("catalog")
                .unwrap()
                .models
                .get_mut("model")
                .unwrap()
                .cost
                .as_mut()
                .unwrap();
            cost.tiers.push(cost.tiers[0].clone());
            "/catalog/catalog/models/model/cost/tiers/1/tier/size"
        } else {
            let pricing = &mut snapshot.payload.providers[0].models[0].pricing;
            pricing.tiers.push(pricing.tiers[0].clone());
            "/providers/0/models/0/pricing/tiers/1/above"
        };
        assert_issue(&snapshot, pointer, TIER_MESSAGE);
    }

    #[test_case(false; "unknown_prefix")]
    #[test_case(true; "invalid_period")]
    fn availability_rules_are_checked(invalid_period: bool) {
        let mut snapshot = fixture();
        let (availability, pointer, message) = if invalid_period {
            let date = "2026-01-01".parse().unwrap();
            (
                Availability {
                    prefix: "model-a".to_owned(),
                    note: None,
                    source: None,
                    during: Period {
                        from: Some(date),
                        until: Some(date),
                    },
                },
                "/providers/0/availability/0/during",
                PERIOD_MESSAGE,
            )
        } else {
            (
                Availability {
                    prefix: "missing".to_owned(),
                    note: None,
                    source: None,
                    during: Period::default(),
                },
                "/providers/0/availability/0/prefix",
                REFERENCE_MESSAGE,
            )
        };
        snapshot.payload.providers[0]
            .availability
            .push(availability);
        assert_issue(&snapshot, pointer, message);
    }

    #[test_case("https://example.com/v1", true; "https")]
    #[test_case("http://127.0.0.1:8000/v1", true; "ipv4_loopback")]
    #[test_case("http://localhost/v1", true; "localhost")]
    #[test_case("http://[::1]:8000/v1", true; "ipv6_loopback")]
    #[test_case("http://example.com", false; "public_http")]
    #[test_case("ftp://localhost", false; "wrong_scheme")]
    #[test_case("https://", false; "missing_host")]
    #[test_case("http://localhost.example.com", false; "domain_suffix")]
    #[test_case("http://localhost@example.com", false; "userinfo_is_not_host")]
    #[test_case("http://[::2]", false; "ipv6_non_loopback")]
    fn catalog_urls_follow_transport_policy(api: &str, valid: bool) {
        let mut snapshot = fixture();
        snapshot.payload.catalog.get_mut("catalog").unwrap().api = Some(api.to_owned());
        if valid {
            assert_eq!(validate(&snapshot), Ok(()));
        } else {
            assert_issue(&snapshot, "/catalog/catalog/api", URL_MESSAGE);
        }
    }

    #[test_case("", false; "empty")]
    #[test_case("_KEY", false; "leading_underscore")]
    #[test_case("Api_KEY", false; "lowercase")]
    #[test_case("API-KEY", false; "hyphen")]
    #[test_case("API_KEY\n", false; "newline")]
    #[test_case("ÄPI_KEY", false; "non_ascii")]
    #[test_case("0_API_KEY", true; "leading_digit")]
    #[test_case("API_KEY", true; "uppercase")]
    fn catalog_env_names_follow_pattern(env: &str, valid: bool) {
        let mut snapshot = fixture();
        snapshot.payload.catalog.get_mut("catalog").unwrap().env = vec![env.to_owned()];
        if valid {
            assert_eq!(validate(&snapshot), Ok(()));
        } else {
            assert_issue(&snapshot, "/catalog/catalog/env/0", ENV_MESSAGE);
        }
    }

    #[test_case(false; "provider_count")]
    #[test_case(true; "catalog_model_count")]
    fn count_bounds_are_checked(catalog: bool) {
        let mut snapshot = fixture();
        if catalog {
            let models = &mut snapshot.payload.catalog.get_mut("catalog").unwrap().models;
            models.clear();
            models
                .extend((0..=MAX_CATALOG_MODELS).map(|i| (i.to_string(), CatalogModel::default())));
            assert_issue(&snapshot, "/catalog", CATALOG_COUNT_MESSAGE);
        } else {
            let provider = snapshot.payload.providers[0].clone();
            snapshot.payload.providers = (0..=MAX_PROVIDERS)
                .map(|i| {
                    let mut provider = provider.clone();
                    provider.slug = i.to_string();
                    provider
                })
                .collect();
            assert_issue(&snapshot, "/providers", PROVIDER_COUNT_MESSAGE);
        }
    }

    #[test]
    fn catalog_ids_are_json_pointer_escaped() {
        let mut snapshot = fixture();
        let mut provider = snapshot.payload.catalog.remove("catalog").unwrap();
        let mut model = provider.models.remove("model").unwrap();
        model.cost.as_mut().unwrap().input = Some(-1.0);
        provider.models.insert("a/b~c".to_owned(), model);
        snapshot
            .payload
            .catalog
            .insert("x/y~z".to_owned(), provider);
        assert_issue(
            &snapshot,
            "/catalog/x~1y~0z/models/a~1b~0c/cost/input",
            PRICE_MESSAGE,
        );
    }

    #[test]
    fn issue_display_includes_pointer_and_message() {
        let issue = Issue {
            pointer: "/format".to_owned(),
            message: FORMAT_MESSAGE.to_owned(),
        };
        assert_eq!(issue.to_string(), DISPLAY_MESSAGE);
    }
}
