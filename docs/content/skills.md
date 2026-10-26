---
title: "Skills"
description: "Write Markdown playbooks the agent loads on demand."
---

A skill is a short Markdown how-to that the agent loads only when it needs it. The `skill` tool shows the agent what is available, and when it picks one, the file drops into the conversation and the agent follows it.

Write one for anything you keep explaining: how you cut a release, how you write a caudra plugin, how a PR should look in this repo. `AGENTS.md` is always in context and always costs tokens. A skill costs nothing until it is loaded, only its name and description sit in the tool list. So big skills are fine.

## Where skills live

A skill is a directory with a `SKILL.md` inside. Caudra looks for them every time the `skill` tool runs. It builds the list the model reads at startup and again when a skill directory changes. The model sees a changed list as a reminder until its tools are next rebuilt, see [Add a skill](/docs/configuration/#add-a-skill).

Personal skills come from one directory. Caudra takes the first of these that exists and reads nothing below it:

1. `~/.config/caudra/skills/` (Windows: `%APPDATA%\caudra\skills\`)
2. `~/.claude/skills/`
3. `~/.config/opencode/skills/`
4. `~/.agents/skills/`

Project skills work the same way at each level of the walk from your current directory up to the `.git` root. At every level Caudra takes the first of `.caudra/skills/`, `.claude/skills/`, `.opencode/skills/`, `.agents/skills/` that exists, and skips the others.

The `.claude`, `.opencode` and `.agents` directories are there so skills you already wrote for other agents keep working. Once you make a `.caudra/skills/` next to them, they stop being read. An empty `~/.config/caudra/skills/` counts as existing, so it switches the compatibility directories off. Delete it if you want them back.

Levels still combine: a skill at the repo root and a skill in a subdirectory both load. When two skills share a name, the one found last wins, so project skills beat personal ones and the repo root beats a nested directory. The builtins `caudra-docs`, `caudra-workflow-dev`, `caudra-automation-dev` and `caudra-plugin-dev` sit below all of them and any file of the same name replaces one.

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

Caudra ships four skills. Each is a normal entry in the `skill` tool's list, and a `SKILL.md` of the same name in any of your directories replaces it. `caudra-workflow-dev`, `caudra-automation-dev` and `caudra-plugin-dev` also need their [experimental feature](/docs/configuration/#experimental-features). Your own Markdown skills need no experimental switch.

### caudra-docs

It is on by default. It holds this manual as it was when your binary was built, so the agent answers questions about Caudra from the docs of the version you run. Loading it returns an index of about 2.5k tokens: every page with its size, its one-line description and its sections. The agent then loads only the part it needs:

| Name | Returns |
|---|---|
| `caudra-docs` | the index |
| `caudra-docs/permissions` | one whole page |
| `caudra-docs/permissions#plan-mode` | one section, up to the next heading of the same or a higher level |
| `caudra-docs?shell timeout` | the 8 best sections that contain every word, each with its address and a snippet |

An address follows the site URL: a link to `/docs/permissions/#plan-mode` is `caudra-docs/permissions#plan-mode`. A search ignores case and matches inside words, so `config` finds `configuration`. A misspelt word of five or more letters also matches the closest words in the docs, and the result names the correction.

Until the agent loads it, the skill costs one line in the `skill` tool's list. `/context` and `/skills` count every page and search it loads under `caudra-docs`. Nothing is written to disk, so the skill works the same in remote workspaces, headless runs and subagents. [`/docs`](/docs/commands/#docs) shows you the same pages in the TUI.

### caudra-workflow-dev

It needs `experimental.workflows`. With workflows on, it is on by default. It is the complete authoring guide for [workflows](/docs/workflows/): where a script goes and which scope to choose, the `meta` header rules, every host function with its result shape and failure modes, the parts of Rhai that trip people up, how replay and resume constrain a script, prompt patterns for untrusted agent output, three complete worked scripts, and a table of common errors with their fixes. With it loaded, "write me a workflow that reviews a branch with three readers and verifies their findings" produces a file the agent can validate and start in the same session. The examples in the guide are compiled and smoke-run by Caudra's own test suite, so they cannot drift from the engine.

The agent writes to the project directory when the plan belongs to the repository and to your user directory when it is personal. A project script still needs your approval in `/workflows` before it can start. The agent cannot grant that.

### caudra-automation-dev

It needs `experimental.automations`. With automations on, it is on by default. It is the complete authoring guide for [automations](/docs/automations/#write-one): where a script goes and which scope to choose, the header and its triggers, every event field and host function, how untrusted text and state behave, the limits and how to end a loop, the parts of Rhai that trip people up, complete worked scripts, and a table of common mistakes with their fixes. The worked scripts are the examples Caudra's own test suite replays against scripted events, so they cannot drift from the engine.

The agent validates the script with [`automation.validate()`](/docs/code/#tools-reached-from-code) and ends its turn with the line that arms it. A project script still needs your trust in `/automations`, and only you can arm a script.

### caudra-plugin-dev

It needs `experimental.lua_plugins`. With Lua on, it is still off by default. It teaches the agent how to write caudra Lua plugins, and on load it writes the full Lua API reference to a file in the state dir, so the agent can read it in pieces instead of swallowing it whole. It carries the same guide you can read in [Plugins](/docs/plugins/), so "write me a plugin that ..." is usually enough.

### pstack

It is off by default. [pstack](https://github.com/cursor/plugins/tree/main/pstack) is Lauren Tan's set of engineering skills for Cursor, bundled under its MIT license with the `deslop`, `control-cli` and `control-ui` skills from Cursor's team kit. It is opinionated about how work is planned, reviewed and shipped, so turn it on only if you want to work that way. `pstack = true` adds five things:

- The `pstack` skill, one line in the `skill` tool's list however many skills it holds.
- A `/pstack:<name>` command for each skill you run directly. `/pstack:poteto-mode fix the flaky test` tells the agent to load `pstack/poteto-mode` and follow it for that request. The `principle-*` skills are read from within other skills and have no command.
- The task profiles `pstack-poteto-agent` and `pstack-comment-sicko`, described in [Built-in task profiles](/docs/system-prompts/#built-in-task-profiles).
- pstack's scripts as [`pstack` calls in `code`](/docs/code/#pstack-scripts).
- The [automations](/docs/automations/#where-scripts-live) `pstack-watch-pr`, `benny-triage` and `benny-reproduce`, listed in `/automations` as `bundled` while `experimental.automations` is on.

The skill uses addresses the way `caudra-docs` does:

| Load | You get |
|---|---|
| `pstack` | the index: every skill with its summary, and the Model roles table |
| `pstack/poteto-mode` | one skill |
| `pstack/poteto-mode/playbooks/bug-fix.md` | a file inside a skill |
| `pstack?flaky test` | the 8 best pages that contain every word, ranked like a `caudra-docs` search, each with its address and a snippet |

Upstream wrote the skills for Cursor. Caudra ships them translated: the pages name Caudra's tools, paths, models and commands, so the agent follows them as written. Pages that need an automation say how to arm it and with which args, such as `pstack-watch-pr`, which watches a pull request and messages the session when its checks or reviews change. There is nothing to write or trust: you arm the bundled script in `/automations`. A user or project script of the same name replaces it.

#### Model roles

The pages never name a model. They name six kinds of work, and every page that starts tasks ends with a **Model roles** table that gives each kind's `model` and `thinking` for the `task` call:

| Kind | Used for | Unless you set it |
|---|---|---|
| Code | code-writing delegates, explorers, investigators | `inherit`: the profile's `subagent_model`, else the Subagent [model job](/docs/providers/#model-jobs) |
| Hardest | the hardest changes | your newest Best-lane favorite, else a provider flagship |
| Judgment | prose, judgment, explainers, synthesizers | your newest Best-lane favorite, else a provider flagship |
| Verify | verification lanes | `inherit` |
| Panel | a diverse panel, one task per seat, such as arena runners and interrogate reviewers | two Best-lane favorites on different providers |
| Cross-judge pool | the model that judges an arena, on a provider other than the session's when it can | the Panel's models |

The lane is the one `caudra models` and the `# Model favorites` reminder show. A provider flagship is the Best-lane model Caudra starts on for a provider you are signed in to, such as `claude-opus-5-5` for Anthropic and `gpt-6-astra` for OpenAI, never a pricier model beside it such as Fable. Caudra checks Anthropic, OpenAI, xAI, Copilot, Copilot Enterprise, Z.AI, Synthetic and DeepSeek, in that order, and Hardest and Judgment take the first flagship. Short of Best-lane favorites on two providers, the Panel fills its second seat from your other favorites, then from the flagships on another provider. With every candidate on one provider, the table repeats one model and says the panel is not diverse. With no favorites and none of those providers signed in, every kind is `inherit`. Each value gets the default effort, `xhigh` unless you change it, lowered to the highest level the model supports.

Open `/settings` and its **pstack** tab, the last one, shown while pstack is on, to choose a kind's model. `Enter` opens the model list with `inherit` and your favorites first, `t` changes a row's thinking level, and `R` returns the kind to automatic. On the Effort row, `Enter` steps the default effort and `R` restores `xhigh`. A Panel or Cross-judge pool has a line per seat, `+ add seat` adds one, and `d` removes one. The choices are saved globally with your model jobs and apply the next time a page loads, in every session, including `--print` and ACP. A saved model you can no longer use, because it is unstarred and Caudra cannot resolve it or `allowed_models` rules it out, is replaced by the automatic choice, and the table says so. With `agent.task_model_choice = false`, `task` has no `model` or `thinking`, so every kind runs as `inherit`.

### mattpocock

It is off by default. [mattpocock](https://github.com/mattpocock/skills) is Matt Pocock's set of small engineering disciplines, bundled under its MIT license. Caudra ships 16 of them, the ones that work in any repository. `mattpocock = true` adds two things:

- The `mattpocock` skill, one line in the `skill` tool's list. Its description names the work that makes the agent reach for it, such as test-first building or a hard bug.
- A `/mattpocock:<name>` command for every skill. `/mattpocock:grill-me the cache rewrite` tells the agent to load `mattpocock/grill-me` and follow it for that request.

| Who starts it | Skills |
|---|---|
| The agent, when the work calls for it, or you | `grilling`, `domain-modeling`, `codebase-design`, `tdd`, `diagnosing-bugs`, `code-review`, `pr`, `writing-for-agents` |
| Only you, with its command | `grill-me`, `grill-with-docs`, `improve-codebase-architecture`, `to-spec`, `to-tickets`, `to-questionnaire`, `handoff`, `retro` |

The index splits the skills the same way, and the agent loads a skill from the second row only when you send its command. Addresses work as they do for pstack: `mattpocock`, `mattpocock/tdd`, `mattpocock/tdd/mocking.md` and `mattpocock?seam`.

Upstream wrote the skills for Claude Code. Caudra translates them and adapts a few to how it works:

- Grilling asks each round through the `question` tool, with the recommended answer first and room to type your own. In a run without the `question` tool, the agent asks in numbered plain text instead.
- `tdd` adds a refactor step after each test turns green.
- `improve-codebase-architecture` writes its report as Markdown with Mermaid diagrams, and as HTML only when you ask.
- No skill commits or pushes on its own, and in Plan mode the documents a skill writes go to the plan.
- Upstream's setup skill is replaced by one page that finds your issue tracker: the project's `AGENTS.md`, then the git remote, then local Markdown files under `.scratch/`.

#### mattpocock and pstack

Both packs can be on at once, and each keeps its own commands, so `/pstack:tdd` and `/mattpocock:tdd` sit side by side. pstack is one opinionated way of working, with orchestration, playbooks and model roles. mattpocock is a set of disciplines you pick one at a time.

| Work | pstack | mattpocock |
|---|---|---|
| Tests | `tdd` writes tests first only when you ask, and may skip tests that are impractical | `tdd` agrees the seams up front and builds in vertical slices |
| Bugs | the bug-fix playbook, alongside Caudra's `/root-cause` | `diagnosing-bugs`: a feedback loop first, then minimise, hypothesise and instrument |
| Review | `interrogate` reviews adversarially across several models | `code-review` checks the change against standards and against the spec in parallel |
| Architecture | `architect` designs and builds alternatives, and `how` explains existing code | `improve-codebase-architecture` surveys where modules could be deeper |
| Interviews and specs | `in-your-own-words` restates what you asked for | grilling, domain modeling with a glossary and ADRs, specs, tickets and questionnaires |
| Handing over | `ketchup` summarises live state, beside Caudra's `/btw`, `/catchup` and `/fork` | `handoff` writes a document another session or person can pick up |
| Retrospective | `reflect` turns lessons into skill edits, beside Caudra's `/learn` | `retro` improves the agent's environment |

The `pr` skill's Summary section comes from [Dex Horthy](https://github.com/dexhorthy)'s `show-me` skill, credited in `mattpocock/pr/CREDITS.md`.

Each builtin has a switch under `plugins.skill`:

```toml
# ~/.config/caudra/caudra.toml
[plugins.skill]
automation_dev = true
docs = false
mattpocock = true
plugin_dev = true
pstack = true
workflow_dev = false
```

`automation_dev`, `plugin_dev` and `workflow_dev` have no effect while their experimental feature is off. `docs = false` removes the skill only, and `/docs` keeps working. `pstack = false` removes the skill, its commands, its profiles and its `code` calls, and `mattpocock = false` removes its skill and commands. While the terminal UI runs, switching `pstack` or `mattpocock` applies as soon as you save the file.
