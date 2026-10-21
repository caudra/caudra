---
name: pstack-translate
description: Translate the vendored pstack skills from Cursor's vocabulary into Caudra's, so agents follow the pages Caudra serves as written. Use for the pstack-sync run, after `make sync-pstack` reports TRANSLATE, NEW, or ORPHAN, and for any edit under caudra-pstack/translated/.
---

# Translating pstack into Caudra

pstack (cursor/plugins) is written for Cursor. Caudra bundles it as the `pstack` skill and serves every file as a page. The agent that reads a page must be able to follow it as written, with no mapping table. So every Cursor tool, field, path, model slug, command, and feature is rewritten here, once, by us.

## Layout

- `caudra-pstack/upstream/` is the verbatim copy `make sync-pstack` writes. Never edit it by hand. It is the base for the next translation and the license record.
- `caudra-pstack/translated/` has the same layout (`pstack/...`, `cursor-team-kit/...`). A file there replaces its upstream page. Copy a file here only when it needs a change.
- `caudra-pstack/translated/translations.json` records, for each translated file, the hash of the upstream file it was translated from. `python3 scripts/sync-pstack.py --mark <path>...` writes it, with paths as in the overlay, such as `pstack/skills/how/SKILL.md`.
- `caudra-pstack/automations/*.rhai` are Caudra's own ports of pstack's automations, served as `pstack/caudra/automations/<file>`.

## The check

`cargo test -p caudra-pstack --lib -- library::` fails when:

- a served page still names a Cursor term (the list is `CURSOR_TERMS` in `caudra-pstack/src/library.rs`; its allowlist names each exception and why),
- a translation's recorded upstream hash differs from the upstream file (the upstream file changed: retranslate),
- a translation has no upstream file (upstream deleted or moved it: delete or move the translation),
- a relative link leads nowhere.

## How to translate a file

1. Copy `caudra-pstack/upstream/<path>` to `caudra-pstack/translated/<path>`.
2. Rewrite only what the tables below cover, plus the sentences whose reasoning depends on Cursor. Keep upstream's structure, headings, voice, and every rule that still holds. Do not reword text that is already right, and do not add commentary about the translation.
3. Read the result as a Caudra agent would: every tool, field, value, path, and command must exist in Caudra and be spelled as its schema spells it.
4. Run `python3 scripts/sync-pstack.py --mark <path>` and the check.

## How to retranslate after a sync

`make sync-pstack` prints one line per file that needs work:

- `TRANSLATE <path>`: upstream changed a translated file. Diff the old and new upstream text (the command is printed with it, `git diff HEAD -- caudra-pstack/upstream/<path>` before you commit), apply the same change to the translation, translating any new Cursor text, then `--mark` it.
- `NEW <path>`: upstream added or changed a file that has no translation. Translate it when the check names it.
- `ORPHAN <path>`: the translation's upstream file is gone. Delete the translation, or move it with the upstream file and retranslate.
- `PORT <script>`: a ported script changed. Update its Rust port in `caudra-pstack/src/scripts/` to match, with tests.

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

- A model slug with an effort, `claude-opus-5-5-xhigh`, `grok-4.7-xhigh-fast`, `gpt-6.1-sol-high`: write `model: "<model>"` and `thinking: "<effort>"` on the `task` call, as `claude-opus-5-5` with `xhigh`. Drop `-fast`.
- In tables of defaults, and in the role file, a role's value is `<model> <thinking>`, such as `claude-opus-5-5 xhigh` or `grok-4.7 xhigh`, or a comma-separated list of those for a panel.
- `auto` and `inherit-parent` become `inherit`: omit `model` and `thinking`, so the task runs on the session's Subagent model.
- A model `task` rejects: omit `model`, keep `thinking`, and say so in the reply.
- `~/.cursor/rules/pstack-models.mdc`, "the `pstack-models.mdc` rule": the role file `~/.config/caudra/pstack/models.md`. It has no frontmatter: comment lines starting `#`, a `# budget: <label> (<thinking>)` line, and one `<role>: <value>` line per role. The `pstack` index shows the lines in force, so a skill reads them there. A missing file or role line means the skill's default.
- Detecting models: the models `task` accepts are the session's model favorites and the models `caudra models` lists for providers the user is logged in to.

