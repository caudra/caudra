# pstack in Caudra

pstack's skills are written for Cursor. Caudra serves them unchanged, so read this page once per session before you follow one, then map each Cursor name with the tables below. If a Cursor field has no row, drop it and say so. Never pass a field or value a tool's schema rejects.

## Addresses

Every pstack file is a page of the `pstack` skill, loaded with the `skill` tool:

- `pstack/<skill>` is that skill's `SKILL.md`, such as `pstack/poteto-mode`.
- `pstack/<path>` is any file by its path in the plugin, such as `pstack/skills/poteto-mode/playbooks/bug-fix.md` or `pstack/docs/guide/README.md`. `pstack/<skill>/<path>` is short for `pstack/skills/<skill>/<path>`.
- A page starts with its address. A relative path or link in it, such as `playbooks/feature.md`, `references/rubric.md`, or `../how/SKILL.md`, resolves against that page's directory. Load the result the same way.
- `deslop`, `control-cli`, and `control-ui` come from Cursor's `cursor-team-kit` and are bundled as `pstack/deslop`, `pstack/control-cli`, and `pstack/control-ui`.

Never fetch a `github.com/cursor/plugins` URL for a file that has an address here. Give the user the URL when they need a link.

## Tools

| Cursor | Caudra |
| --- | --- |
| `Task` | `task` |
| `AskQuestion` | `question`. In a headless run it is absent, so ask in plain text. |
| Open a todolist | `todo_write` |
| Run another pstack skill (`/name`, or "the **name** skill") | `skill` with `pstack/<name>`, then follow it. When you tell the user to type it, write `/pstack:<name>`. |
| `Shell`, `Read`, `Grep`, `Glob` | `shell`, `file_read`, `file_grep`, `file_glob` |
| `create-skill` (Cursor's built-in skill for writing SKILL.md files) | `skill` with `caudra-docs/skills` |
| The image-generation tool | `image.generate` in `code` |

## Subagent fields

| Cursor | Caudra `task` |
| --- | --- |
| `subagent_type: generalPurpose` | omit `profile` |
| `subagent_type: "poteto-agent"` | `profile: "pstack-poteto-agent"`. It loads `pstack/poteto-mode` itself, so the brief needs no path to it. |
| `subagent_type: "comment-sicko"` | `profile: "pstack-comment-sicko"` |
| `readonly: true` | `mode: "plan"`. It cannot edit files or run mutating commands. |
| `readonly: false` | `mode: "build"` |
| `run_in_background: true`, `is_background` | `background: true` |
| `resume` | `task_id` of a settled task. While a task still runs, start a fresh one with the consolidated brief. |
| `environment: "cloud"` | The worker runs locally in its own git worktree: the brief tells it to `git worktree add` a new branch under the scratch directory, off the base the step names, and to work only there. |
| `environment: "local"` | omit |
| `cloud_base_branch: <b>` | the brief names `<b>` as the worktree's base |
| `file_attachments` | list the paths in the brief |
| Cursor's agent dashboard, a cloud agent's URL or machine | Skip with the line below. |

## Models

| Cursor | Caudra |
| --- | --- |
| `claude-<family>-<version>-<effort>[-fast]`, such as `claude-opus-5-5-xhigh` | `model: "claude-<family>-<version>"` and `thinking: "<effort>"`. Drop `-fast`. |
| `grok-<version>-<effort>[-fast]`, `gpt-<version>-<effort>`, or another family's slug | `model` with the slug minus its effort and `-fast` tokens, and `thinking` with the effort. |
| `auto` or `inherit-parent` | omit `model` and `thinking` |
| A model `task` rejects | omit `model`, keep `thinking`, and say so in your reply |
| The `pstack-models.mdc` rule, `~/.cursor/rules/pstack-models.mdc` | `~/.config/caudra/pstack/models.md`. The `pstack` index shows its role lines, so read them there. A missing file or role line means the skill's default slug. |

## setup-pstack

Follow its steps with these changes:

- Detect. The detected models are the ones `task` accepts: the session's model favorites, and the models `caudra models` lists for providers you are logged in to. Write a real model as its `provider/model` id or its Cursor-style slug with an effort token, such as `claude-opus-5-5-xhigh`, so the budget step works as written.
- Write. Write `~/.config/caudra/pstack/models.md` with no frontmatter: the comment lines, the `# budget` line, and one line per role from step 5. Overwrite the whole file. Never put the role lines in `AGENTS.md`.
- Confirm. The lines apply the next time the `pstack` skill is loaded, in this session or a new one.

## Scripts

pstack's scripts are built into Caudra. Call them in the `code` tool with the same arguments, one string each, as on the upstream command line. Each call returns `{"code", "stdout", "stderr"}` with the script's exit code and output; a nonzero code is the script's verdict, not an exception. Every call asks permission as the `code` tool. `log` and `orch` write files, so plan mode refuses them. The commands a script runs, such as `git` and `gh`, run on this machine; in a remote or sandboxed session the namespace raises, and you follow the step with `shell` instead.

| Upstream | Caudra `code` |
| --- | --- |
| `scripts/log.sh <logfile> <phase> <decision> <why> <evidence> <result>` | `await pstack.log(logfile, phase, decision, why, evidence, result)` |
| `node .../scripts/check-plan.mjs <plan.md>` | `await pstack.check_plan("plan.md")` |
| `scripts/worktree-audit.sh [-b <ref>] [repo]` | `await pstack.worktree_audit("-b", "origin/main")`. `LAST_CHAT` is the last Caudra session that worked in the worktree. |
| `scripts/watch-pr/watch-pr <args>` | `await pstack.watch_pr("--pr", "123", "--status-only")`, with the same flags upstream takes. A polling run streams its NDJSON lines to the call's row and ends at `timeoutSec`, at most 3600, so raise `timeoutSec` for `drive` or run one check per turn. |
| `orch <command> <args>` | `await pstack.orch("--store", store, "unit", "add", ...)`, where `store` is the orchestrate store under Paths and state. Pass `--store` on every call, `init` included: there is no default. |

## Paths and state

| Cursor | Caudra |
| --- | --- |
| `.cursor/skills/`, `~/.cursor/skills/` | `.caudra/skills/`, `~/.config/caudra/skills/` |
| `~/.cursor/plugins/`, `.cursor-plugin/plugin.json` | pstack is built in. A user copy under `~/.config/caudra/skills/pstack/SKILL.md` replaces the whole builtin. |
| `.cursor/rules/*.mdc`, always-applied rules | `AGENTS.md` |
| Transcripts in `~/.cursor/projects/<slug>/agent-transcripts/` | `history.search` and `history.read` in `code`. This chat is `scope="session"`, this project's chats `scope="project"`. |
| "The current agent's store" (orchestrate) | `.caudra/pstack/orchestrate/<project-slug>/` in the main checkout, with that folder in `.git/info/exclude` |

## Orchestrate

| Cursor | Caudra |
| --- | --- |
| A worker's completion notification | A background `task`'s final report, which arrives in this conversation by itself. Push it with `pstack.orch("--store", store, "inbox", "push", <task_id>, <unit>, <status>)` and go back to what you were doing. For a long report, save it under the store and add `"--report", <path>`. |
| A worker that is still running when the drain wants it | `task_control` `status` for one look. Never poll it in a loop: the report comes on its own. |
| A frontier watcher wake or heartbeat armed through the loop skill | An automation with a `schedule` trigger that messages this session, such as `pstack-watch-pr` under Automations. Without automations, drain when a report arrives and at the other three drain points. |
| Agents in other chats reporting into the orchestrator | Cross-session messaging, when the user has it on: each message from another session is an inbox arrival. Otherwise every worker is a `task` of this session. |

## Automations

Caudra ports pstack's automations as Caudra automation scripts, served as pages under `pstack/caudra/automations/`. They need `experimental.automations`. Get the directories from `await automation.list()` in `code`, write a script there byte for byte (everything after the page's first line), check it with `await automation.validate(...)`, then give the user the arm line. A project script waits until the user trusts it in `/automations`.

