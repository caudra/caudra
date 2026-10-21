---
name: setup-benny
description: Configure Benny and prepare its triage and repro automations. Use when installing Benny or changing its Slack, tracker, repository, routing, control, model, or budget settings.
disable-model-invocation: true
---

# Set up Benny

Benny ships as a dormant automation pack inside pstack. Its files are pages of the `pstack` skill, not skills of their own; this file and the two operational files have no slash command.

The human enters setup by pointing Caudra at the pack's `FOR_AGENTS.md`, the page `pstack/automations/benny/FOR_AGENTS.md`, which leads here. The pack is never copied into the target repository: its skills, references, and templates stay pages at `pstack/automations/benny/...`, loaded with the `skill` tool.

Benny needs external configuration and two Caudra automations, `benny-triage` and `benny-reproduce`. Both need `experimental.automations` in the global `caudra.toml`.

Do not install or update an automation until the user explicitly asks. Never put a secret value in an automation script, a prompt, or configuration.

## 1. Check the prerequisites

Do this before asking for Benny configuration and before installing either automation.

Ask which repository will run the automations. Setup, and every session that arms an automation, works in that repository: a project automation lives in its `.caudra/automations/` and messages the session it is armed in.

Check that automations are on: `code` reaches the `automation` module only when `experimental.automations` is set. If it is off, tell the user to set `automations = true` under `[experimental]` in the global `caudra.toml` and restart Caudra, then stop.

The shared pstack skills Benny uses are built in. Load each with the `skill` tool when a Benny file names it:

- `pstack/how`
- `pstack/why`
- `pstack/tdd`
- `pstack/unslop`
- `pstack/principle-separate-before-serializing-shared-state`
- `pstack/principle-minimize-reader-load`
- `pstack/principle-guard-the-context-window`
- `pstack/principle-sequence-verifiable-units`
- `pstack/principle-fix-root-causes`
- `pstack/principle-prove-it-works`

## 2. Adapt the configuration

Open these examples:

- `../../templates/configuration.example.yaml`
- `../reproduce-and-fix-issues/references/feature-map.example.md`

Create user-owned copies in the target repository's `.caudra/benny/`. These are configuration files, not pack files:

- Configuration: `.caudra/benny/configuration.yaml`
- Feature map: `.caudra/benny/feature-map.md`
- Routing map: `.caudra/benny/routing.md`

Fill one feature-map section for every user-facing feature the automation may reproduce. Keep it at the user point of view. Do not freeze implementation details or current code paths in the map.

Both automations pass the configuration path to their session with every firing, from their `config_path` arg, which defaults to `.caudra/benny/configuration.yaml`. The session reads it and the maps it names from the project's working tree. Keep every value in them secret-free.

Use stable repository-relative paths for configuration files. Name the pack's files by their page addresses, never by a path on disk.

## 3. Fill the required choices

Ask for or confirm:

- Source Slack channel ID
- Optional operations or status channel ID
- Repository URL and default branch
- Triage identity's Slack user ID: the user the triage session posts as
- Issue tracker type, team, project, labels, and intake status
- Tracker adapter skill or MCP actions
- Optional routing map path
- Required control skill name
- Required user-facing feature-map path
- Status emoji strings
- Pull request URL format
- Polling and effort budgets
- Model and thinking level for triage, repro, code work, and media review, as `<model> <thinking>`

Use only models the `task` tool accepts: the session's model favorites and the models `caudra models` lists for providers the user is logged in to. Triage and repro run on the model of the session each automation is armed in. Do not guess a model and do not carry over a private default.

The source channel, triage identity, repository, tracker adapter, control skill, and feature map must be explicit. Fail setup if any required value stays ambiguous.

## 4. Check integration capabilities

The triage automation needs:

- Read access to the configured source Slack channel and its threads
- Thread-reply access in that channel
- Attachment metadata and file download access when reports include media
- Search, read, create, and update access through the configured issue-tracker adapter

The repro automation needs:

- Read access to the source thread
- Thread-reply access in the source channel
- Optional post and edit access in the configured operations channel
- Repository read and history access
- A pull request action that can open a draft pull request, such as `gh pr create --draft`
- The configured control-adapter skill

Both automations poll Slack with `BENNY_SLACK_BOT_TOKEN`, so its bot needs `channels:history` on the source channel and must be a member of it. The user sets the variable in the environment Caudra starts from or in the global `.env`; a project `.env` cannot set it. Never store the value in YAML.

Slack reads, posts, reactions, and file downloads use a Slack MCP server's tools when one is enabled, else the Slack Web API with `curl` and the same token. With `curl`, the bot also needs the scopes for what the session does, such as `chat:write`, `reactions:write`, and `files:read`.

