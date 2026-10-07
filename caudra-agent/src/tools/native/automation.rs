//! `automation`: what the model may read of the session's automation runtime. It lists the
//! scripts the session can arm, validates one, and reads this session's firings back. Trusting,
//! arming and choosing args stay with the user, so nothing here changes the runtime.

use std::borrow::Cow;
use std::fmt::Write;
use std::path::Path;

use caudra_automation::args::ArgDecl;
use caudra_automation::request::{AutomationError, AutomationRequest, AutomationResponse};
use caudra_automation::snapshot::{
    ActionRow, AutomationSnapshot, AutomationStatus, Availability, ErrorView, FiringDetail,
    FiringSummary, WaitReason,
};
use jiff::Timestamp;
use serde_json::Value;

use crate::automation::catalog::AutomationDirs;
use crate::automation::handle::AutomationHandle;
use crate::automation::manager::spelled;
use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolExecResult, ToolFailure,
    ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{AUTOMATION_TOOL_NAME, DescriptionContext, ToolAudience, ToolContext};
use crate::types::ToolOutput;

pub const DESCRIPTION: &str = "Read this session's automations: short Rhai scripts that react to events in one session, such as the session going idle, a goal finishing, or a schedule coming due. Load the `caudra-automation-dev` skill before writing or debugging one.

Actions:
- `list`: every automation this session can arm, with its scope, availability, status, triggers, args and warnings, files that cannot load with the reason, and the two directories scripts go in, as resolved for this machine and build.
- `validate`: parse the header of `name`, compile the script, check the args its body reads, and run the body once per trigger against a canned event, performing nothing.
- `history`: this session's newest firings with their status, only one automation's when you pass `name`, at most `limit` (20 by default, at most 50). Pass `fire_id` to see one firing's event and its actions, log lines included, with its error and state patch.

Input: { action: \"list\" | \"validate\" | \"history\", name?: string, fire_id?: string, limit?: integer }";

pub const UNAVAILABLE: &str = "this session has no automation runtime: automations need an \
     interactive session with experimental.automations on";

const ACTION_FIELD: &str = "action";
const NAME_FIELD: &str = "name";
const FIRE_ID_FIELD: &str = "fire_id";
const LIMIT_FIELD: &str = "limit";
const ACTION_LIST: &str = "list";
const ACTION_VALIDATE: &str = "validate";
const ACTION_HISTORY: &str = "history";
const ACTIONS: &[&str] = &[ACTION_LIST, ACTION_VALIDATE, ACTION_HISTORY];
const NAME_REQUIRED: &str = "name is required for validate";
const BLANK: &str = "must not be blank";
const UNKNOWN_ACTION: &str = "unknown action";
const UNEXPECTED_RESPONSE: &str =
    "the automation runtime answered with something this tool does not read";

const VALID: &str = "valid";
const INVALID: &str = "invalid";
const ARMED: &str = "armed";
const AVAILABLE: &str = "available";
const NEEDS_TRUST: &str = "needs trust";
const DIGEST_LABEL: &str = "digest ";
const REQUIRED: &str = "required";
const DEFAULT_LABEL: &str = "default ";
const NO_AUTOMATIONS: &str = "No automations are available.";
const TRIGGERS_LABEL: &str = "triggers: ";
const ARG_LABEL: &str = "arg ";
const WARNING_LABEL: &str = "warning: ";
const HIDES_LABEL: &str = "hides the script of the same name in: ";
const PROJECT_DIR_LABEL: &str =
    "Project scripts (the user must trust each digest in /automations): ";
const USER_DIR_LABEL: &str = "User scripts (trusted as written): ";
const NO_PROJECT_DIR: &str = "Project scripts: none, this session has no local project directory.";
const NO_USER_DIR: &str = "User scripts: none, no user config directory resolves.";
const HAND_OVER: &str = "Trusting a project script, arming an automation and choosing its args \
     belong to the user: give them `/automations arm NAME {json}`, or `--automation NAME='{json}'` \
     to arm it at launch.";

const NO_FIRINGS: &str = "No firings in this session yet.";
const DETAIL_HINT: &str = "Pass fire_id for one firing's event, actions and state patch.";
const REPEATS_MARK: &str = "\u{d7}";
const CONSUMED: &str = "consumed";
const RESULT_ARROW: &str = "\u{2192} ";
const REASON_LABEL: &str = "reason: ";
const ERROR_LABEL: &str = "error: ";
const SOURCE_LABEL: &str = "source: ";
const QUEUED_LABEL: &str = "queued ";
const DEFERRED_LABEL: &str = "deferred until ";
const STARTED_LABEL: &str = "started ";
const FINISHED_LABEL: &str = "finished ";
const OPERATIONS_LABEL: &str = "operations: ";
const STATE_LABEL: &str = "state: ";
const EVENT_LABEL: &str = "event: ";
const ACTIONS_LABEL: &str = "actions:";
const NO_ACTIONS: &str = "actions: none";
const PATCH_LABEL: &str = "state patch: ";
const WAITS_LABEL: &str = "waits: ";
const TURN_LABEL: &str = "turn: ";
const STORED_PREVIEW: &str = " (stored as a preview)";
const LINE_PREFIX: &str = "L";
const INDENT: &str = "  ";
const FIELD_SEPARATOR: &str = " \u{b7} ";
const LIST_SEPARATOR: &str = ", ";
const TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";

/// The whole answer, so a firing with a large event or many actions cannot flood the context.
const MAX_ANSWER_BYTES: usize = 32 * 1024;
/// Bytes of a firing's event JSON kept in its trace.
const MAX_EVENT_BYTES: usize = 8 * 1024;
/// Bytes of a firing's state patch JSON kept in its trace.
const MAX_PATCH_BYTES: usize = 8 * 1024;
const TRUNCATED_JSON: &str = "…[truncated]";
const TRUNCATED_ANSWER: &str = "\n…[truncated: the answer reached its 32 KiB bound]";

static ACTION_PARAM: ParamSchema = ParamSchema::Enum {
    variants: ACTIONS,
    description: "What to do.",
};
static NAME_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Automation name: required for validate, and narrows history to one automation.",
};
static FIRE_ID_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "A firing of this session, for history: its event, actions and state patch.",
};
static LIMIT_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Integer,
    description: "Most firings a history answer lists: 20 by default, at most 50.",
};
static PROPERTIES: &[Property] = &[
    (ACTION_FIELD, &ACTION_PARAM, true, &[]),
    (NAME_FIELD, &NAME_PARAM, false, &[]),
    (FIRE_ID_FIELD, &FIRE_ID_PARAM, false, &[]),
    (LIMIT_FIELD, &LIMIT_PARAM, false, &[]),
];
static SCHEMA: ParamSchema = ParamSchema::Object {
    properties: PROPERTIES,
    description: "",
    reject_unknown: true,
};