| Cursor | Caudra |
| --- | --- |
| benny's setup (`pstack/automations/benny/FOR_AGENTS.md`, `setup-benny`): merge the pack into `.cursor/automations/benny/` | Do not copy the pack: its skills stay pages such as `pstack/automations/benny/skills/triage-issue-reports/SKILL.md`. Write `pstack/caudra/automations/benny-triage.rhai` and `pstack/caudra/automations/benny-reproduce.rhai` to the project's `project_dir`. Keep the configuration, routing map, and feature map in `.caudra/benny/`. |
| benny's Slack trigger, a new top-level report in the source channel | The scripts poll Slack every minute or two with the bot token in `BENNY_SLACK_BOT_TOKEN`, which the user sets in their environment or the global `.env`, with `channels:history` on the source channel. Arm with `/automations arm benny-triage {"channel_id": "C…", "triage_user_id": "U…"}` and `/automations arm benny-reproduce {"channel_id": "C…", "triage_user_id": "U…"}`. |
| benny's Slack actions (`read_action`, `thread_post_action`, reactions, file downloads) | A Slack MCP server's tools when one is enabled, else the Slack Web API with `curl` and the same token through `shell`. Every rule about posting only in the report's thread stands. |
| `.cursor/automations/benny/...` paths in benny's prompts | `pstack/automations/benny/...` |
| watch-pr's drive mode, a `watch-pr` run without `--status-only` that blocks until a verdict | One `pstack.watch_pr` call whose run has a `timeoutSec` that covers the wait, or, for a long wait, the `pstack-watch-pr` automation: write `pstack/caudra/automations/pstack-watch-pr.rhai` to `user_dir` and give the user `/automations arm pstack-watch-pr {"owner": "…", "repo": "…", "pr": "123"}`. It needs `GITHUB_TOKEN`. It messages the session when the PR changes; then run `pstack.watch_pr` with `--status-only` and follow the drive step. End your turn instead of waiting. |

## Cursor-only features

When a step needs one of these, put one line in your reply, `Skipped in Caudra: <step>. <reason>.`, then go on to the next step. Do not stop, ask, or retry. When a later step needs what the skipped step made, such as a URL or a key, leave a placeholder and name it in the line.

| Cursor | Caudra |
| --- | --- |
| Cursor Automations, routine wakes, `update_state` routine calls, `api2.cursor.sh/automations` webhooks, `SendToUser` secret requests | Caudra automations, when `experimental.automations` is on: use a script from Automations above, or load `caudra-automation-dev` and write one with a `schedule` or `message_received` trigger. The user trusts and arms it. Otherwise skip with the line. |
| `/loop until X`, the loop skill | a completion goal (`/goal`) or autopilot, which the user sets. Within your turn, keep working until X holds. |
| Bugbot | Bugbot comments on a GitHub PR are ordinary review threads. Triage them with the forge command the step gives. Skip a step that starts, waits for, or configures Bugbot when the PR has no Bugbot check or comment. |
| A built-in PR tool | the forge command the step gives, such as `gh pr create`. That is not a skip, so write no line. |
| Cursor cloud agents | `environment: "cloud"` maps to a worktree, as in Subagent fields. Skip a step that needs the cloud agent itself. |
| A Cursor restart | a Caudra restart. Running tasks end with it; resume their work from their reports and branches. |
