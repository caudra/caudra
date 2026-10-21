use std::fmt;

use caudra_storage::auth::{NameError, validate_name};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use crate::workcell::{WorkcellEndpoint, WorkcellEndpointError};
use crate::{ConfigField, ConfigValue};

/// The `providers.toml` table holding named decision endpoints.
pub const DECISIONS_TABLE: &str = "decisions";
pub const SYSTEM_ONE_PATH: &str = "/v1/systemone";
pub const OPENAI_DECISIONS_PATH: &str = "/decisions";
const DEFAULT_MODEL: &str = "jev-latest";
const DEFAULT_API_KEY_ENV: &str = "TYPESAFE_API_KEY";
const OPENAI_DEFAULT_MODEL: &str = "gpt-6-luna";
const OPENAI_DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";
const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_RERANK_TIMEOUT_MS: u64 = 3_000;
const DEFAULT_LOG_RETENTION_DAYS: u32 = 90;
const DEFAULT_FLAG_THRESHOLD: f64 = 0.85;
const DEFAULT_CONFIDENCE_THRESHOLD: f64 = 0.9;
const DEFAULT_GOAL_SKIP_BELOW: f64 = 0.05;
const DEFAULT_QUESTION_TOOL_NUDGE: f64 = 0.85;
const DEFAULT_RELEVANCE_FLOOR: f64 = 0.1;
const INVALID_BASE_URL_MESSAGE: &str = "must be an absolute HTTP(S) URL without credentials, query, fragment, whitespace or control characters";
const FULL_ENDPOINT_MESSAGE: &str =
    "Caudra appends /v1/systemone; set base_url to the part before it";
const OPENAI_FULL_ENDPOINT_MESSAGE: &str = "Caudra appends /decisions; set base_url to the versioned API base, such as https://api.openai.com/v1";
const PLAIN_HTTP_MESSAGE: &str = "non-loopback HTTP endpoints send data and credentials without TLS; set allow_http = true on this endpoint to opt in";
const INVALID_CREDENTIAL_MESSAGE: &str =
    "credential must be nonempty and contain no control characters";
const STORED_KEY_FIELD: &str = "api_key";
const API_KEY_ENV_FIELD: &str = "api_key_env";

/// A ready-made endpoint the login flows offer by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionPreset {
    pub name: &'static str,
    pub display_name: &'static str,
    pub protocol: DecisionProtocol,
    pub base_url: &'static str,
    pub model: &'static str,
    pub api_key_env: &'static str,
    /// Where to get a key for it.
    pub key_url: &'static str,
}

pub static DECISION_PRESETS: [DecisionPreset; 2] = [
    DecisionPreset {
        name: "typesafe",
        display_name: "TypeSafe Jev",
        protocol: DecisionProtocol::TypeSafe,
        base_url: "https://api.typesafe.ai",
        model: DEFAULT_MODEL,
        api_key_env: DEFAULT_API_KEY_ENV,
        key_url: "https://console.typesafe.ai",
    },
    DecisionPreset {
        name: "openai",
        display_name: "OpenAI Luna",
        protocol: DecisionProtocol::OpenAI,
        base_url: "https://api.openai.com/v1",
        model: OPENAI_DEFAULT_MODEL,
        api_key_env: OPENAI_DEFAULT_API_KEY_ENV,
        key_url: "https://platform.openai.com/api-keys",
    },
];

impl DecisionPreset {
    pub fn find(name: &str) -> Option<&'static Self> {
        DECISION_PRESETS.iter().find(|preset| preset.name == name)
    }

    pub fn def(&self) -> DecisionEndpointDef {
        DecisionEndpointDef {
            protocol: self.protocol.clone(),
            base_url: self.base_url.into(),
            model: Some(self.model.into()),
            api_key_env: Some(self.api_key_env.into()),
            allow_http: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionProtocol {
    #[default]
    TypeSafe,
    OpenAI,
}

impl DecisionProtocol {
    pub const ALL: [Self; 2] = [Self::TypeSafe, Self::OpenAI];

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::TypeSafe => "typesafe",
            Self::OpenAI => "openai",
        }
    }

    /// The preset named after this protocol.
    pub fn preset(&self) -> &'static DecisionPreset {
        match self {
            Self::TypeSafe => &DECISION_PRESETS[0],
            Self::OpenAI => &DECISION_PRESETS[1],
        }
    }

    pub const fn default_model(&self) -> &'static str {
        match self {
            Self::TypeSafe => DEFAULT_MODEL,
            Self::OpenAI => OPENAI_DEFAULT_MODEL,
        }
    }

    pub const fn default_api_key_env(&self) -> &'static str {
        match self {
            Self::TypeSafe => DEFAULT_API_KEY_ENV,
            Self::OpenAI => OPENAI_DEFAULT_API_KEY_ENV,
        }
    }

    /// The path Caudra appends to `base_url`.
    pub const fn endpoint_path(&self) -> &'static str {
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
    #[error("invalid providers.toml: decisions.{name}.{field}: {message}")]
    InvalidEndpoint {
        name: String,
        field: &'static str,
        message: &'static str,
    },
    #[error("invalid providers.toml: decisions.{name}: {source}")]
    EndpointName { name: String, source: NameError },
    #[error(
        "invalid config: [decisions] no longer takes {keys}; decision endpoints are named tables in the global providers.toml now. Remove them from caudra.toml and add this to providers.toml:\n\n{table}\nor run `caudra auth login --decision` to set one up"
    )]
    LegacyEndpointKeys { keys: String, table: String },
}

