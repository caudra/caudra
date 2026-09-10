+++
title = "Configuration"
weight = 2
[extra]
group = "Getting Started"
+++

# Configuration

Settings go in `init.lua`, a Lua script that calls `caudra.setup()`. Same language as plugins.

Two places, both optional:

- **Global**: `~/.config/caudra/init.lua`
- **Project**: `.caudra/init.lua` (relative to your working directory)

When both exist, project settings override global ones. Neither file is required.

## Example

```lua
caudra.setup({
    ui = {
        splash_animation = true,
        mouse_scroll_lines = 5,
        theme = "tokyonight",
        tool_output_lines = {
            bash = 8,
            read = 5,
        },
    },
    agent = {
        max_output_lines = 3000,
    },
    provider = {
        default_model = "anthropic/claude-sonnet-4-6",
        allowed_models = { "anthropic/*", "openai/gpt-5" },
        excluded_models = { "*/*-preview" },
    },

    storage = {
        max_log_files = 5,
    },
    plugins = {
        bash = { timeout_secs = 180 },
        index = { max_file_size_mb = 4 },
    },
})
```

All fields are optional. Typos in field names cause an error right away.

`provider.allowed_models` is a list of glob patterns for qualified `provider/model-id` specs. `*` also matches `/`, so `opencode/*` includes nested model IDs. When the list is empty or omitted, every model is allowed. `provider.excluded_models` removes matching models after that, so exclusions always win. A project list replaces the matching global list; omit it to inherit or use `{}` to clear it. The policy applies to selectors, CLI and API model changes, delegation, and `caudra models`.

`caudra.setup()` can only be called once per init.lua.

## Full Reference

### Top-level

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `always_yolo` | bool | `false` | Start every session with YOLO mode (skip permission prompts, deny rules still apply) |
| `always_fast` | bool | `false` | Start every session with Anthropic fast mode (Opus only; ignored otherwise) |
| `always_thinking` | bool \| string | `false` | Start every session with extended thinking (true/"adaptive", "off", an effort level ("minimal" to "max"), or a token budget) |

### `ui`

| Field | Type | Default | Env | Min | Description |
|-------|------|---------|-----|-----|-------------|
| `splash_animation` | bool | `true` | - | - | Show splash animation on startup |
| `scrollbar` | bool | `true` | - | - | Show vertical scrollbar in scrollable areas |
| `notifications` | string | `auto` | - | - | Terminal notification method: auto, osc9, bell, or off |
| `math` | string | `unicode` | - | - | How LaTeX maths renders: unicode (approximate with Unicode) or raw (show the LaTeX source) |
| `mermaid` | string | `unicode` | - | - | How mermaid flowcharts render: unicode (draw them with box-drawing characters) or off (leave the fence as code) |
| `flash_duration_ms` | u64 | `1500` | - | - | Duration of flash messages (ms) |
| `which_key_delay_ms` | u64 | `250` | - | - | How long Ctrl+X waits before listing the chords it can still reach (ms). 0 shows the list at once |
| `typewriter_ms_per_char` | u64 | `4` | - | - | Typewriter effect speed (ms/char) |
| `mouse_scroll_lines` | u32 | `3` | - | 1 | Lines per mouse wheel scroll |
| `max_input_lines` | u32 | `20` | - | 1 | Maximum visible input lines |
| `show_thinking` | bool | `true` | - | - | Show full model reasoning live and persisted. Turn this off to start every reasoning block collapsed behind a Thinking or Thought header that can be clicked to expand |
| `show_reminders` | bool | `true` | - | - | Show the messages Caudra writes into the conversation on your behalf: standing reminders, goal check-ins, nudges, and continuations. Each is one dim row that expands on click to the exact text the model was sent. Turn this off to keep the transcript to the conversation alone |
| `clock_format` | String | `system` | - | - | Clock format for timestamps: "12h", "24h", or "system" (follow the OS preference, 24h when unknown) |
| `update_check` | bool | `false` | `CAUDRA_ENABLE_UPDATE_CHECK` | - | Ask GitHub for the latest release on startup and show it in the splash. Off by default, so Caudra makes no such request unless you turn this on |

### `ui.theme`

