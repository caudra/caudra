use std::env;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use crate::workcell::{WorkcellEndpoint, WorkcellEndpointError};

pub const BASE_URL_ENV: &str = "TYPESAFE_BASE_URL";
const DEFAULT_MODEL: &str = "jev-latest";
const DEFAULT_API_KEY_ENV: &str = "TYPESAFE_API_KEY";
const DEFAULT_TIMEOUT_MS: u64 = 400;
const DEFAULT_LOG_RETENTION_DAYS: u32 = 90;
const DEFAULT_FLAG_THRESHOLD: f64 = 0.85;
const DEFAULT_CONFIDENCE_THRESHOLD: f64 = 0.9;
const DEFAULT_GOAL_SKIP_BELOW: f64 = 0.05;
const INVALID_ENDPOINT_MESSAGE: &str = "must be an absolute HTTP(S) URL without credentials, query, fragment, whitespace or control characters";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureMode {
    #[default]
    Off,
    Shadow,
    Advise,
    Enforce,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DecisionsConfigError {
    #[error("invalid config: decisions.{field}: {message}")]
    Invalid {
        field: &'static str,
        message: &'static str,
    },
    #[error(
        "invalid project config: decisions.{0} is global-only; projects may only disable features or logging and shorten log retention"
    )]
    ProjectOverride(&'static str),
}

fn invalid(field: &'static str, message: &'static str) -> DecisionsConfigError {
    DecisionsConfigError::Invalid { field, message }
}

macro_rules! features {
    ($($field:ident: [$($mode:ident),+]),+ $(,)?) => {
        #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
        pub struct DecisionFeatures {
            $(pub $field: FeatureMode,)+
        }

        #[derive(Debug, Clone, Default, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        pub struct RawDecisionFeatures {
            $(pub $field: Option<FeatureMode>,)+
        }

        impl RawDecisionFeatures {
            fn resolve(self) -> Result<DecisionFeatures, DecisionsConfigError> {
                let features = DecisionFeatures {
                    $($field: self.$field.unwrap_or_default(),)+
                };
                features.validate()?;
                Ok(features)
            }

            fn restrict(&mut self, overlay: Self) -> Result<(), DecisionsConfigError> {
                $(if overlay.$field.as_ref().is_some_and(|mode| *mode != FeatureMode::Off) {
                    return Err(DecisionsConfigError::ProjectOverride(concat!("features.", stringify!($field))));
                })+
                self.overlay(overlay);
                Ok(())
            }

            fn overlay(&mut self, overlay: Self) {
                $(if overlay.$field.is_some() {
                    self.$field = overlay.$field;
                })+
            }
        }

        impl DecisionFeatures {
            pub fn validate(&self) -> Result<(), DecisionsConfigError> {
                $(if !matches!(self.$field, FeatureMode::Off | FeatureMode::Shadow $(| FeatureMode::$mode)+) {
                    return Err(invalid(concat!("features.", stringify!($field)), "unsupported feature mode"));
                })+
                Ok(())
            }

            pub fn any_enabled(&self) -> bool {
                $(self.$field != FeatureMode::Off)||+
            }
        }
    };
}