## Scripts

pstack's scripts are built into Caudra as `pstack` calls in the `code` tool. Each takes the upstream command line's arguments as strings and returns `{"code", "stdout", "stderr"}`; a nonzero `code` is the script's verdict, not an exception. `log` and `orch` write files, so plan mode refuses them. In a remote session the calls raise, and the step runs with `shell` instead.

| Upstream | Caudra |
| --- | --- |
| `scripts/log.sh <logfile> <phase> <decision> <why> <evidence> <result>` | `await pstack.log(logfile, phase, decision, why, evidence, result)` |
| `node .../check-plan.mjs <plan.md>`, `bun ...` | `await pstack.check_plan("plan.md")` |
| `scripts/worktree-audit.sh [-b <ref>] [repo]` | `await pstack.worktree_audit("-b", "origin/main")`. `LAST_CHAT` is the last Caudra session that worked in the worktree. |
| `scripts/watch-pr/watch-pr <args>` | `await pstack.watch_pr("--pr", "123", "--status-only")`, same flags. A polling run streams its lines to the call's row and ends at the run's `timeoutSec`, at most 3600: raise `timeoutSec` for drive mode, or arm the `pstack-watch-pr` automation for a long wait. |
| `orch <command> <args>` | `await pstack.orch("--store", store, "unit", "add", ...)`. Every call passes `--store`, `init` included. |
| installing bun, node, or a script's dependencies | drop the step |

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

A page's own links stay relative: a page is served at `pstack/<path>`, and a relative link resolves against its directory.

## Cursor-only features

Map first. Remove a step only when Caudra has nothing like it, and then remove it cleanly, with no "skipped" note. If a later step needs what the removed one made, rewrite that step too.

| Cursor | Caudra |
| --- | --- |
| `/loop until X`, the loop skill | `/goal X`, which the user sets (autopilot answers for them). Within a turn, keep working until X holds. |
| Cursor Automations, routine wakes, `update_state`, `api2.cursor.sh/automations` webhooks | Caudra automations (`experimental.automations`): a script from `pstack/caudra/automations/`, or one written with the `caudra-automation-dev` skill, with a `schedule` or `message_received` trigger. The agent writes and validates it; the user trusts and arms it. |
| `SendToUser` secret requests | the user sets the variable in their environment or the global `.env` |
| a frontier watcher or heartbeat armed through the loop skill | an automation with a `schedule` trigger that messages the session, such as `pstack-watch-pr`; without automations, drain when a report arrives |
| agents in other chats reporting to an orchestrator | cross-session messaging (`send_message`, `list_sessions`) when the user has it on; otherwise every worker is a `task` of this session |
| Cursor cloud agents | worktree tasks, as in Subagent fields |
| a Cursor restart | a Caudra restart; running tasks end with it, so resume their work from their reports and branches |
| a built-in PR tool | the forge command the step gives, such as `gh pr create` |
| Bugbot | unchanged: it is a GitHub app, and its comments are ordinary review threads |

Cursor's product name: where the text means the agent host, write Caudra. Where it credits where pstack comes from (its README, the license), keep it.

## Benny

benny's pack is not copied into the project. Its skills stay pages, such as `pstack/automations/benny/skills/triage-issue-reports/SKILL.md`. Its Slack trigger is the `benny-triage` and `benny-reproduce` automations (`pstack/caudra/automations/benny-triage.rhai`, `pstack/caudra/automations/benny-reproduce.rhai`), written to the project's automation directory (from `await automation.list()`), polling Slack with `BENNY_SLACK_BOT_TOKEN` and armed with `/automations arm benny-triage {"channel_id": "C…", "triage_user_id": "U…"}` and `/automations arm benny-reproduce {"channel_id": "C…", "triage_user_id": "U…"}`. Slack actions (`read_action`, `thread_post_action`, reactions, file downloads) use a Slack MCP server's tools when one is enabled, else the Slack Web API with `curl` and the same token. Every rule about posting only in the report's thread stands.
