+++
title = "System Prompt Profiles"
weight = 45
+++

# System Prompt Profiles

System prompt profiles change the main agent prompt without copying Maki's built-in prompt. Overlay profiles preserve current tool guidance, environment details, instruction files, plugin hints, and plan mode text.

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

Profiles affect the main TUI, print, SDK, ACP, prompt inspection, and `/btw` prompts. Research and general subagent prompts, compaction prompts, and goal evaluation prompts remain host-controlled.
