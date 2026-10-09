use caudra_agent::template::Vars;
use caudra_agent::tools::profile_policy::PLAN_REQUIRED;
use caudra_agent::tools::{
    DescriptionContext, ToolAudience, ToolFilter, ToolRegistry, ToolSource, feature_exclusions,
};
use caudra_config::{Feature, FeatureFlags, PluginsConfig, ProfileToolPolicy};
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::sync::Arc;

use caudra_lua::{OptionType, PluginHost};

use crate::{page_header, repository_path};

const DATE_PLACEHOLDER: &str = "YYYY-MM-DD";
/// The reference documents `plan`, which is only offered with a session plan.
const WITH_SESSION_PLAN: bool = true;
const CODE_INTELLIGENCE: &str = "Code Intelligence";

const SECTIONS: &[(&str, &[&str])] = &[
    (
        "File Operations",
        &[
            "file_read",
            "file_write",
            "file_edit",
            "file_apply_patch",
            "file_index",
            "file_glob",
            "file_grep",
            "tool_output",
            "view_image",
        ],
    ),
    (
        CODE_INTELLIGENCE,
        &[
            "code_map",
            "code_context",
            "code_refs",
            "code_impact",
            "code_expand",
        ],
    ),
    (
        "Execution & Control",
        &[
            "batch",
            "shell",
            "python_execution",
            "execution_environment",
            "question",
        ],
    ),
    (
        "Agent & Knowledge",
        &[
            "task",
            "task_control",
            "workflow",
            "automation",
            "list_sessions",
            "send_message",
            "publish_message",
            "read_topic",
            "work_assignment",
            "todo_write",
            "plan",
            "memory",
            "skill",
        ],
    ),
    ("Media", &["image_generate"]),
    ("Web", &["webfetch", "websearch"]),
];

struct ToolInfo {
    def: Value,
    source: ToolSource,
}

/// Hand-written prose that belongs with the generated tool list. Kept here so
/// the page stays a single generated file.
fn write_disabling_section(out: &mut String) {
    let companions = code_list(caudra_config::INTERNAL_COMPANION_TOOL_NAMES);
    let (verb, pronoun) = match caudra_config::INTERNAL_COMPANION_TOOL_NAMES.len() {
        1 => ("stays", "it"),
        _ => ("stay", "them"),
    };
    writeln!(
        out,
        "\n## Disabling tools\n\n\
         `agent.disabled_tools` withholds a tool from the model. Entries are built-in tool names, \
         an MCP tool as `server.tool`, or a whole MCP server as `server.*`. An unknown name fails \
         at startup with the list of valid names. A project list extends the global one, so a \
         project can restrict further and cannot re-enable what the global config turned off.\n\n\
         ```toml\n\
         [agent]\n\
         disabled_tools = [\"shell\", \"file_write\", \"github.*\"]\n\
         ```\n\n\
         `--disallowed-tools` does the same for one run and accepts the same names. \
         `plugins.<name>.enabled = false` still works and maps to the tools that plugin was \
         replaced by, so `plugins.bash` turns off `shell`.\n\n\
         {companions} {verb} available whatever the lists say. The agent calls {pronoun} on its \
         own to page through a truncated result.\n\n\
         Run [`caudra tools`](/docs/cli/) to see the resulting set, including which rule turned \
         each tool off, or `/tools` inside a session for a \
         [mode-aware inventory](/docs/context/#inspect-the-active-window). To keep \
         a tool available but gate every call, use a `deny` or `prompt` default in \
         [Permissions](/docs/permissions/) instead.\n\n\
         [System prompt profiles](/docs/system-prompts/#choose-tool-availability) can make \
         eligible tools eager, lazy, or disabled for one actor. Pass \
         `--system-prompt-profile NAME` to `caudra tools` or `caudra prompt --tools` to inspect \
         that profile. A profile cannot re-enable tools excluded by config, CLI flags, \
         experimental feature gates, mode, or runtime requirements."
    )
    .unwrap();
}

