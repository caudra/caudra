//! Port of poteto-mode's `orch`: plain-file bookkeeping for the orchestrate playbook (units, a
//! verification ledger, an inbox of agent pointers, decision gates, the stack frontier, and
//! standing orders), one command in and one line out. This file is upstream's `orch.ts` CLI;
//! [`store`] is its `store.ts`.

mod commander;
mod compat;
mod files;
mod forge;
mod lock;
mod model;
mod status;
mod store;

use serde::Serialize;
use serde_json::json;

use crate::host::{Host, Output};

use commander::{Argument, Command, Invocation, Opt, Value, help, parse};
use compat::MAX_SAFE_INTEGER;
use forge::FORGES;
use lock::LockTakeover;
use model::{
    Counts, Frontier, Gate, InboxPointer, OrchError, OrchResult, StandingLine, StatusReport, Unit,
    parse_verdict,
};
use store::{GatePark, LedgerRecord, Store, UnitUpdate};

const DISPLAY_LIMIT: usize = 4;
const STORE: &str = "--store";
const JSON: &str = "--json";
const FORCE: &str = "--force";
const STATE: &str = "--state";
const TRACK: &str = "--track";
const BRIEF: &str = "--brief";
const BRANCH: &str = "--branch";
const PR: &str = "--pr";
const SHA: &str = "--sha";
const EVIDENCE: &str = "--evidence";
const VERIFIER: &str = "--verifier";
const REPORT: &str = "--report";
const PEEK: &str = "--peek";
const QUESTION: &str = "--question";
const OPTIONS: &str = "--options";
const DEFAULT: &str = "--default";
const ANSWER: &str = "--answer";
const REPO: &str = "--repo";
const FORGE: &str = "--forge";
const PRS: &str = "--prs";
const DEFAULT_FORGE: &str = "gh";
const STORE_REQUIRED: &str = "set --store <dir> or ORCH_STORE";
const REPO_REQUIRED: &str = "set --repo <dir> or ORCH_REPO";
const COMMAND_REQUIRED: &str = "a valid command is required";
const NOT_POSITIVE: &str = "must be a positive integer";
const NOT_A_PR_LIST: &str = "requires a comma-separated PR list";
const NOT_A_FORGE: &str = "Allowed choices are gh, origin.";

#[derive(Debug, Clone, Copy)]
enum Action {
    RequireSubcommand,
    Init,
    UnitAdd,
    UnitSet,
    UnitGet,
    UnitList,
    UnitCounts,
    LedgerRecord,
    LedgerCheck,
    LedgerSummary,
    InboxPush,
    InboxDrain,
    InboxCount,
    GatePark,
    GateList,
    GateResolve,
    FrontierSet,
    FrontierShow,
    Status,
    StandingShow,
    StandingAdd,
}

fn positive_integer(value: &str) -> Result<Value, String> {
    let canonical = value.starts_with(|first: char| ('1'..='9').contains(&first))
        && value.bytes().all(|byte| byte.is_ascii_digit());
    value
        .parse::<u64>()
        .ok()
        .filter(|parsed| canonical && *parsed <= MAX_SAFE_INTEGER.unsigned_abs())
        .map(Value::Integer)
        .ok_or_else(|| NOT_POSITIVE.to_owned())
}

fn pr_list(value: &str) -> Result<Value, String> {
    let parts: Vec<&str> = value.split(',').collect();
    if parts.iter().any(|part| part.is_empty()) {
        return Err(NOT_A_PR_LIST.to_owned());
    }
    parts
        .into_iter()
        .map(|part| match positive_integer(part)? {
            Value::Integer(pr) => Ok(pr),
            _ => Err(NOT_POSITIVE.to_owned()),
        })
        .collect::<Result<_, _>>()
        .map(Value::Integers)
}

fn forge_choice(value: &str) -> Result<Value, String> {
    if FORGES.contains(&value) {
        Ok(Value::Text(value.to_owned()))
    } else {
        Err(NOT_A_FORGE.to_owned())
    }
}

const fn option(flags: &'static str, description: &'static str) -> Opt {
    Opt {
        flags,
        description,
        mandatory: false,
        env: None,
        parser: None,
    }
}

const fn required(flags: &'static str, description: &'static str) -> Opt {
    Opt {
        mandatory: true,
        ..option(flags, description)
    }
}

const fn argument(name: &'static str) -> Argument {
    Argument {
        name,
        description: "",
        parser: None,
    }
}

