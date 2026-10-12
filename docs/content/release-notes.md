---
title: "Release notes"
description: "What changed in each Caudra release, newest first."
---

Each release lists what changed, newest first. After an update, the first interactive start shows the notes for every release since the one you ran before. Run `/changelog` to read them again, or `/changelog all` for every release. [`ui.changelog_on_update`](/docs/configuration/#ui-changelog-on-update) turns the automatic view off.

To install Caudra, follow the [Quick Start](/docs/quick-start/). To update, run [`caudra update`](/docs/cli/#caudra-update-caudra-rollback). Each release links to its GitHub page, which holds the downloads.

## Caudra 0.2 Preview 7 {#v0-2-0-preview-7}

[v0.2.0-preview.7 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.7)

Preview 7 keeps the reasoning of GPT models on Copilot in one piece, makes copying from Caudra in WSL reach the Windows clipboard, and leaves approving a saved plan to the plan menu.

### Providers

- **Copilot reasoning stays whole.** With GPT models on Copilot and Copilot Enterprise, a finished turn split its reasoning into a separate block for each streamed word. Caudra then sent that reasoning back to the model as plain reply text, once in fragments and once in full, and dropped the encrypted reasoning the model needs on later turns. Each reasoning summary is now one block, and later turns send the encrypted reasoning back as they do with OpenAI. Turns saved before the update keep their split blocks.

### Terminal

- **Copying works in WSL.** In WSL, Caudra showed "Copied" while the text often stayed in the WSLg clipboard and never reached Windows. Every copy now also asks the terminal to take the text with OSC 52, which Windows Terminal and Orca pass to the Windows clipboard. In Orca, this needs Allow TUI Clipboard Writes (OSC 52), which older Orca releases leave off.

### Plan mode

- **A saved plan goes to the plan menu.** With the question-tool nudge on, a plan-mode reply that offered to implement the saved plan was turned into a question about implementing it. Choosing to implement there could not leave plan mode, so the model refused. Once the plan is saved, the nudge now stays off until your next message, and the plan-mode instructions ask the model to end with a summary instead of asking. Approve the plan from the plan menu as before.

## Caudra 0.2 Preview 6 {#v0-2-0-preview-6}

[v0.2.0-preview.6 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.6)

Preview 6 adds autopilot, an experimental mode that answers permission prompts, questions, and ready plans for you while a session works toward its goal. It also forwards chosen environment variables to shell commands, draws MCP results readably, logs in to MCP servers that offer no client registration, shows what is new after an update, supports Orca terminals, and makes Enter answer a permission prompt in terminals where it did not.

> **Important**
>
> **Close every other Caudra session before you start Preview 6.** On its first start, Preview 6 upgrades the session database in your [state directory](/docs/configuration/#directory-layout), and the upgrade needs exclusive access. This includes other terminals, editor sessions over ACP, and `caudra storage` commands. While any of them still has the database open, Preview 6 does not start. It lists the open sessions to close instead, and nothing is changed.
>
> The upgrade keeps your sessions and saves a copy of the database from before it as `caudra.db.v23.bak` next to `caudra.db`. Preview 5 and earlier cannot open the upgraded database, and `caudra rollback` restores only the previous binary, not the database.

### Autopilot (experimental)

- **Keep a session moving.** With `experimental.autopilot` on, `/autopilot <goal>` sets a goal and hands what would wait for you to a separate delegate model: permission prompts, the `question` tool, questions asked in prose, a plan that is ready, and the stalls that end a run, such as a goal loop cap or a provider outage. Autopilot ends when the goal is met or impossible, a budget runs out, or you stop it. See [Autopilot](/docs/autopilot/).
- **Prompts it never approves.** The delegate approves a permission prompt once and never adds a rule. Prompts that plan mode raises, tools that always prompt, protected files and commands, and every prompt in a session that content screening flagged are only ever denied with guidance or left to you.
- **Budgets.** Each engagement may start 100 turns without you by default. `max_cost` and `max_minutes` under `[autopilot]` add spend and time limits, and edits apply from the next turn.
- **You stay in charge.** Every decision is a transcript row, and a footer chip opens a panel with the budget and recent decisions. A key or click on a card autopilot is deciding makes it yours. Esc Esc, Ctrl-C while the agent works, or `/autopilot off` stops autopilot. An item it leaves to you raises an urgent notification and a blocked pane status.
- **Headless and CI.** `caudra -p --autopilot "/goal <condition>"` runs unattended, and a prompt autopilot leaves to a human is denied. SDK clients engage it with `--autopilot` or the `autopilot_start` control. See [Headless Mode](/docs/headless/#autopilot).
- **Its own model job.** Decisions use the new Autopilot job, which follows Best unless you bind it in `/model` or with `/autopilot model`. The lifetime view in `/usage` lists its spend under `autopilot`.

### Shell environment

- **Forward the variables your commands need.** `/env` lists every variable Caudra can see, shows which ones reach shell commands, and edits `[agent.shell_env]` in the global `caudra.toml`. `pass` forwards names such as `DOCKER_HOST` or `AWS_*`, and `set` gives commands their own values, with `{env:NAME}` for secrets. A project `.env` never supplies a forwarded value, and a project config cannot hold the table. See [Forwarding more variables](/docs/cli/#forwarding-more-variables).
- **Sandboxes forward their guest's variables.** A sandbox profile's `shell_env` chooses guest variables, and everything by default. Values stay in the guest, and its Workcell never hands its own token or settings to a command. See [Shell environment](/docs/sandboxes/#shell-environment).

### MCP tool cards

- **Readable results.** Each block of an MCP result is drawn by its type. A JSON object or array is indented and coloured, other text is rendered as markdown, and images, audio, and resources become labelled rows with their type and size. The model receives the same text as before.
- **Cleaner header.** A card reads `server › tool` instead of repeating the tool name, with short arguments beside it. The header shows the size of the result and an estimate of the tokens the model reads from it, such as `JSON · 128 lines · ~3.2k tokens`. Opening the card shows every argument. Sessions saved by earlier versions show the same labels and indented JSON.
- **Code it ran.** When a call runs code, such as a `code`, `script`, or `sql` argument, the card shows it numbered and highlighted above the result, in the language the argument name or the code suggests. See [Results in the transcript](/docs/mcp/#results-in-the-transcript).

### MCP OAuth

- **Servers without client registration.** Caudra now logs in to servers that accept OAuth client ID metadata documents, such as Home Assistant, with no client to configure. These logins used to fail with "no stored client and server has no registration endpoint". Caudra's document lives at `https://caudra.ai/oauth/client-metadata.json`, so the server must be able to reach caudra.ai. See [OAuth](/docs/mcp/#oauth).
- **Discovery follows the MCP spec.** `caudra mcp auth` reads the server's login challenge first, and authorization server metadata must name the issuer Caudra asked for. An authorization server whose address has a path no longer falls back to the metadata at the host root, which can describe a different server. Servers without S256 PKCE are refused. A home server on plain HTTP works when its login pages share its origin.
- **Clients stay with their server.** A client is reused only with the authorization server that issued it, and the redirect checks the issuer when the server reports it. After this upgrade, the next interactive login gets a new client once. Stored tokens keep refreshing.

### Permission prompts

- **Enter answers a fresh prompt.** In Herdr and some other terminals, Enter did nothing on a new permission prompt until you clicked or pressed another key. A prompt still ignores a key you were already holding when it appeared. In terminals that never report a key's release, a key counts again once it has been quiet for a second, and the footer says whether to let go or to wait. See [Permission prompts](/docs/permissions/#permission-prompts).
- **Tooltips on every part of the prompt.** Rest the mouse on an answer, a command row, a tab, a Customize field, or a control in the pattern inspector or Advanced view, and a tooltip explains what it does and which key presses it.

### What's new and release notes

- **What's new after an update.** The first time the terminal UI starts after an update, it shows the notes for every release since the one you last ran. `/changelog` shows them again, and `/changelog all` shows every release. In the window, `g` switches between the two, `n` and `p` move between releases, and `o` opens a release on GitHub. The notes ship inside Caudra, so they work offline. Set `ui.changelog_on_update = false` to show them only through `/changelog`.
- **Release notes in the docs.** Every release's notes are on the [Release notes](/docs/release-notes/) page, in `/docs`, and in docs search.

### Terminal integration

- **Orca terminals.** Inside an [Orca](https://github.com/stablyai/orca) terminal, Caudra reports to Orca its state, its model, and the label of a prompt that waits for you. `/worktree` creates and opens worktrees through Orca for repositories Orca knows, and git otherwise. Shell commands get Orca's terminal variables, and an `orca` skill teaches the model Orca's CLI. See [Orca notifications](/docs/notifications/#orca) and [Orca worktrees](/docs/worktrees/#orca-integration).
- **More accurate pane status.** Herdr shows Caudra as working while it saves a permission choice, acknowledges a sandbox, or transfers files, shows held peer messages as waiting for you, and shows a finished or failed turn as idle. With OSC 7501 status, an open login picker shows as waiting for you. Automation `notify()` messages now always arrive as notifications, even inside Herdr and Orca.

### Sessions

- **To-do list of the task you view.** The sticky to-do list now follows the conversation you view. A task shows its own list, labelled with its name, and a task without one shows none instead of the main session's list.

## Caudra 0.2 Preview 5 {#v0-2-0-preview-5}

[v0.2.0-preview.5 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.5)

Preview 5 applies configuration edits while Caudra runs, lets global config files read environment variables, and asks fewer questions about shell lines that set variables.

> **Important**
>
> **After you update, restart running Caudra sessions** so they pick up hot reload. Until every Caudra process that reads your config files runs Preview 5, keep `{env:NAME}` out of them: older versions read it as literal text. Preview 5 does not upgrade the session database.

### Configuration

- **Edits apply while Caudra runs.** The terminal UI watches its config files, skills, custom commands, and system prompt profiles, and applies an edit shortly after you save it. Permissions apply before the next tool call, MCP servers start, stop, and restart at once, and `[agent]` settings apply from the next turn. Some changes still need `/reload` or a restart, and a footer line says which. Headless runs and ACP still read configuration once, at startup. See [Hot reload](/docs/configuration/#hot-reload).
- **Edits keep the prompt cache.** Changing the tool definitions or the system prompt mid-conversation makes the next request read the whole conversation at full price. Caudra therefore keeps what the model was shown unchanged until a moment when the cache is lost anyway, such as a compaction or a model switch, and tells the model about the change in a short reminder meanwhile. What Caudra enforces, such as permissions and which tools can run, follows your edit at once.
- **Broken files fail safely, without dialogs.** A config file that fails to load keeps its last valid version and shows a red chip in the status bar until it loads again. A broken `permissions.toml` instead denies every tool call until it loads again, so a bad edit never leaves an older, looser policy in place. Changed project permissions and MCP servers that need review show a chip that opens the review when you are ready.
- **Environment variables in global config files.** A string value in the global `caudra.toml`, `permissions.toml`, `mcp.toml`, `providers.toml`, or `workcell.toml` can contain `{env:NAME}`. Caudra fills it in from the process environment, or else from the global `.env`, so secrets can stay out of the file. An unset variable is an error that names the file, line, and variable, never a value. Project files under `.caudra/` cannot use references. `caudra auth login` and `/mcp` keep a reference in the file rather than writing its value. See [Environment variables](/docs/configuration/#environment-variables).

### Permissions

- **Shell lines that set variables.** `export`, `unset`, `declare`, `typeset`, `readonly`, `local`, and `set` no longer make a whole line unreadable, which allowed only "run once" or "no". Clearing ordinary variables and switching error options such as `set -euo pipefail` count as read-only, so `unset FOO; set -euo pipefail; cargo test` asks only about `cargo test`, with its usual scopes. Clearing a variable Bash itself reads, such as `PATH` or `IFS`, and declarations that set a variable, such as `export A=1`, are screened as commands built at run time. Commands that only print variables are treated like `printenv`, because their output can show credentials.

## Caudra 0.2 Preview 4 {#v0-2-0-preview-4}

[v0.2.0-preview.4 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.4)

Preview 4 adds hover tooltips, shows Caudra's own files and folders of your choice in the workbench explorer, offers narrower permission scopes for commands that take paths, and makes xAI sign-in work again.

> **Important**
>
> **After you update, restart every running Caudra session**, including sessions in other projects. Preview 3 cannot read saved permissions once one of the new folder scopes is remembered. Preview 4 does not upgrade the session database.

### Interface

- **Hover tooltips.** Rest the mouse on a footer chip, a composer or queue control, the to-do list header, an icon, or text that is cut short, and a tooltip explains it or shows the whole text. A paste token previews its first lines. Set `tooltips = false` under `[ui]` to turn tooltips off.
- **More in the workbench explorer.** A `CAUDRA` section lists your configuration, plans, memory notes, and in a local session your scratch files. A `FOLDERS` section holds any folder on this machine that you add with `Ctrl+X d` or with Add Folder on a section header. Each section folds, resizes, and scrolls on its own. Search, `Ctrl+P`, and source control still cover only the project.

### Permissions

- **Scopes that stop at a folder.** When you remember an answer for a command with a path in it, the scope choices now include its folders. For `gh api repos/acme/tool/issues`, you can allow `gh api repos/acme/tool/*` up to `gh api repos/*`. Patterns you write yourself can end the same way. Shells, interpreters, and other commands that run code get no such scopes.

### Agent

- **A reminder to act after repeated compactions.** When one request has been compacted twice and no work has started, Caudra reminds the agent to act. In Build mode it asks for the first change, in Plan mode for the plan or a question, and in a read-only task for a report. Change the threshold with `work_nudge_compactions` under `[agent]`, or set it to `0` to turn the reminder off.
- **Plan mode with small models.** The `plan` tool no longer waits behind tool search, so models that never search for it can still save their plan.

### Worktrees and Herdr

- **Labeled Herdr tabs.** When `/worktree` creates a worktree inside Herdr, the new tab and pane carry the branch name. If the session's old tab was the last one in its pane, Caudra now exits there and leaves the pane at its shell instead of opening a fresh session.
- **Fewer blocked moves.** A pending revert in an unrelated old conversation no longer stops sessions from moving to another directory or worktree. `/worktree` checks before it creates the checkout and names the session in the way. Moves also wait longer for the session database, so they no longer fail with "storage writer operation timed out" while other Caudra processes are busy.

### Fixes

- **xAI sign-in works again.** Requests over an xAI login failed with HTTP 426 because the Grok CLI proxy now requires a minimum client version. Caudra now declares a supported version. If xAI raises the minimum again before the next release, set `CAUDRA_XAI_CLIENT_VERSION` to the version the error names.
- **`caudra -p` reports failures in its exit status.** A print run whose result is an error now exits with status 1 in every output format, so scripts and CI can detect it. The output itself is unchanged.

## Caudra 0.2 Preview 3 {#v0-2-0-preview-3}

[v0.2.0-preview.3 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.3)

Preview 3 rebuilds project memory around a summarized journal, adds GitHub sign-in for Copilot and a Copilot Enterprise provider, and shows available updates in every session.

> **Important**
>
> **Close every other Caudra session before you start Preview 3.** On its first start, Preview 3 upgrades the session database in your [state directory](/docs/configuration/#directory-layout), and the upgrade needs exclusive access. This includes other terminals, editor sessions over ACP, and `caudra storage` commands. While any of them still has the database open, Preview 3 does not start. It lists the open sessions to close instead, and nothing is changed.
>
> The upgrade keeps your sessions and saves a copy of the database from before it as `caudra.db.v22.bak` next to `caudra.db`. Preview 2 and earlier cannot open the upgraded database, and `caudra rollback` restores only the previous binary, not the database.

### Project memory

- **Memory summarizes itself.** Notes the agent saves are kept in a journal, and a background job writes one-line summaries of older notes. The system prompt carries a view of at most 32 KiB, however many notes a project collects. The model opens a summary line to read the notes behind it, and searches the journal for anything the view leaves out.
- **Existing notes are imported automatically** the first time Caudra opens a project's memory, oldest first. Note files are never modified. Imported notes leave their tags behind, and the memory tool's `list` and `tags` actions are replaced by `view`, `zoom`, and `search`.
- **Summaries cost model usage.** By default the Memory job uses the same model as compaction, because a summary line stands in for its notes for a long time. To spend less, bind Memory to a Fast model in `/model`. To turn summaries off, set `summarize_memory = false` under `[agent]`. Summaries are written during TUI, SDK, and ACP sessions, never during one-shot `--print` runs. The lifetime view in `/usage` lists their spend under `memory`.
- **The summarizer follows `/model`.** Switching the chat model or changing a binding takes effect on the next summary line.
- **`/memory` is an inspector.** It shows the view as the model reads it, expands each line into the notes it covers, and tracks the summaries being written.

### GitHub Copilot

- **Sign in with GitHub.** `caudra auth login copilot` now signs in through the browser with a one-time code. `--method import` still imports tokens from the gh CLI, the Copilot plugin, and the keyring.
- **Copilot Enterprise provider.** `caudra auth login copilot-enterprise --hostname company.ghe.com` signs in to a GitHub Enterprise host. Personal and enterprise accounts can stay signed in together, and enterprise requests go only to `https://copilot-api.<host>`, never to public GitHub. For automation, set `GH_COPILOT_ENTERPRISE_TOKEN` and `GH_COPILOT_ENTERPRISE_HOST` together. This provider is new and has not yet been tested end to end with a live enterprise account, so please report any problems.

### Updates

- **Update banner.** When a newer release is available, a banner above the prompt names it, together with the update command for your channel, in every session. Close it with its `×` or `/dismiss-update`. It returns the next time Caudra starts. Caudra never installs updates on its own.
- **Check now.** `CAUDRA_ENABLE_UPDATE_CHECK=1` asks GitHub immediately instead of using the result cached for 24 hours.

### Interface fixes

- **Guide and replace while work runs.** `Ctrl+X g` and `Ctrl+X x` now work whenever a prompt would queue, including while the agent waits on background tasks or workflows.
- **Usage limits.** When the Anthropic usage endpoint is rate-limited, `/usage` keeps the last figures, shows when they were fetched, and counts down to the next refresh. Click the fetch time to refresh anyway.
- **Rhai highlighting.** Automation and workflow scripts are syntax-highlighted in the workbench, tool cards, diffs, and Markdown code blocks.
- **Workbench scrolling.** Mouse wheel and scrollbar scrolling in Files, Git, and Search no longer snaps back to the selected row.
- **Workbench Git graph.** The graph refreshes when commits are made while the workbench is open, including in linked worktrees.

## Caudra 0.2 Preview 2 {#v0-2-0-preview-2}

[v0.2.0-preview.2 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.2)

Preview 2 improves installation and recovery, adds new opt-in decision features, and makes background shell commands easier to read.

### Installation and updates

- **Clearer PATH setup on macOS and Linux.** The installer prints a command you can use immediately, even when the installation directory is missing from `PATH`. It provides separate instructions for zsh, bash, and fish without editing your shell configuration.
- **Clearer root-install warnings.** The installer explains when it is installing for root rather than your regular account, and distinguishes an explicitly chosen installation directory.
- **Fewer installed license files.** License notices and offline attribution evidence now ship in a compact six-file bundle instead of thousands of individual files. Readable notices remain available, with the detailed evidence preserved in a compressed companion archive.
- **More reliable recovery.** Updates support both existing and compact license bundles. Rollback can restore a valid saved installation even when the current license bundle is damaged. Recognized installer backups are rotated after successful installation, while unrecognized or invalid backups are retained.

### Experimental decision features

- **OpenAI Decisions support.** The decision engine can use the OpenAI Decisions API or a compatible local ninfer server with `decisions.protocol = "openai"`. Existing TypeSafe configurations keep their current protocol by default.
- **Interactive question reminders.** The optional `question_tool_nudge` feature can remind the assistant to use the interactive question tool when it asks you to clarify, choose, or approve something in a prose reply. It does not invent an answer or grant approval.
- **Off unless you turn them on.** The decision inspector and status reporting show the selected protocol. Decision features remain experimental and opt-in, and remote endpoints still require explicit consent. No configuration change is needed to keep using Caudra with your existing setup.

### Interface fixes

- **Highlighted background commands.** Background shell results now syntax-highlight commands, matching the foreground shell display and respecting the syntax-highlighting setting.
