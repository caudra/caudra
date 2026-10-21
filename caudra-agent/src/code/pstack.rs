//! `pstack` as a host module: the Rust ports of the scripts pstack's skills run, each called with
//! the command line upstream takes. It joins a run only while the bundled pstack is on, and stays
//! out of the `code` tool's description, so a session without pstack never reads about it.
//!
//! Every call asks as the `code` tool, naming the script and its arguments. A script that writes
//! files, `log` and `orch`, is held out of plan and read-only modes. Commands a script runs, such
//! as `git` and `gh`, start on this machine in the session's project, and a cancelled call kills
//! them.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use caudra_config::ToolKey;
use caudra_pstack::scripts::Script;
use caudra_pstack::{Cancelled, Host, NAME, Output};
use caudra_storage::StateDir;
use caudra_storage::sessions::SessionDatabase;
use jiff::Timestamp;
use serde_json::{Value, json};
use smol::Timer;
use smol::process::{Command, Stdio};

use super::gateway::Permission;
use super::module::{CallRun, HostCallPlan, HostModule, MemberDoc, ModuleDoc, Prepared};
use super::output::CodeCall;
use super::runner::{ScriptCall, ScriptError, ScriptErrorKind};
use crate::cancel::CancelToken;
use crate::mcp::output::{McpBlock, McpOutput};
use crate::permissions::{
    PermissionExecutorKind, PermissionResource, PermissionResourceAccess, PermissionResourceKind,
    PermissionRisk, PermissionSubject,
};
use crate::tools::registry::{PermissionIntent, PermissionScopes};
use crate::tools::{CODE_TOOL_NAME, ToolContext, ToolLive, native};

const ARGV_FIELD: &str = "argv";
const CODE_FIELD: &str = "code";
const STDOUT_FIELD: &str = "stdout";
const STDERR_FIELD: &str = "stderr";
const PERMISSION_SCOPE_PREFIX: &str = "pstack:";
const RUNS: &str = "Runs pstack's";
const EMPTY_COMMAND: &str = "empty command";
const COMMAND_CANCELLED: &str = "cancelled";
/// What a command killed by a signal exits with, as a shell reports it.
const SIGNALLED: i32 = 128;
/// Why the namespace answers nothing in a session whose commands run on another machine.
pub const ELSEWHERE: &str = "pstack's scripts run on this machine, and this session's workspace is \
     elsewhere; follow the skill's steps with `shell` instead";
pub static DOC: ModuleDoc = ModuleDoc {
    namespace: NAME,
    summary: "`pstack`: Rust ports of the scripts pstack's skills run; `pstack.log`, `pstack.check_plan`, `pstack.worktree_audit`, `pstack.watch_pr`, `pstack.orch`.",
    page: "pstack: the scripts pstack's skills run, ported to Rust. Each call takes the arguments upstream's command line takes, as strings, and returns `{\"code\", \"stdout\", \"stderr\"}`: a nonzero `code` is the script's own verdict, not an error. `log` and `orch` write files, so plan mode refuses them.\n\
Example: `r = await pstack.check_plan(\"plans/feature.md\")`, then `r[\"code\"] == 0`.",
    members: &[
        MemberDoc {
            name: "log",
            signature: "log(*argv: str) -> dict[str, Any]",
            awaited: true,
            read_first: false,
            page: "`show-me-your-work/scripts/log.sh`: `pstack.log(logfile, phase, decision, why, evidence, result)` appends one tab-separated row, writing the header first.",
        },
        MemberDoc {
            name: "check_plan",
            signature: "check_plan(*argv: str) -> dict[str, Any]",
            awaited: true,
            read_first: false,
            page: "`poteto-mode/scripts/check-plan.mjs`: `pstack.check_plan(plan_md)` checks a plan's structure and verification boxes.",
        },
        MemberDoc {
            name: "worktree_audit",
            signature: "worktree_audit(*argv: str) -> dict[str, Any]",
            awaited: true,
            read_first: false,
            page: "`poteto-mode/scripts/worktree-audit.sh`: `pstack.worktree_audit(\"-b\", \"origin/main\", repo_path)` tables every worktree; LAST_CHAT is the newest Caudra session that worked there.",
        },
        MemberDoc {
            name: "watch_pr",
            signature: "watch_pr(*argv: str) -> dict[str, Any]",
            awaited: true,
            read_first: false,
            page: "`poteto-mode/scripts/watch-pr`: `pstack.watch_pr(\"--pr\", \"123\", \"--status-only\")` reports once; without `--status-only` it polls until a verdict, so give the run a `timeoutSec` that covers it. `pstack.watch_pr(\"--help\")` lists every option.",
        },
        MemberDoc {
            name: "orch",
            signature: "orch(*argv: str) -> dict[str, Any]",
            awaited: true,
            read_first: false,
            page: "`poteto-mode/scripts/orch/orch.ts`: the orchestrate store, `pstack.orch(\"--store\", dir, \"unit\", \"add\", ...)`. `pstack.orch(\"--help\")` lists every command.",
        },
    ],
};