impl DecisionsConfigError {
    fn for_endpoint(self, name: &str) -> Self {
        match self {
            Self::Invalid { field, message } => Self::InvalidEndpoint {
                name: name.into(),
                field,
                message,
            },
            other => other,
        }
    }
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
    skill_suggestions: [Advise],
        "Suggest a shortlisted skill. The agent still chooses whether to load it.";
    goal_prescreen: [Enforce],
        "Skip an unlikely-to-pass goal evaluation within the continuation budget and continue work. Only the normal evaluator can certify completion.";
    subagent_routing: [Enforce],
        "Choose a model job for a new unpinned subagent from its task label, mode, profile, and a redacted prompt excerpt. Explicit jobs, models chosen on the `task` call, profile pins, and continuations keep their routing.";
    question_tool_nudge: [Advise],
        "At a main-session handoff with a usable question tool, advise adds one visible reminder per user-input episode to ask a live user question through that tool. Shadow only evaluates. Uses a redacted, bounded request and reply excerpt; uncertainty and failures leave the reply unchanged.";
    memory_search: [Enforce],
        "Rerank the memory tool's keyword matches against the agent's query and omit those judged unrelated, saying how many were omitted. Sends the query and each candidate note's name, heading, and a redacted body excerpt.";
    history_search: [Enforce],
        "Rerank session history search's keyword matches against the agent's query, drop those judged unrelated, and return the rest best first. Sends the query and each candidate's kind, tool, date, session title, and a redacted excerpt.";
    memory_recall: [Advise],
        "In main sessions, remind the agent of at most three saved notes likely relevant to a new user message, without loading them. Sends a redacted message excerpt and each keyword-matched note's name, heading, and redacted excerpt.";
    memory_duplicates: [Advise],
        "When a new memory note is written, name existing notes that may cover the same topic in the write receipt. The write itself is never blocked or changed. Sends redacted excerpts of the new note and of keyword-matched notes.";
    compaction_retention: [Enforce],
        "During compaction, keep the tool results most relevant to the active goal, open todos, and latest request instead of only the newest, and keep the most relevant part of a long result. Sends redacted goal, todo, and request excerpts, and each recent result's tool name with redacted input and output excerpts.";
    output_pointers: [Advise],
        "When a saved tool output was truncated, point the agent to at most two omitted line ranges likely to matter. Nothing extra is removed. Sends redacted excerpts of the in-progress todos, latest request, tool input, and omitted lines.";
    source_code_context: [Enforce],
        "In local sessions, judge the first 64 code-context rows against the task and keep only those judged relevant, best first. Sends the task and each row's symbol name, kind, path, line span, and reference counts, never source text.";
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DecisionThresholds {
    pub permission_flag: f64,
    pub auto_flag: f64,
    pub content_injection: f64,
    pub content_addressed_to_agent: f64,
    pub routing_confidence: f64,
    pub goal_skip_below: f64,
    pub question_tool_nudge: f64,
    pub relevance_floor: f64,
    pub shell_writes: Option<f64>,
}

impl Default for DecisionThresholds {
    fn default() -> Self {
        Self {
            permission_flag: DEFAULT_FLAG_THRESHOLD,
            auto_flag: DEFAULT_FLAG_THRESHOLD,
            content_injection: DEFAULT_CONFIDENCE_THRESHOLD,
            content_addressed_to_agent: DEFAULT_CONFIDENCE_THRESHOLD,
            routing_confidence: DEFAULT_CONFIDENCE_THRESHOLD,
            goal_skip_below: DEFAULT_GOAL_SKIP_BELOW,
            question_tool_nudge: DEFAULT_QUESTION_TOOL_NUDGE,
            relevance_floor: DEFAULT_RELEVANCE_FLOOR,
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
            choices: &[],
            description: "Probability for a permission warning.",
        },
        ConfigField {
            name: "auto_flag",
            ty: "float",
            default: ConfigValue::F64(DEFAULT_FLAG_THRESHOLD),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Probability for escalating an eligible Auto call.",
        },
        ConfigField {
            name: "content_injection",
            ty: "float",
            default: ConfigValue::F64(DEFAULT_CONFIDENCE_THRESHOLD),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Probability that sampled content attempts instruction injection.",
        },
        ConfigField {
            name: "content_addressed_to_agent",
            ty: "float",
            default: ConfigValue::F64(DEFAULT_CONFIDENCE_THRESHOLD),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Probability that sampled content addresses the agent.",
        },
        ConfigField {
            name: "routing_confidence",
            ty: "float",
            default: ConfigValue::F64(DEFAULT_CONFIDENCE_THRESHOLD),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Confidence required for skill suggestions. Subagent routing picks the Fast model when this much difficulty probability is at or below routine work, and the Best model when this much is on open-ended work. Memory recall, memory duplicates, and output pointers name a candidate only at or above this relevance probability.",
        },
        ConfigField {
            name: "goal_skip_below",
            ty: "float",
            default: ConfigValue::F64(DEFAULT_GOAL_SKIP_BELOW),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Skip an evaluator at or below this completion probability, within the continuation budget.",
        },
        ConfigField {
            name: "shell_writes",
            ty: "float",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Optional project-write warning threshold. Omission leaves the warning disabled. No built-in enforcement threshold.",
        },
        ConfigField {
            name: "question_tool_nudge",
            ty: "float",
            default: ConfigValue::F64(DEFAULT_QUESTION_TOOL_NUDGE),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Minimum noul score for a question-tool reminder. Provisional, not a calibrated probability; advise only, and errors or uncertainty never reopen the turn.",
        },
        ConfigField {
            name: "relevance_floor",
            ty: "float",
            default: ConfigValue::F64(DEFAULT_RELEVANCE_FLOOR),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Relevance probability below which an enforcing rerank feature prunes or demotes a candidate. Memory search omits it, compaction retention keeps a newer result in its place, and tool search ranks it after the candidates judged relevant, in keyword order. A list the engine could not judge is never emptied.",
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
            (
                "thresholds.routing_confidence",
                Some(self.routing_confidence),
            ),
            ("thresholds.goal_skip_below", Some(self.goal_skip_below)),
            (
                "thresholds.question_tool_nudge",
                Some(self.question_tool_nudge),
            ),
            ("thresholds.relevance_floor", Some(self.relevance_floor)),
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
    pub timeout_ms: Option<u64>,
    pub rerank_timeout_ms: Option<u64>,
    pub log: Option<bool>,
    pub log_retention_days: Option<u32>,
    pub features: RawDecisionFeatures,
    pub thresholds: Option<DecisionThresholds>,
    #[serde(rename = "base_url")]
    legacy_base_url: Option<String>,
    #[serde(rename = "protocol")]
    legacy_protocol: Option<DecisionProtocol>,
    #[serde(rename = "model")]
    legacy_model: Option<String>,
    #[serde(rename = "api_key_env")]
    legacy_api_key_env: Option<String>,
    #[serde(rename = "allow_http")]
    legacy_allow_http: Option<bool>,
    #[serde(skip)]
    project_error: Option<DecisionsConfigError>,
    #[serde(skip)]
    auto_screening_restricted: bool,
}

/// The shared `[decisions]` settings joined with the active endpoint, which
/// the caller injects after reading `providers.toml` and the active row.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionsConfig {
    pub endpoint: Option<DecisionEndpoint>,
    pub timeout_ms: u64,
    pub rerank_timeout_ms: u64,
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
            timeout_ms: DEFAULT_TIMEOUT_MS,
            rerank_timeout_ms: DEFAULT_RERANK_TIMEOUT_MS,
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
            timeout_ms,
            rerank_timeout_ms,
            log,
            log_retention_days,
            thresholds,
            legacy_base_url,
            legacy_protocol,
            legacy_model,
            legacy_api_key_env,
            legacy_allow_http
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
        if let Some(error) = overlay.legacy_error() {
            return Err(error);
        }
        for (field, present) in [
            ("timeout_ms", overlay.timeout_ms.is_some()),
            ("rerank_timeout_ms", overlay.rerank_timeout_ms.is_some()),
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

    /// The migration error for connection keys that moved to `providers.toml`,
    /// spelling out the table that replaces them.
    fn legacy_error(&self) -> Option<DecisionsConfigError> {
        let keys = [
            ("base_url", self.legacy_base_url.is_some()),
            ("protocol", self.legacy_protocol.is_some()),
            ("model", self.legacy_model.is_some()),
            ("api_key_env", self.legacy_api_key_env.is_some()),
            ("allow_http", self.legacy_allow_http.is_some()),
        ]
        .into_iter()
        .filter(|(_, present)| *present)
        .map(|(key, _)| format!("`{key}`"))
        .collect::<Vec<_>>();
        if keys.is_empty() {
            return None;
        }
        let protocol = self.legacy_protocol.clone().unwrap_or_default();
        let preset = protocol.preset();
        let def = DecisionEndpointDef {
            base_url: self
                .legacy_base_url
                .clone()
                .unwrap_or_else(|| preset.base_url.into()),
            model: self.legacy_model.clone(),
            api_key_env: Some(
                self.legacy_api_key_env
                    .clone()
                    .unwrap_or_else(|| protocol.default_api_key_env().into()),
            ),
            allow_http: self.legacy_allow_http.filter(|allow| *allow),
            protocol,
        };
        Some(DecisionsConfigError::LegacyEndpointKeys {
            keys: keys.join(", "),
            table: def.to_table(preset.name),
        })
    }

    /// The shared settings, with no endpoint until the caller injects the active one.
    pub fn resolve(self) -> Result<DecisionsConfig, DecisionsConfigError> {
        if let Some(error) = self.project_error {
            return Err(error);
        }
        if let Some(error) = self.legacy_error() {
            return Err(error);
        }
        let config = DecisionsConfig {
            endpoint: None,
            timeout_ms: self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            rerank_timeout_ms: self.rerank_timeout_ms.unwrap_or(DEFAULT_RERANK_TIMEOUT_MS),
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
}

impl DecisionsConfig {
    pub const FIELDS: &[ConfigField] = &[
        ConfigField {
            name: "timeout_ms",
            ty: "integer",
            default: ConfigValue::U64(DEFAULT_TIMEOUT_MS),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Positive decision-request deadline in milliseconds, separate from shell execution timeouts. Tool search and every reranking feature except memory recall use `rerank_timeout_ms` instead.",
        },
        ConfigField {
            name: "rerank_timeout_ms",
            ty: "integer",
            default: ConfigValue::U64(DEFAULT_RERANK_TIMEOUT_MS),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Positive deadline in milliseconds for tool search and the reranking features, per 16 candidates a request judges. A request holds up to 64, so a full one waits up to four budgets. Memory recall keeps `timeout_ms` because it delays the start of a turn. A `decide()` or `rerank()` call from a workflow or a `code` snippet may ask for up to the larger of the two.",
        },
        ConfigField {
            name: "log",
            ty: "boolean",
            default: ConfigValue::Bool(false),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Retain bounded decision records in the local `caudra.db`.",
        },
        ConfigField {
            name: "log_retention_days",
            ty: "integer",
            default: ConfigValue::U64(DEFAULT_LOG_RETENTION_DAYS as u64),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Positive retention period for decision records.",
        },
        ConfigField {
            name: "features",
            ty: "table",
            default: ConfigValue::Toml("{}"),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Per-feature modes below.",
        },
        ConfigField {
            name: "thresholds",
            ty: "table",
            default: ConfigValue::Toml("{}"),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Probability thresholds, all finite and within 0–1 inclusive.",
        },
    ];

    pub fn with_endpoint(self, endpoint: Option<DecisionEndpoint>) -> Self {
        Self { endpoint, ..self }
    }

    /// The full URL requests go to, while an endpoint is active.
    pub fn endpoint_url(&self) -> Option<Url> {
        self.endpoint.as_ref().map(DecisionEndpoint::endpoint)
    }

    pub fn validate(&self) -> Result<(), DecisionsConfigError> {
        if let Some(endpoint) = &self.endpoint {
            endpoint.validate()?;
        }
        if self.timeout_ms == 0 {
            return Err(invalid("timeout_ms", "must be greater than zero"));
        }
        if self.rerank_timeout_ms == 0 {
            return Err(invalid("rerank_timeout_ms", "must be greater than zero"));
        }
        if self.log_retention_days == 0 {
            return Err(invalid("log_retention_days", "must be greater than zero"));
        }
        self.features.validate()?;
        self.thresholds.validate()
    }

    /// The active endpoint's credential; see [`DecisionEndpoint::api_key`].
    pub fn api_key(
        &self,
        stored: Option<String>,
    ) -> Result<Option<DecisionApiKey>, DecisionsConfigError> {
        self.endpoint
            .as_ref()
            .map_or(Ok(None), |endpoint| endpoint.api_key(stored))
    }
}

/// One `[decisions.<name>]` table of the global `providers.toml`, as written.
/// Configuring it is the consent to send decision context to it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionEndpointDef {
    pub protocol: DecisionProtocol,
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_http: Option<bool>,
}

impl DecisionEndpointDef {
    pub fn resolve(&self, name: &str) -> Result<DecisionEndpoint, DecisionsConfigError> {
        validate_name(name).map_err(|source| DecisionsConfigError::EndpointName {
            name: name.into(),
            source,
        })?;
        let allow_http = self.allow_http.unwrap_or(false);
        let endpoint = DecisionEndpoint {
            name: name.into(),
            base_url: parse_base_url(&self.base_url, &self.protocol, allow_http)
                .map_err(|error| error.for_endpoint(name))?,
            model: self
                .model
                .clone()
                .unwrap_or_else(|| self.protocol.default_model().into()),
            api_key_env: self.api_key_env.clone(),
            protocol: self.protocol.clone(),
            allow_http,
        };
        endpoint.validate()?;
        Ok(endpoint)
    }

    /// Whether `base_url` resolves only with `allow_http`, which is plain HTTP
    /// to a non-loopback host. Login flows ask for consent exactly then.
    pub fn needs_allow_http(&self) -> bool {
        parse_base_url(&self.base_url, &self.protocol, false).is_err()
            && parse_base_url(&self.base_url, &self.protocol, true).is_ok()
    }

    /// This definition as a `providers.toml` table named `name`.
    #[expect(
        clippy::disallowed_methods,
        reason = "a snippet to show, never written into a file"
    )]
    pub fn to_table(&self, name: &str) -> String {
        let body = toml::to_string(self).unwrap_or_default();
        format!("[{DECISIONS_TABLE}.{name}]\n{body}")
    }
}

/// A resolved, validated decision endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionEndpoint {
    pub name: String,
    pub protocol: DecisionProtocol,
    pub base_url: Url,
    pub model: String,
    pub api_key_env: Option<String>,
    pub allow_http: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionKeySource {
    Stored,
    Environment,
}

impl DecisionKeySource {
    /// Where an endpoint's key comes from as listings show it: `saved`,
    /// `env`, or `none` for a keyless endpoint.
    pub const fn label(source: Option<Self>) -> &'static str {
        match source {
            Some(Self::Stored) => "saved",
            Some(Self::Environment) => "env",
            None => "none",
        }
    }