pub struct AutomationTool;

impl Tool for AutomationTool {
    fn name(&self) -> &str {
        AUTOMATION_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(DESCRIPTION)
    }

    fn schema(&self) -> Value {
        to_json_schema(&SCHEMA)
    }

    fn audience(&self) -> ToolAudience {
        ToolAudience::MAIN
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        Ok(Box::new(AutomationCall {
            request: parse_request(input)?,
        }))
    }
}

struct AutomationCall {
    request: AutomationRequest,
}

impl ToolInvocation for AutomationCall {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(header(&self.request)))
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            let Some(handle) = ctx
                .session_id
                .as_ref()
                .and_then(|session| AutomationHandle::lookup(session.id()))
            else {
                return error(ToolFailure::Other, UNAVAILABLE.to_owned());
            };
            render(&handle, self.request).await
        })
    }
}

/// A blank `name` or `fire_id` is refused rather than sent: the runtime would answer it with an
/// empty history or an unknown firing, which says less than the refusal.
fn parse_request(input: &Value) -> Result<AutomationRequest, ParseError> {
    let input = validate(&SCHEMA, input.clone())?;
    let text = |field: &str| match input.get(field).and_then(Value::as_str) {
        Some(value) if value.trim().is_empty() => {
            Err(ParseError::custom(format!("{field} {BLANK}")))
        }
        value => Ok(value.map(str::to_owned)),
    };
    let action = input
        .get(ACTION_FIELD)
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(match action {
        ACTION_LIST => AutomationRequest::List,
        ACTION_VALIDATE => AutomationRequest::Validate {
            name: text(NAME_FIELD)?.ok_or_else(|| ParseError::custom(NAME_REQUIRED))?,
        },
        ACTION_HISTORY => AutomationRequest::History {
            name: text(NAME_FIELD)?,
            fire_id: text(FIRE_ID_FIELD)?,
            limit: input
                .get(LIMIT_FIELD)
                .and_then(Value::as_u64)
                .map(|limit| usize::try_from(limit).unwrap_or(usize::MAX)),
        },
        other => return Err(ParseError::custom(format!("{UNKNOWN_ACTION}: {other}"))),
    })
}

fn header(request: &AutomationRequest) -> String {
    match request {
        AutomationRequest::Validate { name } => format!("{ACTION_VALIDATE} {name}"),
        AutomationRequest::History {
            fire_id: Some(subject),
            ..
        }
        | AutomationRequest::History {
            name: Some(subject),
            ..
        } => format!("{ACTION_HISTORY} {subject}"),
        AutomationRequest::History { .. } => ACTION_HISTORY.to_owned(),
        _ => ACTION_LIST.to_owned(),
    }
}

