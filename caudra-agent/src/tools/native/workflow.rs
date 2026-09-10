//! `workflow`: the model's control surface over the session's workflow
//! runtime. A run outlives the tool call that started it, so every action
//! answers immediately and the run's progress is read back through `status`.

use std::borrow::Cow;

use caudra_workflow::{
    CatalogEntry, InvalidEntry, RunSnapshot, WorkflowCatalog, WorkflowError, WorkflowRequest,
    WorkflowResponse,
};
use serde_json::Value;

use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolExecResult, ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{DescriptionContext, ToolAudience, ToolContext};
use crate::types::ToolOutput;
use crate::workflow::WorkflowHandle;
use caudra_workflow::LaunchRequest;

pub const DESCRIPTION: &str = "Run durable, multi-agent workflows: scripted plans that launch subagents in phases, keep a journal, and can be paused and resumed.

Actions:
- `list`: every workflow this session can launch, with its source, trust state, and description. Invalid scripts are listed with their error.
- `validate`: parse, compile, and smoke-run `name` against a canned host without launching agents.
- `start`: launch `name` with `args` (an object the script reads as `args`; `objective` or `query` becomes the run's objective) and an optional `agent_budget`. Only trusted definitions start: built-in and user scripts always are, a project script must be approved first via `/workflows`. Returns immediately with the run id; the run continues in the background and its agents' results are not returned here.
- `status`: the run named by `run_id`, or every run of the session when omitted. Poll it to see the current phase, the agent roster, usage, and, once the run has ended, its `result` or `error`.
- `pause` / `stop`: end the current attempt of an active run; `pause` keeps it resumable.
- `resume`: continue a paused, failed, cancelled, or budget-limited run from its journal. A budget-limited run needs a higher `agent_budget`.

Input: { action: \"list\" | \"validate\" | \"start\" | \"status\" | \"pause\" | \"resume\" | \"stop\", name?: string, args?: object, agent_budget?: integer, run_id?: string }

A run's completion is reported to you later; you do not need to wait on it. Use `status` when you want to check on progress.";

pub const UNAVAILABLE: &str = "workflow runtime unavailable in this session";
const UNAVAILABLE_NOTE: &str =
    "No workflow runtime is attached to this session, so every action reports it unavailable.";

const ACTION_LIST: &str = "list";
const ACTION_VALIDATE: &str = "validate";
const ACTION_START: &str = "start";
const ACTION_STATUS: &str = "status";
const ACTION_PAUSE: &str = "pause";
const ACTION_RESUME: &str = "resume";
const ACTION_STOP: &str = "stop";
const ACTIONS: &[&str] = &[
    ACTION_LIST,
    ACTION_VALIDATE,
    ACTION_START,
    ACTION_STATUS,
    ACTION_PAUSE,
    ACTION_RESUME,
    ACTION_STOP,
];
const NAME_REQUIRED: &str = "name is required for this action";
const RUN_ID_REQUIRED: &str = "run_id is required for this action";
const ARGS_MUST_BE_OBJECT: &str = "args must be an object";
const TRUST_HINT: &str = "Approve it with /workflows before starting it.";
const TRUSTED: &str = "trusted";
const UNTRUSTED: &str = "untrusted";
const INVALID_HEADING: &str = "Invalid:";
const NO_WORKFLOWS: &str = "No workflows are available.";
const PROJECT_DIR_LABEL: &str =
    "Project scripts (need approval in /workflows before they can start): ";
const USER_DIR_LABEL: &str = "User scripts (trusted as written): ";
const STATUS_HINT: &str = "The run continues in the background. Check on it with";
/// Log lines kept per run in a `status` answer.
const MAX_STATUS_LOGS: usize = 20;
/// Bytes of a run's `result` kept in a `status` answer.
const MAX_STATUS_RESULT_BYTES: usize = 8 * 1024;
const TRUNCATED_RESULT_SUFFIX: &str = "…[truncated]";

static ACTION_PARAM: ParamSchema = ParamSchema::Enum {
    variants: ACTIONS,
    description: "What to do.",
};
static NAME_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Workflow name, for validate and start.",
};
static ARGS_PARAM: ParamSchema = ParamSchema::Any {
    description: "Object the script receives as `args` on start.",
};
static AGENT_BUDGET_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Integer,
    description: "Most agents the run may launch, for start and resume.",
};
static RUN_ID_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Run id, for status, pause, resume, and stop.",
};
static PROPERTIES: &[Property] = &[
    ("action", &ACTION_PARAM, true, &[]),
    ("name", &NAME_PARAM, false, &[]),
    ("args", &ARGS_PARAM, false, &[]),
    ("agent_budget", &AGENT_BUDGET_PARAM, false, &[]),
    ("run_id", &RUN_ID_PARAM, false, &[]),
];
static SCHEMA: ParamSchema = ParamSchema::Object {
    properties: PROPERTIES,
    description: "",
    reject_unknown: true,
};