features! {
    permission_advice: [Advise],
    auto_screening: [Enforce],
    shell_effect: [Advise],
    content_screening: [Advise],
    shell_duration: [Advise, Enforce],
    tool_search: [Enforce],
    skill_suggestions: [Advise],
    goal_prescreen: [Enforce],
    subagent_routing: [Enforce],
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DecisionThresholds {
    pub permission_flag: f64,
    pub auto_flag: f64,
    pub content_injection: f64,
    pub content_addressed_to_agent: f64,
    pub shell_endless: f64,
    pub shell_heavy: f64,
    pub routing_confidence: f64,
    pub goal_skip_below: f64,
    pub shell_writes: Option<f64>,
}

impl Default for DecisionThresholds {
    fn default() -> Self {
        Self {
            permission_flag: DEFAULT_FLAG_THRESHOLD,
            auto_flag: DEFAULT_FLAG_THRESHOLD,
            content_injection: DEFAULT_CONFIDENCE_THRESHOLD,
            content_addressed_to_agent: DEFAULT_CONFIDENCE_THRESHOLD,
            shell_endless: DEFAULT_CONFIDENCE_THRESHOLD,
            shell_heavy: DEFAULT_CONFIDENCE_THRESHOLD,
            routing_confidence: DEFAULT_CONFIDENCE_THRESHOLD,
            goal_skip_below: DEFAULT_GOAL_SKIP_BELOW,
            shell_writes: None,
        }
    }
}

impl DecisionThresholds {
    fn validate(&self) -> Result<(), DecisionsConfigError> {
        for (field, value) in [
            ("thresholds.permission_flag", Some(self.permission_flag)),
            ("thresholds.auto_flag", Some(self.auto_flag)),
            ("thresholds.content_injection", Some(self.content_injection)),
            (
                "thresholds.content_addressed_to_agent",
                Some(self.content_addressed_to_agent),
            ),
            ("thresholds.shell_endless", Some(self.shell_endless)),
            ("thresholds.shell_heavy", Some(self.shell_heavy)),
            (
                "thresholds.routing_confidence",
                Some(self.routing_confidence),
            ),
            ("thresholds.goal_skip_below", Some(self.goal_skip_below)),
            ("thresholds.shell_writes", self.shell_writes),
        ] {
            if value.is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value)) {
                return Err(invalid(
                    field,
                    "must be a finite probability between 0 and 1",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RawDecisionsConfig {
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub api_key_env: Option<String>,
    pub allow_remote: Option<bool>,
    pub allow_http: Option<bool>,
    pub timeout_ms: Option<u64>,
    pub log: Option<bool>,
    pub log_retention_days: Option<u32>,
    pub features: RawDecisionFeatures,
    pub thresholds: Option<DecisionThresholds>,
    #[serde(skip)]
    project_error: Option<DecisionsConfigError>,
    #[serde(skip)]
    auto_screening_restricted: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecisionsConfig {
    pub endpoint: Option<Url>,
    pub model: String,
    pub api_key_env: String,
    pub allow_remote: bool,
    pub allow_http: bool,
    pub timeout_ms: u64,
    pub log: bool,
    pub log_retention_days: u32,
    pub features: DecisionFeatures,
    pub thresholds: DecisionThresholds,
    pub auto_screening_restricted: bool,
}

impl Default for DecisionsConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            model: DEFAULT_MODEL.into(),
            api_key_env: DEFAULT_API_KEY_ENV.into(),
            allow_remote: false,
            allow_http: false,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            log: false,
            log_retention_days: DEFAULT_LOG_RETENTION_DAYS,
            features: DecisionFeatures::default(),
            thresholds: DecisionThresholds::default(),
            auto_screening_restricted: false,
        }
    }
}

impl RawDecisionsConfig {
    /// A layer with the same authority replaces whatever it names; restriction
    /// provenance from earlier layers is never cleared.
    pub(crate) fn overlay(&mut self, overlay: Self) {
        macro_rules! replace {
            ($($field:ident),+) => {
                $(if overlay.$field.is_some() { self.$field = overlay.$field; })+
            };
        }
        replace!(
            endpoint,
            model,
            api_key_env,
            allow_remote,
            allow_http,
            timeout_ms,
            log,
            log_retention_days,
            thresholds
        );
        self.features.overlay(overlay.features);
        self.project_error = self.project_error.take().or(overlay.project_error);
        self.auto_screening_restricted |= overlay.auto_screening_restricted;
    }

    pub(crate) fn restrict(&mut self, overlay: Self) {
        if self.project_error.is_some() {
            return;
        }
        self.project_error = self.try_restrict(overlay).err();
    }

    fn try_restrict(&mut self, overlay: Self) -> Result<(), DecisionsConfigError> {
        if let Some(error) = overlay.project_error {
            return Err(error);
        }
        for (field, present) in [
            ("endpoint", overlay.endpoint.is_some()),
            ("model", overlay.model.is_some()),
            ("api_key_env", overlay.api_key_env.is_some()),
            ("allow_remote", overlay.allow_remote.is_some()),
            ("allow_http", overlay.allow_http.is_some()),
            ("timeout_ms", overlay.timeout_ms.is_some()),
            ("thresholds", overlay.thresholds.is_some()),
            ("log", overlay.log == Some(true)),
            (
                "log_retention_days",
                overlay.log_retention_days.is_some_and(|days| {
                    days > self
                        .log_retention_days
                        .unwrap_or(DEFAULT_LOG_RETENTION_DAYS)
                }),
            ),
        ] {
            if present {
                return Err(DecisionsConfigError::ProjectOverride(field));
            }
        }
        let screening_restricted = self.features.auto_screening == Some(FeatureMode::Enforce)
            && (overlay.features.auto_screening == Some(FeatureMode::Off)
                || (self.features.content_screening == Some(FeatureMode::Advise)
                    && overlay.features.content_screening == Some(FeatureMode::Off)));
        self.features.restrict(overlay.features)?;
        self.auto_screening_restricted |= screening_restricted || overlay.auto_screening_restricted;
        if overlay.log.is_some() {
            self.log = overlay.log;
        }
        if overlay.log_retention_days.is_some() {
            self.log_retention_days = overlay.log_retention_days;
        }
        Ok(())
    }

    pub fn resolve(self, base_url: Option<&str>) -> Result<DecisionsConfig, DecisionsConfigError> {
        if let Some(error) = self.project_error {
            return Err(error);
        }
        let allow_remote = self.allow_remote.unwrap_or(false);
        let allow_http = self.allow_http.unwrap_or(false);
        let endpoint = self
            .endpoint
            .as_deref()
            .map(|endpoint| -> Result<Url, DecisionsConfigError> {
                let mut endpoint = parse_endpoint(endpoint, allow_remote, allow_http)?;
                if let Some(base_url) = base_url {
                    let mut origin = parse_endpoint(base_url, allow_remote, allow_http)?;
                    if origin.path() != "/" {
                        return Err(invalid(
                            "endpoint",
                            "TYPESAFE_BASE_URL must be an origin without a path",
                        ));
                    }
                    origin.set_path(endpoint.path());
                    endpoint = parse_endpoint(origin.as_str(), allow_remote, allow_http)?;
                }
                Ok(endpoint)
            })
            .transpose()?;
        let config = DecisionsConfig {
            endpoint,
            model: self.model.unwrap_or_else(|| DEFAULT_MODEL.into()),
            api_key_env: self
                .api_key_env
                .unwrap_or_else(|| DEFAULT_API_KEY_ENV.into()),
            allow_remote,
            allow_http,
            timeout_ms: self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            log: self.log.unwrap_or(false),
            log_retention_days: self
                .log_retention_days
                .unwrap_or(DEFAULT_LOG_RETENTION_DAYS),
            features: self.features.resolve()?,
            thresholds: self.thresholds.unwrap_or_default(),
            auto_screening_restricted: self.auto_screening_restricted,
        };
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn resolve_env(self) -> Result<DecisionsConfig, DecisionsConfigError> {
        if self.endpoint.is_none() {
            return self.resolve(None);
        }
        let base_url = env::var(BASE_URL_ENV)
            .map(Some)
            .or_else(|error| match error {
                env::VarError::NotPresent => Ok(None),
                env::VarError::NotUnicode(_) => {
                    Err(invalid("endpoint", "TYPESAFE_BASE_URL must be UTF-8"))
                }
            })?;
        self.resolve(base_url.as_deref())
    }
}

impl DecisionsConfig {
    pub fn validate(&self) -> Result<(), DecisionsConfigError> {
        if let Some(endpoint) = &self.endpoint {
            parse_endpoint(endpoint.as_str(), self.allow_remote, self.allow_http)?;
        }
        let mut key = self.api_key_env.bytes();
        if !key
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            || !key.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(invalid(
                "api_key_env",
                "must be an environment variable name, not a credential",
            ));
        }
        if self.model.trim().is_empty() || self.model.chars().any(char::is_control) {
            return Err(invalid(
                "model",
                "must be nonempty and contain no control characters",
            ));
        }
        if self.timeout_ms == 0 {
            return Err(invalid("timeout_ms", "must be greater than zero"));
        }
        if self.log_retention_days == 0 {
            return Err(invalid("log_retention_days", "must be greater than zero"));
        }
        self.features.validate()?;
        self.thresholds.validate()
    }

    pub fn api_key(&self) -> Result<Option<String>, DecisionsConfigError> {
        self.validate()?;
        let key = crate::global_env_value(&self.api_key_env)
            .map_err(|_| invalid("api_key_env", "credential must be UTF-8"))?;
        if key
            .as_ref()
            .is_some_and(|key| key.is_empty() || key.chars().any(char::is_control))
        {
            return Err(invalid(
                "api_key_env",
                "credential must be nonempty and contain no control characters",
            ));
        }
        Ok(key)
    }
}

fn parse_endpoint(
    endpoint: &str,
    allow_remote: bool,
    allow_http: bool,
) -> Result<Url, DecisionsConfigError> {
    if endpoint
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
        || !endpoint
            .split_once(':')
            .and_then(|(_, authority)| authority.strip_prefix("//"))
            .is_some_and(|authority| !authority.starts_with(['/', '\\']))
    {
        return Err(invalid("endpoint", INVALID_ENDPOINT_MESSAGE));
    }
    let (endpoint, is_loopback) = match WorkcellEndpoint::parse(endpoint) {
        Ok(endpoint) => (endpoint.as_url().clone(), endpoint.is_loopback()),
        Err(WorkcellEndpointError::InsecureRemote) => (
            Url::parse(endpoint).map_err(|_| invalid("endpoint", INVALID_ENDPOINT_MESSAGE))?,
            false,
        ),
        Err(_) => return Err(invalid("endpoint", INVALID_ENDPOINT_MESSAGE)),
    };
    if !is_loopback && !allow_remote {
        return Err(invalid(
            "allow_remote",
            "non-loopback endpoints send data off-machine; set allow_remote = true in global config to opt in",
        ));
    }
    if !is_loopback && endpoint.scheme() == "http" && !allow_http {
        return Err(invalid(
            "allow_http",
            "non-loopback HTTP endpoints send data and credentials without TLS; set allow_http = true in global config to opt in",
        ));
    }
    Ok(endpoint)
}

#[cfg(test)]
mod tests {
    use super::{
        DecisionsConfig, DecisionsConfigError, FeatureMode, INVALID_ENDPOINT_MESSAGE,
        RawDecisionsConfig, invalid,
    };
    use crate::{ConfigError, RawConfig};
    use test_case::test_case;
    use url::Url;

    const LOCAL_ENDPOINT: &str = "http://127.0.0.1:8000/v1/systemone";
    const REMOTE_ORIGIN: &str = "https://decisions.example.test";
    const REMOTE_ENDPOINT: &str = "https://decisions.example.test/v1/systemone";
    const REMOTE_HTTP_ORIGIN: &str = "http://decisions.example.test:443";
    const REMOTE_HTTP_ENDPOINT: &str = "http://decisions.example.test:443/v1/systemone";
    const ENDPOINT_SIZE_LIMIT: usize = 2048;

    #[test]
    fn defaults_and_environment_never_activate_decisions() {
        let config = RawDecisionsConfig::default()
            .resolve(Some(REMOTE_ORIGIN))
            .unwrap();
        assert_eq!(config, DecisionsConfig::default());
        assert!(!config.features.any_enabled());
        assert!(!config.allow_remote);
        assert!(!config.allow_http);
        assert!(!config.log);
        assert!(config.thresholds.shell_writes.is_none());
        assert!(
            RawConfig::default()
                .into_config(false)
                .unwrap()
                .decisions
                .endpoint
                .is_none()
        );
    }

    #[test_case("permission_advice", true, false)]
    #[test_case("auto_screening", false, true)]
    #[test_case("shell_effect", true, false)]
    #[test_case("content_screening", true, false)]
    #[test_case("shell_duration", true, true)]
    #[test_case("tool_search", false, true)]
    #[test_case("skill_suggestions", true, false)]
    #[test_case("goal_prescreen", false, true)]
    #[test_case("subagent_routing", false, true)]
    fn feature_modes_are_validated(feature: &str, advise: bool, enforce: bool) {
        for (mode, allowed) in [
            ("off", true),
            ("shadow", true),
            ("advise", advise),
            ("enforce", enforce),
        ] {
            let raw: RawDecisionsConfig =
                toml::from_str(&format!("[features]\n{feature} = '{mode}'")).unwrap();
            assert_eq!(raw.resolve(None).is_ok(), allowed, "{feature}: {mode}");
        }
    }

    #[test_case("http://127.0.0.1:8000/v1/systemone", false, true; "numeric_loopback")]
    #[test_case("http://[::1]:8000/v1/systemone", false, true; "ipv6_loopback")]
    #[test_case("https://decisions.example.test/v1/systemone", false, false; "remote_requires_opt_in")]
    #[test_case("https://decisions.example.test/v1/systemone", true, true; "remote_opt_in")]
    #[test_case("http://decisions.example.test/v1/systemone", true, false; "remote_requires_tls")]
    #[test_case("https://localhost/v1/systemone", false, false; "dns_is_not_loopback")]
    #[test_case("http://127.0.0.1.evil.test/v1/systemone", false, false; "loopback_lookalike")]
    #[test_case("https://user:secret@decisions.example.test/v1/systemone", true, false; "credentials")]
    #[test_case("https://@decisions.example.test/v1/systemone", true, false; "empty_userinfo")]
    #[test_case("https://decisions.example.test/v1/systemone#secret", true, false; "fragment")]
    #[test_case("https://decisions.example.test/v1/systemone?api_key=secret", true, false; "query")]
    #[test_case("file:///etc/passwd", true, false; "unsupported_scheme")]
    fn endpoint_policy(endpoint: &str, allow_remote: bool, valid: bool) {
        let raw = RawDecisionsConfig {
            endpoint: Some(endpoint.into()),
            allow_remote: Some(allow_remote),
            ..RawDecisionsConfig::default()
        };
        assert_eq!(raw.resolve(None).is_ok(), valid);
    }

    #[test_case(REMOTE_HTTP_ENDPOINT; "dns")]
    #[test_case("http://decisions.example.test:80/v1/systemone"; "default_port")]
    #[test_case("http://192.0.2.1:8000/v1/systemone"; "ipv4")]
    #[test_case("http://[2001:db8::1]:8000/v1/systemone"; "ipv6")]
    #[test_case("http://100.64.0.1:8000/v1/systemone"; "cgnat_start")]
    #[test_case("http://100.127.255.254:8000/v1/systemone"; "cgnat_end")]
    #[test_case("http://localhost:8000/v1/systemone"; "localhost_is_dns")]
    #[test_case("http://127.0.0.1.evil.test/v1/systemone"; "loopback_lookalike")]
    #[test_case("HTTP://decisions.example.test:443/v1/systemone"; "uppercase_scheme")]
    fn remote_http_requires_both_global_opt_ins(endpoint: &str) {
        for (allow_remote, allow_http, error_field) in [
            (false, false, Some("allow_remote")),
            (false, true, Some("allow_remote")),
            (true, false, Some("allow_http")),
            (true, true, None),
        ] {
            let raw = RawDecisionsConfig {
                endpoint: Some(endpoint.into()),
                allow_remote: Some(allow_remote),
                allow_http: Some(allow_http),
                ..RawDecisionsConfig::default()
            };
            let expected = DecisionsConfig {
                endpoint: Some(Url::parse(endpoint).unwrap()),
                allow_remote,
                allow_http,
                ..DecisionsConfig::default()
            };
            let result = raw.resolve(None);
            if let Some(field) = error_field {
                assert!(matches!(
                    result,
                    Err(DecisionsConfigError::Invalid { field: actual, .. }) if actual == field
                ));
                assert!(matches!(
                    expected.validate(),
                    Err(DecisionsConfigError::Invalid { field: actual, .. }) if actual == field
                ));
            } else {
                assert_eq!(result.unwrap(), expected);
                assert!(expected.validate().is_ok());
            }
        }
    }

    #[test_case("http://user:secret@decisions.example.test"; "credentials")]
    #[test_case("http://@decisions.example.test"; "empty_userinfo")]
    #[test_case("http://decisions.example.test?"; "empty_query")]
    #[test_case("http://decisions.example.test?api_key=secret"; "query")]
    #[test_case("http://decisions.example.test#"; "empty_fragment")]
    #[test_case("http://decisions.example.test#secret"; "fragment")]
    #[test_case(" http://decisions.example.test"; "leading_space")]
    #[test_case("http://decisions.example.test/ "; "trailing_space")]
    #[test_case("http://decisions.example.test/a b"; "path_space")]
    #[test_case("http://decisions.example.test/a\u{2003}b"; "unicode_whitespace")]
    #[test_case("http://decisions.\texample.test"; "tab")]
    #[test_case("http://decisions.example.test/\n"; "newline")]
    #[test_case("http://decisions.example.test/\0"; "nul")]
    #[test_case("http://decisions.example.test/\u{7f}"; "control")]
    #[test_case("http:@decisions.example.test"; "missing_authority_separator")]
    #[test_case("http:user:secret@decisions.example.test/path://suffix"; "separator_in_path")]
    #[test_case("http:///@decisions.example.test"; "extra_authority_slash")]
    #[test_case("http://\\@decisions.example.test"; "authority_backslash")]
    #[test_case("http:///decisions.example.test"; "empty_authority")]
    #[test_case("http://[::1"; "invalid_ipv6")]
    #[test_case("http://decisions.example.test:65536"; "invalid_port")]
    #[test_case("http://"; "missing_host")]
    #[test_case("file:///etc/passwd"; "file_scheme")]
    #[test_case("ftp://decisions.example.test"; "ftp_scheme")]
    #[test_case("/v1/systemone"; "relative")]
    #[test_case(""; "empty")]
    fn http_opt_in_does_not_relax_url_validation(endpoint: &str) {
        let raw = RawDecisionsConfig {
            endpoint: Some(endpoint.into()),
            allow_remote: Some(true),
            allow_http: Some(true),
            ..RawDecisionsConfig::default()
        };
        assert_eq!(
            raw.resolve(None),
            Err(invalid("endpoint", INVALID_ENDPOINT_MESSAGE))
        );
    }

    #[test_case(ENDPOINT_SIZE_LIMIT, true; "at_limit")]
    #[test_case(ENDPOINT_SIZE_LIMIT + 1, false; "over_limit")]
    fn http_opt_in_preserves_endpoint_size_limit(length: usize, valid: bool) {
        let endpoint = format!(
            "{REMOTE_HTTP_ENDPOINT}{}",
            "a".repeat(length - REMOTE_HTTP_ENDPOINT.len())
        );
        let raw = RawDecisionsConfig {
            endpoint: Some(endpoint),
            allow_remote: Some(true),
            allow_http: Some(true),
            ..RawDecisionsConfig::default()
        };
        assert_eq!(raw.resolve(None).is_ok(), valid);
    }

    #[test_case(""; "omitted")]
    #[test_case("allow_http = false"; "explicit_false")]
    #[test_case("allow_http = true"; "explicit_true")]
    fn numeric_loopback_http_does_not_require_opt_in(setting: &str) {
        for endpoint in [LOCAL_ENDPOINT, "http://[::1]:8000/v1/systemone"] {
            let raw: RawDecisionsConfig =
                toml::from_str(&format!("endpoint = '{endpoint}'\n{setting}")).unwrap();
            let config = raw.resolve(None).unwrap();
            assert_eq!(config.endpoint.unwrap().as_str(), endpoint);
            assert!(!config.allow_remote);
        }
    }

    #[test_case(false; "remote_redirect_rejected")]
    #[test_case(true; "remote_redirect_opted_in")]
    fn environment_origin_is_revalidated(allow_remote: bool) {
        let raw = RawDecisionsConfig {
            endpoint: Some(LOCAL_ENDPOINT.into()),
            allow_remote: Some(allow_remote),
            ..RawDecisionsConfig::default()
        };
        let result = raw.resolve(Some(REMOTE_ORIGIN));
        if allow_remote {
            assert_eq!(result.unwrap().endpoint.unwrap().as_str(), REMOTE_ENDPOINT);
        } else {
            assert!(matches!(
                result,
                Err(DecisionsConfigError::Invalid {
                    field: "allow_remote",
                    ..
                })
            ));
        }
    }

    #[test_case(LOCAL_ENDPOINT; "local_http")]
    #[test_case("https://127.0.0.1:8000/v1/systemone"; "local_https")]
    fn environment_remote_http_requires_both_opt_ins(endpoint: &str) {
        for (allow_remote, allow_http, error_field) in [
            (false, false, Some("allow_remote")),
            (false, true, Some("allow_remote")),
            (true, false, Some("allow_http")),
            (true, true, None),
        ] {
            let raw = RawDecisionsConfig {
                endpoint: Some(endpoint.into()),
                allow_remote: Some(allow_remote),
                allow_http: Some(allow_http),
                ..RawDecisionsConfig::default()
            };
            let result = raw.resolve(Some(REMOTE_HTTP_ORIGIN));
            if let Some(field) = error_field {
                assert!(matches!(
                    result,
                    Err(DecisionsConfigError::Invalid { field: actual, .. }) if actual == field
                ));
            } else {
                assert_eq!(
                    result.unwrap().endpoint.unwrap().as_str(),
                    REMOTE_HTTP_ENDPOINT
                );
            }
        }
    }

    #[test_case(false; "https_downgrade_rejected")]
    #[test_case(true; "https_downgrade_opted_in")]
    fn environment_https_downgrade_requires_http_opt_in(allow_http: bool) {
        let raw = RawDecisionsConfig {
            endpoint: Some(REMOTE_ENDPOINT.into()),
            allow_remote: Some(true),
            allow_http: Some(allow_http),
            ..RawDecisionsConfig::default()
        };
        let result = raw.resolve(Some(REMOTE_HTTP_ORIGIN));
        if allow_http {
            assert_eq!(
                result.unwrap().endpoint.unwrap().as_str(),
                REMOTE_HTTP_ENDPOINT
            );
        } else {
            assert!(matches!(
                result,
                Err(DecisionsConfigError::Invalid {
                    field: "allow_http",
                    ..
                })
            ));
        }
    }

    #[test_case("https://user:secret@decisions.example.test"; "credentials")]
    #[test_case("https://decisions.example.test/#secret"; "fragment")]
    #[test_case("https://decisions.example.test/?api_key=secret"; "query")]
    #[test_case("https://decisions.example.test/replacement"; "path_not_origin")]
    #[test_case("http://user:secret@decisions.example.test"; "http_credentials")]
    #[test_case("http://@decisions.example.test"; "http_empty_userinfo")]
    #[test_case("http://decisions.example.test/#secret"; "http_fragment")]
    #[test_case("http://decisions.example.test/?api_key=secret"; "http_query")]
    #[test_case("http://decisions.example.test/replacement"; "http_path_not_origin")]
    #[test_case("http://decisions.\texample.test"; "http_control")]
    #[test_case("http://decisions.example.test/ "; "http_whitespace")]
    #[test_case(""; "empty")]
    fn environment_override_cannot_smuggle_url_components(base_url: &str) {
        let raw = RawDecisionsConfig {
            endpoint: Some(LOCAL_ENDPOINT.into()),
            allow_remote: Some(true),
            allow_http: Some(true),
            ..RawDecisionsConfig::default()
        };
        assert!(raw.resolve(Some(base_url)).is_err());
    }

    #[test]
    fn environment_override_preserves_double_slash_path_without_reinterpreting_authority() {
        let raw = RawDecisionsConfig {
            endpoint: Some("http://127.0.0.1//other.example.test/predict".into()),
            ..RawDecisionsConfig::default()
        };
        let endpoint = raw
            .resolve(Some("http://127.0.0.2:8001"))
            .unwrap()
            .endpoint
            .unwrap();
        assert_eq!(endpoint.host_str(), Some("127.0.0.2"));
        assert_eq!(endpoint.path(), "//other.example.test/predict");
    }

    #[test_case("endpoint = 'https://decisions.example.test/v1/systemone'", "endpoint")]
    #[test_case("allow_remote = true", "allow_remote")]
    #[test_case("allow_http = true", "allow_http")]
    #[test_case("allow_http = false", "allow_http")]
    #[test_case("api_key_env = 'PROJECT_SECRET'", "api_key_env")]
    #[test_case("model = 'unreviewed'", "model")]
    #[test_case("timeout_ms = 999999", "timeout_ms")]
    #[test_case("log = true", "log")]
    #[test_case("log_retention_days = 365", "log_retention_days")]
    #[test_case("[decisions.thresholds]\nauto_flag = 1.0", "thresholds")]
    #[test_case(
        "[decisions.features]\nauto_screening = 'shadow'",
        "features.auto_screening"
    )]
    #[test_case(
        "[decisions.features]\nshell_duration = 'enforce'",
        "features.shell_duration"
    )]
    fn project_cannot_redirect_enable_or_weaken(global_only: &str, field: &str) {
        let mut global: RawConfig = toml::from_str(
            "[decisions]\nlog = true\n[decisions.features]\nauto_screening = 'enforce'",
        )
        .unwrap();
        let project: RawConfig = toml::from_str(&format!("[decisions]\n{global_only}")).unwrap();
        global.merge(project);
        assert!(
            matches!(global.into_config(false), Err(ConfigError::Decisions(DecisionsConfigError::ProjectOverride(actual))) if actual == field)
        );
    }

    #[test]
    fn project_only_cannot_activate_and_later_layers_cannot_clear_rejection() {
        let mut global = RawConfig::default();
        let project = toml::from_str("[decisions.features]\npermission_advice = 'shadow'").unwrap();
        global.merge(project);
        global.merge(RawConfig::default());
        assert!(global.into_config(false).is_err());
    }

    #[test]
    fn project_restrictions_preserve_unspecified_features() {
        let mut global: RawConfig = toml::from_str("[decisions]\nallow_remote = true\nallow_http = true\nlog = true\n[decisions.features]\npermission_advice = 'advise'\nauto_screening = 'enforce'").unwrap();
        let project = toml::from_str("[decisions]\nlog = false\nlog_retention_days = 7\n[decisions.features]\npermission_advice = 'off'").unwrap();
        global.merge(project);
        let config = global.into_config(false).unwrap().decisions;
        assert_eq!(config.features.permission_advice, FeatureMode::Off);
        assert_eq!(config.features.auto_screening, FeatureMode::Enforce);
        assert!(config.allow_remote);
        assert!(config.allow_http);
        assert!(!config.log);
        assert_eq!(config.log_retention_days, 7);
    }

    #[test_case("enforce", "off", "auto_screening = 'off'", true; "required_screening_disabled")]
    #[test_case("enforce", "advise", "content_screening = 'off'", true; "required_taint_protection_disabled")]
    #[test_case("enforce", "shadow", "content_screening = 'off'", false; "shadow_content_has_no_policy_effect")]
    #[test_case("enforce", "off", "content_screening = 'off'", false; "already_disabled_content")]
    #[test_case("enforce", "advise", "", false; "no_restriction")]
    #[test_case("off", "advise", "content_screening = 'off'", false; "global_auto_screening_off")]
    #[test_case("off", "off", "auto_screening = 'off'", false; "global_off_baseline")]
    #[test_case("shadow", "advise", "auto_screening = 'off'", false; "shadow_has_no_policy_effect")]
    fn project_disabling_required_screening_requires_auto_prompts(
        auto: &str,
        content: &str,
        project_features: &str,
        restricted: bool,
    ) {
        let mut global: RawConfig = toml::from_str(&format!(
            "[decisions.features]\nauto_screening = '{auto}'\ncontent_screening = '{content}'"
        ))
        .unwrap();
        let project = toml::from_str(&format!("[decisions.features]\n{project_features}")).unwrap();
        global.merge(project);
        global.merge(RawConfig::default());
        let config = global.into_config(false).unwrap().decisions;
        assert_eq!(config.auto_screening_restricted, restricted);
        if project_features.contains("auto_screening") {
            assert_eq!(config.features.auto_screening, FeatureMode::Off);
        }
        if project_features.contains("content_screening") {
            assert_eq!(config.features.content_screening, FeatureMode::Off);
        }
    }

    #[test]
    fn project_off_preserves_unconfigured_auto_baseline() {
        let mut global = RawConfig::default();
        global.merge(
            toml::from_str(
                "[decisions.features]\nauto_screening = 'off'\ncontent_screening = 'off'",
            )
            .unwrap(),
        );
        let config = global.into_config(false).unwrap().decisions;
        assert!(!config.auto_screening_restricted);
    }

    #[test_case("api_key_env = 'Bearer secret'", "api_key_env")]
    #[test_case("api_key_env = 'KEY=secret'", "api_key_env")]
    #[test_case("api_key_env = '9KEY'", "api_key_env")]
    #[test_case("api_key_env = ''", "api_key_env")]
    #[test_case("model = ''", "model")]
    #[test_case("timeout_ms = 0", "timeout_ms")]
    #[test_case("log_retention_days = 0", "log_retention_days")]
    #[test_case("[thresholds]\npermission_flag = nan", "thresholds.permission_flag")]
    #[test_case("[thresholds]\nauto_flag = inf", "thresholds.auto_flag")]
    #[test_case("[thresholds]\nshell_writes = 1.1", "thresholds.shell_writes")]
    #[test_case("[thresholds]\ngoal_skip_below = -0.1", "thresholds.goal_skip_below")]
    fn invalid_global_settings_are_rejected(source: &str, field: &str) {
        let raw: RawDecisionsConfig = toml::from_str(source).unwrap();
        assert!(
            matches!(raw.resolve(None), Err(DecisionsConfigError::Invalid { field: actual, .. }) if actual == field)
        );
    }

    #[test_case("api_key = 'secret'")]
    #[test_case("auto_screening_restricted = false")]
    #[test_case("[features]\nunknown = 'shadow'")]
    #[test_case("[thresholds]\nunknown = 0.1")]
    fn unknown_settings_are_not_silently_ignored(source: &str) {
        assert!(toml::from_str::<RawDecisionsConfig>(source).is_err());
    }
}
