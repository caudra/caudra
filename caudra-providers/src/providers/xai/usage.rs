use crate::{AgentError, ProviderUsage, UsageLimit, accounts::WindowKind};
use jiff::Timestamp;
use serde::Deserialize;
use std::time::Duration;

const FULL_PERCENT: f64 = 100.0;
const CENTS_PER_DOLLAR: u64 = 100;
const MILLIS_PER_SECOND: u64 = 1_000;
const WEEKLY_PERIOD: &str = "USAGE_PERIOD_TYPE_WEEKLY";
const MONTHLY_PERIOD: &str = "USAGE_PERIOD_TYPE_MONTHLY";
const INCLUDED_LABEL: &str = "Included credits";
const UNREPORTED_QUOTA: &str = "Personal included-credit usage is not reported for this account.";
const INVALID_TIMESTAMP: &str = "Invalid xAI billing period timestamp";

#[derive(Deserialize)]
struct BillingResponse {
    #[serde(deserialize_with = "Option::deserialize")]
    config: Option<BillingConfig>,
    #[serde(alias = "subscriptionTier")]
    subscription_tier: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillingConfig {
    credit_usage_percent: Option<f64>,
    current_period: Option<UsagePeriod>,
    monthly_limit: Option<Cent>,
    used: Option<Cent>,
    on_demand_cap: Option<Cent>,
    on_demand_used: Option<Cent>,
    prepaid_balance: Option<Cent>,
    billing_period_start: Option<String>,
    billing_period_end: Option<String>,
}

#[derive(Deserialize)]
struct UsagePeriod {
    #[serde(rename = "type")]
    kind: Option<String>,
    start: Option<String>,
    end: Option<String>,
}

#[derive(Deserialize)]
struct Cent {
    #[serde(default)]
    val: i64,
}

impl Cent {
    fn dollars(&self) -> String {
        let cents = self.val.unsigned_abs();
        let sign = if self.val < 0 { "-" } else { "" };
        format!(
            "{sign}${}.{:02}",
            cents / CENTS_PER_DOLLAR,
            cents % CENTS_PER_DOLLAR
        )
    }
}

fn timestamp(value: Option<&str>) -> Result<Option<u64>, AgentError> {
    value
        .map(|value| {
            value
                .parse::<Timestamp>()
                .ok()
                .and_then(|timestamp| u64::try_from(timestamp.as_millisecond()).ok())
                .ok_or_else(|| AgentError::Config {
                    message: INVALID_TIMESTAMP.into(),
                })
        })
        .transpose()
}

fn detail_row(label: &str, detail: String) -> UsageLimit {
    UsageLimit {
        label: label.into(),
        percentage: None,
        reset_at: None,
        detail: Some(detail),
        window: None,
        length_secs: None,
    }
}

fn included_limit(config: &BillingConfig) -> Result<UsageLimit, AgentError> {
    let percentage = config.credit_usage_percent.or_else(|| {
        let limit = config.monthly_limit.as_ref()?.val;
        let used = config.used.as_ref()?.val;
        (limit > 0).then(|| used as f64 / limit as f64 * FULL_PERCENT)
    });
    let Some(percentage) = percentage.filter(|value| value.is_finite()) else {
        return Ok(detail_row(INCLUDED_LABEL, UNREPORTED_QUOTA.into()));
    };
    let period = config.current_period.as_ref();
    let start = timestamp(
        period
            .and_then(|period| period.start.as_deref())
            .or(config.billing_period_start.as_deref()),
    )?;
    let reset_at = timestamp(
        period
            .and_then(|period| period.end.as_deref())
            .or(config.billing_period_end.as_deref()),
    )?;
    let length_secs = start
        .zip(reset_at)
        .and_then(|(start, end)| end.checked_sub(start))
        .map(|millis| millis / MILLIS_PER_SECOND)
        .filter(|seconds| *seconds > 0);
    let window = match period.and_then(|period| period.kind.as_deref()) {
        Some(WEEKLY_PERIOD) => WindowKind::Weekly,
        Some(MONTHLY_PERIOD) => WindowKind::Monthly,
        _ => length_secs.map_or_else(
            || {
                if config.credit_usage_percent.is_none() {
                    WindowKind::Monthly
                } else {
                    WindowKind::Other(INCLUDED_LABEL.into())
                }
            },
            |seconds| WindowKind::from_length(Duration::from_secs(seconds)),
        ),
    };
    let label = match window {
        WindowKind::Weekly => "Weekly included credits",
        WindowKind::Monthly => "Monthly included credits",
        _ => INCLUDED_LABEL,
    };
    Ok(UsageLimit {
        label: label.into(),
        percentage: Some(percentage.round().clamp(0.0, FULL_PERCENT) as u32),
        reset_at,
        detail: None,
        window: Some(window),
        length_secs,
    })
}

pub(super) fn parse_usage(response: &str) -> Result<ProviderUsage, AgentError> {
    let response: BillingResponse = serde_json::from_str(response)?;
    let mut usage = ProviderUsage {
        plan: response.subscription_tier,
        limits: Vec::new(),
    };
    let Some(config) = response.config else {
        usage
            .limits
            .push(detail_row(INCLUDED_LABEL, UNREPORTED_QUOTA.into()));
        return Ok(usage);
    };
    usage.limits.push(included_limit(&config)?);
    if let Some(balance) = config.prepaid_balance {
        usage.limits.push(detail_row(
            "Prepaid balance",
            format!("{} remaining", balance.dollars()),
        ));
    }
    let on_demand = match (config.on_demand_used, config.on_demand_cap) {
        (Some(used), Some(cap)) => Some(format!("{} used / {} cap", used.dollars(), cap.dollars())),
        (Some(used), None) => Some(format!("{} used", used.dollars())),
        (None, Some(cap)) => Some(format!("{} cap", cap.dollars())),
        (None, None) => None,
    };
    if let Some(detail) = on_demand {
        usage.limits.push(detail_row("On-demand credits", detail));
    }
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::{INVALID_TIMESTAMP, UNREPORTED_QUOTA, parse_usage};
    use crate::accounts::WindowKind;
    use serde_json::{Value, json};
    use test_case::test_case;

    const START: &str = "2026-10-27T19:07:54.438+00:00";
    const END: &str = "2026-11-03T19:07:54.438+00:00";
    const RESET_AT: u64 = 1_793_732_874_438;
    const WEEK_SECONDS: u64 = 604_800;
    const LIVE_RESPONSE: &str = r#"{"config":{"creditUsagePercent":76.0,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","start":"2026-10-27T19:07:54.438+00:00","end":"2026-11-03T19:07:54.438+00:00"},"prepaidBalance":{"val":0},"isUnifiedBillingUser":true,"billingPeriodStart":"2026-10-27T19:07:54.438+00:00","billingPeriodEnd":"2026-11-03T19:07:54.438+00:00","productUsage":[{"product":"GrokBuild","usagePercent":76.0}]}}"#;

    #[test]
    fn live_weekly_credits_normalize_only_the_included_pool() {
        let usage = parse_usage(LIVE_RESPONSE).unwrap();
        assert_eq!(usage.limits.len(), 2);
        let included = &usage.limits[0];
        assert_eq!(included.percentage, Some(76));
        assert_eq!(included.window, Some(WindowKind::Weekly));
        assert_eq!(included.reset_at, Some(RESET_AT));
        assert_eq!(included.length_secs, Some(WEEK_SECONDS));
        let quota = included.quota_window().unwrap();
        assert_eq!(quota.used_percent, 76.0);
        assert_eq!(quota.resets_at, Some(RESET_AT));
        assert!(usage.limits[1].quota_window().is_none());
        assert_eq!(usage.limits[1].detail.as_deref(), Some("$0.00 remaining"));
    }

    #[test_case(0.0, 0; "explicit_zero")]
    #[test_case(75.6, 76; "rounded")]
    #[test_case(125.0, 100; "over_limit")]
    #[test_case(-5.0, 0; "negative")]
    fn current_usage_and_period_override_legacy(percent: f64, expected: u32) {
        let usage = parse_usage(
            &json!({"config": {
                "creditUsagePercent": percent,
                "currentPeriod": {"type": "USAGE_PERIOD_TYPE_WEEKLY", "start": START, "end": END},
                "monthlyLimit": {"val": 100}, "used": {"val": 100},
                "billingPeriodStart": "2026-10-01T00:00:00Z",
                "billingPeriodEnd": "2026-11-01T00:00:00Z"
            }})
            .to_string(),
        )
        .unwrap();
        let included = &usage.limits[0];
        assert_eq!(included.percentage, Some(expected));
        assert_eq!(included.window, Some(WindowKind::Weekly));
        assert_eq!(included.reset_at, Some(RESET_AT));
        assert_eq!(included.length_secs, Some(WEEK_SECONDS));
    }

    #[test_case(r#"{"monthlyLimit":{"val":1000},"used":{"val":250}}"#, Some(25); "legacy")]
    #[test_case(r#"{"monthlyLimit":{"val":1000},"used":{}}"#, Some(0); "empty_cent_is_zero")]
    #[test_case(r#"{"monthlyLimit":{"val":1000},"used":{"val":0}}"#, Some(0); "zero_cent")]
    #[test_case(r#"{"monthlyLimit":{"val":1000}}"#, None; "missing_used")]
    #[test_case(r#"{"monthlyLimit":{},"used":{"val":5}}"#, None; "zero_limit")]
    #[test_case(r#"{"monthlyLimit":{"val":-100},"used":{"val":5}}"#, None; "negative_limit")]
    #[test_case(r#"{"used":{"val":5}}"#, None; "missing_limit")]
    #[test_case(r#"{"productUsage":[{"product":"GrokBuild","usagePercent":76}]}"#, None; "product_is_not_account_quota")]
    #[test_case("{}", None; "no_personal_quota")]
    #[test_case("null", None; "null_config")]
    fn legacy_and_unreported_quota_preserve_presence(config: &str, expected: Option<u32>) {
        let usage = parse_usage(&format!(r#"{{"config":{config}}}"#)).unwrap();
        let included = &usage.limits[0];
        assert_eq!(included.percentage, expected);
        if expected.is_some() {
            assert_eq!(included.window, Some(WindowKind::Monthly));
            assert!(included.quota_window().is_some());
        } else {
            assert_eq!(included.detail.as_deref(), Some(UNREPORTED_QUOTA));
            assert!(included.quota_window().is_none());
        }
    }

    #[test_case(Some("USAGE_PERIOD_TYPE_MONTHLY"), WindowKind::Monthly; "monthly_type")]
    #[test_case(None, WindowKind::Weekly; "inferred_length")]
    #[test_case(Some("UNRECOGNIZED"), WindowKind::Weekly; "unknown_type")]
    fn period_types_and_legacy_dates(kind: Option<&str>, expected: WindowKind) {
        let usage = parse_usage(
            &json!({"config": {
                "creditUsagePercent": 25,
                "currentPeriod": {"type": kind},
                "billingPeriodStart": START, "billingPeriodEnd": END
            }})
            .to_string(),
        )
        .unwrap();
        assert_eq!(usage.limits[0].window, Some(expected));
        assert_eq!(usage.limits[0].reset_at, Some(RESET_AT));
    }

    #[test_case(r#"{"onDemandUsed":{}}"#, "$0.00 used"; "zero_used")]
    #[test_case(r#"{"onDemandCap":{"val":1234}}"#, "$12.34 cap"; "missing_used")]
    #[test_case(r#"{"onDemandUsed":{"val":1234},"onDemandCap":{"val":2000}}"#, "$12.34 used / $20.00 cap"; "used_and_cap")]
    fn purchased_credits_never_route(config: &str, expected: &str) {
        let config: Value = serde_json::from_str(config).unwrap();
        let usage = parse_usage(
            &json!({"config": config, "subscription_tier": "SuperGrok Heavy"}).to_string(),
        )
        .unwrap();
        assert_eq!(usage.plan.as_deref(), Some("SuperGrok Heavy"));
        assert_eq!(usage.limits[1].detail.as_deref(), Some(expected));
        assert!(
            usage
                .limits
                .iter()
                .all(|limit| limit.quota_window().is_none())
        );
    }

    #[test_case("not json"; "invalid_json")]
    #[test_case("{}"; "missing_config")]
    #[test_case("null"; "null_response")]
    #[test_case(r#"{"config":[]}"#; "array_config")]
    #[test_case(r#"{"config":{"creditUsagePercent":"76"}}"#; "string_percentage")]
    #[test_case(r#"{"config":{"monthlyLimit":{"val":"100"}}}"#; "string_cent")]
    #[test_case(r#"{"config":{"used":{"val":null}}}"#; "null_cent_value")]
    #[test_case(r#"{"config":{"currentPeriod":{"end":42}}}"#; "invalid_date_type")]
    fn malformed_responses_are_errors(response: &str) {
        assert!(parse_usage(response).is_err());
    }

    #[test_case("invalid"; "malformed_date")]
    #[test_case("1960-01-01T00:00:00Z"; "negative_epoch")]
    fn invalid_quota_timestamps_are_errors(end: &str) {
        let response = json!({"config": {
            "creditUsagePercent": 76, "currentPeriod": {"end": end}
        }});
        assert_eq!(
            parse_usage(&response.to_string()).unwrap_err().to_string(),
            INVALID_TIMESTAMP
        );
    }
}
