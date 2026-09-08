use caudra_agent::template::Vars;
use caudra_agent::tools::{DescriptionContext, ToolAudience, ToolFilter, ToolRegistry, ToolSource};
use caudra_config::PluginsConfig;
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::sync::Arc;

use caudra_lua::{OptionType, PluginHost};

const DATE_PLACEHOLDER: &str = "YYYY-MM-DD";

const SECTIONS: &[(&str, &[&str])] = &[
    (
        "File Operations",
        &[
            "file_read",
            "file_write",
            "file_edit",
            "file_apply_patch",
            "index",
            "file_glob",
            "file_grep",
            "tool_output",
            "view_image",
        ],
    ),
    (
        "Code Intelligence",
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
        &["task", "todo_write", "memory", "skill"],
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
         ```lua\n\
         caudra.setup({{\n    \
             agent = {{ disabled_tools = {{ \"shell\", \"file_write\", \"github.*\" }} }},\n\
         }})\n\
         ```\n\n\
         `--disallowed-tools` does the same for one run and accepts the same names. \
         `plugins.<name>.enabled = false` still works and maps to the tools that plugin was \
         replaced by, so `plugins.bash` turns off `shell`.\n\n\
         {companions} {verb} available whatever the lists say. The agent calls {pronoun} on its \
         own to page through a truncated result.\n\n\
         Run [`caudra tools`](/docs/cli/) to see the resulting set, including which rule turned \
         each tool off. To keep a tool available but gate every call, use a `deny` or `prompt` \
         default in [Permissions](/docs/permissions/) instead."
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
         {total} built-in tools start outside the request array. The model sees a `tool_search` \
         entry instead, and one call with a query loads the matching tools for the rest of the \
         session. Sessions that never need them never pay for their descriptions."
    )
    .unwrap();
    for (group, members) in &groups {
        writeln!(
            out,
            "\n{} load together as the {group} group, because a question about an unfamiliar \
             codebase usually takes several of them in a row.",
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
         Listing a tool in `--allowed-tools` asks for it upfront and skips the search. \
         [`caudra tools`](/docs/cli/) marks the rest as `deferred behind tool_search`."
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
            .unwrap_or("string");
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

fn write_tool_entry(out: &mut String, name: &str, info: &ToolInfo, opt_in: &HashSet<String>) {
    let description = info
        .def
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("");
    let schema = info.def.get("input_schema").cloned().unwrap_or(Value::Null);
    let params = extract_params(&schema);
    let summary = first_paragraph(description);

    writeln!(out).unwrap();
    let mut badges = String::new();
    if matches!(info.source, ToolSource::Mcp { .. }) {
        badges.push_str(" <span class=\"badge\">mcp</span>");
    }
    if opt_in.contains(name) {
        badges.push_str(" <span class=\"badge badge-optin\">opt-in</span>");
    }
    if caudra_config::is_deferred_builtin(name) {
        badges.push_str(" <span class=\"badge\">on demand</span>");
    }
    writeln!(out, "### `{name}`{badges} {{#{name}}}").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "{summary}").unwrap();
    writeln!(out).unwrap();
    if name == "python_execution" {
        writeln!(
            out,
            "Release builds include the isolated Monty worker. `WORKCELL_MCP_CODE_WORKER` can override it with an operator-supplied worker binary."
        )
        .unwrap();
        writeln!(out).unwrap();
    }
    if name == "shell" {
        writeln!(
            out,
            "Caudra shows unfiltered output while the command runs. After completion, the TUI switches to the filtered model-facing result when Workcell reduced it. The output footer names every reduction that ran and toggles between filtered and raw views. Filtering is enabled by default and never changes the reviewed command or structured capture. Set `agent.shell_output_filter = false` or use `--no-rtk` to disable it."
        )
        .unwrap();
        writeln!(out).unwrap();
        writeln!(
            out,
            "A progress bar redraws a row instead of printing lines. Caudra renders both the live view and the capture as a terminal would show them, so a bar appears as one updating row rather than a single very long line, and the output printed before it is not pushed out of the retained window. Rendering is decoding rather than filtering, so `--no-rtk` does not disable it; the footer reports how many frames were absorbed."
        )
        .unwrap();
        writeln!(out).unwrap();
    }
    if name == "file_glob" || name == "file_grep" {
        writeln!(
            out,
            "A search that reaches its bounds returns what it found instead of failing. The result then reports how much was withheld, and the tool card says how far the scan got, so an absent match is distinguishable from an unsearched file."
        )
        .unwrap();
        writeln!(out).unwrap();
    }
    write_param_table(out, &params);
}

/// Replace `target` with `placeholder`. Empty `target` is a no-op.
fn redact_path(input: &str, target: &str, placeholder: &str) -> String {
    if target.is_empty() {
        input.to_string()
    } else {
        input.replace(target, placeholder)
    }
}

/// Plugins bake env-specific values into their `description` at registration:
/// `bash` interpolates `caudra.uv.cwd()` and `websearch` interpolates
/// `os.date("%Y-%m-%d")`. Scrub both so `gen-docs-check` is stable across
/// machines and days. CWD is replaced before HOME so a cwd nested under ~
/// doesn't get partially mangled.
fn redact_env_and_dates(input: &str) -> String {
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|c| c.to_str().map(str::to_owned))
        .unwrap_or_default();
    let mut out = redact_path(input, &cwd, "<cwd>");
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

fn write_front_matter(out: &mut String) {
    writeln!(out, "+++").unwrap();
    writeln!(out, "title = \"Tools\"").unwrap();
    writeln!(out, "weight = 4").unwrap();
    writeln!(out, "[extra]").unwrap();
    writeln!(out, "group = \"Reference\"").unwrap();
    writeln!(out, "+++").unwrap();
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
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let workcell = caudra_workcell::WorkcellHost::new(cwd, None).expect("Workcell host");
    workcell
        .register_documented_tools(&registry)
        .expect("Workcell tools");
    caudra_agent::tools::native::register(&registry).expect("native Caudra tools");
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
    let defs = registry.definitions(
        &vars,
        &DescriptionContext {
            filter: &ToolFilter::All,
            audience: ToolAudience::MAIN,
            workflow: false,
        },
        false,
    );
    let def_map: HashMap<String, &Value> = defs
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
    let mut out = String::new();
    write_front_matter(&mut out);
    writeln!(out).unwrap();
    writeln!(out, "# Tools").unwrap();
    writeln!(out).unwrap();
    let opt_in_n = tools.keys().filter(|n| opt_in.contains(**n)).count();
    let default_n = total - opt_in_n;
    writeln!(
        out,
        "Caudra ships with {total} built-in tools in this reference \
         ({default_n} on by default, {opt_in_n} opt-in via plugin options). \
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
    use super::*;
    use test_case::test_case;

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