async fn render(handle: &AutomationHandle, request: AutomationRequest) -> ToolExecResult {
    match handle.request(request).await {
        Ok(AutomationResponse::Automations(automations)) => {
            plain(render_list(&automations, handle.directories()))
        }
        Ok(AutomationResponse::Validation { name, ok, report }) => {
            let verdict = if ok { VALID } else { INVALID };
            plain(format!("{name}: {verdict}. {report}"))
        }
        Ok(AutomationResponse::Firings(firings)) => plain(render_firings(&firings)),
        Ok(AutomationResponse::Firing(detail)) => plain(render_firing(&detail)),
        Ok(_) => error(ToolFailure::Other, UNEXPECTED_RESPONSE.to_owned()),
        Err(failure) => error(failure_of(&failure), failure.to_string()),
    }
}

fn failure_of(error: &AutomationError) -> ToolFailure {
    match error {
        AutomationError::UnknownAutomation { .. } | AutomationError::UnknownFiring { .. } => {
            ToolFailure::NotFound
        }
        AutomationError::TrustRequired { .. } => ToolFailure::Denied,
        AutomationError::Invalid { .. } | AutomationError::Args { .. } => ToolFailure::InvalidInput,
        AutomationError::Unavailable
        | AutomationError::StateConflict { .. }
        | AutomationError::NotReplayable { .. }
        | AutomationError::NotWaiting { .. }
        | AutomationError::SessionNotSaved { .. }
        | AutomationError::Storage(_)
        | AutomationError::Internal(_) => ToolFailure::Other,
    }
}

/// One block per automation, then where scripts go and who arms them: a writer needs the
/// directories even before the first script exists.
fn render_list(automations: &[AutomationSnapshot], directories: &AutomationDirs) -> String {
    let mut lines = Vec::new();
    if automations.is_empty() {
        lines.push(NO_AUTOMATIONS.to_owned());
    }
    for automation in automations {
        automation_lines(automation, &mut lines);
    }
    lines.push(directory_line(
        directories.project.as_deref(),
        PROJECT_DIR_LABEL,
        NO_PROJECT_DIR,
    ));
    lines.push(directory_line(
        directories.user.as_deref(),
        USER_DIR_LABEL,
        NO_USER_DIR,
    ));
    lines.push(HAND_OVER.to_owned());
    lines.join("\n")
}

fn automation_lines(automation: &AutomationSnapshot, lines: &mut Vec<String>) {
    let mut head = format!(
        "- {} [{}] {}",
        automation.name,
        spelled(automation.scope),
        availability(automation)
    );
    if matches!(automation.availability, Availability::Invalid { .. }) {
        let _ = write!(head, " ({})", automation.path.display());
        lines.push(head);
        return;
    }
    let _ = write!(
        head,
        "{FIELD_SEPARATOR}{}: {}",
        status(automation.status),
        automation.description
    );
    lines.push(head);
    if !automation.triggers.is_empty() {
        let kinds: Vec<String> = automation
            .triggers
            .iter()
            .map(|trigger| spelled(trigger.kind))
            .collect();
        lines.push(format!(
            "{INDENT}{TRIGGERS_LABEL}{}",
            kinds.join(LIST_SEPARATOR)
        ));
    }
    lines.extend(
        automation
            .declared_args
            .iter()
            .map(|arg| format!("{INDENT}{ARG_LABEL}{}", arg_text(arg))),
    );
    lines.extend(
        automation
            .warnings
            .iter()
            .map(|warning| format!("{INDENT}{WARNING_LABEL}{warning}")),
    );
    if !automation.shadowed.is_empty() {
        let scopes: Vec<String> = automation.shadowed.iter().map(spelled).collect();
        lines.push(format!(
            "{INDENT}{HIDES_LABEL}{}",
            scopes.join(LIST_SEPARATOR)
        ));
    }
}

fn availability(automation: &AutomationSnapshot) -> String {
    match &automation.availability {
        Availability::Armed => match automation.armed {
            Some(origin) => format!("{ARMED} ({origin})"),
            None => ARMED.to_owned(),
        },
        Availability::Available => AVAILABLE.to_owned(),
        Availability::NeedsTrust => {
            format!("{NEEDS_TRUST} ({DIGEST_LABEL}{})", automation.digest)
        }
        Availability::Invalid { reason } => format!("{INVALID}: {reason}"),
    }
}