fn code_list(names: &[&str]) -> String {
    let quoted: Vec<String> = names.iter().map(|name| format!("`{name}`")).collect();
    match quoted.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, [first])) => format!("{first} and {last}"),
        Some((last, rest)) => format!("{}, and {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// Derived from `DEFERRED_BUILTIN_TOOLS` so the page cannot drift from the set
/// the agent actually withholds.
fn write_on_demand_section(out: &mut String) {
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut ungrouped: Vec<&str> = Vec::new();
    for deferred in caudra_config::DEFERRED_BUILTIN_TOOLS {
        match deferred.group {
            Some(group) => match groups.iter_mut().find(|(name, _)| *name == group) {
                Some((_, members)) => members.push(deferred.name),
                None => groups.push((group, vec![deferred.name])),
            },
            None => ungrouped.push(deferred.name),
        }
    }
    let total = caudra_config::DEFERRED_BUILTIN_TOOLS.len();

    writeln!(
        out,
        "\n## Tools loaded on demand\n\n\
         The default loading policy lets {total} built-in tools start outside the request array. The model sees a \
         `tool_search` entry instead, and one call with a query loads the matching tools for the \
         rest of the session. Sessions that never need them never pay for their descriptions. \
         An explicit profile policy can make other native, local or remote Workcell, Lua/plugin, \
         local callback, or MCP tools lazy too. \
         A known-name direct call to an eligible lazy tool is valid and loads its schema. \
         `tool_search` disappears when no eligible pending tools remain."
    )
    .unwrap();
    for (group, members) in &groups {
        writeln!(
            out,
            "\n{} load together as the {group} bundle, limited to eligible lazy members. \
              Profile policy groups do not create additional loading bundles.",
            code_list(members)
        )
        .unwrap();
    }
    if !ungrouped.is_empty() {
        writeln!(out, "\n{} load on their own.", code_list(&ungrouped)).unwrap();
    }
    writeln!(
        out,
        "\nLoading changes the tool array, so the provider's prompt cache prefix resets and the \
         next request re-reads the history as fresh input. Caudra posts a notice naming what \
         loaded when it happens.\n\n\
         ### Which models defer\n\n\
         That cache reset is why deferral depends on model supply. Caudra defers for every model \
         recorded as small, whether marked **Small** or **Fast**, and for a model with no supply \
         facts. A known non-small model takes the eligible tools upfront because it would spend a large \
         prefix loading a tool it was likely to need.\n\n\
         Declare `fast` and `best` under `purposes` in `providers.toml` to describe model supply \
         ([Providers](/docs/providers/#supply-metadata)), or set `agent.defer_builtin_tools` to \
         `always` or `never` to decide for every model \
         ([Configuration](/docs/configuration/#agent)).\n\n\
         Listing a tool in `--allowed-tools` asks for it upfront and skips the search unless \
         the selected profile explicitly makes it lazy. \
         [`caudra tools`](/docs/cli/) marks a deferred tool `lazy` and reports whether the \
         choice came from the selected profile or the default loading policy."
    )
    .unwrap();
}

struct Param {
    name: String,
    ty: String,
    required: bool,
    default: String,
    description: String,
}

fn extract_default(desc: &str) -> (String, String) {
    for pattern in ["(default: ", "(default "] {
        if let Some(start) = desc.find(pattern) {
            let after = &desc[start + pattern.len()..];
            if let Some(end) = after.find(')') {
                let default_val = after[..end].to_string();
                let cleaned = format!(
                    "{}{}",
                    desc[..start].trim_end(),
                    &desc[start + pattern.len() + end + 1..]
                )
                .trim()
                .to_string();
                return (default_val, cleaned);
            }
        }
    }
    (String::new(), desc.to_string())
}

fn first_paragraph(desc: &str) -> &str {
    desc.split("\n\n").next().unwrap_or(desc)
}

fn extract_params(schema: &Value) -> Vec<Param> {
    let properties = match schema.get("properties").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let mut params = Vec::new();
    for (name, prop) in properties {
        let raw_type = prop
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("any (JSON)");
        let raw_desc = prop
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("");
        let is_required = required.contains(&name.as_str());
        let (default, description) = extract_default(raw_desc);
        params.push(Param {
            name: name.clone(),
            ty: raw_type.to_string(),
            required: is_required,
            default,
            description,
        });
    }
    params
}

fn write_param_table(out: &mut String, params: &[Param]) {
    let has_defaults = params.iter().any(|p| !p.default.is_empty());
    let header = if has_defaults {
        "| Parameter | Type | Required | Default | Description |\n|-----------|------|----------|---------|-------------|"
    } else {
        "| Parameter | Type | Required | Description |\n|-----------|------|----------|-------------|"
    };
    writeln!(out, "{header}").unwrap();
    for p in params {
        let desc = p.description.replace('|', "\\|").replace('\n', "<br>");
        let required = if p.required { "yes" } else { "no" };
        if has_defaults {
            writeln!(
                out,
                "| `{}` | {} | {} | {} | {} |",
                p.name, p.ty, required, p.default, desc
            )
            .unwrap();
        } else {
            writeln!(out, "| `{}` | {} | {} | {} |", p.name, p.ty, required, desc).unwrap();
        }
    }
}

/// The experiment that keeps `name` out of every catalog while it is off.
fn experiment(name: &str) -> Option<Feature> {
    Feature::ALL.into_iter().find(|&feature| {
        feature_exclusions(FeatureFlags::all().without(feature)).any(|excluded| excluded == name)
    })
}

fn write_tool_entry(out: &mut String, name: &str, info: &ToolInfo, opt_in: &HashSet<String>) {
    let description = info
        .def
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("");
    let schema = info.def.get("input_schema").cloned().unwrap_or(Value::Null);
    let params = extract_params(&schema);
    let summary = first_paragraph(description);
    let experiment = experiment(name);

    writeln!(out).unwrap();
    let mut badges = String::new();
    if matches!(info.source, ToolSource::Mcp { .. }) {
        badges.push_str(" <span class=\"badge\">mcp</span>");
    }
    if opt_in.contains(name) {
        badges.push_str(" <span class=\"badge badge-optin\">opt-in</span>");
    }
    if experiment.is_some() {
        badges.push_str(" <span class=\"badge\">experimental</span>");
    }
    if caudra_config::is_deferred_builtin(name) {
        badges.push_str(" <span class=\"badge\">on demand</span>");
    }
    writeln!(out, "### `{name}`{badges} {{#{name}}}").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "{summary}").unwrap();
    writeln!(out).unwrap();
    if let Some(feature) = experiment {
        writeln!(
            out,
            "Experimental and off by default. Turn it on with `{} = true` under `[experimental]` in the global `caudra.toml`. See [Experimental features](/docs/configuration/#experimental-features).",
            feature.key()
        )
        .unwrap();
        writeln!(out).unwrap();
    }
    for note in tool_notes(name) {
        writeln!(out, "{note}").unwrap();
        writeln!(out).unwrap();
    }
    write_param_table(out, &params);
}