Name of the color theme to load at startup, overriding the theme you last picked interactively. If unset, Caudra keeps your last selection, which starts out as `opencode`. An unknown name is ignored with a warning.

Available themes: `ayu_dark`, `ayu_light`, `ayu_mirage`, `carbonfox`, `catppuccin_frappe`, `catppuccin_latte`, `catppuccin_macchiato`, `catppuccin_mocha`, `dark_daltonized`, `dracula`, `everforest_dark`, `fleet_dark`, `github_dark`, `gruvbox`, `gruvbox_light`, `kanagawa`, `kanagawa_ink`, `kanagawa_plum`, `material_darker`, `monokai_pro`, `night_owl`, `nightfox`, `nord`, `onedark`, `opencode`, `opencode_light`, `rose_pine`, `rose_pine_dawn`, `rose_pine_midnight`, `rose_pine_moon`, `solarized_dark`, `solarized_light`, `tokyonight`, `vscode_dark_plus`, `zenburn`.

You can add your own themes too. Drop a `<name>.toml` file into `themes/` inside your Caudra config directory, for example `~/.config/caudra/themes/`. If it reuses a built-in name, yours wins.

Themes use 24-bit colors, but not every terminal can show them. Caudra checks the environment, terminfo, and the terminal itself, and when truecolor is missing it quietly falls back to the closest of the 256 classic terminal colors. If detection gets it wrong, set `CAUDRA_TRUECOLOR=1` to force truecolor or `CAUDRA_TRUECOLOR=0` to force the fallback.

Some themes ship as a light and dark pair: `ayu_dark` and `ayu_light`, `catppuccin_mocha` and `catppuccin_latte`, `gruvbox` and `gruvbox_light`, `opencode` and `opencode_light`, `rose_pine` and `rose_pine_dawn`, `solarized_dark` and `solarized_light`. Choosing either half makes Caudra ask the terminal for its background color and show the half that matches. Themes outside these pairs stay as you left them.

Caudra asks again every ten minutes, and also when the terminal regains focus or changes size, so reattaching a multiplexer to another terminal updates the theme. Following the terminal only changes the running session, and the theme you saved from `/theme` stays saved. Terminals that do not report a background color are asked a few times and then left alone.

### `ui.theme_light`

Light half to use in place of the one from the pairing table, or to give a theme that has no pair. `ui.theme` becomes the dark half:

```lua
caudra.setup({ ui = { theme = "tokyonight", theme_light = "catppuccin_latte" } })
```

Leave `ui.theme` unset to pair the light theme with whatever you last picked from `/theme`.

### `ui.update_check`

When on, Caudra asks the GitHub releases API for the latest version once at startup and shows it in the splash when yours is older. The request carries a `caudra` user agent and nothing else: no session id, no machine id, not even your current version.

It is off by default, so a normal run reaches only the model provider you configured. Set `CAUDRA_ENABLE_UPDATE_CHECK=1` to turn it on for a single run, or `CAUDRA_ENABLE_UPDATE_CHECK=0` to turn it off when your config has it on. The `caudra update` command always checks, because that is what you asked it to do.

### `ui.tool_output_lines`

How many lines of output an open card shows per tool before it says how many it is holding back. Clicking the card shows all of it regardless. All values are `usize` with a minimum of 1.

| Field | Default | Tools |
|-------|---------|-------|
| `bash` | 5 | `shell` |
| `python_execution` | 5 | `python_execution` |
| `task` | 5 | `task` |
| `index` | 3 | `file_index`, `code_map`, `code_context`, `code_refs`, `code_impact`, `code_expand` |
| `grep` | 3 | `file_grep`, `file_glob` |
| `read` | 3 | `file_read` |
| `write` | 7 | `file_write`, `file_edit`, `file_apply_patch`, `image_generate`, `memory` |
| `web` | 3 | `webfetch`, `websearch` |
| `other` | 3 | `batch`, `execution_environment`, `question`, `skill`, `todo_write`, `tool_output`, `view_image`, `workflow` |

