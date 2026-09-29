+++
title = "Skills"
weight = 20
[extra]
group = "Guides"
+++

# Skills

A skill is a short Markdown how-to that the agent loads only when it needs it. The `skill` tool shows the agent what is available, and when it picks one, the file drops into the conversation and the agent follows it.

Write one for anything you keep explaining: how you cut a release, how you write a caudra plugin, how a PR should look in this repo. `AGENTS.md` is always in context and always costs tokens. A skill costs nothing until it is loaded, only its name and description sit in the tool list. So big skills are fine.

## Where skills live

A skill is a directory with a `SKILL.md` inside. Caudra looks for them every time the `skill` tool runs (and once at startup, to build the list).

Personal skills come from one directory. Caudra takes the first of these that exists and reads nothing below it:

1. `~/.config/caudra/skills/` (Windows: `%APPDATA%\caudra\skills\`)
2. `~/.claude/skills/`
3. `~/.config/opencode/skills/`
4. `~/.agents/skills/`

Project skills work the same way at each level of the walk from your current directory up to the `.git` root. At every level Caudra takes the first of `.caudra/skills/`, `.claude/skills/`, `.opencode/skills/`, `.agents/skills/` that exists, and skips the others.

The `.claude`, `.opencode` and `.agents` directories are there so skills you already wrote for other agents keep working. Once you make a `.caudra/skills/` next to them, they stop being read. An empty `~/.config/caudra/skills/` counts as existing, so it switches the compatibility directories off. Delete it if you want them back.

Levels still combine: a skill at the repo root and a skill in a subdirectory both load. When two skills share a name, the one found last wins, so project skills beat personal ones and the repo root beats a nested directory. The builtins `caudra-workflow-dev` and `caudra-plugin-dev` sit below all of them and any file of the same name replaces one.

Run `caudra skills --dirs` or `/skills` to see every candidate directory and which one won.

Only `SKILL.md` is read. If you want extra notes, put them in files next to it and link them from the body, like `./notes.md`.

## Writing one

Make a directory under `.caudra/skills/` and put a `SKILL.md` in it:

```
.caudra/skills/git-release/SKILL.md
```

```markdown
---
name: git-release
description: Cut a tagged release and open the changelog PR
---

## Steps

1. Read `CHANGELOG.md` and the commits since the last tag.
2. Propose a semver bump and a short release summary.
3. Only tag after the user confirms.
```

The frontmatter is optional. Without it, the directory name is the skill name and the whole file is the body. An empty body is skipped. The `description` is what the model reads when picking a skill, so make it specific.

## How it gets used

The `skill` tool lists every skill it found, the agent calls it with a name and gets the body back. A wrong name errors and reprints the list so the model can pick again.

The model receives the body with each line numbered, so it can cite a line and read on from it. The card in your transcript shows the file the skill came from, then the body as rendered Markdown. An open card shows the first `ui.tool_output_lines.other` rows of it, and a click shows the rest.

Skills are not slash commands: typing `/git-release` does nothing unless you also add a [custom command](/docs/commands/#custom-commands). Ask the agent to use a skill, or let it pick one on its own.

## Seeing what the agent has

`/skills` opens a report with every skill, where its file lives, whether its body is already in the window, and every candidate directory with the state precedence gave it. It reads the disk when it opens, so it works before the first request.

The same report is on the command line:

```
caudra skills                 # every skill with its scope and file
caudra skills git-release     # the body, exactly as the model receives it
caudra skills --names         # names, one per line
caudra skills --json          # full records
caudra skills --dirs          # candidate directories: selected, superseded, or missing
```

## The builtins

Caudra ships two skills. Each is a normal entry in the `skill` tool's list, and a `SKILL.md` of the same name in any of your directories replaces it. Each builtin also needs its [experimental feature](/docs/configuration/#experimental-features). Your own Markdown skills need no experimental switch.

### caudra-workflow-dev

It needs `experimental.workflows`. With workflows on, it is on by default. It is the complete authoring guide for [workflows](/docs/workflows/): where a script goes and which scope to choose, the `meta` header rules, every host function with its result shape and failure modes, the parts of Rhai that trip people up, how replay and resume constrain a script, prompt patterns for untrusted agent output, three complete worked scripts, and a table of common errors with their fixes. With it loaded, "write me a workflow that reviews a branch with three readers and verifies their findings" produces a file the agent can validate and start in the same session. The examples in the guide are compiled and smoke-run by Caudra's own test suite, so they cannot drift from the engine.

The agent writes to the project directory when the plan belongs to the repository and to your user directory when it is personal. A project script still needs your approval in `/workflows` before it can start. The agent cannot grant that.

### caudra-plugin-dev

It needs `experimental.lua_plugins`. With Lua on, it is still off by default. It teaches the agent how to write caudra Lua plugins, and on load it writes the full Lua API reference to a file in the state dir, so the agent can read it in pieces instead of swallowing it whole. It carries the same guide you can read in [Plugins](/docs/plugins/), so "write me a plugin that ..." is usually enough.

Both are switches under `plugins.skill`:

```toml
# ~/.config/caudra/caudra.toml
[plugins.skill]
plugin_dev = true
workflow_dev = false
```

A switch has no effect while its experimental feature is off.