    const fn field(self) -> &'static str {
        match self {
            Self::Stored => STORED_KEY_FIELD,
            Self::Environment => API_KEY_ENV_FIELD,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct DecisionApiKey {
    pub value: String,
    pub source: DecisionKeySource,
}

impl fmt::Debug for DecisionApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DecisionApiKey")
            .field("value", &"<redacted>")
            .field("source", &self.source)
            .finish()
    }
}

impl DecisionEndpoint {
    pub const FIELDS: &[ConfigField] = &[
        ConfigField {
            name: "protocol",
            ty: "string",
            default: ConfigValue::Required("\"typesafe\""),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Decision wire protocol: `typesafe` or `openai`. Never inferred from the URL or model.",
        },
        ConfigField {
            name: "base_url",
            ty: "string",
            default: ConfigValue::Required("\"https://api.typesafe.ai\""),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Decision API base URL. TypeSafe takes a root such as `https://api.typesafe.ai` and appends `/v1/systemone`. OpenAI takes a versioned base such as `https://api.openai.com/v1` and appends `/decisions`. A path prefix stays, and a full endpoint is rejected. `{env:NAME}` reads the base from the environment. HTTPS is required, except for numeric loopback HTTP or with `allow_http`. No credentials, query, fragment, whitespace, or control characters.",
        },
        ConfigField {
            name: "model",
            ty: "string",
            default: ConfigValue::Varies("`jev-latest` for typesafe, `gpt-6-luna` for openai"),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Decision model identifier, nonblank and without control characters.",
        },
        ConfigField {
            name: "api_key_env",
            ty: "string",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Environment variable holding the credential when no key is saved for this endpoint, never the credential itself. Project environment values are excluded. The presets set `TYPESAFE_API_KEY` and `OPENAI_API_KEY`.",
        },
        ConfigField {
            name: "allow_http",
            ty: "boolean",
            default: ConfigValue::Bool(false),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Opt-in for plain HTTP to a non-loopback host. Use only with transport protection you control, such as a trusted encrypted tunnel.",
        },
    ];

