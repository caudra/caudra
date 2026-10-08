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
# [experimental], [automations], and the ones marked global-only.
#
# caudra.toml holds the settings only you write. Other files hold what Caudra
# writes itself, or what needs rules of its own: permissions.toml (permission
# rules for tools and MCP servers), mcp.toml (MCP servers), providers.toml
# (model providers and their models), workcell.toml (profiles for direct remote
# Workcell connections), sandboxes.toml (managed sandbox providers, networks,
# transfers, and profiles), init.lua (Lua code that sets up plugins), .env
# (environment variables, such as API keys, for any the environment does not
# set), commands/ (custom slash commands, one Markdown file each).
# `caudra config files` shows where each one lives, and
# `caudra config example FILE` prints the reference of a TOML file.
#
# Full reference: https://caudra.ai/docs/configuration/

version = 1

# Start every session with YOLO mode (skip permission prompts, deny rules still
# apply); global config only.
# Type: bool.
# always_yolo = false

# Start every session with Auto permission mode (preserve required prompts and
# screen unmatched calls); global config only. Needs
# `experimental.decision_engine`, otherwise sessions start in Ask.
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

# Turn on the decision engine and Auto mode.
# Type: bool.
# decision_engine = false

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
# program status when supported, otherwise uses OSC 9 or BEL. Native Herdr
# reporting takes precedence in a Herdr pane. Explicit osc9, bell, and off skip
# program status detection.
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

# Duration of ordinary status-bar messages (ms). Confirmation prompts use a
# fixed 3-second window.
# Type: u64.
# flash_duration_ms = 10000

# How long Ctrl+X waits before listing the chords it can still reach (ms). 0
# shows the list at once.
# Type: u64.
# which_key_delay_ms = 250

# Typewriter effect speed (ms/char).
# Type: u64.
# typewriter_ms_per_char = 4

# Lines per mouse wheel scroll.
# Type: u32, at least 1.
# mouse_scroll_lines = 3

# Rows of body a shell, python_execution or task card draws. The window follows
# new output while it sits at the bottom and pauses when scrolled up. Click
# inside a window to give it the wheel, which passes back to the transcript at
# either edge, and drag the bar in its last column to move it directly. `0`
# turns scrolling off, restoring the `ui.tool_output_lines` budget for those
# tools. A write is never windowed: it is drawn whole at any setting, as the
# file it created or as the diff of what it replaced.
# Type: u32.
# scroll_card_lines = 10

# Tools whose card never opens on its own: the call stays a single row in every
# view mode until you click it. A server-qualified name still matches, so
# `file_read` also covers `mcp_File_read`. Set to `[]` to opt out.
# Type: string[].
# always_collapsed = ["file_read", "file_glob", "file_grep", "file_index", "webfetch"]

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

# Clock format for timestamps: "12h", "24h", or "system" (follow the OS
# preference, 24h when unknown).
# Type: String.
# clock_format = "system"

# Check GitHub releases in the background at interactive startup and show an
# update notice. Uses a shared 24-hour cache and never installs automatically.
# Set false to disable.
# Type: bool. Env: CAUDRA_ENABLE_UPDATE_CHECK.
# update_check = true

# Release channel: `auto` follows stable from a stable build and preview from a
# prerelease, including graduation to stable. `stable` excludes prereleases.
# `preview` includes prereleases and stable releases.
# Type: string.
# update_channel = "auto"

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

# Rows for `python_execution`.
# Type: usize, at least 1.
# python_execution = 5

# Rows for `task`, `task_control`.
# Type: usize, at least 1.
# task = 12

# Rows for `file_index`, `code_map`, `code_context`, `code_refs`,
# `code_impact`, `code_expand`.
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
# output.
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

# Task delivery: sync waits for the completed result, auto lets the model
# choose, async returns an admission receipt.
# Type: string.
# task_execution = "auto"

# Shell delivery: sync waits for termination, auto routes by requested timeout,
# async returns an admission receipt.
# Type: string.
# shell_execution = "auto"

# Requested shell timeout above which auto delivery returns an admission
# receipt; independent of the enforced execution deadline.
# Type: u64, at least 1.
# shell_async_threshold_secs = 120

# Name a new session by summarizing its first prompt with the Title model.
# Type: bool.
# generate_titles = true

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

# When the on-demand built-in tools start outside the request array: `auto`
# defers them for a small model or one with no supply metadata and declares
# them upfront for a known non-small model, `always` defers for every model,
# `never` declares them upfront.
# Type: string.
# defer_builtin_tools = "auto"

# GPT Image 2.5 model behind `image_generate`: `sunburst` is the most capable
# and the better editor, `flare` is faster at the same price.
# Type: string.
# image_model = "sunburst"

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
# Suggest up to two shorter relative forms when file, patch, code-graph, or
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
# a Herdr pane and git elsewhere, `git` always runs git.
# Type: string.
# backend = "auto"

# Where git-created worktrees go, as `<directory>/<repository>/<branch>`. A
# leading `~/` is your home directory.
# Type: string. Default: `<data dir>/worktrees`.
# directory = <string>

[decisions]
# The typed decision engine. It needs `decision_engine = true` under
# [experimental]. Connection settings and thresholds are global-only.

# Global-only decision wire protocol: `typesafe` or `openai`. Never inferred
# from the URL, model, or environment. Selecting a protocol does not enable
# decisions.
# Type: string.
# protocol = "typesafe"

# Global-only decision API base URL. TypeSafe uses a root such as
# `https://api.typesafe.ai` and appends `/v1/systemone`; OpenAI uses a
# versioned base such as `https://api.openai.com/v1` and appends `/decisions`.
# Path prefixes are preserved; full endpoints are rejected. Only the selected
# protocol's `TYPESAFE_BASE_URL` or `OPENAI_BASE_URL`, from the process or
# global environment, replaces an explicitly configured base. Environment alone
# never enables decisions. HTTPS required except for numeric loopback HTTP or
# explicit `allow_http` consent. No credentials, query, fragment, whitespace,
# or control characters.
# Type: string. Default: unset.
# base_url = <string>