/// Paragraphs written under a section heading, before its first tool.
fn section_notes(section: &str) -> &'static [&'static str] {
    match section {
        CODE_INTELLIGENCE => &[
            "The code tools and `file_index` read 37 languages and formats: Rust, Python, TypeScript, JavaScript, Gleam, Go, HTML, Java, C, C++, CUDA, Objective-C, C#, Ruby, PHP, Swift, Kotlin, Scala, Bash, Lua, Elixir, Markdown, Bazel/Starlark, Zig, Nix, Dart, TOML, YAML, SQL, CSS, JSON, HCL, Containerfile, Make, CMake, Protobuf, and XML.",
            "Every count and reach set is a lower bound. A call made through dynamic dispatch, a callback, or a macro adds no edge, so a count of zero means that none was found. A symbol name that matches nothing is refused with up to five close matches to try.",
        ],
        _ => &[],
    }
}

/// Paragraphs a tool's entry carries between its summary and its parameters.
fn tool_notes(name: &str) -> &'static [&'static str] {
    match name {
        "task" => &[
            "The published task arguments and instructions follow `agent.task_execution`: `sync` waits for final results and omits `background`, `auto` lets the model choose with `background: true`, and `async` always returns an admission receipt. See [background tasks](/docs/sessions/#background-tasks) for automatic continuation, inspection, and shutdown. The TUI and persistent stream-JSON SDK support background work. Print and ACP resolve `auto` to synchronous execution and withhold strict `async` tools. `batch` alone does not make synchronous calls asynchronous. Resume a task ID only after its invocation settles.",
        ],
        "file_edit" | "file_apply_patch" => &[
            "Each model is offered one editor. GPT-5 and later GPT models and the o3, o4, and Codex families get `file_apply_patch`, the patch format they were trained on. Every other model gets `file_edit`.",
        ],
        "file_glob" | "file_grep" => &[
            "A search that reaches its bounds returns what it found instead of failing. The result then reports how much was withheld, and the tool card says how far the scan got, so an absent match is distinguishable from an unsearched file.",
            "A directory search skips the `.git`, `.ssh`, and `.workcell` directories below it and these credential files, in any letter case: `.env` and `.env.*`, `.npmrc`, `.pypirc`, `.netrc`, files ending in `.key`, and the SSH private keys `id_rsa`, `id_dsa`, `id_ecdsa`, and `id_ed25519`. Naming one of these files by its path still reaches it, subject to [permissions](/docs/permissions/).",
        ],
        "view_image" => &["Only models that accept images are offered `view_image`."],
        "shell" => &[
            "`agent.shell_execution` selects `sync`, `auto`, or `async` independently of task execution. In `auto`, a validated requested timeout above `agent.shell_async_threshold_secs` returns an admission receipt. The default threshold is 120 seconds. This is not elapsed-time promotion and never extends the hard execution deadline. Shell has no per-call `background` argument. See [execution policies](/docs/sessions/#execution-policies) for frontend support and child-owned command results.",
            "Caudra shows unfiltered output while the command runs. After completion, the TUI switches to the filtered model-facing result when Workcell reduced it. The output footer names every reduction that ran and toggles between filtered and raw views. Filtering is enabled by default and never changes the reviewed command or structured capture. Set `agent.shell_output_filter = false` or use `--no-rtk` to disable it.",
            "A rule that would report success applies only when the command exited zero, so a failure is never shown as a success. When a failing command reaches a rule's line cap, the result keeps its first and last lines. Filtering never makes a result larger than the raw output, and a filtered result ends with a `[filtered: …]` line that names the stages which changed it. Most rules come from [RTK](https://github.com/rtk-ai/rtk), credited in Workcell's notice file.",
            "A progress bar redraws a row instead of printing lines. Caudra renders both the live view and the capture as a terminal would show them, so a bar appears as one updating row rather than a single very long line, and the output printed before it is not pushed out of the retained window. Rendering is decoding rather than filtering, so `--no-rtk` does not disable it. The footer reports how many frames were absorbed.",
            "Commands start from a cleared environment. [Shell host configuration](/docs/cli/#shell-host-configuration) lists the variables they receive.",
        ],
        "python_execution" => &[
            "Scripts run in a separate worker process with no file system, network, environment variables, or subprocesses, under time, memory, and recursion limits. Host clocks, unseeded randomness, and sleep are refused. Because that isolation fixes what a script can reach, the default permission policy allows the tool without a prompt.",
            "Release builds include the isolated Monty worker. `WORKCELL_MCP_CODE_WORKER` can override it with an operator-supplied worker binary.",
        ],
        "webfetch" => &[
            "Every URL is checked before a connection opens. A private, loopback, link-local, or carrier-grade NAT address is refused, including an IPv6 form that maps to one, and so is a special-use name such as `localhost`, `.local`, or `home.arpa`. Every address a name resolves to must pass, and the connection goes only to those checked addresses. Up to 5 redirects are followed, each checked the same way, and a redirect to another origin carries only the `Accept`, `Accept-Language`, `Cache-Control`, `Pragma`, `Range`, and `User-Agent` headers. With a proxy configured, the proxy resolves the name and the URL checks still run locally.",
            "Text is decoded in the character set the response declares through a byte-order mark, the `Content-Type` header, or an HTML `<meta>` tag, and as UTF-8 otherwise. Up to 5 MiB of a response is read, and the model receives at most 2,000 lines or 50 KiB. Cut text ends with a line that names the limit, such as `[truncated: showing 1999 of 2105 lines]`.",
            "With the default `pdfMode` of `extract`, a PDF of up to 6 MiB and 200 pages arrives as its text. There is no OCR, so a scanned PDF yields little text. With `pdfMode` set to `attachment`, a model that reads PDFs receives the file itself inside the tool result. That covers Claude through an Anthropic API key, a Claude login, or Bedrock, and a custom model on an `anthropic` or `openai-responses` provider that sets [`supports_pdf`](/docs/providers/#model-fields). The PDFs in one request may use a quarter of the context window, counted at 4,500 tokens a page and capped at 100 pages, and an older PDF that no longer fits is replaced by a note that names it. A PDF over that budget, and every PDF for a model that does not read them, arrives as extracted text whose first line says why. Saved sessions keep only a PDF's URL, name, and page count. See [Fetched PDFs](/docs/sessions/#fetched-pdfs).",
        ],
        _ => &[],
    }
}

