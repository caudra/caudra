//! `pstack` as a host module: the Rust ports of the scripts pstack's skills run, each called with
//! the command line upstream takes. It joins a run only while the bundled pstack is on, and stays
//! out of the `code` tool's description, so a session without pstack never reads about it.
//!
//! Every call asks as the `code` tool, naming the script, its arguments, and the files they name.
//! A call that writes files, `log` and orch's commands that change the store, is held out of plan
//! and read-only modes. Commands a script runs, such as `git` and `gh`, start on this machine in
//! the session's project with the environment a `shell` command gets, and the call ending kills
//! them.

mod process;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use caudra_config::ToolKey;
use caudra_config::shell_env::ShellEnvConfig;
use caudra_pstack::scripts::{Script, check_plan, orch};
use caudra_pstack::{Cancelled, Host, NAME, Output};
use caudra_storage::StateDir;
use caudra_storage::sessions::SessionDatabase;
use jiff::Timestamp;
use serde_json::{Value, json};
use smol::Timer;

use super::gateway::Permission;
use super::module::{CallRun, HostCallPlan, HostModule, MemberDoc, ModuleDoc, Prepared};
use super::output::CodeCall;
use super::runner::{ScriptCall, ScriptError, ScriptErrorKind};
use crate::cancel::CancelToken;
use crate::mcp::output::{McpBlock, McpOutput};
use crate::permissions::{
    PermissionExecutorKind, PermissionResource, PermissionResourceAccess, PermissionResourceKind,
    PermissionRisk, PermissionSubject, filesystem_permission_resource,
};
use crate::shell_environment::local_command_environment;
use crate::tools::registry::{PermissionIntent, PermissionScopes};
use crate::tools::{CODE_TOOL_NAME, ToolContext, ToolLive, native};

const ARGV_FIELD: &str = "argv";
const CODE_FIELD: &str = "code";
const STDOUT_FIELD: &str = "stdout";
const STDERR_FIELD: &str = "stderr";
const PERMISSION_SCOPE_PREFIX: &str = "pstack:";
const RUNS: &str = "Runs pstack's";
const EMPTY_COMMAND: &str = "empty command";
/// `check_plan`'s one argument that checks the session plan instead of a file.
const SESSION_PLAN_FLAG: &str = "--plan";
/// What the session plan's problems are reported against, where a file's name would be.
const SESSION_PLAN_LABEL: &str = "plan";
/// Set on every command after the shell environment, so neither CLI waits on a prompt nobody sees.
const PROMPTS_OFF: &[(&str, &str)] = &[("GIT_TERMINAL_PROMPT", "0"), ("GH_PROMPT_DISABLED", "1")];
const GH: &str = "gh";
/// What `gh` prints when it has no credentials it can use.
const GH_SIGNED_OUT: &[&str] = &["gh auth login", "GH_TOKEN", "HTTP 401"];
const GH_SIGNED_OUT_HINT: &str = "Caudra: `gh` could not authenticate. Commands pstack runs get the \
     environment shell commands get, which does not inherit GH_TOKEN, GITHUB_TOKEN, or the \
     variables a keyring needs, such as DBUS_SESSION_BUS_ADDRESS; the user can forward them with \
     /env or `[agent.shell_env] pass`.\n";
/// Why the namespace answers nothing in a session whose commands run on another machine.
pub const ELSEWHERE: &str = "pstack's scripts run on this machine, and this session's workspace is \
     elsewhere; follow the skill's steps with `shell` instead";