### `agent`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `system_prompt_profile` | String | `builtin` | - | Default user system prompt profile from the system-prompts config directory |
| `max_output_bytes` | usize | `51200` | 1024 | Host-enforced default max tool-result size (bytes) |
| `max_output_lines` | usize | `2000` | 10 | Host-enforced default max tool-result lines |
| `max_continuation_turns` | u32 | `3` | 1 | Max automatic continuation turns |
| `compaction_buffer` | u32 \| string | `20%, or 10% when the model's window excludes output` | - | Context reserved for compaction: token count or percent of the context window (e.g. "20%") |
| `compaction_instructions` | String | `none` | - | Extra instructions appended to the compaction summary prompt |
| `post_compaction_instructions` | String | `none` | - | Extra instructions the agent receives after any compaction (e.g. re-read plan.md) |
| `generate_titles` | bool | `true` | - | Name a new session by summarizing its first prompt with a small model |
| `stale_read_check` | bool | `true` | - | Block a write to a file that changed on disk since it was read, and point a failed edit or patch at the change |
| `shell_output_filter` | bool | `true` | - | Filter completed model-facing shell output with built-in rules |
| `defer_builtin_tools` | string | `auto` | - | When the on-demand built-in tools start outside the request array: `auto` defers them for a Fast or unclassified model and declares them upfront for Balanced and Best, `always` defers for every model, `never` declares them upfront |
| `disabled_tools` | string[] | `[]` | - | Tools to withhold from the model: built-in names, `server.tool`, or `server.*` for a whole MCP server. A project list extends the global one |

### `provider`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `default_model` | String | `none` | - | Default model identifier (e.g. `anthropic/claude-sonnet-4-6`) |
| `allowed_models` | string[] | `[]` | - | Glob patterns for permitted qualified model specs; empty permits all models |
| `excluded_models` | string[] | `[]` | - | Glob patterns for excluded qualified model specs; exclusions take precedence |
| `connect_timeout_secs` | u64 | `10` | 1 | HTTP connect timeout (seconds) |
| `stream_timeout_secs` | u64 | `300` | 10 | Longest the server may send nothing before the request is abandoned (seconds) |

### `storage`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_log_bytes_mb` | u64 | `200` | 1 | Max total log size (MB) |
| `max_log_files` | u32 | `10` | 1 | Max number of log files to keep |
| `log_level` | string | `info` | - | Minimum severity written to the log file: trace, debug, info, warn, or error. RUST_LOG overrides it |
| `input_history_size` | usize | `100` | 10 | Number of input history entries to retain |
| `ephemeral` | bool | `false` | - | Store session data in a temporary directory removed when Caudra exits |

### `storage.retention`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `group_by` | string | `directory` | Evaluate policies per working directory (`directory`) or across every session (`none`) |
| `sweep_interval_hours` | u64 | `24` | Hours between background sweeps. `0` disables the sweep; `caudra storage` commands still work |
| `trim` | table | `{ keep_last = 20, keep_within = "90d" }` | Sessions outside this policy lose snapshots, tool output files, archives, and large rich outputs but stay resumable |
| `forget` | table | `{}` | Sessions outside this policy are deleted. Empty means never delete automatically |

