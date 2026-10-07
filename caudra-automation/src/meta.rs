use std::collections::BTreeSet;
use std::net::IpAddr;
use std::time::Duration;

use caudra_script::{HeaderError, SandboxLimits, ScalarKind, parse_header, restricted_engine};
use jiff::tz::TimeZone;
use rhai::{ASTNode, BinaryExpr, Expr, OptimizationLevel, Stmt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use url::{Host, Url};

use crate::args::{ARGS_VARIABLE, ArgDecl, parse_decls};
use crate::event::{
    Admission, Audience, EventDetail, GoalVerdict, InputKind, WorkState, WorkflowStatus,
};

pub const META_VARIABLE: &str = "meta";
pub const MAX_SOURCE_BYTES: usize = 64 * 1024;
pub const MAX_NAME_BYTES: usize = 64;
pub const MAX_DESCRIPTION_BYTES: usize = 512;
pub const MAX_TRIGGERS: usize = 8;
/// The longest `after` delay or `cooldown`.
pub const MAX_WAIT: Duration = Duration::from_hours(24);
/// The shortest `every` period: schedules read the wall clock once a minute.
pub const MIN_EVERY: Duration = Duration::from_mins(1);
pub const DEFAULT_MAX_PER_HOUR: u32 = 12;
pub const MAX_PER_HOUR: u32 = 600;
const MIN_PER_HOUR: u32 = 1;
/// The most entries a header list holds.
pub const MAX_ENTRIES: usize = 16;
pub const MAX_SECRET_BYTES: usize = 128;
const MAX_HANDLE_BYTES: usize = 32;
const MAX_LABEL_BYTES: usize = 256;
/// `messaging.publish` lists it to allow `broadcast()`.
pub const BROADCAST: &str = "broadcast";
/// Ends a sender pattern; alone, it matches every sender.
pub const NAME_WILDCARD: &str = "*";
const NAME_PREFIX: char = '@';
const PREFIX_WILDCARD: &str = "-*";
const HTTPS: &str = "https";
const HTTP: &str = "http";
const ORIGIN_PATH: &str = "/";
const LOCALHOST: &str = "localhost";
const LOCALHOST_SUFFIX: &str = ".localhost";
const CLOCK_SEPARATOR: char = ':';
const CLOCK_DIGITS: usize = 2;
const HOURS_PER_DAY: u8 = 24;
const MINUTES_PER_HOUR: u8 = 60;
const SCRIPT_HINT: &str = "automation scripts; end a firing with return or skip()";
const HEADER_SCALARS: [ScalarKind; 4] = [
    ScalarKind::String,
    ScalarKind::Integer,
    ScalarKind::Float,
    ScalarKind::Bool,
];
const SANDBOX_LIMITS: SandboxLimits = SandboxLimits {
    max_operations: 1_000_000,
    max_call_levels: 64,
    max_expr_depth: 128,
    max_string_size: 1024 * 1024,
    max_array_size: 65_536,
    max_map_size: 65_536,
};
const DEFAULT_INPUTS: [InputKind; 5] = [
    InputKind::Permission,
    InputKind::Question,
    InputKind::Plan,
    InputKind::Auth,
    InputKind::Plugin,
];
const DEFAULT_VERDICTS: [GoalVerdict; 3] = [
    GoalVerdict::Met,
    GoalVerdict::Impossible,
    GoalVerdict::Cleared,
];
const DEFAULT_AUDIENCES: [Audience; 3] = [Audience::Direct, Audience::Topic, Audience::Broadcast];
const DEFAULT_ADMISSIONS: [Admission; 1] = [Admission::Queued];
const DEFAULT_STATES: [WorkState; 3] = [
    WorkState::Completed,
    WorkState::Failed,
    WorkState::Cancelled,
];
const DEFAULT_STATUSES: [WorkflowStatus; 4] = [
    WorkflowStatus::Completed,
    WorkflowStatus::Failed,
    WorkflowStatus::Cancelled,
    WorkflowStatus::Interrupted,
];
const DEFAULT_CATCH_UP: CatchUp = CatchUp::Once;
const FIELD_NAME: &str = "name";
const FIELD_DESCRIPTION: &str = "description";
const FIELD_TRIGGERS: &str = "triggers";
const FIELD_ARGS: &str = "args";
const FIELD_LIMITS: &str = "limits";
const FIELD_COOLDOWN: &str = "cooldown";
const FIELD_MAX_PER_HOUR: &str = "max_per_hour";
const FIELD_NETWORK: &str = "network";
const FIELD_SECRETS: &str = "secrets";
const FIELD_MESSAGING: &str = "messaging";
const FIELD_SEND: &str = "send";
const FIELD_PUBLISH: &str = "publish";
const FIELD_WORKFLOWS: &str = "workflows";
const FIELD_TIMEZONE: &str = "timezone";
const FIELD_AFTER: &str = "after";
const FIELD_INPUTS: &str = "inputs";
const FIELD_VERDICTS: &str = "verdicts";
const FIELD_AUDIENCES: &str = "audiences";
const FIELD_TOPICS: &str = "topics";
const FIELD_SENDERS: &str = "senders";
const FIELD_SCRIPTS: &str = "scripts";
const FIELD_ADMISSIONS: &str = "admissions";
const FIELD_CONSUME: &str = "consume";
const FIELD_GROUPS: &str = "groups";
const FIELD_STATES: &str = "states";
const FIELD_STATUSES: &str = "statuses";
const FIELD_EVERY: &str = "every";
const FIELD_AT: &str = "at";
const FIELD_WEEKDAYS: &str = "weekdays";
pub const INVALID_NAME: &str = "must be kebab-case: lowercase ASCII letters or digits separated by single hyphens, at most 64 bytes";
pub const BLANK_DESCRIPTION: &str = "must not be blank";
pub const DESCRIPTION_TOO_LONG: &str = "must be at most 512 bytes";
pub const TRIGGER_COUNT: &str = "must hold 1 to 8 triggers";
pub const EXPECTED_DURATION: &str = "expected a duration such as \"2m\"";
pub const WAIT_TOO_LONG: &str = "must be at most 24h";
pub const EVERY_TOO_SHORT: &str = "must be at least 1m";
pub const EXPECTED_CLOCK: &str = "expected a 24-hour time such as \"09:00\"";
pub const CADENCE_REQUIRED: &str = "needs exactly one of every and at";
pub const WEEKDAYS_NEED_AT: &str = "applies only with at";
pub const CONSUME_NEEDS_QUEUED: &str = "needs admissions: [\"queued\"]";
pub const EMPTY_LIST: &str = "must not be empty";
pub const TOO_MANY_ENTRIES: &str = "must hold at most 16 entries";
pub const DUPLICATE_ENTRY: &str = "repeats an earlier entry";
pub const INVALID_SENDER: &str = "expected \"@name\", \"@prefix-*\" or \"*\"";
pub const INVALID_LABEL: &str =
    "expected a script label of 1 to 256 bytes without control or invisible characters";
pub const INVALID_GROUP: &str = "expected a consumer group name: 1 to 32 lowercase letters, digits and hyphens, starting with a letter or digit";
pub const INVALID_WORKFLOW: &str = "expected a kebab-case workflow name of at most 64 bytes";
pub const PER_HOUR_RANGE: &str = "must be from 1 to 600";
pub const INVALID_ORIGIN: &str =
    "expected an origin such as \"https://api.example.com\", without a path, query or credentials";
pub const INSECURE_ORIGIN: &str =
    "http:// is allowed only for a loopback or private host; use https://";
pub const INVALID_SECRET: &str = "expected an environment variable name such as \"GITHUB_TOKEN\": uppercase letters, digits and underscores, at most 128 bytes";
pub const UNKNOWN_TIMEZONE: &str = "expected an IANA time zone such as \"Europe/Berlin\"";
pub const ALWAYS_NEEDS_DEFAULT: &str = "needs a default_value, because arm is \"always\"";

/// The validated `let meta = #{…}` header of an automation script, read without running it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AutomationMeta {
    pub name: String,
    pub description: String,
    pub triggers: Vec<Trigger>,
    pub args: Vec<ArgDecl>,
    pub limits: AutomationLimits,
    /// Origins `http()` may reach, normalised to `scheme://host[:port]`.
    pub network: Vec<String>,
    /// Environment variables `http()` may read as credentials or secret URLs.
    pub secrets: Vec<String>,
    pub messaging: MessagingCaps,
    /// Workflows `start_workflow` may launch.
    pub workflows: Vec<String>,
    /// IANA zone for `now()` and schedules; the system zone when unset.
    pub timezone: Option<String>,
    pub arm: ArmMode,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    Armed,
    Idle {
        delay: Duration,
    },
    NeedsInput {
        delay: Duration,
        inputs: Vec<InputKind>,
    },
    GoalFinished {
        verdicts: Vec<GoalVerdict>,
    },
    MessageReceived(MessageFilter),
    WorkFinished {
        /// Every group the session published to when empty.
        groups: Vec<String>,
        states: Vec<WorkState>,
    },
    WorkflowFinished {
        /// Every workflow when empty.
        workflows: Vec<String>,
        statuses: Vec<WorkflowStatus>,
    },
    Schedule(Schedule),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageFilter {
    pub audiences: Vec<Audience>,
    /// Topic patterns in the messaging topic grammar; any topic when empty.
    pub topics: Vec<String>,
    /// `@name`, `@prefix-*` or `*`. With `scripts`, the sender must match one of either.
    pub senders: Vec<String>,
    /// Script labels.
    pub scripts: Vec<String>,
    pub admissions: Vec<Admission>,
    pub from_automations: bool,
    /// Matching messages go to the automation instead of the model.
    pub consume: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Schedule {
    pub cadence: Cadence,
    pub catch_up: CatchUp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cadence {
    Every(Duration),
    At {
        hour: u8,
        minute: u8,
        /// Every day when empty.
        weekdays: Vec<Weekday>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatchUp {
    Once,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationLimits {
    pub cooldown: Duration,
    pub max_per_hour: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagingCaps {
    /// Lets the script answer the sender of a message it consumed.
    pub reply: bool,
    /// `@name`, `@prefix-*` or `*`.
    pub send: Vec<String>,
    /// Concrete topics, or `broadcast`.
    pub publish: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArmMode {
    #[default]
    Manual,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    Armed,
    Idle,
    NeedsInput,
    GoalFinished,
    MessageReceived,
    WorkFinished,
    WorkflowFinished,
    Schedule,
}

impl TriggerKind {
    /// A limit defers a one-shot event and retries it; it records a recurring one as
    /// `rate_limited`.
    pub const fn is_one_shot(self) -> bool {
        !matches!(self, Self::Idle | Self::NeedsInput | Self::Schedule)
    }

    /// A waiting event of these kinds is replaced by a newer one, and dropped at restart,
    /// because resume fires `armed` and schedules apply `catch_up`.
    pub const fn coalesces(self) -> bool {
        matches!(
            self,
            Self::Armed | Self::Idle | Self::NeedsInput | Self::Schedule
        )
    }
}

impl Trigger {
    pub const fn kind(&self) -> TriggerKind {
        match self {
            Self::Armed => TriggerKind::Armed,
            Self::Idle { .. } => TriggerKind::Idle,
            Self::NeedsInput { .. } => TriggerKind::NeedsInput,
            Self::GoalFinished { .. } => TriggerKind::GoalFinished,
            Self::MessageReceived(_) => TriggerKind::MessageReceived,
            Self::WorkFinished { .. } => TriggerKind::WorkFinished,
            Self::WorkflowFinished { .. } => TriggerKind::WorkflowFinished,
            Self::Schedule(_) => TriggerKind::Schedule,
        }
    }
}

impl EventDetail {
    pub const fn kind(&self) -> TriggerKind {
        match self {
            Self::Armed { .. } => TriggerKind::Armed,
            Self::Idle(_) => TriggerKind::Idle,
            Self::NeedsInput { .. } => TriggerKind::NeedsInput,
            Self::GoalFinished(_) => TriggerKind::GoalFinished,
            Self::MessageReceived(_) => TriggerKind::MessageReceived,
            Self::WorkFinished(_) => TriggerKind::WorkFinished,
            Self::WorkflowFinished(_) => TriggerKind::WorkflowFinished,
            Self::Schedule { .. } => TriggerKind::Schedule,
        }
    }
}

/// What a script reads from `args` and calls, found by walking its AST without running it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct References {
    /// Names read as `args.name` or `args["name"]`.
    pub args: BTreeSet<String>,
    /// Every function and method called by name; operators are left out.
    pub calls: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MetaError {
    #[error("the source is {bytes} bytes; the limit is {MAX_SOURCE_BYTES}")]
    SourceTooLarge { bytes: usize },
    #[error("the first statement must be `let {META_VARIABLE} = #{{ … }};`")]
    NotFirst,
    #[error(transparent)]
    Header(HeaderError),
    /// `path` addresses the offending value, such as `meta.triggers[1].after`.
    #[error("{path}: {reason}")]
    Invalid { path: String, reason: String },
}

impl From<HeaderError> for MetaError {
    fn from(error: HeaderError) -> Self {
        match error {
            HeaderError::NotFirst => Self::NotFirst,
            other => Self::Header(other),
        }
    }
}

impl Default for AutomationLimits {
    fn default() -> Self {
        Self {
            cooldown: Duration::ZERO,
            max_per_hour: DEFAULT_MAX_PER_HOUR,
        }
    }
}

/// Sections whose errors deserve their own path stay JSON until they are parsed on their own.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMeta {
    name: String,
    description: String,
    triggers: Vec<Value>,
    #[serde(default)]
    args: Map<String, Value>,
    limits: Option<Value>,
    #[serde(default)]
    network: Vec<String>,
    #[serde(default)]
    secrets: Vec<String>,
    messaging: Option<Value>,
    #[serde(default)]
    workflows: Vec<String>,
    timezone: Option<String>,
    #[serde(default)]
    arm: ArmMode,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RawTrigger {
    Armed {},
    Idle {
        after: Option<String>,
    },
    NeedsInput {
        after: Option<String>,
        inputs: Option<Vec<InputKind>>,
    },
    GoalFinished {
        verdicts: Option<Vec<GoalVerdict>>,
    },
    MessageReceived(RawMessageFilter),
    WorkFinished {
        #[serde(default)]
        groups: Vec<String>,
        states: Option<Vec<WorkState>>,
    },
    WorkflowFinished {
        #[serde(default)]
        workflows: Vec<String>,
        statuses: Option<Vec<WorkflowStatus>>,
    },
    Schedule(RawSchedule),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMessageFilter {
    audiences: Option<Vec<Audience>>,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    senders: Vec<String>,
    #[serde(default)]
    scripts: Vec<String>,
    admissions: Option<Vec<Admission>>,
    #[serde(default)]
    from_automations: bool,
    #[serde(default)]
    consume: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSchedule {
    every: Option<String>,
    at: Option<String>,
    weekdays: Option<Vec<Weekday>>,
    catch_up: Option<CatchUp>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    cooldown: Option<String>,
    max_per_hour: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMessaging {
    #[serde(default)]
    reply: bool,
    #[serde(default)]
    send: Vec<String>,
    #[serde(default)]
    publish: Vec<String>,
}

/// The bounds a header is parsed under and a firing runs under.
pub fn sandbox_limits() -> SandboxLimits {
    SANDBOX_LIMITS
}

/// Reads and validates the header without running the script. An invalid value is reported
/// with its path, such as `meta.triggers[1].after`.
pub fn parse_meta(source: &str) -> Result<AutomationMeta, MetaError> {
    within_size(source)?;
    let header = parse_header(source, &SANDBOX_LIMITS, META_VARIABLE, &HEADER_SCALARS)?;
    let raw: RawMeta = section(META_VARIABLE, header)?;
    let path = |key: &str| field(META_VARIABLE, key);
    if !is_valid_name(&raw.name) {
        return Err(invalid(&path(FIELD_NAME), INVALID_NAME));
    }
    if raw.description.trim().is_empty() {
        return Err(invalid(&path(FIELD_DESCRIPTION), BLANK_DESCRIPTION));
    }
    if raw.description.len() > MAX_DESCRIPTION_BYTES {
        return Err(invalid(&path(FIELD_DESCRIPTION), DESCRIPTION_TOO_LONG));
    }
    let triggers_path = path(FIELD_TRIGGERS);
    if raw.triggers.is_empty() || raw.triggers.len() > MAX_TRIGGERS {
        return Err(invalid(&triggers_path, TRIGGER_COUNT));
    }
    let triggers = raw
        .triggers
        .into_iter()
        .enumerate()
        .map(|(index, trigger)| parse_trigger(&item(&triggers_path, index), trigger))
        .collect::<Result<Vec<_>, _>>()?;
    let args_path = path(FIELD_ARGS);
    let args = parse_decls(&args_path, raw.args)?;
    if raw.arm == ArmMode::Always
        && let Some(required) = args.iter().find(|decl| decl.spec.default.is_none())
    {
        return Err(invalid(
            &field(&args_path, &required.name),
            ALWAYS_NEEDS_DEFAULT,
        ));
    }
    let limits = raw
        .limits
        .map(|limits| parse_limits(&path(FIELD_LIMITS), limits))
        .transpose()?
        .unwrap_or_default();
    let messaging = raw
        .messaging
        .map(|messaging| parse_messaging(&path(FIELD_MESSAGING), messaging))
        .transpose()?
        .unwrap_or_default();
    if raw
        .timezone
        .as_deref()
        .is_some_and(|zone| TimeZone::get(zone).is_err())
    {
        return Err(invalid(&path(FIELD_TIMEZONE), UNKNOWN_TIMEZONE));
    }
    Ok(AutomationMeta {
        name: raw.name,
        description: raw.description,
        triggers,
        args,
        limits,
        network: entries(&path(FIELD_NETWORK), raw.network, origin)?,
        secrets: entries(
            &path(FIELD_SECRETS),
            raw.secrets,
            checked(is_secret_name, INVALID_SECRET),
        )?,
        messaging,
        workflows: entries(
            &path(FIELD_WORKFLOWS),
            raw.workflows,
            checked(is_valid_name, INVALID_WORKFLOW),
        )?,
        timezone: raw.timezone,
        arm: raw.arm,
    })
}

/// The args a script reads by name and the functions it calls, across its body, closures and
/// functions, so `validate` can flag an `args.x` that `meta.args` does not declare.
pub fn references(source: &str) -> Result<References, MetaError> {
    within_size(source)?;
    let mut engine = restricted_engine(&SANDBOX_LIMITS, SCRIPT_HINT);
    engine.set_optimization_level(OptimizationLevel::None);
    let ast = engine
        .compile(source)
        .map_err(|error| MetaError::Header(HeaderError::Parse(error)))?;
    let mut found = References::default();
    ast.walk(&mut |path: &[ASTNode]| {
        match path.last() {
            Some(ASTNode::Expr(Expr::Dot(chain, ..) | Expr::Index(chain, ..))) => {
                found.args.extend(arg_read(chain).map(str::to_owned));
            }
            Some(
                ASTNode::Expr(Expr::FnCall(call, _) | Expr::MethodCall(call, _))
                | ASTNode::Stmt(Stmt::FnCall(call, _)),
            ) if !call.is_operator_call() => {
                found.calls.insert(call.name.to_string());
            }
            _ => {}
        }
        true
    });
    Ok(found)
}

/// The name in `args.name…` or `args["name"]…`. Chains nest to the right, so the name is the
/// right side itself or the left end of the rest of the chain.
fn arg_read(chain: &BinaryExpr) -> Option<&str> {
    let Expr::Variable(variable, ..) = &chain.lhs else {
        return None;
    };
    if variable.1.as_str() != ARGS_VARIABLE {
        return None;
    }
    let accessed = match &chain.rhs {
        Expr::Dot(rest, ..) | Expr::Index(rest, ..) => &rest.lhs,
        accessed => accessed,
    };
    match accessed {
        Expr::Property(property, _) => Some(property.2.as_str()),
        Expr::StringConstant(name, _) => Some(name.as_str()),
        _ => None,
    }
}

fn parse_trigger(path: &str, value: Value) -> Result<Trigger, MetaError> {
    let option = |key: &str| field(path, key);
    Ok(match section::<RawTrigger>(path, value)? {
        RawTrigger::Armed {} => Trigger::Armed,
        RawTrigger::Idle { after } => Trigger::Idle {
            delay: wait(&option(FIELD_AFTER), after)?,
        },
        RawTrigger::NeedsInput { after, inputs } => Trigger::NeedsInput {
            delay: wait(&option(FIELD_AFTER), after)?,
            inputs: options(&option(FIELD_INPUTS), inputs, &DEFAULT_INPUTS)?,
        },
        RawTrigger::GoalFinished { verdicts } => Trigger::GoalFinished {
            verdicts: options(&option(FIELD_VERDICTS), verdicts, &DEFAULT_VERDICTS)?,
        },
        RawTrigger::MessageReceived(filter) => {
            Trigger::MessageReceived(parse_filter(path, filter)?)
        }
        RawTrigger::WorkFinished { groups, states } => Trigger::WorkFinished {
            groups: entries(
                &option(FIELD_GROUPS),
                groups,
                checked(is_handle, INVALID_GROUP),
            )?,
            states: options(&option(FIELD_STATES), states, &DEFAULT_STATES)?,
        },
        RawTrigger::WorkflowFinished {
            workflows,
            statuses,
        } => Trigger::WorkflowFinished {
            workflows: entries(
                &option(FIELD_WORKFLOWS),
                workflows,
                checked(is_valid_name, INVALID_WORKFLOW),
            )?,
            statuses: options(&option(FIELD_STATUSES), statuses, &DEFAULT_STATUSES)?,
        },
        RawTrigger::Schedule(schedule) => Trigger::Schedule(parse_schedule(path, schedule)?),
    })
}

fn parse_filter(path: &str, raw: RawMessageFilter) -> Result<MessageFilter, MetaError> {
    let option = |key: &str| field(path, key);
    let admissions = options(
        &option(FIELD_ADMISSIONS),
        raw.admissions,
        &DEFAULT_ADMISSIONS,
    )?;
    if raw.consume && admissions != [Admission::Queued] {
        return Err(invalid(&option(FIELD_CONSUME), CONSUME_NEEDS_QUEUED));
    }
    Ok(MessageFilter {
        audiences: options(&option(FIELD_AUDIENCES), raw.audiences, &DEFAULT_AUDIENCES)?,
        topics: entries(&option(FIELD_TOPICS), raw.topics, opaque)?,
        senders: entries(
            &option(FIELD_SENDERS),
            raw.senders,
            checked(is_sender_pattern, INVALID_SENDER),
        )?,
        scripts: entries(
            &option(FIELD_SCRIPTS),
            raw.scripts,
            checked(is_label, INVALID_LABEL),
        )?,
        admissions,
        from_automations: raw.from_automations,
        consume: raw.consume,
    })
}

fn parse_schedule(path: &str, raw: RawSchedule) -> Result<Schedule, MetaError> {
    let option = |key: &str| field(path, key);
    let cadence = match (raw.every, raw.at) {
        (Some(every), None) => {
            if raw.weekdays.is_some() {
                return Err(invalid(&option(FIELD_WEEKDAYS), WEEKDAYS_NEED_AT));
            }
            let every_path = option(FIELD_EVERY);
            let period = duration(&every_path, &every)?;
            if period < MIN_EVERY {
                return Err(invalid(&every_path, EVERY_TOO_SHORT));
            }
            Cadence::Every(period)
        }
        (None, Some(at)) => {
            let (hour, minute) =
                clock(&at).ok_or_else(|| invalid(&option(FIELD_AT), EXPECTED_CLOCK))?;
            Cadence::At {
                hour,
                minute,
                weekdays: entries(
                    &option(FIELD_WEEKDAYS),
                    raw.weekdays.unwrap_or_default(),
                    |_, day| Ok(day),
                )?,
            }
        }
        _ => return Err(invalid(path, CADENCE_REQUIRED)),
    };
    Ok(Schedule {
        cadence,
        catch_up: raw.catch_up.unwrap_or(DEFAULT_CATCH_UP),
    })
}

fn parse_limits(path: &str, value: Value) -> Result<AutomationLimits, MetaError> {
    let raw: RawLimits = section(path, value)?;
    let max_per_hour = match raw.max_per_hour {
        None => DEFAULT_MAX_PER_HOUR,
        Some(given) => u32::try_from(given)
            .ok()
            .filter(|rate| (MIN_PER_HOUR..=MAX_PER_HOUR).contains(rate))
            .ok_or_else(|| invalid(&field(path, FIELD_MAX_PER_HOUR), PER_HOUR_RANGE))?,
    };
    Ok(AutomationLimits {
        cooldown: wait(&field(path, FIELD_COOLDOWN), raw.cooldown)?,
        max_per_hour,
    })
}

fn parse_messaging(path: &str, value: Value) -> Result<MessagingCaps, MetaError> {
    let raw: RawMessaging = section(path, value)?;
    Ok(MessagingCaps {
        reply: raw.reply,
        send: entries(
            &field(path, FIELD_SEND),
            raw.send,
            checked(is_sender_pattern, INVALID_SENDER),
        )?,
        publish: entries(&field(path, FIELD_PUBLISH), raw.publish, opaque)?,
    })
}

fn within_size(source: &str) -> Result<(), MetaError> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(MetaError::SourceTooLarge {
            bytes: source.len(),
        });
    }
    Ok(())
}

/// Deserializes one section of the header, reporting shape errors under its path.
pub(crate) fn section<T: DeserializeOwned>(path: &str, value: Value) -> Result<T, MetaError> {
    serde_json::from_value(value).map_err(|error| invalid(path, error.to_string()))
}

pub(crate) fn invalid(path: &str, reason: impl Into<String>) -> MetaError {
    MetaError::Invalid {
        path: path.to_owned(),
        reason: reason.into(),
    }
}

pub(crate) fn field(path: &str, key: &str) -> String {
    format!("{path}.{key}")
}

fn item(path: &str, index: usize) -> String {
    format!("{path}[{index}]")
}

/// Parses each entry of a list of at most [`MAX_ENTRIES`] and refuses a repeated result.
pub(crate) fn entries<I, T: PartialEq>(
    path: &str,
    given: Vec<I>,
    parse: impl Fn(&str, I) -> Result<T, MetaError>,
) -> Result<Vec<T>, MetaError> {
    if given.len() > MAX_ENTRIES {
        return Err(invalid(path, TOO_MANY_ENTRIES));
    }
    let mut parsed = Vec::with_capacity(given.len());
    for (index, entry) in given.into_iter().enumerate() {
        let entry_path = item(path, index);
        let value = parse(&entry_path, entry)?;
        if parsed.contains(&value) {
            return Err(invalid(&entry_path, DUPLICATE_ENTRY));
        }
        parsed.push(value);
    }
    Ok(parsed)
}

/// `default` when the option is absent; a given list must name at least one.
fn options<T: Clone + PartialEq>(
    path: &str,
    given: Option<Vec<T>>,
    default: &[T],
) -> Result<Vec<T>, MetaError> {
    match given {
        None => Ok(default.to_vec()),
        Some(given) if given.is_empty() => Err(invalid(path, EMPTY_LIST)),
        Some(given) => entries(path, given, |_, option| Ok(option)),
    }
}

fn checked(
    valid: fn(&str) -> bool,
    reason: &'static str,
) -> impl Fn(&str, String) -> Result<String, MetaError> {
    move |path, entry| {
        if valid(&entry) {
            Ok(entry)
        } else {
            Err(invalid(path, reason))
        }
    }
}

/// Topics stay opaque here: the runtime checks them against the messaging grammar.
fn opaque(_path: &str, topic: String) -> Result<String, MetaError> {
    Ok(topic)
}

fn duration(path: &str, value: &str) -> Result<Duration, MetaError> {
    humantime::parse_duration(value).map_err(|_| invalid(path, EXPECTED_DURATION))
}

/// An optional delay of at most [`MAX_WAIT`], zero when absent.
fn wait(path: &str, value: Option<String>) -> Result<Duration, MetaError> {
    let Some(value) = value else {
        return Ok(Duration::ZERO);
    };
    let wait = duration(path, &value)?;
    if wait > MAX_WAIT {
        return Err(invalid(path, WAIT_TOO_LONG));
    }
    Ok(wait)
}

/// `HH:MM` on a 24-hour clock.
fn clock(value: &str) -> Option<(u8, u8)> {
    let (hour, minute) = value.split_once(CLOCK_SEPARATOR)?;
    let (hour, minute) = (two_digits(hour)?, two_digits(minute)?);
    (hour < HOURS_PER_DAY && minute < MINUTES_PER_HOUR).then_some((hour, minute))
}

fn two_digits(part: &str) -> Option<u8> {
    if part.len() == CLOCK_DIGITS && part.bytes().all(|byte| byte.is_ascii_digit()) {
        part.parse().ok()
    } else {
        None
    }
}

/// Whether `name` can name an automation: kebab-case (`^[a-z0-9]+(-[a-z0-9]+)*$`) within
/// [`MAX_NAME_BYTES`], as workflow names are.
pub fn is_valid_name(name: &str) -> bool {
    name.len() <= MAX_NAME_BYTES
        && name.split('-').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

/// A messaging name or consumer group name, without the `@`.
fn is_handle(name: &str) -> bool {
    name.len() <= MAX_HANDLE_BYTES
        && name
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// `@name`, `@prefix-*`, or `*` for every sender.
fn is_sender_pattern(pattern: &str) -> bool {
    pattern == NAME_WILDCARD
        || pattern
            .strip_prefix(NAME_PREFIX)
            .is_some_and(|name| is_handle(name.strip_suffix(PREFIX_WILDCARD).unwrap_or(name)))
}

/// A script sender's label, as `caudra message --from` accepts it.
fn is_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_LABEL_BYTES
        && !label
            .chars()
            .any(|character| character.is_control() || deceptive(character))
}

/// Invisible and bidirectional characters, which messaging refuses in labels.
fn deceptive(character: char) -> bool {
    matches!(
        character,
        '\u{00ad}'
            | '\u{061c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
    )
}

/// `[A-Z_][A-Z0-9_]*` within [`MAX_SECRET_BYTES`].
fn is_secret_name(name: &str) -> bool {
    name.len() <= MAX_SECRET_BYTES
        && name
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_uppercase() || first == b'_')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

/// `scheme://host[:port]` of an https origin, or of an http one on a loopback or private host.
fn origin(path: &str, value: String) -> Result<String, MetaError> {
    let url = Url::parse(&value).map_err(|_| invalid(path, INVALID_ORIGIN))?;
    let bare = url.path() == ORIGIN_PATH
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none();
    let host = url
        .host()
        .filter(|_| bare)
        .ok_or_else(|| invalid(path, INVALID_ORIGIN))?;
    match url.scheme() {
        HTTPS => {}
        HTTP if private_host(host) => {}
        HTTP => return Err(invalid(path, INSECURE_ORIGIN)),
        _ => return Err(invalid(path, INVALID_ORIGIN)),
    }
    Ok(url.origin().ascii_serialization())
}

/// `localhost` or a name under it, or a loopback or private address literal.
pub fn private_host(host: Host<&str>) -> bool {
    let address = match host {
        Host::Domain(domain) => return domain == LOCALHOST || domain.ends_with(LOCALHOST_SUFFIX),
        Host::Ipv4(address) => IpAddr::V4(address),
        Host::Ipv6(address) => IpAddr::V6(address).to_canonical(),
    };
    match address {
        IpAddr::V4(address) => address.is_loopback() || address.is_private(),
        IpAddr::V6(address) => address.is_loopback() || address.is_unique_local(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Number, json};
    use test_case::test_case;

    use super::*;
    use crate::args::{ArgSpec, ArgType};

    const VALID_HEADER: &str = "the header is valid";
    const INVALID_HEADER: &str = "the header is invalid";
    const VALID_SOURCE: &str = "the source compiles";
    const PROBE: &str = "probe";
    const PROBE_DESCRIPTION: &str = "Probe the header";
    const ARMED: &str = r#"#{ kind: "armed" }"#;
    /// The default `max_per_hour` the plan documents.
    const PLAN_MAX_PER_HOUR: u32 = 12;
    const KEEP_GOING: &str = r##"let meta = #{
    name: "keep-going",
    description: "Work through a backlog file during work hours",
    triggers: [#{ kind: "idle", after: "2m" }],
    limits: #{ max_per_hour: 6 },
    args: #{
        file: #{ type: "string", default_value: "TODO.md", description: "Backlog with checkboxes" },
        from_hour: #{ type: "int", default_value: 9, min: 0, max: 23 },
        until_hour: #{ type: "int", default_value: 18, min: 1, max: 24 },
    },
};
let t = now();
if (t.weekday in ["sat", "sun"]) || t.hour < args.from_hour || t.hour >= args.until_hour { skip("outside work hours"); }
if event.outcome != "completed" { skip("the last turn ended " + event.outcome); }
if event.last_response.contains("BACKLOG EMPTY") { skip("the backlog is empty"); }
message("Continue with the next unchecked item in " + args.file + ". When none remain, reply with BACKLOG EMPTY.");
"##;
    const STANDUP: &str = r##"let meta = #{
    name: "standup",
    description: "Write standup bullets at 09:00 on weekdays and post them to Slack",
    triggers: [
        #{ kind: "schedule", at: "09:00", weekdays: ["mon", "tue", "wed", "thu", "fri"], catch_up: "skip" },
        #{ kind: "idle" },
    ],
    network: ["https://hooks.slack.com"],
    secrets: ["SLACK_STANDUP_URL"],
    timezone: "Europe/Berlin",
};
if event.trigger == "schedule" {
    message("Summarize yesterday's commits in this repository as three standup bullets. Reply with only the bullets.");
} else if (meta.name in event.automations) && event.outcome == "completed" {
    http(#{ method: "POST", url_env: "SLACK_STANDUP_URL", json: #{ text: event.last_response } });
}
"##;
    const GOAL_CHAIN: &str = r##"let meta = #{
    name: "goal-chain",
    description: "Pursue a list of goals in order, starting with the first when armed",
    triggers: [#{ kind: "armed" }, #{ kind: "goal_finished" }],
    args: #{
        goals: #{ type: "list", min: 1, description: "Goal conditions, in order" },
        continuation_limit: #{ type: "int", default_value: 24, min: 1, max: 100 },
    },
};
let done = state.done ?? [];
if event.trigger == "goal_finished" && event.condition == state.current {
    state.current = ();
    if event.verdict != "met" {
        notify("goal-chain stopped: verdict " + event.verdict);
        return;
    }
    done.push(event.condition);
    state.done = done;
} else if event.session.goal != () {
    skip("another goal is active");
}
let next = args.goals.find(|goal| !(goal in done));
if next == () {
    notify("goal-chain: all " + args.goals.len() + " goals are met");
    return;
}
state.current = set_goal(next, #{ continuation_limit: args.continuation_limit }).condition;
"##;
    const PAGE_ME: &str = r##"let meta = #{
    name: "page-me",
    description: "Push a phone notification when the session has waited on me for 10 minutes",
    triggers: [#{ kind: "needs_input", after: "10m" }],
    network: ["https://ntfy.sh"],
    secrets: ["NTFY_URL", "NTFY_TOKEN"],
    arm: "always",
};
let ask = switch event.input {
    "permission" => "approve " + event.tool,
    "plan" => "review a plan",
    "auth" => "sign in again",
    _ => "answer a " + event.input,
};
http(#{
    method: "POST",
    url_env: "NTFY_URL",
    bearer_env: "NTFY_TOKEN",
    headers: #{ Title: "Caudra is waiting" },
    body: event.session.title + " needs you to " + ask,
});
"##;
    const CI_WATCH: &str = r##"let meta = #{
    name: "ci-watch",
    description: "Poll GitHub Actions on main and publish new failures to ci.failures",
    triggers: [#{ kind: "schedule", every: "10m" }],
    network: ["https://api.github.com"],
    secrets: ["GITHUB_TOKEN"],
    messaging: #{ publish: ["ci.failures"] },
};
let response = http(#{
    method: "GET",
    url: "https://api.github.com/repos/acme/app/actions/runs",
    query: #{ branch: "main", per_page: "1" },
    bearer_env: "GITHUB_TOKEN",
    headers: #{ Accept: "application/vnd.github+json", "User-Agent": "caudra-ci-watch" },
});
if response.status != 200 { log("GitHub returned " + response.status); return; }
let run = response.json.workflow_runs[0];
if run == () || run.id == state.last_run { return; }
state.last_run = run.id;
if run.conclusion == "failure" {
    publish("ci.failures", "CI failed on main: " + run.display_title + " " + run.html_url);
}
"##;
    const TASK_TRACKER: &str = r##"let meta = #{
    name: "task-tracker",
    description: "React to the outcomes of tasks this coordinator published",
    triggers: [#{ kind: "work_finished", groups: ["swarm-tasks"], states: ["completed", "failed", "paused"] }],
    limits: #{ max_per_hour: 30 },
};
if event.state == "completed" {
    message("A swarm task finished. Check its result, then publish follow-up tasks to swarm.tasks if any remain.", #{ attach: event });
} else {
    notify("Swarm task " + event.work + " is " + event.state + " after " + event.attempts + " attempts");
}
"##;
    const CI_TRIAGE: &str = r##"let meta = #{
    name: "ci-triage",
    description: "Start a root-cause run for each CI failure, instead of waking the model",
    triggers: [#{ kind: "message_received", topics: ["ci.failures"], senders: ["@ci-watcher"], scripts: ["nightly-ci"], consume: true }],
    workflows: ["root-cause"],
    limits: #{ max_per_hour: 2 },
};
start_workflow("root-cause", #{ failure: event.text }, #{ agent_budget: 24 });
"##;
    const RESEARCH_DESK: &str = r##"let meta = #{
    name: "research-desk",
    description: "Answer research requests from other sessions with deep-research",
    triggers: [
        #{ kind: "message_received", audiences: ["direct"], consume: true },
        #{ kind: "workflow_finished", workflows: ["deep-research"] },
    ],
    workflows: ["deep-research"],
    messaging: #{ reply: true, send: ["*"] },
};
let requests = state.requests ?? #{};
if event.trigger == "message_received" {
    if event.sender == () || !event.text.starts_with("research:") { release("not a research request"); }
    let run = start_workflow("deep-research", #{ query: event.text.sub_string(9) });
    requests[run.run_id] = #{ to: event.sender, message: event.message_id };
    try {
        reply("Started " + run.name + ". The report follows when it finishes.");
    } catch (err) {
        log("Could not acknowledge the request: " + err.message);
    }
} else if requests.contains(event.run_id) {
    let request = requests.remove(event.run_id);
    send(request.to, "Report from " + event.name + " (" + event.status + ")\n\n" + event.report, #{ reply_to: request.message });
}
state.requests = requests;
"##;
    const BARE_TRIGGERS: &str = r##"let meta = #{
    name: "probe",
    description: "Probe the header",
    triggers: [
        #{ kind: "armed" },
        #{ kind: "idle" },
        #{ kind: "needs_input" },
        #{ kind: "goal_finished" },
        #{ kind: "message_received" },
        #{ kind: "work_finished" },
        #{ kind: "workflow_finished" },
        #{ kind: "schedule", at: "02:30" },
    ],
};"##;
    const EVERY_OPTION: &str = r##"let meta = #{
    name: "probe",
    description: "Probe the header",
    triggers: [
        #{ kind: "idle", after: "90s" },
        #{ kind: "needs_input", after: "5m", inputs: ["messages", "plan"] },
        #{ kind: "goal_finished", verdicts: ["impossible"] },
        #{ kind: "message_received", audiences: ["topic"], topics: ["swarm.*"], senders: ["@worker-*", "*"], admissions: ["held"], from_automations: true },
        #{ kind: "work_finished", states: ["paused"] },
        #{ kind: "workflow_finished", statuses: ["failed", "interrupted"] },
        #{ kind: "schedule", every: "1h", catch_up: "skip" },
    ],
    limits: #{ cooldown: "30m", max_per_hour: 600 },
    messaging: #{ publish: ["swarm.status", "broadcast"] },
};"##;
    const REFERENCING: &str = r#"let meta = #{ name: "probe" };
let flag = args.flag && args["quoted"] == 1;
let next = args.goals.find(|goal| goal != args.in_closure);
args["chained"].len();
fn helper(x) { notify(args.in_function + x); }
let dynamic = args[event.key];
let unrelated = event.args + state.args.nested;
skip("done");
"#;

    /// The body calls a function nothing defines, so a header that parses was never run.
    fn script(fields: &str) -> String {
        format!("let meta = #{{ {fields} }};\nundefined_host_call();")
    }

    fn with(fields: &str) -> String {
        script(&format!(
            r#"name: "{PROBE}", description: "{PROBE_DESCRIPTION}", triggers: [{ARMED}], {fields}"#
        ))
    }

    /// `trigger` is `meta.triggers[1]`.
    fn triggered(trigger: &str) -> String {
        script(&format!(
            r#"name: "{PROBE}", description: "{PROBE_DESCRIPTION}", triggers: [{ARMED}, {trigger}]"#
        ))
    }

    fn named(name: &str, description: &str) -> String {
        script(&format!(
            r#"name: "{name}", description: "{description}", triggers: [{ARMED}]"#
        ))
    }

    fn repeated(entry: &str, count: usize) -> String {
        vec![entry; count].join(", ")
    }

    fn rejection(source: &str) -> String {
        parse_meta(source).expect_err(INVALID_HEADER).to_string()
    }

    fn at_path(path: &str, reason: &str) -> String {
        format!("{path}: {reason}")
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn rate(max_per_hour: u32) -> AutomationLimits {
        AutomationLimits {
            cooldown: Duration::ZERO,
            max_per_hour,
        }
    }

    fn header(name: &str, description: &str) -> AutomationMeta {
        AutomationMeta {
            name: name.to_owned(),
            description: description.to_owned(),
            triggers: Vec::new(),
            args: Vec::new(),
            limits: rate(PLAN_MAX_PER_HOUR),
            network: Vec::new(),
            secrets: Vec::new(),
            messaging: MessagingCaps::default(),
            workflows: Vec::new(),
            timezone: None,
            arm: ArmMode::Manual,
        }
    }

    fn arg(name: &str, spec: ArgSpec) -> ArgDecl {
        ArgDecl {
            name: name.to_owned(),
            spec,
        }
    }

    fn spec(kind: ArgType) -> ArgSpec {
        ArgSpec {
            kind,
            default: None,
            min: None,
            max: None,
            choices: Vec::new(),
            description: None,
            example: None,
        }
    }

    fn int(default: i64, min: i64, max: i64) -> ArgSpec {
        ArgSpec {
            default: Some(json!(default)),
            min: Some(Number::from(min)),
            max: Some(Number::from(max)),
            ..spec(ArgType::Int)
        }
    }

    fn default_inputs() -> Vec<InputKind> {
        vec![
            InputKind::Permission,
            InputKind::Question,
            InputKind::Plan,
            InputKind::Auth,
            InputKind::Plugin,
        ]
    }

    fn default_verdicts() -> Vec<GoalVerdict> {
        vec![
            GoalVerdict::Met,
            GoalVerdict::Impossible,
            GoalVerdict::Cleared,
        ]
    }

    fn default_filter() -> MessageFilter {
        MessageFilter {
            audiences: vec![Audience::Direct, Audience::Topic, Audience::Broadcast],
            topics: Vec::new(),
            senders: Vec::new(),
            scripts: Vec::new(),
            admissions: vec![Admission::Queued],
            from_automations: false,
            consume: false,
        }
    }

    fn default_statuses() -> Vec<WorkflowStatus> {
        vec![
            WorkflowStatus::Completed,
            WorkflowStatus::Failed,
            WorkflowStatus::Cancelled,
            WorkflowStatus::Interrupted,
        ]
    }

    fn keep_going() -> AutomationMeta {
        AutomationMeta {
            triggers: vec![Trigger::Idle {
                delay: Duration::from_mins(2),
            }],
            limits: rate(6),
            args: vec![
                arg(
                    "file",
                    ArgSpec {
                        default: Some(json!("TODO.md")),
                        description: Some("Backlog with checkboxes".to_owned()),
                        ..spec(ArgType::String)
                    },
                ),
                arg("from_hour", int(9, 0, 23)),
                arg("until_hour", int(18, 1, 24)),
            ],
            ..header(
                "keep-going",
                "Work through a backlog file during work hours",
            )
        }
    }

    fn standup() -> AutomationMeta {
        AutomationMeta {
            triggers: vec![
                Trigger::Schedule(Schedule {
                    cadence: Cadence::At {
                        hour: 9,
                        minute: 0,
                        weekdays: vec![
                            Weekday::Mon,
                            Weekday::Tue,
                            Weekday::Wed,
                            Weekday::Thu,
                            Weekday::Fri,
                        ],
                    },
                    catch_up: CatchUp::Skip,
                }),
                Trigger::Idle {
                    delay: Duration::ZERO,
                },
            ],
            network: strings(&["https://hooks.slack.com"]),
            secrets: strings(&["SLACK_STANDUP_URL"]),
            timezone: Some("Europe/Berlin".to_owned()),
            ..header(
                "standup",
                "Write standup bullets at 09:00 on weekdays and post them to Slack",
            )
        }
    }

    fn goal_chain() -> AutomationMeta {
        AutomationMeta {
            triggers: vec![
                Trigger::Armed,
                Trigger::GoalFinished {
                    verdicts: default_verdicts(),
                },
            ],
            args: vec![
                arg(
                    "goals",
                    ArgSpec {
                        min: Some(Number::from(1)),
                        description: Some("Goal conditions, in order".to_owned()),
                        ..spec(ArgType::List)
                    },
                ),
                arg("continuation_limit", int(24, 1, 100)),
            ],
            ..header(
                "goal-chain",
                "Pursue a list of goals in order, starting with the first when armed",
            )
        }
    }

    fn page_me() -> AutomationMeta {
        AutomationMeta {
            triggers: vec![Trigger::NeedsInput {
                delay: Duration::from_mins(10),
                inputs: default_inputs(),
            }],
            network: strings(&["https://ntfy.sh"]),
            secrets: strings(&["NTFY_URL", "NTFY_TOKEN"]),
            arm: ArmMode::Always,
            ..header(
                "page-me",
                "Push a phone notification when the session has waited on me for 10 minutes",
            )
        }
    }

    fn ci_watch() -> AutomationMeta {
        AutomationMeta {
            triggers: vec![Trigger::Schedule(Schedule {
                cadence: Cadence::Every(Duration::from_mins(10)),
                catch_up: CatchUp::Once,
            })],
            network: strings(&["https://api.github.com"]),
            secrets: strings(&["GITHUB_TOKEN"]),
            messaging: MessagingCaps {
                publish: strings(&["ci.failures"]),
                ..MessagingCaps::default()
            },
            ..header(
                "ci-watch",
                "Poll GitHub Actions on main and publish new failures to ci.failures",
            )
        }
    }

    fn task_tracker() -> AutomationMeta {
        AutomationMeta {
            triggers: vec![Trigger::WorkFinished {
                groups: strings(&["swarm-tasks"]),
                states: vec![WorkState::Completed, WorkState::Failed, WorkState::Paused],
            }],
            limits: rate(30),
            ..header(
                "task-tracker",
                "React to the outcomes of tasks this coordinator published",
            )
        }
    }

    fn ci_triage() -> AutomationMeta {
        AutomationMeta {
            triggers: vec![Trigger::MessageReceived(MessageFilter {
                topics: strings(&["ci.failures"]),
                senders: strings(&["@ci-watcher"]),
                scripts: strings(&["nightly-ci"]),
                consume: true,
                ..default_filter()
            })],
            workflows: strings(&["root-cause"]),
            limits: rate(2),
            ..header(
                "ci-triage",
                "Start a root-cause run for each CI failure, instead of waking the model",
            )
        }
    }

    fn research_desk() -> AutomationMeta {
        AutomationMeta {
            triggers: vec![
                Trigger::MessageReceived(MessageFilter {
                    audiences: vec![Audience::Direct],
                    consume: true,
                    ..default_filter()
                }),
                Trigger::WorkflowFinished {
                    workflows: strings(&["deep-research"]),
                    statuses: default_statuses(),
                },
            ],
            workflows: strings(&["deep-research"]),
            messaging: MessagingCaps {
                reply: true,
                send: strings(&["*"]),
                publish: Vec::new(),
            },
            ..header(
                "research-desk",
                "Answer research requests from other sessions with deep-research",
            )
        }
    }

    #[test_case(KEEP_GOING, keep_going(); "keep_going_example")]
    #[test_case(STANDUP, standup(); "standup_example")]
    #[test_case(GOAL_CHAIN, goal_chain(); "goal_chain_example")]
    #[test_case(PAGE_ME, page_me(); "page_me_example")]
    #[test_case(CI_WATCH, ci_watch(); "ci_watch_example")]
    #[test_case(TASK_TRACKER, task_tracker(); "task_tracker_example")]
    #[test_case(CI_TRIAGE, ci_triage(); "ci_triage_example")]
    #[test_case(RESEARCH_DESK, research_desk(); "research_desk_example")]
    fn the_plan_examples_parse(source: &str, expected: AutomationMeta) {
        assert_eq!(parse_meta(source), Ok(expected));
    }

    #[test]
    fn omitted_options_take_their_defaults() {
        assert_eq!(
            parse_meta(BARE_TRIGGERS),
            Ok(AutomationMeta {
                triggers: vec![
                    Trigger::Armed,
                    Trigger::Idle {
                        delay: Duration::ZERO,
                    },
                    Trigger::NeedsInput {
                        delay: Duration::ZERO,
                        inputs: default_inputs(),
                    },
                    Trigger::GoalFinished {
                        verdicts: default_verdicts(),
                    },
                    Trigger::MessageReceived(default_filter()),
                    Trigger::WorkFinished {
                        groups: Vec::new(),
                        states: vec![
                            WorkState::Completed,
                            WorkState::Failed,
                            WorkState::Cancelled,
                        ],
                    },
                    Trigger::WorkflowFinished {
                        workflows: Vec::new(),
                        statuses: default_statuses(),
                    },
                    Trigger::Schedule(Schedule {
                        cadence: Cadence::At {
                            hour: 2,
                            minute: 30,
                            weekdays: Vec::new(),
                        },
                        catch_up: CatchUp::Once,
                    }),
                ],
                ..header(PROBE, PROBE_DESCRIPTION)
            })
        );
    }

    #[test]
    fn given_options_replace_the_defaults() {
        assert_eq!(
            parse_meta(EVERY_OPTION),
            Ok(AutomationMeta {
                triggers: vec![
                    Trigger::Idle {
                        delay: Duration::from_secs(90),
                    },
                    Trigger::NeedsInput {
                        delay: Duration::from_mins(5),
                        inputs: vec![InputKind::Messages, InputKind::Plan],
                    },
                    Trigger::GoalFinished {
                        verdicts: vec![GoalVerdict::Impossible],
                    },
                    Trigger::MessageReceived(MessageFilter {
                        audiences: vec![Audience::Topic],
                        topics: strings(&["swarm.*"]),
                        senders: strings(&["@worker-*", NAME_WILDCARD]),
                        scripts: Vec::new(),
                        admissions: vec![Admission::Held],
                        from_automations: true,
                        consume: false,
                    }),
                    Trigger::WorkFinished {
                        groups: Vec::new(),
                        states: vec![WorkState::Paused],
                    },
                    Trigger::WorkflowFinished {
                        workflows: Vec::new(),
                        statuses: vec![WorkflowStatus::Failed, WorkflowStatus::Interrupted],
                    },
                    Trigger::Schedule(Schedule {
                        cadence: Cadence::Every(Duration::from_hours(1)),
                        catch_up: CatchUp::Skip,
                    }),
                ],
                limits: AutomationLimits {
                    cooldown: Duration::from_mins(30),
                    max_per_hour: MAX_PER_HOUR,
                },
                messaging: MessagingCaps {
                    publish: strings(&["swarm.status", BROADCAST]),
                    ..MessagingCaps::default()
                },
                ..header(PROBE, PROBE_DESCRIPTION)
            })
        );
    }

    #[test_case(&named(&"a".repeat(MAX_NAME_BYTES), PROBE_DESCRIPTION); "the_longest_name")]
    #[test_case(&named(PROBE, &"d".repeat(MAX_DESCRIPTION_BYTES)); "the_longest_description")]
    #[test_case(&with(r#"limits: #{ cooldown: "24h", max_per_hour: 1 }"#); "the_longest_cooldown_and_lowest_rate")]
    #[test_case(&triggered(r#"#{ kind: "idle", after: "24h" }"#); "the_longest_delay")]
    #[test_case(&triggered(r#"#{ kind: "schedule", every: "1m" }"#); "the_shortest_period")]
    #[test_case(&triggered(r#"#{ kind: "schedule", at: "23:59" }"#); "the_last_minute_of_the_day")]
    #[test_case(&with(r#"arm: "always", args: #{ file: #{ type: "string", default_value: "TODO.md" } }"#); "always_with_every_arg_defaulted")]
    fn limits_are_inclusive(source: &str) {
        parse_meta(source).expect(VALID_HEADER);
    }

    #[test_case(r#"#{ kind: "idle", after: "soon" }"#, "meta.triggers[1].after", EXPECTED_DURATION; "an_unreadable_delay")]
    #[test_case(r#"#{ kind: "needs_input", after: "25h" }"#, "meta.triggers[1].after", WAIT_TOO_LONG; "a_delay_past_a_day")]
    #[test_case(r#"#{ kind: "needs_input", inputs: [] }"#, "meta.triggers[1].inputs", EMPTY_LIST; "no_inputs")]
    #[test_case(r#"#{ kind: "needs_input", inputs: ["plan", "plan"] }"#, "meta.triggers[1].inputs[1]", DUPLICATE_ENTRY; "a_repeated_input")]
    #[test_case(r#"#{ kind: "goal_finished", verdicts: [] }"#, "meta.triggers[1].verdicts", EMPTY_LIST; "no_verdicts")]
    #[test_case(r#"#{ kind: "message_received", audiences: [] }"#, "meta.triggers[1].audiences", EMPTY_LIST; "no_audiences")]
    #[test_case(r#"#{ kind: "message_received", consume: true, admissions: ["queued", "held"] }"#, "meta.triggers[1].consume", CONSUME_NEEDS_QUEUED; "consuming_held_messages_too")]
    #[test_case(r#"#{ kind: "message_received", consume: true, admissions: ["held"] }"#, "meta.triggers[1].consume", CONSUME_NEEDS_QUEUED; "consuming_only_held_messages")]
    #[test_case(r#"#{ kind: "message_received", senders: ["ci-watcher"] }"#, "meta.triggers[1].senders[0]", INVALID_SENDER; "a_sender_without_at")]
    #[test_case(r#"#{ kind: "message_received", senders: ["@worker*"] }"#, "meta.triggers[1].senders[0]", INVALID_SENDER; "a_wildcard_without_its_hyphen")]
    #[test_case(r#"#{ kind: "message_received", senders: ["@CI"] }"#, "meta.triggers[1].senders[0]", INVALID_SENDER; "an_uppercase_sender")]
    #[test_case(r#"#{ kind: "message_received", scripts: [""] }"#, "meta.triggers[1].scripts[0]", INVALID_LABEL; "an_empty_label")]
    #[test_case(r#"#{ kind: "message_received", scripts: ["nightly\u200bci"] }"#, "meta.triggers[1].scripts[0]", INVALID_LABEL; "an_invisible_character_in_a_label")]
    #[test_case(r#"#{ kind: "message_received", topics: ["ci.failures", "ci.failures"] }"#, "meta.triggers[1].topics[1]", DUPLICATE_ENTRY; "a_repeated_topic")]
    #[test_case(r#"#{ kind: "work_finished", groups: ["Swarm"] }"#, "meta.triggers[1].groups[0]", INVALID_GROUP; "an_invalid_group")]
    #[test_case(r#"#{ kind: "work_finished", states: [] }"#, "meta.triggers[1].states", EMPTY_LIST; "no_states")]
    #[test_case(r#"#{ kind: "workflow_finished", workflows: ["deep_research"] }"#, "meta.triggers[1].workflows[0]", INVALID_WORKFLOW; "an_invalid_workflow")]
    #[test_case(r#"#{ kind: "workflow_finished", statuses: [] }"#, "meta.triggers[1].statuses", EMPTY_LIST; "no_statuses")]
    #[test_case(r#"#{ kind: "schedule" }"#, "meta.triggers[1]", CADENCE_REQUIRED; "no_cadence")]
    #[test_case(r#"#{ kind: "schedule", every: "1h", at: "09:00" }"#, "meta.triggers[1]", CADENCE_REQUIRED; "both_cadences")]
    #[test_case(r#"#{ kind: "schedule", every: "often" }"#, "meta.triggers[1].every", EXPECTED_DURATION; "an_unreadable_period")]
    #[test_case(r#"#{ kind: "schedule", every: "30s" }"#, "meta.triggers[1].every", EVERY_TOO_SHORT; "a_period_under_a_minute")]
    #[test_case(r#"#{ kind: "schedule", at: "9:00" }"#, "meta.triggers[1].at", EXPECTED_CLOCK; "a_one_digit_hour")]
    #[test_case(r#"#{ kind: "schedule", at: "+9:00" }"#, "meta.triggers[1].at", EXPECTED_CLOCK; "a_signed_hour")]
    #[test_case(r#"#{ kind: "schedule", at: "24:00" }"#, "meta.triggers[1].at", EXPECTED_CLOCK; "hour_24")]
    #[test_case(r#"#{ kind: "schedule", at: "09:60" }"#, "meta.triggers[1].at", EXPECTED_CLOCK; "minute_60")]
    #[test_case(r#"#{ kind: "schedule", every: "1h", weekdays: ["mon"] }"#, "meta.triggers[1].weekdays", WEEKDAYS_NEED_AT; "weekdays_with_every")]
    #[test_case(r#"#{ kind: "schedule", at: "09:00", weekdays: ["mon", "mon"] }"#, "meta.triggers[1].weekdays[1]", DUPLICATE_ENTRY; "a_repeated_weekday")]
    fn trigger_options_are_validated(trigger: &str, path: &str, reason: &str) {
        assert_eq!(rejection(&triggered(trigger)), at_path(path, reason));
    }

    #[test_case(r#"#{ kind: "armed", after: "2m" }"#, "meta.triggers[1]: unknown field `after`"; "an_option_of_another_kind")]
    #[test_case(r#"#{ kind: "idle", "for": "2m" }"#, "meta.triggers[1]: unknown field `for`"; "the_keyword_the_plan_renamed")]
    #[test_case(r#"#{ kind: "sometimes" }"#, "meta.triggers[1]: unknown variant `sometimes`"; "an_unknown_kind")]
    #[test_case(r#"#{ after: "2m" }"#, "meta.triggers[1]: missing field `kind`"; "no_kind")]
    #[test_case(r#"#{ kind: "needs_input", inputs: ["typing"] }"#, "meta.triggers[1]: unknown variant `typing`"; "an_unknown_input")]
    #[test_case(r#"#{ kind: "idle", after: 120 }"#, "meta.triggers[1]: invalid type: integer `120`"; "a_delay_without_a_unit")]
    #[test_case(r#"#{ kind: "schedule", at: "09:00", weekdays: ["someday"] }"#, "meta.triggers[1]: unknown variant `someday`"; "an_unknown_weekday")]
    fn trigger_shapes_are_checked(trigger: &str, prefix: &str) {
        let rejection = rejection(&triggered(trigger));
        assert!(rejection.starts_with(prefix), "{rejection}");
    }

    #[test_case(&named("Keep-Going", PROBE_DESCRIPTION), "meta.name", INVALID_NAME; "an_uppercase_name")]
    #[test_case(&named("keep--going", PROBE_DESCRIPTION), "meta.name", INVALID_NAME; "a_double_hyphen")]
    #[test_case(&named(&"a".repeat(MAX_NAME_BYTES + 1), PROBE_DESCRIPTION), "meta.name", INVALID_NAME; "a_name_past_the_limit")]
    #[test_case(&named(PROBE, "  "), "meta.description", BLANK_DESCRIPTION; "a_blank_description")]
    #[test_case(&named(PROBE, &"d".repeat(MAX_DESCRIPTION_BYTES + 1)), "meta.description", DESCRIPTION_TOO_LONG; "a_description_past_the_limit")]
    #[test_case(&script(r#"name: "probe", description: "Probe", triggers: []"#), "meta.triggers", TRIGGER_COUNT; "no_triggers")]
    #[test_case(&script(&format!(r#"name: "probe", description: "Probe", triggers: [{}]"#, repeated(ARMED, MAX_TRIGGERS + 1))), "meta.triggers", TRIGGER_COUNT; "a_trigger_past_the_limit")]
    #[test_case(&with(r#"limits: #{ cooldown: "a while" }"#), "meta.limits.cooldown", EXPECTED_DURATION; "an_unreadable_cooldown")]
    #[test_case(&with(r#"limits: #{ cooldown: "25h" }"#), "meta.limits.cooldown", WAIT_TOO_LONG; "a_cooldown_past_a_day")]
    #[test_case(&with("limits: #{ max_per_hour: 0 }"), "meta.limits.max_per_hour", PER_HOUR_RANGE; "a_zero_rate")]
    #[test_case(&with("limits: #{ max_per_hour: 601 }"), "meta.limits.max_per_hour", PER_HOUR_RANGE; "a_rate_past_the_cap")]
    #[test_case(&with("limits: #{ max_per_hour: -1 }"), "meta.limits.max_per_hour", PER_HOUR_RANGE; "a_negative_rate")]
    #[test_case(&with(r#"secrets: ["github_token"]"#), "meta.secrets[0]", INVALID_SECRET; "a_lowercase_secret")]
    #[test_case(&with(r#"secrets: ["1PASSWORD"]"#), "meta.secrets[0]", INVALID_SECRET; "a_secret_starting_with_a_digit")]
    #[test_case(&with(&format!(r#"secrets: ["{}"]"#, "S".repeat(MAX_SECRET_BYTES + 1))), "meta.secrets[0]", INVALID_SECRET; "a_secret_past_the_limit")]
    #[test_case(&with(r#"secrets: ["TOKEN", "TOKEN"]"#), "meta.secrets[1]", DUPLICATE_ENTRY; "a_repeated_secret")]
    #[test_case(&with(&format!("secrets: [{}]", repeated(r#""TOKEN""#, MAX_ENTRIES + 1))), "meta.secrets", TOO_MANY_ENTRIES; "a_secret_past_the_count")]
    #[test_case(&with(r#"messaging: #{ send: ["worker-1"] }"#), "meta.messaging.send[0]", INVALID_SENDER; "a_target_without_at")]
    #[test_case(&with(r#"messaging: #{ publish: ["ci.failures", "ci.failures"] }"#), "meta.messaging.publish[1]", DUPLICATE_ENTRY; "a_repeated_publication_topic")]
    #[test_case(&with(r#"workflows: ["review_changes"]"#), "meta.workflows[0]", INVALID_WORKFLOW; "an_invalid_workflow")]
    #[test_case(&with(r#"workflows: ["review", "review"]"#), "meta.workflows[1]", DUPLICATE_ENTRY; "a_repeated_workflow")]
    #[test_case(&with(r#"timezone: "Mars/Olympus_Mons""#), "meta.timezone", UNKNOWN_TIMEZONE; "an_unknown_zone")]
    #[test_case(&with(r#"arm: "always", args: #{ goals: #{ type: "list" } }"#), "meta.args.goals", ALWAYS_NEEDS_DEFAULT; "always_with_a_required_arg")]
    fn header_fields_are_validated(source: &str, path: &str, reason: &str) {
        assert_eq!(rejection(source), at_path(path, reason));
    }

    #[test_case(&with("cron: true"), "meta: unknown field `cron`"; "an_unknown_key")]
    #[test_case(&script(&format!(r#"name: "probe", triggers: [{ARMED}]"#)), "meta: missing field `description`"; "no_description")]
    #[test_case(&with(r#"arm: "sometimes""#), "meta: unknown variant `sometimes`"; "an_unknown_arm_mode")]
    #[test_case(&with("limits: #{ burst: 3 }"), "meta.limits: unknown field `burst`"; "an_unknown_limit")]
    #[test_case(&with(r#"messaging: #{ reply: "yes" }"#), "meta.messaging: invalid type: string \"yes\""; "a_reply_that_is_not_a_bool")]
    #[test_case(&with(r#"args: #{ goals: #{ type: "list", "default": [] } }"#), "meta.args.goals: unknown field `default`"; "the_arg_keyword_the_plan_renamed")]
    fn header_shapes_are_checked(source: &str, prefix: &str) {
        let rejection = rejection(source);
        assert!(rejection.starts_with(prefix), "{rejection}");
    }

    #[test_case("https://api.github.com", "https://api.github.com"; "an_https_origin")]
    #[test_case("HTTPS://API.GitHub.com:443/", "https://api.github.com"; "case_and_the_default_port")]
    #[test_case("https://hooks.example.com:8443", "https://hooks.example.com:8443"; "another_port")]
    #[test_case("https://bücher.example", "https://xn--bcher-kva.example"; "an_international_domain")]
    #[test_case("http://localhost:3000", "http://localhost:3000"; "http_on_localhost")]
    #[test_case("http://127.0.0.1:8080", "http://127.0.0.1:8080"; "http_on_loopback")]
    #[test_case("http://192.168.1.20", "http://192.168.1.20"; "http_on_a_private_address")]
    #[test_case("http://[::1]:8080", "http://[::1]:8080"; "http_on_ipv6_loopback")]
    #[test_case("http://[fd00::1]", "http://[fd00::1]"; "http_on_a_unique_local_address")]
    fn network_origins_are_normalised(origin: &str, normalised: &str) {
        let meta = parse_meta(&with(&format!(r#"network: ["{origin}"]"#))).expect(VALID_HEADER);
        assert_eq!(meta.network, [normalised]);
    }

    #[test_case("http://example.com", INSECURE_ORIGIN; "http_on_a_public_host")]
    #[test_case("http://ci.internal", INSECURE_ORIGIN; "http_on_a_name_that_may_resolve_anywhere")]
    #[test_case("http://169.254.169.254", INSECURE_ORIGIN; "http_on_link_local")]
    #[test_case("ftp://example.com", INVALID_ORIGIN; "another_scheme")]
    #[test_case("https://example.com/hooks", INVALID_ORIGIN; "a_path")]
    #[test_case("https://example.com?token=1", INVALID_ORIGIN; "a_query")]
    #[test_case("https://user:secret@example.com", INVALID_ORIGIN; "credentials")]
    #[test_case("example.com", INVALID_ORIGIN; "no_scheme")]
    fn network_origins_are_checked(origin: &str, reason: &str) {
        assert_eq!(
            rejection(&with(&format!(r#"network: ["{origin}"]"#))),
            at_path("meta.network[0]", reason)
        );
    }

    #[test]
    fn origins_repeat_once_normalised() {
        assert_eq!(
            rejection(&with(
                r#"network: ["https://example.com", "https://EXAMPLE.com:443"]"#
            )),
            at_path("meta.network[1]", DUPLICATE_ENTRY)
        );
    }

    #[test]
    fn a_source_past_the_limit_is_refused_unread() {
        let mut source = with("");
        source.push_str(&" ".repeat(MAX_SOURCE_BYTES));
        let refusal = MetaError::SourceTooLarge {
            bytes: source.len(),
        };
        assert_eq!(parse_meta(&source), Err(refusal.clone()));
        assert_eq!(references(&source), Err(refusal));
    }

    #[test_case("let other = 1;\nlet meta = #{};"; "a_later_header")]
    #[test_case("const meta = #{};"; "a_constant_header")]
    fn the_header_comes_first(source: &str) {
        assert_eq!(parse_meta(source), Err(MetaError::NotFirst));
    }

    #[test]
    fn the_header_holds_only_literals() {
        assert!(matches!(
            parse_meta(&with("timezone: zone")),
            Err(MetaError::Header(HeaderError::NonLiteral { .. }))
        ));
    }

    #[test]
    fn references_find_every_named_arg_read() {
        let expected: BTreeSet<String> = [
            "chained",
            "flag",
            "goals",
            "in_closure",
            "in_function",
            "quoted",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert_eq!(references(REFERENCING).expect(VALID_SOURCE).args, expected);
    }

    #[test_case("skip" => true; "a_statement_call")]
    #[test_case("find" => true; "a_method_call")]
    #[test_case("len" => true; "a_method_call_on_an_indexed_arg")]
    #[test_case("notify" => true; "a_call_inside_a_function")]
    #[test_case("+" => false; "an_operator")]
    #[test_case("==" => false; "a_comparison")]
    fn references_find_calls(name: &str) -> bool {
        references(REFERENCING)
            .expect(VALID_SOURCE)
            .calls
            .contains(name)
    }

    #[test]
    fn references_report_parse_errors() {
        assert!(matches!(
            references("let meta = #{"),
            Err(MetaError::Header(HeaderError::Parse(_)))
        ));
    }
}