    /// An endpoint with the protocol's default model and no credential source.
    pub fn new(name: impl Into<String>, protocol: DecisionProtocol, base_url: Url) -> Self {
        Self {
            name: name.into(),
            model: protocol.default_model().into(),
            protocol,
            base_url,
            api_key_env: None,
            allow_http: false,
        }
    }

    pub fn endpoint(&self) -> Url {
        let mut endpoint = self.base_url.clone();
        endpoint.set_path(&format!(
            "{}{}",
            self.base_url.path().trim_end_matches('/'),
            self.protocol.endpoint_path()
        ));
        endpoint
    }

    pub fn validate(&self) -> Result<(), DecisionsConfigError> {
        validate_name(&self.name).map_err(|source| DecisionsConfigError::EndpointName {
            name: self.name.clone(),
            source,
        })?;
        self.check().map_err(|error| error.for_endpoint(&self.name))
    }

    fn check(&self) -> Result<(), DecisionsConfigError> {
        parse_base_url(self.base_url.as_str(), &self.protocol, self.allow_http)?;
        if let Some(variable) = &self.api_key_env {
            let mut bytes = variable.bytes();
            if !bytes
                .next()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
                || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                return Err(invalid(
                    API_KEY_ENV_FIELD,
                    "must be an environment variable name, not a credential",
                ));
            }
        }
        if self.model.trim().is_empty() || self.model.chars().any(char::is_control) {
            return Err(invalid(
                "model",
                "must be nonempty and contain no control characters",
            ));
        }
        Ok(())
    }

    /// The saved key wins; otherwise the global value of `api_key_env`. A
    /// keyless endpoint, such as a loopback server, answers `None`.
    pub fn api_key(
        &self,
        stored: Option<String>,
    ) -> Result<Option<DecisionApiKey>, DecisionsConfigError> {
        self.validate()?;
        let key = match (stored, &self.api_key_env) {
            (Some(value), _) => Some(DecisionApiKey {
                value,
                source: DecisionKeySource::Stored,
            }),
            (None, Some(variable)) => crate::global_env_value(variable)
                .map_err(|_| {
                    invalid(API_KEY_ENV_FIELD, "credential must be UTF-8").for_endpoint(&self.name)
                })?
                .map(|value| DecisionApiKey {
                    value,
                    source: DecisionKeySource::Environment,
                }),
            (None, None) => None,
        };
        if let Some(key) = &key
            && (key.value.is_empty() || key.value.chars().any(char::is_control))
        {
            return Err(
                invalid(key.source.field(), INVALID_CREDENTIAL_MESSAGE).for_endpoint(&self.name)
            );
        }
        Ok(key)
    }
}

