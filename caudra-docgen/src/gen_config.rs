use std::fmt::Write;

use caudra_config::config_file::CONFIG_VERSION;
use caudra_config::decisions::{DecisionFeatures, DecisionThresholds, FeatureMode};
use caudra_config::example::Entry;
use caudra_config::files::{self, CONFIG_FILES, ConfigFile, Scope};
use caudra_config::steering::{SteeringRule, SteeringRulesConfig};
use caudra_config::{
    AgentConfig, ConfigField, ConfigValue, DEFAULT_MAX_LOG_FILES, DEFAULT_MAX_OUTPUT_LINES,
    DEFAULT_MOUSE_SCROLL_LINES, DecisionsConfig, Feature, FeatureFlags, MIN_TOOL_OUTPUT_LINES,
    MessagingConfig, NATIVE_PLUGIN_OPTIONS, ProviderConfig, RetentionConfig, SnapshotsConfig,
    SteeringConfig, StorageConfig, TOP_LEVEL_FIELDS, TelemetryConfig, ToolOutputLines, UiConfig,
    WorktreesConfig,
};

use crate::gen_providers::join_and;

const EXAMPLE_SUFFIX: &str = ".example.toml";
/// The one feature with a section of its own further down the page.
const SHELL_DURATION_FEATURE: &str = "shell_duration";
const SHELL_DURATION_LINK: &str = " See [shell duration](#shell-duration).";

/// The file under the site's static root that holds a file's reference.
pub fn example_file_name(file: &ConfigFile) -> String {
    format!("{}{EXAMPLE_SUFFIX}", file.stem())
}

/// Shown bare, since the text is prose rather than a value to copy.
fn default_cell(value: &ConfigValue) -> String {
    match value {
        ConfigValue::Unset | ConfigValue::Varies(_) | ConfigValue::Required(_) => {
            value.format_default()
        }
        _ => format!("`{}`", value.format_default()),
    }
}

fn range_cell(field: &ConfigField) -> String {
    match (field.min, field.max) {
        (Some(min), Some(max)) => format!("{min}–{max}"),
        _ => "-".into(),
    }
}

