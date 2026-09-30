+++
title = "Configuration"
weight = 2
[extra]
group = "Getting Started"
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

## Config files

`caudra.toml` holds the settings that only you write. A file stays separate from it when Caudra also writes the file, when the file decides where credentials or processes go, or when it has its own rules for trust, errors, or privacy.

| File | Scope | Holds | Kept separate because | Reference |
|------|-------|-------|-----------------------|-----------|
| [`caudra.toml`](/docs/configuration/) | global, project | settings, and in the global file the [experimental] switches | It is the main file, and only you write it | `caudra config example caudra`, [caudra.example.toml](/docs/caudra.example.toml) |
| [`permissions.toml`](/docs/permissions/#toml-policy) | global, project | permission rules for tools and MCP servers | It has its own error rule: a file that fails to load denies every tool call | `caudra config example permissions`, [permissions.example.toml](/docs/permissions.example.toml) |
| [`mcp.toml`](/docs/mcp/) | global, project | MCP servers | Caudra writes it when `/mcp` turns a server on or off, and it starts processes | `caudra config example mcp`, [mcp.example.toml](/docs/mcp.example.toml) |
| [`providers.toml`](/docs/providers/#providers-toml) | global | model providers and their models | `caudra auth login` and `caudra auth logout` write it, and it can hold API keys | `caudra config example providers`, [providers.example.toml](/docs/providers.example.toml) |
| [`workcell.toml`](/docs/remote-workspaces/#configure-a-profile) | global | profiles for direct remote Workcell connections (needs `experimental.remote_workcell`) | It decides where credentials go, so it has to be a private file | `caudra config example workcell`, [workcell.example.toml](/docs/workcell.example.toml) |
| [`sandboxes.toml`](/docs/sandboxes/#configuration-schema) | global | managed sandbox providers, networks, transfers, and profiles (needs `experimental.sandboxes`) | The `/sandbox` manager writes it, and it has to be a private file | `caudra config example sandboxes`, [sandboxes.example.toml](/docs/sandboxes.example.toml) |
| [`init.lua`](/docs/plugins/) | global, project | Lua code that sets up plugins (needs `experimental.lua_plugins`) | It is a program, not settings | - |
| [`.env`](/docs/configuration/#config-files) | global, project | environment variables, such as API keys, for any the environment does not set | It holds secrets | - |
| [`commands/`](/docs/commands/#custom-commands) | global, project | custom slash commands, one Markdown file each | Each command is a file of its own | - |

`caudra config files` lists where each file lives on your machine and whether it is there. `caudra config example FILE` prints the reference of a TOML file, such as `caudra config example mcp`. See [`caudra config`](/docs/cli/#caudra-config).

## Example

```toml
[ui]
splash_animation = true
mouse_scroll_lines = 5
theme = "tokyonight"

[ui.tool_output_lines]
bash = 8
read = 5

[agent]
max_output_lines = 3000

[provider]
default_model = "anthropic/claude-sonnet-4-6"
allowed_models = ["anthropic/*", "openai/gpt-5"]
excluded_models = ["*/*-preview"]

[storage]
max_log_files = 5

[plugins.bash]
timeout_secs = 180

[plugins.index]
max_file_size_mb = 4
```

All fields are optional. A file may start with `version = 1`, and a file without it counts as version 1. Typos in field names and values of the wrong type cause an error right away, with the file and line.

For every setting in one file, with its type, default, and description, run [`caudra config example`](/docs/cli/#caudra-config) or download [caudra.example.toml](/docs/caudra.example.toml).

`provider.allowed_models` is a list of glob patterns for qualified `provider/model-id` specs. `*` also matches `/`, so `opencode/*` includes nested model IDs. When the list is empty or omitted, every model is allowed. `provider.excluded_models` removes matching models after that, so exclusions always win. A project list replaces the matching global list. Omit it to inherit, or use `[]` to clear it. The policy applies to selectors, CLI and API model changes, delegation, and `caudra models`.

## Experimental features

Some features are experimental and stay off until you turn them on. Each one has its own switch in the `[experimental]` table of the global `caudra.toml`:

```toml
[experimental]
workflows = true
decision_engine = true
```

| Key | Default | Turns on |
|-----|---------|----------|
| `workflows` | `false` | [Workflows](/docs/workflows/): the `workflow` tool, the workflow commands and inspector, and the `caudra-workflow-dev` skill. |
| `sandboxes` | `false` | [Managed sandboxes](/docs/sandboxes/): `caudra sandbox`, `caudra auth sandbox`, `--sandbox`, `/sandbox`, and the workbench Transfer view. Sandboxes bring their own connection to Workcell and do not need `remote_workcell`. |
| `remote_workcell` | `false` | Direct [remote Workcell](/docs/remote-workspaces/) connections: the `--workcell-*` flags and `caudra auth workcell`. |
| `lua_plugins` | `false` | Every use of Lua: [plugins](/docs/plugins/), the [Lua API](/docs/lua-api/), global and project `init.lua`, and the `caudra-plugin-dev` skill. `--no-plugins` still turns Lua off for one run. |
| `decision_engine` | `false` | The [decision engine](#decisions), [Auto mode](/docs/permissions/#auto-mode), `caudra decisions`, and workflow [`decide()` calls](/docs/workflows/#typed-decisions). |

Each switch is independent, so turning one on never turns on another. A missing file, table, or key leaves a switch off, and an unknown key is an error. `caudra remote` and `/remote` work when either `sandboxes` or `remote_workcell` is on, and each session checks the switch for its own source.

Only the global file may hold `[experimental]`. Caudra rejects a project `.caudra/caudra.toml` that contains the table, even an empty one, so a repository cannot opt you in. Lua, tool allowlists, and saved sessions cannot turn a feature on either.

Caudra reads the switches once at startup and keeps them until it exits. `/reload`, session switches, and ACP sessions all use the startup values. When the table changes on disk, Caudra shows a notice asking for a restart.

A feature that is off is hidden and does no work. Its tools, commands, shortcuts, help entries, and status chips are gone, and startup skips it. Asking for it directly, such as typing its command or passing its flag, fails with a message that names the switch. A saved session attached to a sandbox or a remote workspace does not resume while its switch is off, and it never falls back to local execution. Turning a feature off keeps its data and leaves external resources alone, so a running sandbox keeps running until you stop it.

With `decision_engine` off, `always_auto = true` and sessions saved in Auto start in Ask. Caudra keeps the saved choice, so Auto returns once the switch is on again.

## Migrating from Lua settings

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

## Full Reference

### Top-level

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `always_yolo` | bool | `false` | Start every session with YOLO mode (skip permission prompts, deny rules still apply); global config only |
| `always_auto` | bool | `false` | Start every session with Auto permission mode (preserve required prompts and screen unmatched calls); global config only. Needs `experimental.decision_engine`, otherwise sessions start in Ask |
| `always_fast` | bool | `false` | Start every session with fast mode, on the models that sell a fast tier (ignored otherwise) |
| `always_thinking` | bool \| string | unset | Start every session with extended thinking (true/"adaptive", "off", an effort level ("minimal" to "max"), or a token budget) |

### `ui`

| Field | Type | Default | Env | Min | Description |
|-------|------|---------|-----|-----|-------------|
| `splash_animation` | bool | `true` | - | - | Show splash animation on startup |
| `scrollbar` | bool | `true` | - | - | Show vertical scrollbar in scrollable areas |
| `touch` | string | `auto` | - | - | Touch-friendly pointer handling: auto, on, or off. Widens the scrollbar's hit zone so a finger can tap it, scrolls one line per wheel event instead of mouse_scroll_lines, and leaves text selection to the terminal. Auto detects Termux around Caudra itself, which SSH does not carry, so set this to on when reaching Caudra from a phone over SSH |
| `notifications` | string | `auto` | - | - | Terminal notification method: auto, osc9, bell, or off. Auto is off in a Herdr pane, where Herdr shows its own notification when Caudra is blocked or finished |
| `math` | string | `unicode` | - | - | How LaTeX maths renders: unicode (approximate with Unicode) or raw (show the LaTeX source) |
| `mermaid` | string | `unicode` | - | - | How mermaid flowcharts render: unicode (draw them with box-drawing characters) or off (leave the fence as code) |
| `flash_duration_ms` | u64 | `1500` | - | - | Duration of flash messages (ms) |
| `which_key_delay_ms` | u64 | `250` | - | - | How long Ctrl+X waits before listing the chords it can still reach (ms). 0 shows the list at once |
| `typewriter_ms_per_char` | u64 | `4` | - | - | Typewriter effect speed (ms/char) |
| `mouse_scroll_lines` | u32 | `3` | - | 1 | Lines per mouse wheel scroll |
| `scroll_card_lines` | u32 | `10` | - | - | Rows of body a shell, python_execution or task card draws. The window follows new output while it sits at the bottom and pauses when scrolled up. Click inside a window to give it the wheel, which passes back to the transcript at either edge, and drag the bar in its last column to move it directly. `0` turns scrolling off, restoring the `ui.tool_output_lines` budget for those tools. A write is never windowed: it is drawn whole at any setting, as the file it created or as the diff of what it replaced |
| `always_collapsed` | string[] | `["file_read", "file_glob", "file_grep", "file_index", "webfetch"]` | - | - | Tools whose card never opens on its own: the call stays a single row in every view mode until you click it. A server-qualified name still matches, so `file_read` also covers `mcp_File_read`. Set to `[]` to opt out |
| `max_input_lines` | u32 | `20` | - | 1 | Maximum visible input lines |
| `show_thinking` | bool | `true` | - | - | Show full model reasoning live and persisted. Turn this off to start every reasoning block collapsed behind a Thinking or Thought header that can be clicked to expand |
| `thinking_lines` | u32 | `10` | - | - | Rows of body an open reasoning block draws. The window follows the reasoning while it streams and pauses when scrolled up, and a footer reports how much sits above and below. Click inside a window to give it the wheel, which passes back to the transcript at either edge, and drag the bar in its last column to move it directly. Click the footer to follow again. A finished block rests on its last rows until you move it. `0` draws every block whole |
| `show_reminders` | bool | `true` | - | - | Show the messages Caudra writes into the conversation on your behalf: standing reminders, goal check-ins, nudges, and continuations. Each is one dim row that expands on click to the exact text the model was sent. Turn this off to keep the transcript to the conversation alone |
| `clock_format` | String | `system` | - | - | Clock format for timestamps: "12h", "24h", or "system" (follow the OS preference, 24h when unknown) |
| `update_check` | bool | `false` | `CAUDRA_ENABLE_UPDATE_CHECK` | - | Ask GitHub for the latest release on startup and show it in the splash. Off by default, so Caudra makes no such request unless you turn this on |
| `theme` | string | unset | - | - | Name of the color theme to load at startup, overriding the theme you last picked with `/theme`. Unset keeps your last pick |
| `theme_light` | string | unset | - | - | Light theme to pair with `theme`, in place of the one from the pairing table or for a theme that has no pair. `theme` becomes the dark half |

### `ui.theme`

Name of the color theme to load at startup, overriding the theme you last picked interactively. If unset, Caudra keeps your last selection, which starts out as `opencode`. An unknown name is ignored with a warning.

Available themes: `ayu_dark`, `ayu_light`, `ayu_mirage`, `carbonfox`, `catppuccin_frappe`, `catppuccin_latte`, `catppuccin_macchiato`, `catppuccin_mocha`, `dark_daltonized`, `dracula`, `everforest_dark`, `fleet_dark`, `github_dark`, `gruvbox`, `gruvbox_light`, `kanagawa`, `kanagawa_ink`, `kanagawa_plum`, `material_darker`, `monokai_pro`, `night_owl`, `nightfox`, `nord`, `onedark`, `opencode`, `opencode_light`, `rose_pine`, `rose_pine_dawn`, `rose_pine_midnight`, `rose_pine_moon`, `solarized_dark`, `solarized_light`, `tokyonight`, `vscode_dark_plus`, `zenburn`.

You can add your own themes too. Drop a `<name>.toml` file into `themes/` inside your Caudra config directory, for example `~/.config/caudra/themes/`. If it reuses a built-in name, yours wins.

Themes use 24-bit colors, but not every terminal can show them. Caudra checks the environment, terminfo, and the terminal itself, and when truecolor is missing it quietly falls back to the closest of the 256 classic terminal colors. If detection gets it wrong, set `CAUDRA_TRUECOLOR=1` to force truecolor or `CAUDRA_TRUECOLOR=0` to force the fallback.

Some themes ship as a light and dark pair: `ayu_dark` and `ayu_light`, `catppuccin_mocha` and `catppuccin_latte`, `gruvbox` and `gruvbox_light`, `opencode` and `opencode_light`, `rose_pine` and `rose_pine_dawn`, `solarized_dark` and `solarized_light`. Choosing either half makes Caudra ask the terminal for its background color and show the half that matches. Themes outside these pairs stay as you left them.

Caudra asks again every ten minutes, and also when the terminal regains focus or changes size, so reattaching a multiplexer to another terminal updates the theme. Following the terminal only changes the running session, and the theme you saved from `/theme` stays saved. Terminals that do not report a background color are asked a few times and then left alone.

### `ui.theme_light`

Light half to use in place of the one from the pairing table, or to give a theme that has no pair. `ui.theme` becomes the dark half:

```toml
[ui]
theme = "tokyonight"
theme_light = "catppuccin_latte"
```

Leave `ui.theme` unset to pair the light theme with whatever you last picked from `/theme`.

### `ui.update_check`

When on, Caudra asks the GitHub releases API for the latest version once at startup and shows it in the splash when yours is older. The request carries a `caudra` user agent and nothing else: no session id, no machine id, not even your current version.

It is off by default, so a normal run reaches only the model provider you configured. Set `CAUDRA_ENABLE_UPDATE_CHECK=1` to turn it on for a single run, or `CAUDRA_ENABLE_UPDATE_CHECK=0` to turn it off when your config has it on. The `caudra update` command always checks, because that is what you asked it to do.

### `ui.tool_output_lines`

How many terminal rows of output an open card shows per tool before it says how many it is holding back. A line that wraps spends a row for each row it wraps to, so an abridged card is the same height whatever its lines are. Clicking the card shows all of it regardless. All values are `usize` with a minimum of 1.

The `bash`, `python_execution`, and `task` entries apply only when `ui.scroll_card_lines` is `0`. Above that, those tools draw a fixed window of that many rows instead, and the budget here goes unused. `write` does not reach a `file_write` that created a file, whose body is that file and is always drawn whole. It is also a floor rather than a bound for anything drawn as a diff, since a diff is already only the part that changed: an edit, a patch, and an overwrite are drawn whole until they run long, and raising `write` past that point is what makes this number matter to them.

| Field | Default | Tools |
|-------|---------|-------|
| `bash` | 5 | `shell` |
| `python_execution` | 5 | `python_execution` |
| `task` | 12 | `task`, `task_control` |
| `index` | 3 | `file_index`, `code_map`, `code_context`, `code_refs`, `code_impact`, `code_expand` |
| `grep` | 3 | `file_grep`, `file_glob` |
| `read` | 3 | `file_read`, `local_document_read` |
| `write` | 7 | `file_write`, `file_edit`, `file_apply_patch`, `image_generate`, `local_document_apply_patch`, `local_document_write`, `memory` |
| `web` | 3 | `webfetch`, `websearch` |
| `other` | 3 | `batch`, `execution_environment`, `question`, `skill`, `todo_write`, `tool_output`, `view_image`, `workflow` |

### `agent`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `system_prompt_profile` | String | `builtin` | - | Default user system prompt profile from the system-prompts config directory |
| `max_output_bytes` | usize | `51200` | 1024 | Host-enforced default max tool-result size (bytes) |
| `max_output_lines` | usize | `2000` | 10 | Host-enforced default max tool-result lines |
| `compaction_buffer` | u32 \| string | 20%, or 10% when the model's window excludes output | - | Context reserved for compaction: token count or percent of the context window (e.g. "20%") |
| `compaction_instructions` | String | unset | - | Extra instructions appended to the compaction summary prompt |
| `post_compaction_instructions` | String | unset | - | Extra instructions the agent receives after any compaction (e.g. re-read plan.md) |
| `compaction_requirements` | bool | `true` | - | Append a `# User requirements` section to every compaction summary: what the user asked for, constrained, and decided, read from their own messages and answered questions across every earlier compaction, and extracted by the Extract model so the conversation model never sees the request |
| `background_reminder_turns` | u32 | `0` | - | Committed main-agent response groups between unchanged active background-work reminders; 0 disables periodic refresh only, not state-change or post-compaction reminders |
| `todo_reminder` | bool | `true` | - | Before the main agent hands control back with pending or in-progress todos and no background work running, remind it once per run, repeating the full todo list, to verify the work and update the list |
| `task_execution` | string | `auto` | - | Task delivery: sync waits for the completed result, auto lets the model choose, async returns an admission receipt |
| `shell_execution` | string | `auto` | - | Shell delivery: sync waits for termination, auto routes by requested timeout, async returns an admission receipt |
| `shell_async_threshold_secs` | u64 | `120` | 1 | Requested shell timeout above which auto delivery returns an admission receipt; independent of the enforced execution deadline |
| `generate_titles` | bool | `true` | - | Name a new session by summarizing its first prompt with the Title model |
| `stale_read_check` | bool | `true` | - | Block a write to a file that changed on disk since it was read, and point a failed edit or patch at the change |
| `tool_json_repair` | bool | `true` | - | Repair malformed tool JSON syntax locally, with one bounded isolated model fallback; independent of eager dispatch |
| `eager_tool_dispatch` | bool | `true` | - | Start tools and batch children as soon as their complete arguments arrive, instead of waiting for the whole message |
| `shell_output_filter` | bool | `true` | - | Filter completed model-facing shell output with built-in rules |
| `shell_workdir_redirect` | bool | `true` | - | Refuse shell commands with a leading literal `cd ... &&` in favor of the shell `workdir` parameter. Set to `false` to disable this nudge independently of `shell_native_redirect` |
| `shell_native_redirect` | string | `enforce` | - | What happens when a shell command only re-implements a native tool, such as bare `rg` or `cat`: `enforce` refuses it and names the tool to call instead, `annotate` only logs the finding, `off` disables the check. A command using any flag the native tool cannot express is never affected |
| `defer_builtin_tools` | string | `auto` | - | When the on-demand built-in tools start outside the request array: `auto` defers them for a small model or one with no supply metadata and declares them upfront for a known non-small model, `always` defers for every model, `never` declares them upfront |
| `image_model` | string | `sunburst` | - | GPT Image 2.5 model behind `image_generate`: `sunburst` is the most capable and the better editor, `flare` is faster at the same price |
| `disabled_tools` | string[] | `[]` | - | Tools to withhold from the model: built-in names, `server.tool`, or `server.*` for a whole MCP server. A project list extends the global one |

### `agent.steering`

Automatic steering repairs unusable model output and can add bounded guidance about repeated behavior or needlessly long tool paths. Every rule is enabled by default. Configure overrides in the `[agent.steering]` table. All fields are optional.

| Field | Type | Default | Min | Max | Description |
|-------|------|---------|-----|-----|-------------|
| `enabled` | boolean | `true` | - | - | Master switch for automatic steering, including truncation recovery and repeat-policy blocking. |
| `max_recoveries` | integer | `32` | 0 | 1024 | Corrective continuations per externally initiated invocation. Zero prevents optional recovery continuations. |
| `max_advisories` | integer | `4` | 0 | 1024 | Advisory injections per invocation. Zero suppresses advisories. |
| `max_stalled_turns` | integer | `5` | 0 | 1024 | Consecutive turns carrying neither a tool call nor visible text before the run ends, whichever rule intervened. Zero disables the backstop. |
| `rules` | table | `{}` | - | - | Overrides by rule name, listed below. Omission uses built-in defaults. |
| `models` | table | `{}` | - | - | Up to 256 exact `provider/model-id` keys, each with its own overrides. |

#### Rules

| Rule in `rules` | Behavior |
|-----------------|----------|
| `truncation` | Continue output cut off by the response token limit, up to 3 corrective requests per externally initiated invocation. |
| `empty_response` | Continue after empty output, with separate per-episode limits after recent tools and while idle. |
| `repeated_tool_call` | Refuse the third consecutive identical top-level tool name/input before execution. Native batch children do not acquire this hard blocker. |
| `protocol_mismatch` | Correct an explicit provider tool-use indication with no actual tool calls, up to 2 continuations per episode. |
| `missing_task_report` | Request a missing task summary or required structured report, up to 2 corrections. |
| `abandoned_turn` | Continue a turn that ended by announcing work the response never performed, up to 2 continuations per episode. Spending the allowance accepts the text rather than failing the turn. |
| `repetition` | Advise on short exact tool cycles, including normalized native batch leaf calls, or repeated normalized assistant text. |
| `tool_planning` | Advise after consecutive failed tool attempts across responses, including attempts with different tools or inputs. Any successful tool result ends the failure episode. Repeating a successful call is insufficient. |
| `relative_paths` | Suggest up to two shorter relative forms when file, patch, code-graph, or shell `workdir` paths spell out the working directory or its parent. The hint appears once per context. |

Recovery and advisory budgets are separate. Advisory rules allow at most 4 total injections per invocation. Repetition and tool-planning advisories each wait a default cooldown of 3 completed model responses.

Tool-planning evidence starts after the last response containing any successful tool result, including results outside the retained batch window. A background admission ends the failure episode without proving that the background work succeeded. Later terminal outcomes do not retroactively change that admission into a failed attempt.

Relative-path evidence is the latest response only, and the hint yields to repetition and tool-planning advisories. While an earlier hint remains in context, even across user turns, no new hint is added. Once compaction removes it, another hint appears only if a later response uses absolute paths again. A suggestion climbs at most one directory with `../`. Local sessions get suggestions only when the working directory the model sees is the canonical project path. Remote workspaces, sandboxes, and code-graph scopes get suggestions inside the working directory only. Paths with backticks, angle brackets, control characters, or invisible Unicode formatting characters are never quoted. Caudra never rewrites the paths a model sends.

An empty-response episode lives in the transcript tail, so it survives a restore and a new invocation. A message typed into a stall is answered, but it does not refill the budget: only a response carrying a tool call or visible text ends the episode. `max_stalled_turns` bounds the turns that interleaved rules spend between them, independently of any single rule's allowance.

Advisories only accompany an independently scheduled next request. They never reopen a valid final answer. Tool-looking prose, JSON, XML, code fences, and quoted examples do not independently trigger protocol correction. Caudra does not scrape tool names or arguments from text and execute them. Only actual tool calls pass through normal validation and authorization. Ordinary assistant answers do not have to be JSON.

#### Rule fields

Each table at `agent.steering.rules.<rule>` accepts these common fields:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | boolean | `true` | Explicit `false` disables this rule. |
| `prompt` | string | unset | Use built-in guidance when omitted. Custom text must be nonblank and at most 16,384 UTF-8 bytes. |

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
| `tool_planning.after_calls` | `6` | 1–1024 | Number of most recent leaf tool calls that must all have failed since the last response containing a successful result. |
| `tool_planning.after_responses` | `3` | 1–1024 | Distinct completed model responses represented by those failed calls. |
| `tool_planning.cooldown` | `3` | 1–1024 | Completed model responses between this rule's advisories. |
| `relative_paths.min_saved_chars` | `12` | 1–1024 | Characters a relative form must save over its absolute path before the path is suggested. |

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

### `provider`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `default_model` | String | unset | - | Default model identifier (e.g. `anthropic/claude-sonnet-4-6`) |
| `allowed_models` | string[] | `[]` | - | Glob patterns for permitted qualified model specs; empty permits all models |
| `excluded_models` | string[] | `[]` | - | Glob patterns for excluded qualified model specs; exclusions take precedence |
| `connect_timeout_secs` | u64 | `10` | 1 | HTTP connect timeout (seconds) |
| `stream_timeout_secs` | u64 | `300` | 10 | Longest the server may send nothing before the request is abandoned (seconds) |

### `storage`

| Field | Type | Default | Env | Min | Description |
|-------|------|---------|-----|-----|-------------|
| `max_log_bytes_mb` | u64 | `200` | - | 1 | Max total log size (MB) |
| `max_log_files` | u32 | `10` | - | 1 | Max number of log files to keep |
| `max_eager_load_mb` | u64 | `1024` | `CAUDRA_MAX_EAGER_LOAD_MB` | 64 | Largest session Caudra will hydrate when opening one (MB), counted in uncompressed payload bytes rather than disk or memory. A session past this refuses to load; trim it or raise this |
| `log_level` | string | `info` | - | - | Minimum severity written to the log file: trace, debug, info, warn, or error. RUST_LOG overrides it |
| `input_history_size` | usize | `100` | - | 10 | Number of input history entries to retain |
| `ephemeral` | bool | `false` | - | - | Store session data in a temporary directory removed when Caudra exits |

### `[storage.retention]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `group_by` | string | `directory` | Evaluate policies per working directory (`directory`) or across every session (`none`) |
| `sweep_interval_hours` | u64 | `24` | Hours between background sweeps. A sweep reclaims freed space, and applies `trim` and `forget` when they are set. `0` disables the sweep; `caudra storage` commands still work |
| `trim` | table | `{}` | Sessions outside this policy lose snapshots, tool output files, archives, and large rich outputs but stay resumable. Empty means never trim automatically |
| `forget` | table | `{}` | Sessions outside this policy are deleted. Empty means never delete automatically |

`trim` and `forget` are keep policies in `restic forget` terms: `keep_last`, `keep_hourly`, `keep_daily`, `keep_weekly`, `keep_monthly`, `keep_yearly` take a count, and `keep_within` plus `keep_within_hourly` through `keep_within_yearly` take a duration such as `"90d"` or `"2y5m7d3h"`. A session is kept when any rule matches. An empty `forget` policy disables automatic deletion. See [Sessions](/docs/sessions/#retention) for what each tier keeps and how the sweep runs.

### `[storage.snapshots]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Capture automatic workspace snapshots locally and remotely, including session-start and final captures. `false` disables capture and file revert without deleting existing snapshots or bypassing restore recovery. `--no-snapshots` overrides this for one run |
| `max_bytes_mb` | u64 | `512` | Largest working tree a capture will take, and the retention target for the compressed object store each workspace shares across its sessions. A workspace above it loses file revert rather than paying for a snapshot the store cannot keep |
| `max_files` | u64 | `50000` | Most files a capture will take, counted after ignore rules |
| `max_file_bytes_mb` | u64 | `100` | Largest single file a capture will take. A bigger one is left out of the snapshot and left alone on disk, so it cannot be reverted |

A workspace over `max_bytes_mb` or `max_files` is refused rather than captured, and individual files over `max_file_bytes_mb` are skipped while the rest of the tree is still captured. A refusal costs file revert and lets the tool call proceed. See [Sessions](/docs/sessions/#limits) for what a capture covers.

### `[telemetry]`

| Field | Type | Default | Env | Description |
|-------|------|---------|-----|-------------|
| `enabled` | bool | `false` | `CAUDRA_ENABLE_TELEMETRY` | Master switch |
| `metrics_exporter` | string | `none` | `OTEL_METRICS_EXPORTER` | Where metrics go: `otlp`, `console`, `none`, or a comma-separated mix |
| `logs_exporter` | string | `none` | `OTEL_LOGS_EXPORTER` | Where events go: `otlp`, `console`, `none`, or a comma-separated mix |
| `protocol` | string | unset | `OTEL_EXPORTER_OTLP_PROTOCOL` | OTLP protocol: `grpc`, `http/protobuf`, or `http/json`. Required when an exporter is `otlp` |
| `endpoint` | string | unset | `OTEL_EXPORTER_OTLP_ENDPOINT` | Collector endpoint. HTTP appends `/v1/metrics` and `/v1/logs` |
| `headers` | table | `{}` | `OTEL_EXPORTER_OTLP_HEADERS` | Extra headers sent with every export |
| `timeout_ms` | integer | `10000` | `OTEL_EXPORTER_OTLP_TIMEOUT` | Per-export request timeout (ms) |
| `compression` | string | `none` | `OTEL_EXPORTER_OTLP_COMPRESSION` | Payload compression: `gzip` or `none` |
| `metrics_protocol` | string | unset | `OTEL_EXPORTER_OTLP_METRICS_PROTOCOL` | Metrics-only protocol override |
| `metrics_endpoint` | string | unset | `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | Metrics-only endpoint, used verbatim with no path appended |
| `metrics_headers` | table | `{}` | `OTEL_EXPORTER_OTLP_METRICS_HEADERS` | Metrics-only headers, merged over `headers` |
| `metrics_timeout_ms` | integer | unset | `OTEL_EXPORTER_OTLP_METRICS_TIMEOUT` | Metrics-only request timeout (ms) |
| `logs_protocol` | string | unset | `OTEL_EXPORTER_OTLP_LOGS_PROTOCOL` | Logs-only protocol override |
| `logs_endpoint` | string | unset | `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | Logs-only endpoint, used verbatim with no path appended |
| `logs_headers` | table | `{}` | `OTEL_EXPORTER_OTLP_LOGS_HEADERS` | Logs-only headers, merged over `headers` |
| `logs_timeout_ms` | integer | unset | `OTEL_EXPORTER_OTLP_LOGS_TIMEOUT` | Logs-only request timeout (ms) |
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

### `[worktrees]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `backend` | string | `auto` | What creates and removes worktrees for `/worktree`: `auto` uses Herdr inside a Herdr pane and git elsewhere, `git` always runs git |
| `directory` | string | `<data dir>/worktrees` | Where git-created worktrees go, as `<directory>/<repository>/<branch>`. A leading `~/` is your home directory |

Inside a Herdr pane, `auto` asks Herdr to create and remove worktrees, so each one opens as a grouped Herdr workspace. `directory` applies only to worktrees git creates. See [Worktrees](/docs/worktrees/) for what `/worktree` does with each backend.

### `decisions`

Configure the optional typed decision engine in the `[decisions]` table. The engine is experimental and needs `decision_engine = true` under [`[experimental]`](#experimental-features). Without that switch Caudra still validates this table and starts no engine. It then sends no decision requests, reads no engine credentials, and leaves decision logs and shell duration history untouched. No endpoint, passive feature, or decision logging is enabled by default. Explicit workflow [`decide()` calls](/docs/workflows/#typed-decisions) need an endpoint but do not need a passive feature enabled. Shell duration history can work without an endpoint.

Connection settings and thresholds are global-only. Projects may set individual features to `"off"`, set `log = false`, or keep or shorten inherited log retention. Other project overrides are errors, even when they repeat a global value. Disabling globally required Auto screening or its active content screening restores prompting for eligible Auto calls.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `endpoint` | string | unset | Full request URL. HTTPS required except for numeric loopback HTTP or explicit `allow_http` consent. No credentials, query, fragment, whitespace, or control characters. |
| `model` | string | `jev-latest` | Decision model identifier, nonblank and without control characters. |
| `api_key_env` | string | `TYPESAFE_API_KEY` | Environment variable containing the optional credential, never the credential itself. Project environment values are excluded. |
| `allow_remote` | boolean | `false` | Explicit global consent to send decision context to a non-loopback endpoint. |
| `allow_http` | boolean | `false` | Global-only opt-in for non-loopback HTTP. Also requires `allow_remote = true`. Use only with transport protection you control, such as a trusted encrypted tunnel. |
| `timeout_ms` | integer | `400` | Positive decision-request deadline in milliseconds, separate from shell execution timeouts. |
| `log` | boolean | `false` | Retain bounded decision records in the separate local `decisions.db`. |
| `log_retention_days` | integer | `90` | Positive retention period for decision records. |
| `features` | table | `{}` | Per-feature modes below. |
| `thresholds` | table | `{}` | Probability thresholds, all finite and within 0–1 inclusive. |

`TYPESAFE_BASE_URL` replaces only the origin of an explicitly configured endpoint, preserving its path. It must be an origin without a path and passes the same endpoint and both transport opt-in checks. The variable alone never activates the engine. Requests ignore ambient proxies and do not follow redirects. `localhost` is a DNS name, not numeric loopback for this policy. Private and CGNAT addresses receive no automatic HTTP exemption. These settings do not change Workcell transport policy.

Redaction is best effort. Decision context can include commands, task text, tool output, and candidate descriptions. Review what you send and any exports before sharing them. See [decision advice and logging](/docs/permissions/#decision-engine-advice).

#### `decisions.features`

`off` disables the feature. `shadow` collects predictions without applying them. `advise` adds caution or suggestions. `enforce` applies only the feature-specific behavior listed below, never permission grants or relaxed executor restrictions. Unsupported modes are configuration errors. Passive features are suppressed in YOLO.

| Feature | Default | Supported modes | Behavior beyond shadow |
|---------|---------|-----------------|------------------------|
| `permission_advice` | `off` | `off`, `shadow`, `advise` | Add warnings to an existing permission prompt without delaying the answer. |
| `auto_screening` | `off` | `off`, `shadow`, `enforce` | Escalate an eligible Auto call to a prompt on a flag or engine failure. No answer channel means denial. |
| `shell_effect` | `off` | `off`, `shadow`, `advise` | Warn about possible project writes during Plan review only when `shell_writes` is configured. Never establish read-only authority. |
| `content_screening` | `off` | `off`, `shadow`, `advise` | Add caution to flagged web/MCP output and tighten upload/credential Auto screening for the session. Content remains available. |
| `shell_duration` | `off` | `off`, `shadow`, `advise`, `enforce` | Advise with local shell estimates. Enforce may fill an omitted timeout and select delivery at admission. Explicit timeouts stay unchanged. See [shell duration](#shell-duration). |
| `tool_search` | `off` | `off`, `shadow`, `enforce` | Rerank the existing lexical tool shortlist. This neither loads arbitrary names nor grants execution permission. |
| `skill_suggestions` | `off` | `off`, `shadow`, `advise` | Suggest a shortlisted skill. The agent still chooses whether to load it. |
| `goal_prescreen` | `off` | `off`, `shadow`, `enforce` | Skip an unlikely-to-pass goal evaluation within the continuation budget and continue work. Only the normal evaluator can certify completion. |
| `subagent_routing` | `off` | `off`, `shadow`, `enforce` | Choose a model job for a new unpinned subagent from its task label, not its full prompt. Explicit jobs, profile pins, and continuations keep their routing. |

#### `decisions.thresholds`

Flag thresholds trigger at or above the configured value. Goal prescreening uses an at-or-below comparison. Content screening requires both signals in a sampled chunk. After content is flagged, Auto uses 75% of `auto_flag` for upload and credential flags.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `permission_flag` | float | `0.85` | Probability for a permission warning. |
| `auto_flag` | float | `0.85` | Probability for escalating an eligible Auto call. |
| `content_injection` | float | `0.9` | Probability that sampled content attempts instruction injection. |
| `content_addressed_to_agent` | float | `0.9` | Probability that sampled content addresses the agent. |
| `shell_endless` | float | `0.9` | Probability that a shell command runs until stopped. |
| `shell_heavy` | float | `0.9` | Probability for a heavy-command prior and confidence required for a duration choice. |
| `routing_confidence` | float | `0.9` | Confidence required for tool search, skill suggestions, and subagent routing. Tool-search choice probability must also meet it. |
| `goal_skip_below` | float | `0.05` | Skip an evaluator at or below this completion probability, within the continuation budget. |
| `shell_writes` | float | unset | Optional project-write warning threshold. Omission leaves the warning disabled. No built-in enforcement threshold. |

#### Shell duration

`shell_duration` applies only to eligible local native shell calls, not remote workspaces or managed sandboxes. Measured exact-command history takes priority over command-family history, and both take priority over a model estimate. Timeouts, cancellations, and failures are recorded separately from completed latency samples. History is separate from the opt-in decision log, so `log = false` does not disable duration observations.

In `advise`, estimates and warnings leave execution unchanged. In `enforce`, an omitted `timeoutSec` may receive a default based on 1.5 times estimated p90, bounded by the tool schema's default and maximum. An explicit timeout is never changed. An endless prediction gives caution only and does not remove the execution deadline.

With `agent.shell_execution = "auto"`, Enforce estimates can select synchronous or asynchronous delivery at admission, bounded by the effective timeout and `agent.shell_async_threshold_secs`. Explicit sync/async settings still win. Elapsed runtime never promotes a synchronous call to asynchronous delivery. An admission receipt is not completion or success.

#### Question overrides

Only the permission question set currently supports a user-global file override: `~/.config/caudra/decisions/permission.json`. It must be a regular JSON file no larger than 64 KiB, retain all required question IDs as `noul`, and pass question validation. It is read when an endpoint and permission advice or Auto screening are enabled. Projects cannot supply this override. Other feature question sets have no file override.

## Plugins

The `plugins` table turns bundled features and plugins on or off and passes options to them. All bundled features are on by default. Set `enabled = false` to turn one off.

Each feature checks its own options at startup. A typo, a wrong type, or an unknown plugin name gives you a clear error right away.

`enabled = false` turns off the tools that key produced, under the names they are registered with today, so `plugins.bash` turns off `shell` and `plugins.edit` turns off `file_edit` and `file_apply_patch`. To name a tool directly, use `agent.disabled_tools`, described in [Disabling tools](/docs/tools/#disabling-tools).

The edit plugin's extra tools are options too: `plugins.edit = { multiedit = false, insert_lines = true }`.

This table is for bundled plugins only, and it works without Lua. Your own plugins go in `~/.config/caudra/lua/` and need `lua_plugins` turned on. See [Plugins](/docs/plugins/).

```toml
[plugins.bash]
timeout_secs = 180

[plugins.websearch]
enabled = false
```

### `plugins.index`

`file_index` executes as a native Workcell tool, and this table keeps the `plugins.index` key it was configured under. The file-size limit accepts 1 through 16 MiB to bound parser memory and work.

| Field | Type | Default | Min | Max | Description |
|-------|------|---------|-----|-----|-------------|
| `max_file_size_mb` | integer | `2` | 1 | 16 | Refuse to index files larger than this many MiB. |

### `plugins.skill`

`skill` executes as a native Caudra tool. This table keeps its existing configuration key.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `plugin_dev` | boolean | `false` | Offer the builtin caudra-plugin-dev skill for writing caudra plugins. Needs `experimental.lua_plugins`. |
| `workflow_dev` | boolean | `true` | Offer the builtin caudra-workflow-dev skill for writing and running workflows. Needs `experimental.workflows`. |

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
| Scratch | `$TMPDIR/caudra/` | `%TEMP%\caudra\` |

Config holds `caudra.toml`, `permissions.toml`, `mcp.toml`, `providers.toml`, `workcell.toml`, `sandboxes.toml`, `init.lua`, `.env` and `commands/`. State holds sessions, auth tokens, memories, plans, model-job bindings, sandbox lifecycle records and transfer recovery journals. The install script puts the binary under `%LOCALAPPDATA%\caudra` on Windows; that is separate from these runtime dirs.

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

(Linux/macOS: `~/.local/state/caudra/…`; Windows: `%APPDATA%\caudra\…`). Use them for non-obvious gotchas and decisions that should survive across sessions. They are separate from skills and from `AGENTS.md`.

Related pages: [Skills](/docs/skills/), [CLI](/docs/cli/), [Providers](/docs/providers/#providers-toml).