pub static DOC: ModuleDoc = ModuleDoc {
    namespace: NAME,
    summary: "`pstack`: Rust ports of the scripts pstack's skills run; `pstack.log`, `pstack.check_plan`, `pstack.worktree_audit`, `pstack.watch_pr`, `pstack.orch`.",
    page: "pstack: the scripts pstack's skills run, ported to Rust. Each call takes the arguments upstream's command line takes, as strings, and returns `{\"code\", \"stdout\", \"stderr\"}`: a nonzero `code` is the script's own verdict, not an error. `log` and the `orch` commands that change the store write files, so plan mode refuses them; `orch`'s help and read-only commands run there. Commands a script runs get the environment shell commands get.\n\
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
            page: "`poteto-mode/scripts/check-plan.mjs`: `pstack.check_plan(plan_md)` checks a plan file's structure and verification boxes. `pstack.check_plan(\"--plan\")` checks the session plan, the one the `plan` tool writes.",
        },
        MemberDoc {
            name: "worktree_audit",
            signature: "worktree_audit(*argv: str) -> dict[str, Any]",
            awaited: true,
            read_first: false,
            page: "`poteto-mode/scripts/worktree-audit.sh`: `pstack.worktree_audit(\"-b\", \"origin/main\", repo_path)` fetches the base, best effort, then tables every worktree; LAST_CHAT is the newest Caudra session that worked there.",
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
    let script = Script::parse(&call.name)
        .ok_or_else(|| ScriptError::new(ScriptErrorKind::Attribute, DOC.no_member(&call.name)))?;
    let qualified = format!("{NAME}.{}", script.name());
    let argv = arguments(&qualified, &call)?;
    let row = CodeCall {
        server: NAME.to_owned(),
        tool: script.name().to_owned(),
        called_as: qualified.clone(),
        input: json!({ ARGV_FIELD: argv }),
        ..CodeCall::default()
    };
    let writes = script.writes(&argv, &global_env);
    let permission = permission(script, &qualified, &argv, &cwd, writes);
    let session_plan = script == Script::CheckPlan && argv == [SESSION_PLAN_FLAG];
    let run: CallRun = Box::new(move |ctx: ToolContext| {
        Box::pin(async move {
            if session_plan {
                return Ok(answered(check_session_plan(&ctx)));
            }
            let (_stop_on_drop, cancel) = ctx.cancel.child();
            let host = LocalHost {
                cwd,
                cancel,
                live: ctx.live_sink.clone(),
                state: StateDir::resolve()
                    .ok()
                    .filter(|state| !state.is_ephemeral()),
                environment: command_environment(&ctx.config.shell_env),
                gh_signed_out: AtomicBool::new(false),
            };
            let output = smol::unblock(move || {
                let output = script.run(&host, &argv);
                host.hinted(output)
            })
            .await;
            Ok(answered(output))
        })
    });
    Ok(HostCallPlan {
        row,
        permission,
        read_only: !writes,
        run,
    })
}

fn global_env(name: &str) -> Option<String> {
    caudra_config::global_env_value(name).ok().flatten()
}

fn check_session_plan(ctx: &ToolContext) -> Output {
    match native::plan::read(ctx) {
        Ok(content) => check_plan::check_text(SESSION_PLAN_LABEL, &content),
        Err(error) => Output::failure(1, format!("{}\n", error.message)),
    }
}