/// Replace `target` with `placeholder`. Empty `target` is a no-op.
fn redact_path(input: &str, target: &str, placeholder: &str) -> String {
    if target.is_empty() {
        input.to_string()
    } else {
        input.replace(target, placeholder)
    }
}

/// Scrubs registration-time paths and dates. Use the registry's fixed root, not
/// the process cwd: running from `/` must not replace every slash in the docs.
fn redact_env_and_dates(input: &str) -> String {
    let root = repository_path("");
    let mut out = redact_path(input, &root.to_string_lossy(), "<cwd>");
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        out = redact_path(&out, &home, "~");
    }
    DATE_RE.replace_all(&out, DATE_PLACEHOLDER).into_owned()
}

fn redact_def(def: &Value) -> Value {
    let Some(d) = def.get("description").and_then(|v| v.as_str()) else {
        return def.clone();
    };
    let redacted = redact_env_and_dates(d);
    if redacted == d {
        def.clone()
    } else {
        let mut out = def.clone();
        out["description"] = Value::String(redacted);
        out
    }
}

fn collect_tool_info(
    def_map: &HashMap<String, &Value>,
    entry: &caudra_agent::tools::RegisteredTool,
) -> Option<ToolInfo> {
    let name = entry.name();
    let def = def_map.get(name)?;
    Some(ToolInfo {
        def: redact_def(def),
        source: entry.source.clone(),
    })
}

