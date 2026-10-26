# pstack: from Cursor to Caudra

pstack (cursor/plugins) is written for Cursor. The crate is `caudra-pstack`, synced with `make sync-pstack ARGS='--source <cursor/plugins checkout>'`. Its overlay keeps upstream's roots: `pstack/...` and `cursor-team-kit/...`, so a path to `--mark` reads `pstack/skills/how/SKILL.md`. The term list is `CURSOR_TERMS` in `caudra-pstack/src/library.rs`.

`caudra-pstack/automations/*.rhai` are Caudra's own ports of pstack's automations, served as `pstack/caudra/automations/<file>` and bundled as `/automations` entries while pstack is on, so users arm them without writing a file.

## Tools

| Cursor | Caudra |
| --- | --- |
| `Task`, a Task subagent | the `task` tool, a task |
| `AskQuestion` | the `question` tool. A headless run has none, so the step says to ask in plain text then. |
| a todolist | `todo_write` |
| `Shell`, `Read`, `Grep`, `Glob` | `shell`, `file_read`, `file_grep`, `file_glob` |
| Run another pstack skill: `/name`, "invoke `/name`", "the **name** skill" | Load `pstack/<name>` with the `skill` tool and follow it. When the text tells the user to type it, write `/pstack:<name>`. Keep "the **name** skill" as a name. |
| `create-skill` (Cursor's skill for writing skills) | load `caudra-docs/skills` with the `skill` tool |
| the image-generation tool | `image.generate` in `code` |
| Cursor's agent dashboard | `/tasks` |

## Subagent fields

| Cursor | Caudra `task` |
| --- | --- |
| `subagent_type: generalPurpose` | a plain `task`: no `profile` |
| `subagent_type: "poteto-agent"` | `profile: "pstack-poteto-agent"` |
| `subagent_type: "comment-sicko"` | `profile: "pstack-comment-sicko"` |
| `readonly: true`, Ask mode | `mode: "plan"`. It cannot edit or run mutating commands, and it loses most MCP tools. |
| `readonly: false`, agent mode | `mode: "build"` |
| `run_in_background: true`, `is_background: true` | `background: true` |
| `resume` | `task_id` of a settled task. A running task cannot be resumed: start a fresh one with the consolidated brief. |
| `environment: "cloud"` | a task that works in its own git worktree: the brief tells it to `git worktree add` a new branch under the scratch directory, off the base the step names, and to work only there |
| `environment: "local"` | drop it |
| `cloud_base_branch: <b>` | the brief names `<b>` as the worktree's base |
| `file_attachments` | list the paths in the brief |
| a cloud agent's URL or machine | drop it |

A completion notification is the background task's final report, which arrives in the conversation by itself. Checking on a running task is one `task_control` `status` call, never a polling loop.

## Models

Never name a model on a page. Caudra appends a **Model roles** table to every page that contains the exact phrase `the **Model roles** table` (bold included), computed when the page is served from the user's `/settings` → pstack choices, else from their favorite models, model jobs and signed-in providers' flagships, so it only lists models they can run. Each row gives ready `task` arguments: `model` and `thinking`, or `inherit`. A page that tells the agent which model to use must contain that phrase, as in "the Code row of the **Model roles** table below", and names rows by their kind exactly.

| Upstream role, default, or model slug | Kind |
| --- | --- |
| feature, refactoring, bug-fix, perf-issue, hillclimb; how explorer; why investigators; reflect tooling; any code-writing delegate | the Code row |
| hardest tasks | the Hardest row |
| judgment and prose; how explainer; why synthesizer; reflect judgment, divergent, synthesizer | the Judgment row |
| swarm workers | the Verify row |
| arena runners, architect runners, interrogate reviewers | the Panel rows, one task per seat; the seat count sets the panel size |
| arena cross-judge pool | the Cross-judge pool row, picking a model on a provider other than the parent's when possible |

- A model slug or a default (`<model> <effort>`, a panel list of them) becomes its role's row. Drop the slug and the default.
- `auto` and `inherit-parent` become `inherit`: omit `model` and `thinking`, so the task runs on the profile's `subagent_model`, else the user's Subagent model.
- When `task` has no `model` or `thinking` argument, omit both and say so: every row acts as `inherit`.
- A value `task` rejects: run that seat as `inherit` and say so in the reply. Drop upstream's fallbacks to per-skill defaults and its family-prefix matching.
- "A model family different from the parent's", for the cross-judge: a provider other than the parent's.
- Reasoning levels start from the row's `thinking`. A page changes only `thinking` and keeps the row's model, so a diverse panel stays diverse.
- `~/.cursor/rules/pstack-models.mdc`, "the `pstack-models.mdc` rule", reasoning budgets, and `setup-pstack` (retired): `/settings` → pstack, as in "Change these in `/settings` → pstack." setup-pstack's offers of a verification skill and a formal verification skill are suggestions in `docs/guide/01-setup.md`.

## Scripts

pstack's scripts are built into Caudra as `pstack` calls in the `code` tool. Each takes the upstream command line's arguments as strings and returns `{"code", "stdout", "stderr"}`; a nonzero `code` is the script's verdict, not an exception. `log`, the `orch` commands that change the store, and `statechart`'s `from-spec` and `from-chart` write files, so plan mode refuses them; `orch`'s help and read-only commands, and `statechart`'s `compare` and help, run there. Commands a script runs get the shell tool's environment, so `gh` needs its token or keyring variables forwarded with `/env` or `[agent.shell_env] pass`. In a remote session the calls raise, and the step runs with `shell` instead.

| Upstream | Caudra |
| --- | --- |
| `scripts/log.sh <logfile> <phase> <decision> <why> <evidence> <result>` | `await pstack.log(logfile, phase, decision, why, evidence, result)` |
| `node .../check-plan.mjs <plan.md>`, `bun ...` | `await pstack.check_plan("plan.md")`, or `await pstack.check_plan("--plan")` for the session plan (see Plan mode) |
| `scripts/worktree-audit.sh [-b <ref>] [repo]` | `await pstack.worktree_audit("-b", "origin/main")`. `LAST_CHAT` is the last Caudra session that worked in the worktree. |
| `scripts/watch-pr/watch-pr <args>` | `await pstack.watch_pr("--pr", "123", "--status-only")`, same flags. A polling run streams its lines to the call's row and ends at the run's `timeoutSec`, at most 3600: raise `timeoutSec` for drive mode, or arm the `pstack-watch-pr` automation for a long wait. |
| `orch <command> <args>` | `await pstack.orch("--store", store, "unit", "add", ...)`. Every call passes `--store`, `init` included. |
| `node .../statechart.mjs <command> <args>` | `await pstack.statechart("from-spec", "Pay.tla", ...)`, same commands and flags, plus `compare` for `compare-charts.mjs`. It checks with tla-checker, so TLC, Java, and `TLA2TOOLS` go. A mapping is Rhai, `SPEC.map.rhai`, with snake_case names (`reader_actions`); `--png` is gone, and a chart is shown in a ```` ```mermaid ```` block. `from-spec` and `from-chart` write, so plan mode refuses them. |
| installing bun, node, Java, TLC, or a script's dependencies | drop the step |

## Paths and state

| Cursor | Caudra |
| --- | --- |
| `.cursor/skills/`, `~/.cursor/skills/` | `.caudra/skills/`, `~/.config/caudra/skills/` |
| `~/.cursor/plugins/`, `.cursor-plugin/plugin.json`, "where pstack is installed" | pstack is built in; its files are pages of the `pstack` skill |
| `.cursor/rules/*.mdc`, an always-applied rule | `AGENTS.md` |
| transcripts in `~/.cursor/projects/<slug>/agent-transcripts/` | `history.search` and `history.read` in `code`: `scope="session"` for this chat, `scope="project"` for this project's chats |
| "the current agent's store" (orchestrate) | `.caudra/pstack/orchestrate/<project-slug>/` in the main checkout, with that folder in `.git/info/exclude` |
| `.cursor/automations/benny/...` paths | the pages `pstack/automations/benny/...` for the pack's skills and prompts; `.caudra/benny/` for the user's configuration, routing map, and feature map |
| links to `cursor.com/docs/...` | the matching `https://caudra.ai/docs/...` page, or drop the link |
| a hard-coded `/tmp/...` | the scratch directory, which Caudra gives each project and writes to without asking: `$TMPDIR/...` in shell commands and env assignments, `${process.env.TMPDIR}` in JavaScript run through `shell`, and in prose "the scratch directory (the `Scratch directory` path in the Environment block, `$TMPDIR` in shell commands)" on first mention. A config value nothing expands takes the placeholder `<scratch directory>/...` |

A page's own links stay relative: a page is served at `pstack/<path>`, and a relative link resolves against its directory.

## Cursor-only features

Map first. Remove a step only when Caudra has nothing like it, and then remove it cleanly, with no "skipped" note. If a later step needs what the removed one made, rewrite that step too.

| Cursor | Caudra |
| --- | --- |
| `/loop until X`, the loop skill | `/goal X`, which the user sets (autopilot answers for them). Within a turn, keep working until X holds. |
| Cursor Automations, routine wakes, `update_state`, `api2.cursor.sh/automations` webhooks | Caudra automations (`experimental.automations`) with a `schedule` or `message_received` trigger. A script from `pstack/caudra/automations/` is bundled: with pstack on it is already listed in `/automations`, so the page gives the arm line and args and never a write, validate, or trust step, and says a project or user script of the same name overrides it. Any other script is written with the `caudra-automation-dev` skill: the agent writes and validates it; the user trusts and arms it. |
| `SendToUser` secret requests | the user sets the variable in their environment or the global `.env` |
| a frontier watcher or heartbeat armed through the loop skill | an automation with a `schedule` trigger that messages the session, such as `pstack-watch-pr`; without automations, drain when a report arrives |
| agents in other chats reporting to an orchestrator | cross-session messaging (`send_message`, `list_sessions`) when the user has it on; otherwise every worker is a `task` of this session |
| Cursor cloud agents | worktree tasks, as in Subagent fields |
| a Cursor restart | a Caudra restart; running tasks end with it, so resume their work from their reports and branches |
| a built-in PR tool | the forge command the step gives, such as `gh pr create` |
| Bugbot | unchanged: it is a GitHub app, and its comments are ordinary review threads |

Cursor's product name: where the text means the agent host, write Caudra. Where it credits where pstack comes from (its README, the license), keep it.

## Plan mode

Caudra opens sessions in Plan mode, where only the session plan changes. A step that writes a plan says: in Plan mode, write it with the `plan` tool (Caudra's session plan; Ctrl+O opens it, and autopilot can review it) and check it with `await pstack.check_plan("--plan")`; in Build mode, write the file under `.caudra/pstack/docs/` for hand-off to another session and check it by path. A playbook that builds, edits, or commits needs Build mode: the user switches with Tab, and a task's mode is capped by its caller's, so `mode: "build"` cannot lift a task out of Plan mode.

## Benny

benny's pack is not copied into the project. Its skills stay pages, such as `pstack/automations/benny/skills/triage-issue-reports/SKILL.md`. Its Slack trigger is the `benny-triage` and `benny-reproduce` automations (`pstack/caudra/automations/benny-triage.rhai`, `pstack/caudra/automations/benny-reproduce.rhai`), bundled with pstack and already listed in `/automations` with pstack and `experimental.automations` on, so setup writes, validates, and trusts nothing (a project or user script of the same name overrides one), polling Slack with `BENNY_SLACK_BOT_TOKEN` and armed with `/automations arm benny-triage {"channel_id": "C…", "triage_user_id": "U…"}` and `/automations arm benny-reproduce {"channel_id": "C…", "triage_user_id": "U…"}`. Slack actions (`read_action`, `thread_post_action`, reactions, file downloads) use a Slack MCP server's tools when one is enabled, else the Slack Web API with `curl` and the same token. Every rule about posting only in the report's thread stands.