fn status(status: AutomationStatus) -> String {
    match status {
        AutomationStatus::Idle => "idle".to_owned(),
        AutomationStatus::Running => "running".to_owned(),
        AutomationStatus::Queued { waiting } => format!("queued ({waiting} waiting)"),
        AutomationStatus::Deferred { until } => format!("deferred until {}", time(until)),
        AutomationStatus::BackingOff { until } => format!("backing off until {}", time(until)),
        AutomationStatus::Paused => "paused".to_owned(),
        AutomationStatus::Failed => "failed".to_owned(),
    }
}

fn arg_text(arg: &ArgDecl) -> String {
    let requirement = match &arg.spec.default {
        Some(default) => format!("{DEFAULT_LABEL}{default}"),
        None => REQUIRED.to_owned(),
    };
    let mut text = format!("{} ({}, {requirement})", arg.name, spelled(arg.spec.kind));
    if let Some(description) = &arg.spec.description {
        let _ = write!(text, ": {description}");
    }
    text
}

fn directory_line(dir: Option<&Path>, label: &str, missing: &str) -> String {
    dir.map_or_else(
        || missing.to_owned(),
        |dir| format!("{label}{}", dir.display()),
    )
}

/// A row per firing, newest first, ending in what a reader looks at first: the error, the
/// reason, or the first thing it did.
fn render_firings(firings: &[FiringSummary]) -> String {
    if firings.is_empty() {
        return NO_FIRINGS.to_owned();
    }
    let mut lines: Vec<String> = firings.iter().map(firing_line).collect();
    lines.push(DETAIL_HINT.to_owned());
    lines.join("\n")
}

fn firing_line(firing: &FiringSummary) -> String {
    let mut line = format!("{} {}", time(firing.queued_at), firing_head(firing));
    if let Some(error) = &firing.error {
        let _ = write!(line, "{FIELD_SEPARATOR}{}", error_text(error));
    } else if let Some(reason) = &firing.reason {
        let _ = write!(line, "{FIELD_SEPARATOR}{reason}");
    } else if let Some(action) = firing.first_action {
        let _ = write!(line, "{FIELD_SEPARATOR}{RESULT_ARROW}{}", action.as_str());
    }
    line
}

/// `fire_id` leads because a trace takes one.
fn firing_head(firing: &FiringSummary) -> String {
    let mut head = format!(
        "{} {} {}[{}] {}",
        firing.fire_id,
        firing.automation,
        spelled(firing.trigger),
        firing.trigger_index,
        firing.status
    );
    if firing.repeats > 1 {
        let _ = write!(head, " {REPEATS_MARK}{}", firing.repeats);
    }
    if firing.consumed {
        let _ = write!(head, " {CONSUMED}");
    }
    head
}

/// The firing, what happened to it and when, its event, a row per action, and its state patch.
/// The event and the patch are capped first, since either alone could fill the answer.
fn render_firing(detail: &FiringDetail) -> String {
    let firing = &detail.firing;
    let mut lines = vec![firing_head(firing)];
    if let Some(reason) = &firing.reason {
        lines.push(format!("{REASON_LABEL}{reason}"));
    }
    if let Some(error) = &firing.error {
        lines.push(format!("{ERROR_LABEL}{}", error_text(error)));
    }
    if let Some(source) = &detail.error_source {
        lines.push(format!("{INDENT}{SOURCE_LABEL}{}", source.trim()));
    }
    let timings: Vec<String> = [
        Some((QUEUED_LABEL, firing.queued_at)),
        firing.deferred_until.map(|at| (DEFERRED_LABEL, at)),
        firing.started_at.map(|at| (STARTED_LABEL, at)),
        firing.finished_at.map(|at| (FINISHED_LABEL, at)),
    ]
    .into_iter()
    .flatten()
    .map(|(label, at)| format!("{label}{}", time(at)))
    .collect();
    lines.push(timings.join(FIELD_SEPARATOR));
    let mut effort = format!("{OPERATIONS_LABEL}{}", firing.operations);
    if let Some(outcome) = firing.state_outcome {
        let _ = write!(effort, "{FIELD_SEPARATOR}{STATE_LABEL}{outcome}");
    }
    lines.push(effort);
    lines.push(format!(
        "{EVENT_LABEL}{}",
        capped_json(&detail.event, detail.event_cut, MAX_EVENT_BYTES)
    ));
    if detail.actions.is_empty() {
        lines.push(NO_ACTIONS.to_owned());
    } else {
        lines.push(ACTIONS_LABEL.to_owned());
        lines.extend(detail.actions.iter().map(action_line));
    }
    if let Some(patch) = &detail.state_patch {
        lines.push(format!(
            "{PATCH_LABEL}{}",
            capped_json(patch, detail.patch_cut, MAX_PATCH_BYTES)
        ));
    }
    lines.join("\n")
}

