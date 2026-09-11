+++
title = "System Prompt Profiles"
weight = 45
[extra]
group = "Guides"
+++

# System Prompt Profiles

System prompt profiles change main and task prompts without copying Caudra's built-in prompts. Overlay profiles preserve current tool guidance, environment details, instruction files, plugin hints, and mode text.

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

The system prompt is identical in plan and build mode, and carries no working directory, date, or model. All of those are announced in the conversation instead, and re-announced only when they change, so switching mode, crossing midnight, or changing model does not invalidate the prompt cache. Editing an instruction file mid-session is announced the same way, as a diff against the copy the prompt already carries.

Those announcements arrive wrapped in `<system-reminder>`. They are appended to the conversation and never edited, so a kind is restated only when its content changes and earlier blocks of the same kind remain as history. The most recent block of a kind is the only one in force; the system prompt tells the model this, and that a reminder is not the user talking.

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
| `{{caudra.context}}` | Instruction files and plugin runtime context |
| `{{caudra.plan}}` | The system-reminder contract, and how plan and build mode work. Identical in both modes |

A directive expands only when it occupies a complete line. Prefix it with `\` to keep it literal, for example `\{{caudra.tools}}`.

Each component directive may appear once. `{{caudra.default}}` cannot be combined with another component directive. Custom layouts may omit components, though omitting `tools`, `context`, or `plan` can remove information the agent relies on.

Profile files must be valid UTF-8 and no larger than 64 KiB. Unknown frontmatter fields and unknown directives make the profile invalid.

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

The `task` tool accepts `profile` and `mode`. A new task inherits the parent profile when `profile` is omitted. Set `profile` to `builtin` to use Caudra's built-in task prompt. `mode` defaults to `plan`, which has a host-enforced read-only tool set. `build` enables implementation tools.

Task profiles support overlay and custom layouts. For a custom task prompt, directives resolve to the matching research or general task component. `{{caudra.default}}` expands to the complete built-in task prompt. Caudra appends the plan or build contract after the rendered profile, so custom layouts cannot remove it.

The effective profile and mode are stored with task history. A continuation uses the stored values when they are omitted and rejects conflicting values. Legacy task histories bind both values on their first successful continuation.

The task API no longer accepts `subagent_type`, `model`, or `model_tier`. Replace `subagent_type = "research"` with `mode = "plan"` and `subagent_type = "general"` with `mode = "build"`. Move model selection into profile frontmatter and remove `plugins.task.allow_model` from `init.lua`.

## Select a profile

Set the default in `init.lua`:

```lua
caudra.setup({
  agent = {
    system_prompt_profile = "review",
  },
})
```

Use `builtin` to clear a default inherited from another config file.

Override the default for one invocation:

```bash
caudra --system-prompt-profile review
caudra --system-prompt-profile review --print "Review this change"
caudra --system-prompt-profile review prompt system
```

Inside the TUI, run `/system-prompt` to switch the current session. The selected name is stored with the session. Profile content stays in the config directory, so edits apply when the session is resumed or Caudra is reloaded.

An explicit CLI profile takes precedence over the stored session profile and the configured default. It applies only to that invocation, so `/system-prompt` cannot switch profiles until the next invocation.

`--system-prompt-profile` and the raw SDK `--system-prompt` override cannot be used together. A raw SDK override continues to replace normal prompt assembly.

Profiles affect the main TUI, print, SDK, ACP, prompt inspection, `/btw`, and task prompts. Compaction and goal evaluation prompts remain host-controlled.