# Decision model identifier, nonblank and without control characters.
# Type: string. Default: `jev-latest` for typesafe; `gpt-6-luna` for openai.
# model = <string>

# Environment variable containing the optional credential, never the credential
# itself. Project environment values are excluded.
# Type: string. Default: `TYPESAFE_API_KEY` for typesafe; `OPENAI_API_KEY` for
# openai.
# api_key_env = <string>

# Explicit global consent to send decision context to a non-loopback endpoint.
# Type: boolean.
# allow_remote = false

# Global-only opt-in for non-loopback HTTP. Also requires
# `allow_remote = true`. Use only with transport protection you control, such
# as a trusted encrypted tunnel.
# Type: boolean.
# allow_http = false

# Positive decision-request deadline in milliseconds, separate from shell
# execution timeouts.
# Type: integer.
# timeout_ms = 800

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

# Add caution to flagged web/MCP output and tighten upload/credential Auto
# screening for the session. Content remains available. Modes: off, shadow,
# advise.
# Type: string.
# content_screening = "off"

# Advise with local shell estimates. Enforce may fill an omitted timeout and
# select delivery at admission. Explicit timeouts stay unchanged. Modes: off,
# shadow, advise, enforce.
# Type: string.
# shell_duration = "off"

# Rerank the existing lexical tool shortlist. This neither loads arbitrary
# names nor grants execution permission. Modes: off, shadow, enforce.
# Type: string.
# tool_search = "off"

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
# profile, and a redacted prompt excerpt. Explicit jobs, profile pins, and
# continuations keep their routing. Modes: off, shadow, enforce.
# Type: string.
# subagent_routing = "off"

# At a main-session handoff with a usable question tool, advise adds one
# visible reminder per user-input episode to ask a live user question through
# that tool. Shadow only evaluates. Uses a redacted, bounded request and reply
# excerpt; uncertainty and failures leave the reply unchanged. Modes: off,
# shadow, advise.
# Type: string.
# question_tool_nudge = "off"

[decisions.thresholds]
# Probabilities between 0 and 1. Flags trigger at or above their threshold, and
# goal prescreening skips at or below its own.

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

# Probability that a shell command runs until stopped.
# Type: float.
# shell_endless = 0.9

# Probability mass a duration bound needs before an engine estimate is used.
# Type: float.
# shell_duration = 0.9

# Confidence required for tool search and skill suggestions. Tool-search choice
# probability must also meet it. Subagent routing picks the Fast model when
# this much difficulty probability is at or below routine work, and the Best
# model when this much is on open-ended work.
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

version = 1

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
# `github__` prefix.

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

version = 1

# Defer MCP tools behind `tool_search` only when the servers offer more than
# this many. `0` always defers. A project value replaces the global one.
# Type: integer.
# defer_tools = 10

# [mcp.filesystem]
# Each [mcp.NAME] table is one server, and a stdio server like this one runs
# `command`. NAME is 1 to 64 ASCII letters, digits, and hyphens, and cannot be
# the name of a built-in tool. The tools of the server are named NAME__TOOL.

# Start the server. `/mcp` sets this key when it turns a server on or off.
# Type: bool.
# enabled = true

# Milliseconds to wait for each response from the server.
# Type: integer, 1 to 300000.
# timeout = 30000

# Load every tool of the server up front instead of through `tool_search`.
# Type: bool.
# always_load = false

# Stdio servers: the program and its arguments. It must not be empty. When
# `url` is also set, `command` wins.
# Type: string[]. Required.
# command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

# Stdio servers: environment variables for the server process, such as
# `{ GITHUB_TOKEN = "..." }`. Values are stored as plain text.
# Type: table.
# environment = {}

# [mcp.analytics]
# An HTTP server connects to `url` instead of running a command. `enabled`,
# `timeout`, and `always_load` work here too.

# HTTP servers: the server URL. It must start with `http://` or `https://`.
# Type: string. Required.
# url = "https://mcp.example.com/mcp"

# HTTP servers: headers sent with every request, such as
# `{ Authorization = "Bearer ..." }`. Values are stored as plain text.
# Type: table.
# headers = {}

# [mcp.analytics.oauth]
# The static OAuth client of [mcp.analytics], for a server that has no dynamic
# client registration. Without it, Caudra registers a client when the server
# asks for a login.

# The client ID of the app you registered with the server.
# Type: string. Required.
# client_id = "analytics-client"

# The client secret, for a confidential client. It is stored as plain text.
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

[`providers.toml`](/docs/providers/#providers-toml) holds model providers and their models. Download this reference as [providers.example.toml](/docs/providers.example.toml).

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
# `caudra auth login` and `caudra auth logout` rewrite this file and drop its
# comments, so keep a copy of the ones you want.
#
# Full reference: https://caudra.ai/docs/providers/#providers-toml

version = 1

# [my-provider]
# Each top-level table is one provider, named by its slug. A new slug, such as
# my-provider here, adds a custom provider whose models are
# `my-provider/MODEL`. A built-in slug, such as [anthropic], changes that
# provider and ignores `protocol`, `api_key_env`, `discover_models`, `models`,
# `enable_free_models`, except that opencode reads `enable_free_models`. In an
# environment variable name, `<SLUG>` is the slug in capitals with `_` for `-`.

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

# An API key, stored as plain text. Caudra tries the environment variable and
# saved credentials first.
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

# The model accepts extended thinking.
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

# The model accepts extended thinking.
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
```