pub struct PstackModule {
    unavailable: Option<&'static str>,
}

impl PstackModule {
    pub fn for_session(ctx: &ToolContext) -> Self {
        let elsewhere = ctx.workspace_session.is_some() || ctx.host_cwd.is_some();
        Self {
            unavailable: elsewhere.then_some(ELSEWHERE),
        }
    }
}

impl HostModule for PstackModule {
    fn doc(&self) -> &'static ModuleDoc {
        &DOC
    }

    fn unavailable(&self) -> Option<String> {
        self.unavailable.map(str::to_owned)
    }

    fn prepare(&self, ctx: &ToolContext, call: ScriptCall) -> Prepared {
        Prepared::Plan(plan(ctx.permissions.project_cwd(), call).map(Box::new))
    }
}

fn plan(cwd: PathBuf, call: ScriptCall) -> Result<HostCallPlan, ScriptError> {
    let script = Script::parse(&call.name).ok_or_else(|| {
        ScriptError::new(ScriptErrorKind::Attribute, DOC.no_member(&call.name))
    })?;
    let qualified = format!("{NAME}.{}", script.name());
    let argv = arguments(&qualified, &call)?;
    let row = CodeCall {
        server: NAME.to_owned(),
        tool: script.name().to_owned(),
        called_as: qualified.clone(),
        input: json!({ ARGV_FIELD: argv }),
        ..CodeCall::default()
    };
    let permission = permission(script, &qualified, &argv);
    let run: CallRun = Box::new(move |ctx: ToolContext| {
        Box::pin(async move {
            let host = LocalHost {
                cwd,
                cancel: ctx.cancel.clone(),
                live: ctx.live_sink.clone(),
                state: StateDir::resolve().ok().filter(|state| !state.is_ephemeral()),
            };
            let output = smol::unblock(move || script.run(&host, &argv)).await;
            Ok(answered(output))
        })
    });
    Ok(HostCallPlan {
        row,
        permission,
        read_only: !script.writes(),
        run,
    })
}

/// The command line as strings, positional only, as upstream's scripts take it.
fn arguments(qualified: &str, call: &ScriptCall) -> Result<Vec<String>, ScriptError> {
    let type_error = |message: String| ScriptError::new(ScriptErrorKind::Type, message);
    if let Some(name) = call.kwargs.keys().next() {
        return Err(type_error(format!(
            "{qualified}() got an unexpected keyword argument '{name}'; pass the command line as strings"
        )));
    }
    call.args
        .iter()
        .map(|argument| match argument {
            Value::String(text) => Ok(text.clone()),
            other => Err(type_error(format!(
                "{qualified}() arguments must be str, not {other}"
            ))),
        })
        .collect()
}