pub struct WorkflowTool;

impl Tool for WorkflowTool {
    fn name(&self) -> &str {
        crate::tools::WORKFLOW_TOOL_NAME
    }

    fn description(&self, ctx: &DescriptionContext) -> Cow<'_, str> {
        if ctx.workflows_available {
            Cow::Borrowed(DESCRIPTION)
        } else {
            Cow::Owned(format!("{DESCRIPTION}\n\n{UNAVAILABLE_NOTE}"))
        }
    }

    fn schema(&self) -> Value {
        to_json_schema(&SCHEMA)
    }

    fn audience(&self) -> ToolAudience {
        ToolAudience::MAIN
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let input = validate(&SCHEMA, input.clone())?;
        let text = |field: &str| input.get(field).and_then(Value::as_str).map(str::to_owned);
        let name = || text("name").ok_or_else(|| ParseError::custom(NAME_REQUIRED));
        let run_id = || text("run_id").ok_or_else(|| ParseError::custom(RUN_ID_REQUIRED));
        let agent_budget = input
            .get("agent_budget")
            .and_then(Value::as_u64)
            .map(|budget| u32::try_from(budget).unwrap_or(u32::MAX));
        let action = text("action").unwrap_or_default();
        let request = match action.as_str() {
            ACTION_LIST => WorkflowRequest::List,
            ACTION_VALIDATE => WorkflowRequest::Validate { name: name()? },
            ACTION_START => {
                let args = match input.get("args") {
                    None | Some(Value::Null) => Value::Object(serde_json::Map::new()),
                    Some(args @ Value::Object(_)) => args.clone(),
                    Some(_) => return Err(ParseError::custom(ARGS_MUST_BE_OBJECT)),
                };
                WorkflowRequest::Start(LaunchRequest {
                    name: name()?,
                    args,
                    agent_budget,
                })
            }
            ACTION_STATUS => WorkflowRequest::Status {
                run_id: text("run_id"),
            },
            ACTION_PAUSE => WorkflowRequest::Pause { run_id: run_id()? },
            ACTION_RESUME => WorkflowRequest::Resume {
                run_id: run_id()?,
                agent_budget,
            },
            ACTION_STOP => WorkflowRequest::Stop { run_id: run_id()? },
            other => {
                return Err(ParseError::custom(format!("unknown action: {other}")));
            }
        };
        Ok(Box::new(WorkflowCall { action, request }))
    }
}

struct WorkflowCall {
    action: String,
    request: WorkflowRequest,
}

impl ToolInvocation for WorkflowCall {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(header(&self.action, &self.request)))
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            let Some(handle) = ctx.workflow.as_ref() else {
                return error(UNAVAILABLE.to_owned());
            };
            render(handle, self.request).await
        })
    }
}