type ExtraColumn = (&'static str, fn(&Entry) -> String);

fn write_table(out: &mut String, fields: &[ConfigField]) {
    let entries: Vec<Entry> = fields.iter().map(Entry::from).collect();
    write_entries(out, &entries);
}

pub fn write_entries<'a>(out: &mut String, entries: impl IntoIterator<Item = &'a Entry>) {
    let entries: Vec<&Entry> = entries.into_iter().collect();
    let mut extras: Vec<ExtraColumn> = Vec::new();
    if entries.iter().any(|f| f.env.is_some()) {
        extras.push(("Env", |f: &Entry| {
            f.env.map_or("-".to_string(), |e| {
                e.split(", ")
                    .map(|v| format!("`{v}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
        }));
    }
    if entries.iter().any(|f| f.min.is_some()) {
        extras.push(("Min", |f: &Entry| {
            f.min.map_or("-".to_string(), |v| v.to_string())
        }));
    }
    if entries.iter().any(|f| f.max.is_some()) {
        extras.push(("Max", |f: &Entry| {
            f.max.map_or("-".to_string(), |v| v.to_string())
        }));
    }

    let header: String = extras
        .iter()
        .map(|(name, _)| format!(" {name} |"))
        .collect();
    let rule: String = extras.iter().map(|_| "-----|").collect();
    writeln!(out, "| Field | Type | Default |{header} Description |").unwrap();
    writeln!(out, "|-------|------|---------|{rule}-------------|").unwrap();
    for f in entries {
        let cells: String = extras
            .iter()
            .map(|(_, cell)| format!(" {} |", cell(f)))
            .collect();
        writeln!(
            out,
            "| `{name}` | {ty} | {default} |{cells} {desc} |",
            name = f.name,
            ty = escape_pipes(f.ty),
            default = default_cell(&f.default),
            desc = f.description,
        )
        .unwrap();
    }
}

fn escape_pipes(ty: &str) -> String {
    ty.replace('|', "\\|")
}

fn write_section(out: &mut String, table: &str, fields: &[ConfigField]) {
    writeln!(out, "### `{table}`\n").unwrap();
    write_table(out, fields);
    writeln!(out).unwrap();
}

fn experiment_scope(feature: Feature) -> &'static str {
    match feature {
        Feature::Workflows => {
            "[Workflows](/docs/workflows/): the `workflow` tool, the workflow commands and \
             inspector, and the `caudra-workflow-dev` skill."
        }
        Feature::Sandboxes => {
            "[Managed sandboxes](/docs/sandboxes/): `caudra sandbox`, `caudra auth sandbox`, \
             `--sandbox`, `/sandbox`, and the workbench Transfer view. Sandboxes bring their own \
             connection to Workcell and do not need `remote_workcell`."
        }
        Feature::RemoteWorkcell => {
            "Direct [remote Workcell](/docs/remote-workspaces/) connections: the `--workcell-*` \
             flags and `caudra auth workcell`."
        }
        Feature::LuaPlugins => {
            "Every use of Lua: [plugins](/docs/plugins/), the [Lua API](/docs/lua-api/), global \
             and project `init.lua`, and the `caudra-plugin-dev` skill. `--no-plugins` still \
             turns Lua off for one run."
        }
        Feature::DecisionEngine => {
            "The [decision engine](#decisions), [Auto mode](/docs/permissions/#auto-mode), \
              `caudra decisions`, and workflow [`decide()` calls](/docs/workflows/#typed-decisions)."
        }
        Feature::CrossSessionMessaging => {
            "Local [cross-session messaging](/docs/messaging/): the `list_sessions`, `send_message`, `publish_message`, and `read_topic` tools, `/peers`, `/messages`, `/topics`, `caudra message`, live session inboxes, and the message history. Both processes must opt in."
        }
    }
}

fn write_experimental_section(out: &mut String) {
    out.push_str(
        "## Experimental features\n\n\
         Some features are experimental and stay off until you turn them on. Each one has its \
         own switch in the `[experimental]` table of the global `caudra.toml`:\n\n\
         ```toml\n\
         [experimental]\n\
         workflows = true\n\
         decision_engine = true\n\
         ```\n\n\
         | Key | Default | Turns on |\n\
         |-----|---------|----------|\n",
    );
    for feature in Feature::ALL {
        writeln!(
            out,
            "| `{key}` | `{default}` | {scope} |",
            key = feature.key(),
            default = FeatureFlags::default().enabled(feature),
            scope = experiment_scope(feature),
        )
        .unwrap();
    }
    out.push_str(
        "\nEach switch is independent, so turning one on never turns on another. A missing file, \
         table, or key leaves a switch off, and an unknown key is an error. `caudra remote` and \
         `/remote` work when either `sandboxes` or `remote_workcell` is on, and each session \
         checks the switch for its own source.\n\n\
         Only the global file may hold `[experimental]`. Caudra rejects a project \
         `.caudra/caudra.toml` that contains the table, even an empty one, so a repository cannot \
         opt you in. Lua, tool allowlists, and saved sessions cannot turn a feature on either.\n\n\
         Caudra reads the switches once at startup and keeps them until it exits. `/reload`, \
         session switches, and ACP sessions all use the startup values. When the table changes on \
         disk, Caudra shows a notice asking for a restart.\n\n\
         A feature that is off is hidden and does no work. Its tools, commands, shortcuts, help \
         entries, and status chips are gone, and startup skips it. Asking for it directly, such \
         as typing its command or passing its flag, fails with a message that names the switch. \
         A saved session attached to a sandbox or a remote workspace does not resume while its \
         switch is off, and it never falls back to local execution. Turning a feature off keeps \
         its data and leaves external resources alone, so a running sandbox keeps running until \
         you stop it.\n\n\
         With `decision_engine` off, `always_auto = true` and sessions saved in Auto start in \
         Ask. Caudra keeps the saved choice, so Auto returns once the switch is on again.\n\n",
    );
}

fn write_migration_section(out: &mut String) {
    out.push_str(
        r#"## Migrating from Lua settings

Earlier releases read settings from `init.lua` through `caudra.setup()`. Caudra now runs `init.lua` only when `lua_plugins` is on. With Lua off, Caudra shows one notice that names each `init.lua` it skipped. It does not read, run, or change those files.

To migrate, move the table you passed to `caudra.setup()` into the `caudra.toml` of the same scope. Keys and values stay the same. Top-level values come first, and each nested table becomes a TOML table:

```lua
caudra.setup({
    always_fast = true,
    ui = { theme = "tokyonight" },
    agent = { disabled_tools = { "websearch" } },
})
```

```toml
always_fast = true

[ui]
theme = "tokyonight"

[agent]
disabled_tools = ["websearch"]
```

Lists become arrays, and deeper tables become dotted headers such as `[agent.steering.rules.repetition]`. Quote a key that holds other characters, as in `[agent.steering.models."openai/gpt-5"]`. Leave out any key you set to `nil`.

To keep using `init.lua`, set `lua_plugins = true` under `[experimental]`. Its `caudra.setup()` values then apply on top of the `caudra.toml` of the same scope, in the order shown [above](#configuration).

"#,
    );
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

fn write_plugin_options(out: &mut String) {
    for (plugin, fields) in NATIVE_PLUGIN_OPTIONS {
        writeln!(out, "### `plugins.{plugin}`\n").unwrap();
        if let Some(note) = native_tool_note(plugin) {
            writeln!(out, "{note}\n").unwrap();
        }
        write_table(out, fields);
        writeln!(out).unwrap();
    }
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
         half makes Caudra ask the terminal whether it is in light or dark mode \
         and show the half that matches. Caudra changes halves as soon as the \
         terminal reports a new mode, for example when your desktop switches to \
         dark mode. A terminal that cannot report its mode is asked for its \
         background color instead. Themes outside these pairs stay as you left \
         them.\n"
    )
    .unwrap();
    writeln!(
        out,
        "Caudra asks again every ten minutes, and also when the terminal \
         regains focus or changes size, so reattaching a multiplexer to \
         another terminal updates the theme. Following the terminal only \
         changes the running session, and the theme you saved from `/theme` \
         stays saved. A terminal that answers neither question is asked for its \
         background color only a few times. After that, Caudra asks only \
         whether it is in light or dark mode.\n"
    )
    .unwrap();
    writeln!(
        out,
        "Mosh drops the request for mode reports before it reaches your \
         terminal. Turn the reports on yourself when you connect, and off again \
         when you leave:\n"
    )
    .unwrap();
    writeln!(
        out,
        "```sh\nprintf '\\033[?2031h'; mosh user@host; printf '\\033[?2031l'\n```\n"
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
        "```toml\n[ui]\ntheme = \"tokyonight\"\ntheme_light = \"catppuccin_latte\"\n```\n"
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
        "A change record that would cover more than `max_files` files or `max_bytes_mb` of \
         file data is refused, and its call runs without a record. A file over \
         `max_file_bytes_mb` is left unrecorded. Values above the store's limits are lowered \
         to them, and zero is rejected. See [Sessions](/docs/sessions/#limits) for what a \
         record covers.\n"
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

fn write_worktrees_section(out: &mut String) {
    write_section(out, "[worktrees]", WorktreesConfig::FIELDS);
    writeln!(
        out,
        "Inside a Herdr pane, `auto` asks Herdr to create and remove worktrees, so each one \
         opens as a grouped Herdr workspace. `directory` applies only to worktrees git \
         creates. See [Worktrees](/docs/worktrees/) for what `/worktree` does with each \
         backend.\n"
    )
    .unwrap();
}

fn write_steering_section(out: &mut String) {
    out.push_str(
        "### `agent.steering`\n\n\
         Automatic steering repairs unusable model output and can add bounded guidance about \
         repeated behavior or needlessly long tool paths. Every rule is enabled by default. \
         Configure overrides in the `[agent.steering]` table. All fields are optional.\n\n",
    );
    write_table(out, SteeringConfig::FIELDS);
    out.push_str(
        "\n#### Rules\n\n\
         | Rule in `rules` | Behavior |\n\
         |-----------------|----------|\n",
    );
    for rule in SteeringRulesConfig::RULES {
        writeln!(out, "| `{}` | {} |", rule.name, rule.description).unwrap();
    }
    out.push_str(
        r###"
Recovery and advisory budgets are separate. Advisory rules allow at most 4 total injections per invocation. Repetition and tool-planning advisories each wait a default cooldown of 3 completed model responses.

Tool-planning evidence starts after the last response containing any successful tool result, including results outside the retained batch window. A background admission ends the failure episode without proving that the background work succeeded. Later terminal outcomes do not retroactively change that admission into a failed attempt.

Relative-path evidence is the latest response only, and the hint yields to repetition and tool-planning advisories. While an earlier hint remains in context, even across user turns, no new hint is added. Once compaction removes it, another hint appears only if a later response uses absolute paths again. A suggestion climbs at most one directory with `../`. Local sessions get suggestions only when the working directory the model sees is the canonical project path. Remote workspaces, sandboxes, and code-graph scopes get suggestions inside the working directory only. Paths with backticks, angle brackets, control characters, or invisible Unicode formatting characters are never quoted. Caudra never rewrites the paths a model sends.

An empty-response episode lives in the transcript tail, so it survives a restore and a new invocation. A message typed into a stall is answered, but it does not refill the budget: only a response carrying a tool call or visible text ends the episode. `max_stalled_turns` bounds the turns that interleaved rules spend between them, independently of any single rule's allowance.

Advisories only accompany an independently scheduled next request. They never reopen a valid final answer. Tool-looking prose, JSON, XML, code fences, and quoted examples do not independently trigger protocol correction. Caudra does not scrape tool names or arguments from text and execute them. Only actual tool calls pass through normal validation and authorization. Ordinary assistant answers do not have to be JSON.

#### Rule fields

Each table at `agent.steering.rules.<rule>` accepts these common fields:

"###,
    );
    write_table(out, SteeringRule::COMMON_FIELDS);
    out.push_str(
        r###"
Custom prompts replace guidance only. They are literal user-configured text, without template expansion or executable expressions. They do not change triggers, budgets, enforcement, or factual tool-failure information. A custom prompt cannot authorize a tool or turn a rejected call into an executed one.

The remaining fields are integers. All ranges are inclusive. Set `enabled = false` to disable a rule rather than setting a positive threshold to zero.

| Field under `rules` | Default | Range | Unit and meaning |
|---------------------|---------|-------|------------------|
"###,
    );
    for rule in SteeringRulesConfig::RULES {
        for field in rule.fields {
            writeln!(
                out,
                "| `{}.{}` | {} | {} | {} |",
                rule.name,
                field.name,
                default_cell(&field.default),
                range_cell(field),
                field.description,
            )
            .unwrap();
        }
    }
    out.push_str(
        r###"
Validation also requires:

- `repetition.window >= repetition.max_cycle * repetition.cycle_repeats`.
- `repetition.text_window >= repetition.text_repeats`.
- `tool_planning.after_calls >= tool_planning.after_responses`.

Unknown fields, invalid types, out-of-range values, and impossible threshold/window combinations are rejected, even for disabled rules. Cooldowns count completed model responses, not seconds, stream chunks, tool children, or injected messages. Advisory eligibility excludes synthetic messages, empty markers, reasoning-only padding, and private title, compaction, or evaluator requests. Advisory evidence is the responses to the request in flight: a new user message ends it, as a compaction or a model change does, so an ordinary conversation is never advised. Recent-pattern windows reset with it, without refilling an active invocation's budgets.

Tool-planning guidance asks the model to reconsider its tool choices and identify the next useful action. It does not switch Plan Mode or require a todo list.

#### Global and exact-model overrides

This example disables repetition guidance globally, then enables it with a higher threshold for one exact model and adjusts that model's tool-planning guidance:

```toml
[agent.steering.rules.repetition]
enabled = false

[agent.steering.models."openai/gpt-5".rules.repetition]
enabled = true
text_repeats = 4

[agent.steering.models."openai/gpt-5".rules.tool_planning]
after_calls = 8
prompt = "Reassess your recent tool choices. Choose a different useful action if these calls are not helping."
```

Model entries accept `enabled`, `max_recoveries`, `max_advisories`, `max_stalled_turns`, and `rules` with the same types and limits as the global fields. They cannot contain another `models` table. Omitted fields inherit through the resolution order below.

Keys are case-sensitive exact IDs, at most 512 UTF-8 bytes each. Use a nonempty provider and model suffix separated by `/`. Additional slashes inside the suffix are allowed, but every segment must be nonempty. Whitespace, control characters, `*`, `?`, `[`, `]`, `{`, `}`, and backslashes are rejected. Matching requires no authentication or model discovery. There are no glob overrides, provider-wide layers, capability guesses from model names, or Lua detector callbacks.

Global and project settings merge field by field, with project values taking precedence. Model maps merge by exact key and rules merge by rule name and field. Omission inherits. Explicit `false` and `0` survive merging. An empty table does not clear inherited entries. Disable an inherited model policy or rule with `enabled = false`.

After merging, resolve against the effective routed model for Chat, Plan, or a delegated task:

1. Start with built-in policy and rule defaults.
2. Apply explicit global fields and rule fields.
3. Apply explicit matching model fields and rule fields.

A child resolves its own effective model using the inherited unresolved configuration, rather than inheriting the parent's resolved policy or runtime counters.

#### Budgets and safety boundaries

A new externally initiated main-agent or task invocation gets its own allowance. An explicit user/caller resume starts a fresh bounded invocation. Automatic continuations, internal retries, task report-correction prompts, compaction, and mid-run queued instructions do not refill the active allowance, including when report correction constructs a fresh agent. Separately delegated children have independent allowances. Counters are not durable across process restarts or explicit task resume.

Charge one recovery for a completed-response-to-next-request transition caused by empty or truncated output, all-invalid tool calls, a response consisting entirely of repeat-policy refusals, an explicit protocol mismatch, or a missing task report. Corrective tool-error feedback can supply the guidance without a supplemental prompt and still consumes the transition. Malformed-argument and schema repair use this allowance without a separate rule table. Per-rule limits apply underneath the combined recovery cap.

A mixed batch with useful successful siblings proceeds normally. It is not replayed or charged once per child. Transport and authentication retries, ordinary tool execution failures, permission denials, normal successful tool progress, explicit goal evaluation, and manual steering are separate from model-format recovery. The recovery budget does not bound every possible agent loop. Outer turn limits and cancellation still apply.

At most one supplemental steering message is added per request. Recovery takes priority, then repetition, tool planning, and relative paths. Advisory exhaustion only suppresses hints. Recovery exhaustion with an unmet output contract reports a failure and retains partial output, except for `abandoned_turn`, which stops intervening and lets the turn end. A valid captured structured task report remains usable after an empty tail, but cancellation, transport/permission failures, and hard outer-limit failures do not become success.

`abandoned_turn` reads the tail of a response that called no tool and would otherwise end the turn. It fires on a text stopping at a bare colon, or on a last sentence that opens on an intent to act. It does not fire on a question, an offer, a completion, or a promise deferred behind another event, and code spans and quoted prose are removed before any of that is matched. Tool-looking prose is not executed here either; the rule only decides whether to ask for one more response.

The resolved `enabled = false` disables automatic recovery, including truncation, advisories, and repeat-policy blocking. It leaves malformed-input rejection, schema validation, permissions, mode restrictions, cancellation, explicit goals, manual steering, and compaction policy intact. A rule-level switch disables only that rule. Zero budgets prevent the corresponding continuations or hints without bypassing input validation or repeat-policy enforcement.

Truncation recovery counts corrective requests, not ordinary responses or tool rounds. Each request consumes one attempt from `rules.truncation.max_attempts` and one recovery from `max_recoveries`. Automatic report-correction prompts and compaction do not refill either allowance. The last allowed correction may complete the answer. If another correction is needed, exhaustion reports an error with partial output and usage retained, including when `max_recoveries = 0`.

Disabling the master switch or setting `rules.truncation.enabled = false` stops with the truncated outcome instead of requesting a continuation. Disabled or exhausted truncation does not fall through to empty-response repair, even when the truncated response is empty. An unresolved truncated task result remains cut short rather than reopening through report repair. A valid structured report can still satisfy the task's report contract. Cancellation, queued user instructions, and outer turn limits take priority over automatic steering.

"###,
    );
}

fn write_decisions_section(out: &mut String) {
    out.push_str(
        "### `decisions`\n\n\
         Configure the optional typed decision engine in the `[decisions]` table. \
         The engine is experimental and needs `decision_engine = true` under \
         [`[experimental]`](#experimental-features). Without that switch Caudra still validates \
         this table and starts no engine. It then sends no decision requests, reads no engine \
         credentials, and leaves decision logs and shell duration history untouched. \
         No base URL, passive feature, or decision logging is enabled by default. \
         Explicit workflow [`decide()` calls](/docs/workflows/#typed-decisions) need a base URL \
         but do not need a passive feature enabled. Shell duration history can work without a base URL.\n\n\
         Connection settings and thresholds are global-only. Projects may set individual features \
         to `\"off\"`, set `log = false`, or keep or shorten inherited log retention. \
         Other project overrides are errors, even when they repeat a global value. \
         Disabling globally required Auto screening or its active content screening restores \
         prompting for eligible Auto calls.\n\n",
    );
    write_table(out, DecisionsConfig::FIELDS);
    write!(
        out,
        "\nCaudra sends each request to `base_url` with `/v1/systemone` appended. A path prefix \
         stays in place, so a server mounted under `/typesafe` uses \
         `base_url = \"http://127.0.0.1:8080/typesafe\"` and receives requests at \
         `/typesafe/v1/systemone`. Leave `/v1/systemone` out of `base_url`. The hosted API also \
         needs remote consent:\n\n\
         ```toml\n\
         [decisions]\n\
         base_url = \"https://api.typesafe.ai\"\n\
         allow_remote = true\n\
         ```\n\n\
         `TYPESAFE_BASE_URL` replaces the whole configured base URL, path prefix included, and \
         passes the same URL and transport opt-in checks. It applies only when `base_url` is set, \
         so the variable alone never activates the engine. Project `.env` files cannot set it.\n\n\
         Caudra retries HTTP 408, 429, and 5xx responses at most twice. Each retry waits for the \
         delay the server requests in `retry-after-ms` or `Retry-After`, or else for an exponential \
         backoff that starts near half a second. No retry waits past `timeout_ms`, so under the \
         default {timeout_ms} ms deadline most retries need a short server-requested delay. Connection \
         failures, 401, 422, and other client errors fail at once.\n\n\
         Requests ignore ambient proxies and do not follow redirects. `localhost` is a DNS name, \
         not numeric loopback for this policy. Private and CGNAT addresses receive no automatic HTTP exemption. \
         These settings do not change Workcell transport policy.\n\n\
         Redaction is best effort. Decision context can include commands, task text, tool output, \
         and candidate descriptions. Review what you send and any exports before sharing them. \
         See [decision advice and logging](/docs/permissions/#decision-engine-advice).\n\n\
         #### `decisions.features`\n\n\
         `off` disables the feature. `shadow` collects predictions without applying them. \
         `advise` adds caution or suggestions. `enforce` applies only the feature-specific behavior \
         listed below, never permission grants or relaxed executor restrictions. \
         Unsupported modes are configuration errors. Passive features are suppressed in YOLO.\n\n\
         | Feature | Default | Supported modes | Behavior beyond shadow |\n\
         |---------|---------|-----------------|------------------------|\n",
        timeout_ms = DecisionsConfig::default().timeout_ms,
    )
    .unwrap();
    for feature in DecisionFeatures::ALL {
        let modes: Vec<String> = feature
            .modes
            .iter()
            .map(|mode| format!("`{}`", mode.as_str()))
            .collect();
        let see_also = if feature.name == SHELL_DURATION_FEATURE {
            SHELL_DURATION_LINK
        } else {
            ""
        };
        writeln!(
            out,
            "| `{}` | `{}` | {} | {}{see_also} |",
            feature.name,
            FeatureMode::default().as_str(),
            modes.join(", "),
            feature.description,
        )
        .unwrap();
    }
    out.push_str(
        "\n#### `decisions.thresholds`\n\n\
         Flag thresholds trigger at or above the configured value. Goal prescreening uses an \
         at-or-below comparison. Content screening requires both signals in a sampled chunk. \
         After content is flagged, Auto uses 75% of `auto_flag` for upload and credential flags.\n\n",
    );
    write_table(out, DecisionThresholds::FIELDS);
    out.push_str(
        "\n#### Shell duration\n\n\
         `shell_duration` applies only to eligible local native shell calls, not remote workspaces \
         or managed sandboxes. Measured exact-command history takes priority over command-family \
         history, and both take priority over a model estimate. Timeouts, cancellations, and failures \
         are recorded separately from completed latency samples. History is separate from the \
         opt-in decision log, so `log = false` does not disable duration observations.\n\n\
         A model estimate scores a command on four levels: exits at once, runs for seconds, \
         runs for minutes, or runs until stopped. It counts as running until stopped when the \
         `endless` answer or the probability of that level reaches `shell_endless`. Otherwise \
         the first of these bounds whose probability reaches `shell_duration` decides: at least \
         minutes, at once, then at most seconds. With no bound reached, the call runs without an \
         estimate.\n\n\
         Measured runs are labeled by fixed boundaries. A run within 1 second exited at once, a \
         run under 120 seconds took seconds, and a longer run took minutes. These boundaries \
         stay the same whatever `agent.shell_async_threshold_secs` is. When neither the command \
         nor its family has enough history, the request lists up to four related families as \
         `earlier_runs`, drawn from runs in the same project and working directory. The \
         command's own family comes first, then families with the same program, each with how \
         long its runs took.\n\n\
         In `advise`, estimates and warnings leave execution unchanged. In `enforce`, an omitted \
         `timeoutSec` may receive a default based on 1.5 times estimated p90, bounded by the \
         tool schema's default and maximum. An explicit timeout is never changed. An endless \
         prediction gives caution only and does not remove the execution deadline.\n\n\
         With `agent.shell_execution = \"auto\"`, Enforce estimates can select synchronous or \
         asynchronous delivery at admission, bounded by the effective timeout and \
         `agent.shell_async_threshold_secs`. Explicit sync/async settings still win. Elapsed \
         runtime never promotes a synchronous call to asynchronous delivery. An admission \
         receipt is not completion or success.\n\n\
         #### Question overrides\n\n\
         Only the permission question set currently supports a user-global file override: \
         `~/.config/caudra/decisions/permission.json`. It must be a regular JSON file no larger \
         than 64 KiB, retain all required question IDs as `noul`, and pass question validation. \
         It is read when a base URL is set and permission advice or Auto screening is enabled. \
         Projects cannot supply this override. Other feature question sets have no file override.\n\n",
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

const CONFIG_FILES_INTRO: &str = "## Config files\n\n\
    `caudra.toml` holds the settings that only you write. A file stays separate from it when \
    Caudra also writes the file, when the file decides where credentials or processes go, or when \
    it has its own rules for trust, errors, or privacy.\n\n\
    | File | Scope | Holds | Kept separate because | Reference |\n\
    |------|-------|-------|-----------------------|-----------|\n";
const CONFIG_FILES_COMMANDS: &str = "\n`caudra config files` lists where each file lives on your \
    machine and whether it is there. `caudra config example FILE` prints the reference of a TOML \
    file, such as `caudra config example mcp`. See [`caudra config`](/docs/cli/#caudra-config).\n";

fn write_config_files_section(out: &mut String) {
    out.push_str(CONFIG_FILES_INTRO);
    for file in CONFIG_FILES {
        let scopes: Vec<&str> = file.scopes.iter().map(|scope| scope.label()).collect();
        let needs = file
            .feature
            .map(|feature| format!(" (needs `experimental.{}`)", feature.key()))
            .unwrap_or_default();
        let reference = if file.example.is_some() {
            format!(
                "`{}`, [{name}](/docs/{name})",
                file.example_command(),
                name = example_file_name(file)
            )
        } else {
            "-".to_owned()
        };
        writeln!(
            out,
            "| [`{name}`]({docs}) | {scopes} | {holds}{needs} | {why} | {reference} |",
            name = file.name,
            docs = file.docs,
            scopes = scopes.join(", "),
            holds = file.holds,
            why = file.separate_because,
        )
        .unwrap();
    }
    out.push_str(CONFIG_FILES_COMMANDS);
}

/// Every file the global config directory can hold, in catalog order.
fn config_dir_files() -> String {
    let names: Vec<String> = CONFIG_FILES
        .iter()
        .filter(|file| file.scopes.contains(&Scope::Global))
        .map(|file| format!("`{}`", file.name))
        .collect();
    join_and(&names)
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

Settings go in `caudra.toml`. It has two places, and both are optional:

- **Global**: `~/.config/caudra/caudra.toml`
- **Project**: `.caudra/caudra.toml` (relative to your working directory)

When both exist, project settings override global ones field by field. A few settings are global-only, and their descriptions say so. `/reload` reads both files again, except for the [experimental switches](#experimental-features), which apply from startup.

Settings apply in this order, and each layer overrides the ones before it:

1. Built-in defaults
2. Global `caudra.toml`
3. Global `init.lua`, only with Lua plugins turned on
4. Project `.caudra/caudra.toml`
5. Project `.caudra/init.lua`, only with Lua plugins turned on
6. Command-line flags

Remote sessions load only the client's global configuration. They skip both project layers, project environment files, and project MCP configuration. Remote project context uses a bounded declarative asset manifest instead. See [Remote Workspaces](/docs/remote-workspaces/#project-context-and-trust).
"
    )
    .unwrap();
    write_config_files_section(&mut out);
    writeln!(
        out,
        "
## Example

```toml
[ui]
splash_animation = true
mouse_scroll_lines = {mouse_scroll}
theme = \"tokyonight\"

[ui.tool_output_lines]
bash = {tol_bash}
read = {tol_read}

[agent]
max_output_lines = {max_output_lines}

[provider]
default_model = \"anthropic/claude-sonnet-4-6\"
allowed_models = [\"anthropic/*\", \"openai/gpt-5\"]
excluded_models = [\"*/*-preview\"]

[storage]
max_log_files = {max_log_files}

[plugins.bash]
timeout_secs = 180

[plugins.index]
max_file_size_mb = 4
```

All fields are optional. A file may start with `version = {version}`, and a file without it counts as version {version}. Typos in field names and values of the wrong type cause an error right away, with the file and line.

For every setting in one file, with its type, default, and description, run [`caudra config example`](/docs/cli/#caudra-config) or download [{example_file}](/docs/{example_file}).

`provider.allowed_models` is a list of glob patterns for qualified `provider/model-id` specs. `*` also matches `/`, so `opencode/*` includes nested model IDs. When the list is empty or omitted, every model is allowed. `provider.excluded_models` removes matching models after that, so exclusions always win. A project list replaces the matching global list. Omit it to inherit, or use `[]` to clear it. The policy applies to selectors, CLI and API model changes, delegation, and `caudra models`.
",
        mouse_scroll = DEFAULT_MOUSE_SCROLL_LINES + 2,
        tol_bash = ToolOutputLines::DEFAULT.bash + 3,
        tol_read = ToolOutputLines::DEFAULT.read + 2,
        max_output_lines = DEFAULT_MAX_OUTPUT_LINES + 1000,
        max_log_files = DEFAULT_MAX_LOG_FILES / 2,
        version = CONFIG_VERSION,
        example_file = example_file_name(&files::CAUDRA),
    )
    .unwrap();
    write_experimental_section(&mut out);
    write_migration_section(&mut out);

    writeln!(out, "## Full Reference\n").unwrap();
    writeln!(out, "### Top-level\n").unwrap();
    write_table(&mut out, TOP_LEVEL_FIELDS);
    writeln!(out).unwrap();

    write_section(&mut out, "ui", UiConfig::FIELDS);
    write_theme_section(&mut out);
    write_update_check_section(&mut out);
    write_tool_output_section(&mut out);
    write_section(&mut out, "agent", AgentConfig::FIELDS);
    write_section(&mut out, "agent.messaging", MessagingConfig::FIELDS);
    write_steering_section(&mut out);
    write_section(&mut out, "provider", ProviderConfig::FIELDS);
    write_section(&mut out, "storage", StorageConfig::FIELDS);
    write_retention_section(&mut out);
    write_snapshots_section(&mut out);
    write_telemetry_section(&mut out);
    write_worktrees_section(&mut out);
    write_decisions_section(&mut out);

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
         This table is for bundled plugins only, and it works without Lua. Your own plugins go \
         in `~/.config/caudra/lua/` and need `lua_plugins` turned on. See \
         [Plugins](/docs/plugins/).\n"
    )
    .unwrap();
    writeln!(
        out,
        "\
```toml
[plugins.bash]
timeout_secs = 180

[plugins.websearch]
enabled = false
```\n"
    )
    .unwrap();

    write_plugin_options(&mut out);

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

Config holds {config_files}. State holds sessions, auth tokens, memories, plans, model-job bindings, sandbox lifecycle records and transfer recovery journals. The install script puts the binary under `%LOCALAPPDATA%\\caudra` on Windows; that is separate from these runtime dirs.

Scratch holds work that belongs outside your project, such as a file the model writes while thinking or a temporary a command leaves behind. Each project gets its own subdirectory, named by the project directory plus a three-word phrase derived from its path, as in `caudra-heroic-easy-grouse`. Two checkouts sharing a name get different phrases, so they cannot overwrite each other. The phrase is derived rather than drawn at random, so a project returns to the same directory on every run. Caudra creates it at startup and points `TMPDIR`, `TMP`, and `TEMP` at it, so every command Caudra runs puts its own temporary files there instead of the shared temp root. The model is told the path, and writing anywhere under the scratch root needs no approval. Paths beside the root still ask.

The choice of subdirectory is fixed when Caudra starts. `/cd` moves the project without moving it, which is why approval covers the whole root rather than one project's share of it. Directories are owner-only, and Caudra refuses to use one that turns out to be a symlink. Nothing sweeps them, so they live as long as your system keeps its temp root.

State that belongs to one project sits under `…/state/caudra/projects/<project-id>/`, where the id is the project directory name plus a hash of its path. Memory notes and plan-mode documents both live there, so removing that directory clears everything Caudra kept for the project.

Development builds compiled with debug assertions use `caudra-debug` for every platform directory. This keeps global config, sessions, auth, logs, and caches separate from release builds. Per-project `.caudra/` directories remain shared.

Set `CAUDRA_NAMESPACE` to choose the directory name yourself instead of letting the build profile pick it. The value is a single directory name, so `CAUDRA_NAMESPACE=caudra-review` reads and writes `~/.config/caudra-review/`, `~/.local/state/caudra-review/`, and the rest. Use it to give a run its own config and session store, or to point a debug build at your release directories. A value that cannot be a directory name, such as one holding a path separator or `..`, stops Caudra with an error rather than falling back. An empty value counts as unset. Per-project `.caudra/` directories are unaffected.

## Config file versions

Each TOML config file takes a top-level `version`, and every format is at version 1. Where the key is optional, a file without it counts as version 1. A build that finds a newer version than it reads refuses the file instead of guessing what it means. For a file with an optional key, the error says the file needs a newer Caudra.

| File | `version` | Newer than this build reads |
|------|-----------|-----------------------------|
| `caudra.toml` | Optional | Caudra stops with an error |
| `permissions.toml` | Optional | Fails closed. Tool calls are denied until the file is fixed |
| `mcp.toml` | Optional | Servers from that file do not start, and Caudra shows the error |
| `providers.toml` | Optional | Caudra stops with an error |
| `plugin.toml` | Optional | Every permission of that plugin is denied |
| `workcell.toml` | Required | Rejected with an error |
| `sandboxes.toml` | Required | Rejected with an error |

Caudra writes `version = 1` whenever it saves `providers.toml`. `init.lua`, which runs only with Lua plugins turned on, has no version because it is a script. To share one `init.lua` across releases, branch on [`caudra.version()`](/docs/lua-api/#caudra-version).

## Personal Instructions

On top of the project instruction files Caudra loads from the git root down to the cwd (`AGENTS.md`, `CLAUDE.md`, and friends; see [Context](/docs/context/#instruction-files)), you can add:

- `AGENTS.local.md` in any of those project directories for per-directory preferences (gitignored)
- `~/.config/caudra/AGENTS.md` for preferences that apply to all projects

All of these are added to the system prompt at the start of every session.

## Memory

The `memory` tool and `/memory` command store small Markdown notes under the state directory, scoped per project:

`…/state/caudra/projects/<project-id>/memories/`

(Linux/macOS: `~/.local/state/caudra/…`; Windows: `%APPDATA%\\caudra\\…`). Use them for non-obvious gotchas and decisions that should survive across sessions. They are separate from skills and from `AGENTS.md`.

Related pages: [Skills](/docs/skills/), [CLI](/docs/cli/), [Providers](/docs/providers/#providers-toml).",
        config_files = config_dir_files(),
    )
    .unwrap();

    out
}

#[cfg(test)]
mod tests {
    use caudra_config::config_file::GlobalConfigFile;
    use caudra_config::{
        AgentConfig, ConfigValue, DecisionsConfig, Feature, SteeringConfig,
        decisions::RawDecisionsConfig,
    };
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::{
        generate, write_decisions_section, write_experimental_section, write_steering_section,
    };

    const MODEL: &str = "provider/model";
    const HEADING: &str = "### `agent.steering`";
    const PROMPT_ROW: &str = "| `prompt` | string | unset |";
    const RULE_COUNT: usize = 9;
    const TOML_FENCE: &str = "```toml\n";
    const FENCE_END: &str = "```";

    #[test]
    fn experimental_reference_lists_every_switch_off() {
        let mut reference = String::new();
        write_experimental_section(&mut reference);
        for feature in Feature::ALL {
            let row = format!("| `{}` | `false` |", feature.key());
            assert!(
                reference.lines().any(|line| line.starts_with(&row)),
                "{row}"
            );
        }
    }

    #[test]
    fn every_toml_example_is_a_valid_global_config() {
        let page = generate();
        let examples: Vec<&str> = page
            .split(TOML_FENCE)
            .skip(1)
            .filter_map(|rest| rest.split_once(FENCE_END).map(|(body, _)| body))
            .collect();
        assert!(!examples.is_empty());
        for example in examples {
            if let Err(error) = GlobalConfigFile::parse(example) {
                panic!("{error}\n{example}");
            }
        }
    }

    #[test_case("allow_remote", DecisionsConfig::default().allow_remote)]
    #[test_case("allow_http", DecisionsConfig::default().allow_http)]
    fn decision_reference_covers_transport_defaults(field: &str, default: bool) {
        let mut reference = String::new();
        write_decisions_section(&mut reference);
        let prefix = format!("| `{field}` | boolean | `{default}` |");
        assert!(reference.lines().any(|line| line.starts_with(&prefix)));
    }

    #[test_case("off")]
    #[test_case("shadow")]
    #[test_case("advise")]
    #[test_case("enforce")]
    fn decision_reference_modes_match_validation(mode: &str) {
        let defaults = serde_json::to_value(DecisionsConfig::default().features).unwrap();
        let mut reference = String::new();
        write_decisions_section(&mut reference);
        for (feature, default) in defaults.as_object().unwrap() {
            let row = reference
                .lines()
                .find(|line| line.starts_with(&format!("| `{feature}` |")))
                .unwrap();
            assert_eq!(
                row.split('|').nth(2).unwrap().trim(),
                format!("`{}`", default.as_str().unwrap())
            );
            let documented = row
                .split('|')
                .nth(3)
                .unwrap()
                .contains(&format!("`{mode}`"));
            let raw: RawDecisionsConfig =
                serde_json::from_value(json!({"features": {feature: mode}})).unwrap();
            assert_eq!(documented, raw.resolve(None).is_ok(), "{feature}: {mode}");
        }
    }

    #[test]
    fn decision_reference_covers_threshold_defaults() {
        let defaults = serde_json::to_value(DecisionsConfig::default().thresholds).unwrap();
        let mut reference = String::new();
        write_decisions_section(&mut reference);
        for (field, value) in defaults.as_object().unwrap() {
            let default = if value.is_null() {
                ConfigValue::Unset.format_default()
            } else {
                format!("`{value}`")
            };
            let prefix = format!("| `{field}` | float | {default} |");
            assert!(
                reference.lines().any(|line| line.starts_with(&prefix)),
                "{field}"
            );
        }
    }

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
            assert!(
                reference
                    .lines()
                    .any(|line| line.starts_with(&format!("| `{rule}` |")))
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
