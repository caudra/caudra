use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use crate::workcell::{WorkcellEndpoint, WorkcellEndpointError};
use crate::{ConfigField, ConfigValue};

pub const BASE_URL_ENV: &str = "TYPESAFE_BASE_URL";
pub const OPENAI_BASE_URL_ENV: &str = "OPENAI_BASE_URL";
pub const SYSTEM_ONE_PATH: &str = "/v1/systemone";
pub const OPENAI_DECISIONS_PATH: &str = "/decisions";
const DEFAULT_MODEL: &str = "jev-latest";
const DEFAULT_API_KEY_ENV: &str = "TYPESAFE_API_KEY";
const OPENAI_DEFAULT_MODEL: &str = "gpt-6-luna";
const OPENAI_DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";
const DEFAULT_TIMEOUT_MS: u64 = 800;
const DEFAULT_LOG_RETENTION_DAYS: u32 = 90;
const DEFAULT_FLAG_THRESHOLD: f64 = 0.85;
const DEFAULT_CONFIDENCE_THRESHOLD: f64 = 0.9;
const DEFAULT_GOAL_SKIP_BELOW: f64 = 0.05;
const DEFAULT_QUESTION_TOOL_NUDGE: f64 = 0.85;
const INVALID_BASE_URL_MESSAGE: &str = "must be an absolute HTTP(S) URL without credentials, query, fragment, whitespace or control characters";
const FULL_ENDPOINT_MESSAGE: &str =
    "Caudra appends /v1/systemone; set base_url to the part before it";
const OPENAI_FULL_ENDPOINT_MESSAGE: &str = "Caudra appends /decisions; set base_url to the versioned API base, such as https://api.openai.com/v1";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionProtocol {
    #[default]
    TypeSafe,
    OpenAI,
}

impl DecisionProtocol {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::TypeSafe => "typesafe",
            Self::OpenAI => "openai",
        }
    }

    pub const fn base_url_env(&self) -> &'static str {
        match self {
            Self::TypeSafe => BASE_URL_ENV,
            Self::OpenAI => OPENAI_BASE_URL_ENV,
        }
    }

    const fn default_model(&self) -> &'static str {
        match self {
            Self::TypeSafe => DEFAULT_MODEL,
            Self::OpenAI => OPENAI_DEFAULT_MODEL,
        }
    }

    const fn default_api_key_env(&self) -> &'static str {
        match self {
            Self::TypeSafe => DEFAULT_API_KEY_ENV,
            Self::OpenAI => OPENAI_DEFAULT_API_KEY_ENV,
        }
    }

    const fn endpoint_path(&self) -> &'static str {
        match self {
            Self::TypeSafe => SYSTEM_ONE_PATH,
            Self::OpenAI => OPENAI_DECISIONS_PATH,
        }
    }

    const fn full_endpoint_message(&self) -> &'static str {
        match self {
            Self::TypeSafe => FULL_ENDPOINT_MESSAGE,
            Self::OpenAI => OPENAI_FULL_ENDPOINT_MESSAGE,
        }
    }

    fn environment_error(&self, source: DecisionsConfigError) -> DecisionsConfigError {
        DecisionsConfigError::Environment {
            variable: self.base_url_env(),
            source: Box::new(source),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureMode {
    #[default]
    Off,
    Shadow,
    Advise,
    Enforce,
}

impl FeatureMode {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::Advise => "advise",
            Self::Enforce => "enforce",
        }
    }
}

/// One `[decisions.features]` key and the modes it accepts.
pub struct DecisionFeature {
    pub name: &'static str,
    pub modes: &'static [FeatureMode],
    /// What the feature does beyond `shadow`.
    pub description: &'static str,
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
    #[error("{variable} environment override rejected: {source}")]
    Environment {
        variable: &'static str,
        source: Box<DecisionsConfigError>,
    },
}

fn invalid(field: &'static str, message: &'static str) -> DecisionsConfigError {
    DecisionsConfigError::Invalid { field, message }
}

macro_rules! features {
    ($($field:ident: [$($mode:ident),+], $description:literal);+ $(;)?) => {
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
            pub const ALL: &[DecisionFeature] = &[$(DecisionFeature {
                name: stringify!($field),
                modes: &[FeatureMode::Off, FeatureMode::Shadow $(, FeatureMode::$mode)+],
                description: $description,
            }),+];

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
        "Add warnings to an existing permission prompt without delaying the answer.";
    auto_screening: [Enforce],
        "Escalate an eligible Auto call to a prompt on a flag or engine failure. Only `enforce` lets Auto run scripts and other lines that cannot be checked command by command. No answer channel means denial.";
    shell_effect: [Advise],
        "Warn about possible project writes during Plan review only when `shell_writes` is configured. Never establish read-only authority.";
    content_screening: [Advise],
        "Add caution to flagged web/MCP output and tighten upload/credential Auto screening for the session. Content remains available.";
    shell_duration: [Advise, Enforce],
        "Advise with local shell estimates. Enforce may fill an omitted timeout and select delivery at admission. Explicit timeouts stay unchanged.";
    tool_search: [Enforce],
        "Rerank the existing lexical tool shortlist. This neither loads arbitrary names nor grants execution permission.";
    skill_suggestions: [Advise],
        "Suggest a shortlisted skill. The agent still chooses whether to load it.";
    goal_prescreen: [Enforce],
        "Skip an unlikely-to-pass goal evaluation within the continuation budget and continue work. Only the normal evaluator can certify completion.";
    subagent_routing: [Enforce],
        "Choose a model job for a new unpinned subagent from its task label, mode, profile, and a redacted prompt excerpt. Explicit jobs, profile pins, and continuations keep their routing.";
    question_tool_nudge: [Advise],
        "At a main-session handoff with a usable question tool, advise adds one visible reminder per user-input episode to ask a live user question through that tool. Shadow only evaluates. Uses a redacted, bounded request and reply excerpt; uncertainty and failures leave the reply unchanged.";
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DecisionThresholds {
    pub permission_flag: f64,
    pub auto_flag: f64,
    pub content_injection: f64,
    pub content_addressed_to_agent: f64,
    pub shell_endless: f64,
    pub shell_duration: f64,
    pub routing_confidence: f64,
    pub goal_skip_below: f64,
    pub question_tool_nudge: f64,
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
            shell_duration: DEFAULT_CONFIDENCE_THRESHOLD,
            routing_confidence: DEFAULT_CONFIDENCE_THRESHOLD,
            goal_skip_below: DEFAULT_GOAL_SKIP_BELOW,
            question_tool_nudge: DEFAULT_QUESTION_TOOL_NUDGE,
            shell_writes: None,
        }
    }
}