fn header(action: &str, request: &WorkflowRequest) -> String {
    match request {
        WorkflowRequest::Validate { name } => format!("{action} {name}"),
        WorkflowRequest::Start(launch) => format!("{action} {}", launch.name),
        WorkflowRequest::Status { run_id: Some(id) }
        | WorkflowRequest::Pause { run_id: id }
        | WorkflowRequest::Resume { run_id: id, .. }
        | WorkflowRequest::Stop { run_id: id } => format!("{action} {id}"),
        _ => action.to_owned(),
    }
}

async fn render(handle: &WorkflowHandle, request: WorkflowRequest) -> ToolExecResult {
    match handle.request(request).await {
        Ok(WorkflowResponse::Catalog(catalog)) => plain(render_catalog(&catalog)),
        Ok(WorkflowResponse::Validation { name, ok, report }) => {
            let verdict = if ok { "valid" } else { "invalid" };
            plain(format!("{name}: {verdict}. {report}"))
        }
        Ok(WorkflowResponse::Started(run)) => plain(format!(
            "Started run {} ({}) — {} [{}]. {STATUS_HINT} {{\"action\":\"status\",\"run_id\":\"{}\"}}.",
            run.display_name, run.run_id, run.workflow_name, run.status, run.run_id
        )),
        Ok(WorkflowResponse::Run(run)) => plain(render_runs(std::slice::from_ref(&run))),
        Ok(WorkflowResponse::Runs(runs)) => plain(render_runs(&runs)),
        Ok(WorkflowResponse::Trusted { name }) => plain(format!("{name} is now trusted.")),
        Ok(WorkflowResponse::Acked(_) | WorkflowResponse::Ack) => plain(String::new()),
        Err(WorkflowError::TrustRequired { name, digest, path }) => error(format!(
            "workflow {name:?} at {} is not trusted (digest {digest}). {TRUST_HINT}",
            path.display()
        )),
        Err(failure) => error(failure.to_string()),
    }
}

fn render_catalog(catalog: &WorkflowCatalog) -> String {
    let mut lines: Vec<String> = if catalog.entries.is_empty() && catalog.invalid.is_empty() {
        vec![NO_WORKFLOWS.to_owned()]
    } else {
        catalog.entries.iter().map(catalog_line).collect()
    };
    if !catalog.invalid.is_empty() {
        lines.push(INVALID_HEADING.to_owned());
        lines.extend(catalog.invalid.iter().map(invalid_line));
    }
    if let Some(dir) = &catalog.project_dir {
        lines.push(format!("{PROJECT_DIR_LABEL}{}", dir.display()));
    }
    if let Some(dir) = &catalog.user_dir {
        lines.push(format!("{USER_DIR_LABEL}{}", dir.display()));
    }
    lines.join("\n")
}

fn catalog_line(entry: &CatalogEntry) -> String {
    let trust = if entry.trusted { TRUSTED } else { UNTRUSTED };
    let mut line = format!(
        "- {} [{}, {trust}]: {}",
        entry.name, entry.source_kind, entry.description
    );
    if let Some(when) = &entry.when_to_use {
        line.push_str(&format!(" When to use: {when}"));
    }
    if !entry.phases.is_empty() {
        line.push_str(&format!(" Phases: {}.", entry.phases.join(" → ")));
    }
    line
}

fn invalid_line(entry: &InvalidEntry) -> String {
    format!(
        "- {} [{}]: {}",
        entry.path.display(),
        entry.source_kind,
        entry.error
    )
}

/// Snapshots as JSON, with the parts that grow without bound cut down: only
/// the newest log lines, and a `result` capped in bytes.
fn render_runs(runs: &[RunSnapshot]) -> String {
    let bounded: Vec<Value> = runs.iter().map(bounded_snapshot).collect();
    serde_json::to_string_pretty(&bounded).unwrap_or_else(|error| error.to_string())
}

