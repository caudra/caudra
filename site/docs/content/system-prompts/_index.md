+++
title = "System Prompt Profiles"
weight = 45
[extra]
group = "Guides"
+++

# System Prompt Profiles

System prompt profiles change main and task prompts without copying Caudra's built-in prompts. They can also choose which tools are available and which schemas load on demand. Overlay profiles preserve current tool guidance, environment details, instruction files, plugin hints, and mode text.

Profiles are Markdown files in the user config directory:

```text
~/.config/caudra/system-prompts/review.md
```

The filename is the profile name. Names may contain ASCII letters, digits, `-`, and `_`, with a maximum length of 64 characters. `builtin` is reserved.

## Add guidance

Plain Markdown creates an overlay profile:

```markdown
---
description: Review code without modifying it
layout: overlay
---

# Review profile

Prioritize correctness bugs, regressions, and missing tests.
Do not edit files unless the user explicitly asks.
```

`layout: overlay` is the default, so the frontmatter is optional. Caudra inserts the profile after runtime context and before the mode section.

The system prompt is identical in plan and build mode, and carries no working directory, date, or model. All of those are announced in the conversation instead, and re-announced only when they change, so switching mode, crossing midnight, or changing model does not invalidate the prompt cache. Editing an instruction file mid-session is announced the same way, as a diff against the copy the prompt already carries. Task prompts follow the same rule: a subagent is told its environment and the mode it was granted in the conversation.

Those announcements arrive wrapped in `<system-reminder>`. They are appended to the conversation and never edited, so a kind is restated only when its content changes or a compaction summarized the block in force, and earlier blocks of the same kind remain as history. The most recent block of a kind is the only one in force; the system prompt tells the model this, and that a reminder is not the user talking.

## Control the layout

Set `layout: custom` to compose the full main-agent prompt from dynamic Caudra components:

```markdown
---
description: Security-focused reviewer
layout: custom
---

{{caudra.identity}}

# Role

Act as a security-focused reviewer. Report findings before summaries.

{{caudra.context}}
{{caudra.tools}}
{{caudra.conventions}}
{{caudra.completion}}
{{caudra.plan}}
```

| Directive | Content |
| --- | --- |
| `{{caudra.default}}` | The complete current built-in prompt |
| `{{caudra.identity}}` | Resolved built-in or plugin identity |
| `{{caudra.style}}` | Tone and professional objectivity |
| `{{caudra.tools}}` | Tool rules, plugin hints, and efficient tools |
| `{{caudra.conventions}}` | Git, security, and plugin conventions |
| `{{caudra.completion}}` | Completion requirements |
| `{{caudra.context}}` | Instruction files and plugin runtime context. In a task prompt, the instruction files alone |
| `{{caudra.plan}}` | The system-reminder contract, and how plan and build mode work. Identical in both modes |

A directive expands only when it occupies a complete line. Prefix it with `\` to keep it literal, for example `\{{caudra.tools}}`.

Each component directive may appear once. `{{caudra.default}}` cannot be combined with another component directive. Custom layouts may omit components, though omitting `tools`, `context`, or `plan` can remove information the agent relies on.

Profile files must be valid UTF-8 and no larger than 64 KiB. Unknown frontmatter fields and unknown directives make the profile invalid.

## Choose tool availability

Add a `tools` block to either layout. It covers native tools, local and remote Workcell tools, MCP tools, Lua/plugin tools, and local callbacks. Omitting it, or writing `tools: {}`, preserves the existing loading behavior.

```yaml
tools:
  default: inherit
  groups:
    files: disabled
  overrides:
    file_read: eager
    "github.*": lazy
    "github.delete_issue": disabled