fn parse_base_url(
    base_url: &str,
    protocol: &DecisionProtocol,
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
    if !is_loopback && url.scheme() == "http" && !allow_http {
        return Err(invalid("allow_http", PLAIN_HTTP_MESSAGE));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::{
        DECISION_PRESETS, DEFAULT_RELEVANCE_FLOOR, DEFAULT_RERANK_TIMEOUT_MS, DecisionApiKey,
        DecisionEndpoint, DecisionEndpointDef, DecisionKeySource, DecisionPreset, DecisionProtocol,
        DecisionsConfig, DecisionsConfigError, FULL_ENDPOINT_MESSAGE, FeatureMode,
        INVALID_BASE_URL_MESSAGE, INVALID_CREDENTIAL_MESSAGE, OPENAI_FULL_ENDPOINT_MESSAGE,
        PLAIN_HTTP_MESSAGE, RawDecisionsConfig,
    };
    use crate::{ConfigError, RawConfig};
    use caudra_storage::auth::NameError;
    use test_case::test_case;

    const NAME: &str = "tunnel";
    const LOCAL_BASE_URL: &str = "http://127.0.0.1:8000";
    const LOCAL_ENDPOINT: &str = "http://127.0.0.1:8000/v1/systemone";
    const PREFIXED_BASE_URL: &str = "http://127.0.0.1:8080/typesafe";
    const PREFIXED_ENDPOINT: &str = "http://127.0.0.1:8080/typesafe/v1/systemone";
    const TAILNET_BASE_URL: &str = "http://100.64.0.3:8080/typesafe";
    const REMOTE_BASE_URL: &str = "https://decisions.example.test";
    const REMOTE_HTTP_BASE_URL: &str = "http://decisions.example.test:443";
    const BASE_URL_SIZE_LIMIT: usize = 2048;
    const REMOVED_ENDPOINT_MESSAGE: &str = "unknown field `endpoint`";
    const UNKNOWN_ALLOW_REMOTE: &str = "unknown field `allow_remote`";
    const LOGIN_HINT: &str = "caudra auth login --decision";
    const CUSTOM_MODEL: &str = "custom-model";
    const CUSTOM_API_KEY_ENV: &str = "CUSTOM_DECISION_API_KEY";
    const CUSTOM_TIMEOUT_MS: u64 = 500;
    const CUSTOM_RERANK_TIMEOUT_MS: u64 = 4_000;
    const STORED_KEY: &str = "stored-test-credential";
    const ENV_KEY: &str = "env-test-credential";

    fn def(protocol: DecisionProtocol, base_url: &str, allow_http: bool) -> DecisionEndpointDef {
        DecisionEndpointDef {
            protocol,
            base_url: base_url.into(),
            model: None,
            api_key_env: None,
            allow_http: allow_http.then_some(true),
        }
    }

    fn invalid_field(error: DecisionsConfigError) -> Option<&'static str> {
        match error {
            DecisionsConfigError::InvalidEndpoint { name, field, .. } if name == NAME => {
                Some(field)
            }
            _ => None,
        }
    }

    fn invalid_endpoint(field: &'static str, message: &'static str) -> DecisionsConfigError {
        DecisionsConfigError::InvalidEndpoint {
            name: NAME.into(),
            field,
            message,
        }
    }

    #[test]
    fn defaults_never_activate_decisions() {
        let config = RawDecisionsConfig::default().resolve().unwrap();
        assert_eq!(config, DecisionsConfig::default());
        assert!(config.endpoint.is_none());
        assert!(config.endpoint_url().is_none());
        assert!(!config.features.any_enabled());
        assert!(!config.log);
        assert!(config.thresholds.shell_writes.is_none());
        assert_eq!(config.features.question_tool_nudge, FeatureMode::Off);
        assert_eq!(
            config.thresholds.question_tool_nudge,
            super::DEFAULT_QUESTION_TOOL_NUDGE
        );
        assert_eq!(config.rerank_timeout_ms, DEFAULT_RERANK_TIMEOUT_MS);
        assert_eq!(config.thresholds.relevance_floor, DEFAULT_RELEVANCE_FLOOR);
        assert_eq!(config.api_key(Some(STORED_KEY.into())), Ok(None));
        assert!(
            RawConfig::default()
                .into_config(false)
                .unwrap()
                .decisions
                .endpoint
                .is_none()
        );
    }

    #[test_case(DecisionProtocol::TypeSafe, "jev-latest", "https://api.typesafe.ai/v1/systemone"; "typesafe")]
    #[test_case(DecisionProtocol::OpenAI, "gpt-6-luna", "https://api.typesafe.ai/decisions"; "openai")]
    fn endpoint_def_resolves_with_protocol_defaults(
        protocol: DecisionProtocol,
        model: &str,
        endpoint: &str,
    ) {
        let resolved = def(protocol.clone(), DECISION_PRESETS[0].base_url, false)
            .resolve(NAME)
            .unwrap();
        assert_eq!(resolved.name, NAME);
        assert_eq!(resolved.protocol, protocol);
        assert_eq!(resolved.model, model);
        assert_eq!(resolved.api_key_env, None);
        assert!(!resolved.allow_http);
        assert_eq!(resolved.endpoint().as_str(), endpoint);
        assert_eq!(serde_json::to_value(&protocol).unwrap(), protocol.as_str());
    }

    #[test_case("typesafe", "https://api.typesafe.ai/v1/systemone", "TYPESAFE_API_KEY"; "typesafe")]
    #[test_case("openai", "https://api.openai.com/v1/decisions", "OPENAI_API_KEY"; "openai")]
    fn presets_resolve_to_their_hosted_endpoints(name: &str, endpoint: &str, api_key_env: &str) {
        let preset = DecisionPreset::find(name).unwrap();
        assert_eq!(preset.protocol.preset(), preset);
        let resolved = preset.def().resolve(name).unwrap();
        assert_eq!(resolved.endpoint().as_str(), endpoint);
        assert_eq!(resolved.model, preset.protocol.default_model());
        assert_eq!(resolved.api_key_env.as_deref(), Some(api_key_env));
        let table: toml::Table = toml::from_str(&preset.def().to_table(name)).unwrap();
        let reparsed: DecisionEndpointDef = table[super::DECISIONS_TABLE][name]
            .clone()
            .try_into()
            .unwrap();
        assert_eq!(reparsed, preset.def());
    }

    #[test_case("typesafe"; "typesafe")]
    #[test_case("openai"; "openai")]
    fn endpoint_def_preserves_explicit_model_and_credential_name(protocol: &str) {
        let parsed: DecisionEndpointDef = toml::from_str(&format!(
            "protocol = '{protocol}'\nbase_url = '{REMOTE_BASE_URL}'\nmodel = '{CUSTOM_MODEL}'\napi_key_env = '{CUSTOM_API_KEY_ENV}'"
        ))
        .unwrap();
        let resolved = parsed.resolve(NAME).unwrap();
        assert_eq!(resolved.model, CUSTOM_MODEL);
        assert_eq!(resolved.api_key_env.as_deref(), Some(CUSTOM_API_KEY_ENV));
    }

    #[test_case("protocol = 'unknown'"; "unknown")]
    #[test_case("protocol = 'OpenAI'"; "case")]
    #[test_case("protocol = 'type_safe'"; "underscore")]
    #[test_case("protocol = ''"; "empty")]
    #[test_case(""; "missing")]
    fn unusable_protocols_are_rejected(protocol: &str) {
        assert!(
            toml::from_str::<DecisionEndpointDef>(&format!(
                "{protocol}\nbase_url = '{REMOTE_BASE_URL}'"
            ))
            .is_err()
        );
    }

    #[test]
    fn allow_remote_is_unknown_in_endpoint_tables() {
        let error = toml::from_str::<DecisionEndpointDef>(&format!(
            "protocol = 'typesafe'\nbase_url = '{REMOTE_BASE_URL}'\nallow_remote = true"
        ))
        .unwrap_err();
        assert!(error.to_string().contains(UNKNOWN_ALLOW_REMOTE), "{error}");
    }

    #[test_case(LOCAL_BASE_URL, true; "numeric_loopback")]
    #[test_case("http://[::1]:8000", true; "ipv6_loopback")]
    #[test_case(PREFIXED_BASE_URL, true; "path_prefix")]
    #[test_case(REMOTE_BASE_URL, true; "remote_https_needs_no_opt_in")]
    #[test_case("https://localhost", true; "dns_https")]
    #[test_case("http://decisions.example.test", false; "remote_requires_tls")]
    #[test_case("http://127.0.0.1.evil.test", false; "loopback_lookalike")]
    #[test_case("https://user:secret@decisions.example.test", false; "credentials")]
    #[test_case("https://@decisions.example.test", false; "empty_userinfo")]
    #[test_case("https://decisions.example.test#secret", false; "fragment")]
    #[test_case("https://decisions.example.test?api_key=secret", false; "query")]
    #[test_case("file:///etc/passwd", false; "unsupported_scheme")]
    fn base_url_policy(base_url: &str, valid: bool) {
        for protocol in DecisionProtocol::ALL {
            assert_eq!(def(protocol, base_url, false).resolve(NAME).is_ok(), valid);
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
        assert_eq!(
            def(protocol, base_url, false).resolve(NAME),
            Err(invalid_endpoint("base_url", message))
        );
    }

    #[test_case(DecisionProtocol::TypeSafe, LOCAL_BASE_URL, LOCAL_ENDPOINT; "origin")]
    #[test_case(DecisionProtocol::TypeSafe, "http://127.0.0.1:8000/", LOCAL_ENDPOINT; "origin_trailing_slash")]
    #[test_case(DecisionProtocol::TypeSafe, PREFIXED_BASE_URL, PREFIXED_ENDPOINT; "prefix")]
    #[test_case(DecisionProtocol::TypeSafe, "http://127.0.0.1:8080/typesafe/", PREFIXED_ENDPOINT; "prefix_trailing_slash")]
    #[test_case(DecisionProtocol::OpenAI, "https://api.openai.com/v1", "https://api.openai.com/v1/decisions"; "openai")]
    #[test_case(DecisionProtocol::OpenAI, "http://127.0.0.1:8000/proxy/v1///", "http://127.0.0.1:8000/proxy/v1/decisions"; "openai_prefix")]
    #[test_case(DecisionProtocol::OpenAI, "http://[::1]:8000/v1/", "http://[::1]:8000/v1/decisions"; "openai_ipv6")]
    #[test_case(DecisionProtocol::TypeSafe, "https://api.openai.com/v1", "https://api.openai.com/v1/v1/systemone"; "url_does_not_select_protocol")]
    fn endpoint_appends_the_protocol_path(
        protocol: DecisionProtocol,
        base_url: &str,
        endpoint: &str,
    ) {
        let resolved = def(protocol, base_url, false).resolve(NAME).unwrap();
        assert_eq!(resolved.endpoint().as_str(), endpoint);
        let config = DecisionsConfig::default().with_endpoint(Some(resolved));
        assert_eq!(config.endpoint_url().unwrap().as_str(), endpoint);
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
    fn non_loopback_http_requires_allow_http(base_url: &str) {
        for protocol in DecisionProtocol::ALL {
            assert_eq!(
                def(protocol.clone(), base_url, false).resolve(NAME),
                Err(invalid_endpoint("allow_http", PLAIN_HTTP_MESSAGE))
            );
            let resolved = def(protocol, base_url, true).resolve(NAME).unwrap();
            assert!(resolved.allow_http);
            assert!(resolved.validate().is_ok());
            let downgraded = DecisionEndpoint {
                allow_http: false,
                ..resolved
            };
            assert_eq!(
                invalid_field(downgraded.validate().unwrap_err()),
                Some("allow_http")
            );
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
        for protocol in DecisionProtocol::ALL {
            assert_eq!(
                def(protocol, base_url, true).resolve(NAME),
                Err(invalid_endpoint("base_url", INVALID_BASE_URL_MESSAGE))
            );
        }
    }

    #[test_case(BASE_URL_SIZE_LIMIT, true; "at_limit")]
    #[test_case(BASE_URL_SIZE_LIMIT + 1, false; "over_limit")]
    fn http_opt_in_preserves_base_url_size_limit(length: usize, valid: bool) {
        let prefix = format!("{REMOTE_HTTP_BASE_URL}/");
        let base_url = format!("{prefix}{}", "a".repeat(length - prefix.len()));
        assert_eq!(
            def(DecisionProtocol::TypeSafe, &base_url, true)
                .resolve(NAME)
                .is_ok(),
            valid
        );
    }

    #[test_case(REMOTE_HTTP_BASE_URL, true; "remote_http")]
    #[test_case(TAILNET_BASE_URL, true; "cgnat_http")]
    #[test_case("http://localhost:8000", true; "localhost_is_dns")]
    #[test_case(LOCAL_BASE_URL, false; "numeric_loopback")]
    #[test_case(REMOTE_BASE_URL, false; "remote_https")]
    #[test_case("http://decisions.example.test?api_key=secret", false; "invalid_url")]
    #[test_case(LOCAL_ENDPOINT, false; "full_endpoint")]
    fn plain_http_consent_matches_the_resolve_rule(base_url: &str, expected: bool) {
        assert_eq!(
            def(DecisionProtocol::TypeSafe, base_url, false).needs_allow_http(),
            expected
        );
    }

    #[test_case(None; "omitted")]
    #[test_case(Some(false); "explicit_false")]
    #[test_case(Some(true); "explicit_true")]
    fn numeric_loopback_http_does_not_require_opt_in(allow_http: Option<bool>) {
        for (base_url, endpoint) in [
            (LOCAL_BASE_URL, LOCAL_ENDPOINT),
            (PREFIXED_BASE_URL, PREFIXED_ENDPOINT),
            ("http://[::1]:8000", "http://[::1]:8000/v1/systemone"),
        ] {
            let resolved = DecisionEndpointDef {
                allow_http,
                ..def(DecisionProtocol::TypeSafe, base_url, false)
            }
            .resolve(NAME)
            .unwrap();
            assert_eq!(resolved.endpoint().as_str(), endpoint);
        }
    }

    #[test_case(DecisionProtocol::TypeSafe, "http://127.0.0.1//other.example.test/predict", "//other.example.test/predict/v1/systemone"; "typesafe")]
    #[test_case(DecisionProtocol::OpenAI, "http://127.0.0.1//other.example.test/v1", "//other.example.test/v1/decisions"; "openai")]
    fn double_slash_paths_never_reinterpret_the_authority(
        protocol: DecisionProtocol,
        base_url: &str,
        path: &str,
    ) {
        let endpoint = def(protocol, base_url, false)
            .resolve(NAME)
            .unwrap()
            .endpoint();
        assert_eq!(endpoint.host_str(), Some("127.0.0.1"));
        assert_eq!(endpoint.path(), path);
    }

    #[test_case("", NameError::Empty; "empty")]
    #[test_case("My Endpoint", NameError::InvalidStart; "unslugged")]
    #[test_case("a.b", NameError::InvalidCharacter; "dotted")]
    fn endpoint_names_must_be_slugs(name: &str, expected: NameError) {
        assert_eq!(
            def(DecisionProtocol::TypeSafe, REMOTE_BASE_URL, false).resolve(name),
            Err(DecisionsConfigError::EndpointName {
                name: name.into(),
                source: expected,
            })
        );
    }

    #[test_case("api_key_env = 'Bearer secret'", "api_key_env"; "spaced_env")]
    #[test_case("api_key_env = 'KEY=secret'", "api_key_env"; "assignment_env")]
    #[test_case("api_key_env = '9KEY'", "api_key_env"; "digit_env")]
    #[test_case("api_key_env = ''", "api_key_env"; "empty_env")]
    #[test_case("model = ''", "model"; "empty_model")]
    #[test_case("model = \"a\\u0007b\"", "model"; "control_model")]
    fn invalid_endpoint_settings_are_rejected(setting: &str, field: &str) {
        let parsed: DecisionEndpointDef = toml::from_str(&format!(
            "protocol = 'typesafe'\nbase_url = '{REMOTE_BASE_URL}'\n{setting}"
        ))
        .unwrap();
        assert_eq!(
            invalid_field(parsed.resolve(NAME).unwrap_err()),
            Some(field)
        );
    }

    #[test_case(Some(STORED_KEY), Some(ENV_KEY), Some((STORED_KEY, DecisionKeySource::Stored)); "stored_over_env")]
    #[test_case(None, Some(ENV_KEY), Some((ENV_KEY, DecisionKeySource::Environment)); "env_fallback")]
    #[test_case(Some(STORED_KEY), None, Some((STORED_KEY, DecisionKeySource::Stored)); "stored_only")]
    #[test_case(None, None, None; "keyless")]
    fn stored_key_wins_over_the_environment(
        stored: Option<&str>,
        environment: Option<&str>,
        expected: Option<(&str, DecisionKeySource)>,
    ) {
        const VARIABLE: &str = "TEST_CAUDRA_DECISION_ENDPOINT_KEY";
        match environment {
            Some(value) => unsafe { std::env::set_var(VARIABLE, value) },
            None => unsafe { std::env::remove_var(VARIABLE) },
        }
        let endpoint = DecisionEndpointDef {
            api_key_env: Some(VARIABLE.into()),
            ..def(DecisionProtocol::OpenAI, REMOTE_BASE_URL, false)
        }
        .resolve(NAME)
        .unwrap();
        let key = endpoint.api_key(stored.map(Into::into)).unwrap();
        unsafe { std::env::remove_var(VARIABLE) };
        assert_eq!(
            key,
            expected.map(|(value, source)| DecisionApiKey {
                value: value.into(),
                source,
            })
        );
    }

    #[test_case(""; "empty")]
    #[test_case("invalid\r\nheader"; "header_injection")]
    fn stored_keys_are_validated(stored: &str) {
        let endpoint = def(DecisionProtocol::TypeSafe, REMOTE_BASE_URL, false)
            .resolve(NAME)
            .unwrap();
        assert_eq!(
            endpoint.api_key(Some(stored.into())),
            Err(invalid_endpoint("api_key", INVALID_CREDENTIAL_MESSAGE))
        );
    }

    #[test]
    fn api_key_debug_redacts_the_value() {
        let key = DecisionApiKey {
            value: STORED_KEY.into(),
            source: DecisionKeySource::Stored,
        };
        assert!(!format!("{key:?}").contains(STORED_KEY));
    }

    #[test_case("base_url = 'http://100.64.0.3:8000/v1'\nprotocol = 'openai'\nallow_http = true", "openai", "http://100.64.0.3:8000/v1"; "full")]
    #[test_case("base_url = 'https://decisions.example.test'", "typesafe", "https://decisions.example.test"; "base_url")]
    #[test_case("protocol = 'openai'", "openai", "https://api.openai.com/v1"; "protocol")]
    #[test_case("model = 'custom'", "typesafe", "https://api.typesafe.ai"; "model")]
    #[test_case("api_key_env = 'CUSTOM_KEY'", "typesafe", "https://api.typesafe.ai"; "api_key_env")]
    #[test_case("allow_http = true", "typesafe", "https://api.typesafe.ai"; "allow_http")]
    fn legacy_connection_keys_point_to_providers_toml(source: &str, name: &str, base_url: &str) {
        let Err(error) = toml::from_str::<RawConfig>(&format!("[decisions]\n{source}"))
            .unwrap()
            .into_config(false)
        else {
            panic!("legacy keys must not resolve");
        };
        let ConfigError::Decisions(DecisionsConfigError::LegacyEndpointKeys { table, .. }) = &error
        else {
            panic!("expected the migration error, got {error}");
        };
        assert!(error.to_string().contains(LOGIN_HINT), "{error}");
        let parsed: toml::Table = toml::from_str(table).unwrap();
        let migrated: DecisionEndpointDef = parsed[super::DECISIONS_TABLE][name]
            .clone()
            .try_into()
            .unwrap();
        assert_eq!(migrated.base_url, base_url);
        assert!(migrated.api_key_env.is_some());
        assert!(migrated.resolve(name).is_ok());
    }

    #[test]
    fn legacy_connection_keys_in_a_project_point_to_providers_toml() {
        let mut global = RawConfig::default();
        global.merge(
            toml::from_str("[decisions]\nbase_url = 'https://project.example.test'").unwrap(),
        );
        assert!(matches!(
            global.into_config(false),
            Err(ConfigError::Decisions(
                DecisionsConfigError::LegacyEndpointKeys { .. }
            ))
        ));
    }

    #[test_case("permission_advice", true, false)]
    #[test_case("auto_screening", false, true)]
    #[test_case("shell_effect", true, false)]
    #[test_case("content_screening", true, false)]
    #[test_case("skill_suggestions", true, false)]
    #[test_case("goal_prescreen", false, true)]
    #[test_case("subagent_routing", false, true)]
    #[test_case("question_tool_nudge", true, false)]
    #[test_case("memory_search", false, true)]
    #[test_case("memory_recall", true, false)]
    #[test_case("memory_duplicates", true, false)]
    #[test_case("compaction_retention", false, true)]
    #[test_case("output_pointers", true, false)]
    #[test_case("source_code_context", false, true)]
    fn feature_modes_are_validated(feature: &str, advise: bool, enforce: bool) {
        for (mode, allowed) in [
            ("off", true),
            ("shadow", true),
            ("advise", advise),
            ("enforce", enforce),
        ] {
            let raw: RawDecisionsConfig =
                toml::from_str(&format!("[features]\n{feature} = '{mode}'")).unwrap();
            assert_eq!(raw.resolve().is_ok(), allowed, "{feature}: {mode}");
        }
    }

    #[test]
    fn global_layers_set_the_two_deadlines_independently() {
        let mut global: RawConfig = toml::from_str(&format!(
            "[decisions]\ntimeout_ms = {CUSTOM_TIMEOUT_MS}\nrerank_timeout_ms = {DEFAULT_RERANK_TIMEOUT_MS}"
        ))
        .unwrap();
        global.merge_global(
            toml::from_str(&format!(
                "[decisions]\nrerank_timeout_ms = {CUSTOM_RERANK_TIMEOUT_MS}"
            ))
            .unwrap(),
        );
        let config = global.into_config(false).unwrap().decisions;
        assert_eq!(config.timeout_ms, CUSTOM_TIMEOUT_MS);
        assert_eq!(config.rerank_timeout_ms, CUSTOM_RERANK_TIMEOUT_MS);
    }

    #[test_case(FeatureMode::Off)]
    #[test_case(FeatureMode::Shadow)]
    #[test_case(FeatureMode::Advise)]
    #[test_case(FeatureMode::Enforce)]
    fn feature_mode_names_match_their_config_spelling(mode: FeatureMode) {
        assert_eq!(serde_json::to_value(&mode).unwrap(), mode.as_str());
    }

    #[test_case("timeout_ms = 999999", "timeout_ms")]
    #[test_case("rerank_timeout_ms = 999999", "rerank_timeout_ms")]
    #[test_case("rerank_timeout_ms = 1", "rerank_timeout_ms")]
    #[test_case("log = true", "log")]
    #[test_case("log_retention_days = 365", "log_retention_days")]
    #[test_case("[decisions.thresholds]\nauto_flag = 1.0", "thresholds")]
    #[test_case("[decisions.thresholds]\nquestion_tool_nudge = 0.5", "thresholds")]
    #[test_case("[decisions.thresholds]\nrelevance_floor = 0.0", "thresholds")]
    #[test_case(
        "[decisions.features]\nmemory_search = 'enforce'",
        "features.memory_search"
    )]
    #[test_case(
        "[decisions.features]\nmemory_recall = 'shadow'",
        "features.memory_recall"
    )]
    #[test_case(
        "[decisions.features]\nmemory_duplicates = 'advise'",
        "features.memory_duplicates"
    )]
    #[test_case(
        "[decisions.features]\ncompaction_retention = 'enforce'",
        "features.compaction_retention"
    )]
    #[test_case(
        "[decisions.features]\noutput_pointers = 'advise'",
        "features.output_pointers"
    )]
    #[test_case(
        "[decisions.features]\nsource_code_context = 'shadow'",
        "features.source_code_context"
    )]
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
    fn project_cannot_enable_or_weaken(global_only: &str, field: &str) {
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
        let mut global: RawConfig = toml::from_str("[decisions]\nlog = true\n[decisions.features]\npermission_advice = 'advise'\nauto_screening = 'enforce'").unwrap();
        let project = toml::from_str("[decisions]\nlog = false\nlog_retention_days = 7\n[decisions.features]\npermission_advice = 'off'").unwrap();
        global.merge(project);
        let config = global.into_config(false).unwrap().decisions;
        assert_eq!(config.features.permission_advice, FeatureMode::Off);
        assert_eq!(config.features.auto_screening, FeatureMode::Enforce);
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

    #[test_case("question_tool_nudge", FeatureMode::Advise, true; "question_tool_nudge_disabled")]
    #[test_case("question_tool_nudge", FeatureMode::Advise, false; "question_tool_nudge_preserved")]
    #[test_case("memory_search", FeatureMode::Enforce, true; "memory_search_disabled")]
    #[test_case("memory_search", FeatureMode::Enforce, false; "memory_search_preserved")]
    #[test_case("memory_recall", FeatureMode::Advise, true; "memory_recall_disabled")]
    #[test_case("memory_duplicates", FeatureMode::Advise, true; "memory_duplicates_disabled")]
    #[test_case("compaction_retention", FeatureMode::Enforce, true; "compaction_retention_disabled")]
    #[test_case("output_pointers", FeatureMode::Advise, true; "output_pointers_disabled")]
    #[test_case("source_code_context", FeatureMode::Enforce, false; "source_code_context_preserved")]
    fn project_can_only_disable_features(feature: &str, global_mode: FeatureMode, disable: bool) {
        let mut global: RawConfig = toml::from_str(&format!(
            "[decisions.features]\n{feature} = '{}'",
            global_mode.as_str()
        ))
        .unwrap();
        let source = if disable {
            format!("[decisions.features]\n{feature} = 'off'")
        } else {
            String::new()
        };
        global.merge(toml::from_str(&source).unwrap());
        let features = global.into_config(false).unwrap().decisions.features;
        let expected = if disable {
            FeatureMode::Off
        } else {
            global_mode
        };
        assert_eq!(
            serde_json::to_value(&features).unwrap()[feature],
            expected.as_str()
        );
    }

    #[test_case("timeout_ms = 0", "timeout_ms")]
    #[test_case("rerank_timeout_ms = 0", "rerank_timeout_ms")]
    #[test_case("log_retention_days = 0", "log_retention_days")]
    #[test_case("[thresholds]\nrelevance_floor = -0.1", "thresholds.relevance_floor")]
    #[test_case("[thresholds]\nrelevance_floor = 1.1", "thresholds.relevance_floor")]
    #[test_case("[thresholds]\nrelevance_floor = nan", "thresholds.relevance_floor")]
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
            matches!(raw.resolve(), Err(DecisionsConfigError::Invalid { field: actual, .. }) if actual == field)
        );
    }

    #[test_case("api_key = 'secret'")]
    #[test_case("auto_screening_restricted = false")]
    #[test_case("[features]\nunknown = 'shadow'")]
    #[test_case("[features]\ncode_context = 'shadow'")]
    #[test_case("[thresholds]\nunknown = 0.1")]
    fn unknown_settings_are_not_silently_ignored(source: &str) {
        assert!(toml::from_str::<RawDecisionsConfig>(source).is_err());
    }

    #[test]
    fn removed_endpoint_key_is_unknown() {
        let error =
            toml::from_str::<RawConfig>(&format!("[decisions]\nendpoint = '{LOCAL_ENDPOINT}'"))
                .unwrap_err();
        assert!(error.to_string().contains(REMOVED_ENDPOINT_MESSAGE));
    }
}
