---
title: "Reference configs"
description: "Every TOML config file in full, with each key commented out and described."
---

`caudra config example FILE` prints the reference of a TOML config file: every key the file accepts, with its description, type, and default, all commented out. This page shows the reference of each file. [Config files](/docs/configuration/#config-files) explains what each file is for.

## caudra.toml

[`caudra.toml`](/docs/configuration/) holds settings, and in the global file the [experimental] switches. Download this reference as [caudra.example.toml](/docs/caudra.example.toml).

```toml
# Every caudra.toml setting. Each one is commented out, so this file changes
# nothing until you edit it. `caudra config example caudra` prints it.
#
# To change a setting, copy its line into your caudra.toml under the same
# [table] header, remove the "#", and set your value. A value in angle
# brackets, such as <string>, marks a setting that has no default. The global
# file is ~/.config/caudra/caudra.toml, or %APPDATA%\caudra\caudra.toml on
# Windows. A project .caudra/caudra.toml takes the same settings, except
# [experimental], [automations], [agent.shell_env], [web], [websearch], and the
# ones marked global-only.
#
# caudra.toml holds the settings only you write. Other files hold what Caudra
# writes itself, or what needs rules of its own: permissions.toml (permission
# rules for tools and MCP servers), mcp.toml (MCP servers), providers.toml
# (model providers, their models, and their accounts), workcell.toml (profiles
# for direct remote Workcell connections), sandboxes.toml (managed sandbox
# providers, networks, transfers, and profiles), init.lua (Lua code that sets
# up plugins), .env (environment variables, such as API keys, for any the
# environment does not set), commands/ (custom slash commands, one Markdown
# file each). `caudra config files` shows where each one lives, and
# `caudra config example FILE` prints the reference of a TOML file.
#
# Full reference: https://caudra.ai/docs/configuration/

version = 4

# Start every session with YOLO mode (skip permission prompts, deny rules still
# apply); global config only.
# Type: bool.
# always_yolo = false

# Start every session with Auto permission mode (preserve required prompts and
# screen unmatched calls); global config only. Screening runs only while a
# decision endpoint is active.
# Type: bool.
# always_auto = false

# Start every session with fast mode, on the models that sell a fast tier
# (ignored otherwise).
# Type: bool.
# always_fast = false

# Start every session with extended thinking (true/"adaptive", "off", an effort
# level ("minimal" to "max"), or a token budget).
# Type: bool | string. Default: unset.
# always_thinking = <bool | string>

[experimental]
# Experimental features stay off until you turn them on, and each switch is
# independent. Only the global caudra.toml may hold this table. Caudra reads it
# once at startup, so a change needs a restart.

# Turn on workflows.
# Type: bool.
# workflows = false

# Turn on managed sandboxes.
# Type: bool.
# sandboxes = false

# Turn on direct remote Workcell connections.
# Type: bool.
# remote_workcell = false

# Turn on Lua plugins and init.lua.
# Type: bool.
# lua_plugins = false

# Turn on cross-session messages.
# Type: bool.
# cross_session_messaging = false

# Turn on automations.
# Type: bool.
# automations = false

[ui]

# Show splash animation on startup.
# Type: bool.
# splash_animation = true

# Show vertical scrollbar in scrollable areas.
# Type: bool.
# scrollbar = true

# Touch-friendly pointer handling: auto, on, or off. Widens the scrollbar's hit
# zone so a finger can tap it, scrolls one line per wheel event instead of
# mouse_scroll_lines, and leaves text selection to the terminal. Auto detects
# Termux around Caudra itself, which SSH does not carry, so set this to on when
# reaching Caudra from a phone over SSH.
# Type: string.
# touch = "auto"

# Terminal notification method: auto, osc9, bell, or off. Auto reports OSC 7501
# program status when supported, otherwise uses OSC 9 or BEL. Native Herdr or
# Orca reporting takes precedence in their panes. Explicit osc9, bell, and off
# skip program status detection.
# Type: string.
# notifications = "auto"

# How LaTeX maths renders: unicode (approximate with Unicode) or raw (show the
# LaTeX source).
# Type: string.
# math = "unicode"

# How mermaid flowcharts render: unicode (draw them with box-drawing
# characters) or off (leave the fence as code).
# Type: string.
# mermaid = "unicode"

# How the transcript draws images that `view_image` reads, `image_generate`
# saves, and markdown names with `![alt](path)`: auto (Kitty graphics where the
# terminal supports them, coloured quadrant blocks anywhere else), kitty,
# blocks, sextants (sharper blocks, for fonts and terminals that draw Unicode
# 13 sextants, such as Ghostty, kitty, WezTerm, and foot), or off (show only
# the caption; `false` also turns them off). Markdown images are read from
# local files only and never fetched.
# Type: string.
# inline_images = "auto"

# Duration of ordinary status-bar messages (ms). Confirmation prompts use a
# fixed 3-second window.
# Type: u64.
# flash_duration_ms = 10000

# How long Ctrl+X waits before listing the chords it can still reach (ms). 0
# shows the list at once.
# Type: u64.
# which_key_delay_ms = 250

# Show a tooltip when the mouse rests on a footer chip, a composer, queue or
# permission prompt control, an icon, a cut-off name or a paste token.
# Type: bool.
# tooltips = true

# Typewriter effect speed (ms/char).
# Type: u64.
# typewriter_ms_per_char = 4

# Lines per mouse wheel scroll.
# Type: u32, at least 1.
# mouse_scroll_lines = 3

# Rows of body a shell, code or task card draws. The window follows new output
# while it sits at the bottom and pauses when scrolled up. Click inside a
# window to give it the wheel, which passes back to the transcript at either
# edge, and drag the bar in its last column to move it directly. `0` turns
# scrolling off, restoring the `ui.tool_output_lines` budget for those tools. A
# write is never windowed: it is drawn whole at any setting, as the file it
# created or as the diff of what it replaced.
# Type: u32.
# scroll_card_lines = 10

# Tools whose card never opens on its own: the call stays a single row in every
# view mode until you click it. A server-qualified name still matches, so
# `file_read` also covers `mcp_File_read`. A `code` snippet's history calls are
# named `history.search`, `history.read`, and `history.sessions`, and its
# `host.environment` call is `execution_environment`. `memory_read`,
# `plan_read`, `workflow_read`, and `task_control_read` cover only the calls of
# `memory`, `plan`, `workflow`, and `task_control` that read, and leave the
# calls that change something open. The bare tool name covers all of its calls.
# Set to `[]` to opt out.
# Type: string[].
# always_collapsed = ["file_read", "file_glob", "file_grep", "file_index", "webfetch", "source_code_context", "source_code_refs", "history.search", "history.read", "history.sessions", "tool_output", "skill", "execution_environment", "list_sessions", "read_topic", "automation", "memory_read", "plan_read", "workflow_read", "task_control_read"]

# Maximum visible input lines.
# Type: u32, at least 1.
# max_input_lines = 20

# Show full model reasoning live and persisted. Turn this off to start every
# reasoning block collapsed behind a Thinking or Thought header that can be
# clicked to expand.
# Type: bool.
# show_thinking = true

# Rows of body an open reasoning block draws. The window follows the reasoning
# while it streams and pauses when scrolled up, and a footer reports how much
# sits above and below. Click inside a window to give it the wheel, which
# passes back to the transcript at either edge, and drag the bar in its last
# column to move it directly. Click the footer to follow again. A finished
# block rests on its last rows until you move it. `0` draws every block whole.
# Type: u32.
# thinking_lines = 10

# Show the messages Caudra writes into the conversation on your behalf:
# standing reminders, goal check-ins, nudges, and continuations. Each is one
# dim row that expands on click to the exact text the model was sent. Turn this
# off to keep the transcript to the conversation alone.
# Type: bool.
# show_reminders = true

# Show what the decision engine changed or is deciding. A decision about a tool
# call is a chip on the call's card, such as `◆ reranking…` while the
# engine works and `◆ reranked` once it answers, and opening the card shows
# the candidates and scores behind it. A decision about a turn, such as a skill
# suggestion or a skipped goal check, is a row that expands on click. Failures
# say the default applied, and `shadow` marks what a feature in shadow mode
# would have done. The model never sees them.
# Type: bool.
# show_decisions = true

# Clock format for timestamps: "12h", "24h", or "system" (follow the OS
# preference, 24h when unknown).
# Type: String.
# clock_format = "system"

# Check GitHub releases in the background at interactive startup and show a
# banner above the prompt when a newer version is available. Uses a shared
# 24-hour cache and never installs automatically. Set false to disable.
# Type: bool. Env: CAUDRA_ENABLE_UPDATE_CHECK.
# update_check = true

# Release channel: `auto` follows stable from a stable build and preview from a
# prerelease, including graduation to stable. `stable` excludes prereleases.
# `preview` includes prereleases and stable releases.
# Type: string.
# update_channel = "auto"

# After an update, show what is new in the releases since the one you last ran,
# once, the next time the terminal UI starts with nothing else to show.
# `/changelog` shows the notes at any time. Set false to keep them to
# `/changelog`.
# Type: bool.
# changelog_on_update = true

# Mode a new terminal UI session opens in: `plan`, where the agent writes only
# the plan file until you switch, or `build`. A resumed session reopens in the
# mode it was left in, and `/new` in an idle tab keeps that tab's mode.
# `--print` and ACP always start in build.
# Type: string.
# default_mode = "plan"

# Name of the color theme to load at startup, overriding the theme you last
# picked with `/theme`. Unset keeps your last pick.
# Type: string. Default: unset.
# theme = <string>

# Light theme to pair with `theme`, in place of the one from the pairing table
# or for a theme that has no pair. `theme` becomes the dark half.
# Type: string. Default: unset.
# theme_light = <string>

[ui.tool_output_lines]
# Rows of output an open tool card shows before it holds the rest back. `other`
# covers every tool that no other key names.

# Rows for `shell`.
# Type: usize, at least 1.
# bash = 5

# Rows for `code`.
# Type: usize, at least 1.
# code = 5

# Rows for `task`, `task_control`.
# Type: usize, at least 1.
# task = 12

# Rows for `file_index`, `source_code_context`, `source_code_refs`.
# Type: usize, at least 1.
# index = 3

# Rows for `file_grep`, `file_glob`.
# Type: usize, at least 1.
# grep = 3

# Rows for `file_read`.
# Type: usize, at least 1.
# read = 3

# Rows for `file_write`, `file_edit`, `file_apply_patch`, `image_generate`,
# `memory`, `plan`.
# Type: usize, at least 1.
# write = 7

# Rows for `webfetch`, `websearch`.
# Type: usize, at least 1.
# web = 3

# Rows for `automation`, `batch`, `execution_environment`, `list_sessions`,
# `publish_message`, `question`, `read_topic`, `send_message`, `skill`,
# `todo_write`, `tool_output`, `view_image`, `work_assignment`, `workflow`.
# Type: usize, at least 1.
# other = 3

[agent]

# Default user system prompt profile from the system-prompts config directory.
# Type: String.
# system_prompt_profile = "builtin"

# Host-enforced default max tool-result size (bytes).
# Type: usize, at least 1024.
# max_output_bytes = 51200

# Host-enforced default max tool-result lines.
# Type: usize, at least 10.
# max_output_lines = 2000

# Context reserved for compaction: token count or percent of the context window
# (e.g. "20%").
# Type: u32 | string. Default: 20%, or 10% when the model's window excludes
# output. On a window that includes output, at least the model's output
# allowance, up to half the window.
# compaction_buffer = <u32 | string>

# Extra instructions appended to the compaction summary prompt.
# Type: String. Default: unset.
# compaction_instructions = <String>

# Extra instructions the agent receives after any compaction (e.g. re-read
# plan.md).
# Type: String. Default: unset.
# post_compaction_instructions = <String>

# Append a `# User requirements` section to every compaction summary: what the
# user asked for, constrained, and decided, read from their own messages and
# answered questions across every earlier compaction, and extracted by the
# Extract model so the conversation model never sees the request.
# Type: bool.
# compaction_requirements = true

# Committed main-agent response groups between unchanged active background-work
# reminders; 0 disables periodic refresh only, not state-change or
# post-compaction reminders.
# Type: u32.
# background_reminder_turns = 0

# Before the main agent hands control back with pending or in-progress todos
# and no background work running, remind it once per run, repeating the full
# todo list, to verify the work and update the list.
# Type: bool.
# todo_reminder = true

# Compactions within one request, with no work started since it, before Caudra
# reminds the agent to act: make a change or delegate one in build mode, write
# the plan or ask in plan mode, report in a read-only task. Repeated at every
# compaction after that. 0 disables.
# Type: u32.
# work_nudge_compactions = 2

# Task delivery: sync waits for the completed result, auto lets the model
# choose, async returns an admission receipt.
# Type: string.
# task_execution = "auto"

# Let the main agent choose a model and reasoning level for each `task` call,
# such as when you ask it to run work on a named model. When off, `task` has no
# `model` or `thinking` argument and tasks run on the Subagent model or a
# profile's pin.
# Type: bool.
# task_model_choice = true

# Let the main agent ask a `question` without waiting for the answer, so it
# keeps working while the form waits for you. The answer reaches it later, and
# you can also put a waiting question off with F4. When off, every question
# waits for its answer.
# Type: bool.
# deferred_questions = true

# Shell delivery: sync waits for termination, auto routes by requested timeout,
# async returns an admission receipt.
# Type: string.
# shell_execution = "auto"

# Requested shell timeout above which auto delivery returns an admission
# receipt; independent of the enforced execution deadline.
# Type: u64, at least 1.
# shell_async_threshold_secs = 120

# Show an ETA on a shell card, from a small model trained on this machine's
# past shell runs. It only labels the card: timeouts and delivery never change.
# When off, nothing is predicted or recorded.
# Type: bool.
# shell_eta = true

# Name a new session by summarizing its first prompt with the Title model.
# Type: bool.
# generate_titles = true

# Summarize memory notes in the background with the Memory model, so the memory
# view in the system prompt stays within its budget. When off, new notes reach
# the view only as titles and `memory search` still finds them.
# Type: bool.
# summarize_memory = true

# Block a write to a file that changed on disk since it was read, and point a
# failed edit or patch at the change.
# Type: bool.
# stale_read_check = true

# Repair malformed tool JSON syntax locally, with one bounded isolated model
# fallback; independent of eager dispatch.
# Type: bool.
# tool_json_repair = true

# Start tools and batch children as soon as their complete arguments arrive,
# instead of waiting for the whole message.
# Type: bool.
# eager_tool_dispatch = true

# Filter completed model-facing shell output with built-in rules.
# Type: bool.
# shell_output_filter = true

# Refuse shell commands with a leading literal `cd ... &&` in favor of the
# shell `workdir` parameter. Set to `false` to disable this nudge independently
# of `shell_native_redirect`.
# Type: bool.
# shell_workdir_redirect = true

# What happens when a shell command only re-implements a native tool, such as
# bare `rg` or `cat`: `enforce` refuses it and names the tool to call instead,
# `annotate` only logs the finding, `off` disables the check. A command using
# any flag the native tool cannot express is never affected.
# Type: string.
# shell_native_redirect = "enforce"

# GPT Image 2.5 model behind `image_generate`: `sunburst` is the most capable
# and the better editor, `flare` is faster at the same price.
# Type: string.
# image_model = "sunburst"

# Keep the prompt cache from expiring by re-sending the last request with no
# output, on the Anthropic API only: `running` while a run waits on a long
# tool, subagent, question or permission prompt, `idle` also for up to 30
# minutes after it ends, `off` never. Each refresh is billed as a cache read;
# global config only.
# Type: string.
# cache_warming = "off"

# What reopening a session does with work Caudra stopping cut off (a crash, a
# kill, or quitting mid-turn): `auto` resumes the main turn and its tasks at
# once, `ask` offers to, `off` only says what was interrupted. `auto` asks
# instead when a resume already crashed once, when the work is older than
# `resume_max_age_hours`, or when automatic wakes are suppressed.
# Type: string.
# resume_interrupted = "auto"

# Hours after which `resume_interrupted = "auto"` asks before resuming
# interrupted work instead of resuming it.
# Type: u64, at least 1.
# resume_max_age_hours = 24

# Learn from your conversations in the background: after enough new prompts,
# the Learn model reads them and keeps durable conventions, setup facts and
# preferences in the instruction files and memory notes. A project config can
# turn it off, never on.
# Type: bool.
# continual_learning = true

# Where continual learning may write: `project` (the committed AGENTS.md),
# `local` (AGENTS.local.md), `global` (~/.config/caudra/AGENTS.md), `memory`
# (memory notes). A project list can only narrow the global one.
# Type: string[].
# learning_targets = ["project", "local", "global", "memory"]

# Tools to withhold from the model: built-in names, `server.tool`, or
# `server.*` for a whole MCP server. A project list extends the global one.
# Type: string[].
# disabled_tools = []

[agent.messaging]

# Inbound cross-session messages: `auto` accepts only compatible trusted peers,
# `accept` allows wider delivery, `hold` requires approval, `refuse` rejects
# messages. Project settings may only tighten policy: accept < auto < hold <
# refuse. Needs `experimental.cross_session_messaging`; accepting messages can
# start billable turns.
# Type: string.
# inbound = "auto"

# Most peer messages a session admits per minute from all senders together.
# Project settings may only lower it.
# Type: usize, at least 1.
# inbound_per_minute = 64

# Most peer messages a session admits per minute from one sending session.
# Project settings may only lower it.
# Type: usize, at least 1.
# sender_per_minute = 16

# Most topic and broadcast publications a session sends per minute. Project
# settings may only lower it.
# Type: usize, at least 1.
# publish_per_minute = 16

# Most live sessions one topic or broadcast publication reaches. Extra
# recipients are skipped and counted. Project settings may only lower it.
# Type: usize, at least 1.
# max_fanout = 32

# Days the shared message history keeps a message. The newest message on each
# topic outlives this until `history_max_messages` evicts it. Global config
# only.
# Type: u64, at least 1.
# history_days = 30

# Most messages the shared message history keeps; the oldest go first. Global
# config only.
# Type: u64, at least 1.
# history_max_messages = 50000

[agent.shell_env]
# Variables local shell commands receive beyond the fixed allowlist. Only the
# global caudra.toml may hold this table.

# Host variables forwarded to local shell commands: exact names or NAME_*
# prefixes, never a bare *. Values come from Caudra's environment and the
# global .env, never a project .env.
# Type: string[].
# pass = []

# Values set for local shell commands; {env:NAME} reads Caudra's environment
# and {file:PATH} a file. Wins over pass.
# Type: table.
# set = {}

[agent.steering]
# Automatic steering repairs unusable model output and can add bounded guidance
# about repeated behavior. Each rule has its own table below.

# Master switch for automatic steering, including truncation recovery and
# repeat-policy blocking.
# Type: boolean.
# enabled = true

# Corrective continuations per externally initiated invocation. Zero prevents
# optional recovery continuations.
# Type: integer, 0 to 1024.
# max_recoveries = 32

# Advisory injections per invocation. Zero suppresses advisories.
# Type: integer, 0 to 1024.
# max_advisories = 4

# Consecutive turns carrying neither a tool call nor visible text before the
# run ends, whichever rule intervened. Zero disables the backstop.
# Type: integer, 0 to 1024.
# max_stalled_turns = 5

# Up to 256 exact `provider/model-id` keys, each with its own overrides.
# Type: table.
# models = {}

[agent.steering.rules.truncation]
# Continue output cut off by the response token limit, up to 3 corrective
# requests per externally initiated invocation.

# Explicit `false` disables this rule.
# Type: boolean.
# enabled = true

# Use built-in guidance when omitted. Custom text must be nonblank and at most
# 16,384 UTF-8 bytes.
# Type: string. Default: unset.
# prompt = <string>

# Actual truncation-correction requests per externally initiated invocation,
# shared across truncation episodes.
# Type: integer, 1 to 1024.
# max_attempts = 3

[agent.steering.rules.empty_response]
# Continue after empty output, with separate per-episode limits after recent
# tools and while idle.

# Explicit `false` disables this rule.
# Type: boolean.
# enabled = true

# Use built-in guidance when omitted. Custom text must be nonblank and at most
# 16,384 UTF-8 bytes.
# Type: string. Default: unset.
# prompt = <string>

# Empty-output continuations per episode after recent tool results.
# Type: integer, 1 to 1024.
# max_after_tools = 3

# Empty-output continuations per episode without recent tool results.
# Type: integer, 1 to 1024.
# max_idle = 2

# Continuations per episode after a response that carried no content at all.
# Clamped by the limit above; repeating an unchanged request is not a retry.
# Type: integer, 1 to 1024.
# max_barren = 1

# Non-padding history messages inspected for recent tool results.
# Type: integer, 1 to 4096.
# recent_tool_window = 5

[agent.steering.rules.repeated_tool_call]
# Refuse the third consecutive identical top-level tool name/input before
# execution. Native batch children do not acquire this hard blocker.

# Explicit `false` disables this rule.
# Type: boolean.
# enabled = true

# Use built-in guidance when omitted. Custom text must be nonblank and at most
# 16,384 UTF-8 bytes.
# Type: string. Default: unset.
# prompt = <string>

# Consecutive identical top-level calls. Refuse the call reaching this
# threshold.
# Type: integer, 2 to 1024.
# threshold = 3

[agent.steering.rules.protocol_mismatch]
# Correct an explicit provider tool-use indication with no actual tool calls,
# up to 2 continuations per episode.

# Explicit `false` disables this rule.
# Type: boolean.
# enabled = true

# Use built-in guidance when omitted. Custom text must be nonblank and at most
# 16,384 UTF-8 bytes.
# Type: string. Default: unset.
# prompt = <string>

# Protocol corrective continuations per episode.
# Type: integer, 1 to 1024.
# max_attempts = 2

[agent.steering.rules.missing_task_report]
# Request a missing task summary or required structured report, up to 2
# corrections.

# Explicit `false` disables this rule.
# Type: boolean.
# enabled = true

# Use built-in guidance when omitted. Custom text must be nonblank and at most
# 16,384 UTF-8 bytes.
# Type: string. Default: unset.
# prompt = <string>

# Additional report-correction prompts per task invocation.
# Type: integer, 1 to 1024.
# max_attempts = 2

[agent.steering.rules.abandoned_turn]
# Continue a turn that ended by announcing work the response never performed,
# up to 2 continuations per episode. Spending the allowance accepts the text
# rather than failing the turn.

# Explicit `false` disables this rule.
# Type: boolean.
# enabled = true

# Use built-in guidance when omitted. Custom text must be nonblank and at most
# 16,384 UTF-8 bytes.
# Type: string. Default: unset.
# prompt = <string>

# Continuations per episode after a turn that announced work instead of doing
# it.
# Type: integer, 1 to 1024.
# max_attempts = 2

[agent.steering.rules.repetition]
# Advise on short exact tool cycles, including normalized native batch leaf
# calls, or repeated normalized assistant text.

# Explicit `false` disables this rule.
# Type: boolean.
# enabled = true

# Use built-in guidance when omitted. Custom text must be nonblank and at most
# 16,384 UTF-8 bytes.
# Type: string. Default: unset.
# prompt = <string>

# Recent normalized leaf tool calls retained for cycle detection.
# Type: integer, 1 to 4096.
# window = 24

# Exact repetitions of a tool cycle needed for an advisory.
# Type: integer, 2 to 1024.
# cycle_repeats = 3

# Maximum cycle length in leaf calls. Candidate cycle lengths start at 2.
# Type: integer, 2 to 1024.
# max_cycle = 4

# Recent completed assistant responses retained for text repetition.
# Type: integer, 1 to 4096.
# text_window = 8

# Matching nontrivial normalized assistant responses needed for an advisory.
# Type: integer, 2 to 1024.
# text_repeats = 3

# Completed model responses between this rule's advisories.
# Type: integer, 1 to 1024.
# cooldown = 3

[agent.steering.rules.tool_planning]
# Advise after consecutive failed tool attempts across responses, including
# attempts with different tools or inputs. Any successful tool result ends the
# failure episode. Repeating a successful call is insufficient.

# Explicit `false` disables this rule.
# Type: boolean.
# enabled = true

# Use built-in guidance when omitted. Custom text must be nonblank and at most
# 16,384 UTF-8 bytes.
# Type: string. Default: unset.
# prompt = <string>

# Number of most recent leaf tool calls that must all have failed since the
# last response containing a successful result.
# Type: integer, 1 to 1024.
# after_calls = 6

# Distinct completed model responses represented by those failed calls.
# Type: integer, 1 to 1024.
# after_responses = 3

# Completed model responses between this rule's advisories.
# Type: integer, 1 to 1024.
# cooldown = 3

[agent.steering.rules.relative_paths]
# Suggest up to two shorter relative forms when file, patch, source-code, or
# shell `workdir` paths spell out the working directory or its parent. The hint
# appears once per context.

# Explicit `false` disables this rule.
# Type: boolean.
# enabled = true

# Use built-in guidance when omitted. Custom text must be nonblank and at most
# 16,384 UTF-8 bytes.
# Type: string. Default: unset.
# prompt = <string>

# Characters a relative form must save over its absolute path before the path
# is suggested.
# Type: integer, 1 to 1024.
# min_saved_chars = 12

[provider]

# Default model identifier (e.g. `anthropic/claude-sonnet-4-6`).
# Type: String. Default: unset.
# default_model = <String>

# Glob patterns for permitted qualified model specs; empty permits all models.
# Type: string[].
# allowed_models = []

# Glob patterns for excluded qualified model specs; exclusions take precedence.
# Type: string[].
# excluded_models = []

# HTTP connect timeout (seconds).
# Type: u64, at least 1.
# connect_timeout_secs = 10

# Longest the server may send nothing before the request is abandoned
# (seconds).
# Type: u64, at least 10.
# stream_timeout_secs = 300

[storage]

# Max total log size (MB).
# Type: u64, at least 1.
# max_log_bytes_mb = 200

# Max number of log files to keep.
# Type: u32, at least 1.
# max_log_files = 10

# Largest session Caudra will hydrate when opening one (MB), counted in
# uncompressed payload bytes rather than disk or memory. A session past this
# refuses to load; trim it or raise this.
# Type: u64, at least 64. Env: CAUDRA_MAX_EAGER_LOAD_MB.
# max_eager_load_mb = 1024

# Minimum severity written to the log file: trace, debug, info, warn, or error.
# RUST_LOG overrides it.
# Type: string.
# log_level = "info"

# Number of input history entries to retain.
# Type: usize, at least 10.
# input_history_size = 100

# Store session data in a temporary directory removed when Caudra exits.
# Type: bool.
# ephemeral = false

[storage.retention]
# `trim` and `forget` take keep policies in `restic forget` terms, such as
# `{ keep_last = 50, keep_within = "90d" }`. A session is kept when any rule
# matches.

# Evaluate policies per working directory (`directory`) or across every session
# (`none`).
# Type: string.
# group_by = "directory"

# Hours between background sweeps. A sweep reclaims freed space, and applies
# `trim` and `forget` when they are set. `0` disables the sweep;
# `caudra storage` commands still work.
# Type: u64.
# sweep_interval_hours = 24

# Sessions outside this policy lose file revert, tool output files, archives,
# and large rich outputs but stay resumable. Empty means never trim
# automatically.
# Type: table.
# trim = {}

# Sessions outside this policy are deleted. Empty means never delete
# automatically.
# Type: table.
# forget = {}

[storage.snapshots]
# Each tool call that changes files leaves a change record, which file revert
# undoes. A record over a limit is refused and its call runs unrecorded.

# Record each tool call's file changes so file revert can undo them, locally
# and remotely. `false` turns recording and file revert off and keeps records
# already made. `--no-snapshots` overrides this for one run.
# Type: bool.
# enabled = true

# Most file data one change record may cover, and the size each workspace's
# change store is trimmed to. A record over it is refused and its call runs
# unrecorded. Values above the store's limit are lowered to it, and locally
# that limit is the default.
# Type: u64, at least 1.
# max_bytes_mb = 512

# Most files one change record may cover, counted after ignore rules. A record
# over it is refused and its call runs unrecorded. Values above the store's
# limit are lowered to it, and locally that limit is the default.
# Type: u64, at least 1.
# max_files = 50000

# Largest file a change record stores. A larger file is left unrecorded, and a
# file revert across a call that changed it stops with a conflict. Values above
# the store's limit are lowered to it, and locally that limit is the default.
# Type: u64, at least 1.
# max_file_bytes_mb = 100

[telemetry]
# Each setting that names an environment variable gives way to it.

# Master switch.
# Type: bool. Env: CAUDRA_ENABLE_TELEMETRY.
# enabled = false

# Where metrics go: `otlp`, `console`, `none`, or a comma-separated mix.
# Type: string. Env: OTEL_METRICS_EXPORTER.
# metrics_exporter = "none"

# Where events go: `otlp`, `console`, `none`, or a comma-separated mix.
# Type: string. Env: OTEL_LOGS_EXPORTER.
# logs_exporter = "none"

# OTLP protocol: `grpc`, `http/protobuf`, or `http/json`. Required when an
# exporter is `otlp`.
# Type: string. Default: unset. Env: OTEL_EXPORTER_OTLP_PROTOCOL.
# protocol = <string>

# Collector endpoint. HTTP appends `/v1/metrics` and `/v1/logs`.
# Type: string. Default: unset. Env: OTEL_EXPORTER_OTLP_ENDPOINT.
# endpoint = <string>

# Extra headers sent with every export.
# Type: table. Env: OTEL_EXPORTER_OTLP_HEADERS.
# headers = {}

# Per-export request timeout (ms).
# Type: integer. Env: OTEL_EXPORTER_OTLP_TIMEOUT.
# timeout_ms = 10000

# Payload compression: `gzip` or `none`.
# Type: string. Env: OTEL_EXPORTER_OTLP_COMPRESSION.
# compression = "none"

# Metrics-only protocol override.
# Type: string. Default: unset. Env: OTEL_EXPORTER_OTLP_METRICS_PROTOCOL.
# metrics_protocol = <string>

# Metrics-only endpoint, used verbatim with no path appended.
# Type: string. Default: unset. Env: OTEL_EXPORTER_OTLP_METRICS_ENDPOINT.
# metrics_endpoint = <string>

# Metrics-only headers, merged over `headers`.
# Type: table. Env: OTEL_EXPORTER_OTLP_METRICS_HEADERS.
# metrics_headers = {}

# Metrics-only request timeout (ms).
# Type: integer. Default: unset. Env: OTEL_EXPORTER_OTLP_METRICS_TIMEOUT.
# metrics_timeout_ms = <integer>

# Logs-only protocol override.
# Type: string. Default: unset. Env: OTEL_EXPORTER_OTLP_LOGS_PROTOCOL.
# logs_protocol = <string>

# Logs-only endpoint, used verbatim with no path appended.
# Type: string. Default: unset. Env: OTEL_EXPORTER_OTLP_LOGS_ENDPOINT.
# logs_endpoint = <string>

# Logs-only headers, merged over `headers`.
# Type: table. Env: OTEL_EXPORTER_OTLP_LOGS_HEADERS.
# logs_headers = {}

# Logs-only request timeout (ms).
# Type: integer. Default: unset. Env: OTEL_EXPORTER_OTLP_LOGS_TIMEOUT.
# logs_timeout_ms = <integer>

# How often metrics are exported (ms).
# Type: integer. Env: OTEL_METRIC_EXPORT_INTERVAL.
# metrics_interval_ms = 60000

# Deadline for one metrics export, retries included (ms).
# Type: integer. Env: OTEL_METRIC_EXPORT_TIMEOUT.
# metrics_export_timeout_ms = 30000

# How often queued events are flushed (ms).
# Type: integer. Env: OTEL_LOGS_EXPORT_INTERVAL, OTEL_BLRP_SCHEDULE_DELAY.
# logs_interval_ms = 5000

# Event queue capacity. Events are dropped and counted when it is full.
# Type: integer. Env: OTEL_BLRP_MAX_QUEUE_SIZE.
# logs_max_queue_size = 2048

# Maximum events per export request.
# Type: integer. Env: OTEL_BLRP_MAX_EXPORT_BATCH_SIZE.
# logs_max_export_batch_size = 512

# Deadline for one events export, retries included (ms).
# Type: integer. Env: OTEL_BLRP_EXPORT_TIMEOUT.
# logs_export_timeout_ms = 30000

# Metric temporality: `delta` or `cumulative`.
# Type: string. Env: OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE.
# metrics_temporality = "delta"

# `service.name` on the exported resource.
# Type: string. Env: OTEL_SERVICE_NAME.
# service_name = "caudra"

# Extra resource attributes, your place for team or environment labels.
# Type: table. Env: OTEL_RESOURCE_ATTRIBUTES.
# resource_attributes = {}

# Attach `session.id` to metrics. Turn off to keep metric cardinality low.
# Type: bool. Env: OTEL_METRICS_INCLUDE_SESSION_ID.
# metrics_include_session_id = true

# Attach `app.version` to metrics.
# Type: bool. Env: OTEL_METRICS_INCLUDE_VERSION.
# metrics_include_version = false

# Include prompt text in `caudra.user_prompt` events. Off by default.
# Type: bool. Env: OTEL_LOG_USER_PROMPTS.
# log_user_prompts = false

# Include tool input in `caudra.tool_result` events. Off by default.
# Type: bool. Env: OTEL_LOG_TOOL_DETAILS.
# log_tool_details = false

# Character cap on any logged prompt or tool input.
# Type: integer. Env: CAUDRA_OTEL_CONTENT_MAX_LENGTH.
# content_max_length = 10240

[worktrees]

# What creates and removes worktrees for `/worktree`: `auto` uses Herdr inside
# a Herdr pane, Orca inside an Orca terminal, and git elsewhere, `git` always
# runs git.
# Type: string.
# backend = "auto"

# Where git-created worktrees go, as `<directory>/<repository>/<branch>`. A
# leading `~/` is your home directory.
# Type: string. Default: `<data dir>/worktrees`.
# directory = <string>

[decisions]
# Shared settings for the typed decision engine, which runs only while a
# decision endpoint is active. Endpoints are `[decisions.<name>]` tables in the
# global providers.toml. Timeouts and thresholds are global-only.

# Positive decision-request deadline in milliseconds, separate from shell
# execution timeouts. Tool search and every reranking feature except memory
# recall use `rerank_timeout_ms` instead.
# Type: integer.
# timeout_ms = 10000

# Positive deadline in milliseconds for tool search and the reranking features,
# per 16 candidates a request judges. A request holds up to 64, so a full one
# waits up to four budgets. Memory recall keeps `timeout_ms` because it delays
# the start of a turn. A `decide()` or `rerank()` call from a workflow or a
# `code` snippet may ask for up to the larger of the two.
# Type: integer.
# rerank_timeout_ms = 3000

# Retain bounded decision records in the local `caudra.db`.
# Type: boolean.
# log = false

# Positive retention period for decision records.
# Type: integer.
# log_retention_days = 90

[decisions.features]
# `off` turns a feature off, `shadow` collects predictions without applying
# them, `advise` adds caution or suggestions, and `enforce` applies the
# feature's own behavior. Each feature lists the modes it takes.

# Add warnings to an existing permission prompt without delaying the answer.
# Modes: off, shadow, advise.
# Type: string.
# permission_advice = "off"

# Escalate an eligible Auto call to a prompt on a flag or engine failure. Only
# `enforce` lets Auto run scripts and other lines that cannot be checked
# command by command. No answer channel means denial. Modes: off, shadow,
# enforce.
# Type: string.
# auto_screening = "off"

# Warn about possible project writes during Plan review only when
# `shell_writes` is configured. Never establish read-only authority. Modes:
# off, shadow, advise.
# Type: string.
# shell_effect = "off"

# Screen web, MCP and `code` output for text that tries to instruct the agent.
# `advise` adds caution to a flagged result and tightens upload/credential Auto
# screening for the session; the content stays available. `enforce` does the
# same, and also withholds from the model a flagged result, or one it could not
# screen in full, with a notice saying why. You still see the result on its
# card. Modes: off, shadow, advise, enforce.
# Type: string.
# content_screening = "off"

# Suggest a shortlisted skill. The agent still chooses whether to load it.
# Modes: off, shadow, advise.
# Type: string.
# skill_suggestions = "off"

# Skip an unlikely-to-pass goal evaluation within the continuation budget and
# continue work. Only the normal evaluator can certify completion. Modes: off,
# shadow, enforce.
# Type: string.
# goal_prescreen = "off"

# Choose a model job for a new unpinned subagent from its task label, mode,
# profile, and a redacted prompt excerpt. Explicit jobs, models chosen on the
# `task` call, profile pins, and continuations keep their routing. Modes: off,
# shadow, enforce.
# Type: string.
# subagent_routing = "off"

# At a main-session handoff with a usable question tool, advise adds one
# visible reminder per user-input episode to ask a live user question through
# that tool. Shadow only evaluates. Uses a redacted, bounded request and reply
# excerpt; uncertainty and failures leave the reply unchanged. Modes: off,
# shadow, advise.
# Type: string.
# question_tool_nudge = "off"

# Rerank the memory tool's keyword matches against the agent's query and omit
# those judged unrelated, saying how many were omitted. Sends the query and
# each candidate note's name, heading, and a redacted body excerpt. Modes: off,
# shadow, enforce.
# Type: string.
# memory_search = "off"

# Rerank session history search's keyword matches against the agent's query,
# drop those judged unrelated, and return the rest best first. Sends the query
# and each candidate's kind, tool, date, session title, and a redacted excerpt.
# Modes: off, shadow, enforce.
# Type: string.
# history_search = "off"

# Remind the agent of at most three saved notes likely relevant to a new user
# message, a todo step it starts, an unmet goal, or the work after compaction,
# without loading them. Sends a redacted excerpt of the request and of the open
# todos, the previous request when this one is short, and an unmet goal with
# the evaluator's reason, and each keyword-matched note's name, heading, and
# redacted excerpt. Modes: off, shadow, advise.
# Type: string.
# memory_recall = "off"

# When a new memory note is written, name existing notes that may cover the
# same topic in the write receipt. The write itself is never blocked or
# changed. Sends redacted excerpts of the new note and of keyword-matched
# notes. Modes: off, shadow, advise.
# Type: string.
# memory_duplicates = "off"

# During compaction, keep the tool results most relevant to the active goal,
# open todos, and latest request instead of only the newest, and keep the most
# relevant part of a long result. Sends redacted goal, todo, and request
# excerpts, and each recent result's tool name with redacted input and output
# excerpts. Modes: off, shadow, enforce.
# Type: string.
# compaction_retention = "off"

# When a saved tool output was truncated, point the agent to at most two
# omitted line ranges likely to matter. Nothing extra is removed. Sends
# redacted excerpts of the in-progress todos, latest request, tool input, and
# omitted lines. Modes: off, shadow, advise.
# Type: string.
# output_pointers = "off"

# In local sessions, judge the first 64 code-context rows against the task and
# keep only those judged relevant, best first. Sends the task and each row's
# symbol name, kind, path, line span, and reference counts, never source text.
# Modes: off, shadow, enforce.
# Type: string.
# source_code_context = "off"

[decisions.thresholds]
# Probabilities between 0 and 1. Flags trigger at or above their threshold,
# goal prescreening skips at or below its own, and enforcing rerank features
# prune or demote candidates below the relevance floor.

# Probability for a permission warning.
# Type: float.
# permission_flag = 0.85

# Probability for escalating an eligible Auto call.
# Type: float.
# auto_flag = 0.85

# Probability that sampled content attempts instruction injection.
# Type: float.
# content_injection = 0.9

# Probability that sampled content addresses the agent.
# Type: float.
# content_addressed_to_agent = 0.9

# Confidence required for skill suggestions. Subagent routing picks the Fast
# model when this much difficulty probability is at or below routine work, and
# the Best model when this much is on open-ended work. Memory duplicates and
# output pointers name a candidate only at or above this relevance probability.
# Type: float.
# routing_confidence = 0.9

# Skip an evaluator at or below this completion probability, within the
# continuation budget.
# Type: float.
# goal_skip_below = 0.05

# Optional project-write warning threshold. Omission leaves the warning
# disabled. No built-in enforcement threshold.
# Type: float. Default: unset.
# shell_writes = <float>

# Minimum noul score for a question-tool reminder. Provisional, not a
# calibrated probability; advise only, and errors or uncertainty never reopen
# the turn.
# Type: float.
# question_tool_nudge = 0.85

# Relevance probability below which an enforcing rerank feature prunes or
# demotes a candidate. Memory search omits it, compaction retention keeps a
# newer result in its place, and tool search ranks it after the candidates
# judged relevant, in keyword order. A list the engine could not judge is never
# emptied.
# Type: float.
# relevance_floor = 0.1

# Relevance probability a note needs before memory recall names it in a
# reminder.
# Type: float.
# recall_confidence = 0.75

[automations]
# Limits for automations, which need `automations = true` under [experimental].
# Only the global caudra.toml may hold this table.

# Most turns automations may start in one session per rolling hour, shared by
# all of its automations.
# Type: u32, 1 to 600.
# turns_per_hour = 20

# Stop automation-started turns after this many since the last human input.
# Human input resets the count, and unset means no cap.
# Type: u32, 1 to 10000. Default: unset.
# max_unattended_turns = <u32>

# Let `http()` in automations reach loopback and private network hosts. Without
# it, automations reach public hosts only.
# Type: bool.
# allow_private_network = false

[autopilot]
# Limits for autopilot. Only the global caudra.toml may hold this table.

# Most continuations autopilot may send per engagement before it ends. `0`
# removes the limit.
# Type: u32, 0 to 10000.
# max_continuations = 100

# Most spending in USD per autopilot engagement, counting API-billed spend
# only. Must be positive, and unset means no cap.
# Type: f64. Default: unset.
# max_cost = <f64>

# Most wall-clock minutes per autopilot engagement while the session is open.
# Unset means no cap.
# Type: u32, 1 to 525600. Default: unset.
# max_minutes = <u32>

# Retries for transient provider errors during an autopilot engagement.
# Type: u32, 0 to 20.
# error_retries = 5

[web]
# Private targets `webfetch` may reach in local sessions, besides the public
# internet. A redirect may lead to a listed target only from another one. Only
# the global caudra.toml may hold this table, and a change applies at once.

# Exact `HOST:PORT` targets on loopback or private networks that `webfetch` may
# reach, such as `"localhost:3000"` or `"[fd7a::1]:80"`. The port is required.
# Type: string[].
# allow_private_hosts = []

# Networks `webfetch` may reach on any port: `loopback`, `private`, `cgnat`,
# `tailscale`, or a CIDR such as `"10.20.0.0/16"`. Every private address a name
# resolves to must be inside one.
# Type: string[].
# allow_private_networks = []

[websearch]
# Backends of the websearch tool. Only the global caudra.toml may hold this
# table and its backend tables, because they hold credentials and decide where
# search queries go. A key left unset reads the variable in its Env column,
# from Caudra's environment or the global .env; prefer {env:NAME} references
# over credentials written out. Changes apply at once.

# How the ready backends answer: `single` uses the first ready one, `fallback`
# tries them in order when one fails, and `all` lets the model pick one, trying
# them in order when it does not.
# Type: string. Env: WORKCELL_WEBSEARCH_MODE.
# mode = "fallback"

# Backends in priority order: exa-mcp, searxng, exa, brave, kagi, or serpapi. A
# backend that is not set up correctly is skipped with a warning. An empty list
# turns websearch off.
# Type: string[]. Default: the comma-separated list in
# WORKCELL_WEBSEARCH_BACKEND, else ["exa-mcp"]. Env:
# WORKCELL_WEBSEARCH_BACKEND.
# backends = <string[]>

[websearch.searxng]
# Settings of the SearXNG backend.

# SearXNG base URL, HTTPS when a credential is set and never with credentials
# in it.
# Type: string. Default: unset. Env: SEARXNG_URL.
# url = <string>

# API key sent to SearXNG; set at most one of api_key, bearer_token, or user
# and password.
# Type: string. Default: unset. Env: SEARXNG_API_KEY.
# api_key = <string>

# Bearer token sent to SearXNG.
# Type: string. Default: unset. Env: SEARXNG_BEARER_TOKEN.
# bearer_token = <string>

# Basic authentication user, set with password.
# Type: string. Default: unset. Env: SEARXNG_USER.
# user = <string>

# Basic authentication password, set with user.
# Type: string. Default: unset. Env: SEARXNG_PASSWORD.
# password = <string>

[websearch.exa]
# Settings of the Exa backend.

# Exa API key.
# Type: string. Default: unset. Env: EXA_API_KEY.
# api_key = <string>

[websearch.brave]
# Settings of the Brave backend.

# Brave Search API key.
# Type: string. Default: unset. Env: BRAVE_API_KEY.
# api_key = <string>

[websearch.kagi]
# Settings of the Kagi backend.

# Kagi API key.
# Type: string. Default: unset. Env: KAGI_API_KEY.
# api_key = <string>

[websearch.serpapi]
# Settings of the SerpApi backend.

# SerpApi API key.
# Type: string. Default: unset. Env: SERPAPI_API_KEY.
# api_key = <string>

# SerpApi engine: google or bing.
# Type: string. Default: unset. Env: SERPAPI_ENGINE.
# engine = <string>

[plugins]
# Bundled tools are on by default. Turn one off with `enabled = false` in its
# own table, such as [plugins.websearch]. Names: bash, batch, edit, glob, grep,
# index, list, memory, question, read, sessions, skill, task, todo_write,
# tool_output, view_image, webfetch, websearch, write.

[plugins.index]

# Refuse to index files larger than this many MiB.
# Type: integer, 1 to 16.
# max_file_size_mb = 2

[plugins.skill]

# Offer the builtin caudra-plugin-dev skill for writing caudra plugins. Needs
# `experimental.lua_plugins`.
# Type: boolean.
# plugin_dev = false

# Offer the builtin caudra-workflow-dev skill for writing and running
# workflows. Needs `experimental.workflows`.
# Type: boolean.
# workflow_dev = true

# Offer the builtin caudra-automation-dev skill for writing automations. Needs
# `experimental.automations`.
# Type: boolean.
# automation_dev = true

# Offer the builtin caudra-docs skill: this build's user documentation, loaded
# one page or section at a time.
# Type: boolean.
# docs = true

# Offer the bundled pstack: its skills as the `pstack` skill, the `/pstack:`
# commands, the `pstack-poteto-agent` and `pstack-comment-sicko` task profiles,
# and its scripts as `pstack` calls in `code`.
# Type: boolean.
# pstack = false

[plugins.task]

# Max concurrently running subagents.
# Type: integer, at least 1.
# max_concurrent = 8
```

## permissions.toml

[`permissions.toml`](/docs/permissions/#toml-policy) holds permission rules for tools and MCP servers. Download this reference as [permissions.example.toml](/docs/permissions.example.toml).

```toml
# Every permissions.toml setting. Each one is commented out, so this file
# changes nothing until you edit it. `caudra config example permissions` prints
# it.
#
# Each commented table header starts an example. To use one, copy the header
# and the lines you need, remove the "#", and put your own names and values in
# place of the examples. Leave the lines without a "#" as they are. A required
# key shows a sample value, and any other key shows its default. A value in
# angle brackets, such as <string>, marks a key with no default to show.
#
# The global file is ~/.config/caudra/permissions.toml, or
# %APPDATA%\caudra\permissions.toml on Windows. A project
# .caudra/permissions.toml adds rules of its own. Deny and ask rules from both
# files apply, and a project shell allow waits until you trust the project
# policy in /permissions.
#
# A file that is unreadable, malformed, or newer than this build fails closed:
# Caudra denies tool calls until you fix it.
#
# Full reference: https://caudra.ai/docs/permissions/#toml-policy

version = 2

# What a call that no rule matches does: `allow`, `deny`, or `prompt`. `allow`
# acts as `prompt` and waits in /permissions for review, and a project cannot
# weaken a global `deny`.
# Type: string.
# default = "prompt"

# [shell]
# Each [TOOL] table holds the rules of one tool, such as [shell] here, and
# ["*"] holds rules for every tool. A shell allow or ask pattern is literal
# words with an optional final ` *`, such as `git status *`, in at most 8 words
# and 256 bytes. One invalid pattern makes the whole file fail closed.

# Scopes the tool may use without asking, such as `["git status *"]`, or `true`
# for every call. Only shell allows grant access, and a project shell allow
# waits until you trust the project policy. Other allows wait in /permissions
# for review.
# Type: bool | string[]. Default: unset.
# allow = <bool | string[]>

# Scopes that always ask, such as `["git push *"]`, or `true` for every call.
# Type: bool | string[]. Default: unset.
# ask = <bool | string[]>

# Scopes the tool may never use, such as `["rm -rf *"]`, or `true` for every
# call. A deny in either file blocks the whole call.
# Type: bool | string[]. Default: unset.
# deny = <bool | string[]>

# What a call of this tool that no rule matches does: `allow`, `deny`, or
# `prompt`. Unset follows the top-level `default`.
# Type: string. Default: unset.
# default = <string>

# [mcp.github]
# Each [mcp.SERVER] table holds the rules for the tools of one MCP server, such
# as [mcp.github] here. Name each tool as the server does, without the
# `github.` prefix.

# Tools to allow without asking: a list of names, one name, `"*"` for every
# tool, or `true` for every tool. MCP allows wait in /permissions for review.
# Type: bool | string | string[]. Default: unset.
# allow = <bool | string | string[]>

# Tools that always ask, in the same forms as `allow`.
# Type: bool | string | string[]. Default: unset.
# ask = <bool | string | string[]>

# Tools the model may never call, in the same forms as `allow`. `false` in any
# of the three adds nothing.
# Type: bool | string | string[]. Default: unset.
# deny = <bool | string | string[]>

# What a call to a tool of this server that no rule matches does: `allow`,
# `deny`, or `prompt`. Unset follows the top-level `default`.
# Type: string. Default: unset.
# default = <string>
```

## mcp.toml

[`mcp.toml`](/docs/mcp/) holds MCP servers. Download this reference as [mcp.example.toml](/docs/mcp.example.toml).

```toml
# Every mcp.toml setting. Each one is commented out, so this file changes
# nothing until you edit it. `caudra config example mcp` prints it.
#
# Each commented table header starts an example. To use one, copy the header
# and the lines you need, remove the "#", and put your own names and values in
# place of the examples. Leave the lines without a "#" as they are. A required
# key shows a sample value, and any other key shows its default. A value in
# angle brackets, such as <string>, marks a key with no default to show.
#
# The global file is ~/.config/caudra/mcp.toml, or %APPDATA%\caudra\mcp.toml on
# Windows. A project .caudra/mcp.toml adds servers, and a project server
# replaces a global server with the same name. A project server that runs a
# command, or that reaches a private address, waits until you review it in
# /mcp.
#
# When /mcp turns a server on or off, Caudra sets `enabled` in the file that
# defines the server and keeps your comments.
#
# Full reference: https://caudra.ai/docs/mcp/

version = 2

# [mcp.filesystem]
# Each [mcp.NAME] table is one server, and a stdio server like this one runs
# `command`. NAME is 1 to 64 ASCII letters, digits, and hyphens, and cannot be
# the name of a built-in tool. In the `code` tool the server is a Python
# namespace, and its tools are that namespace's async functions.

# Start the server. `/mcp` sets this key when it turns a server on or off.
# Type: bool.
# enabled = true

# Milliseconds to wait for each response from the server.
# Type: integer, 1 to 300000.
# timeout = 30000

# Stdio servers: the program and its arguments. It must not be empty. When
# `url` is also set, `command` wins.
# Type: string[]. Required.
# command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

# Stdio servers: environment variables for the server process, such as
# `{ GITHUB_TOKEN = "{env:GITHUB_TOKEN}" }`. In the global file, `{env:NAME}`
# reads a value from your environment and `{file:PATH}` reads a file. Other
# values are stored as plain text.
# Type: table.
# environment = {}

# [mcp.analytics]
# An HTTP server connects to `url` instead of running a command. `enabled` and
# `timeout` work here too.

# HTTP servers: the server URL. It must start with `http://` or `https://`.
# Type: string. Required.
# url = "https://mcp.example.com/mcp"

# HTTP servers: headers sent with every request, such as
# `{ Authorization = "Bearer {env:GITHUB_TOKEN}" }`. In the global file,
# `{env:NAME}` reads a value from your environment and `{file:PATH}` reads a
# file. Other values are stored as plain text.
# Type: table.
# headers = {}

# [mcp.analytics.oauth]
# The static OAuth client of [mcp.analytics], for a server that has no dynamic
# client registration. Without it, Caudra registers a client when the server
# asks for a login.

# The client ID of the app you registered with the server.
# Type: string. Required.
# client_id = "analytics-client"

# The client secret, for a confidential client. It is stored as plain text
# unless the global file reads it with `{env:NAME}` from your environment or
# with `{file:PATH}` from a file.
# Type: string. Default: unset.
# client_secret = <string>

# Pin the loopback port of the redirect URI, so you can register the URI in
# advance. Unset tries the default port, then any free port, so the URI can
# change between runs.
# Type: integer, at most 65535. Default: unset.
# callback_port = <integer>

# The path of the redirect URI. It must start with `/`.
# Type: string.
# callback_path = "/mcp/oauth/callback"

# The host name of the redirect URI, such as `localhost` when the server
# registered that form. The listener still binds to 127.0.0.1.
# Type: string.
# callback_hostname = "127.0.0.1"
```

## providers.toml

[`providers.toml`](/docs/providers/#providers-toml) holds model providers, their models, and their accounts. Download this reference as [providers.example.toml](/docs/providers.example.toml).

```toml
# Every providers.toml setting. Each one is commented out, so this file changes
# nothing until you edit it. `caudra config example providers` prints it.
#
# Each commented table header starts an example. To use one, copy the header
# and the lines you need, remove the "#", and put your own names and values in
# place of the examples. Leave the lines without a "#" as they are. A required
# key shows a sample value, and any other key shows its default. A value in
# angle brackets, such as <string>, marks a key with no default to show.
#
# The global file is ~/.config/caudra/providers.toml, or
# %APPDATA%\caudra\providers.toml on Windows. There is no project file.
#
# `caudra auth login` and `caudra auth logout` replace only the table they
# change, and keep the rest of the file and its comments.
#
# Full reference: https://caudra.ai/docs/providers/#providers-toml

version = 1

# [my-provider]
# Each top-level table is one provider, named by its slug. A new slug, such as
# my-provider here, adds a custom provider whose models are
# `my-provider/MODEL`. A built-in slug, such as [anthropic], changes that
# provider and ignores `protocol`, `api_key_env`, `discover_models`,
# `enable_free_models`, except that opencode reads `enable_free_models`. From
# [anthropic.model_defaults] and [[anthropic.models]], a built-in reads only
# these model keys: `context_window`, `max_output_tokens`. In an environment
# variable name, `<SLUG>` is the slug in capitals with `_` for `-`.

# The name pickers and auth status show.
# Type: string. Default: the built-in name, or the slug.
# display_name = <string>

# The wire format: `openai`, `openai-responses`, `anthropic`, or `google`.
# Type: string. Required.
# protocol = "openai"

# The API origin. Caudra appends the protocol paths.
# Type: string. Default: the plan URL, or the built-in URL. Env:
# <SLUG>_BASE_URL.
# base_url = <string>

# A built-in plan key, which sets the base URL and the default model.
# Type: string. Default: unset.
# plan = <string>

# The environment variable that holds the API key.
# Type: string. Default: `<SLUG>_API_KEY`.
# api_key_env = <string>

# An API key, stored as plain text unless you write `{env:NAME}` or
# `{file:PATH}` to read it from your environment or a file. Caudra tries the
# environment variable and saved credentials first.
# Type: string. Default: unset.
# api_key = <string>

# The model to use after login when none is saved yet, such as
# `my-provider/my-model`.
# Type: string. Default: unset.
# default_model = <string>

# Also list the models the provider's model endpoint reports.
# Type: bool.
# discover_models = false

# Opencode only. Show the free models of its catalog. Unset counts as `false`.
# Type: bool. Default: unset.
# enable_free_models = <bool>

# Aperture only. Overrides for the upstream providers it routes, keyed by
# upstream id.
# Type: table. Default: unset.
# overrides = <table>

# [my-provider.purposes]
# Which models of my-provider are small and which are flagships. Each key takes
# one prefix or a list.

# Model id prefixes for small, fast models, best first. A prefix covers every
# id that starts with it, and the first one also names the model that fills the
# slot, so it has to be a real id.
# Type: string | string[]. Default: unset.
# fast = <string | string[]>

# Model id prefixes for flagship models, best first. A prefix cannot also be in
# `fast`. A model a job is bound to in the picker wins over both lists.
# Type: string | string[]. Default: unset.
# best = <string | string[]>

# [my-provider.model_defaults]
# Model keys for every model of my-provider, including the ones only discovery
# finds. A [[my-provider.models]] entry wins key by key.

# Tokens of context.
# Type: integer. Default: discovered, or the protocol default.
# context_window = <integer>

# The most tokens one response may hold.
# Type: integer. Default: discovered, or the protocol default.
# max_output_tokens = <integer>

# Send tool examples as a structured field. It is off unless declared, because
# the protocol says nothing about the model behind it.
# Type: bool. Default: false.
# supports_tool_examples = <bool>

# The model accepts extended thinking. On `openai` and `openai-responses`, the
# default is on only when `reasoning_options` declares levels.
# Type: bool. Default: discovered, or the protocol default.
# supports_thinking = <bool>

# For an API that rejects requests with thinking off. It implies
# `supports_thinking` and raises thinking to minimal effort when it is off,
# compaction included.
# Type: bool. Default: false.
# requires_thinking = <bool>

# The model accepts images. When false, image input and `view_image` are off.
# Type: bool. Default: false.
# supports_vision = <bool>

# `anthropic` and `openai-responses` only. The model reads a PDF that
# `webfetch` attaches inside its tool result. When it is off, `webfetch`
# returns the text of the PDF instead.
# Type: bool. Default: false.
# supports_pdf = <bool>

# `openai-responses` only. The endpoint honours an explicit
# `prompt_cache_breakpoint`, so the system prompt closes with one.
# Type: bool. Default: false.
# supports_cache_breakpoints = <bool>

# The reasoning controls the model takes, such as
# `[{ type = "effort", values = ["low", "high"] }]`. A `type` is `toggle`,
# `effort` with `values`, or `budget_tokens` with an optional `min` and `max`.
# `[]` declares that it takes none, so Caudra sends no reasoning level.
# Type: table[]. Default: unset.
# reasoning_options = <table[]>

# USD per million input tokens.
# Type: float. Default: 0.
# pricing_input = <float>

# USD per million output tokens.
# Type: float. Default: 0.
# pricing_output = <float>

# USD per million tokens written to the prompt cache.
# Type: float. Default: 0.
# pricing_cache_write = <float>

# USD per million tokens read from the prompt cache.
# Type: float. Default: 0.
# pricing_cache_read = <float>

# USD per million input tokens in fast mode.
# Type: float. Default: unset.
# pricing_fast_input = <float>

# USD per million output tokens in fast mode.
# Type: float. Default: unset.
# pricing_fast_output = <float>

# [[my-provider.models]]
# One model my-provider serves, so repeat the table for each model. A key it
# leaves out comes from [my-provider.model_defaults], then from discovery, and
# then from the default shown.

# The model id, which makes the spec `SLUG/ID`.
# Type: string. Required.
# id = "my-model"

# Tokens of context.
# Type: integer. Default: discovered, or the protocol default.
# context_window = <integer>

# The most tokens one response may hold.
# Type: integer. Default: discovered, or the protocol default.
# max_output_tokens = <integer>

# Send tool examples as a structured field. It is off unless declared, because
# the protocol says nothing about the model behind it.
# Type: bool. Default: false.
# supports_tool_examples = <bool>

# The model accepts extended thinking. On `openai` and `openai-responses`, the
# default is on only when `reasoning_options` declares levels.
# Type: bool. Default: discovered, or the protocol default.
# supports_thinking = <bool>

# For an API that rejects requests with thinking off. It implies
# `supports_thinking` and raises thinking to minimal effort when it is off,
# compaction included.
# Type: bool. Default: false.
# requires_thinking = <bool>

# The model accepts images. When false, image input and `view_image` are off.
# Type: bool. Default: false.
# supports_vision = <bool>

# `anthropic` and `openai-responses` only. The model reads a PDF that
# `webfetch` attaches inside its tool result. When it is off, `webfetch`
# returns the text of the PDF instead.
# Type: bool. Default: false.
# supports_pdf = <bool>

# `openai-responses` only. The endpoint honours an explicit
# `prompt_cache_breakpoint`, so the system prompt closes with one.
# Type: bool. Default: false.
# supports_cache_breakpoints = <bool>

# The reasoning controls the model takes, such as
# `[{ type = "effort", values = ["low", "high"] }]`. A `type` is `toggle`,
# `effort` with `values`, or `budget_tokens` with an optional `min` and `max`.
# `[]` declares that it takes none, so Caudra sends no reasoning level.
# Type: table[]. Default: unset.
# reasoning_options = <table[]>

# USD per million input tokens.
# Type: float. Default: 0.
# pricing_input = <float>

# USD per million output tokens.
# Type: float. Default: 0.
# pricing_output = <float>

# USD per million tokens written to the prompt cache.
# Type: float. Default: 0.
# pricing_cache_write = <float>

# USD per million tokens read from the prompt cache.
# Type: float. Default: 0.
# pricing_cache_read = <float>

# USD per million input tokens in fast mode.
# Type: float. Default: unset.
# pricing_fast_input = <float>

# USD per million output tokens in fast mode.
# Type: float. Default: unset.
# pricing_fast_output = <float>

# [my-provider.switching]
# When Auto moves a session from one account of my-provider to another. It
# applies once the provider has two or more accounts and its selection is Auto.

# How Auto picks an account. `ordered` takes the first usable account in
# `accounts` order and returns to it after its quota resets. `paced` takes the
# account furthest ahead of its quota schedule, so capacity that is about to
# reset gets used first.
# Type: string.
# strategy = "ordered"

# The quota left in a watched window, in percent, below which a session moves
# to another account at its next request, even when that loses a warm prompt
# cache. It has to be lower than `prefer_below`.
# Type: integer, 0 to 99.
# switch_below = 5

# The quota left in a watched window, in percent, below which a session moves
# to another account once its prompt cache is cold anyway, such as in a new
# session or after compaction.
# Type: integer, 1 to 100.
# prefer_below = 20

# The quota windows the thresholds watch: `5h`, `weekly`, and `monthly`.
# Type: string[]. Default: every window the provider reports for the model.
# windows = <string[]>

# [[my-provider.accounts]]
# One account to sign in to my-provider with, so repeat the table for each
# account. The first entry has the highest priority. Accounts saved by
# `caudra auth login` that no entry lists come after these. A slug whose table
# holds only accounts and switching, such as a built-in one, stays the provider
# it was.

# The account name, which `caudra auth login --name` saves a credential under
# and `caudra auth use` selects. It takes lowercase letters, digits, `-`, and
# `_`, and `auto` is reserved.
# Type: string. Required.
# name = "personal"

# The environment variable that holds this account's API key. Leave it out to
# use the credential `caudra auth login` saved.
# Type: string. Default: unset.
# api_key_env = <string>

# Let Auto move a subscription session onto this pay-per-token account, which
# it does only when every subscription is spent. A subscription account ignores
# it.
# Type: bool.
# fallback = false

# [aperture.overrides.llmserver]
# Aperture routes the models of upstream providers as
# `aperture/UPSTREAM/MODEL`. Each [aperture.overrides.UPSTREAM] table overrides
# the models of one upstream, such as llmserver here.

# Tokens of context.
# Type: integer. Default: unset.
# context_window = <integer>

# The most tokens one response may hold.
# Type: integer. Default: unset.
# max_output_tokens = <integer>

# The models accept extended thinking.
# Type: bool. Default: unset.
# supports_thinking = <bool>

# The models accept images.
# Type: bool. Default: unset.
# supports_vision = <bool>

# The native provider an opaque upstream works like, such as `llama-cpp`,
# `google`, or `anthropic`. Caudra warns about a value it does not know and
# ignores it.
# Type: string. Default: unset.
# base = <string>

# The path Caudra sends ahead of each request, which Aperture appends to the
# upstream base URL. Set it to `""` when that URL already has its own path.
# Type: string. Default: `/v1`, `/v1beta` for Gemini routes, none for Anthropic
# and Z.AI.
# path_prefix = <string>

# Overrides for single models, keyed by model id, which win key by key. Quote
# an id that holds a dot, such as `models."qwen-3.6"`.
# Type: table.
# models = {}

# [decisions.my-endpoint]
# One decision endpoint, named my-endpoint here. Configuring it is the consent
# to send decision context to it, and `/decision-model` or
# `caudra decisions use` makes it the active one.

# Decision wire protocol: `typesafe` or `openai`. Never inferred from the URL
# or model.
# Type: string. Required.
# protocol = "typesafe"

# Decision API base URL. TypeSafe takes a root such as
# `https://api.typesafe.ai` and appends `/v1/systemone`. OpenAI takes a
# versioned base such as `https://api.openai.com/v1` and appends `/decisions`.
# A path prefix stays, and a full endpoint is rejected. `{env:NAME}` reads the
# base from the environment. HTTPS is required, except for numeric loopback
# HTTP or with `allow_http`. No credentials, query, fragment, whitespace, or
# control characters.
# Type: string. Required.
# base_url = "https://api.typesafe.ai"

# Decision model identifier, nonblank and without control characters.
# Type: string. Default: `jev-latest` for typesafe, `gpt-6-luna` for openai.
# model = <string>

# Environment variable holding the credential when no key is saved for this
# endpoint, never the credential itself. Project environment values are
# excluded. The presets set `TYPESAFE_API_KEY` and `OPENAI_API_KEY`.
# Type: string. Default: unset.
# api_key_env = <string>

# Opt-in for plain HTTP to a non-loopback host. Use only with transport
# protection you control, such as a trusted encrypted tunnel.
# Type: boolean.
# allow_http = false

# [switching]
# Lets Auto move a conversation to another provider's subscription when its own
# provider's subscriptions run low. It applies only to a provider whose
# selection is Auto, and every move loses the prompt cache.

# The provider slugs Auto may move a conversation across, in the order it tries
# them, when every subscription of the conversation's provider is spent or
# running low. The conversation moves to a subscription account of the next
# provider with quota left, on a model of the same size, and comes back once
# its own provider has quota again. Leave the table out to keep every
# conversation on its provider.
# Type: string[]. Required.
# providers = ["anthropic", "openai"]
```

## workcell.toml

[`workcell.toml`](/docs/remote-workspaces/#configure-a-profile) holds profiles for direct remote Workcell connections. Caudra reads it only when `experimental.remote_workcell` is on. Download this reference as [workcell.example.toml](/docs/workcell.example.toml).

```toml
# Every workcell.toml setting. Each one is commented out, so this file changes
# nothing until you edit it. `caudra config example workcell` prints it.
#
# Each commented table header starts an example. To use one, copy the header
# and the lines you need, remove the "#", and put your own names and values in
# place of the examples. Leave the lines without a "#" as they are. A required
# key shows a sample value, and any other key shows its default. A value in
# angle brackets, such as <string>, marks a key with no default to show.
#
# The global file is ~/.config/caudra/workcell.toml, or
# %APPDATA%\caudra\workcell.toml on Windows. There is no project file. It has
# to be a regular file, not a symlink, that you own and that group and others
# cannot write, so `chmod 600` suits it. It holds at most 256 KiB.
#
# Caudra reads this file only while `remote_workcell` is true under
# [experimental] in the global caudra.toml.
#
# Full reference: https://caudra.ai/docs/remote-workspaces/#configure-a-profile

version = 1

[workcell.profiles]
# Keep this header, because the file needs it even without profiles.

# [workcell.profiles.dev]
# Each [workcell.profiles.NAME] table is one profile, which
# `caudra --workcell-profile NAME` connects to. NAME is 1 to 64 ASCII letters,
# digits, ".", "-", and "_", and starts with a letter or digit. `endpoint` is
# at most 2048 bytes, and each expected ID at most 512.

# The Workcell endpoint: HTTPS with any host, or HTTP on a numeric loopback
# address such as `127.0.0.1`. Caudra refuses `localhost`, user information, a
# query, and a fragment.
# Type: string. Required.
# endpoint = "https://workcell.example/mcp"

# The working directory, relative to the Workcell root, where `.` is the root
# itself. It cannot start with `/` or hold `..`.
# Type: string. Required.
# cwd = "projects/app"

# The saved bearer credential, as `credential:NAME`, which
# `caudra auth workcell set NAME` creates. A loopback profile needs one too.
# Type: string. Required.
# credential_ref = "credential:dev"

# An identity check: the connection fails unless the server reports this ID.
# Type: string. Default: unset.
# expected_server_id = <string>

# An identity check: the connection fails unless the workspace reports this ID.
# Type: string. Default: unset.
# expected_workspace_id = <string>
```

## sandboxes.toml

[`sandboxes.toml`](/docs/sandboxes/#configuration-schema) holds managed sandbox providers, networks, transfers, and profiles. Caudra reads it only when `experimental.sandboxes` is on. Download this reference as [sandboxes.example.toml](/docs/sandboxes.example.toml).

```toml
# Every sandboxes.toml setting. Each one is commented out, so this file changes
# nothing until you edit it. `caudra config example sandboxes` prints it.
#
# Each commented table header starts an example. To use one, copy the header
# and the lines you need, remove the "#", and put your own names and values in
# place of the examples. Leave the lines without a "#" as they are. A required
# key shows a sample value, and any other key shows its default. A value in
# angle brackets, such as <string>, marks a key with no default to show.
#
# The global file is ~/.config/caudra/sandboxes.toml, or
# %APPDATA%\caudra\sandboxes.toml on Windows. There is no project file. It has
# to be a regular file with one link, owned by you, that group and others have
# no access to, so `chmod 600` suits it. The directory that holds it has to be
# yours, and each directory above that has to be yours or root's. None of them
# may let group or others write, except a sticky directory that root owns, such
# as /tmp. No part of the path may be a symlink.
#
# The file holds at most 256 records and 1024 KiB, and one record at most 64
# KiB. A network lists at most 256 domains and CIDRs together, and a transfer
# at most 128 globs.
#
# `/sandbox` saves this file and keeps your comments. Saving creates no VM.
#
# Caudra reads this file only while `sandboxes` is true under [experimental] in
# the global caudra.toml.
#
# Full reference: https://caudra.ai/docs/sandboxes/#configuration-schema

version = 1

[sandbox]
# Keep this header, because the file needs it even without records.

# [sandbox.providers.local]
# Each [sandbox.providers.NAME] table is one sandbox provider. Every kind of
# record has names of its own: 1 to 64 ASCII letters, digits, ".", "-", and
# "_", starting with a letter or digit.

# The provider type. `e2b-libvirt` is the only one.
# Type: string. Required.
# kind = "e2b-libvirt"

# The origin of the lifecycle API: HTTPS, or HTTP on a numeric loopback
# address, with no path, credentials, query, or fragment. Caudra refuses
# `http://localhost`.
# Type: string. Required.
# api_endpoint = "http://127.0.0.1:3000"

# The origin of the sandbox proxy, with the same rules as `api_endpoint`.
# Type: string. Required.
# proxy_endpoint = "http://127.0.0.1:49983"

# The lifecycle API key, as `sandbox-api:NAME`, not a Workcell
# `credential:NAME`. `caudra auth sandbox generate NAME` or
# `caudra auth sandbox set NAME` saves it.
# Type: string. Required.
# credential_ref = "sandbox-api:local"

# [sandbox.networks.default]
# Each [sandbox.networks.NAME] table is one network policy, which profiles can
# share.

# `required` enforces the lists below, so empty lists deny all traffic. `off`
# enforces nothing, and then both lists must be empty and `tls_mode` must stay
# `sni-only`.
# Type: string. Required.
# enforcement = "required"

# `sni-only` or `mitm`.
# Type: string.
# tls_mode = "sni-only"

# Domains the sandbox may reach, such as `github.com` or
# `*.githubusercontent.com`.
# Type: string[].
# domains = []

# Address ranges the sandbox may reach, such as `10.0.0.0/8`.
# Type: string[].
# cidrs = []

# [sandbox.transfers.default]
# Each [sandbox.transfers.NAME] table is one file transfer policy, which
# profiles can share. Protected paths stay protected even with `exclude = []`.

# Leave out the files that `.gitignore` ignores.
# Type: bool.
# respect_gitignore = true

# `ask` offers to copy the project into a new sandbox, and `none` skips the
# offer. Later transfers work either way.
# Type: string.
# initial_seed = "ask"

# Only `false` works, because Caudra never deletes files on its own.
# Type: bool.
# delete_extraneous = false

# Relative globs of paths never to copy. A list you set replaces the one shown.
# A glob cannot start with `/` or `!`, or hold `.`, `..`, `\`, or `:`.
# Type: string[].
# exclude = ["**/.git/**", "**/.env*", "**/target/**", "**/node_modules/**", "**/.venv/**", "**/.ssh/**", "**/.aws/**", "**/.caudra/**", "**/*.pem", "**/*.key"]

# [sandbox.profiles.dev]
# Each [sandbox.profiles.NAME] table is one launch profile, which
# `caudra --sandbox NAME` starts. The file is refused while a record the
# profile names is missing. The provider checks the template, the resources,
# and the lease only at launch.

# The name of a [sandbox.providers.NAME] record.
# Type: string. Required.
# provider = "local"

# A template ID from the provider catalog. Each launch uses the revision the
# catalog lists at that time.
# Type: string. Required.
# template = "caudra-rust"

# Virtual CPUs.
# Type: integer, at least 1. Required.
# cpus = 4

# Memory in MiB.
# Type: integer, at least 1. Required.
# memory_mib = 4096

# Virtual disk size in GiB. It reserves no host space.
# Type: integer, at least 1. Required.
# disk_gib = 20

# The working directory, relative to the Workcell root, where `.` is the root
# itself.
# Type: string. Required.
# cwd = "."

# The name of a [sandbox.networks.NAME] record.
# Type: string. Required.
# network = "default"

# The name of a [sandbox.transfers.NAME] record.
# Type: string. Required.
# transfer = "default"

# Keep the disk when the sandbox pauses or its lease ends. Only a persistent
# sandbox can pause.
# Type: bool.
# persistent = true

# Seconds the sandbox may run before its lease ends. `0` never ends, which
# needs a provider without a lease cap.
# Type: integer. Required.
# running_ttl_seconds = 3600

# What happens to the sandbox when Caudra exits. `detach` is the only value.
# Type: string.
# on_exit = "detach"

# Guest variables forwarded to shell commands in this sandbox, resolved inside
# the guest: pass takes names, NAME_* or *; set takes literal non-secret
# values. Workcell's own settings, Bash startup names, and proxies are never
# forwarded.
# Type: table. Default: pass everything.
# shell_env = <table>
```
