+++
title = "System Prompt Profiles"
weight = 45
+++

# System Prompt Profiles

System prompt profiles change main and task prompts without copying Maki's built-in prompts. Overlay profiles preserve current tool guidance, environment details, instruction files, plugin hints, and mode text.

Profiles are Markdown files in the user config directory:

```text
~/.config/maki/system-prompts/review.md
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

`layout: overlay` is the default, so the frontmatter is optional. Maki inserts the profile after runtime context and before the plan mode reminder.

## Control the layout

Set `layout: custom` to compose the full main-agent prompt from dynamic Maki components:

```markdown
---
description: Security-focused reviewer
layout: custom
---

{{maki.identity}}

# Role

Act as a security-focused reviewer. Report findings before summaries.

{{maki.context}}
{{maki.tools}}
{{maki.conventions}}
{{maki.completion}}
{{maki.plan}}
```

| Directive | Content |
| --- | --- |
| `{{maki.default}}` | The complete current built-in prompt |
| `{{maki.identity}}` | Resolved built-in or plugin identity |
| `{{maki.style}}` | Tone and professional objectivity |
| `{{maki.tools}}` | Tool rules, plugin hints, and efficient tools |
| `{{maki.conventions}}` | Git, security, and plugin conventions |
| `{{maki.completion}}` | Completion requirements |
| `{{maki.context}}` | Environment, model, instruction files, and plugin runtime context |
| `{{maki.plan}}` | The plan mode reminder when plan mode is active |

A directive expands only when it occupies a complete line. Prefix it with `\` to keep it literal, for example `\{{maki.tools}}`.

Each component directive may appear once. `{{maki.default}}` cannot be combined with another component directive. Custom layouts may omit components, though omitting `tools`, `context`, or `plan` can remove information the agent relies on.

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

`subagent_model` uses a qualified `provider/model` name. `subagent_thinking` accepts `off`, `adaptive`, an effort level, or a positive token budget. Omitted fields inherit the parent model and thinking setting. These fields do not change the main agent.

Maki validates each profile against the effective subagent model. An explicit thinking setting must be supported exactly. Maki does not snap effort levels, clamp budgets, or translate between effort and budget modes for a profile. An incompatible profile remains available to the main agent, but Maki warns and removes it from the task profile list.

The `task` tool accepts `profile` and `mode`. A new task inherits the parent profile when `profile` is omitted. Set `profile` to `builtin` to use Maki's built-in task prompt. `mode` defaults to `plan`, which has a host-enforced read-only tool set. `build` enables implementation tools.

Task profiles support overlay and custom layouts. For a custom task prompt, directives resolve to the matching research or general task component. `{{maki.default}}` expands to the complete built-in task prompt. Maki appends the plan or build contract after the rendered profile, so custom layouts cannot remove it.

The effective profile and mode are stored with task history. A continuation uses the stored values when they are omitted and rejects conflicting values. Legacy task histories bind both values on their first successful continuation.

The task API no longer accepts `subagent_type`, `model`, or `model_tier`. Replace `subagent_type = "research"` with `mode = "plan"` and `subagent_type = "general"` with `mode = "build"`. Move model selection into profile frontmatter and remove `plugins.task.allow_model` from `init.lua`.

## Select a profile

Set the default in `init.lua`:

```lua
maki.setup({
  agent = {
    system_prompt_profile = "review",
  },
})
```

Use `builtin` to clear a default inherited from another config file.

Override the default for one invocation:

```bash
maki --system-prompt-profile review
maki --system-prompt-profile review --print "Review this change"
maki --system-prompt-profile review prompt system
```

Inside the TUI, run `/system-prompt` to switch the current session. The selected name is stored with the session. Profile content stays in the config directory, so edits apply when the session is resumed or Maki is reloaded.

An explicit CLI profile takes precedence over the stored session profile and the configured default. It applies only to that invocation, so `/system-prompt` cannot switch profiles until the next invocation.

`--system-prompt-profile` and the raw SDK `--system-prompt` override cannot be used together. A raw SDK override continues to replace normal prompt assembly.

Profiles affect the main TUI, print, SDK, ACP, prompt inspection, `/btw`, and task prompts. Compaction and goal evaluation prompts remain host-controlled.