/// What a `shell` command gets, then [`PROMPTS_OFF`].
fn command_environment(config: &ShellEnvConfig) -> Vec<(String, OsString)> {
    let mut environment = local_command_environment(config);
    environment.extend(
        PROMPTS_OFF
            .iter()
            .map(|&(name, value)| (name.to_owned(), OsString::from(value))),
    );
    environment
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

fn permission(
    script: Script,
    qualified: &str,
    argv: &[String],
    cwd: &Path,
    writes: bool,
) -> Permission {
    let command = std::iter::once(qualified.to_owned())
        .chain(argv.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    let (access, risk) = match writes {
        true => (PermissionResourceAccess::Write, PermissionRisk::Medium),
        false => (PermissionResourceAccess::Execute, PermissionRisk::Low),
    };
    let script_resource = PermissionResource {
        kind: PermissionResourceKind::Custom {
            name: NAME.to_owned(),
        },
        value: command,
        access: Some(access),
        protected: false,
        requires_prompt: false,
        attributes: BTreeMap::new(),
    };
    let intent = PermissionIntent::new(
        PermissionScopes::single(format!("{PERMISSION_SCOPE_PREFIX}{}", script.name())),
        std::iter::once(script_resource)
            .chain(path_resource(script, argv, cwd, writes))
            .collect(),
        risk,
    );
    let identity = (
        PermissionSubject::Native {
            owner: native::OWNER.into(),
            contract: native::code::permission_contract().into(),
        },
        PermissionExecutorKind::Native,
    );
    Permission::ask(ToolKey::native(CODE_TOOL_NAME), intent, Some(identity))
        .with_context(format!("{RUNS} `{}` script", script.upstream()))
}

/// The file or directory a call's arguments name, so file rules, protected paths, and the
/// outside-project check hold it as they would a file tool's.
fn path_resource(
    script: Script,
    argv: &[String],
    cwd: &Path,
    writes: bool,
) -> Option<PermissionResource> {
    let access = match writes {
        true => PermissionResourceAccess::Write,
        false => PermissionResourceAccess::Read,
    };
    let (kind, path) = match script {
        Script::Log => (PermissionResourceKind::File, argv.first().cloned()),
        Script::CheckPlan => (
            PermissionResourceKind::File,
            argv.first()
                .filter(|path| *path != SESSION_PLAN_FLAG)
                .cloned(),
        ),
        Script::Orch => (
            PermissionResourceKind::Directory,
            orch::store_use(argv, &global_env).map(|used| used.store),
        ),
        Script::WorktreeAudit | Script::WatchPr => return None,
    };
    path.filter(|path| !path.is_empty())
        .map(|path| filesystem_permission_resource(kind, &cwd.join(path), access, cwd))
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

/// Commands start on this machine with stdin closed and the environment a `shell` command gets,
/// and the call's cancellation kills them and ends any wait.
struct LocalHost {
    cwd: PathBuf,
    cancel: CancelToken,
    live: Option<flume::Sender<ToolLive>>,
    state: Option<StateDir>,
    environment: Vec<(String, OsString)>,
    /// Whether a `gh` command failed for want of credentials, which the output then explains.
    gh_signed_out: AtomicBool,
}

fn gh_signed_out(program: &str, output: &Output) -> bool {
    program == GH
        && output.code != 0
        && GH_SIGNED_OUT
            .iter()
            .any(|marker| output.stderr.contains(marker))
}

impl LocalHost {
    fn hinted(&self, mut output: Output) -> Output {
        if self.gh_signed_out.load(Ordering::Relaxed) {
            if !output.stderr.is_empty() && !output.stderr.ends_with('\n') {
                output.stderr.push('\n');
            }
            output.stderr.push_str(GH_SIGNED_OUT_HINT);
        }
        output
    }
}

impl Host for LocalHost {
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Never a value only the project's `.env` set, so a repository cannot point `orch` at a
    /// store or repository of its choosing.
    fn env_var(&self, name: &str) -> Option<String> {
        global_env(name)
    }

    fn run(&self, argv: &[&str], cwd: Option<&Path>) -> io::Result<Output> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, EMPTY_COMMAND))?;
        let mut command =
            process::command(program, args, cwd.unwrap_or(&self.cwd), &self.environment);
        caudra_config::proxy_env().configure_command(&mut command);
        let output = smol::block_on(process::run(command, &self.cancel))?;
        if gh_signed_out(program, &output) {
            self.gh_signed_out.store(true, Ordering::Relaxed);
        }
        Ok(output)
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
    use std::env;
    use std::fs;
    use std::process::Command;
    use std::sync::Arc;

    use caudra_config::PermissionsConfig;
    use caudra_storage::plans::PlanFile;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::super::gateway::Gateway;
    use super::super::runner::{ScriptHost, ScriptInvocation, ScriptReply};
    use super::process::COMMAND_CANCELLED;
    use super::*;
    use crate::AgentMode;
    use crate::permissions::{PermissionManager, PermissionMode};
    use crate::tools::native::tests::tempdir;
    use crate::tools::test_support::stub_ctx_with_permissions;

    const PLAN: &str = "plan.md";
    const WORK: &str = "/work";
    const STORE: &str = "store";
    const ORCH_STORE: &str = "ORCH_STORE";
    const PLANTED_STORE: &str = "/planted";
    const DOTENV_CHILD: &str = "CAUDRA_PSTACK_DOTENV_TEST";
    const DOTENV_TEST: &str =
        "code::pstack::tests::a_project_env_file_cannot_set_what_scripts_read";
    const PROJECT_ENV: &str = ".caudra/.env";
    const VALID_PLAN_HEAD: &str = "# Plan";

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

    fn strings(argv: &[&str]) -> Vec<Value> {
        argv.iter().map(|arg| json!(arg)).collect()
    }

    #[test]
    fn every_script_is_a_documented_member() {
        let members: Vec<&str> = DOC.member_names().collect();
        let scripts: Vec<&str> = Script::ALL.iter().map(|script| script.name()).collect();
        assert_eq!(members, scripts);
    }

    #[test_case("log", &[PLAN], false ; "log_writes")]
    #[test_case("orch", &["--store", STORE, "unit", "add", "u1", "--track", "t"], false ; "orch_add_writes")]
    #[test_case("orch", &["--store", STORE, "unit", "list"], true ; "orch_list_reads")]
    #[test_case("orch", &["--help"], true ; "orch_help_reads")]
    #[test_case("check_plan", &[PLAN], true ; "check_plan_reads")]
    #[test_case("worktree_audit", &[PLAN], true ; "worktree_audit_reads")]
    #[test_case("watch_pr", &[PLAN], true ; "watch_pr_reads")]
    fn only_writing_calls_leave_plan_mode(name: &str, argv: &[&str], read_only: bool) {
        let plan = plan(PathBuf::from(WORK), call(name, strings(argv), &[])).unwrap();
        assert_eq!(plan.read_only, read_only);
        assert_eq!(plan.row.input, json!({ ARGV_FIELD: argv }));
    }

    #[test_case("log", &[PLAN, "p", "d", "w", "e", "r"], Some((PermissionResourceKind::File, PLAN, PermissionResourceAccess::Write)) ; "log_writes_its_logfile")]
    #[test_case("orch", &["--store", STORE, "init"], Some((PermissionResourceKind::Directory, STORE, PermissionResourceAccess::Write)) ; "orch_writes_its_store")]
    #[test_case("orch", &["--store", STORE, "gate", "list"], Some((PermissionResourceKind::Directory, STORE, PermissionResourceAccess::Read)) ; "orch_reads_its_store")]
    #[test_case("orch", &["--help"], None ; "orch_help_names_no_store")]
    #[test_case("check_plan", &[PLAN], Some((PermissionResourceKind::File, PLAN, PermissionResourceAccess::Read)) ; "check_plan_reads_its_plan")]
    #[test_case("check_plan", &[SESSION_PLAN_FLAG], None ; "the_session_plan_names_no_file")]
    #[test_case("watch_pr", &["--pr", "1"], None ; "watch_pr_names_no_file")]
    fn calls_name_the_paths_they_touch(
        name: &str,
        argv: &[&str],
        expected: Option<(PermissionResourceKind, &str, PermissionResourceAccess)>,
    ) {
        let script = Script::parse(name).unwrap();
        let argv: Vec<String> = argv.iter().map(|arg| (*arg).to_owned()).collect();
        let writes = script.writes(&argv, &global_env);
        let resource = path_resource(script, &argv, Path::new(WORK), writes);
        assert_eq!(
            resource.map(|resource| (resource.kind, resource.value, resource.access)),
            expected.map(|(kind, path, access)| (
                kind,
                Path::new(WORK).join(path).to_string_lossy().into_owned(),
                Some(access)
            ))
        );
    }

    #[test]
    fn a_path_outside_the_project_asks() {
        let resource = path_resource(
            Script::Log,
            &["/elsewhere/log.tsv".to_owned()],
            Path::new(WORK),
            true,
        )
        .unwrap();
        assert!(resource.requires_prompt);
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
            cwd: env::temp_dir(),
            cancel,
            live: None,
            state: None,
            environment: command_environment(&ShellEnvConfig::default()),
            gh_signed_out: AtomicBool::new(false),
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

    #[cfg(unix)]
    #[test_case("GIT_TERMINAL_PROMPT", "0" ; "git_never_prompts")]
    #[test_case("GH_PROMPT_DISABLED", "1" ; "gh_never_prompts")]
    #[test_case("NO_COLOR", "1" ; "colour_is_off")]
    fn commands_get_the_shell_environment_without_prompts(name: &str, value: &str) {
        let output = local_host(CancelToken::none())
            .run(&["sh", "-c", &format!("printf %s \"${name}\"")], None)
            .unwrap();
        assert_eq!(output.stdout, value);
    }

    const GH_LOGIN: &str = "To get started with GitHub CLI, please run:  gh auth login";

    #[test_case(GH, 4, GH_LOGIN, true ; "signed_out")]
    #[test_case(GH, 1, "HTTP 401: Bad credentials", true ; "rejected_token")]
    #[test_case(GH, 1, "no pull requests found", false ; "another_failure")]
    #[test_case(GH, 0, GH_LOGIN, false ; "a_success")]
    #[test_case("git", 1, GH_LOGIN, false ; "another_program")]
    fn a_signed_out_gh_explains_where_its_token_went(
        program: &str,
        code: i32,
        stderr: &str,
        hinted: bool,
    ) {
        let host = local_host(CancelToken::none());
        host.gh_signed_out.store(
            gh_signed_out(program, &Output::failure(code, stderr)),
            Ordering::Relaxed,
        );
        let output = host.hinted(Output::failure(1, stderr));
        assert_eq!(output.stderr.ends_with(GH_SIGNED_OUT_HINT), hinted);
        assert!(output.stderr.starts_with(stderr));
    }

    #[test]
    fn a_cancelled_call_ends_its_commands_and_waits_at_once() {
        let (trigger, cancel) = CancelToken::new();
        trigger.cancel();
        let host = local_host(cancel);
        let error = host.run(&["sh", "-c", "exit 0"], None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(error.to_string(), COMMAND_CANCELLED);
        assert_eq!(host.sleep(Duration::from_secs(3_600)), Err(Cancelled));
    }

    #[test]
    fn an_unsaved_session_names_no_last_session() {
        assert_eq!(
            local_host(CancelToken::none()).last_session_at(Path::new(WORK)),
            None
        );
    }

    /// Runs itself again with a project `.env` loaded and nothing else, since the process
    /// environment cannot be changed from a test.
    #[test]
    fn a_project_env_file_cannot_set_what_scripts_read() {
        let Some(root) = env::var_os(DOTENV_CHILD) else {
            let home = TempDir::new().unwrap();
            let project = home.path().join("project");
            fs::create_dir_all(project.join(".caudra")).unwrap();
            fs::write(
                project.join(PROJECT_ENV),
                format!("{ORCH_STORE}={PLANTED_STORE}\n"),
            )
            .unwrap();
            let status = Command::new(env::current_exe().unwrap())
                .args(["--exact", DOTENV_TEST, "--nocapture"])
                .env_clear()
                .env(DOTENV_CHILD, &project)
                .env("HOME", home.path())
                .env("XDG_CONFIG_HOME", home.path())
                .env("XDG_DATA_HOME", home.path())
                .env("XDG_STATE_HOME", home.path())
                .env("XDG_CACHE_HOME", home.path())
                .current_dir(home.path())
                .status()
                .unwrap();
            assert!(status.success());
            return;
        };
        caudra_config::load_env_files(Path::new(&root));
        assert_eq!(env::var(ORCH_STORE).as_deref(), Ok(PLANTED_STORE));
        assert_eq!(local_host(CancelToken::none()).env_var(ORCH_STORE), None);
    }

    #[test]
    fn the_snippet_receives_the_code_and_both_streams() {
        let (row, value) = answered(Output {
            code: 1,
            stdout: "out".to_owned(),
            stderr: "err".to_owned(),
        });
        assert_eq!(
            value,
            json!({ CODE_FIELD: 1, STDOUT_FIELD: "out", STDERR_FIELD: "err" })
        );
        assert!(matches!(&row.blocks[0], McpBlock::Text { text } if text == "exit 1\nout\nerr"));
    }

    /// A gateway for a session whose project is the returned directory, planning into its
    /// `PLAN` when `planning`.
    fn gateway(planning: bool) -> (TempDir, Gateway) {
        let dir = tempdir();
        let permissions = Arc::new(PermissionManager::new_nonpersistent(
            PermissionsConfig::default(),
            dir.path().to_path_buf(),
            Arc::default(),
        ));
        permissions.set_session_mode(Some(PermissionMode::Yolo));
        let mode = match planning {
            true => AgentMode::Plan(dir.path().join(PLAN)),
            false => AgentMode::Build,
        };
        let mut ctx = stub_ctx_with_permissions(&mode, permissions);
        ctx.task_environment = ctx
            .task_environment
            .set(native::plan::CWD, dir.path().to_str().unwrap());
        let module = PstackModule { unavailable: None };
        (dir, Gateway::new(ctx, vec![Arc::new(module)]))
    }

    fn invoke(gateway: &Gateway, name: &str, argv: &[&str]) -> ScriptReply {
        match gateway.call(call(name, strings(argv), &[])) {
            ScriptInvocation::Ready(reply) => reply,
            ScriptInvocation::Deferred(future) => smol::block_on(future),
        }
    }

    #[test]
    fn orch_help_runs_in_plan_mode() {
        let (_dir, gateway) = gateway(true);
        let reply = invoke(&gateway, "orch", &["--help"]).unwrap();
        assert_eq!(reply[CODE_FIELD], 0);
    }

    #[test]
    fn an_orch_write_is_refused_in_plan_mode() {
        let (_dir, gateway) = gateway(true);
        let error = invoke(&gateway, "orch", &["--store", STORE, "init"]).unwrap_err();
        assert_eq!(error.kind, ScriptErrorKind::Permission);
    }

    #[test]
    fn an_orch_write_runs_in_build_mode() {
        let (dir, gateway) = gateway(false);
        let reply = invoke(&gateway, "orch", &["--store", STORE, "init"]).unwrap();
        assert_eq!(reply[CODE_FIELD], 0, "{reply}");
        assert!(dir.path().join(STORE).is_dir());
    }

    #[test]
    fn check_plan_reads_the_session_plan() {
        let (dir, gateway) = gateway(true);
        let plan = format!("{VALID_PLAN_HEAD}\n");
        PlanFile::new(dir.path().join(PLAN))
            .unwrap()
            .write(&plan)
            .unwrap();
        let reply = invoke(&gateway, "check_plan", &[SESSION_PLAN_FLAG]).unwrap();
        let expected = check_plan::check_text(SESSION_PLAN_LABEL, &plan);
        assert_eq!(
            reply,
            json!({ CODE_FIELD: expected.code, STDOUT_FIELD: expected.stdout, STDERR_FIELD: expected.stderr })
        );
    }

    #[test]
    fn a_session_without_a_plan_fails_cleanly() {
        let (_dir, gateway) = gateway(false);
        let reply = invoke(&gateway, "check_plan", &[SESSION_PLAN_FLAG]).unwrap();
        assert_eq!(reply[CODE_FIELD], 1);
        assert!(!reply[STDERR_FIELD].as_str().unwrap().is_empty());
    }
}