impl DecisionThresholds {
    pub const FIELDS: &[ConfigField] = &[
        ConfigField {
            name: "permission_flag",
            ty: "float",
            default: ConfigValue::F64(DEFAULT_FLAG_THRESHOLD),
            min: None,
            max: None,
            env: None,
            description: "Probability for a permission warning.",
        },
        ConfigField {
            name: "auto_flag",
            ty: "float",
            default: ConfigValue::F64(DEFAULT_FLAG_THRESHOLD),
            min: None,
            max: None,
            env: None,
            description: "Probability for escalating an eligible Auto call.",
        },
        ConfigField {
            name: "content_injection",
            ty: "float",
            default: ConfigValue::F64(DEFAULT_CONFIDENCE_THRESHOLD),
            min: None,
            max: None,
            env: None,
            description: "Probability that sampled content attempts instruction injection.",
        },
        ConfigField {
            name: "content_addressed_to_agent",
            ty: "float",
            default: ConfigValue::F64(DEFAULT_CONFIDENCE_THRESHOLD),
            min: None,
            max: None,
            env: None,
            description: "Probability that sampled content addresses the agent.",
        },
        ConfigField {
            name: "shell_endless",
            ty: "float",
            default: ConfigValue::F64(DEFAULT_CONFIDENCE_THRESHOLD),
            min: None,
            max: None,
            env: None,
            description: "Probability that a shell command runs until stopped.",
        },
        ConfigField {
            name: "shell_duration",
            ty: "float",
            default: ConfigValue::F64(DEFAULT_CONFIDENCE_THRESHOLD),
            min: None,
            max: None,
            env: None,
            description: "Probability mass a duration bound needs before an engine estimate is used.",
        },
        ConfigField {
            name: "routing_confidence",
            ty: "float",
            default: ConfigValue::F64(DEFAULT_CONFIDENCE_THRESHOLD),
            min: None,
            max: None,
            env: None,
            description: "Confidence required for tool search and skill suggestions. Tool-search choice probability must also meet it. Subagent routing picks the Fast model when this much difficulty probability is at or below routine work, and the Best model when this much is on open-ended work.",
        },
        ConfigField {
            name: "goal_skip_below",
            ty: "float",
            default: ConfigValue::F64(DEFAULT_GOAL_SKIP_BELOW),
            min: None,
            max: None,
            env: None,
            description: "Skip an evaluator at or below this completion probability, within the continuation budget.",
        },
        ConfigField {
            name: "shell_writes",
            ty: "float",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "Optional project-write warning threshold. Omission leaves the warning disabled. No built-in enforcement threshold.",
        },
        ConfigField {
            name: "question_tool_nudge",
            ty: "float",
            default: ConfigValue::F64(DEFAULT_QUESTION_TOOL_NUDGE),
            min: None,
            max: None,
            env: None,
            description: "Minimum noul score for a question-tool reminder. Provisional, not a calibrated probability; advise only, and errors or uncertainty never reopen the turn.",
        },
    ];

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
            ("thresholds.shell_duration", Some(self.shell_duration)),
            (
                "thresholds.routing_confidence",
                Some(self.routing_confidence),
            ),
            ("thresholds.goal_skip_below", Some(self.goal_skip_below)),
            (
                "thresholds.question_tool_nudge",
                Some(self.question_tool_nudge),
            ),
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
    pub base_url: Option<String>,
    pub protocol: Option<DecisionProtocol>,
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
    pub base_url: Option<Url>,
    pub protocol: DecisionProtocol,
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
            base_url: None,
            protocol: DecisionProtocol::default(),
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
            base_url,
            protocol,
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
            ("base_url", overlay.base_url.is_some()),
            ("protocol", overlay.protocol.is_some()),
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