/// Loads every builtin with all sub-tools on, so the reference documents
/// opt-in tools too. "Opt-in" means the plugin declares the tool as a boolean
/// option defaulting to false, so the badge cannot drift from the defaults.
fn load_registry_with_builtins() -> (Arc<ToolRegistry>, HashSet<String>) {
    let registry = Arc::new(ToolRegistry::new());
    let workcell =
        caudra_workcell::WorkcellHost::new(repository_path(""), None).expect("Workcell host");
    workcell
        .register_documented_tools(&registry)
        .expect("Workcell tools");
    caudra_agent::tools::native::register(&registry, FeatureFlags::all())
        .expect("native Caudra tools");
    let mut host = PluginHost::new(Arc::clone(&registry)).expect("plugin host");

    host.load_production_builtins(&PluginsConfig::from_plugins(HashMap::new()))
        .expect("loading builtin plugins");

    let opt_in = host
        .plugin_options()
        .expect("collecting plugin options")
        .into_values()
        .flatten()
        .filter(|o| o.ty == OptionType::Boolean && o.default == Some(Value::Bool(false)))
        .map(|o| o.name)
        .collect();
    (registry, opt_in)
}

pub fn generate() -> String {
    let vars = Vars::new()
        .set("{cwd}", "<cwd>")
        .set("{platform}", "linux")
        .set("{date}", "YYYY-MM-DD");

    let (registry, opt_in) = load_registry_with_builtins();
    let defs = registry.definitions_split_with_policy(
        &vars,
        &DescriptionContext {
            filter: &ToolFilter::All,
            audience: ToolAudience::MAIN,
            workflows_available: false,
        },
        false,
        &[],
        &ProfileToolPolicy::default(),
        WITH_SESSION_PLAN,
    );
    let def_map: HashMap<String, &Value> = defs
        .declared
        .as_array()
        .expect("definitions should be an array")
        .iter()
        .filter_map(|t| {
            t.get("name")
                .and_then(|n| n.as_str())
                .map(|n| (n.to_string(), t))
        })
        .collect();

    let snapshot = registry.iter();
    let mut tools: HashMap<&str, ToolInfo> = HashMap::new();
    for entry in snapshot.iter() {
        if let Some(info) = collect_tool_info(&def_map, entry) {
            tools.insert(entry.name(), info);
        }
    }

    let total = tools.len();
    let mut out = page_header("Tools", "Every built-in tool and its parameters.");
    let opt_in_n = tools.keys().filter(|n| opt_in.contains(**n)).count();
    let default_n = total - opt_in_n;
    writeln!(
        out,
        "Caudra ships with {total} built-in tools in this reference \
         ({default_n} requiring no plugin opt-in, {opt_in_n} opt-in via plugin options). \
         Availability depends on the selected workspace backend. \
         Tools marked **opt-in** are off until you enable them under `plugins` \
         in [Configuration](/docs/configuration/)."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "First-party file, web, shell, index, Python, and environment tools run through protocol-neutral Workcell contracts. Workcell owns schemas, validation, execution bounds, atomic file changes, network policy, subprocess cleanup, cancellation, and the bundled worker lifecycle. Caudra owns registration, authorization, retained session output, and model or UI presentation. Release builds pin an exact Workcell revision."
    )
    .unwrap();
    writeln!(
        out,
        "\nRemote Workcell selection replaces the first-party execution backend. Startup requires the complete compatible catalog and workspace capabilities, even when a tool is disabled for the model. A missing or incompatible remote tool never falls back to local execution. Direct Workcell connections are experimental and require `experimental.remote_workcell = true` and a server compatible with Caudra's pinned Workcell contracts. A matching version label alone does not establish compatibility. See [Remote Workspaces](/docs/remote-workspaces/)."
    )
    .unwrap();
    writeln!(out, "\nThe single `plan` tool reads or replaces this session's plan in local and remote workspaces. Use `{{\"action\":\"read\"}}` to read it or `{{\"action\":\"write\",\"content\":\"Complete plan document\"}}` to replace it. It accepts no path, reference, or session selector and has no patch, approval, or mode-switch action. A session gets its plan the first time it enters Plan. The plan survives Implement and every mode switch, and returning to Plan revises the same document. The main agent can read and replace it in Plan and Build, subject to the selected profile and [permissions](/docs/permissions/#plan-mode). Tasks and other subagents, in the foreground or background, can only read it. An agent a workflow starts while the session is in Plan can read it too. One started in Build gets no plan. Without a session plan, `/tools` and `caudra tools` list `plan` as off with the reason `{PLAN_REQUIRED}`.").unwrap();
    writeln!(out, "\nImplement and Clear-and-Implement capture the validated content before they switch to Build or clear the session. The model-visible Build request opens with \"Implement the plan from `<path>`.\" for a local plan or \"Implement this session's plan.\" for a remote one, followed by the content, so implementation does not need a tool call to read the plan. A capture failure leaves the plan available and does not start implementation.").unwrap();
    writeln!(out, "\nClear-and-Implement moves the plan to the new session, and the old session keeps none. A local plan keeps its path. A remote plan is copied into a document the new session owns. If that copy fails, implementation still starts from the captured content and Caudra shows a warning.").unwrap();
    writeln!(out, "\nSecure plan storage currently requires a Unix client. Windows and other non-Unix clients return `UnsupportedPlatform` for secure plan storage operations. This applies to local plans and client-owned plans for remote workspaces, regardless of the Workcell server's platform.").unwrap();
    writeln!(out, "\nThe `memory` tool reads, writes, and deletes named notes in local and remote workspaces. Its `view`, `zoom`, and `search` commands find the notes worth reading, as [Memory](/docs/memory/#zoom-and-search) describes. Writes replace the complete note. Remote notes stay on the client and cannot be edited through remote file tools. Workbench saves retain revision-conflict checks.").unwrap();
    write_disabling_section(&mut out);
    write_on_demand_section(&mut out);

    let mut rendered: HashSet<&str> = HashSet::new();

    for (section_name, tool_names) in SECTIONS {
        let present: Vec<&str> = tool_names
            .iter()
            .copied()
            .filter(|n| tools.contains_key(*n))
            .collect();
        if present.is_empty() {
            continue;
        }
        writeln!(out).unwrap();
        writeln!(out, "## {section_name}").unwrap();
        for note in section_notes(section_name) {
            writeln!(out).unwrap();
            writeln!(out, "{note}").unwrap();
        }
        for name in present {
            let info = tools.get(name).expect("checked above");
            write_tool_entry(&mut out, name, info, &opt_in);
            rendered.insert(name);
        }
    }

    let mut leftovers: Vec<&str> = tools
        .keys()
        .filter(|n| !rendered.contains(*n))
        .copied()
        .collect();
    leftovers.sort_unstable();
    if !leftovers.is_empty() {
        writeln!(out).unwrap();
        writeln!(out, "## Additional tools").unwrap();
        for name in leftovers {
            let info = tools.get(name).expect("checked above");
            write_tool_entry(&mut out, name, info, &opt_in);
        }
    }

    if out.ends_with('\n') {
        out.pop();
    }
    out
}