```

| Field | Values | Default |
| --- | --- | --- |
| `tools.default` | `inherit`, `eager`, `lazy`, `disabled` | `inherit` |
| `tools.groups` | Group name mapped to `eager`, `lazy`, or `disabled` | Empty |
| `tools.overrides` | Tool selector mapped to `eager`, `lazy`, or `disabled` | Empty |

An `eager` tool starts with its full schema in the request. A `lazy` tool is callable and searchable, but initially contributes only a catalog summary. Calling a lazy tool directly by its known name is valid and loads its schema. A `disabled` tool is absent from schemas and search, and calls are refused before argument repair, permission prompts, or execution.

Loaded schemas remain session-local. Restored history and MCP reconnects cannot re-enable disabled tools.

Resolution goes from the most specific rule to the least specific:

1. Exact tool override.
2. MCP `server.*` override.
3. Built-in group.
4. Profile default.
5. Existing loading preferences when the default is `inherit`.

Explicit `eager` and `lazy` settings override model-class deferral, `agent.defer_builtin_tools`, MCP thresholds and `always_load`, and the eager hint from `--allowed-tools`. Loading remains separate from [synchronous and background execution](/docs/sessions/#background-tasks).

MCP selectors use canonical `server.tool` names, not provider wire aliases. Plugin and custom tools use their exact registered names. Unknown groups, invalid states, malformed selectors, duplicate keys, and reserved infrastructure selectors are errors. Bare custom names must resolve in the runtime registry before the profile can run. Qualified MCP selectors may precede a server connection, but an unavailable tool grants no access. Arbitrary globs, custom groups, and profile inheritance are unsupported.

### Tool groups

| Group | Members |
| --- | --- |
| `web` | `websearch`, `webfetch` |
| `files` | `file_read`, `file_write`, `file_edit`, `file_apply_patch`, `file_glob`, `file_grep`, `file_index` |
| `code_graph` | `code_map`, `code_context`, `code_refs`, `code_impact`, `code_expand` |
| `execution` | `shell`, `python_execution`, `execution_environment` |
| `delegation` | `task`, `task_control`, `workflow` |
| `support` | `batch`, `question`, `todo_write`, `plan` |
| `images` | `view_image`, `image_generate` |
| `messaging` | `list_sessions`, `send_message` |

Select `memory` and `skill` individually. Messaging is optional and separate from support and delegation. It still requires its experimental opt-in, an eligible main-session runtime, peer inbound controls, and outgoing permissions.

Groups are authoring shortcuts. They do not guarantee that every member exists in a runtime or cause members to load together. The existing code-graph loading bundle remains, limited to its eligible lazy members. A custom tool using a built-in name does not acquire that built-in's group or infrastructure privileges.

### Restrictions and infrastructure

A profile cannot restore tools removed by global or CLI restrictions, disabled experimental features, model compatibility, execution policy, audience, or mode. A task in plan mode stays read-only. Permissions still authorize each call, and explicit denies remain effective.

The trusted `tool_output` pager stays available for truncated results. Host-required task-report and structured-output sinks also remain where their protocol needs them. `tool_search` appears only while an eligible lazy catalog has pending tools. These are infrastructure exceptions, identified by their trusted bindings. `todo_write`, `question`, `batch`, `plan`, `memory`, `skill`, `task`, and `workflow` require normal opt-in under `default: disabled`.

Tool availability is not a sandbox. Permitted shell commands, trusted plugins, and delegated tasks can perform broader work. Profiles do not isolate prompt history or sandbox plugin code.

### Web researcher

Save this as `system-prompts/researcher.md`:

```markdown
---
description: Research online sources and report evidence
tools:
  default: disabled
  groups:
    web: eager
    support: lazy
  overrides:
    todo_write: eager
---
Research the question using online sources. Cite evidence and distinguish uncertainty.
```

Remove `support` and the override for a web-only profile with just the required infrastructure.

### Main-agent scheduler

Save this as `system-prompts/scheduler.md`:

```markdown
---
description: Coordinate specialists and track the plan
tools:
  default: disabled
  groups:
    delegation: eager
    support: eager
  overrides:
    workflow: lazy
    plan: lazy
---
Delegate research and implementation to appropriate task profiles.
Coordinate results, maintain the todo list, and keep the session plan current.
```

This actor cannot call file, web, or shell tools directly. It can delegate to a task using `profile: researcher`, another coding profile, or `profile: builtin`. Each task resolves its own selected profile against the inherited CLI, config, mode, and security restrictions. The parent's profile-local mask is not inherited as a global restriction. Tool compatibility is recalculated for the worker's model. Omitting `profile` still selects the parent's profile, so an omitted profile keeps the scheduler's tool choices.

This independence applies to Caudra's audited task delegation path, including workflow-created tasks and nested `task` calls. Ordinary Lua tool calls retain the current actor's policy. Generic/custom subagent APIs can only narrow their caller's effective access.

The single [`plan` tool](/docs/tools/#plan) reads or replaces this session's plan through the same interface for local and remote workspaces. It has no model-supplied path, reference, or session selector. It is available once the session has a plan, which it gets the first time it enters Plan. The main agent can then read and replace the plan in Plan and Build. Saving cannot approve a plan or switch modes. Secure plan storage currently requires a Unix client, including for remote workspaces.

Implement and Clear-and-Implement capture validated plan content before they switch to Build or clear the session. The content is included in the model-visible Build request, so a scheduler does not need file access to receive it. If capture fails, the plan remains available and implementation does not start.

## Configure subagents

A profile can select a model and thinking setting for subagents:

```markdown
---
description: Deep security analysis
layout: overlay
subagent_model: anthropic/claude-opus-4-6
subagent_thinking: high
---

