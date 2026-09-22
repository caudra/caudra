use std::fmt::Write;
use std::sync::Arc;

use caudra_agent::tools::ToolRegistry;
use caudra_config::{
    AgentConfig, ConfigField, DEFAULT_MAX_LOG_FILES, DEFAULT_MAX_OUTPUT_LINES,
    DEFAULT_MOUSE_SCROLL_LINES, MIN_TOOL_OUTPUT_LINES, ProviderConfig, RetentionConfig,
    SnapshotsConfig, StorageConfig, TOP_LEVEL_FIELDS, TelemetryConfig, ToolOutputLines, UiConfig,
};
use caudra_lua::{OptionSpec, OptionType, PluginHost, PluginOptionSpecs};

const PLUGIN_DEV_DESC: &str =
    "Offer the builtin caudra-plugin-dev skill for writing caudra plugins.";
const WORKFLOW_DEV_DESC: &str =
    "Offer the builtin caudra-workflow-dev skill for writing and running workflows.";
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
            "`file_index` executes as a native Workcell tool, and this table keeps the `plugins.index` key it was configured under. The file-size limit accepts {} through {} MiB to bound parser memory and work.",
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
        vec![
            OptionSpec {
                name: "plugin_dev".into(),
                ty: OptionType::Boolean,
                default: Some(serde_json::json!(caudra_config::DEFAULT_SKILL_PLUGIN_DEV)),
                min: None,
                desc: PLUGIN_DEV_DESC.into(),
            },
            OptionSpec {
                name: "workflow_dev".into(),
                ty: OptionType::Boolean,
                default: Some(serde_json::json!(caudra_config::DEFAULT_SKILL_WORKFLOW_DEV)),
                min: None,
                desc: WORKFLOW_DEV_DESC.into(),
            },
        ],
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