fn action_line(action: &ActionRow) -> String {
    let mut line = format!("{INDENT}#{}", action.seq);
    if let Some(at) = action.line {
        let _ = write!(line, " {LINE_PREFIX}{at}");
    }
    let _ = write!(line, " {} {}", action.kind.as_str(), action.status);
    if !action.summary.is_empty() {
        let _ = write!(line, ": {}", action.summary);
    }
    if let Some(error) = &action.error {
        let _ = write!(line, "{FIELD_SEPARATOR}{ERROR_LABEL}{error}");
    }
    if let Some(wait) = action.wait {
        let _ = write!(line, "{FIELD_SEPARATOR}{WAITS_LABEL}{}", wait_text(wait));
    } else if let Some(outcome) = action.turn_outcome {
        let _ = write!(line, "{FIELD_SEPARATOR}{TURN_LABEL}{}", spelled(outcome));
        if let Some(cost) = action.turn_cost {
            let _ = write!(line, ", ${cost:.4}");
        }
    }
    line
}

fn wait_text(reason: WaitReason) -> String {
    match reason {
        WaitReason::Paused => "automations are paused".to_owned(),
        WaitReason::HumanPromptQueued => "a human prompt goes first".to_owned(),
        WaitReason::ModalOpen => "a dialog is open".to_owned(),
        WaitReason::Busy => "the session is busy".to_owned(),
        WaitReason::PeersFirst => "peer messages go first".to_owned(),
        WaitReason::TurnRateFull { until } => {
            format!("the turn rate is full until {}", time(until))
        }
        WaitReason::UnattendedCap { cap } => {
            format!("the cap of {cap} unattended turns is reached")
        }
        WaitReason::Backoff { until } => format!("backing off until {}", time(until)),
        WaitReason::ExpiresAt { at } => format!("expires at {}", time(at)),
    }
}

/// `kind: message (L3:9)`, at the script position Rhai reported.
fn error_text(error: &ErrorView) -> String {
    let mut text = format!("{}: {}", error.kind, error.message);
    if let Some(line) = error.line {
        let _ = write!(text, " ({LINE_PREFIX}{line}");
        if let Some(column) = error.column {
            let _ = write!(text, ":{column}");
        }
        text.push(')');
    }
    text
}

/// Compact JSON with its `$untrusted` wrappers, cut at a character boundary past `limit`.
fn capped_json(value: &Value, stored_as_preview: bool, limit: usize) -> String {
    let mut text = value.to_string();
    if text.len() > limit {
        text.truncate(text.floor_char_boundary(limit));
        text.push_str(TRUNCATED_JSON);
    }
    if stored_as_preview {
        text.push_str(STORED_PREVIEW);
    }
    text
}

fn time(at: i64) -> String {
    Timestamp::from_millisecond(at).map_or_else(
        |_| at.to_string(),
        |at| at.strftime(TIME_FORMAT).to_string(),
    )
}

fn bounded(mut text: String) -> String {
    if text.len() > MAX_ANSWER_BYTES {
        text.truncate(text.floor_char_boundary(MAX_ANSWER_BYTES - TRUNCATED_ANSWER.len()));
        text.push_str(TRUNCATED_ANSWER);
    }
    text
}

fn plain(text: String) -> ToolExecResult {
    ToolExecResult::from(Ok(ToolOutput::Plain(bounded(text).into())))
}

fn error(failure: ToolFailure, message: String) -> ToolExecResult {
    ToolExecResult::from(Ok(ToolOutput::Plain(message.into()))).with_failure(failure)
}

#[cfg(test)]
mod tests {
    use caudra_automation::args::ArgType;
    use caudra_automation::catalog::{STEM_MISMATCH, Scope};
    use caudra_automation::host::ActionKind;
    use caudra_automation::meta::TriggerKind;
    use caudra_automation::snapshot::{ActionStatus, ArmOrigin, FiringStatus};
    use caudra_storage::id::SessionRef;
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::AgentMode;
    use crate::automation::testing::{AutomationFixture, until};
    use crate::tools::test_support::stub_ctx;

    const GREETER: &str = "greeter";
    const PLANNER: &str = "planner";
    const REVIEWER: &str = "reviewer";
    const BROKEN: &str = "broken";
    const FAILING: &str = "failing";
    const TURNS_ARG: &str = "turns";
    const TURNS_DEFAULT: u32 = 3;
    const TURNS_DESCRIPTION: &str = "Most turns";
    const GOAL_ARG: &str = "goal";
    const GOAL_DESCRIPTION: &str = "What to pursue";
    const ARMED_TRIGGER: &str = r#"triggers: [#{ kind: "armed" }]"#;
    const IDLE_TRIGGER: &str = r#"triggers: [#{ kind: "idle" }]"#;
    const QUIET_BODY: &str = r#"log("quiet");"#;
    const MISNAMED: &str = r#"let meta = #{ name: "elsewhere", description: "Misnamed", triggers: [#{ kind: "idle" }] };"#;
    const LOG_TEXT: &str = "checked";
    const THROWN: &str = "boom";
    /// The header takes line 1, so the body's `log` is on line 2 and its `throw` on line 3.
    const LOG_LINE: u32 = 2;
    const ERROR_LINE: u32 = 3;
    const FIRE_ID: &str = "fire-1";
    const DIGEST: &str = "abc123";
    const QUEUED_AT: i64 = 1_790_000_000_000;
    const LIMIT: usize = 5;
    const SUMMARY_CHARS: usize = 200;
    /// Enough log rows to outgrow the answer on their own.
    const MANY_ACTIONS: u64 = 300;