    /// `base_url_override` replaces a configured base URL, path included. It
    /// never supplies one on its own.
    pub fn resolve(
        self,
        base_url_override: Option<&str>,
    ) -> Result<DecisionsConfig, DecisionsConfigError> {
        if let Some(error) = self.project_error {
            return Err(error);
        }
        let allow_remote = self.allow_remote.unwrap_or(false);
        let allow_http = self.allow_http.unwrap_or(false);
        let protocol = self.protocol.unwrap_or_default();
        let base_url = self
            .base_url
            .as_deref()
            .map(|configured| {
                let configured = parse_base_url(configured, &protocol, allow_remote, allow_http)?;
                base_url_override.map_or(Ok(configured), |value| {
                    parse_base_url(value, &protocol, allow_remote, allow_http)
                        .map_err(|error| protocol.environment_error(error))
                })
            })
            .transpose()?;
        let config = DecisionsConfig {
            base_url,
            model: self
                .model
                .unwrap_or_else(|| protocol.default_model().into()),
            api_key_env: self
                .api_key_env
                .unwrap_or_else(|| protocol.default_api_key_env().into()),
            protocol,
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
        if self.base_url.is_none() {
            return self.resolve(None);
        }
        let protocol = self.protocol.clone().unwrap_or_default();
        let base_url = crate::global_env_value(protocol.base_url_env())
            .map_err(|_| protocol.environment_error(invalid("base_url", "must be UTF-8")))?;
        self.resolve(base_url.as_deref())
    }
}

impl DecisionsConfig {
    pub const FIELDS: &[ConfigField] = &[
        ConfigField {
            name: "protocol",
            ty: "string",
            default: ConfigValue::Str("typesafe"),
            min: None,
            max: None,
            env: None,
            description: "Global-only decision wire protocol: `typesafe` or `openai`. Never inferred from the URL, model, or environment. Selecting a protocol does not enable decisions.",
        },
        ConfigField {
            name: "base_url",
            ty: "string",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "Global-only decision API base URL. TypeSafe uses a root such as `https://api.typesafe.ai` and appends `/v1/systemone`; OpenAI uses a versioned base such as `https://api.openai.com/v1` and appends `/decisions`. Path prefixes are preserved; full endpoints are rejected. Only the selected protocol's `TYPESAFE_BASE_URL` or `OPENAI_BASE_URL`, from the process or global environment, replaces an explicitly configured base. Environment alone never enables decisions. HTTPS required except for numeric loopback HTTP or explicit `allow_http` consent. No credentials, query, fragment, whitespace, or control characters.",
        },
        ConfigField {
            name: "model",
            ty: "string",
            default: ConfigValue::Varies("`jev-latest` for typesafe; `gpt-6-luna` for openai"),
            min: None,
            max: None,
            env: None,
            description: "Decision model identifier, nonblank and without control characters.",
        },
        ConfigField {
            name: "api_key_env",
            ty: "string",
            default: ConfigValue::Varies(
                "`TYPESAFE_API_KEY` for typesafe; `OPENAI_API_KEY` for openai",
            ),
            min: None,
            max: None,
            env: None,
            description: "Environment variable containing the optional credential, never the credential itself. Project environment values are excluded.",
        },
        ConfigField {
            name: "allow_remote",
            ty: "boolean",
            default: ConfigValue::Bool(false),
            min: None,
            max: None,
            env: None,
            description: "Explicit global consent to send decision context to a non-loopback endpoint.",
        },
        ConfigField {
            name: "allow_http",
            ty: "boolean",
            default: ConfigValue::Bool(false),
            min: None,
            max: None,
            env: None,
            description: "Global-only opt-in for non-loopback HTTP. Also requires `allow_remote = true`. Use only with transport protection you control, such as a trusted encrypted tunnel.",
        },
        ConfigField {
            name: "timeout_ms",
            ty: "integer",
            default: ConfigValue::U64(DEFAULT_TIMEOUT_MS),
            min: None,
            max: None,
            env: None,
            description: "Positive decision-request deadline in milliseconds, separate from shell execution timeouts.",
        },
        ConfigField {
            name: "log",
            ty: "boolean",
            default: ConfigValue::Bool(false),
            min: None,
            max: None,
            env: None,
            description: "Retain bounded decision records in the local `caudra.db`.",
        },
        ConfigField {
            name: "log_retention_days",
            ty: "integer",
            default: ConfigValue::U64(DEFAULT_LOG_RETENTION_DAYS as u64),
            min: None,
            max: None,
            env: None,
            description: "Positive retention period for decision records.",
        },
        ConfigField {
            name: "features",
            ty: "table",
            default: ConfigValue::Toml("{}"),
            min: None,
            max: None,
            env: None,
            description: "Per-feature modes below.",
        },
        ConfigField {
            name: "thresholds",
            ty: "table",
            default: ConfigValue::Toml("{}"),
            min: None,
            max: None,
            env: None,
            description: "Probability thresholds, all finite and within 0–1 inclusive.",
        },
    ];

    pub fn endpoint(&self) -> Option<Url> {
        self.base_url.as_ref().map(|base_url| {
            let mut endpoint = base_url.clone();
            endpoint.set_path(&format!(
                "{}{}",
                base_url.path().trim_end_matches('/'),
                self.protocol.endpoint_path()
            ));
            endpoint
        })
    }

