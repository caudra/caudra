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

Remote sessions load only the client's global executable configuration. They do not load either checkout's project `init.lua`, project environment files, or project MCP configuration. Remote project context uses a bounded declarative asset manifest instead. See [Remote Workspaces](/docs/remote-workspaces/#project-context-and-trust).

Remote endpoint profiles live in a separate user `workcell.toml`, with `version = 1` and tables named `[workcell.profiles.NAME]`. They are not `caudra.setup()` settings. See [profile configuration](/docs/remote-workspaces/#configure-a-profile) for the exact fields and credential rules.

Managed sandbox providers, profiles, network policies and transfer defaults live in user-global `sandboxes.toml`, also with `version = 1`. They are separate from direct Workcell and model-provider profiles. See [Managed Sandboxes](/docs/sandboxes/#configuration-schema) for the schema, TUI editor and release status. Saving these defaults does not create a VM or change a running instance.

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
| `always_fast` | bool | `false` | Start every session with fast mode, on the models that sell a fast tier (ignored otherwise) |
| `always_thinking` | bool \| string | `false` | Start every session with extended thinking (true/"adaptive", "off", an effort level ("minimal" to "max"), or a token budget) |

### `ui`

| Field | Type | Default | Env | Min | Description |
|-------|------|---------|-----|-----|-------------|
| `splash_animation` | bool | `true` | - | - | Show splash animation on startup |
| `scrollbar` | bool | `true` | - | - | Show vertical scrollbar in scrollable areas |
| `touch` | string | `auto` | - | - | Touch-friendly pointer handling: auto, on, or off. Widens the scrollbar's hit zone so a finger can tap it, scrolls one line per wheel event instead of mouse_scroll_lines, and leaves text selection to the terminal. Auto detects Termux around Caudra itself, which SSH does not carry, so set this to on when reaching Caudra from a phone over SSH |
| `notifications` | string | `auto` | - | - | Terminal notification method: auto, osc9, bell, or off |
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

How many terminal rows of output an open card shows per tool before it says how many it is holding back. A line that wraps spends a row for each row it wraps to, so an abridged card is the same height whatever its lines are. Clicking the card shows all of it regardless. All values are `usize` with a minimum of 1.

The `bash`, `python_execution`, and `task` entries apply only when `ui.scroll_card_lines` is `0`. Above that, those tools draw a fixed window of that many rows instead, and the budget here goes unused. `write` does not reach a `file_write` that created a file, whose body is that file and is always drawn whole. It is also a floor rather than a bound for anything drawn as a diff, since a diff is already only the part that changed: an edit, a patch, and an overwrite are drawn whole until they run long, and raising `write` past that point is what makes this number matter to them.

| Field | Default | Tools |
|-------|---------|-------|
| `bash` | 5 | `shell` |
| `python_execution` | 5 | `python_execution` |
| `task` | 12 | `task` |
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
| `compaction_buffer` | u32 \| string | `20%, or 10% when the model's window excludes output` | - | Context reserved for compaction: token count or percent of the context window (e.g. "20%") |
| `compaction_instructions` | String | `none` | - | Extra instructions appended to the compaction summary prompt |
| `post_compaction_instructions` | String | `none` | - | Extra instructions the agent receives after any compaction (e.g. re-read plan.md) |
| `compaction_requirements` | bool | `true` | - | Append a `# User requirements` section to every compaction summary: what the user asked for, constrained, and decided, read from their own messages and answered questions across every earlier compaction, and extracted by the Extract model so the conversation model never sees the request |
| `generate_titles` | bool | `true` | - | Name a new session by summarizing its first prompt with the Title model |
| `stale_read_check` | bool | `true` | - | Block a write to a file that changed on disk since it was read, and point a failed edit or patch at the change |
| `tool_json_repair` | bool | `true` | - | Repair malformed tool JSON syntax locally, with one bounded isolated model fallback; independent of eager dispatch |
| `eager_tool_dispatch` | bool | `true` | - | Start tools and batch children as soon as their complete arguments arrive, instead of waiting for the whole message |
| `shell_output_filter` | bool | `true` | - | Filter completed model-facing shell output with built-in rules |
| `shell_native_redirect` | string | `enforce` | - | What happens when a shell command only re-implements a native tool, such as bare `rg` or `cat`: `enforce` refuses it and names the tool to call instead, `annotate` only logs the finding, `off` disables the check. A command using any flag the native tool cannot express is never affected |
| `defer_builtin_tools` | string | `auto` | - | When the on-demand built-in tools start outside the request array: `auto` defers them for a small model or one with no supply metadata and declares them upfront for a known non-small model, `always` defers for every model, `never` declares them upfront |
| `image_model` | string | `sunburst` | - | GPT Image 2.5 model behind `image_generate`: `sunburst` is the most capable and the better editor, `flare` is faster at the same price |
| `disabled_tools` | string[] | `[]` | - | Tools to withhold from the model: built-in names, `server.tool`, or `server.*` for a whole MCP server. A project list extends the global one |

### `agent.steering`

Automatic steering repairs unusable model output and can add bounded guidance about repeated behavior. All eight rules are enabled by default. Configure overrides inside `agent` in `caudra.setup()`. All fields are optional.

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

Validation also requires:

- `repetition.window >= repetition.max_cycle * repetition.cycle_repeats`.
- `repetition.text_window >= repetition.text_repeats`.
- `tool_planning.after_calls >= tool_planning.after_responses`.

Unknown fields, invalid types, out-of-range values, and impossible threshold/window combinations are rejected, even for disabled rules. Cooldowns count completed model responses, not seconds, stream chunks, tool children, or injected messages. Advisory eligibility excludes synthetic messages, empty markers, reasoning-only padding, and private title, compaction, or evaluator requests. Advisory evidence is the responses to the request in flight: a new user message ends it, as a compaction or a model change does, so an ordinary conversation is never advised. Recent-pattern windows reset with it, without refilling an active invocation's budgets.

Tool-planning guidance asks the model to reconsider its tool choices and identify the next useful action. It does not switch Plan Mode or require a todo list.

#### Global and exact-model overrides

This example disables repetition guidance globally, then enables it with a higher threshold for one exact model and adjusts that model's tool-planning guidance:

```lua
caudra.setup({
    agent = {
        steering = {
            rules = {
                repetition = { enabled = false },
            },
            models = {
                ["openai/gpt-5"] = {
                    rules = {
                        repetition = { enabled = true, text_repeats = 4 },
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

At most one supplemental steering message is added per request. Recovery takes priority, then repetition and tool planning. Advisory exhaustion only suppresses hints. Recovery exhaustion with an unmet output contract reports a failure and retains partial output, except for `abandoned_turn`, which stops intervening and lets the turn end. A valid captured structured task report remains usable after an empty tail, but cancellation, transport/permission failures, and hard outer-limit failures do not become success.

`abandoned_turn` reads the tail of a response that called no tool and would otherwise end the turn. It fires on a text stopping at a bare colon, or on a last sentence that opens on an intent to act. It does not fire on a question, an offer, a completion, or a promise deferred behind another event, and code spans and quoted prose are removed before any of that is matched. Tool-looking prose is not executed here either; the rule only decides whether to ask for one more response.

The resolved `enabled = false` disables automatic recovery, including truncation, advisories, and repeat-policy blocking. It leaves malformed-input rejection, schema validation, permissions, mode restrictions, cancellation, explicit goals, manual steering, and compaction policy intact. A rule-level switch disables only that rule. Zero budgets prevent the corresponding continuations or hints without bypassing input validation or repeat-policy enforcement.

Truncation recovery counts corrective requests, not ordinary responses or tool rounds. Each request consumes one attempt from `rules.truncation.max_attempts` and one recovery from `max_recoveries`. Automatic report-correction prompts and compaction do not refill either allowance. The last allowed correction may complete the answer. If another correction is needed, exhaustion reports an error with partial output and usage retained, including when `max_recoveries = 0`.

Disabling the master switch or setting `rules.truncation.enabled = false` stops with the truncated outcome instead of requesting a continuation. Disabled or exhausted truncation does not fall through to empty-response repair, even when the truncated response is empty. An unresolved truncated task result remains cut short rather than reopening through report repair. A valid structured report can still satisfy the task's report contract. Cancellation, queued user instructions, and outer turn limits take priority over automatic steering.

### `provider`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `default_model` | String | `none` | - | Default model identifier (e.g. `anthropic/claude-sonnet-4-6`) |
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

### `storage.retention`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `group_by` | string | `directory` | Evaluate policies per working directory (`directory`) or across every session (`none`) |
| `sweep_interval_hours` | u64 | `24` | Hours between background sweeps. A sweep reclaims freed space, and applies `trim` and `forget` when they are set. `0` disables the sweep; `caudra storage` commands still work |
| `trim` | table | `{}` | Sessions outside this policy lose snapshots, tool output files, archives, and large rich outputs but stay resumable. Empty means never trim automatically |
| `forget` | table | `{}` | Sessions outside this policy are deleted. Empty means never delete automatically |

`trim` and `forget` are keep policies in `restic forget` terms: `keep_last`, `keep_hourly`, `keep_daily`, `keep_weekly`, `keep_monthly`, `keep_yearly` take a count, and `keep_within` plus `keep_within_hourly` through `keep_within_yearly` take a duration such as `"90d"` or `"2y5m7d3h"`. A session is kept when any rule matches. An empty `forget` policy disables automatic deletion. See [Sessions](/docs/sessions/#retention) for what each tier keeps and how the sweep runs.

### `storage.snapshots`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Capture automatic workspace snapshots locally and remotely, including session-start and final captures. `false` disables capture and file revert without deleting existing snapshots or bypassing restore recovery. `--no-snapshots` overrides this for one run |
| `max_bytes_mb` | u64 | `512` | Largest working tree a capture will take, and the cap on one session's object store. A workspace above it loses file revert rather than paying for a snapshot the store cannot keep |
| `max_files` | u64 | `50000` | Most files a capture will take, counted after ignore rules |
| `max_file_bytes_mb` | u64 | `100` | Largest single file a capture will take. A bigger one is left out of the snapshot and left alone on disk, so it cannot be reverted |

A workspace over `max_bytes_mb` or `max_files` is refused rather than captured, and individual files over `max_file_bytes_mb` are skipped while the rest of the tree is still captured. A refusal costs file revert and lets the tool call proceed. See [Sessions](/docs/sessions/#limits) for what a capture covers.

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
| Scratch | `$TMPDIR/caudra/` | `%TEMP%\caudra\` |

Config holds `init.lua`, `permissions.toml`, `mcp.toml`, `providers.toml`, `workcell.toml`, `sandboxes.toml`, and `commands/`. State holds sessions, auth tokens, memories, plans, model-job bindings, sandbox lifecycle records and transfer recovery journals. The install script puts the binary under `%LOCALAPPDATA%\caudra` on Windows; that is separate from these runtime dirs.

Scratch holds work that belongs outside your project, such as a file the model writes while thinking or a temporary a command leaves behind. Each project gets its own subdirectory, named by the project directory plus a three-word phrase derived from its path, as in `caudra-heroic-easy-grouse`. Two checkouts sharing a name get different phrases, so they cannot overwrite each other. The phrase is derived rather than drawn at random, so a project returns to the same directory on every run. Caudra creates it at startup and points `TMPDIR`, `TMP`, and `TEMP` at it, so every command Caudra runs puts its own temporary files there instead of the shared temp root. The model is told the path, and writing anywhere under the scratch root needs no approval. Paths beside the root still ask.

The choice of subdirectory is fixed when Caudra starts. `/cd` moves the project without moving it, which is why approval covers the whole root rather than one project's share of it. Directories are owner-only, and Caudra refuses to use one that turns out to be a symlink. Nothing sweeps them, so they live as long as your system keeps its temp root.

State that belongs to one project sits under `…/state/caudra/projects/<project-id>/`, where the id is the project directory name plus a hash of its path. Memory notes and plan-mode documents both live there, so removing that directory clears everything Caudra kept for the project.

Development builds compiled with debug assertions use `caudra-debug` for every platform directory. This keeps global config, sessions, auth, logs, and caches separate from release builds. Per-project `.caudra/` directories remain shared.

Set `CAUDRA_NAMESPACE` to choose the directory name yourself instead of letting the build profile pick it. The value is a single directory name, so `CAUDRA_NAMESPACE=caudra-review` reads and writes `~/.config/caudra-review/`, `~/.local/state/caudra-review/`, and the rest. Use it to give a run its own config and session store, or to point a debug build at your release directories. A value that cannot be a directory name, such as one holding a path separator or `..`, stops Caudra with an error rather than falling back. An empty value counts as unset. Per-project `.caudra/` directories are unaffected.

## Config file versions

Each TOML config file takes a top-level `version`, and every format is at version 1. Where the key is optional, a file without it counts as version 1. A build that finds a newer version than it reads refuses the file instead of guessing what it means, and the error says to upgrade Caudra.

| File | `version` | Newer than this build reads |
|------|-----------|-----------------------------|
| `permissions.toml` | Optional | Fails closed. Tool calls are denied until the file is fixed |
| `mcp.toml` | Optional | Servers from that file do not start, and Caudra shows the error |
| `providers.toml` | Optional | Caudra stops with an error |
| `plugin.toml` | Optional | Every permission of that plugin is denied |
| `workcell.toml` | Required | Rejected with an error |
| `sandboxes.toml` | Required | Rejected with an error |

Caudra writes `version = 1` whenever it saves `providers.toml`. `init.lua` has no version because it is a script. To share one `init.lua` across releases, branch on [`caudra.version()`](/docs/lua-api/#caudra-version).

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
