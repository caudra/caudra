//! Subscription quota windows, normalized across providers, and the response
//! headers that report them on every request.

use std::time::Duration;

use isahc::http::HeaderMap;
use serde::{Deserialize, Serialize};

const FIVE_HOURS_SECS: u64 = 5 * 60 * 60;
const WEEK_SECS: u64 = 7 * 24 * 60 * 60;
const MONTH_MIN_SECS: u64 = 28 * 24 * 60 * 60;
const MONTH_MAX_SECS: u64 = 31 * 24 * 60 * 60;
const DAY_SECS: u64 = 24 * 60 * 60;
const HOUR_SECS: u64 = 60 * 60;
/// A reported length within this share of a known window is that window.
const LENGTH_TOLERANCE_DIVISOR: u64 = 10;
const FULL_PERCENT: f32 = 100.0;
const MILLIS_PER_SEC: u64 = 1000;

const ANTHROPIC_PREFIX: &str = "anthropic-ratelimit-unified-";
const ANTHROPIC_UTILIZATION_SUFFIX: &str = "-utilization";
const ANTHROPIC_RESET_SUFFIX: &str = "-reset";
const ANTHROPIC_FIVE_HOUR: &str = "5h";
const ANTHROPIC_WEEK: &str = "7d";
const ANTHROPIC_SCOPED_WEEK_PREFIX: &str = "7d_";
/// Extra-usage credit, not a window that runs out.
const ANTHROPIC_OVERAGE_PREFIX: &str = "overage";

const CODEX_WINDOWS: [&str; 2] = ["primary", "secondary"];
const CODEX_PREFIX: &str = "x-codex-";
const CODEX_USED_SUFFIX: &str = "-used-percent";
const CODEX_MINUTES_SUFFIX: &str = "-window-minutes";
const CODEX_RESET_AT_SUFFIX: &str = "-reset-at";
const CODEX_RESET_AFTER_SUFFIX: &str = "-reset-after-seconds";

pub const CONFIG_FIVE_HOUR: &str = "5h";
pub const CONFIG_WEEKLY: &str = "weekly";
pub const CONFIG_MONTHLY: &str = "monthly";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "scope", rename_all = "snake_case")]
pub enum WindowKind {
    FiveHour,
    Weekly,
    /// A weekly window that only one model family draws from, such as Opus.
    WeeklyScoped(String),
    Monthly,
    Other(String),
}

impl WindowKind {
    pub fn from_length(length: Duration) -> Self {
        let secs = length.as_secs();
        let near = |target: u64| secs.abs_diff(target) <= target / LENGTH_TOLERANCE_DIVISOR;
        if near(FIVE_HOURS_SECS) {
            Self::FiveHour
        } else if near(WEEK_SECS) {
            Self::Weekly
        } else if (MONTH_MIN_SECS..=MONTH_MAX_SECS).contains(&secs) {
            Self::Monthly
        } else if secs >= DAY_SECS && secs.is_multiple_of(DAY_SECS) {
            Self::Other(format!("{}d", secs / DAY_SECS))
        } else {
            Self::Other(format!("{}h", secs.div_ceil(HOUR_SECS)))
        }
    }