static DATE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"\d{4}-\d{2}-\d{2}").expect("valid date regex"));

#[cfg(test)]
mod tests {
    use super::{
        DATE_PLACEHOLDER, DATE_RE, PLAN_REQUIRED, SECTIONS, extract_default, extract_params,
        generate, load_registry_with_builtins, redact_path,
    };
    use serde_json::{Value, json};
    use std::collections::HashSet;
    use test_case::test_case;

    #[test_case(json!({}), "any (JSON)"; "unconstrained_json")]
    #[test_case(json!({"type": "object"}), "object"; "object")]
    #[test_case(json!({"type": "string"}), "string"; "string")]
    #[test_case(json!({"type": "boolean"}), "boolean"; "boolean")]
    fn extracts_parameter_type(mut property: Value, expected: &str) {
        const DESCRIPTION: &str = "JSON Schema (object)";
        property["description"] = json!(DESCRIPTION);
        let schema = json!({"properties": {"output_schema": property, "other": property}});
        let params = extract_params(&schema);
        assert_eq!(params.len(), 2);
        for param in params {
            assert_eq!(param.ty, expected);
            assert_eq!(param.description, DESCRIPTION);
        }
    }

    #[test]
    fn task_reference_preserves_async_guidance_and_schema_contract() {
        const TASK_HEADING: &str = "### `task` {#task}";
        const EXPECTED: &[&str] = &[
            "Delegate a bounded task to an autonomous subagent",
            "`agent.task_execution`",
            "`sync` waits for final results and omits `background`",
            "`async` always returns an admission receipt",
            "[background tasks](/docs/sessions/#background-tasks)",
            "Print and ACP resolve `auto` to synchronous execution",
            "`batch` alone does not make synchronous calls asynchronous.",
            "Resume a task ID only after its invocation settles.",
            "| `output_schema` | any (JSON) | no | JSON Schema (object)",
        ];
        let page = generate();
        let task = page
            .split_once(TASK_HEADING)
            .expect("task reference")
            .1
            .split("\n### ")
            .next()
            .unwrap();
        for expected in EXPECTED {
            assert!(task.contains(expected), "missing task guidance: {expected}");
        }
    }