    const FIRED: &str = "the launch firing must end";
    const BOTH_DIRS: &str = "the fixture resolves both directories";
    const EVENT_CAPPED: &str = "the event must be capped before the answer is";
    const PATCH_CAPPED: &str = "the state patch must be capped before the answer is";
    const ANSWER_BOUNDED: &str = "an answer must stay within 32 KiB and say it was cut";

    async fn run(input: Value, ctx: &ToolContext) -> ToolExecResult {
        AutomationTool.parse(&input).unwrap().execute(ctx).await
    }

    async fn execute(input: Value, ctx: &ToolContext) -> (bool, String) {
        let result = run(input, ctx).await;
        (result.is_error, text_of(result))
    }

    fn text_of(result: ToolExecResult) -> String {
        result
            .output
            .map_or_else(|error| error, |output| output.as_text())
    }

    fn session_ctx(fixture: &AutomationFixture) -> ToolContext {
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.session_id = Some(fixture.session_id().into());
        ctx
    }

    fn failing_body() -> String {
        format!("log(\"{LOG_TEXT}\");\nthrow \"{THROWN}\";")
    }

    async fn first_firing(handle: &AutomationHandle, status: FiringStatus) -> String {
        until(handle, |state| {
            state.recent.iter().any(|firing| firing.status == status)
        })
        .await;
        handle.state().recent.first().expect(FIRED).fire_id.clone()
    }

    #[test_case(json!({"action": "list"}), AutomationRequest::List; "list")]
    #[test_case(json!({"action": "validate", "name": GREETER}), AutomationRequest::Validate { name: GREETER.into() }; "validate")]
    #[test_case(json!({"action": "history"}), AutomationRequest::History { name: None, fire_id: None, limit: None }; "history_of_every_automation")]
    #[test_case(json!({"action": "history", "name": GREETER, "limit": LIMIT}), AutomationRequest::History { name: Some(GREETER.into()), fire_id: None, limit: Some(LIMIT) }; "history_of_one_automation")]
    #[test_case(json!({"action": "history", "fire_id": FIRE_ID}), AutomationRequest::History { name: None, fire_id: Some(FIRE_ID.into()), limit: None }; "one_firing")]
    fn actions_map_to_runtime_requests(input: Value, expected: AutomationRequest) {
        assert_eq!(parse_request(&input).unwrap(), expected);
    }

    #[test_case(json!({"action": "validate"}), NAME_REQUIRED; "validate_needs_a_name")]
    #[test_case(json!({"action": "validate", "name": " "}), BLANK; "validate_refuses_a_blank_name")]
    #[test_case(json!({"action": "history", "name": ""}), BLANK; "history_refuses_a_blank_name")]
    #[test_case(json!({"action": "history", "fire_id": ""}), BLANK; "history_refuses_a_blank_fire_id")]
    fn incomplete_actions_are_refused_at_parse_time(input: Value, expected: &str) {
        let error = parse_request(&input).unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
    }

    #[test_case("arm"; "arm")]
    #[test_case("trust"; "trust")]
    #[test_case("pause"; "pause")]
    fn the_tool_only_reads(action: &str) {
        assert!(parse_request(&json!({"action": action, "name": GREETER})).is_err());
    }