const fn leaf(name: &'static str, description: &'static str, action: Action) -> Command<Action> {
    Command {
        name,
        description,
        usage: None,
        arguments: &[],
        options: &[],
        commands: &[],
        action,
    }
}

const fn group(
    name: &'static str,
    description: &'static str,
    commands: &'static [Command<Action>],
) -> Command<Action> {
    Command {
        commands,
        ..leaf(name, description, Action::RequireSubcommand)
    }
}

const ID: &[Argument] = &[argument("id")];

static PROGRAM: Command<Action> = Command {
    name: "orch",
    description: "Plain-file orchestrate bookkeeping",
    usage: Some("[--store <dir>] [--json] [--force] <command>"),
    arguments: &[],
    options: &[
        Opt {
            env: Some("ORCH_STORE"),
            ..option(
                "--store <dir>",
                "store directory (or ORCH_STORE) (env: ORCH_STORE)",
            )
        },
        option("--json", "print complete rows as JSON (default: false)"),
        option("--force", "steal an existing store lock (default: false)"),
    ],
    commands: &[
        leaf("init", "initialize the store", Action::Init),
        group(
            "unit",
            "manage work units",
            &[
                Command {
                    arguments: ID,
                    options: &[
                        required("--track <track>", "unit track"),
                        option("--brief <path>", "brief path"),
                    ],
                    ..leaf("add", "add a unit", Action::UnitAdd)
                },
                Command {
                    arguments: ID,
                    options: &[
                        required("--state <state>", "unit state"),
                        option("--branch <branch>", "branch name"),
                        Opt {
                            parser: Some(positive_integer),
                            ..option("--pr <number>", "pull request number")
                        },
                        option("--sha <sha>", "commit SHA"),
                    ],
                    ..leaf("set", "update a unit", Action::UnitSet)
                },
                Command {
                    arguments: ID,
                    ..leaf("get", "get a unit", Action::UnitGet)
                },
                Command {
                    options: &[
                        option("--state <state>", "filter by state"),
                        option("--track <track>", "filter by track"),
                    ],
                    ..leaf("list", "list units", Action::UnitList)
                },
                leaf("counts", "count units by state", Action::UnitCounts),
            ],
        ),
        group(
            "ledger",
            "manage verification records",
            &[
                Command {
                    arguments: &[
                        Argument {
                            name: "pr",
                            description: "pull request number",
                            parser: Some(positive_integer),
                        },
                        Argument {
                            name: "sha",
                            description: "commit SHA",
                            parser: None,
                        },
                        Argument {
                            name: "verdict",
                            description: "verification verdict",
                            parser: None,
                        },
                    ],
                    options: &[
                        required("--evidence <path>", "evidence path"),
                        option("--verifier <name>", "verifier name"),
                    ],
                    ..leaf(
                        "record",
                        "record a verification verdict",
                        Action::LedgerRecord,
                    )
                },
                Command {
                    arguments: &[
                        Argument {
                            name: "pr",
                            description: "pull request number",
                            parser: Some(positive_integer),
                        },
                        Argument {
                            name: "sha",
                            description: "commit SHA",
                            parser: None,
                        },
                    ],
                    ..leaf("check", "check a verification verdict", Action::LedgerCheck)
                },
                leaf(
                    "summary",
                    "count verification verdicts",
                    Action::LedgerSummary,
                ),
            ],
        ),
        group(
            "inbox",
            "manage agent pointers",
            &[
                Command {
                    arguments: &[argument("agent"), argument("unit"), argument("status")],
                    options: &[option("--report <path>", "report path")],
                    ..leaf("push", "push an inbox pointer", Action::InboxPush)
                },
                Command {
                    options: &[option("--peek", "read without draining (default: false)")],
                    ..leaf("drain", "drain inbox pointers", Action::InboxDrain)
                },
                leaf("count", "count inbox pointers", Action::InboxCount),
            ],
        ),
        group(
            "gate",
            "manage decision gates",
            &[
                Command {
                    arguments: ID,
                    options: &[
                        required("--question <question>", "gate question"),
                        required("--options <options>", "gate options"),
                        required("--default <answer>", "default answer"),
                    ],
                    ..leaf("park", "park a decision gate", Action::GatePark)
                },
                leaf("list", "list open decision gates", Action::GateList),
                Command {
                    arguments: ID,
                    options: &[required("--answer <answer>", "chosen answer")],
                    ..leaf("resolve", "resolve a decision gate", Action::GateResolve)
                },
            ],
        ),
        group(
            "frontier",
            "manage the stack frontier",
            &[
                Command {
                    options: &[
                        Opt {
                            env: Some("ORCH_REPO"),
                            ..option(
                                "--repo <dir>",
                                "repository directory (or ORCH_REPO) (env: ORCH_REPO)",
                            )
                        },
                        Opt {
                            parser: Some(forge_choice),
                            ..option(
                                "--forge <forge>",
                                "pull request forge (choices: \"gh\", \"origin\", default: \"gh\")",
                            )
                        },
                        Opt {
                            parser: Some(pr_list),
                            ..option("--prs <n,...>", "optional expected pull request order pin")
                        },
                    ],
                    ..leaf(
                        "set",
                        "resolve the pull request base chain and set the frontier",
                        Action::FrontierSet,
                    )
                },
                leaf("show", "show the frontier", Action::FrontierShow),
            ],
        ),
        leaf(
            "status",
            "render status.md and print a summary",
            Action::Status,
        ),
        group(
            "standing",
            "manage standing orders",
            &[
                leaf("show", "show standing orders", Action::StandingShow),
                Command {
                    arguments: &[argument("line")],
                    ..leaf("add", "add a standing order", Action::StandingAdd)
                },
            ],
        ),
    ],
    action: Action::RequireSubcommand,
};