    pub fn validate(&self) -> Result<(), DecisionsConfigError> {
        if let Some(base_url) = &self.base_url {
            parse_base_url(
                base_url.as_str(),
                &self.protocol,
                self.allow_remote,
                self.allow_http,
            )?;
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

fn parse_base_url(
    base_url: &str,
    protocol: &DecisionProtocol,
    allow_remote: bool,
    allow_http: bool,
) -> Result<Url, DecisionsConfigError> {
    if base_url
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
        || !base_url
            .split_once(':')
            .and_then(|(_, authority)| authority.strip_prefix("//"))
            .is_some_and(|authority| !authority.starts_with(['/', '\\']))
    {
        return Err(invalid("base_url", INVALID_BASE_URL_MESSAGE));
    }
    let (url, is_loopback) = match WorkcellEndpoint::parse(base_url) {
        Ok(endpoint) => (endpoint.as_url().clone(), endpoint.is_loopback()),
        Err(WorkcellEndpointError::InsecureRemote) => (
            Url::parse(base_url).map_err(|_| invalid("base_url", INVALID_BASE_URL_MESSAGE))?,
            false,
        ),
        Err(_) => return Err(invalid("base_url", INVALID_BASE_URL_MESSAGE)),
    };
    if url
        .path()
        .trim_end_matches('/')
        .ends_with(protocol.endpoint_path())
    {
        return Err(invalid("base_url", protocol.full_endpoint_message()));
    }
    if !is_loopback && !allow_remote {
        return Err(invalid(
            "allow_remote",
            "non-loopback endpoints send data off-machine; set allow_remote = true in global config to opt in",
        ));
    }
    if !is_loopback && url.scheme() == "http" && !allow_http {
        return Err(invalid(
            "allow_http",
            "non-loopback HTTP endpoints send data and credentials without TLS; set allow_http = true in global config to opt in",
        ));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::{
        DecisionProtocol, DecisionsConfig, DecisionsConfigError, FULL_ENDPOINT_MESSAGE,
        FeatureMode, INVALID_BASE_URL_MESSAGE, OPENAI_FULL_ENDPOINT_MESSAGE, RawDecisionsConfig,
        invalid,
    };
    use crate::{ConfigError, RawConfig};
    use test_case::test_case;
    use url::Url;

    const LOCAL_BASE_URL: &str = "http://127.0.0.1:8000";
    const LOCAL_ENDPOINT: &str = "http://127.0.0.1:8000/v1/systemone";
    const PREFIXED_BASE_URL: &str = "http://127.0.0.1:8080/typesafe";
    const PREFIXED_ENDPOINT: &str = "http://127.0.0.1:8080/typesafe/v1/systemone";
    const TAILNET_BASE_URL: &str = "http://100.64.0.3:8080/typesafe";
    const TAILNET_ENDPOINT: &str = "http://100.64.0.3:8080/typesafe/v1/systemone";
    const REMOTE_BASE_URL: &str = "https://decisions.example.test";
    const REMOTE_ENDPOINT: &str = "https://decisions.example.test/v1/systemone";
    const REMOTE_HTTP_BASE_URL: &str = "http://decisions.example.test:443";
    const REMOTE_HTTP_ENDPOINT: &str = "http://decisions.example.test:443/v1/systemone";
    const BASE_URL_SIZE_LIMIT: usize = 2048;
    const REMOVED_ENDPOINT_MESSAGE: &str = "unknown field `endpoint`, expected one of `base_url`";
    const CUSTOM_MODEL: &str = "custom-model";
    const CUSTOM_API_KEY_ENV: &str = "CUSTOM_DECISION_API_KEY";

    fn environment_error_field(error: DecisionsConfigError) -> Option<&'static str> {
        match error {
            DecisionsConfigError::Environment { source, .. } => match *source {
                DecisionsConfigError::Invalid { field, .. } => Some(field),
                _ => None,
            },
            _ => None,
        }
    }

    #[test]
    fn defaults_and_environment_never_activate_decisions() {
        let config = RawDecisionsConfig::default()
            .resolve(Some(REMOTE_BASE_URL))
            .unwrap();
        assert_eq!(config, DecisionsConfig::default());
        assert!(!config.features.any_enabled());
        assert!(!config.allow_remote);
        assert!(!config.allow_http);
        assert!(!config.log);
        assert!(config.thresholds.shell_writes.is_none());
        assert_eq!(config.features.question_tool_nudge, FeatureMode::Off);
        assert_eq!(
            config.thresholds.question_tool_nudge,
            super::DEFAULT_QUESTION_TOOL_NUDGE
        );
        assert!(
            RawConfig::default()
                .into_config(false)
                .unwrap()
                .decisions
                .base_url
                .is_none()
        );
    }

    #[test_case("", DecisionProtocol::TypeSafe, "jev-latest", "TYPESAFE_API_KEY"; "omitted")]
    #[test_case("protocol = 'typesafe'", DecisionProtocol::TypeSafe, "jev-latest", "TYPESAFE_API_KEY"; "typesafe")]
    #[test_case("protocol = 'openai'", DecisionProtocol::OpenAI, "gpt-6-luna", "OPENAI_API_KEY"; "openai")]
    fn protocol_defaults_never_activate_decisions(
        source: &str,
        protocol: DecisionProtocol,
        model: &str,
        api_key_env: &str,
    ) {
        let raw: RawDecisionsConfig = toml::from_str(source).unwrap();
        let config = raw.resolve(Some(REMOTE_BASE_URL)).unwrap();
        assert_eq!(config.protocol, protocol);
        assert_eq!(config.model, model);
        assert_eq!(config.api_key_env, api_key_env);
        assert!(config.endpoint().is_none());
        assert!(!config.features.any_enabled());
        assert!(!config.allow_remote);
        assert!(!config.allow_http);
        assert!(!config.log);
        assert_eq!(serde_json::to_value(&protocol).unwrap(), protocol.as_str());
    }

    #[test_case("typesafe"; "typesafe")]
    #[test_case("openai"; "openai")]
    fn protocol_preserves_explicit_model_and_credential_name(protocol: &str) {
        let raw: RawDecisionsConfig = toml::from_str(&format!(
            "protocol = '{protocol}'\nmodel = '{CUSTOM_MODEL}'\napi_key_env = '{CUSTOM_API_KEY_ENV}'"
        ))
        .unwrap();
        let config = raw.resolve(None).unwrap();
        assert_eq!(config.model, CUSTOM_MODEL);
        assert_eq!(config.api_key_env, CUSTOM_API_KEY_ENV);
    }

    #[test_case("unknown")]
    #[test_case("OpenAI")]
    #[test_case("type_safe")]
    #[test_case("")]
    fn unknown_protocols_are_rejected(protocol: &str) {
        assert!(toml::from_str::<RawDecisionsConfig>(&format!("protocol = '{protocol}'")).is_err());
    }

    #[test_case("typesafe", "openai", "gpt-6-luna", "OPENAI_API_KEY")]
    #[test_case("openai", "typesafe", "jev-latest", "TYPESAFE_API_KEY")]
    fn global_protocol_overlay_resolves_omitted_defaults(
        initial: &str,
        overlay: &str,
        model: &str,
        api_key_env: &str,
    ) {
        let mut global: RawConfig =
            toml::from_str(&format!("[decisions]\nprotocol = '{initial}'")).unwrap();
        global
            .merge_global(toml::from_str(&format!("[decisions]\nprotocol = '{overlay}'")).unwrap());
        let config = global.into_config(false).unwrap().decisions;
        assert_eq!(config.protocol.as_str(), overlay);
        assert_eq!(config.model, model);
        assert_eq!(config.api_key_env, api_key_env);
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
    #[test_case("question_tool_nudge", true, false)]
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

    #[test_case(FeatureMode::Off)]
    #[test_case(FeatureMode::Shadow)]
    #[test_case(FeatureMode::Advise)]
    #[test_case(FeatureMode::Enforce)]
    fn feature_mode_names_match_their_config_spelling(mode: FeatureMode) {
        assert_eq!(serde_json::to_value(&mode).unwrap(), mode.as_str());
    }

    #[test_case(LOCAL_BASE_URL, false, true; "numeric_loopback")]
    #[test_case("http://[::1]:8000", false, true; "ipv6_loopback")]
    #[test_case(PREFIXED_BASE_URL, false, true; "path_prefix")]
    #[test_case(REMOTE_BASE_URL, false, false; "remote_requires_opt_in")]
    #[test_case(REMOTE_BASE_URL, true, true; "remote_opt_in")]
    #[test_case("http://decisions.example.test", true, false; "remote_requires_tls")]
    #[test_case("https://localhost", false, false; "dns_is_not_loopback")]
    #[test_case("http://127.0.0.1.evil.test", false, false; "loopback_lookalike")]
    #[test_case("https://user:secret@decisions.example.test", true, false; "credentials")]
    #[test_case("https://@decisions.example.test", true, false; "empty_userinfo")]
    #[test_case("https://decisions.example.test#secret", true, false; "fragment")]
    #[test_case("https://decisions.example.test?api_key=secret", true, false; "query")]
    #[test_case("file:///etc/passwd", true, false; "unsupported_scheme")]
    fn base_url_policy(base_url: &str, allow_remote: bool, valid: bool) {
        for protocol in [DecisionProtocol::TypeSafe, DecisionProtocol::OpenAI] {
            let raw = RawDecisionsConfig {
                base_url: Some(base_url.into()),
                protocol: Some(protocol),
                allow_remote: Some(allow_remote),
                ..RawDecisionsConfig::default()
            };
            assert_eq!(raw.resolve(None).is_ok(), valid);
        }
    }

    #[test_case(DecisionProtocol::TypeSafe, LOCAL_ENDPOINT, FULL_ENDPOINT_MESSAGE; "origin")]
    #[test_case(DecisionProtocol::TypeSafe, PREFIXED_ENDPOINT, FULL_ENDPOINT_MESSAGE; "prefix")]
    #[test_case(DecisionProtocol::TypeSafe, "http://127.0.0.1:8080/typesafe/v1/systemone/", FULL_ENDPOINT_MESSAGE; "trailing_slash")]
    #[test_case(DecisionProtocol::OpenAI, "https://api.openai.com/v1/decisions", OPENAI_FULL_ENDPOINT_MESSAGE; "openai")]
    #[test_case(DecisionProtocol::OpenAI, "http://127.0.0.1/proxy/v1/decisions/", OPENAI_FULL_ENDPOINT_MESSAGE; "openai_prefix_trailing_slash")]
    fn base_url_is_not_the_full_endpoint(
        protocol: DecisionProtocol,
        base_url: &str,
        message: &'static str,
    ) {
        let raw = RawDecisionsConfig {
            base_url: Some(base_url.into()),
            protocol: Some(protocol),
            allow_remote: Some(true),
            ..RawDecisionsConfig::default()
        };
        assert_eq!(raw.clone().resolve(None), Err(invalid("base_url", message)));
        let mut override_raw = raw;
        override_raw.base_url = Some(LOCAL_BASE_URL.into());
        let error = override_raw.resolve(Some(base_url)).unwrap_err();
        assert!(matches!(
            error,
            DecisionsConfigError::Environment { source, .. }
                if *source == invalid("base_url", message)
        ));
    }

    #[test_case(DecisionProtocol::TypeSafe, LOCAL_BASE_URL, LOCAL_ENDPOINT; "origin")]
    #[test_case(DecisionProtocol::TypeSafe, "http://127.0.0.1:8000/", LOCAL_ENDPOINT; "origin_trailing_slash")]
    #[test_case(DecisionProtocol::TypeSafe, PREFIXED_BASE_URL, PREFIXED_ENDPOINT; "prefix")]
    #[test_case(DecisionProtocol::TypeSafe, "http://127.0.0.1:8080/typesafe/", PREFIXED_ENDPOINT; "prefix_trailing_slash")]
    #[test_case(DecisionProtocol::TypeSafe, "https://api.typesafe.ai", "https://api.typesafe.ai/v1/systemone"; "hosted")]
    #[test_case(DecisionProtocol::OpenAI, "https://api.openai.com/v1", "https://api.openai.com/v1/decisions"; "openai")]
    #[test_case(DecisionProtocol::OpenAI, "http://127.0.0.1:8000/proxy/v1///", "http://127.0.0.1:8000/proxy/v1/decisions"; "openai_prefix")]
    #[test_case(DecisionProtocol::OpenAI, "http://[::1]:8000/v1/", "http://[::1]:8000/v1/decisions"; "openai_ipv6")]
    #[test_case(DecisionProtocol::TypeSafe, "https://api.openai.com/v1", "https://api.openai.com/v1/v1/systemone"; "url_does_not_select_protocol")]
    #[test_case(DecisionProtocol::OpenAI, "https://api.typesafe.ai", "https://api.typesafe.ai/decisions"; "url_does_not_change_protocol")]
    fn endpoint_appends_the_protocol_path(
        protocol: DecisionProtocol,
        base_url: &str,
        endpoint: &str,
    ) {
        let raw = RawDecisionsConfig {
            base_url: Some(base_url.into()),
            protocol: Some(protocol),
            allow_remote: Some(true),
            ..RawDecisionsConfig::default()
        };
        for environment in [None, Some(base_url)] {
            let config = raw.clone().resolve(environment).unwrap();
            assert_eq!(config.endpoint().unwrap().as_str(), endpoint);
        }
    }

    #[test_case(false, false, Some("allow_remote"); "no_consent")]
    #[test_case(false, true, Some("allow_remote"); "only_http_consent")]
    #[test_case(true, false, Some("allow_http"); "only_remote_consent")]
    #[test_case(true, true, None; "both_consents")]
    fn openai_remote_http_requires_both_consents(
        allow_remote: bool,
        allow_http: bool,
        error_field: Option<&str>,
    ) {
        for environment in [None, Some(REMOTE_HTTP_BASE_URL)] {
            let raw = RawDecisionsConfig {
                protocol: Some(DecisionProtocol::OpenAI),
                base_url: Some(if environment.is_some() {
                    LOCAL_BASE_URL.into()
                } else {
                    REMOTE_HTTP_BASE_URL.into()
                }),
                allow_remote: Some(allow_remote),
                allow_http: Some(allow_http),
                ..RawDecisionsConfig::default()
            };
            let result = raw.resolve(environment);
            match error_field {
                Some(expected) => {
                    let error = result.unwrap_err();
                    let field = match error {
                        DecisionsConfigError::Invalid { field, .. } => Some(field),
                        error => environment_error_field(error),
                    };
                    assert_eq!(field, Some(expected));
                }
                None => assert_eq!(
                    result.unwrap().endpoint().unwrap().as_str(),
                    format!("{REMOTE_HTTP_BASE_URL}/decisions")
                ),
            }
        }
    }

    #[test_case(REMOTE_HTTP_BASE_URL; "dns")]
    #[test_case("http://decisions.example.test:80"; "default_port")]
    #[test_case("http://192.0.2.1:8000"; "ipv4")]
    #[test_case("http://[2001:db8::1]:8000"; "ipv6")]
    #[test_case("http://100.64.0.1:8000"; "cgnat_start")]
    #[test_case("http://100.127.255.254:8000"; "cgnat_end")]
    #[test_case(TAILNET_BASE_URL; "cgnat_prefix")]
    #[test_case("http://localhost:8000"; "localhost_is_dns")]
    #[test_case("http://127.0.0.1.evil.test"; "loopback_lookalike")]
    #[test_case("HTTP://decisions.example.test:443"; "uppercase_scheme")]
    fn remote_http_requires_both_global_opt_ins(base_url: &str) {
        for (allow_remote, allow_http, error_field) in [
            (false, false, Some("allow_remote")),
            (false, true, Some("allow_remote")),
            (true, false, Some("allow_http")),
            (true, true, None),
        ] {
            let raw = RawDecisionsConfig {
                base_url: Some(base_url.into()),
                allow_remote: Some(allow_remote),
                allow_http: Some(allow_http),
                ..RawDecisionsConfig::default()
            };
            let expected = DecisionsConfig {
                base_url: Some(Url::parse(base_url).unwrap()),
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
    fn http_opt_in_does_not_relax_url_validation(base_url: &str) {
        for protocol in [DecisionProtocol::TypeSafe, DecisionProtocol::OpenAI] {
            let raw = RawDecisionsConfig {
                base_url: Some(base_url.into()),
                protocol: Some(protocol),
                allow_remote: Some(true),
                allow_http: Some(true),
                ..RawDecisionsConfig::default()
            };
            assert_eq!(
                raw.resolve(None),
                Err(invalid("base_url", INVALID_BASE_URL_MESSAGE))
            );
        }
    }

    #[test_case(BASE_URL_SIZE_LIMIT, true; "at_limit")]
    #[test_case(BASE_URL_SIZE_LIMIT + 1, false; "over_limit")]
    fn http_opt_in_preserves_base_url_size_limit(length: usize, valid: bool) {
        let prefix = format!("{REMOTE_HTTP_BASE_URL}/");
        let base_url = format!("{prefix}{}", "a".repeat(length - prefix.len()));
        let raw = RawDecisionsConfig {
            base_url: Some(base_url),
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
        for (base_url, endpoint) in [
            (LOCAL_BASE_URL, LOCAL_ENDPOINT),
            (PREFIXED_BASE_URL, PREFIXED_ENDPOINT),
            ("http://[::1]:8000", "http://[::1]:8000/v1/systemone"),
        ] {
            let raw: RawDecisionsConfig =
                toml::from_str(&format!("base_url = '{base_url}'\n{setting}")).unwrap();
            let config = raw.resolve(None).unwrap();
            assert_eq!(config.endpoint().unwrap().as_str(), endpoint);
            assert!(!config.allow_remote);
        }
    }

    #[test_case(LOCAL_BASE_URL, TAILNET_BASE_URL, TAILNET_ENDPOINT; "adds_prefix")]
    #[test_case(PREFIXED_BASE_URL, LOCAL_BASE_URL, LOCAL_ENDPOINT; "drops_prefix")]
    #[test_case(REMOTE_BASE_URL, PREFIXED_BASE_URL, PREFIXED_ENDPOINT; "replaces_host")]
    fn environment_replaces_the_whole_base_url(
        configured: &str,
        environment: &str,
        endpoint: &str,
    ) {
        let raw = RawDecisionsConfig {
            base_url: Some(configured.into()),
            allow_remote: Some(true),
            allow_http: Some(true),
            ..RawDecisionsConfig::default()
        };
        let config = raw.resolve(Some(environment)).unwrap();
        assert_eq!(config.endpoint().unwrap().as_str(), endpoint);
    }

    #[test_case(false; "remote_redirect_rejected")]
    #[test_case(true; "remote_redirect_opted_in")]
    fn environment_base_url_is_revalidated(allow_remote: bool) {
        let raw = RawDecisionsConfig {
            base_url: Some(LOCAL_BASE_URL.into()),
            allow_remote: Some(allow_remote),
            ..RawDecisionsConfig::default()
        };
        let result = raw.resolve(Some(REMOTE_BASE_URL));
        if allow_remote {
            assert_eq!(
                result.unwrap().endpoint().unwrap().as_str(),
                REMOTE_ENDPOINT
            );
        } else {
            assert_eq!(
                environment_error_field(result.unwrap_err()),
                Some("allow_remote")
            );
        }
    }

    #[test_case(LOCAL_BASE_URL; "local_http")]
    #[test_case("https://127.0.0.1:8000"; "local_https")]
    fn environment_remote_http_requires_both_opt_ins(base_url: &str) {
        for (allow_remote, allow_http, error_field) in [
            (false, false, Some("allow_remote")),
            (false, true, Some("allow_remote")),
            (true, false, Some("allow_http")),
            (true, true, None),
        ] {
            let raw = RawDecisionsConfig {
                base_url: Some(base_url.into()),
                allow_remote: Some(allow_remote),
                allow_http: Some(allow_http),
                ..RawDecisionsConfig::default()
            };
            let result = raw.resolve(Some(REMOTE_HTTP_BASE_URL));
            match error_field {
                Some(field) => {
                    assert_eq!(environment_error_field(result.unwrap_err()), Some(field));
                }
                None => assert_eq!(
                    result.unwrap().endpoint().unwrap().as_str(),
                    REMOTE_HTTP_ENDPOINT
                ),
            }
        }
    }

    #[test_case(false; "https_downgrade_rejected")]
    #[test_case(true; "https_downgrade_opted_in")]
    fn environment_https_downgrade_requires_http_opt_in(allow_http: bool) {
        let raw = RawDecisionsConfig {
            base_url: Some(REMOTE_BASE_URL.into()),
            allow_remote: Some(true),
            allow_http: Some(allow_http),
            ..RawDecisionsConfig::default()
        };
        let result = raw.resolve(Some(REMOTE_HTTP_BASE_URL));
        if allow_http {
            assert_eq!(
                result.unwrap().endpoint().unwrap().as_str(),
                REMOTE_HTTP_ENDPOINT
            );
        } else {
            assert_eq!(
                environment_error_field(result.unwrap_err()),
                Some("allow_http")
            );
        }
    }

    #[test_case("https://user:secret@decisions.example.test"; "credentials")]
    #[test_case("https://decisions.example.test/#secret"; "fragment")]
    #[test_case("https://decisions.example.test/?api_key=secret"; "query")]
    #[test_case("http://user:secret@decisions.example.test"; "http_credentials")]
    #[test_case("http://@decisions.example.test"; "http_empty_userinfo")]
    #[test_case("http://decisions.example.test/#secret"; "http_fragment")]
    #[test_case("http://decisions.example.test/?api_key=secret"; "http_query")]
    #[test_case("http://decisions.\texample.test"; "http_control")]
    #[test_case("http://decisions.example.test/ "; "http_whitespace")]
    #[test_case(""; "empty")]
    fn environment_override_cannot_smuggle_url_components(base_url: &str) {
        for protocol in [DecisionProtocol::TypeSafe, DecisionProtocol::OpenAI] {
            let raw = RawDecisionsConfig {
                base_url: Some(LOCAL_BASE_URL.into()),
                protocol: Some(protocol),
                allow_remote: Some(true),
                allow_http: Some(true),
                ..RawDecisionsConfig::default()
            };
            assert_eq!(
                environment_error_field(raw.resolve(Some(base_url)).unwrap_err()),
                Some("base_url")
            );
        }
    }

    #[test_case(DecisionProtocol::TypeSafe, "http://127.0.0.1//other.example.test/predict", None, "127.0.0.1", "//other.example.test/predict/v1/systemone"; "configured")]
    #[test_case(DecisionProtocol::TypeSafe, LOCAL_BASE_URL, Some("http://127.0.0.2:8001//other.example.test"), "127.0.0.2", "//other.example.test/v1/systemone"; "environment")]
    #[test_case(DecisionProtocol::OpenAI, "http://127.0.0.1//other.example.test/v1", None, "127.0.0.1", "//other.example.test/v1/decisions"; "openai_configured")]
    #[test_case(DecisionProtocol::OpenAI, LOCAL_BASE_URL, Some("http://127.0.0.2:8001//other.example.test/v1"), "127.0.0.2", "//other.example.test/v1/decisions"; "openai_environment")]
    fn double_slash_paths_never_reinterpret_the_authority(
        protocol: DecisionProtocol,
        base_url: &str,
        environment: Option<&str>,
        host: &str,
        path: &str,
    ) {
        let raw = RawDecisionsConfig {
            base_url: Some(base_url.into()),
            protocol: Some(protocol),
            ..RawDecisionsConfig::default()
        };
        let endpoint = raw.resolve(environment).unwrap().endpoint().unwrap();
        assert_eq!(endpoint.host_str(), Some(host));
        assert_eq!(endpoint.path(), path);
    }

    #[test_case("base_url = 'https://decisions.example.test'", "base_url")]
    #[test_case("protocol = 'typesafe'", "protocol")]
    #[test_case("protocol = 'openai'", "protocol")]
    #[test_case("allow_remote = true", "allow_remote")]
    #[test_case("allow_http = true", "allow_http")]
    #[test_case("allow_http = false", "allow_http")]
    #[test_case("api_key_env = 'PROJECT_SECRET'", "api_key_env")]
    #[test_case("model = 'unreviewed'", "model")]
    #[test_case("timeout_ms = 999999", "timeout_ms")]
    #[test_case("log = true", "log")]
    #[test_case("log_retention_days = 365", "log_retention_days")]
    #[test_case("[decisions.thresholds]\nauto_flag = 1.0", "thresholds")]
    #[test_case("[decisions.thresholds]\nquestion_tool_nudge = 0.5", "thresholds")]
    #[test_case(
        "[decisions.features]\nquestion_tool_nudge = 'advise'",
        "features.question_tool_nudge"
    )]
    #[test_case(
        "[decisions.features]\nquestion_tool_nudge = 'shadow'",
        "features.question_tool_nudge"
    )]
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

    #[test_case("off", FeatureMode::Off; "project_disables")]
    #[test_case("", FeatureMode::Advise; "project_preserves")]
    fn question_tool_nudge_project_can_only_disable(mode: &str, expected: FeatureMode) {
        let mut global: RawConfig =
            toml::from_str("[decisions.features]\nquestion_tool_nudge = 'advise'").unwrap();
        let source = if mode.is_empty() {
            String::new()
        } else {
            format!("[decisions.features]\nquestion_tool_nudge = '{mode}'")
        };
        global.merge(toml::from_str(&source).unwrap());
        assert_eq!(
            global
                .into_config(false)
                .unwrap()
                .decisions
                .features
                .question_tool_nudge,
            expected
        );
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
    #[test_case(
        "[thresholds]\nquestion_tool_nudge = -0.1",
        "thresholds.question_tool_nudge"
    )]
    #[test_case(
        "[thresholds]\nquestion_tool_nudge = 1.1",
        "thresholds.question_tool_nudge"
    )]
    #[test_case(
        "[thresholds]\nquestion_tool_nudge = nan",
        "thresholds.question_tool_nudge"
    )]
    #[test_case(
        "[thresholds]\nquestion_tool_nudge = inf",
        "thresholds.question_tool_nudge"
    )]
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

    #[test]
    fn removed_endpoint_key_points_to_base_url() {
        let error =
            toml::from_str::<RawConfig>(&format!("[decisions]\nendpoint = '{LOCAL_ENDPOINT}'"))
                .unwrap_err();
        assert!(error.to_string().contains(REMOVED_ENDPOINT_MESSAGE));
    }
}