    #[test]
    fn list_shows_availability_args_and_both_directories() {
        smol::block_on(async {
            let fixture = AutomationFixture::default();
            let user = fixture.user_scripts();
            let greeter_fields = format!(
                "{IDLE_TRIGGER}, args: #{{ {TURNS_ARG}: #{{ type: \"int\", default_value: {TURNS_DEFAULT}, description: \"{TURNS_DESCRIPTION}\" }} }}"
            );
            let planner_fields = format!(
                "{IDLE_TRIGGER}, args: #{{ {GOAL_ARG}: #{{ type: \"string\", description: \"{GOAL_DESCRIPTION}\" }} }}"
            );
            fixture.script(&user, GREETER, &greeter_fields, QUIET_BODY);
            fixture.script(&user, PLANNER, &planner_fields, QUIET_BODY);
            fixture.script(
                &fixture.project_scripts(),
                REVIEWER,
                IDLE_TRIGGER,
                QUIET_BODY,
            );
            fixture.write(&user, BROKEN, MISNAMED);
            let runtime = fixture.spawn(&[GREETER]).await;
            let handle = runtime.handle();
            until(&handle, |state| {
                state
                    .find(GREETER)
                    .is_some_and(|automation| automation.availability == Availability::Armed)
            })
            .await;

            let (is_error, text) = execute(json!({"action": "list"}), &session_ctx(&fixture)).await;

            assert!(!is_error, "{text}");
            let user_scope = spelled(Scope::User);
            let digest = handle.state().find(REVIEWER).unwrap().digest.clone();
            for expected in [
                format!("- {GREETER} [{user_scope}] {ARMED} ({})", ArmOrigin::Cli),
                format!(
                    "{ARG_LABEL}{TURNS_ARG} ({}, {DEFAULT_LABEL}{TURNS_DEFAULT}): {TURNS_DESCRIPTION}",
                    spelled(ArgType::Int)
                ),
                format!("- {PLANNER} [{user_scope}] {AVAILABLE}"),
                format!(
                    "{ARG_LABEL}{GOAL_ARG} ({}, {REQUIRED}): {GOAL_DESCRIPTION}",
                    spelled(ArgType::String)
                ),
                format!(
                    "- {REVIEWER} [{}] {NEEDS_TRUST} ({DIGEST_LABEL}{digest})",
                    spelled(Scope::Project)
                ),
                format!("- {BROKEN} [{user_scope}] {INVALID}: {STEM_MISMATCH}"),
            ] {
                assert!(text.contains(&expected), "{expected:?} in {text}");
            }
            let directories = handle.directories();
            let project_dir = directories.project.as_ref().expect(BOTH_DIRS);
            let user_dir = directories.user.as_ref().expect(BOTH_DIRS);
            assert!(
                text.contains(&format!("{PROJECT_DIR_LABEL}{}", project_dir.display())),
                "{text}"
            );
            assert!(
                text.contains(&format!("{USER_DIR_LABEL}{}", user_dir.display())),
                "{text}"
            );
            assert!(text.ends_with(HAND_OVER), "{text}");
            runtime.shutdown().await;
        });
    }

    #[test_case(QUIET_BODY.to_owned(), VALID; "valid")]
    #[test_case(failing_body(), INVALID; "invalid")]
    fn validate_reports_whether_the_script_runs(body: String, verdict: &str) {
        smol::block_on(async {
            let fixture = AutomationFixture::default();
            fixture.script(&fixture.user_scripts(), GREETER, ARMED_TRIGGER, &body);
            let runtime = fixture.spawn(&[]).await;

            let input = json!({"action": "validate", "name": GREETER});
            let (is_error, text) = execute(input, &session_ctx(&fixture)).await;

            assert!(!is_error, "{text}");
            assert!(
                text.starts_with(&format!("{GREETER}: {verdict}.")),
                "{text}"
            );
            runtime.shutdown().await;
        });
    }

    #[test]
    fn history_lists_this_sessions_firings_and_traces_one() {
        smol::block_on(async {
            let fixture = AutomationFixture::default();
            fixture.script(
                &fixture.user_scripts(),
                FAILING,
                ARMED_TRIGGER,
                &failing_body(),
            );
            let runtime = fixture.spawn(&[FAILING]).await;
            let fire_id = first_firing(&runtime.handle(), FiringStatus::Failed).await;
            let ctx = session_ctx(&fixture);

            let (is_error, listed) =
                execute(json!({"action": "history", "name": FAILING}), &ctx).await;
            let (trace_is_error, trace) =
                execute(json!({"action": "history", "fire_id": fire_id}), &ctx).await;

            assert!(!is_error, "{listed}");
            let row = format!(
                "{fire_id} {FAILING} {}[0] {}",
                spelled(TriggerKind::Armed),
                FiringStatus::Failed
            );
            let failed = listed
                .lines()
                .find(|line| line.contains(&row))
                .unwrap_or_else(|| panic!("{row:?} in {listed}"));
            assert!(failed.contains(THROWN), "{listed}");
            assert!(
                failed.contains(&format!("({LINE_PREFIX}{ERROR_LINE}:")),
                "{listed}"
            );
            assert!(!trace_is_error, "{trace}");
            let log = format!(
                "{LINE_PREFIX}{LOG_LINE} {} {}: {LOG_TEXT}",
                ActionKind::Log.as_str(),
                ActionStatus::Done
            );
            assert!(trace.contains(&log), "{log:?} in {trace}");
            assert!(
                trace
                    .lines()
                    .any(|line| line.starts_with(ERROR_LABEL) && line.contains(THROWN)),
                "{trace}"
            );
            assert!(
                trace.lines().any(|line| line.starts_with(EVENT_LABEL)),
                "{trace}"
            );
            runtime.shutdown().await;
        });
    }