#[derive(Default)]
struct Io {
    stdout: String,
    stderr: String,
}

impl Io {
    /// Prints `value` as pretty JSON under `--json`, else the compact line, newline-terminated.
    fn emit(&mut self, json: bool, value: &impl Serialize, compact: String) -> OrchResult<()> {
        let rendered = if json {
            serde_json::to_string_pretty(value).map_err(std::io::Error::other)?
        } else {
            compact
        };
        self.stdout.push_str(&rendered);
        if !rendered.ends_with('\n') {
            self.stdout.push('\n');
        }
        Ok(())
    }
}

fn unit_line(unit: &Unit) -> String {
    unit.cells().join("\t")
}

fn pointer_line(pointer: &InboxPointer) -> String {
    pointer.cells().join("\t")
}

fn gate_line(gate: &Gate) -> String {
    [
        gate.id.as_str(),
        &gate.question,
        &gate.options,
        &gate.default_answer,
    ]
    .join("\t")
}

fn standing_line(item: &StandingLine) -> String {
    format!("{}. {}", item.number, item.line)
}

fn compact_rows<T>(
    rows: &[T],
    format: impl Fn(&T) -> String,
    empty: &str,
    limit: Option<usize>,
) -> String {
    if rows.is_empty() {
        return empty.to_owned();
    }
    let visible = limit.map_or(rows, |limit| &rows[..rows.len().min(limit)]);
    let mut lines: Vec<String> = visible.iter().map(format).collect();
    if let Some(limit) = limit.filter(|limit| rows.len() > *limit) {
        lines.push(format!("... {} more; use --json", rows.len() - limit));
    }
    lines.join("\n")
}

