use std::fmt::Write;
use std::sync::Arc;

use caudra_agent::tools::ToolRegistry;
use caudra_config::{
    AgentConfig, ConfigField, DEFAULT_MAX_LOG_FILES, DEFAULT_MAX_OUTPUT_LINES,
    DEFAULT_MOUSE_SCROLL_LINES, MIN_TOOL_OUTPUT_LINES, ProviderConfig, RetentionConfig,
    StorageConfig, TOP_LEVEL_FIELDS, TelemetryConfig, ToolOutputLines, UiConfig,
};
use caudra_lua::{OptionSpec, OptionType, PluginHost, PluginOptionSpecs};

const PLUGIN_DEV_DESC: &str =
    "Offer the builtin caudra-plugin-dev skill for writing caudra plugins.";
const MAX_CONCURRENT_DESC: &str = "Max concurrently running subagents.";

type ExtraColumn = (&'static str, fn(&ConfigField) -> String);

fn write_table(out: &mut String, fields: &[ConfigField]) {
    let mut extras: Vec<ExtraColumn> = Vec::new();
    if fields.iter().any(|f| f.env.is_some()) {
        extras.push(("Env", |f: &ConfigField| {
            f.env.map_or("-".to_string(), |e| {
                e.split(", ")
                    .map(|v| format!("`{v}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
        }));
    }
    if fields.iter().any(|f| f.min.is_some()) {
        extras.push(("Min", |f: &ConfigField| {
            f.min.map_or("-".to_string(), |v| v.to_string())
        }));
    }

    let header: String = extras
        .iter()
        .map(|(name, _)| format!(" {name} |"))
        .collect();
    let rule: String = extras.iter().map(|_| "-----|").collect();
    writeln!(out, "| Field | Type | Default |{header} Description |").unwrap();
    writeln!(out, "|-------|------|---------|{rule}-------------|").unwrap();
    for f in fields {
        let cells: String = extras
            .iter()
            .map(|(_, cell)| format!(" {} |", cell(f)))
            .collect();
        writeln!(
            out,
            "| `{name}` | {ty} | `{default}` |{cells} {desc} |",
            name = f.name,
            ty = escape_pipes(f.ty),
            default = f.default.format_default(),
            desc = f.description,
        )
        .unwrap();
    }
}

fn escape_pipes(ty: &str) -> String {
    ty.replace('|', "\\|")
}

fn lua_section_name(heading: &str) -> String {
    heading
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string()
}

fn write_section(out: &mut String, heading: &str, fields: &[ConfigField]) {
    let lua_name = lua_section_name(heading);
    writeln!(out, "### `{lua_name}`\n").unwrap();
    write_table(out, fields);
    writeln!(out).unwrap();
}

/// These tables outlived the Lua plugins they were written for: the tool is
/// native now and Rust validates the same keys.
fn native_tool_note(plugin: &str) -> Option<String> {
    match plugin {
        "index" => Some(format!(
            "`index` executes as a native Workcell tool. This table keeps its existing configuration keys. The file-size limit accepts {} through {} MiB to bound parser memory and work.",
            caudra_config::MIN_INDEX_MAX_FILE_SIZE_MB,
            caudra_config::MAX_INDEX_MAX_FILE_SIZE_MB,
        )),
        "skill" | "task" => Some(format!(
            "`{plugin}` executes as a native Caudra tool. This table keeps its existing configuration key."
        )),
        _ => None,
    }
}

fn write_plugin_options(out: &mut String, specs: &PluginOptionSpecs) {
    for (plugin, options) in specs {
        writeln!(out, "### `plugins.{plugin}`\n").unwrap();
        if let Some(note) = native_tool_note(plugin) {
            writeln!(out, "{note}\n").unwrap();
        }
        writeln!(out, "| Field | Type | Default | Min | Description |").unwrap();
        writeln!(out, "|-------|------|---------|-----|-------------|").unwrap();
        for o in options {
            let default = o
                .default
                .as_ref()
                .map_or("-".to_string(), |d| format!("`{d}`"));
            let min = o.min.map_or("-".to_string(), |m| m.to_string());
            writeln!(
                out,
                "| `{name}` | {ty} | {default} | {min} | {desc} |",
                name = o.name,
                ty = o.ty,
                desc = o.desc,
            )
            .unwrap();
        }
        writeln!(out).unwrap();
    }
}

fn collect_plugin_options() -> PluginOptionSpecs {
    let registry = Arc::new(ToolRegistry::new());
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let workcell = caudra_workcell::WorkcellHost::new(cwd, None).expect("Workcell host");
    workcell
        .register_documented_tools(&registry)
        .expect("Workcell tools");
    let mut host = PluginHost::new(registry).expect("plugin host");
    host.load_production_builtins(&caudra_config::PluginsConfig::from_plugins(
        std::collections::HashMap::new(),
    ))
    .expect("loading builtins");
    let mut specs = host.plugin_options().expect("collecting plugin options");
    specs.insert(
        "index".into(),
        vec![OptionSpec {
            name: "max_file_size_mb".into(),
            ty: OptionType::Integer,
            default: Some(serde_json::json!(
                caudra_config::DEFAULT_INDEX_MAX_FILE_SIZE_MB
            )),
            min: Some(caudra_config::MIN_INDEX_MAX_FILE_SIZE_MB as f64),
            desc: format!(
                "Refuse to index files larger than this many MiB (maximum {}).",
                caudra_config::MAX_INDEX_MAX_FILE_SIZE_MB
            ),
        }],
    );
    specs.insert(
        "task".into(),
        vec![OptionSpec {
            name: "max_concurrent".into(),
            ty: OptionType::Integer,
            default: Some(serde_json::json!(
                caudra_config::DEFAULT_TASK_MAX_CONCURRENT
            )),
            min: Some(caudra_config::MIN_TASK_MAX_CONCURRENT as f64),
            desc: MAX_CONCURRENT_DESC.into(),
        }],
    );
    specs.insert(
        "skill".into(),
        vec![OptionSpec {
            name: "plugin_dev".into(),
            ty: OptionType::Boolean,
            default: Some(serde_json::json!(caudra_config::DEFAULT_SKILL_PLUGIN_DEV)),
            min: None,
            desc: PLUGIN_DEV_DESC.into(),
        }],
    );
    assert!(
        !specs.is_empty(),
        "no plugin declared options; the plugins reference would be empty"
    );
    specs
}

fn write_theme_section(out: &mut String) {
    writeln!(out, "### `ui.theme`\n").unwrap();
    writeln!(
        out,
        "Name of the color theme to load at startup, overriding the theme you \
         last picked interactively. If unset, Caudra keeps your last selection, \
         which starts out as `{}`. An unknown name is ignored with a warning.\n",
        caudra_ui::DEFAULT_THEME
    )
    .unwrap();
    let names = caudra_ui::BUNDLED_THEMES
        .iter()
        .map(|t| format!("`{}`", t.name))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(out, "Available themes: {names}.\n").unwrap();
    writeln!(
        out,
        "You can add your own themes too. Drop a `<name>.toml` file into \
         `themes/` inside your Caudra config directory, for example \
         `~/.config/caudra/themes/`. If it reuses a built-in name, yours wins.\n"
    )
    .unwrap();
    writeln!(
        out,
        "Themes use 24-bit colors, but not every terminal can show them. Caudra \
         checks the environment, terminfo, and the terminal itself, and when \
         truecolor is missing it quietly falls back to the closest of the 256 \
         classic terminal colors. If detection gets it wrong, set \
         `CAUDRA_TRUECOLOR=1` to force truecolor or `CAUDRA_TRUECOLOR=0` to force \
         the fallback.\n"
    )
    .unwrap();

    let pairs = caudra_ui::THEME_PAIRS
        .iter()
        .map(|pair| format!("`{}` and `{}`", pair.dark, pair.light))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(
        out,
        "Some themes ship as a light and dark pair: {pairs}. Choosing either \
         half makes Caudra ask the terminal for its background color and show \
         the half that matches. Themes outside these pairs stay as you left \
         them.\n"
    )
    .unwrap();
    writeln!(
        out,
        "Caudra asks again every ten minutes, and also when the terminal \
         regains focus or changes size, so reattaching a multiplexer to \
         another terminal updates the theme. Following the terminal only \
         changes the running session, and the theme you saved from `/theme` \
         stays saved. Terminals that do not report a background color are \
         asked a few times and then left alone.\n"
    )
    .unwrap();

    writeln!(out, "### `ui.theme_light`\n").unwrap();
    writeln!(
        out,
        "Light half to use in place of the one from the pairing table, or to \
         give a theme that has no pair. `ui.theme` becomes the dark half:\n"
    )
    .unwrap();
    writeln!(
        out,
        "```lua\ncaudra.setup({{ ui = {{ theme = \"tokyonight\", theme_light = \
         \"catppuccin_latte\" }} }})\n```\n"
    )
    .unwrap();
    writeln!(
        out,
        "Leave `ui.theme` unset to pair the light theme with whatever you last \
         picked from `/theme`.\n"
    )
    .unwrap();
}

fn write_update_check_section(out: &mut String) {
    writeln!(out, "### `ui.update_check`\n").unwrap();
    writeln!(
        out,
        "When on, Caudra asks the GitHub releases API for the latest version \
         once at startup and shows it in the splash when yours is older. The \
         request carries a `caudra` user agent and nothing else: no session id, \
         no machine id, not even your current version.\n"
    )
    .unwrap();
    writeln!(
        out,
        "It is off by default, so a normal run reaches only the model \
         provider you configured. Set `CAUDRA_ENABLE_UPDATE_CHECK=1` to turn it \
         on for a single run, or `CAUDRA_ENABLE_UPDATE_CHECK=0` to turn it off \
         when your config has it on. The `caudra update` command always \
         checks, because that is what you asked it to do.\n"
    )
    .unwrap();
}

fn write_retention_section(out: &mut String) {
    write_section(out, "[storage.retention]", RetentionConfig::FIELDS);
    writeln!(
        out,
        "`trim` and `forget` are keep policies in `restic forget` terms: `keep_last`, \
         `keep_hourly`, `keep_daily`, `keep_weekly`, `keep_monthly`, `keep_yearly` take a \
         count, and `keep_within` plus `keep_within_hourly` through `keep_within_yearly` \
         take a duration such as `\"90d\"` or `\"2y5m7d3h\"`. A session is kept when any \
         rule matches. An empty `forget` policy disables automatic deletion. See \
         [Sessions](/docs/sessions/#retention) for what each tier keeps and how the sweep \
         runs.\n"
    )
    .unwrap();
}

fn write_telemetry_section(out: &mut String) {
    write_section(out, "[telemetry]", TelemetryConfig::FIELDS);
    writeln!(
        out,
        "Every field also has an environment variable, shown in the Env \
         column, and the variable wins. See [Telemetry](/docs/telemetry/) \
         for the full picture.\n"
    )
    .unwrap();
}

fn write_tool_output_section(out: &mut String) {
    writeln!(out, "### `ui.tool_output_lines`\n").unwrap();
    writeln!(
        out,
        "How many lines of output to show per tool in the UI. \
         All values are `usize` with a minimum of {MIN_TOOL_OUTPUT_LINES}.\n"
    )
    .unwrap();
    writeln!(out, "| Field | Default | Tools |").unwrap();
    writeln!(out, "|-------|---------|-------|").unwrap();
    for (name, default) in ToolOutputLines::FIELD_DEFAULTS {
        let tools = ToolOutputLines::FIELD_TOOLS
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, tools)| {
                tools
                    .iter()
                    .map(|tool| format!("`{tool}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        writeln!(out, "| `{name}` | {default} | {tools} |").unwrap();
    }
    writeln!(out).unwrap();
}

pub fn generate() -> String {
    let mut out = String::with_capacity(4096);

    writeln!(
        out,
        "\
+++
title = \"Configuration\"
weight = 2
[extra]
group = \"Getting Started\"
+++

# Configuration

Settings go in `init.lua`, a Lua script that calls `caudra.setup()`. Same language as plugins.

Two places, both optional:

- **Global**: `~/.config/caudra/init.lua`
- **Project**: `.caudra/init.lua` (relative to your working directory)

When both exist, project settings override global ones. Neither file is required.

## Example

```lua
caudra.setup({{
    ui = {{
        splash_animation = true,
        mouse_scroll_lines = {mouse_scroll},
        theme = \"tokyonight\",
        tool_output_lines = {{
            bash = {tol_bash},
            read = {tol_read},
        }},
    }},
    agent = {{
        max_output_lines = {max_output_lines},
    }},
    provider = {{
        default_model = \"anthropic/claude-sonnet-4-6\",
        allowed_models = {{ \"anthropic/*\", \"openai/gpt-5\" }},
        excluded_models = {{ \"*/*-preview\" }},
    }},

    storage = {{
        max_log_files = {max_log_files},
    }},
    plugins = {{
        bash = {{ timeout_secs = 180 }},
        index = {{ max_file_size_mb = 4 }},
    }},
}})
```

All fields are optional. Typos in field names cause an error right away.

`provider.allowed_models` is a list of glob patterns for qualified `provider/model-id` specs. `*` also matches `/`, so `opencode/*` includes nested model IDs. When the list is empty or omitted, every model is allowed. `provider.excluded_models` removes matching models after that, so exclusions always win. A project list replaces the matching global list; omit it to inherit or use `{{}}` to clear it. The policy applies to selectors, CLI and API model changes, delegation, and `caudra models`.

`caudra.setup()` can only be called once per init.lua.

## Full Reference
",
        mouse_scroll = DEFAULT_MOUSE_SCROLL_LINES + 2,
        tol_bash = ToolOutputLines::DEFAULT.bash + 3,
        tol_read = ToolOutputLines::DEFAULT.read + 2,
        max_output_lines = DEFAULT_MAX_OUTPUT_LINES + 1000,
        max_log_files = DEFAULT_MAX_LOG_FILES / 2,
    )
    .unwrap();

    writeln!(out, "### Top-level\n").unwrap();
    write_table(&mut out, TOP_LEVEL_FIELDS);
    writeln!(out).unwrap();

    write_section(&mut out, "[ui]", UiConfig::FIELDS);
    write_theme_section(&mut out);
    write_update_check_section(&mut out);
    write_tool_output_section(&mut out);
    write_section(&mut out, "[agent]", AgentConfig::FIELDS);
    write_section(&mut out, "[provider]", ProviderConfig::FIELDS);
    write_section(&mut out, "[storage]", StorageConfig::FIELDS);
    write_retention_section(&mut out);
    write_telemetry_section(&mut out);

    writeln!(out, "## Plugins\n").unwrap();
    writeln!(
        out,
        "The `plugins` table turns bundled features and plugins on or off and passes options to \
         them. All bundled features are on by default. Set \
         `enabled = false` to turn one off.\n\n\
         Each feature checks its own options at startup. A typo, a wrong \
         type, or an unknown plugin name gives you a clear error right \
         away.\n\n\
         The edit plugin's extra tools are options too: \
         `plugins.edit = {{ multiedit = false, insert_lines = true }}`. \
         The old `tools` table is gone. If your config still uses it, \
         Caudra stops at startup and shows you the new form.\n\n\
         This table is for bundled plugins only. Your own plugins go in \
         `~/.config/caudra/lua/`, see [Plugins](/docs/plugins/).\n"
    )
    .unwrap();
    writeln!(
        out,
        "\
```lua
caudra.setup({{
    plugins = {{
        bash = {{ timeout_secs = 180 }},
        websearch = {{ enabled = false }},
    }},
}})
```\n"
    )
    .unwrap();

    write_plugin_options(&mut out, &collect_plugin_options());

    writeln!(out, "## Validation\n").unwrap();
    writeln!(
        out,
        "If a value is below its minimum, Caudra shows a `ConfigError` with the field name, \
         value, and minimum."
    )
    .unwrap();

    writeln!(
        out,
        "
## Directory layout

Caudra follows platform directory conventions. On Linux and macOS that is XDG. On Windows, config, data, state, and logs all live under Roaming AppData (Windows has no separate state dir in this layout).

| Purpose | Linux / macOS | Windows |
|---------|---------------|---------|
| Config | `~/.config/caudra/` | `%APPDATA%\\caudra\\` |
| Data | `~/.local/share/caudra/` | `%APPDATA%\\caudra\\` |
| State | `~/.local/state/caudra/` | `%APPDATA%\\caudra\\` |
| Logs | `~/.local/logs/caudra/` | `%APPDATA%\\caudra\\` |
| Cache | `~/.cache/caudra/` | `%LOCALAPPDATA%\\caudra\\` |

Config holds `init.lua`, `permissions.toml`, `mcp.toml`, `providers.toml`, and `commands/`. State holds sessions, auth tokens, memories, plans, and model-tier overrides. The install script puts the binary under `%LOCALAPPDATA%\\caudra` on Windows; that is separate from these runtime dirs.

`~/.caudra/` (or `%USERPROFILE%\\.caudra\\`) is checked as a legacy fallback. If that directory still exists, caudra uses it for everything until you migrate.

Development builds compiled with debug assertions use `caudra-debug` for every platform directory and `~/.caudra-debug/` for the legacy fallback. This keeps global config, sessions, auth, logs, and caches separate from release builds. Per-project `.caudra/` directories remain shared.

### Migrating from ~/.caudra/

```
caudra migrate xdg
```

This safely moves sessions, auth, plans, memories, logs, and preferences to the platform locations above. Where both old and new files exist, they are merged (input history, model tiers, etc.). Nothing is deleted until it has been copied. At the end you get a summary of where everything lives now.

Safe to run more than once.

## Personal Instructions

On top of the project instruction files Caudra loads from the git root down to the cwd (`AGENTS.md`, `CLAUDE.md`, and friends; see [Context](/docs/context/#instruction-files)), you can add:

- `AGENTS.local.md` in any of those project directories for per-directory preferences (gitignored)
- `~/.config/caudra/AGENTS.md` for preferences that apply to all projects

All of these are added to the system prompt at the start of every session.

## Memory

The `memory` tool and `/memory` command store small Markdown notes under the state directory, scoped per project:

`…/state/caudra/projects/<project-id>/memories/`

(Linux/macOS: `~/.local/state/caudra/…`; Windows: `%APPDATA%\\caudra\\…`). Use them for non-obvious gotchas and decisions that should survive across sessions. They are separate from skills and from `AGENTS.md`.

Related pages: [Skills](/docs/skills/), [CLI](/docs/cli/), [Providers](/docs/providers/#providers-toml)."
    )
    .unwrap();

    out
}
