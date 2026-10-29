//! Modules backed by a registered tool: `image`, `host`, `workflow` and `automation`. A member call
//! becomes that tool's input, and the gateway runs it through tool dispatch, so the tool keeps its
//! own permission rules, mode checks, change records, ledger name and card.

use serde_json::{Map, Value, json};

use super::args::bind;
use super::module::{HostModule, MemberDoc, ModuleDoc, Prepared, ToolCallPlan};
use super::names::{AUTOMATION_MODULE, HOST_MODULE, IMAGE_MODULE, WORKFLOW_MODULE};
use super::output::CodeCall;
use super::runner::{ScriptCall, ScriptError, ScriptErrorKind};
use crate::automation::handle::AutomationHandle;
use crate::tools::native::{automation, workflow};
use crate::tools::{
    AUTOMATION_TOOL_NAME, EXECUTION_ENVIRONMENT_TOOL_NAME, IMAGE_GENERATE_TOOL_NAME, ToolContext,
    WORKFLOW_TOOL_NAME,
};
use crate::types::ToolOutput;

const ACTION_FIELD: &str = "action";
const PATH_FIELD: &str = "path";
const TEXT_FIELD: &str = "text";
const IMAGE_UNAVAILABLE: &str = "image is unavailable: it needs a ChatGPT subscription login (`caudra auth login openai`) and the `images` tool group, which the profile or `disabled_tools` may turn off";
const HOST_UNAVAILABLE: &str = "host is unavailable: the profile or `disabled_tools` turns off the `execution` tool group, or this session's Workcell does not offer it";
const WORKFLOW_DISABLED: &str = "workflow is unavailable: the profile or `disabled_tools` turns off the `delegation` tool group";
const AUTOMATION_DISABLED: &str = "automation is unavailable: the profile or `disabled_tools` turns off the `delegation` tool group";

/// One member of a tool-backed module: the tool's `action` it stands for, if the tool has several,
/// and its parameters in positional order, each passed as the tool's field of the same name.
struct ToolMethod {
    member: &'static str,
    action: Option<&'static str>,
    params: &'static [&'static str],
}

struct ToolBacking {
    doc: &'static ModuleDoc,
    tool: &'static str,
    methods: &'static [ToolMethod],
}