    /// The `switching.windows` name this window answers to.
    pub fn config_name(&self) -> Option<&'static str> {
        match self {
            Self::FiveHour => Some(CONFIG_FIVE_HOUR),
            Self::Weekly | Self::WeeklyScoped(_) => Some(CONFIG_WEEKLY),
            Self::Monthly => Some(CONFIG_MONTHLY),
            Self::Other(_) => None,
        }
    }

    /// A scoped window only limits the models it names.
    pub fn applies_to(&self, model_id: &str) -> bool {
        match self {
            Self::WeeklyScoped(scope) => model_id
                .to_ascii_lowercase()
                .contains(&scope.to_ascii_lowercase()),
            Self::FiveHour | Self::Weekly | Self::Monthly | Self::Other(_) => true,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::FiveHour => CONFIG_FIVE_HOUR.to_owned(),
            Self::Weekly => CONFIG_WEEKLY.to_owned(),
            Self::WeeklyScoped(scope) => format!("{CONFIG_WEEKLY} {scope}"),
            Self::Monthly => CONFIG_MONTHLY.to_owned(),
            Self::Other(label) => label.clone(),
        }
    }

    /// Two or three characters, for a quota bar's row label.
    pub fn short_label(&self) -> &str {
        match self {
            Self::FiveHour => CONFIG_FIVE_HOUR,
            Self::Weekly | Self::WeeklyScoped(_) => "wk",
            Self::Monthly => "mo",
            Self::Other(label) => label,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub kind: WindowKind,
    /// 0 to 100.
    pub used_percent: f32,
    /// Epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length_secs: Option<u64>,
}

impl QuotaWindow {
    pub fn new(kind: WindowKind, used_percent: f32, resets_at: Option<u64>) -> Self {
        let length_secs = match kind {
            WindowKind::FiveHour => Some(FIVE_HOURS_SECS),
            WindowKind::Weekly | WindowKind::WeeklyScoped(_) => Some(WEEK_SECS),
            WindowKind::Monthly | WindowKind::Other(_) => None,
        };
        Self {
            kind,
            used_percent: used_percent.clamp(0.0, FULL_PERCENT),
            resets_at,
            length_secs,
        }
    }

    pub fn with_length(mut self, length: Duration) -> Self {
        self.length_secs = Some(length.as_secs());
        self
    }

    /// A window whose reset has passed is full again.
    pub fn left_percent(&self, now_ms: u64) -> f32 {
        if self.has_reset(now_ms) {
            FULL_PERCENT
        } else {
            FULL_PERCENT - self.used_percent
        }
    }

    pub fn has_reset(&self, now_ms: u64) -> bool {
        self.resets_at.is_some_and(|reset| reset <= now_ms)
    }

    /// How far through its window the clock is, when the window's length and
    /// reset are both known.
    pub fn elapsed_percent(&self, now_ms: u64) -> Option<f32> {
        let length_ms = self.length_secs? * MILLIS_PER_SEC;
        let reset = self.resets_at?;
        if length_ms == 0 || reset <= now_ms {
            return Some(0.0);
        }
        let remaining = (reset - now_ms).min(length_ms);
        Some((length_ms - remaining) as f32 * FULL_PERCENT / length_ms as f32)
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok().map(str::trim)
}

fn header_f32(headers: &HeaderMap, name: &str) -> Option<f32> {
    header(headers, name)?
        .parse::<f32>()
        .ok()
        .filter(|value| value.is_finite())
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    let value = header(headers, name)?;
    value
        .parse::<u64>()
        .ok()
        .or_else(|| value.parse::<f64>().ok().map(|float| float.max(0.0) as u64))
}

/// `anthropic-ratelimit-unified-<window>-utilization` (0 to 1) and `-reset`
/// (epoch seconds), sent to subscription OAuth requests only.
pub fn from_anthropic_headers(headers: &HeaderMap) -> Vec<QuotaWindow> {
    headers
        .keys()
        .filter_map(|name| {
            name.as_str()
                .strip_prefix(ANTHROPIC_PREFIX)?
                .strip_suffix(ANTHROPIC_UTILIZATION_SUFFIX)
        })
        .filter(|window| !window.starts_with(ANTHROPIC_OVERAGE_PREFIX))
        .filter_map(|window| {
            let utilization = header_f32(
                headers,
                &format!("{ANTHROPIC_PREFIX}{window}{ANTHROPIC_UTILIZATION_SUFFIX}"),
            )?;
            let resets_at = header_u64(
                headers,
                &format!("{ANTHROPIC_PREFIX}{window}{ANTHROPIC_RESET_SUFFIX}"),
            )
            .map(|secs| secs * MILLIS_PER_SEC);
            let kind = match window {
                ANTHROPIC_FIVE_HOUR => WindowKind::FiveHour,
                ANTHROPIC_WEEK => WindowKind::Weekly,
                scoped if scoped.starts_with(ANTHROPIC_SCOPED_WEEK_PREFIX) => {
                    WindowKind::WeeklyScoped(scoped[ANTHROPIC_SCOPED_WEEK_PREFIX.len()..].into())
                }
                other => WindowKind::Other(other.to_owned()),
            };
            Some(QuotaWindow::new(
                kind,
                utilization * FULL_PERCENT,
                resets_at,
            ))
        })
        .collect()
}

/// `x-codex-{primary,secondary}-used-percent`, `-window-minutes`, and
/// `-reset-at` (epoch seconds) or the older `-reset-after-seconds`. A slot
/// without a window length names no window: Codex sends such placeholders
/// for limits the plan does not have.
pub fn from_codex_headers(headers: &HeaderMap, now_ms: u64) -> Vec<QuotaWindow> {
    CODEX_WINDOWS
        .iter()
        .filter_map(|window| {
            let prefix = format!("{CODEX_PREFIX}{window}");
            let used = header_f32(headers, &format!("{prefix}{CODEX_USED_SUFFIX}"))?;
            let length = header_u64(headers, &format!("{prefix}{CODEX_MINUTES_SUFFIX}"))
                .filter(|minutes| *minutes > 0)
                .map(|minutes| Duration::from_secs(minutes * 60))?;
            let resets_at = header_u64(headers, &format!("{prefix}{CODEX_RESET_AT_SUFFIX}"))
                .map(|secs| secs * MILLIS_PER_SEC)
                .or_else(|| {
                    header_u64(headers, &format!("{prefix}{CODEX_RESET_AFTER_SUFFIX}"))
                        .map(|secs| now_ms + secs * MILLIS_PER_SEC)
                });
            Some(
                QuotaWindow::new(WindowKind::from_length(length), used, resets_at)
                    .with_length(length),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use isahc::http::HeaderValue;
    use test_case::test_case;

    use super::*;

    const NOW_MS: u64 = 1_800_000_000_000;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn anthropic_headers_read_every_window() {
        let parsed = from_anthropic_headers(&headers(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.41"),
            ("anthropic-ratelimit-unified-5h-reset", "1800003600"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.82"),
            ("anthropic-ratelimit-unified-7d-reset", "1800300000"),
            ("anthropic-ratelimit-unified-7d_opus-utilization", "0.5"),
            ("anthropic-ratelimit-unified-overage-utilization", "0.9"),
            ("anthropic-ratelimit-unified-status", "allowed"),
        ]));
        let mut kinds: Vec<(WindowKind, u32, Option<u64>)> = parsed
            .iter()
            .map(|window| {
                (
                    window.kind.clone(),
                    window.used_percent.round() as u32,
                    window.resets_at,
                )
            })
            .collect();
        kinds.sort_by_key(|(kind, ..)| kind.label());
        assert_eq!(
            kinds,
            vec![
                (WindowKind::FiveHour, 41, Some(1_800_003_600_000)),
                (WindowKind::Weekly, 82, Some(1_800_300_000_000)),
                (WindowKind::WeeklyScoped("opus".into()), 50, None),
            ]
        );
    }

    #[test]
    fn anthropic_headers_absent_for_api_keys() {
        assert!(
            from_anthropic_headers(&headers(&[("anthropic-ratelimit-requests-limit", "50")]))
                .is_empty()
        );
    }

    #[test]
    fn codex_headers_read_primary_and_secondary() {
        let parsed = from_codex_headers(
            &headers(&[
                ("x-codex-primary-used-percent", "65.5"),
                ("x-codex-primary-window-minutes", "300"),
                ("x-codex-primary-reset-at", "1800003600"),
                ("x-codex-secondary-used-percent", "23.8"),
                ("x-codex-secondary-window-minutes", "10080"),
                ("x-codex-secondary-reset-after-seconds", "60"),
            ]),
            NOW_MS,
        );
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].kind, WindowKind::FiveHour);
        assert_eq!(parsed[0].resets_at, Some(1_800_003_600_000));
        assert_eq!(parsed[1].kind, WindowKind::Weekly);
        assert_eq!(parsed[1].resets_at, Some(NOW_MS + 60_000));
        assert_eq!(parsed[1].length_secs, Some(WEEK_SECS));
    }

    #[test_case(&[("x-codex-secondary-reset-at", "1800003600")] ; "no_minutes")]
    #[test_case(&[("x-codex-secondary-window-minutes", "0"), ("x-codex-secondary-reset-after-seconds", "0")] ; "placeholder")]
    fn codex_slots_without_a_length_are_not_windows(secondary: &[(&'static str, &str)]) {
        let mut pairs = vec![
            ("x-codex-primary-used-percent", "82"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-secondary-used-percent", "0"),
        ];
        pairs.extend_from_slice(secondary);
        let kinds: Vec<WindowKind> = from_codex_headers(&headers(&pairs), NOW_MS)
            .into_iter()
            .map(|window| window.kind)
            .collect();
        assert_eq!(kinds, vec![WindowKind::FiveHour]);
    }

    #[test_case(FIVE_HOURS_SECS, WindowKind::FiveHour ; "five_hours")]
    #[test_case(WEEK_SECS, WindowKind::Weekly ; "week")]
    #[test_case(30 * DAY_SECS, WindowKind::Monthly ; "month")]
    #[test_case(2 * DAY_SECS, WindowKind::Other("2d".into()) ; "two_days")]
    #[test_case(2 * HOUR_SECS, WindowKind::Other("2h".into()) ; "two_hours")]
    fn lengths_name_windows(secs: u64, expected: WindowKind) {
        assert_eq!(WindowKind::from_length(Duration::from_secs(secs)), expected);
    }

    #[test_case(Some(NOW_MS - 1), 100.0 ; "reset_passed")]
    #[test_case(Some(NOW_MS + 1), 30.0 ; "reset_ahead")]
    #[test_case(None, 30.0 ; "reset_unknown")]
    fn windows_refill_after_their_reset(resets_at: Option<u64>, expected: f32) {
        let window = QuotaWindow::new(WindowKind::FiveHour, 70.0, resets_at);
        assert_eq!(window.left_percent(NOW_MS), expected);
    }

    #[test]
    fn elapsed_share_follows_the_reset() {
        let half = FIVE_HOURS_SECS * MILLIS_PER_SEC / 2;
        let window = QuotaWindow::new(WindowKind::FiveHour, 10.0, Some(NOW_MS + half));
        assert_eq!(window.elapsed_percent(NOW_MS), Some(50.0));
    }

    #[test_case(WindowKind::WeeklyScoped("opus".into()), "claude-opus-5-5", true ; "scoped_match")]
    #[test_case(WindowKind::WeeklyScoped("opus".into()), "claude-sonnet-5", false ; "scoped_other")]
    #[test_case(WindowKind::Weekly, "claude-sonnet-5", true ; "unscoped")]
    fn scoped_windows_limit_their_models(kind: WindowKind, model: &str, expected: bool) {
        assert_eq!(kind.applies_to(model), expected);
    }
}