fn frontier_line(value: &Frontier) -> String {
    let prs = if value.prs.is_empty() {
        "none".to_owned()
    } else {
        value
            .prs
            .iter()
            .map(|row| {
                format!(
                    "{}#{}@{}:{}",
                    row.branches,
                    row.pr,
                    row.sha,
                    row.state.as_str()
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    let lowest = value
        .lowest_unmerged
        .map_or_else(|| "none".to_owned(), |pr| pr.to_string());
    format!(
        "generation={} prs={prs} lowest-unmerged={lowest}",
        value.generation
    )
}

fn status_lines(report: &StatusReport) -> String {
    let ids = &report.summary.open_gate_ids;
    let visible = &ids[..ids.len().min(DISPLAY_LIMIT)];
    let more = if ids.len() > DISPLAY_LIMIT {
        format!(",+{} more", ids.len() - DISPLAY_LIMIT)
    } else {
        String::new()
    };
    let listed = if visible.is_empty() {
        String::new()
    } else {
        format!("; ids={}{more}", visible.join(","))
    };
    [
        format!(
            "counts: units={}; states={}; ledger={}",
            report.units.len(),
            report.summary.unit_states.line(),
            report.summary.ledger_verdicts.line()
        ),
        format!("changed: {}", report.changed),
        format!("gates open: {}{listed}", ids.len()),
    ]
    .join("\n")
}

fn arg_text<A>(invocation: &Invocation<A>, index: usize) -> &str {
    match invocation.args.get(index) {
        Some(Value::Text(text)) => text,
        _ => "",
    }
}

fn arg_integer<A>(invocation: &Invocation<A>, index: usize) -> u64 {
    match invocation.args.get(index) {
        Some(Value::Integer(value)) => *value,
        _ => 0,
    }
}

fn store_directory(invocation: &Invocation<Action>) -> OrchResult<&str> {
    invocation
        .text(STORE)
        .filter(|store| !store.trim().is_empty())
        .ok_or_else(|| OrchError::Usage(STORE_REQUIRED.to_owned()))
}

fn frontier_repo(invocation: &Invocation<Action>) -> OrchResult<&str> {
    invocation
        .text(REPO)
        .filter(|repo| !repo.trim().is_empty())
        .ok_or_else(|| OrchError::Usage(REPO_REQUIRED.to_owned()))
}

fn takeover_notice(takeover: &LockTakeover) -> String {
    match takeover {
        LockTakeover::Stolen(holder) => format!("stealing store lock held by pid {holder}\n"),
        LockTakeover::Stale(holder) => {
            format!("replacing stale store lock (pid {holder} is dead)\n")
        }
    }
}

/// Opens the store, runs `operation`, and closes it, whose failure outranks the operation's.
fn with_store<T>(
    host: &dyn Host,
    invocation: &Invocation<Action>,
    io: &mut Io,
    operation: impl FnOnce(&mut Store<'_>) -> OrchResult<T>,
) -> OrchResult<T> {
    let directory = store_directory(invocation)?;
    let mut store = Store::open(host, directory, invocation.flag(FORCE), |takeover| {
        io.stderr.push_str(&takeover_notice(&takeover));
    });
    let result = operation(&mut store);
    store.close()?;
    result
}

fn execute(host: &dyn Host, invocation: &Invocation<Action>, io: &mut Io) -> OrchResult<()> {
    let json = invocation.flag(JSON);
    let text = |long: &str| invocation.text(long);
    match invocation.action {
        Action::RequireSubcommand => {
            store_directory(invocation)?;
            Err(OrchError::Usage(COMMAND_REQUIRED.to_owned()))
        }
        Action::Init => {
            let store = with_store(host, invocation, io, |store| store.init())?;
            io.emit(
                json,
                &json!({ "store": store }),
                format!("initialized {}", store.display()),
            )
        }
        Action::UnitAdd => {
            let unit = with_store(host, invocation, io, |store| {
                store.add_unit(
                    arg_text(invocation, 0),
                    text(TRACK).unwrap_or_default(),
                    text(BRIEF),
                )
            })?;
            io.emit(json, &unit, unit_line(&unit))
        }
        Action::UnitSet => {
            let unit = with_store(host, invocation, io, |store| {
                store.set_unit(&UnitUpdate {
                    id: arg_text(invocation, 0),
                    state: text(STATE).unwrap_or_default(),
                    branch: text(BRANCH),
                    pr: invocation.integer(PR),
                    sha: text(SHA),
                })
            })?;
            io.emit(json, &unit, unit_line(&unit))
        }
        Action::UnitGet => {
            let unit = with_store(host, invocation, io, |store| {
                store.unit(arg_text(invocation, 0))
            })?;
            io.emit(json, &unit, unit_line(&unit))
        }
        Action::UnitList => {
            let rows = with_store(host, invocation, io, |store| {
                store.units(text(STATE), text(TRACK))
            })?;
            let compact = compact_rows(&rows, unit_line, "(no units)", Some(DISPLAY_LIMIT));
            io.emit(json, &rows, compact)
        }
        Action::UnitCounts => emit_counts(host, invocation, io, |store| store.unit_counts()),
        Action::LedgerRecord => {
            let verdict = parse_verdict(arg_text(invocation, 2))?;
            let row = with_store(host, invocation, io, |store| {
                store.record(&LedgerRecord {
                    pr: arg_integer(invocation, 0),
                    sha: arg_text(invocation, 1),
                    verdict,
                    evidence: text(EVIDENCE).unwrap_or_default(),
                    verifier: text(VERIFIER),
                })
            })?;
            let compact = format!("{}\t{}\t{}", row.pr, row.sha, row.verdict);
            io.emit(json, &row, compact)
        }
        Action::LedgerCheck => {
            let row = with_store(host, invocation, io, |store| {
                store.check(arg_integer(invocation, 0), arg_text(invocation, 1))
            })?;
            io.emit(json, &row, row.verdict.to_owned())
        }
        Action::LedgerSummary => emit_counts(host, invocation, io, |store| store.ledger_summary()),
        Action::InboxPush => {
            let pushed = with_store(host, invocation, io, |store| {
                store.push(
                    arg_text(invocation, 0),
                    arg_text(invocation, 1),
                    arg_text(invocation, 2),
                    text(REPORT),
                )
            })?;
            let compact = format!(
                "{}\t{}\t{}",
                pushed.pointer.unit, pushed.pointer.status, pushed.filename
            );
            io.emit(json, &pushed.pointer, compact)
        }
        Action::InboxDrain => {
            let rows = with_store(host, invocation, io, |store| {
                if invocation.flag(PEEK) {
                    store.peek()
                } else {
                    store.drain()
                }
            })?;
            let compact = compact_rows(&rows, pointer_line, "(empty)", None);
            io.emit(json, &rows, compact)
        }
        Action::InboxCount => {
            let count = with_store(host, invocation, io, |store| store.inbox_count())?;
            io.emit(json, &json!({ "count": count }), count.to_string())
        }
        Action::GatePark => {
            let gate = with_store(host, invocation, io, |store| {
                store.park(&GatePark {
                    id: arg_text(invocation, 0),
                    question: text(QUESTION).unwrap_or_default(),
                    options: text(OPTIONS).unwrap_or_default(),
                    default_answer: text(DEFAULT).unwrap_or_default(),
                })
            })?;
            let compact = format!("{}\topen", gate.id);
            io.emit(json, &gate, compact)
        }
        Action::GateList => {
            let rows = with_store(host, invocation, io, |store| store.open_gates())?;
            let compact = compact_rows(&rows, gate_line, "(no open gates)", Some(DISPLAY_LIMIT));
            io.emit(json, &rows, compact)
        }
        Action::GateResolve => {
            let gate = with_store(host, invocation, io, |store| {
                store.resolve_gate(arg_text(invocation, 0), text(ANSWER).unwrap_or_default())
            })?;
            let compact = format!(
                "{}\tresolved\t{}",
                gate.id,
                gate.answer.as_deref().unwrap_or_default()
            );
            io.emit(json, &gate, compact)
        }
        Action::FrontierSet => {
            let frontier = with_store(host, invocation, io, |store| {
                store.set_frontier(
                    frontier_repo(invocation)?,
                    text(FORGE).unwrap_or(DEFAULT_FORGE),
                    invocation.integers(PRS),
                )
            })?;
            io.emit(json, &frontier, frontier_line(&frontier))
        }
        Action::FrontierShow => {
            let frontier = with_store(host, invocation, io, |store| store.frontier())?;
            io.emit(json, &frontier, frontier_line(&frontier))
        }
        Action::Status => {
            let report = with_store(host, invocation, io, |store| store.render_status())?;
            io.emit(json, &report, status_lines(&report))
        }
        Action::StandingShow => {
            let rows = with_store(host, invocation, io, |store| store.standing())?;
            let compact = compact_rows(
                &rows,
                standing_line,
                "(no standing orders)",
                Some(DISPLAY_LIMIT),
            );
            io.emit(json, &rows, compact)
        }
        Action::StandingAdd => {
            let item = with_store(host, invocation, io, |store| {
                store.add_standing(arg_text(invocation, 0))
            })?;
            io.emit(json, &item, standing_line(&item))
        }
    }
}

fn emit_counts(
    host: &dyn Host,
    invocation: &Invocation<Action>,
    io: &mut Io,
    operation: impl FnOnce(&mut Store<'_>) -> OrchResult<Counts>,
) -> OrchResult<()> {
    let counts = with_store(host, invocation, io, operation)?;
    io.emit(invocation.flag(JSON), &counts, counts.line())
}

/// Prints `error` as upstream's `handleError` does and returns the exit code.
fn handle_error(error: OrchError, json: bool, io: &mut Io) -> i32 {
    match error {
        OrchError::NotFound {
            output: Some(output),
            ..
        } => {
            if let Err(error) = io.emit(json, &output.json, output.compact) {
                io.stderr.push_str(&format!("error: {error}\n"));
            }
            2
        }
        OrchError::NotFound { message, .. } => {
            io.stderr.push_str(&format!("error: {message}\n"));
            2
        }
        OrchError::Usage(message) => {
            io.stderr.push_str(&format!("error: {message}\n"));
            io.stderr.push_str(&help(&[&PROGRAM]));
            1
        }
        other => {
            io.stderr.push_str(&format!("error: {other}\n"));
            1
        }
    }
}

pub fn run(host: &dyn Host, argv: &[String]) -> Output {
    let env = |name: &str| host.env_var(name);
    let invocation = match parse(&PROGRAM, argv, &env) {
        Ok(invocation) => invocation,
        Err(output) => return output,
    };
    let mut io = Io::default();
    let code = match execute(host, &invocation, &mut io) {
        Ok(()) => 0,
        Err(error) => handle_error(error, invocation.flag(JSON), &mut io),
    };
    Output {
        code,
        stdout: io.stdout,
        stderr: io.stderr,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::run;
    use crate::host::Output;
    use crate::host::fake::FakeHost;

    const PROGRAM_HELP: &str = "Usage: orch [--store <dir>] [--json] [--force] <command>

Plain-file orchestrate bookkeeping

Options:
  --store <dir>  store directory (or ORCH_STORE) (env: ORCH_STORE)
  --json         print complete rows as JSON (default: false)
  --force        steal an existing store lock (default: false)
  -h, --help     display help for command

Commands:
  init           initialize the store
  unit           manage work units
  ledger         manage verification records
  inbox          manage agent pointers
  gate           manage decision gates
  frontier       manage the stack frontier
  status         render status.md and print a summary
  standing       manage standing orders
";
    const FRONTIER_SET_HELP: &str = "Usage: orch frontier set [options]

resolve the pull request base chain and set the frontier

Options:
  --repo <dir>     repository directory (or ORCH_REPO) (env: ORCH_REPO)
  --forge <forge>  pull request forge (choices: \"gh\", \"origin\", default: \"gh\")
  --prs <n,...>    optional expected pull request order pin
  -h, --help       display help for command
";
    const LEDGER_RECORD_HELP: &str = "Usage: orch ledger record [options] <pr> <sha> <verdict>

record a verification verdict

Arguments:
  pr                 pull request number
  sha                commit SHA
  verdict            verification verdict

Options:
  --evidence <path>  evidence path
  --verifier <name>  verifier name
  -h, --help         display help for command
";
    const UNIT_HELP: &str = "Usage: orch unit [options] [command]

manage work units

Options:
  -h, --help          display help for command

Commands:
  add [options] <id>  add a unit
  set [options] <id>  update a unit
  get <id>            get a unit
  list [options]      list units
  counts              count units by state
";
    const UNIT_ADD_HELP: &str = "Usage: orch unit add [options] <id>

add a unit

Options:
  --track <track>  unit track
  --brief <path>   brief path
  -h, --help       display help for command
";
    const MISSING_TRACK: &str = "error: required option '--track <track>' not specified";
    const BAD_FORGE: &str =
        "error: option '--forge <forge>' argument 'gt' is invalid. Allowed choices are gh, origin.";
    const BAD_PR_ARGUMENT: &str = "error: command-argument value '0' is invalid for argument 'pr'. must be a positive integer";
    const BAD_PR_LIST: &str = "error: option '--prs <n,...>' argument '1,,2' is invalid. requires a comma-separated PR list";
    const MISTYPED_TRACK: &str = "error: unknown option '--trak'\n(Did you mean --track?)";
    const EXCESS_UNIT: &str =
        "error: too many arguments for 'unit'. Expected 0 arguments but got 1.";
    const EXCESS_PROGRAM: &str = "error: too many arguments. Expected 0 arguments but got 1.";
    const MISSING_ID: &str = "error: missing required argument 'id'";
    const STORE_MISSING: &str = "error: set --store <dir> or ORCH_STORE\n";
    const COMMAND_MISSING: &str = "error: a valid command is required\n";
    const REPO_MISSING: &str = "error: set --repo <dir> or ORCH_REPO\n";
    const BAD_VERDICT: &str = "error: verdict must be live-ui-verified, unit-test-verified, type-check-only, verifier-blocked, or verifier-failed\n";
    const EMPTY_ID: &str = "error: unit id must not be empty\n";
    const MISSING_UNIT: &str = "error: unit missing not found\n";

    fn orch(host: &FakeHost, args: &[&str]) -> Output {
        let argv: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        run(host, &argv)
    }

    fn with_store(store: &TempDir, args: &[&str]) -> Vec<String> {
        let mut argv = vec![
            "--store".to_owned(),
            store.path().to_string_lossy().into_owned(),
        ];
        argv.extend(args.iter().map(|arg| (*arg).to_owned()));
        argv
    }

    fn initialized() -> (TempDir, FakeHost) {
        let store = TempDir::new().expect("tempdir");
        let host = FakeHost::new(store.path());
        let init = run(&host, &with_store(&store, &["init"]));
        assert_eq!(init.code, 0, "{}", init.stderr);
        (store, host)
    }

    #[test_case(&["--help"], PROGRAM_HELP ; "program")]
    #[test_case(&["-h"], PROGRAM_HELP ; "program_short")]
    #[test_case(&["unit", "--help"], UNIT_HELP ; "group")]
    #[test_case(&["unit", "add", "--help"], UNIT_ADD_HELP ; "leaf")]
    #[test_case(&["frontier", "set", "--help"], FRONTIER_SET_HELP ; "choices_and_env")]
    #[test_case(&["ledger", "record", "--help"], LEDGER_RECORD_HELP ; "described_arguments")]
    fn prints_commander_help(args: &[&str], expected: &str) {
        let host = FakeHost::new("/work");
        assert_eq!(orch(&host, args), Output::success(expected));
    }

    #[test_case(&["unit", "add", "u1"], MISSING_TRACK, UNIT_ADD_HELP ; "missing_mandatory_option")]
    #[test_case(&["unit", "add", "--track", "x"], MISSING_ID, UNIT_ADD_HELP ; "missing_argument")]
    #[test_case(&["unit", "add", "u1", "--track", "x", "--trak"], MISTYPED_TRACK, UNIT_ADD_HELP ; "suggests_a_flag")]
    #[test_case(&["unit", "foo"], EXCESS_UNIT, UNIT_HELP ; "unknown_subcommand")]
    #[test_case(&["foo"], EXCESS_PROGRAM, PROGRAM_HELP ; "unknown_command")]
    #[test_case(&["frontier", "set", "--forge", "gt"], BAD_FORGE, FRONTIER_SET_HELP ; "bad_choice")]
    #[test_case(&["frontier", "set", "--prs", "1,,2"], BAD_PR_LIST, FRONTIER_SET_HELP ; "bad_pr_list")]
    #[test_case(&["ledger", "record", "0", "abc", "nope", "--evidence", "e"], BAD_PR_ARGUMENT, LEDGER_RECORD_HELP ; "bad_argument")]
    fn rejects_invalid_parsing_with_exit_1(args: &[&str], error: &str, help: &str) {
        let host = FakeHost::new("/work");
        assert_eq!(
            orch(&host, args),
            Output::failure(1, format!("{error}\n\n{help}"))
        );
    }

    #[test_case(&[], STORE_MISSING ; "no_store")]
    #[test_case(&["--store", "/s"], COMMAND_MISSING ; "no_command")]
    #[test_case(&["--store", "/s", "unit"], COMMAND_MISSING ; "no_subcommand")]
    fn usage_errors_print_the_program_help(args: &[&str], error: &str) {
        let host = FakeHost::new("/work");
        assert_eq!(
            orch(&host, args),
            Output::failure(1, format!("{error}{PROGRAM_HELP}"))
        );
    }

    #[test]
    fn accepts_orch_store_and_emits_complete_json() {
        let store = TempDir::new().expect("tempdir");
        let host = FakeHost::new("/work").with_env("ORCH_STORE", &store.path().to_string_lossy());
        assert_eq!(
            orch(&host, &["init"]),
            Output::success(format!("initialized {}\n", store.path().display()))
        );

        let added = orch(&host, &["unit", "add", "u1", "--track", "build", "--json"]);
        assert_eq!(added.code, 0);
        assert_eq!(
            serde_json::from_str::<Value>(&added.stdout).expect("json"),
            json!({ "id": "u1", "track": "build", "state": "pending", "branch": "", "pr": "", "sha": "", "brief": "" })
        );
        assert_eq!(
            added.stdout,
            "{\n  \"id\": \"u1\",\n  \"track\": \"build\",\n  \"state\": \"pending\",\n  \"branch\": \"\",\n  \"pr\": \"\",\n  \"sha\": \"\",\n  \"brief\": \"\"\n}\n"
        );
    }

    #[test]
    fn maps_user_and_not_found_outcomes_to_the_preserved_exit_codes() {
        let (store, host) = initialized();

        let missing_repo = run(&host, &with_store(&store, &["frontier", "set"]));
        assert_eq!(
            missing_repo,
            Output::failure(1, format!("{REPO_MISSING}{PROGRAM_HELP}"))
        );
        let empty_id = run(
            &host,
            &with_store(&store, &["unit", "add", "", "--track", "build"]),
        );
        assert_eq!(empty_id, Output::failure(1, EMPTY_ID));
        let missing_unit = run(&host, &with_store(&store, &["unit", "get", "missing"]));
        assert_eq!(missing_unit, Output::failure(2, MISSING_UNIT));
        let bad_verdict = run(
            &host,
            &with_store(
                &store,
                &["ledger", "record", "1", "abc", "nope", "--evidence", "e"],
            ),
        );
        assert_eq!(bad_verdict, Output::failure(1, BAD_VERDICT));

        let missing_ledger = run(
            &host,
            &with_store(&store, &["--json", "ledger", "check", "184530", "abc123"]),
        );
        assert_eq!(missing_ledger.code, 2);
        assert_eq!(missing_ledger.stderr, "");
        assert_eq!(
            serde_json::from_str::<Value>(&missing_ledger.stdout).expect("json"),
            json!({ "pr": "184530", "sha": "abc123", "verdict": "NOT-VERIFIED" })
        );
        let compact = run(
            &host,
            &with_store(&store, &["ledger", "check", "184530", "abc123"]),
        );
        assert_eq!(
            compact,
            Output {
                code: 2,
                stdout: "NOT-VERIFIED\n".to_owned(),
                stderr: String::new()
            }
        );
    }

    #[test]
    fn drives_the_playbook_drain_loop_with_compact_lines() {
        let (store, host) = initialized();
        let step = |args: &[&str]| {
            let output = run(&host, &with_store(&store, args));
            assert_eq!(output.code, 0, "{args:?}: {}", output.stderr);
            output.stdout
        };

        let pushed = step(&[
            "inbox", "push", "worker-1", "u1", "done", "--report", "r.md",
        ]);
        assert!(pushed.starts_with("u1\tdone\t1970-01-01T00-00-00-000Z-"));
        assert_eq!(step(&["inbox", "count"]), "1\n");
        assert_eq!(
            step(&["inbox", "drain"]),
            "1970-01-01T00:00:00.000Z\tworker-1\tu1\tdone\tr.md\n"
        );
        assert_eq!(step(&["inbox", "drain"]), "(empty)\n");
        assert_eq!(
            step(&["unit", "add", "u1", "--track", "build"]),
            "u1\tbuild\tpending\t\t\t\t\n"
        );
        assert_eq!(
            step(&[
                "unit", "set", "u1", "--state", "landed", "--pr", "7", "--sha", "abc"
            ]),
            "u1\tbuild\tlanded\t\t7\tabc\t\n"
        );
        assert_eq!(
            step(&[
                "ledger",
                "record",
                "7",
                "abc",
                "unit-test-verified",
                "--evidence",
                "e.md"
            ]),
            "7\tabc\tunit-test-verified\n"
        );
        assert_eq!(
            step(&["ledger", "check", "7", "abc"]),
            "unit-test-verified\n"
        );
        assert_eq!(
            step(&[
                "gate",
                "park",
                "ship",
                "--question",
                "Ship?",
                "--options",
                "y,n",
                "--default",
                "n"
            ]),
            "ship\topen\n"
        );
        assert_eq!(step(&["gate", "list"]), "ship\tShip?\ty,n\tn\n");
        assert_eq!(
            step(&["standing", "add", "Never force push."]),
            "1. Never force push.\n"
        );
        assert_eq!(step(&["standing", "show"]), "1. Never force push.\n");
        assert_eq!(
            step(&["frontier", "show"]),
            "generation=0 prs=none lowest-unmerged=none\n"
        );
        assert_eq!(
            step(&["status"]),
            "counts: units=1; states=landed=1; ledger=unit-test-verified=1\nchanged: first render\ngates open: 1; ids=ship\n"
        );
        assert_eq!(
            step(&["gate", "resolve", "ship", "--answer", "y"]),
            "ship\tresolved\ty\n"
        );
        assert_eq!(
            step(&["status"]),
            "counts: units=1; states=landed=1; ledger=unit-test-verified=1\nchanged: open gates 1->0\ngates open: 0\n"
        );
        assert!(
            !fs::read_to_string(store.path().join("status.md"))
                .expect("status")
                .is_empty()
        );
    }

    #[test]
    fn caps_compact_lists_and_points_at_json() {
        let (store, host) = initialized();
        for id in ["a", "b", "c", "d", "e", "f"] {
            assert_eq!(
                run(
                    &host,
                    &with_store(&store, &["unit", "add", id, "--track", "t"])
                )
                .code,
                0
            );
        }
        let listed = run(&host, &with_store(&store, &["unit", "list"]));
        assert_eq!(listed.stdout.lines().last(), Some("... 2 more; use --json"));
        let json = run(&host, &with_store(&store, &["--json", "unit", "list"]));
        assert_eq!(
            serde_json::from_str::<Vec<Value>>(&json.stdout)
                .expect("json")
                .len(),
            6
        );
    }
}