    #[test]
    fn plan_reference_preserves_session_plan_and_named_memory() {
        const PLAN_HEADING: &str = "### `plan`";
        const PLAN_ANCHOR: &str = " {#plan}";
        const EXPECTED: &[&str] = &[
            "no path, reference, or session selector",
            "returning to Plan revises the same document",
            "The main agent can read and replace it in Plan and Build",
            "Tasks and other subagents, in the foreground or background, can only read it.",
            PLAN_REQUIRED,
            "model-visible Build request",
            "\"Implement this session's plan.\"",
            "A capture failure leaves the plan available",
            "Clear-and-Implement moves the plan to the new session",
            "Windows and other non-Unix clients return `UnsupportedPlatform`",
            "The `memory` tool reads, writes, and deletes named notes",
            "Workbench saves retain revision-conflict checks",
            "A known-name direct call to an eligible lazy tool is valid",
        ];
        let page = generate();
        assert!(
            page.lines()
                .any(|line| line.starts_with(PLAN_HEADING) && line.ends_with(PLAN_ANCHOR))
        );
        for expected in EXPECTED {
            assert!(
                page.contains(expected),
                "missing plan or document contract: {expected}"
            );
        }
        assert!(!page.contains("local_document_"));
        assert!(!page.contains("## Local Documents"));
    }