    #[test]
    fn history_refuses_another_sessions_firing() {
        smol::block_on(async {
            let fixture = AutomationFixture::default();
            fixture.script(&fixture.user_scripts(), GREETER, ARMED_TRIGGER, QUIET_BODY);
            let runtime = fixture.spawn(&[]).await;
            let elsewhere = fixture
                .spawn_session(fixture.other_session(), &[GREETER])
                .await;
            let foreign = first_firing(&elsewhere.handle(), FiringStatus::Completed).await;

            let input = json!({"action": "history", "fire_id": foreign});
            let result = run(input, &session_ctx(&fixture)).await;

            assert!(result.is_error);
            assert_eq!(result.failure, Some(ToolFailure::NotFound));
            assert_eq!(
                text_of(result),
                AutomationError::UnknownFiring { fire_id: foreign }.to_string()
            );
            elsewhere.shutdown().await;
            runtime.shutdown().await;
        });
    }

    fn summary() -> FiringSummary {
        FiringSummary {
            fire_id: FIRE_ID.into(),
            automation: GREETER.into(),
            digest: DIGEST.into(),
            trigger: TriggerKind::Idle,
            trigger_index: 0,
            event_key: None,
            consumed: false,
            status: FiringStatus::Completed,
            reason: None,
            error: None,
            repeats: 1,
            attempts: 0,
            operations: 0,
            state_outcome: None,
            queued_at: QUEUED_AT,
            deferred_until: None,
            started_at: None,
            finished_at: None,
            action_count: 0,
            first_action: None,
        }
    }

    fn log_row(seq: u64) -> ActionRow {
        ActionRow {
            seq,
            kind: ActionKind::Log,
            line: Some(LOG_LINE),
            column: None,
            status: ActionStatus::Done,
            summary: LOG_TEXT.repeat(SUMMARY_CHARS / LOG_TEXT.len()),
            error: None,
            target: None,
            delivery: None,
            expires_at: None,
            wait: None,
            turn_outcome: None,
            turn_cost: None,
            started_at: QUEUED_AT,
            finished_at: None,
            delivered_at: None,
            request_cut: false,
            result_cut: false,
        }
    }

    /// An event and a state patch each larger than the whole answer, with `actions` log rows.
    fn huge_trace(actions: u64) -> String {
        let huge = THROWN.repeat(MAX_ANSWER_BYTES);
        bounded(render_firing(&FiringDetail {
            firing: summary(),
            event: json!({ "text": { "$untrusted": huge } }),
            event_cut: false,
            state_patch: Some(json!({ "notes": huge })),
            patch_cut: false,
            actions: (0..actions).map(log_row).collect(),
            error_source: None,
        }))
    }

    #[test]
    fn a_huge_event_and_patch_are_capped_first() {
        let trace = huge_trace(0);

        let capped = |label: &str, limit: usize, message: &str| {
            let json = trace
                .lines()
                .find_map(|line| line.strip_prefix(label))
                .expect(message);
            assert!(json.ends_with(TRUNCATED_JSON), "{message}");
            assert!(json.len() <= limit + TRUNCATED_JSON.len(), "{message}");
        };
        capped(EVENT_LABEL, MAX_EVENT_BYTES, EVENT_CAPPED);
        capped(PATCH_LABEL, MAX_PATCH_BYTES, PATCH_CAPPED);
        assert!(!trace.ends_with(TRUNCATED_ANSWER), "{EVENT_CAPPED}");
    }

    #[test]
    fn a_trace_never_outgrows_its_bound() {
        let trace = huge_trace(MANY_ACTIONS);

        assert!(trace.len() <= MAX_ANSWER_BYTES, "{ANSWER_BOUNDED}");
        assert!(trace.ends_with(TRUNCATED_ANSWER), "{ANSWER_BOUNDED}");
    }

    #[test_case(None; "no_session")]
    #[test_case(Some(SessionRef::generate()); "no_runtime")]
    fn every_action_is_unavailable_without_a_runtime(session: Option<SessionRef>) {
        smol::block_on(async {
            let mut ctx = stub_ctx(&AgentMode::Build);
            ctx.session_id = session;
            for input in [
                json!({"action": "list"}),
                json!({"action": "validate", "name": GREETER}),
                json!({"action": "history"}),
            ] {
                let result = run(input, &ctx).await;
                assert_eq!(result.failure, Some(ToolFailure::Other));
                assert_eq!(text_of(result), UNAVAILABLE);
            }
        });
    }
}