`trim` and `forget` are keep policies in `restic forget` terms: `keep_last`, `keep_hourly`, `keep_daily`, `keep_weekly`, `keep_monthly`, `keep_yearly` take a count, and `keep_within` plus `keep_within_hourly` through `keep_within_yearly` take a duration such as `"90d"` or `"2y5m7d3h"`. A session is kept when any rule matches. An empty `forget` policy disables automatic deletion. See [Sessions](/docs/sessions/#retention) for what each tier keeps and how the sweep runs.

### `telemetry`

| Field | Type | Default | Env | Description |
|-------|------|---------|-----|-------------|
| `enabled` | bool | `false` | `CAUDRA_ENABLE_TELEMETRY` | Master switch |
| `metrics_exporter` | string | `none` | `OTEL_METRICS_EXPORTER` | Where metrics go: `otlp`, `console`, `none`, or a comma-separated mix |
| `logs_exporter` | string | `none` | `OTEL_LOGS_EXPORTER` | Where events go: `otlp`, `console`, `none`, or a comma-separated mix |
| `protocol` | string | `-` | `OTEL_EXPORTER_OTLP_PROTOCOL` | OTLP protocol: `grpc`, `http/protobuf`, or `http/json`. Required when an exporter is `otlp` |
| `endpoint` | string | `-` | `OTEL_EXPORTER_OTLP_ENDPOINT` | Collector endpoint. HTTP appends `/v1/metrics` and `/v1/logs` |
| `headers` | table | `{}` | `OTEL_EXPORTER_OTLP_HEADERS` | Extra headers sent with every export |
| `timeout_ms` | integer | `10000` | `OTEL_EXPORTER_OTLP_TIMEOUT` | Per-export request timeout (ms) |
| `compression` | string | `none` | `OTEL_EXPORTER_OTLP_COMPRESSION` | Payload compression: `gzip` or `none` |
| `metrics_protocol` | string | `-` | `OTEL_EXPORTER_OTLP_METRICS_PROTOCOL` | Metrics-only protocol override |
| `metrics_endpoint` | string | `-` | `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | Metrics-only endpoint, used verbatim with no path appended |
| `metrics_headers` | table | `{}` | `OTEL_EXPORTER_OTLP_METRICS_HEADERS` | Metrics-only headers, merged over `headers` |
| `metrics_timeout_ms` | integer | `-` | `OTEL_EXPORTER_OTLP_METRICS_TIMEOUT` | Metrics-only request timeout (ms) |
| `logs_protocol` | string | `-` | `OTEL_EXPORTER_OTLP_LOGS_PROTOCOL` | Logs-only protocol override |
| `logs_endpoint` | string | `-` | `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | Logs-only endpoint, used verbatim with no path appended |
| `logs_headers` | table | `{}` | `OTEL_EXPORTER_OTLP_LOGS_HEADERS` | Logs-only headers, merged over `headers` |
| `logs_timeout_ms` | integer | `-` | `OTEL_EXPORTER_OTLP_LOGS_TIMEOUT` | Logs-only request timeout (ms) |
| `metrics_interval_ms` | integer | `60000` | `OTEL_METRIC_EXPORT_INTERVAL` | How often metrics are exported (ms) |
| `metrics_export_timeout_ms` | integer | `30000` | `OTEL_METRIC_EXPORT_TIMEOUT` | Deadline for one metrics export, retries included (ms) |
| `logs_interval_ms` | integer | `5000` | `OTEL_LOGS_EXPORT_INTERVAL`, `OTEL_BLRP_SCHEDULE_DELAY` | How often queued events are flushed (ms) |
| `logs_max_queue_size` | integer | `2048` | `OTEL_BLRP_MAX_QUEUE_SIZE` | Event queue capacity. Events are dropped and counted when it is full |
| `logs_max_export_batch_size` | integer | `512` | `OTEL_BLRP_MAX_EXPORT_BATCH_SIZE` | Maximum events per export request |
| `logs_export_timeout_ms` | integer | `30000` | `OTEL_BLRP_EXPORT_TIMEOUT` | Deadline for one events export, retries included (ms) |
| `metrics_temporality` | string | `delta` | `OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE` | Metric temporality: `delta` or `cumulative` |
| `service_name` | string | `caudra` | `OTEL_SERVICE_NAME` | `service.name` on the exported resource |
| `resource_attributes` | table | `{}` | `OTEL_RESOURCE_ATTRIBUTES` | Extra resource attributes, your place for team or environment labels |
| `metrics_include_session_id` | bool | `true` | `OTEL_METRICS_INCLUDE_SESSION_ID` | Attach `session.id` to metrics. Turn off to keep metric cardinality low |
| `metrics_include_version` | bool | `false` | `OTEL_METRICS_INCLUDE_VERSION` | Attach `app.version` to metrics |
| `log_user_prompts` | bool | `false` | `OTEL_LOG_USER_PROMPTS` | Include prompt text in `caudra.user_prompt` events. Off by default |
| `log_tool_details` | bool | `false` | `OTEL_LOG_TOOL_DETAILS` | Include tool input in `caudra.tool_result` events. Off by default |
| `content_max_length` | integer | `10240` | `CAUDRA_OTEL_CONTENT_MAX_LENGTH` | Character cap on any logged prompt or tool input |

Every field also has an environment variable, shown in the Env column, and the variable wins. See [Telemetry](/docs/telemetry/) for the full picture.

## Plugins

The `plugins` table turns bundled features and plugins on or off and passes options to them. All bundled features are on by default. Set `enabled = false` to turn one off.

Each feature checks its own options at startup. A typo, a wrong type, or an unknown plugin name gives you a clear error right away.

`enabled = false` turns off the tools that key produced, under the names they are registered with today, so `plugins.bash` turns off `shell` and `plugins.edit` turns off `file_edit` and `file_apply_patch`. To name a tool directly, use `agent.disabled_tools`, described in [Disabling tools](/docs/tools/#disabling-tools).

The edit plugin's extra tools are options too: `plugins.edit = { multiedit = false, insert_lines = true }`.

This table is for bundled plugins only. Your own plugins go in `~/.config/caudra/lua/`, see [Plugins](/docs/plugins/).

```lua
caudra.setup({
    plugins = {
        bash = { timeout_secs = 180 },
        websearch = { enabled = false },
    },
})
```

### `plugins.index`

`file_index` executes as a native Workcell tool, and this table keeps the `plugins.index` key it was configured under. The file-size limit accepts 1 through 16 MiB to bound parser memory and work.

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_file_size_mb` | integer | `2` | 1 | Refuse to index files larger than this many MiB (maximum 16). |

### `plugins.skill`

`skill` executes as a native Caudra tool. This table keeps its existing configuration key.

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `plugin_dev` | boolean | `false` | - | Offer the builtin caudra-plugin-dev skill for writing caudra plugins. |
| `workflow_dev` | boolean | `true` | - | Offer the builtin caudra-workflow-dev skill for writing and running workflows. |

### `plugins.task`

`task` executes as a native Caudra tool. This table keeps its existing configuration key.

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_concurrent` | integer | `8` | 1 | Max concurrently running subagents. |

## Validation

If a value is below its minimum, Caudra shows a `ConfigError` with the field name, value, and minimum.

## Directory layout

Caudra follows platform directory conventions. On Linux and macOS that is XDG. On Windows, config, data, state, and logs all live under Roaming AppData (Windows has no separate state dir in this layout).

| Purpose | Linux / macOS | Windows |
|---------|---------------|---------|
| Config | `~/.config/caudra/` | `%APPDATA%\caudra\` |
| Data | `~/.local/share/caudra/` | `%APPDATA%\caudra\` |
| State | `~/.local/state/caudra/` | `%APPDATA%\caudra\` |
| Logs | `~/.local/logs/caudra/` | `%APPDATA%\caudra\` |
| Cache | `~/.cache/caudra/` | `%LOCALAPPDATA%\caudra\` |

Config holds `init.lua`, `permissions.toml`, `mcp.toml`, `providers.toml`, and `commands/`. State holds sessions, auth tokens, memories, plans, and model-purpose bindings. The install script puts the binary under `%LOCALAPPDATA%\caudra` on Windows; that is separate from these runtime dirs.

State that belongs to one project sits under `…/state/caudra/projects/<project-id>/`, where the id is the project directory name plus a hash of its path. Memory notes and plan-mode documents both live there, so removing that directory clears everything Caudra kept for the project.

Development builds compiled with debug assertions use `caudra-debug` for every platform directory. This keeps global config, sessions, auth, logs, and caches separate from release builds. Per-project `.caudra/` directories remain shared.

## Personal Instructions

On top of the project instruction files Caudra loads from the git root down to the cwd (`AGENTS.md`, `CLAUDE.md`, and friends; see [Context](/docs/context/#instruction-files)), you can add:

- `AGENTS.local.md` in any of those project directories for per-directory preferences (gitignored)
- `~/.config/caudra/AGENTS.md` for preferences that apply to all projects

All of these are added to the system prompt at the start of every session.

## Memory

The `memory` tool and `/memory` command store small Markdown notes under the state directory, scoped per project:

`…/state/caudra/projects/<project-id>/memories/`

(Linux/macOS: `~/.local/state/caudra/…`; Windows: `%APPDATA%\caudra\…`). Use them for non-obvious gotchas and decisions that should survive across sessions. They are separate from skills and from `AGENTS.md`.

Related pages: [Skills](/docs/skills/), [CLI](/docs/cli/), [Providers](/docs/providers/#providers-toml).