Prioritize exploitable findings and concrete fixes.
```

`subagent_model` accepts an exact qualified `provider/model` name or the same-as target `chat`, `plan`, `fast`, or `best`. A named target follows that job's current binding and default. A profile value overrides the global Subagent binding. If the profile omits it, the global binding applies. When both are unbound, the subagent inherits the model currently running its parent.

`subagent_thinking` accepts `off`, `adaptive`, an effort level, or a positive token budget. It inherits the parent's thinking setting when omitted. These fields do not change the main agent. See [Model jobs](/docs/providers/#model-jobs) for routing rules.

Caudra validates each profile against the effective subagent model. An explicit thinking setting must be supported exactly. Caudra does not snap effort levels, clamp budgets, or translate between effort and budget modes for a profile. An incompatible profile remains available to the main agent, but Caudra warns and removes it from the task profile list.

The `task` tool accepts `profile` and `mode`. A new task inherits the parent profile when `profile` is omitted. Set `profile` to `builtin` to use Caudra's built-in task prompt.

`mode` defaults to the caller's own mode and can never exceed it, so a task launched from build mode can build, and one launched from plan mode stays read-only. `plan` has a host-enforced read-only tool set with no file writes. Eligible shell calls must pass the host's confined read-only checks. A task that needs mutating commands requires `build`. A `build` request from a plan-mode caller runs as `plan` instead. Every result reports the mode the task ran as. A task in either mode can read the session plan but cannot replace it. See [Read-only agents](/docs/permissions/#read-only-agents).

Task profiles support overlay and custom layouts. For a custom task prompt, directives resolve to the matching research or general task component. `{{caudra.default}}` expands to the complete built-in task prompt. Caudra appends the system-reminder contract after the rendered profile, so custom layouts cannot remove it.

The effective profile and mode are stored with task history. A continuation uses the stored values when they are omitted and rejects conflicting values. Legacy task histories bind both values on their first successful continuation.

The task API no longer accepts `subagent_type`, `model`, or `model_tier`. Replace `subagent_type = "research"` with `mode = "plan"` and `subagent_type = "general"` with `mode = "build"`. Move model selection into profile frontmatter and remove `plugins.task.allow_model` from your `caudra.toml` or `init.lua`.

## Select a profile

Set the default in `caudra.toml`:

```toml
[agent]
system_prompt_profile = "review"
```

Use `builtin` to clear a default inherited from another config file.

Override the default for one invocation:

```bash
caudra --system-prompt-profile review
caudra --system-prompt-profile review --print "Review this change"
caudra --system-prompt-profile review prompt system
caudra --system-prompt-profile researcher tools
caudra --system-prompt-profile scheduler prompt --plan --tools
```

`caudra tools` reports the initial eager, lazy, and disabled set. `caudra tools --schemas` and `caudra prompt --tools` show the initial request schemas, including one combined pending catalog when needed. They do not restore a session's previously loaded schemas. Inside a session, `/tools` provides a [mode-aware inventory](/docs/context/#inspect-the-active-window).

Inside the TUI, run `/system-prompt` to read the prompt the current session is sending. The modal shows the text the agent bound, so it matches what the provider received rather than a fresh assembly of it. Press `r` to swap between rendered markdown and the source, `y` to copy, and `p` to open the profile picker.

Drag the pointer to select a passage, and releasing the mouse button copies it without the line numbers. Press `Ctrl+A` to select everything. With a selection standing, `y` and `Ctrl+C` copy it the same way. With nothing selected, `y` copies the whole prompt source and `Ctrl+C` closes the modal.

Line numbers count source lines in both views. A rendered row is numbered by the line it draws from, so a heading row and the code inside a fence point at the text you would find at that line in the profile file. A line too long for the modal is folded across several rows and numbered once, at its head.

Switching a profile from that picker stores the selected name with the session. Profile content stays in the config directory, so edits apply when the session is resumed or Caudra is reloaded.

A selected profile that is missing or invalid is rejected across the TUI, print/headless, SDK, and ACP. Caudra does not fall back to unrestricted built-in behavior. Select `builtin` explicitly to reset it.

An explicit CLI profile takes precedence over the stored session profile and the configured default. It applies only to that invocation, so the picker cannot switch profiles until the next invocation.

`--system-prompt-profile` and the raw SDK `--system-prompt` override cannot be used together. A raw SDK override continues to replace normal prompt assembly.

Profiles affect the main TUI, print, SDK, ACP, prompt inspection, `/btw`, and task prompts. Compaction and goal evaluation prompts remain host-controlled.