fn permission(script: Script, qualified: &str, argv: &[String]) -> Permission {
    let command = std::iter::once(qualified.to_owned())
        .chain(argv.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    let (access, risk) = match script.writes() {
        true => (PermissionResourceAccess::Write, PermissionRisk::Medium),
        false => (PermissionResourceAccess::Execute, PermissionRisk::Low),
    };
    let intent = PermissionIntent::new(
        PermissionScopes::single(format!("{PERMISSION_SCOPE_PREFIX}{}", script.name())),
        vec![PermissionResource {
            kind: PermissionResourceKind::Custom {
                name: NAME.to_owned(),
            },
            value: command,
            access: Some(access),
            protected: false,
            requires_prompt: false,
            attributes: BTreeMap::new(),
        }],
        risk,
    );
    let identity = (
        PermissionSubject::Native {
            owner: native::OWNER.into(),
            contract: native::code::permission_contract().into(),
        },
        PermissionExecutorKind::Native,
    );
    Permission::ask(ToolKey::native(CODE_TOOL_NAME), intent, Some(identity)).with_context(format!(
        "{RUNS} `{}` script",
        script.upstream()
    ))
}

/// The row shows what the script printed; the snippet gets the exit code and both streams.
fn answered(output: Output) -> (McpOutput, Value) {
    let Output {
        code,
        stdout,
        stderr,
    } = output;
    let printed = [stdout.as_str(), stderr.as_str()]
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let value = json!({ CODE_FIELD: code, STDOUT_FIELD: stdout, STDERR_FIELD: stderr });
    let summary = match printed.is_empty() {
        true => format!("exit {code}"),
        false => format!("exit {code}\n{printed}"),
    };
    let row = McpOutput {
        blocks: vec![McpBlock::Text { text: summary }],
        structured: Some(value.clone()),
        ..McpOutput::default()
    };
    (row, value)
}

/// Commands start on this machine with stdin closed, and the call's cancellation kills them and
/// ends any wait.
struct LocalHost {
    cwd: PathBuf,
    cancel: CancelToken,
    live: Option<flume::Sender<ToolLive>>,
    state: Option<StateDir>,
}

impl Host for LocalHost {
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn env_var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn run(&self, argv: &[&str], cwd: Option<&Path>) -> io::Result<Output> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, EMPTY_COMMAND))?;
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(cwd.unwrap_or(&self.cwd))
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let output = smol::block_on(self.cancel.race(command.output()))
            .map_err(|_| io::Error::new(io::ErrorKind::Interrupted, COMMAND_CANCELLED))??;
        Ok(Output {
            code: output.status.code().unwrap_or(SIGNALLED),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn now(&self) -> Timestamp {
        Timestamp::now()
    }

    fn sleep(&self, duration: Duration) -> Result<(), Cancelled> {
        smol::block_on(self.cancel.race(Timer::after(duration)))
            .map(drop)
            .map_err(|_| Cancelled)
    }

    fn progress(&self, line: &str) {
        if let Some(live) = &self.live {
            let _ = live.try_send(ToolLive::Annotation(line.to_owned()));
        }
    }

    fn last_session_at(&self, dir: &Path) -> Option<Timestamp> {
        let database = SessionDatabase::open_read_only(self.state.as_ref()?).ok()?;
        let newest = database
            .list(&dir.to_string_lossy())
            .ok()?
            .into_iter()
            .next()?;
        Timestamp::from_second(i64::try_from(newest.updated_at).ok()?).ok()
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const PLAN: &str = "plan.md";
    const WORK: &str = "/work";

    fn call(name: &str, args: Vec<Value>, kwargs: &[(&str, Value)]) -> ScriptCall {
        ScriptCall {
            namespace: Some(NAME.to_owned()),
            name: name.to_owned(),
            args,
            kwargs: kwargs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
        }
    }

    #[test]
    fn every_script_is_a_documented_member() {
        let members: Vec<&str> = DOC.member_names().collect();
        let scripts: Vec<&str> = Script::ALL.iter().map(|script| script.name()).collect();
        assert_eq!(members, scripts);
    }

    #[test_case("log", false ; "log_writes")]
    #[test_case("orch", false ; "orch_writes")]
    #[test_case("check_plan", true ; "check_plan_reads")]
    #[test_case("worktree_audit", true ; "worktree_audit_reads")]
    #[test_case("watch_pr", true ; "watch_pr_reads")]
    fn only_writing_scripts_leave_plan_mode(name: &str, read_only: bool) {
        let plan = plan(PathBuf::from(WORK), call(name, vec![json!(PLAN)], &[])).unwrap();
        assert_eq!(plan.read_only, read_only);
        assert_eq!(plan.row.input, json!({ ARGV_FIELD: [PLAN] }));
    }

    #[test_case(vec![json!(1)], &[] ; "non_string_argument")]
    #[test_case(vec![], &[("plan", json!(PLAN))] ; "keyword_argument")]
    fn arguments_are_a_command_line_of_strings(args: Vec<Value>, kwargs: &[(&str, Value)]) {
        let error = refused(call("check_plan", args, kwargs));
        assert_eq!(error.kind, ScriptErrorKind::Type);
    }

    fn refused(call: ScriptCall) -> ScriptError {
        plan(PathBuf::from(WORK), call)
            .err()
            .expect("the call is refused")
    }

    #[test]
    fn an_unknown_member_names_the_real_ones() {
        let error = refused(call("lint", vec![], &[]));
        assert_eq!(error.kind, ScriptErrorKind::Attribute);
        assert_eq!(error.message, DOC.no_member("lint"));
    }

    fn local_host(cancel: CancelToken) -> LocalHost {
        LocalHost {
            cwd: std::env::temp_dir(),
            cancel,
            live: None,
            state: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_command_reports_its_exit_code_and_both_streams() {
        let output = local_host(CancelToken::none())
            .run(&["sh", "-c", "printf out; printf err >&2; exit 3"], None)
            .unwrap();
        assert_eq!(
            output,
            Output {
                code: 3,
                stdout: "out".to_owned(),
                stderr: "err".to_owned(),
            }
        );
    }

    #[test]
    fn a_cancelled_call_ends_its_commands_and_waits_at_once() {
        let (trigger, cancel) = CancelToken::new();
        trigger.cancel();
        let host = local_host(cancel);
        let error = host.run(&["sh", "-c", "exit 0"], None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(host.sleep(Duration::from_secs(3_600)), Err(Cancelled));
    }

    #[test]
    fn an_unsaved_session_names_no_last_session() {
        assert_eq!(
            local_host(CancelToken::none()).last_session_at(Path::new(WORK)),
            None
        );
    }

    #[test]
    fn the_snippet_receives_the_code_and_both_streams() {
        let (row, value) = answered(Output {
            code: 1,
            stdout: "out".to_owned(),
            stderr: "err".to_owned(),
        });
        assert_eq!(value, json!({ CODE_FIELD: 1, STDOUT_FIELD: "out", STDERR_FIELD: "err" }));
        assert!(matches!(&row.blocks[0], McpBlock::Text { text } if text == "exit 1\nout\nerr"));
    }
}