Do not use undocumented integration endpoints.

## 5. Prepare the routing map

If the user wants reroutes or owner pings:

1. Copy `../triage-issue-reports/references/routing.example.md` to `.caudra/benny/routing.md`.
2. Replace every placeholder with public or organization-local values.
3. Keep owner pings off by default.
4. Allow a ping only for a configured feature owner or a confirmed likely regression author.

If no routing map is configured, triage may classify a report but must not guess a destination or owner.

## 6. Verify the control adapter

Read `../reproduce-and-fix-issues/references/control-adapter.md` and the user's completed feature map.

Confirm that the named skill can:

- Bring up the target app
- Navigate every mapped feature through the real UI
- Exercise mapped states through declared adapter actions
- Inspect state without forcing the result
- Capture screenshots
- Start and stop a recording
- Clean up its processes and temporary data

If any capability is missing, leave the repro automation disarmed. It must fail closed rather than claim a reproduction it did not perform.

## 7. Install the automations

Run `await automation.list()` in `code`. Its `project_dir` is the project automation directory, and its `automations` show whether `benny-triage` or `benny-reproduce` is already installed.

Read `../../FOR_AGENTS.md` as the primary user-intent source for either path. Use it to understand the two triggers, tools, instructions, outcomes, and shared rules. The templates `../../templates/triage-automation-prompt.md` and `../../templates/reproduce-automation-prompt.md` say what each automation's message asks of its session.

### First-time installation

For each automation:

1. Load its page with the `skill` tool: `pstack/caudra/automations/benny-triage.rhai`, then `pstack/caudra/automations/benny-reproduce.rhai`.
2. Write everything after the page's first header line to `<project_dir>/benny-triage.rhai` or `<project_dir>/benny-reproduce.rhai` with `file_write`, unchanged.
3. Validate it with `await automation.validate("benny-triage")` or `await automation.validate("benny-reproduce")`. A failure names the line: report it and stop.

`benny-triage` polls the source channel every minute and messages its session with each new top-level report that is neither a bot's nor the triage identity's. Its message tells the session to load `pstack/automations/benny/skills/triage-issue-reports/SKILL.md` and follow it for each report, with the configuration in `config_path`. It starts watching when armed, so earlier messages are never triaged.

`benny-reproduce` polls the channel's recent threads every two minutes and messages its session with each thread where the triage identity's reply carries `[benny:bug]` or `[benny:performance]`. Its message tells the session to load `pstack/automations/benny/skills/reproduce-and-fix-issues/SKILL.md` and follow it for each thread.

Then hand over. Trusting and arming belong to the user. Tell them to:

1. Trust both scripts in `/automations`: select each and press `t`.
2. Set `BENNY_SLACK_BOT_TOKEN`, as in step 4.
3. Arm each in the session that should do its work, filled from the configuration. Separate sessions keep a long repro from delaying triage. Each polls only while its session is open.

```text
/automations arm benny-triage {"channel_id": "C…", "triage_user_id": "U…"}
/automations arm benny-reproduce {"channel_id": "C…", "triage_user_id": "U…"}
```

Add `"config_path": "<path>"` when the configuration is not at `.caudra/benny/configuration.yaml`.

### Existing automations

Finish configuration, routing, control-adapter, and feature-map validation. Then compare each installed script with its page.

- If a script differs from its page, show the user the diff. Replace it only when they agree, validate it again, and tell them to trust the new version in `/automations`.
- Changed args, such as a new channel or triage identity, need no new script: the user arms the automation again with the new args, or edits them under Args in `/automations`.

Do not install a second copy under another name.

### Installation boundary

Never trust or arm an automation yourself, and never edit a script beyond the page's text without the user's consent.

Do not arm either automation on the source channel until the thread-safety test passes.

## 8. Test thread safety

Use a test channel or a harmless test report.

Before testing, confirm that the configuration and every file it names exist under the target repository's `.caudra/benny/`, that `/automations` shows both automations trusted and validated, and that `BENNY_SLACK_BOT_TOKEN` is set. If any check fails, stop. Tell the user that the automations cannot be armed yet.

Verify:

1. Triage stores the root `thread_ts` and posts exactly one verdict as a reply.
2. The verdict contains one configured marker.
3. Repro accepts the marker only from the configured triage identity.
4. Repro keeps the same immutable source coordinates.
5. No source-channel root message appears.
6. A delegated worker cannot use any Slack write action.
7. Missing coordinates, a deleted parent, or a failed preflight produces no post and no tracker issue.

Arm both on the source channel for normal traffic only after all seven checks pass.