const fn action(member: &'static str, params: &'static [&'static str]) -> ToolMethod {
    ToolMethod {
        member,
        action: Some(member),
        params,
    }
}

static IMAGE: ToolBacking = ToolBacking {
    doc: &IMAGE_DOC,
    tool: IMAGE_GENERATE_TOOL_NAME,
    methods: &[ToolMethod {
        member: "generate",
        action: None,
        params: &["prompt", "out", "quality", "size", "images"],
    }],
};

static HOST: ToolBacking = ToolBacking {
    doc: &HOST_DOC,
    tool: EXECUTION_ENVIRONMENT_TOOL_NAME,
    methods: &[ToolMethod {
        member: "environment",
        action: None,
        params: &[],
    }],
};

static WORKFLOW: ToolBacking = ToolBacking {
    doc: &WORKFLOW_DOC,
    tool: WORKFLOW_TOOL_NAME,
    methods: &[
        action("list", &[]),
        action("validate", &["name"]),
        action("start", &["name", "args", "agent_budget"]),
        action("status", &["run_id"]),
        action("inspect", &["run_id"]),
        action("history", &["limit"]),
        action("pause", &["run_id"]),
        action("resume", &["run_id", "agent_budget"]),
        action("stop", &["run_id"]),
    ],
};

static AUTOMATION: ToolBacking = ToolBacking {
    doc: &AUTOMATION_DOC,
    tool: AUTOMATION_TOOL_NAME,
    methods: &[
        action("list", &[]),
        action("validate", &["name"]),
        action("history", &["name", "fire_id", "limit"]),
        action("arm", &["name", "args"]),
        action("disarm", &["name"]),
        action("delete", &["name"]),
    ],
};

pub static IMAGE_DOC: ModuleDoc = ModuleDoc {
    namespace: IMAGE_MODULE,
    summary: "`image`: generate a raster image with a ChatGPT subscription; `image.generate`.",
    page: "image: raster images generated through the ChatGPT subscription login (`caudra auth login openai`), billed against that plan. Prefer SVG, HTML/CSS or canvas when the asset can be written directly.\n\
Example: `saved = await image.generate(\"a pixel-art fox, transparent background\", \"assets/fox.png\", size=\"1024x1024\")`, then `view_image` on `saved[\"path\"]` to see it.",
    members: &[MemberDoc {
        name: "generate",
        signature: r#"generate(prompt: str, out: str, quality: Literal["low", "medium", "high", "xhigh", "max", "auto"] | None = None, size: str | None = None, images: list[str] | None = None) -> dict[str, Any]"#,
        awaited: true,
        read_first: true,
        page: "Saves one PNG and returns `{\"path\", \"text\"}`: where it was saved and the tool's note. It never overwrites: a local file that exists gets a versioned name, so read `path`.\n\
- `out` is relative to the project directory unless absolute.\n\
- `size` is `auto` or `WIDTHxHEIGHT`, both multiples of 16, the long edge at most 3840, at most 3:1, between 655,360 and 8,294,400 pixels.\n\
- `images` are reference images, described in prompt order; name their roles in `prompt` (\"Image 1: style reference\").\n\
- Asks under `image_generate`'s own permission rules. One image per call; `asyncio.gather` several calls for several assets.",
    }],
};

pub static HOST_DOC: ModuleDoc = ModuleDoc {
    namespace: HOST_MODULE,
    summary: "`host`: what the machine commands run on offers (OS, runtimes, sandboxing, commands, versions); `host.environment`.",
    page: "host: the machine `shell` commands run on, local or remote, as this session's Workcell reports it. It never describes the machine Caudra runs on when commands run elsewhere.\n\
Example: `env = await host.environment()`, then `[c[\"id\"] for c in env[\"commands\"] if c[\"available\"]]`.",
    members: &[MemberDoc {
        name: "environment",
        signature: "environment() -> dict[str, Any]",
        awaited: true,
        read_first: false,
        page: "Returns `{\"headline\", \"summary\", \"facts\", \"commands\"}`: `headline` names the OS, `summary` what commands run in and may reach, `facts` maps labels such as `workspace` to values, and `commands` lists `{\"id\", \"available\", \"version\"}` for a fixed set of tools. Takes a fresh snapshot each call, so call it again when something may have changed.",
    }],
};

pub static WORKFLOW_DOC: ModuleDoc = ModuleDoc {
    namespace: WORKFLOW_MODULE,
    summary: "`workflow`: durable multi-agent workflows, scripted plans that launch subagents in phases; `workflow.list`, `.start`, `.status` and more.",
    page: "workflow: durable, multi-agent workflows: scripted plans that launch subagents in phases, keep a journal, and can be paused and resumed. A run outlives the call that started it, so every call answers at once; read progress back with `workflow.status`. A run's completion is also reported to you later.\n\
Answers are the runtime's own records as dicts (`status` is `active`, `paused`, `budget_limited`, `interrupted`, `completed`, `cancelled` or `failed`).\n\
Polling example, with `timeoutSec` raised to cover the wait:\n\
```python\n\
import asyncio\n\
run = await workflow.start(\"review\", {\"objective\": \"review the open PR\"})\n\
while (run := await workflow.status(run[\"run_id\"]))[\"status\"] == \"active\":\n\
    await asyncio.sleep(10)\n\
run[\"status\"], run.get(\"result\")\n\
```",
    members: &[
        MemberDoc {
            name: "list",
            signature: "list() -> dict[str, Any]",
            awaited: true,
            read_first: false,
            page: "Returns `{\"entries\", \"invalid\", \"project_dir\", \"user_dir\"}`: every workflow this session can launch, with its source, trust state and description, invalid scripts with their error, and the directories scripts go in. `project_dir` is missing without a project root.",
        },
        MemberDoc {
            name: "validate",
            signature: "validate(name: str) -> dict[str, Any]",
            awaited: true,
            read_first: false,
            page: "Parses, compiles and smoke-runs `name` against a canned host without launching agents. Returns `{\"name\", \"ok\", \"report\"}`.",
        },
        MemberDoc {
            name: "start",
            signature: "start(name: str, args: dict[str, Any] | None = None, agent_budget: int | None = None) -> dict[str, Any]",
            awaited: true,
            read_first: true,
            page: "Launches `name` and returns the run's snapshot at once, `run_id` included; the run continues in the background. The script reads `args` as `args`; `objective` or `query` becomes the run's objective. `agent_budget` caps the agents it may launch. Only trusted definitions start: built-in and user scripts are, a project script must be approved in `/workflows` first.",
        },
        MemberDoc {
            name: "status",
            signature: "status(run_id: str | None = None) -> Any",
            awaited: true,
            read_first: false,
            page: "One run's snapshot as a dict, or a list of every run of this session when `run_id` is omitted: current phase, phases so far, agent roster, usage, and once it ended its `result` or `error`.",
        },
        MemberDoc {
            name: "inspect",
            signature: "inspect(run_id: str) -> dict[str, Any]",
            awaited: true,
            read_first: false,
            page: "One run in full, from any session: its snapshot, every journaled host call with timings and a bounded result or error, and its timeline of phases and log lines.",
        },
        MemberDoc {
            name: "history",
            signature: "history(limit: int | None = None) -> list[dict[str, Any]]",
            awaited: true,
            read_first: false,
            page: "Recent runs of earlier sessions, newest first, each with the title of the session that ran it. View only: a run resumes only in the session that started it.",
        },
        MemberDoc {
            name: "pause",
            signature: "pause(run_id: str) -> Any",
            awaited: true,
            read_first: false,
            page: "Ends the current attempt of an active run and keeps it resumable.",
        },
        MemberDoc {
            name: "resume",
            signature: "resume(run_id: str, agent_budget: int | None = None) -> Any",
            awaited: true,
            read_first: false,
            page: "Continues a paused, failed, cancelled or budget-limited run from its journal. A budget-limited run needs a higher `agent_budget`.",
        },
        MemberDoc {
            name: "stop",
            signature: "stop(run_id: str) -> Any",
            awaited: true,
            read_first: false,
            page: "Ends an active run for good.",
        },
    ],
};

pub static AUTOMATION_DOC: ModuleDoc = ModuleDoc {
    namespace: AUTOMATION_MODULE,
    summary: "`automation`: inspect and manage this session's automations (Rhai scripts reacting to session events); `automation.list`, `.validate`, `.history`, `.arm`, `.disarm`, `.delete`.",
    page: "automation: this session's automations, short Rhai scripts that react to events such as the session going idle, a goal finishing or a schedule coming due. Load the `caudra-automation-dev` skill before writing or debugging one. Reads never prompt. Arm, disarm and delete require permission unless a rule, YOLO or autopilot approves. They are unavailable in Plan mode. Approval to arm also trusts an untrusted project script's exact reviewed digest. Tell the user what you armed.\n\
Answers are the runtime's own records as dicts.",
    members: &[
        MemberDoc {
            name: "list",
            signature: "list() -> dict[str, Any]",
            awaited: true,
            read_first: false,
            page: "Returns `{\"automations\", \"session_dir\", \"project_dir\", \"user_dir\"}`. Each automation has its scope (`session`, `project`, `user`, or `bundled`), availability, status, triggers, args and warnings. Files that cannot load include the reason. The directories are the resolved locations for scripts. `session_dir` is absent when tools run remotely. Bundled scripts ship with Caudra through an enabled provider such as pstack.",
        },
        MemberDoc {
            name: "validate",
            signature: "validate(name: str) -> dict[str, Any]",
            awaited: true,
            read_first: false,
            page: "Parses the header of `name`, compiles the script, checks the args its body reads, and runs the body once per trigger against a canned event, performing nothing. Returns `{\"name\", \"ok\", \"report\"}`.",
        },
        MemberDoc {
            name: "history",
            signature: "history(name: str | None = None, fire_id: str | None = None, limit: int | None = None) -> Any",
            awaited: true,
            read_first: false,
            page: "This session's newest firings with their status, only `name`'s when given, at most `limit` (20 by default, at most 50). With `fire_id`, one firing's event and actions, log lines included, with its error and state patch.",
        },
        MemberDoc {
            name: "arm",
            signature: "arm(name: str, args: dict[str, Any] | None = None) -> dict[str, Any]",
            awaited: true,
            read_first: false,
            page: "Arms or re-arms a validated script in this session. Omit args to reuse stored args or defaults. The permission prompt shows its scope, path, digest, triggers, args and capabilities. Approval also trusts an untrusted project script's reviewed digest. A changed script is refused. Returns its automation snapshot.",
        },
        MemberDoc {
            name: "disarm",
            signature: "disarm(name: str) -> dict[str, Any]",
            awaited: true,
            read_first: false,
            page: "Disarms the automation in this session and drops queued firings and deliveries, after permission approval. Returns its automation snapshot.",
        },
        MemberDoc {
            name: "delete",
            signature: "delete(name: str) -> Any",
            awaited: true,
            read_first: false,
            page: "Disarms the automation in this session, removes its project or user script file, and rescans, after permission approval. Refuses bundled scripts and symlinks. Other open sessions keep their bindings.",
        },
    ],
};

/// A tool-backed module as one run sees it: the tool's availability is taken when the run starts.
pub struct ToolModule {
    backing: &'static ToolBacking,
    unavailable: Option<&'static str>,
}

impl ToolModule {
    pub fn image(ctx: &ToolContext) -> Self {
        Self::of(ctx, &IMAGE, IMAGE_UNAVAILABLE, true)
    }

    pub fn host(ctx: &ToolContext) -> Self {
        Self::of(ctx, &HOST, HOST_UNAVAILABLE, true)
    }

    pub fn workflow(ctx: &ToolContext) -> Self {
        match ctx.workflow.is_some() {
            true => Self::of(ctx, &WORKFLOW, WORKFLOW_DISABLED, true),
            false => Self::of(ctx, &WORKFLOW, workflow::UNAVAILABLE, false),
        }
    }

    pub fn automation(ctx: &ToolContext) -> Self {
        let runtime = ctx
            .session_id
            .as_ref()
            .is_some_and(|session| AutomationHandle::lookup(session.id()).is_some());
        match runtime {
            true => Self::of(ctx, &AUTOMATION, AUTOMATION_DISABLED, true),
            false => Self::of(ctx, &AUTOMATION, automation::UNAVAILABLE, false),
        }
    }

    fn of(
        ctx: &ToolContext,
        backing: &'static ToolBacking,
        reason: &'static str,
        runtime: bool,
    ) -> Self {
        let available = runtime && ctx.tool_available(backing.tool);
        Self {
            backing,
            unavailable: (!available).then_some(reason),
        }
    }
}

impl HostModule for ToolModule {
    fn doc(&self) -> &'static ModuleDoc {
        self.backing.doc
    }

    fn unavailable(&self) -> Option<String> {
        self.unavailable.map(str::to_owned)
    }

    fn prepare(&self, _ctx: &ToolContext, call: ScriptCall) -> Prepared {
        let doc = self.backing.doc;
        let Some(method) = self
            .backing
            .methods
            .iter()
            .find(|method| method.member == call.name)
        else {
            return Prepared::Plan(Err(ScriptError::new(
                ScriptErrorKind::Attribute,
                doc.no_member(&call.name),
            )));
        };
        let called_as = format!("{}.{}", doc.namespace, method.member);
        let arguments = match bind(&called_as, method.params, &call) {
            Ok(arguments) => arguments,
            Err(error) => return Prepared::Plan(Err(error)),
        };
        let mut input: Map<String, Value> = arguments
            .iter()
            .filter(|(_, value)| !value.is_null())
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        if let Some(action) = method.action {
            input.insert(ACTION_FIELD.to_owned(), Value::String(action.to_owned()));
        }
        Prepared::Tool(Box::new(ToolCallPlan {
            row: CodeCall {
                server: doc.namespace.to_owned(),
                tool: method.member.to_owned(),
                called_as,
                input: Value::Object(arguments),
                ..CodeCall::default()
            },
            tool: self.backing.tool,
            input: Value::Object(input),
        }))
    }
}