    #[test_case("2026-07-05", "YYYY-MM-DD"; "simple date")]
    #[test_case("today is 2026-07-05 here", "today is YYYY-MM-DD here"; "embedded date")]
    #[test_case("v2024-01-15", "vYYYY-MM-DD"; "prefix embedded")]
    #[test_case("2026-7-5", "2026-7-5"; "single digit not matched")]
    #[test_case("no date here", "no date here"; "no date")]
    #[test_case("2026-07-05 and 2025-12-31", "YYYY-MM-DD and YYYY-MM-DD"; "two dates")]
    fn redacts_date(input: &str, expected: &str) {
        // redact_env_and_dates also scrubs HOME/cwd; isolate date logic by
        // calling the regex replacement directly.
        assert_eq!(
            DATE_RE.replace_all(input, DATE_PLACEHOLDER).as_ref(),
            expected
        );
    }

    #[test_case("/home/user/repo", "/home/user", "~", "~/repo"; "path under home")]
    #[test_case("/home/user", "/home/user", "~", "~"; "exact home")]
    #[test_case("/elsewhere", "/home/user", "~", "/elsewhere"; "unrelated path")]
    #[test_case("any", "", "<cwd>", "any"; "empty target no-op")]
    #[test_case("", "/home/user", "~", ""; "empty input")]
    fn redacts_path(input: &str, target: &str, placeholder: &str, expected: &str) {
        assert_eq!(redact_path(input, target, placeholder), expected);
    }

    #[test_case("$(default: foo)", "foo", "$"; "colon form")]
    #[test_case("desc (default bar) suffix", "bar", "desc suffix"; "space form")]
    #[test_case("prefix (default: baz) extra", "baz", "prefix extra"; "in middle")]
    #[test_case("plain description", "", "plain description"; "no default")]
    fn extracts_default(input: &str, expected_default: &str, remaining_prefix: &str) {
        let (default, cleaned) = extract_default(input);
        assert_eq!(default, expected_default);
        assert!(cleaned.starts_with(remaining_prefix), "cleaned: {cleaned}");
    }

    #[test]
    fn sections_partition_registered_tools() {
        let (registry, _) = load_registry_with_builtins();
        let snapshot = registry.iter();
        let registered: HashSet<&str> = snapshot.iter().map(|e| e.name()).collect();

        let mut sectioned: HashSet<&str> = HashSet::new();
        for (_, names) in SECTIONS {
            for &n in *names {
                assert!(
                    registered.contains(n),
                    "SECTIONS references \"{n}\" which isn't a registered tool"
                );
                assert!(sectioned.insert(n), "\"{n}\" appears in multiple sections");
            }
        }

        let unsectioned: Vec<&str> = registered.difference(&sectioned).copied().collect();
        assert!(
            unsectioned.is_empty(),
            "registered tools missing from SECTIONS: {unsectioned:?}"
        );
    }
}