fn write_snapshots_section(out: &mut String) {
    write_section(out, "[storage.snapshots]", SnapshotsConfig::FIELDS);
    writeln!(
        out,
        "A workspace over `max_bytes_mb` or `max_files` is refused rather than captured, \
         and individual files over `max_file_bytes_mb` are skipped while the rest of the \
         tree is still captured. A refusal costs file revert and lets the tool call \
         proceed. See [Sessions](/docs/sessions/#limits) for what a capture covers.\n"
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

fn write_steering_section(out: &mut String) {
    out.push_str(
        r###"### `agent.steering`

Automatic steering repairs unusable model output and can add bounded guidance about repeated behavior. All nine rules are enabled by default. Configure overrides inside `agent` in `caudra.setup()`. All fields are optional.

| Field | Type | Default | Limits and meaning |
|-------|------|---------|--------------------|
| `enabled` | boolean | `true` | Master switch for automatic steering, including truncation recovery and repeat-policy blocking. |
| `max_recoveries` | integer | `32` | 0–1024 corrective continuations per externally initiated invocation. Zero prevents optional recovery continuations. |
| `max_advisories` | integer | `4` | 0–1024 advisory injections per invocation. Zero suppresses advisories. |
| `max_stalled_turns` | integer | `5` | 0–1024 consecutive turns carrying neither a tool call nor visible text before the run ends, whichever rule intervened. Zero disables the backstop. |
| `rules` | table | `{}` | Overrides by rule name, listed below. Omission uses built-in defaults. |
| `models` | table | `{}` | Up to 256 exact `provider/model-id` keys, each with its own overrides. |

#### Rules

| Rule in `rules` | Enabled by default | Behavior |
|-----------------|--------------------|----------|
| `truncation` | `true` | Continue output cut off by the response token limit, up to 3 corrective requests per externally initiated invocation. |
| `empty_response` | `true` | Continue after empty output, with separate per-episode limits after recent tools and while idle. |
| `repeated_tool_call` | `true` | Refuse the third consecutive identical top-level tool name/input before execution. Native batch children do not acquire this hard blocker. |
| `protocol_mismatch` | `true` | Correct an explicit provider tool-use indication with no actual tool calls, up to 2 continuations per episode. |
| `missing_task_report` | `true` | Request a missing task summary or required structured report, up to 2 corrections. |
| `abandoned_turn` | `true` | Continue a turn that ended by announcing work the response never performed, up to 2 continuations per episode. Spending the allowance accepts the text rather than failing the turn. |
| `repetition` | `true` | Advise on short exact tool cycles, including normalized native batch leaf calls, or repeated normalized assistant text. |
| `tool_planning` | `true` | Advise after repeated narrow tool choice across responses, with repeated-call/cycle or repeated-error evidence. Successful reads of different files alone are insufficient. |
| `no_tool_use` | `true` | Suggest tools when useful after eligible responses without tool attempts, only when the effective tool inventory is nonempty. |

Recovery and advisory budgets are separate. Advisory rules allow at most 4 total injections per invocation, with a default cooldown of 3 completed model responses for each rule.

An empty-response episode lives in the transcript tail, so it survives a restore and a new invocation. A message typed into a stall is answered, but it does not refill the budget: only a response carrying a tool call or visible text ends the episode. `max_stalled_turns` bounds the turns that interleaved rules spend between them, independently of any single rule's allowance.

Advisories only accompany an independently scheduled next request. They never reopen a valid final answer. Tool-looking prose, JSON, XML, code fences, and quoted examples do not independently trigger protocol correction. Caudra does not scrape tool names or arguments from text and execute them. Only actual tool calls pass through normal validation and authorization. Ordinary assistant answers do not have to be JSON.

#### Rule fields

Each table at `agent.steering.rules.<rule>` accepts these common fields:

| Field | Type | Default | Limits and meaning |
|-------|------|---------|--------------------|
| `enabled` | boolean | `true` | Explicit `false` disables this rule. |
| `prompt` | string | `nil` | Use built-in guidance when omitted. Custom text must be nonblank and at most 16,384 UTF-8 bytes. |

Custom prompts replace guidance only. They are literal user-configured text, without template expansion or executable expressions. They do not change triggers, budgets, enforcement, or factual tool-failure information. A custom prompt cannot authorize a tool or turn a rejected call into an executed one.

The remaining fields are integers. All ranges are inclusive. Set `enabled = false` to disable a rule rather than setting a positive threshold to zero.

| Field under `rules` | Default | Range | Unit and meaning |
|---------------------|---------|-------|------------------|
| `truncation.max_attempts` | `3` | 1–1024 | Actual truncation-correction requests per externally initiated invocation, shared across truncation episodes. |
| `empty_response.max_after_tools` | `3` | 1–1024 | Empty-output continuations per episode after recent tool results. |
| `empty_response.max_idle` | `2` | 1–1024 | Empty-output continuations per episode without recent tool results. |
| `empty_response.max_barren` | `1` | 1–1024 | Continuations per episode after a response that carried no content at all. Clamped by the limit above; repeating an unchanged request is not a retry. |
| `empty_response.recent_tool_window` | `5` | 1–4096 | Non-padding history messages inspected for recent tool results. |
| `repeated_tool_call.threshold` | `3` | 2–1024 | Consecutive identical top-level calls. Refuse the call reaching this threshold. |
| `protocol_mismatch.max_attempts` | `2` | 1–1024 | Protocol corrective continuations per episode. |
| `missing_task_report.max_attempts` | `2` | 1–1024 | Additional report-correction prompts per task invocation. |
| `abandoned_turn.max_attempts` | `2` | 1–1024 | Continuations per episode after a turn that announced work instead of doing it. |
| `repetition.window` | `24` | 1–4096 | Recent normalized leaf tool calls retained for cycle detection. |
| `repetition.cycle_repeats` | `3` | 2–1024 | Exact repetitions of a tool cycle needed for an advisory. |
| `repetition.max_cycle` | `4` | 2–1024 | Maximum cycle length in leaf calls. Candidate cycle lengths start at 2. |
| `repetition.text_window` | `8` | 1–4096 | Recent completed assistant responses retained for text repetition. |
| `repetition.text_repeats` | `3` | 2–1024 | Matching nontrivial normalized assistant responses needed for an advisory. |
| `repetition.cooldown` | `3` | 1–1024 | Completed model responses between this rule's advisories. |
| `tool_planning.after_calls` | `6` | 1–1024 | Uses of the same canonical tool, with repetition or error evidence. |
| `tool_planning.after_responses` | `3` | 1–1024 | Completed model responses across which those tool uses must occur. |
| `tool_planning.cooldown` | `3` | 1–1024 | Completed model responses between this rule's advisories. |
| `no_tool_use.after_responses` | `3` | 1–1024 | Eligible completed assistant responses without tool attempts. |
| `no_tool_use.window` | `8` | 1–4096 | Recent assistant responses inspected for the no-tool pattern. |
| `no_tool_use.cooldown` | `3` | 1–1024 | Completed model responses between this rule's advisories. |

Validation also requires:

- `repetition.window >= repetition.max_cycle * repetition.cycle_repeats`.
- `repetition.text_window >= repetition.text_repeats`.
- `no_tool_use.window >= no_tool_use.after_responses`.
- `tool_planning.after_calls >= tool_planning.after_responses`.

Unknown fields, invalid types, out-of-range values, and impossible threshold/window combinations are rejected, even for disabled rules. Cooldowns count completed model responses, not seconds, stream chunks, tool children, or injected messages. No-tool eligibility excludes synthetic messages, empty markers, reasoning-only padding, and private title, compaction, or evaluator requests. Recent-pattern windows reset after compaction or a model change without refilling an active invocation's budgets.

Tool-planning guidance asks the model to reconsider its tool choices and identify the next useful action. It does not switch Plan Mode or require a todo list. No-tool guidance permits a direct answer when tools are unnecessary or contrary to the user's instructions.

#### Global and exact-model overrides

This example disables no-tool guidance globally, then enables it with a higher threshold for one exact model and adjusts that model's tool-planning guidance:

```lua
caudra.setup({
    agent = {
        steering = {
            rules = {
                no_tool_use = { enabled = false },
            },
            models = {
                ["openai/gpt-5"] = {
                    rules = {
                        no_tool_use = { enabled = true, after_responses = 4 },
                        tool_planning = {
                            after_calls = 8,
                            prompt = "Reassess your recent tool choices. Choose a different useful action if these calls are not helping.",
                        },
                    },
                },
            },
        },
    },
})
```

Model entries accept `enabled`, `max_recoveries`, `max_advisories`, `max_stalled_turns`, and `rules` with the same types and limits as the global fields. They cannot contain another `models` table. Omitted fields inherit through the resolution order below.

Keys are case-sensitive exact IDs, at most 512 UTF-8 bytes each. Use a nonempty provider and model suffix separated by `/`. Additional slashes inside the suffix are allowed, but every segment must be nonempty. Whitespace, control characters, `*`, `?`, `[`, `]`, `{`, `}`, and backslashes are rejected. Matching requires no authentication or model discovery. There are no glob overrides, provider-wide layers, capability guesses from model names, or Lua detector callbacks.

Global and project Lua settings merge field by field, with project values taking precedence. Model maps merge by exact key and rules merge by rule name and field. Omission inherits. Explicit `false` and `0` survive merging. An empty table does not clear inherited entries. Disable an inherited model policy or rule with `enabled = false`.

After merging, resolve against the effective routed model for Chat, Plan, or a delegated task:

1. Start with built-in policy and rule defaults.
2. Apply explicit global fields and rule fields.
3. Apply explicit matching model fields and rule fields.

A child resolves its own effective model using the inherited unresolved configuration, rather than inheriting the parent's resolved policy or runtime counters.

#### Budgets and safety boundaries

A new externally initiated main-agent or task invocation gets its own allowance. An explicit user/caller resume starts a fresh bounded invocation. Automatic continuations, internal retries, task report-correction prompts, compaction, and mid-run queued instructions do not refill the active allowance, including when report correction constructs a fresh agent. Separately delegated children have independent allowances. Counters are not durable across process restarts or explicit task resume.

Charge one recovery for a completed-response-to-next-request transition caused by empty or truncated output, all-invalid tool calls, a response consisting entirely of repeat-policy refusals, an explicit protocol mismatch, or a missing task report. Corrective tool-error feedback can supply the guidance without a supplemental prompt and still consumes the transition. Malformed-argument and schema repair use this allowance without a separate rule table. Per-rule limits apply underneath the combined recovery cap.

A mixed batch with useful successful siblings proceeds normally. It is not replayed or charged once per child. Transport and authentication retries, ordinary tool execution failures, permission denials, normal successful tool progress, explicit goal evaluation, and manual steering are separate from model-format recovery. The recovery budget does not bound every possible agent loop. Outer turn limits and cancellation still apply.

At most one supplemental steering message is added per request. Recovery takes priority, then repetition, tool planning, and no-tool guidance. Advisory exhaustion only suppresses hints. Recovery exhaustion with an unmet output contract reports a failure and retains partial output, except for `abandoned_turn`, which stops intervening and lets the turn end. A valid captured structured task report remains usable after an empty tail, but cancellation, transport/permission failures, and hard outer-limit failures do not become success.

`abandoned_turn` reads the tail of a response that called no tool and would otherwise end the turn. It fires on a text stopping at a bare colon, or on a last sentence that opens on an intent to act. It does not fire on a question, an offer, a completion, or a promise deferred behind another event, and code spans and quoted prose are removed before any of that is matched. Tool-looking prose is not executed here either; the rule only decides whether to ask for one more response.

The resolved `enabled = false` disables automatic recovery, including truncation, advisories, and repeat-policy blocking. It leaves malformed-input rejection, schema validation, permissions, mode restrictions, cancellation, explicit goals, manual steering, and compaction policy intact. A rule-level switch disables only that rule. Zero budgets prevent the corresponding continuations or hints without bypassing input validation or repeat-policy enforcement.

Truncation recovery counts corrective requests, not ordinary responses or tool rounds. Each request consumes one attempt from `rules.truncation.max_attempts` and one recovery from `max_recoveries`. Automatic report-correction prompts and compaction do not refill either allowance. The last allowed correction may complete the answer. If another correction is needed, exhaustion reports an error with partial output and usage retained, including when `max_recoveries = 0`.

Disabling the master switch or setting `rules.truncation.enabled = false` stops with the truncated outcome instead of requesting a continuation. Disabled or exhausted truncation does not fall through to empty-response repair, even when the truncated response is empty. An unresolved truncated task result remains cut short rather than reopening through report repair. A valid structured report can still satisfy the task's report contract. Cancellation, queued user instructions, and outer turn limits take priority over automatic steering.

"###,
    );
}

fn write_tool_output_section(out: &mut String) {
    writeln!(out, "### `ui.tool_output_lines`\n").unwrap();
    writeln!(
        out,
        "How many terminal rows of output an open card shows per tool before it says how \
         many it is holding back. A line that wraps spends a row for each row it wraps to, \
         so an abridged card is the same height whatever its lines are. Clicking the card \
         shows all of it regardless. \
         All values are `usize` with a minimum of {MIN_TOOL_OUTPUT_LINES}.\n\n\
         The `bash`, `python_execution`, and `task` entries apply only when \
         `ui.scroll_card_lines` is `0`. Above that, those tools draw a fixed window of \
         that many rows instead, and the budget here goes unused. `write` does not reach \
         a `file_write` that created a file, whose body is that file and is always drawn \
         whole. It is also a floor rather than a bound for anything drawn as a diff, since \
         a diff is already only the part that changed: an edit, a patch, and an overwrite \
         are drawn whole until they run long, and raising `write` past that point is what \
         makes this number matter to them.\n"
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

Remote sessions load only the client's global executable configuration. They do not load either checkout's project `init.lua`, project environment files, or project MCP configuration. Remote project context uses a bounded declarative asset manifest instead. See [Remote Workspaces](/docs/remote-workspaces/#project-context-and-trust).

Remote endpoint profiles live in a separate user `workcell.toml`, with `version = 1` and tables named `[workcell.profiles.NAME]`. They are not `caudra.setup()` settings. See [profile configuration](/docs/remote-workspaces/#configure-a-profile) for the exact fields and credential rules.

Managed sandbox providers, profiles, network policies and transfer defaults live in user-global `sandboxes.toml`, also with `version = 1`. They are separate from direct Workcell and model-provider profiles. See [Managed Sandboxes](/docs/sandboxes/#configuration-schema) for the schema, TUI editor and release status. Saving these defaults does not create a VM or change a running instance.

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
    write_steering_section(&mut out);
    write_section(&mut out, "[provider]", ProviderConfig::FIELDS);
    write_section(&mut out, "[storage]", StorageConfig::FIELDS);
    write_retention_section(&mut out);
    write_snapshots_section(&mut out);
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
         `enabled = false` turns off the tools that key produced, under the \
         names they are registered with today, so `plugins.bash` turns off \
         `shell` and `plugins.edit` turns off `file_edit` and \
         `file_apply_patch`. To name a tool directly, use \
         `agent.disabled_tools`, described in \
         [Disabling tools](/docs/tools/#disabling-tools).\n\n\
         The edit plugin's extra tools are options too: \
         `plugins.edit = {{ multiedit = false, insert_lines = true }}`.\n\n\
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
| Scratch | `$TMPDIR/caudra/` | `%TEMP%\\caudra\\` |

Config holds `init.lua`, `permissions.toml`, `mcp.toml`, `providers.toml`, `workcell.toml`, `sandboxes.toml`, and `commands/`. State holds sessions, auth tokens, memories, plans, model-job bindings, sandbox lifecycle records and transfer recovery journals. The install script puts the binary under `%LOCALAPPDATA%\\caudra` on Windows; that is separate from these runtime dirs.

Scratch holds work that belongs outside your project, such as a file the model writes while thinking or a temporary a command leaves behind. Each project gets its own subdirectory, named by the project directory plus a three-word phrase derived from its path, as in `caudra-heroic-easy-grouse`. Two checkouts sharing a name get different phrases, so they cannot overwrite each other. The phrase is derived rather than drawn at random, so a project returns to the same directory on every run. Caudra creates it at startup and points `TMPDIR`, `TMP`, and `TEMP` at it, so every command Caudra runs puts its own temporary files there instead of the shared temp root. The model is told the path, and writing anywhere under the scratch root needs no approval. Paths beside the root still ask.

The choice of subdirectory is fixed when Caudra starts. `/cd` moves the project without moving it, which is why approval covers the whole root rather than one project's share of it. Directories are owner-only, and Caudra refuses to use one that turns out to be a symlink. Nothing sweeps them, so they live as long as your system keeps its temp root.

State that belongs to one project sits under `…/state/caudra/projects/<project-id>/`, where the id is the project directory name plus a hash of its path. Memory notes and plan-mode documents both live there, so removing that directory clears everything Caudra kept for the project.

Development builds compiled with debug assertions use `caudra-debug` for every platform directory. This keeps global config, sessions, auth, logs, and caches separate from release builds. Per-project `.caudra/` directories remain shared.

Set `CAUDRA_NAMESPACE` to choose the directory name yourself instead of letting the build profile pick it. The value is a single directory name, so `CAUDRA_NAMESPACE=caudra-review` reads and writes `~/.config/caudra-review/`, `~/.local/state/caudra-review/`, and the rest. Use it to give a run its own config and session store, or to point a debug build at your release directories. A value that cannot be a directory name, such as one holding a path separator or `..`, stops Caudra with an error rather than falling back. An empty value counts as unset. Per-project `.caudra/` directories are unaffected.

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

#[cfg(test)]
mod tests {
    use caudra_config::{AgentConfig, SteeringConfig};
    use serde_json::Value;
    use test_case::test_case;

    use super::write_steering_section;

    const MODEL: &str = "provider/model";
    const HEADING: &str = "### `agent.steering`";
    const PROMPT_ROW: &str = "| `prompt` | string | `nil` |";
    const RULE_COUNT: usize = 9;

    #[test]
    fn steering_reference_matches_resolved_defaults() {
        let config = SteeringConfig::default();
        config.validate().unwrap();
        let policy = serde_json::to_value(config.resolve(MODEL)).unwrap();
        let mut reference = String::new();
        write_steering_section(&mut reference);
        assert_eq!(reference.matches(HEADING).count(), 1);
        assert!(
            AgentConfig::FIELDS
                .iter()
                .all(|field| field.name != "steering")
        );
        assert!(reference.lines().any(|line| line.starts_with(PROMPT_ROW)));

        for field in [
            "enabled",
            "max_recoveries",
            "max_advisories",
            "max_stalled_turns",
        ] {
            let row = reference
                .lines()
                .find(|line| line.starts_with(&format!("| `{field}` |")))
                .unwrap();
            assert_eq!(
                row.split('|').nth(3).unwrap().trim(),
                format!("`{}`", policy[field])
            );
        }

        let mut numeric_fields = 0;
        let rules = policy["rules"].as_object().unwrap();
        assert_eq!(rules.len(), RULE_COUNT);
        for (rule, fields) in rules {
            let row = reference
                .lines()
                .find(|line| line.starts_with(&format!("| `{rule}` |")))
                .unwrap();
            assert_eq!(
                row.split('|').nth(2).unwrap().trim(),
                format!("`{}`", fields["enabled"])
            );
            for (field, value) in fields.as_object().unwrap() {
                match field.as_str() {
                    "enabled" => assert_eq!(value, &Value::Bool(true)),
                    "prompt" => assert_eq!(value, &Value::Null),
                    _ => {
                        assert!(value.is_number());
                        let prefix = format!("| `{rule}.{field}` | `{value}` |");
                        assert!(reference.lines().any(|line| line.starts_with(&prefix)));
                        numeric_fields += 1;
                    }
                }
            }
        }
        assert_eq!(
            reference
                .lines()
                .filter(
                    |line| line.starts_with("| `") && line.split('|').nth(1).unwrap().contains('.')
                )
                .count(),
            numeric_fields
        );
    }

    #[test_case("/max_recoveries", 32; "combined_recoveries")]
    #[test_case("/max_advisories", 4; "combined_advisories")]
    #[test_case("/rules/truncation/max_attempts", 3; "truncation_attempts")]
    #[test_case("/rules/abandoned_turn/max_attempts", 2; "abandoned_turn_attempts")]
    fn steering_numeric_defaults(path: &str, expected: u64) {
        let policy = serde_json::to_value(SteeringConfig::default().resolve(MODEL)).unwrap();
        assert_eq!(policy.pointer(path).and_then(Value::as_u64), Some(expected));
    }
}