fn bounded_snapshot(run: &RunSnapshot) -> Value {
    let mut run = run.clone();
    let excess = run.logs.len().saturating_sub(MAX_STATUS_LOGS);
    run.logs.drain(..excess);
    if let Some(result) = run.result.take() {
        let text = result.to_string();
        run.result = Some(if text.len() > MAX_STATUS_RESULT_BYTES {
            let end = (0..=MAX_STATUS_RESULT_BYTES)
                .rev()
                .find(|index| text.is_char_boundary(*index))
                .unwrap_or(0);
            Value::String(format!("{}{TRUNCATED_RESULT_SUFFIX}", &text[..end]))
        } else {
            result
        });
    }
    serde_json::to_value(run).unwrap_or(Value::Null)
}

fn plain(text: String) -> ToolExecResult {
    ToolExecResult::from(Ok(ToolOutput::Plain(text.into())))
}

fn error(message: String) -> ToolExecResult {
    ToolExecResult {
        is_error: true,
        ..ToolExecResult::from(Ok(ToolOutput::Plain(message.into())))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use caudra_workflow::{RunStatus, RunUsage, SourceKind};
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;

    const NAME: &str = "review";
    const RUN_ID: &str = "run-1";
    const DIGEST: &str = "abc123";
    const SCRIPT_PATH: &str = "/project/.caudra/workflows/review.rhai";
    const PROJECT_WORKFLOWS_DIR: &str = "/project/.caudra/workflows";
    const USER_WORKFLOWS_DIR: &str = "/home/me/.config/caudra/workflows";
    const RUNTIME_ANSWERS: &str = "the tool must relay the runtime's answer";
    const ERRORS_ARE_FLAGGED: &str = "a refused action must be an error result";
    const LOGS_ARE_BOUNDED: &str = "status must keep only the newest log lines";
    const RESULT_IS_BOUNDED: &str = "status must cap a run's result";

    fn execute(input: Value, ctx: &ToolContext) -> (bool, String) {
        let invocation = WorkflowTool.parse(&input).unwrap();
        let result = smol::block_on(invocation.execute(ctx));
        let text = result
            .output
            .map_or_else(|error| error, |output| output.as_text());
        (result.is_error, text)
    }

    fn with_runtime(
        answer: impl Fn(WorkflowRequest) -> Result<WorkflowResponse, WorkflowError> + Send + 'static,
    ) -> ToolContext {
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.workflow = Some(WorkflowHandle::scripted(answer));
        ctx
    }

    fn snapshot() -> RunSnapshot {
        RunSnapshot {
            run_id: RUN_ID.into(),
            display_name: NAME.into(),
            workflow_name: NAME.into(),
            source_kind: SourceKind::User,
            objective: None,
            status: RunStatus::Active,
            pause_kind: None,
            pause_message: None,
            revision: 3,
            execution_epoch: 0,
            phase: None,
            phases: Vec::new(),
            agent_budget: 4,
            usage: RunUsage::default(),
            roster: Vec::new(),
            result: None,
            error: None,
            logs: Vec::new(),
            outbox_pending: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test_case(json!({"action": "list"}), WorkflowRequest::List; "list")]
    #[test_case(json!({"action": "validate", "name": NAME}), WorkflowRequest::Validate { name: NAME.into() }; "validate")]
    #[test_case(json!({"action": "start", "name": NAME, "args": {"branch": "main"}, "agent_budget": 3}), WorkflowRequest::Start(LaunchRequest { name: NAME.into(), args: json!({"branch": "main"}), agent_budget: Some(3) }); "start_with_args")]
    #[test_case(json!({"action": "start", "name": NAME}), WorkflowRequest::Start(LaunchRequest { name: NAME.into(), args: json!({}), agent_budget: None }); "start_defaults_to_empty_args")]
    #[test_case(json!({"action": "status"}), WorkflowRequest::Status { run_id: None }; "status_of_every_run")]
    #[test_case(json!({"action": "status", "run_id": RUN_ID}), WorkflowRequest::Status { run_id: Some(RUN_ID.into()) }; "status_of_one_run")]
    #[test_case(json!({"action": "pause", "run_id": RUN_ID}), WorkflowRequest::Pause { run_id: RUN_ID.into() }; "pause")]
    #[test_case(json!({"action": "resume", "run_id": RUN_ID, "agent_budget": 8}), WorkflowRequest::Resume { run_id: RUN_ID.into(), agent_budget: Some(8) }; "resume")]
    #[test_case(json!({"action": "stop", "run_id": RUN_ID}), WorkflowRequest::Stop { run_id: RUN_ID.into() }; "stop")]
    fn actions_map_to_runtime_requests(input: Value, expected: WorkflowRequest) {
        let (seen_tx, seen) = flume::unbounded();
        let ctx = with_runtime(move |request| {
            seen_tx.send(request).unwrap();
            Ok(WorkflowResponse::Ack)
        });

        let (is_error, _) = execute(input, &ctx);

        assert!(!is_error);
        assert_eq!(seen.recv().unwrap(), expected);
    }

    #[test_case(json!({"action": "validate"}), NAME_REQUIRED; "validate_needs_a_name")]
    #[test_case(json!({"action": "start"}), NAME_REQUIRED; "start_needs_a_name")]
    #[test_case(json!({"action": "pause"}), RUN_ID_REQUIRED; "pause_needs_a_run")]
    #[test_case(json!({"action": "resume"}), RUN_ID_REQUIRED; "resume_needs_a_run")]
    #[test_case(json!({"action": "stop"}), RUN_ID_REQUIRED; "stop_needs_a_run")]
    #[test_case(json!({"action": "start", "name": NAME, "args": "main"}), ARGS_MUST_BE_OBJECT; "args_must_be_an_object")]
    fn incomplete_actions_are_refused_at_parse_time(input: Value, expected: &str) {
        let error = match WorkflowTool.parse(&input) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("{input} must not parse"),
        };
        assert!(error.contains(expected), "{error}");
    }

    #[test]
    fn every_action_is_unavailable_without_a_runtime() {
        let ctx = stub_ctx(&AgentMode::Build);
        for input in [
            json!({"action": "list"}),
            json!({"action": "start", "name": NAME}),
            json!({"action": "status"}),
            json!({"action": "stop", "run_id": RUN_ID}),
        ] {
            let (is_error, text) = execute(input, &ctx);
            assert!(is_error, "{ERRORS_ARE_FLAGGED}");
            assert_eq!(text, UNAVAILABLE);
        }
    }

    #[test]
    fn the_description_says_when_no_runtime_is_attached() {
        let filter = crate::tools::ToolFilter::All;
        let describe = |workflows_available| {
            WorkflowTool
                .description(&DescriptionContext {
                    filter: &filter,
                    audience: ToolAudience::MAIN,
                    workflows_available,
                })
                .into_owned()
        };
        assert!(describe(false).contains(UNAVAILABLE_NOTE));
        assert!(!describe(true).contains(UNAVAILABLE_NOTE));
    }

    #[test]
    fn the_catalog_lists_trust_and_invalid_scripts() {
        let ctx = with_runtime(|_| {
            Ok(WorkflowResponse::Catalog(WorkflowCatalog {
                entries: vec![CatalogEntry {
                    name: NAME.into(),
                    description: "Reviews a branch".into(),
                    when_to_use: Some("before merging".into()),
                    phases: vec!["Plan".into(), "Review".into()],
                    source_kind: SourceKind::Project,
                    path: Some(PathBuf::from(SCRIPT_PATH)),
                    digest: DIGEST.into(),
                    trusted: false,
                    shadowed: Vec::new(),
                }],
                invalid: vec![InvalidEntry {
                    path: PathBuf::from("/project/.caudra/workflows/broken.rhai"),
                    source_kind: SourceKind::Project,
                    error: "meta: missing name".into(),
                }],
                project_dir: Some(PathBuf::from(PROJECT_WORKFLOWS_DIR)),
                user_dir: Some(PathBuf::from(USER_WORKFLOWS_DIR)),
            }))
        });

        let (is_error, text) = execute(json!({"action": "list"}), &ctx);

        assert!(!is_error);
        assert!(
            text.contains(&format!("{NAME} [project, {UNTRUSTED}]")),
            "{text}"
        );
        assert!(text.contains("Plan → Review"), "{text}");
        assert!(text.contains(INVALID_HEADING), "{text}");
        assert!(text.contains("broken.rhai"), "{text}");
        assert!(
            text.contains(&format!("{PROJECT_DIR_LABEL}{PROJECT_WORKFLOWS_DIR}")),
            "{text}"
        );
        assert!(
            text.contains(&format!("{USER_DIR_LABEL}{USER_WORKFLOWS_DIR}")),
            "{text}"
        );
    }

    /// A writer needs the directories even before the first script exists.
    #[test]
    fn an_empty_catalog_still_names_where_scripts_go() {
        let ctx = with_runtime(|_| {
            Ok(WorkflowResponse::Catalog(WorkflowCatalog {
                user_dir: Some(PathBuf::from(USER_WORKFLOWS_DIR)),
                ..WorkflowCatalog::default()
            }))
        });

        let (is_error, text) = execute(json!({"action": "list"}), &ctx);

        assert!(!is_error);
        assert!(text.starts_with(NO_WORKFLOWS), "{text}");
        assert!(text.contains(USER_WORKFLOWS_DIR), "{text}");
        assert!(!text.contains(PROJECT_DIR_LABEL), "{text}");
    }

    #[test]
    fn a_start_answers_with_the_run_and_how_to_poll_it() {
        let ctx = with_runtime(|_| Ok(WorkflowResponse::Started(Box::new(snapshot()))));

        let (is_error, text) = execute(json!({"action": "start", "name": NAME}), &ctx);

        assert!(!is_error);
        assert!(text.contains(RUN_ID), "{RUNTIME_ANSWERS}: {text}");
        assert!(text.contains(STATUS_HINT), "{text}");
    }

    #[test]
    fn an_untrusted_script_is_refused_with_the_approval_hint() {
        let ctx = with_runtime(|_| {
            Err(WorkflowError::TrustRequired {
                name: NAME.into(),
                digest: DIGEST.into(),
                path: PathBuf::from(SCRIPT_PATH),
            })
        });

        let (is_error, text) = execute(json!({"action": "start", "name": NAME}), &ctx);

        assert!(is_error, "{ERRORS_ARE_FLAGGED}");
        assert!(text.contains(SCRIPT_PATH), "{text}");
        assert!(text.contains(DIGEST), "{text}");
        assert!(text.contains(TRUST_HINT), "{text}");
    }

    #[test]
    fn status_keeps_the_newest_logs_and_caps_the_result() {
        let mut run = snapshot();
        run.status = RunStatus::Completed;
        run.logs = (0..MAX_STATUS_LOGS + 5)
            .map(|i| format!("line {i}"))
            .collect();
        run.result = Some(Value::String("x".repeat(MAX_STATUS_RESULT_BYTES * 2)));
        let ctx = with_runtime(move |_| Ok(WorkflowResponse::Run(Box::new(run.clone()))));

        let (is_error, text) = execute(json!({"action": "status", "run_id": RUN_ID}), &ctx);

        assert!(!is_error);
        let runs: Vec<Value> = serde_json::from_str(&text).unwrap();
        assert_eq!(runs.len(), 1);
        let logs = runs[0]["logs"].as_array().unwrap();
        assert_eq!(logs.len(), MAX_STATUS_LOGS, "{LOGS_ARE_BOUNDED}");
        assert_eq!(logs[0], json!("line 5"), "{LOGS_ARE_BOUNDED}");
        let result = runs[0]["result"].as_str().unwrap();
        assert!(
            result.ends_with(TRUNCATED_RESULT_SUFFIX),
            "{RESULT_IS_BOUNDED}"
        );
        assert!(
            result.len() <= MAX_STATUS_RESULT_BYTES + TRUNCATED_RESULT_SUFFIX.len(),
            "{RESULT_IS_BOUNDED}"
        );
    }
}