/// What a snippet receives for a hosted tool's card when the tool offered nothing richer: the
/// card's own fields for the outputs that have them.
pub fn output_value(output: &ToolOutput) -> Option<Value> {
    match output {
        ToolOutput::SavedImage { path, text } => Some(json!({PATH_FIELD: path, TEXT_FIELD: text})),
        ToolOutput::Environment {
            headline,
            summary,
            facts,
            commands,
        } => Some(json!({
            "headline": headline,
            "summary": summary,
            "facts": facts
                .iter()
                .map(|fact| (fact.label.clone(), Value::String(fact.value.clone())))
                .collect::<Map<String, Value>>(),
            "commands": commands,
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;

    /// Every member a module documents is a call it makes, and each tool it makes calls of is
    /// one of the tools only modules reach.
    #[test_case(&IMAGE ; "image")]
    #[test_case(&HOST ; "host")]
    #[test_case(&WORKFLOW ; "workflow")]
    #[test_case(&AUTOMATION ; "automation")]
    fn every_documented_member_is_a_tool_call(backing: &ToolBacking) {
        let documented: Vec<&str> = backing.doc.member_names().collect();
        let methods: Vec<&str> = backing.methods.iter().map(|method| method.member).collect();
        assert_eq!(documented, methods);
        assert!(caudra_config::CODE_HOSTED_TOOLS.contains(&backing.tool));
    }

    #[test]
    fn a_call_becomes_the_tools_input_with_its_action() {
        let module = ToolModule {
            backing: &WORKFLOW,
            unavailable: None,
        };
        let call = ScriptCall {
            namespace: Some(WORKFLOW_MODULE.to_owned()),
            name: "start".to_owned(),
            args: vec![json!("review")],
            kwargs: [("agent_budget".to_owned(), json!(3))]
                .into_iter()
                .collect(),
        };
        let ctx = stub_ctx(&AgentMode::Build);
        let Prepared::Tool(plan) = module.prepare(&ctx, call) else {
            panic!("a workflow call runs the workflow tool");
        };
        assert_eq!(plan.tool, WORKFLOW_TOOL_NAME);
        assert_eq!(
            plan.input,
            json!({"action": "start", "name": "review", "agent_budget": 3})
        );
        assert_eq!(plan.row.called_as, "workflow.start");
        assert_eq!(plan.row.input, json!({"name": "review", "agent_budget": 3}));
    }

    #[test]
    fn a_saved_image_reads_as_its_path() {
        let output = ToolOutput::SavedImage {
            path: "a.png".to_owned(),
            text: "saved".to_owned(),
        };
        assert_eq!(
            output_value(&output),
            Some(json!({"path": "a.png", "text": "saved"}))
        );
    }
}
